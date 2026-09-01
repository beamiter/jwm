//! CLIPBOARD monitoring and ownership.
//!
//! On X11 the clipboard is not storage but a protocol: the copying
//! application keeps the data and hands it over on request, and it disappears
//! when that application exits. A clipboard history therefore has to do two
//! separate jobs, and both live here:
//!
//! * **Watch** — XFIXES reports every change of CLIPBOARD ownership. On each
//!   change the current owner is asked for its target list, and only if that
//!   list is text and is not marked secret is the payload requested at all.
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
    ClipboardOffer, X11_DIRECT_PROPERTY_BYTES, X11_INCR_CHUNK_BYTES, X11_MAX_ACTIVE_INCR_BYTES,
    X11_MAX_MULTIPLE_CONVERSIONS, next_x11_incr_chunk_with_limit, x11_selection_time_is_valid,
};

/// A requester that never deletes the INCR property must not retain transfer
/// state forever, and many hostile requesters must not grow it without bound.
const MAX_OUTGOING_INCR_TRANSFERS: usize = 32;
const MAX_OUTGOING_INCR_PER_REQUESTOR: usize = 8;
const OUTGOING_INCR_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const INCOMING_CONVERSION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const MAX_INCOMING_CHUNK_BYTES: u32 = 1024 * 1024;
const MAX_TARGET_ATOMS: usize = 256;
const MAX_TARGET_LIST_BYTES: usize = MAX_TARGET_ATOMS * 4;

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
    utf8_string: Atom,
    text_plain_utf8: Atom,
    text_plain: Atom,
    image_png: Atom,
    incr: Atom,
    /// Property on our own window that conversions are delivered into.
    transfer: Atom,
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
}

/// The watcher, living on its own thread.
struct Watcher {
    conn: RustConnection,
    /// Unmapped `InputOnly` window that receives conversions and owns the
    /// selection when JWM serves an entry back.
    window: Window,
    atoms: Atoms,
    /// Data JWM currently offers as owner; `None` when another application
    /// owns the clipboard.
    offered: Option<OfferedData>,
    pending_offer: Option<ClipboardOffer>,
    pending_probe_events: usize,
    /// Server timestamp used for the most recent successful ownership change.
    ownership_time: Option<u32>,
    property_payload_bytes: usize,
    /// One generation-isolated request for data owned by another client.
    capture: Option<CaptureRequest>,
    outgoing_incr: std::collections::HashMap<(Window, Atom), OutgoingIncr>,
}

#[derive(Debug)]
struct CaptureRequest {
    owner: Window,
    window: Window,
    request_time: u32,
    conversion: Conversion,
    target: Atom,
    incoming_incr: Option<IncomingIncr>,
    last_activity: std::time::Instant,
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
}

impl From<ClipboardOffer> for OfferedData {
    fn from(offer: ClipboardOffer) -> Self {
        match offer {
            ClipboardOffer::Text(text) => Self::Text(text.into_bytes().into()),
            ClipboardOffer::Png(png) => Self::Png(png.into()),
        }
    }
}

impl OfferedData {
    fn targets(&self, atoms: &Atoms) -> Vec<Atom> {
        match self {
            Self::Text(_) => vec![
                atoms.targets,
                atoms.multiple,
                atoms.timestamp,
                atoms.utf8_string,
                atoms.text_plain_utf8,
                atoms.text_plain,
            ],
            Self::Png(_) => vec![
                atoms.targets,
                atoms.multiple,
                atoms.timestamp,
                atoms.image_png,
            ],
        }
    }

    fn payload_for(&self, atoms: &Atoms, target: Atom) -> Option<std::sync::Arc<[u8]>> {
        match self {
            Self::Text(bytes)
                if target == atoms.utf8_string
                    || target == atoms.text_plain_utf8
                    || target == atoms.text_plain =>
            {
                Some(std::sync::Arc::clone(bytes))
            }
            Self::Png(bytes) if target == atoms.image_png => Some(std::sync::Arc::clone(bytes)),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct OutgoingIncr {
    target: Atom,
    data: std::sync::Arc<[u8]>,
    offset: usize,
    last_activity: std::time::Instant,
}

/// Handle held by the backend: captured text arrives on `captured`, entries
/// to serve are sent on `serve`.
pub(crate) struct Clipboard {
    captured: std::sync::mpsc::Receiver<String>,
    serve: std::sync::mpsc::Sender<ClipboardOffer>,
    #[cfg(feature = "backend-x11rb")]
    notifier: std::sync::Arc<
        std::sync::Mutex<Option<crate::backend::update_notifier::AsyncUpdateNotifier>>,
    >,
}

impl Clipboard {
    /// Start watching CLIPBOARD on a dedicated connection and thread.
    ///
    /// `display` selects the X server, so the remote helper can watch the
    /// display it is actually sharing or presenting rather than whatever
    /// `$DISPLAY` happens to say.
    pub(crate) fn start(display: Option<&str>) -> Result<Self, String> {
        let display = display.map(str::to_string);
        let (captured_tx, captured) = std::sync::mpsc::channel();
        let (serve, serve_rx) = std::sync::mpsc::channel();
        let notifier = std::sync::Arc::new(std::sync::Mutex::new(None));
        let worker_notifier = std::sync::Arc::clone(&notifier);
        // Build the watcher on the thread that will own it: RustConnection is
        // not Sync, and nothing outside the thread may touch it.
        let (ready_tx, ready) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("jwm-clipboard".to_string())
            .spawn(move || match Watcher::new(display.as_deref()) {
                Ok(mut watcher) => {
                    let _ = ready_tx.send(Ok(()));
                    watcher.run(&captured_tx, &serve_rx, &worker_notifier);
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                }
            })
            .map_err(|error| format!("spawn clipboard thread: {error}"))?;

        ready
            .recv()
            .map_err(|_| "clipboard thread died during startup".to_string())??;
        Ok(Self {
            captured,
            serve,
            #[cfg(feature = "backend-x11rb")]
            notifier,
        })
    }

    /// Text copied since the last call, oldest first.
    #[cfg(feature = "backend-x11rb")]
    pub(crate) fn drain_captured(&self) -> Vec<String> {
        self.captured.try_iter().collect()
    }

    /// Offer `text` to other applications.
    #[cfg(feature = "backend-x11rb")]
    pub(crate) fn set_text(&self, text: &str) -> bool {
        if text.len() > crate::backend::clipboard_offer::MAX_TEXT_BYTES {
            return false;
        }
        self.serve
            .send(ClipboardOffer::Text(text.to_string()))
            .is_ok()
    }

    /// Route an encoded PNG to this backend's native selection owner.
    #[cfg(feature = "backend-x11rb")]
    pub(crate) fn image_sender(&self) -> ClipboardImageSender {
        ClipboardImageSender::new(self.serve.clone())
    }

    /// Attach captures to the owning handler after that handler has created
    /// its aggregate readiness fd. The watcher connection starts earlier than
    /// JWM, so attaching itself emits one conservative wake to cover a capture
    /// that may already be queued.
    #[cfg(feature = "backend-x11rb")]
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

    /// Split into halves that can live on different threads.
    ///
    /// The remote helper watches captures on one thread and serves incoming
    /// text from another, and neither may block the other.
    #[cfg(feature = "remote-x11")]
    pub(crate) fn split(self) -> (ClipboardCaptures, ClipboardSetter) {
        (
            ClipboardCaptures(self.captured),
            ClipboardSetter(self.serve),
        )
    }
}

/// Receiving half: text copied on this display.
#[cfg(feature = "remote-x11")]
pub(crate) struct ClipboardCaptures(std::sync::mpsc::Receiver<String>);

#[cfg(feature = "remote-x11")]
impl ClipboardCaptures {
    /// Block for the next captured text, giving up after `timeout`.
    ///
    /// Returning on a timeout rather than parking forever lets the caller
    /// notice session shutdown without a second wake channel.
    pub(crate) fn recv_timeout(&self, timeout: std::time::Duration) -> Option<String> {
        self.0.recv_timeout(timeout).ok()
    }
}

/// Sending half: text to offer to other applications on this display.
#[cfg(feature = "remote-x11")]
#[derive(Clone)]
pub(crate) struct ClipboardSetter(std::sync::mpsc::Sender<ClipboardOffer>);

#[cfg(feature = "remote-x11")]
impl ClipboardSetter {
    pub(crate) fn set_text(&self, text: &str) -> bool {
        if text.len() > crate::backend::clipboard_offer::MAX_TEXT_BYTES {
            return false;
        }
        self.0.send(ClipboardOffer::Text(text.to_string())).is_ok()
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
    /// Open a private connection, create the owner window, and start
    /// watching CLIPBOARD.
    fn new(display: Option<&str>) -> Result<Self, String> {
        let (conn, screen_num) =
            x11rb::connect(display).map_err(|error| format!("clipboard connect: {error}"))?;
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
            utf8_string: intern(&conn, "UTF8_STRING")?,
            text_plain_utf8: intern(&conn, "text/plain;charset=utf-8")?,
            text_plain: intern(&conn, "text/plain")?,
            image_png: intern(&conn, "image/png")?,
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
            offered: None,
            pending_offer: None,
            pending_probe_events: 0,
            ownership_time: None,
            property_payload_bytes,
            capture: None,
            outgoing_incr: std::collections::HashMap::new(),
        };
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
        Ok(watcher)
    }

    /// Event loop for the clipboard thread.
    ///
    /// Polls rather than blocking on `wait_for_event` so entries to serve are
    /// picked up promptly; the interval is irrelevant to frame pacing because
    /// this is not the compositor's thread.
    fn run(
        &mut self,
        captured: &std::sync::mpsc::Sender<String>,
        serve: &std::sync::mpsc::Receiver<ClipboardOffer>,
        notifier: &std::sync::Mutex<Option<crate::backend::update_notifier::AsyncUpdateNotifier>>,
    ) {
        const IDLE: std::time::Duration = std::time::Duration::from_millis(20);
        loop {
            match serve.try_recv() {
                Ok(mut offer) => {
                    // Only the newest clipboard value can be current. Drop
                    // superseded queued PNG allocations before taking X11
                    // ownership instead of replaying every intermediate copy.
                    while let Ok(newer) = serve.try_recv() {
                        offer = newer;
                    }
                    self.take_ownership(offer);
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }

            self.expire_outgoing_incr();

            let mut idle = true;
            for _ in 0..256 {
                match self.conn.poll_for_event() {
                    Ok(Some(event)) => {
                        idle = false;
                        if let Some(text) = self.handle(&event)
                            && !publish_capture(captured, notifier, text)
                        {
                            return;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        log::warn!("clipboard: X11 connection failed: {error}");
                        return;
                    }
                }
            }
            if idle {
                // During INCR, every chunk is acknowledged by a fresh
                // PropertyDelete round trip. Poll promptly then; retain the
                // relaxed cadence while the clipboard is otherwise idle.
                let recently_active = self.outgoing_incr.values().any(|transfer| {
                    transfer.last_activity.elapsed() < std::time::Duration::from_millis(100)
                });
                std::thread::sleep(if recently_active {
                    std::time::Duration::from_millis(2)
                } else {
                    IDLE
                });
            }
        }
    }

    /// Route one X event. Returns copied text when a conversion completed.
    fn handle(&mut self, event: &x11rb::protocol::Event) -> Option<String> {
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
                if self.pending_offer.is_none() && !self.is_current_owner(self.window) {
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
            incoming_incr: None,
            last_activity: std::time::Instant::now(),
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

    /// A conversion finished. Returns the copied text once it has been asked
    /// for and delivered.
    fn on_selection_notify(&mut self, event: &SelectionNotifyEvent) -> Option<String> {
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
        let cap = match conversion {
            Conversion::TargetList => MAX_TARGET_LIST_BYTES,
            Conversion::Text => crate::backend::clipboard_offer::MAX_TEXT_BYTES,
        };
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
            let valid_announcement = reply.format == 32 && reply.value_len == 1;
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
                self.request_text_if_allowed(&targets);
                None
            }
            Conversion::Text => {
                if reply.type_ != event.target || reply.format != 8 {
                    self.cancel_capture();
                    return None;
                }
                self.cancel_capture();
                String::from_utf8(reply.value).ok()
            }
        }
    }

    /// Decide from the target list whether to ask for the payload at all.
    ///
    /// The names are resolved so the shared policy in
    /// `jwm::features::clipboard` makes the call — one round trip on a copy,
    /// and worth it to keep one definition of "this is a secret".
    fn request_text_if_allowed(&mut self, targets: &[Atom]) {
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
        // Ask for the richest text form the owner actually advertises.
        let Some(target) = [
            self.atoms.text_plain_utf8,
            self.atoms.utf8_string,
            self.atoms.text_plain,
        ]
        .into_iter()
        .find(|wanted| targets.contains(wanted)) else {
            self.cancel_capture();
            return;
        };

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
            capture.conversion = Conversion::Text;
            capture.target = target;
            capture.incoming_incr = None;
            capture.last_activity = std::time::Instant::now();
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
            log::debug!("clipboard: requesting text failed: {error}");
            self.cancel_capture();
            return;
        }
        let _ = self.conn.flush();
    }

    /// Offer one payload to other applications by taking CLIPBOARD ownership.
    fn take_ownership(&mut self, offer: ClipboardOffer) {
        self.cancel_capture();
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
        let Some(offer) = self.pending_offer.take() else {
            return;
        };
        self.offered = Some(offer.into());
        let set = self
            .conn
            .set_selection_owner(self.window, self.atoms.clipboard, timestamp)
            .ok()
            .and_then(|cookie| cookie.check().ok())
            .is_some();
        if !set || !self.is_current_owner(self.window) {
            self.offered = None;
            self.ownership_time = None;
            return;
        }
        self.ownership_time = Some(timestamp);
        let _ = self.conn.flush();
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
        let served = if !request_valid {
            false
        } else if event.target == self.atoms.multiple {
            event.property != x11rb::NONE && self.serve_multiple(event.requestor, event.property)
        } else {
            self.serve_target(event.requestor, property, event.target)
        };

        let notify = SelectionNotifyEvent {
            response_type: x11rb::protocol::xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: event.time,
            requestor: event.requestor,
            selection: event.selection,
            target: event.target,
            property: if served { property } else { x11rb::NONE },
        };
        let _ = self
            .conn
            .send_event(false, event.requestor, EventMask::NO_EVENT, notify);
        let _ = self.conn.flush();
    }

    fn serve_target(&mut self, requestor: Window, property: Atom, target: Atom) -> bool {
        let Some(offered) = self.offered.clone() else {
            return false;
        };
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
    fn on_property_notify(&mut self, event: &PropertyNotifyEvent) -> Option<String> {
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

    fn on_incoming_incr_property(&mut self, window: Window, property: Atom) -> Option<String> {
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
            Conversion::Text => 8,
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
        let cap = match conversion {
            Conversion::TargetList => MAX_TARGET_LIST_BYTES,
            Conversion::Text => crate::backend::clipboard_offer::MAX_TEXT_BYTES,
        };
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
    ) -> Option<String> {
        match conversion {
            Conversion::TargetList => {
                let _ = bytes;
                self.cancel_capture();
                None
            }
            Conversion::Text => {
                self.cancel_capture();
                String::from_utf8(bytes).ok()
            }
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
        if self.capture.as_ref().is_some_and(|capture| {
            now.saturating_duration_since(capture.last_activity) >= INCOMING_CONVERSION_TIMEOUT
        }) {
            self.cancel_capture();
        }
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
    captured: &std::sync::mpsc::Sender<String>,
    notifier: &std::sync::Mutex<Option<crate::backend::update_notifier::AsyncUpdateNotifier>>,
    text: String,
) -> bool {
    // Channel publication happens first. Once the eventfd is readable the
    // handler must be able to drain this text without another timer tick.
    if captured.send(text).is_err() {
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
    use super::*;

    #[cfg(feature = "backend-x11rb")]
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
    }

    #[test]
    fn capture_is_queued_before_its_completion_wake() {
        let notifier = crate::backend::update_notifier::AsyncUpdateNotifier::new().unwrap();
        let slot = std::sync::Mutex::new(Some(notifier.clone()));
        let (send, receive) = std::sync::mpsc::channel();

        assert!(publish_capture(&send, &slot, "ready".to_string()));
        assert_eq!(notifier.drain().unwrap(), 1);
        assert_eq!(receive.try_recv().unwrap(), "ready");
    }

    #[cfg(feature = "backend-x11rb")]
    #[test]
    #[ignore = "requires an isolated X11 server in DISPLAY"]
    fn native_png_offer_serves_payload_beyond_xclips_one_mib_cliff() {
        let _serial = crate::backend::clipboard_offer::X11_CLIPBOARD_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let clipboard_owner = Clipboard::start(None).unwrap();
        let (conn, screen_num) = x11rb::connect(None).unwrap();
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
    #[ignore = "requires an isolated X11 server in DISPLAY"]
    fn native_owner_metadata_multiple_and_direct_round_trip() {
        let _serial = crate::backend::clipboard_offer::X11_CLIPBOARD_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let clipboard_owner = Clipboard::start(None).unwrap();
        let (conn, screen_num) = x11rb::connect(None).unwrap();
        let root = conn.setup().roots[screen_num].root;
        let clipboard = intern(&conn, "CLIPBOARD").unwrap();
        let targets = intern(&conn, "TARGETS").unwrap();
        let multiple = intern(&conn, "MULTIPLE").unwrap();
        let timestamp = intern(&conn, "TIMESTAMP").unwrap();
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
        assert!(clipboard_owner.set_text("hello π"));
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
        for required in [targets, multiple, timestamp, utf8] {
            assert!(advertised.contains(&required));
        }

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
    #[ignore = "requires an isolated X11 server in DISPLAY"]
    fn native_watcher_collects_and_drains_incoming_incr() {
        let _serial = crate::backend::clipboard_offer::X11_CLIPBOARD_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let clipboard_watcher = Clipboard::start(None).unwrap();
        let (conn, screen_num) = x11rb::connect(None).unwrap();
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
                assert_eq!(captured, vec![String::from_utf8(expected).unwrap()]);
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
}
