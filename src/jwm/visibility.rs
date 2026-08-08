//! 窗口显示控制模块
//!
//! 这个模块负责管理窗口的显示和隐藏，包括动画效果

use crate::backend::api::Backend;
use crate::config::CONFIG;
use crate::core::animation::AnimationKind;
use crate::core::models::{ClientGeometry, ClientKey, MonitorKey};
use crate::core::types::Rect;
use crate::jwm::Jwm;
use log::warn;
use std::time::Instant;

pub(super) fn hidden_x_left_of_desktop(desktop_left: i32, total_width: i32) -> i32 {
    desktop_left.saturating_sub(total_width.max(1).saturating_mul(2))
}

fn is_fully_left_of_desktop(x: i32, total_width: i32, desktop_left: i32) -> bool {
    x.saturating_add(total_width.max(1)) <= desktop_left
}

/// Park one geometry without borrowing the layout/fullscreen `old_*` slot.
/// Repeated hides preserve the first visible rectangle until a show consumes
/// it.
pub(super) fn stage_hidden_geometry(geometry: &mut ClientGeometry, restore: Rect, hidden_x: i32) {
    if geometry.hidden_restore_rect.is_none() {
        geometry.hidden_restore_rect = Some(restore);
    }
    geometry.x = hidden_x;
    geometry.hidden_x = Some(hidden_x);
}

/// Consume a parked geometry. `legacy_fallback_x` is used only for a client
/// left by an older JWM whose overloaded `old_x` is itself off-screen.
pub(super) fn restore_hidden_geometry(
    geometry: &mut ClientGeometry,
    desktop_left: i32,
    legacy_fallback_x: i32,
) -> Option<Rect> {
    let total_width = geometry
        .w
        .saturating_add(geometry.border_w.saturating_mul(2));
    let had_hidden_marker = geometry.hidden_x.take().is_some();
    let restore = geometry.hidden_restore_rect.take();
    let was_parked = had_hidden_marker
        || restore.is_some()
        || is_fully_left_of_desktop(geometry.x, total_width, desktop_left);
    if !was_parked {
        return None;
    }

    let restore = restore.unwrap_or_else(|| {
        let x = if is_fully_left_of_desktop(geometry.old_x, total_width, desktop_left) {
            legacy_fallback_x
        } else {
            geometry.old_x
        };
        Rect::new(x, geometry.y, geometry.w, geometry.h)
    });
    geometry.x = restore.x;
    geometry.y = restore.y;
    geometry.w = restore.w;
    geometry.h = restore.h;
    Some(restore)
}

impl Jwm {
    pub(super) fn desktop_left_edge(&self) -> i32 {
        self.state
            .monitors
            .values()
            .map(|monitor| monitor.geometry.m_x)
            .min()
            .unwrap_or(0)
    }

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
        // Dock windows manage their own visibility via togglebar /
        // position_secondary_bar_on_monitor.  Letting show_client run on them
        // causes an infinite loop: move_window sends ConfigureWindow with the
        // hidden position, then resize_client's constrain_to_monitor clamps it
        // by 1 px, generating a ConfigureNotify that re-triggers arrange.
        if self
            .state
            .clients
            .get(client_key)
            .map(|c| c.state.is_dock)
            .unwrap_or(false)
        {
            return;
        }

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

        // Prefer the semantic restore slot. The geometric fallback adopts
        // clients hidden by an older JWM that only left `old_x` plus an
        // off-screen coordinate. Comparing with the desktop's true left edge
        // keeps legitimate negative-origin windows from being mistaken for
        // hidden clients.
        let desktop_left = self.desktop_left_edge();
        let legacy_fallback_x = self
            .state
            .clients
            .get(client_key)
            .and_then(|client| client.mon)
            .and_then(|monitor| self.monitor_work_area(monitor))
            .map_or(desktop_left, |area| area.x);
        if let Some(client) = self.state.clients.get_mut(client_key) {
            restore_hidden_geometry(&mut client.geometry, desktop_left, legacy_fallback_x);
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

        let hidden_x = hidden_x_left_of_desktop(self.desktop_left_edge(), width);

        // Save visible geometry in its own semantic slot so show_client can
        // restore it without overwriting `old_*` (the layout/fullscreen
        // previous rectangle), then update client.geometry to the hidden
        // position. This prevents
        // tick_animations from snapping the window back on-screen when the
        // Hide animation completes.
        //
        // The explicit restore slot survives repeated arrange calls and output
        // topology changes, so neither can overwrite the last visible geometry
        // with an obsolete off-screen coordinate.
        if let Some(client) = self.state.clients.get_mut(client_key) {
            let restore = Rect::new(
                client.geometry.x,
                client.geometry.y,
                client.geometry.w,
                client.geometry.h,
            );
            stage_hidden_geometry(&mut client.geometry, restore, hidden_x);
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

#[cfg(test)]
mod tests {
    use super::{
        hidden_x_left_of_desktop, is_fully_left_of_desktop, restore_hidden_geometry,
        stage_hidden_geometry,
    };
    use crate::core::models::ClientGeometry;
    use crate::core::types::Rect;

    #[test]
    fn hidden_position_stays_left_of_negative_origin_outputs() {
        let hidden_x = hidden_x_left_of_desktop(-1920, 800);
        assert_eq!(hidden_x, -3520);
        assert!(is_fully_left_of_desktop(hidden_x, 800, -1920));
        assert!(!is_fully_left_of_desktop(-1600, 800, -1920));
    }

    #[test]
    fn hidden_position_saturates_instead_of_overflowing() {
        let hidden_x = hidden_x_left_of_desktop(i32::MIN + 10, i32::MAX);
        assert_eq!(hidden_x, i32::MIN);
        assert!(is_fully_left_of_desktop(hidden_x, i32::MAX, 0));
    }

    #[test]
    fn fullscreen_hide_restore_never_overwrites_the_layout_return_rectangle() {
        let pre_fullscreen = Rect::new(240, 120, 960, 720);
        let fullscreen = Rect::new(0, 0, 1920, 1080);
        let mut geometry = ClientGeometry {
            x: fullscreen.x,
            y: fullscreen.y,
            w: fullscreen.w,
            h: fullscreen.h,
            old_x: pre_fullscreen.x,
            old_y: pre_fullscreen.y,
            old_w: pre_fullscreen.w,
            old_h: pre_fullscreen.h,
            ..Default::default()
        };

        stage_hidden_geometry(&mut geometry, fullscreen, -3840);
        // A repeated arrange/hide observes the parked coordinate but must keep
        // the first visible rectangle.
        let parked = Rect::new(geometry.x, geometry.y, geometry.w, geometry.h);
        stage_hidden_geometry(&mut geometry, parked, -3840);

        assert_eq!(geometry.hidden_restore_rect, Some(fullscreen));
        assert_eq!(
            Rect::new(
                geometry.old_x,
                geometry.old_y,
                geometry.old_w,
                geometry.old_h,
            ),
            pre_fullscreen
        );
        assert_eq!(
            restore_hidden_geometry(&mut geometry, 0, 0),
            Some(fullscreen)
        );
        assert_eq!(
            Rect::new(geometry.x, geometry.y, geometry.w, geometry.h),
            fullscreen
        );

        // This is the geometry assignment performed when fullscreen exits.
        geometry.x = geometry.old_x;
        geometry.y = geometry.old_y;
        geometry.w = geometry.old_w;
        geometry.h = geometry.old_h;
        assert_eq!(
            Rect::new(geometry.x, geometry.y, geometry.w, geometry.h),
            pre_fullscreen
        );
    }

    #[test]
    fn legacy_offscreen_client_uses_a_visible_fallback_without_a_restore_slot() {
        let mut geometry = ClientGeometry {
            x: -4000,
            y: 70,
            w: 800,
            h: 600,
            old_x: -4000,
            hidden_x: Some(-4000),
            ..Default::default()
        };

        assert_eq!(
            restore_hidden_geometry(&mut geometry, -1920, -1920),
            Some(Rect::new(-1920, 70, 800, 600))
        );
        assert!(geometry.hidden_x.is_none());
        assert!(geometry.hidden_restore_rect.is_none());
    }
}
