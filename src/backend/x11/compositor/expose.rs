use super::{Compositor, SnapPreview, class_matches_exclude};
use crate::backend::api::ExposeNavDirection;
use crate::backend::compositor_common::ui_theme;
use crate::backend::compositor_common::window_tabs::{self, TabGroup};
use crate::backend::compositor_font;
use crate::backend::x11::compositor_common::expose::{
    build_expose_entries, expose_grid_cols, move_expose_selection, tick_expose_entries,
};
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
                let wt = match self.windows.get(&entry.id) {
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
                    self.set_border_radii(self.corner_radius, self.corner_radius);
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
        let hit_id = self
            .expose_entries
            .iter()
            .find(|entry| {
                x >= entry.current_x
                    && x <= entry.current_x + entry.current_w
                    && y >= entry.current_y
                    && y <= entry.current_y + entry.current_h
            })
            .map(|entry| entry.id);
        self.expose_select_id(hit_id);
    }

    /// Highlight the expose entry for `id` (`None` clears the highlight).
    /// Mouse hover and keyboard selection share this single highlight.
    pub(crate) fn expose_select_id(&mut self, id: Option<u32>) {
        let mut changed = false;
        for entry in &mut self.expose_entries {
            let should_hover = Some(entry.id) == id;
            if entry.is_hovered != should_hover {
                entry.is_hovered = should_hover;
                changed = true;
            }
        }
        if changed {
            self.needs_render = true;
        }
    }

    /// Move the expose highlight one grid step in `dir`.
    pub(crate) fn expose_move_selection(&mut self, dir: ExposeNavDirection) {
        let current = self
            .expose_entries
            .iter()
            .position(|entry| entry.is_hovered);
        let len = self.expose_entries.len();
        let cols = expose_grid_cols(len, self.screen_w as f32, self.screen_h as f32);
        let selected = move_expose_selection(current, dir, len, cols)
            .map(|index| self.expose_entries[index].id);
        self.expose_select_id(selected);
    }

    /// The currently highlighted expose entry's window, if any.
    pub(crate) fn expose_selected(&self) -> Option<u32> {
        self.expose_entries
            .iter()
            .find(|entry| entry.is_hovered)
            .map(|entry| entry.id)
    }

    /// Handle click in expose mode. Returns the x11_win of the clicked window.
    pub(crate) fn expose_click(&mut self, x: f32, y: f32) -> Option<u32> {
        let result = self.expose_entries.iter().find_map(|entry| {
            if x >= entry.current_x
                && x <= entry.current_x + entry.current_w
                && y >= entry.current_y
                && y <= entry.current_y + entry.current_h
            {
                Some(entry.id)
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
            let (opacity, still_animating) =
                snap_preview_animation_state(elapsed, duration_ms, sp.fading_out);
            sp.opacity = opacity;
            if sp.fading_out {
                if !still_animating {
                    self.snap_target = None;
                    return false;
                }
            }
            still_animating
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
            self.set_border_radii(self.corner_radius, self.corner_radius);
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
            self.set_border_radii(2.0, 2.0);
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

    /// Take the tab bars the window manager reserved. Each group carries the
    /// strip the layout made room for, so the compositor paints where there is
    /// space instead of deriving a position from a window's geometry — which
    /// is what used to put the bar on top of the status bar.
    pub(crate) fn set_window_groups(&mut self, groups: Vec<TabGroup>) {
        if self.window_groups == groups {
            return;
        }
        self.window_groups = groups;
        // Re-derive the hovered cell against the new layout: the tab under
        // the pointer may sit at another index now, or be gone entirely.
        self.tab_hover = window_tabs::tab_hover_at(&self.window_groups, self.mouse_x, self.mouse_y);
        // A group change is the only thing that can invalidate a title: the
        // text, the cell width and the focus flag all live in it.
        self.tab_titles_dirty = true;
        self.needs_render = true;
    }

    /// Rasterise and upload every tab title, once per change. Doing it per
    /// frame instead — a CPU text raster plus a texture create, upload and
    /// delete for every tab — is pure waste on a desktop that is not moving.
    ///
    /// Titles are drawn in the UI theme's ink with the configured system-UI
    /// font, the same as every other surface the compositor owns, so the ink
    /// is part of what a change invalidates: the focused cell reads in
    /// `title_ink` and the rest in the dimmer `label_ink`.
    pub(super) fn refresh_tab_titles(&mut self) {
        if !self.tab_titles_dirty {
            return;
        }
        self.tab_titles_dirty = false;

        let stale = std::mem::take(&mut self.tab_title_textures);
        unsafe {
            for (texture, _, _) in stale.into_iter().flatten().flatten() {
                self.gl.delete_texture(texture);
            }
        }

        let ui = ui_theme::palette();
        let config = crate::config::CONFIG.load();
        let font = config.system_ui_font();

        let mut cache = Vec::with_capacity(self.window_groups.len());
        for group in &self.window_groups {
            let count = group.tabs.len();
            let mut row = Vec::with_capacity(count);
            for (index, tab) in group.tabs.iter().enumerate() {
                row.push(window_tabs::cell_rect(group.bar, count, index).and_then(
                    |[_, _, cell_w, cell_h]| {
                        // The strip's height is configurable, so the type is
                        // sized from the cell rather than from the system-UI
                        // font's own size, which would overflow it.
                        let size = window_tabs::title_font_size(cell_h);
                        let budget = window_tabs::title_budget(cell_w);
                        let text = compositor_font::fit_ui_text(&tab.title, font, size, budget);
                        if text.is_empty() {
                            return None;
                        }
                        let ink = if tab.active {
                            ui.title_ink
                        } else {
                            ui.label_ink
                        };
                        let (pixels, w, h) =
                            compositor_font::render_ui_text_to_rgba(&text, font, size, ink);
                        if w == 0 || h == 0 {
                            return None;
                        }
                        let texture = unsafe { self.upload_tab_title(&pixels, w, h) }?;
                        Some((texture, w, h))
                    },
                ));
            }
            cache.push(row);
        }
        self.tab_title_textures = cache;
    }

    unsafe fn upload_tab_title(&self, pixels: &[u8], w: u32, h: u32) -> Option<glow::Texture> {
        unsafe {
            let texture = self.gl.create_texture().ok()?;
            self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            self.gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                w as i32,
                h as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(pixels)),
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
            self.gl.bind_texture(glow::TEXTURE_2D, None);
            Some(texture)
        }
    }

    /// Paint every tab bar. Called from render_frame once the windows are down.
    ///
    /// The strip is one of JWM's own surfaces, so it is drawn like the rest of
    /// them: a rounded track in the theme's card tone — frosted over a blurred
    /// backdrop under the glass themes, flat under Material — carrying one
    /// pill per window, the focused one raised as a chip and washed with the
    /// same accent the launcher marks its selected row with.
    ///
    /// Taking `&mut self` is what the frosted themes cost: the track samples
    /// the blurred scene, and that capture has to happen before the first
    /// cell is filled.
    pub(super) fn render_tab_bar(&mut self, proj: &[f32; 16]) {
        let ui = ui_theme::palette();
        self.ensure_glass_backdrop(ui);
        let accent = self.border_gradient_color_a;
        let tab_hover = self.tab_hover;
        let hover_scale = ui_theme::TAB_HOVER_ALPHA_SCALE;

        unsafe {
            self.gl.bind_vertex_array(Some(self.quad_vao));

            for (group_index, group) in self.window_groups.iter().enumerate() {
                let count = group.tabs.len();
                if !window_tabs::wants_bar(count) {
                    continue;
                }
                let Some([tx, ty, tw, th]) = window_tabs::track_rect(group.bar) else {
                    continue;
                };

                // Track first, then every cell: a title must never end up
                // under the neighbouring cell's fill. `ui_fill_island` leaves
                // the border program bound for the pills that follow.
                let track_radius = window_tabs::pill_radius(th);
                self.ui_fill_island(
                    proj,
                    ui,
                    tx,
                    ty,
                    tw,
                    th,
                    track_radius,
                    track_radius,
                    ui.card,
                    1.0,
                );

                for (index, tab) in group.tabs.iter().enumerate() {
                    // The focused cell is drawn raised; the hovered one takes
                    // the same chip at half strength so the pointer's target
                    // shows without competing with the focus. Anything else
                    // is the track showing through, which is what makes the
                    // raised cells read as lifted out of it. A hover index
                    // that outlived its group simply matches nothing here.
                    let hovered = tab_hover == Some((group_index, index));
                    if !tab.active && !hovered {
                        continue;
                    }
                    let Some([x, y, w, h]) = window_tabs::cell_rect(group.bar, count, index) else {
                        continue;
                    };
                    let radius = window_tabs::pill_radius(h);
                    if tab.active {
                        self.sysui_fill_rounded(x, y, w, h, radius, ui.chip);
                        self.sysui_fill_rounded(
                            x,
                            y,
                            w,
                            h,
                            radius,
                            [accent[0], accent[1], accent[2], ui.selection_alpha],
                        );
                    } else {
                        self.sysui_fill_rounded(
                            x,
                            y,
                            w,
                            h,
                            radius,
                            [ui.chip[0], ui.chip[1], ui.chip[2], ui.chip[3] * hover_scale],
                        );
                        self.sysui_fill_rounded(
                            x,
                            y,
                            w,
                            h,
                            radius,
                            [
                                accent[0],
                                accent[1],
                                accent[2],
                                ui.selection_alpha * hover_scale,
                            ],
                        );
                    }
                }

                let Some(titles) = self.tab_title_textures.get(group_index) else {
                    continue;
                };
                self.gl.use_program(Some(self.hud_text_program));
                self.gl.uniform_matrix_4_f32_slice(
                    self.hud_text_uniforms.projection.as_ref(),
                    false,
                    proj,
                );
                self.gl
                    .uniform_1_i32(self.hud_text_uniforms.texture.as_ref(), 0);
                self.gl
                    .uniform_1_f32(self.hud_text_uniforms.opacity.as_ref(), 1.0);
                self.gl.active_texture(glow::TEXTURE0);
                for (index, slot) in titles.iter().enumerate() {
                    let Some((texture, tw, th)) = slot else {
                        continue;
                    };
                    let Some([x, y, w, h]) = window_tabs::cell_rect(group.bar, count, index) else {
                        continue;
                    };
                    let (tw, th) = (*tw as f32, *th as f32);
                    self.gl.uniform_4_f32(
                        self.hud_text_uniforms.rect.as_ref(),
                        (x + (w - tw) * 0.5).round(),
                        (y + (h - th) * 0.5).round(),
                        tw,
                        th,
                    );
                    self.gl.bind_texture(glow::TEXTURE_2D, Some(*texture));
                    self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
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
}

fn snap_preview_animation_state(
    elapsed_ms: f32,
    duration_ms: f32,
    fading_out: bool,
) -> (f32, bool) {
    let t = (elapsed_ms / duration_ms.max(1.0)).clamp(0.0, 1.0);
    let opacity = if fading_out { 1.0 - t } else { t };
    (opacity, t < 1.0)
}

#[cfg(test)]
mod tests {
    use super::snap_preview_animation_state;

    #[test]
    fn snap_preview_stops_animating_at_steady_state() {
        assert_eq!(
            snap_preview_animation_state(50.0, 100.0, false),
            (0.5, true)
        );
        assert_eq!(
            snap_preview_animation_state(100.0, 100.0, false),
            (1.0, false)
        );
        assert_eq!(
            snap_preview_animation_state(150.0, 100.0, false),
            (1.0, false)
        );
    }

    #[test]
    fn snap_preview_fade_out_finishes_transparent() {
        assert_eq!(snap_preview_animation_state(50.0, 100.0, true), (0.5, true));
        assert_eq!(
            snap_preview_animation_state(100.0, 100.0, true),
            (0.0, false)
        );
    }
}
