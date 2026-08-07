//! Painting the screenshot editor's toolbar.
//!
//! The window manager decides what the strip contains, how big its buttons are
//! and where it sits; everything here is the drawing. That split is the same
//! one the window tab bar uses, and it is what lets a click land on the button
//! it looks like it lands on — both sides read the rectangles out of
//! [`screenshot_toolbar`], and neither derives them independently.
//!
//! Visually the strip is one of JWM's own surfaces, so it is drawn like the
//! rest of them: a frosted or flat track from the active `ui_theme` palette,
//! with the selected tool raised out of it as an accent chip and the hovered
//! one lit more faintly.

use super::{Compositor, CompositorConnection};
use crate::backend::compositor_common::screenshot_toolbar::{
    self as toolbar, ButtonFace, ScreenshotToolbar, ToolbarIcon,
};
use crate::backend::compositor_common::ui_theme;
use crate::backend::compositor_font;
use glow::HasContext;

/// How much of the selected button's accent a merely-hovered one gets. Enough
/// to read as "this is what you would click", far enough from 1.0 that it never
/// reads as "this is the current tool".
const HOVER_WASH: f32 = 0.4;

impl<C: CompositorConnection> Compositor<C> {
    /// Take the strip the window manager published, or withdraw it.
    pub(crate) fn set_screenshot_toolbar(&mut self, toolbar: Option<ScreenshotToolbar>) {
        if self.screenshot_toolbar == toolbar {
            return;
        }
        self.screenshot_toolbar = toolbar;
        // Everything a glyph depends on — which icon, how big, what ink —
        // lives in the model, so a model change is exactly what invalidates
        // the rasterised icons.
        self.screenshot_toolbar_dirty = true;
        self.needs_render = true;
    }

    /// Rasterise and upload one icon per button, once per change.
    pub(super) fn refresh_screenshot_toolbar(&mut self) {
        if !self.screenshot_toolbar_dirty {
            return;
        }
        self.screenshot_toolbar_dirty = false;

        let stale = std::mem::take(&mut self.screenshot_toolbar_icons);
        unsafe {
            for (texture, _, _) in stale.into_iter().flatten() {
                self.gl.delete_texture(texture);
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
            let raster = match &button.face {
                ButtonFace::Icon(icon) => {
                    // The swatch is the one glyph whose color is data rather
                    // than theme: it *is* the current ink.
                    let ink = if *icon == ToolbarIcon::Color {
                        button.tint.unwrap_or(ink)
                    } else {
                        ink
                    };
                    Some(toolbar::icon_rgba(*icon, extent, ink))
                }
                ButtonFace::Label(text) => {
                    let size = toolbar::label_font_size(bar.button_size);
                    Some(compositor_font::render_ui_text_to_rgba(
                        text, font, size, ink,
                    ))
                }
            };
            icons.push(raster.and_then(|(pixels, w, h)| {
                if w == 0 || h == 0 {
                    return None;
                }
                unsafe { self.upload_overlay_texture(&pixels, w, h) }.map(|t| (t, w, h))
            }));
        }
        self.screenshot_toolbar_icons = icons;
    }

    /// Paint the track, the chips, and the glyphs on top.
    ///
    /// Takes `&mut self` for the same reason the tab bar does: under the glass
    /// themes the track samples the blurred scene, and that capture has to
    /// happen before the first chip is filled.
    pub(super) fn render_screenshot_toolbar(&mut self, proj: &[f32; 16]) {
        let Some(bar) = self.screenshot_toolbar.clone() else {
            return;
        };
        if bar.buttons.is_empty() {
            return;
        }
        let ui = ui_theme::palette();
        self.ensure_glass_backdrop(ui);
        let accent = self.border_gradient_color_a;

        unsafe {
            self.gl.bind_vertex_array(Some(self.quad_vao));

            let [tx, ty, tw, th] = bar.bar;
            let track_radius = toolbar::pill_radius(th);
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
                self.sysui_fill_rounded(x, y, w, h, radius, ui.chip);
                // Both states need the accent, not just the selected one: the
                // chip tone is a near-white, and the frosted track over a
                // bright desktop is near-white too, so a chip on its own is
                // invisible exactly where you are pointing.
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
                        ui.selection_alpha * if button.active { 1.0 } else { HOVER_WASH },
                    ],
                );
            }

            self.gl.use_program(Some(self.hud_text_program));
            self.gl.uniform_matrix_4_f32_slice(
                self.hud_text_uniforms.projection.as_ref(),
                false,
                proj,
            );
            self.gl
                .uniform_1_i32(self.hud_text_uniforms.texture.as_ref(), 0);
            self.gl.active_texture(glow::TEXTURE0);
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
                self.gl
                    .uniform_1_f32(self.hud_text_uniforms.opacity.as_ref(), opacity);
                let (gw, gh) = (*gw as f32, *gh as f32);
                self.gl.uniform_4_f32(
                    self.hud_text_uniforms.rect.as_ref(),
                    (x + (w - gw) * 0.5).round(),
                    (y + (h - gh) * 0.5).round(),
                    gw,
                    gh,
                );
                self.gl.bind_texture(glow::TEXTURE_2D, Some(*texture));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            }

            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
        }
    }
}
