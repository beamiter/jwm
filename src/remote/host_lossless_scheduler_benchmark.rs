//! Explicit private synthetic TCP/GL measurement, not a performance gate.
//! Legacy and proposed policies share input, clocks, validation, and setup.
use super::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const CONTINUOUS_GENERATIONS: usize = 80;
const CONTINUOUS_PERIOD_MS: u64 = 100;
const CONTINUOUS_WINDOW_NS: u64 = 8_000_000_000;
const PRESSURE_FEEDBACK_DELAY: Duration = Duration::from_millis(150);
const CONTINUOUS_MARKER_BITS: usize = 8;

fn marker_pixels(generation: usize) -> Vec<u8> {
    marker_pixels_with_columns(generation, 2)
}
fn marker_pixels_with_columns(generation: usize, columns: usize) -> Vec<u8> {
    assert!(matches!(columns, 2 | 4));
    assert!(generation < (1 << (columns * 2)));
    let mut bytes = pattern(false);
    let gray = generation ^ (generation >> 1);
    for y in 0..16usize {
        for x in 0..columns * 8 {
            let bit = (y / 8) * columns + x / 8;
            let value = if gray >> bit & 1 == 0 { 32 } else { 224 };
            let i = (y * 800 + x) * 4;
            bytes[i..i + 4].copy_from_slice(&[value, value, value, 255]);
        }
    }
    bytes
}
fn marker_id(bytes: &[u8]) -> Option<usize> {
    marker_id_with_columns(bytes, 2)
}
fn marker_id_with_columns(bytes: &[u8], columns: usize) -> Option<usize> {
    assert!(matches!(columns, 2 | 4));
    if bytes.len() != 800 * 600 * 4 {
        return None;
    }
    let mut gray = 0usize;
    for bit in 0..columns * 2 {
        let x0 = (bit % columns) * 8 + 3;
        let y0 = (bit / columns) * 8 + 3;
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
// The continuous animation occupies exactly two 16x16 codec tiles. Every
// remaining pixel/channel participates in static equality, including edges.
fn continuous_static_sse(actual: &[u8], expected: &[u8]) -> u64 {
    assert_eq!(actual.len(), 800 * 600 * 4);
    assert_eq!(actual.len(), expected.len());
    actual
        .chunks_exact(800 * 4)
        .zip(expected.chunks_exact(800 * 4))
        .enumerate()
        .map(|(y, (a, b))| {
            let start = if y < 16 { 32 * 4 } else { 0 };
            let (a, b) = (&a[start..], &b[start..]);
            if a == b { 0 } else { sse(a, b) }
        })
        .sum()
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
        "comparison_ns":s.comparison_ns,"stale_png_dropped":s.stale_png_dropped,
        "followup_opportunities":s.followup_opportunities,"priority_deferrals":s.priority_deferrals,
        "retained_followups":s.retained_followups})
}
#[test]
fn region_priority_report_includes_actual_decision_counters() {
    let stats = host_stats(&host::LosslessScheduleProbe::default());
    for field in [
        "followup_opportunities",
        "priority_deferrals",
        "retained_followups",
    ] {
        assert_eq!(
            stats[field].as_u64(),
            Some(0),
            "missing decision counter: {field}"
        );
    }
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
    run_session_workload(arm, report, false, Duration::ZERO);
}
fn run_session_workload(arm: &str, report: &mut Value, continuous: bool, feedback_delay: Duration) {
    let enabled = arm != "off";
    let policy = match arm {
        "legacy" => host::LosslessSchedule::Legacy100,
        "off" | "proposed" | "new250" => host::LosslessSchedule::New250,
        "region_deadline" => host::LosslessSchedule::RegionDeadline,
        "region_priority" => host::LosslessSchedule::RegionPriority,
        "quiet0_pace100" => host::LosslessSchedule::Quiet0Pace100,
        "quiet0_pace250" => host::LosslessSchedule::Quiet0Pace250,
        "quiet250_pace100" => host::LosslessSchedule::Quiet250Pace100,
        "quiet250_pace250" => host::LosslessSchedule::Quiet250Pace250,
        _ => panic!("unknown explicit benchmark policy"),
    };
    let probe = Arc::new(host::LosslessScheduleProbe::default());
    let generations = if continuous {
        CONTINUOUS_GENERATIONS
    } else {
        16
    };
    let period_ms = if continuous {
        CONTINUOUS_PERIOD_MS
    } else {
        500
    };
    let inputs: Vec<_> = (0..generations)
        .map(|generation| {
            if continuous {
                marker_pixels_with_columns(generation, 4)
            } else {
                marker_pixels(generation)
            }
        })
        .collect();
    report["markers"] = json!((0..generations).map(|generation|json!({"generation":generation,"scheduled_ns":generation as u64*period_ms*1_000_000,"source_sha256":input_hash(&inputs[generation]),"paint_started_ns":null,"commit_issued_ns":null,"commit_completed_ns":null})).collect::<Vec<_>>());
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
    let mut feedback_observed = None;
    let end = base + Duration::from_secs(9); // eight seconds scheduled work + one bounded drain
    while Instant::now() < end {
        while next < generations
            && Instant::now() >= base + Duration::from_millis(next as u64 * period_ms)
        {
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
        // Hold display credit without blocking source commits or compositor pumping.
        // This models delayed presentation feedback, not bandwidth or WAN latency.
        if !feedback.is_empty() && feedback_observed.is_none() {
            feedback_observed = Some(Instant::now());
        }
        if !feedback.is_empty()
            && feedback_observed.is_some_and(|at| at.elapsed() >= feedback_delay)
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
            let generation = if continuous {
                marker_id_with_columns(&bytes, 4)
            } else {
                marker_id(&bytes)
            };
            let error = if continuous {
                continuous_static_sse(&bytes, &expected)
            } else {
                static_sse(&bytes, &expected)
            };
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
                "feedback_observed_ns":elapsed_ns(base,feedback_observed.take().unwrap()),
                "gl_started_ns":elapsed_ns(base,gl_started),"gl_completed_ns":elapsed_ns(base,gl_completed),
                "validation_completed_ns":elapsed_ns(base,validated),"feedback_started_ns":elapsed_ns(base,feedback_started),"feedback_sent_ns":elapsed_ns(base,feedback_sent),
                "presented_ns":null,"static_sse":error}));
            last = Some(seq);
        }
        update_presented(report, base, &presented_times.lock().unwrap());
        report["host_stats"] = host_stats(&probe);
        if continuous && elapsed_ns(base, Instant::now()) <= CONTINUOUS_WINDOW_NS {
            report["active_window_host_stats"] = host_stats(&probe);
        }
        if let Ok(result) = host_done.try_recv() {
            panic!("benchmark host ended early: {result:?}");
        }
        if let Ok(result) = client_done.try_recv() {
            panic!("benchmark viewer ended early: {result:?}");
        }
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        next, generations,
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
fn continuous_acceptance(session: &mut Value) -> bool {
    let frames = session["frames"].as_array().unwrap();
    let observed: Vec<_> = frames
        .iter()
        .filter_map(|frame| frame["presented_ns"].as_u64().map(|at| (at, frame)))
        .collect();
    let active: Vec<_> = observed
        .iter()
        .filter(|(at, _)| *at <= CONTINUOUS_WINDOW_NS)
        .collect();
    let exact = active
        .iter()
        .filter(|(_, frame)| frame["static_sse"] == 0)
        .map(|(at, _)| *at)
        .min();
    let first_observed_exact = observed
        .iter()
        .filter(|(_, frame)| frame["static_sse"] == 0)
        .map(|(at, _)| *at)
        .min();
    let regression_count = first_observed_exact.map_or(0, |first| {
        observed
            .iter()
            .filter(|(at, frame)| *at > first && frame["static_sse"] != 0)
            .count()
    });
    let later_animation_presented = exact.is_some_and(|first| {
        active.iter().any(|(at, frame)| {
            frame["marker_generation"]
                .as_u64()
                .is_some_and(|generation| {
                    session["markers"][generation as usize]["commit_issued_ns"]
                        .as_u64()
                        .is_some_and(|commit| commit > first && *at >= commit)
                })
        })
    });
    let active_png = active
        .iter()
        .filter(|(_, frame)| frame["is_png"] == true)
        .count();
    let static_criterion = if session["arm"] == "new250" {
        // The negative control must actually reproduce global-quiet starvation.
        exact.is_none()
            && active
                .iter()
                .any(|(at, frame)| *at >= 7_000_000_000 && frame["static_sse"] != 0)
    } else {
        exact.is_some() && later_animation_presented && active_png > 1
    };
    session["acceptance"] = json!({
        "static_criterion":static_criterion,
        "first_static_exact_active_ns":exact,
        "later_animation_presented_after_exact":later_animation_presented,
        "active_presented_png_frames":active_png,
        "regressed_frames_after_exact_including_drain":regression_count,
        "no_exact_to_inexact_regression":regression_count == 0
    });
    static_criterion && regression_count == 0
}

#[test]
#[ignore = "explicit private continuous-animation starvation experiment using real TCP/GL"]
fn actual_static_lossless_region_deadline_continuous_measurement() {
    run_continuous_measurement(
        &["new250", "region_deadline", "region_deadline", "new250"],
        "static_lossless_region_deadline_continuous_tcp_ab",
        "JWM_REMOTE_REGION_DEADLINE_REPORT",
        Duration::ZERO,
    );
}

#[test]
#[ignore = "explicit private latest-frame-priority experiment using real TCP/GL"]
fn actual_static_lossless_region_priority_continuous_measurement() {
    run_continuous_measurement(
        &[
            "new250",
            "region_deadline",
            "region_priority",
            "region_priority",
            "region_deadline",
            "new250",
        ],
        "static_lossless_region_priority_continuous_tcp_ab",
        "JWM_REMOTE_REGION_PRIORITY_REPORT",
        Duration::ZERO,
    );
}

#[test]
#[ignore = "explicit private delayed display-credit pressure experiment using real TCP/GL"]
fn actual_static_lossless_region_priority_credit_pressure_measurement() {
    run_continuous_measurement(
        &[
            "new250",
            "region_deadline",
            "region_priority",
            "region_priority",
            "region_deadline",
            "new250",
        ],
        "static_lossless_region_priority_credit_pressure_tcp_ab",
        "JWM_REMOTE_REGION_PRESSURE_REPORT",
        PRESSURE_FEEDBACK_DELAY,
    );
}

fn run_continuous_measurement(
    order: &[&str],
    benchmark: &str,
    report_variable: &str,
    feedback_delay: Duration,
) {
    let mut report = json!({"schema_version":1,
        "benchmark":benchmark,
        "debug_assertions":cfg!(debug_assertions),
        "presentation_feedback_delay_ns":u64::try_from(feedback_delay.as_nanos()).unwrap(),
        "scope":"same-process synthetic owned displays and controlled software GL presentation; not physical display latency",
        "scheduled_interval_ns":CONTINUOUS_PERIOD_MS*1_000_000,
        "generation_budget":CONTINUOUS_GENERATIONS,"active_window_ns":CONTINUOUS_WINDOW_NS,
        "drain_ns":1_000_000_000u64,"width":800,"height":600,
        "dynamic_region_width":32,"dynamic_region_height":16,
        "static_pixels":800*600-32*16,"maximum_png_batch_pixels":65_536,
        "success":false,"sessions":[]});
    let mut failed = false;
    for (index, arm) in order.iter().enumerate() {
        let mut session = json!({"arm":arm,"order":index,"success":false,"failure":null,
            "markers":[],"frames":[],"host_stats":null,"active_window_host_stats":null,
            "host_records":[],"acceptance":null});
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_session_workload(arm, &mut session, true, feedback_delay);
            let accepted = continuous_acceptance(&mut session);
            if feedback_delay.is_zero() {
                assert!(
                    accepted,
                    "continuous static starvation/convergence or regression criterion failed"
                );
            } else {
                // Pressure can prevent convergence: retain that negative result rather
                // than confusing policy acceptance with measurement infrastructure.
                assert_eq!(
                    session["acceptance"]["no_exact_to_inexact_regression"],
                    true
                );
            }
        }));
        if let Err(error) = result {
            failed = true;
            session["success"] = json!(false);
            session["failure"] = json!(
                error
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| error.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "non-string test panic".into())
            );
        }
        report["sessions"].as_array_mut().unwrap().push(session);
    }
    let sessions = report["sessions"].as_array().unwrap();
    for generation in 0..CONTINUOUS_GENERATIONS {
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
    let path = std::env::var(report_variable)
        .expect("explicit private continuous-animation report path required");
    let bytes = serde_json::to_vec_pretty(&report).unwrap();
    assert!(bytes.len() <= 4 * 1024 * 1024);
    std::fs::write(path, bytes).unwrap();
    assert!(
        !failed,
        "continuous-animation experiment failed; raw private report retains every session"
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

#[test]
fn continuous_gray_markers_survive_production_jpeg_and_reject_uncertain_cells() {
    use crate::remote::{
        tile::{TileDecoder, TileEncodeRequest, TileEncoder},
        x11_capture::CapturedFrame,
    };
    // Exhaust the finite 8-bit identity set, not just the 80 scheduled markers.
    for generation in 0..1 << CONTINUOUS_MARKER_BITS {
        let bytes = marker_pixels_with_columns(generation, 4);
        assert_eq!(marker_id_with_columns(&bytes, 4), Some(generation));
        let frame = CapturedFrame {
            geometry_epoch: 1,
            source_width: 800,
            source_height: 600,
            image: image::RgbImage::from_fn(800, 600, |x, y| {
                let i = ((y * 800 + x) * 4) as usize;
                image::Rgb([bytes[i + 2], bytes[i + 1], bytes[i]])
            }),
        };
        let mut encoder = TileEncoder::new();
        let mut decoder = TileDecoder::new();
        encoder.plan(&frame, TileEncodeRequest::Keyframe).unwrap();
        let mut payload = Vec::new();
        encoder.encode_into(&mut payload, 0, &frame, 60).unwrap();
        let decoded = decoder
            .decode_into(&payload, crate::remote::frame::new_decode_buffer_pool())
            .unwrap();
        let actual: Vec<_> = decoded
            .image()
            .pixels()
            .flat_map(|pixel| [pixel[2], pixel[1], pixel[0], 255])
            .collect();
        assert_eq!(
            marker_id_with_columns(&actual, 4),
            Some(generation),
            "real JPEG classifier generation {generation}"
        );
    }
    for bit in 0..CONTINUOUS_MARKER_BITS {
        let mut uncertain = marker_pixels_with_columns(79, 4);
        let x0 = (bit % 4) * 8 + 3;
        let y0 = (bit / 4) * 8 + 3;
        for y in y0..y0 + 2 {
            for x in x0..x0 + 2 {
                let i = (y * 800 + x) * 4;
                uncertain[i..i + 3].fill(127);
            }
        }
        assert_eq!(marker_id_with_columns(&uncertain, 4), None);
    }
    assert_eq!(marker_id_with_columns(&[0; 16], 4), None);
}

#[test]
fn continuous_static_region_excludes_only_the_two_marker_tiles() {
    let expected = pattern(false);
    let mut bytes = marker_pixels_with_columns(79, 4);
    assert_eq!(continuous_static_sse(&bytes, &expected), 0);
    for (x, y) in [(32, 15), (31, 16), (799, 599)] {
        let index = (y * 800 + x) * 4;
        bytes[index] ^= 1;
        assert_eq!(continuous_static_sse(&bytes, &expected), 1);
        bytes[index] ^= 1;
    }
}
