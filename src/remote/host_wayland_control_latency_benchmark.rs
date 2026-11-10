//! Controlled single-process loopback measurement, not physical input-to-photon.
//! CI opts in explicitly; ordinary tests only exercise marker/report logic.
use super::*;
use serde_json::{Value, json};

const WARMUP: usize = 2;
const MEASURED: usize = 16;
const BUDGET: Duration = Duration::from_secs(25);

fn marker_word(run: u16, sample: u8) -> u32 {
    let checksum = (run as u8) ^ (run >> 8) as u8 ^ sample ^ 0xa7;
    (u32::from(run) << 16) | (u32::from(sample) << 8) | u32::from(checksum)
}
fn marker_pixels(word: u32) -> Vec<u8> {
    let mut pixels = vec![0; (SW * SH * 4) as usize];
    for y in 0..SH {
        for x in 0..SW {
            let bit = (y / 18) * 8 + x / 25;
            let white = bit < 64
                && if bit < 32 {
                    word >> bit & 1 == 1
                } else {
                    word >> (bit - 32) & 1 == 0
                };
            let i = ((y * SW + x) * 4) as usize;
            pixels[i..i + 3].fill(if white { 255 } else { 0 });
            pixels[i + 3] = 255;
        }
    }
    pixels
}
fn decode_marker(pixels: &[u8]) -> Option<u32> {
    if pixels.len() != (VW * VH * 4) as usize {
        return None;
    }
    let mut bits = 0u64;
    for cell in 0..64u32 {
        let cx = (cell % 8) * 50 + 25;
        let cy = (cell / 8) * 36 + 18;
        let mut white = None;
        for y in cy - 3..=cy + 3 {
            for x in cx - 3..=cx + 3 {
                let i = ((y * VW + x) * 4) as usize;
                let p = &pixels[i..i + 4];
                if p[3] != 255 {
                    return None;
                }
                let value = if p[..3].iter().all(|&c| c >= 220) {
                    true
                } else if p[..3].iter().all(|&c| c <= 35) {
                    false
                } else {
                    return None;
                };
                if white.is_some_and(|v| v != value) {
                    return None;
                }
                white = Some(value);
            }
        }
        if white == Some(true) {
            bits |= 1u64 << cell;
        }
    }
    let word = bits as u32;
    if (bits >> 32) as u32 != !word {
        return None;
    }
    let run = (word >> 16) as u16;
    let sample = (word >> 8) as u8;
    (marker_word(run, sample) == word).then_some(word)
}
fn commit_marker(h: &mut Harness, word: u32) -> Instant {
    let (buffer, mut file) = shm(&mut h.native, &mut h.target.client, [0, 0, 0, 255], SW, SH);
    file.seek(SeekFrom::Start(0)).unwrap();
    file.write_all(&marker_pixels(word)).unwrap();
    h.target
        .client
        .request(h.target.surface, 1, &[buffer, 0, 0]);
    h.target
        .client
        .request(h.target.surface, 2, &[0, 0, SW, SH]);
    h.target.client.request(h.target.surface, 6, &[]);
    // Wire request submitted, not compositor processing or a physical scanout.
    let committed = Instant::now();
    h.native.roundtrip();
    h.native.roundtrip();
    committed
}
fn complete(expected: u32, previous: u64, gl: Option<(u32, u64)>, presented: Option<u64>) -> bool {
    gl.is_some_and(|(word, seq)| word == expected && seq > previous && presented == Some(seq))
}
fn quantiles(values: &[f64]) -> Value {
    if values.is_empty() {
        return Value::Null;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let nearest = |percent: usize| sorted[(sorted.len() * percent).div_ceil(100).saturating_sub(1)];
    json!({"n": sorted.len(), "min_ms": sorted[0], "p50_ms": nearest(50), "p95_ms": nearest(95), "max_ms": sorted[sorted.len()-1], "method":"nearest-rank; small sample counts do not provide a stable tail estimate"})
}
struct Report {
    samples: Vec<Value>,
    success: bool,
    failure: String,
    emit: bool,
    measured: usize,
    legacy_schedule: bool,
}
impl Report {
    fn new() -> Self {
        Self {
            emit: true,
            measured: MEASURED,
            legacy_schedule: false,
            samples: Vec::new(),
            success: false,
            failure: "setup did not complete".into(),
        }
    }
    fn value(&mut self) -> Value {
        for sample in &mut self.samples {
            if sample["status"] == "pending" {
                sample["status"] = json!("failed");
                sample["failure"] = json!(self.failure);
            }
        }
        let mut distributions = serde_json::Map::new();
        for name in [
            "inject_to_recipient_observed_ms",
            "recipient_observed_to_app_commit_ms",
            "app_commit_to_gl_verified_ms",
            "gl_verified_to_present_observed_ms",
            "inject_to_present_observed_ms",
        ] {
            let values: Vec<f64> = self
                .samples
                .iter()
                .filter(|s| s["phase"] == "measured" && s["status"] == "ok")
                .filter_map(|s| s[name].as_f64())
                .collect();
            distributions.insert(name.into(), quantiles(&values));
        }
        let report = json!({"schema_version":1, "success":self.success, "failure": if self.success { Value::Null } else { json!(self.failure) },
            "environment":{"os":std::env::consts::OS,"arch":std::env::consts::ARCH,"pid":std::process::id(),"libgl_always_software":std::env::var("LIBGL_ALWAYS_SOFTWARE").ok(),"mesa_driver_override":std::env::var("MESA_LOADER_DRIVER_OVERRIDE").ok(),"gl_renderer":"production headless Smithay GLES; driver identity not queried", "topology":"single-process owned dual Wayland displays and loopback authenticated TCP", "source_size":[SW,SH],"viewer_size":[VW,VH],"fps":30,"jpeg_quality":90,"poll_sleep_ms":1},
            "clock":"one process monotonic Instant", "scope":"one fixed selected native root identity and geometry; no source requests or shared sources; one control lease. Wire source/geometry epoch fields are not separately observed.", "endpoints":"injection before click wire requests; recipient observed after fixture dispatch; app commit request submitted; actual GL readback marker verified; exact client presentation feedback observed. Includes polling/readback/test feedback overhead, not physical input or scanout; no cross-machine claim.",
            "budget":"25s checked including setup and after each sample; synchronous fixture operations and teardown have their own bounded waits, so this is not a strict wall-clock process cap", "warmup_requested":WARMUP,"measured_requested":self.measured,"legacy_retained_schedule":self.legacy_schedule,"samples":self.samples,"quantiles":distributions});
        report
    }
}
impl Drop for Report {
    fn drop(&mut self) {
        if !self.emit {
            return;
        }
        let report = self.value();
        let encoded = serde_json::to_string_pretty(&report).unwrap();
        eprintln!("JWM_REMOTE_LATENCY_REPORT\n{encoded}");
        if let Ok(path) = std::env::var("JWM_REMOTE_LATENCY_REPORT") {
            if let Err(error) = std::fs::write(&path, encoded) {
                eprintln!("failed to write latency artifact {path:?}: {error}");
                if !std::thread::panicking() {
                    panic!("required latency artifact write failed: {error}");
                }
            }
        }
    }
}

#[test]
#[ignore = "owned TCP/Wayland/production GL latency benchmark; explicit CI only"]
fn controlled_input_to_unique_visible_marker_latency() {
    let mut report = Report::new();
    run_session(&mut report);
}

fn run_session(report: &mut Report) {
    let started = Instant::now();
    let mut gl = HeadlessSmithayGles::new()
        .expect("explicit latency benchmark requires real headless GL, never skip");
    let mut h = Harness::new_with_legacy_schedule(&mut gl, false, report.legacy_schedule);
    let mut previous = h.activate(&mut gl);
    assert!(
        started.elapsed() < BUDGET,
        "setup exceeded measurement budget"
    );
    let viewer_root = viewer_surface(&h.local);
    let viewer_window = *h.local.state.toplevels.keys().next().unwrap();
    let target_surface = h.native.state.surface_for_window(h.target.window).unwrap();
    let run: u16 = rand::random();
    let ms = |duration: Duration| duration.as_secs_f64() * 1000.0;
    for index in 0..WARMUP + report.measured {
        report.failure = format!(
            "sample {index}: deadline, protocol, recipient, marker or presentation assertion failed"
        );
        report.samples.push(json!({"sample":index,"phase":if index < WARMUP {"warmup"} else {"measured"},"status":"pending","run_token":run}));
        assert!(
            started.elapsed() < BUDGET,
            "whole benchmark budget includes setup and lease activation"
        );
        let word = marker_word(run, (index + 1) as u8);
        let before = h.received.pointer_actions.len();
        let injected = Instant::now();
        h.click();
        let mut received = None;
        let mut committed = None;
        let mut verified: Option<(u32, u64, Instant)> = None;
        let presented;
        loop {
            assert!(
                started.elapsed() < BUDGET,
                "global 25 second budget, not renewed per sample"
            );
            assert_eq!(
                h.native.state.surface_for_window(h.target.window).as_ref(),
                Some(&target_surface)
            );
            let geometry = h.native.state.window_geometry[&h.target.window];
            assert_eq!(
                (geometry.w, geometry.h),
                (SW, SH),
                "source geometry must not change during samples"
            );
            assert_eq!(viewer_surface(&h.local), viewer_root);
            let viewer_geometry = h.local.state.window_geometry[&viewer_window];
            assert_eq!((viewer_geometry.w, viewer_geometry.h), (VW, VH));
            assert!(remote_control::active(&h.native.state));
            assert_eq!(remote_control::attempts_for_test(&h.native.state), 1);
            h.tick_with_inspector(&mut gl, verified.is_none(), |pixels, sequence| {
                if let Some(actual) = decode_marker(pixels) {
                    if actual == word && sequence > previous && verified.is_none() {
                        assert!(
                            committed.is_some(),
                            "marker cannot exist before actual recipient caused app commit"
                        );
                        verified = Some((actual, sequence, Instant::now()));
                    }
                }
                false
            });
            assert!(h.received.keys.is_empty());
            let events = &h.received.pointer_actions[before..];
            assert!(
                events.len() <= 3,
                "one input must yield exactly one balanced click"
            );
            if received.is_none() && events.len() == 3 {
                assert_eq!(events[0].0, 2);
                assert_eq!(&events[0].1[1..], &[80 * 256, 60 * 256]);
                assert_eq!(events[1].0, 3);
                assert_eq!(&events[1].1[2..], &[0x110, 1]);
                assert_eq!(events[2].0, 3);
                assert_eq!(&events[2].1[2..], &[0x110, 0]);
                received = Some(Instant::now());
                // The app responds only to its own actual native wire events.
                committed = Some(commit_marker(&mut h, word));
            }
            let client_presented = h.frames.lock().unwrap().1;
            if complete(
                word,
                previous,
                verified.map(|(w, s, _)| (w, s)),
                client_presented,
            ) {
                assert!(
                    started.elapsed() < BUDGET,
                    "sample completed after measurement budget"
                );
                presented = Instant::now();
                break;
            }
            assert!(
                matches!(h.client_done.try_recv(), Err(mpsc::TryRecvError::Empty)),
                "viewer ended before sample"
            );
            assert!(
                matches!(h.host_done.try_recv(), Err(mpsc::TryRecvError::Empty)),
                "host ended before sample"
            );
            thread::sleep(Duration::from_millis(1));
        }
        let recipient = received.unwrap();
        let commit = committed.unwrap();
        let (_, sequence, gl_verified) = verified.unwrap();
        assert!(
            injected <= recipient
                && recipient <= commit
                && commit <= gl_verified
                && gl_verified <= presented
        );
        report.samples[index] = json!({"sample":index,"phase":if index < WARMUP {"warmup"} else {"measured"},"status":"ok","run_token":run,"marker":word,"previous_sequence":previous,"presented_sequence":sequence,
            "inject_to_recipient_observed_ms":ms(recipient-injected),"recipient_observed_to_app_commit_ms":ms(commit-recipient),"app_commit_to_gl_verified_ms":ms(gl_verified-commit),"gl_verified_to_present_observed_ms":ms(presented-gl_verified),"inject_to_present_observed_ms":ms(presented-injected)});
        // No next sample until this exact new frame has restored control Ready.
        // Withhold further feedback so a later unrelated frame cannot replace it.
        while *h.ready.lock().unwrap() != Some(sequence) {
            assert!(started.elapsed() < BUDGET);
            h.tick_with_inspector(&mut gl, false, |_, _| false);
            thread::sleep(Duration::from_millis(1));
        }
        previous = sequence;
    }
    report.failure = "post-measurement focus revocation or cleanup failed".into();
    let before = h.received.pointer_actions.clone();
    h.keyboard
        .as_ref()
        .unwrap()
        .leave(400, &viewer_surface(&h.local));
    h.rejected(&mut gl, &[], &before, 1, "Wayland keyboard focus left");
    report.success = true;
}

// A is the historical retained-capture scheduling, B is the production fix.
// Separate sessions prevent either arm from renewing a control lease.
const AB_ORDER: [bool; 4] = [true, false, false, true];
const AB_MEASURED_PER_SESSION: usize = MEASURED;

fn ab_comparison(sessions: &[Value]) -> Value {
    if sessions.len() != 4 || sessions.iter().any(|s| s["success"] != true) {
        return Value::Null; // Never compare a partial or failed measurement.
    }
    let mut stages = serde_json::Map::new();
    for name in [
        "inject_to_recipient_observed_ms",
        "recipient_observed_to_app_commit_ms",
        "app_commit_to_gl_verified_ms",
        "gl_verified_to_present_observed_ms",
        "inject_to_present_observed_ms",
    ] {
        let mut arms = [Vec::new(), Vec::new()];
        let mut medians = Vec::new();
        for (index, session) in sessions.iter().enumerate() {
            let values: Vec<f64> = session["samples"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|sample| sample["phase"] == "measured" && sample["status"] == "ok")
                .map(|sample| sample[name].as_f64().unwrap())
                .collect();
            if values.len() != AB_MEASURED_PER_SESSION {
                return Value::Null;
            }
            medians.push(quantiles(&values)["p50_ms"].as_f64().unwrap());
            arms[usize::from(!AB_ORDER[index])].extend(values);
        }
        let baseline = quantiles(&arms[0]);
        let candidate = quantiles(&arms[1]);
        stages.insert(name.into(), json!({"baseline_A":baseline,"candidate_B":candidate,
            "pooled_p50_delta_B_minus_A_ms":candidate["p50_ms"].as_f64().unwrap()-baseline["p50_ms"].as_f64().unwrap(),
            "pooled_p95_delta_B_minus_A_ms":candidate["p95_ms"].as_f64().unwrap()-baseline["p95_ms"].as_f64().unwrap(),
            "block_median_deltas_B_minus_A_ms":[medians[1]-medians[0],medians[2]-medians[3]]}));
    }
    json!({"stages":stages,"interpretation":"same process/binary/config AB then BA; negative B-A is lower observed latency, descriptive only, no performance pass threshold; no cross-CI-run comparison or causal effect/significance claim"})
}
struct AbReport {
    sessions: Vec<Report>,
    success: bool,
}
impl Drop for AbReport {
    fn drop(&mut self) {
        let sessions: Vec<Value> = self
            .sessions
            .iter_mut()
            .enumerate()
            .map(|(index, report)| {
                let mut value = report.value();
                value["session_order"] = json!(index);
                value["arm"] = json!(if AB_ORDER[index] {
                    "A_legacy"
                } else {
                    "B_candidate"
                });
                value["order_block"] = json!(index / 2);
                value
            })
            .collect();
        let comparison = if self.success {
            ab_comparison(&sessions)
        } else {
            Value::Null
        };
        let report = json!({"schema_version":1,"benchmark":"retained_frame_schedule_abba","success":self.success,
            "configuration":"identical source/viewer size, fps, JPEG quality, input workload, build and process; only per-host-thread test-only retained scheduling switch differs",
            "order":["A_legacy","B_candidate","B_candidate","A_legacy"],"sessions_requested":4,"warmup_per_session":WARMUP,"measured_per_session":AB_MEASURED_PER_SESSION,"warmup_per_arm":4,"measured_per_arm":2 * AB_MEASURED_PER_SESSION,
            "lease_policy":"new session per arm occurrence; per-session 25s checked budget includes setup, no renewal; bounded teardown excluded from measurement",
            "failure":if self.success {Value::Null} else {json!("setup, sample or cleanup failed; inspect all retained session samples")},"sessions":sessions,"comparison":comparison});
        let encoded = serde_json::to_string_pretty(&report).unwrap();
        eprintln!("JWM_REMOTE_LATENCY_AB_REPORT\n{encoded}");
        if let Ok(path) = std::env::var("JWM_REMOTE_LATENCY_REPORT") {
            if let Err(error) = std::fs::write(&path, encoded) {
                eprintln!("failed to write AB artifact {path:?}: {error}");
                if !thread::panicking() {
                    panic!("required AB artifact write failed: {error}");
                }
            }
        }
    }
}

#[test]
#[ignore = "same-process ABBA owned TCP/Wayland/GL benchmark; explicit CI only"]
fn controlled_input_to_unique_visible_marker_latency_abba() {
    let mut report = AbReport {
        sessions: Vec::new(),
        success: false,
    };
    for legacy in AB_ORDER {
        let mut session = Report::new();
        session.emit = false;
        session.measured = AB_MEASURED_PER_SESSION;
        session.legacy_schedule = legacy;
        report.sessions.push(session);
        run_session(report.sessions.last_mut().unwrap());
    }
    report.success = true;
}

#[test]
fn latency_ab_comparison_requires_complete_sessions_and_uses_same_run_blocks() {
    assert_eq!(ab_comparison(&[]), Value::Null);
    let mut sessions = Vec::new();
    for (index, legacy) in AB_ORDER.into_iter().enumerate() {
        let samples: Vec<Value> = (0..AB_MEASURED_PER_SESSION).map(|_| {
            let ms = if legacy { 20.0 } else { 10.0 };
            json!({"phase":"measured","status":"ok","inject_to_recipient_observed_ms":ms,"recipient_observed_to_app_commit_ms":ms,"app_commit_to_gl_verified_ms":ms,"gl_verified_to_present_observed_ms":ms,"inject_to_present_observed_ms":ms})
        }).collect();
        sessions.push(json!({"success":true,"session_order":index,"samples":samples}));
    }
    let comparison = ab_comparison(&sessions);
    assert_eq!(
        comparison["stages"]["inject_to_present_observed_ms"]["baseline_A"]["n"],
        32
    );
    assert_eq!(
        comparison["stages"]["inject_to_present_observed_ms"]["block_median_deltas_B_minus_A_ms"],
        json!([-10.0, -10.0])
    );
    sessions[2]["success"] = json!(false);
    assert_eq!(ab_comparison(&sessions), Value::Null);
    sessions[2]["success"] = json!(true);
    sessions[2]["samples"][0]["status"] = json!("failed");
    assert_eq!(ab_comparison(&sessions), Value::Null);
}

#[test]
fn latency_marker_rejects_stale_wrong_run_corruption_and_missing_presentation() {
    let expected = marker_word(0x4821, 3);
    let source = marker_pixels(expected);
    let mut scaled = vec![0; (VW * VH * 4) as usize];
    for y in 0..VH {
        for x in 0..VW {
            let src = (((y / 2) * SW + x / 2) * 4) as usize;
            let dst = ((y * VW + x) * 4) as usize;
            scaled[dst..dst + 4].copy_from_slice(&source[src..src + 4]);
        }
    }
    assert_eq!(decode_marker(&scaled), Some(expected));
    assert!(!complete(
        expected,
        4,
        Some((marker_word(0x4821, 2), 5)),
        Some(5)
    ));
    assert!(!complete(
        expected,
        4,
        Some((marker_word(0x4822, 3), 5)),
        Some(5)
    ));
    assert!(!complete(expected, 4, Some((expected, 4)), Some(4)));
    assert!(!complete(expected, 4, Some((expected, 5)), None));
    assert!(!complete(expected, 4, Some((expected, 5)), Some(6)));
    assert!(
        !complete(expected, 4, None, Some(5)),
        "no app repaint cannot produce evidence"
    );
    assert!(complete(expected, 4, Some((expected, 5)), Some(5)));
    assert_eq!(decode_marker(&[]), None);
    // A clear black/white bit flip must fail complement validation, not only
    // ambiguous JPEG threshold handling.
    let mut complement_broken = scaled.clone();
    for y in 15..=21 {
        for x in 22..=28 {
            let i = ((y * VW + x) * 4) as usize;
            for channel in &mut complement_broken[i..i + 3] {
                *channel = 255 - *channel;
            }
        }
    }
    assert_eq!(decode_marker(&complement_broken), None);
    let invalid_checksum = marker_pixels(expected ^ 1);
    let mut checksum_broken = scaled.clone();
    for y in 0..VH {
        for x in 0..VW {
            let src = (((y / 2) * SW + x / 2) * 4) as usize;
            let dst = ((y * VW + x) * 4) as usize;
            checksum_broken[dst..dst + 4].copy_from_slice(&invalid_checksum[src..src + 4]);
        }
    }
    assert_eq!(decode_marker(&checksum_broken), None);
    let i = ((18 * VW + 25) * 4) as usize;
    scaled[i..i + 3].fill(127);
    assert_eq!(decode_marker(&scaled), None);
    assert_eq!(quantiles(&[1.0, 2.0, 3.0, 4.0])["p50_ms"], 2.0);
}
