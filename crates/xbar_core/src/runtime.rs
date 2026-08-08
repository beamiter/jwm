//! Feature-aware orchestration for the backend-neutral [`BarModel`].
//!
//! `BarRuntime` owns provider and transport adapters only when their Cargo
//! features are enabled.  The model remains the single source of semantic
//! state; adapters merely translate snapshots and execute effects.  Effects
//! that require a window/event-loop integration are returned to the frontend.

use std::fmt;
use std::time::{Duration, Instant};

use crate::{
    BarEffect, BarEvent, BarModel, BarSnapshot, BarView, DirtyBits, ModelConfig, ModelError,
    ModelUpdate, PercentError, UserAction, WmCommand,
};

#[cfg(feature = "provider-battery-sysfs")]
use crate::BatteryState;
#[cfg(feature = "provider-brightnessctl")]
use crate::BrightnessState;
#[cfg(feature = "provider-alsa")]
use crate::{AudioDeviceInfo, AudioState, Percent};
#[cfg(feature = "transport-shared")]
use crate::{
    DockItemGeometry, MAX_MODEL_MINIMIZED_WINDOWS, SendOutcome, SharedTransport, WindowToken,
};
#[cfg(feature = "provider-system")]
use crate::{SystemDetails, SystemLoadAverage, SystemState};

/// Adapter category associated with a runtime issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeAdapter {
    Transport,
    Audio,
    System,
    Brightness,
    Battery,
    Network,
    Clock,
}

impl fmt::Display for RuntimeAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Transport => "window-manager transport",
            Self::Audio => "audio provider",
            Self::System => "system provider",
            Self::Brightness => "brightness provider",
            Self::Battery => "battery provider",
            Self::Network => "network provider",
            Self::Clock => "clock provider",
        })
    }
}

/// A recoverable problem observed while reducing an event or executing an
/// adapter effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeIssue {
    Model(ModelError),
    /// A WM command was rejected because no authoritative WM snapshot has
    /// been reduced yet. Transport presence alone is not proof that the
    /// current monitor projection is valid after startup or reconnect.
    WindowManagerUnavailable {
        command: WmCommand,
    },
    QueueFull {
        command: WmCommand,
    },
    AdapterFailed {
        adapter: RuntimeAdapter,
        operation: &'static str,
        message: String,
    },
    InvalidProviderPercent {
        adapter: RuntimeAdapter,
        field: &'static str,
        error: PercentError,
    },
}

impl RuntimeIssue {
    /// Return the adapter responsible for this issue, when the issue came
    /// from an adapter rather than model validation or queue backpressure.
    #[must_use]
    pub const fn adapter(&self) -> Option<RuntimeAdapter> {
        match self {
            Self::AdapterFailed { adapter, .. } | Self::InvalidProviderPercent { adapter, .. } => {
                Some(*adapter)
            }
            Self::Model(_) | Self::WindowManagerUnavailable { .. } | Self::QueueFull { .. } => None,
        }
    }
}

impl fmt::Display for RuntimeIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Model(error) => write!(f, "model rejected event: {error}"),
            Self::WindowManagerUnavailable { command } => write!(
                f,
                "window-manager state is unavailable; command rejected: {command:?}"
            ),
            Self::QueueFull { command } => {
                write!(f, "window-manager command queue is full: {command:?}")
            }
            Self::AdapterFailed {
                adapter,
                operation,
                message,
            } => write!(f, "{adapter} {operation} failed: {message}"),
            Self::InvalidProviderPercent {
                adapter,
                field,
                error,
            } => write!(f, "{adapter} returned invalid {field}: {error}"),
        }
    }
}

impl std::error::Error for RuntimeIssue {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Model(error) => Some(error),
            Self::InvalidProviderPercent { error, .. } => Some(error),
            Self::WindowManagerUnavailable { .. }
            | Self::QueueFull { .. }
            | Self::AdapterFailed { .. } => None,
        }
    }
}

/// Invalid lifecycle configuration supplied to [`RuntimeSchedule`] or the
/// managed shared-transport recovery policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeConfigError {
    EmptyTransportPath,
    ZeroInterval { field: &'static str },
    IntervalTooLarge { field: &'static str },
}

impl fmt::Display for RuntimeConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyTransportPath => f.write_str("managed transport path must not be empty"),
            Self::ZeroInterval { field } => write!(f, "{field} must be greater than zero"),
            Self::IntervalTooLarge { field } => {
                write!(f, "{field} is too large for the monotonic clock")
            }
        }
    }
}

impl std::error::Error for RuntimeConfigError {}

/// Recommended cadence for clock and provider refreshes.
pub const DEFAULT_RUNTIME_TICK_INTERVAL: Duration = Duration::from_secs(1);

/// Recommended bounded retry interval for the JWM shared transport.
#[cfg(feature = "transport-shared")]
pub const DEFAULT_TRANSPORT_RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// Retry cadence for restore commands retained after bounded transport
/// backpressure. Restore is user-critical and must not disappear merely
/// because geometry/preview traffic briefly filled the shared command ring.
#[cfg(feature = "transport-shared")]
pub const CRITICAL_RESTORE_RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// Portable scheduling state for event loops that poll frequently but only
/// want to refresh providers at a lower cadence.
///
/// Every service call polls the WM transport. The clock and providers are
/// refreshed immediately on the first call and then at `tick_interval`.
/// Deadlines are monotonic and missed intervals are coalesced into one tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSchedule {
    tick_interval: Duration,
    next_tick: Option<Instant>,
}

impl Default for RuntimeSchedule {
    fn default() -> Self {
        Self {
            tick_interval: DEFAULT_RUNTIME_TICK_INTERVAL,
            next_tick: None,
        }
    }
}

impl RuntimeSchedule {
    pub fn new(tick_interval: Duration) -> Result<Self, RuntimeConfigError> {
        validate_runtime_interval("runtime tick interval", tick_interval)?;
        Ok(Self {
            tick_interval,
            next_tick: None,
        })
    }

    #[must_use]
    pub const fn tick_interval(&self) -> Duration {
        self.tick_interval
    }

    #[must_use]
    pub const fn next_tick(&self) -> Option<Instant> {
        self.next_tick
    }

    /// Return the earliest time at which a normal service turn is required.
    ///
    /// Before the first service turn this returns `now`. With managed shared
    /// transport recovery enabled, a disconnected transport retry can make
    /// the deadline earlier than the next provider tick. Event loops may
    /// still service sooner in response to native input or transport wakes.
    #[must_use]
    pub fn next_service_deadline(&self, runtime: &BarRuntime, now: Instant) -> Instant {
        let deadline = self.next_tick.unwrap_or(now);

        #[cfg(feature = "transport-shared")]
        let mut deadline = deadline;

        #[cfg(feature = "transport-shared")]
        if let Some(retry) = runtime.pending_restore_retry_at {
            deadline = deadline.min(retry);
        }

        #[cfg(feature = "transport-shared")]
        if runtime.transport.is_none()
            && let Some(recovery) = runtime.transport_recovery.as_ref()
        {
            deadline = deadline.min(recovery.next_attempt.unwrap_or(now));
        }

        #[cfg(not(feature = "transport-shared"))]
        let _ = runtime;

        deadline
    }

    /// Make the next service call refresh providers regardless of its time.
    pub fn reset(&mut self) {
        self.next_tick = None;
    }

    /// Poll transport now and refresh providers when the cadence is due.
    pub fn service(&mut self, runtime: &mut BarRuntime) -> RuntimeUpdate {
        self.service_at(runtime, Instant::now())
    }

    /// Service the runtime and atomically capture the resulting owned frame.
    ///
    /// This is the preferred toolkit/web entry point: the returned frame
    /// contains one coherent snapshot, revision, accumulated change set, and
    /// the issues/platform work produced by this service turn.
    pub fn service_frame(&mut self, runtime: &mut BarRuntime) -> RuntimeFrame {
        self.service_frame_at(runtime, Instant::now())
    }

    /// Deterministic variant of [`Self::service`] for event loops that already
    /// sampled their monotonic clock and for tests.
    pub fn service_at(&mut self, runtime: &mut BarRuntime, now: Instant) -> RuntimeUpdate {
        let mut update = runtime.poll_transport_at(now);
        if self.next_tick.is_none_or(|deadline| now >= deadline) {
            update.merge(runtime.tick());
            self.next_tick = Some(runtime_deadline(now, self.tick_interval));
        }
        update
    }

    /// Deterministic variant of [`Self::service_frame`].
    pub fn service_frame_at(&mut self, runtime: &mut BarRuntime, now: Instant) -> RuntimeFrame {
        let update = self.service_at(runtime, now);
        runtime.frame(update)
    }
}

#[cfg(feature = "transport-shared")]
/// Configuration for core-owned opening and bounded retry of a JWM shared
/// transport. A configured runtime remains semantically unavailable until a
/// fresh snapshot arrives from a successfully opened transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportRecoveryConfig {
    path: String,
    retry_interval: Duration,
}

#[cfg(feature = "transport-shared")]
/// Observable lifecycle state of the optional shared transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportStatus {
    /// No transport handle or automatic recovery policy is installed.
    Disabled,
    /// Automatic recovery is configured and waiting for/opening the path.
    Recovering,
    /// A handle is open but no authoritative WM snapshot has arrived yet.
    Connected,
    /// A handle is open and its latest WM projection is authoritative.
    Ready,
}

#[cfg(feature = "transport-shared")]
impl TransportRecoveryConfig {
    pub fn with_default_retry(path: impl Into<String>) -> Result<Self, RuntimeConfigError> {
        Self::new(path, DEFAULT_TRANSPORT_RETRY_INTERVAL)
    }

    pub fn new(
        path: impl Into<String>,
        retry_interval: Duration,
    ) -> Result<Self, RuntimeConfigError> {
        let path = path.into();
        if path.is_empty() {
            return Err(RuntimeConfigError::EmptyTransportPath);
        }
        validate_runtime_interval("transport retry interval", retry_interval)?;
        Ok(Self {
            path,
            retry_interval,
        })
    }

    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    #[must_use]
    pub const fn retry_interval(&self) -> Duration {
        self.retry_interval
    }
}

#[cfg(feature = "transport-shared")]
#[derive(Debug)]
struct TransportRecoveryState {
    config: TransportRecoveryConfig,
    /// `None` means the next disconnected poll may attempt immediately.
    next_attempt: Option<Instant>,
}

#[cfg(feature = "transport-shared")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingRestore {
    window: WindowToken,
    wm_session_id: u64,
    minimized_generation: u64,
    geometry: DockItemGeometry,
}

#[cfg(feature = "transport-shared")]
impl PendingRestore {
    fn from_command(command: WmCommand) -> Option<Self> {
        match command {
            WmCommand::RestoreWindow {
                window,
                wm_session_id,
                minimized_generation,
                geometry,
                ..
            } => Some(Self {
                window,
                wm_session_id,
                minimized_generation,
                geometry,
            }),
            _ => None,
        }
    }

    fn matches(self, other: Self) -> bool {
        self.window == other.window
            && self.wm_session_id == other.wm_session_id
            && self.minimized_generation == other.minimized_generation
    }
}

/// Result of one runtime operation.
///
/// `changes` is suitable for frame scheduling. `platform_effects` contains
/// work deliberately not performed by this runtime, such as window placement,
/// screenshots, process launching, or an adapter disabled at compile time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeUpdate {
    pub changes: DirtyBits,
    pub platform_effects: Vec<BarEffect>,
    pub issues: Vec<RuntimeIssue>,
}

/// One coherent owned result for toolkit stores, cross-thread handoff, and
/// frontend wire bridges.
///
/// `update.changes` includes all model changes accumulated since the previous
/// frame capture, even if an intermediate [`RuntimeUpdate`] was discarded.
/// Platform effects and issues belong to the operation that produced this
/// frame and are never replayed implicitly.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeFrame {
    pub revision: u64,
    pub snapshot: BarSnapshot,
    pub update: RuntimeUpdate,
}

/// Host implementation for platform work deliberately left by the runtime.
/// A closure with the same signature implements this trait automatically.
pub trait PlatformEffectHandler {
    type Error;

    fn handle(&mut self, effect: BarEffect) -> Result<(), Self::Error>;
}

impl<F, E> PlatformEffectHandler for F
where
    F: FnMut(BarEffect) -> Result<(), E>,
{
    type Error = E;

    fn handle(&mut self, effect: BarEffect) -> Result<(), Self::Error> {
        self(effect)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct PlatformEffectFailure<E> {
    pub effect: BarEffect,
    pub error: E,
}

/// Outcome of draining a [`RuntimeUpdate`]'s platform-effect queue.
#[derive(Debug, PartialEq, Eq)]
pub struct PlatformEffectReport<E> {
    pub handled: usize,
    pub failures: Vec<PlatformEffectFailure<E>>,
}

impl<E> Default for PlatformEffectReport<E> {
    fn default() -> Self {
        Self {
            handled: 0,
            failures: Vec::new(),
        }
    }
}

impl<E> PlatformEffectReport<E> {
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.failures.is_empty()
    }

    pub fn failed_effects(&self) -> impl Iterator<Item = BarEffect> + '_ {
        self.failures.iter().map(|failure| failure.effect)
    }
}

impl RuntimeFrame {
    #[must_use]
    pub const fn changes(&self) -> DirtyBits {
        self.update.changes
    }

    #[must_use]
    pub fn needs_redraw(&self) -> bool {
        self.update.needs_redraw()
    }

    #[must_use]
    pub fn has_platform_work(&self) -> bool {
        self.update.has_platform_work()
    }

    #[must_use]
    pub fn has_issues(&self) -> bool {
        self.update.has_issues()
    }

    #[must_use]
    pub fn into_parts(self) -> (u64, BarSnapshot, RuntimeUpdate) {
        (self.revision, self.snapshot, self.update)
    }
}

impl RuntimeUpdate {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty() && self.platform_effects.is_empty() && self.issues.is_empty()
    }

    #[must_use]
    pub const fn needs_redraw(&self) -> bool {
        !self.changes.is_empty()
    }

    #[must_use]
    pub fn has_platform_work(&self) -> bool {
        !self.platform_effects.is_empty()
    }

    #[must_use]
    pub fn has_issues(&self) -> bool {
        !self.issues.is_empty()
    }

    /// Drain and route every pending platform effect through one host policy.
    /// Failures retain both the effect and the host error so callers can log,
    /// retry, or convert them to application-specific diagnostics.
    pub fn handle_platform_effects<H>(&mut self, handler: &mut H) -> PlatformEffectReport<H::Error>
    where
        H: PlatformEffectHandler,
    {
        let effects = std::mem::take(&mut self.platform_effects);
        let mut report = PlatformEffectReport::default();
        for effect in effects {
            match handler.handle(effect) {
                Ok(()) => report.handled += 1,
                Err(error) => report
                    .failures
                    .push(PlatformEffectFailure { effect, error }),
            }
        }
        report
    }

    /// Whether any issue was produced by `adapter`.
    #[must_use]
    pub fn has_adapter_issue(&self, adapter: RuntimeAdapter) -> bool {
        self.issues
            .iter()
            .any(|issue| issue.adapter() == Some(adapter))
    }

    /// Whether the shared transport failed an open, read, or write operation.
    /// An unavailable-yet-authoritative WM projection and a full command queue
    /// are deliberately not classified as broken connections.
    #[must_use]
    pub fn transport_failed(&self) -> bool {
        self.issues.iter().any(|issue| {
            matches!(
                issue,
                RuntimeIssue::AdapterFailed {
                    adapter: RuntimeAdapter::Transport,
                    ..
                }
            )
        })
    }

    pub fn merge(&mut self, mut other: Self) {
        self.changes |= other.changes;
        self.platform_effects.append(&mut other.platform_effects);
        self.issues.append(&mut other.issues);
    }

    fn platform(effect: BarEffect) -> Self {
        Self {
            platform_effects: vec![effect],
            ..Self::default()
        }
    }

    fn issue(issue: RuntimeIssue) -> Self {
        Self {
            issues: vec![issue],
            ..Self::default()
        }
    }
}

/// Provider/transport orchestration around one canonical [`BarModel`].
pub struct BarRuntime {
    model: BarModel,
    pending_changes: DirtyBits,
    revision: u64,

    #[cfg(feature = "transport-shared")]
    transport: Option<SharedTransport>,
    #[cfg(feature = "transport-shared")]
    transport_generation: u64,
    #[cfg(feature = "transport-shared")]
    transport_recovery: Option<TransportRecoveryState>,
    #[cfg(feature = "transport-shared")]
    pending_restores: Vec<PendingRestore>,
    #[cfg(feature = "transport-shared")]
    pending_restore_retry_at: Option<Instant>,
    #[cfg(feature = "provider-alsa")]
    audio: crate::audio_manager::AudioManager,
    #[cfg(feature = "provider-system")]
    system: crate::system_monitor::SystemMonitor,
    #[cfg(feature = "provider-brightnessctl")]
    brightness: crate::brightness::BrightnessManager,
    #[cfg(feature = "provider-battery-sysfs")]
    battery: crate::battery::BatteryManager,
    #[cfg(feature = "provider-network-sysfs")]
    network: crate::network::NetworkMonitor,
}

impl Default for BarRuntime {
    fn default() -> Self {
        Self::new(ModelConfig::default()).expect("default model config is valid")
    }
}

impl BarRuntime {
    pub fn new(config: ModelConfig) -> Result<Self, ModelError> {
        Ok(Self {
            model: BarModel::new(config)?,
            pending_changes: DirtyBits::all(),
            revision: 0,
            #[cfg(feature = "transport-shared")]
            transport: None,
            #[cfg(feature = "transport-shared")]
            transport_generation: 0,
            #[cfg(feature = "transport-shared")]
            transport_recovery: None,
            #[cfg(feature = "transport-shared")]
            pending_restores: Vec::new(),
            #[cfg(feature = "transport-shared")]
            pending_restore_retry_at: None,
            #[cfg(feature = "provider-alsa")]
            audio: crate::audio_manager::AudioManager::new(),
            #[cfg(feature = "provider-system")]
            system: crate::system_monitor::SystemMonitor::new(5),
            #[cfg(feature = "provider-brightnessctl")]
            brightness: crate::brightness::BrightnessManager::new(),
            #[cfg(feature = "provider-battery-sysfs")]
            battery: crate::battery::BatteryManager::new(),
            #[cfg(feature = "provider-network-sysfs")]
            network: crate::network::NetworkMonitor::new(),
        })
    }

    #[cfg(feature = "transport-shared")]
    pub fn with_transport(
        config: ModelConfig,
        transport: Option<SharedTransport>,
    ) -> Result<Self, ModelError> {
        let mut runtime = Self::new(config)?;
        runtime.transport = transport;
        if runtime.transport.is_some() {
            runtime.transport_generation = 1;
        }
        Ok(runtime)
    }

    /// Construct a runtime whose normal transport polling also opens and
    /// recovers the configured shared transport. The first
    /// [`Self::poll_transport`] or scheduled service call attempts the open,
    /// so startup failures are returned as ordinary [`RuntimeIssue`] values.
    #[cfg(feature = "transport-shared")]
    pub fn with_managed_transport(
        config: ModelConfig,
        recovery: TransportRecoveryConfig,
    ) -> Result<Self, ModelError> {
        let mut runtime = Self::new(config)?;
        runtime.transport_recovery = Some(TransportRecoveryState {
            config: recovery,
            next_attempt: None,
        });
        Ok(runtime)
    }

    #[cfg(feature = "transport-shared")]
    pub fn set_transport(&mut self, transport: Option<SharedTransport>) -> Option<SharedTransport> {
        let replacing_handle = self.transport.is_some() || transport.is_some();
        let previous = std::mem::replace(&mut self.transport, transport);
        if replacing_handle {
            self.bump_transport_generation();
        }
        if self.transport.is_none() {
            self.suspend_pending_restore_retries();
        } else if self.model.view().wm_available && !self.pending_restores.is_empty() {
            // A replacement channel can have capacity immediately. Preserve
            // the semantic intents, but do not carry the retired transport's
            // QueueFull delay into the new generation.
            self.pending_restore_retry_at = Some(Instant::now());
        }
        if let Some(recovery) = self.transport_recovery.as_mut() {
            if self.transport.is_some() {
                recovery.next_attempt = None;
            } else if previous.is_some() {
                recovery.next_attempt = Some(runtime_deadline(
                    Instant::now(),
                    recovery.config.retry_interval,
                ));
            }
        }
        previous
    }

    #[cfg(feature = "transport-shared")]
    #[must_use]
    pub fn transport(&self) -> Option<&SharedTransport> {
        self.transport.as_ref()
    }

    /// Opaque generation that changes whenever a transport handle is
    /// installed, replaced, or removed. Native event loops can rebuild an
    /// optional notifier when this value changes while periodic polling keeps
    /// correctness independent of notifier registration.
    #[cfg(feature = "transport-shared")]
    #[must_use]
    pub const fn transport_generation(&self) -> u64 {
        self.transport_generation
    }

    #[cfg(feature = "transport-shared")]
    #[must_use]
    pub fn transport_status(&self) -> TransportStatus {
        if self.transport.is_some() {
            if self.model.view().wm_available {
                TransportStatus::Ready
            } else {
                TransportStatus::Connected
            }
        } else if self.transport_recovery.is_some() {
            TransportStatus::Recovering
        } else {
            TransportStatus::Disabled
        }
    }

    /// Replace the automatic recovery policy without replacing an already
    /// installed transport. If disconnected, the next poll attempts the new
    /// path immediately.
    #[cfg(feature = "transport-shared")]
    pub fn set_transport_recovery(&mut self, recovery: Option<TransportRecoveryConfig>) {
        self.transport_recovery = recovery.map(|config| TransportRecoveryState {
            config,
            next_attempt: None,
        });
    }

    #[cfg(feature = "transport-shared")]
    #[must_use]
    pub fn transport_recovery(&self) -> Option<&TransportRecoveryConfig> {
        self.transport_recovery.as_ref().map(|state| &state.config)
    }

    #[must_use]
    pub const fn model(&self) -> &BarModel {
        &self.model
    }

    #[must_use]
    pub fn view(&self) -> BarView<'_> {
        self.model.view()
    }

    #[must_use]
    pub fn snapshot(&self) -> BarSnapshot {
        self.model.snapshot()
    }

    /// Revision assigned to the most recently captured [`RuntimeFrame`].
    /// It advances for every capture, including explicit state replay, so a
    /// frontend can reject an older asynchronously delivered frame even when
    /// two frames contain the same snapshot.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Capture an operation result and the current snapshot as one frame.
    /// Any accumulated model changes are merged into `update.changes` and
    /// cleared from the runtime.
    #[must_use]
    pub fn frame(&mut self, mut update: RuntimeUpdate) -> RuntimeFrame {
        update.changes |= self.take_changes();
        self.revision = self.revision.saturating_add(1);
        RuntimeFrame {
            revision: self.revision,
            snapshot: self.snapshot(),
            update,
        }
    }

    /// Capture the current state without performing an operation. This is
    /// useful for initial toolkit state and explicit frontend replay.
    #[must_use]
    pub fn current_frame(&mut self) -> RuntimeFrame {
        self.frame(RuntimeUpdate::default())
    }

    /// Reduce any semantic event and execute every effect supported by the
    /// currently enabled adapters.
    pub fn apply_event(&mut self, event: BarEvent) -> RuntimeUpdate {
        match self.model.update(event) {
            Ok(update) => self.consume_model_update(update),
            Err(error) => RuntimeUpdate::issue(RuntimeIssue::Model(error)),
        }
    }

    /// Dispatch semantic user intent through the same reducer/effect path as
    /// provider and window-manager events.
    pub fn dispatch(&mut self, action: UserAction) -> RuntimeUpdate {
        self.apply_event(BarEvent::User(action))
    }

    /// Dispatch one semantic action and capture the coherent resulting frame.
    #[must_use]
    pub fn dispatch_frame(&mut self, action: UserAction) -> RuntimeFrame {
        let update = self.dispatch(action);
        self.frame(update)
    }

    /// Refresh the clock and all enabled providers. Provider polling remains
    /// synchronous and event-loop neutral; frontends decide when to call it.
    pub fn tick(&mut self) -> RuntimeUpdate {
        #[allow(unused_mut)]
        let mut update = RuntimeUpdate::default();

        #[cfg(feature = "clock-chrono")]
        {
            let now = chrono::Local::now();
            let (minute_format, second_format) = {
                let config = self.model.config();
                (
                    config.clock_minute_format.clone(),
                    config.clock_second_format.clone(),
                )
            };
            match (
                format_clock(&now, &minute_format),
                format_clock(&now, &second_format),
            ) {
                (Ok(minute), Ok(second)) => {
                    update.merge(
                        self.apply_event(BarEvent::Clock(crate::ClockState { minute, second })),
                    );
                }
                (minute, second) => {
                    let message = minute
                        .err()
                        .into_iter()
                        .chain(second.err())
                        .collect::<Vec<_>>()
                        .join("; ");
                    update.issues.push(RuntimeIssue::AdapterFailed {
                        adapter: RuntimeAdapter::Clock,
                        operation: "format",
                        message,
                    });
                }
            }
        }

        #[cfg(feature = "provider-system")]
        {
            let _ = self.system.update_if_needed();
            update.merge(self.sync_system());
        }

        #[cfg(feature = "provider-alsa")]
        {
            if let Err(error) = self.audio.try_update_if_needed() {
                update.issues.push(RuntimeIssue::AdapterFailed {
                    adapter: RuntimeAdapter::Audio,
                    operation: "poll",
                    message: error.to_string(),
                });
            }
            update.merge(self.sync_audio());
        }

        #[cfg(feature = "provider-brightnessctl")]
        {
            if let Err(error) = self.brightness.try_update_if_needed() {
                update.issues.push(RuntimeIssue::AdapterFailed {
                    adapter: RuntimeAdapter::Brightness,
                    operation: "poll",
                    message: error.to_string(),
                });
            }
            update.merge(self.sync_brightness());
        }

        #[cfg(feature = "provider-battery-sysfs")]
        {
            if let Err(error) = self.battery.try_update_if_needed() {
                update.issues.push(RuntimeIssue::AdapterFailed {
                    adapter: RuntimeAdapter::Battery,
                    operation: "poll",
                    message: error.to_string(),
                });
            }
            update.merge(self.sync_battery());
        }

        #[cfg(feature = "provider-network-sysfs")]
        match self.network.poll() {
            Ok(state) => update.merge(self.apply_event(BarEvent::Network(state))),
            Err(error) => {
                update.issues.push(RuntimeIssue::AdapterFailed {
                    adapter: RuntimeAdapter::Network,
                    operation: "poll",
                    message: error.to_string(),
                });
                update.merge(
                    self.apply_event(BarEvent::Network(crate::NetworkState::disconnected())),
                );
            }
        }

        update
    }

    /// Perform one complete unscheduled service pass: poll (and, when
    /// configured, recover) the WM transport, then refresh all providers.
    /// Event loops that run faster than the provider cadence should use
    /// [`RuntimeSchedule`] instead.
    pub fn service(&mut self) -> RuntimeUpdate {
        let mut update = self.poll_transport();
        update.merge(self.tick());
        update
    }

    /// Drain the configured shared transport and reduce its newest WM
    /// snapshot. A managed transport is opened or retried when due. Without
    /// the feature or a configured transport this is a harmless no-op.
    pub fn poll_transport(&mut self) -> RuntimeUpdate {
        self.poll_transport_at(Instant::now())
    }

    /// Deterministic transport poll using a caller-supplied monotonic time.
    /// This is useful to share one `Instant` across an event-loop turn and to
    /// test retry boundaries without sleeping.
    pub fn poll_transport_at(&mut self, now: Instant) -> RuntimeUpdate {
        #[cfg(feature = "transport-shared")]
        {
            let mut update = self.reconnect_transport_at(now);
            let result = match self.transport.as_ref() {
                Some(transport) => transport.drain_latest(),
                None => {
                    self.suspend_pending_restore_retries();
                    return update;
                }
            };

            match result {
                Ok(Some(snapshot)) => {
                    update.merge(self.apply_event(BarEvent::WindowManager(snapshot)));
                }
                Ok(None) => {}
                Err(error) => {
                    self.drop_transport();
                    self.schedule_transport_retry_at(now);
                    update.merge(self.apply_event(BarEvent::WindowManagerUnavailable));
                    update.issues.push(RuntimeIssue::AdapterFailed {
                        adapter: RuntimeAdapter::Transport,
                        operation: "drain_latest",
                        message: error.to_string(),
                    });
                }
            }
            if self.transport.is_some() {
                update.merge(self.retry_pending_restores_at(now));
            }
            update
        }

        #[cfg(not(feature = "transport-shared"))]
        {
            let _ = now;
            RuntimeUpdate::default()
        }
    }

    #[cfg(feature = "transport-shared")]
    fn reconnect_transport_at(&mut self, now: Instant) -> RuntimeUpdate {
        if self.transport.is_some() {
            return RuntimeUpdate::default();
        }

        let path = {
            let Some(recovery) = self.transport_recovery.as_ref() else {
                return RuntimeUpdate::default();
            };
            if recovery.next_attempt.is_some_and(|deadline| now < deadline) {
                return RuntimeUpdate::default();
            }
            recovery.config.path.clone()
        };

        match SharedTransport::open(&path) {
            Ok(transport) => {
                self.transport = Some(transport);
                self.bump_transport_generation();
                if let Some(recovery) = self.transport_recovery.as_mut() {
                    recovery.next_attempt = None;
                }
                RuntimeUpdate::default()
            }
            Err(error) => {
                self.schedule_transport_retry_at(now);
                RuntimeUpdate::issue(RuntimeIssue::AdapterFailed {
                    adapter: RuntimeAdapter::Transport,
                    operation: "open",
                    message: error.to_string(),
                })
            }
        }
    }

    #[cfg(feature = "transport-shared")]
    fn schedule_transport_retry_at(&mut self, now: Instant) {
        if let Some(recovery) = self.transport_recovery.as_mut() {
            recovery.next_attempt = Some(runtime_deadline(now, recovery.config.retry_interval));
        }
    }

    #[cfg(feature = "transport-shared")]
    fn drop_transport(&mut self) {
        // The unavailable interval cannot prove that a restore target is
        // stale. Keep the bounded intents dormant until a replacement
        // transport supplies an authoritative projection to prune them.
        self.suspend_pending_restore_retries();
        if self.transport.take().is_some() {
            self.bump_transport_generation();
        }
    }

    #[cfg(feature = "transport-shared")]
    fn bump_transport_generation(&mut self) {
        self.transport_generation = self.transport_generation.wrapping_add(1);
    }

    /// Return and clear all accumulated model changes, including changes from
    /// earlier operations whose individual [`RuntimeUpdate`] was discarded.
    pub fn take_changes(&mut self) -> DirtyBits {
        self.pending_changes.take()
    }

    fn consume_model_update(&mut self, update: ModelUpdate) -> RuntimeUpdate {
        let ModelUpdate { dirty, effects } = update;
        self.pending_changes |= dirty;

        #[cfg(feature = "transport-shared")]
        self.prune_pending_restores();

        let mut runtime_update = RuntimeUpdate {
            changes: dirty,
            ..RuntimeUpdate::default()
        };
        for effect in effects {
            runtime_update.merge(self.execute_effect(effect));
        }
        runtime_update
    }

    fn execute_effect(&mut self, effect: BarEffect) -> RuntimeUpdate {
        match effect {
            BarEffect::WindowManager(command) => self.execute_wm(command),
            BarEffect::ToggleMute | BarEffect::AdjustVolume(_) => self.execute_audio(effect),
            BarEffect::AdjustBrightness(_) => self.execute_brightness(effect),
            BarEffect::RefreshBattery => self.execute_battery(effect),
            BarEffect::ApplyMonitorGeometry(_)
            | BarEffect::ClearMonitorGeometry
            | BarEffect::Screenshot
            | BarEffect::OpenAudioControl => RuntimeUpdate::platform(effect),
        }
    }

    fn execute_wm(&mut self, command: WmCommand) -> RuntimeUpdate {
        if !self.model.view().wm_available {
            return RuntimeUpdate::issue(RuntimeIssue::WindowManagerUnavailable { command });
        }

        #[cfg(feature = "transport-shared")]
        if self.transport.is_some() {
            let pending_restore = PendingRestore::from_command(command);
            let result = self
                .transport
                .as_ref()
                .expect("transport presence was checked")
                .execute(command);
            return match result {
                Ok(SendOutcome::Sent) => {
                    if let Some(restore) = pending_restore {
                        self.remove_pending_restore(restore);
                    }
                    RuntimeUpdate::default()
                }
                Ok(SendOutcome::Full) => match pending_restore {
                    Some(restore) if self.enqueue_pending_restore(restore, Instant::now()) => {
                        RuntimeUpdate::default()
                    }
                    _ => RuntimeUpdate::issue(RuntimeIssue::QueueFull { command }),
                },
                Err(error) => {
                    self.drop_transport();
                    self.schedule_transport_retry_at(Instant::now());
                    let mut update = self.apply_event(BarEvent::WindowManagerUnavailable);
                    update.issues.push(RuntimeIssue::AdapterFailed {
                        adapter: RuntimeAdapter::Transport,
                        operation: "execute",
                        message: error.to_string(),
                    });
                    update
                }
            };
        }

        RuntimeUpdate::platform(BarEffect::WindowManager(command))
    }

    #[cfg(feature = "transport-shared")]
    fn enqueue_pending_restore(&mut self, restore: PendingRestore, now: Instant) -> bool {
        self.prune_pending_restores();
        if let Some(existing) = self
            .pending_restores
            .iter_mut()
            .find(|existing| existing.matches(restore))
        {
            *existing = restore;
        } else if self.pending_restores.len() < MAX_MODEL_MINIMIZED_WINDOWS {
            self.pending_restores.push(restore);
        } else {
            return false;
        }

        let deadline = runtime_deadline(now, CRITICAL_RESTORE_RETRY_INTERVAL);
        self.pending_restore_retry_at = Some(
            self.pending_restore_retry_at
                .map_or(deadline, |current| current.min(deadline)),
        );
        true
    }

    #[cfg(feature = "transport-shared")]
    fn remove_pending_restore(&mut self, restore: PendingRestore) {
        self.pending_restores
            .retain(|pending| !pending.matches(restore));
        if self.pending_restores.is_empty() {
            self.pending_restore_retry_at = None;
        }
    }

    #[cfg(feature = "transport-shared")]
    fn suspend_pending_restore_retries(&mut self) {
        self.pending_restore_retry_at = None;
    }

    #[cfg(feature = "transport-shared")]
    fn prune_pending_restores(&mut self) {
        let view = self.model.view();
        if !view.wm_available {
            self.suspend_pending_restore_retries();
            return;
        }
        self.pending_restores.retain(|pending| {
            pending.wm_session_id == view.wm_session_id
                && Some(pending.minimized_generation) == view.wm_sequence
                && view
                    .minimized_windows
                    .iter()
                    .any(|window| window.token == pending.window)
        });
        if self.pending_restores.is_empty() {
            self.pending_restore_retry_at = None;
        }
    }

    #[cfg(feature = "transport-shared")]
    fn retry_pending_restores_at(&mut self, now: Instant) -> RuntimeUpdate {
        self.prune_pending_restores();
        if !self.model.view().wm_available
            || self.transport.is_none()
            || self.pending_restores.is_empty()
            || self
                .pending_restore_retry_at
                .is_some_and(|deadline| now < deadline)
        {
            return RuntimeUpdate::default();
        }

        while let Some(restore) = self.pending_restores.first().copied() {
            let monitor = self
                .model
                .view()
                .minimized_windows
                .iter()
                .find(|window| window.token == restore.window)
                .map(|window| window.monitor)
                .expect("pending restores were pruned against the current model");
            let command = WmCommand::RestoreWindow {
                window: restore.window,
                wm_session_id: restore.wm_session_id,
                minimized_generation: restore.minimized_generation,
                monitor,
                geometry: restore.geometry,
            };
            let result = self
                .transport
                .as_ref()
                .expect("pending restore retry requires a transport")
                .execute(command);
            match result {
                Ok(SendOutcome::Sent) => {
                    self.pending_restores.remove(0);
                }
                Ok(SendOutcome::Full) => {
                    self.pending_restore_retry_at =
                        Some(runtime_deadline(now, CRITICAL_RESTORE_RETRY_INTERVAL));
                    return RuntimeUpdate::default();
                }
                Err(error) => {
                    self.drop_transport();
                    self.schedule_transport_retry_at(now);
                    let mut update = self.apply_event(BarEvent::WindowManagerUnavailable);
                    update.issues.push(RuntimeIssue::AdapterFailed {
                        adapter: RuntimeAdapter::Transport,
                        operation: "execute",
                        message: error.to_string(),
                    });
                    return update;
                }
            }
        }
        self.pending_restore_retry_at = None;
        RuntimeUpdate::default()
    }

    fn execute_audio(&mut self, effect: BarEffect) -> RuntimeUpdate {
        #[cfg(feature = "provider-alsa")]
        {
            let operation = match effect {
                BarEffect::ToggleMute => "toggle_mute",
                BarEffect::AdjustVolume(_) => "adjust_volume",
                _ => unreachable!("execute_audio only receives audio effects"),
            };
            let device = self.audio.get_master_device().cloned();
            let result = match (device, effect) {
                (Some(device), BarEffect::ToggleMute) => self
                    .audio
                    .toggle_mute(&device.name)
                    .map(|_| ())
                    .map_err(|error| error.to_string()),
                (Some(device), BarEffect::AdjustVolume(delta)) => self
                    .audio
                    .adjust_volume(&device.name, delta)
                    .map(|_| ())
                    .map_err(|error| error.to_string()),
                (None, _) => Err("no playback volume device is available".to_owned()),
                _ => unreachable!("execute_audio only receives audio effects"),
            };

            let mut update = RuntimeUpdate::default();
            if let Err(error) = result {
                update.issues.push(RuntimeIssue::AdapterFailed {
                    adapter: RuntimeAdapter::Audio,
                    operation,
                    message: error,
                });
            }
            update.merge(self.sync_audio());
            update
        }

        #[cfg(not(feature = "provider-alsa"))]
        RuntimeUpdate::platform(effect)
    }

    fn execute_brightness(&mut self, effect: BarEffect) -> RuntimeUpdate {
        #[cfg(feature = "provider-brightnessctl")]
        {
            let delta = match effect {
                BarEffect::AdjustBrightness(delta) => delta,
                _ => unreachable!("execute_brightness only receives brightness effects"),
            };
            let result = self.brightness.try_adjust(delta);
            let mut update = RuntimeUpdate::default();
            if let Err(error) = result {
                update.issues.push(RuntimeIssue::AdapterFailed {
                    adapter: RuntimeAdapter::Brightness,
                    operation: "adjust",
                    message: error.to_string(),
                });
            }
            update.merge(self.sync_brightness());
            update
        }

        #[cfg(not(feature = "provider-brightnessctl"))]
        RuntimeUpdate::platform(effect)
    }

    fn execute_battery(&mut self, effect: BarEffect) -> RuntimeUpdate {
        #[cfg(feature = "provider-battery-sysfs")]
        {
            debug_assert_eq!(effect, BarEffect::RefreshBattery);
            let mut update = RuntimeUpdate::default();
            if let Err(error) = self.battery.try_refresh() {
                update.issues.push(RuntimeIssue::AdapterFailed {
                    adapter: RuntimeAdapter::Battery,
                    operation: "refresh",
                    message: error.to_string(),
                });
            }
            update.merge(self.sync_battery());
            update
        }

        #[cfg(not(feature = "provider-battery-sysfs"))]
        RuntimeUpdate::platform(effect)
    }

    #[cfg(feature = "provider-alsa")]
    fn sync_audio(&mut self) -> RuntimeUpdate {
        let device = self.audio.get_master_device().cloned();
        let state = device.as_ref().map_or_else(AudioState::default, |device| {
            let percent = device.has_volume_control.then(|| {
                let whole = device.volume.clamp(0, 100) as u8;
                Percent::from_whole(whole).expect("clamped audio volume is valid")
            });
            AudioState::new(percent, device.is_muted)
        });
        let info = device.map(|device| AudioDeviceInfo {
            name: device.name,
            index: device.index,
            volume: device.volume.clamp(0, 100),
            is_muted: device.is_muted,
            description: device.description,
            has_volume_control: device.has_volume_control,
            has_switch_control: device.has_switch_control,
        });
        let mut update = self.apply_event(BarEvent::Audio(state));
        update.merge(self.apply_event(BarEvent::AudioDevice(info)));
        update
    }

    #[cfg(feature = "provider-system")]
    fn sync_system(&mut self) -> RuntimeUpdate {
        let Some(snapshot) = self.system.get_snapshot() else {
            let mut update = self.apply_event(BarEvent::System(SystemState::default()));
            update.merge(self.apply_event(BarEvent::SystemDetails(SystemDetails::default())));
            return update;
        };
        let details = SystemDetails {
            cpu_usage: snapshot.cpu_usage.clone(),
            cpu_average: snapshot.cpu_average,
            memory_total: snapshot.memory_total,
            memory_used: snapshot.memory_used,
            memory_available: snapshot.memory_available,
            memory_usage_percent: snapshot.memory_usage_percent,
            uptime: snapshot.uptime,
            load_average: SystemLoadAverage {
                one_minute: snapshot.load_average.one_minute,
                five_minutes: snapshot.load_average.five_minutes,
                fifteen_minutes: snapshot.load_average.fifteen_minutes,
            },
        };
        let cpu = f64::from(snapshot.cpu_average);
        let memory = f64::from(snapshot.memory_usage_percent);

        let mut update = RuntimeUpdate::default();
        let cpu = provider_percent(cpu, RuntimeAdapter::System, "cpu_percent", &mut update);
        let memory = provider_percent(
            memory,
            RuntimeAdapter::System,
            "memory_percent",
            &mut update,
        );
        update.merge(self.apply_event(BarEvent::System(SystemState::new(cpu, memory))));
        update.merge(self.apply_event(BarEvent::SystemDetails(details)));
        update
    }

    #[cfg(feature = "provider-brightnessctl")]
    fn sync_brightness(&mut self) -> RuntimeUpdate {
        let percent = self
            .brightness
            .percent()
            .and_then(|value| crate::Percent::from_whole(value).ok());
        self.apply_event(BarEvent::Brightness(BrightnessState::new(percent)))
    }

    #[cfg(feature = "provider-battery-sysfs")]
    fn sync_battery(&mut self) -> RuntimeUpdate {
        let state = if self.battery.is_present() {
            let percent = self
                .battery
                .capacity()
                .and_then(|value| crate::Percent::from_whole(value).ok());
            BatteryState::present(percent, self.battery.is_charging())
        } else {
            BatteryState::absent()
        };
        self.apply_event(BarEvent::Battery(state))
    }
}

fn validate_runtime_interval(
    field: &'static str,
    interval: Duration,
) -> Result<(), RuntimeConfigError> {
    if interval.is_zero() {
        return Err(RuntimeConfigError::ZeroInterval { field });
    }
    if Instant::now().checked_add(interval).is_none() {
        return Err(RuntimeConfigError::IntervalTooLarge { field });
    }
    Ok(())
}

fn runtime_deadline(now: Instant, interval: Duration) -> Instant {
    // Constructors reject intervals that overflow the process's current
    // monotonic instant. A synthetic caller-supplied instant can still sit at
    // the representable edge; treating it as immediately due is safe and
    // avoids a panic in deterministic tests or unusual embedders.
    now.checked_add(interval).unwrap_or(now)
}

#[cfg(feature = "clock-chrono")]
fn format_clock(now: &chrono::DateTime<chrono::Local>, pattern: &str) -> Result<String, String> {
    use chrono::format::{Item, StrftimeItems};

    if StrftimeItems::new(pattern).any(|item| matches!(item, Item::Error)) {
        return Err(format!("invalid chrono clock format {pattern:?}"));
    }
    Ok(now.format(pattern).to_string())
}

#[cfg(feature = "provider-system")]
fn provider_percent(
    value: f64,
    adapter: RuntimeAdapter,
    field: &'static str,
    update: &mut RuntimeUpdate,
) -> Option<crate::Percent> {
    match crate::Percent::new(value) {
        Ok(percent) => Some(percent),
        Err(error) => {
            update.issues.push(RuntimeIssue::InvalidProviderPercent {
                adapter,
                field,
                error,
            });
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LayoutId, MonitorGeometry, MonitorId, TagId, ThemeMode, WmSnapshot};
    #[cfg(feature = "transport-shared")]
    use crate::{MINIMIZED_WINDOW_FLAG_PREVIEW_AVAILABLE, MinimizedWindow, WindowToken};
    #[cfg(feature = "transport-shared")]
    use std::sync::atomic::{AtomicU64, Ordering};

    #[cfg(feature = "transport-shared")]
    static NEXT_TRANSPORT_PATH: AtomicU64 = AtomicU64::new(0);

    #[cfg(feature = "transport-shared")]
    fn minimized_snapshot(
        wm_session_id: u64,
        minimized_generation: u64,
        tokens: &[u64],
    ) -> WmSnapshot {
        WmSnapshot {
            sequence: Some(minimized_generation),
            wm_session_id,
            monitor: MonitorId(4),
            geometry: Some(MonitorGeometry {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            }),
            layout_symbol: "[]=".to_owned(),
            minimized_windows: tokens
                .iter()
                .copied()
                .map(|token| MinimizedWindow {
                    token: WindowToken(token),
                    monitor: MonitorId(4),
                    title: format!("window {token}"),
                    app_id: "test".to_owned(),
                    flags: MINIMIZED_WINDOW_FLAG_PREVIEW_AVAILABLE,
                })
                .collect(),
            ..WmSnapshot::default()
        }
    }

    #[cfg(feature = "transport-shared")]
    fn critical_restore_runtime(
        wm_session_id: u64,
        tokens: &[u64],
    ) -> (BarRuntime, shared_structures::SharedRingBuffer) {
        let sequence = NEXT_TRANSPORT_PATH.fetch_add(1, Ordering::Relaxed);
        let path = format!(
            "/tmp/xbar-core-runtime-critical-restore-{}-{sequence}",
            std::process::id()
        );
        let owner = shared_structures::SharedRingBufferOptions::new()
            .capacity(8)
            .command_capacity(2)
            .adaptive_poll_spins(0)
            .create(&path)
            .expect("create small critical-restore transport");
        let transport = SharedTransport::open(&path).unwrap();
        let mut runtime =
            BarRuntime::with_transport(ModelConfig::default(), Some(transport)).unwrap();
        let _ = runtime.apply_event(BarEvent::WindowManager(minimized_snapshot(
            wm_session_id,
            1,
            tokens,
        )));
        (runtime, owner)
    }

    #[cfg(feature = "transport-shared")]
    fn fill_command_ring(runtime: &BarRuntime, owner: &shared_structures::SharedRingBuffer) {
        let filler = WmCommand::SetLayout {
            layout: LayoutId(1),
            monitor: MonitorId(4),
        };
        for _ in 0..owner.command_capacity() {
            assert_eq!(
                runtime.transport().unwrap().execute(filler).unwrap(),
                SendOutcome::Sent
            );
        }
    }

    #[cfg(feature = "transport-shared")]
    fn restore_action(window: u64, wm_session_id: u64, x: i32) -> UserAction {
        restore_action_at_generation(window, wm_session_id, 1, x)
    }

    #[cfg(feature = "transport-shared")]
    fn restore_action_at_generation(
        window: u64,
        wm_session_id: u64,
        minimized_generation: u64,
        x: i32,
    ) -> UserAction {
        UserAction::RestoreWindow {
            window: WindowToken(window),
            wm_session_id,
            minimized_generation,
            geometry: DockItemGeometry::new(x, 20, 36, 24),
        }
    }

    #[test]
    fn lifecycle_intervals_are_validated_and_schedule_is_monotonic() {
        assert!(matches!(
            RuntimeSchedule::new(Duration::ZERO),
            Err(RuntimeConfigError::ZeroInterval {
                field: "runtime tick interval"
            })
        ));
        assert!(matches!(
            RuntimeSchedule::new(Duration::MAX),
            Err(RuntimeConfigError::IntervalTooLarge {
                field: "runtime tick interval"
            })
        ));

        let interval = Duration::from_secs(1);
        let start = Instant::now();
        let mut schedule = RuntimeSchedule::new(interval).unwrap();
        let mut runtime = BarRuntime::default();

        assert_eq!(schedule.next_service_deadline(&runtime, start), start);

        let _ = schedule.service_at(&mut runtime, start);
        assert_eq!(schedule.next_tick(), start.checked_add(interval));
        assert_eq!(
            schedule.next_service_deadline(&runtime, start),
            start + interval
        );
        let deadline = schedule.next_tick();

        let _ = schedule.service_at(&mut runtime, start + Duration::from_millis(500));
        assert_eq!(schedule.next_tick(), deadline);

        let delayed = start + Duration::from_secs(5);
        let _ = schedule.service_at(&mut runtime, delayed);
        assert_eq!(schedule.next_tick(), delayed.checked_add(interval));

        schedule.reset();
        assert_eq!(schedule.next_tick(), None);
        assert_eq!(schedule.next_service_deadline(&runtime, delayed), delayed);
    }

    #[cfg(feature = "transport-shared")]
    #[test]
    fn service_deadline_includes_managed_transport_retry() {
        let start = Instant::now();
        let retry = Duration::from_secs(2);
        let recovery =
            TransportRecoveryConfig::new("/definitely/missing/xbar-core-deadline-test", retry)
                .unwrap();
        let mut runtime =
            BarRuntime::with_managed_transport(ModelConfig::default(), recovery).unwrap();
        let mut schedule = RuntimeSchedule::new(Duration::from_secs(10)).unwrap();

        assert_eq!(schedule.next_service_deadline(&runtime, start), start);
        let update = schedule.service_at(&mut runtime, start);
        assert!(update.transport_failed());
        assert_eq!(
            schedule.next_service_deadline(&runtime, start),
            start + retry
        );
    }

    #[cfg(feature = "transport-shared")]
    #[test]
    fn critical_restore_survives_full_ring_and_retries_once_at_its_deadline() {
        let (mut runtime, owner) = critical_restore_runtime(91, &[41]);
        fill_command_ring(&runtime, &owner);

        let update = runtime.dispatch(restore_action(41, 91, 10));
        assert!(update.is_empty(), "a safely buffered restore is accepted");
        assert_eq!(runtime.pending_restores.len(), 1);
        let deadline = runtime
            .pending_restore_retry_at
            .expect("buffered restore has a retry deadline");
        let schedule = RuntimeSchedule {
            tick_interval: Duration::from_secs(10),
            next_tick: Some(deadline + Duration::from_secs(1)),
        };
        assert_eq!(
            schedule.next_service_deadline(&runtime, deadline - Duration::from_millis(50)),
            deadline
        );

        for _ in 0..owner.command_capacity() {
            assert!(owner.try_receive_command().unwrap().is_some());
        }
        assert!(owner.try_receive_command().unwrap().is_none());

        let early = runtime.poll_transport_at(deadline - Duration::from_nanos(1));
        assert!(early.is_empty());
        assert!(owner.try_receive_command().unwrap().is_none());

        let retried = runtime.poll_transport_at(deadline);
        assert!(retried.is_empty());
        let command = owner
            .try_receive_command()
            .unwrap()
            .expect("restore is sent at the exact retry boundary");
        assert_eq!(
            command.get_command_type(),
            shared_structures::CommandType::RestoreMinimized
        );
        assert_eq!(command.get_window_id(), 41);
        assert_eq!(command.get_wm_session_id(), 91);
        assert!(owner.try_receive_command().unwrap().is_none());
        assert!(runtime.pending_restores.is_empty());
        assert!(runtime.pending_restore_retry_at.is_none());

        let later = runtime.poll_transport_at(deadline + CRITICAL_RESTORE_RETRY_INTERVAL);
        assert!(later.is_empty());
        assert!(owner.try_receive_command().unwrap().is_none());
        owner.destroy().unwrap();
    }

    #[cfg(feature = "transport-shared")]
    #[test]
    fn critical_restore_survives_unavailable_gap_and_retries_after_matching_snapshot() {
        let (mut runtime, owner) = critical_restore_runtime(92, &[42]);
        let replacement = runtime.transport().unwrap().clone();
        fill_command_ring(&runtime, &owner);

        assert!(runtime.dispatch(restore_action(42, 92, 10)).is_empty());
        assert_eq!(runtime.pending_restores.len(), 1);
        assert!(runtime.pending_restore_retry_at.is_some());

        drop(runtime.set_transport(None));
        let _ = runtime.apply_event(BarEvent::WindowManagerUnavailable);
        assert_eq!(runtime.pending_restores.len(), 1);
        assert!(
            runtime.pending_restore_retry_at.is_none(),
            "offline intent must not keep an overdue service deadline armed"
        );
        assert!(runtime.poll_transport_at(Instant::now()).is_empty());
        assert_eq!(runtime.pending_restores.len(), 1);

        for _ in 0..owner.command_capacity() {
            assert!(owner.try_receive_command().unwrap().is_some());
        }
        runtime.set_transport(Some(replacement));
        let _ = runtime.apply_event(BarEvent::WindowManager(minimized_snapshot(92, 1, &[42])));

        assert!(runtime.poll_transport_at(Instant::now()).is_empty());
        let command = owner
            .try_receive_command()
            .unwrap()
            .expect("matching reconnect retries the retained restore");
        assert_eq!(
            command.get_command_type(),
            shared_structures::CommandType::RestoreMinimized
        );
        assert_eq!(command.get_window_id(), 42);
        assert_eq!(command.get_wm_session_id(), 92);
        assert!(runtime.pending_restores.is_empty());
        assert!(runtime.poll_transport_at(Instant::now()).is_empty());
        assert!(owner.try_receive_command().unwrap().is_none());
        owner.destroy().unwrap();
    }

    #[cfg(feature = "transport-shared")]
    #[test]
    fn critical_restore_is_pruned_by_first_mismatching_snapshot_after_unavailable_gap() {
        let (mut runtime, owner) = critical_restore_runtime(93, &[43]);
        let replacement = runtime.transport().unwrap().clone();
        fill_command_ring(&runtime, &owner);

        assert!(runtime.dispatch(restore_action(43, 93, 10)).is_empty());
        drop(runtime.set_transport(None));
        let _ = runtime.apply_event(BarEvent::WindowManagerUnavailable);
        assert_eq!(runtime.pending_restores.len(), 1);

        for _ in 0..owner.command_capacity() {
            assert!(owner.try_receive_command().unwrap().is_some());
        }
        runtime.set_transport(Some(replacement));
        let _ = runtime.apply_event(BarEvent::WindowManager(minimized_snapshot(93, 2, &[43])));

        assert!(runtime.pending_restores.is_empty());
        assert!(runtime.poll_transport_at(Instant::now()).is_empty());
        assert!(owner.try_receive_command().unwrap().is_none());
        owner.destroy().unwrap();
    }

    #[cfg(feature = "transport-shared")]
    #[test]
    fn critical_restore_is_cancelled_when_window_session_or_generation_changes() {
        let (mut runtime, owner) = critical_restore_runtime(71, &[1, 2]);
        fill_command_ring(&runtime, &owner);
        assert!(runtime.dispatch(restore_action(1, 71, 10)).is_empty());
        assert!(runtime.dispatch(restore_action(2, 71, 20)).is_empty());
        assert_eq!(runtime.pending_restores.len(), 2);

        let _ = runtime.apply_event(BarEvent::WindowManager(WmSnapshot {
            sequence: Some(1),
            wm_session_id: 71,
            monitor: MonitorId(4),
            minimized_windows: vec![MinimizedWindow {
                token: WindowToken(2),
                monitor: MonitorId(4),
                title: "still minimized".to_owned(),
                app_id: "test".to_owned(),
                flags: MINIMIZED_WINDOW_FLAG_PREVIEW_AVAILABLE,
            }],
            ..WmSnapshot::default()
        }));
        assert_eq!(runtime.pending_restores.len(), 1);
        assert_eq!(runtime.pending_restores[0].window, WindowToken(2));

        let _ = runtime.apply_event(BarEvent::WindowManager(WmSnapshot {
            sequence: Some(2),
            wm_session_id: 71,
            monitor: MonitorId(4),
            minimized_windows: vec![MinimizedWindow {
                token: WindowToken(2),
                monitor: MonitorId(4),
                title: "rapidly re-minimized".to_owned(),
                app_id: "test".to_owned(),
                flags: MINIMIZED_WINDOW_FLAG_PREVIEW_AVAILABLE,
            }],
            ..WmSnapshot::default()
        }));
        assert!(
            runtime.pending_restores.is_empty(),
            "same token in a new generation must not inherit a delayed click"
        );

        assert!(
            runtime
                .dispatch(restore_action_at_generation(2, 71, 2, 30))
                .is_empty()
        );
        assert_eq!(runtime.pending_restores.len(), 1);

        let _ = runtime.apply_event(BarEvent::WindowManager(WmSnapshot {
            sequence: Some(3),
            wm_session_id: 72,
            monitor: MonitorId(4),
            minimized_windows: vec![MinimizedWindow {
                token: WindowToken(2),
                monitor: MonitorId(4),
                title: "reused token".to_owned(),
                app_id: "other".to_owned(),
                flags: MINIMIZED_WINDOW_FLAG_PREVIEW_AVAILABLE,
            }],
            ..WmSnapshot::default()
        }));
        assert!(runtime.pending_restores.is_empty());
        assert!(runtime.pending_restore_retry_at.is_none());

        while owner.try_receive_command().unwrap().is_some() {}
        let update = runtime.poll_transport_at(Instant::now() + Duration::from_secs(1));
        assert!(update.is_empty());
        assert!(owner.try_receive_command().unwrap().is_none());
        owner.destroy().unwrap();
    }

    #[cfg(feature = "transport-shared")]
    #[test]
    fn critical_restore_queue_is_bounded_deduplicated_and_restore_only() {
        let tokens: Vec<_> = (1..=MAX_MODEL_MINIMIZED_WINDOWS as u64).collect();
        let (mut runtime, owner) = critical_restore_runtime(81, &tokens);
        fill_command_ring(&runtime, &owner);

        for token in &tokens {
            let update = runtime.dispatch(restore_action(*token, 81, *token as i32));
            assert!(update.is_empty());
        }
        assert_eq!(runtime.pending_restores.len(), MAX_MODEL_MINIMIZED_WINDOWS);

        let replacement = restore_action(1, 81, 999);
        assert!(runtime.dispatch(replacement).is_empty());
        assert_eq!(runtime.pending_restores.len(), MAX_MODEL_MINIMIZED_WINDOWS);
        assert_eq!(
            runtime
                .pending_restores
                .iter()
                .find(|pending| pending.window == WindowToken(1))
                .unwrap()
                .geometry
                .x,
            999
        );

        let preview = runtime.dispatch(UserAction::PreviewWindow {
            window: WindowToken(1),
            wm_session_id: 81,
            minimized_generation: 1,
            visible: true,
            renewal: false,
            geometry: DockItemGeometry::new(10, 20, 36, 24),
        });
        assert!(matches!(
            preview.issues.as_slice(),
            [RuntimeIssue::QueueFull {
                command: WmCommand::PreviewWindow { .. }
            }]
        ));
        let geometry = runtime.dispatch(UserAction::SetDockGeometry {
            window: Some(WindowToken(1)),
            wm_session_id: 81,
            minimized_generation: 1,
            geometry: DockItemGeometry::new(10, 20, 36, 24),
        });
        assert!(matches!(
            geometry.issues.as_slice(),
            [RuntimeIssue::QueueFull {
                command: WmCommand::SetDockGeometry { .. }
            }]
        ));
        assert_eq!(runtime.pending_restores.len(), MAX_MODEL_MINIMIZED_WINDOWS);
        owner.destroy().unwrap();
    }

    #[test]
    fn runtime_frames_capture_revision_snapshot_and_accumulated_changes() {
        let mut runtime = BarRuntime::default();

        let initial = runtime.current_frame();
        assert_eq!(initial.revision, 1);
        assert_eq!(initial.changes(), DirtyBits::all());
        assert_eq!(initial.snapshot.theme, ThemeMode::Dark);

        let empty = runtime.current_frame();
        assert!(empty.update.is_empty());
        assert_eq!(empty.revision, 2);

        // Discarding an individual update does not lose its state damage.
        let _ = runtime.dispatch(UserAction::ToggleTheme);
        assert_eq!(runtime.revision(), 2);
        let frame = runtime.current_frame();
        assert_eq!(frame.revision, 3);
        assert!(frame.changes().contains(DirtyBits::THEME_CHANGED));
        assert_eq!(frame.snapshot.theme, ThemeMode::Light);
        assert!(runtime.take_changes().is_empty());

        // Platform-only work still receives an ordering revision without
        // claiming model changes.
        let frame = runtime.dispatch_frame(UserAction::Screenshot);
        assert_eq!(frame.revision, 4);
        assert!(frame.changes().is_empty());
        assert_eq!(frame.update.platform_effects, vec![BarEffect::Screenshot]);
    }

    #[test]
    fn scheduled_frame_is_an_initial_full_projection_then_a_delta() {
        let now = Instant::now();
        let mut runtime = BarRuntime::default();
        let mut schedule = RuntimeSchedule::default();

        let first = schedule.service_frame_at(&mut runtime, now);
        assert_eq!(first.changes(), DirtyBits::all());

        let second = schedule.service_frame_at(&mut runtime, now + Duration::from_millis(100));
        assert!(second.changes().is_empty());
    }

    #[test]
    fn runtime_update_classifies_adapter_failures_without_false_disconnects() {
        let command = WmCommand::SetLayout {
            layout: LayoutId(1),
            monitor: MonitorId(2),
        };
        let unavailable = RuntimeUpdate::issue(RuntimeIssue::WindowManagerUnavailable { command });
        assert!(unavailable.has_issues());
        assert!(!unavailable.transport_failed());

        let mut failed = RuntimeUpdate::default();
        failed.issues.push(RuntimeIssue::AdapterFailed {
            adapter: RuntimeAdapter::Transport,
            operation: "open",
            message: "not found".into(),
        });
        assert!(failed.transport_failed());
        assert!(failed.has_adapter_issue(RuntimeAdapter::Transport));
        assert!(!failed.has_adapter_issue(RuntimeAdapter::Audio));
        assert_eq!(
            failed.issues[0].to_string(),
            "window-manager transport open failed: not found"
        );
    }

    #[test]
    fn platform_effect_handler_drains_all_work_and_preserves_failures() {
        let mut update = RuntimeUpdate {
            platform_effects: vec![
                BarEffect::Screenshot,
                BarEffect::OpenAudioControl,
                BarEffect::ClearMonitorGeometry,
            ],
            ..RuntimeUpdate::default()
        };
        let mut seen = Vec::new();
        let report = update.handle_platform_effects(&mut |effect| {
            seen.push(effect);
            if effect == BarEffect::OpenAudioControl {
                Err("launcher unavailable")
            } else {
                Ok(())
            }
        });

        assert_eq!(seen.len(), 3);
        assert_eq!(report.handled, 2);
        assert!(!report.is_success());
        assert_eq!(
            report.failures,
            vec![PlatformEffectFailure {
                effect: BarEffect::OpenAudioControl,
                error: "launcher unavailable",
            }]
        );
        assert!(update.platform_effects.is_empty());
        assert_eq!(
            report.failed_effects().collect::<Vec<_>>(),
            vec![BarEffect::OpenAudioControl]
        );
    }

    #[cfg(feature = "transport-shared")]
    #[test]
    fn managed_transport_config_rejects_invalid_input() {
        assert_eq!(
            TransportRecoveryConfig::with_default_retry("").unwrap_err(),
            RuntimeConfigError::EmptyTransportPath
        );
        assert!(matches!(
            TransportRecoveryConfig::new("/tmp/xbar", Duration::ZERO),
            Err(RuntimeConfigError::ZeroInterval {
                field: "transport retry interval"
            })
        ));
        assert!(matches!(
            TransportRecoveryConfig::new("/tmp/xbar", Duration::MAX),
            Err(RuntimeConfigError::IntervalTooLarge {
                field: "transport retry interval"
            })
        ));
    }

    #[cfg(feature = "transport-shared")]
    #[test]
    fn managed_transport_retries_on_deadline_and_recovers_authoritative_state() {
        let sequence = NEXT_TRANSPORT_PATH.fetch_add(1, Ordering::Relaxed);
        let path = format!(
            "/tmp/xbar-core-runtime-managed-{}-{sequence}",
            std::process::id()
        );
        let retry = Duration::from_secs(2);
        let recovery = TransportRecoveryConfig::new(path.clone(), retry).unwrap();
        let mut runtime =
            BarRuntime::with_managed_transport(ModelConfig::default(), recovery).unwrap();
        let start = Instant::now();
        assert_eq!(runtime.transport_status(), TransportStatus::Recovering);
        assert_eq!(runtime.transport_generation(), 0);

        let first = runtime.poll_transport_at(start);
        assert!(first.transport_failed());
        assert!(matches!(
            first.issues.as_slice(),
            [RuntimeIssue::AdapterFailed {
                adapter: RuntimeAdapter::Transport,
                operation: "open",
                ..
            }]
        ));

        let early = runtime.poll_transport_at(start + Duration::from_secs(1));
        assert!(early.is_empty());

        let owner = shared_structures::SharedRingBufferOptions::new()
            .capacity(8)
            .adaptive_poll_spins(0)
            .create(&path)
            .expect("create isolated transport");
        let mut monitor_info = shared_structures::MonitorInfo {
            monitor_num: 4,
            ..shared_structures::MonitorInfo::default()
        };
        monitor_info.set_ltsymbol("[M]");
        let message = shared_structures::SharedMessage {
            timestamp: 91,
            monitor_info,
            ..shared_structures::SharedMessage::default()
        };
        assert!(owner.try_write_message(&message).unwrap());

        let recovered = runtime.poll_transport_at(start + retry);
        assert!(!recovered.transport_failed());
        assert!(runtime.transport().is_some());
        assert!(runtime.view().wm_available);
        assert_eq!(runtime.transport_status(), TransportStatus::Ready);
        assert_eq!(runtime.transport_generation(), 1);
        assert_eq!(runtime.view().monitor, MonitorId(4));
        assert_eq!(runtime.view().wm_sequence, Some(91));

        owner.destroy().unwrap();
        let disconnected = runtime.poll_transport_at(start + retry + Duration::from_secs(1));
        assert!(disconnected.transport_failed());
        assert!(runtime.transport().is_none());
        assert!(!runtime.view().wm_available);
        assert_eq!(runtime.transport_status(), TransportStatus::Recovering);
        assert_eq!(runtime.transport_generation(), 2);

        let before_retry = runtime.poll_transport_at(start + retry + Duration::from_secs(2));
        assert!(before_retry.is_empty());
        assert!(runtime.transport().is_none());

        let after_retry = runtime.poll_transport_at(start + retry + Duration::from_secs(3));
        assert!(after_retry.transport_failed());
        assert!(matches!(
            after_retry.issues.as_slice(),
            [RuntimeIssue::AdapterFailed {
                adapter: RuntimeAdapter::Transport,
                operation: "open",
                ..
            }]
        ));
        assert!(runtime.transport().is_none());
        assert!(!runtime.view().wm_available);
        assert_eq!(runtime.transport_generation(), 2);
    }

    #[test]
    fn pure_runtime_reduces_actions_and_accumulates_changes() {
        let mut runtime = BarRuntime::default();
        assert!(!runtime.take_changes().is_empty());
        assert!(runtime.take_changes().is_empty());

        let update = runtime.dispatch(UserAction::ToggleTheme);
        assert!(update.changes.contains(DirtyBits::THEME_CHANGED));
        assert_eq!(runtime.view().theme, ThemeMode::Light);
        assert!(runtime.take_changes().contains(DirtyBits::THEME_CHANGED));
    }

    #[test]
    fn platform_only_effects_are_returned_to_the_frontend() {
        let mut runtime = BarRuntime::default();
        let update = runtime.dispatch(UserAction::Screenshot);
        assert_eq!(update.platform_effects, vec![BarEffect::Screenshot]);

        runtime.apply_event(BarEvent::WindowManager(WmSnapshot {
            sequence: Some(1),
            monitor: MonitorId(0),
            geometry: None,
            layout_symbol: "[]=".into(),
            client_name: String::new(),
            tags: Vec::new(),
            ..WmSnapshot::default()
        }));
        let update = runtime.dispatch(UserAction::SetLayout(LayoutId(2)));
        assert_eq!(
            update.platform_effects,
            vec![BarEffect::WindowManager(WmCommand::SetLayout {
                layout: LayoutId(2),
                monitor: crate::MonitorId(0),
            })]
        );
    }

    #[test]
    fn window_geometry_is_reduced_then_returned_as_platform_work() {
        let mut runtime = BarRuntime::default();
        let geometry = MonitorGeometry {
            x: 100,
            y: 20,
            width: 1920,
            height: 1080,
        };
        let update = runtime.apply_event(BarEvent::WindowManager(WmSnapshot {
            sequence: Some(1),
            monitor: MonitorId(2),
            geometry: Some(geometry),
            layout_symbol: "[M]".into(),
            client_name: "terminal".into(),
            tags: Vec::new(),
            ..WmSnapshot::default()
        }));

        assert_eq!(runtime.view().geometry, Some(geometry));
        assert_eq!(
            update.platform_effects,
            vec![BarEffect::ApplyMonitorGeometry(geometry)]
        );
    }

    #[cfg(not(feature = "provider-alsa"))]
    #[test]
    fn disabled_audio_adapter_returns_effect_to_frontend() {
        let mut runtime = BarRuntime::default();
        let update = runtime.dispatch(UserAction::VolumeUp);
        assert_eq!(
            update.platform_effects,
            vec![BarEffect::AdjustVolume(i32::from(
                runtime.model().config().volume_step
            ))]
        );
    }

    #[cfg(not(feature = "provider-brightnessctl"))]
    #[test]
    fn disabled_brightness_adapter_returns_effect_to_frontend() {
        let mut runtime = BarRuntime::default();
        let update = runtime.dispatch(UserAction::BrightnessDown);
        assert_eq!(
            update.platform_effects,
            vec![BarEffect::AdjustBrightness(-i32::from(
                runtime.model().config().brightness_step
            ))]
        );
    }

    #[cfg(not(feature = "provider-battery-sysfs"))]
    #[test]
    fn disabled_battery_adapter_returns_effect_to_frontend() {
        let mut runtime = BarRuntime::default();
        let update = runtime.dispatch(UserAction::RefreshBattery);
        assert_eq!(update.platform_effects, vec![BarEffect::RefreshBattery]);
    }

    #[test]
    fn invalid_actions_are_reported_without_mutating_the_model() {
        let mut runtime = BarRuntime::new(ModelConfig {
            tag_count: 1,
            ..ModelConfig::default()
        })
        .unwrap();
        let before = runtime.snapshot();
        let update = runtime.dispatch(UserAction::ViewTag(TagId::new(1).unwrap()));

        assert!(matches!(
            update.issues.as_slice(),
            [RuntimeIssue::Model(ModelError::TagOutOfRange {
                index: 1,
                tag_count: 1,
            })]
        ));
        assert_eq!(runtime.snapshot(), before);
    }

    #[test]
    fn wm_commands_wait_for_an_authoritative_snapshot() {
        let mut runtime = BarRuntime::default();
        let command = WmCommand::ViewTag {
            tag: TagId::new(0).unwrap(),
            monitor: MonitorId(0),
        };

        let update = runtime.dispatch(UserAction::ViewTag(TagId::new(0).unwrap()));

        assert_eq!(
            update.issues,
            vec![RuntimeIssue::WindowManagerUnavailable { command }]
        );
        assert!(update.platform_effects.is_empty());
        assert!(!runtime.view().wm_available);
    }

    #[cfg(feature = "transport-shared")]
    #[test]
    fn a_reopened_transport_does_not_enable_commands_before_a_snapshot() {
        let sequence = NEXT_TRANSPORT_PATH.fetch_add(1, Ordering::Relaxed);
        let path = format!(
            "/tmp/xbar-core-runtime-reopen-gate-{}-{sequence}",
            std::process::id()
        );
        let owner = shared_structures::SharedRingBufferOptions::new()
            .capacity(8)
            .adaptive_poll_spins(0)
            .create(&path)
            .expect("create isolated transport");
        let transport = SharedTransport::open(&path).unwrap();
        let mut runtime =
            BarRuntime::with_transport(ModelConfig::default(), Some(transport)).unwrap();
        let command = WmCommand::ViewTag {
            tag: TagId::new(0).unwrap(),
            monitor: MonitorId(0),
        };

        let update = runtime.dispatch(UserAction::ViewTag(TagId::new(0).unwrap()));

        assert_eq!(
            update.issues,
            vec![RuntimeIssue::WindowManagerUnavailable { command }]
        );
        assert!(update.platform_effects.is_empty());
        assert!(runtime.transport().is_some());
        assert!(!runtime.view().wm_available);
        owner.destroy().unwrap();
    }

    #[test]
    fn owned_snapshot_does_not_follow_later_runtime_changes() {
        let mut runtime = BarRuntime::default();
        let snapshot = runtime.snapshot();
        runtime.dispatch(UserAction::ToggleSeconds);

        assert!(!snapshot.show_seconds);
        assert!(runtime.snapshot().show_seconds);
    }

    #[cfg(feature = "transport-shared")]
    #[test]
    fn destroyed_transport_becomes_an_explicit_runtime_issue() {
        let sequence = NEXT_TRANSPORT_PATH.fetch_add(1, Ordering::Relaxed);
        let path = format!(
            "/tmp/xbar-core-runtime-transport-{}-{sequence}",
            std::process::id()
        );
        let owner = shared_structures::SharedRingBufferOptions::new()
            .capacity(8)
            .adaptive_poll_spins(0)
            .create(&path)
            .expect("create isolated transport");
        let transport = SharedTransport::open(&path).unwrap();
        let mut runtime =
            BarRuntime::with_transport(ModelConfig::default(), Some(transport)).unwrap();
        runtime.apply_event(BarEvent::WindowManager(WmSnapshot {
            sequence: Some(1),
            monitor: MonitorId(0),
            geometry: Some(MonitorGeometry {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            }),
            layout_symbol: "[]=".into(),
            client_name: String::new(),
            tags: Vec::new(),
            ..WmSnapshot::default()
        }));
        owner.destroy().unwrap();

        let update = runtime.poll_transport();
        assert!(matches!(
            update.issues.as_slice(),
            [RuntimeIssue::AdapterFailed {
                adapter: RuntimeAdapter::Transport,
                operation: "drain_latest",
                ..
            }]
        ));
        assert!(runtime.transport().is_none());
        assert!(!runtime.view().wm_available);
        assert_eq!(
            update.platform_effects,
            vec![BarEffect::ClearMonitorGeometry]
        );
    }

    #[cfg(feature = "transport-shared")]
    #[test]
    fn command_failure_drops_transport_and_clears_wm_projection() {
        let sequence = NEXT_TRANSPORT_PATH.fetch_add(1, Ordering::Relaxed);
        let path = format!(
            "/tmp/xbar-core-runtime-command-{}-{sequence}",
            std::process::id()
        );
        let owner = shared_structures::SharedRingBufferOptions::new()
            .capacity(8)
            .adaptive_poll_spins(0)
            .create(&path)
            .expect("create isolated transport");
        let transport = SharedTransport::open(&path).unwrap();
        let mut runtime =
            BarRuntime::with_transport(ModelConfig::default(), Some(transport)).unwrap();
        runtime.apply_event(BarEvent::WindowManager(WmSnapshot {
            sequence: Some(1),
            monitor: MonitorId(0),
            geometry: Some(MonitorGeometry {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            }),
            layout_symbol: "[]=".into(),
            client_name: String::new(),
            tags: Vec::new(),
            ..WmSnapshot::default()
        }));
        owner.destroy().unwrap();

        let update = runtime.dispatch(UserAction::ViewTag(TagId::new(0).unwrap()));
        assert!(matches!(
            update.issues.as_slice(),
            [RuntimeIssue::AdapterFailed {
                adapter: RuntimeAdapter::Transport,
                operation: "execute",
                ..
            }]
        ));
        assert!(runtime.transport().is_none());
        assert!(!runtime.view().wm_available);
        assert_eq!(
            update.platform_effects,
            vec![BarEffect::ClearMonitorGeometry]
        );
    }

    #[cfg(feature = "clock-chrono")]
    #[test]
    fn clock_tick_feeds_both_model_formats() {
        let mut runtime = BarRuntime::new(ModelConfig {
            clock_minute_format: "minute:%M".into(),
            clock_second_format: "second:%S".into(),
            ..ModelConfig::default()
        })
        .unwrap();
        runtime.take_changes();
        let update = runtime.tick();

        assert!(update.changes.contains(DirtyBits::TIME_CHANGED));
        assert!(runtime.view().time.starts_with("minute:"));
        runtime.dispatch(UserAction::ToggleSeconds);
        assert!(runtime.view().time.starts_with("second:"));
    }

    #[cfg(feature = "clock-chrono")]
    #[test]
    fn invalid_clock_format_is_reported_without_panicking() {
        let mut runtime = BarRuntime::new(ModelConfig {
            clock_minute_format: "%".into(),
            ..ModelConfig::default()
        })
        .unwrap();

        let update = runtime.tick();
        assert!(update.issues.iter().any(|issue| matches!(
            issue,
            RuntimeIssue::AdapterFailed {
                adapter: RuntimeAdapter::Clock,
                operation: "format",
                ..
            }
        )));
    }

    #[cfg(feature = "provider-system")]
    #[test]
    fn invalid_provider_percent_becomes_an_explicit_issue() {
        let mut update = RuntimeUpdate::default();
        assert_eq!(
            provider_percent(f64::NAN, RuntimeAdapter::System, "cpu_percent", &mut update,),
            None
        );
        assert!(matches!(
            update.issues.as_slice(),
            [RuntimeIssue::InvalidProviderPercent {
                adapter: RuntimeAdapter::System,
                field: "cpu_percent",
                error: PercentError::NotFinite,
            }]
        ));
    }
}
