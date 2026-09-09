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
    ///
    /// The appear ease keys on presence alone: publishing the strip starts
    /// it fresh and withdrawing clears it, while a republished strip —
    /// every hover move is one — keeps the ease it has, which is exactly
    /// why the envelope lives here and not on the model it would restart
    /// with.
    pub(crate) fn set_screenshot_toolbar(&mut self, toolbar: Option<ScreenshotToolbar>) {
        if self.screenshot_toolbar == toolbar {
            return;
        }
        // A presence change is the only thing that resets the ease; the
        // first drawn frame after this starts the fresh envelope.
        if self.screenshot_toolbar.is_some() != toolbar.is_some() {
            self.screenshot_toolbar_appear.clear();
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
    /// The whole strip eases in when the editor appears — one blank frame
    /// after publish, then the tab strips' 120 ms ease-out quad to full —
    /// through `screenshot_toolbar_appear`, multiplied into every layer as
    /// alpha. With motion off the first frame is already full and no extra
    /// frames tick.
    ///
    /// Takes `&mut self` for the same reason the tab bar does: under the glass
    /// themes the track samples the blurred scene, and that capture has to
    /// happen before the first chip is filled.
    pub(super) fn render_screenshot_toolbar(&mut self, proj: &[f32; 16]) {
        // The hover wash eases in on the button under the pointer instead of
        // flipping on in a single frame, and is gone the same frame the hover
        // leaves — JWM draws no fade-outs. The envelope is draw strength
        // only: the hit test resolves the model's rectangles, which never
        // see it.
        let Some(stored) = self.screenshot_toolbar.as_mut() else {
            return;
        };
        let hovered = toolbar::hovered_key(&stored.buttons);
        let hover_p = stored.hover_ease.advance_with_motion(
            std::time::Instant::now(),
            hovered,
            crate::config::CONFIG.load().motion_enabled(),
        );
        // The wash follows a clock, not an event: keep the frame loop alive
        // while it is easing in, or an idle screen would freeze it mid-fade.
        if stored.hover_ease.animating() {
            self.needs_render = true;
        }
        let bar = stored.clone();
        if bar.buttons.is_empty() {
            return;
        }
        // The strip as a whole eases in over the tab strips' 120 ms when the
        // editor appears, presence-keyed so the hover republishes cannot
        // restart it. It runs on a clock like the wash, so the pump mirrors
        // it: frames must keep coming while the ease is mid-flight.
        let appear = self.screenshot_toolbar_appear.advance(
            std::time::Instant::now(),
            true,
            crate::config::CONFIG.load().motion_enabled(),
        );
        if self.screenshot_toolbar_appear.animating() {
            self.needs_render = true;
        }
        // The ease opens with one blank frame — the tab strips' shape.
        if appear <= 0.0 {
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
                appear,
            );

            for (index, button) in bar.buttons.iter().enumerate() {
                // The selected tool sits at full strength; the hovered button
                // rides the envelope. Anything else draws no chip at all —
                // which is also how a hover that just left disappears on the
                // spot.
                let wash = if button.active {
                    1.0
                } else if Some(index) == hovered {
                    hover_p
                } else {
                    0.0
                };
                if wash <= 0.0 {
                    continue;
                }
                let Some([x, y, w, h]) =
                    toolbar::button_rect(bar.bar, &bar.buttons, bar.button_size, index)
                else {
                    continue;
                };
                let radius = toolbar::pill_radius(h.min(w));
                self.sysui_fill_rounded(
                    x,
                    y,
                    w,
                    h,
                    radius,
                    [
                        ui.chip[0],
                        ui.chip[1],
                        ui.chip[2],
                        ui.chip[3] * wash * appear,
                    ],
                );
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
                        ui.selection_alpha
                            * wash
                            * appear
                            * if button.active { 1.0 } else { HOVER_WASH },
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
                    .uniform_1_f32(self.hud_text_uniforms.opacity.as_ref(), opacity * appear);
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

#[cfg(test)]
mod tests {
    /// The hover wash eases in on a clock, not on an event, so the strip's
    /// render path must keep the frame loop alive while the envelope is in
    /// flight — an idle screen would otherwise freeze the cue mid-fade. The
    /// strip's own appear ease keeps the same discipline, keyed on the
    /// strip's *presence* rather than its content: the window manager
    /// republishes the strip on every hover move, and the ease must ride
    /// through every one of them (the round-16 rejection). Pin the wiring:
    /// the hover ease keys on the model's hovered button (the same index
    /// the hit test resolves), the appear ease clears only on a presence
    /// transition, both advance on the frame clock under the motion setting
    /// and arm `needs_render` exactly while animating, and the appear ease
    /// scales every layer's alpha after one blank first frame. The haystack
    /// is the shipped source; the needles are built at runtime so this test
    /// cannot match its own.
    #[test]
    fn the_toolbar_pumps_frames_while_a_hover_wash_is_easing_in() {
        let compact: String = include_str!("screenshot_toolbar.rs")
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();

        // The hover envelope keys on the model's hovered button…
        assert!(compact.contains(concat!("toolbar::hovered", "_key(&stored.buttons)")));
        // …advances on the frame clock under the motion setting…
        assert!(compact.contains(concat!("hover_ease.advance", "_with_motion(")));
        assert!(compact.contains(concat!("crate::config::CONFIG.load().motion", "_enabled()")));
        // …and arms the pump exactly while the ease is in flight.
        assert!(compact.contains(concat!(
            "ifstored.hover_ease.animating(){",
            "self.needs_render=true;}"
        )));

        // The hover flag no longer gates drawing directly: the eased wash
        // decides, and a hover that left skips the chip on the spot.
        assert!(!compact.contains(concat!("button.", "hovered")));
        assert!(compact.contains(concat!("ifwash<=0.0{", "continue;}")));

        // The appear ease clears only when the strip's presence changes —
        // never on a republish, or the fade would restart on every hover
        // move…
        assert!(compact.contains(concat!(
            "self.screenshot_toolbar.is_some()!=toolbar.is_some(){",
            "self.screenshot_toolbar",
            "_appear.clear();}"
        )));
        // …advances on the same frame clock and motion setting…
        assert!(compact.contains(concat!(
            "self.screenshot_toolbar",
            "_appear.advance(std::time::Instant::now(),true,"
        )));
        // …arms the pump exactly while it is in flight…
        assert!(compact.contains(concat!(
            "ifself.screenshot_toolbar",
            "_appear.animating(){self.needs_render=true;}"
        )));
        // …and opens with one blank frame, the tab strips' shape.
        assert!(compact.contains(concat!("ifappear<=0.0{", "return;}")));

        // Every layer rides the appear envelope as alpha — the track…
        assert!(compact.contains(concat!("ui.card,", "appear,")));
        assert!(!compact.contains(concat!("ui.card,", "1.0,")));
        // …both chip fills, which keep the hover wash beside it…
        assert!(compact.contains(concat!("ui.chip[3]*wash*", "appear")));
        assert!(compact.contains(concat!(
            "ui.selection_alpha*wash*",
            "appear*ifbutton.active{1.0}else{HOVER",
            "_WASH},"
        )));
        // …and the glyph opacity uniform.
        assert!(compact.contains(concat!("opacity*", "appear")));
    }
}
