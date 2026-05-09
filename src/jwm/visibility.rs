//! 窗口显示控制模块
//!
//! 这个模块负责管理窗口的显示和隐藏，包括动画效果

use crate::backend::api::Backend;
use crate::config::CONFIG;
use crate::core::animation::AnimationKind;
use crate::core::models::{ClientKey, MonitorKey};
use crate::core::types::Rect;
use crate::jwm::Jwm;
use log::warn;
use std::time::Instant;

impl Jwm {
    /// 显示/隐藏指定显示器上的所有窗口
    ///
    /// 根据每个窗口在显示器上的可见性决定显示或隐藏
    pub(crate) fn showhide_monitor(&mut self, backend: &mut dyn Backend, mon_key: MonitorKey) {
        if let Some(stack_clients) = self.state.monitor_stack.get(mon_key).cloned() {
            for client_key in stack_clients {
                self.showhide_client(backend, client_key, mon_key);
            }
        }
    }

    /// 根据窗口在指定显示器上的可见性，显示或隐藏窗口
    pub(crate) fn showhide_client(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        mon_key: MonitorKey,
    ) {
        let is_visible = self.is_client_visible_on_monitor(client_key, mon_key);

        if is_visible {
            self.show_client(backend, client_key);
        } else {
            self.hide_client(backend, client_key);
        }
    }

    /// 显示窗口（将窗口移动到可见区域）
    ///
    /// - 取消任何进行中的隐藏动画
    /// - 恢复窗口的可见位置
    /// - 对浮动窗口应用正确的几何
    pub(crate) fn show_client(&mut self, backend: &mut dyn Backend, client_key: ClientKey) {
        // Cancel any in-flight Hide animation so it doesn't keep moving
        // the window off-screen.  Preserve Layout / Appear animations so
        // that repeated arrange() calls don't kill in-flight transitions.
        self.animations.remove_if_hide(client_key);

        // Restore on-screen x from old_x if client.geometry.x is still at
        // the hidden position (negative, off-screen).
        if let Some(client) = self.state.clients.get_mut(client_key) {
            if client.geometry.x < -(client.geometry.w) {
                client.geometry.x = client.geometry.old_x;
            }
        }

        let (win, x, y, is_floating, is_fullscreen) =
            if let Some(client) = self.state.clients.get(client_key) {
                (
                    client.win,
                    client.geometry.x,
                    client.geometry.y,
                    client.state.is_floating,
                    client.state.is_fullscreen,
                )
            } else {
                warn!("[show_client] Client {:?} not found", client_key);
                return;
            };

        if let Err(e) = self.move_window(backend, win, x, y) {
            warn!("[show_client] Failed to move window {:?}: {:?}", win, e);
        }

        if is_floating && !is_fullscreen {
            let (w, h) = if let Some(client) = self.state.clients.get(client_key) {
                (client.geometry.w, client.geometry.h)
            } else {
                return;
            };
            self.resize_client(backend, client_key, x, y, w, h, false);
        }
    }

    /// 隐藏窗口（将窗口移动到屏幕外）
    ///
    /// - 保存当前位置以便后续恢复
    /// - 将窗口移动到屏幕左侧外
    /// - 使用滑出动画（如果启用）
    pub(crate) fn hide_client(&mut self, backend: &mut dyn Backend, client_key: ClientKey) {
        let (win, x, y, w, h, width) = if let Some(client) = self.state.clients.get(client_key) {
            (
                client.win,
                client.geometry.x,
                client.geometry.y,
                client.geometry.w,
                client.geometry.h,
                client.total_width(),
            )
        } else {
            warn!("[hide_client] Client {:?} not found", client_key);
            return;
        };

        let hidden_x = width * -2;

        // Save visible geometry so show_client can restore it, then update
        // client.geometry to the hidden position. This prevents
        // tick_animations from snapping the window back on-screen when the
        // Hide animation completes.
        //
        // Guard: only save old_x/old_y when the window is still on-screen.
        // If it is already hidden (x is far negative), a repeated hide_client
        // call must NOT overwrite old_x with the hidden position — otherwise
        // show_client will restore the window to an off-screen coordinate.
        if let Some(client) = self.state.clients.get_mut(client_key) {
            if client.geometry.x >= -(client.geometry.w) {
                client.geometry.old_x = client.geometry.x;
                client.geometry.old_y = client.geometry.y;
            }
            client.geometry.x = hidden_x;
            // y, w, h stay unchanged
        }

        let cfg = CONFIG.load();
        if cfg.animation_enabled() {
            let now = Instant::now();
            let visual = self
                .animations
                .current_visual_rect(client_key, now)
                .unwrap_or(Rect::new(x, y, w, h));
            let target = Rect::new(hidden_x, y, w, h);
            self.animations.start(
                client_key,
                visual,
                target,
                cfg.animation_duration(),
                cfg.animation_easing(),
                AnimationKind::Hide,
            );
            // When compositor is active, move the actual X11 window to the
            // hidden position immediately.  The compositor handles the visual
            // slide-out via the scene, but the X server delivers input events
            // based on the real window geometry — without this the hidden
            // window still receives hover/click events at its old position.
            if backend.has_compositor() {
                if let Err(e) = self.move_window(backend, win, hidden_x, y) {
                    warn!(
                        "[hide_client] Failed to move window off-screen {:?}: {:?}",
                        win, e
                    );
                }
            }
        } else {
            if let Err(e) = self.move_window(backend, win, hidden_x, y) {
                warn!("[hide_client] Failed to hide window {:?}: {:?}", win, e);
            }
        }
    }
}
