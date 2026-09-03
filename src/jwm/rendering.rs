use crate::backend::api::Backend;
use crate::backend::common_define::{Pixel, SchemeType, WindowId};
use crate::backend::error::BackendError;
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
    pub(crate) fn battery_next_wakeup(&self, now: Instant) -> Duration {
        self.last_battery_poll.map_or(Duration::ZERO, |last| {
            crate::jwm::features::power::POLL_INTERVAL
                .saturating_sub(now.saturating_duration_since(last))
        })
    }

    /// Apply the X11 presentation contract for one compositor mode.
    ///
    /// Native mode needs server-side borders and, for animations, the sampled
    /// visual rectangle as the real window geometry. Composited mode keeps X
    /// borders at zero and places animated input windows at their logical
    /// target while the renderer interpolates the pixels independently.
    fn sync_x11_client_presentation(
        &self,
        backend: &mut dyn Backend,
        composited: bool,
        now: Instant,
    ) -> Result<(), BackendError> {
        if !backend.capabilities().supports_client_list {
            return Ok(());
        }

        let focused = self.get_selected_client_key();
        let attention_enabled = CONFIG.load().behavior().attention_animation;
        let entries: Vec<_> = self
            .state
            .clients
            .iter()
            .map(|(client_key, client)| {
                let border = if composited {
                    0
                } else {
                    client.geometry.border_w.max(0) as u32
                };
                let scheme = super::window_state::client_decoration_scheme(
                    focused == Some(client_key),
                    client.state.is_urgent,
                    attention_enabled,
                );
                // Hidden/Iconic clients have already crossed the checked
                // parking barrier. Never let a retained visual animation move
                // their real X window back out of that safe native slot.
                let animation_rect = (!client.state.is_hidden)
                    .then(|| self.animations.active.get(&client_key))
                    .flatten()
                    .map(|animation| {
                        if composited {
                            Rect::new(
                                client.geometry.x,
                                client.geometry.y,
                                client.geometry.w,
                                client.geometry.h,
                            )
                        } else {
                            animation.sample(now).0
                        }
                    });
                (client.win, border, scheme, animation_rect)
            })
            .collect();

        let mut failures = Vec::new();
        let mut pixels: HashMap<SchemeType, Option<Pixel>> = HashMap::new();
        for &(_, _, scheme, _) in &entries {
            if let std::collections::hash_map::Entry::Vacant(entry) = pixels.entry(scheme) {
                match backend.color_allocator().get_border_pixel_of(scheme) {
                    Ok(pixel) => {
                        entry.insert(Some(pixel));
                    }
                    Err(error) => {
                        failures.push(format!("allocate {scheme:?} border: {error}"));
                        entry.insert(None);
                    }
                }
            }
        }

        for (window, border, scheme, animation_rect) in entries {
            if let Some(pixel) = pixels.get(&scheme).copied().flatten()
                && let Err(error) = backend
                    .window_ops()
                    .set_decoration_style(window, border, pixel)
            {
                failures.push(format!("decorate {window:?}: {error}"));
            }
            if let Some(rect) = animation_rect {
                if let Err(error) = backend.window_ops().configure(
                    window,
                    rect.x,
                    rect.y,
                    rect.w.max(1) as u32,
                    rect.h.max(1) as u32,
                    border,
                ) {
                    failures.push(format!("configure {window:?}: {error}"));
                }
            }
        }
        if let Err(error) = backend.window_ops().flush() {
            failures.push(format!("flush X11 presentation: {error}"));
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(BackendError::Message(format!(
                "X11 presentation reconciliation had {} failure(s): {}",
                failures.len(),
                failures.join("; ")
            )))
        }
    }

    fn replay_compositor_runtime_state(&mut self, backend: &mut dyn Backend, now: Instant) {
        backend.compositor_apply_config();
        self.refresh_compositor_monitors(backend);
        // The compositor behind this replay may be freshly created with empty
        // groups, so push unconditionally and keep the delivery cache truthful
        // for the change-gated syncs that follow.
        let groups = self.build_window_groups();
        backend.compositor_set_window_groups(groups.clone());
        self.pushed_window_groups = groups;

        let window_states: Vec<_> = self
            .state
            .clients
            .values()
            .map(|client| (client.win, client.state.is_urgent, client.state.is_pip))
            .collect();
        for (window, urgent, pip) in window_states {
            backend.compositor_set_window_urgent(window, urgent);
            backend.compositor_set_window_pip(window, pip);
        }

        backend.compositor_set_debug_hud(self.debug_hud_on);
        backend.compositor_set_debug_hud_extended(self.debug_hud_on);
        backend.compositor_set_magnifier(self.features.magnifier.enabled);
        backend.compositor_set_peek_mode(self.features.peek_active);

        let behavior = CONFIG.load().behavior().clone();
        let temperature = match self.night_light_override {
            Some(true) => behavior.night_light_temp,
            Some(false) => 0.0,
            None if behavior.night_light => Self::compute_night_light_temp(
                &behavior.night_light_start,
                &behavior.night_light_end,
                behavior.night_light_temp,
                behavior.night_light_transition_mins,
            ),
            None => 0.0,
        };
        backend.compositor_set_color_temperature(temperature);
        self.last_night_light_update = Some(now);
        self.reapply_idle_dim(backend);
        backend.compositor_force_full_redraw();
    }

    /// Change compositor mode while keeping native X11 presentation and
    /// JWM-owned renderer state coherent across the hand-off.
    pub(crate) fn set_compositor_enabled_reconciled(
        &mut self,
        backend: &mut dyn Backend,
        enabled: bool,
    ) -> Result<bool, BackendError> {
        let before = backend.has_compositor();
        if before == enabled {
            return Ok(false);
        }
        let now = Instant::now();
        let attempt_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok());
        self.features
            .compositor_transition
            .begin(enabled, attempt_unix_ms);

        // Stage native borders/animation geometry behind the overlay, then
        // flush before XComposite exposes the real window tree.
        if !enabled {
            if let Err(error) = self.sync_x11_client_presentation(backend, false, now) {
                if let Err(rollback) = self.sync_x11_client_presentation(backend, true, now) {
                    log::warn!(
                        "could not restore composited client presentation after failed staging: {rollback}"
                    );
                }
                self.features.compositor_transition.fail(error.to_string());
                return Err(error);
            }
        }

        let transition = backend.set_compositor_enabled(enabled);
        let after = backend.has_compositor();
        match transition {
            Err(error) => {
                // A backend error can theoretically arrive after its mode bit
                // changed (for example a trailing flush failure). Keep the
                // observed presentation coherent even though the caller still
                // receives the error.
                if after == enabled {
                    if enabled {
                        if let Err(sync_error) =
                            self.sync_x11_client_presentation(backend, true, now)
                        {
                            log::warn!(
                                "could not fully switch clients to composited presentation after a partial enable: {sync_error}"
                            );
                        }
                        self.replay_compositor_runtime_state(backend, now);
                    }
                } else if !enabled
                    && let Err(rollback) = self.sync_x11_client_presentation(backend, true, now)
                {
                    log::warn!(
                        "could not restore composited client presentation after failed disable: {rollback}"
                    );
                }
                self.features.compositor_transition.fail(error.to_string());
                return Err(error);
            }
            Ok(_) if after != enabled => {
                if !enabled
                    && let Err(rollback) = self.sync_x11_client_presentation(backend, true, now)
                {
                    log::warn!(
                        "could not restore composited client presentation after refused disable: {rollback}"
                    );
                }
                let error = BackendError::Message(format!(
                    "backend reported compositor transition without reaching {} state",
                    if enabled { "enabled" } else { "disabled" }
                ));
                self.features.compositor_transition.fail(error.to_string());
                return Err(error);
            }
            Ok(_) => {}
        }

        if enabled {
            if let Err(error) = self.sync_x11_client_presentation(backend, true, now) {
                // A disappearing client must not tear the newly created
                // compositor back down; its lifecycle event will retire it.
                // The renderer is nevertheless only partially reconciled, so
                // do not publish this hand-off as a success or acknowledge it
                // to the caller. Replay the remaining runtime state first so
                // the compositor that is now active is as coherent as possible.
                let transition_error = BackendError::Message(format!(
                    "compositor is enabled, but X11 presentation reconciliation failed: {error}"
                ));
                log::warn!("{transition_error}");
                self.replay_compositor_runtime_state(backend, now);
                self.features
                    .compositor_transition
                    .fail(transition_error.to_string());
                return Err(transition_error);
            }
            self.replay_compositor_runtime_state(backend, now);
        }
        self.features.compositor_transition.succeed();
        Ok(after != before)
    }

    /// Hand the compositor the current tab groups when they changed since the
    /// last delivery. Returns true when new groups were pushed.
    ///
    /// The push must not wait on the compositor's `needs_render` flag: that
    /// flag can only learn about a groups change through this very delivery,
    /// and damage-driven frames (`tick_animations`) consume it without ever
    /// delivering. A changed group set is therefore itself a reason to render
    /// — the compositor answers the push with `needs_render = true`, and the
    /// caller renders on the same pass.
    pub(super) fn sync_window_groups(&mut self, backend: &mut dyn Backend) -> bool {
        let groups = self.build_window_groups();
        if groups == self.pushed_window_groups {
            return false;
        }
        backend.compositor_set_window_groups(groups.clone());
        self.pushed_window_groups = groups;
        true
    }

    pub(super) fn render_pending_frame(&mut self, backend: &mut dyn Backend) {
        if !backend.has_compositor() {
            return;
        }
        // Skip if animations are active — tick_animations handles rendering
        // during animation frames, so we don't want to double-render.
        if self.animations.has_active() {
            return;
        }
        // Deliver tab groups before consulting the render gate, and let a
        // groups change open it: otherwise window add/remove frames rendered
        // from tick_animations leave the compositor painting a stale strip
        // until some unrelated event arms `needs_render` again.
        let groups_changed = self.sync_window_groups(backend);
        // When overview is active the prism rotation runs inside the render
        // pass (tick_overview_prism), but clear_needs_render() after
        // render_frame() wipes the flag it sets.  So we must keep rendering
        // every frame unconditionally while overview is up; vsync provides
        // natural ~60 fps pacing.
        if !groups_changed && !backend.compositor_needs_render() && !self.features.overview.active {
            return;
        }
        let scene = self.build_compositor_scene(backend, &HashMap::new());
        let focused = self
            .get_selected_client_key()
            .and_then(|ck| self.state.clients.get(ck))
            .map(|c| c.win.raw());
        let _ = backend.compositor_render_frame(&scene, focused);
    }

    pub(super) fn tick_animations(&mut self, backend: &mut dyn Backend) {
        let now = Instant::now();
        // --- Night Light: update color temperature once per minute ---
        if backend.has_compositor() {
            let should_update = match self.last_night_light_update {
                Some(last) => now.saturating_duration_since(last) >= Duration::from_secs(60),
                None => true,
            };
            if should_update {
                self.last_night_light_update = Some(now);
                let cfg = CONFIG.load();
                let behavior = cfg.behavior();
                // A user override outranks the schedule until it is toggled
                // back, so the control-center row does not lose to the clock.
                let temp = match self.night_light_override {
                    Some(true) => behavior.night_light_temp,
                    Some(false) => 0.0,
                    None if behavior.night_light => Self::compute_night_light_temp(
                        &behavior.night_light_start,
                        &behavior.night_light_end,
                        behavior.night_light_temp,
                        behavior.night_light_transition_mins,
                    ),
                    None => 0.0,
                };
                backend.compositor_set_color_temperature(temp);
            }
        }

        // --- Battery: re-read on its own, slower interval. This is hardware
        // state rather than compositor state, so headless/non-composited
        // sessions consume the same deadline instead of leaving it at ZERO.
        if self.battery_next_wakeup(now).is_zero() {
            self.last_battery_poll = Some(now);
            self.poll_battery(backend);
            // Periodic re-read; skip while one is in flight so a hung nmcli
            // cannot pile up worker threads.
            if backend.has_compositor() && self.features.connectivity_poll.is_none() {
                self.refresh_connectivity();
            }
        }
        // Adopt whatever background connectivity read has finished, whether
        // the periodic one above or one kicked off by a toggle.
        self.poll_connectivity_job();

        // --- CPU / memory / network: a much faster interval, gated inside
        // the sampler because a rate divides by the gap actually observed.
        self.poll_resources();

        // Clipboard capture runs on its own thread and connection; adopt what
        // it copied here.
        for text in backend.drain_clipboard() {
            self.record_clipboard(&text);
        }

        // Application discovery traverses desktop-entry trees and every PATH
        // directory on a worker. Publish a completed immutable snapshot here;
        // an open launcher keeps its query and is redrawn by the single flush
        // at the end of this tick.
        self.poll_launcher_catalog_job();

        // The Shell Hub's slow controls are sampled off-thread. Always adopt
        // a completed value so a panel closed mid-read still warms the next
        // opening; only a visible Hub schedules periodic SWR refreshes.
        self.poll_control_snapshot_job();
        if self.features.system_ui.is_control_center() {
            self.ensure_control_snapshot_refresh(Instant::now());
        }

        // The Wi-Fi picker's scan and connect run on worker threads; adopt
        // their results here rather than blocking a frame on nmcli.
        self.poll_wifi_jobs(backend);
        self.poll_bluetooth_jobs(backend);

        // Wallpaper colour extraction decodes an image; the same applies.
        self.poll_wallpaper_theme(backend);

        // Dim, lock, or blank a session nobody is at.
        self.poll_idle(backend);

        // Everything above may have rebuilt an open panel. Push it once,
        // after the last of them, so one tick costs at most one redraw.
        self.flush_system_ui(backend);

        let composited = backend.has_compositor();

        if !self.animations.has_active() {
            if composited && backend.compositor_needs_render() {
                // No animations but compositor has dirty windows (damage, add/remove, resize)
                let scene = self.build_compositor_scene(backend, &HashMap::new());
                // Damage frames are the ones window add/remove actually
                // produce, so they must carry tab groups too — otherwise the
                // strip keeps the previous set until render_pending_frame
                // happens to run with the gate open.
                self.sync_window_groups(backend);
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
            // A layout animation can itself change the tab groups (window
            // opened/closed under an animated arrange); deliver before the
            // frame so the strip tracks the layout being animated.
            self.sync_window_groups(backend);
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

        // Include override-redirect windows (menus, launchers, tooltips) on top.
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
            let scheme = super::window_state::client_decoration_scheme(
                self.get_selected_client_key() == Some(client_key),
                client.state.is_urgent,
                CONFIG.load().behavior().attention_animation,
            );
            let border_color = backend.color_allocator().get_border_pixel_of(scheme)?;
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
