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

    /// The window whose cell covers `(x, y)`, if a tab bar does.
    pub(crate) fn window_tab_at(&self, x: f64, y: f64) -> Option<ClientKey> {
        for &mon_key in &self.state.monitor_order {
            let group = self.tab_group_clients(mon_key);
            if group.is_empty() {
                continue;
            }
            let Some(bar) = self.monitor_tab_bar(mon_key) else {
                continue;
            };
            if let Some(index) = tabs::tab_at(bar, group.len(), x as f32, y as f32) {
                return group.get(index).copied();
            }
        }
        None
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

#[cfg(test)]
mod tests {
    use super::without_tab_bar;
    use crate::core::types::Rect;

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
}
