//! Explicit one-shot text -> exact TIv3 commit+done -> unique application pixels.
//! Owned normal IME, authenticated TCP, two owned displays, production GL.
use super::*;
use crate::backend::wayland_udev::remote_text::{
    self,
    tests::{TextContext, events as wire_events},
};
use crate::remote::{
    client::{ClientOptions, run_client},
    host,
    viewer_backend::{ViewerBackend, with_wayland_test_stream},
    wayland_capture::WaylandCapture,
    wayland_text_wire::ExplicitText,
    wayland_viewer::with_wayland_control_frame_observer,
};
use std::{
    io::Write,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
const KEY: [u8; 32] = [0x83; 32];
struct Stop(Arc<AtomicBool>);
impl Drop for Stop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
fn host_options(allow: bool) -> host::HostOptions {
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
        allow_wayland_keyboard: false,
        allow_wayland_text: allow,
        allow_static_lossless: false,
        allow_wayland_pointer: false,
        allow_wayland_control: false,
        allow_resumable_control: false,
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
fn marker(word: u32) -> Vec<u8> {
    let mut bytes = vec![0; 800 * 600 * 4];
    for y in 0..600 {
        for x in 0..800 {
            let bit = (y / 75) * 8 + x / 100;
            let white = if bit < 32 {
                word >> bit & 1 == 1
            } else {
                word >> (bit - 32) & 1 == 0
            };
            let i = ((y * 800 + x) * 4) as usize;
            bytes[i..i + 3].fill(if white { 255 } else { 0 });
            bytes[i + 3] = 255;
        }
    }
    bytes
}
fn repaint(native: &mut Server, target: &mut App, word: u32) {
    let (buffer, mut file) = shm_buffer_file(native, &mut target.client, [0, 0, 0, 255]);
    file.seek(SeekFrom::Start(0)).unwrap();
    file.write_all(&marker(word)).unwrap();
    target.client.request(target.surface, 1, &[buffer, 0, 0]);
    target.client.request(target.surface, 2, &[0, 0, 800, 600]);
    target.client.request(target.surface, 6, &[]);
    native.roundtrip();
    native.roundtrip();
}
fn read_marker(
    local: &mut Server,
    gl: &mut HeadlessSmithayGles,
    reader: &mut Option<Capture>,
    word: u32,
) -> bool {
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
    assert!(
        wire_events(&mut capture.client)
            .iter()
            .any(|e| e.sender == frame && e.opcode == 3)
    );
    file.seek(SeekFrom::Start(0)).unwrap();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes.len(), 800 * 600 * 4);
    let red = bytes
        .chunks_exact(4)
        .all(|p| p[2] >= 220 && p[0] <= 35 && p[1] <= 35 && p[3] == 255);
    let expected = marker(word);
    let mut matches = true;
    for y in 2..598 {
        for x in 2..798 {
            // Leave JPEG ringing only around actual cell edges, not arbitrary pixels.
            if x % 100 < 8 || x % 100 > 92 || y % 75 < 8 || y % 75 > 67 {
                continue;
            }
            let i = (y * 800 + x) * 4;
            let white = expected[i] == 255;
            let p = &bytes[i..i + 4];
            matches &= p[3] == 255
                && if white {
                    p[..3].iter().all(|&c| c >= 220)
                } else {
                    p[..3].iter().all(|&c| c <= 35)
                };
        }
    }
    assert!(
        red || matches,
        "actual viewer must show selected red or exact response marker, never bystander"
    );
    capture.client.request(frame, 0, &[]);
    local.roundtrip();
    matches
}
fn pump(native: &mut Server, local: &mut Server, gl: &mut HeadlessSmithayGles) {
    native.roundtrip();
    fulfill_test_toplevel_capture(
        &mut gl.renderer,
        &mut None,
        &native.state,
        native.state.image_capture_pending.as_ref().unwrap(),
    );
    native.roundtrip();
    local.roundtrip();
    for (&window, top) in &local.state.toplevels {
        if !top.is_initial_configure_sent() {
            let geo = local.state.window_geometry.get_mut(&window).unwrap();
            geo.w = 800;
            geo.h = 600;
            top.with_pending_state(|s| s.size = Some((800, 600).into()));
            top.send_configure();
        }
    }
    local.roundtrip();
}
fn exercise(
    payload: &str,
    host_permission: bool,
    client_permission: bool,
    with_ime: bool,
    tag: u32,
) {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut native = Server::new();
    let mut local = Server::new();
    native.state.image_capture_pending = Some(init_image_copy_capture(&native.display.handle()));
    local.state.image_capture_pending = Some(init_image_copy_capture(&local.display.handle()));
    register_test_presentation(&mut local);
    let mut target = App::with_color(&mut native, [0, 0, 255, 255]);
    let context = TextContext::new(&mut native, &mut target, with_ime);
    let (server, stream) = UnixStream::pair().unwrap();
    native
        .display
        .handle()
        .insert_client(server, Arc::new(JwmClientState::default()))
        .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let _stop = Stop(stop.clone());
    let stopping = stop.clone();
    let (selected_tx, selected) = mpsc::channel();
    let (host_tx, host_done) = mpsc::channel();
    let host_thread = thread::spawn(move || {
        let result = (|| -> crate::remote::RemoteResult<()> {
            let capture = WaylandCapture::connect_test_stream(stream, 1, 0)?;
            selected_tx.send(())?;
            let deadline = Instant::now() + Duration::from_secs(8);
            let stream = loop {
                assert!(Instant::now() < deadline && !stopping.load(Ordering::Acquire));
                match listener.accept() {
                    Ok((s, _)) => break s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1))
                    }
                    Err(e) => return Err(e.into()),
                }
            };
            let peer = host::authenticate_peer(stream, &KEY)?;
            host::serve_authenticated(
                peer,
                &host_options(host_permission),
                &stopping,
                &Mutex::new(Some(host::GrantedCapture::Wayland(Box::new(capture)))),
            )
        })()
        .map_err(|e| e.to_string());
        let _ = host_tx.send(result);
    });
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        pump(&mut native, &mut local, &mut gl);
        if selected.try_recv().is_ok() {
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
    let mut other = App::with_color(&mut native, [0, 255, 0, 255]);
    let seat = other.client.bind("wl_seat", 7);
    let manager = other.client.bind("zwp_text_input_manager_v3", 1);
    let other_text = other.client.new_id();
    other.client.request(manager, 1, &[other_text, seat]);
    native.roundtrip();
    native.roundtrip();
    wire_events(&mut other.client);
    let (server, viewer_stream) = UnixStream::pair().unwrap();
    local
        .display
        .handle()
        .insert_client(server, Arc::new(JwmClientState::default()))
        .unwrap();
    let frames = Arc::new(Mutex::new((None, None)));
    let observer = frames.clone();
    let requested = client_permission.then(|| payload.to_owned());
    let (client_tx, client_done) = mpsc::channel();
    let client_thread = thread::spawn(move || {
        let opts = ClientOptions {
            viewer_backend: ViewerBackend::Wayland,
            viewer_wayland_display: None,
            wayland_keyboard: false,
            wayland_text: requested.map(|text| ExplicitText::new(text.into_bytes()).unwrap()),
            wayland_pointer: false,
            wayland_control: false,
            resumable_control: false,
            static_lossless: false,
            wayland_scroll: false,
            wayland_horizontal_scroll: false,
            wayland_popup_pointer: false,
            resize_source: None,
            follow_source_size: false,
            window_size: None,
            window_position: None,
            frame_limits: Default::default(),
            address: address.to_string(),
            peer_label: "owned one-shot text".into(),
            key: crate::remote::key::KeySource::Bytes(KEY.to_vec()),
            display: None,
            // No text permission means an actual legal read-only session,
            // not an invalid zero-mode client rejected before TCP connect.
            view_only: !client_permission,
            grab_input: false,
            seamless_input: false,
            clipboard: false,
            clipboard_mode: Default::default(),
            clipboard_direction: Default::default(),
            clipboard_file_limit: 0,
            escape_key: "Escape".into(),
            scaling: Default::default(),
        };
        let result = with_wayland_control_frame_observer(observer, || {
            with_wayland_test_stream(viewer_stream, || run_client(opts))
        })
        .map_err(|error| {
            let diagnostic = crate::remote::client::classify_failure(error.as_ref());
            // Keep the actual causal message separate from ClientFailure's
            // user-facing stage and troubleshooting-hint formatting.
            let mut cause: &(dyn std::error::Error + 'static) = error.as_ref();
            for _ in 0..32 {
                match cause.source() {
                    Some(source) => cause = source,
                    None => return (diagnostic, cause.to_string()),
                }
            }
            panic!("unexpected cyclic/oversized client error chain");
        });
        let _ = client_tx.send(result);
    });
    let positive = host_permission && client_permission && with_ime;
    let word = 0x711a0000 | tag;
    let mut reader = None;
    let mut pending_feedback = Vec::new();
    let mut first_presented = None;
    let mut marker_sequence = None;
    let mut commits = Vec::new();
    let mut repainted = false;
    let mut last_feedback = None;
    let mut host_result = None;
    let mut client_result = None;
    let held_until = Instant::now() + Duration::from_millis(120);
    loop {
        assert!(Instant::now() < deadline, "text TCP/GL finite completion");
        pump(&mut native, &mut local, &mut gl);
        let actual = context.take_commits(&mut target);
        if first_presented.is_none() {
            assert!(
                actual.is_empty(),
                "no text before genuine first-frame presentation"
            );
        }
        commits.extend(actual);
        assert!(
            !wire_events(&mut other.client)
                .iter()
                .any(|e| e.sender == other_text && matches!(e.opcode, 2..=5)),
            "bystander text-input must stay untouched"
        );
        if !positive {
            assert!(commits.is_empty());
        }
        if positive && !commits.is_empty() && !repainted {
            assert_eq!(commits, [payload]);
            repaint(&mut native, &mut target, word);
            repainted = true;
        }
        if let Some(top) = local.state.toplevels.values().next() {
            pending_feedback.extend(take_feedback(top.wl_surface()));
        }
        let sequence = frames.lock().unwrap().0;
        if Instant::now() >= held_until
            && marker_sequence.is_none()
            && !pending_feedback.is_empty()
            && sequence.is_some_and(|s| last_feedback.is_none_or(|old| s > old))
        {
            assert_eq!(pending_feedback.len(), 1);
            let seq = sequence.unwrap();
            let marker = read_marker(&mut local, &mut gl, &mut reader, word);
            assert_eq!(frames.lock().unwrap().0, Some(seq));
            if marker {
                assert!(repainted);
                marker_sequence = Some(seq);
            }
            present(&mut local, std::mem::take(&mut pending_feedback));
            last_feedback = Some(seq);
            first_presented.get_or_insert(seq);
        }
        if positive && marker_sequence.is_some_and(|seq| frames.lock().unwrap().1 == Some(seq)) {
            assert_eq!(commits, [payload]);
            assert!(marker_sequence > first_presented);
            break;
        }
        if host_result.is_none() {
            if let Ok(result) = host_done.try_recv() {
                host_result = Some(result);
            }
        }
        if client_result.is_none() {
            if let Ok(result) = client_done.try_recv() {
                client_result = Some(result);
            }
        }
        if !positive {
            if client_permission {
                if host_result.is_some() && client_result.is_some() {
                    let error = client_result
                        .as_ref()
                        .unwrap()
                        .as_ref()
                        .expect_err("text refusal must reach the requesting client");
                    let expected = if !host_permission {
                        "one-shot text permission was not mutually negotiated exclusively"
                    } else {
                        "host closed the remote session: host cannot capture its display: native text context unavailable or refused"
                    };
                    assert_eq!(error.0.stage, Some("negotiate"));
                    assert_eq!(
                        error.1, expected,
                        "unrelated transport failure is not a permission refusal"
                    );
                    if !host_permission {
                        assert_eq!(error.0.category, "io");
                        assert_eq!(error.0.io_kind, Some("invalid_data"));
                        assert_eq!(error.0.peer_close_code, None);
                    } else {
                        assert_eq!(error.0.category, "peer_closed");
                        assert_eq!(
                            error.0.peer_close_code,
                            Some(crate::remote::close_wire::SessionCloseCode::CaptureUnavailable)
                        );
                    }
                    break;
                }
            } else {
                assert!(
                    host_result.is_none() && client_result.is_none(),
                    "read-only negative control must remain connected"
                );
                if first_presented
                    .is_some_and(|seq| frames.lock().unwrap().1.is_some_and(|seen| seen >= seq))
                    && Instant::now() > held_until + Duration::from_millis(150)
                {
                    assert!(!remote_text::active(&native.state));
                    break;
                }
            }
        } else {
            assert!(
                host_result.is_none() && client_result.is_none(),
                "positive text endpoint ended early"
            );
        }
        thread::sleep(Duration::from_millis(1));
    }
    stop.store(true, Ordering::Release);
    let until = Instant::now() + Duration::from_secs(6);
    while host_result.is_none() || client_result.is_none() {
        assert!(Instant::now() < until, "owned text endpoint cleanup");
        pump(&mut native, &mut local, &mut gl);
        assert!(
            context.take_commits(&mut target).is_empty(),
            "text one-shot must not replay during cleanup"
        );
        if host_result.is_none() {
            if let Ok(r) = host_done.try_recv() {
                host_result = Some(r);
            }
        }
        if client_result.is_none() {
            if let Ok(r) = client_done.try_recv() {
                client_result = Some(r);
            }
        }
        thread::sleep(Duration::from_millis(1));
    }
    host_thread.join().unwrap();
    client_thread.join().unwrap();
    if !client_permission {
        assert!(
            host_result.as_ref().unwrap().is_ok(),
            "read-only host must stop normally: {host_result:?}"
        );
        let (diagnostic, reason) = client_result.as_ref().unwrap().as_ref().unwrap_err();
        assert_eq!(diagnostic.stage, Some("session"));
        assert_eq!(diagnostic.category, "peer_closed");
        assert_eq!(
            diagnostic.peer_close_code,
            Some(crate::remote::close_wire::SessionCloseCode::HostStopping)
        );
        assert_eq!(
            reason, "host closed the remote session: the host stopped sharing its desktop",
            "read-only completion must be the requested host stop, not an authorization error"
        );
        assert!(commits.is_empty());
        assert!(!remote_text::active(&native.state));
    }
}
#[test]
fn actual_text_tcp_chinese_combining_emoji_commit_done_then_unique_gl_presented() {
    for (index, text) in ["中文输入", "e\u{301}", "👩🏽‍💻🙂"].into_iter().enumerate()
    {
        exercise(text, true, true, true, index as u32 + 1);
    }
}
#[test]
fn actual_text_tcp_requires_both_permissions_and_existing_normal_ime() {
    for (host, client, ime) in [
        (false, true, true),
        (true, false, true),
        (true, true, false),
    ] {
        exercise("不得泄露", host, client, ime, 9);
    }
}
