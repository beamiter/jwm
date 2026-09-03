// src/backend/api.rs

use crate::backend::common_define::OutputId;
use crate::backend::common_define::{
    ColorScheme, CursorHandle, KeySym, Mods, Pixel, SchemeType, StdCursorKind, WindowId,
};
use crate::backend::error::BackendError;
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::fmt::Debug;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompositorMetrics {
    /// Live graphics platform ("glx/opengl", "egl/gles3", ...), so perf
    /// baselines can label the renderer actually chosen at runtime.
    #[serde(default)]
    pub renderer_api: String,
    pub fps: f32,
    pub frame_count: u64,
    pub avg_frame_time_ms: f32,
    pub max_frame_time_ms: f32,
    pub min_frame_time_ms: f32,
    /// Recent frame-time tail latency, calculated over the compositor's
    /// bounded sample window. These distinguish smooth averages from jank.
    pub frame_time_p95_ms: f32,
    pub frame_time_p99_ms: f32,
    pub gpu_load_percent: u32,
    pub cpu_load_percent: u32,
    pub draw_calls: u32,
    pub texture_memory_bytes: u64,
    pub blur_cache_hits: u64,
    pub blur_cache_misses: u64,
    pub blur_cache_hit_rate: f32,
    // P4: Temporal blur reuse metrics
    pub temporal_blur_reuse_count: u64,
    pub temporal_blur_total_count: u64,
    pub temporal_blur_reuse_rate: f32,
    pub dirty_regions_count: usize,
    pub dirty_fraction_percent: f32,
    pub window_count: usize,
    pub blur_quality: String,
    pub vrr_enabled: bool,
    pub vrr_active: bool, // VRR currently active for focused game window
    pub current_refresh_rate: u32, // Current target refresh rate (Hz)
    // Task 8: Input latency metrics
    pub input_latency_avg_ms: f32,
    pub input_latency_p50_ms: f32,
    pub input_latency_p95_ms: f32,
    pub input_latency_p99_ms: f32,
    // Phase 2-3: Optimization statistics
    pub direct_scanout_active: bool,
    pub direct_scanout_count: u64,
    pub direct_scanout_bypass_time_ms: u64,
    pub gl_state_changes_avoided: u32,
    pub profiling_enabled: bool,
    pub dirty_region_merge_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitTarget {
    Surface(WindowId),
    Background { output: Option<OutputId> },
}

/// A rectangle in the compositor's global physical-pixel coordinate space.
///
/// Bars use this for Dock slots and hover anchors.  Keeping the coordinate
/// contract at the backend boundary avoids making either compositor guess a
/// toolkit's logical scale or a Wayland surface's global position.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompositorRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl CompositorRect {
    #[must_use]
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    #[must_use]
    pub fn normalized(self) -> Option<Self> {
        (self.x.is_finite()
            && self.y.is_finite()
            && self.width.is_finite()
            && self.height.is_finite()
            && self.width > 0.0
            && self.height > 0.0)
            .then_some(self)
    }

    #[must_use]
    pub fn center(self) -> (f32, f32) {
        (self.x + self.width * 0.5, self.y + self.height * 0.5)
    }
}

#[derive(Clone, Debug)]
pub struct OutputIdentity {
    pub connector: String,
    pub vendor: Option<String>,
    pub product_code: Option<u16>,
    pub serial_number: Option<u32>,
    pub monitor_name: Option<String>,
    pub monitor_serial: Option<String>,
    pub stable_key: String,
}

impl OutputIdentity {
    pub fn connector_only(connector: impl Into<String>) -> Self {
        let connector = connector.into();
        Self {
            stable_key: connector.clone(),
            connector,
            vendor: None,
            product_code: None,
            serial_number: None,
            monitor_name: None,
            monitor_serial: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct OutputInfo {
    pub id: OutputId,
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub scale: f32,
    pub refresh_rate: u32,
    pub hdr_capable: bool,
    pub hdr_metadata: Option<crate::backend::edid::EdidHdrCapabilities>,
    pub identity: OutputIdentity,
}

#[derive(Clone, Debug)]
pub struct VrrCapabilities {
    pub supported: bool,
    pub current_enabled: bool,
    pub min_refresh_hz: u32,
    pub max_refresh_hz: u32,
}

/// Per-CRTC KMS color pipeline capabilities. A `_size` of 0 indicates the LUT
/// hardware is absent (and the matching `_supported` flag will be false). When
/// `_supported` is true, `_size` is the number of `drm_color_lut` entries the
/// kernel expects in a `*_LUT` blob. Future SOTA work uses these to offload
/// the encode/decode/CTM passes from the GL shader to fixed-function hardware.
#[derive(Clone, Debug, Default)]
pub struct KmsColorPipelineCaps {
    pub degamma_lut_supported: bool,
    pub degamma_lut_size: u32,
    pub gamma_lut_supported: bool,
    pub gamma_lut_size: u32,
    pub ctm_supported: bool,
}

/// Snapshot of one surface's wp-color-management-v1 image description, used by
/// the diagnostic IPC. All numeric fields are taken directly from the protocol
/// (named enums as u32, luminances in the protocol's scaled form).
#[derive(Clone, Debug)]
pub struct ColorManagedSurfaceInfo {
    /// Stringified wl_surface ObjectId.
    pub surface_object_id: String,
    /// Compositor-assigned image-description identity (monotonic).
    pub identity: u64,
    pub tf_named: Option<u32>,
    pub tf_power: Option<u32>,
    pub primaries_named: Option<u32>,
    pub primaries: Option<[i32; 8]>,
    pub min_lum: Option<u32>,
    pub max_lum: Option<u32>,
    pub reference_lum: Option<u32>,
    pub mastering_primaries: Option<[i32; 8]>,
    pub mastering_min_lum: Option<u32>,
    pub mastering_max_lum: Option<u32>,
    pub max_cll: Option<u32>,
    pub max_fall: Option<u32>,
}

/// Snapshot of the compositor's blur pipeline state, used by the diagnostic
/// IPC. Lets you verify Hz→strength selection and reuse rate without HW.
#[derive(Clone, Debug, Default)]
pub struct BlurStatus {
    /// Current blur strength (downsample levels).
    pub current_strength: u32,
    /// Whether temporal blur reuse is enabled.
    pub temporal_enabled: bool,
    /// EMA of frames where prior blur was reused (0-100).
    pub temporal_reuse_rate_pct: f32,
    /// Live `blur_strength_by_hz` lookup table, sorted ascending by Hz.
    pub hz_table: Vec<(u32, u32)>,
    /// Live per-output refresh rates: (monitor_id, hz). Monitor 0 == primary.
    pub per_monitor_hz: Vec<(u32, u32)>,
    /// Per-monitor blur-quality overrides: (monitor_id, "Full"|"Reduced"|"Minimal").
    pub blur_quality_by_monitor: Vec<(u32, String)>,
}

/// Snapshot of the WaterLily layer, used by the `get_waterlily_status` IPC.
/// Lets automation decide whether to toggle the effect and wait for frames
/// instead of blind-toggling and sleeping.
#[derive(Clone, Debug, Default)]
pub struct WaterlilyStatus {
    /// The user-facing effect toggle (`toggle_waterlily`).
    pub enabled: bool,
    /// True when the effect is enabled, a worker is connected, and a frame
    /// texture is on screen.
    pub active: bool,
    /// True while a WaterLily worker holds the wake socket.
    pub worker_connected: bool,
    /// Dimensions of the last uploaded frame, or zero before the first one.
    /// A depth above one means the worker publishes volumetric frames and the
    /// compositor is ray-marching them natively in 3D.
    pub frame_width: u32,
    pub frame_height: u32,
    pub frame_depth: u32,
    /// Sequence number of the last uploaded frame, or zero before the first.
    pub frame_sequence: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirectScanoutOutputStatus {
    pub output_name: String,
    pub eligible: bool,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirectScanoutStatus {
    pub enabled: bool,
    pub active: bool,
    pub current_window: Option<u64>,
    pub scanout_count: u64,
    pub bypass_time_ms: u64,
    pub candidate_count: usize,
    pub compositor_reason: String,
    pub kms_outputs: Vec<DirectScanoutOutputStatus>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PresentationTimingOutputStatus {
    pub output_name: String,
    pub refresh_interval_ms: f64,
    pub last_vblank_monotonic_ms: Option<u64>,
    pub last_vblank_ago_ms: Option<u64>,
    pub frame_pending: bool,
    pub frame_pending_for_ms: Option<u64>,
    pub watchdog_timeout_ms: u64,
    pub frame_callback_roots: usize,
    pub visible_surface_count: usize,
    pub send_frame_callbacks: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PresentationTimingStatus {
    pub any_frame_pending: bool,
    pub outputs: Vec<PresentationTimingOutputStatus>,
}

/// Latest color-delivery policy decision and the last presentation that was
/// actually observed for each output.
///
/// The policy decision is intentionally separate from `last_success`:
/// configuring a profile, installing KMS properties, or even queueing a
/// framebuffer does not prove that the display presented it. The actual route
/// may also become direct scanout on an individual output. Backends update
/// `last_success` only at their presentation-completion boundary (a DRM
/// page-flip/vblank for udev).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ColorDeliveryStatus {
    pub schema_version: u32,
    pub observation: String,
    /// Monotonic count of output presentations promoted into `last_success`;
    /// policy evaluation, queue failure, and participation changes do not
    /// increment it.
    pub generation: u64,
    pub last_policy_decision: Option<ColorDeliveryPolicyDecisionStatus>,
    pub outputs: Vec<ColorDeliveryOutputStatus>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ColorDeliveryPolicyDecisionStatus {
    pub sequence: u64,
    pub composited_route: String,
    pub blocked: bool,
    pub reason: Option<String>,
    pub scene_linear_active: bool,
    pub linear_tail_safe: bool,
    /// Frame-tail classes observed outside the common linear workspace when
    /// this decision was evaluated. `Some([])` means the inventory ran and
    /// found none; `None` means it was not applicable or an older schema-v1
    /// payload did not record it. The inventory is diagnostic, not proof that
    /// any one class caused the selected route.
    #[serde(default, deserialize_with = "deserialize_linear_tail_blockers")]
    pub linear_tail_blockers: Option<Vec<String>>,
    /// Per-class view of the KMS-assembled external elements behind
    /// `linear_tail_blockers`: cursor, drag icon, session lock, and the top /
    /// overlay layer classes, each with its visibility and importability
    /// basis. `None` means the decision predates or skipped the external
    /// element plan (legacy path or older schema-v1 payload). The list is
    /// diagnostic; the route is still selected from the aggregate bits above.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_external_element_classes"
    )]
    pub external_elements: Option<Vec<ExternalElementClassStatus>>,
}

/// Diagnostic status of one KMS-assembled external element class for one
/// policy decision. All names are stable snake_case tokens; `outputs` lists
/// the participating outputs where the class produces pixels.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExternalElementClassStatus {
    /// The class and (when blocked) blocker wire name: one of `cursor`,
    /// `drag_icon`, `session_lock_surface`, `top_layer_surface`,
    /// `overlay_layer_surface`.
    pub class: String,
    /// The class produces pixels on at least one participating output.
    pub visible: bool,
    /// Every drawable surface passed the import precheck, so the
    /// common-linear adapter may own the class. `false` while hidden.
    pub importable: bool,
    /// Who assembles the class this frame: `kms_external` (outside the
    /// compositor texture), `common_linear` (internalized into the
    /// compositor's shared linear-sRGB target ahead of the per-output
    /// transform), or `none` (hidden).
    pub assembly: String,
    /// The contributed linear-tail blocker name when the class blocks.
    pub blocker: Option<String>,
    pub outputs: Vec<String>,
    /// Stable token stating the visibility/importability judgment.
    pub basis: String,
}

/// Maximum number of blocker classes accepted from either the typed status
/// schema or its JSON diagnostic representation.
pub const MAX_LINEAR_TAIL_BLOCKERS: usize = 32;
/// Maximum bytes in one stable or future blocker name.
pub const MAX_LINEAR_TAIL_BLOCKER_NAME_BYTES: usize = 64;
/// Maximum number of external element class entries accepted from the typed
/// status schema. The compositor emits one entry per class (currently five);
/// the bound only guards against malformed or future payloads.
pub const MAX_EXTERNAL_ELEMENT_CLASSES: usize = 16;

/// Stable blocker names recognized by the current compositor. Future names
/// that satisfy the grammar remain valid and are counted as unknown.
/// `top_or_overlay_layer_surface` is the legacy aggregate from before the
/// per-layer split; it stays recognized so recorded payloads keep classifying
/// as known, but the compositor now emits only the granular names.
pub(crate) const LINEAR_TAIL_BLOCKER_NAMES: [&str; 8] = [
    "compositor_encoded_tail",
    "capture_readback",
    "session_lock_surface",
    "drag_icon",
    "cursor",
    "top_or_overlay_layer_surface",
    "top_layer_surface",
    "overlay_layer_surface",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinearTailInventoryObservation {
    Unknown,
    ObservedClear,
    ObservedBlocked,
    Malformed,
}

impl LinearTailInventoryObservation {
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::ObservedClear => "observed_clear",
            Self::ObservedBlocked => "observed_blocked",
            Self::Malformed => "malformed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinearTailInventoryIssue {
    NonArray,
    TooManyBlockers,
    NonStringBlocker,
    InvalidBlockerName,
    DuplicateBlocker,
    MissingSafeFlag,
    SafeFlagMismatch,
}

impl LinearTailInventoryIssue {
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::NonArray => "non_array",
            Self::TooManyBlockers => "too_many_blockers",
            Self::NonStringBlocker => "non_string_blocker",
            Self::InvalidBlockerName => "invalid_blocker_name",
            Self::DuplicateBlocker => "duplicate_blocker",
            Self::MissingSafeFlag => "missing_safe_flag",
            Self::SafeFlagMismatch => "safe_flag_mismatch",
        }
    }
}

/// Content-free classification of one linear-tail inventory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinearTailInventorySummary {
    pub observation: LinearTailInventoryObservation,
    pub issue: Option<LinearTailInventoryIssue>,
    pub blocker_count: usize,
    pub known_blocker_count: usize,
    pub unknown_blocker_count: usize,
}

impl LinearTailInventorySummary {
    #[must_use]
    pub const fn is_consistent(self) -> bool {
        self.issue.is_none()
    }
}

impl ColorDeliveryPolicyDecisionStatus {
    /// Whether an observed inventory agrees with its aggregate safety bit.
    /// `None` remains valid because legacy/inapplicable payloads are unknown;
    /// unknown future blocker names are accepted, while empty names and
    /// duplicates are rejected as malformed diagnostics.
    #[must_use]
    pub fn linear_tail_inventory_consistent(&self) -> bool {
        self.linear_tail_inventory_summary().is_consistent()
    }

    /// Classify unknown, clear, blocked, and malformed inventories while
    /// keeping future valid names forward-compatible.
    #[must_use]
    pub fn linear_tail_inventory_summary(&self) -> LinearTailInventorySummary {
        match self.linear_tail_blockers.as_deref() {
            None => summarize_linear_tail_names(Some(self.linear_tail_safe), None),
            Some(blockers) if blockers.len() > MAX_LINEAR_TAIL_BLOCKERS => {
                malformed_linear_tail_summary(LinearTailInventoryIssue::TooManyBlockers)
            }
            Some(blockers) => summarize_linear_tail_names(
                Some(self.linear_tail_safe),
                Some(blockers.iter().map(String::as_str).collect()),
            ),
        }
    }

    /// The observed blocker list, distinguishing unknown (`None`) from an
    /// observed clear inventory (`Some([])`).
    #[must_use]
    pub fn observed_linear_tail_blockers(&self) -> Option<&[String]> {
        self.linear_tail_blockers.as_deref()
    }
}

pub(crate) fn linear_tail_blocker_name_is_valid(blocker: &str) -> bool {
    !blocker.is_empty()
        && blocker.len() <= MAX_LINEAR_TAIL_BLOCKER_NAME_BYTES
        && blocker
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

#[must_use]
pub fn is_known_linear_tail_blocker_name(blocker: &str) -> bool {
    LINEAR_TAIL_BLOCKER_NAMES.contains(&blocker)
}

fn malformed_linear_tail_summary(issue: LinearTailInventoryIssue) -> LinearTailInventorySummary {
    LinearTailInventorySummary {
        observation: LinearTailInventoryObservation::Malformed,
        issue: Some(issue),
        blocker_count: 0,
        known_blocker_count: 0,
        unknown_blocker_count: 0,
    }
}

fn summarize_linear_tail_names(
    safe: Option<bool>,
    blockers: Option<Vec<&str>>,
) -> LinearTailInventorySummary {
    let Some(blockers) = blockers else {
        return LinearTailInventorySummary {
            observation: LinearTailInventoryObservation::Unknown,
            issue: None,
            blocker_count: 0,
            known_blocker_count: 0,
            unknown_blocker_count: 0,
        };
    };
    if blockers.len() > MAX_LINEAR_TAIL_BLOCKERS {
        return malformed_linear_tail_summary(LinearTailInventoryIssue::TooManyBlockers);
    }
    if blockers
        .iter()
        .any(|blocker| !linear_tail_blocker_name_is_valid(blocker))
    {
        return malformed_linear_tail_summary(LinearTailInventoryIssue::InvalidBlockerName);
    }
    if blockers
        .iter()
        .enumerate()
        .any(|(index, blocker)| blockers[..index].contains(blocker))
    {
        return malformed_linear_tail_summary(LinearTailInventoryIssue::DuplicateBlocker);
    }
    let blocker_count = blockers.len();
    let known_blocker_count = blockers
        .iter()
        .filter(|blocker| is_known_linear_tail_blocker_name(blocker))
        .count();
    let summary = LinearTailInventorySummary {
        observation: if blockers.is_empty() {
            LinearTailInventoryObservation::ObservedClear
        } else {
            LinearTailInventoryObservation::ObservedBlocked
        },
        issue: if safe.is_none() {
            Some(LinearTailInventoryIssue::MissingSafeFlag)
        } else if safe != Some(blockers.is_empty()) {
            Some(LinearTailInventoryIssue::SafeFlagMismatch)
        } else {
            None
        },
        blocker_count,
        known_blocker_count,
        unknown_blocker_count: blocker_count.saturating_sub(known_blocker_count),
    };
    if summary.issue.is_some() {
        LinearTailInventorySummary {
            observation: LinearTailInventoryObservation::Malformed,
            ..summary
        }
    } else {
        summary
    }
}

pub(crate) fn summarize_linear_tail_inventory_value(
    safe: Option<bool>,
    value: Option<&serde_json::Value>,
) -> LinearTailInventorySummary {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return summarize_linear_tail_names(safe, None);
    };
    let Some(blockers) = value.as_array() else {
        return malformed_linear_tail_summary(LinearTailInventoryIssue::NonArray);
    };
    if blockers.len() > MAX_LINEAR_TAIL_BLOCKERS {
        return malformed_linear_tail_summary(LinearTailInventoryIssue::TooManyBlockers);
    }
    let Some(names) = blockers
        .iter()
        .map(serde_json::Value::as_str)
        .collect::<Option<Vec<_>>>()
    else {
        return malformed_linear_tail_summary(LinearTailInventoryIssue::NonStringBlocker);
    };
    summarize_linear_tail_names(safe, Some(names))
}

fn deserialize_linear_tail_blockers<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct OptionalInventoryVisitor;
    struct InventoryVisitor;
    struct BoundedBlockerName(String);

    impl<'de> Deserialize<'de> for BoundedBlockerName {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            struct NameVisitor;

            impl serde::de::Visitor<'_> for NameVisitor {
                type Value = BoundedBlockerName;

                fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    write!(
                        formatter,
                        "a blocker name of at most {MAX_LINEAR_TAIL_BLOCKER_NAME_BYTES} bytes"
                    )
                }

                fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
                where
                    E: serde::de::Error,
                {
                    if value.len() > MAX_LINEAR_TAIL_BLOCKER_NAME_BYTES {
                        return Err(E::custom("linear-tail blocker name exceeds its byte limit"));
                    }
                    Ok(BoundedBlockerName(value.to_owned()))
                }

                fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
                where
                    E: serde::de::Error,
                {
                    if value.len() > MAX_LINEAR_TAIL_BLOCKER_NAME_BYTES {
                        return Err(E::custom("linear-tail blocker name exceeds its byte limit"));
                    }
                    Ok(BoundedBlockerName(value))
                }
            }

            deserializer.deserialize_string(NameVisitor)
        }
    }

    impl<'de> serde::de::Visitor<'de> for OptionalInventoryVisitor {
        type Value = Option<Vec<String>>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("null or a bounded linear-tail blocker array")
        }

        fn visit_none<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserializer.deserialize_seq(InventoryVisitor).map(Some)
        }
    }

    impl<'de> serde::de::Visitor<'de> for InventoryVisitor {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a bounded linear-tail blocker array")
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let mut blockers = Vec::with_capacity(
                sequence
                    .size_hint()
                    .unwrap_or(0)
                    .min(MAX_LINEAR_TAIL_BLOCKERS),
            );
            while blockers.len() < MAX_LINEAR_TAIL_BLOCKERS {
                let Some(blocker) = sequence.next_element::<BoundedBlockerName>()? else {
                    return Ok(blockers);
                };
                blockers.push(blocker.0);
            }
            if sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {
                return Err(serde::de::Error::custom(
                    "linear-tail blocker inventory exceeds its entry limit",
                ));
            }
            Ok(blockers)
        }
    }

    deserializer.deserialize_option(OptionalInventoryVisitor)
}

fn deserialize_external_element_classes<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<ExternalElementClassStatus>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct OptionalClassesVisitor;
    struct ClassesVisitor;

    impl<'de> serde::de::Visitor<'de> for OptionalClassesVisitor {
        type Value = Option<Vec<ExternalElementClassStatus>>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("null or a bounded external element class array")
        }

        fn visit_none<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserializer.deserialize_seq(ClassesVisitor).map(Some)
        }
    }

    impl<'de> serde::de::Visitor<'de> for ClassesVisitor {
        type Value = Vec<ExternalElementClassStatus>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a bounded external element class array")
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let mut classes = Vec::with_capacity(
                sequence
                    .size_hint()
                    .unwrap_or(0)
                    .min(MAX_EXTERNAL_ELEMENT_CLASSES),
            );
            while classes.len() < MAX_EXTERNAL_ELEMENT_CLASSES {
                let Some(class) = sequence.next_element::<ExternalElementClassStatus>()? else {
                    return Ok(classes);
                };
                classes.push(class);
            }
            if sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {
                return Err(serde::de::Error::custom(
                    "external element class list exceeds its entry limit",
                ));
            }
            Ok(classes)
        }
    }

    deserializer.deserialize_option(OptionalClassesVisitor)
}

#[cfg(test)]
mod linear_tail_inventory_tests {
    use super::*;

    fn status(safe: bool, blockers: Option<Vec<&str>>) -> ColorDeliveryPolicyDecisionStatus {
        ColorDeliveryPolicyDecisionStatus {
            sequence: 1,
            composited_route: "global_srgb_fallback".into(),
            blocked: false,
            reason: None,
            scene_linear_active: true,
            linear_tail_safe: safe,
            linear_tail_blockers: blockers
                .map(|blockers| blockers.into_iter().map(ToOwned::to_owned).collect()),
            external_elements: None,
        }
    }

    #[test]
    fn summaries_distinguish_unknown_clear_blocked_and_future_names() {
        let unknown = status(false, None).linear_tail_inventory_summary();
        assert_eq!(unknown.observation, LinearTailInventoryObservation::Unknown);
        assert!(unknown.is_consistent());

        let clear = status(true, Some(Vec::new())).linear_tail_inventory_summary();
        assert_eq!(
            clear.observation,
            LinearTailInventoryObservation::ObservedClear
        );
        assert_eq!(clear.blocker_count, 0);

        let blocked =
            status(false, Some(vec!["cursor", "future_blocker_2"])).linear_tail_inventory_summary();
        assert_eq!(
            blocked.observation,
            LinearTailInventoryObservation::ObservedBlocked
        );
        assert_eq!(blocked.blocker_count, 2);
        assert_eq!(blocked.known_blocker_count, 1);
        assert_eq!(blocked.unknown_blocker_count, 1);
        assert!(blocked.is_consistent());
        assert!(is_known_linear_tail_blocker_name("cursor"));
        assert!(!is_known_linear_tail_blocker_name("future_blocker_2"));
    }

    #[test]
    fn raw_inventory_classification_reports_one_stable_issue() {
        let cases = [
            (
                Some(false),
                serde_json::json!("cursor"),
                LinearTailInventoryIssue::NonArray,
            ),
            (
                Some(false),
                serde_json::json!(["cursor", 1]),
                LinearTailInventoryIssue::NonStringBlocker,
            ),
            (
                Some(false),
                serde_json::json!(["Bad-Name"]),
                LinearTailInventoryIssue::InvalidBlockerName,
            ),
            (
                Some(false),
                serde_json::json!(["cursor", "cursor"]),
                LinearTailInventoryIssue::DuplicateBlocker,
            ),
            (
                None,
                serde_json::json!(["cursor"]),
                LinearTailInventoryIssue::MissingSafeFlag,
            ),
            (
                Some(true),
                serde_json::json!(["cursor"]),
                LinearTailInventoryIssue::SafeFlagMismatch,
            ),
        ];
        for (safe, value, expected) in cases {
            let summary = summarize_linear_tail_inventory_value(safe, Some(&value));
            assert_eq!(
                summary.observation,
                LinearTailInventoryObservation::Malformed
            );
            assert_eq!(summary.issue, Some(expected));
            assert!(!summary.is_consistent());
        }
    }

    #[test]
    fn blocker_entry_limit_applies_to_typed_raw_and_deserialized_schemas() {
        let blockers = vec!["future"; MAX_LINEAR_TAIL_BLOCKERS + 1];
        let typed = status(false, Some(blockers.clone())).linear_tail_inventory_summary();
        assert_eq!(typed.issue, Some(LinearTailInventoryIssue::TooManyBlockers));

        let raw = serde_json::json!(blockers);
        assert_eq!(
            summarize_linear_tail_inventory_value(Some(false), Some(&raw)).issue,
            Some(LinearTailInventoryIssue::TooManyBlockers)
        );

        let encoded = serde_json::json!({
            "sequence": 1,
            "composited_route": "global_srgb_fallback",
            "blocked": false,
            "reason": null,
            "scene_linear_active": true,
            "linear_tail_safe": false,
            "linear_tail_blockers": blockers,
        });
        assert!(serde_json::from_value::<ColorDeliveryPolicyDecisionStatus>(encoded).is_err());

        let huge_name = serde_json::json!({
            "sequence": 1,
            "composited_route": "global_srgb_fallback",
            "blocked": false,
            "reason": null,
            "scene_linear_active": true,
            "linear_tail_safe": false,
            "linear_tail_blockers": ["x".repeat(1024 * 1024)],
        });
        assert!(serde_json::from_value::<ColorDeliveryPolicyDecisionStatus>(huge_name).is_err());
    }

    #[test]
    fn blocker_name_table_stays_unique_and_recognizes_split_layer_names() {
        let mut sorted = LINEAR_TAIL_BLOCKER_NAMES.to_vec();
        sorted.sort_unstable();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), before, "blocker name table must not repeat");

        for name in [
            "cursor",
            "drag_icon",
            "session_lock_surface",
            "top_layer_surface",
            "overlay_layer_surface",
        ] {
            assert!(is_known_linear_tail_blocker_name(name), "{name}");
            assert!(linear_tail_blocker_name_is_valid(name), "{name}");
        }
        // The pre-split aggregate stays recognized for recorded payloads even
        // though the compositor no longer emits it.
        assert!(is_known_linear_tail_blocker_name(
            "top_or_overlay_layer_surface"
        ));
    }

    #[test]
    fn external_element_classes_serialize_bounded_and_legacy_absent() {
        let mut decision = status(false, Some(vec!["cursor"]));
        assert_eq!(decision.external_elements, None);
        // Legacy schema-v1 payloads never carried the field; absence and an
        // explicit null both decode as not-applicable.
        let encoded = serde_json::to_value(&decision).unwrap();
        assert!(encoded.get("external_elements").is_none());
        let decoded: ColorDeliveryPolicyDecisionStatus = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.external_elements, None);
        let with_null = serde_json::json!({
            "sequence": 1,
            "composited_route": "global_srgb_fallback",
            "blocked": false,
            "reason": null,
            "scene_linear_active": true,
            "linear_tail_safe": false,
            "linear_tail_blockers": ["cursor"],
            "external_elements": null,
        });
        let decoded: ColorDeliveryPolicyDecisionStatus = serde_json::from_value(with_null).unwrap();
        assert_eq!(decoded.external_elements, None);

        decision.external_elements = Some(vec![ExternalElementClassStatus {
            class: "cursor".into(),
            visible: true,
            importable: true,
            assembly: "kms_external".into(),
            blocker: Some("cursor".into()),
            outputs: vec!["HDMI-A-1".into()],
            basis: "pointer_on_output".into(),
        }]);
        let encoded = serde_json::to_value(&decision).unwrap();
        assert_eq!(encoded["external_elements"][0]["class"], "cursor");
        assert_eq!(encoded["external_elements"][0]["assembly"], "kms_external");
        let decoded: ColorDeliveryPolicyDecisionStatus = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, decision);

        let entry = serde_json::json!({
            "class": "cursor",
            "visible": true,
            "importable": true,
            "assembly": "kms_external",
            "blocker": "cursor",
            "outputs": [],
            "basis": "pointer_on_output",
        });
        let too_many = serde_json::json!({
            "sequence": 1,
            "composited_route": "global_srgb_fallback",
            "blocked": false,
            "reason": null,
            "scene_linear_active": true,
            "linear_tail_safe": false,
            "linear_tail_blockers": ["cursor"],
            "external_elements": vec![entry; MAX_EXTERNAL_ELEMENT_CLASSES + 1],
        });
        assert!(serde_json::from_value::<ColorDeliveryPolicyDecisionStatus>(too_many).is_err());
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ColorDeliveryOutputStatus {
    pub output_name: String,
    /// Whether the output currently participates in presentation (not DPMS
    /// off or soft-disabled). Aggregate diagnostics ignore inactive outputs;
    /// the udev backend also invalidates their success record across a
    /// participation epoch so re-enable starts unknown until its first vblank.
    pub participating: bool,
    pub last_success: Option<ColorDeliveryPresentationStatus>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ColorDeliveryPresentationStatus {
    pub generation: u64,
    /// Policy decision under which this framebuffer was queued. Consumers can
    /// use the observed sequence set to distinguish a stable cohort from a
    /// multi-output transition; an older sequence may still be the latest
    /// framebuffer physically visible on that output.
    pub policy_sequence: u64,
    pub route: String,
    pub working_space: String,
    pub target_transfer_function: String,
    pub target_primaries: String,
    pub hdr_metadata_active: bool,
    pub colorspace_signal: String,
    pub fallback_reason: Option<String>,
    pub presented_at_monotonic_ms: Option<u64>,
    pub presented_ago_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
pub struct ScreenInfo {
    pub width: i32,
    pub height: i32,
}

/// Global output rectangle targeted by a non-locking system-UI surface.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SystemUiViewport {
    /// Global compositor coordinates. Origins are signed because outputs may
    /// sit left of or above the primary output.
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl SystemUiViewport {
    #[must_use]
    pub fn new(x: i32, y: i32, width: i32, height: i32) -> Option<Self> {
        (width > 0 && height > 0).then_some(Self {
            x,
            y,
            width,
            height,
        })
    }

    #[must_use]
    pub fn fullscreen(width: i32, height: i32) -> Self {
        Self {
            x: 0,
            y: 0,
            width: width.max(1),
            height: height.max(1),
        }
    }

    #[must_use]
    pub fn rect(self) -> [f32; 4] {
        [
            self.x as f32,
            self.y as f32,
            self.width as f32,
            self.height as f32,
        ]
    }
}

/// Backend-neutral compositor UI drawn above every client. Input and policy
/// live in JWM; backends only present this snapshot as a styled panel:
/// headline, optional search field, list rows (one optionally highlighted),
/// and a footer hint.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SystemUiOverlay {
    pub title: String,
    /// Search-field content; `Some` renders a query bar with a caret.
    pub query: Option<String>,
    pub items: Vec<String>,
    /// Row in `items` the renderer highlights with a selection pill.
    pub selected: Option<usize>,
    pub hint: String,
    /// Where `items` sits in a longer list, when the window manager is only
    /// sending a slice of one. `None` means everything there is to show is on
    /// screen.
    pub scroll: Option<ScrollWindow>,
    /// A lock overlay is opaque; other system UI dims the current desktop.
    pub locked: bool,
    /// Global rectangle of the output this non-locking panel belongs to.
    /// Invalid/default rectangles safely fall back to the compositor's full
    /// virtual screen. Locked overlays always ignore this field and use the
    /// full screen, preserving the lock's security boundary.
    pub viewport: SystemUiViewport,
    /// Set by the layout picker, which is drawn as a film strip of layout
    /// thumbnails instead of the list card.
    pub filmstrip: Option<LayoutFilmstrip>,
    /// Set by the tags overview, which is drawn as a grid of per-tag
    /// wireframe cells instead of the list card.
    pub tags_grid: Option<TagsGrid>,
}

/// What is under the pointer on the compositor-drawn system-UI card.
///
/// The compositor owns the final animated geometry (including the docked
/// card's spring), while JWM owns what activating a row means.  Keeping this
/// tiny semantic boundary lets mouse input land on exactly what was painted
/// without moving any policy into a renderer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SystemUiHitTarget {
    /// No system-UI frame has been painted yet (or this backend has no
    /// compositor hit map). Input should be left untouched.
    #[default]
    Unavailable,
    /// Outside the card, on its modal scrim.
    Outside,
    /// Inside the card, but not on a selectable list row.
    Panel,
    /// A visible row in [`SystemUiOverlay::items`].
    Item(usize),
}

impl SystemUiOverlay {
    /// Viewport a renderer must use. Keeping the lock override here makes it
    /// impossible for either backend to accidentally weaken the lock screen
    /// by trusting a monitor-local rectangle.
    #[must_use]
    pub fn effective_viewport(&self, screen_width: i32, screen_height: i32) -> [f32; 4] {
        let fullscreen = SystemUiViewport::fullscreen(screen_width, screen_height);
        if self.locked || self.viewport.width <= 0 || self.viewport.height <= 0 {
            fullscreen.rect()
        } else {
            self.viewport.rect()
        }
    }
}

#[cfg(test)]
mod system_ui_viewport_tests {
    use super::{SystemUiOverlay, SystemUiViewport};

    #[test]
    fn a_monitor_viewport_preserves_its_signed_global_origin() {
        let overlay = SystemUiOverlay {
            viewport: SystemUiViewport::new(-1920, 180, 1920, 1080).unwrap(),
            ..SystemUiOverlay::default()
        };
        assert_eq!(
            overlay.effective_viewport(3840, 1440),
            [-1920.0, 180.0, 1920.0, 1080.0]
        );
    }

    #[test]
    fn a_lock_and_an_invalid_viewport_both_fall_back_to_the_full_screen() {
        let locked = SystemUiOverlay {
            locked: true,
            viewport: SystemUiViewport::new(-1920, 180, 1920, 1080).unwrap(),
            ..SystemUiOverlay::default()
        };
        assert_eq!(
            locked.effective_viewport(3840, 1440),
            [0.0, 0.0, 3840.0, 1440.0]
        );

        assert_eq!(
            SystemUiOverlay::default().effective_viewport(3840, 1440),
            [0.0, 0.0, 3840.0, 1440.0]
        );
    }
}

/// The slice of a longer list that [`SystemUiOverlay::items`] is showing.
///
/// The window manager decides how many rows fit and which ones to send, so
/// without this the compositor cannot tell a complete list from the top of a
/// long one — and the user cannot tell either.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScrollWindow {
    /// Index of the first row being shown.
    pub first: usize,
    /// How many rows are being shown.
    pub visible: usize,
    /// How many rows there are in total.
    pub total: usize,
}

/// The layout picker's contents: one film cell per layout.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LayoutFilmstrip {
    pub cells: Vec<LayoutFilmCell>,
    pub selected: usize,
    /// Fraction of the auto-confirm delay already elapsed, `0.0..=1.0`.
    pub countdown: f32,
}

/// One layout's thumbnail.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LayoutFilmCell {
    /// Window outlines in `0.0..=1.0` of the cell's exposed frame, back to
    /// front.
    pub windows: Vec<[f32; 4]>,
    /// Whether this layout leaves room for the status bar, drawn as a rule
    /// across the top of the thumbnail.
    pub shows_bar: bool,
}

/// The tags overview's contents: one grid cell per tag of the selected
/// monitor.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TagsGrid {
    pub cells: Vec<TagsGridCell>,
    /// Columns the window manager's keyboard walk uses; the renderer lays the
    /// same grid out, so the two can never disagree on the shape.
    pub cols: u32,
    pub selected: usize,
    /// The currently visible tag's cell upgraded to live content. `None`
    /// keeps every cell a wireframe.
    pub live: Option<LiveTagsCell>,
}

impl TagsGrid {
    /// The live payload a renderer applies to cell `index`, when the grid
    /// carries one for it: the on-screen tag's cell draws the windows' own
    /// textures instead of wireframes, while every other cell keeps its
    /// outlines — a parked window's texture only holds the stale image from
    /// before it was moved off-screen.
    #[must_use]
    pub fn live_for_cell(&self, index: usize) -> Option<&LiveTagsCell> {
        self.live.as_ref().filter(|live| live.cell == index)
    }
}

/// The currently visible tag's cell upgraded to live content: which cell,
/// and each window's id + normalized rect (same `0.0..=1.0` cell space as
/// the wireframes, back to front). Precedent for ids crossing this boundary:
/// expose mode.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LiveTagsCell {
    pub cell: usize,
    pub windows: Vec<(WindowId, [f32; 4])>,
}

#[cfg(test)]
mod tags_grid_tests {
    use super::{LiveTagsCell, TagsGrid, WindowId};

    #[test]
    fn live_for_cell_matches_only_the_live_cell() {
        let grid = TagsGrid {
            live: Some(LiveTagsCell {
                cell: 2,
                windows: vec![(WindowId::from_raw(7), [0.0, 0.0, 1.0, 1.0])],
            }),
            ..TagsGrid::default()
        };
        assert_eq!(grid.live_for_cell(2), grid.live.as_ref());
        assert_eq!(grid.live_for_cell(1), None);
        assert_eq!(grid.live_for_cell(3), None);
    }

    #[test]
    fn a_grid_without_live_content_is_wireframe_everywhere() {
        let grid = TagsGrid::default();
        assert_eq!(grid.live_for_cell(0), None);
    }
}

/// One tag's cell in the overview grid.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TagsGridCell {
    /// Zero-based tag index. The drawn label is `tag_index + 1`, matching the
    /// status bar.
    pub tag_index: usize,
    /// Window outlines in `0.0..=1.0` of the cell's exposed frame, back to
    /// front.
    pub windows: Vec<[f32; 4]>,
    /// The tag holds windows at all, minimized and swallowed ones included —
    /// which `windows` cannot say, because those draw no outline.
    pub occupied: bool,
    /// The tag holds at least one window demanding attention. Follows
    /// `occupied`: minimized windows count, sticky and all-tags windows mark
    /// nothing. Drawn as a dot in the cell's label band.
    pub urgent: bool,
    /// The tag is currently visible on the monitor.
    pub active: bool,
}

/// What the OSD card depicts. Unlike toasts the OSD is a single
/// replace-in-place card at the bottom center of the primary output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OsdKind {
    Volume,
    VolumeMuted,
    Brightness,
    /// Track label on media-key presses and track changes; drawn without a
    /// progress bar, on a wider card.
    Media,
}

/// One keyboard navigation step in Expose / Mission Control, in the grid the
/// compositor laid out. Left/Right walk the row, Up/Down walk the column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExposeNavDirection {
    Left,
    Right,
    Up,
    Down,
}

/// One action button a notification sender offered. The type lives here
/// because both the toast card and the notification center draw it; the
/// center re-exports it from `jwm::features::notifications`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationAction {
    /// What goes back out over `ActionInvoked`; meaningful only to the sender.
    pub key: String,
    /// What the button draws.
    pub label: String,
}

/// One transient notification card the compositor stacks in the top-right
/// corner. Pushed fire-and-forget; the compositor owns display and expiry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToastNotification {
    pub title: String,
    /// Optional multi-line body under the title.
    pub body: String,
    /// 0 = low (dim accent), 1 = normal, 2 = critical (danger accent).
    pub urgency: u8,
    /// Display time in milliseconds; 0 selects the default.
    pub timeout_ms: u32,
    /// Action buttons drawn as a chip row at the bottom of the card. The
    /// toast stack caps and cleans the list before anyone sees it.
    pub actions: Vec<NotificationAction>,
    /// Notification-center record this toast mirrors; a button click reports
    /// it so the WM can invoke the action against the record. Zero marks a
    /// standalone toast with no record — its buttons only dismiss the card.
    pub notification_id: u32,
}

/// Outcome of a left-click hit-test against the toast stack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToastClick {
    /// No card under the point; the click belongs to whatever is below.
    Miss,
    /// A card was hit and dismissed; the WM swallows the click.
    Dismissed,
    /// An action button was hit. The card is dismissed either way; the WM
    /// additionally invokes the action against the notification-center
    /// record and swallows the click.
    Action {
        /// Record the action belongs to (`ToastNotification::notification_id`).
        notification_id: u32,
        /// The action's sender-defined key, ready for `ActionInvoked`.
        action_key: String,
    },
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Capabilities {
    pub can_warp_pointer: bool,
    pub supports_client_list: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetWmState {
    Fullscreen,
    MaximizedVert,
    MaximizedHorz,
    Hidden,
    Above,
    Below,
    DemandsAttention,
    Sticky,
    SkipTaskbar,
    SkipPager,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeEdge {
    Top,
    Bottom,
    Left,
    Right,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// A backend-owned interactive window operation.
///
/// X11 transports use this while tracking an active pointer grab. Keeping the
/// type in the platform contract prevents transports from depending on JWM
/// policy modules.
#[derive(Debug, Clone, Copy)]
pub enum InteractionAction {
    Move,
    Resize(ResizeEdge),
    /// Grab the pointer and report motion/release without touching any
    /// window. The WM uses this to watch a drag until it crosses the
    /// drag threshold (or, for tiled reorder drags, for their whole life).
    Track,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetWmAction {
    Add,
    Remove,
    Toggle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackMode {
    Above,
    Below,
    TopIf,
    BottomIf,
    Opposite,
}

#[derive(Debug, Clone, Default)]
pub struct WindowChanges {
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub border_width: Option<u32>,
    pub sibling: Option<WindowId>,
    pub stack_mode: Option<StackMode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowType {
    Normal,
    Desktop,
    Dock,
    Toolbar,
    Menu,
    Utility,
    Splash,
    Dialog,
    DropdownMenu,
    PopupMenu,
    Tooltip,
    Notification,
    Combo,
    Dnd,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyKind {
    Title,
    Class,
    TransientFor,
    SizeHints,
    Urgency,
    WindowType,
    Protocols,
    Strut,
    MotifHints,
    GtkFrameExtents,
    BypassCompositor,
    /// JWM-private root marker held by an out-of-process remote capture host.
    RemoteCapture,
    OpaqueRegion,
    NetWmIcon,
    UserTime,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyMode {
    Normal,
    Grab,
    Ungrab,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseResult {
    Graceful,
    Forced,
}

#[derive(Debug, Clone)]
pub struct WindowAttributes {
    pub override_redirect: bool,
    pub map_state_viewable: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Geometry {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub border: u32,
}

/// Backend-native identity that remains meaningful across one exec handoff.
///
/// [`WindowId`] is deliberately local to one backend instance and may be
/// allocated in discovery order.  Persisting `WindowId::raw()` across exec can
/// therefore bind state to a different client when a fresh backend scans the
/// same windows in another order.  X11's server-owned XID survives reconnecting
/// the window manager and is the only identity currently supported here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WindowHandoffIdentity {
    X11(u32),
}

/// ICCCM `WM_STATE` values shared by policy and the concrete X11 decoders.
///
/// Keeping these at the backend contract boundary prevents the policy layer
/// from importing an X11 implementation module merely to verify a protocol
/// number. Non-X11 property implementations accept the values as no-ops.
pub const ICCCM_WITHDRAWN_STATE: u8 = 0;
pub const ICCCM_NORMAL_STATE: u8 = 1;
pub const ICCCM_ICONIC_STATE: u8 = 3;

/// Largest persisted Dock insertion order accepted from an X11 client
/// property.  The upper half of `u64` remains allocator headroom, preventing
/// a forged snapshot from driving the process-local counter straight into a
/// wraparound.
pub const MAX_MINIMIZED_RESTORE_ORDER: u64 = i64::MAX as u64;

/// A semantic client rectangle persisted while an X11 window is minimized.
///
/// Unlike [`Geometry`], this contains no live X11 border value: it describes
/// JWM's client-area restore target, whose dimensions must be positive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MinimizedRestoreRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// JWM-owned state needed to adopt a minimized X11 client across an exec
/// restart without deriving restore coordinates from its off-screen parking
/// position.
///
/// The X11 transport has a versioned wire encoding for this semantic value;
/// non-X11 backends may leave the corresponding [`PropertyOps`] hooks as
/// no-ops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MinimizedRestoreState {
    pub tags: u32,
    pub monitor_num: i32,
    pub visible_rect: MinimizedRestoreRect,
    pub is_floating: bool,
    pub is_drag_floating: bool,
    pub floating_rect: Option<MinimizedRestoreRect>,
    pub is_pip: bool,
    /// Sticky state to reinstate when the active PiP mode exits.
    /// Meaningful only while `is_pip` is true.
    pub pip_restore_sticky: bool,
    /// Floating state to reinstate when leaving the active fullscreen/PiP
    /// mode. Fullscreen and PiP are mutually exclusive, so this single slot
    /// always belongs to exactly one active mode.
    pub old_state: bool,
    /// Pre-fullscreen geometry, when JWM has a meaningful fullscreen restore
    /// target. This deliberately does not expose the overloaded live
    /// `ClientGeometry::old_*` fields as a persistence contract.
    pub fullscreen_restore_rect: Option<MinimizedRestoreRect>,
    /// Stable Dock insertion order. A minimized snapshot always carries a
    /// non-zero value.
    pub minimized_order: u64,
}

/// A single output's requested configuration, produced by the
/// wlr-output-management protocol and applied by the backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputConfigChange {
    pub name: String,
    pub enabled: bool,
    /// Requested mode as `(width, height, refresh_mhz)`; `None` keeps the current mode.
    pub mode: Option<(i32, i32, i32)>,
    pub position: Option<(i32, i32)>,
    /// wl_output transform numeric value (0..=7); `None` keeps the current transform.
    pub transform: Option<i32>,
    pub scale: Option<f64>,
    pub adaptive_sync: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutputManagementFailure {
    pub output_name: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drm_property: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_value: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutputManagementOutputSnapshot {
    pub name: String,
    pub stable_key: String,
    pub enabled: bool,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub scale: f32,
    pub refresh_rate: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutputManagementTransactionStatus {
    pub id: u64,
    pub requested_at_unix_ms: u64,
    pub success: bool,
    pub changes: Vec<OutputConfigChange>,
    pub outputs_before: Vec<OutputManagementOutputSnapshot>,
    pub outputs_after: Vec<OutputManagementOutputSnapshot>,
    pub failed_outputs: Vec<OutputManagementFailure>,
    pub rollback_attempted: bool,
    pub rollback_succeeded: bool,
    pub rollback_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutputManagementRejectedConfig {
    pub attempted_at_unix_ms: u64,
    pub serial: u32,
    pub action: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drm_property: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_value: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutputManagementStatus {
    pub pending_ack_count: usize,
    pub soft_disabled_outputs: Vec<String>,
    pub last_transaction: Option<OutputManagementTransactionStatus>,
    pub last_rejected: Option<OutputManagementRejectedConfig>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CaptureProtocolStatus {
    pub enabled: bool,
    pub pending_frames: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CaptureStatus {
    pub screencopy: CaptureProtocolStatus,
    pub image_copy_capture: CaptureProtocolStatus,
    pub image_copy_output_pending_frames: usize,
    pub image_copy_toplevel_pending_frames: usize,
    pub screencopy_queued_total: u64,
    pub screencopy_failed_total: u64,
    pub screencopy_fulfilled_total: u64,
    pub screencopy_render_failed_total: u64,
    pub image_copy_sessions_total: u64,
    pub image_copy_queued_total: u64,
    pub image_copy_failed_total: u64,
    pub image_copy_fulfilled_total: u64,
    pub image_copy_render_failed_total: u64,
    pub image_copy_output_queued_total: u64,
    pub image_copy_toplevel_queued_total: u64,
    pub last_queued_unix_ms: Option<u64>,
    pub last_fulfilled_unix_ms: Option<u64>,
    pub last_failed_unix_ms: Option<u64>,
    pub last_failure_reason: Option<String>,
    pub dmabuf_advertised: bool,
    pub dmabuf_format_count: usize,
    pub cursor_capture_supported: bool,
    pub sensitive_content_masking: bool,
    pub policy: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct XWaylandStatus {
    pub available: bool,
    pub wm_ready: bool,
    pub display: Option<String>,
    pub mapped_window_count: usize,
    pub associated_surface_count: usize,
    pub pending_association_count: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProtocolBindStatus {
    pub protocol: String,
    pub bind_count: u64,
    pub last_bound_unix_ms: Option<u64>,
}

// --- 事件定义 ---

/// Why JWM deliberately unmapped an X11 client that remains managed.
///
/// This is intentionally a reason rather than a boolean: each lifecycle owns
/// different compositor resources. Future true-Iconic minimization can retain
/// a named pixmap without being confused with swallowing's silent discard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ManagedUnmapReason {
    /// A terminal is hidden while its graphical child replaces it. The live
    /// compositor texture is released immediately, without close effects.
    SwallowDiscard,
    /// A minimized X11 client entered true ICCCM IconicState after the
    /// compositor retained its visual. The compositor and desired-state
    /// replay continue to own that visual while the client stays managed.
    /// `generation` fences rapid restore/re-minimize cycles: an acknowledgement
    /// for an older `UnmapWindow` cannot commit the newer incarnation.
    IconifyRetain { generation: u64 },
}

#[derive(Debug, Clone)]
pub enum BackendEvent {
    // === 硬件与输出 ===
    OutputAdded(OutputInfo),
    OutputRemoved(OutputId),
    OutputChanged(OutputInfo),
    /// Apply a client-requested output configuration (wlr-output-management).
    OutputConfigure {
        changes: Vec<OutputConfigChange>,
    },
    ScreenLayoutChanged,
    ChildProcessExited,
    ConfigChanged,

    // === 窗口生命周期 ===
    WindowCreated(WindowId),
    WindowDestroyed(WindowId),
    WindowMapped(WindowId),
    WindowUnmapped {
        window: WindowId,
        /// True only for the core X11 `UnmapGravity` transition caused by a
        /// parent configure. It is not a client withdrawal.
        from_configure: bool,
    },
    /// First normalized `UnmapNotify` for an `UnmapWindow` request issued by
    /// JWM. The client remains managed; duplicate root/client deliveries are
    /// consumed inside the X11 transport before this event is produced.
    WindowManagerUnmapped {
        window: WindowId,
        reason: ManagedUnmapReason,
    },
    WindowConfigured {
        window: WindowId,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        border_width: u32,
    },

    ButtonPress {
        target: HitTarget,
        state: u16,
        detail: u8,
        time: u32,
        root_x: f64,
        root_y: f64,
    },
    ButtonRelease {
        target: HitTarget,
        time: u32,
    },
    MotionNotify {
        target: HitTarget,
        root_x: f64,
        root_y: f64,
        time: u32,
    },
    KeyPress {
        keycode: u8,
        state: u16,
        time: u32,
    },
    KeyRelease {
        keycode: u8,
        state: u16,
        time: u32,
    },

    // === 焦点与状态 ===
    EnterNotify {
        window: WindowId,
        subwindow: Option<WindowId>,
        mode: NotifyMode,
        root_x: f64,
        root_y: f64,
    },
    LeaveNotify {
        window: WindowId,
        mode: NotifyMode,
    },
    FocusIn {
        window: WindowId,
    },
    FocusOut {
        window: WindowId,
    },

    // === 客户端请求 (Policy) ===
    ConfigureRequest {
        window: WindowId,
        changes: WindowChanges,
        mask_bits: u16,
    },
    WindowStateRequest {
        window: WindowId,
        action: NetWmAction,
        state: NetWmState,
    },
    PropertyChanged {
        window: WindowId,
        kind: PropertyKind,
    },
    WmKeyboardShortcut {
        keysym: KeySym,
        mods: Mods,
    },
    Expose {
        window: WindowId,
    },
    ActiveWindowMessage {
        window: WindowId,
    },
    /// A pager/taskbar requested graceful close of a window (_NET_CLOSE_WINDOW).
    CloseWindowRequest {
        window: WindowId,
    },
    PingResponse {
        window: WindowId,
    },
    ShapeChanged {
        window: WindowId,
        shaped: bool,
    },
    ClientMessage {
        window: WindowId,
        type_: u32,
        data: [u32; 5],
        format: u8,
    },
    MoveResizeRequest {
        window: WindowId,
        direction: u32,
        button: u32,
    },
    MappingNotify,
    DamageNotify {
        drawable: WindowId,
    },

    // === Touchpad gesture events (Wayland only) ===
    /// A configured 3+ finger swipe gesture has completed and was intercepted
    /// by the compositor (not forwarded to clients).
    GestureSwipeAction {
        fingers: u32,
        /// One of: "left", "right", "up", "down".
        direction: &'static str,
    },

    // === Workspace protocol events ===
    WorkspaceActivate {
        monitor: usize,
        tag_mask: u32,
    },

    // === Output power (DPMS) ===
    OutputPowerSet {
        output_name: String,
        on: bool,
    },

    // === Gamma LUT (night light) ===
    GammaSet {
        output_name: String,
        gamma_size: u32,
        ramp: Vec<u16>,
    },

    // === Foreign toplevel management (taskbar window control) ===
    ForeignToplevelActivate(WindowId),
    ForeignToplevelClose(WindowId),
    ForeignToplevelSetMaximized(WindowId, bool),
    ForeignToplevelSetMinimized(WindowId, bool),
    ForeignToplevelSetFullscreen(WindowId, bool),

    // === Present extension events ===
    PresentComplete {
        window: WindowId,
        serial: u32,
        msc: u64,
        ust: u64,
    },
    PresentIdle {
        window: WindowId,
        serial: u32,
        pixmap: u32,
    },
}

pub trait WindowOps: Send {
    fn set_position(&self, win: WindowId, x: i32, y: i32) -> Result<(), BackendError>;
    fn configure(
        &self,
        win: WindowId,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        border: u32,
    ) -> Result<(), BackendError>;
    fn set_decoration_style(
        &self,
        win: WindowId,
        border_width: u32,
        border_color: Pixel,
    ) -> Result<(), BackendError>;
    fn raise_window(&self, win: WindowId) -> Result<(), BackendError>;
    fn map_window(&self, win: WindowId) -> Result<(), BackendError>;
    fn unmap_window(&self, win: WindowId) -> Result<(), BackendError>;
    /// Unmap a client while retaining WM ownership of it.
    ///
    /// X11 implementations correlate the returned request sequence with raw
    /// `UnmapNotify`; other backends keep the ordinary unmap fallback.
    fn unmap_managed_window(
        &self,
        win: WindowId,
        _reason: ManagedUnmapReason,
    ) -> Result<(), BackendError> {
        self.unmap_window(win)
    }
    fn close_window(&self, win: WindowId) -> Result<CloseResult, BackendError>;
    fn set_input_focus(&self, win: WindowId) -> Result<(), BackendError>;
    fn set_input_focus_root(&self) -> Result<(), BackendError>;
    fn get_window_attributes(&self, win: WindowId) -> Result<WindowAttributes, BackendError>;
    fn get_geometry(&self, win: WindowId) -> Result<Geometry, BackendError>;
    fn scan_windows(&self) -> Result<Vec<WindowId>, BackendError>;

    fn flush(&self) -> Result<(), BackendError>;

    fn kill_client(&self, win: WindowId) -> Result<(), BackendError>;

    fn apply_window_changes(
        &self,
        win: WindowId,
        changes: WindowChanges,
    ) -> Result<(), BackendError>;

    fn ungrab_all_buttons(&self, _win: WindowId) -> Result<(), BackendError> {
        Ok(())
    }
    fn grab_button_any_anymod(&self, _win: WindowId, _mask: u32) -> Result<(), BackendError> {
        Ok(())
    }
    fn grab_button(
        &self,
        _win: WindowId,
        _btn: u8,
        _mask: u32,
        _mods: Mods,
    ) -> Result<(), BackendError> {
        Ok(())
    }

    fn change_event_mask(&self, _win: WindowId, _mask: u32) -> Result<(), BackendError> {
        Ok(())
    }
    fn get_tree_child(&self, _win: WindowId) -> Result<Vec<WindowId>, BackendError> {
        Ok(vec![])
    }
    /// Send WM_TAKE_FOCUS client message if the window supports it.
    /// Returns true if the message was sent.
    fn send_take_focus(&self, _win: WindowId) -> Result<bool, BackendError> {
        Ok(false)
    }

    /// Restack windows in order (first = bottom, last = top).
    /// Uses sibling stacking for fewer X11 round-trips.
    /// Default implementation falls back to sequential raise_window.
    fn restack_windows(&self, windows: &[WindowId]) -> Result<(), BackendError> {
        for &win in windows {
            self.raise_window(win)?;
        }
        Ok(())
    }

    fn shape_select_input(&self, _win: WindowId) -> Result<(), BackendError> {
        Ok(())
    }

    fn get_window_shaped(&self, _win: WindowId) -> bool {
        false
    }
}

pub trait InputOps: Send {
    fn set_cursor(&self, kind: StdCursorKind) -> Result<(), BackendError>;

    fn get_pointer_position(&self) -> Result<(f64, f64), BackendError>;

    /// Return the top-level window directly under the pointer when the backend
    /// can query it independently of the currently grabbed event target.
    ///
    /// X11 active grabs report the grab window as the event target, so modal
    /// capture source selection uses this hook to recover the actual child.
    fn window_under_pointer(&self) -> Result<Option<WindowId>, BackendError> {
        Ok(None)
    }

    fn grab_pointer(&self, mask: u32, cursor: Option<u64>) -> Result<bool, BackendError>;

    fn ungrab_pointer(&self) -> Result<(), BackendError>;

    fn warp_pointer(&self, _x: f64, _y: f64) -> Result<(), BackendError> {
        Ok(())
    }

    fn query_pointer_root(&self) -> Result<(i32, i32, u16, u16), BackendError>;
    fn warp_pointer_to_window(&self, _win: WindowId, _x: i16, _y: i16) -> Result<(), BackendError> {
        Ok(())
    }
    fn allow_events(
        &self,
        _mode: crate::backend::api::AllowMode,
        _time: u32,
    ) -> Result<(), BackendError> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LayerSurfaceInfo {
    /// wlr-layer-shell exclusive zone semantics.
    /// - `0`: does not reserve space
    /// - `-1`: reserve the full surface dimension along the anchored edge
    /// - `>0`: reserve that many logical pixels
    pub exclusive_zone: i32,
    pub anchor_top: bool,
    pub anchor_bottom: bool,
    pub anchor_left: bool,
    pub anchor_right: bool,
}

pub trait PropertyOps: Send {
    fn get_title(&self, win: WindowId) -> String;
    fn get_class(&self, win: WindowId) -> (String, String); // (instance, class)
    fn get_window_types(&self, win: WindowId) -> Vec<WindowType>;

    fn is_fullscreen(&self, win: WindowId) -> bool;
    fn set_fullscreen_state(&self, win: WindowId, on: bool) -> Result<(), BackendError>;

    fn transient_for(&self, win: WindowId) -> Option<WindowId>;

    // Hints
    fn get_wm_hints(&self, win: WindowId) -> Option<crate::backend::api::WmHints>;
    fn set_urgent_hint(&self, win: WindowId, urgent: bool) -> Result<(), BackendError>;
    fn fetch_normal_hints(
        &self,
        win: WindowId,
    ) -> Result<Option<crate::backend::api::NormalHints>, BackendError>;

    fn set_window_strut_top(
        &self,
        win: WindowId,
        top: u32,
        start_x: u32,
        end_x: u32,
    ) -> Result<(), BackendError>;
    fn set_window_type_dock(&self, win: WindowId) -> Result<(), BackendError>;
    fn clear_window_strut(&self, win: WindowId) -> Result<(), BackendError>;

    fn get_wm_state(&self, win: WindowId) -> Result<i64, BackendError>;
    fn set_wm_state(&self, win: WindowId, state: i64) -> Result<(), BackendError>;

    /// Read JWM's private, versioned minimized-client restart snapshot.
    /// Missing or malformed properties are reported as `Ok(None)` so an
    /// untrusted client property cannot prevent window adoption.
    fn get_minimized_restore_state(
        &self,
        _win: WindowId,
    ) -> Result<Option<MinimizedRestoreState>, BackendError> {
        Ok(None)
    }

    /// Replace JWM's private minimized-client restart snapshot.
    fn set_minimized_restore_state(
        &self,
        _win: WindowId,
        _state: MinimizedRestoreState,
    ) -> Result<(), BackendError> {
        Ok(())
    }

    /// Remove JWM's private minimized-client restart snapshot. This operation
    /// is idempotent when no snapshot exists.
    fn clear_minimized_restore_state(&self, _win: WindowId) -> Result<(), BackendError> {
        Ok(())
    }

    fn set_client_info_props(
        &self,
        win: WindowId,
        tags: u32,
        monitor_num: u32,
    ) -> Result<(), BackendError>;

    fn get_window_strut_partial(&self, _win: WindowId) -> Option<StrutPartial> {
        None
    }

    fn get_layer_surface_info(&self, _win: WindowId) -> Option<LayerSurfaceInfo> {
        None
    }

    /// Get the PID of the process that owns this window
    fn get_window_pid(&self, _win: WindowId) -> Option<u32> {
        None
    }

    // --- Phase 1: EWMH compliance ---

    fn set_net_wm_state_flag(
        &self,
        _win: WindowId,
        _state: NetWmState,
        _on: bool,
    ) -> Result<(), BackendError> {
        Ok(())
    }

    /// Query one EWMH state atom when the backend exposes `_NET_WM_STATE`.
    /// Non-X11 backends may keep the default because JWM already owns their
    /// live state; this read is primarily an adoption/migration seam.
    fn has_net_wm_state_flag(
        &self,
        _win: WindowId,
        _state: NetWmState,
    ) -> Result<bool, BackendError> {
        Ok(false)
    }

    fn set_frame_extents(
        &self,
        _win: WindowId,
        _left: u32,
        _right: u32,
        _top: u32,
        _bottom: u32,
    ) -> Result<(), BackendError> {
        Ok(())
    }

    fn set_allowed_actions(
        &self,
        _win: WindowId,
        _actions: &[AllowedAction],
    ) -> Result<(), BackendError> {
        Ok(())
    }

    fn send_ping(&self, _win: WindowId, _timestamp: u32) -> Result<bool, BackendError> {
        Ok(false)
    }

    fn get_user_time(&self, _win: WindowId) -> Option<u32> {
        None
    }

    fn get_net_wm_icon(&self, _win: WindowId) -> Option<Vec<IconData>> {
        None
    }

    fn get_bypass_compositor(&self, _win: WindowId) -> Option<u32> {
        None
    }

    fn get_opaque_region(&self, _win: WindowId) -> Option<Vec<(i32, i32, u32, u32)>> {
        None
    }

    fn get_motif_hints(&self, _win: WindowId) -> Option<MotifWmHints> {
        None
    }

    fn get_gtk_frame_extents(&self, _win: WindowId) -> Option<[u32; 4]> {
        None
    }

    fn get_sync_counter(&self, _win: WindowId) -> Option<u32> {
        None
    }

    fn send_sync_request(
        &self,
        _win: WindowId,
        _counter: u32,
        _value: u64,
    ) -> Result<(), BackendError> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StrutPartial {
    pub left: u32,
    pub right: u32,
    pub top: u32,
    pub bottom: u32,
    pub left_start_y: u32,
    pub left_end_y: u32,
    pub right_start_y: u32,
    pub right_end_y: u32,
    pub top_start_x: u32,
    pub top_end_x: u32,
    pub bottom_start_x: u32,
    pub bottom_end_x: u32,
}

pub struct WmHints {
    pub urgent: bool,
    pub input: Option<bool>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NormalHints {
    pub base_w: i32,
    pub base_h: i32,
    pub inc_w: i32,
    pub inc_h: i32,
    pub max_w: i32,
    pub max_h: i32,
    pub min_w: i32,
    pub min_h: i32,
    pub min_aspect: f32,
    pub max_aspect: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowedAction {
    Move,
    Resize,
    Minimize,
    MaximizeHorz,
    MaximizeVert,
    Fullscreen,
    Close,
    Stick,
    Above,
    Below,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MotifWmHints {
    pub flags: u32,
    pub functions: u32,
    pub decorations: u32,
    pub input_mode: i32,
    pub status: u32,
}

impl MotifWmHints {
    pub fn has_decorations_hint(&self) -> bool {
        self.flags & 0x2 != 0
    }
    pub fn decorations_none(&self) -> bool {
        self.has_decorations_hint() && self.decorations == 0
    }
}

#[derive(Debug, Clone)]
pub struct IconData {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

pub trait OutputOps: Send {
    /// 获取当前所有连接的输出设备
    fn enumerate_outputs(&self) -> Vec<OutputInfo>;
    /// 获取主屏幕信息 (兼容旧接口)
    fn screen_info(&self) -> ScreenInfo;

    fn output_at(&self, x: i32, y: i32) -> Option<OutputId>;

    /// Invalidate cached output layout (no-op for backends that don't cache)
    fn invalidate_output_cache(&self) {}

    /// Set hardware gamma ramp for an output (XRandR CRTC gamma)
    fn set_gamma_ramp(
        &self,
        _output: OutputId,
        _red: &[u16],
        _green: &[u16],
        _blue: &[u16],
    ) -> Result<(), BackendError> {
        Ok(())
    }

    /// Get current gamma ramp for an output
    fn get_gamma_ramp(&self, _output: OutputId) -> Option<(Vec<u16>, Vec<u16>, Vec<u16>)> {
        None
    }
}

pub trait KeyOps: Send {
    // 注册全局快捷键
    fn grab_keys(&self, root: WindowId, bindings: &[(Mods, KeySym)]) -> Result<(), BackendError>;
    fn clear_key_grabs(&self, root: WindowId) -> Result<(), BackendError>;

    /// Grab the entire keyboard so all key events are delivered to the WM.
    /// Used for modal states like overview mode.
    fn grab_keyboard(&self, _root: WindowId) -> Result<(), BackendError> {
        Ok(())
    }
    /// Release the keyboard grab.
    fn ungrab_keyboard(&self) -> Result<(), BackendError> {
        Ok(())
    }

    // 辅助转换
    fn clean_mods(&self, raw_state: u16) -> Mods;
    fn keysym_from_keycode(&mut self, keycode: u8) -> Result<KeySym, BackendError>;
    fn clear_cache(&mut self);
}

pub trait EwmhFacade: Send {
    fn set_active_window(&self, win: WindowId) -> Result<(), BackendError>;
    fn clear_active_window(&self) -> Result<(), BackendError>;
    fn set_client_list(&self, list: &[WindowId]) -> Result<(), BackendError>;
    fn set_client_list_stacking(&self, list: &[WindowId]) -> Result<(), BackendError>;
    fn setup_supporting_wm_check(&self, wm_name: &str) -> Result<WindowId, BackendError>;
    fn declare_supported(&self, features: &[EwmhFeature]) -> Result<(), BackendError>;
    fn reset_root_properties(&self) -> Result<(), BackendError>;
    fn set_desktop_info(
        &self,
        current: u32,
        total: u32,
        names: &[&str],
    ) -> Result<(), BackendError>;
    fn set_workarea(&self, _areas: &[(i32, i32, u32, u32)]) -> Result<(), BackendError> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EwmhFeature {
    ActiveWindow,
    Supported,
    WmName,
    WmState,
    SupportingWmCheck,
    WmStateFullscreen,
    WmStateMaximizedVert,
    WmStateMaximizedHorz,
    WmStateHidden,
    WmStateAbove,
    WmStateBelow,
    WmStateDemandsAttention,
    WmStateSticky,
    WmStateSkipTaskbar,
    WmStateSkipPager,
    ClientList,
    ClientInfo,
    WmWindowType,
    WmWindowTypeDialog,
    CurrentDesktop,
    NumberOfDesktops,
    DesktopNames,
    DesktopViewport,
    WmMoveResize,
    FrameExtents,
    WmAllowedActions,
    Workarea,
    CloseWindow,
    RestackWindow,
    WmPing,
    WmUserTime,
    WmIcon,
    WmBypassCompositor,
    WmOpaqueRegion,
}

pub trait ColorAllocator: Send {
    fn set_scheme(&mut self, t: SchemeType, s: ColorScheme);
    fn allocate_schemes_pixels(&mut self) -> Result<(), BackendError>;
    fn get_border_pixel_of(&mut self, t: SchemeType) -> Result<Pixel, BackendError>;
    fn free_all_theme_pixels(&mut self) -> Result<(), BackendError>;
}

pub trait CursorProvider: Send {
    fn preload_common(&mut self) -> Result<(), BackendError>;
    fn get(&mut self, kind: StdCursorKind) -> Result<CursorHandle, BackendError>;
    fn apply(&mut self, window_id: WindowId, kind: StdCursorKind) -> Result<(), BackendError>;
    fn cleanup(&mut self) -> Result<(), BackendError>;

    /// Re-read the cursor theme/size from the live `[appearance]` config and
    /// rebuild any cached cursors. Returns `true` when the theme or size
    /// actually changed, so the caller can re-apply the pointer to the root
    /// window. Backends without themed cursors keep the no-op default.
    fn reload_theme(&mut self) -> Result<bool, BackendError> {
        Ok(false)
    }
}

/// Benchmark capability exposed by compositor-backed platforms.
///
/// Keeping this separate lets orchestration and IPC depend on a focused port
/// and allows non-compositing backends to use the no-op defaults.
pub trait CompositorBenchmark: Send {
    /// Start collecting `frames` samples after `warmup` frames.
    fn compositor_benchmark_start(&mut self, _frames: u32, _warmup: u32) -> bool {
        false
    }

    fn compositor_benchmark_stop(&mut self) -> Option<String> {
        None
    }

    fn compositor_benchmark_report(&self) -> Option<String> {
        None
    }

    fn compositor_benchmark_is_complete(&self) -> bool {
        false
    }

    fn compositor_benchmark_set_auto_exit(&mut self, _enabled: bool) {}
}

/// Read-only operational information exposed by a backend.
///
/// This focused interface starts with performance telemetry. Protocol and
/// output status snapshots can migrate here incrementally without growing the
/// control surface of `Backend` further.
pub trait BackendDiagnostics: Send {
    fn compositor_fps(&self) -> f32 {
        0.0
    }

    fn compositor_get_metrics(&self) -> Option<CompositorMetrics> {
        None
    }

    fn compositor_tearing_hint_count(&self) -> usize {
        0
    }

    fn compositor_session_lock_surface_count(&self) -> usize {
        0
    }

    fn compositor_session_locked(&self) -> bool {
        false
    }

    fn compositor_color_managed_surfaces(&self) -> Vec<ColorManagedSurfaceInfo> {
        Vec::new()
    }

    /// Whether the compositor is currently rendering through its
    /// scene-linear intermediate, after allocation/fallback handling.
    fn compositor_scene_linear_active(&self) -> bool {
        false
    }

    fn compositor_blur_status(&self) -> Option<BlurStatus> {
        None
    }

    fn compositor_direct_scanout_status(&self) -> Option<DirectScanoutStatus> {
        None
    }

    fn compositor_presentation_timing_status(&self) -> Option<PresentationTimingStatus> {
        None
    }

    fn compositor_color_delivery_status(&self) -> Option<ColorDeliveryStatus> {
        None
    }

    fn compositor_output_management_status(&self) -> Option<OutputManagementStatus> {
        None
    }

    fn compositor_capture_status(&self) -> Option<CaptureStatus> {
        None
    }

    fn compositor_xwayland_status(&self) -> Option<XWaylandStatus> {
        None
    }

    fn compositor_protocol_bind_counts(&self) -> Vec<ProtocolBindStatus> {
        Vec::new()
    }
}

/// Runtime controls for compositor-wide visual state.
pub trait CompositorControl: Send {
    fn compositor_set_color_temperature(&mut self, _temperature: f32) {}
    fn compositor_set_saturation(&mut self, _saturation: f32) {}
    fn compositor_set_brightness(&mut self, _brightness: f32) {}
    fn compositor_set_contrast(&mut self, _contrast: f32) {}
    fn compositor_set_invert_colors(&mut self, _invert: bool) {}
    fn compositor_set_grayscale(&mut self, _grayscale: bool) {}
    fn compositor_set_debug_hud(&mut self, _enabled: bool) {}
    fn compositor_set_debug_hud_extended(&mut self, _enabled: bool) {}

    fn compositor_toggle_waterlily_effect(&mut self) -> Option<bool> {
        None
    }

    /// Ask the connected WaterLily worker to switch its simulation case
    /// (`next` cycles). Returns None when the backend has no compositor and
    /// Some(delivered) otherwise.
    fn compositor_set_waterlily_case(&mut self, _case: &str) -> Option<bool> {
        None
    }

    /// Ask the connected WaterLily worker to switch its render palette
    /// (`next` cycles, `auto` restores the per-case default). Returns None
    /// when the backend has no compositor and Some(delivered) otherwise.
    fn compositor_set_waterlily_palette(&mut self, _palette: &str) -> Option<bool> {
        None
    }

    /// Snapshot the WaterLily layer state. Returns None when the backend has
    /// no compositor.
    fn compositor_waterlily_status(&self) -> Option<WaterlilyStatus> {
        None
    }

    fn compositor_set_transition_mode(&mut self, _mode: &str) {}
    fn compositor_apply_config(&mut self) {}
}

/// What a recording in progress is actually achieving, as opposed to what it
/// was asked for.
///
/// A recorder that quietly runs at a third of the requested rate, or that is
/// dropping frames because the encoder cannot keep up, looks exactly like a
/// healthy one until the file is played back. These are the numbers that tell
/// the difference, so they are reported while the recording is still running.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RecordingStats {
    /// The size actually being encoded, which differs from the captured region
    /// when `behavior.recording_max_height` scales it down.
    pub output_size: (u32, u32),
    /// Frames read back off the GPU since the recording started.
    pub captured: u64,
    /// Frames the encoder had no room for and the compositor discarded rather
    /// than block on. Any non-zero value means the encoder is behind.
    pub dropped: u64,
    /// Wall-clock seconds since the recording started.
    pub elapsed_secs: f64,
}

impl RecordingStats {
    /// Frames actually captured per second of wall clock. Well below the
    /// configured rate means the capture path, not the encoder, is the limit —
    /// including the deliberate case of a screen that simply is not changing.
    pub fn effective_fps(&self) -> f64 {
        if self.elapsed_secs <= 0.0 {
            return 0.0;
        }
        self.captured as f64 / self.elapsed_secs
    }
}

#[cfg(test)]
mod recording_stats_tests {
    use super::RecordingStats;

    #[test]
    fn the_effective_rate_is_what_was_captured_not_what_was_asked_for() {
        // A recording configured for 30 fps that only managed 11.
        let stats = RecordingStats {
            output_size: (1920, 1080),
            captured: 220,
            dropped: 0,
            elapsed_secs: 20.0,
        };
        assert!((stats.effective_fps() - 11.0).abs() < 1e-9);
    }

    #[test]
    fn a_recording_that_has_not_run_yet_reports_no_rate() {
        // Guards the division: stats are readable the instant recording starts.
        let stats = RecordingStats::default();
        assert_eq!(stats.effective_fps(), 0.0);
        assert_eq!(
            RecordingStats {
                output_size: (1920, 1080),
                captured: 5,
                dropped: 0,
                elapsed_secs: 0.0,
            }
            .effective_fps(),
            0.0
        );
    }
}

/// Capture, thumbnail, recording and media-timing operations.
pub trait CompositorMedia: Send {
    fn take_screenshot_to_file(&mut self, _path: &std::path::Path) -> Result<bool, BackendError> {
        Ok(false)
    }

    fn take_screenshot_region_to_file(
        &mut self,
        _path: &std::path::Path,
        _x: i32,
        _y: i32,
        _width: u32,
        _height: u32,
    ) -> Result<bool, BackendError> {
        Ok(false)
    }

    fn compositor_capture_thumbnail(
        &self,
        _window: WindowId,
        _max_size: u32,
    ) -> Option<(Vec<u8>, u32, u32)> {
        None
    }

    fn compositor_request_live_thumbnail(
        &mut self,
        _window: u32,
        _max_size: u32,
    ) -> Option<(Vec<u8>, u32, u32)> {
        None
    }

    fn compositor_start_recording(&mut self, _path: &str) {}
    fn compositor_start_recording_region(&mut self, path: &str, region: (i32, i32, u32, u32)) {
        self.compositor_set_recording_region(region);
        self.compositor_start_recording(path);
    }
    fn compositor_set_recording_region(&mut self, _region: (i32, i32, u32, u32)) {}
    fn compositor_set_recording_region_overlay(&mut self, _region: Option<(i32, i32, u32, u32)>) {}
    fn compositor_stop_recording(&mut self) {}
    /// How the recording in progress is actually going, or `None` when none is.
    fn compositor_recording_stats(&self) -> Option<RecordingStats> {
        None
    }

    fn compositor_notify_audio_timing(
        &mut self,
        _window: WindowId,
        _fps: f32,
        _buffer_latency_ms: u32,
    ) {
    }
}

/// Workspace transition and interactive preview effects.
pub trait CompositorWorkspaceEffects: Send {
    fn compositor_set_system_ui(&mut self, _overlay: Option<SystemUiOverlay>) {}

    /// Show a quiet hover cue on an interactive visible row. `None` clears it;
    /// the keyboard selection remains independently visible.
    fn compositor_set_system_ui_hover(&mut self, _row: Option<usize>) {}

    /// Hit-test the last system-UI frame at global screen coordinates.
    fn compositor_system_ui_hit_test(&self, _x: f64, _y: f64) -> SystemUiHitTarget {
        SystemUiHitTarget::Unavailable
    }

    fn compositor_push_toast(&mut self, _toast: ToastNotification) {}

    /// Show (or refresh in place) the volume/brightness OSD card.
    fn compositor_show_osd(&mut self, _kind: OsdKind, _percent: u8) {}
    /// Show the media OSD card with a track label instead of a value bar.
    fn compositor_show_media_osd(&mut self, _label: &str) {}
    /// Left-click hit-test against the toast cards drawn last frame. A card
    /// hit dismisses the card; a button hit additionally reports the action
    /// so the WM can invoke it. Anything but `ToastClick::Miss` is swallowed.
    fn compositor_click_toast(&mut self, _x: f32, _y: f32) -> ToastClick {
        ToastClick::Miss
    }
    fn compositor_notify_tag_switch(
        &mut self,
        _duration: std::time::Duration,
        _direction: i32,
        _exclude_top: u32,
        _monitor_rect: (i32, i32, u32, u32),
    ) {
    }

    fn compositor_set_magnifier(&mut self, _enabled: bool) {}
    fn compositor_set_snap_preview(&mut self, _preview: Option<(f32, f32, f32, f32)>) {}
    fn compositor_clear_snap_preview_immediate(&mut self) {}
    /// Enable or disable freezing the current compositor scene while an
    /// interactive screenshot is being selected or edited. Backends without
    /// a texture overlay keep a live scene.
    fn compositor_set_screenshot_freeze(&mut self, _active: bool) {}

    fn compositor_set_overview_mode(
        &mut self,
        _active: bool,
        _windows: &[(WindowId, f32, f32, f32, f32, bool, String)],
    ) {
    }

    fn compositor_set_overview_monitor(&mut self, _x: i32, _y: i32, _width: u32, _height: u32) {}
    fn compositor_set_monitors(&mut self, _monitors: &[(u32, i32, i32, u32, u32, u32)]) {}
    fn compositor_set_overview_selection(&mut self, _window: WindowId) {}

    fn compositor_set_expose_mode(
        &mut self,
        _active: bool,
        _windows: Vec<(WindowId, i32, i32, u32, u32)>,
    ) {
    }

    fn compositor_expose_click(&mut self, _x: f32, _y: f32) -> Option<WindowId> {
        None
    }

    /// Move the expose highlight one grid step in `dir`; called on arrow keys.
    fn compositor_expose_move(&mut self, _dir: ExposeNavDirection) {}
    /// The window currently highlighted in expose, if any.
    fn compositor_expose_selected(&mut self) -> Option<WindowId> {
        None
    }
    /// Highlight a specific expose entry; `None` clears the highlight.
    fn compositor_expose_select(&mut self, _win: Option<WindowId>) {}
}

/// Per-window compositor visual state.
pub trait CompositorWindowEffects: Send {
    fn compositor_set_frame_extents(
        &mut self,
        _window: WindowId,
        _left: u32,
        _right: u32,
        _top: u32,
        _bottom: u32,
    ) {
    }

    fn compositor_set_window_shaped(&mut self, _window: WindowId, _shaped: bool) {}
    fn compositor_set_window_urgent(&mut self, _window: WindowId, _urgent: bool) {}
    fn compositor_set_window_pip(&mut self, _window: WindowId, _pip: bool) {}
    fn compositor_force_full_redraw(&mut self) {}
    fn compositor_set_mouse_position(&mut self, _x: f32, _y: f32) {}
    fn compositor_deactivate_edge_glow(&mut self) {}
    fn compositor_unsuppress_edge_glow(&mut self) {}
    fn compositor_notify_window_move_start(&mut self, _window: WindowId) {}
    fn compositor_notify_window_move_delta(&mut self, _window: WindowId, _dx: f32, _dy: f32) {}
    fn compositor_notify_window_move_end(&mut self, _window: WindowId) {}
    /// Request true X11 ICCCM iconification after the compositor has admitted
    /// a restart-recoverable retained snapshot. Backends without this
    /// lifecycle keep the existing semantic minimize behavior.
    fn compositor_request_window_iconify(&mut self, _window: WindowId) -> Result<(), BackendError> {
        Ok(())
    }
    /// Cancel a pending or physical true-Iconic transition. X11 maps and then
    /// confirms a sent/acknowledged client is viewable before dropping
    /// coordinator ownership; the pinned snapshot remains until live
    /// compositor import succeeds.
    fn compositor_cancel_window_iconify(&mut self, _window: WindowId) -> Result<(), BackendError> {
        Ok(())
    }
    fn compositor_set_window_minimized(&mut self, _window: WindowId, _minimized: bool) {}
    /// Forget every compositor-owned resource and replay record for a window
    /// that remains hidden in JWM but is no longer eligible for the Dock.
    ///
    /// This is resource retirement, not a restore request: implementations
    /// must not play a reverse Genie transition or make the client visible.
    /// Resource-free window metadata may be retained so a later eligibility
    /// re-entry preserves PiP/class/rule state.
    fn compositor_forget_minimized_window_visual(&mut self, _window: WindowId) {}
    /// Reconcile a window that is already hidden in WM state with the
    /// compositor's minimized lifecycle without replaying the minimize
    /// animation. Implementations may defer the static texture import until a
    /// valid Dock geometry makes the window addressable.
    fn compositor_ensure_minimized_window_visual(&mut self, _window: WindowId) {}
    /// Set or withdraw one window's Dock slot in global physical pixels.
    fn compositor_set_window_dock_geometry(
        &mut self,
        _window: WindowId,
        _target: Option<CompositorRect>,
    ) {
    }
    /// Show, move, replace, or hide the compositor-owned minimized preview.
    /// `None` for either value hides it; repeated identical updates are cheap.
    fn compositor_set_minimized_window_preview(
        &mut self,
        _window: Option<WindowId>,
        _anchor: Option<CompositorRect>,
    ) {
    }
    fn compositor_set_dock_position(&mut self, _x: f32, _y: f32) {}
    fn compositor_set_peek_mode(&mut self, _active: bool) {}
    /// Hand the compositor the tab bars to paint. Each group carries the strip
    /// the window manager reserved for it, so the compositor never has to
    /// derive the position from a window's geometry — and never has to know
    /// which windows the cells stand for, since the window manager owns the
    /// hit test.
    fn compositor_set_window_groups(
        &mut self,
        _groups: Vec<crate::backend::compositor_common::window_tabs::TabGroup>,
    ) {
    }
    /// Zoom one window so it fits the screen, or reset with `None`. The
    /// window crosses this boundary as a [`WindowId`]; each backend
    /// translates it to its native id before touching the compositor.
    fn compositor_zoom_to_fit(&mut self, _window: Option<WindowId>) {}
}

/// Accessibility color correction and interactive screen annotations.
pub trait CompositorAnnotation: Send {
    fn compositor_set_colorblind_mode(&mut self, _mode: &str) {}
    fn compositor_set_annotation_mode(&mut self, _active: bool) {}
    fn compositor_set_annotation_color(&mut self, _rgba: [f32; 4]) {}
    fn compositor_set_annotation_line_width(&mut self, _width: f32) {}
    fn compositor_annotation_add_point(&mut self, _x: f32, _y: f32) {}
    fn compositor_annotation_begin_stroke(&mut self) {}

    /// Add a filled (optionally rounded) rectangle to the overlay.
    ///
    /// Strokes cannot express a redaction bar or a counter bubble without
    /// degenerating into hundreds of hatch segments rebuilt on every motion
    /// event, so those travel as shapes and are drawn with the same rounded
    /// rect program the compositor uses for the rest of its own UI. Cleared
    /// with the strokes by `compositor_set_annotation_mode(false)`.
    fn compositor_annotation_add_quad(
        &mut self,
        _quad: crate::backend::compositor_common::annotation_overlay::AnnotationQuad,
    ) {
    }

    /// Add a run of text to the overlay, rasterised by the compositor in the
    /// configured UI font — the same one the baked PNG uses, so what the
    /// selection shows is what the file gets.
    fn compositor_annotation_add_text(
        &mut self,
        _label: crate::backend::compositor_common::annotation_overlay::AnnotationLabel,
    ) {
    }

    /// Publish (or withdraw, with `None`) the screenshot editor's toolbar.
    ///
    /// The window manager owns the model and the hit test; the compositor only
    /// paints what it is handed, exactly as it does for window tab bars. No
    /// window id and no tool identity crosses this boundary — just rectangles,
    /// icons and flags.
    fn compositor_set_screenshot_toolbar(
        &mut self,
        _toolbar: Option<crate::backend::compositor_common::screenshot_toolbar::ScreenshotToolbar>,
    ) {
    }
}

/// Output hardware capabilities and runtime display controls.
pub trait DisplayControl: Send {
    fn query_vrr_capabilities(&self, _output: OutputId) -> Option<VrrCapabilities> {
        None
    }
    fn query_kms_color_pipeline_caps(&self, _output: OutputId) -> Option<KmsColorPipelineCaps> {
        None
    }
    fn set_vrr_enabled(&mut self, _output: OutputId, _enabled: bool) -> Result<(), BackendError> {
        Ok(())
    }
    fn set_hdr_metadata(&mut self, _output: OutputId, _enabled: bool) -> Result<(), BackendError> {
        Err(BackendError::Unsupported(
            "HDR metadata push not implemented",
        ))
    }
}

/// Lightweight compositor scheduling and state queries.
pub trait RenderScheduler: Send {
    fn request_render(&mut self) {}
    fn has_compositor(&self) -> bool {
        false
    }
    fn compositor_needs_render(&self) -> bool {
        false
    }
    /// How long an otherwise idle event loop may sleep before the compositor
    /// itself needs a frame. Screen recording is the case that matters: it must
    /// composite at the recording rate even on a completely static desktop, and
    /// without a deadline the loop would either block until some client happens
    /// to send an event or spin at 1 ms for the whole recording. `None` means
    /// the compositor has no self-imposed deadline.
    fn compositor_frame_deadline(&self) -> Option<std::time::Duration> {
        None
    }
    fn compositor_overlay_window(&self) -> Option<WindowId> {
        None
    }
}

pub trait EventHandler {
    fn handle_event(
        &mut self,
        backend: &mut dyn Backend,
        event: BackendEvent,
    ) -> Result<(), BackendError>;

    fn update(&mut self, backend: &mut dyn Backend) -> Result<(), BackendError>;

    fn should_exit(&self) -> bool;

    /// Returns true when the handler has active work or a deadline that is due
    /// and needs the event loop to tick now.
    fn needs_tick(&self) -> bool {
        false
    }

    /// Returns the maximum duration an event loop may sleep before calling
    /// [`EventHandler::update`] again. Event loops with their own periodic
    /// timers may ignore this; loops that otherwise block indefinitely should
    /// include it in their dispatch timeout. `Duration::ZERO` means the update
    /// is due now.
    fn next_wakeup(&self) -> Option<std::time::Duration> {
        None
    }

    /// Duplicate a stable level-triggered descriptor that becomes readable
    /// when external I/O requires [`EventHandler::update`]. The event loop owns
    /// the returned fd; the handler must clear the underlying readiness while
    /// updating. Backends without fd integration may ignore this hook and keep
    /// their normal maintenance fallback.
    fn duplicate_update_readiness_fd(&self) -> Option<std::os::fd::OwnedFd> {
        None
    }

    /// Notifier attached to every handler-owned worker queue that is polled
    /// from [`EventHandler::update`]. X11 backends also attach it to their
    /// clipboard watcher. Returning `Some` promises that its fd is aggregated
    /// into [`EventHandler::duplicate_update_readiness_fd`].
    fn async_update_notifier(
        &self,
    ) -> Option<crate::backend::update_notifier::AsyncUpdateNotifier> {
        None
    }

    /// Whether completion signalling is still usable. An unexpected eventfd
    /// write failure revokes the readiness promise so X11 immediately returns
    /// to its conservative idle polling policy.
    fn async_update_readiness_healthy(&self) -> bool {
        false
    }

    /// Immediately render the compositor if it has pending damage.
    /// Called from the event loop right after processing X events to
    /// minimise visual latency for rapidly-updating overlay windows
    /// (e.g. flameshot screenshot selection).  The default is a no-op.
    fn render_compositor_immediate(&mut self, _backend: &mut dyn Backend) {}
}

pub trait Backend:
    CompositorBenchmark
    + BackendDiagnostics
    + CompositorControl
    + CompositorMedia
    + CompositorWorkspaceEffects
    + CompositorWindowEffects
    + CompositorAnnotation
    + DisplayControl
    + RenderScheduler
{
    fn capabilities(&self) -> Capabilities;
    fn root_window(&self) -> Option<WindowId>;
    fn as_any(&self) -> &dyn Any;
    fn check_existing_wm(&self) -> Result<(), BackendError>;

    /// Export a backend-native identity for a managed window.
    ///
    /// The default is deliberately unavailable: a backend-local raw
    /// [`WindowId`] must never masquerade as a cross-exec identity.
    fn window_handoff_identity(&self, _window: WindowId) -> Option<WindowHandoffIdentity> {
        None
    }

    /// Resolve a handoff identity only if the fresh backend has already
    /// discovered and interned that native window.
    ///
    /// Implementations must not create/intern a mapping from untrusted handoff
    /// data. The caller performs a second managed-client and PID check after
    /// this lookup.
    fn resolve_window_handoff_identity(
        &self,
        _identity: WindowHandoffIdentity,
    ) -> Option<WindowId> {
        None
    }

    /// Put `text` on the clipboard, returning whether the backend could.
    /// X11 must own the CLIPBOARD selection to do it; Wayland sets its data
    /// device selection. Backends without clipboard support say so.
    fn set_clipboard_text(&mut self, _text: &str) -> bool {
        false
    }

    /// Return a thread-safe route to the backend's native image clipboard.
    ///
    /// Screenshot PNG encoding completes asynchronously, so the producer
    /// cannot borrow the backend when the bytes become ready. X11 backends
    /// route this to their dedicated selection-owner thread; unsupported
    /// backends return `None` and the screenshot feature may use a native
    /// platform helper instead.
    fn clipboard_image_sender(
        &self,
    ) -> Option<crate::backend::clipboard_offer::ClipboardImageSender> {
        None
    }

    /// Text copied by other applications since the last call, oldest first.
    /// The backend has already dropped offers marked as secrets and anything
    /// that is not text, so a password never reaches the history.
    fn drain_clipboard(&mut self) -> Vec<String> {
        Vec::new()
    }

    /// Milliseconds since the last keyboard or pointer input anywhere in the
    /// session, if this backend can tell.
    ///
    /// The window manager only sees the events it grabbed, so it cannot count
    /// this itself: a session spent typing into one window looks idle from up
    /// here. `None` means the backend has no idle clock, and the idle policy
    /// stays out of the way rather than guessing.
    fn idle_millis(&mut self) -> Option<u64> {
        None
    }

    /// Whether a client asked to be left awake — a video player holding an
    /// idle inhibitor. Backends without the protocol never inhibit.
    fn idle_inhibited_by_client(&self) -> bool {
        false
    }

    /// Switch off the display server's own blanking timer, because this
    /// session now has an idle policy of its own.
    ///
    /// On X11 the two do not merely overlap, they fight: when the server's
    /// blanker fires it resets the idle clock this policy reads, so a lock
    /// timeout longer than the server's blanking timeout would never arrive.
    /// Returns whether anything was changed.
    fn suppress_server_screensaver(&mut self) -> bool {
        false
    }

    // Ops Getters
    fn window_ops(&self) -> &dyn WindowOps;
    fn input_ops(&self) -> &dyn InputOps;
    fn property_ops(&self) -> &dyn PropertyOps;
    fn output_ops(&self) -> &dyn OutputOps;
    fn key_ops(&self) -> &dyn KeyOps;
    fn key_ops_mut(&mut self) -> &mut dyn KeyOps;
    fn cursor_provider(&mut self) -> &mut dyn CursorProvider;
    fn color_allocator(&mut self) -> &mut dyn ColorAllocator;

    fn register_wm(&self, _name: &str) -> Result<(), BackendError> {
        Ok(())
    }

    // 通用清理接口
    fn cleanup(&mut self) -> Result<(), BackendError> {
        Ok(())
    }

    fn on_focused_client_changed(&mut self, _win: Option<WindowId>) -> Result<(), BackendError> {
        Ok(())
    }
    fn on_client_list_changed(
        &mut self,
        _clients: &[WindowId],
        _stack: &[WindowId],
    ) -> Result<(), BackendError> {
        Ok(())
    }

    fn on_desktop_changed(
        &mut self,
        _current: u32,
        _total: u32,
        _names: &[&str],
    ) -> Result<(), BackendError> {
        Ok(())
    }

    fn set_workarea(&mut self, _areas: &[(i32, i32, u32, u32)]) -> Result<(), BackendError> {
        Ok(())
    }

    fn begin_move(&mut self, _win: WindowId) -> Result<(), BackendError> {
        Ok(())
    }

    fn begin_resize(&mut self, _win: WindowId, _edge: ResizeEdge) -> Result<(), BackendError> {
        Ok(())
    }

    /// Start a track-only interactive drag: grab the pointer and feed
    /// `handle_motion`/`handle_button_release` without moving any window.
    /// `intent` is the operation the drag will become once it crosses the
    /// drag threshold, and picks the cursor shown meanwhile. Returns `false`
    /// when the backend cannot track (no grab support, or the grab failed) —
    /// the caller must then abandon the drag.
    fn begin_track(
        &mut self,
        _win: WindowId,
        _intent: InteractionAction,
    ) -> Result<bool, BackendError> {
        Ok(false)
    }

    fn handle_motion(&mut self, _x: f64, _y: f64, _time: u32) -> Result<bool, BackendError> {
        Ok(false)
    }

    fn handle_button_release(&mut self, _time: u32) -> Result<bool, BackendError> {
        Ok(false)
    }

    /// Return the current geometry of the window being dragged/resized, if any.
    /// Used to keep JWM's client.geometry in sync during interactive move/resize.
    fn interaction_geometry(&self) -> Option<(WindowId, i32, i32, u32, u32)> {
        None
    }

    /// Which interactive operation the backend is currently tracking, if any.
    /// `None` also covers backends that track drags without reporting them.
    fn interaction_action(&self) -> Option<InteractionAction> {
        None
    }

    fn run(&mut self, handler: &mut dyn EventHandler) -> Result<(), BackendError>;

    fn compositor_render_frame(
        &mut self,
        _scene: &[(u64, i32, i32, u32, u32)],
        _focused_window: Option<u64>,
    ) -> Result<bool, BackendError> {
        Ok(false)
    }

    fn set_compositor_enabled(&mut self, _enabled: bool) -> Result<bool, BackendError> {
        Ok(false)
    }
    fn has_partial_damage(&self) -> bool {
        false
    }
    fn set_partial_damage(&mut self, _enabled: bool) -> Result<bool, BackendError> {
        Ok(false)
    }
}

// 兼容性定义
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AllowMode {
    AsyncPointer,
    ReplayPointer,
    SyncPointer,
    AsyncKeyboard,
    SyncKeyboard,
    ReplayKeyboard,
    AsyncBoth,
    SyncBoth,
}
