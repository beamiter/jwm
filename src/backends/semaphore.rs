// src/backends/semaphore.rs
#![cfg(feature = "semaphore")]

use super::common::SyncBackend;
use libc::{sem_destroy, sem_init, sem_post, sem_t, sem_timedwait, sem_wait};
use std::hint;
use std::io::{Error, ErrorKind, Result};
use std::ptr::addr_of_mut;
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[repr(C)]
pub struct SemaphoreHeader {
    message_sem: sem_t,
    command_sem: sem_t,
    message_waiters: AtomicI32,
    command_waiters: AtomicI32,
    message_sequence: AtomicU32,
    command_sequence: AtomicU32,
}

pub struct SemaphoreBackend {
    header: *mut SemaphoreHeader,
}

unsafe impl Send for SemaphoreBackend {}
unsafe impl Sync for SemaphoreBackend {}

impl SemaphoreBackend {
    pub fn new() -> Self {
        Self {
            header: std::ptr::null_mut(),
        }
    }

    fn wait_on_semaphore(
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

        let deadline = timeout
            .map(|duration| {
                SystemTime::now()
                    .checked_add(duration)
                    .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "Timeout is too large"))
            })
            .transpose()?;

        let (sem_ptr, waiters, sequence) = unsafe {
            if is_message {
                (
                    addr_of_mut!((*self.header).message_sem),
                    &(*self.header).message_waiters,
                    &(*self.header).message_sequence,
                )
            } else {
                (
                    addr_of_mut!((*self.header).command_sem),
                    &(*self.header).command_waiters,
                    &(*self.header).command_sequence,
                )
            }
        };

        loop {
            if has_data() {
                return Ok(true);
            }
            if deadline.is_some_and(|deadline| SystemTime::now() >= deadline) {
                return Ok(false);
            }

            // Sequence + waiter count form a SeqCst registration handshake. A signal racing
            // before registration changes `sequence`; one racing after the second sequence load
            // must observe this waiter and post. Thus the no-waiter fast path cannot lose a wake.
            let snapshot = sequence.load(Ordering::SeqCst);
            waiters.fetch_add(1, Ordering::SeqCst);
            if has_data() {
                waiters.fetch_sub(1, Ordering::SeqCst);
                return Ok(true);
            }
            if sequence.load(Ordering::SeqCst) != snapshot {
                waiters.fetch_sub(1, Ordering::SeqCst);
                continue;
            }

            let wait_result = wait_until(sem_ptr, deadline);
            waiters.fetch_sub(1, Ordering::SeqCst);
            match wait_result {
                Ok(true) if has_data() => return Ok(true),
                // A rare registration race can leave a stale token. Consume it inside this
                // call, but always preserve the original absolute deadline.
                Ok(true) => continue,
                Ok(false) => return Ok(has_data()),
                Err(error) => {
                    log::warn!("semaphore wait error: {}. Fallback to check state.", error);
                    return Ok(has_data());
                }
            }
        }
    }
}

impl SyncBackend for SemaphoreBackend {
    fn init(&mut self, is_creator: bool, backend_ptr: *mut u8) -> Result<()> {
        self.header = backend_ptr as *mut SemaphoreHeader;
        if is_creator {
            unsafe {
                let message_sem = addr_of_mut!((*self.header).message_sem);
                let command_sem = addr_of_mut!((*self.header).command_sem);

                if sem_init(message_sem, 1, 0) != 0 {
                    return Err(Error::last_os_error());
                }
                if sem_init(command_sem, 1, 0) != 0 {
                    let error = Error::last_os_error();
                    sem_destroy(message_sem);
                    return Err(error);
                }
                // Atomics 在共享映射的原始存储中尚未构造。
                // ptr::write + AtomicI32::new 正式开始对象生命期。
                addr_of_mut!((*self.header).message_waiters).write(AtomicI32::new(0));
                addr_of_mut!((*self.header).command_waiters).write(AtomicI32::new(0));
                addr_of_mut!((*self.header).message_sequence).write(AtomicU32::new(0));
                addr_of_mut!((*self.header).command_sequence).write(AtomicU32::new(0));
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
        self.wait_on_semaphore(true, has_data, spins, timeout)
    }

    fn wait_for_command(
        &self,
        has_data: impl Fn() -> bool,
        spins: u32,
        timeout: Option<Duration>,
    ) -> Result<bool> {
        self.wait_on_semaphore(false, has_data, spins, timeout)
    }

    fn signal_message(&self) -> Result<()> {
        unsafe {
            signal_if_waiting(
                addr_of_mut!((*self.header).message_sem),
                &(*self.header).message_waiters,
                &(*self.header).message_sequence,
            )
        }
    }

    fn signal_command(&self) -> Result<()> {
        unsafe {
            signal_if_waiting(
                addr_of_mut!((*self.header).command_sem),
                &(*self.header).command_waiters,
                &(*self.header).command_sequence,
            )
        }
    }

    fn wake_all(&self) -> Result<()> {
        unsafe {
            wake_registered_waiters(
                addr_of_mut!((*self.header).message_sem),
                &(*self.header).message_waiters,
                &(*self.header).message_sequence,
            )?;
            wake_registered_waiters(
                addr_of_mut!((*self.header).command_sem),
                &(*self.header).command_waiters,
                &(*self.header).command_sequence,
            )
        }
    }

    fn abort_init(&mut self) {
        if self.header.is_null() {
            return;
        }
        // The mapping has not been published, so no other process can be using these semaphores.
        unsafe {
            sem_destroy(addr_of_mut!((*self.header).message_sem));
            sem_destroy(addr_of_mut!((*self.header).command_sem));
        }
    }

    fn cleanup(&mut self, _is_creator: bool) {}
}

unsafe fn post_notification(sem: *mut sem_t) -> Result<()> {
    if unsafe { sem_post(sem) } == 0 {
        return Ok(());
    }

    let error = Error::last_os_error();
    if error.raw_os_error() == Some(libc::EOVERFLOW) {
        // 计数已饱和意味着 semaphore 中已有可观测的通知。
        Ok(())
    } else {
        Err(error)
    }
}

unsafe fn signal_if_waiting(
    sem: *mut sem_t,
    waiters: &AtomicI32,
    sequence: &AtomicU32,
) -> Result<()> {
    sequence.fetch_add(1, Ordering::SeqCst);
    if waiters.load(Ordering::SeqCst) > 0 {
        unsafe { post_notification(sem) }
    } else {
        Ok(())
    }
}

unsafe fn wake_registered_waiters(
    sem: *mut sem_t,
    waiters: &AtomicI32,
    sequence: &AtomicU32,
) -> Result<()> {
    sequence.fetch_add(1, Ordering::SeqCst);
    let count = waiters.load(Ordering::SeqCst).max(0) as usize;
    for _ in 0..count {
        unsafe { post_notification(sem) }?;
    }
    Ok(())
}

fn wait_until(sem: *mut sem_t, deadline: Option<SystemTime>) -> Result<bool> {
    unsafe {
        loop {
            match deadline {
                Some(deadline) => {
                    let ts = deadline
                        .duration_since(UNIX_EPOCH)
                        .map(|d| libc::timespec {
                            tv_sec: d.as_secs() as libc::time_t,
                            tv_nsec: d.subsec_nanos() as libc::c_long,
                        })
                        .map_err(|_| Error::new(ErrorKind::InvalidInput, "Invalid time"))?;
                    if sem_timedwait(sem, &ts) == 0 {
                        return Ok(true);
                    }

                    let error = Error::last_os_error();
                    match error.raw_os_error() {
                        Some(libc::EINTR) => continue,
                        Some(libc::ETIMEDOUT) => return Ok(false),
                        _ => return Err(error),
                    }
                }
                None => {
                    if sem_wait(sem) == 0 {
                        return Ok(true);
                    }

                    let error = Error::last_os_error();
                    if error.raw_os_error() != Some(libc::EINTR) {
                        return Err(error);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::MaybeUninit;

    #[test]
    fn signal_without_waiters_does_not_accumulate_tokens() {
        let mut storage = MaybeUninit::<SemaphoreHeader>::uninit();
        let header = storage.as_mut_ptr();
        let mut backend = SemaphoreBackend::new();
        backend.init(true, header.cast()).unwrap();

        backend.signal_message().unwrap();
        backend.signal_message().unwrap();

        let mut value = -1;
        let get_result =
            unsafe { libc::sem_getvalue(addr_of_mut!((*header).message_sem), &mut value) };
        let sequence = unsafe { (*header).message_sequence.load(Ordering::SeqCst) };
        unsafe {
            libc::sem_destroy(addr_of_mut!((*header).message_sem));
            libc::sem_destroy(addr_of_mut!((*header).command_sem));
        }

        assert_eq!(get_result, 0);
        assert_eq!(value, 0);
        assert_eq!(sequence, 2);
    }
}
