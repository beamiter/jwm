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
        let mic_bars = self.state.mic_bars([0.0, 0.0, w as f32, h as f32], now);
        let progress = self
            .state
            .progress_fill(now)
            .zip(self.state.progress_rect([0.0, 0.0, w as f32, h as f32]));
        let ui = ui_theme::palette();
        let mic_color = if ui.osd_ink[..3]
            .iter()
            .map(|value| *value as u32)
            .sum::<u32>()
            < 384
        {
            [0.05, 0.50, 0.36, 1.0]
        } else {
            [0.16, 0.72, 0.56, 1.0]
        };
        let cfg = crate::config::CONFIG.load();
        let font = cfg.system_ui_font();
        let accent = self.state.accent(now);
        let raster = self.state.text_raster(now, self.state.text_width(w as f32));
        let key = format!(
            "{label}|{mic_bars:?}|{progress:?}|{w}|{h}|{font}|{:?}|{:?}|{:?}|{accent:?}|{}",
            ui.osd, ui.osd_ink, ui.slider_track, raster.layout_key
        );
        if self.cached.as_ref().is_none_or(|(old, _)| old != &key) {
            let mut pixels = vec![0u8; w * h * 4];
            let r = ui.osd_radius.min(w as f32 / 2.0).min(h as f32 / 2.0);
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
            let (tw, th) = (raster.width as usize, raster.height as usize);
            let [tx, ty, rw, rh] = self.state.text_rect(
                [0.0, 0.0, w as f32, h as f32],
                [raster.width, raster.height],
            );
            let (rw, rh) = (rw.round().max(1.0) as usize, rh.round().max(1.0) as usize);
            let (ox, oy) = (tx.max(0.0) as usize, ty.max(0.0) as usize);
            if tw > 0 && th > 0 {
                for yy in 0..rh.min(h.saturating_sub(oy)) {
                    for xx in 0..rw.min(w.saturating_sub(ox)) {
                        let (sx, sy) = (xx * tw / rw, yy * th / rh);
                        let src = &raster.pixels[(sy * tw + sx) * 4..(sy * tw + sx + 1) * 4];
                        let dst = &mut pixels
                            [((oy + yy) * w + ox + xx) * 4..((oy + yy) * w + ox + xx + 1) * 4];
                        let a = src[3] as u32;
                        for c in 0..3 {
                            dst[c] = ((src[c] as u32 * a + dst[c] as u32 * (255 - a)) / 255) as u8;
                        }
                        dst[3] = (a + (dst[3] as u32 * (255 - a)) / 255).min(255) as u8;
                    }
                }
            }
            let rail_width = w.saturating_sub(48).clamp(1, 32);
            paint_box(
                &mut pixels,
                (w, h),
                [(w - rail_width) / 2, 10.min(h / 6), rail_width, 2.min(h)],
                accent,
            );
            for (rect, active) in mic_bars {
                let color = if active { mic_color } else { ui.slider_track };
                paint_box(&mut pixels, (w, h), pixel_rect(rect), color);
            }
            if let Some((fill, rect)) = progress {
                paint_box(&mut pixels, (w, h), pixel_rect(rect), ui.slider_track);
                if fill > 0.0 {
                    let mut filled = rect;
                    filled[2] *= fill;
                    paint_box(
                        &mut pixels,
                        (w, h),
                        pixel_rect(filled),
                        [0.30, 0.49, 0.90, 1.0],
                    );
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

fn pixel_rect(rect: [f32; 4]) -> [usize; 4] {
    rect.map(|value| value.round().max(0.0) as usize)
}

/// Blend into the premultiplied fallback card. Theme colors retain their own
/// alpha; a quiet microphone does not acquire a fake waveform or activity.
fn paint_box(pixels: &mut [u8], size: (usize, usize), rect: [usize; 4], color: [f32; 4]) {
    let (w, h) = size;
    let [x, y, width, height] = rect;
    let alpha = color[3].clamp(0.0, 1.0);
    for yy in y.min(h)..y.saturating_add(height).min(h) {
        for xx in x.min(w)..x.saturating_add(width).min(w) {
            let dst = &mut pixels[(yy * w + xx) * 4..(yy * w + xx + 1) * 4];
            for c in 0..3 {
                dst[c] = (color[c] * alpha * 255.0 + dst[c] as f32 * (1.0 - alpha)) as u8;
            }
            dst[3] = (alpha * 255.0 + dst[3] as f32 * (1.0 - alpha)) as u8;
        }
    }
}
