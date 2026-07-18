// Config rule lookups, VRR, monitor, blur quality
#[allow(unused_imports)]
use super::math::ortho;
#[allow(unused_imports)]
use super::*;
#[allow(unused_imports)]
use glow::HasContext;
#[allow(unused_imports)]
use std::collections::HashMap;
#[allow(unused_imports)]
use std::ffi::CString;
#[allow(unused_imports)]
use std::sync::Arc;
#[allow(unused_imports)]
use std::sync::mpsc;

fn blur_cache_matches(
    valid: bool,
    cached_hash: u64,
    below_hash: u64,
    cached_levels: usize,
    blur_levels: usize,
    backdrop_dirty: bool,
) -> bool {
    valid && !backdrop_dirty && cached_hash == below_hash && cached_levels == blur_levels
}

fn temporal_cache_matches(
    valid: bool,
    cached_hash: u64,
    below_hash: u64,
    cached_levels: usize,
    blur_levels: usize,
) -> bool {
    valid && cached_hash == below_hash && cached_levels == blur_levels
}

impl<C: CompositorConnection> Compositor<C> {
    /// Look up per-window opacity from opacity_rules.
    pub(super) fn lookup_opacity_rule(&self, class_name: &str) -> Option<f32> {
        opacity_rule_for_class(&self.opacity_rules, class_name)
    }

    /// Look up per-window corner radius (feature 3).
    pub(super) fn lookup_corner_radius_rule(&self, class_name: &str) -> Option<f32> {
        corner_radius_rule_for_class(&self.corner_radius_rules, class_name)
    }

    /// Look up whether a window should have frosted glass effect.
    pub(super) fn lookup_frosted_glass_rule(&self, class_name: &str) -> bool {
        if class_name.is_empty() {
            return false;
        }
        self.frosted_glass_rules
            .iter()
            .any(|r| r.eq_ignore_ascii_case(class_name))
    }

    /// Detect if window is a game (for VRR)
    pub(super) fn detect_game_window(&self, class_name: &str) -> bool {
        if class_name.is_empty() {
            return false;
        }
        let cfg = crate::config::CONFIG.load();
        let behavior = cfg.behavior();

        // Check user's game_classes list (substring match).
        for game_class in &behavior.game_classes {
            if contains_ignore_case(class_name, game_class) {
                return true;
            }
        }

        // Built-in game/emulator detection (exact match).
        const BUILTIN_GAME_CLASSES: &[&str] = &[
            "steam",
            "steamapps",
            "proton",
            "dxvk",
            "lutris",
            "wine",
            "minecraft",
            "dosbox",
            "mgba",
            "pcsx2",
            "yuzu",
            "dolphin",
        ];
        BUILTIN_GAME_CLASSES
            .iter()
            .any(|g| class_name.eq_ignore_ascii_case(g))
    }

    /// Check if currently focused window is a game
    pub(crate) fn is_focused_window_game(&self, focused_win: Option<u32>) -> bool {
        match focused_win {
            Some(win) => self.is_game_window.get(&win).copied().unwrap_or(false),
            None => false,
        }
    }

    /// Update VRR state based on focused window type
    pub(crate) fn update_vrr_state(&mut self, focused_win: Option<u32>) {
        let cfg = crate::config::CONFIG.load();
        let behavior = cfg.behavior();

        if !behavior.vrr_enabled {
            self.vrr_active = false;
            return;
        }

        // Limit updates to once per second to avoid flapping
        if self.vrr_last_check.elapsed().as_secs() < 1 {
            return;
        }
        self.vrr_last_check = std::time::Instant::now();

        // Enable VRR for game windows, disable for desktop
        let should_vrr = self.is_focused_window_game(focused_win);
        if should_vrr != self.vrr_active {
            self.vrr_active = should_vrr;
            log::info!("VRR {}", if should_vrr { "enabled" } else { "disabled" });
        }
    }

    /// Get current VRR refresh rate target (Hz)
    pub(crate) fn get_vrr_refresh_rate(&self) -> u32 {
        if self.vrr_active {
            let cfg = crate::config::CONFIG.load();
            let behavior = cfg.behavior();
            behavior.vrr_max_fps
        } else {
            60 // Default refresh rate for non-game windows
        }
    }

    /// Record input event for latency tracking (Task 8)
    pub(crate) fn record_input_event(&mut self) {
        self.frame_stats.last_input_time = Some(std::time::Instant::now());
    }

    /// Compute and record input→display latency when frame is rendered
    pub(super) fn record_latency_sample(&mut self) {
        if let Some(input_time) = self.frame_stats.last_input_time {
            let now = std::time::Instant::now();
            let input_to_render_ms = now.duration_since(input_time).as_secs_f32() * 1000.0;

            // Estimate GPU→display latency using OML sync or vblank period
            let gpu_to_display_ms = if let Some(oml) = &self.oml {
                // If OML available, estimate remaining time to vblank
                if let Some((ust, _msc, _sbc)) = oml.get_sync_values() {
                    // Assume 60Hz default; in practice, would query RandR for actual refresh rate
                    let vblank_interval_ns = 16_666_667u64; // 60Hz = 16.67ms
                    let frame_age_ns = ust % vblank_interval_ns;
                    let time_to_next_vblank_ns = vblank_interval_ns - frame_age_ns;
                    (time_to_next_vblank_ns as f32 / 1_000_000.0) + 1.0 // +1ms buffer for display pipeline
                } else {
                    // Fallback: assume 1 frame time (~16.67ms at 60Hz)
                    16.67
                }
            } else {
                // Fallback without OML: assume 1-2 frames of pipeline latency
                33.33
            };

            let total_latency_ms = input_to_render_ms + gpu_to_display_ms;

            self.frame_stats.latency_samples.push_back(total_latency_ms);
            // Ring buffer: keep最多 300 samples (~5 seconds at 60fps).
            // VecDeque::pop_front is O(1); Vec::remove(0) was an O(N) memmove.
            if self.frame_stats.latency_samples.len() > 300 {
                self.frame_stats.latency_samples.pop_front();
            }

            // Diagnostic logging for high latency
            if total_latency_ms > 100.0 {
                log::warn!(
                    "compositor: high input latency detected: {:.1}ms (input→render: {:.1}ms, gpu→display: {:.1}ms)",
                    total_latency_ms,
                    input_to_render_ms,
                    gpu_to_display_ms
                );
            } else if total_latency_ms > 50.0 {
                log::debug!(
                    "compositor: elevated input latency: {:.1}ms (input→render: {:.1}ms, gpu→display: {:.1}ms)",
                    total_latency_ms,
                    input_to_render_ms,
                    gpu_to_display_ms
                );
            }

            // Clear the input timestamp after recording
            self.frame_stats.last_input_time = None;
        }
    }

    /// Compute latency statistics (p50, p95, p99)
    pub(super) fn compute_latency_stats(&self) -> (f32, f32, f32, f32) {
        latency_stats(self.frame_stats.latency_samples.iter().copied())
    }

    /// P5B: Build monitor rectangles from RandR outputs
    pub(super) fn build_monitor_rects(conn: &Arc<C>, root: u32) -> Vec<(u32, i32, i32, u32, u32)> {
        conn.query_monitor_rects(root)
    }

    /// P5B Phase 2: Build monitor refresh rates from RandR outputs
    pub(super) fn build_monitor_refresh_rates(conn: &Arc<C>, root: u32) -> HashMap<u32, u32> {
        conn.query_monitor_refresh_rates(root)
    }

    /// P5B Phase 1: Map window position to monitor index using real RandR geometry.
    /// Picks the monitor with the largest rectangular overlap area; falls back
    /// to center-point containment, then to the first monitor (primary).
    pub(super) fn get_window_monitor_id(
        &self,
        window_x: i32,
        window_y: i32,
        window_w: u32,
        window_h: u32,
    ) -> u32 {
        if let Some(id) =
            monitor_id_by_overlap(&self.monitor_rects, window_x, window_y, window_w, window_h)
        {
            return id;
        }
        self.monitor_rects.first().map(|r| r.0).unwrap_or(0)
    }

    /// P5B Phase 2: Get refresh rate for a specific monitor
    pub(super) fn get_monitor_refresh_hz(&self, monitor_id: u32) -> u32 {
        self.monitor_refresh_rates
            .get(&monitor_id)
            .copied()
            .unwrap_or(60) // Fallback to 60Hz if not found
    }

    /// Rebuild monitor geometry + refresh-rate maps from RandR after a layout
    /// change (hotplug / mode change). Both maps were previously only built once
    /// at init, so per-window blur quality and per-monitor refresh lookups went
    /// stale when displays were added/removed or modes changed.
    ///
    /// Map-only (no GL): the global Hz->blur_strength FBO rebuild is left to the
    /// config-apply path, which runs with a current GL context — recreating FBOs
    /// here (event-dispatch context) cannot assume the GL context is bound.
    pub(crate) fn refresh_monitor_layout(&mut self, root: u32) {
        self.monitor_rects = Self::build_monitor_rects(&self.conn, root);
        self.monitor_refresh_rates = Self::build_monitor_refresh_rates(&self.conn, root);
        log::info!(
            "compositor: monitor layout refreshed: {} monitors",
            self.monitor_rects.len()
        );
    }

    /// Compute blur quality for a specific window (Task 10: Adaptive Blur + Per-Monitor)
    pub(super) fn compute_window_blur_quality(
        &self,
        wt: &WindowTexture,
        focused: Option<u32>,
    ) -> BlurQuality {
        let cfg = crate::config::CONFIG.load();
        let behavior = cfg.behavior();

        // If adaptive blur is disabled, use global quality
        if !behavior.blur_quality_auto {
            return self.blur_quality;
        }

        // Global quality acts as an upper bound (e.g. Minimal under heavy load)
        let max_quality = self.blur_quality;

        // Check if this window is focused
        let is_focused = focused.map_or(false, |f| f == wt.x11_win);

        // Check if window is visible on screen (simple heuristic)
        let is_onscreen = wt.x + (wt.w as i32) > 0
            && wt.y + (wt.h as i32) > 0
            && wt.x < self.screen_w as i32
            && wt.y < self.screen_h as i32;

        // Status bar should not have adaptive blur quality changes
        let status_bar_name = cfg.status_bar_name();
        let is_statusbar =
            wt.class_name == status_bar_name || wt.class_name.contains(status_bar_name);
        if is_statusbar {
            return self.blur_quality;
        }

        // P5B: Apply per-monitor quality override using real RandR geometry
        let monitor_id = self.get_window_monitor_id(wt.x, wt.y, wt.w, wt.h);
        let monitor_override = self.blur_quality_by_monitor.get(&monitor_id);

        if let Some(&override_quality) = monitor_override {
            // Per-monitor config takes precedence
            return override_quality.min(max_quality);
        }

        // Estimate GPU load from recent frame times (naive approach)
        // Assume 60Hz = 16.67ms ideal frame time; if actual is higher, GPU is under pressure
        let current_gpu_load = {
            let target_frame_time_ms = 1000.0 / 60.0; // 60Hz baseline
            if self.frame_stats.frame_times.is_empty() {
                0 // No data yet
            } else {
                let avg_frame_time_ms = self.frame_stats.frame_times.iter().sum::<f32>()
                    / self.frame_stats.frame_times.len() as f32;
                let load = (avg_frame_time_ms / target_frame_time_ms * 100.0) as u32;
                load.min(100)
            }
        };

        // Apply hysteresis: only update if delta > 5% or elapsed > 0.5s
        // This prevents rapid quality oscillation when load hovers around thresholds
        let gpu_load = if current_gpu_load > self.last_gpu_load + 5
            || current_gpu_load + 5 < self.last_gpu_load
            || self.last_gpu_load_update.elapsed().as_millis() > 500
        {
            // Update the cached load
            // Note: We can't mutate self here, so we use current_gpu_load
            // The actual update happens in the blur rendering pass
            current_gpu_load
        } else {
            // Use previous value for stability
            self.last_gpu_load
        };

        // Under high GPU load (>80%), only focused window keeps full quality
        // Unfocused/off-screen windows degrade to minimal to reduce GPU pressure
        let per_window_quality = if gpu_load > 80 {
            // Critical load: protect focused window, minimize others
            if is_focused {
                BlurQuality::Full // Focused: maintain full quality
            } else {
                BlurQuality::Minimal // Unfocused/off-screen: minimal
            }
        } else if gpu_load > 70 {
            // Moderate load: reduce unfocused windows only
            if is_focused {
                BlurQuality::Full // Focused: full quality
            } else if !is_onscreen {
                BlurQuality::Minimal // Off-screen: minimal
            } else {
                BlurQuality::Reduced // Inactive but visible: reduced
            }
        } else {
            // Low load: normal priority-based tiering
            if is_focused {
                BlurQuality::Full
            } else if !is_onscreen {
                BlurQuality::Minimal
            } else {
                BlurQuality::Reduced
            }
        };

        // Apply global cap (animation/overview can further reduce)
        match max_quality {
            BlurQuality::Minimal => BlurQuality::Minimal,
            BlurQuality::Reduced => match per_window_quality {
                BlurQuality::Full => BlurQuality::Reduced,
                other => other,
            },
            BlurQuality::Full => per_window_quality,
        }
    }

    /// Parse blur_strength_by_hz config string: "60:2,75:2.5,144:3.5" -> [(60, 2), (75, 2), (144, 3)]
    /// Get blur strength for a given refresh rate Hz.
    /// If exact Hz not found, returns closest lower, or if none, closest higher.
    pub(super) fn get_blur_strength_for_hz(&self, hz: u32) -> Option<u32> {
        blur_strength_for_hz(&self.blur_strength_by_hz, hz)
    }

    /// Make sure every visible backdrop consumer owns an independent cached
    /// blur result. The temporal scratch target can be shared because mix
    /// passes are serialized.
    pub(super) fn ensure_window_blur_caches(&mut self, blur_windows: &[u32]) {
        if !self.blur_enabled || self.blur_fbos.is_empty() {
            self.clear_window_blur_caches();
            return;
        }

        let cache_w = self.blur_fbos[0].w;
        let cache_h = self.blur_fbos[0].h;

        let stale: Vec<u32> = self
            .window_blur_caches
            .keys()
            .copied()
            .filter(|win| !blur_windows.contains(win))
            .collect();
        unsafe {
            for win in stale {
                if let Some(cache) = self.window_blur_caches.remove(&win) {
                    self.gl.delete_framebuffer(cache.fbo);
                    self.gl.delete_texture(cache.texture);
                }
            }
        }

        for &win in blur_windows {
            if self.window_blur_caches.contains_key(&win) {
                continue;
            }
            if let Ok((fbo, texture)) =
                unsafe { Self::create_scene_fbo(&self.gl, cache_w, cache_h) }
            {
                self.window_blur_caches.insert(
                    win,
                    WindowBlurCache {
                        fbo,
                        texture,
                        below_hash: std::cell::Cell::new(0),
                        blur_levels: std::cell::Cell::new(0),
                        valid: std::cell::Cell::new(false),
                    },
                );
            }
        }

        if self.temporal_blur_enabled && !blur_windows.is_empty() {
            if self.temporal_blur_fbo.is_none() {
                self.temporal_blur_fbo =
                    unsafe { Self::create_scene_fbo(&self.gl, cache_w, cache_h).ok() };
            }
        } else {
            unsafe {
                if let Some((fbo, texture)) = self.temporal_blur_fbo.take() {
                    self.gl.delete_framebuffer(fbo);
                    self.gl.delete_texture(texture);
                }
            }
        }
    }

    pub(super) fn invalidate_window_blur_caches(&self) {
        for cache in self.window_blur_caches.values() {
            cache.valid.set(false);
        }
    }

    pub(super) fn clear_window_blur_caches(&mut self) {
        unsafe {
            for (_, cache) in self.window_blur_caches.drain() {
                self.gl.delete_framebuffer(cache.fbo);
                self.gl.delete_texture(cache.texture);
            }
            if let Some((fbo, texture)) = self.temporal_blur_fbo.take() {
                self.gl.delete_framebuffer(fbo);
                self.gl.delete_texture(texture);
            }
        }
    }

    pub(super) fn window_blur_cache_hit(
        &self,
        win: u32,
        below_hash: u64,
        blur_levels: usize,
        backdrop_dirty: bool,
    ) -> bool {
        self.window_blur_caches.get(&win).is_some_and(|cache| {
            blur_cache_matches(
                cache.valid.get(),
                cache.below_hash.get(),
                below_hash,
                cache.blur_levels.get(),
                blur_levels,
                backdrop_dirty,
            )
        })
    }

    pub(super) fn window_blur_cache_texture(&self, win: u32) -> Option<glow::Texture> {
        self.window_blur_caches.get(&win).map(|cache| cache.texture)
    }

    /// Store the freshly filtered result in this consumer's cache, optionally
    /// mixing it with that same consumer's temporal history. The shared scratch
    /// target keeps the history texture read-only for the whole shader pass.
    ///
    /// This method intentionally takes `&self` because it runs while the
    /// window loop holds an immutable borrow of a `WindowTexture`.
    pub(super) fn update_window_blur_cache(
        &self,
        win: u32,
        current_blur_tex: glow::Texture,
        below_hash: u64,
        blur_levels: usize,
    ) -> (glow::Texture, bool) {
        // Leave a deterministic raw draw state for the tracked restore in
        // render_frame, including all early-return paths below.
        unsafe {
            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
        }

        let cache = match self.window_blur_caches.get(&win) {
            Some(cache) => cache,
            None => return (current_blur_tex, false),
        };
        let current_level = match self
            .blur_fbos
            .iter()
            .find(|level| level.texture == current_blur_tex)
        {
            Some(level) => level,
            None => return (current_blur_tex, false),
        };
        let reuse_previous = self.temporal_blur_enabled
            && temporal_cache_matches(
                cache.valid.get(),
                cache.below_hash.get(),
                below_hash,
                cache.blur_levels.get(),
                blur_levels,
            )
            && self.temporal_blur_fbo.is_some();

        unsafe {
            let blend_enabled = self.gl.is_enabled(glow::BLEND);
            let scissor_enabled = self.gl.is_enabled(glow::SCISSOR_TEST);
            if blend_enabled {
                self.gl.disable(glow::BLEND);
            }
            if scissor_enabled {
                self.gl.disable(glow::SCISSOR_TEST);
            }

            if !reuse_previous {
                self.gl
                    .bind_framebuffer(glow::READ_FRAMEBUFFER, Some(current_level.fbo));
                self.gl
                    .bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(cache.fbo));
                self.gl.blit_framebuffer(
                    0,
                    0,
                    current_level.w as i32,
                    current_level.h as i32,
                    0,
                    0,
                    current_level.w as i32,
                    current_level.h as i32,
                    glow::COLOR_BUFFER_BIT,
                    glow::NEAREST,
                );
                self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                if blend_enabled {
                    self.gl.enable(glow::BLEND);
                }
                if scissor_enabled {
                    self.gl.enable(glow::SCISSOR_TEST);
                }
                cache.below_hash.set(below_hash);
                cache.blur_levels.set(blur_levels);
                cache.valid.set(true);
                return (cache.texture, false);
            }

            let (mix_fbo, _) = self.temporal_blur_fbo.unwrap();
            let proj = math::ortho(
                0.0,
                current_level.w as f32,
                current_level.h as f32,
                0.0,
                -1.0,
                1.0,
            );

            self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(mix_fbo));
            self.gl.use_program(Some(self.temporal_blur_mix_program));
            self.gl
                .viewport(0, 0, current_level.w as i32, current_level.h as i32);

            // Bind current blur as input
            self.gl.active_texture(glow::TEXTURE0);
            self.gl
                .bind_texture(glow::TEXTURE_2D, Some(current_blur_tex));
            self.gl.uniform_1_i32(
                self.gl
                    .get_uniform_location(self.temporal_blur_mix_program, "u_current_blur")
                    .as_ref(),
                0,
            );

            // Bind previous blur as input
            self.gl.active_texture(glow::TEXTURE1);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(cache.texture));
            self.gl.uniform_1_i32(
                self.gl
                    .get_uniform_location(self.temporal_blur_mix_program, "u_previous_blur")
                    .as_ref(),
                1,
            );

            // Set temporal mix ratio
            self.gl.uniform_1_f32(
                self.gl
                    .get_uniform_location(self.temporal_blur_mix_program, "u_temporal_mix")
                    .as_ref(),
                self.temporal_blur_mix_ratio,
            );

            // Set projection and screen rect
            self.gl.uniform_matrix_4_f32_slice(
                self.temporal_blur_mix_uniforms.projection.as_ref(),
                false,
                &proj,
            );
            self.gl.uniform_4_f32(
                self.temporal_blur_mix_uniforms.rect.as_ref(),
                0.0,
                0.0,
                current_level.w as f32,
                current_level.h as f32,
            );

            // Draw mix quad
            self.gl.bind_vertex_array(Some(self.quad_vao));
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);

            self.gl.active_texture(glow::TEXTURE1);
            self.gl.bind_texture(glow::TEXTURE_2D, None);
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_2D, None);

            // Promote the completed scratch result to history only after the
            // sampling pass has finished.
            self.gl
                .bind_framebuffer(glow::READ_FRAMEBUFFER, Some(mix_fbo));
            self.gl
                .bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(cache.fbo));
            self.gl.blit_framebuffer(
                0,
                0,
                current_level.w as i32,
                current_level.h as i32,
                0,
                0,
                current_level.w as i32,
                current_level.h as i32,
                glow::COLOR_BUFFER_BIT,
                glow::NEAREST,
            );
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);

            if blend_enabled {
                self.gl.enable(glow::BLEND);
            }
            if scissor_enabled {
                self.gl.enable(glow::SCISSOR_TEST);
            }
        }

        cache.below_hash.set(below_hash);
        cache.blur_levels.set(blur_levels);
        cache.valid.set(true);
        (cache.texture, true)
    }

    /// Whether a window should receive per-frame backdrop blur compositing.
    pub(super) fn needs_backdrop_blur(&self, wt: &WindowTexture, status_bar_name: &str) -> bool {
        // Skip backdrop blur for statusbar
        if wt.class_name == status_bar_name || wt.class_name.contains(status_bar_name) {
            return false;
        }
        if class_matches_exclude(&wt.class_name, &self.blur_exclude) {
            return false;
        }
        // Skip backdrop blur for large override-redirect RGBA windows.  These
        // are typically screen-sharing overlays (e.g. Feishu/Lark) or screenshot
        // selection tools that are intentionally transparent.  Applying blur
        // behind them produces an unwanted frosted-glass effect that covers the
        // actual screen content.
        //
        // "Large" = covers at least 80 % of any single monitor in both dimensions.
        if wt.is_override_redirect && wt.has_rgba {
            let dominated = self
                .monitor_wallpapers
                .iter()
                .any(|mw| wt.w >= mw.mon_w * 4 / 5 && wt.h >= mw.mon_h * 4 / 5);
            if dominated {
                return false;
            }
        }
        let explicit_translucency =
            wt.fade_opacity < 1.0 || wt.opacity_override.map_or(false, |o| o < 1.0);

        wt.is_frosted || explicit_translucency || wt.has_rgba
    }

    /// Look up per-window scale (feature 4).
    pub(super) fn lookup_scale_rule(&self, class_name: &str) -> Option<f32> {
        scale_rule_for_class(&self.scale_rules, class_name)
    }
}

#[cfg(test)]
mod tests {
    use super::{blur_cache_matches, temporal_cache_matches};

    #[test]
    fn empty_below_scene_is_cacheable() {
        assert!(blur_cache_matches(true, 0, 0, 3, 3, false));
    }

    #[test]
    fn dirty_or_different_below_scene_misses_cache() {
        assert!(!blur_cache_matches(true, 7, 7, 3, 3, true));
        assert!(!blur_cache_matches(true, 7, 8, 3, 3, false));
        assert!(!blur_cache_matches(true, 7, 7, 2, 3, false));
        assert!(!blur_cache_matches(false, 7, 7, 3, 3, false));
    }

    #[test]
    fn temporal_reuse_depends_only_on_this_consumers_below_scene() {
        assert!(temporal_cache_matches(true, 41, 41, 3, 3));
        assert!(!temporal_cache_matches(true, 41, 42, 3, 3));
        assert!(!temporal_cache_matches(true, 41, 41, 2, 3));
        assert!(!temporal_cache_matches(false, 41, 41, 3, 3));
    }
}
