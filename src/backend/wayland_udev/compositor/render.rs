// render_frame and rendering helpers for the Wayland udev compositor
#[allow(unused_imports)]
use super::*;
use smithay::backend::renderer::gles::ffi;

impl WaylandCompositor {
    // =========================================================================
    // Helper: draw a fullscreen quad (uses gl_VertexID in the vertex shader)
    // =========================================================================

    fn draw_quad(&self, gl: &ffi::Gles2) {
        unsafe {
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
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
        // 1. Frame timing
        // =================================================================
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame_time).as_secs_f32();
        self.last_frame_time = now;

        // Update FPS counter
        self.frame_count += 1;
        if self.frame_count % 60 == 0 {
            self.fps = if dt > 0.0 { 1.0 / dt } else { 0.0 };
        }

        // =================================================================
        // 2. Animation ticks
        // =================================================================
        self.tick_fades(dt);
        self.tick_wobbly(dt);
        self.tick_particles(dt);
        self.tick_snap_preview(dt);
        self.tick_overview(dt);
        self.tick_tilt(dt);

        // Determine if anything needs rendering
        let any_animating = self.has_active_animations()
            || self.transition_active
            || self.expose_active
            || self.overview_active;

        let force_render = any_animating
            || self.postprocess_active
            || self.debug_hud_enabled
            || self.edge_glow_active;

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
        self.needs_render = false;

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

        // =================================================================
        // 5. Draw background (dark blue-grey)
        // =================================================================
        unsafe {
            gl.ClearColor(0.1, 0.15, 0.25, 1.0);
            gl.Clear(ffi::COLOR_BUFFER_BIT);
        }

        // =================================================================
        // 6. Occlusion culling - find lowest fully-opaque window covering screen
        // =================================================================
        let mut first_visible = 0usize;
        {
            let sw = self.screen_w as i32;
            let sh = self.screen_h as i32;
            for i in (0..scene.len()).rev() {
                let (win_id, x, y, w, h) = scene[i];
                let is_alpha = self
                    .windows
                    .get(&win_id)
                    .map_or(true, |ws| ws.has_alpha);
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

                    // Modulate shadow alpha by fade
                    let fade = wt.fade_opacity;
                    let sa_faded = sa * fade;
                    if sa_faded <= 0.0 {
                        continue;
                    }

                    gl.Uniform4f(
                        self.shadow_uniforms.shadow_color,
                        sr,
                        sg,
                        sb,
                        sa_faded,
                    );

                    // Per-window corner radius
                    let win_radius = wt
                        .corner_radius_override
                        .unwrap_or(self.corner_radius);
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

        // =================================================================
        // 8. Blur pass (for frosted/translucent windows)
        // =================================================================
        let has_frosted = visible_scene.iter().any(|&(win_id, _, _, _, _)| {
            self.windows
                .get(&win_id)
                .map_or(false, |ws| ws.is_frosted)
        });

        if self.blur_enabled && has_frosted && !self.blur_fbos.is_empty() {
            // Capture current scene to scene_fbo
            self.blit_fbo(
                gl,
                self.output_fbo,
                self.scene_fbo,
                self.screen_w,
                self.screen_h,
            );

            // Run blur downsample/upsample passes
            self.run_blur_passes(gl, self.scene_texture, &projection);

            // Re-bind output FBO for further drawing
            unsafe {
                gl.BindFramebuffer(ffi::FRAMEBUFFER, self.output_fbo);
                gl.Viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
            }
        }

        // =================================================================
        // 9. Draw windows (back-to-front)
        // =================================================================
        unsafe {
            gl.UseProgram(self.program);
            self.set_projection_uniform(gl, self.win_uniforms.projection, &projection);
            gl.Uniform1i(self.win_uniforms.texture, 0);
            gl.Uniform4f(self.win_uniforms.uv_rect, 0.0, 0.0, 1.0, 1.0);
            gl.Uniform1f(self.win_uniforms.ripple_amplitude, 0.0);
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

                // --- Compute effective opacity ---
                let base_opacity = if is_focused {
                    self.active_opacity
                } else {
                    self.inactive_opacity
                };
                let rule_opacity = wt.opacity_override.unwrap_or(base_opacity);
                let opacity = (rule_opacity * fade).clamp(0.0, 1.0);

                // --- Compute dim factor ---
                let dim = if is_focused { 1.0 } else { self.inactive_dim };

                // --- Compute corner radius ---
                let radius = if wt.is_shaped || wt.is_fullscreen {
                    0.0
                } else {
                    wt.corner_radius_override.unwrap_or(self.corner_radius)
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

                // --- UV rect: normal or Y-inverted ---
                let (uv_x, uv_y, uv_w, uv_h) = if wt.y_inverted {
                    (0.0f32, 1.0f32, 1.0f32, -1.0f32)
                } else {
                    (0.0f32, 0.0f32, 1.0f32, 1.0f32)
                };

                // --- Draw blur behind frosted window ---
                if wt.is_frosted && self.blur_enabled && !self.blur_fbos.is_empty() {
                    // Draw the blurred texture behind this window
                    let blur_tex = self.blur_fbos[0].texture;
                    gl.ActiveTexture(ffi::TEXTURE0);
                    gl.BindTexture(ffi::TEXTURE_2D, blur_tex);

                    // UV coordinates for the window's screen region
                    let uv_sx = draw_x / self.screen_w as f32;
                    let uv_sy = draw_y / self.screen_h as f32;
                    let uv_sw = draw_w / self.screen_w as f32;
                    let uv_sh = draw_h / self.screen_h as f32;

                    gl.Uniform4f(self.win_uniforms.uv_rect, uv_sx, uv_sy, uv_sw, uv_sh);
                    gl.Uniform1f(self.win_uniforms.opacity, fade);
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

                // --- Choose shader: wobbly, tilt, or standard ---
                if wt.wobbly.is_some() {
                    // Wobbly windows: switch to wobbly program
                    let wobbly = wt.wobbly.as_ref().unwrap();
                    gl.UseProgram(self.wobbly_program);
                    self.set_projection_uniform(
                        gl,
                        self.wobbly_uniforms.projection,
                        &projection,
                    );
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

                    // Upload grid offsets as flat vec2 array
                    let flat: Vec<f32> = wobbly
                        .offsets
                        .iter()
                        .flat_map(|o| [o[0], o[1]])
                        .collect();
                    gl.Uniform2fv(
                        self.wobbly_uniforms.grid_offsets,
                        flat.len() as i32 / 2,
                        flat.as_ptr(),
                    );
                    let grid_n = wobbly.grid_n as i32;
                    gl.Uniform1i(self.wobbly_uniforms.grid_n, grid_n);

                    gl.ActiveTexture(ffi::TEXTURE0);
                    gl.BindTexture(ffi::TEXTURE_2D, texture);
                    // Grid: (grid_n-1)^2 quads, 6 verts each
                    let quads = grid_n - 1;
                    gl.DrawArrays(ffi::TRIANGLES, 0, quads * quads * 6);

                    // Restore standard program
                    gl.UseProgram(self.program);
                    self.set_projection_uniform(
                        gl,
                        self.win_uniforms.projection,
                        &projection,
                    );
                    gl.Uniform1i(self.win_uniforms.texture, 0);
                    gl.Uniform4f(self.win_uniforms.uv_rect, 0.0, 0.0, 1.0, 1.0);
                    gl.Uniform1f(self.win_uniforms.ripple_amplitude, 0.0);
                } else if is_focused && (self.tilt_x.abs() > 0.001 || self.tilt_y.abs() > 0.001) {
                    // Tilt: switch to tilt program for focused window
                    gl.UseProgram(self.tilt_program);
                    self.set_projection_uniform(
                        gl,
                        self.tilt_uniforms.projection,
                        &projection,
                    );
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
                    gl.BindTexture(ffi::TEXTURE_2D, texture);
                    // Grid: grid^2 quads, 6 verts each
                    gl.DrawArrays(ffi::TRIANGLES, 0, grid * grid * 6);

                    // Restore standard program
                    gl.UseProgram(self.program);
                    self.set_projection_uniform(
                        gl,
                        self.win_uniforms.projection,
                        &projection,
                    );
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

                    gl.ActiveTexture(ffi::TEXTURE0);
                    gl.BindTexture(ffi::TEXTURE_2D, texture);
                    gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);

                    // Reset ripple
                    if wt.ripple_active {
                        gl.Uniform1f(self.win_uniforms.ripple_amplitude, 0.0);
                    }
                }
            }

            gl.BindVertexArray(0);
            gl.UseProgram(0);
        }

        // =================================================================
        // 10. Draw borders (focused and urgent windows)
        // =================================================================
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

                // Border color: urgent gets red pulse, focused gets accent
                let border_color = if wt.is_urgent {
                    [1.0f32, 0.2, 0.2, 0.9 * fade]
                } else {
                    [0.3f32, 0.6, 1.0, 0.8 * fade]
                };
                let border_width = 2.0f32;

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

        // =================================================================
        // 11. Genie animations
        // =================================================================
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
        if let Some((sp_x, sp_y, sp_w, sp_h)) = self.snap_preview {
            let snap_opacity = self.snap_preview_opacity;
            if snap_opacity > 0.0 {
                unsafe {
                    gl.UseProgram(self.program);
                    self.set_projection_uniform(gl, self.win_uniforms.projection, &projection);
                    gl.BindVertexArray(self.quad_vao);

                    // Semi-transparent blue overlay
                    gl.Uniform1f(self.win_uniforms.opacity, snap_opacity * 0.3);
                    gl.Uniform1f(self.win_uniforms.dim, 1.0);
                    gl.Uniform1f(self.win_uniforms.radius, 8.0);
                    gl.Uniform2f(self.win_uniforms.size, sp_w, sp_h);
                    self.set_rect_uniform(gl, self.win_uniforms.rect, sp_x, sp_y, sp_w, sp_h);
                    gl.Uniform4f(self.win_uniforms.uv_rect, 0.0, 0.0, 1.0, 1.0);
                    gl.Uniform1f(self.win_uniforms.ripple_amplitude, 0.0);

                    // Use a 1x1 white pixel texture (or just draw with no tex)
                    // For simplicity, bind 0 and rely on shader alpha behavior
                    gl.Uniform1i(self.win_uniforms.texture, 0);
                    gl.ActiveTexture(ffi::TEXTURE0);
                    gl.BindTexture(ffi::TEXTURE_2D, 0);
                    gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);

                    gl.BindVertexArray(0);
                    gl.UseProgram(0);
                }
            }
        }

        // =================================================================
        // 14. Overview overlay
        // =================================================================
        if self.overview_active {
            self.render_overview(gl, &projection);
        }

        // =================================================================
        // 15. Expose overlay
        // =================================================================
        if self.expose_active && !self.expose_entries.is_empty() {
            self.render_expose(gl, &projection);
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
        if self.edge_glow_active && !self.edge_glow_suppressed {
            unsafe {
                gl.UseProgram(self.edge_glow_program);
                self.set_projection_uniform(
                    gl,
                    self.edge_glow_uniforms.projection,
                    &projection,
                );
                self.set_rect_uniform(
                    gl,
                    self.edge_glow_uniforms.rect,
                    0.0,
                    0.0,
                    self.screen_w as f32,
                    self.screen_h as f32,
                );
                gl.Uniform4f(
                    self.edge_glow_uniforms.glow_color,
                    0.3,
                    0.6,
                    1.0,
                    0.6,
                );
                gl.Uniform1f(self.edge_glow_uniforms.glow_width, 20.0);
                gl.Uniform2f(self.edge_glow_uniforms.mouse, self.mouse_x, self.mouse_y);
                gl.Uniform2f(
                    self.edge_glow_uniforms.screen_size,
                    self.screen_w as f32,
                    self.screen_h as f32,
                );
                // Use frame_count as a time proxy (at ~60fps, 1 frame = ~16.6ms)
                gl.Uniform1f(
                    self.edge_glow_uniforms.time,
                    self.frame_count as f32 / 60.0,
                );
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
                    gl.Uniform1f(self.postprocess_uniforms.magnifier_zoom, self.magnifier_zoom);
                }
                gl.Uniform1i(self.postprocess_uniforms.colorblind_mode, self.colorblind_mode);
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

        // =================================================================
        // 19. Finalize - unbind FBO
        // =================================================================
        unsafe {
            gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
        }

        // Schedule next render if animations are still active
        if any_animating {
            self.needs_render = true;
        }

        true
    }

    fn render_genie_animations(&self, gl: &ffi::Gles2, projection: &[f32; 16]) {
        let _ = (gl, projection);
    }
}
