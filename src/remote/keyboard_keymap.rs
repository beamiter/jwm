//! Strict, full-map keyboard evidence. Never use the legacy shared-key digest.
use super::RemoteResult;
use sha2::{Digest, Sha256};
use std::io;
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::wrapper::ConnectionExt as _;
use xcb::{XidNew, x, xkb as xcb_xkb};
use xkbcommon::xkb;

const MAX_KEYMAP_BYTES: usize = 1024 * 1024;

pub(super) fn canonical_digest(bytes: &[u8]) -> RemoteResult<[u8; 32]> {
    let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
    if bytes.is_empty() || bytes.len() > MAX_KEYMAP_BYTES || bytes.contains(&0) {
        return Err(invalid("invalid full XKB keymap length").into());
    }
    let text = std::str::from_utf8(bytes)?;
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let map = xkb::Keymap::new_from_string(
        &context,
        text.to_owned(),
        xkb::KEYMAP_FORMAT_TEXT_V1,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .ok_or_else(|| invalid("invalid full XKB keymap"))?;
    let canonical = map.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
    if canonical.len() > MAX_KEYMAP_BYTES {
        return Err(invalid("canonical XKB keymap is too large").into());
    }
    Ok(Sha256::digest(canonical.as_bytes()).into())
}

pub(super) struct KeyboardKeymap {
    conn: xcb::Connection,
    device: i32,
    digest: [u8; 32],
    revoked: bool,
}
impl KeyboardKeymap {
    pub(super) fn connect(
        display: Option<&str>,
        viewer: &x11rb::rust_connection::RustConnection,
        window: u32,
    ) -> RemoteResult<Self> {
        let (conn, _) =
            xcb::Connection::connect_with_extensions(display, &[xcb::Extension::Xkb], &[])?;
        // A DISPLAY string is not an identity proof for a second connection.
        let atom = viewer
            .intern_atom(false, b"_JWM_REMOTE_KEYMAP_CONNECTION_PROOF")?
            .reply()?
            .atom;
        let nonce: [u8; 32] = rand::random();
        viewer
            .change_property8(
                x11rb::protocol::xproto::PropMode::REPLACE,
                window,
                atom,
                x11rb::protocol::xproto::AtomEnum::STRING,
                &nonce,
            )?
            .check()?;
        let proof = conn.wait_for_reply(conn.send_request(&x::GetProperty {
            delete: false,
            window: x::Window::new(window),
            property: x::Atom::new(atom),
            r#type: x::ATOM_STRING,
            long_offset: 0,
            long_length: 9,
        }));
        let local_proof = viewer
            .get_property(
                false,
                window,
                atom,
                x11rb::protocol::xproto::AtomEnum::STRING,
                0,
                9,
            )?
            .reply()?;
        if local_proof.format == 8 && local_proof.bytes_after == 0 && local_proof.value == nonce {
            viewer.delete_property(window, atom)?.check()?;
        }
        let proof = proof?;
        if proof.format() != 8 || proof.bytes_after() != 0 || proof.value::<u8>() != nonce {
            return Err(
                invalid("viewer and keymap connections refer to different X servers").into(),
            );
        }
        let (mut major, mut minor, mut event, mut error) = (0, 0, 0, 0);
        if !xkb::x11::setup_xkb_extension(
            &conn,
            1,
            0,
            xkb::x11::SetupXkbExtensionFlags::NoFlags,
            &mut major,
            &mut minor,
            &mut event,
            &mut error,
        ) {
            return Err(invalid("XKB extension unavailable").into());
        }
        let device = xkb::x11::get_core_keyboard_device_id(&conn);
        if device < 0 {
            return Err(invalid("core XKB keyboard unavailable").into());
        }
        let events = xcb_xkb::EventType::NEW_KEYBOARD_NOTIFY
            | xcb_xkb::EventType::MAP_NOTIFY
            | xcb_xkb::EventType::STATE_NOTIFY
            | xcb_xkb::EventType::NAMES_NOTIFY
            | xcb_xkb::EventType::COMPAT_MAP_NOTIFY;
        let map = xcb_xkb::MapPart::all();
        conn.check_request(conn.send_request_checked(&xcb_xkb::SelectEvents {
            device_spec: xcb_xkb::Id::UseCoreKbd as xcb_xkb::DeviceSpec,
            affect_which: events,
            clear: xcb_xkb::EventType::empty(),
            select_all: events,
            affect_map: map,
            map,
            details: &[],
        }))?;
        let mut guard = Self {
            conn,
            device,
            digest: [0; 32],
            revoked: false,
        };
        guard.digest = guard.snapshot(true)?;
        #[cfg(test)]
        eprintln!(
            "XKB guard initialization selected device={device}; starting initial history check"
        );
        guard.check(true)?;
        #[cfg(test)]
        eprintln!("XKB guard initialization complete device={device}");
        Ok(guard)
    }
    pub(super) fn digest(&self) -> [u8; 32] {
        self.digest
    }
    fn snapshot(&self, initial: bool) -> RemoteResult<[u8; 32]> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let map = xkb::x11::keymap_new_from_device(
            &context,
            &self.conn,
            self.device,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        );
        if map.get_raw_ptr().is_null() {
            return Err(invalid("XKB map unavailable").into());
        }
        let state = xkb::x11::state_new_from_device(&map, &self.conn, self.device);
        if state.get_raw_ptr().is_null() {
            return Err(invalid("XKB state unavailable").into());
        }
        let forbidden_mods = xkb::STATE_MODS_LATCHED
            | xkb::STATE_MODS_LOCKED
            | if initial {
                xkb::STATE_MODS_DEPRESSED
            } else {
                0
            };
        if state.serialize_mods(forbidden_mods) != 0
            || state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE) != 0
        {
            return Err(invalid(
                "keyboard lease requires group zero and no locked/latched modifiers",
            )
            .into());
        }
        if initial
            && self
                .conn
                .wait_for_reply(self.conn.send_request(&x::QueryKeymap {}))?
                .keys()
                .iter()
                .any(|byte| *byte != 0)
        {
            return Err(invalid("release every local key before starting a keyboard lease").into());
        }
        canonical_digest(map.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1).as_bytes())
    }
    pub(super) fn check(&mut self, initial: bool) -> RemoteResult<()> {
        let result = self.check_inner(initial);
        if result.is_err() {
            self.revoked = true;
        }
        result
    }
    fn check_inner(&mut self, initial: bool) -> RemoteResult<()> {
        if self.revoked {
            return Err(invalid("keyboard lease was permanently revoked").into());
        }
        let current = self.snapshot(initial)?; // ordered server round trips precede draining history
        if current != self.digest {
            return Err(invalid("full local XKB keymap changed").into());
        }
        for _ in 0..256 {
            let Some(event) = self.conn.poll_for_event()? else {
                return Ok(());
            };
            #[cfg(test)]
            match &event {
                xcb::Event::Xkb(xcb_xkb::Event::NewKeyboardNotify(e)) => {
                    eprintln!(
                        "XKB history initial={initial} guard_device={} NewKeyboardNotify: device={} old_device={} seq={} time={} request_major={} request_minor={} changed={:?}; full={e:?}",
                        self.device,
                        e.device_id(),
                        e.old_device_id(),
                        e.sequence(),
                        e.time(),
                        e.request_major(),
                        e.request_minor(),
                        e.changed()
                    );
                }
                xcb::Event::Xkb(xcb_xkb::Event::MapNotify(e)) => {
                    eprintln!(
                        "XKB history initial={initial} guard_device={} MapNotify: device={} seq={} time={} changed={:?}; full={e:?}",
                        self.device,
                        e.device_id(),
                        e.sequence(),
                        e.time(),
                        e.changed()
                    );
                }
                xcb::Event::Xkb(xcb_xkb::Event::NamesNotify(e)) => {
                    eprintln!(
                        "XKB history initial={initial} guard_device={} NamesNotify: device={} seq={} time={} changed={:?}; full={e:?}",
                        self.device,
                        e.device_id(),
                        e.sequence(),
                        e.time(),
                        e.changed()
                    );
                }
                xcb::Event::Xkb(xcb_xkb::Event::CompatMapNotify(e)) => {
                    eprintln!(
                        "XKB history initial={initial} guard_device={} CompatMapNotify: device={} seq={} time={} groups={:?}; full={e:?}",
                        self.device,
                        e.device_id(),
                        e.sequence(),
                        e.time(),
                        e.changed_groups()
                    );
                }
                xcb::Event::Xkb(xcb_xkb::Event::StateNotify(e)) => {
                    eprintln!(
                        "XKB history initial={initial} guard_device={} StateNotify: device={} seq={} time={} request_major={} request_minor={} changed={:?}; full={e:?}",
                        self.device,
                        e.device_id(),
                        e.sequence(),
                        e.time(),
                        e.request_major(),
                        e.request_minor(),
                        e.changed()
                    );
                }
                xcb::Event::Xkb(e) => eprintln!("XKB history initial={initial} other={e:?}"),
                _ => {}
            }
            match event {
                xcb::Event::Xkb(xcb_xkb::Event::StateNotify(state)) => {
                    if !state.locked_mods().is_empty()
                        || !state.latched_mods().is_empty()
                        || state.group() != xcb_xkb::Group::N1
                        || state.base_group() != 0
                        || state.latched_group() != 0
                        || state.locked_group() != xcb_xkb::Group::N1
                    {
                        return Err(invalid("local XKB layout/modifier history changed").into());
                    }
                }
                xcb::Event::Xkb(_) => return Err(invalid("local XKB map history changed").into()),
                _ => {}
            }
        }
        Err(invalid("local keyboard history budget exhausted").into())
    }
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keyboard_full_map_rejects_unbounded_or_invalid_text() {
        assert!(canonical_digest(&[]).is_err());
        assert!(canonical_digest(b"not a keymap").is_err());
        assert!(canonical_digest(&vec![b'x'; MAX_KEYMAP_BYTES + 1]).is_err());
        assert!(canonical_digest(b"x\0y").is_err());
    }
    /// Only the paired CI helper creates these servers. Never use DISPLAY.
    #[test]
    #[ignore = "requires scripts/test-remote-keyboard-keymap.py owned Xvfb pair"]
    fn keyboard_keymap_owned_x11_nonce_and_history() {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::xproto::{ConnectionExt as _, CreateWindowAux, WindowClass};
        use x11rb::wrapper::ConnectionExt as _;
        assert_eq!(std::env::var("JWM_KEYMAP_TEST_OWNED").as_deref(), Ok("1"));
        let first = std::env::var("JWM_KEYMAP_TEST_DISPLAY_A").expect("owned server A");
        let second = std::env::var("JWM_KEYMAP_TEST_DISPLAY_B").expect("owned server B");
        assert_ne!(first, second);
        let (a, ai) = x11rb::connect(Some(&first)).unwrap();
        let (b, bi) = x11rb::connect(Some(&second)).unwrap();
        let create = |conn: &x11rb::rust_connection::RustConnection, screen: usize| {
            let root = conn.setup().roots[screen].root;
            assert!(
                conn.query_tree(root)
                    .unwrap()
                    .reply()
                    .unwrap()
                    .children
                    .is_empty(),
                "refuse any server that already has application windows"
            );
            let window = conn.generate_id().unwrap();
            conn.create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                window,
                root,
                0,
                0,
                64,
                48,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new(),
            )
            .unwrap()
            .check()
            .unwrap();
            window
        };
        let aw = create(&a, ai);
        let bw = create(&b, bi);
        assert_eq!(aw, bw, "negative control must use identical numeric XIDs");
        let name = b"_JWM_REMOTE_KEYMAP_CONNECTION_PROOF";
        let aa = a.intern_atom(false, name).unwrap().reply().unwrap().atom;
        let ba = b.intern_atom(false, name).unwrap().reply().unwrap().atom;
        assert_eq!(
            aa, ba,
            "negative control must also have the same existing atom"
        );
        b.change_property8(
            x11rb::protocol::xproto::PropMode::REPLACE,
            bw,
            ba,
            x11rb::protocol::xproto::AtomEnum::STRING,
            &[0x55; 32],
        )
        .unwrap()
        .check()
        .unwrap();

        let mut guard = KeyboardKeymap::connect(Some(&first), &a, aw).unwrap();
        assert_ne!(guard.digest(), [0; 32]);
        assert_eq!(guard.snapshot(true).unwrap(), guard.digest());
        guard.check(true).unwrap();
        let wrong = KeyboardKeymap::connect(Some(&second), &a, aw)
            .err()
            .expect("cross-server must fail");
        assert!(wrong.to_string().contains("different X servers"), "{wrong}");
        assert_eq!(
            b.get_property(
                false,
                bw,
                ba,
                x11rb::protocol::xproto::AtomEnum::STRING,
                0,
                9
            )
            .unwrap()
            .reply()
            .unwrap()
            .value,
            vec![0x55; 32],
            "another server's property stays untouched"
        );
        assert!(
            a.get_property(false, aw, aa, x11rb::protocol::xproto::AtomEnum::ANY, 0, 9)
                .unwrap()
                .reply()
                .unwrap()
                .value
                .is_empty(),
            "own proof cleaned up"
        );

        let before = a.get_keyboard_mapping(38, 1).unwrap().reply().unwrap();
        assert!(before.keysyms_per_keycode > 0);
        let mut changed = before.keysyms.clone();
        changed[0] = if changed[0] == 0x61 { 0x7a } else { 0x61 };
        a.change_keyboard_mapping(1, 38, before.keysyms_per_keycode, &changed)
            .unwrap()
            .check()
            .unwrap();
        assert_ne!(
            guard.snapshot(true).unwrap(),
            guard.digest(),
            "mutation must really change the map"
        );
        a.change_keyboard_mapping(1, 38, before.keysyms_per_keycode, &before.keysyms)
            .unwrap()
            .check()
            .unwrap();
        assert_eq!(
            guard.snapshot(true).unwrap(),
            guard.digest(),
            "restore must restore the complete canonical map before history check"
        );
        let history = guard.check(true).unwrap_err();
        assert!(history.to_string().contains("history changed"), "{history}");
        assert!(guard.revoked);
        assert!(
            guard
                .check(true)
                .unwrap_err()
                .to_string()
                .contains("permanently revoked")
        );
        a.destroy_window(aw).unwrap().check().unwrap();
        b.destroy_window(bw).unwrap().check().unwrap();
    }
}
