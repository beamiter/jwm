//! Tauri 2 host bridge for `xbar_core`.
//!
//! The bridge installs one checked action command, one frontend-ready replay
//! command, a complete revisioned state event, managed JWM transport recovery,
//! portable runtime cadence, window placement effects, and bounded worker
//! shutdown. Web-framework components and generated Tauri context stay in the
//! consumer application.

use std::{
    fmt,
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

use tauri::{
    AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, RunEvent, Runtime, State,
    WebviewWindow, Window, WindowEvent,
};
use xbar_core::glass::wallpaper::WallpaperFile;
use xbar_core::glass::{DEFAULT_BACKGROUND_OPACITY, GlassStrip, fallback_rgb};
use xbar_core::{
    ActionRequest, BarEffect, BarPlacement, FrontendSession, ModelConfig, ModelError,
    MonitorGeometry, PlatformEffectHandler, SessionOutput, TransportRecoveryConfig,
};
use xbar_linux_actions::{ProcessActionConfig, ProcessActionHandler};

/// Default event carrying one complete `FrontendEnvelope`.
pub const DEFAULT_STATE_EVENT: &str = "xbar-state";
/// Default Tauri label of the bar window.
pub const DEFAULT_WINDOW_LABEL: &str = "main";
/// Poll fallback used when no native transport wake is integrated by Tauri.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Element every frontend in this family uses as its bar root, and so the one
/// the frosted backdrop is installed behind.
pub const DEFAULT_GLASS_SELECTOR: &str = ".button-row";
/// `id` of the style element the bridge owns in the page.
const GLASS_STYLE_ID: &str = "xbar-glass";

fn is_valid_tauri_identifier(value: &str) -> bool {
    value
        .chars()
        .all(|character| character.is_alphanumeric() || matches!(character, '-' | '/' | ':' | '_'))
}

/// Configuration shared by all Tauri web-framework frontends.
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    pub shared_path: String,
    pub window_label: String,
    pub state_event: String,
    pub logical_width: f64,
    pub logical_height: f64,
    pub poll_interval: Duration,
    pub transport_retry_interval: Duration,
    pub model: ModelConfig,
    pub process_actions: ProcessActionConfig,
}

impl BridgeConfig {
    #[must_use]
    pub fn new(shared_path: impl Into<String>) -> Self {
        Self {
            shared_path: shared_path.into(),
            window_label: DEFAULT_WINDOW_LABEL.to_owned(),
            state_event: DEFAULT_STATE_EVENT.to_owned(),
            logical_width: 800.0,
            logical_height: 40.0,
            poll_interval: DEFAULT_POLL_INTERVAL,
            transport_retry_interval: xbar_core::DEFAULT_TRANSPORT_RETRY_INTERVAL,
            model: ModelConfig::default(),
            process_actions: ProcessActionConfig::default(),
        }
    }

    pub fn validate(&self) -> Result<(), BridgeConfigError> {
        if self.window_label.is_empty() {
            return Err(BridgeConfigError::EmptyWindowLabel);
        }
        if !is_valid_tauri_identifier(&self.window_label) {
            return Err(BridgeConfigError::InvalidWindowLabel);
        }
        if self.state_event.is_empty() {
            return Err(BridgeConfigError::EmptyStateEvent);
        }
        if !is_valid_tauri_identifier(&self.state_event) {
            return Err(BridgeConfigError::InvalidStateEvent);
        }
        if !self.logical_width.is_finite() || self.logical_width <= 0.0 {
            return Err(BridgeConfigError::InvalidLogicalWidth);
        }
        if !self.logical_height.is_finite() || self.logical_height <= 0.0 {
            return Err(BridgeConfigError::InvalidLogicalHeight);
        }
        if self.poll_interval.is_zero() {
            return Err(BridgeConfigError::ZeroPollInterval);
        }
        self.model.validate().map_err(BridgeConfigError::Model)?;
        if !self.shared_path.is_empty() {
            TransportRecoveryConfig::new(self.shared_path.clone(), self.transport_retry_interval)
                .map_err(|error| BridgeConfigError::Transport(error.to_string()))?;
        }
        Ok(())
    }
}

/// Invalid Tauri bridge configuration rejected before the app starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeConfigError {
    EmptyWindowLabel,
    InvalidWindowLabel,
    EmptyStateEvent,
    InvalidStateEvent,
    InvalidLogicalWidth,
    InvalidLogicalHeight,
    ZeroPollInterval,
    Model(ModelError),
    Transport(String),
}

impl fmt::Display for BridgeConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyWindowLabel => f.write_str("Tauri bar window label must not be empty"),
            Self::InvalidWindowLabel => f.write_str(
                "Tauri bar window label may contain only alphanumeric characters, '-', '/', ':', and '_'",
            ),
            Self::EmptyStateEvent => f.write_str("Tauri state event must not be empty"),
            Self::InvalidStateEvent => f.write_str(
                "Tauri state event may contain only alphanumeric characters, '-', '/', ':', and '_'",
            ),
            Self::InvalidLogicalWidth => {
                f.write_str("Tauri baseline width must be finite and greater than zero")
            }
            Self::InvalidLogicalHeight => {
                f.write_str("Tauri bar height must be finite and greater than zero")
            }
            Self::ZeroPollInterval => f.write_str("Tauri poll interval must be greater than zero"),
            Self::Model(error) => write!(f, "invalid bar model configuration: {error}"),
            Self::Transport(error) => {
                write!(f, "invalid transport recovery configuration: {error}")
            }
        }
    }
}

impl std::error::Error for BridgeConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Model(error) => Some(error),
            _ => None,
        }
    }
}

/// Install the common xbar lifecycle into an existing Tauri builder.
///
/// Consumers can attach their own plugins before or after this call, then run
/// the returned builder with their local `tauri::generate_context!()` value.
pub fn configure<R: Runtime>(
    builder: tauri::Builder<R>,
    config: BridgeConfig,
) -> Result<tauri::Builder<R>, BridgeConfigError> {
    config.validate()?;
    let setup_config = config.clone();

    Ok(builder
        .plugin(worker_lifecycle_plugin())
        .setup(move |app| setup(app.handle(), setup_config))
        .on_window_event(handle_window_event)
        .invoke_handler(tauri::generate_handler![dispatch_action, frontend_ready]))
}

struct BridgeState {
    inner: Arc<BridgeInner>,
}

struct BridgeInner {
    session: Mutex<FrontendSession>,
    baseline: WindowBaseline,
    config: BridgeConfig,
    process_actions: Mutex<ProcessActionHandler>,
    glass: Mutex<Option<GlassBackdrop>>,
}

/// The frosted backdrop, which a webview can only receive as an image file.
///
/// `backdrop-filter` blurs what is inside the page and never the desktop
/// behind the window, so the bridge frosts the wallpaper itself and hands the
/// page a `data:` URL to composite its own tint over.
struct GlassBackdrop {
    strip: GlassStrip<WallpaperFile>,
    tint: [u8; 3],
    generation: u64,
}

#[derive(Debug, Clone, Copy)]
struct WindowBaseline {
    initial_position: Option<PhysicalPosition<i32>>,
    logical_width: f64,
    logical_height: f64,
}

struct WorkerSignal {
    stopped: Mutex<bool>,
    wake: Condvar,
}

struct WorkerGuard {
    signal: Arc<WorkerSignal>,
    worker: Option<thread::JoinHandle<()>>,
}

impl WorkerGuard {
    fn request_stop(&self) {
        let mut stopped = match self.signal.stopped.lock() {
            Ok(stopped) => stopped,
            Err(error) => {
                log::error!("xbar worker stop lock poisoned during shutdown: {error}");
                error.into_inner()
            }
        };
        *stopped = true;
        self.signal.wake.notify_all();
    }
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        self.request_stop();
        let Some(worker) = self.worker.take() else {
            return;
        };
        if worker.thread().id() == thread::current().id() {
            log::debug!("xbar Tauri worker guard dropped on its worker thread; detaching");
            return;
        }
        if let Err(payload) = worker.join() {
            log::error!("xbar Tauri worker panicked during shutdown: {payload:?}");
        }
    }
}

fn worker_lifecycle_plugin<R: Runtime>() -> tauri::plugin::TauriPlugin<R> {
    // The worker owns an AppHandle, so its managed guard cannot be the first
    // owner to drop. Break that ownership cycle while the app is still alive.
    tauri::plugin::Builder::new("xbar-worker-lifecycle")
        .on_event(|app, event| {
            if matches!(event, RunEvent::Exit) {
                request_worker_stop(app);
            }
        })
        .build()
}

fn request_worker_stop<R: Runtime>(manager: &impl Manager<R>) {
    if let Some(worker) = manager.try_state::<WorkerGuard>() {
        worker.request_stop();
    }
}

fn setup<R: Runtime>(
    app: &AppHandle<R>,
    config: BridgeConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    if app.try_state::<BridgeState>().is_some() || app.try_state::<WorkerGuard>().is_some() {
        return Err("xbar Tauri bridge is already configured".into());
    }
    let window = app
        .get_webview_window(&config.window_label)
        .ok_or_else(|| format!("Tauri bar window {:?} is unavailable", config.window_label))?;
    let baseline = WindowBaseline {
        initial_position: window.outer_position().ok(),
        logical_width: config.logical_width,
        logical_height: config.logical_height,
    };
    let scale_factor = window.scale_factor()?;
    apply_window_baseline(&window, baseline, scale_factor)?;

    let runtime = if config.shared_path.is_empty() {
        xbar_core::BarRuntime::new(config.model.clone())?
    } else {
        let recovery = TransportRecoveryConfig::new(
            config.shared_path.clone(),
            config.transport_retry_interval,
        )?;
        xbar_core::BarRuntime::with_managed_transport(config.model.clone(), recovery)?
    };
    let inner = Arc::new(BridgeInner {
        session: Mutex::new(FrontendSession::new(runtime)),
        baseline,
        process_actions: Mutex::new(ProcessActionHandler::new(config.process_actions.clone())),
        glass: Mutex::new(glass_backdrop()),
        config,
    });
    let guard = spawn_worker(app.clone(), Arc::clone(&inner))?;
    if !app.manage(BridgeState { inner }) {
        return Err("failed to install xbar Tauri bridge state".into());
    }
    if !app.manage(guard) {
        return Err("failed to install xbar Tauri worker guard".into());
    }
    Ok(())
}

fn spawn_worker<R: Runtime>(
    app: AppHandle<R>,
    inner: Arc<BridgeInner>,
) -> Result<WorkerGuard, std::io::Error> {
    let signal = Arc::new(WorkerSignal {
        stopped: Mutex::new(false),
        wake: Condvar::new(),
    });
    let worker_signal = Arc::clone(&signal);
    let worker = thread::Builder::new()
        .name("xbar-tauri-runtime".to_owned())
        .spawn(move || worker_loop(&app, &inner, &worker_signal))?;
    Ok(WorkerGuard {
        signal,
        worker: Some(worker),
    })
}

fn worker_loop<R: Runtime>(app: &AppHandle<R>, inner: &BridgeInner, signal: &WorkerSignal) {
    loop {
        let (deadline, poll_interval) = {
            let mut session = match inner.session.lock() {
                Ok(session) => session,
                Err(error) => {
                    log::error!("xbar frontend session lock poisoned: {error}");
                    return;
                }
            };
            let output = session.service();
            let deadline = session.next_service_deadline(Instant::now());
            if let Err(error) = deliver_output(app, inner, output, false) {
                log::error!("xbar background service failed: {error}");
            }
            let geometry = release_session_for_glass(session);
            // Cheap unless the wallpaper file or the bar geometry changed.
            if let Err(error) =
                refresh_glass(app, &inner.glass, &inner.config.window_label, geometry)
            {
                log::warn!("frosted backdrop unavailable: {error}");
            }
            (deadline, inner.config.poll_interval)
        };

        let now = Instant::now();
        let until_deadline = deadline.saturating_duration_since(now);
        let wait = poll_interval.min(until_deadline);
        let stopped = match signal.stopped.lock() {
            Ok(stopped) => stopped,
            Err(error) => {
                log::error!("xbar worker stop lock poisoned: {error}");
                return;
            }
        };
        if *stopped {
            return;
        }
        match signal.wake.wait_timeout(stopped, wait) {
            Ok((stopped, _)) if *stopped => return,
            Ok(_) => {}
            Err(error) => {
                log::error!("xbar worker stop wait poisoned: {error}");
                return;
            }
        }
    }
}

/// Capture the only session value glass refresh needs and release the worker's
/// session guard before the refresh phase tries to acquire any other locks.
fn release_session_for_glass(
    session: std::sync::MutexGuard<'_, FrontendSession>,
) -> Option<MonitorGeometry> {
    let geometry = session.runtime().view().geometry;
    drop(session);
    geometry
}

#[tauri::command]
fn dispatch_action<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, BridgeState>,
    request: ActionRequest,
) -> Result<(), String> {
    let mut session = state
        .inner
        .session
        .lock()
        .map_err(|error| format!("xbar frontend session lock poisoned: {error}"))?;
    let output = session
        .dispatch(request)
        .map_err(|error| error.to_string())?;
    deliver_output(&app, &state.inner, output, true)
}

#[tauri::command]
fn frontend_ready<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, BridgeState>,
) -> Result<(), String> {
    let mut session = state
        .inner
        .session
        .lock()
        .map_err(|error| format!("xbar frontend session lock poisoned: {error}"))?;
    if let Some(envelope) = session.replay() {
        return app
            .emit(&state.inner.config.state_event, envelope)
            .map_err(|error| format!("failed to replay xbar state: {error}"));
    }

    // A very fast webview can announce readiness before the worker's first
    // service turn. Initialize and deliver synchronously instead of exposing
    // a startup race to every frontend.
    let output = session.service();
    deliver_output(&app, &state.inner, output, true)
}

fn deliver_output<R: Runtime>(
    app: &AppHandle<R>,
    inner: &BridgeInner,
    mut output: SessionOutput,
    fail_on_issue: bool,
) -> Result<(), String> {
    let mut failures = Vec::new();
    for issue in &output.frame.update.issues {
        log::warn!("{issue}");
        if fail_on_issue {
            failures.push(issue.to_string());
        }
    }

    for effect in std::mem::take(&mut output.frame.update.platform_effects) {
        if let Err(error) = execute_effect(app, inner, effect) {
            log::error!("{error}");
            failures.push(error);
        }
    }

    if let Some(envelope) = output.envelope
        && let Err(error) = app.emit(&inner.config.state_event, envelope)
    {
        failures.push(format!("failed to emit xbar state: {error}"));
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

fn execute_effect<R: Runtime>(
    app: &AppHandle<R>,
    inner: &BridgeInner,
    effect: BarEffect,
) -> Result<(), String> {
    match effect {
        BarEffect::Screenshot | BarEffect::OpenAudioControl => inner
            .process_actions
            .lock()
            .map_err(|error| format!("process action lock poisoned: {error}"))?
            .handle(effect)
            .map_err(|error| error.to_string()),
        BarEffect::ApplyMonitorGeometry(geometry) => {
            let window = main_window(app, &inner.config.window_label)?;
            let scale_factor = window
                .scale_factor()
                .map_err(|error| format!("failed to query bar scale factor: {error}"))?;
            apply_monitor_geometry(&window, geometry, inner.config.logical_height, scale_factor)
        }
        BarEffect::ClearMonitorGeometry => {
            let window = main_window(app, &inner.config.window_label)?;
            let scale_factor = window
                .scale_factor()
                .map_err(|error| format!("failed to query bar scale factor: {error}"))?;
            apply_window_baseline(&window, inner.baseline, scale_factor)
        }
        _ => Err(format!("Tauri bridge cannot execute {effect:?}")),
    }
}

fn handle_window_event<R: Runtime>(window: &Window<R>, event: &WindowEvent) {
    let Some(state) = window.try_state::<BridgeState>() else {
        return;
    };
    if window.label() != state.inner.config.window_label {
        return;
    }
    if matches!(event, WindowEvent::Destroyed) {
        request_worker_stop(window);
        return;
    }
    let WindowEvent::ScaleFactorChanged { scale_factor, .. } = event else {
        return;
    };
    let geometry = match state.inner.session.lock() {
        Ok(session) => session.runtime().snapshot().geometry,
        Err(error) => {
            log::error!("xbar frontend session lock poisoned during scale change: {error}");
            return;
        }
    };
    let result = geometry.map_or_else(
        || apply_window_baseline(window, state.inner.baseline, *scale_factor),
        |geometry| {
            apply_monitor_geometry(
                window,
                geometry,
                state.inner.config.logical_height,
                *scale_factor,
            )
        },
    );
    if let Err(error) = result {
        log::error!("failed to apply xbar scale-factor update: {error}");
    }
}

/// The wallpaper source this bar's configuration asks for, if any.
///
/// Only the `[glass]` section of the shared bar config applies here; the rest
/// of a Tauri bar's appearance belongs to its own stylesheet.
fn glass_backdrop() -> Option<GlassBackdrop> {
    let config = xbar_core::config::BarConfig::load_default().unwrap_or_else(|error| {
        log::warn!("falling back to the default bar config: {error}");
        xbar_core::config::BarConfig::default()
    });
    let tint = fallback_rgb(config.theme);
    // A provisional screen size; `refresh_glass` corrects it from the window
    // manager's monitor geometry as soon as one arrives.
    let strip = config.glass.file_strip(1920, 1080, tint)?;
    Some(GlassBackdrop {
        strip,
        tint,
        generation: 0,
    })
}

/// Re-frost the wallpaper under the bar and install it in the page when it
/// changed.
///
/// The stylesheet is injected rather than routed through the frontend so that
/// all six web frameworks share one implementation; the page keeps ownership
/// of everything else it draws.
fn refresh_glass<R: Runtime>(
    app: &AppHandle<R>,
    glass_state: &Mutex<Option<GlassBackdrop>>,
    window_label: &str,
    geometry: Option<MonitorGeometry>,
) -> Result<(), String> {
    let mut glass = glass_state
        .lock()
        .map_err(|error| format!("glass backdrop lock poisoned: {error}"))?;
    let Some(glass) = glass.as_mut() else {
        return Ok(());
    };
    let window = main_window(app, window_label)?;
    if let Some(geometry) = geometry {
        glass
            .strip
            .source_mut()
            .set_screen(geometry.width.max(1), geometry.height.max(1));
    }
    let (x, y) = geometry.map_or((0, 0), |geometry| (geometry.x, geometry.y));
    let size = window
        .inner_size()
        .map_err(|error| format!("failed to query bar size: {error}"))?;

    let Some((generation, image)) = glass
        .strip
        .ensure(x, y, size.width.max(1), size.height.max(1))
    else {
        return Ok(());
    };
    if generation == glass.generation {
        return Ok(());
    }
    let css = image
        .to_css_backdrop(
            DEFAULT_GLASS_SELECTOR,
            glass.tint,
            DEFAULT_BACKGROUND_OPACITY as f32,
        )
        .map_err(|error| format!("failed to encode the frosted backdrop: {error}"))?;
    window
        .eval(install_style_script(&css))
        .map_err(|error| format!("failed to install the frosted backdrop: {error}"))?;
    glass.generation = generation;
    Ok(())
}

/// Script that creates the bridge's own style element once and then only ever
/// replaces its text, so repeated updates cannot accumulate elements.
fn install_style_script(css: &str) -> String {
    let css = json_string(css);
    format!(
        "(function(){{var e=document.getElementById({id});         if(!e){{e=document.createElement('style');e.id={id};         document.head.appendChild(e);}}e.textContent={css};}})()",
        id = json_string(GLASS_STYLE_ID),
    )
}

/// Quote `text` as a JavaScript string literal.
///
/// A `data:` URL is base64 and a stylesheet is ASCII, but neither is trusted
/// to stay that way: anything that could end the literal early is escaped.
fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // `</script>` and U+2028/9 end a literal in some HTML contexts.
            '<' => out.push_str("\\u003c"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            character if (character as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => out.push(character),
        }
    }
    out.push('"');
    out
}

fn main_window<R: Runtime>(app: &AppHandle<R>, label: &str) -> Result<WebviewWindow<R>, String> {
    app.get_webview_window(label)
        .ok_or_else(|| format!("Tauri bar window {label:?} is unavailable"))
}

trait BarWindow {
    fn set_bar_position(&self, position: PhysicalPosition<i32>) -> tauri::Result<()>;
    fn set_bar_size(&self, size: PhysicalSize<u32>) -> tauri::Result<()>;
}

impl<R: Runtime> BarWindow for Window<R> {
    fn set_bar_position(&self, position: PhysicalPosition<i32>) -> tauri::Result<()> {
        self.set_position(position)
    }

    fn set_bar_size(&self, size: PhysicalSize<u32>) -> tauri::Result<()> {
        self.set_size(size)
    }
}

impl<R: Runtime> BarWindow for WebviewWindow<R> {
    fn set_bar_position(&self, position: PhysicalPosition<i32>) -> tauri::Result<()> {
        self.set_position(position)
    }

    fn set_bar_size(&self, size: PhysicalSize<u32>) -> tauri::Result<()> {
        self.set_size(size)
    }
}

fn apply_monitor_geometry(
    window: &impl BarWindow,
    geometry: MonitorGeometry,
    logical_height: f64,
    scale_factor: f64,
) -> Result<(), String> {
    let placement = BarPlacement::top(geometry, logical_height, scale_factor)
        .map_err(|error| format!("invalid bar placement: {error}"))?;
    window
        .set_bar_position(PhysicalPosition::new(placement.x, placement.y))
        .map_err(|error| format!("failed to position bar window: {error}"))?;
    window
        .set_bar_size(PhysicalSize::new(placement.width, placement.height))
        .map_err(|error| format!("failed to resize bar window: {error}"))
}

fn apply_window_baseline(
    window: &impl BarWindow,
    baseline: WindowBaseline,
    scale_factor: f64,
) -> Result<(), String> {
    if let Some(position) = baseline.initial_position {
        window
            .set_bar_position(position)
            .map_err(|error| format!("failed to restore bar position: {error}"))?;
    }
    let size = baseline_physical_size(baseline, scale_factor)?;
    window
        .set_bar_size(size)
        .map_err(|error| format!("failed to restore bar size: {error}"))
}

fn baseline_physical_size(
    baseline: WindowBaseline,
    scale_factor: f64,
) -> Result<PhysicalSize<u32>, String> {
    if !scale_factor.is_finite() || scale_factor <= 0.0 {
        return Err("window scale factor must be finite and greater than zero".to_owned());
    }
    let physical = |logical: f64| {
        (logical * scale_factor)
            .round()
            .clamp(1.0, f64::from(u32::MAX)) as u32
    };
    Ok(PhysicalSize::new(
        physical(baseline.logical_width),
        physical(baseline.logical_height),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid_with_or_without_transport() {
        BridgeConfig::new("").validate().unwrap();
        BridgeConfig::new("/run/user/1000/jwm").validate().unwrap();
    }

    #[test]
    fn config_rejects_invalid_host_boundaries_before_startup() {
        let mut config = BridgeConfig::new("");
        config.window_label.clear();
        assert_eq!(config.validate(), Err(BridgeConfigError::EmptyWindowLabel));

        let mut config = BridgeConfig::new("");
        config.state_event.clear();
        assert_eq!(config.validate(), Err(BridgeConfigError::EmptyStateEvent));

        let mut config = BridgeConfig::new("");
        config.window_label = "main.window".to_owned();
        assert_eq!(
            config.validate(),
            Err(BridgeConfigError::InvalidWindowLabel)
        );

        let mut config = BridgeConfig::new("");
        config.state_event = "xbar state".to_owned();
        assert_eq!(config.validate(), Err(BridgeConfigError::InvalidStateEvent));

        let mut config = BridgeConfig::new("");
        config.window_label = "主窗口/main:1".to_owned();
        config.state_event = "状态/xbar_state-1".to_owned();
        config.validate().unwrap();

        let mut config = BridgeConfig::new("");
        config.logical_height = f64::NAN;
        assert_eq!(
            config.validate(),
            Err(BridgeConfigError::InvalidLogicalHeight)
        );

        let mut config = BridgeConfig::new("");
        config.poll_interval = Duration::ZERO;
        assert_eq!(config.validate(), Err(BridgeConfigError::ZeroPollInterval));
    }

    #[test]
    fn baseline_size_uses_physical_scale_and_validates_it() {
        let baseline = WindowBaseline {
            initial_position: None,
            logical_width: 800.0,
            logical_height: 40.0,
        };
        assert_eq!(
            baseline_physical_size(baseline, 1.25).unwrap(),
            PhysicalSize::new(1000, 50)
        );
        assert!(baseline_physical_size(baseline, 0.0).is_err());
    }

    #[test]
    fn glass_refresh_input_releases_the_worker_session_guard() {
        let config = ModelConfig {
            resolve_client_icons: false,
            ..ModelConfig::default()
        };
        let session = Mutex::new(FrontendSession::new(
            xbar_core::BarRuntime::new(config).unwrap(),
        ));

        let guard = session.lock().unwrap();
        assert_eq!(release_session_for_glass(guard), None);
        assert!(session.try_lock().is_ok());
    }

    #[test]
    fn worker_stop_wakes_the_waiter_before_drop_joins_it() {
        let signal = Arc::new(WorkerSignal {
            stopped: Mutex::new(false),
            wake: Condvar::new(),
        });
        let worker_signal = Arc::clone(&signal);
        let (waiting_tx, waiting_rx) = std::sync::mpsc::sync_channel(0);
        let worker = thread::spawn(move || {
            let stopped = worker_signal.stopped.lock().unwrap();
            waiting_tx.send(()).unwrap();
            let stopped = worker_signal
                .wake
                .wait_while(stopped, |stopped| !*stopped)
                .unwrap();
            assert!(*stopped);
        });
        let guard = WorkerGuard {
            signal,
            worker: Some(worker),
        };

        waiting_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        guard.request_stop();
        drop(guard);
    }

    #[test]
    fn worker_guard_does_not_join_its_own_thread() {
        let signal = Arc::new(WorkerSignal {
            stopped: Mutex::new(false),
            wake: Condvar::new(),
        });
        let (guard_tx, guard_rx) = std::sync::mpsc::sync_channel(0);
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(0);
        let worker = thread::spawn(move || {
            let guard: WorkerGuard = guard_rx.recv().unwrap();
            drop(guard);
            done_tx.send(()).unwrap();
        });
        let guard = WorkerGuard {
            signal,
            worker: Some(worker),
        };

        guard_tx.send(guard).unwrap();
        done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn the_style_script_installs_one_element_and_escapes_its_payload() {
        let script = install_style_script(".button-row { color: red; }");

        // The element is looked up before it is created, so repeated updates
        // replace text rather than pile up style elements.
        let lookup = script.find("getElementById").expect("lookup");
        let create = script.find("createElement").expect("create");
        assert!(lookup < create);
        assert!(script.contains("e.textContent="));

        // Anything that could end the JavaScript string literal early is
        // escaped, including the `<` of a stray `</script>`.
        let hostile = install_style_script("a\"b\\c\n<d");
        assert!(hostile.contains("\\\"b"), "{hostile}");
        assert!(hostile.contains("\\\\c"), "{hostile}");
        assert!(hostile.contains("\\n"), "{hostile}");
        assert!(hostile.contains("\\u003cd"), "{hostile}");
        assert!(!hostile.contains("</"), "{hostile}");
    }

    #[test]
    fn bridge_configures_the_real_wry_builder() {
        let builder = tauri::Builder::<tauri::Wry>::default();
        let configured = configure(builder, BridgeConfig::new("")).unwrap();
        drop(configured);
    }
}
