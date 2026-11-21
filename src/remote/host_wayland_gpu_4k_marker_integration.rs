//! View the existing P09 X11 marker workload in the private Wayland output.
use super::*;
use std::{
    io::{BufRead, BufReader, LineWriter},
    net::SocketAddr,
};

const CELL: usize = ROI_SIZE as usize / 8;

/// Same central 5×5 majority, complement and checksum as P09's X11 observer.
fn read_marker(rgba: &[u8]) -> Option<(u8, u16)> {
    assert_eq!(rgba.len(), (ROI_SIZE * ROI_SIZE * 4) as usize);
    let mut bits = 0u64;
    for cell in 0..64 {
        let x = (cell % 8) * CELL + CELL / 2;
        let y = (cell / 8) * CELL + CELL / 2;
        let mut white = 0;
        let mut black = 0;
        for row in y - 2..=y + 2 {
            for column in x - 2..=x + 2 {
                let start = (row * ROI_SIZE as usize + column) * 4;
                let rgb = &rgba[start..start + 3];
                white += usize::from(rgb.iter().all(|&value| value >= 180));
                black += usize::from(rgb.iter().all(|&value| value <= 75));
            }
        }
        if white >= 19 {
            bits |= 1 << cell;
        } else if black < 19 {
            return None;
        }
    }
    let word = bits as u32;
    let run_id = (word >> 24) as u8;
    let frame_id = (word >> 8) as u16;
    let identity = word >> 8;
    let mut check = identity ^ 0xc949_59a7;
    check = (check ^ (check >> 16)).wrapping_mul(0x7feb_352d);
    check = (check ^ (check >> 15)).wrapping_mul(0x846c_a68b);
    check ^= check >> 16;
    (run_id != 0 && frame_id != 0 && bits >> 32 == u64::from(!word) && word & 255 == check & 255)
        .then_some((run_id, frame_id))
}

#[test]
fn p09_marker_roi_decodes_identity_and_rejects_corruption() {
    // Reference word from p09-workload.c marker(41, 1763), with its complement.
    let word = 0x2906_e30e_u32;
    let bits = (u64::from(!word) << 32) | u64::from(word);
    let mut rgba = vec![128; (ROI_SIZE * ROI_SIZE * 4) as usize];
    for cell in 0..64 {
        let x = (cell % 8) * CELL + CELL / 2;
        let y = (cell / 8) * CELL + CELL / 2;
        let value = if bits & (1 << cell) != 0 { 248 } else { 8 };
        for row in y - 2..=y + 2 {
            for column in x - 2..=x + 2 {
                let start = (row * ROI_SIZE as usize + column) * 4;
                rgba[start..start + 3].fill(value);
            }
        }
    }
    assert_eq!(read_marker(&rgba), Some((41, 1763)));
    // Flip a resilient whole cell so all votes agree but the complement fails.
    for row in 4..=8 {
        for column in 4..=8 {
            let start = (row * ROI_SIZE as usize + column) * 4;
            rgba[start..start + 3].fill(248);
        }
    }
    assert_eq!(read_marker(&rgba), None);
}

#[derive(Debug, PartialEq, Eq, serde::Serialize)]
struct MarkerScope {
    session_id: String,
    source_epoch: u64,
    geometry_epoch: u64,
}

struct ViewerReports {
    events: BufReader<File>,
    partial_line: Vec<u8>,
    scope: Option<MarkerScope>,
    matched_ack: bool,
}

impl ViewerReports {
    fn poll(&mut self, mut clock_log: impl Write, clock: &Clock<Monotonic>, viewer: &Process) {
        while self
            .events
            .read_until(b'\n', &mut self.partial_line)
            .unwrap()
            != 0
        {
            if !self.partial_line.ends_with(b"\n") {
                // A concurrently written partial line is continued next poll.
                break;
            }
            let row: Value = serde_json::from_slice(&self.partial_line).expect("valid viewer JSON");
            self.partial_line.clear();
            assert_ne!(
                row["state"],
                "failed",
                "actual viewer failed: {row}\n{}",
                viewer.text()
            );
            let metrics = &row["metrics"];
            if metrics.is_null() {
                continue;
            }
            log_row(
                &mut clock_log,
                json!({
                    "monotonic_ns": monotonic_ns(clock),
                    "session_id": metrics["session_id"],
                    "last_decoded_sequence": metrics["last_decoded_frame"]["sequence"],
                    "window_ms": metrics["window_ms"],
                    "final": metrics["final"],
                }),
            );
            assert_eq!(
                metrics["negotiated_codec"], "H.264",
                "negotiated viewer codec: {row}"
            );
            assert_eq!(
                metrics["hardware_download_ms"]["observations"], 0,
                "GPU viewer downloaded a frame: {row}"
            );
            let presented = metrics["presented_frames"].as_u64().unwrap();
            assert_eq!(
                metrics["native_presented_frames"], presented,
                "viewer used another presentation path: {row}"
            );
            if presented > 0 {
                assert_eq!(metrics["last_presentation_path"], "wayland-vaapi-dmabuf");
            }
            let frame = &metrics["last_decoded_frame"];
            if frame.is_null() {
                if let Some(scope) = &self.scope {
                    assert_eq!(metrics["session_id"], scope.session_id);
                }
                continue;
            }
            assert!(exact_h264(frame), "viewer must keep exact H.264 4K: {row}");
            let next = MarkerScope {
                session_id: metrics["session_id"]
                    .as_str()
                    .expect("authenticated session ID")
                    .to_owned(),
                source_epoch: frame["source_epoch"].as_u64().expect("source epoch"),
                geometry_epoch: frame["geometry_epoch"].as_u64().expect("geometry epoch"),
            };
            if let Some(scope) = &self.scope {
                assert_eq!(scope, &next, "viewer changed session, source or geometry");
            } else {
                self.scope = Some(next);
            }
            self.matched_ack |= presented > 0 && metrics["acked_frames"].as_u64().unwrap() > 0;
        }
    }
}

fn log_row(mut writer: impl Write, row: Value) {
    serde_json::to_writer(&mut writer, &row).unwrap();
    writer.write_all(b"\n").unwrap();
}

fn monotonic_ns(clock: &Clock<Monotonic>) -> u64 {
    u64::try_from(Duration::from(clock.now()).as_nanos()).unwrap()
}

#[derive(Default)]
struct MarkerCounts {
    reads: u64,
    unique: u64,
    scored_unique: u64,
    duplicates: u64,
    invalid: u64,
    wrong_run: u64,
    reordered: u64,
    last_frame: u16,
}

impl MarkerCounts {
    fn classify(&mut self, marker: Option<(u8, u16)>, expected_run: u8) -> &'static str {
        self.reads += 1;
        match marker {
            None => {
                self.invalid += 1;
                "invalid"
            }
            Some((run, _)) if run != expected_run => {
                self.wrong_run += 1;
                "wrong_run"
            }
            Some((_, frame)) if frame == self.last_frame => {
                self.duplicates += 1;
                "duplicate"
            }
            Some((_, frame)) if frame < self.last_frame => {
                self.reordered += 1;
                "reordered"
            }
            Some((_, frame)) => {
                self.last_frame = frame;
                "new"
            }
        }
    }
}

fn duration_seconds(name: &str, default: u64, min: u64, max: u64) -> u64 {
    let seconds = std::env::var(name).map_or(default, |value| value.parse().unwrap());
    assert!(
        (min..=max).contains(&seconds),
        "{name} outside {min}..={max}"
    );
    seconds
}

#[test]
#[ignore = "requires an owned external P09 host, private key, and Intel VA-API"]
fn native_cli_wayland_gpu_viewer_observes_p09_4k_marker() {
    let endpoint: SocketAddr = std::env::var("JWM_TEST_P09_ENDPOINT")
        .unwrap()
        .parse()
        .unwrap();
    let key = PathBuf::from(std::env::var_os("JWM_TEST_P09_KEY_FILE").unwrap());
    assert!(
        key.is_absolute() && key.is_file(),
        "absolute private key file required"
    );
    let run_id: u8 = std::env::var("JWM_TEST_P09_RUN_ID")
        .unwrap()
        .parse()
        .unwrap();
    assert_ne!(run_id, 0);
    let evidence = PathBuf::from(std::env::var_os("JWM_TEST_P09_EVIDENCE_DIR").unwrap());
    assert!(evidence.is_absolute());
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&evidence)
        .expect("new evidence directory");
    let warmup = duration_seconds("JWM_TEST_P09_WARMUP_SECONDS", 0, 0, 120);
    let duration = duration_seconds("JWM_TEST_P09_OBSERVE_SECONDS", 5, 3, 120);

    let node = PathBuf::from(std::env::var_os("JWM_TEST_WAYLAND_GPU_CAPTURE_NODE").unwrap());
    let render_node_device = node.metadata().unwrap().rdev();
    let mut gl =
        HeadlessSmithayGles::on_render_node(&node).expect("explicit Intel hardware GBM/EGL");
    let directory = PrivateDir::new();
    let socket = directory.0.join("viewer.sock");
    let listener = UnixListener::bind(&socket).expect("owned Wayland listener");
    listener.set_nonblocking(true).unwrap();
    let mut server = Server::new();
    register_test_presentation(&mut server);
    server.state.ensure_dmabuf_global_with_feedback(
        &server.display.handle(),
        gl.renderer
            .egl_context()
            .dmabuf_render_formats()
            .iter()
            .copied(),
        [],
        render_node_device,
    );
    let mut output = Private4kOutput::new(&mut server, &mut gl.renderer);

    let events = evidence.join("viewer-events.jsonl");
    let stdout = File::create(&events).unwrap();
    let mut observer = LineWriter::new(File::create(evidence.join("observed.jsonl")).unwrap());
    let mut viewer_clock =
        LineWriter::new(File::create(evidence.join("viewer-clock.jsonl")).unwrap());
    let mut reports = ViewerReports {
        events: BufReader::new(File::open(events).unwrap()),
        partial_line: Vec::new(),
        scope: None,
        matched_ack: false,
    };
    let mut command = cli(&cli_binary(), &directory.0);
    command
        .args([
            "session",
            &endpoint.to_string(),
            "--viewer-backend",
            "wayland",
            "--window-size",
            "3840x2160",
            "--scaling",
            "gpu",
            "--viewer-wayland-display",
        ])
        .arg(&socket)
        .arg("--key-file")
        .arg(&key)
        .env("JWM_REMOTE_GPU_PRESENT", &node);
    let mut viewer = Process::spawn_with_stdout(&mut command, stdout.into());

    let mut counts = MarkerCounts::default();
    let mut root = None;
    let mut interval = None;
    let mut max_gap_ns = 0;
    let mut last_scored_ns = 0;
    let startup_deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(
            !viewer.exited(),
            "viewer exited before observation ended: {}",
            viewer.text()
        );
        accept(&listener, &mut server);
        server.dispatch_timers(Duration::ZERO);
        assert!(server.state.toplevels.len() <= 1, "one owned viewer role");
        if let Some((&window, toplevel)) = server.state.toplevels.iter().next() {
            let surface = toplevel.wl_surface().clone();
            if !toplevel.is_initial_configure_sent() {
                let geo = server.state.window_geometry.get_mut(&window).unwrap();
                (geo.w, geo.h) = (WIDTH, HEIGHT);
                toplevel.with_pending_state(|state| {
                    state.size = Some((WIDTH as i32, HEIGHT as i32).into());
                    state.states.set(TopState::Activated);
                });
                toplevel.send_configure();
                server.display.flush_clients().unwrap();
            }
            assert_eq!(
                &*root.get_or_insert_with(|| surface.clone()),
                &surface,
                "owned viewer role changed"
            );
            let mut marker = None;
            if let Some(completed_us) =
                output.present(&mut server, &mut gl.renderer, &surface, |rgba| {
                    marker = read_marker(rgba)
                })
            {
                let completed = completed_us * 1000;
                let classification = counts.classify(marker, run_id);
                let (within_interval, scored) =
                    interval.map_or((false, false), |(start, score_start, end)| {
                        (
                            (start..end).contains(&completed),
                            (score_start..end).contains(&completed),
                        )
                    });
                let unique = within_interval && classification == "new";
                if unique {
                    counts.unique += 1;
                    if scored {
                        counts.scored_unique += 1;
                        max_gap_ns = max_gap_ns.max(completed - last_scored_ns);
                        last_scored_ns = completed;
                    }
                }
                log_row(
                    &mut observer,
                    json!({
                        "type": if unique { "p09_frame" } else { "p09_presentation" },
                        "monotonic_ns": completed,
                        "compositor_sequence": output.sequence,
                        "wayland_feedback": "presented",
                        "run_id": run_id,
                        "observed_run_id": marker.map(|(run, _)| run),
                        "source_frame": marker.map(|(_, frame)| frame),
                        "classification": classification,
                        "scored": scored,
                        "scope": reports.scope,
                    }),
                );
                assert_eq!(
                    counts.wrong_run, 0,
                    "viewer presented a different P09 trial ID"
                );
            }
        } else {
            assert!(root.is_none(), "owned viewer role disappeared");
        }
        reports.poll(&mut viewer_clock, &output.clock, &viewer);
        let now = monotonic_ns(&output.clock);
        if let Some((_, _, end)) = interval {
            if now >= end {
                break;
            }
        } else if reports.matched_ack && counts.last_frame != 0 {
            let score_start = now + warmup * 1_000_000_000;
            let end = score_start + duration * 1_000_000_000;
            interval = Some((now, score_start, end));
            last_scored_ns = score_start;
            log_row(
                &mut observer,
                json!({
                    "type": "p09_start", "role": "observer", "backend": "private-wayland-dmabuf",
                    "monotonic_ns": now, "score_start_ns": score_start, "score_end_ns": end,
                    "run_id": run_id, "width": WIDTH, "height": HEIGHT, "scope": reports.scope,
                    "roi": [ROI_ORIGIN, ROI_ORIGIN, ROI_SIZE, ROI_SIZE],
                    "roi_bytes": ROI_SIZE * ROI_SIZE * 4, "render_node_device": render_node_device,
                }),
            );
        } else {
            assert!(
                Instant::now() < startup_deadline,
                "no exact P09 marker and H.264 DMA-BUF ACK; read={} invalid={} scope={:?} viewer={}",
                counts.reads,
                counts.invalid,
                reports.scope,
                viewer.text()
            );
        }
        thread::sleep(Duration::from_millis(1));
    }

    let (_, score_start, score_end) = interval.unwrap();
    log_row(
        &mut observer,
        json!({
            "type": "p09_end", "role": "observer", "monotonic_ns": monotonic_ns(&output.clock),
            "run_id": run_id, "reads": counts.reads, "unique": counts.unique,
            "scored_unique": counts.scored_unique, "score_start_ns": score_start, "score_end_ns": score_end,
            "duplicates": counts.duplicates, "invalid": counts.invalid, "wrong_run": counts.wrong_run,
            "reordered": counts.reordered, "target_lost": false, "interrupted": false,
            "longest_scored_gap_ns": max_gap_ns.max(score_end - last_scored_ns),
            "scope": reports.scope,
        }),
    );
    assert!(
        counts.scored_unique >= 3,
        "P09 must render distinct marker IDs after warmup"
    );
    assert_eq!(
        counts.reordered, 0,
        "already presented marker order regressed"
    );

    viewer
        .child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"{\"command\":\"disconnect\",\"request\":1}\n")
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    while !viewer.exited() {
        server.roundtrip();
        assert!(
            Instant::now() < deadline,
            "viewer did not close: {}",
            viewer.text()
        );
        thread::sleep(Duration::from_millis(1));
    }
    reports.poll(&mut viewer_clock, &output.clock, &viewer);
    std::fs::write(evidence.join("viewer-stderr.log"), viewer.text()).unwrap();
    assert!(viewer.status.unwrap().success());
}
