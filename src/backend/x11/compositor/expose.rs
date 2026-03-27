use glow::HasContext;
use super::{Compositor, ExposeEntry, SnapPreview, WindowTab};

impl Compositor {
    // =========================================================================
    // 5.1 Expose / Mission Control mode
    // =========================================================================

    /// Activate or deactivate expose mode.
    /// `windows` contains (x11_win, x, y, w, h) for each window to arrange.
    pub(in crate::backend::x11) fn set_expose_mode(
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

            let sw = self.screen_w as f32;
            let sh = self.screen_h as f32;
            let gap = self.expose_gap;
            let screen_aspect = sw / sh;

            // Compute grid layout
            let cols = ((n as f32 * screen_aspect).sqrt()).ceil() as u32;
            let cols = cols.max(1);
            let rows = ((n as u32 + cols - 1) / cols).max(1);

            let cell_w = (sw - gap * (cols as f32 + 1.0)) / cols as f32;
            let cell_h = (sh - gap * (rows as f32 + 1.0)) / rows as f32;

            self.expose_entries = windows
                .iter()
                .enumerate()
                .map(|(i, &(win, x, y, w, h))| {
                    let col = i as u32 % cols;
                    let row = i as u32 / cols;

                    let cell_x = gap + col as f32 * (cell_w + gap);
                    let cell_y = gap + row as f32 * (cell_h + gap);

                    // Scale window to fit cell preserving aspect ratio
                    let win_aspect = w as f32 / h.max(1) as f32;
                    let cell_aspect = cell_w / cell_h;
                    let (tw, th) = if win_aspect > cell_aspect {
                        (cell_w, cell_w / win_aspect)
                    } else {
                        (cell_h * win_aspect, cell_h)
                    };
                    // Center in cell
                    let tx = cell_x + (cell_w - tw) * 0.5;
                    let ty = cell_y + (cell_h - th) * 0.5;

                    ExposeEntry {
                        x11_win: win,
                        orig_x: x as f32,
                        orig_y: y as f32,
                        orig_w: w as f32,
                        orig_h: h as f32,
                        target_x: tx,
                        target_y: ty,
                        target_w: tw,
                        target_h: th,
                        current_x: x as f32,
                        current_y: y as f32,
                        current_w: w as f32,
                        current_h: h as f32,
                        is_hovered: false,
                    }
                })
                .collect();

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

        let dt = 1.0 / 60.0_f32;
        let ease_speed = 12.0_f32;
        let t = 1.0 - (-ease_speed * dt).exp();
        let mut any_moving = false;

        if self.expose_active {
            // Fade in
            self.expose_opacity = (self.expose_opacity + dt * 4.0).min(1.0);
            if self.expose_opacity < 1.0 {
                any_moving = true;
            }

            // Animate current -> target
            for entry in &mut self.expose_entries {
                let dx = entry.target_x - entry.current_x;
                let dy = entry.target_y - entry.current_y;
                let dw = entry.target_w - entry.current_w;
                let dh = entry.target_h - entry.current_h;

                if dx.abs() > 0.5 || dy.abs() > 0.5 || dw.abs() > 0.5 || dh.abs() > 0.5 {
                    entry.current_x += dx * t;
                    entry.current_y += dy * t;
                    entry.current_w += dw * t;
                    entry.current_h += dh * t;
                    any_moving = true;
                } else {
                    entry.current_x = entry.target_x;
                    entry.current_y = entry.target_y;
                    entry.current_w = entry.target_w;
                    entry.current_h = entry.target_h;
                }
            }
        } else {
            // Animate current -> orig
            for entry in &mut self.expose_entries {
                let dx = entry.orig_x - entry.current_x;
                let dy = entry.orig_y - entry.current_y;
                let dw = entry.orig_w - entry.current_w;
                let dh = entry.orig_h - entry.current_h;

                if dx.abs() > 0.5 || dy.abs() > 0.5 || dw.abs() > 0.5 || dh.abs() > 0.5 {
                    entry.current_x += dx * t;
                    entry.current_y += dy * t;
                    entry.current_w += dw * t;
                    entry.current_h += dh * t;
                    any_moving = true;
                } else {
                    entry.current_x = entry.orig_x;
                    entry.current_y = entry.orig_y;
                    entry.current_w = entry.orig_w;
                    entry.current_h = entry.orig_h;
                }
            }

            // Fade out the dark overlay faster than the position animation,
            // so it disappears before windows reach their original positions.
            // This avoids a visible flash of dark overlay at the end.
            let fade_speed = if any_moving { 8.0 } else { 20.0 };
            self.expose_opacity = (self.expose_opacity - dt * fade_speed).max(0.0);
            if self.expose_opacity > 0.0 {
                any_moving = true;
            }

            // Clean up when animation finishes
            if self.expose_opacity <= 0.0 && !any_moving {
                self.expose_entries.clear();
                return false;
            }
        }

        any_moving || self.expose_opacity > 0.0 && self.expose_opacity < 1.0
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
            self.gl.uniform_matrix_4_f32_slice(
                self.win_uniforms.projection.as_ref(),
                false,
                proj,
            );
            self.gl
                .uniform_1_i32(self.win_uniforms.texture.as_ref(), 0);
            self.gl.uniform_4_f32(
                self.win_uniforms.uv_rect.as_ref(),
                0.0,
                0.0,
                1.0,
                1.0,
            );
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
                self.gl
                    .uniform_1_f32(self.win_uniforms.dim.as_ref(), 1.0);
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
                self.gl
                    .bind_texture(glow::TEXTURE_2D, Some(wt.gl_texture));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                // Highlight border if hovered
                if entry.is_hovered {
                    self.gl.use_program(Some(self.border_program));
                    self.gl.uniform_matrix_4_f32_slice(
                        self.border_uniforms.projection.as_ref(),
                        false,
                        proj,
                    );
                    self.gl.uniform_1_f32(
                        self.border_uniforms.border_width.as_ref(),
                        3.0,
                    );
                    self.gl.uniform_4_f32(
                        self.border_uniforms.border_color.as_ref(),
                        0.4,
                        0.6,
                        1.0,
                        opacity,
                    );
                    self.gl.uniform_1_f32(
                        self.border_uniforms.radius.as_ref(),
                        self.corner_radius,
                    );
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
                    self.gl
                        .uniform_1_i32(self.win_uniforms.texture.as_ref(), 0);
                    self.gl.uniform_4_f32(
                        self.win_uniforms.uv_rect.as_ref(),
                        0.0,
                        0.0,
                        1.0,
                        1.0,
                    );
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
    pub(in crate::backend::x11) fn expose_click(&mut self, x: f32, y: f32) -> Option<u32> {
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
    pub(in crate::backend::x11) fn set_snap_preview(&mut self, preview: Option<(f32, f32, f32, f32)>) {
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
            self.gl.uniform_4_f32(
                self.border_uniforms.border_color.as_ref(),
                r,
                g,
                b,
                alpha,
            );
            self.gl
                .uniform_1_f32(self.border_uniforms.radius.as_ref(), self.corner_radius);
            self.gl
                .uniform_2_f32(self.border_uniforms.size.as_ref(), sp.w, sp.h);
            self.gl.uniform_4_f32(
                self.border_uniforms.rect.as_ref(),
                sp.x,
                sp.y,
                sp.w,
                sp.h,
            );
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

    // =========================================================================
    // 5.3 Window Peek (Boss Key)
    // =========================================================================

    /// Toggle peek mode. When active, all windows fade to transparent.
    pub(in crate::backend::x11) fn set_peek_mode(&mut self, active: bool) {
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
        if Self::class_matches_exclude(class_name, &self.peek_exclude) {
            return 1.0;
        }
        self.peek_opacity
    }

    // =========================================================================
    // 5.4 Window Tabs Rendering
    // =========================================================================

    /// Set window tab groups from the WM.
    pub(in crate::backend::x11) fn set_window_groups(
        &mut self,
        groups: Vec<(u32, Vec<(u32, String, bool)>)>,
    ) {
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
                self.gl.uniform_1_f32(
                    self.border_uniforms.border_width.as_ref(),
                    fill_size,
                );
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
                self.gl.uniform_4_f32(
                    self.border_uniforms.rect.as_ref(),
                    tx,
                    bar_y,
                    tab_w,
                    bar_h,
                );
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
                        self.gl
                            .uniform_1_i32(self.win_uniforms.texture.as_ref(), 0);
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
    pub(in crate::backend::x11) fn request_live_thumbnail(
        &self,
        x11_win: u32,
        max_size: u32,
    ) -> Option<(Vec<u8>, u32, u32)> {
        self.capture_window_thumbnail(x11_win, max_size)
    }
}
