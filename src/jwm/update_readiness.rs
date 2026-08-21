//! Stable aggregation fd for handler-owned external I/O.

use nix::errno::Errno;
use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags};
use std::io;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};

const READY_BATCH_CAPACITY: usize = 64;

/// Process-lifetime epoll layer registered once by backend event loops.
///
/// IPC and per-bar notifier descriptors can be added later without changing
/// the outer calloop registrations. Closing a source descriptor removes its
/// interest from epoll automatically; tokens are diagnostic only because one
/// readiness wake asks JWM to scan all current sources fairly.
#[derive(Debug)]
pub(crate) struct UpdateReadinessHub {
    epoll: Epoll,
    events: Vec<EpollEvent>,
    next_token: AtomicU64,
}

impl UpdateReadinessHub {
    pub(crate) fn new() -> io::Result<Self> {
        let epoll = Epoll::new(EpollCreateFlags::EPOLL_CLOEXEC).map_err(errno_io)?;
        Ok(Self {
            epoll,
            events: vec![EpollEvent::empty(); READY_BATCH_CAPACITY],
            next_token: AtomicU64::new(1),
        })
    }

    pub(crate) fn register(&self, fd: BorrowedFd<'_>) -> io::Result<u64> {
        let token = self.next_token.fetch_add(1, Ordering::Relaxed).max(1);
        self.epoll
            .add(fd, EpollEvent::new(EpollFlags::EPOLLIN, token))
            .map_err(errno_io)?;
        Ok(token)
    }

    pub(crate) fn duplicate_fd(&self) -> io::Result<OwnedFd> {
        self.epoll.0.try_clone()
    }

    pub(crate) fn unregister(&self, fd: BorrowedFd<'_>) -> io::Result<()> {
        self.epoll.delete(fd).map_err(errno_io)
    }

    /// Consume the aggregate ready list after individual owners have drained
    /// their level sources in the same handler update.
    pub(crate) fn drain(&mut self) -> io::Result<usize> {
        self.epoll.wait(&mut self.events, 0u8).map_err(errno_io)
    }
}

fn errno_io(error: Errno) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::fcntl::{FcntlArg, FdFlag, fcntl};
    use nix::poll::{PollFd, PollFlags, poll};
    use nix::sys::eventfd::{EfdFlags, EventFd};
    use std::os::fd::AsFd;

    fn readable(fd: &OwnedFd) -> bool {
        let mut descriptors = [PollFd::new(fd.as_fd(), PollFlags::POLLIN)];
        poll(&mut descriptors, 0u8).unwrap() > 0
    }

    #[test]
    fn stable_fd_tracks_dynamic_level_sources() {
        let mut hub = UpdateReadinessHub::new().unwrap();
        let stable = hub.duplicate_fd().unwrap();
        let source = EventFd::from_flags(EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK).unwrap();
        assert_eq!(hub.register(source.as_fd()).unwrap(), 1);
        assert!(!readable(&stable));

        source.write(1).unwrap();
        assert!(readable(&stable));
        assert_eq!(hub.drain().unwrap(), 1);
        assert!(readable(&stable), "the underlying level is still armed");

        assert_eq!(source.read().unwrap(), 1);
        hub.drain().unwrap();
        assert!(!readable(&stable));

        hub.unregister(source.as_fd()).unwrap();
        source.write(1).unwrap();
        assert!(!readable(&stable));
        assert_eq!(source.read().unwrap(), 1);
        drop(source);
        assert_eq!(hub.drain().unwrap(), 0);
        assert!(!readable(&stable));
    }

    #[test]
    fn ipc_style_nested_epoll_reaches_an_outer_event_loop() {
        let leaf = EventFd::from_flags(EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK).unwrap();
        let inner = Epoll::new(EpollCreateFlags::EPOLL_CLOEXEC).unwrap();
        inner
            .add(&leaf, EpollEvent::new(EpollFlags::EPOLLIN, 11))
            .unwrap();

        let mut hub = UpdateReadinessHub::new().unwrap();
        hub.register(inner.0.as_fd()).unwrap();
        let stable = hub.duplicate_fd().unwrap();
        let flags = FdFlag::from_bits_truncate(fcntl(&stable, FcntlArg::F_GETFD).unwrap());
        assert!(flags.contains(FdFlag::FD_CLOEXEC));

        let outer = Epoll::new(EpollCreateFlags::EPOLL_CLOEXEC).unwrap();
        outer
            .add(&stable, EpollEvent::new(EpollFlags::EPOLLIN, 22))
            .unwrap();
        leaf.write(1).unwrap();

        let mut outer_events = [EpollEvent::empty()];
        assert_eq!(outer.wait(&mut outer_events, 100u8).unwrap(), 1);
        assert_eq!(outer_events[0].data(), 22);

        let mut inner_events = [EpollEvent::empty()];
        assert_eq!(inner.wait(&mut inner_events, 0u8).unwrap(), 1);
        assert_eq!(leaf.read().unwrap(), 1);
        // Linux may retire the hub's queued level as soon as the nested epoll
        // becomes empty, or return that now-stale record once. Both converge.
        let _ = hub.drain().unwrap();
        assert_eq!(outer.wait(&mut outer_events, 0u8).unwrap(), 0);
    }
}
