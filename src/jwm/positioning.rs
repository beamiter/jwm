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
            if cfg.animation_enabled() && !self.suppress_layout_animation {
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
                self.animations.start(
                    client_key,
                    visual,
                    target,
                    duration,
                    easing,
                    AnimationKind::Layout,
                );
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
            }
        }
        Ok(())
    }

    pub(super) fn getrootptr(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(i32, i32), Box<dyn std::error::Error>> {
        let (x, y) = backend.input_ops().get_pointer_position()?;
        Ok((x as i32, y as i32))
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
