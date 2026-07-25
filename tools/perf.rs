//! Collection and evaluation front-end for the Phase 5 performance contract.
//!
//! `record` drives the contract scenarios against the live JWM session over
//! the IPC socket plus `/proc` sampling and writes one labeled baseline.
//! Scenarios the running session cannot measure are recorded as skipped with
//! the reason, never silently omitted. `compare` evaluates a candidate
//! against a baseline under the version-1 budgets and refuses mismatched or
//! unlabeled results.

use crate::perf_contract::{
    self, PerfBaselineV1, ScenarioResult, SystemLabel, VerdictOutcome, default_budgets,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub struct RecordOptions {
    pub out: Option<PathBuf>,
    pub frames: u32,
    pub warmup: u32,
    pub idle_seconds: u32,
    /// Drive a continuous animation during the benchmark window so frame
    /// pacing does not depend on ambient desktop activity.
    pub waterlily_workload: bool,
}

// ---------------------------------------------------------------------------
// IPC plumbing
// ---------------------------------------------------------------------------

fn ipc_call(request: &Value) -> Result<Value, String> {
    let path = crate::ipc_socket_path();
    if !path.exists() {
        return Err(format!(
            "IPC socket not found at {}; is JWM running?",
            path.display()
        ));
    }
    let mut stream = UnixStream::connect(&path)
        .map_err(|error| format!("connect {}: {error}", path.display()))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| error.to_string())?;
    let mut line = serde_json::to_string(request).map_err(|error| error.to_string())?;
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .map_err(|error| error.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader
        .read_line(&mut response)
        .map_err(|error| error.to_string())?;
    serde_json::from_str(response.trim()).map_err(|error| format!("malformed response: {error}"))
}

fn ipc_query(name: &str) -> Result<Value, String> {
    let response = ipc_call(&serde_json::json!({ "query": name, "args": {} }))?;
    if response.get("success").and_then(Value::as_bool) == Some(false) {
        return Err(response
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("query failed")
            .to_string());
    }
    Ok(response.get("data").cloned().unwrap_or(Value::Null))
}

fn ipc_command(name: &str, args: Value) -> Result<(), String> {
    let response = ipc_call(&serde_json::json!({ "command": name, "args": args }))?;
    if response.get("success").and_then(Value::as_bool) == Some(false) {
        return Err(response
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("command failed")
            .to_string());
    }
    Ok(())
}

fn metric_f64(value: &Value, key: &str) -> Option<f64> {
    value.get(key).and_then(Value::as_f64)
}

// ---------------------------------------------------------------------------
// System label
// ---------------------------------------------------------------------------

fn cpu_model() -> String {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|content| {
            content.lines().find_map(|line| {
                line.strip_prefix("model name")
                    .and_then(|rest| rest.split(':').nth(1))
                    .map(|name| name.trim().to_string())
            })
        })
        .unwrap_or_else(|| "unknown".into())
}

fn kernel_release() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|value| value.trim().to_string())
        .unwrap_or_else(|_| "unknown".into())
}

/// GPU model fallback for sessions whose benchmark report predates GL-string
/// capture: NVIDIA's procfs first, then the bound DRM driver name.
fn gpu_fallback() -> Option<String> {
    if let Ok(entries) = glob::glob("/proc/driver/nvidia/gpus/*/information") {
        for path in entries.flatten() {
            if let Ok(info) = std::fs::read_to_string(&path)
                && let Some(model) = info.lines().find_map(|line| {
                    line.strip_prefix("Model:")
                        .map(|value| value.trim().to_string())
                })
            {
                return Some(model);
            }
        }
    }
    None
}

/// Driver identity fallback: the NVIDIA kernel-module version, else the DRM
/// driver bound to card0.
fn driver_fallback() -> Option<String> {
    if let Ok(version) = std::fs::read_to_string("/proc/driver/nvidia/version")
        && let Some(line) = version.lines().next()
    {
        let release = line
            .split_whitespace()
            .find(|token| {
                token.chars().next().is_some_and(|c| c.is_ascii_digit()) && token.contains('.')
            })
            .unwrap_or("");
        if !release.is_empty() {
            return Some(format!("nvidia {release}"));
        }
    }
    std::fs::read_link("/sys/class/drm/card0/device/driver")
        .ok()
        .and_then(|target| {
            target
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
}

/// Renderer API from an explicit configuration choice. `auto` resolves at
/// runtime and is deliberately not trusted as a label.
fn renderer_api_from_config(backend: &str) -> Option<String> {
    let choice = backend.parse::<jwm::application::BackendChoice>().ok()?;
    let path = jwm::application::config_path(choice);
    let content = std::fs::read_to_string(path).ok()?;
    let parsed: toml::Table = toml::from_str(&content).ok()?;
    let api = parsed
        .get("behavior")
        .and_then(|section| section.get("compositor_api"))
        .and_then(toml::Value::as_str)?;
    match api {
        "egl" | "gles" | "egl-gles" | "egl_gles" => Some("egl/gles3".to_string()),
        "glx" | "opengl" => Some("glx/opengl".to_string()),
        _ => None,
    }
}

/// Deterministic FNV-1a 64 over the on-disk configuration file. Hashing the
/// file (not the effective config JSON) keeps the fingerprint stable across
/// jwm versions that add new defaulted settings.
fn config_fingerprint(backend: &str) -> String {
    let Ok(choice) = backend.parse::<jwm::application::BackendChoice>() else {
        return "unknown".into();
    };
    let path = jwm::application::config_path(choice);
    let bytes = std::fs::read(&path).unwrap_or_default();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

// ---------------------------------------------------------------------------
// /proc sampling for the idle scenario
// ---------------------------------------------------------------------------

struct ProcSample {
    cpu_ticks: u64,
    voluntary_switches: u64,
    rss_kb: u64,
}

fn sample_proc(pid: u32) -> Result<ProcSample, String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map_err(|error| format!("/proc/{pid}/stat: {error}"))?;
    // Fields after the parenthesised comm, which may itself contain spaces.
    let after_comm = stat
        .rsplit_once(')')
        .map(|(_, rest)| rest.trim_start())
        .ok_or("malformed /proc stat")?;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // Post-comm index: state=0, ..., utime=11, stime=12.
    let utime: u64 = fields
        .get(11)
        .and_then(|value| value.parse().ok())
        .ok_or("missing utime")?;
    let stime: u64 = fields
        .get(12)
        .and_then(|value| value.parse().ok())
        .ok_or("missing stime")?;

    let status = std::fs::read_to_string(format!("/proc/{pid}/status"))
        .map_err(|error| format!("/proc/{pid}/status: {error}"))?;
    let field = |name: &str| {
        status.lines().find_map(|line| {
            line.strip_prefix(name)?
                .trim_start_matches(':')
                .split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()
        })
    };
    Ok(ProcSample {
        cpu_ticks: utime + stime,
        voluntary_switches: field("voluntary_ctxt_switches").unwrap_or(0),
        rss_kb: field("VmRSS").unwrap_or(0),
    })
}

/// Find the compositor pid: the versioned status reports it directly on
/// current builds; older sessions fall back to the daemon's child, then to a
/// process-name scan.
fn compositor_pid(status: &Value) -> Result<u32, String> {
    if let Some(pid) = status.get("pid").and_then(Value::as_u64) {
        return u32::try_from(pid).map_err(|_| "status pid out of range".into());
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    if let Some(runtime) = runtime
        && let Ok(daemon) = std::fs::read_to_string(runtime.join("jwm_daemon.pid"))
        && let Ok(daemon_pid) = daemon.trim().parse::<u32>()
        && let Ok(children) =
            std::fs::read_to_string(format!("/proc/{daemon_pid}/task/{daemon_pid}/children"))
        && let Some(child) = children.split_whitespace().next()
        && let Ok(pid) = child.parse::<u32>()
    {
        return Ok(pid);
    }
    // Last resort: unique process whose comm is exactly "jwm".
    let mut candidates = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(pid) = name.to_str().and_then(|value| value.parse::<u32>().ok()) else {
                continue;
            };
            if std::fs::read_to_string(format!("/proc/{pid}/comm"))
                .is_ok_and(|comm| comm.trim() == "jwm")
            {
                candidates.push(pid);
            }
        }
    }
    candidates.sort_unstable();
    match candidates.as_slice() {
        [pid] => Ok(*pid),
        [] => Err(
            "could not determine the compositor pid (no status pid, daemon, or jwm process)".into(),
        ),
        many => Ok(many[0]),
    }
}

// ---------------------------------------------------------------------------
// record
// ---------------------------------------------------------------------------

pub fn run_record(options: &RecordOptions) -> io::Result<()> {
    record_baseline(options).map_err(|error| io::Error::other(error))
}

fn record_baseline(options: &RecordOptions) -> Result<(), String> {
    let status = ipc_query("get_status")?;
    let backend = status
        .get("backend")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let jwm_version = status
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    eprintln!("perf record: live session backend={backend} version={jwm_version}");

    let mut scenarios: BTreeMap<String, ScenarioResult> = BTreeMap::new();

    // -- idle -------------------------------------------------------------
    let idle_result = match compositor_pid(&status) {
        Err(reason) => ScenarioResult::skipped(reason),
        Ok(pid) => {
            let seconds = options.idle_seconds.max(2);
            eprintln!(
                "perf record: sampling idle pid {pid} for {seconds}s (leave the session untouched)"
            );
            match record_idle(pid, seconds) {
                Ok(metrics) => ScenarioResult::recorded(metrics),
                Err(reason) => ScenarioResult::skipped(reason),
            }
        }
    };
    scenarios.insert("idle".into(), idle_result);

    // -- compositor-backed scenarios ---------------------------------------
    let metrics_before = ipc_query("get_metrics")
        .ok()
        .filter(|value| value.is_object());
    let mut gpu = "unknown".to_string();
    let mut driver = "unknown".to_string();
    let mut resolution = "unknown".to_string();
    let mut renderer_api = "unknown".to_string();

    match metrics_before {
        None => {
            let reason = "compositor inactive: no get_metrics data".to_string();
            for scenario in [
                "steady_frame",
                "damage_redraw",
                "input_latency",
                "allocation_steady",
                "direct_scanout",
            ] {
                scenarios.insert(scenario.into(), ScenarioResult::skipped(reason.clone()));
            }
        }
        Some(before) => {
            renderer_api = before
                .get("renderer_api")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .unwrap_or("unknown")
                .to_string();
            let allocations_before = status.get("allocations").and_then(Value::as_u64);
            let frames_before = metric_f64(&before, "frame_count").unwrap_or(0.0);

            eprintln!(
                "perf record: benchmarking {} frames (warmup {}) at ambient workload",
                options.frames, options.warmup
            );
            if options.waterlily_workload {
                eprintln!("perf record: enabling the waterlily animation as a paced workload");
                ipc_command("toggle_waterlily", serde_json::json!({}))?;
            }
            ipc_command(
                "benchmark",
                serde_json::json!({
                    "action": "start",
                    "frames": options.frames,
                    "warmup": options.warmup,
                }),
            )?;

            let window_start = Instant::now();
            let deadline = window_start + Duration::from_secs(300);
            let mut samples: Vec<Value> = Vec::new();
            let report = loop {
                std::thread::sleep(Duration::from_secs(1));
                if let Ok(sample) = ipc_query("get_metrics")
                    && sample.is_object()
                {
                    samples.push(sample);
                }
                match ipc_query("benchmark_report") {
                    Ok(report) if report.is_object() || report.is_string() => break Some(report),
                    _ => {}
                }
                if Instant::now() > deadline {
                    break None;
                }
            };
            if options.waterlily_workload {
                let _ = ipc_command("toggle_waterlily", serde_json::json!({}));
            }
            let window_minutes = window_start.elapsed().as_secs_f64() / 60.0;
            let after = ipc_query("get_metrics").unwrap_or(Value::Null);
            let status_after = ipc_query("get_status").unwrap_or(Value::Null);

            // benchmark_report may arrive as a JSON string payload.
            let report = report.map(|value| match value {
                Value::String(text) => serde_json::from_str(&text).unwrap_or(Value::String(text)),
                other => other,
            });

            match &report {
                None => {
                    scenarios.insert(
                        "steady_frame".into(),
                        ScenarioResult::skipped(
                            "benchmark did not complete within 300s (too little ambient damage)",
                        ),
                    );
                    scenarios.insert(
                        "input_latency".into(),
                        ScenarioResult::skipped("benchmark did not complete"),
                    );
                }
                Some(report) => {
                    if let Some(system) = report.get("system") {
                        for (field, target) in [
                            ("gpu", &mut gpu),
                            ("driver", &mut driver),
                            ("resolution", &mut resolution),
                        ] {
                            if let Some(value) = system
                                .get(field)
                                .and_then(Value::as_str)
                                .filter(|v| !v.is_empty())
                            {
                                *target = value.to_string();
                            }
                        }
                    }
                    // Sessions predating the metrics renderer_api field:
                    // derive the API family from the GL version string the
                    // benchmark captured (GLES contexts always embed
                    // "OpenGL ES").
                    if renderer_api == "unknown" && driver != "unknown" {
                        renderer_api = if driver.contains("OpenGL ES") {
                            "egl/gles3".to_string()
                        } else {
                            "glx/opengl".to_string()
                        };
                    }
                    if gpu == "unknown"
                        && let Some(model) = gpu_fallback()
                    {
                        gpu = model;
                    }
                    if driver == "unknown"
                        && let Some(name) = driver_fallback()
                    {
                        driver = name;
                    }
                    let mut frame = BTreeMap::new();
                    if let Some(stats) = report.get("frame_time") {
                        for (metric, key) in [
                            ("frame_time_avg_ms", "avg_ms"),
                            ("frame_time_p50_ms", "p50_ms"),
                            ("frame_time_p95_ms", "p95_ms"),
                            ("frame_time_p99_ms", "p99_ms"),
                            ("frame_time_stddev_ms", "stddev_ms"),
                            ("fps_avg", "fps_avg"),
                            ("frame_samples", "count"),
                        ] {
                            if let Some(value) = metric_f64(stats, key) {
                                frame.insert(metric.to_string(), value);
                            }
                        }
                    }
                    scenarios.insert(
                        "steady_frame".into(),
                        if frame.is_empty() {
                            ScenarioResult::skipped("benchmark report carried no frame stats")
                        } else {
                            ScenarioResult::recorded(frame)
                        },
                    );

                    // Input latency: prefer the benchmark's per-run stats,
                    // fall back to the compositor's rolling window.
                    let mut latency = BTreeMap::new();
                    let from_report = report
                        .get("input_latency")
                        .filter(|stats| metric_f64(stats, "count").unwrap_or(0.0) > 0.0);
                    if let Some(stats) = from_report {
                        for (metric, key) in [
                            ("input_latency_p50_ms", "p50_ms"),
                            ("input_latency_p95_ms", "p95_ms"),
                            ("input_latency_p99_ms", "p99_ms"),
                            ("input_latency_samples", "count"),
                        ] {
                            if let Some(value) = metric_f64(stats, key) {
                                latency.insert(metric.to_string(), value);
                            }
                        }
                    } else if after.is_object() {
                        for metric in [
                            "input_latency_p50_ms",
                            "input_latency_p95_ms",
                            "input_latency_p99_ms",
                        ] {
                            if let Some(value) = metric_f64(&after, metric).filter(|v| *v > 0.0) {
                                latency.insert(metric.to_string(), value);
                            }
                        }
                    }
                    scenarios.insert(
                        "input_latency".into(),
                        if latency.is_empty() {
                            ScenarioResult::skipped(
                                "no input-to-present timestamps observed during the window",
                            )
                        } else {
                            ScenarioResult::recorded(latency)
                        },
                    );
                }
            }

            // Damage/redraw ratios: averages over the sampled window.
            let averaged = |key: &str| {
                let values: Vec<f64> = samples
                    .iter()
                    .filter_map(|sample| metric_f64(sample, key))
                    .collect();
                (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
            };
            let mut damage = BTreeMap::new();
            if let Some(value) = averaged("dirty_fraction_percent") {
                damage.insert("dirty_fraction_avg_percent".into(), value);
            }
            if let Some(value) = averaged("dirty_regions_count") {
                damage.insert("dirty_regions_avg".into(), value);
            }
            if let Some(value) = averaged("dirty_region_merge_count") {
                damage.insert("dirty_region_merges_avg".into(), value);
            }
            scenarios.insert(
                "damage_redraw".into(),
                if damage.is_empty() {
                    ScenarioResult::skipped("no damage metrics sampled")
                } else {
                    ScenarioResult::recorded(damage)
                },
            );

            // Direct scanout entry/exit stability over the window.
            let mut scanout = BTreeMap::new();
            if let (Some(first), Some(last)) = (
                metric_f64(&before, "direct_scanout_count"),
                after
                    .is_object()
                    .then(|| metric_f64(&after, "direct_scanout_count"))
                    .flatten(),
            ) {
                let toggles = (last - first).max(0.0);
                scanout.insert(
                    "scanout_toggles_per_minute".into(),
                    if window_minutes > 0.0 {
                        toggles / window_minutes
                    } else {
                        0.0
                    },
                );
            }
            if let Some(active) = after
                .is_object()
                .then(|| after.get("direct_scanout_active").and_then(Value::as_bool))
                .flatten()
            {
                scanout.insert("scanout_active_end".into(), f64::from(u8::from(active)));
            }
            scenarios.insert(
                "direct_scanout".into(),
                if scanout.is_empty() {
                    ScenarioResult::skipped("backend exposes no direct-scanout counters")
                } else {
                    ScenarioResult::recorded(scanout)
                },
            );

            // Steady-state allocations per produced frame.
            let allocations_after = status_after.get("allocations").and_then(Value::as_u64);
            let frames_after = after
                .is_object()
                .then(|| metric_f64(&after, "frame_count"))
                .flatten()
                .unwrap_or(frames_before);
            let alloc_result = match (allocations_before, allocations_after) {
                (Some(first), Some(last)) if frames_after > frames_before => {
                    let mut metrics = BTreeMap::new();
                    metrics.insert(
                        "allocs_per_frame".into(),
                        (last.saturating_sub(first)) as f64 / (frames_after - frames_before),
                    );
                    metrics.insert("frames_observed".into(), frames_after - frames_before);
                    ScenarioResult::recorded(metrics)
                }
                (Some(_), Some(_)) => {
                    ScenarioResult::skipped("no frames were produced during the window")
                }
                _ => ScenarioResult::skipped(
                    "allocation counter not compiled in (build with --features alloc-counter)",
                ),
            };
            scenarios.insert("allocation_steady".into(), alloc_result);
        }
    }

    // Explicit configuration choice is an honest last resort for the label;
    // "auto" resolves at runtime and is deliberately not trusted.
    if renderer_api == "unknown"
        && let Some(api) = renderer_api_from_config(&backend)
    {
        renderer_api = api;
    }

    // -- multi-monitor ------------------------------------------------------
    let mut monitor_metrics = BTreeMap::new();
    if let Ok(monitors) = ipc_query("get_monitors")
        && let Some(list) = monitors.as_array()
    {
        monitor_metrics.insert("monitor_count".into(), list.len() as f64);
    }
    if let Ok(metrics) = ipc_query("get_metrics")
        && let Some(value) = metric_f64(&metrics, "current_refresh_rate")
    {
        monitor_metrics.insert("refresh_hz".into(), value);
    }
    scenarios.insert(
        "multi_monitor".into(),
        if monitor_metrics.is_empty() {
            ScenarioResult::skipped("no monitor data available")
        } else {
            ScenarioResult::recorded(monitor_metrics)
        },
    );

    let label = SystemLabel {
        cpu: cpu_model(),
        gpu,
        driver,
        kernel: kernel_release(),
        backend: backend.clone(),
        renderer_api,
        resolution,
        config_fingerprint: config_fingerprint(&backend),
    };

    let baseline = PerfBaselineV1 {
        schema_version: perf_contract::SCHEMA_VERSION,
        recorded_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        jwm_version,
        label,
        scenarios,
    };

    let out = options.out.clone().unwrap_or_else(|| {
        PathBuf::from("perf/baselines").join(format!("{}.json", baseline.label.slug()))
    });
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let mut encoded = serde_json::to_string_pretty(&baseline).map_err(|error| error.to_string())?;
    encoded.push('\n');
    std::fs::write(&out, encoded).map_err(|error| error.to_string())?;

    println!("perf baseline written to {}", out.display());
    let incomplete = baseline.label.incomplete_fields();
    if incomplete.is_empty() {
        println!("label complete: {}", baseline.label.slug());
    } else {
        println!(
            "WARNING: label incomplete ({}); comparisons against this file will be refused",
            incomplete.join(", ")
        );
    }
    for (name, result) in &baseline.scenarios {
        match result.status {
            perf_contract::ScenarioStatus::Recorded => {
                println!("  {name}: recorded ({} metrics)", result.metrics.len());
            }
            perf_contract::ScenarioStatus::Skipped => {
                println!(
                    "  {name}: skipped — {}",
                    result.reason.as_deref().unwrap_or("no reason")
                );
            }
        }
    }
    Ok(())
}

fn record_idle(pid: u32, seconds: u32) -> Result<BTreeMap<String, f64>, String> {
    let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    let clk_tck = if clk_tck > 0 { clk_tck as f64 } else { 100.0 };
    let first = sample_proc(pid)?;
    let started = Instant::now();
    std::thread::sleep(Duration::from_secs(u64::from(seconds)));
    let second = sample_proc(pid)?;
    let elapsed = started.elapsed().as_secs_f64();

    let mut metrics = BTreeMap::new();
    metrics.insert(
        "cpu_percent_avg".into(),
        (second.cpu_ticks.saturating_sub(first.cpu_ticks)) as f64 / clk_tck / elapsed * 100.0,
    );
    metrics.insert(
        "wakeups_per_s".into(),
        (second
            .voluntary_switches
            .saturating_sub(first.voluntary_switches)) as f64
            / elapsed,
    );
    metrics.insert("rss_mb".into(), second.rss_kb as f64 / 1024.0);
    metrics.insert("sample_seconds".into(), elapsed);
    Ok(metrics)
}

// ---------------------------------------------------------------------------
// compare / budgets
// ---------------------------------------------------------------------------

fn load_baseline(path: &Path) -> Result<PerfBaselineV1, String> {
    let content =
        std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    serde_json::from_str(&content).map_err(|error| format!("{}: {error}", path.display()))
}

pub fn run_compare(baseline: &Path, candidate: &Path, json: bool) -> io::Result<()> {
    let result = (|| -> Result<perf_contract::CompareReport, String> {
        let baseline = load_baseline(baseline)?;
        let candidate = load_baseline(candidate)?;
        perf_contract::compare(&baseline, &candidate, &default_budgets())
    })();

    match result {
        Err(refusal) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({"comparable": false, "refusal": refusal})
                );
            } else {
                eprintln!("comparison refused: {refusal}");
            }
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "comparison refused",
            ))
        }
        Ok(report) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({"comparable": true, "report": report})
                );
            } else {
                for verdict in &report.verdicts {
                    let tag = match verdict.outcome {
                        VerdictOutcome::Pass => "PASS",
                        VerdictOutcome::Violation => "FAIL",
                        VerdictOutcome::NotComparable => "n/a ",
                    };
                    println!(
                        "[{tag}] {}/{}: {}",
                        verdict.scenario, verdict.metric, verdict.detail
                    );
                }
                println!(
                    "verdict: {}",
                    if report.passed {
                        "within budgets"
                    } else {
                        "REGRESSION beyond budgets"
                    }
                );
            }
            if report.passed {
                Ok(())
            } else {
                Err(io::Error::other("regression beyond budgets"))
            }
        }
    }
}

pub fn run_budgets(json: bool) -> io::Result<()> {
    let budgets = default_budgets();
    if json {
        println!(
            "{}",
            serde_json::json!({
                "schema_version": perf_contract::SCHEMA_VERSION,
                "scenarios": perf_contract::SCENARIOS,
                "budgets": budgets,
            })
        );
    } else {
        println!("performance contract v{}", perf_contract::SCHEMA_VERSION);
        for rule in budgets {
            let bound = match rule.direction {
                perf_contract::Direction::LowerIsBetter => {
                    format!("<= baseline x {:.2}", rule.ratio)
                }
                perf_contract::Direction::HigherIsBetter => {
                    format!(">= baseline x {:.2}", rule.ratio)
                }
                perf_contract::Direction::Exact => "must equal the baseline".to_string(),
            };
            let absolute = rule
                .absolute
                .map(|value| format!(" (absolute rail {value:.1})"))
                .unwrap_or_default();
            println!("  {}/{}: {bound}{absolute}", rule.scenario, rule.metric);
        }
    }
    Ok(())
}
