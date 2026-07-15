//! Feature-aware orchestration for the backend-neutral [`BarModel`].
//!
//! `BarRuntime` owns provider and transport adapters only when their Cargo
//! features are enabled.  The model remains the single source of semantic
//! state; adapters merely translate snapshots and execute effects.  Effects
//! that require a window/event-loop integration are returned to the frontend.

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
use crate::{SendOutcome, SharedTransport};
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
    Clock,
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

impl RuntimeUpdate {
    #[must_use]
    pub const fn needs_redraw(&self) -> bool {
        !self.changes.is_empty()
    }

    #[must_use]
    pub fn has_platform_work(&self) -> bool {
        !self.platform_effects.is_empty()
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

    #[cfg(feature = "transport-shared")]
    transport: Option<SharedTransport>,
    #[cfg(feature = "provider-alsa")]
    audio: crate::audio_manager::AudioManager,
    #[cfg(feature = "provider-system")]
    system: crate::system_monitor::SystemMonitor,
    #[cfg(feature = "provider-brightnessctl")]
    brightness: crate::brightness::BrightnessManager,
    #[cfg(feature = "provider-battery-sysfs")]
    battery: crate::battery::BatteryManager,
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
            #[cfg(feature = "transport-shared")]
            transport: None,
            #[cfg(feature = "provider-alsa")]
            audio: crate::audio_manager::AudioManager::new(),
            #[cfg(feature = "provider-system")]
            system: crate::system_monitor::SystemMonitor::new(5),
            #[cfg(feature = "provider-brightnessctl")]
            brightness: crate::brightness::BrightnessManager::new(),
            #[cfg(feature = "provider-battery-sysfs")]
            battery: crate::battery::BatteryManager::new(),
        })
    }

    #[cfg(feature = "transport-shared")]
    pub fn with_transport(
        config: ModelConfig,
        transport: Option<SharedTransport>,
    ) -> Result<Self, ModelError> {
        let mut runtime = Self::new(config)?;
        runtime.transport = transport;
        Ok(runtime)
    }

    #[cfg(feature = "transport-shared")]
    pub fn set_transport(&mut self, transport: Option<SharedTransport>) -> Option<SharedTransport> {
        std::mem::replace(&mut self.transport, transport)
    }

    #[cfg(feature = "transport-shared")]
    #[must_use]
    pub fn transport(&self) -> Option<&SharedTransport> {
        self.transport.as_ref()
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

        update
    }

    /// Drain the configured shared transport and reduce its newest WM
    /// snapshot. Without the feature or a configured transport this is a
    /// harmless no-op.
    pub fn poll_transport(&mut self) -> RuntimeUpdate {
        #[cfg(feature = "transport-shared")]
        {
            let result = match self.transport.as_ref() {
                Some(transport) => transport.drain_latest(),
                None => return RuntimeUpdate::default(),
            };

            match result {
                Ok(Some(snapshot)) => self.apply_event(BarEvent::WindowManager(snapshot)),
                Ok(None) => RuntimeUpdate::default(),
                Err(error) => {
                    self.transport = None;
                    let mut update = self.apply_event(BarEvent::WindowManagerUnavailable);
                    update.issues.push(RuntimeIssue::AdapterFailed {
                        adapter: RuntimeAdapter::Transport,
                        operation: "drain_latest",
                        message: error.to_string(),
                    });
                    update
                }
            }
        }

        #[cfg(not(feature = "transport-shared"))]
        RuntimeUpdate::default()
    }

    /// Return and clear all accumulated model changes, including changes from
    /// earlier operations whose individual [`RuntimeUpdate`] was discarded.
    pub fn take_changes(&mut self) -> DirtyBits {
        self.pending_changes.take()
    }

    fn consume_model_update(&mut self, update: ModelUpdate) -> RuntimeUpdate {
        let ModelUpdate { dirty, effects } = update;
        self.pending_changes |= dirty;

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
            let result = self
                .transport
                .as_ref()
                .expect("transport presence was checked")
                .execute(command);
            return match result {
                Ok(SendOutcome::Sent) => RuntimeUpdate::default(),
                Ok(SendOutcome::Full) => RuntimeUpdate::issue(RuntimeIssue::QueueFull { command }),
                Err(error) => {
                    self.transport = None;
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
    use std::sync::atomic::{AtomicU64, Ordering};

    #[cfg(feature = "transport-shared")]
    static NEXT_TRANSPORT_PATH: AtomicU64 = AtomicU64::new(0);

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
        let owner = shared_structures::SharedRingBuffer::create_aux(&path, Some(8), Some(0))
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
        let owner = shared_structures::SharedRingBuffer::create_aux(&path, Some(8), Some(0))
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
        let owner = shared_structures::SharedRingBuffer::create_aux(&path, Some(8), Some(0))
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
