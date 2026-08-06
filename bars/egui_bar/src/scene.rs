//! egui renderer for the shared `xbar_core` presentation scene.
//!
//! Layout, colours, and hit regions all come from `xbar_core::presentation` —
//! the same display list `xcb_bar` hands to Cairo. Only the translation into
//! egui shapes lives here. That is what keeps this bar looking like the Cairo
//! bars instead of restating their design in a second widget tree, and it is
//! why adding a status cell to the core shows up here with no work.

use egui::{
    Color32, CornerRadius, FontFamily, FontId, Painter, Pos2, Rect as EguiRect, Shape,
    Stroke as EguiStroke, StrokeKind, Vec2,
};
use xbar_core::presentation::{Rect, Rgba, Scene, SceneNode, Size, TextAlign, TextMeasurer};

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
        let painter = painter.with_clip_rect(to_screen(visible, origin).intersect(painter.clip_rect()));
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
        }
    }
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
