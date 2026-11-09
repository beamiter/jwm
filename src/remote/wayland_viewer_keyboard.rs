//! Opt-in, same-connection keyboard evidence for the existing native lease.
use super::*;
use std::collections::BTreeSet;
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;
use wayland_client::protocol::{wl_keyboard, wl_seat};
use xkbcommon::xkb;

const MAX_KEYMAP: usize = 1024 * 1024;
const MAX_EVENTS: usize = 32;
pub(super) struct KeyboardFence(pub u64);
#[derive(Default)]
pub(super) struct Keyboard {
    pub(super) seats: Vec<(u32, u32)>,
    pub(super) ambiguous: bool,
    pub(super) requested: bool,
    pub(super) seat: Option<wl_seat::WlSeat>,
    pub(super) device: Option<wl_keyboard::WlKeyboard>,
    pub(super) surface: Option<wl_surface::WlSurface>,
    pub(super) digest: Option<[u8; 32]>,
    map_state: Option<xkb::State>,
    pub(super) focused: bool,
    entered: bool,
    capabilities_seen: bool,
    modifiers_seen: bool,
    repeat_seen: bool,
    held: BTreeSet<u32>,
    events: Vec<ViewerEvent>,
    pub(super) revoked: Option<&'static str>,
    reported: bool,
    pub(super) fence: u64,
    pub(super) fence_done: u64,
}
impl Keyboard {
    pub(super) fn revoke(&mut self, reason: &'static str) {
        self.revoked.get_or_insert(reason);
        self.events.clear();
    }
    pub(super) fn global(&mut self, name: u32, version: u32) {
        if self.seats.len() >= 2 || self.seats.iter().any(|(old, _)| *old == name) {
            self.ambiguous = true;
        } else {
            self.seats.push((name, version));
        }
        if self.requested {
            self.revoke("Wayland seat topology changed");
        }
    }
    pub(super) fn removed(&mut self, name: u32) {
        if self.seats.iter().any(|(id, _)| *id == name) {
            if self.requested {
                self.revoke("Wayland seat removed");
            }
            self.seats.retain(|(id, _)| *id != name);
        }
    }
    pub(super) fn check(&self) -> RemoteResult<()> {
        if let Some(reason) = self.revoked {
            return Err(reason.into());
        }
        Ok(())
    }
    pub(super) fn pending_keys(&self) -> bool {
        !self.events.is_empty() || !self.held.is_empty()
    }
    pub(super) fn clean_ready(&self) -> bool {
        self.focused
            && self.modifiers_seen
            && self.repeat_seen
            && self.digest.is_some()
            && self.held.is_empty()
            && self.map_state.as_ref().is_some_and(|state| {
                state.serialize_mods(
                    xkb::STATE_MODS_DEPRESSED | xkb::STATE_MODS_LATCHED | xkb::STATE_MODS_LOCKED,
                ) == 0
                    && state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE) == 0
            })
    }
    pub(super) fn take_events(&mut self) -> Vec<ViewerEvent> {
        if self.revoked.is_some() {
            self.events.clear();
            if self.requested && !self.reported {
                self.reported = true;
                return vec![ViewerEvent::ReleaseAll];
            }
            return Vec::new();
        }
        std::mem::take(&mut self.events)
    }
    fn queue_key(&mut self, key: u32, pressed: bool) {
        if !self.focused
            || !self.modifiers_seen
            || !self.repeat_seen
            || !crate::remote_keyboard_protocol::keyboard_key_allowed(key)
            || key > 247
            || key == 0
            || self.events.len() >= MAX_EVENTS
        {
            self.revoke("Wayland keyboard edge lacks clean focus or exceeds bounds");
            return;
        }
        if pressed {
            if self.held.len() >= 32 || !self.held.insert(key) {
                self.revoke("Wayland repeat or held-key overflow");
                return;
            }
        } else if !self.held.remove(&key) {
            self.revoke("Wayland release without matching press");
            return;
        }
        let Some(state) = self.map_state.as_mut() else {
            self.revoke("Wayland key before keymap");
            return;
        };
        state.update_key(
            (key + 8).into(),
            if pressed {
                xkb::KeyDirection::Down
            } else {
                xkb::KeyDirection::Up
            },
        );
        if state.serialize_mods(xkb::STATE_MODS_LATCHED | xkb::STATE_MODS_LOCKED) != 0
            || state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE) != 0
        {
            self.revoke("Wayland key changed locked modifiers or group");
            return;
        }
        self.events.push(ViewerEvent::Key {
            keycode: (key + 8) as u8,
            pressed,
        });
    }
}

fn read_keymap(fd: OwnedFd, size: u32) -> RemoteResult<Vec<u8>> {
    let file = File::from(fd);
    let size = size as usize;
    let metadata = file.metadata()?;
    if !(2..=MAX_KEYMAP).contains(&size) || !metadata.is_file() || metadata.len() != size as u64 {
        return Err("Wayland keymap must be a bounded regular file".into());
    }
    let mut bytes = vec![0; size];
    file.read_exact_at(&mut bytes, 0)?;
    if bytes.last() != Some(&0) || bytes[..size - 1].contains(&0) {
        return Err("Wayland keymap must have exactly one terminal NUL".into());
    }
    Ok(bytes)
}

impl Dispatch<wl_callback::WlCallback, KeyboardFence> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        tag: &KeyboardFence,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            if tag.0 == state.keyboard.fence && tag.0 > state.keyboard.fence_done {
                state.keyboard.fence_done = tag.0;
            } else {
                state.keyboard.revoke("stale keyboard protocol fence");
            }
        }
    }
}
impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let keyboard = &mut state.keyboard;
        if keyboard.revoked.is_some() {
            return;
        }
        match event {
            wl_seat::Event::Capabilities {
                capabilities: WEnum::Value(capabilities),
            } => {
                if keyboard.capabilities_seen
                    || !capabilities.contains(wl_seat::Capability::Keyboard)
                {
                    keyboard.revoke("Wayland seat capabilities changed or keyboard missing");
                    return;
                }
                keyboard.capabilities_seen = true;
                keyboard.device = Some(seat.get_keyboard(qh, ()));
            }
            wl_seat::Event::Name { .. } => {}
            _ => keyboard.revoke("unsupported Wayland seat event"),
        }
    }
}
impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let keyboard = &mut state.keyboard;
        if keyboard.revoked.is_some() {
            return;
        }
        match event {
            wl_keyboard::Event::Keymap {
                format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
                fd,
                size,
            } => {
                if keyboard.digest.is_some() {
                    keyboard.revoke("Wayland keymap replaced");
                    return;
                }
                let parsed = (|| -> RemoteResult<([u8; 32], xkb::State)> {
                    let bytes = read_keymap(fd, size)?;
                    let digest = crate::remote::keyboard_keymap::canonical_digest(&bytes)?;
                    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
                    let map = xkb::Keymap::new_from_string(
                        &context,
                        std::str::from_utf8(&bytes[..bytes.len() - 1])?.to_owned(),
                        xkb::KEYMAP_FORMAT_TEXT_V1,
                        xkb::KEYMAP_COMPILE_NO_FLAGS,
                    )
                    .ok_or("invalid Wayland XKB map")?;
                    Ok((digest, xkb::State::new(&map)))
                })();
                match parsed {
                    Ok((digest, map)) => {
                        keyboard.digest = Some(digest);
                        keyboard.map_state = Some(map);
                    }
                    Err(_) => keyboard.revoke("invalid Wayland keyboard keymap evidence"),
                }
            }
            wl_keyboard::Event::Enter { surface, keys, .. } => {
                if keyboard.entered
                    || keyboard.surface.as_ref() != Some(&surface)
                    || !keys.is_empty()
                    || keyboard.digest.is_none()
                {
                    keyboard.revoke("Wayland keyboard enter is not a fresh clean own surface");
                    return;
                }
                keyboard.entered = true;
                keyboard.focused = true;
                keyboard.modifiers_seen = false;
            }
            wl_keyboard::Event::Leave { .. } => {
                keyboard.focused = false;
                keyboard.revoke("Wayland keyboard focus left");
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                let Some(map) = keyboard.map_state.as_ref() else {
                    keyboard.revoke("Wayland modifiers before keymap");
                    return;
                };
                if !keyboard.focused
                    || mods_latched != 0
                    || mods_locked != 0
                    || group != 0
                    || mods_depressed != map.serialize_mods(xkb::STATE_MODS_DEPRESSED)
                {
                    keyboard.revoke("Wayland modifier state does not match accepted local keys");
                    return;
                }
                keyboard.modifiers_seen = true;
            }
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(wl_keyboard::KeyState::Pressed),
                ..
            } => keyboard.queue_key(key, true),
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(wl_keyboard::KeyState::Released),
                ..
            } => keyboard.queue_key(key, false),
            wl_keyboard::Event::RepeatInfo { rate, delay } => {
                if keyboard.repeat_seen
                    || !(0..=1000).contains(&rate)
                    || !(0..=60_000).contains(&delay)
                {
                    keyboard.revoke("Wayland repeat configuration changed or invalid");
                    return;
                }
                keyboard.repeat_seen = true; // Never schedule local repeat.
            }
            _ => keyboard.revoke("unsupported Wayland keyboard event"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preactivation_edges_cannot_hide_behind_empty_held_set() {
        let mut keyboard = Keyboard::default();
        assert!(!keyboard.pending_keys());
        keyboard.events.extend([
            ViewerEvent::Key {
                keycode: 38,
                pressed: true,
            },
            ViewerEvent::Key {
                keycode: 38,
                pressed: false,
            },
        ]);
        assert!(keyboard.held.is_empty());
        assert!(keyboard.pending_keys());
        keyboard.requested = true;
        keyboard.revoke("preactivation edges");
        assert_eq!(keyboard.take_events(), vec![ViewerEvent::ReleaseAll]);
        assert!(keyboard.take_events().is_empty());
        assert!(keyboard.check().is_err());
    }
    #[test]
    fn seat_history_is_bounded_and_cannot_revive_after_removal() {
        let mut keyboard = Keyboard::default();
        keyboard.global(1, 7);
        keyboard.requested = true;
        keyboard.removed(1);
        keyboard.global(1, 7);
        assert!(keyboard.check().is_err());
        for id in 2..100 {
            keyboard.global(id, 7);
        }
        assert!(keyboard.seats.len() <= 2);
        assert!(keyboard.ambiguous);
    }
    #[test]
    fn keymap_reader_requires_exact_bounded_regular_nul_terminated_storage() {
        fn fd(bytes: &[u8]) -> OwnedFd {
            let owned = memfd_create(c"jwm-viewer-keymap-test", MFdFlags::MFD_CLOEXEC).unwrap();
            let mut file = File::from(owned);
            file.write_all(bytes).unwrap();
            file.into()
        }
        assert_eq!(read_keymap(fd(b"abc\0"), 4).unwrap(), b"abc\0");
        assert!(read_keymap(fd(b"abc\0"), 3).is_err());
        assert!(read_keymap(fd(b"abc\0"), 5).is_err());
        assert!(read_keymap(fd(b"abcd"), 4).is_err());
        assert!(read_keymap(fd(b"a\0b\0"), 4).is_err());
        assert!(read_keymap(fd(b"abc\0"), MAX_KEYMAP as u32 + 1).is_err());
    }
}
