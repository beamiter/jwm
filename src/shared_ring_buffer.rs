//! src/shared_ring_buffer.rs

use crate::backends::common::{
    AnySyncBackend, GenericHeader, QueueCursor, SyncBackend, SyncStrategy,
};
use crate::shared_message::{SharedCommand, SharedMessage, TagStatus, MAX_TAGS};

use log::{error, info, warn};
use shared_memory::{Shmem, ShmemConf, ShmemError};
use std::io::{Error, ErrorKind, Result, Write};
use std::mem::{align_of, size_of};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const RING_BUFFER_MAGIC: u64 = 0x52494E47_42554646;
const RING_BUFFER_VERSION: u64 = 10;
const LAYOUT_MARKER: u32 = 0x5352_4232; // "SRB2"
const DEFAULT_BUFFER_SIZE: usize = 16;
const CMD_BUFFER_SIZE: usize = 16;
const DEFAULT_ADAPTIVE_POLL_SPINS: u32 = 400;
const OPEN_RETRY_TIMEOUT: Duration = Duration::from_millis(250);
static FLINK_NONCE: AtomicU64 = AtomicU64::new(0);

#[inline]
fn checked_align_up(value: usize, align: usize) -> Result<usize> {
    if !align.is_power_of_two() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "layout alignment must be a non-zero power of two",
        ));
    }
    value
        .checked_add(align - 1)
        .map(|value| value & !(align - 1))
        .ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                "shared-memory layout overflows usize",
            )
        })
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
    written_at: u64,
    checksum: u32,
    _padding: u32,
    message: WireMessage,
}

/// 共享内存中不含 `bool` 的稳定消息表示。
///
/// 所有位模式都是有效的 Rust 值；只有在 checksum 通过后才会转换成
/// 领域对象中的 `bool`。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct WireMessage {
    timestamp: u64,
    monitor_num: i32,
    monitor_width: i32,
    monitor_height: i32,
    monitor_x: i32,
    monitor_y: i32,
    tag_flags: [u8; MAX_TAGS],
    client_name: [u8; crate::MAX_CLIENT_NAME_LEN],
    ltsymbol: [u8; crate::MAX_LT_SYMBOL_LEN],
}

impl From<&SharedMessage> for WireMessage {
    fn from(message: &SharedMessage) -> Self {
        let mut tag_flags = [0; MAX_TAGS];
        for (flags, status) in tag_flags
            .iter_mut()
            .zip(message.monitor_info.tag_status_vec.iter())
        {
            *flags = (status.is_selected as u8)
                | ((status.is_urg as u8) << 1)
                | ((status.is_filled as u8) << 2)
                | ((status.is_occ as u8) << 3);
        }
        Self {
            timestamp: message.timestamp,
            monitor_num: message.monitor_info.monitor_num,
            monitor_width: message.monitor_info.monitor_width,
            monitor_height: message.monitor_info.monitor_height,
            monitor_x: message.monitor_info.monitor_x,
            monitor_y: message.monitor_info.monitor_y,
            tag_flags,
            client_name: message.monitor_info.client_name,
            ltsymbol: message.monitor_info.ltsymbol,
        }
    }
}

impl From<WireMessage> for SharedMessage {
    fn from(message: WireMessage) -> Self {
        let mut monitor_info = crate::MonitorInfo {
            monitor_num: message.monitor_num,
            monitor_width: message.monitor_width,
            monitor_height: message.monitor_height,
            monitor_x: message.monitor_x,
            monitor_y: message.monitor_y,
            client_name: message.client_name,
            ltsymbol: message.ltsymbol,
            ..crate::MonitorInfo::default()
        };
        for (status, flags) in monitor_info
            .tag_status_vec
            .iter_mut()
            .zip(message.tag_flags)
        {
            *status = TagStatus::new(
                flags & 1 != 0,
                flags & 2 != 0,
                flags & 4 != 0,
                flags & 8 != 0,
            );
        }
        Self {
            timestamp: message.timestamp,
            monitor_info,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct CommandSlot {
    checksum: u32,
    _padding: u32,
    command: SharedCommand,
}

struct Checksum(u32);

impl Checksum {
    const fn new() -> Self {
        Self(0x811c_9dc5)
    }

    #[inline(always)]
    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u32::from(*byte);
            self.0 = self.0.wrapping_mul(0x0100_0193);
        }
    }

    const fn finish(self) -> u32 {
        self.0
    }
}

fn calculate_message_checksum(message: &WireMessage) -> u32 {
    let mut checksum = Checksum::new();
    checksum.write(&message.timestamp.to_ne_bytes());
    checksum.write(&message.monitor_num.to_ne_bytes());
    checksum.write(&message.monitor_width.to_ne_bytes());
    checksum.write(&message.monitor_height.to_ne_bytes());
    checksum.write(&message.monitor_x.to_ne_bytes());
    checksum.write(&message.monitor_y.to_ne_bytes());
    checksum.write(&message.tag_flags);
    checksum.write(&message.client_name);
    checksum.write(&message.ltsymbol);
    checksum.finish()
}

fn calculate_command_checksum(command: &SharedCommand) -> u32 {
    let mut checksum = Checksum::new();
    checksum.write(&command.cmd_type.to_ne_bytes());
    checksum.write(&command.parameter.to_ne_bytes());
    checksum.write(&command.monitor_id.to_ne_bytes());
    checksum.write(&command.timestamp.to_ne_bytes());
    checksum.finish()
}

#[derive(Debug, Clone, Copy)]
struct BufferLayout {
    backend_offset: usize,
    messages_offset: usize,
    commands_offset: usize,
    total_size: usize,
}

impl BufferLayout {
    fn calculate(strategy: SyncStrategy, buffer_size: usize) -> Result<Self> {
        if !strategy.is_supported() {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "no synchronization backend is enabled",
            ));
        }
        if buffer_size == 0 || !buffer_size.is_power_of_two() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "message capacity must be a non-zero power of two",
            ));
        }
        u32::try_from(buffer_size).map_err(|_| {
            Error::new(
                ErrorKind::InvalidInput,
                "message capacity does not fit the shared protocol",
            )
        })?;

        let backend_offset =
            checked_align_up(size_of::<GenericHeader>(), strategy.backend_align())?;
        let after_backend = backend_offset
            .checked_add(strategy.backend_size())
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "backend layout overflows usize"))?;
        let messages_offset = checked_align_up(after_backend, align_of::<MessageSlot>())?;
        let messages_size = buffer_size
            .checked_mul(size_of::<MessageSlot>())
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "message layout overflows usize"))?;
        let after_messages = messages_offset
            .checked_add(messages_size)
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "message layout overflows usize"))?;
        let commands_offset = checked_align_up(after_messages, align_of::<CommandSlot>())?;
        let commands_size = CMD_BUFFER_SIZE
            .checked_mul(size_of::<CommandSlot>())
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "command layout overflows usize"))?;
        let total_size = commands_offset.checked_add(commands_size).ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                "shared-memory layout overflows usize",
            )
        })?;

        Ok(Self {
            backend_offset,
            messages_offset,
            commands_offset,
            total_size,
        })
    }
}

fn map_shmem_error(operation: &str, error: ShmemError) -> Error {
    let kind = match &error {
        ShmemError::LinkDoesNotExist => ErrorKind::NotFound,
        ShmemError::LinkExists | ShmemError::MappingIdExists => ErrorKind::AlreadyExists,
        ShmemError::LinkOpenFailed(source)
        | ShmemError::LinkCreateFailed(source)
        | ShmemError::LinkReadFailed(source)
        | ShmemError::LinkWriteFailed(source) => source.kind(),
        _ => ErrorKind::Other,
    };
    Error::new(kind, format!("{operation}: {error}"))
}

fn absolute_flink_path(path: &str) -> Result<PathBuf> {
    let requested = PathBuf::from(path);
    let target = if requested.is_absolute() {
        requested
    } else {
        std::env::current_dir()
            .map_err(|error| {
                Error::new(
                    error.kind(),
                    format!("failed to resolve relative flink path: {error}"),
                )
            })?
            .join(requested)
    };
    if target.file_name().is_none() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "shared-memory link path must name a file",
        ));
    }
    Ok(target)
}

/// Atomically publishes a fully initialized mapping without exposing a partially written flink.
fn publish_flink(target: &Path, os_id: &str) -> Result<PathBuf> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;

    let parent = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let nonce = FLINK_NONCE.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = parent.join(format!(
        ".shared_structures.{}.{}.{}.tmp",
        std::process::id(),
        timestamp,
        nonce
    ));

    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|error| {
                Error::new(
                    error.kind(),
                    format!("failed to create flink staging file: {error}"),
                )
            })?;
        file.write_all(os_id.as_bytes()).map_err(|error| {
            Error::new(
                error.kind(),
                format!("failed to write flink staging file: {error}"),
            )
        })?;
        file.sync_all().map_err(|error| {
            Error::new(
                error.kind(),
                format!("failed to sync flink staging file: {error}"),
            )
        })?;

        // A hard link is atomic and does not replace an existing target. Because the source is
        // in the same directory, readers can only observe a complete os_id.
        std::fs::hard_link(&temporary, target).map_err(|error| {
            Error::new(
                error.kind(),
                format!("failed to publish shared-memory flink: {error}"),
            )
        })?;
        Ok(target.to_path_buf())
    })();

    let _ = std::fs::remove_file(&temporary);
    result
}

struct CursorGuard<'a> {
    lock: &'a AtomicU32,
}

impl<'a> CursorGuard<'a> {
    fn acquire(cursor: &'a QueueCursor) -> Self {
        let mut attempts = 0u32;
        while cursor
            .lock
            .compare_exchange_weak(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            if attempts < 64 {
                std::hint::spin_loop();
                attempts += 1;
            } else {
                std::thread::yield_now();
            }
        }
        Self { lock: &cursor.lock }
    }
}

impl Drop for CursorGuard<'_> {
    fn drop(&mut self) {
        self.lock.store(0, Ordering::Release);
    }
}

/// 环形缓冲区的构建选项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SharedRingBufferOptions {
    strategy: SyncStrategy,
    buffer_size: usize,
    adaptive_poll_spins: u32,
}

impl Default for SharedRingBufferOptions {
    fn default() -> Self {
        Self {
            strategy: SyncStrategy::default(),
            buffer_size: DEFAULT_BUFFER_SIZE,
            adaptive_poll_spins: DEFAULT_ADAPTIVE_POLL_SPINS,
        }
    }
}

impl SharedRingBufferOptions {
    /// 创建一组默认选项。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 选择同步后端。
    #[must_use]
    pub const fn strategy(mut self, strategy: SyncStrategy) -> Self {
        self.strategy = strategy;
        self
    }

    /// 设置消息环容量（必须是非零的 2 的幂）。
    #[must_use]
    pub const fn capacity(mut self, buffer_size: usize) -> Self {
        self.buffer_size = buffer_size;
        self
    }

    /// 设置进入内核等待前的自适应自旋次数。
    #[must_use]
    pub const fn adaptive_poll_spins(mut self, spins: u32) -> Self {
        self.adaptive_poll_spins = spins;
        self
    }

    /// 排他创建新缓冲区。
    pub fn create(self, path: &str) -> Result<SharedRingBuffer> {
        SharedRingBuffer::create(
            path,
            self.strategy,
            Some(self.buffer_size),
            Some(self.adaptive_poll_spins),
        )
    }

    /// 打开并校验已有缓冲区的后端。
    pub fn open(self, path: &str) -> Result<SharedRingBuffer> {
        SharedRingBuffer::open(path, self.strategy, Some(self.adaptive_poll_spins))
    }

    /// 打开缓冲区；仅在确认链接不存在时创建。
    pub fn open_or_create(self, path: &str) -> Result<SharedRingBuffer> {
        SharedRingBuffer::open_or_create(
            path,
            self.strategy,
            Some(self.buffer_size),
            Some(self.adaptive_poll_spins),
        )
    }
}

/// 某一时刻的缓冲区状态快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SharedRingBufferStats {
    pub capacity: usize,
    pub available_messages: usize,
    pub command_capacity: usize,
    pub available_commands: usize,
    pub last_message_timestamp: u64,
    pub is_destroyed: bool,
    pub is_creator: bool,
    pub strategy: SyncStrategy,
}

/// 基于共享内存的有界环形缓冲区。
///
/// 每个方向使用进程共享锁保护游标，因此安全 API 即使被多线程调用
/// 也不会产生 slot 数据竞争；快速路径仍针对单生产者/单消费者优化。
pub struct SharedRingBuffer {
    shmem: Shmem,
    flink_path: Option<PathBuf>,
    header: *mut GenericHeader,
    message_slots: *mut MessageSlot,
    command_slots: *mut CommandSlot,
    is_creator: bool,
    adaptive_poll_spins: u32,
    strategy: SyncStrategy,
    backend: AnySyncBackend,
}

impl std::fmt::Debug for SharedRingBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedRingBuffer")
            .field("os_id", &self.shmem.get_os_id())
            .field("strategy", &self.strategy)
            .field("capacity", &self.capacity())
            .field("is_creator", &self.is_creator)
            .field("is_destroyed", &self.is_destroyed())
            .finish_non_exhaustive()
    }
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

// SAFETY: 映射在 `shmem` 的生命期内始终有效；四个可变队列方向分别由
// 共享 `QueueCursor::lock` 串行化，slot 则通过 Release/Acquire 游标交接。
unsafe impl Send for SharedRingBuffer {}
unsafe impl Sync for SharedRingBuffer {}

impl SharedRingBuffer {
    #[inline]
    fn header(&self) -> &GenericHeader {
        // SAFETY: constructors validate the complete mapping before constructing `Self`.
        unsafe { &*self.header }
    }

    #[inline]
    fn buffer_size(&self) -> u32 {
        self.header().buffer_size
    }

    #[inline]
    fn buffer_mask(&self) -> u32 {
        self.buffer_size() - 1
    }

    #[inline]
    fn cmd_buffer_mask(&self) -> u32 {
        self.header().command_buffer_size - 1
    }

    /// 使用默认选项打开或创建缓冲区的兼容辅助函数。
    pub fn create_shared_ring_buffer_aux(shared_path: &str) -> Option<Self> {
        Self::create_shared_ring_buffer(shared_path, SyncStrategy::default())
    }

    /// 打开或创建缓冲区的兼容辅助函数。
    ///
    /// 新代码应优先使用 [`SharedRingBufferOptions::open_or_create`]，以保留
    /// 完整的错误信息。
    pub fn create_shared_ring_buffer(shared_path: &str, strategy: SyncStrategy) -> Option<Self> {
        if shared_path.is_empty() {
            warn!("No shared path provided, cannot use shared ring buffer.");
            return None;
        }
        match Self::open_or_create(shared_path, strategy, None, None) {
            Ok(shared_buffer) => {
                info!("Opened or created shared ring buffer: {shared_path}");
                Some(shared_buffer)
            }
            Err(error) => {
                error!("Failed to open or create shared ring buffer: {error}");
                None
            }
        }
    }

    /// 使用默认后端创建缓冲区。
    pub fn create_aux(
        path: &str,
        buffer_size: Option<usize>,
        adaptive_poll_spins: Option<u32>,
    ) -> Result<Self> {
        Self::create(
            path,
            SyncStrategy::default(),
            buffer_size,
            adaptive_poll_spins,
        )
    }

    /// 排他创建一个新共享缓冲区。
    pub fn create(
        path: &str,
        strategy: SyncStrategy,
        buffer_size: Option<usize>,
        adaptive_poll_spins: Option<u32>,
    ) -> Result<Self> {
        if path.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "shared-memory link path must not be empty",
            ));
        }
        let flink_path = absolute_flink_path(path)?;
        let buffer_size = buffer_size.unwrap_or(DEFAULT_BUFFER_SIZE);
        let layout = BufferLayout::calculate(strategy, buffer_size)?;

        // Fast path only; the final hard-link publication remains the authoritative exclusive
        // create operation and closes the time-of-check/time-of-use race.
        match std::fs::symlink_metadata(&flink_path) {
            Ok(_) => {
                return Err(Error::new(
                    ErrorKind::AlreadyExists,
                    "shared-memory flink already exists",
                ));
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(Error::new(
                    error.kind(),
                    format!("failed to inspect shared-memory flink: {error}"),
                ));
            }
        }

        // Create the OS mapping privately. The public flink is hard-linked only after every
        // protocol field and backend resource is initialized, so an opener can never race with
        // construction of Rust atomic objects or plain metadata.
        let shmem = ShmemConf::new()
            .size(layout.total_size)
            .create()
            .map_err(|error| map_shmem_error("failed to create shared memory", error))?;

        let base_ptr = shmem.as_ptr();
        if (base_ptr as usize) % align_of::<GenericHeader>() != 0 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "shared-memory mapping is not sufficiently aligned",
            ));
        }

        let header = base_ptr as *mut GenericHeader;
        // SAFETY: the mapping has at least `size_of::<GenericHeader>()` writable bytes and the
        // pointer alignment was checked above. The public flink is not visible yet.
        unsafe {
            header.write(GenericHeader::new(
                RING_BUFFER_VERSION,
                layout.total_size as u64,
                buffer_size as u32,
                CMD_BUFFER_SIZE as u32,
                strategy.id(),
                size_of::<MessageSlot>() as u32,
                size_of::<CommandSlot>() as u32,
                LAYOUT_MARKER,
            ));
        }

        // SAFETY: checked layout offsets are within the mapping and correctly aligned.
        let backend_ptr = unsafe { base_ptr.add(layout.backend_offset) };
        let message_slots = unsafe { base_ptr.add(layout.messages_offset).cast::<MessageSlot>() };
        let command_slots = unsafe { base_ptr.add(layout.commands_offset).cast::<CommandSlot>() };
        let mut backend = Self::new_backend(strategy);
        backend.init(true, backend_ptr)?;

        // Publish readiness last. An opener that observes magic with Acquire also observes every
        // plain metadata field and the initialized backend.
        unsafe {
            (*header).magic.store(RING_BUFFER_MAGIC, Ordering::Release);
        }

        let flink_path = match publish_flink(&flink_path, shmem.get_os_id()) {
            Ok(path) => path,
            Err(error) => {
                backend.abort_init();
                return Err(error);
            }
        };

        Ok(Self {
            shmem,
            flink_path: Some(flink_path),
            header,
            message_slots,
            command_slots,
            is_creator: true,
            adaptive_poll_spins: adaptive_poll_spins.unwrap_or(DEFAULT_ADAPTIVE_POLL_SPINS),
            strategy,
            backend,
        })
    }

    /// 自动识别共享头中记录的后端并打开。
    pub fn open_aux(path: &str, adaptive_poll_spins: Option<u32>) -> Result<Self> {
        Self::open_auto(path, adaptive_poll_spins)
    }

    /// 自动识别后端并打开已有缓冲区。
    pub fn open_auto(path: &str, adaptive_poll_spins: Option<u32>) -> Result<Self> {
        Self::open_impl(path, None, adaptive_poll_spins)
    }

    /// 使用指定后端打开已有缓冲区。
    ///
    /// 头部记录的后端不匹配时返回 `InvalidData`，不会按错误布局解释内存。
    pub fn open(
        path: &str,
        strategy: SyncStrategy,
        adaptive_poll_spins: Option<u32>,
    ) -> Result<Self> {
        Self::open_impl(path, Some(strategy), adaptive_poll_spins)
    }

    fn open_impl(
        path: &str,
        expected_strategy: Option<SyncStrategy>,
        adaptive_poll_spins: Option<u32>,
    ) -> Result<Self> {
        if path.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "shared-memory link path must not be empty",
            ));
        }
        let shmem = ShmemConf::new()
            .flink(path)
            .open()
            .map_err(|error| map_shmem_error("failed to open shared memory", error))?;

        if shmem.len() < size_of::<GenericHeader>() {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "shared-memory mapping is shorter than the protocol header",
            ));
        }

        let base_ptr = shmem.as_ptr();
        if (base_ptr as usize) % align_of::<GenericHeader>() != 0 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "shared-memory mapping is not sufficiently aligned",
            ));
        }
        let header = base_ptr as *mut GenericHeader;

        // SAFETY: the fixed prefix length and alignment were checked above. Integer and atomic
        // integer fields accept every bit pattern.
        let magic = unsafe { (*header).magic.load(Ordering::Acquire) };
        if magic == 0 {
            return Err(Error::new(
                ErrorKind::WouldBlock,
                "shared-memory buffer is still initializing",
            ));
        }
        if magic != RING_BUFFER_MAGIC {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!("invalid shared-memory magic: {magic:#x}"),
            ));
        }
        let version = unsafe { (*header).version.load(Ordering::Relaxed) };
        if version != RING_BUFFER_VERSION {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!(
                    "incompatible shared-memory protocol version {version}; expected {RING_BUFFER_VERSION}"
                ),
            ));
        }

        let backend_id = unsafe { std::ptr::addr_of!((*header).backend_id).read() };
        let strategy = SyncStrategy::from_id(backend_id).ok_or_else(|| {
            let kind = if (1..=3).contains(&backend_id) {
                ErrorKind::Unsupported
            } else {
                ErrorKind::InvalidData
            };
            Error::new(
                kind,
                format!("shared-memory backend id {backend_id} is unavailable"),
            )
        })?;
        if !strategy.is_supported() {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "shared-memory buffer does not name a usable backend",
            ));
        }
        if let Some(expected) = expected_strategy {
            if expected != strategy {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    format!(
                        "synchronization backend mismatch: mapping uses {strategy}, caller requested {expected}"
                    ),
                ));
            }
        }

        let buffer_size = unsafe { std::ptr::addr_of!((*header).buffer_size).read() as usize };
        let command_buffer_size =
            unsafe { std::ptr::addr_of!((*header).command_buffer_size).read() as usize };
        let layout = BufferLayout::calculate(strategy, buffer_size).map_err(|error| {
            Error::new(
                ErrorKind::InvalidData,
                format!("invalid shared-memory layout metadata: {error}"),
            )
        })?;
        let recorded_total = unsafe { std::ptr::addr_of!((*header).total_size).read() };
        let message_slot_size =
            unsafe { std::ptr::addr_of!((*header).message_slot_size).read() as usize };
        let command_slot_size =
            unsafe { std::ptr::addr_of!((*header).command_slot_size).read() as usize };
        let marker = unsafe { std::ptr::addr_of!((*header).layout_marker).read() };

        if command_buffer_size != CMD_BUFFER_SIZE
            || message_slot_size != size_of::<MessageSlot>()
            || command_slot_size != size_of::<CommandSlot>()
            || marker != LAYOUT_MARKER
            || recorded_total != layout.total_size as u64
            || shmem.len() != layout.total_size
        {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "shared-memory layout metadata does not match this build",
            ));
        }
        if unsafe { (*header).is_destroyed.load(Ordering::Acquire) } != 0 {
            return Err(Error::new(
                ErrorKind::BrokenPipe,
                "shared-memory buffer is destroyed",
            ));
        }

        // SAFETY: the complete layout was checked against the exact mapping length.
        let backend_ptr = unsafe { base_ptr.add(layout.backend_offset) };
        let message_slots = unsafe { base_ptr.add(layout.messages_offset).cast::<MessageSlot>() };
        let command_slots = unsafe { base_ptr.add(layout.commands_offset).cast::<CommandSlot>() };
        let mut backend = Self::new_backend(strategy);
        backend.init(false, backend_ptr)?;

        Ok(Self {
            shmem,
            flink_path: None,
            header,
            message_slots,
            command_slots,
            is_creator: false,
            adaptive_poll_spins: adaptive_poll_spins.unwrap_or(DEFAULT_ADAPTIVE_POLL_SPINS),
            strategy,
            backend,
        })
    }

    /// 打开缓冲区；仅对确定的 NotFound 创建，并处理并发创建竞态。
    pub fn open_or_create(
        path: &str,
        strategy: SyncStrategy,
        buffer_size: Option<usize>,
        adaptive_poll_spins: Option<u32>,
    ) -> Result<Self> {
        let deadline = Instant::now() + OPEN_RETRY_TIMEOUT;
        let mut may_create = true;
        loop {
            match Self::open(path, strategy, adaptive_poll_spins) {
                Ok(buffer) => return Ok(buffer),
                Err(error) if error.kind() == ErrorKind::NotFound && may_create => {
                    match Self::create(path, strategy, buffer_size, adaptive_poll_spins) {
                        Ok(buffer) => return Ok(buffer),
                        Err(create_error)
                            if create_error.kind() == ErrorKind::AlreadyExists
                                && Instant::now() < deadline =>
                        {
                            may_create = false;
                        }
                        Err(create_error) => return Err(create_error),
                    }
                }
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::NotFound)
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => return Err(error),
            }
        }
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
            SyncStrategy::Unsupported => AnySyncBackend::_Unsupported,
        }
    }

    /// 尝试写入一条消息。队列已满时返回 `Ok(false)`。
    ///
    /// 通知后端只是等待优化；通知失败不会把已提交的消息伪装成
    /// 写入失败，避免调用者重试后产生重复消息。
    pub fn try_write_message(&self, message: &SharedMessage) -> Result<bool> {
        if self.is_destroyed() {
            return Err(Error::new(ErrorKind::BrokenPipe, "buffer is destroyed"));
        }

        let header = self.header();
        let guard = CursorGuard::acquire(&header.message_write);
        if self.is_destroyed() {
            return Err(Error::new(ErrorKind::BrokenPipe, "buffer is destroyed"));
        }

        unsafe {
            let write_idx = header.message_write.index.load(Ordering::Relaxed);
            let read_idx = header.message_read.index.load(Ordering::Acquire);
            if write_idx.wrapping_sub(read_idx) >= self.buffer_size() {
                return Ok(false);
            }

            let slot_idx = (write_idx & self.buffer_mask()) as usize;
            let wire_message = WireMessage::from(message);
            let written_at = now_millis();
            self.message_slots.add(slot_idx).write(MessageSlot {
                written_at,
                checksum: calculate_message_checksum(&wire_message),
                _padding: 0,
                message: wire_message,
            });
            header.last_timestamp.store(written_at, Ordering::Release);
            header
                .message_write
                .index
                .store(write_idx.wrapping_add(1), Ordering::Release);
        }
        drop(guard);

        if let Err(error) = self.backend.signal_message() {
            warn!("message committed, but waiter notification failed: {error}");
        }
        Ok(true)
    }

    /// 读取并移除最早的消息。
    pub fn try_read_next_message(&self) -> Result<Option<SharedMessage>> {
        if self.is_destroyed() {
            return Ok(None);
        }

        let header = self.header();
        let _guard = CursorGuard::acquire(&header.message_read);
        if self.is_destroyed() {
            return Ok(None);
        }

        unsafe {
            let write_idx = header.message_write.index.load(Ordering::Acquire);
            let read_idx = header.message_read.index.load(Ordering::Relaxed);
            if read_idx == write_idx {
                return Ok(None);
            }

            let slot_idx = (read_idx & self.buffer_mask()) as usize;
            let slot = self.message_slots.add(slot_idx).read();
            header
                .message_read
                .index
                .store(read_idx.wrapping_add(1), Ordering::Release);
            if calculate_message_checksum(&slot.message) != slot.checksum {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "message checksum mismatch",
                ));
            }
            Ok(Some(SharedMessage::from(slot.message)))
        }
    }

    /// 读取最新消息并一次丢弃它之前的所有待读消息。
    pub fn try_read_latest_message(&self) -> Result<Option<SharedMessage>> {
        if self.is_destroyed() {
            return Ok(None);
        }

        let header = self.header();
        let _guard = CursorGuard::acquire(&header.message_read);
        if self.is_destroyed() {
            return Ok(None);
        }

        unsafe {
            let write_idx = header.message_write.index.load(Ordering::Acquire);
            let read_idx = header.message_read.index.load(Ordering::Relaxed);
            if read_idx == write_idx {
                return Ok(None);
            }

            let newest_idx = write_idx.wrapping_sub(1);
            let slot_idx = (newest_idx & self.buffer_mask()) as usize;
            let slot = self.message_slots.add(slot_idx).read();
            header
                .message_read
                .index
                .store(write_idx, Ordering::Release);
            if calculate_message_checksum(&slot.message) != slot.checksum {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "latest message checksum mismatch",
                ));
            }
            Ok(Some(SharedMessage::from(slot.message)))
        }
    }

    /// 尝试写入命令。命令队列已满时返回 `Ok(false)`。
    pub fn send_command(&self, command: SharedCommand) -> Result<bool> {
        if self.is_destroyed() {
            return Err(Error::new(ErrorKind::BrokenPipe, "buffer is destroyed"));
        }

        let header = self.header();
        let guard = CursorGuard::acquire(&header.command_write);
        if self.is_destroyed() {
            return Err(Error::new(ErrorKind::BrokenPipe, "buffer is destroyed"));
        }

        unsafe {
            let write_idx = header.command_write.index.load(Ordering::Relaxed);
            let read_idx = header.command_read.index.load(Ordering::Acquire);
            if write_idx.wrapping_sub(read_idx) >= header.command_buffer_size {
                return Ok(false);
            }

            let slot_idx = (write_idx & self.cmd_buffer_mask()) as usize;
            self.command_slots.add(slot_idx).write(CommandSlot {
                checksum: calculate_command_checksum(&command),
                _padding: 0,
                command,
            });
            header
                .command_write
                .index
                .store(write_idx.wrapping_add(1), Ordering::Release);
        }
        drop(guard);

        if let Err(error) = self.backend.signal_command() {
            warn!("command committed, but waiter notification failed: {error}");
        }
        Ok(true)
    }

    /// 读取并移除最早的命令，并校验其完整性。
    pub fn try_receive_command(&self) -> Result<Option<SharedCommand>> {
        if self.is_destroyed() {
            return Ok(None);
        }

        let header = self.header();
        let _guard = CursorGuard::acquire(&header.command_read);
        if self.is_destroyed() {
            return Ok(None);
        }

        unsafe {
            let write_idx = header.command_write.index.load(Ordering::Acquire);
            let read_idx = header.command_read.index.load(Ordering::Relaxed);
            if read_idx == write_idx {
                return Ok(None);
            }

            let slot_idx = (read_idx & self.cmd_buffer_mask()) as usize;
            let slot = self.command_slots.add(slot_idx).read();
            header
                .command_read
                .index
                .store(read_idx.wrapping_add(1), Ordering::Release);
            if calculate_command_checksum(&slot.command) != slot.checksum {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "command checksum mismatch",
                ));
            }
            Ok(Some(slot.command))
        }
    }

    /// 读取命令的兼容 API。校验失败时记录错误并返回 `None`。
    #[must_use]
    pub fn receive_command(&self) -> Option<SharedCommand> {
        match self.try_receive_command() {
            Ok(command) => command,
            Err(error) => {
                warn!("discarded invalid command: {error}");
                None
            }
        }
    }

    /// 等待消息可读。
    pub fn wait_for_message(&self, timeout: Option<Duration>) -> Result<bool> {
        if self.is_destroyed() {
            return Ok(false);
        }
        let notified = self.backend.wait_for_message(
            || self.is_destroyed_for_wait() || self.has_message(),
            self.adaptive_poll_spins,
            timeout,
        )?;
        Ok(notified && !self.is_destroyed() && self.has_message())
    }

    /// 等待命令可读。
    pub fn wait_for_command(&self, timeout: Option<Duration>) -> Result<bool> {
        if self.is_destroyed() {
            return Ok(false);
        }
        let notified = self.backend.wait_for_command(
            || self.is_destroyed_for_wait() || self.has_command(),
            self.adaptive_poll_spins,
            timeout,
        )?;
        Ok(notified && !self.is_destroyed() && self.has_command())
    }

    /// 将整个共享缓冲区标记为已销毁并唤醒等待者。
    ///
    /// 该操作幂等；它不会立即删除 flink，flink 由 creator Drop 时清理。
    pub fn destroy(&self) -> Result<()> {
        // SeqCst pairs with backend waiter registration and the registered-waiter count read.
        // This closes the classic two-atomic registration/shutdown missed-wakeup window.
        self.header().is_destroyed.store(1, Ordering::SeqCst);
        // Even a repeated call retries the wake: a previous best-effort backend notification may
        // have failed after the destroyed flag itself was successfully published.
        self.backend.wake_all()
    }

    /// 返回缓冲区是否已销毁。
    #[inline]
    #[must_use]
    pub fn is_destroyed(&self) -> bool {
        self.header().is_destroyed.load(Ordering::Acquire) != 0
    }

    #[inline]
    fn is_destroyed_for_wait(&self) -> bool {
        self.header().is_destroyed.load(Ordering::SeqCst) != 0
    }

    /// 返回是否有待读消息。
    #[must_use]
    pub fn has_message(&self) -> bool {
        !self.is_destroyed() && self.available_messages() > 0
    }

    /// 返回消息环容量。
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.buffer_size() as usize
    }

    /// 返回当前待读消息数。
    #[must_use]
    pub fn available_messages(&self) -> usize {
        if self.is_destroyed() {
            return 0;
        }
        let header = self.header();
        let _write_guard = CursorGuard::acquire(&header.message_write);
        let _read_guard = CursorGuard::acquire(&header.message_read);
        header
            .message_write
            .index
            .load(Ordering::Acquire)
            .wrapping_sub(header.message_read.index.load(Ordering::Acquire))
            .min(header.buffer_size) as usize
    }

    /// 返回是否有待读命令。
    #[must_use]
    pub fn has_command(&self) -> bool {
        !self.is_destroyed() && self.available_commands() > 0
    }

    /// 返回命令环容量。
    #[must_use]
    pub fn command_capacity(&self) -> usize {
        self.header().command_buffer_size as usize
    }

    /// 返回当前待读命令数。
    #[must_use]
    pub fn available_commands(&self) -> usize {
        if self.is_destroyed() {
            return 0;
        }
        let header = self.header();
        let _write_guard = CursorGuard::acquire(&header.command_write);
        let _read_guard = CursorGuard::acquire(&header.command_read);
        header
            .command_write
            .index
            .load(Ordering::Acquire)
            .wrapping_sub(header.command_read.index.load(Ordering::Acquire))
            .min(header.command_buffer_size) as usize
    }

    /// 返回当前后端。
    #[must_use]
    pub const fn strategy(&self) -> SyncStrategy {
        self.strategy
    }

    /// 返回当前句柄是否创建了该映射。
    #[must_use]
    pub const fn is_creator(&self) -> bool {
        self.is_creator
    }

    /// 返回最近一次消息提交的 Unix 毫秒时间戳。
    #[must_use]
    pub fn last_message_timestamp(&self) -> u64 {
        self.header().last_timestamp.load(Ordering::Acquire)
    }

    /// 返回一份状态快照。
    #[must_use]
    pub fn stats(&self) -> SharedRingBufferStats {
        SharedRingBufferStats {
            capacity: self.capacity(),
            available_messages: self.available_messages(),
            command_capacity: self.command_capacity(),
            available_commands: self.available_commands(),
            last_message_timestamp: self.last_message_timestamp(),
            is_destroyed: self.is_destroyed(),
            is_creator: self.is_creator,
            strategy: self.strategy,
        }
    }
}

impl Drop for SharedRingBuffer {
    fn drop(&mut self) {
        if self.header.is_null() {
            return;
        }

        // A non-owner only detaches. The creator owns global shutdown and unlink semantics.
        if self.is_creator {
            let _ = self.destroy();
            if let Some(path) = self.flink_path.take() {
                let _ = std::fs::remove_file(path);
            }
        }
        self.backend.cleanup(self.is_creator);
    }
}

#[cfg(all(
    test,
    any(feature = "futex", feature = "semaphore", feature = "eventfd")
))]
mod tests {
    use super::*;
    use crate::shared_message::{
        CommandType, MonitorInfo, SharedCommand, SharedMessage, TagStatus,
    };
    use std::sync::{Arc, Barrier, Mutex};
    use std::time::Duration;

    // EventFd backend 的 socket 路径使用毫秒精度，并发创建会撞路径，用 Mutex 串行化
    #[cfg(feature = "eventfd")]
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
    fn test_creator_drop_removes_atomically_published_flink() {
        let path = mk_path("creator_unlink");
        let creator = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        assert!(Path::new(&path).is_file());
        drop(creator);
        assert!(!Path::new(&path).exists());
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
    fn test_create_zero_and_overflowing_capacity_fail_cleanly() {
        let zero_path = mk_path("zero_capacity");
        let zero = SharedRingBuffer::create_aux(&zero_path, Some(0), Some(0));
        assert_eq!(zero.unwrap_err().kind(), ErrorKind::InvalidInput);

        let huge_path = mk_path("huge_capacity");
        let huge_capacity = 1usize << (usize::BITS - 1);
        let huge = SharedRingBuffer::create_aux(&huge_path, Some(huge_capacity), Some(0));
        assert_eq!(huge.unwrap_err().kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn test_create_is_exclusive() {
        let path = mk_path("exclusive_create");
        let _creator = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        let duplicate = SharedRingBuffer::create_aux(&path, Some(16), Some(0));
        assert_eq!(duplicate.unwrap_err().kind(), ErrorKind::AlreadyExists);
    }

    #[test]
    fn test_concurrent_open_or_create_publishes_one_ready_mapping() {
        const PARTICIPANTS: usize = 4;

        let path = Arc::new(mk_path("concurrent_publish"));
        let barrier = Arc::new(Barrier::new(PARTICIPANTS + 1));
        let mut threads = Vec::with_capacity(PARTICIPANTS);
        for _ in 0..PARTICIPANTS {
            let path = Arc::clone(&path);
            let barrier = Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                SharedRingBufferOptions::new()
                    .capacity(32)
                    .adaptive_poll_spins(0)
                    .open_or_create(&path)
            }));
        }
        barrier.wait();

        let buffers: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap().unwrap())
            .collect();
        assert_eq!(
            buffers.iter().filter(|buffer| buffer.is_creator()).count(),
            1
        );
        assert!(buffers.iter().all(|buffer| buffer.capacity() == 32));
        assert!(buffers.windows(2).all(|pair| pair[0] == pair[1]));
    }

    #[test]
    fn test_options_and_stats_report_configuration() {
        let path = mk_path("options_stats");
        let buffer = SharedRingBufferOptions::new()
            .capacity(32)
            .adaptive_poll_spins(0)
            .create(&path)
            .unwrap();
        assert_eq!(buffer.capacity(), 32);
        assert_eq!(buffer.command_capacity(), CMD_BUFFER_SIZE);
        assert!(buffer.is_creator());
        assert_eq!(buffer.strategy(), SyncStrategy::default());

        buffer.try_write_message(&make_msg(9)).unwrap();
        let stats = buffer.stats();
        assert_eq!(stats.capacity, 32);
        assert_eq!(stats.available_messages, 1);
        assert_eq!(stats.command_capacity, CMD_BUFFER_SIZE);
        assert_eq!(stats.available_commands, 0);
        assert!(stats.last_message_timestamp > 0);
        assert!(!stats.is_destroyed);
        assert!(stats.is_creator);
    }

    #[test]
    fn test_open_rejects_mapping_shorter_than_header() {
        let path = mk_path("short_mapping");
        let _mapping = ShmemConf::new().size(8).flink(&path).create().unwrap();
        let error = SharedRingBuffer::open_aux(&path, Some(0)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
    }

    #[test]
    #[cfg(all(feature = "futex", feature = "semaphore"))]
    fn test_open_rejects_wrong_backend_and_auto_detects_actual_backend() {
        let path = mk_path("backend_identity");
        let creator =
            SharedRingBuffer::create(&path, SyncStrategy::Semaphore, Some(16), Some(0)).unwrap();
        let wrong = SharedRingBuffer::open(&path, SyncStrategy::Futex, Some(0)).unwrap_err();
        assert_eq!(wrong.kind(), ErrorKind::InvalidData);

        let opener = SharedRingBuffer::open_auto(&path, Some(0)).unwrap();
        assert_eq!(opener.strategy(), SyncStrategy::Semaphore);
        assert_eq!(opener.capacity(), creator.capacity());
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

        let mut mi = MonitorInfo {
            monitor_num: 7,
            monitor_width: 1920,
            monitor_height: 1080,
            ..MonitorInfo::default()
        };
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
    fn test_concurrent_producers_and_consumers_are_serialized_safely() {
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        const PRODUCERS: usize = 4;
        const CONSUMERS: usize = 4;
        const PER_PRODUCER: usize = 100;
        const TOTAL: usize = PRODUCERS * PER_PRODUCER;

        let path = mk_path("mpmc_serialized");
        let buffer = Arc::new(SharedRingBuffer::create_aux(&path, Some(128), Some(0)).unwrap());
        let barrier = Arc::new(Barrier::new(PRODUCERS + CONSUMERS + 1));
        let consumed = Arc::new(AtomicUsize::new(0));
        let received = Arc::new(Mutex::new(Vec::with_capacity(TOTAL)));
        let mut handles = Vec::new();

        for producer_id in 0..PRODUCERS {
            let buffer = Arc::clone(&buffer);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                for sequence in 0..PER_PRODUCER {
                    let id = (producer_id * PER_PRODUCER + sequence) as i32;
                    while !buffer.try_write_message(&make_msg(id)).unwrap() {
                        std::thread::yield_now();
                    }
                }
            }));
        }

        for _ in 0..CONSUMERS {
            let buffer = Arc::clone(&buffer);
            let barrier = Arc::clone(&barrier);
            let consumed = Arc::clone(&consumed);
            let received = Arc::clone(&received);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let deadline = Instant::now() + Duration::from_secs(2);
                loop {
                    if consumed.load(AtomicOrdering::Acquire) >= TOTAL {
                        break;
                    }
                    if let Some(message) = buffer.try_read_next_message().unwrap() {
                        received
                            .lock()
                            .unwrap()
                            .push(message.get_monitor_info().monitor_num);
                        consumed.fetch_add(1, AtomicOrdering::Release);
                    } else {
                        assert!(Instant::now() < deadline, "concurrent receive timed out");
                        std::thread::yield_now();
                    }
                }
            }));
        }

        barrier.wait();
        for handle in handles {
            handle.join().unwrap();
        }

        let mut received = received.lock().unwrap().clone();
        received.sort_unstable();
        assert_eq!(received, (0..TOTAL as i32).collect::<Vec<_>>());
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
    fn test_explicit_destroy_is_idempotent_and_visible_to_openers() {
        let path = mk_path("explicit_destroy");
        let creator = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        let opener = SharedRingBuffer::open_aux(&path, Some(0)).unwrap();

        creator.destroy().unwrap();
        creator.destroy().unwrap();
        assert!(creator.is_destroyed());
        assert!(opener.is_destroyed());
        assert_eq!(
            opener.try_write_message(&make_msg(1)).unwrap_err().kind(),
            ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn test_destroy_racing_wait_wakes_both_channels() {
        use std::sync::mpsc;

        for round in 0..8 {
            let path = mk_path(&format!("destroy_wait_race_{round}"));
            let creator = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
            let opener = Arc::new(SharedRingBuffer::open_aux(&path, Some(0)).unwrap());
            let barrier = Arc::new(Barrier::new(3));
            let (sender, receiver) = mpsc::channel();

            let message_opener = Arc::clone(&opener);
            let message_barrier = Arc::clone(&barrier);
            let message_sender = sender.clone();
            let message_waiter = std::thread::spawn(move || {
                message_barrier.wait();
                message_sender
                    .send(message_opener.wait_for_message(None))
                    .unwrap();
            });

            let command_opener = Arc::clone(&opener);
            let command_barrier = Arc::clone(&barrier);
            let command_waiter = std::thread::spawn(move || {
                command_barrier.wait();
                sender.send(command_opener.wait_for_command(None)).unwrap();
            });

            barrier.wait();
            if round % 2 == 0 {
                std::thread::yield_now();
            }
            creator.destroy().unwrap();

            for _ in 0..2 {
                let result = receiver
                    .recv_timeout(Duration::from_secs(1))
                    .expect("destroy did not wake a registered waiter")
                    .unwrap();
                assert!(!result);
            }
            message_waiter.join().unwrap();
            command_waiter.join().unwrap();
        }
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
            c.wait_for_message(Some(Duration::from_millis(500)))
                .unwrap()
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
    fn test_opener_drop_does_not_destroy_buffer() {
        let path = mk_path("opener_drop");
        let creator = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        {
            let _opener = SharedRingBuffer::open_aux(&path, Some(0)).unwrap();
            // opener 在此处 drop，不影响 creator 和其他 opener。
        }
        assert!(!creator.is_destroyed());
        assert!(creator.try_write_message(&make_msg(7)).unwrap());
        assert_eq!(
            creator
                .try_read_next_message()
                .unwrap()
                .unwrap()
                .get_monitor_info()
                .monitor_num,
            7
        );
    }

    #[test]
    fn test_multiple_opener_drops_do_not_destroy_buffer() {
        let path = mk_path("multi_opener_drop");
        let creator = SharedRingBuffer::create_aux(&path, Some(16), Some(0)).unwrap();
        // 多个 opener 依次创建并 drop。
        for _ in 0..3 {
            let _opener = SharedRingBuffer::open_aux(&path, Some(0)).unwrap();
        }
        assert!(!creator.is_destroyed());
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
                .message_write
                .index
                .load(std::sync::atomic::Ordering::Acquire)
        };
        assert_eq!(initial_widx, 0);

        for round in 0..3u32 {
            for _ in 0..8 {
                buf.try_write_message(&make_msg(0)).unwrap();
            }
            let widx = unsafe {
                (*header_ptr)
                    .message_write
                    .index
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
