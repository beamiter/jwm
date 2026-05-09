//! 窗口堆叠管理模块
//!
//! 这个模块负责管理窗口的 Z 轴顺序（堆叠顺序）

use crate::backend::api::Backend;
use crate::backend::common_define::WindowId;
use crate::core::models::{ClientKey, MonitorKey};
use crate::jwm::Jwm;
use log::info;

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
    /// 堆叠规则（从下到上）：
    /// 1. 平铺窗口（tiled）
    /// 2. 浮动窗口（floating）
    /// 3. 选中的平铺窗口（提升到浮动窗口之上，避免被遮挡）
    /// 4. PiP 窗口（始终在最顶层）
    pub(crate) fn restack(
        &mut self,
        backend: &mut dyn Backend,
        mon_key_opt: Option<MonitorKey>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[restack]");

        let mon_key = mon_key_opt.ok_or("Monitor is required for restack operation")?;
        let monitor = self
            .state
            .monitors
            .get(mon_key)
            .ok_or("Monitor not found")?;
        let monitor_num = monitor.num;

        let stack = self.get_monitor_stack(mon_key);

        let mut tiled_bottom_to_top: Vec<WindowId> = Vec::new();
        let mut floating_bottom_to_top: Vec<WindowId> = Vec::new();
        let mut pip_bottom_to_top: Vec<WindowId> = Vec::new();

        for &ck in stack.iter().rev() {
            if let Some(c) = self.state.clients.get(ck) {
                if !self.is_client_visible_on_monitor(ck, mon_key) {
                    continue;
                }
                if c.state.is_pip {
                    pip_bottom_to_top.push(c.win);
                } else if c.state.is_floating {
                    floating_bottom_to_top.push(c.win);
                } else {
                    tiled_bottom_to_top.push(c.win);
                }
            }
        }

        // Promote selected window to top of its layer, and if it's tiled,
        // raise it above floating windows so it's not obscured.
        let sel_win = monitor
            .sel
            .and_then(|ck| self.state.clients.get(ck))
            .map(|c| (c.win, c.state.is_floating, c.state.is_pip));

        let mut final_bottom_to_top: Vec<WindowId> = Vec::with_capacity(
            tiled_bottom_to_top.len() + floating_bottom_to_top.len() + pip_bottom_to_top.len(),
        );

        if let Some((win, is_floating, is_pip)) = sel_win {
            if is_pip {
                // PiP: promote within pip layer
                if let Some(idx) = pip_bottom_to_top.iter().position(|&w| w == win) {
                    let w = pip_bottom_to_top.remove(idx);
                    pip_bottom_to_top.push(w);
                }
                final_bottom_to_top.extend(tiled_bottom_to_top);
                final_bottom_to_top.extend(floating_bottom_to_top);
                final_bottom_to_top.extend(pip_bottom_to_top);
            } else if is_floating {
                // Floating: promote to top of floating layer (above other floats, below pip)
                if let Some(idx) = floating_bottom_to_top.iter().position(|&w| w == win) {
                    let w = floating_bottom_to_top.remove(idx);
                    floating_bottom_to_top.push(w);
                }
                final_bottom_to_top.extend(tiled_bottom_to_top);
                final_bottom_to_top.extend(floating_bottom_to_top);
                final_bottom_to_top.extend(pip_bottom_to_top);
            } else {
                // Tiled: raise focused tiled window above all floats so it's not obscured
                tiled_bottom_to_top.retain(|&w| w != win);
                final_bottom_to_top.extend(tiled_bottom_to_top);
                final_bottom_to_top.extend(floating_bottom_to_top);
                final_bottom_to_top.push(win); // focused tiled above floats
                final_bottom_to_top.extend(pip_bottom_to_top);
            }
        } else {
            final_bottom_to_top.extend(tiled_bottom_to_top);
            final_bottom_to_top.extend(floating_bottom_to_top);
            final_bottom_to_top.extend(pip_bottom_to_top);
        }

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

        info!("[restack] finish");
        Ok(())
    }
}
