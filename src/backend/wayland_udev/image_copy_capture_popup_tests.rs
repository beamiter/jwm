//! Owned real protocol and production offscreen pixels; not DRM/presentation proof.
#![cfg(feature = "backend-wayland-udev")]
use super::authority_tests::App;
use super::wire_test_client::{Client, Event, Server};
use super::*;
use crate::backend::wayland_udev::backend::fulfill_test_toplevel_capture;
use crate::backend::wayland_udev::compositor::headless_render::HeadlessSmithayGles;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;

fn sync(server: &mut Server) {
    server.roundtrip();
    server.roundtrip();
}
fn events(client: &mut Client) -> Vec<Event> {
    let e = client.events();
    assert!(
        e.iter().all(|e| !(e.sender == 1 && e.opcode == 0)),
        "wl_display.error"
    );
    e
}
fn fence(server: &mut Server, client: &mut Client) -> Vec<Event> {
    let id = client.new_id();
    client.request(1, 0, &[id]);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut result = Vec::new();
    loop {
        sync(server);
        let e = events(client);
        let done = e.iter().any(|e| e.sender == id && e.opcode == 0);
        result.extend(e);
        if done {
            return result;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "real wire sync timeout"
        );
    }
}
fn buffer(
    server: &mut Server,
    client: &mut Client,
    size: (u32, u32),
    pixel: [u8; 4],
) -> (u32, File) {
    assert!(size.0 > 0 && size.1 > 0 && size.0 <= 800 && size.1 <= 600);
    let shm = client.bind("wl_shm", 1);
    let fd = nix::sys::memfd::memfd_create(
        c"owned-popup-capture",
        nix::sys::memfd::MFdFlags::MFD_CLOEXEC,
    )
    .unwrap();
    let len = size.0 * size.1 * 4;
    nix::unistd::ftruncate(&fd, i64::from(len)).unwrap();
    let mut file = File::from(fd);
    file.write_all(&pixel.repeat((size.0 * size.1) as usize))
        .unwrap();
    let pool = client.new_id();
    client.request_with_fd_args(shm, 0, &[pool, len], file.as_raw_fd());
    let id = client.new_id();
    client.request(pool, 0, &[id, 0, size.0, size.1, size.0 * 4, 0]);
    fence(server, client);
    (id, file)
}
fn positioner(client: &mut Client, rect: (i32, i32, u32, u32)) -> u32 {
    let shell = client.bind("xdg_wm_base", 6);
    let id = client.new_id();
    client.request(shell, 1, &[id]);
    client.request(id, 1, &[rect.2, rect.3]);
    client.request(id, 2, &[0, 0, 1, 1]);
    client.request(id, 3, &[5]);
    client.request(id, 4, &[8]);
    client.request(id, 6, &[rect.0 as u32, rect.1 as u32]);
    id
}
pub(crate) struct Popup {
    pub(crate) surface: u32,
    pub(crate) xdg: u32,
    pub(crate) role: u32,
    size: (u32, u32),
}
impl Popup {
    pub(crate) fn new(
        server: &mut Server,
        client: &mut Client,
        parent_xdg: u32,
        rect: (i32, i32, u32, u32),
        pixel: [u8; 4],
    ) -> Self {
        let compositor = client.bind("wl_compositor", 6);
        let shell = client.bind("xdg_wm_base", 6);
        let positioner = positioner(client, rect);
        let surface = client.new_id();
        client.request(compositor, 0, &[surface]);
        let xdg = client.new_id();
        client.request(shell, 2, &[xdg, surface]);
        let role = client.new_id();
        client.request(xdg, 2, &[role, parent_xdg, positioner]);
        client.request(surface, 6, &[]);
        let e = fence(server, client);
        assert!(
            e.iter().any(|e| e.sender == role
                && e.opcode == 0
                && e.args == [rect.0 as u32, rect.1 as u32, rect.2, rect.3]),
            "actual popup configured at requested rectangle"
        );
        let serial = e
            .iter()
            .filter(|e| e.sender == xdg && e.opcode == 0)
            .next_back()
            .expect("popup configure")
            .args[0];
        client.request(xdg, 4, &[serial]);
        let (id, _file) = buffer(server, client, (rect.2, rect.3), pixel);
        client.request(xdg, 3, &[0, 0, rect.2, rect.3]);
        client.request(surface, 1, &[id, 0, 0]);
        client.request(surface, 6, &[]);
        fence(server, client);
        Self {
            surface,
            xdg,
            role,
            size: (rect.2, rect.3),
        }
    }
    pub(crate) fn reposition(&self, server: &mut Server, client: &mut Client, at: (i32, i32)) {
        let rect = (at.0, at.1, self.size.0, self.size.1);
        let pos = positioner(client, rect);
        client.request(self.role, 2, &[pos, 77]);
        client.request(self.surface, 6, &[]);
        let e = fence(server, client);
        assert!(
            e.iter()
                .any(|e| e.sender == self.role && e.opcode == 2 && e.args == [77])
        );
        assert!(e.iter().any(|e| e.sender == self.role
            && e.opcode == 0
            && e.args == [at.0 as u32, at.1 as u32, self.size.0, self.size.1]));
        let serial = e
            .iter()
            .filter(|e| e.sender == self.xdg && e.opcode == 0)
            .next_back()
            .expect("reposition configure")
            .args[0];
        client.request(self.xdg, 4, &[serial]);
        client.request(self.surface, 6, &[]);
        fence(server, client);
    }
    pub(crate) fn destroy(self, server: &mut Server, client: &mut Client) {
        client.request(self.role, 0, &[]);
        client.request(self.xdg, 0, &[]);
        client.request(self.surface, 0, &[]);
        fence(server, client);
    }
}
struct Capture {
    client: Client,
    source: u32,
    original: u32,
    manager: u32,
    session: u32,
    policy: u32,
    size: (u32, u32),
}
impl Capture {
    fn new(server: &mut Server, size: (u32, u32), popup: bool) -> Self {
        let mut client = server.connect();
        let list = client.bind("ext_foreign_toplevel_list_v1", 1);
        let sources = client.bind("ext_foreign_toplevel_image_capture_source_manager_v1", 1);
        let manager = client.bind("ext_image_copy_capture_manager_v1", 1);
        let e = fence(server, &mut client);
        let handle = e
            .iter()
            .find(|e| e.sender == list && e.opcode == 0)
            .expect("sole target handle")
            .args[0];
        let original = client.new_id();
        client.request(sources, 0, &[original, handle]);
        fence(server, &mut client);
        let policy = if popup {
            client.bind("jwm_readonly_popup_capture_manager_v1", 1)
        } else {
            0
        };
        let source = if popup {
            let id = client.new_id();
            client.request(policy, 1, &[id, original]);
            let e = fence(server, &mut client);
            assert!(
                e.iter()
                    .any(|e| e.sender == policy && e.opcode == 0 && e.args == [size.0, size.1])
            );
            assert!(!e.iter().any(|e| e.sender == policy && e.opcode == 1));
            id
        } else {
            original
        };
        let session = client.new_id();
        client.request(manager, 0, &[session, source, 0]);
        let e = fence(server, &mut client);
        assert!(
            e.iter()
                .any(|e| e.sender == session && e.opcode == 0 && e.args == [size.0, size.1])
        );
        Self {
            client,
            source,
            original,
            manager,
            session,
            policy,
            size,
        }
    }
    fn pixels(&mut self, server: &mut Server, gl: &mut HeadlessSmithayGles) -> Vec<u8> {
        let (buffer, mut file) = buffer(server, &mut self.client, self.size, [0x5a; 4]);
        let frame = self.client.new_id();
        self.client.request(self.session, 0, &[frame]);
        self.client.request(frame, 1, &[buffer]);
        self.client.request(frame, 3, &[]);
        fence(server, &mut self.client);
        let queue = server.state.image_capture_pending.as_ref().unwrap().clone();
        let mut cache = None;
        fulfill_test_toplevel_capture(&mut gl.renderer, &mut cache, &server.state, &queue);
        let e = fence(server, &mut self.client);
        assert!(
            e.iter().any(|e| e.sender == frame && e.opcode == 3),
            "production popup frame ready"
        );
        assert!(!e.iter().any(|e| e.sender == frame && e.opcode == 4));
        self.client.request(frame, 0, &[]);
        fence(server, &mut self.client);
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        bytes
    }
    fn stopped(&mut self, server: &mut Server) {
        let session = self.client.new_id();
        self.client
            .request(self.manager, 0, &[session, self.source, 0]);
        let e = fence(server, &mut self.client);
        assert!(e.iter().any(|e| e.sender == session && e.opcode == 5));
        assert!(!e.iter().any(|e| e.sender == session && e.opcode == 0));
    }
}
fn server() -> Server {
    let mut server = Server::new();
    server.state.image_capture_pending = Some(init_image_copy_capture(&server.display.handle()));
    server
}
fn assert_pixels(bytes: &[u8], expected: impl Fn(u32, u32) -> [u8; 4]) {
    assert_eq!(bytes.len(), 32 * 24 * 4);
    for y in 0..24 {
        for x in 0..32 {
            assert_eq!(
                &bytes[((y * 32 + x) * 4) as usize..((y * 32 + x + 1) * 4) as usize],
                &expected(x, y),
                "pixel{x},{y}"
            );
        }
    }
}

#[test]
fn wire_popup_capture_nested_order_alpha_negative_clip_and_foreign_root_isolation() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut server = server();
    let mut app = App::with_size_color(&mut server, [0, 0, 255, 255], 32, 24);
    let mut normal = Capture::new(&mut server, (32, 24), false);
    let mut capture = Capture::new(&mut server, (32, 24), true);
    let parent = Popup::new(
        &mut server,
        &mut app.client,
        app.xdg,
        (4, 3, 12, 10),
        [0, 255, 0, 255],
    );
    let _nested = Popup::new(
        &mut server,
        &mut app.client,
        parent.xdg,
        (2, 2, 4, 4),
        [255, 0, 0, 255],
    );
    let _alpha = Popup::new(
        &mut server,
        &mut app.client,
        app.xdg,
        (20, 2, 8, 8),
        [0, 128, 0, 128],
    );
    let _negative = Popup::new(
        &mut server,
        &mut app.client,
        app.xdg,
        (-3, 17, 8, 8),
        [0, 255, 0, 255],
    );
    let mut other = App::with_size_color(&mut server, [255, 255, 0, 255], 32, 24);
    let _other_popup = Popup::new(
        &mut server,
        &mut other.client,
        other.xdg,
        (0, 0, 32, 24),
        [0, 255, 255, 255],
    );
    // Both actual roles exist, but their root/owner identities differ.
    let root = server.state.surface_for_window(app.window).unwrap();
    let foreign = server.state.surface_for_window(other.window).unwrap();
    use smithay::reexports::wayland_server::Resource;
    assert_ne!(root.client().unwrap().id(), foreign.client().unwrap().id());
    assert_ne!(root, foreign);
    assert_pixels(&normal.pixels(&mut server, &mut gl), |_, _| {
        [0, 0, 255, 255]
    });
    let actual = capture.pixels(&mut server, &mut gl);
    assert_eq!(actual.len(), 32 * 24 * 4);
    for y in 0..24 {
        for x in 0..32 {
            let expected = if (6..10).contains(&x) && (5..9).contains(&y) {
                [255, 0, 0, 255]
            } else if (4..16).contains(&x) && (3..13).contains(&y) {
                [0, 255, 0, 255]
            } else if (20..28).contains(&x) && (2..10).contains(&y) {
                [0, 128, 127, 255]
            } else if x < 5 && y >= 17 {
                [0, 255, 0, 255]
            } else {
                [0, 0, 255, 255]
            };
            let pixel = &actual[((y * 32 + x) * 4) as usize..((y * 32 + x + 1) * 4) as usize];
            if (20..28).contains(&x) && (2..10).contains(&y) {
                for c in 0..3 {
                    assert!(
                        (i16::from(pixel[c]) - i16::from(expected[c])).abs() <= 1,
                        "alpha pixel{x},{y} channel{c}: {pixel:?}"
                    );
                }
                assert_eq!(pixel[3], 255);
            } else {
                assert_eq!(pixel, &expected, "pixel{x},{y}");
            }
        }
    }
}

#[test]
fn wire_popup_reposition_and_destroy_leave_no_old_pixels() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut server = server();
    let mut app = App::with_size_color(&mut server, [0, 0, 255, 255], 32, 24);
    let mut capture = Capture::new(&mut server, (32, 24), true);
    let popup = Popup::new(
        &mut server,
        &mut app.client,
        app.xdg,
        (2, 2, 6, 6),
        [0, 255, 0, 255],
    );
    assert_pixels(&capture.pixels(&mut server, &mut gl), |x, y| {
        if (2..8).contains(&x) && (2..8).contains(&y) {
            [0, 255, 0, 255]
        } else {
            [0, 0, 255, 255]
        }
    });
    popup.reposition(&mut server, &mut app.client, (18, 12));
    assert_pixels(&capture.pixels(&mut server, &mut gl), |x, y| {
        if (18..24).contains(&x) && (12..18).contains(&y) {
            [0, 255, 0, 255]
        } else {
            [0, 0, 255, 255]
        }
    });
    popup.destroy(&mut server, &mut app.client);
    assert_pixels(&capture.pixels(&mut server, &mut gl), |_, _| {
        [0, 0, 255, 255]
    });
}

#[test]
fn wire_popup_source_lock_and_root_remap_never_revive() {
    for lock in [false, true] {
        let mut server = server();
        let mut app = App::with_size_color(&mut server, [0, 0, 255, 255], 32, 24);
        let mut capture = Capture::new(&mut server, (32, 24), true);
        if lock {
            server.state.session_lock_supported = true;
            let mut locker = server.connect();
            let manager = locker.bind("ext_session_lock_manager_v1", 1);
            let id = locker.new_id();
            locker.request(manager, 1, &[id]);
            let e = fence(&mut server, &mut locker);
            assert!(server.state.session_locked);
            assert!(e.iter().any(|e| e.sender == id && e.opcode == 0));
            locker.request(id, 2, &[]);
            fence(&mut server, &mut locker);
            assert!(!server.state.session_locked);
        } else {
            app.unmap(&mut server);
            app.remap(&mut server);
        }
        let e = fence(&mut server, &mut capture.client);
        assert!(
            e.iter()
                .any(|e| e.sender == capture.policy && e.opcode == 1)
        );
        capture.stopped(&mut server);
        capture.stopped(&mut server);
    }
}

#[test]
fn wire_popup_derived_and_original_cannot_authorize_input_or_resize_follow() {
    use smithay::backend::input::InputTime;
    use smithay::input::pointer::MotionEvent;
    use smithay::utils::SERIAL_COUNTER;
    for (interface, version, revoked) in [
        ("jwm_remote_keyboard_manager_v1", 1, 4),
        ("jwm_remote_pointer_manager_v1", 3, 2),
        ("jwm_remote_control_manager_v1", 3, 4),
    ] {
        let mut server = server();
        let mut app = App::with_size_color(&mut server, [0, 0, 255, 255], 32, 24);
        let seat = app.client.bind("wl_seat", 7);
        let pointer = app.client.new_id();
        app.client.request(seat, 0, &[pointer]);
        let keyboard = app.client.new_id();
        app.client.request(seat, 1, &[keyboard]);
        fence(&mut server, &mut app.client);
        let surface = server.state.surface_for_window(app.window).unwrap();
        server
            .state
            .set_keyboard_focus(Some(surface.clone()), SERIAL_COUNTER.next_serial());
        let p = server.state.seat.get_pointer().unwrap();
        p.motion(
            &mut server.state,
            Some((surface.clone(), (0.0, 0.0).into())),
            &MotionEvent {
                location: (1.0, 1.0).into(),
                serial: SERIAL_COUNTER.next_serial(),
                time: InputTime::now(),
            },
        );
        p.frame(&mut server.state);
        fence(&mut server, &mut app.client);
        assert!(p.snapshot_authorized_pointer(&surface).is_some());
        assert!(
            server
                .state
                .seat
                .get_keyboard()
                .unwrap()
                .snapshot_authorized_keyboard(&surface)
                .is_some()
        );
        let mut capture = Capture::new(&mut server, (32, 24), true);
        for source in [capture.original, capture.source] {
            let manager = capture.client.bind(interface, version);
            let lease = capture.client.new_id();
            capture.client.request(manager, 1, &[lease, source]);
            let e = fence(&mut server, &mut capture.client);
            assert!(e.iter().any(|e| e.sender == lease && e.opcode == revoked));
            assert!(!e.iter().any(|e| e.sender == lease && e.opcode != revoked));
            let follow = capture
                .client
                .bind("jwm_readonly_resize_capture_manager_v1", 1);
            let derived = capture.client.new_id();
            capture.client.request(follow, 1, &[derived, source]);
            let e = fence(&mut server, &mut capture.client);
            assert!(e.iter().any(|e| e.sender == follow && e.opcode == 2));
            assert!(!e.iter().any(|e| e.sender == follow && e.opcode == 1));
        }
    }
}

#[test]
fn wire_popup_count_and_depth_budget_fail_before_output_write() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for nested in [false, true] {
        let mut server = server();
        let mut app = App::with_size_color(&mut server, [0, 0, 255, 255], 32, 24);
        let mut capture = Capture::new(&mut server, (32, 24), true);
        let mut parent = app.xdg;
        for _ in 0..if nested { 17 } else { 33 } {
            let popup = Popup::new(
                &mut server,
                &mut app.client,
                parent,
                (0, 0, 1, 1),
                [0, 255, 0, 255],
            );
            if nested {
                parent = popup.xdg;
            }
        }
        let (buffer, mut output) = buffer(&mut server, &mut capture.client, (32, 24), [0x5a; 4]);
        let frame = capture.client.new_id();
        capture.client.request(capture.session, 0, &[frame]);
        capture.client.request(frame, 1, &[buffer]);
        capture.client.request(frame, 3, &[]);
        fence(&mut server, &mut capture.client);
        let queue = server.state.image_capture_pending.as_ref().unwrap().clone();
        let mut cache = None;
        fulfill_test_toplevel_capture(&mut gl.renderer, &mut cache, &server.state, &queue);
        let e = fence(&mut server, &mut capture.client);
        assert!(e.iter().any(|e| e.sender == frame && e.opcode == 4));
        assert!(!e.iter().any(|e| e.sender == frame && e.opcode == 3));
        output.seek(SeekFrom::Start(0)).unwrap();
        let mut bytes = Vec::new();
        output.read_to_end(&mut bytes).unwrap();
        assert!(bytes.iter().all(|b| *b == 0x5a));
    }
}

// The owned server has no WM layout tick. Exercise the real pre-mutation
// invalidation hook, configure/ACK, and resized SHM commit, not just a map edit.
fn resize_popup_root(server: &mut Server, app: &mut App, size: (u32, u32)) {
    toplevel_capture_geometry_changing(&mut server.state, app.window, size.0, size.1);
    let geometry = server.state.window_geometry.get_mut(&app.window).unwrap();
    geometry.w = size.0;
    geometry.h = size.1;
    server.state.toplevels[&app.window].with_pending_state(|pending| {
        pending.size = Some((size.0 as i32, size.1 as i32).into());
    });
    server.state.toplevels[&app.window].send_configure();
    let e = fence(server, &mut app.client);
    let serial = e
        .iter()
        .filter(|e| e.sender == app.xdg && e.opcode == 0)
        .next_back()
        .expect("actual resize configure")
        .args[0];
    app.client.request(app.xdg, 4, &[serial]);
    let (id, _file) = buffer(server, &mut app.client, size, [0, 0, 255, 255]);
    app.client.request(app.xdg, 3, &[0, 0, size.0, size.1]);
    app.client.request(app.surface, 1, &[id, 0, 0]);
    app.client.request(app.surface, 6, &[]);
    fence(server, &mut app.client);
}

fn queue_popup_frame(server: &mut Server, capture: &mut Capture) -> (u32, File) {
    let (buffer, file) = buffer(server, &mut capture.client, capture.size, [0x5a; 4]);
    let frame = capture.client.new_id();
    capture.client.request(capture.session, 0, &[frame]);
    capture.client.request(frame, 1, &[buffer]);
    capture.client.request(frame, 3, &[]);
    let e = fence(server, &mut capture.client);
    assert!(
        !e.iter()
            .any(|e| e.sender == frame && matches!(e.opcode, 3 | 4))
    );
    (frame, file)
}

fn combined_capture(server: &mut Server, size: (u32, u32)) -> (Capture, u32) {
    let mut capture = Capture::new(server, size, false);
    let policy = capture
        .client
        .bind("jwm_readonly_resize_capture_manager_v1", 2);
    let source = capture.client.new_id();
    capture
        .client
        .request(policy, 2, &[source, capture.original]);
    let e = fence(server, &mut capture.client);
    assert!(e.iter().any(|e| e.sender == policy && e.opcode == 0));
    let layout = e
        .iter()
        .find(|e| e.sender == policy && e.opcode == 1)
        .expect("combined stable initial layout");
    assert_eq!(layout.args[1..], [size.0, size.1]);
    assert!(!e.iter().any(|e| e.sender == policy && e.opcode == 2));
    let epoch = layout.args[0];
    capture.source = source;
    capture.policy = policy;
    replace_popup_session(server, &mut capture, size);
    (capture, epoch)
}
fn replace_popup_session(server: &mut Server, capture: &mut Capture, size: (u32, u32)) {
    capture.session = capture.client.new_id();
    capture.size = size;
    capture
        .client
        .request(capture.manager, 0, &[capture.session, capture.source, 0]);
    let e = fence(server, &mut capture.client);
    assert!(
        e.iter()
            .any(|e| e.sender == capture.session && e.opcode == 0 && e.args == [size.0, size.1])
    );
    assert!(
        !e.iter()
            .any(|e| e.sender == capture.session && e.opcode == 5)
    );
}

#[test]
fn wire_popup_follow_resize_aba_replaces_canvas_and_never_writes_old_pending() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut server = server();
    let mut app = App::with_size_color(&mut server, [0, 0, 255, 255], 32, 24);
    let (mut capture, mut epoch) = combined_capture(&mut server, (32, 24));
    let _popup = Popup::new(
        &mut server,
        &mut app.client,
        app.xdg,
        (20, 12, 12, 12),
        [0, 255, 0, 255],
    );
    let mut old_sessions = Vec::new();
    for size in [(24, 18), (32, 24)] {
        let old_session = capture.session;
        old_sessions.push(old_session);
        let (old_frame, mut old_file) = queue_popup_frame(&mut server, &mut capture);
        resize_popup_root(&mut server, &mut app, size);
        let e = fence(&mut server, &mut capture.client);
        assert!(e.iter().any(|e| e.sender == old_session && e.opcode == 5));
        assert!(
            e.iter()
                .any(|e| e.sender == capture.policy && e.opcode == 0)
        );
        let layout = e
            .iter()
            .filter(|e| e.sender == capture.policy && e.opcode == 1)
            .next_back()
            .unwrap();
        assert!(layout.args[0] > epoch);
        epoch = layout.args[0];
        assert_eq!(layout.args[1..], [size.0, size.1]);
        replace_popup_session(&mut server, &mut capture, size);
        // New session is queued before old pending is fulfilled: stopping the
        // stale snapshot must not stop this newer same-source family member.
        let (frame, mut output) = queue_popup_frame(&mut server, &mut capture);
        let queue = server.state.image_capture_pending.as_ref().unwrap().clone();
        fulfill_test_toplevel_capture(&mut gl.renderer, &mut None, &server.state, &queue);
        let completed = fence(&mut server, &mut capture.client);
        assert!(
            e.iter()
                .chain(completed.iter())
                .any(|e| e.sender == old_frame && e.opcode == 4)
        );
        assert!(
            !e.iter()
                .chain(completed.iter())
                .any(|e| e.sender == old_frame && e.opcode == 3)
        );
        assert!(completed.iter().any(|e| e.sender == frame && e.opcode == 3));
        old_file.rewind().unwrap();
        let mut old = Vec::new();
        old_file.read_to_end(&mut old).unwrap();
        assert!(old.iter().all(|b| *b == 0x5a));
        output.rewind().unwrap();
        let mut pixels = Vec::new();
        output.read_to_end(&mut pixels).unwrap();
        assert_eq!(pixels.len(), (size.0 * size.1 * 4) as usize);
        for y in 0..size.1 {
            for x in 0..size.0 {
                let expected = if x >= 20 && y >= 12 {
                    [0, 255, 0, 255]
                } else {
                    [0, 0, 255, 255]
                };
                assert_eq!(
                    &pixels[((y * size.0 + x) * 4) as usize..((y * size.0 + x + 1) * 4) as usize],
                    &expected
                );
            }
        }
        capture.client.request(frame, 0, &[]);
        capture.client.request(old_frame, 0, &[]);
        fence(&mut server, &mut capture.client);
    }
    for session in old_sessions {
        let frame = capture.client.new_id();
        capture.client.request(session, 0, &[frame]);
        capture.client.request(frame, 3, &[]);
        let e = fence(&mut server, &mut capture.client);
        assert!(e.iter().any(|e| e.sender == frame && e.opcode == 4));
        assert!(!e.iter().any(|e| e.sender == frame && e.opcode == 3));
    }
}

#[test]
fn wire_popup_follow_lock_and_root_remap_never_revive() {
    for lock in [false, true] {
        let mut server = server();
        let mut app = App::with_size_color(&mut server, [0, 0, 255, 255], 32, 24);
        let (mut capture, _) = combined_capture(&mut server, (32, 24));
        if lock {
            server.state.session_lock_supported = true;
            let mut locker = server.connect();
            let manager = locker.bind("ext_session_lock_manager_v1", 1);
            let id = locker.new_id();
            locker.request(manager, 1, &[id]);
            let e = fence(&mut server, &mut locker);
            assert!(server.state.session_locked);
            assert!(e.iter().any(|e| e.sender == id && e.opcode == 0));
            locker.request(id, 2, &[]);
            fence(&mut server, &mut locker);
            assert!(!server.state.session_locked);
        } else {
            app.unmap(&mut server);
            app.remap(&mut server);
        }
        let e = fence(&mut server, &mut capture.client);
        assert!(
            e.iter()
                .any(|e| e.sender == capture.policy && e.opcode == 2)
        );
        capture.stopped(&mut server);
        capture.stopped(&mut server);
    }
}

#[test]
fn wire_popup_follow_entire_family_cannot_authorize_input_or_rederive() {
    use smithay::backend::input::InputTime;
    use smithay::input::pointer::MotionEvent;
    use smithay::utils::SERIAL_COUNTER;
    for (interface, version, revoked) in [
        ("jwm_remote_keyboard_manager_v1", 1, 4),
        ("jwm_remote_pointer_manager_v1", 3, 2),
        ("jwm_remote_control_manager_v1", 3, 4),
    ] {
        let mut server = server();
        let mut app = App::with_size_color(&mut server, [0, 0, 255, 255], 32, 24);
        let seat = app.client.bind("wl_seat", 7);
        let pointer = app.client.new_id();
        app.client.request(seat, 0, &[pointer]);
        let keyboard = app.client.new_id();
        app.client.request(seat, 1, &[keyboard]);
        fence(&mut server, &mut app.client);
        let surface = server.state.surface_for_window(app.window).unwrap();
        server
            .state
            .set_keyboard_focus(Some(surface.clone()), SERIAL_COUNTER.next_serial());
        let p = server.state.seat.get_pointer().unwrap();
        p.motion(
            &mut server.state,
            Some((surface.clone(), (0.0, 0.0).into())),
            &MotionEvent {
                location: (1.0, 1.0).into(),
                serial: SERIAL_COUNTER.next_serial(),
                time: InputTime::now(),
            },
        );
        p.frame(&mut server.state);
        fence(&mut server, &mut app.client);
        assert!(p.snapshot_authorized_pointer(&surface).is_some());
        assert!(
            server
                .state
                .seat
                .get_keyboard()
                .unwrap()
                .snapshot_authorized_keyboard(&surface)
                .is_some()
        );
        let (mut capture, _) = combined_capture(&mut server, (32, 24));
        for source in [capture.original, capture.source] {
            let manager = capture.client.bind(interface, version);
            let lease = capture.client.new_id();
            capture.client.request(manager, 1, &[lease, source]);
            let e = fence(&mut server, &mut capture.client);
            assert!(e.iter().any(|e| e.sender == lease && e.opcode == revoked));
            assert!(!e.iter().any(|e| e.sender == lease && e.opcode != revoked));
            let follow = capture
                .client
                .bind("jwm_readonly_resize_capture_manager_v1", 1);
            let derived = capture.client.new_id();
            capture.client.request(follow, 1, &[derived, source]);
            let e = fence(&mut server, &mut capture.client);
            assert!(e.iter().any(|e| e.sender == follow && e.opcode == 2));
            assert!(!e.iter().any(|e| e.sender == follow && e.opcode == 1));
        }
    }
}

#[test]
fn wire_popup_follow_distinct_xdg_and_effective_origin_align_pixels_and_revoke_on_change() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut server = server();
    let mut app = App::with_size_color(&mut server, [0, 0, 255, 255], 32, 24);
    // Legal explicit xdg geometry extends beyond the root's actual buffer.
    // Effective root crop is (0,0,32,24), not declared origin (-4,-3).
    app.client
        .request(app.xdg, 3, &[(-4_i32) as u32, (-3_i32) as u32, 36, 27]);
    app.client.request(app.surface, 6, &[]);
    fence(&mut server, &mut app.client);
    let (mut capture, _) = combined_capture(&mut server, (32, 24));
    let _popup = Popup::new(
        &mut server,
        &mut app.client,
        app.xdg,
        (7, 9, 8, 8),
        [0, 255, 0, 255],
    );
    let _edge = Popup::new(
        &mut server,
        &mut app.client,
        app.xdg,
        (2, 20, 6, 8),
        [255, 0, 0, 255],
    );
    assert_pixels(&capture.pixels(&mut server, &mut gl), |x, y| {
        if x < 4 && y >= 17 {
            [255, 0, 0, 255]
        } else if (3..11).contains(&x) && (6..14).contains(&y) {
            [0, 255, 0, 255]
        } else {
            [0, 0, 255, 255]
        }
    });
    // Same effective canvas, changed declared origin: not a resize-follow
    // opportunity, and restoring the old declaration cannot revive authority.
    for origin in [(-3_i32, -3_i32), (-4, -3)] {
        app.client
            .request(app.xdg, 3, &[origin.0 as u32, origin.1 as u32, 36, 27]);
        app.client.request(app.surface, 6, &[]);
        fence(&mut server, &mut app.client);
    }
    let e = fence(&mut server, &mut capture.client);
    assert!(
        e.iter()
            .any(|e| e.sender == capture.policy && e.opcode == 2)
    );
    capture.stopped(&mut server);
}

#[test]
fn wire_popup_follow_invalid_effective_extent_then_restore_is_terminal() {
    let mut server = server();
    let mut app = App::with_size_color(&mut server, [0, 0, 255, 255], 32, 24);
    // In this vendor implementation root SurfaceView.offset is always zero;
    // only subsurfaces have a nonzero offset. With a fixed xdg origin, resizing
    // a valid root extent cannot independently move the intersection origin.
    // Exercise the actually reachable failure instead: committed extent no
    // longer intersects the unchanged declaration, then becomes valid again.
    toplevel_capture_geometry_changing(&mut server.state, app.window, 24, 20);
    let geometry = server.state.window_geometry.get_mut(&app.window).unwrap();
    geometry.w = 24;
    geometry.h = 20;
    server.state.toplevels[&app.window].with_pending_state(|pending| {
        pending.size = Some((24, 20).into());
    });
    server.state.toplevels[&app.window].send_configure();
    let e = fence(&mut server, &mut app.client);
    let serial = e
        .iter()
        .filter(|e| e.sender == app.xdg && e.opcode == 0)
        .next_back()
        .unwrap()
        .args[0];
    app.client.request(app.xdg, 4, &[serial]);
    app.client.request(app.xdg, 3, &[8, 4, 24, 20]);
    app.client.request(app.surface, 6, &[]);
    fence(&mut server, &mut app.client);
    let (mut capture, _) = combined_capture(&mut server, (24, 20));
    let surface = server.state.surface_for_window(app.window).unwrap();
    let declared = server.state.surface_window_geometry_rect(&surface).unwrap();
    for size in [(4, 4), (32, 24)] {
        let (id, _file) = buffer(&mut server, &mut app.client, size, [0, 0, 255, 255]);
        app.client.request(app.surface, 1, &[id, 0, 0]);
        app.client.request(app.surface, 6, &[]);
        fence(&mut server, &mut app.client);
        assert_eq!(
            server.state.surface_window_geometry_rect(&surface),
            Some(declared)
        );
        let e = fence(&mut server, &mut capture.client);
        if size == (4, 4) {
            assert!(
                e.iter()
                    .any(|e| e.sender == capture.policy && e.opcode == 2)
            );
        }
        assert!(
            !e.iter()
                .any(|e| e.sender == capture.policy && e.opcode == 1)
        );
        capture.stopped(&mut server);
    }
}
