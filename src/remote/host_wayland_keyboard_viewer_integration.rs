//! Actual native viewer -> authenticated TCP -> selected native keyboard.
//! Both displays and all input resources are owned by this test. Controlled
//! feedback follows real production GL capture, not a claimed DRM page flip.
use super::*;
use crate::backend::wayland_udev::remote_keyboard;
use crate::remote::{
    client::{ClientOptions, run_client},
    host,
    viewer_backend::{ViewerBackend, with_wayland_keyboard_observer, with_wayland_test_stream},
    wayland_capture::WaylandCapture,
};
use smithay::reexports::wayland_server::protocol::wl_keyboard::{KeyState, WlKeyboard};
use smithay::utils::SERIAL_COUNTER;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicU8, Ordering},
};
const KEY: [u8; 32] = [0x63; 32];
struct Stop(Arc<AtomicBool>);
impl Drop for Stop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
fn host_options() -> host::HostOptions {
    host::HostOptions {
        frame_limits: Default::default(),
        listen: "127.0.0.1:0".into(),
        key: crate::remote::key::KeySource::Bytes(KEY.to_vec()),
        display: None,
        wayland_select: true,
        wayland_resize_follow: false,
        wayland_display: None,
        fps: 12,
        jpeg_quality: 90,
        jpeg_quality_floor: 90,
        fixed_jpeg_quality: true,
        max_width: 0,
        capture_source: Default::default(),
        capture_area: crate::remote::x11_capture::CaptureArea::Root,
        shared_sources: Vec::new(),
        scaling: Default::default(),
        allow_lan: false,
        allow_input: false,
        allow_window_resize: false,
        allow_wayland_keyboard: true,
        allow_wayland_pointer: false,
        allow_wayland_control: false,
        allow_wayland_scroll: false,
        scroll_copy: false,
        allow_clipboard: false,
        clipboard_mode: Default::default(),
        clipboard_direction: Default::default(),
        clipboard_file_limit: 0,
        once: true,
        stats_json: false,
    }
}
fn bind_keyboard(server: &mut Server, app: &mut App) -> u32 {
    let seat = app.client.bind("wl_seat", 7);
    let keyboard = app.client.new_id();
    app.client.request(seat, 1, &[keyboard]);
    server.roundtrip();
    server.roundtrip();
    app.client.events();
    keyboard
}
fn collect(app: &mut App, keyboard: u32, keys: &mut Vec<(u32, u32)>) {
    for event in app.client.events() {
        assert!(
            !(event.sender == 1 && event.opcode == 0),
            "native wl_display error"
        );
        if event.sender == keyboard && event.opcode == 3 {
            keys.push((event.args[2], event.args[3]));
        }
    }
}
fn pump(host: &mut Server, viewer: &mut Server, gl: &mut HeadlessSmithayGles) {
    host.roundtrip();
    fulfill_test_toplevel_capture(
        &mut gl.renderer,
        &mut None,
        &host.state,
        host.state.image_capture_pending.as_ref().unwrap(),
    );
    host.roundtrip();
    viewer.roundtrip();
    for top in viewer.state.toplevels.values() {
        if !top.is_initial_configure_sent() {
            top.with_pending_state(|s| s.size = Some((800, 600).into()));
            top.send_configure();
        }
    }
    viewer.roundtrip();
}
fn key(k: &WlKeyboard, serial: u32, code: u32, pressed: bool) {
    k.key(
        serial,
        serial,
        code,
        if pressed {
            KeyState::Pressed
        } else {
            KeyState::Released
        },
    );
}

#[test]
fn actual_wayland_keyboard_viewer_tcp_requires_presented_and_targets_only_original_surface() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut native = Server::new();
    let mut local = Server::new();
    native.state.image_capture_pending = Some(init_image_copy_capture(&native.display.handle()));
    local.state.image_capture_pending = Some(init_image_copy_capture(&local.display.handle()));
    register_test_presentation(&mut local);
    // Explicit identical complete maps, not an assumption about desktop defaults.
    let context = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
    let keymap = xkbcommon::xkb::Keymap::new_from_names(
        &context,
        "",
        "",
        "us",
        "",
        None,
        xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .unwrap();
    let shift_index = keymap.mod_get_index(xkbcommon::xkb::MOD_NAME_SHIFT);
    let shift_mask = 1u32
        .checked_shl(shift_index)
        .expect("owned US map has a valid Shift modifier index");
    let map = keymap.get_as_string(xkbcommon::xkb::KEYMAP_FORMAT_TEXT_V1);
    for server in [&mut native, &mut local] {
        server
            .state
            .seat
            .get_keyboard()
            .unwrap()
            .set_keymap_from_string(&mut server.state, map.clone())
            .unwrap();
    }
    let mut target = App::with_color(&mut native, [0, 0, 255, 255]);
    let target_keyboard = bind_keyboard(&mut native, &mut target);
    let target_surface = native.state.surface_for_window(target.window).unwrap();
    native
        .state
        .set_keyboard_focus(Some(target_surface.clone()), SERIAL_COUNTER.next_serial());
    native.roundtrip();
    target.client.events();
    let (native_socket, capture_stream) = UnixStream::pair().unwrap();
    native
        .display
        .handle()
        .insert_client(native_socket, Arc::new(JwmClientState::default()))
        .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let shutdown = Arc::new(AtomicBool::new(false));
    let _stop = Stop(shutdown.clone());
    let (selected_tx, selected) = mpsc::channel();
    let (host_done_tx, host_done) = mpsc::channel();
    let host_shutdown = shutdown.clone();
    let host_thread = thread::spawn(move || {
        let result = (|| -> crate::remote::RemoteResult<()> {
            let capture = WaylandCapture::connect_test_stream(capture_stream, 1, 0)?;
            selected_tx.send(())?;
            let deadline = Instant::now() + Duration::from_secs(10);
            let stream = loop {
                assert!(
                    !host_shutdown.load(Ordering::Acquire) && Instant::now() < deadline,
                    "owned accept deadline"
                );
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1))
                    }
                    Err(e) => return Err(e.into()),
                }
            };
            let peer = host::authenticate_peer(stream, &KEY)?;
            host::serve_authenticated(
                peer,
                &host_options(),
                &host_shutdown,
                &Mutex::new(Some(host::GrantedCapture::Wayland(Box::new(capture)))),
            )
        })()
        .map_err(|e| e.to_string());
        host_done_tx.send(result).unwrap();
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pump(&mut native, &mut local, &mut gl);
        if selected.try_recv().is_ok() {
            break;
        }
        assert!(Instant::now() < deadline, "native capture selection");
        thread::sleep(Duration::from_millis(1));
    }
    let mut other = App::with_color(&mut native, [255, 0, 0, 255]);
    let other_keyboard = bind_keyboard(&mut native, &mut other);
    native
        .state
        .set_keyboard_focus(Some(target_surface), SERIAL_COUNTER.next_serial());
    native.roundtrip();
    target.client.events();
    other.client.events();
    let (server_socket, viewer_stream) = UnixStream::pair().unwrap();
    local
        .display
        .handle()
        .insert_client(server_socket, Arc::new(JwmClientState::default()))
        .unwrap();
    let observed = Arc::new(AtomicU8::new(0));
    let client_observed = observed.clone();
    let (client_tx, client_done) = mpsc::channel();
    let client_thread = thread::spawn(move || {
        let opts = ClientOptions {
            viewer_backend: ViewerBackend::Wayland,
            viewer_wayland_display: None,
            wayland_keyboard: true,
            wayland_pointer: false,
            wayland_control: false,
            wayland_scroll: false,
            resize_source: None,
            follow_source_size: false,
            window_size: None,
            window_position: None,
            frame_limits: Default::default(),
            address: address.to_string(),
            peer_label: "owned native keyboard".into(),
            key: crate::remote::key::KeySource::Bytes(KEY.to_vec()),
            display: None,
            view_only: false,
            grab_input: false,
            clipboard: false,
            clipboard_mode: Default::default(),
            clipboard_direction: Default::default(),
            clipboard_file_limit: 0,
            escape_key: "Escape".into(),
        };
        let result = with_wayland_keyboard_observer(client_observed, || {
            with_wayland_test_stream(viewer_stream, || run_client(opts))
        })
        .map_err(|e| e.to_string());
        client_tx.send(result).unwrap();
    });
    let mut viewer_keyboard = None;
    let mut initial_feedback = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    let surface = loop {
        pump(&mut native, &mut local, &mut gl);
        if let Some(top) = local.state.toplevels.values().next() {
            let surface = top.wl_surface().clone();
            if viewer_keyboard.is_none() {
                let resources: Vec<_> = local
                    .state
                    .seat
                    .get_keyboard()
                    .unwrap()
                    .client_keyboards(&surface.client().unwrap())
                    .collect();
                if let Some(k) = resources.first() {
                    assert_eq!(resources.len(), 1);
                    k.enter(1, &surface, Vec::new());
                    k.modifiers(2, 0, 0, 0, 0);
                    viewer_keyboard = Some(k.clone());
                    local.roundtrip();
                }
            }
            initial_feedback.extend(take_feedback(&surface));
            if viewer_keyboard.is_some() && !initial_feedback.is_empty() {
                break surface;
            }
        }
        if let Ok(result) = client_done.try_recv() {
            panic!("client ended before first feedback: {result:?}");
        }
        assert!(Instant::now() < deadline, "native viewer startup");
        thread::sleep(Duration::from_millis(1));
    };
    let keyboard = viewer_keyboard.unwrap();
    let mut captured = Capture::new(&mut local);
    verify_actual_gl_pixels(&mut local, &mut gl, &mut captured);
    let mut received = Vec::new();
    let mut unexpected = Vec::new();
    local.roundtrip();
    let until = Instant::now() + Duration::from_millis(150);
    while Instant::now() < until {
        pump(&mut native, &mut local, &mut gl);
        collect(&mut target, target_keyboard, &mut received);
        collect(&mut other, other_keyboard, &mut unexpected);
        assert!(received.is_empty() && unexpected.is_empty());
        assert_eq!(
            remote_keyboard::attempts_for_test(&native.state),
            0,
            "no request before real presentation"
        );
        assert_eq!(observed.load(Ordering::Acquire), 0);
        thread::sleep(Duration::from_millis(1));
    }
    present(&mut local, initial_feedback);
    let deadline = Instant::now() + Duration::from_secs(5);
    while observed.load(Ordering::Acquire) != 1 {
        pump(&mut native, &mut local, &mut gl);
        if let Ok(result) = client_done.try_recv() {
            panic!("client ended before lease activation: {result:?}");
        }
        assert!(
            Instant::now() < deadline,
            "real granted and activated state"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(remote_keyboard::attempts_for_test(&native.state), 1);
    for (index, (code, down)) in [
        (30, true),
        (30, false),
        (42, true),
        (30, true),
        (30, false),
        (42, false),
    ]
    .into_iter()
    .enumerate()
    {
        let serial = 20 + index as u32 * 2;
        key(&keyboard, serial, code, down);
        if code == 42 {
            keyboard.modifiers(serial + 1, if down { shift_mask } else { 0 }, 0, 0, 0);
        }
    }
    local.roundtrip();
    let expected = vec![(30, 1), (30, 0), (42, 1), (30, 1), (30, 0), (42, 0)];
    let deadline = Instant::now() + Duration::from_secs(5);
    while received.len() < expected.len() {
        pump(&mut native, &mut local, &mut gl);
        collect(&mut target, target_keyboard, &mut received);
        collect(&mut other, other_keyboard, &mut unexpected);
        assert!(unexpected.is_empty());
        assert!(
            Instant::now() < deadline,
            "balanced A and Shift+A reach native recipient"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(received, expected);
    // Leave is terminal for the native adapter. Even an immediate clean enter
    // and balanced key pair in the same dispatch cannot revive that authority.
    keyboard.leave(40, &surface);
    keyboard.enter(41, &surface, Vec::new());
    keyboard.modifiers(42, 0, 0, 0, 0);
    key(&keyboard, 43, 30, true);
    key(&keyboard, 44, 30, false);
    local.roundtrip();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        pump(&mut native, &mut local, &mut gl);
        collect(&mut target, target_keyboard, &mut received);
        collect(&mut other, other_keyboard, &mut unexpected);
        assert_eq!(received, expected);
        assert!(unexpected.is_empty());
        if let Ok(result) = client_done.try_recv() {
            let error = result.expect_err("native focus loss must close, not revive, the session");
            assert!(
                error.contains("Wayland keyboard focus left"),
                "expected explicit focus-loss failure, not panic/timeout: {error}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "explicit native focus-loss termination"
        );
        thread::sleep(Duration::from_millis(1));
    }
    shutdown.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut host_finished = false;
    loop {
        pump(&mut native, &mut local, &mut gl);
        if let Ok(result) = host_done.try_recv() {
            // The client intentionally ended with the exact failure above;
            // transport EOF/reset is not itself a successful input assertion.
            eprintln!("owned native host after explicit viewer failure: {result:?}");
            host_finished = true;
        }
        collect(&mut target, target_keyboard, &mut received);
        collect(&mut other, other_keyboard, &mut unexpected);
        assert_eq!(received, expected);
        assert!(unexpected.is_empty());
        assert_eq!(
            remote_keyboard::attempts_for_test(&native.state),
            1,
            "no renewed native lease request after focus enter"
        );
        if host_finished && !remote_keyboard::active(&native.state) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "native host must finish and revoke active lease"
        );
        thread::sleep(Duration::from_millis(1));
    }
    client_thread.join().unwrap();
    host_thread.join().unwrap();
}
