//! Same-process ABBA of input notification only. Retained-frame scheduling,
//! FPS, input gates, marker workload and controlled feedback remain identical.
use super::*;

const ORDER: [bool; 4] = [false, true, true, false];
const STAGES: [&str; 5] = [
    "inject_to_recipient_observed_ms",
    "recipient_observed_to_app_commit_ms",
    "app_commit_to_gl_verified_ms",
    "gl_verified_to_present_observed_ms",
    "inject_to_present_observed_ms",
];
fn comparison(sessions: &[Value]) -> Value {
    if sessions.len() != ORDER.len() || sessions.iter().any(|s| s["success"] != true) {
        return Value::Null;
    }
    let mut stages = serde_json::Map::new();
    for name in STAGES {
        let mut values = [Vec::new(), Vec::new()];
        let mut medians = Vec::new();
        for (index, session) in sessions.iter().enumerate() {
            if session["input_wake_enabled"] != ORDER[index] {
                return Value::Null;
            }
            let measured: Vec<_> = session["samples"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|s| s["phase"] == "measured" && s["status"] == "ok")
                .map(|s| s[name].as_f64().unwrap())
                .collect();
            if measured.len() != MEASURED {
                return Value::Null;
            }
            medians.push(quantiles(&measured)["p50_ms"].as_f64().unwrap());
            values[usize::from(ORDER[index])].extend(measured);
        }
        let baseline = quantiles(&values[0]);
        let candidate = quantiles(&values[1]);
        stages.insert(name.into(), json!({"baseline_A_no_wake":baseline,"candidate_B_input_wake":candidate,
            "pooled_p50_delta_B_minus_A_ms":candidate["p50_ms"].as_f64().unwrap()-baseline["p50_ms"].as_f64().unwrap(),
            "pooled_p95_delta_B_minus_A_ms":candidate["p95_ms"].as_f64().unwrap()-baseline["p95_ms"].as_f64().unwrap(),
            "block_median_deltas_B_minus_A_ms":[medians[1]-medians[0],medians[2]-medians[3]]}));
    }
    json!({"stages":stages,"interpretation":"same binary/process AB then BA; negative B-A is lower observed latency. All results including regressions retained; descriptive small-sample comparison, no performance threshold or cross-CI-run comparison."})
}
struct Session {
    report: Report,
    probe: Arc<host::InputWakeProbe>,
    wake_enabled: bool,
}
struct WakeReport {
    sessions: Vec<Session>,
    success: bool,
}
impl Drop for WakeReport {
    fn drop(&mut self) {
        let sessions: Vec<Value> = self.sessions.iter_mut().enumerate().map(|(index, session)| {
            let mut value = session.report.value();
            let counts = session.probe.snapshot();
            value["session_order"] = json!(index);
            value["arm"] = json!(if session.wake_enabled { "B_input_wake" } else { "A_no_wake" });
            value["input_wake_enabled"] = json!(session.wake_enabled);
            value["order_block"] = json!(index / 2);
            value["wake_counters"] = json!({"notify_attempts":counts.notify_attempts,"notify_coalesced":counts.notify_coalesced,"drain_calls":counts.drain_calls,"drained_signals":counts.drained_signals,"activity_waits":counts.activity_waits});
            value
        }).collect();
        let compared = if self.success {
            comparison(&sessions)
        } else {
            Value::Null
        };
        let report = json!({"schema_version":1,"benchmark":"input_wake_abba","success":self.success,
            "configuration":"identical binary, process, fps, quality, selected surfaces, input gates, retained-frame billing and response workload; only the per-host-thread test-only input-wake switch differs",
            "order":["A_no_wake","B_input_wake","B_input_wake","A_no_wake"],"sessions_requested":4,"warmup_per_session":WARMUP,"measured_per_session":MEASURED,"warmup_per_arm":2*WARMUP,"measured_per_arm":2*MEASURED,
            "lease_policy":"fresh session per occurrence; no renewal; 25s checked budget per session including setup, bounded teardown excluded from latency measurement",
            "counter_scope":"per-host-thread owned collector, whole session including warmup/setup/cleanup; eventfd accumulated hints are not input-action counts or CPU utilization. Activity waits count calls, not early poll wake returns; no poll-return cause counter is collected. Generous fixed iteration bounds catch runaway loops, not a hardware performance gate.",
            "failure":if self.success {Value::Null} else {json!("session setup, sample, cleanup or wake invariant failed; incomplete comparisons suppressed, all acquired data retained")},"sessions":sessions,"comparison":compared});
        let encoded = serde_json::to_string_pretty(&report).unwrap();
        eprintln!("JWM_REMOTE_INPUT_WAKE_AB_REPORT\n{encoded}");
        if let Ok(path) = std::env::var("JWM_REMOTE_LATENCY_REPORT") {
            if let Err(error) = std::fs::write(&path, encoded) {
                eprintln!("failed to write input-wake artifact {path:?}: {error}");
                if !thread::panicking() {
                    panic!("required input-wake artifact write failed: {error}");
                }
            }
        }
    }
}

#[test]
#[ignore = "same-process owned TCP/Wayland/GL input-wake ABBA; explicit CI only"]
fn controlled_input_wake_abba_latency() {
    let mut result = WakeReport {
        sessions: Vec::new(),
        success: false,
    };
    for wake_enabled in ORDER {
        let mut report = Report::new();
        report.emit = false;
        let probe = Arc::new(host::InputWakeProbe::default());
        result.sessions.push(Session {
            report,
            probe: probe.clone(),
            wake_enabled,
        });
        let session = result.sessions.last_mut().unwrap();
        run_session(&mut session.report, |gl| {
            Harness::new_with_input_wake(gl, false, wake_enabled, probe)
        });
        session.report.success = false;
        session.report.failure =
            "input-wake counter invariant failed after completed samples".into();
        let counts = session.probe.snapshot();
        assert!(
            counts.activity_waits <= 100_000,
            "bounded session must not spin in activity wait"
        );
        assert!(
            counts.notify_attempts <= 4096,
            "bounded input workload must not cause a notification storm"
        );
        assert!(
            counts.activity_waits > 0,
            "both arms must exercise native activity waits"
        );
        assert!(counts.notify_coalesced <= counts.notify_attempts);
        assert!(counts.drained_signals <= counts.notify_attempts);
        if wake_enabled {
            assert!(
                counts.notify_attempts > 0 && counts.drain_calls > 0 && counts.drained_signals > 0,
                "candidate must actually publish and consume input-wake hints"
            );
        } else {
            assert_eq!(
                counts.notify_attempts, 0,
                "baseline must not publish wake hints"
            );
            assert_eq!(
                counts.drain_calls, 0,
                "baseline must not consume wake hints"
            );
            assert_eq!(counts.drained_signals, 0);
        }
        session.report.success = true;
    }
    result.success = true;
}

#[test]
fn input_wake_comparison_keeps_regressions_and_rejects_failed_or_mixed_arms() {
    assert_eq!(comparison(&[]), Value::Null);
    let mut sessions = Vec::new();
    for enabled in ORDER {
        let samples: Vec<Value> = (0..MEASURED)
            .map(|_| {
                let mut sample = json!({"phase":"measured","status":"ok"});
                for name in STAGES {
                    sample[name] = json!(if enabled { 30.0 } else { 20.0 });
                }
                sample
            })
            .collect();
        sessions.push(json!({"success":true,"input_wake_enabled":enabled,"samples":samples}));
    }
    let compared = comparison(&sessions);
    assert_eq!(
        compared["stages"][STAGES[4]]["block_median_deltas_B_minus_A_ms"],
        json!([10.0, 10.0]),
        "negative performance evidence must not be hidden"
    );
    assert_eq!(compared["stages"][STAGES[4]]["baseline_A_no_wake"]["n"], 32);
    sessions[1]["success"] = json!(false);
    assert_eq!(comparison(&sessions), Value::Null);
    sessions[1]["success"] = json!(true);
    sessions[1]["input_wake_enabled"] = json!(false);
    assert_eq!(comparison(&sessions), Value::Null);
}
