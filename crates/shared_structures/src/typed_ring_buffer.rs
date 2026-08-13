//! 泛型双通道共享内存环形缓冲核心。
//!
//! [`TypedRingBuffer`] 把布局计算、跨进程方向锁、游标协调、同步后端和
//! 生命周期管理与具体 payload 解耦：任意满足 [`WireSafe`] 契约的
//! `#[repr(C)]` POD 类型都可以作为消息或命令通道的槽位类型。
//! 领域封装 [`SharedRingBuffer`](crate::SharedRingBuffer) 只是
//! `TypedRingBuffer<WireMessage, WireCommand>` 外加领域类型转换。

use crate::backends::common::{
    AnySyncBackend, GenericHeader, QueueCursor, SyncBackend, SyncStrategy,
};

use log::warn;
use shared_memory::{Shmem, ShmemConf, ShmemError};
use std::io::{Error, ErrorKind, Result, Write};
use std::marker::PhantomData;
use std::mem::{align_of, size_of};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const RING_BUFFER_MAGIC: u64 = 0x52494E47_42554646;
const RING_BUFFER_VERSION: u64 = 14;
const LAYOUT_MARKER: u32 = 0x5352_4234; // "SRB4"

/// mmap 对基址的对齐保证（Linux 基础页大小）。payload 对齐超过它时
/// 无法保证槽位对齐，布局计算直接拒绝。
const MAX_PAYLOAD_ALIGN: usize = 4096;
pub(crate) const DEFAULT_BUFFER_SIZE: usize = 16;
pub(crate) const DEFAULT_CMD_BUFFER_SIZE: usize = 16;
pub(crate) const DEFAULT_ADAPTIVE_POLL_SPINS: u32 = 400;
const OPEN_RETRY_TIMEOUT: Duration = Duration::from_millis(250);
static FLINK_NONCE: AtomicU64 = AtomicU64::new(0);

/// 可直接放入共享内存槽位的 payload 类型契约。
///
/// # Safety
///
/// 实现者必须保证：
///
/// 1. 类型是 `#[repr(C)]`（或 `#[repr(C, align(N))]`），布局在所有共享
///    同一映射的进程间完全一致；
/// 2. 类型**没有任何 padding 字节**——校验和按整体字节计算，读取
///    padding 是未定义行为；不满足时应加入显式的 `_reserved` 字段补齐；
/// 3. **每一种位模式都是合法值**：不含 `bool`、`char`、枚举、引用、
///    指针或任何有位有效性约束的字段——共享内存可能被其他进程写入
///    任意字节；
/// 4. 类型不含内部可变性与析构逻辑（`Copy` 已排除 `Drop`）；
/// 5. **对象的所有字节在任何时刻都已初始化**——实际上禁止 `union` 与
///    `MaybeUninit` 字段：校验和按整体字节读取对象，读未初始化字节是
///    未定义行为（无 padding 不足以保证这一点，小变体构造的 union
///    就是反例）；
/// 6. 对齐不得超过 4096（mmap 只保证页对齐，超页对齐的槽位无法保证；
///    布局计算会在运行期拒绝违例类型）。
///
/// 违反契约不会破坏队列协调（游标与锁独立于 payload），但会把未定义
/// 行为引入槽位读取路径。领域类型应转换为满足契约的 wire 表示后再
/// 进入队列，参见 crate 文档中 `WireMessage` 的做法。
pub unsafe trait WireSafe: Copy + 'static {
    /// 槽位类型指纹：创建时写入共享 header，打开时校验，用于拒绝
    /// "槽大小相同但类型不同"的错配打开（大小校验对此无能为力）。
    ///
    /// 默认实现哈希 `std::any::type_name::<Self>()`。注意 type name 含
    /// crate/模块路径：多个二进制各自定义同布局类型共享一个映射时，
    /// 两侧应把 `fingerprint` 覆写为同一常量。指纹是防错配自检，
    /// 不是安全边界——同名同路径但字段重排的类型仍检测不到，跨版本
    /// 布局纪律仍由使用者负责。
    #[must_use]
    fn fingerprint() -> u32 {
        fnv32_str(std::any::type_name::<Self>())
    }
}

/// 字符串 FNV-1a（const fn），用于默认类型指纹。
const fn fnv32_str(value: &str) -> u32 {
    let bytes = value.as_bytes();
    let mut hash = 0x811c_9dc5u32;
    let mut index = 0;
    while index < bytes.len() {
        hash ^= bytes[index] as u32;
        hash = hash.wrapping_mul(0x0100_0193);
        index += 1;
    }
    hash
}

// SAFETY: 固定宽度整数与 IEEE-754 浮点数无 padding、任意位模式有效、
// 无内部可变性。有意不包含 `bool`/`char`（位有效性约束）与
// `usize`/`isize`（宽度随目标平台变化，跨进程契约含糊）。
macro_rules! impl_wire_safe_for_primitive {
    ($($ty:ty),* $(,)?) => {
        $(unsafe impl WireSafe for $ty {})*
    };
}
impl_wire_safe_for_primitive!(u8, u16, u32, u64, u128, i8, i16, i32, i64, i128, f32, f64);

// SAFETY: 元素无 padding 时数组按元素紧密排列（stride == size），整体
// 仍无 padding；其余契约逐元素继承。
unsafe impl<T: WireSafe, const N: usize> WireSafe for [T; N] {}

/// 泛型槽位：校验和 + payload。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct Slot<T> {
    pub(crate) checksum: u32,
    pub(crate) _padding: u32,
    pub(crate) payload: T,
}

/// FNV-1a 的 8 字节块化变体：吞吐比逐字节版本高约一个量级，
/// 结果仍是确定性的（同一构建下跨进程一致）。尾部不足 8 字节的块
/// 补零；payload 长度在协议内固定，无歧义。
struct Checksum(u64);

impl Checksum {
    const fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    #[inline(always)]
    fn write(&mut self, bytes: &[u8]) {
        const PRIME: u64 = 0x0000_0100_0000_01b3;
        let mut chunks = bytes.chunks_exact(8);
        for chunk in &mut chunks {
            let word = u64::from_ne_bytes(chunk.try_into().expect("chunks_exact yields 8 bytes"));
            self.0 = (self.0 ^ word).wrapping_mul(PRIME);
        }
        let remainder = chunks.remainder();
        if !remainder.is_empty() {
            let mut word = [0u8; 8];
            word[..remainder.len()].copy_from_slice(remainder);
            self.0 = (self.0 ^ u64::from_ne_bytes(word)).wrapping_mul(PRIME);
        }
    }

    const fn finish(self) -> u32 {
        (self.0 ^ (self.0 >> 32)) as u32
    }
}

/// 对 payload 的全部字节计算校验和。
pub(crate) fn checksum_of<T: WireSafe>(value: &T) -> u32 {
    // SAFETY: `WireSafe` 契约保证 T 无 padding 字节，因此整个对象
    // 表示都是已初始化内存。
    let bytes = unsafe {
        std::slice::from_raw_parts(std::ptr::from_ref(value).cast::<u8>(), size_of::<T>())
    };
    let mut checksum = Checksum::new();
    checksum.write(bytes);
    checksum.finish()
}

/// 从 `/proc/<pid>/stat` 中提取进程状态字段。
///
/// 第二个字段 `comm` 被括号包围，但进程名本身可以包含空格和右括号，
/// 因此必须从末尾的右括号定位第三个字段，不能简单按空白切分。
fn proc_stat_state(stat: &[u8]) -> Option<u8> {
    let comm_end = stat.iter().rposition(|byte| *byte == b')')?;
    stat.get(comm_end + 1..)?
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
}

/// 返回给定 PID 的进程当前是否还能执行用户代码。
///
/// Linux 会为尚未被父进程 `wait` 的僵尸保留 `/proc/<pid>` 目录；只检查
/// 目录存在会把已经不可能释放方向锁的僵尸误判为存活。因此这里读取
/// `/proc/<pid>/stat`，把 zombie/dead 状态（Z/X/x）也视为已退出。
///
/// 防御：`/proc` 不可用、stat 无法读取或内容无法解析时无从可靠判断，
/// 返回"存活"——夺锁/回收机制退化为纯等待，绝不基于失明的探测误夺
/// 活锁。跨 PID namespace 部署的前提要求见 SAFETY.md。
#[inline]
pub(crate) fn process_alive(pid: u32) -> bool {
    if std::fs::metadata("/proc/self/stat").is_err() {
        return true;
    }

    let stat_path = Path::new("/proc").join(pid.to_string()).join("stat");
    match std::fs::read(stat_path) {
        Ok(stat) => !matches!(proc_stat_state(&stat), Some(b'Z' | b'X' | b'x')),
        Err(error) if error.kind() == ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

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
pub(crate) fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Debug, Clone, Copy)]
struct BufferLayout {
    backend_offset: usize,
    messages_offset: usize,
    commands_offset: usize,
    total_size: usize,
}

impl BufferLayout {
    fn calculate<M: WireSafe, C: WireSafe>(
        strategy: SyncStrategy,
        buffer_size: usize,
        command_buffer_size: usize,
    ) -> Result<Self> {
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
        if command_buffer_size == 0 || !command_buffer_size.is_power_of_two() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "command capacity must be a non-zero power of two",
            ));
        }
        if align_of::<Slot<M>>() > MAX_PAYLOAD_ALIGN || align_of::<Slot<C>>() > MAX_PAYLOAD_ALIGN {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "payload alignment exceeds the page-alignment guarantee of the mapping",
            ));
        }
        u32::try_from(buffer_size)
            .and(u32::try_from(command_buffer_size))
            .map_err(|_| {
                Error::new(
                    ErrorKind::InvalidInput,
                    "queue capacity does not fit the shared protocol",
                )
            })?;

        let backend_offset =
            checked_align_up(size_of::<GenericHeader>(), strategy.backend_align())?;
        let after_backend = backend_offset
            .checked_add(strategy.backend_size())
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "backend layout overflows usize"))?;
        let messages_offset = checked_align_up(after_backend, align_of::<Slot<M>>())?;
        let messages_size = buffer_size
            .checked_mul(size_of::<Slot<M>>())
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "message layout overflows usize"))?;
        let after_messages = messages_offset
            .checked_add(messages_size)
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "message layout overflows usize"))?;
        let commands_offset = checked_align_up(after_messages, align_of::<Slot<C>>())?;
        let commands_size = command_buffer_size
            .checked_mul(size_of::<Slot<C>>())
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

/// 跨进程方向锁的 RAII guard。
///
/// 锁字存放持有者 PID（0 表示空闲）。持有者在临界区内崩溃时，其他
/// 进程会通过 `/proc/<pid>` 探测发现持有者已死并原子夺回锁；半写的
/// slot 由校验和兜底，未发布的游标推进随崩溃一起丢弃。
///
/// 已知限制：PID 复用可能让探测误判"仍存活"，代价是退回崩溃前的
/// 行为（等待）而不是错误夺锁，安全方向不变。
pub(crate) struct CursorGuard<'a> {
    lock: &'a AtomicU32,
}

impl<'a> CursorGuard<'a> {
    /// 每多少次失败尝试做一次持有者存活探测（探测本身约一次微秒级 statx）。
    const LIVENESS_CHECK_INTERVAL: u32 = 256;
    const SPIN_LIMIT: u32 = 64;

    pub(crate) fn acquire(cursor: &'a QueueCursor) -> Self {
        let self_pid = std::process::id();
        let mut attempts = 0u32;
        loop {
            match cursor.lock.compare_exchange_weak(
                0,
                self_pid,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Self { lock: &cursor.lock },
                Err(holder) => {
                    attempts = attempts.wrapping_add(1);
                    if holder != 0
                        && holder != self_pid
                        && attempts % Self::LIVENESS_CHECK_INTERVAL == 0
                        && !process_alive(holder)
                        && cursor
                            .lock
                            .compare_exchange(
                                holder,
                                self_pid,
                                Ordering::Acquire,
                                Ordering::Relaxed,
                            )
                            .is_ok()
                    {
                        warn!(
                            "reclaimed shared direction lock from dead process {holder}; \
                             a torn slot, if any, is caught by its checksum"
                        );
                        return Self { lock: &cursor.lock };
                    }
                    if attempts < Self::SPIN_LIMIT {
                        std::hint::spin_loop();
                    } else {
                        std::thread::yield_now();
                    }
                }
            }
        }
    }
}

impl Drop for CursorGuard<'_> {
    fn drop(&mut self) {
        self.lock.store(0, Ordering::Release);
    }
}

/// 环形缓冲区的构建选项。
///
/// 领域封装（[`SharedRingBuffer`](crate::SharedRingBuffer)）与泛型
/// [`TypedRingBuffer`] 共用同一组选项；后者通过 `*_typed` 方法构建。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SharedRingBufferOptions {
    pub(crate) strategy: SyncStrategy,
    pub(crate) buffer_size: usize,
    pub(crate) command_buffer_size: usize,
    pub(crate) adaptive_poll_spins: u32,
    pub(crate) reclaim_stale: bool,
}

impl Default for SharedRingBufferOptions {
    fn default() -> Self {
        Self {
            strategy: SyncStrategy::default(),
            buffer_size: DEFAULT_BUFFER_SIZE,
            command_buffer_size: DEFAULT_CMD_BUFFER_SIZE,
            adaptive_poll_spins: DEFAULT_ADAPTIVE_POLL_SPINS,
            reclaim_stale: false,
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

    /// 设置命令环容量（必须是非零的 2 的幂，默认 16）。
    #[must_use]
    pub const fn command_capacity(mut self, command_buffer_size: usize) -> Self {
        self.command_buffer_size = command_buffer_size;
        self
    }

    /// 设置进入内核等待前的自适应自旋次数。
    #[must_use]
    pub const fn adaptive_poll_spins(mut self, spins: u32) -> Self {
        self.adaptive_poll_spins = spins;
        self
    }

    /// 允许 open-or-create 入口回收创建者已死的残留映射。
    ///
    /// 创建者崩溃（未执行 Drop）会留下 flink 与映射，此后 `create` 报
    /// `AlreadyExists`、`open` 得到一个永远不会被销毁的僵尸缓冲区。开启
    /// 本选项后，open-or-create 打开映射时若发现创建者进程已不存在，
    /// 会移除旧 flink 并重新创建。
    ///
    /// 应仅由单一"监督者"角色开启：多个进程同时回收同一路径时可能
    /// 互相删除对方刚发布的 flink；PID 复用也可能让残留映射被误判为
    /// 仍然存活（此时行为退回默认，即打开旧映射）。
    #[must_use]
    pub const fn reclaim_stale(mut self, reclaim: bool) -> Self {
        self.reclaim_stale = reclaim;
        self
    }

    /// 用自定义 payload 类型排他创建新缓冲区。
    pub fn create_typed<M: WireSafe, C: WireSafe>(
        self,
        path: &str,
    ) -> Result<TypedRingBuffer<M, C>> {
        TypedRingBuffer::create_impl(
            path,
            self.strategy,
            self.buffer_size,
            self.command_buffer_size,
            self.adaptive_poll_spins,
        )
    }

    /// 用自定义 payload 类型打开并校验已有缓冲区。
    pub fn open_typed<M: WireSafe, C: WireSafe>(self, path: &str) -> Result<TypedRingBuffer<M, C>> {
        TypedRingBuffer::open_impl(path, Some(self.strategy), Some(self.adaptive_poll_spins))
    }

    /// 用自定义 payload 类型打开缓冲区；仅在确认链接不存在（或确认可
    /// 回收，见 [`reclaim_stale`](Self::reclaim_stale)）时创建。
    pub fn open_or_create_typed<M: WireSafe, C: WireSafe>(
        self,
        path: &str,
    ) -> Result<TypedRingBuffer<M, C>> {
        TypedRingBuffer::open_or_create_impl(path, self)
    }
}

/// 一次有界等待的结局。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitOutcome {
    /// 返回时目标通道确有数据可读（仍可能被其他消费者抢走）。
    Ready,
    /// 等待超时，未观察到数据。
    TimedOut,
    /// 缓冲区已被销毁，后续等待不会再有结果。
    Destroyed,
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

/// 基于共享内存的泛型有界双通道环形缓冲区。
///
/// `M` 是消息通道的槽位类型，`C` 是命令通道的槽位类型；二者都必须
/// 满足 [`WireSafe`] 契约。每个方向使用进程共享锁保护游标，因此安全
/// API 即使被多线程调用也不会产生 slot 数据竞争；快速路径仍针对单
/// 生产者/单消费者优化。
///
/// # Example
///
/// ```no_run
/// use shared_structures::{SharedRingBufferOptions, TypedRingBuffer, WireSafe};
///
/// #[repr(C)]
/// #[derive(Clone, Copy, PartialEq, Debug)]
/// struct Sample {
///     sequence: u64,
///     value: f64,
/// }
/// // SAFETY: repr(C)、无 padding（8+8 字节）、所有位模式有效、无内部可变性。
/// unsafe impl WireSafe for Sample {}
///
/// # fn main() -> std::io::Result<()> {
/// let ring: TypedRingBuffer<Sample, u64> = SharedRingBufferOptions::new()
///     .capacity(64)
///     .create_typed("/tmp/typed-ring-example")?;
/// ring.try_write_message(&Sample { sequence: 1, value: 0.5 })?;
/// assert!(ring.try_read_next_message()?.is_some());
/// # Ok(())
/// # }
/// ```
pub struct TypedRingBuffer<M: WireSafe, C: WireSafe> {
    shmem: Shmem,
    flink_path: Option<PathBuf>,
    pub(crate) header: *mut GenericHeader,
    pub(crate) message_slots: *mut Slot<M>,
    pub(crate) command_slots: *mut Slot<C>,
    is_creator: bool,
    adaptive_poll_spins: u32,
    strategy: SyncStrategy,
    backend: AnySyncBackend,
    _payload: PhantomData<(M, C)>,
}

impl<M: WireSafe, C: WireSafe> std::fmt::Debug for TypedRingBuffer<M, C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TypedRingBuffer")
            .field("os_id", &self.shmem.get_os_id())
            .field("strategy", &self.strategy)
            .field("capacity", &self.capacity())
            .field("is_creator", &self.is_creator)
            .field("is_destroyed", &self.is_destroyed())
            .finish_non_exhaustive()
    }
}

impl<M: WireSafe, C: WireSafe> std::hash::Hash for TypedRingBuffer<M, C> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.shmem.get_os_id().hash(state);
    }
}
impl<M: WireSafe, C: WireSafe> PartialEq for TypedRingBuffer<M, C> {
    fn eq(&self, other: &Self) -> bool {
        self.shmem.get_os_id() == other.shmem.get_os_id()
    }
}
impl<M: WireSafe, C: WireSafe> Eq for TypedRingBuffer<M, C> {}

// SAFETY: 映射在 `shmem` 的生命期内始终有效；四个可变队列方向分别由
// 共享 `QueueCursor::lock` 串行化，slot 则通过 Release/Acquire 游标交接。
// `WireSafe` payload 是 Copy 的纯数据，不含线程亲和状态。
unsafe impl<M: WireSafe, C: WireSafe> Send for TypedRingBuffer<M, C> {}
unsafe impl<M: WireSafe, C: WireSafe> Sync for TypedRingBuffer<M, C> {}

impl<M: WireSafe, C: WireSafe> TypedRingBuffer<M, C> {
    #[inline]
    pub(crate) fn header(&self) -> &GenericHeader {
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

    /// 自动识别后端并打开已有缓冲区。
    pub fn open_auto(path: &str, adaptive_poll_spins: Option<u32>) -> Result<Self> {
        Self::open_impl(path, None, adaptive_poll_spins)
    }

    pub(crate) fn create_impl(
        path: &str,
        strategy: SyncStrategy,
        buffer_size: usize,
        command_buffer_size: usize,
        adaptive_poll_spins: u32,
    ) -> Result<Self> {
        if path.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "shared-memory link path must not be empty",
            ));
        }
        let flink_path = absolute_flink_path(path)?;
        let layout = BufferLayout::calculate::<M, C>(strategy, buffer_size, command_buffer_size)?;

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
                command_buffer_size as u32,
                strategy.id(),
                size_of::<Slot<M>>() as u32,
                size_of::<Slot<C>>() as u32,
                LAYOUT_MARKER,
                std::process::id(),
                M::fingerprint(),
                C::fingerprint(),
            ));
        }

        // SAFETY: checked layout offsets are within the mapping and correctly aligned.
        let backend_ptr = unsafe { base_ptr.add(layout.backend_offset) };
        let message_slots = unsafe { base_ptr.add(layout.messages_offset).cast::<Slot<M>>() };
        let command_slots = unsafe { base_ptr.add(layout.commands_offset).cast::<Slot<C>>() };
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
            adaptive_poll_spins,
            strategy,
            backend,
            _payload: PhantomData,
        })
    }

    pub(crate) fn open_impl(
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
        // 与 create 使用同一套路径归一化，避免同一参数在两端解析出不同目标。
        let flink_path = absolute_flink_path(path)?;
        let shmem = ShmemConf::new()
            .flink(&flink_path)
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
        let layout = BufferLayout::calculate::<M, C>(strategy, buffer_size, command_buffer_size)
            .map_err(|error| {
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

        if message_slot_size != size_of::<Slot<M>>()
            || command_slot_size != size_of::<Slot<C>>()
            || marker != LAYOUT_MARKER
            || recorded_total != layout.total_size as u64
            || shmem.len() != layout.total_size
        {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "shared-memory layout metadata does not match this build",
            ));
        }
        let message_fingerprint =
            unsafe { std::ptr::addr_of!((*header).message_fingerprint).read() };
        let command_fingerprint =
            unsafe { std::ptr::addr_of!((*header).command_fingerprint).read() };
        if message_fingerprint != M::fingerprint() || command_fingerprint != C::fingerprint() {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "shared-memory payload type fingerprint mismatch; \
                 the mapping was created with different slot types",
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
        let message_slots = unsafe { base_ptr.add(layout.messages_offset).cast::<Slot<M>>() };
        let command_slots = unsafe { base_ptr.add(layout.commands_offset).cast::<Slot<C>>() };
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
            _payload: PhantomData,
        })
    }

    pub(crate) fn open_or_create_impl(
        path: &str,
        options: SharedRingBufferOptions,
    ) -> Result<Self> {
        let deadline = Instant::now() + OPEN_RETRY_TIMEOUT;
        let mut may_create = true;
        let mut may_reclaim = options.reclaim_stale;
        let create = |may_create: &mut bool| -> Result<Option<Self>> {
            match options.create_typed(path) {
                Ok(buffer) => Ok(Some(buffer)),
                Err(create_error)
                    if create_error.kind() == ErrorKind::AlreadyExists
                        && Instant::now() < deadline =>
                {
                    *may_create = false;
                    Ok(None)
                }
                Err(create_error) => Err(create_error),
            }
        };
        loop {
            match Self::open_impl(
                path,
                Some(options.strategy),
                Some(options.adaptive_poll_spins),
            ) {
                Ok(buffer) => {
                    if may_reclaim && !buffer.is_creator() && !buffer.creator_alive() {
                        // 只回收一次：若竞争者抢先重建，第二轮 open 到的就是新映射。
                        may_reclaim = false;
                        warn!(
                            "reclaiming stale shared ring buffer {path}: creator {} is gone",
                            buffer.creator_pid()
                        );
                        drop(buffer);
                        let _ = std::fs::remove_file(absolute_flink_path(path)?);
                        if let Some(buffer) = create(&mut may_create)? {
                            return Ok(buffer);
                        }
                        continue;
                    }
                    return Ok(buffer);
                }
                Err(error) if error.kind() == ErrorKind::NotFound && may_create => {
                    if let Some(buffer) = create(&mut may_create)? {
                        return Ok(buffer);
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
    pub fn try_write_message(&self, payload: &M) -> Result<bool> {
        if self.is_destroyed() {
            return Err(Error::new(ErrorKind::BrokenPipe, "buffer is destroyed"));
        }

        // 校验和与时间戳都不依赖锁内状态，放在临界区外完成，
        // 缩短多生产者争用窗口。
        let slot = Slot {
            checksum: checksum_of(payload),
            _padding: 0,
            payload: *payload,
        };
        let written_at = now_millis();

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
            self.message_slots.add(slot_idx).write(slot);
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

    /// 写入一条消息；队列满时覆盖最旧的一条待读消息，因此总是成功。
    ///
    /// 适合"只关心最新状态"的广播场景（与 [`try_read_latest_message`]
    /// 配对）。覆盖通过短暂持有读方向锁推进读游标实现，被覆盖的消息
    /// 对所有消费者都不可见。
    ///
    /// [`try_read_latest_message`]: Self::try_read_latest_message
    pub fn write_message_overwrite(&self, payload: &M) -> Result<()> {
        if self.is_destroyed() {
            return Err(Error::new(ErrorKind::BrokenPipe, "buffer is destroyed"));
        }

        let slot = Slot {
            checksum: checksum_of(payload),
            _padding: 0,
            payload: *payload,
        };
        let written_at = now_millis();

        let header = self.header();
        let guard = CursorGuard::acquire(&header.message_write);
        if self.is_destroyed() {
            return Err(Error::new(ErrorKind::BrokenPipe, "buffer is destroyed"));
        }

        unsafe {
            let write_idx = header.message_write.index.load(Ordering::Relaxed);
            let read_idx = header.message_read.index.load(Ordering::Acquire);
            if write_idx.wrapping_sub(read_idx) >= self.buffer_size() {
                // 锁序固定为「写锁 → 读锁」，与其他多锁路径一致，不会死锁。
                let _read_guard = CursorGuard::acquire(&header.message_read);
                let read_idx = header.message_read.index.load(Ordering::Relaxed);
                if write_idx.wrapping_sub(read_idx) >= self.buffer_size() {
                    header
                        .message_read
                        .index
                        .store(read_idx.wrapping_add(1), Ordering::Release);
                }
            }

            let slot_idx = (write_idx & self.buffer_mask()) as usize;
            self.message_slots.add(slot_idx).write(slot);
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
        Ok(())
    }

    /// 读取并移除最早的消息。
    ///
    /// 校验和不匹配时返回 `InvalidData`；此时损坏的 slot **已被消费**
    /// （读游标已推进），下一次调用会读取后续消息，损坏内容不会重复
    /// 出现，也无法找回。
    pub fn try_read_next_message(&self) -> Result<Option<M>> {
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
            if checksum_of(&slot.payload) != slot.checksum {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "message checksum mismatch",
                ));
            }
            Ok(Some(slot.payload))
        }
    }

    /// 读取最新消息并一次丢弃它之前的所有待读消息。
    ///
    /// 校验和不匹配时返回 `InvalidData`，此时读游标同样已推进到写游标：
    /// 包括可能完好的更早消息在内的**全部**待读消息都已被丢弃。
    pub fn try_read_latest_message(&self) -> Result<Option<M>> {
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
            if checksum_of(&slot.payload) != slot.checksum {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "latest message checksum mismatch",
                ));
            }
            Ok(Some(slot.payload))
        }
    }

    /// 复制最早的待读消息但不移除它。
    ///
    /// 校验和不匹配时返回 `InvalidData` 且**不**推进读游标；随后的
    /// [`try_read_next_message`](Self::try_read_next_message) 会消费并跳过
    /// 该损坏 slot。
    pub fn try_peek_message(&self) -> Result<Option<M>> {
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
            if checksum_of(&slot.payload) != slot.checksum {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "message checksum mismatch",
                ));
            }
            Ok(Some(slot.payload))
        }
    }

    /// 单次持锁批量读取至多 `max` 条消息。
    ///
    /// 相比循环调用 [`try_read_next_message`](Self::try_read_next_message)，
    /// 只做一次方向锁获取。校验和不匹配的 slot 会被跳过并记录警告，
    /// 不会中断整个批次。
    pub fn drain_messages(&self, max: usize) -> Result<Vec<M>> {
        if max == 0 || self.is_destroyed() {
            return Ok(Vec::new());
        }

        let header = self.header();
        let _guard = CursorGuard::acquire(&header.message_read);
        if self.is_destroyed() {
            return Ok(Vec::new());
        }

        let mut drained = Vec::new();
        unsafe {
            let write_idx = header.message_write.index.load(Ordering::Acquire);
            let mut read_idx = header.message_read.index.load(Ordering::Relaxed);
            while read_idx != write_idx && drained.len() < max {
                let slot_idx = (read_idx & self.buffer_mask()) as usize;
                let slot = self.message_slots.add(slot_idx).read();
                read_idx = read_idx.wrapping_add(1);
                header.message_read.index.store(read_idx, Ordering::Release);
                if checksum_of(&slot.payload) == slot.checksum {
                    drained.push(slot.payload);
                } else {
                    warn!("skipped a corrupt message slot while draining");
                }
            }
        }
        Ok(drained)
    }

    /// 尝试写入命令。命令队列已满时返回 `Ok(false)`。
    pub fn try_send_command(&self, command: C) -> Result<bool> {
        if self.is_destroyed() {
            return Err(Error::new(ErrorKind::BrokenPipe, "buffer is destroyed"));
        }

        let slot = Slot {
            checksum: checksum_of(&command),
            _padding: 0,
            payload: command,
        };
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
            self.command_slots.add(slot_idx).write(slot);
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
    pub fn try_receive_command(&self) -> Result<Option<C>> {
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
            if checksum_of(&slot.payload) != slot.checksum {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "command checksum mismatch",
                ));
            }
            Ok(Some(slot.payload))
        }
    }

    /// 等待消息可读。返回 `Ok(true)` 表示当下确有消息可读。
    pub fn wait_for_message(&self, timeout: Option<Duration>) -> Result<bool> {
        Ok(matches!(self.wait_message(timeout)?, WaitOutcome::Ready))
    }

    /// 等待命令可读。返回 `Ok(true)` 表示当下确有命令可读。
    pub fn wait_for_command(&self, timeout: Option<Duration>) -> Result<bool> {
        Ok(matches!(self.wait_command(timeout)?, WaitOutcome::Ready))
    }

    /// 等待消息可读，并区分「可读 / 超时 / 已销毁」三种结局。
    ///
    /// `timeout` 为 `None` 时只会以 [`WaitOutcome::Ready`] 或
    /// [`WaitOutcome::Destroyed`] 返回。返回 [`WaitOutcome::Ready`] 后消息
    /// 仍可能被其他消费者抢走；需要独占语义时用
    /// [`read_message_timeout`](Self::read_message_timeout)。
    pub fn wait_message(&self, timeout: Option<Duration>) -> Result<WaitOutcome> {
        self.wait_channel(timeout, true)
    }

    /// 等待命令可读，语义同 [`wait_message`](Self::wait_message)。
    pub fn wait_command(&self, timeout: Option<Duration>) -> Result<WaitOutcome> {
        self.wait_channel(timeout, false)
    }

    fn wait_channel(&self, timeout: Option<Duration>, is_message: bool) -> Result<WaitOutcome> {
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        loop {
            if self.is_destroyed() {
                return Ok(WaitOutcome::Destroyed);
            }
            let has_data = if is_message {
                self.has_message()
            } else {
                self.has_command()
            };
            if has_data {
                return Ok(WaitOutcome::Ready);
            }
            let remaining = match deadline {
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Ok(WaitOutcome::TimedOut);
                    }
                    Some(deadline - now)
                }
                None => None,
            };
            if is_message {
                self.backend.wait_for_message(
                    || self.is_destroyed_for_wait() || self.has_message(),
                    self.adaptive_poll_spins,
                    remaining,
                )?;
            } else {
                self.backend.wait_for_command(
                    || self.is_destroyed_for_wait() || self.has_command(),
                    self.adaptive_poll_spins,
                    remaining,
                )?;
            }
        }
    }

    /// 阻塞读取一条消息：内部完成「等待 → 读取 → 被抢走则重试」循环。
    ///
    /// 返回 `Ok(None)` 表示超时或缓冲区被销毁；校验和错误原样上抛
    /// （对应的损坏 slot 已被消费）。
    pub fn read_message_timeout(&self, timeout: Option<Duration>) -> Result<Option<M>> {
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        loop {
            if let Some(message) = self.try_read_next_message()? {
                return Ok(Some(message));
            }
            let remaining = match deadline {
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Ok(None);
                    }
                    Some(deadline - now)
                }
                None => None,
            };
            match self.wait_message(remaining)? {
                WaitOutcome::Ready => {}
                WaitOutcome::TimedOut | WaitOutcome::Destroyed => return Ok(None),
            }
        }
    }

    /// 阻塞读取一条命令，语义同 [`read_message_timeout`](Self::read_message_timeout)。
    pub fn receive_command_timeout(&self, timeout: Option<Duration>) -> Result<Option<C>> {
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        loop {
            if let Some(command) = self.try_receive_command()? {
                return Ok(Some(command));
            }
            let remaining = match deadline {
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Ok(None);
                    }
                    Some(deadline - now)
                }
                None => None,
            };
            match self.wait_command(remaining)? {
                WaitOutcome::Ready => {}
                WaitOutcome::TimedOut | WaitOutcome::Destroyed => return Ok(None),
            }
        }
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
    ///
    /// 免锁快照：游标单调递增，两个原子读即可得到一致的估计值，
    /// 返回值在离开函数的瞬间就可能过期。等待路径的自旋探测高频调用
    /// 此函数，因此绝不能在这里抢方向锁。
    #[must_use]
    pub fn available_messages(&self) -> usize {
        if self.is_destroyed() {
            return 0;
        }
        let header = self.header();
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

    /// 返回当前待读命令数（免锁快照，语义同
    /// [`available_messages`](Self::available_messages)）。
    #[must_use]
    pub fn available_commands(&self) -> usize {
        if self.is_destroyed() {
            return 0;
        }
        let header = self.header();
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

    /// 返回创建该映射的进程 PID。
    #[must_use]
    pub fn creator_pid(&self) -> u32 {
        self.header().creator_pid
    }

    /// 返回创建者进程当前是否存在。
    ///
    /// 基于 `/proc/<pid>/stat` 探测，未回收的僵尸进程也视为已退出；PID
    /// 复用仍可能造成误报"存活"。用于发现创建者崩溃后的僵尸映射（参见
    /// [`SharedRingBufferOptions::reclaim_stale`]）。
    #[must_use]
    pub fn creator_alive(&self) -> bool {
        process_alive(self.creator_pid())
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

impl<M: WireSafe, C: WireSafe> Drop for TypedRingBuffer<M, C> {
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

    #[repr(C)]
    #[derive(Debug, Clone, Copy, PartialEq)]
    struct Sample {
        sequence: u64,
        value: f64,
    }
    // SAFETY: repr(C)，8+8 字节无 padding，全部位模式有效，无内部可变性。
    unsafe impl WireSafe for Sample {}

    fn mk_path(name: &str) -> String {
        format!("/tmp/typed_ring_test_{}_{}_{}", std::process::id(), name, {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        })
    }

    #[test]
    fn typed_roundtrip_custom_message_and_command() {
        let path = mk_path("roundtrip");
        let ring: TypedRingBuffer<Sample, u64> = SharedRingBufferOptions::new()
            .capacity(8)
            .adaptive_poll_spins(0)
            .create_typed(&path)
            .unwrap();

        assert!(ring
            .try_write_message(&Sample {
                sequence: 7,
                value: 2.5,
            })
            .unwrap());
        assert!(ring.try_send_command(42u64).unwrap());

        let opener: TypedRingBuffer<Sample, u64> =
            TypedRingBuffer::open_auto(&path, Some(0)).unwrap();
        let message = opener.try_read_next_message().unwrap().unwrap();
        assert_eq!(
            message,
            Sample {
                sequence: 7,
                value: 2.5,
            }
        );
        assert_eq!(opener.try_receive_command().unwrap(), Some(42));
    }

    #[test]
    fn typed_peek_drain_and_overwrite() {
        let path = mk_path("peek_drain");
        let ring: TypedRingBuffer<u64, u32> = SharedRingBufferOptions::new()
            .capacity(4)
            .adaptive_poll_spins(0)
            .create_typed(&path)
            .unwrap();

        for value in 0u64..4 {
            assert!(ring.try_write_message(&value).unwrap());
        }
        assert!(!ring.try_write_message(&99).unwrap());
        ring.write_message_overwrite(&100).unwrap();

        assert_eq!(ring.try_peek_message().unwrap(), Some(1));
        assert_eq!(ring.drain_messages(usize::MAX).unwrap(), vec![1, 2, 3, 100]);
    }

    #[test]
    fn typed_open_rejects_mismatched_slot_size() {
        let path = mk_path("slot_mismatch");
        let _ring: TypedRingBuffer<Sample, u64> = SharedRingBufferOptions::new()
            .adaptive_poll_spins(0)
            .create_typed(&path)
            .unwrap();

        // Sample 槽 24 字节，u64 槽 16 字节：布局校验必须拒绝错配打开。
        let mismatch = TypedRingBuffer::<u64, u64>::open_auto(&path, Some(0));
        assert_eq!(mismatch.unwrap_err().kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn typed_open_rejects_same_size_different_type() {
        let path = mk_path("fingerprint_mismatch");
        let _ring: TypedRingBuffer<u64, u64> = SharedRingBufferOptions::new()
            .adaptive_poll_spins(0)
            .create_typed(&path)
            .unwrap();

        // [u32; 2] 与 u64 槽大小相同（16 字节），只有类型指纹能拒绝错配。
        let mismatch = TypedRingBuffer::<[u32; 2], u64>::open_auto(&path, Some(0));
        assert_eq!(mismatch.unwrap_err().kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn typed_create_rejects_overaligned_payload() {
        #[repr(C, align(8192))]
        #[derive(Clone, Copy)]
        struct OverAligned([u8; 8192]);
        // SAFETY（测试用途）: repr(C)、无 padding、任意位模式有效；对齐
        // 超限恰好是本测试要验证被拒绝的属性。
        unsafe impl WireSafe for OverAligned {}

        let path = mk_path("overaligned");
        let result = SharedRingBufferOptions::new()
            .adaptive_poll_spins(0)
            .create_typed::<OverAligned, u64>(&path);
        assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn typed_wait_and_timeout_reads() {
        let path = mk_path("wait");
        let ring: TypedRingBuffer<u64, u32> = SharedRingBufferOptions::new()
            .adaptive_poll_spins(0)
            .create_typed(&path)
            .unwrap();

        assert_eq!(
            ring.wait_message(Some(Duration::from_millis(5))).unwrap(),
            WaitOutcome::TimedOut
        );
        assert!(ring.try_write_message(&5).unwrap());
        assert_eq!(
            ring.read_message_timeout(Some(Duration::from_millis(100)))
                .unwrap(),
            Some(5)
        );
        assert_eq!(
            ring.receive_command_timeout(Some(Duration::from_millis(5)))
                .unwrap(),
            None
        );
    }

    #[test]
    fn proc_stat_state_handles_spaces_and_parentheses_in_comm() {
        assert_eq!(proc_stat_state(b"12 (worker) R 1 2 3"), Some(b'R'));
        assert_eq!(
            proc_stat_state(b"34 (worker pool (idle)) Z 1 2 3"),
            Some(b'Z')
        );
        assert_eq!(proc_stat_state(b"malformed stat"), None);
    }

    #[test]
    fn process_alive_rejects_an_unreaped_zombie() {
        use std::process::{Command, Stdio};

        // A child remains in state Z until this parent calls `wait`. The old
        // `/proc/<pid>` existence check incorrectly reported it as alive.
        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg("--list")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let stat_path = Path::new("/proc").join(pid.to_string()).join("stat");
        let deadline = Instant::now() + Duration::from_secs(5);
        let observed_state = loop {
            let state = std::fs::read(&stat_path)
                .ok()
                .and_then(|stat| proc_stat_state(&stat));
            if matches!(state, Some(b'Z' | b'X' | b'x')) || Instant::now() >= deadline {
                break state;
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        let alive_while_unreaped = process_alive(pid);
        let status = child.wait().unwrap();

        assert!(status.success());
        assert!(
            matches!(observed_state, Some(b'Z' | b'X' | b'x')),
            "child did not enter a dead state before the deadline: {observed_state:?}"
        );
        assert!(!alive_while_unreaped);
    }
}
