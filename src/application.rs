//! Application composition root.
//!
//! This module owns backend construction and the top-level JWM lifecycle. The
//! binary is intentionally kept as a thin process bootstrap layer so startup
//! options can be parsed and tested without starting an X11/Wayland server.

use crate::Jwm;
use crate::backend::api::{Backend, CompositorBenchmark};
use crate::backend::error::{BackendContextExt, BackendError, ErrorBoundary};
#[cfg(feature = "backend-wayland-udev")]
use crate::backend::wayland_udev::backend::UdevBackend;
#[cfg(feature = "backend-wayland-nested")]
use crate::backend::wayland_winit::backend::WaylandWinitBackend;
#[cfg(feature = "backend-wayland-nested")]
use crate::backend::wayland_x11::backend::WaylandX11Backend;
#[cfg(feature = "backend-x11rb")]
use crate::backend::x11rb::backend::X11rbBackend;
#[cfg(feature = "backend-xcb")]
use crate::backend::xcb::backend::XcbBackend;
use crate::config::{
    BackendFamily, CONFIG, Config, ConfigDiagnostics, ConfigError, set_backend_family,
};
use crate::jwm::process::{TRANSIENT_CHILD_HANDOFF_ENV, TransientChildRestartHandoff};
use crate::jwm::scratchpad_handoff::{
    LEGACY_SCRATCHPAD_HANDOFF_ENV, SCRATCHPAD_HANDOFF_ENV, ScratchpadRestartHandoff,
};
use log::{error, info, warn};
use std::env;
use std::ffi::{OsStr, OsString};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

pub const BACKEND_ENV: &str = "JWM_BACKEND";
pub const BENCHMARK_ENV: &str = "JWM_BENCHMARK";
pub const BENCHMARK_WARMUP_ENV: &str = "JWM_BENCHMARK_WARMUP";
const RESTART_MARKER_ENV: &str = "JWM_RESTARTING";
const DBUS_LAUNCH_TIMEOUT: Duration = Duration::from_secs(5);
const DBUS_LAUNCH_OUTPUT_LIMIT: usize = 64 * 1024;
const RESTART_BOOTSTRAP_BACKOFFS: [Duration; 4] = [
    Duration::from_millis(20),
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
];

/// Start a private D-Bus session bus and return its shell environment output.
///
/// The short-lived launcher is bounded, but a daemon left behind by a
/// successful launch is intentionally allowed to keep running.
#[doc(hidden)]
pub fn launch_dbus_session() -> std::io::Result<std::process::Output> {
    crate::external_command::daemon_launcher_output_with_limits(
        "dbus-launch",
        &["--sh-syntax"],
        DBUS_LAUNCH_TIMEOUT,
        DBUS_LAUNCH_OUTPUT_LIMIT,
    )
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BackendChoice {
    #[default]
    X11rb,
    Xcb,
    WaylandUdev,
    WaylandX11,
    WaylandWinit,
}

impl BackendChoice {
    #[must_use]
    pub const fn family(self) -> BackendFamily {
        match self {
            Self::X11rb | Self::Xcb => BackendFamily::X11,
            Self::WaylandUdev | Self::WaylandX11 | Self::WaylandWinit => BackendFamily::Wayland,
        }
    }

    #[must_use]
    pub const fn config_name(self) -> &'static str {
        match self.family() {
            BackendFamily::X11 => "config_x11.toml",
            BackendFamily::Wayland => "config_wayland.toml",
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::X11rb => "x11rb",
            Self::Xcb => "xcb",
            Self::WaylandUdev => "wayland-udev",
            Self::WaylandX11 => "wayland-x11",
            Self::WaylandWinit => "wayland-winit",
        }
    }

    /// The Cargo feature that compiles this backend into the binary.
    #[must_use]
    pub const fn feature_name(self) -> &'static str {
        match self {
            Self::X11rb => "backend-x11rb",
            Self::Xcb => "backend-xcb",
            Self::WaylandUdev => "backend-wayland-udev",
            Self::WaylandX11 | Self::WaylandWinit => "backend-wayland-nested",
        }
    }

    /// Whether this backend was compiled into the running binary.
    #[must_use]
    pub const fn is_compiled(self) -> bool {
        match self {
            Self::X11rb => cfg!(feature = "backend-x11rb"),
            Self::Xcb => cfg!(feature = "backend-xcb"),
            Self::WaylandUdev => cfg!(feature = "backend-wayland-udev"),
            Self::WaylandX11 | Self::WaylandWinit => cfg!(feature = "backend-wayland-nested"),
        }
    }
}

/// Names of the backends compiled into this binary, in canonical order.
#[must_use]
pub fn compiled_backends() -> Vec<&'static str> {
    [
        BackendChoice::X11rb,
        BackendChoice::Xcb,
        BackendChoice::WaylandUdev,
        BackendChoice::WaylandX11,
        BackendChoice::WaylandWinit,
    ]
    .into_iter()
    .filter(|choice| choice.is_compiled())
    .map(BackendChoice::as_str)
    .collect()
}

impl FromStr for BackendChoice {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "x11rb" => Ok(Self::X11rb),
            "xcb" | "x11-xcb" => Ok(Self::Xcb),
            "wayland-udev" | "udev" | "wayland" => Ok(Self::WaylandUdev),
            "wayland-x11" | "x11-wayland" | "windowed" => Ok(Self::WaylandX11),
            "wayland-winit" | "winit" => Ok(Self::WaylandWinit),
            other => Err(format!(
                "unknown backend {other:?}; expected one of: x11rb, xcb, wayland-udev, wayland-x11, wayland-winit"
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BenchmarkRequest {
    pub frames: u32,
    pub warmup: u32,
}

impl BenchmarkRequest {
    /// Maximum number of measured frames retained by one benchmark run.
    ///
    /// A benchmark stores several per-frame sample streams in memory. Keeping
    /// this at 100,000 bounds those buffers while still allowing roughly 28
    /// minutes of measurements at 60 Hz.
    pub const MAX_FRAMES: u32 = 100_000;

    /// Maximum number of frames discarded before measurement begins.
    ///
    /// Warm-up frames are not retained, but bounding them prevents an
    /// accidental or remote request from keeping benchmark mode in warm-up
    /// indefinitely (10,000 frames is roughly 2.8 minutes at 60 Hz).
    pub const MAX_WARMUP_FRAMES: u32 = 10_000;

    /// Validate and construct a bounded benchmark request.
    ///
    /// # Errors
    ///
    /// Returns an error when the measured frame count is zero or either count
    /// exceeds its documented resource limit.
    pub fn new(frames: u32, warmup: u32) -> Result<Self, String> {
        if frames == 0 {
            return Err("benchmark frame count must be greater than zero".to_string());
        }
        if frames > Self::MAX_FRAMES {
            return Err(format!(
                "benchmark frame count {frames} exceeds the maximum of {}",
                Self::MAX_FRAMES
            ));
        }
        if warmup > Self::MAX_WARMUP_FRAMES {
            return Err(format!(
                "benchmark warm-up frame count {warmup} exceeds the maximum of {}",
                Self::MAX_WARMUP_FRAMES
            ));
        }
        Ok(Self { frames, warmup })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ApplicationOptions {
    pub backend: BackendChoice,
    pub benchmark: Option<BenchmarkRequest>,
}

impl ApplicationOptions {
    pub fn from_env() -> Result<Self, String> {
        Ok(Self {
            backend: configured_backend()?,
            benchmark: configured_benchmark()?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedConfig {
    pub path: PathBuf,
    pub backup: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigCheck {
    pub path: PathBuf,
    pub diagnostics: ConfigDiagnostics,
}

/// Resolve the configured backend without constructing it.
///
/// Environment access is kept at this boundary for compatibility. New callers
/// should prefer constructing `ApplicationOptions` explicitly.
pub fn configured_backend() -> Result<BackendChoice, String> {
    env::var(BACKEND_ENV).map_or_else(
        |error| match error {
            env::VarError::NotPresent => Ok(BackendChoice::default()),
            env::VarError::NotUnicode(_) => Err(format!("{BACKEND_ENV} is not valid UTF-8")),
        },
        |value| value.parse(),
    )
}

#[must_use]
pub fn config_path(choice: BackendChoice) -> PathBuf {
    Config::get_config_path_for(choice.family())
}

pub fn generate_config_templates() -> Result<Vec<GeneratedConfig>, ConfigError> {
    let mut generated = Vec::with_capacity(2);
    for family in [BackendFamily::X11, BackendFamily::Wayland] {
        let path = Config::get_config_path_for(family);
        let backup = if path.exists() {
            Some(Config::backup_config(&path)?)
        } else {
            None
        };
        Config::generate_template(&path)?;
        generated.push(GeneratedConfig { path, backup });
    }
    Ok(generated)
}

pub fn validate_config(choice: BackendChoice) -> Result<ConfigCheck, ConfigError> {
    let path = config_path(choice);
    let diagnostics = Config::validate_config_file(&path)?;
    Ok(ConfigCheck { path, diagnostics })
}

fn preflight_config(choice: BackendChoice) -> Result<(), ConfigError> {
    let path = config_path(choice);
    if !path.exists() {
        return Ok(());
    }

    let diagnostics = Config::validate_config_file(&path)?;
    if diagnostics.has_errors() {
        return Err(ConfigError::Validation(diagnostics));
    }
    Ok(())
}

fn create_backend(choice: BackendChoice) -> Result<Box<dyn Backend>, Box<dyn std::error::Error>> {
    if !choice.is_compiled() {
        return Err(format!(
            "backend '{}' is not compiled into this jwm binary (rebuild with \
             --features {}); compiled backends: {}",
            choice.as_str(),
            choice.feature_name(),
            compiled_backends().join(", ")
        )
        .into());
    }
    preflight_config(choice)?;
    // Config is a process-wide singleton, so its family must be established
    // before any backend constructor can access CONFIG.
    set_backend_family(choice.family());
    info!(
        "Initializing {} backend (config: {})",
        choice.as_str(),
        choice.config_name()
    );

    #[allow(unreachable_patterns)]
    let backend: Result<Box<dyn Backend>, BackendError> = match choice {
        #[cfg(feature = "backend-x11rb")]
        BackendChoice::X11rb => X11rbBackend::new().map(|b| Box::new(b) as Box<dyn Backend>),
        #[cfg(feature = "backend-xcb")]
        BackendChoice::Xcb => XcbBackend::new().map(|b| Box::new(b) as Box<dyn Backend>),
        #[cfg(feature = "backend-wayland-udev")]
        BackendChoice::WaylandUdev => UdevBackend::new().map(|b| Box::new(b) as Box<dyn Backend>),
        #[cfg(feature = "backend-wayland-nested")]
        BackendChoice::WaylandX11 => {
            WaylandX11Backend::new().map(|b| Box::new(b) as Box<dyn Backend>)
        }
        #[cfg(feature = "backend-wayland-nested")]
        BackendChoice::WaylandWinit => {
            WaylandWinitBackend::new().map(|b| Box::new(b) as Box<dyn Backend>)
        }
        // Uncompiled variants are rejected by the is_compiled guard above;
        // this arm only matters if that guard is ever bypassed.
        _ => Err(BackendError::Message(format!(
            "backend '{}' is not compiled into this jwm binary",
            choice.as_str()
        ))),
    };
    Ok(backend.backend_context(
        choice.as_str(),
        ErrorBoundary::Display,
        "initialize display backend",
    )?)
}

/// Temporarily expose restart intent to backends that must distinguish their
/// own inherited display socket from a genuinely nested session. The marker is
/// restored as soon as backend construction finishes, so the replacement WM's
/// event loop and subsequently spawned children do not inherit a one-shot
/// bootstrap signal.
struct RestartMarkerGuard {
    previous: Option<OsString>,
}

impl RestartMarkerGuard {
    fn install() -> Self {
        let previous = env::var_os(RESTART_MARKER_ENV);
        // SAFETY: application bootstrap owns process-environment mutation at
        // this boundary, before the new backend and its worker threads exist.
        unsafe { env::set_var(RESTART_MARKER_ENV, "1") };
        Self { previous }
    }
}

impl Drop for RestartMarkerGuard {
    fn drop(&mut self) {
        // SAFETY: this guard is scoped to synchronous backend construction and
        // is dropped before the replacement JWM starts its event loop.
        unsafe {
            if let Some(previous) = self.previous.as_ref() {
                env::set_var(RESTART_MARKER_ENV, previous);
            } else {
                env::remove_var(RESTART_MARKER_ENV);
            }
        }
    }
}

fn take_restart_intent_from_environment() -> bool {
    let marker = env::var_os(RESTART_MARKER_ENV);
    // Consume the exec marker once at the composition root. A narrowly scoped
    // guard reinstalls it around backend construction when it is authoritative.
    // SAFETY: this runs before any backend or application worker is created.
    unsafe { env::remove_var(RESTART_MARKER_ENV) };
    let restarting = marker.as_deref() == Some(OsStr::new("1"));
    if marker.is_some() && !restarting {
        warn!("[application] ignoring invalid {RESTART_MARKER_ENV} marker");
    }
    restarting
}

fn reload_validated_global_config(choice: BackendChoice) -> Result<(), ConfigError> {
    let path = config_path(choice);
    if !path.exists() {
        // There is no disk snapshot to install. Keeping the current validated
        // in-memory value is safer for an in-process recovery than silently
        // replacing it with unrelated defaults.
        return Ok(());
    }

    let config = Config::load_from_file(&path)?;
    // CONFIG's Lazy initializer also resolves a backend-specific path. Set the
    // family before the first possible access, especially in a freshly exec'd
    // Wayland process where X11 would otherwise be the compatibility default.
    set_backend_family(choice.family());
    CONFIG.store(Arc::new(config));
    Ok(())
}

fn create_backend_for_startup(
    choice: BackendChoice,
    restart_intent: bool,
) -> Result<Box<dyn Backend>, Box<dyn std::error::Error>> {
    let _restart_marker = restart_intent.then(RestartMarkerGuard::install);
    create_backend(choice)
}

fn bootstrap_jwm_instance(
    options: ApplicationOptions,
    scratchpad_handoff: Option<&ScratchpadRestartHandoff>,
    transient_child_handoff: Option<&TransientChildRestartHandoff>,
    restart_intent: bool,
) -> Result<(Box<dyn Backend>, Jwm), Box<dyn std::error::Error>> {
    if restart_intent {
        // A real exec would initialize CONFIG from disk in the replacement
        // process. Do the same after exec(2) fails instead of accidentally
        // carrying the old process's ArcSwap value into a fresh backend.
        reload_validated_global_config(options.backend)?;
    }

    info!(
        "[application] starting JWM instance with backend {}",
        options.backend.as_str()
    );
    let mut backend = create_backend_for_startup(options.backend, restart_intent)?;
    backend.check_existing_wm().backend_context(
        options.backend.as_str(),
        ErrorBoundary::Display,
        "acquire window-manager selection",
    )?;

    let mut jwm = Jwm::new_with_runtime_backend(&mut *backend, options.backend.as_str())?;
    if let Some(handoff) = scratchpad_handoff {
        // Borrow the immutable handoff for every bounded attempt. Ownership is
        // consumed only after a complete setup succeeds.
        jwm.install_pending_scratchpad_restart_handoff(handoff);
    }
    if let Some(handoff) = transient_child_handoff {
        jwm.install_transient_child_restart_handoff(handoff.clone())?;
    }
    jwm.setup(&mut *backend)?;
    Ok((backend, jwm))
}

fn run_restart_bootstrap_with_retry<T>(
    mut attempt: impl FnMut() -> Result<T, Box<dyn std::error::Error>>,
    mut wait: impl FnMut(Duration),
) -> Result<T, Box<dyn std::error::Error>> {
    let total_attempts = RESTART_BOOTSTRAP_BACKOFFS.len() + 1;
    for (failure_index, delay) in RESTART_BOOTSTRAP_BACKOFFS.iter().copied().enumerate() {
        match attempt() {
            Ok(value) => return Ok(value),
            Err(error) => {
                warn!(
                    "[application] restart bootstrap attempt {}/{} failed: {error}; retrying in {} ms",
                    failure_index + 1,
                    total_attempts,
                    delay.as_millis()
                );
                wait(delay);
            }
        }
    }

    attempt().map_err(|error| {
        Box::new(std::io::Error::other(format!(
            "restart bootstrap exhausted {total_attempts} attempts: {error}"
        ))) as Box<dyn std::error::Error>
    })
}

#[derive(Debug)]
struct RestartCommand {
    executable: OsString,
    arguments: Vec<OsString>,
}

impl RestartCommand {
    fn current() -> Self {
        let mut arguments = env::args_os();
        let invoked_as = arguments.next().unwrap_or_else(|| OsString::from("jwm"));
        let executable = env::current_exe().map_or(invoked_as, |path| path.into_os_string());
        Self {
            executable,
            arguments: arguments.collect(),
        }
    }

    fn command(
        &self,
        scratchpad_handoff: Option<&str>,
        transient_child_handoff: Option<&str>,
    ) -> Command {
        let mut command = Command::new(&self.executable);
        command
            .args(&self.arguments)
            .env(RESTART_MARKER_ENV, "1")
            // Never accidentally forward a payload inherited from an older
            // process. Only the snapshot captured for this exact exec may be
            // installed below.
            .env_remove(SCRATCHPAD_HANDOFF_ENV)
            .env_remove(LEGACY_SCRATCHPAD_HANDOFF_ENV)
            .env_remove(TRANSIENT_CHILD_HANDOFF_ENV);
        if let Some(payload) = scratchpad_handoff {
            command.env(SCRATCHPAD_HANDOFF_ENV, payload);
        }
        if let Some(payload) = transient_child_handoff {
            command.env(TRANSIENT_CHILD_HANDOFF_ENV, payload);
        }
        command
    }

    fn exec(
        &self,
        scratchpad_handoff: Option<&str>,
        transient_child_handoff: Option<&str>,
    ) -> std::io::Error {
        self.command(scratchpad_handoff, transient_child_handoff)
            .exec()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RestartPreparationStage {
    FlushLayout,
    ValidateConfig,
    CaptureScratchpads,
    CaptureTransientChildren,
    PrepareClients,
}

impl RestartPreparationStage {
    const ORDER: [Self; 5] = [
        Self::FlushLayout,
        Self::ValidateConfig,
        Self::CaptureTransientChildren,
        Self::CaptureScratchpads,
        Self::PrepareClients,
    ];

    const fn description(self) -> &'static str {
        match self {
            Self::FlushLayout => "layout persistence",
            Self::ValidateConfig => "configuration validation",
            Self::CaptureScratchpads => "scratchpad identity handoff",
            Self::CaptureTransientChildren => "transient-child ownership handoff",
            Self::PrepareClients => "X11 client discovery handoff",
        }
    }
}

/// Run the cancellable restart stages in their safety order. The caller keeps
/// every captured proof in local storage and crosses into cleanup only after
/// this function has visited the complete sequence.
fn run_restart_preparation_steps<E>(
    mut step: impl FnMut(RestartPreparationStage) -> Result<(), E>,
) -> Result<(), (RestartPreparationStage, E)> {
    for stage in RestartPreparationStage::ORDER {
        step(stage).map_err(|error| (stage, error))?;
    }
    Ok(())
}

/// A successful handoff preflight is the restart transaction's commit point.
/// Cleanup after that point is best-effort: dropping the old backend releases
/// its remaining display resources, while aborting here would leave clients in
/// restart-preserved hidden geometry with no replacement window manager.
fn execute_restart_after_cleanup<T>(
    cleanup_result: Result<(), Box<dyn std::error::Error>>,
    executor: impl FnOnce() -> T,
) -> T {
    if let Err(error) = cleanup_result {
        error!(
            "[application] restart cleanup was incomplete after successful handoff: {error}; continuing with exec"
        );
    }
    executor()
}

fn decode_restart_handoff(
    restarting: bool,
    payload: Option<&OsStr>,
    current_pid: u32,
) -> Result<Option<ScratchpadRestartHandoff>, String> {
    // A payload without JWM's exec marker is ordinary inherited environment
    // residue and has no authority over newly managed windows.
    if !restarting {
        return Ok(None);
    }
    let Some(payload) = payload else {
        return Ok(None);
    };
    let payload = payload
        .to_str()
        .ok_or_else(|| format!("{SCRATCHPAD_HANDOFF_ENV} is not valid UTF-8"))?;
    ScratchpadRestartHandoff::decode_for_pid(payload, current_pid)
        .map(Some)
        .map_err(|error| error.to_string())
}

fn take_restart_handoff_from_environment(restarting: bool) -> Option<ScratchpadRestartHandoff> {
    let payload = env::var_os(SCRATCHPAD_HANDOFF_ENV);
    // This is called at the process composition root before a backend (and
    // its worker threads) is constructed. Consuming the value prevents JWM's
    // own children or a later restart from inheriting a replayable payload.
    unsafe { env::remove_var(SCRATCHPAD_HANDOFF_ENV) };
    // V1 never carried pending launches. It is deliberately not decoded by
    // the strict V2 reader, but remove it so an upgraded exec cannot leak the
    // obsolete one-shot capability to children.
    unsafe { env::remove_var(LEGACY_SCRATCHPAD_HANDOFF_ENV) };
    match decode_restart_handoff(restarting, payload.as_deref(), std::process::id()) {
        Ok(handoff) => handoff,
        Err(error) => {
            error!("[application] rejecting scratchpad restart handoff: {error}");
            None
        }
    }
}

fn decode_transient_child_restart_handoff(
    restarting: bool,
    payload: Option<&OsStr>,
    current_pid: u32,
) -> Result<Option<TransientChildRestartHandoff>, String> {
    // Exact child PIDs are authoritative only for the same process after JWM's
    // own exec marker. Ordinary inherited environment residue is ignored.
    if !restarting {
        return Ok(None);
    }
    let Some(payload) = payload else {
        return Ok(None);
    };
    let payload = payload
        .to_str()
        .ok_or_else(|| format!("{TRANSIENT_CHILD_HANDOFF_ENV} is not valid UTF-8"))?;
    TransientChildRestartHandoff::decode_for_parent_pid(payload, current_pid)
        .map(Some)
        .map_err(|error| error.to_string())
}

fn take_transient_child_handoff_from_environment(
    restarting: bool,
) -> Option<TransientChildRestartHandoff> {
    let payload = env::var_os(TRANSIENT_CHILD_HANDOFF_ENV);
    // Consume the one-shot capability before backend construction can spawn
    // workers or helper processes that might otherwise inherit it.
    unsafe { env::remove_var(TRANSIENT_CHILD_HANDOFF_ENV) };
    match decode_transient_child_restart_handoff(restarting, payload.as_deref(), std::process::id())
    {
        Ok(handoff) => handoff,
        Err(error) => {
            error!("[application] rejecting transient-child restart handoff: {error}");
            None
        }
    }
}

/// Run JWM using environment-based compatibility options.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    run_with_options(ApplicationOptions::from_env()?)
}

/// Run JWM until it exits or replaces itself during a restart.
pub fn run_with_options(options: ApplicationOptions) -> Result<(), Box<dyn std::error::Error>> {
    let restart_command = RestartCommand::current();
    let inherited_restart_intent = take_restart_intent_from_environment();
    let mut pending_scratchpad_handoff =
        take_restart_handoff_from_environment(inherited_restart_intent);
    let mut pending_transient_child_handoff =
        take_transient_child_handoff_from_environment(inherited_restart_intent);
    let mut restart_bootstrap = inherited_restart_intent;

    loop {
        let startup = || {
            bootstrap_jwm_instance(
                options,
                pending_scratchpad_handoff.as_ref(),
                pending_transient_child_handoff.as_ref(),
                restart_bootstrap,
            )
        };
        let (mut backend, mut jwm) = if restart_bootstrap {
            run_restart_bootstrap_with_retry(startup, std::thread::sleep)?
        } else {
            startup()?
        };

        // `Jwm::setup` owns the one authoritative root-child scan. Running a
        // second QueryTree pass here both duplicates management work and can
        // turn a successful adoption into startup failure if the redundant
        // round-trip races a disconnect.
        if let Some(handoff) = pending_scratchpad_handoff.take() {
            jwm.adopt_scratchpad_restart_handoff(&*backend, handoff);
        }
        // The immutable PID handoff was cloned into this successful JWM
        // during bootstrap. Failed bootstrap attempts leave it available for
        // retry; a successful one consumes the one-shot application copy.
        pending_transient_child_handoff.take();
        configure_benchmark(&mut *backend, options.benchmark);

        // Exit preparation is fail-closed. Keep this JWM and its ClientKeys
        // alive when either restart identity encoding or the bounded
        // swallowed-parent remap cannot be completed. In particular, cleanup
        // must not drop the only registry that knows an unmapped parent.
        let (
            prepared_scratchpad_handoff,
            prepared_transient_child_handoff,
            prepared_restart_clients,
            prepared_normal_exit_handoff,
        ) = loop {
            jwm.run(&mut *backend)?;
            let restarting = jwm.is_restarting.load(Ordering::SeqCst);
            if restarting {
                let mut prepared = None;
                let mut transient_children = None;
                let mut restart_clients = None;
                let preparation = run_restart_preparation_steps(
                    |stage| -> Result<(), Box<dyn std::error::Error>> {
                        match stage {
                            RestartPreparationStage::FlushLayout => {
                                jwm.flush_layout_persistence_on_exit()?;
                            }
                            RestartPreparationStage::ValidateConfig => {
                                preflight_config(options.backend)?;
                            }
                            RestartPreparationStage::CaptureScratchpads => {
                                let handoff = jwm.capture_scratchpad_restart_handoff(&*backend)?;
                                let payload = handoff.encode()?;
                                prepared = Some((handoff, payload));
                            }
                            RestartPreparationStage::CaptureTransientChildren => {
                                let handoff = jwm.capture_transient_child_restart_handoff()?;
                                let payload = handoff.encode()?;
                                transient_children = Some((handoff, payload));
                            }
                            RestartPreparationStage::PrepareClients => {
                                restart_clients = Some(jwm.prepare_restart_clients(&mut *backend)?);
                            }
                        }
                        Ok(())
                    },
                );
                if let Err((stage, error)) = preparation {
                    error!(
                        "[application] restart cancelled during {}: {error}",
                        stage.description()
                    );
                    jwm.is_restarting.store(false, Ordering::SeqCst);
                    jwm.running.store(true, Ordering::SeqCst);
                    continue;
                }
                break (prepared, transient_children, restart_clients, None);
            }

            match jwm.prepare_normal_exit_handoff(&mut *backend) {
                Ok(handoff) => break (None, None, None, Some(handoff)),
                Err(error) => {
                    if error.resume_safe() {
                        // Phase A restored every touched client in reverse
                        // order and verified the resulting server state. Keep
                        // this backend and its ClientKeys alive instead of
                        // crossing into destructive cleanup.
                        error!(
                            "[application] shutdown cancelled; verified rollback restored the active WM: {error}"
                        );
                    } else {
                        // Dropping the backend here would lose the remaining
                        // WM/compositor ownership and any pinned Iconic
                        // snapshot. The least destructive fallback is to keep
                        // servicing this display with the same JWM while
                        // making the unconfirmed rollback explicit. A future
                        // exit request retries the idempotent preflight.
                        error!(
                            "[application] CRITICAL: shutdown cancelled after an unconfirmed rollback; retaining the active WM and refusing destructive cleanup: {error}"
                        );
                    }
                    jwm.is_restarting.store(false, Ordering::SeqCst);
                    jwm.running.store(true, Ordering::SeqCst);
                }
            }
        };
        let restarting = jwm.is_restarting.load(Ordering::SeqCst);
        let cleanup_result = if restarting {
            let _restart_clients = prepared_restart_clients
                .expect("restart cannot cross cleanup without a client preflight proof");
            jwm.cleanup(&mut *backend)
        } else {
            jwm.cleanup_after_normal_exit_handoff(
                &mut *backend,
                prepared_normal_exit_handoff
                    .expect("normal exit cannot cross cleanup without a handoff proof"),
            )
        };

        if restarting {
            info!("[application] restarting via exec");
            let (handoff, payload) = prepared_scratchpad_handoff
                .expect("a restart cannot pass the handoff preparation guard without a payload");
            let (transient_child_handoff, transient_child_payload) =
                prepared_transient_child_handoff
                    .expect("a restart cannot drop JWM-owned child handles without a PID handoff");
            drop(jwm);
            drop(backend);

            let error = execute_restart_after_cleanup(cleanup_result, || {
                restart_command.exec(Some(&payload), Some(&transient_child_payload))
            });
            error!("[application] exec failed: {error}; falling back to in-process restart");
            // `exec` failure leaves us in the original process. The old JWM
            // is gone, but the immutable identity table reconstructs the same
            // scratchpad mapping after the fallback instance adopts its
            // windows. Swallowed parents need no payload: exit preflight
            // already made that bounded set viewable, so the new backend's
            // ordinary startup scan will find them too.
            pending_scratchpad_handoff = Some(handoff);
            pending_transient_child_handoff = Some(transient_child_handoff);
            restart_bootstrap = true;
            continue;
        }

        if let Err(error) = Command::new("jwm-tool").arg("quit").spawn() {
            error!("[application] failed to quit jwm daemon: {error}");
        }
        cleanup_result?;
        return Ok(());
    }
}

fn parse_benchmark(
    frames: Option<&str>,
    warmup: Option<&str>,
) -> Result<Option<BenchmarkRequest>, String> {
    let Some(frames) = frames else {
        if warmup.is_some() {
            return Err(format!("{BENCHMARK_WARMUP_ENV} requires {BENCHMARK_ENV}"));
        }
        return Ok(None);
    };

    let frames = frames
        .parse::<u32>()
        .map_err(|error| format!("invalid {BENCHMARK_ENV} value {frames:?}: {error}"))?;
    let warmup = warmup.map_or(Ok(60), |value| {
        value
            .parse::<u32>()
            .map_err(|error| format!("invalid {BENCHMARK_WARMUP_ENV} value {value:?}: {error}"))
    })?;

    BenchmarkRequest::new(frames, warmup).map(Some)
}

fn configured_benchmark() -> Result<Option<BenchmarkRequest>, String> {
    let frames = env::var(BENCHMARK_ENV).ok();
    let warmup = env::var(BENCHMARK_WARMUP_ENV).ok();
    parse_benchmark(frames.as_deref(), warmup.as_deref())
}

fn configure_benchmark<B: CompositorBenchmark + ?Sized>(
    backend: &mut B,
    request: Option<BenchmarkRequest>,
) {
    let Some(request) = request else {
        return;
    };
    let request = match BenchmarkRequest::new(request.frames, request.warmup) {
        Ok(request) => request,
        Err(error) => {
            error!("Benchmark mode rejected: {error}");
            return;
        }
    };
    if !backend.compositor_benchmark_start(request.frames, request.warmup) {
        error!("Benchmark mode could not start: compositor unavailable or request refused");
        return;
    }
    backend.compositor_benchmark_set_auto_exit(true);
    info!(
        "Benchmark mode: collecting {} frames (warmup={})",
        request.frames, request.warmup
    );
}

#[cfg(test)]
mod tests {
    use super::{
        ApplicationOptions, BackendChoice, BenchmarkRequest, RESTART_BOOTSTRAP_BACKOFFS,
        RESTART_MARKER_ENV, RestartCommand, RestartPreparationStage, config_path,
        configure_benchmark, decode_restart_handoff, decode_transient_child_restart_handoff,
        execute_restart_after_cleanup, parse_benchmark, run_restart_bootstrap_with_retry,
        run_restart_preparation_steps,
    };
    use crate::backend::api::CompositorBenchmark;
    use crate::config::BackendFamily;
    use crate::core::state::WMState;
    use crate::jwm::process::TRANSIENT_CHILD_HANDOFF_ENV;
    use crate::jwm::scratchpad_handoff::{
        LEGACY_SCRATCHPAD_HANDOFF_ENV, SCRATCHPAD_HANDOFF_ENV, ScratchpadRestartHandoff,
    };
    use std::cell::Cell;
    use std::ffi::{OsStr, OsString};
    use std::io;

    const APPLICATION_SRC: &str = include_str!("application.rs");

    fn function_body_after<'a>(source: &'a str, signature: &str) -> &'a str {
        let start = source
            .find(signature)
            .unwrap_or_else(|| panic!("missing `{signature}`"));
        let body_start = source[start..]
            .find('{')
            .map(|offset| start + offset + 1)
            .unwrap_or_else(|| panic!("missing body for `{signature}`"));
        let mut depth = 1usize;
        for (offset, ch) in source[body_start..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return &source[body_start..body_start + offset];
                    }
                }
                _ => {}
            }
        }
        panic!("unterminated body for `{signature}`");
    }

    #[derive(Default)]
    struct BenchmarkSpy {
        started: Option<(u32, u32)>,
        auto_exit: bool,
    }

    impl CompositorBenchmark for BenchmarkSpy {
        fn compositor_benchmark_start(&mut self, frames: u32, warmup: u32) -> bool {
            self.started = Some((frames, warmup));
            true
        }

        fn compositor_benchmark_set_auto_exit(&mut self, enabled: bool) {
            self.auto_exit = enabled;
        }
    }

    #[test]
    fn backend_aliases_are_parsed() {
        assert_eq!("x11rb".parse(), Ok(BackendChoice::X11rb));
        assert_eq!("X11-XCB".parse(), Ok(BackendChoice::Xcb));
        assert_eq!("wayland".parse(), Ok(BackendChoice::WaylandUdev));
        assert_eq!("windowed".parse(), Ok(BackendChoice::WaylandX11));
        assert_eq!("winit".parse(), Ok(BackendChoice::WaylandWinit));
    }

    #[test]
    fn compiled_backends_reflect_the_enabled_features() {
        let compiled = super::compiled_backends();
        assert_eq!(compiled.contains(&"x11rb"), cfg!(feature = "backend-x11rb"));
        assert_eq!(compiled.contains(&"xcb"), cfg!(feature = "backend-xcb"));
        assert_eq!(
            compiled.contains(&"wayland-udev"),
            cfg!(feature = "backend-wayland-udev")
        );
        assert_eq!(
            compiled.contains(&"wayland-x11"),
            cfg!(feature = "backend-wayland-nested")
        );
        assert_eq!(
            compiled.contains(&"wayland-winit"),
            cfg!(feature = "backend-wayland-nested")
        );
        // Every backend advertises the feature that would compile it in.
        assert_eq!(BackendChoice::Xcb.feature_name(), "backend-xcb");
        assert_eq!(
            BackendChoice::WaylandWinit.feature_name(),
            "backend-wayland-nested"
        );
    }

    #[test]
    fn invalid_backend_reports_supported_choices() {
        let error = "invalid".parse::<BackendChoice>().unwrap_err();
        assert!(error.contains("x11rb"));
        assert!(error.contains("wayland-winit"));
    }

    #[test]
    fn backend_family_matches_configuration_format() {
        assert_eq!(BackendChoice::Xcb.family(), BackendFamily::X11);
        assert_eq!(BackendChoice::WaylandUdev.family(), BackendFamily::Wayland);
        assert_eq!(BackendChoice::X11rb.config_name(), "config_x11.toml");
        assert_eq!(
            BackendChoice::WaylandWinit.config_name(),
            "config_wayland.toml"
        );
        assert_eq!(
            config_path(BackendChoice::X11rb)
                .file_name()
                .and_then(|name| name.to_str()),
            Some("config_x11.toml")
        );
    }

    #[test]
    fn explicit_application_options_have_stable_defaults() {
        assert_eq!(
            ApplicationOptions::default(),
            ApplicationOptions {
                backend: BackendChoice::X11rb,
                benchmark: None,
            }
        );
    }

    #[test]
    fn application_startup_delegates_initial_window_scan_to_setup_once() {
        let startup = function_body_after(APPLICATION_SRC, "fn bootstrap_jwm_instance");
        assert_eq!(
            startup.matches("jwm.setup(&mut *backend)?;").count(),
            1,
            "one bootstrap attempt must enter Jwm::setup exactly once"
        );
        assert!(
            !startup.contains("jwm.setup_initial_windows("),
            "Jwm::setup already owns QueryTree adoption; a second application-level scan can turn a successful adoption into startup failure"
        );

        let application = function_body_after(APPLICATION_SRC, "pub fn run_with_options");
        assert!(application.contains("bootstrap_jwm_instance("));
        assert!(!application.contains("jwm.setup_initial_windows("));
    }

    #[test]
    fn restart_bootstrap_retry_is_bounded_backed_off_and_keeps_handoff_immutable() {
        let handoff = ScratchpadRestartHandoff::capture(
            &Default::default(),
            &WMState::new(),
            &Default::default(),
            std::time::Instant::now(),
            8123,
        )
        .unwrap();
        let expected = handoff.clone();
        let attempts = Cell::new(0usize);
        let mut waits = Vec::new();

        let adopted = run_restart_bootstrap_with_retry(
            || {
                assert_eq!(handoff, expected, "retry must only borrow the handoff");
                let attempt = attempts.get() + 1;
                attempts.set(attempt);
                if attempt < 4 {
                    Err(Box::new(io::Error::other("injected bootstrap failure"))
                        as Box<dyn std::error::Error>)
                } else {
                    Ok(handoff.clone())
                }
            },
            |delay| waits.push(delay),
        )
        .unwrap();

        assert_eq!(adopted, expected);
        assert_eq!(attempts.get(), 4);
        assert_eq!(waits, RESTART_BOOTSTRAP_BACKOFFS[..3]);
    }

    #[test]
    fn restart_bootstrap_retry_returns_the_final_failure_without_a_hot_loop() {
        let attempts = Cell::new(0usize);
        let mut waits = Vec::new();
        let error = run_restart_bootstrap_with_retry::<()>(
            || {
                attempts.set(attempts.get() + 1);
                Err(Box::new(io::Error::other("persistent bootstrap failure")))
            },
            |delay| waits.push(delay),
        )
        .unwrap_err();

        assert_eq!(attempts.get(), RESTART_BOOTSTRAP_BACKOFFS.len() + 1);
        assert_eq!(waits, RESTART_BOOTSTRAP_BACKOFFS);
        assert!(error.to_string().contains("exhausted"));
        assert!(error.to_string().contains("persistent bootstrap failure"));
    }

    #[test]
    fn restart_bootstrap_scopes_marker_and_reloads_config_before_backend_creation() {
        let intent =
            function_body_after(APPLICATION_SRC, "fn take_restart_intent_from_environment");
        assert!(intent.contains("env::remove_var(RESTART_MARKER_ENV)"));

        let create = function_body_after(APPLICATION_SRC, "fn create_backend_for_startup");
        assert!(create.contains("restart_intent.then(RestartMarkerGuard::install)"));

        let bootstrap = function_body_after(APPLICATION_SRC, "fn bootstrap_jwm_instance");
        let reload = bootstrap
            .find("reload_validated_global_config(options.backend)?")
            .expect("restart bootstrap must load and store a validated disk config");
        let create = bootstrap
            .find("create_backend_for_startup(options.backend, restart_intent)?")
            .expect("restart bootstrap must construct its backend");
        assert!(reload < create, "validated CONFIG must be installed first");
    }

    #[test]
    fn benchmark_configuration_uses_narrow_capability() {
        let mut backend = BenchmarkSpy::default();
        configure_benchmark(&mut backend, Some(BenchmarkRequest::new(120, 30).unwrap()));

        assert_eq!(backend.started, Some((120, 30)));
        assert!(backend.auto_exit);
    }

    #[test]
    fn benchmark_configuration_revalidates_explicit_options() {
        let mut backend = BenchmarkSpy::default();
        configure_benchmark(
            &mut backend,
            Some(BenchmarkRequest {
                frames: u32::MAX,
                warmup: 0,
            }),
        );

        assert_eq!(backend.started, None);
        assert!(!backend.auto_exit);
    }

    #[test]
    fn benchmark_values_are_validated_without_environment_access() {
        assert_eq!(
            parse_benchmark(Some("120"), None),
            Ok(Some(BenchmarkRequest::new(120, 60).unwrap()))
        );
        assert!(parse_benchmark(Some("invalid"), Some("10")).is_err());
        assert!(parse_benchmark(Some("0"), Some("10")).is_err());
        assert!(parse_benchmark(None, Some("10")).is_err());
        assert_eq!(parse_benchmark(None, None), Ok(None));

        let too_many_frames = (BenchmarkRequest::MAX_FRAMES + 1).to_string();
        let too_much_warmup = (BenchmarkRequest::MAX_WARMUP_FRAMES + 1).to_string();
        assert!(parse_benchmark(Some(&too_many_frames), Some("0")).is_err());
        assert!(parse_benchmark(Some("1"), Some(&too_much_warmup)).is_err());
    }

    #[test]
    fn benchmark_request_accepts_limits_and_rejects_values_beyond_them() {
        assert_eq!(
            BenchmarkRequest::new(
                BenchmarkRequest::MAX_FRAMES,
                BenchmarkRequest::MAX_WARMUP_FRAMES,
            ),
            Ok(BenchmarkRequest {
                frames: BenchmarkRequest::MAX_FRAMES,
                warmup: BenchmarkRequest::MAX_WARMUP_FRAMES,
            })
        );

        let frames_error = BenchmarkRequest::new(BenchmarkRequest::MAX_FRAMES + 1, 0).unwrap_err();
        assert!(frames_error.contains("frame count"));
        assert!(frames_error.contains(&BenchmarkRequest::MAX_FRAMES.to_string()));

        let warmup_error =
            BenchmarkRequest::new(1, BenchmarkRequest::MAX_WARMUP_FRAMES + 1).unwrap_err();
        assert!(warmup_error.contains("warm-up"));
        assert!(warmup_error.contains(&BenchmarkRequest::MAX_WARMUP_FRAMES.to_string()));
    }

    #[test]
    fn restart_handoff_requires_marker_and_same_exec_pid() {
        let handoff = ScratchpadRestartHandoff::capture(
            &Default::default(),
            &WMState::new(),
            &Default::default(),
            std::time::Instant::now(),
            8123,
        )
        .unwrap();
        let payload = handoff.encode().unwrap();

        assert_eq!(
            decode_restart_handoff(false, Some(OsStr::new(&payload)), 8123).unwrap(),
            None,
            "ordinary startup must ignore inherited handoff residue"
        );
        assert_eq!(
            decode_restart_handoff(false, Some(OsStr::new("malformed residue")), 8123).unwrap(),
            None
        );
        assert!(decode_restart_handoff(true, Some(OsStr::new(&payload)), 8124).is_err());
        assert_eq!(
            decode_restart_handoff(true, Some(OsStr::new(&payload)), 8123)
                .unwrap()
                .unwrap(),
            handoff
        );
    }

    #[test]
    fn transient_child_handoff_requires_marker_and_same_parent_pid() {
        let payload = serde_json::json!({
            "version": 1,
            "parent_pid": 8123,
            "pids": [9001, 9002]
        })
        .to_string();

        assert!(
            decode_transient_child_restart_handoff(false, Some(OsStr::new(&payload)), 8123,)
                .unwrap()
                .is_none(),
            "ordinary startup must ignore inherited PID residue"
        );
        assert!(
            decode_transient_child_restart_handoff(true, Some(OsStr::new(&payload)), 8124).is_err()
        );
        assert!(
            decode_transient_child_restart_handoff(true, Some(OsStr::new(&payload)), 8123)
                .unwrap()
                .is_some()
        );
        assert!(
            decode_transient_child_restart_handoff(true, Some(OsStr::new("malformed")), 8123,)
                .is_err()
        );
    }

    #[test]
    fn restart_command_replaces_inherited_handoff_instead_of_forwarding_it() {
        let restart = RestartCommand {
            executable: OsString::from("jwm"),
            arguments: vec![OsString::from("--example")],
        };

        let without_payload = restart.command(None, None);
        let removed = without_payload
            .get_envs()
            .find(|(name, _)| *name == OsStr::new(SCRATCHPAD_HANDOFF_ENV));
        assert!(
            removed.is_some(),
            "the inherited payload needs an explicit tombstone"
        );
        assert_eq!(removed.unwrap().1, None);
        let legacy_removed = without_payload
            .get_envs()
            .find(|(name, _)| *name == OsStr::new(LEGACY_SCRATCHPAD_HANDOFF_ENV));
        assert!(legacy_removed.is_some());
        assert_eq!(legacy_removed.unwrap().1, None);
        let transient_removed = without_payload
            .get_envs()
            .find(|(name, _)| *name == OsStr::new(TRANSIENT_CHILD_HANDOFF_ENV));
        assert!(transient_removed.is_some());
        assert_eq!(transient_removed.unwrap().1, None);
        assert_eq!(
            without_payload
                .get_envs()
                .find(|(name, _)| *name == OsStr::new(RESTART_MARKER_ENV))
                .and_then(|(_, value)| value),
            Some(OsStr::new("1"))
        );

        let with_payload =
            restart.command(Some("bounded-payload"), Some("bounded-transient-payload"));
        assert_eq!(
            with_payload
                .get_envs()
                .find(|(name, _)| *name == OsStr::new(SCRATCHPAD_HANDOFF_ENV))
                .and_then(|(_, value)| value),
            Some(OsStr::new("bounded-payload"))
        );
        assert_eq!(
            with_payload
                .get_envs()
                .find(|(name, _)| *name == OsStr::new(TRANSIENT_CHILD_HANDOFF_ENV))
                .and_then(|(_, value)| value),
            Some(OsStr::new("bounded-transient-payload"))
        );
    }

    #[test]
    fn exec_failure_fallback_reinstalls_the_captured_transient_handoff() {
        let application = function_body_after(APPLICATION_SRC, "pub fn run_with_options");
        let capture = application
            .find("let (transient_child_handoff, transient_child_payload)")
            .expect("restart must retain the validated handoff beside its payload");
        let drop_old_jwm = application[capture..]
            .find("drop(jwm);")
            .map(|offset| capture + offset)
            .expect("old Child handles must be dropped before exec");
        let exec = application[drop_old_jwm..]
            .find("restart_command.exec(")
            .map(|offset| drop_old_jwm + offset)
            .expect("restart must attempt exec");
        let fallback = application[exec..]
            .find("pending_transient_child_handoff = Some(transient_child_handoff);")
            .map(|offset| exec + offset)
            .expect("exec failure must install the exact same handoff for in-process retry");

        assert!(capture < drop_old_jwm && drop_old_jwm < exec && exec < fallback);
    }

    #[test]
    fn restart_executor_runs_after_post_handoff_cleanup_failure() {
        let invoked = Cell::new(false);
        let cleanup_result: Result<(), Box<dyn std::error::Error>> =
            Err(Box::new(io::Error::other("injected X11 cleanup failure")));

        let result = execute_restart_after_cleanup(cleanup_result, || {
            invoked.set(true);
            "exec attempted"
        });

        assert!(invoked.get());
        assert_eq!(result, "exec attempted");
    }

    #[test]
    fn restart_preparation_uses_the_commit_safe_order() {
        let mut observed = Vec::new();
        run_restart_preparation_steps(|stage| {
            observed.push(stage);
            Ok::<(), ()>(())
        })
        .unwrap();

        assert_eq!(observed, RestartPreparationStage::ORDER);
    }

    #[test]
    fn restart_preparation_failure_returns_to_the_loop_before_later_stages() {
        for (failed_index, &failed_stage) in RestartPreparationStage::ORDER.iter().enumerate() {
            let mut observed = Vec::new();
            let result = run_restart_preparation_steps(|stage| {
                observed.push(stage);
                if stage == failed_stage {
                    Err("injected preflight failure")
                } else {
                    Ok(())
                }
            });

            assert_eq!(result, Err((failed_stage, "injected preflight failure")));
            assert_eq!(
                observed,
                RestartPreparationStage::ORDER[..=failed_index],
                "no post-failure stage may cross the cleanup commit point"
            );
        }
    }
}
