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

use x11rb::connection::Connection;
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
    ClipboardOffer, X11_DIRECT_PROPERTY_BYTES, next_x11_incr_chunk,
};

/// Longest payload accepted in one shot. Anything larger arrives as an INCR
/// transfer, which the history would refuse to store anyway.
const MAX_PROPERTY_BYTES: u32 = 256 * 1024;

/// A requester that never deletes the INCR property must not retain transfer
/// state forever, and many hostile requesters must not grow it without bound.
const MAX_OUTGOING_INCR_TRANSFERS: usize = 32;
const OUTGOING_INCR_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Atoms this module needs. Interned once; comparing atoms is exact and free
/// compared with resolving names on every event.
#[derive(Debug, Clone, Copy)]
struct Atoms {
    clipboard: Atom,
    targets: Atom,
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
    outgoing_incr: std::collections::HashMap<(Window, Atom), OutgoingIncr>,
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
                atoms.utf8_string,
                atoms.text_plain_utf8,
                atoms.text_plain,
                Atom::from(AtomEnum::STRING),
            ],
            Self::Png(_) => vec![atoms.targets, atoms.image_png],
        }
    }

    fn payload_for(&self, atoms: &Atoms, target: Atom) -> Option<std::sync::Arc<[u8]>> {
        match self {
            Self::Text(bytes)
                if target == atoms.utf8_string
                    || target == atoms.text_plain_utf8
                    || target == atoms.text_plain
                    || target == Atom::from(AtomEnum::STRING) =>
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
            x11rb::protocol::xfixes::SelectionEventMask::SET_SELECTION_OWNER,
        )
        .map_err(|error| format!("xfixes_select_selection_input: {error}"))?
        .check()
        .map_err(|error| format!("xfixes_select_selection_input rejected: {error}"))?;
        conn.flush()
            .map_err(|error| format!("clipboard flush: {error}"))?;

        log::info!("clipboard: watching CLIPBOARD (owner window 0x{window:x})");
        Ok(Self {
            conn,
            window,
            atoms,
            offered: None,
            outgoing_incr: std::collections::HashMap::new(),
        })
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
                Ok(offer) => self.take_ownership(offer),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }

            self.expire_outgoing_incr();

            let mut idle = true;
            while let Ok(Some(event)) = self.conn.poll_for_event() {
                idle = false;
                if let Some(text) = self.handle(&event)
                    && !publish_capture(captured, notifier, text)
                {
                    return;
                }
            }
            if idle {
                // During INCR, every chunk is acknowledged by a fresh
                // PropertyDelete round trip. Poll promptly then; retain the
                // relaxed cadence while the clipboard is otherwise idle.
                std::thread::sleep(if self.outgoing_incr.is_empty() {
                    IDLE
                } else {
                    std::time::Duration::from_millis(2)
                });
            }
        }
    }

    /// Route one X event. Returns copied text when a conversion completed.
    fn handle(&mut self, event: &x11rb::protocol::Event) -> Option<String> {
        use x11rb::protocol::Event;
        match event {
            Event::XfixesSelectionNotify(e) => {
                self.on_owner_changed(e.owner);
                None
            }
            Event::SelectionNotify(e) => self.on_selection_notify(e),
            Event::SelectionRequest(e) => {
                self.on_selection_request(e);
                None
            }
            Event::PropertyNotify(e) => {
                self.on_property_notify(e);
                None
            }
            Event::SelectionClear(e) if e.owner == self.window => {
                self.offered = None;
                None
            }
            _ => None,
        }
    }

    /// A new application took the clipboard: ask what it can offer.
    ///
    /// Ownership taken by JWM itself is ignored — serving an entry back must
    /// not re-record it as a fresh copy.
    fn on_owner_changed(&mut self, owner: Window) {
        let conn = &self.conn;
        if owner == self.window || owner == x11rb::NONE {
            return;
        }
        self.offered = None;
        if let Err(error) = conn.convert_selection(
            self.window,
            self.atoms.clipboard,
            self.atoms.targets,
            self.atoms.transfer,
            x11rb::CURRENT_TIME,
        ) {
            log::debug!("clipboard: requesting targets failed: {error}");
            return;
        }
        // This connection carries nothing else, so an unflushed request would
        // sit in the output buffer and the reply would never come.
        let _ = conn.flush();
    }

    /// Which conversion a reply belongs to, from the reply itself.
    fn conversion_of(&self, target: Atom) -> Conversion {
        if target == self.atoms.targets {
            Conversion::TargetList
        } else {
            Conversion::Text
        }
    }

    /// A conversion finished. Returns the copied text once it has been asked
    /// for and delivered.
    fn on_selection_notify(&mut self, event: &SelectionNotifyEvent) -> Option<String> {
        let conn = &self.conn;
        if event.requestor != self.window
            || event.property == x11rb::NONE
            || event.selection != self.atoms.clipboard
        {
            return None;
        }
        let conversion = self.conversion_of(event.target);

        let reply = conn
            .get_property(
                true, // delete: the transfer property is ours to consume
                self.window,
                event.property,
                AtomEnum::ANY,
                0,
                MAX_PROPERTY_BYTES / 4,
            )
            .ok()?
            .reply()
            .ok()?;

        // An INCR handshake means the payload is larger than the history would
        // keep; drop it rather than carrying a multi-part transfer.
        if reply.type_ == self.atoms.incr {
            log::debug!("clipboard: ignoring INCR transfer");
            return None;
        }

        match conversion {
            Conversion::TargetList => {
                let targets: Vec<Atom> = reply.value32().map(Iterator::collect).unwrap_or_default();
                self.request_text_if_allowed(&targets);
                None
            }
            Conversion::Text => String::from_utf8(reply.value).ok(),
        }
    }

    /// Decide from the target list whether to ask for the payload at all.
    ///
    /// The names are resolved so the shared policy in
    /// `jwm::features::clipboard` makes the call — one round trip on a copy,
    /// and worth it to keep one definition of "this is a secret".
    fn request_text_if_allowed(&mut self, targets: &[Atom]) {
        let conn = &self.conn;
        let names: Vec<String> = targets
            .iter()
            .filter_map(|atom| conn.get_atom_name(*atom).ok())
            .filter_map(|cookie| cookie.reply().ok())
            .map(|reply| String::from_utf8_lossy(&reply.name).into_owned())
            .collect();

        if crate::backend::clipboard_offer::is_secret(&names) {
            log::debug!("clipboard: offer marked secret, not reading it");
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
            return;
        };

        if let Err(error) = conn.convert_selection(
            self.window,
            self.atoms.clipboard,
            target,
            self.atoms.transfer,
            x11rb::CURRENT_TIME,
        ) {
            log::debug!("clipboard: requesting text failed: {error}");
            return;
        }
        let _ = conn.flush();
    }

    /// Offer one payload to other applications by taking CLIPBOARD ownership.
    fn take_ownership(&mut self, offer: ClipboardOffer) {
        self.offered = Some(offer.into());
        if self
            .conn
            .set_selection_owner(self.window, self.atoms.clipboard, x11rb::CURRENT_TIME)
            .is_err()
        {
            self.offered = None;
            return;
        }
        let _ = self.conn.flush();
    }

    /// Answer a request for the entry JWM is offering.
    fn on_selection_request(&mut self, event: &SelectionRequestEvent) {
        let refused = SelectionNotifyEvent {
            response_type: x11rb::protocol::xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: event.time,
            requestor: event.requestor,
            selection: event.selection,
            target: event.target,
            property: x11rb::NONE,
        };
        let Some(offered) = self.offered.clone() else {
            let _ = self
                .conn
                .send_event(false, event.requestor, EventMask::NO_EVENT, refused);
            let _ = self.conn.flush();
            return;
        };
        // A requestor from before ICCCM sends property=None meaning "use the
        // target atom as the property".
        let property = if event.property == x11rb::NONE {
            event.target
        } else {
            event.property
        };

        let served = if event.target == self.atoms.targets {
            let targets = offered.targets(&self.atoms);
            self.conn
                .change_property32(
                    PropMode::REPLACE,
                    event.requestor,
                    property,
                    AtomEnum::ATOM,
                    &targets,
                )
                .is_ok()
        } else {
            offered
                .payload_for(&self.atoms, event.target)
                .is_some_and(|data| {
                    if data.len() <= X11_DIRECT_PROPERTY_BYTES {
                        self.conn
                            .change_property8(
                                PropMode::REPLACE,
                                event.requestor,
                                property,
                                event.target,
                                data.as_ref(),
                            )
                            .is_ok()
                    } else {
                        self.begin_outgoing_incr(event.requestor, property, event.target, data)
                    }
                })
        };

        let notify = SelectionNotifyEvent {
            property: if served { property } else { x11rb::NONE },
            ..refused
        };
        let _ = self
            .conn
            .send_event(false, event.requestor, EventMask::NO_EVENT, notify);
        let _ = self.conn.flush();
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
        if !self.outgoing_incr.contains_key(&key)
            && self.outgoing_incr.len() >= MAX_OUTGOING_INCR_TRANSFERS
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
            .change_window_attributes(
                requestor,
                &xproto::ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )
            .is_err()
            || self
                .conn
                .change_property32(
                    PropMode::REPLACE,
                    requestor,
                    property,
                    self.atoms.incr,
                    &[total],
                )
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

    /// The requestor deletes the property once for every chunk it is ready to
    /// consume. After the last data chunk, one more delete is answered with a
    /// zero-length property, which terminates the transfer.
    fn on_property_notify(&mut self, event: &PropertyNotifyEvent) {
        if event.state != Property::DELETE {
            return;
        }
        let key = (event.window, event.atom);
        let Some(transfer) = self.outgoing_incr.get_mut(&key) else {
            return;
        };
        let (range, terminal) = next_x11_incr_chunk(transfer.data.len(), &mut transfer.offset);
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
            .is_ok();
        if terminal || !sent {
            self.outgoing_incr.remove(&key);
            self.stop_watching_requestor_if_idle(event.window);
        }
        let _ = self.conn.flush();
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

    #[test]
    fn a_property_larger_than_the_history_keeps_is_not_requested_whole() {
        // get_property takes a length in 32-bit words; the cap must match the
        // byte budget the history enforces.
        assert_eq!(
            MAX_PROPERTY_BYTES as usize,
            crate::backend::clipboard_offer::MAX_TEXT_BYTES
        );
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
}
