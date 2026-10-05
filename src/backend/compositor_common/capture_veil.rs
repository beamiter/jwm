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
/// Corner radius of the clear hole and of the outline that names it.
pub(crate) const CAPTURE_HOLE_RADIUS: f32 = 8.0;
/// Stroke of the hole's outline, in pixels.
pub(crate) const CAPTURE_OUTLINE_WIDTH: f32 = 2.5;
/// Edge of a resize handle drawn on the hole.
pub(crate) const CAPTURE_HANDLE_SIZE: f32 = 10.0;

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

/// Eight resize-handle rects around `hole`: corners then edge midpoints,
/// clockwise from the top-left. Empty when the hole is degenerate. On a
/// hole smaller than eight full-size handles, the squares shrink so they
/// no longer sit on top of each other; below a few pixels they drop out
/// rather than becoming untouchable specks.
#[must_use]
pub(crate) fn handle_rects(hole: (f32, f32, f32, f32)) -> Vec<(f32, f32, f32, f32)> {
    let (x, y, w, h) = hole;
    if !(x.is_finite() && y.is_finite() && w.is_finite() && h.is_finite()) || w <= 0.0 || h <= 0.0 {
        return Vec::new();
    }
    let s = handle_size(w, h);
    if s <= 0.0 {
        return Vec::new();
    }
    let half = s * 0.5;
    let mut pts = vec![
        (x, y),
        (x + w, y),
        (x + w, y + h),
        (x, y + h),
    ];
    // Mid-edge grips need a clear gap from the corners; otherwise they
    // land on the same pixels as a corner and steal the first hit.
    if w >= 2.5 * s && h >= 2.5 * s {
        pts = vec![
            (x, y),
            (x + w * 0.5, y),
            (x + w, y),
            (x + w, y + h * 0.5),
            (x + w, y + h),
            (x + w * 0.5, y + h),
            (x, y + h),
            (x, y + h * 0.5),
        ];
    }
    pts.into_iter()
        .map(|(hx, hy)| (hx - half, hy - half, s, s))
        .collect()
}

#[must_use]
fn handle_size(w: f32, h: f32) -> f32 {
    let fitted = CAPTURE_HANDLE_SIZE.min(w / 3.0).min(h / 3.0);
    if fitted >= 4.0 {
        fitted
    } else {
        let corner = CAPTURE_HANDLE_SIZE.min(w * 0.45).min(h * 0.45);
        if corner >= 3.0 { corner } else { 0.0 }
    }
}

/// Which handle, if any, contains `(px, py)`. Interior of the hole that is
/// not on a handle is a miss — dragging there moves the pick, not a corner.
#[cfg(test)]
#[must_use]
fn handle_at(hole: (f32, f32, f32, f32), px: f32, py: f32) -> Option<usize> {
    handle_rects(hole)
        .into_iter()
        .position(|(x, y, w, h)| px >= x && px < x + w && py >= y && py < y + h)
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

    #[test]
    fn handles_sit_on_the_hole_corners_and_edges() {
        let hole = (20.0, 10.0, 40.0, 30.0);
        let handles = handle_rects(hole);
        assert_eq!(handles.len(), 8);
        assert_eq!(handles[0], (15.0, 5.0, 10.0, 10.0));
        assert_eq!(handles[2], (55.0, 5.0, 10.0, 10.0));
        assert_eq!(handle_at(hole, 20.0, 10.0), Some(0));
        assert_eq!(
            handle_at(hole, 40.0, 25.0),
            None,
            "interior is not a handle"
        );
        assert!(handle_rects((0.0, 0.0, 0.0, 10.0)).is_empty());
        let _ = (CAPTURE_HOLE_RADIUS, CAPTURE_OUTLINE_WIDTH);
    }

    #[test]
    fn tiny_holes_shrink_handles_instead_of_stacking_them() {
        let hole = (10.0, 10.0, 16.0, 16.0);
        let handles = handle_rects(hole);
        assert!(!handles.is_empty());
        let s = handles[0].2;
        assert!(s < CAPTURE_HANDLE_SIZE);
        assert!(s >= 3.0);
        for (i, a) in handles.iter().enumerate() {
            for (j, b) in handles.iter().enumerate() {
                if i >= j {
                    continue;
                }
                let (ax, ay, aw, ah) = *a;
                let (bx, by, bw, bh) = *b;
                let overlap = ax < bx + bw && ax + aw > bx && ay < by + bh && ay + ah > by;
                assert!(!overlap, "handle {i} overlaps {j}");
            }
        }
        assert!(handle_rects((0.0, 0.0, 2.0, 2.0)).is_empty());
    }
}
