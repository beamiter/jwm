use super::render::transform_for_encoded_srgb;
use super::*;
use crate::backend::api::ExposeNavDirection;
use crate::backend::compositor_common::expose::{expose_grid_cols, move_expose_selection};
use crate::backend::compositor_common::ui_theme;
use crate::backend::compositor_common::window_tabs;
use crate::backend::compositor_font;
use smithay::backend::renderer::gles::ffi;

fn snap_preview_colors(color: [f32; 4], opacity: f32) -> ([f32; 4], [f32; 4]) {
    let [r, g, b, a] = color;
    let alpha = a * opacity.clamp(0.0, 1.0);
    ([r, g, b, alpha], [r * 1.5, g * 1.5, b * 1.5, alpha * 2.0])
}

impl WaylandCompositor {
    /// Render the expose (mission control) mode overlay.
    /// Shows all windows arranged in a grid layout with animation.
    /// Includes dark backdrop, shadows, hover highlight with scale, and content_uv handling.
    ///
    /// `tail_scene_linear` is the frame-tail domain table's draw-domain bit:
    /// true when this overlay draws into the common linear-sRGB target
    /// (deferred routes), false when it draws into the encoded output target
    /// (legacy and early-fallback routes — the historical SDR draw).
    pub(crate) fn render_expose(
        &self,
        gl: &ffi::Gles2,
        projection: &[f32; 16],
        tail_scene_linear: bool,
    ) {
        // The draw domain below is only correct while the domain table keeps
        // this class common-linear-aware; flipping it back to encoded-only
        // must revisit this draw path (the gate then keeps it off deferred
        // routes anyway).
        debug_assert_eq!(
            tail_domain::TailOverlayClass::Expose.domain(),
            tail_domain::TailOverlayDomain::CommonLinearAware
        );
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
            let scene_linear_loc = gl.GetUniformLocation(
                self.overview_bg_program,
                b"u_scene_linear\0".as_ptr() as *const _,
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
            if scene_linear_loc >= 0 {
                gl.Uniform1i(scene_linear_loc, i32::from(tail_scene_linear));
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
                let win = match self.windows.get(&entry.id) {
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

                // Draw shadow behind each window. The shadow color is fixed
                // black, whose RGB is identical in the encoded and linear
                // domains; only the blend follows the bound target's domain,
                // which is the pipeline's defined linear-blend semantic.
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

                // The transform's forward stage is ignored by the shader when
                // drawing into the linear target, so only the encoded draw
                // (legacy/early-fallback routes on an active scene-linear
                // pipeline) needs the sRGB re-encode override.
                let color_transform = win.color_transform.map(|transform| {
                    if tail_scene_linear || !self.scene_linear_color_path_active() {
                        transform
                    } else {
                        transform_for_encoded_srgb(transform)
                    }
                });
                self.upload_window_color_transform(gl, color_transform, tail_scene_linear);

                gl.ActiveTexture(ffi::TEXTURE0);
                self.bind_window_texture(gl, tex);
                gl.Uniform1i(self.win_uniforms.texture, 0);

                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                self.reset_window_color_transform(gl);

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
                    gl.Uniform1f(self.border_uniforms.radius_top, 6.0);
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
    ///
    /// The border program's `u_scene_linear` tracks the bound target via
    /// `sync_overlay_color_domain`, so this draw needs no per-route branch;
    /// the domain table pins the class as common-linear-aware.
    pub(crate) fn render_snap_preview(&self, gl: &ffi::Gles2, projection: &[f32; 16]) {
        debug_assert_eq!(
            tail_domain::TailOverlayClass::SnapPreview.domain(),
            tail_domain::TailOverlayDomain::CommonLinearAware
        );
        let (x, y, w, h) = match self.snap_preview {
            Some(rect) => rect,
            None => return,
        };
        if self.snap_preview_opacity <= 0.0 {
            return;
        }

        let (fill_color, outline_color) =
            snap_preview_colors(self.snap_preview_color, self.snap_preview_opacity);

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
                fill_color[0],
                fill_color[1],
                fill_color[2],
                fill_color[3],
            );
            gl.Uniform2f(self.border_uniforms.size, w, h);
            gl.Uniform1f(self.border_uniforms.radius, 8.0);
            gl.Uniform1f(self.border_uniforms.radius_top, 8.0);
            // Use a very large border_width to fill the entire rect
            gl.Uniform1f(self.border_uniforms.border_width, w.max(h));
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);

            // Draw border outline (2px solid)
            gl.Uniform4f(
                self.border_uniforms.border_color,
                outline_color[0],
                outline_color[1],
                outline_color[2],
                outline_color[3],
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
            gl.Uniform1f(self.border_uniforms.radius_top, 2.0);
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
    ///
    /// `tail_scene_linear` follows the frame-tail domain table: true when this
    /// overlay draws into the common linear-sRGB target (deferred routes).
    pub(crate) fn render_peek_mode(
        &self,
        gl: &ffi::Gles2,
        projection: &[f32; 16],
        focused: Option<u64>,
        scene: &[(u64, i32, i32, u32, u32)],
        tail_scene_linear: bool,
    ) {
        debug_assert_eq!(
            tail_domain::TailOverlayClass::Peek.domain(),
            tail_domain::TailOverlayDomain::CommonLinearAware
        );
        if self.peek_opacity <= 0.0 {
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
            let scene_linear_loc = gl.GetUniformLocation(
                self.overview_bg_program,
                b"u_scene_linear\0".as_ptr() as *const _,
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
                gl.Uniform1f(opacity_loc, 0.7 * self.peek_opacity.clamp(0.0, 1.0));
            }
            if scene_linear_loc >= 0 {
                gl.Uniform1i(scene_linear_loc, i32::from(tail_scene_linear));
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

                // The transform's forward stage is ignored by the shader when
                // drawing into the linear target, so only the encoded draw
                // (legacy/early-fallback routes on an active scene-linear
                // pipeline) needs the sRGB re-encode override.
                let color_transform = win.color_transform.map(|transform| {
                    if tail_scene_linear || !self.scene_linear_color_path_active() {
                        transform
                    } else {
                        transform_for_encoded_srgb(transform)
                    }
                });
                self.upload_window_color_transform(gl, color_transform, tail_scene_linear);

                gl.ActiveTexture(ffi::TEXTURE0);
                self.bind_window_texture(gl, tex);
                gl.Uniform1i(self.win_uniforms.texture, 0);

                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                self.reset_window_color_transform(gl);
            }
        }
    }

    /// Rasterise and upload every tab title, once per change rather than once
    /// per frame.
    ///
    /// Titles are drawn in the UI theme's ink with the configured system-UI
    /// font, the same as every other surface the compositor owns, so the ink
    /// is part of what a change invalidates: the focused cell reads in
    /// `title_ink` and the rest in the dimmer `label_ink`.
    pub(crate) fn refresh_tab_titles(&mut self, gl: &ffi::Gles2) {
        if !self.tab_titles_dirty {
            return;
        }
        self.tab_titles_dirty = false;

        let stale = std::mem::take(&mut self.tab_title_textures);
        unsafe {
            for (texture, _, _) in stale.into_iter().flatten().flatten() {
                gl.DeleteTextures(1, &texture);
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
                        let mut texture = 0u32;
                        unsafe {
                            gl.GenTextures(1, &mut texture);
                            if texture == 0 {
                                return None;
                            }
                            gl.BindTexture(ffi::TEXTURE_2D, texture);
                            gl.TexParameteri(
                                ffi::TEXTURE_2D,
                                ffi::TEXTURE_MIN_FILTER,
                                ffi::NEAREST as i32,
                            );
                            gl.TexParameteri(
                                ffi::TEXTURE_2D,
                                ffi::TEXTURE_MAG_FILTER,
                                ffi::NEAREST as i32,
                            );
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
                            gl.BindTexture(ffi::TEXTURE_2D, 0);
                        }
                        Some((texture, w, h))
                    },
                ));
            }
            cache.push(row);
        }
        self.tab_title_textures = cache;
    }

    /// Paint every tab bar into the strip the window manager reserved for it.
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
    pub(crate) fn render_tab_bar(&mut self, gl: &ffi::Gles2, projection: &[f32; 16]) {
        if self.window_groups.is_empty() {
            return;
        }
        let ui = ui_theme::palette();
        self.ensure_glass_backdrop(gl, ui, projection);
        let accent = self.border_gradient_color_a;
        let tab_hover = self.tab_hover;
        let hover_scale = ui_theme::TAB_HOVER_ALPHA_SCALE;

        let (text_rect, text_proj, text_tex, text_opacity) = unsafe {
            (
                super::get_uniform_loc(gl, self.sysui_text_program, "u_rect"),
                super::get_uniform_loc(gl, self.sysui_text_program, "u_projection"),
                super::get_uniform_loc(gl, self.sysui_text_program, "u_texture"),
                super::get_uniform_loc(gl, self.sysui_text_program, "u_opacity"),
            )
        };

        unsafe {
            gl.BindVertexArray(self.quad_vao);

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
                    gl,
                    projection,
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
                        self.sysui_fill_rounded(gl, x, y, w, h, radius, ui.chip);
                        self.sysui_fill_rounded(
                            gl,
                            x,
                            y,
                            w,
                            h,
                            radius,
                            [accent[0], accent[1], accent[2], ui.selection_alpha],
                        );
                    } else {
                        self.sysui_fill_rounded(
                            gl,
                            x,
                            y,
                            w,
                            h,
                            radius,
                            [ui.chip[0], ui.chip[1], ui.chip[2], ui.chip[3] * hover_scale],
                        );
                        self.sysui_fill_rounded(
                            gl,
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
                gl.UseProgram(self.sysui_text_program);
                self.set_projection_uniform(gl, text_proj, projection);
                gl.Uniform1i(text_tex, 0);
                gl.Uniform1f(text_opacity, 1.0);
                gl.ActiveTexture(ffi::TEXTURE0);
                for (index, slot) in titles.iter().enumerate() {
                    let Some((texture, tw, th)) = slot else {
                        continue;
                    };
                    let Some([x, y, w, h]) = window_tabs::cell_rect(group.bar, count, index) else {
                        continue;
                    };
                    let (tw, th) = (*tw as f32, *th as f32);
                    self.set_rect_uniform(
                        gl,
                        text_rect,
                        (x + (w - tw) * 0.5).round(),
                        (y + (h - th) * 0.5).round(),
                        tw,
                        th,
                    );
                    gl.BindTexture(ffi::TEXTURE_2D, *texture);
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
                return Some(entry.id);
            }
        }
        None
    }

    pub(crate) fn set_expose_hover(&mut self, x: f32, y: f32) {
        let hit_id = self.expose_hit_test(x, y);
        self.expose_select_id(hit_id);
    }

    /// Highlight the expose entry for `id` (`None` clears the highlight).
    /// Mouse hover and keyboard selection share this single highlight.
    pub(crate) fn expose_select_id(&mut self, id: Option<u64>) {
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
    pub(crate) fn expose_selected(&self) -> Option<u64> {
        self.expose_entries
            .iter()
            .find(|entry| entry.is_hovered)
            .map(|entry| entry.id)
    }
}

#[cfg(test)]
mod tests {
    use super::snap_preview_colors;

    #[test]
    fn snap_preview_uses_configured_rgba_and_derives_a_brighter_outline() {
        let (fill, outline) = snap_preview_colors([0.2, 0.4, 0.6, 0.3], 0.5);
        assert_eq!(fill, [0.2, 0.4, 0.6, 0.15]);
        assert_eq!(outline, [0.3, 0.6, 0.90000004, 0.3]);

        let (hidden, hidden_outline) = snap_preview_colors([0.2, 0.4, 0.6, 0.3], -1.0);
        assert_eq!(hidden[3], 0.0);
        assert_eq!(hidden_outline[3], 0.0);
    }
}
