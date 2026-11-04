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
use super::messages::{MAX_CLIPBOARD_BYTES, decode_clipboard, encode_clipboard};
use super::protocol::MessageKind;
use crate::backend::clipboard_x11::{
    CopiedClipboard, PendingKind, RemoteCaptures, RemoteClipboard, RemoteClipboardOptions,
    RemoteServe, RemoteSetter,
};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
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
    /// Copied files were negotiated with the peer.
    pub files: bool,
    /// Largest file copy this end sends or accepts.
    pub file_limit: u64,
    /// Share what is on the clipboard when the session starts.
    pub share_existing: bool,
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
    Stop,
}

/// Owns the session's clipboard threads; dropping it stops them.
pub struct ClipboardSync {
    commands: mpsc::Sender<Command>,
    stop: Arc<AtomicBool>,
    status: Arc<SyncStatus>,
    worker: Option<thread::JoinHandle<()>>,
    forwarder: Option<thread::JoinHandle<()>>,
}

/// Network-reader half: hands peer clipboard records to the worker.
pub struct ClipboardInbox {
    commands: mpsc::Sender<Command>,
    in_flight: Arc<AtomicU64>,
    files: bool,
}

/// Writer half: records the worker wants sent.
pub struct ClipboardOutbox {
    records: mpsc::Receiver<Outgoing>,
}

type Wake = Box<dyn Fn() + Send>;

/// Where the worker puts what arrives from the other machine: this display's
/// clipboard, or a recorder in tests.
trait ClipboardTarget {
    /// `false` when the target can no longer accept requests.
    fn serve(&self, request: RemoteServe) -> bool;
}

impl ClipboardTarget for RemoteSetter {
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
        mpsc::Sender<Command>,
        thread::JoinHandle<()>,
        ClipboardInbox,
        ClipboardOutbox,
        Arc<SyncStatus>,
    ),
    String,
> {
    let (commands, command_rx) = mpsc::channel();
    let (records_tx, records) = mpsc::channel();
    let status = Arc::new(SyncStatus::default());
    let in_flight = Arc::new(AtomicU64::new(0));
    let mut worker = Worker {
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
        sent_total: 0,
        acked_total: 0,
        consumed_total: 0,
        status_expires: None,
        last_received: None,
    };
    let join = thread::Builder::new()
        .name("jwm-remote-clipsync".into())
        .spawn(move || worker.run(&command_rx))
        .map_err(|error| format!("spawn clipboard worker: {error}"))?;
    Ok((
        commands.clone(),
        join,
        ClipboardInbox {
            commands,
            in_flight,
            files: options.files,
        },
        ClipboardOutbox { records },
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
                files: options.files,
                image_bytes: MAX_SHARED_IMAGE_BYTES,
                capture_existing: options.share_existing,
            },
        )?;
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
                let _ = worker.join();
                return Err(format!("spawn clipboard watcher: {error}"));
            }
        };
        Ok((
            Self {
                commands,
                stop,
                status,
                worker: Some(worker),
                forwarder: Some(forwarder),
            },
            inbox,
            outbox,
        ))
    }

    #[must_use]
    pub fn status(&self) -> Arc<SyncStatus> {
        Arc::clone(&self.status)
    }
}

impl Drop for ClipboardSync {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.commands.send(Command::Stop);
        if let Some(forwarder) = self.forwarder.take() {
            let _ = forwarder.join();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn forward_captures(
    captures: &RemoteCaptures,
    commands: &mpsc::Sender<Command>,
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

impl ClipboardInbox {
    /// Hand one authenticated peer record to the worker.
    ///
    /// Only the record's shape and the flow-control promise are checked
    /// here; an error is a protocol violation that ends the session.
    pub fn deliver(&self, kind: MessageKind, payload: &[u8]) -> RemoteResult<()> {
        let command = match kind {
            MessageKind::Clipboard => Command::PeerText(decode_clipboard(payload)?),
            MessageKind::ClipboardTransfer => {
                let op = TransferOp::decode(payload)?;
                match &op {
                    TransferOp::Begin {
                        kind: TransferKind::Files,
                        ..
                    } if !self.files => {
                        return Err(invalid_data("file sharing was not negotiated").into());
                    }
                    TransferOp::Data { bytes, .. } => {
                        let len = bytes.len() as u64;
                        let pending = self.in_flight.fetch_add(len, Ordering::AcqRel) + len;
                        if pending > TRANSFER_WINDOW_BYTES {
                            return Err(invalid_data(
                                "peer sent more clipboard data than it was allowed in flight",
                            )
                            .into());
                        }
                    }
                    _ => {}
                }
                Command::Peer(op)
            }
            other => {
                return Err(invalid_data(format!("{other:?} is not a clipboard record")).into());
            }
        };
        self.commands.send(command).map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "local clipboard worker stopped").into()
        })
    }
}

impl ClipboardOutbox {
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
    out: mpsc::Sender<Outgoing>,
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

/// How long "ready to paste" and similar outcomes stay in the title.
const OUTCOME_SHOWN_FOR: Duration = Duration::from_secs(10);

impl<T: ClipboardTarget> Worker<T> {
    fn run(&mut self, commands: &mpsc::Receiver<Command>) {
        let mut last_status = Instant::now();
        loop {
            self.pump();
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
        if let Some(receiving) = self.receiving.take() {
            self.target.serve(RemoteServe::Abandon {
                token: receiving.token,
            });
        }
    }

    fn apply(&mut self, command: Command) {
        match command {
            Command::Local(copied) => self.share_local(copied),
            Command::PeerText(text) => {
                self.cancel_sending();
                self.drop_receiving(false);
                self.last_received = Some(Fingerprint::Text(text.clone()));
                if !self.target.serve(RemoteServe::Text(text)) {
                    eprintln!(
                        "jwm-remote: could not put the other machine's text on the clipboard"
                    );
                }
                self.status.set(None);
            }
            Command::Peer(op) => self.on_peer(op),
            Command::LocalClosed | Command::Stop => {}
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
        if self.last_received.as_ref() == Some(&Fingerprint::of(&copied)) {
            // The peer's own copy, re-offered here: it already has it.
            return;
        }
        // The newest copy wins: whatever was still travelling, either way,
        // is now stale.
        self.cancel_sending();
        self.drop_receiving(true);
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
                    self.sent_total += bytes.len() as u64;
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
            && !(sending.ended && self.acked_total >= sending.start + sending.length)
        {
            self.queue_op(&TransferOp::Cancel { id: sending.id });
        }
    }

    /// Stop receiving; `refuse` tells the sender to stop too.
    fn drop_receiving(&mut self, refuse: bool) {
        let Some(receiving) = self.receiving.take() else {
            return;
        };
        if refuse {
            self.queue_op(&TransferOp::Refuse { id: receiving.id });
        }
        self.target.serve(RemoteServe::Abandon {
            token: receiving.token,
        });
        if let Some(staging) = self.staging.as_mut() {
            staging.discard_in_progress();
        }
    }

    fn on_peer(&mut self, op: TransferOp) {
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
                // The peer has moved on, whether or not this copy is taken.
                self.last_received = None;
                match self.accept(kind, length, entries) {
                    Ok(sink) => {
                        let token = self.next_token;
                        self.next_token += 1;
                        self.target.serve(RemoteServe::Announce {
                            token,
                            kind: match kind {
                                TransferKind::Image => PendingKind::Png,
                                TransferKind::Files => PendingKind::Files,
                            },
                        });
                        self.receiving = Some(Receiving {
                            id,
                            token,
                            sink,
                            length,
                            received: 0,
                            label: item_label(kind, items),
                            started: Instant::now(),
                        });
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
                self.consumed_total += len;
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
                if sending.ended && acked_total >= sending.start + sending.length {
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
        match kind {
            TransferKind::Image => {
                if length == 0 || length > MAX_SHARED_IMAGE_BYTES as u64 {
                    return Err(format!(
                        "the image is larger than {}",
                        human_bytes(MAX_SHARED_IMAGE_BYTES as u64)
                    ));
                }
                Ok(Sink::Image(Vec::with_capacity(length as usize)))
            }
            TransferKind::Files => {
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
                    .begin()
                    .map_err(|error| format!("could not create a staging directory: {error}"))?;
                Ok(Sink::Files(ArchiveReceiver::new(directory, entries)))
            }
        }
    }

    fn receive_chunk(&mut self, id: u32, bytes: &[u8]) -> Result<(), String> {
        let Some(receiving) = self.receiving.as_mut().filter(|r| r.id == id) else {
            // A transfer this end already dropped; its tail is discarded.
            return Ok(());
        };
        let len = bytes.len() as u64;
        if receiving.received + len > receiving.length {
            return Err("the copy was longer than announced".into());
        }
        match &mut receiving.sink {
            Sink::Image(png) => png.extend_from_slice(bytes),
            Sink::Files(archive) => archive.feed(bytes).map_err(|error| error.to_string())?,
        }
        receiving.received += len;
        Ok(())
    }

    fn finish_receiving(&mut self, id: u32) {
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
                    if let Some(staging) = self.staging.as_mut() {
                        staging.complete();
                    }
                    self.last_received = Some(Fingerprint::Files(paths.clone()));
                    Ok(RemoteServe::FulfilFiles { token, paths })
                }
                Err(error) => Err(error.to_string()),
            },
            _ => Err("the copy ended early".to_owned()),
        };
        match served {
            Ok(request) => {
                self.target.serve(request);
                eprintln!(
                    "jwm-remote: received {} ({}) in {elapsed:.1} s; ready to paste",
                    receiving.label,
                    human_bytes(receiving.length)
                );
                self.show_outcome(format!("{} ready to paste", receiving.label));
            }
            Err(reason) => {
                eprintln!("jwm-remote: the other machine's copy was not usable: {reason}");
                self.target.serve(RemoteServe::Abandon { token });
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
        commands: mpsc::Sender<Command>,
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
            files: true,
            file_limit,
            share_existing: false,
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
        let (commands, _keep) = mpsc::channel();
        let inbox = ClipboardInbox {
            commands,
            in_flight: Arc::new(AtomicU64::new(0)),
            files: false,
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
        assert_eq!(line.as_deref(), Some("2 files ready to paste"));

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
}
