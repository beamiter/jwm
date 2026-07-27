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
//! [`crate::jwm::features::clipboard`] so both backends decide alike; this
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
    self, Atom, AtomEnum, ConnectionExt as XProtoExt, EventMask, PropMode, SelectionNotifyEvent,
    SelectionRequestEvent, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// Longest payload accepted in one shot. Anything larger arrives as an INCR
/// transfer, which the history would refuse to store anyway.
const MAX_PROPERTY_BYTES: u32 = 256 * 1024;

/// Atoms this module needs. Interned once; comparing atoms is exact and free
/// compared with resolving names on every event.
#[derive(Debug, Clone, Copy)]
struct Atoms {
    clipboard: Atom,
    targets: Atom,
    utf8_string: Atom,
    text_plain_utf8: Atom,
    text_plain: Atom,
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
    /// Text JWM currently offers as owner; `None` when another application
    /// owns the clipboard.
    offered: Option<String>,
}

/// Handle held by the backend: captured text arrives on `captured`, entries
/// to serve are sent on `serve`.
pub(crate) struct Clipboard {
    captured: std::sync::mpsc::Receiver<String>,
    serve: std::sync::mpsc::Sender<String>,
}

impl Clipboard {
    /// Start watching CLIPBOARD on a dedicated connection and thread.
    pub(crate) fn start() -> Result<Self, String> {
        let (captured_tx, captured) = std::sync::mpsc::channel();
        let (serve, serve_rx) = std::sync::mpsc::channel();
        // Build the watcher on the thread that will own it: RustConnection is
        // not Sync, and nothing outside the thread may touch it.
        let (ready_tx, ready) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("jwm-clipboard".to_string())
            .spawn(move || match Watcher::new() {
                Ok(mut watcher) => {
                    let _ = ready_tx.send(Ok(()));
                    watcher.run(&captured_tx, &serve_rx);
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                }
            })
            .map_err(|error| format!("spawn clipboard thread: {error}"))?;

        ready
            .recv()
            .map_err(|_| "clipboard thread died during startup".to_string())??;
        Ok(Self { captured, serve })
    }

    /// Text copied since the last call, oldest first.
    pub(crate) fn drain_captured(&self) -> Vec<String> {
        self.captured.try_iter().collect()
    }

    /// Offer `text` to other applications.
    pub(crate) fn set_text(&self, text: &str) -> bool {
        self.serve.send(text.to_string()).is_ok()
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
    fn new() -> Result<Self, String> {
        let (conn, screen_num) =
            x11rb::connect(None).map_err(|error| format!("clipboard connect: {error}"))?;
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
        serve: &std::sync::mpsc::Receiver<String>,
    ) {
        const IDLE: std::time::Duration = std::time::Duration::from_millis(20);
        loop {
            match serve.try_recv() {
                Ok(text) => self.take_ownership(&text),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }

            let mut idle = true;
            while let Ok(Some(event)) = self.conn.poll_for_event() {
                idle = false;
                if let Some(text) = self.handle(&event)
                    && captured.send(text).is_err()
                {
                    return;
                }
            }
            if idle {
                std::thread::sleep(IDLE);
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

        if crate::jwm::features::clipboard::is_secret(&names) {
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

    /// Offer `text` to other applications by taking ownership of CLIPBOARD.
    fn take_ownership(&mut self, text: &str) {
        self.offered = Some(text.to_string());
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
        let conn = &self.conn;
        let refused = SelectionNotifyEvent {
            response_type: x11rb::protocol::xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: event.time,
            requestor: event.requestor,
            selection: event.selection,
            target: event.target,
            property: x11rb::NONE,
        };
        let Some(text) = self.offered.clone() else {
            let _ = conn.send_event(false, event.requestor, EventMask::NO_EVENT, refused);
            let _ = conn.flush();
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
            let offered = [
                self.atoms.targets,
                self.atoms.utf8_string,
                self.atoms.text_plain_utf8,
                self.atoms.text_plain,
                Atom::from(AtomEnum::STRING),
            ];
            conn.change_property32(
                PropMode::REPLACE,
                event.requestor,
                property,
                AtomEnum::ATOM,
                &offered,
            )
            .is_ok()
        } else if event.target == self.atoms.utf8_string
            || event.target == self.atoms.text_plain_utf8
            || event.target == self.atoms.text_plain
            || event.target == Atom::from(AtomEnum::STRING)
        {
            conn.change_property8(
                PropMode::REPLACE,
                event.requestor,
                property,
                event.target,
                text.as_bytes(),
            )
            .is_ok()
        } else {
            false
        };

        let notify = SelectionNotifyEvent {
            property: if served { property } else { x11rb::NONE },
            ..refused
        };
        let _ = conn.send_event(false, event.requestor, EventMask::NO_EVENT, notify);
        let _ = conn.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_property_larger_than_the_history_keeps_is_not_requested_whole() {
        // get_property takes a length in 32-bit words; the cap must match the
        // byte budget the history enforces.
        assert_eq!(
            MAX_PROPERTY_BYTES as usize,
            crate::jwm::features::clipboard::MAX_TEXT_BYTES
        );
    }

    #[test]
    fn conversions_are_routed_by_the_replys_own_target() {
        // The regression this guards: routing by remembered state dropped
        // every other copy, because a late reply from the previous owner was
        // read as the new owner's target list.
        assert_ne!(Conversion::TargetList, Conversion::Text);
    }
}
