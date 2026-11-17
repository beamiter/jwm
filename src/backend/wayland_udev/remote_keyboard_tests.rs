//! Actual private Wayland protocol + production offscreen renderer + actual
//! wl_keyboard recipients. No DRM presentation or user display is exercised.
#![cfg(feature = "backend-wayland-udev")]
use super::*;
use crate::backend::wayland_udev::backend::fulfill_test_toplevel_capture;
use crate::backend::wayland_udev::compositor::headless_render::HeadlessSmithayGles;
use crate::backend::wayland_udev::image_copy_capture::{
    authority_tests::{App, Capture, shm_buffer_file},
    init_image_copy_capture,
    wire_test_client::{Client, Event, Server},
};

fn roundtrip(server: &mut Server) {
    server.roundtrip();
    server.roundtrip();
}
fn events(client: &mut Client) -> Vec<Event> {
    let events = client.events();
    assert!(
        events.iter().all(|e| !(e.sender == 1 && e.opcode == 0)),
        "unexpected protocol error"
    );
    events
}
fn keyboard(server: &mut Server, app: &mut App) -> u32 {
    let seat = app.client.bind("wl_seat", 7);
    let keyboard = app.client.new_id();
    app.client.request(seat, 1, &[keyboard]);
    roundtrip(server);
    events(&mut app.client);
    keyboard
}
fn focus(server: &mut Server, app: &App) {
    let surface = server.state.surface_for_window(app.window).unwrap();
    server
        .state
        .set_keyboard_focus(Some(surface), SERIAL_COUNTER.next_serial());
    roundtrip(server);
}
fn keys(app: &mut App, keyboard: u32) -> Vec<(u32, u32)> {
    events(&mut app.client)
        .into_iter()
        .filter(|e| e.sender == keyboard && e.opcode == 3)
        .map(|e| (e.args[2], e.args[3]))
        .collect()
}
struct Fixture {
    server: Server,
    app: App,
    keyboard: u32,
    other: App,
    other_keyboard: u32,
    capture: Option<Capture>,
    lease: u32,
    epoch: u32,
    frame: u32,
}
impl Fixture {
    fn new(gl: &mut HeadlessSmithayGles) -> Self {
        let mut server = Server::new();
        server.state.image_capture_pending =
            Some(init_image_copy_capture(&server.display.handle()));
        let mut app = App::new(&mut server);
        let keyboard = keyboard(&mut server, &mut app);
        // Bind the sole announced surface; another application cannot affect selection.
        let mut capture = Capture::new(&mut server);
        let mut other = App::new(&mut server);
        let other_keyboard = self::keyboard(&mut server, &mut other);
        focus(&mut server, &app);
        events(&mut app.client);
        events(&mut other.client);
        let (buffer, _file) = shm_buffer_file(&mut server, &mut capture.client, [0x5a; 4]);
        let frame = capture.client.new_id();
        capture.client.request(capture.session, 0, &[frame]);
        capture.client.request(frame, 1, &[buffer]);
        capture.client.request(frame, 3, &[]);
        roundtrip(&mut server);
        let queue = server.state.image_capture_pending.as_ref().unwrap().clone();
        fulfill_test_toplevel_capture(&mut gl.renderer, &mut None, &server.state, &queue);
        roundtrip(&mut server);
        assert!(
            events(&mut capture.client)
                .iter()
                .any(|e| e.sender == frame && e.opcode == 3),
            "arm requires a genuinely rendered ready frame"
        );
        let manager = capture.client.bind("jwm_remote_keyboard_manager_v1", 1);
        let lease = capture.client.new_id();
        capture.client.request(manager, 1, &[lease, capture.source]);
        roundtrip(&mut server);
        let epoch = events(&mut capture.client)
            .into_iter()
            .find(|e| e.sender == lease && e.opcode == 1)
            .expect("positive control native grant")
            .args[1];
        assert!(server.state.remote_keyboard.active.is_some());
        Self {
            server,
            app,
            keyboard,
            other,
            other_keyboard,
            capture: Some(capture),
            lease,
            epoch,
            frame,
        }
    }
    fn arm(&mut self) {
        self.capture
            .as_mut()
            .unwrap()
            .client
            .request(self.lease, 1, &[self.epoch, self.frame]);
        roundtrip(&mut self.server);
        assert!(
            events(&mut self.capture.as_mut().unwrap().client)
                .iter()
                .any(|e| e.sender == self.lease && e.opcode == 2)
        );
    }
    fn key(&mut self, epoch: u32, sequence: u32, key: u32, pressed: bool) {
        self.capture.as_mut().unwrap().client.request(
            self.lease,
            2,
            &[epoch, sequence, key, u32::from(pressed)],
        );
        roundtrip(&mut self.server);
    }
    fn denied(&mut self) {
        assert!(self.server.state.remote_keyboard.active.is_none());
        assert!(
            events(&mut self.capture.as_mut().unwrap().client)
                .iter()
                .any(|e| e.sender == self.lease && e.opcode == 4)
        );
    }
    fn none_elsewhere(&mut self) {
        assert!(keys(&mut self.other, self.other_keyboard).is_empty());
    }
}

#[test]
fn wire_keyboard_delivers_only_selected_recipient_and_balances_edges() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new(&mut gl);
    f.arm();
    f.key(f.epoch, 1, 30, true);
    assert_eq!(keys(&mut f.app, f.keyboard), [(30, 1)]);
    f.none_elsewhere();
    f.key(f.epoch, 2, 30, false);
    assert_eq!(keys(&mut f.app, f.keyboard), [(30, 0)]);
    f.none_elsewhere();
    assert_eq!(
        f.server
            .state
            .seat
            .get_keyboard()
            .unwrap()
            .pressed_keys()
            .len(),
        0,
        "remote input never changes physical pressed state"
    );
}

#[test]
fn wire_keyboard_unarmed_wrong_epoch_sequence_and_key_pairs_fail_closed() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for case in 0..6 {
        let mut f = Fixture::new(&mut gl);
        if case != 0 {
            f.arm();
        }
        match case {
            0 => f.key(f.epoch, 1, 30, true),
            1 => f.key(f.epoch + 1, 1, 30, true),
            2 => f.key(f.epoch, 2, 30, true),
            3 => f.key(f.epoch, 1, 30, false),
            4 => f.key(f.epoch, 1, 125, true), // Meta not permitted
            _ => {
                f.key(f.epoch, 1, 30, true);
                assert_eq!(keys(&mut f.app, f.keyboard), [(30, 1)]);
                events(&mut f.capture.as_mut().unwrap().client);
                f.key(f.epoch, 2, 30, true);
            }
        }
        f.denied();
        let delivered = keys(&mut f.app, f.keyboard);
        if case == 5 {
            assert_eq!(delivered, [(30, 0)]);
        } else {
            assert!(delivered.is_empty());
        }
        f.none_elsewhere();
        f.key(f.epoch, 3, 31, true);
        assert!(keys(&mut f.app, f.keyboard).is_empty());
    }
}

#[test]
fn wire_keyboard_cannot_arm_with_a_frame_from_another_source_instance() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new(&mut gl);
    let capture = f.capture.as_mut().unwrap();
    let other_source = capture.client.new_id();
    capture
        .client
        .request(capture.sources, 0, &[other_source, capture.handle]);
    let session = capture.client.new_id();
    capture
        .client
        .request(capture.manager, 0, &[session, other_source, 0]);
    let frame = capture.client.new_id();
    capture.client.request(session, 0, &[frame]);
    let (buffer, _file) = shm_buffer_file(&mut f.server, &mut capture.client, [0x5a; 4]);
    capture.client.request(frame, 1, &[buffer]);
    capture.client.request(frame, 3, &[]);
    roundtrip(&mut f.server);
    let queue = f
        .server
        .state
        .image_capture_pending
        .as_ref()
        .unwrap()
        .clone();
    fulfill_test_toplevel_capture(&mut gl.renderer, &mut None, &f.server.state, &queue);
    roundtrip(&mut f.server);
    assert!(
        events(&mut capture.client)
            .iter()
            .any(|e| e.sender == frame && e.opcode == 3)
    );
    capture.client.request(f.lease, 1, &[f.epoch, frame]);
    roundtrip(&mut f.server);
    f.denied();
    assert!(keys(&mut f.app, f.keyboard).is_empty());
    f.none_elsewhere();
}

#[test]
fn wire_keyboard_focus_away_and_back_never_revives_old_lease() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new(&mut gl);
    f.arm();
    f.key(f.epoch, 1, 30, true);
    assert_eq!(keys(&mut f.app, f.keyboard), [(30, 1)]);
    focus(&mut f.server, &f.other);
    assert_eq!(
        keys(&mut f.app, f.keyboard),
        [(30, 0)],
        "cleanup before focus transition stays with original receiver"
    );
    f.none_elsewhere();
    focus(&mut f.server, &f.app);
    f.denied();
    f.key(f.epoch, 2, 31, true);
    assert!(keys(&mut f.app, f.keyboard).is_empty());
    f.none_elsewhere();
}

#[test]
fn wire_keyboard_local_takeover_disconnect_and_timer_expiry_release_only_owned_keys() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for case in 0..3 {
        let mut f = Fixture::new(&mut gl);
        f.arm();
        f.key(f.epoch, 1, 30, true);
        assert_eq!(keys(&mut f.app, f.keyboard), [(30, 1)]);
        match case {
            0 => revoke(&mut f.server.state, LOCAL),
            1 => {
                drop(f.capture.take());
                roundtrip(&mut f.server);
            }
            _ => {
                // Age only the fixture clock; the real registered calloop callback
                // must revoke without any following remote request.
                f.server
                    .state
                    .remote_keyboard
                    .active
                    .as_mut()
                    .unwrap()
                    .created = Instant::now() - TTL;
                let deadline = Instant::now() + Duration::from_secs(1);
                while f.server.state.remote_keyboard.active.is_some() && Instant::now() < deadline {
                    f.server.dispatch_timers(Duration::from_millis(100));
                }
            }
        }
        roundtrip(&mut f.server);
        assert!(f.server.state.remote_keyboard.active.is_none());
        assert_eq!(keys(&mut f.app, f.keyboard), [(30, 0)]);
        f.none_elsewhere();
    }
}

#[test]
fn wire_keyboard_unmap_remap_and_lock_unlock_cannot_revive_permission() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for locked in [false, true] {
        let mut f = Fixture::new(&mut gl);
        f.arm();
        let mut locker = None;
        if locked {
            f.server.state.session_lock_supported = true;
            let mut client = f.server.connect();
            let manager = client.bind("ext_session_lock_manager_v1", 1);
            let lock = client.new_id();
            client.request(manager, 1, &[lock]);
            roundtrip(&mut f.server);
            assert!(f.server.state.session_locked);
            client.request(lock, 2, &[]);
            roundtrip(&mut f.server);
            assert!(
                !f.server.state.session_locked,
                "unlock negative control must really restore unlocked state"
            );
            events(&mut client);
            locker = Some(client);
        } else {
            f.app.unmap(&mut f.server);
            f.app.remap(&mut f.server);
        }
        focus(&mut f.server, &f.app);
        f.denied();
        f.key(f.epoch, 1, 30, true);
        assert!(keys(&mut f.app, f.keyboard).is_empty());
        f.none_elsewhere();
        drop(locker);
    }
}

#[test]
fn wire_keyboard_same_map_replacement_invalidates_snapshot_history() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new(&mut gl);
    f.arm();
    let map = f
        .server
        .state
        .remote_keyboard
        .active
        .as_ref()
        .unwrap()
        .snapshot
        .keymap_text()
        .to_owned();
    let keyboard = f.server.state.seat.get_keyboard().unwrap();
    keyboard
        .set_keymap_from_string(&mut f.server.state, map)
        .unwrap();
    f.key(f.epoch, 1, 30, true);
    f.denied();
    assert!(keys(&mut f.app, f.keyboard).is_empty());
    f.none_elsewhere();
}

#[test]
fn wire_keyboard_same_client_second_surface_and_new_receiver_cannot_inherit_lease() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for new_receiver in [false, true] {
        let mut f = Fixture::new(&mut gl);
        f.arm();
        if new_receiver {
            let _second = keyboard(&mut f.server, &mut f.app);
            f.key(f.epoch, 1, 30, true);
            f.denied();
            assert!(
                events(&mut f.app.client).iter().all(|e| e.opcode != 3),
                "neither old nor new keyboard receives a key"
            );
        } else {
            let compositor = f.app.client.bind("wl_compositor", 6);
            let id = f.app.client.new_id();
            f.app.client.request(compositor, 0, &[id]);
            roundtrip(&mut f.server);
            let original = f.server.state.surface_for_window(f.app.window).unwrap();
            let second = original
                .client()
                .unwrap()
                .object_from_protocol_id(&f.server.display.handle(), id)
                .unwrap();
            f.server
                .state
                .set_keyboard_focus(Some(second), SERIAL_COUNTER.next_serial());
            roundtrip(&mut f.server);
            events(&mut f.app.client);
            f.key(f.epoch, 1, 30, true);
            f.denied();
        }
        assert!(keys(&mut f.app, f.keyboard).is_empty());
        f.none_elsewhere();
    }
}

#[test]
fn wire_keyboard_foreign_connection_cannot_use_source_or_disrupt_existing_owner() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new(&mut gl);
    f.arm();
    let mut stranger = f.server.connect();
    let manager = stranger.bind("jwm_remote_keyboard_manager_v1", 1);
    let lease = stranger.new_id();
    stranger.request(manager, 1, &[lease, f.capture.as_ref().unwrap().source]);
    roundtrip(&mut f.server);
    // Foreign object numbers are not capabilities: Wayland rejects that client.
    assert!(
        stranger
            .events()
            .iter()
            .any(|e| e.sender == 1 && e.opcode == 0)
    );
    assert!(f.server.state.remote_keyboard.active.is_some());
    f.key(f.epoch, 1, 30, true);
    assert_eq!(keys(&mut f.app, f.keyboard), [(30, 1)]);
    f.none_elsewhere();
}

#[test]
fn wire_keyboard_takeover_cleanup_does_not_release_subsequent_physical_key() {
    use smithay::input::keyboard::FilterResult;
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new(&mut gl);
    f.arm();
    f.key(f.epoch, 1, 30, true);
    assert_eq!(keys(&mut f.app, f.keyboard), [(30, 1)]);
    // This is the production trusted-backend takeover hook. Hardware/backend
    // acquisition is not simulated; the following normal keyboard input is real.
    revoke(&mut f.server.state, LOCAL);
    let keyboard = f.server.state.seat.get_keyboard().unwrap();
    keyboard.input(
        &mut f.server.state,
        (30_u32 + 8).into(),
        KeyState::Pressed,
        SERIAL_COUNTER.next_serial(),
        InputTime::from_millis(100),
        |_, _, _| FilterResult::<()>::Forward,
    );
    roundtrip(&mut f.server);
    assert_eq!(keys(&mut f.app, f.keyboard), [(30, 0), (30, 1)]);
    f.key(f.epoch, 2, 30, false);
    assert!(keys(&mut f.app, f.keyboard).is_empty());
    assert!(keyboard.pressed_keys().contains(&(30_u32 + 8).into()));
    f.none_elsewhere();
}

struct TestGrab(smithay::input::keyboard::GrabStartData<JwmWaylandState>);
impl smithay::input::keyboard::KeyboardGrab<JwmWaylandState> for TestGrab {
    fn input(
        &mut self,
        _: &mut JwmWaylandState,
        _: &mut smithay::input::keyboard::KeyboardInnerHandle<'_, JwmWaylandState>,
        _: smithay::input::keyboard::Keycode,
        _: KeyState,
        _: Option<ModifiersState>,
        _: smithay::utils::Serial,
        _: InputTime,
    ) {
    }
    fn set_focus(
        &mut self,
        _: &mut JwmWaylandState,
        _: &mut smithay::input::keyboard::KeyboardInnerHandle<'_, JwmWaylandState>,
        _: Option<smithay::reexports::wayland_server::protocol::wl_surface::WlSurface>,
        _: smithay::utils::Serial,
    ) {
    }
    fn start_data(&self) -> &smithay::input::keyboard::GrabStartData<JwmWaylandState> {
        &self.0
    }
    fn unset(&mut self, _: &mut JwmWaylandState) {}
}
#[test]
fn wire_keyboard_grab_then_ungrab_cannot_restore_old_authority() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new(&mut gl);
    f.arm();
    let keyboard = f.server.state.seat.get_keyboard().unwrap();
    let start = smithay::input::keyboard::GrabStartData {
        focus: keyboard.current_focus(),
    };
    keyboard.set_grab(
        &mut f.server.state,
        TestGrab(start),
        SERIAL_COUNTER.next_serial(),
    );
    keyboard.unset_grab(&mut f.server.state);
    assert!(!keyboard.is_grabbed());
    f.key(f.epoch, 1, 30, true);
    f.denied();
    assert!(keys(&mut f.app, f.keyboard).is_empty());
    f.none_elsewhere();
}

#[test]
fn wire_keyboard_consumed_source_cannot_grant_again_after_revoke_or_expiry() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for expired in [false, true] {
        let mut f = Fixture::new(&mut gl);
        f.arm();
        if expired {
            f.server
                .state
                .remote_keyboard
                .active
                .as_mut()
                .unwrap()
                .created = Instant::now() - TTL;
            let deadline = Instant::now() + Duration::from_secs(1);
            while f.server.state.remote_keyboard.active.is_some() && Instant::now() < deadline {
                f.server.dispatch_timers(Duration::from_millis(100));
            }
        } else {
            revoke(&mut f.server.state, LOCAL);
            roundtrip(&mut f.server);
        }
        assert!(f.server.state.remote_keyboard.active.is_none());
        let capture = f.capture.as_mut().unwrap();
        events(&mut capture.client);
        let manager = capture.client.bind("jwm_remote_keyboard_manager_v1", 1);
        let replacement = capture.client.new_id();
        capture
            .client
            .request(manager, 1, &[replacement, capture.source]);
        roundtrip(&mut f.server);
        let replies = events(&mut capture.client);
        assert!(
            replies
                .iter()
                .any(|e| e.sender == replacement && e.opcode == 4)
        );
        assert!(
            !replies
                .iter()
                .any(|e| e.sender == replacement && e.opcode == 1)
        );
        assert!(f.server.state.remote_keyboard.active.is_none());
        assert!(keys(&mut f.app, f.keyboard).is_empty());
        f.none_elsewhere();
    }
}
