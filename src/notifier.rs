//! Owned Linux eventfd bridge for shared-memory notifications.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use log::warn;
use shared_structures::SharedRingBuffer;

const WAIT_SLICE: Duration = Duration::from_millis(250);
const LEVEL_RECHECK: Duration = Duration::from_millis(10);

/// An owned eventfd plus its notification worker.
///
/// The handle owns both the file descriptor and the worker lifetime. Dropping
/// it requests cancellation, waits for the bounded `wait_for_message` call to
/// return, joins the thread, and only then closes the descriptor. This ordering
/// prevents a worker from writing through a descriptor number that the process
/// has already reused for another resource.
#[derive(Debug)]
pub struct SharedEventNotifier {
    fd: OwnedFd,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    non_blocking: bool,
}

impl SharedEventNotifier {
    /// Spawn a notifier for an existing shared ring buffer.
    pub(crate) fn spawn(buffer: Arc<SharedRingBuffer>, non_blocking: bool) -> io::Result<Self> {
        let flags = libc::EFD_CLOEXEC | if non_blocking { libc::EFD_NONBLOCK } else { 0 };
        let raw_fd = unsafe { libc::eventfd(0, flags) };
        if raw_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);

        // Safety invariant: `Drop` joins `worker` before `fd` is dropped, so
        // this descriptor stays valid for the entire closure lifetime.
        let worker_fd = fd.as_raw_fd();
        let worker = thread::Builder::new()
            .name("xbar-shared-notifier".into())
            .spawn(move || worker_loop(buffer, worker_fd, worker_stop));

        match worker {
            Ok(worker) => Ok(Self {
                fd,
                stop,
                worker: Some(worker),
                non_blocking,
            }),
            Err(error) => Err(error),
        }
    }

    /// Drain pending notifications and return their accumulated eventfd count.
    /// Call this only after the descriptor has reported readability when using
    /// a blocking notifier.
    pub fn drain(&self) -> io::Result<u64> {
        let mut total = 0_u64;
        loop {
            let mut value = 0_u64;
            let read = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    (&mut value as *mut u64).cast::<libc::c_void>(),
                    size_of::<u64>(),
                )
            };
            if read == size_of::<u64>() as isize {
                total = total.saturating_add(value);
                if !self.non_blocking {
                    return Ok(total);
                }
                continue;
            }
            if read < 0 {
                let error = io::Error::last_os_error();
                match error.raw_os_error() {
                    Some(libc::EINTR) => continue,
                    Some(libc::EAGAIN) => return Ok(total),
                    _ => return Err(error),
                }
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "short read from eventfd",
            ));
        }
    }

    /// Ask the worker to stop. The worker is joined when this handle is
    /// dropped; repeated calls are harmless.
    pub fn request_shutdown(&self) {
        self.stop.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn is_shutdown_requested(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }
}

impl AsFd for SharedEventNotifier {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl AsRawFd for SharedEventNotifier {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl Drop for SharedEventNotifier {
    fn drop(&mut self) {
        self.request_shutdown();
        if let Some(worker) = self.worker.take()
            && let Err(payload) = worker.join()
        {
            warn!("shared notifier worker panicked: {payload:?}");
        }
    }
}

fn worker_loop(buffer: Arc<SharedRingBuffer>, fd: RawFd, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Acquire) {
        match buffer.wait_for_message(Some(WAIT_SLICE)) {
            Ok(true) => {
                // Keep eventfd level-triggered while the ring is non-empty,
                // but never increment an already-readable counter.  Calling
                // `wait_for_message` again immediately would busy-loop because
                // its predicate remains true until the frontend consumes the
                // ring.  Checking the descriptor also closes the drain/read
                // race: if the frontend drains eventfd before it drains the
                // ring, the worker re-arms one notification.
                while buffer.has_message() && !stop.load(Ordering::Acquire) {
                    match eventfd_is_readable(fd) {
                        Ok(true) => {}
                        Ok(false) => {
                            if let Err(error) = write_eventfd(fd, 1) {
                                match error.raw_os_error() {
                                    Some(libc::EAGAIN) => {}
                                    Some(libc::EBADF) => return,
                                    _ => warn!("shared notifier eventfd write failed: {error}"),
                                }
                            }
                        }
                        Err(error) => {
                            if error.raw_os_error() == Some(libc::EBADF) {
                                return;
                            }
                            warn!("shared notifier eventfd poll failed: {error}");
                            break;
                        }
                    }
                    thread::sleep(LEVEL_RECHECK);
                }
            }
            Ok(false) => {
                if buffer.is_destroyed() {
                    break;
                }
                // Normally this is the timeout. Yield briefly in case a
                // backend reports a transient false result before the slice.
                if !stop.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(1));
                }
            }
            Err(error) => {
                if !stop.load(Ordering::Acquire) {
                    warn!("shared notifier wait failed: {error}");
                }
                break;
            }
        }
    }
}

fn eventfd_is_readable(fd: RawFd) -> io::Result<bool> {
    loop {
        let mut descriptor = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if ready > 0 {
            return Ok(descriptor.revents & libc::POLLIN != 0);
        }
        if ready == 0 {
            return Ok(false);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(error);
        }
    }
}

fn write_eventfd(fd: RawFd, value: u64) -> io::Result<()> {
    loop {
        let written = unsafe {
            libc::write(
                fd,
                (&value as *const u64).cast::<libc::c_void>(),
                size_of::<u64>(),
            )
        };
        if written == size_of::<u64>() as isize {
            return Ok(());
        }
        if written < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(error);
        }
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "short write to eventfd",
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_structures::SharedMessage;
    use std::sync::atomic::AtomicU64;

    static NEXT_TEST_PATH: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn eventfd_counter_round_trips_and_drains() {
        let raw_fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        assert!(raw_fd >= 0);
        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

        write_eventfd(fd.as_raw_fd(), 2).unwrap();
        write_eventfd(fd.as_raw_fd(), 3).unwrap();

        let mut value = 0_u64;
        let read = unsafe {
            libc::read(
                fd.as_raw_fd(),
                (&mut value as *mut u64).cast::<libc::c_void>(),
                size_of::<u64>(),
            )
        };
        assert_eq!(read, size_of::<u64>() as isize);
        assert_eq!(value, 5);
    }

    #[test]
    fn notifier_coalesces_one_unconsumed_ring_batch() {
        let sequence = NEXT_TEST_PATH.fetch_add(1, Ordering::Relaxed);
        let path = format!("/tmp/xbar-core-notifier-{}-{sequence}", std::process::id());
        let buffer = Arc::new(
            SharedRingBuffer::create_aux(&path, Some(8), Some(0))
                .expect("create isolated shared ring"),
        );
        let notifier = SharedEventNotifier::spawn(Arc::clone(&buffer), true).unwrap();

        assert!(buffer.try_write_message(&SharedMessage::default()).unwrap());
        let mut descriptor = libc::pollfd {
            fd: notifier.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut descriptor, 1, 2_000) }, 1);

        // Leaving the ring untouched used to make the worker increment the
        // eventfd counter in a tight loop. It must remain a single readable
        // level notification instead.
        thread::sleep(Duration::from_millis(100));
        assert_eq!(notifier.drain().unwrap(), 1);

        // Draining only eventfd causes one safe re-arm while the ring remains
        // readable; consuming the ring then lets the worker sleep again.
        descriptor.revents = 0;
        assert_eq!(unsafe { libc::poll(&mut descriptor, 1, 2_000) }, 1);
        assert_eq!(notifier.drain().unwrap(), 1);
        assert!(buffer.try_read_latest_message().unwrap().is_some());
        notifier.request_shutdown();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !notifier.worker.as_ref().unwrap().is_finished()
            && std::time::Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(notifier.worker.as_ref().unwrap().is_finished());
        let _ = notifier.drain(); // tolerate one write racing with ring drain
        assert_eq!(notifier.drain().unwrap(), 0);

        buffer.destroy().unwrap();
    }

    #[test]
    fn notifier_worker_stops_when_the_owner_destroys_the_ring() {
        let sequence = NEXT_TEST_PATH.fetch_add(1, Ordering::Relaxed);
        let path = format!(
            "/tmp/xbar-core-notifier-destroy-{}-{sequence}",
            std::process::id()
        );
        let buffer = Arc::new(
            SharedRingBuffer::create_aux(&path, Some(8), Some(0))
                .expect("create isolated shared ring"),
        );
        let notifier = SharedEventNotifier::spawn(Arc::clone(&buffer), true).unwrap();

        buffer.destroy().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !notifier.worker.as_ref().unwrap().is_finished()
            && std::time::Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(notifier.worker.as_ref().unwrap().is_finished());
    }
}
