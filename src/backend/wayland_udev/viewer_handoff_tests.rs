//! Actual private wire requests, focus events and production keyboard router.
use super::*;
use crate::backend::wayland_udev::image_copy_capture::{
    authority_tests::App,
    wire_test_client::{Event, Server},
};
use smithay::backend::input::{InputTime, KeyState};
use smithay::input::pointer::MotionEvent;
use std::collections::HashSet;
fn sync(s: &mut Server) {
    s.roundtrip();
    s.roundtrip();
}
struct Fixture {
    server: Server,
    viewer: App,
    prior: App,
    session: u32,
    pointer: u32,
    keyboard: u32,
    prior_keyboard: u32,
    suppressed: HashSet<u8>,
}
impl Fixture {
    fn new() -> Self {
        let mut server = Server::new();
        init(&server.display.handle());
        let mut prior = App::new(&mut server);
        let priorseat = prior.client.bind("wl_seat", 7);
        let prior_keyboard = prior.client.new_id();
        prior.client.request(priorseat, 1, &[prior_keyboard]);
        let mut viewer = App::new(&mut server);
        let seat = viewer.client.bind("wl_seat", 7);
        let pointer = viewer.client.new_id();
        let keyboard = viewer.client.new_id();
        viewer.client.request(seat, 0, &[pointer]);
        viewer.client.request(seat, 1, &[keyboard]);
        sync(&mut server);
        let old = server.state.surface_for_window(prior.window).unwrap();
        change_focus(&mut server.state, Some(old));
        sync(&mut server);
        prior.client.events();
        viewer.client.events();
        let manager = viewer
            .client
            .bind("jwm_remote_viewer_handoff_manager_v1", 1);
        let session = viewer.client.new_id();
        viewer
            .client
            .request(manager, 1, &[session, viewer.surface, seat, 30_000]);
        sync(&mut server);
        assert!(
            viewer
                .client
                .events()
                .iter()
                .any(|e| e.sender == session && e.opcode == 0)
        );
        Self {
            server,
            viewer,
            prior,
            session,
            pointer,
            keyboard,
            prior_keyboard,
            suppressed: HashSet::new(),
        }
    }
    fn motion(&mut self, inside: bool) -> u32 {
        let pointer = self.server.state.seat.get_pointer().unwrap();
        let surface = inside.then(|| {
            self.server
                .state
                .surface_for_window(self.viewer.window)
                .unwrap()
        });
        let serial = SERIAL_COUNTER.next_serial();
        pointer.motion(
            &mut self.server.state,
            surface.map(|s| (s, (0.0, 0.0).into())),
            &MotionEvent {
                location: (5.0, 5.0).into(),
                serial,
                time: InputTime::now(),
            },
        );
        pointer.frame(&mut self.server.state);
        sync(&mut self.server);
        serial.into()
    }
    fn enter(&mut self, seq: u32) -> Vec<Event> {
        let serial = self.motion(true);
        self.viewer.client.request(self.session, 1, &[seq, serial]);
        sync(&mut self.server);
        let observed = self.viewer.client.events();
        assert!(
            observed.iter().any(|event| event.sender == self.pointer
                && event.opcode == 0
                && event.args.first() == Some(&serial)),
            "entry uses an actual recipient pointer-enter serial"
        );
        observed
    }
    fn exit(&mut self, seq: u32) -> Vec<Event> {
        self.viewer.client.request(self.session, 2, &[seq]);
        sync(&mut self.server);
        self.viewer.client.events()
    }
    fn key(&mut self, evdev: u32, pressed: bool) -> super::super::backend::HandoffKeyOutcome {
        let result = super::super::backend::route_viewer_handoff_keyboard(
            &mut self.server.state,
            (evdev + 8).into(),
            if pressed {
                KeyState::Pressed
            } else {
                KeyState::Released
            },
            SERIAL_COUNTER.next_serial(),
            InputTime::now(),
            &crate::config::CONFIG.load(),
            &mut self.suppressed,
        )
        .expect("actual handoff route");
        sync(&mut self.server);
        result
    }
}
#[test]
fn handoff_wire_order_focus_inhibition_and_fresh_serial_reentry() {
    let mut f = Fixture::new();
    let events = f.enter(1);
    let transition = events
        .iter()
        .position(|e| e.sender == f.session && e.opcode == 1)
        .unwrap();
    let enter = events
        .iter()
        .position(|e| e.sender == f.keyboard && e.opcode == 1)
        .unwrap();
    let active_ack = events
        .iter()
        .position(|e| e.sender == f.session && e.opcode == 2)
        .unwrap();
    assert!(transition < enter && enter < active_ack);
    assert!(inhibits(&f.server.state));
    let serial = f
        .server
        .state
        .seat
        .get_pointer()
        .unwrap()
        .last_enter()
        .unwrap();
    let events = f.exit(2);
    assert!(
        events
            .iter()
            .any(|e| e.sender == f.session && e.opcode == 3)
    );
    assert!(!inhibits(&f.server.state));
    assert_eq!(
        f.server.state.seat.get_keyboard().unwrap().current_focus(),
        f.server.state.surface_for_window(f.prior.window)
    );
    // Inside-surface exit is valid, but the consumed serial cannot reacquire.
    f.viewer.client.request(f.session, 1, &[3, serial.into()]);
    sync(&mut f.server);
    assert!(
        f.viewer
            .client
            .events()
            .iter()
            .any(|e| e.sender == f.session && e.opcode == 4)
    );
    assert!(!registered_for_test(&f.server.state));
}
#[test]
fn handoff_held_exit_quarantines_real_keys_without_replaying_prior() {
    let mut f = Fixture::new();
    f.enter(1);
    f.prior.client.events();
    f.key(42, true);
    let delivered = f.viewer.client.events();
    assert!(
        delivered
            .iter()
            .any(|e| e.sender == f.keyboard && e.opcode == 3 && e.args[2..] == [42, 1])
    );
    f.exit(2);
    assert!(quarantined(&f.server.state));
    assert!(
        f.server
            .state
            .seat
            .get_keyboard()
            .unwrap()
            .current_focus()
            .is_none()
    );
    assert!(
        f.prior
            .client
            .events()
            .iter()
            .all(|e| e.sender != f.prior_keyboard)
    );
    f.key(42, false);
    assert!(!quarantined(&f.server.state));
    let restored = f.prior.client.events();
    let enter = restored
        .iter()
        .find(|e| e.sender == f.prior_keyboard && e.opcode == 1)
        .unwrap();
    assert_eq!(
        enter.args.last(),
        Some(&0),
        "restored held-key array is empty"
    );
    assert!(
        !restored
            .iter()
            .any(|e| e.sender == f.prior_keyboard && e.opcode == 3)
    );
    f.motion(false);
    let e = f.enter(3);
    assert!(e.iter().any(|e| e.sender == f.session && e.opcode == 2));
}
#[test]
fn handoff_actual_router_reserves_lock_and_emergency_but_forwards_typing() {
    for emergency in [false, true] {
        let mut f = Fixture::new();
        f.enter(1);
        f.key(30, true);
        f.key(30, false);
        assert_eq!(
            f.viewer
                .client
                .events()
                .iter()
                .filter(|e| e.sender == f.keyboard && e.opcode == 3)
                .count(),
            2
        );
        // The default Alt+r launcher shortcut is ordinary remote input here,
        // unlike the explicitly reserved lock functions below.
        f.key(56, true);
        assert!(f.key(19, true).wm_key.is_none());
        assert!(
            f.viewer
                .client
                .events()
                .iter()
                .any(|e| e.sender == f.keyboard && e.opcode == 3 && e.args[2..] == [19, 1])
        );
        f.key(19, false);
        f.key(56, false);
        f.viewer.client.events();
        // Default configured lock is Ctrl+Alt+Escape; emergency is Ctrl+Super+F12.
        f.key(29, true);
        f.key(if emergency { 125 } else { 56 }, true);
        f.viewer.client.events();
        let outcome = f.key(if emergency { 88 } else { 1 }, true);
        assert_eq!(outcome.wm_key.is_some(), !emergency);
        assert!(!inhibits(&f.server.state));
        assert!(quarantined(&f.server.state));
        assert!(
            !f.viewer
                .client
                .events()
                .iter()
                .any(|e| e.sender == f.keyboard && e.opcode == 3),
            "reserved final key never reaches remote viewer"
        );
        f.key(if emergency { 88 } else { 1 }, false);
        f.key(if emergency { 125 } else { 56 }, false);
        f.key(29, false);
        assert!(!quarantined(&f.server.state));
    }
}
#[test]
fn handoff_foreign_focus_loss_expiry_and_surface_regrant_are_terminal() {
    for mode in 0..3 {
        let mut f = Fixture::new();
        f.enter(1);
        if mode == 2 {
            f.server
                .state
                .viewer_handoff
                .session
                .as_mut()
                .unwrap()
                .deadline = Instant::now();
            assert!(!watchdog(&mut f.server.state));
        } else {
            let other =
                (mode == 0).then(|| f.server.state.surface_for_window(f.prior.window).unwrap());
            f.server
                .state
                .set_keyboard_focus(other, SERIAL_COUNTER.next_serial());
        }
        sync(&mut f.server);
        assert!(
            f.viewer
                .client
                .events()
                .iter()
                .any(|e| e.sender == f.session && e.opcode == 4)
        );
        assert!(!registered_for_test(&f.server.state));
        let manager = f
            .viewer
            .client
            .bind("jwm_remote_viewer_handoff_manager_v1", 1);
        let seat = f.viewer.client.bind("wl_seat", 7);
        let next = f.viewer.client.new_id();
        f.viewer
            .client
            .request(manager, 1, &[next, f.viewer.surface, seat, 30_000]);
        sync(&mut f.server);
        assert!(
            f.viewer
                .client
                .events()
                .iter()
                .any(|e| e.sender == next && e.opcode == 4)
        );
    }
}

#[test]
fn handoff_cannot_steal_non_toplevel_keyboard_focus() {
    let mut f = Fixture::new();
    let compositor = f.prior.client.bind("wl_compositor", 6);
    let modal_id = f.prior.client.new_id();
    f.prior.client.request(compositor, 0, &[modal_id]);
    sync(&mut f.server);
    let prior = f.server.state.surface_for_window(f.prior.window).unwrap();
    let modal: WlSurface = prior
        .client()
        .unwrap()
        .object_from_protocol_id(&f.server.display.handle(), modal_id)
        .unwrap();
    f.server
        .state
        .set_keyboard_focus(Some(modal.clone()), SERIAL_COUNTER.next_serial());
    let events = f.enter(1);
    assert!(
        events
            .iter()
            .any(|e| e.sender == f.session && e.opcode == 4)
    );
    assert!(!active(&f.server.state));
    assert_eq!(
        f.server.state.seat.get_keyboard().unwrap().current_focus(),
        Some(modal)
    );
}

#[test]
fn handoff_expired_lease_revokes_before_physical_input_without_timer() {
    let mut f = Fixture::new();
    f.enter(1);
    f.key(30, true);
    assert!(
        f.viewer
            .client
            .events()
            .iter()
            .any(|e| e.sender == f.keyboard && e.opcode == 3 && e.args[2..] == [30, 1])
    );
    f.prior.client.events();
    f.server
        .state
        .viewer_handoff
        .session
        .as_mut()
        .unwrap()
        .deadline = Instant::now();
    // Deliberately never dispatch the timer: routing alone must enforce expiry.
    f.key(31, true);
    assert!(!registered_for_test(&f.server.state));
    assert!(quarantined(&f.server.state));
    assert!(
        f.viewer
            .client
            .events()
            .iter()
            .all(|e| e.sender != f.keyboard || e.opcode != 3)
    );
    assert!(
        f.prior
            .client
            .events()
            .iter()
            .all(|e| e.sender != f.prior_keyboard || e.opcode != 3)
    );
    f.key(31, false);
    f.key(30, false);
    assert!(!quarantined(&f.server.state));
    assert!(
        f.server
            .state
            .seat
            .get_keyboard()
            .unwrap()
            .pressed_keys()
            .is_empty()
    );
    assert!(
        f.viewer
            .client
            .events()
            .iter()
            .all(|e| e.sender != f.keyboard || e.opcode != 3)
    );
    assert!(
        f.prior
            .client
            .events()
            .iter()
            .all(|e| e.sender != f.prior_keyboard || e.opcode != 3)
    );
}
