#![allow(dead_code)]
//! Best-effort bridge to jwm's IPC socket, meant to let the picker resolve
//! `JWM_PORTAL_WINDOW=class:firefox` style queries against the live window
//! list, so we don't depend on the user having set wm_class properly in the
//! Wayland toplevel-list app_id.
//!
//! Not wired in yet: `picker::pick_windows` still matches `JWM_PORTAL_WINDOW`
//! against the Wayland toplevel list alone. The socket resolution, endpoint
//! checks and wire format here are kept to the compositor's contract so that
//! wiring it in is only a matter of calling [`query_windows`].
//!
//! Failure is non-fatal — the picker keeps the Wayland-bound app_id/title.

use std::ffi::OsStr;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct WindowInfo {
    pub id: u64,
    pub name: String,
    pub class: String,
    pub instance: String,
    #[serde(default)]
    pub tags: u32,
}

/// Mirror of the compositor's per-client IPC buffer ceiling, the cap the
/// bridge's client applies too: a stale or replaced peer that never ends its
/// line cannot make the portal buffer without bound.
const MAX_IPC_FRAME_BYTES: u64 = 1024 * 1024;

/// The envelope every jwm IPC reply is wrapped in (`jwm::ipc::IpcResponse`);
/// for `get_windows`, `data` is the window list.
#[derive(Debug, Deserialize)]
struct WindowsResponse {
    success: bool,
    #[serde(default)]
    data: Option<Vec<WindowInfo>>,
    #[serde(default)]
    error: Option<String>,
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail. Filesystem
    // ownership checks use the process's effective credentials, as the
    // compositor's do.
    unsafe { libc::geteuid() }
}

fn socket_path() -> PathBuf {
    resolve_socket_path(
        std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
        effective_uid(),
    )
}

/// Mirror of `jwm::ipc_server::socket_location`: an absolute
/// `XDG_RUNTIME_DIR` wins, otherwise the compositor serves from a per-uid
/// directory in `/tmp`. The endpoint is named `jwm-ipc.sock`; the old
/// `jwm.sock` never existed. A bare `/tmp/jwm-ipc.sock` fallback, or a
/// relative `XDG_RUNTIME_DIR` taken at face value, names a path the
/// compositor never serves, and one any local user could claim.
fn resolve_socket_path(runtime_dir: Option<&OsStr>, uid: u32) -> PathBuf {
    let runtime = runtime_dir
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute());
    match runtime {
        Some(runtime) => runtime.join("jwm-ipc.sock"),
        None => PathBuf::from(format!("/tmp/jwm-{uid}")).join("jwm-ipc.sock"),
    }
}

fn endpoint_error(kind: io::ErrorKind, path: &Path, message: &str) -> io::Error {
    io::Error::new(kind, format!("{}: {message}", path.display()))
}

/// Client half of `jwm::ipc_server::validate_private_directory`, the same
/// policy as the bridge's `validate_runtime_endpoint`: the runtime directory
/// must be a real directory owned by `uid` with no group or other access, and
/// the endpoint inside it must be a socket owned by `uid`.
///
/// The compositor never serves from a directory that fails these checks, so
/// refusing it cannot lock out a genuine jwm; it only stops the picker from
/// resolving windows against a listener another local user planted in `/tmp`.
/// The client never creates or chmods the directory. Once the directory is
/// private to `uid`, no other user can swap entries in it between this check
/// and the connect.
fn validate_runtime_endpoint(socket: &Path, uid: u32) -> io::Result<()> {
    let directory = socket
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            endpoint_error(
                io::ErrorKind::InvalidInput,
                socket,
                "jwm IPC socket path has no runtime directory",
            )
        })?;

    let metadata = fs::symlink_metadata(directory)?;
    if !metadata.file_type().is_dir() {
        return Err(endpoint_error(
            io::ErrorKind::InvalidInput,
            directory,
            "jwm IPC runtime path must be a real directory (not a symlink)",
        ));
    }
    if metadata.uid() != uid {
        return Err(endpoint_error(
            io::ErrorKind::PermissionDenied,
            directory,
            "jwm IPC runtime directory is not owned by the current user",
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(endpoint_error(
            io::ErrorKind::PermissionDenied,
            directory,
            "jwm IPC runtime directory must not be accessible by group or other users",
        ));
    }

    let metadata = fs::symlink_metadata(socket)?;
    if !metadata.file_type().is_socket() {
        return Err(endpoint_error(
            io::ErrorKind::InvalidInput,
            socket,
            "jwm IPC endpoint exists but is not a Unix socket",
        ));
    }
    if metadata.uid() != uid {
        return Err(endpoint_error(
            io::ErrorKind::PermissionDenied,
            socket,
            "jwm IPC socket is not owned by the current user",
        ));
    }
    Ok(())
}

/// Validate the runtime endpoint, then connect to it.
fn connect_validated(socket: &Path, uid: u32) -> io::Result<UnixStream> {
    validate_runtime_endpoint(socket, uid)?;
    UnixStream::connect(socket)
}

pub fn query_windows() -> std::io::Result<Vec<WindowInfo>> {
    query_windows_at(&socket_path(), effective_uid())
}

fn query_windows_at(socket: &Path, uid: u32) -> io::Result<Vec<WindowInfo>> {
    let mut sock = connect_validated(socket, uid)?;
    sock.set_read_timeout(Some(Duration::from_millis(500)))?;
    sock.set_write_timeout(Some(Duration::from_millis(500)))?;
    // The compositor reads newline-delimited JSON messages. A bare
    // `get_windows` line is a parse error it answers without closing.
    sock.write_all(b"{\"query\":\"get_windows\"}\n")?;
    // The connection stays open after the reply, so read exactly one frame:
    // reading to EOF would always end in the read timeout.
    read_windows_response(BufReader::new(&sock))
}

/// Parse one newline-terminated reply frame into the window list.
fn read_windows_response<R: BufRead>(reader: R) -> io::Result<Vec<WindowInfo>> {
    let mut frame = Vec::new();
    let read = reader
        .take(MAX_IPC_FRAME_BYTES + 1)
        .read_until(b'\n', &mut frame)?;
    if read == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "jwm closed the socket without responding",
        ));
    }
    if frame.last() != Some(&b'\n') && frame.len() as u64 > MAX_IPC_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "jwm IPC frame exceeded limit",
        ));
    }
    let response: WindowsResponse = serde_json::from_slice(&frame)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if !response.success {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            response
                .error
                .unwrap_or_else(|| "jwm rejected get_windows".to_string()),
        ));
    }
    response.data.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "jwm answered get_windows without a window list",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A per-test directory under the temp dir, never the real runtime dir.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(mode: u32) -> ScratchDir {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "jwm-portal-ipc-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir(&path).unwrap();
            // Set the mode explicitly: create_dir is filtered by the umask.
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            ScratchDir(path)
        }

        fn socket(&self) -> PathBuf {
            self.0.join("jwm-ipc.sock")
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_socket_is_resolved_like_the_compositor_resolves_it() {
        let uid = 4242;
        assert_eq!(
            resolve_socket_path(Some(OsStr::new("/run/user/4242")), uid),
            PathBuf::from("/run/user/4242/jwm-ipc.sock")
        );
        // Unset, empty and relative values all fall back to the per-uid
        // directory, never to a shared `/tmp/jwm-ipc.sock`.
        for runtime in [None, Some(""), Some("relative/run")] {
            assert_eq!(
                resolve_socket_path(runtime.map(OsStr::new), uid),
                PathBuf::from("/tmp/jwm-4242/jwm-ipc.sock"),
                "{runtime:?}"
            );
        }
    }

    #[test]
    fn a_socket_in_a_shared_directory_is_refused_before_connecting() {
        let dir = ScratchDir::new(0o755);
        let listener = UnixListener::bind(dir.socket()).unwrap();
        listener.set_nonblocking(true).unwrap();

        let error = connect_validated(&dir.socket(), effective_uid()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock),
            "the planted listener must never see a connection"
        );
    }

    #[test]
    fn a_private_runtime_directory_is_connected() {
        let dir = ScratchDir::new(0o700);
        let listener = UnixListener::bind(dir.socket()).unwrap();
        listener.set_nonblocking(true).unwrap();

        assert!(connect_validated(&dir.socket(), effective_uid()).is_ok());
        assert!(
            listener.accept().is_ok(),
            "the connect reached the listener"
        );
    }

    #[test]
    fn a_runtime_directory_owned_by_another_user_is_refused() {
        let dir = ScratchDir::new(0o700);
        let _listener = UnixListener::bind(dir.socket()).unwrap();

        validate_runtime_endpoint(&dir.socket(), effective_uid()).unwrap();
        let error =
            validate_runtime_endpoint(&dir.socket(), effective_uid().wrapping_add(1)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn a_symlinked_runtime_directory_is_refused() {
        let dir = ScratchDir::new(0o700);
        let real = dir.0.join("real");
        fs::create_dir(&real).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
        let _listener = UnixListener::bind(real.join("jwm-ipc.sock")).unwrap();
        let link = dir.0.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let error =
            validate_runtime_endpoint(&link.join("jwm-ipc.sock"), effective_uid()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn a_runtime_endpoint_that_is_not_a_socket_is_refused() {
        let dir = ScratchDir::new(0o700);
        fs::write(dir.socket(), b"not a socket").unwrap();

        let error = validate_runtime_endpoint(&dir.socket(), effective_uid()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    /// Regression: the query used to be the bare line `get_windows`, which
    /// the compositor rejects as a parse error, and the reply was read to EOF
    /// although the compositor keeps the connection open. The fake server
    /// answers like the real one and holds the socket until the client hangs
    /// up, so only a client that reads one frame gets the list.
    #[test]
    fn the_window_list_is_read_from_one_reply_while_the_socket_stays_open() {
        let dir = ScratchDir::new(0o700);
        let listener = UnixListener::bind(dir.socket()).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            // Bounds the test if the client never hangs up; not a sleep.
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&stream);
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            (&stream)
                .write_all(
                    br#"{"success":true,"data":[{"id":7,"name":"Mozilla Firefox","class":"firefox","instance":"Navigator","tags":2,"monitor":0,"is_focused":true}]}"#,
                )
                .unwrap();
            (&stream).write_all(b"\n").unwrap();
            // Keep the connection open until the client closes it.
            let mut rest = String::new();
            let closed = reader.read_line(&mut rest).map(|read| read == 0);
            (request, closed)
        });

        let windows = query_windows_at(&dir.socket(), effective_uid()).unwrap();
        let (request, closed) = server.join().unwrap();
        let request: serde_json::Value = serde_json::from_str(&request).unwrap();
        assert_eq!(request, serde_json::json!({ "query": "get_windows" }));
        assert!(matches!(closed, Ok(true)), "the client hung up: {closed:?}");
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].id, 7);
        assert_eq!(windows[0].class, "firefox");
        assert_eq!(windows[0].instance, "Navigator");
        assert_eq!(windows[0].tags, 2);
    }

    #[test]
    fn a_rejected_query_is_an_error_not_a_window_list() {
        let reply = b"{\"success\":false,\"error\":\"unknown query\"}\n";
        let error = read_windows_response(&reply[..]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("unknown query"), "{error}");

        let error = read_windows_response(&b"{\"success\":true}\n"[..]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_reply_frame_is_bounded_and_must_arrive() {
        let endless = BufReader::new(io::repeat(b'x').take(MAX_IPC_FRAME_BYTES * 2));
        let error = read_windows_response(endless).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        let error = read_windows_response(&b""[..]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_missing_runtime_endpoint_reads_as_not_found() {
        let dir = ScratchDir::new(0o700);
        let error = validate_runtime_endpoint(&dir.socket(), effective_uid()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
