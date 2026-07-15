use log::{debug, info, warn};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use crate::ipc::{IpcEvent, IpcMessage, IpcResponse};

/// 单个客户端未分帧缓冲的上限(1 MiB)。正常 IPC 消息远小于此值，
/// 超限说明对端发送了无换行的巨型数据或恶意流量。
const MAX_CLIENT_BUF: usize = 1024 * 1024;
/// Per-client input work allowed in one compositor update tick.
const MAX_READ_BYTES_PER_POLL: usize = 64 * 1024;
const MAX_MESSAGES_PER_POLL: usize = 64;
/// Bound the amount of per-client state retained by the WM process.
const MAX_CLIENTS: usize = 128;
/// Do not let a connection storm monopolize one compositor update tick.
const MAX_ACCEPTS_PER_POLL: usize = 32;
const MAX_SUBSCRIPTION_TOPICS: usize = 64;
const MAX_SUBSCRIPTION_TOPIC_LEN: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RuntimeDirectorySource {
    Xdg,
    Fallback,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SocketIdentity {
    device: u64,
    inode: u64,
    owner: u32,
}

impl SocketIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            owner: metadata.uid(),
        }
    }
}

fn current_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail. Filesystem access
    // and ownership checks use the process's effective credentials.
    unsafe { libc::geteuid() }
}

fn socket_location_from(
    xdg_runtime_dir: Option<&OsStr>,
    uid: u32,
) -> (PathBuf, RuntimeDirectorySource) {
    if let Some(runtime) = xdg_runtime_dir.filter(|runtime| !runtime.is_empty()) {
        let runtime = PathBuf::from(runtime);
        if runtime.is_absolute() {
            return (runtime.join("jwm-ipc.sock"), RuntimeDirectorySource::Xdg);
        }
    }

    (
        PathBuf::from(format!("/tmp/jwm-{uid}")).join("jwm-ipc.sock"),
        RuntimeDirectorySource::Fallback,
    )
}

fn socket_location() -> (PathBuf, RuntimeDirectorySource) {
    socket_location_from(
        std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
        current_uid(),
    )
}

fn directory_error(kind: io::ErrorKind, path: &Path, message: &str) -> io::Error {
    io::Error::new(kind, format!("{}: {message}", path.display()))
}

fn validate_private_directory(path: &Path, tighten_permissions: bool) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() {
        return Err(directory_error(
            io::ErrorKind::InvalidInput,
            path,
            "IPC runtime path must be a real directory (not a symlink)",
        ));
    }
    if metadata.uid() != current_uid() {
        return Err(directory_error(
            io::ErrorKind::PermissionDenied,
            path,
            "IPC runtime directory is not owned by the current user",
        ));
    }

    let mode = metadata.mode() & 0o777;
    if mode & 0o077 != 0 {
        if !tighten_permissions {
            return Err(directory_error(
                io::ErrorKind::PermissionDenied,
                path,
                "IPC runtime directory must not be accessible by group or other users",
            ));
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        let updated = fs::symlink_metadata(path)?;
        if !updated.file_type().is_dir()
            || updated.uid() != current_uid()
            || updated.mode() & 0o077 != 0
        {
            return Err(directory_error(
                io::ErrorKind::PermissionDenied,
                path,
                "failed to secure fallback IPC runtime directory",
            ));
        }
    }
    Ok(())
}

fn prepare_socket_directory(path: &Path, source: RuntimeDirectorySource) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            directory_error(
                io::ErrorKind::InvalidInput,
                path,
                "IPC socket path has no runtime directory",
            )
        })?;

    match source {
        RuntimeDirectorySource::Xdg => {
            // XDG_RUNTIME_DIR is session-manager owned. Validate it, but never
            // create it or mutate its permissions.
            validate_private_directory(parent, false)
        }
        RuntimeDirectorySource::Fallback => {
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            match builder.create(parent) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            // The fallback directory belongs to JWM, so an older permissive
            // directory may be tightened after ownership/type validation.
            validate_private_directory(parent, true)
        }
    }
}

/// Resolve the IPC endpoint and verify (or create, for the private fallback)
/// its runtime directory. IPC clients should use this before connecting so
/// they apply exactly the same ownership and permission policy as the server.
///
/// # Errors
///
/// Returns an error if the runtime directory is missing or unsafe, or if the
/// private fallback cannot be created and secured.
pub fn validated_socket_path() -> io::Result<PathBuf> {
    let (path, source) = socket_location();
    prepare_socket_directory(&path, source)?;
    Ok(path)
}

fn socket_identity(path: &Path) -> io::Result<SocketIdentity> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket() {
        return Err(directory_error(
            io::ErrorKind::InvalidInput,
            path,
            "IPC endpoint exists but is not a Unix socket",
        ));
    }
    Ok(SocketIdentity::from_metadata(&metadata))
}

fn remove_socket_if_unchanged(path: &Path, expected: SocketIdentity) -> io::Result<bool> {
    let current = match socket_identity(path) {
        Ok(identity) => identity,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if current != expected {
        return Ok(false);
    }
    fs::remove_file(path)?;
    Ok(true)
}

fn bind_owned_socket(path: &Path) -> io::Result<(UnixListener, SocketIdentity)> {
    match socket_identity(path) {
        Ok(identity) => {
            if identity.owner != current_uid() {
                return Err(directory_error(
                    io::ErrorKind::PermissionDenied,
                    path,
                    "refusing to replace a Unix socket owned by another user",
                ));
            }

            match UnixStream::connect(path) {
                Ok(stream) => {
                    drop(stream);
                    return Err(directory_error(
                        io::ErrorKind::AddrInUse,
                        path,
                        "another JWM IPC server is already listening",
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                    // A socket inode without a listener is stale. Re-check its
                    // identity before unlinking so a concurrently replaced
                    // endpoint is never removed.
                    if !remove_socket_if_unchanged(path, identity)? {
                        return Err(directory_error(
                            io::ErrorKind::AddrInUse,
                            path,
                            "IPC endpoint changed while checking whether it was stale",
                        ));
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(io::Error::new(
                        error.kind(),
                        format!(
                            "cannot determine whether IPC endpoint {} is active: {error}",
                            path.display()
                        ),
                    ));
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    let identity = socket_identity(path)?;
    Ok((listener, identity))
}

// ---------------------------------------------------------------------------
// Client wrapper
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct IpcClient {
    stream: UnixStream,
    buf: Vec<u8>,
    /// First byte not consumed as a complete frame.
    buf_start: usize,
    /// First byte not yet inspected for a newline.
    scan_pos: usize,
    out_buf: Vec<u8>,
    subscriptions: Vec<String>,
    read_closed: bool,
}

impl IpcClient {
    fn new(stream: UnixStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream,
            buf: Vec::with_capacity(4096),
            buf_start: 0,
            scan_pos: 0,
            out_buf: Vec::new(),
            subscriptions: Vec::new(),
            read_closed: false,
        })
    }

    /// Extract up to `limit` complete frames without shifting the buffer.
    /// `scan_pos` resumes at the first uninspected byte, so fragmented frames
    /// and complete overflow frames are each scanned only once.
    fn take_complete_messages(&mut self, limit: usize) -> Vec<String> {
        if limit == 0 || self.buf_start == self.buf.len() {
            return Vec::new();
        }

        let mut messages = Vec::with_capacity(limit.min(8));
        let mut line_start = self.buf_start;
        let mut scan_pos = self.scan_pos.max(line_start);

        while scan_pos < self.buf.len() {
            if self.buf[scan_pos] != b'\n' {
                scan_pos += 1;
                continue;
            }

            let line = &self.buf[line_start..scan_pos];
            line_start = scan_pos + 1;
            scan_pos = line_start;
            if !line.is_empty() {
                messages.push(String::from_utf8_lossy(line).into_owned());
                if messages.len() == limit {
                    break;
                }
            }
        }

        self.buf_start = line_start;
        self.scan_pos = scan_pos;
        messages
    }

    /// Reclaim consumed storage geometrically. This makes prefix removal
    /// amortized O(n) even when a large batch is delivered over many ticks.
    fn compact_input_buffer(&mut self) {
        if self.buf_start == 0 {
            return;
        }
        if self.buf_start == self.buf.len() {
            self.buf.clear();
            self.buf_start = 0;
            self.scan_pos = 0;
            return;
        }
        if self.buf_start >= 64 * 1024 || self.buf_start >= self.buf.len() / 2 {
            let consumed = self.buf_start;
            self.buf.copy_within(consumed.., 0);
            self.buf.truncate(self.buf.len() - consumed);
            self.buf_start = 0;
            self.scan_pos = self.scan_pos.saturating_sub(consumed);
        }
    }

    /// Try to read available data. Returns complete newline-delimited messages.
    fn read_messages(&mut self) -> io::Result<Vec<String>> {
        // Drain already-buffered complete frames first. This is important when
        // the previous tick stopped at the message fairness limit.
        let mut messages = self.take_complete_messages(MAX_MESSAGES_PER_POLL);
        self.compact_input_buffer();
        if messages.len() == MAX_MESSAGES_PER_POLL {
            return Ok(messages);
        }

        if self.read_closed {
            return if messages.is_empty() {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "client disconnected",
                ))
            } else {
                Ok(messages)
            };
        }

        let mut tmp = [0u8; 4096];
        let mut bytes_read = 0;
        while bytes_read < MAX_READ_BYTES_PER_POLL {
            let remaining = MAX_READ_BYTES_PER_POLL - bytes_read;
            let chunk_len = remaining.min(tmp.len());
            match self.stream.read(&mut tmp[..chunk_len]) {
                Ok(0) => {
                    self.read_closed = true;
                    break;
                }
                Ok(n) => {
                    bytes_read += n;
                    self.buf.extend_from_slice(&tmp[..n]);
                    // 防止恶意/异常客户端发送无换行字节导致 buf 无界增长耗尽内存，
                    // 拖垮整个 WM。超过上限直接断开该客户端。
                    if self.buf.len() - self.buf_start > MAX_CLIENT_BUF {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "client message buffer exceeded limit",
                        ));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }

        let remaining_messages = MAX_MESSAGES_PER_POLL - messages.len();
        messages.extend(self.take_complete_messages(remaining_messages));
        self.compact_input_buffer();
        if messages.is_empty() && self.read_closed {
            Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "client disconnected",
            ))
        } else {
            Ok(messages)
        }
    }

    fn queue(&mut self, mut json: String) {
        json.push('\n');
        self.out_buf.extend_from_slice(json.as_bytes());
    }

    /// 尽量把待发字节写出。仅在致命错误(对端关闭/缓冲超限)时返回 Err；
    /// WouldBlock(对端接收缓冲暂满)会把剩余字节留待下次 flush，不视为错误,
    /// 从而不会误删健康但慢速的客户端,也不会因 write_all 半包写入而错乱 JSON 流。
    fn flush_out(&mut self) -> io::Result<()> {
        while !self.out_buf.is_empty() {
            match self.stream.write(&self.out_buf) {
                Ok(0) => {
                    return Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0"));
                }
                Ok(n) => {
                    self.out_buf.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        if self.out_buf.len() > MAX_CLIENT_BUF {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "client outbound buffer exceeded limit",
            ));
        }
        Ok(())
    }

    fn send_response(&mut self, resp: &IpcResponse) -> io::Result<()> {
        self.queue(serde_json::to_string(resp).unwrap_or_default());
        self.flush_out()
    }

    fn send_event(&mut self, event: &IpcEvent) -> io::Result<()> {
        self.queue(serde_json::to_string(event).unwrap_or_default());
        self.flush_out()
    }

    fn is_subscribed(&self, event_type: &str) -> bool {
        self.subscriptions.iter().any(|subscription| {
            subscription == "*"
                || subscription == event_type
                || event_type
                    .strip_prefix(subscription)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        })
    }
}

fn normalize_subscriptions(topics: Vec<String>) -> Vec<String> {
    let mut normalized = Vec::with_capacity(topics.len().min(MAX_SUBSCRIPTION_TOPICS));
    for topic in topics {
        let topic = topic.trim();
        if topic.is_empty()
            || topic.len() > MAX_SUBSCRIPTION_TOPIC_LEN
            || normalized.iter().any(|existing| existing == topic)
        {
            continue;
        }
        normalized.push(topic.to_string());
        if normalized.len() == MAX_SUBSCRIPTION_TOPICS {
            break;
        }
    }
    normalized
}

// ---------------------------------------------------------------------------
// IPC Server
// ---------------------------------------------------------------------------

pub struct IpcServer {
    listener: UnixListener,
    socket_path: PathBuf,
    socket_identity: Option<SocketIdentity>,
    clients: HashMap<u64, IpcClient>,
    next_id: u64,
}

/// Parsed & validated message from a client, ready to process.
pub enum IncomingIpc {
    Command {
        client_id: u64,
        name: String,
        args: serde_json::Value,
    },
    Query {
        client_id: u64,
        name: String,
        args: serde_json::Value,
    },
    Subscribe {
        client_id: u64,
        topics: Vec<String>,
    },
}

impl IpcServer {
    /// Create and bind the IPC socket.
    ///
    /// # Errors
    ///
    /// Returns an error when the runtime directory is unsafe, an active server
    /// already owns the endpoint, or the socket cannot be bound.
    pub fn new() -> io::Result<Self> {
        let path = validated_socket_path()?;
        let (listener, identity) = bind_owned_socket(&path)?;
        listener.set_nonblocking(true)?;
        info!("[ipc] listening on {}", path.display());
        Ok(Self {
            listener,
            socket_path: path,
            socket_identity: Some(identity),
            clients: HashMap::new(),
            next_id: 1,
        })
    }

    #[must_use]
    pub fn socket_path() -> PathBuf {
        socket_location().0
    }

    /// Accept any pending connections.
    pub fn accept_connections(&mut self) {
        for _ in 0..MAX_ACCEPTS_PER_POLL {
            match self.listener.accept() {
                Ok((stream, _addr)) => {
                    if self.clients.len() >= MAX_CLIENTS {
                        warn!("[ipc] rejecting client: connection limit ({MAX_CLIENTS}) reached");
                        drop(stream);
                        continue;
                    }
                    let id = self.next_id;
                    self.next_id = self.next_id.wrapping_add(1).max(1);
                    match IpcClient::new(stream) {
                        Ok(client) => {
                            debug!("[ipc] client {} connected", id);
                            self.clients.insert(id, client);
                        }
                        Err(e) => warn!("[ipc] failed to setup client: {e}"),
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    warn!("[ipc] accept error: {e}");
                    break;
                }
            }
        }
    }

    /// Read from all clients and return parsed messages.
    pub fn poll_clients(&mut self) -> Vec<IncomingIpc> {
        let mut incoming = Vec::new();
        let mut dead = Vec::new();

        for (&id, client) in self.clients.iter_mut() {
            // 先尝试把上次因 WouldBlock 滞留的出站字节冲刷出去。
            if client.flush_out().is_err() {
                dead.push(id);
                continue;
            }
            match client.read_messages() {
                Ok(lines) => {
                    for line in lines {
                        match serde_json::from_str::<IpcMessage>(&line) {
                            Ok(IpcMessage::Command(cmd)) => {
                                incoming.push(IncomingIpc::Command {
                                    client_id: id,
                                    name: cmd.command,
                                    args: cmd.args,
                                });
                            }
                            Ok(IpcMessage::Query(q)) => {
                                incoming.push(IncomingIpc::Query {
                                    client_id: id,
                                    name: q.query,
                                    args: q.args,
                                });
                            }
                            Ok(IpcMessage::Subscribe(sub)) => {
                                incoming.push(IncomingIpc::Subscribe {
                                    client_id: id,
                                    topics: sub.subscribe,
                                });
                            }
                            Err(e) => {
                                warn!("[ipc] bad message from client {id}: {e}");
                                let _ = client
                                    .send_response(&IpcResponse::err(format!("parse error: {e}")));
                            }
                        }
                    }
                }
                Err(_) => dead.push(id),
            }
        }

        for id in dead {
            debug!("[ipc] client {} disconnected", id);
            self.clients.remove(&id);
        }

        incoming
    }

    /// Send a response to a specific client.
    pub fn respond(&mut self, client_id: u64, resp: &IpcResponse) {
        if let Some(client) = self.clients.get_mut(&client_id) {
            if let Err(e) = client.send_response(resp) {
                warn!("[ipc] failed to send response to client {client_id}: {e}");
                self.clients.remove(&client_id);
            }
        }
    }

    /// Register subscriptions for a client.
    pub fn subscribe(&mut self, client_id: u64, topics: Vec<String>) {
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.subscriptions = normalize_subscriptions(topics);
        }
    }

    /// Broadcast an event to all subscribed clients.
    pub fn broadcast(&mut self, event: &IpcEvent) {
        let mut dead = Vec::new();
        for (&id, client) in self.clients.iter_mut() {
            if client.is_subscribed(&event.event) && client.send_event(event).is_err() {
                dead.push(id);
            }
        }
        for id in dead {
            self.clients.remove(&id);
        }
    }

    /// Clean shutdown: close all clients and remove the socket file.
    pub fn shutdown(&mut self) {
        self.clients.clear();
        if let Some(identity) = self.socket_identity.take() {
            match remove_socket_if_unchanged(&self.socket_path, identity) {
                Ok(true) => {}
                Ok(false) => warn!(
                    "[ipc] endpoint {} was replaced; leaving the newer socket intact",
                    self.socket_path.display()
                ),
                Err(error) => warn!(
                    "[ipc] failed to remove endpoint {} safely: {error}",
                    self.socket_path.display()
                ),
            }
        }
        info!("[ipc] server shut down");
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    use std::sync::atomic::{AtomicU64, Ordering};
    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temporary_path(label: &str) -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("jwm-ipc-{label}-{}-{id}", std::process::id()))
    }

    /// Helper: create an `IpcServer` bound to a unique temp path.
    fn make_test_server() -> IpcServer {
        let path = temporary_path("test").with_extension("sock");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let identity = socket_identity(&path).unwrap();
        IpcServer {
            listener,
            socket_path: path,
            socket_identity: Some(identity),
            clients: HashMap::new(),
            next_id: 1,
        }
    }

    #[test]
    fn socket_location_uses_only_nonempty_absolute_xdg_paths() {
        let uid = 4242;
        let fallback = PathBuf::from("/tmp/jwm-4242/jwm-ipc.sock");
        assert_eq!(socket_location_from(None, uid).0, fallback);
        assert_eq!(socket_location_from(Some(OsStr::new("")), uid).0, fallback);
        assert_eq!(
            socket_location_from(Some(OsStr::new("relative/runtime")), uid).0,
            fallback
        );

        let (path, source) = socket_location_from(Some(OsStr::new("/run/user/4242")), uid);
        assert_eq!(path, PathBuf::from("/run/user/4242/jwm-ipc.sock"));
        assert_eq!(source, RuntimeDirectorySource::Xdg);
    }

    #[test]
    fn active_socket_is_never_unlinked() {
        let path = temporary_path("active").with_extension("sock");
        let listener = UnixListener::bind(&path).unwrap();
        let original = socket_identity(&path).unwrap();

        let error = bind_owned_socket(&path).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert_eq!(socket_identity(&path).unwrap(), original);
        assert!(UnixStream::connect(&path).is_ok());

        drop(listener);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn stale_owned_socket_is_recovered() {
        let path = temporary_path("stale").with_extension("sock");
        let stale = UnixListener::bind(&path).unwrap();
        drop(stale);

        let (replacement, replacement_identity) = bind_owned_socket(&path).unwrap();

        // Filesystems may immediately reuse the stale inode number, so success
        // and connectability are the reliable recovery signals.
        assert_eq!(socket_identity(&path).unwrap(), replacement_identity);
        assert!(UnixStream::connect(&path).is_ok());
        assert_eq!(replacement_identity.owner, current_uid());
        assert_eq!(
            std::fs::symlink_metadata(&path).unwrap().mode() & 0o777,
            0o600
        );

        drop(replacement);
        assert!(remove_socket_if_unchanged(&path, replacement_identity).unwrap());
    }

    #[test]
    fn xdg_runtime_directory_permissions_are_never_mutated() {
        let directory = temporary_path("xdg-dir");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o750)).unwrap();
        let path = directory.join("jwm-ipc.sock");

        let error = prepare_socket_directory(&path, RuntimeDirectorySource::Xdg).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            std::fs::symlink_metadata(&directory).unwrap().mode() & 0o777,
            0o750
        );
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn fallback_directory_is_private_and_rejects_symlinks() {
        let directory = temporary_path("fallback-dir");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o777)).unwrap();
        let path = directory.join("jwm-ipc.sock");

        prepare_socket_directory(&path, RuntimeDirectorySource::Fallback).unwrap();
        assert_eq!(
            std::fs::symlink_metadata(&directory).unwrap().mode() & 0o777,
            0o700
        );

        let target = temporary_path("fallback-target");
        let link = temporary_path("fallback-link");
        std::fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let error =
            prepare_socket_directory(&link.join("jwm-ipc.sock"), RuntimeDirectorySource::Fallback)
                .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        std::fs::remove_file(link).unwrap();
        std::fs::remove_dir(target).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn shutdown_does_not_remove_a_replacement_endpoint() {
        let mut server = make_test_server();
        let path = server.socket_path.clone();
        std::fs::remove_file(&path).unwrap();
        let replacement = UnixListener::bind(&path).unwrap();
        let replacement_identity = socket_identity(&path).unwrap();

        server.shutdown();

        assert_eq!(socket_identity(&path).unwrap(), replacement_identity);
        drop(replacement);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn batched_messages_are_limited_and_resumed_without_loss() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        let mut client = IpcClient::new(reader).unwrap();
        let total = MAX_MESSAGES_PER_POLL + 7;
        let mut payload = String::new();
        for index in 0..total {
            std::fmt::Write::write_fmt(&mut payload, format_args!("message-{index}\n")).unwrap();
        }
        writer.write_all(payload.as_bytes()).unwrap();

        let first = client.read_messages().unwrap();
        let second = client.read_messages().unwrap();

        assert_eq!(first.len(), MAX_MESSAGES_PER_POLL);
        assert_eq!(first.first().unwrap(), "message-0");
        assert_eq!(
            first.last().unwrap(),
            &format!("message-{}", MAX_MESSAGES_PER_POLL - 1)
        );
        assert_eq!(second.len(), 7);
        assert_eq!(
            second.first().unwrap(),
            &format!("message-{MAX_MESSAGES_PER_POLL}")
        );
        assert_eq!(second.last().unwrap(), &format!("message-{}", total - 1));
    }

    #[test]
    fn per_poll_read_bytes_are_bounded() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        let mut client = IpcClient::new(reader).unwrap();
        let payload = vec![b'x'; MAX_READ_BYTES_PER_POLL + 17];
        writer.write_all(&payload).unwrap();

        assert!(client.read_messages().unwrap().is_empty());
        assert_eq!(client.buf.len(), MAX_READ_BYTES_PER_POLL);

        assert!(client.read_messages().unwrap().is_empty());
        assert_eq!(client.buf.len(), payload.len());
    }

    #[test]
    fn complete_final_frame_is_delivered_before_disconnect() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        let mut client = IpcClient::new(reader).unwrap();
        writer.write_all(b"final-message\n").unwrap();
        drop(writer);

        assert_eq!(client.read_messages().unwrap(), ["final-message"]);
        assert_eq!(
            client.read_messages().unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn subscription_topics_are_trimmed_deduplicated_and_bounded() {
        let mut topics = vec![
            " window ".to_string(),
            "window".to_string(),
            String::new(),
            "x".repeat(MAX_SUBSCRIPTION_TOPIC_LEN + 1),
        ];
        topics.extend((0..MAX_SUBSCRIPTION_TOPICS + 10).map(|index| format!("topic-{index}")));

        let normalized = normalize_subscriptions(topics);

        assert_eq!(normalized.len(), MAX_SUBSCRIPTION_TOPICS);
        assert_eq!(normalized[0], "window");
        assert_eq!(
            normalized.iter().filter(|topic| *topic == "window").count(),
            1
        );
        assert!(normalized.iter().all(|topic| !topic.trim().is_empty()));
        assert!(
            normalized
                .iter()
                .all(|topic| topic.len() <= MAX_SUBSCRIPTION_TOPIC_LEN)
        );
    }

    #[test]
    fn subscription_prefix_matching_respects_topic_boundaries() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let mut client = IpcClient::new(stream).unwrap();
        client.subscriptions = normalize_subscriptions(vec![" window ".into()]);

        assert!(client.is_subscribed("window"));
        assert!(client.is_subscribed("window/new"));
        assert!(!client.is_subscribed("windowing/new"));
        assert!(!client.is_subscribed("monitor/new"));

        client.subscriptions = normalize_subscriptions(vec!["*".into()]);
        assert!(client.is_subscribed("monitor/new"));
    }

    #[test]
    fn accept_and_poll_command() {
        let mut server = make_test_server();
        let path = server.socket_path.clone();

        // Connect a client and send a command
        let mut client = UnixStream::connect(&path).unwrap();
        client
            .write_all(b"{\"command\":\"killclient\",\"args\":null}\n")
            .unwrap();

        // Give the OS a moment
        std::thread::sleep(std::time::Duration::from_millis(20));

        server.accept_connections();
        let msgs = server.poll_clients();
        assert_eq!(msgs.len(), 1);
        match &msgs[0] {
            IncomingIpc::Command { name, .. } => assert_eq!(name, "killclient"),
            _ => panic!("expected Command"),
        }
    }

    #[test]
    fn respond_to_client() {
        let mut server = make_test_server();
        let path = server.socket_path.clone();

        let mut client = UnixStream::connect(&path).unwrap();
        client.set_nonblocking(false).unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        client.write_all(b"{\"query\":\"get_version\"}\n").unwrap();

        std::thread::sleep(std::time::Duration::from_millis(20));

        server.accept_connections();
        let msgs = server.poll_clients();
        assert_eq!(msgs.len(), 1);

        // Respond
        match &msgs[0] {
            IncomingIpc::Query { client_id, .. } => {
                let resp = crate::ipc::IpcResponse::ok(Some(serde_json::json!({"v": "0.2"})));
                server.respond(*client_id, &resp);
            }
            _ => panic!("expected Query"),
        }

        // Client reads the response
        let mut buf = [0u8; 1024];
        let n = std::io::Read::read(&mut client, &mut buf).unwrap();
        let line = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(line.contains("\"success\":true"));
    }

    #[test]
    fn broadcast_to_subscriber() {
        let mut server = make_test_server();
        let path = server.socket_path.clone();

        let mut c1 = UnixStream::connect(&path).unwrap();
        c1.set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        c1.write_all(b"{\"subscribe\":[\"window\"]}\n").unwrap();

        let mut c2 = UnixStream::connect(&path).unwrap();
        c2.set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        // c2 doesn't subscribe

        std::thread::sleep(std::time::Duration::from_millis(20));

        server.accept_connections();
        let msgs = server.poll_clients();

        // Process subscribe
        for msg in &msgs {
            if let IncomingIpc::Subscribe { client_id, topics } = msg {
                server.subscribe(*client_id, topics.clone());
                server.respond(*client_id, &crate::ipc::IpcResponse::ok(None));
            }
        }

        // Read the subscribe confirmation from c1
        let mut buf = [0u8; 1024];
        let _ = std::io::Read::read(&mut c1, &mut buf).unwrap();

        // Broadcast
        let event = crate::ipc::IpcEvent {
            event: "window/new".to_string(),
            payload: serde_json::json!({"id": 42}),
        };
        server.broadcast(&event);

        // c1 should receive the event
        let mut buf = [0u8; 1024];
        let n = std::io::Read::read(&mut c1, &mut buf).unwrap();
        let line = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(line.contains("window/new"));

        // c2 should NOT receive (no subscription, and read would block/timeout)
        c2.set_read_timeout(Some(std::time::Duration::from_millis(100)))
            .unwrap();
        let result = std::io::Read::read(&mut c2, &mut [0u8; 1024]);
        assert!(result.is_err() || result.unwrap() == 0);
    }

    #[test]
    fn disconnected_client_is_cleaned() {
        let mut server = make_test_server();
        let path = server.socket_path.clone();

        let client = UnixStream::connect(&path).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        server.accept_connections();
        assert_eq!(server.clients.len(), 1);

        // Drop the client (disconnect)
        drop(client);
        std::thread::sleep(std::time::Duration::from_millis(20));

        // Polling should detect the disconnect
        let _ = server.poll_clients();
        assert_eq!(server.clients.len(), 0);
    }
}
