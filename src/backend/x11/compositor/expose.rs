use super::{Compositor, SnapPreview, class_matches_exclude};
use crate::backend::api::ExposeNavDirection;
use crate::backend::compositor_common::expose::expose_label_origin;
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
    /// `windows` contains (x11_win, x, y, w, h, title) for each window to
    /// arrange; the sanitized title labels the thumbnail, an empty one
    /// draws no label.
    pub(crate) fn set_expose_mode(
        &mut self,
        active: bool,
        windows: Vec<(u32, i32, i32, u32, u32, String)>,
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
                windows,
            );
            // A new entry set is a new grid: the hover cue starts over from
            // nothing rather than resuming at full strength on a cell the
            // previous exposé left hovered.
            self.expose_hover_ease.clear();
            // The label textures derive from the entry set, so they rebuild
            // with it; the fly-in animation itself never invalidates them
            // (the raster is fitted to the settled cell width).
            self.refresh_expose_title_textures();

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
    pub(super) fn render_expose(&mut self, proj: &[f32; 16]) {
        if self.expose_entries.is_empty() || self.expose_opacity <= 0.0 {
            return;
        }

        // The hover ring eases in on the cell under the pointer rather than
        // snapping on; hover leaving snaps it off the same frame. Overlay
        // only — hit-testing keeps the unscaled entry geometry.
        let hovered_id = self.expose_selected();
        let hover_p = self.expose_hover_ease.advance_with_motion(
            std::time::Instant::now(),
            hovered_id,
            crate::config::CONFIG.load().motion_enabled(),
        );
        if self.expose_hover_ease.animating() {
            self.needs_render = true;
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
            // The window program's uniforms are sticky: the main pass leaves
            // the last window's `desat` (and any ripple) behind, so the
            // thumbnails would take on whatever was on top of the stack.
            self.gl.uniform_1_f32(self.win_uniforms.desat.as_ref(), 0.0);
            self.gl
                .uniform_1_f32(self.win_uniforms.ripple_progress.as_ref(), -1.0);
            self.gl
                .uniform_1_f32(self.win_uniforms.ripple_amplitude.as_ref(), 0.0);

            // When exiting (expose_active=false), keep windows fully opaque
            // so only the dark overlay fades — avoids a dim flash at the end.
            // The title labels take the same scalar.
            let opacity = if self.expose_active {
                self.expose_opacity
            } else {
                1.0
            };

            for entry in &self.expose_entries {
                let wt = match self.windows.get(&entry.id) {
                    Some(wt) => wt,
                    None => continue,
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

                // Highlight border while hovered, fading in with the ease
                // above and snapping off with it.
                if entry.is_hovered && hover_p > 0.0 {
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
                        opacity * hover_p,
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

            // Title labels come last, under one text-program bind for the
            // whole grid: a label must never end up under a neighbour's
            // thumbnail or hover ring while cells are still flying in. A
            // label is pure overlay — hit-testing still sees only the entry
            // geometry — riding the expose fade exactly like the thumbnails.
            if !self.expose_title_textures.is_empty() {
                self.gl.use_program(Some(self.hud_text_program));
                self.gl.uniform_matrix_4_f32_slice(
                    self.hud_text_uniforms.projection.as_ref(),
                    false,
                    proj,
                );
                self.gl
                    .uniform_1_i32(self.hud_text_uniforms.texture.as_ref(), 0);
                self.gl
                    .uniform_1_f32(self.hud_text_uniforms.opacity.as_ref(), opacity);
                self.gl.active_texture(glow::TEXTURE0);
                for (entry, slot) in self
                    .expose_entries
                    .iter()
                    .zip(self.expose_title_textures.iter())
                {
                    let Some((texture, tw, th)) = slot else {
                        continue;
                    };
                    let (tw, th) = (*tw as f32, *th as f32);
                    // Cells whose in-flight thumbnail is still narrower than
                    // the rasterised label draw nothing until they settle.
                    let Some((lx, ly)) =
                        expose_label_origin(entry.current_x, entry.current_y, entry.current_w, tw)
                    else {
                        continue;
                    };
                    self.gl.uniform_4_f32(
                        self.hud_text_uniforms.rect.as_ref(),
                        lx.round(),
                        ly.round(),
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

    /// Rasterise and upload every expose entry's title, once per entry-set
    /// change rather than once per frame — the same bargain
    /// [`Self::refresh_tab_titles`] strikes.
    ///
    /// The raster is fitted against the cell the fly-in animation settles
    /// into (`target_w`), never the in-flight rect, so pure geometry motion
    /// cannot invalidate the cache. Labels use the configured system-UI font
    /// at the cube overview's window-title size and the theme's `title_ink`,
    /// so an expose label reads as the same text the other overviews draw.
    pub(super) fn refresh_expose_title_textures(&mut self) {
        let stale = std::mem::take(&mut self.expose_title_textures);
        unsafe {
            for (texture, _, _) in stale.into_iter().flatten() {
                self.gl.delete_texture(texture);
            }
        }

        let ui = ui_theme::palette();
        let config = crate::config::CONFIG.load();
        let font = config.system_ui_font();
        let size = compositor_font::ui_font_pixel_size(font);

        let mut cache = Vec::with_capacity(self.expose_entries.len());
        for entry in &self.expose_entries {
            // A `None` slot is "no label": an empty title, a title too long
            // for even the ellipsis, or a failed upload all draw nothing
            // rather than an empty box.
            let budget = window_tabs::title_budget(entry.target_w);
            let text = compositor_font::fit_ui_text(&entry.title, font, size, budget);
            let slot = if text.is_empty() {
                None
            } else {
                let (pixels, w, h) =
                    compositor_font::render_ui_text_to_rgba(&text, font, size, ui.title_ink);
                if w == 0 || h == 0 {
                    None
                } else {
                    unsafe { self.upload_text_texture(&pixels, w, h) }
                        .map(|texture| (texture, w, h))
                }
            };
            cache.push(slot);
        }
        self.expose_title_textures = cache;
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
        self.tab_hover = tab_hover_for_pointer(
            &self.window_groups,
            self.pointer_seen.then_some((self.mouse_x, self.mouse_y)),
        );
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
            // The chip's cached line derives from the same titles (and the
            // same font and ink), so it goes with them and re-rasterises
            // lazily the next time the dwell shows it.
            if let Some((_, texture, _, _)) = self.tab_tooltip_texture.take() {
                self.gl.delete_texture(texture);
            }
        }

        let ui = ui_theme::palette();
        let config = crate::config::CONFIG.load();
        let font = config.system_ui_font();

        let mut cache = Vec::with_capacity(self.window_groups.len());
        let mut truncated = Vec::with_capacity(self.window_groups.len());
        for group in &self.window_groups {
            let count = group.tabs.len();
            let mut row = Vec::with_capacity(count);
            let mut truncated_row = Vec::with_capacity(count);
            for (index, tab) in group.tabs.iter().enumerate() {
                let mut cell_truncated = false;
                row.push(window_tabs::cell_rect(group.bar, count, index).and_then(
                    |[_, _, cell_w, cell_h]| {
                        // The strip's height is configurable, so the type is
                        // sized from the cell rather than from the system-UI
                        // font's own size, which would overflow it.
                        let size = window_tabs::title_font_size(cell_h);
                        let budget = window_tabs::title_budget(cell_w);
                        let text = compositor_font::fit_ui_text(&tab.title, font, size, budget);
                        // A cell whose raster differs from the whole line was
                        // cut — exactly the set that earns a dwell tooltip.
                        // The uncut reference costs one extra fit per title,
                        // paid once per refresh rather than per frame.
                        cell_truncated =
                            text != compositor_font::fit_ui_text(&tab.title, font, size, u32::MAX);
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
                        let texture = unsafe { self.upload_text_texture(&pixels, w, h) }?;
                        Some((texture, w, h))
                    },
                ));
                truncated_row.push(cell_truncated);
            }
            cache.push(row);
            truncated.push(truncated_row);
        }
        self.tab_title_textures = cache;
        self.tab_titles_truncated = truncated;
    }

    /// Upload rasterised UI text (tab titles, expose labels) as a texture.
    unsafe fn upload_text_texture(&self, pixels: &[u8], w: u32, h: u32) -> Option<glow::Texture> {
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
        // The hover chip eases in under the pointer like the other cues and
        // snaps off the frame the hover leaves.
        let hover_p = self.tab_hover_ease.advance_with_motion(
            std::time::Instant::now(),
            self.tab_hover,
            crate::config::CONFIG.load().motion_enabled(),
        );
        if self.tab_hover_ease.animating() {
            self.needs_render = true;
        }
        // The dwell tooltip rides the same channel: rest the pointer on a
        // truncated cell for `window_tabs::TOOLTIP_DWELL` and its full title
        // floats up in a chip off the strip. The dwell is policy, not motion,
        // so it advances regardless of the motion setting; only the chip's
        // fade eases.
        let tooltip_eligible = self.tab_hover.is_some_and(|(group_index, index)| {
            self.tab_titles_truncated
                .get(group_index)
                .and_then(|row| row.get(index))
                .copied()
                == Some(true)
        });
        self.tab_tooltip_dwell
            .advance(std::time::Instant::now(), self.tab_hover, tooltip_eligible);
        if self.tab_tooltip_dwell.needs_frame() {
            // The dwell lapses on a clock, not on an event: frames must keep
            // coming or an idle screen sleeps through the chip's due moment.
            self.needs_render = true;
        }
        let tooltip_key = self.tab_tooltip_dwell.visible_key();
        let tooltip_p = self.tab_tooltip_ease.advance_with_motion(
            std::time::Instant::now(),
            tooltip_key,
            crate::config::CONFIG.load().motion_enabled(),
        );
        if self.tab_tooltip_ease.animating() {
            self.needs_render = true;
        }
        let ui = ui_theme::palette();
        self.ensure_glass_backdrop(ui);
        let accent = self.border_gradient_color_a;
        let tab_hover = self.tab_hover;
        let hover_scale = ui_theme::TAB_HOVER_ALPHA_SCALE * hover_p;

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
                    // shows without competing with the focus, easing in with
                    // the envelope above. Anything else is the track showing
                    // through, which is what makes the raised cells read as
                    // lifted out of it. A hover index that outlived its group
                    // simply matches nothing here.
                    let hovered = tab_hover == Some((group_index, index)) && hover_p > 0.0;
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

        // The chip draws over every strip and under nothing: placed off the
        // band, it can never cover the cell whose clicks must still land.
        if let Some((group_index, index)) = tooltip_key
            && tooltip_p > 0.0
        {
            self.render_tab_tooltip(proj, group_index, index, tooltip_p);
        }
    }

    /// Float the full title of the truncated cell `(group_index, index)` in a
    /// chip off its strip, at `p` fade strength. Pure overlay: hit-testing is
    /// untouched, and [`window_tabs::tooltip_rect`] keeps the chip off the
    /// band and on the screen, so the pointer path and the click path are
    /// exactly as without it.
    fn render_tab_tooltip(&mut self, proj: &[f32; 16], group_index: usize, index: usize, p: f32) {
        let Some((bar, cell, title)) = self.window_groups.get(group_index).and_then(|group| {
            let cell = window_tabs::cell_rect(group.bar, group.tabs.len(), index)?;
            Some((group.bar, cell, group.tabs.get(index)?.title.clone()))
        }) else {
            return;
        };
        let config = crate::config::CONFIG.load();
        let font = config.system_ui_font();
        let size = compositor_font::ui_font_pixel_size(font);
        // The chip re-ellipsizes against its own budget: a title that would
        // run off the screen is still cut, just against the screen instead
        // of the cell.
        let text =
            compositor_font::fit_ui_text(&title, font, size, window_tabs::TOOLTIP_MAX_TEXT_WIDTH);
        if text.is_empty() {
            return;
        }
        self.update_tab_tooltip_texture(&text);
        let Some((texture, tw, th)) = self
            .tab_tooltip_texture
            .as_ref()
            .map(|&(_, texture, tw, th)| (texture, tw, th))
        else {
            return;
        };

        let chip_w = tw as f32 + 2.0 * window_tabs::TOOLTIP_PAD_X;
        let chip_h = th as f32 + 2.0 * window_tabs::TOOLTIP_PAD_Y;
        let Some([x, y, w, h]) = window_tabs::tooltip_rect(
            bar,
            cell,
            chip_w,
            chip_h,
            self.screen_w as f32,
            self.screen_h as f32,
        ) else {
            return;
        };

        let ui = ui_theme::palette();
        unsafe {
            self.gl.bind_vertex_array(Some(self.quad_vao));
            let radius = window_tabs::pill_radius(h);
            self.ui_fill_island(proj, ui, x, y, w, h, radius, radius, ui.osd, p);

            self.gl.use_program(Some(self.hud_text_program));
            self.gl.uniform_matrix_4_f32_slice(
                self.hud_text_uniforms.projection.as_ref(),
                false,
                proj,
            );
            self.gl
                .uniform_1_i32(self.hud_text_uniforms.texture.as_ref(), 0);
            self.gl
                .uniform_1_f32(self.hud_text_uniforms.opacity.as_ref(), p);
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.uniform_4_f32(
                self.hud_text_uniforms.rect.as_ref(),
                (x + window_tabs::TOOLTIP_PAD_X).round(),
                (y + (h - th as f32) * 0.5).round(),
                tw as f32,
                th as f32,
            );
            self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
        }
    }

    /// Rasterise the chip's one line, re-rendering only when the text
    /// changed — the same text-keyed bargain the OSD label strikes.
    /// [`Self::refresh_tab_titles`] frees the cache with the titles it
    /// derives from.
    fn update_tab_tooltip_texture(&mut self, text: &str) {
        if self
            .tab_tooltip_texture
            .as_ref()
            .is_some_and(|(cached, _, _, _)| cached == text)
        {
            return;
        }
        if let Some((_, texture, _, _)) = self.tab_tooltip_texture.take() {
            unsafe { self.gl.delete_texture(texture) };
        }
        let config = crate::config::CONFIG.load();
        let font = config.system_ui_font();
        let size = compositor_font::ui_font_pixel_size(font);
        let (pixels, w, h) =
            compositor_font::render_ui_text_to_rgba(text, font, size, ui_theme::palette().osd_ink);
        if w == 0 || h == 0 {
            return;
        }
        if let Some(texture) = unsafe { self.upload_text_texture(&pixels, w, h) } {
            self.tab_tooltip_texture = Some((text.to_string(), texture, w, h));
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

/// The tab cell to paint as hovered. `pointer` is `None` until a real pointer
/// position has been reported: `(0.0, 0.0)` is both the field's initial value
/// and a perfectly legal place for the pointer to be, so the coordinates alone
/// cannot tell "never saw the pointer" from "pointer at the origin". Without
/// the distinction, the first groups a freshly created compositor is handed
/// light the first cell of any strip that contains the origin — a hidden
/// status bar, or a bar that is not at the top — and the phantom hover sits
/// there until the user moves the mouse.
fn tab_hover_for_pointer(
    groups: &[TabGroup],
    pointer: Option<(f32, f32)>,
) -> Option<(usize, usize)> {
    let (x, y) = pointer?;
    window_tabs::tab_hover_at(groups, x, y)
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
    use super::{TabGroup, snap_preview_animation_state, tab_hover_for_pointer};
    use crate::backend::compositor_common::window_tabs::Tab;

    fn strip_at(bar: [f32; 4]) -> Vec<TabGroup> {
        vec![TabGroup {
            bar,
            tabs: vec![
                Tab {
                    title: "left".to_string(),
                    active: true,
                },
                Tab {
                    title: "right".to_string(),
                    active: false,
                },
            ],
        }]
    }

    #[test]
    fn an_unseen_pointer_hovers_no_tab_even_when_the_strip_holds_the_origin() {
        // A hidden status bar (or one anchored anywhere but the top) puts the
        // strip over (0, 0), which is also the compositor's initial pointer
        // value. Until a real position arrives, no cell may light up.
        let groups = strip_at([0.0, 0.0, 800.0, 24.0]);
        assert_eq!(tab_hover_for_pointer(&groups, None), None);
        assert_eq!(
            tab_hover_for_pointer(&groups, Some((0.0, 0.0))),
            Some((0, 0))
        );
    }

    #[test]
    fn a_seen_pointer_still_reports_the_cell_it_is_over() {
        let groups = strip_at([100.0, 40.0, 800.0, 24.0]);
        assert_eq!(
            tab_hover_for_pointer(&groups, Some((700.0, 50.0))),
            Some((0, 1))
        );
        assert_eq!(tab_hover_for_pointer(&groups, Some((10.0, 10.0))), None);
        // A strip away from the origin cannot be hovered by the initial
        // value either, so the gate changes nothing for the common case.
        assert_eq!(tab_hover_for_pointer(&groups, None), None);
    }

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

    /// The dwell chip appears on a clock, not on an event, so the strip's
    /// render path must keep the frame loop alive while a rest is pending —
    /// an idle screen would otherwise sleep through the moment the chip is
    /// due. Pin the wiring: the truncation set is recorded at title refresh,
    /// the live hover feeds the dwell every drawn frame, a pending dwell
    /// arms `needs_render`, the fade follows the motion setting, and only
    /// the dwell's visible cell is ever chipped.
    #[test]
    fn the_tab_bar_pumps_frames_while_a_tooltip_dwell_is_pending() {
        let compact: String = include_str!("expose.rs")
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();

        // Eligibility is the per-(group, index) truncation recorded when the
        // titles were last rasterised: a cell cut against its budget earns a
        // chip, one drawn whole never does.
        assert!(compact.contains(
            "cell_truncated=text!=compositor_font::fit_ui_text(&tab.title,font,size,u32::MAX);"
        ));
        assert!(compact.contains("self.tab_titles_truncated=truncated;"));

        // Every drawn frame feeds the live hover into the dwell.
        assert!(compact.contains("self.tab_tooltip_dwell.advance("));
        // While a chip is due but not up yet the pump stays armed: the
        // `needs_render = true` lands inside the `needs_frame` guard, behind
        // only its explaining comment.
        let guard = compact
            .find("ifself.tab_tooltip_dwell.needs_frame(){")
            .expect("the dwell pump guards on needs_frame");
        let armed = compact[guard..]
            .find("self.needs_render=true;")
            .expect("the dwell pump arms needs_render");
        assert!(
            armed < 400,
            "needs_render must be armed by the needs_frame guard"
        );

        // The fade takes the dwell's visible cell and the motion setting, so
        // reduced motion snaps the chip but never skips the dwell.
        assert!(compact.contains("self.tab_tooltip_ease.advance_with_motion("));
        assert!(compact.contains("tooltip_key,crate::config::CONFIG.load().motion_enabled()"));
        // The chip draws for that visible cell only, and only at a strength
        // above zero.
        assert!(compact.contains(
            "ifletSome((group_index,index))=tooltip_key&&tooltip_p>0.0{self.render_tab_tooltip("
        ));
    }
}
