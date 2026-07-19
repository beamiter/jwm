use super::*;
use smithay::backend::renderer::gles::ffi;

#[derive(Clone, Copy, Debug, PartialEq)]
struct TabBarLayout {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    tab_count: usize,
}

impl TabBarLayout {
    fn tab_rect(self, index: usize) -> Option<[f32; 4]> {
        if index >= self.tab_count {
            return None;
        }

        // Derive both edges from the full window width. This keeps every tab
        // equal-width while guaranteeing that floating-point accumulation can
        // never extend the final tab past the window's right edge.
        let count = self.tab_count as f32;
        let left = self.x + self.width * index as f32 / count;
        let right = self.x + self.width * (index + 1) as f32 / count;
        Some([left, self.y, (right - left).max(0.0), self.height])
    }
}

fn tab_bar_layout(
    scene: &[(u64, i32, i32, u32, u32)],
    active_window: u64,
    tab_count: usize,
    height: f32,
) -> Option<TabBarLayout> {
    if tab_count == 0 || !height.is_finite() || height <= 0.0 {
        return None;
    }

    let &(_, x, y, width, _) = scene
        .iter()
        .find(|&&(window, _, _, _, _)| window == active_window)?;
    if width == 0 {
        return None;
    }

    Some(TabBarLayout {
        x: x as f32,
        y: (y as f32 - height).max(0.0),
        width: width as f32,
        height,
        tab_count,
    })
}

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
                gl.Uniform4f(
                    rect_loc,
                    0.0,
                    0.0,
                    self.screen_w as f32,
                    self.screen_h as f32,
                );
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
                    (
                        entry.current_x,
                        entry.current_y,
                        entry.current_w,
                        entry.current_h,
                    )
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
            self.bind_quad_vao(gl);

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

    /// Render local recording crop controls after the recorder has copied the
    /// frame, keeping this overlay out of the encoded stream.
    pub(crate) fn render_recording_region_overlay(&self, gl: &ffi::Gles2, projection: &[f32; 16]) {
        let Some((x, y, width, height)) = self.recording_region_overlay else {
            return;
        };
        let x = x as f32;
        let y = y as f32;
        let width = width as f32;
        let height = height as f32;
        if width <= 0.0 || height <= 0.0 {
            return;
        }

        unsafe {
            self.bind_quad_vao(gl);
            gl.UseProgram(self.border_program);
            gl.UniformMatrix4fv(
                self.border_uniforms.projection,
                1,
                ffi::FALSE as u8,
                projection.as_ptr(),
            );
            gl.Uniform4f(self.border_uniforms.border_color, 1.0, 0.2, 0.12, 0.95);
            gl.Uniform1f(self.border_uniforms.radius, 2.0);
            gl.Uniform2f(self.border_uniforms.size, width, height);
            gl.Uniform4f(self.border_uniforms.rect, x, y, width, height);
            gl.Uniform1f(self.border_uniforms.border_width, 3.0);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);

            let handle_size = 10.0;
            for (handle_x, handle_y) in [
                (x, y),
                (x + width * 0.5, y),
                (x + width, y),
                (x, y + height * 0.5),
                (x + width, y + height * 0.5),
                (x, y + height),
                (x + width * 0.5, y + height),
                (x + width, y + height),
            ] {
                gl.Uniform2f(self.border_uniforms.size, handle_size, handle_size);
                gl.Uniform4f(
                    self.border_uniforms.rect,
                    handle_x - handle_size * 0.5,
                    handle_y - handle_size * 0.5,
                    handle_size,
                    handle_size,
                );
                gl.Uniform1f(self.border_uniforms.border_width, handle_size);
                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
            }
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
                gl.Uniform4f(
                    rect_loc,
                    0.0,
                    0.0,
                    self.screen_w as f32,
                    self.screen_h as f32,
                );
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
    pub(crate) fn render_tab_bar(
        &self,
        gl: &ffi::Gles2,
        projection: &[f32; 16],
        scene: &[(u64, i32, i32, u32, u32)],
    ) {
        if self.window_groups.is_empty() {
            return;
        }

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

                // A tab bar belongs to the normal desktop scene. If its active
                // window was culled or is otherwise absent this frame, skip it
                // instead of inventing a (0, 0) fallback position.
                let layout =
                    match tab_bar_layout(scene, active_win_id, tabs.len(), self.tab_bar_height) {
                        Some(layout) => layout,
                        None => continue,
                    };

                gl.UseProgram(self.border_program);
                gl.UniformMatrix4fv(
                    self.border_uniforms.projection,
                    1,
                    ffi::FALSE as u8,
                    projection.as_ptr(),
                );

                for (index, (_, _, is_active)) in tabs.iter().enumerate() {
                    let Some([tab_x, tab_y, tab_width, tab_height]) = layout.tab_rect(index) else {
                        continue;
                    };
                    let color = if *is_active {
                        self.tab_active_color
                    } else {
                        self.tab_bar_color
                    };

                    gl.Uniform4f(
                        self.border_uniforms.rect,
                        tab_x,
                        tab_y,
                        tab_width,
                        tab_height,
                    );
                    gl.Uniform4f(
                        self.border_uniforms.border_color,
                        color[0],
                        color[1],
                        color[2],
                        color[3],
                    );
                    gl.Uniform2f(self.border_uniforms.size, tab_width, tab_height);
                    gl.Uniform1f(self.border_uniforms.radius, 0.0);
                    gl.Uniform1f(self.border_uniforms.border_width, tab_width.max(tab_height));
                    gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
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

#[cfg(test)]
mod tests {
    use super::tab_bar_layout;

    #[test]
    fn tab_bar_layout_uses_active_window_scene_geometry() {
        let scene = [
            (10, -20, 15, 640, 480),
            (20, 180, 90, 303, 220),
            (30, 700, 40, 100, 100),
        ];

        let layout = tab_bar_layout(&scene, 20, 3, 30.0).expect("active window is in scene");
        assert_eq!(
            [layout.x, layout.y, layout.width, layout.height],
            [180.0, 60.0, 303.0, 30.0]
        );

        assert_eq!(layout.tab_rect(0), Some([180.0, 60.0, 101.0, 30.0]));
        assert_eq!(layout.tab_rect(1), Some([281.0, 60.0, 101.0, 30.0]));
        assert_eq!(layout.tab_rect(2), Some([382.0, 60.0, 101.0, 30.0]));
    }

    #[test]
    fn tab_rects_are_equal_and_never_exceed_window_width() {
        let scene = [(42, 11, 70, 100, 60)];
        let layout = tab_bar_layout(&scene, 42, 3, 18.0).expect("valid layout");
        let rects = [
            layout.tab_rect(0).unwrap(),
            layout.tab_rect(1).unwrap(),
            layout.tab_rect(2).unwrap(),
        ];

        let expected_width = 100.0 / 3.0;
        for rect in rects {
            assert!((rect[2] - expected_width).abs() < 0.000_01);
            assert!(rect[0] >= layout.x);
            assert!(rect[0] + rect[2] <= layout.x + layout.width);
        }
        let last = rects[2];
        assert!((last[0] + last[2] - (layout.x + layout.width)).abs() < f32::EPSILON);
    }

    #[test]
    fn tab_bar_layout_has_no_missing_scene_fallback() {
        let scene = [(7, 300, 200, 500, 400)];

        assert!(tab_bar_layout(&scene, 99, 2, 24.0).is_none());
        assert!(tab_bar_layout(&scene, 7, 0, 24.0).is_none());
        assert!(tab_bar_layout(&scene, 7, 2, 0.0).is_none());
    }

    #[test]
    fn tab_bar_layout_clamps_to_output_top_edge() {
        let scene = [(42, 120, 12, 320, 200)];
        let layout = tab_bar_layout(&scene, 42, 2, 30.0).expect("valid layout");

        assert_eq!(layout.y, 0.0);
        assert_eq!(layout.tab_rect(0).unwrap()[1], 0.0);
        assert_eq!(layout.tab_rect(1).unwrap()[1], 0.0);
    }
}
