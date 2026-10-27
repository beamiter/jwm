use crate::backend::api::Backend;
use crate::backend::common_define::WindowId;
use crate::config::CONFIG;
use crate::core::animation::AnimationKind;
use crate::core::models::ClientKey;
use crate::core::types::Rect;
use std::time::Instant;

use super::Jwm;

impl Jwm {
    pub(super) fn resize_client(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        mut x: i32,
        mut y: i32,
        mut w: i32,
        mut h: i32,
        interact: bool,
    ) {
        if self
            .applysizehints(
                backend, client_key, &mut x, &mut y, &mut w, &mut h, interact,
            )
            .is_ok()
        {
            let _ = self.resizeclient(backend, client_key, x, y, w, h);
        }
    }

    /// `resizeclient` for a window already in fullscreen (or otherwise
    /// holding a return rectangle in `old_*`): refit it without overwriting
    /// the slot it returns to when fullscreen ends. Plain `resizeclient`
    /// copies the live — monitor-sized — rectangle there, so leaving
    /// fullscreen after an output change or a monitor move would restore a
    /// monitor-sized window.
    pub(super) fn refit_keeping_restore_slot(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        rect: Rect,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let saved = self.state.clients.get(client_key).map(|client| {
            (
                client.geometry.old_x,
                client.geometry.old_y,
                client.geometry.old_w,
                client.geometry.old_h,
            )
        });
        let result = self.resizeclient(backend, client_key, rect.x, rect.y, rect.w, rect.h);
        if let (Some((x, y, w, h)), Some(client)) = (saved, self.state.clients.get_mut(client_key))
        {
            client.geometry.old_x = x;
            client.geometry.old_y = y;
            client.geometry.old_w = w;
            client.geometry.old_h = h;
        }
        result
    }

    pub(super) fn resizeclient(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(client) = self.state.clients.get_mut(client_key) {
            client.geometry.old_x = client.geometry.x;
            client.geometry.old_y = client.geometry.y;
            client.geometry.old_w = client.geometry.w;
            client.geometry.old_h = client.geometry.h;

            client.geometry.x = x;
            client.geometry.y = y;
            client.geometry.w = w;
            client.geometry.h = h;

            // When compositor is active, borders are rendered by the compositor
            // (Pass 3) — tell X11 the border is 0 so it doesn't draw its own.
            let x11_bw = if backend.has_compositor() {
                0
            } else {
                client.geometry.border_w as u32
            };

            let cfg = CONFIG.load();
            if cfg.motion_enabled() && !self.suppress_layout_animation {
                let old_rect = Rect::new(
                    client.geometry.old_x,
                    client.geometry.old_y,
                    client.geometry.old_w,
                    client.geometry.old_h,
                );
                let target = Rect::new(x, y, w, h);
                let duration = cfg.animation_duration();
                let easing = cfg.animation_easing();
                drop(cfg);
                let now = Instant::now();
                let visual = self
                    .animations
                    .current_visual_rect(client_key, now)
                    .unwrap_or(old_rect);
                // Repeated floating show/arrange calls must preserve an
                // Appear trajectory as well as its original deadline. A real
                // layout change retargets from the sampled visual rectangle.
                if !self
                    .animations
                    .active
                    .get(&client_key)
                    .is_some_and(|animation| animation.to == target)
                {
                    self.animations.start(
                        client_key,
                        visual,
                        target,
                        duration,
                        easing,
                        AnimationKind::Layout,
                    );
                }
                // When compositor is active, move the actual X11 window to the
                // target position immediately.  The compositor handles visual
                // interpolation via the scene, but the X server delivers input
                // events based on the real window geometry — so the window must
                // be at the correct position for clicks to work.
                //
                // When compositor is OFF, we still need to position the X11 window
                // at the animation's starting point (visual) so that tick_animations
                // can animate from the correct position. Without this, the window
                // might be off-screen or at a stale position, causing visual glitches.
                if backend.has_compositor() {
                    backend
                        .window_ops()
                        .configure(client.win, x, y, w as u32, h as u32, x11_bw)?;
                } else {
                    backend.window_ops().configure(
                        client.win,
                        visual.x,
                        visual.y,
                        visual.w as u32,
                        visual.h as u32,
                        x11_bw,
                    )?;
                }
            } else {
                backend
                    .window_ops()
                    .configure(client.win, x, y, w as u32, h as u32, x11_bw)?;
                // Immediate placement supersedes any earlier layout/hide
                // trajectory, including when a workspace transition owns the
                // motion or live configuration disables animations.
                self.animations.remove(client_key);
            }
        }
        Ok(())
    }

    pub(super) fn getrootptr(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(i32, i32), Box<dyn std::error::Error>> {
        let (x, y) = backend.input_ops().get_pointer_position()?;
        // Monitor rectangles are half-open: use the containing pixel, including
        // for fractional coordinates immediately left of or above zero.
        Ok((x.floor() as i32, y.floor() as i32))
    }

    pub(super) fn recttomon(
        &mut self,
        backend: &mut dyn Backend,
        x: i32,
        y: i32,
    ) -> Option<super::MonitorKey> {
        if let Some(output_id) = backend.output_ops().output_at(x, y) {
            for (mon_key, &oid) in &self.state.output_map {
                if oid == output_id {
                    return Some(mon_key);
                }
            }
        }
        self.state.sel_mon
    }

    /// A dropped window belongs to the output showing the largest part of
    /// its outer rectangle. Keep its home output on ties and off-screen drops;
    /// pointer hit testing continues to use `recttomon`.
    pub(crate) fn monitor_for_client_drop(
        &self,
        client_key: ClientKey,
    ) -> Option<super::MonitorKey> {
        let client = self.state.clients.get(client_key)?;
        let home = client
            .mon
            .filter(|key| self.state.monitors.contains_key(*key))
            .or_else(|| self.state.monitor_order.first().copied())?;
        let x = i64::from(client.geometry.x);
        let y = i64::from(client.geometry.y);
        let border = i64::from(client.geometry.border_w.max(0)) * 2;
        let right = x + (i64::from(client.geometry.w) + border).max(0);
        let bottom = y + (i64::from(client.geometry.h) + border).max(0);
        let area = |key| {
            let Some(monitor) = self.state.monitors.get(key) else {
                return 0;
            };
            let g = &monitor.geometry;
            let left = i64::from(g.m_x);
            let top = i64::from(g.m_y);
            let width = (right.min(left + i64::from(g.m_w.max(0))) - x.max(left)).max(0);
            let height = (bottom.min(top + i64::from(g.m_h.max(0))) - y.max(top)).max(0);
            width * height
        };
        let mut target = home;
        let mut largest = area(home);
        for &key in &self.state.monitor_order {
            let overlap = area(key);
            if overlap > largest {
                target = key;
                largest = overlap;
            }
        }
        Some(target)
    }

    pub(super) fn wintomon(
        &mut self,
        backend: &mut dyn Backend,
        w: Option<WindowId>,
    ) -> Option<super::MonitorKey> {
        if w.is_none() || w == backend.root_window() {
            if let Ok((x, y)) = self.getrootptr(backend) {
                return self.recttomon(backend, x, y);
            }
            return self.state.sel_mon;
        }
        let win_id = match w {
            Some(id) => id,
            None => return self.state.sel_mon,
        };
        if let Some(client_key) = self.wintoclient(win_id) {
            if let Some(client) = self.state.clients.get(client_key) {
                return client.mon.or(self.state.sel_mon);
            }
        }
        self.state.sel_mon
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::animation::Easing;
    use crate::core::models::WMClient;
    use crate::jwm::monitor::test_support::{DisplaySpyBackend, output};
    use std::time::Duration;

    #[test]
    fn fractional_pointer_motion_stays_on_the_containing_output() {
        use crate::backend::api::HitTarget;
        use crate::core::controller::WMController;

        for (origin, vertical) in [(0, false), (-1920, false), (0, true), (-1080, true)] {
            let (width, height) = (1920, 1080);
            let (x, y) = if vertical { (0, origin) } else { (origin, 0) };
            let (next_x, next_y) = if vertical {
                (x, y + height)
            } else {
                (x + width, y)
            };
            let mut backend = DisplaySpyBackend::new(vec![
                output(1, x, y, width, height),
                output(2, next_x, next_y, width, height),
            ]);
            let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
            let first = jwm.state.monitor_order[0];
            let second = jwm.state.monitor_order[1];
            let edge = f64::from(if vertical { next_y } else { next_x });
            for (coordinate, expected) in [
                (edge - 0.25, first),
                (edge.next_down(), first),
                (edge, second),
            ] {
                jwm.suppress_mouse_focus_until = None;
                let (px, py) = if vertical {
                    (100.0, coordinate)
                } else {
                    (coordinate, 100.0)
                };
                <Jwm as WMController>::on_motion_notify(
                    &mut jwm,
                    &mut backend,
                    HitTarget::Background { output: None },
                    px,
                    py,
                    0,
                );
                assert_eq!(jwm.state.motion_mon, Some(expected));
            }
        }
    }

    #[test]
    fn dropped_windows_follow_largest_overlap_in_every_direction() {
        for (second, rect, home, expected) in [
            ((1920, 0), (1800, 100, 800, 400), 0, 1),
            ((1920, 0), (1400, 100, 800, 400), 1, 0),
            ((0, 1080), (100, 1000, 800, 400), 0, 1),
            ((0, 1080), (100, 800, 800, 400), 1, 0),
            ((-1920, 0), (-600, 100, 800, 400), 0, 1),
            ((1920, 0), (1520, 100, 800, 400), 1, 1),
            ((1920, 0), (1520, 100, 800, 400), 0, 0),
            ((1920, 0), (5000, 3000, 800, 400), 1, 1),
        ] {
            let mut backend = DisplaySpyBackend::new(vec![
                output(1, 0, 0, 1920, 1080),
                output(2, second.0, second.1, 1920, 1080),
            ]);
            let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
            let monitors = jwm.state.monitor_order.clone();
            let mut client = WMClient::new(WindowId::from_raw(42));
            client.mon = Some(monitors[home]);
            client.geometry.x = rect.0;
            client.geometry.y = rect.1;
            client.geometry.w = rect.2;
            client.geometry.h = rect.3;
            client.geometry.border_w = 0;
            let key = jwm.state.clients.insert(client);
            assert_eq!(
                jwm.monitor_for_client_drop(key),
                Some(monitors[expected]),
                "second={second:?} rect={rect:?} home={home}"
            );
        }
    }

    #[test]
    fn dropped_window_overlap_includes_borders_and_ignores_workarea() {
        let mut backend = DisplaySpyBackend::new(vec![
            output(1, 0, 0, 1920, 1080),
            output(2, 1920, 0, 1920, 1080),
        ]);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitors = jwm.state.monitor_order.clone();
        jwm.state.monitors[monitors[1]].geometry.w_x = 2200;
        let mut client = WMClient::new(WindowId::from_raw(42));
        client.mon = Some(monitors[0]);
        client.geometry.x = 1810;
        client.geometry.y = 0;
        client.geometry.w = 200;
        client.geometry.h = 100;
        client.geometry.border_w = 20;
        let key = jwm.state.clients.insert(client);
        assert_eq!(jwm.monitor_for_client_drop(key), Some(monitors[1]));
    }

    #[test]
    fn dropped_window_overlap_uses_wide_coordinates_and_areas() {
        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let g = &mut jwm.state.monitors[monitor].geometry;
        g.m_x = i32::MAX - 10;
        g.m_y = i32::MAX - 10;
        g.m_w = i32::MAX;
        g.m_h = i32::MAX;
        let mut client = WMClient::new(WindowId::from_raw(42));
        client.mon = Some(monitor);
        client.geometry.x = i32::MAX - 5;
        client.geometry.y = i32::MAX - 5;
        client.geometry.w = i32::MAX;
        client.geometry.h = i32::MAX;
        client.geometry.border_w = i32::MAX;
        let key = jwm.state.clients.insert(client);
        assert_eq!(jwm.monitor_for_client_drop(key), Some(monitor));
    }

    #[test]
    fn immediate_resize_cancels_a_previous_animation() {
        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let key = jwm
            .state
            .clients
            .insert(WMClient::new(WindowId::from_raw(42)));
        for kind in [
            AnimationKind::Layout,
            AnimationKind::Hide,
            AnimationKind::Appear,
        ] {
            jwm.animations.start(
                key,
                Rect::new(0, 0, 100, 100),
                Rect::new(200, 0, 100, 100),
                Duration::from_secs(1),
                Easing::Linear,
                kind,
            );
            jwm.suppress_layout_animation = true;
            jwm.resizeclient(&mut backend, key, 400, 300, 500, 400)
                .unwrap();
            assert!(!jwm.animations.active.contains_key(&key));
            let client = &jwm.state.clients[key];
            assert_eq!(client.rect(), (400, 300, 500, 400));
        }
    }
}
