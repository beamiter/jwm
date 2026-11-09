//! Real private Wayland wire requests, production GL capture and wl_pointer
//! recipients. Not physical input acquisition, DRM or presentation acceptance.
#![cfg(feature = "backend-wayland-udev")]
use crate::backend::wayland_udev::backend::fulfill_test_toplevel_capture;
use crate::backend::wayland_udev::compositor::headless_render::HeadlessSmithayGles;
use crate::backend::wayland_udev::image_copy_capture::{
    authority_tests::{App, Capture, shm_buffer_file},
    init_image_copy_capture,
    wire_test_client::{Client, Event, Server},
};
use smithay::backend::input::InputTime;
use smithay::input::pointer::MotionEvent;
use smithay::utils::SERIAL_COUNTER;

fn sync(server: &mut Server) {
    server.roundtrip();
    server.roundtrip();
}
fn events(client: &mut Client) -> Vec<Event> {
    let events = client.events();
    assert!(
        events.iter().all(|e| !(e.sender == 1 && e.opcode == 0)),
        "wl_display error"
    );
    events
}
fn bind_pointer(server: &mut Server, app: &mut App) -> (u32, u32) {
    bind_pointer_version(server, app, 7)
}
fn bind_pointer_version(server: &mut Server, app: &mut App, version: u32) -> (u32, u32) {
    let seat = app.client.bind("wl_seat", version);
    let pointer = app.client.new_id();
    app.client.request(seat, 0, &[pointer]);
    let keyboard = app.client.new_id();
    app.client.request(seat, 1, &[keyboard]);
    sync(server);
    events(&mut app.client);
    (seat, pointer)
}
fn focus(server: &mut Server, app: &App) {
    let surface = server.state.surface_for_window(app.window).unwrap();
    server
        .state
        .set_keyboard_focus(Some(surface.clone()), SERIAL_COUNTER.next_serial());
    let pointer = server.state.seat.get_pointer().unwrap();
    pointer.motion(
        &mut server.state,
        Some((surface, (0.0, 0.0).into())),
        &MotionEvent {
            location: (5.0, 5.0).into(),
            serial: SERIAL_COUNTER.next_serial(),
            time: InputTime::now(),
        },
    );
    pointer.frame(&mut server.state);
    sync(server);
}
fn pointer_events(app: &mut App, pointer: u32) -> Vec<(u16, Vec<u32>)> {
    events(&mut app.client)
        .into_iter()
        .filter(|e| e.sender == pointer)
        .map(|e| (e.opcode, e.args))
        .collect()
}
struct Fixture {
    server: Server,
    app: App,
    seat: u32,
    pointer: u32,
    other: App,
    other_pointer: u32,
    capture: Capture,
    lease: u32,
    epoch: u32,
}
impl Fixture {
    fn new() -> Self {
        Self::new_with_scroll(7, false)
    }
    fn new_with_scroll(version: u32, scroll: bool) -> Self {
        let mut server = Server::new();
        server.state.image_capture_pending =
            Some(init_image_copy_capture(&server.display.handle()));
        let mut app = App::new(&mut server);
        let (seat, pointer) = bind_pointer_version(&mut server, &mut app, version);
        let mut capture = Capture::new(&mut server);
        let mut other = App::with_color(&mut server, [255, 0, 0, 255]);
        let (_, other_pointer) = bind_pointer(&mut server, &mut other);
        focus(&mut server, &app);
        events(&mut app.client);
        events(&mut other.client);
        let manager = capture.client.bind("jwm_remote_pointer_manager_v1", 2);
        let lease = capture.client.new_id();
        capture.client.request(
            manager,
            if scroll { 2 } else { 1 },
            &[lease, capture.source],
        );
        sync(&mut server);
        let grant = events(&mut capture.client)
            .into_iter()
            .find(|e| e.sender == lease && e.opcode == 0)
            .expect("native pointer grant");
        assert_eq!(grant.args[1], 30_000);
        Self {
            server,
            app,
            seat,
            pointer,
            other,
            other_pointer,
            capture,
            lease,
            epoch: grant.args[0],
        }
    }
    fn frame(&mut self, gl: &mut HeadlessSmithayGles) -> u32 {
        let (buffer, _file) =
            shm_buffer_file(&mut self.server, &mut self.capture.client, [0x5a; 4]);
        // Keep the frame resource alive for the action proof. A second ready
        // proof uses another session on the SAME source, not DuplicateFrame.
        let session = self.capture.client.new_id();
        self.capture
            .client
            .request(self.capture.manager, 0, &[session, self.capture.source, 0]);
        let frame = self.capture.client.new_id();
        self.capture.client.request(session, 0, &[frame]);
        self.capture.client.request(frame, 1, &[buffer]);
        self.capture.client.request(frame, 3, &[]);
        sync(&mut self.server);
        let queue = self
            .server
            .state
            .image_capture_pending
            .as_ref()
            .unwrap()
            .clone();
        fulfill_test_toplevel_capture(&mut gl.renderer, &mut None, &self.server.state, &queue);
        sync(&mut self.server);
        assert!(
            events(&mut self.capture.client)
                .iter()
                .any(|e| e.sender == frame && e.opcode == 3),
            "actual production ready"
        );
        frame
    }
    fn action(&mut self, sequence: u32, frame: u32, kind: u32, x: u32, y: u32) -> Vec<Event> {
        self.capture
            .client
            .request(self.lease, 1, &[self.epoch, sequence, frame, kind, x, y]);
        sync(&mut self.server);
        events(&mut self.capture.client)
    }
    fn accepted(&self, ev: &[Event], seq: u32) {
        assert!(
            ev.iter()
                .any(|e| e.sender == self.lease && e.opcode == 1 && e.args == [self.epoch, seq])
        );
    }
    fn denied(&mut self, ev: &[Event]) {
        assert!(
            ev.iter().any(|e| e.sender == self.lease && e.opcode == 2),
            "explicit revocation required"
        );
        assert!(pointer_events(&mut self.app, self.pointer).is_empty());
        assert!(pointer_events(&mut self.other, self.other_pointer).is_empty());
    }
}
#[test]
fn wire_pointer_motion_and_atomic_click_only_original_recipient_without_seat_mutation() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new();
    let pointer = f.server.state.seat.get_pointer().unwrap();
    let before = pointer.current_location();
    let focus = pointer.current_focus();
    let frame = f.frame(&mut gl);
    let ev = f.action(1, frame, 0, 40, 30);
    f.accepted(&ev, 1);
    let ev = pointer_events(&mut f.app, f.pointer);
    assert!(
        ev.iter()
            .any(|(op, args)| *op == 2 && args[1..] == [40 * 256, 30 * 256])
    );
    assert!(
        ev.iter().all(|(op, _)| matches!(op, 2 | 5)),
        "no enter/leave/button/axis on motion"
    );
    assert!(pointer_events(&mut f.other, f.other_pointer).is_empty());
    let frame = f.frame(&mut gl);
    let ev = f.action(2, frame, 1, 41, 31);
    f.accepted(&ev, 2);
    let ev = pointer_events(&mut f.app, f.pointer);
    let edges: Vec<_> = ev
        .iter()
        .filter(|(op, _)| *op == 3)
        .map(|(_, a)| (a[2], a[3]))
        .collect();
    assert_eq!(edges, [(0x110, 1), (0x110, 0)]);
    assert!(ev.iter().all(|(op, _)| matches!(op, 2 | 3 | 5)));
    assert_eq!(ev.last().map(|e| e.0), Some(5));
    assert!(pointer_events(&mut f.other, f.other_pointer).is_empty());
    assert_eq!(pointer.current_location(), before);
    assert_eq!(pointer.current_focus(), focus);
    assert!(!pointer.is_grabbed());
}
#[test]
fn wire_pointer_requires_new_ready_after_each_action_not_prequeued_frames() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for prequeued in [false, true] {
        let mut f = Fixture::new();
        let first = f.frame(&mut gl);
        let second = if prequeued { f.frame(&mut gl) } else { first };
        let ev = f.action(1, first, 0, 20, 20);
        f.accepted(&ev, 1);
        pointer_events(&mut f.app, f.pointer);
        let ev = f.action(2, second, 1, 20, 20);
        f.denied(&ev);
    }
}
#[test]
fn wire_pointer_invalid_sequence_kind_and_half_open_coordinates_fail_closed() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for (seq, kind, x, y) in [
        (2, 0, 10, 10),
        (1, 2, 10, 10),
        (1, 0, 800, 10),
        (1, 0, 10, 600),
        (1, 0, u32::MAX, 10),
    ] {
        let mut f = Fixture::new();
        let frame = f.frame(&mut gl);
        let ev = f.action(seq, frame, kind, x, y);
        f.denied(&ev);
        let ev = f.action(1, frame, 1, 10, 10);
        assert!(!ev.iter().any(|e| e.sender == f.lease && e.opcode == 1));
        assert!(pointer_events(&mut f.app, f.pointer).is_empty());
    }
}
#[test]
fn wire_pointer_unready_frame_cannot_authorize_action() {
    let Some(_gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new();
    let frame = f.capture.client.new_id();
    f.capture.client.request(f.capture.session, 0, &[frame]);
    sync(&mut f.server);
    let ev = f.action(1, frame, 1, 10, 10);
    f.denied(&ev);
}
#[test]
fn wire_pointer_focus_aba_and_new_receiver_revoke_original_lease() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for new_receiver in [false, true] {
        let mut f = Fixture::new();
        let frame = f.frame(&mut gl);
        let extra = if new_receiver {
            Some(bind_pointer(&mut f.server, &mut f.app).1)
        } else {
            focus(&mut f.server, &f.other);
            focus(&mut f.server, &f.app);
            None
        };
        events(&mut f.app.client);
        events(&mut f.other.client);
        let ev = f.action(1, frame, 1, 10, 10);
        assert!(ev.iter().any(|e| e.sender == f.lease && e.opcode == 2));
        let ev = events(&mut f.app.client);
        assert!(
            ev.iter()
                .all(|e| e.sender != f.pointer && Some(e.sender) != extra)
        );
        assert!(pointer_events(&mut f.other, f.other_pointer).is_empty());
    }
}
#[test]
fn wire_pointer_remote_click_serial_does_not_authorize_wm_move_resize_or_dnd() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new();
    let frame = f.frame(&mut gl);
    let ev = f.action(1, frame, 1, 20, 20);
    f.accepted(&ev, 1);
    let ev = pointer_events(&mut f.app, f.pointer);
    let serial = ev.iter().find(|(op, a)| *op == 3 && a[3] == 1).unwrap().1[0];
    f.server.state.pending_events.lock().unwrap().clear();
    f.app.client.request(f.app.toplevel, 5, &[f.seat, serial]); // xdg move
    f.app
        .client
        .request(f.app.toplevel, 6, &[f.seat, serial, 8]); // xdg resize right
    let manager = f.app.client.bind("wl_data_device_manager", 3);
    let device = f.app.client.new_id();
    f.app.client.request(manager, 1, &[device, f.seat]);
    f.app
        .client
        .request(device, 0, &[0, f.app.surface, 0, serial]);
    sync(&mut f.server);
    events(&mut f.app.client);
    assert!(!f.server.state.seat.get_pointer().unwrap().is_grabbed());
    assert!(
        f.server
            .state
            .pending_events
            .lock()
            .unwrap()
            .iter()
            .all(|e| !matches!(
                e,
                crate::backend::api::BackendEvent::MoveResizeRequest { .. }
            ))
    );
}
#[test]
fn wire_pointer_lifecycle_and_coordinate_layout_aba_never_revive_lease() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for case in 0..4 {
        let mut f = Fixture::new();
        let frame = f.frame(&mut gl);
        let mut locker = None;
        match case {
            0 => {
                f.app.unmap(&mut f.server);
                f.app.remap(&mut f.server);
            }
            1 => {
                f.server.state.session_lock_supported = true;
                let mut client = f.server.connect();
                let manager = client.bind("ext_session_lock_manager_v1", 1);
                let lock = client.new_id();
                client.request(manager, 1, &[lock]);
                sync(&mut f.server);
                assert!(f.server.state.session_locked);
                client.request(lock, 2, &[]);
                sync(&mut f.server);
                assert!(!f.server.state.session_locked);
                events(&mut client);
                locker = Some(client);
            }
            2 => {
                f.app.client.request(f.app.surface, 8, &[2]);
                f.app.client.request(f.app.surface, 6, &[]);
                sync(&mut f.server);
                f.app.client.request(f.app.surface, 8, &[1]);
                f.app.client.request(f.app.surface, 6, &[]);
                sync(&mut f.server);
            }
            _ => {
                let compositor = f.app.client.bind("wl_compositor", 6);
                let region = f.app.client.new_id();
                f.app.client.request(compositor, 1, &[region]);
                f.app.client.request(f.app.surface, 5, &[region]);
                f.app.client.request(f.app.surface, 6, &[]);
                sync(&mut f.server);
                f.app.client.request(f.app.surface, 5, &[0]);
                f.app.client.request(f.app.surface, 6, &[]);
                sync(&mut f.server);
            }
        }
        // Restoring focus/layout is not a new explicit local authorization.
        focus(&mut f.server, &f.app);
        events(&mut f.app.client);
        events(&mut f.other.client);
        let ev = f.action(1, frame, 1, 10, 10);
        f.denied(&ev);
        drop(locker);
    }
}
#[test]
fn wire_pointer_remote_click_serial_cannot_authorize_popup_grab() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new();
    let frame = f.frame(&mut gl);
    let ev = f.action(1, frame, 1, 20, 20);
    f.accepted(&ev, 1);
    let ev = pointer_events(&mut f.app, f.pointer);
    let serial = ev.iter().find(|(op, a)| *op == 3 && a[3] == 1).unwrap().1[0];
    let compositor = f.app.client.bind("wl_compositor", 6);
    let shell = f.app.client.bind("xdg_wm_base", 6);
    let positioner = f.app.client.new_id();
    f.app.client.request(shell, 1, &[positioner]);
    f.app.client.request(positioner, 1, &[40, 40]); // size
    f.app.client.request(positioner, 2, &[0, 0, 40, 40]); // anchor rectangle
    let surface = f.app.client.new_id();
    f.app.client.request(compositor, 0, &[surface]);
    let xdg = f.app.client.new_id();
    f.app.client.request(shell, 2, &[xdg, surface]);
    let popup = f.app.client.new_id();
    f.app
        .client
        .request(xdg, 2, &[popup, f.app.xdg, positioner]);
    f.app.client.request(popup, 1, &[f.seat, serial]);
    // Smithay defers requested_grab until the popup's first surface commit.
    // Commit without a buffer (before any configure acknowledgement/mapping)
    // so this exercises the real JWM grab policy, not merely the wire parser.
    f.app.client.request(surface, 6, &[]);
    sync(&mut f.server);
    let ev = events(&mut f.app.client);
    assert!(
        ev.iter().any(|e| e.sender == popup && e.opcode == 1),
        "unauthorized popup receives done"
    );
    assert!(!f.server.state.seat.get_pointer().unwrap().is_grabbed());
    assert!(!f.server.state.seat.get_keyboard().unwrap().is_grabbed());
    assert_eq!(
        f.server.state.seat.get_keyboard().unwrap().current_focus(),
        f.server.state.surface_for_window(f.app.window)
    );
    assert!(pointer_events(&mut f.other, f.other_pointer).is_empty());
}
#[test]
fn wire_pointer_ready_from_different_source_instance_cannot_authorize_action() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new();
    let original = f.capture.source;
    let different = f.capture.client.new_id();
    f.capture
        .client
        .request(f.capture.sources, 0, &[different, f.capture.handle]);
    sync(&mut f.server);
    f.capture.source = different;
    let frame = f.frame(&mut gl);
    f.capture.source = original;
    let ev = f.action(1, frame, 1, 10, 10);
    f.denied(&ev);
}
#[test]
fn wire_pointer_same_client_other_surface_is_not_the_authorized_target() {
    use smithay::reexports::wayland_server::Resource;
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new();
    let frame = f.frame(&mut gl);
    let compositor = f.app.client.bind("wl_compositor", 6);
    let id = f.app.client.new_id();
    f.app.client.request(compositor, 0, &[id]);
    sync(&mut f.server);
    let original = f.server.state.surface_for_window(f.app.window).unwrap();
    let second = original
        .client()
        .unwrap()
        .object_from_protocol_id(&f.server.display.handle(), id)
        .unwrap();
    let pointer = f.server.state.seat.get_pointer().unwrap();
    pointer.motion(
        &mut f.server.state,
        Some((second, (0.0, 0.0).into())),
        &MotionEvent {
            location: (5.0, 5.0).into(),
            serial: SERIAL_COUNTER.next_serial(),
            time: InputTime::now(),
        },
    );
    pointer.frame(&mut f.server.state);
    sync(&mut f.server);
    events(&mut f.app.client);
    let ev = f.action(1, frame, 1, 10, 10);
    f.denied(&ev);
}
#[test]
fn wire_pointer_wrong_epoch_is_rejected_and_does_not_deliver_motion() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new();
    let frame = f.frame(&mut gl);
    f.epoch = f.epoch.checked_add(1).unwrap();
    let ev = f.action(1, frame, 0, 10, 10);
    f.denied(&ev);
}
#[test]
fn wire_pointer_real_timer_expires_lease_and_original_source_cannot_renew() {
    use std::time::{Duration, Instant};
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new();
    let frame = f.frame(&mut gl);
    super::expire_for_test(&mut f.server.state);
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut revoked = false;
    while Instant::now() < deadline && !revoked {
        f.server.dispatch_timers(Duration::from_millis(100));
        revoked = events(&mut f.capture.client)
            .iter()
            .any(|e| e.sender == f.lease && e.opcode == 2);
    }
    assert!(
        revoked,
        "production timer must revoke without a new action request"
    );
    let ev = f.action(1, frame, 1, 10, 10);
    assert!(!ev.iter().any(|e| e.sender == f.lease && e.opcode == 1));
    assert!(pointer_events(&mut f.app, f.pointer).is_empty());
    let manager = f.capture.client.bind("jwm_remote_pointer_manager_v1", 1);
    let replacement = f.capture.client.new_id();
    f.capture
        .client
        .request(manager, 1, &[replacement, f.capture.source]);
    sync(&mut f.server);
    let ev = events(&mut f.capture.client);
    assert!(ev.iter().any(|e| e.sender == replacement && e.opcode == 2));
    assert!(!ev.iter().any(|e| e.sender == replacement && e.opcode == 0));
    assert!(pointer_events(&mut f.other, f.other_pointer).is_empty());
}
#[test]
fn wire_pointer_destroyed_source_revokes_even_with_live_ready_frame_and_session() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new();
    let frame = f.frame(&mut gl);
    f.capture.client.request(f.capture.source, 0, &[]);
    sync(&mut f.server);
    let ev = events(&mut f.capture.client);
    assert!(
        ev.iter().any(|e| e.sender == f.lease && e.opcode == 2),
        "source destroy actively revokes"
    );
    let ev = f.action(1, frame, 1, 10, 10);
    assert!(!ev.iter().any(|e| e.sender == f.lease && e.opcode == 1));
    assert!(pointer_events(&mut f.app, f.pointer).is_empty());
    assert!(pointer_events(&mut f.other, f.other_pointer).is_empty());
}
#[test]
fn wire_pointer_constraint_creation_and_destruction_cannot_restore_lease() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new();
    let frame = f.frame(&mut gl);
    let manager = f.app.client.bind("zwp_pointer_constraints_v1", 1);
    let locked = f.app.client.new_id();
    f.app
        .client
        .request(manager, 1, &[locked, f.app.surface, f.pointer, 0, 1]);
    sync(&mut f.server);
    f.app.client.request(locked, 0, &[]);
    sync(&mut f.server);
    events(&mut f.app.client);
    let ev = f.action(1, frame, 1, 10, 10);
    f.denied(&ev);
}

#[path = "remote_pointer_scroll_tests.rs"]
mod scroll_tests;
