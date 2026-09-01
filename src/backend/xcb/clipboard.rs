//! CLIPBOARD monitoring and ownership for the XCB transport.
//!
//! The X11 mechanics and the reasoning behind them are the same as the x11rb
//! watcher in [`crate::backend::x11rb::clipboard`] — watch ownership through
//! XFIXES, ask each new owner for its target list, request the payload only
//! when it is text and unmarked, and become the owner to serve an entry back.
//! Only the transport differs.
//!
//! Like that one it runs on **its own connection and thread**: selection
//! traffic waits on other clients, and the window manager's loop must never
//! be the thing waiting.

use crate::backend::clipboard_offer::{
    self as clipboard, ClipboardImageSender, ClipboardOffer, X11_DIRECT_PROPERTY_BYTES,
    X11_INCR_CHUNK_BYTES, X11_MAX_ACTIVE_INCR_BYTES, X11_MAX_MULTIPLE_CONVERSIONS,
    next_x11_incr_chunk_with_limit, x11_selection_time_is_valid,
};
use xcb::x::{self, ATOM_ANY, ATOM_ATOM, ATOM_INTEGER, Atom};
use xcb::{Connection, Xid};

/// How long the thread sleeps when the connection had nothing to report.
const IDLE: std::time::Duration = std::time::Duration::from_millis(20);

const MAX_OUTGOING_INCR_TRANSFERS: usize = 32;
const MAX_OUTGOING_INCR_PER_REQUESTOR: usize = 8;
const OUTGOING_INCR_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const INCOMING_CONVERSION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const MAX_INCOMING_CHUNK_BYTES: u32 = 1024 * 1024;
const MAX_TARGET_ATOMS: usize = 256;
const MAX_TARGET_LIST_BYTES: usize = MAX_TARGET_ATOMS * 4;

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
    transfer: Atom,
}

/// What a finished conversion carries. Routing uses the reply's own target,
/// never a remembered request: ownership changes mid-conversion and a late
/// reply from the previous owner would otherwise be read as a target list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Conversion {
    TargetList,
    Text,
}

/// Handle held by the backend.
pub(crate) struct Clipboard {
    captured: std::sync::mpsc::Receiver<String>,
    serve: std::sync::mpsc::Sender<ClipboardOffer>,
    notifier: std::sync::Arc<
        std::sync::Mutex<Option<crate::backend::update_notifier::AsyncUpdateNotifier>>,
    >,
}

impl Clipboard {
    /// Start watching CLIPBOARD on a dedicated connection and thread.
    pub(crate) fn start() -> Result<Self, String> {
        let (captured_tx, captured) = std::sync::mpsc::channel();
        let (serve, serve_rx) = std::sync::mpsc::channel();
        let notifier = std::sync::Arc::new(std::sync::Mutex::new(None));
        let worker_notifier = std::sync::Arc::clone(&notifier);
        let (ready_tx, ready) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("jwm-clipboard".to_string())
            .spawn(move || match Watcher::new() {
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
            notifier,
        })
    }

    /// Text copied since the last call, oldest first.
    pub(crate) fn drain_captured(&self) -> Vec<String> {
        self.captured.try_iter().collect()
    }

    /// Offer `text` to other applications.
    pub(crate) fn set_text(&self, text: &str) -> bool {
        if text.len() > clipboard::MAX_TEXT_BYTES {
            return false;
        }
        self.serve
            .send(ClipboardOffer::Text(text.to_string()))
            .is_ok()
    }

    pub(crate) fn image_sender(&self) -> ClipboardImageSender {
        ClipboardImageSender::new(self.serve.clone())
    }

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
            // Also covers a capture queued between backend construction and
            // event-loop registration.
            notifier.notify();
        }
    }
}

struct Watcher {
    conn: Connection,
    window: x::Window,
    atoms: Atoms,
    offered: Option<OfferedData>,
    pending_offer: Option<ClipboardOffer>,
    pending_probe_events: usize,
    ownership_time: Option<u32>,
    property_payload_bytes: usize,
    capture: Option<CaptureRequest>,
    outgoing_incr: std::collections::HashMap<(x::Window, Atom), OutgoingIncr>,
}

#[derive(Debug)]
struct CaptureRequest {
    owner: x::Window,
    window: x::Window,
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

fn intern(conn: &Connection, name: &str) -> Result<Atom, String> {
    let cookie = conn.send_request(&x::InternAtom {
        only_if_exists: false,
        name: name.as_bytes(),
    });
    conn.wait_for_reply(cookie)
        .map(|reply| reply.atom())
        .map_err(|error| format!("intern {name}: {error}"))
}

impl Watcher {
    fn new() -> Result<Self, String> {
        // XFIXES has to be named at connect time: the crate resolves an
        // extension's event codes then, and an unlisted extension's events
        // arrive as unrecognized rather than as SelectionNotify.
        let (conn, screen_num) =
            Connection::connect_with_extensions(None, &[], &[xcb::Extension::XFixes])
                .map_err(|error| format!("clipboard connect: {error}"))?;
        let root = conn
            .get_setup()
            .roots()
            .nth(screen_num as usize)
            .ok_or_else(|| "clipboard: no screen".to_string())?
            .root();

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

        let window: x::Window = conn.generate_id();
        conn.send_and_check_request(&x::CreateWindow {
            depth: 0,
            wid: window,
            parent: root,
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            border_width: 0,
            class: x::WindowClass::InputOnly,
            visual: x::COPY_FROM_PARENT,
            value_list: &[x::Cw::EventMask(x::EventMask::PROPERTY_CHANGE)],
        })
        .map_err(|error| format!("create clipboard window: {error}"))?;

        // XFIXES only honors its requests after a version handshake.
        let version = conn.send_request(&xcb::xfixes::QueryVersion {
            client_major_version: 5,
            client_minor_version: 0,
        });
        conn.wait_for_reply(version)
            .map_err(|error| format!("xfixes version: {error}"))?;

        conn.send_and_check_request(&xcb::xfixes::SelectSelectionInput {
            window,
            selection: atoms.clipboard,
            event_mask: xcb::xfixes::SelectionEventMask::SET_SELECTION_OWNER
                | xcb::xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
                | xcb::xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE,
        })
        .map_err(|error| format!("xfixes_select_selection_input: {error}"))?;

        log::info!(
            "clipboard: watching CLIPBOARD (owner window 0x{:x})",
            window.resource_id()
        );
        let property_payload_bytes = usize::try_from(conn.get_maximum_request_length())
            .unwrap_or(usize::MAX)
            .saturating_mul(4)
            .saturating_sub(24)
            .max(4);
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
        let cookie = watcher.conn.send_request(&x::GetSelectionOwner {
            selection: watcher.atoms.clipboard,
        });
        let existing_owner = watcher
            .conn
            .wait_for_reply(cookie)
            .map(|reply| reply.owner())
            .unwrap_or(x::WINDOW_NONE);
        if existing_owner != x::WINDOW_NONE {
            watcher.begin_capture(existing_owner, x::CURRENT_TIME);
        }
        Ok(watcher)
    }

    fn run(
        &mut self,
        captured: &std::sync::mpsc::Sender<String>,
        serve: &std::sync::mpsc::Receiver<ClipboardOffer>,
        notifier: &std::sync::Mutex<Option<crate::backend::update_notifier::AsyncUpdateNotifier>>,
    ) {
        loop {
            match serve.try_recv() {
                Ok(mut offer) => {
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
                        log::warn!("clipboard: XCB connection failed: {error}");
                        return;
                    }
                }
            }
            if idle {
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

    fn handle(&mut self, event: &xcb::Event) -> Option<String> {
        match event {
            xcb::Event::XFixes(xcb::xfixes::Event::SelectionNotify(e)) => {
                self.on_owner_changed(e.owner(), e.timestamp(), e.selection_timestamp());
                None
            }
            xcb::Event::X(x::Event::SelectionNotify(e)) => self.on_selection_notify(e),
            xcb::Event::X(x::Event::SelectionRequest(e)) => {
                self.on_selection_request(e);
                None
            }
            xcb::Event::X(x::Event::PropertyNotify(e)) => self.on_property_notify(e),
            xcb::Event::X(x::Event::SelectionClear(e)) if e.owner() == self.window => {
                if self.pending_offer.is_none() && !self.is_current_owner(self.window) {
                    self.offered = None;
                    self.pending_offer = None;
                    self.pending_probe_events = 0;
                    self.ownership_time = None;
                }
                None
            }
            xcb::Event::X(x::Event::DestroyNotify(e)) => {
                self.outgoing_incr
                    .retain(|(window, _), _| *window != e.window());
                if self
                    .capture
                    .as_ref()
                    .is_some_and(|capture| capture.window == e.window())
                {
                    self.capture = None;
                }
                None
            }
            _ => None,
        }
    }

    /// A new application took the clipboard: ask what it can offer. Ownership
    /// JWM took itself is ignored, so serving an entry does not re-record it.
    fn on_owner_changed(&mut self, owner: x::Window, timestamp: u32, selection_timestamp: u32) {
        if owner == self.window {
            self.cancel_capture();
            self.ownership_time = Some(selection_timestamp);
            return;
        }
        if self.pending_offer.is_some() {
            return;
        }
        if owner == x::WINDOW_NONE {
            self.cancel_capture();
            if !self.is_current_owner(self.window) {
                self.offered = None;
                self.pending_offer = None;
                self.pending_probe_events = 0;
                self.ownership_time = None;
            }
            return;
        }
        if self.is_current_owner(self.window) {
            return;
        }
        self.offered = None;
        self.pending_offer = None;
        self.pending_probe_events = 0;
        self.ownership_time = None;
        self.begin_capture(owner, timestamp);
    }

    fn is_current_owner(&self, owner: x::Window) -> bool {
        let cookie = self.conn.send_request(&x::GetSelectionOwner {
            selection: self.atoms.clipboard,
        });
        self.conn
            .wait_for_reply(cookie)
            .is_ok_and(|reply| reply.owner() == owner)
    }

    fn begin_capture(&mut self, owner: x::Window, request_time: u32) {
        self.cancel_capture();
        let window: x::Window = self.conn.generate_id();
        if self
            .conn
            .send_and_check_request(&x::CreateWindow {
                depth: 0,
                wid: window,
                parent: self.window,
                x: 0,
                y: 0,
                width: 1,
                height: 1,
                border_width: 0,
                class: x::WindowClass::InputOnly,
                visual: x::COPY_FROM_PARENT,
                value_list: &[x::Cw::EventMask(x::EventMask::PROPERTY_CHANGE)],
            })
            .is_err()
        {
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
        self.conn.send_request(&x::ConvertSelection {
            requestor: window,
            selection: self.atoms.clipboard,
            target: self.atoms.targets,
            property: self.atoms.transfer,
            time: request_time,
        });
        let _ = self.conn.flush();
    }

    fn cancel_capture(&mut self) {
        let Some(capture) = self.capture.take() else {
            return;
        };
        self.conn.send_request(&x::DestroyWindow {
            window: capture.window,
        });
        let _ = self.conn.flush();
    }

    fn convert_capture_to_text(&mut self, target: Atom) -> bool {
        let Some(capture) = self.capture.as_ref() else {
            return false;
        };
        let owner = capture.owner;
        let window = capture.window;
        let request_time = capture.request_time;
        if !self.is_current_owner(owner) {
            self.cancel_capture();
            return false;
        }
        if let Some(capture) = self.capture.as_mut() {
            capture.conversion = Conversion::Text;
            capture.target = target;
            capture.incoming_incr = None;
            capture.last_activity = std::time::Instant::now();
        }
        self.conn.send_request(&x::ConvertSelection {
            requestor: window,
            selection: self.atoms.clipboard,
            target,
            property: self.atoms.transfer,
            time: request_time,
        });
        self.conn.flush().is_ok()
    }

    fn on_selection_notify(&mut self, event: &x::SelectionNotifyEvent) -> Option<String> {
        let Some(capture) = self.capture.as_ref() else {
            return None;
        };
        if event.requestor() != capture.window
            || event.selection() != self.atoms.clipboard
            || event.target() != capture.target
        {
            return None;
        }
        if event.property() == x::ATOM_NONE {
            self.cancel_capture();
            return None;
        }
        if event.property() != self.atoms.transfer {
            return None;
        }
        if !self.is_current_owner(capture.owner) {
            self.cancel_capture();
            return None;
        }

        let conversion = capture.conversion;
        let cap = match conversion {
            Conversion::TargetList => MAX_TARGET_LIST_BYTES,
            Conversion::Text => clipboard::MAX_TEXT_BYTES,
        };

        let cookie = self.conn.send_request(&x::GetProperty {
            delete: true,
            window: event.requestor(),
            property: event.property(),
            r#type: ATOM_ANY,
            long_offset: 0,
            long_length: (cap / 4 + 1) as u32,
        });
        let reply = self.conn.wait_for_reply(cookie).ok()?;

        if reply.r#type() == self.atoms.incr {
            let announced = (reply.format() == 32)
                .then(|| reply.value::<u32>())
                .and_then(|values| (values.len() == 1).then_some(values[0]));
            if let Some(capture) = self.capture.as_mut() {
                capture.incoming_incr = Some(IncomingIncr {
                    bytes: Vec::with_capacity(
                        announced
                            .and_then(|bytes| usize::try_from(bytes).ok())
                            .unwrap_or_default()
                            .min(cap),
                    ),
                    oversized: conversion == Conversion::TargetList
                        || announced.is_none()
                        || announced.is_some_and(|bytes| bytes as usize > cap),
                });
                capture.last_activity = std::time::Instant::now();
            }
            return None;
        }
        if reply.bytes_after() != 0 {
            self.conn.send_request(&x::DeleteProperty {
                window: event.requestor(),
                property: event.property(),
            });
            self.cancel_capture();
            return None;
        }

        match conversion {
            Conversion::TargetList => {
                if reply.r#type() != ATOM_ATOM || reply.format() != 32 {
                    self.cancel_capture();
                    return None;
                }
                let targets: Vec<Atom> = reply.value::<Atom>().to_vec();
                self.request_text_if_allowed(&targets);
                None
            }
            Conversion::Text => {
                if reply.r#type() != event.target() || reply.format() != 8 {
                    self.cancel_capture();
                    return None;
                }
                let text = String::from_utf8(reply.value::<u8>().to_vec()).ok();
                self.cancel_capture();
                text
            }
        }
    }

    /// Decide from the target list whether to ask for the payload at all.
    /// Names are resolved so the shared policy makes the call.
    fn request_text_if_allowed(&mut self, targets: &[Atom]) {
        let mut unique = std::collections::HashSet::with_capacity(targets.len());
        let targets: Vec<Atom> = targets
            .iter()
            .copied()
            .filter(|atom| unique.insert(*atom))
            .collect();
        if targets.len() > MAX_TARGET_ATOMS {
            self.cancel_capture();
            return;
        }
        let cookies: Vec<_> = targets
            .iter()
            .map(|atom| self.conn.send_request(&x::GetAtomName { atom: *atom }))
            .collect();
        let mut names = Vec::with_capacity(cookies.len());
        for cookie in cookies {
            let Ok(reply) = self.conn.wait_for_reply(cookie) else {
                self.cancel_capture();
                return;
            };
            names.push(reply.name().to_string());
        }

        if clipboard::is_secret(&names) {
            log::debug!("clipboard: offer marked secret, not reading it");
            self.cancel_capture();
            return;
        }
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
        if !self.convert_capture_to_text(target) {
            self.cancel_capture();
        }
    }

    /// Offer one payload by taking ownership of CLIPBOARD.
    fn take_ownership(&mut self, offer: ClipboardOffer) {
        self.cancel_capture();
        self.offered = None;
        self.ownership_time = None;
        self.pending_offer = Some(offer);
        self.pending_probe_events = self.pending_probe_events.saturating_add(1);
        if self
            .conn
            .send_and_check_request(&x::ChangeProperty {
                mode: x::PropMode::Append,
                window: self.window,
                property: self.atoms.timestamp_probe,
                r#type: ATOM_INTEGER,
                data: &[] as &[u8],
            })
            .is_err()
        {
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
        if self
            .conn
            .send_and_check_request(&x::SetSelectionOwner {
                owner: self.window,
                selection: self.atoms.clipboard,
                time: timestamp,
            })
            .is_err()
            || !self.is_current_owner(self.window)
        {
            self.offered = None;
            self.ownership_time = None;
            return;
        }
        self.ownership_time = Some(timestamp);
        let _ = self.conn.flush();
    }

    /// Answer a request for the entry JWM is offering.
    fn on_selection_request(&mut self, event: &x::SelectionRequestEvent) {
        // A pre-ICCCM requestor sends property=None, meaning "use the target".
        let property = if event.property() == x::ATOM_NONE {
            event.target()
        } else {
            event.property()
        };

        let request_valid = event.owner() == self.window
            && event.selection() == self.atoms.clipboard
            && event.requestor() != self.window
            && self
                .capture
                .as_ref()
                .is_none_or(|capture| event.requestor() != capture.window)
            && self.offered.is_some()
            && x11_selection_time_is_valid(event.time(), self.ownership_time);
        let served = if !request_valid {
            false
        } else if event.target() == self.atoms.multiple {
            event.property() != x::ATOM_NONE
                && self.serve_multiple(event.requestor(), event.property())
        } else {
            self.serve_target(event.requestor(), property, event.target())
        };

        self.reply(event, if served { property } else { x::ATOM_NONE });
    }

    fn serve_target(&mut self, requestor: x::Window, property: Atom, target: Atom) -> bool {
        let Some(offered) = self.offered.clone() else {
            return false;
        };
        if target == self.atoms.targets {
            let targets = offered.targets(&self.atoms);
            return self
                .conn
                .send_and_check_request(&x::ChangeProperty {
                    mode: x::PropMode::Replace,
                    window: requestor,
                    property,
                    r#type: ATOM_ATOM,
                    data: &targets,
                })
                .is_ok();
        }
        if target == self.atoms.timestamp {
            let Some(timestamp) = self.ownership_time else {
                return false;
            };
            return self
                .conn
                .send_and_check_request(&x::ChangeProperty {
                    mode: x::PropMode::Replace,
                    window: requestor,
                    property,
                    r#type: ATOM_INTEGER,
                    data: &[timestamp],
                })
                .is_ok();
        }

        offered
            .payload_for(&self.atoms, target)
            .is_some_and(|data| {
                if data.len() <= X11_DIRECT_PROPERTY_BYTES.min(self.property_payload_bytes) {
                    self.conn
                        .send_and_check_request(&x::ChangeProperty {
                            mode: x::PropMode::Replace,
                            window: requestor,
                            property,
                            r#type: target,
                            data: data.as_ref(),
                        })
                        .is_ok()
                } else {
                    self.begin_outgoing_incr(requestor, property, target, data)
                }
            })
    }

    fn serve_multiple(&mut self, requestor: x::Window, property: Atom) -> bool {
        let cookie = self.conn.send_request(&x::GetProperty {
            delete: false,
            window: requestor,
            property,
            r#type: self.atoms.atom_pair,
            long_offset: 0,
            long_length: (X11_MAX_MULTIPLE_CONVERSIONS * 2) as u32,
        });
        let Ok(reply) = self.conn.wait_for_reply(cookie) else {
            return false;
        };
        if reply.r#type() != self.atoms.atom_pair
            || reply.format() != 32
            || reply.bytes_after() != 0
        {
            return false;
        }
        let mut pairs: Vec<Atom> = reply.value::<Atom>().to_vec();
        if pairs.is_empty() || !pairs.len().is_multiple_of(2) {
            return false;
        }
        for pair in pairs.chunks_exact_mut(2) {
            let target = pair[0];
            let destination = pair[1];
            if destination == x::ATOM_NONE
                || destination == property
                || target == self.atoms.multiple
                || !self.serve_target(requestor, destination, target)
            {
                pair[0] = x::ATOM_NONE;
            }
        }
        self.conn
            .send_and_check_request(&x::ChangeProperty {
                mode: x::PropMode::Replace,
                window: requestor,
                property,
                r#type: self.atoms.atom_pair,
                data: &pairs,
            })
            .is_ok()
    }

    fn begin_outgoing_incr(
        &mut self,
        requestor: x::Window,
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

        if self
            .conn
            .send_and_check_request(&x::ChangeWindowAttributes {
                window: requestor,
                value_list: &[x::Cw::EventMask(
                    x::EventMask::PROPERTY_CHANGE | x::EventMask::STRUCTURE_NOTIFY,
                )],
            })
            .is_err()
            || self
                .conn
                .send_and_check_request(&x::ChangeProperty {
                    mode: x::PropMode::Replace,
                    window: requestor,
                    property,
                    r#type: self.atoms.incr,
                    data: &[total],
                })
                .is_err()
        {
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

    fn on_property_notify(&mut self, event: &x::PropertyNotifyEvent) -> Option<String> {
        if event.state() == x::Property::NewValue
            && event.window() == self.window
            && event.atom() == self.atoms.timestamp_probe
        {
            self.conn.send_request(&x::DeleteProperty {
                window: self.window,
                property: self.atoms.timestamp_probe,
            });
            self.finish_pending_ownership(event.time());
            return None;
        }
        if event.state() == x::Property::Delete {
            let key = (event.window(), event.atom());
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
                .send_and_check_request(&x::ChangeProperty {
                    mode: x::PropMode::Replace,
                    window: event.window(),
                    property: event.atom(),
                    r#type: target,
                    data: &data[range],
                })
                .is_ok();
            if terminal || !sent {
                self.outgoing_incr.remove(&key);
                self.stop_watching_requestor_if_idle(event.window());
            }
            let _ = self.conn.flush();
            return None;
        }
        if event.state() == x::Property::NewValue {
            return self.on_incoming_incr_property(event.window(), event.atom());
        }
        None
    }

    fn on_incoming_incr_property(&mut self, window: x::Window, property: Atom) -> Option<String> {
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

        let cookie = self.conn.send_request(&x::GetProperty {
            delete: false,
            window,
            property,
            r#type: ATOM_ANY,
            long_offset: 0,
            long_length: 0,
        });
        let peek = self.conn.wait_for_reply(cookie).ok()?;
        let expected_format = match conversion {
            Conversion::TargetList => 32,
            Conversion::Text => 8,
        };
        let valid_type = peek.r#type() == target && peek.format() == expected_format;

        if peek.bytes_after() == 0 {
            self.conn
                .send_request(&x::DeleteProperty { window, property });
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
            if conversion == Conversion::TargetList {
                self.cancel_capture();
                return None;
            }
            self.cancel_capture();
            return String::from_utf8(incoming.bytes).ok();
        }

        // We do not need an enormous or incrementally encoded TARGETS list to
        // classify a clipboard. Delete each chunk to release the source owner,
        // but retain bytes only for a bounded format-8 text conversion.
        if conversion == Conversion::TargetList
            || peek.bytes_after() > MAX_INCOMING_CHUNK_BYTES
            || !valid_type
        {
            self.conn
                .send_request(&x::DeleteProperty { window, property });
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

        let cookie = self.conn.send_request(&x::GetProperty {
            delete: true,
            window,
            property,
            r#type: ATOM_ANY,
            long_offset: 0,
            long_length: peek.bytes_after().div_ceil(4),
        });
        let reply = self.conn.wait_for_reply(cookie).ok()?;
        if let Some(capture) = self.capture.as_mut() {
            if let Some(incoming) = capture.incoming_incr.as_mut() {
                if reply.r#type() != target || reply.format() != 8 {
                    incoming.oversized = true;
                } else {
                    let value = reply.value::<u8>();
                    if incoming.bytes.len().saturating_add(value.len()) > clipboard::MAX_TEXT_BYTES
                    {
                        incoming.oversized = true;
                    } else if !incoming.oversized {
                        incoming.bytes.extend_from_slice(value);
                    }
                }
            }
            capture.last_activity = std::time::Instant::now();
        }
        None
    }

    fn expire_outgoing_incr(&mut self) {
        let now = std::time::Instant::now();
        let expired: Vec<x::Window> = self
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

    fn stop_watching_requestor_if_idle(&self, requestor: x::Window) {
        if self
            .outgoing_incr
            .keys()
            .any(|(window, _)| *window == requestor)
        {
            return;
        }
        self.conn.send_request(&x::ChangeWindowAttributes {
            window: requestor,
            value_list: &[x::Cw::EventMask(x::EventMask::NO_EVENT)],
        });
        let _ = self.conn.flush();
    }

    fn reply(&self, event: &x::SelectionRequestEvent, property: Atom) {
        self.conn.send_request(&x::SendEvent {
            propagate: false,
            destination: x::SendEventDest::Window(event.requestor()),
            event_mask: x::EventMask::NO_EVENT,
            event: &x::SelectionNotifyEvent::new(
                event.time(),
                event.requestor(),
                event.selection(),
                event.target(),
                property,
            ),
        });
        let _ = self.conn.flush();
    }
}

fn publish_capture(
    captured: &std::sync::mpsc::Sender<String>,
    notifier: &std::sync::Mutex<Option<crate::backend::update_notifier::AsyncUpdateNotifier>>,
    text: String,
) -> bool {
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

    fn wait_for_selection_owner(conn: &Connection, selection: Atom) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let cookie = conn.send_request(&x::GetSelectionOwner { selection });
            if conn
                .wait_for_reply(cookie)
                .is_ok_and(|reply| reply.owner() != x::WINDOW_NONE)
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

    fn receive_png(
        conn: &Connection,
        requestor: x::Window,
        clipboard: Atom,
        image_png: Atom,
        incr: Atom,
        property: Atom,
    ) -> (Vec<u8>, Option<u32>) {
        conn.send_request(&x::ConvertSelection {
            requestor,
            selection: clipboard,
            target: image_png,
            property,
            time: x::CURRENT_TIME,
        });
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
                xcb::Event::X(x::Event::SelectionNotify(event))
                    if event.requestor() == requestor =>
                {
                    assert_ne!(event.property(), x::ATOM_NONE, "selection was refused");
                    let cookie = conn.send_request(&x::GetProperty {
                        delete: true,
                        window: requestor,
                        property,
                        r#type: ATOM_ANY,
                        long_offset: 0,
                        long_length: u32::MAX,
                    });
                    let reply = conn.wait_for_reply(cookie).unwrap();
                    if reply.r#type() == incr {
                        let values = reply.value::<u32>();
                        assert_eq!(values.len(), 1, "INCR must announce one byte count");
                        announced = Some(values[0]);
                        incremental = true;
                    } else {
                        assert_eq!(reply.r#type(), image_png);
                        return (reply.value::<u8>().to_vec(), None);
                    }
                }
                xcb::Event::X(x::Event::PropertyNotify(event))
                    if incremental
                        && event.window() == requestor
                        && event.atom() == property
                        && event.state() == x::Property::NewValue =>
                {
                    let cookie = conn.send_request(&x::GetProperty {
                        delete: true,
                        window: requestor,
                        property,
                        r#type: ATOM_ANY,
                        long_offset: 0,
                        long_length: u32::MAX,
                    });
                    let reply = conn.wait_for_reply(cookie).unwrap();
                    assert_eq!(reply.r#type(), image_png);
                    let chunk = reply.value::<u8>();
                    if chunk.is_empty() {
                        return (bytes, announced);
                    }
                    bytes.extend_from_slice(chunk);
                }
                _ => {}
            }
        }
    }

    fn convert_and_wait(
        conn: &Connection,
        requestor: x::Window,
        selection: Atom,
        target: Atom,
        property: Atom,
        time: u32,
    ) -> x::SelectionNotifyEvent {
        conn.send_request(&x::ConvertSelection {
            requestor,
            selection,
            target,
            property,
            time,
        });
        conn.flush().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "selection reply timed out"
            );
            match conn.poll_for_event().unwrap() {
                Some(xcb::Event::X(x::Event::SelectionNotify(event)))
                    if event.requestor() == requestor
                        && event.selection() == selection
                        && event.target() == target =>
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

    #[test]
    #[ignore = "requires an isolated X11 server in DISPLAY"]
    fn native_png_offer_serves_payload_beyond_xclips_one_mib_cliff() {
        let _serial = crate::backend::clipboard_offer::X11_CLIPBOARD_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let clipboard_owner = Clipboard::start().unwrap();
        let (conn, screen_num) = Connection::connect(None).unwrap();
        let root = conn
            .get_setup()
            .roots()
            .nth(screen_num as usize)
            .unwrap()
            .root();
        let clipboard = intern(&conn, "CLIPBOARD").unwrap();
        let image_png = intern(&conn, "image/png").unwrap();
        let incr = intern(&conn, "INCR").unwrap();
        let property = intern(&conn, "JWM_TEST_IMAGE").unwrap();
        let requestor: x::Window = conn.generate_id();
        conn.send_and_check_request(&x::CreateWindow {
            depth: 0,
            wid: requestor,
            parent: root,
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            border_width: 0,
            class: x::WindowClass::InputOnly,
            visual: x::COPY_FROM_PARENT,
            value_list: &[x::Cw::EventMask(x::EventMask::PROPERTY_CHANGE)],
        })
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

    #[test]
    #[ignore = "requires an isolated X11 server in DISPLAY"]
    fn native_owner_metadata_multiple_and_direct_round_trip() {
        let _serial = crate::backend::clipboard_offer::X11_CLIPBOARD_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let clipboard_owner = Clipboard::start().unwrap();
        let (conn, screen_num) = Connection::connect(None).unwrap();
        let root = conn
            .get_setup()
            .roots()
            .nth(screen_num as usize)
            .unwrap()
            .root();
        let clipboard = intern(&conn, "CLIPBOARD").unwrap();
        let targets = intern(&conn, "TARGETS").unwrap();
        let multiple = intern(&conn, "MULTIPLE").unwrap();
        let timestamp = intern(&conn, "TIMESTAMP").unwrap();
        let atom_pair = intern(&conn, "ATOM_PAIR").unwrap();
        let utf8 = intern(&conn, "UTF8_STRING").unwrap();
        let unknown = intern(&conn, "JWM_TEST_UNKNOWN_TARGET").unwrap();
        let requestor: x::Window = conn.generate_id();
        conn.send_and_check_request(&x::CreateWindow {
            depth: 0,
            wid: requestor,
            parent: root,
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            border_width: 0,
            class: x::WindowClass::InputOnly,
            visual: x::COPY_FROM_PARENT,
            value_list: &[x::Cw::EventMask(x::EventMask::PROPERTY_CHANGE)],
        })
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
            x::CURRENT_TIME,
        );
        assert_eq!(notify.property(), targets_property);
        let cookie = conn.send_request(&x::GetProperty {
            delete: true,
            window: requestor,
            property: targets_property,
            r#type: ATOM_ANY,
            long_offset: 0,
            long_length: 64,
        });
        let reply = conn.wait_for_reply(cookie).unwrap();
        assert_eq!(reply.r#type(), ATOM_ATOM);
        assert_eq!(reply.format(), 32);
        let advertised = reply.value::<Atom>();
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
            x::CURRENT_TIME,
        );
        assert_eq!(notify.property(), timestamp_property);
        let cookie = conn.send_request(&x::GetProperty {
            delete: true,
            window: requestor,
            property: timestamp_property,
            r#type: ATOM_ANY,
            long_offset: 0,
            long_length: 1,
        });
        let reply = conn.wait_for_reply(cookie).unwrap();
        assert_eq!(reply.r#type(), ATOM_INTEGER);
        assert_eq!(reply.format(), 32);
        let acquired = reply.value::<u32>()[0];
        assert_ne!(acquired, 0);

        let text_property = intern(&conn, "JWM_TEST_TEXT").unwrap();
        let notify = convert_and_wait(&conn, requestor, clipboard, utf8, text_property, acquired);
        assert_eq!(notify.time(), acquired);
        let cookie = conn.send_request(&x::GetProperty {
            delete: true,
            window: requestor,
            property: text_property,
            r#type: ATOM_ANY,
            long_offset: 0,
            long_length: 64,
        });
        let reply = conn.wait_for_reply(cookie).unwrap();
        assert_eq!(reply.r#type(), utf8);
        assert_eq!(reply.format(), 8);
        assert_eq!(reply.value::<u8>(), "hello π".as_bytes());

        let multiple_property = intern(&conn, "JWM_TEST_MULTIPLE").unwrap();
        let multiple_text = intern(&conn, "JWM_TEST_MULTIPLE_TEXT").unwrap();
        let multiple_time = intern(&conn, "JWM_TEST_MULTIPLE_TIME").unwrap();
        let multiple_bad = intern(&conn, "JWM_TEST_MULTIPLE_BAD").unwrap();
        conn.send_and_check_request(&x::ChangeProperty {
            mode: x::PropMode::Replace,
            window: requestor,
            property: multiple_property,
            r#type: atom_pair,
            data: &[
                utf8,
                multiple_text,
                timestamp,
                multiple_time,
                unknown,
                multiple_bad,
            ],
        })
        .unwrap();
        let notify = convert_and_wait(
            &conn,
            requestor,
            clipboard,
            multiple,
            multiple_property,
            acquired,
        );
        assert_eq!(notify.property(), multiple_property);
        let cookie = conn.send_request(&x::GetProperty {
            delete: false,
            window: requestor,
            property: multiple_property,
            r#type: ATOM_ANY,
            long_offset: 0,
            long_length: 6,
        });
        let reply = conn.wait_for_reply(cookie).unwrap();
        assert_eq!(reply.r#type(), atom_pair);
        assert_eq!(
            reply.value::<Atom>(),
            &[
                utf8,
                multiple_text,
                timestamp,
                multiple_time,
                x::ATOM_NONE,
                multiple_bad
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
        assert_eq!(stale.property(), x::ATOM_NONE);

        drop(clipboard_owner);
        std::thread::sleep(std::time::Duration::from_millis(30));
    }

    #[test]
    #[ignore = "requires an isolated X11 server in DISPLAY"]
    fn native_watcher_collects_and_drains_incoming_incr() {
        let _serial = crate::backend::clipboard_offer::X11_CLIPBOARD_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let clipboard_watcher = Clipboard::start().unwrap();
        let (conn, screen_num) = Connection::connect(None).unwrap();
        let root = conn
            .get_setup()
            .roots()
            .nth(screen_num as usize)
            .unwrap()
            .root();
        let clipboard = intern(&conn, "CLIPBOARD").unwrap();
        let targets = intern(&conn, "TARGETS").unwrap();
        let utf8 = intern(&conn, "UTF8_STRING").unwrap();
        let incr = intern(&conn, "INCR").unwrap();
        let owner: x::Window = conn.generate_id();
        conn.send_and_check_request(&x::CreateWindow {
            depth: 0,
            wid: owner,
            parent: root,
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            border_width: 0,
            class: x::WindowClass::InputOnly,
            visual: x::COPY_FROM_PARENT,
            value_list: &[],
        })
        .unwrap();
        conn.send_and_check_request(&x::SetSelectionOwner {
            owner,
            selection: clipboard,
            time: x::CURRENT_TIME,
        })
        .unwrap();

        let expected = "incoming INCR π stays intact".as_bytes().to_vec();
        let mut transfer: Option<(x::Window, Atom, usize)> = None;
        let mut terminal_sent = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !terminal_sent {
            assert!(std::time::Instant::now() < deadline, "fake owner timed out");
            let Some(event) = conn.poll_for_event().unwrap() else {
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            };
            match event {
                xcb::Event::X(x::Event::SelectionRequest(event))
                    if event.selection() == clipboard =>
                {
                    let property = if event.property() == x::ATOM_NONE {
                        event.target()
                    } else {
                        event.property()
                    };
                    let served = if event.target() == targets {
                        conn.send_and_check_request(&x::ChangeProperty {
                            mode: x::PropMode::Replace,
                            window: event.requestor(),
                            property,
                            r#type: ATOM_ATOM,
                            data: &[targets, utf8],
                        })
                        .is_ok()
                    } else if event.target() == utf8 {
                        let watching = conn
                            .send_and_check_request(&x::ChangeWindowAttributes {
                                window: event.requestor(),
                                value_list: &[x::Cw::EventMask(x::EventMask::PROPERTY_CHANGE)],
                            })
                            .is_ok();
                        let announced = watching
                            && conn
                                .send_and_check_request(&x::ChangeProperty {
                                    mode: x::PropMode::Replace,
                                    window: event.requestor(),
                                    property,
                                    r#type: incr,
                                    data: &[expected.len() as u32],
                                })
                                .is_ok();
                        if announced {
                            transfer = Some((event.requestor(), property, 0));
                        }
                        announced
                    } else {
                        false
                    };
                    conn.send_request(&x::SendEvent {
                        propagate: false,
                        destination: x::SendEventDest::Window(event.requestor()),
                        event_mask: x::EventMask::NO_EVENT,
                        event: &x::SelectionNotifyEvent::new(
                            event.time(),
                            event.requestor(),
                            event.selection(),
                            event.target(),
                            if served { property } else { x::ATOM_NONE },
                        ),
                    });
                    conn.flush().unwrap();
                }
                xcb::Event::X(x::Event::PropertyNotify(event))
                    if event.state() == x::Property::Delete =>
                {
                    let Some((requestor, property, offset)) = transfer.as_mut() else {
                        continue;
                    };
                    if event.window() != *requestor || event.atom() != *property {
                        continue;
                    }
                    let end = offset.saturating_add(3).min(expected.len());
                    conn.send_and_check_request(&x::ChangeProperty {
                        mode: x::PropMode::Replace,
                        window: *requestor,
                        property: *property,
                        r#type: utf8,
                        data: &expected[*offset..end],
                    })
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
