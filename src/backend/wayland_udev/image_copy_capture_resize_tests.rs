//! Owned real Wayland wire and production offscreen capture regressions.
//! No DRM/presentation claim; execute only in the permitted CI environment.
#![cfg(feature = "backend-wayland-udev")]
use super::authority_tests::App;
use super::wire_test_client::{Client, Event, Server};
use super::*;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;

fn sync(server: &mut Server) {
    server.roundtrip();
    server.roundtrip();
}
fn events(client: &mut Client) -> Vec<Event> {
    let result = client.events();
    assert!(
        result.iter().all(|e| !(e.sender == 1 && e.opcode == 0)),
        "unexpected wl_display.error"
    );
    result
}
fn buffer(
    server: &mut Server,
    client: &mut Client,
    size: (u32, u32),
    pixel: [u8; 4],
) -> (u32, File) {
    let (width, height) = size;
    assert!(width > 0 && height > 0 && width <= 800 && height <= 600);
    let shm = client.bind("wl_shm", 1);
    let fd = nix::sys::memfd::memfd_create(
        c"resize-follow-owned-wire",
        nix::sys::memfd::MFdFlags::MFD_CLOEXEC,
    )
    .unwrap();
    nix::unistd::ftruncate(&fd, i64::from(width * height * 4)).unwrap();
    let mut file = File::from(fd);
    file.write_all(&pixel.repeat((width * height) as usize))
        .unwrap();
    let pool = client.new_id();
    client.request_with_fd_args(shm, 0, &[pool, width * height * 4], file.as_raw_fd());
    let id = client.new_id();
    client.request(pool, 0, &[id, 0, width, height, width * 4, 0]);
    sync(server);
    events(client);
    (id, file)
}
fn bytes(file: &mut File) -> Vec<u8> {
    file.seek(SeekFrom::Start(0)).unwrap();
    let mut result = Vec::new();
    file.read_to_end(&mut result).unwrap();
    result
}
fn resize(server: &mut Server, app: &mut App, size: (u32, u32), pixel: [u8; 4]) {
    // The owned headless wire server has no WM layout tick. Match its layout
    // to the actual xdg configure/ACK followed by a real resized SHM commit.
    let geometry = server.state.window_geometry.get_mut(&app.window).unwrap();
    geometry.w = size.0;
    geometry.h = size.1;
    server.state.toplevels[&app.window].with_pending_state(|pending| {
        pending.size = Some((size.0 as i32, size.1 as i32).into());
    });
    server.state.toplevels[&app.window].send_configure();
    sync(server);
    let serial = events(&mut app.client)
        .into_iter()
        .filter(|e| e.sender == app.xdg && e.opcode == 0)
        .next_back()
        .expect("real xdg configure")
        .args[0];
    app.client.request(app.xdg, 4, &[serial]);
    let (id, _file) = buffer(server, &mut app.client, size, pixel);
    app.client.request(app.surface, 1, &[id, 0, 0]);
    app.client.request(app.surface, 6, &[]);
    // Explicit sync acknowledgement closes the SCM_RIGHTS dispatch boundary.
    let done = app.client.new_id();
    app.client.request(1, 0, &[done]);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        sync(server);
        if events(&mut app.client)
            .iter()
            .any(|e| e.sender == done && e.opcode == 0)
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "resized commit sync timeout"
        );
    }
    assert!(server.state.mapped_windows.contains(&app.window));
}

fn server() -> Server {
    let mut server = Server::new();
    server.state.image_capture_pending = Some(init_image_copy_capture(&server.display.handle()));
    server
}
struct Capture {
    client: Client,
    source: u32,
    original: u32,
    manager: u32,
    session: u32,
    follow_manager: Option<u32>,
    layout_epoch: u32,
}
impl Capture {
    fn assert_new_layout(&mut self, e: &[Event], size: (u32, u32)) {
        let manager = self.follow_manager.unwrap();
        assert!(
            e.iter().any(|e| e.sender == manager && e.opcode == 0),
            "resize suspended old layout"
        );
        let layout = e
            .iter()
            .filter(|e| e.sender == manager && e.opcode == 1)
            .next_back()
            .expect("stable replacement layout");
        assert_eq!(layout.args.len(), 3);
        assert!(
            layout.args[0] > self.layout_epoch,
            "layout epoch cannot revive after size ABA"
        );
        assert_eq!(layout.args[1..], [size.0, size.1]);
        self.layout_epoch = layout.args[0];
        assert!(!e.iter().any(|e| e.sender == manager && e.opcode == 2));
    }
    fn session(&mut self, server: &mut Server, expected: Option<(u32, u32)>) {
        self.session = self.client.new_id();
        self.client
            .request(self.manager, 0, &[self.session, self.source, 0]);
        sync(server);
        let e = events(&mut self.client);
        match expected {
            Some((w, h)) => {
                assert!(
                    e.iter()
                        .any(|e| e.sender == self.session && e.opcode == 0 && e.args == [w, h]),
                    "new same-source session constraints"
                );
                assert!(e.iter().any(|e| e.sender == self.session && e.opcode == 4));
                assert!(!e.iter().any(|e| e.sender == self.session && e.opcode == 5));
            }
            None => {
                assert!(e.iter().any(|e| e.sender == self.session && e.opcode == 5));
                assert!(!e.iter().any(|e| e.sender == self.session && e.opcode == 0));
            }
        }
    }
    fn queue(&mut self, server: &mut Server, size: (u32, u32)) -> (u32, File) {
        let (id, file) = buffer(server, &mut self.client, size, [0x5a; 4]);
        let frame = self.client.new_id();
        self.client.request(self.session, 0, &[frame]);
        self.client.request(frame, 1, &[id]);
        self.client.request(frame, 3, &[]);
        sync(server);
        let e = events(&mut self.client);
        assert!(
            !e.iter()
                .any(|e| e.sender == frame && (e.opcode == 3 || e.opcode == 4)),
            "frame should be queued, not completed"
        );
        (frame, file)
    }
}

impl Capture {
    fn new(server: &mut Server, follow: bool, size: (u32, u32)) -> Self {
        let mut client = server.connect();
        let list = client.bind("ext_foreign_toplevel_list_v1", 1);
        let sources = client.bind("ext_foreign_toplevel_image_capture_source_manager_v1", 1);
        let manager = client.bind("ext_image_copy_capture_manager_v1", 1);
        sync(server);
        let handle = events(&mut client)
            .into_iter()
            .find(|e| e.sender == list && e.opcode == 0)
            .expect("sole real toplevel")
            .args[0];
        let initial = client.new_id();
        client.request(sources, 0, &[initial, handle]);
        sync(server);
        events(&mut client);
        let mut follow_identity = None;
        let mut layout_epoch = 0;
        let source = if follow {
            let follow_manager = client.bind("jwm_readonly_resize_capture_manager_v1", 1);
            let source = client.new_id();
            client.request(follow_manager, 1, &[source, initial]);
            sync(server);
            let e = events(&mut client);
            assert!(
                e.iter().any(|e| e.sender == follow_manager
                    && e.opcode == 1
                    && e.args.len() == 3
                    && e.args[0] > 0
                    && e.args[1..] == [size.0, size.1]),
                "initial stable follow layout"
            );
            assert!(
                !e.iter()
                    .any(|e| e.sender == follow_manager && e.opcode == 2)
            );
            follow_identity = Some(follow_manager);
            layout_epoch = e
                .iter()
                .find(|e| e.sender == follow_manager && e.opcode == 1)
                .unwrap()
                .args[0];
            source
        } else {
            initial
        };
        let mut capture = Self {
            client,
            source,
            original: initial,
            manager,
            session: 0,
            follow_manager: follow_identity,
            layout_epoch,
        };
        capture.session(server, Some(size));
        capture
    }
}

#[test]
fn wire_default_source_still_stops_permanently_on_resize() {
    let mut server = server();
    let mut app = App::with_size_color(&mut server, [0, 0, 255, 255], 320, 240);
    let mut capture = Capture::new(&mut server, false, (320, 240));
    let old = capture.session;
    resize(&mut server, &mut app, (400, 300), [0, 255, 0, 255]);
    assert!(
        events(&mut capture.client)
            .iter()
            .any(|e| e.sender == old && e.opcode == 5)
    );
    capture.session(&mut server, None);
    resize(&mut server, &mut app, (320, 240), [0, 0, 255, 255]);
    capture.session(&mut server, None);
}

#[test]
fn wire_follow_resize_aba_old_pending_cannot_revive_or_stop_new_session_pixels() {
    use crate::backend::wayland_udev::backend::fulfill_test_toplevel_capture;
    use crate::backend::wayland_udev::compositor::headless_render::HeadlessSmithayGles;
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for restore in [false, true] {
        let mut server = server();
        let mut app = App::with_size_color(&mut server, [0, 0, 255, 255], 320, 240);
        let mut capture = Capture::new(&mut server, true, (320, 240));
        let _other = App::with_size_color(&mut server, [255, 0, 0, 255], 400, 300);
        let old_session = capture.session;
        let (old_frame, mut old_file) = capture.queue(&mut server, (320, 240));
        let queue = server.state.image_capture_pending.as_ref().unwrap().clone();
        assert_eq!(queue.lock().unwrap().len(), 1);
        resize(&mut server, &mut app, (400, 300), [0, 255, 0, 255]);
        let changed = events(&mut capture.client);
        assert!(
            changed
                .iter()
                .any(|e| e.sender == old_session && e.opcode == 5)
        );
        capture.assert_new_layout(&changed, (400, 300));
        let size = if restore {
            resize(&mut server, &mut app, (320, 240), [0, 255, 0, 255]);
            let changed = events(&mut capture.client);
            capture.assert_new_layout(&changed, (320, 240));
            (320, 240)
        } else {
            (400, 300)
        };
        capture.session(&mut server, Some(size));
        let new_session = capture.session;
        let (new_frame, mut new_file) = capture.queue(&mut server, size);
        let mut cache = None;
        fulfill_test_toplevel_capture(&mut gl.renderer, &mut cache, &server.state, &queue);
        sync(&mut server);
        let e = events(&mut capture.client);
        assert!(
            e.iter()
                .any(|e| e.sender == old_frame && e.opcode == 4 && e.args == [2]),
            "old frame must fail Stopped"
        );
        assert!(!e.iter().any(|e| e.sender == old_frame && e.opcode == 3));
        assert!(
            e.iter().any(|e| e.sender == new_frame && e.opcode == 3),
            "new same-source session captures stable new layout"
        );
        assert!(
            !e.iter().any(|e| e.sender == new_session && e.opcode == 5),
            "old failure must not stop replacement session"
        );
        assert!(
            bytes(&mut old_file).iter().all(|b| *b == 0x5a),
            "old pending must not write even after dimensions restored"
        );
        let pixels = bytes(&mut new_file);
        assert_eq!(pixels.len(), (size.0 * size.1 * 4) as usize);
        assert!(
            pixels.chunks_exact(4).all(|p| p == [0, 255, 0, 255]),
            "selected green target only, not overlapping blue"
        );
    }
}

#[test]
fn wire_follow_unmap_lock_and_destroy_are_permanent_not_resize() {
    for mode in 0..3 {
        let mut server = server();
        let mut app = App::new(&mut server);
        let mut capture = Capture::new(&mut server, true, (800, 600));
        let old = capture.session;
        match mode {
            0 => {
                app.unmap(&mut server);
                app.remap(&mut server);
            }
            1 => {
                server.state.session_lock_supported = true;
                let mut locker = server.connect();
                let manager = locker.bind("ext_session_lock_manager_v1", 1);
                let lock = locker.new_id();
                locker.request(manager, 1, &[lock]);
                sync(&mut server);
                assert!(server.state.session_locked);
                assert!(
                    events(&mut locker)
                        .iter()
                        .any(|e| e.sender == lock && e.opcode == 0)
                );
                locker.request(lock, 2, &[]);
                sync(&mut server);
                assert!(!server.state.session_locked);
            }
            _ => {
                app.client.request(app.toplevel, 0, &[]);
                sync(&mut server);
                events(&mut app.client);
            }
        }
        assert!(
            events(&mut capture.client)
                .iter()
                .any(|e| e.sender == old && e.opcode == 5)
        );
        capture.session(&mut server, None);
        capture.session(&mut server, None);
    }
}

#[test]
fn wire_follow_source_never_authorizes_keyboard_pointer_or_control() {
    use smithay::backend::input::InputTime;
    use smithay::input::pointer::MotionEvent;
    use smithay::utils::SERIAL_COUNTER;
    for (interface, version, revoked) in [
        ("jwm_remote_keyboard_manager_v1", 1, 4),
        ("jwm_remote_pointer_manager_v1", 2, 2),
        ("jwm_remote_control_manager_v1", 2, 4),
    ] {
        let mut server = server();
        let mut app = App::new(&mut server);
        let seat = app.client.bind("wl_seat", 7);
        let pointer = app.client.new_id();
        app.client.request(seat, 0, &[pointer]);
        let keyboard = app.client.new_id();
        app.client.request(seat, 1, &[keyboard]);
        sync(&mut server);
        events(&mut app.client);
        let surface = server.state.surface_for_window(app.window).unwrap();
        server
            .state
            .set_keyboard_focus(Some(surface.clone()), SERIAL_COUNTER.next_serial());
        let handle = server.state.seat.get_pointer().unwrap();
        handle.motion(
            &mut server.state,
            Some((surface, (0.0, 0.0).into())),
            &MotionEvent {
                location: (5.0, 5.0).into(),
                serial: SERIAL_COUNTER.next_serial(),
                time: InputTime::now(),
            },
        );
        handle.frame(&mut server.state);
        sync(&mut server);
        events(&mut app.client);
        let target = server.state.surface_for_window(app.window).unwrap();
        assert!(
            server
                .state
                .seat
                .get_keyboard()
                .unwrap()
                .snapshot_authorized_keyboard(&target)
                .is_some()
        );
        assert!(
            server
                .state
                .seat
                .get_pointer()
                .unwrap()
                .snapshot_authorized_pointer(&target)
                .is_some()
        );
        let mut capture = Capture::new(&mut server, true, (800, 600));
        let manager = capture.client.bind(interface, version);
        for source in [capture.source, capture.original] {
            let lease = capture.client.new_id();
            capture.client.request(manager, 1, &[lease, source]);
            sync(&mut server);
            let e = events(&mut capture.client);
            assert!(
                e.iter().any(|e| e.sender == lease && e.opcode == revoked),
                "{interface} must reject read-only follow source"
            );
            let granted = if revoked == 2 { 0 } else { 1 };
            assert!(!e.iter().any(|e| e.sender == lease && e.opcode == granted));
            assert!(!crate::backend::wayland_udev::remote_keyboard::active(
                &server.state
            ));
            assert!(!crate::backend::wayland_udev::remote_pointer::active(
                &server.state
            ));
            assert!(!crate::backend::wayland_udev::remote_control::active(
                &server.state
            ));
        }
    }
}

fn assert_old_session_stopped(server: &mut Server, capture: &mut Capture) {
    let frame = capture.client.new_id();
    capture.client.request(capture.session, 0, &[frame]);
    capture.client.request(frame, 3, &[]);
    sync(server);
    let e = events(&mut capture.client);
    assert!(
        e.iter()
            .any(|e| e.sender == frame && e.opcode == 4 && e.args == [2])
    );
    assert!(!e.iter().any(|e| e.sender == frame && e.opcode == 3));
    capture.client.request(frame, 0, &[]);
    sync(server);
    events(&mut capture.client);
}

#[test]
fn wire_follow_manager_or_source_destroy_stops_only_its_family() {
    for destroy_source in [false, true] {
        let mut server = server();
        let _app = App::new(&mut server);
        let mut capture = Capture::new(&mut server, true, (800, 600));
        // A distinct locally selected authority of the same window is not this family.
        let mut other = Capture::new(&mut server, true, (800, 600));
        let old = capture.session;
        let object = if destroy_source {
            capture.source
        } else {
            capture.follow_manager.unwrap()
        };
        capture.client.request(object, 0, &[]);
        sync(&mut server);
        assert!(
            events(&mut capture.client)
                .iter()
                .any(|e| e.sender == old && e.opcode == 5)
        );
        assert_old_session_stopped(&mut server, &mut capture);
        if !destroy_source {
            capture.session(&mut server, None);
        }
        let other_events = events(&mut other.client);
        assert!(
            !other_events
                .iter()
                .any(|e| e.sender == other.session && e.opcode == 5)
        );
        other.session(&mut server, Some((800, 600)));
    }
}

#[test]
fn wire_follow_repeat_create_stops_original_family_and_new_source_is_inert() {
    let mut server = server();
    let _app = App::new(&mut server);
    let mut capture = Capture::new(&mut server, true, (800, 600));
    let old = capture.session;
    let new = capture.client.new_id();
    capture
        .client
        .request(capture.follow_manager.unwrap(), 1, &[new, capture.original]);
    sync(&mut server);
    let e = events(&mut capture.client);
    assert!(e.iter().any(|e| e.sender == old && e.opcode == 5));
    assert!(
        e.iter()
            .any(|e| e.sender == capture.follow_manager.unwrap() && e.opcode == 2)
    );
    assert_old_session_stopped(&mut server, &mut capture);
    capture.session(&mut server, None);
    capture.source = new;
    capture.session(&mut server, None);
}

#[test]
fn wire_follow_original_cannot_be_derived_twice_even_with_fresh_manager() {
    let mut server = server();
    let _app = App::new(&mut server);
    let mut capture = Capture::new(&mut server, true, (800, 600));
    let original_follow = capture.source;
    let manager = capture
        .client
        .bind("jwm_readonly_resize_capture_manager_v1", 1);
    let second = capture.client.new_id();
    capture
        .client
        .request(manager, 1, &[second, capture.original]);
    sync(&mut server);
    let e = events(&mut capture.client);
    assert!(e.iter().any(|e| e.sender == manager && e.opcode == 2));
    assert!(!e.iter().any(|e| e.sender == manager && e.opcode == 1));
    capture.source = second;
    capture.session(&mut server, None);
    capture.source = original_follow;
    capture.session(&mut server, Some((800, 600)));
}

#[test]
fn wire_follow_epoch_budget_exhaustion_is_permanent() {
    let mut server = server();
    let mut app = App::new(&mut server);
    let mut capture = Capture::new(&mut server, true, (800, 600));
    // Inject only the budget counter, never a ready frame, authority or success.
    // The subsequent real resized commit executes the production exhaustion path.
    let root = server.state.toplevel_capture_authorities.by_window[&app.window]
        .iter()
        .filter_map(|w| w.upgrade())
        .find(|a| a.follow.is_some())
        .unwrap();
    root.follow.as_ref().unwrap().lock().unwrap().epoch = 1024;
    resize(&mut server, &mut app, (400, 300), [0, 255, 0, 255]);
    let e = events(&mut capture.client);
    assert!(
        e.iter()
            .any(|e| e.sender == capture.follow_manager.unwrap() && e.opcode == 2)
    );
    assert!(
        e.iter()
            .any(|e| e.sender == capture.session && e.opcode == 5)
    );
    capture.session(&mut server, None);
    resize(&mut server, &mut app, (800, 600), [0, 0, 255, 255]);
    capture.session(&mut server, None);
}
