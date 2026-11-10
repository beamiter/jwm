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
fn bind_pointer(server: &mut Server, app: &mut App) -> (u32, u32, u32) {
    bind_pointer_version(server, app, 7)
}
fn bind_pointer_version(server: &mut Server, app: &mut App, version: u32) -> (u32, u32, u32) {
    let seat = app.client.bind("wl_seat", version);
    let pointer = app.client.new_id();
    app.client.request(seat, 0, &[pointer]);
    let keyboard = app.client.new_id();
    app.client.request(seat, 1, &[keyboard]);
    sync(server);
    events(&mut app.client);
    (seat, pointer, keyboard)
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
    keyboard: u32,
    pointer: u32,
    other: App,
    other_pointer: u32,
    other_keyboard: u32,
    capture: Capture,
    lease: u32,
    epoch: u32,
}
impl Fixture {
    fn new() -> Self {
        Self::with_scroll(7, 1, false)
    }
    fn with_scroll(version: u32, manager_version: u32, scroll: bool) -> Self {
        let mut server = Server::new();
        server.state.image_capture_pending =
            Some(init_image_copy_capture(&server.display.handle()));
        let mut app = App::new(&mut server);
        let (_, pointer, keyboard) = bind_pointer_version(&mut server, &mut app, version);
        let mut capture = Capture::new(&mut server);
        let mut other = App::with_color(&mut server, [255, 0, 0, 255]);
        let (_, other_pointer, other_keyboard) = bind_pointer(&mut server, &mut other);
        focus(&mut server, &app);
        events(&mut app.client);
        events(&mut other.client);
        let manager = capture
            .client
            .bind("jwm_remote_control_manager_v1", manager_version);
        let lease = capture.client.new_id();
        capture.client.request(
            manager,
            if scroll { 2 } else { 1 },
            &[lease, capture.source],
        );
        sync(&mut server);
        let grant = events(&mut capture.client)
            .into_iter()
            .find(|e| e.sender == lease && e.opcode == 1)
            .expect("native control grant");
        assert_eq!(grant.args[2], 30_000);
        Self {
            server,
            app,
            keyboard,
            pointer,
            other,
            other_pointer,
            other_keyboard,
            capture,
            lease,
            epoch: grant.args[1],
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
            .request(self.lease, 3, &[self.epoch, sequence, frame, kind, x, y]);
        sync(&mut self.server);
        events(&mut self.capture.client)
    }
    fn accepted(&self, ev: &[Event], seq: u32) {
        assert!(
            ev.iter()
                .any(|e| e.sender == self.lease && e.opcode == 3 && e.args == [self.epoch, seq])
        );
    }
    fn denied(&mut self, ev: &[Event]) {
        assert!(
            ev.iter().any(|e| e.sender == self.lease && e.opcode == 4),
            "explicit revocation required"
        );
        let target = events(&mut self.app.client);
        assert!(
            !target
                .iter()
                .any(|e| (e.sender == self.keyboard && e.opcode == 3)
                    || (e.sender == self.pointer && matches!(e.opcode, 2..=4))),
            "revoked target input: {:?}",
            target
                .iter()
                .map(|e| (e.sender, e.opcode, &e.args))
                .collect::<Vec<_>>()
        );
        self.other_empty();
    }
}

impl Fixture {
    fn arm(&mut self, frame: u32) {
        self.capture
            .client
            .request(self.lease, 1, &[self.epoch, frame]);
        sync(&mut self.server);
        assert!(
            events(&mut self.capture.client)
                .iter()
                .any(|e| e.sender == self.lease && e.opcode == 2)
        );
    }
    fn key(&mut self, seq: u32, key: u32, down: bool) -> Vec<Event> {
        self.capture
            .client
            .request(self.lease, 2, &[self.epoch, seq, key, u32::from(down)]);
        sync(&mut self.server);
        events(&mut self.capture.client)
    }
    fn target_keys(&mut self) -> Vec<(u32, u32)> {
        events(&mut self.app.client)
            .into_iter()
            .filter(|e| e.sender == self.keyboard && e.opcode == 3)
            .map(|e| (e.args[2], e.args[3]))
            .collect()
    }
    fn other_empty(&mut self) {
        let e = events(&mut self.other.client);
        assert!(
            !e.iter()
                .any(|e| e.sender == self.other_keyboard || e.sender == self.other_pointer),
            "other receiver leaked input: {:?}",
            e.iter()
                .map(|e| (e.sender, e.opcode, &e.args))
                .collect::<Vec<_>>()
        );
    }
}
#[test]
fn wire_control_click_type_shift_a_share_one_sequence_and_exact_target() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new();
    let frame = f.frame(&mut gl);
    f.arm(frame);
    let ev = f.action(1, frame, 1, 40, 30);
    f.accepted(&ev, 1);
    let pointer = pointer_events(&mut f.app, f.pointer);
    assert_eq!(
        pointer.iter().map(|e| e.0).collect::<Vec<_>>(),
        [2, 3, 3, 5]
    );
    assert_eq!(&pointer[0].1[1..], &[40 * 256, 30 * 256]);
    assert_eq!(&pointer[1].1[2..], &[0x110, 1]);
    assert_eq!(&pointer[2].1[2..], &[0x110, 0]);
    f.other_empty();
    for (seq, key, down) in [
        (2, 30, true),
        (3, 30, false),
        (4, 42, true),
        (5, 30, true),
        (6, 30, false),
        (7, 42, false),
    ] {
        let ev = f.key(seq, key, down);
        f.accepted(&ev, seq);
        assert_eq!(f.target_keys(), [(key, u32::from(down))]);
        f.other_empty();
    }
    let frame = f.frame(&mut gl);
    let ev = f.action(8, frame, 0, 41, 31);
    f.accepted(&ev, 8);
    assert!(
        pointer_events(&mut f.app, f.pointer)
            .iter()
            .any(|e| e.0 == 2)
    );
    f.other_empty();
    assert!(
        f.server
            .state
            .seat
            .get_keyboard()
            .unwrap()
            .pressed_keys()
            .is_empty()
    );
}
#[test]
fn wire_control_held_key_pointer_revokes_and_releases_only_original_keys() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new();
    let frame = f.frame(&mut gl);
    f.arm(frame);
    let ev = f.key(1, 30, true);
    f.accepted(&ev, 1);
    assert_eq!(f.target_keys(), [(30, 1)]);
    let frame = f.frame(&mut gl);
    let ev = f.action(2, frame, 1, 40, 30);
    assert!(ev.iter().any(|e| e.sender == f.lease && e.opcode == 4));
    let target = events(&mut f.app.client);
    assert_eq!(
        target
            .iter()
            .filter(|e| e.sender == f.keyboard && e.opcode == 3)
            .map(|e| (e.args[2], e.args[3]))
            .collect::<Vec<_>>(),
        [(30, 0)]
    );
    assert!(!target.iter().any(|e| e.sender == f.pointer));
    f.other_empty();
    let ev = f.key(3, 30, true);
    assert!(!ev.iter().any(|e| e.sender == f.lease && e.opcode == 3));
    assert!(f.target_keys().is_empty());
    f.other_empty();
}
#[test]
fn wire_control_replay_old_frame_and_wrong_epoch_cannot_deliver() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for case in 0..3 {
        let mut f = Fixture::new();
        let frame = f.frame(&mut gl);
        f.arm(frame);
        let ev = f.key(1, 30, true);
        f.accepted(&ev, 1);
        assert_eq!(f.target_keys(), [(30, 1)]);
        let ev = f.key(2, 30, false);
        f.accepted(&ev, 2);
        assert_eq!(f.target_keys(), [(30, 0)]);
        let ev = match case {
            0 => f.key(2, 30, true),
            1 => f.action(3, frame, 0, 40, 30),
            _ => {
                f.epoch += 1;
                f.key(3, 30, true)
            }
        };
        f.denied(&ev);
        f.other_empty();
    }
}
#[test]
fn wire_control_focus_unmap_lock_source_lifecycle_aba_never_revives() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for case in 0..4 {
        let mut f = Fixture::new();
        let frame = f.frame(&mut gl);
        f.arm(frame);
        let mut locker = None;
        match case {
            0 => {
                focus(&mut f.server, &f.other);
                focus(&mut f.server, &f.app);
            }
            1 => {
                f.app.unmap(&mut f.server);
                f.app.remap(&mut f.server);
            }
            2 => {
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
                locker = Some(client);
            }
            _ => {
                f.capture.client.request(f.capture.source, 0, &[]);
                sync(&mut f.server);
            }
        }
        focus(&mut f.server, &f.app);
        events(&mut f.app.client);
        events(&mut f.other.client);
        let ev = f.action(1, frame, 1, 40, 30);
        f.denied(&ev);
        let ev = f.key(2, 30, true);
        assert!(!ev.iter().any(|e| e.sender == f.lease && e.opcode == 3));
        assert!(f.target_keys().is_empty());
        f.other_empty();
        drop(locker);
    }
}
#[test]
fn wire_control_same_authority_rejects_second_bundle_and_legacy_grants() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new();
    let frame = f.frame(&mut gl);
    f.arm(frame);
    for (name, revoked, granted) in [
        ("jwm_remote_control_manager_v1", 4, 1),
        ("jwm_remote_keyboard_manager_v1", 4, 1),
        ("jwm_remote_pointer_manager_v1", 2, 0),
    ] {
        let manager = f.capture.client.bind(name, 1);
        let lease = f.capture.client.new_id();
        f.capture
            .client
            .request(manager, 1, &[lease, f.capture.source]);
        sync(&mut f.server);
        let ev = events(&mut f.capture.client);
        assert!(
            ev.iter().any(|e| e.sender == lease && e.opcode == revoked),
            "{name} missing explicit denial"
        );
        assert!(
            !ev.iter().any(|e| e.sender == lease && e.opcode == granted),
            "{name} wrongly granted"
        );
        assert!(f.target_keys().is_empty());
        f.other_empty();
    }
}
#[test]
fn wire_control_timer_and_local_takeover_cleanup_without_new_request() {
    use std::time::{Duration, Instant};
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for expired in [false, true] {
        let mut f = Fixture::new();
        let frame = f.frame(&mut gl);
        f.arm(frame);
        let ev = f.key(1, 42, true);
        f.accepted(&ev, 1);
        assert_eq!(f.target_keys(), [(42, 1)]);
        if expired {
            f.server
                .state
                .remote_control
                .active
                .as_mut()
                .unwrap()
                .created = Instant::now() - super::TTL;
            let end = Instant::now() + Duration::from_secs(1);
            while super::active(&f.server.state) && Instant::now() < end {
                f.server.dispatch_timers(Duration::from_millis(100));
            }
        } else {
            super::revoke(&mut f.server.state, super::SEAT);
        }
        sync(&mut f.server);
        assert!(!super::active(&f.server.state));
        assert!(
            events(&mut f.capture.client)
                .iter()
                .any(|e| e.sender == f.lease && e.opcode == 4)
        );
        assert_eq!(f.target_keys(), [(42, 0)]);
        f.other_empty();
    }
}
#[test]
fn wire_control_disconnect_releases_original_held_key_and_stops_pointer() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new();
    let frame = f.frame(&mut gl);
    f.arm(frame);
    let ev = f.key(1, 30, true);
    f.accepted(&ev, 1);
    assert_eq!(f.target_keys(), [(30, 1)]);
    let Fixture {
        mut server,
        mut app,
        keyboard,
        pointer,
        mut other,
        other_keyboard,
        other_pointer,
        capture,
        ..
    } = f;
    drop(capture);
    sync(&mut server);
    assert!(!super::active(&server.state));
    let e = events(&mut app.client);
    assert_eq!(
        e.iter()
            .filter(|e| e.sender == keyboard && e.opcode == 3)
            .map(|e| (e.args[2], e.args[3]))
            .collect::<Vec<_>>(),
        [(30, 0)]
    );
    assert!(!e.iter().any(|e| e.sender == pointer));
    assert!(
        !events(&mut other.client)
            .iter()
            .any(|e| e.sender == other_pointer || e.sender == other_keyboard)
    );
}
#[test]
fn wire_control_different_capture_source_frame_is_not_a_ticket() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new();
    let valid = f.frame(&mut gl);
    f.arm(valid);
    let original = f.capture.source;
    let source = f.capture.client.new_id();
    f.capture
        .client
        .request(f.capture.sources, 0, &[source, f.capture.handle]);
    sync(&mut f.server);
    f.capture.source = source;
    let frame = f.frame(&mut gl);
    f.capture.source = original;
    let ev = f.action(1, frame, 1, 40, 30);
    f.denied(&ev);
}
#[test]
fn wire_control_legacy_first_and_consumed_source_cannot_join_bundle() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for (legacy, grant_opcode) in [
        ("jwm_remote_keyboard_manager_v1", 1),
        ("jwm_remote_pointer_manager_v1", 0),
    ] {
        let mut f = Fixture::new();
        let frame = f.frame(&mut gl);
        f.arm(frame);
        f.capture.client.request(f.lease, 0, &[]);
        sync(&mut f.server);
        assert!(!super::active(&f.server.state));
        events(&mut f.capture.client);
        let manager = f.capture.client.bind("jwm_remote_control_manager_v1", 1);
        let replacement = f.capture.client.new_id();
        f.capture
            .client
            .request(manager, 1, &[replacement, f.capture.source]);
        sync(&mut f.server);
        let ev = events(&mut f.capture.client);
        assert!(ev.iter().any(|e| e.sender == replacement && e.opcode == 4));
        assert!(!ev.iter().any(|e| e.sender == replacement && e.opcode == 1));
        let fresh = f.capture.client.new_id();
        f.capture
            .client
            .request(f.capture.sources, 0, &[fresh, f.capture.handle]);
        sync(&mut f.server);
        let old_manager = f.capture.client.bind(legacy, 1);
        let old = f.capture.client.new_id();
        f.capture.client.request(old_manager, 1, &[old, fresh]);
        sync(&mut f.server);
        assert!(
            events(&mut f.capture.client)
                .iter()
                .any(|e| e.sender == old && e.opcode == grant_opcode),
            "fresh source positive {legacy}"
        );
        let bundle = f.capture.client.new_id();
        f.capture.client.request(manager, 1, &[bundle, fresh]);
        sync(&mut f.server);
        let ev = events(&mut f.capture.client);
        assert!(ev.iter().any(|e| e.sender == bundle && e.opcode == 4));
        assert!(!ev.iter().any(|e| e.sender == bundle && e.opcode == 1));
        assert!(f.target_keys().is_empty());
        f.other_empty();
    }
}

#[test]
fn wire_control_scroll_explicit_grant_versions_and_both_directions() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for version in [4, 5, 7, 8, 9] {
        let mut f = Fixture::with_scroll(version, 2, true);
        let pointer = f.server.state.seat.get_pointer().unwrap();
        let keyboard = f.server.state.seat.get_keyboard().unwrap();
        let old_location = pointer.current_location();
        let old_focus = pointer.current_focus();
        let old_keyboard_focus = keyboard.current_focus();
        let frame = f.frame(&mut gl);
        f.arm(frame);
        for (sequence, kind, direction) in [(1, 2, -1i32), (2, 3, 1i32)] {
            let frame = if sequence == 1 {
                frame
            } else {
                f.frame(&mut gl)
            };
            let result = f.action(sequence, frame, kind, 40, 30);
            f.accepted(&result, sequence);
            // Inspect the entire target stream before filtering: scroll must not
            // manufacture keyboard edges or extra pointer enter/button events.
            let events = events(&mut f.app.client);
            assert!(!events.iter().any(|e| e.sender == f.keyboard));
            let delivered: Vec<_> = events.iter().filter(|e| e.sender == f.pointer).collect();
            let expected: &[u16] = if version >= 8 {
                &[2, 6, 9, 4, 5]
            } else if version >= 5 {
                &[2, 6, 8, 4, 5]
            } else {
                &[2, 4]
            };
            assert_eq!(
                delivered.iter().map(|e| e.opcode).collect::<Vec<_>>(),
                expected,
                "receiver version {version}"
            );
            assert_eq!(delivered[0].args[1..], [40 * 256, 30 * 256]);
            if version >= 5 {
                assert_eq!(delivered[1].args, [0]);
                assert_eq!(
                    delivered[2].args,
                    [0, (direction * if version >= 8 { 120 } else { 1 }) as u32]
                );
                assert!(delivered.last().unwrap().args.is_empty());
            }
            let axis = delivered.iter().find(|e| e.opcode == 4).unwrap();
            assert_eq!(axis.args.len(), 3);
            assert_eq!(axis.args[1..], [0, (direction * 10 * 256) as u32]);
            f.other_empty();
            assert_eq!(pointer.current_location(), old_location);
            assert_eq!(pointer.current_focus(), old_focus);
            assert_eq!(keyboard.current_focus(), old_keyboard_focus);
            assert!(!pointer.is_grabbed());
            assert!(!keyboard.is_grabbed());
            assert!(keyboard.pressed_keys().is_empty());
        }
    }
}

#[test]
fn wire_control_scroll_v1_and_v2_ordinary_create_never_authorize_axis() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for manager in [1, 2] {
        for kind in [2, 3] {
            let mut f = Fixture::with_scroll(9, manager, false);
            let frame = f.frame(&mut gl);
            f.arm(frame);
            let result = f.action(1, frame, kind, 40, 30);
            f.denied(&result);
            let late = f.key(2, 30, true);
            assert!(!late.iter().any(|e| e.sender == f.lease && e.opcode == 3));
            assert!(f.target_keys().is_empty());
            f.other_empty();
        }
    }
}

#[test]
fn wire_control_scroll_held_shift_revokes_and_cleans_only_original_keys() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for kind in [2, 3] {
        let mut f = Fixture::with_scroll(9, 2, true);
        let frame = f.frame(&mut gl);
        f.arm(frame);
        let result = f.key(1, 42, true);
        f.accepted(&result, 1);
        assert_eq!(f.target_keys(), [(42, 1)]);
        let frame = f.frame(&mut gl);
        let result = f.action(2, frame, kind, 40, 30);
        assert!(result.iter().any(|e| e.sender == f.lease && e.opcode == 4));
        let target = events(&mut f.app.client);
        assert_eq!(
            target
                .iter()
                .filter(|e| e.sender == f.keyboard && e.opcode == 3)
                .map(|e| (e.args[2], e.args[3]))
                .collect::<Vec<_>>(),
            [(42, 0)]
        );
        assert!(!target.iter().any(|e| e.sender == f.pointer));
        f.other_empty();
        assert!(!super::active(&f.server.state));
    }
}

#[test]
fn wire_control_scroll_replay_prequeued_and_prekey_frame_never_deliver() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for case in 0..4 {
        let mut f = Fixture::with_scroll(9, 2, true);
        let frame = f.frame(&mut gl);
        f.arm(frame);
        let queued = if case == 1 { f.frame(&mut gl) } else { frame };
        let (seq, proof) = if case == 3 {
            let result = f.key(1, 30, true);
            f.accepted(&result, 1);
            assert_eq!(f.target_keys(), [(30, 1)]);
            let result = f.key(2, 30, false);
            f.accepted(&result, 2);
            assert_eq!(f.target_keys(), [(30, 0)]);
            (3, frame)
        } else {
            let result = f.action(1, frame, 2, 40, 30);
            f.accepted(&result, 1);
            let delivered = pointer_events(&mut f.app, f.pointer);
            assert!(delivered.iter().any(|e| e.0 == 9));
            f.other_empty();
            (if case == 2 { 1 } else { 2 }, queued)
        };
        let result = f.action(seq, proof, 3, 40, 30);
        f.denied(&result);
        let again = f.action(seq + 1, proof, 2, 40, 30);
        assert!(!again.iter().any(|e| e.sender == f.lease && e.opcode == 3));
        assert!(pointer_events(&mut f.app, f.pointer).is_empty());
        f.other_empty();
    }
}

#[test]
fn wire_control_scroll_wrong_source_and_expired_frame_cannot_deliver() {
    use crate::backend::wayland_udev::image_copy_capture::CaptureFrameData;
    use smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1;
    use smithay::reexports::wayland_server::Resource;
    use std::time::{Duration, Instant};
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for expired in [false, true] {
        let mut f = Fixture::with_scroll(9, 2, true);
        let valid = f.frame(&mut gl);
        f.arm(valid);
        let original = f.capture.source;
        if !expired {
            let source = f.capture.client.new_id();
            f.capture
                .client
                .request(f.capture.sources, 0, &[source, f.capture.handle]);
            sync(&mut f.server);
            f.capture.source = source;
        }
        let frame = f.frame(&mut gl);
        f.capture.source = original;
        if expired {
            // Alter only the fixture timestamp of a genuinely rendered frame;
            // no fake ready bit or capture receipt is introduced.
            let lease = f.server.state.remote_control.active.as_ref().unwrap();
            let native: ExtImageCopyCaptureFrameV1 = lease
                .resource
                .client()
                .unwrap()
                .object_from_protocol_id(&f.server.display.handle(), frame)
                .unwrap();
            let mut stamp = native
                .data::<CaptureFrameData>()
                .unwrap()
                .ready_stamp
                .lock()
                .unwrap();
            let (serial, _) = stamp.unwrap();
            *stamp = Some((serial, Instant::now() - Duration::from_secs(2)));
        }
        let result = f.action(1, frame, 2, 40, 30);
        f.denied(&result);
    }
}
