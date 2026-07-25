//! src/backends/common.rs

use std::io::Result;
use std::sync::atomic::{AtomicU32, AtomicU64};
use std::time::Duration;

#[cfg(any(feature = "futex", feature = "semaphore", feature = "eventfd"))]
pub(crate) use waiter_gate::{RegisterOutcome, WaiterGate};

/// sequence/waiter 注册握手：三个后端共享的丢唤醒防护原语。
///
/// 协议（全部 SeqCst）：
/// - signal 侧总是先推进 `sequence`，再读 `waiters`，只在确有已登记
///   等待者时进入内核唤醒；
/// - waiter 侧先快照 `sequence`，递增 `waiters` 完成登记，随后复核
///   数据与 `sequence`——若 signal 发生在登记前，复核会看到 sequence
///   变化而拒绝入睡；若发生在登记后，signal 必然观察到该 waiter。
///
/// 两侧至少一方能观察到对方，因此"无等待者即跳过内核唤醒"的快路径
/// 不会重新打开丢唤醒窗口。这段最微妙的并发逻辑集中在此处，修复只需
/// 改一份代码。
#[cfg(any(feature = "futex", feature = "semaphore", feature = "eventfd"))]
mod waiter_gate {
    use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};

    pub(crate) struct WaiterGate<'a> {
        pub sequence: &'a AtomicU32,
        pub waiters: &'a AtomicI32,
    }

    /// 已登记等待者的 RAII 凭据；Drop 时注销登记。
    pub(crate) struct WaiterRegistration<'a> {
        waiters: &'a AtomicI32,
    }

    impl Drop for WaiterRegistration<'_> {
        fn drop(&mut self) {
            self.waiters.fetch_sub(1, Ordering::SeqCst);
        }
    }

    pub(crate) enum RegisterOutcome<'a> {
        /// 登记过程中观察到数据已到，未保持登记。
        DataReady,
        /// sequence 在登记期间变化（可能有并发 signal），应重试外层循环。
        Retry,
        /// 登记成功；`snapshot` 是登记前的 sequence 值（futex 用作比较字）。
        Registered {
            registration: WaiterRegistration<'a>,
            snapshot: u32,
        },
    }

    impl<'a> WaiterGate<'a> {
        /// waiter 侧：执行注册握手。
        pub(crate) fn register(&self, has_data: impl Fn() -> bool) -> RegisterOutcome<'a> {
            let snapshot = self.sequence.load(Ordering::SeqCst);
            self.waiters.fetch_add(1, Ordering::SeqCst);
            let registration = WaiterRegistration {
                waiters: self.waiters,
            };
            if has_data() {
                drop(registration);
                return RegisterOutcome::DataReady;
            }
            if self.sequence.load(Ordering::SeqCst) != snapshot {
                drop(registration);
                return RegisterOutcome::Retry;
            }
            RegisterOutcome::Registered {
                registration,
                snapshot,
            }
        }

        /// signal 侧：推进 sequence，返回是否需要进入内核唤醒一个等待者。
        pub(crate) fn signal(&self) -> bool {
            self.sequence.fetch_add(1, Ordering::SeqCst);
            self.waiters.load(Ordering::SeqCst) > 0
        }

        /// 关停广播：推进 sequence，返回当前已登记的等待者数量。
        pub(crate) fn broadcast_count(&self) -> usize {
            self.sequence.fetch_add(1, Ordering::SeqCst);
            self.waiters.load(Ordering::SeqCst).max(0) as usize
        }
    }
}

/// 运行时同步后端。
///
/// 只有启用对应 Cargo feature 时，该变体才可用。数值是共享内存
/// 协议的一部分，不随枚举顺序变化。
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SyncStrategy {
    #[cfg(feature = "futex")]
    Futex = 1,
    #[cfg(feature = "semaphore")]
    Semaphore = 2,
    #[cfg(feature = "eventfd")]
    EventFd = 3,
    /// 未启用任何后端时的占位策略。
    #[doc(hidden)]
    Unsupported = 0,
}

impl SyncStrategy {
    pub(crate) const fn id(self) -> u32 {
        self as u32
    }

    pub(crate) fn from_id(id: u32) -> Option<Self> {
        match id {
            #[cfg(feature = "futex")]
            1 => Some(Self::Futex),
            #[cfg(feature = "semaphore")]
            2 => Some(Self::Semaphore),
            #[cfg(feature = "eventfd")]
            3 => Some(Self::EventFd),
            // 0 never appears in an initialized header: `create` rejects `Unsupported`
            // before writing any metadata, so an all-zero (torn/uninitialized) header
            // must be rejected instead of silently degrading every wait to a poll.
            _ => None,
        }
    }

    /// 返回当前构建是否包含该后端。
    #[must_use]
    pub const fn is_supported(self) -> bool {
        !matches!(self, Self::Unsupported)
    }

    /// 返回稳定的后端名称。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            #[cfg(feature = "futex")]
            Self::Futex => "futex",
            #[cfg(feature = "semaphore")]
            Self::Semaphore => "semaphore",
            #[cfg(feature = "eventfd")]
            Self::EventFd => "eventfd",
            Self::Unsupported => "unsupported",
        }
    }

    /// 返回此策略在共享内存中需要的后端专属 Header 大小。
    #[doc(hidden)]
    pub fn backend_size(&self) -> usize {
        match self {
            #[cfg(feature = "futex")]
            Self::Futex => std::mem::size_of::<super::futex::FutexHeader>(),
            #[cfg(feature = "semaphore")]
            Self::Semaphore => std::mem::size_of::<super::semaphore::SemaphoreHeader>(),
            #[cfg(feature = "eventfd")]
            Self::EventFd => std::mem::size_of::<super::eventfd::EventFdHeader>(),
            Self::Unsupported => 0,
        }
    }

    /// 返回此策略的内存对齐要求。
    #[doc(hidden)]
    pub fn backend_align(&self) -> usize {
        match self {
            #[cfg(feature = "futex")]
            Self::Futex => std::mem::align_of::<super::futex::FutexHeader>(),
            #[cfg(feature = "semaphore")]
            Self::Semaphore => std::mem::align_of::<super::semaphore::SemaphoreHeader>(),
            #[cfg(feature = "eventfd")]
            Self::EventFd => std::mem::align_of::<super::eventfd::EventFdHeader>(),
            Self::Unsupported => 1,
        }
    }
}

#[allow(clippy::derivable_impls)] // Default depends on the enabled/legacy selector features.
impl Default for SyncStrategy {
    fn default() -> Self {
        // 多个 legacy `use-*` 同时存在时使用确定性优先级。
        #[cfg(feature = "use-futex")]
        {
            Self::Futex
        }
        #[cfg(all(not(feature = "use-futex"), feature = "use-semaphore"))]
        {
            Self::Semaphore
        }
        #[cfg(all(
            not(feature = "use-futex"),
            not(feature = "use-semaphore"),
            feature = "use-eventfd"
        ))]
        {
            Self::EventFd
        }
        #[cfg(all(
            not(any(
                feature = "use-futex",
                feature = "use-semaphore",
                feature = "use-eventfd"
            )),
            feature = "futex"
        ))]
        {
            Self::Futex
        }
        #[cfg(all(
            not(any(
                feature = "use-futex",
                feature = "use-semaphore",
                feature = "use-eventfd",
                feature = "futex"
            )),
            feature = "semaphore"
        ))]
        {
            Self::Semaphore
        }
        #[cfg(all(
            not(any(
                feature = "use-futex",
                feature = "use-semaphore",
                feature = "use-eventfd",
                feature = "futex",
                feature = "semaphore"
            )),
            feature = "eventfd"
        ))]
        {
            Self::EventFd
        }
        #[cfg(not(any(feature = "futex", feature = "semaphore", feature = "eventfd")))]
        {
            Self::Unsupported
        }
    }
}

impl std::fmt::Display for SyncStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[repr(C, align(64))]
#[derive(Debug)]
pub(crate) struct QueueCursor {
    pub index: AtomicU32,
    pub lock: AtomicU32,
    _padding: [u8; 56],
}

impl QueueCursor {
    const fn new() -> Self {
        Self {
            index: AtomicU32::new(0),
            lock: AtomicU32::new(0),
            _padding: [0; 56],
        }
    }
}

/// 环形缓冲区的自描述共享头。
///
/// 元数据、`last_timestamp` 和四个队列游标分别占据独立 cache line：
/// 元数据行在初始化后只读（含每次操作都要检查的 `is_destroyed`），
/// 生产者高频写入的 `last_timestamp` 单独隔离，避免弄脏其他行。
#[repr(C, align(64))]
#[derive(Debug)]
pub struct GenericHeader {
    pub magic: AtomicU64,
    pub version: AtomicU64,
    pub total_size: u64,
    pub buffer_size: u32,
    pub command_buffer_size: u32,
    pub backend_id: u32,
    pub message_slot_size: u32,
    pub command_slot_size: u32,
    pub layout_marker: u32,
    pub is_destroyed: AtomicU32,
    pub creator_pid: u32,
    _metadata_padding: [u8; 8],
    pub last_timestamp: AtomicU64,
    _timestamp_padding: [u8; 56],
    pub(crate) message_write: QueueCursor,
    pub(crate) message_read: QueueCursor,
    pub(crate) command_write: QueueCursor,
    pub(crate) command_read: QueueCursor,
}

impl GenericHeader {
    #[allow(clippy::too_many_arguments)]
    pub(crate) const fn new(
        version: u64,
        total_size: u64,
        buffer_size: u32,
        command_buffer_size: u32,
        backend_id: u32,
        message_slot_size: u32,
        command_slot_size: u32,
        layout_marker: u32,
        creator_pid: u32,
    ) -> Self {
        Self {
            magic: AtomicU64::new(0),
            version: AtomicU64::new(version),
            total_size,
            buffer_size,
            command_buffer_size,
            backend_id,
            message_slot_size,
            command_slot_size,
            layout_marker,
            is_destroyed: AtomicU32::new(0),
            creator_pid,
            _metadata_padding: [0; 8],
            last_timestamp: AtomicU64::new(0),
            _timestamp_padding: [0; 56],
            message_write: QueueCursor::new(),
            message_read: QueueCursor::new(),
            command_write: QueueCursor::new(),
            command_read: QueueCursor::new(),
        }
    }
}

/// 同步后端必须实现的 Trait
pub trait SyncBackend: Send + Sync {
    /// 初始化后端。
    /// is_creator: true 表示创建并初始化共享内存，false 表示打开已有的。
    /// backend_ptr: 指向为后端分配的专属内存区域的指针。
    fn init(&mut self, is_creator: bool, backend_ptr: *mut u8) -> Result<()>;

    /// 等待消息。
    ///
    /// 返回值契约：`Ok(true)` 表示 `has_data` 已观察为真；`Ok(false)` 仅表示
    /// 本次等待没有观察到数据（超时、可恢复的底层错误降级等），**不保证**
    /// 已经等满 `timeout`——调用方必须重查条件后自行决定是否继续等待。
    fn wait_for_message(
        &self,
        has_data: impl Fn() -> bool,
        adaptive_poll_spins: u32,
        timeout: Option<Duration>,
    ) -> Result<bool>;

    /// 等待命令，契约同 [`wait_for_message`](Self::wait_for_message)。
    fn wait_for_command(
        &self,
        has_data: impl Fn() -> bool,
        adaptive_poll_spins: u32,
        timeout: Option<Duration>,
    ) -> Result<bool>;

    /// 唤醒正在等待消息的进程
    fn signal_message(&self) -> Result<()>;

    /// 唤醒正在等待命令的进程
    fn signal_command(&self) -> Result<()>;

    /// 广播唤醒两个通道上的所有等待者。
    ///
    /// 边沿语义：只保证唤醒调用时刻已注册的等待者，恰在注册路径上的
    /// 等待者可能带新快照重新入睡。因此用于关停时，调用方必须保证
    /// `wake_all` 之后 `has_data` 恒为真（例如把销毁标志纳入条件闭包），
    /// 本 crate 的 `destroy` 正是这样做的。
    fn wake_all(&self) -> Result<()> {
        self.signal_message()?;
        self.signal_command()
    }

    /// 回滚尚未对外发布的创建者初始化。
    fn abort_init(&mut self) {
        self.cleanup(true);
    }

    /// 释放当前进程拥有的后端资源。
    fn cleanup(&mut self, is_creator: bool);
}

#[cfg(feature = "eventfd")]
use crate::backends::eventfd::EventFdBackend;
#[cfg(feature = "futex")]
use crate::backends::futex::FutexBackend;
#[cfg(feature = "semaphore")]
use crate::backends::semaphore::SemaphoreBackend;

/// 对当前构建包含的同步后端做静态分发。
pub enum AnySyncBackend {
    #[cfg(feature = "futex")]
    Futex(FutexBackend),
    #[cfg(feature = "semaphore")]
    Semaphore(SemaphoreBackend),
    #[cfg(feature = "eventfd")]
    EventFd(EventFdBackend),
    /// 没有启用后端 feature 时的占位分支。
    #[doc(hidden)]
    _Unsupported,
}

impl SyncBackend for AnySyncBackend {
    fn init(&mut self, _is_creator: bool, _backend_ptr: *mut u8) -> Result<()> {
        match self {
            #[cfg(feature = "futex")]
            Self::Futex(b) => b.init(_is_creator, _backend_ptr),
            #[cfg(feature = "semaphore")]
            Self::Semaphore(b) => b.init(_is_creator, _backend_ptr),
            #[cfg(feature = "eventfd")]
            Self::EventFd(b) => b.init(_is_creator, _backend_ptr),
            Self::_Unsupported => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "no synchronization backend feature is enabled",
            )),
        }
    }

    fn wait_for_message(
        &self,
        has_data: impl Fn() -> bool,
        _adaptive_poll_spins: u32,
        _timeout: Option<Duration>,
    ) -> Result<bool> {
        match self {
            #[cfg(feature = "futex")]
            Self::Futex(b) => b.wait_for_message(has_data, _adaptive_poll_spins, _timeout),
            #[cfg(feature = "semaphore")]
            Self::Semaphore(b) => b.wait_for_message(has_data, _adaptive_poll_spins, _timeout),
            #[cfg(feature = "eventfd")]
            Self::EventFd(b) => b.wait_for_message(has_data, _adaptive_poll_spins, _timeout),
            Self::_Unsupported => Ok(has_data()),
        }
    }

    fn wait_for_command(
        &self,
        has_data: impl Fn() -> bool,
        _adaptive_poll_spins: u32,
        _timeout: Option<Duration>,
    ) -> Result<bool> {
        match self {
            #[cfg(feature = "futex")]
            Self::Futex(b) => b.wait_for_command(has_data, _adaptive_poll_spins, _timeout),
            #[cfg(feature = "semaphore")]
            Self::Semaphore(b) => b.wait_for_command(has_data, _adaptive_poll_spins, _timeout),
            #[cfg(feature = "eventfd")]
            Self::EventFd(b) => b.wait_for_command(has_data, _adaptive_poll_spins, _timeout),
            Self::_Unsupported => Ok(has_data()),
        }
    }

    fn signal_message(&self) -> Result<()> {
        match self {
            #[cfg(feature = "futex")]
            Self::Futex(b) => b.signal_message(),
            #[cfg(feature = "semaphore")]
            Self::Semaphore(b) => b.signal_message(),
            #[cfg(feature = "eventfd")]
            Self::EventFd(b) => b.signal_message(),
            Self::_Unsupported => Ok(()),
        }
    }

    fn signal_command(&self) -> Result<()> {
        match self {
            #[cfg(feature = "futex")]
            Self::Futex(b) => b.signal_command(),
            #[cfg(feature = "semaphore")]
            Self::Semaphore(b) => b.signal_command(),
            #[cfg(feature = "eventfd")]
            Self::EventFd(b) => b.signal_command(),
            Self::_Unsupported => Ok(()),
        }
    }

    fn wake_all(&self) -> Result<()> {
        match self {
            #[cfg(feature = "futex")]
            Self::Futex(b) => b.wake_all(),
            #[cfg(feature = "semaphore")]
            Self::Semaphore(b) => b.wake_all(),
            #[cfg(feature = "eventfd")]
            Self::EventFd(b) => b.wake_all(),
            Self::_Unsupported => Ok(()),
        }
    }

    fn abort_init(&mut self) {
        match self {
            #[cfg(feature = "futex")]
            Self::Futex(b) => b.abort_init(),
            #[cfg(feature = "semaphore")]
            Self::Semaphore(b) => b.abort_init(),
            #[cfg(feature = "eventfd")]
            Self::EventFd(b) => b.abort_init(),
            Self::_Unsupported => {}
        }
    }

    fn cleanup(&mut self, _is_creator: bool) {
        match self {
            #[cfg(feature = "futex")]
            Self::Futex(b) => b.cleanup(_is_creator),
            #[cfg(feature = "semaphore")]
            Self::Semaphore(b) => b.cleanup(_is_creator),
            #[cfg(feature = "eventfd")]
            Self::EventFd(b) => b.cleanup(_is_creator),
            Self::_Unsupported => {}
        }
    }
}

#[cfg(all(
    test,
    any(feature = "futex", feature = "semaphore", feature = "eventfd")
))]
mod tests {
    use super::*;

    #[cfg(any(
        feature = "use-futex",
        feature = "use-semaphore",
        feature = "use-eventfd"
    ))]
    #[test]
    fn legacy_selector_has_deterministic_priority() {
        #[cfg(feature = "use-futex")]
        assert_eq!(SyncStrategy::default(), SyncStrategy::Futex);

        #[cfg(all(not(feature = "use-futex"), feature = "use-semaphore"))]
        assert_eq!(SyncStrategy::default(), SyncStrategy::Semaphore);

        #[cfg(all(
            not(feature = "use-futex"),
            not(feature = "use-semaphore"),
            feature = "use-eventfd"
        ))]
        assert_eq!(SyncStrategy::default(), SyncStrategy::EventFd);
    }

    // ── SyncStrategy::backend_size ────────────────────────────────────────────

    #[test]
    #[cfg(feature = "futex")]
    fn test_futex_backend_size_positive() {
        assert!(SyncStrategy::Futex.backend_size() > 0);
    }

    #[test]
    #[cfg(feature = "semaphore")]
    fn test_semaphore_backend_size_positive() {
        assert!(SyncStrategy::Semaphore.backend_size() > 0);
    }

    #[test]
    #[cfg(feature = "eventfd")]
    fn test_eventfd_backend_size_positive() {
        assert!(SyncStrategy::EventFd.backend_size() > 0);
    }

    // ── SyncStrategy::backend_align ───────────────────────────────────────────

    #[test]
    #[cfg(feature = "futex")]
    fn test_futex_backend_align_is_power_of_two() {
        let align = SyncStrategy::Futex.backend_align();
        assert!(align >= 1);
        assert!(align.is_power_of_two());
    }

    #[test]
    #[cfg(feature = "semaphore")]
    fn test_semaphore_backend_align_is_power_of_two() {
        let align = SyncStrategy::Semaphore.backend_align();
        assert!(align >= 1);
        assert!(align.is_power_of_two());
    }

    #[test]
    #[cfg(feature = "eventfd")]
    fn test_eventfd_backend_align_is_power_of_two() {
        let align = SyncStrategy::EventFd.backend_align();
        assert!(align >= 1);
        assert!(align.is_power_of_two());
    }

    // ── backend_size >= backend_align（合理布局约束）──────────────────────────

    #[test]
    #[cfg(feature = "futex")]
    fn test_futex_size_ge_align() {
        assert!(SyncStrategy::Futex.backend_size() >= SyncStrategy::Futex.backend_align());
    }

    #[test]
    #[cfg(feature = "semaphore")]
    fn test_semaphore_size_ge_align() {
        assert!(SyncStrategy::Semaphore.backend_size() >= SyncStrategy::Semaphore.backend_align());
    }

    // ── 各策略 backend_size 互不相同（验证每个 match 分支各自生效）────────────

    #[test]
    #[cfg(all(feature = "futex", feature = "semaphore"))]
    fn test_futex_and_semaphore_sizes_differ() {
        // Futex header 与 Semaphore header 结构不同，大小应当不同
        assert_ne!(
            SyncStrategy::Futex.backend_size(),
            SyncStrategy::Semaphore.backend_size()
        );
    }
}
