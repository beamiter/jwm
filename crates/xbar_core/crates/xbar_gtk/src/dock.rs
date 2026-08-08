//! GTK implementation of the compositor-owned minimized-window Dock.
//!
//! GTK draws only the shelf, fallback cards, and pointer affordances. JWM's
//! compositor paints the real cached window texture into the physical
//! rectangles reported here. Coordinates deliberately start at the physical
//! bar origin carried by [`BarSnapshot`]; GTK4 does not expose (and Wayland
//! does not define) a client-controlled global window position.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::{Rc, Weak};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use gtk4::prelude::*;
use gtk4::{Button, EventControllerMotion, Label, Orientation, Separator, glib};
use xbar_core::controls::ControlSpec;
use xbar_core::{BarSnapshot, DockItemGeometry, UserAction, WindowToken};

use crate::{Dispatch, Metrics};

/// Renew before the compositor's four-second preview lease expires. Geometry
/// uses the same cadence so a bounded command queue that was full on the
/// layout turn naturally gets another chance without pointer-event flooding.
const DOCK_RENEW_INTERVAL: Duration = Duration::from_secs(2);
const DOCK_RETRY_INTERVAL: Duration = Duration::from_millis(100);
const PREVIEW_WITHDRAW_RETRY_WINDOW: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct LogicalRect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl LogicalRect {
    const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    fn is_finite_positive(self) -> bool {
        self.x.is_finite()
            && self.y.is_finite()
            && self.width.is_finite()
            && self.height.is_finite()
            && self.width > 0.0
            && self.height > 0.0
    }

    fn fully_inside(self, clip: Self) -> bool {
        const EPSILON: f64 = 0.01;
        self.is_finite_positive()
            && clip.is_finite_positive()
            && self.x + EPSILON >= clip.x
            && self.y + EPSILON >= clip.y
            && self.x + self.width <= clip.x + clip.width + EPSILON
            && self.y + self.height <= clip.y + clip.height + EPSILON
    }
}

/// Convert a bar-local logical allocation to the wire contract: global
/// physical pixels. Rounding edges instead of width/height separately keeps
/// adjacent rectangles contiguous at fractional logical coordinates.
fn global_physical_geometry(
    rect: LogicalRect,
    clip: LogicalRect,
    origin_x: i32,
    origin_y: i32,
    scale: i32,
) -> DockItemGeometry {
    if !rect.fully_inside(clip) {
        return DockItemGeometry::default();
    }
    let scale = f64::from(scale.max(1));
    let left = (rect.x * scale).round();
    let top = (rect.y * scale).round();
    let right = ((rect.x + rect.width) * scale).round();
    let bottom = ((rect.y + rect.height) * scale).round();
    let x = saturating_add_physical(origin_x, left);
    let y = saturating_add_physical(origin_y, top);
    let width = (right - left).round().clamp(1.0, f64::from(u32::MAX)) as u32;
    let height = (bottom - top).round().clamp(1.0, f64::from(u32::MAX)) as u32;
    DockItemGeometry::new(x, y, width, height)
}

fn saturating_add_physical(origin: i32, offset: f64) -> i32 {
    (f64::from(origin) + offset)
        .round()
        .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GeometryReport {
    window: Option<WindowToken>,
    geometry: DockItemGeometry,
}

/// Pure retained synchronization state. Missing items become explicit zero
/// rectangles, which withdraws compositor overlays when GTK clips a narrow
/// bar or removes a card.
#[derive(Debug, Default)]
struct GeometryLedger {
    reported: Vec<GeometryReport>,
}

impl GeometryLedger {
    fn reconcile(&mut self, desired: &[GeometryReport], force: bool) -> Vec<GeometryReport> {
        let mut changes = Vec::new();
        for previous in &self.reported {
            if !desired
                .iter()
                .any(|candidate| candidate.window == previous.window)
            {
                changes.push(GeometryReport {
                    window: previous.window,
                    geometry: DockItemGeometry::default(),
                });
            }
        }
        for desired in desired {
            if force || !self.reported.iter().any(|previous| previous == desired) {
                changes.push(*desired);
            }
        }
        self.reported.clear();
        self.reported.extend_from_slice(desired);
        changes
    }

    fn clear(&mut self) {
        self.reported.clear();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HoveredWindow {
    wm_session_id: u64,
    minimized_generation: u64,
    token: WindowToken,
}

#[derive(Debug, Clone, Copy)]
struct PreviewWithdrawal {
    hovered: HoveredWindow,
    last_sent: Instant,
    expires_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreviewAttemptKind {
    Fresh,
    Renewal,
}

struct PreviewAttempt {
    hovered: HoveredWindow,
    result: Receiver<bool>,
    expires_at: Instant,
}

#[derive(Debug, Clone, Copy)]
struct DockFrame {
    wm_available: bool,
    has_geometry: bool,
    wm_session_id: u64,
    minimized_generation: u64,
    origin_x: i32,
    origin_y: i32,
}

impl Default for DockFrame {
    fn default() -> Self {
        Self {
            wm_available: false,
            has_geometry: false,
            wm_session_id: 0,
            minimized_generation: 0,
            origin_x: 0,
            origin_y: 0,
        }
    }
}

struct DockState {
    frame: DockFrame,
    preview_available: HashSet<WindowToken>,
    hovered: Option<HoveredWindow>,
    confirmed_preview: Option<HoveredWindow>,
    preview_last_sent: Option<Instant>,
    preview_retry_not_before: Option<Instant>,
    preview_attempt: Option<PreviewAttempt>,
    preview_withdrawal: Option<PreviewWithdrawal>,
    geometry_last_sent: Option<Instant>,
    ledger: GeometryLedger,
    reconcile_scheduled: bool,
}

impl Default for DockState {
    fn default() -> Self {
        Self {
            frame: DockFrame::default(),
            preview_available: HashSet::new(),
            hovered: None,
            confirmed_preview: None,
            preview_last_sent: None,
            preview_retry_not_before: None,
            preview_attempt: None,
            preview_withdrawal: None,
            geometry_last_sent: None,
            ledger: GeometryLedger::default(),
            reconcile_scheduled: false,
        }
    }
}

impl DockState {
    fn cancel_preview_delivery(&mut self) {
        self.confirmed_preview = None;
        self.preview_last_sent = None;
        self.preview_retry_not_before = None;
        // Dropping the receiver makes a late frontend result inert. In
        // particular, an old result can never emit a LEAVE that clears a
        // same-token preview acquired by a newer hover attempt.
        self.preview_attempt = None;
    }

    fn finish_preview_attempt(&mut self, now: Instant) {
        let Some(attempt) = self.preview_attempt.as_ref() else {
            return;
        };
        let delivered = match attempt.result.try_recv() {
            Ok(delivered) => delivered,
            Err(TryRecvError::Empty) if now < attempt.expires_at => return,
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => false,
        };
        let attempt = self
            .preview_attempt
            .take()
            .expect("preview attempt was present while polling its receipt");
        let still_current = self.hovered == Some(attempt.hovered)
            && self.frame.wm_available
            && self.frame.wm_session_id == attempt.hovered.wm_session_id
            && self.frame.minimized_generation == attempt.hovered.minimized_generation
            && self.preview_available.contains(&attempt.hovered.token);
        if !still_current {
            return;
        }

        if delivered {
            self.confirmed_preview = Some(attempt.hovered);
            self.preview_last_sent = Some(now);
            self.preview_retry_not_before = None;
        } else {
            // A failed renewal no longer proves ownership either. Reacquire
            // with a fresh ENTER after the short bounded-queue backoff.
            self.confirmed_preview = None;
            self.preview_last_sent = None;
            self.preview_retry_not_before = Some(retry_deadline(now));
        }
    }

    fn preview_attempt_kind(
        &self,
        now: Instant,
        geometry_ready: bool,
    ) -> Option<PreviewAttemptKind> {
        let hovered = self.hovered?;
        if self.preview_attempt.is_some() || !geometry_ready {
            return None;
        }
        if self.confirmed_preview == Some(hovered) {
            return self
                .preview_last_sent
                .is_none_or(|last| now.saturating_duration_since(last) >= DOCK_RENEW_INTERVAL)
                .then_some(PreviewAttemptKind::Renewal);
        }
        self.preview_retry_not_before
            .is_none_or(|deadline| now >= deadline)
            .then_some(PreviewAttemptKind::Fresh)
    }
}

fn retry_deadline(now: Instant) -> Instant {
    now.checked_add(DOCK_RETRY_INTERVAL).unwrap_or(now)
}

struct DockCard {
    wm_session_id: u64,
    minimized_generation: u64,
    token: WindowToken,
    slot: gtk4::Box,
    button: Button,
    label: Label,
}

impl DockCard {
    fn update(&self, spec: &ControlSpec) {
        if self.label.text() != spec.icon {
            self.label.set_text(&spec.icon);
        }
        let title = if spec.value.trim().is_empty() {
            "Minimized window"
        } else {
            spec.value.as_str()
        };
        self.button.set_tooltip_text(Some(title));
        self.button
            .update_property(&[gtk4::accessible::Property::Label(title)]);
        set_class(&self.button, "urgent", spec.state.urgent);
        set_class(&self.button, "unavailable", !spec.available);
        self.button
            .set_sensitive(spec.available && spec.state.enabled);
    }
}

struct DockInner {
    bar_root: gtk4::Box,
    root: gtk4::Box,
    items: gtk4::Box,
    empty_slot: gtk4::Box,
    overflow_slot: gtk4::Box,
    cards: RefCell<Vec<DockCard>>,
    state: RefCell<DockState>,
    dispatch: Dispatch,
    metrics: Metrics,
}

impl DockInner {
    fn schedule_reconcile(self: &Rc<Self>) {
        if self.state.borrow().reconcile_scheduled {
            return;
        }
        self.state.borrow_mut().reconcile_scheduled = true;
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            inner.state.borrow_mut().reconcile_scheduled = false;
            inner.reconcile_geometry(false, Instant::now());
        });
    }

    fn sync(self: &Rc<Self>, snapshot: &BarSnapshot, controls: &[ControlSpec], overflow: bool) {
        let (origin_x, origin_y) = snapshot
            .geometry
            .map_or((0, 0), |geometry| (geometry.x, geometry.y));
        let next_frame = DockFrame {
            wm_available: snapshot.wm_available,
            has_geometry: snapshot.geometry.is_some(),
            wm_session_id: snapshot.wm_session_id,
            minimized_generation: snapshot.wm_sequence.unwrap_or_default(),
            origin_x,
            origin_y,
        };
        let identities_match = {
            let cards = self.cards.borrow();
            cards.len() == controls.len()
                && cards.iter().zip(controls).all(|(card, control)| {
                    card.wm_session_id == snapshot.wm_session_id
                        && card.minimized_generation == snapshot.wm_sequence.unwrap_or_default()
                        && matches!(
                            control.id,
                            xbar_core::presentation::NodeId::MinimizedWindow(token)
                                if token == card.token
                        )
                })
        };

        let cancelled_preview = {
            let mut state = self.state.borrow_mut();
            let projection_changed = state.frame.wm_session_id != next_frame.wm_session_id
                || state.frame.minimized_generation != next_frame.minimized_generation;
            let old_hover = state.hovered;
            if projection_changed {
                state.hovered = None;
                state.cancel_preview_delivery();
                state.preview_withdrawal = None;
                state.geometry_last_sent = None;
                state.ledger.clear();
            }
            state.frame = next_frame;
            state.preview_available = snapshot
                .minimized_windows
                .iter()
                .filter(|window| window.preview_available())
                .map(|window| window.token)
                .collect();
            let hovered_retired = state.hovered.is_some_and(|hovered| {
                hovered.wm_session_id != snapshot.wm_session_id
                    || hovered.minimized_generation != snapshot.wm_sequence.unwrap_or_default()
                    || !state.preview_available.contains(&hovered.token)
                    || !controls.iter().any(|control| {
                        matches!(
                            control.id,
                            xbar_core::presentation::NodeId::MinimizedWindow(token)
                                if token == hovered.token
                        )
                    })
            });
            if hovered_retired {
                state.hovered = None;
                state.cancel_preview_delivery();
                state.preview_withdrawal = None;
            }
            (projection_changed || hovered_retired)
                .then_some(old_hover)
                .flatten()
                .map(|hovered| Self::preview_action(&state, hovered, false, false))
        };
        if let Some(action) = cancelled_preview {
            (self.dispatch)(action, None);
        }

        if !identities_match {
            self.rebuild_cards(
                snapshot.wm_session_id,
                snapshot.wm_sequence.unwrap_or_default(),
                controls,
            );
        } else {
            for (card, spec) in self.cards.borrow().iter().zip(controls) {
                card.update(spec);
            }
        }

        self.empty_slot
            .set_visible(controls.is_empty() && !overflow);
        self.overflow_slot.set_visible(overflow);
        self.schedule_reconcile();
    }

    fn rebuild_cards(
        self: &Rc<Self>,
        wm_session_id: u64,
        minimized_generation: u64,
        controls: &[ControlSpec],
    ) {
        for card in self.cards.borrow_mut().drain(..) {
            self.items.remove(&card.slot);
        }

        let mut cards = self.cards.borrow_mut();
        for spec in controls {
            let xbar_core::presentation::NodeId::MinimizedWindow(token) = spec.id else {
                continue;
            };
            let card = self.make_card(wm_session_id, minimized_generation, token, spec);
            self.items.append(&card.slot);
            cards.push(card);
        }
    }

    fn make_card(
        self: &Rc<Self>,
        wm_session_id: u64,
        minimized_generation: u64,
        token: WindowToken,
        spec: &ControlSpec,
    ) -> DockCard {
        let slot = gtk4::Box::new(Orientation::Horizontal, 0);
        slot.add_css_class("dock-slot");
        slot.set_size_request(self.metrics.dock_slot_width, self.metrics.dock_slot_height);
        slot.set_halign(gtk4::Align::Center);
        slot.set_valign(gtk4::Align::Center);
        for property in ["width", "height", "mapped"] {
            slot.connect_notify_local(Some(property), {
                let weak = Rc::downgrade(self);
                move |_, _| {
                    if let Some(inner) = weak.upgrade() {
                        inner.schedule_reconcile();
                    }
                }
            });
        }

        let label = Label::new(None);
        label.set_single_line_mode(true);
        let button = Button::new();
        button.add_css_class("minimized-card");
        button.set_can_focus(false);
        button.set_focus_on_click(false);
        button.set_halign(gtk4::Align::Center);
        button.set_valign(gtk4::Align::Center);
        button.set_size_request(self.metrics.dock_item_width, self.metrics.dock_item_height);
        button.set_child(Some(&label));
        slot.append(&button);

        button.connect_clicked({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(inner) = weak.upgrade() {
                    inner.restore(wm_session_id, minimized_generation, token);
                }
            }
        });

        let motion = EventControllerMotion::new();
        motion.connect_enter({
            let weak = Rc::downgrade(self);
            let button = button.clone();
            move |_, _, _| {
                button.add_css_class("magnified");
                if let Some(inner) = weak.upgrade() {
                    inner.preview_enter(wm_session_id, minimized_generation, token);
                }
            }
        });
        motion.connect_leave({
            let weak = Rc::downgrade(self);
            let button = button.clone();
            move |_| {
                button.remove_css_class("magnified");
                if let Some(inner) = weak.upgrade() {
                    inner.preview_leave(wm_session_id, minimized_generation, token);
                }
            }
        });
        slot.add_controller(motion);

        let card = DockCard {
            wm_session_id,
            minimized_generation,
            token,
            slot,
            button,
            label,
        };
        card.update(spec);
        card
    }

    fn preview_enter(&self, wm_session_id: u64, minimized_generation: u64, token: WindowToken) {
        let now = Instant::now();
        let previous_leave = {
            let mut state = self.state.borrow_mut();
            if state.frame.wm_session_id != wm_session_id
                || state.frame.minimized_generation != minimized_generation
                || !state.frame.wm_available
                || !state.preview_available.contains(&token)
            {
                return;
            }
            let hovered = HoveredWindow {
                wm_session_id,
                minimized_generation,
                token,
            };
            if state.hovered == Some(hovered) {
                state.preview_withdrawal = None;
                return;
            }
            let previous_leave = state
                .hovered
                .map(|previous| Self::preview_action(&state, previous, false, false));
            state.hovered = Some(hovered);
            state.cancel_preview_delivery();
            state.preview_withdrawal = None;
            previous_leave
        };
        if let Some(action) = previous_leave {
            (self.dispatch)(action, None);
        }
        self.drive_preview(now);
    }

    fn preview_leave(&self, wm_session_id: u64, minimized_generation: u64, token: WindowToken) {
        let hovered = HoveredWindow {
            wm_session_id,
            minimized_generation,
            token,
        };
        let action = {
            let mut state = self.state.borrow_mut();
            if state.hovered != Some(hovered) {
                return;
            }
            state.hovered = None;
            state.cancel_preview_delivery();
            let now = Instant::now();
            state.preview_withdrawal = Some(PreviewWithdrawal {
                hovered,
                last_sent: now,
                expires_at: now + PREVIEW_WITHDRAW_RETRY_WINDOW,
            });
            Self::preview_action(&state, hovered, false, false)
        };
        (self.dispatch)(action, None);
    }

    fn restore(&self, wm_session_id: u64, minimized_generation: u64, token: WindowToken) {
        let hovered = HoveredWindow {
            wm_session_id,
            minimized_generation,
            token,
        };
        let (preview_leave, geometry) = {
            let mut state = self.state.borrow_mut();
            if state.frame.wm_session_id != wm_session_id
                || state.frame.minimized_generation != minimized_generation
                || !state.frame.wm_available
            {
                return;
            }
            let preview_leave = (state.hovered == Some(hovered)).then(|| {
                state.hovered = None;
                state.cancel_preview_delivery();
                state.preview_withdrawal = None;
                Self::preview_action(&state, hovered, false, false)
            });
            (preview_leave, Self::geometry_for(&state, Some(token)))
        };
        if let Some(preview_leave) = preview_leave {
            (self.dispatch)(preview_leave, None);
        }
        (self.dispatch)(
            UserAction::RestoreWindow {
                window: token,
                wm_session_id,
                minimized_generation,
                geometry,
            },
            None,
        );
    }

    fn preview_action(
        state: &DockState,
        hovered: HoveredWindow,
        visible: bool,
        renewal: bool,
    ) -> UserAction {
        let geometry = Self::geometry_for(state, Some(hovered.token));
        UserAction::PreviewWindow {
            window: hovered.token,
            wm_session_id: hovered.wm_session_id,
            minimized_generation: hovered.minimized_generation,
            visible,
            renewal,
            geometry,
        }
    }

    fn geometry_for(state: &DockState, window: Option<WindowToken>) -> DockItemGeometry {
        state
            .ledger
            .reported
            .iter()
            .find(|report| report.window == window)
            .map_or_else(DockItemGeometry::default, |report| report.geometry)
    }

    fn drive_preview(&self, now: Instant) {
        self.state.borrow_mut().finish_preview_attempt(now);
        let Some((action, completion)) = (|| {
            let mut state = self.state.borrow_mut();
            let hovered = state.hovered?;
            let geometry = Self::geometry_for(&state, Some(hovered.token));
            let geometry_ready = !geometry.is_empty();
            let Some(kind) = state.preview_attempt_kind(now, geometry_ready) else {
                if !geometry_ready
                    && state.preview_attempt.is_none()
                    && state.confirmed_preview != Some(hovered)
                    && state.preview_retry_not_before.is_none()
                {
                    state.preview_retry_not_before = Some(retry_deadline(now));
                }
                return None;
            };
            let (completion, result) = mpsc::channel();
            state.preview_attempt = Some(PreviewAttempt {
                hovered,
                result,
                expires_at: retry_deadline(now),
            });
            state.preview_retry_not_before = None;
            Some((
                UserAction::PreviewWindow {
                    window: hovered.token,
                    wm_session_id: hovered.wm_session_id,
                    minimized_generation: hovered.minimized_generation,
                    visible: true,
                    renewal: kind == PreviewAttemptKind::Renewal,
                    geometry,
                },
                completion,
            ))
        })() else {
            return;
        };

        (self.dispatch)(action, Some(completion));
        // gtk_bar completes synchronously. Relm returns on a later main-loop
        // turn and is polled by `maintain`; both paths share the same state.
        self.state.borrow_mut().finish_preview_attempt(now);
    }

    fn maintain(&self, now: Instant) {
        let force_geometry = self
            .state
            .borrow()
            .geometry_last_sent
            .is_none_or(|last| now.saturating_duration_since(last) >= DOCK_RENEW_INTERVAL);
        if force_geometry {
            self.reconcile_geometry(true, now);
        }

        self.state.borrow_mut().finish_preview_attempt(now);

        let withdrawal_actions = {
            let mut state = self.state.borrow_mut();
            let mut actions = Vec::new();
            if let Some(withdrawal) = state.preview_withdrawal {
                if now >= withdrawal.expires_at
                    || state.frame.wm_session_id != withdrawal.hovered.wm_session_id
                    || state.frame.minimized_generation != withdrawal.hovered.minimized_generation
                {
                    state.preview_withdrawal = None;
                } else if now.saturating_duration_since(withdrawal.last_sent) >= DOCK_RENEW_INTERVAL
                {
                    actions.push(Self::preview_action(
                        &state,
                        withdrawal.hovered,
                        false,
                        false,
                    ));
                    if let Some(withdrawal) = state.preview_withdrawal.as_mut() {
                        withdrawal.last_sent = now;
                    }
                }
            }
            actions
        };
        self.dispatch_all(withdrawal_actions);
        self.drive_preview(now);
    }

    fn reconcile_geometry(&self, force: bool, now: Instant) {
        let desired = self.desired_geometry();
        let actions = {
            let mut state = self.state.borrow_mut();
            if !state.frame.wm_available || state.frame.wm_session_id == 0 {
                state.ledger.clear();
                state.geometry_last_sent = None;
                return;
            }
            let wm_session_id = state.frame.wm_session_id;
            let minimized_generation = state.frame.minimized_generation;
            let changes = state.ledger.reconcile(&desired, force);
            state.geometry_last_sent = Some(now);
            changes
                .into_iter()
                .map(|report| UserAction::SetDockGeometry {
                    window: report.window,
                    wm_session_id,
                    minimized_generation,
                    geometry: report.geometry,
                })
                .collect::<Vec<_>>()
        };
        self.dispatch_all(actions);
    }

    fn desired_geometry(&self) -> Vec<GeometryReport> {
        let state = self.state.borrow();
        let frame = state.frame;
        drop(state);

        let scale = self.bar_root.scale_factor().max(1);
        let clip = LogicalRect::new(
            0.0,
            0.0,
            f64::from(self.bar_root.width()),
            f64::from(self.bar_root.height()),
        );
        let mut reports = Vec::with_capacity(self.cards.borrow().len() + 1);
        let shelf_bounds = if frame.has_geometry {
            widget_bounds(&self.root, &self.bar_root)
        } else {
            None
        };
        let shelf = shelf_bounds.map_or_else(DockItemGeometry::default, |bounds| {
            global_physical_geometry(bounds, clip, frame.origin_x, frame.origin_y, scale)
        });
        reports.push(GeometryReport {
            window: None,
            geometry: shelf,
        });

        for card in self.cards.borrow().iter() {
            let slot = if frame.has_geometry {
                widget_bounds(&card.slot, &self.bar_root)
            } else {
                None
            };
            let geometry = slot
                .map(|slot| resting_card_rect(slot, self.metrics))
                .map_or_else(DockItemGeometry::default, |bounds| {
                    global_physical_geometry(bounds, clip, frame.origin_x, frame.origin_y, scale)
                });
            reports.push(GeometryReport {
                window: Some(card.token),
                geometry,
            });
        }
        reports
    }

    fn dispatch_all(&self, actions: Vec<UserAction>) {
        for action in actions {
            (self.dispatch)(action, None);
        }
    }
}

fn widget_bounds(widget: &impl IsA<gtk4::Widget>, root: &gtk4::Box) -> Option<LogicalRect> {
    if !widget.as_ref().is_mapped() || !root.is_mapped() {
        return None;
    }
    widget
        .as_ref()
        .compute_bounds(root)
        .map(|bounds| {
            LogicalRect::new(
                f64::from(bounds.x()),
                f64::from(bounds.y()),
                f64::from(bounds.width()),
                f64::from(bounds.height()),
            )
        })
        .filter(|bounds| bounds.is_finite_positive())
}

fn resting_card_rect(slot: LogicalRect, metrics: Metrics) -> LogicalRect {
    let width = f64::from(metrics.dock_item_width).min(slot.width);
    let height = f64::from(metrics.dock_item_height).min(slot.height);
    LogicalRect::new(
        slot.x + (slot.width - width) * 0.5,
        slot.y + (slot.height - height) * 0.5,
        width,
        height,
    )
}

fn set_class(widget: &impl IsA<gtk4::Widget>, class: &str, present: bool) {
    if present {
        widget.as_ref().add_css_class(class);
    } else {
        widget.as_ref().remove_css_class(class);
    }
}

/// Shared minimized-window shelf used by both GTK frontends.
pub struct MinimizedDock {
    inner: Rc<DockInner>,
}

impl MinimizedDock {
    pub(crate) fn new(bar_root: &gtk4::Box, theme: &crate::BarTheme, dispatch: Dispatch) -> Self {
        let root = gtk4::Box::new(Orientation::Horizontal, theme.metrics.dock_item_gap);
        root.add_css_class("dock-shelf");
        root.set_valign(gtk4::Align::Center);

        let separator = Separator::new(Orientation::Vertical);
        separator.add_css_class("dock-separator");
        separator.set_size_request(
            theme.metrics.dock_separator_width,
            theme.metrics.dock_item_height,
        );
        separator.set_valign(gtk4::Align::Center);
        root.append(&separator);

        let empty_slot = gtk4::Box::new(Orientation::Horizontal, 0);
        empty_slot.add_css_class("dock-empty-slot");
        empty_slot.set_size_request(
            theme.metrics.dock_slot_width,
            theme.metrics.dock_slot_height,
        );
        root.append(&empty_slot);

        let items = gtk4::Box::new(Orientation::Horizontal, theme.metrics.dock_item_gap);
        items.add_css_class("dock-items");
        items.set_valign(gtk4::Align::Center);
        root.append(&items);

        let overflow_slot = gtk4::Box::new(Orientation::Horizontal, 0);
        overflow_slot.add_css_class("dock-slot");
        overflow_slot.set_size_request(
            theme.metrics.dock_slot_width,
            theme.metrics.dock_slot_height,
        );
        overflow_slot.set_halign(gtk4::Align::Center);
        overflow_slot.set_valign(gtk4::Align::Center);
        let overflow = Label::new(Some("…"));
        overflow.add_css_class("dock-overflow");
        overflow.set_tooltip_text(Some("More minimized windows"));
        overflow_slot.append(&overflow);
        overflow_slot.set_visible(false);
        root.append(&overflow_slot);

        let inner = Rc::new(DockInner {
            bar_root: bar_root.clone(),
            root,
            items,
            empty_slot,
            overflow_slot,
            cards: RefCell::new(Vec::new()),
            state: RefCell::new(DockState::default()),
            dispatch,
            metrics: theme.metrics,
        });

        connect_geometry_changes(&inner);
        Self { inner }
    }

    #[must_use]
    pub(crate) fn widget(&self) -> &gtk4::Box {
        &self.inner.root
    }

    pub(crate) fn sync(&self, snapshot: &BarSnapshot, controls: &[ControlSpec], overflow: bool) {
        self.inner.sync(snapshot, controls, overflow);
    }

    /// Retry geometry publication and renew an active hover-preview lease.
    /// Frontends may call this from a fast transport poll; it is internally
    /// rate-limited to two seconds.
    pub(crate) fn maintain(&self) {
        self.inner.maintain(Instant::now());
    }
}

fn connect_geometry_changes(inner: &Rc<DockInner>) {
    for property in ["width", "height", "scale-factor"] {
        inner.bar_root.connect_notify_local(Some(property), {
            let weak: Weak<DockInner> = Rc::downgrade(inner);
            move |_, _| {
                if let Some(inner) = weak.upgrade() {
                    inner.schedule_reconcile();
                }
            }
        });
    }
    for property in ["width", "height"] {
        inner.root.connect_notify_local(Some(property), {
            let weak: Weak<DockInner> = Rc::downgrade(inner);
            move |_, _| {
                if let Some(inner) = weak.upgrade() {
                    inner.schedule_reconcile();
                }
            }
        });
    }
    inner.root.connect_map({
        let weak = Rc::downgrade(inner);
        move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.schedule_reconcile();
            }
        }
    });
    inner.root.connect_unmap({
        let weak = Rc::downgrade(inner);
        move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.schedule_reconcile();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::{
        DOCK_RENEW_INTERVAL, DOCK_RETRY_INTERVAL, DockState, GeometryLedger, GeometryReport,
        HoveredWindow, LogicalRect, PreviewAttempt, PreviewAttemptKind, global_physical_geometry,
    };
    use xbar_core::{DockItemGeometry, WindowToken};

    fn preview_state() -> (DockState, HoveredWindow) {
        let hovered = HoveredWindow {
            wm_session_id: 91,
            minimized_generation: 73,
            token: WindowToken(41),
        };
        let mut state = DockState::default();
        state.frame.wm_available = true;
        state.frame.wm_session_id = hovered.wm_session_id;
        state.frame.minimized_generation = hovered.minimized_generation;
        state.preview_available.insert(hovered.token);
        state.hovered = Some(hovered);
        (state, hovered)
    }

    fn install_attempt(
        state: &mut DockState,
        hovered: HoveredWindow,
        now: Instant,
    ) -> mpsc::Sender<bool> {
        let (completion, result) = mpsc::channel();
        state.preview_attempt = Some(PreviewAttempt {
            hovered,
            result,
            expires_at: now + DOCK_RETRY_INTERVAL,
        });
        completion
    }

    #[test]
    fn logical_geometry_uses_snapshot_origin_and_gtk_scale() {
        let geometry = global_physical_geometry(
            LogicalRect::new(10.25, 2.5, 27.0, 18.0),
            LogicalRect::new(0.0, 0.0, 800.0, 38.0),
            -1920,
            120,
            2,
        );
        assert_eq!(geometry, DockItemGeometry::new(-1899, 125, 54, 36));
    }

    #[test]
    fn clipped_or_zero_allocation_withdraws_geometry() {
        let clip = LogicalRect::new(0.0, 0.0, 100.0, 38.0);
        assert!(
            global_physical_geometry(LogicalRect::new(90.0, 4.0, 27.0, 18.0), clip, 10, 20, 1,)
                .is_empty()
        );
        assert!(
            global_physical_geometry(LogicalRect::new(1.0, 1.0, 0.0, 18.0), clip, 10, 20, 1,)
                .is_empty()
        );
    }

    #[test]
    fn reconciliation_reports_changes_removals_and_heartbeat_retries() {
        let token = WindowToken(41);
        let shelf = GeometryReport {
            window: None,
            geometry: DockItemGeometry::new(100, 4, 48, 30),
        };
        let item = GeometryReport {
            window: Some(token),
            geometry: DockItemGeometry::new(111, 10, 27, 18),
        };
        let mut ledger = GeometryLedger::default();
        assert_eq!(ledger.reconcile(&[shelf, item], false), vec![shelf, item]);
        assert!(ledger.reconcile(&[shelf, item], false).is_empty());
        assert_eq!(ledger.reconcile(&[shelf, item], true), vec![shelf, item]);

        let withdrawals = ledger.reconcile(&[shelf], false);
        assert_eq!(
            withdrawals,
            vec![GeometryReport {
                window: Some(token),
                geometry: DockItemGeometry::default(),
            }]
        );
    }

    #[test]
    fn failed_fresh_enter_retries_as_fresh_before_any_renewal() {
        let now = Instant::now();
        let (mut state, hovered) = preview_state();
        assert_eq!(
            state.preview_attempt_kind(now, true),
            Some(PreviewAttemptKind::Fresh)
        );

        let completion = install_attempt(&mut state, hovered, now);
        completion.send(false).unwrap();
        state.finish_preview_attempt(now);
        assert_eq!(state.confirmed_preview, None);
        assert_eq!(
            state.preview_attempt_kind(now + DOCK_RETRY_INTERVAL - Duration::from_millis(1), true),
            None
        );
        assert_eq!(
            state.preview_attempt_kind(now + DOCK_RETRY_INTERVAL, true),
            Some(PreviewAttemptKind::Fresh)
        );
    }

    #[test]
    fn empty_geometry_waits_then_success_starts_the_renewal_clock() {
        let now = Instant::now();
        let (mut state, hovered) = preview_state();
        assert_eq!(state.preview_attempt_kind(now, false), None);
        assert_eq!(
            state.preview_attempt_kind(now, true),
            Some(PreviewAttemptKind::Fresh)
        );

        let completion = install_attempt(&mut state, hovered, now);
        completion.send(true).unwrap();
        state.finish_preview_attempt(now);
        assert_eq!(state.confirmed_preview, Some(hovered));
        assert_eq!(
            state.preview_attempt_kind(now + DOCK_RENEW_INTERVAL - Duration::from_millis(1), true),
            None
        );
        assert_eq!(
            state.preview_attempt_kind(now + DOCK_RENEW_INTERVAL, true),
            Some(PreviewAttemptKind::Renewal)
        );
    }

    #[test]
    fn cancellation_drops_a_late_receipt_without_touching_a_new_attempt() {
        let now = Instant::now();
        let (mut state, hovered) = preview_state();
        let old_completion = install_attempt(&mut state, hovered, now);

        state.cancel_preview_delivery();
        state.hovered = Some(hovered);
        let new_completion = install_attempt(&mut state, hovered, now);
        assert!(old_completion.send(true).is_err());
        new_completion.send(true).unwrap();
        state.finish_preview_attempt(now);

        assert_eq!(state.confirmed_preview, Some(hovered));
        assert_eq!(state.preview_last_sent, Some(now));
    }

    #[test]
    fn failed_renewal_falls_back_to_a_fresh_enter() {
        let now = Instant::now();
        let (mut state, hovered) = preview_state();
        state.confirmed_preview = Some(hovered);
        state.preview_last_sent = Some(now - DOCK_RENEW_INTERVAL);
        assert_eq!(
            state.preview_attempt_kind(now, true),
            Some(PreviewAttemptKind::Renewal)
        );

        let completion = install_attempt(&mut state, hovered, now);
        completion.send(false).unwrap();
        state.finish_preview_attempt(now);
        assert_eq!(state.confirmed_preview, None);
        assert_eq!(
            state.preview_attempt_kind(now + DOCK_RETRY_INTERVAL, true),
            Some(PreviewAttemptKind::Fresh)
        );
    }
}
