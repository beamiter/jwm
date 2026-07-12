use crate::backend::api::Backend;
use crate::backend::common_define::{SchemeType, WindowId};
use crate::config::CONFIG;
use crate::core::models::ClientKey;
use crate::core::types::Rect;
use log::info;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use super::Jwm;

type SceneEntry = (u64, i32, i32, u32, u32);

fn compositor_debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();

    *ENABLED.get_or_init(|| {
        std::env::var("JWM_DEBUG_COMPOSITOR")
            .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

fn push_scene_window(
    jwm: &Jwm,
    scene: &mut Vec<SceneEntry>,
    secondary_bar_wins: &HashSet<WindowId>,
    visual_overrides: &HashMap<ClientKey, Rect>,
    win_id: WindowId,
) {
    if secondary_bar_wins.contains(&win_id) {
        return;
    }

    let Some(&client_key) = jwm.state.win_to_client.get(&win_id) else {
        return;
    };
    let Some(client) = jwm.state.clients.get(client_key) else {
        return;
    };

    let (x, y, w, h) = if let Some(rect) = visual_overrides.get(&client_key) {
        (rect.x, rect.y, rect.w as u32, rect.h as u32)
    } else {
        (
            client.geometry.x,
            client.geometry.y,
            client.geometry.w as u32,
            client.geometry.h as u32,
        )
    };

    if w > 0 && h > 0 {
        scene.push((win_id.raw(), x, y, w, h));
    }
}

impl Jwm {
    #[allow(dead_code)]
    pub(super) fn render_compositor_immediate(&mut self, backend: &mut dyn Backend) {
        if !backend.has_compositor() {
            return;
        }
        // Skip if animations are active — tick_animations handles rendering
        // during animation frames, so we don't want to double-render.
        if self.animations.has_active() {
            return;
        }
        // When overview is active the prism rotation runs inside the render
        // pass (tick_overview_prism), but clear_needs_render() after
        // render_frame() wipes the flag it sets.  So we must keep rendering
        // every frame unconditionally while overview is up; vsync provides
        // natural ~60 fps pacing.
        if !backend.compositor_needs_render() && !self.features.overview.active {
            return;
        }
        let scene = self.build_compositor_scene(backend, &HashMap::new());
        let groups = self.build_window_groups();
        backend.compositor_set_window_groups(groups);
        let focused = self
            .get_selected_client_key()
            .and_then(|ck| self.state.clients.get(ck))
            .map(|c| c.win.raw());
        let _ = backend.compositor_render_frame(&scene, focused);
    }

    pub(super) fn tick_animations(&mut self, backend: &mut dyn Backend) {
        // --- Night Light: update color temperature once per minute ---
        if backend.has_compositor() {
            let should_update = match self.last_night_light_update {
                Some(last) => last.elapsed() >= Duration::from_secs(60),
                None => true,
            };
            if should_update {
                self.last_night_light_update = Some(Instant::now());
                let cfg = CONFIG.load();
                let behavior = cfg.behavior();
                if behavior.night_light {
                    let temp = Self::compute_night_light_temp(
                        &behavior.night_light_start,
                        &behavior.night_light_end,
                        behavior.night_light_temp,
                        behavior.night_light_transition_mins,
                    );
                    backend.compositor_set_color_temperature(temp);
                } else {
                    backend.compositor_set_color_temperature(0.0);
                }
            }
        }

        let composited = backend.has_compositor();

        if !self.animations.has_active() {
            if composited && backend.compositor_needs_render() {
                // No animations but compositor has dirty windows (damage, add/remove, resize)
                let scene = self.build_compositor_scene(backend, &HashMap::new());
                if scene.is_empty() {
                    // Log once per second at most
                    static LAST_EMPTY: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(0);
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let prev = LAST_EMPTY.load(std::sync::atomic::Ordering::Relaxed);
                    if now > prev {
                        LAST_EMPTY.store(now, std::sync::atomic::Ordering::Relaxed);
                        log::warn!(
                            "[tick_animations] compositor scene is EMPTY (no windows to render)"
                        );
                    }
                }
                let focused = self
                    .get_selected_client_key()
                    .and_then(|ck| self.state.clients.get(ck))
                    .map(|c| c.win.raw());
                let _ = backend.compositor_render_frame(&scene, focused);
            }
            return;
        }

        let now = Instant::now();
        let active_animation_count = self.animations.active.len();
        let mut completed = Vec::with_capacity(active_animation_count);
        let mut visual_overrides: HashMap<ClientKey, Rect> =
            HashMap::with_capacity(active_animation_count);

        let keys: Vec<ClientKey> = self.animations.active.keys().copied().collect();
        for key in keys {
            let anim = match self.animations.active.get(&key) {
                Some(a) => a,
                None => continue,
            };
            let (rect, done) = anim.sample(now);

            if self.state.clients.get(key).is_none() {
                completed.push(key);
                continue;
            }

            if composited {
                // Store visual override — compositor draws at interpolated position.
                // Real window is already at the target position (set by resizeclient).
                visual_overrides.insert(key, rect);
            } else {
                // Non-composited fallback: physically move the window each frame
                if let Some(client) = self.state.clients.get(key) {
                    let _ = backend.window_ops().configure(
                        client.win,
                        rect.x,
                        rect.y,
                        rect.w as u32,
                        rect.h as u32,
                        client.geometry.border_w as u32,
                    );
                }
            }

            if done {
                completed.push(key);
            }
        }

        if composited {
            let scene = self.build_compositor_scene(backend, &visual_overrides);
            let focused = self
                .get_selected_client_key()
                .and_then(|ck| self.state.clients.get(ck))
                .map(|c| c.win.raw());
            let _ = backend.compositor_render_frame(&scene, focused);
        }

        for key in completed {
            self.animations.active.remove(&key);
        }
    }

    /// Build window tab groups: one group per monitor, containing visible tiled windows.
    /// The focused window is marked as active tab.
    #[allow(dead_code)]
    pub(super) fn build_window_groups(&self) -> Vec<(u32, Vec<(u32, String, bool)>)> {
        let mut groups = Vec::with_capacity(self.state.monitor_order.len());
        let focused_ck = self.get_selected_client_key();
        for (i, &mon_key) in self.state.monitor_order.iter().enumerate() {
            let monitor_clients = self.get_monitor_clients(mon_key);
            let mut tabs = Vec::with_capacity(monitor_clients.len());
            for &ck in monitor_clients {
                if !self.is_client_visible_on_monitor(ck, mon_key) {
                    continue;
                }
                let client = match self.state.clients.get(ck) {
                    Some(c) => c,
                    None => continue,
                };
                if client.state.is_floating || client.state.is_fullscreen {
                    continue;
                }
                let is_active = focused_ck == Some(ck);
                tabs.push((client.win.raw() as u32, client.name.clone(), is_active));
            }
            if tabs.len() > 1 {
                groups.push((i as u32, tabs));
            }
        }
        groups
    }

    /// Build an ordered scene for the compositor: Vec<(window_id_raw, x, y, w, h)>
    /// from bottom to top, using the last_stacking order. For windows with
    /// active animation overrides, use the interpolated rect instead of actual geometry.
    pub(super) fn build_compositor_scene(
        &self,
        backend: &dyn Backend,
        visual_overrides: &HashMap<ClientKey, Rect>,
    ) -> Vec<SceneEntry> {
        let estimated_window_count = self.state.client_order.len()
            + self.secondary_bars.len()
            + self.override_redirect_windows.len();
        let mut scene = Vec::with_capacity(estimated_window_count);
        let debug_compositor = compositor_debug_enabled();

        // Secondary bars are appended explicitly after managed windows. Build this
        // lookup once per frame rather than once for every monitor.
        let secondary_bar_wins: HashSet<WindowId> = self
            .secondary_bars
            .values()
            .filter_map(|bar_instance| {
                let bar_key = bar_instance.client_key?;
                Some(self.state.clients.get(bar_key)?.win)
            })
            .collect();

        // Iterate all monitors, using last_stacking order (bottom to top)
        for &mon_key in &self.state.monitor_order {
            if debug_compositor {
                let has_stacking = self.last_stacking.get(mon_key).is_some();
                let stack_len = self
                    .last_stacking
                    .get(mon_key)
                    .map(|s| s.len())
                    .unwrap_or(0);
                let client_count = self
                    .state
                    .monitor_clients
                    .get(mon_key)
                    .map(|c| c.len())
                    .unwrap_or(0);
                info!(
                    "[compositor_scene] mon={:?} has_stacking={} stack_len={} clients={}",
                    mon_key, has_stacking, stack_len, client_count
                );
            }

            // Use last_stacking if available, otherwise fall back to
            // monitor_stack so the compositor still has something to render
            // when restack() hasn't run yet for this monitor. Iterate borrowed
            // storage directly to avoid cloning/collecting a temporary Vec every frame.
            if let Some(stacking) = self.last_stacking.get(mon_key) {
                for &win_id in stacking {
                    push_scene_window(
                        self,
                        &mut scene,
                        &secondary_bar_wins,
                        visual_overrides,
                        win_id,
                    );
                }
            } else if let Some(stack) = self.state.monitor_stack.get(mon_key) {
                // monitor_stack is top-to-bottom, so traverse it in reverse.
                for &client_key in stack.iter().rev() {
                    let Some(client) = self.state.clients.get(client_key) else {
                        continue;
                    };
                    if !self.is_client_visible_on_monitor(client_key, mon_key) {
                        continue;
                    }
                    push_scene_window(
                        self,
                        &mut scene,
                        &secondary_bar_wins,
                        visual_overrides,
                        client.win,
                    );
                }
            }
        }

        // Also include the status bar if present — but skip it when a large
        // override-redirect window (e.g. screenshot overlay) covers the bar area.
        // RGBA OR overlays don't participate in occlusion culling, so without
        // this check the real status bar would render beneath the overlay's
        // semi-transparent region, producing a "double bar" artifact.
        let overlay_win = backend.compositor_overlay_window();
        // Include per-monitor secondary status bars
        for bar_instance in self.secondary_bars.values() {
            if let Some(bar_key) = bar_instance.client_key {
                if let Some(bar) = self.state.clients.get(bar_key) {
                    let w = bar.geometry.w as u32;
                    let h = bar.geometry.h as u32;
                    if w > 0 && h > 0 {
                        scene.push((bar.win.raw(), bar.geometry.x, bar.geometry.y, w, h));
                    }
                }
            }
        }

        // Include override-redirect windows (menus, dmenu, tooltips) on top.
        // These are not managed by the WM but must be composited.
        // Filter out the compositor's overlay window to avoid feedback loops.
        // Use cached geometries to avoid synchronous GetGeometry round-trips
        // on every frame (which add per-window X11 latency).
        for &or_win in &self.override_redirect_windows {
            if Some(or_win) == overlay_win {
                continue;
            }
            if let Some(&(x, y, w, h)) = self.or_window_geometries.get(&or_win) {
                if w > 0 && h > 0 {
                    scene.push((or_win.raw(), x, y, w, h));
                }
            }
        }

        scene
    }

    pub(super) fn sync_focused_floating_geometry(&mut self, backend: &mut dyn Backend) {
        let sel_key = match self.get_selected_client_key() {
            Some(k) => k,
            None => return,
        };
        let win = match self.state.clients.get(sel_key) {
            Some(c) if c.state.is_floating => c.win,
            _ => return,
        };
        let geom = match backend.window_ops().get_geometry(win) {
            Ok(g) => g,
            Err(_) => return,
        };
        if let Some(client) = self.state.clients.get_mut(sel_key) {
            client.geometry.x = geom.x as i32;
            client.geometry.y = geom.y as i32;
            client.geometry.w = geom.w as i32;
            client.geometry.h = geom.h as i32;
            client.geometry.floating_x = geom.x as i32;
            client.geometry.floating_y = geom.y as i32;
            client.geometry.floating_w = geom.w as i32;
            client.geometry.floating_h = geom.h as i32;
        }
    }

    pub(super) fn configure_client(
        &self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(client) = self.state.clients.get(client_key) {
            // Compositor renders borders via GPU — tell X11 border is 0.
            let x11_bw = if backend.has_compositor() {
                0
            } else {
                client.geometry.border_w as u32
            };

            backend.window_ops().configure(
                client.win,
                client.geometry.x,
                client.geometry.y,
                client.geometry.w as u32,
                client.geometry.h as u32,
                x11_bw,
            )?;

            // 分离装饰设置
            let border_color = backend
                .color_allocator()
                .get_border_pixel_of(SchemeType::Norm)?;
            backend
                .window_ops()
                .set_decoration_style(client.win, x11_bw, border_color)?;
        }
        Ok(())
    }

    pub(super) fn move_window(
        &mut self,
        backend: &mut dyn Backend,
        win: WindowId,
        x: i32,
        y: i32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        backend.window_ops().set_position(win, x, y)?;
        Ok(())
    }
}
