//! 鼠标交互处理模块
//!
//! 这个模块包含鼠标拖动窗口和调整窗口大小的功能

use crate::backend::api::{Backend, ResizeEdge};
use crate::jwm::types::WMArgEnum;
use crate::jwm::Jwm;
use log::debug;

impl Jwm {
    /// 开始鼠标拖动窗口（Alt+左键拖动）
    ///
    /// - 如果窗口是平铺状态，自动切换为浮动
    /// - 全屏窗口不能拖动
    pub fn movemouse(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let client_key = self.get_selected_client_key().ok_or("No client selected")?;

        // 获取只读引用进行检查
        let (is_fullscreen, is_floating, win_id) =
            if let Some(c) = self.state.clients.get(client_key) {
                (c.state.is_fullscreen, c.state.is_floating, c.win)
            } else {
                return Ok(());
            };

        if is_fullscreen {
            return Ok(());
        }

        // 浮动检查：如果是平铺窗口，自动切换为浮动（保持当前几何，不恢复历史 floating_*）
        if !is_floating {
            self.enable_floating_keep_geometry(backend, client_key)?;
        }
        debug!(
            "Initiating move for window {:?} (floating: {}, fullscreen: {})",
            win_id, !is_floating, is_fullscreen
        );

        // [修改] 提升窗口堆叠顺序
        self.restack(backend, self.state.sel_mon)?;

        // [修改] 将控制权移交 Backend
        backend.begin_move(win_id)?;

        // Notify compositor of window move start (for wobbly windows effect)
        if backend.has_compositor() {
            backend.compositor_notify_window_move_start(win_id);
        }

        // Jwm 不再维护 InteractionState
        Ok(())
    }

    /// 开始鼠标调整窗口大小（Alt+右键拖动）
    ///
    /// - 如果窗口是平铺状态，自动切换为浮动
    /// - 全屏窗口不能调整大小
    /// - 根据鼠标位置智能选择调整边缘/角
    pub fn resizemouse(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let client_key = self.get_selected_client_key().ok_or("No client selected")?;

        let (is_fullscreen, is_floating, win_id) =
            if let Some(c) = self.state.clients.get(client_key) {
                (c.state.is_fullscreen, c.state.is_floating, c.win)
            } else {
                return Ok(());
            };

        if is_fullscreen {
            return Ok(());
        }

        if !is_floating {
            self.enable_floating_keep_geometry(backend, client_key)?;
        }

        self.restack(backend, self.state.sel_mon)?;

        // [修改] 将控制权移交 Backend。
        // Wayland/udev 通常不能 warp 指针，所以根据鼠标落点选择更直观的 resize 边/角：
        // - 靠近边：Top/Bottom/Left/Right
        // - 靠近角：TopLeft/TopRight/BottomLeft/BottomRight
        // - 中间区域：退化为象限选择（避免出现"怎么拖都不动"的感觉）
        let geom = backend.window_ops().get_geometry(win_id)?;
        let (px, py) = backend.input_ops().get_pointer_position()?;

        let w = (geom.w as f64).max(1.0);
        let h = (geom.h as f64).max(1.0);

        let rel_x = px - geom.x as f64;
        let rel_y = py - geom.y as f64;

        // Dynamic grip size: small windows still get a usable edge area.
        let threshold = 24.0_f64.min(w / 3.0).min(h / 3.0).max(8.0);

        let near_left = rel_x <= threshold;
        let near_right = rel_x >= (w - threshold);
        let near_top = rel_y <= threshold;
        let near_bottom = rel_y >= (h - threshold);

        let edge = if near_top && near_left {
            ResizeEdge::TopLeft
        } else if near_top && near_right {
            ResizeEdge::TopRight
        } else if near_bottom && near_left {
            ResizeEdge::BottomLeft
        } else if near_bottom && near_right {
            ResizeEdge::BottomRight
        } else if near_top {
            ResizeEdge::Top
        } else if near_bottom {
            ResizeEdge::Bottom
        } else if near_left {
            ResizeEdge::Left
        } else if near_right {
            ResizeEdge::Right
        } else {
            // Not near any border: pick a quadrant as a reasonable default.
            let left = rel_x < (w / 2.0);
            let top = rel_y < (h / 2.0);
            match (top, left) {
                (true, true) => ResizeEdge::TopLeft,
                (true, false) => ResizeEdge::TopRight,
                (false, true) => ResizeEdge::BottomLeft,
                (false, false) => ResizeEdge::BottomRight,
            }
        };

        backend.begin_resize(win_id, edge)?;

        Ok(())
    }

    /// 检查窗口拖动/调整大小后是否需要切换显示器
    ///
    /// 如果窗口移动到了另一个显示器，自动将其迁移过去
    pub(crate) fn check_monitor_consistency(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // 类似于之前的 check_monitor_change_after_resize
        let client_key = match self.get_selected_client_key() {
            Some(k) => k,
            None => return Ok(()),
        };

        let (x, y) = match self.state.clients.get(client_key) {
            Some(client) => (client.geometry.x, client.geometry.y),
            None => return Ok(()),
        };

        let target_monitor = self.recttomon(backend, x, y);
        if let Some(target_mon_key) = target_monitor {
            if Some(target_mon_key) != self.state.sel_mon {
                self.sendmon(backend, Some(client_key), Some(target_mon_key));
                self.state.sel_mon = Some(target_mon_key);
                self.focus(backend, None)?;
            }
        }
        Ok(())
    }
}
