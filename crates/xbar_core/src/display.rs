//! Pure display semantics shared by widget-toolkit frontends.
//!
//! This module deliberately contains no geometry, renderer, process, provider,
//! or event-loop integration. It turns validated model values into stable
//! presentation categories and compact labels that toolkits can style in
//! their native widget systems.

use std::borrow::Cow;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::model::{
    AudioDeviceInfo, AudioState, BatteryState, LayoutId, MonitorId, Percent, TagId,
};

/// Renderer-independent emphasis for a percentage metric.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricTone {
    /// The provider, device, or value is not available.
    #[default]
    Unavailable,
    Good,
    Warning,
    High,
    Critical,
}

/// Invalid ordering supplied to a display threshold profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThresholdError {
    UsageNotStrictlyIncreasing,
    BatteryNotStrictlyDescending,
    VolumeNotStrictlyIncreasing,
}

impl fmt::Display for ThresholdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UsageNotStrictlyIncreasing => {
                f.write_str("usage thresholds must satisfy good_max < warning_max < high_max")
            }
            Self::BatteryNotStrictlyDescending => {
                f.write_str("battery thresholds must satisfy warning_above < good_above")
            }
            Self::VolumeNotStrictlyIncreasing => {
                f.write_str("volume thresholds must satisfy low_below < high_below")
            }
        }
    }
}

impl std::error::Error for ThresholdError {}

const fn whole_percent(value: u8) -> Percent {
    match Percent::from_whole(value) {
        Ok(percent) => percent,
        Err(_) => panic!("display threshold percentage is outside 0..=100"),
    }
}

/// Default usage bands: `0..=30`, `30..=60`, `60..=80`, then critical.
pub const DEFAULT_USAGE_THRESHOLDS: UsageThresholds = UsageThresholds {
    good_max: whole_percent(30),
    warning_max: whole_percent(60),
    high_max: whole_percent(80),
};

/// Validated thresholds for CPU, memory, and similar usage metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "UsageThresholdsWire")]
pub struct UsageThresholds {
    good_max: Percent,
    warning_max: Percent,
    high_max: Percent,
}

#[derive(Deserialize)]
struct UsageThresholdsWire {
    good_max: Percent,
    warning_max: Percent,
    high_max: Percent,
}

impl TryFrom<UsageThresholdsWire> for UsageThresholds {
    type Error = ThresholdError;

    fn try_from(value: UsageThresholdsWire) -> Result<Self, Self::Error> {
        Self::new(value.good_max, value.warning_max, value.high_max)
    }
}

impl Default for UsageThresholds {
    fn default() -> Self {
        DEFAULT_USAGE_THRESHOLDS
    }
}

impl UsageThresholds {
    /// Construct a strictly increasing usage profile.
    pub const fn new(
        good_max: Percent,
        warning_max: Percent,
        high_max: Percent,
    ) -> Result<Self, ThresholdError> {
        if good_max.basis_points() < warning_max.basis_points()
            && warning_max.basis_points() < high_max.basis_points()
        {
            Ok(Self {
                good_max,
                warning_max,
                high_max,
            })
        } else {
            Err(ThresholdError::UsageNotStrictlyIncreasing)
        }
    }

    #[must_use]
    pub const fn good_max(self) -> Percent {
        self.good_max
    }

    #[must_use]
    pub const fn warning_max(self) -> Percent {
        self.warning_max
    }

    #[must_use]
    pub const fn high_max(self) -> Percent {
        self.high_max
    }

    /// Classify a possibly unavailable usage value.
    #[must_use]
    pub const fn tone(self, value: Option<Percent>) -> MetricTone {
        let Some(value) = value else {
            return MetricTone::Unavailable;
        };
        let value = value.basis_points();
        if value <= self.good_max.basis_points() {
            MetricTone::Good
        } else if value <= self.warning_max.basis_points() {
            MetricTone::Warning
        } else if value <= self.high_max.basis_points() {
            MetricTone::High
        } else {
            MetricTone::Critical
        }
    }
}

/// Classify a usage value with the canonical `30/60/80` profile.
#[must_use]
pub const fn usage_tone(value: Option<Percent>) -> MetricTone {
    DEFAULT_USAGE_THRESHOLDS.tone(value)
}

/// Default battery bands: above 50 is good, above 20 is warning, then critical.
pub const DEFAULT_BATTERY_THRESHOLDS: BatteryThresholds = BatteryThresholds {
    good_above: whole_percent(50),
    warning_above: whole_percent(20),
};

/// Validated thresholds for the inverse severity of remaining battery charge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "BatteryThresholdsWire")]
pub struct BatteryThresholds {
    good_above: Percent,
    warning_above: Percent,
}

#[derive(Deserialize)]
struct BatteryThresholdsWire {
    good_above: Percent,
    warning_above: Percent,
}

impl TryFrom<BatteryThresholdsWire> for BatteryThresholds {
    type Error = ThresholdError;

    fn try_from(value: BatteryThresholdsWire) -> Result<Self, Self::Error> {
        Self::new(value.good_above, value.warning_above)
    }
}

impl Default for BatteryThresholds {
    fn default() -> Self {
        DEFAULT_BATTERY_THRESHOLDS
    }
}

impl BatteryThresholds {
    /// Construct a battery profile whose warning boundary is below its good
    /// boundary.
    pub const fn new(good_above: Percent, warning_above: Percent) -> Result<Self, ThresholdError> {
        if warning_above.basis_points() < good_above.basis_points() {
            Ok(Self {
                good_above,
                warning_above,
            })
        } else {
            Err(ThresholdError::BatteryNotStrictlyDescending)
        }
    }

    #[must_use]
    pub const fn good_above(self) -> Percent {
        self.good_above
    }

    #[must_use]
    pub const fn warning_above(self) -> Percent {
        self.warning_above
    }

    /// Classify an available battery percentage. `None` remains unavailable;
    /// it is never presented as empty or full charge.
    #[must_use]
    pub const fn tone(self, value: Option<Percent>) -> MetricTone {
        let Some(value) = value else {
            return MetricTone::Unavailable;
        };
        let value = value.basis_points();
        if value > self.good_above.basis_points() {
            MetricTone::Good
        } else if value > self.warning_above.basis_points() {
            MetricTone::Warning
        } else {
            MetricTone::Critical
        }
    }

    /// Classify a normalized battery model, preserving absence and an unknown
    /// capacity as [`MetricTone::Unavailable`].
    #[must_use]
    pub const fn tone_for(self, battery: BatteryState) -> MetricTone {
        if battery.present {
            self.tone(battery.percent)
        } else {
            MetricTone::Unavailable
        }
    }
}

/// Classify a battery with the canonical `50/20` profile.
#[must_use]
pub const fn battery_tone(battery: BatteryState) -> MetricTone {
    DEFAULT_BATTERY_THRESHOLDS.tone_for(battery)
}

/// Renderer-neutral audio volume band.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VolumeLevel {
    /// No playback device or no readable volume control is available.
    #[default]
    Unavailable,
    /// The device is muted or its volume is exactly zero.
    Muted,
    Low,
    Medium,
    High,
}

/// Default volume bands: below 34 is low and below 67 is medium.
pub const DEFAULT_VOLUME_THRESHOLDS: VolumeThresholds = VolumeThresholds {
    low_below: whole_percent(34),
    high_below: whole_percent(67),
};

/// Validated volume-level boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "VolumeThresholdsWire")]
pub struct VolumeThresholds {
    low_below: Percent,
    high_below: Percent,
}

#[derive(Deserialize)]
struct VolumeThresholdsWire {
    low_below: Percent,
    high_below: Percent,
}

impl TryFrom<VolumeThresholdsWire> for VolumeThresholds {
    type Error = ThresholdError;

    fn try_from(value: VolumeThresholdsWire) -> Result<Self, Self::Error> {
        Self::new(value.low_below, value.high_below)
    }
}

impl Default for VolumeThresholds {
    fn default() -> Self {
        DEFAULT_VOLUME_THRESHOLDS
    }
}

impl VolumeThresholds {
    pub const fn new(low_below: Percent, high_below: Percent) -> Result<Self, ThresholdError> {
        if low_below.basis_points() < high_below.basis_points() {
            Ok(Self {
                low_below,
                high_below,
            })
        } else {
            Err(ThresholdError::VolumeNotStrictlyIncreasing)
        }
    }

    #[must_use]
    pub const fn low_below(self) -> Percent {
        self.low_below
    }

    #[must_use]
    pub const fn high_below(self) -> Percent {
        self.high_below
    }

    /// Classify compact model audio state.
    #[must_use]
    pub const fn level_for(self, audio: AudioState) -> VolumeLevel {
        let Some(volume) = audio.volume_percent else {
            return VolumeLevel::Unavailable;
        };
        self.level_for_percent(volume, audio.muted)
    }

    /// Classify optional provider metadata. A device without a readable volume
    /// control is deliberately unavailable, rather than silently treated as
    /// zero volume.
    #[must_use]
    pub fn level_for_device(self, device: Option<&AudioDeviceInfo>) -> VolumeLevel {
        let Some(device) = device else {
            return VolumeLevel::Unavailable;
        };
        if !device.has_volume_control {
            return VolumeLevel::Unavailable;
        }
        let volume = device.volume.clamp(0, 100) as u8;
        let volume = whole_percent(volume);
        self.level_for_percent(volume, device.is_muted)
    }

    const fn level_for_percent(self, volume: Percent, muted: bool) -> VolumeLevel {
        let volume = volume.basis_points();
        if muted || volume == 0 {
            VolumeLevel::Muted
        } else if volume < self.low_below.basis_points() {
            VolumeLevel::Low
        } else if volume < self.high_below.basis_points() {
            VolumeLevel::Medium
        } else {
            VolumeLevel::High
        }
    }
}

/// Classify compact audio state with the canonical `34/67` profile.
#[must_use]
pub const fn volume_level(audio: AudioState) -> VolumeLevel {
    DEFAULT_VOLUME_THRESHOLDS.level_for(audio)
}

/// Classify optional audio-device metadata with the canonical `34/67` profile.
#[must_use]
pub fn volume_level_for_device(device: Option<&AudioDeviceInfo>) -> VolumeLevel {
    DEFAULT_VOLUME_THRESHOLDS.level_for_device(device)
}

/// Format bytes with binary (base-1024) IEC units.
#[must_use]
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];

    if bytes < 1024 {
        return format!("{bytes} B");
    }

    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Compact per-second transfer rate, e.g. `1.5MiB/s`.
#[must_use]
pub fn format_transfer_rate(bytes_per_second: u64) -> String {
    format!("{}/s", format_bytes(bytes_per_second))
}

/// Stable compact fallback for an arbitrary (including negative) monitor ID.
#[must_use]
pub fn compact_monitor_label(monitor: MonitorId) -> String {
    format!("M{}", monitor.0)
}

/// Safe behavior when an icon set has fewer tag glyphs than the model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum TagFallback {
    #[default]
    OneBasedNumber,
    Icon(String),
}

/// Configurable semantic glyphs for toolkit renderers.
///
/// Missing or empty tag and monitor entries always fall back to generated
/// compact labels, so a larger runtime tag/monitor count cannot panic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IconSet {
    pub tags: Vec<String>,
    pub tag_fallback: TagFallback,
    pub cpu: String,
    pub memory: String,
    pub battery: String,
    pub battery_charging: String,
    pub brightness: String,
    pub screenshot: String,
    pub clock: String,
    pub monitor: String,
    pub monitor_labels: Vec<String>,
    pub theme_dark: String,
    pub theme_light: String,
    pub volume_unavailable: String,
    pub volume_muted: String,
    pub volume_low: String,
    pub volume_medium: String,
    pub volume_high: String,
    pub unavailable: String,
}

impl Default for IconSet {
    fn default() -> Self {
        Self::nerd_font()
    }
}

impl IconSet {
    /// Icon set shared by the existing Nerd Font toolkit bars.
    #[must_use]
    pub fn nerd_font() -> Self {
        Self {
            tags: [
                "\u{F0A1E}",
                "\u{F0239}",
                "\u{F0A1B}",
                "\u{F0B79}",
                "\u{F024B}",
                "\u{F0388}",
                "\u{F0567}",
                "\u{F01F0}",
                "\u{F0297}",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            tag_fallback: TagFallback::OneBasedNumber,
            // Classic Font Awesome microchip: present in every Nerd Font
            // build, unlike the octicon range which older installs lack.
            cpu: "\u{F2DB}".to_owned(),
            memory: "\u{F035B}".to_owned(),
            battery: "\u{F0079}".to_owned(),
            battery_charging: "\u{F0084}".to_owned(),
            brightness: "\u{F00DE}".to_owned(),
            screenshot: "\u{F0104}".to_owned(),
            clock: "\u{F0954}".to_owned(),
            monitor: "\u{F0379}".to_owned(),
            monitor_labels: ["\u{F02DA}", "\u{F02DB}"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            theme_dark: "\u{F0594}".to_owned(),
            theme_light: "\u{F0599}".to_owned(),
            volume_unavailable: "\u{F075F}".to_owned(),
            volume_muted: "\u{F075F}".to_owned(),
            volume_low: "\u{F057F}".to_owned(),
            volume_medium: "\u{F0580}".to_owned(),
            volume_high: "\u{F057E}".to_owned(),
            unavailable: "?".to_owned(),
        }
    }

    /// Resolve a tag index without indexing past the configured glyph list.
    #[must_use]
    pub fn tag_icon(&self, index: usize) -> Cow<'_, str> {
        if let Some(icon) = self.tags.get(index).filter(|icon| !icon.is_empty()) {
            return Cow::Borrowed(icon);
        }
        match &self.tag_fallback {
            TagFallback::Icon(icon) if !icon.is_empty() => Cow::Borrowed(icon),
            TagFallback::OneBasedNumber | TagFallback::Icon(_) => {
                Cow::Owned(index.saturating_add(1).to_string())
            }
        }
    }

    /// Resolve a checked model tag ID.
    #[must_use]
    pub fn tag_icon_for(&self, tag: TagId) -> Cow<'_, str> {
        self.tag_icon(tag.index())
    }

    /// Resolve a configured monitor glyph, then fall back to `M<id>`.
    #[must_use]
    pub fn monitor_label(&self, monitor: MonitorId) -> Cow<'_, str> {
        let configured = usize::try_from(monitor.0)
            .ok()
            .and_then(|index| self.monitor_labels.get(index))
            .filter(|label| !label.is_empty());
        configured.map_or_else(
            || Cow::Owned(compact_monitor_label(monitor)),
            |label| Cow::Borrowed(label.as_str()),
        )
    }

    #[must_use]
    pub fn volume_icon(&self, level: VolumeLevel) -> &str {
        match level {
            VolumeLevel::Unavailable => &self.volume_unavailable,
            VolumeLevel::Muted => &self.volume_muted,
            VolumeLevel::Low => &self.volume_low,
            VolumeLevel::Medium => &self.volume_medium,
            VolumeLevel::High => &self.volume_high,
        }
    }

    #[must_use]
    pub fn battery_icon(&self, battery: BatteryState) -> &str {
        if !battery.present {
            &self.unavailable
        } else if battery.charging {
            &self.battery_charging
        } else {
            &self.battery
        }
    }
}

/// One layout as both sides of the protocol know it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CanonicalLayout {
    /// Wire identifier carried by `SetLayout`. Explicit, and never inferred
    /// from this table's order.
    pub id: LayoutId,
    /// Name used by the window manager's configuration and IPC.
    pub name: &'static str,
    /// Compact symbol the bar shows in its layout pill.
    pub symbol: &'static str,
    /// Human-facing name for UI with room for more than the symbol.
    pub label: &'static str,
}

impl CanonicalLayout {
    const fn new(id: u32, name: &'static str, symbol: &'static str, label: &'static str) -> Self {
        Self {
            id: LayoutId(id),
            name,
            symbol,
            label,
        }
    }
}

/// Canonical JWM layout protocol mapping — the one table both sides read.
///
/// The window manager derives its symbols, labels, names and cycle order from
/// this array, and every bar builds its layout picker from it, so a layout
/// added here appears in both without a second edit and without either side
/// silently offering a layout the other does not have. Rows are in *cycle*
/// order (the order `cyclelayout` visits them), which is also the order a
/// picker should present; the wire ID lives in the row precisely because that
/// order is a presentation choice and the ID is not.
pub const CANONICAL_LAYOUTS: [CanonicalLayout; 13] = [
    CanonicalLayout::new(0, "tile", "[]=", "Tile"),
    CanonicalLayout::new(3, "fibonacci", "[@]", "Fibonacci"),
    CanonicalLayout::new(4, "centeredmaster", "|M|", "Centered Master"),
    CanonicalLayout::new(5, "bstack", "TTT", "Bottom Stack"),
    CanonicalLayout::new(6, "grid", "HHH", "Grid"),
    CanonicalLayout::new(7, "deck", "[D]", "Deck"),
    CanonicalLayout::new(8, "threecol", "|||", "Three Column"),
    CanonicalLayout::new(9, "tatami", "[+]", "Tatami"),
    CanonicalLayout::new(2, "monocle", "[M]", "Monocle"),
    CanonicalLayout::new(10, "fullscreen", "[ ]", "Fullscreen"),
    CanonicalLayout::new(11, "scrolling", "[S]", "Scrolling"),
    CanonicalLayout::new(12, "vstack", "V[]", "V-Stack"),
    CanonicalLayout::new(1, "float", "><>", "Float"),
];

/// How many layouts this build of the protocol knows about.
pub const CANONICAL_LAYOUT_COUNT: usize = CANONICAL_LAYOUTS.len();

/// Maximum number of layout choices a bar will materialize from one window
/// manager snapshot. A layout menu is interactive UI, not an unbounded wire
/// dump; this also keeps a malformed `layout_count` from expanding to billions
/// of synthetic rows.
pub const MAX_LAYOUT_CHOICES: usize = 256;

/// Owned, serializable layout catalog entry for toolkit and web state stores.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LayoutCatalogEntry {
    pub id: LayoutId,
    pub symbol: String,
}

/// Return an owned canonical catalog in cycle order.
#[must_use]
pub fn canonical_layout_catalog() -> [LayoutCatalogEntry; CANONICAL_LAYOUT_COUNT] {
    CANONICAL_LAYOUTS.map(|layout| LayoutCatalogEntry {
        id: layout.id,
        symbol: layout.symbol.to_owned(),
    })
}

/// Look up an exact canonical symbol, ignoring surrounding whitespace.
#[must_use]
pub fn canonical_layout_id(symbol: &str) -> Option<LayoutId> {
    let symbol = symbol.trim();
    CANONICAL_LAYOUTS
        .iter()
        .find_map(|layout| (layout.symbol == symbol).then_some(layout.id))
}

/// Look up a canonical symbol by protocol ID.
#[must_use]
pub fn canonical_layout_symbol(id: LayoutId) -> Option<&'static str> {
    canonical_layout(id).map(|layout| layout.symbol)
}

/// Look up a whole row by protocol ID.
#[must_use]
pub fn canonical_layout(id: LayoutId) -> Option<&'static CanonicalLayout> {
    CANONICAL_LAYOUTS.iter().find(|layout| layout.id == id)
}

/// Whether a window manager offering `offered` layouts accepts `id`.
///
/// `None` means the window manager did not say how many it has, and a bar then
/// trusts its own catalog. Identifiers are dense and appended as layouts are
/// added — see `canonical_layout_ids_are_dense_and_appended` — so "the first
/// `offered` identifiers" is exactly "the layouts a smaller compositor has".
#[must_use]
pub fn layout_is_offered(id: LayoutId, offered: Option<usize>) -> bool {
    offered.is_none_or(|count| (id.0 as usize) < count)
}

/// Identifiers a window manager offers that this build has no row for.
///
/// A bar built before a layout existed still lets the user reach it, labelled
/// by number, instead of pretending the compositor has fewer layouts than it
/// does.
#[must_use]
pub fn unknown_layout_ids(offered: Option<usize>) -> std::ops::Range<u32> {
    let first_unknown = CANONICAL_LAYOUT_COUNT as u32;
    let last = offered
        .unwrap_or(CANONICAL_LAYOUT_COUNT)
        .min(MAX_LAYOUT_CHOICES) as u32;
    first_unknown..last.max(first_unknown)
}

/// Label for a layout this build has no symbol for.
#[must_use]
pub fn unknown_layout_label(id: LayoutId) -> String {
    format!("L{}", id.0)
}

/// Look up a whole row by window-manager layout name, case-insensitively.
#[must_use]
pub fn canonical_layout_by_name(name: &str) -> Option<&'static CanonicalLayout> {
    let name = name.trim();
    CANONICAL_LAYOUTS
        .iter()
        .find(|layout| layout.name.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn percent(value: f64) -> Percent {
        Percent::new(value).expect("test percentage is valid")
    }

    #[test]
    fn usage_tones_cover_boundaries_and_unavailable() {
        assert_eq!(usage_tone(None), MetricTone::Unavailable);
        assert_eq!(usage_tone(Some(percent(0.0))), MetricTone::Good);
        assert_eq!(usage_tone(Some(percent(30.0))), MetricTone::Good);
        assert_eq!(usage_tone(Some(percent(30.01))), MetricTone::Warning);
        assert_eq!(usage_tone(Some(percent(60.0))), MetricTone::Warning);
        assert_eq!(usage_tone(Some(percent(60.01))), MetricTone::High);
        assert_eq!(usage_tone(Some(percent(80.0))), MetricTone::High);
        assert_eq!(usage_tone(Some(percent(80.01))), MetricTone::Critical);
        assert_eq!(usage_tone(Some(percent(100.0))), MetricTone::Critical);
    }

    #[test]
    fn threshold_profiles_reject_ambiguous_ordering() {
        assert_eq!(
            UsageThresholds::new(percent(30.0), percent(30.0), percent(80.0)),
            Err(ThresholdError::UsageNotStrictlyIncreasing)
        );
        assert_eq!(
            BatteryThresholds::new(percent(20.0), percent(50.0)),
            Err(ThresholdError::BatteryNotStrictlyDescending)
        );
        assert_eq!(
            VolumeThresholds::new(percent(67.0), percent(34.0)),
            Err(ThresholdError::VolumeNotStrictlyIncreasing)
        );
    }

    #[test]
    fn battery_tones_preserve_absence_and_unknown_capacity() {
        assert_eq!(
            battery_tone(BatteryState::absent()),
            MetricTone::Unavailable
        );
        assert_eq!(
            battery_tone(BatteryState::present(None, false)),
            MetricTone::Unavailable
        );
        assert_eq!(
            battery_tone(BatteryState::present(Some(percent(50.01)), false)),
            MetricTone::Good
        );
        assert_eq!(
            battery_tone(BatteryState::present(Some(percent(50.0)), false)),
            MetricTone::Warning
        );
        assert_eq!(
            battery_tone(BatteryState::present(Some(percent(20.01)), false)),
            MetricTone::Warning
        );
        assert_eq!(
            battery_tone(BatteryState::present(Some(percent(20.0)), false)),
            MetricTone::Critical
        );
    }

    #[test]
    fn volume_levels_cover_device_mute_and_band_boundaries() {
        assert_eq!(
            volume_level(AudioState::default()),
            VolumeLevel::Unavailable
        );
        assert_eq!(
            volume_level(AudioState::new(Some(percent(40.0)), true)),
            VolumeLevel::Muted
        );
        assert_eq!(
            volume_level(AudioState::new(Some(percent(0.0)), false)),
            VolumeLevel::Muted
        );
        assert_eq!(
            volume_level(AudioState::new(Some(percent(33.99)), false)),
            VolumeLevel::Low
        );
        assert_eq!(
            volume_level(AudioState::new(Some(percent(34.0)), false)),
            VolumeLevel::Medium
        );
        assert_eq!(
            volume_level(AudioState::new(Some(percent(66.99)), false)),
            VolumeLevel::Medium
        );
        assert_eq!(
            volume_level(AudioState::new(Some(percent(67.0)), false)),
            VolumeLevel::High
        );

        let without_control = AudioDeviceInfo {
            volume: 80,
            has_volume_control: false,
            ..AudioDeviceInfo::default()
        };
        assert_eq!(
            volume_level_for_device(Some(&without_control)),
            VolumeLevel::Unavailable
        );
        assert_eq!(volume_level_for_device(None), VolumeLevel::Unavailable);
    }

    #[test]
    fn binary_byte_formatting_uses_iec_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(1024_u64.pow(3)), "1.0 GiB");
    }

    #[test]
    fn icon_lookup_is_dynamic_and_never_indexes_past_configuration() {
        let mut icons = IconSet::nerd_font();
        icons.tags.truncate(1);
        assert_eq!(icons.tag_icon(0), Cow::Borrowed("\u{F0A1E}"));
        assert_eq!(icons.tag_icon(9), Cow::<str>::Owned("10".to_owned()));

        icons.tag_fallback = TagFallback::Icon("□".to_owned());
        assert_eq!(icons.tag_icon(9), Cow::Borrowed("□"));
        icons.tag_fallback = TagFallback::Icon(String::new());
        assert_eq!(icons.tag_icon(9), Cow::<str>::Owned("10".to_owned()));
    }

    #[test]
    fn monitor_labels_use_configured_glyphs_then_compact_fallbacks() {
        let icons = IconSet::nerd_font();
        assert_eq!(
            icons.monitor_label(MonitorId(0)),
            Cow::Borrowed("\u{F02DA}")
        );
        assert_eq!(
            icons.monitor_label(MonitorId(4)),
            Cow::<str>::Owned("M4".to_owned())
        );
        assert_eq!(compact_monitor_label(MonitorId(-1)), "M-1");
    }

    #[test]
    fn canonical_layout_ids_are_explicit_and_stable() {
        let catalog = canonical_layout_catalog();
        // Cycle order leads with tile and ends with float; the IDs in between
        // are deliberately not the array index.
        assert_eq!(catalog[0].id, LayoutId(0));
        assert_eq!(catalog[0].symbol, "[]=");
        assert_eq!(catalog[1].id, LayoutId(3));
        assert_eq!(catalog[1].symbol, "[@]");
        assert_eq!(catalog[CANONICAL_LAYOUT_COUNT - 1].id, LayoutId(1));
        assert_eq!(catalog[CANONICAL_LAYOUT_COUNT - 1].symbol, "><>");
        assert_eq!(canonical_layout_id("  ><>  "), Some(LayoutId(1)));
        assert_eq!(canonical_layout_id("<><>"), None);
        assert_eq!(canonical_layout_symbol(LayoutId(2)), Some("[M]"));
        assert_eq!(canonical_layout_symbol(LayoutId(99)), None);
        assert_eq!(
            canonical_layout(LayoutId(2)).map(|l| l.name),
            Some("monocle")
        );
        assert_eq!(
            canonical_layout_by_name("CenteredMaster").map(|l| l.id),
            Some(LayoutId(4))
        );
        assert_eq!(canonical_layout_by_name("spiral"), None);
    }

    /// A bar reconciles its own catalog with the layout *count* a compositor
    /// reports, which only works while identifiers are dense and a new layout
    /// takes the next one. Adding a layout with a gap in the identifiers would
    /// make the older bars around it drop entries they can in fact enter.
    #[test]
    fn canonical_layout_ids_are_dense_and_appended() {
        let mut ids: Vec<u32> = CANONICAL_LAYOUTS.iter().map(|layout| layout.id.0).collect();
        ids.sort_unstable();
        assert_eq!(ids, (0..CANONICAL_LAYOUT_COUNT as u32).collect::<Vec<_>>());
    }

    #[test]
    fn unknown_layout_iteration_is_bounded_for_malformed_counts() {
        let unknown = unknown_layout_ids(Some(usize::MAX));

        assert_eq!(unknown.len(), MAX_LAYOUT_CHOICES - CANONICAL_LAYOUT_COUNT);
        assert_eq!(unknown.last(), Some((MAX_LAYOUT_CHOICES - 1) as u32));
    }

    /// Both sides index this table by ID, and the bar labels an unknown ID by
    /// its number — so a duplicate ID or symbol would quietly make one layout
    /// unreachable rather than fail anywhere visible.
    #[test]
    fn every_canonical_layout_is_uniquely_addressable() {
        for (index, layout) in CANONICAL_LAYOUTS.iter().enumerate() {
            assert!(!layout.name.is_empty() && !layout.symbol.is_empty());
            for other in CANONICAL_LAYOUTS.iter().skip(index + 1) {
                assert_ne!(layout.id, other.id, "duplicate id: {}", layout.name);
                assert_ne!(layout.name, other.name, "duplicate name: {}", layout.name);
                assert_ne!(
                    layout.symbol, other.symbol,
                    "duplicate symbol: {}",
                    layout.name
                );
            }
        }
    }
}
