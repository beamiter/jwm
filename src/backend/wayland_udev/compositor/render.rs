// render_frame and rendering helpers for the Wayland udev compositor
#[allow(unused_imports)]
use super::*;
use smithay::backend::renderer::gles::ffi;

impl WaylandCompositor {
    // =========================================================================
    // Helper: draw a fullscreen quad (uses gl_VertexID in the vertex shader)
    // =========================================================================

    #[allow(dead_code)]
    fn draw_quad(&self, gl: &ffi::Gles2) {
        unsafe {
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
        }
    }

    pub(super) fn bind_window_texture(&self, gl: &ffi::Gles2, texture: u32) {
        unsafe {
            gl.BindTexture(ffi::TEXTURE_2D, texture);
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, ffi::LINEAR as i32);
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, ffi::LINEAR as i32);
            gl.TexParameteri(
                ffi::TEXTURE_2D,
                ffi::TEXTURE_WRAP_S,
                ffi::CLAMP_TO_EDGE as i32,
            );
            gl.TexParameteri(
                ffi::TEXTURE_2D,
                ffi::TEXTURE_WRAP_T,
                ffi::CLAMP_TO_EDGE as i32,
            );
        }
    }

    // =========================================================================
    // Helper: set a vec4 uniform (u_rect, etc.)
    // =========================================================================

    fn set_rect_uniform(&self, gl: &ffi::Gles2, loc: i32, x: f32, y: f32, w: f32, h: f32) {
        unsafe {
            gl.Uniform4f(loc, x, y, w, h);
        }
    }

    // =========================================================================
    // Helper: set a mat4 uniform (u_projection, etc.)
    // =========================================================================

    fn set_projection_uniform(&self, gl: &ffi::Gles2, loc: i32, proj: &[f32; 16]) {
        unsafe {
            gl.UniformMatrix4fv(loc, 1, ffi::FALSE as u8, proj.as_ptr());
        }
    }

    // =========================================================================
    // Helper: blit one FBO into another
    // =========================================================================

    fn blit_fbo(&self, gl: &ffi::Gles2, src_fbo: u32, dst_fbo: u32, w: u32, h: u32) {
        unsafe {
            gl.BindFramebuffer(ffi::READ_FRAMEBUFFER, src_fbo);
            gl.BindFramebuffer(ffi::DRAW_FRAMEBUFFER, dst_fbo);
            gl.BlitFramebuffer(
                0,
                0,
                w as i32,
                h as i32,
                0,
                0,
                w as i32,
                h as i32,
                ffi::COLOR_BUFFER_BIT,
                ffi::NEAREST,
            );
        }
    }

    /// Bounding box (top-left logical px) of everything that changed since the
    /// previous frame, or `None` to request a full redraw.
    ///
    /// SAFETY INVARIANT: the returned box must be a *superset* of every pixel of
    /// `output_fbo` that differs from the previous frame. Pixels outside it are
    /// left persisted from prior frames, so under-reporting shows stale content.
    /// Callers only invoke this on provably "calm" frames (no animation, blur,
    /// or effect overlays); here we additionally cover window geometry changes,
    /// content updates, and focus-driven border/opacity changes.
    fn compute_partial_damage_box(
        &self,
        scene: &[(u64, i32, i32, u32, u32)],
        focused: Option<u64>,
    ) -> Option<dirty_region::DirtyRect> {
        use dirty_region::DirtyRect;

        // Expand each window rect to cover its border and shadow footprint.
        let margin = self.border_width
            + if self.shadow_enabled && self.shadow_radius > 0.0 {
                self.shadow_spread
                    + self.shadow_radius
                    + self.shadow_offset[0].abs().max(self.shadow_offset[1].abs())
            } else {
                0.0
            };

        fn fold(acc: &mut Option<DirtyRect>, r: DirtyRect) {
            *acc = Some(match *acc {
                Some(a) => a.union(&r),
                None => r,
            });
        }
        let win_rect = |id: u64| -> Option<DirtyRect> {
            scene.iter().find(|&&(wid, ..)| wid == id).map(|&(_, x, y, w, h)| {
                DirtyRect::new(x as f32, y as f32, w as f32, h as f32).expand(margin)
            })
        };

        let mut acc: Option<DirtyRect> = None;

        // Geometry changes (appear/disappear/move/resize), already tracked.
        for r in self.dirty_region_tracker.regions() {
            fold(&mut acc, r.expand(margin));
        }
        // Window content updates committed this frame.
        for &id in &self.content_dirty_ids {
            if let Some(r) = win_rect(id) {
                fold(&mut acc, r);
            }
        }
        // Focus change: border/opacity/dim differ on old and new focused windows.
        if focused != self.prev_focused {
            for fid in [focused, self.prev_focused].into_iter().flatten() {
                if let Some(r) = win_rect(fid) {
                    fold(&mut acc, r);
                }
            }
        }
        // Urgent windows draw an attention border that may toggle independently
        // of content; keep them in the box so it never goes stale.
        for &(id, x, y, w, h) in scene {
            if self.windows.get(&id).map_or(false, |ws| ws.is_urgent) {
                fold(
                    &mut acc,
                    DirtyRect::new(x as f32, y as f32, w as f32, h as f32).expand(margin),
                );
            }
        }

        let bbox = acc?;
        // Clamp to screen bounds.
        let x0 = bbox.x.max(0.0);
        let y0 = bbox.y.max(0.0);
        let x1 = (bbox.x + bbox.width).min(self.screen_w as f32);
        let y1 = (bbox.y + bbox.height).min(self.screen_h as f32);
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        let clamped = DirtyRect::new(x0, y0, x1 - x0, y1 - y0);
        // Scissoring a near-full-screen box is not worth the bookkeeping.
        let screen_area = (self.screen_w as f32) * (self.screen_h as f32);
        if clamped.area() >= 0.7 * screen_area {
            return None;
        }
        Some(clamped)
    }

    /// Main rendering function. Composites the entire scene into the output FBO.
    /// `scene` is a list of (window_id, x, y, w, h) in bottom-to-top order.
    /// `focused` is the currently focused window.
    /// Returns true if a frame was rendered (false if skipped due to no changes).
    pub(crate) fn render_frame(
        &mut self,
        gl: &ffi::Gles2,
        scene: &[(u64, i32, i32, u32, u32)],
        focused: Option<u64>,
    ) -> bool {
        // =================================================================
        // 0. Performance infrastructure - frame start
        // =================================================================
        self.frame_profiler.begin_frame();
        self.gl_state_tracker.reset();

        // GPU fence sync: poll pending fences, cleanup old ones
        unsafe {
            self.gpu_fence_sync_mgr.update_fence_states(gl);
            self.gpu_fence_sync_mgr.cleanup_old_fences(gl);
        }

        // Power saving: periodic update (every 5s)
        if self.power_saving_mgr.update() {
            let recs = self.power_saving_mgr.get_recommendations();
            self.adaptive_frame_rate.limiter_mut().set_target_fps(recs.fps_limit);
        }

        // Shader hot-reload: check for modified shader files
        let reloaded_shaders = self.shader_hot_reload.poll();
        if !reloaded_shaders.is_empty() {
            log::info!("[compositor] Shader hot-reload: {} shaders changed", reloaded_shaders.len());
        }

        // Direct scanout: check if we can bypass composition entirely
        if !self.transition_active && !self.overview_active && !self.expose_active && !self.postprocess_active {
            // Reuse a persistent scratch Vec (taken out so we can borrow other
            // self fields while filling it) instead of allocating every frame.
            let mut scanout_windows = std::mem::take(&mut self.scratch_scanout);
            scanout_windows.clear();
            for &(win_id, x, y, w, h) in scene {
                if let Some(ws) = self.windows.get(&win_id) {
                    scanout_windows.push((win_id, direct_scanout::WindowScanoutInfo {
                        x, y, width: w, height: h,
                        is_fullscreen: ws.is_fullscreen,
                        has_alpha: ws.has_alpha,
                        has_blur: ws.is_frosted,
                        has_shadow: self.shadow_enabled,
                        corner_radius: ws.corner_radius_override.unwrap_or(self.corner_radius),
                        opacity: ws.fade_opacity,
                    }));
                }
            }
            let (can_scanout, _scanout_win) = self.direct_scanout_mgr.check_scene(&scanout_windows, focused);
            self.scratch_scanout = scanout_windows;
            if can_scanout && self.fullscreen_unredirect {
                self.frame_profiler.end_frame();
                self.frame_rate_limiter.mark_frame();
                return true;
            }
        }

        // =================================================================
        // 1. Frame timing
        // =================================================================
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame_time).as_secs_f32();
        self.last_frame_time = now;

        // Update FPS counter and perf metrics
        self.frame_count += 1;
        if self.frame_count % 60 == 0 {
            self.fps = if dt > 0.0 { 1.0 / dt } else { 0.0 };
        }
        self.perf_metrics.record_frame(std::time::Duration::from_secs_f32(dt));

        // =================================================================
        // 1b. Dirty region tracking: compare current scene vs previous frame
        // =================================================================
        {
            // Reuse persistent scratch buffers: current-frame id set + previous
            // geometry-by-id map. Avoids two per-frame HashSet allocations and
            // turns the move/resize lookup from O(N^2) linear scan into O(N).
            self.scratch_curr_ids.clear();
            self.scratch_curr_ids.extend(scene.iter().map(|&(id, _, _, _, _)| id));

            self.scratch_prev_geom.clear();
            for &(id, x, y, w, h) in &self.prev_scene {
                self.scratch_prev_geom.insert(id, (x, y, w, h));
            }

            // Windows that disappeared — mark their old rect dirty
            for &(id, x, y, w, h) in &self.prev_scene {
                if !self.scratch_curr_ids.contains(&id) {
                    self.dirty_region_tracker.mark_dirty(
                        dirty_region::DirtyRect::new(x as f32, y as f32, w as f32, h as f32),
                    );
                }
            }

            // Windows that appeared or moved/resized
            for &(id, x, y, w, h) in scene {
                match self.scratch_prev_geom.get(&id) {
                    None => {
                        // New window
                        self.dirty_region_tracker.mark_dirty(
                            dirty_region::DirtyRect::new(x as f32, y as f32, w as f32, h as f32),
                        );
                    }
                    Some(&(px, py, pw, ph)) => {
                        if x != px || y != py || w != pw || h != ph {
                            // Moved or resized — mark both old and new rects
                            self.dirty_region_tracker.mark_dirty(
                                dirty_region::DirtyRect::new(px as f32, py as f32, pw as f32, ph as f32),
                            );
                            self.dirty_region_tracker.mark_dirty(
                                dirty_region::DirtyRect::new(x as f32, y as f32, w as f32, h as f32),
                            );
                        }
                    }
                }
            }

            self.prev_scene.clear();
            self.prev_scene.extend_from_slice(scene);
        }

        // Feed dirty regions to per-monitor renderer
        {
            // Borrow the tracker's deque directly instead of collecting into a
            // fresh Vec every frame. VecDeque exposes its (up to two) contiguous
            // slices; marking from each is equivalent to one combined call.
            let regions = self.dirty_region_tracker.regions();
            if regions.is_empty() {
                // No tracked dirty regions yet — mark all monitors dirty (full redraw)
                self.per_monitor_renderer.mark_all_dirty();
            } else {
                let (front, back) = regions.as_slices();
                self.per_monitor_renderer.mark_dirty_from_regions(front);
                if !back.is_empty() {
                    self.per_monitor_renderer.mark_dirty_from_regions(back);
                }
            }
            self.per_monitor_renderer.next_frame();
        }

        // =================================================================
        // 2. Animation ticks
        // =================================================================
        self.tick_fades(dt);
        self.tick_genie();
        self.tick_wobbly(dt);
        self.tick_particles(dt);
        self.tick_snap_preview(dt);
        self.tick_overview(dt);
        self.tick_overview_prism(dt);
        self.tick_tilt(dt);
        self.tick_expose(dt);

        // Focus highlight: arm a one-shot pulse on the new focus.
        // Done before any_animating so the highlight keeps the loop ticking
        // until the duration expires, instead of stalling on the first frame.
        if self.focus_highlight_enabled && focused != self.prev_focused {
            if let Some(fw) = focused {
                self.focus_highlight_start = Some((fw, Instant::now()));
            }
        }
        let focus_highlight_active = self.focus_highlight_enabled
            && self
                .focus_highlight_start
                .map(|(_, start)| {
                    (start.elapsed().as_millis() as u64) < self.focus_highlight_duration_ms
                })
                .unwrap_or(false);
        // Motion trail keeps the loop ticking until trails drain to empty,
        // even if the user has stopped moving the window.
        let motion_trail_active = self.motion_trail_enabled
            && self.windows.values().any(|w| !w.motion_trail.is_empty());

        // Determine if anything needs rendering
        let any_animating = self.has_active_animations()
            || self.transition_active
            || self.expose_active
            || !self.expose_entries.is_empty()
            || self.overview_active
            || focus_highlight_active
            || motion_trail_active
            || !self.genie_active.is_empty();

        let force_render = any_animating
            || self.postprocess_active
            || self.debug_hud_enabled
            || (self.edge_glow_enabled && self.edge_glow_active);

        // Check if any window texture has been updated
        let has_dirty = scene.iter().any(|&(win_id, _, _, _, _)| {
            self.windows
                .get(&win_id)
                .map_or(false, |ws| ws.gl_texture.is_some() && ws.fade_opacity > 0.0)
        });

        // Skip frame if nothing changed
        if !self.needs_render && !force_render && !has_dirty {
            return false;
        }
        // If animations are still running, keep the flag set so the next
        // tick_animations call re-invokes compositor_render_frame automatically.
        self.needs_render = any_animating || self.has_active_animations();

        // Rate-limited diagnostic logging (once per second when scene is non-empty)
        static LAST_RF_LOG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let rf_log_this = !scene.is_empty() && {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let prev = LAST_RF_LOG.load(std::sync::atomic::Ordering::Relaxed);
            if now > prev {
                LAST_RF_LOG.store(now, std::sync::atomic::Ordering::Relaxed);
                true
            } else {
                false
            }
        };
        if rf_log_this {
            log::info!(
                "[rf] windows={} scene={} force={force_render} dirty={has_dirty}",
                self.windows.len(),
                scene.len()
            );
            for &(win_id, x, y, w, h) in scene {
                if let Some(ws) = self.windows.get(&win_id) {
                    log::info!("[rf] win={win_id:#x} tex={:?} fade={:.3} pos=({x},{y}) size={w}x{h} y_inv={}",
                        ws.gl_texture, ws.fade_opacity, ws.y_inverted);
                } else {
                    log::info!(
                        "[rf] win={win_id:#x} NOT in compositor.windows pos=({x},{y}) size={w}x{h}"
                    );
                }
            }
        }

        // =================================================================
        // 2b. Partial-damage decision (experimental, default off)
        // =================================================================
        // Only scissor on provably "calm" frames: no animation, no blur, no
        // effect overlays, no tilt. Everything excluded here either redraws the
        // whole screen continuously or samples regions outside any damage box.
        let blur_would_run = self.blur_enabled
            && scene.iter().any(|&(win_id, ..)| {
                self.windows.get(&win_id).map_or(false, |ws| ws.is_frosted)
            });
        let allow_partial = self.partial_damage_enabled
            && !self.force_full_damage_next
            && !any_animating
            && !force_render
            && !self.peek_active
            && (!self.window_tabs_enabled || self.window_groups.is_empty())
            && !self.annotation_active
            && self.tilt_x.abs() <= 0.001
            && self.tilt_y.abs() <= 0.001
            && !blur_would_run;
        let partial_box = if allow_partial {
            self.compute_partial_damage_box(scene, focused)
        } else {
            None
        };
        // Consumed for this frame; next frame may go partial again.
        self.force_full_damage_next = false;

        // =================================================================
        // 3. Setup projection matrix
        // =================================================================
        let projection = ortho(0.0, self.screen_w as f32, self.screen_h as f32, 0.0);

        // =================================================================
        // 4. Bind output FBO and clear
        // =================================================================
        unsafe {
            gl.BindFramebuffer(ffi::FRAMEBUFFER, self.output_fbo);
            gl.Viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
            gl.Enable(ffi::BLEND);
            gl.BlendFunc(ffi::ONE, ffi::ONE_MINUS_SRC_ALPHA);
        }

        // Restrict all output_fbo passes (clear, wallpaper, shadows, windows,
        // borders) to the damage box. Regions outside persist from prior frames.
        // GL scissor uses a bottom-left origin; our draw coords are top-left.
        let scissor_active = if let Some(b) = partial_box {
            let sx = b.x.floor().max(0.0) as i32;
            let sw = b.width.ceil() as i32;
            let sh = b.height.ceil() as i32;
            let sy = ((self.screen_h as i32) - (b.y.floor() as i32) - sh).max(0);
            unsafe {
                gl.Enable(ffi::SCISSOR_TEST);
                gl.Scissor(sx, sy, sw.max(0), sh.max(0));
            }
            true
        } else {
            false
        };

        // =================================================================
        // 5. Draw background (dark blue-grey) + wallpaper
        // =================================================================
        unsafe {
            gl.ClearColor(0.1, 0.15, 0.25, 1.0);
            gl.Clear(ffi::COLOR_BUFFER_BIT);
        }

        // Poll pending wallpaper loads and render wallpaper if set
        unsafe {
            self.poll_pending_wallpapers(gl);
        }
        if self.wallpaper_texture.is_some() || !self.monitor_wallpapers.is_empty() {
            unsafe {
                self.render_wallpaper(gl, &projection);
            }
        }

        // VRR: update state based on focused window
        self.update_vrr_state(focused);

        // =================================================================
        // 6. Occlusion culling - find lowest fully-opaque window covering screen
        // =================================================================
        let mut first_visible = 0usize;
        {
            let sw = self.screen_w as i32;
            let sh = self.screen_h as i32;
            for i in (0..scene.len()).rev() {
                let (win_id, x, y, w, h) = scene[i];
                let is_alpha = self.windows.get(&win_id).map_or(true, |ws| ws.has_alpha);
                let has_fade = self
                    .windows
                    .get(&win_id)
                    .map_or(false, |ws| ws.fade_opacity < 1.0);
                if !is_alpha
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

        let visible_scene = &scene[first_visible..];

        // =================================================================
        // 7. Draw shadows
        // =================================================================
        self.frame_profiler.zone_start("shadows");
        if self.shadow_enabled && self.shadow_radius > 0.0 {
            unsafe {
                gl.UseProgram(self.shadow_program);
                self.set_projection_uniform(gl, self.shadow_uniforms.projection, &projection);
                gl.BindVertexArray(self.quad_vao);

                let spread = self.shadow_spread;
                let [ox, oy] = self.shadow_offset;
                let [sr, sg, sb, sa] = self.shadow_color;

                gl.Uniform1f(self.shadow_uniforms.spread, spread);

                for &(win_id, x, y, w, h) in visible_scene {
                    let wt = match self.windows.get(&win_id) {
                        Some(wt) => wt,
                        None => continue,
                    };

                    // Skip shaped / fullscreen windows
                    if wt.is_shaped || wt.is_fullscreen {
                        continue;
                    }

                    // Skip windows in shadow_exclude list
                    if !wt.class_name.is_empty()
                        && Self::class_matches_exclude(&wt.class_name, &self.shadow_exclude)
                    {
                        continue;
                    }

                    // Modulate shadow alpha by fade
                    let fade = wt.fade_opacity;
                    let sa_faded = sa * fade;
                    if sa_faded <= 0.0 {
                        continue;
                    }

                    gl.Uniform4f(self.shadow_uniforms.shadow_color, sr, sg, sb, sa_faded);

                    // Per-window corner radius
                    let win_radius = wt.corner_radius_override.unwrap_or(self.corner_radius);
                    gl.Uniform1f(self.shadow_uniforms.radius, win_radius);

                    // Shadow rect: expanded by spread + offset
                    let sx = x as f32 + ox - spread;
                    let sy = y as f32 + oy - spread;
                    let sw = w as f32 + 2.0 * spread;
                    let sh = h as f32 + 2.0 * spread;

                    self.set_rect_uniform(gl, self.shadow_uniforms.rect, sx, sy, sw, sh);
                    gl.Uniform2f(self.shadow_uniforms.size, w as f32, h as f32);

                    gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                }

                gl.BindVertexArray(0);
                gl.UseProgram(0);
            }
        }

        self.frame_profiler.zone_end();

        // =================================================================
        // 8. Blur pass (for frosted/translucent windows)
        // =================================================================
        self.frame_profiler.zone_start("blur");
        let has_frosted = visible_scene.iter().any(|&(win_id, _, _, _, _)| {
            self.windows.get(&win_id).map_or(false, |ws| {
                ws.is_frosted
                    && (ws.class_name.is_empty()
                        || !Self::class_matches_exclude(&ws.class_name, &self.blur_exclude))
            })
        });

        let blur_result_tex = if self.blur_enabled && has_frosted && !self.blur_fbos.is_empty() {
            self.temporal_blur_total_count += 1;

            let current_hash = self.compute_window_positions_hash();
            let can_reuse = self.temporal_blur_enabled
                && current_hash == self.prev_window_positions_hash
                && self.prev_blur_fbo.is_some();

            let tex = if can_reuse {
                self.temporal_blur_reuse_count += 1;
                self.prev_blur_fbo.unwrap().1
            } else {
                // Capture current scene to scene_fbo
                self.blit_fbo(
                    gl,
                    self.output_fbo,
                    self.scene_fbo,
                    self.screen_w,
                    self.screen_h,
                );

                // Run blur downsample/upsample passes. Per-window quality:
                // pick the highest quality among visible frosted windows so
                // focused windows stay sharp while unfocused/off-screen ones
                // don't drive cost up.
                let blur_quality = self.compute_max_visible_blur_quality(visible_scene, focused);
                self.run_blur_passes(gl, self.scene_texture, &projection, blur_quality);

                // Record blur operation for cache warmup statistics
                self.cache_warmup_mgr.record_blur_operation(self.screen_w, self.screen_h);

                let result = self.blur_fbos[0].texture;

                // Temporal mix: blend a motion-scaled amount of the previous
                // blur into the fresh result to reduce frame-to-frame shimmer.
                // On large motion the ratio decays to ~0 (pure current) to avoid
                // ghosting. The displayed result is fed back as the new history
                // (exponential moving average).
                let display_tex = if self.temporal_blur_enabled {
                    let ratio = self.temporal_mix_ratio_for_motion(visible_scene);
                    let mixed = match self.prev_blur_fbo {
                        Some((_, prev_tex)) if ratio > 0.001 => unsafe {
                            self.run_temporal_mix(gl, result, prev_tex, ratio)
                        },
                        _ => result,
                    };
                    unsafe {
                        self.copy_blur_to_prev_fbo(gl, mixed);
                    }
                    mixed
                } else {
                    result
                };

                self.prev_window_positions_hash = current_hash;
                display_tex
            };

            // Re-bind output FBO for further drawing
            unsafe {
                gl.BindFramebuffer(ffi::FRAMEBUFFER, self.output_fbo);
                gl.Viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
            }
            Some(tex)
        } else {
            None
        };

        self.frame_profiler.zone_end();

        // Motion trail: sample per-window position into a ring buffer.
        // Pre-pass before the immutable draw loop so we can take &mut on the
        // window state. When the position is unchanged we pop one entry per
        // frame so the trail naturally drains to empty after the window stops
        // (mirroring X11 effects.rs::update_motion_trail semantics — only there
        // it relies on geometry-sync side effects, here we do it inline).
        if self.motion_trail_enabled && self.motion_trail_frames > 0 {
            let cap = self.motion_trail_frames as usize;
            for &(win_id, x, y, _, _) in visible_scene {
                if let Some(wt) = self.windows.get_mut(&win_id) {
                    let last = wt.motion_trail.back().copied();
                    if last.map_or(true, |(lx, ly)| lx != x || ly != y) {
                        wt.motion_trail.push_back((x, y));
                        while wt.motion_trail.len() > cap {
                            wt.motion_trail.pop_front();
                        }
                    } else if !wt.motion_trail.is_empty() {
                        wt.motion_trail.pop_front();
                    }
                }
            }
        }

        // =================================================================
        // 9. Draw windows (back-to-front)
        // =================================================================
        self.frame_profiler.zone_start("windows");
        unsafe {
            gl.UseProgram(self.program);
            self.set_projection_uniform(gl, self.win_uniforms.projection, &projection);
            gl.Uniform1i(self.win_uniforms.texture, 0);
            gl.Uniform4f(self.win_uniforms.uv_rect, 0.0, 0.0, 1.0, 1.0);
            gl.Uniform1f(self.win_uniforms.ripple_amplitude, 0.0);
            // Default off — only the per-window standard draw path conditionally
            // enables color management. Ancillary draws (blur/ghost) share this
            // program and must not inherit a stale transform.
            gl.Uniform1i(self.win_uniforms.color_managed, 0);
            gl.BindVertexArray(self.quad_vao);

            for &(win_id, x, y, w, h) in visible_scene {
                let wt = match self.windows.get(&win_id) {
                    Some(wt) => wt,
                    None => continue,
                };

                let texture = match wt.gl_texture {
                    Some(tex) => tex,
                    None => continue,
                };

                let is_focused = focused == Some(win_id);
                let fade = wt.fade_opacity;
                if fade <= 0.0 {
                    continue;
                }

                // --- Compute effective opacity (per-window rules override) ---
                let base_opacity = if is_focused {
                    self.active_opacity
                } else {
                    self.inactive_opacity
                };
                let rule_opacity = wt.opacity_override
                    .or_else(|| self.lookup_opacity_rule(&wt.class_name))
                    .unwrap_or(base_opacity);
                let has_explicit_transparency = rule_opacity < 1.0;

                // --- Compute dim factor ---
                let inactive_dim_factor = if is_focused { 1.0 } else { self.inactive_dim };
                let dim = if wt.has_alpha {
                    (rule_opacity * fade * inactive_dim_factor).clamp(0.0, 1.0)
                } else {
                    inactive_dim_factor
                };
                let opacity = if wt.has_alpha {
                    -1.0
                } else if has_explicit_transparency || fade < 1.0 {
                    (rule_opacity * fade).clamp(0.0, 1.0)
                } else {
                    1.0
                };

                // --- Compute corner radius (per-window rules override) ---
                let radius = if wt.is_shaped || wt.is_fullscreen {
                    0.0
                } else if !wt.class_name.is_empty()
                    && Self::class_matches_exclude(
                        &wt.class_name,
                        &self.rounded_corners_exclude,
                    )
                {
                    0.0
                } else {
                    wt.corner_radius_override
                        .or_else(|| self.lookup_corner_radius_rule(&wt.class_name))
                        .unwrap_or(self.corner_radius)
                };

                // --- Compute scale from animation ---
                let scale = wt.anim_scale;
                let (draw_x, draw_y, draw_w, draw_h) = if (scale - 1.0).abs() > f32::EPSILON {
                    let cw = w as f32 * scale;
                    let ch = h as f32 * scale;
                    let cx = x as f32 + (w as f32 - cw) * 0.5;
                    let cy = y as f32 + (h as f32 - ch) * 0.5;
                    (cx, cy, cw, ch)
                } else {
                    (x as f32, y as f32, w as f32, h as f32)
                };

                // --- UV rect: use content_uv (accounts for CSD geometry offset) ---
                let [cu, cv, cw, ch] = wt.content_uv;
                let (uv_x, uv_y, uv_w, uv_h) = if wt.y_inverted {
                    (cu, cv + ch, cw, -ch)
                } else {
                    (cu, cv, cw, ch)
                };

                // --- Draw blur behind frosted window ---
                if wt.is_frosted && self.blur_enabled && blur_result_tex.is_some() {
                    let blur_tex = blur_result_tex.unwrap();
                    gl.ActiveTexture(ffi::TEXTURE0);
                    gl.BindTexture(ffi::TEXTURE_2D, blur_tex);

                    // UV coordinates for the window's screen region
                    let uv_sx = draw_x / self.screen_w as f32;
                    let uv_sy = draw_y / self.screen_h as f32;
                    let uv_sw = draw_w / self.screen_w as f32;
                    let uv_sh = draw_h / self.screen_h as f32;

                    // Per-window frosted strength modulates blur opacity
                    let blur_opacity = fade * wt.frosted_strength.max(0.1);

                    gl.Uniform4f(self.win_uniforms.uv_rect, uv_sx, uv_sy, uv_sw, uv_sh);
                    gl.Uniform1f(self.win_uniforms.opacity, blur_opacity);
                    gl.Uniform1f(self.win_uniforms.dim, 1.0);
                    gl.Uniform1f(self.win_uniforms.radius, radius);
                    gl.Uniform2f(self.win_uniforms.size, draw_w, draw_h);
                    self.set_rect_uniform(
                        gl,
                        self.win_uniforms.rect,
                        draw_x,
                        draw_y,
                        draw_w,
                        draw_h,
                    );
                    gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);

                    // Restore UV for the actual window texture
                    gl.Uniform4f(self.win_uniforms.uv_rect, uv_x, uv_y, uv_w, uv_h);
                }

                // --- Motion trail ghost copies (Phase 3.1, mirrors X11) ---
                // Draw historical positions with decreasing opacity *before* the
                // main texture so the live window paints on top of its trail.
                // Skips wobbly/tilt windows because the ghost would not match the
                // deformed shader output; trails on plain moving windows are the
                // common case and visually consistent with X11.
                if self.motion_trail_enabled
                    && !wt.motion_trail.is_empty()
                    && wt.wobbly.is_none()
                {
                    let trail_len = wt.motion_trail.len();
                    gl.Uniform4f(self.win_uniforms.uv_rect, uv_x, uv_y, uv_w, uv_h);
                    gl.ActiveTexture(ffi::TEXTURE0);
                    gl.BindTexture(ffi::TEXTURE_2D, texture);
                    gl.Uniform1f(self.win_uniforms.radius, radius);
                    gl.Uniform2f(self.win_uniforms.size, draw_w, draw_h);
                    for (i, &(tx, ty)) in wt.motion_trail.iter().enumerate() {
                        let ghost_opacity =
                            self.motion_trail_opacity * (i as f32 + 1.0) / trail_len as f32;
                        gl.Uniform1f(self.win_uniforms.opacity, ghost_opacity * fade);
                        gl.Uniform1f(self.win_uniforms.dim, 0.7);
                        self.set_rect_uniform(
                            gl,
                            self.win_uniforms.rect,
                            tx as f32,
                            ty as f32,
                            draw_w,
                            draw_h,
                        );
                        gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                    }
                    // Restore main-pass uniforms; opacity/dim are written below
                    // anyway, but keep the texture bound for the standard draw.
                }

                // --- Choose shader: wobbly, tilt, or standard ---
                if wt.wobbly.is_some() {
                    // Wobbly windows: switch to wobbly program
                    let wobbly = wt.wobbly.as_ref().unwrap();
                    gl.UseProgram(self.wobbly_program);
                    self.set_projection_uniform(gl, self.wobbly_uniforms.projection, &projection);
                    self.set_rect_uniform(
                        gl,
                        self.wobbly_uniforms.rect,
                        draw_x,
                        draw_y,
                        draw_w,
                        draw_h,
                    );
                    gl.Uniform1i(self.wobbly_uniforms.texture, 0);
                    gl.Uniform1f(self.wobbly_uniforms.opacity, opacity);
                    gl.Uniform1f(self.wobbly_uniforms.radius, radius);
                    gl.Uniform2f(self.wobbly_uniforms.size, draw_w, draw_h);
                    gl.Uniform1f(self.wobbly_uniforms.dim, dim);
                    gl.Uniform4f(self.wobbly_uniforms.uv_rect, uv_x, uv_y, uv_w, uv_h);

                    // Upload grid offsets as flat vec2 array, reusing a
                    // persistent scratch buffer instead of allocating per frame.
                    let flat = &mut self.scratch_wobbly_flat;
                    flat.clear();
                    flat.reserve(wobbly.offsets.len() * 2);
                    for o in &wobbly.offsets {
                        flat.push(o[0]);
                        flat.push(o[1]);
                    }
                    gl.Uniform2fv(
                        self.wobbly_uniforms.grid_offsets,
                        flat.len() as i32 / 2,
                        flat.as_ptr(),
                    );
                    let grid_n = wobbly.grid_n as i32;
                    gl.Uniform1i(self.wobbly_uniforms.grid_n, grid_n);

                    gl.ActiveTexture(ffi::TEXTURE0);
                    self.bind_window_texture(gl, texture);
                    // Grid: (grid_n-1)^2 quads, 6 verts each
                    let quads = grid_n - 1;
                    gl.DrawArrays(ffi::TRIANGLES, 0, quads * quads * 6);

                    // Restore standard program
                    gl.UseProgram(self.program);
                    self.set_projection_uniform(gl, self.win_uniforms.projection, &projection);
                    gl.Uniform1i(self.win_uniforms.texture, 0);
                    gl.Uniform4f(self.win_uniforms.uv_rect, 0.0, 0.0, 1.0, 1.0);
                    gl.Uniform1f(self.win_uniforms.ripple_amplitude, 0.0);
                } else if is_focused && (self.tilt_x.abs() > 0.001 || self.tilt_y.abs() > 0.001) {
                    // Tilt: switch to tilt program for focused window
                    gl.UseProgram(self.tilt_program);
                    self.set_projection_uniform(gl, self.tilt_uniforms.projection, &projection);
                    self.set_rect_uniform(
                        gl,
                        self.tilt_uniforms.rect,
                        draw_x,
                        draw_y,
                        draw_w,
                        draw_h,
                    );
                    gl.Uniform1i(self.tilt_uniforms.texture, 0);
                    gl.Uniform1f(self.tilt_uniforms.opacity, opacity);
                    gl.Uniform1f(self.tilt_uniforms.radius, radius);
                    gl.Uniform2f(self.tilt_uniforms.size, draw_w, draw_h);
                    gl.Uniform1f(self.tilt_uniforms.dim, dim);
                    gl.Uniform4f(self.tilt_uniforms.uv_rect, uv_x, uv_y, uv_w, uv_h);
                    gl.Uniform2f(self.tilt_uniforms.tilt, self.tilt_x, self.tilt_y);
                    gl.Uniform1f(self.tilt_uniforms.perspective, 800.0);
                    let grid = 12i32;
                    gl.Uniform1i(self.tilt_uniforms.grid_size, grid);
                    gl.Uniform2f(self.tilt_uniforms.light_dir, 0.0, -1.0);

                    gl.ActiveTexture(ffi::TEXTURE0);
                    self.bind_window_texture(gl, texture);
                    // Grid: grid^2 quads, 6 verts each
                    gl.DrawArrays(ffi::TRIANGLES, 0, grid * grid * 6);

                    // Restore standard program
                    gl.UseProgram(self.program);
                    self.set_projection_uniform(gl, self.win_uniforms.projection, &projection);
                    gl.Uniform1i(self.win_uniforms.texture, 0);
                    gl.Uniform4f(self.win_uniforms.uv_rect, 0.0, 0.0, 1.0, 1.0);
                    gl.Uniform1f(self.win_uniforms.ripple_amplitude, 0.0);
                } else {
                    // Standard window draw
                    gl.Uniform1f(self.win_uniforms.opacity, opacity);
                    gl.Uniform1f(self.win_uniforms.dim, dim);
                    gl.Uniform1f(self.win_uniforms.radius, radius);
                    gl.Uniform2f(self.win_uniforms.size, draw_w, draw_h);
                    self.set_rect_uniform(
                        gl,
                        self.win_uniforms.rect,
                        draw_x,
                        draw_y,
                        draw_w,
                        draw_h,
                    );
                    gl.Uniform4f(self.win_uniforms.uv_rect, uv_x, uv_y, uv_w, uv_h);

                    // Ripple animation
                    if wt.ripple_active {
                        gl.Uniform1f(self.win_uniforms.ripple_progress, wt.ripple_progress);
                        gl.Uniform1f(self.win_uniforms.ripple_amplitude, 0.03);
                    }

                    // wp-color-management transform for this surface, if any.
                    // GLSL's mat3 is column-major; ColorTransform stores
                    // matrix_row_major, so pass GL_TRUE for transpose.
                    if let Some(t) = wt.color_transform.as_ref() {
                        gl.Uniform1i(self.win_uniforms.color_managed, 1);
                        gl.UniformMatrix3fv(
                            self.win_uniforms.color_matrix,
                            1,
                            ffi::TRUE,
                            t.matrix_row_major.as_ptr(),
                        );
                        gl.Uniform1i(
                            self.win_uniforms.decode_tf,
                            t.inverse_eotf.shader_id(),
                        );
                        gl.Uniform1f(
                            self.win_uniforms.decode_gamma,
                            t.inverse_eotf.gamma_for_shader(),
                        );
                        gl.Uniform1i(
                            self.win_uniforms.encode_tf,
                            t.forward_eotf.shader_id(),
                        );
                        gl.Uniform1f(
                            self.win_uniforms.encode_gamma,
                            t.forward_eotf.gamma_for_shader(),
                        );
                    }

                    gl.ActiveTexture(ffi::TEXTURE0);
                    self.bind_window_texture(gl, texture);
                    gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);

                    // Reset to default off so the next iteration's blur/ghost
                    // draws don't inherit this window's transform.
                    if wt.color_transform.is_some() {
                        gl.Uniform1i(self.win_uniforms.color_managed, 0);
                    }

                    // Reset ripple
                    if wt.ripple_active {
                        gl.Uniform1f(self.win_uniforms.ripple_amplitude, 0.0);
                    }
                }
            }

            gl.BindVertexArray(0);
            gl.UseProgram(0);
        }

        self.frame_profiler.zone_end();

        // =================================================================
        // 9b. Genie minimize animations (mirror X11 pass 2b)
        // =================================================================
        if !self.genie_active.is_empty() {
            self.frame_profiler.zone_start("genie");
            let genie_duration_ms = self.genie_duration_ms;
            let dock = (self.dock_x, self.dock_y);
            unsafe {
                gl.UseProgram(self.genie_program);
                self.set_projection_uniform(gl, self.genie_uniforms.projection, &projection);
                gl.Uniform1i(self.genie_uniforms.texture, 0);
                gl.Uniform4f(self.genie_uniforms.uv_rect, 0.0, 0.0, 1.0, 1.0);
                gl.Uniform1f(self.genie_uniforms.radius, 0.0);
                let grid = 12i32;
                gl.Uniform1i(self.genie_uniforms.grid_size, grid);
                gl.BindVertexArray(self.quad_vao);

                for ga in &self.genie_active {
                    let elapsed = ga.start.elapsed().as_millis() as f32;
                    let progress = (elapsed / genie_duration_ms as f32).min(1.0);
                    let opacity = 1.0 - progress;
                    self.set_rect_uniform(gl, self.genie_uniforms.rect, ga.x, ga.y, ga.w, ga.h);
                    gl.Uniform2f(self.genie_uniforms.size, ga.w, ga.h);
                    gl.Uniform1f(self.genie_uniforms.progress, progress);
                    gl.Uniform2f(self.genie_uniforms.dock_pos, dock.0, dock.1);
                    // Sign of opacity encodes "premultiplied alpha" path in shader
                    // (matches X11 convention: negative for RGBA buffers).
                    gl.Uniform1f(
                        self.genie_uniforms.opacity,
                        if ga.has_alpha { -opacity } else { opacity },
                    );
                    gl.Uniform1f(self.genie_uniforms.dim, 1.0);
                    gl.ActiveTexture(ffi::TEXTURE0);
                    gl.BindTexture(ffi::TEXTURE_2D, ga.gl_texture);
                    gl.DrawArrays(ffi::TRIANGLES, 0, grid * grid * 6);
                }

                gl.BindVertexArray(0);
                gl.UseProgram(0);
            }
            self.frame_profiler.zone_end();
        }

        // =================================================================
        // 10. Draw borders (focused and urgent windows)
        // =================================================================
        self.frame_profiler.zone_start("borders");
        if self.border_enabled {
        unsafe {
            gl.UseProgram(self.border_program);
            self.set_projection_uniform(gl, self.border_uniforms.projection, &projection);
            gl.BindVertexArray(self.quad_vao);

            for &(win_id, x, y, w, h) in visible_scene {
                let wt = match self.windows.get(&win_id) {
                    Some(wt) => wt,
                    None => continue,
                };

                let is_focused = focused == Some(win_id);
                if !is_focused && !wt.is_urgent {
                    continue;
                }

                let fade = wt.fade_opacity;
                if fade <= 0.0 {
                    continue;
                }

                let radius = if wt.is_shaped || wt.is_fullscreen {
                    0.0
                } else {
                    wt.corner_radius_override.unwrap_or(self.corner_radius)
                };

                let scale = wt.anim_scale;
                let (draw_x, draw_y, draw_w, draw_h) = if (scale - 1.0).abs() > f32::EPSILON {
                    let cw = w as f32 * scale;
                    let ch = h as f32 * scale;
                    let cx = x as f32 + (w as f32 - cw) * 0.5;
                    let cy = y as f32 + (h as f32 - ch) * 0.5;
                    (cx, cy, cw, ch)
                } else {
                    (x as f32, y as f32, w as f32, h as f32)
                };

                // Focus highlight: temporary pulse + thicker border on the
                // window that just became focused. Mirrors the X11 behavior
                // (effects.rs::tick_focus_highlight) so the visual is the same
                // on both backends.
                let highlight_for_win = focus_highlight_active
                    && self
                        .focus_highlight_start
                        .map(|(hw, _)| hw == win_id)
                        .unwrap_or(false);

                let border_color = if highlight_for_win {
                    let (_, start) = self.focus_highlight_start.unwrap();
                    let elapsed_ms = start.elapsed().as_millis() as f32;
                    let dur = self.focus_highlight_duration_ms.max(1) as f32;
                    let pulse = ((elapsed_ms / dur * std::f32::consts::PI).sin()).abs();
                    let [r, g, b, a] = self.focus_highlight_color;
                    [r, g, b, a * pulse * fade]
                } else if wt.is_urgent {
                    [1.0f32, 0.2, 0.2, 0.9 * fade]
                } else {
                    let c = self.border_color_focused;
                    [c[0], c[1], c[2], c[3] * fade]
                };
                let border_width = if highlight_for_win {
                    (self.border_width + 2.0).max(3.0)
                } else {
                    self.border_width
                };

                let bdr_x = draw_x - border_width;
                let bdr_y = draw_y - border_width;
                let bdr_w = draw_w + 2.0 * border_width;
                let bdr_h = draw_h + 2.0 * border_width;

                gl.Uniform4f(
                    self.border_uniforms.border_color,
                    border_color[0],
                    border_color[1],
                    border_color[2],
                    border_color[3],
                );
                gl.Uniform1f(self.border_uniforms.border_width, border_width);
                gl.Uniform1f(self.border_uniforms.radius, radius);
                gl.Uniform2f(self.border_uniforms.size, bdr_w, bdr_h);
                self.set_rect_uniform(gl, self.border_uniforms.rect, bdr_x, bdr_y, bdr_w, bdr_h);

                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
            }

            gl.BindVertexArray(0);
            gl.UseProgram(0);
        }
        } // border_enabled
        self.frame_profiler.zone_end();

        // End of scissored output_fbo passes. Effect overlays below always run
        // full-screen, and allow_partial already excludes every one of them, so
        // disabling here keeps the scissor strictly around the calm-frame draws.
        if scissor_active {
            unsafe {
                gl.Disable(ffi::SCISSOR_TEST);
            }
        }

        // =================================================================
        // 11. Genie animations
        // =================================================================
        self.frame_profiler.zone_start("effects");
        // Genie minimize/unminimize animations are rendered by the effects
        // module via render_genie_animations() if any are active. That method
        // is defined in effects.rs.
        self.render_genie_animations(gl, &projection);

        // =================================================================
        // 12. Workspace transitions
        // =================================================================
        if self.transition_active {
            self.render_transition(gl, &projection);
        }

        // =================================================================
        // 13. Snap preview overlay
        // =================================================================
        self.render_snap_preview(gl, &projection);

        // =================================================================
        // 14. Overview overlay
        // =================================================================
        if self.overview_active {
            self.render_overview(gl, &projection);
        }

        // =================================================================
        // 15. Expose overlay
        // =================================================================
        if !self.expose_entries.is_empty() && self.expose_opacity > 0.0 {
            self.render_expose(gl, &projection);
        }

        // =================================================================
        // 15b. Peek mode (fade out non-focused windows)
        // =================================================================
        if self.peek_active {
            self.render_peek_mode(gl, &projection, focused, scene);
        }

        // =================================================================
        // 15c. Tab bar for window groups
        // =================================================================
        if self.window_tabs_enabled && !self.window_groups.is_empty() {
            self.render_tab_bar(gl, &projection);
        }

        // =================================================================
        // 16. Particles
        // =================================================================
        if !self.particle_systems.is_empty() {
            self.render_particles(gl, &projection);
        }

        // =================================================================
        // 17. Edge glow
        // =================================================================
        if self.edge_glow_enabled && self.edge_glow_active && !self.edge_glow_suppressed {
            unsafe {
                gl.UseProgram(self.edge_glow_program);
                self.set_projection_uniform(gl, self.edge_glow_uniforms.projection, &projection);
                self.set_rect_uniform(
                    gl,
                    self.edge_glow_uniforms.rect,
                    0.0,
                    0.0,
                    self.screen_w as f32,
                    self.screen_h as f32,
                );
                let egc = self.edge_glow_color;
                gl.Uniform4f(self.edge_glow_uniforms.glow_color, egc[0], egc[1], egc[2], egc[3]);
                gl.Uniform1f(self.edge_glow_uniforms.glow_width, self.edge_glow_width);
                gl.Uniform2f(self.edge_glow_uniforms.mouse, self.mouse_x, self.mouse_y);
                gl.Uniform2f(
                    self.edge_glow_uniforms.screen_size,
                    self.screen_w as f32,
                    self.screen_h as f32,
                );
                // Use frame_count as a time proxy (at ~60fps, 1 frame = ~16.6ms)
                gl.Uniform1f(self.edge_glow_uniforms.time, self.frame_count as f32 / 60.0);
                gl.BindVertexArray(self.quad_vao);
                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                gl.BindVertexArray(0);
                gl.UseProgram(0);
            }
        }

        // =================================================================
        // 18. Post-processing
        // =================================================================
        if self.postprocess_active {
            // Copy output_fbo to postprocess_fbo
            self.blit_fbo(
                gl,
                self.output_fbo,
                self.postprocess_fbo,
                self.screen_w,
                self.screen_h,
            );

            unsafe {
                // Bind output FBO for final post-processed result
                gl.BindFramebuffer(ffi::FRAMEBUFFER, self.output_fbo);
                gl.Viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
                gl.Clear(ffi::COLOR_BUFFER_BIT);

                gl.UseProgram(self.postprocess_program);
                gl.Uniform1i(self.postprocess_uniforms.texture, 0);
                gl.Uniform1f(self.postprocess_uniforms.color_temp, self.color_temperature);
                gl.Uniform1f(self.postprocess_uniforms.saturation, self.saturation);
                gl.Uniform1f(self.postprocess_uniforms.brightness, self.brightness);
                gl.Uniform1f(self.postprocess_uniforms.contrast, self.contrast);
                gl.Uniform1i(
                    self.postprocess_uniforms.invert,
                    if self.invert_colors { 1 } else { 0 },
                );
                gl.Uniform1i(
                    self.postprocess_uniforms.grayscale,
                    if self.grayscale { 1 } else { 0 },
                );
                gl.Uniform1i(
                    self.postprocess_uniforms.magnifier_enabled,
                    if self.magnifier_enabled { 1 } else { 0 },
                );
                if self.magnifier_enabled {
                    let cx = self.mouse_x / self.screen_w as f32;
                    let cy = self.mouse_y / self.screen_h as f32;
                    gl.Uniform2f(self.postprocess_uniforms.magnifier_center, cx, 1.0 - cy);
                    gl.Uniform1f(
                        self.postprocess_uniforms.magnifier_radius,
                        self.magnifier_radius / self.screen_w as f32,
                    );
                    gl.Uniform1f(
                        self.postprocess_uniforms.magnifier_zoom,
                        self.magnifier_zoom,
                    );
                }
                gl.Uniform1i(
                    self.postprocess_uniforms.colorblind_mode,
                    self.colorblind_mode,
                );
                gl.Uniform1i(
                    self.postprocess_uniforms.hdr_enabled,
                    if self.hdr_enabled { 1 } else { 0 },
                );
                gl.Uniform1f(self.postprocess_uniforms.hdr_peak_nits, self.hdr_peak_nits);
                gl.Uniform1i(
                    self.postprocess_uniforms.tone_mapping_method,
                    self.tone_mapping_method,
                );

                gl.ActiveTexture(ffi::TEXTURE0);
                gl.BindTexture(ffi::TEXTURE_2D, self.postprocess_texture);
                gl.BindVertexArray(self.quad_vao);
                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);

                gl.BindVertexArray(0);
                gl.UseProgram(0);
            }
        }

        self.frame_profiler.zone_end();

        // =================================================================
        // 19. Screenshot capture (region or full)
        // =================================================================
        if self.pending_screenshot.is_some() || self.pending_screenshot_region.is_some() {
            unsafe {
                gl.BindFramebuffer(ffi::FRAMEBUFFER, self.output_fbo);
                self.capture_pending_screenshots(gl);
            }
        }

        // =================================================================
        // 19b. Extended Debug HUD
        // =================================================================
        if self.debug_hud_enabled && self.debug_hud_extended {
            unsafe {
                gl.BindFramebuffer(ffi::FRAMEBUFFER, self.output_fbo);
                self.render_debug_hud(gl, &projection);
            }
        }

        // =================================================================
        // 19c. Annotations overlay
        // =================================================================
        if self.annotation_active && !self.annotation_strokes.is_empty() {
            unsafe {
                gl.BindFramebuffer(ffi::FRAMEBUFFER, self.output_fbo);
                self.render_annotations(gl, &projection);
            }
        }

        // =================================================================
        // 20. Finalize - unbind FBO
        // =================================================================
        unsafe {
            gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
        }

        // =================================================================
        // 21. Recording capture (async PBO readback to ffmpeg)
        // =================================================================
        if let Some(path) = self.pending_recording_start.take() {
            unsafe {
                if let Err(e) = self.recording.start(gl, self.screen_w, self.screen_h, &path, 30) {
                    log::error!("[compositor] Failed to start recording: {}", e);
                }
            }
        }
        if self.recording.is_active() {
            unsafe {
                self.recording.capture_frame(gl, self.output_fbo);
            }
        }
        if self.pending_recording_stop {
            self.pending_recording_stop = false;
            unsafe {
                self.recording.stop(gl);
            }
        }

        // =================================================================
        // 22. Performance infrastructure - frame end
        // =================================================================
        let frame_ms = self.frame_profiler.end_frame();
        self.perf_metrics.record_compositor(std::time::Duration::from_secs_f32(frame_ms / 1000.0));
        self.adaptive_scheduler.on_frame_completed(std::time::Duration::from_secs_f32(frame_ms / 1000.0));
        self.dirty_region_tracker.clear();
        self.content_dirty_ids.clear();
        self.prev_focused = focused;

        // Predictive render: update scene activity periodically
        self.predictive_render_mgr.update_scene_activity();

        // Schedule next render if animations are still active
        if any_animating {
            self.needs_render = true;
        }

        // Mark frame for rate limiter
        self.frame_rate_limiter.mark_frame();

        true
    }

    fn render_genie_animations(&self, gl: &ffi::Gles2, projection: &[f32; 16]) {
        let _ = (gl, projection);
    }

    unsafe fn capture_pending_screenshots(&mut self, gl: &ffi::Gles2) {
        unsafe {
            // Full screenshot
            if let Some(path) = self.pending_screenshot.take() {
                let w = self.screen_w;
                let h = self.screen_h;
                let mut pixels = vec![0u8; (w * h * 4) as usize];
                gl.ReadPixels(
                    0,
                    0,
                    w as i32,
                    h as i32,
                    ffi::RGBA,
                    ffi::UNSIGNED_BYTE,
                    pixels.as_mut_ptr() as *mut _,
                );
                // Flip vertically (OpenGL reads bottom-to-top)
                let row_bytes = (w * 4) as usize;
                let mut temp = vec![0u8; row_bytes];
                for y in 0..(h as usize / 2) {
                    let top = y * row_bytes;
                    let bot = ((h as usize) - 1 - y) * row_bytes;
                    temp.copy_from_slice(&pixels[top..top + row_bytes]);
                    pixels.copy_within(bot..bot + row_bytes, top);
                    pixels[bot..bot + row_bytes].copy_from_slice(&temp);
                }
                // Save in background thread
                std::thread::spawn(move || {
                    if let Err(e) = image::save_buffer(&path, &pixels, w, h, image::ColorType::Rgba8)
                    {
                        log::warn!("[compositor] screenshot failed: {}", e);
                    } else {
                        log::info!("[compositor] screenshot saved to {:?}", path);
                    }
                });
            }

            // Region screenshot
            if let Some((path, rx, ry, rw, rh)) = self.pending_screenshot_region.take() {
                let mut pixels = vec![0u8; (rw * rh * 4) as usize];
                gl.ReadPixels(
                    rx,
                    (self.screen_h as i32) - ry - (rh as i32),
                    rw as i32,
                    rh as i32,
                    ffi::RGBA,
                    ffi::UNSIGNED_BYTE,
                    pixels.as_mut_ptr() as *mut _,
                );
                // Flip vertically
                let row_bytes = (rw * 4) as usize;
                let mut temp = vec![0u8; row_bytes];
                for y in 0..(rh as usize / 2) {
                    let top = y * row_bytes;
                    let bot = ((rh as usize) - 1 - y) * row_bytes;
                    temp.copy_from_slice(&pixels[top..top + row_bytes]);
                    pixels.copy_within(bot..bot + row_bytes, top);
                    pixels[bot..bot + row_bytes].copy_from_slice(&temp);
                }
                std::thread::spawn(move || {
                    if let Err(e) =
                        image::save_buffer(&path, &pixels, rw, rh, image::ColorType::Rgba8)
                    {
                        log::warn!("[compositor] region screenshot failed: {}", e);
                    } else {
                        log::info!("[compositor] region screenshot saved to {:?}", path);
                    }
                });
            }
        }
    }

    unsafe fn render_debug_hud(&mut self, gl: &ffi::Gles2, projection: &[f32; 16]) {
        let uptime = self.compositor_start_time.elapsed().as_secs();
        let hud_text = format!(
            "FPS: {:.0}\nFrame: {}\nWindows: {}\nUptime: {}s\nBlur reuse: {}/{}\nVRR: {}",
            self.fps,
            self.frame_count,
            self.windows.len(),
            uptime,
            self.temporal_blur_reuse_count,
            self.temporal_blur_total_count,
            if self.vrr_active { "ON" } else { "off" },
        );

        if hud_text != self.hud_text_cache {
            let (pixels, w, h) =
                font::render_text_to_rgba(&hud_text, 2, [255, 255, 255, 220]);
            if w > 0 && h > 0 {
                unsafe {
                    // Delete old texture
                    if let Some(old) = self.hud_text_texture.take() {
                        gl.DeleteTextures(1, &old);
                    }
                    // Create and upload new texture
                    let mut tex = 0u32;
                    gl.GenTextures(1, &mut tex);
                    gl.BindTexture(ffi::TEXTURE_2D, tex);
                    gl.TexImage2D(
                        ffi::TEXTURE_2D,
                        0,
                        ffi::RGBA as i32,
                        w as i32,
                        h as i32,
                        0,
                        ffi::RGBA,
                        ffi::UNSIGNED_BYTE,
                        pixels.as_ptr() as *const _,
                    );
                    gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, ffi::LINEAR as i32);
                    gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, ffi::LINEAR as i32);
                    self.hud_text_texture = Some(tex);
                    self.hud_text_width = w;
                    self.hud_text_height = h;
                }
            }
            self.hud_text_cache = hud_text;
        }

        // Draw the HUD texture in the top-left corner
        if let Some(tex) = self.hud_text_texture {
            unsafe {
                gl.UseProgram(self.program);
                self.set_projection_uniform(gl, self.win_uniforms.projection, projection);
                gl.Uniform1i(self.win_uniforms.texture, 0);
                gl.Uniform1f(self.win_uniforms.opacity, 0.85);
                gl.Uniform1f(self.win_uniforms.dim, 1.0);
                gl.Uniform1f(self.win_uniforms.radius, 4.0);
                gl.Uniform2f(
                    self.win_uniforms.size,
                    self.hud_text_width as f32,
                    self.hud_text_height as f32,
                );
                self.set_rect_uniform(
                    gl,
                    self.win_uniforms.rect,
                    10.0,
                    10.0,
                    self.hud_text_width as f32,
                    self.hud_text_height as f32,
                );
                gl.Uniform4f(self.win_uniforms.uv_rect, 0.0, 0.0, 1.0, 1.0);
                gl.Uniform1f(self.win_uniforms.ripple_amplitude, 0.0);

                gl.ActiveTexture(ffi::TEXTURE0);
                gl.BindTexture(ffi::TEXTURE_2D, tex);
                gl.BindVertexArray(self.quad_vao);
                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                gl.BindVertexArray(0);
                gl.UseProgram(0);
            }
        }
    }

    #[allow(dead_code)]
    pub(crate) fn request_screenshot(&mut self, path: PathBuf) {
        self.pending_screenshot = Some(path);
        self.needs_render = true;
    }

    #[allow(dead_code)]
    pub(crate) fn request_screenshot_region(
        &mut self,
        path: PathBuf,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    ) {
        self.pending_screenshot_region = Some((path, x, y, w, h));
        self.needs_render = true;
    }

    /// Render annotation strokes as GL_LINES using the line shader.
    unsafe fn render_annotations(&self, gl: &ffi::Gles2, projection: &[f32; 16]) {
        unsafe {
            gl.UseProgram(self.line_program);
            gl.UniformMatrix4fv(
                self.line_uniform_projection,
                1,
                ffi::FALSE as u8,
                projection.as_ptr(),
            );
            gl.Enable(ffi::BLEND);
            gl.BlendFunc(ffi::SRC_ALPHA, ffi::ONE_MINUS_SRC_ALPHA);

            for stroke in &self.annotation_strokes {
                if stroke.points.len() < 2 {
                    continue;
                }

                gl.LineWidth(stroke.width);
                gl.Uniform4f(
                    self.line_uniform_color,
                    stroke.color[0],
                    stroke.color[1],
                    stroke.color[2],
                    stroke.color[3],
                );

                // Build vertex data for GL_LINES (pairs of adjacent points)
                let mut vertices: Vec<f32> = Vec::with_capacity((stroke.points.len() - 1) * 4);
                for i in 0..stroke.points.len() - 1 {
                    let (x0, y0) = stroke.points[i];
                    let (x1, y1) = stroke.points[i + 1];
                    vertices.extend_from_slice(&[x0, y0, x1, y1]);
                }

                let mut vbo = 0u32;
                gl.GenBuffers(1, &mut vbo);
                gl.BindBuffer(ffi::ARRAY_BUFFER, vbo);
                gl.BufferData(
                    ffi::ARRAY_BUFFER,
                    (vertices.len() * std::mem::size_of::<f32>()) as isize,
                    vertices.as_ptr() as *const _,
                    ffi::STREAM_DRAW,
                );

                gl.EnableVertexAttribArray(0);
                gl.VertexAttribPointer(0, 2, ffi::FLOAT, ffi::FALSE as u8, 8, std::ptr::null());

                let num_verts = ((stroke.points.len() - 1) * 2) as i32;
                gl.DrawArrays(ffi::LINES, 0, num_verts);

                gl.DisableVertexAttribArray(0);
                gl.DeleteBuffers(1, &vbo);
            }

            gl.LineWidth(1.0);
            gl.UseProgram(0);
        }
    }
}
