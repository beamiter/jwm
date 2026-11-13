//! Explicit keyboard-only lease survives visual PNG repair, never reauthorized by it.
//! Both displays and all input resources are owned by this test. Controlled
//! feedback follows real production GL capture, not a claimed DRM page flip.
use super::*;
use crate::backend::wayland_udev::remote_keyboard;
use crate::remote::{
    client::{
        ClientOptions, run_client, with_frame_purpose_observer, with_static_lossless_frame_observer,
    },
    host,
    viewer_backend::{ViewerBackend, with_wayland_keyboard_observer, with_wayland_test_stream},
    wayland_capture::WaylandCapture,
    wayland_viewer::with_wayland_control_frame_observer,
};
use smithay::reexports::wayland_server::protocol::wl_keyboard::{KeyState, WlKeyboard};
use smithay::utils::SERIAL_COUNTER;
use std::io::Write;
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
        wayland_popups: false,
        wayland_resize_follow: false,
        wayland_display: None,
        fps: 12,
        jpeg_quality: 60,
        jpeg_quality_floor: 60,
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
        allow_resumable_control: false,
        allow_wayland_text: false,
        allow_static_lossless: true,
        allow_wayland_scroll: false,
        allow_wayland_horizontal_scroll: false,
        allow_wayland_popup_pointer: false,
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

fn pattern() -> Vec<u8> {
    let mut bytes = vec![0; 800 * 600 * 4];
    for y in 0..600usize {
        for x in 0..800usize {
            let rgb = if x % 13 == 0 || y % 19 == 0 || (x % 13 == 8 && y % 19 < 12) {
                [23, 191, 47]
            } else {
                [211, 35, 173]
            };
            bytes[(y * 800 + x) * 4..(y * 800 + x) * 4 + 4]
                .copy_from_slice(&[rgb[2], rgb[1], rgb[0], 255]);
        }
    }
    bytes
}

fn repaint(server: &mut Server, app: &mut App, bytes: &[u8]) {
    let (buffer, mut file) = shm_buffer_file(server, &mut app.client, [0, 0, 0, 255]);
    file.seek(SeekFrom::Start(0)).unwrap();
    file.write_all(bytes).unwrap();
    app.client.request(app.surface, 1, &[buffer, 0, 0]);
    app.client.request(app.surface, 2, &[0, 0, 800, 600]);
    app.client.request(app.surface, 6, &[]);
    server.roundtrip();
    server.roundtrip();
}
fn read_pixels(
    local: &mut Server,
    gl: &mut HeadlessSmithayGles,
    reader: &mut Option<Capture>,
) -> Vec<u8> {
    read_pixels_or_stopped(local, gl, reader).expect("capture must be ready")
}

// Only the expiry caller may consider an explicit Stopped terminal event.
fn read_pixels_or_stopped(
    local: &mut Server,
    gl: &mut HeadlessSmithayGles,
    reader: &mut Option<Capture>,
) -> Option<Vec<u8>> {
    if reader.is_none() {
        *reader = Some(Capture::new(local));
    }
    let capture = reader.as_mut().unwrap();
    let (buffer, mut file) = shm_buffer_file(local, &mut capture.client, [0x5a; 4]);
    let frame = capture.client.new_id();
    capture.client.request(capture.session, 0, &[frame]);
    capture.client.request(frame, 1, &[buffer]);
    capture.client.request(frame, 3, &[]);
    local.roundtrip();
    local.roundtrip();
    fulfill_test_toplevel_capture(
        &mut gl.renderer,
        &mut None,
        &local.state,
        local.state.image_capture_pending.as_ref().unwrap(),
    );
    local.roundtrip();
    local.roundtrip();
    let events = capture.client.events();
    assert!(!events.iter().any(|e| e.sender == 1 && e.opcode == 0));
    let ready = events
        .iter()
        .filter(|e| e.sender == frame && e.opcode == 3)
        .count();
    let failed: Vec<_> = events
        .iter()
        .filter(|e| e.sender == frame && e.opcode == 4)
        .collect();
    if !failed.is_empty() {
        assert_eq!(ready, 0, "conflicting capture terminals");
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].args, [2], "only explicit Stopped is eligible");
        capture.client.request(frame, 0, &[]);
        local.roundtrip();
        return None;
    }
    assert_eq!(ready, 1, "capture terminal missing");
    file.seek(SeekFrom::Start(0)).unwrap();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes.len(), 800 * 600 * 4);
    capture.client.request(frame, 0, &[]);
    local.roundtrip();
    Some(bytes)
}

fn expiry_capture_stop_is_proven(
    failed_at: Duration,
    ended_at: Duration,
    ttl_error: bool,
    inactive: bool,
    released: bool,
) -> bool {
    (Duration::from_secs(29)..=Duration::from_secs(40)).contains(&failed_at)
        && (Duration::from_secs(29)..=Duration::from_secs(40)).contains(&ended_at)
        && ttl_error
        && inactive
        && released
}

#[test]
fn expiry_capture_stop_requires_all_terminal_evidence() {
    let t = Duration::from_secs(30);
    assert!(expiry_capture_stop_is_proven(t, t, true, true, true));
    for (error, inactive, released) in [
        (false, true, true),
        (true, false, true),
        (true, true, false),
    ] {
        assert!(!expiry_capture_stop_is_proven(
            t, t, error, inactive, released
        ));
    }
    for bad in [
        Duration::from_secs(28),
        Duration::from_secs(40) + Duration::from_nanos(1),
    ] {
        assert!(!expiry_capture_stop_is_proven(bad, t, true, true, true));
        assert!(!expiry_capture_stop_is_proven(t, bad, true, true, true));
    }
}

#[derive(Clone, Copy, Debug)]
enum Termination {
    LocalFocus,
    HostLock,
    HostUnmap,
    Expiry,
}
#[test]
fn actual_keyboard_lossless_png_preserves_held_release_and_targets_only_original_surface() {
    run(Termination::LocalFocus);
}
#[test]
fn actual_keyboard_lossless_lock_unlock_cannot_revive_lease() {
    run(Termination::HostLock);
}
#[test]
fn actual_keyboard_lossless_unmap_remap_cannot_revive_lease() {
    run(Termination::HostUnmap);
}
#[test]
fn actual_keyboard_lossless_png_does_not_renew_real_thirty_second_lease() {
    run(Termination::Expiry);
}
fn run(termination: Termination) {
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
    let expected_pixels = pattern();
    repaint(&mut native, &mut target, &expected_pixels);
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
    let codecs = Arc::new(Mutex::new(Vec::new()));
    let client_codecs = codecs.clone();
    let frames = Arc::new(Mutex::new((None, None)));
    let client_frames = frames.clone();
    let purposes = Arc::new(Mutex::new(Vec::new()));
    let client_purposes = purposes.clone();
    let (client_tx, client_done) = mpsc::channel();
    let exact_ttl_error = Arc::new(AtomicBool::new(false));
    let client_ttl_error = exact_ttl_error.clone();
    let client_thread = thread::spawn(move || {
        let opts = ClientOptions {
            viewer_backend: ViewerBackend::Wayland,
            viewer_wayland_display: None,
            wayland_keyboard: true,
            wayland_pointer: false,
            wayland_control: false,
            resumable_control: false,
            wayland_seamless_input: false,
            wayland_text: None,
            static_lossless: true,
            wayland_scroll: false,
            wayland_horizontal_scroll: false,
            wayland_popup_pointer: false,
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
            seamless_input: false,
            clipboard: false,
            clipboard_mode: Default::default(),
            clipboard_direction: Default::default(),
            clipboard_file_limit: 0,
            escape_key: "Escape".into(),
            scaling: Default::default(),
        };
        let result = with_frame_purpose_observer(client_purposes, || {
            with_wayland_keyboard_observer(client_observed, || {
                with_static_lossless_frame_observer(client_codecs, || {
                    with_wayland_control_frame_observer(client_frames, || {
                        with_wayland_test_stream(viewer_stream, || run_client(opts))
                    })
                })
            })
        })
        .map_err(|e| {
            let diagnostic = crate::remote::client::classify_failure(e.as_ref());
            let mut leaf: &(dyn std::error::Error + 'static) = e.as_ref();
            for _ in 0..32 {
                match leaf.source() {
                    Some(next) => leaf = next,
                    None => break,
                }
            }
            client_ttl_error.store(
                diagnostic.stage == Some("session")
                    && diagnostic.cleanup == "unknown"
                    && leaf.source().is_none()
                    && leaf.downcast_ref::<std::io::Error>().is_some_and(|error| {
                        error.kind() == std::io::ErrorKind::InvalidData
                            && error.to_string() == "keyboard lease deadline expired; not renewed"
                    }),
                Ordering::Release,
            );
            e.to_string()
        });
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
    let mut reader = None;
    let initial_pixels = read_pixels(&mut local, &mut gl, &mut reader);
    assert_ne!(
        initial_pixels, expected_pixels,
        "initial JPEG must really be lossy"
    );
    let initial_seq = frames.lock().unwrap().0.unwrap();
    assert_eq!(
        purposes
            .lock()
            .unwrap()
            .iter()
            .find(|(seq, _)| *seq == initial_seq)
            .unwrap()
            .1,
        crate::remote::source_wire::FramePurpose::NativeProof
    );
    assert!(
        !codecs
            .lock()
            .unwrap()
            .iter()
            .find(|(seq, _)| *seq == initial_seq)
            .unwrap()
            .1
    );
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
    let activated_at = Instant::now();
    // Hold a real accepted key while PNG refinement is decoded and actually
    // presented. Repair must neither renew the lease nor lose its release.
    key(&keyboard, 10, 30, true);
    local.roundtrip();
    let deadline = Instant::now() + Duration::from_secs(18);
    let mut exact_sequence = None;
    let mut saw_png = false;
    loop {
        pump(&mut native, &mut local, &mut gl);
        collect(&mut target, target_keyboard, &mut received);
        collect(&mut other, other_keyboard, &mut unexpected);
        assert!(unexpected.is_empty());
        assert!(received.is_empty() || received == vec![(30, 1)]);
        assert_eq!(remote_keyboard::attempts_for_test(&native.state), 1);
        assert!(remote_keyboard::active(&native.state));
        let feedback = take_feedback(&surface);
        if !feedback.is_empty() {
            assert_eq!(feedback.len(), 1);
            let sequence = frames.lock().unwrap().0.unwrap();
            let actual = read_pixels(&mut local, &mut gl, &mut reader);
            assert_eq!(frames.lock().unwrap().0, Some(sequence));
            let png = codecs
                .lock()
                .unwrap()
                .iter()
                .find(|(seq, _)| *seq == sequence)
                .unwrap()
                .1;
            saw_png |= png;
            assert!(
                !actual
                    .chunks_exact(4)
                    .any(|p| p[0] > 230 && p[1] < 20 && p[2] < 20),
                "bystander blue leaked"
            );
            if actual == expected_pixels && png {
                exact_sequence = Some(sequence);
            }
            present(&mut local, feedback);
        }
        if exact_sequence.is_some_and(|seq| frames.lock().unwrap().1 == Some(seq))
            && received == vec![(30, 1)]
        {
            break;
        }
        if let Ok(result) = client_done.try_recv() {
            panic!("viewer ended during PNG refinement: {result:?}");
        }
        if let Ok(result) = host_done.try_recv() {
            panic!("host ended during PNG refinement: {result:?}");
        }
        assert!(
            Instant::now() < deadline,
            "full exact PNG must present while original key is held"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert!(saw_png);
    assert_eq!(
        purposes
            .lock()
            .unwrap()
            .iter()
            .find(|(seq, _)| Some(*seq) == exact_sequence)
            .unwrap()
            .1,
        crate::remote::source_wire::FramePurpose::VisualRepair
    );
    key(&keyboard, 12, 30, false);
    local.roundtrip();
    let deadline = Instant::now() + Duration::from_secs(5);
    while received.len() < 2 {
        pump(&mut native, &mut local, &mut gl);
        collect(&mut target, target_keyboard, &mut received);
        collect(&mut other, other_keyboard, &mut unexpected);
        assert!(unexpected.is_empty());
        assert!(
            Instant::now() < deadline,
            "held release after actual PNG must reach original recipient"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(received, vec![(30, 1), (30, 0)]);
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
    let expected = vec![
        (30, 1),
        (30, 0),
        (30, 1),
        (30, 0),
        (42, 1),
        (30, 1),
        (30, 0),
        (42, 0),
    ];
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
    if matches!(termination, Termination::Expiry) {
        key(&keyboard, 35, 48, true);
        local.roundtrip();
        let deadline = activated_at + Duration::from_secs(40);
        let mut next_change = Instant::now() + Duration::from_secs(3);
        let mut revision = 0_u8;
        let mut png_after_hold = 0;
        let mut done_client = false;
        let mut done_host = false;
        let mut capture_stopped_at = None;
        loop {
            if Instant::now() >= next_change && remote_keyboard::active(&native.state) {
                revision = revision.checked_add(1).unwrap();
                let mut image = expected_pixels.clone();
                image[2] += revision;
                repaint(&mut native, &mut target, &image);
                next_change += Duration::from_secs(3);
            }
            pump(&mut native, &mut local, &mut gl);
            collect(&mut target, target_keyboard, &mut received);
            collect(&mut other, other_keyboard, &mut unexpected);
            assert!(unexpected.is_empty());
            assert_eq!(remote_keyboard::attempts_for_test(&native.state), 1);
            let feedback = take_feedback(&surface);
            if !feedback.is_empty() && capture_stopped_at.is_none() {
                let seq = frames.lock().unwrap().0.unwrap();
                if let Some(pixels) = read_pixels_or_stopped(&mut local, &mut gl, &mut reader) {
                    assert_eq!(pixels.len(), expected_pixels.len());
                    if codecs
                        .lock()
                        .unwrap()
                        .iter()
                        .find(|(s, _)| *s == seq)
                        .unwrap()
                        .1
                    {
                        png_after_hold += 1;
                    }
                    present(&mut local, feedback);
                } else {
                    capture_stopped_at = Some(activated_at.elapsed());
                    // Failed readback is neither PNG nor presentation evidence.
                }
            }
            if let Ok(result) = client_done.try_recv() {
                let error = result.expect_err(
                    "only explicit local lease deadline may end viewer before test shutdown",
                );
                assert!(
                    error.contains("keyboard lease deadline expired; not renewed"),
                    "unexpected expiry client failure: {error}"
                );
                done_client = true;
            }
            if let Ok(result) = host_done.try_recv() {
                assert!(
                    activated_at.elapsed() >= Duration::from_secs(29),
                    "host ended before lease expiry window: {result:?}"
                );
                done_host = true;
            }
            let mut released = expected.clone();
            released.extend([(48, 1), (48, 0)]);
            assert!(
                received == expected
                    || received == [expected.as_slice(), &[(48, 1)]].concat()
                    || received == released
            );
            if !remote_keyboard::active(&native.state)
                && received == released
                && (capture_stopped_at.is_none() || done_client)
            {
                if let Some(failed_at) = capture_stopped_at {
                    assert!(expiry_capture_stop_is_proven(
                        failed_at,
                        activated_at.elapsed(),
                        done_client && exact_ttl_error.load(Ordering::Acquire),
                        !remote_keyboard::active(&native.state),
                        received == released
                    ));
                }
                assert!(
                    activated_at.elapsed() >= Duration::from_secs(29),
                    "unexpected premature revoke, not TTL evidence"
                );
                assert!(
                    png_after_hold > 0,
                    "real PNG traffic must occur while expiry key is held"
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "real 30s lease must expire and release original held key despite PNG"
            );
            thread::sleep(Duration::from_millis(1));
        }
        // Restore local state and send another edge: neither a new proof nor
        // continuing repair traffic may renew the already-ended lease.
        if !done_client {
            key(&keyboard, 80, 48, false);
            key(&keyboard, 81, 30, true);
            key(&keyboard, 82, 30, false);
            local.roundtrip();
        }
        shutdown.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done_client || !done_host {
            pump(&mut native, &mut local, &mut gl);
            let feedback = take_feedback(&surface);
            if !feedback.is_empty() {
                present(&mut local, feedback);
            }
            collect(&mut target, target_keyboard, &mut received);
            collect(&mut other, other_keyboard, &mut unexpected);
            assert_eq!(remote_keyboard::attempts_for_test(&native.state), 1);
            assert_eq!(received.len(), expected.len() + 2);
            assert!(unexpected.is_empty());
            if client_done.try_recv().is_ok() {
                done_client = true;
            }
            if host_done.try_recv().is_ok() {
                done_host = true;
            }
            assert!(Instant::now() < deadline, "expiry test teardown completes");
            thread::sleep(Duration::from_millis(1));
        }
        client_thread.join().unwrap();
        host_thread.join().unwrap();
        return;
    }
    // Each case restores the superficial state, but never the original lease.
    match termination {
        Termination::LocalFocus => {
            keyboard.leave(40, &surface);
            keyboard.enter(41, &surface, Vec::new());
            keyboard.modifiers(42, 0, 0, 0, 0);
            key(&keyboard, 43, 30, true);
            key(&keyboard, 44, 30, false);
            local.roundtrip();
        }
        Termination::HostLock => {
            native.state.session_lock_supported = true;
            let mut locker = native.connect();
            let manager = locker.bind("ext_session_lock_manager_v1", 1);
            let lease = locker.new_id();
            locker.request(manager, 1, &[lease]);
            native.roundtrip();
            native.roundtrip();
            assert!(native.state.session_locked);
            locker.request(lease, 2, &[]);
            native.roundtrip();
            native.roundtrip();
            assert!(!native.state.session_locked);
        }
        Termination::Expiry => unreachable!("expiry handled above"),
        Termination::HostUnmap => {
            target.unmap(&mut native);
            target.remap(&mut native);
        }
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        pump(&mut native, &mut local, &mut gl);
        collect(&mut target, target_keyboard, &mut received);
        collect(&mut other, other_keyboard, &mut unexpected);
        assert_eq!(received, expected);
        assert!(unexpected.is_empty());
        if let Ok(result) = client_done.try_recv() {
            let error = result.expect_err("native focus loss must close, not revive, the session");
            let expected_cause = match termination {
                Termination::LocalFocus => "Wayland keyboard focus left",
                Termination::Expiry => unreachable!("expiry handled above"),
                Termination::HostLock | Termination::HostUnmap => "host capture became unavailable",
            };
            assert!(
                error.contains(expected_cause),
                "expected {termination:?} cause {expected_cause}, not unrelated failure: {error}"
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
