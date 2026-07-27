//! Blocking client for jwm's newline-delimited JSON control socket.
//!
//! Two access patterns, deliberately on separate connections so a slow
//! subscriber can never delay a command's response:
//!
//! * [`JwmIpc::command`] — one short-lived connection per request, with
//!   bounded connect/read/write timeouts. Notifications are rare enough that
//!   the connection churn is irrelevant, and a stuck compositor cannot wedge
//!   the D-Bus service.
//! * [`subscribe`] — one long-lived connection that streams events, healing
//!   across compositor restarts with a bounded backoff.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

/// Every socket operation is bounded; jwm processes IPC on its event loop, so
/// a slow frame can delay a response but never indefinitely.
const IO_TIMEOUT: Duration = Duration::from_secs(2);
/// Wait between reconnect attempts while the compositor is down.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct JwmIpc {
    socket: PathBuf,
}

impl Default for JwmIpc {
    fn default() -> Self {
        Self::new()
    }
}

impl JwmIpc {
    #[must_use]
    pub fn new() -> Self {
        Self {
            socket: socket_path(),
        }
    }

    #[must_use]
    pub fn socket(&self) -> &std::path::Path {
        &self.socket
    }

    fn connect(&self) -> std::io::Result<UnixStream> {
        let stream = UnixStream::connect(&self.socket)?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        Ok(stream)
    }

    /// Send one command and return its `data` payload.
    ///
    /// A command jwm rejects becomes an `InvalidInput` error carrying jwm's
    /// own message, so the D-Bus caller sees the real reason.
    pub fn command(&self, name: &str, args: Value) -> std::io::Result<Value> {
        self.request(serde_json::json!({ "command": name, "args": args }))
    }

    /// Send one query and return its `data` payload.
    pub fn query(&self, name: &str) -> std::io::Result<Value> {
        self.request(serde_json::json!({ "query": name }))
    }

    fn request(&self, request: Value) -> std::io::Result<Value> {
        let mut stream = self.connect()?;
        writeln!(stream, "{request}")?;
        stream.flush()?;

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "jwm closed the socket without responding",
            ));
        }
        let response: Value = serde_json::from_str(line.trim())
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        if response
            .get("success")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Ok(response.get("data").cloned().unwrap_or(Value::Null));
        }
        let message = response
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("jwm rejected the command")
            .to_string();
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            message,
        ))
    }
}

/// Stream events for `topics` to `sink` until the process exits.
///
/// Runs on its own thread: the socket read is blocking, and reconnecting is
/// the normal path whenever the compositor restarts under a live session bus.
pub fn subscribe(ipc: JwmIpc, topics: &[&str], sink: tokio::sync::mpsc::UnboundedSender<Value>) {
    let subscribe = serde_json::json!({ "subscribe": topics });
    std::thread::spawn(move || {
        loop {
            match pump_events(&ipc, &subscribe, &sink) {
                Ok(()) => log::warn!("jwm closed the event stream; reconnecting"),
                Err(error) => log::debug!("jwm event stream unavailable: {error}"),
            }
            if sink.is_closed() {
                return;
            }
            std::thread::sleep(RECONNECT_BACKOFF);
        }
    });
}

fn pump_events(
    ipc: &JwmIpc,
    subscribe: &Value,
    sink: &tokio::sync::mpsc::UnboundedSender<Value>,
) -> std::io::Result<()> {
    let mut stream = ipc.connect()?;
    writeln!(stream, "{subscribe}")?;
    stream.flush()?;
    // Events arrive whenever they happen; a read timeout here would tear the
    // subscription down during quiet periods.
    stream.set_read_timeout(None)?;
    log::info!("subscribed to jwm events on {}", ipc.socket().display());

    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(trimmed) {
            // Responses (the subscribe acknowledgement) share the connection;
            // only event frames carry an `event` field.
            Ok(value) if value.get("event").is_some() => {
                if sink.send(value).is_err() {
                    return Ok(());
                }
            }
            Ok(_) => {}
            Err(error) => log::warn!("ignoring malformed jwm event: {error}"),
        }
    }
    Ok(())
}

/// Mirror of `jwm::ipc_server::socket_location`: an absolute `XDG_RUNTIME_DIR`
/// wins, otherwise the compositor falls back to a per-uid directory in `/tmp`.
/// `JWM_SOCKET` overrides both, which is how the nested test sessions point the
/// bridge at a private compositor.
fn socket_path() -> PathBuf {
    if let Some(explicit) = std::env::var_os("JWM_SOCKET").filter(|value| !value.is_empty()) {
        return PathBuf::from(explicit);
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute());
    match runtime {
        Some(runtime) => runtime.join("jwm-ipc.sock"),
        // SAFETY: geteuid has no preconditions and cannot fail.
        None => {
            PathBuf::from(format!("/tmp/jwm-{}", unsafe { libc::geteuid() })).join("jwm-ipc.sock")
        }
    }
}
