// Drag-and-snap re-attach: dropping a dragged window near a monitor edge
// inserts it back into the current layout at the slot under the pointer,
// instead of leaving it as a floating half-screen window.
//
// The slot is found by simulation: the pure layout calculators take an
// ordered client list, so we insert the dragged client at every possible
// index, run the current layout's calculator, and keep the index whose
// resulting rect contains the pointer. Fibonacci's bottom-right spiral cell,
// grid cells, bstack columns … all fall out of the same mechanism.

use crate::backend::api::{Backend, MaximizeAxes, NetWmAction};
use crate::config::CONFIG;
use crate::core::layout::{
    self as core_layout, LayoutClient, LayoutEnum, LayoutParams, LayoutResult,
};
use crate::core::maximize::{MaximizeOrigin, requested_axes};
use crate::core::models::{ClientKey, MonitorKey};
use crate::core::types::Rect;
use crate::jwm::{Jwm, WMArgEnum};
use log::{error, info};

/// Edge-snap targets shared by the mouse drop below and the bindable
/// `snap_window` command: left/right halves, top-edge maximize, and the four
/// corner quarters. There is deliberately no plain `Top`/`Bottom` half — the
/// mouse path never snaps to the bottom edge, and the top edge maximizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SnapDirection {
    Left,
    Right,
    Maximize,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl SnapDirection {
    /// The names accepted by the `snap_window` command, case-insensitively.
    /// The IPC dispatch in `crate::ipc` validates against the same list;
    /// both sides pin the accepted set in their tests.
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Some(match name.to_ascii_lowercase().as_str() {
            "left" => SnapDirection::Left,
            "right" => SnapDirection::Right,
            "maximize" => SnapDirection::Maximize,
            "top-left" => SnapDirection::TopLeft,
            "top-right" => SnapDirection::TopRight,
            "bottom-left" => SnapDirection::BottomLeft,
            "bottom-right" => SnapDirection::BottomRight,
            _ => return None,
        })
    }

    pub(crate) fn as_name(self) -> &'static str {
        match self {
            SnapDirection::Left => "left",
            SnapDirection::Right => "right",
            SnapDirection::Maximize => "maximize",
            SnapDirection::TopLeft => "top-left",
            SnapDirection::TopRight => "top-right",
            SnapDirection::BottomLeft => "bottom-left",
            SnapDirection::BottomRight => "bottom-right",
        }
    }

    /// Parse a `snap_window` binding/IPC argument: exactly one direction
    /// string (a plain config string arrives as a one-element vector).
    /// Anything else is a caller error, never a silent no-op.
    pub(crate) fn from_arg(arg: &WMArgEnum) -> Result<Self, String> {
        let WMArgEnum::StringVec(values) = arg else {
            return Err(format!(
                "snap_window takes a direction string (\"left\", \"right\", \"maximize\", \"top-left\", \"top-right\", \"bottom-left\", \"bottom-right\"), got {arg:?}"
            ));
        };
        let [name] = values.as_slice() else {
            return Err(format!(
                "snap_window takes exactly one direction, got {}",
                values.len()
            ));
        };
        Self::from_name(name).ok_or_else(|| {
            format!(
                "unknown snap direction {name:?}; expected \"left\", \"right\", \"maximize\", \"top-left\", \"top-right\", \"bottom-left\", or \"bottom-right\""
            )
        })
    }
}

/// The snap geometry of the classic mouse float snap, extracted so the
/// keyboard command produces the same rect: left/right halves of the monitor
/// and the four corner quarters. Quarters reuse the halves' integer rule —
/// both dimensions floor, so an odd width or height leaves the last
/// column/row uncovered.
///
/// `Maximize` still maps to the full monitor rect for its unit tests, but no
/// caller snaps to it any more: the top-edge drop and `snap_window maximize`
/// are real maximize requests that fill the work area (bar, docks and tab bar
/// excluded) through `Jwm::set_client_maximized`.
pub(crate) fn snap_rect(monitor: Rect, direction: SnapDirection) -> Rect {
    match direction {
        SnapDirection::Left => Rect::new(monitor.x, monitor.y, monitor.w / 2, monitor.h),
        SnapDirection::Right => Rect::new(
            monitor.x + monitor.w / 2,
            monitor.y,
            monitor.w / 2,
            monitor.h,
        ),
        SnapDirection::Maximize => monitor,
        SnapDirection::TopLeft => Rect::new(monitor.x, monitor.y, monitor.w / 2, monitor.h / 2),
        SnapDirection::TopRight => Rect::new(
            monitor.x + monitor.w / 2,
            monitor.y,
            monitor.w / 2,
            monitor.h / 2,
        ),
        SnapDirection::BottomLeft => Rect::new(
            monitor.x,
            monitor.y + monitor.h / 2,
            monitor.w / 2,
            monitor.h / 2,
        ),
        SnapDirection::BottomRight => Rect::new(
            monitor.x + monitor.w / 2,
            monitor.y + monitor.h / 2,
            monitor.w / 2,
            monitor.h / 2,
        ),
    }
}

/// Map the near-edge booleans of a drop to its classic float-snap direction.
/// The corner hitbox is the overlap of the two near-edge bands: near one
/// horizontal and one vertical edge plans that corner's quarter, a lone edge
/// keeps the round-13 behavior (left/right halves, top maximizes, the bottom
/// edge has no zone). On a monitor smaller than twice the snap distance three
/// or four edges read "near" at once — the check order then keeps the
/// round-13 priority, left before right and top before bottom.
fn float_snap_direction(
    near_left: bool,
    near_right: bool,
    near_top: bool,
    near_bottom: bool,
) -> Option<SnapDirection> {
    if near_left && near_top {
        Some(SnapDirection::TopLeft)
    } else if near_right && near_top {
        Some(SnapDirection::TopRight)
    } else if near_left && near_bottom {
        Some(SnapDirection::BottomLeft)
    } else if near_right && near_bottom {
        Some(SnapDirection::BottomRight)
    } else if near_left {
        Some(SnapDirection::Left)
    } else if near_right {
        Some(SnapDirection::Right)
    } else if near_top {
        Some(SnapDirection::Maximize)
    } else {
        None
    }
}

/// What a drop at the current pointer position would do.
pub(crate) enum DragSnapPlan {
    /// Keep the window floating and give it this rect (float layout, or a
    /// window that was floating before the drag started).
    Float { rect: Rect },
    /// Top-edge float drop: a real maximize (BOTH, MaximizeOrigin::User) of the
    /// dragged client on `mon_key`; `rect` is that monitor's outer work area,
    /// used only as the preview.
    Maximize { mon_key: MonitorKey, rect: Rect },
    /// Re-tile the window into the monitor's layout at this position in the
    /// tiled client order.
    Attach {
        mon_key: MonitorKey,
        tiled_index: usize,
        rect: Rect,
    },
    /// Scrolling layout: insert the window as a new column at this index.
    AttachScrolling {
        mon_key: MonitorKey,
        column_index: usize,
        rect: Rect,
    },
}

impl DragSnapPlan {
    pub(crate) fn preview_rect(&self) -> Rect {
        match self {
            DragSnapPlan::Float { rect }
            | DragSnapPlan::Maximize { rect, .. }
            | DragSnapPlan::Attach { rect, .. }
            | DragSnapPlan::AttachScrolling { rect, .. } => *rect,
        }
    }

    /// The monitor a `Maximize` plan maximizes on. This module is private to
    /// `layout`, so policy tests elsewhere cannot name the variant.
    #[cfg(test)]
    pub(crate) fn maximize_monitor(&self) -> Option<MonitorKey> {
        match self {
            DragSnapPlan::Maximize { mon_key, .. } => Some(*mon_key),
            _ => None,
        }
    }
}

/// Among candidate (index, rect) slots, pick the one the pointer is in.
/// Containing rects win over non-containing ones; among containing rects the
/// smallest wins (fibonacci/tatami nest small cells inside the master's
/// bounding area, deck stacks previews), otherwise nearest center wins.
fn pick_best_candidate(candidates: &[(usize, Rect)], px: i32, py: i32) -> Option<(usize, Rect)> {
    candidates.iter().copied().min_by_key(|&(_, r)| {
        let contains = px >= r.x && px < r.x + r.w.max(1) && py >= r.y && py < r.y + r.h.max(1);
        if contains {
            (r.w as i64) * (r.h as i64)
        } else {
            let cx = r.x + r.w / 2;
            let cy = r.y + r.h / 2;
            let dx = (px - cx) as i64;
            let dy = (py - cy) as i64;
            // Never let a far center beat any containing rect.
            i64::MAX / 2 + dx * dx + dy * dy
        }
    })
}

/// The layout calculators that share the plain (params, clients) signature.
fn layout_calc_fn(
    layout: &LayoutEnum,
) -> Option<fn(&LayoutParams, &[LayoutClient<ClientKey>]) -> Vec<LayoutResult<ClientKey>>> {
    Some(match *layout {
        LayoutEnum::TILE => core_layout::calculate_tile,
        LayoutEnum::MONOCLE => core_layout::calculate_monocle,
        LayoutEnum::FIBONACCI => core_layout::calculate_fibonacci,
        LayoutEnum::CENTERED_MASTER => core_layout::calculate_centered_master,
        LayoutEnum::BSTACK => core_layout::calculate_bstack,
        LayoutEnum::GRID => core_layout::calculate_grid,
        LayoutEnum::DECK => core_layout::calculate_deck,
        LayoutEnum::THREE_COL => core_layout::calculate_three_col,
        LayoutEnum::TATAMI => core_layout::calculate_tatami,
        LayoutEnum::FULLSCREEN => core_layout::calculate_fullscreen,
        LayoutEnum::VSTACK => core_layout::calculate_vstack,
        _ => return None,
    })
}

impl Jwm {
    /// Plan what releasing the selected client's drag at root position
    /// (px, py) on `mon_key` should do. See [`Self::plan_drag_snap_for`].
    pub(crate) fn plan_drag_snap(
        &self,
        mon_key: MonitorKey,
        px: i32,
        py: i32,
    ) -> Option<DragSnapPlan> {
        self.plan_drag_snap_for(self.get_selected_client_key()?, mon_key, px, py)
    }

    /// Plan what releasing `drag_key`'s drag at root position (px, py) on
    /// `mon_key` should do. Returns None when the pointer is outside every
    /// snap zone, i.e. the window simply stays floating where the user
    /// dropped it, when the client no longer exists, or when `mon_key` is
    /// behind a lock shade.
    ///
    /// Keyed by the dragged client rather than the selection: the keyboard
    /// stays free during a drag, and a mapped window or an activation can
    /// take focus from the window the user is still holding. The drop must
    /// still snap the held window, not the newly selected one.
    pub(crate) fn plan_drag_snap_for(
        &self,
        drag_key: ClientKey,
        mon_key: MonitorKey,
        px: i32,
        py: i32,
    ) -> Option<DragSnapPlan> {
        self.state.clients.get(drag_key)?;
        // A drag begun on an unlocked output can be released over a shade.
        // Every snap moves the window onto that monitor, and maximize and
        // attach also select it there: focus would land on a window nobody
        // can see. No zone, and no preview, over a locked monitor.
        if self.monitor_key_is_locked(mon_key) {
            return None;
        }
        let snap_dist = CONFIG.load().snap() as i32;

        let (mx, my, mw, mh) = self.monitor_rect(mon_key);
        let mw = mw as i32;
        let mh = mh as i32;

        let near_left = px - mx < snap_dist;
        let near_right = (mx + mw) - px < snap_dist;
        let near_top = py - my < snap_dist;
        let near_bottom = (my + mh) - py < snap_dist;

        let attach_eligible = self
            .state
            .clients
            .get(drag_key)
            .map(|c| {
                // Mirror reclaim_drag_floating: only drag-induced floats go
                // back to tiling; design floats keep the classic float snap.
                c.state.is_drag_floating
                    && !c.state.is_fixed
                    && !c.state.is_fullscreen
                    && !c.state.is_pip
                    && !c.state.is_dock
                    && !c.state.is_sticky
                    && !c.state.is_swallowed
            })
            .unwrap_or(false);

        let layout = self.state.monitors.get(mon_key).map(|m| (*m.lt).clone())?;

        if attach_eligible && layout.is_tile() {
            // All four edges (and thus corners) are attach zones.
            if !(near_left || near_right || near_top || near_bottom) {
                return None;
            }
            if layout == LayoutEnum::SCROLLING {
                return self.plan_scrolling_attach(mon_key, px);
            }
            return self.plan_layout_attach(mon_key, drag_key, &layout, px, py);
        }

        // Classic float snap: left/right halves, top edge maximizes, and a
        // corner drop (near one horizontal and one vertical edge) plans that
        // corner's quarter. The bottom edge alone has no zone.
        let direction = float_snap_direction(near_left, near_right, near_top, near_bottom)?;
        if direction == SnapDirection::Maximize {
            // Only a window a user maximize can take gets the zone, so the
            // preview never promises what the drop will not do: a fixed-size
            // window or a dock refuses the maximize, and a PiP window would
            // stay small with the maximized state published on it (and its
            // PiP return slot retargeted to the work area). Those drops
            // leave the window where it was released.
            let maximizable = self.state.clients.get(drag_key).is_some_and(|c| {
                !c.state.is_pip && !c.state.is_fixed && !c.state.is_dock && !c.state.is_fullscreen
            });
            if !maximizable {
                return None;
            }
            // A real maximize, so the window fills the work area (not the
            // bar's pixels), carries the EWMH/xdg maximized state, and a
            // later unmaximize returns it to the pre-drop rect. The preview
            // is the outer work area the maximized window will cover.
            return Some(DragSnapPlan::Maximize {
                mon_key,
                rect: self.maximize_work_area(mon_key)?,
            });
        }
        let rect = snap_rect(Rect::new(mx, my, mw, mh), direction);
        Some(DragSnapPlan::Float { rect })
    }

    /// Plan a reorder drop for a window that stayed tiled through its drag:
    /// the whole monitor is a drop zone and the plan is always an attach at
    /// the layout slot under the pointer. Returns None when the monitor's
    /// layout is not a tiling one, the client cannot be re-slotted, or the
    /// monitor is behind a lock shade (see [`Self::plan_drag_snap_for`]).
    pub(crate) fn plan_drag_reorder(
        &self,
        drag_key: ClientKey,
        mon_key: MonitorKey,
        px: i32,
        py: i32,
    ) -> Option<DragSnapPlan> {
        if self.monitor_key_is_locked(mon_key) {
            return None;
        }
        let eligible = self
            .state
            .clients
            .get(drag_key)
            .map(|c| {
                !c.state.is_floating
                    && !c.state.is_fixed
                    && !c.state.is_fullscreen
                    && !c.state.is_pip
                    && !c.state.is_dock
                    && !c.state.is_sticky
                    && !c.state.is_swallowed
            })
            .unwrap_or(false);
        if !eligible {
            return None;
        }

        let layout = self.state.monitors.get(mon_key).map(|m| (*m.lt).clone())?;
        if !layout.is_tile() {
            return None;
        }
        if layout == LayoutEnum::SCROLLING {
            return self.plan_scrolling_attach(mon_key, px);
        }
        self.plan_layout_attach(mon_key, drag_key, &layout, px, py)
    }

    /// Simulate the layout with the dragged client inserted at every index of
    /// the tiled order and keep the index whose rect lands under the pointer.
    fn plan_layout_attach(
        &self,
        mon_key: MonitorKey,
        drag_key: ClientKey,
        layout: &LayoutEnum,
        px: i32,
        py: i32,
    ) -> Option<DragSnapPlan> {
        let calc = layout_calc_fn(layout)?;

        // Simulate without the dragged client: it is absent already when the
        // drag floated it, and must be pulled out for a reorder drag where it
        // is still tiled.
        let tiled: Vec<(ClientKey, f32, i32)> = self
            .collect_tileable_clients(mon_key)
            .into_iter()
            .filter(|&(key, _, _)| key != drag_key)
            .collect();
        let count = tiled.len() + 1;

        let (wx, wy, ww, wh, m_fact, n_master, _, _) = self.get_monitor_info(mon_key);
        let screen_area = self
            .monitor_work_area(mon_key)
            .unwrap_or(Rect::new(wx, wy, ww, wh));

        // Mirror apply_smart_borders for a single-window monitor.
        let cfg = CONFIG.load();
        let border_w = if count == 1 {
            0
        } else {
            cfg.border_px() as i32
        };
        let monitor_gap = self
            .state
            .monitors
            .get(mon_key)
            .map(|m| m.layout.gap)
            .unwrap_or_else(|| cfg.gap_px() as i32);
        let gap = if count == 1 { 0 } else { monitor_gap };

        let params = LayoutParams {
            screen_area,
            n_master,
            m_fact,
            gap,
        };

        let drag_factor = self
            .state
            .clients
            .get(drag_key)
            .map(|c| c.state.client_fact)
            .unwrap_or(1.0);

        // vstack always shows the focused client as master, and after the
        // drop the dragged client is the focused one — only index 0 is honest.
        let candidate_indices: Vec<usize> = if *layout == LayoutEnum::VSTACK {
            vec![0]
        } else {
            (0..count).collect()
        };

        let mut candidates: Vec<(usize, Rect)> = Vec::with_capacity(candidate_indices.len());
        for index in candidate_indices {
            let mut sim: Vec<LayoutClient<ClientKey>> = Vec::with_capacity(count);
            for &(key, factor, _) in &tiled {
                sim.push(LayoutClient {
                    key,
                    factor,
                    border_w,
                });
            }
            sim.insert(
                index.min(sim.len()),
                LayoutClient {
                    key: drag_key,
                    factor: drag_factor,
                    border_w,
                },
            );
            if let Some(result) = calc(&params, &sim).into_iter().find(|r| r.key == drag_key) {
                candidates.push((index, result.rect));
            }
        }

        pick_best_candidate(&candidates, px, py).map(|(tiled_index, rect)| DragSnapPlan::Attach {
            mon_key,
            tiled_index,
            rect,
        })
    }

    /// Scrolling layout: the drop point picks a column boundary; the window
    /// becomes its own column there.
    fn plan_scrolling_attach(&self, mon_key: MonitorKey, px: i32) -> Option<DragSnapPlan> {
        let (wx, wy, ww, wh, m_fact, _, _, _) = self.get_monitor_info(mon_key);
        let area = self
            .monitor_work_area(mon_key)
            .unwrap_or(Rect::new(wx, wy, ww, wh));

        // Visible x-span of each column, from its first client's live geometry.
        let spans: Vec<(i32, i32)> = self
            .scrolling_state_for_monitor(mon_key)
            .map(|state| {
                state
                    .columns
                    .iter()
                    .filter_map(|col| col.first())
                    .filter_map(|&key| self.state.clients.get(key))
                    .map(|c| (c.geometry.x, c.geometry.x + c.geometry.w))
                    .collect()
            })
            .unwrap_or_default();

        let column_index = spans
            .iter()
            .filter(|&&(left, right)| (left + right) / 2 < px)
            .count();

        let strip_w = ((area.w as f32) * m_fact.clamp(0.1, 1.0)).max(1.0) as i32;
        let boundary_x = if column_index < spans.len() {
            spans[column_index].0
        } else {
            spans.last().map(|&(_, right)| right).unwrap_or(area.x)
        };
        let rect = Rect::new(
            (boundary_x - strip_w / 2).clamp(area.x, (area.x + area.w - strip_w).max(area.x)),
            area.y,
            strip_w,
            area.h,
        );

        Some(DragSnapPlan::AttachScrolling {
            mon_key,
            column_index,
            rect,
        })
    }

    /// Execute a drop plan produced by [`Self::plan_drag_snap`].
    pub(crate) fn apply_drag_snap(
        &mut self,
        backend: &mut dyn Backend,
        drag_key: ClientKey,
        plan: DragSnapPlan,
    ) {
        // The planners refuse a locked monitor, but a plan made before a lock
        // landed must not select a monitor behind the shade either: skip it,
        // as if the drop had been outside every zone.
        if let DragSnapPlan::Maximize { mon_key, .. }
        | DragSnapPlan::Attach { mon_key, .. }
        | DragSnapPlan::AttachScrolling { mon_key, .. } = &plan
            && self.monitor_key_is_locked(*mon_key)
        {
            info!(
                "[apply_drag_snap] monitor {:?} is locked; dropped client {:?} stays put",
                mon_key, drag_key
            );
            return;
        }
        match plan {
            DragSnapPlan::Float { rect } => {
                self.unmaximize_dropped_client(backend, drag_key);
                self.apply_float_snap_rect(backend, drag_key, rect);
            }
            DragSnapPlan::Maximize { mon_key, .. } => {
                // PiP windows ignore maximize, as in `snap_window`. The
                // planner offers them no zone, but a plan made before the
                // window entered PiP can still arrive here.
                if self
                    .state
                    .clients
                    .get(drag_key)
                    .is_none_or(|c| c.state.is_pip)
                {
                    return;
                }
                let source_mon = self.state.clients.get(drag_key).and_then(|c| c.mon);
                if source_mon != Some(mon_key) {
                    self.sendmon(backend, Some(drag_key), Some(mon_key));
                    // sendmon refocuses the source monitor; the dropped
                    // window is the one the user is holding, so keep it
                    // selected on its new monitor, as attach does.
                    self.state.sel_mon = Some(mon_key);
                    let _ = self.focus(backend, Some(drag_key));
                }
                if let Err(e) = self.set_client_maximized(
                    backend,
                    drag_key,
                    MaximizeAxes::BOTH,
                    MaximizeOrigin::User,
                ) {
                    error!(
                        "[apply_drag_snap] could not maximize dropped client {:?}: {}",
                        drag_key, e
                    );
                }
            }
            DragSnapPlan::Attach {
                mon_key,
                tiled_index,
                ..
            } => {
                self.unmaximize_dropped_client(backend, drag_key);
                self.attach_dragged_client(backend, drag_key, mon_key, |jwm| {
                    // Anchor before mutating: the monitor_clients position of
                    // the tiled client currently holding the target index.
                    // The dragged client is skipped to mirror the plan's
                    // simulation (it is still tiled during a reorder drag).
                    jwm.collect_tileable_clients(mon_key)
                        .into_iter()
                        .filter(|&(key, _, _)| key != drag_key)
                        .nth(tiled_index)
                        .map(|(key, _, _)| key)
                });
            }
            DragSnapPlan::AttachScrolling {
                mon_key,
                column_index,
                ..
            } => {
                self.unmaximize_dropped_client(backend, drag_key);
                let width_factor = self
                    .scrolling_default_column_width_for_client(drag_key)
                    .unwrap_or(1.0);
                self.attach_dragged_client(backend, drag_key, mon_key, |_| None);
                if let Some(state) = self.scrolling_state_for_monitor_mut_or_default(mon_key) {
                    state.ensure_column_metadata();
                    for col in &mut state.columns {
                        col.retain(|&k| k != drag_key);
                    }
                    state.retain_non_empty_columns();
                    let index = column_index.min(state.columns.len());
                    state.columns.insert(index, vec![drag_key]);
                    state.column_width_factors.insert(index, width_factor);
                    state.focused_clients.insert(index, Some(drag_key));
                    state.focused_column = Some(index);
                    self.arrange(backend, Some(mon_key));
                }
            }
        }
    }

    /// Defensive: drag activation already unmaximizes in place, but a drop
    /// that moves or re-tiles a window must never leave maximize owning its
    /// geometry, or the next arrange would refit it back to the work area.
    fn unmaximize_dropped_client(&mut self, backend: &mut dyn Backend, drag_key: ClientKey) {
        let realized = self
            .state
            .clients
            .get(drag_key)
            .is_some_and(|c| c.state.is_maximize_realized());
        if realized && let Err(e) = self.unmaximize_in_place(backend, drag_key) {
            error!(
                "[apply_drag_snap] could not unmaximize dropped client {:?}: {}",
                drag_key, e
            );
        }
    }

    /// Move/resize a floating client to a snap rect (outer geometry including
    /// the border) through the same hint-respecting path a mouse drop uses.
    fn apply_float_snap_rect(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        rect: Rect,
    ) {
        let bw = self
            .state
            .clients
            .get(client_key)
            .map(|c| c.geometry.border_w)
            .unwrap_or(0);
        self.resize_client(
            backend,
            client_key,
            rect.x + bw,
            rect.y + bw,
            rect.w - 2 * bw,
            rect.h - 2 * bw,
            false,
        );
    }

    /// `snap_window` command: the keyboard/IPC form of dropping a dragged
    /// window on a monitor edge — snap the focused window to a half or a
    /// corner quarter, using the same geometry as the mouse float snap, or
    /// toggle a real maximize with `maximize`.
    ///
    /// `maximize` requests `requested_axes(current, Toggle, BOTH)` with
    /// origin `User`: a restored window fills its monitor's work area and
    /// publishes the maximized state; a maximized one returns to its
    /// pre-maximize rect. PiP windows ignore it, like `togglemaximize`.
    /// Any other direction on a maximized window first drops maximize in
    /// place, so the snapped rect is not refitted away by the next arrange.
    ///
    /// Snapping is a floating-geometry operation: a focused *tiled* window is
    /// a deliberate no-op (tile it to floating first with `togglefloating`),
    /// as are a fullscreen window, an empty focus, or a missing monitor. A
    /// malformed direction argument is an error, never a no-op.
    pub(crate) fn snap_window(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let direction = SnapDirection::from_arg(arg)?;

        let Some(client_key) = self.get_selected_client_key() else {
            return Ok(());
        };
        let Some(client) = self.state.clients.get(client_key) else {
            return Ok(());
        };
        if !client.state.is_floating || client.state.is_fullscreen {
            return Ok(());
        }
        let Some(mon_key) = client.mon.or(self.state.sel_mon) else {
            return Ok(());
        };

        if direction == SnapDirection::Maximize {
            if client.state.is_pip {
                return Ok(());
            }
            let next = requested_axes(
                client.state.maximized_axes(),
                NetWmAction::Toggle,
                MaximizeAxes::BOTH,
            );
            info!(
                "[snap_window] maximize toggle on client {:?} -> {:?}",
                client_key, next
            );
            self.set_client_maximized(backend, client_key, next, MaximizeOrigin::User)?;
            return Ok(());
        }

        if client.state.is_maximize_realized() {
            self.unmaximize_in_place(backend, client_key)?;
        }

        let (mx, my, mw, mh) = self.monitor_rect(mon_key);
        let rect = snap_rect(Rect::new(mx, my, mw as i32, mh as i32), direction);
        info!(
            "[snap_window] snapping client {:?} {} -> {:?}",
            client_key,
            direction.as_name(),
            rect
        );
        self.apply_float_snap_rect(backend, client_key, rect);

        // Remember the snapped geometry as the floating geometry a later
        // tile→float toggle restores — the state-side half of what
        // sync_focused_floating_geometry does after a mouse drop (a backend
        // round-trip here could read the animation's starting rect instead).
        if let Some(client) = self.state.clients.get_mut(client_key) {
            client.geometry.floating_x = client.geometry.x;
            client.geometry.floating_y = client.geometry.y;
            client.geometry.floating_w = client.geometry.w;
            client.geometry.floating_h = client.geometry.h;
        }
        Ok(())
    }

    /// Shared attach tail: move to the target monitor if needed, drop the
    /// floating state, put the client back among the tiled group (before
    /// `anchor`, or at the tiled/floating boundary), then re-arrange.
    fn attach_dragged_client(
        &mut self,
        backend: &mut dyn Backend,
        drag_key: ClientKey,
        mon_key: MonitorKey,
        anchor_of: impl Fn(&Self) -> Option<ClientKey>,
    ) {
        if self.state.clients.get(drag_key).and_then(|c| c.mon) != Some(mon_key) {
            self.sendmon(backend, Some(drag_key), Some(mon_key));
        }
        let anchor = anchor_of(self);

        if let Some(client) = self.state.clients.get_mut(drag_key) {
            // Remember where the user left it so toggling back to floating
            // restores the drop position, matching reclaim_drag_floating.
            // A reorder drag never floated the window, so its tile rect must
            // not clobber the remembered floating geometry.
            if client.state.is_floating {
                client.geometry.floating_x = client.geometry.x;
                client.geometry.floating_y = client.geometry.y;
                client.geometry.floating_w = client.geometry.w;
                client.geometry.floating_h = client.geometry.h;
            }
            client.state.is_floating = false;
            client.state.is_drag_floating = false;
        }

        let insert_pos = {
            let list = self.get_monitor_clients(mon_key);
            let position_of = |target: ClientKey| {
                list.iter()
                    .filter(|&&k| k != drag_key)
                    .position(|&k| k == target)
            };
            match anchor.and_then(position_of) {
                Some(pos) => pos,
                // Past the last tiled client: the tiled/floating boundary.
                None => list
                    .iter()
                    .filter(|&&k| k != drag_key)
                    .position(|&k| {
                        self.state
                            .clients
                            .get(k)
                            .map(|c| c.state.is_floating)
                            .unwrap_or(false)
                    })
                    .unwrap_or_else(|| list.iter().filter(|&&k| k != drag_key).count()),
            }
        };
        if let Some(list) = self.state.monitor_clients.get_mut(mon_key) {
            list.retain(|&k| k != drag_key);
            list.insert(insert_pos.min(list.len()), drag_key);
        }

        info!(
            "[drag_attach] re-tiled client {:?} at slot {} on monitor {:?}",
            drag_key, insert_pos, mon_key
        );

        self.state.sel_mon = Some(mon_key);
        let _ = self.focus(backend, Some(drag_key));
        self.arrange(backend, Some(mon_key));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_inside_a_slot_wins_over_nearer_centers() {
        // Slot 1 contains the pointer; slot 0's center is closer.
        let candidates = [
            (0usize, Rect::new(0, 0, 100, 100)),
            (1usize, Rect::new(100, 0, 400, 400)),
        ];
        let best = pick_best_candidate(&candidates, 110, 90).unwrap();
        assert_eq!(best.0, 1);
    }

    #[test]
    fn nested_slots_prefer_the_smallest_containing_rect() {
        // Fibonacci-style nesting: the small bottom-right cell sits inside
        // the area a shallower simulation assigns to the same drop point.
        let candidates = [
            (0usize, Rect::new(0, 0, 800, 600)),
            (1usize, Rect::new(400, 300, 400, 300)),
            (2usize, Rect::new(600, 450, 200, 150)),
        ];
        let best = pick_best_candidate(&candidates, 700, 500).unwrap();
        assert_eq!(best.0, 2);
    }

    #[test]
    fn no_containing_rect_falls_back_to_nearest_center() {
        let candidates = [
            (0usize, Rect::new(0, 0, 10, 10)),
            (1usize, Rect::new(500, 500, 10, 10)),
        ];
        let best = pick_best_candidate(&candidates, 490, 490).unwrap();
        assert_eq!(best.0, 1);
    }

    #[test]
    fn empty_candidates_yield_no_plan() {
        assert!(pick_best_candidate(&[], 10, 10).is_none());
    }

    /// The user-facing scenario: with three windows tiled in fibonacci,
    /// dropping a fourth at the bottom-right corner must pick the insertion
    /// index whose simulated rect is the small bottom-right spiral cell.
    #[test]
    fn fibonacci_bottom_right_drop_lands_in_the_spiral_tail() {
        let params = LayoutParams {
            screen_area: Rect::new(0, 0, 1920, 1080),
            n_master: 1,
            m_fact: 0.55,
            gap: 5,
        };
        let tiled: Vec<u32> = vec![1, 2, 3];
        let drag: u32 = 99;

        let mut candidates = Vec::new();
        for index in 0..=tiled.len() {
            let mut sim: Vec<LayoutClient<u32>> = tiled
                .iter()
                .map(|&key| LayoutClient {
                    key,
                    factor: 1.0,
                    border_w: 1,
                })
                .collect();
            sim.insert(
                index,
                LayoutClient {
                    key: drag,
                    factor: 1.0,
                    border_w: 1,
                },
            );
            let rect = core_layout::calculate_fibonacci(&params, &sim)
                .into_iter()
                .find(|r| r.key == drag)
                .unwrap()
                .rect;
            candidates.push((index, rect));
        }

        let (index, rect) = pick_best_candidate(&candidates, 1900, 1060).unwrap();
        // Appending at the tail is the only insertion that puts the dragged
        // window in the spiral's last (bottom-right) cell.
        assert_eq!(index, tiled.len());
        assert!(rect.x >= 960, "cell should sit in the right half: {rect:?}");
        assert!(
            rect.y >= 540,
            "cell should sit in the bottom half: {rect:?}"
        );

        // And a drop on the left edge must instead claim the master slot.
        let (index, rect) = pick_best_candidate(&candidates, 5, 540).unwrap();
        assert_eq!(index, 0);
        assert_eq!(rect.x, params.screen_area.x + params.gap);
    }

    /// Pin the extraction: these are the exact rects the inline math in
    /// `plan_drag_snap` produced before `snap_rect` existed, for the
    /// geometries that exercise the math (ultrawide, small, offset origin),
    /// plus the corner quarters added on top of the same integer rule.
    #[test]
    fn snap_rect_matches_the_classic_mouse_drop_geometry() {
        // Plain 1080p at the origin.
        let mon = Rect::new(0, 0, 1920, 1080);
        assert_eq!(
            snap_rect(mon, SnapDirection::Left),
            Rect::new(0, 0, 960, 1080)
        );
        assert_eq!(
            snap_rect(mon, SnapDirection::Right),
            Rect::new(960, 0, 960, 1080)
        );
        assert_eq!(snap_rect(mon, SnapDirection::Maximize), mon);
        assert_eq!(
            snap_rect(mon, SnapDirection::TopLeft),
            Rect::new(0, 0, 960, 540)
        );
        assert_eq!(
            snap_rect(mon, SnapDirection::TopRight),
            Rect::new(960, 0, 960, 540)
        );
        assert_eq!(
            snap_rect(mon, SnapDirection::BottomLeft),
            Rect::new(0, 540, 960, 540)
        );
        assert_eq!(
            snap_rect(mon, SnapDirection::BottomRight),
            Rect::new(960, 540, 960, 540)
        );

        // Ultrawide.
        let mon = Rect::new(0, 0, 3440, 1440);
        assert_eq!(
            snap_rect(mon, SnapDirection::Left),
            Rect::new(0, 0, 1720, 1440)
        );
        assert_eq!(
            snap_rect(mon, SnapDirection::Right),
            Rect::new(1720, 0, 1720, 1440)
        );
        assert_eq!(snap_rect(mon, SnapDirection::Maximize), mon);
        assert_eq!(
            snap_rect(mon, SnapDirection::TopLeft),
            Rect::new(0, 0, 1720, 720)
        );
        assert_eq!(
            snap_rect(mon, SnapDirection::TopRight),
            Rect::new(1720, 0, 1720, 720)
        );
        assert_eq!(
            snap_rect(mon, SnapDirection::BottomLeft),
            Rect::new(0, 720, 1720, 720)
        );
        assert_eq!(
            snap_rect(mon, SnapDirection::BottomRight),
            Rect::new(1720, 720, 1720, 720)
        );

        // Small monitor.
        let mon = Rect::new(0, 0, 1024, 600);
        assert_eq!(
            snap_rect(mon, SnapDirection::Left),
            Rect::new(0, 0, 512, 600)
        );
        assert_eq!(
            snap_rect(mon, SnapDirection::Right),
            Rect::new(512, 0, 512, 600)
        );
        assert_eq!(snap_rect(mon, SnapDirection::Maximize), mon);
        assert_eq!(
            snap_rect(mon, SnapDirection::TopLeft),
            Rect::new(0, 0, 512, 300)
        );
        assert_eq!(
            snap_rect(mon, SnapDirection::TopRight),
            Rect::new(512, 0, 512, 300)
        );
        assert_eq!(
            snap_rect(mon, SnapDirection::BottomLeft),
            Rect::new(0, 300, 512, 300)
        );
        assert_eq!(
            snap_rect(mon, SnapDirection::BottomRight),
            Rect::new(512, 300, 512, 300)
        );

        // Non-zero origin: a right-hand output in a dual-monitor setup.
        let mon = Rect::new(1920, 40, 2560, 1440);
        assert_eq!(
            snap_rect(mon, SnapDirection::Left),
            Rect::new(1920, 40, 1280, 1440)
        );
        assert_eq!(
            snap_rect(mon, SnapDirection::Right),
            Rect::new(3200, 40, 1280, 1440)
        );
        assert_eq!(snap_rect(mon, SnapDirection::Maximize), mon);
        assert_eq!(
            snap_rect(mon, SnapDirection::TopLeft),
            Rect::new(1920, 40, 1280, 720)
        );
        assert_eq!(
            snap_rect(mon, SnapDirection::TopRight),
            Rect::new(3200, 40, 1280, 720)
        );
        assert_eq!(
            snap_rect(mon, SnapDirection::BottomLeft),
            Rect::new(1920, 760, 1280, 720)
        );
        assert_eq!(
            snap_rect(mon, SnapDirection::BottomRight),
            Rect::new(3200, 760, 1280, 720)
        );
    }

    #[test]
    fn snap_rect_floors_odd_dimensions_the_same_for_halves_and_quarters() {
        // Odd widths floor the half and start the right half there, leaving
        // the last column uncovered — the pre-extraction integer math.
        // Quarters apply the same floor to both dimensions, so an odd height
        // likewise leaves the last row uncovered.
        let mon = Rect::new(0, 0, 1921, 1080);
        assert_eq!(
            snap_rect(mon, SnapDirection::Left),
            Rect::new(0, 0, 960, 1080)
        );
        assert_eq!(
            snap_rect(mon, SnapDirection::Right),
            Rect::new(960, 0, 960, 1080)
        );
        assert_eq!(
            snap_rect(mon, SnapDirection::TopLeft),
            Rect::new(0, 0, 960, 540)
        );
        assert_eq!(
            snap_rect(mon, SnapDirection::BottomRight),
            Rect::new(960, 540, 960, 540)
        );

        let mon = Rect::new(0, 0, 1920, 1081);
        assert_eq!(
            snap_rect(mon, SnapDirection::TopLeft),
            Rect::new(0, 0, 960, 540)
        );
        assert_eq!(
            snap_rect(mon, SnapDirection::BottomLeft),
            Rect::new(0, 540, 960, 540)
        );
        assert_eq!(
            snap_rect(mon, SnapDirection::BottomRight),
            Rect::new(960, 540, 960, 540)
        );
    }

    #[test]
    fn snap_direction_names_are_case_insensitive() {
        assert_eq!(SnapDirection::from_name("left"), Some(SnapDirection::Left));
        assert_eq!(
            SnapDirection::from_name("Right"),
            Some(SnapDirection::Right)
        );
        assert_eq!(
            SnapDirection::from_name("MAXIMIZE"),
            Some(SnapDirection::Maximize)
        );
        assert_eq!(
            SnapDirection::from_name("top-left"),
            Some(SnapDirection::TopLeft)
        );
        assert_eq!(
            SnapDirection::from_name("Top-Right"),
            Some(SnapDirection::TopRight)
        );
        assert_eq!(
            SnapDirection::from_name("BOTTOM-LEFT"),
            Some(SnapDirection::BottomLeft)
        );
        assert_eq!(
            SnapDirection::from_name("bottom-right"),
            Some(SnapDirection::BottomRight)
        );
    }

    #[test]
    fn snap_direction_rejects_unknown_names() {
        // No plain top/bottom halves exist, and only the kebab corner names
        // are accepted; nothing silently maps to a half or quarter.
        for name in [
            "",
            "bottom",
            "up",
            "max",
            "full",
            "quarter",
            "top",
            "topleft",
            "top_left",
            "left-top",
            "bottomright",
        ] {
            assert_eq!(SnapDirection::from_name(name), None, "accepted {name:?}");
        }
    }

    #[test]
    fn corner_drops_plan_quarters_and_lone_edges_keep_the_classic_directions() {
        // A lone edge is byte-identical to round 13, including the bottom
        // edge having no zone at all.
        assert_eq!(
            float_snap_direction(true, false, false, false),
            Some(SnapDirection::Left)
        );
        assert_eq!(
            float_snap_direction(false, true, false, false),
            Some(SnapDirection::Right)
        );
        assert_eq!(
            float_snap_direction(false, false, true, false),
            Some(SnapDirection::Maximize)
        );
        assert_eq!(float_snap_direction(false, false, false, true), None);
        assert_eq!(float_snap_direction(false, false, false, false), None);

        // The corner hitbox is the overlap of the two near-edge bands: near
        // one horizontal and one vertical edge plans that corner's quarter.
        assert_eq!(
            float_snap_direction(true, false, true, false),
            Some(SnapDirection::TopLeft)
        );
        assert_eq!(
            float_snap_direction(false, true, true, false),
            Some(SnapDirection::TopRight)
        );
        assert_eq!(
            float_snap_direction(true, false, false, true),
            Some(SnapDirection::BottomLeft)
        );
        assert_eq!(
            float_snap_direction(false, true, false, true),
            Some(SnapDirection::BottomRight)
        );
    }

    #[test]
    fn float_snap_direction_keeps_left_then_top_priority_on_tiny_monitors() {
        // A monitor smaller than twice the snap distance can read three or
        // four edges "near" at once; the check order then keeps the round-13
        // priority, left before right and top before bottom.
        assert_eq!(
            float_snap_direction(true, true, true, false),
            Some(SnapDirection::TopLeft)
        );
        assert_eq!(
            float_snap_direction(true, true, false, true),
            Some(SnapDirection::BottomLeft)
        );
        assert_eq!(
            float_snap_direction(true, false, true, true),
            Some(SnapDirection::TopLeft)
        );
        assert_eq!(
            float_snap_direction(false, true, true, true),
            Some(SnapDirection::TopRight)
        );
        assert_eq!(
            float_snap_direction(true, true, true, true),
            Some(SnapDirection::TopLeft)
        );
    }

    #[test]
    fn snap_window_argument_must_be_exactly_one_direction_string() {
        assert_eq!(
            SnapDirection::from_arg(&WMArgEnum::StringVec(vec!["left".into()])).unwrap(),
            SnapDirection::Left
        );
        assert_eq!(
            SnapDirection::from_arg(&WMArgEnum::StringVec(vec!["Right".into()])).unwrap(),
            SnapDirection::Right
        );
        assert_eq!(
            SnapDirection::from_arg(&WMArgEnum::StringVec(vec!["maximize".into()])).unwrap(),
            SnapDirection::Maximize
        );
        assert_eq!(
            SnapDirection::from_arg(&WMArgEnum::StringVec(vec!["top-left".into()])).unwrap(),
            SnapDirection::TopLeft
        );
        assert_eq!(
            SnapDirection::from_arg(&WMArgEnum::StringVec(vec!["Top-Right".into()])).unwrap(),
            SnapDirection::TopRight
        );
        assert_eq!(
            SnapDirection::from_arg(&WMArgEnum::StringVec(vec!["bottom-left".into()])).unwrap(),
            SnapDirection::BottomLeft
        );
        assert_eq!(
            SnapDirection::from_arg(&WMArgEnum::StringVec(vec!["BOTTOM-RIGHT".into()])).unwrap(),
            SnapDirection::BottomRight
        );

        // Missing, extra, non-string, and unknown arguments are all errors.
        for arg in [
            WMArgEnum::Int(0),
            WMArgEnum::UInt(2),
            WMArgEnum::Float(0.5),
            WMArgEnum::StringVec(Vec::new()),
            WMArgEnum::StringVec(vec!["left".into(), "right".into()]),
            WMArgEnum::StringVec(vec!["bottom".into()]),
            WMArgEnum::StringVec(vec!["top".into()]),
            WMArgEnum::StringVec(vec!["topleft".into()]),
        ] {
            assert!(
                SnapDirection::from_arg(&arg).is_err(),
                "accepted invalid snap_window argument: {arg:?}"
            );
        }
    }

    /// A shown window on `monitor` at `rect`, floating unless `tiled`.
    fn shown_window(
        jwm: &mut Jwm,
        raw: u64,
        monitor: MonitorKey,
        rect: Rect,
        tiled: bool,
    ) -> ClientKey {
        use crate::backend::common_define::WindowId;
        use crate::core::models::WMClient;

        let mut client = WMClient::new(WindowId::from_raw(raw));
        client.mon = Some(monitor);
        client.state.tags = jwm.state.monitors[monitor].get_active_tags();
        client.state.is_floating = !tiled;
        client.geometry.border_w = 0;
        (client.geometry.x, client.geometry.y) = (rect.x, rect.y);
        (client.geometry.w, client.geometry.h) = (rect.w, rect.h);
        (client.geometry.floating_x, client.geometry.floating_y) = (rect.x, rect.y);
        (client.geometry.floating_w, client.geometry.floating_h) = (rect.w, rect.h);
        let key = jwm.insert_client(client);
        jwm.attach_to_monitor(key, monitor);
        key
    }

    /// A drag begun on an unlocked output can be released over a shade. The
    /// top-edge maximize used to move the window behind it, select that
    /// monitor and focus the window there; the attach drop did the same. No
    /// zone is planned over a locked monitor now, and a plan made before the
    /// lock landed is not applied.
    #[test]
    fn a_drop_over_a_locked_monitor_neither_moves_nor_selects_anything_there() {
        use crate::jwm::monitor::test_support::{DisplaySpyBackend, output};

        let mut backend = DisplaySpyBackend::new(vec![
            output(1, 0, 0, 1920, 1080),
            output(2, 1920, 0, 1920, 1080),
        ]);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").expect("test jwm");
        let (shown, shaded) = (jwm.state.monitor_order[0], jwm.state.monitor_order[1]);
        assert_eq!(jwm.state.monitors[shaded].num, 1);
        let floating = shown_window(
            &mut jwm,
            0x5d01,
            shown,
            Rect::new(200, 150, 600, 400),
            false,
        );
        let tiled = shown_window(&mut jwm, 0x5d02, shown, Rect::new(0, 0, 900, 1000), true);
        jwm.focus(&mut backend, Some(floating)).expect("focus");

        let (mx, my, mw, mh) = jwm.monitor_rect(shaded);
        let top = (mx + mw as i32 / 2, my + 1);
        let left = (mx + 1, my + mh as i32 / 2);
        let stale_maximize = jwm
            .plan_drag_snap_for(floating, shaded, top.0, top.1)
            .expect("an unlocked top edge is a maximize zone");
        assert_eq!(stale_maximize.maximize_monitor(), Some(shaded));
        let stale_attach = jwm
            .plan_drag_reorder(tiled, shaded, top.0, my + mh as i32 / 2)
            .expect("an unlocked tiling monitor is a reorder zone");

        jwm.lock_monitor(&mut backend, &WMArgEnum::Int(1))
            .expect("monitor 1 locks");

        for (px, py) in [top, left] {
            assert!(
                jwm.plan_drag_snap_for(floating, shaded, px, py).is_none(),
                "a snap zone was planned over the shade at ({px}, {py})"
            );
        }
        assert!(
            jwm.plan_drag_reorder(tiled, shaded, top.0, my + mh as i32 / 2)
                .is_none()
        );

        jwm.apply_drag_snap(&mut backend, floating, stale_maximize);
        jwm.apply_drag_snap(&mut backend, tiled, stale_attach);

        assert_eq!(jwm.state.sel_mon, Some(shown), "the shade was selected");
        let floating_client = &jwm.state.clients[floating];
        assert_eq!(floating_client.mon, Some(shown));
        assert_eq!(floating_client.state.maximized_axes(), MaximizeAxes::NONE);
        assert_eq!(jwm.state.clients[tiled].mon, Some(shown));
        let selected = jwm.get_selected_client_key();
        assert!(
            selected.is_none_or(|key| !jwm.client_is_on_locked_monitor(key)),
            "focus landed behind the shade"
        );
    }

    /// The top-edge zone previewed the work area for every float, but a
    /// fixed-size window or a dock refuses the maximize, and a PiP window
    /// took it while staying small: the maximized state was published on it
    /// and it came back maximized when PiP ended. Those windows get no zone
    /// now, and a maximize plan handed in for a PiP window is ignored, as
    /// `snap_window maximize` ignores it.
    #[test]
    fn the_top_edge_zone_skips_windows_a_user_maximize_would_not_take() {
        use crate::jwm::monitor::test_support::{DisplaySpyBackend, output};

        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").expect("test jwm");
        let mon = jwm.state.monitor_order[0];
        let dropped = Rect::new(300, 200, 480, 270);
        let key = shown_window(&mut jwm, 0x5d03, mon, dropped, false);
        jwm.focus(&mut backend, Some(key)).expect("focus");
        let (mx, my, mw, _) = jwm.monitor_rect(mon);
        let (px, py) = (mx + mw as i32 / 2, my + 1);
        assert_eq!(
            jwm.plan_drag_snap_for(key, mon, px, py)
                .and_then(|plan| plan.maximize_monitor()),
            Some(mon),
            "an ordinary float gets the zone"
        );

        let set_flag = |jwm: &mut Jwm, name: &str, on: bool| {
            let state = &mut jwm.state.clients[key].state;
            match name {
                "pip" => state.is_pip = on,
                "fixed" => state.is_fixed = on,
                _ => state.is_dock = on,
            }
        };
        for name in ["pip", "fixed", "dock"] {
            set_flag(&mut jwm, name, true);
            assert!(
                jwm.plan_drag_snap_for(key, mon, px, py).is_none(),
                "a {name} window got the maximize zone"
            );
            set_flag(&mut jwm, name, false);
        }

        // A PiP window that was floating before PiP: the maximize would be
        // admitted and land on its PiP return slot.
        let client = &mut jwm.state.clients[key];
        client.state.is_pip = true;
        client.state.old_state = true;
        let rect = jwm.maximize_work_area(mon).expect("a work area");
        jwm.apply_drag_snap(
            &mut backend,
            key,
            DragSnapPlan::Maximize { mon_key: mon, rect },
        );

        let client = &jwm.state.clients[key];
        assert_eq!(client.state.maximized_axes(), MaximizeAxes::NONE);
        assert_eq!(client.geometry.maximize_restore_rect, None);
        assert_eq!(
            Rect::new(
                client.geometry.floating_x,
                client.geometry.floating_y,
                client.geometry.floating_w,
                client.geometry.floating_h,
            ),
            dropped,
            "the PiP return slot was retargeted"
        );
    }
}
