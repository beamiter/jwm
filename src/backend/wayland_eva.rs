//! Eva status for the lightweight nested Smithay renderers. The status is a
//! compositor element, never a Wayland client surface or keyboard owner.
use crate::backend::compositor_common::{eva_overlay::EvaOverlayState, ui_theme};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::ImportMem;
use smithay::backend::renderer::element::{
    Kind,
    memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
};
use smithay::utils::{Physical, Point, Transform};
use std::time::Instant;

#[derive(Default)]
pub(crate) struct EvaElement {
    state: EvaOverlayState,
    cached: Option<(String, MemoryRenderBuffer)>,
}
impl EvaElement {
    pub(crate) fn update(&mut self, snapshot: &serde_json::Value) -> bool {
        self.state.update(snapshot, Instant::now())
    }
    pub(crate) fn needs_frame(&self) -> bool {
        self.state.needs_frame(Instant::now())
    }
    pub(crate) fn render<R: ImportMem>(
        &mut self,
        renderer: &mut R,
        size: (i32, i32),
    ) -> Option<MemoryRenderBufferRenderElement<R>>
    where
        R::TextureId: Send + Clone + 'static,
    {
        if !self.state.visible() {
            self.cached = None;
            return None;
        }
        let now = Instant::now();
        let alpha = self.state.alpha(now);
        let [x, y, w, h] = self.state.rect([size.0 as f32, size.1 as f32]);
        let (w, h) = (w.round().max(1.0) as usize, h.round().max(1.0) as usize);
        let label = self.state.label(now);
        let level = self.state.level(now);
        let key = format!("{label}|{level:?}|{w}|{h}");
        if self.cached.as_ref().is_none_or(|(old, _)| old != &key) {
            let ui = ui_theme::palette();
            let mut pixels = vec![0u8; w * h * 4];
            let r = 16.0f32.min(w as f32 / 2.0).min(h as f32 / 2.0);
            for yy in 0..h {
                for xx in 0..w {
                    let cx = (xx as f32).clamp(r, w as f32 - r);
                    let cy = (yy as f32).clamp(r, h as f32 - r);
                    let dx = xx as f32 - cx;
                    let dy = yy as f32 - cy;
                    if dx * dx + dy * dy > r * r {
                        continue;
                    }
                    let px = &mut pixels[(yy * w + xx) * 4..(yy * w + xx + 1) * 4];
                    for c in 0..3 {
                        px[c] = (ui.osd[c] * ui.osd[3] * 255.0) as u8;
                    }
                    px[3] = (ui.osd[3] * 255.0) as u8;
                }
            }
            let cfg = crate::config::CONFIG.load();
            let font = cfg.system_ui_font();
            let font_size = crate::backend::compositor_font::ui_font_pixel_size(font);
            let fitted = crate::backend::compositor_common::eva_overlay::fit_eva_label(
                &label,
                font,
                font_size,
                w.saturating_sub(40) as u32,
            );
            let (text, tw, th) = crate::backend::compositor_font::render_ui_text_to_rgba(
                &fitted, font, font_size, ui.osd_ink,
            );
            let (tw, th) = (tw as usize, th as usize);
            let ox = w.saturating_sub(tw) / 2;
            let oy = h.saturating_sub(th) / 2;
            for ty in 0..th.min(h) {
                for tx in 0..tw.min(w) {
                    if ox + tx >= w || oy + ty >= h {
                        continue;
                    }
                    let src = &text[(ty * tw + tx) * 4..(ty * tw + tx + 1) * 4];
                    let dst = &mut pixels
                        [((oy + ty) * w + ox + tx) * 4..((oy + ty) * w + ox + tx + 1) * 4];
                    let a = src[3] as u32;
                    for c in 0..3 {
                        dst[c] = ((src[c] as u32 * a + dst[c] as u32 * (255 - a)) / 255) as u8;
                    }
                    dst[3] = (a + (dst[3] as u32 * (255 - a)) / 255).min(255) as u8;
                }
            }
            if let Some(level) = level {
                let count = ((w.saturating_sub(40)) as f32 * level) as usize;
                for yy in h.saturating_sub(12)..h.saturating_sub(9) {
                    for xx in 20..w.saturating_sub(20) {
                        let color = if xx - 20 < count {
                            ui.osd_ink
                        } else {
                            [80, 80, 80, 255]
                        };
                        pixels[(yy * w + xx) * 4..(yy * w + xx + 1) * 4].copy_from_slice(&color);
                    }
                }
            }
            self.cached = Some((
                key,
                MemoryRenderBuffer::from_slice(
                    &pixels,
                    Fourcc::Abgr8888,
                    (w as i32, h as i32),
                    1,
                    Transform::Normal,
                    None,
                ),
            ));
        }
        self.state.mark_drawn(now);
        MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            Point::<f64, Physical>::from((x as f64, y as f64)),
            &self.cached.as_ref()?.1,
            Some(alpha),
            None,
            None,
            Kind::Unspecified,
        )
        .ok()
    }
}
