//! CLIPBOARD monitoring and ownership.
//!
//! On X11 the clipboard is not storage but a protocol: the copying
//! application keeps the data and hands it over on request, and it disappears
//! when that application exits. A clipboard history therefore has to do two
//! separate jobs, and both live here:
//!
//! * **Watch** — XFIXES reports every change of CLIPBOARD ownership. On each
//!   change the current owner is asked for its target list, and only if that
//!   list is text or PNG and is not marked secret is the payload requested.
//! * **Serve** — putting an entry back means *becoming* the owner and
//!   answering `SelectionRequest` for as long as JWM holds it.
//!
//! Policy (what counts as a secret, what counts as text) lives in
//! [`crate::backend::clipboard_offer`] so both backends decide alike; this
//! module is only the X11 mechanics.
//!
//! It runs on **its own X connection and thread**, not the window manager's.
//! Selection traffic is request/response with other clients — a conversion
//! blocks until the owner answers — and the WM's event loop is driven by
//! frame pacing and its own reply traffic. Sharing one connection starved
//! selection events entirely: a standalone client on the same server
//! received them while the same code inside the WM loop received none.
//! Isolating the connection also means a slow or hostile clipboard owner can
//! never delay a frame.

use std::os::fd::AsRawFd as _;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xfixes::ConnectionExt as XFixesExt;
use x11rb::protocol::xproto::{
    self, Atom, AtomEnum, ConnectionExt as XProtoExt, EventMask, PropMode, Property,
    PropertyNotifyEvent, SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

#[cfg(feature = "backend-x11rb")]
use crate::backend::clipboard_offer::ClipboardImageSender;
use crate::backend::clipboard_offer::{
    CapturedClipboard, ClipboardOffer, ClipboardReceiptGuard, MAX_IMAGE_HISTORY_BYTES,
    X11_DIRECT_PROPERTY_BYTES, X11_INCR_CHUNK_BYTES, X11_MAX_ACTIVE_INCR_BYTES,
    X11_MAX_MULTIPLE_CONVERSIONS, next_x11_incr_chunk_with_limit, x11_selection_time_is_valid,
};

#[cfg(feature = "remote-x11")]
#[path = "clipboard_remote_mailbox.rs"]
mod remote_mailbox;

/// A requester that never deletes the INCR property must not retain transfer
/// state forever, and many hostile requesters must not grow it without bound.
const MAX_OUTGOING_INCR_TRANSFERS: usize = 32;
const MAX_OUTGOING_INCR_PER_REQUESTOR: usize = 8;
const OUTGOING_INCR_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const INCOMING_CONVERSION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Ask again when an owner has not answered a conversion at all.
///
/// xclip serves one INCR transfer at a time and silently drops any request
/// that arrives meanwhile. On a desktop where JWM's history and the remote
/// helper both read every copy, one of them lost every image over ~1 MiB.
/// Asking again once the other transfer has had time to finish gets it.
const UNANSWERED_CONVERSION_RETRY: std::time::Duration = std::time::Duration::from_millis(1500);
const MAX_CONVERSION_ATTEMPTS: u8 = 3;
const MAX_INCOMING_CHUNK_BYTES: u32 = 1024 * 1024;
const MAX_TARGET_ATOMS: usize = 256;
const MAX_TARGET_LIST_BYTES: usize = MAX_TARGET_ATOMS * 4;
const CLIPBOARD_HANDOFF_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// How long a paste may wait for data the remote helper is still receiving.
///
/// Comfortably inside GTK 3's 30-second selection timeout; Qt gives up after
/// five seconds on its own. A paste that outlives this is refused and works
/// again once the transfer has finished.
#[cfg(feature = "remote-x11")]
const DEFERRED_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(25);
/// Requests parked behind one pending offer. Real pastes are one or two; the
/// bound keeps a misbehaving requestor from growing the list.
#[cfg(feature = "remote-x11")]
const MAX_DEFERRED_REQUESTS: usize = 16;

/// Atoms this module needs. Interned once; comparing atoms is exact and free
/// compared with resolving names on every event.
#[derive(Debug, Clone, Copy)]
struct Atoms {
    clipboard: Atom,
    targets: Atom,
    multiple: Atom,
    timestamp: Atom,
    atom_pair: Atom,
    timestamp_probe: Atom,
    clipboard_manager: Atom,
    save_targets: Atom,
    null: Atom,
    handoff_property: Atom,
    handoff_timestamp_probe: Atom,
    utf8_string: Atom,
    text_plain_utf8: Atom,
    text_plain: Atom,
    image_png: Atom,
    image_jpeg: Atom,
    image_bmp: Atom,
    #[cfg(feature = "remote-x11")]
    gnome_copied_files: Atom,
    #[cfg(feature = "remote-x11")]
    uri_list: Atom,
    incr: Atom,
    /// Property on our own window that conversions are delivered into.
    transfer: Atom,
}

/// What a watcher captures and how much it keeps.
///
/// JWM's clipboard history and the remote helper share the X11 mechanics but
/// not the policy: only the remote helper treats a file manager's copy as
/// files (the history keeps its text form), and it shares one image at a time
/// rather than keeping fifty.
#[derive(Debug, Clone, Copy)]
struct CaptureProfile {
    /// Whether local clipboard content may be read at all. Serving stays independent.
    capture_enabled: bool,
    /// Whether image targets may be requested and decoded.
    images: bool,
    /// Prefer a copied file list over its plain-text form.
    #[cfg(feature = "remote-x11")]
    files: bool,
    /// Encoded-byte ceiling for a captured image.
    image_bytes: usize,
    /// Capture whatever is already on the clipboard when the watcher starts.
    capture_existing: bool,
}

impl CaptureProfile {
    #[cfg_attr(not(feature = "backend-x11rb"), allow(dead_code))]
    const HISTORY: Self = Self {
        capture_enabled: true,
        images: true,
        #[cfg(feature = "remote-x11")]
        files: false,
        image_bytes: MAX_IMAGE_HISTORY_BYTES,
        capture_existing: true,
    };
}

impl CaptureProfile {
    fn capture_existing(self) -> bool {
        self.capture_enabled && self.capture_existing
    }

    fn allows_conversion(self, conversion: Conversion) -> bool {
        self.capture_enabled
            && match conversion {
                Conversion::TargetList | Conversion::Text => true,
                Conversion::Png => self.images,
                #[cfg(feature = "remote-x11")]
                Conversion::Files => self.files,
            }
    }

    /// Forms worth asking for, best first.
    ///
    /// Text wins over images, matching the history. With the remote file
    /// profile a file manager's list wins over both, since its text form is
    /// only the paths; a list that names no local file falls through.
    fn capture_candidates(self, atoms: &Atoms, targets: &[Atom]) -> Vec<(Conversion, Atom)> {
        if !self.capture_enabled {
            return Vec::new();
        }
        let mut wanted = Vec::with_capacity(8);
        #[cfg(feature = "remote-x11")]
        if self.files {
            wanted.push((Conversion::Files, atoms.gnome_copied_files));
            wanted.push((Conversion::Files, atoms.uri_list));
        }
        // Only the richest advertised text form is asked for, and one image.
        if let Some(text) = [atoms.text_plain_utf8, atoms.utf8_string, atoms.text_plain]
            .into_iter()
            .find(|wanted| targets.contains(wanted))
        {
            wanted.push((Conversion::Text, text));
        }
        if self.images
            && let Some(image) = [atoms.image_png, atoms.image_jpeg, atoms.image_bmp]
                .into_iter()
                .find(|wanted| targets.contains(wanted))
        {
            wanted.push((Conversion::Png, image));
        }
        wanted.retain(|(_, target)| targets.contains(target));
        // Text first, then an image: once text exists the image is never
        // reached, exactly as before. Files are only ever ahead of both.
        let first_text = wanted
            .iter()
            .position(|(conversion, _)| *conversion == Conversion::Text);
        if let Some(index) = first_text {
            wanted.truncate(index + 1);
        }
        wanted
    }
}

/// One copy as a watcher captured it, before any consumer's filtering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CopiedClipboard {
    Text(String),
    Png(Vec<u8>),
    /// Absolute local paths from a file manager's copy.
    #[cfg(feature = "remote-x11")]
    Files(Vec<std::path::PathBuf>),
}

#[cfg(feature = "remote-x11")]
impl CopiedClipboard {
    fn content_bit(&self) -> u8 {
        match self {
            Self::Text(_) => 1,
            Self::Png(_) => 2,
            Self::Files(_) => 4,
        }
    }
    pub(crate) fn retained_bytes(&self) -> Option<usize> {
        remote_mailbox::copied_bytes(self)
    }
}

/// Where captured copies go.
enum CaptureSink {
    /// JWM's clipboard history: text and PNG only.
    #[cfg_attr(not(feature = "backend-x11rb"), allow(dead_code))]
    History(std::sync::mpsc::Sender<CapturedClipboard>),
    /// The remote helper: everything its profile captured.
    #[cfg(feature = "remote-x11")]
    Remote(remote_mailbox::CaptureSender),
}

impl CaptureSink {
    /// `false` once the consumer is gone.
    fn send(&self, copied: CopiedClipboard) -> bool {
        match self {
            Self::History(sender) => {
                let captured = match copied {
                    CopiedClipboard::Text(text) => CapturedClipboard::Text(text),
                    CopiedClipboard::Png(png) => CapturedClipboard::Png(png),
                    #[cfg(feature = "remote-x11")]
                    CopiedClipboard::Files(_) => return true,
                };
                sender.send(captured).is_ok()
            }
            #[cfg(feature = "remote-x11")]
            Self::Remote(sender) => sender.send(copied),
        }
    }
}

/// Where offers to serve come from.
enum ServeSource {
    #[cfg_attr(not(feature = "backend-x11rb"), allow(dead_code))]
    History(std::sync::mpsc::Receiver<ClipboardOffer>),
    #[cfg(feature = "remote-x11")]
    Remote(remote_mailbox::ServeReceiver),
}

/// Own both endpoints together, including before the worker starts. Rust
/// drops fields in declaration order: receiver teardown latches backend
/// failure before the capture sender wakes a forwarder on close. This also
/// holds on startup errors, spawn failure, and panic unwinding.
struct WatcherEndpoints {
    source: ServeSource,
    sink: CaptureSink,
}

impl Drop for WatcherEndpoints {
    fn drop(&mut self) {
        #[cfg(feature = "remote-x11")]
        if let ServeSource::Remote(source) = &self.source {
            source.0.close();
        }
    }
}

/// One transfer's disposition, not a token-to-path lookup. Dropping this receipt
/// does not imply rejection: unwinding or shutdown may follow publication.
#[cfg(feature = "remote-x11")]
#[derive(Clone, Debug, Default)]
pub(crate) struct FileFulfilReceipt(std::sync::Arc<std::sync::atomic::AtomicU8>);

#[cfg(feature = "remote-x11")]
impl FileFulfilReceipt {
    pub(crate) fn reject_unpublished(&self) {
        let _ = self.0.compare_exchange(
            0,
            1,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        );
    }
    pub(crate) fn accepted_or_may_publish(&self) {
        self.0.store(2, std::sync::atomic::Ordering::Release);
    }
    pub(crate) fn rejected_unpublished(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Acquire) == 1
    }
}

/// Monotonic content consent shared by capture, serving and session workers.
#[cfg(feature = "remote-x11")]
#[derive(Clone, Debug)]
pub(crate) struct RuntimeClipboardPolicy(std::sync::Arc<std::sync::atomic::AtomicU8>);

#[cfg(feature = "remote-x11")]
impl RuntimeClipboardPolicy {
    pub(crate) fn new(content: u8) -> Self {
        assert!(matches!(content, 0 | 1 | 3 | 7));
        Self(std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
            content,
        )))
    }
    pub(crate) fn shared_content(&self) -> std::sync::Arc<std::sync::atomic::AtomicU8> {
        self.0.clone()
    }
    pub(crate) fn content(&self) -> u8 {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }
    pub(crate) fn narrow(&self, content: u8) -> Result<(), String> {
        if !matches!(content, 0 | 1 | 3 | 7) {
            return Err("invalid clipboard content mask".into());
        }
        let mut current = self.content();
        loop {
            if content & !current != 0 {
                return Err("clipboard policy cannot expand".into());
            }
            match self.0.compare_exchange_weak(
                current,
                content,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => current = actual,
            }
        }
    }
    fn allows(&self, bit: u8) -> bool {
        self.content() & bit == bit
    }
}

/// What the remote helper asks its watcher to serve.
///
/// A copy received from the other machine is announced as soon as it starts
/// arriving, so CLIPBOARD changes the moment the copy was made rather than
/// when the last byte lands; a paste in between waits for the data.
#[cfg(feature = "remote-x11")]
#[derive(Debug)]
pub(crate) enum RemoteServe {
    Text(String),
    /// Own CLIPBOARD now with the targets of `kind`; data follows.
    Announce {
        token: u64,
        kind: PendingKind,
    },
    FulfilPng {
        token: u64,
        png: Vec<u8>,
    },
    FulfilFiles {
        token: u64,
        paths: Vec<std::path::PathBuf>,
        receipt: FileFulfilReceipt,
    },
    /// The announced data will never arrive; refuse waiting pastes.
    Abandon {
        token: u64,
    },
}

#[cfg(feature = "remote-x11")]
impl RemoteServe {
    fn content_bit(&self) -> u8 {
        match self {
            Self::Text(_) => 1,
            Self::FulfilPng { .. }
            | Self::Announce {
                kind: PendingKind::Png,
                ..
            } => 2,
            Self::FulfilFiles { .. }
            | Self::Announce {
                kind: PendingKind::Files,
                ..
            } => 4,
            Self::Abandon { .. } => 0,
        }
    }
    pub(crate) fn reject_unpublished(&self) {
        if let Self::FulfilFiles { receipt, .. } = self {
            receipt.reject_unpublished();
        }
    }

    pub(crate) fn retained_bytes(&self) -> Option<usize> {
        remote_mailbox::request_bytes(self)
    }
}

#[cfg(feature = "remote-x11")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PendingKind {
    Png,
    Files,
}

/// Pure offer installation decision. Some(true) means answer current waiting
/// pastes; Some(false) means a timestamp-pending offer; None proves rejection.
#[cfg(feature = "remote-x11")]
fn install_remote_fulfil(
    token: u64,
    data: OfferedData,
    pending: Option<&mut OfferedData>,
    offered: &mut Option<OfferedData>,
    receipt: Option<&FileFulfilReceipt>,
) -> Option<bool> {
    if let Some(pending) = pending
        && pending.pending_token() == Some(token)
    {
        *pending = data;
        if let Some(receipt) = receipt {
            receipt.accepted_or_may_publish();
        }
        return Some(false);
    }
    if offered.as_ref().and_then(OfferedData::pending_token) != Some(token) {
        if let Some(receipt) = receipt {
            receipt.reject_unpublished();
        }
        return None;
    }
    *offered = Some(data);
    if let Some(receipt) = receipt {
        receipt.accepted_or_may_publish();
    }
    Some(true)
}

/// What a finished conversion carries.
///
/// Routing is decided by the reply's own `target`, never by remembering what
/// was asked for last: ownership changes while a conversion is in flight, and
/// a late reply from the previous owner would otherwise be read as the new
/// owner's target list — which silently dropped every other copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Conversion {
    TargetList,
    Text,
    Png,
    #[cfg(feature = "remote-x11")]
    Files,
}

/// The watcher, living on its own thread.
struct Watcher {
    conn: RustConnection,
    /// Unmapped `InputOnly` window that receives conversions and owns the
    /// selection when JWM serves an entry back.
    window: Window,
    atoms: Atoms,
    profile: CaptureProfile,
    #[cfg(feature = "remote-x11")]
    remote_policy: Option<RuntimeClipboardPolicy>,
    /// Data JWM currently offers as owner; `None` when another application
    /// owns the clipboard.
    offered: Option<OfferedData>,
    pending_offer: Option<PendingOwnership>,
    /// Pastes waiting for a pending offer's data.
    #[cfg(feature = "remote-x11")]
    deferred: Vec<DeferredRequest>,
    pending_probe_events: usize,
    /// Server timestamp used for the most recent successful ownership change.
    ownership_time: Option<u32>,
    property_payload_bytes: usize,
    /// One generation-isolated request for data owned by another client.
    capture: Option<CaptureRequest>,
    outgoing_incr: std::collections::HashMap<(Window, Atom), OutgoingIncr>,
    shutting_down: bool,
}

#[derive(Debug)]
struct CaptureRequest {
    owner: Window,
    window: Window,
    request_time: u32,
    conversion: Conversion,
    target: Atom,
    /// Further targets to try, in order, if this one yields nothing usable
    /// (a `text/uri-list` of web links falls back to the owner's text).
    fallbacks: Vec<(Conversion, Atom)>,
    incoming_incr: Option<IncomingIncr>,
    last_activity: std::time::Instant,
    /// When the current conversion was (last) requested, and how often.
    requested_at: std::time::Instant,
    attempts: u8,
}

/// An offer waiting for the server timestamp that makes it the owner.
struct PendingOwnership {
    data: OfferedData,
    receipt: Option<ClipboardReceiptGuard>,
}

/// A paste parked until a pending offer's data arrives.
#[cfg(feature = "remote-x11")]
#[derive(Debug, Clone, Copy)]
struct DeferredRequest {
    requestor: Window,
    selection: Atom,
    target: Atom,
    property: Atom,
    time: u32,
    deadline: std::time::Instant,
}

#[derive(Debug)]
struct IncomingIncr {
    bytes: Vec<u8>,
    oversized: bool,
}

#[derive(Clone, Debug)]
enum OfferedData {
    Text(std::sync::Arc<[u8]>),
    Png(std::sync::Arc<[u8]>),
    /// A file list in each encoding it is served as.
    #[cfg(feature = "remote-x11")]
    Files {
        gnome: std::sync::Arc<[u8]>,
        uri_list: std::sync::Arc<[u8]>,
        text: std::sync::Arc<[u8]>,
    },
    /// Targets announced, data still arriving from the other machine.
    #[cfg(feature = "remote-x11")]
    Pending {
        token: u64,
        kind: PendingKind,
    },
}

impl From<ClipboardOffer> for OfferedData {
    fn from(offer: ClipboardOffer) -> Self {
        match offer {
            ClipboardOffer::Text(text) => Self::Text(text.into_bytes().into()),
            ClipboardOffer::Png(png) => Self::Png(png.into()),
            ClipboardOffer::Tracked(offer, _receipt) => (*offer).into(),
        }
    }
}

impl OfferedData {
    #[cfg(feature = "remote-x11")]
    fn content_bit(&self) -> u8 {
        match self {
            Self::Text(_) => 1,
            Self::Png(_)
            | Self::Pending {
                kind: PendingKind::Png,
                ..
            } => 2,
            Self::Files { .. }
            | Self::Pending {
                kind: PendingKind::Files,
                ..
            } => 4,
        }
    }
    #[cfg(feature = "remote-x11")]
    fn files(paths: &[std::path::PathBuf]) -> Self {
        let payloads = crate::backend::clipboard_offer::file_list_payloads(paths);
        Self::Files {
            gnome: payloads.gnome.into(),
            uri_list: payloads.uri_list.into(),
            text: payloads.text.into(),
        }
    }

    fn targets(&self, atoms: &Atoms) -> Vec<Atom> {
        let mut targets = vec![
            atoms.targets,
            atoms.multiple,
            atoms.timestamp,
            atoms.save_targets,
        ];
        targets.extend(self.payload_targets(atoms));
        targets
    }

    fn payload_for(&self, atoms: &Atoms, target: Atom) -> Option<std::sync::Arc<[u8]>> {
        let is_text = target == atoms.utf8_string
            || target == atoms.text_plain_utf8
            || target == atoms.text_plain;
        match self {
            Self::Text(bytes) if is_text => Some(std::sync::Arc::clone(bytes)),
            Self::Png(bytes) if target == atoms.image_png => Some(std::sync::Arc::clone(bytes)),
            #[cfg(feature = "remote-x11")]
            Self::Files {
                gnome,
                uri_list,
                text,
            } => {
                if target == atoms.gnome_copied_files {
                    Some(std::sync::Arc::clone(gnome))
                } else if target == atoms.uri_list {
                    Some(std::sync::Arc::clone(uri_list))
                } else if is_text {
                    Some(std::sync::Arc::clone(text))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn payload_targets(&self, atoms: &Atoms) -> Vec<Atom> {
        match self {
            Self::Text(_) => vec![atoms.utf8_string, atoms.text_plain_utf8, atoms.text_plain],
            Self::Png(_) => vec![atoms.image_png],
            #[cfg(feature = "remote-x11")]
            Self::Files { .. }
            | Self::Pending {
                kind: PendingKind::Files,
                ..
            } => vec![
                atoms.gnome_copied_files,
                atoms.uri_list,
                atoms.utf8_string,
                atoms.text_plain_utf8,
                atoms.text_plain,
            ],
            #[cfg(feature = "remote-x11")]
            Self::Pending {
                kind: PendingKind::Png,
                ..
            } => vec![atoms.image_png],
        }
    }

    /// Whether a request for `target` must wait for data still arriving.
    #[cfg(feature = "remote-x11")]
    fn defers(&self, atoms: &Atoms, target: Atom) -> bool {
        matches!(self, Self::Pending { .. }) && self.payload_targets(atoms).contains(&target)
    }

    #[cfg(feature = "remote-x11")]
    fn pending_token(&self) -> Option<u64> {
        match self {
            Self::Pending { token, .. } => Some(*token),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct OutgoingIncr {
    #[cfg(feature = "remote-x11")]
    content_bit: u8,
    target: Atom,
    data: std::sync::Arc<[u8]>,
    offset: usize,
    last_activity: std::time::Instant,
}

#[cfg(feature = "remote-x11")]
impl OutgoingIncr {
    /// Retain only the bounded handshake metadata, not revoked content.
    /// ICCCM requires waiting for Property::DELETE before the empty terminal
    /// chunk. Sending it now could overwrite an unread announcement/chunk.
    fn restrict(&mut self, content: u8) {
        if self.content_bit & content == 0 {
            self.data = std::sync::Arc::from([]);
            self.offset = 0;
        }
    }
}

/// Handle held by the backend: captured payloads arrive on `captured`,
/// entries to serve are sent on `serve`.
#[cfg(feature = "backend-x11rb")]
pub(crate) struct Clipboard {
    captured: std::sync::mpsc::Receiver<CapturedClipboard>,
    serve: std::sync::mpsc::Sender<ClipboardOffer>,
    worker_wake: crate::backend::update_notifier::AsyncUpdateNotifier,
    notifier: std::sync::Arc<
        std::sync::Mutex<Option<crate::backend::update_notifier::AsyncUpdateNotifier>>,
    >,
    worker: std::sync::Arc<ClipboardWorkerLifetime>,
}

struct ClipboardWorkerLifetime {
    signal_only: bool,
    shutdown: std::sync::mpsc::SyncSender<()>,
    wake: crate::backend::update_notifier::AsyncUpdateNotifier,
    done: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    join: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Drop for ClipboardWorkerLifetime {
    fn drop(&mut self) {
        let _ = self.shutdown.try_send(());
        self.wake.notify();
        if self.signal_only {
            return;
        }
        // Remote sessions take the join handle for explicit bounded shutdown.
        // Their foreground Drop must not wait again on this lifetime.
        if self
            .join
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_none()
        {
            return;
        }
        let finished = match self
            .done
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .recv_timeout(CLIPBOARD_HANDOFF_TIMEOUT + std::time::Duration::from_secs(1))
        {
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => true,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => false,
        };
        let join = self
            .join
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if finished && let Some(join) = join {
            let _ = join.join();
        } else if !finished {
            log::warn!("clipboard: worker did not stop within the shutdown deadline");
        }
    }
}

type SharedNotifier =
    std::sync::Arc<std::sync::Mutex<Option<crate::backend::update_notifier::AsyncUpdateNotifier>>>;

/// Start a watcher on its own connection and thread.
///
/// The watcher is built on the thread that will own it: RustConnection is not
/// Sync, and nothing outside the thread may touch it.
fn spawn_watcher(
    display: Option<&str>,
    profile: CaptureProfile,
    sink: CaptureSink,
    source: ServeSource,
    notifier: SharedNotifier,
) -> Result<
    (
        crate::backend::update_notifier::AsyncUpdateNotifier,
        std::sync::Arc<ClipboardWorkerLifetime>,
    ),
    String,
> {
    let signal_only = match &source {
        ServeSource::History(_) => false,
        #[cfg(feature = "remote-x11")]
        ServeSource::Remote(_) => true,
    };
    let display = display.map(str::to_string);
    let (shutdown, shutdown_rx) = std::sync::mpsc::sync_channel(1);
    let (done_tx, done) = std::sync::mpsc::channel();
    let worker_wake = crate::backend::update_notifier::AsyncUpdateNotifier::new()
        .map_err(|error| format!("create clipboard worker wake fd: {error}"))?;
    let thread_wake = worker_wake.clone();
    let (ready_tx, ready) = std::sync::mpsc::channel();
    let endpoints = WatcherEndpoints { source, sink };
    let join = std::thread::Builder::new()
        .name("jwm-clipboard".to_string())
        .spawn(move || {
            match Watcher::new(display.as_deref(), profile) {
                Ok(mut watcher) => {
                    #[cfg(feature = "remote-x11")]
                    if let ServeSource::Remote(source) = &endpoints.source {
                        watcher.remote_policy = Some(source.0.policy());
                    }
                    let _ = ready_tx.send(Ok(()));
                    watcher.run(
                        &endpoints.sink,
                        &endpoints.source,
                        &notifier,
                        &thread_wake,
                        &shutdown_rx,
                    );
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                }
            }
            thread_wake.mark_unhealthy();
            let _ = done_tx.send(());
        })
        .map_err(|error| format!("spawn clipboard thread: {error}"))?;

    ready
        .recv()
        .map_err(|_| "clipboard thread died during startup".to_string())??;
    let worker = std::sync::Arc::new(ClipboardWorkerLifetime {
        signal_only,
        shutdown,
        wake: worker_wake.clone(),
        done: std::sync::Mutex::new(done),
        join: std::sync::Mutex::new(Some(join)),
    });
    Ok((worker_wake, worker))
}

#[cfg(feature = "backend-x11rb")]
impl Clipboard {
    /// Start watching CLIPBOARD on a dedicated connection and thread.
    pub(crate) fn start(display: Option<&str>) -> Result<Self, String> {
        let (captured_tx, captured) = std::sync::mpsc::channel();
        let (serve, serve_rx) = std::sync::mpsc::channel();
        let notifier = std::sync::Arc::new(std::sync::Mutex::new(None));
        let (worker_wake, worker) = spawn_watcher(
            display,
            CaptureProfile::HISTORY,
            CaptureSink::History(captured_tx),
            ServeSource::History(serve_rx),
            std::sync::Arc::clone(&notifier),
        )?;
        Ok(Self {
            captured,
            serve,
            worker_wake,
            notifier,
            worker,
        })
    }

    /// Payloads copied since the last call, oldest first.
    pub(crate) fn drain_captured(&self) -> Vec<CapturedClipboard> {
        self.captured.try_iter().collect()
    }

    /// Offer `text` to other applications.
    pub(crate) fn offer_with_receipt(
        &self,
        offer: crate::backend::clipboard_offer::CapturedClipboard,
    ) -> Option<crate::backend::clipboard_offer::ClipboardReceipt> {
        use crate::backend::clipboard_offer::{
            CapturedClipboard, MAX_TEXT_BYTES, X11_MAX_OFFER_BYTES,
        };
        let offer = match offer {
            CapturedClipboard::Text(text) if text.len() <= MAX_TEXT_BYTES => {
                ClipboardOffer::Text(text)
            }
            CapturedClipboard::Png(png) if png.len() <= X11_MAX_OFFER_BYTES => {
                ClipboardOffer::Png(png)
            }
            _ => return None,
        };
        let (offer, receipt) = ClipboardOffer::tracked(offer);
        if !self.worker_wake.is_healthy()
            || self.serve.send(offer).is_err()
            || !self.worker_wake.notify()
        {
            receipt.complete(false);
        }
        Some(receipt)
    }
    pub(crate) fn set_text(&self, text: &str) -> bool {
        if text.len() > crate::backend::clipboard_offer::MAX_TEXT_BYTES {
            return false;
        }
        let _keep_worker_alive = &self.worker;
        if !self.worker_wake.is_healthy() {
            return false;
        }
        let sent = self
            .serve
            .send(ClipboardOffer::Text(text.to_string()))
            .is_ok();
        sent && self.worker_wake.notify()
    }

    /// Route an encoded PNG to this backend's native selection owner.
    pub(crate) fn image_sender(&self) -> ClipboardImageSender {
        let _keep_worker_alive = &self.worker;
        ClipboardImageSender::new_with_wake(self.serve.clone(), self.worker_wake.clone())
    }

    /// Attach captures to the owning handler after that handler has created
    /// its aggregate readiness fd. The watcher connection starts earlier than
    /// JWM, so attaching itself emits one conservative wake to cover a capture
    /// that may already be queued.
    pub(crate) fn set_update_notifier(
        &self,
        notifier: Option<crate::backend::update_notifier::AsyncUpdateNotifier>,
    ) {
        let wake = {
            let mut slot = self
                .notifier
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *slot = notifier;
            slot.clone()
        };
        if let Some(notifier) = wake {
            notifier.notify();
        }
    }
}

/// What the remote helper's watcher captures.
#[cfg(feature = "remote-x11")]
#[derive(Debug, Clone, Copy)]
pub(crate) struct RemoteClipboardOptions {
    /// Capture local clipboard content for the peer. False is receive-only.
    pub(crate) send: bool,
    /// Request image targets only when image sharing was negotiated.
    pub(crate) images: bool,
    /// Share a file manager's copy as files rather than as its path text.
    pub(crate) files: bool,
    /// Encoded-byte ceiling for a shared image.
    pub(crate) image_bytes: usize,
    /// Share what is already on the clipboard when the session starts.
    pub(crate) capture_existing: bool,
}

/// The remote helper's watcher, split into a capture half and a serve half
/// that live on different threads.
#[cfg(feature = "remote-x11")]
pub(crate) struct RemoteClipboard;

#[cfg(feature = "remote-x11")]
impl RemoteClipboard {
    /// `display` selects the X server, so the helper watches the display it
    /// is actually sharing or presenting rather than whatever `$DISPLAY` says.
    pub(crate) fn start(
        display: Option<&str>,
        options: RemoteClipboardOptions,
    ) -> Result<(RemoteCaptures, RemoteSetter), String> {
        if options.files && !options.images {
            return Err(
                "remote clipboard files require the canonical image-and-file content grant".into(),
            );
        }
        let policy = RuntimeClipboardPolicy::new(
            1 | if options.images { 2 } else { 0 } | if options.files { 4 } else { 0 },
        );
        let (captured_tx, captured) = remote_mailbox::capture_channel_with_policy(policy.clone());
        let serve = std::sync::Arc::new(remote_mailbox::ServeQueue::with_policy(policy));
        let serve_rx = remote_mailbox::ServeReceiver(serve.clone());
        let (wake, worker) = spawn_watcher(
            display,
            CaptureProfile {
                capture_enabled: options.send,
                images: options.images,
                files: options.files,
                image_bytes: options.image_bytes.min(remote_mailbox::MAX_IMAGE_BYTES),
                capture_existing: options.capture_existing,
            },
            CaptureSink::Remote(captured_tx),
            ServeSource::Remote(serve_rx),
            std::sync::Arc::new(std::sync::Mutex::new(None)),
        )?;
        Ok((
            RemoteCaptures {
                captured,
                worker: std::sync::Arc::clone(&worker),
            },
            RemoteSetter {
                serve: std::sync::Arc::new(remote_mailbox::ServeProducer {
                    queue: serve,
                    wake: wake.clone(),
                }),
                wake,
                worker,
            },
        ))
    }
}

/// Receiving half: copies made on this display.
#[cfg(feature = "remote-x11")]
pub(crate) struct RemoteCaptures {
    captured: remote_mailbox::CaptureReceiver,
    worker: std::sync::Arc<ClipboardWorkerLifetime>,
}

#[cfg(feature = "remote-x11")]
impl RemoteCaptures {
    /// Block for the next copy, giving up after `timeout`.
    ///
    /// `Ok(None)` is a live timeout; `Err` means the watcher stopped for good
    /// after every queued copy was taken.
    pub(crate) fn recv_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> Result<Option<CopiedClipboard>, std::sync::mpsc::RecvError> {
        let _keep_worker_alive = &self.worker;
        self.captured.recv_timeout(timeout)
    }
}

/// Signal-only ownership of the remote X11 worker's shutdown path.
#[cfg(feature = "remote-x11")]
pub(crate) struct RemoteClipboardShutdown {
    shutdown: std::sync::mpsc::SyncSender<()>,
    wake: crate::backend::update_notifier::AsyncUpdateNotifier,
}

#[cfg(feature = "remote-x11")]
impl RemoteClipboardShutdown {
    pub(crate) fn stop(&self) {
        let _ = self.shutdown.try_send(());
        self.wake.notify();
    }
}

#[cfg(feature = "remote-x11")]
impl Drop for RemoteClipboardShutdown {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Sending half: what to offer to other applications on this display.
#[cfg(feature = "remote-x11")]
#[derive(Clone)]
pub(crate) struct RemoteSetter {
    serve: std::sync::Arc<remote_mailbox::ServeProducer>,
    wake: crate::backend::update_notifier::AsyncUpdateNotifier,
    worker: std::sync::Arc<ClipboardWorkerLifetime>,
}

#[cfg(feature = "remote-x11")]
impl RemoteSetter {
    pub(crate) fn policy(&self) -> RuntimeClipboardPolicy {
        self.serve.queue.policy()
    }

    /// Reserved fence; completion is signalled only by the native owner thread.
    pub(crate) fn restrict(&self, content: u8) -> Result<std::sync::mpsc::Receiver<()>, String> {
        if !self.is_healthy() {
            return Err("clipboard backend unavailable".into());
        }
        let acknowledged = self.serve.queue.restrict(content)?;
        if !self.wake.notify() {
            self.serve.queue.abort();
            return Err("clipboard backend wake failed".into());
        }
        Ok(acknowledged)
    }
    /// Sticky abnormal termination, excluding successful manager handoff.
    pub(crate) fn is_failed(&self) -> bool {
        self.serve.queue.aborted()
    }

    pub(crate) fn is_healthy(&self) -> bool {
        self.wake.is_healthy() && !self.is_failed()
    }

    /// Move join ownership to the remote session before any handles can drop.
    pub(crate) fn take_shutdown(&self) -> (RemoteClipboardShutdown, std::thread::JoinHandle<()>) {
        let join = self
            .worker
            .join
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .expect("remote clipboard shutdown taken once");
        (
            RemoteClipboardShutdown {
                shutdown: self.worker.shutdown.clone(),
                wake: self.worker.wake.clone(),
            },
            join,
        )
    }

    /// Nonblocking admission. Invalid/oversized payloads or a full queue
    /// fail the entire remote endpoint and refuse pending pastes.
    pub(crate) fn send(&self, request: RemoteServe) -> bool {
        let _keep_worker_alive = &self.worker;
        if !self.is_healthy() {
            request.reject_unpublished();
            self.serve.queue.abort();
            self.wake.notify();
            return false;
        }
        let accepted = self.serve.queue.send(request);
        let notified = self.wake.notify();
        if !notified {
            self.serve.queue.abort();
        }
        accepted && notified
    }
}

fn intern(conn: &RustConnection, name: &str) -> Result<Atom, String> {
    conn.intern_atom(false, name.as_bytes())
        .map_err(|error| format!("intern {name}: {error}"))?
        .reply()
        .map(|reply| reply.atom)
        .map_err(|error| format!("intern {name} reply: {error}"))
}

impl Watcher {
    fn effective_profile(&self) -> CaptureProfile {
        #[cfg(feature = "remote-x11")]
        if let Some(policy) = &self.remote_policy {
            let mut profile = self.profile;
            profile.capture_enabled &= policy.allows(1);
            profile.images &= policy.allows(2);
            profile.files &= policy.allows(4);
            return profile;
        }
        self.profile
    }

    #[cfg(feature = "remote-x11")]
    fn apply_restriction(&mut self, content: u8) -> bool {
        if self.capture.as_ref().is_some_and(|capture| {
            !self
                .effective_profile()
                .allows_conversion(capture.conversion)
        }) {
            self.cancel_capture();
        }
        if self
            .pending_offer
            .as_ref()
            .is_some_and(|pending| pending.data.content_bit() & content == 0)
        {
            self.pending_offer = None;
        }
        if self
            .offered
            .as_ref()
            .is_some_and(|offer| offer.content_bit() & content == 0)
        {
            self.answer_deferred(false);
            // Stop future serving without claiming to revoke pastes or file paths.
            self.offered = None;
        }
        for transfer in self.outgoing_incr.values_mut() {
            transfer.restrict(content);
        }
        // Completion certifies no forbidden content remains available to send.
        // Empty INCR terminators follow each requestor's next delete; their
        // delivery, and external paste completion, are not certified here.
        // Flush and round-trip: completion cannot precede native operations above.
        self.conn.flush().is_ok()
            && self
                .conn
                .get_input_focus()
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .is_some()
    }
    /// Open a private connection, create the owner window, and start
    /// watching CLIPBOARD.
    fn new(display: Option<&str>, profile: CaptureProfile) -> Result<Self, String> {
        let (conn, screen_num) = xbar_core::x11_connection::connect(display)
            .map_err(|error| format!("clipboard connect: {error}"))?;
        let root = conn
            .setup()
            .roots
            .get(screen_num)
            .ok_or_else(|| "clipboard: no screen".to_string())?
            .root;
        let atoms = Atoms {
            clipboard: intern(&conn, "CLIPBOARD")?,
            targets: intern(&conn, "TARGETS")?,
            multiple: intern(&conn, "MULTIPLE")?,
            timestamp: intern(&conn, "TIMESTAMP")?,
            atom_pair: intern(&conn, "ATOM_PAIR")?,
            timestamp_probe: intern(&conn, "JWM_CLIPBOARD_TIMESTAMP")?,
            clipboard_manager: intern(&conn, "CLIPBOARD_MANAGER")?,
            save_targets: intern(&conn, "SAVE_TARGETS")?,
            null: intern(&conn, "NULL")?,
            handoff_property: intern(&conn, "JWM_CLIPBOARD_SAVE_TARGETS")?,
            handoff_timestamp_probe: intern(&conn, "JWM_CLIPBOARD_HANDOFF_TIMESTAMP")?,
            utf8_string: intern(&conn, "UTF8_STRING")?,
            text_plain_utf8: intern(&conn, "text/plain;charset=utf-8")?,
            text_plain: intern(&conn, "text/plain")?,
            image_png: intern(&conn, "image/png")?,
            image_jpeg: intern(&conn, "image/jpeg")?,
            image_bmp: intern(&conn, "image/bmp")?,
            #[cfg(feature = "remote-x11")]
            gnome_copied_files: intern(
                &conn,
                crate::backend::clipboard_offer::GNOME_COPIED_FILES_MIME,
            )?,
            #[cfg(feature = "remote-x11")]
            uri_list: intern(&conn, crate::backend::clipboard_offer::URI_LIST_MIME)?,
            incr: intern(&conn, "INCR")?,
            transfer: intern(&conn, "JWM_CLIPBOARD")?,
        };

        let window = conn
            .generate_id()
            .map_err(|error| format!("clipboard window id: {error}"))?;
        (&conn)
            .create_window(
                0,
                window,
                root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                0,
                &xproto::CreateWindowAux::default().event_mask(EventMask::PROPERTY_CHANGE),
            )
            .map_err(|error| format!("create clipboard window: {error}"))?;

        // XFIXES requires a version handshake before any of its requests are
        // honored. The compositor negotiates it too, but this module runs
        // before that and must not depend on the order.
        conn.xfixes_query_version(5, 0)
            .map_err(|error| format!("xfixes_query_version: {error}"))?
            .reply()
            .map_err(|error| format!("xfixes version reply: {error}"))?;

        // SET_SELECTION_OWNER is the only mask needed: every copy changes the
        // owner, and window-destroy/close arrive as an owner change to None.
        conn.xfixes_select_selection_input(
            window,
            atoms.clipboard,
            x11rb::protocol::xfixes::SelectionEventMask::SET_SELECTION_OWNER
                | x11rb::protocol::xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
                | x11rb::protocol::xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE,
        )
        .map_err(|error| format!("xfixes_select_selection_input: {error}"))?
        .check()
        .map_err(|error| format!("xfixes_select_selection_input rejected: {error}"))?;
        conn.flush()
            .map_err(|error| format!("clipboard flush: {error}"))?;

        log::info!("clipboard: watching CLIPBOARD (owner window 0x{window:x})");
        let property_payload_bytes = conn.maximum_request_bytes().saturating_sub(24).max(4);
        let mut watcher = Self {
            conn,
            window,
            atoms,
            profile,
            #[cfg(feature = "remote-x11")]
            remote_policy: None,
            offered: None,
            pending_offer: None,
            #[cfg(feature = "remote-x11")]
            deferred: Vec::new(),
            pending_probe_events: 0,
            ownership_time: None,
            property_payload_bytes,
            capture: None,
            outgoing_incr: std::collections::HashMap::new(),
            shutting_down: false,
        };
        // Do not even inspect the preexisting selection in receive-only mode.
        if profile.capture_existing() {
            let existing_owner = watcher
                .conn
                .get_selection_owner(watcher.atoms.clipboard)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .map(|reply| reply.owner)
                .unwrap_or(x11rb::NONE);
            if existing_owner != x11rb::NONE {
                watcher.begin_capture(existing_owner, x11rb::CURRENT_TIME);
            }
        }
        Ok(watcher)
    }

    /// Event loop for the clipboard thread. X traffic and local offers share
    /// one blocking poll, so an idle session performs no timer wakeups.
    fn run(
        &mut self,
        captured: &CaptureSink,
        serve: &ServeSource,
        notifier: &std::sync::Mutex<Option<crate::backend::update_notifier::AsyncUpdateNotifier>>,
        wake: &crate::backend::update_notifier::AsyncUpdateNotifier,
        shutdown: &std::sync::mpsc::Receiver<()>,
    ) {
        let mut serve_connected = true;
        let mut capture_connected = true;
        loop {
            let _ = wake.drain();
            let shutdown_requested = !matches!(
                shutdown.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            );

            // Keep history's final queued offer/receipt handoff behavior.
            if serve_connected && matches!(serve, ServeSource::History(_)) {
                serve_connected = self.take_queued_offers(serve);
            }
            if shutdown_requested {
                #[cfg(feature = "remote-x11")]
                if let ServeSource::Remote(source) = serve {
                    // No new admission after stop. At most MAX_COMMANDS
                    // remain, so this final drain cannot be producer-starved.
                    source.0.close_sender();
                    while source.0.has_backlog() && !source.0.aborted() {
                        if !self.take_queued_offers(serve) {
                            return;
                        }
                    }
                    if source.0.aborted() {
                        self.abort_remote();
                        return;
                    }
                    self.answer_deferred(false);
                }
                self.handoff_to_clipboard_manager(wake, serve);
                #[cfg(feature = "remote-x11")]
                if let ServeSource::Remote(source) = serve
                    && !source.0.close_normally()
                {
                    self.abort_remote();
                }
                return;
            }

            #[cfg(feature = "remote-x11")]
            if serve_connected && matches!(serve, ServeSource::Remote(_)) {
                serve_connected = self.take_queued_offers(serve);
            }
            #[cfg(feature = "remote-x11")]
            if !serve_connected && matches!(serve, ServeSource::Remote(_)) {
                return;
            }
            self.expire_outgoing_incr();
            #[cfg(feature = "remote-x11")]
            self.expire_deferred();

            let mut handled_event = false;
            for _ in 0..256 {
                match self.conn.poll_for_event() {
                    Ok(Some(event)) => {
                        handled_event = true;
                        if let Some(payload) = self.handle(&event)
                            && capture_connected
                            && !publish_capture(captured, notifier, payload)
                        {
                            capture_connected = false;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        log::warn!("clipboard: X11 connection failed: {error}");
                        return;
                    }
                }
            }
            #[cfg(feature = "remote-x11")]
            if let ServeSource::Remote(source) = serve
                && (source.0.has_backlog() || (serve_connected && source.0.aborted()))
            {
                continue;
            }
            if handled_event {
                continue;
            }
            if let Err(error) = self.wait_for_work(wake, None) {
                log::warn!("clipboard: event wait failed: {error}");
                return;
            }
        }
    }

    /// Apply offers queued since the last pass. `false` once the sender
    /// side has gone away for good.
    fn take_queued_offers(&mut self, serve: &ServeSource) -> bool {
        match serve {
            ServeSource::History(serve) => match serve.try_recv() {
                Ok(mut offer) => {
                    // Only the newest clipboard value can be current. Drop
                    // superseded queued PNG allocations before taking X11
                    // ownership instead of replaying every intermediate copy.
                    while let Ok(newer) = serve.try_recv() {
                        offer = newer;
                    }
                    let (offer, receipt) = offer.split_receipt();
                    self.take_ownership(PendingOwnership {
                        data: offer.into(),
                        receipt,
                    });
                    true
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => false,
                Err(std::sync::mpsc::TryRecvError::Empty) => true,
            },
            // Remote requests are applied in order: an announcement and the
            // data that fulfils it must not be coalesced into one.
            #[cfg(feature = "remote-x11")]
            ServeSource::Remote(serve) => {
                if let Some((content, acknowledged)) = serve.0.take_restriction() {
                    if !self.apply_restriction(content) {
                        serve.0.abort();
                        self.abort_remote();
                        return false;
                    }
                    let _ = acknowledged.try_send(());
                }
                for _ in 0..remote_mailbox::SERVE_BUDGET {
                    if serve.0.aborted() {
                        self.abort_remote();
                        return false;
                    }
                    let Some(request) = serve.0.pop() else { break };
                    self.apply_remote(request);
                }
                true
            }
        }
    }

    /// Terminal overflow/disconnection path, independent of queue capacity.
    #[cfg(feature = "remote-x11")]
    fn abort_remote(&mut self) {
        self.answer_deferred(false);
        self.pending_offer = None;
        self.cancel_capture();
        // Destroying our owner window releases only our selection. A
        // get-owner/set-NONE pair could race with another application's copy.
        // This is terminal, so no later timestamp probe may reclaim it.
        let _ = self.conn.destroy_window(self.window);
        self.offered = None;
        self.ownership_time = None;
        self.outgoing_incr.clear();
        let _ = self.conn.flush();
    }

    #[cfg(feature = "remote-x11")]
    fn apply_remote(&mut self, request: RemoteServe) {
        if self
            .remote_policy
            .as_ref()
            .is_some_and(|policy| !policy.allows(request.content_bit()))
        {
            request.reject_unpublished();
            return;
        }
        match request {
            RemoteServe::Text(text) => self.take_ownership(PendingOwnership {
                data: OfferedData::Text(text.into_bytes().into()),
                receipt: None,
            }),
            RemoteServe::Announce { token, kind } => self.take_ownership(PendingOwnership {
                data: OfferedData::Pending { token, kind },
                receipt: None,
            }),
            RemoteServe::FulfilPng { token, png } => {
                self.fulfil(token, OfferedData::Png(png.into()), None);
            }
            RemoteServe::FulfilFiles {
                token,
                paths,
                receipt,
            } => {
                self.fulfil(token, OfferedData::files(&paths), Some(&receipt));
            }
            RemoteServe::Abandon { token } => self.abandon(token),
        }
    }

    /// Replace pending offer `token` with its data and answer the pastes
    /// that were waiting for it. A token that is no longer current — the
    /// user copied something else meanwhile — is ignored: the late data must
    /// not take the clipboard back.
    #[cfg(feature = "remote-x11")]
    fn fulfil(&mut self, token: u64, data: OfferedData, receipt: Option<&FileFulfilReceipt>) {
        let installed = install_remote_fulfil(
            token,
            data,
            self.pending_offer.as_mut().map(|p| &mut p.data),
            &mut self.offered,
            receipt,
        );
        if installed == Some(true) {
            self.answer_deferred(true);
        }
    }

    #[cfg(feature = "remote-x11")]
    fn abandon(&mut self, token: u64) {
        if let Some(pending) = self.pending_offer.as_ref()
            && pending.data.pending_token() == Some(token)
        {
            self.pending_offer = None;
            return;
        }
        if self.offered.as_ref().and_then(OfferedData::pending_token) != Some(token) {
            return;
        }
        self.answer_deferred(false);
        // Nothing will ever be served; give CLIPBOARD up rather than keep
        // advertising targets that can only be refused.
        if let Some(time) = self.ownership_time {
            let _ = self
                .conn
                .set_selection_owner(x11rb::NONE, self.atoms.clipboard, time);
            let _ = self.conn.flush();
        }
        self.offered = None;
        self.ownership_time = None;
    }

    /// Serve (or refuse) every parked paste.
    #[cfg(feature = "remote-x11")]
    fn answer_deferred(&mut self, serve: bool) {
        for request in std::mem::take(&mut self.deferred) {
            let served =
                serve && self.serve_target(request.requestor, request.property, request.target);
            self.notify_requestor(
                request.time,
                request.requestor,
                request.selection,
                request.target,
                served.then_some(request.property),
            );
        }
        let _ = self.conn.flush();
    }

    #[cfg(feature = "remote-x11")]
    fn expire_deferred(&mut self) {
        let now = std::time::Instant::now();
        if self.deferred.iter().all(|request| request.deadline > now) {
            return;
        }
        let (expired, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.deferred)
            .into_iter()
            .partition(|request| request.deadline <= now);
        self.deferred = waiting;
        if !expired.is_empty() {
            log::debug!("clipboard: refusing a paste whose data did not arrive in time");
        }
        for request in expired {
            self.notify_requestor(
                request.time,
                request.requestor,
                request.selection,
                request.target,
                None,
            );
        }
        let _ = self.conn.flush();
    }

    fn notify_requestor(
        &self,
        time: u32,
        requestor: Window,
        selection: Atom,
        target: Atom,
        property: Option<Atom>,
    ) {
        let notify = SelectionNotifyEvent {
            response_type: x11rb::protocol::xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time,
            requestor,
            selection,
            target,
            property: property.unwrap_or(x11rb::NONE),
        };
        let _ = self
            .conn
            .send_event(false, requestor, EventMask::NO_EVENT, notify);
    }

    fn wait_for_work(
        &self,
        wake: &crate::backend::update_notifier::AsyncUpdateNotifier,
        deadline: Option<std::time::Instant>,
    ) -> std::io::Result<()> {
        let mut descriptors = [
            libc::pollfd {
                fd: self.conn.stream().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake.as_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let ready = unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as libc::nfds_t,
                self.next_poll_timeout_ms(deadline),
            )
        };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                return Ok(());
            }
            return Err(error);
        }
        if descriptors[0].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "X11 clipboard connection closed",
            ));
        }
        if descriptors[1].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "clipboard worker wake fd closed",
            ));
        }
        Ok(())
    }

    fn next_poll_timeout_ms(&self, deadline: Option<std::time::Instant>) -> i32 {
        let now = std::time::Instant::now();
        let outgoing = self.outgoing_incr.values().map(|transfer| {
            OUTGOING_INCR_TIMEOUT
                .saturating_sub(now.saturating_duration_since(transfer.last_activity))
        });
        let incoming = self.capture.as_ref().map(|capture| {
            let timeout = INCOMING_CONVERSION_TIMEOUT
                .saturating_sub(now.saturating_duration_since(capture.last_activity));
            if Self::awaits_retry(capture) {
                timeout.min(
                    UNANSWERED_CONVERSION_RETRY
                        .saturating_sub(now.saturating_duration_since(capture.requested_at)),
                )
            } else {
                timeout
            }
        });
        let handoff = deadline.map(|deadline| deadline.saturating_duration_since(now));
        #[cfg(feature = "remote-x11")]
        let deferred = self
            .deferred
            .iter()
            .map(|request| request.deadline.saturating_duration_since(now));
        #[cfg(not(feature = "remote-x11"))]
        let deferred = std::iter::empty();
        outgoing
            .chain(incoming)
            .chain(handoff)
            .chain(deferred)
            .min()
            .map_or(-1, |remaining| {
                if remaining.is_zero() {
                    0
                } else {
                    let millis = remaining.as_nanos().div_ceil(1_000_000);
                    i32::try_from(millis).unwrap_or(i32::MAX)
                }
            })
    }

    /// Observe a sticky failure throughout the bounded manager handoff,
    /// including between X events so an abort cannot be starved by traffic.
    fn abort_failed_handoff(&mut self, _source: &ServeSource) -> bool {
        #[cfg(feature = "remote-x11")]
        if let ServeSource::Remote(source) = _source
            && source.0.aborted()
        {
            self.abort_remote();
            return true;
        }
        false
    }

    fn fresh_handoff_timestamp(
        &mut self,
        wake: &crate::backend::update_notifier::AsyncUpdateNotifier,
        deadline: std::time::Instant,
        source: &ServeSource,
    ) -> Option<u32> {
        self.conn
            .change_property8(
                PropMode::APPEND,
                self.window,
                self.atoms.handoff_timestamp_probe,
                AtomEnum::INTEGER,
                &[],
            )
            .ok()?
            .check()
            .ok()?;
        self.conn.flush().ok()?;

        loop {
            if self.abort_failed_handoff(source) {
                return None;
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            let mut handled = false;
            for _ in 0..256 {
                if self.abort_failed_handoff(source) {
                    return None;
                }
                let event = match self.conn.poll_for_event() {
                    Ok(Some(event)) => event,
                    Ok(None) => break,
                    Err(_) => return None,
                };
                handled = true;
                if let x11rb::protocol::Event::PropertyNotify(property) = &event
                    && property.window == self.window
                    && property.atom == self.atoms.handoff_timestamp_probe
                    && property.state == Property::NEW_VALUE
                {
                    let _ = self
                        .conn
                        .delete_property(self.window, self.atoms.handoff_timestamp_probe);
                    return Some(property.time);
                }
                let _ = self.handle(&event);
            }
            if !handled && self.wait_for_work(wake, Some(deadline)).is_err() {
                return None;
            }
        }
    }

    fn handoff_to_clipboard_manager(
        &mut self,
        wake: &crate::backend::update_notifier::AsyncUpdateNotifier,
        source: &ServeSource,
    ) {
        if self.abort_failed_handoff(source) {
            return;
        }
        self.shutting_down = true;
        self.cancel_capture();
        if self.pending_offer.is_none()
            && (self.offered.is_none() || !self.is_current_owner(self.window))
        {
            return;
        }
        let manager = self
            .conn
            .get_selection_owner(self.atoms.clipboard_manager)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .map(|reply| reply.owner)
            .unwrap_or(x11rb::NONE);
        if manager == x11rb::NONE {
            return;
        }
        let deadline = std::time::Instant::now() + CLIPBOARD_HANDOFF_TIMEOUT;
        let Some(timestamp) = self.fresh_handoff_timestamp(wake, deadline, source) else {
            return;
        };
        if self.abort_failed_handoff(source) {
            return;
        }
        if self.pending_offer.is_some() {
            self.pending_probe_events = 0;
            let _ = self
                .conn
                .delete_property(self.window, self.atoms.timestamp_probe);
            if !self.acquire_pending_ownership(timestamp) {
                return;
            }
        }
        let Some(offered) = self.offered.as_ref() else {
            return;
        };
        // Data still in flight cannot be handed to a manager to keep.
        #[cfg(feature = "remote-x11")]
        if offered.pending_token().is_some() {
            return;
        }
        if !self.is_current_owner(self.window) {
            return;
        }
        let targets = offered.payload_targets(&self.atoms);
        let stored = self
            .conn
            .change_property32(
                PropMode::REPLACE,
                self.window,
                self.atoms.handoff_property,
                AtomEnum::ATOM,
                &targets,
            )
            .ok()
            .and_then(|cookie| cookie.check().ok())
            .is_some();
        if self.abort_failed_handoff(source) {
            return;
        }
        if !stored
            || self
                .conn
                .convert_selection(
                    self.window,
                    self.atoms.clipboard_manager,
                    self.atoms.save_targets,
                    self.atoms.handoff_property,
                    timestamp,
                )
                .is_err()
            || self.conn.flush().is_err()
        {
            return;
        }

        let mut manager_notified = false;
        let mut protocol_activity = false;
        let mut provisional_failure = None;
        loop {
            if self.abort_failed_handoff(source) {
                return;
            }
            if std::time::Instant::now() >= deadline {
                log::warn!("clipboard: SAVE_TARGETS handoff timed out");
                return;
            }
            self.expire_outgoing_incr();
            let mut handled = false;
            for _ in 0..256 {
                if self.abort_failed_handoff(source) {
                    return;
                }
                let event = match self.conn.poll_for_event() {
                    Ok(Some(event)) => event,
                    Ok(None) => break,
                    Err(_) => return,
                };
                handled = true;
                match &event {
                    x11rb::protocol::Event::SelectionNotify(notify)
                        if notify.requestor == self.window
                            && notify.selection == self.atoms.clipboard_manager
                            && notify.target == self.atoms.save_targets
                            && notify.time == timestamp =>
                    {
                        if notify.property == self.atoms.handoff_property {
                            manager_notified = true;
                        } else if notify.property == x11rb::NONE {
                            provisional_failure = Some(std::time::Instant::now());
                        }
                    }
                    x11rb::protocol::Event::SelectionRequest(request)
                        if request.selection == self.atoms.clipboard =>
                    {
                        protocol_activity = true;
                        self.on_selection_request(request);
                    }
                    _ => {
                        let _ = self.handle(&event);
                    }
                }
            }

            if (manager_notified || protocol_activity)
                && !self.is_current_owner(self.window)
                && self.outgoing_incr.is_empty()
            {
                log::debug!("clipboard: SAVE_TARGETS handoff completed");
                return;
            }
            if provisional_failure.is_some_and(|failed_at| {
                !protocol_activity && failed_at.elapsed() >= std::time::Duration::from_millis(100)
            }) {
                return;
            }
            let poll_deadline = crate::backend::clipboard_offer::x11_handoff_poll_deadline(
                deadline,
                provisional_failure,
                protocol_activity,
            );
            if !handled && self.wait_for_work(wake, Some(poll_deadline)).is_err() {
                return;
            }
        }
    }

    /// Route one X event. Returns a captured payload when a conversion completed.
    fn handle(&mut self, event: &x11rb::protocol::Event) -> Option<CopiedClipboard> {
        #[cfg(feature = "remote-x11")]
        if self.remote_policy.is_some()
            && self.capture.as_ref().is_some_and(|capture| {
                !self
                    .effective_profile()
                    .allows_conversion(capture.conversion)
            })
        {
            self.cancel_capture();
        }
        use x11rb::protocol::Event;
        match event {
            Event::XfixesSelectionNotify(e) => {
                self.on_owner_changed(e.owner, e.timestamp, e.selection_timestamp);
                None
            }
            Event::SelectionNotify(e) => self.on_selection_notify(e),
            Event::SelectionRequest(e) => {
                self.on_selection_request(e);
                None
            }
            Event::PropertyNotify(e) => self.on_property_notify(e),
            Event::SelectionClear(e) if e.owner == self.window => {
                // A queued clear from the previous offer may be delivered
                // after a new offer has reacquired CLIPBOARD. Confirm against
                // the server before discarding the new bytes.
                if !self.shutting_down
                    && self.pending_offer.is_none()
                    && !self.is_current_owner(self.window)
                {
                    #[cfg(feature = "remote-x11")]
                    self.answer_deferred(false);
                    self.offered = None;
                    self.pending_offer = None;
                    self.pending_probe_events = 0;
                    self.ownership_time = None;
                }
                None
            }
            Event::DestroyNotify(e) => {
                self.outgoing_incr
                    .retain(|(window, _), _| *window != e.window);
                if self
                    .capture
                    .as_ref()
                    .is_some_and(|capture| capture.window == e.window)
                {
                    self.capture = None;
                }
                None
            }
            _ => None,
        }
    }

    /// A new application took the clipboard: ask what it can offer.
    ///
    /// Ownership taken by JWM itself is ignored — serving an entry back must
    /// not re-record it as a fresh copy.
    fn on_owner_changed(&mut self, owner: Window, timestamp: u32, selection_timestamp: u32) {
        if self.shutting_down {
            if owner == self.window {
                self.ownership_time = Some(selection_timestamp);
            }
            return;
        }
        if owner == self.window {
            self.cancel_capture();
            self.ownership_time = Some(selection_timestamp);
            return;
        }
        if self.pending_offer.is_some() {
            return;
        }
        if owner == x11rb::NONE {
            self.cancel_capture();
            if !self.is_current_owner(self.window) {
                #[cfg(feature = "remote-x11")]
                self.answer_deferred(false);
                self.offered = None;
                self.pending_offer = None;
                self.pending_probe_events = 0;
                self.ownership_time = None;
            }
            return;
        }
        // Ignore an old XFixes notification that was already queued when a
        // newer local offer reacquired the selection.
        if self.is_current_owner(self.window) {
            return;
        }
        #[cfg(feature = "remote-x11")]
        self.answer_deferred(false);
        self.offered = None;
        self.pending_offer = None;
        self.pending_probe_events = 0;
        self.ownership_time = None;
        self.begin_capture(owner, timestamp);
    }

    fn is_current_owner(&self, owner: Window) -> bool {
        self.conn
            .get_selection_owner(self.atoms.clipboard)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .is_some_and(|reply| reply.owner == owner)
    }

    fn begin_capture(&mut self, owner: Window, request_time: u32) {
        if !self.effective_profile().capture_enabled {
            return;
        }
        self.cancel_capture();
        let window = match self.conn.generate_id() {
            Ok(window) => window,
            Err(error) => {
                log::debug!("clipboard: capture window id failed: {error}");
                return;
            }
        };
        let created = self
            .conn
            .create_window(
                0,
                window,
                self.window,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                0,
                &xproto::CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )
            .ok()
            .and_then(|cookie| cookie.check().ok())
            .is_some();
        if !created {
            return;
        }

        self.capture = Some(CaptureRequest {
            owner,
            window,
            request_time,
            conversion: Conversion::TargetList,
            target: self.atoms.targets,
            fallbacks: Vec::new(),
            incoming_incr: None,
            last_activity: std::time::Instant::now(),
            requested_at: std::time::Instant::now(),
            attempts: 1,
        });
        if self
            .conn
            .convert_selection(
                window,
                self.atoms.clipboard,
                self.atoms.targets,
                self.atoms.transfer,
                request_time,
            )
            .is_err()
        {
            self.cancel_capture();
            return;
        }
        let _ = self.conn.flush();
    }

    fn cancel_capture(&mut self) {
        let Some(capture) = self.capture.take() else {
            return;
        };
        let _ = self.conn.destroy_window(capture.window);
        let _ = self.conn.flush();
    }

    /// A conversion finished. Returns the copied payload once it has been asked
    /// for and delivered.
    fn on_selection_notify(&mut self, event: &SelectionNotifyEvent) -> Option<CopiedClipboard> {
        let Some(capture) = self.capture.as_ref() else {
            return None;
        };
        if event.requestor != capture.window
            || event.selection != self.atoms.clipboard
            || event.target != capture.target
        {
            return None;
        }
        if event.property == x11rb::NONE {
            // An owner may advertise a file list it then refuses to convert;
            // its text form can still be shared.
            #[cfg(feature = "remote-x11")]
            if capture.conversion == Conversion::Files {
                return self.try_next_candidate();
            }
            self.cancel_capture();
            return None;
        }
        if event.property != self.atoms.transfer {
            return None;
        }
        if !self.is_current_owner(capture.owner) {
            self.cancel_capture();
            return None;
        }

        let conversion = capture.conversion;
        let cap = self.conversion_cap(conversion);
        let reply = self
            .conn
            .get_property(
                true, // delete: the transfer property is ours to consume
                event.requestor,
                event.property,
                AtomEnum::ANY,
                0,
                (cap / 4 + 1) as u32,
            )
            .ok()?
            .reply()
            .ok()?;

        if reply.type_ == self.atoms.incr {
            let announced = reply.value32().and_then(|mut values| values.next());
            // ICCCM asks for one 32-bit lower bound, but xclip 0.13 — what
            // `maim | xclip` and most screenshot scripts use — announces
            // none, which dropped every image it served over ~1 MiB. The
            // bound is advisory either way: the cap is enforced chunk by
            // chunk below.
            let valid_announcement = reply.format == 32 && reply.value_len <= 1;
            if let Some(capture) = self.capture.as_mut() {
                capture.incoming_incr = Some(IncomingIncr {
                    bytes: Vec::with_capacity(
                        announced
                            .and_then(|bytes| usize::try_from(bytes).ok())
                            .unwrap_or_default()
                            .min(cap),
                    ),
                    oversized: conversion == Conversion::TargetList
                        || !valid_announcement
                        || announced.is_some_and(|bytes| bytes as usize > cap),
                });
                capture.last_activity = std::time::Instant::now();
            }
            return None;
        }
        if reply.bytes_after != 0 {
            let _ = self.conn.delete_property(event.requestor, event.property);
            self.cancel_capture();
            return None;
        }

        match conversion {
            Conversion::TargetList => {
                if reply.type_ != Atom::from(AtomEnum::ATOM) || reply.format != 32 {
                    self.cancel_capture();
                    return None;
                }
                let Some(values) = reply.value32() else {
                    self.cancel_capture();
                    return None;
                };
                let targets: Vec<Atom> = values.collect();
                self.request_payload_if_allowed(&targets);
                None
            }
            Conversion::Text => {
                if reply.type_ != event.target || reply.format != 8 {
                    self.cancel_capture();
                    return None;
                }
                self.cancel_capture();
                String::from_utf8(reply.value)
                    .ok()
                    .map(CopiedClipboard::Text)
            }
            Conversion::Png => {
                if reply.type_ != event.target || reply.format != 8 {
                    self.cancel_capture();
                    return None;
                }
                let mime = self.image_mime_for_atom(event.target);
                self.cancel_capture();
                crate::backend::clipboard_offer::image_offer_to_png_within(
                    &reply.value,
                    mime,
                    self.profile.image_bytes,
                )
                .map(CopiedClipboard::Png)
            }
            #[cfg(feature = "remote-x11")]
            Conversion::Files => {
                if reply.type_ != event.target || reply.format != 8 {
                    return self.try_next_candidate();
                }
                self.files_or_next_candidate(event.target, &reply.value)
            }
        }
    }

    fn conversion_cap(&self, conversion: Conversion) -> usize {
        match conversion {
            Conversion::TargetList => MAX_TARGET_LIST_BYTES,
            Conversion::Text => crate::backend::clipboard_offer::MAX_TEXT_BYTES,
            Conversion::Png => self.profile.image_bytes,
            #[cfg(feature = "remote-x11")]
            Conversion::Files => crate::backend::clipboard_offer::MAX_TEXT_BYTES,
        }
    }

    /// A delivered file list, or — when it names no local files — the next
    /// advertised form of the same copy.
    #[cfg(feature = "remote-x11")]
    fn files_or_next_candidate(&mut self, target: Atom, bytes: &[u8]) -> Option<CopiedClipboard> {
        let mime = if target == self.atoms.gnome_copied_files {
            crate::backend::clipboard_offer::GNOME_COPIED_FILES_MIME
        } else {
            crate::backend::clipboard_offer::URI_LIST_MIME
        };
        match crate::backend::clipboard_offer::parse_file_list(mime, bytes) {
            Some(paths) => {
                self.cancel_capture();
                Some(CopiedClipboard::Files(paths))
            }
            None => self.try_next_candidate(),
        }
    }

    /// Ask the owner for its next advertised form, or give up on this copy.
    #[cfg(feature = "remote-x11")]
    fn try_next_candidate(&mut self) -> Option<CopiedClipboard> {
        let next = self.capture.as_mut().and_then(|capture| {
            if capture.fallbacks.is_empty() {
                None
            } else {
                Some(capture.fallbacks.remove(0))
            }
        });
        match next {
            Some(candidate) => self.request_candidate(candidate),
            None => self.cancel_capture(),
        }
        None
    }

    /// Decide from the target list whether to ask for the payload at all.
    ///
    /// The names are resolved so the shared policy in
    /// `clipboard_offer` makes the call — one round trip on a copy,
    /// and worth it to keep one definition of "this is a secret". Text
    /// wins over PNG when both are advertised.
    fn request_payload_if_allowed(&mut self, targets: &[Atom]) {
        if !self.effective_profile().capture_enabled {
            self.cancel_capture();
            return;
        }
        let mut unique = std::collections::HashSet::with_capacity(targets.len());
        let targets: Vec<Atom> = targets
            .iter()
            .copied()
            .filter(|atom| unique.insert(*atom))
            .collect();
        if targets.len() > MAX_TARGET_ATOMS {
            log::debug!("clipboard: refusing an oversized TARGETS list");
            self.cancel_capture();
            return;
        }
        let names = {
            let cookies = targets
                .iter()
                .map(|atom| self.conn.get_atom_name(*atom))
                .collect::<Result<Vec<_>, _>>();
            cookies.ok().and_then(|cookies| {
                let mut names = Vec::with_capacity(cookies.len());
                for cookie in cookies {
                    let reply = cookie.reply().ok()?;
                    names.push(String::from_utf8_lossy(&reply.name).into_owned());
                }
                Some(names)
            })
        };
        let Some(names) = names else {
            // Secret classification is a privacy boundary: an atom we cannot
            // resolve must fail closed, never silently disappear.
            self.cancel_capture();
            return;
        };

        if crate::backend::clipboard_offer::is_secret(&names) {
            log::debug!("clipboard: offer marked secret, not reading it");
            self.cancel_capture();
            return;
        }
        let mut candidates = self
            .effective_profile()
            .capture_candidates(&self.atoms, &targets)
            .into_iter();
        let Some(first) = candidates.next() else {
            self.cancel_capture();
            return;
        };
        if let Some(capture) = self.capture.as_mut() {
            capture.fallbacks = candidates.collect();
        }
        self.request_candidate(first);
    }

    fn request_candidate(&mut self, (conversion, target): (Conversion, Atom)) {
        if !self.effective_profile().allows_conversion(conversion) {
            self.cancel_capture();
            return;
        }
        let Some(capture) = self.capture.as_ref() else {
            return;
        };
        let owner = capture.owner;
        let window = capture.window;
        let request_time = capture.request_time;
        if !self.is_current_owner(owner) {
            self.cancel_capture();
            return;
        }
        if let Some(capture) = self.capture.as_mut() {
            capture.conversion = conversion;
            capture.target = target;
            capture.incoming_incr = None;
            capture.last_activity = std::time::Instant::now();
            capture.requested_at = capture.last_activity;
            capture.attempts = 1;
        }

        let request = self
            .conn
            .convert_selection(
                window,
                self.atoms.clipboard,
                target,
                self.atoms.transfer,
                request_time,
            )
            .map(|_| ());
        if let Err(error) = request {
            log::debug!("clipboard: requesting payload failed: {error}");
            self.cancel_capture();
            return;
        }
        let _ = self.conn.flush();
    }

    /// Offer one payload to other applications by taking CLIPBOARD ownership.
    fn take_ownership(&mut self, offer: PendingOwnership) {
        self.cancel_capture();
        // Pastes parked behind a previous pending offer asked for data that
        // will now never be served.
        #[cfg(feature = "remote-x11")]
        self.answer_deferred(false);
        self.offered = None;
        self.ownership_time = None;
        self.pending_offer = Some(offer);
        self.pending_probe_events = self.pending_probe_events.saturating_add(1);
        let requested = self
            .conn
            .change_property8(
                PropMode::APPEND,
                self.window,
                self.atoms.timestamp_probe,
                AtomEnum::INTEGER,
                &[],
            )
            .ok()
            .and_then(|cookie| cookie.check().ok())
            .is_some();
        if !requested {
            self.pending_offer = None;
            self.pending_probe_events = 0;
        }
        let _ = self.conn.flush();
    }

    fn finish_pending_ownership(&mut self, timestamp: u32) {
        if self.pending_probe_events > 1 {
            self.pending_probe_events -= 1;
            return;
        }
        self.pending_probe_events = 0;
        self.acquire_pending_ownership(timestamp);
    }

    fn acquire_pending_ownership(&mut self, timestamp: u32) -> bool {
        let Some(PendingOwnership {
            data: offer,
            receipt,
        }) = self.pending_offer.take()
        else {
            return self.offered.is_some() && self.is_current_owner(self.window);
        };
        #[cfg(feature = "remote-x11")]
        if self
            .remote_policy
            .as_ref()
            .is_some_and(|policy| !policy.allows(offer.content_bit()))
        {
            return false;
        }
        self.offered = Some(offer);
        let set = self
            .conn
            .set_selection_owner(self.window, self.atoms.clipboard, timestamp)
            .ok()
            .and_then(|cookie| cookie.check().ok())
            .is_some();
        if !set || !self.is_current_owner(self.window) {
            self.offered = None;
            self.ownership_time = None;
            return false;
        }
        self.ownership_time = Some(timestamp);
        if let Some(receipt) = receipt {
            receipt.owned();
        }
        let _ = self.conn.flush();
        true
    }

    /// Answer a request for the entry JWM is offering.
    fn on_selection_request(&mut self, event: &SelectionRequestEvent) {
        // A requestor from before ICCCM sends property=None meaning "use the
        // target atom as the property".
        let property = if event.property == x11rb::NONE {
            event.target
        } else {
            event.property
        };

        let request_valid = event.owner == self.window
            && event.selection == self.atoms.clipboard
            && event.requestor != self.window
            && self
                .capture
                .as_ref()
                .is_none_or(|capture| event.requestor != capture.window)
            && self.offered.is_some()
            && x11_selection_time_is_valid(event.time, self.ownership_time);
        // Data still arriving from the other machine: park the paste and
        // answer it when the data lands (or refuse it at the deadline).
        #[cfg(feature = "remote-x11")]
        if request_valid
            && self
                .offered
                .as_ref()
                .is_some_and(|offered| offered.defers(&self.atoms, event.target))
            && self.deferred.len() < MAX_DEFERRED_REQUESTS
        {
            self.deferred.push(DeferredRequest {
                requestor: event.requestor,
                selection: event.selection,
                target: event.target,
                property,
                time: event.time,
                deadline: std::time::Instant::now() + DEFERRED_REQUEST_TIMEOUT,
            });
            return;
        }
        let served = if !request_valid {
            false
        } else if event.target == self.atoms.multiple {
            event.property != x11rb::NONE && self.serve_multiple(event.requestor, event.property)
        } else {
            self.serve_target(event.requestor, property, event.target)
        };

        self.notify_requestor(
            event.time,
            event.requestor,
            event.selection,
            event.target,
            served.then_some(property),
        );
        let _ = self.conn.flush();
    }

    fn serve_target(&mut self, requestor: Window, property: Atom, target: Atom) -> bool {
        let Some(offered) = self.offered.clone() else {
            return false;
        };
        #[cfg(feature = "remote-x11")]
        if self
            .remote_policy
            .as_ref()
            .is_some_and(|policy| !policy.allows(offered.content_bit()))
        {
            return false;
        }
        if target == self.atoms.targets {
            let targets = offered.targets(&self.atoms);
            return self
                .conn
                .change_property32(
                    PropMode::REPLACE,
                    requestor,
                    property,
                    AtomEnum::ATOM,
                    &targets,
                )
                .ok()
                .and_then(|cookie| cookie.check().ok())
                .is_some();
        }
        if target == self.atoms.timestamp {
            let Some(timestamp) = self.ownership_time else {
                return false;
            };
            return self
                .conn
                .change_property32(
                    PropMode::REPLACE,
                    requestor,
                    property,
                    AtomEnum::INTEGER,
                    &[timestamp],
                )
                .ok()
                .and_then(|cookie| cookie.check().ok())
                .is_some();
        }
        if target == self.atoms.save_targets {
            return self
                .conn
                .change_property8(PropMode::REPLACE, requestor, property, self.atoms.null, &[])
                .ok()
                .and_then(|cookie| cookie.check().ok())
                .is_some();
        }

        offered
            .payload_for(&self.atoms, target)
            .is_some_and(|data| {
                if data.len() <= X11_DIRECT_PROPERTY_BYTES.min(self.property_payload_bytes) {
                    self.conn
                        .change_property8(
                            PropMode::REPLACE,
                            requestor,
                            property,
                            target,
                            data.as_ref(),
                        )
                        .ok()
                        .and_then(|cookie| cookie.check().ok())
                        .is_some()
                } else {
                    self.begin_outgoing_incr(requestor, property, target, data)
                }
            })
    }

    /// Process ICCCM MULTIPLE pairs in order and acknowledge only after every
    /// independent conversion has either succeeded or had its target replaced
    /// with None in the ATOM_PAIR property.
    fn serve_multiple(&mut self, requestor: Window, property: Atom) -> bool {
        let reply = match self.conn.get_property(
            false,
            requestor,
            property,
            self.atoms.atom_pair,
            0,
            (X11_MAX_MULTIPLE_CONVERSIONS * 2) as u32,
        ) {
            Ok(cookie) => match cookie.reply() {
                Ok(reply) => reply,
                Err(_) => return false,
            },
            Err(_) => return false,
        };
        if reply.type_ != self.atoms.atom_pair || reply.format != 32 || reply.bytes_after != 0 {
            return false;
        }
        let Some(values) = reply.value32() else {
            return false;
        };
        let mut pairs: Vec<Atom> = values.collect();
        if pairs.is_empty() || !pairs.len().is_multiple_of(2) {
            return false;
        }

        for pair in pairs.chunks_exact_mut(2) {
            let target = pair[0];
            let destination = pair[1];
            if destination == x11rb::NONE
                || destination == property
                || target == self.atoms.multiple
                || !self.serve_target(requestor, destination, target)
            {
                pair[0] = x11rb::NONE;
            }
        }

        self.conn
            .change_property32(
                PropMode::REPLACE,
                requestor,
                property,
                self.atoms.atom_pair,
                &pairs,
            )
            .ok()
            .and_then(|cookie| cookie.check().ok())
            .is_some()
    }

    /// Announce an ICCCM INCR transfer. The property must contain a 32-bit
    /// lower bound for the byte count; an empty announcement is the xclip
    /// 0.13 bug that makes payloads just over 1 MiB fail in many clients.
    fn begin_outgoing_incr(
        &mut self,
        requestor: Window,
        property: Atom,
        target: Atom,
        data: std::sync::Arc<[u8]>,
    ) -> bool {
        let key = (requestor, property);
        if self.outgoing_incr.contains_key(&key) {
            return false;
        }
        let active_bytes = self.outgoing_incr.values().fold(0usize, |total, transfer| {
            total.saturating_add(transfer.data.len())
        });
        let requestor_transfers = self
            .outgoing_incr
            .keys()
            .filter(|(window, _)| *window == requestor)
            .count();
        if self.outgoing_incr.len() >= MAX_OUTGOING_INCR_TRANSFERS
            || requestor_transfers >= MAX_OUTGOING_INCR_PER_REQUESTOR
            || active_bytes.saturating_add(data.len()) > X11_MAX_ACTIVE_INCR_BYTES
        {
            log::warn!("clipboard: refusing INCR transfer; too many requesters are stalled");
            return false;
        }
        let Ok(total) = u32::try_from(data.len()) else {
            log::warn!("clipboard: refusing INCR transfer larger than 4 GiB");
            return false;
        };

        let watching = self
            .conn
            .change_window_attributes(
                requestor,
                &xproto::ChangeWindowAttributesAux::new()
                    .event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY),
            )
            .ok()
            .and_then(|cookie| cookie.check().ok())
            .is_some();
        if !watching {
            return false;
        }
        let announced = self
            .conn
            .change_property32(
                PropMode::REPLACE,
                requestor,
                property,
                self.atoms.incr,
                &[total],
            )
            .ok()
            .and_then(|cookie| cookie.check().ok())
            .is_some();
        if !announced {
            self.stop_watching_requestor_if_idle(requestor);
            return false;
        }

        self.outgoing_incr.insert(
            key,
            OutgoingIncr {
                #[cfg(feature = "remote-x11")]
                content_bit: self.offered.as_ref().map_or(0, OfferedData::content_bit),
                target,
                data,
                offset: 0,
                last_activity: std::time::Instant::now(),
            },
        );
        true
    }

    /// The requestor deletes the property once for every chunk it is ready to
    /// consume. After the last data chunk, one more delete is answered with a
    /// zero-length property, which terminates the transfer.
    fn on_property_notify(&mut self, event: &PropertyNotifyEvent) -> Option<CopiedClipboard> {
        if event.state == Property::NEW_VALUE
            && event.window == self.window
            && event.atom == self.atoms.timestamp_probe
        {
            let _ = self
                .conn
                .delete_property(self.window, self.atoms.timestamp_probe);
            self.finish_pending_ownership(event.time);
            return None;
        }
        if event.state == Property::DELETE {
            let key = (event.window, event.atom);
            let Some(transfer) = self.outgoing_incr.get_mut(&key) else {
                return None;
            };
            #[cfg(feature = "remote-x11")]
            if let Some(policy) = &self.remote_policy {
                transfer.restrict(policy.content());
            }
            let (range, terminal) = next_x11_incr_chunk_with_limit(
                transfer.data.len(),
                &mut transfer.offset,
                X11_INCR_CHUNK_BYTES.min(self.property_payload_bytes),
            );
            transfer.last_activity = std::time::Instant::now();
            let target = transfer.target;
            let data = std::sync::Arc::clone(&transfer.data);

            let sent = self
                .conn
                .change_property8(
                    PropMode::REPLACE,
                    event.window,
                    event.atom,
                    target,
                    &data[range],
                )
                .ok()
                .and_then(|cookie| cookie.check().ok())
                .is_some();
            if terminal || !sent {
                self.outgoing_incr.remove(&key);
                self.stop_watching_requestor_if_idle(event.window);
            }
            let _ = self.conn.flush();
            return None;
        }

        if event.state == Property::NEW_VALUE {
            return self.on_incoming_incr_property(event.window, event.atom);
        }
        None
    }

    fn on_incoming_incr_property(
        &mut self,
        window: Window,
        property: Atom,
    ) -> Option<CopiedClipboard> {
        let Some(capture) = self.capture.as_ref() else {
            return None;
        };
        if capture.window != window
            || property != self.atoms.transfer
            || capture.incoming_incr.is_none()
        {
            return None;
        }
        let owner = capture.owner;
        let conversion = capture.conversion;
        let target = capture.target;
        if !self.is_current_owner(owner) {
            self.cancel_capture();
            return None;
        }

        let peek = self
            .conn
            .get_property(false, window, property, AtomEnum::ANY, 0, 0)
            .ok()?
            .reply()
            .ok()?;
        let expected_format = match conversion {
            Conversion::TargetList => 32,
            _ => 8,
        };
        let valid_type = peek.type_ == target && peek.format == expected_format;

        if peek.bytes_after == 0 {
            let _ = self.conn.delete_property(window, property);
            let incoming = self
                .capture
                .as_mut()
                .and_then(|capture| capture.incoming_incr.take());
            let Some(incoming) = incoming else {
                return None;
            };
            if incoming.oversized || !valid_type {
                self.cancel_capture();
                return None;
            }
            return self.finish_incoming_conversion(conversion, incoming.bytes);
        }

        if conversion == Conversion::TargetList
            || peek.bytes_after > MAX_INCOMING_CHUNK_BYTES
            || !valid_type
        {
            let _ = self.conn.delete_property(window, property);
            if let Some(incoming) = self
                .capture
                .as_mut()
                .and_then(|capture| capture.incoming_incr.as_mut())
            {
                incoming.oversized = true;
            }
            if let Some(capture) = self.capture.as_mut() {
                capture.last_activity = std::time::Instant::now();
            }
            let _ = self.conn.flush();
            return None;
        }

        let reply = self
            .conn
            .get_property(
                true,
                window,
                property,
                AtomEnum::ANY,
                0,
                peek.bytes_after.div_ceil(4),
            )
            .ok()?
            .reply()
            .ok()?;
        let cap = self.conversion_cap(conversion);
        if let Some(capture) = self.capture.as_mut() {
            if let Some(incoming) = capture.incoming_incr.as_mut() {
                if reply.type_ != target
                    || reply.format != expected_format
                    || incoming.bytes.len().saturating_add(reply.value.len()) > cap
                {
                    incoming.oversized = true;
                } else if !incoming.oversized {
                    incoming.bytes.extend_from_slice(&reply.value);
                }
            }
            capture.last_activity = std::time::Instant::now();
        }
        None
    }

    fn finish_incoming_conversion(
        &mut self,
        conversion: Conversion,
        bytes: Vec<u8>,
    ) -> Option<CopiedClipboard> {
        match conversion {
            Conversion::TargetList => {
                let _ = bytes;
                self.cancel_capture();
                None
            }
            Conversion::Text => {
                self.cancel_capture();
                String::from_utf8(bytes).ok().map(CopiedClipboard::Text)
            }
            Conversion::Png => {
                let mime = self
                    .capture
                    .as_ref()
                    .map(|capture| self.image_mime_for_atom(capture.target))
                    .unwrap_or("image/png");
                self.cancel_capture();
                crate::backend::clipboard_offer::image_offer_to_png_within(
                    &bytes,
                    mime,
                    self.profile.image_bytes,
                )
                .map(CopiedClipboard::Png)
            }
            #[cfg(feature = "remote-x11")]
            Conversion::Files => {
                let target = self.capture.as_ref().map(|capture| capture.target)?;
                self.files_or_next_candidate(target, &bytes)
            }
        }
    }

    fn image_mime_for_atom(&self, atom: Atom) -> &'static str {
        if atom == self.atoms.image_jpeg {
            "image/jpeg"
        } else if atom == self.atoms.image_bmp {
            "image/bmp"
        } else {
            "image/png"
        }
    }

    fn expire_outgoing_incr(&mut self) {
        let now = std::time::Instant::now();
        let expired: Vec<Window> = self
            .outgoing_incr
            .iter()
            .filter_map(|(&(window, _), transfer)| {
                (now.saturating_duration_since(transfer.last_activity) >= OUTGOING_INCR_TIMEOUT)
                    .then_some(window)
            })
            .collect();
        self.outgoing_incr.retain(|_, transfer| {
            now.saturating_duration_since(transfer.last_activity) < OUTGOING_INCR_TIMEOUT
        });
        for window in expired {
            self.stop_watching_requestor_if_idle(window);
        }
        self.retry_unanswered_conversion(now);
        if self.capture.as_ref().is_some_and(|capture| {
            now.saturating_duration_since(capture.last_activity) >= INCOMING_CONVERSION_TIMEOUT
        }) {
            self.cancel_capture();
        }
    }

    /// A conversion nobody has answered yet that may be asked for again.
    fn awaits_retry(capture: &CaptureRequest) -> bool {
        capture.incoming_incr.is_none() && capture.attempts < MAX_CONVERSION_ATTEMPTS
    }

    fn retry_unanswered_conversion(&mut self, now: std::time::Instant) {
        if !self.effective_profile().capture_enabled {
            self.cancel_capture();
            return;
        }
        let Some(capture) = self.capture.as_mut() else {
            return;
        };
        if !Self::awaits_retry(capture)
            || now.saturating_duration_since(capture.requested_at) < UNANSWERED_CONVERSION_RETRY
        {
            return;
        }
        capture.attempts += 1;
        capture.requested_at = now;
        capture.last_activity = now;
        let (owner, window, target, time) = (
            capture.owner,
            capture.window,
            capture.target,
            capture.request_time,
        );
        if !self.is_current_owner(owner) {
            self.cancel_capture();
            return;
        }
        log::debug!("clipboard: owner did not answer; asking again");
        if self
            .conn
            .convert_selection(
                window,
                self.atoms.clipboard,
                target,
                self.atoms.transfer,
                time,
            )
            .is_err()
        {
            self.cancel_capture();
            return;
        }
        let _ = self.conn.flush();
    }

    fn stop_watching_requestor_if_idle(&self, requestor: Window) {
        if self
            .outgoing_incr
            .keys()
            .any(|(window, _)| *window == requestor)
        {
            return;
        }
        let _ = self.conn.change_window_attributes(
            requestor,
            &xproto::ChangeWindowAttributesAux::new().event_mask(EventMask::NO_EVENT),
        );
        let _ = self.conn.flush();
    }
}

fn publish_capture(
    captured: &CaptureSink,
    notifier: &std::sync::Mutex<Option<crate::backend::update_notifier::AsyncUpdateNotifier>>,
    payload: CopiedClipboard,
) -> bool {
    // Channel publication happens first. Once the eventfd is readable the
    // handler must be able to drain this payload without another timer tick.
    if !captured.send(payload) {
        return false;
    }
    let notifier = notifier
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some(notifier) = notifier {
        notifier.notify();
    }
    true
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "remote-x11")]
    #[test]
    fn file_fulfil_receipt_tracks_pending_current_and_stale_installation() {
        use super::*;
        for pending in [false, true] {
            let receipt = FileFulfilReceipt::default();
            let announcement = OfferedData::Pending {
                token: 7,
                kind: PendingKind::Files,
            };
            let mut pending_offer = pending.then_some(announcement);
            let mut offered = (!pending).then_some(OfferedData::Pending {
                token: 7,
                kind: PendingKind::Files,
            });
            let stale = FileFulfilReceipt::default();
            assert_eq!(
                install_remote_fulfil(
                    6,
                    OfferedData::files(&[]),
                    pending_offer.as_mut(),
                    &mut offered,
                    Some(&stale)
                ),
                None
            );
            assert!(stale.rejected_unpublished());
            assert_eq!(
                install_remote_fulfil(
                    7,
                    OfferedData::files(&[]),
                    pending_offer.as_mut(),
                    &mut offered,
                    Some(&receipt)
                ),
                Some(!pending)
            );
            receipt.reject_unpublished();
            assert!(
                !receipt.rejected_unpublished(),
                "accepted receipt cannot regress to rejected"
            );
        }
    }

    #[cfg(feature = "remote-x11")]
    #[test]
    fn remote_repeated_shutdown_signals_are_coalesced() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let shutdown = super::RemoteClipboardShutdown {
            shutdown: sender,
            wake: crate::backend::update_notifier::AsyncUpdateNotifier::new().unwrap(),
        };
        for _ in 0..10_000 {
            shutdown.stop();
        }
        assert_eq!(receiver.try_recv(), Ok(()));
        assert_eq!(
            receiver.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        );
    }

    #[cfg(feature = "remote-x11")]
    #[test]
    fn runtime_restriction_preserves_empty_incr_termination_without_payload() {
        let mut file = OutgoingIncr {
            content_bit: 4,
            target: 123,
            data: std::sync::Arc::from([1u8, 2, 3, 4]),
            offset: 2,
            last_activity: std::time::Instant::now(),
        };
        let old = std::sync::Arc::downgrade(&file.data);
        file.restrict(3);
        assert!(
            old.upgrade().is_none(),
            "revoked payload allocation is released"
        );
        assert!(file.data.is_empty());
        assert_eq!(file.offset, 0);
        assert_eq!(
            file.target, 123,
            "terminal chunk retains the original target"
        );
        file.restrict(3);
        let (range, terminal) =
            next_x11_incr_chunk_with_limit(file.data.len(), &mut file.offset, 2);
        assert!(terminal);
        assert!(file.data[range].is_empty());

        let mut image = OutgoingIncr {
            content_bit: 2,
            target: 124,
            data: std::sync::Arc::from([9u8, 8]),
            offset: 1,
            last_activity: std::time::Instant::now(),
        };
        image.restrict(3);
        assert_eq!(&*image.data, &[9, 8]);
        assert_eq!(image.offset, 1);
        image.restrict(1);
        let (range, terminal) =
            next_x11_incr_chunk_with_limit(image.data.len(), &mut image.offset, 2);
        assert!(terminal && image.data[range].is_empty());
        image.restrict(0);
        assert!(image.data.is_empty());
    }

    #[cfg(feature = "remote-x11")]
    #[test]
    fn noncanonical_remote_clipboard_options_fail_before_opening_display() {
        let options = RemoteClipboardOptions {
            images: false,
            ..remote_test_options()
        };
        let result = RemoteClipboard::start(Some("must-not-open-display"), options);
        assert!(matches!(result, Err(message) if message.contains("canonical")));
    }

    #[cfg(feature = "remote-x11")]
    #[test]
    fn remote_watcher_failure_is_latched_before_capture_close_wakes_consumer() {
        for panic in [false, true] {
            let queue = std::sync::Arc::new(remote_mailbox::ServeQueue::default());
            let (sender, receiver) = remote_mailbox::capture_channel();
            let endpoints = WatcherEndpoints {
                source: ServeSource::Remote(remote_mailbox::ServeReceiver(queue.clone())),
                sink: CaptureSink::Remote(sender),
            };
            let worker = std::thread::spawn(move || {
                let _endpoints = endpoints;
                assert!(!panic, "simulated abnormal watcher unwind");
                // An ordinary early/error return has the same close ordering.
            });
            assert!(
                receiver
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .is_err()
            );
            // Check before joining: observing capture closure must itself
            // guarantee failure is already visible to staging cleanup.
            assert!(queue.aborted());
            assert_eq!(worker.join().is_err(), panic);
        }
    }

    #[cfg(feature = "remote-x11")]
    #[test]
    fn remote_clean_handoff_closes_capture_without_latching_failure() {
        let queue = std::sync::Arc::new(remote_mailbox::ServeQueue::default());
        let (sender, receiver) = remote_mailbox::capture_channel();
        let endpoints = WatcherEndpoints {
            source: ServeSource::Remote(remote_mailbox::ServeReceiver(queue.clone())),
            sink: CaptureSink::Remote(sender),
        };
        assert!(queue.close_normally());
        drop(endpoints);
        assert!(receiver.recv_timeout(std::time::Duration::ZERO).is_err());
        assert!(!queue.aborted());
    }

    // Synthetic atoms keep candidate-policy tests independent of X11 and the
    // user's real clipboard. Only identity matters to candidate selection.
    fn policy_atoms() -> Atoms {
        Atoms {
            clipboard: 1,
            targets: 2,
            multiple: 3,
            timestamp: 4,
            atom_pair: 5,
            timestamp_probe: 6,
            clipboard_manager: 7,
            save_targets: 8,
            null: 9,
            handoff_property: 10,
            handoff_timestamp_probe: 11,
            utf8_string: 12,
            text_plain_utf8: 13,
            text_plain: 14,
            image_png: 15,
            image_jpeg: 16,
            image_bmp: 17,
            #[cfg(feature = "remote-x11")]
            gnome_copied_files: 18,
            #[cfg(feature = "remote-x11")]
            uri_list: 19,
            incr: 20,
            transfer: 21,
        }
    }

    #[test]
    fn receive_only_never_selects_or_starts_capture() {
        let atoms = policy_atoms();
        let profile = CaptureProfile {
            capture_enabled: false,
            ..CaptureProfile::HISTORY
        };
        assert!(!profile.capture_existing());
        for conversion in [Conversion::TargetList, Conversion::Text, Conversion::Png] {
            assert!(!profile.allows_conversion(conversion));
        }
        assert!(
            profile
                .capture_candidates(&atoms, &[atoms.text_plain_utf8, atoms.image_png])
                .is_empty()
        );
        #[cfg(feature = "remote-x11")]
        {
            let profile = CaptureProfile {
                files: true,
                ..profile
            };
            assert!(!profile.allows_conversion(Conversion::Files));
            assert!(
                profile
                    .capture_candidates(&atoms, &[atoms.gnome_copied_files, atoms.uri_list])
                    .is_empty()
            );
        }
    }

    #[test]
    fn text_only_never_requests_image_targets_or_conversions() {
        let atoms = policy_atoms();
        let profile = CaptureProfile {
            images: false,
            ..CaptureProfile::HISTORY
        };
        assert!(profile.allows_conversion(Conversion::Text));
        assert!(!profile.allows_conversion(Conversion::Png));
        assert!(
            profile
                .capture_candidates(
                    &atoms,
                    &[atoms.image_png, atoms.image_jpeg, atoms.image_bmp]
                )
                .is_empty()
        );
        assert_eq!(
            profile.capture_candidates(&atoms, &[atoms.image_png, atoms.text_plain_utf8]),
            vec![(Conversion::Text, atoms.text_plain_utf8)]
        );
        #[cfg(feature = "remote-x11")]
        {
            assert!(!profile.allows_conversion(Conversion::Files));
            assert!(
                profile
                    .capture_candidates(&atoms, &[atoms.gnome_copied_files, atoms.uri_list])
                    .is_empty()
            );
            assert_eq!(
                profile.capture_candidates(&atoms, &[atoms.uri_list, atoms.text_plain_utf8]),
                vec![(Conversion::Text, atoms.text_plain_utf8)]
            );
        }
    }

    #[test]
    fn enabled_capture_preserves_text_preference_and_image_fallback() {
        let atoms = policy_atoms();
        let profile = CaptureProfile::HISTORY;
        assert!(profile.capture_existing());
        assert!(profile.allows_conversion(Conversion::Png));
        assert_eq!(
            profile.capture_candidates(&atoms, &[atoms.image_png, atoms.image_jpeg]),
            vec![(Conversion::Png, atoms.image_png)]
        );
        assert_eq!(
            profile.capture_candidates(
                &atoms,
                &[atoms.text_plain, atoms.text_plain_utf8, atoms.image_png]
            ),
            vec![(Conversion::Text, atoms.text_plain_utf8)]
        );
        assert!(
            !CaptureProfile {
                capture_existing: false,
                ..profile
            }
            .capture_existing()
        );
    }

    use super::*;

    #[cfg(any(feature = "backend-x11rb", feature = "remote-x11"))]
    fn wait_for_selection_owner(conn: &RustConnection, selection: Atom) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if conn
                .get_selection_owner(selection)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .is_some_and(|reply| reply.owner != x11rb::NONE)
            {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "native clipboard did not take selection ownership"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    #[cfg(feature = "backend-x11rb")]
    fn receive_png(
        conn: &RustConnection,
        requestor: Window,
        clipboard: Atom,
        image_png: Atom,
        incr: Atom,
        property: Atom,
    ) -> (Vec<u8>, Option<u32>) {
        conn.convert_selection(
            requestor,
            clipboard,
            image_png,
            property,
            x11rb::CURRENT_TIME,
        )
        .unwrap();
        conn.flush().unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut bytes = Vec::new();
        let mut announced = None;
        let mut incremental = false;
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out receiving native image clipboard"
            );
            let Some(event) = conn.poll_for_event().unwrap() else {
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            };
            match event {
                x11rb::protocol::Event::SelectionNotify(event) if event.requestor == requestor => {
                    assert_ne!(event.property, x11rb::NONE, "selection was refused");
                    let reply = conn
                        .get_property(true, requestor, property, AtomEnum::ANY, 0, u32::MAX)
                        .unwrap()
                        .reply()
                        .unwrap();
                    if reply.type_ == incr {
                        let values: Vec<u32> = reply.value32().unwrap().collect();
                        assert_eq!(values.len(), 1, "INCR must announce one byte count");
                        announced = Some(values[0]);
                        incremental = true;
                    } else {
                        assert_eq!(reply.type_, image_png);
                        return (reply.value, None);
                    }
                }
                x11rb::protocol::Event::PropertyNotify(event)
                    if incremental
                        && event.window == requestor
                        && event.atom == property
                        && event.state == Property::NEW_VALUE =>
                {
                    let reply = conn
                        .get_property(true, requestor, property, AtomEnum::ANY, 0, u32::MAX)
                        .unwrap()
                        .reply()
                        .unwrap();
                    assert_eq!(reply.type_, image_png);
                    if reply.value.is_empty() {
                        return (bytes, announced);
                    }
                    bytes.extend_from_slice(&reply.value);
                }
                _ => {}
            }
        }
    }

    #[cfg(feature = "backend-x11rb")]
    fn convert_and_wait(
        conn: &RustConnection,
        requestor: Window,
        selection: Atom,
        target: Atom,
        property: Atom,
        time: u32,
    ) -> SelectionNotifyEvent {
        conn.convert_selection(requestor, selection, target, property, time)
            .unwrap();
        conn.flush().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "selection reply timed out"
            );
            match conn.poll_for_event().unwrap() {
                Some(x11rb::protocol::Event::SelectionNotify(event))
                    if event.requestor == requestor
                        && event.selection == selection
                        && event.target == target =>
                {
                    return event;
                }
                Some(_) => {}
                None => std::thread::sleep(std::time::Duration::from_millis(1)),
            }
        }
    }

    #[test]
    fn conversions_are_routed_by_the_replys_own_target() {
        // The regression this guards: routing by remembered state dropped
        // every other copy, because a late reply from the previous owner was
        // read as the new owner's target list.
        assert_ne!(Conversion::TargetList, Conversion::Text);
        assert_ne!(Conversion::Text, Conversion::Png);
    }

    #[test]
    fn capture_is_queued_before_its_completion_wake() {
        let notifier = crate::backend::update_notifier::AsyncUpdateNotifier::new().unwrap();
        let slot = std::sync::Mutex::new(Some(notifier.clone()));
        let (send, receive) = std::sync::mpsc::channel();

        assert!(publish_capture(
            &CaptureSink::History(send),
            &slot,
            CopiedClipboard::Text("ready".to_string())
        ));
        assert_eq!(notifier.drain().unwrap(), 1);
        assert_eq!(
            receive.try_recv().unwrap(),
            CapturedClipboard::Text("ready".to_string())
        );
    }

    #[cfg(feature = "backend-x11rb")]
    #[test]
    fn native_png_offer_serves_payload_beyond_xclips_one_mib_cliff() {
        let x11 = crate::backend::clipboard_offer::IsolatedXvfb::acquire();
        let display = x11.name();
        let clipboard_owner = Clipboard::start(Some(display)).unwrap();
        let (conn, screen_num) = x11rb::connect(Some(display)).unwrap();
        let root = conn.setup().roots[screen_num].root;
        let clipboard = intern(&conn, "CLIPBOARD").unwrap();
        let image_png = intern(&conn, "image/png").unwrap();
        let incr = intern(&conn, "INCR").unwrap();
        let property = intern(&conn, "JWM_TEST_IMAGE").unwrap();
        let requestor = conn.generate_id().unwrap();
        conn.create_window(
            0,
            requestor,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &xproto::CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .unwrap()
        .check()
        .unwrap();

        let expected: Vec<u8> = (0..1_053_049).map(|index| index as u8).collect();
        assert!(clipboard_owner.image_sender().send_png(expected.clone()));
        wait_for_selection_owner(&conn, clipboard);

        let (actual, announced) =
            receive_png(&conn, requestor, clipboard, image_png, incr, property);
        assert_eq!(announced, Some(expected.len() as u32));
        assert_eq!(actual, expected);
        drop(clipboard_owner);
        std::thread::sleep(std::time::Duration::from_millis(30));
    }

    #[cfg(feature = "backend-x11rb")]
    #[test]
    fn native_owner_metadata_multiple_and_direct_round_trip() {
        let x11 = crate::backend::clipboard_offer::IsolatedXvfb::acquire();
        let display = x11.name();
        let clipboard_owner = Clipboard::start(Some(display)).unwrap();
        let (conn, screen_num) = x11rb::connect(Some(display)).unwrap();
        let root = conn.setup().roots[screen_num].root;
        let clipboard = intern(&conn, "CLIPBOARD").unwrap();
        let targets = intern(&conn, "TARGETS").unwrap();
        let multiple = intern(&conn, "MULTIPLE").unwrap();
        let timestamp = intern(&conn, "TIMESTAMP").unwrap();
        let save_targets = intern(&conn, "SAVE_TARGETS").unwrap();
        let null = intern(&conn, "NULL").unwrap();
        let atom_pair = intern(&conn, "ATOM_PAIR").unwrap();
        let utf8 = intern(&conn, "UTF8_STRING").unwrap();
        let unknown = intern(&conn, "JWM_TEST_UNKNOWN_TARGET").unwrap();
        let requestor = conn.generate_id().unwrap();
        conn.create_window(
            0,
            requestor,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &xproto::CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .unwrap()
        .check()
        .unwrap();
        let receipt = clipboard_owner
            .offer_with_receipt(CapturedClipboard::Text("hello π".into()))
            .unwrap();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while receipt.status() == crate::backend::clipboard_offer::ClipboardReceiptStatus::Pending
            && std::time::Instant::now() < until
        {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            receipt.status(),
            crate::backend::clipboard_offer::ClipboardReceiptStatus::Owned
        );

        wait_for_selection_owner(&conn, clipboard);

        let targets_property = intern(&conn, "JWM_TEST_TARGETS").unwrap();
        let notify = convert_and_wait(
            &conn,
            requestor,
            clipboard,
            targets,
            targets_property,
            x11rb::CURRENT_TIME,
        );
        assert_eq!(notify.property, targets_property);
        let reply = conn
            .get_property(true, requestor, targets_property, AtomEnum::ANY, 0, 64)
            .unwrap()
            .reply()
            .unwrap();
        assert_eq!(reply.type_, Atom::from(AtomEnum::ATOM));
        assert_eq!(reply.format, 32);
        let advertised: Vec<Atom> = reply.value32().unwrap().collect();
        for required in [targets, multiple, timestamp, save_targets, utf8] {
            assert!(advertised.contains(&required));
        }

        let save_property = intern(&conn, "JWM_TEST_SAVE_TARGETS").unwrap();
        let notify = convert_and_wait(
            &conn,
            requestor,
            clipboard,
            save_targets,
            save_property,
            x11rb::CURRENT_TIME,
        );
        assert_eq!(notify.property, save_property);
        let reply = conn
            .get_property(true, requestor, save_property, AtomEnum::ANY, 0, 1)
            .unwrap()
            .reply()
            .unwrap();
        assert_eq!(reply.type_, null);
        assert_eq!(reply.format, 8);
        assert!(reply.value.is_empty());

        let timestamp_property = intern(&conn, "JWM_TEST_TIMESTAMP").unwrap();
        let notify = convert_and_wait(
            &conn,
            requestor,
            clipboard,
            timestamp,
            timestamp_property,
            x11rb::CURRENT_TIME,
        );
        assert_eq!(notify.property, timestamp_property);
        let reply = conn
            .get_property(true, requestor, timestamp_property, AtomEnum::ANY, 0, 1)
            .unwrap()
            .reply()
            .unwrap();
        assert_eq!(reply.type_, Atom::from(AtomEnum::INTEGER));
        assert_eq!(reply.format, 32);
        let acquired = reply.value32().unwrap().next().unwrap();
        assert_ne!(acquired, 0);

        let text_property = intern(&conn, "JWM_TEST_TEXT").unwrap();
        let notify = convert_and_wait(&conn, requestor, clipboard, utf8, text_property, acquired);
        assert_eq!(notify.time, acquired);
        let reply = conn
            .get_property(true, requestor, text_property, AtomEnum::ANY, 0, 64)
            .unwrap()
            .reply()
            .unwrap();
        assert_eq!(reply.type_, utf8);
        assert_eq!(reply.format, 8);
        assert_eq!(reply.value, "hello π".as_bytes());

        let multiple_property = intern(&conn, "JWM_TEST_MULTIPLE").unwrap();
        let multiple_text = intern(&conn, "JWM_TEST_MULTIPLE_TEXT").unwrap();
        let multiple_time = intern(&conn, "JWM_TEST_MULTIPLE_TIME").unwrap();
        let multiple_bad = intern(&conn, "JWM_TEST_MULTIPLE_BAD").unwrap();
        conn.change_property32(
            PropMode::REPLACE,
            requestor,
            multiple_property,
            atom_pair,
            &[
                utf8,
                multiple_text,
                timestamp,
                multiple_time,
                unknown,
                multiple_bad,
            ],
        )
        .unwrap()
        .check()
        .unwrap();
        let notify = convert_and_wait(
            &conn,
            requestor,
            clipboard,
            multiple,
            multiple_property,
            acquired,
        );
        assert_eq!(notify.property, multiple_property);
        let reply = conn
            .get_property(false, requestor, multiple_property, AtomEnum::ANY, 0, 6)
            .unwrap()
            .reply()
            .unwrap();
        assert_eq!(reply.type_, atom_pair);
        assert_eq!(
            reply.value32().unwrap().collect::<Vec<_>>(),
            vec![
                utf8,
                multiple_text,
                timestamp,
                multiple_time,
                x11rb::NONE,
                multiple_bad,
            ]
        );

        let stale_property = intern(&conn, "JWM_TEST_STALE").unwrap();
        let stale = convert_and_wait(
            &conn,
            requestor,
            clipboard,
            utf8,
            stale_property,
            acquired.wrapping_sub(1),
        );
        assert_eq!(stale.property, x11rb::NONE);

        drop(clipboard_owner);
        std::thread::sleep(std::time::Duration::from_millis(30));
    }

    #[cfg(feature = "backend-x11rb")]
    #[test]
    fn native_watcher_collects_and_drains_incoming_incr() {
        let x11 = crate::backend::clipboard_offer::IsolatedXvfb::acquire();
        let display = x11.name();
        let clipboard_watcher = Clipboard::start(Some(display)).unwrap();
        let (conn, screen_num) = x11rb::connect(Some(display)).unwrap();
        let root = conn.setup().roots[screen_num].root;
        let clipboard = intern(&conn, "CLIPBOARD").unwrap();
        let targets = intern(&conn, "TARGETS").unwrap();
        let utf8 = intern(&conn, "UTF8_STRING").unwrap();
        let incr = intern(&conn, "INCR").unwrap();
        let owner = conn.generate_id().unwrap();
        conn.create_window(
            0,
            owner,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &xproto::CreateWindowAux::new(),
        )
        .unwrap()
        .check()
        .unwrap();
        conn.set_selection_owner(owner, clipboard, x11rb::CURRENT_TIME)
            .unwrap()
            .check()
            .unwrap();

        let expected = "incoming INCR π stays intact".as_bytes().to_vec();
        let mut transfer: Option<(Window, Atom, usize)> = None;
        let mut terminal_sent = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !terminal_sent {
            assert!(std::time::Instant::now() < deadline, "fake owner timed out");
            let Some(event) = conn.poll_for_event().unwrap() else {
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            };
            match event {
                x11rb::protocol::Event::SelectionRequest(event) if event.selection == clipboard => {
                    let property = if event.property == x11rb::NONE {
                        event.target
                    } else {
                        event.property
                    };
                    let served = if event.target == targets {
                        conn.change_property32(
                            PropMode::REPLACE,
                            event.requestor,
                            property,
                            AtomEnum::ATOM,
                            &[targets, utf8],
                        )
                        .ok()
                        .and_then(|cookie| cookie.check().ok())
                        .is_some()
                    } else if event.target == utf8 {
                        let watching = conn
                            .change_window_attributes(
                                event.requestor,
                                &xproto::ChangeWindowAttributesAux::new()
                                    .event_mask(EventMask::PROPERTY_CHANGE),
                            )
                            .ok()
                            .and_then(|cookie| cookie.check().ok())
                            .is_some();
                        let announced = watching
                            && conn
                                .change_property32(
                                    PropMode::REPLACE,
                                    event.requestor,
                                    property,
                                    incr,
                                    &[expected.len() as u32],
                                )
                                .ok()
                                .and_then(|cookie| cookie.check().ok())
                                .is_some();
                        if announced {
                            transfer = Some((event.requestor, property, 0));
                        }
                        announced
                    } else {
                        false
                    };
                    conn.send_event(
                        false,
                        event.requestor,
                        EventMask::NO_EVENT,
                        SelectionNotifyEvent {
                            response_type: xproto::SELECTION_NOTIFY_EVENT,
                            sequence: 0,
                            time: event.time,
                            requestor: event.requestor,
                            selection: event.selection,
                            target: event.target,
                            property: if served { property } else { x11rb::NONE },
                        },
                    )
                    .unwrap();
                    conn.flush().unwrap();
                }
                x11rb::protocol::Event::PropertyNotify(event)
                    if event.state == Property::DELETE =>
                {
                    let Some((requestor, property, offset)) = transfer.as_mut() else {
                        continue;
                    };
                    if event.window != *requestor || event.atom != *property {
                        continue;
                    }
                    let end = offset.saturating_add(3).min(expected.len());
                    conn.change_property8(
                        PropMode::REPLACE,
                        *requestor,
                        *property,
                        utf8,
                        &expected[*offset..end],
                    )
                    .unwrap()
                    .check()
                    .unwrap();
                    terminal_sent = *offset == expected.len();
                    *offset = end;
                }
                _ => {}
            }
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let captured = clipboard_watcher.drain_captured();
            if !captured.is_empty() {
                assert_eq!(
                    captured,
                    vec![CapturedClipboard::Text(
                        String::from_utf8(expected).unwrap()
                    )]
                );
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "watcher did not publish incoming INCR"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        drop(clipboard_watcher);
        std::thread::sleep(std::time::Duration::from_millis(30));
    }

    #[cfg(feature = "backend-x11rb")]
    #[test]
    fn native_owner_hands_text_to_clipboard_manager_on_drop() {
        let x11 = crate::backend::clipboard_offer::IsolatedXvfb::acquire();
        let display = x11.name().to_string();
        let clipboard_owner = Clipboard::start(Some(display.as_str())).unwrap();
        assert!(clipboard_owner.set_text("persist across restart"));

        let (manager_ready_tx, manager_ready_rx) = std::sync::mpsc::channel();
        let (saved_tx, saved_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let manager = std::thread::spawn(move || {
            let (conn, screen_num) = x11rb::connect(Some(display.as_str())).unwrap();
            let root = conn.setup().roots[screen_num].root;
            let clipboard = intern(&conn, "CLIPBOARD").unwrap();
            let clipboard_manager = intern(&conn, "CLIPBOARD_MANAGER").unwrap();
            let save_targets = intern(&conn, "SAVE_TARGETS").unwrap();
            let utf8 = intern(&conn, "UTF8_STRING").unwrap();
            let data_property = intern(&conn, "JWM_TEST_MANAGER_DATA").unwrap();
            let window = conn.generate_id().unwrap();
            conn.create_window(
                0,
                window,
                root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                0,
                &xproto::CreateWindowAux::new(),
            )
            .unwrap()
            .check()
            .unwrap();
            conn.set_selection_owner(window, clipboard_manager, x11rb::CURRENT_TIME)
                .unwrap()
                .check()
                .unwrap();
            manager_ready_tx.send(()).unwrap();

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let mut handoff = None;
            loop {
                assert!(std::time::Instant::now() < deadline, "manager timed out");
                let Some(event) = conn.poll_for_event().unwrap() else {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    continue;
                };
                match event {
                    x11rb::protocol::Event::SelectionRequest(event)
                        if event.selection == clipboard_manager && event.target == save_targets =>
                    {
                        assert_ne!(event.property, x11rb::NONE);
                        let targets = conn
                            .get_property(
                                false,
                                event.requestor,
                                event.property,
                                AtomEnum::ANY,
                                0,
                                32,
                            )
                            .unwrap()
                            .reply()
                            .unwrap();
                        assert_eq!(targets.type_, Atom::from(AtomEnum::ATOM));
                        assert_eq!(targets.format, 32);
                        assert!(targets.value32().unwrap().any(|target| target == utf8));
                        handoff = Some((event.requestor, event.property, event.time));
                        conn.convert_selection(window, clipboard, utf8, data_property, event.time)
                            .unwrap();
                        conn.flush().unwrap();
                    }
                    x11rb::protocol::Event::SelectionNotify(event)
                        if event.requestor == window
                            && event.selection == clipboard
                            && event.target == utf8 =>
                    {
                        assert_eq!(event.property, data_property);
                        let data = conn
                            .get_property(true, window, data_property, AtomEnum::ANY, 0, 1024)
                            .unwrap()
                            .reply()
                            .unwrap();
                        assert_eq!(data.type_, utf8);
                        assert_eq!(data.format, 8);
                        let saved = String::from_utf8(data.value).unwrap();
                        let (requestor, property, time) = handoff.take().unwrap();
                        conn.set_selection_owner(window, clipboard, time)
                            .unwrap()
                            .check()
                            .unwrap();
                        conn.send_event(
                            false,
                            requestor,
                            EventMask::NO_EVENT,
                            SelectionNotifyEvent {
                                response_type: xproto::SELECTION_NOTIFY_EVENT,
                                sequence: 0,
                                time,
                                requestor,
                                selection: clipboard_manager,
                                target: save_targets,
                                property,
                            },
                        )
                        .unwrap();
                        conn.flush().unwrap();
                        assert_eq!(
                            conn.get_selection_owner(clipboard)
                                .unwrap()
                                .reply()
                                .unwrap()
                                .owner,
                            window
                        );
                        saved_tx.send(saved).unwrap();
                        release_rx.recv().unwrap();
                        return;
                    }
                    _ => {}
                }
            }
        });
        manager_ready_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();

        drop(clipboard_owner);
        assert_eq!(
            saved_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap(),
            "persist across restart"
        );
        release_tx.send(()).unwrap();
        manager.join().unwrap();
    }

    #[cfg(feature = "backend-x11rb")]
    #[test]
    fn native_worker_shutdown_does_not_wait_for_image_sender_clone() {
        let x11 = crate::backend::clipboard_offer::IsolatedXvfb::acquire();
        let clipboard = Clipboard::start(Some(x11.name())).unwrap();
        let image_sender = clipboard.image_sender();
        let started = std::time::Instant::now();
        drop(clipboard);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(!image_sender.send_png(vec![1, 2, 3, 4]));
    }

    #[cfg(all(feature = "backend-x11rb", feature = "remote-x11"))]
    #[test]
    fn native_split_setter_keeps_serving_after_capture_half_drops() {
        let x11 = crate::backend::clipboard_offer::IsolatedXvfb::acquire();
        let display = x11.name();
        let (captures, setter) =
            RemoteClipboard::start(Some(display), remote_test_options()).unwrap();
        drop(captures);
        assert!(setter.send(RemoteServe::Text("setter survives".into())));

        let (conn, screen_num) = x11rb::connect(Some(display)).unwrap();
        let root = conn.setup().roots[screen_num].root;
        let selection = intern(&conn, "CLIPBOARD").unwrap();
        let utf8 = intern(&conn, "UTF8_STRING").unwrap();
        let property = intern(&conn, "JWM_TEST_SPLIT_TEXT").unwrap();
        let requestor = conn.generate_id().unwrap();
        conn.create_window(
            0,
            requestor,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &xproto::CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .unwrap()
        .check()
        .unwrap();
        wait_for_selection_owner(&conn, selection);
        let notify = convert_and_wait(
            &conn,
            requestor,
            selection,
            utf8,
            property,
            x11rb::CURRENT_TIME,
        );
        assert_eq!(notify.property, property);
        let reply = conn
            .get_property(true, requestor, property, AtomEnum::ANY, 0, 64)
            .unwrap()
            .reply()
            .unwrap();
        assert_eq!(reply.value, b"setter survives");
        drop(setter);
    }

    #[cfg(feature = "remote-x11")]
    fn remote_test_options() -> RemoteClipboardOptions {
        RemoteClipboardOptions {
            send: true,
            images: true,
            files: true,
            image_bytes: 32 * 1024 * 1024,
            capture_existing: false,
        }
    }

    #[cfg(feature = "remote-x11")]
    fn input_only_window(conn: &RustConnection, root: Window) -> Window {
        let window = conn.generate_id().unwrap();
        conn.create_window(
            0,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &xproto::CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .unwrap()
        .check()
        .unwrap();
        window
    }

    /// Own CLIPBOARD and answer requests from `offers` until `done` holds.
    #[cfg(all(feature = "backend-x11rb", feature = "remote-x11"))]
    fn serve_fake_owner(
        conn: &RustConnection,
        owner: Window,
        offers: &[(Atom, Vec<u8>)],
        mut done: impl FnMut() -> bool,
    ) {
        let clipboard = intern(conn, "CLIPBOARD").unwrap();
        let targets = intern(conn, "TARGETS").unwrap();
        conn.set_selection_owner(owner, clipboard, x11rb::CURRENT_TIME)
            .unwrap()
            .check()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !done() {
            assert!(std::time::Instant::now() < deadline, "fake owner timed out");
            let Some(event) = conn.poll_for_event().unwrap() else {
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            };
            let x11rb::protocol::Event::SelectionRequest(event) = event else {
                continue;
            };
            let property = if event.property == x11rb::NONE {
                event.target
            } else {
                event.property
            };
            let served = if event.target == targets {
                let mut list = vec![targets];
                list.extend(offers.iter().map(|(atom, _)| *atom));
                conn.change_property32(
                    PropMode::REPLACE,
                    event.requestor,
                    property,
                    AtomEnum::ATOM,
                    &list,
                )
                .unwrap()
                .check()
                .is_ok()
            } else if let Some((atom, bytes)) =
                offers.iter().find(|(atom, _)| *atom == event.target)
            {
                conn.change_property8(PropMode::REPLACE, event.requestor, property, *atom, bytes)
                    .unwrap()
                    .check()
                    .is_ok()
            } else {
                false
            };
            conn.send_event(
                false,
                event.requestor,
                EventMask::NO_EVENT,
                SelectionNotifyEvent {
                    response_type: xproto::SELECTION_NOTIFY_EVENT,
                    sequence: 0,
                    time: event.time,
                    requestor: event.requestor,
                    selection: event.selection,
                    target: event.target,
                    property: if served { property } else { x11rb::NONE },
                },
            )
            .unwrap();
            conn.flush().unwrap();
        }
    }

    /// The SelectionNotify for `target`, if one arrives within `wait`.
    #[cfg(feature = "remote-x11")]
    fn selection_notify_within(
        conn: &RustConnection,
        requestor: Window,
        target: Atom,
        wait: std::time::Duration,
    ) -> Option<SelectionNotifyEvent> {
        let deadline = std::time::Instant::now() + wait;
        while std::time::Instant::now() < deadline {
            match conn.poll_for_event().unwrap() {
                Some(x11rb::protocol::Event::SelectionNotify(event))
                    if event.requestor == requestor && event.target == target =>
                {
                    return Some(event);
                }
                Some(_) => {}
                None => std::thread::sleep(std::time::Duration::from_millis(1)),
            }
        }
        None
    }

    #[cfg(all(feature = "backend-x11rb", feature = "remote-x11"))]
    #[test]
    fn remote_profile_shares_a_file_managers_copy_as_files_and_links_as_text() {
        let x11 = crate::backend::clipboard_offer::IsolatedXvfb::acquire();
        let display = x11.name();
        let (captures, _setter) =
            RemoteClipboard::start(Some(display), remote_test_options()).unwrap();
        let (conn, screen_num) = x11rb::connect(Some(display)).unwrap();
        let root = conn.setup().roots[screen_num].root;
        let gnome = intern(&conn, "x-special/gnome-copied-files").unwrap();
        let uri_list = intern(&conn, "text/uri-list").unwrap();
        let utf8 = intern(&conn, "UTF8_STRING").unwrap();
        let owner = input_only_window(&conn, root);

        let receive = |captures: &RemoteCaptures| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if let Some(copied) = captures
                    .recv_timeout(std::time::Duration::from_millis(1))
                    .unwrap()
                {
                    return Some(copied);
                }
                if std::time::Instant::now() >= deadline {
                    return None;
                }
            }
        };

        // Nautilus: the file list wins over the path text it also offers.
        let mut copied = None;
        serve_fake_owner(
            &conn,
            owner,
            &[
                (
                    gnome,
                    b"copy\nfile:///tmp/My%20Report.pdf\nfile:///tmp/dir".to_vec(),
                ),
                (utf8, b"/tmp/My Report.pdf\n/tmp/dir".to_vec()),
            ],
            || {
                copied = captures.recv_timeout(std::time::Duration::ZERO).unwrap();
                copied.is_some()
            },
        );
        assert_eq!(
            copied,
            Some(CopiedClipboard::Files(vec![
                std::path::PathBuf::from("/tmp/My Report.pdf"),
                std::path::PathBuf::from("/tmp/dir"),
            ]))
        );

        // A browser's link list names no local file: its text is shared.
        let mut copied = None;
        serve_fake_owner(
            &conn,
            owner,
            &[
                (uri_list, b"https://example.com/page\r\n".to_vec()),
                (utf8, b"https://example.com/page".to_vec()),
            ],
            || {
                copied = captures.recv_timeout(std::time::Duration::ZERO).unwrap();
                copied.is_some()
            },
        );
        assert_eq!(
            copied,
            Some(CopiedClipboard::Text("https://example.com/page".into()))
        );
        assert_eq!(receive(&captures), None, "each copy is captured once");
    }

    /// Serve `bytes` as `target` through INCR the way xclip 0.13 does: an
    /// announcement with no size, then ~1 MiB chunks.
    #[cfg(feature = "backend-x11rb")]
    fn serve_like_xclip(conn: &RustConnection, owner: Window, target: Atom, bytes: &[u8]) {
        let clipboard = intern(conn, "CLIPBOARD").unwrap();
        let targets = intern(conn, "TARGETS").unwrap();
        let incr = intern(conn, "INCR").unwrap();
        conn.set_selection_owner(owner, clipboard, x11rb::CURRENT_TIME)
            .unwrap()
            .check()
            .unwrap();
        let chunk = 1024 * 1024 - 1;
        let mut transfer: Option<(Window, Atom, usize)> = None;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "xclip stand-in timed out"
            );
            let Some(event) = conn.poll_for_event().unwrap() else {
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            };
            match event {
                x11rb::protocol::Event::SelectionRequest(event) => {
                    let property = event.property;
                    let served = if event.target == targets {
                        conn.change_property32(
                            PropMode::REPLACE,
                            event.requestor,
                            property,
                            AtomEnum::ATOM,
                            &[targets, target],
                        )
                        .unwrap()
                        .check()
                        .is_ok()
                    } else if event.target == target {
                        conn.change_window_attributes(
                            event.requestor,
                            &xproto::ChangeWindowAttributesAux::new()
                                .event_mask(EventMask::PROPERTY_CHANGE),
                        )
                        .unwrap()
                        .check()
                        .unwrap();
                        conn.change_property32(
                            PropMode::REPLACE,
                            event.requestor,
                            property,
                            incr,
                            &[],
                        )
                        .unwrap()
                        .check()
                        .unwrap();
                        transfer = Some((event.requestor, property, 0));
                        true
                    } else {
                        false
                    };
                    conn.send_event(
                        false,
                        event.requestor,
                        EventMask::NO_EVENT,
                        SelectionNotifyEvent {
                            response_type: xproto::SELECTION_NOTIFY_EVENT,
                            sequence: 0,
                            time: event.time,
                            requestor: event.requestor,
                            selection: event.selection,
                            target: event.target,
                            property: if served { property } else { x11rb::NONE },
                        },
                    )
                    .unwrap();
                    conn.flush().unwrap();
                }
                x11rb::protocol::Event::PropertyNotify(event)
                    if event.state == Property::DELETE =>
                {
                    let Some((requestor, property, offset)) = transfer.as_mut() else {
                        continue;
                    };
                    if event.window != *requestor || event.atom != *property {
                        continue;
                    }
                    let end = offset.saturating_add(chunk).min(bytes.len());
                    conn.change_property8(
                        PropMode::REPLACE,
                        *requestor,
                        *property,
                        target,
                        &bytes[*offset..end],
                    )
                    .unwrap()
                    .check()
                    .unwrap();
                    conn.flush().unwrap();
                    if *offset == bytes.len() {
                        return;
                    }
                    *offset = end;
                }
                _ => {}
            }
        }
    }

    #[cfg(feature = "backend-x11rb")]
    #[test]
    fn a_request_the_owner_drops_is_asked_again() {
        // xclip ignores a request that arrives while it serves another
        // reader's INCR transfer; the watcher must not give up on the copy.
        let x11 = crate::backend::clipboard_offer::IsolatedXvfb::acquire();
        let display = x11.name();
        let watcher = Clipboard::start(Some(display)).unwrap();
        let (conn, screen_num) = x11rb::connect(Some(display)).unwrap();
        let root = conn.setup().roots[screen_num].root;
        let clipboard = intern(&conn, "CLIPBOARD").unwrap();
        let targets = intern(&conn, "TARGETS").unwrap();
        let utf8 = intern(&conn, "UTF8_STRING").unwrap();
        let owner = conn.generate_id().unwrap();
        conn.create_window(
            0,
            owner,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &xproto::CreateWindowAux::new(),
        )
        .unwrap()
        .check()
        .unwrap();
        conn.set_selection_owner(owner, clipboard, x11rb::CURRENT_TIME)
            .unwrap()
            .check()
            .unwrap();
        let mut payload_requests = 0;
        let started = std::time::Instant::now();
        let deadline = started + std::time::Duration::from_secs(8);
        loop {
            let captured = watcher.drain_captured();
            if !captured.is_empty() {
                assert_eq!(
                    captured,
                    vec![CapturedClipboard::Text("second time".into())]
                );
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "a dropped request was never repeated"
            );
            let Some(x11rb::protocol::Event::SelectionRequest(event)) =
                conn.poll_for_event().unwrap()
            else {
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            };
            let served = if event.target == targets {
                conn.change_property32(
                    PropMode::REPLACE,
                    event.requestor,
                    event.property,
                    AtomEnum::ATOM,
                    &[targets, utf8],
                )
                .unwrap()
                .check()
                .is_ok()
            } else if event.target == utf8 {
                payload_requests += 1;
                if payload_requests == 1 {
                    // Dropped without any answer, as a busy xclip does.
                    continue;
                }
                conn.change_property8(
                    PropMode::REPLACE,
                    event.requestor,
                    event.property,
                    utf8,
                    b"second time",
                )
                .unwrap()
                .check()
                .is_ok()
            } else {
                false
            };
            conn.send_event(
                false,
                event.requestor,
                EventMask::NO_EVENT,
                SelectionNotifyEvent {
                    response_type: xproto::SELECTION_NOTIFY_EVENT,
                    sequence: 0,
                    time: event.time,
                    requestor: event.requestor,
                    selection: event.selection,
                    target: event.target,
                    property: if served { event.property } else { x11rb::NONE },
                },
            )
            .unwrap();
            conn.flush().unwrap();
        }
        assert_eq!(payload_requests, 2);
        assert!(started.elapsed() >= UNANSWERED_CONVERSION_RETRY);
    }

    #[cfg(feature = "backend-x11rb")]
    #[test]
    fn an_image_from_xclip_over_one_mib_is_captured() {
        let x11 = crate::backend::clipboard_offer::IsolatedXvfb::acquire();
        let display = x11.name();
        let watcher = Clipboard::start(Some(display)).unwrap();
        let (conn, screen_num) = x11rb::connect(Some(display)).unwrap();
        let root = conn.setup().roots[screen_num].root;
        let png_atom = intern(&conn, "image/png").unwrap();
        let owner = conn.generate_id().unwrap();
        conn.create_window(
            0,
            owner,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &xproto::CreateWindowAux::new(),
        )
        .unwrap()
        .check()
        .unwrap();
        // Noise does not compress: about 2.3 MiB of PNG, three INCR chunks.
        let mut state = 0x2545_f491_u32;
        let noise = image::RgbImage::from_fn(900, 900, |_, _| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            image::Rgb(state.to_le_bytes()[..3].try_into().unwrap())
        });
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(noise)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        assert!(png.len() > 2 * 1024 * 1024 && png.len() < MAX_IMAGE_HISTORY_BYTES);
        serve_like_xclip(&conn, owner, png_atom, &png);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let captured = watcher.drain_captured();
            if !captured.is_empty() {
                assert_eq!(captured, vec![CapturedClipboard::Png(png)]);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "an INCR image without a size announcement was dropped"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    #[cfg(all(feature = "backend-x11rb", feature = "remote-x11"))]
    #[test]
    fn a_paste_during_a_transfer_waits_for_the_data() {
        let x11 = crate::backend::clipboard_offer::IsolatedXvfb::acquire();
        let display = x11.name();
        let (_captures, setter) =
            RemoteClipboard::start(Some(display), remote_test_options()).unwrap();
        let (conn, screen_num) = x11rb::connect(Some(display)).unwrap();
        let root = conn.setup().roots[screen_num].root;
        let selection = intern(&conn, "CLIPBOARD").unwrap();
        let targets = intern(&conn, "TARGETS").unwrap();
        let gnome = intern(&conn, "x-special/gnome-copied-files").unwrap();
        let uri_list = intern(&conn, "text/uri-list").unwrap();
        let property = intern(&conn, "JWM_TEST_PENDING").unwrap();
        let requestor = input_only_window(&conn, root);

        assert!(setter.send(RemoteServe::Announce {
            token: 7,
            kind: PendingKind::Files,
        }));
        wait_for_selection_owner(&conn, selection);
        // A file manager can enable Paste at once.
        let notify = convert_and_wait(
            &conn,
            requestor,
            selection,
            targets,
            property,
            x11rb::CURRENT_TIME,
        );
        assert_eq!(notify.property, property);
        let offered: Vec<Atom> = conn
            .get_property(true, requestor, property, AtomEnum::ATOM, 0, 64)
            .unwrap()
            .reply()
            .unwrap()
            .value32()
            .unwrap()
            .collect();
        assert!(offered.contains(&gnome) && offered.contains(&uri_list));

        conn.convert_selection(requestor, selection, gnome, property, x11rb::CURRENT_TIME)
            .unwrap();
        conn.flush().unwrap();
        let wait = std::time::Duration::from_millis(200);
        assert!(
            selection_notify_within(&conn, requestor, gnome, wait).is_none(),
            "the paste waits while the data is still arriving"
        );
        // Data for an older announcement never answers it.
        assert!(setter.send(RemoteServe::FulfilFiles {
            token: 6,
            paths: vec!["/stale".into()],
            receipt: FileFulfilReceipt::default(),
        }));
        assert!(selection_notify_within(&conn, requestor, gnome, wait).is_none());
        let paths = vec![std::path::PathBuf::from("/home/u/.cache/jwm/x/report.pdf")];
        assert!(setter.send(RemoteServe::FulfilFiles {
            token: 7,
            paths,
            receipt: FileFulfilReceipt::default()
        }));
        let notify =
            selection_notify_within(&conn, requestor, gnome, std::time::Duration::from_secs(3))
                .expect("the waiting paste is answered");
        assert_eq!(notify.property, property);
        let reply = conn
            .get_property(true, requestor, property, AtomEnum::ANY, 0, 1024)
            .unwrap()
            .reply()
            .unwrap();
        assert_eq!(reply.type_, gnome);
        assert_eq!(reply.value, b"copy\nfile:///home/u/.cache/jwm/x/report.pdf");

        // Once fulfilled, later pastes are served at once.
        let notify = convert_and_wait(
            &conn,
            requestor,
            selection,
            uri_list,
            property,
            x11rb::CURRENT_TIME,
        );
        assert_eq!(notify.property, property);
    }

    #[cfg(all(feature = "backend-x11rb", feature = "remote-x11"))]
    #[test]
    fn an_abandoned_transfer_refuses_waiting_pastes_and_frees_the_clipboard() {
        let x11 = crate::backend::clipboard_offer::IsolatedXvfb::acquire();
        let display = x11.name();
        let (_captures, setter) =
            RemoteClipboard::start(Some(display), remote_test_options()).unwrap();
        let (conn, screen_num) = x11rb::connect(Some(display)).unwrap();
        let root = conn.setup().roots[screen_num].root;
        let selection = intern(&conn, "CLIPBOARD").unwrap();
        let png = intern(&conn, "image/png").unwrap();
        let property = intern(&conn, "JWM_TEST_ABANDON").unwrap();
        let requestor = input_only_window(&conn, root);

        assert!(setter.send(RemoteServe::Announce {
            token: 1,
            kind: PendingKind::Png,
        }));
        wait_for_selection_owner(&conn, selection);
        conn.convert_selection(requestor, selection, png, property, x11rb::CURRENT_TIME)
            .unwrap();
        conn.flush().unwrap();
        let wait = std::time::Duration::from_millis(150);
        assert!(selection_notify_within(&conn, requestor, png, wait).is_none());
        assert!(setter.send(RemoteServe::Abandon { token: 1 }));
        let notify =
            selection_notify_within(&conn, requestor, png, std::time::Duration::from_secs(3))
                .expect("the waiting paste is refused");
        assert_eq!(notify.property, x11rb::NONE);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while conn
            .get_selection_owner(selection)
            .unwrap()
            .reply()
            .unwrap()
            .owner
            != x11rb::NONE
        {
            assert!(
                std::time::Instant::now() < deadline,
                "clipboard still owned after abandon"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
    #[cfg(feature = "remote-x11")]
    #[test]
    #[ignore = "requires isolated X server in JWM_TEST_X11_DISPLAY; never use ambient DISPLAY"]
    fn remote_overload_refuses_waiting_pastes_and_closes_capture() {
        let isolated_display = std::env::var("JWM_TEST_X11_DISPLAY")
            .expect("set JWM_TEST_X11_DISPLAY to an isolated test X server");
        assert!(
            !isolated_display.is_empty(),
            "test X display must be explicit"
        );
        let display = isolated_display.as_str();
        let (captures, setter) =
            RemoteClipboard::start(Some(display), remote_test_options()).unwrap();
        let (conn, screen_num) = x11rb::connect(Some(display)).unwrap();
        let root = conn.setup().roots[screen_num].root;
        let selection = intern(&conn, "CLIPBOARD").unwrap();
        let png = intern(&conn, "image/png").unwrap();
        let property = intern(&conn, "JWM_TEST_ABANDON").unwrap();
        let requestor = input_only_window(&conn, root);

        assert!(setter.send(RemoteServe::Announce {
            token: 1,
            kind: PendingKind::Png,
        }));
        wait_for_selection_owner(&conn, selection);
        conn.convert_selection(requestor, selection, png, property, x11rb::CURRENT_TIME)
            .unwrap();
        conn.flush().unwrap();
        let wait = std::time::Duration::from_millis(150);
        assert!(selection_notify_within(&conn, requestor, png, wait).is_none());
        assert!(!setter.send(RemoteServe::Text(String::with_capacity(
            crate::backend::clipboard_offer::MAX_TEXT_BYTES + 1,
        ))));
        assert!(setter.is_failed());
        let notify =
            selection_notify_within(&conn, requestor, png, std::time::Duration::from_secs(3))
                .expect("the waiting paste is refused");
        assert_eq!(notify.property, x11rb::NONE);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while conn
            .get_selection_owner(selection)
            .unwrap()
            .reply()
            .unwrap()
            .owner
            != x11rb::NONE
        {
            assert!(
                std::time::Instant::now() < deadline,
                "clipboard still owned after overload"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(
            captures
                .recv_timeout(std::time::Duration::from_secs(3))
                .is_err()
        );
    }
}
