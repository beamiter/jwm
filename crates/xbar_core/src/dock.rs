//! Toolkit-neutral minimized-window Dock reporting state.
//!
//! Renderers build the shared presentation [`Scene`],
//! then feed it to [`DockReporter`]. The reporter turns stable logical slots
//! into global physical animation targets and owns preview/geometry delivery
//! acknowledgement, retry throttling, and lease renewal. It never executes an
//! effect itself, so Cairo, GTK, egui, HTML, and GPU adapters share one policy.

use std::time::{Duration, Instant};

use crate::model::{DockItemGeometry, MinimizedWindow, MonitorGeometry, UserAction, WindowToken};
use crate::presentation::{NodeId, PresentationConfig, Rect, Scene};

/// A visible preview is renewed before the compositor's lease can expire.
pub const DOCK_PREVIEW_LEASE_INTERVAL: Duration = Duration::from_secs(2);
/// A moving/magnified Dock card may refresh its preview anchor at most this
/// often.  This keeps the compositor preview visually attached to the card
/// without turning every pointer-motion frame into an IPC write.
pub const DOCK_PREVIEW_ANCHOR_INTERVAL: Duration = Duration::from_millis(50);
/// Backpressure retry throttle used between native input/service turns.
pub const DOCK_RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// One shelf (`window == None`) or item target in global physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DockGeometryReport {
    pub window: Option<WindowToken>,
    pub geometry: DockItemGeometry,
}

/// Complete input for a render-time Dock synchronization pass.
#[derive(Debug, Clone, Copy)]
pub struct DockReporterInput<'a> {
    pub wm_available: bool,
    pub wm_session_id: u64,
    /// Exact minimized projection generation used to build this scene.
    pub minimized_generation: u64,
    /// Increment whenever the adapter replaces or reconnects its WM channel.
    pub transport_generation: u64,
    pub monitor_geometry: Option<MonitorGeometry>,
    pub minimized_windows: &'a [MinimizedWindow],
    pub scene: &'a Scene,
    pub presentation: &'a PresentationConfig,
    /// Normally [`InteractionState::hovered`](crate::presentation::InteractionState::hovered).
    pub hovered: Option<NodeId>,
    pub scale_factor: f64,
}

/// Delivery state shared by native and toolkit Dock adapters.
#[derive(Debug, Clone, Default)]
pub struct DockReporter {
    state_key: Option<(u64, u64)>,
    minimized_generation: u64,
    desired_preview: Option<WindowToken>,
    acknowledged_preview: Option<WindowToken>,
    preview_last_sent: Option<Instant>,
    preview_anchor: DockItemGeometry,
    acknowledged_preview_anchor: DockItemGeometry,
    preview_waiting_for_scene: bool,
    desired_geometry: Vec<DockGeometryReport>,
    interactive_geometry: Vec<DockGeometryReport>,
    acknowledged_geometry: Vec<DockGeometryReport>,
    /// Item targets acknowledged in an older projection but absent from the
    /// current one. They remain pending until a zero geometry carrying the
    /// current generation is delivered. Keeping this separate from current
    /// acknowledgements also forces surviving/reused tokens to republish
    /// their geometry for the new incarnation.
    pending_geometry_withdrawals: Vec<WindowToken>,
    retry_not_before: Option<Instant>,
}

impl DockReporter {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state_key: None,
            minimized_generation: 0,
            desired_preview: None,
            acknowledged_preview: None,
            preview_last_sent: None,
            preview_anchor: DockItemGeometry::new(0, 0, 0, 0),
            acknowledged_preview_anchor: DockItemGeometry::new(0, 0, 0, 0),
            preview_waiting_for_scene: false,
            desired_geometry: Vec::new(),
            interactive_geometry: Vec::new(),
            acknowledged_geometry: Vec::new(),
            pending_geometry_withdrawals: Vec::new(),
            retry_not_before: None,
        }
    }

    /// Synchronize model lifetime, hover intent, and the latest rendered scene.
    pub fn synchronize(&mut self, input: DockReporterInput<'_>) {
        self.synchronize_lifetime(
            input.wm_available,
            input.wm_session_id,
            input.minimized_generation,
            input.transport_generation,
            input.minimized_windows,
        );
        if self.state_key.is_none() {
            return;
        }
        self.desired_geometry = input.monitor_geometry.map_or_else(Vec::new, |monitor| {
            dock_geometry_reports(input.scene, input.presentation, monitor, input.scale_factor)
        });
        self.desired_geometry.retain(|report| {
            report.window.is_none_or(|token| {
                input
                    .minimized_windows
                    .iter()
                    .any(|window| window.token == token)
            })
        });
        self.interactive_geometry = input.monitor_geometry.map_or_else(Vec::new, |monitor| {
            interactive_dock_geometry_reports(input.scene, monitor, input.scale_factor)
        });
        self.interactive_geometry.retain(|report| {
            report.window.is_some_and(|token| {
                input
                    .minimized_windows
                    .iter()
                    .any(|window| window.token == token)
            })
        });
        self.set_hover_from_scene(input.hovered, input.minimized_windows);
    }

    /// Synchronize WM/session state when the model changes before a new frame.
    /// Existing scene targets are retained only for windows still in the model.
    pub fn synchronize_model(
        &mut self,
        wm_available: bool,
        wm_session_id: u64,
        minimized_generation: u64,
        transport_generation: u64,
        minimized_windows: &[MinimizedWindow],
        hovered: Option<NodeId>,
    ) {
        let session_changed = self
            .state_key
            .is_some_and(|(session, _)| session != wm_session_id);
        let projection_changed = self.state_key.is_some()
            && (session_changed || self.minimized_generation != minimized_generation);
        self.synchronize_lifetime(
            wm_available,
            wm_session_id,
            minimized_generation,
            transport_generation,
            minimized_windows,
        );
        // The hovered id belongs to the last rendered scene. A restarted WM
        // may immediately reuse that numeric token for a different window,
        // so only a fresh `synchronize` pass may bind it in the new session.
        if !projection_changed && self.state_key.is_some() {
            self.set_hover(hovered, minimized_windows);
        }
    }

    fn synchronize_lifetime(
        &mut self,
        wm_available: bool,
        wm_session_id: u64,
        minimized_generation: u64,
        transport_generation: u64,
        minimized_windows: &[MinimizedWindow],
    ) {
        if !wm_available {
            self.reset();
            return;
        }

        let next = (wm_session_id, transport_generation);
        let same_connection = self.state_key == Some(next);
        let same_session = self
            .state_key
            .is_some_and(|(session, _)| session == wm_session_id);
        let session_changed = self
            .state_key
            .is_some_and(|(session, _)| session != wm_session_id);
        let projection_changed = self.state_key.is_some()
            && (session_changed || self.minimized_generation != minimized_generation);
        let contains = |token| minimized_windows.iter().any(|window| window.token == token);

        if same_session {
            // A projection can stop carrying an item while its compositor
            // target is still live (for example when a newly eligible 17th
            // window pushes the oldest item out of the bounded snapshot).
            // Remember that old acknowledgement before current-model pruning.
            for token in self
                .acknowledged_geometry
                .iter()
                .filter_map(|report| report.window)
                .filter(|token| !contains(*token))
            {
                if !self.pending_geometry_withdrawals.contains(&token) {
                    self.pending_geometry_withdrawals.push(token);
                }
            }
            // If the same numeric token is present in the new projection, it
            // denotes a surviving or reused current incarnation. Never let a
            // withdrawal remembered for an intermediate projection clear it.
            self.pending_geometry_withdrawals
                .retain(|token| !contains(*token));
        }

        if !same_connection {
            self.state_key = Some(next);
            self.acknowledged_preview = None;
            self.preview_last_sent = None;
            self.acknowledged_preview_anchor = DockItemGeometry::default();
            self.acknowledged_geometry.clear();
            // A replacement channel does not reset compositor state in the
            // same WM process. Keep current-epoch withdrawals so QueueFull or
            // reconnect cannot strand an old target; a new WM session cannot
            // safely inherit them because window tokens may be reused.
            if !same_session {
                self.pending_geometry_withdrawals.clear();
            }
            self.retry_not_before = None;
        } else if projection_changed {
            self.acknowledged_preview = None;
            self.preview_last_sent = None;
            self.acknowledged_preview_anchor = DockItemGeometry::default();
            // Old acknowledgements cannot suppress reports in the new
            // generation. Removed items were copied to the withdrawal queue
            // above; current/reused tokens must send a fresh non-empty target.
            self.acknowledged_geometry.clear();
            self.retry_not_before = None;
        }
        if projection_changed {
            self.desired_preview = None;
            self.preview_anchor = DockItemGeometry::default();
            self.preview_waiting_for_scene = false;
            self.desired_geometry.clear();
            self.interactive_geometry.clear();
        }
        self.minimized_generation = minimized_generation;

        if self.desired_preview.is_some_and(|token| !contains(token)) {
            self.desired_preview = None;
            self.preview_anchor = DockItemGeometry::default();
            self.preview_waiting_for_scene = false;
        }
        if self
            .acknowledged_preview
            .is_some_and(|token| !contains(token))
        {
            self.acknowledged_preview = None;
            self.preview_last_sent = None;
        }
        self.desired_geometry
            .retain(|report| report.window.is_none_or(contains));
        self.interactive_geometry
            .retain(|report| report.window.is_some_and(contains));
        self.acknowledged_geometry
            .retain(|report| report.window.is_none_or(contains));
    }

    /// Update preview intent without rebuilding target geometry.
    pub fn set_hover(&mut self, hovered: Option<NodeId>, minimized_windows: &[MinimizedWindow]) {
        let next = preview_token(hovered, minimized_windows);
        if next != self.desired_preview {
            self.desired_preview = next;
            self.preview_anchor = DockItemGeometry::default();
            // Preview enter waits for a frame containing the hover transform;
            // leave still dispatches immediately from model-only input.
            self.preview_waiting_for_scene = next.is_some();
        }
    }

    fn set_hover_from_scene(
        &mut self,
        hovered: Option<NodeId>,
        minimized_windows: &[MinimizedWindow],
    ) {
        let next = preview_token(hovered, minimized_windows).filter(|token| {
            self.interactive_geometry
                .iter()
                .any(|report| report.window == Some(*token))
        });
        self.preview_anchor =
            next.map_or_else(DockItemGeometry::default, |token| self.geometry_for(token));
        if next != self.desired_preview || self.preview_waiting_for_scene {
            self.desired_preview = next;
            self.preview_waiting_for_scene = false;
        }
    }

    /// Request preview teardown while retaining acknowledgement until sent.
    pub fn clear_preview(&mut self) {
        self.desired_preview = None;
        self.preview_anchor = DockItemGeometry::default();
        self.preview_waiting_for_scene = false;
    }

    /// Return ordered work: withdrawals, geometry updates, preview leave, enter.
    #[must_use]
    pub fn pending_actions(&self, now: Instant) -> Vec<UserAction> {
        if self.retry_not_before.is_some_and(|deadline| now < deadline) {
            return Vec::new();
        }
        self.pending_actions_unthrottled(now)
    }

    /// Mark one returned action as successfully handed to the WM channel.
    pub fn acknowledge(&mut self, action: UserAction, now: Instant) -> bool {
        let Some((session, _)) = self.state_key else {
            return false;
        };
        let acknowledged = match action {
            UserAction::SetDockGeometry {
                window,
                wm_session_id,
                minimized_generation,
                geometry,
            } if wm_session_id == session && minimized_generation == self.minimized_generation => {
                if let Some(window) = window {
                    self.pending_geometry_withdrawals
                        .retain(|pending| *pending != window);
                }
                self.acknowledged_geometry
                    .retain(|report| report.window != window);
                if !geometry.is_empty() {
                    self.acknowledged_geometry
                        .push(DockGeometryReport { window, geometry });
                }
                true
            }
            UserAction::PreviewWindow {
                window,
                wm_session_id,
                minimized_generation,
                visible,
                renewal: _,
                geometry,
            } if wm_session_id == session && minimized_generation == self.minimized_generation => {
                if visible {
                    self.acknowledged_preview = Some(window);
                    self.acknowledged_preview_anchor = geometry;
                    self.preview_last_sent = Some(now);
                } else if self.acknowledged_preview == Some(window) {
                    self.acknowledged_preview = None;
                    self.acknowledged_preview_anchor = DockItemGeometry::default();
                    self.preview_last_sent = None;
                }
                true
            }
            _ => false,
        };
        if acknowledged {
            self.retry_not_before = None;
        }
        acknowledged
    }

    /// Keep unacknowledged work pending and rate-limit pointer-storm retries.
    pub fn record_failure(&mut self, now: Instant) {
        self.retry_not_before = now.checked_add(DOCK_RETRY_INTERVAL).or(Some(now));
    }

    /// Earliest retry or preview-renewal deadline, if work remains.
    #[must_use]
    pub fn next_retry_deadline(&self, now: Instant) -> Option<Instant> {
        self.state_key?;
        if !self.pending_actions_unthrottled(now).is_empty() {
            return Some(
                self.retry_not_before
                    .map_or(now, |deadline| deadline.max(now)),
            );
        }
        if self.desired_preview.is_some() && self.desired_preview == self.acknowledged_preview {
            return self.preview_last_sent.map(|sent| {
                let lease = sent
                    .checked_add(DOCK_PREVIEW_LEASE_INTERVAL)
                    .unwrap_or(sent);
                if !self.preview_anchor.is_empty()
                    && self.preview_anchor != self.acknowledged_preview_anchor
                {
                    sent.checked_add(DOCK_PREVIEW_ANCHOR_INTERVAL)
                        .unwrap_or(sent)
                        .min(lease)
                } else {
                    lease
                }
            });
        }
        None
    }

    #[must_use]
    pub fn geometry_for(&self, window: WindowToken) -> DockItemGeometry {
        self.interactive_geometry
            .iter()
            .find(|report| report.window == Some(window))
            .or_else(|| {
                self.desired_geometry
                    .iter()
                    .find(|report| report.window == Some(window))
            })
            .map_or_else(DockItemGeometry::default, |report| report.geometry)
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    #[must_use]
    pub const fn state_key(&self) -> Option<(u64, u64)> {
        self.state_key
    }

    #[must_use]
    pub const fn minimized_generation(&self) -> u64 {
        self.minimized_generation
    }

    #[must_use]
    pub const fn desired_preview(&self) -> Option<WindowToken> {
        self.desired_preview
    }

    #[must_use]
    pub const fn acknowledged_preview(&self) -> Option<WindowToken> {
        self.acknowledged_preview
    }

    #[must_use]
    pub const fn preview_last_sent(&self) -> Option<Instant> {
        self.preview_last_sent
    }

    #[must_use]
    pub fn desired_geometry(&self) -> &[DockGeometryReport] {
        &self.desired_geometry
    }

    /// Current transformed item rectangles from the last rendered scene.
    #[must_use]
    pub fn interactive_geometry(&self) -> &[DockGeometryReport] {
        &self.interactive_geometry
    }

    #[must_use]
    pub fn acknowledged_geometry(&self) -> &[DockGeometryReport] {
        &self.acknowledged_geometry
    }

    fn pending_actions_unthrottled(&self, now: Instant) -> Vec<UserAction> {
        let Some((wm_session_id, _)) = self.state_key else {
            return Vec::new();
        };
        let mut actions = Vec::new();

        for &window in &self.pending_geometry_withdrawals {
            actions.push(UserAction::SetDockGeometry {
                window: Some(window),
                wm_session_id,
                minimized_generation: self.minimized_generation,
                geometry: DockItemGeometry::default(),
            });
        }
        for report in self.acknowledged_geometry.iter().filter(|report| {
            !self
                .desired_geometry
                .iter()
                .any(|desired| desired.window == report.window)
        }) {
            actions.push(UserAction::SetDockGeometry {
                window: report.window,
                wm_session_id,
                minimized_generation: self.minimized_generation,
                geometry: DockItemGeometry::default(),
            });
        }
        for report in self
            .desired_geometry
            .iter()
            .filter(|report| !self.acknowledged_geometry.contains(report))
        {
            actions.push(UserAction::SetDockGeometry {
                window: report.window,
                wm_session_id,
                minimized_generation: self.minimized_generation,
                geometry: report.geometry,
            });
        }

        if let Some(current) = self.acknowledged_preview
            && self.desired_preview != Some(current)
        {
            actions.push(UserAction::PreviewWindow {
                window: current,
                wm_session_id,
                minimized_generation: self.minimized_generation,
                visible: false,
                renewal: false,
                geometry: self.acknowledged_preview_anchor,
            });
        }
        let same_preview = self.desired_preview == self.acknowledged_preview
            && self.desired_preview.is_some()
            && !self.preview_anchor.is_empty();
        let renew = same_preview
            && self.preview_last_sent.is_none_or(|sent| {
                now.saturating_duration_since(sent) >= DOCK_PREVIEW_LEASE_INTERVAL
            });
        let anchor_refresh = same_preview
            && self.preview_anchor != self.acknowledged_preview_anchor
            && self.preview_last_sent.is_none_or(|sent| {
                now.saturating_duration_since(sent) >= DOCK_PREVIEW_ANCHOR_INTERVAL
            });
        if let Some(desired) = self.desired_preview
            && !self.preview_waiting_for_scene
            && !self.preview_anchor.is_empty()
            && (self.acknowledged_preview != Some(desired) || renew || anchor_refresh)
        {
            actions.push(UserAction::PreviewWindow {
                window: desired,
                wm_session_id,
                minimized_generation: self.minimized_generation,
                visible: true,
                // Both the lease heartbeat and a throttled anchor refresh
                // extend the same compositor-side ownership lease.
                renewal: self.acknowledged_preview == Some(desired),
                geometry: self.preview_anchor,
            });
        }
        actions
    }
}

/// Derive stable shelf/item targets from a shared scene.
#[must_use]
pub fn dock_geometry_reports(
    scene: &Scene,
    config: &PresentationConfig,
    monitor: MonitorGeometry,
    scale_factor: f64,
) -> Vec<DockGeometryReport> {
    let scale = if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor
    } else {
        1.0
    };
    let Some(shelf) = scene.bounds_for(NodeId::DockShelf) else {
        return Vec::new();
    };
    let mut reports = vec![DockGeometryReport {
        window: None,
        geometry: global_physical_geometry(shelf, monitor, scale),
    }];

    let padding = finite_non_negative(config.dock_shelf_padding);
    let usable_height = (shelf.height - padding * 2.0).max(0.0);
    let base_height = finite_non_negative(config.dock_item_size).min(usable_height);
    if base_height < 1.0 {
        return reports;
    }
    let aspect = if config.dock_item_aspect_ratio.is_finite() {
        config.dock_item_aspect_ratio.max(1.0)
    } else {
        1.0
    };
    let base_width = base_height * aspect;
    let hover_scale = if config.dock_hover_scale.is_finite() {
        config.dock_hover_scale.max(1.0)
    } else {
        1.0
    }
    .min((usable_height / base_height).max(1.0));
    let slot_width = base_width * hover_scale;
    let slot_pitch = slot_width + finite_non_negative(config.dock_item_gap);
    let slot_start = shelf.x + padding * 2.0 + finite_non_negative(config.dock_separator_width);
    for (slot, token) in scene
        .hits
        .iter()
        .filter_map(|hit| match hit.id {
            NodeId::MinimizedWindow(token) => Some(token),
            _ => None,
        })
        .enumerate()
    {
        let stable = Rect::new(
            slot_start + slot as f32 * slot_pitch + (slot_width - base_width) * 0.5,
            shelf.y + (shelf.height - base_height) * 0.5,
            base_width,
            base_height,
        );
        reports.push(DockGeometryReport {
            window: Some(token),
            geometry: global_physical_geometry(stable, monitor, scale),
        });
    }
    reports
}

/// Derive the current transformed item rectangles used by preview/restore.
///
/// Unlike [`dock_geometry_reports`], these bounds follow hover magnification
/// and are never published as persistent compositor targets.
#[must_use]
pub fn interactive_dock_geometry_reports(
    scene: &Scene,
    monitor: MonitorGeometry,
    scale_factor: f64,
) -> Vec<DockGeometryReport> {
    scene
        .hits
        .iter()
        .filter_map(|hit| match hit.id {
            NodeId::MinimizedWindow(token) => Some(DockGeometryReport {
                window: Some(token),
                geometry: global_physical_geometry(hit.bounds, monitor, scale_factor),
            }),
            _ => None,
        })
        .collect()
}

/// Convert one logical bar rectangle to global physical pixels.
#[must_use]
pub fn global_physical_geometry(
    bounds: Rect,
    monitor: MonitorGeometry,
    scale_factor: f64,
) -> DockItemGeometry {
    let scale = if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor
    } else {
        1.0
    };
    let x = monitor
        .x
        .saturating_add((f64::from(bounds.x) * scale).round() as i32);
    let y = monitor
        .y
        .saturating_add((f64::from(bounds.y) * scale).round() as i32);
    let width = (f64::from(bounds.width.max(0.0)) * scale)
        .round()
        .clamp(0.0, f64::from(u32::MAX)) as u32;
    let height = (f64::from(bounds.height.max(0.0)) * scale)
        .round()
        .clamp(0.0, f64::from(u32::MAX)) as u32;
    DockItemGeometry::new(x, y, width, height)
}

fn finite_non_negative(value: f32) -> f32 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

fn preview_token(
    hovered: Option<NodeId>,
    minimized_windows: &[MinimizedWindow],
) -> Option<WindowToken> {
    match hovered {
        Some(NodeId::MinimizedWindow(token))
            if minimized_windows
                .iter()
                .any(|window| window.token == token && window.preview_available()) =>
        {
            Some(token)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MINIMIZED_WINDOW_FLAG_PREVIEW_AVAILABLE, MonitorId};
    use crate::presentation::{HitRegion, Rgba, SceneNode, Size, VisualState};

    fn minimized(token: u64) -> MinimizedWindow {
        MinimizedWindow {
            token: WindowToken(token),
            monitor: MonitorId(4),
            title: "Terminal".to_owned(),
            app_id: "foot".to_owned(),
            flags: MINIMIZED_WINDOW_FLAG_PREVIEW_AVAILABLE,
        }
    }

    fn dock_scene(item_bounds: Option<Rect>) -> Scene {
        let shelf = Rect::new(100.0, 0.0, 100.0, 38.0);
        let mut scene = Scene {
            viewport: Size::new(200.0, 38.0),
            clip: Rect::new(0.0, 0.0, 200.0, 38.0),
            nodes: vec![SceneNode::RoundedRect {
                id: NodeId::DockShelf,
                bounds: shelf,
                radius: 6.0,
                fill: Rgba::new(0.0, 0.0, 0.0, 0.4),
                stroke: None,
                state: VisualState::default(),
            }],
            hits: Vec::new(),
        };
        if let Some(bounds) = item_bounds {
            scene.hits.push(HitRegion {
                id: NodeId::MinimizedWindow(WindowToken(41)),
                bounds,
                primary: None,
                secondary: None,
                scroll_up: None,
                scroll_down: None,
            });
        }
        scene
    }

    #[test]
    fn reporter_converts_stable_slots_and_renews_preview_lease() {
        let scene = dock_scene(Some(Rect::new(105.0, 5.05, 41.85, 27.9)));
        let config = PresentationConfig::default();
        let windows = [minimized(41)];
        let monitor = MonitorGeometry {
            x: 300,
            y: 400,
            width: 1600,
            height: 900,
        };
        let mut reporter = DockReporter::new();
        reporter.synchronize(DockReporterInput {
            wm_available: true,
            wm_session_id: 91,
            minimized_generation: 7,
            transport_generation: 3,
            monitor_geometry: Some(monitor),
            minimized_windows: &windows,
            scene: &scene,
            presentation: &config,
            hovered: Some(NodeId::MinimizedWindow(WindowToken(41))),
            scale_factor: 2.0,
        });

        let start = Instant::now();
        let actions = reporter.pending_actions(start);
        assert_eq!(actions.len(), 3, "shelf, item, then preview enter");
        assert!(actions.iter().all(|action| match action {
            UserAction::SetDockGeometry { wm_session_id, .. }
            | UserAction::PreviewWindow { wm_session_id, .. } => *wm_session_id == 91,
            _ => false,
        }));
        let stable = reporter
            .desired_geometry()
            .iter()
            .find(|report| report.window == Some(WindowToken(41)))
            .expect("stable item target");
        assert_eq!(stable.geometry, DockItemGeometry::new(525, 420, 54, 36));
        assert_eq!(
            reporter.geometry_for(WindowToken(41)),
            DockItemGeometry::new(510, 410, 84, 56),
            "preview/restore follows the transformed scene card"
        );
        assert!(actions.iter().any(|action| matches!(
            action,
            UserAction::PreviewWindow {
                renewal: false,
                geometry: DockItemGeometry {
                    width: 84,
                    height: 56,
                    ..
                },
                ..
            }
        )));
        for action in actions {
            assert!(reporter.acknowledge(action, start));
        }
        assert_eq!(
            reporter.next_retry_deadline(start),
            start.checked_add(DOCK_PREVIEW_LEASE_INTERVAL)
        );
        let moved_scene = dock_scene(Some(Rect::new(120.0, 4.0, 45.0, 30.0)));
        reporter.synchronize(DockReporterInput {
            wm_available: true,
            wm_session_id: 91,
            minimized_generation: 7,
            transport_generation: 3,
            monitor_geometry: Some(monitor),
            minimized_windows: &windows,
            scene: &moved_scene,
            presentation: &config,
            hovered: Some(NodeId::MinimizedWindow(WindowToken(41))),
            scale_factor: 2.0,
        });
        assert_eq!(
            reporter.geometry_for(WindowToken(41)),
            DockItemGeometry::new(540, 408, 90, 60)
        );
        assert_eq!(
            reporter.pending_actions(start + Duration::from_millis(1)),
            Vec::new(),
            "same-token motion must not emit a per-frame preview command"
        );
        assert_eq!(
            reporter.next_retry_deadline(start + Duration::from_millis(1)),
            start.checked_add(DOCK_PREVIEW_ANCHOR_INTERVAL)
        );
        let anchor_refresh = reporter
            .pending_actions(start + DOCK_PREVIEW_ANCHOR_INTERVAL)
            .into_iter()
            .find(|action| {
                matches!(
                    action,
                    UserAction::PreviewWindow {
                        window: WindowToken(41),
                        visible: true,
                        ..
                    }
                )
            })
            .expect("throttled preview anchor refresh");
        assert!(matches!(
            anchor_refresh,
            UserAction::PreviewWindow {
                renewal: true,
                geometry: DockItemGeometry {
                    x: 540,
                    y: 408,
                    width: 90,
                    height: 60,
                },
                ..
            }
        ));
        assert!(reporter.acknowledge(anchor_refresh, start + DOCK_PREVIEW_ANCHOR_INTERVAL));
        assert_eq!(
            reporter.next_retry_deadline(start + DOCK_PREVIEW_ANCHOR_INTERVAL),
            start
                .checked_add(DOCK_PREVIEW_ANCHOR_INTERVAL)
                .and_then(|sent| sent.checked_add(DOCK_PREVIEW_LEASE_INTERVAL))
        );
    }

    #[test]
    fn generation_bump_withdraws_a_removed_acknowledged_target_with_the_new_epoch() {
        let scene = dock_scene(Some(Rect::new(112.0, 10.0, 27.0, 18.0)));
        let empty_scene = dock_scene(None);
        let config = PresentationConfig::default();
        let windows = [minimized(41)];
        let monitor = MonitorGeometry {
            x: 0,
            y: 0,
            width: 1600,
            height: 900,
        };
        let now = Instant::now();
        let mut reporter = DockReporter::new();
        reporter.synchronize(DockReporterInput {
            wm_available: true,
            wm_session_id: 91,
            minimized_generation: 7,
            transport_generation: 3,
            monitor_geometry: Some(monitor),
            minimized_windows: &windows,
            scene: &scene,
            presentation: &config,
            hovered: None,
            scale_factor: 1.0,
        });
        for action in reporter.pending_actions(now) {
            assert!(reporter.acknowledge(action, now));
        }

        reporter.synchronize(DockReporterInput {
            wm_available: true,
            wm_session_id: 91,
            minimized_generation: 8,
            transport_generation: 3,
            monitor_geometry: Some(monitor),
            minimized_windows: &[],
            scene: &empty_scene,
            presentation: &config,
            hovered: None,
            scale_factor: 1.0,
        });
        let withdrawal = reporter
            .pending_actions(now)
            .into_iter()
            .find(|action| {
                matches!(
                    action,
                    UserAction::SetDockGeometry {
                        window: Some(WindowToken(41)),
                        minimized_generation: 8,
                        geometry,
                        ..
                    } if geometry.is_empty()
                )
            })
            .expect("old target withdrawal in the current projection");

        let mut reconnected = reporter.clone();
        reconnected.synchronize_model(true, 91, 8, 4, &[], None);
        assert!(
            reconnected
                .pending_actions(now)
                .iter()
                .any(|action| matches!(
                    action,
                    UserAction::SetDockGeometry {
                        window: Some(WindowToken(41)),
                        minimized_generation: 8,
                        geometry,
                        ..
                    } if geometry.is_empty()
                ))
        );

        let mut restarted = reporter.clone();
        restarted.synchronize_model(true, 92, 9, 3, &[], None);
        assert!(restarted.pending_geometry_withdrawals.is_empty());
        assert!(restarted.pending_actions(now).is_empty());

        assert!(reporter.acknowledge(withdrawal, now));
        assert!(!reporter.pending_actions(now).iter().any(|action| matches!(
            action,
            UserAction::SetDockGeometry {
                window: Some(WindowToken(41)),
                ..
            }
        )));
    }

    #[test]
    fn generation_bump_never_withdraws_a_token_present_in_the_new_projection() {
        let scene = dock_scene(Some(Rect::new(112.0, 10.0, 27.0, 18.0)));
        let config = PresentationConfig::default();
        let windows = [minimized(41)];
        let monitor = MonitorGeometry {
            x: 0,
            y: 0,
            width: 1600,
            height: 900,
        };
        let now = Instant::now();
        let mut reporter = DockReporter::new();
        reporter.synchronize(DockReporterInput {
            wm_available: true,
            wm_session_id: 91,
            minimized_generation: 7,
            transport_generation: 3,
            monitor_geometry: Some(monitor),
            minimized_windows: &windows,
            scene: &scene,
            presentation: &config,
            hovered: None,
            scale_factor: 1.0,
        });
        for action in reporter.pending_actions(now) {
            assert!(reporter.acknowledge(action, now));
        }

        reporter.synchronize(DockReporterInput {
            wm_available: true,
            wm_session_id: 91,
            minimized_generation: 8,
            transport_generation: 3,
            monitor_geometry: Some(monitor),
            minimized_windows: &windows,
            scene: &scene,
            presentation: &config,
            hovered: None,
            scale_factor: 1.0,
        });
        let actions = reporter.pending_actions(now);
        assert!(!actions.iter().any(|action| matches!(
            action,
            UserAction::SetDockGeometry {
                window: Some(WindowToken(41)),
                geometry,
                ..
            } if geometry.is_empty()
        )));
        assert!(actions.iter().any(|action| matches!(
            action,
            UserAction::SetDockGeometry {
                window: Some(WindowToken(41)),
                minimized_generation: 8,
                geometry,
                ..
            } if !geometry.is_empty()
        )));
    }

    #[test]
    fn reporter_withdraws_hidden_targets_retries_and_reemits_after_session_change() {
        let scene = dock_scene(Some(Rect::new(112.0, 10.0, 27.0, 18.0)));
        let narrow_scene = dock_scene(None);
        let config = PresentationConfig::default();
        let windows = [minimized(41)];
        let monitor = MonitorGeometry {
            x: -1600,
            y: 20,
            width: 1600,
            height: 900,
        };
        let mut reporter = DockReporter::new();
        let start = Instant::now();
        reporter.synchronize(DockReporterInput {
            wm_available: true,
            wm_session_id: 91,
            minimized_generation: 7,
            transport_generation: 3,
            monitor_geometry: Some(monitor),
            minimized_windows: &windows,
            scene: &scene,
            presentation: &config,
            hovered: None,
            scale_factor: 1.0,
        });
        for action in reporter.pending_actions(start) {
            assert!(reporter.acknowledge(action, start));
        }

        reporter.synchronize_model(true, 91, 7, 4, &windows, None);
        assert_eq!(reporter.desired_geometry().len(), 2);
        assert_eq!(reporter.interactive_geometry().len(), 1);
        let reconnected = reporter.pending_actions(start);
        assert_eq!(
            reconnected.len(),
            2,
            "same-session reconnect replays targets"
        );
        for action in reconnected {
            assert!(reporter.acknowledge(action, start));
        }

        reporter.synchronize(DockReporterInput {
            wm_available: true,
            wm_session_id: 91,
            minimized_generation: 7,
            transport_generation: 4,
            monitor_geometry: Some(monitor),
            minimized_windows: &windows,
            scene: &narrow_scene,
            presentation: &config,
            hovered: None,
            scale_factor: 1.0,
        });
        let withdrawal = reporter.pending_actions(start);
        assert!(withdrawal.iter().any(|action| matches!(
            action,
            UserAction::SetDockGeometry {
                window: Some(WindowToken(41)),
                geometry,
                ..
            } if geometry.is_empty()
        )));
        for action in withdrawal {
            assert!(reporter.acknowledge(action, start));
        }

        reporter.synchronize_model(
            true,
            92,
            8,
            5,
            &windows,
            Some(NodeId::MinimizedWindow(WindowToken(41))),
        );
        assert_eq!(reporter.state_key(), Some((92, 5)));
        assert!(reporter.desired_geometry().is_empty());
        assert!(reporter.interactive_geometry().is_empty());
        assert!(reporter.desired_preview().is_none());
        assert!(
            reporter.pending_actions(start).is_empty(),
            "a reused token cannot bind geometry or hover from the old session"
        );

        reporter.synchronize(DockReporterInput {
            wm_available: true,
            wm_session_id: 92,
            minimized_generation: 8,
            transport_generation: 5,
            monitor_geometry: Some(monitor),
            minimized_windows: &windows,
            scene: &scene,
            presentation: &config,
            hovered: None,
            scale_factor: 1.0,
        });
        assert_eq!(reporter.pending_actions(start).len(), 2);

        reporter.record_failure(start);
        assert!(reporter.pending_actions(start).is_empty());
        assert_eq!(
            reporter.next_retry_deadline(start),
            start.checked_add(DOCK_RETRY_INTERVAL)
        );
        assert_eq!(
            reporter.pending_actions(start + DOCK_RETRY_INTERVAL).len(),
            2
        );

        reporter.synchronize_model(false, 92, 8, 5, &windows, None);
        assert!(reporter.state_key().is_none());
        assert!(reporter.desired_geometry().is_empty());
        assert!(reporter.acknowledged_geometry().is_empty());
        assert!(reporter.desired_preview().is_none());
        assert!(reporter.acknowledged_preview().is_none());
        assert!(reporter.next_retry_deadline(start).is_none());
    }
}
