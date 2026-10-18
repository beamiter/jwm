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
/// Minimum acquisition extent for the actionable overflow indicator.
pub const DOCK_OVERFLOW_WIDTH: f32 = 32.0;
const MAX_VIEWPORT_FRACTION: f32 = 0.45;
const MIN_DOCK_WAKE_DELAY: Duration = Duration::from_millis(1);

/// Cap a host's normal sleep with a pending Dock deadline.
///
/// An overdue deadline yields a short non-zero delay so callers can service
/// it promptly without accidentally selecting a busy-polling control flow.
#[must_use]
pub fn dock_wake_delay(now: Instant, fallback: Duration, deadline: Option<Instant>) -> Duration {
    let fallback = fallback.max(MIN_DOCK_WAKE_DELAY);
    deadline
        .map(|deadline| {
            deadline
                .saturating_duration_since(now)
                .max(MIN_DOCK_WAKE_DELAY)
                .min(fallback)
        })
        .unwrap_or(fallback)
}

#[derive(Debug, Clone, Copy)]
struct DockLayout {
    edge: crate::placement::BarEdge,
    monitor: crate::MonitorGeometry,
    scale_factor: f64,
    shelf: Rect,
}

/// Immutable identity captured by one rendered minimized-window control.
///
/// A numeric window token may be reused after restore/re-minimize while the
/// WM session stays alive.  Retained widgets therefore keep the projection
/// generation alongside the token instead of looking up the bridge's newest
/// generation when an old callback eventually fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DockItemBinding {
    /// WM session that owned the rendered control.
    pub wm_session_id: u64,
    /// Exact minimized projection generation rendered by the control.
    pub minimized_generation: u64,
    /// Window represented by the control in that projection.
    pub token: WindowToken,
}

/// Protocol/reporting state used by custom retained toolkits.
#[derive(Debug, Default)]
pub struct DockBridge {
    edge: crate::placement::BarEdge,
    reporter: DockReporter,
    state_key: Option<(u64, u64, u64)>,
    windows: Vec<MinimizedWindow>,
    visible_tokens: Vec<WindowToken>,
    hovered: Option<WindowToken>,
    layout: Option<DockLayout>,
    overflow: bool,
    collapsed: bool,
    pending_restore: Option<UserAction>,
    retry_not_before: Option<Instant>,
}

impl DockBridge {
    /// Creates an empty bridge with no active WM session.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the screen edge before synchronizing a new frame. Window identity
    /// and queued restore intent survive a placement change; hover ownership
    /// is cleared until a pointer enters the new surface.
    pub fn set_edge(&mut self, edge: crate::placement::BarEdge) {
        if self.edge != edge {
            self.edge = edge;
            self.hovered = None;
            self.reporter.clear_preview();
            self.layout = None;
        }
    }

    #[must_use]
    pub const fn edge(&self) -> crate::placement::BarEdge {
        self.edge
    }

    /// Reconcile one owned model snapshot with a toolkit's fixed right-edge
    /// allocation (bottom allocation on sidebars). `right_padding` is logical
    /// padding along the main axis; the origin always comes from
    /// the WM snapshot and never from a toolkit window-position query.
    pub fn synchronize(
        &mut self,
        snapshot: &BarSnapshot,
        transport_generation: u64,
        scale_factor: f64,
        bar_height: f32,
        right_padding: f32,
    ) {
        let next_key = snapshot.wm_available.then_some((
            snapshot.wm_session_id,
            snapshot.wm_sequence.unwrap_or_default(),
            transport_generation,
        ));
        // Unavailability is not evidence that a click is stale: managed
        // transport recovery normally publishes an unavailable frame between
        // the retired and replacement handles. Keep the bounded intent while
        // offline, but let the first authoritative snapshot validate the full
        // projection identity before it can be emitted again.
        if snapshot.wm_available
            && self
                .pending_restore
                .as_ref()
                .is_some_and(|action| !restore_matches_snapshot(action, snapshot))
        {
            self.pending_restore = None;
            self.retry_not_before = None;
        }
        if self.state_key != next_key {
            self.hovered = None;
            // A replacement channel may have capacity immediately even when
            // the retired one returned QueueFull. Retry preserved intent on
            // the new transport instead of carrying its old backoff across.
            // While offline this remains unarmed because pending actions and
            // deadlines require a matching available projection below.
            self.retry_not_before = None;
            self.state_key = next_key;
        }
        self.windows.clone_from(&snapshot.minimized_windows);
        self.overflow = snapshot.minimized_overflow;

        let scale = sane_scale(scale_factor);
        let logical_width = snapshot
            .geometry
            .map(|geometry| {
                (if self.edge.is_vertical() {
                    geometry.height
                } else {
                    geometry.width
                }) as f32
                    / scale as f32
            })
            .unwrap_or(0.0);
        let right_padding = if right_padding.is_finite() {
            right_padding.clamp(0.0, logical_width)
        } else {
            0.0
        };
        let available_width = if snapshot.wm_available && snapshot.geometry.is_some() {
            (logical_width * MAX_VIEWPORT_FRACTION - right_padding).max(0.0)
        } else {
            0.0
        };
        let total = self.windows.len();
        let mut visible_count =
            if crate::presentation::DockMetrics::new(&dock_presentation(self.edge), bar_height)
                .is_some()
            {
                total
            } else {
                0
            };
        while visible_count > 0
            && shelf_extent(
                self.edge,
                bar_height,
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

        let extent = shelf_extent(
            self.edge,
            bar_height,
            self.visible_tokens.len(),
            self.overflow || self.collapsed,
        )
        .min((logical_width - right_padding).max(0.0));
        let thickness = if bar_height.is_finite() {
            bar_height.max(0.0)
        } else {
            0.0
        };
        let shelf = if self.edge.is_vertical() {
            Rect::new(
                0.0,
                (logical_width - right_padding - extent).max(0.0),
                thickness,
                extent,
            )
        } else {
            Rect::new(
                (logical_width - right_padding - extent).max(0.0),
                0.0,
                extent,
                thickness,
            )
        };
        self.layout = snapshot
            .geometry
            .filter(|_| snapshot.wm_available && !shelf.is_empty())
            .map(|monitor| DockLayout {
                edge: self.edge,
                monitor,
                scale_factor: scale,
                shelf,
            });

        let scene = dock_scene(shelf, self.edge, &self.visible_tokens, self.hovered);
        let presentation = dock_presentation(self.edge);
        self.reporter.synchronize(DockReporterInput {
            wm_available: snapshot.wm_available,
            wm_session_id: snapshot.wm_session_id,
            minimized_generation: snapshot.wm_sequence.unwrap_or_default(),
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

    /// Returns the immutable identity a rendered control must capture.
    ///
    /// The returned value remains intentionally detached from the bridge. If
    /// the projection changes before the callback runs, [`Self::enter`],
    /// [`Self::leave`], and [`Self::request_restore`] reject it.
    #[must_use]
    pub fn item_binding(&self, token: WindowToken) -> Option<DockItemBinding> {
        let (wm_session_id, minimized_generation, _) = self.state_key?;
        self.visible_tokens
            .contains(&token)
            .then_some(DockItemBinding {
                wm_session_id,
                minimized_generation,
                token,
            })
    }

    /// Identity for the newest locally omitted window, restored by overflow.
    ///
    /// Capture this value in the overflow control just like an ordinary item
    /// binding. It remains projection-scoped and cannot silently acquire a
    /// different window after resize, restore/re-minimize, or WM restart.
    /// Protocol-level overflow alone has no token to restore and returns None.
    #[must_use]
    pub fn overflow_binding(&self) -> Option<DockItemBinding> {
        let (wm_session_id, minimized_generation, _) = self.state_key?;
        self.overflow_bounds()?;
        let token = self
            .windows
            .iter()
            .rev()
            .find(|window| !self.visible_tokens.contains(&window.token))?
            .token;
        Some(DockItemBinding {
            wm_session_id,
            minimized_generation,
            token,
        })
    }

    /// Known local omission count for a `+N` overflow label.
    ///
    /// A zero count with [`Self::overflow`] still true means the WM's bounded
    /// projection omits additional windows whose tokens/count are unknown.
    #[must_use]
    pub fn overflow_count(&self) -> usize {
        self.windows.len().saturating_sub(self.visible_tokens.len())
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

    /// Returns the required main-axis extent: width for a horizontal shelf,
    /// height for a vertical shelf. Kept under the original method name for
    /// toolkit API compatibility.
    #[must_use]
    pub fn shelf_width(&self) -> f32 {
        self.layout.map_or_else(
            || {
                shelf_extent(
                    self.edge,
                    40.0,
                    self.visible_tokens.len(),
                    self.overflow || self.collapsed,
                )
            },
            |layout| {
                if self.edge.is_vertical() {
                    layout.shelf.height
                } else {
                    layout.shelf.width
                }
            },
        )
    }

    /// Exact shelf allocation in logical, bar-local coordinates.
    ///
    /// A retained toolkit must position this allocation in the bar's fixed
    /// root, outside scrolling controls. All item/slot accessors below use
    /// the same coordinate space, so subtract this rectangle's x/y when
    /// positioning children relative to the shelf. The WM's physical desktop
    /// origin is applied only when geometry is sent to the compositor.
    #[must_use]
    pub fn shelf_bounds(&self) -> Option<Rect> {
        self.layout.map(|layout| layout.shelf)
    }

    /// Current visual card rectangle in logical, bar-local coordinates.
    ///
    /// Includes hover/neighbor magnification. Render this exact rectangle;
    /// applying another scale or centering it in a different allocation would
    /// detach preview and restore animations from the visible thumbnail.
    #[must_use]
    pub fn item_bounds(&self, token: WindowToken) -> Option<Rect> {
        let layout = self.layout?;
        let index = self.visible_tokens.iter().position(|item| *item == token)?;
        Some(dock_item_visual_rect(
            layout.shelf,
            layout.edge,
            &self.visible_tokens,
            self.hovered,
            index,
        ))
    }

    /// Resting card rectangle in logical, bar-local coordinates.
    ///
    /// This is the stable compositor animation target, independent of hover.
    #[must_use]
    pub fn resting_item_bounds(&self, token: WindowToken) -> Option<Rect> {
        let layout = self.layout?;
        let index = self.visible_tokens.iter().position(|item| *item == token)?;
        Some(dock_item_visual_rect(
            layout.shelf,
            layout.edge,
            &self.visible_tokens,
            None,
            index,
        ))
    }

    /// Fixed acquisition target in logical, bar-local coordinates.
    ///
    /// Spans the full bar thickness and reserves peak magnification along
    /// the shelf's main axis. Attach pointer and focus interactions to this
    /// stable target while drawing the card at [`Self::item_bounds`].
    #[must_use]
    pub fn slot_bounds(&self, token: WindowToken) -> Option<Rect> {
        let layout = self.layout?;
        let index = self.visible_tokens.iter().position(|item| *item == token)?;
        dock_item_slot_rect(layout.shelf, layout.edge, index)
    }

    /// Overflow affordance rectangle in logical, bar-local coordinates.
    ///
    /// Both orientations reserve at least 32 logical pixels along the main
    /// axis. Returns `None` when every window fits or no current layout has
    /// been synchronized; exceptionally tiny viewports clip the affordance.
    #[must_use]
    pub fn overflow_bounds(&self) -> Option<Rect> {
        if !self.overflow && !self.collapsed {
            return None;
        }
        let layout = self.layout?;
        let axis = crate::presentation::BarAxis::from_rect(layout.edge, layout.shelf);
        let metrics = crate::presentation::DockMetrics::new(
            &dock_presentation(layout.edge),
            axis.cross_len(),
        )?;
        let bounds = if layout.edge.is_vertical() {
            metrics.slot_rect(axis, self.visible_tokens.len())
        } else {
            let offset = if self.visible_tokens.is_empty() {
                metrics.gap
            } else {
                self.visible_tokens.len() as f32 * metrics.slot_pitch
            };
            axis.rect(
                metrics.slot_start(axis) + offset,
                axis.cross_start(),
                DOCK_OVERFLOW_WIDTH,
                axis.cross_len(),
            )
        };
        bounds.intersection(layout.shelf)
    }

    /// Upright resting thumbnail dimensions after clipping to this bar's
    /// actual logical thickness. Native widgets should use this instead of
    /// constants when the bar is scaled or lies on a side edge.
    #[must_use]
    pub fn item_size(&self) -> Size {
        self.layout
            .and_then(|layout| {
                let axis = crate::presentation::BarAxis::from_rect(self.edge, layout.shelf);
                crate::presentation::DockMetrics::new(
                    &dock_presentation(self.edge),
                    axis.cross_len(),
                )
            })
            .map_or(Size::new(DOCK_ITEM_WIDTH, DOCK_ITEM_HEIGHT), |metrics| {
                Size::new(metrics.item_width, metrics.item_height)
            })
    }

    /// Fixed main-axis slot extent, including the peak hover allowance.
    #[must_use]
    pub fn slot_extent(&self) -> f32 {
        self.layout
            .and_then(|layout| {
                let axis = crate::presentation::BarAxis::from_rect(self.edge, layout.shelf);
                crate::presentation::DockMetrics::new(
                    &dock_presentation(self.edge),
                    axis.cross_len(),
                )
            })
            .map_or(
                if self.edge.is_vertical() {
                    32.0
                } else {
                    DOCK_SLOT_WIDTH
                },
                |metrics| metrics.slot_extent,
            )
    }

    /// Returns the macOS-style neighbour magnification for `token`.
    #[must_use]
    pub fn scale_for(&self, token: WindowToken) -> f32 {
        let Some(index) = self.visible_tokens.iter().position(|item| *item == token) else {
            return 1.0;
        };
        let peak = self
            .layout
            .and_then(|layout| {
                let axis = crate::presentation::BarAxis::from_rect(self.edge, layout.shelf);
                crate::presentation::DockMetrics::new(
                    &dock_presentation(self.edge),
                    axis.cross_len(),
                )
                .map(|metrics| metrics.peak_scale)
            })
            .unwrap_or(1.0);
        dock_item_scale(&self.visible_tokens, self.hovered, index, peak)
    }

    /// The full projection identity is captured by the rendered event
    /// closure. A stale retained node can therefore never silently rebind
    /// after a WM restart or a same-session token reuse.
    pub fn enter(&mut self, binding: DockItemBinding) -> bool {
        if !self.matches(binding)
            || !self.visible_tokens.contains(&binding.token)
            || !self
                .windows
                .iter()
                .any(|window| window.token == binding.token && window.preview_available())
        {
            return false;
        }
        self.hovered = Some(binding.token);
        self.reporter
            .set_hover(Some(NodeId::MinimizedWindow(binding.token)), &self.windows);
        true
    }

    /// Clears hover only if the captured control still owns it in the exact
    /// minimized projection.
    pub fn leave(&mut self, binding: DockItemBinding) -> bool {
        if !self.matches(binding) || self.hovered != Some(binding.token) {
            return false;
        }
        self.hovered = None;
        self.reporter.set_hover(None, &self.windows);
        true
    }

    /// Clears any hover owned by the exact projection captured by the shelf.
    pub fn leave_shelf(&mut self, wm_session_id: u64, minimized_generation: u64) -> bool {
        if self.state_key.is_none_or(|(session, generation, _)| {
            session != wm_session_id || generation != minimized_generation
        }) || self.hovered.is_none()
        {
            return false;
        }
        self.hovered = None;
        self.reporter.set_hover(None, &self.windows);
        true
    }

    /// Restore carries the exact visual rectangle seen by the user, while
    /// normal geometry reports retain the untransformed resting cards.
    /// Overflow restores use the overflow affordance as the animation origin.
    #[must_use]
    pub fn restore_action(&self, binding: DockItemBinding) -> Option<UserAction> {
        if !self.matches(binding) {
            return None;
        }
        let geometry = if self.visible_tokens.contains(&binding.token) {
            self.visual_geometry(binding.token)
        } else if self.overflow_binding() == Some(binding) {
            let layout = self.layout?;
            global_physical_geometry(self.overflow_bounds()?, layout.monitor, layout.scale_factor)
        } else {
            return None;
        };
        Some(UserAction::RestoreWindow {
            window: binding.token,
            wm_session_id: binding.wm_session_id,
            minimized_generation: binding.minimized_generation,
            geometry,
        })
    }

    /// Queue a restore so transient queue pressure or a reconnect cannot eat
    /// the user's click. Only one click is retained, bounding pointer storms.
    pub fn request_restore(&mut self, binding: DockItemBinding) -> bool {
        let Some(action) = self.restore_action(binding) else {
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
        let mut actions = Vec::new();
        let reported: Vec<_> = self
            .reporter
            .pending_actions(now)
            .into_iter()
            .map(|action| match action {
                UserAction::PreviewWindow {
                    window,
                    wm_session_id,
                    minimized_generation,
                    visible,
                    renewal,
                    ..
                } => UserAction::PreviewWindow {
                    window,
                    wm_session_id,
                    minimized_generation,
                    visible,
                    renewal,
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
        if local_retry_ready
            && self.pending_restore_ready()
            && let Some(restore) = self.pending_restore
        {
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
            self.pending_restore_ready().then(|| {
                self.retry_not_before
                    .map_or(now, |deadline| deadline.max(now))
            }),
        ) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (left, right) => left.or(right),
        }
    }

    /// Delay an event loop may sleep before servicing Dock work again.
    ///
    /// `fallback` is the host's existing transport/runtime cadence. Pending
    /// Dock work may shorten it, while the non-zero floor prevents an overdue
    /// deadline from turning a retained-mode event loop into a busy loop.
    #[must_use]
    pub fn next_wake_delay(&self, now: Instant, fallback: Duration) -> Duration {
        dock_wake_delay(now, fallback, self.next_retry_deadline(now))
    }

    #[must_use]
    fn matches(&self, binding: DockItemBinding) -> bool {
        self.state_key.is_some_and(|(session, generation, _)| {
            session == binding.wm_session_id && generation == binding.minimized_generation
        })
    }

    #[must_use]
    fn pending_restore_ready(&self) -> bool {
        let Some((session, generation, _)) = self.state_key else {
            return false;
        };
        self.pending_restore.as_ref().is_some_and(|action| {
            matches!(
                action,
                UserAction::RestoreWindow {
                    window,
                    wm_session_id,
                    minimized_generation,
                    ..
                } if *wm_session_id == session
                    && *minimized_generation == generation
                    && self.windows.iter().any(|candidate| candidate.token == *window)
            )
        })
    }

    #[must_use]
    fn visual_geometry(&self, token: WindowToken) -> DockItemGeometry {
        let Some(layout) = self.layout else {
            return DockItemGeometry::default();
        };
        let Some(visual) = self.item_bounds(token) else {
            return DockItemGeometry::default();
        };
        global_physical_geometry(visual, layout.monitor, layout.scale_factor)
    }
}

#[must_use]
fn restore_matches_snapshot(action: &UserAction, snapshot: &BarSnapshot) -> bool {
    matches!(
        action,
        UserAction::RestoreWindow {
            window,
            wm_session_id,
            minimized_generation,
            ..
        } if *wm_session_id == snapshot.wm_session_id
            && *minimized_generation == snapshot.wm_sequence.unwrap_or_default()
            && snapshot
                .minimized_windows
                .iter()
                .any(|candidate| candidate.token == *window)
    )
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

fn shelf_extent(
    edge: crate::placement::BarEdge,
    thickness: f32,
    item_count: usize,
    show_overflow: bool,
) -> f32 {
    let Some(metrics) = crate::presentation::DockMetrics::new(&dock_presentation(edge), thickness)
    else {
        return 0.0;
    };
    if edge.is_vertical() {
        return metrics.width_for_slots((item_count + usize::from(show_overflow)).max(1));
    }
    let item_extent = if item_count == 0 {
        0.0
    } else {
        item_count as f32 * metrics.slot_extent
            + item_count.saturating_sub(1) as f32 * DOCK_ITEM_GAP
    };
    DOCK_SHELF_PADDING * 3.0
        + DOCK_SEPARATOR_WIDTH
        + item_extent
        + if show_overflow {
            DOCK_ITEM_GAP + DOCK_OVERFLOW_WIDTH
        } else {
            0.0
        }
}

fn dock_presentation(edge: crate::placement::BarEdge) -> PresentationConfig {
    PresentationConfig {
        edge,
        dock_item_size: DOCK_ITEM_HEIGHT,
        dock_item_aspect_ratio: DOCK_ITEM_WIDTH / DOCK_ITEM_HEIGHT,
        dock_item_gap: DOCK_ITEM_GAP,
        dock_shelf_padding: DOCK_SHELF_PADDING,
        dock_hover_scale: DOCK_HOVER_SCALE,
        dock_separator_width: DOCK_SEPARATOR_WIDTH,
        ..PresentationConfig::default()
    }
}

fn dock_item_scale(
    tokens: &[WindowToken],
    hovered: Option<WindowToken>,
    index: usize,
    peak: f32,
) -> f32 {
    let Some(hover_index) = hovered.and_then(|token| tokens.iter().position(|item| *item == token))
    else {
        return 1.0;
    };
    let influence = match index.abs_diff(hover_index) {
        0 => 1.0,
        1 => (DOCK_NEIGHBOUR_SCALE - 1.0) / (DOCK_HOVER_SCALE - 1.0),
        2 => (DOCK_SECOND_NEIGHBOUR_SCALE - 1.0) / (DOCK_HOVER_SCALE - 1.0),
        _ => 0.0,
    };
    1.0 + (peak - 1.0).max(0.0) * influence
}

fn dock_item_visual_rect(
    shelf: Rect,
    edge: crate::placement::BarEdge,
    tokens: &[WindowToken],
    hovered: Option<WindowToken>,
    index: usize,
) -> Rect {
    let axis = crate::presentation::BarAxis::from_rect(edge, shelf);
    let Some(metrics) =
        crate::presentation::DockMetrics::new(&dock_presentation(edge), axis.cross_len())
    else {
        return Rect::default();
    };
    let scale = dock_item_scale(tokens, hovered, index, metrics.peak_scale);
    metrics.card_rect(axis, index, scale)
}

fn dock_item_slot_rect(shelf: Rect, edge: crate::placement::BarEdge, index: usize) -> Option<Rect> {
    let axis = crate::presentation::BarAxis::from_rect(edge, shelf);
    let metrics =
        crate::presentation::DockMetrics::new(&dock_presentation(edge), axis.cross_len())?;
    metrics.slot_rect(axis, index).intersection(shelf)
}

fn dock_scene(
    shelf: Rect,
    edge: crate::placement::BarEdge,
    tokens: &[WindowToken],
    hovered: Option<WindowToken>,
) -> Scene {
    let mut scene = Scene {
        viewport: Size::new(shelf.right().max(1.0), shelf.bottom().max(1.0)),
        clip: Rect::new(0.0, 0.0, shelf.right().max(1.0), shelf.bottom().max(1.0)),
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
        let Some(slot) = dock_item_slot_rect(shelf, edge, index) else {
            continue;
        };
        scene.nodes.push(SceneNode::RoundedRect {
            id: NodeId::MinimizedWindow(token),
            bounds: dock_item_visual_rect(shelf, edge, tokens, hovered, index),
            radius: 0.0,
            fill: Rgba::new(0.0, 0.0, 0.0, 0.0),
            stroke: None,
            state: VisualState::default(),
        });
        scene.hits.push(HitRegion {
            id: NodeId::MinimizedWindow(token),
            bounds: slot,
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

    fn assert_contains(outer: Rect, inner: Rect) {
        // Intersections at far output coordinates can round width/height by
        // a few float ULPs; containment concerns edges, not bitwise subtraction.
        const EPSILON: f32 = 0.001;
        assert!(
            !inner.is_empty()
                && inner.x + EPSILON >= outer.x
                && inner.y + EPSILON >= outer.y
                && inner.right() <= outer.right() + EPSILON
                && inner.bottom() <= outer.bottom() + EPSILON,
            "{outer:?} must contain {inner:?}"
        );
    }

    #[test]
    fn overdue_or_zero_wake_inputs_keep_a_nonzero_floor() {
        let now = Instant::now();
        assert_eq!(
            dock_wake_delay(now, Duration::ZERO, Some(now)),
            MIN_DOCK_WAKE_DELAY
        );
        assert_eq!(
            dock_wake_delay(
                now,
                Duration::from_millis(100),
                now.checked_sub(Duration::from_secs(1))
            ),
            MIN_DOCK_WAKE_DELAY
        );
    }

    #[test]
    fn stale_closure_cannot_rebind_reused_window_token_in_a_new_generation() {
        let now = Instant::now();
        let mut bridge = DockBridge::new();
        let mut first = snapshot(11);
        first.wm_sequence = Some(1);
        bridge.synchronize(&first, 1, 2.0, 40.0, 6.0);
        for action in bridge.pending_actions(now) {
            bridge.acknowledge(action, now);
        }
        let stale = bridge
            .item_binding(WindowToken(7))
            .expect("first rendered control identity");
        assert!(bridge.enter(stale));

        let mut second = snapshot(11);
        second.wm_sequence = Some(2);
        bridge.synchronize(&second, 1, 2.0, 40.0, 6.0);

        let current_actions = bridge.pending_actions(now);
        assert!(!current_actions.iter().any(|action| matches!(
            action,
            UserAction::SetDockGeometry {
                window: Some(WindowToken(7)),
                geometry,
                ..
            } if geometry.is_empty()
        )));
        assert!(current_actions.iter().any(|action| matches!(
            action,
            UserAction::SetDockGeometry {
                window: Some(WindowToken(7)),
                minimized_generation: 2,
                geometry,
                ..
            } if !geometry.is_empty()
        )));
        assert!(!bridge.enter(stale));
        assert!(bridge.restore_action(stale).is_none());
        assert!(!bridge.request_restore(stale));
        assert!(
            !bridge.pending_actions(Instant::now()).iter().any(|action| {
                matches!(
                    action,
                    UserAction::RestoreWindow { .. }
                        | UserAction::PreviewWindow { visible: true, .. }
                )
            })
        );

        let current = bridge
            .item_binding(WindowToken(7))
            .expect("replacement rendered control identity");
        assert_ne!(stale, current);
        assert!(bridge.enter(current));
        assert!(!bridge.leave(stale));
        assert_eq!(bridge.hovered(), Some(current.token));
        assert!(bridge.restore_action(current).is_some());
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
        let binding = bridge.item_binding(WindowToken(7)).expect("dock binding");
        assert!(bridge.enter(binding));
        let UserAction::RestoreWindow { geometry, .. } =
            bridge.restore_action(binding).expect("fresh restore")
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
            vec![WindowToken(3), WindowToken(4)]
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

        let one_slot = snapshot_with_windows(41, 4, 220);
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
        let mut populated = snapshot(46);
        populated.wm_sequence = Some(1);
        bridge.synchronize(&populated, 1, 2.0, 40.0, 6.0);
        for action in bridge.pending_actions(now) {
            bridge.acknowledge(action, now);
        }

        let mut empty = populated;
        empty.wm_sequence = Some(2);
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
                        minimized_generation: 2,
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

        let binding = bridge.item_binding(WindowToken(7)).expect("dock binding");
        assert!(bridge.request_restore(binding));
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
    fn queued_restore_survives_same_incarnation_transport_reconnect() {
        let now = Instant::now();
        let model = snapshot(52);
        let mut bridge = DockBridge::new();
        bridge.synchronize(&model, 1, 2.0, 40.0, 6.0);
        for action in bridge.pending_actions(now) {
            bridge.acknowledge(action, now);
        }

        let binding = bridge.item_binding(WindowToken(7)).expect("dock binding");
        assert!(bridge.request_restore(binding));
        bridge.record_failure(now);
        assert!(bridge.pending_actions(now).is_empty());

        let mut unavailable = model.clone();
        unavailable.wm_available = false;
        bridge.synchronize(&unavailable, 2, 2.0, 40.0, 6.0);
        assert!(
            !bridge
                .pending_actions(now + Duration::from_secs(1))
                .iter()
                .any(|action| matches!(action, UserAction::RestoreWindow { .. })),
            "offline intent is retained but must not be emitted"
        );
        assert_eq!(bridge.next_retry_deadline(now), None);

        bridge.synchronize(&model, 3, 2.0, 40.0, 6.0);
        assert!(bridge.pending_actions(now).iter().any(|action| matches!(
            action,
            UserAction::RestoreWindow {
                window: WindowToken(7),
                wm_session_id: 52,
                minimized_generation: 0,
                ..
            }
        )));
        assert_eq!(bridge.next_retry_deadline(now), Some(now));

        let mut new_incarnation = model;
        new_incarnation.wm_sequence = Some(1);
        bridge.synchronize(&new_incarnation, 4, 2.0, 40.0, 6.0);
        assert!(
            !bridge
                .pending_actions(now)
                .iter()
                .any(|action| matches!(action, UserAction::RestoreWindow { .. })),
            "a projection epoch change must still retire the old click"
        );
    }

    #[test]
    fn wake_delay_services_stale_anchor_then_queue_full_backoff() {
        let start = Instant::now();
        let model = snapshot(52);
        let mut bridge = DockBridge::new();
        bridge.synchronize(&model, 1, 2.0, 40.0, 6.0);
        for action in bridge.pending_actions(start) {
            bridge.acknowledge(action, start);
        }

        let binding = bridge.item_binding(WindowToken(7)).expect("dock binding");
        assert!(bridge.enter(binding));
        bridge.synchronize(&model, 1, 2.0, 40.0, 6.0);
        for action in bridge.pending_actions(start) {
            bridge.acknowledge(action, start);
        }

        // A retained toolkit lays the magnified card out at a new scale one
        // frame later. Geometry reports can be acknowledged immediately, but
        // the preview anchor itself remains throttled until 50ms.
        let moved = start + Duration::from_millis(1);
        bridge.synchronize(&model, 1, 1.5, 40.0, 6.0);
        for action in bridge.pending_actions(moved) {
            assert!(!matches!(action, UserAction::PreviewWindow { .. }));
            bridge.acknowledge(action, moved);
        }
        assert_eq!(
            bridge.next_retry_deadline(moved),
            start.checked_add(crate::DOCK_PREVIEW_ANCHOR_INTERVAL)
        );
        assert_eq!(
            bridge.next_wake_delay(moved, Duration::from_millis(250)),
            Duration::from_millis(49)
        );

        let anchor_due = start + crate::DOCK_PREVIEW_ANCHOR_INTERVAL;
        assert!(
            bridge
                .pending_actions(anchor_due)
                .iter()
                .any(|action| matches!(action, UserAction::PreviewWindow { renewal: true, .. }))
        );
        // QueueFull leaves that action pending. The event loop must now honor
        // the reporter's 100ms backoff instead of spinning on an overdue
        // anchor deadline or sleeping for the host's 250ms fallback.
        bridge.record_failure(anchor_due);
        assert!(bridge.pending_actions(anchor_due).is_empty());
        assert_eq!(
            bridge.next_wake_delay(anchor_due, Duration::from_millis(250)),
            Duration::from_millis(100)
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

        let binding = bridge.item_binding(WindowToken(7)).expect("dock binding");
        assert!(bridge.enter(binding));
        bridge.synchronize(&model, 1, 2.0, 40.0, 6.0);
        let enter = bridge
            .pending_actions(now)
            .into_iter()
            .find(|action| matches!(action, UserAction::PreviewWindow { visible: true, .. }))
            .expect("preview enter");
        assert!(matches!(
            enter,
            UserAction::PreviewWindow {
                renewal: false,
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
                .any(|action| matches!(
                    action,
                    UserAction::PreviewWindow {
                        visible: true,
                        renewal: true,
                        ..
                    }
                ))
        );

        assert!(bridge.leave(binding));
        assert!(bridge.pending_actions(now).iter().any(|action| matches!(
            action,
            UserAction::PreviewWindow {
                window: WindowToken(7),
                wm_session_id: 61,
                visible: false,
                renewal: false,
                ..
            }
        )));
    }

    #[test]
    fn sidebar_bridge_budgets_output_height_and_preserves_authoritative_origin_at_every_scale() {
        use crate::placement::BarEdge;
        for edge in [BarEdge::Left, BarEdge::Right] {
            for scale in [1.0, 1.25, 2.0] {
                let mut model = snapshot_with_windows(81, 6, 42);
                model.geometry = Some(MonitorGeometry {
                    x: -42,
                    y: 80,
                    width: 42,
                    height: 1080,
                });
                let mut bridge = DockBridge::new();
                bridge.set_edge(edge);
                bridge.synchronize(&model, 1, scale, (42.0 / scale) as f32, 6.0);
                assert!(
                    bridge.visible_windows().count() > 0,
                    "must budget output height, not 42px width"
                );
                assert!(bridge.slot_extent() >= 32.0);
                let visible: Vec<_> = bridge
                    .visible_windows()
                    .map(|window| window.token)
                    .collect();
                let now = Instant::now();
                let targets: Vec<_> = bridge
                    .pending_actions(now)
                    .into_iter()
                    .filter_map(|action| match action {
                        UserAction::SetDockGeometry {
                            window: Some(_),
                            geometry,
                            ..
                        } => Some(geometry),
                        _ => None,
                    })
                    .collect();
                assert_eq!(targets.len(), visible.len());
                for target in targets {
                    assert!(target.x >= -42 && i64::from(target.x) + i64::from(target.width) <= 0);
                    assert!(
                        target.y >= 80 && i64::from(target.y) + i64::from(target.height) <= 1160
                    );
                    assert!(target.width >= target.height, "thumbnails remain landscape");
                }
                let binding = bridge.item_binding(visible[0]).unwrap();
                bridge.enter(binding);
                let size = bridge.item_size();
                assert!(f64::from(size.width * bridge.scale_for(visible[0])) * scale <= 42.001);
                let UserAction::RestoreWindow { geometry, .. } =
                    bridge.restore_action(binding).unwrap()
                else {
                    panic!("restore")
                };
                assert!(
                    geometry.x >= -42 && i64::from(geometry.x) + i64::from(geometry.width) <= 0
                );
            }
        }
    }

    #[test]
    fn retained_allocations_match_reported_and_interactive_geometry_on_every_edge() {
        use crate::placement::BarEdge;
        for edge in [BarEdge::Top, BarEdge::Bottom, BarEdge::Left, BarEdge::Right] {
            for scale in [1.0, 1.25, 2.0] {
                let mut model = snapshot_with_windows(91, 4, 1600);
                let monitor = MonitorGeometry {
                    x: -1200,
                    y: 45,
                    width: if edge.is_vertical() { 42 } else { 1600 },
                    height: if edge.is_vertical() { 1600 } else { 42 },
                };
                model.geometry = Some(monitor);
                let mut bridge = DockBridge::new();
                bridge.set_edge(edge);
                bridge.synchronize(&model, 1, scale, (42.0 / scale) as f32, 6.0);
                let shelf = bridge.shelf_bounds().expect("fixed shelf allocation");
                let tokens: Vec<_> = bridge
                    .visible_windows()
                    .map(|window| window.token)
                    .collect();
                assert_eq!(tokens.len(), 4);
                let slots: Vec<_> = tokens
                    .iter()
                    .map(|token| bridge.slot_bounds(*token).unwrap())
                    .collect();
                let stable: Vec<_> = tokens
                    .iter()
                    .map(|token| bridge.resting_item_bounds(*token).unwrap())
                    .collect();
                let reports = bridge.pending_actions(Instant::now());
                assert!(reports.iter().any(|action| matches!(action,
                    UserAction::SetDockGeometry { window: None, geometry, .. }
                        if *geometry == global_physical_geometry(shelf, monitor, scale)
                )));
                for (index, token) in tokens.iter().enumerate() {
                    assert_eq!(bridge.item_bounds(*token), Some(stable[index]));
                    assert_contains(slots[index], stable[index]);
                    assert!(reports.iter().any(|action| matches!(action,
                        UserAction::SetDockGeometry { window: Some(window), geometry, .. }
                            if window == token && *geometry == global_physical_geometry(stable[index], monitor, scale)
                    )));
                }
                let binding = bridge.item_binding(tokens[1]).unwrap();
                assert!(bridge.enter(binding));
                bridge.synchronize(&model, 1, scale, (42.0 / scale) as f32, 6.0);
                for (index, token) in tokens.iter().enumerate() {
                    assert_eq!(bridge.slot_bounds(*token), Some(slots[index]));
                    assert_eq!(bridge.resting_item_bounds(*token), Some(stable[index]));
                    let visual = bridge.item_bounds(*token).unwrap();
                    assert_contains(slots[index], visual);
                    assert!(
                        visual.width >= visual.height,
                        "upright landscape cards on {edge:?}"
                    );
                    let action = bridge
                        .restore_action(bridge.item_binding(*token).unwrap())
                        .unwrap();
                    assert!(matches!(action, UserAction::RestoreWindow { geometry, .. }
                        if geometry == global_physical_geometry(visual, monitor, scale)));
                }
                let synthetic = dock_scene(shelf, edge, &tokens, Some(tokens[1]));
                for (index, hit) in synthetic.hits.iter().enumerate() {
                    assert_eq!(hit.bounds, slots[index]);
                    assert_eq!(
                        synthetic.bounds_for(hit.id),
                        bridge.item_bounds(tokens[index])
                    );
                }
            }
        }
    }

    #[test]
    fn overflow_restores_newest_omitted_window_from_visible_affordance_on_every_edge() {
        use crate::placement::BarEdge;
        for edge in [BarEdge::Top, BarEdge::Bottom, BarEdge::Left, BarEdge::Right] {
            let mut model = snapshot_with_windows(92, 8, 400);
            model.wm_sequence = Some(8);
            let monitor = MonitorGeometry {
                x: -42,
                y: 90,
                width: if edge.is_vertical() { 42 } else { 400 },
                height: if edge.is_vertical() { 400 } else { 42 },
            };
            model.geometry = Some(monitor);
            let mut bridge = DockBridge::new();
            bridge.set_edge(edge);
            bridge.synchronize(&model, 1, 1.25, 42.0 / 1.25, 6.0);
            let overflow = bridge.overflow_bounds().unwrap();
            assert!(
                if edge.is_vertical() {
                    overflow.height
                } else {
                    overflow.width
                } >= 32.0
            );
            let binding = bridge.overflow_binding().expect("known omitted window");
            assert_eq!(binding.token.0 as usize, bridge.overflow_count());
            assert!(bridge.item_binding(binding.token).is_none());
            assert!(
                !bridge.enter(binding),
                "overflow is not a false thumbnail preview"
            );
            let action = bridge.restore_action(binding).unwrap();
            assert!(
                matches!(action, UserAction::RestoreWindow { window, geometry, .. }
                if window == binding.token && geometry == global_physical_geometry(overflow, monitor, 1.25))
            );
            assert!(
                !bridge
                    .pending_actions(Instant::now())
                    .iter()
                    .any(|action| matches!(action,
                        UserAction::SetDockGeometry { window: Some(token), geometry, .. }
                            if *token == binding.token && !geometry.is_empty()
                    )),
                "omitted windows must not acquire fictional persistent card slots"
            );

            // A queued overflow click is preserved through transport recovery,
            // then retired by a changed minimized projection.
            let now = Instant::now();
            assert!(bridge.request_restore(binding));
            bridge.record_failure(now);
            let mut offline = model.clone();
            offline.wm_available = false;
            bridge.synchronize(&offline, 2, 1.25, 42.0 / 1.25, 6.0);
            assert!(bridge.overflow_binding().is_none());
            assert!(
                !bridge
                    .pending_actions(now)
                    .iter()
                    .any(|action| matches!(action, UserAction::RestoreWindow { .. }))
            );
            bridge.synchronize(&model, 3, 1.25, 42.0 / 1.25, 6.0);
            assert!(bridge.pending_actions(now).contains(&action));
            model.wm_sequence = Some(9);
            bridge.synchronize(&model, 3, 1.25, 42.0 / 1.25, 6.0);
            assert!(bridge.restore_action(binding).is_none());
            assert!(!bridge.request_restore(binding));
            assert!(
                !bridge
                    .pending_actions(now)
                    .iter()
                    .any(|action| matches!(action, UserAction::RestoreWindow { .. }))
            );
            model.minimized_windows.clear();
            model.minimized_overflow = true;
            bridge.synchronize(&model, 3, 1.25, 42.0 / 1.25, 6.0);
            assert!(bridge.overflow());
            assert_eq!(bridge.overflow_count(), 0);
            assert!(
                bridge.overflow_binding().is_none(),
                "unknown protocol overflow cannot invent a token"
            );
        }
    }

    #[test]
    fn retained_geometry_stays_inside_short_viewports_and_rejects_obsolete_overflow_binding() {
        use crate::placement::BarEdge;
        for edge in [BarEdge::Top, BarEdge::Bottom, BarEdge::Left, BarEdge::Right] {
            for main in [1, 24, 80, 100, 220, 400] {
                let mut model = snapshot_with_windows(93, 12, main);
                model.geometry = Some(MonitorGeometry {
                    x: -1920,
                    y: 100,
                    width: if edge.is_vertical() { 42 } else { main },
                    height: if edge.is_vertical() { main } else { 42 },
                });
                let mut bridge = DockBridge::new();
                bridge.set_edge(edge);
                bridge.synchronize(&model, 1, 1.0, 42.0, 6.0);
                let viewport = if edge.is_vertical() {
                    Rect::new(0.0, 0.0, 42.0, main as f32)
                } else {
                    Rect::new(0.0, 0.0, main as f32, 42.0)
                };
                if let Some(shelf) = bridge.shelf_bounds() {
                    assert_eq!(shelf.intersection(viewport), Some(shelf));
                }
                if let Some(overflow) = bridge.overflow_bounds() {
                    assert_eq!(overflow.intersection(viewport), Some(overflow));
                }
                let items: Vec<_> = bridge
                    .visible_windows()
                    .map(|window| window.token)
                    .collect();
                for token in items {
                    let slot = bridge.slot_bounds(token).unwrap();
                    assert_eq!(slot.intersection(viewport), Some(slot));
                    assert!(
                        bridge
                            .overflow_bounds()
                            .is_none_or(|overflow| slot.intersection(overflow).is_none())
                    );
                }
            }
            let mut model = snapshot_with_windows(94, 12, 600);
            if edge.is_vertical() {
                model.geometry.as_mut().unwrap().width = 42;
                model.geometry.as_mut().unwrap().height = 600;
            }
            let mut bridge = DockBridge::new();
            bridge.set_edge(edge);
            bridge.synchronize(&model, 1, 1.0, 42.0, 6.0);
            let old_overflow = bridge.overflow_binding().unwrap();
            if edge.is_vertical() {
                model.geometry.as_mut().unwrap().height = 100;
            } else {
                model.geometry.as_mut().unwrap().width = 100;
            }
            bridge.synchronize(&model, 1, 1.0, 42.0, 6.0);
            assert_ne!(bridge.overflow_binding(), Some(old_overflow));
            assert!(bridge.restore_action(old_overflow).is_none());
        }
    }
}
