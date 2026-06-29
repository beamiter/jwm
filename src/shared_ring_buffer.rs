//! src/shared_ring_buffer.rs

use crate::backends::common::{AnySyncBackend, GenericHeader, SyncBackend, SyncStrategy};
use crate::shared_message::{SharedCommand, SharedMessage};

use log::{error, info, warn};
use shared_memory::{Shmem, ShmemConf};
use std::io::{Error, ErrorKind, Result};
use std::mem::size_of;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const RING_BUFFER_MAGIC: u64 = 0x52494E47_42554646;
const RING_BUFFER_VERSION: u64 = 9; // v9: FutexHeader cache-line split (256B), EventFdHeader waiter counts
const DEFAULT_BUFFER_SIZE: usize = 16;
const CMD_BUFFER_SIZE: usize = 16;
const DEFAULT_ADAPTIVE_POLL_SPINS: u32 = 400;

#[inline]
fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

#[inline]
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct MessageSlot {
    timestamp: u64,
    checksum: u32,
    _padding: u32,
    message: SharedMessage,
}

fn calculate_message_checksum(m: &SharedMessage) -> u32 {
    let mut sum = 0u32;

    #[inline(always)]
    fn mix_u64(sum: &mut u32, v: u64) {
        *sum = sum.wrapping_add((v as u32) ^ ((v >> 32) as u32));
    }

    #[inline(always)]
    fn mix_i32(sum: &mut u32, v: i32) {
        *sum = sum.wrapping_add(v as u32);
    }

    // timestamp
    mix_u64(&mut sum, m.timestamp);

    let mi = &m.monitor_info;

    // scalar fields
    mix_i32(&mut sum, mi.monitor_num);
    mix_i32(&mut sum, mi.monitor_width);
    mix_i32(&mut sum, mi.monitor_height);
    mix_i32(&mut sum, mi.monitor_x);
    mix_i32(&mut sum, mi.monitor_y);

    // tag_status_vec：将 bool 压缩成位
    for ts in &mi.tag_status_vec {
        let bits: u8 = (ts.is_selected as u8)
            | ((ts.is_urg as u8) << 1)
            | ((ts.is_filled as u8) << 2)
            | ((ts.is_occ as u8) << 3);
        sum = sum.wrapping_add(bits as u32);
    }

    // client_name 和 ltsymbol 数组
    for &b in &mi.client_name {
        sum = sum.wrapping_add(b as u32);
    }
    for &b in &mi.ltsymbol {
        sum = sum.wrapping_add(b as u32);
    }

    sum
}

/// 基于共享内存的高性能 SPSC 环形缓冲区
pub struct SharedRingBuffer {
    shmem: Shmem,
    header: *mut GenericHeader,
    message_slots: *mut MessageSlot,
    cmd_buffer_start: *mut SharedCommand,
    is_creator: bool,
    adaptive_poll_spins: u32,
    backend: AnySyncBackend,
}
impl std::hash::Hash for SharedRingBuffer {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.shmem.get_os_id().hash(state);
    }
}
impl PartialEq for SharedRingBuffer {
    fn eq(&self, other: &Self) -> bool {
        self.shmem.get_os_id() == other.shmem.get_os_id()
    }
}
impl Eq for SharedRingBuffer {}

unsafe impl Send for SharedRingBuffer {}
unsafe impl Sync for SharedRingBuffer {}

impl SharedRingBuffer {
    #[inline]
    fn buffer_size(&self) -> u32 {
        unsafe { (*self.header).buffer_size }
    }

    #[inline]
    fn buffer_mask(&self) -> u32 {
        self.buffer_size() - 1
    }

    #[inline]
    fn cmd_buffer_mask(&self) -> u32 {
        (CMD_BUFFER_SIZE as u32) - 1
    }

    pub fn create_shared_ring_buffer_aux(shared_path: &str) -> Option<Self> {
        return Self::create_shared_ring_buffer(shared_path, Self::get_default_strategy());
    }
    pub fn create_shared_ring_buffer(shared_path: &str, strategy: SyncStrategy) -> Option<Self> {
        if shared_path.is_empty() {
            warn!("No shared path provided, cannot use shared ring buffer.");
            return None;
        }
        match Self::open(shared_path, strategy, None) {
            Ok(shared_buffer) => {
                info!("Successfully opened shared ring buffer: {}", shared_path);
                Some(shared_buffer)
            }
            Err(e) => {
                warn!(
                    "Failed to open existing buffer ('{}'), attempting to create.",
                    e
                );
                match Self::create(shared_path, strategy, None, None) {
                    Ok(shared_buffer) => {
                        info!("Created new shared ring buffer: {}", shared_path);
                        Some(shared_buffer)
                    }
                    Err(create_err) => {
                        error!("Failed to create shared ring buffer: {}", create_err);
                        None
                    }
                }
            }
        }
    }

    pub fn create_aux(
        path: &str,
        buffer_size: Option<usize>,
        adaptive_poll_spins: Option<u32>,
    ) -> Result<Self> {
        return Self::create(
            path,
            Self::get_default_strategy(),
            buffer_size,
            adaptive_poll_spins,
        );
    }
    pub fn create(
        path: &str,
        strategy: SyncStrategy,
        buffer_size: Option<usize>,
        adaptive_poll_spins: Option<u32>,
    ) -> Result<Self> {
        let buffer_size = buffer_size.unwrap_or(DEFAULT_BUFFER_SIZE);
        if !buffer_size.is_power_of_two() || !CMD_BUFFER_SIZE.is_power_of_two() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Buffer sizes must be powers of 2",
            ));
        }

        // 计算内存布局
        let generic_header_size = size_of::<GenericHeader>();
        let backend_header_size = strategy.backend_size();
        let backend_header_align = strategy.backend_align();

        let backend_offset = align_up(generic_header_size, backend_header_align);
        let messages_offset = align_up(
            backend_offset + backend_header_size,
            std::mem::align_of::<MessageSlot>(),
        );
        let messages_size = buffer_size * size_of::<MessageSlot>();
        let commands_offset = align_up(
            messages_offset + messages_size,
            std::mem::align_of::<SharedCommand>(),
        );
        let commands_size = CMD_BUFFER_SIZE * size_of::<SharedCommand>();
        let total_size = commands_offset + commands_size;

        let shmem = ShmemConf::new()
            .size(total_size)
            .flink(path)
            .force_create_flink()
            .create()
            .map_err(|e| Error::new(ErrorKind::Other, format!("Failed to create shmem: {}", e)))?;

        let base_ptr = shmem.as_ptr();
        let header = base_ptr as *mut GenericHeader;
        let backend_ptr = unsafe { base_ptr.add(backend_offset) };
        let message_slots = unsafe { base_ptr.add(messages_offset) as *mut MessageSlot };
        let cmd_buffer_start = unsafe { base_ptr.add(commands_offset) as *mut SharedCommand };

        // 创建并初始化后端
        let mut backend = Self::new_backend(strategy);
        backend.init(true, backend_ptr)?;

        // 初始化通用 Header
        unsafe {
            // 先清零，再设置
            header.write_bytes(0, 1);
            (*header).magic.store(RING_BUFFER_MAGIC, Ordering::Release);
            (*header)
                .version
                .store(RING_BUFFER_VERSION, Ordering::Release);
            (*header).buffer_size = buffer_size as u32;
            (*header).is_destroyed.store(false, Ordering::Release);
        }

        Ok(Self {
            shmem,
            header,
            message_slots,
            cmd_buffer_start,
            is_creator: true,
            backend,
            adaptive_poll_spins: adaptive_poll_spins.unwrap_or(DEFAULT_ADAPTIVE_POLL_SPINS),
        })
    }

    #[allow(unreachable_code)]
    fn get_default_strategy() -> SyncStrategy {
        #[cfg(feature = "use-eventfd")]
        {
            return SyncStrategy::EventFd;
        }
        #[cfg(feature = "use-futex")]
        {
            return SyncStrategy::Futex;
        }
        #[cfg(feature = "use-semaphore")]
        {
            return SyncStrategy::Semaphore;
        }
        return SyncStrategy::Futex;
    }

    pub fn open_aux(path: &str, adaptive_poll_spins: Option<u32>) -> Result<Self> {
        return Self::open(path, Self::get_default_strategy(), adaptive_poll_spins);
    }

    pub fn open(
        path: &str,
        strategy: SyncStrategy,
        adaptive_poll_spins: Option<u32>,
    ) -> Result<Self> {
        let shmem = ShmemConf::new()
            .flink(path)
            .open()
            .map_err(|e| Error::new(ErrorKind::Other, format!("Failed to open shmem: {}", e)))?;

        let base_ptr = shmem.as_ptr();
        let header = base_ptr as *mut GenericHeader;
        let buffer_size;

        // 校验 Header
        unsafe {
            if (*header).magic.load(Ordering::Acquire) != RING_BUFFER_MAGIC {
                return Err(Error::new(ErrorKind::InvalidData, "Invalid magic number"));
            }
            if (*header).version.load(Ordering::Acquire) != RING_BUFFER_VERSION {
                return Err(Error::new(ErrorKind::InvalidData, "Incompatible version"));
            }
            buffer_size = (*header).buffer_size as usize;
        }

        // 根据 Header 信息计算偏移
        let generic_header_size = size_of::<GenericHeader>();
        let backend_header_align = strategy.backend_align();
        let backend_offset = align_up(generic_header_size, backend_header_align);
        let messages_offset = align_up(
            backend_offset + strategy.backend_size(),
            std::mem::align_of::<MessageSlot>(),
        );
        let messages_size = buffer_size * size_of::<MessageSlot>();
        let commands_offset = align_up(
            messages_offset + messages_size,
            std::mem::align_of::<SharedCommand>(),
        );

        let backend_ptr = unsafe { base_ptr.add(backend_offset) };
        let message_slots = unsafe { base_ptr.add(messages_offset) as *mut MessageSlot };
        let cmd_buffer_start = unsafe { base_ptr.add(commands_offset) as *mut SharedCommand };

        // 创建并初始化后端（作为打开者）
        let mut backend = Self::new_backend(strategy);
        backend.init(false, backend_ptr)?;

        Ok(Self {
            shmem,
            header,
            message_slots,
            cmd_buffer_start,
            is_creator: false,
            backend,
            adaptive_poll_spins: adaptive_poll_spins.unwrap_or(DEFAULT_ADAPTIVE_POLL_SPINS),
        })
    }

    fn new_backend(strategy: SyncStrategy) -> AnySyncBackend {
        match strategy {
            #[cfg(feature = "futex")]
            SyncStrategy::Futex => {
                AnySyncBackend::Futex(crate::backends::futex::FutexBackend::new())
            }

            #[cfg(feature = "semaphore")]
            SyncStrategy::Semaphore => {
                AnySyncBackend::Semaphore(crate::backends::semaphore::SemaphoreBackend::new())
            }

            #[cfg(feature = "eventfd")]
            SyncStrategy::EventFd => {
                AnySyncBackend::EventFd(crate::backends::eventfd::EventFdBackend::new())
            }

            #[cfg(not(any(feature = "futex", feature = "semaphore", feature = "eventfd")))]
            _ => unreachable!(),
        }
    }

    pub fn try_write_message(&self, message: &SharedMessage) -> Result<bool> {
        if self.is_destroyed() {
            return Err(Error::new(ErrorKind::BrokenPipe, "Buffer is destroyed"));
        }

        unsafe {
            let write_idx = (*self.header).write_idx.load(Ordering::Relaxed);
            let read_idx = (*self.header).read_idx.load(Ordering::Acquire);

            if write_idx.wrapping_sub(read_idx) >= self.buffer_size() {
                return Ok(false); // 缓冲区已满
            }

            let slot_idx = (write_idx & self.buffer_mask()) as usize;
            let slot = &mut *self.message_slots.add(slot_idx);

            // 构造 Slot 并写入
            *slot = MessageSlot {
                timestamp: now_millis(),
                checksum: calculate_message_checksum(message),
                _padding: 0,
                message: *message,
            };

            // 使用 Release 内存顺序确保写入内容对其他核心可见
            (*self.header)
                .last_timestamp
                .store(slot.timestamp, Ordering::Release);
            (*self.header)
                .write_idx
                .store(write_idx.wrapping_add(1), Ordering::Release);
        }

        self.backend.signal_message()?;
        Ok(true)
    }

    pub fn try_read_next_message(&self) -> Result<Option<SharedMessage>> {
        if self.is_destroyed() {
            return Ok(None);
        }

        unsafe {
            let write_idx = (*self.header).write_idx.load(Ordering::Acquire);
            let read_idx = (*self.header).read_idx.load(Ordering::Relaxed);

            if read_idx == write_idx {
                return Ok(None);
            } // 缓冲区为空

            let slot_idx = (read_idx & self.buffer_mask()) as usize;
            let slot = &*self.message_slots.add(slot_idx);

            if calculate_message_checksum(&slot.message) != slot.checksum {
                (*self.header)
                    .read_idx
                    .store(read_idx.wrapping_add(1), Ordering::Release);
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "Checksum mismatch on read",
                ));
            }

            let message = slot.message;
            (*self.header)
                .read_idx
                .store(read_idx.wrapping_add(1), Ordering::Release);
            Ok(Some(message))
        }
    }

    pub fn try_read_latest_message(&self) -> Result<Option<SharedMessage>> {
        if self.is_destroyed() {
            return Ok(None);
        }

        unsafe {
            let write_idx = (*self.header).write_idx.load(Ordering::Acquire);
            let read_idx = (*self.header).read_idx.load(Ordering::Relaxed);

            if read_idx == write_idx {
                return Ok(None);
            }

            // 跳到最新的消息
            let new_read_idx = write_idx.wrapping_sub(1);

            let slot_idx = (new_read_idx & self.buffer_mask()) as usize;
            let slot = &*self.message_slots.add(slot_idx);

            if calculate_message_checksum(&slot.message) != slot.checksum {
                // 如果最新的也损坏了，就没办法了
                (*self.header).read_idx.store(write_idx, Ordering::Release); // 清空
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "Latest message checksum mismatch",
                ));
            }

            let message = slot.message;
            // 将 read_idx 更新到 write_idx，表示消费了所有消息
            (*self.header).read_idx.store(write_idx, Ordering::Release);
            Ok(Some(message))
        }
    }

    pub fn send_command(&self, command: SharedCommand) -> Result<bool> {
        if self.is_destroyed() {
            return Err(Error::new(ErrorKind::BrokenPipe, "Buffer is destroyed"));
        }

        unsafe {
            let write_idx = (*self.header).cmd_write_idx.load(Ordering::Relaxed);
            let read_idx = (*self.header).cmd_read_idx.load(Ordering::Acquire);

            if write_idx.wrapping_sub(read_idx) >= CMD_BUFFER_SIZE as u32 {
                return Ok(false); // 命令队列已满
            }

            let slot_idx = (write_idx & self.cmd_buffer_mask()) as usize;
            *self.cmd_buffer_start.add(slot_idx) = command;

            (*self.header)
                .cmd_write_idx
                .store(write_idx.wrapping_add(1), Ordering::Release);
        }

        self.backend.signal_command()?;
        Ok(true)
    }

    pub fn receive_command(&self) -> Option<SharedCommand> {
        if self.is_destroyed() {
            return None;
        }

        unsafe {
            let write_idx = (*self.header).cmd_write_idx.load(Ordering::Acquire);
            let read_idx = (*self.header).cmd_read_idx.load(Ordering::Relaxed);

            if read_idx == write_idx {
                return None;
            } // 命令队列为空

            let slot_idx = (read_idx & self.cmd_buffer_mask()) as usize;
            let command = *self.cmd_buffer_start.add(slot_idx);

            (*self.header)
                .cmd_read_idx
                .store(read_idx.wrapping_add(1), Ordering::Release);
            Some(command)
        }
    }

    // --- 等待与状态查询 API ---
    pub fn wait_for_message(&self, timeout: Option<Duration>) -> Result<bool> {
        if self.is_destroyed() {
            return Ok(false);
        }
        self.backend
            .wait_for_message(|| self.has_message(), self.adaptive_poll_spins, timeout)
    }

    pub fn wait_for_command(&self, timeout: Option<Duration>) -> Result<bool> {
        if self.is_destroyed() {
            return Ok(false);
        }
        self.backend
            .wait_for_command(|| self.has_command(), self.adaptive_poll_spins, timeout)
    }

    #[inline]
    pub fn is_destroyed(&self) -> bool {
        unsafe { (*self.header).is_destroyed.load(Ordering::Acquire) }
    }

    pub fn has_message(&self) -> bool {
        !self.is_destroyed() && self.available_messages() > 0
    }

    pub fn available_messages(&self) -> usize {
        if self.is_destroyed() {
            return 0;
        }
        unsafe {
            (*self.header)
                .write_idx
                .load(Ordering::Acquire)
                .wrapping_sub((*self.header).read_idx.load(Ordering::Acquire)) as usize
        }
    }

    pub fn has_command(&self) -> bool {
        !self.is_destroyed() && self.available_commands() > 0
    }

    pub fn available_commands(&self) -> usize {
        if self.is_destroyed() {
            return 0;
        }
        unsafe {
            (*self.header)
                .cmd_write_idx
                .load(Ordering::Acquire)
                .wrapping_sub((*self.header).cmd_read_idx.load(Ordering::Acquire))
                as usize
        }
    }
}

impl Drop for SharedRingBuffer {
    fn drop(&mut self) {
        if !self.header.is_null() {
            if !self.is_destroyed() {
                unsafe {
                    (*self.header).is_destroyed.store(true, Ordering::Release);
                }
            }
            self.backend.cleanup(self.is_creator);
        }

        if self.is_creator {
            if let Some(path) = self.shmem.get_flink_path() {
                info!("(Creator) Removing shmem flink: {:?}", path);
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared_message::{
        CommandType, MonitorInfo, SharedCommand, SharedMessage, TagStatus,
    };
    use std::sync::{Arc, Barrier, Mutex};
    use std::time::Duration;

    // EventFd backend 的 socket 路径使用毫秒精度，并发创建会撞路径，用 Mutex 串行化
    static EVENTFD_LOCK: Mutex<()> = Mutex::new(());

    fn mk_path(name: &str) -> String {
        format!("/tmp/srb_test_{}_{}_{}", std::process::id(), name, {
            use std::time::{SystemTime, UNIX_EPOCH};
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        })
    }

    fn make_msg(num: i32) -> SharedMessage {
        let mut m = SharedMessage::default();
        m.get_monitor_info_mut().monitor_num = num;
        m
    }

    // ── 创建 / 打开 ──────────────────────────────────────────────────────────

    #[test]
    fn test_create_success() {
        let path = mk_path("create");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(0));
        assert!(buf.is_ok());
    }

    #[test]
    fn test_open_after_create() {
        let path = mk_path("open_after_create");
        let _creator = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        let opener = SharedRingBuffer::open_aux(&path, Some(0));
        assert!(opener.is_ok());
    }

    #[test]
    fn test_open_nonexistent_fails() {
        let result = SharedRingBuffer::open_aux("/tmp/srb_does_not_exist_xyzzy", Some(0));
        assert!(result.is_err());
    }

    #[test]
    fn test_create_non_power_of_two_fails() {
        let path = mk_path("non_pow2");
        let result = SharedRingBuffer::create_aux(&path, Some(15), Some(0));
        assert!(result.is_err());
    }

    #[test]
    fn test_empty_path_returns_none() {
        let result = SharedRingBuffer::create_shared_ring_buffer_aux("");
        assert!(result.is_none());
    }

    // ── 空缓冲区行为 ──────────────────────────────────────────────────────────

    #[test]
    fn test_empty_buffer_read_returns_none() {
        let path = mk_path("empty_read");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        assert_eq!(buf.try_read_next_message().unwrap(), None);
        assert_eq!(buf.try_read_latest_message().unwrap(), None);
    }

    #[test]
    fn test_empty_buffer_available_messages_zero() {
        let path = mk_path("empty_avail");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        assert_eq!(buf.available_messages(), 0);
        assert!(!buf.has_message());
    }

    #[test]
    fn test_empty_command_queue() {
        let path = mk_path("empty_cmd");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        assert_eq!(buf.receive_command(), None);
        assert!(!buf.has_command());
        assert_eq!(buf.available_commands(), 0);
    }

    // ── 写 / 读消息 ───────────────────────────────────────────────────────────

    #[test]
    fn test_write_then_read() {
        let path = mk_path("write_read");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        let msg = make_msg(42);
        assert!(buf.try_write_message(&msg).unwrap());
        let got = buf.try_read_next_message().unwrap().unwrap();
        assert_eq!(got.get_monitor_info().monitor_num, 42);
    }

    #[test]
    fn test_message_data_integrity() {
        let path = mk_path("integrity");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();

        let mut mi = MonitorInfo::default();
        mi.monitor_num = 7;
        mi.monitor_width = 1920;
        mi.monitor_height = 1080;
        mi.set_client_name("test_wm");
        mi.set_ltsymbol("[M]");
        mi.set_tag_status(0, TagStatus::new(true, false, true, false));
        let msg = SharedMessage::with_monitor_info(mi);

        buf.try_write_message(&msg).unwrap();
        let got = buf.try_read_next_message().unwrap().unwrap();
        let got_mi = got.get_monitor_info();

        assert_eq!(got_mi.monitor_num, 7);
        assert_eq!(got_mi.monitor_width, 1920);
        assert_eq!(got_mi.monitor_height, 1080);
        assert_eq!(got_mi.get_client_name(), "test_wm");
        assert_eq!(got_mi.get_ltsymbol(), "[M]");
        assert_eq!(
            got_mi.get_tag_status(0),
            Some(TagStatus::new(true, false, true, false))
        );
    }

    #[test]
    fn test_fifo_ordering() {
        let path = mk_path("fifo");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        for i in 0..8i32 {
            assert!(buf.try_write_message(&make_msg(i)).unwrap());
        }
        for i in 0..8i32 {
            let got = buf.try_read_next_message().unwrap().unwrap();
            assert_eq!(got.get_monitor_info().monitor_num, i);
        }
        assert_eq!(buf.try_read_next_message().unwrap(), None);
    }

    #[test]
    fn test_available_messages_count() {
        let path = mk_path("avail_count");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        assert_eq!(buf.available_messages(), 0);
        for i in 1..=5 {
            buf.try_write_message(&make_msg(i)).unwrap();
            assert_eq!(buf.available_messages(), i as usize);
        }
        buf.try_read_next_message().unwrap();
        assert_eq!(buf.available_messages(), 4);
    }

    #[test]
    fn test_has_message_reflects_state() {
        let path = mk_path("has_msg");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        assert!(!buf.has_message());
        buf.try_write_message(&make_msg(1)).unwrap();
        assert!(buf.has_message());
        buf.try_read_next_message().unwrap();
        assert!(!buf.has_message());
    }

    // ── 缓冲区满 ─────────────────────────────────────────────────────────────

    #[test]
    fn test_buffer_full_returns_false() {
        let path = mk_path("buf_full");
        let buf = SharedRingBuffer::create_aux(&path, Some(4), Some(0)).unwrap();
        // 写满 4 条
        for i in 0..4 {
            assert!(buf.try_write_message(&make_msg(i)).unwrap());
        }
        // 第 5 条应返回 false（缓冲区满）
        assert!(!buf.try_write_message(&make_msg(99)).unwrap());
    }

    #[test]
    fn test_write_after_read_when_full() {
        let path = mk_path("write_after_read");
        let buf = SharedRingBuffer::create_aux(&path, Some(4), Some(0)).unwrap();
        for i in 0..4 {
            buf.try_write_message(&make_msg(i)).unwrap();
        }
        // 读一条腾出空间，再写一条
        buf.try_read_next_message().unwrap();
        assert!(buf.try_write_message(&make_msg(100)).unwrap());
    }

    // ── 读最新消息 ────────────────────────────────────────────────────────────

    #[test]
    fn test_read_latest_gets_newest() {
        let path = mk_path("read_latest");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        for i in 0..5 {
            buf.try_write_message(&make_msg(i)).unwrap();
        }
        let got = buf.try_read_latest_message().unwrap().unwrap();
        assert_eq!(got.get_monitor_info().monitor_num, 4);
        // 读最新后缓冲区应被清空
        assert_eq!(buf.try_read_next_message().unwrap(), None);
    }

    #[test]
    fn test_read_latest_single_message() {
        let path = mk_path("read_latest_single");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        buf.try_write_message(&make_msg(77)).unwrap();
        let got = buf.try_read_latest_message().unwrap().unwrap();
        assert_eq!(got.get_monitor_info().monitor_num, 77);
    }

    // ── 环形回绕 ──────────────────────────────────────────────────────────────

    #[test]
    fn test_ring_wraparound() {
        let path = mk_path("wraparound");
        let buf = SharedRingBuffer::create_aux(&path, Some(4), Some(0)).unwrap();
        // 写 4 条 → 读 4 条 → 再写 4 条（触发环形回绕）
        for round in 0..3 {
            for i in 0..4i32 {
                while !buf.try_write_message(&make_msg(round * 10 + i)).unwrap() {
                    buf.try_read_next_message().unwrap();
                }
            }
            for i in 0..4i32 {
                let got = buf.try_read_next_message().unwrap().unwrap();
                assert_eq!(got.get_monitor_info().monitor_num, round * 10 + i);
            }
        }
    }

    // ── 命令队列 ──────────────────────────────────────────────────────────────

    #[test]
    fn test_send_and_receive_command() {
        let path = mk_path("cmd_basic");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        let cmd = SharedCommand::view_tag(1 << 3, 0);
        assert!(buf.send_command(cmd).unwrap());
        assert!(buf.has_command());
        let got = buf.receive_command().unwrap();
        assert_eq!(got.get_command_type(), CommandType::ViewTag);
        assert_eq!(got.get_parameter(), 1 << 3);
    }

    #[test]
    fn test_command_fifo_ordering() {
        let path = mk_path("cmd_fifo");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        let cmds = [
            SharedCommand::view_tag(1, 0),
            SharedCommand::toggle_tag(2, 1),
            SharedCommand::set_layout(3, 0),
        ];
        for &c in &cmds {
            buf.send_command(c).unwrap();
        }
        assert_eq!(
            buf.receive_command().unwrap().get_command_type(),
            CommandType::ViewTag
        );
        assert_eq!(
            buf.receive_command().unwrap().get_command_type(),
            CommandType::ToggleTag
        );
        assert_eq!(
            buf.receive_command().unwrap().get_command_type(),
            CommandType::SetLayout
        );
        assert_eq!(buf.receive_command(), None);
    }

    #[test]
    fn test_command_queue_full_returns_false() {
        let path = mk_path("cmd_full");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        // CMD_BUFFER_SIZE = 16，塞满
        for i in 0..16 {
            assert!(buf.send_command(SharedCommand::view_tag(i, 0)).unwrap());
        }
        assert!(!buf.send_command(SharedCommand::view_tag(99, 0)).unwrap());
    }

    #[test]
    fn test_available_commands_count() {
        let path = mk_path("cmd_avail");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        assert_eq!(buf.available_commands(), 0);
        for i in 1..=3 {
            buf.send_command(SharedCommand::view_tag(i, 0)).unwrap();
            assert_eq!(buf.available_commands(), i as usize);
        }
    }

    // ── 等待超时 ──────────────────────────────────────────────────────────────

    #[test]
    fn test_wait_for_message_timeout_when_empty() {
        let path = mk_path("wait_msg_timeout");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        let result = buf.wait_for_message(Some(Duration::from_millis(5)));
        // 空缓冲区 + 超时 → Ok(false)
        assert!(result.is_ok());
    }

    #[test]
    fn test_wait_for_command_timeout_when_empty() {
        let path = mk_path("wait_cmd_timeout");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        let result = buf.wait_for_command(Some(Duration::from_millis(5)));
        assert!(result.is_ok());
    }

    #[test]
    fn test_wait_for_message_returns_true_when_data_present() {
        let path = mk_path("wait_msg_present");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        buf.try_write_message(&make_msg(1)).unwrap();
        let result = buf
            .wait_for_message(Some(Duration::from_millis(50)))
            .unwrap();
        assert!(result);
    }

    // ── 多条消息 / 大批量 ─────────────────────────────────────────────────────

    #[test]
    fn test_write_and_read_many() {
        let path = mk_path("many");
        let buf = SharedRingBuffer::create_aux(&path, Some(256), Some(0)).unwrap();
        let n = 200usize;
        for i in 0..n {
            while !buf.try_write_message(&make_msg(i as i32)).unwrap() {
                buf.try_read_next_message().unwrap();
            }
        }
        let mut count = 0;
        while buf.try_read_next_message().unwrap().is_some() {
            count += 1;
        }
        assert!(count > 0);
    }

    // ── 并发（SPSC）────────────────────────────────────────────────────────────

    #[test]
    fn test_spsc_producer_consumer() {
        let path = mk_path("spsc");
        let producer = Arc::new(SharedRingBuffer::create_aux(&path, Some(64), Some(0)).unwrap());
        std::thread::sleep(Duration::from_millis(5));
        let consumer = Arc::new(SharedRingBuffer::open_aux(&path, Some(0)).unwrap());

        let total = 500usize;
        let barrier = Arc::new(Barrier::new(2));

        let p = producer.clone();
        let b = barrier.clone();
        let prod = std::thread::spawn(move || {
            b.wait();
            for i in 0..total {
                while !p.try_write_message(&make_msg(i as i32)).unwrap_or(false) {
                    std::hint::spin_loop();
                }
            }
        });

        let c = consumer.clone();
        let b2 = barrier.clone();
        let cons = std::thread::spawn(move || {
            b2.wait();
            let mut received = 0usize;
            while received < total {
                if c.try_read_next_message().unwrap_or(None).is_some() {
                    received += 1;
                }
            }
            received
        });

        prod.join().unwrap();
        let received = cons.join().unwrap();
        assert_eq!(received, total);
    }

    #[test]
    fn test_spsc_command_producer_consumer() {
        let path = mk_path("spsc_cmd");
        let sender = Arc::new(SharedRingBuffer::create_aux(&path, Some(64), Some(0)).unwrap());
        std::thread::sleep(Duration::from_millis(5));
        let receiver = Arc::new(SharedRingBuffer::open_aux(&path, Some(0)).unwrap());

        let total = 200usize;
        let barrier = Arc::new(Barrier::new(2));

        let s = sender.clone();
        let b1 = barrier.clone();
        let prod = std::thread::spawn(move || {
            b1.wait();
            for i in 0..total {
                let cmd = SharedCommand::view_tag(i as u32, 0);
                while !s.send_command(cmd).unwrap_or(false) {
                    std::hint::spin_loop();
                }
            }
        });

        let r = receiver.clone();
        let b2 = barrier.clone();
        let cons = std::thread::spawn(move || {
            b2.wait();
            let mut count = 0usize;
            while count < total {
                if r.receive_command().is_some() {
                    count += 1;
                }
            }
            count
        });

        prod.join().unwrap();
        assert_eq!(cons.join().unwrap(), total);
    }

    // ── is_destroyed ──────────────────────────────────────────────────────────

    #[test]
    fn test_not_destroyed_initially() {
        let path = mk_path("not_destroyed");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        assert!(!buf.is_destroyed());
    }

    #[test]
    fn test_destroyed_after_creator_drop() {
        let path = mk_path("destroyed_after_drop");
        let opener;
        {
            let _creator = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
            std::thread::sleep(Duration::from_millis(5));
            opener = SharedRingBuffer::open_aux(&path, Some(0)).unwrap();
            assert!(!opener.is_destroyed());
        } // creator dropped here
        assert!(opener.is_destroyed());
    }

    #[test]
    fn test_write_to_destroyed_buffer_errors() {
        let path = mk_path("write_destroyed");
        let opener;
        {
            let _creator = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
            std::thread::sleep(Duration::from_millis(5));
            opener = SharedRingBuffer::open_aux(&path, Some(0)).unwrap();
        }
        let result = opener.try_write_message(&make_msg(1));
        assert!(result.is_err());
    }

    // ── 不同缓冲区大小 ────────────────────────────────────────────────────────

    #[test]
    fn test_various_buffer_sizes() {
        for &size in &[4usize, 8, 16, 32, 64, 128] {
            let path = mk_path(&format!("size_{}", size));
            let buf = SharedRingBuffer::create_aux(&path, Some(size), Some(0)).unwrap();
            // 写满再全部读出
            for i in 0..size {
                assert!(buf.try_write_message(&make_msg(i as i32)).unwrap());
            }
            assert!(!buf.try_write_message(&make_msg(-1)).unwrap());
            for _ in 0..size {
                assert!(buf.try_read_next_message().unwrap().is_some());
            }
            assert_eq!(buf.try_read_next_message().unwrap(), None);
        }
    }

    // ── 显式策略：Futex ───────────────────────────────────────────────────────

    #[test]
    #[cfg(feature = "futex")]
    fn test_explicit_futex_strategy_create_open() {
        let path = mk_path("futex_explicit");
        let creator = SharedRingBuffer::create(&path, SyncStrategy::Futex, Some(16), Some(0));
        assert!(creator.is_ok());
        let opener = SharedRingBuffer::open(&path, SyncStrategy::Futex, Some(0));
        assert!(opener.is_ok());
    }

    #[test]
    #[cfg(feature = "futex")]
    fn test_futex_write_read_roundtrip() {
        let path = mk_path("futex_rw");
        let buf = SharedRingBuffer::create(&path, SyncStrategy::Futex, Some(16), Some(0)).unwrap();
        buf.try_write_message(&make_msg(11)).unwrap();
        let got = buf.try_read_next_message().unwrap().unwrap();
        assert_eq!(got.get_monitor_info().monitor_num, 11);
    }

    #[test]
    #[cfg(feature = "futex")]
    fn test_futex_spsc() {
        let path = mk_path("futex_spsc");
        let producer = Arc::new(
            SharedRingBuffer::create(&path, SyncStrategy::Futex, Some(64), Some(0)).unwrap(),
        );
        std::thread::sleep(Duration::from_millis(5));
        let consumer =
            Arc::new(SharedRingBuffer::open(&path, SyncStrategy::Futex, Some(0)).unwrap());
        let total = 300usize;
        let b = Arc::new(Barrier::new(2));
        let p = producer.clone();
        let b1 = b.clone();
        let prod = std::thread::spawn(move || {
            b1.wait();
            for i in 0..total {
                while !p.try_write_message(&make_msg(i as i32)).unwrap_or(false) {
                    std::hint::spin_loop();
                }
            }
        });
        let c = consumer.clone();
        let b2 = b.clone();
        let cons = std::thread::spawn(move || {
            b2.wait();
            let mut n = 0usize;
            while n < total {
                if c.try_read_next_message().unwrap_or(None).is_some() {
                    n += 1;
                }
            }
            n
        });
        prod.join().unwrap();
        assert_eq!(cons.join().unwrap(), total);
    }

    // ── 显式策略：Semaphore ───────────────────────────────────────────────────

    #[test]
    #[cfg(feature = "semaphore")]
    fn test_explicit_semaphore_strategy_create_open() {
        let path = mk_path("sem_explicit");
        let creator = SharedRingBuffer::create(&path, SyncStrategy::Semaphore, Some(16), Some(0));
        assert!(creator.is_ok());
        let opener = SharedRingBuffer::open(&path, SyncStrategy::Semaphore, Some(0));
        assert!(opener.is_ok());
    }

    #[test]
    #[cfg(feature = "semaphore")]
    fn test_semaphore_write_read_roundtrip() {
        let path = mk_path("sem_rw");
        let buf =
            SharedRingBuffer::create(&path, SyncStrategy::Semaphore, Some(16), Some(0)).unwrap();
        buf.try_write_message(&make_msg(22)).unwrap();
        let got = buf.try_read_next_message().unwrap().unwrap();
        assert_eq!(got.get_monitor_info().monitor_num, 22);
    }

    #[test]
    #[cfg(feature = "semaphore")]
    fn test_semaphore_spsc() {
        let path = mk_path("sem_spsc");
        let producer = Arc::new(
            SharedRingBuffer::create(&path, SyncStrategy::Semaphore, Some(64), Some(0)).unwrap(),
        );
        std::thread::sleep(Duration::from_millis(5));
        let consumer =
            Arc::new(SharedRingBuffer::open(&path, SyncStrategy::Semaphore, Some(0)).unwrap());
        let total = 300usize;
        let b = Arc::new(Barrier::new(2));
        let p = producer.clone();
        let b1 = b.clone();
        let prod = std::thread::spawn(move || {
            b1.wait();
            for i in 0..total {
                while !p.try_write_message(&make_msg(i as i32)).unwrap_or(false) {
                    std::hint::spin_loop();
                }
            }
        });
        let c = consumer.clone();
        let b2 = b.clone();
        let cons = std::thread::spawn(move || {
            b2.wait();
            let mut n = 0usize;
            while n < total {
                if c.try_read_next_message().unwrap_or(None).is_some() {
                    n += 1;
                }
            }
            n
        });
        prod.join().unwrap();
        assert_eq!(cons.join().unwrap(), total);
    }

    // ── 显式策略：EventFd ─────────────────────────────────────────────────────

    #[test]
    #[cfg(feature = "eventfd")]
    fn test_explicit_eventfd_strategy_create_open() {
        let _lock = EVENTFD_LOCK.lock().unwrap();
        let path = mk_path("efd_explicit");
        let creator = SharedRingBuffer::create(&path, SyncStrategy::EventFd, Some(16), Some(0));
        assert!(creator.is_ok());
        std::thread::sleep(Duration::from_millis(10));
        let opener = SharedRingBuffer::open(&path, SyncStrategy::EventFd, Some(0));
        assert!(opener.is_ok());
    }

    #[test]
    #[cfg(feature = "eventfd")]
    fn test_eventfd_spsc_with_wait() {
        let _lock = EVENTFD_LOCK.lock().unwrap();
        let path = mk_path("efd_spsc_wait");
        let producer = Arc::new(
            SharedRingBuffer::create(&path, SyncStrategy::EventFd, Some(64), Some(0)).unwrap(),
        );
        std::thread::sleep(Duration::from_millis(20));
        let consumer =
            Arc::new(SharedRingBuffer::open(&path, SyncStrategy::EventFd, Some(0)).unwrap());

        let total = 200usize;
        let b = Arc::new(Barrier::new(2));
        let p = producer.clone();
        let b1 = b.clone();
        let prod = std::thread::spawn(move || {
            b1.wait();
            for i in 0..total {
                while !p.try_write_message(&make_msg(i as i32)).unwrap_or(false) {
                    std::hint::spin_loop();
                }
            }
        });
        let c = consumer.clone();
        let b2 = b.clone();
        let cons = std::thread::spawn(move || {
            b2.wait();
            let mut n = 0usize;
            while n < total {
                // 利用 wait_for_message 阻塞等待唤醒，而非忙轮询
                let _ = c.wait_for_message(Some(Duration::from_millis(5)));
                while let Ok(Some(_)) = c.try_read_next_message() {
                    n += 1;
                    if n >= total {
                        break;
                    }
                }
            }
            n
        });
        prod.join().unwrap();
        assert_eq!(cons.join().unwrap(), total);
    }

    // ── create_shared_ring_buffer_aux（create-or-open 辅助） ──────────────────

    #[test]
    fn test_create_shared_ring_buffer_aux_creates_new() {
        let path = mk_path("aux_create");
        let buf = SharedRingBuffer::create_shared_ring_buffer_aux(&path);
        assert!(buf.is_some());
    }

    #[test]
    fn test_create_shared_ring_buffer_aux_opens_existing() {
        let path = mk_path("aux_open");
        // 先手动创建
        let _first = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        // aux 应能打开已有的
        let second = SharedRingBuffer::create_shared_ring_buffer_aux(&path);
        assert!(second.is_some());
    }

    // ── 最小缓冲区 size = 1 ───────────────────────────────────────────────────

    #[test]
    fn test_buffer_size_one() {
        let path = mk_path("size_one");
        let buf = SharedRingBuffer::create_aux(&path, Some(1), Some(0)).unwrap();
        // 写 1 条
        assert!(buf.try_write_message(&make_msg(1)).unwrap());
        // 已满，不能再写
        assert!(!buf.try_write_message(&make_msg(2)).unwrap());
        // 读出那一条
        let got = buf.try_read_next_message().unwrap().unwrap();
        assert_eq!(got.get_monitor_info().monitor_num, 1);
        // 再次为空
        assert_eq!(buf.try_read_next_message().unwrap(), None);
    }

    #[test]
    fn test_buffer_size_one_many_rounds() {
        let path = mk_path("size_one_rounds");
        let buf = SharedRingBuffer::create_aux(&path, Some(1), Some(0)).unwrap();
        for i in 0..50i32 {
            assert!(buf.try_write_message(&make_msg(i)).unwrap());
            let got = buf.try_read_next_message().unwrap().unwrap();
            assert_eq!(got.get_monitor_info().monitor_num, i);
        }
    }

    // ── 交替单条读写 ──────────────────────────────────────────────────────────

    #[test]
    fn test_interleaved_write_read() {
        let path = mk_path("interleaved");
        let buf = SharedRingBuffer::create_aux(&path, Some(8), Some(0)).unwrap();
        for i in 0..100i32 {
            assert!(buf.try_write_message(&make_msg(i)).unwrap());
            let got = buf.try_read_next_message().unwrap().unwrap();
            assert_eq!(got.get_monitor_info().monitor_num, i);
            assert_eq!(buf.available_messages(), 0);
        }
    }

    // ── 消息时间戳单调不减 ────────────────────────────────────────────────────

    #[test]
    fn test_message_timestamps_nondecreasing() {
        let path = mk_path("timestamps");
        let buf = SharedRingBuffer::create_aux(&path, Some(64), Some(0)).unwrap();
        let n = 20;
        for i in 0..n {
            buf.try_write_message(&make_msg(i)).unwrap();
        }
        let mut prev_ts = 0u64;
        for _ in 0..n {
            let got = buf.try_read_next_message().unwrap().unwrap();
            assert!(got.get_timestamp() >= prev_ts);
            prev_ts = got.get_timestamp();
        }
    }

    // ── PartialEq / Hash ──────────────────────────────────────────────────────

    #[test]
    fn test_eq_same_shmem() {
        let path = mk_path("eq_same");
        let a = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let b = SharedRingBuffer::open_aux(&path, Some(0)).unwrap();
        // 指向同一底层 shmem，应相等
        assert!(a == b);
    }

    #[test]
    fn test_ne_different_shmem() {
        let path1 = mk_path("ne_a");
        let path2 = mk_path("ne_b");
        let a = SharedRingBuffer::create_aux(&path1, Some(16), Some(0)).unwrap();
        let b = SharedRingBuffer::create_aux(&path2, Some(16), Some(0)).unwrap();
        assert!(a != b);
    }

    #[test]
    fn test_hash_same_shmem_equals() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let path = mk_path("hash_same");
        let a = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let b = SharedRingBuffer::open_aux(&path, Some(0)).unwrap();
        let hash = |rb: &SharedRingBuffer| {
            let mut h = DefaultHasher::new();
            rb.hash(&mut h);
            h.finish()
        };
        assert_eq!(hash(&a), hash(&b));
    }

    // ── wait_for_message 实际唤醒（跨线程）──────────────────────────────────

    #[test]
    fn test_wait_for_message_wakes_on_signal() {
        let path = mk_path("wake_msg");
        let producer = Arc::new(SharedRingBuffer::create_aux(&path, Some(16), Some(100)).unwrap());
        std::thread::sleep(Duration::from_millis(5));
        let consumer = Arc::new(SharedRingBuffer::open_aux(&path, Some(100)).unwrap());

        // 消费者线程先等待，生产者延迟 30ms 后写入
        let c = consumer.clone();
        let waiter = std::thread::spawn(move || {
            // 最多等 500ms
            let got = c
                .wait_for_message(Some(Duration::from_millis(500)))
                .unwrap();
            got
        });

        std::thread::sleep(Duration::from_millis(30));
        producer.try_write_message(&make_msg(99)).unwrap();

        assert!(waiter.join().unwrap());
        assert!(consumer.has_message());
    }

    #[test]
    fn test_wait_for_command_wakes_on_signal() {
        let path = mk_path("wake_cmd");
        let sender = Arc::new(SharedRingBuffer::create_aux(&path, Some(16), Some(100)).unwrap());
        std::thread::sleep(Duration::from_millis(5));
        let receiver = Arc::new(SharedRingBuffer::open_aux(&path, Some(100)).unwrap());

        let r = receiver.clone();
        let waiter = std::thread::spawn(move || {
            r.wait_for_command(Some(Duration::from_millis(500)))
                .unwrap()
        });

        std::thread::sleep(Duration::from_millis(30));
        sender.send_command(SharedCommand::view_tag(1, 0)).unwrap();

        assert!(waiter.join().unwrap());
        assert!(receiver.has_command());
    }

    // ── 多轮填满 + 排空 ───────────────────────────────────────────────────────

    #[test]
    fn test_repeated_fill_and_drain() {
        let path = mk_path("fill_drain");
        let buf = SharedRingBuffer::create_aux(&path, Some(32), Some(0)).unwrap();
        for round in 0..10i32 {
            // 填满
            for i in 0..32i32 {
                assert!(buf.try_write_message(&make_msg(round * 100 + i)).unwrap());
            }
            assert_eq!(buf.available_messages(), 32);
            // 全部读出
            let mut drained = 0;
            while buf.try_read_next_message().unwrap().is_some() {
                drained += 1;
            }
            assert_eq!(drained, 32);
            assert_eq!(buf.available_messages(), 0);
        }
    }

    // ── 命令：跨策略 SPSC ─────────────────────────────────────────────────────

    #[test]
    #[cfg(feature = "futex")]
    fn test_futex_command_cross_instance() {
        let path = mk_path("futex_cmd_cross");
        let sender =
            SharedRingBuffer::create(&path, SyncStrategy::Futex, Some(16), Some(0)).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let receiver = SharedRingBuffer::open(&path, SyncStrategy::Futex, Some(0)).unwrap();

        sender
            .send_command(SharedCommand::toggle_tag(0b011, 1))
            .unwrap();
        let got = receiver.receive_command().unwrap();
        assert_eq!(got.get_command_type(), CommandType::ToggleTag);
        assert_eq!(got.get_parameter(), 0b011);
        assert_eq!(got.get_monitor_id(), 1);
    }

    #[test]
    #[cfg(feature = "semaphore")]
    fn test_semaphore_command_cross_instance() {
        let path = mk_path("sem_cmd_cross");
        let sender =
            SharedRingBuffer::create(&path, SyncStrategy::Semaphore, Some(16), Some(0)).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let receiver = SharedRingBuffer::open(&path, SyncStrategy::Semaphore, Some(0)).unwrap();

        sender
            .send_command(SharedCommand::set_layout(2, 0))
            .unwrap();
        let got = receiver.receive_command().unwrap();
        assert_eq!(got.get_command_type(), CommandType::SetLayout);
        assert_eq!(got.get_parameter(), 2);
    }

    // ── 跨实例单线程读写 ──────────────────────────────────────────────────────

    #[test]
    fn test_cross_instance_creator_writes_opener_reads() {
        let path = mk_path("cross_write_read");
        let creator = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let opener = SharedRingBuffer::open_aux(&path, Some(0)).unwrap();

        creator.try_write_message(&make_msg(55)).unwrap();
        let got = opener.try_read_next_message().unwrap().unwrap();
        assert_eq!(got.get_monitor_info().monitor_num, 55);
    }

    #[test]
    fn test_cross_instance_opener_writes_creator_reads() {
        let path = mk_path("cross_rev");
        let creator = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let opener = SharedRingBuffer::open_aux(&path, Some(0)).unwrap();

        opener.try_write_message(&make_msg(66)).unwrap();
        let got = creator.try_read_next_message().unwrap().unwrap();
        assert_eq!(got.get_monitor_info().monitor_num, 66);
    }

    #[test]
    fn test_cross_instance_command_opener_sends_creator_receives() {
        let path = mk_path("cross_cmd_rev");
        let creator = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let opener = SharedRingBuffer::open_aux(&path, Some(0)).unwrap();

        opener
            .send_command(SharedCommand::toggle_tag(0b110, 2))
            .unwrap();
        let got = creator.receive_command().unwrap();
        assert_eq!(got.get_command_type(), CommandType::ToggleTag);
        assert_eq!(got.get_parameter(), 0b110);
        assert_eq!(got.get_monitor_id(), 2);
    }

    #[test]
    fn test_cross_instance_multiple_messages() {
        let path = mk_path("cross_multi");
        let creator = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let opener = SharedRingBuffer::open_aux(&path, Some(0)).unwrap();

        for i in 0..8i32 {
            creator.try_write_message(&make_msg(i)).unwrap();
        }
        for i in 0..8i32 {
            let got = opener.try_read_next_message().unwrap().unwrap();
            assert_eq!(got.get_monitor_info().monitor_num, i);
        }
        assert_eq!(opener.try_read_next_message().unwrap(), None);
    }

    // ── create_shared_ring_buffer（显式策略入口）─────────────────────────────

    #[test]
    #[cfg(feature = "futex")]
    fn test_create_shared_ring_buffer_with_strategy() {
        let path = mk_path("csrb_strategy");
        let buf = SharedRingBuffer::create_shared_ring_buffer(&path, SyncStrategy::Futex);
        assert!(buf.is_some());
        let b = buf.unwrap();
        b.try_write_message(&make_msg(7)).unwrap();
        assert_eq!(b.available_messages(), 1);
    }

    #[test]
    #[cfg(feature = "futex")]
    fn test_create_shared_ring_buffer_opens_existing_with_strategy() {
        let path = mk_path("csrb_open_existing");
        // 先用 create 建立
        let _first =
            SharedRingBuffer::create(&path, SyncStrategy::Futex, Some(16), Some(0)).unwrap();
        // create_shared_ring_buffer 应打开已有的
        let second = SharedRingBuffer::create_shared_ring_buffer(&path, SyncStrategy::Futex);
        assert!(second.is_some());
    }

    // ── available_messages 在 try_read_latest_message 后归零 ─────────────────

    #[test]
    fn test_available_after_read_latest_is_zero() {
        let path = mk_path("avail_after_latest");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        for i in 0..5 {
            buf.try_write_message(&make_msg(i)).unwrap();
        }
        assert_eq!(buf.available_messages(), 5);
        let got = buf.try_read_latest_message().unwrap();
        assert!(got.is_some());
        assert_eq!(buf.available_messages(), 0);
        assert!(!buf.has_message());
    }

    #[test]
    fn test_read_latest_consecutive_calls_return_none_after_drain() {
        let path = mk_path("latest_consec");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        buf.try_write_message(&make_msg(1)).unwrap();
        // 第一次取最新
        assert!(buf.try_read_latest_message().unwrap().is_some());
        // 随后为空
        assert_eq!(buf.try_read_latest_message().unwrap(), None);
        assert_eq!(buf.try_read_latest_message().unwrap(), None);
    }

    #[test]
    fn test_read_latest_after_partial_drain() {
        let path = mk_path("latest_partial");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        for i in 0..8i32 {
            buf.try_write_message(&make_msg(i)).unwrap();
        }
        // 先顺序读 3 条
        for _ in 0..3 {
            buf.try_read_next_message().unwrap();
        }
        // read_latest 应得到最后写入的 7
        let got = buf.try_read_latest_message().unwrap().unwrap();
        assert_eq!(got.get_monitor_info().monitor_num, 7);
        assert_eq!(buf.available_messages(), 0);
    }

    // ── size = 2 的回绕边界 ───────────────────────────────────────────────────

    #[test]
    fn test_buffer_size_two_wraparound() {
        let path = mk_path("size_two_wrap");
        let buf = SharedRingBuffer::create_aux(&path, Some(2), Some(0)).unwrap();
        for round in 0..20i32 {
            assert!(buf.try_write_message(&make_msg(round * 2)).unwrap());
            assert!(buf.try_write_message(&make_msg(round * 2 + 1)).unwrap());
            assert!(!buf.try_write_message(&make_msg(-1)).unwrap()); // 已满
            let a = buf.try_read_next_message().unwrap().unwrap();
            let b = buf.try_read_next_message().unwrap().unwrap();
            assert_eq!(a.get_monitor_info().monitor_num, round * 2);
            assert_eq!(b.get_monitor_info().monitor_num, round * 2 + 1);
            assert_eq!(buf.try_read_next_message().unwrap(), None);
        }
    }

    // ── 高自旋数不影响正确性 ──────────────────────────────────────────────────

    #[test]
    fn test_high_adaptive_spin_count() {
        let path = mk_path("high_spins");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(100_000)).unwrap();
        buf.try_write_message(&make_msg(42)).unwrap();
        // wait_for_message 应能在有消息时立即返回（自旋命中）
        let ready = buf
            .wait_for_message(Some(Duration::from_millis(100)))
            .unwrap();
        assert!(ready);
        let got = buf.try_read_next_message().unwrap().unwrap();
        assert_eq!(got.get_monitor_info().monitor_num, 42);
    }

    #[test]
    fn test_zero_spin_count() {
        let path = mk_path("zero_spins");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        for i in 0..10i32 {
            buf.try_write_message(&make_msg(i)).unwrap();
        }
        let mut count = 0;
        while buf.try_read_next_message().unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 10);
    }

    // ── 销毁后 send_command 返回错误 ─────────────────────────────────────────

    #[test]
    fn test_send_command_to_destroyed_buffer_errors() {
        let path = mk_path("cmd_destroyed");
        let opener;
        {
            let _creator = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
            std::thread::sleep(Duration::from_millis(5));
            opener = SharedRingBuffer::open_aux(&path, Some(0)).unwrap();
        }
        assert!(opener.is_destroyed());
        let result = opener.send_command(SharedCommand::view_tag(1, 0));
        assert!(result.is_err());
    }

    // ── 消息与命令并存互不干扰 ────────────────────────────────────────────────

    #[test]
    fn test_messages_and_commands_independent() {
        let path = mk_path("msg_cmd_independent");
        let creator = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let opener = SharedRingBuffer::open_aux(&path, Some(0)).unwrap();

        // 写消息 + 发命令
        creator.try_write_message(&make_msg(10)).unwrap();
        creator.try_write_message(&make_msg(20)).unwrap();
        opener.send_command(SharedCommand::view_tag(1, 0)).unwrap();
        opener
            .send_command(SharedCommand::toggle_tag(2, 1))
            .unwrap();

        // 消息和命令独立计数
        assert_eq!(creator.available_messages(), 2);
        assert_eq!(creator.available_commands(), 2);

        // 读消息不影响命令
        creator.try_read_next_message().unwrap();
        assert_eq!(creator.available_messages(), 1);
        assert_eq!(creator.available_commands(), 2);

        // 读命令不影响消息
        creator.receive_command().unwrap();
        assert_eq!(creator.available_messages(), 1);
        assert_eq!(creator.available_commands(), 1);
    }

    // ── has_command 在命令读完后变为 false ───────────────────────────────────

    #[test]
    fn test_has_command_false_after_drain() {
        let path = mk_path("has_cmd_drain");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        buf.send_command(SharedCommand::view_tag(1, 0)).unwrap();
        assert!(buf.has_command());
        buf.receive_command().unwrap();
        assert!(!buf.has_command());
        assert_eq!(buf.available_commands(), 0);
    }

    // ── 精确容量边界：写满后读完再写满 ──────────────────────────────────────

    #[test]
    fn test_write_exact_capacity_read_all_write_again() {
        let path = mk_path("exact_cap");
        let cap = 8usize;
        let buf = SharedRingBuffer::create_aux(&path, Some(cap), Some(0)).unwrap();

        for pass in 0..3 {
            // 恰好写满 cap 条
            for i in 0..cap {
                assert!(buf
                    .try_write_message(&make_msg((pass * cap + i) as i32))
                    .unwrap());
            }
            // 第 cap+1 条写入失败
            assert!(!buf.try_write_message(&make_msg(-1)).unwrap());
            // 全部读出
            for i in 0..cap {
                let got = buf.try_read_next_message().unwrap().unwrap();
                assert_eq!(got.get_monitor_info().monitor_num, (pass * cap + i) as i32);
            }
            assert_eq!(buf.try_read_next_message().unwrap(), None);
        }
    }

    // ── 已销毁缓冲区：所有方法的防御路径 ────────────────────────────────────

    fn make_destroyed_opener(name: &str) -> SharedRingBuffer {
        let path = mk_path(name);
        let _creator = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let opener = SharedRingBuffer::open_aux(&path, Some(0)).unwrap();
        drop(_creator); // creator drop → destroyed
        opener
    }

    #[test]
    fn test_destroyed_try_read_next_message_returns_none() {
        let opener = make_destroyed_opener("dest_read_next");
        assert!(opener.is_destroyed());
        assert_eq!(opener.try_read_next_message().unwrap(), None);
    }

    #[test]
    fn test_destroyed_try_read_latest_message_returns_none() {
        let opener = make_destroyed_opener("dest_read_latest");
        assert_eq!(opener.try_read_latest_message().unwrap(), None);
    }

    #[test]
    fn test_destroyed_receive_command_returns_none() {
        let opener = make_destroyed_opener("dest_recv_cmd");
        assert_eq!(opener.receive_command(), None);
    }

    #[test]
    fn test_destroyed_wait_for_message_returns_false() {
        let opener = make_destroyed_opener("dest_wait_msg");
        let result = opener
            .wait_for_message(Some(Duration::from_millis(5)))
            .unwrap();
        assert!(!result);
    }

    #[test]
    fn test_destroyed_wait_for_command_returns_false() {
        let opener = make_destroyed_opener("dest_wait_cmd");
        let result = opener
            .wait_for_command(Some(Duration::from_millis(5)))
            .unwrap();
        assert!(!result);
    }

    #[test]
    fn test_destroyed_available_messages_returns_zero() {
        let opener = make_destroyed_opener("dest_avail_msg");
        assert_eq!(opener.available_messages(), 0);
    }

    #[test]
    fn test_destroyed_available_commands_returns_zero() {
        let opener = make_destroyed_opener("dest_avail_cmd");
        assert_eq!(opener.available_commands(), 0);
    }

    #[test]
    fn test_destroyed_has_message_returns_false() {
        let opener = make_destroyed_opener("dest_has_msg");
        assert!(!opener.has_message());
    }

    #[test]
    fn test_destroyed_has_command_returns_false() {
        let opener = make_destroyed_opener("dest_has_cmd");
        assert!(!opener.has_command());
    }

    // ── opener 连接前 creator 已写入的消息仍可读 ─────────────────────────────

    #[test]
    fn test_opener_reads_messages_written_before_open() {
        let path = mk_path("prewrite");
        let creator = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        // 先写 5 条消息，再打开
        for i in 0..5i32 {
            creator.try_write_message(&make_msg(i)).unwrap();
        }
        std::thread::sleep(Duration::from_millis(5));
        let opener = SharedRingBuffer::open_aux(&path, Some(0)).unwrap();
        // opener 应能读到这 5 条
        for i in 0..5i32 {
            let got = opener.try_read_next_message().unwrap().unwrap();
            assert_eq!(got.get_monitor_info().monitor_num, i);
        }
        assert_eq!(opener.try_read_next_message().unwrap(), None);
    }

    #[test]
    fn test_opener_reads_latest_written_before_open() {
        let path = mk_path("prewrite_latest");
        let creator = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        for i in 0..8i32 {
            creator.try_write_message(&make_msg(i)).unwrap();
        }
        std::thread::sleep(Duration::from_millis(5));
        let opener = SharedRingBuffer::open_aux(&path, Some(0)).unwrap();
        let got = opener.try_read_latest_message().unwrap().unwrap();
        assert_eq!(got.get_monitor_info().monitor_num, 7);
    }

    // ── 多个 opener 共享同一 creator 的消息 ──────────────────────────────────

    #[test]
    fn test_multiple_openers_share_same_buffer() {
        let path = mk_path("multi_opener");
        let creator = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let opener1 = SharedRingBuffer::open_aux(&path, Some(0)).unwrap();
        let opener2 = SharedRingBuffer::open_aux(&path, Some(0)).unwrap();

        creator.try_write_message(&make_msg(100)).unwrap();

        // 两个 opener 看到同一条消息（SPSC：谁先读谁拿到）
        assert_eq!(opener1.available_messages(), opener2.available_messages());
        assert_eq!(opener1.available_messages(), 1);

        // opener1 读走后，opener2 看不到了
        opener1.try_read_next_message().unwrap().unwrap();
        assert_eq!(opener2.available_messages(), 0);
    }

    // ── 全量 client_name 内容的消息完整性 ────────────────────────────────────

    #[test]
    fn test_full_client_name_message_integrity() {
        let path = mk_path("full_name");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();

        // 填满 client_name（MAX_CLIENT_NAME_LEN - 1 字节）
        let name = "A".repeat(crate::MAX_CLIENT_NAME_LEN - 1);
        let sym = "B".repeat(crate::MAX_LT_SYMBOL_LEN - 1);
        let mut mi = MonitorInfo::default();
        mi.set_client_name(&name);
        mi.set_ltsymbol(&sym);
        mi.monitor_num = 999;
        for i in 0..crate::MAX_TAGS {
            mi.set_tag_status(i, TagStatus::new(true, false, true, false));
        }
        let msg = SharedMessage::with_monitor_info(mi);
        buf.try_write_message(&msg).unwrap();
        let got = buf.try_read_next_message().unwrap().unwrap();
        let got_mi = got.get_monitor_info();
        assert_eq!(got_mi.get_client_name(), name);
        assert_eq!(got_mi.get_ltsymbol(), sym);
        assert_eq!(got_mi.monitor_num, 999);
        for i in 0..crate::MAX_TAGS {
            assert_eq!(
                got_mi.get_tag_status(i),
                Some(TagStatus::new(true, false, true, false))
            );
        }
    }

    // ── available_messages 随每次写入精确递增 ────────────────────────────────

    #[test]
    fn test_available_messages_increments_per_write() {
        let path = mk_path("incr_avail");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        for i in 1..=8usize {
            buf.try_write_message(&make_msg(i as i32)).unwrap();
            assert_eq!(buf.available_messages(), i);
        }
    }

    #[test]
    fn test_available_commands_increments_per_send() {
        let path = mk_path("incr_cmd_avail");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        for i in 1..=8usize {
            buf.send_command(SharedCommand::view_tag(i as u32, 0))
                .unwrap();
            assert_eq!(buf.available_commands(), i);
        }
    }

    // ── 跨策略：opener 的 read_latest ────────────────────────────────────────

    #[test]
    fn test_opener_read_latest_from_cross_instance() {
        let path = mk_path("opener_latest");
        let creator = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let opener = SharedRingBuffer::open_aux(&path, Some(0)).unwrap();

        for i in 0..6i32 {
            creator.try_write_message(&make_msg(i)).unwrap();
        }
        let got = opener.try_read_latest_message().unwrap().unwrap();
        assert_eq!(got.get_monitor_info().monitor_num, 5);
        assert_eq!(creator.available_messages(), 0);
    }

    // ── Semaphore 策略：高自旋 SPSC ──────────────────────────────────────────

    #[test]
    #[cfg(feature = "semaphore")]
    fn test_semaphore_high_spin_spsc() {
        let path = mk_path("sem_high_spin");
        let producer = Arc::new(
            SharedRingBuffer::create(&path, SyncStrategy::Semaphore, Some(32), Some(10_000))
                .unwrap(),
        );
        std::thread::sleep(Duration::from_millis(5));
        let consumer =
            Arc::new(SharedRingBuffer::open(&path, SyncStrategy::Semaphore, Some(10_000)).unwrap());
        let total = 200usize;
        let b = Arc::new(Barrier::new(2));
        let p = producer.clone();
        let b1 = b.clone();
        let prod = std::thread::spawn(move || {
            b1.wait();
            for i in 0..total {
                while !p.try_write_message(&make_msg(i as i32)).unwrap_or(false) {
                    std::hint::spin_loop();
                }
            }
        });
        let c = consumer.clone();
        let b2 = b.clone();
        let cons = std::thread::spawn(move || {
            b2.wait();
            let mut n = 0usize;
            while n < total {
                let _ = c.wait_for_message(Some(Duration::from_millis(5)));
                while let Ok(Some(_)) = c.try_read_next_message() {
                    n += 1;
                    if n >= total {
                        break;
                    }
                }
            }
            n
        });
        prod.join().unwrap();
        assert_eq!(cons.join().unwrap(), total);
    }

    // ── 空路径 create_shared_ring_buffer 返回 None ────────────────────────────

    #[test]
    #[cfg(feature = "futex")]
    fn test_create_shared_ring_buffer_empty_path_returns_none() {
        let result = SharedRingBuffer::create_shared_ring_buffer("", SyncStrategy::Futex);
        assert!(result.is_none());
    }

    // ── 任意实例 drop 会将共享缓冲区标记为已销毁 ────────────────────────────

    #[test]
    fn test_opener_drop_marks_buffer_destroyed() {
        let path = mk_path("opener_drop");
        let creator = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        {
            let _opener = SharedRingBuffer::open_aux(&path, Some(0)).unwrap();
            // opener 在此处 drop，Drop impl 对所有实例均设置 is_destroyed = true
        }
        // Drop 实现对所有实例（含 creator）无差别地设置销毁标志
        assert!(creator.is_destroyed());
    }

    #[test]
    fn test_any_instance_drop_marks_buffer_destroyed() {
        let path = mk_path("multi_opener_drop");
        let creator = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        // 多个 opener 依次创建并 drop，每次 drop 都会设置 is_destroyed = true
        for _ in 0..3 {
            let _opener = SharedRingBuffer::open_aux(&path, Some(0)).unwrap();
        }
        // 任意一个 opener drop 后，creator 所见的标志均为已销毁
        assert!(creator.is_destroyed());
    }

    // ── 全双工：creator 和 opener 互相收发消息 ────────────────────────────────

    #[test]
    fn test_full_duplex_message_exchange() {
        let path = mk_path("full_duplex");
        let creator = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let opener = SharedRingBuffer::open_aux(&path, Some(0)).unwrap();

        // creator → opener
        creator.try_write_message(&make_msg(10)).unwrap();
        assert_eq!(
            opener
                .try_read_next_message()
                .unwrap()
                .unwrap()
                .get_monitor_info()
                .monitor_num,
            10
        );

        // opener → creator
        opener.try_write_message(&make_msg(20)).unwrap();
        assert_eq!(
            creator
                .try_read_next_message()
                .unwrap()
                .unwrap()
                .get_monitor_info()
                .monitor_num,
            20
        );

        // creator → opener（多条）
        for i in 0..4i32 {
            creator.try_write_message(&make_msg(100 + i)).unwrap();
        }
        for i in 0..4i32 {
            assert_eq!(
                opener
                    .try_read_next_message()
                    .unwrap()
                    .unwrap()
                    .get_monitor_info()
                    .monitor_num,
                100 + i
            );
        }
    }

    #[test]
    fn test_full_duplex_commands_both_directions() {
        let path = mk_path("duplex_cmd");
        let creator = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let opener = SharedRingBuffer::open_aux(&path, Some(0)).unwrap();

        creator.send_command(SharedCommand::view_tag(1, 0)).unwrap();
        opener
            .send_command(SharedCommand::toggle_tag(2, 1))
            .unwrap();

        let from_creator = opener.receive_command().unwrap();
        let from_opener = creator.receive_command().unwrap();

        // 共享同一命令队列，读取顺序为 FIFO
        assert_eq!(from_creator.get_command_type(), CommandType::ViewTag);
        assert_eq!(from_opener.get_command_type(), CommandType::ToggleTag);
    }

    // ── 大 buffer size ────────────────────────────────────────────────────────

    #[test]
    fn test_large_buffer_size_create_and_use() {
        let path = mk_path("large_buf");
        let buf = SharedRingBuffer::create_aux(&path, Some(1024), Some(0)).unwrap();
        // 写满一半
        for i in 0..512i32 {
            assert!(buf.try_write_message(&make_msg(i)).unwrap());
        }
        assert_eq!(buf.available_messages(), 512);
        // 全部读出，验证顺序
        for i in 0..512i32 {
            let got = buf.try_read_next_message().unwrap().unwrap();
            assert_eq!(got.get_monitor_info().monitor_num, i);
        }
        assert_eq!(buf.available_messages(), 0);
    }

    // ── wait_for_message/command 零超时 ──────────────────────────────────────

    #[test]
    fn test_wait_for_message_zero_timeout_empty() {
        let path = mk_path("zero_to_empty");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        // 空缓冲 + 零超时，应立即返回
        let result = buf.wait_for_message(Some(Duration::ZERO));
        assert!(result.is_ok());
    }

    #[test]
    fn test_wait_for_message_zero_timeout_with_data() {
        let path = mk_path("zero_to_data");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        buf.try_write_message(&make_msg(1)).unwrap();
        let result = buf.wait_for_message(Some(Duration::ZERO)).unwrap();
        assert!(result);
    }

    #[test]
    fn test_wait_for_command_zero_timeout_empty() {
        let path = mk_path("zero_to_cmd_empty");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        let result = buf.wait_for_command(Some(Duration::ZERO));
        assert!(result.is_ok());
    }

    // ── opener 侧的 available_messages / has_message ─────────────────────────

    #[test]
    fn test_available_messages_on_opener_side() {
        let path = mk_path("avail_opener");
        let creator = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let opener = SharedRingBuffer::open_aux(&path, Some(0)).unwrap();

        assert_eq!(opener.available_messages(), 0);
        creator.try_write_message(&make_msg(1)).unwrap();
        assert_eq!(opener.available_messages(), 1);
        creator.try_write_message(&make_msg(2)).unwrap();
        assert_eq!(opener.available_messages(), 2);
        opener.try_read_next_message().unwrap();
        assert_eq!(opener.available_messages(), 1);
    }

    #[test]
    fn test_has_message_on_opener_side() {
        let path = mk_path("has_msg_opener");
        let creator = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let opener = SharedRingBuffer::open_aux(&path, Some(0)).unwrap();

        assert!(!opener.has_message());
        creator.try_write_message(&make_msg(5)).unwrap();
        assert!(opener.has_message());
        opener.try_read_next_message().unwrap();
        assert!(!opener.has_message());
    }

    #[test]
    fn test_available_commands_on_opener_side() {
        let path = mk_path("avail_cmd_opener");
        let creator = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let opener = SharedRingBuffer::open_aux(&path, Some(0)).unwrap();

        assert_eq!(creator.available_commands(), 0);
        opener.send_command(SharedCommand::view_tag(1, 0)).unwrap();
        assert_eq!(creator.available_commands(), 1);
        creator.receive_command().unwrap();
        assert_eq!(creator.available_commands(), 0);
    }

    // ── 消息时间戳经过缓冲区后保持不变 ──────────────────────────────────────

    #[test]
    fn test_message_timestamp_preserved_through_buffer() {
        let path = mk_path("ts_preserved");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        let msg = SharedMessage::new();
        let original_ts = msg.get_timestamp();
        buf.try_write_message(&msg).unwrap();
        let got = buf.try_read_next_message().unwrap().unwrap();
        assert_eq!(got.get_timestamp(), original_ts);
    }

    // ── 读操作不破坏相邻 slot 的数据 ─────────────────────────────────────────

    #[test]
    fn test_read_does_not_corrupt_adjacent_slots() {
        let path = mk_path("no_corrupt");
        let buf = SharedRingBuffer::create_aux(&path, Some(8), Some(0)).unwrap();
        // 写 8 条，每条有唯一 monitor_num
        for i in 0..8i32 {
            buf.try_write_message(&make_msg(i * 100)).unwrap();
        }
        // 顺序读出，验证每条内容完整，前一条的读取不影响后一条
        for i in 0..8i32 {
            let got = buf.try_read_next_message().unwrap().unwrap();
            assert_eq!(got.get_monitor_info().monitor_num, i * 100);
        }
    }

    // ── 同实例发送并接收命令 ─────────────────────────────────────────────────

    #[test]
    fn test_same_instance_send_and_receive_command() {
        let path = mk_path("same_inst_cmd");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        buf.send_command(SharedCommand::view_tag(0b1010, 0))
            .unwrap();
        buf.send_command(SharedCommand::set_layout(1, 2)).unwrap();
        let a = buf.receive_command().unwrap();
        let b = buf.receive_command().unwrap();
        assert_eq!(a.get_parameter(), 0b1010);
        assert_eq!(b.get_command_type(), CommandType::SetLayout);
        assert_eq!(b.get_monitor_id(), 2);
        assert_eq!(buf.receive_command(), None);
    }

    // ── 写后读不影响 write_idx 单调递增 ──────────────────────────────────────

    #[test]
    fn test_write_idx_monotonically_increases() {
        let path = mk_path("write_idx_mono");
        let buf = SharedRingBuffer::create_aux(&path, Some(8), Some(0)).unwrap();
        // 多轮写满 + 读空，写索引应持续增加
        let header_ptr = buf.header;
        let initial_widx = unsafe {
            (*header_ptr)
                .write_idx
                .load(std::sync::atomic::Ordering::Acquire)
        };
        assert_eq!(initial_widx, 0);

        for round in 0..3u32 {
            for _ in 0..8 {
                buf.try_write_message(&make_msg(0)).unwrap();
            }
            let widx = unsafe {
                (*header_ptr)
                    .write_idx
                    .load(std::sync::atomic::Ordering::Acquire)
            };
            assert_eq!(widx, (round + 1) * 8);
            while buf.try_read_next_message().unwrap().is_some() {}
        }
    }

    // ── 消息与命令交替操作稳定性 ─────────────────────────────────────────────

    #[test]
    fn test_interleaved_messages_and_commands_stability() {
        let path = mk_path("interleaved_mc");
        let creator = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let opener = SharedRingBuffer::open_aux(&path, Some(0)).unwrap();

        for i in 0..10i32 {
            creator.try_write_message(&make_msg(i)).unwrap();
            opener
                .send_command(SharedCommand::view_tag(i as u32, 0))
                .unwrap();

            let msg = opener.try_read_next_message().unwrap().unwrap();
            assert_eq!(msg.get_monitor_info().monitor_num, i);

            let cmd = creator.receive_command().unwrap();
            assert_eq!(cmd.get_parameter(), i as u32);
        }
        assert_eq!(creator.available_messages(), 0);
        assert_eq!(creator.available_commands(), 0);
    }

    // ── 多次 write/read 循环不丢数据 ─────────────────────────────────────────

    #[test]
    fn test_multiple_write_read_cycles_no_data_loss() {
        let path = mk_path("no_loss");
        let buf = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        let mut total_written = 0i32;
        let mut total_read = 0i32;

        for cycle in 0..20 {
            let batch = (cycle % 7) + 1; // 1..=7 条
            for i in 0..batch {
                buf.try_write_message(&make_msg(total_written + i)).unwrap();
            }
            total_written += batch;

            for i in 0..batch {
                let got = buf.try_read_next_message().unwrap().unwrap();
                assert_eq!(got.get_monitor_info().monitor_num, total_read + i);
            }
            total_read += batch;
        }
        assert_eq!(total_written, total_read);
        assert_eq!(buf.available_messages(), 0);
    }
}
