//! Same-binary proof encoding ABBA; only legacy forced keyframe vs delta differs.
//! Tiny unique marker (96x72 of 200x150), identical source RGB in every arm.
use super::*;
use sha2::{Digest, Sha256};

const ORDER: [bool; 4] = [true, false, false, true];
const METRICS: [&str; 4] = [
    "encoded_bytes",
    "jpeg_ms",
    "app_commit_to_gl_verified_ms",
    "inject_to_present_observed_ms",
];
fn small_pixels(word: u32, blue: bool) -> Vec<u8> {
    let mut bytes = if blue {
        [255, 0, 0, 255]
    } else {
        [0, 0, 255, 255]
    }
    .repeat((SW * SH) as usize);
    for y in 0..72 {
        for x in 0..96 {
            let bit = (y / 9) * 8 + x / 12;
            let white = if bit < 32 {
                word >> bit & 1 == 1
            } else {
                word >> (bit - 32) & 1 == 0
            };
            let i = ((y * SW + x) * 4) as usize;
            bytes[i..i + 3].fill(if white { 255 } else { 0 });
        }
    }
    bytes
}
fn rgb_hash(bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    for pixel in bytes.chunks_exact(4) {
        digest.update([pixel[2], pixel[1], pixel[0]]);
    }
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn decode_small(bytes: &[u8], blue: bool) -> Option<u32> {
    let word = decode_marker_cells(bytes, 24, 18)?;
    // The >75% unchanged region cannot accidentally come from a bystander,
    // stale full-screen color or an incomplete delta reconstruction.
    for y in 2..VH - 2 {
        for x in 2..VW - 2 {
            if x < 200 && y < 152 {
                continue;
            } // exclude JPEG boundary ringing
            let i = ((y * VW + x) * 4) as usize;
            let p = &bytes[i..i + 4];
            let expected = if blue {
                p[0] >= 220 && p[2] <= 35
            } else {
                p[2] >= 220 && p[0] <= 35
            };
            if !expected || p[1] > 35 || p[3] != 255 {
                return None;
            }
        }
    }
    Some(word)
}
fn evidence_value(frame: &host::ProofFrameEvidence) -> Value {
    json!({"native_serial":frame.native_serial,"source_epoch":frame.source_epoch,"geometry_epoch":frame.geometry_epoch,"sequence":frame.sequence,"keyframe":frame.keyframe,"dirty_tiles":frame.dirty_tiles,"total_tiles":frame.total_tiles,"encoded_bytes":frame.encoded_bytes,"jpeg_ns":frame.jpeg_ns})
}
fn signed_ms(later: Instant, earlier: Instant) -> f64 {
    if let Some(elapsed) = later.checked_duration_since(earlier) {
        elapsed.as_secs_f64() * 1000.0
    } else {
        -earlier.duration_since(later).as_secs_f64() * 1000.0
    }
}
fn matches_event(event: &host::ProofStageEvent, frame: &host::ProofFrameEvidence) -> bool {
    event.native_serial == frame.native_serial
        && event.source_epoch == frame.source_epoch
        && event.geometry_epoch == frame.geometry_epoch
        && event.sequence == frame.sequence
}
fn frame_timing(frame: &host::ProofFrameEvidence, origin: Instant) -> Value {
    json!({"capture_started_ms":signed_ms(frame.captured_at,origin),
        "capture_completed_ms":signed_ms(frame.capture_completed_at,origin),
        "plan_started_ms":signed_ms(frame.planned_at,origin),"flush_completed_ms":signed_ms(frame.flushed_at,origin)})
}
fn sample_timing(
    t: &SampleTimeline,
    frame: &host::ProofFrameEvidence,
    events: &[host::ProofStageEvent],
    previous: Option<&host::ProofFrameEvidence>,
    origin: Instant,
) -> Value {
    let ack = events
        .iter()
        .find(|e| matches_event(e, frame) && e.kind == host::ProofStageKind::AckReceived)
        .map(|e| e.at);
    let ready = events
        .iter()
        .find(|e| matches_event(e, frame) && e.kind == host::ProofStageKind::ReadyPublished)
        .map(|e| e.at);
    let previous_ready = previous
        .and_then(|f| {
            events
                .iter()
                .find(|e| matches_event(e, f) && e.kind == host::ProofStageKind::ReadyPublished)
        })
        .map(|e| e.at);
    json!({"raw_offsets_ms":{"injected":signed_ms(t.injected,origin),"recipient_observed":signed_ms(t.recipient_observed,origin),"app_commit_submitted":signed_ms(t.app_committed,origin),"gl_verified":signed_ms(t.gl_verified,origin),"client_presented_observed":signed_ms(t.presented_observed,origin),"client_ready_observed":t.ready_observed.map(|at|signed_ms(at,origin)),"previous_client_ready_observed":signed_ms(t.previous_ready_observed,origin),"host_ack_received":ack.map(|at|signed_ms(at,origin)),"host_ready_published":ready.map(|at|signed_ms(at,origin)),"previous_host_ready_published":previous_ready.map(|at|signed_ms(at,origin))},
        "app_commit_to_capture_start_ms":signed_ms(frame.captured_at,t.app_committed),
        "capture_elapsed_ms":signed_ms(frame.capture_completed_at,frame.captured_at),
        "capture_end_to_plan_start_ms":signed_ms(frame.planned_at,frame.capture_completed_at),
        "plan_start_to_flush_end_ms":signed_ms(frame.flushed_at,frame.planned_at),
        "flush_end_to_gl_verified_ms":signed_ms(t.gl_verified,frame.flushed_at),
        "flush_end_to_host_ack_ms":ack.map(|at|signed_ms(at,frame.flushed_at)),
        "host_ack_to_ready_published_ms":ack.zip(ready).map(|(a,r)|signed_ms(r,a)),
        "previous_host_ready_to_inject_ms":previous_ready.map(|at|signed_ms(t.injected,at)),
        "previous_client_ready_to_inject_ms":signed_ms(t.injected,t.previous_ready_observed)})
}
fn checkpoints(
    h: &mut Harness,
    gl: &mut HeadlessSmithayGles,
    started: Instant,
    word: u32,
    previous: u64,
    legacy: bool,
    probe: &host::ProofDeltaProbe,
    checks: &mut Vec<Value>,
) {
    let mut previous = previous;
    for (name, expected, blue) in [
        ("static_new_proof", word, false),
        ("full_change", !word, true),
    ] {
        checks.push(json!({"name":name,"status":"pending","source_rgb_sha256":rgb_hash(&small_pixels(expected,blue))}));
        let at = checks.len() - 1;
        let baseline = loop {
            if let Some(frame) = probe
                .snapshot()
                .into_iter()
                .find(|f| f.sequence == previous)
            {
                break frame;
            }
            assert!(
                started.elapsed() < BUDGET,
                "published previous proof evidence required"
            );
            h.tick_with_inspector(gl, false, |_, _| false);
            thread::sleep(Duration::from_millis(1));
        };
        let input_before = h.received.pointer_actions.len();
        h.click();
        loop {
            assert!(
                started.elapsed() < BUDGET,
                "checkpoint input shares session budget"
            );
            h.tick_with_inspector(gl, false, |_, _| false);
            let events = &h.received.pointer_actions[input_before..];
            assert!(events.len() <= 3);
            assert!(h.received.keys.is_empty());
            if events.len() == 3 {
                assert_eq!(events[0].0, 2);
                assert_eq!(&events[0].1[1..], &[80 * 256, 60 * 256]);
                assert_eq!(events[1].0, 3);
                assert_eq!(&events[1].1[2..], &[0x110, 1]);
                assert_eq!(events[2].0, 3);
                assert_eq!(&events[2].1[2..], &[0x110, 0]);
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        // The real accepted click consumes the prior proof. The static case
        // tests fresh proof/presentation, not a visually distinguishable app
        // response latency; only the 18 unique-marker samples measure latency.
        commit_pixels(h, &small_pixels(expected, blue));
        let mut seen = None;
        let frame = loop {
            assert!(
                started.elapsed() < BUDGET,
                "checkpoint shares session budget"
            );
            h.tick_with_inspector(gl, seen.is_none(), |bytes, seq| {
                if seq > previous && decode_small(bytes, blue) == Some(expected) && seen.is_none() {
                    seen = Some(seq);
                }
                false
            });
            if let Some(seq) = seen {
                if h.frames.lock().unwrap().1 == Some(seq) {
                    if let Some(frame) = probe.snapshot().into_iter().find(|f| f.sequence == seq) {
                        break frame;
                    }
                }
            }
            thread::sleep(Duration::from_millis(1));
        };
        assert!(frame.native_serial > baseline.native_serial);
        assert_eq!(frame.source_epoch, baseline.source_epoch);
        assert_eq!(frame.geometry_epoch, baseline.geometry_epoch);
        assert!(frame.sequence > previous);
        if blue || legacy {
            assert_eq!(frame.dirty_tiles, frame.total_tiles);
            assert!(frame.encoded_bytes > 0 && frame.jpeg_ns > 0);
        } else {
            assert!(!frame.keyframe);
            assert_eq!(
                frame.dirty_tiles, 0,
                "same pixels with a new proof must be emitted zero-dirty, not suppressed or reset"
            );
            assert_eq!(frame.jpeg_ns, 0);
        }
        checks[at]["status"] = json!("ok");
        checks[at]["evidence"] = evidence_value(&frame);
        checks[at]["actual_presented_sequence"] = json!(h.frames.lock().unwrap().1);
        previous = frame.sequence;
        while *h.ready.lock().unwrap() != Some(previous) {
            assert!(started.elapsed() < BUDGET);
            h.tick_with_inspector(gl, false, |_, _| false);
            thread::sleep(Duration::from_millis(1));
        }
    }
}
fn compare(sessions: &[Value]) -> Value {
    if sessions.len() != 4 || sessions.iter().any(|s| s["success"] != true) {
        return Value::Null;
    }
    if sessions.iter().any(|s| {
        s["samples"]
            .as_array()
            .is_none_or(|v| v.len() != WARMUP + MEASURED)
            || s["checks"].as_array().is_none_or(|v| v.len() != 2)
    }) {
        return Value::Null;
    }
    for index in 0..WARMUP + MEASURED {
        let first = &sessions[0]["samples"][index];
        if first["source_rgb_sha256"]
            .as_str()
            .is_none_or(|hash| hash.len() != 64)
            || first["marker"].as_u64().is_none()
        {
            return Value::Null;
        }
        for session in sessions {
            let sample = &session["samples"][index];
            if sample["status"] != "ok"
                || sample["source_rgb_sha256"] != first["source_rgb_sha256"]
                || sample["marker"] != first["marker"]
            {
                return Value::Null;
            }
        }
    }
    for index in 0..2 {
        for session in sessions {
            if session["checks"][index]["status"] != "ok"
                || session["checks"][index]["source_rgb_sha256"]
                    != sessions[0]["checks"][index]["source_rgb_sha256"]
            {
                return Value::Null;
            }
        }
    }
    let mut metrics = serde_json::Map::new();
    for metric in METRICS {
        let mut arms = [Vec::new(), Vec::new()];
        let mut medians = Vec::new();
        for (index, session) in sessions.iter().enumerate() {
            if session["legacy_forced_keyframe"] != ORDER[index] {
                return Value::Null;
            }
            let values: Vec<f64> = session["samples"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|s| s["phase"] == "measured")
                .map(|s| s[metric].as_f64().unwrap())
                .collect();
            if values.len() != MEASURED {
                return Value::Null;
            }
            medians.push(quantiles(&values)["p50_ms"].as_f64().unwrap());
            arms[usize::from(!ORDER[index])].extend(values);
        }
        let baseline = metric_quantiles(&arms[0]);
        let candidate = metric_quantiles(&arms[1]);
        metrics.insert(metric.into(),json!({"A_forced_keyframe":baseline,"B_delta":candidate,"p50_delta_B_minus_A":candidate["p50"].as_f64().unwrap()-baseline["p50"].as_f64().unwrap(),"block_median_deltas_B_minus_A":[medians[1]-medians[0],medians[2]-medians[3]],"unit":if metric=="encoded_bytes" {"bytes"} else {"ms"}}));
    }
    json!({"metrics":metrics,"interpretation":"same-process ABBA identical source RGB; negative B-A is lower. Descriptive small samples, no performance pass threshold; regressions retained."})
}
fn metric_quantiles(values: &[f64]) -> Value {
    let q = quantiles(values);
    json!({"n":q["n"],"min":q["min_ms"],"p50":q["p50_ms"],"p95":q["p95_ms"],"max":q["max_ms"]})
}
struct Session {
    report: Report,
    probe: Arc<host::ProofDeltaProbe>,
    legacy: bool,
    checks: Vec<Value>,
}
struct ProofReport {
    sessions: Vec<Session>,
    success: bool,
    run: u16,
}
impl Drop for ProofReport {
    fn drop(&mut self) {
        let sessions: Vec<Value> = self
            .sessions
            .iter_mut()
            .enumerate()
            .map(|(index, s)| {
                let mut value = s.report.value();
                let frames = s.probe.snapshot();
                let events = s.probe.events_snapshot();
                let origin = frames.first().map(|f|f.captured_at).or_else(||s.report.timelines.first().map(|t|t.injected));
                for sample in value["samples"].as_array_mut().unwrap() {
                    let id = sample["sample"].as_u64().unwrap() as u8 + 1;
                    let word = marker_word(self.run, id);
                    sample["source_rgb_sha256"] = json!(rgb_hash(&small_pixels(word, false)));
                    if sample["status"] == "ok" {
                        let previous = sample["previous_sequence"].as_u64().unwrap();
                        let presented = sample["presented_sequence"].as_u64().unwrap();
                        let interval: Vec<_> = frames
                            .iter()
                            .filter(|f| f.sequence > previous && f.sequence <= presented)
                            .collect();
                        sample["encoded_bytes"] =
                            json!(interval.iter().map(|f| f.encoded_bytes as u64).sum::<u64>());
                        sample["jpeg_ms"] = json!(
                            interval.iter().map(|f| f.jpeg_ns).sum::<u64>() as f64 / 1_000_000.0
                        );
                        sample["encoded_frame_count"] = json!(interval.len());
                        if let (Some(timeline), Some(frame), Some(origin)) = (s.report.timelines.get(id as usize - 1), frames.iter().find(|f|f.sequence==presented), origin) {
                            sample["timing"] = sample_timing(timeline, frame, &events, frames.iter().find(|f|f.sequence==previous), origin);
                        }
                    }
                }
                for check in &mut s.checks {
                    if check["status"] == "pending" {
                        check["status"] = json!("failed");
                    }
                }
                value["session_order"] = json!(index);
                value["legacy_forced_keyframe"] = json!(s.legacy);
                value["checks"] = json!(s.checks);
                value["frames"] = json!(frames.iter().map(|f| {
                    let mut value = evidence_value(f);
                    if let Some(origin) = origin { value["timing_offsets_ms"] = frame_timing(f,origin); }
                    value
                }).collect::<Vec<_>>());
                value["stage_events"] = json!(events.iter().map(|event|json!({"native_serial":event.native_serial,"source_epoch":event.source_epoch,"geometry_epoch":event.geometry_epoch,"sequence":event.sequence,"kind":format!("{:?}",event.kind),"offset_ms":origin.map(|origin|signed_ms(event.at,origin))})).collect::<Vec<_>>());
                value["timeline_clock"] = json!("single-process Instant offsets from this session's first captured proof; negative app_commit_to_capture_start means overlapping outstanding capture, not clamped. plan-to-flush includes scan/encoding/writer work; flush-to-GL includes receive/decode/viewer scheduling/GL readback, not physical presentation.");
                value
            })
            .collect();
        let comparison = if self.success {
            compare(&sessions)
        } else {
            Value::Null
        };
        let valid = self.success && !comparison.is_null();
        let report = json!({"schema_version":1,"benchmark":"proof_delta_abba","success":valid,"shared_run_token":self.run,"order":["A_forced_keyframe","B_delta","B_delta","A_forced_keyframe"],"sessions_requested":4,"warmup_per_session":WARMUP,"measured_per_session":MEASURED,"samples_requested":72,"configuration":"same source RGB per sample/check across all four new sessions; only test-only proof keyframe policy differs; production input wake and retained capture scheduling unchanged","workload":"96x72 unique marker on stable 200x150 red canvas (76.96% stable); same-pixel fresh proof and full-canvas change checkpoints","cost_scope":"successful published native-proof frames in (previous presented sequence,current presented sequence]; bytes include source envelope 8B, exclude transport padding/header/tag; JPEG duration is encoder JPEG time, not total sender CPU","failure":if self.success {Value::Null} else {json!("incomplete or failed session/check; all acquired samples/evidence retained")},"sessions":sessions,"comparison":comparison});
        let encoded = serde_json::to_string_pretty(&report).unwrap();
        eprintln!("JWM_REMOTE_PROOF_DELTA_REPORT\n{encoded}");
        if let Ok(path) = std::env::var("JWM_REMOTE_LATENCY_REPORT") {
            if let Err(error) = std::fs::write(&path, encoded) {
                eprintln!("proof-delta artifact write failed: {error}");
                if !thread::panicking() {
                    panic!("required proof-delta artifact write failed: {error}");
                }
            }
        }
        if self.success && !valid && !thread::panicking() {
            panic!(
                "complete proof-delta sessions failed same-RGB/complete-sample comparison validation"
            );
        }
    }
}
#[test]
#[ignore = "same-binary owned TCP/Wayland/GL proof-delta ABBA; explicit CI only"]
fn controlled_proof_delta_abba_latency_and_bytes() {
    let mut result = ProofReport {
        sessions: Vec::new(),
        success: false,
        run: rand::random(),
    };
    for legacy in ORDER {
        let mut report = Report::new();
        report.emit = false;
        let probe = Arc::new(host::ProofDeltaProbe::default());
        result.sessions.push(Session {
            report,
            probe: probe.clone(),
            legacy,
            checks: Vec::new(),
        });
        let session = result.sessions.last_mut().unwrap();
        let capture = probe.clone();
        run_session_workload(
            &mut session.report,
            |gl| Harness::new_with_proof_keyframes(gl, false, legacy, capture),
            Some(result.run),
            |h, word| commit_pixels(h, &small_pixels(word, false)),
            |bytes| decode_small(bytes, false),
            |h, gl, started, word, previous| {
                checkpoints(
                    h,
                    gl,
                    started,
                    word,
                    previous,
                    legacy,
                    &probe,
                    &mut session.checks,
                )
            },
        );
        session.report.success = false;
        session.report.failure = "missing published proof or partial-delta coverage".into();
        let frames = probe.snapshot();
        let events = probe.events_snapshot();
        assert_eq!(session.report.timelines.len(), WARMUP + MEASURED);
        let first_proof = frames
            .first()
            .expect("session must publish native proof evidence");
        let mut prior_sample_serial = None;
        for sample in &session.report.samples {
            let seq = sample["presented_sequence"].as_u64().unwrap();
            let proof = frames
                .iter()
                .find(|f| f.sequence == seq)
                .expect("every successful sample needs matching published native proof evidence");
            assert_eq!(
                proof.source_epoch, first_proof.source_epoch,
                "sample source scope must stay fixed"
            );
            assert_eq!(
                proof.geometry_epoch, first_proof.geometry_epoch,
                "sample geometry scope must stay fixed"
            );
            assert!(
                prior_sample_serial.is_none_or(|prior| proof.native_serial > prior),
                "each input response must carry a strictly newer native proof"
            );
            assert!(
                proof.captured_at <= proof.capture_completed_at
                    && proof.capture_completed_at <= proof.planned_at
                    && proof.planned_at <= proof.flushed_at
            );
            for kind in [
                host::ProofStageKind::AckReceived,
                host::ProofStageKind::ReadyPublished,
            ] {
                assert!(
                    events
                        .iter()
                        .any(|event| matches_event(event, proof) && event.kind == kind),
                    "successful sample requires scoped ACK/Ready timeline evidence"
                );
            }
            prior_sample_serial = Some(proof.native_serial);
        }
        if legacy {
            assert!(frames.iter().all(|f| f.keyframe));
        } else {
            assert!(
                frames
                    .iter()
                    .any(|f| !f.keyframe && f.dirty_tiles > 0 && f.dirty_tiles < f.total_tiles),
                "small marker must exercise partial-delta encoding"
            );
        }
        session.report.success = true;
    }
    result.success = true;
}
#[test]
fn proof_delta_small_workload_is_stable_and_full_change_changes_every_rgb_pixel() {
    let word = marker_word(0x8217, 3);
    let a = small_pixels(word, false);
    let b = small_pixels(marker_word(0x8217, 4), false);
    let stable = a
        .chunks_exact(4)
        .zip(b.chunks_exact(4))
        .filter(|(x, y)| x == y)
        .count();
    assert!(stable * 4 > SW as usize * SH as usize * 3);
    let full = small_pixels(!word, true);
    assert!(
        a.chunks_exact(4)
            .zip(full.chunks_exact(4))
            .all(|(x, y)| x[..3] != y[..3])
    );
    let mut scaled = vec![0; (VW * VH * 4) as usize];
    for y in 0..VH {
        for x in 0..VW {
            let src = (((y / 2) * SW + x / 2) * 4) as usize;
            let dst = ((y * VW + x) * 4) as usize;
            scaled[dst..dst + 4].copy_from_slice(&full[src..src + 4]);
        }
    }
    assert_eq!(
        decode_small(&scaled, true),
        Some(!word),
        "complemented full-change marker remains checksum-valid"
    );
    assert_ne!(rgb_hash(&a), rgb_hash(&full));
    assert_eq!(rgb_hash(&a), rgb_hash(&small_pixels(word, false)));
    assert_eq!(compare(&[]), Value::Null);
}

#[test]
fn proof_delta_comparison_rejects_different_rgb_failed_and_incomplete_sessions() {
    let mut sessions = Vec::new();
    for legacy in ORDER {
        let samples: Vec<Value> = (0..WARMUP + MEASURED).map(|index| {
            let word = marker_word(0x8217, (index+1) as u8);
            json!({"status":"ok","phase":if index<WARMUP {"warmup"} else {"measured"},"marker":word,"source_rgb_sha256":rgb_hash(&small_pixels(word,false)),"encoded_bytes":if legacy {100.0} else {120.0},"jpeg_ms":1.0,"app_commit_to_gl_verified_ms":10.0,"inject_to_present_observed_ms":20.0})
        }).collect();
        let word = marker_word(0x8217, (WARMUP + MEASURED) as u8);
        sessions.push(json!({"success":true,"legacy_forced_keyframe":legacy,"samples":samples,"checks":[{"status":"ok","source_rgb_sha256":rgb_hash(&small_pixels(word,false))},{"status":"ok","source_rgb_sha256":rgb_hash(&small_pixels(!word,true))}]}));
    }
    assert_eq!(
        compare(&sessions)["metrics"]["encoded_bytes"]["block_median_deltas_B_minus_A"],
        json!([20.0, 20.0]),
        "regression evidence retained"
    );
    let hash = sessions[1]["samples"][3]["source_rgb_sha256"].clone();
    sessions[1]["samples"][3]["source_rgb_sha256"] = json!("0".repeat(64));
    assert_eq!(compare(&sessions), Value::Null);
    sessions[1]["samples"][3]["source_rgb_sha256"] = hash;
    sessions[1]["success"] = json!(false);
    assert_eq!(compare(&sessions), Value::Null);
    sessions[1]["success"] = json!(true);
    sessions[1]["samples"].as_array_mut().unwrap().pop();
    assert_eq!(compare(&sessions), Value::Null);
}

#[test]
fn proof_delta_signed_timing_keeps_capture_overlap_and_telescopes() {
    let base = Instant::now();
    let capture = base + Duration::from_millis(2);
    let commit = base + Duration::from_millis(5);
    let end = base + Duration::from_millis(8);
    let plan = base + Duration::from_millis(10);
    let flush = base + Duration::from_millis(12);
    let gl = base + Duration::from_millis(16);
    assert_eq!(signed_ms(capture, commit), -3.0);
    let sum = signed_ms(capture, commit)
        + signed_ms(end, capture)
        + signed_ms(plan, end)
        + signed_ms(flush, plan)
        + signed_ms(gl, flush);
    assert_eq!(sum, signed_ms(gl, commit));
}
