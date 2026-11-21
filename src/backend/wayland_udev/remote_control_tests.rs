//! Real private Wayland wire requests, production GL capture and wl_pointer
//! recipients. Not physical input acquisition, DRM or presentation acceptance.
#![cfg(feature = "backend-wayland-udev")]
use crate::backend::wayland_udev::backend::fulfill_test_toplevel_capture;
use crate::backend::wayland_udev::compositor::headless_render::HeadlessSmithayGles;
use crate::backend::wayland_udev::image_copy_capture::{
    authority_tests::{App, Capture, sized_shm_buffer_file},
    init_image_copy_capture,
    wire_test_client::{Client, Event, Server},
};
use crate::backend::wayland_udev::remote_keyboard;
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
    capture_size: (u32, u32),
    lease: u32,
    epoch: u32,
}
struct LeaseRequest {
    opcode: u16,
    arguments: Vec<u32>,
    expected_ttl: Option<u32>,
}
impl Fixture {
    fn new() -> Self {
        Self::with_scroll(7, 1, false)
    }
    fn with_scroll(version: u32, manager_version: u32, scroll: bool) -> Self {
        Self::with_scroll_axes(version, manager_version, scroll, None)
    }
    fn with_scroll_axes(
        version: u32,
        manager_version: u32,
        scroll: bool,
        axes: Option<u32>,
    ) -> Self {
        Self::with_mode(version, manager_version, scroll, axes, false)
    }
    fn resumable() -> Self {
        Self::with_mode(7, 4, false, Some(0), true)
    }
    fn with_mode(
        version: u32,
        manager_version: u32,
        scroll: bool,
        axes: Option<u32>,
        resumable: bool,
    ) -> Self {
        Self::with_duration(version, manager_version, scroll, axes, resumable, None)
    }
    fn with_duration(
        version: u32,
        manager_version: u32,
        scroll: bool,
        axes: Option<u32>,
        resumable: bool,
        ttl_ms: Option<u32>,
    ) -> Self {
        let (opcode, arguments, allowed) = if let Some(ttl_ms) = ttl_ms {
            let axes = axes.unwrap_or(0);
            (
                5,
                vec![axes, ttl_ms],
                axes <= 3 && (1..=300_000).contains(&ttl_ms),
            )
        } else if resumable {
            let axes = axes.unwrap_or(0);
            (4, vec![axes], axes <= 3)
        } else if let Some(axes) = axes {
            (3, vec![axes], matches!(axes, 1..=3))
        } else {
            (if scroll { 2 } else { 1 }, Vec::new(), true)
        };
        let grant = LeaseRequest {
            opcode,
            arguments,
            expected_ttl: allowed.then_some(ttl_ms.unwrap_or(30_000)),
        };
        Self::create(version, manager_version, grant, None, |_, _| {})
    }
    fn gestures() -> Self {
        Self::gestures_with_crop(None)
    }
    fn gestures_with_crop(crop: Option<[u32; 4]>) -> Self {
        let grant = LeaseRequest {
            opcode: 6,
            arguments: vec![0],
            expected_ttl: Some(30_000),
        };
        Self::create(7, 6, grant, crop, |_, _| {})
    }
    fn create(
        version: u32,
        manager_version: u32,
        grant: LeaseRequest,
        crop: Option<[u32; 4]>,
        prepare: impl FnOnce(&mut Server, &mut App),
    ) -> Self {
        let mut server = Server::new();
        server.state.image_capture_pending =
            Some(init_image_copy_capture(&server.display.handle()));
        let mut app = App::new(&mut server);
        if let Some([x, y, width, height]) = crop {
            let root = server.state.window_geometry[&app.window];
            let (buffer, _file) = sized_shm_buffer_file(
                &mut server,
                &mut app.client,
                [0, 0, 255, 255],
                root.w,
                root.h,
            );
            app.client.request(app.surface, 8, &[2]); // 2× integer buffer scale
            app.client.request(app.surface, 1, &[buffer, 0, 0]);
            app.client.request(app.xdg, 3, &[x, y, width, height]);
            let compositor = app.client.bind("wl_compositor", 1);
            let region = app.client.new_id();
            app.client.request(compositor, 1, &[region]);
            app.client
                .request(region, 1, &[x + 10, y + 10, width - 10, height - 10]);
            app.client.request(app.surface, 5, &[region]);
            app.client.request(app.surface, 6, &[]);
            sync(&mut server);
            let geometry = server.state.window_geometry.get_mut(&app.window).unwrap();
            geometry.w = width;
            geometry.h = height;
        }
        prepare(&mut server, &mut app);
        let size = server.state.window_geometry[&app.window];
        let capture_size = (size.w, size.h);
        let (_, pointer, keyboard) = bind_pointer_version(&mut server, &mut app, version);
        let mut capture = Capture::new_with_size(&mut server, capture_size);
        let mut other = App::with_color(&mut server, [255, 0, 0, 255]);
        let (_, other_pointer, other_keyboard) = bind_pointer(&mut server, &mut other);
        focus(&mut server, &app);
        events(&mut app.client);
        events(&mut other.client);
        let manager = capture
            .client
            .bind("jwm_remote_control_manager_v1", manager_version);
        let lease = capture.client.new_id();
        let mut arguments = vec![lease, capture.source];
        arguments.extend(grant.arguments);
        capture.client.request(manager, grant.opcode, &arguments);

        sync(&mut server);
        let admission = events(&mut capture.client);
        let epoch = if let Some(ttl_ms) = grant.expected_ttl {
            let grant = admission
                .into_iter()
                .find(|e| e.sender == lease && e.opcode == 1)
                .expect("native control grant");
            assert_eq!(grant.args[2], ttl_ms);
            grant.args[1]
        } else {
            assert!(
                admission.iter().any(|e| e.sender == lease && e.opcode == 4),
                "refused lease must explicitly revoke"
            );
            assert!(
                !admission.iter().any(|e| e.sender == lease && e.opcode != 4),
                "refused lease cannot publish grant/keymap/armed"
            );
            0 // Inert test resource, never used for an authorized action.
        };
        Self {
            server,
            app,
            keyboard,
            pointer,
            other,
            other_pointer,
            other_keyboard,
            capture,
            capture_size,
            lease,
            epoch,
        }
    }
    fn frame(&mut self, gl: &mut HeadlessSmithayGles) -> u32 {
        let (buffer, _file) = sized_shm_buffer_file(
            &mut self.server,
            &mut self.capture.client,
            [0x5a; 4],
            self.capture_size.0,
            self.capture_size.1,
        );
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
    fn drag_motion(&mut self, sequence: u32, x: u32, y: u32) -> Vec<Event> {
        self.capture
            .client
            .request(self.lease, 6, &[self.epoch, sequence, x, y]);
        sync(&mut self.server);
        events(&mut self.capture.client)
    }
    fn drag_release(&mut self, sequence: u32) -> Vec<Event> {
        self.capture
            .client
            .request(self.lease, 7, &[self.epoch, sequence]);
        sync(&mut self.server);
        events(&mut self.capture.client)
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

#[test]
fn native_gesture_lease_keeps_modifier_and_drag_edges_on_one_surface() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::gestures();
    let first = f.frame(&mut gl);
    f.arm(first);

    let result = f.key(1, 42, true);
    f.accepted(&result, 1);
    let proof = f.frame(&mut gl);
    let result = f.action(2, proof, 1, 12, 13);
    f.accepted(&result, 2);
    let proof = f.frame(&mut gl);
    let result = f.action(3, proof, 6, 13, 14);
    f.accepted(&result, 3);
    let result = f.drag_motion(4, 18, 20);
    f.accepted(&result, 4);
    let result = f.key(5, 42, false);
    f.accepted(&result, 5);
    let result = f.drag_release(6);
    f.accepted(&result, 6);

    let input: Vec<_> = events(&mut f.app.client)
        .into_iter()
        .filter_map(|e| match (e.sender, e.opcode) {
            (id, 3) if id == f.keyboard => Some(("key", e.args[2], e.args[3])),
            (id, 2) if id == f.pointer => Some(("motion", e.args[1] / 256, e.args[2] / 256)),
            (id, 3) if id == f.pointer => Some(("button", e.args[2], e.args[3])),
            _ => None,
        })
        .collect();
    assert_eq!(
        input,
        [
            ("key", 42, 1),
            ("motion", 12, 13),
            ("button", 0x110, 1),
            ("button", 0x110, 0),
            ("motion", 13, 14),
            ("button", 0x110, 1),
            ("motion", 18, 20),
            ("key", 42, 0),
            ("button", 0x110, 0),
        ]
    );
    assert!(!f.server.state.seat.get_pointer().unwrap().is_grabbed());
    f.other_empty();
}

#[test]
fn native_gesture_cleanup_balances_original_receiver_before_revoked() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for focus_loss in [false, true] {
        let mut f = Fixture::gestures();
        let initial = f.frame(&mut gl);
        f.arm(initial);
        let proof = f.frame(&mut gl);
        let result = f.action(1, proof, 6, 13, 14);
        f.accepted(&result, 1);
        assert!(
            pointer_events(&mut f.app, f.pointer)
                .iter()
                .any(|(opcode, args)| *opcode == 3 && args[2..] == [0x110, 1])
        );

        if focus_loss {
            let other = f.server.state.surface_for_window(f.other.window).unwrap();
            let keyboard = f.server.state.seat.get_keyboard().unwrap();
            keyboard.set_focus(
                &mut f.server.state,
                Some(other),
                SERIAL_COUNTER.next_serial(),
            );
        } else {
            remote_keyboard::revoke(&mut f.server.state, remote_keyboard::LOCAL);
        }
        sync(&mut f.server);
        let releases: Vec<_> = pointer_events(&mut f.app, f.pointer)
            .into_iter()
            .filter(|(opcode, _)| *opcode == 3)
            .map(|(_, args)| (args[2], args[3]))
            .collect();
        assert_eq!(releases, [(0x110, 0)]);
        assert!(
            events(&mut f.capture.client)
                .iter()
                .any(|e| e.sender == f.lease && e.opcode == 4)
        );
        assert!(pointer_events(&mut f.other, f.other_pointer).is_empty());
        assert!(!super::active(&f.server.state));
    }
}

#[test]
fn native_gestures_require_explicit_creation_even_when_bound_at_v6() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::with_scroll(7, 6, false);
    let first = f.frame(&mut gl);
    f.arm(first);
    let proof = f.frame(&mut gl);
    let result = f.action(1, proof, 6, 13, 14);
    f.denied(&result);
}

#[test]
fn native_gesture_revocation_never_releases_on_new_pointer_receiver_or_enter() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for new_receiver in [false, true] {
        let mut f = Fixture::gestures();
        let initial = f.frame(&mut gl);
        f.arm(initial);
        let proof = f.frame(&mut gl);
        let started = f.action(1, proof, 6, 13, 14);
        f.accepted(&started, 1);
        let old_events = pointer_events(&mut f.app, f.pointer);
        assert!(
            old_events
                .iter()
                .any(|(opcode, args)| *opcode == 3 && args[2..] == [0x110, 1])
        );

        let added = if new_receiver {
            let (_, pointer, _) = bind_pointer(&mut f.server, &mut f.app);
            Some(pointer)
        } else {
            // Bypass the normal local takeover route to exercise the exact
            // enter check after a compositor focus change away and back.
            let pointer = f.server.state.seat.get_pointer().unwrap();
            let target = f.server.state.surface_for_window(f.app.window).unwrap();
            for focus in [None, Some((target, (0.0, 0.0).into()))] {
                pointer.motion(
                    &mut f.server.state,
                    focus,
                    &MotionEvent {
                        location: (5.0, 5.0).into(),
                        serial: SERIAL_COUNTER.next_serial(),
                        time: InputTime::now(),
                    },
                );
                pointer.frame(&mut f.server.state);
            }
            sync(&mut f.server);
            assert!(
                pointer_events(&mut f.app, f.pointer)
                    .iter()
                    .any(|(opcode, _)| *opcode == 1)
            );
            None
        };
        let result = f.drag_motion(2, 18, 20);
        assert!(result.iter().any(|e| e.sender == f.lease && e.opcode == 4));
        let releases: Vec<_> = events(&mut f.app.client)
            .into_iter()
            .filter(|e| (e.sender == f.pointer || Some(e.sender) == added) && e.opcode == 3)
            .map(|e| (e.sender, e.args[2], e.args[3]))
            .collect();
        if new_receiver {
            assert_eq!(releases, [(f.pointer, 0x110, 0)]);
        } else {
            assert!(releases.is_empty());
        }
        f.other_empty();
    }
}

#[test]
fn native_gesture_maps_cropped_hidpi_capture_to_logical_root_and_retires_old_layout() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::gestures_with_crop(Some([8, 12, 380, 270]));
    let surface = f.server.state.surface_for_window(f.app.window).unwrap();
    let rendered =
        smithay::backend::renderer::utils::with_renderer_surface_state(&surface, |state| {
            let size = state.buffer_size().unwrap();
            (state.buffer_scale(), size.w, size.h)
        });
    assert_eq!(rendered, Some((2, 400, 300)));
    let initial = f.frame(&mut gl);
    f.arm(initial);
    let proof = f.frame(&mut gl);
    let result = f.action(1, proof, 6, 15, 25);
    f.accepted(&result, 1);
    let result = f.drag_motion(2, 20, 30);
    f.accepted(&result, 2);
    let positions: Vec<_> = pointer_events(&mut f.app, f.pointer)
        .into_iter()
        .filter(|(opcode, _)| *opcode == 2)
        .map(|(_, args)| (args[1] / 256, args[2] / 256))
        .collect();
    assert_eq!(positions, [(23, 37), (28, 42)]);

    f.app.client.request(f.app.xdg, 3, &[9, 12, 380, 270]);
    f.app.client.request(f.app.surface, 6, &[]);
    sync(&mut f.server);
    assert!(
        events(&mut f.capture.client)
            .iter()
            .any(|e| e.sender == f.lease && e.opcode == 4)
    );
    let releases: Vec<_> = pointer_events(&mut f.app, f.pointer)
        .into_iter()
        .filter(|(opcode, _)| *opcode == 3)
        .map(|(_, args)| (args[2], args[3]))
        .collect();
    assert_eq!(releases, [(0x110, 0)]);
    assert!(f.drag_motion(3, 22, 33).is_empty());
    assert!(pointer_events(&mut f.app, f.pointer).is_empty());
    f.other_empty();
}

#[test]
fn native_gesture_paint_only_subsurface_keeps_root_input_and_revokes_on_region_change() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut child = 0;
    let mut f = Fixture::create(
        7,
        6,
        LeaseRequest {
            opcode: 6,
            arguments: vec![0],
            expected_ttl: Some(30_000),
        },
        None,
        |server, app| {
            let compositor = app.client.bind("wl_compositor", 6);
            let subcompositor = app.client.bind("wl_subcompositor", 1);
            child = app.client.new_id();
            app.client.request(compositor, 0, &[child]);
            let subsurface = app.client.new_id();
            app.client
                .request(subcompositor, 1, &[subsurface, child, app.surface]);
            app.client.request(subsurface, 5, &[]); // set_desync
            let empty = app.client.new_id();
            app.client.request(compositor, 1, &[empty]);
            app.client.request(child, 5, &[empty]);
            let (buffer, _file) =
                sized_shm_buffer_file(server, &mut app.client, [0, 120, 0, 255], 120, 120);
            app.client.request(child, 1, &[buffer, 0, 0]);
            app.client.request(child, 6, &[]);
            app.client.request(app.surface, 6, &[]);
            sync(server);
        },
    );
    let initial = f.frame(&mut gl);
    f.arm(initial);
    let proof = f.frame(&mut gl);
    let result = f.action(1, proof, 6, 15, 25);
    f.accepted(&result, 1);
    let original = pointer_events(&mut f.app, f.pointer);
    assert!(
        original
            .iter()
            .any(|(op, args)| *op == 3 && args[2..] == [0x110, 1])
    );

    f.app.client.request(child, 5, &[0]); // default full input region
    f.app.client.request(child, 6, &[]);
    sync(&mut f.server);
    assert!(
        events(&mut f.capture.client)
            .iter()
            .any(|e| e.sender == f.lease && e.opcode == 4)
    );
    let released = pointer_events(&mut f.app, f.pointer);
    assert_eq!(
        released
            .iter()
            .filter(|(op, _)| *op == 3)
            .map(|(_, args)| &args[2..])
            .collect::<Vec<_>>(),
        [&[0x110, 0]]
    );
    assert!(f.drag_motion(2, 20, 30).is_empty());
    f.other_empty();
}

#[test]
fn native_window_gesture_clicks_only_the_subsurface_proven_by_the_frame() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let grant = LeaseRequest {
        opcode: 7,
        arguments: vec![0],
        expected_ttl: Some(30_000),
    };
    let mut f = Fixture::create(7, 7, grant, None, |_, _| {});
    let initial = f.frame(&mut gl);
    f.arm(initial);
    let client = &mut f.app.client;
    let compositor = client.bind("wl_compositor", 6);
    let subcompositor = client.bind("wl_subcompositor", 1);
    let child = client.new_id();
    client.request(compositor, 0, &[child]);
    let subsurface = client.new_id();
    client.request(subcompositor, 1, &[subsurface, child, f.app.surface]);
    client.request(subsurface, 1, &[8, 12]);
    client.request(subsurface, 5, &[]); // set_desync
    let (buffer, _file) = sized_shm_buffer_file(&mut f.server, client, [0, 120, 0, 255], 120, 120);
    client.request(child, 1, &[buffer, 0, 0]);
    client.request(child, 6, &[]);
    client.request(f.app.surface, 6, &[]);
    sync(&mut f.server);

    let pointer = f.server.state.seat.get_pointer().unwrap();
    let root_focus = pointer.current_focus().unwrap();
    let root_position = pointer.current_location();
    let initial_enter = pointer.last_enter();
    let proof = f.frame(&mut gl);
    let result = f.action(1, proof, 6, 20, 28);
    f.accepted(&result, 1);
    assert!(pointer_events(&mut f.app, f.pointer).is_empty());
    let result = f.drag_release(2);
    f.accepted(&result, 2);
    let input: Vec<_> = pointer_events(&mut f.app, f.pointer)
        .into_iter()
        .filter_map(|(op, args)| match op {
            0 | 1 => Some((op, args[1], 0)),
            2 => Some((op, args[1] / 256, args[2] / 256)),
            3 => Some((op, args[2], args[3])),
            _ => None,
        })
        .collect();
    assert_eq!(
        input,
        [
            (1, f.app.surface, 0),
            (0, child, 0),
            (2, 12, 16),
            (3, 0x110, 1),
            (3, 0x110, 0),
            (1, child, 0),
            (0, f.app.surface, 0),
        ]
    );
    assert_eq!(pointer.current_focus(), Some(root_focus));
    assert_eq!(pointer.current_location(), root_position);
    assert_eq!(pointer.last_enter(), initial_enter);
    assert!(!pointer.is_grabbed());
    assert!(!pointer.has_pressed_buttons());
    f.other_empty();

    let proof = f.frame(&mut gl);
    let result = f.action(3, proof, 6, 20, 28);
    f.accepted(&result, 3);
    let empty = f.app.client.new_id();
    f.app.client.request(compositor, 1, &[empty]);
    f.app.client.request(child, 5, &[empty]);
    f.app.client.request(child, 6, &[]);
    sync(&mut f.server);
    let result = f.drag_release(4);
    assert!(result.iter().any(|e| e.sender == f.lease && e.opcode == 4));
    assert!(pointer_events(&mut f.app, f.pointer).is_empty());
    f.other_empty();
}

#[test]
fn native_gesture_defers_whole_window_confinement_but_revokes_other_constraints() {
    use smithay::wayland::pointer_constraints::with_pointer_constraint;

    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    // Unrestricted normal confinement during a drag, regional confinement,
    // pointer lock, and normal confinement outside of a drag.
    for (kind, region, drag) in [
        (2, false, true),
        (2, true, true),
        (1, false, true),
        (2, false, false),
    ] {
        let mut f = Fixture::gestures();
        let root = f.server.state.surface_for_window(f.app.window).unwrap();
        f.server.state.active_toplevel = Some(f.app.window);
        let pointer = f.server.state.seat.get_pointer().unwrap();
        let location = pointer.current_location();
        let first = f.frame(&mut gl);
        f.arm(first);
        let result = f.key(1, 56, true);
        f.accepted(&result, 1);
        if drag {
            let proof = f.frame(&mut gl);
            let result = f.action(2, proof, 6, 10, 15);
            f.accepted(&result, 2);
        }
        let mut region_object = 0;
        if region {
            let compositor = f.app.client.bind("wl_compositor", 6);
            region_object = f.app.client.new_id();
            f.app.client.request(compositor, 1, &[region_object]);
            f.app.client.request(region_object, 1, &[0, 0, 40, 40]);
        }
        f.app.client.events();
        let manager = f.app.client.bind("zwp_pointer_constraints_v1", 1);
        let constraint = f.app.client.new_id();
        f.app.client.request(
            manager,
            kind,
            &[constraint, f.app.surface, f.pointer, region_object, 2],
        );
        sync(&mut f.server);
        let controller = events(&mut f.capture.client);
        let revoked = controller
            .iter()
            .any(|e| e.sender == f.lease && e.opcode == 4);
        if kind == 2 && !region && drag {
            assert!(!revoked);
            assert!(with_pointer_constraint(&root, &pointer, |value| {
                value.is_some_and(|c| !c.is_active())
            }));
            let result = f.drag_motion(3, 80, 85);
            f.accepted(&result, 3);
            let result = f.drag_release(4);
            f.accepted(&result, 4);
            // The release and modifier up can precede the application's
            // asynchronous destruction of its confinement object.
            let result = f.key(5, 56, false);
            f.accepted(&result, 5);
            f.app.client.request(constraint, 0, &[]);
            sync(&mut f.server);
            let proof = f.frame(&mut gl);
            let result = f.action(6, proof, 1, 20, 25);
            f.accepted(&result, 6);
            assert_eq!(pointer.current_location(), location);
            assert_eq!(pointer.current_focus(), Some(root));
        } else {
            assert!(revoked);
            let cleanup = f.app.client.events();
            assert!(
                cleanup
                    .iter()
                    .any(|e| e.sender == f.keyboard && e.opcode == 3 && e.args[2..] == [56, 0])
            );
            assert_eq!(
                cleanup
                    .iter()
                    .filter(|e| e.sender == f.pointer && e.opcode == 3 && e.args[2..] == [0x110, 0])
                    .count(),
                usize::from(drag)
            );
        }
        f.other_empty();
    }
}

#[test]
fn native_gesture_disconnect_balances_delivered_button_and_modifier() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::gestures();
    let initial = f.frame(&mut gl);
    f.arm(initial);
    let result = f.key(1, 42, true);
    f.accepted(&result, 1);
    let proof = f.frame(&mut gl);
    let result = f.action(2, proof, 6, 13, 14);
    f.accepted(&result, 2);
    assert_eq!(f.target_keys(), [(42, 1)]);
    drop(f.capture);
    sync(&mut f.server);
    let records = events(&mut f.app.client);
    let buttons: Vec<_> = records
        .iter()
        .filter(|e| e.sender == f.pointer && e.opcode == 3)
        .map(|e| (e.args[2], e.args[3]))
        .collect();
    let keys: Vec<_> = records
        .iter()
        .filter(|e| e.sender == f.keyboard && e.opcode == 3)
        .map(|e| (e.args[2], e.args[3]))
        .collect();
    assert_eq!(buttons, [(0x110, 0)]);
    assert_eq!(keys, [(42, 0)]);
    assert!(pointer_events(&mut f.other, f.other_pointer).is_empty());
    assert!(!super::active(&f.server.state));
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
                .deadline = Instant::now();
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

#[path = "remote_control_scroll_tests.rs"]
mod scroll_axes_tests;

#[path = "remote_control_resumable_tests.rs"]
mod resumable_tests;

#[test]
fn wire_control_timed_creation_bounds_and_legacy_fixed_deadline() {
    for ttl in [1, 30_000, 30_001, 300_000] {
        let f = Fixture::with_duration(7, 5, false, Some(0), true, Some(ttl));
        let lease = f.server.state.remote_control.active.as_ref().unwrap();
        assert_eq!(
            lease.deadline.duration_since(lease.created),
            std::time::Duration::from_millis(u64::from(ttl))
        );
        assert!(lease.mode == super::LeaseMode::Resumable);
        assert_eq!(lease.scroll_axes, 0);
    }
    for ttl in [0, 300_001, u32::MAX] {
        let f = Fixture::with_duration(7, 5, false, Some(0), true, Some(ttl));
        assert!(f.server.state.remote_control.active.is_none());
    }
    for (scroll, axes, resumable) in [
        (false, None, false),
        (true, None, false),
        (false, Some(3), false),
        (false, Some(0), true),
    ] {
        let f = Fixture::with_duration(7, 5, scroll, axes, resumable, None);
        let lease = f.server.state.remote_control.active.as_ref().unwrap();
        assert_eq!(lease.deadline.duration_since(lease.created), super::TTL);
        assert_eq!(lease.mode == super::LeaseMode::Resumable, resumable);
    }
}
