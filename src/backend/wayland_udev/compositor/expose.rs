use super::*;
use smithay::backend::renderer::gles::ffi;

impl WaylandCompositor {
    /// Render the expose (mission control) mode overlay.
    /// Shows all windows arranged in a grid layout with animation.
    /// Includes dark backdrop, shadows, hover highlight with scale, and content_uv handling.
    pub(crate) fn render_expose(&self, gl: &ffi::Gles2, projection: &[f32; 16]) {
        if self.expose_entries.is_empty() || self.expose_opacity <= 0.0 {
            return;
        }

        unsafe {
            // Dark backdrop
            gl.UseProgram(self.overview_bg_program);
            let rect_loc =
                gl.GetUniformLocation(self.overview_bg_program, b"u_rect\0".as_ptr() as *const _);
            let proj_loc = gl.GetUniformLocation(
                self.overview_bg_program,
                b"u_projection\0".as_ptr() as *const _,
            );
            let opacity_loc = gl.GetUniformLocation(
                self.overview_bg_program,
                b"u_opacity\0".as_ptr() as *const _,
            );

            if rect_loc >= 0 {
                gl.Uniform4f(rect_loc, 0.0, 0.0, self.screen_w as f32, self.screen_h as f32);
            }
            if proj_loc >= 0 {
                gl.UniformMatrix4fv(proj_loc, 1, ffi::FALSE as u8, projection.as_ptr());
            }
            if opacity_loc >= 0 {
                gl.Uniform1f(opacity_loc, self.expose_opacity * 0.85);
            }

            gl.BindVertexArray(self.quad_vao);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);

            // Draw each expose window at its current animated position
            gl.UseProgram(self.program);
            gl.UniformMatrix4fv(
                self.win_uniforms.projection,
                1,
                ffi::FALSE as u8,
                projection.as_ptr(),
            );

            for entry in &self.expose_entries {
                let win = match self.windows.get(&entry.window_id) {
                    Some(w) => w,
                    None => continue,
                };
                let tex = match win.gl_texture {
                    Some(t) => t,
                    None => continue,
                };

                // Apply hover scale: hovered windows get 1.05x centered scale
                let (x, y, w, h) = if entry.is_hovered {
                    let scale = 1.05f32;
                    let sw = entry.current_w * scale;
                    let sh = entry.current_h * scale;
                    let sx = entry.current_x - (sw - entry.current_w) * 0.5;
                    let sy = entry.current_y - (sh - entry.current_h) * 0.5;
                    (sx, sy, sw, sh)
                } else {
                    (entry.current_x, entry.current_y, entry.current_w, entry.current_h)
                };

                // Draw shadow behind each window
                gl.UseProgram(self.shadow_program);
                gl.UniformMatrix4fv(
                    self.shadow_uniforms.projection,
                    1,
                    ffi::FALSE as u8,
                    projection.as_ptr(),
                );
                let spread = 15.0f32;
                gl.Uniform4f(
                    self.shadow_uniforms.rect,
                    x - spread,
                    y - spread,
                    w + spread * 2.0,
                    h + spread * 2.0,
                );
                gl.Uniform4f(
                    self.shadow_uniforms.shadow_color,
                    0.0,
                    0.0,
                    0.0,
                    0.5 * self.expose_opacity,
                );
                gl.Uniform2f(self.shadow_uniforms.size, w, h);
                gl.Uniform1f(self.shadow_uniforms.radius, 6.0);
                gl.Uniform1f(self.shadow_uniforms.spread, spread);
                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);

                // Draw window content
                gl.UseProgram(self.program);
                gl.UniformMatrix4fv(
                    self.win_uniforms.projection,
                    1,
                    ffi::FALSE as u8,
                    projection.as_ptr(),
                );
                gl.Uniform4f(self.win_uniforms.rect, x, y, w, h);

                let opacity = if self.expose_active {
                    self.expose_opacity
                } else {
                    1.0
                };
                gl.Uniform1f(self.win_uniforms.opacity, opacity);
                gl.Uniform1f(self.win_uniforms.radius, 6.0);
                gl.Uniform2f(self.win_uniforms.size, w, h);
                gl.Uniform1f(self.win_uniforms.dim, 1.0);

                // Use content_uv to crop out CSD shadows/decorations
                let [cu, cv, cw, ch] = win.content_uv;
                let (uv_x, uv_y, uv_w, uv_h) = if win.y_inverted {
                    (cu, cv + ch, cw, -ch)
                } else {
                    (cu, cv, cw, ch)
                };
                gl.Uniform4f(self.win_uniforms.uv_rect, uv_x, uv_y, uv_w, uv_h);
                gl.Uniform1f(self.win_uniforms.ripple_progress, -1.0);
                gl.Uniform1f(self.win_uniforms.ripple_amplitude, 0.0);

                gl.ActiveTexture(ffi::TEXTURE0);
                self.bind_window_texture(gl, tex);
                gl.Uniform1i(self.win_uniforms.texture, 0);

                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);

                // Highlight border if hovered (blue, 3px)
                if entry.is_hovered {
                    gl.UseProgram(self.border_program);
                    gl.UniformMatrix4fv(
                        self.border_uniforms.projection,
                        1,
                        ffi::FALSE as u8,
                        projection.as_ptr(),
                    );
                    gl.Uniform1f(self.border_uniforms.border_width, 3.0);
                    gl.Uniform4f(self.border_uniforms.border_color, 0.4, 0.6, 1.0, opacity);
                    gl.Uniform1f(self.border_uniforms.radius, 6.0);
                    gl.Uniform2f(self.border_uniforms.size, w, h);
                    gl.Uniform4f(self.border_uniforms.rect, x, y, w, h);
                    gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);

                    // Restore window program for next iteration
                    gl.UseProgram(self.program);
                    gl.UniformMatrix4fv(
                        self.win_uniforms.projection,
                        1,
                        ffi::FALSE as u8,
                        projection.as_ptr(),
                    );
                }
            }
        }
    }

    /// Render the snap preview highlight rectangle.
    /// Shows a translucent blue rounded rect where a window will snap to.
    pub(crate) fn render_snap_preview(&self, gl: &ffi::Gles2, projection: &[f32; 16]) {
        let (x, y, w, h) = match self.snap_preview {
            Some(rect) => rect,
            None => return,
        };
        if self.snap_preview_opacity <= 0.0 {
            return;
        }

        let opacity = self.snap_preview_opacity;

        unsafe {
            gl.BindVertexArray(self.quad_vao);

            // Draw filled semi-transparent background
            gl.UseProgram(self.border_program);
            gl.UniformMatrix4fv(
                self.border_uniforms.projection,
                1,
                ffi::FALSE as u8,
                projection.as_ptr(),
            );
            gl.Uniform4f(self.border_uniforms.rect, x, y, w, h);
            gl.Uniform4f(
                self.border_uniforms.border_color,
                0.3,
                0.5,
                0.9,
                0.3 * opacity,
            );
            gl.Uniform2f(self.border_uniforms.size, w, h);
            gl.Uniform1f(self.border_uniforms.radius, 8.0);
            // Use a very large border_width to fill the entire rect
            gl.Uniform1f(self.border_uniforms.border_width, w.max(h));
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);

            // Draw border outline (2px solid)
            gl.Uniform4f(
                self.border_uniforms.border_color,
                0.4,
                0.6,
                1.0,
                0.8 * opacity,
            );
            gl.Uniform1f(self.border_uniforms.border_width, 2.0);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
        }
    }

    /// Render peek mode ("boss key") overlay.
    /// Draws a dark overlay over everything, then redraws only the focused window
    /// at full opacity on top, creating a spotlight effect.
    pub(crate) fn render_peek_mode(
        &self,
        gl: &ffi::Gles2,
        projection: &[f32; 16],
        focused: Option<u64>,
        scene: &[(u64, i32, i32, u32, u32)],
    ) {
        if !self.peek_active {
            return;
        }

        unsafe {
            gl.BindVertexArray(self.quad_vao);

            // Draw dark semi-transparent overlay over the entire screen
            gl.UseProgram(self.overview_bg_program);
            let rect_loc = gl.GetUniformLocation(
                self.overview_bg_program,
                b"u_rect\0".as_ptr() as *const _,
            );
            let proj_loc = gl.GetUniformLocation(
                self.overview_bg_program,
                b"u_projection\0".as_ptr() as *const _,
            );
            let opacity_loc = gl.GetUniformLocation(
                self.overview_bg_program,
                b"u_opacity\0".as_ptr() as *const _,
            );

            if rect_loc >= 0 {
                gl.Uniform4f(rect_loc, 0.0, 0.0, self.screen_w as f32, self.screen_h as f32);
            }
            if proj_loc >= 0 {
                gl.UniformMatrix4fv(proj_loc, 1, ffi::FALSE as u8, projection.as_ptr());
            }
            if opacity_loc >= 0 {
                gl.Uniform1f(opacity_loc, 0.7);
            }
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);

            // Redraw the focused window plus any peek-excluded windows (e.g. the
            // status bar) on top at full opacity, mirroring the X11 backend where
            // `peek_exclude` classes keep full opacity during peek.
            gl.UseProgram(self.program);
            gl.UniformMatrix4fv(
                self.win_uniforms.projection,
                1,
                ffi::FALSE as u8,
                projection.as_ptr(),
            );

            for &(id, x, y, w, h) in scene {
                let win = match self.windows.get(&id) {
                    Some(w) => w,
                    None => continue,
                };

                let is_focused = focused == Some(id);
                let is_excluded = !win.class_name.is_empty()
                    && Self::class_matches_exclude(&win.class_name, &self.peek_exclude);
                if !is_focused && !is_excluded {
                    continue;
                }

                let tex = match win.gl_texture {
                    Some(t) => t,
                    None => continue,
                };

                let (wx, wy, ww, wh) = (x as f32, y as f32, w as f32, h as f32);
                gl.Uniform4f(self.win_uniforms.rect, wx, wy, ww, wh);
                gl.Uniform1f(self.win_uniforms.opacity, 1.0);
                gl.Uniform1f(self.win_uniforms.radius, 6.0);
                gl.Uniform2f(self.win_uniforms.size, ww, wh);
                gl.Uniform1f(self.win_uniforms.dim, 1.0);

                let [cu, cv, cw, ch] = win.content_uv;
                let (uv_x, uv_y, uv_w, uv_h) = if win.y_inverted {
                    (cu, cv + ch, cw, -ch)
                } else {
                    (cu, cv, cw, ch)
                };
                gl.Uniform4f(self.win_uniforms.uv_rect, uv_x, uv_y, uv_w, uv_h);
                gl.Uniform1f(self.win_uniforms.ripple_progress, -1.0);
                gl.Uniform1f(self.win_uniforms.ripple_amplitude, 0.0);

                gl.ActiveTexture(ffi::TEXTURE0);
                self.bind_window_texture(gl, tex);
                gl.Uniform1i(self.win_uniforms.texture, 0);

                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
            }
        }
    }

    /// Render tab bars above grouped windows.
    /// Each tab group shows a horizontal bar with tab labels; the active tab is highlighted.
    pub(crate) fn render_tab_bar(&self, gl: &ffi::Gles2, projection: &[f32; 16]) {
        if self.window_groups.is_empty() {
            return;
        }

        const TAB_BAR_HEIGHT: f32 = 24.0;
        const CHAR_WIDTH: f32 = 8.0;
        const TAB_PADDING: f32 = 16.0;

        unsafe {
            gl.BindVertexArray(self.quad_vao);

            for (_, tabs) in &self.window_groups {
                if tabs.is_empty() {
                    continue;
                }

                // Find the active window to position the tab bar above it
                let active_win_id = tabs
                    .iter()
                    .find(|(_, _, active)| *active)
                    .map(|(id, _, _)| *id as u64)
                    .unwrap_or(tabs[0].0 as u64);

                let win = match self.windows.get(&active_win_id) {
                    Some(w) => w,
                    None => continue,
                };

                // The tab bar sits above the window; we need window geometry.
                // Use the window's width and infer position from scene context.
                let bar_w = win.width as f32;
                // We position the bar at y = 0 relative to the window top.
                // Since we don't have absolute coords here, we look for the window
                // in expose_entries or fallback to (0, 0).
                let (bar_x, bar_y) = self
                    .expose_entries
                    .iter()
                    .find(|e| e.window_id == active_win_id)
                    .map(|e| (e.current_x, e.current_y - TAB_BAR_HEIGHT))
                    .unwrap_or((0.0, 0.0));

                // Draw tab bar background
                gl.UseProgram(self.border_program);
                gl.UniformMatrix4fv(
                    self.border_uniforms.projection,
                    1,
                    ffi::FALSE as u8,
                    projection.as_ptr(),
                );
                gl.Uniform4f(self.border_uniforms.rect, bar_x, bar_y, bar_w, TAB_BAR_HEIGHT);
                gl.Uniform4f(self.border_uniforms.border_color, 0.1, 0.1, 0.15, 0.9);
                gl.Uniform2f(self.border_uniforms.size, bar_w, TAB_BAR_HEIGHT);
                gl.Uniform1f(self.border_uniforms.radius, 4.0);
                gl.Uniform1f(self.border_uniforms.border_width, bar_w.max(TAB_BAR_HEIGHT));
                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);

                // Draw individual tabs
                let mut tab_x = bar_x;
                for (i, (_, title, is_active)) in tabs.iter().enumerate() {
                    let tab_w = (title.len() as f32 * CHAR_WIDTH) + TAB_PADDING * 2.0;

                    if *is_active {
                        // Highlighted active tab
                        gl.Uniform4f(self.border_uniforms.rect, tab_x, bar_y, tab_w, TAB_BAR_HEIGHT);
                        gl.Uniform4f(self.border_uniforms.border_color, 0.2, 0.3, 0.5, 0.9);
                        gl.Uniform2f(self.border_uniforms.size, tab_w, TAB_BAR_HEIGHT);
                        gl.Uniform1f(self.border_uniforms.radius, 4.0);
                        gl.Uniform1f(self.border_uniforms.border_width, tab_w.max(TAB_BAR_HEIGHT));
                        gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                    }

                    // Draw separator between tabs (1px line on the right edge)
                    if i < tabs.len() - 1 {
                        let sep_x = tab_x + tab_w - 0.5;
                        gl.Uniform4f(
                            self.border_uniforms.rect,
                            sep_x,
                            bar_y + 4.0,
                            1.0,
                            TAB_BAR_HEIGHT - 8.0,
                        );
                        gl.Uniform4f(self.border_uniforms.border_color, 0.4, 0.4, 0.5, 0.6);
                        gl.Uniform2f(self.border_uniforms.size, 1.0, TAB_BAR_HEIGHT - 8.0);
                        gl.Uniform1f(self.border_uniforms.radius, 0.0);
                        gl.Uniform1f(self.border_uniforms.border_width, 1.0);
                        gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                    }

                    tab_x += tab_w;
                }
            }
        }
    }

    #[allow(dead_code)]
    pub(crate) fn expose_hit_test(&self, x: f32, y: f32) -> Option<u64> {
        for entry in &self.expose_entries {
            if x >= entry.current_x
                && x <= entry.current_x + entry.current_w
                && y >= entry.current_y
                && y <= entry.current_y + entry.current_h
            {
                return Some(entry.window_id);
            }
        }
        None
    }

    #[allow(dead_code)]
    pub(crate) fn set_expose_hover(&mut self, x: f32, y: f32) {
        let hit_id = self.expose_hit_test(x, y);
        let mut changed = false;

        for entry in &mut self.expose_entries {
            let should_hover = Some(entry.window_id) == hit_id;
            if entry.is_hovered != should_hover {
                entry.is_hovered = should_hover;
                changed = true;
            }
        }

        if changed {
            self.needs_render = true;
        }
    }
}
