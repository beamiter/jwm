//! Deterministic nested-backend smoke matrix (roadmap Phase 1).
//!
//! Boots the nested Wayland development backends (`wayland-winit`,
//! `wayland-x11`) inside a private `XDG_RUNTIME_DIR`, drives them through
//! startup, IPC health, config reload, basic window lifecycle, screenshot
//! capture, and clean shutdown, and emits one versioned, machine-readable
//! report. Every wait is bounded by an explicit per-step timeout; a failed
//! step preserves the run's log directory and points at it in the report.
//!
//! The matrix definition (steps, ordering, timeouts, required flags) and the
//! report schema are pure and unit-tested; only the executor touches
//! processes and sockets.

use serde::Serialize;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const NESTED_SMOKE_SCHEMA_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Matrix definition (pure)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NestedBackendKind {
    Winit,
    X11,
    X11rb,
    Xcb,
}

/// Which display protocol the nested jwm session serves to its clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NestedFamily {
    Wayland,
    X11,
}

impl NestedBackendKind {
    #[must_use]
    pub const fn jwm_backend_value(self) -> &'static str {
        match self {
            Self::Winit => "wayland-winit",
            Self::X11 => "wayland-x11",
            Self::X11rb => "x11rb",
            Self::Xcb => "xcb",
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Winit => "winit",
            Self::X11 => "x11",
            Self::X11rb => "x11rb",
            Self::Xcb => "xcb",
        }
    }

    #[must_use]
    pub const fn family(self) -> NestedFamily {
        match self {
            Self::Winit | Self::X11 => NestedFamily::Wayland,
            Self::X11rb | Self::Xcb => NestedFamily::X11,
        }
    }
}

/// Which nested backends can run on this host session.
///
/// `wayland-winit` follows winit and accepts either host session type;
/// `wayland-x11` requires an X11 host; the `x11rb`/`xcb` transports run
/// inside a Xephyr server, which itself needs an X11-capable host display.
#[must_use]
pub fn eligible_backends(
    display: Option<&str>,
    wayland_display: Option<&str>,
    has_xephyr: bool,
) -> Vec<NestedBackendKind> {
    let has_x11 = display.is_some_and(|value| !value.is_empty());
    let has_wayland = wayland_display.is_some_and(|value| !value.is_empty());
    let mut backends = Vec::new();
    if has_x11 || has_wayland {
        backends.push(NestedBackendKind::Winit);
    }
    if has_x11 {
        backends.push(NestedBackendKind::X11);
    }
    if has_x11 && has_xephyr {
        backends.push(NestedBackendKind::X11rb);
        backends.push(NestedBackendKind::Xcb);
    }
    backends
}

/// Whether the Xephyr nested X server needed by the X11 transports exists.
#[must_use]
pub fn xephyr_available() -> bool {
    command_in_path("Xephyr")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepSpec {
    pub name: &'static str,
    /// A failure in a required step fails the run and skips later steps
    /// (shutdown always still runs).
    pub required: bool,
    pub timeout: Duration,
}

/// The fixed, ordered smoke matrix. Timeouts are generous enough for debug
/// builds; every executor wait derives its deadline from this table.
pub const MATRIX: &[StepSpec] = &[
    StepSpec {
        name: "startup",
        required: true,
        timeout: Duration::from_secs(20),
    },
    StepSpec {
        name: "ipc_health",
        required: true,
        timeout: Duration::from_secs(5),
    },
    StepSpec {
        name: "config_reload",
        required: true,
        timeout: Duration::from_secs(5),
    },
    StepSpec {
        name: "window_lifecycle",
        required: false,
        timeout: Duration::from_secs(25),
    },
    StepSpec {
        name: "screenshot_capture",
        required: false,
        timeout: Duration::from_secs(10),
    },
    StepSpec {
        name: "policy_scenario",
        required: false,
        timeout: Duration::from_secs(30),
    },
    StepSpec {
        name: "clean_shutdown",
        required: true,
        timeout: Duration::from_secs(10),
    },
];

#[cfg(test)]
#[must_use]
pub fn step_spec(name: &str) -> Option<&'static StepSpec> {
    MATRIX.iter().find(|spec| spec.name == name)
}

/// Resolve the host `WAYLAND_DISPLAY` into an absolute socket path so the
/// nested child can still reach the host compositor after its
/// `XDG_RUNTIME_DIR` is redirected to the private smoke directory.
#[must_use]
pub fn absolute_wayland_display(host_runtime_dir: &str, wayland_display: &str) -> PathBuf {
    let display = Path::new(wayland_display);
    if display.is_absolute() {
        display.to_path_buf()
    } else {
        Path::new(host_runtime_dir).join(display)
    }
}

/// Environment overrides for the nested jwm child process.
///
/// Returns `(set, remove)` pairs; pure so the isolation policy is testable.
/// `nested_x11_display` is the Xephyr display the X11 transports run inside.
#[must_use]
pub fn child_env_overrides(
    kind: NestedBackendKind,
    private_runtime_dir: &Path,
    host_runtime_dir: Option<&str>,
    host_wayland_display: Option<&str>,
    nested_x11_display: Option<&str>,
) -> (Vec<(String, String)>, Vec<&'static str>) {
    let mut set = vec![
        (
            "XDG_RUNTIME_DIR".to_string(),
            private_runtime_dir.display().to_string(),
        ),
        (
            "JWM_BACKEND".to_string(),
            kind.jwm_backend_value().to_string(),
        ),
    ];
    let mut remove = Vec::new();
    match kind {
        NestedBackendKind::Winit => {
            // winit may connect to the host through Wayland; the relative
            // socket name would resolve against the private runtime dir.
            if let (Some(runtime), Some(display)) = (host_runtime_dir, host_wayland_display)
                && !display.is_empty()
            {
                set.push((
                    "WAYLAND_DISPLAY".to_string(),
                    absolute_wayland_display(runtime, display)
                        .display()
                        .to_string(),
                ));
            }
        }
        NestedBackendKind::X11 => {
            // The smithay X11 backend uses DISPLAY only; a live host
            // WAYLAND_DISPLAY must not leak into the nested session.
            remove.push("WAYLAND_DISPLAY");
        }
        NestedBackendKind::X11rb | NestedBackendKind::Xcb => {
            // The X11 transports manage the private Xephyr display; the
            // host session must not leak in through either protocol.
            if let Some(display) = nested_x11_display {
                set.push(("DISPLAY".to_string(), display.to_string()));
            }
            remove.push("WAYLAND_DISPLAY");
        }
    }
    (set, remove)
}

/// Find the nested compositor's own Wayland socket inside the private
/// runtime directory (the only `wayland-*` socket that can exist there).
#[must_use]
pub fn find_nested_wayland_socket(runtime_dir: &Path) -> Option<String> {
    let entries = fs::read_dir(runtime_dir).ok()?;
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_type()
                .map(|kind| kind.is_socket_like())
                .unwrap_or(false)
        })
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with("wayland-") && !name.ends_with(".lock"))
        .collect();
    names.sort();
    names.into_iter().next()
}

trait SocketLike {
    fn is_socket_like(&self) -> bool;
}

impl SocketLike for fs::FileType {
    fn is_socket_like(&self) -> bool {
        use std::os::unix::fs::FileTypeExt;
        self.is_socket()
    }
}

/// Default client candidates for the window-lifecycle step, tried in order.
/// All spawn with Wayland-first environment hints, so toolkit apps join the
/// nested session instead of the host.
pub const CLIENT_CANDIDATES: &[&str] = &[
    "foot",
    "weston-terminal",
    "alacritty",
    "kitty",
    "adwaita-1-demo",
    "gtk4-demo",
];

/// Client candidates for the nested X11 (Xephyr) sessions.
pub const X11_CLIENT_CANDIDATES: &[&str] = &["xterm", "xclock", "xeyes"];

#[must_use]
pub const fn client_candidates(family: NestedFamily) -> &'static [&'static str] {
    match family {
        NestedFamily::Wayland => CLIENT_CANDIDATES,
        NestedFamily::X11 => X11_CLIENT_CANDIDATES,
    }
}

#[must_use]
pub fn png_signature_valid(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A])
}

/// Sanity-check an `xwd` dump: the header is at least 100 bytes and encodes
/// `header_size` then `file_version == 7`, both big-endian.
#[must_use]
pub fn xwd_signature_valid(bytes: &[u8]) -> bool {
    if bytes.len() < 100 {
        return false;
    }
    let header_size = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let version = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    header_size >= 100 && version == 7
}

// ---------------------------------------------------------------------------
// Differential policy scenario (pure normalization + comparison)
// ---------------------------------------------------------------------------

/// The fixed IPC command sequence driven between scenario snapshots. Snapshot
/// 0 is taken right after the scenario client maps; each command produces one
/// further snapshot, so `SCENARIO_COMMANDS.len() + 1` snapshots total.
pub const SCENARIO_COMMANDS: &[(&str, &str)] = &[
    ("view", r#"{"tag":2}"#),
    ("view", r#"{"tag":1}"#),
    ("togglefloating", "null"),
];

/// Reduce `get_windows` + `get_workspaces` payloads to the transport-neutral
/// observable state the differential comparison is defined over.
///
/// Dropped on purpose: window ids (transport-relative) and titles (set
/// asynchronously by clients, so capture timing would make them flaky).
/// Geometry stays: the tiling layout fully determines it, so a divergence is
/// a real policy or pixel-math difference between the transports.
#[must_use]
pub fn normalize_observable_state(
    windows: &serde_json::Value,
    workspaces: &serde_json::Value,
) -> serde_json::Value {
    let mut normalized_windows: Vec<serde_json::Value> = windows
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .map(|window| {
                    serde_json::json!({
                        "class": window["class"],
                        "instance": window["instance"],
                        "tags": window["tags"],
                        "floating": window["is_floating"],
                        "fullscreen": window["is_fullscreen"],
                        "focused": window["is_focused"],
                        "monitor": window["monitor"],
                        "x": window["x"],
                        "y": window["y"],
                        "w": window["w"],
                        "h": window["h"],
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    normalized_windows.sort_by_key(|window| {
        (
            window["class"].as_str().unwrap_or_default().to_string(),
            window["instance"].as_str().unwrap_or_default().to_string(),
            window["x"].as_i64().unwrap_or_default(),
            window["y"].as_i64().unwrap_or_default(),
        )
    });

    let mut normalized_workspaces: Vec<serde_json::Value> = workspaces
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .map(|workspace| {
                    serde_json::json!({
                        "monitor": workspace["monitor"],
                        "tag_index": workspace["tag_index"],
                        "layout": workspace["layout"],
                        "n_master": workspace["n_master"],
                        "num_clients": workspace["num_clients"],
                        "focused": workspace["focused"],
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    normalized_workspaces.sort_by_key(|workspace| {
        (
            workspace["monitor"].as_i64().unwrap_or_default(),
            workspace["tag_index"].as_i64().unwrap_or_default(),
        )
    });

    serde_json::json!({
        "windows": normalized_windows,
        "workspaces": normalized_workspaces,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DifferentialStatus {
    Pass,
    Fail,
    Skip,
}

/// Outcome of comparing the two X11 transports' scenario snapshots.
#[derive(Debug, Clone, Serialize)]
pub struct DifferentialReport {
    pub status: DifferentialStatus,
    /// Transports whose snapshots were compared (labels).
    pub compared: Vec<String>,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

/// Compare the x11rb and xcb scenario snapshots. Only produced when at least
/// one X11 transport was part of the matrix; skips (with the reason) when the
/// comparison has fewer than two complete snapshot sets.
#[must_use]
pub fn differential_outcome(runs: &[RunReport]) -> Option<DifferentialReport> {
    let x11_runs: Vec<&RunReport> = runs
        .iter()
        .filter(|run| run.backend.family() == NestedFamily::X11)
        .collect();
    if x11_runs.is_empty() {
        return None;
    }
    let complete: Vec<&RunReport> = x11_runs
        .iter()
        .copied()
        .filter(|run| run.scenario.is_some())
        .collect();
    if complete.len() < 2 {
        return Some(DifferentialReport {
            status: DifferentialStatus::Skip,
            compared: Vec::new(),
            detail: format!(
                "need scenario snapshots from both X11 transports, have {}",
                complete.len()
            ),
            action: None,
        });
    }
    let reference = complete[0];
    let reference_snapshots = reference.scenario.as_ref().expect("filtered above");
    for candidate in &complete[1..] {
        let candidate_snapshots = candidate.scenario.as_ref().expect("filtered above");
        if reference_snapshots.len() != candidate_snapshots.len() {
            return Some(divergence_report(
                reference,
                candidate,
                format!(
                    "snapshot counts differ: {} has {}, {} has {}",
                    reference.backend.label(),
                    reference_snapshots.len(),
                    candidate.backend.label(),
                    candidate_snapshots.len()
                ),
            ));
        }
        for (index, (left, right)) in reference_snapshots
            .iter()
            .zip(candidate_snapshots)
            .enumerate()
        {
            if left != right {
                let section = if left["windows"] != right["windows"] {
                    "windows"
                } else {
                    "workspaces"
                };
                return Some(divergence_report(
                    reference,
                    candidate,
                    format!(
                        "snapshot {index} diverges in `{section}` between {} and {}",
                        reference.backend.label(),
                        candidate.backend.label()
                    ),
                ));
            }
        }
    }
    Some(DifferentialReport {
        status: DifferentialStatus::Pass,
        compared: complete
            .iter()
            .map(|run| run.backend.label().to_string())
            .collect(),
        detail: format!(
            "{} scenario snapshots identical across {} transports",
            reference_snapshots.len(),
            complete.len()
        ),
        action: None,
    })
}

fn divergence_report(
    reference: &RunReport,
    candidate: &RunReport,
    detail: String,
) -> DifferentialReport {
    DifferentialReport {
        status: DifferentialStatus::Fail,
        compared: vec![
            reference.backend.label().to_string(),
            candidate.backend.label().to_string(),
        ],
        detail,
        action: Some(
            "re-run with `--json --save` and diff the two runs' `scenario` arrays".to_string(),
        ),
    }
}

// ---------------------------------------------------------------------------
// Report schema (pure)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pass,
    Fail,
    Skip,
    NotRun,
}

#[derive(Debug, Clone, Serialize)]
pub struct StepReport {
    pub name: &'static str,
    pub status: StepStatus,
    pub required: bool,
    pub duration_ms: u64,
    /// Human-readable observation for any status.
    pub detail: String,
    /// One actionable instruction; present exactly when `status` is `fail`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunReport {
    pub backend: NestedBackendKind,
    pub result: RunResult,
    /// Preserved on failure so the report points at exactly one log bundle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifacts_dir: Option<String>,
    pub steps: Vec<StepReport>,
    /// Normalized observable-state snapshots captured by `policy_scenario`;
    /// present only when the scenario ran to completion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scenario: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunResult {
    Pass,
    Fail,
}

#[derive(Debug, Clone, Serialize)]
pub struct HostInfo {
    pub session_type: Option<String>,
    pub display: Option<String>,
    pub wayland_display: Option<String>,
    pub jwm_binary: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MatrixReport {
    pub schema_version: u32,
    pub generated_unix_ms: u128,
    pub host: HostInfo,
    pub runs: Vec<RunReport>,
    /// x11rb-versus-xcb observable-state comparison; present whenever at
    /// least one X11 transport was part of the matrix.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub differential: Option<DifferentialReport>,
}

/// A run fails exactly when a step failed; skips (missing optional tooling,
/// unsupported capability) keep the matrix deterministic without hiding
/// executed failures.
#[must_use]
pub fn run_result(steps: &[StepReport]) -> RunResult {
    if steps
        .iter()
        .any(|step| matches!(step.status, StepStatus::Fail))
    {
        RunResult::Fail
    } else {
        RunResult::Pass
    }
}

#[must_use]
pub fn matrix_exit_code(report: &MatrixReport) -> i32 {
    if report.runs.is_empty() {
        return 2;
    }
    let differential_failed = report
        .differential
        .as_ref()
        .is_some_and(|differential| matches!(differential.status, DifferentialStatus::Fail));
    if differential_failed
        || report
            .runs
            .iter()
            .any(|run| matches!(run.result, RunResult::Fail))
    {
        1
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

pub struct NestedSmokeOptions {
    pub backends: Vec<NestedBackendKind>,
    pub jwm_binary: PathBuf,
    /// Explicit lifecycle client command; probes `CLIENT_CANDIDATES` if None.
    pub client: Option<Vec<String>>,
    /// Keep the private runtime/log directory even when the run passes.
    pub keep_artifacts: bool,
}

const POLL_INTERVAL: Duration = Duration::from_millis(150);
const IPC_IO_TIMEOUT: Duration = Duration::from_secs(5);
/// Preserved log lines quoted into a failure detail; keeps the report bounded.
const LOG_TAIL_LINES: usize = 5;

pub fn run_nested_smoke(options: &NestedSmokeOptions) -> MatrixReport {
    let host = HostInfo {
        session_type: std::env::var("XDG_SESSION_TYPE").ok(),
        display: std::env::var("DISPLAY").ok(),
        wayland_display: std::env::var("WAYLAND_DISPLAY").ok(),
        jwm_binary: options.jwm_binary.display().to_string(),
    };
    let runs: Vec<RunReport> = options
        .backends
        .iter()
        .map(|kind| run_backend(*kind, options))
        .collect();
    let differential = differential_outcome(&runs);
    MatrixReport {
        schema_version: NESTED_SMOKE_SCHEMA_VERSION,
        generated_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or_default(),
        host,
        runs,
        differential,
    }
}

struct RunContext {
    kind: NestedBackendKind,
    runtime_dir: PathBuf,
    child: Option<Child>,
    log_path: PathBuf,
    /// The private Xephyr server hosting an X11-transport run.
    xephyr: Option<Child>,
    /// `DISPLAY` value of that Xephyr server (e.g. `:91`).
    nested_x11_display: Option<String>,
}

impl RunContext {
    fn socket_path(&self) -> PathBuf {
        self.runtime_dir.join("jwm-ipc.sock")
    }

    fn log_tail(&self) -> String {
        log_tail(&self.log_path, LOG_TAIL_LINES)
    }
}

fn run_backend(kind: NestedBackendKind, options: &NestedSmokeOptions) -> RunReport {
    let mut steps: Vec<StepReport> = Vec::with_capacity(MATRIX.len());
    let mut context = match prepare_run_context(kind, options) {
        Ok(context) => context,
        Err(error) => {
            steps.push(StepReport {
                name: "startup",
                status: StepStatus::Fail,
                required: true,
                duration_ms: 0,
                detail: error.to_string(),
                action: Some(
                    "verify the jwm binary path and that /tmp is writable, then re-run \
                     `jwm-tool nested-smoke`"
                        .to_string(),
                ),
            });
            fill_not_run(&mut steps);
            return finish_run(kind, steps, None, None, options.keep_artifacts);
        }
    };

    let mut aborted = false;
    let mut scenario: Option<Vec<serde_json::Value>> = None;
    for spec in MATRIX {
        if spec.name == "clean_shutdown" {
            // Shutdown always runs so a crashed step never leaks the child.
            steps.push(run_shutdown_step(spec, &mut context));
            continue;
        }
        if aborted {
            steps.push(StepReport {
                name: spec.name,
                status: StepStatus::NotRun,
                required: spec.required,
                duration_ms: 0,
                detail: "skipped after an earlier required step failed".to_string(),
                action: None,
            });
            continue;
        }
        let report = match spec.name {
            "startup" => run_startup_step(spec, &mut context),
            "ipc_health" => run_ipc_health_step(spec, &context),
            "config_reload" => run_config_reload_step(spec, &context),
            "window_lifecycle" => run_window_lifecycle_step(spec, &context, options),
            "screenshot_capture" => run_screenshot_step(spec, &context),
            "policy_scenario" => {
                let (report, snapshots) = run_policy_scenario_step(spec, &context, options);
                scenario = snapshots;
                report
            }
            other => StepReport {
                name: spec.name,
                status: StepStatus::Fail,
                required: spec.required,
                duration_ms: 0,
                detail: format!("unknown step '{other}' in matrix"),
                action: Some("update the executor to cover every MATRIX entry".to_string()),
            },
        };
        let failed_required = spec.required && matches!(report.status, StepStatus::Fail);
        steps.push(report);
        if failed_required {
            aborted = true;
        }
    }

    let artifacts = Some(context.runtime_dir.clone());
    // Belt and braces: never leave a child behind, whatever the steps did.
    kill_child(&mut context);
    finish_run(kind, steps, scenario, artifacts, options.keep_artifacts)
}

fn finish_run(
    kind: NestedBackendKind,
    steps: Vec<StepReport>,
    scenario: Option<Vec<serde_json::Value>>,
    artifacts: Option<PathBuf>,
    keep_artifacts: bool,
) -> RunReport {
    let result = run_result(&steps);
    let artifacts_dir = match (&artifacts, result, keep_artifacts) {
        (Some(dir), RunResult::Fail, _) | (Some(dir), _, true) => Some(dir.display().to_string()),
        (Some(dir), RunResult::Pass, false) => {
            let _ = fs::remove_dir_all(dir);
            None
        }
        (None, _, _) => None,
    };
    RunReport {
        backend: kind,
        result,
        artifacts_dir,
        steps,
        scenario,
    }
}

fn fill_not_run(steps: &mut Vec<StepReport>) {
    for spec in MATRIX.iter().skip(steps.len()) {
        steps.push(StepReport {
            name: spec.name,
            status: StepStatus::NotRun,
            required: spec.required,
            duration_ms: 0,
            detail: "not run".to_string(),
            action: None,
        });
    }
}

fn prepare_run_context(
    kind: NestedBackendKind,
    options: &NestedSmokeOptions,
) -> io::Result<RunContext> {
    let runtime_dir = create_private_runtime_dir(kind)?;
    let (xephyr, nested_x11_display) = if kind.family() == NestedFamily::X11 {
        match spawn_xephyr(&runtime_dir) {
            Ok((child, display)) => (Some(child), Some(display)),
            Err(error) => {
                let _ = fs::remove_dir_all(&runtime_dir);
                return Err(error);
            }
        }
    } else {
        (None, None)
    };
    let log_path = runtime_dir.join("jwm.log");
    let log_file = fs::File::create(&log_path)?;
    let (set, remove) = child_env_overrides(
        kind,
        &runtime_dir,
        std::env::var("XDG_RUNTIME_DIR").ok().as_deref(),
        std::env::var("WAYLAND_DISPLAY").ok().as_deref(),
        nested_x11_display.as_deref(),
    );
    let mut command = Command::new(&options.jwm_binary);
    command
        .stdin(Stdio::null())
        .stdout(log_file.try_clone()?)
        .stderr(log_file)
        .env("RUST_LOG", "info");
    for (key, value) in set {
        command.env(key, value);
    }
    for key in remove {
        command.env_remove(key);
    }
    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            if let Some(mut xephyr) = xephyr {
                let _ = xephyr.kill();
                let _ = xephyr.wait();
            }
            let _ = fs::remove_dir_all(&runtime_dir);
            return Err(io::Error::new(
                error.kind(),
                format!("could not launch {}: {error}", options.jwm_binary.display()),
            ));
        }
    };
    Ok(RunContext {
        kind,
        runtime_dir,
        child: Some(child),
        log_path,
        xephyr,
        nested_x11_display,
    })
}

const XEPHYR_SCREEN: &str = "1280x800";
const XEPHYR_START_TIMEOUT: Duration = Duration::from_secs(5);

/// Boot a private Xephyr server on a free display and wait (bounded) for its
/// socket. The fixed screen size keeps X11-transport geometry deterministic.
fn spawn_xephyr(runtime_dir: &Path) -> io::Result<(Child, String)> {
    let Some(number) = find_free_x11_display() else {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "no free X display number in :80..=:99 for Xephyr",
        ));
    };
    let display = format!(":{number}");
    let log = fs::File::create(runtime_dir.join("xephyr.log"))?;
    let mut child = Command::new("Xephyr")
        .arg(&display)
        .args(["-screen", XEPHYR_SCREEN, "-nolisten", "tcp"])
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()
        .map_err(|error| {
            io::Error::new(error.kind(), format!("could not launch Xephyr: {error}"))
        })?;
    let socket = PathBuf::from(format!("/tmp/.X11-unix/X{number}"));
    let deadline = Instant::now() + XEPHYR_START_TIMEOUT;
    while Instant::now() < deadline {
        if socket.exists() {
            return Ok((child, display));
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(io::Error::other(format!(
                "Xephyr exited during startup ({status}); see xephyr.log"
            )));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    let _ = child.kill();
    let _ = child.wait();
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("Xephyr did not create {display} within {XEPHYR_START_TIMEOUT:?}"),
    ))
}

/// First display number in `:80..=:99` with neither a lock file nor a socket.
fn find_free_x11_display() -> Option<u32> {
    (80..=99).find(|number| {
        !Path::new(&format!("/tmp/.X{number}-lock")).exists()
            && !Path::new(&format!("/tmp/.X11-unix/X{number}")).exists()
    })
}

fn create_private_runtime_dir(kind: NestedBackendKind) -> io::Result<PathBuf> {
    let base = std::env::temp_dir();
    for attempt in 0..64u32 {
        let candidate = base.join(format!(
            "jwm-nested-smoke-{}-{}-{attempt}",
            kind.label(),
            std::process::id()
        ));
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(&candidate) {
            Ok(()) => {
                // A pre-existing path was skipped; make sure ours is private.
                fs::set_permissions(&candidate, fs::Permissions::from_mode(0o700))?;
                return Ok(candidate);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a fresh private runtime directory",
    ))
}

// --- IPC plumbing ----------------------------------------------------------

fn ipc_roundtrip(socket: &Path, request: &serde_json::Value) -> Result<serde_json::Value, String> {
    let mut stream =
        UnixStream::connect(socket).map_err(|error| format!("connect {socket:?}: {error}"))?;
    stream
        .set_read_timeout(Some(IPC_IO_TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(IPC_IO_TIMEOUT)))
        .map_err(|error| format!("socket timeout setup: {error}"))?;
    let mut line = request.to_string();
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .map_err(|error| format!("send request: {error}"))?;
    let mut response = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                response.push(byte[0]);
                if response.len() > 1 << 20 {
                    return Err("response exceeded 1 MiB".to_string());
                }
            }
            Err(error) => return Err(format!("read response: {error}")),
        }
    }
    serde_json::from_slice(&response).map_err(|error| format!("parse response: {error}"))
}

fn ipc_command(socket: &Path, name: &str) -> Result<serde_json::Value, String> {
    let response = ipc_roundtrip(socket, &serde_json::json!({ "command": name }))?;
    expect_success(name, response)
}

fn ipc_query(socket: &Path, name: &str) -> Result<serde_json::Value, String> {
    let response = ipc_roundtrip(socket, &serde_json::json!({ "query": name }))?;
    expect_success(name, response)
}

fn expect_success(name: &str, response: serde_json::Value) -> Result<serde_json::Value, String> {
    if response["success"].as_bool() == Some(true) {
        Ok(response)
    } else {
        Err(format!("{name} rejected: {response}"))
    }
}

fn window_count(socket: &Path) -> Result<usize, String> {
    let response = ipc_query(socket, "get_windows")?;
    response["data"]
        .as_array()
        .map(Vec::len)
        .ok_or_else(|| format!("get_windows returned a non-array payload: {response}"))
}

// --- Steps -----------------------------------------------------------------

fn run_startup_step(spec: &StepSpec, context: &mut RunContext) -> StepReport {
    let started = Instant::now();
    let deadline = started + spec.timeout;
    let socket = context.socket_path();
    loop {
        if socket.exists() && ipc_query(&socket, "get_version").is_ok() {
            return StepReport {
                name: spec.name,
                status: StepStatus::Pass,
                required: spec.required,
                duration_ms: elapsed_ms(started),
                detail: format!("IPC socket answered after {} ms", elapsed_ms(started)),
                action: None,
            };
        }
        if let Some(child) = context.child.as_mut()
            && let Ok(Some(status)) = child.try_wait()
        {
            context.child = None;
            return StepReport {
                name: spec.name,
                status: StepStatus::Fail,
                required: spec.required,
                duration_ms: elapsed_ms(started),
                detail: format!(
                    "jwm exited during startup ({status}); {}",
                    context.log_tail()
                ),
                action: Some(format!(
                    "inspect the preserved log at {}",
                    context.log_path.display()
                )),
            };
        }
        if Instant::now() >= deadline {
            return StepReport {
                name: spec.name,
                status: StepStatus::Fail,
                required: spec.required,
                duration_ms: elapsed_ms(started),
                detail: format!(
                    "IPC socket did not answer within {:?}; {}",
                    spec.timeout,
                    context.log_tail()
                ),
                action: Some(format!(
                    "inspect the preserved log at {}",
                    context.log_path.display()
                )),
            };
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn run_ipc_health_step(spec: &StepSpec, context: &RunContext) -> StepReport {
    let started = Instant::now();
    let socket = context.socket_path();
    match ipc_query(&socket, "get_status") {
        Ok(response) => {
            let health = response["data"]["health"]["status"]
                .as_str()
                .unwrap_or("missing")
                .to_string();
            let failed = health == "missing" || health == "error";
            StepReport {
                name: spec.name,
                status: if failed {
                    StepStatus::Fail
                } else {
                    StepStatus::Pass
                },
                required: spec.required,
                duration_ms: elapsed_ms(started),
                detail: format!("health status: {health}"),
                action: failed.then(|| {
                    format!(
                        "run `jwm-tool health --json` against {} for the failing reasons",
                        context.runtime_dir.display()
                    )
                }),
            }
        }
        Err(error) => step_fail(spec, started, error, context),
    }
}

fn run_config_reload_step(spec: &StepSpec, context: &RunContext) -> StepReport {
    let started = Instant::now();
    let socket = context.socket_path();
    match ipc_command(&socket, "reload_config") {
        Ok(_) => StepReport {
            name: spec.name,
            status: StepStatus::Pass,
            required: spec.required,
            duration_ms: elapsed_ms(started),
            detail: "reload_config acknowledged".to_string(),
            action: None,
        },
        Err(error) => step_fail(spec, started, error, context),
    }
}

/// Resolve the client command for a run: explicit override first, otherwise
/// the first present candidate for the session's display family.
fn resolve_client_argv(
    kind: NestedBackendKind,
    options: &NestedSmokeOptions,
) -> Option<Vec<String>> {
    match &options.client {
        Some(argv) if !argv.is_empty() => Some(argv.clone()),
        _ => client_candidates(kind.family())
            .iter()
            .find(|candidate| command_in_path(candidate))
            .map(|candidate| vec![(*candidate).to_string()]),
    }
}

/// Spawn a client into the nested session with the environment matching the
/// session's display family.
fn spawn_nested_client(
    kind: NestedBackendKind,
    argv: &[String],
    context: &RunContext,
) -> Result<Child, String> {
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env("XDG_RUNTIME_DIR", &context.runtime_dir);
    match kind.family() {
        NestedFamily::Wayland => {
            let nested_socket = find_nested_wayland_socket(&context.runtime_dir)
                .ok_or("no nested wayland socket found in the private runtime directory")?;
            command
                .env("WAYLAND_DISPLAY", &nested_socket)
                .env("GDK_BACKEND", "wayland")
                .env("QT_QPA_PLATFORM", "wayland")
                .env("SDL_VIDEODRIVER", "wayland")
                .env_remove("DISPLAY");
        }
        NestedFamily::X11 => {
            let display = context
                .nested_x11_display
                .as_deref()
                .ok_or("no nested Xephyr display recorded for this run")?;
            command
                .env("DISPLAY", display)
                .env_remove("WAYLAND_DISPLAY");
        }
    }
    command
        .spawn()
        .map_err(|error| format!("spawn {argv:?}: {error}"))
}

fn run_window_lifecycle_step(
    spec: &StepSpec,
    context: &RunContext,
    options: &NestedSmokeOptions,
) -> StepReport {
    let started = Instant::now();
    let deadline = started + spec.timeout;
    let socket = context.socket_path();
    let kind = context.kind;

    let Some(argv) = resolve_client_argv(kind, options) else {
        return StepReport {
            name: spec.name,
            status: StepStatus::Skip,
            required: spec.required,
            duration_ms: elapsed_ms(started),
            detail: format!(
                "no lifecycle client found (probed: {})",
                client_candidates(kind.family()).join(", ")
            ),
            action: None,
        };
    };

    let baseline = match window_count(&socket) {
        Ok(count) => count,
        Err(error) => return step_fail(spec, started, error, context),
    };

    let mut client = match spawn_nested_client(kind, &argv, context) {
        Ok(child) => child,
        Err(error) => return step_fail(spec, started, error, context),
    };

    let mapped = poll_until(deadline, || {
        window_count(&socket).map(|count| count > baseline)
    });
    let outcome = match mapped {
        Ok(true) => match ipc_command(&socket, "killclient") {
            Ok(_) => match poll_until(deadline, || {
                window_count(&socket).map(|count| count <= baseline)
            }) {
                Ok(true) => Ok(format!(
                    "{} mapped a window and killclient removed it",
                    argv[0]
                )),
                Ok(false) => Err("window survived killclient until the step deadline".to_string()),
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        },
        Ok(false) => Err(format!(
            "{} never appeared in get_windows before the step deadline",
            argv[0]
        )),
        Err(error) => Err(error),
    };
    // The client must never outlive the step, whatever happened above.
    let _ = client.kill();
    let _ = client.wait();

    match outcome {
        Ok(detail) => StepReport {
            name: spec.name,
            status: StepStatus::Pass,
            required: spec.required,
            duration_ms: elapsed_ms(started),
            detail,
            action: None,
        },
        Err(error) => step_fail(spec, started, error, context),
    }
}

fn run_screenshot_step(spec: &StepSpec, context: &RunContext) -> StepReport {
    if context.kind.family() == NestedFamily::X11 {
        return run_x11_screenshot_step(spec, context);
    }
    let started = Instant::now();
    let socket = context.socket_path();

    match ipc_query(&socket, "get_capture_status") {
        Ok(response) => {
            if response["data"]["screencopy"]["enabled"].as_bool() != Some(true) {
                return StepReport {
                    name: spec.name,
                    status: StepStatus::Skip,
                    required: spec.required,
                    duration_ms: elapsed_ms(started),
                    detail: "backend does not advertise wlr-screencopy; frame capture is \
                             only serviced on the DRM/KMS backend"
                        .to_string(),
                    action: None,
                };
            }
        }
        Err(error) => return step_fail(spec, started, error, context),
    }
    if !command_in_path("grim") {
        return StepReport {
            name: spec.name,
            status: StepStatus::Skip,
            required: spec.required,
            duration_ms: elapsed_ms(started),
            detail: "grim is not installed".to_string(),
            action: None,
        };
    }
    let Some(nested_socket) = find_nested_wayland_socket(&context.runtime_dir) else {
        return step_fail(
            spec,
            started,
            "no nested wayland socket found in the private runtime directory".to_string(),
            context,
        );
    };
    let shot = context.runtime_dir.join("smoke-shot.png");
    let mut command = Command::new("grim");
    command
        .arg(&shot)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env("XDG_RUNTIME_DIR", &context.runtime_dir)
        .env("WAYLAND_DISPLAY", &nested_socket)
        .env_remove("DISPLAY");
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return step_fail(spec, started, format!("spawn grim: {error}"), context),
    };
    let deadline = started + spec.timeout;
    let exited = poll_until(deadline, || {
        child
            .try_wait()
            .map(|status| status.is_some())
            .map_err(|error| error.to_string())
    });
    if !matches!(exited, Ok(true)) {
        let _ = child.kill();
        let _ = child.wait();
        return step_fail(
            spec,
            started,
            "grim did not finish before the step deadline".to_string(),
            context,
        );
    }
    match fs::read(&shot) {
        Ok(bytes) if png_signature_valid(&bytes) => StepReport {
            name: spec.name,
            status: StepStatus::Pass,
            required: spec.required,
            duration_ms: elapsed_ms(started),
            detail: format!("grim wrote a valid PNG ({} bytes)", bytes.len()),
            action: None,
        },
        Ok(bytes) => step_fail(
            spec,
            started,
            format!("grim output is not a PNG ({} bytes)", bytes.len()),
            context,
        ),
        Err(error) => step_fail(
            spec,
            started,
            format!("grim wrote no file: {error}"),
            context,
        ),
    }
}

/// Capture the nested Xephyr root window with `xwd`; validates that the
/// nested X display is alive and produces readable pixels.
fn run_x11_screenshot_step(spec: &StepSpec, context: &RunContext) -> StepReport {
    let started = Instant::now();
    if !command_in_path("xwd") {
        return StepReport {
            name: spec.name,
            status: StepStatus::Skip,
            required: spec.required,
            duration_ms: elapsed_ms(started),
            detail: "xwd is not installed".to_string(),
            action: None,
        };
    }
    let Some(display) = context.nested_x11_display.as_deref() else {
        return step_fail(
            spec,
            started,
            "no nested Xephyr display recorded for this run".to_string(),
            context,
        );
    };
    let shot = context.runtime_dir.join("smoke-shot.xwd");
    let mut child = match Command::new("xwd")
        .args(["-root", "-silent", "-display", display, "-out"])
        .arg(&shot)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => return step_fail(spec, started, format!("spawn xwd: {error}"), context),
    };
    let deadline = started + spec.timeout;
    let exited = poll_until(deadline, || {
        child
            .try_wait()
            .map(|status| status.is_some())
            .map_err(|error| error.to_string())
    });
    if !matches!(exited, Ok(true)) {
        let _ = child.kill();
        let _ = child.wait();
        return step_fail(
            spec,
            started,
            "xwd did not finish before the step deadline".to_string(),
            context,
        );
    }
    match fs::read(&shot) {
        Ok(bytes) if xwd_signature_valid(&bytes) => StepReport {
            name: spec.name,
            status: StepStatus::Pass,
            required: spec.required,
            duration_ms: elapsed_ms(started),
            detail: format!(
                "xwd captured the nested root window ({} bytes)",
                bytes.len()
            ),
            action: None,
        },
        Ok(bytes) => step_fail(
            spec,
            started,
            format!("xwd output is not a valid dump ({} bytes)", bytes.len()),
            context,
        ),
        Err(error) => step_fail(
            spec,
            started,
            format!("xwd wrote no file: {error}"),
            context,
        ),
    }
}

/// Drive the fixed `SCENARIO_COMMANDS` sequence against a mapped client and
/// capture a normalized observable-state snapshot after each stage. Snapshots
/// feed the x11rb-versus-xcb differential comparison; the wayland rows skip
/// (their clients and protocol surface differ, so equality is not defined).
fn run_policy_scenario_step(
    spec: &StepSpec,
    context: &RunContext,
    options: &NestedSmokeOptions,
) -> (StepReport, Option<Vec<serde_json::Value>>) {
    let started = Instant::now();
    if context.kind.family() != NestedFamily::X11 {
        return (
            StepReport {
                name: spec.name,
                status: StepStatus::Skip,
                required: spec.required,
                duration_ms: elapsed_ms(started),
                detail: "differential scenario applies to the X11 transports".to_string(),
                action: None,
            },
            None,
        );
    }
    let Some(argv) = resolve_client_argv(context.kind, options) else {
        return (
            StepReport {
                name: spec.name,
                status: StepStatus::Skip,
                required: spec.required,
                duration_ms: elapsed_ms(started),
                detail: format!(
                    "no scenario client found (probed: {})",
                    client_candidates(context.kind.family()).join(", ")
                ),
                action: None,
            },
            None,
        );
    };
    let deadline = started + spec.timeout;
    let socket = context.socket_path();

    let baseline = match window_count(&socket) {
        Ok(count) => count,
        Err(error) => return (step_fail(spec, started, error, context), None),
    };
    let mut client = match spawn_nested_client(context.kind, &argv, context) {
        Ok(child) => child,
        Err(error) => return (step_fail(spec, started, error, context), None),
    };

    let outcome = drive_scenario(&socket, deadline, baseline);

    // The client must never outlive the step, whatever happened above.
    let _ = ipc_command(&socket, "killclient");
    let _ = poll_until(deadline, || {
        window_count(&socket).map(|count| count <= baseline)
    });
    let _ = client.kill();
    let _ = client.wait();

    match outcome {
        Ok(snapshots) => (
            StepReport {
                name: spec.name,
                status: StepStatus::Pass,
                required: spec.required,
                duration_ms: elapsed_ms(started),
                detail: format!(
                    "captured {} normalized snapshots with {}",
                    snapshots.len(),
                    argv[0]
                ),
                action: None,
            },
            Some(snapshots),
        ),
        Err(error) => (step_fail(spec, started, error, context), None),
    }
}

/// Wait for the scenario client, then take one settled snapshot per stage.
fn drive_scenario(
    socket: &Path,
    deadline: Instant,
    baseline: usize,
) -> Result<Vec<serde_json::Value>, String> {
    match poll_until(deadline, || {
        window_count(socket).map(|count| count > baseline)
    }) {
        Ok(true) => {}
        Ok(false) => return Err("scenario client never mapped a window".to_string()),
        Err(error) => return Err(error),
    }
    let mut snapshots = Vec::with_capacity(SCENARIO_COMMANDS.len() + 1);
    snapshots.push(settled_snapshot(socket, deadline)?);
    for (command, args) in SCENARIO_COMMANDS {
        let args: serde_json::Value = serde_json::from_str(args)
            .map_err(|error| format!("scenario args for {command}: {error}"))?;
        let request = serde_json::json!({ "command": command, "args": args });
        let response = ipc_roundtrip(socket, &request)?;
        expect_success(command, response)?;
        snapshots.push(settled_snapshot(socket, deadline)?);
    }
    Ok(snapshots)
}

/// Normalized snapshot that is stable across two consecutive reads, so
/// asynchronous follow-ups (property updates, arrange) cannot race the
/// comparison. Falls back to the last read at the deadline.
fn settled_snapshot(socket: &Path, deadline: Instant) -> Result<serde_json::Value, String> {
    let mut previous = read_normalized_state(socket)?;
    loop {
        std::thread::sleep(POLL_INTERVAL);
        let current = read_normalized_state(socket)?;
        if current == previous || Instant::now() >= deadline {
            return Ok(current);
        }
        previous = current;
    }
}

fn read_normalized_state(socket: &Path) -> Result<serde_json::Value, String> {
    let windows = ipc_query(socket, "get_windows")?;
    let workspaces = ipc_query(socket, "get_workspaces")?;
    Ok(normalize_observable_state(
        &windows["data"],
        &workspaces["data"],
    ))
}

fn run_shutdown_step(spec: &StepSpec, context: &mut RunContext) -> StepReport {
    let started = Instant::now();
    let deadline = started + spec.timeout;
    let socket = context.socket_path();

    let Some(child) = context.child.as_mut() else {
        return StepReport {
            name: spec.name,
            status: StepStatus::Fail,
            required: spec.required,
            duration_ms: 0,
            detail: "jwm was no longer running when shutdown was requested".to_string(),
            action: Some(format!(
                "inspect the preserved log at {}",
                context.log_path.display()
            )),
        };
    };

    let _ = ipc_command(&socket, "quit");
    let mut status = None;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(exit)) => {
                status = Some(exit);
                break;
            }
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
            Err(_) => break,
        }
    }
    match status {
        Some(exit) if exit.success() => {
            context.child = None;
            let socket_removed = !socket.exists();
            StepReport {
                name: spec.name,
                status: if socket_removed {
                    StepStatus::Pass
                } else {
                    StepStatus::Fail
                },
                required: spec.required,
                duration_ms: elapsed_ms(started),
                detail: if socket_removed {
                    format!("exited cleanly in {} ms", elapsed_ms(started))
                } else {
                    "exited cleanly but left the IPC socket behind".to_string()
                },
                action: (!socket_removed)
                    .then(|| "check the IPC server shutdown path for socket cleanup".to_string()),
            }
        }
        Some(exit) => {
            context.child = None;
            StepReport {
                name: spec.name,
                status: StepStatus::Fail,
                required: spec.required,
                duration_ms: elapsed_ms(started),
                detail: format!("jwm exited abnormally ({exit}); {}", context.log_tail()),
                action: Some(format!(
                    "inspect the preserved log at {}",
                    context.log_path.display()
                )),
            }
        }
        None => {
            kill_child(context);
            StepReport {
                name: spec.name,
                status: StepStatus::Fail,
                required: spec.required,
                duration_ms: elapsed_ms(started),
                detail: format!(
                    "quit did not terminate jwm within {:?}; the process was killed",
                    spec.timeout
                ),
                action: Some(format!(
                    "inspect the preserved log at {}",
                    context.log_path.display()
                )),
            }
        }
    }
}

// --- Small helpers ---------------------------------------------------------

fn step_fail(spec: &StepSpec, started: Instant, error: String, context: &RunContext) -> StepReport {
    StepReport {
        name: spec.name,
        status: StepStatus::Fail,
        required: spec.required,
        duration_ms: elapsed_ms(started),
        detail: error,
        action: Some(format!(
            "inspect the preserved log at {}",
            context.log_path.display()
        )),
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Poll `probe` until it returns true, errors, or the deadline passes.
fn poll_until(
    deadline: Instant,
    mut probe: impl FnMut() -> Result<bool, String>,
) -> Result<bool, String> {
    loop {
        match probe() {
            Ok(true) => return Ok(true),
            Ok(false) => {}
            Err(error) => return Err(error),
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn kill_child(context: &mut RunContext) {
    if let Some(mut child) = context.child.take() {
        if matches!(child.try_wait(), Ok(None)) {
            let _ = child.kill();
        }
        let _ = child.wait();
    }
    // The private Xephyr server has no purpose once jwm is gone.
    if let Some(mut xephyr) = context.xephyr.take() {
        if matches!(xephyr.try_wait(), Ok(None)) {
            let _ = xephyr.kill();
        }
        let _ = xephyr.wait();
    }
}

fn command_in_path(command: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|directory| {
        let candidate = directory.join(command);
        candidate
            .metadata()
            .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    })
}

fn log_tail(path: &Path, lines: usize) -> String {
    match fs::read_to_string(path) {
        Ok(content) => {
            let tail: Vec<&str> = content
                .lines()
                .rev()
                .take(lines)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            if tail.is_empty() {
                "log is empty".to_string()
            } else {
                format!("last log lines: {}", tail.join(" | "))
            }
        }
        Err(error) => format!("log unavailable: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn winit_runs_on_either_host_session_and_x11_kinds_need_display() {
        assert_eq!(
            eligible_backends(Some(":0"), None, false),
            vec![NestedBackendKind::Winit, NestedBackendKind::X11]
        );
        assert_eq!(
            eligible_backends(Some(":0"), None, true),
            vec![
                NestedBackendKind::Winit,
                NestedBackendKind::X11,
                NestedBackendKind::X11rb,
                NestedBackendKind::Xcb
            ]
        );
        // The X11 transports need both an X-capable host and Xephyr.
        assert_eq!(
            eligible_backends(None, Some("wayland-1"), true),
            vec![NestedBackendKind::Winit]
        );
        assert_eq!(eligible_backends(None, None, true), Vec::new());
        assert_eq!(eligible_backends(Some(""), Some(""), true), Vec::new());
    }

    #[test]
    fn matrix_covers_the_roadmap_steps_in_order_with_bounded_timeouts() {
        let names: Vec<&str> = MATRIX.iter().map(|spec| spec.name).collect();
        assert_eq!(
            names,
            vec![
                "startup",
                "ipc_health",
                "config_reload",
                "window_lifecycle",
                "screenshot_capture",
                "policy_scenario",
                "clean_shutdown",
            ]
        );
        for spec in MATRIX {
            assert!(
                spec.timeout <= Duration::from_secs(30),
                "{} must stay bounded",
                spec.name
            );
        }
        // Startup, health, reload and shutdown gate the run; the
        // tooling-dependent steps may skip without failing the matrix.
        assert!(step_spec("startup").unwrap().required);
        assert!(step_spec("ipc_health").unwrap().required);
        assert!(step_spec("config_reload").unwrap().required);
        assert!(step_spec("clean_shutdown").unwrap().required);
        assert!(!step_spec("window_lifecycle").unwrap().required);
        assert!(!step_spec("screenshot_capture").unwrap().required);
        assert!(!step_spec("policy_scenario").unwrap().required);
    }

    #[test]
    fn host_wayland_display_is_resolved_to_an_absolute_socket_path() {
        assert_eq!(
            absolute_wayland_display("/run/user/1000", "wayland-1"),
            PathBuf::from("/run/user/1000/wayland-1")
        );
        assert_eq!(
            absolute_wayland_display("/run/user/1000", "/custom/socket"),
            PathBuf::from("/custom/socket")
        );
    }

    #[test]
    fn child_environment_isolates_the_runtime_dir_per_backend() {
        let private = PathBuf::from("/tmp/private");
        let (set, remove) = child_env_overrides(
            NestedBackendKind::Winit,
            &private,
            Some("/run/user/1000"),
            Some("wayland-1"),
            None,
        );
        assert!(set.contains(&("XDG_RUNTIME_DIR".to_string(), "/tmp/private".to_string())));
        assert!(set.contains(&("JWM_BACKEND".to_string(), "wayland-winit".to_string())));
        // The host compositor stays reachable through an absolute path.
        assert!(set.contains(&(
            "WAYLAND_DISPLAY".to_string(),
            "/run/user/1000/wayland-1".to_string()
        )));
        assert!(remove.is_empty());

        let (set, remove) = child_env_overrides(
            NestedBackendKind::X11,
            &private,
            Some("/run/user/1000"),
            Some("wayland-1"),
            None,
        );
        assert!(set.contains(&("JWM_BACKEND".to_string(), "wayland-x11".to_string())));
        assert!(remove.contains(&"WAYLAND_DISPLAY"));

        // The X11 transports run inside Xephyr, never the host session.
        let (set, remove) = child_env_overrides(
            NestedBackendKind::Xcb,
            &private,
            Some("/run/user/1000"),
            Some("wayland-1"),
            Some(":91"),
        );
        assert!(set.contains(&("JWM_BACKEND".to_string(), "xcb".to_string())));
        assert!(set.contains(&("DISPLAY".to_string(), ":91".to_string())));
        assert!(remove.contains(&"WAYLAND_DISPLAY"));
    }

    #[test]
    fn run_result_fails_on_any_failed_step_but_tolerates_skips() {
        let pass = StepReport {
            name: "startup",
            status: StepStatus::Pass,
            required: true,
            duration_ms: 1,
            detail: String::new(),
            action: None,
        };
        let skip = StepReport {
            name: "screenshot_capture",
            status: StepStatus::Skip,
            required: false,
            duration_ms: 1,
            detail: String::new(),
            action: None,
        };
        let fail = StepReport {
            name: "clean_shutdown",
            status: StepStatus::Fail,
            required: true,
            duration_ms: 1,
            detail: String::new(),
            action: Some("action".to_string()),
        };
        assert_eq!(run_result(&[pass.clone(), skip.clone()]), RunResult::Pass);
        assert_eq!(run_result(&[pass, skip, fail]), RunResult::Fail);
    }

    #[test]
    fn exit_code_reflects_matrix_outcome_and_empty_matrices_are_errors() {
        let mut report = MatrixReport {
            schema_version: NESTED_SMOKE_SCHEMA_VERSION,
            generated_unix_ms: 0,
            host: HostInfo {
                session_type: None,
                display: None,
                wayland_display: None,
                jwm_binary: "jwm".to_string(),
            },
            runs: Vec::new(),
            differential: None,
        };
        assert_eq!(matrix_exit_code(&report), 2);
        report.runs.push(RunReport {
            backend: NestedBackendKind::Winit,
            result: RunResult::Pass,
            artifacts_dir: None,
            steps: Vec::new(),
            scenario: None,
        });
        assert_eq!(matrix_exit_code(&report), 0);
        // A transport divergence fails the matrix even with all runs green.
        report.differential = Some(DifferentialReport {
            status: DifferentialStatus::Fail,
            compared: vec!["x11rb".to_string(), "xcb".to_string()],
            detail: "diverged".to_string(),
            action: None,
        });
        assert_eq!(matrix_exit_code(&report), 1);
        report.differential = None;
        report.runs.push(RunReport {
            backend: NestedBackendKind::X11,
            result: RunResult::Fail,
            artifacts_dir: None,
            steps: Vec::new(),
            scenario: None,
        });
        assert_eq!(matrix_exit_code(&report), 1);
    }

    #[test]
    fn report_serializes_with_the_frozen_version_1_field_names() {
        let report = MatrixReport {
            schema_version: NESTED_SMOKE_SCHEMA_VERSION,
            generated_unix_ms: 42,
            host: HostInfo {
                session_type: Some("x11".to_string()),
                display: Some(":0".to_string()),
                wayland_display: None,
                jwm_binary: "/usr/bin/jwm".to_string(),
            },
            runs: vec![RunReport {
                backend: NestedBackendKind::Winit,
                result: RunResult::Fail,
                artifacts_dir: Some("/tmp/keep".to_string()),
                steps: vec![StepReport {
                    name: "startup",
                    status: StepStatus::Fail,
                    required: true,
                    duration_ms: 7,
                    detail: "boom".to_string(),
                    action: Some("look at the log".to_string()),
                }],
                scenario: None,
            }],
            differential: Some(DifferentialReport {
                status: DifferentialStatus::Pass,
                compared: vec!["x11rb".to_string(), "xcb".to_string()],
                detail: "identical".to_string(),
                action: None,
            }),
        };
        let value = serde_json::to_value(&report).expect("serialize");
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["runs"][0]["backend"], "winit");
        assert_eq!(value["runs"][0]["result"], "fail");
        assert_eq!(value["runs"][0]["steps"][0]["status"], "fail");
        assert_eq!(value["runs"][0]["steps"][0]["action"], "look at the log");
        assert_eq!(value["differential"]["status"], "pass");
        assert_eq!(value["differential"]["compared"][0], "x11rb");
        // Runs without a scenario omit the field entirely.
        assert!(value["runs"][0].get("scenario").is_none());
        // Passing steps omit `action` entirely instead of writing null.
        let pass = serde_json::to_value(StepReport {
            name: "ipc_health",
            status: StepStatus::Pass,
            required: true,
            duration_ms: 1,
            detail: String::new(),
            action: None,
        })
        .expect("serialize step");
        assert!(pass.get("action").is_none());
    }

    #[test]
    fn nested_socket_discovery_ignores_lock_files_and_missing_dirs() {
        assert_eq!(
            find_nested_wayland_socket(Path::new("/nonexistent/jwm-smoke")),
            None
        );
        let dir = std::env::temp_dir().join(format!("jwm-smoke-sock-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create test dir");
        fs::write(dir.join("wayland-1.lock"), b"").expect("write lock");
        fs::write(dir.join("jwm.log"), b"").expect("write log");
        // Plain files are not sockets, so nothing qualifies yet.
        assert_eq!(find_nested_wayland_socket(&dir), None);
        let listener =
            std::os::unix::net::UnixListener::bind(dir.join("wayland-1")).expect("bind socket");
        assert_eq!(
            find_nested_wayland_socket(&dir),
            Some("wayland-1".to_string())
        );
        drop(listener);
        let _ = fs::remove_dir_all(&dir);
    }

    fn run_with_scenario(
        backend: NestedBackendKind,
        scenario: Option<Vec<serde_json::Value>>,
    ) -> RunReport {
        RunReport {
            backend,
            result: RunResult::Pass,
            artifacts_dir: None,
            steps: Vec::new(),
            scenario,
        }
    }

    #[test]
    fn normalizer_drops_transport_ids_and_titles_and_sorts_deterministically() {
        let windows = serde_json::json!([
            {"id": 99, "name": "async title", "class": "xterm", "instance": "xterm",
             "tags": 1, "is_floating": false, "is_fullscreen": false, "is_focused": true,
             "monitor": 0, "x": 640, "y": 52, "w": 636, "h": 745},
            {"id": 3, "name": "other", "class": "xclock", "instance": "xclock",
             "tags": 1, "is_floating": true, "is_fullscreen": false, "is_focused": false,
             "monitor": 0, "x": 0, "y": 52, "w": 640, "h": 745},
        ]);
        let workspaces = serde_json::json!([
            {"tag_mask": 2, "tag_index": 1, "monitor": 0, "layout": "L", "m_fact": 0.55,
             "n_master": 1, "num_clients": 0, "focused": false},
            {"tag_mask": 1, "tag_index": 0, "monitor": 0, "layout": "L", "m_fact": 0.55,
             "n_master": 1, "num_clients": 2, "focused": true},
        ]);
        let normalized = normalize_observable_state(&windows, &workspaces);
        // Sorted by class: xclock before xterm; ids and titles gone.
        assert_eq!(normalized["windows"][0]["class"], "xclock");
        assert_eq!(normalized["windows"][1]["class"], "xterm");
        assert!(normalized["windows"][0].get("id").is_none());
        assert!(normalized["windows"][0].get("name").is_none());
        // Workspaces sorted by (monitor, tag_index).
        assert_eq!(normalized["workspaces"][0]["tag_index"], 0);
        assert_eq!(normalized["workspaces"][0]["num_clients"], 2);
        // Identical inputs in a different order normalize identically.
        let reordered_windows = serde_json::json!([windows[1].clone(), windows[0].clone(),]);
        assert_eq!(
            normalize_observable_state(&reordered_windows, &workspaces),
            normalized
        );
    }

    #[test]
    fn differential_needs_both_transports_and_detects_divergence() {
        // No X11 transports in the matrix: no differential section at all.
        assert!(
            differential_outcome(&[run_with_scenario(NestedBackendKind::Winit, None)]).is_none()
        );

        // Only one complete scenario: skip with the reason.
        let one = [
            run_with_scenario(
                NestedBackendKind::X11rb,
                Some(vec![serde_json::json!({"a": 1})]),
            ),
            run_with_scenario(NestedBackendKind::Xcb, None),
        ];
        let outcome = differential_outcome(&one).expect("x11 transports present");
        assert_eq!(outcome.status, DifferentialStatus::Skip);

        // Identical snapshots: pass, naming both transports.
        let snapshot = serde_json::json!({"windows": [], "workspaces": [{"tag_index": 0}]});
        let same = [
            run_with_scenario(NestedBackendKind::X11rb, Some(vec![snapshot.clone()])),
            run_with_scenario(NestedBackendKind::Xcb, Some(vec![snapshot.clone()])),
        ];
        let outcome = differential_outcome(&same).expect("comparison ran");
        assert_eq!(outcome.status, DifferentialStatus::Pass);
        assert_eq!(outcome.compared, vec!["x11rb", "xcb"]);

        // A divergence names the snapshot index and the differing section.
        let diverged =
            serde_json::json!({"windows": [{"class": "xterm"}], "workspaces": [{"tag_index": 0}]});
        let differ = [
            run_with_scenario(NestedBackendKind::X11rb, Some(vec![snapshot.clone()])),
            run_with_scenario(NestedBackendKind::Xcb, Some(vec![diverged])),
        ];
        let outcome = differential_outcome(&differ).expect("comparison ran");
        assert_eq!(outcome.status, DifferentialStatus::Fail);
        assert!(outcome.detail.contains("snapshot 0"));
        assert!(outcome.detail.contains("windows"));
        assert!(outcome.action.is_some());
    }

    #[test]
    fn xwd_signature_check_validates_header_size_and_version() {
        let mut header = vec![0u8; 100];
        header[..4].copy_from_slice(&100u32.to_be_bytes());
        header[4..8].copy_from_slice(&7u32.to_be_bytes());
        assert!(xwd_signature_valid(&header));
        // Wrong version.
        header[4..8].copy_from_slice(&6u32.to_be_bytes());
        assert!(!xwd_signature_valid(&header));
        // Too short.
        assert!(!xwd_signature_valid(&[0u8; 50]));
    }

    #[test]
    fn scenario_commands_parse_and_target_known_ipc_commands() {
        for (command, args) in SCENARIO_COMMANDS {
            let parsed: serde_json::Value =
                serde_json::from_str(args).expect("scenario args must be valid JSON");
            assert!(parsed.is_object() || parsed.is_null());
            assert!(!command.is_empty());
        }
    }

    #[test]
    fn png_signature_check_accepts_real_headers_only() {
        assert!(png_signature_valid(&[
            0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00
        ]));
        assert!(!png_signature_valid(b"JFIF"));
        assert!(!png_signature_valid(&[]));
    }
}
