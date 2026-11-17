//! One per-session, nonblocking hint. Queues remain the authoritative data.
use nix::errno::Errno;
use nix::sys::eventfd::{EfdFlags, EventFd};
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(super) struct InputWake {
    event: Arc<EventFd>,
    healthy: AtomicBool,
    #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
    probe: Option<Arc<InputWakeProbe>>,
}
impl InputWake {
    pub(super) fn new() -> io::Result<Self> {
        Ok(Self {
            event: Arc::new(
                EventFd::from_flags(EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK)
                    .map_err(errno_io)?,
            ),
            healthy: AtomicBool::new(true),
            #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
            probe: test_probe(),
        })
    }
    pub(super) fn as_fd(&self) -> BorrowedFd<'_> {
        self.event.as_fd()
    }
    pub(super) fn notify(&self) -> io::Result<()> {
        #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
        if let Some(p) = &self.probe {
            p.notify_attempts.fetch_add(1, Ordering::Relaxed);
        }
        match self.event.write(1) {
            Ok(_) => Ok(()),
            Err(Errno::EAGAIN) => {
                #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
                if let Some(p) = &self.probe {
                    p.notify_coalesced.fetch_add(1, Ordering::Relaxed);
                }
                Ok(())
            }
            Err(error) => {
                self.healthy.store(false, Ordering::Release);
                Err(errno_io(error))
            }
        }
    }
    // One eventfd read clears its accumulated level. Do not drain in a loop:
    // publishers can race indefinitely, and the next queue scan is mandatory.
    pub(super) fn drain(&self) -> io::Result<()> {
        if !self.healthy.load(Ordering::Acquire) {
            return Err(io::Error::other("native input wake failed"));
        }
        #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
        if let Some(p) = &self.probe {
            p.drain_calls.fetch_add(1, Ordering::Relaxed);
        }
        match self.event.read() {
            Ok(count) => {
                #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
                if let Some(p) = &self.probe {
                    p.drained_signals.fetch_add(count, Ordering::Relaxed);
                }
                let _ = count;
                Ok(())
            }
            Err(Errno::EAGAIN) => Ok(()),
            Err(error) => {
                self.healthy.store(false, Ordering::Release);
                Err(errno_io(error))
            }
        }
    }
}
fn errno_io(error: Errno) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}

pub(super) fn enabled() -> bool {
    #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
    {
        TEST_CONFIG.with(|value| value.borrow().as_ref().is_none_or(|(enabled, _)| *enabled))
    }
    #[cfg(not(all(test, feature = "remote-wayland", feature = "backend-wayland-udev")))]
    {
        true
    }
}

#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
#[derive(Default)]
pub(in crate::remote) struct InputWakeProbe {
    notify_attempts: std::sync::atomic::AtomicU64,
    notify_coalesced: std::sync::atomic::AtomicU64,
    drain_calls: std::sync::atomic::AtomicU64,
    drained_signals: std::sync::atomic::AtomicU64,
    activity_waits: std::sync::atomic::AtomicU64,
}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
pub(in crate::remote) struct InputWakeSnapshot {
    pub notify_attempts: u64,
    pub notify_coalesced: u64,
    pub drain_calls: u64,
    pub drained_signals: u64,
    pub activity_waits: u64,
}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
impl InputWakeProbe {
    pub(in crate::remote) fn snapshot(&self) -> InputWakeSnapshot {
        InputWakeSnapshot {
            notify_attempts: self.notify_attempts.load(Ordering::Relaxed),
            notify_coalesced: self.notify_coalesced.load(Ordering::Relaxed),
            drain_calls: self.drain_calls.load(Ordering::Relaxed),
            drained_signals: self.drained_signals.load(Ordering::Relaxed),
            activity_waits: self.activity_waits.load(Ordering::Relaxed),
        }
    }
    pub(super) fn record_wait(&self) {
        self.activity_waits.fetch_add(1, Ordering::Relaxed);
    }
}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
thread_local! {
    static TEST_CONFIG: std::cell::RefCell<Option<(bool, Arc<InputWakeProbe>)>> = const { std::cell::RefCell::new(None) };
}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
pub(super) fn test_probe() -> Option<Arc<InputWakeProbe>> {
    TEST_CONFIG.with(|value| value.borrow().as_ref().map(|(_, p)| p.clone()))
}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
pub(in crate::remote) fn with_input_wake<T>(
    enabled: bool,
    probe: Arc<InputWakeProbe>,
    run: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<(bool, Arc<InputWakeProbe>)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_CONFIG.with(|value| *value.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(TEST_CONFIG.with(|value| value.replace(Some((enabled, probe)))));
    run()
}

#[cfg(test)]
mod tests {
    use super::*;
    // No descriptors: exercise publication/drain/queue ordering explicitly.
    #[test]
    fn input_wake_model_drain_before_scan_preserves_racing_publication() {
        for publish_after_drain in [false, true] {
            let mut queue = std::collections::VecDeque::new();
            let mut hint = 0;
            if !publish_after_drain {
                queue.push_back(1);
                hint += 1;
            }
            let drained = std::mem::take(&mut hint);
            if publish_after_drain {
                queue.push_back(1);
                hint += 1;
            }
            assert!(drained > 0 || hint > 0);
            assert_eq!(queue.pop_front(), Some(1));
            assert!(queue.is_empty());
        }
        // Coalesced drain is not an instruction to handle only one command.
        let mut queue: std::collections::VecDeque<_> = (0..32).collect();
        for expected in 0..32 {
            assert_eq!(queue.pop_front(), Some(expected));
            assert_eq!(!queue.is_empty(), expected != 31);
        }
    }
    // Real fd tests: CI only, never used as the local pure-model filter.
    #[test]
    fn input_wake_fd_coalesces_and_nonblocking_drain_clears_readiness() {
        use nix::poll::{PollFd, PollFlags, poll};
        let wake = InputWake::new().unwrap();
        wake.drain().unwrap();
        for _ in 0..32 {
            wake.notify().unwrap();
        }
        let mut descriptors = [PollFd::new(wake.as_fd(), PollFlags::POLLIN)];
        assert_eq!(poll(&mut descriptors, 0u8).unwrap(), 1);
        wake.drain().unwrap();
        assert_eq!(poll(&mut descriptors, 0u8).unwrap(), 0);
        wake.notify().unwrap();
        assert_eq!(poll(&mut descriptors, 0u8).unwrap(), 1);
    }
    #[test]
    fn input_wake_fd_publication_races_after_drain_and_after_scan_are_not_lost() {
        use nix::poll::{PollFd, PollFlags, poll};
        use std::sync::{Barrier, Mutex};
        for after_scan in [false, true] {
            let wake = Arc::new(InputWake::new().unwrap());
            let queue = Arc::new(Mutex::new(std::collections::VecDeque::new()));
            let publish = Arc::new(Barrier::new(2));
            let published = Arc::new(Barrier::new(2));
            let worker = {
                let (wake, queue, publish, published) = (
                    wake.clone(),
                    queue.clone(),
                    publish.clone(),
                    published.clone(),
                );
                std::thread::spawn(move || {
                    publish.wait();
                    queue.lock().unwrap().push_back(17);
                    wake.notify().unwrap(); // publish before signal, no held queue lock
                    published.wait();
                })
            };
            wake.drain().unwrap();
            if after_scan {
                assert!(queue.lock().unwrap().is_empty());
            }
            publish.wait();
            published.wait();
            // Even if the prior scan was empty, publication remains level
            // readable before the consumer enters poll. No timing sleeps.
            let mut fds = [PollFd::new(wake.as_fd(), PollFlags::POLLIN)];
            assert_eq!(poll(&mut fds, 0u8).unwrap(), 1);
            wake.drain().unwrap();
            assert_eq!(queue.lock().unwrap().pop_front(), Some(17));
            assert!(queue.lock().unwrap().is_empty());
            assert_eq!(poll(&mut fds, 0u8).unwrap(), 0);
            worker.join().unwrap();
        }
    }

    #[test]
    fn input_wake_fd_is_per_session_and_last_owner_closes_resource() {
        use nix::poll::{PollFd, PollFlags, poll};
        let first = Arc::new(InputWake::new().unwrap());
        let second = InputWake::new().unwrap();
        let publisher = first.clone();
        let lifetime = Arc::downgrade(&first.event);
        publisher.notify().unwrap();
        let mut other = [PollFd::new(second.as_fd(), PollFlags::POLLIN)];
        assert_eq!(poll(&mut other, 0u8).unwrap(), 0, "no cross-session wake");
        drop(first);
        assert!(lifetime.upgrade().is_some(), "publisher retains its own fd");
        publisher.drain().unwrap();
        drop(publisher);
        assert!(
            lifetime.upgrade().is_none(),
            "EventFd owned descriptor dropped with last session owner"
        );
        second.notify().unwrap();
        assert_eq!(
            poll(&mut other, 0u8).unwrap(),
            1,
            "other session remains live"
        );
    }
}
