// Drag-and-snap re-attach: dropping a dragged window near a monitor edge
// inserts it back into the current layout at the slot under the pointer,
// instead of leaving it as a floating half-screen window.
//
// The slot is found by simulation: the pure layout calculators take an
// ordered client list, so we insert the dragged client at every possible
// index, run the current layout's calculator, and keep the index whose
// resulting rect contains the pointer. Fibonacci's bottom-right spiral cell,
// grid cells, bstack columns … all fall out of the same mechanism.

use crate::backend::api::Backend;
use crate::config::CONFIG;
use crate::core::layout::{
    self as core_layout, LayoutClient, LayoutEnum, LayoutParams, LayoutResult,
};
use crate::core::models::{ClientKey, MonitorKey};
use crate::core::types::Rect;
use crate::jwm::{Jwm, WMArgEnum};
use log::info;

/// Edge-snap targets shared by the mouse drop below and the bindable
/// `snap_window` command. There is deliberately no `Bottom` (the mouse path
/// never snaps to the bottom edge) and no quarter-tiling: corner drops
/// resolve to a horizontal half, left winning over right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SnapDirection {
    Left,
    Right,
    Maximize,
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
            _ => return None,
        })
    }

    pub(crate) fn as_name(self) -> &'static str {
        match self {
            SnapDirection::Left => "left",
            SnapDirection::Right => "right",
            SnapDirection::Maximize => "maximize",
        }
    }

    /// Parse a `snap_window` binding/IPC argument: exactly one direction
    /// string (a plain config string arrives as a one-element vector).
    /// Anything else is a caller error, never a silent no-op.
    pub(crate) fn from_arg(arg: &WMArgEnum) -> Result<Self, String> {
        let WMArgEnum::StringVec(values) = arg else {
            return Err(format!(
                "snap_window takes a direction string (\"left\", \"right\", \"maximize\"), got {arg:?}"
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
                "unknown snap direction {name:?}; expected \"left\", \"right\", or \"maximize\""
            )
        })
    }
}

/// The snap geometry of the classic mouse float snap, extracted so the
/// keyboard command produces the same rect: left/right halves of the monitor
/// and the full monitor for a top-edge maximize.
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
    }
}

/// What a drop at the current pointer position would do.
pub(crate) enum DragSnapPlan {
    /// Keep the window floating and give it this rect (float layout, or a
    /// window that was floating before the drag started).
    Float { rect: Rect },
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
            | DragSnapPlan::Attach { rect, .. }
            | DragSnapPlan::AttachScrolling { rect, .. } => *rect,
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
    /// Plan what releasing the drag at root position (px, py) on `mon_key`
    /// should do. Returns None when the pointer is outside every snap zone,
    /// i.e. the window simply stays floating where the user dropped it.
    pub(crate) fn plan_drag_snap(
        &self,
        mon_key: MonitorKey,
        px: i32,
        py: i32,
    ) -> Option<DragSnapPlan> {
        let drag_key = self.get_selected_client_key()?;
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

        // Classic float snap: left/right halves, top edge maximizes. Corner
        // drops resolve to the horizontal half (left before right); the
        // bottom edge has no zone.
        let direction = if near_left {
            SnapDirection::Left
        } else if near_right {
            SnapDirection::Right
        } else if near_top {
            SnapDirection::Maximize
        } else {
            return None;
        };
        let rect = snap_rect(Rect::new(mx, my, mw, mh), direction);
        Some(DragSnapPlan::Float { rect })
    }

    /// Plan a reorder drop for a window that stayed tiled through its drag:
    /// the whole monitor is a drop zone and the plan is always an attach at
    /// the layout slot under the pointer. Returns None when the monitor's
    /// layout is not a tiling one or the client cannot be re-slotted.
    pub(crate) fn plan_drag_reorder(
        &self,
        drag_key: ClientKey,
        mon_key: MonitorKey,
        px: i32,
        py: i32,
    ) -> Option<DragSnapPlan> {
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
        match plan {
            DragSnapPlan::Float { rect } => {
                self.apply_float_snap_rect(backend, drag_key, rect);
            }
            DragSnapPlan::Attach {
                mon_key,
                tiled_index,
                ..
            } => {
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
    /// window on a monitor edge — snap the focused window to a half or the
    /// full monitor rect, using the same geometry as the mouse float snap.
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
    /// geometries that exercise the math (ultrawide, small, offset origin).
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
    }

    #[test]
    fn snap_rect_halves_odd_widths_the_way_the_mouse_path_did() {
        // Odd widths floor the half and start the right half there, leaving
        // the last column uncovered — the pre-extraction integer math.
        let mon = Rect::new(0, 0, 1921, 1080);
        assert_eq!(
            snap_rect(mon, SnapDirection::Left),
            Rect::new(0, 0, 960, 1080)
        );
        assert_eq!(
            snap_rect(mon, SnapDirection::Right),
            Rect::new(960, 0, 960, 1080)
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
    }

    #[test]
    fn snap_direction_rejects_unknown_names() {
        // No bottom/quarter directions exist; nothing silently maps to a half.
        for name in ["", "bottom", "up", "max", "full", "quarter", "top"] {
            assert_eq!(SnapDirection::from_name(name), None, "accepted {name:?}");
        }
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

        // Missing, extra, non-string, and unknown arguments are all errors.
        for arg in [
            WMArgEnum::Int(0),
            WMArgEnum::UInt(2),
            WMArgEnum::Float(0.5),
            WMArgEnum::StringVec(Vec::new()),
            WMArgEnum::StringVec(vec!["left".into(), "right".into()]),
            WMArgEnum::StringVec(vec!["bottom".into()]),
        ] {
            assert!(
                SnapDirection::from_arg(&arg).is_err(),
                "accepted invalid snap_window argument: {arg:?}"
            );
        }
    }
}
