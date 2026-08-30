// src/backends/semaphore.rs
#![cfg(feature = "semaphore")]

use super::common::{RegisterOutcome, SyncBackend, WaiterGate};
use libc::{sem_destroy, sem_init, sem_post, sem_t, sem_timedwait, sem_wait};
use std::hint;
use std::io::{Error, ErrorKind, Result};
use std::ptr::addr_of_mut;
use std::sync::atomic::{AtomicI32, AtomicU32};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// 自旋阶段每多少次迭代检查一次超时。
const SPIN_TIMEOUT_CHECK_INTERVAL: u32 = 1024;

/// 每轮 `sem_timedwait` 的最长绝对期限。
///
/// 超时的权威判定基于单调钟（`Instant`）；`sem_timedwait` 只能接受
/// CLOCK_REALTIME 的绝对期限，把它切成短片段后，墙钟跳变（NTP 回拨等）
/// 最多影响一个片段，不会把有界等待放大成近乎无限等待。
const REALTIME_SLICE: Duration = Duration::from_millis(100);

/// 单个通道的等待原语，独占 cache line 以避免消息/命令通道间的 false sharing。
#[repr(C, align(64))]
struct SemaphoreChannel {
    sem: sem_t,
    waiters: AtomicI32,
    sequence: AtomicU32,
}

#[repr(C)]
pub struct SemaphoreHeader {
    message: SemaphoreChannel,
    command: SemaphoreChannel,
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

    fn ensure_initialized(&self) -> Result<()> {
        debug_assert!(!self.header.is_null(), "semaphore backend not initialized");
        if self.header.is_null() {
            return Err(Error::new(
                ErrorKind::NotConnected,
                "semaphore backend is not initialized",
            ));
        }
        Ok(())
    }

    fn wait_on_semaphore(
        &self,
        is_message: bool,
        has_data: impl Fn() -> bool,
        adaptive_poll_spins: u32,
        timeout: Option<Duration>,
    ) -> Result<bool> {
        self.ensure_initialized()?;
        // 单调钟做权威超时判定，不受墙钟跳变影响。
        let deadline = timeout.map(|duration| {
            Instant::now()
                .checked_add(duration)
                .unwrap_or_else(|| Instant::now() + Duration::from_secs(u32::MAX as u64))
        });
        if timeout != Some(Duration::ZERO) {
            for spin in 0..adaptive_poll_spins {
                if has_data() {
                    return Ok(true);
                }
                if spin % SPIN_TIMEOUT_CHECK_INTERVAL == SPIN_TIMEOUT_CHECK_INTERVAL - 1
                    && deadline.is_some_and(|deadline| Instant::now() >= deadline)
                {
                    break;
                }
                hint::spin_loop();
            }
        }
        if has_data() {
            return Ok(true);
        }

        let channel = unsafe {
            if is_message {
                addr_of_mut!((*self.header).message)
            } else {
                addr_of_mut!((*self.header).command)
            }
        };
        let (sem_ptr, gate) = unsafe {
            (
                addr_of_mut!((*channel).sem),
                WaiterGate {
                    sequence: &(*channel).sequence,
                    waiters: &(*channel).waiters,
                },
            )
        };

        loop {
            if has_data() {
                return Ok(true);
            }
            let remaining = match deadline {
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Ok(false);
                    }
                    Some(deadline - now)
                }
                None => None,
            };

            match gate.register(&has_data) {
                RegisterOutcome::DataReady => return Ok(true),
                RegisterOutcome::Retry => continue,
                RegisterOutcome::Registered { registration, .. } => {
                    let wait_result = wait_one_slice(sem_ptr, remaining);
                    drop(registration);
                    match wait_result {
                        Ok(true) if has_data() => return Ok(true),
                        // A rare registration race can leave a stale token. Consume it inside
                        // this call; the monotonic deadline above still bounds the total wait.
                        Ok(true) => continue,
                        // 单个片段超时不是整体超时；回到循环顶部用单调钟重新判定。
                        Ok(false) => continue,
                        Err(error) => {
                            log::warn!("semaphore wait error: {}. Fallback to check state.", error);
                            return Ok(has_data());
                        }
                    }
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
                let message_sem = addr_of_mut!((*self.header).message.sem);
                let command_sem = addr_of_mut!((*self.header).command.sem);

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
                addr_of_mut!((*self.header).message.waiters).write(AtomicI32::new(0));
                addr_of_mut!((*self.header).command.waiters).write(AtomicI32::new(0));
                addr_of_mut!((*self.header).message.sequence).write(AtomicU32::new(0));
                addr_of_mut!((*self.header).command.sequence).write(AtomicU32::new(0));
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
        self.ensure_initialized()?;
        unsafe {
            signal_if_waiting(
                addr_of_mut!((*self.header).message.sem),
                &(*self.header).message.waiters,
                &(*self.header).message.sequence,
            )
        }
    }

    fn signal_command(&self) -> Result<()> {
        self.ensure_initialized()?;
        unsafe {
            signal_if_waiting(
                addr_of_mut!((*self.header).command.sem),
                &(*self.header).command.waiters,
                &(*self.header).command.sequence,
            )
        }
    }

    fn wake_all(&self) -> Result<()> {
        self.ensure_initialized()?;
        unsafe {
            wake_registered_waiters(
                addr_of_mut!((*self.header).message.sem),
                &(*self.header).message.waiters,
                &(*self.header).message.sequence,
            )?;
            wake_registered_waiters(
                addr_of_mut!((*self.header).command.sem),
                &(*self.header).command.waiters,
                &(*self.header).command.sequence,
            )
        }
    }

    fn abort_init(&mut self) {
        if self.header.is_null() {
            return;
        }
        // The mapping has not been published, so no other process can be using these semaphores.
        unsafe {
            sem_destroy(addr_of_mut!((*self.header).message.sem));
            sem_destroy(addr_of_mut!((*self.header).command.sem));
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
    let gate = WaiterGate { sequence, waiters };
    if gate.signal() {
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
    let gate = WaiterGate { sequence, waiters };
    for _ in 0..gate.broadcast_count() {
        unsafe { post_notification(sem) }?;
    }
    Ok(())
}

/// 在信号量上等待至多一个 `REALTIME_SLICE` 片段（或 `remaining`，取更小者）。
///
/// 返回 `Ok(true)` 表示消费到一个 token，`Ok(false)` 表示本片段超时。
/// 整体超时由调用方基于单调钟判定。
fn wait_one_slice(sem: *mut sem_t, remaining: Option<Duration>) -> Result<bool> {
    match remaining {
        Some(remaining) => {
            let slice = remaining.min(REALTIME_SLICE);
            let deadline = SystemTime::now()
                .checked_add(slice)
                .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "Invalid time"))?;
            let ts = deadline
                .duration_since(UNIX_EPOCH)
                .map(|d| libc::timespec {
                    tv_sec: d.as_secs() as libc::time_t,
                    tv_nsec: d.subsec_nanos() as libc::c_long,
                })
                .map_err(|_| Error::new(ErrorKind::InvalidInput, "Invalid time"))?;

            // Retry EINTR against the same absolute deadline. Recomputing
            // `now + slice` here would let a signal storm extend a finite wait
            // indefinitely and prevent the outer monotonic deadline check.
            unsafe {
                loop {
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
            }
        }
        None => unsafe {
            loop {
                if sem_wait(sem) == 0 {
                    return Ok(true);
                }

                let error = Error::last_os_error();
                if error.raw_os_error() != Some(libc::EINTR) {
                    return Err(error);
                }
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::MaybeUninit;
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;

    extern "C" fn noop_signal_handler(_: libc::c_int) {}

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
            unsafe { libc::sem_getvalue(addr_of_mut!((*header).message.sem), &mut value) };
        let sequence = unsafe { (*header).message.sequence.load(Ordering::SeqCst) };
        unsafe {
            libc::sem_destroy(addr_of_mut!((*header).message.sem));
            libc::sem_destroy(addr_of_mut!((*header).command.sem));
        }

        assert_eq!(get_result, 0);
        assert_eq!(value, 0);
        assert_eq!(sequence, 2);
    }

    #[test]
    fn channels_occupy_distinct_cache_lines() {
        assert_eq!(std::mem::align_of::<SemaphoreChannel>(), 64);
        assert!(std::mem::offset_of!(SemaphoreHeader, command) >= 64);
    }

    #[test]
    fn finite_wait_deadline_survives_repeated_eintr() {
        const CHILD_MARKER_ENV: &str = "SHARED_STRUCTURES_SEMAPHORE_EINTR_MARKER";

        if let Some(marker) = std::env::var_os(CHILD_MARKER_ENV) {
            // Signal disposition is process-global, so run the interruption
            // storm only in this exact child test.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = noop_signal_handler as *const () as usize;
                action.sa_flags = 0;
                libc::sigemptyset(&mut action.sa_mask);
                assert_eq!(
                    libc::sigaction(libc::SIGUSR1, &action, std::ptr::null_mut()),
                    0
                );
            }

            let mut storage = Box::new(MaybeUninit::<sem_t>::uninit());
            let semaphore = storage.as_mut_ptr();
            assert_eq!(unsafe { sem_init(semaphore, 0, 0) }, 0);

            let semaphore_address = semaphore as usize;
            let (thread_tx, thread_rx) = mpsc::sync_channel(0);
            let (result_tx, result_rx) = mpsc::sync_channel(0);
            let waiter = std::thread::spawn(move || {
                thread_tx.send(unsafe { libc::pthread_self() }).unwrap();
                let started = Instant::now();
                let result = wait_one_slice(
                    semaphore_address as *mut sem_t,
                    Some(Duration::from_millis(50)),
                );
                result_tx.send((result, started.elapsed())).unwrap();
            });
            let waiter_thread = thread_rx.recv().unwrap();

            let signal_deadline = Instant::now() + Duration::from_millis(400);
            let mut outcome = None;
            while Instant::now() < signal_deadline {
                if let Ok(result) = result_rx.try_recv() {
                    outcome = Some(result);
                    break;
                }
                let _ = unsafe { libc::pthread_kill(waiter_thread, libc::SIGUSR1) };
                std::thread::sleep(Duration::from_millis(2));
            }
            let outcome = outcome.unwrap_or_else(|| {
                result_rx
                    .recv_timeout(Duration::from_millis(200))
                    .expect("semaphore wait stayed blocked after the signal storm")
            });
            waiter.join().unwrap();
            assert_eq!(unsafe { sem_destroy(semaphore) }, 0);

            assert!(
                !outcome.0.unwrap(),
                "empty semaphore unexpectedly became ready"
            );
            assert!(
                outcome.1 < Duration::from_millis(250),
                "EINTR extended a 50ms semaphore slice to {:?}",
                outcome.1
            );
            std::fs::write(marker, b"completed").unwrap();
            return;
        }

        let marker = PathBuf::from(format!("/tmp/srb-semaphore-eintr-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let test_name = "backends::semaphore::tests::finite_wait_deadline_survives_repeated_eintr";
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(test_name)
            .arg("--nocapture")
            .env(CHILD_MARKER_ENV, &marker)
            .status()
            .unwrap();
        let child_completed = marker.is_file();
        let _ = std::fs::remove_file(marker);

        assert!(status.success(), "semaphore EINTR child failed: {status}");
        assert!(child_completed, "semaphore EINTR child did not run");
    }
}
