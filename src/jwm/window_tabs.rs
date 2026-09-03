// Window tab bar: the window-manager half.
//
// A monitor whose tiling area holds more than one window gets a strip across
// the top of that area, one cell per window. The window manager owns three
// things about it and the compositors own none of them: which windows are in
// it, where the strip is (it reserves the pixels out of the work area, so no
// window is ever under it), and what a click on it means. The compositors are
// handed the finished rectangle and the titles, and paint that.
//
// Every rectangle comes from `compositor_common::window_tabs`, which the
// compositors also draw from, so the reserved strip and the painted strip
// cannot drift apart.

use crate::Jwm;
use crate::backend::api::Backend;
use crate::backend::compositor_common::window_tabs as tabs;
use crate::config::CONFIG;
use crate::core::models::{ClientKey, MonitorKey};
use crate::core::types::Rect;
use log::info;

/// A left-button press on a tab cell being watched as a reorder drag.
///
/// Unlike [`crate::jwm::mouse_handler::DragCtl`] this never arms a backend
/// interaction or grabs the pointer: the strip is layout-reserved
/// background, so press, motion and release all arrive as ordinary
/// root-window events. The press has already focused the window; crossing
/// the drag threshold activates the drag, and the release commits the new
/// slot (see [`Jwm::commit_window_tab_reorder`]).
#[derive(Debug, Clone, Copy)]
pub(crate) struct TabDragCtl {
    /// The window the pressed cell stands for.
    pub client: ClientKey,
    /// The monitor whose bar the press started on. Diagnostic only — the
    /// release re-resolves its target monitor from the drop coordinates.
    pub mon: MonitorKey,
    pub start_root: (f64, f64),
    /// Set once the pointer travelled `behavior.drag_threshold_px` from the
    /// press; below that the gesture stays a plain click.
    pub activated: bool,
}

impl Jwm {
    /// The windows sharing `mon_key`'s tab bar, in the order their cells are
    /// drawn. Empty when the feature is off, when the monitor has nothing to
    /// choose between, or when a fullscreen window is covering the strip.
    ///
    /// This is the single predicate behind both the reservation and the bar
    /// itself: reserving without drawing would leave an unreachable band of
    /// wallpaper, and drawing without reserving is what used to put the bar
    /// on top of the status bar.
    pub(crate) fn tab_group_clients(&self, mon_key: MonitorKey) -> Vec<ClientKey> {
        if !CONFIG.load().behavior().window_tabs {
            return Vec::new();
        }
        // The fullscreen layout hands the whole output to one window and takes
        // the status bar down with it; the tab strip follows, or it would be
        // the only chrome left floating over the fullscreen surface.
        if let Some(monitor) = self.state.monitors.get(mon_key) {
            if monitor.lt.is_fullscreen_layout() {
                return Vec::new();
            }
        }
        let Some(client_keys) = self.state.monitor_clients.get(mon_key) else {
            return Vec::new();
        };

        let mut group = Vec::new();
        for &client_key in client_keys {
            let Some(client) = self.state.clients.get(client_key) else {
                continue;
            };
            if !self.is_client_visible_on_monitor(client_key, mon_key) {
                continue;
            }
            // A fullscreen window owns the whole output. Painting a strip over
            // it would be the same bug as painting over the status bar, so the
            // monitor loses its bar for as long as one is up.
            if client.state.is_fullscreen {
                return Vec::new();
            }
            if client.state.is_floating {
                continue;
            }
            group.push(client_key);
        }

        if tabs::wants_bar(group.len()) {
            group
        } else {
            Vec::new()
        }
    }

    /// Height reserved at the top of `mon_key`'s work area, 0 when it has no
    /// bar.
    pub(crate) fn tab_bar_reserved(&self, mon_key: MonitorKey) -> i32 {
        if self.tab_group_clients(mon_key).is_empty() {
            0
        } else {
            tabs::bar_height(CONFIG.load().behavior().tab_bar_height).round() as i32
        }
    }

    /// The strip itself, in screen pixels — the band `monitor_work_area` took
    /// off the top.
    pub(crate) fn monitor_tab_bar(&self, mon_key: MonitorKey) -> Option<tabs::Rect> {
        let reserved = self.tab_bar_reserved(mon_key);
        if reserved <= 0 {
            return None;
        }
        let area = self.monitor_work_area_untabbed(mon_key)?;
        Some([area.x as f32, area.y as f32, area.w as f32, reserved as f32])
    }

    /// Build every monitor's bar for the compositor.
    pub(crate) fn build_window_groups(&self) -> Vec<tabs::TabGroup> {
        let focused = self.get_selected_client_key();
        let mut groups = Vec::with_capacity(self.state.monitor_order.len());
        for &mon_key in &self.state.monitor_order {
            let group = self.tab_group_clients(mon_key);
            if group.is_empty() {
                continue;
            }
            let Some(bar) = self.monitor_tab_bar(mon_key) else {
                continue;
            };
            let cells = group
                .iter()
                .filter_map(|&client_key| {
                    let client = self.state.clients.get(client_key)?;
                    Some(tabs::Tab {
                        title: client.name.clone(),
                        active: focused == Some(client_key),
                    })
                })
                .collect::<Vec<_>>();
            if !tabs::wants_bar(cells.len()) {
                continue;
            }
            groups.push(tabs::TabGroup { bar, tabs: cells });
        }
        groups
    }

    /// The monitor and window whose cell covers `(x, y)`, if a tab bar does.
    pub(crate) fn window_tab_hit(&self, x: f64, y: f64) -> Option<(MonitorKey, ClientKey)> {
        for &mon_key in &self.state.monitor_order {
            let group = self.tab_group_clients(mon_key);
            if group.is_empty() {
                continue;
            }
            let Some(bar) = self.monitor_tab_bar(mon_key) else {
                continue;
            };
            if let Some(index) = tabs::tab_at(bar, group.len(), x as f32, y as f32) {
                return group
                    .get(index)
                    .copied()
                    .map(|client_key| (mon_key, client_key));
            }
        }
        None
    }

    /// The window whose cell covers `(x, y)`, if a tab bar does.
    pub(crate) fn window_tab_at(&self, x: f64, y: f64) -> Option<ClientKey> {
        self.window_tab_hit(x, y).map(|(_, client_key)| client_key)
    }

    /// Focus the window a tab-bar click landed on. Returns false when the
    /// click was not on a bar, leaving it to the ordinary click handling.
    pub(crate) fn click_window_tab(
        &mut self,
        backend: &mut dyn Backend,
        x: f64,
        y: f64,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(client_key) = self.window_tab_at(x, y) else {
            return Ok(false);
        };
        if self.get_selected_client_key() == Some(client_key) {
            // Already the active cell; a redundant click must not disturb the
            // stacking order.
            return Ok(true);
        }
        self.focus(backend, Some(client_key))?;
        if let Some(mon_key) = self.state.sel_mon {
            self.last_stacking.remove(mon_key);
        }
        let _ = self.restack(backend, self.state.sel_mon);
        Ok(true)
    }

    /// Middle-click on a tab cell closes the window it stands for, the way a
    /// browser tab's middle-click does. Returns false when the click was not
    /// on a bar, leaving it to the ordinary click handling.
    pub(crate) fn close_window_tab(
        &mut self,
        backend: &mut dyn Backend,
        x: f64,
        y: f64,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(client_key) = self.window_tab_at(x, y) else {
            return Ok(false);
        };
        let Some(client) = self.state.clients.get(client_key) else {
            return Ok(false);
        };
        let win = client.win;
        info!("[close_window_tab] Closing window {:?} via tab", win);
        let res = backend.window_ops().close_window(win)?;
        if res == crate::backend::api::CloseResult::Forced {
            info!("[close_window_tab] Force killed client");
        }
        Ok(true)
    }

    /// Record a left press on a tab cell as a possible reorder drag. The
    /// press itself already focused the window via [`Self::click_window_tab`];
    /// this only remembers where a drag would have started from.
    pub(crate) fn arm_window_tab_drag(&mut self, x: f64, y: f64) {
        self.tab_drag = self
            .window_tab_hit(x, y)
            .map(|(mon_key, client_key)| TabDragCtl {
                client: client_key,
                mon: mon_key,
                start_root: (x, y),
                activated: false,
            });
    }

    /// Commit an activated tab drag: move the dragged window to the cell the
    /// release point resolves to, inside that monitor's tiled order. A drop
    /// that resolves to nothing (no tab bar there, same slot, or a bar the
    /// dragged window is not part of) simply cancels the gesture.
    pub(crate) fn commit_window_tab_reorder(
        &mut self,
        backend: &mut dyn Backend,
        drag: TabDragCtl,
        x: f64,
        y: f64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some((mon_key, slot)) = self.tab_reorder_target(backend, x, y) else {
            return Ok(());
        };
        let group = self.tab_group_clients(mon_key);
        let Some(new_order) = plan_tab_reorder(&group, drag.client, slot) else {
            return Ok(());
        };
        if let Some(list) = self.state.monitor_clients.get_mut(mon_key) {
            // Same re-insertion tail as `attach_dragged_client`: out of the
            // list, then back in right before the window the new order puts
            // after ours — or at the end when ours is now last.
            let successor = new_order
                .iter()
                .position(|&k| k == drag.client)
                .and_then(|pos| new_order.get(pos + 1))
                .copied();
            list.retain(|&k| k != drag.client);
            let insert_pos = successor
                .and_then(|k| list.iter().position(|&k2| k2 == k))
                .unwrap_or(list.len());
            list.insert(insert_pos.min(list.len()), drag.client);
        }
        info!(
            "[commit_window_tab_reorder] moved {:?} to slot {} on monitor {:?} (drag from {:?})",
            drag.client, slot, mon_key, drag.mon
        );
        self.arrange(backend, Some(mon_key));
        Ok(())
    }

    /// The monitor and cell a tab-drag release at `(x, y)` commits to. A
    /// point on a bar picks that bar's cell; anywhere else the monitor under
    /// the point lends its bar, clamping x into the first/last cell. A
    /// monitor without a tab group cancels the drop.
    fn tab_reorder_target(
        &mut self,
        backend: &mut dyn Backend,
        x: f64,
        y: f64,
    ) -> Option<(MonitorKey, usize)> {
        for &mon_key in &self.state.monitor_order {
            let group = self.tab_group_clients(mon_key);
            if group.is_empty() {
                continue;
            }
            let Some(bar) = self.monitor_tab_bar(mon_key) else {
                continue;
            };
            if let Some(index) = tabs::tab_at(bar, group.len(), x as f32, y as f32) {
                return Some((mon_key, index));
            }
        }
        let mon_key = self.recttomon(backend, x as i32, y as i32)?;
        let group = self.tab_group_clients(mon_key);
        if group.is_empty() {
            return None;
        }
        let bar = self.monitor_tab_bar(mon_key)?;
        let [bx, by, bw, bh] = bar;
        let cx = (x as f32).clamp(bx, bx + bw);
        let cy = (y as f32).clamp(by, by + bh);
        let index = tabs::tab_at(bar, group.len(), cx, cy)?;
        Some((mon_key, index))
    }
}

/// Take the tab strip off the top of a work area. Free function so the layout
/// helper can apply it without another borrow of `self`.
pub(crate) fn without_tab_bar(area: Rect, reserved: i32) -> Rect {
    if reserved <= 0 {
        return area;
    }
    let reserved = reserved.min(area.h);
    Rect::new(area.x, area.y + reserved, area.w, area.h - reserved)
}

/// The tab order after dragging `dragged` to `target_slot`. Cells share the
/// monitor's tiled order, so slot indexes index the group itself; an
/// out-of-range slot clamps to the last one. `None` when the drag changes
/// nothing: the window is not in the group, or the clamped target is the
/// slot it already occupies.
pub(crate) fn plan_tab_reorder(
    order: &[ClientKey],
    dragged: ClientKey,
    target_slot: usize,
) -> Option<Vec<ClientKey>> {
    let from = order.iter().position(|&k| k == dragged)?;
    let to = target_slot.min(order.len().saturating_sub(1));
    if to == from {
        return None;
    }
    let mut new_order = order.to_vec();
    let key = new_order.remove(from);
    new_order.insert(to, key);
    Some(new_order)
}

#[cfg(test)]
mod tests {
    use super::{plan_tab_reorder, without_tab_bar};
    use crate::core::models::ClientKey;
    use crate::core::types::Rect;
    use slotmap::SlotMap;

    fn keys(n: usize) -> Vec<ClientKey> {
        let mut sm: SlotMap<ClientKey, ()> = SlotMap::new();
        (0..n).map(|_| sm.insert(())).collect()
    }

    #[test]
    fn reserving_moves_the_area_down_and_shortens_it() {
        let area = Rect::new(10, 40, 800, 600);
        assert_eq!(without_tab_bar(area, 28), Rect::new(10, 68, 800, 572));
    }

    #[test]
    fn no_bar_leaves_the_area_alone() {
        let area = Rect::new(10, 40, 800, 600);
        assert_eq!(without_tab_bar(area, 0), area);
        assert_eq!(without_tab_bar(area, -5), area);
    }

    #[test]
    fn a_bar_taller_than_the_area_cannot_make_it_negative() {
        let area = Rect::new(0, 0, 800, 20);
        assert_eq!(without_tab_bar(area, 200), Rect::new(0, 20, 800, 0));
    }

    #[test]
    fn reorder_moves_a_cell_forward() {
        let order = keys(4);
        let dragged = order[0];
        assert_eq!(
            plan_tab_reorder(&order, dragged, 2),
            Some(vec![order[1], order[2], order[0], order[3]])
        );
    }

    #[test]
    fn reorder_moves_a_cell_backward() {
        let order = keys(4);
        let dragged = order[3];
        assert_eq!(
            plan_tab_reorder(&order, dragged, 0),
            Some(vec![order[3], order[0], order[1], order[2]])
        );
    }

    #[test]
    fn reorder_to_an_adjacent_cell_swaps_them() {
        let order = keys(3);
        let dragged = order[2];
        assert_eq!(
            plan_tab_reorder(&order, dragged, 1),
            Some(vec![order[0], order[2], order[1]])
        );
    }

    #[test]
    fn dropping_on_the_same_slot_is_a_noop() {
        let order = keys(4);
        assert_eq!(plan_tab_reorder(&order, order[1], 1), None);
    }

    #[test]
    fn a_window_outside_the_group_is_a_noop() {
        // SlotMap keys are only unique per map, so the stranger must come
        // from the same map as the group.
        let mut sm: SlotMap<ClientKey, ()> = SlotMap::new();
        let order: Vec<ClientKey> = (0..3).map(|_| sm.insert(())).collect();
        let stranger = sm.insert(());
        assert_eq!(plan_tab_reorder(&order, stranger, 1), None);
        assert_eq!(plan_tab_reorder(&[], stranger, 0), None);
    }

    #[test]
    fn an_out_of_range_slot_clamps_to_the_last_cell() {
        let order = keys(4);
        let dragged = order[0];
        let expected = Some(vec![order[1], order[2], order[3], order[0]]);
        assert_eq!(plan_tab_reorder(&order, dragged, 99), expected);
        // The slot right past the end clamps the same way.
        assert_eq!(plan_tab_reorder(&order, dragged, 4), expected);
    }
}
