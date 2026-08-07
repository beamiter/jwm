//! Painting the screenshot editor's toolbar and the annotation overlay's
//! filled shapes and labels.
//!
//! The mirror of `backend/x11/compositor/screenshot_toolbar.rs`, down to the
//! order of the passes: shapes, then strokes, then the toolbar floating above
//! everything it edits. Both compositors read their rectangles out of
//! [`screenshot_toolbar`], which is the same module the window manager
//! hit-tests against, so a button does what it looks like it does regardless of
//! which backend is running.

use super::{WaylandCompositor, ffi};
use crate::backend::compositor_common::screenshot_toolbar::{
    self as toolbar, ButtonFace, ToolbarIcon,
};
use crate::backend::compositor_common::ui_theme;
use crate::backend::compositor_font;

/// How much of the selected button's accent a merely-hovered one gets. Enough
/// to read as "this is what you would click", far enough from 1.0 that it never
/// reads as "this is the current tool".
const HOVER_WASH: f32 = 0.4;

impl WaylandCompositor {
    /// Rasterise and upload every annotation label, once per change.
    pub(crate) fn refresh_annotation_labels(&mut self, gl: &ffi::Gles2) {
        if !self.annotation_labels_dirty {
            return;
        }
        self.annotation_labels_dirty = false;

        let stale = std::mem::take(&mut self.annotation_label_textures);
        unsafe {
            for (texture, _, _) in stale.into_iter().flatten() {
                gl.DeleteTextures(1, &texture);
            }
        }

        let config = crate::config::CONFIG.load();
        let font = config.system_ui_font();
        let mut textures = Vec::with_capacity(self.annotation_labels.len());
        for label in &self.annotation_labels {
            let (pixels, w, h) = compositor_font::render_ui_text_to_rgba(
                &label.text,
                font,
                label.size,
                ink_bytes(label.color),
            );
            textures.push(unsafe { upload_overlay_texture(gl, &pixels, w, h) });
        }
        self.annotation_label_textures = textures;
    }

    /// Paint the annotation overlay's filled shapes and labels.
    pub(crate) fn render_annotation_shapes(&mut self, gl: &ffi::Gles2, projection: &[f32; 16]) {
        if self.annotation_quads.is_empty() && self.annotation_label_textures.is_empty() {
            return;
        }
        let (text_rect, text_proj, text_tex, text_opacity) = text_uniforms(gl, self);

        unsafe {
            gl.BindVertexArray(self.quad_vao);
            // `sysui_fill_rounded` fills through the border program and takes
            // the binding as given — the tab bar gets it from the
            // `ui_fill_island` that draws its track. Nothing binds it for us,
            // so a redaction bar would draw in whatever colour the last pass
            // left behind and a counter bubble would not appear at all.
            if !self.annotation_quads.is_empty() {
                gl.UseProgram(self.border_program);
                self.set_projection_uniform(gl, self.border_uniforms.projection, projection);
                gl.Uniform1i(self.border_uniforms.scene_linear, 0);
                for quad in &self.annotation_quads {
                    self.sysui_fill_rounded(
                        gl,
                        quad.x,
                        quad.y,
                        quad.w,
                        quad.h,
                        quad.radius,
                        quad.color,
                    );
                }
            }

            if !self.annotation_label_textures.is_empty() {
                gl.UseProgram(self.sysui_text_program);
                self.set_projection_uniform(gl, text_proj, projection);
                gl.Uniform1i(text_tex, 0);
                gl.Uniform1f(text_opacity, 1.0);
                gl.ActiveTexture(ffi::TEXTURE0);
                for (label, slot) in self
                    .annotation_labels
                    .iter()
                    .zip(self.annotation_label_textures.iter())
                {
                    let Some((texture, w, h)) = slot else {
                        continue;
                    };
                    let (w, h) = (*w as f32, *h as f32);
                    let (x, y) = label.origin(w, h);
                    self.set_rect_uniform(gl, text_rect, x, y, w, h);
                    gl.BindTexture(ffi::TEXTURE_2D, *texture);
                    gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                }
            }
            gl.BindVertexArray(0);
            gl.UseProgram(0);
        }
    }

    /// Rasterise and upload one glyph per toolbar button, once per change.
    pub(crate) fn refresh_screenshot_toolbar(&mut self, gl: &ffi::Gles2) {
        if !self.screenshot_toolbar_dirty {
            return;
        }
        self.screenshot_toolbar_dirty = false;

        let stale = std::mem::take(&mut self.screenshot_toolbar_icons);
        unsafe {
            for (texture, _, _) in stale.into_iter().flatten() {
                gl.DeleteTextures(1, &texture);
            }
        }

        let Some(bar) = self.screenshot_toolbar.as_ref() else {
            return;
        };
        let ui = ui_theme::palette();
        let config = crate::config::CONFIG.load();
        let font = config.system_ui_font();
        let extent = toolbar::icon_extent(bar.button_size);

        let mut icons = Vec::with_capacity(bar.buttons.len());
        for button in &bar.buttons {
            let ink = if button.active {
                ui.title_ink
            } else {
                ui.label_ink
            };
            let (pixels, w, h) = match &button.face {
                ButtonFace::Icon(icon) => {
                    // The swatch is the one glyph whose color is data rather
                    // than theme: it *is* the current ink.
                    let ink = if *icon == ToolbarIcon::Color {
                        button.tint.unwrap_or(ink)
                    } else {
                        ink
                    };
                    toolbar::icon_rgba(*icon, extent, ink)
                }
                ButtonFace::Label(text) => compositor_font::render_ui_text_to_rgba(
                    text,
                    font,
                    toolbar::label_font_size(bar.button_size),
                    ink,
                ),
            };
            icons.push(unsafe { upload_overlay_texture(gl, &pixels, w, h) });
        }
        self.screenshot_toolbar_icons = icons;
    }

    /// Paint the track, the chips, and the glyphs on top.
    pub(crate) fn render_screenshot_toolbar(&mut self, gl: &ffi::Gles2, projection: &[f32; 16]) {
        let Some(bar) = self.screenshot_toolbar.clone() else {
            return;
        };
        if bar.buttons.is_empty() {
            return;
        }
        let ui = ui_theme::palette();
        self.ensure_glass_backdrop(gl, ui, projection);
        let accent = self.border_gradient_color_a;
        let (text_rect, text_proj, text_tex, text_opacity) = text_uniforms(gl, self);

        unsafe {
            gl.BindVertexArray(self.quad_vao);

            let [tx, ty, tw, th] = bar.bar;
            let track_radius = toolbar::pill_radius(th);
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

            for (index, button) in bar.buttons.iter().enumerate() {
                if !button.active && !button.hovered {
                    continue;
                }
                let Some([x, y, w, h]) =
                    toolbar::button_rect(bar.bar, &bar.buttons, bar.button_size, index)
                else {
                    continue;
                };
                let radius = toolbar::pill_radius(h.min(w));
                self.sysui_fill_rounded(gl, x, y, w, h, radius, ui.chip);
                // Both states need the accent, not just the selected one: the
                // chip tone is a near-white, and the frosted track over a
                // bright desktop is near-white too, so a chip on its own is
                // invisible exactly where you are pointing.
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
                        ui.selection_alpha * if button.active { 1.0 } else { HOVER_WASH },
                    ],
                );
            }

            gl.UseProgram(self.sysui_text_program);
            self.set_projection_uniform(gl, text_proj, projection);
            gl.Uniform1i(text_tex, 0);
            gl.ActiveTexture(ffi::TEXTURE0);
            for (index, slot) in self.screenshot_toolbar_icons.iter().enumerate() {
                let Some((texture, gw, gh)) = slot else {
                    continue;
                };
                let Some([x, y, w, h]) =
                    toolbar::button_rect(bar.bar, &bar.buttons, bar.button_size, index)
                else {
                    continue;
                };
                // A disabled control keeps its glyph but loses its presence,
                // so the row never reflows when undo runs out of history.
                let opacity = if bar.buttons[index].enabled {
                    1.0
                } else {
                    0.38
                };
                gl.Uniform1f(text_opacity, opacity);
                let (gw, gh) = (*gw as f32, *gh as f32);
                self.set_rect_uniform(
                    gl,
                    text_rect,
                    (x + (w - gw) * 0.5).round(),
                    (y + (h - gh) * 0.5).round(),
                    gw,
                    gh,
                );
                gl.BindTexture(ffi::TEXTURE_2D, *texture);
                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
            }

            gl.BindVertexArray(0);
            gl.UseProgram(0);
        }
    }
}

fn text_uniforms(gl: &ffi::Gles2, compositor: &WaylandCompositor) -> (i32, i32, i32, i32) {
    unsafe {
        (
            super::get_uniform_loc(gl, compositor.sysui_text_program, "u_rect"),
            super::get_uniform_loc(gl, compositor.sysui_text_program, "u_projection"),
            super::get_uniform_loc(gl, compositor.sysui_text_program, "u_texture"),
            super::get_uniform_loc(gl, compositor.sysui_text_program, "u_opacity"),
        )
    }
}

fn ink_bytes(color: [f32; 4]) -> [u8; 4] {
    [
        (color[0] * 255.0).clamp(0.0, 255.0) as u8,
        (color[1] * 255.0).clamp(0.0, 255.0) as u8,
        (color[2] * 255.0).clamp(0.0, 255.0) as u8,
        (color[3] * 255.0).clamp(0.0, 255.0) as u8,
    ]
}

/// Upload a straight-alpha RGBA buffer as a linearly filtered texture.
unsafe fn upload_overlay_texture(
    gl: &ffi::Gles2,
    pixels: &[u8],
    w: u32,
    h: u32,
) -> Option<(u32, u32, u32)> {
    if w == 0 || h == 0 || pixels.len() < (w as usize * h as usize * 4) {
        return None;
    }
    unsafe {
        let mut texture = 0u32;
        gl.GenTextures(1, &mut texture);
        if texture == 0 {
            return None;
        }
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
        Some((texture, w, h))
    }
}
