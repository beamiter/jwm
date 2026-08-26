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

use crate::backend::clipboard_offer as clipboard;
use xcb::x::{self, ATOM_ANY, ATOM_ATOM, ATOM_STRING, Atom};
use xcb::{Connection, Xid};

/// Longest payload accepted in one shot, matching the history's own limit.
const MAX_PROPERTY_BYTES: u32 = clipboard::MAX_TEXT_BYTES as u32;

/// How long the thread sleeps when the connection had nothing to report.
const IDLE: std::time::Duration = std::time::Duration::from_millis(20);

#[derive(Debug, Clone, Copy)]
struct Atoms {
    clipboard: Atom,
    targets: Atom,
    utf8_string: Atom,
    text_plain_utf8: Atom,
    text_plain: Atom,
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
    serve: std::sync::mpsc::Sender<String>,
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
        self.serve.send(text.to_string()).is_ok()
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
    offered: Option<String>,
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
            utf8_string: intern(&conn, "UTF8_STRING")?,
            text_plain_utf8: intern(&conn, "text/plain;charset=utf-8")?,
            text_plain: intern(&conn, "text/plain")?,
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
            event_mask: xcb::xfixes::SelectionEventMask::SET_SELECTION_OWNER,
        })
        .map_err(|error| format!("xfixes_select_selection_input: {error}"))?;

        log::info!(
            "clipboard: watching CLIPBOARD (owner window 0x{:x})",
            window.resource_id()
        );
        Ok(Self {
            conn,
            window,
            atoms,
            offered: None,
        })
    }

    fn run(
        &mut self,
        captured: &std::sync::mpsc::Sender<String>,
        serve: &std::sync::mpsc::Receiver<String>,
        notifier: &std::sync::Mutex<Option<crate::backend::update_notifier::AsyncUpdateNotifier>>,
    ) {
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
                    && !publish_capture(captured, notifier, text)
                {
                    return;
                }
            }
            if idle {
                std::thread::sleep(IDLE);
            }
        }
    }

    fn handle(&mut self, event: &xcb::Event) -> Option<String> {
        match event {
            xcb::Event::XFixes(xcb::xfixes::Event::SelectionNotify(e)) => {
                self.on_owner_changed(e.owner());
                None
            }
            xcb::Event::X(x::Event::SelectionNotify(e)) => self.on_selection_notify(e),
            xcb::Event::X(x::Event::SelectionRequest(e)) => {
                self.on_selection_request(e);
                None
            }
            xcb::Event::X(x::Event::SelectionClear(e)) if e.owner() == self.window => {
                self.offered = None;
                None
            }
            _ => None,
        }
    }

    /// A new application took the clipboard: ask what it can offer. Ownership
    /// JWM took itself is ignored, so serving an entry does not re-record it.
    fn on_owner_changed(&mut self, owner: x::Window) {
        if owner == self.window || owner.resource_id() == 0 {
            return;
        }
        self.offered = None;
        self.convert(self.atoms.targets);
    }

    fn convert(&self, target: Atom) {
        self.conn.send_request(&x::ConvertSelection {
            requestor: self.window,
            selection: self.atoms.clipboard,
            target,
            property: self.atoms.transfer,
            time: x::CURRENT_TIME,
        });
        // This connection carries nothing else, so an unflushed request would
        // sit in the output buffer and the reply would never come.
        let _ = self.conn.flush();
    }

    fn conversion_of(&self, target: Atom) -> Conversion {
        if target == self.atoms.targets {
            Conversion::TargetList
        } else {
            Conversion::Text
        }
    }

    fn on_selection_notify(&mut self, event: &x::SelectionNotifyEvent) -> Option<String> {
        if event.requestor() != self.window
            || event.property() == x::ATOM_NONE
            || event.selection() != self.atoms.clipboard
        {
            return None;
        }
        let conversion = self.conversion_of(event.target());

        let cookie = self.conn.send_request(&x::GetProperty {
            delete: true,
            window: self.window,
            property: event.property(),
            r#type: ATOM_ANY,
            long_offset: 0,
            long_length: MAX_PROPERTY_BYTES / 4,
        });
        let reply = self.conn.wait_for_reply(cookie).ok()?;

        // An INCR handshake means a payload larger than the history keeps.
        if reply.r#type() == self.atoms.incr {
            log::debug!("clipboard: ignoring INCR transfer");
            return None;
        }

        match conversion {
            Conversion::TargetList => {
                let targets: Vec<Atom> = reply.value::<Atom>().to_vec();
                self.request_text_if_allowed(&targets);
                None
            }
            Conversion::Text => String::from_utf8(reply.value::<u8>().to_vec()).ok(),
        }
    }

    /// Decide from the target list whether to ask for the payload at all.
    /// Names are resolved so the shared policy makes the call.
    fn request_text_if_allowed(&mut self, targets: &[Atom]) {
        let cookies: Vec<_> = targets
            .iter()
            .map(|atom| self.conn.send_request(&x::GetAtomName { atom: *atom }))
            .collect();
        let names: Vec<String> = cookies
            .into_iter()
            .filter_map(|cookie| self.conn.wait_for_reply(cookie).ok())
            .map(|reply| reply.name().to_string())
            .collect();

        if clipboard::is_secret(&names) {
            log::debug!("clipboard: offer marked secret, not reading it");
            return;
        }
        let Some(target) = [
            self.atoms.text_plain_utf8,
            self.atoms.utf8_string,
            self.atoms.text_plain,
        ]
        .into_iter()
        .find(|wanted| targets.contains(wanted)) else {
            return;
        };
        self.convert(target);
    }

    /// Offer `text` by taking ownership of CLIPBOARD.
    fn take_ownership(&mut self, text: &str) {
        self.offered = Some(text.to_string());
        self.conn.send_request(&x::SetSelectionOwner {
            owner: self.window,
            selection: self.atoms.clipboard,
            time: x::CURRENT_TIME,
        });
        let _ = self.conn.flush();
    }

    /// Answer a request for the entry JWM is offering.
    fn on_selection_request(&mut self, event: &x::SelectionRequestEvent) {
        let Some(text) = self.offered.clone() else {
            self.reply(event, x::ATOM_NONE);
            return;
        };
        // A pre-ICCCM requestor sends property=None, meaning "use the target".
        let property = if event.property() == x::ATOM_NONE {
            event.target()
        } else {
            event.property()
        };

        let served = if event.target() == self.atoms.targets {
            let offered = [
                self.atoms.targets,
                self.atoms.utf8_string,
                self.atoms.text_plain_utf8,
                self.atoms.text_plain,
                ATOM_STRING,
            ];
            self.conn
                .send_and_check_request(&x::ChangeProperty {
                    mode: x::PropMode::Replace,
                    window: event.requestor(),
                    property,
                    r#type: ATOM_ATOM,
                    data: &offered,
                })
                .is_ok()
        } else if event.target() == self.atoms.utf8_string
            || event.target() == self.atoms.text_plain_utf8
            || event.target() == self.atoms.text_plain
            || event.target() == ATOM_STRING
        {
            self.conn
                .send_and_check_request(&x::ChangeProperty {
                    mode: x::PropMode::Replace,
                    window: event.requestor(),
                    property,
                    r#type: event.target(),
                    data: text.as_bytes(),
                })
                .is_ok()
        } else {
            false
        };

        self.reply(event, if served { property } else { x::ATOM_NONE });
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

    #[test]
    fn the_property_cap_matches_what_the_history_keeps() {
        assert_eq!(MAX_PROPERTY_BYTES as usize, clipboard::MAX_TEXT_BYTES);
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
}
