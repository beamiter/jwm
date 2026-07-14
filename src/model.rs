//! Backend-independent bar model and reducer.
//!
//! Window-system frontends should translate native input into [`BarEvent`],
//! call [`BarModel::update`], render [`BarModel::view`], and execute the
//! returned [`BarEffect`] values in their platform/provider layer.  Nothing in
//! this module opens a window, touches ALSA/sysfs, starts a process, or writes
//! to shared memory.

use std::fmt;

use serde::{Deserialize, Serialize};
#[cfg(feature = "transport-shared")]
use shared_structures::{MonitorInfo, SharedCommand, SharedMessage};

use crate::{DirtyBits, ThemeMode};

/// The command protocol currently uses a 32-bit tag mask.
pub const MAX_MODEL_TAGS: usize = u32::BITS as usize;

/// A checked workspace/tag identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TagId(u8);

impl TagId {
    #[must_use]
    pub const fn new(index: usize) -> Option<Self> {
        if index < MAX_MODEL_TAGS {
            Some(Self(index as u8))
        } else {
            None
        }
    }

    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }

    #[must_use]
    pub const fn mask(self) -> u32 {
        1_u32 << self.0
    }
}

impl TryFrom<usize> for TagId {
    type Error = ModelError;

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        Self::new(value).ok_or(ModelError::TagOutOfRange {
            index: value,
            tag_count: MAX_MODEL_TAGS,
        })
    }
}

impl From<TagId> for usize {
    fn from(value: TagId) -> Self {
        value.index()
    }
}

/// Window-manager monitor identifier. Negative identifiers remain valid so
/// transports can preserve an upstream sentinel value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MonitorId(pub i32);

/// Window-manager layout identifier.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LayoutId(pub u32);

/// Logical monitor placement supplied by the window-manager transport.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorGeometry {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl MonitorGeometry {
    #[must_use]
    pub const fn from_raw(x: i32, y: i32, width: i32, height: i32) -> Option<Self> {
        if width > 0 && height > 0 {
            Some(Self {
                x,
                y,
                width: width as u32,
                height: height as u32,
            })
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagState {
    pub selected: bool,
    pub urgent: bool,
    pub filled: bool,
    pub occupied: bool,
}

#[cfg(feature = "transport-shared")]
impl From<shared_structures::TagStatus> for TagState {
    fn from(value: shared_structures::TagStatus) -> Self {
        Self {
            selected: value.is_selected,
            urgent: value.is_urg,
            filled: value.is_filled,
            occupied: value.is_occ,
        }
    }
}

/// Transport-neutral snapshot received from a window manager.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WmSnapshot {
    /// Optional transport sequence/timestamp used only to suppress duplicates.
    pub sequence: Option<u64>,
    pub monitor: MonitorId,
    pub geometry: Option<MonitorGeometry>,
    pub layout_symbol: String,
    pub client_name: String,
    pub tags: Vec<TagState>,
}

#[cfg(feature = "transport-shared")]
impl From<MonitorInfo> for WmSnapshot {
    fn from(info: MonitorInfo) -> Self {
        Self {
            sequence: None,
            monitor: MonitorId(info.monitor_num),
            geometry: MonitorGeometry::from_raw(
                info.monitor_x,
                info.monitor_y,
                info.monitor_width,
                info.monitor_height,
            ),
            layout_symbol: info.ltsymbol_lossy().into_owned(),
            client_name: info.client_name_lossy().into_owned(),
            tags: info.tag_status_vec.into_iter().map(Into::into).collect(),
        }
    }
}

#[cfg(feature = "transport-shared")]
impl From<SharedMessage> for WmSnapshot {
    fn from(message: SharedMessage) -> Self {
        let mut snapshot = Self::from(message.monitor_info);
        snapshot.sequence = Some(message.timestamp);
        snapshot
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClockState {
    /// Preformatted text without seconds, supplied by the clock adapter.
    pub minute: String,
    /// Preformatted text with seconds, supplied by the clock adapter.
    pub second: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioState {
    pub volume_percent: Option<u8>,
    pub muted: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct SystemState {
    pub cpu_percent: Option<f32>,
    pub memory_percent: Option<f32>,
}

impl SystemState {
    #[must_use]
    pub fn normalized(mut self) -> Self {
        self.cpu_percent = self.cpu_percent.map(|value| value.clamp(0.0, 100.0));
        self.memory_percent = self.memory_percent.map(|value| value.clamp(0.0, 100.0));
        self
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrightnessState {
    pub percent: Option<u8>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatteryState {
    pub percent: Option<u8>,
    pub charging: bool,
    pub present: bool,
}

/// Runtime-neutral behavior configuration. Visual labels and layout belong to
/// the renderer configuration, not to the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelConfig {
    pub tag_count: usize,
    pub volume_step: u8,
    pub brightness_step: u8,
    pub initial_theme: ThemeMode,
    pub show_seconds: bool,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            tag_count: 9,
            volume_step: 5,
            brightness_step: 5,
            initial_theme: ThemeMode::Dark,
            show_seconds: false,
        }
    }
}

impl ModelConfig {
    pub fn validate(&self) -> Result<(), ModelError> {
        if !(1..=MAX_MODEL_TAGS).contains(&self.tag_count) {
            return Err(ModelError::InvalidTagCount(self.tag_count));
        }
        if self.volume_step == 0 || self.volume_step > 100 {
            return Err(ModelError::InvalidPercentageStep {
                field: "volume_step",
                value: self.volume_step,
            });
        }
        if self.brightness_step == 0 || self.brightness_step > 100 {
            return Err(ModelError::InvalidPercentageStep {
                field: "brightness_step",
                value: self.brightness_step,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelError {
    InvalidTagCount(usize),
    InvalidPercentageStep { field: &'static str, value: u8 },
    TagOutOfRange { index: usize, tag_count: usize },
}

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTagCount(value) => write!(
                f,
                "tag_count must be between 1 and {MAX_MODEL_TAGS}, got {value}"
            ),
            Self::InvalidPercentageStep { field, value } => {
                write!(f, "{field} must be between 1 and 100, got {value}")
            }
            Self::TagOutOfRange { index, tag_count } => {
                write!(
                    f,
                    "tag index {index} is outside configured range 0..{tag_count}"
                )
            }
        }
    }
}

impl std::error::Error for ModelError {}

/// Events accepted by the pure reducer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BarEvent {
    WindowManager(WmSnapshot),
    Clock(ClockState),
    Audio(AudioState),
    System(SystemState),
    Brightness(BrightnessState),
    Battery(BatteryState),
    User(UserAction),
}

/// Semantic user intent; native button/key enums are translated by a frontend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UserAction {
    ViewTag(TagId),
    ToggleTag(TagId),
    ToggleLayoutSelector,
    SetLayout(LayoutId),
    ToggleSeconds,
    ToggleTheme,
    ToggleMute,
    VolumeUp,
    VolumeDown,
    AdjustVolume(i32),
    BrightnessUp,
    BrightnessDown,
    AdjustBrightness(i32),
    RefreshBattery,
    Screenshot,
    OpenAudioControl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WmCommand {
    ViewTag {
        tag: TagId,
        monitor: MonitorId,
    },
    ToggleTag {
        tag: TagId,
        monitor: MonitorId,
    },
    SetLayout {
        layout: LayoutId,
        monitor: MonitorId,
    },
}

impl WmCommand {
    /// Compatibility bridge for the current JWM shared-memory transport.
    #[cfg(feature = "transport-shared")]
    #[must_use]
    pub fn into_shared_command(self) -> SharedCommand {
        match self {
            Self::ViewTag { tag, monitor } => SharedCommand::view_tag(tag.mask(), monitor.0),
            Self::ToggleTag { tag, monitor } => SharedCommand::toggle_tag(tag.mask(), monitor.0),
            Self::SetLayout { layout, monitor } => SharedCommand::set_layout(layout.0, monitor.0),
        }
    }
}

/// Side effects described by the core and executed by an adapter/provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BarEffect {
    WindowManager(WmCommand),
    ApplyMonitorGeometry(MonitorGeometry),
    ToggleMute,
    AdjustVolume(i32),
    AdjustBrightness(i32),
    RefreshBattery,
    Screenshot,
    OpenAudioControl,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelUpdate {
    pub dirty: DirtyBits,
    pub effects: Vec<BarEffect>,
}

impl ModelUpdate {
    #[must_use]
    pub fn needs_redraw(&self) -> bool {
        !self.dirty.is_empty()
    }

    pub fn merge(&mut self, mut other: Self) {
        self.dirty |= other.dirty;
        self.effects.append(&mut other.effects);
    }
}

/// Read-only projection consumed by any renderer (Cairo, wgpu, egui, GTK,
/// HTML, and so on).
#[derive(Debug, Clone, Copy)]
pub struct BarView<'a> {
    pub tags: &'a [TagState],
    pub active_tag: Option<TagId>,
    pub monitor: MonitorId,
    pub geometry: Option<MonitorGeometry>,
    pub layout_symbol: &'a str,
    pub client_name: &'a str,
    pub time: &'a str,
    pub show_seconds: bool,
    pub layout_selector_open: bool,
    pub theme: ThemeMode,
    pub audio: AudioState,
    pub system: SystemState,
    pub brightness: BrightnessState,
    pub battery: BatteryState,
}

/// Canonical backend-independent model. All fields are private so invariants
/// remain stable as additional frontends adopt it.
#[derive(Debug, Clone)]
pub struct BarModel {
    config: ModelConfig,
    tags: Vec<TagState>,
    active_tag: Option<TagId>,
    monitor: MonitorId,
    geometry: Option<MonitorGeometry>,
    layout_symbol: String,
    client_name: String,
    clock: ClockState,
    show_seconds: bool,
    layout_selector_open: bool,
    theme: ThemeMode,
    audio: AudioState,
    system: SystemState,
    brightness: BrightnessState,
    battery: BatteryState,
    last_wm_sequence: Option<u64>,
}

impl Default for BarModel {
    fn default() -> Self {
        Self::new(ModelConfig::default()).expect("default model config is valid")
    }
}

impl BarModel {
    pub fn new(config: ModelConfig) -> Result<Self, ModelError> {
        config.validate()?;
        Ok(Self {
            tags: vec![TagState::default(); config.tag_count],
            active_tag: None,
            monitor: MonitorId::default(),
            geometry: None,
            layout_symbol: "[]=".to_owned(),
            client_name: String::new(),
            clock: ClockState::default(),
            show_seconds: config.show_seconds,
            layout_selector_open: false,
            theme: config.initial_theme,
            audio: AudioState::default(),
            system: SystemState::default(),
            brightness: BrightnessState::default(),
            battery: BatteryState::default(),
            last_wm_sequence: None,
            config,
        })
    }

    #[must_use]
    pub fn config(&self) -> &ModelConfig {
        &self.config
    }

    #[must_use]
    pub fn view(&self) -> BarView<'_> {
        BarView {
            tags: &self.tags,
            active_tag: self.active_tag,
            monitor: self.monitor,
            geometry: self.geometry,
            layout_symbol: &self.layout_symbol,
            client_name: &self.client_name,
            time: if self.show_seconds {
                &self.clock.second
            } else {
                &self.clock.minute
            },
            show_seconds: self.show_seconds,
            layout_selector_open: self.layout_selector_open,
            theme: self.theme,
            audio: self.audio,
            system: self.system,
            brightness: self.brightness,
            battery: self.battery,
        }
    }

    /// Apply an event and return both visual damage and typed side effects.
    pub fn update(&mut self, event: BarEvent) -> Result<ModelUpdate, ModelError> {
        match event {
            BarEvent::WindowManager(snapshot) => Ok(self.update_wm(snapshot)),
            BarEvent::Clock(clock) => Ok(self.update_clock(clock)),
            BarEvent::Audio(audio) => Ok(self.replace_audio(audio)),
            BarEvent::System(system) => Ok(self.replace_system(system.normalized())),
            BarEvent::Brightness(brightness) => Ok(self.replace_brightness(brightness)),
            BarEvent::Battery(battery) => Ok(self.replace_battery(battery)),
            BarEvent::User(action) => self.update_user(action),
        }
    }

    /// Compatibility bridge used while the current shared-memory frontends
    /// migrate to transport-neutral [`WmSnapshot`] values.
    #[cfg(feature = "transport-shared")]
    pub fn update_from_shared(
        &mut self,
        message: SharedMessage,
    ) -> Result<ModelUpdate, ModelError> {
        self.update(BarEvent::WindowManager(message.into()))
    }

    fn update_wm(&mut self, mut snapshot: WmSnapshot) -> ModelUpdate {
        if snapshot.sequence.is_some() && snapshot.sequence == self.last_wm_sequence {
            return ModelUpdate::default();
        }

        snapshot
            .tags
            .resize(self.config.tag_count, TagState::default());
        snapshot.tags.truncate(self.config.tag_count);

        let mut dirty = DirtyBits::default();
        let next_active = snapshot
            .tags
            .iter()
            .position(|tag| tag.selected)
            .and_then(TagId::new);

        if self.tags != snapshot.tags
            || self.active_tag != next_active
            || self.monitor != snapshot.monitor
        {
            dirty.set(DirtyBits::MONITOR_CHANGED);
        }
        if self.layout_symbol != snapshot.layout_symbol {
            dirty.set(DirtyBits::LAYOUT_CHANGED);
        }
        if self.client_name != snapshot.client_name {
            dirty.set(DirtyBits::CLIENT_CHANGED);
        }
        let geometry_changed = self.geometry != snapshot.geometry;
        if geometry_changed {
            dirty.set(DirtyBits::GEOMETRY_CHANGED);
        }

        self.tags = snapshot.tags;
        self.active_tag = next_active;
        self.monitor = snapshot.monitor;
        self.geometry = snapshot.geometry;
        self.layout_symbol = snapshot.layout_symbol;
        self.client_name = snapshot.client_name;
        self.last_wm_sequence = snapshot.sequence;

        ModelUpdate {
            dirty,
            effects: if geometry_changed {
                self.geometry
                    .map(BarEffect::ApplyMonitorGeometry)
                    .into_iter()
                    .collect()
            } else {
                Vec::new()
            },
        }
    }

    fn update_clock(&mut self, clock: ClockState) -> ModelUpdate {
        let old_display = if self.show_seconds {
            &self.clock.second
        } else {
            &self.clock.minute
        };
        let new_display = if self.show_seconds {
            &clock.second
        } else {
            &clock.minute
        };
        let changed = old_display != new_display;
        self.clock = clock;
        Self::changed(DirtyBits::TIME_CHANGED, changed)
    }

    fn replace_audio(&mut self, mut audio: AudioState) -> ModelUpdate {
        audio.volume_percent = audio.volume_percent.map(|value| value.min(100));
        let changed = self.audio != audio;
        self.audio = audio;
        Self::changed(DirtyBits::AUDIO_CHANGED, changed)
    }

    fn replace_system(&mut self, system: SystemState) -> ModelUpdate {
        let changed = self.system != system;
        self.system = system;
        Self::changed(DirtyBits::SYSTEM_CHANGED, changed)
    }

    fn replace_brightness(&mut self, mut brightness: BrightnessState) -> ModelUpdate {
        brightness.percent = brightness.percent.map(|value| value.min(100));
        let changed = self.brightness != brightness;
        self.brightness = brightness;
        Self::changed(DirtyBits::BRIGHTNESS_CHANGED, changed)
    }

    fn replace_battery(&mut self, mut battery: BatteryState) -> ModelUpdate {
        battery.percent = battery.percent.map(|value| value.min(100));
        if !battery.present {
            battery.percent = None;
            battery.charging = false;
        }
        let changed = self.battery != battery;
        self.battery = battery;
        Self::changed(DirtyBits::BATTERY_CHANGED, changed)
    }

    fn update_user(&mut self, action: UserAction) -> Result<ModelUpdate, ModelError> {
        let mut update = ModelUpdate::default();
        match action {
            UserAction::ViewTag(tag) => {
                self.ensure_configured_tag(tag)?;
                for (index, state) in self.tags.iter_mut().enumerate() {
                    state.selected = index == tag.index();
                }
                self.active_tag = Some(tag);
                update.dirty.set(DirtyBits::MONITOR_CHANGED);
                update
                    .effects
                    .push(BarEffect::WindowManager(WmCommand::ViewTag {
                        tag,
                        monitor: self.monitor,
                    }));
            }
            UserAction::ToggleTag(tag) => {
                self.ensure_configured_tag(tag)?;
                update
                    .effects
                    .push(BarEffect::WindowManager(WmCommand::ToggleTag {
                        tag,
                        monitor: self.monitor,
                    }));
            }
            UserAction::ToggleLayoutSelector => {
                self.layout_selector_open = !self.layout_selector_open;
                update.dirty.set(DirtyBits::LAYOUT_CHANGED);
            }
            UserAction::SetLayout(layout) => {
                self.layout_selector_open = false;
                update.dirty.set(DirtyBits::LAYOUT_CHANGED);
                update
                    .effects
                    .push(BarEffect::WindowManager(WmCommand::SetLayout {
                        layout,
                        monitor: self.monitor,
                    }));
            }
            UserAction::ToggleSeconds => {
                self.show_seconds = !self.show_seconds;
                update.dirty.set(DirtyBits::TIME_CHANGED);
            }
            UserAction::ToggleTheme => {
                self.theme = match self.theme {
                    ThemeMode::Dark => ThemeMode::Light,
                    ThemeMode::Light => ThemeMode::Dark,
                };
                update.dirty.set(DirtyBits::THEME_CHANGED);
            }
            UserAction::ToggleMute => {
                if self.audio.volume_percent.is_some() {
                    self.audio.muted = !self.audio.muted;
                    update.dirty.set(DirtyBits::AUDIO_CHANGED);
                }
                update.effects.push(BarEffect::ToggleMute);
            }
            UserAction::VolumeUp => {
                self.adjust_volume(self.config.volume_step as i32, &mut update);
            }
            UserAction::VolumeDown => {
                self.adjust_volume(-(self.config.volume_step as i32), &mut update);
            }
            UserAction::AdjustVolume(delta) => self.adjust_volume(delta, &mut update),
            UserAction::BrightnessUp => {
                self.adjust_brightness(self.config.brightness_step as i32, &mut update);
            }
            UserAction::BrightnessDown => {
                self.adjust_brightness(-(self.config.brightness_step as i32), &mut update);
            }
            UserAction::AdjustBrightness(delta) => {
                self.adjust_brightness(delta, &mut update);
            }
            UserAction::RefreshBattery => update.effects.push(BarEffect::RefreshBattery),
            UserAction::Screenshot => update.effects.push(BarEffect::Screenshot),
            UserAction::OpenAudioControl => update.effects.push(BarEffect::OpenAudioControl),
        }
        Ok(update)
    }

    fn ensure_configured_tag(&self, tag: TagId) -> Result<(), ModelError> {
        if tag.index() < self.config.tag_count {
            Ok(())
        } else {
            Err(ModelError::TagOutOfRange {
                index: tag.index(),
                tag_count: self.config.tag_count,
            })
        }
    }

    fn adjust_volume(&mut self, delta: i32, update: &mut ModelUpdate) {
        if delta == 0 {
            return;
        }
        if let Some(value) = self.audio.volume_percent {
            let next = (value as i32 + delta).clamp(0, 100) as u8;
            if next != value {
                self.audio.volume_percent = Some(next);
                update.dirty.set(DirtyBits::AUDIO_CHANGED);
            }
        }
        update.effects.push(BarEffect::AdjustVolume(delta));
    }

    fn adjust_brightness(&mut self, delta: i32, update: &mut ModelUpdate) {
        if delta == 0 {
            return;
        }
        if let Some(value) = self.brightness.percent {
            let next = (value as i32 + delta).clamp(0, 100) as u8;
            if next != value {
                self.brightness.percent = Some(next);
                update.dirty.set(DirtyBits::BRIGHTNESS_CHANGED);
            }
        }
        update.effects.push(BarEffect::AdjustBrightness(delta));
    }

    fn changed(flag: u32, changed: bool) -> ModelUpdate {
        let mut dirty = DirtyBits::default();
        if changed {
            dirty.set(flag);
        }
        ModelUpdate {
            dirty,
            effects: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(index: usize) -> TagId {
        TagId::new(index).unwrap()
    }

    #[test]
    fn model_config_rejects_invalid_values() {
        assert!(matches!(
            BarModel::new(ModelConfig {
                tag_count: 0,
                ..ModelConfig::default()
            }),
            Err(ModelError::InvalidTagCount(0))
        ));
        assert!(matches!(
            BarModel::new(ModelConfig {
                volume_step: 0,
                ..ModelConfig::default()
            }),
            Err(ModelError::InvalidPercentageStep {
                field: "volume_step",
                value: 0
            })
        ));
    }

    #[test]
    fn wm_snapshot_updates_model_and_suppresses_duplicate_sequence() {
        let mut model = BarModel::default();
        let mut tags = vec![TagState::default(); 9];
        tags[2].selected = true;
        let snapshot = WmSnapshot {
            sequence: Some(7),
            monitor: MonitorId(3),
            geometry: Some(MonitorGeometry {
                x: 1920,
                y: 0,
                width: 1920,
                height: 1080,
            }),
            layout_symbol: "[M]".into(),
            client_name: "terminal".into(),
            tags,
        };

        let first = model
            .update(BarEvent::WindowManager(snapshot.clone()))
            .unwrap();
        assert!(first.dirty.contains(DirtyBits::MONITOR_CHANGED));
        assert!(first.dirty.contains(DirtyBits::LAYOUT_CHANGED));
        assert!(first.dirty.contains(DirtyBits::CLIENT_CHANGED));
        assert!(first.dirty.contains(DirtyBits::GEOMETRY_CHANGED));
        assert_eq!(model.view().active_tag, Some(tag(2)));
        assert_eq!(model.view().monitor, MonitorId(3));
        assert_eq!(model.view().geometry.unwrap().x, 1920);

        let duplicate = model.update(BarEvent::WindowManager(snapshot)).unwrap();
        assert!(!duplicate.needs_redraw());
    }

    #[test]
    fn tag_action_is_checked_and_emits_transport_neutral_command() {
        let mut model = BarModel::new(ModelConfig {
            tag_count: 3,
            ..ModelConfig::default()
        })
        .unwrap();

        let update = model
            .update(BarEvent::User(UserAction::ViewTag(tag(2))))
            .unwrap();
        assert_eq!(model.view().active_tag, Some(tag(2)));
        assert_eq!(
            update.effects,
            vec![BarEffect::WindowManager(WmCommand::ViewTag {
                tag: tag(2),
                monitor: MonitorId(0),
            })]
        );

        assert!(matches!(
            model.update(BarEvent::User(UserAction::ViewTag(tag(3)))),
            Err(ModelError::TagOutOfRange {
                index: 3,
                tag_count: 3
            })
        ));
    }

    #[test]
    fn configured_steps_drive_optimistic_provider_updates() {
        let mut model = BarModel::new(ModelConfig {
            volume_step: 7,
            brightness_step: 9,
            ..ModelConfig::default()
        })
        .unwrap();
        model
            .update(BarEvent::Audio(AudioState {
                volume_percent: Some(98),
                muted: false,
            }))
            .unwrap();
        model
            .update(BarEvent::Brightness(BrightnessState { percent: Some(4) }))
            .unwrap();

        let volume = model.update(BarEvent::User(UserAction::VolumeUp)).unwrap();
        let brightness = model
            .update(BarEvent::User(UserAction::BrightnessDown))
            .unwrap();

        assert_eq!(model.view().audio.volume_percent, Some(100));
        assert_eq!(model.view().brightness.percent, Some(0));
        assert_eq!(volume.effects, vec![BarEffect::AdjustVolume(7)]);
        assert_eq!(brightness.effects, vec![BarEffect::AdjustBrightness(-9)]);
    }

    #[test]
    fn clock_adapter_supplies_both_formats_for_instant_toggle() {
        let mut model = BarModel::default();
        model
            .update(BarEvent::Clock(ClockState {
                minute: "12:34".into(),
                second: "12:34:56".into(),
            }))
            .unwrap();
        assert_eq!(model.view().time, "12:34");

        model
            .update(BarEvent::User(UserAction::ToggleSeconds))
            .unwrap();
        assert_eq!(model.view().time, "12:34:56");
    }

    #[cfg(feature = "transport-shared")]
    #[test]
    fn wm_commands_bridge_to_current_shared_protocol() {
        let command = WmCommand::ToggleTag {
            tag: tag(4),
            monitor: MonitorId(2),
        }
        .into_shared_command();
        assert_eq!(command.get_parameter(), 1 << 4);
        assert_eq!(command.get_monitor_id(), 2);
        assert_eq!(
            command.get_command_type(),
            shared_structures::CommandType::ToggleTag
        );
    }
}
