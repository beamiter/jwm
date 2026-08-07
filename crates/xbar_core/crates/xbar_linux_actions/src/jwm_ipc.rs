//! A blocking, dependency-light client for jwm's control socket.
//!
//! Bars reach the window manager two ways. Bar *state* — workspaces, layout,
//! the window title — arrives over the shared-memory ring in
//! `shared_structures`, because it is a high-rate stream. A *request* like
//! "start a screenshot" is the opposite shape: it happens when a human clicks,
//! it needs an answer, and the answer is a sentence rather than a struct. That
//! is what the newline-delimited JSON socket at
//! `$XDG_RUNTIME_DIR/jwm-ipc.sock` is for, and this is the smallest client that
//! speaks it correctly.
//!
//! Everything here is deliberately bounded. A bar makes this call on the thread
//! that draws its next frame, so a wedged compositor has to fail fast rather
//! than freeze the panel: the socket carries a sub-second read/write timeout,
//! and a missing socket is reported as "jwm is not running" rather than
//! retried.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

/// Every socket operation is bounded. jwm answers a dispatch command from its
/// event loop, so a busy frame can delay the reply — but a bar must never hang
/// on one.
const IO_TIMEOUT: Duration = Duration::from_millis(500);

/// What can go wrong asking jwm to do something.
#[derive(Debug)]
pub enum JwmIpcError {
    /// The socket is not there, or refused the connection: no jwm is running
    /// on this seat.
    Unreachable {
        socket: PathBuf,
        source: std::io::Error,
    },
    /// The connection broke mid-request.
    Transport {
        socket: PathBuf,
        source: std::io::Error,
    },
    /// jwm answered, but the answer was not a response we understand.
    Malformed { response: String },
    /// jwm refused the command, and this is its own explanation.
    Refused { command: String, message: String },
}

impl std::fmt::Display for JwmIpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable { socket, source } => {
                write!(f, "jwm is not reachable on {}: {source}", socket.display())
            }
            Self::Transport { socket, source } => {
                write!(f, "jwm connection on {} failed: {source}", socket.display())
            }
            Self::Malformed { response } => {
                write!(f, "jwm sent a response we cannot read: {response}")
            }
            Self::Refused { command, message } => {
                write!(f, "jwm refused `{command}`: {message}")
            }
        }
    }
}

impl std::error::Error for JwmIpcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unreachable { source, .. } | Self::Transport { source, .. } => Some(source),
            Self::Malformed { .. } | Self::Refused { .. } => None,
        }
    }
}

/// A handle to one jwm control socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JwmIpc {
    socket: PathBuf,
}

impl Default for JwmIpc {
    fn default() -> Self {
        Self::new()
    }
}

impl JwmIpc {
    /// Resolve the socket the way the running compositor does.
    #[must_use]
    pub fn new() -> Self {
        Self {
            socket: socket_path(),
        }
    }

    /// Point at a specific socket — used by nested test sessions, and by any
    /// host that runs more than one compositor.
    #[must_use]
    pub fn at(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Send one command and wait for jwm to accept or refuse it.
    ///
    /// The `data` payload of a dispatch command is always null, so the success
    /// case carries nothing worth returning; what matters is *which* of the
    /// four failures happened, since only [`JwmIpcError::Unreachable`] means
    /// "there is no compositor to ask".
    pub fn command(&self, name: &str, args: Value) -> Result<(), JwmIpcError> {
        let stream =
            UnixStream::connect(&self.socket).map_err(|source| JwmIpcError::Unreachable {
                socket: self.socket.clone(),
                source,
            })?;
        let transport = |source| JwmIpcError::Transport {
            socket: self.socket.clone(),
            source,
        };
        stream
            .set_read_timeout(Some(IO_TIMEOUT))
            .map_err(transport)?;
        stream
            .set_write_timeout(Some(IO_TIMEOUT))
            .map_err(transport)?;

        let mut request = json!({ "command": name, "args": args }).to_string();
        request.push('\n');
        (&stream).write_all(request.as_bytes()).map_err(transport)?;
        (&stream).flush().map_err(transport)?;

        let mut line = String::new();
        BufReader::new(&stream)
            .read_line(&mut line)
            .map_err(transport)?;

        let response: Value =
            serde_json::from_str(line.trim()).map_err(|_| JwmIpcError::Malformed {
                response: line.trim().to_owned(),
            })?;
        match response.get("success").and_then(Value::as_bool) {
            Some(true) => Ok(()),
            Some(false) => Err(JwmIpcError::Refused {
                command: name.to_owned(),
                message: response
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("no reason given")
                    .to_owned(),
            }),
            None => Err(JwmIpcError::Malformed {
                response: line.trim().to_owned(),
            }),
        }
    }

    /// Ask jwm to enter its interactive region-capture mode.
    pub fn take_screenshot(&self) -> Result<(), JwmIpcError> {
        self.command(TAKE_SCREENSHOT, Value::Null)
    }
}

/// The IPC command name jwm registers for interactive capture.
pub const TAKE_SCREENSHOT: &str = "take_screenshot";
/// …and for the immediate whole-screen one.
pub const TAKE_SCREENSHOT_FULLSCREEN: &str = "take_screenshot_fullscreen";

/// Mirror of `jwm::ipc_server::socket_location`: an absolute `XDG_RUNTIME_DIR`
/// wins, otherwise the compositor falls back to a per-uid directory in `/tmp`.
/// `JWM_SOCKET` overrides both, which is how a nested session points a bar at a
/// private compositor.
fn socket_path() -> PathBuf {
    resolve_socket_path(
        std::env::var_os("JWM_SOCKET"),
        std::env::var_os("XDG_RUNTIME_DIR"),
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() },
    )
}

/// Split out from [`socket_path`] so the three-way precedence can be tested
/// without mutating the process environment.
fn resolve_socket_path(
    explicit: Option<std::ffi::OsString>,
    runtime_dir: Option<std::ffi::OsString>,
    uid: u32,
) -> PathBuf {
    if let Some(explicit) = explicit.filter(|value| !value.is_empty()) {
        return PathBuf::from(explicit);
    }
    let runtime = runtime_dir
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute());
    match runtime {
        Some(runtime) => runtime.join("jwm-ipc.sock"),
        None => PathBuf::from(format!("/tmp/jwm-{uid}")).join("jwm-ipc.sock"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::io::Read;
    use std::os::unix::net::UnixListener;

    #[test]
    fn an_explicit_socket_overrides_both_defaults() {
        assert_eq!(
            resolve_socket_path(
                Some(OsString::from("/run/nested/jwm.sock")),
                Some(OsString::from("/run/user/1000")),
                1000
            ),
            PathBuf::from("/run/nested/jwm.sock")
        );
    }

    #[test]
    fn an_absolute_runtime_dir_wins_over_the_uid_fallback() {
        assert_eq!(
            resolve_socket_path(None, Some(OsString::from("/run/user/1000")), 1000),
            PathBuf::from("/run/user/1000/jwm-ipc.sock")
        );
    }

    /// Empty and relative values are exactly the cases the compositor treats as
    /// "unset"; a client that disagreed would look in a directory jwm never
    /// binds in.
    #[test]
    fn an_empty_or_relative_runtime_dir_falls_back_to_the_uid_directory() {
        for runtime in [
            None,
            Some(OsString::from("")),
            Some(OsString::from("run/user")),
        ] {
            assert_eq!(
                resolve_socket_path(Some(OsString::from("")), runtime, 4242),
                PathBuf::from("/tmp/jwm-4242/jwm-ipc.sock")
            );
        }
    }

    fn scratch_socket(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("xbar-jwm-ipc-{name}-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// Stand up a one-shot server that answers with `reply` and hands back what
    /// the client actually wrote.
    fn serve_once(path: &Path, reply: &'static str) -> std::thread::JoinHandle<String> {
        let listener = UnixListener::bind(path).expect("bind scratch socket");
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = String::new();
            let mut byte = [0u8; 1];
            while stream.read(&mut byte).unwrap_or(0) == 1 {
                request.push(byte[0] as char);
                if byte[0] == b'\n' {
                    break;
                }
            }
            let _ = stream.write_all(reply.as_bytes());
            let _ = stream.flush();
            request
        })
    }

    #[test]
    fn a_screenshot_request_is_the_command_jwm_registers() {
        let path = scratch_socket("ok");
        let server = serve_once(&path, "{\"success\":true}\n");
        JwmIpc::at(&path).take_screenshot().expect("accepted");
        let request = server.join().expect("server thread");
        let sent: Value = serde_json::from_str(request.trim()).expect("valid JSON line");
        assert_eq!(sent["command"], "take_screenshot");
        assert!(sent["args"].is_null());
        assert!(request.ends_with('\n'), "the protocol is newline delimited");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_refusal_carries_jwms_own_explanation() {
        let path = scratch_socket("refused");
        let server = serve_once(
            &path,
            "{\"success\":false,\"error\":\"interactive screenshots require an active compositor\"}\n",
        );
        let error = JwmIpc::at(&path).take_screenshot().unwrap_err();
        server.join().expect("server thread");
        match &error {
            JwmIpcError::Refused { command, message } => {
                assert_eq!(command, "take_screenshot");
                assert!(message.contains("active compositor"), "{message}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(error.to_string().contains("active compositor"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_response_that_is_not_a_response_is_not_mistaken_for_success() {
        let path = scratch_socket("garbage");
        let server = serve_once(&path, "not json at all\n");
        let error = JwmIpc::at(&path).take_screenshot().unwrap_err();
        server.join().expect("server thread");
        assert!(matches!(error, JwmIpcError::Malformed { .. }), "{error:?}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_socket_reports_that_jwm_is_not_running() {
        let error = JwmIpc::at("/definitely/missing/jwm-ipc.sock")
            .take_screenshot()
            .unwrap_err();
        assert!(
            matches!(error, JwmIpcError::Unreachable { .. }),
            "{error:?}"
        );
        assert!(error.to_string().contains("not reachable"));
    }
}
