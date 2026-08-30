//! Backend-independent bar model and reducer.
//!
//! Window-system frontends should translate native input into [`BarEvent`],
//! call [`BarModel::update`], render [`BarModel::view`], and execute the
//! returned [`BarEffect`] values in their platform/provider layer.  Nothing in
//! this module opens a window, touches ALSA/sysfs, starts a process, or writes
//! to shared memory.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{DirtyBits, ThemeMode};

/// The command protocol currently uses a 32-bit tag mask.
pub const MAX_MODEL_TAGS: usize = u32::BITS as usize;

/// Longest accepted chrono format retained and cloned by the clock adapter.
pub const MAX_MODEL_CLOCK_FORMAT_BYTES: usize = 1_024;

/// Maximum number of per-CPU samples retained in a model snapshot.
///
/// This matches Linux's architectural CPU ceiling while keeping an untrusted
/// provider from making every cloned snapshot retain an arbitrarily large
/// allocation.
pub const MAX_MODEL_CPU_SAMPLES: usize = 8_192;

/// A finite percentage in the inclusive `0..=100` range.
///
/// Values are stored as basis points (one hundredth of a percent).  This keeps
/// equality deterministic for change detection while still preserving enough
/// precision for CPU and memory measurements.  Serialization uses the human
/// percentage value rather than the internal representation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Percent(u16);

impl Percent {
    const BASIS_POINTS_PER_PERCENT: f64 = 100.0;
    const MAX_BASIS_POINTS: u16 = 10_000;

    /// Validate and quantize a percentage to one hundredth of a percent.
    pub fn new(value: f64) -> Result<Self, PercentError> {
        if !value.is_finite() {
            return Err(PercentError::NotFinite);
        }
        if !(0.0..=100.0).contains(&value) {
            return Err(PercentError::OutOfRange);
        }

        let basis_points = (value * Self::BASIS_POINTS_PER_PERCENT).round() as u16;
        Ok(Self(basis_points.min(Self::MAX_BASIS_POINTS)))
    }

    /// Construct an integral percentage without floating-point conversion.
    pub const fn from_whole(value: u8) -> Result<Self, PercentError> {
        if value <= 100 {
            Ok(Self(value as u16 * 100))
        } else {
            Err(PercentError::OutOfRange)
        }
    }

    #[must_use]
    pub const fn basis_points(self) -> u16 {
        self.0
    }

    #[must_use]
    pub fn as_f64(self) -> f64 {
        f64::from(self.0) / Self::BASIS_POINTS_PER_PERCENT
    }

    #[must_use]
    pub fn as_f32(self) -> f32 {
        self.as_f64() as f32
    }

    /// Return the nearest integral percentage for compact text displays.
    #[must_use]
    pub const fn rounded(self) -> u8 {
        ((self.0 + 50) / 100) as u8
    }
}

impl fmt::Display for Percent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_f64())
    }
}

impl Serialize for Percent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_f64(self.as_f64())
    }
}

impl<'de> Deserialize<'de> for Percent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = f64::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

impl TryFrom<u8> for Percent {
    type Error = PercentError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Self::from_whole(value)
    }
}

impl TryFrom<f32> for Percent {
    type Error = PercentError;

    fn try_from(value: f32) -> Result<Self, Self::Error> {
        Self::new(f64::from(value))
    }
}

impl TryFrom<f64> for Percent {
    type Error = PercentError;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<Percent> for f32 {
    fn from(value: Percent) -> Self {
        value.as_f32()
    }
}

impl From<Percent> for f64 {
    fn from(value: Percent) -> Self {
        value.as_f64()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PercentError {
    NotFinite,
    OutOfRange,
}

impl fmt::Display for PercentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFinite => f.write_str("percentage must be finite"),
            Self::OutOfRange => f.write_str("percentage must be between 0 and 100"),
        }
    }
}

impl std::error::Error for PercentError {}

/// A checked workspace/tag identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
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

impl<'de> Deserialize<'de> for TagId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let index = u8::deserialize(deserializer)?;
        Self::new(usize::from(index)).ok_or_else(|| {
            serde::de::Error::custom(format_args!(
                "tag index must be between 0 and {}, got {index}",
                MAX_MODEL_TAGS - 1
            ))
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

/// Stable window identity supplied by one window-manager session.
///
/// The opaque value is always paired with [`WmSnapshot::wm_session_id`] when
/// it crosses back to the window manager. This prevents a restarted manager
/// from accepting an id that it has already reused for a different window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WindowToken(pub u64);

impl WindowToken {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Global physical target rectangle used by minimize/restore animations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockItemGeometry {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl DockItemGeometry {
    #[must_use]
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// A compositor-side thumbnail can be requested for this item.
pub const MINIMIZED_WINDOW_FLAG_PREVIEW_AVAILABLE: u32 = 1 << 0;
/// Urgency bit retained from the shared minimized-window protocol.
pub const MINIMIZED_WINDOW_FLAG_URGENT: u32 = 1 << 1;
/// Protocol-aligned upper bound retained even without the shared feature.
pub const MAX_MODEL_MINIMIZED_WINDOWS: usize = 16;
const MAX_WM_MINIMIZED_INPUTS: usize = MAX_MODEL_MINIMIZED_WINDOWS * 16;
const MAX_MODEL_LAYOUT_SYMBOL_BYTES: usize = 64;
const MAX_MODEL_DISPLAY_TEXT_BYTES: usize = 4 * 1024;
const MAX_MODEL_ID_BYTES: usize = 255;

/// One window currently collected by the bar's minimized-window shelf.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MinimizedWindow {
    pub token: WindowToken,
    pub monitor: MonitorId,
    pub title: String,
    pub app_id: String,
    #[serde(default)]
    pub flags: u32,
}

impl MinimizedWindow {
    #[must_use]
    pub const fn urgent(&self) -> bool {
        self.flags & MINIMIZED_WINDOW_FLAG_URGENT != 0
    }

    #[must_use]
    pub const fn preview_available(&self) -> bool {
        self.flags & MINIMIZED_WINDOW_FLAG_PREVIEW_AVAILABLE != 0
    }

    /// Compact fallback glyph for scene renderers without an icon provider.
    #[must_use]
    pub fn initial(&self) -> char {
        self.app_id
            .trim()
            .chars()
            .next()
            .or_else(|| self.title.trim().chars().next())
            .unwrap_or('?')
            .to_uppercase()
            .next()
            .unwrap_or('?')
    }
}

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

/// Transport-neutral snapshot received from a window manager.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WmSnapshot {
    /// Per-monitor minimized-projection epoch when the transport provides it.
    /// Non-Dock fields are still reduced by value even when this stays equal.
    pub sequence: Option<u64>,
    /// Random identity of the producing WM lifetime. Zero is the legacy
    /// fallback used by snapshots serialized before the Dock protocol.
    #[serde(default)]
    pub wm_session_id: u64,
    pub monitor: MonitorId,
    pub geometry: Option<MonitorGeometry>,
    pub layout_symbol: String,
    /// Layout in use, as the window manager identifies it on the wire.
    #[serde(default)]
    pub layout: Option<LayoutId>,
    /// How many layouts the window manager offers. `None` when it did not say,
    /// which leaves the bar on its own compiled catalog.
    #[serde(default)]
    pub layout_count: Option<usize>,
    pub client_name: String,
    /// Wayland app-id or X11 class of the focused window, empty when unknown.
    #[serde(default)]
    pub client_app_id: String,
    pub tags: Vec<TagState>,
    #[serde(default)]
    pub minimized_windows: Vec<MinimizedWindow>,
    #[serde(default)]
    pub minimized_overflow: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClockState {
    /// Preformatted text without seconds, supplied by the clock adapter.
    pub minute: String,
    /// Preformatted text with seconds, supplied by the clock adapter.
    pub second: String,
}

impl ClockState {
    #[must_use]
    pub fn normalized(mut self) -> Self {
        self.minute = bounded_model_string(self.minute, MAX_MODEL_DISPLAY_TEXT_BYTES);
        self.second = bounded_model_string(self.second, MAX_MODEL_DISPLAY_TEXT_BYTES);
        self
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct AudioState {
    pub volume_percent: Option<Percent>,
    pub muted: bool,
}

impl<'de> Deserialize<'de> for AudioState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireState {
            volume_percent: Option<Percent>,
            muted: bool,
        }

        let wire = WireState::deserialize(deserializer)?;
        Ok(Self::new(wire.volume_percent, wire.muted))
    }
}

impl AudioState {
    #[must_use]
    pub const fn new(volume_percent: Option<Percent>, muted: bool) -> Self {
        Self {
            volume_percent,
            muted: muted && volume_percent.is_some(),
        }
    }

    pub fn from_f64(volume_percent: Option<f64>, muted: bool) -> Result<Self, PercentError> {
        Ok(Self::new(
            volume_percent.map(Percent::new).transpose()?,
            muted,
        ))
    }

    #[must_use]
    pub const fn normalized(self) -> Self {
        Self::new(self.volume_percent, self.muted)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemState {
    pub cpu_percent: Option<Percent>,
    pub memory_percent: Option<Percent>,
}

impl SystemState {
    #[must_use]
    pub const fn new(cpu_percent: Option<Percent>, memory_percent: Option<Percent>) -> Self {
        Self {
            cpu_percent,
            memory_percent,
        }
    }

    pub fn from_f64(
        cpu_percent: Option<f64>,
        memory_percent: Option<f64>,
    ) -> Result<Self, PercentError> {
        Ok(Self::new(
            cpu_percent.map(Percent::new).transpose()?,
            memory_percent.map(Percent::new).transpose()?,
        ))
    }

    #[must_use]
    pub const fn normalized(self) -> Self {
        self
    }
}

/// Transport-neutral metadata for the audio device selected by the provider.
///
/// Presence is represented by `Option<AudioDeviceInfo>` on [`BarView`] and
/// [`BarSnapshot`]. Volume and mute remain in [`AudioState`] so renderers that
/// only need the compact status do not have to inspect provider metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioDeviceInfo {
    pub name: String,
    pub index: usize,
    pub volume: i32,
    pub is_muted: bool,
    pub description: String,
    pub has_volume_control: bool,
    pub has_switch_control: bool,
}

impl AudioDeviceInfo {
    /// Bound provider-owned labels before they become part of every frontend
    /// snapshot and normalize the percentage-like volume field.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        self.name = bounded_model_string(self.name, MAX_MODEL_ID_BYTES);
        self.description = bounded_model_string(self.description, MAX_MODEL_DISPLAY_TEXT_BYTES);
        self.volume = self.volume.clamp(0, 100);
        self
    }
}

/// Transport-neutral system load averages.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct SystemLoadAverage {
    pub one_minute: f64,
    pub five_minutes: f64,
    pub fifteen_minutes: f64,
}

/// Rich system telemetry retained by the model for toolkit and web frontends.
///
/// The compact, validated percentages used by renderers stay in
/// [`SystemState`]. This projection preserves provider detail that cannot be
/// reconstructed from those percentages, including per-core samples and
/// exact memory counters.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SystemDetails {
    pub cpu_usage: Vec<f32>,
    pub cpu_average: f32,
    pub memory_total: u64,
    pub memory_used: u64,
    pub memory_available: u64,
    pub memory_usage_percent: f32,
    pub uptime: u64,
    pub load_average: SystemLoadAverage,
}

impl SystemDetails {
    /// Return telemetry with finite, bounded values suitable for long-lived
    /// model snapshots and deterministic change detection.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        self.cpu_usage = self
            .cpu_usage
            .into_iter()
            .take(MAX_MODEL_CPU_SAMPLES)
            .map(normalized_percent)
            .collect();
        self.cpu_average = normalized_percent(self.cpu_average);
        self.memory_used = self.memory_used.min(self.memory_total);
        self.memory_available = self.memory_available.min(self.memory_total);
        self.memory_usage_percent = normalized_percent(self.memory_usage_percent);
        self.load_average = SystemLoadAverage {
            one_minute: normalized_load(self.load_average.one_minute),
            five_minutes: normalized_load(self.load_average.five_minutes),
            fifteen_minutes: normalized_load(self.load_average.fifteen_minutes),
        };
        self
    }
}

fn normalized_percent(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 100.0)
    } else {
        0.0
    }
}

fn normalized_load(value: f64) -> f64 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrightnessState {
    pub percent: Option<Percent>,
}

impl BrightnessState {
    #[must_use]
    pub const fn new(percent: Option<Percent>) -> Self {
        Self { percent }
    }

    pub fn from_f64(percent: Option<f64>) -> Result<Self, PercentError> {
        Ok(Self::new(percent.map(Percent::new).transpose()?))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct BatteryState {
    pub percent: Option<Percent>,
    pub charging: bool,
    pub present: bool,
}

impl<'de> Deserialize<'de> for BatteryState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireState {
            percent: Option<Percent>,
            charging: bool,
            present: bool,
        }

        let wire = WireState::deserialize(deserializer)?;
        Ok(if wire.present {
            Self::present(wire.percent, wire.charging)
        } else {
            Self::absent()
        })
    }
}

impl BatteryState {
    #[must_use]
    pub const fn absent() -> Self {
        Self {
            percent: None,
            charging: false,
            present: false,
        }
    }

    #[must_use]
    pub const fn present(percent: Option<Percent>, charging: bool) -> Self {
        Self {
            percent,
            charging,
            present: true,
        }
    }

    pub fn from_f64(
        percent: Option<f64>,
        charging: bool,
        present: bool,
    ) -> Result<Self, PercentError> {
        let percent = percent.map(Percent::new).transpose()?;
        Ok(if present {
            Self::present(percent, charging)
        } else {
            Self::absent()
        })
    }

    #[must_use]
    pub const fn normalized(self) -> Self {
        if self.present {
            Self::present(self.percent, self.charging)
        } else {
            Self::absent()
        }
    }
}

/// Aggregate throughput of the host's primary network interface.
///
/// Rates are unavailable (`None`) until a provider has observed two samples;
/// they are never displayed as a healthy zero.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct NetworkState {
    /// Primary interface name; `None` while disconnected.
    pub interface: Option<String>,
    pub connected: bool,
    /// Receive rate in bytes per second over the previous sample window.
    pub rx_bytes_per_second: Option<u64>,
    /// Transmit rate in bytes per second over the previous sample window.
    pub tx_bytes_per_second: Option<u64>,
}

impl<'de> Deserialize<'de> for NetworkState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireState {
            interface: Option<String>,
            connected: bool,
            rx_bytes_per_second: Option<u64>,
            tx_bytes_per_second: Option<u64>,
        }

        let wire = WireState::deserialize(deserializer)?;
        Ok(if wire.connected {
            Self::connected(
                wire.interface.unwrap_or_default(),
                wire.rx_bytes_per_second,
                wire.tx_bytes_per_second,
            )
        } else {
            Self::disconnected()
        })
    }
}

impl NetworkState {
    #[must_use]
    pub const fn disconnected() -> Self {
        Self {
            interface: None,
            connected: false,
            rx_bytes_per_second: None,
            tx_bytes_per_second: None,
        }
    }

    #[must_use]
    pub fn connected(
        interface: impl Into<String>,
        rx_bytes_per_second: Option<u64>,
        tx_bytes_per_second: Option<u64>,
    ) -> Self {
        Self {
            interface: Some(bounded_model_string(interface.into(), MAX_MODEL_ID_BYTES)),
            connected: true,
            rx_bytes_per_second,
            tx_bytes_per_second,
        }
    }

    #[must_use]
    pub fn normalized(self) -> Self {
        if self.connected {
            Self::connected(
                self.interface.unwrap_or_default(),
                self.rx_bytes_per_second,
                self.tx_bytes_per_second,
            )
        } else {
            Self::disconnected()
        }
    }
}

/// Media playback status mirroring the MPRIS `PlaybackStatus` values.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaPlayback {
    #[default]
    Stopped,
    Paused,
    Playing,
}

/// Now-playing state from a desktop media player.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct MediaState {
    pub playback: MediaPlayback,
    pub title: Option<String>,
    pub artist: Option<String>,
    /// Player identity, e.g. `spotify` or `mpv`.
    pub player: Option<String>,
}

impl<'de> Deserialize<'de> for MediaState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireState {
            playback: MediaPlayback,
            title: Option<String>,
            artist: Option<String>,
            player: Option<String>,
        }

        let wire = WireState::deserialize(deserializer)?;
        Ok(Self {
            playback: wire.playback,
            title: wire.title,
            artist: wire.artist,
            player: wire.player,
        }
        .normalized())
    }
}

impl MediaState {
    #[must_use]
    pub const fn inactive() -> Self {
        Self {
            playback: MediaPlayback::Stopped,
            title: None,
            artist: None,
            player: None,
        }
    }

    /// Whether a player is meaningfully present: stopped state with no track
    /// metadata reduces to [`MediaState::inactive`].
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.playback != MediaPlayback::Stopped || self.title.is_some()
    }

    #[must_use]
    pub fn normalized(self) -> Self {
        if self.is_active() {
            Self {
                playback: self.playback,
                title: self
                    .title
                    .map(|value| bounded_model_string(value, MAX_MODEL_DISPLAY_TEXT_BYTES)),
                artist: self
                    .artist
                    .map(|value| bounded_model_string(value, MAX_MODEL_DISPLAY_TEXT_BYTES)),
                player: self
                    .player
                    .map(|value| bounded_model_string(value, MAX_MODEL_ID_BYTES)),
            }
        } else {
            Self::inactive()
        }
    }
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
    /// Chrono format used by the optional clock adapter without seconds.
    pub clock_minute_format: String,
    /// Chrono format used by the optional clock adapter with seconds.
    pub clock_second_format: String,
    /// Look the focused window's desktop icon up from its application
    /// identity. On by default: the lookup is cached per identity and only
    /// runs when focus moves to a different application, so a bar that shows
    /// the title gets the matching icon without opting in. Hosts that do not
    /// want a bar touching the desktop database turn it off here.
    pub resolve_client_icons: bool,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            tag_count: 9,
            volume_step: 5,
            brightness_step: 5,
            initial_theme: ThemeMode::Dark,
            show_seconds: false,
            clock_minute_format: "%Y-%m-%d %H:%M".to_owned(),
            clock_second_format: "%Y-%m-%d %H:%M:%S".to_owned(),
            resolve_client_icons: true,
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
        for (field, format) in [
            ("clock_minute_format", &self.clock_minute_format),
            ("clock_second_format", &self.clock_second_format),
        ] {
            if format.len() > MAX_MODEL_CLOCK_FORMAT_BYTES {
                return Err(ModelError::ClockFormatTooLong {
                    field,
                    length: format.len(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelError {
    InvalidTagCount(usize),
    InvalidPercentageStep { field: &'static str, value: u8 },
    ClockFormatTooLong { field: &'static str, length: usize },
    TagOutOfRange { index: usize, tag_count: usize },
    StaleWmSession { requested: u64, current: u64 },
    StaleMinimizedGeneration { requested: u64, current: u64 },
    WindowNotMinimized(WindowToken),
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
            Self::ClockFormatTooLong { field, length } => write!(
                f,
                "{field} must be at most {MAX_MODEL_CLOCK_FORMAT_BYTES} bytes, got {length}"
            ),
            Self::TagOutOfRange { index, tag_count } => {
                write!(
                    f,
                    "tag index {index} is outside configured range 0..{tag_count}"
                )
            }
            Self::StaleWmSession { requested, current } => write!(
                f,
                "window-manager session {requested} is stale; current session is {current}"
            ),
            Self::StaleMinimizedGeneration { requested, current } => write!(
                f,
                "minimized projection generation {requested} is stale; current generation is {current}"
            ),
            Self::WindowNotMinimized(window) => {
                write!(f, "window {} is not in the minimized shelf", window.get())
            }
        }
    }
}

impl std::error::Error for ModelError {}

/// Events accepted by the pure reducer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BarEvent {
    WindowManager(WmSnapshot),
    /// The window-manager transport is no longer authoritative. This clears
    /// every WM-owned projection while leaving provider state and persistent
    /// UI preferences intact.
    WindowManagerUnavailable,
    Clock(ClockState),
    Audio(AudioState),
    AudioDevice(Option<AudioDeviceInfo>),
    System(SystemState),
    SystemDetails(SystemDetails),
    Brightness(BrightnessState),
    Battery(BatteryState),
    Network(NetworkState),
    Media(MediaState),
    /// Desktop icon resolved for the focused window's application identity.
    /// Filesystem lookup is a host concern, so the model is told the answer
    /// rather than going looking for it.
    ClientIcon(Option<crate::app_icon::AppIcon>),
    User(UserAction),
}

/// One page of the window manager's own shell surface.
///
/// JWM ships a native Shell Hub in the spirit of DMS, Noctalia, Caelestia and
/// end-4: a single keyboard-driven surface that routes to the launcher,
/// notifications, clipboard, calendar and wallpaper picker. A bar does not
/// implement any of those pages — it only names the one it wants opened, so
/// the shell stays a window-manager concern and the bar stays a projection.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ShellRoute {
    /// The hub home page: route list plus grouped quick settings.
    #[default]
    Hub,
    Applications,
    Notifications,
    Clipboard,
    Calendar,
    Wallpaper,
}

impl ShellRoute {
    pub const ALL: [Self; 6] = [
        Self::Hub,
        Self::Applications,
        Self::Notifications,
        Self::Clipboard,
        Self::Calendar,
        Self::Wallpaper,
    ];

    /// Wire code shared with the window manager. Kept explicit rather than
    /// derived from declaration order so reordering the enum for readability
    /// can never silently repoint a running bar at a different page.
    #[must_use]
    pub const fn code(self) -> u32 {
        match self {
            Self::Hub => 0,
            Self::Applications => 1,
            Self::Notifications => 2,
            Self::Clipboard => 3,
            Self::Calendar => 4,
            Self::Wallpaper => 5,
        }
    }

    #[must_use]
    pub const fn from_code(code: u32) -> Option<Self> {
        match code {
            0 => Some(Self::Hub),
            1 => Some(Self::Applications),
            2 => Some(Self::Notifications),
            3 => Some(Self::Clipboard),
            4 => Some(Self::Calendar),
            5 => Some(Self::Wallpaper),
            _ => None,
        }
    }

    /// Stable identifier for configuration files and log lines.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Hub => "hub",
            Self::Applications => "applications",
            Self::Notifications => "notifications",
            Self::Clipboard => "clipboard",
            Self::Calendar => "calendar",
            Self::Wallpaper => "wallpaper",
        }
    }

    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        let key = key.trim().to_ascii_lowercase();
        if let Some(route) = Self::ALL.into_iter().find(|route| route.key() == key) {
            return Some(route);
        }
        // Names the surrounding ecosystem uses for the same pages, so a config
        // copied from a DMS or Noctalia setup keeps working.
        match key.as_str() {
            "shell" | "home" | "control-center" | "control_center" => Some(Self::Hub),
            "apps" | "launcher" | "runner" => Some(Self::Applications),
            "notification" | "notification-center" => Some(Self::Notifications),
            "clip" | "clipboard-history" => Some(Self::Clipboard),
            "date" | "agenda" => Some(Self::Calendar),
            "background" | "wallpapers" => Some(Self::Wallpaper),
            _ => None,
        }
    }

    /// Human-readable page name for tooltips and accessibility labels.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Hub => "Shell Hub",
            Self::Applications => "Applications",
            Self::Notifications => "Notifications",
            Self::Clipboard => "Clipboard",
            Self::Calendar => "Calendar",
            Self::Wallpaper => "Wallpaper",
        }
    }

    /// The next page in [`Self::ALL`] order, wrapping. Frontends bind this to
    /// scroll so one bar cell can reach every route without extra chrome.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Hub => Self::Applications,
            Self::Applications => Self::Notifications,
            Self::Notifications => Self::Clipboard,
            Self::Clipboard => Self::Calendar,
            Self::Calendar => Self::Wallpaper,
            Self::Wallpaper => Self::Hub,
        }
    }

    #[must_use]
    pub const fn previous(self) -> Self {
        match self {
            Self::Hub => Self::Wallpaper,
            Self::Applications => Self::Hub,
            Self::Notifications => Self::Applications,
            Self::Clipboard => Self::Notifications,
            Self::Calendar => Self::Clipboard,
            Self::Wallpaper => Self::Calendar,
        }
    }
}

/// Semantic user intent; native button/key enums are translated by a frontend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UserAction {
    ViewTag(TagId),
    ToggleTag(TagId),
    ViewTagOn {
        tag: TagId,
        monitor: MonitorId,
    },
    ToggleTagOn {
        tag: TagId,
        monitor: MonitorId,
    },
    ToggleLayoutSelector,
    SetLayout(LayoutId),
    SetLayoutOn {
        layout: LayoutId,
        monitor: MonitorId,
    },
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
    /// Restore and focus one window from the minimized-window shelf.
    RestoreWindow {
        window: WindowToken,
        wm_session_id: u64,
        minimized_generation: u64,
        geometry: DockItemGeometry,
    },
    /// Enter or leave the compositor-owned preview for one minimized window.
    PreviewWindow {
        window: WindowToken,
        wm_session_id: u64,
        minimized_generation: u64,
        visible: bool,
        /// True only for a periodic lease refresh of an already-owned preview.
        #[serde(default)]
        renewal: bool,
        geometry: DockItemGeometry,
    },
    /// Publish the shelf (`window == None`) or one item animation target.
    SetDockGeometry {
        window: Option<WindowToken>,
        wm_session_id: u64,
        minimized_generation: u64,
        geometry: DockItemGeometry,
    },
    /// Ask the window manager to open its own shell surface at `route`.
    OpenShellHub(ShellRoute),
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
    /// Open the window manager's shell surface on `monitor`. The bar names the
    /// page; the window manager owns everything the page does.
    OpenShellHub {
        route: ShellRoute,
        monitor: MonitorId,
    },
    RestoreWindow {
        window: WindowToken,
        wm_session_id: u64,
        minimized_generation: u64,
        monitor: MonitorId,
        geometry: DockItemGeometry,
    },
    PreviewWindow {
        window: WindowToken,
        wm_session_id: u64,
        minimized_generation: u64,
        monitor: MonitorId,
        visible: bool,
        /// True only for a periodic lease refresh of an already-owned preview.
        #[serde(default)]
        renewal: bool,
        geometry: DockItemGeometry,
    },
    SetDockGeometry {
        window: Option<WindowToken>,
        wm_session_id: u64,
        minimized_generation: u64,
        monitor: MonitorId,
        geometry: DockItemGeometry,
    },
}

/// Side effects described by the core and executed by an adapter/provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BarEffect {
    WindowManager(WmCommand),
    ApplyMonitorGeometry(MonitorGeometry),
    /// Remove a previously applied monitor geometry constraint.
    ClearMonitorGeometry,
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
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BarView<'a> {
    /// Whether at least one window-manager snapshot has been reduced.
    pub wm_available: bool,
    /// Minimized-projection epoch captured by Dock interactions.
    pub wm_sequence: Option<u64>,
    pub wm_session_id: u64,
    pub tags: &'a [TagState],
    pub active_tag: Option<TagId>,
    pub monitor: MonitorId,
    pub geometry: Option<MonitorGeometry>,
    pub layout_symbol: &'a str,
    /// Layout in use, when the window manager names it on the wire.
    pub layout: Option<LayoutId>,
    /// Layouts the window manager offers, when it says how many.
    pub layout_count: Option<usize>,
    pub client_name: &'a str,
    /// Application identity of the focused window, for its desktop icon.
    pub client_app_id: &'a str,
    /// Desktop icon resolved for [`Self::client_app_id`], when one was found.
    pub client_icon: Option<&'a crate::app_icon::AppIcon>,
    pub minimized_windows: &'a [MinimizedWindow],
    pub minimized_overflow: bool,
    pub time: &'a str,
    pub show_seconds: bool,
    pub layout_selector_open: bool,
    pub theme: ThemeMode,
    pub audio: AudioState,
    pub audio_device: Option<&'a AudioDeviceInfo>,
    pub system: SystemState,
    pub system_details: &'a SystemDetails,
    pub brightness: BrightnessState,
    pub battery: BatteryState,
    pub network: &'a NetworkState,
    pub media: &'a MediaState,
}

/// An owned, serializable projection of [`BarModel`].
///
/// This mirrors [`BarView`] without borrowing the model, making it suitable
/// for toolkit state stores, cross-thread messages, and Tauri/HTML frontends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BarSnapshot {
    pub wm_available: bool,
    /// Minimized-projection epoch captured by Dock interactions.
    pub wm_sequence: Option<u64>,
    #[serde(default)]
    pub wm_session_id: u64,
    pub tags: Vec<TagState>,
    pub active_tag: Option<TagId>,
    pub monitor: MonitorId,
    pub geometry: Option<MonitorGeometry>,
    pub layout_symbol: String,
    #[serde(default)]
    pub layout: Option<LayoutId>,
    #[serde(default)]
    pub layout_count: Option<usize>,
    pub client_name: String,
    #[serde(default)]
    pub client_app_id: String,
    #[serde(default)]
    pub client_icon: Option<crate::app_icon::AppIcon>,
    #[serde(default)]
    pub minimized_windows: Vec<MinimizedWindow>,
    #[serde(default)]
    pub minimized_overflow: bool,
    pub time: String,
    pub show_seconds: bool,
    pub layout_selector_open: bool,
    pub theme: ThemeMode,
    pub audio: AudioState,
    pub audio_device: Option<AudioDeviceInfo>,
    pub system: SystemState,
    pub system_details: SystemDetails,
    pub brightness: BrightnessState,
    pub battery: BatteryState,
    pub network: NetworkState,
    pub media: MediaState,
}

impl BarSnapshot {
    #[must_use]
    pub fn view(&self) -> BarView<'_> {
        BarView {
            wm_available: self.wm_available,
            wm_sequence: self.wm_sequence,
            wm_session_id: self.wm_session_id,
            tags: &self.tags,
            active_tag: self.active_tag,
            monitor: self.monitor,
            geometry: self.geometry,
            layout_symbol: &self.layout_symbol,
            layout: self.layout,
            layout_count: self.layout_count,
            client_name: &self.client_name,
            client_app_id: &self.client_app_id,
            client_icon: self.client_icon.as_ref(),
            minimized_windows: &self.minimized_windows,
            minimized_overflow: self.minimized_overflow,
            time: &self.time,
            show_seconds: self.show_seconds,
            layout_selector_open: self.layout_selector_open,
            theme: self.theme,
            audio: self.audio,
            audio_device: self.audio_device.as_ref(),
            system: self.system,
            system_details: &self.system_details,
            brightness: self.brightness,
            battery: self.battery,
            network: &self.network,
            media: &self.media,
        }
    }
}

fn normalize_wm_tags(tags: Vec<TagState>, tag_count: usize) -> Vec<TagState> {
    let mut normalized = Vec::with_capacity(tag_count);
    normalized.extend(tags.into_iter().take(tag_count));
    normalized.resize(tag_count, TagState::default());
    normalized
}

fn bounded_model_string(value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes && value.capacity() <= max_bytes {
        return value;
    }

    let mut end = value.len().min(max_bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn normalize_minimized_windows(
    windows: Vec<MinimizedWindow>,
    upstream_overflow: bool,
) -> (Vec<MinimizedWindow>, bool) {
    let mut normalized = Vec::with_capacity(windows.len().min(MAX_MODEL_MINIMIZED_WINDOWS));
    let mut overflow = upstream_overflow;
    for (input_index, mut window) in windows.into_iter().enumerate() {
        if input_index == MAX_WM_MINIMIZED_INPUTS {
            overflow = true;
            break;
        }
        if window.token.get() == 0
            || normalized
                .iter()
                .any(|kept: &MinimizedWindow| kept.token == window.token)
        {
            continue;
        }
        if normalized.len() == MAX_MODEL_MINIMIZED_WINDOWS {
            overflow = true;
            break;
        }
        window.title = bounded_model_string(window.title, MAX_MODEL_DISPLAY_TEXT_BYTES);
        window.app_id = bounded_model_string(window.app_id, MAX_MODEL_ID_BYTES);
        normalized.push(window);
    }
    (normalized, overflow)
}

/// Canonical backend-independent model. All fields are private so invariants
/// remain stable as additional frontends adopt it.
#[derive(Debug, Clone)]
pub struct BarModel {
    config: ModelConfig,
    wm_available: bool,
    wm_sequence: Option<u64>,
    wm_session_id: u64,
    tags: Vec<TagState>,
    active_tag: Option<TagId>,
    monitor: MonitorId,
    geometry: Option<MonitorGeometry>,
    layout_symbol: String,
    layout: Option<LayoutId>,
    layout_count: Option<usize>,
    client_name: String,
    client_app_id: String,
    client_icon: Option<crate::app_icon::AppIcon>,
    minimized_windows: Vec<MinimizedWindow>,
    minimized_overflow: bool,
    clock: ClockState,
    show_seconds: bool,
    layout_selector_open: bool,
    theme: ThemeMode,
    audio: AudioState,
    audio_device: Option<AudioDeviceInfo>,
    system: SystemState,
    system_details: SystemDetails,
    brightness: BrightnessState,
    battery: BatteryState,
    network: NetworkState,
    media: MediaState,
}

impl Default for BarModel {
    fn default() -> Self {
        Self::new(ModelConfig::default()).expect("default model config is valid")
    }
}

impl BarModel {
    pub fn new(mut config: ModelConfig) -> Result<Self, ModelError> {
        config.validate()?;
        config.clock_minute_format =
            bounded_model_string(config.clock_minute_format, MAX_MODEL_CLOCK_FORMAT_BYTES);
        config.clock_second_format =
            bounded_model_string(config.clock_second_format, MAX_MODEL_CLOCK_FORMAT_BYTES);
        Ok(Self {
            wm_available: false,
            wm_sequence: None,
            wm_session_id: 0,
            tags: vec![TagState::default(); config.tag_count],
            active_tag: None,
            monitor: MonitorId::default(),
            geometry: None,
            layout_symbol: "[]=".to_owned(),
            layout: None,
            layout_count: None,
            client_name: String::new(),
            client_app_id: String::new(),
            client_icon: None,
            minimized_windows: Vec::new(),
            minimized_overflow: false,
            clock: ClockState::default(),
            show_seconds: config.show_seconds,
            layout_selector_open: false,
            theme: config.initial_theme,
            audio: AudioState::default(),
            audio_device: None,
            system: SystemState::default(),
            system_details: SystemDetails::default(),
            brightness: BrightnessState::default(),
            battery: BatteryState::default(),
            network: NetworkState::default(),
            media: MediaState::default(),
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
            wm_available: self.wm_available,
            wm_sequence: self.wm_sequence,
            wm_session_id: self.wm_session_id,
            tags: &self.tags,
            active_tag: self.active_tag,
            monitor: self.monitor,
            geometry: self.geometry,
            layout_symbol: &self.layout_symbol,
            layout: self.layout,
            layout_count: self.layout_count,
            client_name: &self.client_name,
            client_app_id: &self.client_app_id,
            client_icon: self.client_icon.as_ref(),
            minimized_windows: &self.minimized_windows,
            minimized_overflow: self.minimized_overflow,
            time: if self.show_seconds {
                &self.clock.second
            } else {
                &self.clock.minute
            },
            show_seconds: self.show_seconds,
            layout_selector_open: self.layout_selector_open,
            theme: self.theme,
            audio: self.audio,
            audio_device: self.audio_device.as_ref(),
            system: self.system,
            system_details: &self.system_details,
            brightness: self.brightness,
            battery: self.battery,
            network: &self.network,
            media: &self.media,
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> BarSnapshot {
        let view = self.view();
        BarSnapshot {
            wm_available: view.wm_available,
            wm_sequence: view.wm_sequence,
            wm_session_id: view.wm_session_id,
            tags: view.tags.to_vec(),
            active_tag: view.active_tag,
            monitor: view.monitor,
            geometry: view.geometry,
            layout_symbol: view.layout_symbol.to_owned(),
            layout: view.layout,
            layout_count: view.layout_count,
            client_name: view.client_name.to_owned(),
            client_app_id: view.client_app_id.to_owned(),
            client_icon: view.client_icon.cloned(),
            minimized_windows: view.minimized_windows.to_vec(),
            minimized_overflow: view.minimized_overflow,
            time: view.time.to_owned(),
            show_seconds: view.show_seconds,
            layout_selector_open: view.layout_selector_open,
            theme: view.theme,
            audio: view.audio,
            audio_device: view.audio_device.cloned(),
            system: view.system,
            system_details: view.system_details.clone(),
            brightness: view.brightness,
            battery: view.battery,
            network: view.network.clone(),
            media: view.media.clone(),
        }
    }

    /// Apply an event and return both visual damage and typed side effects.
    pub fn update(&mut self, event: BarEvent) -> Result<ModelUpdate, ModelError> {
        match event {
            BarEvent::WindowManager(snapshot) => Ok(self.update_wm(snapshot)),
            BarEvent::WindowManagerUnavailable => Ok(self.clear_wm()),
            BarEvent::Clock(clock) => Ok(self.update_clock(clock)),
            BarEvent::Audio(audio) => Ok(self.replace_audio(audio)),
            BarEvent::AudioDevice(device) => Ok(self.replace_audio_device(device)),
            BarEvent::System(system) => Ok(self.replace_system(system.normalized())),
            BarEvent::SystemDetails(details) => Ok(self.replace_system_details(details)),
            BarEvent::Brightness(brightness) => Ok(self.replace_brightness(brightness)),
            BarEvent::Battery(battery) => Ok(self.replace_battery(battery)),
            BarEvent::Network(network) => Ok(self.replace_network(network)),
            BarEvent::Media(media) => Ok(self.replace_media(media)),
            BarEvent::ClientIcon(icon) => Ok(self.replace_client_icon(icon)),
            BarEvent::User(action) => self.update_user(action),
        }
    }

    /// Attach (or drop) the icon a host resolved for the focused application.
    fn replace_client_icon(&mut self, icon: Option<crate::app_icon::AppIcon>) -> ModelUpdate {
        if self.client_icon == icon {
            return ModelUpdate::default();
        }
        self.client_icon = icon;
        ModelUpdate {
            dirty: DirtyBits::new(DirtyBits::CLIENT_CHANGED),
            effects: Vec::new(),
        }
    }

    fn update_wm(&mut self, mut snapshot: WmSnapshot) -> ModelUpdate {
        snapshot.tags = normalize_wm_tags(snapshot.tags, self.config.tag_count);
        (snapshot.minimized_windows, snapshot.minimized_overflow) =
            normalize_minimized_windows(snapshot.minimized_windows, snapshot.minimized_overflow);
        snapshot.layout_symbol =
            bounded_model_string(snapshot.layout_symbol, MAX_MODEL_LAYOUT_SYMBOL_BYTES);
        snapshot.client_name =
            bounded_model_string(snapshot.client_name, MAX_MODEL_DISPLAY_TEXT_BYTES);
        snapshot.client_app_id = bounded_model_string(snapshot.client_app_id, MAX_MODEL_ID_BYTES);

        let mut dirty = DirtyBits::default();
        let next_active = snapshot
            .tags
            .iter()
            .position(|tag| tag.selected)
            .and_then(TagId::new);

        if !self.wm_available
            || self.tags != snapshot.tags
            || self.active_tag != next_active
            || self.monitor != snapshot.monitor
        {
            dirty.set(DirtyBits::MONITOR_CHANGED);
        }
        if self.layout_symbol != snapshot.layout_symbol
            || self.layout != snapshot.layout
            || self.layout_count != snapshot.layout_count
        {
            dirty.set(DirtyBits::LAYOUT_CHANGED);
        }
        if self.client_name != snapshot.client_name || self.client_app_id != snapshot.client_app_id
        {
            dirty.set(DirtyBits::CLIENT_CHANGED);
        }
        if self.wm_session_id != snapshot.wm_session_id
            || self.minimized_windows != snapshot.minimized_windows
            || self.minimized_overflow != snapshot.minimized_overflow
        {
            dirty.set(DirtyBits::MINIMIZED_CHANGED);
        }
        let geometry_changed = self.geometry != snapshot.geometry;
        if geometry_changed {
            dirty.set(DirtyBits::GEOMETRY_CHANGED);
        }

        self.wm_available = true;
        self.wm_sequence = snapshot.sequence;
        self.wm_session_id = snapshot.wm_session_id;
        self.tags = snapshot.tags;
        self.active_tag = next_active;
        self.monitor = snapshot.monitor;
        self.geometry = snapshot.geometry;
        self.layout_symbol = snapshot.layout_symbol;
        self.layout = snapshot.layout;
        self.layout_count = snapshot.layout_count;
        self.client_name = snapshot.client_name;
        if self.client_app_id != snapshot.client_app_id {
            // The icon belongs to the application that was focused, not to the
            // one that is. Dropping it here means a bar shows no icon for a
            // moment rather than the previous window's icon under the new
            // window's title; the host resolves the replacement.
            self.client_app_id = snapshot.client_app_id;
            self.client_icon = None;
        }
        self.minimized_windows = snapshot.minimized_windows;
        self.minimized_overflow = snapshot.minimized_overflow;

        ModelUpdate {
            dirty,
            effects: if geometry_changed {
                vec![match self.geometry {
                    Some(geometry) => BarEffect::ApplyMonitorGeometry(geometry),
                    None => BarEffect::ClearMonitorGeometry,
                }]
            } else {
                Vec::new()
            },
        }
    }

    fn clear_wm(&mut self) -> ModelUpdate {
        if !self.wm_available {
            return ModelUpdate::default();
        }

        let geometry_was_set = self.geometry.is_some();
        let layout_changed = self.layout_symbol != "[]="
            || self.layout_selector_open
            || self.layout.is_some()
            || self.layout_count.is_some();
        let client_changed = !self.client_name.is_empty()
            || !self.client_app_id.is_empty()
            || self.client_icon.is_some();
        let minimized_changed = self.wm_session_id != 0
            || !self.minimized_windows.is_empty()
            || self.minimized_overflow;

        self.wm_available = false;
        self.wm_sequence = None;
        self.wm_session_id = 0;
        self.tags.fill(TagState::default());
        self.active_tag = None;
        self.monitor = MonitorId::default();
        self.geometry = None;
        self.layout_symbol.clear();
        self.layout_symbol.push_str("[]=");
        self.layout = None;
        self.layout_count = None;
        self.client_name.clear();
        self.client_app_id.clear();
        self.client_icon = None;
        self.minimized_windows.clear();
        self.minimized_overflow = false;
        self.layout_selector_open = false;

        let mut dirty = DirtyBits::new(DirtyBits::MONITOR_CHANGED);
        if geometry_was_set {
            dirty.set(DirtyBits::GEOMETRY_CHANGED);
        }
        if layout_changed {
            dirty.set(DirtyBits::LAYOUT_CHANGED);
        }
        if client_changed {
            dirty.set(DirtyBits::CLIENT_CHANGED);
        }
        if minimized_changed {
            dirty.set(DirtyBits::MINIMIZED_CHANGED);
        }

        ModelUpdate {
            dirty,
            effects: geometry_was_set
                .then_some(BarEffect::ClearMonitorGeometry)
                .into_iter()
                .collect(),
        }
    }

    fn update_clock(&mut self, clock: ClockState) -> ModelUpdate {
        let clock = clock.normalized();
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

    fn replace_audio(&mut self, audio: AudioState) -> ModelUpdate {
        let audio = audio.normalized();
        let changed = self.audio != audio;
        self.audio = audio;
        Self::changed(DirtyBits::AUDIO_CHANGED, changed)
    }

    fn replace_audio_device(&mut self, device: Option<AudioDeviceInfo>) -> ModelUpdate {
        let device = device.map(AudioDeviceInfo::normalized);
        let changed = self.audio_device != device;
        self.audio_device = device;
        Self::changed(DirtyBits::AUDIO_CHANGED, changed)
    }

    fn replace_system(&mut self, system: SystemState) -> ModelUpdate {
        let changed = self.system != system;
        self.system = system;
        Self::changed(DirtyBits::SYSTEM_CHANGED, changed)
    }

    fn replace_system_details(&mut self, details: SystemDetails) -> ModelUpdate {
        let details = details.normalized();
        let changed = self.system_details != details;
        self.system_details = details;
        Self::changed(DirtyBits::SYSTEM_CHANGED, changed)
    }

    fn replace_brightness(&mut self, brightness: BrightnessState) -> ModelUpdate {
        let changed = self.brightness != brightness;
        self.brightness = brightness;
        Self::changed(DirtyBits::BRIGHTNESS_CHANGED, changed)
    }

    fn replace_battery(&mut self, battery: BatteryState) -> ModelUpdate {
        let battery = battery.normalized();
        let changed = self.battery != battery;
        self.battery = battery;
        Self::changed(DirtyBits::BATTERY_CHANGED, changed)
    }

    fn replace_network(&mut self, network: NetworkState) -> ModelUpdate {
        let network = network.normalized();
        let changed = self.network != network;
        self.network = network;
        Self::changed(DirtyBits::NETWORK_CHANGED, changed)
    }

    fn replace_media(&mut self, media: MediaState) -> ModelUpdate {
        let media = media.normalized();
        let changed = self.media != media;
        self.media = media;
        Self::changed(DirtyBits::MEDIA_CHANGED, changed)
    }

    fn update_user(&mut self, action: UserAction) -> Result<ModelUpdate, ModelError> {
        let mut update = ModelUpdate::default();
        match action {
            UserAction::ViewTag(tag) => {
                self.ensure_configured_tag(tag)?;
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
            UserAction::ViewTagOn { tag, monitor } => {
                self.ensure_configured_tag(tag)?;
                update
                    .effects
                    .push(BarEffect::WindowManager(WmCommand::ViewTag {
                        tag,
                        monitor,
                    }));
            }
            UserAction::ToggleTagOn { tag, monitor } => {
                self.ensure_configured_tag(tag)?;
                update
                    .effects
                    .push(BarEffect::WindowManager(WmCommand::ToggleTag {
                        tag,
                        monitor,
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
            UserAction::SetLayoutOn { layout, monitor } => {
                self.layout_selector_open = false;
                update.dirty.set(DirtyBits::LAYOUT_CHANGED);
                update
                    .effects
                    .push(BarEffect::WindowManager(WmCommand::SetLayout {
                        layout,
                        monitor,
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
            UserAction::RestoreWindow {
                window,
                wm_session_id,
                minimized_generation,
                geometry,
            } => {
                self.ensure_wm_session(wm_session_id)?;
                self.ensure_minimized_generation(minimized_generation)?;
                let monitor = self.minimized_window(window)?.monitor;
                update
                    .effects
                    .push(BarEffect::WindowManager(WmCommand::RestoreWindow {
                        window,
                        wm_session_id,
                        minimized_generation,
                        monitor,
                        geometry,
                    }));
            }
            UserAction::PreviewWindow {
                window,
                wm_session_id,
                minimized_generation,
                visible,
                renewal,
                geometry,
            } => {
                self.ensure_wm_session(wm_session_id)?;
                self.ensure_minimized_generation(minimized_generation)?;
                let monitor = self.minimized_window(window)?.monitor;
                update
                    .effects
                    .push(BarEffect::WindowManager(WmCommand::PreviewWindow {
                        window,
                        wm_session_id,
                        minimized_generation,
                        monitor,
                        visible,
                        renewal,
                        geometry,
                    }));
            }
            UserAction::SetDockGeometry {
                window,
                wm_session_id,
                minimized_generation,
                geometry,
            } => {
                self.ensure_wm_session(wm_session_id)?;
                self.ensure_minimized_generation(minimized_generation)?;
                let monitor = match window {
                    Some(window) => match self.minimized_window(window) {
                        Ok(window) => window.monitor,
                        Err(ModelError::WindowNotMinimized(_)) if geometry.is_empty() => {
                            self.monitor
                        }
                        Err(error) => return Err(error),
                    },
                    None => self.monitor,
                };
                update
                    .effects
                    .push(BarEffect::WindowManager(WmCommand::SetDockGeometry {
                        window,
                        wm_session_id,
                        minimized_generation,
                        monitor,
                        geometry,
                    }));
            }
            UserAction::OpenShellHub(route) => {
                // No local state changes: the shell surface is owned by the
                // window manager, so the bar must not render its own idea of
                // whether it is open.
                update
                    .effects
                    .push(BarEffect::WindowManager(WmCommand::OpenShellHub {
                        route,
                        monitor: self.monitor,
                    }));
            }
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

    fn minimized_window(&self, token: WindowToken) -> Result<&MinimizedWindow, ModelError> {
        self.minimized_windows
            .iter()
            .find(|window| window.token == token)
            .ok_or(ModelError::WindowNotMinimized(token))
    }

    fn ensure_wm_session(&self, requested: u64) -> Result<(), ModelError> {
        if requested == self.wm_session_id {
            Ok(())
        } else {
            Err(ModelError::StaleWmSession {
                requested,
                current: self.wm_session_id,
            })
        }
    }

    fn ensure_minimized_generation(&self, requested: u64) -> Result<(), ModelError> {
        let current = self.wm_sequence.unwrap_or_default();
        if requested == current {
            Ok(())
        } else {
            Err(ModelError::StaleMinimizedGeneration { requested, current })
        }
    }

    fn adjust_volume(&mut self, delta: i32, update: &mut ModelUpdate) {
        if delta == 0 {
            return;
        }
        update.effects.push(BarEffect::AdjustVolume(delta));
    }

    fn adjust_brightness(&mut self, delta: i32, update: &mut ModelUpdate) {
        if delta == 0 {
            return;
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

    fn percent(value: u8) -> Percent {
        Percent::from_whole(value).unwrap()
    }

    #[test]
    fn shell_route_codes_are_stable_and_total() {
        for route in ShellRoute::ALL {
            assert_eq!(ShellRoute::from_code(route.code()), Some(route));
            assert_eq!(ShellRoute::from_key(route.key()), Some(route));
        }
        // The codes are the wire contract with the window manager; pinning
        // them here makes an accidental reorder of the enum a test failure
        // rather than a bar that opens the wrong page.
        assert_eq!(
            ShellRoute::ALL.map(ShellRoute::code),
            [0, 1, 2, 3, 4, 5],
            "wire codes must not move"
        );
        assert_eq!(ShellRoute::from_code(6), None);
        assert_eq!(ShellRoute::default(), ShellRoute::Hub);
    }

    #[test]
    fn shell_route_keys_accept_case_padding_and_ecosystem_aliases() {
        assert_eq!(
            ShellRoute::from_key("  Notifications "),
            Some(ShellRoute::Notifications)
        );
        assert_eq!(
            ShellRoute::from_key("LAUNCHER"),
            Some(ShellRoute::Applications)
        );
        assert_eq!(
            ShellRoute::from_key("control-center"),
            Some(ShellRoute::Hub)
        );
        assert_eq!(
            ShellRoute::from_key("background"),
            Some(ShellRoute::Wallpaper)
        );
        assert_eq!(ShellRoute::from_key("nope"), None);
    }

    #[test]
    fn shell_route_neighbours_form_one_cycle_over_every_page() {
        let mut route = ShellRoute::Hub;
        let mut visited = vec![route];
        for _ in 1..ShellRoute::ALL.len() {
            route = route.next();
            assert!(!visited.contains(&route), "next() must not revisit a page");
            visited.push(route);
        }
        assert_eq!(route.next(), ShellRoute::Hub, "the cycle must close");

        // previous() is the exact inverse, so scrolling back and forth over a
        // bar cell lands on the page it started from.
        for route in ShellRoute::ALL {
            assert_eq!(route.next().previous(), route);
            assert_eq!(route.previous().next(), route);
        }
    }

    #[test]
    fn opening_the_shell_asks_the_window_manager_and_changes_no_local_state() {
        let mut model = BarModel::default();
        model
            .update(BarEvent::WindowManager(WmSnapshot {
                sequence: None,
                monitor: MonitorId(2),
                geometry: None,
                layout_symbol: "[]=".to_owned(),
                client_name: String::new(),
                tags: Vec::new(),
                ..WmSnapshot::default()
            }))
            .unwrap();
        let before = model.snapshot();

        let update = model
            .update(BarEvent::User(UserAction::OpenShellHub(
                ShellRoute::Clipboard,
            )))
            .unwrap();

        assert_eq!(
            update.effects,
            vec![BarEffect::WindowManager(WmCommand::OpenShellHub {
                route: ShellRoute::Clipboard,
                monitor: MonitorId(2),
            })]
        );
        // The shell surface belongs to the window manager, so the bar must not
        // start rendering its own idea of whether it is open.
        assert!(update.dirty.is_empty());
        assert_eq!(model.snapshot(), before);
    }

    #[test]
    fn percent_rejects_non_finite_and_out_of_range_values() {
        assert_eq!(Percent::new(f64::NAN), Err(PercentError::NotFinite));
        assert_eq!(Percent::new(f64::INFINITY), Err(PercentError::NotFinite));
        assert_eq!(Percent::new(-0.01), Err(PercentError::OutOfRange));
        assert_eq!(Percent::new(100.01), Err(PercentError::OutOfRange));

        let value = Percent::new(42.345).unwrap();
        assert_eq!(value.basis_points(), 4_235);
        assert_eq!(value.as_f64(), 42.35);
        assert_eq!(value.rounded(), 42);
    }

    #[test]
    fn percent_deserialization_cannot_bypass_validation() {
        use serde::de::value::{Error, F64Deserializer};

        let deserialize = |value| Percent::deserialize(F64Deserializer::<Error>::new(value));
        assert_eq!(deserialize(12.5).unwrap(), Percent::new(12.5).unwrap());
        assert!(deserialize(f64::NAN).is_err());
        assert!(deserialize(-1.0).is_err());
        assert!(deserialize(101.0).is_err());
    }

    #[test]
    fn tag_deserialization_cannot_bypass_mask_width() {
        use serde::de::value::{Error, U8Deserializer};

        let deserialize = |value| TagId::deserialize(U8Deserializer::<Error>::new(value));
        assert_eq!(deserialize(31).unwrap(), tag(31));
        assert!(deserialize(32).is_err());
        assert!(deserialize(255).is_err());
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
        assert!(matches!(
            BarModel::new(ModelConfig {
                clock_minute_format: "x".repeat(MAX_MODEL_CLOCK_FORMAT_BYTES + 1),
                ..ModelConfig::default()
            }),
            Err(ModelError::ClockFormatTooLong {
                field: "clock_minute_format",
                length
            }) if length == MAX_MODEL_CLOCK_FORMAT_BYTES + 1
        ));

        let mut compact_format = String::with_capacity(1_000_000);
        compact_format.push_str("%H:%M");
        let model = BarModel::new(ModelConfig {
            clock_minute_format: compact_format,
            ..ModelConfig::default()
        })
        .unwrap();
        assert!(model.config().clock_minute_format.capacity() <= MAX_MODEL_CLOCK_FORMAT_BYTES);
    }

    #[test]
    fn wm_snapshot_updates_model_and_suppresses_unchanged_content() {
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
            ..WmSnapshot::default()
        };

        let first = model
            .update(BarEvent::WindowManager(snapshot.clone()))
            .unwrap();
        assert!(first.dirty.contains(DirtyBits::MONITOR_CHANGED));
        assert!(first.dirty.contains(DirtyBits::LAYOUT_CHANGED));
        assert!(first.dirty.contains(DirtyBits::CLIENT_CHANGED));
        assert!(first.dirty.contains(DirtyBits::GEOMETRY_CHANGED));
        assert!(model.view().wm_available);
        assert_eq!(model.view().wm_sequence, Some(7));
        assert_eq!(model.view().active_tag, Some(tag(2)));
        assert_eq!(model.view().monitor, MonitorId(3));
        assert_eq!(model.view().geometry.unwrap().x, 1920);

        let duplicate = model
            .update(BarEvent::WindowManager(snapshot.clone()))
            .unwrap();
        assert!(!duplicate.needs_redraw());

        let cleared = model
            .update(BarEvent::WindowManager(WmSnapshot {
                sequence: Some(8),
                geometry: None,
                ..snapshot
            }))
            .unwrap();
        assert!(cleared.dirty.contains(DirtyBits::GEOMETRY_CHANGED));
        assert_eq!(cleared.effects, vec![BarEffect::ClearMonitorGeometry]);
        assert_eq!(model.view().geometry, None);
    }

    #[test]
    fn minimized_snapshot_is_session_scoped_deduplicated_and_cleared() {
        let mut model = BarModel::default();
        let first = MinimizedWindow {
            token: WindowToken(41),
            monitor: MonitorId(3),
            title: "Terminal".into(),
            app_id: "foot".into(),
            flags: MINIMIZED_WINDOW_FLAG_PREVIEW_AVAILABLE | MINIMIZED_WINDOW_FLAG_URGENT,
        };
        let duplicate = MinimizedWindow {
            title: "must be discarded".into(),
            ..first.clone()
        };
        let zero = MinimizedWindow {
            token: WindowToken(0),
            title: "invalid".into(),
            ..MinimizedWindow::default()
        };
        let snapshot = WmSnapshot {
            sequence: Some(8),
            wm_session_id: 73,
            monitor: MonitorId(2),
            minimized_windows: vec![first.clone(), duplicate, zero],
            minimized_overflow: true,
            ..WmSnapshot::default()
        };

        let update = model
            .update(BarEvent::WindowManager(snapshot.clone()))
            .unwrap();
        assert!(update.dirty.contains(DirtyBits::MINIMIZED_CHANGED));
        assert_eq!(model.view().wm_session_id, 73);
        assert_eq!(model.view().minimized_windows, std::slice::from_ref(&first));
        assert!(model.view().minimized_overflow);
        assert!(first.preview_available());
        assert!(first.urgent());
        assert_eq!(first.initial(), 'F');

        let canonical = WmSnapshot {
            minimized_windows: vec![first],
            ..snapshot
        };
        assert!(
            !model
                .update(BarEvent::WindowManager(canonical.clone()))
                .unwrap()
                .dirty
                .contains(DirtyBits::MINIMIZED_CHANGED)
        );

        let restarted = model
            .update(BarEvent::WindowManager(WmSnapshot {
                wm_session_id: 74,
                ..canonical
            }))
            .unwrap();
        assert!(restarted.dirty.contains(DirtyBits::MINIMIZED_CHANGED));

        let unavailable = model.update(BarEvent::WindowManagerUnavailable).unwrap();
        assert!(unavailable.dirty.contains(DirtyBits::MINIMIZED_CHANGED));
        assert_eq!(model.view().wm_session_id, 0);
        assert!(model.view().minimized_windows.is_empty());
        assert!(!model.view().minimized_overflow);

        let oversized = (1..=MAX_MODEL_MINIMIZED_WINDOWS + 2)
            .map(|token| MinimizedWindow {
                token: WindowToken(token as u64),
                ..MinimizedWindow::default()
            })
            .collect();
        model
            .update(BarEvent::WindowManager(WmSnapshot {
                wm_session_id: 75,
                minimized_windows: oversized,
                ..WmSnapshot::default()
            }))
            .unwrap();
        assert_eq!(
            model.view().minimized_windows.len(),
            MAX_MODEL_MINIMIZED_WINDOWS
        );
        assert!(model.view().minimized_overflow);
    }

    #[test]
    fn wm_snapshot_normalization_bounds_unreachable_input() {
        let tags = vec![TagState::default(); MAX_MODEL_TAGS * 8];
        assert_eq!(normalize_wm_tags(tags, 3).len(), 3);

        let duplicates = (0..=MAX_WM_MINIMIZED_INPUTS)
            .map(|index| MinimizedWindow {
                token: WindowToken(1),
                title: format!("duplicate-{index}"),
                ..MinimizedWindow::default()
            })
            .collect();
        let (windows, overflow) = normalize_minimized_windows(duplicates, false);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].title, "duplicate-0");
        assert!(overflow, "unexamined input must be reported as truncated");
    }

    #[test]
    fn wm_snapshot_normalization_bounds_retained_text_and_capacity() {
        let mut model = BarModel::default();
        let unicode_title = "€".repeat(MAX_MODEL_DISPLAY_TEXT_BYTES / 3 + 10);
        let app_id = "a".repeat(MAX_MODEL_ID_BYTES + 10);
        let mut layout_symbol = String::with_capacity(1_000_000);
        layout_symbol.push_str("[M]");

        model
            .update(BarEvent::WindowManager(WmSnapshot {
                layout_symbol,
                client_name: unicode_title.clone(),
                client_app_id: app_id.clone(),
                minimized_windows: vec![MinimizedWindow {
                    token: WindowToken(1),
                    title: unicode_title,
                    app_id,
                    ..MinimizedWindow::default()
                }],
                ..WmSnapshot::default()
            }))
            .unwrap();

        let view = model.view();
        assert_eq!(view.layout_symbol, "[M]");
        assert!(model.layout_symbol.capacity() <= MAX_MODEL_LAYOUT_SYMBOL_BYTES);
        assert_eq!(view.client_name.len(), MAX_MODEL_DISPLAY_TEXT_BYTES - 1);
        assert_eq!(view.client_app_id.len(), MAX_MODEL_ID_BYTES);
        assert_eq!(
            view.minimized_windows[0].title.len(),
            MAX_MODEL_DISPLAY_TEXT_BYTES - 1
        );
        assert_eq!(view.minimized_windows[0].app_id.len(), MAX_MODEL_ID_BYTES);
    }

    #[test]
    fn minimized_actions_bind_current_session_monitor_and_validate_tokens() {
        let mut model = BarModel::default();
        let token = WindowToken(0x1234);
        model
            .update(BarEvent::WindowManager(WmSnapshot {
                sequence: Some(73),
                wm_session_id: 91,
                monitor: MonitorId(2),
                minimized_windows: vec![MinimizedWindow {
                    token,
                    monitor: MonitorId(7),
                    title: "Editor".into(),
                    app_id: "code".into(),
                    flags: MINIMIZED_WINDOW_FLAG_PREVIEW_AVAILABLE,
                }],
                ..WmSnapshot::default()
            }))
            .unwrap();
        let geometry = DockItemGeometry::new(100, 200, 54, 36);

        let restore = model
            .update(BarEvent::User(UserAction::RestoreWindow {
                window: token,
                wm_session_id: 91,
                minimized_generation: 73,
                geometry,
            }))
            .unwrap();
        assert_eq!(
            restore.effects,
            vec![BarEffect::WindowManager(WmCommand::RestoreWindow {
                window: token,
                wm_session_id: 91,
                minimized_generation: 73,
                monitor: MonitorId(7),
                geometry,
            })]
        );

        let preview = model
            .update(BarEvent::User(UserAction::PreviewWindow {
                window: token,
                wm_session_id: 91,
                minimized_generation: 73,
                visible: true,
                renewal: false,
                geometry,
            }))
            .unwrap();
        assert_eq!(
            preview.effects,
            vec![BarEffect::WindowManager(WmCommand::PreviewWindow {
                window: token,
                wm_session_id: 91,
                minimized_generation: 73,
                monitor: MonitorId(7),
                visible: true,
                renewal: false,
                geometry,
            })]
        );

        let item_geometry = model
            .update(BarEvent::User(UserAction::SetDockGeometry {
                window: Some(token),
                wm_session_id: 91,
                minimized_generation: 73,
                geometry,
            }))
            .unwrap();
        assert_eq!(
            item_geometry.effects,
            vec![BarEffect::WindowManager(WmCommand::SetDockGeometry {
                window: Some(token),
                wm_session_id: 91,
                minimized_generation: 73,
                monitor: MonitorId(7),
                geometry,
            })]
        );

        let shelf_geometry = model
            .update(BarEvent::User(UserAction::SetDockGeometry {
                window: None,
                wm_session_id: 91,
                minimized_generation: 73,
                geometry,
            }))
            .unwrap();
        assert_eq!(
            shelf_geometry.effects,
            vec![BarEffect::WindowManager(WmCommand::SetDockGeometry {
                window: None,
                wm_session_id: 91,
                minimized_generation: 73,
                monitor: MonitorId(2),
                geometry,
            })]
        );

        let stale = WindowToken(0x9999);
        assert_eq!(
            model.update(BarEvent::User(UserAction::RestoreWindow {
                window: stale,
                wm_session_id: 91,
                minimized_generation: 73,
                geometry,
            })),
            Err(ModelError::WindowNotMinimized(stale))
        );

        assert_eq!(
            model.update(BarEvent::User(UserAction::RestoreWindow {
                window: token,
                wm_session_id: 90,
                minimized_generation: 73,
                geometry,
            })),
            Err(ModelError::StaleWmSession {
                requested: 90,
                current: 91,
            })
        );

        assert_eq!(
            model.update(BarEvent::User(UserAction::RestoreWindow {
                window: token,
                wm_session_id: 91,
                minimized_generation: 72,
                geometry,
            })),
            Err(ModelError::StaleMinimizedGeneration {
                requested: 72,
                current: 73,
            })
        );
    }

    #[test]
    fn removed_minimized_window_accepts_only_zero_geometry_withdrawal() {
        let mut model = BarModel::default();
        let token = WindowToken(0x1234);
        model
            .update(BarEvent::WindowManager(WmSnapshot {
                wm_session_id: 91,
                monitor: MonitorId(2),
                minimized_windows: vec![MinimizedWindow {
                    token,
                    monitor: MonitorId(7),
                    ..MinimizedWindow::default()
                }],
                ..WmSnapshot::default()
            }))
            .unwrap();
        model
            .update(BarEvent::WindowManager(WmSnapshot {
                wm_session_id: 91,
                monitor: MonitorId(2),
                ..WmSnapshot::default()
            }))
            .unwrap();

        let empty_geometry = DockItemGeometry::new(100, 200, 0, 36);
        let withdrawal = model
            .update(BarEvent::User(UserAction::SetDockGeometry {
                window: Some(token),
                wm_session_id: 91,
                minimized_generation: 0,
                geometry: empty_geometry,
            }))
            .unwrap();
        assert_eq!(
            withdrawal.effects,
            vec![BarEffect::WindowManager(WmCommand::SetDockGeometry {
                window: Some(token),
                wm_session_id: 91,
                minimized_generation: 0,
                monitor: MonitorId(2),
                geometry: empty_geometry,
            })]
        );

        let nonempty_geometry = DockItemGeometry::new(100, 200, 54, 36);
        assert_eq!(
            model.update(BarEvent::User(UserAction::SetDockGeometry {
                window: Some(token),
                wm_session_id: 91,
                minimized_generation: 0,
                geometry: nonempty_geometry,
            })),
            Err(ModelError::WindowNotMinimized(token))
        );
    }

    #[test]
    fn wm_unavailable_clears_authoritative_projection_and_geometry() {
        let mut model = BarModel::default();
        let geometry = MonitorGeometry {
            x: 1920,
            y: 0,
            width: 1920,
            height: 1080,
        };
        model
            .update(BarEvent::WindowManager(WmSnapshot {
                sequence: Some(9),
                monitor: MonitorId(2),
                geometry: Some(geometry),
                layout_symbol: "[M]".into(),
                client_name: "terminal".into(),
                tags: vec![TagState {
                    selected: true,
                    ..TagState::default()
                }],
                ..WmSnapshot::default()
            }))
            .unwrap();
        model
            .update(BarEvent::User(UserAction::ToggleLayoutSelector))
            .unwrap();

        let update = model.update(BarEvent::WindowManagerUnavailable).unwrap();
        let view = model.view();
        assert!(!view.wm_available);
        assert_eq!(view.wm_sequence, None);
        assert_eq!(view.active_tag, None);
        assert_eq!(view.monitor, MonitorId::default());
        assert_eq!(view.geometry, None);
        assert_eq!(view.layout_symbol, "[]=");
        assert_eq!(view.client_name, "");
        assert!(!view.layout_selector_open);
        assert!(view.tags.iter().all(|tag| *tag == TagState::default()));
        assert!(update.dirty.contains(DirtyBits::MONITOR_CHANGED));
        assert!(update.dirty.contains(DirtyBits::GEOMETRY_CHANGED));
        assert!(update.dirty.contains(DirtyBits::LAYOUT_CHANGED));
        assert!(update.dirty.contains(DirtyBits::CLIENT_CHANGED));
        assert_eq!(update.effects, vec![BarEffect::ClearMonitorGeometry]);

        assert!(
            !model
                .update(BarEvent::WindowManagerUnavailable)
                .unwrap()
                .needs_redraw()
        );
    }

    #[test]
    fn equal_wm_sequence_never_hides_changed_content() {
        let mut model = BarModel::default();
        let original = WmSnapshot {
            sequence: Some(7),
            monitor: MonitorId(0),
            geometry: None,
            layout_symbol: "[T]".into(),
            client_name: "first".into(),
            tags: vec![TagState::default(); 9],
            ..WmSnapshot::default()
        };
        model
            .update(BarEvent::WindowManager(original.clone()))
            .unwrap();

        let changed = model
            .update(BarEvent::WindowManager(WmSnapshot {
                client_name: "second".into(),
                ..original
            }))
            .unwrap();

        assert!(changed.dirty.contains(DirtyBits::CLIENT_CHANGED));
        assert_eq!(model.view().client_name, "second");
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
        assert_eq!(model.view().active_tag, None);
        assert!(update.dirty.is_empty());
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
    fn targeted_actions_preserve_the_explicit_monitor() {
        let mut model = BarModel::new(ModelConfig {
            tag_count: 3,
            ..ModelConfig::default()
        })
        .unwrap();
        let monitor = MonitorId(7);

        let tag_update = model
            .update(BarEvent::User(UserAction::ViewTagOn {
                tag: tag(1),
                monitor,
            }))
            .unwrap();
        let layout_update = model
            .update(BarEvent::User(UserAction::SetLayoutOn {
                layout: LayoutId(2),
                monitor,
            }))
            .unwrap();

        assert_eq!(
            tag_update.effects,
            vec![BarEffect::WindowManager(WmCommand::ViewTag {
                tag: tag(1),
                monitor,
            })]
        );
        assert_eq!(
            layout_update.effects,
            vec![BarEffect::WindowManager(WmCommand::SetLayout {
                layout: LayoutId(2),
                monitor,
            })]
        );
        assert_eq!(model.view().monitor, MonitorId(0));
    }

    #[test]
    fn rich_provider_details_are_owned_by_the_model_snapshot() {
        let mut model = BarModel::default();
        let audio_device = AudioDeviceInfo {
            name: "Master".into(),
            index: 0,
            volume: 73,
            is_muted: false,
            description: "Main output".into(),
            has_volume_control: true,
            has_switch_control: true,
        };
        let details = SystemDetails {
            cpu_usage: vec![12.25, 48.5],
            cpu_average: 30.375,
            memory_total: 16_000,
            memory_used: 7_000,
            memory_available: 9_000,
            memory_usage_percent: 43.75,
            uptime: 123,
            load_average: SystemLoadAverage {
                one_minute: 0.5,
                five_minutes: 0.25,
                fifteen_minutes: 0.125,
            },
        };

        model
            .update(BarEvent::AudioDevice(Some(audio_device.clone())))
            .unwrap();
        model
            .update(BarEvent::SystemDetails(details.clone()))
            .unwrap();
        let snapshot = model.snapshot();

        assert_eq!(snapshot.audio_device, Some(audio_device));
        assert_eq!(snapshot.system_details, details);
        model
            .update(BarEvent::SystemDetails(SystemDetails::default()))
            .unwrap();
        assert_eq!(snapshot.system_details.memory_used, 7_000);
    }

    #[test]
    fn provider_labels_are_bounded_before_snapshot_retention() {
        let mut model = BarModel::default();
        let oversized_id = "i".repeat(MAX_MODEL_ID_BYTES + 10);
        let oversized_text = "€".repeat(MAX_MODEL_DISPLAY_TEXT_BYTES / 3 + 10);

        model
            .update(BarEvent::AudioDevice(Some(AudioDeviceInfo {
                name: oversized_id.clone(),
                volume: 150,
                description: oversized_text.clone(),
                ..AudioDeviceInfo::default()
            })))
            .unwrap();
        model
            .update(BarEvent::Network(NetworkState::connected(
                oversized_id.clone(),
                Some(1),
                Some(2),
            )))
            .unwrap();
        model
            .update(BarEvent::Media(MediaState {
                playback: MediaPlayback::Playing,
                title: Some(oversized_text.clone()),
                artist: Some(oversized_text),
                player: Some(oversized_id),
            }))
            .unwrap();

        let view = model.view();
        let audio = view.audio_device.expect("audio device is retained");
        assert_eq!(audio.name.len(), MAX_MODEL_ID_BYTES);
        assert_eq!(audio.volume, 100);
        assert_eq!(audio.description.len(), MAX_MODEL_DISPLAY_TEXT_BYTES - 1);
        assert_eq!(
            view.network.interface.as_deref().map(str::len),
            Some(MAX_MODEL_ID_BYTES)
        );
        assert_eq!(
            view.media.title.as_deref().map(str::len),
            Some(MAX_MODEL_DISPLAY_TEXT_BYTES - 1)
        );
        assert_eq!(
            view.media.artist.as_deref().map(str::len),
            Some(MAX_MODEL_DISPLAY_TEXT_BYTES - 1)
        );
        assert_eq!(
            view.media.player.as_deref().map(str::len),
            Some(MAX_MODEL_ID_BYTES)
        );
    }

    #[test]
    fn system_details_are_bounded_and_stable_after_normalization() {
        let mut model = BarModel::default();
        let mut cpu_usage = vec![50.0; MAX_MODEL_CPU_SAMPLES + 2];
        cpu_usage[..3].copy_from_slice(&[f32::NAN, -5.0, 150.0]);
        let details = SystemDetails {
            cpu_usage,
            cpu_average: f32::INFINITY,
            memory_total: 100,
            memory_used: 101,
            memory_available: 102,
            memory_usage_percent: f32::NAN,
            uptime: 123,
            load_average: SystemLoadAverage {
                one_minute: -1.0,
                five_minutes: f64::NAN,
                fifteen_minutes: f64::INFINITY,
            },
        };

        let first = model
            .update(BarEvent::SystemDetails(details.clone()))
            .unwrap();
        let second = model.update(BarEvent::SystemDetails(details)).unwrap();
        let normalized = model.view().system_details;

        assert!(first.dirty.contains(DirtyBits::SYSTEM_CHANGED));
        assert!(second.dirty.is_empty());
        assert_eq!(normalized.cpu_usage.len(), MAX_MODEL_CPU_SAMPLES);
        assert_eq!(&normalized.cpu_usage[..3], &[0.0, 0.0, 100.0]);
        assert_eq!(normalized.cpu_average, 0.0);
        assert_eq!(normalized.memory_used, 100);
        assert_eq!(normalized.memory_available, 100);
        assert_eq!(normalized.memory_usage_percent, 0.0);
        assert_eq!(normalized.load_average, SystemLoadAverage::default());
    }

    #[test]
    fn configured_steps_emit_effects_and_wait_for_authoritative_provider_updates() {
        let mut model = BarModel::new(ModelConfig {
            volume_step: 7,
            brightness_step: 9,
            ..ModelConfig::default()
        })
        .unwrap();
        model
            .update(BarEvent::Audio(AudioState {
                volume_percent: Some(percent(98)),
                muted: false,
            }))
            .unwrap();
        model
            .update(BarEvent::Brightness(BrightnessState {
                percent: Some(percent(4)),
            }))
            .unwrap();

        let volume = model.update(BarEvent::User(UserAction::VolumeUp)).unwrap();
        let brightness = model
            .update(BarEvent::User(UserAction::BrightnessDown))
            .unwrap();

        assert_eq!(model.view().audio.volume_percent, Some(percent(98)));
        assert_eq!(model.view().brightness.percent, Some(percent(4)));
        assert!(volume.dirty.is_empty());
        assert!(brightness.dirty.is_empty());
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

    #[test]
    fn clock_events_bound_text_and_release_oversized_capacity() {
        let mut model = BarModel::default();
        let unicode = "€".repeat(MAX_MODEL_DISPLAY_TEXT_BYTES / 3 + 10);
        let mut short_with_large_capacity = String::with_capacity(1_000_000);
        short_with_large_capacity.push_str("12:34:56");

        let first = model
            .update(BarEvent::Clock(ClockState {
                minute: unicode.clone(),
                second: short_with_large_capacity,
            }))
            .unwrap();
        let second = model
            .update(BarEvent::Clock(ClockState {
                minute: unicode,
                second: "12:34:56".into(),
            }))
            .unwrap();

        assert!(first.dirty.contains(DirtyBits::TIME_CHANGED));
        assert!(second.dirty.is_empty());
        assert_eq!(model.view().time.len(), MAX_MODEL_DISPLAY_TEXT_BYTES - 1);
        assert!(model.clock.second.capacity() <= MAX_MODEL_DISPLAY_TEXT_BYTES);
    }

    #[test]
    fn model_normalizes_inconsistent_optional_device_states() {
        let mut model = BarModel::default();
        model
            .update(BarEvent::Audio(AudioState {
                volume_percent: None,
                muted: true,
            }))
            .unwrap();
        model
            .update(BarEvent::Battery(BatteryState {
                percent: Some(percent(88)),
                charging: true,
                present: false,
            }))
            .unwrap();

        assert_eq!(model.view().audio, AudioState::default());
        assert_eq!(model.view().battery, BatteryState::absent());
    }

    #[test]
    fn percentage_state_constructors_reject_invalid_provider_values() {
        assert_eq!(
            SystemState::from_f64(Some(f64::NAN), Some(10.0)),
            Err(PercentError::NotFinite)
        );
        assert_eq!(
            BrightnessState::from_f64(Some(120.0)),
            Err(PercentError::OutOfRange)
        );
        assert_eq!(
            BatteryState::from_f64(Some(-1.0), false, true),
            Err(PercentError::OutOfRange)
        );
    }

    #[test]
    fn network_and_media_events_normalize_and_flag_changes() {
        let mut model = BarModel::default();

        let update = model
            .update(BarEvent::Network(NetworkState::connected(
                "wlan0",
                Some(1024),
                None,
            )))
            .unwrap();
        assert!(update.dirty.contains(DirtyBits::NETWORK_CHANGED));
        assert_eq!(model.view().network.interface.as_deref(), Some("wlan0"));
        assert_eq!(model.view().network.rx_bytes_per_second, Some(1024));
        assert_eq!(model.view().network.tx_bytes_per_second, None);

        // A disconnected report clears stale interface and rate values.
        let update = model
            .update(BarEvent::Network(NetworkState {
                interface: Some("stale".to_owned()),
                connected: false,
                rx_bytes_per_second: Some(5),
                tx_bytes_per_second: Some(5),
            }))
            .unwrap();
        assert!(update.dirty.contains(DirtyBits::NETWORK_CHANGED));
        assert_eq!(*model.view().network, NetworkState::disconnected());

        let update = model
            .update(BarEvent::Network(NetworkState::disconnected()))
            .unwrap();
        assert!(update.dirty.is_empty());

        let playing = MediaState {
            playback: MediaPlayback::Playing,
            title: Some("track".to_owned()),
            artist: None,
            player: Some("mpv".to_owned()),
        };
        let update = model.update(BarEvent::Media(playing.clone())).unwrap();
        assert!(update.dirty.contains(DirtyBits::MEDIA_CHANGED));
        assert_eq!(*model.view().media, playing);
        assert!(model.view().media.is_active());

        // Stopped with no track metadata reduces to the inactive state even
        // when a player identity lingers.
        let update = model
            .update(BarEvent::Media(MediaState {
                playback: MediaPlayback::Stopped,
                title: None,
                artist: Some("ghost".to_owned()),
                player: Some("mpv".to_owned()),
            }))
            .unwrap();
        assert!(update.dirty.contains(DirtyBits::MEDIA_CHANGED));
        assert_eq!(*model.view().media, MediaState::inactive());
    }

    #[test]
    fn snapshot_is_owned_and_matches_the_borrowed_view() {
        let mut model = BarModel::default();
        model
            .update(BarEvent::Clock(ClockState {
                minute: "12:34".into(),
                second: "12:34:56".into(),
            }))
            .unwrap();
        model
            .update(BarEvent::System(SystemState::new(
                Some(Percent::new(12.34).unwrap()),
                Some(Percent::new(56.78).unwrap()),
            )))
            .unwrap();

        let snapshot = model.snapshot();
        assert_eq!(snapshot.view(), model.view());

        model
            .update(BarEvent::Clock(ClockState {
                minute: "13:00".into(),
                second: "13:00:01".into(),
            }))
            .unwrap();
        assert_eq!(snapshot.time, "12:34");
        assert_eq!(model.view().time, "13:00");
    }
}
