//! Per-handler wakeup for asynchronous work completed off the event thread.
//!
//! The notifier is deliberately an explicit, cloneable capability rather
//! than a process-global singleton.  A worker may only wake the JWM instance
//! that attached it, which keeps nested/test handlers independent and makes
//! event-loop teardown naturally stop future notifications.

use nix::errno::Errno;
use nix::sys::eventfd::{EfdFlags, EventFd};
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Debug)]
pub struct AsyncUpdateNotifier {
    event: Arc<EventFd>,
    healthy: Arc<AtomicBool>,
}

impl AsyncUpdateNotifier {
    pub(crate) fn new() -> io::Result<Self> {
        let event = EventFd::from_flags(EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK)
            .map_err(errno_io)?;
        Ok(Self {
            event: Arc::new(event),
            healthy: Arc::new(AtomicBool::new(true)),
        })
    }

    pub(crate) fn as_fd(&self) -> BorrowedFd<'_> {
        self.event.as_fd()
    }

    /// Publish after the result itself has been made visible.
    pub(crate) fn notify(&self) -> bool {
        if let Err(error) = self.event.write(1)
            && error != Errno::EAGAIN
        {
            self.healthy.store(false, Ordering::Release);
            log::warn!("could not notify the update event loop: {error}");
            return false;
        }
        true
    }

    pub(crate) fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire)
    }

    pub(crate) fn mark_unhealthy(&self) {
        self.healthy.store(false, Ordering::Release);
    }

    /// Clear the accumulated level before owners inspect their queues.
    ///
    /// Draining first is important: a completion racing with the subsequent
    /// queue scan leaves the eventfd readable and therefore schedules another
    /// update instead of being accidentally consumed after its queue was
    /// already checked.
    pub(crate) fn drain(&self) -> io::Result<u64> {
        match self.event.read() {
            Ok(count) => Ok(count),
            Err(Errno::EAGAIN) => Ok(0),
            Err(error) => {
                self.mark_unhealthy();
                Err(errno_io(error))
            }
        }
    }
}

fn errno_io(error: Errno) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::poll::{PollFd, PollFlags, poll};

    fn readable(notifier: &AsyncUpdateNotifier) -> bool {
        let mut descriptors = [PollFd::new(notifier.as_fd(), PollFlags::POLLIN)];
        poll(&mut descriptors, 0u8).unwrap() > 0
    }

    #[test]
    fn clones_share_one_counted_level_and_drain_nonblocking() {
        let notifier = AsyncUpdateNotifier::new().unwrap();
        let worker = notifier.clone();
        assert!(!readable(&notifier));
        assert_eq!(notifier.drain().unwrap(), 0);

        worker.notify();
        worker.notify();
        assert!(worker.is_healthy());
        assert!(readable(&notifier));
        assert_eq!(notifier.drain().unwrap(), 2);
        assert!(!readable(&notifier));
        assert_eq!(worker.drain().unwrap(), 0);
    }
}
