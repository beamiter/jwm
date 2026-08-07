//! Shapes the annotation overlay can draw besides strokes.
//!
//! The overlay started as polylines only, which is all the free-draw
//! annotation mode ever needs. The screenshot editor needs more: a redaction
//! bar is a filled rectangle, a counter is a disc with a numeral in it, and a
//! label is text. Rasterising those into polylines on the window-manager side
//! would mean hundreds of two-point strokes rebuilt on every pointer motion,
//! so instead they travel as what they are and each compositor draws them with
//! the rounded-rect and text programs it already has for its own UI.
//!
//! Both compositors keep their own stroke types (they predate this module and
//! differ in how they store points); these two are shared, because they are
//! pure data with no GL in them.

/// A filled, optionally rounded rectangle in screen pixels.
///
/// `radius` is what makes one type cover both the redaction bar and the
/// counter bubble: a square whose radius is half its side is a disc.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AnnotationQuad {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub radius: f32,
    pub color: [f32; 4],
}

impl AnnotationQuad {
    /// A disc of `radius` centred on `(cx, cy)`.
    #[must_use]
    pub fn disc(cx: f32, cy: f32, radius: f32, color: [f32; 4]) -> Self {
        Self {
            x: cx - radius,
            y: cy - radius,
            w: radius * 2.0,
            h: radius * 2.0,
            radius,
            color,
        }
    }

    /// Whether this quad is worth handing to the renderer at all.
    #[must_use]
    pub fn is_drawable(&self) -> bool {
        self.x.is_finite()
            && self.y.is_finite()
            && self.w.is_finite()
            && self.h.is_finite()
            && self.w > 0.0
            && self.h > 0.0
    }
}

/// A run of text drawn in the UI font at a point size.
///
/// `anchor_center` is the difference between a screenshot label (which grows
/// right and down from where you clicked) and a counter's numeral (which has
/// to sit in the middle of its bubble whatever its digit count).
#[derive(Clone, Debug, PartialEq)]
pub struct AnnotationLabel {
    pub x: f32,
    pub y: f32,
    pub size: f32,
    pub color: [f32; 4],
    pub text: String,
    pub anchor_center: bool,
}

impl AnnotationLabel {
    /// Where a rasterised `(w, h)` texture goes for this label.
    #[must_use]
    pub fn origin(&self, w: f32, h: f32) -> (f32, f32) {
        if self.anchor_center {
            ((self.x - w * 0.5).round(), (self.y - h * 0.5).round())
        } else {
            (self.x.round(), self.y.round())
        }
    }

    #[must_use]
    pub fn is_drawable(&self) -> bool {
        !self.text.is_empty() && self.size.is_finite() && self.size > 0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disc_is_a_square_rounded_to_half_its_side() {
        let disc = AnnotationQuad::disc(100.0, 50.0, 12.0, [1.0, 0.0, 0.0, 1.0]);
        assert_eq!((disc.x, disc.y, disc.w, disc.h), (88.0, 38.0, 24.0, 24.0));
        assert_eq!(disc.radius, disc.w * 0.5);
        assert!(disc.is_drawable());
    }

    #[test]
    fn degenerate_quads_and_labels_are_rejected_rather_than_drawn() {
        let bad = AnnotationQuad {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 10.0,
            radius: 0.0,
            color: [1.0; 4],
        };
        assert!(!bad.is_drawable());
        assert!(
            !AnnotationQuad {
                x: f32::NAN,
                w: 5.0,
                ..bad
            }
            .is_drawable()
        );

        let label = AnnotationLabel {
            x: 0.0,
            y: 0.0,
            size: 12.0,
            color: [1.0; 4],
            text: String::new(),
            anchor_center: false,
        };
        assert!(!label.is_drawable());
        assert!(
            AnnotationLabel {
                text: "hi".to_owned(),
                ..label.clone()
            }
            .is_drawable()
        );
        assert!(
            !AnnotationLabel {
                text: "hi".to_owned(),
                size: 0.0,
                ..label
            }
            .is_drawable()
        );
    }

    /// A centred label must not drift when its texture is an odd size — the
    /// counter's numeral sits inside a circle, where half a pixel shows.
    #[test]
    fn centering_rounds_to_whole_pixels() {
        let centred = AnnotationLabel {
            x: 100.0,
            y: 50.0,
            size: 12.0,
            color: [1.0; 4],
            text: "7".to_owned(),
            anchor_center: true,
        };
        assert_eq!(centred.origin(9.0, 13.0), (96.0, 44.0));

        let corner = AnnotationLabel {
            anchor_center: false,
            ..centred
        };
        assert_eq!(corner.origin(9.0, 13.0), (100.0, 50.0));
    }
}
