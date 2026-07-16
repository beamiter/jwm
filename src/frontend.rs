//! Host-neutral wire types and change tracking for stateful frontends.
//!
//! This module deliberately knows nothing about Tauri, webviews, windows, or
//! event names. A host can turn a runtime-produced revision, [`DirtyBits`],
//! and [`BarSnapshot`] into one serializable [`FrontendEnvelope`], then use a
//! [`SnapshotCursor`] to suppress duplicate or stale delivery.

use std::{fmt, time::Instant};

use serde::{Deserialize, Serialize};

use crate::{
    DirtyBits,
    model::{BarSnapshot, LayoutId, MAX_MODEL_TAGS, MonitorId, TagId, UserAction},
    runtime::{BarRuntime, RuntimeFrame, RuntimeSchedule, RuntimeUpdate},
};

/// A complete, revisioned frontend state message.
///
/// `changes` describes the semantic fields changed since the preceding
/// revision. `snapshot` is always complete: consumers never need to merge
/// optional model fields or reconstruct an absent geometry or battery.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrontendEnvelope {
    pub revision: u64,
    pub changes: DirtyBits,
    pub snapshot: BarSnapshot,
    /// Optional coarse sections for frontends that keep independent stores.
    /// A consumer can derive the same value from `changes` when this is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition_changes: Option<FrontendPartitions>,
}

impl FrontendEnvelope {
    #[must_use]
    pub const fn new(revision: u64, changes: DirtyBits, snapshot: BarSnapshot) -> Self {
        Self {
            revision,
            changes,
            snapshot,
            partition_changes: None,
        }
    }

    /// Copy the wire-relevant state from a coherent runtime frame. Platform
    /// effects and issues remain on `frame.update` for the host to handle.
    #[must_use]
    pub fn from_runtime_frame(frame: &RuntimeFrame) -> Self {
        Self::new(frame.revision, frame.changes(), frame.snapshot.clone())
    }

    /// Attach coarse frontend sections derived from this envelope's dirty
    /// flags. This does not alter the canonical `changes` value.
    #[must_use]
    pub fn with_derived_partition_changes(mut self) -> Self {
        self.partition_changes = Some(FrontendPartitions::from_dirty(self.changes));
        self
    }

    /// Return explicit section changes when present, otherwise derive them
    /// from the canonical dirty flags.
    #[must_use]
    pub fn effective_partition_changes(&self) -> FrontendPartitions {
        self.partition_changes
            .unwrap_or_else(|| FrontendPartitions::from_dirty(self.changes))
    }
}

/// Coarse frontend sections derived from semantic [`DirtyBits`].
///
/// This is deliberately smaller than `DirtyBits`: time, theme, and other
/// full-snapshot consumers can continue to inspect the canonical flags.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct FrontendPartitions(u8);

impl FrontendPartitions {
    pub const NONE: u8 = 0;
    pub const MONITOR: u8 = 1 << 0;
    pub const SYSTEM: u8 = 1 << 1;
    pub const AUDIO: u8 = 1 << 2;
    pub const BRIGHTNESS: u8 = 1 << 3;

    const KNOWN: u8 = Self::MONITOR | Self::SYSTEM | Self::AUDIO | Self::BRIGHTNESS;
    const MONITOR_DIRTY: DirtyBits = DirtyBits::new(
        DirtyBits::MONITOR_CHANGED
            | DirtyBits::GEOMETRY_CHANGED
            | DirtyBits::LAYOUT_CHANGED
            | DirtyBits::CLIENT_CHANGED,
    );
    const SYSTEM_DIRTY: DirtyBits =
        DirtyBits::new(DirtyBits::SYSTEM_CHANGED | DirtyBits::BATTERY_CHANGED);

    #[must_use]
    pub const fn new(bits: u8) -> Self {
        Self(bits & Self::KNOWN)
    }

    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == Self::NONE
    }

    #[must_use]
    pub const fn contains(self, partition: u8) -> bool {
        self.0 & partition != 0
    }

    #[must_use]
    pub const fn all() -> Self {
        Self(Self::KNOWN)
    }

    /// Map fine-grained model dirtiness to the four stores used by existing
    /// web frontends.
    #[must_use]
    pub const fn from_dirty(changes: DirtyBits) -> Self {
        let mut bits = Self::NONE;
        if changes.intersects(Self::MONITOR_DIRTY) {
            bits |= Self::MONITOR;
        }
        if changes.intersects(Self::SYSTEM_DIRTY) {
            bits |= Self::SYSTEM;
        }
        if changes.contains(DirtyBits::AUDIO_CHANGED) {
            bits |= Self::AUDIO;
        }
        if changes.contains(DirtyBits::BRIGHTNESS_CHANGED) {
            bits |= Self::BRIGHTNESS;
        }
        Self(bits)
    }
}

impl<'de> Deserialize<'de> for FrontendPartitions {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        u8::deserialize(deserializer).map(Self::new)
    }
}

/// Stateful delivery cursor for a complete [`BarSnapshot`] wire stream.
///
/// Revisions provide ordering: an update older than the last accepted
/// revision is ignored. Snapshot equality provides payload deduplication even
/// when a runtime revision advances without an observable state change. If a
/// snapshot changes without complete dirty flags, the cursor derives the
/// missing flags from the previous snapshot.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SnapshotCursor {
    revision: Option<u64>,
    previous_snapshot: Option<BarSnapshot>,
}

/// Result of one frontend session operation.
///
/// The coherent runtime frame always remains available for issue and platform
/// effect handling. `envelope` is present only when frontend-observable state
/// should be delivered, so hosts do not need a second emitted-state cache.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionOutput {
    pub frame: RuntimeFrame,
    pub envelope: Option<FrontendEnvelope>,
}

impl SessionOutput {
    #[must_use]
    pub fn into_parts(self) -> (RuntimeFrame, Option<FrontendEnvelope>) {
        (self.frame, self.envelope)
    }
}

/// Host-neutral owner of runtime cadence and frontend delivery state.
///
/// A host remains responsible for synchronization, threads, native wakeups,
/// window operations, and platform effects. This type only ensures that
/// service and action turns produce one coherent frame and at most one
/// deduplicated wire envelope.
pub struct FrontendSession {
    runtime: BarRuntime,
    schedule: RuntimeSchedule,
    cursor: SnapshotCursor,
}

impl Default for FrontendSession {
    fn default() -> Self {
        Self::new(BarRuntime::default())
    }
}

impl FrontendSession {
    #[must_use]
    pub fn new(runtime: BarRuntime) -> Self {
        Self::with_schedule(runtime, RuntimeSchedule::default())
    }

    #[must_use]
    pub const fn with_schedule(runtime: BarRuntime, schedule: RuntimeSchedule) -> Self {
        Self {
            runtime,
            schedule,
            cursor: SnapshotCursor::new(),
        }
    }

    /// Service transport/providers now and project one optional wire update.
    pub fn service(&mut self) -> SessionOutput {
        let frame = self.schedule.service_frame(&mut self.runtime);
        self.capture(frame)
    }

    /// Deterministic service variant for event loops and tests.
    pub fn service_at(&mut self, now: Instant) -> SessionOutput {
        let frame = self.schedule.service_frame_at(&mut self.runtime, now);
        self.capture(frame)
    }

    /// Validate and dispatch one wire action, retaining all platform work and
    /// issues on the returned frame.
    pub fn dispatch(
        &mut self,
        request: ActionRequest,
    ) -> Result<SessionOutput, ActionRequestError> {
        let frame = request.dispatch_frame(&mut self.runtime)?;
        Ok(self.capture(frame))
    }

    /// Replay the most recently accepted complete frontend state.
    #[must_use]
    pub fn replay(&self) -> Option<FrontendEnvelope> {
        self.cursor.replay()
    }

    /// Make the next delivered state a complete initial envelope without
    /// resetting runtime state or provider cadence.
    pub fn reset_delivery(&mut self) {
        self.cursor.reset();
    }

    /// Earliest required service time; native events may service sooner.
    #[must_use]
    pub fn next_service_deadline(&self, now: Instant) -> Instant {
        self.schedule.next_service_deadline(&self.runtime, now)
    }

    #[must_use]
    pub const fn runtime(&self) -> &BarRuntime {
        &self.runtime
    }

    pub const fn runtime_mut(&mut self) -> &mut BarRuntime {
        &mut self.runtime
    }

    #[must_use]
    pub const fn schedule(&self) -> &RuntimeSchedule {
        &self.schedule
    }

    pub const fn schedule_mut(&mut self) -> &mut RuntimeSchedule {
        &mut self.schedule
    }

    #[must_use]
    pub const fn cursor(&self) -> &SnapshotCursor {
        &self.cursor
    }

    fn capture(&mut self, frame: RuntimeFrame) -> SessionOutput {
        let envelope = self.cursor.update_frame(&frame);
        SessionOutput { frame, envelope }
    }
}

impl SnapshotCursor {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            revision: None,
            previous_snapshot: None,
        }
    }

    /// Accept a runtime frame's wire parts and return an envelope only when a
    /// non-stale snapshot should be delivered.
    pub fn update(
        &mut self,
        revision: u64,
        mut changes: DirtyBits,
        snapshot: BarSnapshot,
    ) -> Option<FrontendEnvelope> {
        if self.revision.is_some_and(|current| revision < current) {
            return None;
        }

        let Some(previous) = self.previous_snapshot.as_ref() else {
            self.revision = Some(revision);
            self.previous_snapshot = Some(snapshot.clone());
            return Some(
                FrontendEnvelope::new(revision, DirtyBits::all(), snapshot)
                    .with_derived_partition_changes(),
            );
        };

        if previous == &snapshot {
            self.revision = Some(
                self.revision
                    .map_or(revision, |current| current.max(revision)),
            );
            return None;
        }

        changes |= snapshot_changes(previous, &snapshot);
        if changes.is_empty() {
            // Opaque metadata such as wm_sequence may advance without a
            // frontend-observable change. Retain it for the next replay while
            // suppressing an empty delivery.
            self.revision = Some(revision);
            self.previous_snapshot = Some(snapshot);
            return None;
        }
        self.revision = Some(revision);
        self.previous_snapshot = Some(snapshot.clone());
        Some(FrontendEnvelope::new(revision, changes, snapshot).with_derived_partition_changes())
    }

    /// Borrowed convenience variant of [`Self::update`].
    pub fn update_ref(
        &mut self,
        revision: u64,
        changes: DirtyBits,
        snapshot: &BarSnapshot,
    ) -> Option<FrontendEnvelope> {
        self.update(revision, changes, snapshot.clone())
    }

    /// Accept the wire-relevant state from one coherent runtime frame.
    pub fn update_frame(&mut self, frame: &RuntimeFrame) -> Option<FrontendEnvelope> {
        self.update_ref(frame.revision, frame.changes(), &frame.snapshot)
    }

    /// Replay the latest accepted snapshot as a complete initial state.
    #[must_use]
    pub fn replay(&self) -> Option<FrontendEnvelope> {
        Some(
            FrontendEnvelope::new(
                self.revision?,
                DirtyBits::all(),
                self.previous_snapshot.as_ref()?.clone(),
            )
            .with_derived_partition_changes(),
        )
    }

    /// Forget both ordering and payload state. The next update is initial and
    /// therefore carries all dirty and partition bits.
    pub fn reset(&mut self) {
        self.revision = None;
        self.previous_snapshot = None;
    }

    #[must_use]
    pub const fn revision(&self) -> Option<u64> {
        self.revision
    }

    #[must_use]
    pub fn previous_snapshot(&self) -> Option<&BarSnapshot> {
        self.previous_snapshot.as_ref()
    }
}

/// Derive semantic dirty flags by comparing two complete model snapshots.
///
/// This is a correctness fallback for bridges that lost or coalesced an
/// operation's `RuntimeUpdate`; the runtime-provided dirty flags remain the
/// primary scheduling signal.
#[must_use]
pub fn snapshot_changes(previous: &BarSnapshot, current: &BarSnapshot) -> DirtyBits {
    let mut changes = DirtyBits::default();

    if previous.wm_available != current.wm_available
        || previous.tags != current.tags
        || previous.active_tag != current.active_tag
        || previous.monitor != current.monitor
    {
        changes.set(DirtyBits::MONITOR_CHANGED);
    }
    if previous.geometry != current.geometry {
        changes.set(DirtyBits::GEOMETRY_CHANGED);
    }
    if previous.layout_symbol != current.layout_symbol
        || previous.layout_selector_open != current.layout_selector_open
    {
        changes.set(DirtyBits::LAYOUT_CHANGED);
    }
    if previous.client_name != current.client_name {
        changes.set(DirtyBits::CLIENT_CHANGED);
    }
    if previous.time != current.time || previous.show_seconds != current.show_seconds {
        changes.set(DirtyBits::TIME_CHANGED);
    }
    if previous.theme != current.theme {
        changes.set(DirtyBits::THEME_CHANGED);
    }
    if previous.audio != current.audio || previous.audio_device != current.audio_device {
        changes.set(DirtyBits::AUDIO_CHANGED);
    }
    if previous.system != current.system || previous.system_details != current.system_details {
        changes.set(DirtyBits::SYSTEM_CHANGED);
    }
    if previous.brightness != current.brightness {
        changes.set(DirtyBits::BRIGHTNESS_CHANGED);
    }
    if previous.battery != current.battery {
        changes.set(DirtyBits::BATTERY_CHANGED);
    }

    changes
}

/// Stable, framework-neutral request accepted from a frontend bridge.
///
/// The internally tagged representation is shaped like
/// `{ "action": "view_tag_on", "tag_index": 0, "monitor_id": 1 }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ActionRequest {
    ViewTag { tag_index: usize },
    ToggleTag { tag_index: usize },
    ViewTagOn { tag_index: usize, monitor_id: i32 },
    ToggleTagOn { tag_index: usize, monitor_id: i32 },
    ToggleLayoutSelector,
    SetLayout { layout_id: u32 },
    SetLayoutOn { layout_id: u32, monitor_id: i32 },
    ToggleSeconds,
    ToggleTheme,
    ToggleMute,
    VolumeUp,
    VolumeDown,
    AdjustVolume { delta: i32 },
    BrightnessUp,
    BrightnessDown,
    AdjustBrightness { delta: i32 },
    RefreshBattery,
    Screenshot,
    OpenAudioControl,
}

impl ActionRequest {
    pub fn into_user_action(self) -> Result<UserAction, ActionRequestError> {
        self.try_into()
    }

    /// Validate and dispatch this wire request through a runtime.
    pub fn dispatch(self, runtime: &mut BarRuntime) -> Result<RuntimeUpdate, ActionRequestError> {
        Ok(runtime.dispatch(self.into_user_action()?))
    }

    /// Validate, dispatch, and capture one coherent frontend-ready frame.
    pub fn dispatch_frame(
        self,
        runtime: &mut BarRuntime,
    ) -> Result<RuntimeFrame, ActionRequestError> {
        Ok(runtime.dispatch_frame(self.into_user_action()?))
    }
}

/// Wire-boundary validation failure while converting an [`ActionRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionRequestError {
    TagOutOfRange {
        tag_index: usize,
        max_exclusive: usize,
    },
}

impl fmt::Display for ActionRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TagOutOfRange {
                tag_index,
                max_exclusive,
            } => write!(
                f,
                "tag_index must be in 0..{max_exclusive}, got {tag_index}"
            ),
        }
    }
}

impl std::error::Error for ActionRequestError {}

impl TryFrom<ActionRequest> for UserAction {
    type Error = ActionRequestError;

    fn try_from(request: ActionRequest) -> Result<Self, Self::Error> {
        Ok(match request {
            ActionRequest::ViewTag { tag_index } => Self::ViewTag(checked_tag(tag_index)?),
            ActionRequest::ToggleTag { tag_index } => Self::ToggleTag(checked_tag(tag_index)?),
            ActionRequest::ViewTagOn {
                tag_index,
                monitor_id,
            } => Self::ViewTagOn {
                tag: checked_tag(tag_index)?,
                monitor: MonitorId(monitor_id),
            },
            ActionRequest::ToggleTagOn {
                tag_index,
                monitor_id,
            } => Self::ToggleTagOn {
                tag: checked_tag(tag_index)?,
                monitor: MonitorId(monitor_id),
            },
            ActionRequest::ToggleLayoutSelector => Self::ToggleLayoutSelector,
            ActionRequest::SetLayout { layout_id } => Self::SetLayout(LayoutId(layout_id)),
            ActionRequest::SetLayoutOn {
                layout_id,
                monitor_id,
            } => Self::SetLayoutOn {
                layout: LayoutId(layout_id),
                monitor: MonitorId(monitor_id),
            },
            ActionRequest::ToggleSeconds => Self::ToggleSeconds,
            ActionRequest::ToggleTheme => Self::ToggleTheme,
            ActionRequest::ToggleMute => Self::ToggleMute,
            ActionRequest::VolumeUp => Self::VolumeUp,
            ActionRequest::VolumeDown => Self::VolumeDown,
            ActionRequest::AdjustVolume { delta } => Self::AdjustVolume(delta),
            ActionRequest::BrightnessUp => Self::BrightnessUp,
            ActionRequest::BrightnessDown => Self::BrightnessDown,
            ActionRequest::AdjustBrightness { delta } => Self::AdjustBrightness(delta),
            ActionRequest::RefreshBattery => Self::RefreshBattery,
            ActionRequest::Screenshot => Self::Screenshot,
            ActionRequest::OpenAudioControl => Self::OpenAudioControl,
        })
    }
}

fn checked_tag(tag_index: usize) -> Result<TagId, ActionRequestError> {
    TagId::new(tag_index).ok_or(ActionRequestError::TagOutOfRange {
        tag_index,
        max_exclusive: MAX_MODEL_TAGS,
    })
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use serde::de::value::{Error as ValueError, U8Deserializer};

    use super::{
        ActionRequest, ActionRequestError, FrontendEnvelope, FrontendPartitions, FrontendSession,
        SnapshotCursor, snapshot_changes,
    };
    use crate::{
        AudioState, BarModel, BatteryState, BrightnessState, DirtyBits, LayoutId, MonitorGeometry,
        MonitorId, Percent, SystemState, TagId, ThemeMode, UserAction,
        model::MAX_MODEL_TAGS,
        runtime::{RuntimeFrame, RuntimeUpdate},
    };

    fn snapshot() -> crate::BarSnapshot {
        BarModel::default().snapshot()
    }

    #[test]
    fn envelope_preserves_optional_model_semantics_and_layout_text() {
        let mut state = snapshot();
        state.geometry = None;
        state.battery = BatteryState::absent();
        state.layout_symbol = "tile s: literal".to_owned();

        let envelope = FrontendEnvelope::new(7, DirtyBits::default(), state.clone());

        assert_eq!(envelope.snapshot, state);
        assert_eq!(envelope.snapshot.geometry, None);
        assert_eq!(envelope.snapshot.battery, BatteryState::absent());
        assert_eq!(envelope.snapshot.layout_symbol, "tile s: literal");
    }

    #[test]
    fn envelope_and_cursor_accept_a_coherent_runtime_frame() {
        let frame = RuntimeFrame {
            revision: 21,
            snapshot: snapshot(),
            update: RuntimeUpdate {
                changes: DirtyBits::new(DirtyBits::AUDIO_CHANGED),
                ..RuntimeUpdate::default()
            },
        };

        let envelope = FrontendEnvelope::from_runtime_frame(&frame);
        assert_eq!(envelope.revision, 21);
        assert_eq!(envelope.changes, frame.update.changes);
        assert_eq!(envelope.snapshot, frame.snapshot);

        let mut cursor = SnapshotCursor::new();
        let initial = cursor.update_frame(&frame).unwrap();
        assert_eq!(initial.revision, 21);
        assert_eq!(initial.changes, DirtyBits::all());
    }

    #[test]
    fn partitions_map_requested_dirty_groups_only() {
        let monitor = FrontendPartitions::from_dirty(DirtyBits::new(
            DirtyBits::MONITOR_CHANGED
                | DirtyBits::GEOMETRY_CHANGED
                | DirtyBits::LAYOUT_CHANGED
                | DirtyBits::CLIENT_CHANGED,
        ));
        assert_eq!(monitor.bits(), FrontendPartitions::MONITOR);

        let system = FrontendPartitions::from_dirty(DirtyBits::new(
            DirtyBits::SYSTEM_CHANGED | DirtyBits::BATTERY_CHANGED,
        ));
        assert_eq!(system.bits(), FrontendPartitions::SYSTEM);

        let controls = FrontendPartitions::from_dirty(DirtyBits::new(
            DirtyBits::AUDIO_CHANGED | DirtyBits::BRIGHTNESS_CHANGED,
        ));
        assert!(controls.contains(FrontendPartitions::AUDIO));
        assert!(controls.contains(FrontendPartitions::BRIGHTNESS));
        assert!(!controls.contains(FrontendPartitions::MONITOR));

        let unrelated = FrontendPartitions::from_dirty(DirtyBits::new(
            DirtyBits::TIME_CHANGED | DirtyBits::THEME_CHANGED,
        ));
        assert!(unrelated.is_empty());
        assert_eq!(
            FrontendPartitions::from_dirty(DirtyBits::all()),
            FrontendPartitions::all()
        );
    }

    #[test]
    fn partition_deserialization_discards_unknown_bits() {
        let partitions =
            FrontendPartitions::deserialize(U8Deserializer::<ValueError>::new(u8::MAX)).unwrap();
        assert_eq!(partitions, FrontendPartitions::all());
    }

    #[test]
    fn cursor_deduplicates_orders_replays_and_resets() {
        let state = snapshot();
        let mut cursor = SnapshotCursor::new();

        let initial = cursor
            .update(10, DirtyBits::default(), state.clone())
            .unwrap();
        assert_eq!(initial.changes, DirtyBits::all());
        assert_eq!(initial.partition_changes, Some(FrontendPartitions::all()));
        assert!(cursor.update(10, DirtyBits::all(), state.clone()).is_none());

        // A newer no-op revision advances ordering without emitting payload.
        assert!(cursor.update(12, DirtyBits::all(), state.clone()).is_none());
        assert_eq!(cursor.revision(), Some(12));

        let replay = cursor.replay().unwrap();
        assert_eq!(replay.revision, 12);
        assert_eq!(replay.changes, DirtyBits::all());

        let mut stale = state.clone();
        stale.client_name = "stale".to_owned();
        assert!(cursor.update(11, DirtyBits::default(), stale).is_none());
        assert_eq!(cursor.previous_snapshot(), Some(&state));

        let mut current = state;
        current.geometry = Some(MonitorGeometry {
            x: 10,
            y: 20,
            width: 1920,
            height: 1080,
        });
        current.battery = BatteryState::present(Some(Percent::from_whole(42).unwrap()), false);
        let update = cursor
            .update(12, DirtyBits::default(), current.clone())
            .unwrap();
        assert!(update.changes.contains(DirtyBits::GEOMETRY_CHANGED));
        assert!(update.changes.contains(DirtyBits::BATTERY_CHANGED));
        assert!(
            update
                .effective_partition_changes()
                .contains(FrontendPartitions::MONITOR)
        );
        assert!(
            update
                .effective_partition_changes()
                .contains(FrontendPartitions::SYSTEM)
        );

        cursor.reset();
        assert_eq!(cursor.revision(), None);
        assert_eq!(cursor.previous_snapshot(), None);
        assert_eq!(cursor.replay(), None);
        assert_eq!(
            cursor
                .update(1, DirtyBits::default(), current)
                .unwrap()
                .changes,
            DirtyBits::all()
        );
    }

    #[test]
    fn cursor_coalesces_opaque_sequence_metadata_but_retains_it_for_replay() {
        let state = snapshot();
        let mut cursor = SnapshotCursor::new();
        cursor
            .update(4, DirtyBits::default(), state.clone())
            .unwrap();

        let mut metadata_only = state;
        metadata_only.wm_sequence = Some(99);
        assert!(
            cursor
                .update(4, DirtyBits::default(), metadata_only.clone())
                .is_none()
        );
        assert_eq!(cursor.previous_snapshot(), Some(&metadata_only));
        assert_eq!(cursor.replay().unwrap().snapshot.wm_sequence, Some(99));
    }

    #[test]
    fn snapshot_diff_classifies_every_serialized_state_group() {
        let original = snapshot();
        let assert_exact = |changed: crate::BarSnapshot, expected| {
            let actual = snapshot_changes(&original, &changed);
            assert_eq!(actual, DirtyBits::new(expected));
        };

        let mut changed = original.clone();
        changed.wm_available = true;
        assert_exact(changed, DirtyBits::MONITOR_CHANGED);

        let mut changed = original.clone();
        changed.wm_sequence = Some(1);
        assert_exact(changed, DirtyBits::NONE);

        let mut changed = original.clone();
        changed.geometry = Some(MonitorGeometry {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        });
        assert_exact(changed, DirtyBits::GEOMETRY_CHANGED);

        let mut changed = original.clone();
        changed.layout_symbol = "monocle".to_owned();
        assert_exact(changed, DirtyBits::LAYOUT_CHANGED);

        let mut changed = original.clone();
        changed.client_name = "client".to_owned();
        assert_exact(changed, DirtyBits::CLIENT_CHANGED);

        let mut changed = original.clone();
        changed.time = "12:34".to_owned();
        assert_exact(changed, DirtyBits::TIME_CHANGED);

        let mut changed = original.clone();
        changed.theme = ThemeMode::Light;
        assert_exact(changed, DirtyBits::THEME_CHANGED);

        let mut changed = original.clone();
        changed.audio = AudioState::from_f64(Some(50.0), false).unwrap();
        assert_exact(changed, DirtyBits::AUDIO_CHANGED);

        let mut changed = original.clone();
        changed.system = SystemState::from_f64(Some(25.0), Some(30.0)).unwrap();
        assert_exact(changed, DirtyBits::SYSTEM_CHANGED);

        let mut changed = original.clone();
        changed.brightness = BrightnessState::from_f64(Some(80.0)).unwrap();
        assert_exact(changed, DirtyBits::BRIGHTNESS_CHANGED);

        let mut changed = original.clone();
        changed.battery = BatteryState::present(None, true);
        assert_exact(changed, DirtyBits::BATTERY_CHANGED);
    }

    #[test]
    fn every_action_request_converts_to_the_typed_model_action() {
        let tag = TagId::new(2).unwrap();
        let cases = [
            (
                ActionRequest::ViewTag { tag_index: 2 },
                UserAction::ViewTag(tag),
            ),
            (
                ActionRequest::ToggleTag { tag_index: 2 },
                UserAction::ToggleTag(tag),
            ),
            (
                ActionRequest::ViewTagOn {
                    tag_index: 2,
                    monitor_id: -1,
                },
                UserAction::ViewTagOn {
                    tag,
                    monitor: MonitorId(-1),
                },
            ),
            (
                ActionRequest::ToggleTagOn {
                    tag_index: 2,
                    monitor_id: 3,
                },
                UserAction::ToggleTagOn {
                    tag,
                    monitor: MonitorId(3),
                },
            ),
            (
                ActionRequest::ToggleLayoutSelector,
                UserAction::ToggleLayoutSelector,
            ),
            (
                ActionRequest::SetLayout { layout_id: 4 },
                UserAction::SetLayout(LayoutId(4)),
            ),
            (
                ActionRequest::SetLayoutOn {
                    layout_id: 4,
                    monitor_id: 3,
                },
                UserAction::SetLayoutOn {
                    layout: LayoutId(4),
                    monitor: MonitorId(3),
                },
            ),
            (ActionRequest::ToggleSeconds, UserAction::ToggleSeconds),
            (ActionRequest::ToggleTheme, UserAction::ToggleTheme),
            (ActionRequest::ToggleMute, UserAction::ToggleMute),
            (ActionRequest::VolumeUp, UserAction::VolumeUp),
            (ActionRequest::VolumeDown, UserAction::VolumeDown),
            (
                ActionRequest::AdjustVolume { delta: -7 },
                UserAction::AdjustVolume(-7),
            ),
            (ActionRequest::BrightnessUp, UserAction::BrightnessUp),
            (ActionRequest::BrightnessDown, UserAction::BrightnessDown),
            (
                ActionRequest::AdjustBrightness { delta: 9 },
                UserAction::AdjustBrightness(9),
            ),
            (ActionRequest::RefreshBattery, UserAction::RefreshBattery),
            (ActionRequest::Screenshot, UserAction::Screenshot),
            (
                ActionRequest::OpenAudioControl,
                UserAction::OpenAudioControl,
            ),
        ];

        for (request, expected) in cases {
            assert_eq!(request.into_user_action().unwrap(), expected);
        }
    }

    #[test]
    fn action_request_rejects_out_of_protocol_tag_index() {
        let error = ActionRequest::ViewTag {
            tag_index: MAX_MODEL_TAGS,
        }
        .into_user_action()
        .unwrap_err();

        assert_eq!(
            error,
            ActionRequestError::TagOutOfRange {
                tag_index: MAX_MODEL_TAGS,
                max_exclusive: MAX_MODEL_TAGS,
            }
        );
        assert_eq!(
            error.to_string(),
            format!("tag_index must be in 0..{MAX_MODEL_TAGS}, got {MAX_MODEL_TAGS}")
        );
    }

    #[test]
    fn action_request_can_dispatch_a_coherent_runtime_frame() {
        let mut runtime = crate::BarRuntime::default();
        let _ = runtime.current_frame();

        let frame = ActionRequest::ToggleTheme
            .dispatch_frame(&mut runtime)
            .unwrap();

        assert_eq!(frame.snapshot.theme, ThemeMode::Light);
        assert!(frame.changes().contains(DirtyBits::THEME_CHANGED));
    }

    #[test]
    fn frontend_session_services_dispatches_deduplicates_and_replays() {
        let now = std::time::Instant::now();
        let mut session = FrontendSession::default();

        let initial = session.service_at(now);
        assert_eq!(initial.envelope.as_ref().unwrap().changes, DirtyBits::all());
        assert_eq!(initial.frame.revision, 1);

        let unchanged = session.service_at(now + std::time::Duration::from_millis(100));
        assert!(unchanged.envelope.is_none());

        let themed = session.dispatch(ActionRequest::ToggleTheme).unwrap();
        assert_eq!(themed.frame.snapshot.theme, ThemeMode::Light);
        assert!(
            themed
                .envelope
                .as_ref()
                .unwrap()
                .changes
                .contains(DirtyBits::THEME_CHANGED)
        );

        let platform_only = session.dispatch(ActionRequest::Screenshot).unwrap();
        assert!(platform_only.envelope.is_none());
        assert_eq!(
            platform_only.frame.update.platform_effects,
            vec![crate::BarEffect::Screenshot]
        );
        assert_eq!(
            session.replay().unwrap().revision,
            platform_only.frame.revision
        );

        session.reset_delivery();
        assert!(session.replay().is_none());
        assert!(
            session
                .dispatch(ActionRequest::ToggleSeconds)
                .unwrap()
                .envelope
                .is_some()
        );
    }
}
