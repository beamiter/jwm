// render_frame and rendering helpers
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

impl<C: CompositorConnection> Compositor<C> {
    // =====================================================================
    // Tag-switch slide transition
    // =====================================================================

    /// Called just before a tag switch. Captures the current back-buffer into
    /// a snapshot texture so `render_frame` can slide the old scene out.
    /// `mon_rect` is (x, y, w, h) of the monitor where the switch happens.
    pub(crate) fn notify_tag_switch(
        &mut self,
        duration: std::time::Duration,
        direction: i32,
        exclude_top: u32,
        mon_rect: (i32, i32, u32, u32),
    ) {
        // Ensure GL context is current
        if !self.context_current {
            unsafe {
                x11::glx::glXMakeContextCurrent(
                    self.xlib_display,
                    self.glx_drawable,
                    self.glx_drawable,
                    self.glx_context,
                );
            }
            self.context_current = true;
        }

        let (mon_x, mon_y, mon_w, mon_h) = mon_rect;
        let mon_w = mon_w.max(1);
        let mon_h = mon_h.max(1);

        // Recreate FBOs if monitor size changed
        let size_changed = self.transition_fbo.as_ref().map_or(true, |_| {
            self.transition_mon_w != mon_w || self.transition_mon_h != mon_h
        });
        if size_changed {
            if let Some((fbo, tex)) = self.transition_fbo.take() {
                unsafe {
                    self.gl.delete_framebuffer(fbo);
                    self.gl.delete_texture(tex);
                }
            }
            if let Some((fbo, tex)) = self.transition_new_fbo.take() {
                unsafe {
                    self.gl.delete_framebuffer(fbo);
                    self.gl.delete_texture(tex);
                }
            }
        }

        // Create snapshot FBO at monitor size
        if self.transition_fbo.is_none() {
            self.transition_fbo = unsafe { Self::create_scene_fbo(&self.gl, mon_w, mon_h).ok() };
        }

        // Create new-scene FBO for modes that need both old and new textures
        let needs_new_fbo = self.transition_mode.needs_new_scene_fbo();
        if needs_new_fbo && self.transition_new_fbo.is_none() {
            self.transition_new_fbo =
                unsafe { Self::create_scene_fbo(&self.gl, mon_w, mon_h).ok() };
        }

        // Store monitor rect for rendering
        self.transition_mon_x = mon_x;
        self.transition_mon_y = mon_y;
        self.transition_mon_w = mon_w;
        self.transition_mon_h = mon_h;

        if let Some((snap_fbo, _)) = &self.transition_fbo {
            self.transition_exclude_top = exclude_top.min(mon_h.saturating_sub(1));
            self.capture_transition_scene(*snap_fbo, mon_x, mon_y, mon_w, mon_h);
            self.transition_start = Some(std::time::Instant::now());
            self.transition_duration = duration;
            self.transition_direction = if direction >= 0 { 1.0 } else { -1.0 };
            // Tag switch can radically change visible scene; force a full redraw
            // to avoid stale pixels from partial-damage scissor regions.
            self.damage_tracker.mark_all_dirty();
            self.dirty_region_tracker.mark_all_dirty(); // P5C: Sync rect tracker
            self.needs_render = true;
            log::debug!(
                "compositor: tag-switch slide transition started ({:?}, dir={}, mon={}x{}+{}+{})",
                duration,
                direction,
                mon_w,
                mon_h,
                mon_x,
                mon_y,
            );
        }
    }

    pub(super) fn capture_transition_scene(
        &self,
        dst_fbo: glow::Framebuffer,
        mon_x: i32,
        mon_y: i32,
        mon_w: u32,
        mon_h: u32,
    ) {
        let exclude_top = self.transition_exclude_top.min(mon_h);
        let workspace_h = mon_h.saturating_sub(exclude_top);
        let gl_y = self.screen_h as i32 - (mon_y + mon_h as i32);

        unsafe {
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(dst_fbo));
            self.gl.viewport(0, 0, mon_w as i32, mon_h as i32);
            self.gl.clear_color(0.0, 0.0, 0.0, 0.0);
            self.gl.clear(glow::COLOR_BUFFER_BIT);
            self.gl.clear_color(0.0, 0.0, 0.0, 1.0);

            if workspace_h > 0 {
                self.gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
                self.gl
                    .bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(dst_fbo));
                self.gl.blit_framebuffer(
                    mon_x,
                    gl_y,
                    mon_x + mon_w as i32,
                    gl_y + workspace_h as i32,
                    0,
                    0,
                    mon_w as i32,
                    workspace_h as i32,
                    glow::COLOR_BUFFER_BIT,
                    glow::NEAREST,
                );
            }

            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.gl
                .viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
        }
    }

    pub(crate) fn force_full_redraw(&mut self) {
        self.damage_tracker.mark_all_dirty();
        self.needs_render = true;
    }

    pub(crate) fn ensure_scene_windows_tracked(
        &mut self,
        scene: &[(u32, i32, i32, u32, u32)],
        root: u32,
        original_scene_len: usize,
        backend_label: &str,
    ) {
        if original_scene_len != 0 && scene.is_empty() {
            log::warn!(
                "[{backend_label} compositor] scene has {original_scene_len} entries but x11_scene is empty (ID lookup failed)"
            );
        }

        for &(x11w, x, y, w, h) in scene {
            if !self.has_window(x11w) && x11w != root {
                log::info!(
                    "[{backend_label} compositor] lazily adding untracked window 0x{:x} {}x{} at ({},{})",
                    x11w,
                    w,
                    h,
                    x,
                    y
                );
                self.add_window(x11w, x, y, w, h);
            }
        }
    }

    // =====================================================================
    // Feature 11: Debug HUD toggle
    // =====================================================================
    pub(crate) fn set_transition_mode(&mut self, mode: &str) {
        self.transition_mode = TransitionMode::from_name(mode);
    }

    pub(crate) fn set_debug_hud(&mut self, enabled: bool) {
        self.debug_hud = enabled;
        self.needs_render = true;
    }

    pub(crate) fn set_debug_hud_extended(&mut self, enabled: bool) {
        self.debug_hud_extended = enabled;
        self.frame_profiler.set_enabled(enabled);
        self.needs_render = true;
    }

    #[allow(dead_code)]
    pub(crate) fn debug_hud_enabled(&self) -> bool {
        self.debug_hud
    }

    pub(crate) fn frame_stats_fps(&self) -> f32 {
        self.frame_stats.fps
    }

    pub(crate) fn get_metrics(&self) -> crate::backend::api::CompositorMetrics {
        let frame_times_vec: Vec<f32> = self.frame_stats.frame_times.iter().copied().collect();
        let avg_frame_time = if frame_times_vec.is_empty() {
            0.0
        } else {
            frame_times_vec.iter().sum::<f32>() / frame_times_vec.len() as f32
        };
        let max_frame_time = frame_times_vec.iter().copied().fold(0.0, f32::max);
        let min_frame_time = frame_times_vec.iter().copied().fold(f32::MAX, f32::min);
        let min_frame_time = if min_frame_time == f32::MAX {
            0.0
        } else {
            min_frame_time
        };

        let blur_hit_rate =
            if self.frame_stats.blur_cache_hits + self.frame_stats.blur_cache_misses > 0 {
                100.0 * self.frame_stats.blur_cache_hits as f32
                    / (self.frame_stats.blur_cache_hits + self.frame_stats.blur_cache_misses) as f32
            } else {
                0.0
            };

        let temporal_blur_reuse_rate = if self.temporal_blur_total_count > 0 {
            100.0 * self.temporal_blur_reuse_count as f32 / self.temporal_blur_total_count as f32
        } else {
            0.0
        };

        let dirty_tiles_count = self.damage_tracker.dirty_tile_count();
        let dirty_fraction = self.damage_tracker.dirty_fraction();

        let latency_stats = self.compute_latency_stats();

        crate::backend::api::CompositorMetrics {
            fps: self.frame_stats.fps,
            frame_count: self.frame_stats.frame_count,
            avg_frame_time_ms: avg_frame_time,
            max_frame_time_ms: max_frame_time,
            min_frame_time_ms: min_frame_time,
            frame_time_p95_ms: 0.0,
            frame_time_p99_ms: 0.0,
            gpu_load_percent: 0, // To be updated from perf_metrics
            cpu_load_percent: 0, // To be updated from perf_metrics
            draw_calls: self.frame_stats.draw_calls,
            texture_memory_bytes: self.frame_stats.texture_memory_bytes,
            blur_cache_hits: self.frame_stats.blur_cache_hits,
            blur_cache_misses: self.frame_stats.blur_cache_misses,
            blur_cache_hit_rate: blur_hit_rate,
            temporal_blur_reuse_count: self.temporal_blur_reuse_count,
            temporal_blur_total_count: self.temporal_blur_total_count,
            temporal_blur_reuse_rate,
            dirty_regions_count: dirty_tiles_count,
            dirty_fraction_percent: dirty_fraction * 100.0,
            window_count: self.windows.len(),
            blur_quality: format!("{:?}", self.blur_quality),
            vrr_enabled: self.vrr_active,
            vrr_active: self.vrr_active,
            current_refresh_rate: self.get_vrr_refresh_rate(),
            input_latency_avg_ms: latency_stats.0,
            input_latency_p50_ms: latency_stats.1,
            input_latency_p95_ms: latency_stats.2,
            input_latency_p99_ms: latency_stats.3,
            // Phase 2-3: Optimization statistics
            direct_scanout_active: self.direct_scanout_mgr.is_active(),
            direct_scanout_count: self.direct_scanout_mgr.stats().scanout_count,
            direct_scanout_bypass_time_ms: self.direct_scanout_mgr.stats().total_bypass_time_ms,
            gl_state_changes_avoided: self.gl_state_tracker.redundant_changes_avoided(),
            profiling_enabled: self.frame_profiler.is_enabled(),
            dirty_region_merge_count: self.dirty_region_tracker.region_count(),
        }
    }

    /// Rasterize HUD text and upload as a GL texture. Skips upload when the
    /// formatted string is identical to the previous frame.
    pub(super) fn update_hud_text_texture(&mut self, text: &str) {
        if text == self.hud_text_cache && self.hud_text_texture.is_some() {
            return;
        }

        let scale = 2u32;
        let fg = [0, 230, 64, 255]; // green
        let (pixels, w, h) = font::render_text_to_rgba(text, scale, fg);
        if w == 0 || h == 0 {
            return;
        }

        unsafe {
            if let Some(old) = self.hud_text_texture.take() {
                self.gl.delete_texture(old);
            }
            if let Ok(tex) = self.gl.create_texture() {
                self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                self.gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA8 as i32,
                    w as i32,
                    h as i32,
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(Some(&pixels)),
                );
                self.gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_MIN_FILTER,
                    glow::NEAREST as i32,
                );
                self.gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_MAG_FILTER,
                    glow::NEAREST as i32,
                );
                self.gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_WRAP_S,
                    glow::CLAMP_TO_EDGE as i32,
                );
                self.gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_WRAP_T,
                    glow::CLAMP_TO_EDGE as i32,
                );
                self.gl.bind_texture(glow::TEXTURE_2D, None);
                self.hud_text_texture = Some(tex);
                self.hud_text_width = w;
                self.hud_text_height = h;
            }
        }

        self.hud_text_cache = text.to_string();
    }

    fn update_system_ui_text_texture(&mut self, text: &str) {
        let config = crate::config::CONFIG.load();
        let description = config.dmenu_font();
        let size = crate::backend::compositor_font::ui_font_pixel_size(description);
        let cache_key = format!("{description}\0{size}\0{text}");
        if cache_key == self.hud_text_cache && self.hud_text_texture.is_some() {
            return;
        }
        let (pixels, w, h) = crate::backend::compositor_font::render_ui_text_to_rgba(
            text,
            description,
            size,
            [235, 240, 255, 255],
        );
        if w == 0 || h == 0 {
            return;
        }
        unsafe {
            if let Some(old) = self.hud_text_texture.take() {
                self.gl.delete_texture(old);
            }
            if let Ok(tex) = self.gl.create_texture() {
                self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                self.gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA8 as i32,
                    w as i32,
                    h as i32,
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(Some(&pixels)),
                );
                for filter in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
                    self.gl
                        .tex_parameter_i32(glow::TEXTURE_2D, filter, glow::LINEAR as i32);
                }
                self.gl.bind_texture(glow::TEXTURE_2D, None);
                self.hud_text_texture = Some(tex);
                self.hud_text_width = w;
                self.hud_text_height = h;
            }
        }
        self.hud_text_cache = cache_key;
    }

    fn render_system_ui(&mut self, proj: &[f32; 16]) {
        let Some(overlay) = self.system_ui.clone() else {
            return;
        };
        self.update_system_ui_text_texture(&overlay.text);
        let pad = 30.0;
        let text_w = self.hud_text_width as f32;
        let text_h = self.hud_text_height as f32;
        let panel_w = (text_w + pad * 2.0).min(self.screen_w as f32 - 32.0);
        let panel_h = text_h + pad * 2.0;
        let x = (self.screen_w as f32 - panel_w) * 0.5;
        let y = (self.screen_h as f32 - panel_h) * 0.5;
        unsafe {
            if overlay.locked {
                self.gl.clear_color(0.018, 0.022, 0.035, 1.0);
                self.gl.clear(glow::COLOR_BUFFER_BIT);
            }
            self.gl.use_program(Some(self.hud_program));
            self.gl
                .uniform_matrix_4_f32_slice(self.hud_uniforms.projection.as_ref(), false, proj);
            self.gl.uniform_4_f32(
                self.hud_uniforms.bg_color.as_ref(),
                0.025,
                0.03,
                0.045,
                if overlay.locked { 1.0 } else { 0.94 },
            );
            self.gl
                .uniform_4_f32(self.hud_uniforms.fg_color.as_ref(), 0.4, 0.7, 1.0, 1.0);
            self.gl.uniform_2_f32(
                self.hud_uniforms.size.as_ref(),
                self.screen_w as f32,
                self.screen_h as f32,
            );
            self.gl.uniform_4_f32(
                self.hud_uniforms.rect.as_ref(),
                0.0,
                0.0,
                self.screen_w as f32,
                self.screen_h as f32,
            );
            self.gl.bind_vertex_array(Some(self.quad_vao));
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            self.gl
                .uniform_4_f32(self.hud_uniforms.bg_color.as_ref(), 0.08, 0.10, 0.15, 0.98);
            self.gl
                .uniform_2_f32(self.hud_uniforms.size.as_ref(), panel_w, panel_h);
            self.gl
                .uniform_4_f32(self.hud_uniforms.rect.as_ref(), x, y, panel_w, panel_h);
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            if let Some(tex) = self.hud_text_texture {
                self.gl.use_program(Some(self.hud_text_program));
                self.gl.uniform_matrix_4_f32_slice(
                    self.hud_text_uniforms.projection.as_ref(),
                    false,
                    proj,
                );
                self.gl.uniform_4_f32(
                    self.hud_text_uniforms.rect.as_ref(),
                    x + pad,
                    y + pad,
                    text_w,
                    text_h,
                );
                self.gl
                    .uniform_1_i32(self.hud_text_uniforms.texture.as_ref(), 0);
                self.gl.active_texture(glow::TEXTURE0);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            }
            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
        }
    }

    // =====================================================================
    // Feature 12: Screenshot
    // =====================================================================
    pub(crate) fn request_screenshot(&mut self, path: std::path::PathBuf) {
        self.screenshot_requests.request_full(path);
        self.needs_render = true;
    }

    pub(crate) fn request_screenshot_region(
        &mut self,
        path: std::path::PathBuf,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    ) {
        self.screenshot_requests.request_region(path, x, y, w, h);
        self.needs_render = true;
    }

    /// Check if there's a single fullscreen opaque window covering the screen.
    /// If so, and fullscreen_unredirect is enabled, we can skip compositing.
    pub(super) fn check_fullscreen_unredirect(
        &mut self,
        scene: &[(u32, i32, i32, u32, u32)],
        focused: Option<u32>,
    ) -> bool {
        // Realtime post-processing cannot run while the X server presents a
        // fullscreen client directly. Restore redirection as soon as a pose is
        // visible, including the first packet received during unredirect.
        if self.slime_state.is_visible() || self.system_ui.is_some() {
            if let Some(previous) = self.unredirected_window.take() {
                let _ = self.conn.redirect_window_manual(previous);
                let _ = self.conn.flush_x11();
                if let Some(wt) = self.windows.get_mut(&previous) {
                    wt.needs_pixmap_refresh = true;
                }
                self.needs_render = true;
                log::info!(
                    "compositor: re-redirected fullscreen window 0x{:x} for overlay",
                    previous
                );
            }
            return false;
        }
        if !self.fullscreen_unredirect {
            return false;
        }
        // Only unredirect if the top (focused) window is fullscreen and opaque
        if let Some(focused_win) = focused {
            if let Some(wt) = self.windows.get(&focused_win) {
                if wt.is_fullscreen && !wt.has_rgba {
                    // Check if it covers the full screen
                    if let Some(&(_, x, y, w, h)) =
                        scene.iter().rfind(|&&(win, _, _, _, _)| win == focused_win)
                    {
                        if x <= 0
                            && y <= 0
                            && (x + w as i32) >= self.screen_w as i32
                            && (y + h as i32) >= self.screen_h as i32
                        {
                            // Unredirect: the X server draws directly
                            if self.unredirected_window != Some(focused_win) {
                                let _ = self.conn.unredirect_window_manual(focused_win);
                                let _ = self.conn.flush_x11();
                                self.unredirected_window = Some(focused_win);
                                log::info!(
                                    "compositor: unredirected fullscreen window 0x{:x}",
                                    focused_win
                                );
                            }
                            return true;
                        }
                    }
                }
            }
        }
        // Re-redirect if we had an unredirected window that's no longer fullscreen
        if let Some(prev) = self.unredirected_window.take() {
            let _ = self.conn.redirect_window_manual(prev);
            let _ = self.conn.flush_x11();
            // The X server allocated a fresh backing pixmap while the window was
            // unredirected; the old NameWindowPixmap binding is now stale. Force
            // a rebind or the window renders frozen content until its next resize.
            if let Some(wt) = self.windows.get_mut(&prev) {
                wt.needs_pixmap_refresh = true;
            }
            log::info!("compositor: re-redirected window 0x{:x}", prev);
            self.needs_render = true;
        }
        false
    }

    // ----- Rendering -----

    /// Compute a simple hash of the scene + focused window for skip-unchanged detection.
    pub(super) fn scene_hash(scene: &[(u32, i32, i32, u32, u32)], focused: Option<u32>) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        scene.hash(&mut hasher);
        focused.hash(&mut hasher);
        hasher.finish()
    }

    /// Render a composited frame.
    ///
    /// `scene` is an ordered list of (x11_win, x, y, w, h) from bottom to top.
    /// `focused` is the X11 window ID of the focused window (if any).
    /// Returns true if a frame was rendered.
    pub(crate) fn render_frame(
        &mut self,
        scene: &[(u32, i32, i32, u32, u32)],
        focused: Option<u32>,
    ) -> bool {
        let bench_frame_start = std::time::Instant::now();

        // Auto-enable profiler when benchmark is running
        if self.benchmark.is_running() && !self.frame_profiler.is_enabled() {
            self.frame_profiler.set_enabled(true);
        }

        // Phase 2: Begin frame profiling
        self.frame_profiler.begin_frame();

        // Drain the lossy pose channel before deciding whether fullscreen can
        // bypass the compositor. Only the newest inference result is retained.
        let slime_updated = self.poll_slime_ipc();
        let slime_active = self.slime_state.is_visible();

        // P6A: Process deferred X11 operations at start of render frame
        self.process_deferred_x11_ops();

        // P4: Ensure temporal-blur prev FBO exists before the windows loop, so
        // the mix call inside the loop can run with &self while wt borrows are live.
        self.ensure_prev_blur_fbo();

        // P6B: Update GPU fence states (non-blocking check)
        unsafe {
            self.gpu_fence_sync_mgr.update_fence_states(&self.gl);
            self.gpu_fence_sync_mgr.cleanup_old_fences(&self.gl);
        }

        // Update GPU load cache with hysteresis: update if delta > 5% or elapsed > 0.5s
        let current_gpu_load = {
            let target_frame_time_ms = 1000.0 / 60.0;
            if self.frame_stats.frame_times.is_empty() {
                0
            } else {
                let avg_frame_time_ms = self.frame_stats.frame_times.iter().sum::<f32>()
                    / self.frame_stats.frame_times.len() as f32;
                let load = (avg_frame_time_ms / target_frame_time_ms * 100.0) as u32;
                load.min(100)
            }
        };

        if current_gpu_load > self.last_gpu_load + 5
            || current_gpu_load + 5 < self.last_gpu_load
            || self.last_gpu_load_update.elapsed().as_millis() > 500
        {
            self.last_gpu_load = current_gpu_load;
            self.last_gpu_load_update = std::time::Instant::now();
        }

        let periodic_60_frame = self.frame_stats.frame_count % 60 == 0;

        // Shader hot-reload: poll every 60 frames (~1s at 60fps)
        if self.shader_hot_reload_enabled && periodic_60_frame {
            self.poll_shader_hot_reload();
        }

        // VRR state update: check every 60 frames (~1s at 60fps)
        if periodic_60_frame {
            self.update_vrr_state(focused);
        }

        // P4: Temporal blur reuse detection
        let current_window_hash = if self.temporal_blur_enabled {
            self.compute_window_positions_hash()
        } else {
            0
        };

        // Track render diagnostics only when info logging is enabled; default
        // runs avoid the atomic counters and realtime-clock read entirely.
        if log::log_enabled!(log::Level::Info) {
            static RENDER_LOG_COUNT: std::sync::atomic::AtomicU32 =
                std::sync::atomic::AtomicU32::new(0);
            let count = RENDER_LOG_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if count < 5 || count % 500 == 0 {
                log::info!(
                    "[compositor::render_frame] frame={} scene={} tracked={}",
                    count,
                    scene.len(),
                    self.windows.len()
                );
            }

            static RENDER_FREQ_COUNT: std::sync::atomic::AtomicU32 =
                std::sync::atomic::AtomicU32::new(0);
            static RENDER_FREQ_EPOCH: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            let epoch = RENDER_FREQ_EPOCH.load(std::sync::atomic::Ordering::Relaxed);
            if epoch == 0 {
                RENDER_FREQ_EPOCH.store(now_ms, std::sync::atomic::Ordering::Relaxed);
            }
            let fc = RENDER_FREQ_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if now_ms - epoch >= 2000 {
                let elapsed = (now_ms - epoch) as f64 / 1000.0;
                log::info!(
                    "[compositor::render_freq] {:.1} renders/sec (needs_render={}, focused={:?})",
                    fc as f64 / elapsed,
                    self.needs_render,
                    focused,
                );
                RENDER_FREQ_COUNT.store(0, std::sync::atomic::Ordering::Relaxed);
                RENDER_FREQ_EPOCH.store(now_ms, std::sync::atomic::Ordering::Relaxed);
            }
        }

        // Phase 2.3: Direct scanout check - bypass compositor for eligible fullscreen windows
        // This provides -8-12ms latency reduction for fullscreen games/video
        {
            let mut scene_info = std::mem::take(&mut self.scratch_scene_info);
            scene_info.clear();
            scene_info.reserve(scene.len());
            scene_info.extend(scene.iter().filter_map(|&(win, x, y, w, h)| {
                self.windows.get(&win).map(|wt| {
                    let corner_radius = wt.corner_radius_override.unwrap_or(self.corner_radius);
                    (
                        win,
                        WindowScanoutInfo {
                            x,
                            y,
                            width: w,
                            height: h,
                            is_fullscreen: wt.is_fullscreen,
                            has_alpha: wt.has_rgba,
                            has_blur: wt.is_frosted || slime_active,
                            has_shadow: self.shadow_enabled,
                            has_corner_radius: corner_radius > 0.0,
                            opacity: wt.fade_opacity,
                        },
                    )
                })
            }));

            let (should_scanout, _scanout_win) =
                self.direct_scanout_mgr.check_scene(&scene_info, focused);
            self.scratch_scene_info = scene_info;
            if should_scanout {
                // Direct scanout active - bypass compositor rendering
                return false;
            }
        }

        // Fullscreen unredirect check
        if self.check_fullscreen_unredirect(scene, focused) {
            return false;
        }

        // Tick fade animations
        let fades_active = self.tick_fades();

        // Tick wobbly spring physics
        let wobbly_active = self.tick_wobbly();

        // Tick Phase 5 animations
        let expose_animating = self.tick_expose();
        let snap_animating = self.tick_snap_preview();
        let peek_animating = self.tick_peek();

        // Tick Phase 3 animations
        let genie_active = self.tick_genie();
        let ripples_active = self.tick_ripples();
        let focus_highlight_active = self.tick_focus_highlight();
        let wallpaper_crossfade_active = self.tick_wallpaper_crossfade();

        // Update damage tracker scene state for dynamic thresholds
        let any_animating = fades_active
            || wobbly_active
            || expose_animating
            || snap_animating
            || peek_animating
            || genie_active
            || ripples_active
            || focus_highlight_active
            || wallpaper_crossfade_active;
        self.damage_tracker
            .update_state(self.windows.len(), any_animating);

        // Phase 3.4: Detect focus change
        if self.focus_highlight {
            if let Some(fw) = focused {
                if self.last_focused_window != Some(fw) {
                    self.focus_highlight_start = Some((fw, std::time::Instant::now()));
                }
            }
            self.last_focused_window = focused;
        }

        // Poll for async wallpaper decode results and upload to GPU if ready.
        let mut wallpaper_just_loaded = false;
        if let Some(rx) = &self.pending_wallpaper {
            if let Ok(data) = rx.try_recv() {
                if let Some((tex, w, h)) =
                    Self::upload_wallpaper_texture(&self.gl, &data, self.hdr_enabled)
                {
                    self.wallpaper_texture = Some(tex);
                    self.wallpaper_img_w = w;
                    self.wallpaper_img_h = h;
                    self.wallpaper_mode = data.mode;
                    wallpaper_just_loaded = true;
                    log::info!("compositor: async wallpaper ready ({}x{})", w, h);
                }
                self.pending_wallpaper = None;
            }
        }
        // Poll per-monitor wallpaper results
        self.pending_monitor_wallpapers.retain_mut(|(idx, rx)| {
            if let Ok(data) = rx.try_recv() {
                if let Some(mw) = self.monitor_wallpapers.get_mut(*idx) {
                    if let Some((tex, w, h)) =
                        Self::upload_wallpaper_texture(&self.gl, &data, self.hdr_enabled)
                    {
                        mw.texture = Some(tex);
                        mw.img_w = w;
                        mw.img_h = h;
                        mw.mode = data.mode;
                        wallpaper_just_loaded = true;
                        log::info!(
                            "compositor: async monitor wallpaper [{}] ready ({}x{})",
                            idx,
                            w,
                            h
                        );
                    }
                }
                false // remove from pending list
            } else {
                true // keep waiting
            }
        });
        if wallpaper_just_loaded {
            self.needs_render = true;
        }

        // Skip-unchanged-frame: if scene hasn't changed and no textures are
        // dirty, we can skip the entire GL render (unless screenshot pending or HUD active).
        // While scanning, also feed the precise dirty-rect tracker so we do not
        // walk the scene a second time later in the frame.
        let mut has_dirty = false;
        for &(win, _, _, _, _) in scene {
            let dirty_rect = self.windows.get(&win).and_then(|wt| {
                (wt.dirty || wt.needs_pixmap_refresh)
                    .then(|| DirtyRect::new(wt.x, wt.y, wt.w, wt.h))
            });
            if let Some(dirty_rect) = dirty_rect {
                has_dirty = true;
                self.dirty_region_tracker.mark_dirty(dirty_rect);
            }
        }
        let explicit_render = std::mem::replace(&mut self.needs_render, false);
        let force_render = self.screenshot_requests.has_pending()
            || self.debug_hud
            || self.transition_active()
            || self.overview_active
            || self.expose_active
            || expose_animating
            || snap_animating
            || peek_animating
            || genie_active
            || ripples_active
            || focus_highlight_active
            || wallpaper_crossfade_active
            || self.recording_active
            || self.annotation_active
            || wallpaper_just_loaded
            || wobbly_active
            || slime_updated
            || slime_active
            || explicit_render;
        let hash = Self::scene_hash(scene, focused);
        let scene_changed = hash != self.last_scene_hash;
        if !has_dirty && !fades_active && !force_render && !scene_changed {
            return false;
        }
        self.last_scene_hash = hash;

        // Snapshot config once for the whole frame. status_bar_name / border_px
        // were previously loaded 4× per frame from separate ArcSwap guards.
        let frame_cfg = crate::config::CONFIG.load();
        let frame_status_bar_name = frame_cfg.status_bar_name();

        // Reset tilt targets — the render loop will set them if a focused window
        // uses tilt; otherwise they stay at 0 so the tilt smoothly returns to rest.
        if self.window_tilt {
            self.tilt_target_x = 0.0;
            self.tilt_target_y = 0.0;
        }

        // Invalidate blur cache when scene structure/focus changes or animations
        // are active — these affect the rendered output of windows below the
        // frosted window even though no individual texture is "dirty".
        if scene_changed || fades_active || force_render {
            self.blur_cache_hash = 0;
        }

        // Ensure context is current
        if !self.context_current {
            unsafe {
                x11::glx::glXMakeContextCurrent(
                    self.xlib_display,
                    self.glx_drawable,
                    self.glx_drawable,
                    self.glx_context,
                );
            }
            self.context_current = true;
        }

        // Recreate pixmaps for windows that were resized (batched, single XSync)
        self.refresh_pixmaps();

        // Collect which windows are dirty this frame (before TFP refresh clears
        // the flags).  Used by the blur cache to skip expensive blur passes when
        // only the frosted window itself updated (e.g. fcitx candidate list).
        let mut blur_dirty_wins = std::mem::take(&mut self.scratch_blur_dirty);
        blur_dirty_wins.clear();
        blur_dirty_wins.reserve(scene.len());
        blur_dirty_wins.extend(scene.iter().filter_map(|&(win, _, _, _, _)| {
            self.windows
                .get(&win)
                .and_then(|wt| if wt.dirty { Some(win) } else { None })
        }));
        blur_dirty_wins.sort_unstable();

        // Refresh TFP textures for dirty windows with per-frame time budget.
        // Focused window always updates; others update within 3ms budget.
        // NOTE: We intentionally do NOT call glGetError() here.
        // Genuine pixmap invalidation is handled by update_geometry → needs_pixmap_refresh.
        let tfp_budget = std::time::Duration::from_micros(3000); // 3ms
        let tfp_start = std::time::Instant::now();

        // Build priority-ordered window list: focused first, then rest of scene
        let mut tfp_order = std::mem::take(&mut self.scratch_tfp_order);
        tfp_order.clear();
        tfp_order.reserve(scene.len());
        let mut focused_in_scene = false;
        if let Some(fw) = focused {
            tfp_order.push(fw);
        }
        for &(win, _, _, _, _) in scene {
            if Some(win) == focused {
                focused_in_scene = true;
            } else {
                tfp_order.push(win);
            }
        }
        if focused.is_some() && !focused_in_scene {
            tfp_order.remove(0);
        }

        let mut tfp_budget_exhausted = false;
        for win in &tfp_order {
            let win = *win;
            // Budget check: focused window (index 0) always updates
            if tfp_budget_exhausted && Some(win) != focused {
                continue;
            }
            if let Some(wt) = self.windows.get_mut(&win) {
                if wt.dirty && wt.glx_pixmap != 0 {
                    // Audio sync: skip texture update if this window's audio timing
                    // says it's not yet time to present the next frame.
                    // This prevents forcing all video windows into the compositor's
                    // frame rate, which was the root cause of audio-video desync.
                    if wt.audio_sync_target.is_some() {
                        if !self.audio_sync.should_render(win) {
                            continue;
                        }
                        // Check for stale audio streams — fall back to normal rendering
                        if self.audio_sync.should_fallback(win) {
                            self.audio_sync.unregister_stream(win);
                            wt.audio_sync_target = None;
                            log::debug!("compositor: audio sync fallback for 0x{:x} (stale)", win);
                        }
                    }

                    // Phase 2.3: Check fence before rebind — skip if GPU not done yet
                    if let Some(fence) = wt.pending_fence.take() {
                        let status = unsafe { self.gl.client_wait_sync(fence, 0, 0) };
                        if status == glow::TIMEOUT_EXPIRED {
                            // GPU not done yet, skip this window's update; use old texture
                            wt.pending_fence = Some(fence);
                            continue;
                        }
                        unsafe {
                            self.gl.delete_sync(fence);
                        }
                    }

                    unsafe {
                        self.gl.bind_texture(glow::TEXTURE_2D, Some(wt.gl_texture));
                        (self.tfp.release)(self.xlib_display, wt.glx_pixmap, GLX_FRONT_LEFT_EXT);
                        (self.tfp.bind)(
                            self.xlib_display,
                            wt.glx_pixmap,
                            GLX_FRONT_LEFT_EXT,
                            std::ptr::null(),
                        );
                        self.gl.bind_texture(glow::TEXTURE_2D, None);

                        // P6B: Use GPU fence sync manager for non-blocking TFP sync
                        if let Ok(fence) = self.gl.fence_sync(glow::SYNC_GPU_COMMANDS_COMPLETE, 0) {
                            self.gpu_fence_sync_mgr.register_fence(win, fence);
                        }
                    }
                    wt.dirty = false;

                    // Mark frame rendered in audio sync manager
                    if wt.audio_sync_target.is_some() {
                        self.audio_sync.mark_frame_rendered(win);
                    }

                    // Check budget (but not for focused window)
                    if Some(win) != focused && tfp_start.elapsed() > tfp_budget {
                        tfp_budget_exhausted = true;
                    }
                }
            }
        }

        // --- Occlusion culling ---
        let mut first_visible = 0usize;
        {
            let sw = self.screen_w as i32;
            let sh = self.screen_h as i32;
            for i in (0..scene.len()).rev() {
                let (win, x, y, w, h) = scene[i];
                let is_rgba = self.windows.get(&win).map_or(false, |wt| wt.has_rgba);
                let has_fade = self
                    .windows
                    .get(&win)
                    .map_or(false, |wt| wt.fade_opacity < 1.0);
                if !is_rgba
                    && !has_fade
                    && x <= 0
                    && y <= 0
                    && (x + w as i32) >= sw
                    && (y + h as i32) >= sh
                {
                    first_visible = i;
                    break;
                }
            }
        }

        // Feature 8/9/10: If postprocessing is active, render into postprocess FBO
        let postprocess_active = self.needs_postprocess() && self.postprocess_fbo.is_some();
        if postprocess_active {
            let (pp_fbo, _) = self.postprocess_fbo.as_ref().unwrap();
            unsafe {
                self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(*pp_fbo));
            }
        }

        // P5C Phase 3: Apply scissor test using rectangle-based dirty tracker
        let dirty_rect = self.dirty_region_tracker.merged();
        let use_scissor = self.partial_damage_enabled && dirty_rect.is_some() && !force_render;
        let mut damage_scissor = (0i32, 0i32, self.screen_w as i32, self.screen_h as i32);
        if let (true, Some(rect)) = (use_scissor, dirty_rect) {
            unsafe {
                self.gl.enable(glow::SCISSOR_TEST);
                // GL scissor uses bottom-left origin
                let gl_y = self.screen_h as i32 - rect.y - rect.height as i32;
                damage_scissor = (rect.x, gl_y, rect.width as i32, rect.height as i32);
                self.gl.scissor(
                    damage_scissor.0,
                    damage_scissor.1,
                    damage_scissor.2,
                    damage_scissor.3,
                );
            }
        }
        self.damage_tracker.clear();
        self.dirty_region_tracker.clear(); // P5C: Clear rect tracker

        // Clear
        unsafe {
            self.gl
                .viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
            self.gl.clear_color(0.0, 0.0, 0.0, 0.0);
            self.gl.clear(glow::COLOR_BUFFER_BIT);
        }

        // Build orthographic projection matrix (column-major)
        let proj = ortho(
            0.0,
            self.screen_w as f32,
            self.screen_h as f32,
            0.0,
            -1.0,
            1.0,
        );

        // Draw wallpaper background (per-monitor or global fallback)
        // Skip if a fully-opaque window already covers the entire screen (occluded).
        {
            let wallpaper_occluded = first_visible > 0;
            let has_wallpaper = !wallpaper_occluded
                && (!self.monitor_wallpapers.is_empty() || self.wallpaper_texture.is_some());
            if has_wallpaper {
                unsafe {
                    self.gl.use_program(Some(self.program));
                    self.gl.uniform_matrix_4_f32_slice(
                        self.win_uniforms.projection.as_ref(),
                        false,
                        &proj,
                    );
                    self.gl.uniform_1_i32(self.win_uniforms.texture.as_ref(), 0);
                    self.gl.bind_vertex_array(Some(self.quad_vao));
                    self.gl
                        .uniform_1_f32(self.win_uniforms.opacity.as_ref(), 1.0);
                    self.gl
                        .uniform_1_f32(self.win_uniforms.radius.as_ref(), 0.0);
                    self.gl.uniform_1_f32(self.win_uniforms.dim.as_ref(), 1.0);
                    self.gl
                        .uniform_4_f32(self.win_uniforms.uv_rect.as_ref(), 0.0, 0.0, 1.0, 1.0);
                    self.gl.active_texture(glow::TEXTURE0);

                    if !self.monitor_wallpapers.is_empty() {
                        // Temporarily disable damage-region scissor for wallpaper
                        if use_scissor {
                            self.gl.disable(glow::SCISSOR_TEST);
                        }

                        // Per-monitor wallpaper rendering with per-monitor scissor
                        for mw in &self.monitor_wallpapers {
                            // Resolve texture: per-monitor override or global default
                            let (tex, mode, iw, ih) = if let Some(t) = mw.texture {
                                (t, mw.mode, mw.img_w, mw.img_h)
                            } else if let Some(t) = self.wallpaper_texture {
                                (
                                    t,
                                    self.wallpaper_mode,
                                    self.wallpaper_img_w,
                                    self.wallpaper_img_h,
                                )
                            } else {
                                continue;
                            };

                            // Scissor to this monitor's area
                            let gl_y = self.screen_h as i32 - (mw.mon_y + mw.mon_h as i32);
                            self.gl.enable(glow::SCISSOR_TEST);
                            self.gl
                                .scissor(mw.mon_x, gl_y, mw.mon_w as i32, mw.mon_h as i32);

                            let area = (
                                mw.mon_x as f32,
                                mw.mon_y as f32,
                                mw.mon_w as f32,
                                mw.mon_h as f32,
                            );
                            let (rx, ry, rw, rh) = compute_wallpaper_rect(mode, area, iw, ih);
                            self.gl
                                .uniform_4_f32(self.win_uniforms.rect.as_ref(), rx, ry, rw, rh);
                            self.gl
                                .uniform_2_f32(self.win_uniforms.size.as_ref(), rw, rh);
                            self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                        }
                        self.gl.disable(glow::SCISSOR_TEST);
                    } else if let Some(wp_tex) = self.wallpaper_texture {
                        // Single global wallpaper (no monitors set yet)
                        let area = (0.0, 0.0, self.screen_w as f32, self.screen_h as f32);
                        let (rx, ry, rw, rh) = compute_wallpaper_rect(
                            self.wallpaper_mode,
                            area,
                            self.wallpaper_img_w,
                            self.wallpaper_img_h,
                        );
                        self.gl
                            .uniform_4_f32(self.win_uniforms.rect.as_ref(), rx, ry, rw, rh);
                        self.gl
                            .uniform_2_f32(self.win_uniforms.size.as_ref(), rw, rh);
                        self.gl.bind_texture(glow::TEXTURE_2D, Some(wp_tex));
                        self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                    }

                    // Phase 3.5: Draw old wallpaper for crossfade
                    if let (Some(old_tex), Some(start)) =
                        (self.old_wallpaper_texture, self.wallpaper_transition_start)
                    {
                        let elapsed = start.elapsed().as_millis() as f32;
                        let duration = self.wallpaper_crossfade_duration_ms as f32;
                        let old_opacity = (1.0 - elapsed / duration).max(0.0);
                        if old_opacity > 0.0 {
                            let area = (0.0, 0.0, self.screen_w as f32, self.screen_h as f32);
                            let (rx, ry, rw, rh) = compute_wallpaper_rect(
                                self.wallpaper_mode,
                                area,
                                self.wallpaper_img_w,
                                self.wallpaper_img_h,
                            );
                            self.gl
                                .uniform_1_f32(self.win_uniforms.opacity.as_ref(), old_opacity);
                            self.gl
                                .uniform_4_f32(self.win_uniforms.rect.as_ref(), rx, ry, rw, rh);
                            self.gl
                                .uniform_2_f32(self.win_uniforms.size.as_ref(), rw, rh);
                            self.gl.bind_texture(glow::TEXTURE_2D, Some(old_tex));
                            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                            // Restore opacity for subsequent draws
                            self.gl
                                .uniform_1_f32(self.win_uniforms.opacity.as_ref(), 1.0);
                        }
                    }

                    self.gl.bind_texture(glow::TEXTURE_2D, None);
                    self.gl.bind_vertex_array(None);
                    self.gl.use_program(None);

                    // Restore damage-region scissor if it was active
                    if use_scissor {
                        self.gl.scissor(
                            damage_scissor.0,
                            damage_scissor.1,
                            damage_scissor.2,
                            damage_scissor.3,
                        );
                        self.gl.enable(glow::SCISSOR_TEST);
                    }
                }
            }
        }

        let visible_scene = &scene[first_visible..];

        // When overview is active, skip rendering windows that belong to the
        // overview monitor — they would be hidden behind the opaque overview
        // background anyway and their presence can visually compete with the
        // 3D prism thumbnails.
        // Copy fields out so the closure does not borrow `self` (which
        // prevents subsequent &mut self calls like apply_temporal_blur_mix).
        let ov_active = self.overview_active;
        let ov_mx = self.overview_mon_x;
        let ov_my = self.overview_mon_y;
        let ov_mw = self.overview_mon_w as i32;
        let ov_mh = self.overview_mon_h as i32;
        let overview_skip = move |x: i32, y: i32, w: u32, h: u32| -> bool {
            if !ov_active {
                return false;
            }
            let cx = x + w as i32 / 2;
            let cy = y + h as i32 / 2;
            cx >= ov_mx && cx < ov_mx + ov_mw && cy >= ov_my && cy < ov_my + ov_mh
        };

        // === Pass 1: Draw shadows (feature 14: improved shape) ===
        if self.shadow_enabled && self.shadow_radius > 0.0 {
            unsafe {
                // Phase 2: Use state tracker for shadow pass
                self.gl_state_tracker
                    .use_program(&self.gl, Some(self.shadow_program));
                self.gl.uniform_matrix_4_f32_slice(
                    self.shadow_uniforms.projection.as_ref(),
                    false,
                    &proj,
                );
                self.gl_state_tracker
                    .bind_vertex_array(&self.gl, Some(self.quad_vao));

                let spread = self.shadow_radius;
                let [ox, oy] = self.shadow_offset;
                let [sr, sg, sb, sa] = self.shadow_color;
                let bottom_extra = self.shadow_bottom_extra;

                self.gl
                    .uniform_1_f32(self.shadow_uniforms.spread.as_ref(), spread);

                let status_bar_name = frame_status_bar_name;

                for &(win, x, y, w, h) in visible_scene {
                    if overview_skip(x, y, w, h) {
                        continue;
                    }
                    let wt = match self.windows.get(&win) {
                        Some(wt) => wt,
                        None => continue,
                    };
                    // Skip shadow for statusbar
                    if wt.class_name == status_bar_name || wt.class_name.contains(status_bar_name) {
                        continue;
                    }
                    // Per-window shadow exclude
                    if class_matches_exclude(&wt.class_name, &self.shadow_exclude) {
                        continue;
                    }
                    // Feature 14: Skip shadow for shaped windows (non-rectangular)
                    if wt.is_shaped {
                        continue;
                    }
                    // Skip compositor shadow for RGBA windows — they manage their own shadow
                    if wt.has_rgba {
                        continue;
                    }
                    // Fade: modulate shadow alpha
                    let fade = wt.fade_opacity;
                    let sa_faded = sa * fade;
                    if sa_faded <= 0.0 {
                        continue;
                    }

                    self.gl.uniform_4_f32(
                        self.shadow_uniforms.shadow_color.as_ref(),
                        sr,
                        sg,
                        sb,
                        sa_faded,
                    );

                    // Feature 3: Per-window corner radius for shadow
                    let win_radius = wt.corner_radius_override.unwrap_or(
                        if class_matches_exclude(&wt.class_name, &self.rounded_corners_exclude) {
                            0.0
                        } else {
                            self.corner_radius
                        },
                    );
                    self.gl
                        .uniform_1_f32(self.shadow_uniforms.radius.as_ref(), win_radius);

                    // Feature 14: Non-uniform shadow offset (heavier bottom)
                    let sy_offset = oy + bottom_extra;
                    let anim_s = wt.anim_scale;
                    let win_w = w as f32 * anim_s;
                    let win_h = h as f32 * anim_s;
                    let cx = x as f32 + (w as f32 - win_w) * 0.5;
                    let cy = y as f32 + (h as f32 - win_h) * 0.5;
                    let mut sx = cx + ox - spread;
                    let mut sy = cy + sy_offset - spread;
                    let mut sw = win_w + 2.0 * spread;
                    let mut sh = win_h + 2.0 * spread + bottom_extra;

                    // Dynamic shadow offset for tilted focused window
                    if self.window_tilt && focused == Some(win) {
                        let tilt_mag =
                            (self.tilt_current_x.powi(2) + self.tilt_current_y.powi(2)).sqrt();
                        let extra = tilt_mag * 15.0;
                        sx += self.tilt_current_y * 30.0 - extra;
                        sy += self.tilt_current_x * 30.0 - extra;
                        sw += extra * 2.0;
                        sh += extra * 2.0;
                    }
                    self.gl
                        .uniform_4_f32(self.shadow_uniforms.rect.as_ref(), sx, sy, sw, sh);
                    self.gl
                        .uniform_2_f32(self.shadow_uniforms.size.as_ref(), win_w, win_h);
                    self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                }

                self.gl.bind_vertex_array(None);
                self.gl.use_program(None);
            }
        }

        // Phase 2.2: Auto blur quality downgrade during animations/transitions
        if self.blur_quality_auto {
            self.blur_quality = if self.transition_active() || self.overview_active {
                BlurQuality::Minimal
            } else if fades_active || wobbly_active {
                BlurQuality::Reduced
            } else {
                BlurQuality::Full
            };
        }

        // === Pass 1.5: Background blur (now computed per-window in Pass 2) ===
        let blur_available =
            self.blur_enabled && !self.blur_fbos.is_empty() && self.scene_fbo.is_some();

        // === Pass 2: Draw window textures ===
        let wm_border_px = frame_cfg.border_px() as f32;

        // Count actual client windows (excluding statusbar) to apply smart borders
        let status_bar_name = frame_status_bar_name;
        let client_window_count = visible_scene
            .iter()
            .filter(|&&(win, _, _, _, _)| {
                self.windows
                    .get(&win)
                    .map(|wt| {
                        !(wt.class_name == status_bar_name
                            || wt.class_name.contains(status_bar_name))
                    })
                    .unwrap_or(false)
            })
            .count();

        let effective_border_enabled =
            (self.border_enabled || wm_border_px > 0.0) && client_window_count > 1;
        let base_border_width = if self.border_enabled {
            self.border_width
        } else {
            wm_border_px
        };

        // Track the below-scene for blur caching: a running hash of (win, x, y, w, h)
        // for all windows drawn so far, plus whether any was dirty this frame.
        let mut blur_below_hash: u64 = 0u64;
        let mut blur_below_dirty = false;

        unsafe {
            // Phase 2: Use state tracker for main window rendering pass
            self.gl_state_tracker
                .use_program(&self.gl, Some(self.program));
            self.gl
                .uniform_matrix_4_f32_slice(self.win_uniforms.projection.as_ref(), false, &proj);
            self.gl.uniform_1_i32(self.win_uniforms.texture.as_ref(), 0);
            self.gl
                .uniform_4_f32(self.win_uniforms.uv_rect.as_ref(), 0.0, 0.0, 1.0, 1.0);
            self.gl
                .uniform_1_f32(self.win_uniforms.ripple_amplitude.as_ref(), 0.0);
            self.gl_state_tracker
                .bind_vertex_array(&self.gl, Some(self.quad_vao));

            let status_bar_name_main = frame_status_bar_name;

            for &(win, x, y, w, h) in visible_scene {
                if overview_skip(x, y, w, h) {
                    continue;
                }
                if let Some(wt) = self.windows.get(&win) {
                    let is_focused = focused == Some(win);
                    let fade = wt.fade_opacity;
                    if fade <= 0.0 {
                        continue;
                    }
                    let focus_highlight_active_for_win =
                        if let Some((hw, start)) = self.focus_highlight_start {
                            hw == win
                                && start.elapsed().as_millis()
                                    < self.focus_highlight_duration_ms as u128
                        } else {
                            false
                        };
                    let attention_active_for_win = wt.is_urgent && self.attention_animation;
                    let has_special_border = attention_active_for_win || wt.is_pip;

                    // Phase 5.3: Peek opacity multiplier
                    let peek_mul = self.peek_opacity_for(&wt.class_name);

                    // Feature 3: Per-window corner radius
                    // Skip compositor rounding for override-redirect RGBA windows
                    // (popups, menus, tooltips) — they manage their own shape.
                    let radius = if wt.is_override_redirect && wt.has_rgba {
                        0.0
                    } else {
                        wt.corner_radius_override.unwrap_or(
                            if class_matches_exclude(&wt.class_name, &self.rounded_corners_exclude)
                            {
                                0.0
                            } else {
                                self.corner_radius
                            },
                        )
                    };
                    self.gl
                        .uniform_1_f32(self.win_uniforms.radius.as_ref(), radius);

                    // Compute effective opacity
                    let is_statusbar = wt.class_name == status_bar_name_main
                        || wt.class_name.contains(status_bar_name_main);

                    let base_opacity = if is_statusbar {
                        1.0
                    } else if is_focused {
                        self.active_opacity
                    } else {
                        self.inactive_opacity
                    };
                    let rule_opacity = wt.opacity_override.unwrap_or(base_opacity);
                    let has_explicit_transparency = wt.opacity_override.map_or(false, |o| o < 1.0);
                    let inactive_dim_factor =
                        if is_statusbar || is_focused || wt.is_override_redirect {
                            1.0
                        } else {
                            self.inactive_dim
                        };
                    let dim = if wt.has_rgba {
                        rule_opacity * fade * inactive_dim_factor
                    } else {
                        inactive_dim_factor
                    };

                    // detect_client_opacity: if window manages its own alpha, don't force opacity.
                    // For RGB windows, keep fully opaque by default, but allow explicit
                    // per-window opacity overrides (and fade animations) to output real
                    // alpha so translucent windows can reveal realtime blurred backdrop.
                    // Override-redirect RGBA windows (popups, menus, tooltips) always
                    // use their own alpha — they render their own shadows/borders.
                    let opacity = if wt.has_rgba {
                        if self.detect_client_opacity || wt.is_override_redirect {
                            -dim
                        } else if has_explicit_transparency || fade < 1.0 {
                            (rule_opacity * fade).clamp(0.0, 1.0)
                        } else {
                            1.0f32
                        }
                    } else {
                        if has_explicit_transparency || fade < 1.0 {
                            (rule_opacity * fade).clamp(0.0, 1.0)
                        } else {
                            1.0f32
                        }
                    };

                    // Phase 5.3: Apply peek opacity
                    let opacity = if peek_mul < 1.0 {
                        if opacity < 0.0 {
                            opacity * peek_mul
                        } else {
                            (opacity * peek_mul).clamp(0.0, 1.0)
                        }
                    } else {
                        opacity
                    };
                    // Feature 4: Apply per-window scale + Phase 3.4 focus bounce
                    let focus_bounce =
                        if !is_statusbar && self.focus_highlight && focused == Some(win) {
                            if let Some((hw, start)) = self.focus_highlight_start {
                                if hw == win
                                    && start.elapsed().as_millis()
                                        < self.focus_highlight_duration_ms as u128
                                {
                                    let t = start.elapsed().as_millis() as f32
                                        / self.focus_highlight_duration_ms as f32;
                                    1.0 + 0.02 * (1.0 - t) * ((t * std::f32::consts::PI).sin())
                                } else {
                                    1.0
                                }
                            } else {
                                1.0
                            }
                        } else {
                            1.0
                        };
                    let scale = wt.scale * wt.anim_scale * focus_bounce;
                    let (draw_x, draw_y, draw_w, draw_h) = if (scale - 1.0).abs() > f32::EPSILON {
                        let cw = w as f32 * scale;
                        let ch = h as f32 * scale;
                        let cx = x as f32 + (w as f32 - cw) * 0.5;
                        let cy = y as f32 + (h as f32 - ch) * 0.5;
                        (cx, cy, cw, ch)
                    } else {
                        (x as f32, y as f32, w as f32, h as f32)
                    };

                    // Feature 13: Draw blurred background behind translucent windows (with frame extents mask)
                    // Blur is captured per-window so it includes all windows drawn below.
                    if blur_available {
                        if self.needs_backdrop_blur(wt, status_bar_name_main) {
                            // Blur cache: if no window below this one was dirty and
                            // the below-scene structure hasn't changed, the previous
                            // blur result stored in blur_fbos[0] is still valid.
                            let cache_hit = !blur_below_dirty
                                && blur_below_hash != 0
                                && blur_below_hash == self.blur_cache_hash;

                            // Track blur cache statistics for diagnostics
                            if cache_hit {
                                self.frame_stats.blur_cache_hits += 1;
                            } else {
                                self.frame_stats.blur_cache_misses += 1;
                            }

                            let mut blur_tex = if cache_hit {
                                Some(self.blur_fbos[0].texture)
                            } else {
                                let blur_bench_start = if self.benchmark.is_running() {
                                    Some(std::time::Instant::now())
                                } else {
                                    None
                                };

                                // Temporarily break out of the window shader to run blur passes.
                                // Capture the current framebuffer (which includes all windows
                                // drawn so far) and produce a blurred texture from it.
                                self.gl.bind_vertex_array(None);
                                self.gl.use_program(None);
                                if use_scissor {
                                    self.gl.disable(glow::SCISSOR_TEST);
                                }

                                // P5B Phase 3: Compute monitor-aware blur strength
                                let base_levels = if wt.is_frosted {
                                    self.frosted_glass_strength as usize
                                } else {
                                    // Get monitor-specific blur strength based on refresh rate
                                    let monitor_id =
                                        self.get_window_monitor_id(wt.x, wt.y, wt.w, wt.h);
                                    let monitor_hz = self.get_monitor_refresh_hz(monitor_id);
                                    let monitor_strength = self
                                        .get_blur_strength_for_hz(monitor_hz)
                                        .unwrap_or(self.blur_strength);

                                    // Cap at available FBO levels (can't exceed pre-created fbos)
                                    (monitor_strength as usize).min(self.blur_fbos.len())
                                };
                                // Phase 2.2: Apply blur quality cap (per-window adaptive)
                                let window_quality = self.compute_window_blur_quality(wt, focused);
                                let blur_levels = match window_quality {
                                    BlurQuality::Full => base_levels,
                                    BlurQuality::Reduced => (base_levels / 2).max(1),
                                    BlurQuality::Minimal => 1,
                                };
                                let tex = self.run_blur_passes_from_fbo(
                                    if postprocess_active {
                                        self.postprocess_fbo.as_ref().map(|(fbo, _)| *fbo)
                                    } else {
                                        None
                                    },
                                    blur_levels,
                                );

                                if let Some(start) = blur_bench_start {
                                    let pixel_count: u64 = self
                                        .blur_fbos
                                        .iter()
                                        .take(blur_levels)
                                        .map(|l| l.w as u64 * l.h as u64)
                                        .sum();
                                    self.benchmark.record_blur_cost(
                                        pixel_count,
                                        start.elapsed().as_secs_f32() * 1000.0,
                                    );
                                }

                                // Restore state for window drawing
                                if use_scissor {
                                    self.gl.enable(glow::SCISSOR_TEST);
                                }
                                if postprocess_active {
                                    let (pp_fbo, _) = self.postprocess_fbo.as_ref().unwrap();
                                    self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(*pp_fbo));
                                } else {
                                    self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                                }
                                self.gl
                                    .viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
                                // Phase 2: Restore state via tracker after blur
                                self.gl_state_tracker
                                    .use_program(&self.gl, Some(self.program));
                                self.gl.uniform_matrix_4_f32_slice(
                                    self.win_uniforms.projection.as_ref(),
                                    false,
                                    &proj,
                                );
                                self.gl.uniform_1_i32(self.win_uniforms.texture.as_ref(), 0);
                                self.gl.uniform_4_f32(
                                    self.win_uniforms.uv_rect.as_ref(),
                                    0.0,
                                    0.0,
                                    1.0,
                                    1.0,
                                );
                                self.gl_state_tracker
                                    .bind_vertex_array(&self.gl, Some(self.quad_vao));
                                self.gl
                                    .uniform_1_f32(self.win_uniforms.radius.as_ref(), radius);

                                self.blur_cache_hash = blur_below_hash;
                                tex
                            };

                            // P4: Temporal blur mix — GLSL blend of current+previous blur on
                            // stable frames. apply_temporal_blur_mix writes into prev_blur_fbo and
                            // returns its texture, which we then composite as the backdrop blur.
                            // On the first temporal frame the function blits a seed; subsequent
                            // frames mix at temporal_blur_mix_ratio.
                            if let Some(blurred) = blur_tex {
                                let final_blur = if !cache_hit
                                    && self.temporal_blur_enabled
                                    && self.prev_window_positions_hash != 0
                                {
                                    let mixed = self.apply_temporal_blur_mix(blurred);
                                    // Restore framebuffer + window-shader state for the
                                    // backdrop-quad draw that follows: the mix function
                                    // changes program/VAO/active framebuffer.
                                    if postprocess_active {
                                        let (pp_fbo, _) = self.postprocess_fbo.as_ref().unwrap();
                                        self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(*pp_fbo));
                                    } else {
                                        self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                                    }
                                    self.gl.viewport(
                                        0,
                                        0,
                                        self.screen_w as i32,
                                        self.screen_h as i32,
                                    );
                                    self.gl_state_tracker
                                        .use_program(&self.gl, Some(self.program));
                                    self.gl.uniform_matrix_4_f32_slice(
                                        self.win_uniforms.projection.as_ref(),
                                        false,
                                        &proj,
                                    );
                                    self.gl.uniform_1_i32(self.win_uniforms.texture.as_ref(), 0);
                                    self.gl.uniform_4_f32(
                                        self.win_uniforms.uv_rect.as_ref(),
                                        0.0,
                                        0.0,
                                        1.0,
                                        1.0,
                                    );
                                    self.gl_state_tracker
                                        .bind_vertex_array(&self.gl, Some(self.quad_vao));
                                    self.gl
                                        .uniform_1_f32(self.win_uniforms.radius.as_ref(), radius);
                                    mixed
                                } else {
                                    blurred
                                };
                                blur_tex = Some(final_blur);
                            }

                            if let Some(blur_tex) = blur_tex {
                                // Feature 13: If blur_use_frame_extents, crop blur to client area
                                // For RGBA windows, always use full rect so transparent areas show blur
                                let (bx, by, bw, bh) =
                                    if self.blur_use_frame_extents && !wt.has_rgba {
                                        let [fl, fr, ft, fb] = wt.frame_extents;
                                        let bx = draw_x + fl as f32;
                                        let by = draw_y + ft as f32;
                                        let bw = (draw_w - fl as f32 - fr as f32).max(1.0);
                                        let bh = (draw_h - ft as f32 - fb as f32).max(1.0);
                                        (bx, by, bw, bh)
                                    } else {
                                        (draw_x, draw_y, draw_w, draw_h)
                                    };
                                self.gl.active_texture(glow::TEXTURE0);
                                self.gl.bind_texture(glow::TEXTURE_2D, Some(blur_tex));
                                let uv_x = (bx / self.screen_w as f32).clamp(0.0, 1.0);
                                let uv_w = (bw / self.screen_w as f32).clamp(0.0, 1.0);
                                let uv_y_top = (by / self.screen_h as f32).clamp(0.0, 1.0);
                                let uv_h = (bh / self.screen_h as f32).clamp(0.0, 1.0);
                                self.gl.uniform_4_f32(
                                    self.win_uniforms.uv_rect.as_ref(),
                                    uv_x,
                                    uv_y_top,
                                    uv_w,
                                    uv_h,
                                );
                                self.gl
                                    .uniform_1_f32(self.win_uniforms.opacity.as_ref(), fade);
                                self.gl.uniform_1_f32(self.win_uniforms.dim.as_ref(), 1.0);
                                self.gl
                                    .uniform_2_f32(self.win_uniforms.size.as_ref(), bw, bh);
                                self.gl.uniform_4_f32(
                                    self.win_uniforms.rect.as_ref(),
                                    bx,
                                    by,
                                    bw,
                                    bh,
                                );
                                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                                // Restore default UV for regular window textures.
                                self.gl.uniform_4_f32(
                                    self.win_uniforms.uv_rect.as_ref(),
                                    0.0,
                                    0.0,
                                    1.0,
                                    1.0,
                                );
                            }
                        }
                    }

                    // Phase 3.1: Motion trail ghost copies at historical positions
                    if self.motion_trail_enabled && !wt.motion_trail.is_empty() {
                        let trail_len = wt.motion_trail.len();
                        for (i, &(tx, ty)) in wt.motion_trail.iter().enumerate() {
                            let trail_opacity =
                                self.motion_trail_opacity * (i as f32 + 1.0) / trail_len as f32;
                            self.gl
                                .uniform_1_f32(self.win_uniforms.opacity.as_ref(), trail_opacity);
                            self.gl.uniform_1_f32(self.win_uniforms.dim.as_ref(), 0.7);
                            self.gl.uniform_4_f32(
                                self.win_uniforms.rect.as_ref(),
                                tx as f32,
                                ty as f32,
                                draw_w,
                                draw_h,
                            );
                            self.gl
                                .uniform_2_f32(self.win_uniforms.size.as_ref(), draw_w, draw_h);
                            self.gl.active_texture(glow::TEXTURE0);
                            self.gl.bind_texture(glow::TEXTURE_2D, Some(wt.gl_texture));
                            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                        }
                    }

                    self.gl.active_texture(glow::TEXTURE0);
                    self.gl.bind_texture(glow::TEXTURE_2D, Some(wt.gl_texture));

                    // Wobbly windows: use grid spring-mass deformation shader
                    if self.wobbly_windows && wt.wobbly.is_some() {
                        let wobbly = wt.wobbly.as_ref().unwrap();
                        self.gl.use_program(Some(self.wobbly_program));
                        self.gl.uniform_matrix_4_f32_slice(
                            self.wobbly_uniforms.projection.as_ref(),
                            false,
                            &proj,
                        );
                        self.gl.uniform_4_f32(
                            self.wobbly_uniforms.rect.as_ref(),
                            draw_x,
                            draw_y,
                            draw_w,
                            draw_h,
                        );
                        self.gl
                            .uniform_1_i32(self.wobbly_uniforms.texture.as_ref(), 0);
                        self.gl
                            .uniform_1_f32(self.wobbly_uniforms.opacity.as_ref(), opacity);
                        self.gl
                            .uniform_1_f32(self.wobbly_uniforms.radius.as_ref(), radius);
                        self.gl
                            .uniform_2_f32(self.wobbly_uniforms.size.as_ref(), draw_w, draw_h);
                        self.gl
                            .uniform_1_f32(self.wobbly_uniforms.dim.as_ref(), dim);
                        self.gl.uniform_4_f32(
                            self.wobbly_uniforms.uv_rect.as_ref(),
                            0.0,
                            0.0,
                            1.0,
                            1.0,
                        );
                        // Upload grid offsets as flat vec2 array
                        let flat: Vec<f32> =
                            wobbly.offsets.iter().flat_map(|o| [o[0], o[1]]).collect();
                        self.gl
                            .uniform_2_f32_slice(self.wobbly_uniforms.grid_offsets.as_ref(), &flat);
                        let grid_n = wobbly.grid_n as i32;
                        self.gl
                            .uniform_1_i32(self.wobbly_uniforms.grid_n.as_ref(), grid_n);
                        // Grid: (grid_n-1)^2 quads, 6 verts each
                        let quads = grid_n - 1;
                        self.gl.draw_arrays(glow::TRIANGLES, 0, quads * quads * 6);

                        // Restore standard window program
                        self.gl.use_program(Some(self.program));
                        self.gl.uniform_matrix_4_f32_slice(
                            self.win_uniforms.projection.as_ref(),
                            false,
                            &proj,
                        );
                        self.gl.uniform_1_i32(self.win_uniforms.texture.as_ref(), 0);
                        self.gl.uniform_4_f32(
                            self.win_uniforms.uv_rect.as_ref(),
                            0.0,
                            0.0,
                            1.0,
                            1.0,
                        );
                        self.gl
                            .uniform_1_f32(self.win_uniforms.radius.as_ref(), radius);
                    } else if self.window_tilt && is_focused && !is_statusbar {
                        // Update tilt target from mouse position (clamped)
                        let cx = draw_x + draw_w * 0.5;
                        let cy = draw_y + draw_h * 0.5;
                        let rel_x = ((self.mouse_x - cx) / (draw_w * 0.5)).clamp(-1.0, 1.0);
                        let rel_y = ((self.mouse_y - cy) / (draw_h * 0.5)).clamp(-1.0, 1.0);
                        self.tilt_target_x = (-rel_y * self.tilt_amount).clamp(-0.35, 0.35);
                        self.tilt_target_y = (rel_x * self.tilt_amount).clamp(-0.35, 0.35);

                        self.gl.use_program(Some(self.tilt_program));
                        self.gl.uniform_matrix_4_f32_slice(
                            self.tilt_uniforms.projection.as_ref(),
                            false,
                            &proj,
                        );
                        self.gl.uniform_4_f32(
                            self.tilt_uniforms.rect.as_ref(),
                            draw_x,
                            draw_y,
                            draw_w,
                            draw_h,
                        );
                        self.gl
                            .uniform_1_i32(self.tilt_uniforms.texture.as_ref(), 0);
                        self.gl
                            .uniform_1_f32(self.tilt_uniforms.opacity.as_ref(), opacity);
                        self.gl
                            .uniform_1_f32(self.tilt_uniforms.radius.as_ref(), radius);
                        self.gl
                            .uniform_2_f32(self.tilt_uniforms.size.as_ref(), draw_w, draw_h);
                        self.gl.uniform_1_f32(self.tilt_uniforms.dim.as_ref(), dim);
                        self.gl.uniform_4_f32(
                            self.tilt_uniforms.uv_rect.as_ref(),
                            0.0,
                            0.0,
                            1.0,
                            1.0,
                        );
                        self.gl.uniform_2_f32(
                            self.tilt_uniforms.tilt.as_ref(),
                            self.tilt_current_x,
                            self.tilt_current_y,
                        );
                        self.gl.uniform_1_f32(
                            self.tilt_uniforms.perspective.as_ref(),
                            self.tilt_perspective,
                        );
                        let grid = self.tilt_grid as i32;
                        self.gl
                            .uniform_1_i32(self.tilt_uniforms.grid_size.as_ref(), grid);
                        self.gl
                            .uniform_2_f32(self.tilt_uniforms.light_dir.as_ref(), 0.0, -1.0);
                        // Grid: grid^2 quads, 6 verts each
                        self.gl.draw_arrays(glow::TRIANGLES, 0, grid * grid * 6);

                        // Restore standard window program
                        self.gl.use_program(Some(self.program));
                        self.gl.uniform_matrix_4_f32_slice(
                            self.win_uniforms.projection.as_ref(),
                            false,
                            &proj,
                        );
                        self.gl.uniform_1_i32(self.win_uniforms.texture.as_ref(), 0);
                        self.gl.uniform_4_f32(
                            self.win_uniforms.uv_rect.as_ref(),
                            0.0,
                            0.0,
                            1.0,
                            1.0,
                        );
                        self.gl
                            .uniform_1_f32(self.win_uniforms.radius.as_ref(), radius);
                    } else {
                        self.gl
                            .uniform_1_f32(self.win_uniforms.opacity.as_ref(), opacity);
                        self.gl.uniform_1_f32(self.win_uniforms.dim.as_ref(), dim);
                        self.gl
                            .uniform_2_f32(self.win_uniforms.size.as_ref(), draw_w, draw_h);
                        self.gl.uniform_4_f32(
                            self.win_uniforms.rect.as_ref(),
                            draw_x,
                            draw_y,
                            draw_w,
                            draw_h,
                        );

                        // Window-open ripple: set per-window distortion uniforms
                        let ripple_prog =
                            self.ripple_active
                                .iter()
                                .find(|r| r.x11_win == win)
                                .map(|r| {
                                    let elapsed = r.start.elapsed().as_secs_f32();
                                    (elapsed / self.ripple_duration).min(1.0)
                                });
                        if let Some(progress) = ripple_prog {
                            self.gl.uniform_1_f32(
                                self.win_uniforms.ripple_progress.as_ref(),
                                progress,
                            );
                            self.gl.uniform_1_f32(
                                self.win_uniforms.ripple_amplitude.as_ref(),
                                self.ripple_amplitude,
                            );
                        } else {
                            self.gl
                                .uniform_1_f32(self.win_uniforms.ripple_amplitude.as_ref(), 0.0);
                        }

                        self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                        // Reset ripple for next window
                        if ripple_prog.is_some() {
                            self.gl
                                .uniform_1_f32(self.win_uniforms.ripple_amplitude.as_ref(), 0.0);
                        }
                    }

                    if !is_statusbar
                        && !wt.is_override_redirect
                        && ((effective_border_enabled && base_border_width > 0.0)
                            || has_special_border)
                    {
                        let color = if focus_highlight_active_for_win {
                            let elapsed_ms =
                                self.focus_highlight_start.unwrap().1.elapsed().as_millis() as f32;
                            let dur = self.focus_highlight_duration_ms as f32;
                            let pulse = ((elapsed_ms / dur * std::f32::consts::PI).sin()).abs();
                            let [r, g, b, a] = self.focus_highlight_color;
                            [r, g, b, a * pulse]
                        } else if attention_active_for_win {
                            let elapsed = self.compositor_start_time.elapsed().as_secs_f32();
                            let pulse = (elapsed * 4.0).sin() * 0.5 + 0.5;
                            let [r, g, b, a] = self.attention_color;
                            [r, g, b, a * pulse]
                        } else if wt.is_pip {
                            self.pip_border_color
                        } else if is_focused {
                            self.border_color_focused
                        } else {
                            self.border_color_unfocused
                        };

                        let bw = if focus_highlight_active_for_win {
                            (base_border_width + 2.0).max(3.0)
                        } else if attention_active_for_win {
                            if effective_border_enabled {
                                base_border_width.max(2.0)
                            } else {
                                2.0
                            }
                        } else if wt.is_pip {
                            self.pip_border_width
                        } else {
                            base_border_width
                        };

                        if bw > 0.0 {
                            let bdr_x = draw_x - bw;
                            let bdr_y = draw_y - bw;
                            let bdr_w = draw_w + 2.0 * bw;
                            let bdr_h = draw_h + 2.0 * bw;

                            self.gl.use_program(Some(self.border_program));
                            self.gl.uniform_matrix_4_f32_slice(
                                self.border_uniforms.projection.as_ref(),
                                false,
                                &proj,
                            );
                            self.gl
                                .uniform_1_f32(self.border_uniforms.border_width.as_ref(), bw);
                            self.gl.uniform_4_f32(
                                self.border_uniforms.border_color.as_ref(),
                                color[0],
                                color[1],
                                color[2],
                                color[3] * fade,
                            );
                            self.gl
                                .uniform_1_f32(self.border_uniforms.radius.as_ref(), radius);
                            self.gl
                                .uniform_2_f32(self.border_uniforms.size.as_ref(), bdr_w, bdr_h);
                            self.gl.uniform_4_f32(
                                self.border_uniforms.rect.as_ref(),
                                bdr_x,
                                bdr_y,
                                bdr_w,
                                bdr_h,
                            );
                            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                            self.gl.use_program(Some(self.program));
                            self.gl.uniform_matrix_4_f32_slice(
                                self.win_uniforms.projection.as_ref(),
                                false,
                                &proj,
                            );
                            self.gl.uniform_1_i32(self.win_uniforms.texture.as_ref(), 0);
                            self.gl.uniform_4_f32(
                                self.win_uniforms.uv_rect.as_ref(),
                                0.0,
                                0.0,
                                1.0,
                                1.0,
                            );
                            self.gl
                                .uniform_1_f32(self.win_uniforms.radius.as_ref(), radius);
                        }
                    }

                    // Update blur below-scene tracking after drawing this window.
                    // The hash encodes (win, x, y, w, h) so structural changes
                    // (reorder, move, resize, add/remove) cause a cache miss.
                    blur_below_hash = blur_below_hash
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(win as u64)
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(((x as u64) << 32) | (y as u32 as u64))
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(((w as u64) << 32) | (h as u64));
                    if blur_dirty_wins.binary_search(&win).is_ok() {
                        blur_below_dirty = true;
                    }
                }
            }

            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
        }

        // === Pass 2b: Genie minimize animations ===
        if !self.genie_active.is_empty() {
            let genie_duration_ms = self.genie_duration_ms;
            unsafe {
                self.gl.use_program(Some(self.genie_program));
                self.gl.uniform_matrix_4_f32_slice(
                    self.genie_uniforms.projection.as_ref(),
                    false,
                    &proj,
                );
                self.gl
                    .uniform_1_i32(self.genie_uniforms.texture.as_ref(), 0);
                self.gl
                    .uniform_4_f32(self.genie_uniforms.uv_rect.as_ref(), 0.0, 0.0, 1.0, 1.0);
                self.gl
                    .uniform_1_f32(self.genie_uniforms.radius.as_ref(), 0.0);
                let grid = 12i32;
                self.gl
                    .uniform_1_i32(self.genie_uniforms.grid_size.as_ref(), grid);
                self.gl.bind_vertex_array(Some(self.quad_vao));

                let dock = self.dock_position;
                for ga in &self.genie_active {
                    let elapsed = ga.start.elapsed().as_millis() as f32;
                    let progress = (elapsed / genie_duration_ms as f32).min(1.0);
                    let opacity = 1.0 - progress;
                    self.gl.uniform_4_f32(
                        self.genie_uniforms.rect.as_ref(),
                        ga.x,
                        ga.y,
                        ga.w,
                        ga.h,
                    );
                    self.gl
                        .uniform_2_f32(self.genie_uniforms.size.as_ref(), ga.w, ga.h);
                    self.gl
                        .uniform_1_f32(self.genie_uniforms.progress.as_ref(), progress);
                    self.gl
                        .uniform_2_f32(self.genie_uniforms.dock_pos.as_ref(), dock.0, dock.1);
                    self.gl.uniform_1_f32(
                        self.genie_uniforms.opacity.as_ref(),
                        if ga.has_rgba { -opacity } else { opacity },
                    );
                    self.gl.uniform_1_f32(self.genie_uniforms.dim.as_ref(), 1.0);
                    self.gl.active_texture(glow::TEXTURE0);
                    self.gl.bind_texture(glow::TEXTURE_2D, Some(ga.gl_texture));
                    self.gl.draw_arrays(glow::TRIANGLES, 0, grid * grid * 6);
                }

                self.gl.bind_vertex_array(None);
                self.gl.use_program(None);
            }
        }

        // === Pass 3c: Window tab bars ===
        if self.window_tabs_enabled && !self.window_groups.is_empty() {
            for &(win, x, y, w, _h) in visible_scene {
                if let Some((_gid, tabs)) = self.find_window_group(win) {
                    self.render_tab_bar(&proj, x as f32, y as f32, w as f32, tabs);
                }
            }
        }

        // Disable scissor (feature 6)
        if use_scissor {
            unsafe {
                self.gl.disable(glow::SCISSOR_TEST);
            }
        }

        // === Pass 4: Post-processing (features 8/9/10) ===
        if postprocess_active {
            if self.slime_state.is_visible() {
                self.run_slime_wave_simulation();
            }
            let slime_wave = self.slime_wave_simulation.as_ref().map(|simulation| {
                (
                    simulation.textures[simulation.front],
                    simulation.width,
                    simulation.height,
                )
            });
            let (_, pp_tex) = self.postprocess_fbo.as_ref().unwrap();
            let pp_tex = *pp_tex;
            unsafe {
                // Switch back to default framebuffer
                self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                self.gl
                    .viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
                self.gl.clear(glow::COLOR_BUFFER_BIT);

                self.gl.use_program(Some(self.postprocess_program));
                // Set up fullscreen quad
                let pp_proj = ortho(
                    0.0,
                    self.screen_w as f32,
                    self.screen_h as f32,
                    0.0,
                    -1.0,
                    1.0,
                );
                // P5F.1: Use cached uniform locations (no per-frame driver call)
                self.gl.uniform_matrix_4_f32_slice(
                    self.postprocess_uniforms.projection.as_ref(),
                    false,
                    &pp_proj,
                );
                self.gl.uniform_4_f32(
                    self.postprocess_uniforms.rect.as_ref(),
                    0.0,
                    0.0,
                    self.screen_w as f32,
                    self.screen_h as f32,
                );

                self.gl
                    .uniform_1_i32(self.postprocess_uniforms.texture.as_ref(), 0);
                self.gl.uniform_1_f32(
                    self.postprocess_uniforms.color_temp.as_ref(),
                    self.color_temperature,
                );
                self.gl.uniform_1_f32(
                    self.postprocess_uniforms.saturation.as_ref(),
                    self.saturation,
                );
                self.gl.uniform_1_f32(
                    self.postprocess_uniforms.brightness.as_ref(),
                    self.brightness,
                );
                self.gl
                    .uniform_1_f32(self.postprocess_uniforms.contrast.as_ref(), self.contrast);
                self.gl.uniform_1_i32(
                    self.postprocess_uniforms.invert.as_ref(),
                    if self.invert_colors { 1 } else { 0 },
                );
                self.gl.uniform_1_i32(
                    self.postprocess_uniforms.grayscale.as_ref(),
                    if self.grayscale { 1 } else { 0 },
                );

                // HDR tone mapping uniforms
                self.gl.uniform_1_i32(
                    self.postprocess_uniforms.hdr_enabled.as_ref(),
                    if self.hdr_enabled { 1 } else { 0 },
                );
                self.gl.uniform_1_f32(
                    self.postprocess_uniforms.hdr_peak_nits.as_ref(),
                    self.hdr_peak_nits,
                );
                self.gl.uniform_1_i32(
                    self.postprocess_uniforms.tone_mapping_method.as_ref(),
                    self.tone_mapping_method,
                );
                self.gl
                    .uniform_1_i32(self.postprocess_uniforms.eotf_mode.as_ref(), self.eotf_mode);
                self.gl.uniform_1_i32(
                    self.postprocess_uniforms.output_colorspace.as_ref(),
                    self.output_colorspace,
                );

                // Magnifier uniforms
                self.gl.uniform_1_i32(
                    self.magnifier_uniforms.magnifier_enabled.as_ref(),
                    if self.magnifier_enabled { 1 } else { 0 },
                );
                if self.magnifier_enabled {
                    let cx = self.mouse_x / self.screen_w as f32;
                    let cy = self.mouse_y / self.screen_h as f32;
                    // The fragment shader flips Y (uv.y = 1.0 - v_uv.y) so that
                    // uv.y=1 corresponds to the top of the screen.  Flip cy to match.
                    self.gl.uniform_2_f32(
                        self.magnifier_uniforms.magnifier_center.as_ref(),
                        cx,
                        1.0 - cy,
                    );
                    self.gl.uniform_1_f32(
                        self.magnifier_uniforms.magnifier_radius.as_ref(),
                        self.magnifier_radius / self.screen_w as f32,
                    );
                    self.gl.uniform_1_f32(
                        self.magnifier_uniforms.magnifier_zoom.as_ref(),
                        self.magnifier_zoom,
                    );
                }

                // Slime hand refraction uniforms
                let slime_opacity = self.slime_state.opacity();
                let slime_enabled = self.slime_state.is_visible() && slime_wave.is_some();
                self.gl.uniform_1_i32(
                    self.magnifier_uniforms.slime_enabled.as_ref(),
                    if slime_enabled { 1 } else { 0 },
                );
                self.gl.uniform_1_f32(
                    self.magnifier_uniforms.slime_opacity.as_ref(),
                    slime_opacity,
                );
                if slime_enabled {
                    self.gl.uniform_2_f32_slice(
                        self.magnifier_uniforms.slime_points.as_ref(),
                        self.slime_state.points(),
                    );
                    self.gl.uniform_1_f32_slice(
                        self.magnifier_uniforms.slime_depths.as_ref(),
                        self.slime_state.depths(),
                    );
                    let [min_x, min_y, max_x, max_y] = self.slime_state.bbox();
                    self.gl.uniform_4_f32(
                        self.magnifier_uniforms.slime_bbox.as_ref(),
                        min_x,
                        min_y,
                        max_x,
                        max_y,
                    );
                    let [surface_min_x, surface_min_y, surface_max_x, surface_max_y] =
                        self.slime_state.surface_rect();
                    self.gl.uniform_4_f32(
                        self.magnifier_uniforms.slime_surface_rect.as_ref(),
                        surface_min_x,
                        surface_min_y,
                        surface_max_x,
                        surface_max_y,
                    );
                    self.gl.uniform_2_f32(
                        self.magnifier_uniforms.slime_screen_size.as_ref(),
                        self.screen_w as f32,
                        self.screen_h as f32,
                    );
                    self.gl.uniform_1_f32(
                        self.magnifier_uniforms.slime_scale.as_ref(),
                        self.slime_state.scale(),
                    );
                    self.gl.uniform_1_f32(
                        self.magnifier_uniforms.slime_strength.as_ref(),
                        self.slime_state.strength(),
                    );
                    self.gl.uniform_1_f32(
                        self.magnifier_uniforms.slime_ocean_strength.as_ref(),
                        self.slime_state.ocean_strength(),
                    );
                    self.gl.uniform_1_f32(
                        self.magnifier_uniforms.slime_turbulence_strength.as_ref(),
                        self.slime_state.turbulence_strength(),
                    );
                    self.gl.uniform_1_f32(
                        self.magnifier_uniforms.slime_foam_strength.as_ref(),
                        self.slime_state.foam_strength(),
                    );
                    self.gl.uniform_1_f32(
                        self.magnifier_uniforms.slime_time.as_ref(),
                        self.compositor_start_time.elapsed().as_secs_f32(),
                    );
                    let (wave_texture, wave_width, wave_height) = slime_wave.unwrap();
                    self.gl
                        .uniform_1_i32(self.magnifier_uniforms.slime_wave.as_ref(), 1);
                    self.gl.uniform_2_f32(
                        self.magnifier_uniforms.slime_wave_texel.as_ref(),
                        1.0 / wave_width as f32,
                        1.0 / wave_height as f32,
                    );
                    self.gl.active_texture(glow::TEXTURE1);
                    self.gl.bind_texture(glow::TEXTURE_2D, Some(wave_texture));
                    self.gl.active_texture(glow::TEXTURE0);
                }

                // Colorblind correction uniform
                self.gl.uniform_1_i32(
                    self.magnifier_uniforms.colorblind_mode.as_ref(),
                    self.colorblind_mode,
                );

                self.gl.active_texture(glow::TEXTURE0);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(pp_tex));
                self.gl.bind_vertex_array(Some(self.quad_vao));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                self.gl.bind_vertex_array(None);
                self.gl.use_program(None);
            }
        }

        // Tick tilt after the render loop has set tilt_target from the focused window.
        // If no focused window set tilt_target this frame, it keeps 0 from the reset
        // at the start of the loop (see the tilt branch which sets tilt_target_x/y).
        {
            let dt = self.frame_stats.last_frame_time.elapsed().as_secs_f32();
            let tilt_animating = self.tick_tilt(dt);
            if tilt_animating {
                self.needs_render = true;
            }
        }

        // === Always update frame stats (decoupled from HUD rendering) ===
        {
            let now = std::time::Instant::now();
            let dt = now
                .duration_since(self.frame_stats.last_frame_time)
                .as_secs_f32();
            self.frame_stats.last_frame_time = now;
            self.frame_stats.frame_count += 1;
            self.frame_stats.frame_times.push_back(dt);
            if self.frame_stats.frame_times.len() > 120 {
                self.frame_stats.frame_times.pop_front();
            }
            let elapsed = now
                .duration_since(self.frame_stats.last_fps_update)
                .as_secs_f32();
            if elapsed >= 1.0 {
                self.frame_stats.fps = self.frame_stats.frame_times.len() as f32 / elapsed;
                self.frame_stats.frame_times.clear();
                self.frame_stats.last_fps_update = now;
            }
            self.record_latency_sample();
        }

        // === Pass 5: Debug HUD (feature 11) ===
        if self.debug_hud {
            self.sys_stats.maybe_sample();

            // Format HUD text
            let avg_dt = if self.frame_stats.frame_times.is_empty() {
                0.0
            } else {
                self.frame_stats.frame_times.iter().sum::<f32>()
                    / self.frame_stats.frame_times.len() as f32
            };
            let max_dt = self
                .frame_stats
                .frame_times
                .iter()
                .copied()
                .fold(0.0, f32::max);
            let min_dt = self
                .frame_stats
                .frame_times
                .iter()
                .copied()
                .fold(f32::MAX, f32::min);
            let min_dt = if min_dt == f32::MAX { 0.0 } else { min_dt };

            let mut hud_text = format!(
                "JWM debug HUD (Alt+Shift+F12)\n\
                 Backend: x11\n\
                 FPS: {:.1}  Avg: {:.1}ms  Max: {:.1}ms  Min: {:.1}ms\n\
                 Windows: {}  Tiles: {}  Dirty: {:.0}%\n\
                 Memory: {:.1} MiB RSS\n\
                 CPU: {:.1} %",
                self.frame_stats.fps,
                avg_dt * 1000.0,
                max_dt * 1000.0,
                min_dt * 1000.0,
                self.windows.len(),
                self.damage_tracker.tile_count(),
                self.damage_tracker.dirty_fraction() * 100.0,
                self.sys_stats.rss_mib(),
                self.sys_stats.cpu_pct(),
            );
            if self.debug_hud_extended {
                let tex_mem_kb = self.frame_stats.texture_memory_bytes / 1024;
                let blur_hit_rate =
                    if self.frame_stats.blur_cache_hits + self.frame_stats.blur_cache_misses > 0 {
                        100.0 * self.frame_stats.blur_cache_hits as f32
                            / (self.frame_stats.blur_cache_hits
                                + self.frame_stats.blur_cache_misses)
                                as f32
                    } else {
                        0.0
                    };
                use std::fmt::Write;
                let _ = write!(
                    hud_text,
                    "\nDraw calls: {}  Mem: {}KB\nBlur: {:.0}% hit rate ({}/{})\nQuality: {:?}",
                    self.frame_stats.draw_calls,
                    tex_mem_kb,
                    blur_hit_rate,
                    self.frame_stats.blur_cache_hits,
                    self.frame_stats.blur_cache_misses,
                    self.blur_quality,
                );

                // Add input latency stats if available
                let (avg, p50, p95, p99) = self.compute_latency_stats();
                if avg > 0.0 {
                    let _ = write!(
                        hud_text,
                        "\nLatency: avg {:.1}ms  p50 {:.1}ms  p95 {:.1}ms  p99 {:.1}ms",
                        avg, p50, p95, p99,
                    );
                }

                // Per-zone profiler breakdown
                let zones_map = self.frame_profiler.all_zone_stats();
                if !zones_map.is_empty() {
                    let _ = write!(hud_text, "\n--- Profiler (ms avg/min/max) ---");
                    let mut zones: Vec<_> = zones_map.into_iter().collect();
                    zones.sort_by(|a, b| a.0.cmp(b.0));
                    for (name, zs) in zones {
                        let _ = write!(
                            hud_text,
                            "\n{:<8}: {:>5.2} / {:>5.2} / {:>5.2}",
                            name, zs.avg_ms, zs.min_ms, zs.max_ms,
                        );
                    }
                }
            }

            // Update text texture (skips upload if content unchanged)
            self.update_hud_text_texture(&hud_text);

            // Compute panel dimensions from text texture
            let pad = 8.0f32;
            let text_w = self.hud_text_width as f32;
            let text_h = self.hud_text_height as f32;
            let hud_w = text_w + pad * 2.0;
            let hud_h = text_h + pad * 2.0;
            let hud_x = 10.0f32;
            let hud_y = 10.0f32;

            unsafe {
                // Draw background panel
                self.gl.use_program(Some(self.hud_program));
                self.gl.uniform_matrix_4_f32_slice(
                    self.hud_uniforms.projection.as_ref(),
                    false,
                    &proj,
                );
                self.gl
                    .uniform_4_f32(self.hud_uniforms.bg_color.as_ref(), 0.0, 0.0, 0.0, 0.7);
                self.gl
                    .uniform_4_f32(self.hud_uniforms.fg_color.as_ref(), 0.0, 1.0, 0.0, 1.0);
                self.gl
                    .uniform_2_f32(self.hud_uniforms.size.as_ref(), hud_w, hud_h);
                self.gl
                    .uniform_4_f32(self.hud_uniforms.rect.as_ref(), hud_x, hud_y, hud_w, hud_h);
                self.gl.bind_vertex_array(Some(self.quad_vao));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                // Draw text overlay
                if let Some(tex) = self.hud_text_texture {
                    self.gl.use_program(Some(self.hud_text_program));
                    self.gl.uniform_matrix_4_f32_slice(
                        self.hud_text_uniforms.projection.as_ref(),
                        false,
                        &proj,
                    );
                    self.gl.uniform_4_f32(
                        self.hud_text_uniforms.rect.as_ref(),
                        hud_x + pad,
                        hud_y + pad,
                        text_w,
                        text_h,
                    );
                    self.gl
                        .uniform_1_i32(self.hud_text_uniforms.texture.as_ref(), 0);
                    self.gl.active_texture(glow::TEXTURE0);
                    self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                    self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                }

                self.gl.bind_vertex_array(None);
                self.gl.use_program(None);
            }

            // Log stats periodically
            if self.frame_stats.frame_count % 60 == 0 {
                if self.debug_hud_extended {
                    let tex_mem_kb = self.frame_stats.texture_memory_bytes / 1024;
                    log::info!(
                        "[HUD] FPS: {:.1}, frame_time: {:.2}ms, windows: {}, draw_calls: {}, tex_mem: {}KB, blur_hits: {}, blur_misses: {}",
                        self.frame_stats.fps,
                        avg_dt * 1000.0,
                        self.windows.len(),
                        self.frame_stats.draw_calls,
                        tex_mem_kb,
                        self.frame_stats.blur_cache_hits,
                        self.frame_stats.blur_cache_misses,
                    );
                    self.frame_stats.draw_calls = 0;
                } else {
                    log::info!(
                        "[HUD] FPS: {:.1}, frame_time: {:.2}ms, windows: {}",
                        self.frame_stats.fps,
                        avg_dt * 1000.0,
                        self.windows.len()
                    );
                }
            }
        }

        // === Pass 5b: Screen edge glow ===
        // Tick the countdown so the glow expires even without new mouse events.
        if self.edge_glow {
            self.edge_glow_tick(self.mouse_x, self.mouse_y);
        }
        if self.edge_glow_active && self.edge_glow_width > 0.0 {
            unsafe {
                self.gl.use_program(Some(self.edge_glow_program));
                self.gl.uniform_matrix_4_f32_slice(
                    self.edge_glow_uniforms.projection.as_ref(),
                    false,
                    &proj,
                );
                self.gl.uniform_4_f32(
                    self.edge_glow_uniforms.rect.as_ref(),
                    0.0,
                    0.0,
                    self.screen_w as f32,
                    self.screen_h as f32,
                );
                self.gl.uniform_4_f32(
                    self.edge_glow_uniforms.glow_color.as_ref(),
                    self.edge_glow_color[0],
                    self.edge_glow_color[1],
                    self.edge_glow_color[2],
                    self.edge_glow_color[3],
                );
                self.gl.uniform_1_f32(
                    self.edge_glow_uniforms.glow_width.as_ref(),
                    self.edge_glow_width,
                );
                self.gl.uniform_2_f32(
                    self.edge_glow_uniforms.mouse.as_ref(),
                    self.mouse_x,
                    self.mouse_y,
                );
                self.gl.uniform_2_f32(
                    self.edge_glow_uniforms.screen_size.as_ref(),
                    self.screen_w as f32,
                    self.screen_h as f32,
                );
                self.gl.uniform_1_f32(
                    self.edge_glow_uniforms.time.as_ref(),
                    self.compositor_start_time.elapsed().as_secs_f32(),
                );
                self.gl.bind_vertex_array(Some(self.quad_vao));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                self.gl.bind_vertex_array(None);
                self.gl.use_program(None);
            }
        }

        // === Pass 5c: Particle effects ===
        if !self.particle_systems.is_empty() {
            self.tick_particles();
            self.render_particles(&proj);
        }

        // === Pass 5d: Overview overlay ===
        if self.overview_active {
            self.tick_overview_prism();
            self.render_overview(&proj, focused);
        }

        // === Pass 5f: Expose/Mission Control overlay ===
        if !self.expose_entries.is_empty() {
            self.render_expose(&proj);
        }

        // A lock screen is sensitive content: remote/IPC captures must see the
        // opaque lock UI, never the client scene underneath it.
        if self
            .system_ui
            .as_ref()
            .is_some_and(|overlay| overlay.locked)
        {
            self.render_system_ui(&proj);
        }

        // === Feature 12: Screenshot capture (after all rendering, before overlays) ===
        // Capture BEFORE rendering snap preview / annotations so the screenshot
        // doesn't include the selection overlay or annotation strokes.
        let has_pending_screenshot = self.screenshot_requests.has_pending();
        for request in self.screenshot_requests.take_all() {
            match request {
                crate::backend::compositor_common::screenshot::ScreenshotRequest::Full(path) => {
                    self.capture_screenshot(&path);
                }
                crate::backend::compositor_common::screenshot::ScreenshotRequest::Region {
                    path,
                    x,
                    y,
                    width,
                    height,
                } => {
                    self.capture_screenshot_region(&path, x, y, width, height);
                }
            }
        }

        // === Pass 5g: Snap preview ===
        // Skip on the frame that captured a screenshot (overlay was already cleared
        // logically; rendering it would leave a ghost on the next visible frame).
        if !has_pending_screenshot {
            self.render_snap_preview(&proj);
        }

        // === Pass 5e: Annotations overlay ===
        if self.annotation_active && !self.annotation_strokes.is_empty() {
            self.render_annotations(&proj);
        }

        // === Tag-switch transition overlay ===
        let transition_still_active = if let Some(progress) =
            self.transition_progress(std::time::Instant::now())
        {
            // Monitor-local geometry for the transition
            let mon_x = self.transition_mon_x;
            let mon_y = self.transition_mon_y;
            let mon_w = self.transition_mon_w;
            let mon_h = self.transition_mon_h;
            let exclude_top = self.transition_exclude_top.min(mon_h);
            let draw_y = (mon_y as u32 + exclude_top) as f32; // Y in screen coords
            let draw_h = (mon_h - exclude_top) as f32;
            let draw_x = mon_x as f32;
            let top_frac = if mon_h == 0 {
                0.0
            } else {
                exclude_top as f32 / mon_h as f32
            };
            // OpenGL scissor Y is flipped
            let scissor_gl_y = self.screen_h as i32 - (mon_y + mon_h as i32);

            match self.transition_mode {
                TransitionMode::None => {}
                TransitionMode::Slide => {
                    // --- Slide mode: old scene slides out + fades ---
                    // New scene is already in the back-buffer at final position.
                    // Old snapshot slides in transition_direction while fading out,
                    // giving the effect of current windows sliding away to reveal
                    // the target windows underneath.
                    if let Some((_, snap_tex)) = &self.transition_fbo {
                        let snap_tex = *snap_tex;

                        // Slide offset: old scene moves in the transition direction
                        let slide_offset = progress * self.transition_direction * mon_w as f32;

                        // Fade out smoothly over the full duration
                        let fade_opacity = (1.0 - progress).max(0.0);

                        unsafe {
                            if draw_h > 0.0 && fade_opacity > 0.0 {
                                self.gl.enable(glow::SCISSOR_TEST);
                                self.gl.scissor(
                                    mon_x,
                                    scissor_gl_y,
                                    mon_w as i32,
                                    (mon_h - exclude_top) as i32,
                                );

                                self.gl
                                    .blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);

                                self.gl.use_program(Some(self.transition_program));
                                self.gl.uniform_matrix_4_f32_slice(
                                    self.transition_uniforms.projection.as_ref(),
                                    false,
                                    &proj,
                                );
                                self.gl
                                    .uniform_1_i32(self.transition_uniforms.texture.as_ref(), 0);
                                self.gl.active_texture(glow::TEXTURE0);

                                let uv = [0.0f32, 0.0, 1.0, 1.0 - top_frac];

                                self.gl.uniform_4_f32(
                                    self.transition_uniforms.rect.as_ref(),
                                    draw_x + slide_offset,
                                    draw_y,
                                    mon_w as f32,
                                    draw_h,
                                );
                                self.gl.uniform_1_f32(
                                    self.transition_uniforms.opacity.as_ref(),
                                    fade_opacity,
                                );
                                self.gl.uniform_4_f32(
                                    self.transition_uniforms.uv_rect.as_ref(),
                                    uv[0],
                                    uv[1],
                                    uv[2],
                                    uv[3],
                                );
                                self.gl.bind_texture(glow::TEXTURE_2D, Some(snap_tex));
                                self.gl.bind_vertex_array(Some(self.quad_vao));
                                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                                self.gl.bind_vertex_array(None);
                                self.gl.use_program(None);

                                self.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
                                self.gl.disable(glow::SCISSOR_TEST);
                            }
                        }
                    }
                }
                TransitionMode::Cube => {
                    // --- Cube mode: 3D rotating cube transition ---
                    self.render_cube_transition(progress, &proj);
                }
                TransitionMode::Fade => {
                    // --- Fade mode: old scene fades out, new scene fades in ---
                    if let Some((_, snap_tex)) = &self.transition_fbo {
                        let snap_tex = *snap_tex;
                        let fade_opacity = (1.0 - progress).max(0.0);
                        unsafe {
                            if draw_h > 0.0 && fade_opacity > 0.0 {
                                self.gl.enable(glow::SCISSOR_TEST);
                                self.gl.scissor(
                                    mon_x,
                                    scissor_gl_y,
                                    mon_w as i32,
                                    (mon_h - exclude_top) as i32,
                                );
                                self.gl
                                    .blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
                                self.gl.use_program(Some(self.transition_program));
                                self.gl.uniform_matrix_4_f32_slice(
                                    self.transition_uniforms.projection.as_ref(),
                                    false,
                                    &proj,
                                );
                                self.gl
                                    .uniform_1_i32(self.transition_uniforms.texture.as_ref(), 0);
                                self.gl.active_texture(glow::TEXTURE0);
                                let uv = [0.0f32, 0.0, 1.0, 1.0 - top_frac];
                                self.gl.uniform_4_f32(
                                    self.transition_uniforms.rect.as_ref(),
                                    draw_x,
                                    draw_y,
                                    mon_w as f32,
                                    draw_h,
                                );
                                self.gl.uniform_1_f32(
                                    self.transition_uniforms.opacity.as_ref(),
                                    fade_opacity,
                                );
                                self.gl.uniform_4_f32(
                                    self.transition_uniforms.uv_rect.as_ref(),
                                    uv[0],
                                    uv[1],
                                    uv[2],
                                    uv[3],
                                );
                                self.gl.bind_texture(glow::TEXTURE_2D, Some(snap_tex));
                                self.gl.bind_vertex_array(Some(self.quad_vao));
                                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                                self.gl.bind_vertex_array(None);
                                self.gl.use_program(None);
                                self.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
                                self.gl.disable(glow::SCISSOR_TEST);
                            }
                        }
                    }
                }
                TransitionMode::Flip => {
                    // --- Flip mode: card-flip around Y axis ---
                    self.render_flip_transition(progress, &proj);
                }
                TransitionMode::Zoom => {
                    // --- Zoom mode: old scene shrinks + fades, new scene grows in ---
                    if let Some((_, snap_tex)) = &self.transition_fbo {
                        let snap_tex = *snap_tex;
                        let fade_opacity = (1.0 - progress).max(0.0);
                        // Old scene shrinks toward center
                        let scale = 1.0 - progress * 0.5; // 1.0 → 0.5
                        let scaled_w = mon_w as f32 * scale;
                        let scaled_h = draw_h * scale;
                        let offset_x = draw_x + (mon_w as f32 - scaled_w) * 0.5;
                        let offset_y = draw_y + (draw_h - scaled_h) * 0.5;
                        unsafe {
                            if draw_h > 0.0 && fade_opacity > 0.0 {
                                self.gl.enable(glow::SCISSOR_TEST);
                                self.gl.scissor(
                                    mon_x,
                                    scissor_gl_y,
                                    mon_w as i32,
                                    (mon_h - exclude_top) as i32,
                                );
                                self.gl
                                    .blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
                                self.gl.use_program(Some(self.transition_program));
                                self.gl.uniform_matrix_4_f32_slice(
                                    self.transition_uniforms.projection.as_ref(),
                                    false,
                                    &proj,
                                );
                                self.gl
                                    .uniform_1_i32(self.transition_uniforms.texture.as_ref(), 0);
                                self.gl.active_texture(glow::TEXTURE0);
                                let uv = [0.0f32, 0.0, 1.0, 1.0 - top_frac];
                                self.gl.uniform_4_f32(
                                    self.transition_uniforms.rect.as_ref(),
                                    offset_x,
                                    offset_y,
                                    scaled_w,
                                    scaled_h,
                                );
                                self.gl.uniform_1_f32(
                                    self.transition_uniforms.opacity.as_ref(),
                                    fade_opacity,
                                );
                                self.gl.uniform_4_f32(
                                    self.transition_uniforms.uv_rect.as_ref(),
                                    uv[0],
                                    uv[1],
                                    uv[2],
                                    uv[3],
                                );
                                self.gl.bind_texture(glow::TEXTURE_2D, Some(snap_tex));
                                self.gl.bind_vertex_array(Some(self.quad_vao));
                                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                                self.gl.bind_vertex_array(None);
                                self.gl.use_program(None);
                                self.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
                                self.gl.disable(glow::SCISSOR_TEST);
                            }
                        }
                    }
                }
                TransitionMode::Stack => {
                    // --- Stack mode: new scene slides over old with depth effect ---
                    if let Some((_, snap_tex)) = &self.transition_fbo {
                        let snap_tex = *snap_tex;
                        // Old scene stays in place but darkens and scales down slightly
                        let dim = 1.0 - progress * 0.3; // 1.0 → 0.7
                        let old_scale = 1.0 - progress * 0.05; // 1.0 → 0.95
                        let old_w = mon_w as f32 * old_scale;
                        let old_h = draw_h * old_scale;
                        let old_x = draw_x + (mon_w as f32 - old_w) * 0.5;
                        let old_y = draw_y + (draw_h - old_h) * 0.5;
                        unsafe {
                            if draw_h > 0.0 {
                                self.gl.enable(glow::SCISSOR_TEST);
                                self.gl.scissor(
                                    mon_x,
                                    scissor_gl_y,
                                    mon_w as i32,
                                    (mon_h - exclude_top) as i32,
                                );

                                // First: clear workspace area and redraw wallpaper behind
                                self.gl.clear_color(0.0, 0.0, 0.0, 1.0);
                                self.gl.clear(glow::COLOR_BUFFER_BIT);
                                self.gl
                                    .viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
                                self.draw_wallpaper_in_region(&proj, mon_x, mon_y, mon_w, mon_h);

                                // Draw dimmed/scaled old scene
                                self.gl
                                    .blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
                                self.gl.use_program(Some(self.transition_program));
                                self.gl.uniform_matrix_4_f32_slice(
                                    self.transition_uniforms.projection.as_ref(),
                                    false,
                                    &proj,
                                );
                                self.gl
                                    .uniform_1_i32(self.transition_uniforms.texture.as_ref(), 0);
                                self.gl.active_texture(glow::TEXTURE0);
                                let uv = [0.0f32, 0.0, 1.0, 1.0 - top_frac];
                                self.gl.uniform_4_f32(
                                    self.transition_uniforms.rect.as_ref(),
                                    old_x,
                                    old_y,
                                    old_w,
                                    old_h,
                                );
                                self.gl
                                    .uniform_1_f32(self.transition_uniforms.opacity.as_ref(), dim);
                                self.gl.uniform_4_f32(
                                    self.transition_uniforms.uv_rect.as_ref(),
                                    uv[0],
                                    uv[1],
                                    uv[2],
                                    uv[3],
                                );
                                self.gl.bind_texture(glow::TEXTURE_2D, Some(snap_tex));
                                self.gl.bind_vertex_array(Some(self.quad_vao));
                                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                                // Draw new scene sliding in from the transition direction
                                // New scene is already rendered in the back-buffer; we blit
                                // from transition_new_fbo if available, otherwise approximate
                                // by drawing the back-buffer content as a sliding overlay.
                                // For Stack, capture new scene like cube does.
                                if self.transition_new_fbo.is_none() {
                                    self.transition_new_fbo =
                                        Self::create_scene_fbo(&self.gl, mon_w, mon_h).ok();
                                }
                                if let Some((new_fbo, new_tex)) = &self.transition_new_fbo {
                                    let new_fbo = *new_fbo;
                                    let new_tex = *new_tex;
                                    self.capture_transition_scene(
                                        new_fbo, mon_x, mon_y, mon_w, mon_h,
                                    );

                                    // New scene slides in from the side
                                    let new_slide =
                                        (1.0 - progress) * self.transition_direction * mon_w as f32;
                                    self.gl.uniform_4_f32(
                                        self.transition_uniforms.rect.as_ref(),
                                        draw_x + new_slide,
                                        draw_y,
                                        mon_w as f32,
                                        draw_h,
                                    );
                                    self.gl.uniform_1_f32(
                                        self.transition_uniforms.opacity.as_ref(),
                                        1.0,
                                    );
                                    self.gl.uniform_4_f32(
                                        self.transition_uniforms.uv_rect.as_ref(),
                                        uv[0],
                                        uv[1],
                                        uv[2],
                                        uv[3],
                                    );
                                    self.gl.bind_texture(glow::TEXTURE_2D, Some(new_tex));
                                    self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                                }

                                self.gl.bind_vertex_array(None);
                                self.gl.use_program(None);
                                self.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
                                self.gl.disable(glow::SCISSOR_TEST);
                            }
                        }
                    }
                }
                TransitionMode::Blinds => {
                    // --- Blinds mode: vertical strips flip to reveal new scene ---
                    self.render_blinds_transition(progress, &proj);
                }
                TransitionMode::CoverFlow => {
                    self.render_coverflow_transition(progress, &proj);
                }
                TransitionMode::Helix => {
                    self.render_helix_transition(progress, &proj);
                }
                TransitionMode::Portal => {
                    self.render_portal_transition(progress, &proj);
                }
            }
            true
        } else {
            // Transition finished — clean up
            if self.transition_start.is_some() {
                self.transition_start = None;
                // Release the monitor-sized snapshot FBOs/textures instead of
                // letting them sit idle in VRAM until the next transition (or
                // Drop) reclaims them.
                if let Some((fbo, tex)) = self.transition_fbo.take() {
                    unsafe {
                        self.gl.delete_framebuffer(fbo);
                        self.gl.delete_texture(tex);
                    }
                }
                if let Some((fbo, tex)) = self.transition_new_fbo.take() {
                    unsafe {
                        self.gl.delete_framebuffer(fbo);
                        self.gl.delete_texture(tex);
                    }
                }
                log::debug!("compositor: tag-switch transition completed");
            }
            false
        };

        // System UI is always the final visual layer, above transitions and clients.
        if self.system_ui.is_some() {
            self.render_system_ui(&proj);
        }

        // Capture before swapping: the GLX back buffer's contents are no
        // longer defined after SwapBuffers, which caused intermittent black or
        // corrupted frames in both X11RB and XCB backends.
        if self.recording_active {
            self.capture_recording_frame();
        }

        // Swap buffers (double-buffered with vsync for tear-free output).
        // VRR (Variable Refresh Rate) is automatically handled by the driver when using Present.
        match self.vsync_method {
            VsyncMethod::OmlSyncControl => {
                // Use GLX_OML_sync_control for per-window MSC-based timing
                if let Some(oml) = &self.oml {
                    // For now, use global swap (future: per-window MSC-based timing)
                    // VRR target is available via get_vrr_refresh_rate() if needed
                    if let Some(_sbc) = oml.swap_buffers_msc(0) {
                        // Successfully used OML swap
                    } else {
                        // Fall back to traditional swap
                        unsafe {
                            x11::glx::glXSwapBuffers(self.xlib_display, self.glx_drawable);
                        }
                    }
                } else {
                    // OML not available, fall back
                    unsafe {
                        x11::glx::glXSwapBuffers(self.xlib_display, self.glx_drawable);
                    }
                }
            }
            VsyncMethod::Present => {
                // Present extension with VRR support
                // When VRR is active for a game window, the driver automatically uses
                // adaptive refresh rates via the Present extension capabilities.
                if self.vrr_active {
                    log::debug!(
                        "compositor: rendering frame with VRR active (target: {} Hz)",
                        self.get_vrr_refresh_rate()
                    );
                }
                unsafe {
                    x11::glx::glXSwapBuffers(self.xlib_display, self.glx_drawable);
                }
            }
            VsyncMethod::Global => {
                // Traditional global vsync (all windows locked to 60Hz)
                unsafe {
                    x11::glx::glXSwapBuffers(self.xlib_display, self.glx_drawable);
                }
            }
        }

        // Schedule re-render if fades or transition are still in progress
        if fades_active
            || transition_still_active
            || wobbly_active
            || !self.particle_systems.is_empty()
            || self.overview_active
            || genie_active
            || ripples_active
            || focus_highlight_active
            || wallpaper_crossfade_active
            || expose_animating
            || snap_animating
            || peek_animating
            || self.expose_active
        {
            self.needs_render = true;
        }

        // Schedule re-render if recording is active (need continuous frames)
        if self.recording_active {
            self.needs_render = true;
        }

        // Animate zoom-to-fit scale
        if (self.zoom_to_fit_scale - self.zoom_to_fit_target).abs() > 0.001 {
            self.zoom_to_fit_scale += (self.zoom_to_fit_target - self.zoom_to_fit_scale) * 0.15;
            if (self.zoom_to_fit_scale - self.zoom_to_fit_target).abs() < 0.001 {
                self.zoom_to_fit_scale = self.zoom_to_fit_target;
            }
            self.needs_render = true;
        }

        // P4: Record temporal blur statistics (if blur happened)
        if self.temporal_blur_enabled && current_window_hash != 0 {
            self.temporal_blur_total_count += 1;
            if current_window_hash == self.prev_window_positions_hash {
                self.temporal_blur_reuse_count += 1;
            }
            self.prev_window_positions_hash = current_window_hash;
        }

        // P4: Finalize temporal blur state for next frame
        self.finalize_temporal_blur();

        // Phase 2: End frame profiling
        let frame_time_ms = self.frame_profiler.end_frame();

        // Benchmark: record frame data
        if self.benchmark.is_running() {
            let frame_us = bench_frame_start.elapsed().as_micros() as u64;
            self.benchmark.record_frame(frame_us);

            // Feed latest input latency
            if let Some(&last_latency) = self.frame_stats.latency_samples.back() {
                self.benchmark.record_input_latency(last_latency);
            }

            // Feed zone stats from profiler
            for (zone, zs) in self.frame_profiler.all_zone_stats() {
                self.benchmark.record_zone(zone, zs.avg_ms);
            }

            // Feed GL stats
            self.benchmark.record_gl_stats(
                self.frame_stats.draw_calls,
                0, // state changes tracked elsewhere
                0, // texture binds tracked elsewhere
            );

            // Feed blur cache stats
            self.benchmark.blur_cache_hits = self.frame_stats.blur_cache_hits;
            self.benchmark.blur_cache_misses = self.frame_stats.blur_cache_misses;
        }

        // Log profiler stats every 300 frames (~5s at 60fps)
        if self.frame_stats.frame_count % 300 == 0 && self.frame_profiler.is_enabled() {
            let stats = self.frame_profiler.all_zone_stats();
            if !stats.is_empty() {
                log::info!("[profiler] Frame time: {:.2}ms", frame_time_ms);
                for (zone, zs) in stats {
                    log::info!(
                        "[profiler]   {}: avg={:.2}ms min={:.2}ms max={:.2}ms",
                        zone,
                        zs.avg_ms,
                        zs.min_ms,
                        zs.max_ms
                    );
                }
            }
        }

        // Return the per-frame scratch buffers to their fields for reuse.
        self.scratch_blur_dirty = blur_dirty_wins;
        self.scratch_tfp_order = tfp_order;

        true
    }

    // =====================================================================
    // New feature methods
    // =====================================================================
}
