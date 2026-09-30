//! Bounded connection establishment for local control-plane clients.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "IPC operation deadline exceeded"))
}

/// Write a request under one deadline, including all partial-write retries.
/// Response reads retain the caller's existing timeout/subscription policy.
pub fn write_all_with_timeout(
    stream: &mut UnixStream,
    bytes: &[u8],
    timeout: Duration,
) -> io::Result<()> {
    use std::io::Write as _;
    if timeout.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "IPC write timeout is zero",
        ));
    }
    let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "IPC write timeout is too large",
        )
    })?;
    let mut written = 0;
    while written < bytes.len() {
        stream.set_write_timeout(Some(remaining(deadline)?))?;
        match stream.write(&bytes[written..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "IPC request write returned zero",
                ));
            }
            Ok(count) => written += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Connect to a filesystem Unix socket within `timeout` and return a blocking
/// stream. A full listener backlog cannot leave control tools waiting forever.
/// Read/write deadlines are configured separately by the caller.
pub fn connect(path: &Path, timeout: Duration) -> io::Result<UnixStream> {
    if timeout.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "IPC connection timeout is zero",
        ));
    }
    let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "IPC connection timeout is too large",
        )
    })?;
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: sockaddr_un contains only integer fields and a byte array;
    // its all-zero representation is valid before filling the family/path.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.is_empty() || bytes.contains(&0) || bytes.len() >= address.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid filesystem Unix socket path",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (destination, byte) in address.sun_path.iter_mut().zip(bytes) {
        *destination = *byte as libc::c_char;
    }
    let address_len = std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
    // SAFETY: socket has no pointer arguments. The flags create a stream
    // descriptor with nonblocking and close-on-exec set atomically.
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful socket() returned this newly owned descriptor.
    // OwnedFd and then UnixStream close it on every success/error path.
    let stream = UnixStream::from(unsafe { OwnedFd::from_raw_fd(fd) });
    loop {
        remaining(deadline)?;
        // SAFETY: stream owns a valid fd; address points to an initialized
        // sockaddr_un whose pathname plus terminator fits address_len.
        let result = unsafe {
            libc::connect(
                stream.as_raw_fd(),
                &address as *const _ as *const libc::sockaddr,
                address_len as libc::socklen_t,
            )
        };
        if result == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EISCONN) => break,
            Some(libc::EINTR) => continue,
            Some(libc::EAGAIN) => {
                // Linux AF_UNIX has not initiated a connection when the
                // accept queue is full. Retry with bounded backoff rather
                // than polling a not-yet-connected socket in a busy loop.
                std::thread::sleep(remaining(deadline)?.min(Duration::from_millis(5)));
            }
            Some(libc::EINPROGRESS) | Some(libc::EALREADY) => {
                loop {
                    let duration = remaining(deadline)?;
                    let millis =
                        duration.as_millis().saturating_add(1).min(i32::MAX as u128) as i32;
                    let mut descriptor = libc::pollfd {
                        fd: stream.as_raw_fd(),
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    // SAFETY: poll receives one valid initialized pollfd;
                    // the owned stream outlives the syscall.
                    let ready = unsafe { libc::poll(&mut descriptor, 1, millis) };
                    if ready < 0 {
                        let error = io::Error::last_os_error();
                        if error.kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        return Err(error);
                    }
                    if ready == 0 {
                        continue;
                    }
                    if let Some(error) = stream.take_error()? {
                        return Err(error);
                    }
                    break;
                }
                break;
            }
            _ => return Err(error),
        }
    }
    remaining(deadline)?;
    stream.set_nonblocking(false)?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct SocketDir(std::path::PathBuf);
    impl SocketDir {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "jwm-connect-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn socket(&self) -> std::path::PathBuf {
            self.0.join("ipc.sock")
        }
    }
    impl Drop for SocketDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn one_pending_connection(path: &Path) -> (UnixListener, UnixStream) {
        let listener = UnixListener::bind(path).unwrap();
        // SAFETY: listener owns a valid listening socket; backlog zero
        // leaves one pending slot on Linux, which the next connection fills.
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
        let pending = UnixStream::connect(path).unwrap();
        (listener, pending)
    }

    fn accept_before(listener: &UnixListener, deadline: Instant) -> io::Result<UnixStream> {
        loop {
            remaining(deadline)?;
            match listener.accept() {
                Ok((stream, _)) => return Ok(stream),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(remaining(deadline)?.min(Duration::from_millis(5)));
                }
                Err(error) => return Err(error),
            }
        }
    }

    #[test]
    fn connected_stream_is_blocking_and_close_on_exec() {
        let dir = SocketDir::new();
        let listener = UnixListener::bind(dir.socket()).unwrap();
        let mut client = connect(&dir.socket(), Duration::from_secs(1)).unwrap();
        // SAFETY: both fcntl calls inspect a live descriptor owned by client.
        let status = unsafe { libc::fcntl(client.as_raw_fd(), libc::F_GETFL) };
        let descriptor = unsafe { libc::fcntl(client.as_raw_fd(), libc::F_GETFD) };
        assert!(status >= 0 && status & libc::O_NONBLOCK == 0);
        assert!(descriptor >= 0 && descriptor & libc::FD_CLOEXEC != 0);
        let (mut server, _) = listener.accept().unwrap();
        server.write_all(b"ok").unwrap();
        let mut received = [0; 2];
        client.read_exact(&mut received).unwrap();
        assert_eq!(&received, b"ok");
    }

    #[test]
    fn full_accept_queue_is_bounded_by_connection_deadline() {
        let dir = SocketDir::new();
        let (_listener, _pending) = one_pending_connection(&dir.socket());
        let started = Instant::now();
        let error = connect(&dir.socket(), Duration::from_millis(40)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn connection_retries_when_the_accept_queue_drains() {
        let dir = SocketDir::new();
        let (listener, _pending) = one_pending_connection(&dir.socket());
        listener.set_nonblocking(true).unwrap();
        let worker = std::thread::spawn(move || -> io::Result<()> {
            let deadline = Instant::now() + Duration::from_secs(2);
            std::thread::sleep(Duration::from_millis(20));
            drop(accept_before(&listener, deadline)?);
            let mut server = accept_before(&listener, deadline)?;
            server.set_write_timeout(Some(Duration::from_secs(1)))?;
            server.write_all(b"ready")
        });
        let received = (|| -> io::Result<[u8; 5]> {
            let mut client = connect(&dir.socket(), Duration::from_secs(1))?;
            client.set_read_timeout(Some(Duration::from_secs(1)))?;
            let mut received = [0; 5];
            client.read_exact(&mut received)?;
            Ok(received)
        })();
        let worker_result = worker.join().unwrap();
        assert_eq!(&received.unwrap(), b"ready");
        worker_result.unwrap();
    }

    #[test]
    fn request_write_is_bounded_when_peer_does_not_read() {
        let (mut client, _server) = UnixStream::pair().unwrap();
        let started = Instant::now();
        let error = write_all_with_timeout(
            &mut client,
            &vec![0; 4 * 1024 * 1024],
            Duration::from_millis(40),
        )
        .unwrap_err();
        assert!(matches!(
            error.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn request_write_preserves_all_bytes() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        write_all_with_timeout(&mut client, b"request\n", Duration::from_secs(1)).unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut received = [0; 8];
        server.read_exact(&mut received).unwrap();
        assert_eq!(&received, b"request\n");
    }

    #[test]
    fn invalid_paths_and_timeouts_fail_without_waiting() {
        let dir = SocketDir::new();
        assert_eq!(
            connect(&dir.socket(), Duration::ZERO).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            connect(Path::new(""), Duration::from_secs(1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            connect(Path::new("a\0b"), Duration::from_secs(1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            connect(&dir.socket(), Duration::from_secs(1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
    }
}
