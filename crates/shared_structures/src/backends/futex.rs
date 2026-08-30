// src/backends/futex.rs
#![cfg(feature = "futex")]

use super::common::{RegisterOutcome, SyncBackend, WaiterGate};
use libc::timespec;
use std::hint;
use std::io::{Error, ErrorKind, Result};
use std::sync::atomic::{AtomicI32, AtomicU32};
use std::time::{Duration, Instant};

/// 自旋阶段每多少次迭代检查一次超时（`Instant::now` 相对自旋并不便宜）。
const SPIN_TIMEOUT_CHECK_INTERVAL: u32 = 1024;

// ── 核心优化：将 seq 和 waiters 放在不同 cache line 上 ──────────────────────
//
// 修改前（16 字节，同一 cache line）:
//   signal 路径读 waiters → 污染 waiter 频繁写入的同一 cache line
//   waiter 路径写 waiters → 导致 signal 侧 cache miss
//
// 修改后（256 字节，每个字段独占 64 字节 cache line）:
//   - seq（cache line 0）：只由 signal 写，waiter 读
//   - waiters（cache line 1）：只由 waiter 写，signal 读
//   两侧互相不写对方的 cache line → 消除 false sharing
#[repr(C, align(64))]
pub struct FutexChannel {
    pub seq: AtomicU32,
    _pad_seq: [u8; 60], // 64 − 4 = 60，seq 独占第一条 cache line
    pub waiters: AtomicI32,
    _pad_waiters: [u8; 60], // waiters 独占第二条 cache line
}

#[repr(C)]
pub struct FutexHeader {
    pub message: FutexChannel,
    pub command: FutexChannel,
}

pub struct FutexBackend {
    header: *mut FutexHeader,
}

unsafe impl Send for FutexBackend {}
unsafe impl Sync for FutexBackend {}

impl FutexBackend {
    pub fn new() -> Self {
        Self {
            header: std::ptr::null_mut(),
        }
    }

    fn ensure_initialized(&self) -> Result<()> {
        debug_assert!(!self.header.is_null(), "futex backend not initialized");
        if self.header.is_null() {
            return Err(Error::new(
                ErrorKind::NotConnected,
                "futex backend is not initialized",
            ));
        }
        Ok(())
    }

    fn wait_on_futex(
        &self,
        is_message: bool,
        has_data: impl Fn() -> bool,
        adaptive_poll_spins: u32,
        timeout: Option<Duration>,
    ) -> Result<bool> {
        self.ensure_initialized()?;
        let started = Instant::now();
        // 试探式非阻塞调用不该烧掉整个自旋预算；长自旋预算配合短超时
        // 时，周期性检查确保 "adaptive" 自旋不会明显超过 timeout。
        if timeout != Some(Duration::ZERO) {
            for spin in 0..adaptive_poll_spins {
                if has_data() {
                    return Ok(true);
                }
                if spin % SPIN_TIMEOUT_CHECK_INTERVAL == SPIN_TIMEOUT_CHECK_INTERVAL - 1
                    && timeout.is_some_and(|limit| started.elapsed() >= limit)
                {
                    break;
                }
                hint::spin_loop();
            }
        }
        if has_data() {
            return Ok(true);
        }

        let ch = unsafe {
            if is_message {
                &(*self.header).message
            } else {
                &(*self.header).command
            }
        };
        let gate = WaiterGate {
            sequence: &ch.seq,
            waiters: &ch.waiters,
        };

        loop {
            if has_data() {
                return Ok(true);
            }
            if timeout.is_some_and(|limit| started.elapsed() >= limit) {
                return Ok(false);
            }

            match gate.register(&has_data)? {
                RegisterOutcome::DataReady => return Ok(true),
                RegisterOutcome::Retry => continue,
                RegisterOutcome::Registered {
                    registration,
                    snapshot,
                } => {
                    let remaining = timeout.map(|limit| limit.saturating_sub(started.elapsed()));
                    // futex 的 sequence 兼作内核比较字：内核在入睡前原子复核
                    // `seq == snapshot`，关闭注册后到入睡前的窗口。
                    let wait_result = futex_wait(&ch.seq, snapshot, remaining);
                    drop(registration);
                    match wait_result {
                        Ok(_) if has_data() => return Ok(true),
                        Ok(_) => continue,
                        Err(error) => {
                            log::warn!("futex_wait error: {error}. Fallback to check state");
                            return Ok(has_data());
                        }
                    }
                }
            }
        }
    }
}

impl SyncBackend for FutexBackend {
    fn init(&mut self, is_creator: bool, backend_ptr: *mut u8) -> Result<()> {
        self.header = backend_ptr as *mut FutexHeader;
        if is_creator {
            unsafe {
                // 共享映射只是原始存储；用 ptr::write + Atomic*::new
                // 正式构造原子对象，不对未初始化的 Atomic 调用 store。
                self.header.write(FutexHeader {
                    message: FutexChannel {
                        seq: AtomicU32::new(0),
                        _pad_seq: [0; 60],
                        waiters: AtomicI32::new(0),
                        _pad_waiters: [0; 60],
                    },
                    command: FutexChannel {
                        seq: AtomicU32::new(0),
                        _pad_seq: [0; 60],
                        waiters: AtomicI32::new(0),
                        _pad_waiters: [0; 60],
                    },
                });
            }
        }
        Ok(())
    }

    fn wait_for_message(
        &self,
        has_data: impl Fn() -> bool,
        spins: u32,
        timeout: Option<Duration>,
    ) -> Result<bool> {
        self.wait_on_futex(true, has_data, spins, timeout)
    }

    fn wait_for_command(
        &self,
        has_data: impl Fn() -> bool,
        spins: u32,
        timeout: Option<Duration>,
    ) -> Result<bool> {
        self.wait_on_futex(false, has_data, spins, timeout)
    }

    fn signal_message(&self) -> Result<()> {
        self.ensure_initialized()?;
        unsafe {
            let ch = &(*self.header).message;
            let gate = WaiterGate {
                sequence: &ch.seq,
                waiters: &ch.waiters,
            };
            if gate.signal() {
                let _ = futex_wake(&ch.seq, 1);
            }
        }
        Ok(())
    }

    fn signal_command(&self) -> Result<()> {
        self.ensure_initialized()?;
        unsafe {
            let ch = &(*self.header).command;
            let gate = WaiterGate {
                sequence: &ch.seq,
                waiters: &ch.waiters,
            };
            if gate.signal() {
                let _ = futex_wake(&ch.seq, 1);
            }
        }
        Ok(())
    }

    fn wake_all(&self) -> Result<()> {
        self.ensure_initialized()?;
        unsafe {
            let h = &*self.header;
            for ch in [&h.message, &h.command] {
                let gate = WaiterGate {
                    sequence: &ch.seq,
                    waiters: &ch.waiters,
                };
                // 广播必须无条件推进 sequence 并唤醒：正在注册路径上的
                // waiter 依赖 sequence 变化拒绝入睡。
                let _ = gate.broadcast_count();
                let _ = futex_wake(&ch.seq, i32::MAX);
            }
        }
        Ok(())
    }

    fn cleanup(&mut self, _is_creator: bool) {}
}

#[inline]
fn futex_wait(addr: &AtomicU32, expected: u32, timeout: Option<Duration>) -> Result<bool> {
    let mut ts = timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let ts_ptr = if let Some(dur) = timeout {
        ts.tv_sec = dur.as_secs() as libc::time_t;
        ts.tv_nsec = dur.subsec_nanos() as libc::c_long;
        &mut ts as *mut timespec
    } else {
        std::ptr::null_mut()
    };
    let uaddr = addr as *const AtomicU32 as *const i32;
    let ret = unsafe {
        libc::syscall(
            libc::SYS_futex,
            uaddr,
            libc::FUTEX_WAIT,
            expected as i32,
            ts_ptr,
        )
    };
    if ret == 0 {
        Ok(true)
    } else {
        let err = Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EAGAIN) | Some(libc::EINTR) | Some(libc::ETIMEDOUT) => Ok(false),
            _ => Err(err),
        }
    }
}

#[inline]
fn futex_wake(addr: &AtomicU32, n: i32) -> Result<i32> {
    let uaddr = addr as *const AtomicU32 as *const i32;
    let ret = unsafe { libc::syscall(libc::SYS_futex, uaddr, libc::FUTEX_WAKE, n) };
    if ret < 0 {
        Err(Error::last_os_error())
    } else {
        Ok(ret as i32)
    }
}
