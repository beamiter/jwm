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
use xbar_core::presentation::{
    ImageSource, Rect, Rgba, Scene, SceneNode, Size, TextAlign, TextMeasurer,
};

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
/// egui hands out no place to keep renderer state across frames, so the handle
/// lives in the context's own temporary store keyed by the scene's stable image
/// key. A file that cannot be decoded is remembered as `None`, so a broken icon
/// costs one failed decode rather than one per frame.
fn image_texture(ctx: &Context, source: &ImageSource) -> Option<TextureHandle> {
    let id = Id::new(("xbar-scene-image", source.key));
    if let Some(cached) = ctx.data(|data| data.get_temp::<Option<TextureHandle>>(id)) {
        return cached;
    }
    let texture = decode_image(&source.path).map(|image| {
        ctx.load_texture(
            format!("xbar-scene-image-{}", source.key),
            image,
            TextureOptions::LINEAR,
        )
    });
    ctx.data_mut(|data| data.insert_temp(id, texture.clone()));
    texture
}

fn decode_image(path: &std::path::Path) -> Option<ColorImage> {
    let rgba = image::open(path).ok()?.into_rgba8();
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
