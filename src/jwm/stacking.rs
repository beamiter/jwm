//! 窗口堆叠管理模块
//!
//! 这个模块负责管理窗口的 Z 轴顺序（堆叠顺序）

use crate::backend::api::Backend;
use crate::backend::common_define::WindowId;
use crate::core::models::{ClientKey, MonitorKey};
use crate::jwm::Jwm;
use log::debug;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StackingClient {
    window: WindowId,
    visible: bool,
    floating: bool,
    pip: bool,
    fullscreen: bool,
    above: bool,
    below: bool,
    selected: bool,
}

#[derive(Default)]
struct StackingLayer {
    tiled: Vec<WindowId>,
    floating: Vec<WindowId>,
    selected_tiled: Option<WindowId>,
    selected_floating: Option<WindowId>,
}

impl StackingLayer {
    fn push(&mut self, client: StackingClient) {
        if client.floating {
            if client.selected {
                self.selected_floating = Some(client.window);
            } else {
                self.floating.push(client.window);
            }
        } else if client.selected {
            self.selected_tiled = Some(client.window);
        } else {
            self.tiled.push(client.window);
        }
    }

    fn append_to(self, output: &mut Vec<WindowId>) {
        output.extend(self.tiled);
        output.extend(self.floating);
        // Preserve JWM's existing focus rule inside each EWMH layer: a
        // selected tiled client rises above floating peers in that same layer.
        if let Some(window) = self.selected_tiled {
            output.push(window);
        }
        if let Some(window) = self.selected_floating {
            output.push(window);
        }
    }
}

#[derive(Default)]
struct PriorityLayer {
    windows: Vec<WindowId>,
    selected: Option<WindowId>,
}

impl PriorityLayer {
    fn push(&mut self, client: StackingClient) {
        if client.selected {
            self.selected = Some(client.window);
        } else {
            self.windows.push(client.window);
        }
    }

    fn append_to(self, output: &mut Vec<WindowId>) {
        output.extend(self.windows);
        if let Some(window) = self.selected {
            output.push(window);
        }
    }
}

/// Plan managed-client stacking from bottom to top.
///
/// EWMH Above/Below form layers around ordinary clients. A visible focused
/// fullscreen client sits above Above, while PiP retains JWM's topmost policy.
/// Above wins defensively when a malformed client advertises both flags.
fn plan_stacking(clients_bottom_to_top: impl IntoIterator<Item = StackingClient>) -> Vec<WindowId> {
    let mut below = StackingLayer::default();
    let mut normal = StackingLayer::default();
    let mut above = StackingLayer::default();
    let mut focused_fullscreen = PriorityLayer::default();
    let mut pip = PriorityLayer::default();

    for client in clients_bottom_to_top {
        if !client.visible {
            continue;
        }
        if client.pip {
            pip.push(client);
        } else if client.selected && client.fullscreen {
            focused_fullscreen.push(client);
        } else if client.above {
            above.push(client);
        } else if client.below {
            below.push(client);
        } else {
            normal.push(client);
        }
    }

    let mut output = Vec::new();
    below.append_to(&mut output);
    normal.append_to(&mut output);
    above.append_to(&mut output);
    focused_fullscreen.append_to(&mut output);
    pip.append_to(&mut output);
    output
}

impl Jwm {
    /// 将窗口提升到堆叠顶部并聚焦
    ///
    /// - 将窗口从当前位置分离
    /// - 附加到显示器窗口列表前端
    /// - 设置焦点并重新排列
    pub(crate) fn pop(&mut self, backend: &mut dyn Backend, client_key: ClientKey) {
        let mon_key = if let Some(client) = self.state.clients.get(client_key) {
            client.mon
        } else {
            return;
        };

        self.detach(client_key);
        self.attach_front(client_key);

        let _ = self.focus(backend, Some(client_key));
        if let Some(mon_key) = mon_key {
            self.arrange(backend, Some(mon_key));
        }
    }

    /// 重新计算并应用窗口堆叠顺序
    ///
    /// Managed-client stacking, bottom to top:
    /// Below, Normal, Above, focused fullscreen, PiP. Inside an EWMH layer,
    /// tiled clients precede floating clients and selection promotes only a
    /// visible member of that same layer.
    pub(crate) fn restack(
        &mut self,
        backend: &mut dyn Backend,
        mon_key_opt: Option<MonitorKey>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // A marker, not a diagnostic: `arrange` calls this once per monitor
        // and every focus change calls it again, while the release default
        // filter is `info`. Keep it out of the session log.
        debug!("[restack]");

        let mon_key = mon_key_opt.ok_or("Monitor is required for restack operation")?;
        let monitor = self
            .state
            .monitors
            .get(mon_key)
            .ok_or("Monitor not found")?;
        let monitor_num = monitor.num;

        let sel_win = monitor
            .sel
            .and_then(|ck| self.state.clients.get(ck))
            .map(|c| c.win);
        let stack = self.get_monitor_stack(mon_key);
        let final_bottom_to_top = plan_stacking(stack.iter().rev().filter_map(|&ck| {
            let client = self.state.clients.get(ck)?;
            Some(StackingClient {
                window: client.win,
                visible: self.is_client_visible_on_monitor(ck, mon_key),
                floating: client.state.is_floating,
                pip: client.state.is_pip,
                fullscreen: client.state.is_fullscreen,
                above: client.state.is_above,
                below: client.state.is_below,
                selected: sel_win == Some(client.win),
            })
        }));

        let need_restack_windows = match self.last_stacking.get(mon_key) {
            Some(prev) => prev.as_slice() != final_bottom_to_top.as_slice(),
            None => true,
        };

        if need_restack_windows {
            backend.window_ops().restack_windows(&final_bottom_to_top)?;
            self.last_stacking
                .insert(mon_key, final_bottom_to_top.clone());
        }

        self.mark_bar_update_needed_if_visible(Some(monitor_num));

        debug!("[restack] finish");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{StackingClient, plan_stacking};
    use crate::backend::common_define::WindowId;

    fn client(id: u64) -> StackingClient {
        StackingClient {
            window: WindowId::from_raw(id),
            visible: true,
            floating: false,
            pip: false,
            fullscreen: false,
            above: false,
            below: false,
            selected: false,
        }
    }

    fn raw(windows: Vec<WindowId>) -> Vec<u64> {
        windows.into_iter().map(WindowId::raw).collect()
    }

    #[test]
    fn planner_orders_the_five_managed_client_layers() {
        let mut normal = client(2);
        normal.floating = true;
        let mut below = client(1);
        below.below = true;
        let mut above = client(3);
        above.above = true;
        let mut fullscreen = client(4);
        fullscreen.fullscreen = true;
        fullscreen.selected = true;
        let mut pip = client(5);
        pip.pip = true;

        assert_eq!(
            raw(plan_stacking([normal, pip, above, below, fullscreen])),
            [1, 2, 3, 4, 5]
        );
    }

    #[test]
    fn selected_tiled_client_only_rises_inside_its_ewmh_layer() {
        let mut normal_selected = client(2);
        normal_selected.selected = true;
        let mut normal_float = client(3);
        normal_float.floating = true;
        let mut above = client(4);
        above.above = true;
        let mut below_float = client(1);
        below_float.below = true;
        below_float.floating = true;

        assert_eq!(
            raw(plan_stacking([
                normal_selected,
                above,
                normal_float,
                below_float,
            ])),
            [1, 3, 2, 4]
        );
    }

    #[test]
    fn invisible_selected_client_is_never_reinserted() {
        let mut hidden_selected = client(9);
        hidden_selected.visible = false;
        hidden_selected.selected = true;
        hidden_selected.fullscreen = true;

        assert_eq!(raw(plan_stacking([client(1), hidden_selected])), [1]);
    }

    #[test]
    fn above_wins_a_malformed_double_state() {
        let mut double_state = client(3);
        double_state.above = true;
        double_state.below = true;
        let mut normal = client(2);
        normal.floating = true;
        let mut below = client(1);
        below.below = true;

        assert_eq!(raw(plan_stacking([double_state, normal, below])), [1, 2, 3]);
    }

    #[test]
    fn unselected_fullscreen_stays_in_its_ewmh_layer() {
        let mut fullscreen_below = client(1);
        fullscreen_below.fullscreen = true;
        fullscreen_below.below = true;
        let mut above = client(2);
        above.above = true;

        assert_eq!(raw(plan_stacking([above, fullscreen_below])), [1, 2]);
    }

    #[test]
    fn restack_markers_stay_below_the_default_log_level() {
        // `restack` runs on every arrange, every focus change and every
        // stacking request; a bare marker at `info` is a write(2) into the
        // journal for each. The needle is assembled at runtime, and the
        // haystack stops at this module, so it cannot match here.
        const SOURCE: &str = include_str!("stacking.rs");
        let body = SOURCE
            .split_once("fn restack")
            .expect("restack")
            .1
            .split_once("#[cfg(test)]")
            .expect("the test module")
            .0;
        let needle = format!("{}!(", "info");
        assert!(
            !body.contains(&needle),
            "restack regained an info-level marker line"
        );
    }
}
