//! Compatibility boundary for Smithay's virtual input-device API.
//!
//! Virtual input remains client pass-through, as before the migration: it does
//! not re-enter IME grabs, trigger WM shortcuts, authorize popups, or modify the
//! physical keyboard's XKB state. All Wayland backends share this policy.
use super::JwmWaylandState;
use smithay::backend::input::{Event, InputEvent, KeyState, KeyboardKeyEvent};
use smithay::input::Seat;
use smithay::input::keyboard::{KeymapFile, ModifiersState};
use smithay::reexports::wayland_server::{Resource, protocol::wl_surface::WlSurface};
use smithay::utils::SERIAL_COUNTER;
use smithay::wayland::virtual_keyboard::{
    VirtualKeyboardBackend, VirtualKeyboardHandler, VirtualKeyboardSpecialEvent,
};
use std::collections::HashMap;
use xkbcommon::xkb;

pub(super) struct VirtualKeyboardDelivery {
    keymap: KeymapFile,
    map_text: String,
    lock_epoch: u64,
    xkb: xkb::State,
    modifiers: ModifiersState,
    // A release (including destroy/disconnect cleanup) must not reach a new
    // focus or cross a lock/unlock cycle. wl_keyboard.leave ends the old focus.
    held: HashMap<u32, (WlSurface, u64)>,
}

impl VirtualKeyboardDelivery {
    pub(super) fn clear_held(&mut self) {
        self.held.clear();
    }

    fn reset_after_lock(&mut self, epoch: u64) {
        self.lock_epoch = epoch;
        self.held.clear();
        self.xkb.update_mask(0, 0, 0, 0, 0, 0);
        self.modifiers.update_with(&self.xkb);
    }

    fn new(map: String, lock_epoch: u64) -> Option<Self> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let map = xkb::Keymap::new_from_string(
            &context,
            map,
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )?;
        Some(Self {
            keymap: KeymapFile::new(&map),
            map_text: map.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1),
            lock_epoch,
            xkb: xkb::State::new(&map),
            modifiers: ModifiersState::default(),
            held: HashMap::new(),
        })
    }
}

impl VirtualKeyboardHandler for JwmWaylandState {
    fn process_virtual_keyboard_event(&mut self, event: InputEvent<VirtualKeyboardBackend>) {
        use VirtualKeyboardSpecialEvent::{KeymapChanged, Modifiers};
        let device = match &event {
            InputEvent::DeviceAdded { device }
            | InputEvent::DeviceRemoved { device }
            | InputEvent::Special(KeymapChanged { device })
            | InputEvent::Special(Modifiers { device, .. }) => device.clone(),
            InputEvent::Keyboard { event } => event.device(),
            _ => return,
        };
        // Do not unwrap a client-supplied seat: an inert/replaced seat is valid
        // protocol input but is not a destination for this compositor.
        if Seat::<Self>::from_resource(device.wl_seat()).as_ref() != Some(&self.seat) {
            return;
        }
        if matches!(&event, InputEvent::DeviceRemoved { .. }) {
            self.virtual_keyboards.remove(&device);
            return;
        }
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        if matches!(
            &event,
            InputEvent::DeviceAdded { .. } | InputEvent::Special(KeymapChanged { .. })
        ) {
            let map = device
                .keymap()
                .map(|map| map.to_string())
                .unwrap_or_else(|| {
                    keyboard.with_xkb_state(self, |context| {
                        context.xkb().lock().unwrap().keymap_as_string()
                    })
                });
            if let Some(mut delivery) = VirtualKeyboardDelivery::new(map, self.session_lock_epoch) {
                if let Some(old) = self.virtual_keyboards.remove(&device) {
                    // Preserve balanced key releases across a keymap update.
                    delivery.held = old.held;
                    delivery.lock_epoch = old.lock_epoch;
                    delivery.modifiers = old.modifiers;
                    let mods = old.modifiers.serialized;
                    delivery.xkb.update_mask(
                        mods.depressed,
                        mods.latched,
                        mods.locked,
                        0,
                        0,
                        mods.layout_effective,
                    );
                    delivery.modifiers.update_with(&delivery.xkb);
                }
                self.virtual_keyboards.insert(device, delivery);
            }
            return;
        }
        // Desktop virtual keyboards must never type into a locker or an app
        // behind it, including devices created before the lock. Physical lock
        // input continues through route_locked_keyboard_input.
        if self.session_locked || self.point_in_monitor_shade(self.pointer_location) {
            if let Some(delivery) = self.virtual_keyboards.get_mut(&device) {
                delivery.reset_after_lock(self.session_lock_epoch);
            }
            return;
        }
        let focus = keyboard
            .current_focus()
            .filter(|surface| surface.is_alive());
        let Some(mut delivery) = self.virtual_keyboards.remove(&device) else {
            return;
        };
        // no_keymap follows a configured seat-map reload without replacing it.
        if device.keymap().is_none() {
            let map = keyboard.with_xkb_state(self, |context| {
                context.xkb().lock().unwrap().keymap_as_string()
            });
            if map != delivery.map_text {
                if let Some(mut replacement) =
                    VirtualKeyboardDelivery::new(map, delivery.lock_epoch)
                {
                    replacement.held = std::mem::take(&mut delivery.held);
                    let mods = delivery.modifiers.serialized;
                    replacement.xkb.update_mask(
                        mods.depressed,
                        mods.latched,
                        mods.locked,
                        0,
                        0,
                        mods.layout_effective,
                    );
                    replacement.modifiers.update_with(&replacement.xkb);
                    delivery = replacement;
                }
            }
        }
        if delivery.lock_epoch != self.session_lock_epoch {
            delivery.reset_after_lock(self.session_lock_epoch);
        }
        match event {
            InputEvent::Keyboard { event } => {
                let Some(focus) = focus else {
                    self.virtual_keyboards.insert(device, delivery);
                    return;
                };
                let code = event.key_code();
                let raw = u32::from(code);
                let target = (focus, self.session_lock_epoch);
                let forward = match event.state() {
                    KeyState::Pressed => {
                        delivery.held.insert(raw, target);
                        true
                    }
                    KeyState::Released => delivery.held.remove(&raw).as_ref() == Some(&target),
                };
                if forward {
                    keyboard.forward_virtual_keyboard(
                        self,
                        &delivery.keymap,
                        delivery.modifiers,
                        Some((
                            code,
                            event.state(),
                            SERIAL_COUNTER.next_serial(),
                            event.time(),
                        )),
                    );
                    self.request_client_flush();
                }
            }
            InputEvent::Special(Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            }) => {
                delivery
                    .xkb
                    .update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                delivery.modifiers.update_with(&delivery.xkb);
                keyboard.forward_virtual_keyboard(self, &delivery.keymap, delivery.modifiers, None);
                self.request_client_flush();
            }
            _ => {}
        }
        self.virtual_keyboards.insert(device, delivery);
    }
}
