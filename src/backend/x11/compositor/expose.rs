use super::{Compositor, SnapPreview, WindowTab, class_matches_exclude};
use crate::backend::x11::compositor_common::expose::{build_expose_entries, tick_expose_entries};
use glow::HasContext;

use super::CompositorConnection;

impl<C: CompositorConnection> Compositor<C> {
    // =========================================================================
    // 5.1 Expose / Mission Control mode
    // =========================================================================

    /// Activate or deactivate expose mode.
    /// `windows` contains (x11_win, x, y, w, h) for each window to arrange.
    pub(crate) fn set_expose_mode(
        &mut self,
        active: bool,
        windows: Vec<(u32, i32, i32, u32, u32)>,
    ) {
        if !self.expose_enabled {
            return;
        }

        if active {
            let n = windows.len();
            if n == 0 {
                self.expose_active = false;
                self.expose_entries.clear();
                self.needs_render = true;
                return;
            }

            self.expose_entries = build_expose_entries(
                self.screen_w as f32,
                self.screen_h as f32,
                self.expose_gap,
                &windows,
            );

            self.expose_active = true;
            self.expose_opacity = 0.0;
            self.expose_start = Some(std::time::Instant::now());
        } else {
            // Deactivating - animate back to original positions
            self.expose_active = false;
            self.expose_start = Some(std::time::Instant::now());
        }
        self.needs_render = true;
    }

    /// Tick expose animation. Called from render_frame.
    pub(super) fn tick_expose(&mut self) -> bool {
        if self.expose_entries.is_empty() {
            return false;
        }

        let result = tick_expose_entries(
            &mut self.expose_entries,
            self.expose_active,
            &mut self.expose_opacity,
            1.0 / 60.0_f32,
        );
        if result.clear_entries {
            self.expose_entries.clear();
        }

        result.keep_animating
    }

    /// Render expose overlay. Called from render_frame after borders, before post-process.
    pub(super) fn render_expose(&self, proj: &[f32; 16]) {
        if self.expose_entries.is_empty() || self.expose_opacity <= 0.0 {
            return;
        }

        unsafe {
            // Dark overlay background
            self.gl.use_program(Some(self.overview_bg_program));
            self.gl.uniform_matrix_4_f32_slice(
                self.overview_bg_uniforms.projection.as_ref(),
                false,
                proj,
            );
            self.gl.uniform_4_f32(
                self.overview_bg_uniforms.rect.as_ref(),
                0.0,
                0.0,
                self.screen_w as f32,
                self.screen_h as f32,
            );
            self.gl.uniform_1_f32(
                self.overview_bg_uniforms.opacity.as_ref(),
                self.expose_opacity,
            );
            self.gl.bind_vertex_array(Some(self.quad_vao));
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

            // Draw each window at its current animated position
            self.gl.use_program(Some(self.program));
            self.gl
                .uniform_matrix_4_f32_slice(self.win_uniforms.projection.as_ref(), false, proj);
            self.gl.uniform_1_i32(self.win_uniforms.texture.as_ref(), 0);
            self.gl
                .uniform_4_f32(self.win_uniforms.uv_rect.as_ref(), 0.0, 0.0, 1.0, 1.0);
            self.gl.active_texture(glow::TEXTURE0);

            for entry in &self.expose_entries {
                let wt = match self.windows.get(&entry.x11_win) {
                    Some(wt) => wt,
                    None => continue,
                };

                // When exiting (expose_active=false), keep windows fully opaque
                // so only the dark overlay fades — avoids a dim flash at the end.
                let opacity = if self.expose_active {
                    self.expose_opacity
                } else {
                    1.0
                };
                self.gl
                    .uniform_1_f32(self.win_uniforms.opacity.as_ref(), opacity);
                self.gl
                    .uniform_1_f32(self.win_uniforms.radius.as_ref(), self.corner_radius);
                self.gl.uniform_1_f32(self.win_uniforms.dim.as_ref(), 1.0);
                self.gl.uniform_2_f32(
                    self.win_uniforms.size.as_ref(),
                    entry.current_w,
                    entry.current_h,
                );
                self.gl.uniform_4_f32(
                    self.win_uniforms.rect.as_ref(),
                    entry.current_x,
                    entry.current_y,
                    entry.current_w,
                    entry.current_h,
                );
                self.gl.bind_texture(glow::TEXTURE_2D, Some(wt.gl_texture));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                // Highlight border if hovered
                if entry.is_hovered {
                    self.gl.use_program(Some(self.border_program));
                    self.gl.uniform_matrix_4_f32_slice(
                        self.border_uniforms.projection.as_ref(),
                        false,
                        proj,
                    );
                    self.gl
                        .uniform_1_f32(self.border_uniforms.border_width.as_ref(), 3.0);
                    self.gl.uniform_4_f32(
                        self.border_uniforms.border_color.as_ref(),
                        0.4,
                        0.6,
                        1.0,
                        opacity,
                    );
                    self.gl
                        .uniform_1_f32(self.border_uniforms.radius.as_ref(), self.corner_radius);
                    self.gl.uniform_2_f32(
                        self.border_uniforms.size.as_ref(),
                        entry.current_w,
                        entry.current_h,
                    );
                    self.gl.uniform_4_f32(
                        self.border_uniforms.rect.as_ref(),
                        entry.current_x,
                        entry.current_y,
                        entry.current_w,
                        entry.current_h,
                    );
                    self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                    // Restore window program
                    self.gl.use_program(Some(self.program));
                    self.gl.uniform_matrix_4_f32_slice(
                        self.win_uniforms.projection.as_ref(),
                        false,
                        proj,
                    );
                    self.gl.uniform_1_i32(self.win_uniforms.texture.as_ref(), 0);
                    self.gl
                        .uniform_4_f32(self.win_uniforms.uv_rect.as_ref(), 0.0, 0.0, 1.0, 1.0);
                }
            }

            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
        }
    }

    /// Handle mouse hover in expose mode.
    #[allow(dead_code)]
    pub(super) fn expose_set_hover(&mut self, x: f32, y: f32) {
        for entry in &mut self.expose_entries {
            entry.is_hovered = x >= entry.current_x
                && x <= entry.current_x + entry.current_w
                && y >= entry.current_y
                && y <= entry.current_y + entry.current_h;
        }
        self.needs_render = true;
    }

    /// Handle click in expose mode. Returns the x11_win of the clicked window.
    pub(crate) fn expose_click(&mut self, x: f32, y: f32) -> Option<u32> {
        let result = self.expose_entries.iter().find_map(|entry| {
            if x >= entry.current_x
                && x <= entry.current_x + entry.current_w
                && y >= entry.current_y
                && y <= entry.current_y + entry.current_h
            {
                Some(entry.x11_win)
            } else {
                None
            }
        });
        if result.is_some() {
            self.set_expose_mode(false, Vec::new());
        }
        result
    }

    // =========================================================================
    // 5.2 Smart Snap Preview
    // =========================================================================

    /// Set or clear the snap preview rectangle.
    /// Instantly remove the snap preview (no fade-out animation).
    pub(crate) fn clear_snap_preview_immediate(&mut self) {
        self.snap_target = None;
        self.needs_render = true;
    }

    pub(crate) fn set_snap_preview(&mut self, preview: Option<(f32, f32, f32, f32)>) {
        if !self.snap_preview_enabled {
            return;
        }

        match preview {
            Some((x, y, w, h)) => {
                if let Some(ref mut sp) = self.snap_target {
                    // Update existing preview position
                    sp.x = x;
                    sp.y = y;
                    sp.w = w;
                    sp.h = h;
                    sp.fading_out = false;
                    if sp.opacity < 0.01 {
                        sp.start = std::time::Instant::now();
                    }
                } else {
                    self.snap_target = Some(SnapPreview {
                        x,
                        y,
                        w,
                        h,
                        opacity: 0.0,
                        start: std::time::Instant::now(),
                        fading_out: false,
                    });
                }
            }
            None => {
                if let Some(ref mut sp) = self.snap_target {
                    sp.fading_out = true;
                    sp.start = std::time::Instant::now();
                }
            }
        }
        self.needs_render = true;
    }

    /// Tick snap preview animation. Returns true if still animating.
    pub(super) fn tick_snap_preview(&mut self) -> bool {
        let duration_ms = self.snap_animation_duration_ms.max(1) as f32;
        if let Some(ref mut sp) = self.snap_target {
            let elapsed = sp.start.elapsed().as_millis() as f32;
            let t = (elapsed / duration_ms).min(1.0);
            if sp.fading_out {
                sp.opacity = (1.0 - t).max(0.0);
                if sp.opacity <= 0.0 {
                    self.snap_target = None;
                    return false;
                }
            } else {
                sp.opacity = t.min(1.0);
            }
            true
        } else {
            false
        }
    }

    /// Render snap preview rectangle. Called from render_frame.
    pub(super) fn render_snap_preview(&self, proj: &[f32; 16]) {
        let sp = match &self.snap_target {
            Some(sp) if sp.opacity > 0.0 => sp,
            _ => return,
        };

        unsafe {
            // Use the border shader to draw a filled translucent rectangle
            self.gl.use_program(Some(self.border_program));
            self.gl.uniform_matrix_4_f32_slice(
                self.border_uniforms.projection.as_ref(),
                false,
                proj,
            );
            self.gl.bind_vertex_array(Some(self.quad_vao));

            let [r, g, b, a] = self.snap_preview_color;
            let alpha = a * sp.opacity;

            // Draw a filled rectangle using a very large border width
            // (effectively fills the entire rect as a "border")
            let fill_size = sp.w.max(sp.h);
            self.gl
                .uniform_1_f32(self.border_uniforms.border_width.as_ref(), fill_size);
            self.gl
                .uniform_4_f32(self.border_uniforms.border_color.as_ref(), r, g, b, alpha);
            self.gl
                .uniform_1_f32(self.border_uniforms.radius.as_ref(), self.corner_radius);
            self.gl
                .uniform_2_f32(self.border_uniforms.size.as_ref(), sp.w, sp.h);
            self.gl
                .uniform_4_f32(self.border_uniforms.rect.as_ref(), sp.x, sp.y, sp.w, sp.h);
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

            // Draw a brighter border outline on top
            self.gl
                .uniform_1_f32(self.border_uniforms.border_width.as_ref(), 2.0);
            self.gl.uniform_4_f32(
                self.border_uniforms.border_color.as_ref(),
                r * 1.5,
                g * 1.5,
                b * 1.5,
                alpha * 2.0,
            );
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
        }
    }

    /// Draw the interactive recording crop outline after frame capture so the
    /// controls remain visible locally without being baked into the video.
    pub(super) fn render_recording_region_overlay(&self, proj: &[f32; 16]) {
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
            self.gl.use_program(Some(self.border_program));
            self.gl.uniform_matrix_4_f32_slice(
                self.border_uniforms.projection.as_ref(),
                false,
                proj,
            );
            self.gl.bind_vertex_array(Some(self.quad_vao));
            self.gl.uniform_4_f32(
                self.border_uniforms.border_color.as_ref(),
                1.0,
                0.2,
                0.12,
                0.95,
            );
            self.gl
                .uniform_1_f32(self.border_uniforms.radius.as_ref(), 2.0);
            self.gl
                .uniform_2_f32(self.border_uniforms.size.as_ref(), width, height);
            self.gl
                .uniform_4_f32(self.border_uniforms.rect.as_ref(), x, y, width, height);
            self.gl
                .uniform_1_f32(self.border_uniforms.border_width.as_ref(), 3.0);
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

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
                self.gl
                    .uniform_2_f32(self.border_uniforms.size.as_ref(), handle_size, handle_size);
                self.gl.uniform_4_f32(
                    self.border_uniforms.rect.as_ref(),
                    handle_x - handle_size * 0.5,
                    handle_y - handle_size * 0.5,
                    handle_size,
                    handle_size,
                );
                self.gl
                    .uniform_1_f32(self.border_uniforms.border_width.as_ref(), handle_size);
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            }

            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
        }
    }

    // =========================================================================
    // 5.3 Window Peek (Boss Key)
    // =========================================================================

    /// Toggle peek mode. When active, all windows fade to transparent.
    pub(crate) fn set_peek_mode(&mut self, active: bool) {
        if !self.peek_enabled {
            return;
        }
        if active == self.peek_active {
            return;
        }
        self.peek_active = active;
        self.peek_start = Some(std::time::Instant::now());
        self.needs_render = true;
    }

    /// Tick peek animation. Returns true if still animating.
    pub(super) fn tick_peek(&mut self) -> bool {
        if self.peek_start.is_none() {
            return false;
        }
        let dt = 1.0 / 60.0_f32;
        let speed = 5.0_f32;

        if self.peek_active {
            // Fade out: 1.0 -> 0.0
            self.peek_opacity = (self.peek_opacity - dt * speed).max(0.0);
            if self.peek_opacity <= 0.0 {
                self.peek_opacity = 0.0;
                self.peek_start = None;
                return false;
            }
        } else {
            // Fade in: 0.0 -> 1.0
            self.peek_opacity = (self.peek_opacity + dt * speed).min(1.0);
            if self.peek_opacity >= 1.0 {
                self.peek_opacity = 1.0;
                self.peek_start = None;
                return false;
            }
        }
        true
    }

    /// Returns the peek opacity multiplier for a given window class.
    /// Excluded windows maintain full opacity.
    pub(super) fn peek_opacity_for(&self, class_name: &str) -> f32 {
        if !self.peek_active && self.peek_opacity >= 1.0 {
            return 1.0;
        }
        if class_matches_exclude(class_name, &self.peek_exclude) {
            return 1.0;
        }
        self.peek_opacity
    }

    // =========================================================================
    // 5.4 Window Tabs Rendering
    // =========================================================================

    /// Set window tab groups from the WM.
    pub(crate) fn set_window_groups(&mut self, groups: Vec<(u32, Vec<(u32, String, bool)>)>) {
        self.window_groups.clear();
        for (group_id, tabs) in groups {
            let tab_entries: Vec<WindowTab> = tabs
                .into_iter()
                .map(|(win, title, active)| WindowTab {
                    x11_win: win,
                    title,
                    is_active: active,
                })
                .collect();
            self.window_groups.insert(group_id, tab_entries);
        }
        self.needs_render = true;
    }

    /// Find the tab group that a window belongs to (as active tab).
    pub(super) fn find_window_group(&self, x11_win: u32) -> Option<(u32, &[WindowTab])> {
        for (&group_id, tabs) in &self.window_groups {
            for tab in tabs {
                if tab.x11_win == x11_win && tab.is_active {
                    return Some((group_id, tabs.as_slice()));
                }
            }
        }
        None
    }

    /// Render tab bar for a window. Called from render_frame after drawing a window.
    pub(super) fn render_tab_bar(
        &self,
        proj: &[f32; 16],
        win_x: f32,
        win_y: f32,
        win_w: f32,
        tabs: &[WindowTab],
    ) {
        if !self.window_tabs_enabled || tabs.len() <= 1 {
            return;
        }

        let bar_h = self.tab_bar_height;
        let bar_y = win_y - bar_h;
        let tab_w = win_w / tabs.len() as f32;

        unsafe {
            // Draw tab bar using the border shader as a filled rect
            self.gl.use_program(Some(self.border_program));
            self.gl.uniform_matrix_4_f32_slice(
                self.border_uniforms.projection.as_ref(),
                false,
                proj,
            );
            self.gl.bind_vertex_array(Some(self.quad_vao));

            for (i, tab) in tabs.iter().enumerate() {
                let tx = win_x + i as f32 * tab_w;
                let color = if tab.is_active {
                    self.tab_active_color
                } else {
                    self.tab_bar_color
                };

                // Draw filled rectangle for each tab
                let fill_size = tab_w.max(bar_h);
                self.gl
                    .uniform_1_f32(self.border_uniforms.border_width.as_ref(), fill_size);
                self.gl.uniform_4_f32(
                    self.border_uniforms.border_color.as_ref(),
                    color[0],
                    color[1],
                    color[2],
                    color[3],
                );
                self.gl
                    .uniform_1_f32(self.border_uniforms.radius.as_ref(), 0.0);
                self.gl
                    .uniform_2_f32(self.border_uniforms.size.as_ref(), tab_w, bar_h);
                self.gl
                    .uniform_4_f32(self.border_uniforms.rect.as_ref(), tx, bar_y, tab_w, bar_h);
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            }

            // Draw tab titles using bitmap font
            for (i, tab) in tabs.iter().enumerate() {
                let tx = win_x + i as f32 * tab_w;
                let max_title_w = (tab_w - 4.0).max(20.0) as u32;
                if let Some((pixels, tw, th)) =
                    Self::render_title_to_pixels(&tab.title, max_title_w)
                {
                    // Upload title texture
                    let title_tex = self.gl.create_texture().ok();
                    if let Some(tex) = title_tex {
                        self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                        self.gl.tex_image_2d(
                            glow::TEXTURE_2D,
                            0,
                            glow::RGBA8 as i32,
                            tw as i32,
                            th as i32,
                            0,
                            glow::RGBA,
                            glow::UNSIGNED_BYTE,
                            glow::PixelUnpackData::Slice(Some(&pixels)),
                        );
                        self.gl.tex_parameter_i32(
                            glow::TEXTURE_2D,
                            glow::TEXTURE_MIN_FILTER,
                            glow::LINEAR as i32,
                        );
                        self.gl.tex_parameter_i32(
                            glow::TEXTURE_2D,
                            glow::TEXTURE_MAG_FILTER,
                            glow::LINEAR as i32,
                        );

                        // Draw title using window program
                        self.gl.use_program(Some(self.program));
                        self.gl.uniform_matrix_4_f32_slice(
                            self.win_uniforms.projection.as_ref(),
                            false,
                            proj,
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
                            .uniform_1_f32(self.win_uniforms.opacity.as_ref(), -1.0);
                        self.gl
                            .uniform_1_f32(self.win_uniforms.radius.as_ref(), 0.0);
                        self.gl.uniform_1_f32(self.win_uniforms.dim.as_ref(), 1.0);

                        let title_x = tx + (tab_w - tw as f32) * 0.5;
                        let title_y = bar_y + (bar_h - th as f32) * 0.5;
                        self.gl.uniform_2_f32(
                            self.win_uniforms.size.as_ref(),
                            tw as f32,
                            th as f32,
                        );
                        self.gl.uniform_4_f32(
                            self.win_uniforms.rect.as_ref(),
                            title_x,
                            title_y,
                            tw as f32,
                            th as f32,
                        );
                        self.gl.active_texture(glow::TEXTURE0);
                        self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                        // Clean up temp texture
                        self.gl.delete_texture(tex);

                        // Restore border program for next tab rect
                        self.gl.use_program(Some(self.border_program));
                        self.gl.uniform_matrix_4_f32_slice(
                            self.border_uniforms.projection.as_ref(),
                            false,
                            proj,
                        );
                    }
                }
            }

            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
        }
    }

    // =========================================================================
    // 5.5 Live Window Thumbnail API
    // =========================================================================

    /// Request a live thumbnail for a window. Delegates to capture_window_thumbnail.
    /// Future: add caching logic.
    pub(crate) fn request_live_thumbnail(
        &self,
        x11_win: u32,
        max_size: u32,
    ) -> Option<(Vec<u8>, u32, u32)> {
        self.capture_window_thumbnail(x11_win, max_size)
    }

    // =========================================================================
    // 5.6 Window Tab Click Detection
    // =========================================================================

    /// Check if a click at (x, y) is on a tab bar and return the tab group and index.
    /// Returns Some((group_id, tab_index)) if click is on a tab, None otherwise.
    #[allow(dead_code)]
    pub(super) fn check_tab_click(&self, x: f32, y: f32) -> Option<(u32, usize)> {
        if !self.window_tabs_enabled {
            return None;
        }

        // Check all window groups for tab hit
        for (&group_id, tabs) in &self.window_groups {
            if tabs.is_empty() {
                continue;
            }

            // Find the active tab's window to get its position
            for tab in tabs {
                if !tab.is_active {
                    continue;
                }

                if let Some(wt) = self.windows.get(&tab.x11_win) {
                    let win_x = wt.x as f32;
                    let win_y = wt.y as f32;
                    let win_w = wt.w as f32;
                    let bar_h = self.tab_bar_height;
                    let bar_y = win_y - bar_h;

                    // Check if click is within tab bar bounds
                    if x >= win_x && x < win_x + win_w && y >= bar_y && y < win_y {
                        // Find which tab was clicked
                        let tab_w = win_w / tabs.len() as f32;
                        let clicked_index = ((x - win_x) / tab_w) as usize;
                        if clicked_index < tabs.len() {
                            return Some((group_id, clicked_index));
                        }
                    }
                    break;
                }
            }
        }
        None
    }
}
