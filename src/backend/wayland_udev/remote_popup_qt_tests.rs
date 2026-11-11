//! Dedicated CI-only real Qt Widgets probe. Native capture is not presentation.
use crate::backend::wayland::state::JwmClientState;
use crate::backend::wayland_udev::{
    backend::fulfill_test_toplevel_capture,
    compositor::headless_render::HeadlessSmithayGles,
    image_copy_capture::{
        authority_tests::Capture,
        init_image_copy_capture,
        wire_test_client::{Client, Event, Server},
    },
};
use smithay::{
    reexports::wayland_server::{Resource, protocol::wl_surface::WlSurface},
    wayland::compositor::{SurfaceAttributes, TraversalAction, with_surface_tree_downward},
};
use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    os::{
        fd::AsRawFd,
        unix::{net::UnixStream, process::CommandExt},
    },
    process::{Child, Command, Stdio},
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};
const W: u32 = 320;
const H: u32 = 240;
struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn fence(server: &mut Server, client: &mut Client) -> Vec<Event> {
    let id = client.new_id();
    client.request(1, 0, &[id]);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut all = Vec::new();
    loop {
        server.roundtrip();
        let events = client.events();
        assert!(
            !events.iter().any(|e| e.sender == 1 && e.opcode == 0),
            "Qt probe protocol failure"
        );
        let done = events.iter().any(|e| e.sender == id && e.opcode == 0);
        all.extend(events);
        if done {
            return all;
        }
        assert!(Instant::now() < deadline, "Qt probe sync timeout");
        thread::yield_now();
    }
}
// Only after a real completed render; wl_callback is not wp_presentation proof.
fn callbacks(surface: &WlSurface) {
    with_surface_tree_downward(
        surface,
        (),
        |_, _, _| TraversalAction::DoChildren(()),
        |_, states, _| {
            for callback in states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .frame_callbacks
                .drain(..)
            {
                callback.done(1);
            }
        },
        |_, _, _| true,
    );
}
struct Frame {
    id: u32,
    objects: [u32; 3],
    pixels: Vec<u8>,
}
impl Frame {
    fn retire(self, server: &mut Server, capture: &mut Capture) {
        capture.client.request(self.id, 0, &[]);
        capture.client.request(self.objects[0], 1, &[]);
        capture.client.request(self.objects[1], 0, &[]);
        capture.client.request(self.objects[2], 1, &[]);
        fence(server, &mut capture.client);
    }
}
fn frame(
    server: &mut Server,
    capture: &mut Capture,
    source: u32,
    gl: &mut HeadlessSmithayGles,
) -> Frame {
    let c = &mut capture.client;
    let shm = c.bind("wl_shm", 1);
    let fd = nix::sys::memfd::memfd_create(c"qt-capture", nix::sys::memfd::MFdFlags::MFD_CLOEXEC)
        .unwrap();
    nix::unistd::ftruncate(&fd, i64::from(W * H * 4)).unwrap();
    let mut file = File::from(fd);
    let pool = c.new_id();
    c.request_with_fd_args(shm, 0, &[pool, W * H * 4], file.as_raw_fd());
    let buffer = c.new_id();
    c.request(pool, 0, &[buffer, 0, W, H, W * 4, 0]);
    let session = c.new_id();
    c.request(capture.manager, 0, &[session, source, 0]);
    let events = fence(server, c);
    assert!(
        events
            .iter()
            .any(|e| e.sender == session && e.opcode == 0 && e.args == [W, H]),
        "Qt canvas constraints"
    );
    let id = c.new_id();
    c.request(session, 0, &[id]);
    c.request(id, 1, &[buffer]);
    c.request(id, 3, &[]);
    fence(server, c);
    let queue = server.state.image_capture_pending.as_ref().unwrap().clone();
    fulfill_test_toplevel_capture(&mut gl.renderer, &mut None, &server.state, &queue);
    let events = fence(server, c);
    assert!(
        events.iter().any(|e| e.sender == id && e.opcode == 3),
        "Qt real render not ready"
    );
    assert!(!events.iter().any(|e| e.sender == id && e.opcode == 4));
    file.seek(SeekFrom::Start(0)).unwrap();
    let mut pixels = vec![0; (W * H * 4) as usize];
    file.read_exact(&mut pixels).unwrap();
    Frame {
        id,
        objects: [session, buffer, pool],
        pixels,
    }
}
#[test]
#[ignore = "mandatory dedicated Qt6 CI; requires JWM_REMOTE_POPUP_QT_PROBE executable"]
fn actual_qt_dynamic_private_seat_menu_selection_and_terminal_retirement() {
    let exe = std::env::var_os("JWM_REMOTE_POPUP_QT_PROBE").expect("Qt probe executable required");
    let mut gl = HeadlessSmithayGles::new().expect("mandatory software GL unavailable");
    let mut server = Server::new();
    server.state.image_capture_pending = Some(init_image_copy_capture(&server.display.handle()));
    let (native, peer) = UnixStream::pair().unwrap();
    let qt_id = server
        .display
        .handle()
        .insert_client(native, Arc::new(JwmClientState::default()))
        .unwrap()
        .id();
    let mut other = server.connect();
    let other_seat = other.bind("wl_seat", 7);
    let other_pointer = other.new_id();
    other.request(other_seat, 0, &[other_pointer]);
    fence(&mut server, &mut other);
    let fd = peer.as_raw_fd();
    let mut command = Command::new(exe);
    command
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env("WAYLAND_SOCKET", fd.to_string())
        .env("QT_QPA_PLATFORM", "wayland")
        .env("QT_WAYLAND_DISABLE_WINDOWDECORATION", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Clear CLOEXEC only on this child's explicitly supplied socket.
    unsafe {
        command.pre_exec(move || {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut process = Process(command.spawn().expect("launch Qt probe"));
    drop(peer);
    let stdout = process.0.stdout.take().unwrap();
    let (tx, rx) = mpsc::sync_channel(4);
    let reader = thread::spawn(move || {
        let mut input = BufReader::new(stdout);
        for _ in 0..8 {
            let mut line = Vec::new();
            let n = input
                .by_ref()
                .take(128)
                .read_until(b'\n', &mut line)
                .unwrap();
            if n == 0 {
                return;
            }
            assert!(n < 128, "bounded Qt probe output");
            match line.as_slice() {
                b"QT_PROBE_ACTIVATED_ONCE\n" => assert!(tx.try_send(()).is_ok()),
                b"QT_PROBE_BUTTON_PRESSED\n" => eprintln!("JWM_QT_STAGE=toolkit_button_pressed"),
                b"QT_PROBE_MENU_ABOUT_TO_SHOW\n" => {
                    eprintln!("JWM_QT_STAGE=toolkit_menu_about_to_show")
                }
                _ => panic!("Qt probe unexpected event"),
            }
        }
        panic!("too many Qt events");
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut configured = false;
    let root = loop {
        server.roundtrip();
        assert!(Instant::now() < deadline, "Qt root timeout");
        assert!(process.0.try_wait().unwrap().is_none(), "Qt process exited");
        let roots: Vec<_> = server
            .state
            .toplevels
            .iter()
            .filter(|(_, t)| t.wl_surface().client().is_some_and(|c| c.id() == qt_id))
            .map(|(id, _)| *id)
            .collect();
        assert!(roots.len() <= 1);
        if let Some(&id) = roots.first() {
            if !configured {
                let g = server.state.window_geometry.get_mut(&id).unwrap();
                g.w = W;
                g.h = H;
                server.state.toplevels[&id]
                    .with_pending_state(|p| p.size = Some((W as i32, H as i32).into()));
                server.state.toplevels[&id].send_configure();
                configured = true;
            }
            if server.state.mapped_windows.contains(&id) {
                break id;
            }
        }
        thread::yield_now();
    };
    let surface = server.state.surface_for_window(root).unwrap();
    assert_eq!(surface.client().unwrap().id(), qt_id);
    eprintln!("JWM_QT_STAGE=root_mapped");
    let local = server.state.seat.get_pointer().unwrap();
    let local_before = (
        local.current_focus(),
        local.current_location(),
        local.is_grabbed(),
        local.has_pressed_buttons(),
    );
    let keyboard = server.state.seat.get_keyboard().unwrap();
    let keyboard_before = (keyboard.current_focus(), keyboard.is_grabbed());
    let mut capture = Capture::new_with_size(&mut server, (W, H));
    let original = capture.source;
    let initial = frame(&mut server, &mut capture, original, &mut gl);
    assert!(
        (100..200).all(|y| (80..240).all(|x| {
            let p = &initial.pixels[((y * W + x) * 4) as usize..][..4];
            p[0] > 160 && p[1] < 100 && p[2] < 100 && p[3] == 255
        })),
        "Qt blue canvas"
    );
    callbacks(&surface);
    initial.retire(&mut server, &mut capture);
    eprintln!("JWM_QT_STAGE=initial_capture");
    // Dynamic lease creation strictly follows real root mapping and capture.
    let manager = capture.client.bind("jwm_remote_popup_manager_v1", 2);
    let lease = capture.client.new_id();
    capture.client.request(manager, 1, &[lease, original]);
    let events = fence(&mut server, &mut capture.client);
    let grant = events
        .iter()
        .find(|e| e.sender == lease && e.opcode == 0)
        .expect("Qt lease grant");
    assert_eq!(grant.args[1], 30000);
    let epoch = grant.args[0];
    let family = capture.client.new_id();
    capture.client.request(lease, 1, &[family]);
    fence(&mut server, &mut capture.client);
    let private = loop {
        server.roundtrip();
        let active = server
            .state
            .remote_popup
            .active
            .as_ref()
            .expect("Qt private seat revoked");
        assert_eq!(active.family.root.surface, surface);
        let pointer = active.seat.get_pointer().unwrap();
        assert!(Instant::now() < deadline, "Qt dynamic pointer bind timeout");
        if pointer.snapshot_private_pointer(qt_id.clone()).is_some() {
            break active.seat.clone();
        }
        assert!(Instant::now() < deadline, "Qt dynamic pointer bind timeout");
        thread::yield_now();
    };
    eprintln!("JWM_QT_STAGE=private_pointer_bound");
    let proof = frame(&mut server, &mut capture, family, &mut gl);
    capture.client.request(lease, 2, &[epoch, proof.id]);
    let events = fence(&mut server, &mut capture.client);
    assert!(
        events
            .iter()
            .any(|e| e.sender == lease && e.opcode == 1 && e.args == [epoch])
    );
    proof.retire(&mut server, &mut capture);
    let proof = frame(&mut server, &mut capture, family, &mut gl);
    capture
        .client
        .request(lease, 3, &[epoch, 1, proof.id, 1, 160, 24]);
    let events = fence(&mut server, &mut capture.client);
    assert!(
        events
            .iter()
            .any(|e| e.sender == lease && e.opcode == 2 && e.args == [epoch, 1])
    );
    proof.retire(&mut server, &mut capture);
    eprintln!("JWM_QT_STAGE=root_clicked");
    let mut observed_popup_states = 0u8;
    let popup = loop {
        server.roundtrip();
        let active = server
            .state
            .remote_popup
            .active
            .as_ref()
            .expect("Qt seat or serial incompatible");
        // Fixed one-time milestones only, never identity, geometry or serial.
        for (bit, observed, label) in [
            (1, active.popup.is_some(), "popup_role_created"),
            (2, active.grabbed, "popup_grab_accepted"),
            (4, active.popup_mapped, "popup_buffer_mapped"),
            (
                8,
                active
                    .popup
                    .as_ref()
                    .and_then(|surface| server.state.popups.get(&surface.id()))
                    .is_some_and(|popup| popup.is_initial_configure_sent()),
                "popup_initial_configure_sent",
            ),
        ] {
            if observed && observed_popup_states & bit == 0 {
                observed_popup_states |= bit;
                eprintln!("JWM_QT_STAGE={label}");
            }
        }
        assert!(Instant::now() < deadline, "Qt menu mapping timeout");
        if active.grabbed && active.popup_mapped {
            break active
                .family
                .snapshot(&server.state)
                .expect("Qt mapped family snapshot invalid")
                .layout
                .popup
                .clone()
                .expect("Qt mapped popup snapshot missing");
        }
        assert!(Instant::now() < deadline, "Qt menu mapping timeout");
        thread::yield_now();
    };
    eprintln!("JWM_QT_STAGE=popup_snapshot_ready");
    assert_eq!(popup.surface.client().unwrap().id(), qt_id);
    let (left, top, width, height) = popup.rect;
    assert!(
        width > 0
            && height > 0
            && left >= 0
            && top >= 0
            && left + width <= W as i32
            && top + height <= H as i32,
        "Qt menu outside canvas"
    );
    eprintln!("JWM_QT_STAGE=popup_geometry_valid");
    let root_pixels = frame(&mut server, &mut capture, original, &mut gl);
    eprintln!("JWM_QT_STAGE=popup_root_comparison_rendered");
    let proof = frame(&mut server, &mut capture, family, &mut gl);
    eprintln!("JWM_QT_STAGE=popup_family_rendered");
    let mut changed = 0;
    let mut unchanged = 0;
    for y in 48..H as i32 {
        for x in 0..W as i32 {
            let i = ((y * W as i32 + x) * 4) as usize;
            let a = &root_pixels.pixels[i..i + 4];
            let b = &proof.pixels[i..i + 4];
            if (left..left + width).contains(&x) && (top..top + height).contains(&y) {
                changed += usize::from(a != b);
            } else {
                assert_eq!(a, b, "Qt pixels outside popup");
                unchanged += 1;
            }
        }
    }
    assert!(
        changed > 0 && unchanged > 0,
        "Qt popup pixel evidence missing"
    );
    callbacks(&surface);
    callbacks(&popup.surface);
    root_pixels.retire(&mut server, &mut capture);
    eprintln!("JWM_QT_STAGE=popup_captured");
    capture.client.request(
        lease,
        3,
        &[
            epoch,
            2,
            proof.id,
            1,
            (left + width / 2) as u32,
            (top + height / 2) as u32,
        ],
    );
    let events = fence(&mut server, &mut capture.client);
    assert!(
        events
            .iter()
            .any(|e| e.sender == lease && e.opcode == 2 && e.args == [epoch, 2]),
        "Qt selection not delivered"
    );
    eprintln!("JWM_QT_STAGE=selection_delivered");
    let mut activated = 0;
    let mut revoked = events
        .iter()
        .any(|e| e.sender == lease && e.opcode == 3 && e.args == [super::SOURCE]);
    loop {
        server.roundtrip();
        activated += rx.try_iter().count();
        assert!(activated <= 1);
        revoked |= capture
            .client
            .events()
            .iter()
            .any(|e| e.sender == lease && e.opcode == 3 && e.args == [super::SOURCE]);
        assert!(
            Instant::now() < deadline,
            "Qt activation or terminal retirement timeout"
        );
        if activated == 1 && revoked && !super::active(&server.state) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Qt activation or terminal retirement timeout"
        );
        thread::yield_now();
    }
    assert!(
        !popup.surface.is_alive()
            || !smithay::backend::renderer::utils::with_renderer_surface_state(
                &popup.surface,
                |s| s.view().is_some()
            )
            .unwrap_or(false),
        "Qt popup must actually unmap or destroy after selection"
    );
    // This is the independent original read-only source, never the revoked
    // family. It observes the application's real post-QAction paint, not input.
    loop {
        assert!(Instant::now() < deadline, "Qt green canvas timeout");
        server.roundtrip();
        let painted = frame(&mut server, &mut capture, original, &mut gl);
        let green = (100..200).all(|y| {
            (80..240).all(|x| {
                let p = &painted.pixels[((y * W + x) * 4) as usize..][..4];
                p[1] > 160 && p[0] < 100 && p[2] < 100 && p[3] == 255
            })
        });
        callbacks(&surface);
        painted.retire(&mut server, &mut capture);
        assert!(Instant::now() < deadline, "Qt green canvas timeout");
        if green {
            break;
        }
    }
    assert!(private.get_pointer().is_none() && private.get_keyboard().is_none());
    assert_eq!(
        (
            local.current_focus(),
            local.current_location(),
            local.is_grabbed(),
            local.has_pressed_buttons()
        ),
        local_before
    );
    assert_eq!(
        (keyboard.current_focus(), keyboard.is_grabbed()),
        keyboard_before
    );
    let foreign = fence(&mut server, &mut other);
    assert!(
        !foreign
            .iter()
            .any(|e| e.sender == other_pointer || e.sender == 2 && e.opcode == 0),
        "foreign input or private global"
    );
    // No post-unmap family capture, renewed grant, or direct toolkit activation.
    assert!(Instant::now() < deadline, "Qt complete probe deadline");
    drop(process);
    reader.join().unwrap();
    activated += rx.try_iter().count();
    assert_eq!(activated, 1);
    eprintln!("JWM_QT_STAGE=activation_and_retirement_verified");
}
