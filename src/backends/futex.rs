// src/backends/futex.rs
#![cfg(feature = "futex")]

use super::common::SyncBackend;
use libc::timespec;
use std::hint;
use std::io::{Error, Result};
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::time::Duration;

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
        for _ in 0..adaptive_poll_spins {
            if has_data() {
                return Ok(true);
            }
            hint::spin_loop();
        }
        if has_data() {
            return Ok(true);
        }

        unsafe {
            let ch = if is_message {
                &(*self.header).message
            } else {
                &(*self.header).command
            };

            // 发布等待意图（Release：让 signal 侧看到最新 waiters 值）
            ch.waiters.fetch_add(1, Ordering::Release);

            // 进内核前再检查一次，避免 signal 先于 waiters++ 发生而被漏掉
            if has_data() {
                ch.waiters.fetch_sub(1, Ordering::Relaxed);
                return Ok(true);
            }

            let snapshot = ch.seq.load(Ordering::Acquire);
            let res = futex_wait(&ch.seq, snapshot as u32, timeout);

            // Release：确保 signal 侧下次读到 waiters 已减少
            ch.waiters.fetch_sub(1, Ordering::Release);

            res.map(|_| has_data()).or_else(|e| {
                log::warn!("futex_wait error: {}. Fallback to check state", e);
                Ok(has_data())
            })
        }
    }
}

impl SyncBackend for FutexBackend {
    fn init(&mut self, is_creator: bool, backend_ptr: *mut u8) -> Result<()> {
        self.header = backend_ptr as *mut FutexHeader;
        if is_creator {
            unsafe {
                // 逐字段初始化，避免 write() 踩 padding 字节（UB）
                let h = &mut *self.header;
                h.message.seq.store(0, Ordering::Relaxed);
                h.message.waiters.store(0, Ordering::Relaxed);
                h.command.seq.store(0, Ordering::Relaxed);
                h.command.waiters.store(0, Ordering::Relaxed);
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
            // Acquire：读到最新的 waiters（与 waiter 的 Release fetch_add 配对）
            if ch.waiters.load(Ordering::Acquire) > 0 {
                ch.seq.fetch_add(1, Ordering::Release);
                let _ = futex_wake(&ch.seq, 1);
            }
        }
        Ok(())
    }

    fn signal_command(&self) -> Result<()> {
        unsafe {
            let ch = &(*self.header).command;
            if ch.waiters.load(Ordering::Acquire) > 0 {
                ch.seq.fetch_add(1, Ordering::Release);
                let _ = futex_wake(&ch.seq, 1);
            }
        }
        Ok(())
    }

    fn cleanup(&mut self, _is_creator: bool) {
        // 修复：只在真正有等待者时才发出 FUTEX_WAKE syscall。
        // 改动前：无论有没有等待者都调用 futex_wake（两次不必要的 syscall）。
        unsafe {
            let h = &*self.header;
            if h.message.waiters.load(Ordering::Acquire) > 0 {
                h.message.seq.fetch_add(1, Ordering::Release);
                let _ = futex_wake(&h.message.seq, i32::MAX);
            }
            if h.command.waiters.load(Ordering::Acquire) > 0 {
                h.command.seq.fetch_add(1, Ordering::Release);
                let _ = futex_wake(&h.command.seq, i32::MAX);
            }
        }
    }
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
