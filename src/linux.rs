//! Linux-specific, owned runtime primitives.
//!
//! The types in this module own their kernel resources.  Frontends can
//! register their file descriptors with epoll or another event loop without
//! having to coordinate a separate `close(2)` path.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::time::Duration;

/// A non-blocking monotonic timer whose first expiration is aligned to the
/// next whole `CLOCK_MONOTONIC` second.
///
/// Subsequent expirations use the configured interval.  The timer owns its
/// descriptor, sets `CLOEXEC`, and closes automatically when dropped.
#[derive(Debug)]
pub struct AlignedTimer {
    fd: OwnedFd,
}

impl AlignedTimer {
    /// Create and arm a timer with `interval` between expirations.
    ///
    /// The first expiration occurs at the next whole monotonic second rather
    /// than one interval after construction.  A zero interval is rejected.
    pub fn new(interval: Duration) -> io::Result<Self> {
        validate_interval(interval)?;

        let raw_fd = unsafe {
            libc::timerfd_create(
                libc::CLOCK_MONOTONIC,
                libc::TFD_NONBLOCK | libc::TFD_CLOEXEC,
            )
        };
        if raw_fd < 0 {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: `timerfd_create` returned a new descriptor and ownership is
        // transferred exactly once to `OwnedFd`.
        let timer = Self {
            fd: unsafe { OwnedFd::from_raw_fd(raw_fd) },
        };
        timer.set_interval(interval)?;
        Ok(timer)
    }

    /// Re-arm the timer at the next whole monotonic second and use `interval`
    /// for every subsequent expiration.
    pub fn set_interval(&self, interval: Duration) -> io::Result<()> {
        validate_interval(interval)?;

        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } < 0 {
            return Err(io::Error::last_os_error());
        }

        let next_second = now.tv_sec.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "timer deadline overflow")
        })?;
        let specification = libc::itimerspec {
            // `TFD_TIMER_ABSTIME` avoids losing alignment between
            // `clock_gettime` and `timerfd_settime`.
            it_value: libc::timespec {
                tv_sec: next_second,
                tv_nsec: 0,
            },
            it_interval: duration_to_timespec(interval)?,
        };

        let result = unsafe {
            libc::timerfd_settime(
                self.fd.as_raw_fd(),
                libc::TFD_TIMER_ABSTIME,
                &specification,
                std::ptr::null_mut(),
            )
        };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Drain all currently pending expirations.
    ///
    /// Since the descriptor is non-blocking, this returns `Ok(0)` when the
    /// timer has not fired.  A timerfd read atomically returns and clears its
    /// complete pending expiration count.
    pub fn drain(&self) -> io::Result<u64> {
        loop {
            let mut expirations = 0_u64;
            let read = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    (&mut expirations as *mut u64).cast::<libc::c_void>(),
                    std::mem::size_of::<u64>(),
                )
            };

            if read == std::mem::size_of::<u64>() as isize {
                // A timerfd read atomically returns and clears the complete
                // pending expiration count.
                return Ok(expirations);
            }
            if read < 0 {
                let error = io::Error::last_os_error();
                match error.raw_os_error() {
                    Some(libc::EINTR) => continue,
                    Some(libc::EAGAIN) => return Ok(0),
                    _ => return Err(error),
                }
            }
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "timerfd closed while draining",
                ));
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "timerfd returned a partial expiration counter",
            ));
        }
    }
}

impl AsFd for AlignedTimer {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl AsRawFd for AlignedTimer {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

fn validate_interval(interval: Duration) -> io::Result<()> {
    if interval.is_zero() {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "timer interval must be greater than zero",
        ))
    } else {
        // Validate platform representation before allocating a descriptor.
        duration_to_timespec(interval).map(|_| ())
    }
}

fn duration_to_timespec(duration: Duration) -> io::Result<libc::timespec> {
    let seconds = libc::time_t::try_from(duration.as_secs()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "timer interval is too large for time_t",
        )
    })?;
    Ok(libc::timespec {
        tv_sec: seconds,
        // Nanoseconds are below 1e9 and therefore fit even when `c_long` is
        // 32-bit.
        tv_nsec: duration.subsec_nanos() as libc::c_long,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero_interval_without_allocating_a_timer() {
        let error = AlignedTimer::new(Duration::ZERO).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn timer_is_nonblocking_cloexec_and_keeps_configured_interval() {
        let interval = Duration::from_millis(125);
        let timer = AlignedTimer::new(interval).unwrap();

        let status_flags = unsafe { libc::fcntl(timer.as_raw_fd(), libc::F_GETFL) };
        assert!(status_flags >= 0);
        assert_ne!(status_flags & libc::O_NONBLOCK, 0);

        let descriptor_flags = unsafe { libc::fcntl(timer.as_raw_fd(), libc::F_GETFD) };
        assert!(descriptor_flags >= 0);
        assert_ne!(descriptor_flags & libc::FD_CLOEXEC, 0);

        let mut current: libc::itimerspec = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::timerfd_gettime(timer.as_raw_fd(), &mut current) },
            0
        );
        assert_eq!(current.it_interval.tv_sec, 0);
        assert_eq!(current.it_interval.tv_nsec, 125_000_000);
        assert!(current.it_value.tv_sec == 0 || current.it_value.tv_sec == 1);
        assert!((0..1_000_000_000).contains(&current.it_value.tv_nsec));
        assert_eq!(timer.drain().unwrap(), 0);
    }

    #[test]
    fn fires_and_drains_without_an_external_runtime() {
        let timer = AlignedTimer::new(Duration::from_millis(20)).unwrap();
        let mut descriptor = libc::pollfd {
            fd: timer.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };

        let ready = unsafe { libc::poll(&mut descriptor, 1, 2_000) };
        assert_eq!(ready, 1, "aligned timer did not fire within two seconds");
        assert_ne!(descriptor.revents & libc::POLLIN, 0);
        assert!(timer.drain().unwrap() >= 1);
        assert_eq!(timer.drain().unwrap(), 0);
    }

    #[test]
    fn duplicated_descriptor_survives_timer_drop() {
        let timer = AlignedTimer::new(Duration::from_secs(1)).unwrap();
        let duplicated = timer.fd.try_clone().unwrap();
        drop(timer);

        let mut current: libc::itimerspec = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::timerfd_gettime(duplicated.as_raw_fd(), &mut current) },
            0
        );
        assert_eq!(current.it_interval.tv_sec, 1);
    }
}
