//! PipeWire's native socket discovery and bounded FD handoff.

use std::ffi::OsString;
use std::io;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::time::Duration;

pub async fn connect() -> io::Result<OwnedFd> {
    let path = socket_path(
        std::env::var_os("PIPEWIRE_REMOTE"),
        [
            std::env::var_os("PIPEWIRE_RUNTIME_DIR"),
            std::env::var_os("XDG_RUNTIME_DIR"),
            std::env::var_os("USERPROFILE"),
        ],
    )?;
    connect_at(path, Duration::from_millis(500)).await
}

async fn connect_at(path: PathBuf, timeout: Duration) -> io::Result<OwnedFd> {
    // A full AF_UNIX accept queue can block connect(). Keep both the wait and
    // its deadline on the blocking pool, leaving D-Bus workers available.
    tokio::task::spawn_blocking(move || {
        jwm_ipc_transport::connect(&path, timeout)
            .map(OwnedFd::from)
            .map_err(|error| {
                io::Error::new(error.kind(), format!("PipeWire socket {path:?}: {error}"))
            })
    })
    .await
    .map_err(io::Error::other)?
}

fn socket_path(
    remote: Option<OsString>,
    runtime_dirs: [Option<OsString>; 3],
) -> io::Result<PathBuf> {
    let remote = PathBuf::from(
        remote
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "pipewire-0".into()),
    );
    if remote.is_absolute() {
        return Ok(remote);
    }
    let directory = runtime_dirs
        .into_iter()
        .flatten()
        .find(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "set an absolute PIPEWIRE_RUNTIME_DIR, XDG_RUNTIME_DIR, or USERPROFILE",
            )
        })?;
    Ok(directory.join(remote))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_names_use_native_runtime_directory_precedence() {
        assert_eq!(
            socket_path(
                Some("capture-remote".into()),
                [
                    Some("/pipewire".into()),
                    Some("/xdg".into()),
                    Some("/profile".into())
                ],
            )
            .unwrap(),
            PathBuf::from("/pipewire/capture-remote"),
        );
        assert_eq!(
            socket_path(None, [None, Some("/xdg".into()), Some("/profile".into())]).unwrap(),
            PathBuf::from("/xdg/pipewire-0"),
        );
        assert_eq!(
            socket_path(
                Some("".into()),
                [Some("".into()), None, Some("/profile".into())]
            )
            .unwrap(),
            PathBuf::from("/profile/pipewire-0"),
        );
    }

    #[test]
    fn absolute_socket_needs_no_runtime_directory() {
        assert_eq!(
            socket_path(Some("/custom/pipewire".into()), [None, None, None]).unwrap(),
            PathBuf::from("/custom/pipewire"),
        );
    }

    #[test]
    fn missing_runtime_directory_cannot_connect_relative_to_the_cwd() {
        assert!(socket_path(None, [None, None, None]).is_err());
        assert!(
            socket_path(
                Some("pipewire-0".into()),
                [Some("relative".into()), None, None]
            )
            .is_err()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn full_socket_backlog_has_a_deadline_without_blocking_other_calls() {
        use std::os::fd::AsRawFd;
        use std::os::unix::net::{UnixListener, UnixStream};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, mpsc};

        let directory = std::env::temp_dir().join(format!("jwm-pw-backlog-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("pipewire-0");
        let listener = UnixListener::bind(&path).unwrap();
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
        let pending = UnixStream::connect(&path).unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        let watchdog = std::thread::spawn(move || {
            let _ = done_rx.recv_timeout(Duration::from_secs(2));
            drop((pending, listener));
        });

        let completed = Arc::new(AtomicBool::new(false));
        let worker_completed = completed.clone();
        let connection = tokio::spawn(async move {
            let result = connect_at(path, Duration::from_millis(100)).await;
            worker_completed.store(true, Ordering::Release);
            result
        });
        tokio::task::yield_now().await;
        let responsive = !completed.load(Ordering::Acquire);
        let result = connection.await.unwrap();
        let _ = done_tx.send(());
        watchdog.join().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
        assert!(responsive, "connection blocked the only async worker");
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
    }
}
