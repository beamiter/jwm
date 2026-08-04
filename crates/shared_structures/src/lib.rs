//! 高性能 Linux 进程间共享内存数据结构。
//!
//! `shared_structures` 提供固定大小消息与命令的有界环形缓冲区，
//! 支持 Futex、POSIX Semaphore 和 EventFd 通知后端。共享内存协议头
//! 会记录版本、后端和完整布局，打开端在做任何 slot 指针运算前
//! 会校验整个映射。
//!
//! # Example
//!
//! ```no_run
//! use shared_structures::{SharedMessage, SharedRingBufferOptions};
//!
//! # fn main() -> std::io::Result<()> {
//! let buffer = SharedRingBufferOptions::new()
//!     .capacity(16)
//!     .create("/tmp/shared-structures-example")?;
//! let message = SharedMessage::new();
//! assert!(buffer.try_write_message(&message)?);
//! let received = buffer.try_read_next_message()?;
//! assert!(received.is_some());
//! # Ok(())
//! # }
//! ```

mod shared_message;
pub use shared_message::{
    CommandType, MonitorInfo, SharedCommand, SharedMessage, ShellHubRoute, TagStatus,
    MAX_CLIENT_NAME_LEN, MAX_LT_SYMBOL_LEN, MAX_TAGS,
};

mod typed_ring_buffer;
pub use typed_ring_buffer::{
    SharedRingBufferOptions, SharedRingBufferStats, TypedRingBuffer, WaitOutcome, WireSafe,
};

mod shared_ring_buffer;
pub use shared_ring_buffer::SharedRingBuffer;

mod backends;
pub use backends::common::SyncStrategy;
