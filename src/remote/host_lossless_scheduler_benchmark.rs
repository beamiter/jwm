//! Explicit private synthetic TCP/GL measurement, not a performance gate.
//! Legacy and proposed policies share input, clocks, validation, and setup.
use super::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn marker_pixels(generation: usize) -> Vec<u8> {
    assert!(generation < 16);
    let mut bytes = pattern(false);
    let gray = generation ^ (generation >> 1);
    for y in 0..16usize {
        for x in 0..16usize {
            let bit = (y / 8) * 2 + x / 8;
            let value = if gray >> bit & 1 == 0 { 32 } else { 224 };
            let i = (y * 800 + x) * 4;
            bytes[i..i + 4].copy_from_slice(&[value, value, value, 255]);
        }
    }
    bytes
}
fn marker_id(bytes: &[u8]) -> Option<usize> {
    if bytes.len() != 800 * 600 * 4 {
        return None;
    }
    let mut gray = 0usize;
    for bit in 0..4 {
        let x0 = (bit % 2) * 8 + 3;
        let y0 = (bit / 2) * 8 + 3;
        let mut dark = true;
        let mut light = true;
        for y in y0..y0 + 2 {
            for x in x0..x0 + 2 {
                let p = &bytes[(y * 800 + x) * 4..(y * 800 + x) * 4 + 4];
                if p[3] != 255 {
                    return None;
                }
                dark &= p[..3].iter().all(|&c| c < 70);
                light &= p[..3].iter().all(|&c| c > 185);
            }
        }
        if dark == light {
            return None;
        }
        if light {
            gray |= 1 << bit;
        }
    }
    let mut binary = gray;
    let mut shift = gray >> 1;
    while shift != 0 {
        binary ^= shift;
        shift >>= 1;
    }
    Some(binary)
}
fn input_hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn elapsed_ns(base: Instant, at: Instant) -> u64 {
    u64::try_from(at.duration_since(base).as_nanos()).unwrap()
}
fn paint_timed(server: &mut Server, app: &mut App, bytes: &[u8]) -> (Instant, Instant, Instant) {
    let started = Instant::now();
    let (buffer, mut file) = shm_buffer_file(server, &mut app.client, [0, 0, 0, 255]);
    file.seek(SeekFrom::Start(0)).unwrap();
    file.write_all(bytes).unwrap();
    app.client.request(app.surface, 1, &[buffer, 0, 0]);
    app.client.request(app.surface, 2, &[0, 0, 800, 600]);
    let issued = Instant::now();
    app.client.request(app.surface, 6, &[]);
    server.roundtrip();
    server.roundtrip();
    (started, issued, Instant::now())
}
fn host_stats(probe: &host::LosslessScheduleProbe) -> Value {
    let s = probe.snapshot();
    json!({"encoded_jpeg":s.encoded_jpeg,"encoded_png":s.encoded_png,"encode_ns":s.encode_ns,
        "jpeg_ns":s.jpeg_ns,"png_ns":s.png_ns,"sent_bytes":s.sent_bytes,"comparisons":s.comparisons,
        "comparison_ns":s.comparison_ns,"stale_png_dropped":s.stale_png_dropped})
}
fn update_presented(report: &mut Value, base: Instant, ledger: &[(u64, Instant)]) {
    for frame in report["frames"].as_array_mut().unwrap() {
        let seq = frame["sequence"].as_u64().unwrap();
        if let Some((_, at)) = ledger.iter().find(|(s, _)| *s == seq) {
            frame["presented_ns"] = json!(elapsed_ns(base, *at));
        }
    }
}
fn run_session(arm: &str, report: &mut Value) {
    let enabled = arm != "off";
    let policy = match arm {
        "legacy" => host::LosslessSchedule::Legacy100,
        "off" | "proposed" => host::LosslessSchedule::New250,
        "quiet0_pace100" => host::LosslessSchedule::Quiet0Pace100,
        "quiet0_pace250" => host::LosslessSchedule::Quiet0Pace250,
        "quiet250_pace100" => host::LosslessSchedule::Quiet250Pace100,
        "quiet250_pace250" => host::LosslessSchedule::Quiet250Pace250,
        _ => panic!("unknown explicit benchmark policy"),
    };
    let probe = Arc::new(host::LosslessScheduleProbe::default());
    let inputs: Vec<_> = (0..16).map(marker_pixels).collect();
    report["markers"]=json!((0..16).map(|generation|json!({"generation":generation,"scheduled_ns":generation as u64*500_000_000,"source_sha256":input_hash(&inputs[generation]),"paint_started_ns":null,"commit_issued_ns":null,"commit_completed_ns":null})).collect::<Vec<_>>());
    let mut gl = HeadlessSmithayGles::new().expect("explicit benchmark requires owned software GL");
    let mut native = Server::new();
    let mut local = Server::new();
    native.state.image_capture_pending = Some(init_image_copy_capture(&native.display.handle()));
    local.state.image_capture_pending = Some(init_image_copy_capture(&local.display.handle()));
    register_test_presentation(&mut local);
    let mut target = App::with_color(&mut native, [0, 0, 255, 255]);
    let expected = pattern(false);
    repaint(&mut native, &mut target, &expected);
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
    let host_probe = probe.clone();
    let host_thread = thread::spawn(move || {
        let result = host::with_lossless_schedule(policy, host_probe, || {
            (|| -> crate::remote::RemoteResult<()> {
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
                    &host_options(enabled),
                    &stopping,
                    &Mutex::new(Some(host::GrantedCapture::Wayland(Box::new(capture)))),
                )
            })()
        })
        .map_err(|e| e.to_string());
        let _ = host_tx.send(result);
    });
    let deadline = Instant::now() + Duration::from_secs(20);
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
    let codecs = Arc::new(Mutex::new(Vec::new()));
    let codec_observer = codecs.clone();
    let (client_tx, client_done) = mpsc::channel();
    let client_thread = thread::spawn(move || {
        let opts = ClientOptions {
            viewer_backend: ViewerBackend::Wayland,
            viewer_wayland_display: None,
            wayland_keyboard: false,
            wayland_text: None,
            static_lossless: enabled,
            wayland_pointer: false,
            wayland_control: false,
            wayland_scroll: false,
            wayland_horizontal_scroll: false,
            wayland_popup_pointer: false,
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
    // Finish initial JPEG presentation before the experimental scheduled clock.
    loop {
        pump(&mut native, &mut local, &mut gl);
        if let Some(top) = local.state.toplevels.values().next() {
            feedback.extend(take_feedback(top.wl_surface()));
        }
        if !feedback.is_empty() {
            assert_eq!(feedback.len(), 1);
            let seq = frames.lock().unwrap().0.unwrap();
            let _ = read_pixels(&mut local, &mut gl, &mut reader);
            assert_eq!(frames.lock().unwrap().0, Some(seq));
            present(&mut local, std::mem::take(&mut feedback));
            last = Some(seq);
        }
        if last.is_some_and(|seq| frames.lock().unwrap().1 == Some(seq)) {
            break;
        }
        assert!(Instant::now() < deadline, "benchmark initial presentation");
        thread::sleep(Duration::from_millis(1));
    }
    report["baseline_sequence"] = json!(last.unwrap());
    report["host_stats_scope"] = json!(
        "whole session including common startup and shutdown; sent_bytes source-envelope payload, not wire traffic; encode totals include later discarded frames"
    );
    let base = Instant::now();
    let mut next = 0usize;
    let end = base + Duration::from_secs(9); // eight seconds scheduled work + one bounded drain
    while Instant::now() < end {
        while next < 16 && Instant::now() >= base + Duration::from_millis(next as u64 * 500) {
            let (started, issued, completed) = paint_timed(&mut native, &mut target, &inputs[next]);
            let row = &mut report["markers"][next];
            row["paint_started_ns"] = json!(elapsed_ns(base, started));
            row["commit_issued_ns"] = json!(elapsed_ns(base, issued));
            row["commit_completed_ns"] = json!(elapsed_ns(base, completed));
            next += 1;
        }
        pump(&mut native, &mut local, &mut gl);
        if let Some(top) = local.state.toplevels.values().next() {
            feedback.extend(take_feedback(top.wl_surface()));
        }
        if !feedback.is_empty()
            && frames
                .lock()
                .unwrap()
                .0
                .is_some_and(|seq| last.is_none_or(|old| seq > old))
        {
            assert_eq!(feedback.len(), 1);
            let seq = frames.lock().unwrap().0.unwrap();
            let gl_started = Instant::now();
            let bytes = read_pixels(&mut local, &mut gl, &mut reader);
            let gl_completed = Instant::now();
            assert_eq!(frames.lock().unwrap().0, Some(seq));
            let generation = marker_id(&bytes);
            let error = static_sse(&bytes, &expected);
            let validated = Instant::now();
            let png = codecs
                .lock()
                .unwrap()
                .iter()
                .find(|(s, _)| *s == seq)
                .expect("real decode evidence")
                .1;
            let feedback_started = Instant::now();
            present(&mut local, std::mem::take(&mut feedback));
            let feedback_sent = Instant::now();
            let rows = report["frames"].as_array_mut().unwrap();
            assert!(rows.len() < 512);
            rows.push(json!({"sequence":seq,"is_png":png,"marker_generation":generation,
                "gl_started_ns":elapsed_ns(base,gl_started),"gl_completed_ns":elapsed_ns(base,gl_completed),
                "validation_completed_ns":elapsed_ns(base,validated),"feedback_started_ns":elapsed_ns(base,feedback_started),"feedback_sent_ns":elapsed_ns(base,feedback_sent),
                "presented_ns":null,"static_sse":error}));
            last = Some(seq);
        }
        update_presented(report, base, &presented_times.lock().unwrap());
        report["host_stats"] = host_stats(&probe);
        if let Ok(result) = host_done.try_recv() {
            panic!("benchmark host ended early: {result:?}");
        }
        if let Ok(result) = client_done.try_recv() {
            panic!("benchmark viewer ended early: {result:?}");
        }
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        next, 16,
        "all scheduled source generations remain in denominator"
    );
    stop.store(true, Ordering::Release);
    let until = Instant::now() + Duration::from_secs(6);
    let mut host_result = None;
    let mut client_result = None;
    while host_result.is_none() || client_result.is_none() {
        assert!(Instant::now() < until, "benchmark endpoint cleanup");
        pump(&mut native, &mut local, &mut gl);
        if host_result.is_none() {
            host_result = host_done.try_recv().ok();
        }
        if client_result.is_none() {
            client_result = client_done.try_recv().ok();
        }
        update_presented(report, base, &presented_times.lock().unwrap());
        thread::sleep(Duration::from_millis(1));
    }
    host_thread.join().unwrap();
    client_thread.join().unwrap();
    assert!(host_result.unwrap().is_ok());
    let (diagnostic, cause) = client_result.unwrap().unwrap_err();
    assert_eq!(
        diagnostic.peer_close_code,
        Some(crate::remote::close_wire::SessionCloseCode::HostStopping)
    );
    assert_eq!(
        cause,
        "host closed the remote session: the host stopped sharing its desktop"
    );
    report["host_stats"] = host_stats(&probe);
    report["host_records"]=json!(probe.records().iter().map(|r|json!({"sequence":r.sequence,"source_epoch":r.source_epoch,"geometry_epoch":r.geometry_epoch,"is_png":r.png,"bytes":r.bytes,"encode_ns":r.encode_ns,"jpeg_ns":r.jpeg_ns,"png_ns":r.png_ns})).collect::<Vec<_>>());
    if !enabled {
        assert!(
            report["frames"]
                .as_array()
                .unwrap()
                .iter()
                .all(|f| f["is_png"] == false)
        );
    }
    report["success"] = json!(true);
}
#[test]
#[ignore = "explicit private synthetic real TCP/GL schedule comparison; no performance threshold"]
fn actual_static_lossless_scheduler_three_arm_measurement() {
    let order = ["off", "legacy", "proposed", "proposed", "legacy", "off"];
    let mut report = json!({"schema_version":1,"benchmark":"static_lossless_scheduler_tcp_ab","scope":"same-process synthetic owned displays and controlled GL presentation; not physical display latency","scheduled_interval_ns":500_000_000u64,"drain_ns":1_000_000_000u64,"success":true,"sessions":[]});
    let mut failed = false;
    for (index, arm) in order.iter().enumerate() {
        let mut session = json!({"arm":arm,"order":index,"success":false,"failure":null,"markers":[],"frames":[],"host_stats":null,"host_records":[]});
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_session(arm, &mut session)
        }));
        if let Err(error) = result {
            failed = true;
            let cause = error
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| error.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "non-string test panic".into());
            session["failure"] = json!(cause);
        }
        report["sessions"].as_array_mut().unwrap().push(session);
    }
    let sessions = report["sessions"].as_array().unwrap();
    for generation in 0..16 {
        let hash = &sessions[0]["markers"][generation]["source_sha256"];
        if hash.is_null()
            || sessions
                .iter()
                .any(|session| &session["markers"][generation]["source_sha256"] != hash)
        {
            failed = true;
            report["input_identity_failure"] = json!(true);
            break;
        }
    }
    report["success"] = json!(!failed);
    let path = std::env::var("JWM_REMOTE_SCHEDULER_REPORT")
        .expect("explicit private report path required");
    let bytes = serde_json::to_vec_pretty(&report).unwrap();
    assert!(bytes.len() <= 2 * 1024 * 1024);
    std::fs::write(path, bytes).unwrap();
    assert!(
        !failed,
        "measurement infrastructure failed; raw private report retains every session"
    );
}
#[test]
#[ignore = "explicit private synthetic real TCP/GL schedule comparison; no performance threshold"]
fn actual_static_lossless_scheduler_factorial_measurement() {
    let order = [
        "quiet0_pace100",
        "quiet0_pace250",
        "quiet250_pace100",
        "quiet250_pace250",
        "quiet250_pace250",
        "quiet250_pace100",
        "quiet0_pace250",
        "quiet0_pace100",
    ];
    let mut report = json!({"schema_version":1,"benchmark":"static_lossless_scheduler_factorial_tcp_ab","scope":"same-process synthetic owned displays and controlled GL presentation; not physical display latency","scheduled_interval_ns":500_000_000u64,"drain_ns":1_000_000_000u64,"success":true,"sessions":[]});
    let mut failed = false;
    for (index, arm) in order.iter().enumerate() {
        let mut session = json!({"arm":arm,"order":index,"success":false,"failure":null,"markers":[],"frames":[],"host_stats":null,"host_records":[]});
        session["policy"] = match *arm {
            "quiet0_pace100" => json!({"quiet_ms":0,"pace_ms":100,"supersession_guard":true}),
            "quiet0_pace250" => json!({"quiet_ms":0,"pace_ms":250,"supersession_guard":true}),
            "quiet250_pace100" => json!({"quiet_ms":250,"pace_ms":100,"supersession_guard":true}),
            "quiet250_pace250" => json!({"quiet_ms":250,"pace_ms":250,"supersession_guard":true}),
            _ => unreachable!(),
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_session(arm, &mut session)
        }));
        if let Err(error) = result {
            failed = true;
            let cause = error
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| error.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "non-string test panic".into());
            session["failure"] = json!(cause);
        }
        report["sessions"].as_array_mut().unwrap().push(session);
    }
    let sessions = report["sessions"].as_array().unwrap();
    for generation in 0..16 {
        let hash = &sessions[0]["markers"][generation]["source_sha256"];
        if hash.is_null()
            || sessions
                .iter()
                .any(|session| &session["markers"][generation]["source_sha256"] != hash)
        {
            failed = true;
            report["input_identity_failure"] = json!(true);
            break;
        }
    }
    report["success"] = json!(!failed);
    let path = std::env::var("JWM_REMOTE_SCHEDULER_REPORT")
        .expect("explicit private report path required");
    let bytes = serde_json::to_vec_pretty(&report).unwrap();
    assert!(bytes.len() <= 2 * 1024 * 1024);
    std::fs::write(path, bytes).unwrap();
    assert!(
        !failed,
        "measurement infrastructure failed; raw private report retains every session"
    );
}
#[test]
fn unique_gray_markers_survive_production_jpeg_and_reject_uncertain_cells() {
    use crate::remote::{
        tile::{TileDecoder, TileEncodeRequest, TileEncoder},
        x11_capture::CapturedFrame,
    };
    for generation in 0..16 {
        let bytes = marker_pixels(generation);
        assert_eq!(marker_id(&bytes), Some(generation));
        let rgb = image::RgbImage::from_fn(800, 600, |x, y| {
            let i = ((y * 800 + x) * 4) as usize;
            image::Rgb([bytes[i + 2], bytes[i + 1], bytes[i]])
        });
        let frame = CapturedFrame {
            geometry_epoch: 1,
            source_width: 800,
            source_height: 600,
            image: rgb,
        };
        let mut encoder = TileEncoder::new();
        let mut decoder = TileDecoder::new();
        encoder.plan(&frame, TileEncodeRequest::Keyframe).unwrap();
        let mut payload = Vec::new();
        encoder.encode_into(&mut payload, 0, &frame, 60).unwrap();
        let decoded = decoder
            .decode_into(&payload, crate::remote::frame::new_decode_buffer_pool())
            .unwrap();
        let mut actual = Vec::with_capacity(800 * 600 * 4);
        for pixel in decoded.image().pixels() {
            actual.extend([pixel[2], pixel[1], pixel[0], 255]);
        }
        assert_eq!(
            marker_id(&actual),
            Some(generation),
            "real JPEG classifier generation {generation}"
        );
    }
    let mut uncertain = marker_pixels(0);
    for y in 3..5 {
        for x in 3..5 {
            let i = (y * 800 + x) * 4;
            uncertain[i..i + 3].fill(127);
        }
    }
    assert_eq!(marker_id(&uncertain), None);
}
