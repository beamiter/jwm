//! Clipboard sharing between the two ends of a remote session.
//!
//! Text travels as one `Clipboard` record, as it always has. Images and
//! copied files are too large for one record and too slow to send in one go
//! without stalling the screen and the keyboard behind them, so they travel
//! as a *transfer* of `ClipboardTransfer` records:
//!
//! ```text
//! op 1 Begin   id (u32) || kind (u8: 1 image, 2 files) || length (u64)
//!              || entries (u32) || items (u32)
//! op 2 Data    id (u32) || up to 128 KiB of the stream
//! op 3 End     id (u32)
//! op 4 Cancel  id (u32)                 sender gave up
//! op 5 Ack     id (u32) || consumed (u64)   receiver -> sender
//! op 6 Refuse  id (u32)                 receiver -> sender: stop
//! op 7 Skip    id (u32) || omitted (u64) counted Data suppressed by v14 restriction
//! ```
//!
//! An image's stream is its PNG; a file copy's is the entry stream of
//! [`super::clipboard_files`]. Flow control spans the whole session, not one
//! transfer: `consumed` counts every `Data` byte the receiver has taken this
//! session, including the tail of a transfer it dropped, and the sender keeps
//! at most [`TRANSFER_WINDOW_BYTES`] of all its `Data` unacknowledged. A
//! cancelled transfer's bytes still in flight therefore count against the
//! next one. The receiver acknowledges a chunk only once it is on disk, so a
//! fast network cannot outrun a slow disk, and a frame or an input batch
//! never queues behind more than a megabyte of copied data.
//!
//! The newest copy wins on both machines. Copying anything locally cancels a
//! transfer in either direction; a copy from the other machine replaces the
//! clipboard the moment it is announced, and a paste made while the data is
//! still arriving waits for it.
//!
//! All of this runs on one worker thread per session, fed by a channel: the
//! network threads only validate a record's shape and hand it over, so disk
//! and X11 work can never delay a frame or an input event.

use super::RemoteResult;
use super::clipboard_files::{ArchiveReceiver, Manifest, ManifestReader, Staging, human_bytes};
use super::clipboard_wire::{ClipboardRestriction, validate_successor};
use super::messages::{MAX_CLIPBOARD_BYTES, decode_clipboard, encode_clipboard};
use super::protocol::MessageKind;
use crate::backend::clipboard_x11::{
    CopiedClipboard, FileFulfilReceipt, PendingKind, RemoteCaptures, RemoteClipboard,
    RemoteClipboardOptions, RemoteClipboardShutdown, RemoteServe, RemoteSetter,
    RuntimeClipboardPolicy,
};
use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

/// Copied data carried by one `Data` record.
pub const TRANSFER_CHUNK_BYTES: usize = 128 * 1024;
/// Unacknowledged copied data a sender may have in flight.
pub const TRANSFER_WINDOW_BYTES: u64 = 1024 * 1024;
/// Largest image shared through the clipboard.
pub const MAX_SHARED_IMAGE_BYTES: usize = 32 * 1024 * 1024;
/// Default for `--clipboard-file-limit`.
pub const DEFAULT_FILE_LIMIT: u64 = 2 << 30;
/// Largest `ClipboardTransfer` payload.
pub const MAX_TRANSFER_RECORD_BYTES: usize = 1 + 4 + TRANSFER_CHUNK_BYTES;
/// Free space left on the receiving disk after a copy lands.
const RESERVED_FREE_BYTES: u64 = 256 << 20;
const STATUS_INTERVAL: Duration = Duration::from_millis(250);
const FORWARD_POLL: Duration = Duration::from_millis(250);
const WORKER_IDLE_WAIT: Duration = Duration::from_millis(500);

const OP_BEGIN: u8 = 1;
const OP_DATA: u8 = 2;
const OP_END: u8 = 3;
const OP_CANCEL: u8 = 4;
const OP_ACK: u8 = 5;
const OP_REFUSE: u8 = 6;
const OP_SKIP: u8 = 7;
const RESTRICTION_BACKEND_TIMEOUT: Duration = Duration::from_secs(5);
const KIND_IMAGE: u8 = 1;
const KIND_FILES: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferKind {
    Image,
    Files,
}

/// One decoded `ClipboardTransfer` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferOp {
    Begin {
        id: u32,
        kind: TransferKind,
        length: u64,
        entries: u32,
        items: u32,
    },
    Data {
        id: u32,
        bytes: Vec<u8>,
    },
    /// Counted outgoing bytes suppressed by a runtime restriction. Never feeds a sink.
    Skip {
        id: u32,
        bytes: u64,
    },
    End {
        id: u32,
    },
    Cancel {
        id: u32,
    },
    Ack {
        id: u32,
        consumed: u64,
    },
    Refuse {
        id: u32,
    },
}

impl TransferOp {
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut payload = Vec::with_capacity(match self {
            Self::Data { bytes, .. } => 5 + bytes.len(),
            _ => 32,
        });
        let (op, id) = match self {
            Self::Begin { id, .. } => (OP_BEGIN, id),
            Self::Data { id, .. } => (OP_DATA, id),
            Self::End { id } => (OP_END, id),
            Self::Cancel { id } => (OP_CANCEL, id),
            Self::Ack { id, .. } => (OP_ACK, id),
            Self::Refuse { id } => (OP_REFUSE, id),
            Self::Skip { id, .. } => (OP_SKIP, id),
        };
        payload.push(op);
        payload.extend_from_slice(&id.to_be_bytes());
        match self {
            Self::Begin {
                kind,
                length,
                entries,
                items,
                ..
            } => {
                payload.push(match kind {
                    TransferKind::Image => KIND_IMAGE,
                    TransferKind::Files => KIND_FILES,
                });
                payload.extend_from_slice(&length.to_be_bytes());
                payload.extend_from_slice(&entries.to_be_bytes());
                payload.extend_from_slice(&items.to_be_bytes());
            }
            Self::Data { bytes, .. } => payload.extend_from_slice(bytes),
            Self::Ack { consumed, .. } => payload.extend_from_slice(&consumed.to_be_bytes()),
            Self::Skip { bytes, .. } => payload.extend_from_slice(&bytes.to_be_bytes()),
            Self::End { .. } | Self::Cancel { .. } | Self::Refuse { .. } => {}
        }
        payload
    }

    pub fn decode(payload: &[u8]) -> RemoteResult<Self> {
        if payload.len() < 5 {
            return Err(invalid_data("clipboard transfer record is too short").into());
        }
        if payload.len() > MAX_TRANSFER_RECORD_BYTES {
            return Err(invalid_data("clipboard transfer record is too large").into());
        }
        let id = u32::from_be_bytes(payload[1..5].try_into().expect("fixed slice"));
        let body = &payload[5..];
        let exact = |len: usize| {
            if body.len() == len {
                Ok(())
            } else {
                Err(invalid_data(
                    "clipboard transfer record has the wrong length",
                ))
            }
        };
        Ok(match payload[0] {
            OP_BEGIN => {
                exact(1 + 8 + 4 + 4)?;
                let kind = match body[0] {
                    KIND_IMAGE => TransferKind::Image,
                    KIND_FILES => TransferKind::Files,
                    _ => return Err(invalid_data("unknown clipboard transfer kind").into()),
                };
                Self::Begin {
                    id,
                    kind,
                    length: u64::from_be_bytes(body[1..9].try_into().expect("fixed slice")),
                    entries: u32::from_be_bytes(body[9..13].try_into().expect("fixed slice")),
                    items: u32::from_be_bytes(body[13..17].try_into().expect("fixed slice")),
                }
            }
            OP_DATA => {
                if body.is_empty() {
                    return Err(invalid_data("clipboard transfer chunk is empty").into());
                }
                Self::Data {
                    id,
                    bytes: body.to_vec(),
                }
            }
            OP_END => {
                exact(0)?;
                Self::End { id }
            }
            OP_CANCEL => {
                exact(0)?;
                Self::Cancel { id }
            }
            OP_ACK => {
                exact(8)?;
                Self::Ack {
                    id,
                    consumed: u64::from_be_bytes(body.try_into().expect("fixed slice")),
                }
            }
            OP_SKIP => {
                exact(8)?;
                let bytes = u64::from_be_bytes(body.try_into().expect("fixed slice"));
                if bytes == 0 || bytes > TRANSFER_CHUNK_BYTES as u64 {
                    return Err(invalid_data("invalid skipped clipboard byte count").into());
                }
                Self::Skip { id, bytes }
            }
            OP_REFUSE => {
                exact(0)?;
                Self::Refuse { id }
            }
            _ => return Err(invalid_data("unknown clipboard transfer operation").into()),
        })
    }
}

/// How this end shares its clipboard.
#[derive(Debug, Clone, Copy)]
pub struct SyncOptions {
    /// This endpoint may capture and send local clipboard content.
    pub send: bool,
    /// This endpoint may receive and serve peer clipboard content.
    pub receive: bool,
    /// Image sharing was negotiated.
    pub images: bool,
    /// Copied files were negotiated with the peer.
    pub files: bool,
    /// Largest file copy this end sends or accepts.
    pub file_limit: u64,
    /// Share what is on the clipboard when the session starts.
    pub share_existing: bool,
}

impl SyncOptions {
    fn content(self) -> u8 {
        if self.files && self.file_limit != 0 {
            7
        } else if self.images {
            3
        } else {
            1
        }
    }
    fn validate_peer_op(self, op: &TransferOp) -> RemoteResult<()> {
        if !self.images && (!self.files || self.file_limit == 0) {
            return Err(invalid_data("clipboard transfers were not negotiated").into());
        }
        match op {
            TransferOp::Ack { .. } | TransferOp::Refuse { .. } if !self.send => {
                Err(invalid_data("clipboard send direction was not negotiated").into())
            }
            TransferOp::Ack { .. } | TransferOp::Refuse { .. } => Ok(()),
            _ if !self.receive => {
                Err(invalid_data("clipboard receive direction was not negotiated").into())
            }
            TransferOp::Begin {
                kind: TransferKind::Image,
                ..
            } if !self.images => Err(invalid_data("image sharing was not negotiated").into()),
            TransferOp::Begin {
                kind: TransferKind::Files,
                ..
            } if !self.files || self.file_limit == 0 => {
                Err(invalid_data("file sharing was not negotiated").into())
            }
            _ => Ok(()),
        }
    }
}

/// A record for the session writer to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    pub kind: MessageKind,
    pub payload: Vec<u8>,
}

/// What the viewer shows about clipboard activity.
#[derive(Default)]
pub struct SyncStatus {
    line: Mutex<Option<String>>,
    version: AtomicU64,
}

impl SyncStatus {
    fn set(&self, line: Option<String>) {
        let mut current = self
            .line
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if *current != line {
            *current = line;
            self.version.fetch_add(1, Ordering::Release);
        }
    }

    /// Changes since `seen`, as `(version, line)`.
    pub fn changed_since(&self, seen: u64) -> Option<(u64, Option<String>)> {
        let version = self.version.load(Ordering::Acquire);
        (version != seen).then(|| {
            let line = self
                .line
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clone();
            (version, line)
        })
    }
}

enum Command {
    Local(CopiedClipboard),
    LocalClosed,
    PeerText(String),
    Peer(TransferOp),
    Restrict {
        record: ClipboardRestriction,
        reply: MessageKind,
    },
    Stop,
}

// A slow disk or writer must not turn clipboard traffic into an unbounded
// allocation, nor block the network reader behind clipboard work. Queue limits
// include both payload bytes and record count (including zero-data controls).
const COMMAND_QUEUE_RECORDS: usize = 64;
const COMMAND_QUEUE_BYTES: usize = MAX_SHARED_IMAGE_BYTES + TRANSFER_WINDOW_BYTES as usize;
const OUTGOING_QUEUE_RECORDS: usize = 64;
const OUTGOING_QUEUE_BYTES: usize = 2 * TRANSFER_WINDOW_BYTES as usize;

struct QueueState<T> {
    entries: VecDeque<(T, usize)>,
    bytes: usize,
    closed: bool,
    reserved: Option<(T, usize)>,
}

struct BoundedQueue<T> {
    state: Mutex<QueueState<T>>,
    ready: Condvar,
    max_records: usize,
    max_bytes: usize,
}

impl<T> BoundedQueue<T> {
    fn new(max_records: usize, max_bytes: usize) -> Self {
        Self {
            state: Mutex::new(QueueState {
                entries: VecDeque::new(),
                bytes: 0,
                closed: false,
                reserved: None,
            }),
            ready: Condvar::new(),
            max_records,
            max_bytes,
        }
    }

    // Replacement is restricted to records with explicit superseding semantics.
    fn push(&self, value: T, bytes: usize, replace: impl Fn(&T) -> bool) -> Result<(), ()> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.closed {
            return Err(());
        }
        let mut removed = 0;
        let preceding = state.reserved.as_ref().map_or(0, |(_, before)| *before);
        let mut index = 0;
        let mut removed_before = 0;
        state.entries.retain(|(entry, size)| {
            let discard = replace(entry);
            if discard {
                removed += size;
                removed_before += usize::from(index < preceding);
            }
            index += 1;
            !discard
        });
        if let Some((_, before)) = &mut state.reserved {
            *before -= removed_before;
        }
        state.bytes -= removed;
        if state.entries.len() >= self.max_records
            || bytes > self.max_bytes.saturating_sub(state.bytes)
        {
            // Fail closed: dropping a transfer record and continuing would corrupt
            // the stream or its cumulative flow-control accounting.
            state.closed = true;
            state.reserved = None;
            state.entries.clear();
            state.bytes = 0;
            self.ready.notify_all();
            return Err(());
        }
        state.entries.push_back((value, bytes));
        state.bytes += bytes;
        self.ready.notify_one();
        Ok(())
    }

    /// A single bounded policy fence never competes with ordinary data capacity.
    /// Worker controls preempt queued content; outgoing fences follow the old FIFO.
    fn reserve(&self, value: T, ordered: bool) -> Result<(), ()> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.closed || state.reserved.is_some() {
            return Err(());
        }
        let before = if ordered { state.entries.len() } else { 0 };
        state.reserved = Some((value, before));
        self.ready.notify_all();
        Ok(())
    }

    fn pop(state: &mut QueueState<T>) -> Option<T> {
        if state
            .reserved
            .as_ref()
            .is_some_and(|(_, before)| *before == 0)
        {
            return state.reserved.take().map(|(value, _)| value);
        }
        let (value, bytes) = state.entries.pop_front()?;
        state.bytes -= bytes;
        if let Some((_, before)) = &mut state.reserved {
            *before -= 1;
        }
        Some(value)
    }

    fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.closed = true;
        state.reserved = None;
        state.entries.clear();
        state.bytes = 0;
        self.ready.notify_all();
    }

    fn closed(&self) -> bool {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).closed
    }

    fn recv_timeout(&self, timeout: Duration) -> Result<T, mpsc::RecvTimeoutError> {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let (mut state, _) = self
            .ready
            .wait_timeout_while(state, timeout, |s| {
                s.entries.is_empty() && s.reserved.is_none() && !s.closed
            })
            .unwrap_or_else(|p| p.into_inner());
        if let Some(value) = Self::pop(&mut state) {
            Ok(value)
        } else if state.closed {
            Err(mpsc::RecvTimeoutError::Disconnected)
        } else {
            Err(mpsc::RecvTimeoutError::Timeout)
        }
    }

    fn try_recv(&self) -> Result<T, mpsc::TryRecvError> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(value) = Self::pop(&mut state) {
            Ok(value)
        } else if state.closed {
            Err(mpsc::TryRecvError::Disconnected)
        } else {
            Err(mpsc::TryRecvError::Empty)
        }
    }
}

impl BoundedQueue<Command> {
    fn send(&self, command: Command) -> Result<(), ()> {
        if matches!(command, Command::Stop | Command::LocalClosed) {
            // Shutdown has a reserved path even when the data queue is full.
            self.close();
            return Ok(());
        }
        if matches!(command, Command::Restrict { .. }) {
            return self.reserve(command, false);
        }
        let bytes = match &command {
            Command::Local(copied) => copied.retained_bytes().unwrap_or(usize::MAX),
            Command::PeerText(text) => text.capacity(),
            Command::Peer(TransferOp::Data { bytes, .. }) => bytes.capacity(),
            _ => 32,
        };
        let local = matches!(command, Command::Local(_));
        // Local copies are generations: only the latest pending value matters.
        // Peer records retain wire order, including ACKs with transfer IDs.
        self.push(command, bytes, |old| {
            local && matches!(old, Command::Local(_))
        })
    }
}

impl BoundedQueue<Outgoing> {
    fn send(&self, record: Outgoing) -> Result<(), ()> {
        if matches!(
            record.kind,
            MessageKind::ClipboardRestrict | MessageKind::ClipboardRestricted
        ) {
            return self.reserve(record, true);
        }
        let bytes = record.payload.capacity();
        let ack = record.kind == MessageKind::ClipboardTransfer
            && record.payload.first() == Some(&OP_ACK);
        let ack_id: Option<[u8; 4]> = ack
            .then(|| record.payload.get(1..5))
            .flatten()
            .and_then(|id| id.try_into().ok());
        self.push(record, bytes, |old| {
            // Offsets are cumulative, but preserve ACKs for other transfer IDs:
            // peers also use the ID to finish their current transfer's status.
            // Append the replacement to preserve ordering with other records.
            ack_id.is_some_and(|id| {
                old.kind == MessageKind::ClipboardTransfer
                    && old.payload.first() == Some(&OP_ACK)
                    && old.payload.get(1..5) == Some(id.as_slice())
            })
        })
    }
}

/// Owns the session clipboard threads. Drop only signals; use explicit shutdown
/// to establish cleanup, including file work and X11 thread-local destructors.
pub struct ClipboardSync {
    commands: Arc<BoundedQueue<Command>>,
    stop: Arc<AtomicBool>,
    status: Arc<SyncStatus>,
    worker: ClipboardThread,
    forwarder: ClipboardThread,
    backend: ClipboardThread,
    backend_stop: Option<RemoteClipboardShutdown>,
}

#[derive(Default)]
struct ClipboardThread {
    worker: Option<thread::JoinHandle<()>>,
    joining: Option<mpsc::Receiver<bool>>,
    failure: Option<String>,
}

impl ClipboardThread {
    fn new(worker: thread::JoinHandle<()>) -> Self {
        Self {
            worker: Some(worker),
            ..Self::default()
        }
    }

    fn join_until(&mut self, deadline: Instant, name: &str) -> Result<(), String> {
        if let Some(worker) = self.worker.take() {
            match super::sender::reap_worker(worker) {
                Ok(joining) => self.joining = Some(joining),
                Err(error) => {
                    self.failure = Some(format!("start {name} reaper (worker detached): {error}"))
                }
            }
        }
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        if let Some(joining) = &self.joining {
            match joining.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(true) => self.joining = None,
                Ok(false) => {
                    self.joining = None;
                    self.failure = Some(format!("{name} panicked"));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(format!(
                        "{name} shutdown timed out (worker/reaper may remain detached)"
                    ));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.joining = None;
                    self.failure =
                        Some(format!("{name} reaper disconnected (cleanup unconfirmed)"));
                }
            }
        }
        self.failure.clone().map_or(Ok(()), Err)
    }
}

/// Network-reader half: hands peer clipboard records to the worker.
pub struct ClipboardInbox {
    commands: Arc<BoundedQueue<Command>>,
    in_flight: Arc<AtomicU64>,
    options: SyncOptions,
    control: ClipboardControl,
}

/// Writer half: records the worker wants sent.
pub struct ClipboardOutbox {
    records: Arc<BoundedQueue<Outgoing>>,
    control: ClipboardControl,
}

#[derive(Clone)]
pub struct ClipboardControl {
    commands: Arc<BoundedQueue<Command>>,
    policy: RuntimeClipboardPolicy,
    state: Arc<Mutex<RestrictionControlState>>,
}
struct RestrictionControlState {
    revision: u64,
    content: u8,
    pending: Option<ClipboardRestriction>,
}
impl ClipboardControl {
    fn new(
        commands: Arc<BoundedQueue<Command>>,
        policy: RuntimeClipboardPolicy,
        content: u8,
    ) -> Self {
        Self {
            commands,
            policy,
            state: Arc::new(Mutex::new(RestrictionControlState {
                revision: 0,
                content,
                pending: None,
            })),
        }
    }
    pub(crate) fn policy(&self) -> RuntimeClipboardPolicy {
        self.policy.clone()
    }
    pub(crate) fn restrict(
        &self,
        record: ClipboardRestriction,
        reply: MessageKind,
    ) -> RemoteResult<()> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.pending.is_some() {
            return Err(invalid_data("clipboard restriction already pending").into());
        }
        validate_successor(state.revision, state.content, record)?;
        self.policy.narrow(record.content).map_err(invalid_data)?;
        state.revision = record.revision;
        state.content = record.content;
        state.pending = Some(record);
        self.commands
            .send(Command::Restrict { record, reply })
            .map_err(|_| invalid_data("clipboard restriction queue unavailable"))?;
        Ok(())
    }
    /// The request reader must not observe the old pending record after the
    /// ACK becomes visible on the wire. Lock order: writer -> this state. The
    /// reader holds this state only for gate/queue admission, never a writer.
    pub(crate) fn with_written_barrier(
        &self,
        record: ClipboardRestriction,
        write: impl FnOnce() -> RemoteResult<()>,
    ) -> RemoteResult<()> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.pending != Some(record) {
            return Err(invalid_data("unexpected clipboard restriction barrier").into());
        }
        write()?;
        state.pending = None;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn for_test(content: u8) -> Self {
        Self::new(
            Arc::new(BoundedQueue::new(
                COMMAND_QUEUE_RECORDS,
                COMMAND_QUEUE_BYTES,
            )),
            RuntimeClipboardPolicy::new(content),
            content,
        )
    }

    pub(crate) fn restriction_written(&self, record: ClipboardRestriction) -> RemoteResult<()> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.pending != Some(record) {
            return Err(invalid_data("unexpected clipboard restriction barrier").into());
        }
        state.pending = None;
        Ok(())
    }
}

type Wake = Box<dyn Fn() + Send>;

/// Where the worker puts what arrives from the other machine: this display's
/// clipboard, or a recorder in tests.
trait ClipboardTarget {
    /// `false` when the target can no longer accept requests.
    fn serve(&self, request: RemoteServe) -> bool;
    fn policy(&self) -> Option<RuntimeClipboardPolicy> {
        None
    }
    fn restrict(&self, _content: u8) -> Result<mpsc::Receiver<()>, String> {
        let (tx, rx) = mpsc::sync_channel(1);
        let _ = tx.send(());
        Ok(rx)
    }
}

impl ClipboardTarget for RemoteSetter {
    fn policy(&self) -> Option<RuntimeClipboardPolicy> {
        Some(self.policy())
    }
    fn restrict(&self, content: u8) -> Result<mpsc::Receiver<()>, String> {
        self.restrict(content)
    }
    fn serve(&self, request: RemoteServe) -> bool {
        self.send(request)
    }
}

/// Start the worker thread and the two network-facing halves.
fn spawn_worker<T: ClipboardTarget + Send + 'static>(
    target: T,
    options: SyncOptions,
    staging_root: Option<PathBuf>,
    wake: Option<Wake>,
) -> Result<
    (
        Arc<BoundedQueue<Command>>,
        thread::JoinHandle<()>,
        ClipboardInbox,
        ClipboardOutbox,
        Arc<SyncStatus>,
    ),
    String,
> {
    let commands = Arc::new(BoundedQueue::new(
        COMMAND_QUEUE_RECORDS,
        COMMAND_QUEUE_BYTES,
    ));
    let command_rx = Arc::clone(&commands);
    let records = Arc::new(BoundedQueue::new(
        OUTGOING_QUEUE_RECORDS,
        OUTGOING_QUEUE_BYTES,
    ));
    let records_tx = Arc::clone(&records);
    let status = Arc::new(SyncStatus::default());
    let in_flight = Arc::new(AtomicU64::new(0));
    let policy = target
        .policy()
        .unwrap_or_else(|| RuntimeClipboardPolicy::new(options.content()));
    let control = ClipboardControl::new(commands.clone(), policy.clone(), options.content());
    let mut worker = Worker {
        policy,
        target,
        out: records_tx,
        wake,
        in_flight: Arc::clone(&in_flight),
        status: Arc::clone(&status),
        options,
        staging_root,
        staging: None,
        next_id: 1,
        next_token: 1,
        sending: None,
        receiving: None,
        pending_token: None,
        sent_total: 0,
        acked_total: 0,
        consumed_total: 0,
        status_expires: None,
        last_received: None,
    };
    let join = thread::Builder::new()
        .name("jwm-remote-clipsync".into())
        .spawn(move || worker.run_guarded(&command_rx))
        .map_err(|error| format!("spawn clipboard worker: {error}"))?;
    Ok((
        commands.clone(),
        join,
        ClipboardInbox {
            commands,
            in_flight,
            options,
            control: control.clone(),
        },
        ClipboardOutbox { records, control },
        status,
    ))
}

impl ClipboardSync {
    /// Start sharing `display`'s clipboard. `wake` is called whenever a
    /// record is queued for the writer.
    pub fn start(
        display: Option<&str>,
        options: SyncOptions,
        wake: Option<Wake>,
    ) -> Result<(Self, ClipboardInbox, ClipboardOutbox), String> {
        let (captures, setter) = RemoteClipboard::start(
            display,
            RemoteClipboardOptions {
                send: options.send,
                images: options.images,
                files: options.files,
                image_bytes: MAX_SHARED_IMAGE_BYTES,
                capture_existing: options.send && options.share_existing,
            },
        )?;
        let (backend_stop, backend) = setter.take_shutdown();
        let (commands, worker, inbox, outbox, status) = spawn_worker(setter, options, None, wake)?;
        let stop = Arc::new(AtomicBool::new(false));
        let forward_commands = commands.clone();
        let forward_stop = Arc::clone(&stop);
        let forwarder = thread::Builder::new()
            .name("jwm-remote-clipwatch".into())
            .spawn(move || forward_captures(&captures, &forward_commands, &forward_stop));
        let forwarder = match forwarder {
            Ok(forwarder) => forwarder,
            Err(error) => {
                let _ = commands.send(Command::Stop);
                // Startup failure must not turn into an unbounded file/X11 join.
                drop(worker);
                return Err(format!("spawn clipboard watcher: {error}"));
            }
        };
        Ok((
            Self {
                commands,
                stop,
                status,
                worker: ClipboardThread::new(worker),
                forwarder: ClipboardThread::new(forwarder),
                backend: ClipboardThread::new(backend),
                backend_stop: Some(backend_stop),
            },
            inbox,
            outbox,
        ))
    }

    /// One shared deadline bounds all clipboard joins. Failure means cleanup
    /// is incomplete, not permission to reconnect or reuse transfer state.
    /// A later call may observe completion of the same reapers, never new workers.
    pub fn shutdown_timeout(&mut self, timeout: Duration) -> RemoteResult<()> {
        self.signal_stop();
        let deadline = Instant::now() + timeout;
        let mut failures = Vec::new();
        for (thread, name) in [
            (&mut self.worker, "clipboard transfer worker"),
            (&mut self.forwarder, "clipboard capture forwarder"),
        ] {
            if let Err(error) = thread.join_until(deadline, name) {
                failures.push(error);
            }
        }
        // Allow transfer cleanup/token abandonment before stopping X11 when
        // possible. On timeout it still owns its state until it actually exits.
        if let Some(stop) = &self.backend_stop {
            stop.stop();
        }
        if let Err(error) = self.backend.join_until(deadline, "clipboard X11 worker") {
            failures.push(error);
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "clipboard cleanup incomplete; {}",
                failures.join("; ")
            ))
            .into())
        }
    }

    fn signal_stop(&self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.commands.send(Command::Stop);
    }

    #[must_use]
    pub fn status(&self) -> Arc<SyncStatus> {
        Arc::clone(&self.status)
    }
}

impl Drop for ClipboardSync {
    fn drop(&mut self) {
        self.signal_stop();
        if let Some(stop) = &self.backend_stop {
            stop.stop();
        }
        // Join handles/reapers detach. Never wait in Drop, including unwinding.
    }
}

fn forward_captures(
    captures: &RemoteCaptures,
    commands: &Arc<BoundedQueue<Command>>,
    stop: &AtomicBool,
) {
    while !stop.load(Ordering::Acquire) {
        match captures.recv_timeout(FORWARD_POLL) {
            Ok(Some(copied)) => {
                if commands.send(Command::Local(copied)).is_err() {
                    return;
                }
            }
            Ok(None) => {}
            Err(_) => {
                let _ = commands.send(Command::LocalClosed);
                return;
            }
        }
    }
}

/// Use a CAS loop rather than APIs newer than the minimum supported Rust.
fn reserve_in_flight(in_flight: &AtomicU64, bytes: u64) -> bool {
    let mut pending = in_flight.load(Ordering::Acquire);
    loop {
        let Some(total) = pending
            .checked_add(bytes)
            .filter(|sum| *sum <= TRANSFER_WINDOW_BYTES)
        else {
            return false;
        };
        match in_flight.compare_exchange_weak(pending, total, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(actual) => pending = actual,
        }
    }
}

impl ClipboardInbox {
    pub(crate) fn restrict(&self, record: ClipboardRestriction) -> RemoteResult<()> {
        self.control
            .restrict(record, MessageKind::ClipboardRestricted)
    }
    /// Hand one authenticated peer record to the worker.
    ///
    /// Checks record shape, flow control and bounded queue admission without
    /// waiting for disk work. An error (including overload) ends the session.
    pub fn deliver(&self, kind: MessageKind, payload: &[u8]) -> RemoteResult<()> {
        let mut data_bytes = 0;
        let command = match kind {
            MessageKind::Clipboard => {
                if !self.options.receive {
                    return Err(
                        invalid_data("clipboard receive direction was not negotiated").into(),
                    );
                }
                let text = decode_clipboard(payload)?;
                if self.control.policy.content() & 1 == 0 {
                    return Ok(());
                }
                Command::PeerText(text)
            }
            MessageKind::ClipboardTransfer => {
                let op = TransferOp::decode(payload)?;
                self.options.validate_peer_op(&op)?;
                data_bytes = match &op {
                    TransferOp::Data { bytes, .. } => bytes.len() as u64,
                    TransferOp::Skip { bytes, .. } => *bytes,
                    _ => 0,
                };
                if data_bytes != 0 && !reserve_in_flight(&self.in_flight, data_bytes) {
                    self.commands.close();
                    return Err(invalid_data(
                        "peer sent more clipboard data than it was allowed in flight",
                    )
                    .into());
                }
                Command::Peer(op)
            }
            other => {
                return Err(invalid_data(format!("{other:?} is not a clipboard record")).into());
            }
        };
        self.commands.send(command).map_err(|_| {
            self.in_flight.fetch_sub(data_bytes, Ordering::AcqRel);
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "clipboard queue closed or overloaded",
            )
            .into()
        })
    }
}

impl Drop for ClipboardOutbox {
    fn drop(&mut self) {
        self.records.close();
    }
}

impl ClipboardOutbox {
    pub(crate) fn control(&self) -> ClipboardControl {
        self.control.clone()
    }
    /// The next queued record without waiting. `Err` once the worker stopped.
    pub fn try_next(&self) -> Result<Option<Outgoing>, mpsc::RecvError> {
        match self.records.try_recv() {
            Ok(record) => Ok(Some(record)),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => Err(mpsc::RecvError),
        }
    }

    /// Wait up to `timeout` for the next record. `Err` once the worker stopped.
    pub fn next_timeout(&self, timeout: Duration) -> Result<Option<Outgoing>, mpsc::RecvError> {
        match self.records.recv_timeout(timeout) {
            Ok(record) => Ok(Some(record)),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(mpsc::RecvError),
        }
    }
}

enum Source {
    Image(Vec<u8>),
    Files(ManifestReader),
}

struct Sending {
    id: u32,
    source: Source,
    length: u64,
    /// Session-wide `Data` offset at which this transfer starts.
    start: u64,
    sent: u64,
    ended: bool,
    label: String,
    started: Instant,
}

enum Sink {
    Image(Vec<u8>),
    Files(ArchiveReceiver),
}

struct Receiving {
    id: u32,
    token: u64,
    sink: Sink,
    length: u64,
    received: u64,
    label: String,
    started: Instant,
}

struct Worker<T> {
    target: T,
    policy: RuntimeClipboardPolicy,
    out: Arc<BoundedQueue<Outgoing>>,
    wake: Option<Wake>,
    in_flight: Arc<AtomicU64>,
    status: Arc<SyncStatus>,
    options: SyncOptions,
    /// Where received files go; `None` is the user's cache directory.
    staging_root: Option<PathBuf>,
    staging: Option<Staging>,
    next_id: u32,
    next_token: u64,
    sending: Option<Sending>,
    receiving: Option<Receiving>,
    // Kept independently while finish consumes the sink, including callback unwind.
    pending_token: Option<u64>,
    /// `Data` bytes sent this session, and how many the peer has consumed.
    sent_total: u64,
    acked_total: u64,
    /// `Data` bytes received and consumed this session.
    consumed_total: u64,
    /// When a finished/failed message stops being shown.
    status_expires: Option<Instant>,
    /// What this end last put on its clipboard for the peer. A clipboard
    /// manager that re-takes ownership, or JWM's history re-serving an entry,
    /// makes it look like a fresh local copy; sending it back would bounce
    /// the same copy between the machines forever.
    last_received: Option<Fingerprint>,
}

/// Identity of a clipboard value, cheap to keep for an image.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Fingerprint {
    Text(String),
    Png(u64),
    Files(Vec<PathBuf>),
}

impl Fingerprint {
    fn of(copied: &CopiedClipboard) -> Self {
        match copied {
            CopiedClipboard::Text(text) => Self::Text(text.clone()),
            CopiedClipboard::Png(png) => Self::Png(png_hash(png)),
            CopiedClipboard::Files(paths) => Self::Files(paths.clone()),
        }
    }
}

fn png_hash(png: &[u8]) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    png.hash(&mut hasher);
    hasher.finish()
}

/// How long "clipboard update queued" and similar outcomes stay in the title.
const OUTCOME_SHOWN_FOR: Duration = Duration::from_secs(10);

impl<T: ClipboardTarget> Worker<T> {
    /// A callback panic must disconnect both halves just like dropping the old
    /// channel senders did. Close first, then independently attempt cleanup and
    /// notification so a second panicking callback cannot leave the writer asleep
    /// or abort the process during unwinding. Preserve the original join error.
    fn run_guarded(&mut self, commands: &BoundedQueue<Command>) {
        use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
        let run = catch_unwind(AssertUnwindSafe(|| self.run(commands)));
        commands.close();
        self.out.close();
        let cleanup = catch_unwind(AssertUnwindSafe(|| self.cleanup()));
        let wake = catch_unwind(AssertUnwindSafe(|| {
            if let Some(wake) = &self.wake {
                wake();
            }
        }));
        if let Err(panic) = run.and(cleanup).and(wake) {
            resume_unwind(panic);
        }
    }

    fn run(&mut self, commands: &BoundedQueue<Command>) {
        let mut last_status = Instant::now();
        while !commands.closed() && !self.out.closed() {
            self.pump();
            if self.out.closed() {
                break;
            }
            match commands.recv_timeout(WORKER_IDLE_WAIT) {
                Ok(Command::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Ok(Command::LocalClosed) => {
                    eprintln!("jwm-remote: the local clipboard watcher stopped");
                    break;
                }
                Ok(command) => self.apply(command),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if last_status.elapsed() >= STATUS_INTERVAL {
                self.publish_progress();
                last_status = Instant::now();
            }
            if self
                .status_expires
                .is_some_and(|expires| Instant::now() >= expires)
                && self.sending.is_none()
                && self.receiving.is_none()
            {
                self.status_expires = None;
                self.status.set(None);
            }
        }
    }

    fn cleanup(&mut self) {
        self.sending = None;
        // Close receiver descriptors before deleting files and releasing budget.
        self.receiving = None;
        if let Some(staging) = self.staging.as_mut() {
            staging.reclaim_rejected();
            staging.discard_in_progress();
        }
        if let Some(token) = self.pending_token.take() {
            self.target.serve(RemoteServe::Abandon { token });
        }
    }

    // A rejected command is terminal: continuing would lose ownership ordering
    // or leave a paste waiting for data that can never arrive.
    fn serve(&self, request: RemoteServe) -> bool {
        if self.out.closed() || request.retained_bytes().is_none() {
            request.reject_unpublished();
            if self.out.closed() {
                return false;
            }
        } else if self.target.serve(request) {
            return true;
        }
        self.out.close();
        if let Some(wake) = &self.wake {
            wake();
        }
        false
    }

    fn apply(&mut self, command: Command) {
        match command {
            Command::Local(copied) => self.share_local(copied),
            Command::PeerText(text) => {
                if !self.options.receive || self.policy.content() & 1 == 0 {
                    return;
                }
                self.cancel_sending();
                self.drop_receiving(false);
                self.last_received = Some(Fingerprint::Text(text.clone()));
                if !self.serve(RemoteServe::Text(text)) {
                    return;
                }
                self.status.set(None);
            }
            Command::Peer(op) => self.on_peer(op),
            Command::Restrict { record, reply } => self.apply_restriction(record, reply),
            Command::LocalClosed | Command::Stop => {}
        }
    }

    fn apply_restriction(&mut self, record: ClipboardRestriction, reply: MessageKind) {
        self.cancel_disallowed();
        let barrier = self.target.restrict(record.content).and_then(|done| {
            done.recv_timeout(RESTRICTION_BACKEND_TIMEOUT)
                .map_err(|error| format!("clipboard backend restriction unconfirmed: {error}"))
        });
        if let Err(error) = barrier {
            eprintln!("jwm-remote: {error}; ending session");
            self.out.close();
            if let Some(wake) = &self.wake {
                wake();
            }
            return;
        }
        // The native fence, rather than an ordinary queue slot, abandoned the
        // removed receive offer. Keep the token until that fence actually wins.
        if self.receiving.is_none() {
            self.pending_token = None;
        }
        if let Some(staging) = self.staging.as_mut() {
            staging.reclaim_rejected();
        }
        self.queue(reply, record.encode().to_vec());
    }

    fn cancel_disallowed(&mut self) {
        let content = self.policy.content();
        if self.sending.as_ref().is_some_and(|s| match s.source {
            Source::Image(_) => content & 2 == 0,
            Source::Files(_) => content & 4 == 0,
        }) {
            self.sending = None;
        }
        // Do not compete with a saturated data queue for Cancel/Refuse slots.
        // The ordered policy record stops the peer, while final dispatch turns
        // already-counted payload into Skip. No credit is reset or discarded.
        if self.receiving.as_ref().is_some_and(|r| match r.sink {
            Sink::Image(_) => content & 2 == 0,
            Sink::Files(_) => content & 4 == 0,
        }) {
            // Do not enqueue Abandon into a saturated backend mailbox. Its
            // reserved fence clears all forbidden offers, including this token.
            // Retain pending_token for teardown if the fence cannot complete.
            self.receiving = None;
            if let Some(staging) = self.staging.as_mut() {
                staging.discard_in_progress();
            }
        }
    }

    fn queue(&self, kind: MessageKind, payload: Vec<u8>) {
        if self.out.send(Outgoing { kind, payload }).is_ok()
            && let Some(wake) = &self.wake
        {
            wake();
        }
    }

    fn queue_op(&self, op: &TransferOp) {
        self.queue(MessageKind::ClipboardTransfer, op.encode());
    }

    /// Something was copied on this machine.
    fn share_local(&mut self, copied: CopiedClipboard) {
        if !self.options.send
            || match &copied {
                CopiedClipboard::Text(_) => self.policy.content() & 1 == 0,
                CopiedClipboard::Png(_) => !self.options.images || self.policy.content() & 2 == 0,
                CopiedClipboard::Files(_) => {
                    !self.options.files
                        || self.options.file_limit == 0
                        || self.policy.content() & 4 == 0
                }
            }
        {
            return;
        }
        if self.last_received.as_ref() == Some(&Fingerprint::of(&copied)) {
            // The peer's own copy, re-offered here: it already has it.
            return;
        }
        // The newest copy wins: whatever was still travelling, either way,
        // is now stale.
        self.cancel_sending();
        self.drop_receiving(true);
        if self.out.closed() {
            return;
        }
        match copied {
            CopiedClipboard::Text(text) => match encode_clipboard(&text) {
                Ok(payload) => {
                    self.queue(MessageKind::Clipboard, payload.to_vec());
                    self.status.set(None);
                }
                Err(_) => eprintln!(
                    "jwm-remote: copied text is larger than {}; not shared",
                    human_bytes(MAX_CLIPBOARD_BYTES as u64)
                ),
            },
            CopiedClipboard::Png(png) => {
                let length = png.len() as u64;
                self.start_sending(TransferKind::Image, Source::Image(png), length, 0, 1);
            }
            CopiedClipboard::Files(paths) => self.share_files(&paths),
        }
    }

    fn share_files(&mut self, paths: &[PathBuf]) {
        match Manifest::build(paths, self.options.file_limit) {
            Ok(manifest) => {
                if manifest.skipped > 0 {
                    eprintln!(
                        "jwm-remote: {} copied entr{} left out (unreadable, a symlink inside a \
                         folder, or not a regular file)",
                        manifest.skipped,
                        if manifest.skipped == 1 {
                            "y is"
                        } else {
                            "ies are"
                        }
                    );
                }
                let length = manifest.stream_bytes;
                let entries = manifest.entry_count();
                let items = manifest.items;
                eprintln!(
                    "jwm-remote: sharing {} ({})",
                    item_label(TransferKind::Files, items),
                    human_bytes(manifest.file_bytes)
                );
                self.start_sending(
                    TransferKind::Files,
                    Source::Files(ManifestReader::new(manifest)),
                    length,
                    entries,
                    items,
                );
            }
            Err(error) => {
                eprintln!("jwm-remote: copied files not shared: {error}");
                self.show_outcome(format!("copy not shared: {}", short_reason(&error)));
            }
        }
    }

    fn start_sending(
        &mut self,
        kind: TransferKind,
        source: Source,
        length: u64,
        entries: u32,
        items: u32,
    ) {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.queue_op(&TransferOp::Begin {
            id,
            kind,
            length,
            entries,
            items,
        });
        self.sending = Some(Sending {
            id,
            source,
            length,
            start: self.sent_total,
            sent: 0,
            ended: false,
            label: item_label(kind, items),
            started: Instant::now(),
        });
        self.publish_progress();
    }

    /// Send as much of the outgoing transfer as the window allows.
    fn pump(&mut self) {
        self.cancel_disallowed();
        if let Some(staging) = self.staging.as_mut() {
            staging.reclaim_rejected();
        }
        let Some(sending) = self.sending.as_mut() else {
            return;
        };
        while !sending.ended && self.sent_total - self.acked_total < TRANSFER_WINDOW_BYTES {
            let room = (TRANSFER_WINDOW_BYTES - (self.sent_total - self.acked_total))
                .min(sending.length - sending.sent)
                .min(TRANSFER_CHUNK_BYTES as u64) as usize;
            if room == 0 {
                sending.ended = true;
                let id = sending.id;
                self.queue_op(&TransferOp::End { id });
                return;
            }
            let chunk = match &mut sending.source {
                Source::Image(png) => {
                    let start = sending.sent as usize;
                    Ok(png[start..start + room].to_vec())
                }
                Source::Files(reader) => reader.next_chunk(room),
            };
            match chunk {
                Ok(bytes) if !bytes.is_empty() => {
                    sending.sent += bytes.len() as u64;
                    let Some(total) = self.sent_total.checked_add(bytes.len() as u64) else {
                        self.out.close();
                        return;
                    };
                    self.sent_total = total;
                    let id = sending.id;
                    // Borrow ends before queueing.
                    let op = TransferOp::Data { id, bytes };
                    if self
                        .out
                        .send(Outgoing {
                            kind: MessageKind::ClipboardTransfer,
                            payload: op.encode(),
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(_) => {
                    eprintln!("jwm-remote: copied files changed while being shared; stopped");
                    self.abort_sending();
                    return;
                }
                Err(error) => {
                    eprintln!("jwm-remote: copied files could not be shared: {error}");
                    self.abort_sending();
                    return;
                }
            }
        }
        if let Some(wake) = &self.wake {
            wake();
        }
    }

    fn abort_sending(&mut self) {
        if let Some(sending) = self.sending.take() {
            self.queue_op(&TransferOp::Cancel { id: sending.id });
            self.show_outcome(format!("{} not shared", sending.label));
        }
    }

    /// A newer copy supersedes the outgoing transfer.
    fn cancel_sending(&mut self) {
        if let Some(sending) = self.sending.take()
            && !(sending.ended && self.acked_total >= sending.start.saturating_add(sending.length))
        {
            self.queue_op(&TransferOp::Cancel { id: sending.id });
        }
    }

    /// Stop receiving; `refuse` tells the sender to stop too.
    fn drop_receiving(&mut self, refuse: bool) {
        let Some(receiving) = self.receiving.as_ref() else {
            return;
        };
        if refuse {
            self.queue_op(&TransferOp::Refuse { id: receiving.id });
        }
        // Keep the token available to boundary cleanup if queueing's wake panics.
        let receiving = self.receiving.take().expect("receiving was present");
        let token = receiving.token;
        drop(receiving);
        if self.serve(RemoteServe::Abandon { token }) {
            self.pending_token = None;
        }
        if let Some(staging) = self.staging.as_mut() {
            staging.discard_in_progress();
        }
    }

    fn on_peer(&mut self, op: TransferOp) {
        self.cancel_disallowed();
        if self.options.validate_peer_op(&op).is_err() {
            return;
        }
        match op {
            TransferOp::Begin {
                id,
                kind,
                length,
                entries,
                items,
            } => {
                self.cancel_sending();
                self.drop_receiving(false);
                if self.out.closed() {
                    return;
                }
                // The peer has moved on, whether or not this copy is taken.
                self.last_received = None;
                match self.accept(kind, length, entries) {
                    Ok(sink) => {
                        let token = self.next_token;
                        self.next_token += 1;
                        self.receiving = Some(Receiving {
                            id,
                            token,
                            sink,
                            length,
                            received: 0,
                            label: item_label(kind, items),
                            started: Instant::now(),
                        });
                        // Record the pending token before invoking the target:
                        // even a panic after announcement can then abandon it.
                        self.pending_token = Some(token);
                        if !self.serve(RemoteServe::Announce {
                            token,
                            kind: match kind {
                                TransferKind::Image => PendingKind::Png,
                                TransferKind::Files => PendingKind::Files,
                            },
                        }) {
                            return;
                        }
                        self.publish_progress();
                    }
                    Err(reason) => {
                        eprintln!(
                            "jwm-remote: the other machine's copy was not accepted: {reason}"
                        );
                        self.show_outcome(format!("copy not accepted: {reason}"));
                        self.queue_op(&TransferOp::Refuse { id });
                    }
                }
            }
            TransferOp::Data { id, bytes } => {
                let len = bytes.len() as u64;
                let result = self.receive_chunk(id, &bytes);
                // Consumed or discarded, the window reopens either way.
                self.in_flight.fetch_sub(len, Ordering::AcqRel);
                let Some(total) = self.consumed_total.checked_add(len) else {
                    self.out.close();
                    return;
                };
                self.consumed_total = total;
                self.queue_op(&TransferOp::Ack {
                    id,
                    consumed: self.consumed_total,
                });
                if let Err(reason) = result {
                    eprintln!("jwm-remote: receiving the other machine's copy failed: {reason}");
                    self.show_outcome("copy failed".into());
                    self.drop_receiving(true);
                }
            }
            TransferOp::Skip { id, bytes } => {
                // A skipped byte was counted by the sender but contains no data.
                // Never feed a sink, accept a transfer or manufacture completion.
                self.in_flight.fetch_sub(bytes, Ordering::AcqRel);
                let Some(total) = self.consumed_total.checked_add(bytes) else {
                    self.out.close();
                    return;
                };
                self.consumed_total = total;
                if self.receiving.as_ref().is_some_and(|r| r.id == id) {
                    self.drop_receiving(true);
                }
                self.queue_op(&TransferOp::Ack {
                    id,
                    consumed: total,
                });
            }
            TransferOp::End { id } => self.finish_receiving(id),
            TransferOp::Cancel { id } => {
                if self.receiving.as_ref().is_some_and(|r| r.id == id) {
                    self.drop_receiving(false);
                    self.status.set(None);
                }
            }
            TransferOp::Ack { id, consumed } => {
                self.acked_total = consumed.min(self.sent_total).max(self.acked_total);
                let acked_total = self.acked_total;
                let Some(sending) = self.sending.as_mut().filter(|s| s.id == id) else {
                    return;
                };
                if sending.ended && acked_total >= sending.start.saturating_add(sending.length) {
                    eprintln!(
                        "jwm-remote: shared {} ({}) in {:.1} s",
                        sending.label,
                        human_bytes(sending.length),
                        sending.started.elapsed().as_secs_f64()
                    );
                    self.sending = None;
                    self.status.set(None);
                }
            }
            TransferOp::Refuse { id } => {
                if self.sending.as_ref().is_some_and(|s| s.id == id) {
                    self.sending = None;
                    self.status.set(None);
                }
            }
        }
    }

    fn accept(&mut self, kind: TransferKind, length: u64, entries: u32) -> Result<Sink, String> {
        if !self.options.receive {
            return Err("clipboard receive direction was not negotiated".into());
        }
        match kind {
            TransferKind::Image => {
                if !self.options.images || self.policy.content() & 2 == 0 {
                    return Err("image sharing was not negotiated".into());
                }
                if length == 0 || length > MAX_SHARED_IMAGE_BYTES as u64 {
                    return Err(format!(
                        "the image is larger than {}",
                        human_bytes(MAX_SHARED_IMAGE_BYTES as u64)
                    ));
                }
                Ok(Sink::Image(Vec::with_capacity(length as usize)))
            }
            TransferKind::Files => {
                if !self.options.files
                    || self.options.file_limit == 0
                    || self.policy.content() & 4 == 0
                {
                    return Err("file sharing was not negotiated".into());
                }
                // The stream carries headers too; its length bounds the
                // contents from above.
                if length
                    > self
                        .options
                        .file_limit
                        .saturating_add(u64::from(entries) * 4200)
                {
                    return Err(format!(
                        "it is larger than this machine's {} file limit",
                        human_bytes(self.options.file_limit)
                    ));
                }
                if entries == 0 || entries > super::clipboard_files::MAX_ENTRIES {
                    return Err("it holds too many files".into());
                }
                if self.staging.is_none() {
                    let staging = match &self.staging_root {
                        Some(root) => Staging::create_in(root),
                        None => Staging::create(),
                    };
                    self.staging =
                        Some(staging.map_err(|error| {
                            format!("no place to keep received files: {error}")
                        })?);
                }
                let staging = self.staging.as_mut().expect("created above");
                let available = staging.available_bytes().unwrap_or(u64::MAX);
                if available < length.saturating_add(RESERVED_FREE_BYTES) {
                    return Err(format!(
                        "only {} free for {}",
                        human_bytes(available),
                        human_bytes(length)
                    ));
                }
                let directory = staging
                    .begin(length)
                    .map_err(|error| format!("could not create a staging directory: {error}"))?;
                Ok(Sink::Files(
                    ArchiveReceiver::new(directory, entries)
                        .map_err(|error| format!("could not pin the staging directory: {error}"))?,
                ))
            }
        }
    }

    fn receive_chunk(&mut self, id: u32, bytes: &[u8]) -> Result<(), String> {
        let Some(receiving) = self.receiving.as_mut().filter(|r| r.id == id) else {
            // A transfer this end already dropped; its tail is discarded.
            return Ok(());
        };
        let len = bytes.len() as u64;
        let received = receiving
            .received
            .checked_add(len)
            .filter(|received| *received <= receiving.length)
            .ok_or_else(|| "the copy was longer than announced".to_owned())?;
        match &mut receiving.sink {
            Sink::Image(png) => png.extend_from_slice(bytes),
            Sink::Files(archive) => archive.feed(bytes).map_err(|error| error.to_string())?,
        }
        receiving.received = received;
        Ok(())
    }

    fn finish_receiving(&mut self, id: u32) {
        self.cancel_disallowed();
        let Some(receiving) = self.receiving.take_if(|r| r.id == id) else {
            return;
        };
        let elapsed = receiving.started.elapsed().as_secs_f64();
        let complete = receiving.received == receiving.length;
        let token = receiving.token;
        let served = match receiving.sink {
            Sink::Image(png) if complete => {
                match crate::backend::clipboard_offer::image_offer_to_png_within(
                    &png,
                    "image/png",
                    MAX_SHARED_IMAGE_BYTES,
                ) {
                    Some(png) => {
                        self.last_received = Some(Fingerprint::Png(png_hash(&png)));
                        Ok(RemoteServe::FulfilPng { token, png })
                    }
                    None => Err("the image was not a valid PNG".to_owned()),
                }
            }
            Sink::Files(archive) if complete => match archive.finish() {
                Ok(paths) => {
                    let request = RemoteServe::FulfilFiles {
                        token,
                        paths,
                        receipt: FileFulfilReceipt::default(),
                    };
                    if request.retained_bytes().is_none() {
                        Err("the received file list is too large".to_owned())
                    } else {
                        if let RemoteServe::FulfilFiles { paths, .. } = &request {
                            self.last_received = Some(Fingerprint::Files(paths.clone()));
                        }
                        Ok(request)
                    }
                }
                Err(error) => Err(error.to_string()),
            },
            _ => Err("the copy ended early".to_owned()),
        };
        match served {
            Ok(request) => {
                // Transfer ownership of the exact staging directory before the
                // callback: a panic or uncertain outcome must retain the files.
                if let RemoteServe::FulfilFiles { receipt, .. } = &request
                    && let Some(staging) = self.staging.as_mut()
                {
                    staging.complete_with_receipt(receipt.clone());
                }
                if !self.serve(request) {
                    return;
                }
                self.pending_token = None;
                eprintln!(
                    "jwm-remote: received {} ({}) in {elapsed:.1} s; clipboard update queued",
                    receiving.label,
                    human_bytes(receiving.length)
                );
                self.show_outcome(format!("{} clipboard update queued", receiving.label));
            }
            Err(reason) => {
                eprintln!("jwm-remote: the other machine's copy was not usable: {reason}");
                if self.serve(RemoteServe::Abandon { token }) {
                    self.pending_token = None;
                }
                if let Some(staging) = self.staging.as_mut() {
                    staging.discard_in_progress();
                }
                self.show_outcome("copy failed".into());
            }
        }
    }

    fn show_outcome(&mut self, line: String) {
        self.status.set(Some(line));
        self.status_expires = Some(Instant::now() + OUTCOME_SHOWN_FOR);
    }

    fn publish_progress(&self) {
        let line = if let Some(receiving) = &self.receiving {
            Some(format!(
                "receiving {} {}%",
                receiving.label,
                percent(receiving.received, receiving.length)
            ))
        } else {
            self.sending.as_ref().map(|sending| {
                let acked = self
                    .acked_total
                    .saturating_sub(sending.start)
                    .min(sending.length);
                format!(
                    "sending {} {}%",
                    sending.label,
                    percent(acked, sending.length)
                )
            })
        };
        if let Some(line) = line {
            self.status.set(Some(line));
        }
    }
}

fn percent(done: u64, total: u64) -> u64 {
    if total == 0 {
        100
    } else {
        (u128::from(done) * 100 / u128::from(total)) as u64
    }
}

fn item_label(kind: TransferKind, items: u32) -> String {
    match (kind, items) {
        (TransferKind::Image, _) => "image".to_owned(),
        (TransferKind::Files, 1) => "1 file".to_owned(),
        (TransferKind::Files, items) => format!("{items} files"),
    }
}

fn short_reason(error: &super::clipboard_files::ManifestError) -> &'static str {
    match error {
        super::clipboard_files::ManifestError::TooLarge { .. } => "too large",
        super::clipboard_files::ManifestError::TooManyEntries => "too many files",
        super::clipboard_files::ManifestError::Nothing => "unreadable",
    }
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Recorder(mpsc::Sender<RemoteServe>);

    impl ClipboardTarget for Recorder {
        fn serve(&self, request: RemoteServe) -> bool {
            self.0.send(request).is_ok()
        }
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "jwm-clipsync-{label}-{}-{:08x}",
                std::process::id(),
                rand::random::<u32>()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct End {
        commands: Arc<BoundedQueue<Command>>,
        join: Option<thread::JoinHandle<()>>,
        served: mpsc::Receiver<RemoteServe>,
        status: Arc<SyncStatus>,
        _staging: TempDir,
    }

    impl Drop for End {
        fn drop(&mut self) {
            let _ = self.commands.send(Command::Stop);
            if let Some(join) = self.join.take() {
                join.join().unwrap();
            }
        }
    }

    impl End {
        fn copy(&self, copied: CopiedClipboard) {
            self.commands.send(Command::Local(copied)).unwrap();
        }

        fn next_served(&self) -> RemoteServe {
            self.served
                .recv_timeout(Duration::from_secs(10))
                .expect("nothing reached the clipboard")
        }

        fn nothing_served_for(&self, wait: Duration) -> bool {
            self.served.recv_timeout(wait).is_err()
        }
    }

    /// Two ends joined by an in-memory wire. `per_data` slows each chunk.
    struct Pair {
        a: End,
        b: End,
        stop: Arc<AtomicBool>,
        wire: Option<thread::JoinHandle<RemoteResult<()>>>,
    }

    impl Drop for Pair {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(wire) = self.wire.take() {
                let _ = wire.join();
            }
        }
    }

    fn end(options: SyncOptions) -> (End, ClipboardInbox, ClipboardOutbox) {
        let staging = TempDir::new("stage");
        let (served_tx, served) = mpsc::channel();
        let (commands, join, inbox, outbox, status) = spawn_worker(
            Recorder(served_tx),
            options,
            Some(staging.0.join("remote-clipboard")),
            None,
        )
        .unwrap();
        (
            End {
                commands,
                join: Some(join),
                served,
                status,
                _staging: staging,
            },
            inbox,
            outbox,
        )
    }

    fn options(file_limit: u64) -> SyncOptions {
        SyncOptions {
            send: true,
            receive: true,
            images: true,
            files: true,
            file_limit,
            share_existing: false,
        }
    }

    fn worker_for_target<T: ClipboardTarget>(target: T, options: SyncOptions) -> Worker<T> {
        Worker {
            policy: RuntimeClipboardPolicy::new(options.content()),
            target,
            out: Arc::new(BoundedQueue::new(
                OUTGOING_QUEUE_RECORDS,
                OUTGOING_QUEUE_BYTES,
            )),
            wake: None,
            in_flight: Arc::new(AtomicU64::new(0)),
            status: Arc::new(SyncStatus::default()),
            options,
            staging_root: None,
            staging: None,
            next_id: 1,
            next_token: 1,
            sending: None,
            receiving: None,
            pending_token: None,
            sent_total: 0,
            acked_total: 0,
            consumed_total: 0,
            status_expires: None,
            last_received: None,
        }
    }

    fn policy_worker(options: SyncOptions) -> (Worker<Recorder>, mpsc::Receiver<RemoteServe>) {
        let (served, received) = mpsc::channel();
        (worker_for_target(Recorder(served), options), received)
    }

    #[test]
    fn staging_budget_refuses_before_announcement_and_reserves_before_acceptance() {
        use super::super::clipboard_staging_budget as budget;
        let temp = TempDir::new("budget-admission");
        let root = temp.0.join("staging");
        budget::set_limit(&root, 99).unwrap();
        let (mut worker, served) = policy_worker(options(1024));
        worker.staging_root = Some(root.clone());
        worker.on_peer(TransferOp::Begin {
            id: 1,
            kind: TransferKind::Files,
            length: 100,
            entries: 1,
            items: 1,
        });
        assert!(
            served.try_recv().is_err(),
            "rejected budget must never announce"
        );
        assert!(worker.receiving.is_none());
        let refusal = worker.out.try_recv().unwrap();
        assert_eq!(
            TransferOp::decode(&refusal.payload).unwrap(),
            TransferOp::Refuse { id: 1 }
        );
        assert_eq!(budget::status(&root).unwrap().reserved_bytes, 0);
        for entry in fs::read_dir(&root).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                assert_eq!(fs::read_dir(path).unwrap().count(), 0);
            }
        }
        budget::set_limit(&root, 100).unwrap();
        worker.on_peer(TransferOp::Begin {
            id: 2,
            kind: TransferKind::Files,
            length: 100,
            entries: 1,
            items: 1,
        });
        assert!(matches!(
            served.try_recv().unwrap(),
            RemoteServe::Announce { .. }
        ));
        assert_eq!(budget::status(&root).unwrap().reserved_bytes, 100);
        worker.on_peer(TransferOp::Cancel { id: 2 });
        assert_eq!(budget::status(&root).unwrap().reserved_bytes, 0);
    }

    #[test]
    fn staging_budget_completion_keeps_archive_charge_until_explicit_rejection() {
        use super::super::clipboard_staging_budget as budget;
        let temp = TempDir::new("budget-receipt");
        let source = temp.0.join("file");
        fs::write(&source, b"payload").unwrap();
        let manifest = Manifest::build(&[source], 1024).unwrap();
        let length = manifest.stream_bytes;
        let entries = manifest.entry_count();
        let bytes = ManifestReader::new(manifest).next_chunk(1024).unwrap();
        let root = temp.0.join("staging");
        budget::set_limit(&root, length).unwrap();
        let (mut worker, served) = policy_worker(options(1024));
        worker.staging_root = Some(root.clone());
        worker.on_peer(TransferOp::Begin {
            id: 1,
            kind: TransferKind::Files,
            length,
            entries,
            items: 1,
        });
        assert!(matches!(
            served.try_recv().unwrap(),
            RemoteServe::Announce { .. }
        ));
        worker
            .in_flight
            .store(bytes.len() as u64, Ordering::Release);
        worker.on_peer(TransferOp::Data { id: 1, bytes });
        worker.on_peer(TransferOp::End { id: 1 });
        let RemoteServe::FulfilFiles { paths, receipt, .. } = served.try_recv().unwrap() else {
            panic!("expected file handoff");
        };
        assert!(length > 7, "headers and names are part of the charge");
        assert_eq!(budget::status(&root).unwrap().reserved_bytes, length);
        worker.cleanup();
        assert!(
            paths[0].exists(),
            "unknown receipt remains possibly advertised"
        );
        assert_eq!(budget::status(&root).unwrap().reserved_bytes, length);
        receipt.reject_unpublished();
        worker.staging.as_mut().unwrap().reclaim_rejected();
        assert!(!paths[0].exists());
        assert_eq!(budget::status(&root).unwrap().reserved_bytes, 0);
    }

    #[test]
    fn ingress_policy_matrix_distinguishes_content_from_responses() {
        for send in [false, true] {
            for receive in [false, true] {
                for images in [false, true] {
                    for files in [false, true] {
                        let policy = SyncOptions {
                            send,
                            receive,
                            images,
                            files,
                            ..options(1024)
                        };
                        let transfer = images || files;
                        let operations = [
                            (
                                TransferOp::Begin {
                                    id: 1,
                                    kind: TransferKind::Image,
                                    length: 1,
                                    entries: 0,
                                    items: 1,
                                },
                                receive && images,
                            ),
                            (
                                TransferOp::Begin {
                                    id: 1,
                                    kind: TransferKind::Files,
                                    length: 1,
                                    entries: 1,
                                    items: 1,
                                },
                                receive && files,
                            ),
                            (
                                TransferOp::Data {
                                    id: 1,
                                    bytes: vec![1],
                                },
                                receive && transfer,
                            ),
                            (TransferOp::End { id: 1 }, receive && transfer),
                            (TransferOp::Cancel { id: 1 }, receive && transfer),
                            (TransferOp::Ack { id: 1, consumed: 1 }, send && transfer),
                            (TransferOp::Refuse { id: 1 }, send && transfer),
                        ];
                        let commands = Arc::new(BoundedQueue::new(
                            COMMAND_QUEUE_RECORDS,
                            COMMAND_QUEUE_BYTES,
                        ));
                        let inbox = ClipboardInbox {
                            control: ClipboardControl::new(
                                commands.clone(),
                                RuntimeClipboardPolicy::new(policy.content()),
                                policy.content(),
                            ),
                            commands: commands.clone(),
                            in_flight: Arc::new(AtomicU64::new(0)),
                            options: policy,
                        };
                        assert_eq!(
                            inbox.deliver(MessageKind::Clipboard, b"text").is_ok(),
                            receive
                        );
                        if receive {
                            assert!(matches!(commands.try_recv(), Ok(Command::PeerText(_))));
                        }
                        for (op, allowed) in operations {
                            assert_eq!(
                                inbox
                                    .deliver(MessageKind::ClipboardTransfer, &op.encode())
                                    .is_ok(),
                                allowed,
                                "{policy:?}: {op:?}"
                            );
                            if allowed {
                                assert!(matches!(commands.try_recv(), Ok(Command::Peer(_))));
                            } else {
                                assert!(matches!(
                                    commands.try_recv(),
                                    Err(mpsc::TryRecvError::Empty)
                                ));
                            }
                        }
                        assert_eq!(
                            inbox.in_flight.load(Ordering::Acquire),
                            u64::from(receive && transfer)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn forbidden_local_values_are_ignored_before_state_changes_or_file_reads() {
        for policy in [
            SyncOptions {
                send: false,
                ..options(1024)
            },
            SyncOptions {
                images: false,
                files: false,
                ..options(1024)
            },
        ] {
            let (mut worker, served) = policy_worker(policy);
            worker.last_received = Some(Fingerprint::Text("unchanged".into()));
            worker.share_local(CopiedClipboard::Png(vec![1, 2, 3]));
            worker.share_local(CopiedClipboard::Files(vec![PathBuf::from(
                "/must-not-read-clipboard-file",
            )]));
            if !policy.send {
                worker.share_local(CopiedClipboard::Text("private".into()));
            }
            assert!(
                worker.sending.is_none() && worker.receiving.is_none() && worker.staging.is_none()
            );
            assert_eq!(
                worker.last_received,
                Some(Fingerprint::Text("unchanged".into()))
            );
            assert!(matches!(
                worker.out.try_recv(),
                Err(mpsc::TryRecvError::Empty)
            ));
            assert!(served.try_recv().is_err());
        }
    }

    #[test]
    fn worker_defense_rejects_bypassed_peer_commands_without_serving_or_staging() {
        let (mut worker, served) = policy_worker(SyncOptions {
            receive: false,
            ..options(1024)
        });
        worker.apply(Command::PeerText("private".into()));
        for kind in [TransferKind::Image, TransferKind::Files] {
            worker.apply(Command::Peer(TransferOp::Begin {
                id: 1,
                kind,
                length: 1,
                entries: 1,
                items: 1,
            }));
            assert!(worker.accept(kind, 1, 1).is_err());
        }
        assert!(worker.receiving.is_none() && worker.staging.is_none());
        assert!(worker.last_received.is_none());
        assert!(served.try_recv().is_err());
        let (mut worker, served) = policy_worker(SyncOptions {
            images: false,
            files: false,
            ..options(1024)
        });
        for kind in [TransferKind::Image, TransferKind::Files] {
            worker.apply(Command::Peer(TransferOp::Begin {
                id: 1,
                kind,
                length: 1,
                entries: 1,
                items: 1,
            }));
            assert!(worker.accept(kind, 1, 1).is_err());
        }
        assert!(worker.receiving.is_none() && worker.staging.is_none());
        assert!(served.try_recv().is_err());
    }

    #[test]
    fn one_way_images_finish_with_reverse_acknowledgments() {
        for reverse in [false, true] {
            let sender = SyncOptions {
                receive: false,
                ..options(0)
            };
            let receiver = SyncOptions {
                send: false,
                ..options(0)
            };
            let pair = if reverse {
                pair(receiver, sender, Duration::ZERO)
            } else {
                pair(sender, receiver, Duration::ZERO)
            };
            let (source, target) = if reverse {
                (&pair.b, &pair.a)
            } else {
                (&pair.a, &pair.b)
            };
            let image = png(12);
            source.copy(CopiedClipboard::Png(image.clone()));
            assert!(matches!(
                target.next_served(),
                RemoteServe::Announce {
                    kind: PendingKind::Png,
                    ..
                }
            ));
            assert!(
                matches!(target.next_served(), RemoteServe::FulfilPng { png, .. } if png == image)
            );
            source.copy(CopiedClipboard::Text("after transfer".into()));
            assert!(
                matches!(target.next_served(), RemoteServe::Text(text) if text == "after transfer")
            );
            assert!(source.served.try_recv().is_err());
        }
    }

    #[test]
    fn one_way_files_cross_flow_window_in_both_directions() {
        let source_dir = TempDir::new("one-way-source");
        let source_path = source_dir.0.join("fixture.bin");
        let contents = vec![42; TRANSFER_WINDOW_BYTES as usize + TRANSFER_CHUNK_BYTES];
        fs::write(&source_path, &contents).unwrap();
        for reverse in [false, true] {
            let sender = SyncOptions {
                receive: false,
                ..options(DEFAULT_FILE_LIMIT)
            };
            let receiver = SyncOptions {
                send: false,
                ..options(DEFAULT_FILE_LIMIT)
            };
            let pair = if reverse {
                pair(receiver, sender, Duration::ZERO)
            } else {
                pair(sender, receiver, Duration::ZERO)
            };
            let (source, target) = if reverse {
                (&pair.b, &pair.a)
            } else {
                (&pair.a, &pair.b)
            };
            source.copy(CopiedClipboard::Files(vec![source_path.clone()]));
            assert!(matches!(
                target.next_served(),
                RemoteServe::Announce {
                    kind: PendingKind::Files,
                    ..
                }
            ));
            let RemoteServe::FulfilFiles { paths, .. } = target.next_served() else {
                panic!("one-way file did not finish");
            };
            assert_eq!(paths.len(), 1);
            assert_eq!(fs::read(&paths[0]).unwrap(), contents);
            assert!(source.served.try_recv().is_err());
        }
    }

    fn pair(a: SyncOptions, b: SyncOptions, per_data: Duration) -> Pair {
        let (a, a_inbox, a_outbox) = end(a);
        let (b, b_inbox, b_outbox) = end(b);
        let stop = Arc::new(AtomicBool::new(false));
        let wire_stop = Arc::clone(&stop);
        let wire = thread::spawn(move || -> RemoteResult<()> {
            while !wire_stop.load(Ordering::Acquire) {
                let mut moved = false;
                for (outbox, inbox) in [(&a_outbox, &b_inbox), (&b_outbox, &a_inbox)] {
                    while let Ok(Some(record)) = outbox.try_next() {
                        moved = true;
                        if record.kind == MessageKind::ClipboardTransfer
                            && record.payload[0] == OP_DATA
                        {
                            thread::sleep(per_data);
                        }
                        inbox.deliver(record.kind, &record.payload)?;
                    }
                }
                if !moved {
                    thread::sleep(Duration::from_millis(1));
                }
            }
            Ok(())
        });
        Pair {
            a,
            b,
            stop,
            wire: Some(wire),
        }
    }

    fn tree(root: &TempDir) -> PathBuf {
        let folder = root.0.join("photos");
        fs::create_dir_all(folder.join("2026")).unwrap();
        let big: Vec<u8> = (0..3_000_000_u32).map(|n| (n * 7 % 256) as u8).collect();
        fs::write(folder.join("2026/big.raw"), &big).unwrap();
        fs::write(folder.join("notes.txt"), "旅行 notes").unwrap();
        folder
    }

    fn png(width: u32) -> Vec<u8> {
        let image = image::RgbImage::from_pixel(width, 3, image::Rgb([10, 200, 30]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    }

    #[test]
    #[ignore = "requires an isolated X11 server in JWM_TEST_X11_DISPLAY"]
    fn remote_clipboard_shutdown_and_clones_on_x11() {
        // Never fall back to DISPLAY: this test owns and changes the clipboard.
        let display = std::env::var("JWM_TEST_X11_DISPLAY")
            .expect("set JWM_TEST_X11_DISPLAY to an isolated Xvfb server");
        assert!(
            !display.trim().is_empty(),
            "an explicit isolated display is required"
        );
        let timeout = Duration::from_secs(2);

        // Exercise the public session path repeatedly, including actual backend
        // join ownership, not only the synthetic thread fixtures.
        for _ in 0..3 {
            let (mut sync, inbox, outbox) = ClipboardSync::start(
                Some(&display),
                SyncOptions {
                    send: true,
                    receive: true,
                    images: true,
                    files: false,
                    file_limit: 0,
                    share_existing: false,
                },
                None,
            )
            .unwrap();
            assert!(sync.backend.worker.is_some());
            inbox
                .deliver(MessageKind::Clipboard, b"isolated clipboard shutdown test")
                .unwrap();
            sync.shutdown_timeout(timeout).unwrap();
            for worker in [&sync.worker, &sync.forwarder, &sync.backend] {
                assert!(worker.worker.is_none(), "join ownership must be consumed");
                assert!(
                    worker.joining.is_none(),
                    "shutdown must finish the actual join"
                );
                assert!(worker.failure.is_none());
            }
            assert!(
                inbox
                    .deliver(MessageKind::Clipboard, b"after shutdown")
                    .is_err()
            );
            assert!(outbox.try_next().is_err());
            sync.shutdown_timeout(Duration::ZERO).unwrap();
        }

        for retain_clone_until_join in [true, false] {
            let (captures, setter) = RemoteClipboard::start(
                Some(&display),
                RemoteClipboardOptions {
                    send: true,
                    images: true,
                    files: false,
                    image_bytes: MAX_SHARED_IMAGE_BYTES,
                    capture_existing: false,
                },
            )
            .unwrap();
            let retained = setter.clone();
            let (stop, worker) = setter.take_shutdown();
            let mut backend = ClipboardThread::new(worker);
            drop(setter);
            drop(captures);
            assert!(
                retained.send(RemoteServe::Text("isolated cloned setter".into())),
                "dropping other handles must not stop a retained setter"
            );
            if retain_clone_until_join {
                // Explicit shutdown must finish even while a setter clone keeps
                // the shared lifetime alive. A clone cannot resurrect the worker.
                stop.stop();
                backend
                    .join_until(Instant::now() + timeout, "clipboard X11 worker")
                    .unwrap();
                assert!(!retained.send(RemoteServe::Text("after joined shutdown".into())));
                drop(retained);
            } else {
                // The final lifetime drop signals shutdown but no longer owns
                // the join. The transferred handle must still observe full exit.
                drop(retained);
                backend
                    .join_until(Instant::now() + timeout, "clipboard X11 worker")
                    .unwrap();
            }
            assert!(backend.worker.is_none());
            assert!(backend.joining.is_none());
            backend
                .join_until(Instant::now(), "clipboard X11 worker")
                .unwrap();
            drop(stop);
        }
    }

    fn sync_for_test(worker: thread::JoinHandle<()>) -> ClipboardSync {
        ClipboardSync {
            commands: Arc::new(BoundedQueue::new(
                COMMAND_QUEUE_RECORDS,
                COMMAND_QUEUE_BYTES,
            )),
            stop: Arc::new(AtomicBool::new(false)),
            status: Arc::new(SyncStatus::default()),
            worker: ClipboardThread::new(worker),
            forwarder: ClipboardThread::default(),
            backend: ClipboardThread::default(),
            backend_stop: None,
        }
    }

    #[test]
    fn clipboard_shutdown_timeout_retains_owned_file_until_worker_exits() {
        let temp = TempDir::new("shutdown-owned-file");
        let path = temp.0.join("partial");
        fs::write(&path, b"partial data").unwrap();
        let file = fs::File::open(&path).unwrap();
        let (release, gate) = mpsc::channel();
        let (entered_tx, entered) = mpsc::channel();
        let (done_tx, done) = mpsc::channel();
        let worker = thread::spawn(move || {
            // A deterministic gate models stalled file work without a pipe,
            // socket, device, real clipboard or filesystem that can hang.
            entered_tx.send(()).unwrap();
            gate.recv().unwrap();
            use std::io::Read;
            let mut bytes = Vec::new();
            let mut file = file;
            file.read_to_end(&mut bytes).unwrap();
            done_tx.send(bytes).unwrap();
        });
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut sync = sync_for_test(worker);
        let start = Instant::now();
        let error = sync
            .shutdown_timeout(Duration::from_millis(10))
            .unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(error.to_string().contains("cleanup incomplete"));
        assert!(sync.worker.worker.is_none());
        assert!(sync.worker.joining.is_some());
        assert!(sync.commands.closed());
        assert!(path.exists());
        assert!(sync.shutdown_timeout(Duration::ZERO).is_err());
        release.send(()).unwrap();
        sync.shutdown_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(done.recv().unwrap(), b"partial data");
        sync.shutdown_timeout(Duration::ZERO).unwrap();
    }

    #[test]
    fn clipboard_shutdown_waits_for_tls_and_reports_panic_persistently() {
        struct TlsGate(mpsc::Sender<()>, mpsc::Receiver<()>);
        impl Drop for TlsGate {
            fn drop(&mut self) {
                self.0.send(()).unwrap();
                self.1.recv().unwrap();
            }
        }
        thread_local! {
            static GATE: std::cell::RefCell<Option<TlsGate>> = const { std::cell::RefCell::new(None) };
        }
        let (entered_tx, entered) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let worker = thread::spawn(move || {
            GATE.with(|slot| {
                *slot.borrow_mut() = Some(TlsGate(entered_tx, gate));
            })
        });
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut sync = sync_for_test(worker);
        assert!(sync.shutdown_timeout(Duration::from_millis(10)).is_err());
        release.send(()).unwrap();
        sync.shutdown_timeout(Duration::from_secs(2)).unwrap();

        let mut sync = sync_for_test(thread::spawn(|| panic!("clipboard worker panic")));
        let first = sync
            .shutdown_timeout(Duration::from_secs(2))
            .unwrap_err()
            .to_string();
        assert!(first.contains("panicked"));
        assert_eq!(
            sync.shutdown_timeout(Duration::ZERO)
                .unwrap_err()
                .to_string(),
            first
        );
    }

    #[test]
    fn clipboard_shutdown_reports_each_stalled_thread_and_later_reaps_all() {
        let mut releases = Vec::new();
        let mut spawn_gated = || {
            let (release, gate) = mpsc::channel();
            releases.push(release);
            ClipboardThread::new(thread::spawn(move || {
                gate.recv().unwrap();
            }))
        };
        let mut sync = sync_for_test(thread::spawn(|| {}));
        sync.forwarder = spawn_gated();
        sync.backend = spawn_gated();
        let error = sync
            .shutdown_timeout(Duration::from_millis(10))
            .unwrap_err()
            .to_string();
        assert!(error.contains("clipboard capture forwarder shutdown timed out"));
        assert!(error.contains("clipboard X11 worker shutdown timed out"));
        for release in releases {
            release.send(()).unwrap();
        }
        sync.shutdown_timeout(Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn explicit_clipboard_shutdown_abandons_pending_transfer() {
        let (mut end, inbox, _outbox) = end(options(u64::MAX));
        inbox
            .deliver(
                MessageKind::ClipboardTransfer,
                &TransferOp::Begin {
                    id: 1,
                    kind: TransferKind::Image,
                    length: 100,
                    entries: 0,
                    items: 1,
                }
                .encode(),
            )
            .unwrap();
        let RemoteServe::Announce { token, .. } = end.next_served() else {
            panic!("expected transfer announcement");
        };
        let mut sync = sync_for_test(end.join.take().unwrap());
        sync.commands = Arc::clone(&end.commands);
        sync.shutdown_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            matches!(end.next_served(), RemoteServe::Abandon { token: abandoned } if abandoned == token)
        );
        assert!(sync.commands.closed());
    }

    #[test]
    fn clipboard_drop_never_waits_for_a_gated_worker() {
        let (release, gate) = mpsc::channel();
        let (done_tx, done) = mpsc::channel();
        let sync = sync_for_test(thread::spawn(move || {
            gate.recv().unwrap();
            done_tx.send(()).unwrap();
        }));
        let start = Instant::now();
        drop(sync);
        assert!(start.elapsed() < Duration::from_secs(1));
        release.send(()).unwrap();
        done.recv_timeout(Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn panicking_target_closes_queues_and_wakes_even_if_abandon_panics() {
        struct PanickingTarget(mpsc::Sender<RemoteServe>);
        impl ClipboardTarget for PanickingTarget {
            fn serve(&self, request: RemoteServe) -> bool {
                self.0.send(request).unwrap();
                panic!("target callback failed");
            }
        }
        let (served_tx, served) = mpsc::channel();
        let (woken_tx, woken) = mpsc::channel();
        let (commands, join, inbox, outbox, _) = spawn_worker(
            PanickingTarget(served_tx),
            options(u64::MAX),
            None,
            Some(Box::new(move || {
                woken_tx.send(()).unwrap();
            })),
        )
        .unwrap();
        inbox
            .deliver(
                MessageKind::ClipboardTransfer,
                &TransferOp::Begin {
                    id: 1,
                    kind: TransferKind::Image,
                    length: 1,
                    entries: 0,
                    items: 1,
                }
                .encode(),
            )
            .unwrap();
        woken
            .recv_timeout(Duration::from_secs(2))
            .expect("panic exit must wake writer");
        assert!(join.join().is_err());
        assert!(commands.closed());
        assert!(outbox.next_timeout(Duration::from_secs(2)).is_err());
        assert!(
            inbox
                .deliver(MessageKind::Clipboard, b"after panic")
                .is_err()
        );
        let RemoteServe::Announce { token, .. } = served.try_recv().unwrap() else {
            panic!("expected announcement");
        };
        assert!(
            matches!(served.try_recv().unwrap(), RemoteServe::Abandon { token: abandoned } if abandoned == token)
        );
    }

    #[test]
    fn panicking_wake_disconnects_and_abandons_pending_receive() {
        let (served_tx, served) = mpsc::channel();
        let (woken_tx, woken) = mpsc::channel();
        let (commands, join, inbox, outbox, _) = spawn_worker(
            Recorder(served_tx),
            options(u64::MAX),
            None,
            Some(Box::new(move || {
                woken_tx.send(()).unwrap();
                panic!("wake callback failed");
            })),
        )
        .unwrap();
        inbox
            .deliver(
                MessageKind::ClipboardTransfer,
                &TransferOp::Begin {
                    id: 1,
                    kind: TransferKind::Image,
                    length: 2,
                    entries: 0,
                    items: 1,
                }
                .encode(),
            )
            .unwrap();
        let RemoteServe::Announce { token, .. } =
            served.recv_timeout(Duration::from_secs(2)).unwrap()
        else {
            panic!("expected announcement");
        };
        inbox
            .deliver(
                MessageKind::ClipboardTransfer,
                &TransferOp::Data {
                    id: 1,
                    bytes: vec![0],
                }
                .encode(),
            )
            .unwrap();
        // First wake panics while queueing the ACK. The exit wake also panics,
        // but both queue closure and pending-paste abandonment must still happen.
        for _ in 0..2 {
            woken.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        assert!(join.join().is_err());
        assert!(commands.closed());
        assert!(outbox.try_next().is_err());
        assert!(
            matches!(served.try_recv().unwrap(), RemoteServe::Abandon { token: abandoned } if abandoned == token)
        );
    }

    #[test]
    fn normal_worker_stop_also_wakes_the_writer() {
        let (served_tx, _served) = mpsc::channel();
        let (woken_tx, woken) = mpsc::channel();
        let (commands, join, _inbox, outbox, _) = spawn_worker(
            Recorder(served_tx),
            options(u64::MAX),
            None,
            Some(Box::new(move || {
                woken_tx.send(()).unwrap();
            })),
        )
        .unwrap();
        commands.send(Command::Stop).unwrap();
        woken.recv_timeout(Duration::from_secs(2)).unwrap();
        join.join().unwrap();
        assert!(outbox.try_next().is_err());
    }

    #[derive(Clone, Copy, Debug)]
    enum RejectedServe {
        Text,
        Announce,
        Fulfil,
        Abandon,
    }

    struct RejectingTarget {
        rejected: RejectedServe,
        panic: bool,
        calls: Arc<Mutex<Vec<RemoteServe>>>,
    }

    impl ClipboardTarget for RejectingTarget {
        fn serve(&self, request: RemoteServe) -> bool {
            let reject = matches!(
                (&request, self.rejected),
                (RemoteServe::Text(_), RejectedServe::Text)
                    | (RemoteServe::Announce { .. }, RejectedServe::Announce)
                    | (
                        RemoteServe::FulfilFiles { .. } | RemoteServe::FulfilPng { .. },
                        RejectedServe::Fulfil
                    )
                    | (RemoteServe::Abandon { .. }, RejectedServe::Abandon)
            );
            if reject && !self.panic {
                request.reject_unpublished();
            }
            self.calls.lock().unwrap().push(request);
            assert!(!(reject && self.panic), "injected target panic");
            !reject
        }
    }

    #[test]
    fn rejected_or_panicking_serve_closes_session_and_cleans_uncommitted_files() {
        for rejected in [
            RejectedServe::Text,
            RejectedServe::Announce,
            RejectedServe::Fulfil,
            RejectedServe::Abandon,
        ] {
            for panic in [false, true] {
                let temp = TempDir::new("rejected-target");
                let source = temp.0.join("fixture.txt");
                fs::write(&source, b"fixture").unwrap();
                let manifest = Manifest::build(&[source], 100).unwrap();
                let entries = manifest.entry_count();
                let length = manifest.stream_bytes;
                let bytes = ManifestReader::new(manifest).next_chunk(1024).unwrap();
                let calls = Arc::new(Mutex::new(Vec::new()));
                let wake_count = Arc::new(AtomicU64::new(0));
                let count = wake_count.clone();
                let mut worker = worker_for_target(
                    RejectingTarget {
                        rejected,
                        panic,
                        calls: calls.clone(),
                    },
                    options(100),
                );
                let staging_root = temp.0.join("staging");
                worker.staging_root = Some(staging_root.clone());
                worker.wake = Some(Box::new(move || {
                    count.fetch_add(1, Ordering::Relaxed);
                }));
                let commands = BoundedQueue::<Command>::new(10, 4096);
                if matches!(rejected, RejectedServe::Text) {
                    commands.send(Command::PeerText("text".into())).unwrap();
                } else {
                    commands
                        .send(Command::Peer(TransferOp::Begin {
                            id: 1,
                            kind: TransferKind::Files,
                            length,
                            entries,
                            items: 1,
                        }))
                        .unwrap();
                    if matches!(rejected, RejectedServe::Abandon) {
                        commands
                            .send(Command::Peer(TransferOp::Cancel { id: 1 }))
                            .unwrap();
                    } else {
                        worker
                            .in_flight
                            .store(bytes.len() as u64, Ordering::Release);
                        commands
                            .send(Command::Peer(TransferOp::Data { id: 1, bytes }))
                            .unwrap();
                        commands
                            .send(Command::Peer(TransferOp::End { id: 1 }))
                            .unwrap();
                    }
                }
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    worker.run_guarded(&commands)
                }));
                assert_eq!(result.is_err(), panic, "{rejected:?}");
                assert!(commands.closed() && worker.out.closed());
                assert!(worker.pending_token.is_none());
                assert!(wake_count.load(Ordering::Relaxed) > 0);
                assert!(
                    !worker
                        .status
                        .line
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|line| line.contains("clipboard update queued"))
                );
                if !matches!(rejected, RejectedServe::Text) {
                    assert!(
                        calls
                            .lock()
                            .unwrap()
                            .iter()
                            .any(|r| matches!(r, RemoteServe::Abandon { token: 1 }))
                    );
                    for session in fs::read_dir(&staging_root).unwrap() {
                        let path = session.unwrap().path();
                        if path.is_dir() {
                            assert_eq!(
                                fs::read_dir(path).unwrap().count(),
                                usize::from(panic && matches!(rejected, RejectedServe::Fulfil))
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn admitted_file_fulfil_survives_later_backend_failure() {
        struct LaterAbort {
            calls: mpsc::Sender<RemoteServe>,
            failed: Arc<AtomicBool>,
        }
        impl ClipboardTarget for LaterAbort {
            fn serve(&self, request: RemoteServe) -> bool {
                !self.failed.load(Ordering::Acquire) && self.calls.send(request).is_ok()
            }
        }
        for abort in [false, true] {
            let temp = TempDir::new("admitted-fulfil");
            let source = temp.0.join("fixture.txt");
            fs::write(&source, b"fixture").unwrap();
            let manifest = Manifest::build(&[source], 100).unwrap();
            let entries = manifest.entry_count();
            let length = manifest.stream_bytes;
            let bytes = ManifestReader::new(manifest).next_chunk(1024).unwrap();
            let failed = Arc::new(AtomicBool::new(false));
            let (calls, received) = mpsc::channel();
            let mut worker = worker_for_target(
                LaterAbort {
                    calls,
                    failed: failed.clone(),
                },
                options(100),
            );
            worker.staging_root = Some(temp.0.join("staging"));
            worker.on_peer(TransferOp::Begin {
                id: 1,
                kind: TransferKind::Files,
                length,
                entries,
                items: 1,
            });
            assert!(matches!(
                received.try_recv().unwrap(),
                RemoteServe::Announce { .. }
            ));
            worker.receive_chunk(1, &bytes).unwrap();
            worker.finish_receiving(1);
            let RemoteServe::FulfilFiles { paths, receipt, .. } = received.try_recv().unwrap()
            else {
                panic!("expected fulfil admission")
            };
            receipt.accepted_or_may_publish();
            assert!(paths[0].exists());
            failed.store(abort, Ordering::Release);
            worker.cleanup();
            drop(worker);
            assert!(
                paths[0].exists(),
                "backend failure does not prove paths unpublished"
            );
        }
    }

    #[test]
    fn normal_shutdown_rejection_preserves_completed_handoff_files() {
        struct ClosedNormally;
        impl ClipboardTarget for ClosedNormally {
            fn serve(&self, _: RemoteServe) -> bool {
                false
            }
        }
        let temp = TempDir::new("normal-close-staging");
        let mut staging = Staging::create_in(&temp.0.join("staging")).unwrap();
        let completed = staging.begin(1024).unwrap().join("complete.txt");
        fs::write(&completed, b"complete").unwrap();
        staging.complete();
        let incomplete = staging.begin(1024).unwrap().join("incomplete.txt");
        fs::write(&incomplete, b"incomplete").unwrap();
        let mut worker = worker_for_target(ClosedNormally, options(100));
        worker.staging = Some(staging);
        assert!(!worker.serve(RemoteServe::FulfilFiles {
            token: 2,
            paths: vec![incomplete.clone()],
            receipt: FileFulfilReceipt::default()
        }));
        worker.cleanup();
        drop(worker);
        assert!(
            completed.exists(),
            "normal handoff may still serve completed paths"
        );
        assert!(!incomplete.exists());
    }

    #[test]
    fn full_outbox_cancel_fails_session_instead_of_losing_control_record() {
        let (mut worker, _) = policy_worker(options(100));
        worker.out = Arc::new(BoundedQueue::new(1, 32));
        worker
            .out
            .send(Outgoing {
                kind: MessageKind::Clipboard,
                payload: vec![],
            })
            .unwrap();
        worker.sending = Some(Sending {
            id: 7,
            source: Source::Image(vec![1]),
            length: 1,
            start: 0,
            sent: 0,
            ended: false,
            label: "image".into(),
            started: Instant::now(),
        });
        let wakes = Arc::new(AtomicU64::new(0));
        let count = wakes.clone();
        worker.wake = Some(Box::new(move || {
            count.fetch_add(1, Ordering::Relaxed);
        }));
        worker.cancel_sending();
        assert!(worker.sending.is_none());
        assert!(worker.out.closed());
        let commands = BoundedQueue::new(1, 32);
        worker.run_guarded(&commands);
        assert!(commands.closed());
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn command_and_outgoing_limits_charge_retained_capacity() {
        let queue = BoundedQueue::<Command>::new(4, 32);
        let mut text = String::with_capacity(128);
        text.push('x');
        assert!(queue.send(Command::PeerText(text)).is_err());
        let queue = BoundedQueue::<Command>::new(4, 32);
        let paths = Vec::<PathBuf>::with_capacity(100);
        assert!(
            queue
                .send(Command::Local(CopiedClipboard::Files(paths)))
                .is_err()
        );
        let queue = BoundedQueue::<Outgoing>::new(4, 32);
        assert!(
            queue
                .send(Outgoing {
                    kind: MessageKind::Clipboard,
                    payload: Vec::with_capacity(128)
                })
                .is_err()
        );
    }

    #[test]
    fn queues_bound_bytes_and_records_and_fail_closed() {
        let queue = BoundedQueue::new(2, 5);
        queue.push(1, 3, |_| false).unwrap();
        queue.push(2, 2, |_| false).unwrap();
        assert!(queue.push(3, 0, |_| false).is_err());
        assert_eq!(queue.try_recv(), Err(mpsc::TryRecvError::Disconnected));
        let queue = BoundedQueue::new(10, 5);
        queue.push(1, 3, |_| false).unwrap();
        assert!(queue.push(2, 3, |_| false).is_err());
        assert!(queue.closed());
    }

    #[test]
    fn pending_local_copies_are_latest_only_and_stop_bypasses_capacity() {
        let queue = BoundedQueue::<Command>::new(1, 10);
        for n in 0..1000 {
            queue
                .send(Command::Local(CopiedClipboard::Text(n.to_string())))
                .unwrap();
        }
        let Command::Local(CopiedClipboard::Text(text)) = queue.try_recv().unwrap() else {
            panic!("latest copy expected");
        };
        assert_eq!(text, "999");
        queue.send(Command::PeerText("full".into())).unwrap();
        queue.send(Command::Stop).unwrap();
        assert!(matches!(
            queue.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn cumulative_ack_coalescing_preserves_other_record_order() {
        let queue = BoundedQueue::<Outgoing>::new(3, 100);
        let ack = |id, consumed| Outgoing {
            kind: MessageKind::ClipboardTransfer,
            payload: TransferOp::Ack { id, consumed }.encode(),
        };
        queue.send(ack(1, 10)).unwrap();
        queue
            .send(Outgoing {
                kind: MessageKind::Clipboard,
                payload: b"text".to_vec(),
            })
            .unwrap();
        queue.send(ack(1, 20)).unwrap();
        queue.send(ack(2, 30)).unwrap();
        assert_eq!(queue.try_recv().unwrap().kind, MessageKind::Clipboard);
        assert_eq!(queue.try_recv().unwrap(), ack(1, 20));
        assert_eq!(queue.try_recv().unwrap(), ack(2, 30));
        assert!(matches!(queue.try_recv(), Err(mpsc::TryRecvError::Empty)));
    }

    #[test]
    fn overloaded_ingress_returns_without_waiting_and_rolls_back_bytes() {
        let commands = Arc::new(BoundedQueue::new(1, TRANSFER_CHUNK_BYTES));
        let inbox = ClipboardInbox {
            control: ClipboardControl::new(commands.clone(), RuntimeClipboardPolicy::new(7), 7),
            commands,
            in_flight: Arc::new(AtomicU64::new(0)),
            options: options(DEFAULT_FILE_LIMIT),
        };
        inbox.deliver(MessageKind::Clipboard, b"text").unwrap();
        let data = TransferOp::Data {
            id: 1,
            bytes: vec![1; TRANSFER_CHUNK_BYTES],
        }
        .encode();
        assert!(
            inbox
                .deliver(MessageKind::ClipboardTransfer, &data)
                .is_err()
        );
        assert_eq!(inbox.in_flight.load(Ordering::Acquire), 0);
        assert!(inbox.commands.closed());
    }

    #[test]
    fn a_full_outbox_does_not_block_worker_shutdown() {
        let (mut end, _inbox, outbox) = end(options(u64::MAX));
        for _ in 0..OUTGOING_QUEUE_RECORDS {
            outbox
                .records
                .send(Outgoing {
                    kind: MessageKind::Clipboard,
                    payload: vec![],
                })
                .unwrap();
        }
        end.copy(CopiedClipboard::Text("overflow".into()));
        let join = end.join.take().unwrap();
        let (done, finished) = mpsc::channel();
        thread::spawn(move || {
            join.join().unwrap();
            done.send(()).unwrap();
        });
        finished
            .recv_timeout(Duration::from_secs(2))
            .expect("worker must not block on a full outbox");
        assert!(outbox.try_next().is_err());
    }

    #[test]
    fn transfer_ops_round_trip_and_malformed_records_are_rejected() {
        for op in [
            TransferOp::Begin {
                id: 7,
                kind: TransferKind::Files,
                length: 5 << 30,
                entries: 12,
                items: 3,
            },
            TransferOp::Begin {
                id: u32::MAX,
                kind: TransferKind::Image,
                length: 1,
                entries: 0,
                items: 1,
            },
            TransferOp::Data {
                id: 1,
                bytes: vec![9; TRANSFER_CHUNK_BYTES],
            },
            TransferOp::End { id: 2 },
            TransferOp::Cancel { id: 3 },
            TransferOp::Ack {
                id: 4,
                consumed: u64::MAX,
            },
            TransferOp::Refuse { id: 5 },
        ] {
            assert_eq!(TransferOp::decode(&op.encode()).unwrap(), op);
        }
        assert!(
            MAX_TRANSFER_RECORD_BYTES
                >= TransferOp::Data {
                    id: 0,
                    bytes: vec![0; TRANSFER_CHUNK_BYTES]
                }
                .encode()
                .len()
        );
        for bad in [
            &[][..],
            &[OP_END, 0, 0, 0],
            &[OP_END, 0, 0, 0, 1, 0],
            &[OP_DATA, 0, 0, 0, 1],
            &[OP_ACK, 0, 0, 0, 1, 0, 0],
            &[
                OP_BEGIN, 0, 0, 0, 1, 9, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1,
            ],
            &[0x7f, 0, 0, 0, 1],
        ] {
            assert!(TransferOp::decode(bad).is_err(), "{bad:?}");
        }
        let oversized = TransferOp::Data {
            id: 1,
            bytes: vec![0; TRANSFER_CHUNK_BYTES + 1],
        };
        assert!(TransferOp::decode(&oversized.encode()).is_err());
    }

    #[test]
    fn the_inbox_holds_a_peer_to_its_promises() {
        let commands = Arc::new(BoundedQueue::new(
            COMMAND_QUEUE_RECORDS,
            COMMAND_QUEUE_BYTES,
        ));
        let inbox = ClipboardInbox {
            control: ClipboardControl::new(commands.clone(), RuntimeClipboardPolicy::new(7), 7),
            commands,
            in_flight: Arc::new(AtomicU64::new(0)),
            options: SyncOptions {
                files: false,
                ..options(0)
            },
        };
        let begin = TransferOp::Begin {
            id: 1,
            kind: TransferKind::Files,
            length: 10,
            entries: 1,
            items: 1,
        };
        assert!(
            inbox
                .deliver(MessageKind::ClipboardTransfer, &begin.encode())
                .is_err(),
            "files were not negotiated"
        );
        let chunk = TransferOp::Data {
            id: 1,
            bytes: vec![0; TRANSFER_CHUNK_BYTES],
        }
        .encode();
        let window_chunks = TRANSFER_WINDOW_BYTES / TRANSFER_CHUNK_BYTES as u64;
        for _ in 0..window_chunks {
            inbox
                .deliver(MessageKind::ClipboardTransfer, &chunk)
                .unwrap();
        }
        assert!(
            inbox
                .deliver(MessageKind::ClipboardTransfer, &chunk)
                .is_err(),
            "a chunk past the window is a protocol violation"
        );
        assert!(inbox.deliver(MessageKind::Clipboard, &[0xff]).is_err());
        assert!(inbox.deliver(MessageKind::Frame, b"").is_err());
    }

    #[test]
    fn copies_travel_both_ways_and_land_ready_to_paste() {
        let pair = pair(options(u64::MAX), options(u64::MAX), Duration::ZERO);
        let source = TempDir::new("src");
        let folder = tree(&source);
        let single = source.0.join("one file.pdf");
        fs::write(&single, b"%PDF-1.7").unwrap();

        pair.a
            .copy(CopiedClipboard::Files(vec![folder.clone(), single]));
        let RemoteServe::Announce { token, kind } = pair.b.next_served() else {
            panic!("the copy is announced first");
        };
        assert_eq!(kind, PendingKind::Files);
        let RemoteServe::FulfilFiles {
            token: fulfilled,
            paths,
            ..
        } = pair.b.next_served()
        else {
            panic!("then fulfilled");
        };
        assert_eq!(fulfilled, token);
        assert_eq!(paths.len(), 2);
        assert!(paths[0].ends_with("photos") && paths[1].ends_with("one file.pdf"));
        assert_eq!(
            fs::read(paths[0].join("2026/big.raw")).unwrap(),
            fs::read(folder.join("2026/big.raw")).unwrap()
        );
        assert_eq!(
            fs::read_to_string(paths[0].join("notes.txt")).unwrap(),
            "旅行 notes"
        );
        assert_eq!(fs::read(&paths[1]).unwrap(), b"%PDF-1.7");
        let (_, line) = pair.b.status.changed_since(0).unwrap();
        assert_eq!(line.as_deref(), Some("2 files clipboard update queued"));

        let image = png(40);
        pair.b.copy(CopiedClipboard::Png(image.clone()));
        assert!(matches!(
            pair.a.next_served(),
            RemoteServe::Announce {
                kind: PendingKind::Png,
                ..
            }
        ));
        let RemoteServe::FulfilPng { png, .. } = pair.a.next_served() else {
            panic!("the image arrives");
        };
        assert_eq!(png, image);

        pair.a.copy(CopiedClipboard::Text("plain 文字".into()));
        let RemoteServe::Text(text) = pair.b.next_served() else {
            panic!("text arrives as text");
        };
        assert_eq!(text, "plain 文字");
    }

    #[test]
    fn a_newer_copy_cancels_a_transfer_still_in_flight() {
        // 20 ms per 128 KiB chunk keeps a 3 MB copy on the wire long
        // enough to be overtaken.
        let pair = pair(
            options(u64::MAX),
            options(u64::MAX),
            Duration::from_millis(20),
        );
        let source = TempDir::new("cancel");
        let folder = tree(&source);
        pair.a.copy(CopiedClipboard::Files(vec![folder.clone()]));
        let RemoteServe::Announce { token, .. } = pair.b.next_served() else {
            panic!("announced");
        };
        pair.a.copy(CopiedClipboard::Text("newer".into()));
        let mut abandoned = false;
        loop {
            match pair.b.next_served() {
                RemoteServe::Abandon { token: dropped } => {
                    assert_eq!(dropped, token);
                    abandoned = true;
                }
                RemoteServe::Text(text) => {
                    assert_eq!(text, "newer");
                    break;
                }
                other => panic!("the stale copy must not land: {other:?}"),
            }
        }
        assert!(abandoned, "the waiting paste is released");

        // Bytes of the cancelled copy still in flight count against the
        // window; the next copy must still go through intact.
        let small = source.0.join("small.txt");
        fs::write(&small, b"after cancel").unwrap();
        pair.a.copy(CopiedClipboard::Files(vec![small]));
        assert!(matches!(pair.b.next_served(), RemoteServe::Announce { .. }));
        let RemoteServe::FulfilFiles { paths, .. } = pair.b.next_served() else {
            panic!("the next copy lands");
        };
        assert_eq!(fs::read(&paths[0]).unwrap(), b"after cancel");
    }

    #[test]
    fn a_received_copy_offered_again_locally_is_not_sent_back() {
        let pair = pair(options(u64::MAX), options(u64::MAX), Duration::ZERO);
        let source = TempDir::new("echo");
        let file = source.0.join("a.txt");
        fs::write(&file, b"once").unwrap();
        pair.a.copy(CopiedClipboard::Files(vec![file]));
        assert!(matches!(pair.b.next_served(), RemoteServe::Announce { .. }));
        let RemoteServe::FulfilFiles { paths, .. } = pair.b.next_served() else {
            panic!("fulfilled");
        };
        // A clipboard manager on B takes the copy over and re-offers it.
        pair.b.copy(CopiedClipboard::Files(paths));
        pair.a.copy(CopiedClipboard::Text("ping".into()));
        let RemoteServe::Text(text) = pair.b.next_served() else {
            panic!("text");
        };
        assert_eq!(text, "ping");
        pair.b.copy(CopiedClipboard::Text("ping".into()));
        let image = png(5);
        pair.b.copy(CopiedClipboard::Png(image.clone()));
        assert!(matches!(pair.a.next_served(), RemoteServe::Announce { .. }));
        assert!(matches!(
            pair.a.next_served(),
            RemoteServe::FulfilPng { .. }
        ));
        pair.a.copy(CopiedClipboard::Png(image));
        // Nothing bounced: A saw only B's image, B nothing after "ping".
        assert!(pair.a.nothing_served_for(Duration::from_millis(300)));
        assert!(pair.b.nothing_served_for(Duration::from_millis(100)));
    }

    #[test]
    fn a_receiver_refuses_what_exceeds_its_own_limit() {
        let pair = pair(options(u64::MAX), options(1000), Duration::ZERO);
        let source = TempDir::new("limit");
        let file = source.0.join("big.bin");
        fs::write(&file, vec![0; 64 * 1024]).unwrap();
        pair.a.copy(CopiedClipboard::Files(vec![file]));
        assert!(pair.b.nothing_served_for(Duration::from_millis(300)));
        // The refusal stops the sender; the session carries on.
        pair.a.copy(CopiedClipboard::Text("still works".into()));
        let RemoteServe::Text(text) = pair.b.next_served() else {
            panic!("text after a refusal");
        };
        assert_eq!(text, "still works");
    }

    #[test]
    fn a_sender_keeps_copies_over_its_limit_to_itself() {
        let pair = pair(options(1000), options(u64::MAX), Duration::ZERO);
        let source = TempDir::new("sendlimit");
        let file = source.0.join("big.bin");
        fs::write(&file, vec![0; 64 * 1024]).unwrap();
        pair.a.copy(CopiedClipboard::Files(vec![file]));
        assert!(pair.b.nothing_served_for(Duration::from_millis(300)));
        let (_, line) = pair.a.status.changed_since(0).unwrap();
        assert_eq!(line.as_deref(), Some("copy not shared: too large"));
    }
    #[test]
    fn restriction_control_uses_reserved_slots_without_reordering_counted_data() {
        let commands = BoundedQueue::<Command>::new(2, 128);
        commands.send(Command::PeerText("one".into())).unwrap();
        commands.send(Command::PeerText("two".into())).unwrap();
        let request = ClipboardRestriction {
            revision: 1,
            content: 0,
        };
        commands
            .send(Command::Restrict {
                record: request,
                reply: MessageKind::ClipboardRestrict,
            })
            .unwrap();
        assert!(matches!(commands.try_recv(), Ok(Command::Restrict { .. })));
        assert!(matches!(commands.try_recv(), Ok(Command::PeerText(_))));
        assert!(!commands.closed());
        let outgoing = BoundedQueue::<Outgoing>::new(2, 128);
        for id in 1..=2 {
            outgoing
                .send(Outgoing {
                    kind: MessageKind::ClipboardTransfer,
                    payload: TransferOp::Data {
                        id,
                        bytes: vec![id as u8],
                    }
                    .encode(),
                })
                .unwrap();
        }
        outgoing
            .send(Outgoing {
                kind: MessageKind::ClipboardRestrict,
                payload: request.encode().to_vec(),
            })
            .unwrap();
        for id in 1..=2 {
            assert!(
                matches!(TransferOp::decode(&outgoing.try_recv().unwrap().payload).unwrap(), TransferOp::Data { id: actual, .. } if actual == id)
            );
        }
        assert_eq!(
            outgoing.try_recv().unwrap().kind,
            MessageKind::ClipboardRestrict
        );
        assert!(!outgoing.closed());
    }

    #[test]
    fn restriction_fence_survives_ack_coalescing_before_and_after_it() {
        let out = BoundedQueue::<Outgoing>::new(2, 128);
        let ack = |n| Outgoing {
            kind: MessageKind::ClipboardTransfer,
            payload: TransferOp::Ack { id: 1, consumed: n }.encode(),
        };
        out.send(ack(1)).unwrap();
        out.send(Outgoing {
            kind: MessageKind::ClipboardRestricted,
            payload: ClipboardRestriction {
                revision: 1,
                content: 0,
            }
            .encode()
            .to_vec(),
        })
        .unwrap();
        out.send(ack(2)).unwrap();
        assert_eq!(
            out.try_recv().unwrap().kind,
            MessageKind::ClipboardRestricted
        );
        assert_eq!(out.try_recv().unwrap(), ack(2));
    }

    #[test]
    fn skipped_bytes_are_bounded_accounted_and_never_fulfil_an_offer() {
        for bytes in [0, TRANSFER_CHUNK_BYTES as u64 + 1, u64::MAX] {
            assert!(TransferOp::decode(&TransferOp::Skip { id: 1, bytes }.encode()).is_err());
        }
        let (mut worker, served) = policy_worker(options(1024));
        worker.on_peer(TransferOp::Begin {
            id: 4,
            kind: TransferKind::Image,
            length: 10,
            entries: 0,
            items: 1,
        });
        assert!(matches!(
            served.try_recv().unwrap(),
            RemoteServe::Announce { .. }
        ));
        worker.in_flight.store(10, Ordering::Release);
        worker.on_peer(TransferOp::Skip { id: 4, bytes: 10 });
        assert_eq!(worker.in_flight.load(Ordering::Acquire), 0);
        assert_eq!(worker.consumed_total, 10);
        assert!(worker.receiving.is_none());
        assert!(matches!(
            served.try_recv().unwrap(),
            RemoteServe::Abandon { .. }
        ));
        worker.on_peer(TransferOp::End { id: 4 });
        assert!(served.try_recv().is_err());
        for id in [4, 3, 99] {
            worker.in_flight.store(7, Ordering::Release);
            worker.on_peer(TransferOp::Skip { id, bytes: 7 });
        }
        assert_eq!(worker.consumed_total, 31);
        assert_eq!(worker.in_flight.load(Ordering::Acquire), 0);
        let mut last_ack = None;
        while let Ok(record) = worker.out.try_recv() {
            if let TransferOp::Ack { consumed, .. } = TransferOp::decode(&record.payload).unwrap() {
                last_ack = Some(consumed);
            }
        }
        assert_eq!(last_ack, Some(31));
    }

    #[test]
    fn skipped_bytes_obey_ingress_window_and_overflow_is_terminal() {
        let commands = Arc::new(BoundedQueue::new(20, 1024));
        let inbox = ClipboardInbox {
            control: ClipboardControl::new(commands.clone(), RuntimeClipboardPolicy::new(7), 7),
            commands,
            in_flight: Arc::new(AtomicU64::new(0)),
            options: options(1024),
        };
        let skipped = TransferOp::Skip {
            id: 123,
            bytes: TRANSFER_CHUNK_BYTES as u64,
        }
        .encode();
        for _ in 0..TRANSFER_WINDOW_BYTES / TRANSFER_CHUNK_BYTES as u64 {
            inbox
                .deliver(MessageKind::ClipboardTransfer, &skipped)
                .unwrap();
        }
        assert_eq!(
            inbox.in_flight.load(Ordering::Acquire),
            TRANSFER_WINDOW_BYTES
        );
        assert!(
            inbox
                .deliver(MessageKind::ClipboardTransfer, &skipped)
                .is_err()
        );
        assert_eq!(
            inbox.in_flight.load(Ordering::Acquire),
            TRANSFER_WINDOW_BYTES
        );
        let (mut worker, _) = policy_worker(options(1024));
        worker.consumed_total = u64::MAX;
        worker.in_flight.store(1, Ordering::Release);
        worker.on_peer(TransferOp::Skip { id: 9, bytes: 1 });
        assert!(worker.out.closed());
        assert_eq!(worker.consumed_total, u64::MAX);
    }

    #[test]
    fn restriction_discards_unpublished_file_staging_and_preserves_credit() {
        use super::super::clipboard_staging_budget as budget;
        let temp = TempDir::new("runtime-restriction-budget");
        let root = temp.0.join("stage");
        budget::set_limit(&root, 1024).unwrap();
        let (mut worker, served) = policy_worker(options(1024));
        worker.staging_root = Some(root.clone());
        worker.on_peer(TransferOp::Begin {
            id: 1,
            kind: TransferKind::Files,
            length: 10,
            entries: 1,
            items: 1,
        });
        assert!(matches!(
            served.try_recv().unwrap(),
            RemoteServe::Announce { .. }
        ));
        assert_eq!(budget::status(&root).unwrap().reserved_bytes, 10);
        worker.sent_total = 21;
        worker.acked_total = 7;
        worker.consumed_total = 8;
        worker.policy.narrow(3).unwrap();
        worker.apply_restriction(
            ClipboardRestriction {
                revision: 1,
                content: 3,
            },
            MessageKind::ClipboardRestricted,
        );
        assert!(served.try_recv().is_err(), "native fence owns abandonment");
        assert!(worker.receiving.is_none());
        assert_eq!(budget::status(&root).unwrap().reserved_bytes, 0);
        assert_eq!(
            (worker.sent_total, worker.acked_total, worker.consumed_total),
            (21, 7, 8)
        );
        assert_eq!(
            worker.out.try_recv().unwrap().kind,
            MessageKind::ClipboardRestricted
        );
        worker.in_flight.store(2, Ordering::Release);
        worker.on_peer(TransferOp::Data {
            id: 1,
            bytes: vec![0; 2],
        });
        assert_eq!(worker.consumed_total, 10);
        assert_eq!(worker.in_flight.load(Ordering::Acquire), 0);
        assert!(served.try_recv().is_err());
    }

    #[test]
    fn restriction_barrier_waits_for_actual_target_and_rejects_overlap() {
        struct FenceTarget {
            calls: mpsc::Sender<RemoteServe>,
            barrier: Mutex<Option<mpsc::Receiver<()>>>,
        }
        impl ClipboardTarget for FenceTarget {
            fn serve(&self, request: RemoteServe) -> bool {
                self.calls.send(request).is_ok()
            }
            fn restrict(&self, _: u8) -> Result<mpsc::Receiver<()>, String> {
                self.barrier
                    .lock()
                    .unwrap()
                    .take()
                    .ok_or_else(|| "duplicate fence".into())
            }
        }
        let (calls, _received) = mpsc::channel();
        let (ack, barrier) = mpsc::sync_channel(1);
        let (commands, join, _inbox, outbox, _) = spawn_worker(
            FenceTarget {
                calls,
                barrier: Mutex::new(Some(barrier)),
            },
            options(1024),
            None,
            None,
        )
        .unwrap();
        let request = ClipboardRestriction {
            revision: 1,
            content: 0,
        };
        let control = outbox.control();
        control
            .restrict(request, MessageKind::ClipboardRestricted)
            .unwrap();
        assert_eq!(control.policy().content(), 0);
        assert!(
            control
                .restrict(
                    ClipboardRestriction {
                        revision: 2,
                        content: 0
                    },
                    MessageKind::ClipboardRestricted
                )
                .is_err()
        );
        assert!(
            outbox
                .next_timeout(Duration::from_millis(30))
                .unwrap()
                .is_none()
        );
        ack.send(()).unwrap();
        let record = outbox
            .next_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert_eq!(record.kind, MessageKind::ClipboardRestricted);
        assert_eq!(
            ClipboardRestriction::decode(&record.payload).unwrap(),
            request
        );
        control.restriction_written(request).unwrap();
        assert!(control.restriction_written(request).is_err());
        commands.send(Command::Stop).unwrap();
        join.join().unwrap();
    }

    #[test]
    fn target_fence_failure_never_emits_restriction_success() {
        struct FailedTarget;
        impl ClipboardTarget for FailedTarget {
            fn serve(&self, _: RemoteServe) -> bool {
                true
            }
            fn restrict(&self, _: u8) -> Result<mpsc::Receiver<()>, String> {
                let (_tx, rx) = mpsc::sync_channel(1);
                Ok(rx)
            }
        }
        let mut worker = worker_for_target(FailedTarget, options(1024));
        worker.policy.narrow(0).unwrap();
        worker.apply_restriction(
            ClipboardRestriction {
                revision: 1,
                content: 0,
            },
            MessageKind::ClipboardRestricted,
        );
        assert!(worker.out.closed());
        assert_eq!(worker.policy.content(), 0);
        assert!(worker.out.try_recv().is_err());
    }
    #[test]
    fn restriction_does_not_need_an_ordinary_backend_abandon_slot() {
        struct FullTarget(Arc<AtomicBool>);
        impl ClipboardTarget for FullTarget {
            fn serve(&self, _: RemoteServe) -> bool {
                assert!(
                    !self.0.load(Ordering::Acquire),
                    "ordinary backend admission while full"
                );
                true
            }
        }
        let full = Arc::new(AtomicBool::new(false));
        let mut worker = worker_for_target(FullTarget(full.clone()), options(1024));
        worker.on_peer(TransferOp::Begin {
            id: 1,
            kind: TransferKind::Image,
            length: 10,
            entries: 0,
            items: 1,
        });
        assert!(worker.receiving.is_some());
        full.store(true, Ordering::Release);
        worker.policy.narrow(1).unwrap();
        worker.pump();
        assert!(worker.receiving.is_none());
        assert!(worker.pending_token.is_some());
        worker.apply_restriction(
            ClipboardRestriction {
                revision: 1,
                content: 1,
            },
            MessageKind::ClipboardRestricted,
        );
        assert!(worker.pending_token.is_none());
        assert!(!worker.out.closed());
        assert_eq!(
            worker.out.try_recv().unwrap().kind,
            MessageKind::ClipboardRestricted
        );
    }
    #[test]
    fn next_revision_cannot_race_visible_ack_before_pending_clear() {
        let control = ClipboardControl::for_test(7);
        let first = ClipboardRestriction {
            revision: 1,
            content: 3,
        };
        control
            .restrict(first, MessageKind::ClipboardRestricted)
            .unwrap();
        control.commands.try_recv().unwrap();
        let (writing_tx, writing_rx) = mpsc::sync_channel(1);
        let (requesting_tx, requesting_rx) = mpsc::sync_channel(1);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let peer = control.clone();
        let worker = thread::spawn(move || {
            writing_rx.recv().unwrap();
            requesting_tx.send(()).unwrap();
            let result = peer.restrict(
                ClipboardRestriction {
                    revision: 2,
                    content: 1,
                },
                MessageKind::ClipboardRestricted,
            );
            done_tx.send(result.is_ok()).unwrap();
        });
        control
            .with_written_barrier(first, || {
                writing_tx.send(()).unwrap();
                requesting_rx.recv().unwrap();
                assert!(matches!(
                    done_rx.recv_timeout(Duration::from_millis(20)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ));
                Ok(())
            })
            .unwrap();
        assert!(done_rx.recv_timeout(Duration::from_secs(1)).unwrap());
        worker.join().unwrap();
        assert_eq!(control.policy().content(), 1);
    }

    #[test]
    fn failed_ack_write_keeps_restriction_pending_and_mask_reduced() {
        let control = ClipboardControl::for_test(7);
        let request = ClipboardRestriction {
            revision: 1,
            content: 3,
        };
        control
            .restrict(request, MessageKind::ClipboardRestricted)
            .unwrap();
        assert!(
            control
                .with_written_barrier(request, || Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "injected write timeout"
                )
                .into()))
                .is_err()
        );
        assert_eq!(control.policy().content(), 3);
        assert!(
            control
                .restrict(
                    ClipboardRestriction {
                        revision: 2,
                        content: 1
                    },
                    MessageKind::ClipboardRestricted
                )
                .is_err()
        );
    }
}
