//! Backend-neutral status-bar state, presentation, and explicit adapters.
//!
//! The default build contains the pure [`BarModel`], [`BarRuntime`], and
//! renderer-neutral [`presentation`] scene. Native providers, transports,
//! Linux descriptors, logging, and Cairo rendering are opt-in features.

use serde::{Deserialize, Serialize};

#[cfg(feature = "provider-alsa")]
pub mod audio_manager;
#[cfg(feature = "provider-battery-sysfs")]
pub mod battery;
#[cfg(feature = "provider-brightnessctl")]
pub mod brightness;
#[cfg(feature = "render-cairo")]
mod cairo_renderer;
#[cfg(feature = "config-toml")]
pub mod config;
pub mod controls;
pub mod display;
pub mod frontend;
#[cfg(feature = "runtime-linux")]
pub mod linux;
#[cfg(feature = "logging-flexi")]
pub mod logging;
pub mod model;
#[cfg(feature = "provider-network-sysfs")]
pub mod network;
#[cfg(all(feature = "runtime-linux", feature = "transport-shared"))]
pub mod notifier;
pub mod placement;
pub mod presentation;
pub mod runtime;
#[cfg(feature = "provider-system")]
pub mod system_monitor;
#[cfg(feature = "transport-shared")]
pub mod transport;
#[cfg(all(feature = "runtime-linux", feature = "transport-shared"))]
pub mod wake;

pub use controls::{
    BarPresentation, ControlSpec, ControlState, InputBindings, PresentationProjector,
    STATUS_ORDER_RIGHT_TO_LEFT, UNKNOWN_VALUE,
};
pub use display::{
    BatteryThresholds, IconSet, LayoutCatalogEntry, MetricTone, TagFallback, ThresholdError,
    UsageThresholds, VolumeLevel, VolumeThresholds, battery_tone, canonical_layout_catalog,
    canonical_layout_id, canonical_layout_symbol, compact_monitor_label, format_bytes,
    format_transfer_rate, usage_tone, volume_level, volume_level_for_device,
};
pub use frontend::{
    ActionRequest, ActionRequestError, FrontendEnvelope, FrontendPartitions, FrontendSession,
    SessionOutput, SnapshotCursor, snapshot_changes,
};
pub use model::{
    AudioDeviceInfo, AudioState, BarEffect, BarEvent, BarModel, BarSnapshot, BarView, BatteryState,
    BrightnessState, ClockState, LayoutId, MediaPlayback, MediaState, ModelConfig, ModelError,
    ModelUpdate, MonitorGeometry, MonitorId, NetworkState, Percent, PercentError, SystemDetails,
    SystemLoadAverage, SystemState, TagId, TagState, UserAction, WmCommand, WmSnapshot,
};
pub use placement::{
    BarPlacement, DockProperty, DockPropertyValue, DockWindowSpec, EwmhStrut, LayerShellAnchors,
    LayerShellLayer, LayerShellPlacement, PlacementError,
};
pub use runtime::{
    BarRuntime, DEFAULT_RUNTIME_TICK_INTERVAL, PlatformEffectFailure, PlatformEffectHandler,
    PlatformEffectReport, RuntimeAdapter, RuntimeConfigError, RuntimeFrame, RuntimeIssue,
    RuntimeSchedule, RuntimeUpdate,
};
#[cfg(feature = "transport-shared")]
pub use runtime::{DEFAULT_TRANSPORT_RETRY_INTERVAL, TransportRecoveryConfig, TransportStatus};

#[cfg(all(feature = "runtime-linux", feature = "transport-shared"))]
pub use notifier::{NotifierChange, SharedEventNotifier, TransportNotifierSlot};
#[cfg(feature = "transport-shared")]
pub use transport::{SendOutcome, SharedTransport};
#[cfg(all(feature = "runtime-linux", feature = "transport-shared"))]
pub use wake::{
    AlignedWakeThread, CoalescedNotifierForwarder, IntoWakeResult, TransportWakeChange,
    TransportWakeSlot, WakeAck, WakeWorkerStatus,
};

/// Optional renderer adapters.
pub mod render {
    /// Cairo/Pango renderer for [`crate::presentation::Scene`].
    #[cfg(feature = "render-cairo")]
    pub mod cairo {
        pub use crate::cairo_renderer::{
            CairoBar, CairoRenderer, CpuCanvas, CpuFrame, PangoTextMeasurer, PixelRect,
            PointerButton, PointerInput, PointerUpdate,
        };
    }
}

/// High-level visual theme preference. Concrete colors live in presentation
/// configuration or a renderer theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeMode {
    Dark,
    Light,
}

/// Semantic change classification used only to schedule work. Correct damage
/// is derived from old/new presentation scenes, not from these flags.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct DirtyBits(u32);

impl DirtyBits {
    pub const NONE: u32 = 0;
    pub const TIME_CHANGED: u32 = 1 << 0;
    pub const HOVER_CHANGED: u32 = 1 << 1;
    pub const MONITOR_CHANGED: u32 = 1 << 2;
    pub const AUDIO_CHANGED: u32 = 1 << 3;
    pub const SYSTEM_CHANGED: u32 = 1 << 4;
    pub const LAYOUT_CHANGED: u32 = 1 << 5;
    pub const THEME_CHANGED: u32 = 1 << 6;
    pub const BRIGHTNESS_CHANGED: u32 = 1 << 7;
    pub const BATTERY_CHANGED: u32 = 1 << 8;
    pub const CLIENT_CHANGED: u32 = 1 << 9;
    pub const GEOMETRY_CHANGED: u32 = 1 << 10;
    pub const NETWORK_CHANGED: u32 = 1 << 11;
    pub const MEDIA_CHANGED: u32 = 1 << 12;

    const KNOWN: u32 = Self::TIME_CHANGED
        | Self::HOVER_CHANGED
        | Self::MONITOR_CHANGED
        | Self::AUDIO_CHANGED
        | Self::SYSTEM_CHANGED
        | Self::LAYOUT_CHANGED
        | Self::THEME_CHANGED
        | Self::BRIGHTNESS_CHANGED
        | Self::BATTERY_CHANGED
        | Self::CLIENT_CHANGED
        | Self::GEOMETRY_CHANGED
        | Self::NETWORK_CHANGED
        | Self::MEDIA_CHANGED;

    #[must_use]
    pub const fn new(bits: u32) -> Self {
        Self(bits & Self::KNOWN)
    }

    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn set(&mut self, flag: u32) {
        self.0 |= flag & Self::KNOWN;
    }

    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    pub fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }

    #[must_use]
    pub const fn contains(self, flag: u32) -> bool {
        (self.0 & flag) != 0
    }

    #[must_use]
    pub const fn intersects(self, other: Self) -> bool {
        (self.0 & other.0) != 0
    }

    pub fn clear(&mut self) {
        self.0 = 0;
    }

    pub fn take(&mut self) -> Self {
        std::mem::take(self)
    }

    #[must_use]
    pub const fn all() -> Self {
        Self(Self::KNOWN)
    }
}

impl std::ops::BitOr for DirtyBits {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for DirtyBits {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl<'de> Deserialize<'de> for DirtyBits {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        u32::deserialize(deserializer).map(Self::new)
    }
}

#[cfg(test)]
mod tests {
    use super::DirtyBits;
    use serde::Deserialize;

    #[test]
    fn change_flags_normalize_accumulate_and_take() {
        let mut changes = DirtyBits::new(u32::MAX);
        assert_eq!(changes, DirtyBits::all());
        changes.remove(DirtyBits::new(DirtyBits::TIME_CHANGED));
        assert!(!changes.contains(DirtyBits::TIME_CHANGED));
        changes.set(DirtyBits::TIME_CHANGED);
        assert!(changes.take().contains(DirtyBits::TIME_CHANGED));
        assert!(changes.is_empty());
    }

    #[test]
    fn deserialized_change_flags_cannot_contain_unknown_bits() {
        use serde::de::value::{Error, U32Deserializer};

        let changes = DirtyBits::deserialize(U32Deserializer::<Error>::new(u32::MAX)).unwrap();
        assert_eq!(changes, DirtyBits::all());
    }
}
