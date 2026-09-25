use log::{debug, info, warn};
use nix::errno::Errno;
use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags};
use nix::sys::eventfd::{EfdFlags, EventFd};
use std::collections::{HashMap, VecDeque};
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
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
/// Bound aggregate IPC input work in one compositor update tick.
const MAX_TOTAL_READ_BYTES_PER_POLL: usize = 256 * 1024;
const MAX_TOTAL_MESSAGES_PER_POLL: usize = 256;
/// Bound the amount of per-client state retained by the WM process.
const MAX_CLIENTS: usize = 128;
/// Do not let a connection storm monopolize one compositor update tick.
const MAX_ACCEPTS_PER_POLL: usize = 32;
const MAX_SUBSCRIPTION_TOPICS: usize = 64;
const MAX_SUBSCRIPTION_TOPIC_LEN: usize = 128;
/// The most dropped topics one subscribe acknowledgement names. A client
/// that sent thousands of junk topics learns the count, not the whole list
/// echoed back.
const MAX_REPORTED_DROPPED_TOPICS: usize = 16;

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
    /// First outbound byte not yet written to the client.
    out_start: usize,
    subscriptions: Vec<String>,
    read_closed: bool,
    writable_interest: bool,
    readable_interest: bool,
    /// Valid frames handed to the caller by `poll_clients`.
    delivered_frames: u64,
    /// Replies sent through `IpcServer::respond`, capped at `delivered_frames`.
    answered_frames: u64,
    /// Parse errors for frames that followed still-unanswered valid frames.
    /// The protocol has no correlation id, so clients pair replies with
    /// requests by order; each entry is held until `answered_frames` reaches
    /// the recorded value, i.e. until every earlier frame has its reply.
    deferred_errors: VecDeque<(u64, String)>,
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
            out_start: 0,
            subscriptions: Vec::new(),
            read_closed: false,
            writable_interest: false,
            readable_interest: true,
            delivered_frames: 0,
            answered_frames: 0,
            deferred_errors: VecDeque::new(),
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

    /// Try to read available data. Returns complete newline-delimited messages
    /// together with the number of bytes read from the socket. The caller's
    /// limits are clamped to the existing per-client limits.
    fn read_messages(
        &mut self,
        message_budget: usize,
        read_byte_budget: usize,
    ) -> (io::Result<Vec<String>>, usize) {
        let message_limit = message_budget.min(MAX_MESSAGES_PER_POLL);
        let read_byte_limit = read_byte_budget.min(MAX_READ_BYTES_PER_POLL);
        if message_limit == 0 {
            return (Ok(Vec::new()), 0);
        }

        // Drain already-buffered complete frames first. This is important when
        // the previous tick stopped at the message fairness limit.
        let mut messages = self.take_complete_messages(message_limit);
        self.compact_input_buffer();
        if messages.len() == message_limit {
            return (Ok(messages), 0);
        }

        if self.read_closed {
            self.take_unterminated_final_frame(message_limit, &mut messages);
            let result = if messages.is_empty() {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "client disconnected",
                ))
            } else {
                Ok(messages)
            };
            return (result, 0);
        }

        let mut tmp = [0u8; 4096];
        let mut bytes_read = 0;
        while bytes_read < read_byte_limit {
            let remaining = read_byte_limit - bytes_read;
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
                        return (
                            Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "client message buffer exceeded limit",
                            )),
                            bytes_read,
                        );
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return (Err(e), bytes_read),
            }
        }

        let remaining_messages = message_limit - messages.len();
        messages.extend(self.take_complete_messages(remaining_messages));
        self.compact_input_buffer();
        self.take_unterminated_final_frame(message_limit, &mut messages);
        let result = if messages.is_empty() && self.read_closed {
            Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "client disconnected",
            ))
        } else {
            Ok(messages)
        };
        (result, bytes_read)
    }

    /// After end-of-stream the bytes behind the last newline can never be
    /// completed, so they are the peer's last frame, like the final line of a
    /// file without a trailing newline. Delivering it gets the peer a reply
    /// (a parse error if it was truncated) instead of dropping it silently.
    fn take_unterminated_final_frame(&mut self, limit: usize, messages: &mut Vec<String>) {
        if !self.read_closed || messages.len() >= limit || self.buf_start == self.buf.len() {
            return;
        }
        let tail = &self.buf[self.buf_start..];
        // `take_complete_messages` stops early only at its limit, so with
        // budget left every complete frame has already been consumed.
        debug_assert!(!tail.contains(&b'\n'));
        if !tail.trim_ascii().is_empty() {
            messages.push(String::from_utf8_lossy(tail).into_owned());
        }
        self.buf_start = self.buf.len();
        self.scan_pos = self.buf.len();
        self.compact_input_buffer();
    }

    fn queue(&mut self, mut json: String) {
        compact_output_buffer(&mut self.out_buf, &mut self.out_start);
        json.push('\n');
        self.out_buf.extend_from_slice(json.as_bytes());
    }

    fn has_pending_output(&self) -> bool {
        self.out_start < self.out_buf.len()
    }

    fn pending_output_len(&self) -> usize {
        self.out_buf.len().saturating_sub(self.out_start)
    }

    /// 尽量把待发字节写出。仅在致命错误(对端关闭/缓冲超限)时返回 Err；
    /// WouldBlock(对端接收缓冲暂满)会把剩余字节留待下次 flush，不视为错误,
    /// 从而不会误删健康但慢速的客户端,也不会因 write_all 半包写入而错乱 JSON 流。
    fn flush_out(&mut self) -> io::Result<()> {
        flush_output_buffer(&mut self.stream, &mut self.out_buf, &mut self.out_start)?;
        if self.pending_output_len() > MAX_CLIENT_BUF {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "client outbound buffer exceeded limit",
            ));
        }
        Ok(())
    }

    /// Send the caller's reply to the oldest unanswered frame, followed by any
    /// parse errors that were waiting for it.
    fn send_response(&mut self, resp: &IpcResponse) -> io::Result<()> {
        self.answered_frames = (self.answered_frames + 1).min(self.delivered_frames);
        self.queue(serde_json::to_string(resp).unwrap_or_default());
        self.queue_due_parse_errors();
        self.flush_out()
    }

    /// Reply to a frame that never reached the caller. The error goes out at
    /// once only when no earlier frame still awaits its reply; otherwise it is
    /// held so the reply stream stays in frame order.
    fn reject_frame(&mut self, error: String) -> io::Result<()> {
        if self.answered_frames == self.delivered_frames && self.deferred_errors.is_empty() {
            self.queue(serde_json::to_string(&IpcResponse::err(error)).unwrap_or_default());
            return self.flush_out();
        }
        self.deferred_errors
            .push_back((self.delivered_frames, error));
        Ok(())
    }

    fn queue_due_parse_errors(&mut self) {
        while self
            .deferred_errors
            .front()
            .is_some_and(|(due, _)| *due <= self.answered_frames)
        {
            if let Some((_, error)) = self.deferred_errors.pop_front() {
                self.queue(serde_json::to_string(&IpcResponse::err(error)).unwrap_or_default());
            }
        }
    }

    /// `poll_clients` callers answer every returned frame before polling
    /// again. Frames still unanswered at the next poll were abandoned, so the
    /// parse errors queued behind them are released rather than held forever.
    fn release_abandoned_replies(&mut self) {
        if self.deferred_errors.is_empty() {
            return;
        }
        self.answered_frames = self.delivered_frames;
        self.queue_due_parse_errors();
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

    fn has_buffered_frame(&self) -> bool {
        // After end-of-stream an unterminated tail is a final frame as well.
        (self.read_closed && self.buf_start < self.buf.len())
            || self.buf[self.scan_pos.max(self.buf_start)..].contains(&b'\n')
    }
}

/// Write the pending suffix without shifting it after every partial write.
fn flush_output_buffer<W: Write>(
    writer: &mut W,
    buf: &mut Vec<u8>,
    start: &mut usize,
) -> io::Result<()> {
    while *start < buf.len() {
        match writer.write(&buf[*start..]) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0")),
            Ok(written) => *start += written,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    compact_output_buffer(buf, start);
    Ok(())
}

/// Reclaim a consumed prefix only after a meaningful fraction or fixed chunk
/// has accumulated. Partial writes therefore advance a cursor in O(1), while
/// retained consumed storage stays below one compaction threshold.
fn compact_output_buffer(buf: &mut Vec<u8>, start: &mut usize) {
    if *start == 0 {
        return;
    }
    if *start == buf.len() {
        buf.clear();
        *start = 0;
        return;
    }
    if *start >= 64 * 1024 || *start >= buf.len() / 2 {
        let consumed = *start;
        buf.copy_within(consumed.., 0);
        buf.truncate(buf.len() - consumed);
        *start = 0;
    }
}

/// Why a requested subscription topic was not stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DroppedTopicReason {
    /// Nothing but whitespace.
    Empty,
    /// Longer than the per-topic bound once trimmed.
    TooLong,
    /// The same topic (after trimming) was already stored.
    Duplicate,
    /// The client already holds the most topics one subscription may.
    Limit,
}

impl DroppedTopicReason {
    /// The wire name used in the subscribe acknowledgement.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooLong => "too_long",
            Self::Duplicate => "duplicate",
            Self::Limit => "limit",
        }
    }
}

/// A requested topic the server did not store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DroppedTopic {
    /// The topic as requested, trimmed — and cut to the per-topic bound, so
    /// an oversized one is never echoed back in full.
    pub topic: String,
    pub reason: DroppedTopicReason,
}

/// What a subscribe request actually registered.
///
/// The bounds on subscriptions are the server's own, so before this a client
/// whose 65th topic (or 200-byte topic) was dropped had no way to learn it:
/// the subscription simply never delivered those events.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SubscriptionOutcome {
    /// The normalized topics stored for the client, in request order.
    pub subscribed: Vec<String>,
    /// The first [`MAX_REPORTED_DROPPED_TOPICS`] topics that were not stored.
    pub dropped: Vec<DroppedTopic>,
    /// How many requested topics were not stored in all.
    pub dropped_total: usize,
}

impl SubscriptionOutcome {
    fn drop_topic(&mut self, topic: &str, reason: DroppedTopicReason) {
        self.dropped_total += 1;
        if self.dropped.len() < MAX_REPORTED_DROPPED_TOPICS {
            self.dropped.push(DroppedTopic {
                topic: utf8_prefix(topic, MAX_SUBSCRIPTION_TOPIC_LEN).to_string(),
                reason,
            });
        }
    }
}

/// The longest prefix of `text` that fits in `max_bytes` without splitting a
/// character.
fn utf8_prefix(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn normalize_subscriptions(topics: Vec<String>) -> SubscriptionOutcome {
    let mut outcome = SubscriptionOutcome {
        subscribed: Vec::with_capacity(topics.len().min(MAX_SUBSCRIPTION_TOPICS)),
        ..SubscriptionOutcome::default()
    };
    for topic in &topics {
        let topic = topic.trim();
        let reason = if topic.is_empty() {
            DroppedTopicReason::Empty
        } else if topic.len() > MAX_SUBSCRIPTION_TOPIC_LEN {
            DroppedTopicReason::TooLong
        } else if outcome.subscribed.iter().any(|existing| existing == topic) {
            DroppedTopicReason::Duplicate
        } else if outcome.subscribed.len() == MAX_SUBSCRIPTION_TOPICS {
            DroppedTopicReason::Limit
        } else {
            outcome.subscribed.push(topic.to_string());
            continue;
        };
        outcome.drop_topic(topic, reason);
    }
    outcome
}

// ---------------------------------------------------------------------------
// IPC Server
// ---------------------------------------------------------------------------

const IPC_READINESS_CAPACITY: usize = MAX_CLIENTS + 2;

/// Stable, level-triggered readiness descriptor for the listener and every
/// dynamic client. The outer compositor loop registers a duplicate of the
/// epoll fd, while this owner updates interests as clients come and go.
#[derive(Debug)]
struct IpcReadiness {
    epoll: Epoll,
    continuation: EventFd,
    continuation_armed: bool,
    events: Vec<EpollEvent>,
}

impl IpcReadiness {
    fn new(listener: &UnixListener) -> io::Result<Self> {
        let epoll = Epoll::new(EpollCreateFlags::EPOLL_CLOEXEC).map_err(errno_io)?;
        let continuation = EventFd::from_flags(EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK)
            .map_err(errno_io)?;
        epoll
            .add(listener, EpollEvent::new(EpollFlags::EPOLLIN, u64::MAX - 1))
            .map_err(errno_io)?;
        epoll
            .add(
                &continuation,
                EpollEvent::new(EpollFlags::EPOLLIN, u64::MAX),
            )
            .map_err(errno_io)?;
        Ok(Self {
            epoll,
            continuation,
            continuation_armed: false,
            events: vec![EpollEvent::empty(); IPC_READINESS_CAPACITY],
        })
    }

    fn duplicate_fd(&self) -> io::Result<OwnedFd> {
        self.epoll.0.try_clone()
    }

    fn client_flags(readable: bool, writable: bool) -> EpollFlags {
        let mut flags = EpollFlags::EPOLLHUP | EpollFlags::EPOLLERR;
        if readable {
            flags |= EpollFlags::EPOLLIN | EpollFlags::EPOLLRDHUP;
        }
        if writable {
            flags |= EpollFlags::EPOLLOUT;
        }
        flags
    }

    fn add_client(&self, id: u64, client: &IpcClient) -> io::Result<()> {
        self.epoll
            .add(
                &client.stream,
                EpollEvent::new(Self::client_flags(true, false), id),
            )
            .map_err(errno_io)
    }

    fn sync_client_interest(&self, id: u64, client: &mut IpcClient) -> io::Result<()> {
        let writable = client.has_pending_output();
        // A peer that shut down its write side stays read-ready forever. While
        // its last replies drain, only writability or a hangup can make
        // progress, so listening for that end-of-file would spin the loop.
        // Read interest returns once the output is gone, and the next tick
        // then observes the end-of-file and retires the client.
        let readable = !(client.read_closed && writable);
        if writable == client.writable_interest && readable == client.readable_interest {
            return Ok(());
        }
        let mut event = EpollEvent::new(Self::client_flags(readable, writable), id);
        self.epoll
            .modify(&client.stream, &mut event)
            .map_err(errno_io)?;
        client.writable_interest = writable;
        client.readable_interest = readable;
        Ok(())
    }

    /// Consume the inner epoll ready list and the userspace-continuation
    /// eventfd. Socket I/O immediately afterwards clears level readiness.
    fn drain(&mut self) -> io::Result<()> {
        self.epoll.wait(&mut self.events, 0u8).map_err(errno_io)?;
        match self.continuation.read() {
            Ok(_) | Err(Errno::EAGAIN) => {}
            Err(error) => return Err(errno_io(error)),
        }
        self.continuation_armed = false;
        Ok(())
    }

    /// Re-publish work that is already buffered in userspace. After a
    /// fairness budget is reached the socket itself may no longer be readable,
    /// so kernel readiness alone cannot schedule the continuation.
    fn arm_continuation(&mut self) -> io::Result<()> {
        if self.continuation_armed {
            return Ok(());
        }
        self.continuation.write(1).map_err(errno_io)?;
        self.continuation_armed = true;
        Ok(())
    }
}

fn errno_io(error: Errno) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}

pub struct IpcServer {
    listener: UnixListener,
    socket_path: PathBuf,
    socket_identity: Option<SocketIdentity>,
    clients: HashMap<u64, IpcClient>,
    next_id: u64,
    /// Client id at which the next poll should begin. Client ids are sorted
    /// before polling so fairness does not depend on `HashMap` iteration order.
    next_poll_client: u64,
    readiness: Option<IpcReadiness>,
    /// Once any readiness operation fails, polling remains the only complete
    /// delivery guarantee for the rest of this server's lifetime.
    readiness_healthy: bool,
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
        let readiness = match IpcReadiness::new(&listener) {
            Ok(readiness) => Some(readiness),
            Err(error) => {
                warn!("[ipc] readiness hub unavailable, retaining timer fallback: {error}");
                None
            }
        };
        info!("[ipc] listening on {}", path.display());
        let readiness_healthy = readiness.is_some();
        Ok(Self {
            listener,
            socket_path: path,
            socket_identity: Some(identity),
            clients: HashMap::new(),
            next_id: 1,
            next_poll_client: 1,
            readiness,
            readiness_healthy,
        })
    }

    #[must_use]
    pub fn socket_path() -> PathBuf {
        socket_location().0
    }

    /// Duplicate the stable readiness descriptor for an owning event source.
    /// The duplicate is close-on-exec and remains valid while client
    /// registrations are changed on the original epoll instance.
    pub fn duplicate_readiness_fd(&self) -> io::Result<Option<OwnedFd>> {
        self.readiness
            .as_ref()
            .map(IpcReadiness::duplicate_fd)
            .transpose()
    }

    /// Borrow the stable inner epoll descriptor for a process-level readiness
    /// aggregator. Its lifetime remains owned by this server.
    pub(crate) fn readiness_fd(&self) -> Option<BorrowedFd<'_>> {
        self.readiness
            .as_ref()
            .map(|readiness| readiness.epoll.0.as_fd())
    }

    pub(crate) fn readiness_is_healthy(&self) -> bool {
        self.readiness_healthy
    }

    fn drain_readiness(&mut self) {
        if let Some(readiness) = self.readiness.as_mut()
            && let Err(error) = readiness.drain()
        {
            self.readiness_healthy = false;
            warn!("[ipc] could not drain readiness hub: {error}");
        }
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
                            if let Some(readiness) = self.readiness.as_ref()
                                && let Err(error) = readiness.add_client(id, &client)
                            {
                                self.readiness_healthy = false;
                                warn!(
                                    "[ipc] client {id} readiness registration failed; retaining timer fallback: {error}"
                                );
                            }
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
    ///
    /// Callers answer every returned message with exactly one [`respond`]
    /// call, in the returned order, before polling again: parse errors for
    /// later frames of the same client wait for those replies so that each
    /// client receives its replies in the order it sent its frames.
    ///
    /// [`respond`]: Self::respond
    pub fn poll_clients(&mut self) -> Vec<IncomingIpc> {
        self.drain_readiness();
        let mut incoming = Vec::new();
        let mut dead = Vec::new();
        let mut client_ids: Vec<_> = self.clients.keys().copied().collect();
        client_ids.sort_unstable();
        if client_ids.is_empty() {
            return incoming;
        }

        let start = client_ids.partition_point(|id| *id < self.next_poll_client);
        let start = if start == client_ids.len() { 0 } else { start };
        client_ids.rotate_left(start);

        let mut messages_seen = 0;
        let mut bytes_read = 0;

        for (index, &id) in client_ids.iter().enumerate() {
            if messages_seen == MAX_TOTAL_MESSAGES_PER_POLL
                || bytes_read == MAX_TOTAL_READ_BYTES_PER_POLL
            {
                break;
            }

            // Advance before doing I/O so a disconnect or parse failure cannot
            // pin the round-robin cursor to this client.
            self.next_poll_client = client_ids[(index + 1) % client_ids.len()];
            let Some(client) = self.clients.get_mut(&id) else {
                continue;
            };

            client.release_abandoned_replies();
            // 先尝试把上次因 WouldBlock 滞留的出站字节冲刷出去。
            if client.flush_out().is_err() {
                dead.push(id);
                continue;
            }
            let remaining_messages = MAX_TOTAL_MESSAGES_PER_POLL - messages_seen;
            let remaining_bytes = MAX_TOTAL_READ_BYTES_PER_POLL - bytes_read;
            let (result, client_bytes_read) =
                client.read_messages(remaining_messages, remaining_bytes);
            bytes_read += client_bytes_read;
            debug_assert!(bytes_read <= MAX_TOTAL_READ_BYTES_PER_POLL);

            match result {
                Ok(lines) => {
                    // Count every non-empty frame, including invalid JSON, so
                    // malformed input cannot bypass the global work budget.
                    messages_seen += lines.len();
                    debug_assert!(messages_seen <= MAX_TOTAL_MESSAGES_PER_POLL);
                    for line in lines {
                        let message = match serde_json::from_str::<IpcMessage>(&line) {
                            Ok(IpcMessage::Command(cmd)) => IncomingIpc::Command {
                                client_id: id,
                                name: cmd.command,
                                args: cmd.args,
                            },
                            Ok(IpcMessage::Query(q)) => IncomingIpc::Query {
                                client_id: id,
                                name: q.query,
                                args: q.args,
                            },
                            Ok(IpcMessage::Subscribe(sub)) => IncomingIpc::Subscribe {
                                client_id: id,
                                topics: sub.subscribe,
                            },
                            Err(e) => {
                                warn!("[ipc] bad message from client {id}: {e}");
                                let _ = client.reject_frame(format!("parse error: {e}"));
                                continue;
                            }
                        };
                        client.delivered_frames += 1;
                        incoming.push(message);
                    }
                }
                // End-of-stream only ends the requests. Replies still queued
                // for the peer are kept until they drain, a write fails, or
                // the peer hangs up; only then is the client retired.
                Err(error)
                    if error.kind() == io::ErrorKind::UnexpectedEof
                        && client.read_closed
                        && client.has_pending_output() => {}
                Err(_) => dead.push(id),
            }

            if let Some(readiness) = self.readiness.as_ref()
                && let Err(error) = readiness.sync_client_interest(id, client)
            {
                self.readiness_healthy = false;
                warn!("[ipc] client {id} readiness update failed: {error}");
            }
        }

        for id in dead {
            debug!("[ipc] client {} disconnected", id);
            self.clients.remove(&id);
        }

        if self.clients.values().any(IpcClient::has_buffered_frame)
            && let Some(readiness) = self.readiness.as_mut()
            && let Err(error) = readiness.arm_continuation()
        {
            self.readiness_healthy = false;
            warn!("[ipc] could not arm buffered-work continuation: {error}");
        }

        incoming
    }

    /// Send a response to a specific client.
    pub fn respond(&mut self, client_id: u64, resp: &IpcResponse) {
        let mut remove = false;
        if let Some(client) = self.clients.get_mut(&client_id) {
            if let Err(e) = client.send_response(resp) {
                warn!("[ipc] failed to send response to client {client_id}: {e}");
                remove = true;
            } else if let Some(readiness) = self.readiness.as_ref()
                && let Err(error) = readiness.sync_client_interest(client_id, client)
            {
                self.readiness_healthy = false;
                warn!("[ipc] client {client_id} readiness update failed: {error}");
            }
        }
        if remove {
            self.clients.remove(&client_id);
        }
    }

    /// Register subscriptions for a client, replacing any it held, and say
    /// what was stored and what was dropped (and why) so the caller can tell
    /// the client.
    pub fn subscribe(&mut self, client_id: u64, topics: Vec<String>) -> SubscriptionOutcome {
        let outcome = normalize_subscriptions(topics);
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.subscriptions = outcome.subscribed.clone();
        }
        outcome
    }

    /// Broadcast an event to all subscribed clients.
    pub fn broadcast(&mut self, event: &IpcEvent) {
        let mut dead = Vec::new();
        for (&id, client) in self.clients.iter_mut() {
            if client.is_subscribed(&event.event) {
                if client.send_event(event).is_err() {
                    dead.push(id);
                    continue;
                }
                if let Some(readiness) = self.readiness.as_ref()
                    && let Err(error) = readiness.sync_client_interest(id, client)
                {
                    self.readiness_healthy = false;
                    warn!("[ipc] client {id} readiness update failed: {error}");
                }
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
    use std::io::{Read, Write};
    use std::os::fd::{AsFd, AsRawFd};

    use nix::poll::{PollFd, PollFlags, poll};
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
        let readiness = Some(IpcReadiness::new(&listener).unwrap());
        IpcServer {
            listener,
            socket_path: path,
            socket_identity: Some(identity),
            clients: HashMap::new(),
            next_id: 1,
            next_poll_client: 1,
            readiness,
            readiness_healthy: true,
        }
    }

    fn attach_test_client(server: &mut IpcServer, id: u64) -> UnixStream {
        let (server_stream, peer) = UnixStream::pair().unwrap();
        let client = IpcClient::new(server_stream).unwrap();
        server
            .readiness
            .as_ref()
            .unwrap()
            .add_client(id, &client)
            .unwrap();
        server.clients.insert(id, client);
        peer
    }

    fn constrain_send_buffer(stream: &UnixStream) {
        let send_buffer_bytes: libc::c_int = 4096;
        // SAFETY: the pointer references a live integer of the supplied size,
        // and `stream` owns a valid Unix stream descriptor.
        let result = unsafe {
            libc::setsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                std::ptr::from_ref(&send_buffer_bytes).cast(),
                std::mem::size_of_val(&send_buffer_bytes) as libc::socklen_t,
            )
        };
        assert_eq!(result, 0, "failed to constrain the socket send buffer");
    }

    fn query_payload(count: usize) -> Vec<u8> {
        "{\"query\":\"get_version\"}\n".repeat(count).into_bytes()
    }

    fn fd_is_readable(fd: &OwnedFd) -> bool {
        let mut descriptors = [PollFd::new(fd.as_fd(), PollFlags::POLLIN)];
        poll(&mut descriptors, 0u8).unwrap() > 0
            && descriptors[0]
                .revents()
                .is_some_and(|events| events.contains(PollFlags::POLLIN))
    }

    #[test]
    fn readiness_duplicate_nests_in_an_outer_epoll_and_is_close_on_exec() {
        use nix::fcntl::{FcntlArg, FdFlag, fcntl};

        let mut server = make_test_server();
        let readiness = server.duplicate_readiness_fd().unwrap().unwrap();
        let descriptor_flags =
            FdFlag::from_bits_truncate(fcntl(&readiness, FcntlArg::F_GETFD).unwrap());
        assert!(descriptor_flags.contains(FdFlag::FD_CLOEXEC));

        let outer = Epoll::new(EpollCreateFlags::EPOLL_CLOEXEC).unwrap();
        outer
            .add(&readiness, EpollEvent::new(EpollFlags::EPOLLIN, 42))
            .unwrap();
        let mut client = UnixStream::connect(server.socket_path.clone()).unwrap();
        client.write_all(b"{\"query\":\"get_version\"}\n").unwrap();

        let mut events = [EpollEvent::empty()];
        assert_eq!(outer.wait(&mut events, 100u8).unwrap(), 1);
        assert_eq!(events[0].data(), 42);
        server.accept_connections();
        assert_eq!(server.poll_clients().len(), 1);
        assert_eq!(outer.wait(&mut events, 0u8).unwrap(), 0);
    }

    #[test]
    fn disconnected_idle_client_wakes_once_and_is_retired() {
        let mut server = make_test_server();
        let readiness = server.duplicate_readiness_fd().unwrap().unwrap();
        let peer = attach_test_client(&mut server, 11);
        assert!(!fd_is_readable(&readiness));

        drop(peer);
        assert!(fd_is_readable(&readiness));
        assert!(server.poll_clients().is_empty());
        assert!(!server.clients.contains_key(&11));
        assert!(!fd_is_readable(&readiness));
    }

    #[test]
    fn listener_and_persistent_client_drive_one_stable_readiness_fd() {
        let mut server = make_test_server();
        let readiness = server.duplicate_readiness_fd().unwrap().unwrap();
        let path = server.socket_path.clone();
        assert!(!fd_is_readable(&readiness));

        let mut client = UnixStream::connect(path).unwrap();
        assert!(fd_is_readable(&readiness));
        server.accept_connections();
        server.poll_clients();
        assert_eq!(server.clients.len(), 1);
        assert!(!fd_is_readable(&readiness));

        client.write_all(b"{\"query\":\"get_version\"}\n").unwrap();
        assert!(fd_is_readable(&readiness));
        let first = server.poll_clients();
        assert_eq!(first.len(), 1);
        assert!(!fd_is_readable(&readiness));

        client.write_all(b"{\"query\":\"get_tree\"}\n").unwrap();
        assert!(fd_is_readable(&readiness));
        let second = server.poll_clients();
        assert_eq!(second.len(), 1);
        assert!(!fd_is_readable(&readiness));
    }

    #[test]
    fn buffered_frames_rearm_readiness_after_the_socket_is_drained() {
        let mut server = make_test_server();
        let readiness = server.duplicate_readiness_fd().unwrap().unwrap();
        let mut peer = attach_test_client(&mut server, 7);
        peer.write_all(&query_payload(MAX_MESSAGES_PER_POLL + 7))
            .unwrap();
        assert!(fd_is_readable(&readiness));

        let first = server.poll_clients();
        assert_eq!(first.len(), MAX_MESSAGES_PER_POLL);
        assert!(server.clients[&7].has_buffered_frame());
        assert!(
            fd_is_readable(&readiness),
            "the continuation eventfd must publish userspace-only work"
        );

        let second = server.poll_clients();
        assert_eq!(second.len(), 7);
        assert!(!server.clients[&7].has_buffered_frame());
        assert!(!fd_is_readable(&readiness));
    }

    #[test]
    fn writable_interest_is_removed_after_the_output_queue_drains() {
        let mut server = make_test_server();
        let readiness_fd = server.duplicate_readiness_fd().unwrap().unwrap();
        let mut peer = attach_test_client(&mut server, 9);
        let readiness = server.readiness.as_ref().unwrap();
        let client = server.clients.get_mut(&9).unwrap();

        client.out_buf.extend_from_slice(b"pending");
        readiness.sync_client_interest(9, client).unwrap();
        assert!(client.writable_interest);
        assert!(fd_is_readable(&readiness_fd));

        assert!(server.poll_clients().is_empty());
        let client = &server.clients[&9];
        assert!(client.out_buf.is_empty());
        assert!(!client.writable_interest);
        assert!(!fd_is_readable(&readiness_fd));

        let mut delivered = [0; 7];
        std::io::Read::read_exact(&mut peer, &mut delivered).unwrap();
        assert_eq!(&delivered, b"pending");
    }

    #[derive(Default)]
    struct OneChunkWriter {
        delivered: Vec<u8>,
        wrote_this_round: bool,
        chunk: usize,
    }

    impl OneChunkWriter {
        fn next_round(&mut self) {
            self.wrote_this_round = false;
        }
    }

    impl Write for OneChunkWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.wrote_this_round {
                return Err(io::Error::from(io::ErrorKind::WouldBlock));
            }
            let written = bytes.len().min(self.chunk);
            self.delivered.extend_from_slice(&bytes[..written]);
            self.wrote_this_round = true;
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn partial_output_writes_compact_by_chunks_not_by_write_count() {
        let expected: Vec<u8> = (0..MAX_CLIENT_BUF).map(|index| index as u8).collect();
        let mut buf = expected.clone();
        let mut start = 0;
        let mut writer = OneChunkWriter {
            chunk: 4096,
            ..OneChunkWriter::default()
        };
        let mut writes = 0;
        let mut compactions = 0;
        let mut bytes_moved = 0;

        while start < buf.len() {
            writer.next_round();
            let len_before = buf.len();
            flush_output_buffer(&mut writer, &mut buf, &mut start).unwrap();
            if buf.len() < len_before {
                compactions += 1;
                // `copy_within` moves exactly the retained suffix. A final
                // clear contributes zero copied bytes.
                bytes_moved += buf.len();
            }
            writes += 1;
        }

        assert_eq!(writer.delivered, expected);
        assert_eq!(writes, MAX_CLIENT_BUF / writer.chunk);
        let eager_drain_bytes = writer.chunk * writes * (writes - 1) / 2;
        assert_eq!(eager_drain_bytes, 133_693_440); // 127.5 MiB
        eprintln!(
            "output cursor: writes={writes} compactions={compactions} bytes_moved={bytes_moved} eager_drain_bytes={eager_drain_bytes}"
        );
        assert!(
            compactions <= 32 && bytes_moved < 8 * 1024 * 1024 && bytes_moved < eager_drain_bytes,
            "{writes} partial writes caused {compactions} compactions and {bytes_moved} copied bytes"
        );
        assert!(buf.is_empty());
        assert_eq!(start, 0);
    }

    #[test]
    fn socketpair_slow_reader_receives_the_exact_output_stream() {
        let (server_stream, mut peer) = UnixStream::pair().unwrap();
        let mut client = IpcClient::new(server_stream).unwrap();
        constrain_send_buffer(&client.stream);
        peer.set_nonblocking(true).unwrap();
        let expected: Vec<u8> = (0..256 * 1024).map(|index| index as u8).collect();
        client.out_buf = expected.clone();
        let mut delivered = Vec::with_capacity(expected.len());
        let mut chunk = [0; 1024];

        for _ in 0..expected.len() / chunk.len() + 64 {
            client.flush_out().unwrap();
            match peer.read(&mut chunk) {
                Ok(0) => panic!("socketpair closed before the output drained"),
                Ok(read) => delivered.extend_from_slice(&chunk[..read]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("slow socketpair read failed: {error}"),
            }
            if !client.has_pending_output() && delivered.len() == expected.len() {
                break;
            }
        }

        assert_eq!(delivered, expected);
        assert!(!client.has_pending_output());
    }

    #[test]
    fn slow_output_client_does_not_block_a_fast_clients_query() {
        let mut server = make_test_server();
        let _slow_peer = attach_test_client(&mut server, 8);
        let mut fast_peer = attach_test_client(&mut server, 9);
        let slow_client = server.clients.get_mut(&8).unwrap();
        constrain_send_buffer(&slow_client.stream);
        slow_client.out_buf = vec![b'x'; MAX_CLIENT_BUF];

        fast_peer
            .write_all(b"{\"query\":\"get_version\"}\n")
            .unwrap();
        let incoming = server.poll_clients();

        assert!(incoming.iter().any(|message| matches!(
            message,
            IncomingIpc::Query {
                client_id: 9,
                name,
                ..
            } if name == "get_version"
        )));
        assert!(
            server.clients[&8].has_pending_output(),
            "the socketpair peer does not read, so this exercises the slow-output path"
        );
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

    /// Wait until `path` refuses connections, i.e. it really is the stale
    /// socket file the caller means to hand to `bind_owned_socket`.
    ///
    /// Binding a listener and dropping it is not enough on its own inside a
    /// test binary. `fork` copies the whole descriptor table and `CLOEXEC` only
    /// takes effect at `exec`, so any sibling test that spawns a process during
    /// the window when this listener exists leaves its child holding a copy —
    /// and the socket keeps answering until that child execs. Measured
    /// directly: a forked child that merely sleeps keeps a closed listener
    /// connectable, three times out of three.
    ///
    /// `bind_owned_socket` is right to treat a socket that answers as live, so
    /// the fix belongs here: establish the precondition rather than weaken the
    /// assertion about what recovery does.
    fn wait_until_socket_is_stale(path: &Path) {
        for _ in 0..500 {
            match UnixStream::connect(path) {
                Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => return,
                // Some other process still holds a descriptor for it; that can
                // only be a child that has not reached `exec` yet, and no new
                // one can inherit it now that this process has closed it.
                Ok(stream) => drop(stream),
                Err(_) => {}
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!(
            "{} kept accepting connections; it never became stale",
            path.display()
        );
    }

    #[test]
    fn stale_owned_socket_is_recovered() {
        let path = temporary_path("stale").with_extension("sock");
        let stale = UnixListener::bind(&path).unwrap();
        drop(stale);
        wait_until_socket_is_stale(&path);

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

        let first = client
            .read_messages(MAX_MESSAGES_PER_POLL, MAX_READ_BYTES_PER_POLL)
            .0
            .unwrap();
        let second = client
            .read_messages(MAX_MESSAGES_PER_POLL, MAX_READ_BYTES_PER_POLL)
            .0
            .unwrap();

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

        let (first, first_bytes_read) =
            client.read_messages(MAX_MESSAGES_PER_POLL, MAX_READ_BYTES_PER_POLL);
        assert!(first.unwrap().is_empty());
        assert_eq!(first_bytes_read, MAX_READ_BYTES_PER_POLL);
        assert_eq!(client.buf.len(), MAX_READ_BYTES_PER_POLL);

        let (second, second_bytes_read) =
            client.read_messages(MAX_MESSAGES_PER_POLL, MAX_READ_BYTES_PER_POLL);
        assert!(second.unwrap().is_empty());
        assert_eq!(second_bytes_read, 17);
        assert_eq!(client.buf.len(), payload.len());
    }

    #[test]
    fn complete_final_frame_is_delivered_before_disconnect() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        let mut client = IpcClient::new(reader).unwrap();
        writer.write_all(b"final-message\n").unwrap();
        drop(writer);

        let (final_frame, bytes_read) =
            client.read_messages(MAX_MESSAGES_PER_POLL, MAX_READ_BYTES_PER_POLL);
        assert_eq!(final_frame.unwrap(), ["final-message"]);
        assert_eq!(bytes_read, b"final-message\n".len());
        // The writer end is closed, but a sibling test that forked between
        // `UnixStream::pair` and that close leaves its child holding a copy of
        // the descriptor until it reaches `exec` — the same mechanism spelled
        // out on `wait_until_socket_is_stale`. Until then the read side reports
        // "nothing yet" instead of end-of-file, which is not a violation of
        // what this test asserts. Poll for the disconnect, with a bound so a
        // genuinely missing end-of-file still fails.
        let mut disconnect = None;
        for _ in 0..500 {
            let (result, bytes_read) =
                client.read_messages(MAX_MESSAGES_PER_POLL, MAX_READ_BYTES_PER_POLL);
            assert_eq!(bytes_read, 0, "no bytes should follow the final frame");
            match result {
                Err(error) => {
                    disconnect = Some(error);
                    break;
                }
                Ok(messages) => {
                    assert!(
                        messages.is_empty(),
                        "no message should follow the final frame"
                    );
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let disconnect = disconnect.expect("the read side never reported end-of-file");
        assert_eq!(disconnect.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn poll_clients_respects_global_message_budget() {
        let mut server = make_test_server();
        let payload = query_payload(MAX_MESSAGES_PER_POLL);
        let mut peers = Vec::new();
        for id in 1..=5 {
            let mut peer = attach_test_client(&mut server, id);
            peer.write_all(&payload).unwrap();
            peers.push(peer);
        }

        let messages = server.poll_clients();

        assert_eq!(messages.len(), MAX_TOTAL_MESSAGES_PER_POLL);
        assert!(messages.len() <= MAX_TOTAL_MESSAGES_PER_POLL);
        assert_eq!(server.next_poll_client, 5);
    }

    #[test]
    fn poll_clients_respects_global_read_byte_budget() {
        let mut server = make_test_server();
        let payload = vec![b'x'; MAX_READ_BYTES_PER_POLL];
        let mut peers = Vec::new();
        for id in 1..=5 {
            let mut peer = attach_test_client(&mut server, id);
            peer.write_all(&payload).unwrap();
            peers.push(peer);
        }

        assert!(server.poll_clients().is_empty());
        let buffered_bytes: usize = server
            .clients
            .values()
            .map(|client| client.buf.len() - client.buf_start)
            .sum();

        assert_eq!(buffered_bytes, MAX_TOTAL_READ_BYTES_PER_POLL);
        assert!(buffered_bytes <= MAX_TOTAL_READ_BYTES_PER_POLL);
        assert_eq!(server.next_poll_client, 5);
    }

    #[test]
    fn round_robin_poll_eventually_services_all_flooding_clients() {
        let mut server = make_test_server();
        let payload = query_payload(MAX_MESSAGES_PER_POLL * 2);
        let mut peers = Vec::new();
        for id in 1..=8 {
            let mut peer = attach_test_client(&mut server, id);
            peer.write_all(&payload).unwrap();
            peers.push(peer);
        }

        let mut served = std::collections::HashSet::new();
        for _ in 0..2 {
            let messages = server.poll_clients();
            assert_eq!(messages.len(), MAX_TOTAL_MESSAGES_PER_POLL);
            for message in messages {
                let client_id = match message {
                    IncomingIpc::Command { client_id, .. }
                    | IncomingIpc::Query { client_id, .. }
                    | IncomingIpc::Subscribe { client_id, .. } => client_id,
                };
                served.insert(client_id);
            }
        }

        assert_eq!(served, (1..=8).collect());
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

        let outcome = normalize_subscriptions(topics);
        let normalized = outcome.subscribed;

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
    fn dropped_subscription_topics_are_reported_with_their_reason() {
        let long = format!("{}é", "x".repeat(MAX_SUBSCRIPTION_TOPIC_LEN));
        let mut topics = vec![
            " window ".to_string(),
            "window".to_string(),
            "   ".to_string(),
            long.clone(),
        ];
        topics.extend((0..MAX_SUBSCRIPTION_TOPICS + 2).map(|index| format!("topic-{index}")));

        let outcome = normalize_subscriptions(topics);

        assert_eq!(outcome.subscribed.len(), MAX_SUBSCRIPTION_TOPICS);
        let reasons: Vec<(&str, DroppedTopicReason)> = outcome
            .dropped
            .iter()
            .map(|dropped| (dropped.topic.as_str(), dropped.reason))
            .collect();
        let limit_first = format!("topic-{}", MAX_SUBSCRIPTION_TOPICS - 1);
        let limit_second = format!("topic-{MAX_SUBSCRIPTION_TOPICS}");
        let limit_third = format!("topic-{}", MAX_SUBSCRIPTION_TOPICS + 1);
        assert_eq!(
            reasons,
            vec![
                ("window", DroppedTopicReason::Duplicate),
                ("", DroppedTopicReason::Empty),
                (
                    "x".repeat(MAX_SUBSCRIPTION_TOPIC_LEN).as_str(),
                    DroppedTopicReason::TooLong
                ),
                (limit_first.as_str(), DroppedTopicReason::Limit),
                (limit_second.as_str(), DroppedTopicReason::Limit),
                (limit_third.as_str(), DroppedTopicReason::Limit),
            ]
        );
        assert_eq!(outcome.dropped_total, 6);
        assert_eq!(DroppedTopicReason::TooLong.as_str(), "too_long");
        // The echo of an oversized topic is cut on a character boundary.
        assert!(outcome.dropped[2].topic.len() <= MAX_SUBSCRIPTION_TOPIC_LEN);

        // A flood of junk is counted in full but named only in part.
        let flood = normalize_subscriptions(vec![String::new(); 1000]);
        assert!(flood.subscribed.is_empty());
        assert_eq!(flood.dropped.len(), MAX_REPORTED_DROPPED_TOPICS);
        assert_eq!(flood.dropped_total, 1000);
    }

    #[test]
    fn subscribe_stores_what_it_reports() {
        let mut server = make_test_server();
        let _peer = attach_test_client(&mut server, 1);

        let outcome = server.subscribe(1, vec![" window ".into(), "tag".into(), "window".into()]);

        assert_eq!(
            outcome.subscribed,
            vec!["window".to_string(), "tag".to_string()]
        );
        assert_eq!(outcome.dropped_total, 1);
        let client = server.clients.get(&1).expect("the test client");
        assert_eq!(client.subscriptions, outcome.subscribed);
    }

    #[test]
    fn subscription_prefix_matching_respects_topic_boundaries() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let mut client = IpcClient::new(stream).unwrap();
        client.subscriptions = normalize_subscriptions(vec![" window ".into()]).subscribed;

        assert!(client.is_subscribed("window"));
        assert!(client.is_subscribed("window/new"));
        assert!(!client.is_subscribed("windowing/new"));
        assert!(!client.is_subscribed("monitor/new"));

        client.subscriptions = normalize_subscriptions(vec!["*".into()]).subscribed;
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
    fn ambiguous_wire_message_is_rejected_without_dispatching_a_command() {
        let mut server = make_test_server();
        let mut peer = attach_test_client(&mut server, 7);
        peer.set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        peer.write_all(b"{\"command\":\"quit\",\"query\":\"get_version\"}\n")
            .unwrap();

        assert!(server.poll_clients().is_empty());

        let mut response = [0u8; 512];
        let read = std::io::Read::read(&mut peer, &mut response).unwrap();
        let response = std::str::from_utf8(&response[..read]).unwrap();
        assert!(response.contains("\"success\":false"), "{response}");
        assert!(response.contains("exactly one"), "{response}");
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

    fn client_id_of(message: &IncomingIpc) -> u64 {
        match message {
            IncomingIpc::Command { client_id, .. }
            | IncomingIpc::Query { client_id, .. }
            | IncomingIpc::Subscribe { client_id, .. } => *client_id,
        }
    }

    fn read_reply_lines(peer: &UnixStream, count: usize) -> Vec<String> {
        peer.set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let mut reader = std::io::BufReader::new(peer);
        (0..count)
            .map(|_| {
                let mut line = String::new();
                std::io::BufRead::read_line(&mut reader, &mut line).unwrap();
                line
            })
            .collect()
    }

    #[test]
    fn parse_errors_are_answered_in_frame_order_after_earlier_replies() {
        let mut server = make_test_server();
        let mut peer = attach_test_client(&mut server, 4);
        peer.write_all(
            b"{\"query\":\"get_version\"}\n{bad json\n{\"command\":\"view\",\"args\":2}\n",
        )
        .unwrap();

        let incoming = server.poll_clients();
        assert_eq!(incoming.len(), 2);
        for (index, message) in incoming.iter().enumerate() {
            let reply = IpcResponse::ok(Some(serde_json::json!({ "reply": index })));
            server.respond(client_id_of(message), &reply);
        }

        // Replies carry no correlation id, so a pipelining client can only
        // pair them with its frames by order.
        let lines = read_reply_lines(&peer, 3);
        assert!(lines[0].contains("\"reply\":0"), "{lines:?}");
        assert!(
            lines[1].contains("\"success\":false") && lines[1].contains("parse error"),
            "{lines:?}"
        );
        assert!(lines[2].contains("\"reply\":1"), "{lines:?}");
    }

    #[test]
    fn parse_errors_behind_abandoned_frames_are_released_by_the_next_poll() {
        let mut server = make_test_server();
        let mut peer = attach_test_client(&mut server, 3);
        peer.write_all(b"{\"query\":\"get_version\"}\n{bad json\n")
            .unwrap();

        assert_eq!(server.poll_clients().len(), 1);
        // The caller never answers the query. Its successor's parse error must
        // not wait for a reply that will never come.
        assert!(server.poll_clients().is_empty());

        let lines = read_reply_lines(&peer, 1);
        assert!(lines[0].contains("parse error"), "{lines:?}");
    }

    #[test]
    fn unterminated_final_frame_is_answered_at_end_of_stream() {
        let mut server = make_test_server();
        let mut valid_peer = attach_test_client(&mut server, 6);
        let mut truncated_peer = attach_test_client(&mut server, 7);
        valid_peer
            .write_all(b"{\"query\":\"get_version\"}")
            .unwrap();
        valid_peer.shutdown(std::net::Shutdown::Write).unwrap();
        truncated_peer.write_all(b"{\"query\":\"get_ver").unwrap();
        truncated_peer.shutdown(std::net::Shutdown::Write).unwrap();

        let incoming = server.poll_clients();
        assert!(
            matches!(
                incoming.as_slice(),
                [IncomingIpc::Query { client_id: 6, name, .. }] if name == "get_version"
            ),
            "the frame without a trailing newline must still be dispatched"
        );
        server.respond(6, &IpcResponse::ok(None));

        let valid_reply = read_reply_lines(&valid_peer, 1);
        assert!(
            valid_reply[0].contains("\"success\":true"),
            "{valid_reply:?}"
        );
        let truncated_reply = read_reply_lines(&truncated_peer, 1);
        assert!(
            truncated_reply[0].contains("parse error"),
            "{truncated_reply:?}"
        );

        assert!(server.poll_clients().is_empty());
        assert!(!server.clients.contains_key(&6));
        assert!(!server.clients.contains_key(&7));
    }

    #[test]
    fn half_closed_client_receives_its_whole_reply_before_retirement() {
        let mut server = make_test_server();
        let readiness = server.duplicate_readiness_fd().unwrap().unwrap();
        let mut peer = attach_test_client(&mut server, 5);
        constrain_send_buffer(&server.clients[&5].stream);
        peer.write_all(b"{\"query\":\"get_tree\"}\n").unwrap();
        peer.shutdown(std::net::Shutdown::Write).unwrap();

        let incoming = server.poll_clients();
        assert!(matches!(
            incoming.as_slice(),
            [IncomingIpc::Query { client_id: 5, name, .. }] if name == "get_tree"
        ));
        let reply = IpcResponse::ok(Some(serde_json::json!({ "blob": "x".repeat(256 * 1024) })));
        let mut expected = serde_json::to_vec(&reply).unwrap();
        expected.push(b'\n');
        server.respond(5, &reply);
        assert!(
            server.clients[&5].has_pending_output(),
            "the constrained socket must leave part of the reply queued"
        );

        // The peer's end-of-file stays level-ready. It must neither discard
        // the queued reply nor keep waking the event loop.
        assert!(server.poll_clients().is_empty());
        assert!(
            server.clients.contains_key(&5),
            "queued reply bytes must survive the read-side end-of-file"
        );
        assert!(
            !fd_is_readable(&readiness),
            "a draining half-closed client must wait for writability only"
        );

        peer.set_nonblocking(true).unwrap();
        let mut delivered = Vec::with_capacity(expected.len());
        let mut chunk = [0; 4096];
        for _ in 0..expected.len() / 1024 + 64 {
            match peer.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => delivered.extend_from_slice(&chunk[..read]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("half-closed peer read failed: {error}"),
            }
            server.poll_clients();
            if delivered.len() == expected.len() && !server.clients.contains_key(&5) {
                break;
            }
        }

        assert_eq!(delivered.len(), expected.len());
        assert!(delivered == expected, "the reply stream was corrupted");
        assert!(
            !server.clients.contains_key(&5),
            "a half-closed client is retired once its replies have drained"
        );
    }
}
