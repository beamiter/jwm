//! Opt-in static PNG refinement through authenticated TCP and actual viewer GL.
//! Exact pixels and real controlled presentation, not physical display latency.
use super::*;
use crate::remote::{
    client::{ClientOptions, run_client, with_static_lossless_frame_observer},
    host,
    viewer_backend::{ViewerBackend, with_wayland_test_stream},
    wayland_capture::WaylandCapture,
    wayland_viewer::{with_wayland_control_frame_observer, with_wayland_presented_time_observer},
};
use std::{
    io::Write,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
const KEY: [u8; 32] = [0x94; 32];
struct Stop(Arc<AtomicBool>);
impl Drop for Stop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
fn host_options(allow: bool) -> host::HostOptions {
    host::HostOptions {
        allow_static_lossless: allow,
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
        allow_wayland_keyboard: false,
        allow_wayland_text: false,
        allow_wayland_pointer: false,
        allow_wayland_control: false,
        allow_wayland_scroll: false,
        allow_wayland_horizontal_scroll: false,
        scroll_copy: false,
        allow_clipboard: false,
        clipboard_mode: Default::default(),
        clipboard_direction: Default::default(),
        clipboard_file_limit: 0,
        once: true,
        stats_json: false,
    }
}

fn pattern(revised: bool) -> Vec<u8> {
    let mut bytes = vec![0; 800 * 600 * 4];
    for y in 0..600usize {
        for x in 0..800usize {
            // Repeated glyph-like strokes, chromatic fine lines, and smooth areas.
            // Values avoid clipping so a +1 edit is exact and below JPEG tolerance.
            let stroke = (x % 13 == 0 || y % 19 == 0) || ((x % 13 == 8) && y % 19 < 12);
            let rgb = if stroke {
                [23, 191, 47]
            } else {
                [211, 35, 173]
            };
            let i = (y * 800 + x) * 4;
            bytes[i..i + 4].copy_from_slice(&[rgb[2], rgb[1], rgb[0], 255]);
            if revised && x < 64 && y < 64 {
                bytes[i + 2] += 1;
            }
        }
    }
    bytes
}
// Exactly one native 16x16 codec tile. No neighboring edge pixels are
// excluded from the static-region equality assertion.
const BLINK_SIDE: usize = 16;
fn blink_pattern(generation: usize) -> Vec<u8> {
    let mut bytes = pattern(false);
    let value = if generation % 2 == 0 { 24 } else { 232 };
    for y in 0..BLINK_SIDE {
        for x in 0..BLINK_SIDE {
            bytes[(y * 800 + x) * 4..(y * 800 + x) * 4 + 4]
                .copy_from_slice(&[value, value, value, 255]);
        }
    }
    bytes
}
fn static_sse(actual: &[u8], expected: &[u8]) -> u64 {
    assert_eq!(actual.len(), expected.len());
    actual
        .chunks_exact(4)
        .zip(expected.chunks_exact(4))
        .enumerate()
        .filter(|(index, _)| index % 800 >= BLINK_SIDE || index / 800 >= BLINK_SIDE)
        .map(|(_, (a, b))| sse(a, b))
        .sum()
}
fn blink_matches(bytes: &[u8], generation: usize) -> bool {
    // JPEG motion is deliberately not called lossless. Interior pixels must
    // unambiguously identify this black/white state, never an arbitrary hue.
    (4..12).all(|y| {
        (4..12).all(|x| {
            let p = &bytes[(y * 800 + x) * 4..(y * 800 + x) * 4 + 4];
            p[3] == 255
                && p[..3]
                    .iter()
                    .all(|&v| if generation % 2 == 0 { v < 70 } else { v > 185 })
        })
    })
}
fn observe_blink_presentation(
    candidate: Option<(usize, u64)>,
    deadline: Option<Instant>,
    ledger: &[(u64, Instant)],
    observed: &mut Option<usize>,
) {
    let Some((generation, seq)) = candidate else {
        return;
    };
    let Some((_, at)) = ledger.iter().find(|(s, _)| *s == seq) else {
        return;
    };
    let deadline = deadline.expect("blink candidate has scheduled deadline");
    assert!(
        *at <= deadline,
        "real blink presentation exceeded fixed deadline: generation={generation} sequence={seq} late_by={:?}",
        at.saturating_duration_since(deadline)
    );
    if *observed != Some(generation) {
        eprintln!(
            "blink actual-present generation={generation} sequence={seq} deadline_margin={:?}",
            deadline.duration_since(*at)
        );
    }
    *observed = Some(generation);
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
    assert!(events.iter().any(|e| e.sender == frame && e.opcode == 3));
    file.seek(SeekFrom::Start(0)).unwrap();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes.len(), 800 * 600 * 4);
    capture.client.request(frame, 0, &[]);
    local.roundtrip();
    bytes
}
fn sse(actual: &[u8], expected: &[u8]) -> u64 {
    assert_eq!(actual.len(), expected.len());
    actual
        .iter()
        .zip(expected)
        .map(|(&a, &b)| {
            let d = i64::from(a) - i64::from(b);
            (d * d) as u64
        })
        .sum()
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
// Bounded, content-free failure evidence. No raw pixels, peer text, or paths.
struct BlinkStageDiagnostics {
    base: Instant,
    sources: Vec<serde_json::Value>,
    draws: Vec<serde_json::Value>,
    omitted_draws: usize,
    presented: Arc<Mutex<Vec<(u64, Instant)>>>,
}
impl BlinkStageDiagnostics {
    fn ns(&self, at: Instant) -> u64 {
        u64::try_from(at.saturating_duration_since(self.base).as_nanos()).unwrap_or(u64::MAX)
    }
}
impl Drop for BlinkStageDiagnostics {
    fn drop(&mut self) {
        if !thread::panicking() {
            return;
        }
        let ledger = self.presented.lock().unwrap_or_else(|p| p.into_inner());
        for row in &mut self.draws {
            let seq = row["sequence"].as_u64();
            if let Some((_, at)) = ledger.iter().find(|(seen, _)| Some(*seen) == seq) {
                row["actual_presented_ns"] = serde_json::json!(
                    u64::try_from(at.saturating_duration_since(self.base).as_nanos())
                        .unwrap_or(u64::MAX)
                );
            }
        }
        eprintln!(
            "blink-stage-diagnostic {}",
            serde_json::json!({
                "schema_version":1,"clock":"same_process_monotonic",
                "source_schedule":"generation0 baseline established after first GL; later generations fixed 500ms",
                "sources":self.sources,"draws":self.draws,"omitted_draws":self.omitted_draws
            })
        );
    }
}

fn exercise(host_permission: bool, client_permission: bool) {
    exercise_workload(host_permission, client_permission, false);
}
fn exercise_workload(host_permission: bool, client_permission: bool, blinking: bool) {
    let diagnostic_base = Instant::now();
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut native = Server::new();
    let mut local = Server::new();
    native.state.image_capture_pending = Some(init_image_copy_capture(&native.display.handle()));
    local.state.image_capture_pending = Some(init_image_copy_capture(&local.display.handle()));
    register_test_presentation(&mut local);
    let mut target = App::with_color(&mut native, [0, 0, 255, 255]);
    let mut expected = if blinking {
        blink_pattern(0)
    } else {
        pattern(false)
    };
    let initial_repaint_started = Instant::now();
    repaint(&mut native, &mut target, &expected);
    let initial_repaint_completed = Instant::now();
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
    let deadline = Instant::now() + Duration::from_secs(18);
    loop {
        pump(&mut native, &mut local, &mut gl);
        if selected.try_recv().is_ok() {
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
    let (server, viewer_stream) = UnixStream::pair().unwrap();
    local
        .display
        .handle()
        .insert_client(server, Arc::new(JwmClientState::default()))
        .unwrap();
    let frames = Arc::new(Mutex::new((None, None)));
    let observer = frames.clone();
    let presented_times = Arc::new(Mutex::new(Vec::new()));
    let presented_observer = presented_times.clone();
    let mut diagnostic = blinking.then(|| BlinkStageDiagnostics {
        base: diagnostic_base,
        sources: Vec::with_capacity(28),
        draws: Vec::with_capacity(256),
        omitted_draws: 0,
        presented: presented_times.clone(),
    });
    if let Some(trace) = diagnostic.as_mut() {
        trace
            .sources
            .push(serde_json::json!({"generation":0,"scheduled_ns":null,
            "repaint_started_ns":trace.ns(initial_repaint_started),
            "repaint_completed_ns":trace.ns(initial_repaint_completed)}));
    }
    let codecs = Arc::new(Mutex::new(Vec::new()));
    let codec_observer = codecs.clone();
    let (client_tx, client_done) = mpsc::channel();
    let client_thread = thread::spawn(move || {
        let opts = ClientOptions {
            viewer_backend: ViewerBackend::Wayland,
            viewer_wayland_display: None,
            wayland_keyboard: false,
            wayland_text: None,
            static_lossless: client_permission,
            wayland_pointer: false,
            wayland_control: false,
            wayland_scroll: false,
            wayland_horizontal_scroll: false,
            resize_source: None,
            follow_source_size: false,
            window_size: None,
            window_position: None,
            frame_limits: Default::default(),
            address: address.to_string(),
            peer_label: "owned static lossless".into(),
            key: crate::remote::key::KeySource::Bytes(KEY.to_vec()),
            display: None,
            view_only: true,
            grab_input: false,
            clipboard: false,
            clipboard_mode: Default::default(),
            clipboard_direction: Default::default(),
            clipboard_file_limit: 0,
            escape_key: "Escape".into(),
            scaling: Default::default(),
        };
        let result = with_static_lossless_frame_observer(codec_observer, || {
            with_wayland_presented_time_observer(presented_observer, || {
                with_wayland_control_frame_observer(observer, || {
                    with_wayland_test_stream(viewer_stream, || run_client(opts))
                })
            })
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

    let mut reader = None;
    let mut feedback = Vec::new();
    let mut last = None;
    let mut first = None;
    let mut exact = None;
    let mut phase = 0;
    let mut host_result = None;
    let mut client_result = None;
    let mut phase_started = Instant::now();
    let mut last_sse = None;
    let mut minimum_sse = u64::MAX;
    let mut first_difference = None;
    let mut blink_generation = 0usize;
    let mut blink_observed = None;
    let mut blink_candidate = None;
    let mut blink_next: Option<Instant> = None;
    let mut static_recovered = false;
    loop {
        if Instant::now() >= deadline {
            let encoded = codecs.lock().unwrap();
            let png_count = encoded.iter().filter(|(_, png)| *png).count();
            panic!(
                "bounded static-lossless TCP/GL completion: phase={phase} phase_elapsed={:?} last_gl_sequence={last:?} exact_sequence={exact:?} committed_presented={:?} decoded_frames={} png_frames={png_count} last_sse={last_sse:?} minimum_sse={minimum_sse} first_difference={first_difference:?} blink_generation={blink_generation} blink_observed={blink_observed:?} blink_candidate={blink_candidate:?} static_recovered={static_recovered} host={host_result:?} client={client_result:?}",
                phase_started.elapsed(),
                *frames.lock().unwrap(),
                encoded.len()
            );
        }
        // Consume timestamped actual presentation before inspecting the fixed
        // source deadline. A cached generation number can lag the receiver;
        // an actual late timestamp must still fail, never be rounded away.
        if blinking {
            let ledger = presented_times.lock().unwrap();
            observe_blink_presentation(blink_candidate, blink_next, &ledger, &mut blink_observed);
            if exact.is_some_and(|seq| ledger.iter().any(|(seen, _)| *seen == seq)) {
                static_recovered = true;
            }
            if static_recovered && blink_generation >= 7 && blink_observed == Some(blink_generation)
            {
                break;
            }
        }
        if blinking && blink_next.is_some_and(|at| Instant::now() >= at) {
            // Fixed cadence: never postpone a source update until feedback.
            // A missing generation is a failure, not silently merged evidence.
            assert_eq!(
                blink_observed,
                Some(blink_generation),
                "blink generation {blink_generation} not actually presented within 500ms; candidate={blink_candidate:?} frames={:?}",
                *frames.lock().unwrap()
            );
            blink_generation += 1;
            assert!(
                blink_generation < 28,
                "static region failed to converge under continuous blinking"
            );
            expected = blink_pattern(blink_generation);
            let repaint_started = Instant::now();
            repaint(&mut native, &mut target, &expected);
            let repaint_completed = Instant::now();
            if let Some(trace) = diagnostic.as_mut() {
                trace
                    .sources
                    .push(serde_json::json!({"generation":blink_generation,
                    "scheduled_ns":blink_next.map(|at| trace.ns(at)),
                    "repaint_started_ns":trace.ns(repaint_started),
                    "repaint_completed_ns":trace.ns(repaint_completed)}));
            }
            blink_candidate = None;
            blink_next = blink_next.map(|at| at + Duration::from_millis(500));
        }
        pump(&mut native, &mut local, &mut gl);
        if let Some(top) = local.state.toplevels.values().next() {
            feedback.extend(take_feedback(top.wl_surface()));
        }
        let seq = frames.lock().unwrap().0;
        if !feedback.is_empty() && seq.is_some_and(|s| last.is_none_or(|old| s > old)) {
            assert_eq!(feedback.len(), 1);
            let seq = seq.unwrap();
            let gl_started = Instant::now();
            let pixels = read_pixels(&mut local, &mut gl, &mut reader);
            let gl_completed = Instant::now();
            assert_eq!(frames.lock().unwrap().0, Some(seq));
            let encoded = codecs.lock().unwrap();
            let is_png = encoded
                .iter()
                .find(|(s, _)| *s == seq)
                .expect("actual decoded frame codec evidence")
                .1;
            let has_png_evidence = encoded.iter().any(|(seen, png)| *seen <= seq && *png);
            drop(encoded);
            // The owned snapshot and its unique feedback belong to this exact
            // completed GL commit. Compositor presentation is not gated by the
            // test oracle's subsequent CPU cost; every oracle assertion remains.
            let feedback_started = Instant::now();
            present(&mut local, std::mem::take(&mut feedback));
            let feedback_completed = Instant::now();
            let check_started = Instant::now();
            let error = if blinking {
                static_sse(&pixels, &expected)
            } else {
                sse(&pixels, &expected)
            };
            if blinking {
                if static_recovered {
                    assert_eq!(
                        error, 0,
                        "static exact pixels regressed during one-tile blink"
                    );
                }
                if blink_candidate.is_none() && blink_matches(&pixels, blink_generation) {
                    blink_candidate = Some((blink_generation, seq));
                }
            }
            last_sse = Some(error);
            minimum_sse = minimum_sse.min(error);
            first_difference = pixels
                .iter()
                .zip(&expected)
                .enumerate()
                .find(|(_, (actual, expected))| actual != expected)
                .map(|(index, (&actual, &expected))| (index, actual, expected));
            eprintln!(
                "static-lossless GL phase={phase} elapsed={:?} sequence={seq} png={is_png} sse={error} first_difference={first_difference:?}",
                phase_started.elapsed()
            );
            if first.is_none() {
                assert!(!is_png, "initial frame must remain JPEG");
                assert!(error > 0, "workload must expose real JPEG loss");
                let now = Instant::now();
                first = Some(now);
                if blinking {
                    blink_next = Some(now + Duration::from_millis(500));
                    if let Some(trace) = diagnostic.as_mut() {
                        trace.sources[0]["scheduled_ns"] = serde_json::json!(trace.ns(now));
                    }
                }
            }
            if error == 0 {
                assert!(host_permission && client_permission);
                assert!(
                    has_png_evidence,
                    "exact recovery requires negotiated PNG evidence"
                );
                exact = Some(seq);
            }
            let checked = Instant::now();
            if let Some(trace) = diagnostic.as_mut() {
                if trace.draws.len() < 256 {
                    trace.draws.push(serde_json::json!({"generation_observed":blink_generation,
                        "sequence":seq,"png":is_png,
                        "matched_candidate":blink_candidate == Some((blink_generation,seq)),
                        "scheduled_deadline_ns":blink_next.map(|at|trace.ns(at)),
                        "gl_started_ns":trace.ns(gl_started),"gl_completed_ns":trace.ns(gl_completed),
                        "check_started_ns":trace.ns(check_started),"check_completed_ns":trace.ns(checked),
                        "feedback_started_ns":trace.ns(feedback_started),
                        "feedback_completed_ns":trace.ns(feedback_completed),"actual_presented_ns":null}));
                } else {
                    trace.omitted_draws += 1;
                }
            }
            last = Some(seq);
        }
        if blinking {
            observe_blink_presentation(
                blink_candidate,
                blink_next,
                &presented_times.lock().unwrap(),
                &mut blink_observed,
            );
            if exact.is_some_and(|seq| {
                presented_times
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(seen, _)| *seen == seq)
            }) {
                static_recovered = true;
            }
            if static_recovered && blink_generation >= 7 && blink_observed == Some(blink_generation)
            {
                break;
            }
        } else if let Some(seq) = exact {
            if presented_times
                .lock()
                .unwrap()
                .iter()
                .any(|(seen, _)| *seen == seq)
            {
                if phase == 0 {
                    phase = 1;
                    expected = pattern(true);
                    repaint(&mut native, &mut target, &expected);
                    phase_started = Instant::now();
                    last_sse = None;
                    minimum_sse = u64::MAX;
                    first_difference = None;
                    exact = None;
                } else {
                    break;
                }
            }
        }
        if !client_permission && first.is_some_and(|at| at.elapsed() > Duration::from_secs(3)) {
            assert!(codecs.lock().unwrap().iter().all(|(_, png)| !*png));
            assert!(frames.lock().unwrap().1.is_some());
            break;
        }
        if host_result.is_none() {
            host_result = host_done.try_recv().ok();
        }
        if client_result.is_none() {
            client_result = client_done.try_recv().ok();
        }
        if client_permission && !host_permission && host_result.is_some() && client_result.is_some()
        {
            let (diagnostic, reason) = client_result.as_ref().unwrap().as_ref().unwrap_err();
            assert_eq!(diagnostic.stage, Some("negotiate"));
            assert_eq!(diagnostic.category, "io");
            assert_eq!(diagnostic.io_kind, Some("invalid_data"));
            assert_eq!(
                reason,
                "static lossless permission was not mutually negotiated exclusively"
            );
            assert!(codecs.lock().unwrap().is_empty());
            assert!(first.is_none());
            break;
        }
        if host_permission || !client_permission {
            assert!(
                host_result.is_none() && client_result.is_none(),
                "live test endpoints ended early: {host_result:?} {client_result:?}"
            );
        }
        thread::sleep(Duration::from_millis(1));
    }
    stop.store(true, Ordering::Release);
    let cleanup = Instant::now() + Duration::from_secs(6);
    while host_result.is_none() || client_result.is_none() {
        assert!(Instant::now() < cleanup, "bounded owned endpoint shutdown");
        pump(&mut native, &mut local, &mut gl);
        if host_result.is_none() {
            host_result = host_done.try_recv().ok();
        }
        if client_result.is_none() {
            client_result = client_done.try_recv().ok();
        }
        thread::sleep(Duration::from_millis(1));
    }
    host_thread.join().unwrap();
    client_thread.join().unwrap();
    if client_permission && !host_permission {
        return;
    }
    assert!(host_result.unwrap().is_ok());
    let (diagnostic, reason) = client_result.unwrap().unwrap_err();
    assert_eq!(diagnostic.stage, Some("session"));
    assert_eq!(
        diagnostic.peer_close_code,
        Some(crate::remote::close_wire::SessionCloseCode::HostStopping)
    );
    assert_eq!(
        reason,
        "host closed the remote session: the host stopped sharing its desktop"
    );
}
#[test]
fn actual_static_lossless_tcp_recovers_exact_gl_and_subtolerance_edit() {
    exercise(true, true);
}
#[test]
fn actual_static_lossless_tcp_default_off_never_sends_png() {
    exercise(false, false);
    exercise(true, false);
}
#[test]
fn actual_static_lossless_tcp_requires_host_permission() {
    exercise(false, true);
}

#[test]
fn exact_pixel_sse_detects_single_channel_subtolerance_edit() {
    let a = pattern(false);
    let b = pattern(true);
    assert_eq!(sse(&a, &a), 0);
    assert_eq!(sse(&a, &b), 64 * 64);
}

#[test]
fn actual_static_lossless_tcp_blinking_tile_does_not_starve_exact_text() {
    exercise_workload(true, true, true);
}
#[test]
fn blink_exclusion_is_exactly_one_tile_not_neighboring_edges() {
    let a = blink_pattern(0);
    let mut b = a.clone();
    b[0] += 1;
    assert_eq!(static_sse(&a, &b), 0);
    b[16 * 4] += 1;
    assert_eq!(static_sse(&a, &b), 1);
    b[(16 * 800) * 4] += 1;
    assert_eq!(static_sse(&a, &b), 2);
    assert!(blink_matches(&a, 0));
    assert!(!blink_matches(&a, 1));
}

#[test]
fn blink_actual_timestamp_not_poll_order_decides_deadline() {
    let deadline = Instant::now();
    let mut observed = Some(7);
    observe_blink_presentation(
        Some((8, 16)),
        Some(deadline),
        &[
            (16, deadline - Duration::from_millis(1)),
            (17, deadline + Duration::from_millis(2)),
        ],
        &mut observed,
    );
    assert_eq!(observed, Some(8));
    let late = std::panic::catch_unwind(|| {
        let mut observed = None;
        observe_blink_presentation(
            Some((8, 16)),
            Some(deadline),
            &[
                (16, deadline + Duration::from_nanos(1)),
                (17, deadline + Duration::from_millis(2)),
            ],
            &mut observed,
        );
    });
    assert!(
        late.is_err(),
        "even real 1ns lateness must not be hidden by reordering"
    );
}

#[path = "host_lossless_scheduler_benchmark.rs"]
mod scheduler_benchmark;

#[path = "host_lossless_scroll_integration.rs"]
mod scroll_integration;
