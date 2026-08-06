//! 鼠标交互处理模块
//!
//! 这个模块包含鼠标拖动窗口和调整窗口大小的功能。
//!
//! 所有交互式拖拽都先经过一个"死区"（`behavior.drag_threshold_px`）：
//! 按下后指针位移未超过阈值前，窗口不浮动也不移动；一次没有位移的
//! 单击不放不会破坏平铺布局。越过阈值后按 [`DragMode`] 升级。

use crate::backend::api::{Backend, InteractionAction, ResizeEdge};
use crate::backend::common_define::WindowId;
use crate::core::models::{ClientKey, MonitorKey};
use crate::jwm::Jwm;
use crate::jwm::types::WMArgEnum;
use log::debug;

/// How an in-flight pointer drag will act on its window once it crosses the
/// drag threshold.
#[derive(Debug, Clone, Copy)]
pub(crate) enum DragMode {
    /// Float the window (keeping its geometry) and move it with the pointer.
    MoveFloat,
    /// Keep the window tiled; the drag only picks a new layout slot, shown
    /// as a snap preview and committed on release.
    Reorder,
    /// Float the window (keeping its geometry) and resize it from this edge.
    Resize(ResizeEdge),
}

/// A pointer drag the WM is watching. Armed on button press (or a client's
/// `_NET_WM_MOVERESIZE` request) with a track-only pointer grab; dormant
/// until the pointer travels `behavior.drag_threshold_px` from the press.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DragCtl {
    pub client: ClientKey,
    pub win: WindowId,
    pub mode: DragMode,
    pub start_root: (f64, f64),
    /// Set once the threshold was crossed and the mode's side effects ran.
    pub activated: bool,
    /// Pre-drag state, so `_NET_WM_MOVERESIZE_CANCEL` (e.g. Esc in a GTK
    /// drag) can put everything back.
    pub was_floating: bool,
    pub orig_geom: (i32, i32, i32, i32),
    pub orig_index: Option<usize>,
    pub mon: Option<MonitorKey>,
}

impl Jwm {
    /// 开始鼠标拖动窗口（Alt+左键拖动）
    ///
    /// - 越过拖拽死区后，平铺窗口自动切换为浮动
    /// - 全屏窗口不能拖动
    pub fn movemouse(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let client_key = self.get_selected_client_key().ok_or("No client selected")?;

        // 提升窗口堆叠顺序
        self.restack(backend, self.state.sel_mon)?;

        self.start_pointer_drag(backend, client_key, DragMode::MoveFloat)
    }

    /// 开始鼠标调整窗口大小（Alt+右键拖动）
    ///
    /// - 越过拖拽死区后，平铺窗口自动切换为浮动
    /// - 全屏窗口不能调整大小
    /// - 根据鼠标位置智能选择调整边缘/角
    pub fn resizemouse(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let client_key = self.get_selected_client_key().ok_or("No client selected")?;

        let (is_fullscreen, win_id) = if let Some(c) = self.state.clients.get(client_key) {
            (c.state.is_fullscreen, c.win)
        } else {
            return Ok(());
        };

        if is_fullscreen {
            return Ok(());
        }

        self.restack(backend, self.state.sel_mon)?;

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

        self.start_pointer_drag(backend, client_key, DragMode::Resize(edge))
    }

    /// Arm a deferred pointer drag on `client_key`: capture the pre-drag
    /// state, grab the pointer in track-only mode, and wait for the motion
    /// handler to cross the drag threshold. On backends without track
    /// support the drag is silently abandoned.
    pub(crate) fn start_pointer_drag(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        mode: DragMode,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(client) = self.state.clients.get(client_key) else {
            return Ok(());
        };
        if client.state.is_fullscreen {
            return Ok(());
        }
        let win = client.win;
        let was_floating = client.state.is_floating;
        let orig_geom = (
            client.geometry.x,
            client.geometry.y,
            client.geometry.w,
            client.geometry.h,
        );
        let mon = client.mon;
        let orig_index = mon.and_then(|mk| {
            self.get_monitor_clients(mk)
                .iter()
                .position(|&k| k == client_key)
        });
        let start_root = backend
            .input_ops()
            .get_pointer_position()
            .unwrap_or(self.last_mouse_root);
        let intent = match mode {
            DragMode::Resize(edge) => InteractionAction::Resize(edge),
            DragMode::MoveFloat | DragMode::Reorder => InteractionAction::Move,
        };

        if backend.begin_track(win, intent)? {
            debug!("Armed pointer drag for window {:?} as {:?}", win, mode);
            self.drag_ctl = Some(DragCtl {
                client: client_key,
                win,
                mode,
                start_root,
                activated: false,
                was_floating,
                orig_geom,
                orig_index,
                mon,
            });
        }
        Ok(())
    }

    /// The armed drag crossed the drag threshold: run its mode's side
    /// effects (float the window, hand the pointer to the backend's real
    /// move/resize). Reorder drags stay a track-only grab for their whole
    /// life — the window never leaves the tiling.
    pub(crate) fn activate_pointer_drag(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some((client, win, mode)) = self.drag_ctl.as_ref().map(|c| (c.client, c.win, c.mode))
        else {
            return Ok(());
        };
        if let Some(ctl) = self.drag_ctl.as_mut() {
            ctl.activated = true;
        }
        match mode {
            DragMode::MoveFloat => {
                self.enable_floating_keep_geometry(backend, client)?;
                backend.begin_move(win)?;
                // Notify compositor of window move start (for wobbly windows effect)
                if backend.has_compositor() {
                    backend.compositor_notify_window_move_start(win);
                }
            }
            DragMode::Resize(edge) => {
                self.enable_floating_keep_geometry(backend, client)?;
                backend.begin_resize(win, edge)?;
            }
            DragMode::Reorder => {}
        }
        Ok(())
    }

    /// `_NET_WM_MOVERESIZE_CANCEL` (e.g. Esc during a GTK header-bar drag):
    /// end the grab and put the window back where the drag found it.
    pub(crate) fn cancel_pointer_drag(&mut self, backend: &mut dyn Backend) {
        let ctl = self.drag_ctl.take();
        let _ = backend.handle_button_release(0);
        if backend.has_compositor() {
            backend.compositor_set_snap_preview(None);
        }
        let Some(ctl) = ctl else {
            return;
        };
        if !ctl.activated {
            return;
        }
        if matches!(ctl.mode, DragMode::MoveFloat) && backend.has_compositor() {
            backend.compositor_notify_window_move_end(ctl.win);
        }
        match ctl.mode {
            // The tiling was never touched.
            DragMode::Reorder => {}
            DragMode::MoveFloat | DragMode::Resize(_) => {
                if ctl.was_floating {
                    let (x, y, w, h) = ctl.orig_geom;
                    self.resize_client(backend, ctl.client, x, y, w, h, false);
                } else {
                    if let Some(c) = self.state.clients.get_mut(ctl.client) {
                        c.state.is_floating = false;
                        c.state.is_drag_floating = false;
                    }
                    if let (Some(mk), Some(idx)) = (ctl.mon, ctl.orig_index) {
                        if let Some(list) = self.state.monitor_clients.get_mut(mk) {
                            list.retain(|&k| k != ctl.client);
                            list.insert(idx.min(list.len()), ctl.client);
                        }
                    }
                    self.arrange(backend, ctl.mon);
                }
            }
        }
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
