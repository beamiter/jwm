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

/// Run a command; `Ok` carries its response data (`Null` when it has none).
fn ipc_command(name: &str, args: Value) -> Result<Value, String> {
    let response = ipc_call(&serde_json::json!({ "command": name, "args": args }))?;
    if response.get("success").and_then(Value::as_bool) == Some(false) {
        return Err(response
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("command failed")
            .to_string());
    }
    Ok(response.get("data").cloned().unwrap_or(Value::Null))
}

fn metric_f64(value: &Value, key: &str) -> Option<f64> {
    value.get(key).and_then(Value::as_f64)
}

/// A benchmark report as the session sent it; older sessions send the
/// report as a JSON string payload.
fn decode_report(value: Value) -> Value {
    match value {
        Value::String(text) => serde_json::from_str(&text).unwrap_or(Value::String(text)),
        other => other,
    }
}

/// Pause between benchmark polls.
const BENCHMARK_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// How long `record` waits for the benchmark report before giving up.
const BENCHMARK_DEADLINE: Duration = Duration::from_secs(300);

/// Everything `record` needs from the running session. The live
/// implementation talks to the IPC socket and `/proc`; tests script it so the
/// recorder's skip and cleanup paths run without a session or real waits.
trait RecordSession {
    fn query(&mut self, name: &str) -> Result<Value, String>;
    /// Run a command; `Ok` carries its response data (`Null` when it has
    /// none), which `benchmark stop` uses for the report it ends with.
    fn command(&mut self, name: &str, args: Value) -> Result<Value, String>;
    /// Sample the compositor process for `seconds` (the idle scenario).
    fn sample_idle(&mut self, pid: u32, seconds: u32) -> Result<BTreeMap<String, f64>, String>;
    /// Wait one benchmark poll interval. Returns false once `deadline` has
    /// passed, so the caller makes its final poll and gives up.
    fn wait_poll(&mut self, deadline: Instant) -> bool;
}

/// The live session behind the IPC socket.
struct LiveSession;

impl RecordSession for LiveSession {
    fn query(&mut self, name: &str) -> Result<Value, String> {
        ipc_query(name)
    }

    fn command(&mut self, name: &str, args: Value) -> Result<Value, String> {
        ipc_command(name, args)
    }

    fn sample_idle(&mut self, pid: u32, seconds: u32) -> Result<BTreeMap<String, f64>, String> {
        record_idle(pid, seconds)
    }

    fn wait_poll(&mut self, deadline: Instant) -> bool {
        std::thread::sleep(BENCHMARK_POLL_INTERVAL);
        Instant::now() <= deadline
    }
}

/// The `get_metrics` payload, only when it really comes from a compositor;
/// otherwise the skip reason for the compositor-backed scenarios.
///
/// `get_metrics` answers even without a compositor (a window/monitor/tag
/// count fallback), so an ok object proves nothing. The versioned status
/// reports the renderer state directly; sessions predating that field are
/// judged by `frame_count`, which only compositor metrics carry.
fn compositor_metrics(status: &Value, metrics: Result<Value, String>) -> Result<Value, String> {
    if status.get("compositor_active").and_then(Value::as_bool) == Some(false) {
        return Err("compositor inactive: get_status reports compositor_active=false".into());
    }
    let metrics =
        metrics.map_err(|error| format!("compositor inactive: get_metrics failed: {error}"))?;
    if metrics.get("frame_count").is_some_and(Value::is_number) {
        Ok(metrics)
    } else {
        Err("compositor inactive: get_metrics carried no compositor metrics".into())
    }
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

/// GPU model fallback for runs without a GL-string report (legacy sessions,
/// no compositor, a refused or skipped benchmark): the model NVIDIA's procfs
/// reports; `None` elsewhere, where only `driver_fallback` has anything to
/// say.
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

/// Resolution label fallback for runs without a benchmark report: the extent
/// the `get_monitors` entries span, in the report's `{screen_w}x{screen_h}`
/// form. The report measures the whole screen, not one monitor, so a
/// multi-monitor machine keeps one label either way.
fn screen_extent(monitors: &[Value]) -> Option<String> {
    let mut width = 0_i64;
    let mut height = 0_i64;
    for monitor in monitors {
        let field = |key: &str| monitor.get(key).and_then(Value::as_i64);
        width = width.max(field("x")?.saturating_add(field("w")?));
        height = height.max(field("y")?.saturating_add(field("h")?));
    }
    (width > 0 && height > 0).then(|| format!("{width}x{height}"))
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
    record_baseline(&mut LiveSession, options)
        .and_then(|baseline| write_baseline(options, &baseline))
        .map_err(io::Error::other)
}

/// Measure every contract scenario against `session`. Only a session that
/// cannot report its status fails the run; a scenario the session cannot
/// measure is recorded as skipped with the reason.
fn record_baseline(
    session: &mut dyn RecordSession,
    options: &RecordOptions,
) -> Result<PerfBaselineV1, String> {
    let status = session.query("get_status")?;
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
            match session.sample_idle(pid, seconds) {
                Ok(metrics) => ScenarioResult::recorded(metrics),
                Err(reason) => ScenarioResult::skipped(reason),
            }
        }
    };
    scenarios.insert("idle".into(), idle_result);

    // -- compositor-backed scenarios ---------------------------------------
    let mut gpu = "unknown".to_string();
    let mut driver = "unknown".to_string();
    let mut resolution = "unknown".to_string();
    let mut renderer_api = "unknown".to_string();

    let measured = match compositor_metrics(&status, session.query("get_metrics")) {
        Err(reason) => Err(reason),
        Ok(before) => {
            renderer_api = before
                .get("renderer_api")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .unwrap_or("unknown")
                .to_string();
            eprintln!(
                "perf record: benchmarking {} frames (warmup {}) at ambient workload",
                options.frames, options.warmup
            );
            run_benchmark_window(session, options).map(|window| (before, window))
        }
    };

    match measured {
        Err(reason) => {
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
        Ok((before, window)) => {
            let allocations_before = status.get("allocations").and_then(Value::as_u64);
            let frames_before = metric_f64(&before, "frame_count").unwrap_or(0.0);
            let BenchmarkWindow {
                report,
                stopped_report,
                samples,
                minutes: window_minutes,
            } = window;
            let after = session.query("get_metrics").unwrap_or(Value::Null);
            let status_after = session.query("get_status").unwrap_or(Value::Null);

            let report = report.map(decode_report);
            let stopped_report = stopped_report.map(decode_report);
            // The label comes from the completed report, else from the one
            // `benchmark stop` ended an overdue run with: the same GL strings
            // and screen size either way, so a stalled candidate keeps the
            // label of the baseline it must be judged against. The stopped
            // run's frame stats cover an incomplete window and stay unused.
            if let Some(system) = report
                .as_ref()
                .or(stopped_report.as_ref())
                .and_then(|report| report.get("system"))
            {
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
                // Sessions predating the metrics renderer_api field: derive
                // the API family from the GL version string the benchmark
                // captured (GLES contexts always embed "OpenGL ES").
                if renderer_api == "unknown" && driver != "unknown" {
                    renderer_api = if driver.contains("OpenGL ES") {
                        "egl/gles3".to_string()
                    } else {
                        "glx/opengl".to_string()
                    };
                }
            }

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

    // Host fallbacks for sessions whose report predates GL-string capture,
    // and for runs that got no report at all (no compositor, a refused
    // start), which every comparison would otherwise refuse as unlabeled.
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
    // Explicit configuration choice is an honest last resort for the label;
    // "auto" resolves at runtime and is deliberately not trusted.
    if renderer_api == "unknown"
        && let Some(api) = renderer_api_from_config(&backend)
    {
        renderer_api = api;
    }

    // -- multi-monitor ------------------------------------------------------
    let mut monitor_metrics = BTreeMap::new();
    if let Ok(monitors) = session.query("get_monitors")
        && let Some(list) = monitors.as_array()
    {
        monitor_metrics.insert("monitor_count".into(), list.len() as f64);
        if resolution == "unknown"
            && let Some(extent) = screen_extent(list)
        {
            resolution = extent;
        }
    }
    if let Ok(metrics) = session.query("get_metrics")
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

    Ok(PerfBaselineV1 {
        schema_version: perf_contract::SCHEMA_VERSION,
        recorded_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        jwm_version,
        label,
        scenarios,
    })
}

/// Drive one benchmark window: make sure the WaterLily workload runs when it
/// was requested, start the benchmark, and poll metrics until the report
/// lands or the deadline passes.
///
/// Every exit leaves the session as it was found: WaterLily is switched back
/// off only when this run switched it on, whether or not the benchmark
/// started, and a benchmark that outlives the deadline is stopped instead of
/// being left running. `Err` means nothing was measured; its text is the skip
/// reason.
fn run_benchmark_window(
    session: &mut dyn RecordSession,
    options: &RecordOptions,
) -> Result<BenchmarkWindow, String> {
    // Measuring ambient pacing under a paced-workload request would make the
    // baseline lie about its conditions, so a missing workload skips instead.
    let enabled_here = options.waterlily_workload
        && start_waterlily_workload(session)
            .map_err(|reason| format!("waterlily workload unavailable: {reason}"))?;
    let window = poll_benchmark(session, options);
    if enabled_here && let Err(error) = session.command("toggle_waterlily", serde_json::json!({})) {
        eprintln!(
            "perf record: WARNING: could not switch the waterlily workload back off: {error}"
        );
    }
    window
}

fn waterlily_enabled(status: &Value) -> bool {
    status.get("enabled").and_then(Value::as_bool) == Some(true)
}

/// Make sure the WaterLily animation drives the benchmark window. `Ok` says
/// whether this call switched it on, so the caller must switch it back off;
/// `Err` is why the workload cannot run.
///
/// `toggle_waterlily` cannot answer either question: it flips rather than
/// sets, and it reports success even on a backend without WaterLily (the
/// session only logs a warning there). `get_waterlily_status` fails exactly
/// where WaterLily does not exist, so it is both the availability probe and
/// the guard that keeps an animation the user already runs from being
/// switched off for the measurement.
fn start_waterlily_workload(session: &mut dyn RecordSession) -> Result<bool, String> {
    let status = session.query("get_waterlily_status")?;
    // The effect only shows frames a connected worker publishes; without one
    // it adds no damage, and the window would measure the ambient workload.
    if status.get("worker_connected").and_then(Value::as_bool) != Some(true) {
        return Err("no WaterLily worker is connected".into());
    }
    if waterlily_enabled(&status) {
        eprintln!(
            "perf record: the waterlily animation is already running; using it as the paced workload"
        );
        return Ok(false);
    }
    eprintln!("perf record: enabling the waterlily animation as a paced workload");
    session.command("toggle_waterlily", serde_json::json!({}))?;
    match session.query("get_waterlily_status") {
        Ok(status) if waterlily_enabled(&status) => Ok(true),
        Ok(_) => Err("toggle_waterlily did not enable it".into()),
        // The session lost WaterLily between the calls (the compositor went
        // away) or stopped answering; a toggle back would be a no-op or fail
        // the same way, so none is sent.
        Err(error) => Err(format!("could not confirm it started: {error}")),
    }
}

/// What one benchmark window produced.
struct BenchmarkWindow {
    /// The benchmark report, or `None` when the deadline passed first.
    report: Option<Value>,
    /// The report `benchmark stop` ended an overdue run with. Only its
    /// `system` block is used: its frame stats cover an incomplete window.
    stopped_report: Option<Value>,
    /// `get_metrics` snapshots taken once per poll.
    samples: Vec<Value>,
    minutes: f64,
}

fn poll_benchmark(
    session: &mut dyn RecordSession,
    options: &RecordOptions,
) -> Result<BenchmarkWindow, String> {
    // A refusal (no compositor, another benchmark already running) leaves
    // nothing of ours to stop.
    session
        .command(
            "benchmark",
            serde_json::json!({
                "action": "start",
                "frames": options.frames,
                "warmup": options.warmup,
            }),
        )
        .map_err(|error| format!("benchmark could not start: {error}"))?;

    let window_start = Instant::now();
    let deadline = window_start + BENCHMARK_DEADLINE;
    let mut samples: Vec<Value> = Vec::new();
    let mut stopped_report = None;
    let report = loop {
        let in_time = session.wait_poll(deadline);
        if let Ok(sample) = session.query("get_metrics")
            && sample.is_object()
        {
            samples.push(sample);
        }
        match session.query("benchmark_report") {
            Ok(report) if report.is_object() || report.is_string() => break Some(report),
            _ => {}
        }
        if !in_time {
            // The benchmark we started is still running; leaving it would
            // keep the compositor in benchmark mode after `record` exits.
            // The report it stops with still names the system for the label.
            match session.command("benchmark", serde_json::json!({ "action": "stop" })) {
                Ok(report) if report.is_object() || report.is_string() => {
                    stopped_report = Some(report);
                }
                Ok(_) => {}
                Err(error) => {
                    eprintln!(
                        "perf record: WARNING: could not stop the overdue benchmark: {error}"
                    );
                }
            }
            break None;
        }
    };
    Ok(BenchmarkWindow {
        report,
        stopped_report,
        samples,
        minutes: window_start.elapsed().as_secs_f64() / 60.0,
    })
}

/// Write `baseline` where the options say and print its summary.
fn write_baseline(options: &RecordOptions, baseline: &PerfBaselineV1) -> Result<(), String> {
    let out = options.out.clone().unwrap_or_else(|| {
        PathBuf::from("perf/baselines").join(format!("{}.json", baseline.label.slug()))
    });
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let mut encoded = serde_json::to_string_pretty(baseline).map_err(|error| error.to_string())?;
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
            let optional = if rule.fail_closed {
                ""
            } else {
                " (n/a when only the candidate lacks it)"
            };
            println!(
                "  {}/{}: {bound}{absolute}{optional}",
                rule.scenario, rule.metric
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perf_contract::ScenarioStatus;
    use serde_json::json;

    const COMPOSITOR_SCENARIOS: [&str; 5] = [
        "steady_frame",
        "damage_redraw",
        "input_latency",
        "allocation_steady",
        "direct_scanout",
    ];

    /// What `get_metrics` answers when no compositor is running.
    fn no_compositor_metrics() -> Value {
        json!({ "window_count": 3, "monitor_count": 1, "tag_count": 9 })
    }

    /// Scripted WaterLily state behind `get_waterlily_status`.
    #[derive(Clone, Copy, Debug)]
    struct FakeWaterlily {
        enabled: bool,
        worker_connected: bool,
        /// `toggle_waterlily` is acknowledged but changes nothing.
        toggle_ignored: bool,
    }

    /// Scripted session: answers queries from a table, logs every command,
    /// and lets the benchmark deadline pass after a fixed number of polls.
    struct FakeSession {
        queries: BTreeMap<&'static str, Result<Value, String>>,
        /// `None` models a backend without WaterLily the way the real
        /// session answers it: `get_waterlily_status` fails while
        /// `toggle_waterlily` still reports success.
        waterlily: Option<FakeWaterlily>,
        refuse_benchmark_start: Option<String>,
        /// What `benchmark stop` answers with.
        stop_report: Value,
        polls_before_deadline: u32,
        commands: Vec<String>,
    }

    impl FakeSession {
        fn new(status: Value, metrics: Value) -> Self {
            let mut queries = BTreeMap::new();
            queries.insert("get_status", Ok(status));
            queries.insert("get_metrics", Ok(metrics));
            queries.insert("get_monitors", Ok(json!([{ "id": 0 }])));
            queries.insert(
                "benchmark_report",
                Err("benchmark not complete or not running".to_string()),
            );
            Self {
                queries,
                waterlily: Some(FakeWaterlily {
                    enabled: false,
                    worker_connected: true,
                    toggle_ignored: false,
                }),
                refuse_benchmark_start: None,
                stop_report: Value::Null,
                polls_before_deadline: 0,
                commands: Vec::new(),
            }
        }

        fn waterlily_enabled(&self) -> bool {
            self.waterlily.is_some_and(|waterlily| waterlily.enabled)
        }
    }

    impl RecordSession for FakeSession {
        fn query(&mut self, name: &str) -> Result<Value, String> {
            if name == "get_waterlily_status" {
                return self
                    .waterlily
                    .map(|waterlily| {
                        json!({
                            "enabled": waterlily.enabled,
                            "active": waterlily.enabled && waterlily.worker_connected,
                            "worker_connected": waterlily.worker_connected,
                        })
                    })
                    .ok_or_else(|| "compositor not active".to_string());
            }
            self.queries
                .get(name)
                .cloned()
                .unwrap_or_else(|| Err(format!("unknown query: {name}")))
        }

        fn command(&mut self, name: &str, args: Value) -> Result<Value, String> {
            let action = args.get("action").and_then(Value::as_str);
            self.commands.push(match action {
                Some(action) => format!("{name} {action}"),
                None => name.to_string(),
            });
            match (name, action) {
                ("toggle_waterlily", _) => {
                    if let Some(waterlily) = self.waterlily.as_mut()
                        && !waterlily.toggle_ignored
                    {
                        waterlily.enabled = !waterlily.enabled;
                    }
                    Ok(Value::Null)
                }
                ("benchmark", Some("start")) => self
                    .refuse_benchmark_start
                    .clone()
                    .map_or(Ok(Value::Null), Err),
                ("benchmark", Some("stop")) => Ok(self.stop_report.clone()),
                _ => Ok(Value::Null),
            }
        }

        fn sample_idle(
            &mut self,
            _pid: u32,
            _seconds: u32,
        ) -> Result<BTreeMap<String, f64>, String> {
            Err("idle sampling is not scripted".to_string())
        }

        fn wait_poll(&mut self, _deadline: Instant) -> bool {
            let in_time = self.polls_before_deadline > 0;
            self.polls_before_deadline = self.polls_before_deadline.saturating_sub(1);
            in_time
        }
    }

    fn options(waterlily_workload: bool) -> RecordOptions {
        RecordOptions {
            out: None,
            frames: 120,
            warmup: 10,
            idle_seconds: 2,
            waterlily_workload,
        }
    }

    /// A status whose backend name matches no configuration, so the label
    /// never reads the developer's real config file.
    fn status(compositor_active: bool) -> Value {
        json!({
            "backend": "fake",
            "version": "0.0.0-test",
            "pid": 1,
            "compositor_active": compositor_active,
        })
    }

    fn assert_compositor_scenarios_skipped(baseline: &PerfBaselineV1, reason_part: &str) {
        for name in COMPOSITOR_SCENARIOS {
            let result = &baseline.scenarios[name];
            assert_eq!(result.status, ScenarioStatus::Skipped, "{name}");
            let reason = result.reason.as_deref().unwrap_or_default();
            assert!(reason.contains(reason_part), "{name}: {reason}");
        }
    }

    #[test]
    fn compositor_metrics_ignore_the_no_compositor_fallback() {
        let compositor = json!({ "frame_count": 42, "renderer_api": "egl/gles3" });

        assert!(compositor_metrics(&status(false), Ok(compositor.clone())).is_err());
        assert!(compositor_metrics(&status(true), Ok(no_compositor_metrics())).is_err());
        assert!(compositor_metrics(&status(true), Err("socket closed".into())).is_err());
        assert_eq!(
            compositor_metrics(&status(true), Ok(compositor.clone())),
            Ok(compositor.clone())
        );
        // Sessions predating `compositor_active` are judged by the metrics.
        let legacy = json!({ "backend": "fake" });
        assert!(compositor_metrics(&legacy, Ok(no_compositor_metrics())).is_err());
        assert_eq!(
            compositor_metrics(&legacy, Ok(compositor.clone())),
            Ok(compositor)
        );
    }

    #[test]
    fn record_writes_skips_instead_of_failing_without_a_compositor() {
        let mut session = FakeSession::new(status(false), no_compositor_metrics());
        session.refuse_benchmark_start =
            Some("benchmark: compositor unavailable or benchmark could not be started".to_string());

        let baseline = record_baseline(&mut session, &options(false))
            .expect("a session without a compositor still yields a baseline");

        assert_compositor_scenarios_skipped(&baseline, "compositor inactive");
        assert!(session.commands.is_empty(), "{:?}", session.commands);
        assert_eq!(
            baseline.scenarios["multi_monitor"].status,
            ScenarioStatus::Recorded
        );
        assert_eq!(baseline.scenarios["idle"].status, ScenarioStatus::Skipped);
    }

    #[test]
    fn refused_benchmark_start_is_skipped_and_restores_waterlily() {
        let mut session = FakeSession::new(
            status(true),
            json!({ "frame_count": 100, "renderer_api": "egl/gles3" }),
        );
        session.refuse_benchmark_start = Some("benchmark already running".to_string());

        let baseline = record_baseline(&mut session, &options(true))
            .expect("a refused benchmark still yields a baseline");

        assert_compositor_scenarios_skipped(&baseline, "benchmark already running");
        // Enabled, refused, turned back off; the other benchmark is not ours
        // to stop.
        assert_eq!(
            session.commands,
            ["toggle_waterlily", "benchmark start", "toggle_waterlily"]
        );
        assert!(!session.waterlily_enabled());
        assert_eq!(baseline.label.renderer_api, "egl/gles3");
    }

    #[test]
    fn unavailable_waterlily_workload_skips_without_toggling_or_benchmarking() {
        // The real session acknowledges `toggle_waterlily` even without
        // WaterLily; only the status query tells, and it must decide.
        let mut session = FakeSession::new(status(true), json!({ "frame_count": 100 }));
        session.waterlily = None;

        let baseline = record_baseline(&mut session, &options(true))
            .expect("a missing workload still yields a baseline");

        assert_compositor_scenarios_skipped(
            &baseline,
            "waterlily workload unavailable: compositor not active",
        );
        assert!(session.commands.is_empty(), "{:?}", session.commands);
    }

    #[test]
    fn waterlily_workload_without_a_worker_skips_without_toggling() {
        let mut session = FakeSession::new(status(true), json!({ "frame_count": 100 }));
        session.waterlily = Some(FakeWaterlily {
            enabled: false,
            worker_connected: false,
            toggle_ignored: false,
        });

        let baseline = record_baseline(&mut session, &options(true))
            .expect("a missing worker still yields a baseline");

        assert_compositor_scenarios_skipped(&baseline, "no WaterLily worker is connected");
        assert!(session.commands.is_empty(), "{:?}", session.commands);
        assert!(!session.waterlily_enabled());
    }

    #[test]
    fn waterlily_toggle_that_does_not_take_effect_skips_the_window() {
        let mut session = FakeSession::new(status(true), json!({ "frame_count": 100 }));
        session.waterlily = Some(FakeWaterlily {
            enabled: false,
            worker_connected: true,
            toggle_ignored: true,
        });

        let baseline = record_baseline(&mut session, &options(true))
            .expect("an unconfirmed workload still yields a baseline");

        assert_compositor_scenarios_skipped(&baseline, "did not enable it");
        // Nothing changed, so there is nothing to switch back.
        assert_eq!(session.commands, ["toggle_waterlily"]);
    }

    #[test]
    fn running_waterlily_workload_is_used_and_left_running() {
        let mut session = FakeSession::new(status(true), json!({ "frame_count": 100 }));
        session.waterlily = Some(FakeWaterlily {
            enabled: true,
            worker_connected: true,
            toggle_ignored: false,
        });
        session.queries.insert(
            "benchmark_report",
            Ok(json!({ "frame_time": { "avg_ms": 6.9, "count": 120 } })),
        );

        let baseline = record_baseline(&mut session, &options(true))
            .expect("a completed benchmark yields a baseline");

        // A blind toggle pair would have measured with WaterLily off and
        // then switched the user's animation back on.
        assert_eq!(session.commands, ["benchmark start"]);
        assert!(session.waterlily_enabled());
        assert_eq!(
            baseline.scenarios["steady_frame"].status,
            ScenarioStatus::Recorded
        );
    }

    /// A benchmark report whose `system` block identifies the test GPU.
    fn report_with_system(frame_time: Value) -> Value {
        json!({
            "system": {
                "gpu": "Test GPU/PCIe",
                "driver": "4.6.0 Test 555.0",
                "resolution": "2560x1440",
            },
            "frame_time": frame_time,
        })
    }

    #[test]
    fn overdue_benchmark_is_stopped_and_waterlily_restored() {
        let mut session = FakeSession::new(status(true), json!({ "frame_count": 100 }));
        session.polls_before_deadline = 2;
        session.stop_report = report_with_system(json!({ "avg_ms": 40.0, "count": 3 }));

        let baseline = record_baseline(&mut session, &options(true))
            .expect("an overdue benchmark still yields a baseline");

        assert_eq!(
            session.commands,
            [
                "toggle_waterlily",
                "benchmark start",
                "benchmark stop",
                "toggle_waterlily"
            ]
        );
        assert!(!session.waterlily_enabled());
        let steady = &baseline.scenarios["steady_frame"];
        assert_eq!(steady.status, ScenarioStatus::Skipped);
        assert!(
            steady
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("did not complete")),
            "{steady:?}"
        );
        // The stopped run still names the system, not its partial numbers.
        assert_eq!(baseline.label.gpu, "Test GPU/PCIe");
        assert_eq!(baseline.label.driver, "4.6.0 Test 555.0");
        assert_eq!(baseline.label.resolution, "2560x1440");
        assert!(steady.metrics.is_empty());
    }

    #[test]
    fn overdue_candidate_fails_the_gate_against_a_completed_baseline() {
        let record = |session: &mut FakeSession| {
            let mut baseline = record_baseline(session, &options(false)).unwrap();
            // CPU, kernel and the config fingerprint come from this host and
            // its config file, identical for both runs; pin them so the
            // comparison does not depend on the machine running the test.
            baseline.label.cpu = "Test CPU".into();
            baseline.label.kernel = "6.17.0".into();
            baseline.label.config_fingerprint = "abcd1234".into();
            baseline
        };
        let metrics = json!({ "frame_count": 100, "renderer_api": "glx/opengl" });

        let mut session = FakeSession::new(status(true), metrics.clone());
        session.queries.insert(
            "benchmark_report",
            Ok(report_with_system(json!({
                "avg_ms": 6.9,
                "p50_ms": 6.8,
                "p95_ms": 8.1,
                "p99_ms": 9.0,
                "fps_avg": 60.0,
                "count": 120,
            }))),
        );
        let baseline = record(&mut session);

        let mut session = FakeSession::new(status(true), metrics);
        session.polls_before_deadline = 1;
        session.stop_report = report_with_system(json!({ "avg_ms": 40.0, "count": 3 }));
        let candidate = record(&mut session);

        assert!(candidate.label.differing_fields(&baseline.label).is_empty());
        let report = perf_contract::compare(&baseline, &candidate, &default_budgets())
            .expect("an overdue candidate keeps the baseline's label");
        assert!(!report.passed);
        let lost = report
            .verdicts
            .iter()
            .find(|verdict| verdict.metric == "frame_time_p95_ms")
            .unwrap();
        assert_eq!(lost.outcome, VerdictOutcome::Violation);
        assert!(lost.detail.contains("did not complete"), "{lost:?}");
    }

    #[test]
    fn reportless_runs_take_the_resolution_from_the_monitor_extent() {
        let mut session = FakeSession::new(status(false), no_compositor_metrics());
        session.queries.insert(
            "get_monitors",
            Ok(json!([
                { "num": 0, "x": 0, "y": 0, "w": 1920, "h": 1080 },
                { "num": 1, "x": 1920, "y": 0, "w": 2560, "h": 1440 },
            ])),
        );

        let baseline = record_baseline(&mut session, &options(false))
            .expect("a session without a compositor still yields a baseline");

        assert_eq!(baseline.label.resolution, "4480x1440");
    }

    #[test]
    fn screen_extent_spans_every_monitor_and_needs_their_geometry() {
        let monitors = [
            json!({ "x": 0, "y": 360, "w": 1920, "h": 1080 }),
            json!({ "x": 1920, "y": 0, "w": 2560, "h": 1440 }),
        ];
        assert_eq!(screen_extent(&monitors).as_deref(), Some("4480x1440"));
        assert_eq!(screen_extent(&[]), None);
        assert_eq!(screen_extent(&[json!({ "id": 0 })]), None);
    }

    #[test]
    fn completed_benchmark_is_recorded_without_a_stop() {
        let mut session = FakeSession::new(status(true), json!({ "frame_count": 100 }));
        session.queries.insert(
            "benchmark_report",
            Ok(json!({
                "frame_time": { "avg_ms": 6.9, "p95_ms": 8.1, "count": 120 },
            })),
        );

        let baseline = record_baseline(&mut session, &options(true))
            .expect("a completed benchmark yields a baseline");

        assert_eq!(
            session.commands,
            ["toggle_waterlily", "benchmark start", "toggle_waterlily"]
        );
        assert!(!session.waterlily_enabled());
        let steady = &baseline.scenarios["steady_frame"];
        assert_eq!(steady.status, ScenarioStatus::Recorded);
        assert_eq!(steady.metrics.get("frame_time_avg_ms"), Some(&6.9));
    }
}
