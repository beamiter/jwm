// src/backends/futex.rs
#![cfg(feature = "futex")]

use super::common::SyncBackend;
use libc::timespec;
use std::hint;
use std::io::{Error, Result};
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::time::{Duration, Instant};

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

    fn wait_on_futex(
        &self,
        is_message: bool,
        has_data: impl Fn() -> bool,
        adaptive_poll_spins: u32,
        timeout: Option<Duration>,
    ) -> Result<bool> {
        let started = Instant::now();
        for _ in 0..adaptive_poll_spins {
            if has_data() {
                return Ok(true);
            }
            hint::spin_loop();
        }
        if has_data() {
            return Ok(true);
        }

        loop {
            if has_data() {
                return Ok(true);
            }
            if timeout.is_some_and(|limit| started.elapsed() >= limit) {
                return Ok(false);
            }

            // Sequence + waiter count form the same SeqCst registration handshake used by the
            // other backends. A signal before registration changes the sequence; one after the
            // second sequence load must observe the registered waiter and issue FUTEX_WAKE.
            let (ch, snapshot) = unsafe {
                let ch = if is_message {
                    &(*self.header).message
                } else {
                    &(*self.header).command
                };
                (ch, ch.seq.load(Ordering::SeqCst))
            };

            ch.waiters.fetch_add(1, Ordering::SeqCst);
            if has_data() {
                ch.waiters.fetch_sub(1, Ordering::SeqCst);
                return Ok(true);
            }
            if ch.seq.load(Ordering::SeqCst) != snapshot {
                ch.waiters.fetch_sub(1, Ordering::SeqCst);
                continue;
            }

            let remaining = timeout.map(|limit| limit.saturating_sub(started.elapsed()));
            let wait_result = futex_wait(&ch.seq, snapshot, remaining);
            ch.waiters.fetch_sub(1, Ordering::SeqCst);
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
        unsafe {
            let ch = &(*self.header).message;
            ch.seq.fetch_add(1, Ordering::SeqCst);
            if ch.waiters.load(Ordering::SeqCst) > 0 {
                let _ = futex_wake(&ch.seq, 1);
            }
        }
        Ok(())
    }

    fn signal_command(&self) -> Result<()> {
        unsafe {
            let ch = &(*self.header).command;
            ch.seq.fetch_add(1, Ordering::SeqCst);
            if ch.waiters.load(Ordering::SeqCst) > 0 {
                let _ = futex_wake(&ch.seq, 1);
            }
        }
        Ok(())
    }

    fn wake_all(&self) -> Result<()> {
        unsafe {
            let h = &*self.header;
            h.message.seq.fetch_add(1, Ordering::SeqCst);
            let _ = futex_wake(&h.message.seq, i32::MAX);
            h.command.seq.fetch_add(1, Ordering::SeqCst);
            let _ = futex_wake(&h.command.seq, i32::MAX);
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
