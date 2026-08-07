//! Toolkit-neutral adapter for custom retained-mode minimized-window Docks.
//!
//! [`DockBridge`] keeps session validation, transport backpressure, preview
//! leases, and geometry reporting identical while each frontend remains free
//! to paint the resulting shelf with its native toolkit.

use std::time::{Duration, Instant};

use crate::presentation::{
    HitRegion, NodeId, PresentationConfig, Rect, Rgba, Scene, SceneNode, Size, VisualState,
};
use crate::{
    BarSnapshot, DockItemGeometry, DockReporter, DockReporterInput, MinimizedWindow, UserAction,
    WindowToken, global_physical_geometry,
};

/// Resting width of one minimized-window card in logical pixels.
pub const DOCK_ITEM_WIDTH: f32 = 30.0;
/// Resting height of one minimized-window card in logical pixels.
pub const DOCK_ITEM_HEIGHT: f32 = 20.0;
/// Scale applied to the card directly under the pointer.
pub const DOCK_HOVER_SCALE: f32 = 1.55;
/// Scale applied to the first card on either side of the hovered card.
pub const DOCK_NEIGHBOUR_SCALE: f32 = 1.25;
/// Scale applied to the second card on either side of the hovered card.
pub const DOCK_SECOND_NEIGHBOUR_SCALE: f32 = 1.08;
/// Reserved logical width of each card slot at maximum magnification.
pub const DOCK_SLOT_WIDTH: f32 = DOCK_ITEM_WIDTH * DOCK_HOVER_SCALE;
/// Gap between adjacent card slots in logical pixels.
pub const DOCK_ITEM_GAP: f32 = 4.0;
/// Inset around the minimized-window shelf in logical pixels.
pub const DOCK_SHELF_PADDING: f32 = 2.0;
/// Width of the separator before the minimized-window shelf.
pub const DOCK_SEPARATOR_WIDTH: f32 = 1.0;
/// Width reserved for the overflow indicator.
pub const DOCK_OVERFLOW_WIDTH: f32 = 12.0;
const MAX_VIEWPORT_FRACTION: f32 = 0.45;

#[derive(Debug, Clone, Copy)]
struct DockLayout {
    monitor: crate::MonitorGeometry,
    scale_factor: f64,
    shelf: Rect,
}

/// Protocol/reporting state used by custom retained toolkits.
#[derive(Debug, Default)]
pub struct DockBridge {
    reporter: DockReporter,
    state_key: Option<(u64, u64)>,
    windows: Vec<MinimizedWindow>,
    visible_tokens: Vec<WindowToken>,
    hovered: Option<WindowToken>,
    layout: Option<DockLayout>,
    overflow: bool,
    collapsed: bool,
    pending_withdrawals: Vec<UserAction>,
    pending_restore: Option<UserAction>,
    retry_not_before: Option<Instant>,
}

impl DockBridge {
    /// Creates an empty bridge with no active WM session.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reconcile one owned model snapshot with a toolkit's fixed right-edge
    /// allocation. `right_padding` is logical; the origin always comes from
    /// the WM snapshot and never from a toolkit window-position query.
    pub fn synchronize(
        &mut self,
        snapshot: &BarSnapshot,
        transport_generation: u64,
        scale_factor: f64,
        bar_height: f32,
        right_padding: f32,
    ) {
        let next_key = snapshot
            .wm_available
            .then_some((snapshot.wm_session_id, transport_generation));
        let previous_session = self.session();
        let continuing_session =
            previous_session == next_key.map(|(session, _)| session) && previous_session.is_some();
        if continuing_session {
            self.pending_withdrawals.retain(|action| match action {
                UserAction::SetDockGeometry {
                    window: Some(token),
                    wm_session_id,
                    ..
                } => {
                    *wm_session_id == snapshot.wm_session_id
                        && !snapshot
                            .minimized_windows
                            .iter()
                            .any(|window| window.token == *token)
                }
                _ => false,
            });
            for token in self
                .reporter
                .acknowledged_geometry()
                .iter()
                .filter_map(|report| report.window)
                .filter(|token| {
                    !snapshot
                        .minimized_windows
                        .iter()
                        .any(|window| window.token == *token)
                })
            {
                let withdrawal = UserAction::SetDockGeometry {
                    window: Some(token),
                    wm_session_id: snapshot.wm_session_id,
                    geometry: DockItemGeometry::default(),
                };
                if !self.pending_withdrawals.contains(&withdrawal) {
                    self.pending_withdrawals.push(withdrawal);
                }
            }
        } else {
            self.pending_withdrawals.clear();
        }
        if self.state_key != next_key {
            self.hovered = None;
            self.pending_restore = None;
            self.retry_not_before = None;
            self.state_key = next_key;
        }
        self.windows.clone_from(&snapshot.minimized_windows);
        self.overflow = snapshot.minimized_overflow;

        let scale = sane_scale(scale_factor);
        let logical_width = snapshot
            .geometry
            .map(|geometry| geometry.width as f32 / scale as f32)
            .unwrap_or(0.0);
        let available_width = if snapshot.wm_available && snapshot.geometry.is_some() {
            (logical_width * MAX_VIEWPORT_FRACTION - right_padding).max(0.0)
        } else {
            0.0
        };
        let total = self.windows.len();
        let mut visible_count = total;
        while visible_count > 0
            && shelf_width(
                visible_count,
                snapshot.minimized_overflow || visible_count < total,
            ) > available_width
        {
            visible_count -= 1;
        }
        self.collapsed = snapshot.wm_available && total > 0 && visible_count == 0;
        self.overflow = snapshot.minimized_overflow || visible_count < total;
        // The WM list is stable oldest-to-newest. Retain its newest tail when
        // a narrow bar cannot display every card, without disturbing order.
        self.visible_tokens = self.windows[total.saturating_sub(visible_count)..]
            .iter()
            .map(|window| window.token)
            .collect();
        if self
            .hovered
            .is_some_and(|token| !self.visible_tokens.contains(&token))
        {
            self.hovered = None;
        }

        let shelf_width = shelf_width(self.visible_tokens.len(), self.overflow || self.collapsed);
        let shelf = Rect::new(
            (logical_width - right_padding - shelf_width).max(0.0),
            0.0,
            shelf_width,
            bar_height.max(DOCK_ITEM_HEIGHT + DOCK_SHELF_PADDING * 2.0),
        );
        self.layout = snapshot.geometry.map(|monitor| DockLayout {
            monitor,
            scale_factor: scale,
            shelf,
        });

        let scene = dock_scene(shelf, &self.visible_tokens);
        let presentation = dock_presentation();
        self.reporter.synchronize(DockReporterInput {
            wm_available: snapshot.wm_available,
            wm_session_id: snapshot.wm_session_id,
            transport_generation,
            monitor_geometry: snapshot.geometry,
            minimized_windows: &self.windows,
            scene: &scene,
            presentation: &presentation,
            hovered: self.hovered.map(NodeId::MinimizedWindow),
            scale_factor: scale,
        });
    }

    /// Iterates over the minimized windows that fit in the current shelf.
    pub fn visible_windows(&self) -> impl Iterator<Item = &MinimizedWindow> {
        self.windows
            .iter()
            .filter(|window| self.visible_tokens.contains(&window.token))
    }

    /// Returns the minimized window currently hovered by the pointer.
    #[must_use]
    pub const fn hovered(&self) -> Option<WindowToken> {
        self.hovered
    }

    /// Reports whether the shelf must display an overflow affordance.
    #[must_use]
    pub const fn overflow(&self) -> bool {
        self.overflow
    }

    /// Reports whether minimized windows exist but none fit in the shelf.
    #[must_use]
    pub const fn collapsed(&self) -> bool {
        self.collapsed
    }

    /// Returns the logical width required by the current shelf.
    #[must_use]
    pub fn shelf_width(&self) -> f32 {
        shelf_width(self.visible_tokens.len(), self.overflow || self.collapsed)
    }

    /// Returns the macOS-style neighbour magnification for `token`.
    #[must_use]
    pub fn scale_for(&self, token: WindowToken) -> f32 {
        let Some(hovered) = self.hovered else {
            return 1.0;
        };
        let Some(index) = self.visible_tokens.iter().position(|item| *item == token) else {
            return 1.0;
        };
        let Some(hover_index) = self.visible_tokens.iter().position(|item| *item == hovered) else {
            return 1.0;
        };
        match index.abs_diff(hover_index) {
            0 => DOCK_HOVER_SCALE,
            1 => DOCK_NEIGHBOUR_SCALE,
            2 => DOCK_SECOND_NEIGHBOUR_SCALE,
            _ => 1.0,
        }
    }

    /// Session and token are captured by the rendered event closure. A stale
    /// DOM/retained node can therefore never silently rebind after a WM restart.
    pub fn enter(&mut self, token: WindowToken, wm_session_id: u64) -> bool {
        if self.session() != Some(wm_session_id)
            || !self.visible_tokens.contains(&token)
            || !self
                .windows
                .iter()
                .any(|window| window.token == token && window.preview_available())
        {
            return false;
        }
        self.hovered = Some(token);
        self.reporter
            .set_hover(Some(NodeId::MinimizedWindow(token)), &self.windows);
        true
    }

    /// Clears hover only if `token` still owns it in the captured WM session.
    pub fn leave(&mut self, token: WindowToken, wm_session_id: u64) -> bool {
        if self.session() != Some(wm_session_id) || self.hovered != Some(token) {
            return false;
        }
        self.hovered = None;
        self.reporter.set_hover(None, &self.windows);
        true
    }

    /// Clears any hover owned by the captured WM session.
    pub fn leave_shelf(&mut self, wm_session_id: u64) -> bool {
        if self.session() != Some(wm_session_id) || self.hovered.is_none() {
            return false;
        }
        self.hovered = None;
        self.reporter.set_hover(None, &self.windows);
        true
    }

    /// Restore carries the magnified visual rectangle seen by the user, while
    /// normal geometry reports remain the untransformed 30x20 resting slots.
    #[must_use]
    pub fn restore_action(&self, token: WindowToken, wm_session_id: u64) -> Option<UserAction> {
        if self.session() != Some(wm_session_id) || !self.visible_tokens.contains(&token) {
            return None;
        }
        Some(UserAction::RestoreWindow {
            window: token,
            wm_session_id,
            geometry: self.visual_geometry(token),
        })
    }

    /// Queue a restore so transient queue pressure or a reconnect cannot eat
    /// the user's click. Only one click is retained, bounding pointer storms.
    pub fn request_restore(&mut self, token: WindowToken, wm_session_id: u64) -> bool {
        let Some(action) = self.restore_action(token, wm_session_id) else {
            return false;
        };
        self.hovered = None;
        self.reporter.set_hover(None, &self.windows);
        self.pending_restore = Some(action);
        self.retry_not_before = None;
        true
    }

    /// Ordered, rate-limited work. Preview anchors are upgraded from stable
    /// slots to the currently magnified visual card.
    #[must_use]
    pub fn pending_actions(&self, now: Instant) -> Vec<UserAction> {
        let local_retry_ready = self.retry_not_before.is_none_or(|deadline| now >= deadline);
        let mut actions = if local_retry_ready {
            self.pending_withdrawals.clone()
        } else {
            Vec::new()
        };
        let reported: Vec<_> = self
            .reporter
            .pending_actions(now)
            .into_iter()
            .map(|action| match action {
                UserAction::PreviewWindow {
                    window,
                    wm_session_id,
                    visible,
                    ..
                } => UserAction::PreviewWindow {
                    window,
                    wm_session_id,
                    visible,
                    geometry: self.visual_geometry(window),
                },
                action => action,
            })
            .collect();
        for action in reported {
            if !actions.contains(&action) {
                actions.push(action);
            }
        }
        if local_retry_ready && let Some(restore) = self.pending_restore {
            actions.push(restore);
        }
        actions
    }

    /// Marks an action as delivered and advances the reporter state.
    pub fn acknowledge(&mut self, action: UserAction, now: Instant) {
        if matches!(action, UserAction::RestoreWindow { .. }) {
            if self.pending_restore == Some(action) {
                self.pending_restore = None;
            }
            self.retry_not_before = None;
        } else {
            self.pending_withdrawals
                .retain(|withdrawal| *withdrawal != action);
            let _ = self.reporter.acknowledge(action, now);
            self.retry_not_before = None;
        }
    }

    /// Records transport backpressure and starts the bounded retry delay.
    pub fn record_failure(&mut self, now: Instant) {
        self.reporter.record_failure(now);
        self.retry_not_before = now.checked_add(Duration::from_millis(100)).or(Some(now));
    }

    /// Returns the earliest deadline at which pending work should be retried.
    #[must_use]
    pub fn next_retry_deadline(&self, now: Instant) -> Option<Instant> {
        match (
            self.reporter.next_retry_deadline(now),
            (!self.pending_withdrawals.is_empty() || self.pending_restore.is_some()).then(|| {
                self.retry_not_before
                    .map_or(now, |deadline| deadline.max(now))
            }),
        ) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (left, right) => left.or(right),
        }
    }

    #[must_use]
    fn session(&self) -> Option<u64> {
        self.state_key.map(|(session, _)| session)
    }

    #[must_use]
    fn visual_geometry(&self, token: WindowToken) -> DockItemGeometry {
        let Some(layout) = self.layout else {
            return DockItemGeometry::default();
        };
        let Some(index) = self.visible_tokens.iter().position(|item| *item == token) else {
            return DockItemGeometry::default();
        };
        let scale = self.scale_for(token);
        let base_x = layout.shelf.x
            + DOCK_SHELF_PADDING * 2.0
            + DOCK_SEPARATOR_WIDTH
            + index as f32 * (DOCK_SLOT_WIDTH + DOCK_ITEM_GAP)
            + (DOCK_SLOT_WIDTH - DOCK_ITEM_WIDTH) * 0.5;
        let base_y = layout.shelf.y + (layout.shelf.height - DOCK_ITEM_HEIGHT) * 0.5;
        let width = DOCK_ITEM_WIDTH * scale;
        let height = DOCK_ITEM_HEIGHT * scale;
        let visual = Rect::new(
            base_x + (DOCK_ITEM_WIDTH - width) * 0.5,
            base_y + (DOCK_ITEM_HEIGHT - height) * 0.5,
            width,
            height,
        );
        global_physical_geometry(visual, layout.monitor, layout.scale_factor)
    }
}

#[must_use]
/// Computes the logical width for a shelf with `item_count` visible cards.
pub fn shelf_width(item_count: usize, show_overflow: bool) -> f32 {
    let item_width = if item_count == 0 {
        0.0
    } else {
        item_count as f32 * DOCK_SLOT_WIDTH + item_count.saturating_sub(1) as f32 * DOCK_ITEM_GAP
    };
    DOCK_SHELF_PADDING * 3.0
        + DOCK_SEPARATOR_WIDTH
        + item_width
        + if show_overflow {
            DOCK_ITEM_GAP + DOCK_OVERFLOW_WIDTH
        } else {
            0.0
        }
}

fn dock_presentation() -> PresentationConfig {
    PresentationConfig {
        dock_item_size: DOCK_ITEM_HEIGHT,
        dock_item_aspect_ratio: DOCK_ITEM_WIDTH / DOCK_ITEM_HEIGHT,
        dock_item_gap: DOCK_ITEM_GAP,
        dock_shelf_padding: DOCK_SHELF_PADDING,
        dock_hover_scale: DOCK_HOVER_SCALE,
        dock_separator_width: DOCK_SEPARATOR_WIDTH,
        ..PresentationConfig::default()
    }
}

fn dock_scene(shelf: Rect, tokens: &[WindowToken]) -> Scene {
    let mut scene = Scene {
        viewport: Size::new((shelf.x + shelf.width).max(1.0), shelf.height.max(1.0)),
        clip: Rect::new(
            0.0,
            0.0,
            (shelf.x + shelf.width).max(1.0),
            shelf.height.max(1.0),
        ),
        nodes: vec![SceneNode::RoundedRect {
            id: NodeId::DockShelf,
            bounds: shelf,
            radius: 0.0,
            fill: Rgba::new(0.0, 0.0, 0.0, 0.0),
            stroke: None,
            state: VisualState::default(),
        }],
        hits: Vec::with_capacity(tokens.len()),
    };
    for (index, token) in tokens.iter().copied().enumerate() {
        scene.hits.push(HitRegion {
            id: NodeId::MinimizedWindow(token),
            bounds: Rect::new(
                shelf.x + index as f32 * (DOCK_SLOT_WIDTH + DOCK_ITEM_GAP),
                shelf.y,
                DOCK_SLOT_WIDTH,
                shelf.height,
            ),
            primary: None,
            secondary: None,
            scroll_up: None,
            scroll_down: None,
        });
    }
    scene
}

fn sane_scale(scale_factor: f64) -> f64 {
    if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BarModel, MINIMIZED_WINDOW_FLAG_PREVIEW_AVAILABLE, MonitorGeometry, MonitorId, WindowToken,
    };

    fn snapshot(session: u64) -> BarSnapshot {
        let mut snapshot = BarModel::default().snapshot();
        snapshot.wm_available = true;
        snapshot.wm_session_id = session;
        snapshot.geometry = Some(MonitorGeometry {
            x: -1920,
            y: 20,
            width: 1920,
            height: 40,
        });
        snapshot.minimized_windows.push(MinimizedWindow {
            token: WindowToken(7),
            monitor: MonitorId(0),
            title: "Terminal".into(),
            app_id: "foot".into(),
            flags: MINIMIZED_WINDOW_FLAG_PREVIEW_AVAILABLE,
        });
        snapshot
    }

    fn snapshot_with_windows(session: u64, count: u64, width: u32) -> BarSnapshot {
        let mut snapshot = snapshot(session);
        snapshot.geometry.as_mut().expect("test geometry").width = width;
        snapshot.minimized_windows = (1..=count)
            .map(|token| MinimizedWindow {
                token: WindowToken(token),
                monitor: MonitorId(0),
                title: format!("Window {token}"),
                app_id: "test".into(),
                flags: MINIMIZED_WINDOW_FLAG_PREVIEW_AVAILABLE,
            })
            .collect();
        snapshot
    }

    #[test]
    fn stale_closure_cannot_rebind_reused_window_token() {
        let mut bridge = DockBridge::new();
        bridge.synchronize(&snapshot(11), 1, 2.0, 40.0, 6.0);
        assert!(bridge.enter(WindowToken(7), 11));
        bridge.synchronize(&snapshot(12), 2, 2.0, 40.0, 6.0);
        assert!(!bridge.enter(WindowToken(7), 11));
        assert!(bridge.restore_action(WindowToken(7), 11).is_none());
        assert!(bridge.restore_action(WindowToken(7), 12).is_some());
    }

    #[test]
    fn transformed_restore_anchor_does_not_change_static_target() {
        let mut bridge = DockBridge::new();
        bridge.synchronize(&snapshot(21), 1, 2.0, 40.0, 6.0);
        let static_target = bridge
            .pending_actions(Instant::now())
            .into_iter()
            .find_map(|action| match action {
                UserAction::SetDockGeometry {
                    window: Some(WindowToken(7)),
                    geometry,
                    ..
                } => Some(geometry),
                _ => None,
            })
            .expect("static item geometry");
        assert!(bridge.enter(WindowToken(7), 21));
        let UserAction::RestoreWindow { geometry, .. } = bridge
            .restore_action(WindowToken(7), 21)
            .expect("fresh restore")
        else {
            unreachable!();
        };
        assert!(geometry.width > static_target.width);
        assert!(geometry.height > static_target.height);
    }

    #[test]
    fn negative_origin_and_two_x_scale_produce_global_physical_targets() {
        let mut bridge = DockBridge::new();
        bridge.synchronize(&snapshot(31), 1, 2.0, 40.0, 6.0);
        let target = bridge
            .pending_actions(Instant::now())
            .into_iter()
            .find_map(|action| match action {
                UserAction::SetDockGeometry {
                    window: Some(WindowToken(7)),
                    geometry,
                    ..
                } => Some(geometry),
                _ => None,
            })
            .expect("item target");
        assert_eq!(target, DockItemGeometry::new(-92, 40, 60, 40));
    }

    #[test]
    fn narrow_budget_keeps_newest_tail_and_withdraws_only_omitted_targets() {
        let now = Instant::now();
        let mut bridge = DockBridge::new();
        let wide = snapshot_with_windows(41, 4, 1920);
        bridge.synchronize(&wide, 1, 1.0, 40.0, 6.0);
        for action in bridge.pending_actions(now) {
            bridge.acknowledge(action, now);
        }

        let narrow = snapshot_with_windows(41, 4, 400);
        bridge.synchronize(&narrow, 1, 1.0, 40.0, 6.0);
        assert_eq!(
            bridge
                .visible_windows()
                .map(|window| window.token)
                .collect::<Vec<_>>(),
            vec![WindowToken(2), WindowToken(3), WindowToken(4)]
        );
        assert!(bridge.overflow());
        assert!(!bridge.collapsed());
        assert!(bridge.pending_actions(now).iter().any(|action| matches!(
            action,
            UserAction::SetDockGeometry {
                window: Some(WindowToken(1)),
                geometry,
                ..
            } if geometry.is_empty()
        )));

        let one_slot = snapshot_with_windows(41, 4, 200);
        bridge.synchronize(&one_slot, 1, 1.0, 40.0, 6.0);
        assert_eq!(
            bridge
                .visible_windows()
                .map(|window| window.token)
                .collect::<Vec<_>>(),
            vec![WindowToken(4)]
        );
        assert!(!bridge.collapsed());

        let no_slot = snapshot_with_windows(41, 4, 100);
        bridge.synchronize(&no_slot, 1, 1.0, 40.0, 6.0);
        assert_eq!(bridge.visible_windows().count(), 0);
        assert!(bridge.collapsed());
    }

    #[test]
    fn removed_window_explicitly_withdraws_its_acknowledged_target() {
        let now = Instant::now();
        let mut bridge = DockBridge::new();
        let populated = snapshot(46);
        bridge.synchronize(&populated, 1, 2.0, 40.0, 6.0);
        for action in bridge.pending_actions(now) {
            bridge.acknowledge(action, now);
        }

        let mut empty = populated;
        empty.minimized_windows.clear();
        bridge.synchronize(&empty, 1, 2.0, 40.0, 6.0);
        let withdrawal = bridge
            .pending_actions(now)
            .into_iter()
            .find(|action| {
                matches!(
                    action,
                    UserAction::SetDockGeometry {
                        window: Some(WindowToken(7)),
                        geometry,
                        ..
                    } if geometry.is_empty()
                )
            })
            .expect("removed target withdrawal");
        bridge.acknowledge(withdrawal, now);
        assert!(!bridge.pending_actions(now).iter().any(|action| matches!(
            action,
            UserAction::SetDockGeometry {
                window: Some(WindowToken(7)),
                ..
            }
        )));
    }

    #[test]
    fn failed_restore_is_retried_after_bounded_backoff() {
        let now = Instant::now();
        let mut bridge = DockBridge::new();
        bridge.synchronize(&snapshot(51), 1, 2.0, 40.0, 6.0);
        for action in bridge.pending_actions(now) {
            bridge.acknowledge(action, now);
        }

        assert!(bridge.request_restore(WindowToken(7), 51));
        assert!(bridge.pending_actions(now).iter().any(|action| matches!(
            action,
            UserAction::RestoreWindow {
                window: WindowToken(7),
                wm_session_id: 51,
                ..
            }
        )));
        bridge.record_failure(now);
        assert_eq!(
            bridge.next_retry_deadline(now),
            now.checked_add(Duration::from_millis(100)),
            "event loops use this deadline (or their 100ms transport wake) to retry promptly"
        );
        assert!(
            !bridge
                .pending_actions(now + Duration::from_millis(99))
                .iter()
                .any(|action| matches!(action, UserAction::RestoreWindow { .. }))
        );
        assert!(
            bridge
                .pending_actions(now + Duration::from_millis(100))
                .iter()
                .any(|action| matches!(action, UserAction::RestoreWindow { .. }))
        );
    }

    #[test]
    fn preview_enter_leave_and_two_second_renewal_keep_visual_anchor() {
        let now = Instant::now();
        let model = snapshot(61);
        let mut bridge = DockBridge::new();
        bridge.synchronize(&model, 1, 2.0, 40.0, 6.0);
        for action in bridge.pending_actions(now) {
            bridge.acknowledge(action, now);
        }

        assert!(bridge.enter(WindowToken(7), 61));
        bridge.synchronize(&model, 1, 2.0, 40.0, 6.0);
        let enter = bridge
            .pending_actions(now)
            .into_iter()
            .find(|action| matches!(action, UserAction::PreviewWindow { visible: true, .. }))
            .expect("preview enter");
        assert!(matches!(
            enter,
            UserAction::PreviewWindow {
                geometry: DockItemGeometry {
                    width: 93,
                    height: 62,
                    ..
                },
                ..
            }
        ));
        bridge.acknowledge(enter, now);
        assert!(
            !bridge
                .pending_actions(now + Duration::from_millis(1_999))
                .iter()
                .any(|action| matches!(action, UserAction::PreviewWindow { .. }))
        );
        assert!(
            bridge
                .pending_actions(now + Duration::from_secs(2))
                .iter()
                .any(|action| matches!(action, UserAction::PreviewWindow { visible: true, .. }))
        );

        assert!(bridge.leave(WindowToken(7), 61));
        assert!(bridge.pending_actions(now).iter().any(|action| matches!(
            action,
            UserAction::PreviewWindow {
                window: WindowToken(7),
                wm_session_id: 61,
                visible: false,
                ..
            }
        )));
    }
}
