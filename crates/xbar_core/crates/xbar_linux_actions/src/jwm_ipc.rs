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
//! than freeze the panel: request and response bytes are capped, post-connect
//! transport shares one sub-second deadline, and a missing socket is reported
//! as "jwm is not running" rather than retried.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

/// Maximum encoded JSON request size, including its newline delimiter.
pub const MAX_IPC_REQUEST_BYTES: usize = 64 * 1024;
/// Maximum response line size, including its optional newline delimiter.
pub const MAX_IPC_RESPONSE_BYTES: usize = 64 * 1024;

/// All I/O after connection shares one deadline. jwm answers a dispatch from
/// its event loop, so a busy frame can delay the reply — but not indefinitely.
const IO_TIMEOUT: Duration = Duration::from_millis(500);
const IO_POLL_INTERVAL: Duration = Duration::from_millis(5);

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
    /// A caller tried to serialize more data than one control request permits.
    RequestTooLarge { limit: usize },
    /// jwm sent more than one bounded response line permits.
    ResponseTooLarge { limit: usize },
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
            Self::RequestTooLarge { limit } => {
                write!(f, "jwm request exceeds the {limit}-byte limit")
            }
            Self::ResponseTooLarge { limit } => {
                write!(f, "jwm response exceeds the {limit}-byte limit")
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
            Self::Malformed { .. }
            | Self::RequestTooLarge { .. }
            | Self::ResponseTooLarge { .. }
            | Self::Refused { .. } => None,
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
    /// failure classes happened, since only [`JwmIpcError::Unreachable`] means
    /// "there is no compositor to ask".
    pub fn command(&self, name: &str, args: Value) -> Result<(), JwmIpcError> {
        let request = encode_request(name, &args).map_err(|()| JwmIpcError::RequestTooLarge {
            limit: MAX_IPC_REQUEST_BYTES,
        })?;
        let mut stream =
            UnixStream::connect(&self.socket).map_err(|source| JwmIpcError::Unreachable {
                socket: self.socket.clone(),
                source,
            })?;
        let transport = |source| JwmIpcError::Transport {
            socket: self.socket.clone(),
            source,
        };
        stream.set_nonblocking(true).map_err(transport)?;
        let deadline = Instant::now() + IO_TIMEOUT;
        write_request(&mut stream, &request, deadline).map_err(transport)?;
        let response =
            read_response(&mut stream, deadline, MAX_IPC_RESPONSE_BYTES).map_err(|error| {
                match error {
                    ResponseReadError::Io(source) => transport(source),
                    ResponseReadError::TooLarge => JwmIpcError::ResponseTooLarge {
                        limit: MAX_IPC_RESPONSE_BYTES,
                    },
                }
            })?;
        let line = String::from_utf8(response).map_err(|error| {
            transport(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
        })?;

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

struct RequestBuffer {
    bytes: Vec<u8>,
    limit: usize,
    overflowed: bool,
}

impl RequestBuffer {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(limit.min(1024)),
            limit,
            overflowed: false,
        }
    }
}

impl Write for RequestBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            self.overflowed = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "encoded request exceeds its byte limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn encode_request(name: &str, args: &Value) -> Result<Vec<u8>, ()> {
    let mut request = RequestBuffer::new(MAX_IPC_REQUEST_BYTES);
    request.write_all(b"{\"command\":").map_err(|_| ())?;
    if serde_json::to_writer(&mut request, name).is_err() {
        debug_assert!(request.overflowed);
        return Err(());
    }
    request.write_all(b",\"args\":").map_err(|_| ())?;
    if serde_json::to_writer(&mut request, args).is_err() {
        debug_assert!(request.overflowed);
        return Err(());
    }
    request.write_all(b"}\n").map_err(|_| ())?;
    Ok(request.bytes)
}

fn write_request(
    stream: &mut UnixStream,
    request: &[u8],
    deadline: Instant,
) -> std::io::Result<()> {
    let mut written = 0;
    while written < request.len() {
        ensure_before_deadline(deadline)?;
        match stream.write(&request[written..]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "jwm socket stopped accepting the request",
                ));
            }
            Ok(count) => written += count,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                wait_for_io(deadline)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[derive(Debug)]
enum ResponseReadError {
    Io(std::io::Error),
    TooLarge,
}

fn read_response(
    stream: &mut UnixStream,
    deadline: Instant,
    limit: usize,
) -> Result<Vec<u8>, ResponseReadError> {
    let mut response = Vec::with_capacity(limit.min(1024));
    let mut chunk = [0_u8; 4096];
    loop {
        ensure_before_deadline(deadline).map_err(ResponseReadError::Io)?;
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(response),
            Ok(count) => {
                let newline = chunk[..count].iter().position(|byte| *byte == b'\n');
                let retained = newline.map_or(count, |position| position + 1);
                if retained > limit.saturating_sub(response.len()) {
                    return Err(ResponseReadError::TooLarge);
                }
                response.extend_from_slice(&chunk[..retained]);
                if newline.is_some() {
                    return Ok(response);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                wait_for_io(deadline).map_err(ResponseReadError::Io)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(ResponseReadError::Io(error)),
        }
    }
}

fn ensure_before_deadline(deadline: Instant) -> std::io::Result<()> {
    if Instant::now() >= deadline {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "jwm IPC operation exceeded its deadline",
        ));
    }
    Ok(())
}

fn wait_for_io(deadline: Instant) -> std::io::Result<()> {
    ensure_before_deadline(deadline)?;
    let remaining = deadline.saturating_duration_since(Instant::now());
    thread::sleep(IO_POLL_INTERVAL.min(remaining));
    ensure_before_deadline(deadline)
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
    fn serve_once(path: &Path, reply: impl Into<String>) -> std::thread::JoinHandle<String> {
        let reply = reply.into();
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
    fn requests_and_responses_have_explicit_byte_limits() {
        let oversized = "x".repeat(MAX_IPC_REQUEST_BYTES);
        let request_error = JwmIpc::at("/definitely/missing/jwm-ipc.sock")
            .command("oversized", Value::String(oversized))
            .unwrap_err();
        assert!(matches!(
            request_error,
            JwmIpcError::RequestTooLarge {
                limit: MAX_IPC_REQUEST_BYTES
            }
        ));

        let path = scratch_socket("oversized-response");
        let server = serve_once(&path, "x".repeat(MAX_IPC_RESPONSE_BYTES + 1));
        let response_error = JwmIpc::at(&path).take_screenshot().unwrap_err();
        assert!(matches!(
            response_error,
            JwmIpcError::ResponseTooLarge {
                limit: MAX_IPC_RESPONSE_BYTES
            }
        ));
        server.join().expect("server thread");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn slow_fragmented_responses_cannot_reset_the_deadline() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        client.set_nonblocking(true).unwrap();
        let writer = std::thread::spawn(move || {
            for _ in 0..100 {
                if server.write_all(b"x").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        let started = Instant::now();
        let error =
            read_response(&mut client, started + Duration::from_millis(30), 1024).unwrap_err();

        assert!(matches!(
            error,
            ResponseReadError::Io(ref source)
                if source.kind() == std::io::ErrorKind::TimedOut
        ));
        assert!(started.elapsed() < Duration::from_millis(200));
        drop(client);
        writer.join().unwrap();
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
