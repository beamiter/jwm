//! egui renderer for the shared `xbar_core` presentation scene.
//!
//! Layout, colours, and hit regions all come from `xbar_core::presentation` —
//! the same display list `xcb_bar` hands to Cairo. Only the translation into
//! egui shapes lives here. That is what keeps this bar looking like the Cairo
//! bars instead of restating their design in a second widget tree, and it is
//! why adding a status cell to the core shows up here with no work.

use egui::{
    Color32, ColorImage, Context, CornerRadius, FontFamily, FontId, Id, Painter, Pos2,
    Rect as EguiRect, Shape, Stroke as EguiStroke, StrokeKind, TextureHandle, TextureOptions, Vec2,
};
use std::collections::{HashMap, VecDeque};
use xbar_core::presentation::{
    ImageSource, Rect, Rgba, Scene, SceneNode, Size, TextAlign, TextMeasurer,
};

const MAX_SCENE_IMAGE_DIMENSION: u32 = 2_048;
const MAX_SCENE_IMAGE_ALLOC_BYTES: u64 = 32 * 1024 * 1024;
const MAX_SCENE_IMAGE_CACHE_ENTRIES: usize = 128;
const MAX_SCENE_IMAGE_CACHE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Default)]
struct SceneImageCache {
    entries: HashMap<u64, (Option<TextureHandle>, usize)>,
    /// Least-recently-used key first, including failed decodes.
    recency: VecDeque<u64>,
    total_bytes: usize,
}

impl SceneImageCache {
    fn get(&mut self, key: u64) -> Option<Option<TextureHandle>> {
        let cached = self.entries.get(&key)?.0.clone();
        self.mark_recent(key);
        Some(cached)
    }

    fn mark_recent(&mut self, key: u64) {
        let position = self
            .recency
            .iter()
            .position(|candidate| *candidate == key)
            .expect("a cached egui image has a recency record");
        self.recency.remove(position);
        self.recency.push_back(key);
    }

    fn insert(&mut self, key: u64, texture: Option<TextureHandle>) {
        let bytes = texture.as_ref().map_or(0, |texture| {
            let [width, height] = texture.size();
            width.saturating_mul(height).saturating_mul(4)
        });
        self.insert_with_bytes(key, texture, bytes);
    }

    fn insert_with_bytes(&mut self, key: u64, texture: Option<TextureHandle>, bytes: usize) {
        if bytes > MAX_SCENE_IMAGE_CACHE_BYTES {
            return;
        }

        if let Some((_, previous_bytes)) = self.entries.remove(&key) {
            self.total_bytes = self.total_bytes.saturating_sub(previous_bytes);
            let position = self
                .recency
                .iter()
                .position(|candidate| *candidate == key)
                .expect("a cached egui image has a recency record");
            self.recency.remove(position);
        }
        while !self.entries.is_empty()
            && (self.entries.len() >= MAX_SCENE_IMAGE_CACHE_ENTRIES
                || self.total_bytes.saturating_add(bytes) > MAX_SCENE_IMAGE_CACHE_BYTES)
        {
            let oldest = self
                .recency
                .pop_front()
                .expect("a full egui image cache has an eviction candidate");
            let (_, evicted_bytes) = self
                .entries
                .remove(&oldest)
                .expect("an egui image eviction candidate is cached");
            self.total_bytes = self.total_bytes.saturating_sub(evicted_bytes);
        }
        self.entries.insert(key, (texture, bytes));
        self.recency.push_back(key);
        self.total_bytes = self.total_bytes.saturating_add(bytes);
        debug_assert_eq!(self.entries.len(), self.recency.len());
        debug_assert_eq!(
            self.total_bytes,
            self.entries
                .values()
                .map(|(_, bytes)| *bytes)
                .sum::<usize>()
        );
    }
}

/// Text metrics for `LayoutEngine`, forwarded to a caller-supplied closure.
///
/// The engine measures text while building the scene, but egui only lends out
/// its font stack inside a callback, so the borrow has to stay at the call
/// site and this just forwards to it.
pub struct ClosureMeasurer<F>(pub F);

impl<F: Fn(&str, f32) -> Size> TextMeasurer for ClosureMeasurer<F> {
    fn measure(&self, text: &str, size: f32) -> Size {
        (self.0)(text, size)
    }
}

/// The font a run of scene text is laid out with.
///
/// Measuring and painting must agree exactly: the engine sizes each pill from
/// what the measurer reported, so a different font or size at paint time would
/// draw glyphs that no longer fit the pill reserved for them. Both paths build
/// their `FontId` here for that reason.
#[must_use]
pub fn font_id(size: f32, family: &FontFamily) -> FontId {
    FontId::new(size, family.clone())
}

/// The box a galley actually paints, relative to its own origin.
///
/// egui's `rect` is the logical advance box, which is narrower than the ink of
/// a Nerd Font icon — those glyphs deliberately overhang their advance. Sizing
/// a pill from the advance alone reserves too little width and the icon is
/// then clipped to a sliver, which is exactly what this bar used to show. The
/// Cairo renderer takes the same ink-and-logical union for the same reason.
#[must_use]
pub fn galley_extents(galley: &egui::Galley) -> EguiRect {
    if galley.mesh_bounds.is_positive() {
        galley.rect.union(galley.mesh_bounds)
    } else {
        galley.rect
    }
}

/// How to paint a scene that the presentation layer does not decide itself.
pub struct SceneStyle {
    /// Family every text node is laid out in. Icon glyphs resolve through the
    /// fallback chain installed by [`crate::fonts`].
    pub family: FontFamily,
    /// Alpha multiplier for the background node alone, mirroring
    /// `CairoRenderer::set_background_opacity`. Below 1.0 this is what lets
    /// the compositor's blur show through; an opaque window pins it at 1.0.
    pub background_opacity: f32,
}

/// Draw every node in display-list order, clipped like the Cairo renderer.
///
/// `origin` is where the scene's (0, 0) lands in egui screen coordinates. The
/// scene is laid out in logical units and egui paints in points, so the two
/// need no scaling between them.
pub fn paint(painter: &Painter, origin: Pos2, scene: &Scene, style: &SceneStyle) {
    let viewport = Rect::new(0.0, 0.0, scene.viewport.width, scene.viewport.height);
    let Some(clip) = scene.clip.intersection(viewport) else {
        return;
    };

    for node in &scene.nodes {
        let bounds = node.bounds();
        // A node never paints outside its own bounds; that is what makes the
        // core's damage rectangles true, and it clips an overlong client name
        // to the width the engine gave it.
        let Some(visible) = bounds.intersection(clip) else {
            continue;
        };
        let painter =
            painter.with_clip_rect(to_screen(visible, origin).intersect(painter.clip_rect()));
        let rect = to_screen(bounds, origin);

        match node {
            SceneNode::Background { fill, .. } => {
                painter.rect_filled(
                    rect,
                    CornerRadius::ZERO,
                    color(*fill, style.background_opacity),
                );
            }
            SceneNode::RoundedRect {
                radius,
                fill,
                stroke,
                ..
            } => {
                let limit = bounds.width.min(bounds.height) * 0.5;
                let corner = CornerRadius::same(radius_to_u8(radius.clamp(0.0, limit)));
                // Cairo insets its path by half the stroke width so the stroke
                // lands wholly inside the pill; `StrokeKind::Inside` is the
                // same rule expressed in egui's terms.
                let stroke = stroke.map_or(EguiStroke::NONE, |stroke| {
                    EguiStroke::new(
                        stroke.width.clamp(0.0, bounds.width.min(bounds.height)),
                        color(stroke.color, 1.0),
                    )
                });
                painter.rect(rect, corner, color(*fill, 1.0), stroke, StrokeKind::Inside);
            }
            SceneNode::Text {
                text,
                size,
                color: text_color,
                align,
                ..
            } => {
                if text.is_empty() || !size.is_finite() || *size <= 0.0 {
                    continue;
                }
                let tint = color(*text_color, 1.0);
                let galley =
                    painter.layout_no_wrap(text.clone(), font_id(*size, &style.family), tint);
                let extents = galley_extents(&galley);
                let extent = extents.size();
                let x = match align {
                    TextAlign::Start => bounds.x,
                    TextAlign::Center => bounds.x + (bounds.width - extent.x) * 0.5,
                    TextAlign::End => bounds.right() - extent.x,
                };
                let y = bounds.y + (bounds.height - extent.y) * 0.5;
                // Placing the *painted* box means the glyph is centred on what
                // is actually drawn, so the galley origin shifts by however far
                // the ink starts outside it.
                painter.galley(
                    origin + Vec2::new(x, y) - extents.min.to_vec2(),
                    galley,
                    tint,
                );
            }
            SceneNode::Polyline {
                points,
                color: line_color,
                width,
                ..
            } => {
                if points.len() < 2 || !width.is_finite() || *width <= 0.0 {
                    continue;
                }
                let points = points
                    .iter()
                    .map(|point| origin + Vec2::new(point.x, point.y))
                    .collect::<Vec<_>>();
                painter.add(Shape::line(
                    points,
                    EguiStroke::new(*width, color(*line_color, 1.0)),
                ));
            }
            SceneNode::Image { source, .. } => {
                let Some(texture) = image_texture(painter.ctx(), source) else {
                    continue;
                };
                let size = texture.size_vec2();
                if size.x <= 0.0 || size.y <= 0.0 {
                    continue;
                }
                // Fit inside the bounds the engine reserved, keeping the
                // artwork's own aspect ratio, exactly like the Cairo renderer.
                let scale = (bounds.width / size.x).min(bounds.height / size.y);
                let fitted = EguiRect::from_center_size(rect.center(), size * scale);
                painter.image(
                    texture.id(),
                    fitted,
                    EguiRect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
        }
    }
}

/// Texture for one scene image, uploaded at most once per file.
///
/// egui hands out no place to keep renderer state across frames, so handles
/// live in one bounded LRU in the context's temporary store. A file that cannot
/// be decoded is remembered as `None`, so a broken icon costs one failed decode
/// rather than one per frame without growing the store forever.
fn image_texture(ctx: &Context, source: &ImageSource) -> Option<TextureHandle> {
    let cache_id = Id::new("xbar-scene-image-cache");
    if let Some(cached) = ctx.data_mut(|data| {
        data.get_temp_mut_or_default::<SceneImageCache>(cache_id)
            .get(source.key)
    }) {
        return cached;
    }
    let texture = decode_image(&source.path).map(|image| {
        ctx.load_texture(
            format!("xbar-scene-image-{}", source.key),
            image,
            TextureOptions::LINEAR,
        )
    });
    ctx.data_mut(|data| {
        data.get_temp_mut_or_default::<SceneImageCache>(cache_id)
            .insert(source.key, texture.clone());
    });
    texture
}

fn decode_image(path: &std::path::Path) -> Option<ColorImage> {
    let mut reader = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SCENE_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_SCENE_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_SCENE_IMAGE_ALLOC_BYTES);
    reader.limits(limits);
    let rgba = reader.decode().ok()?.into_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Some(ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
}

fn to_screen(rect: Rect, origin: Pos2) -> EguiRect {
    EguiRect::from_min_size(
        origin + Vec2::new(rect.x, rect.y),
        Vec2::new(rect.width, rect.height),
    )
}

fn color(rgba: Rgba, opacity: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(
        scalar_to_u8(rgba.red),
        scalar_to_u8(rgba.green),
        scalar_to_u8(rgba.blue),
        scalar_to_u8(rgba.alpha * opacity),
    )
}

/// Saturating 0..=1 to 0..=255. NaN reads as zero rather than saturating to
/// the maximum the way a bare cast would, so a malformed config colour cannot
/// turn into an opaque one.
fn scalar_to_u8(value: f32) -> u8 {
    if value.is_nan() {
        return 0;
    }
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// A corner radius already in points, narrowed to egui's byte-sized field.
fn radius_to_u8(radius: f32) -> u8 {
    if radius.is_nan() {
        return 0;
    }
    radius.clamp(0.0, 255.0).round() as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scene_image_decoder_bounds_dimensions_before_texture_upload() {
        let directory = std::env::temp_dir().join(format!(
            "egui_bar_image_limit_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let small = directory.join("small.png");
        let oversized = directory.join("oversized.png");
        image::RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 255]))
            .save(&small)
            .unwrap();
        image::RgbaImage::from_pixel(
            MAX_SCENE_IMAGE_DIMENSION + 1,
            1,
            image::Rgba([1, 2, 3, 255]),
        )
        .save(&oversized)
        .unwrap();

        assert!(decode_image(&small).is_some());
        assert!(decode_image(&oversized).is_none());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn scene_image_cache_bounds_textures_and_failures_with_lru_eviction() {
        let mut cache = SceneImageCache::default();
        for key in 0..MAX_SCENE_IMAGE_CACHE_ENTRIES as u64 {
            cache.insert(key, None);
        }
        assert_eq!(cache.entries.len(), MAX_SCENE_IMAGE_CACHE_ENTRIES);

        assert!(matches!(cache.get(0), Some(None)), "a failure is a hit");
        cache.insert(MAX_SCENE_IMAGE_CACHE_ENTRIES as u64, None);
        assert_eq!(cache.entries.len(), MAX_SCENE_IMAGE_CACHE_ENTRIES);
        assert!(cache.entries.contains_key(&0), "the hot entry stays cached");
        assert!(
            !cache.entries.contains_key(&1),
            "the oldest entry is evicted"
        );
    }

    #[test]
    fn scene_image_cache_evicts_by_decoded_byte_budget() {
        let mut cache = SceneImageCache::default();
        let half = MAX_SCENE_IMAGE_CACHE_BYTES / 2;
        cache.insert_with_bytes(0, None, half);
        cache.insert_with_bytes(1, None, half);
        assert!(matches!(cache.get(0), Some(None)));

        cache.insert_with_bytes(2, None, 1);
        assert!(
            cache.entries.contains_key(&0),
            "the hot texture is retained"
        );
        assert!(
            !cache.entries.contains_key(&1),
            "the cold texture frees bytes"
        );
        assert!(cache.entries.contains_key(&2));
        assert!(cache.total_bytes <= MAX_SCENE_IMAGE_CACHE_BYTES);
    }
}
