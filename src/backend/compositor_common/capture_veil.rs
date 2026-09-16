//! Shared capture-selection veil geometry.
//!
//! Screenshot snap preview and the recording crop cue both want the same
//! "premium" language: darken everything outside the pick, leave the pick
//! itself clear (or lightly tinted), and draw a blue outline. The four
//! outside rectangles are computed here so X11 and Wayland cannot drift.

/// Outside scrim colour — dark enough to isolate the pick on a busy desktop
/// without turning the dim into a solid blackout.
pub(crate) const CAPTURE_SCRIM: [f32; 4] = [0.02, 0.04, 0.08, 0.52];

/// Soft blue wash drawn *inside* the pick so it still reads as selected even
/// when the desktop behind it is bright. Kept lighter than the pre-veil fill
/// so the content of the window stays readable.
pub(crate) const CAPTURE_HOLE_WASH: [f32; 4] = [0.30, 0.55, 1.0, 0.12];

/// Four screen-space rects `(x, y, w, h)` covering everything except `hole`.
/// Degenerate / off-screen holes yield a single full-screen scrim.
#[must_use]
pub(crate) fn outside_dim_rects(
    screen_w: f32,
    screen_h: f32,
    hole: (f32, f32, f32, f32),
) -> Vec<(f32, f32, f32, f32)> {
    let (hx, hy, hw, hh) = hole;
    if screen_w <= 0.0 || screen_h <= 0.0 {
        return Vec::new();
    }
    if hw <= 0.0 || hh <= 0.0 {
        return vec![(0.0, 0.0, screen_w, screen_h)];
    }

    let left = hx.clamp(0.0, screen_w);
    let right = (hx + hw).clamp(0.0, screen_w);
    let top = hy.clamp(0.0, screen_h);
    let bottom = (hy + hh).clamp(0.0, screen_h);
    if right <= left || bottom <= top {
        return vec![(0.0, 0.0, screen_w, screen_h)];
    }

    let mut rects = Vec::with_capacity(4);
    if top > 0.0 {
        rects.push((0.0, 0.0, screen_w, top));
    }
    if bottom < screen_h {
        rects.push((0.0, bottom, screen_w, screen_h - bottom));
    }
    if left > 0.0 {
        rects.push((0.0, top, left, bottom - top));
    }
    if right < screen_w {
        rects.push((right, top, screen_w - right, bottom - top));
    }
    rects
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outside_rects_cover_everything_but_the_hole() {
        let rects = outside_dim_rects(100.0, 80.0, (20.0, 10.0, 30.0, 40.0));
        assert_eq!(
            rects,
            vec![
                (0.0, 0.0, 100.0, 10.0),
                (0.0, 50.0, 100.0, 30.0),
                (0.0, 10.0, 20.0, 40.0),
                (50.0, 10.0, 50.0, 40.0),
            ]
        );
    }

    #[test]
    fn empty_hole_dims_the_whole_screen() {
        assert_eq!(
            outside_dim_rects(100.0, 80.0, (10.0, 10.0, 0.0, 20.0)),
            vec![(0.0, 0.0, 100.0, 80.0)]
        );
    }

    #[test]
    fn hole_flush_with_edges_drops_empty_sides() {
        let rects = outside_dim_rects(100.0, 80.0, (0.0, 0.0, 100.0, 40.0));
        assert_eq!(rects, vec![(0.0, 40.0, 100.0, 40.0)]);
    }
}
