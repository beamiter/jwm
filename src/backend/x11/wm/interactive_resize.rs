//! Transport-independent geometry for interactive X11 moves and resizes.
//!
//! Both X11 transports receive pointer deltas from their active grab. Keeping
//! the edge calculation here prevents their move/resize loops from drifting
//! apart, especially for the left and top edges whose opposite side must stay
//! fixed while the window origin moves.

use crate::backend::api::{Geometry, ResizeEdge};

const MIN_SIZE: i64 = 1;
const MAX_SIZE: i64 = u32::MAX as i64;

/// Apply a pointer delta to a window origin without wrapping at the signed X11
/// coordinate limits.
pub(crate) fn interactive_move_origin(start: Geometry, dx: i32, dy: i32) -> (i32, i32) {
    let offset = |origin: i32, delta: i32| {
        (i64::from(origin) + i64::from(delta)).clamp(i64::from(i32::MIN), i64::from(i32::MAX))
            as i32
    };
    (offset(start.x, dx), offset(start.y, dy))
}

/// Apply a root-pointer delta to `start` from the requested resize `edge`.
///
/// The side opposite the dragged edge remains fixed. Width and height never
/// fall below one pixel, even when the pointer crosses the opposite edge.
/// Arithmetic is performed in `i64` so extreme pointer coordinates cannot
/// overflow the geometry calculation before it is clamped back to X11's
/// public `i32`/`u32` representation.
pub(crate) fn interactive_resize_geometry(
    start: Geometry,
    edge: ResizeEdge,
    dx: i32,
    dy: i32,
) -> Geometry {
    let left = i64::from(start.x);
    let top = i64::from(start.y);
    let width = i64::from(start.w).max(MIN_SIZE);
    let height = i64::from(start.h).max(MIN_SIZE);
    let right = left + width;
    let bottom = top + height;

    let moves_left = matches!(
        edge,
        ResizeEdge::Left | ResizeEdge::TopLeft | ResizeEdge::BottomLeft
    );
    let moves_right = matches!(
        edge,
        ResizeEdge::Right | ResizeEdge::TopRight | ResizeEdge::BottomRight
    );
    let moves_top = matches!(
        edge,
        ResizeEdge::Top | ResizeEdge::TopLeft | ResizeEdge::TopRight
    );
    let moves_bottom = matches!(
        edge,
        ResizeEdge::Bottom | ResizeEdge::BottomLeft | ResizeEdge::BottomRight
    );

    let (x, w) = if moves_left {
        // Keep `right` fixed. The lower bound also caps the resulting width at
        // u32::MAX, while the upper bound prevents crossing the fixed edge.
        let min_left = (right - MAX_SIZE).max(i64::from(i32::MIN));
        let max_left = (right - MIN_SIZE).min(i64::from(i32::MAX));
        let new_left = (left + i64::from(dx)).clamp(min_left, max_left);
        (new_left as i32, (right - new_left) as u32)
    } else if moves_right {
        // Keep `left` fixed and move only the right edge.
        let new_right = (right + i64::from(dx)).clamp(left + MIN_SIZE, left + MAX_SIZE);
        (start.x, (new_right - left) as u32)
    } else {
        (start.x, width as u32)
    };

    let (y, h) = if moves_top {
        // Keep `bottom` fixed, mirroring the left-edge calculation above.
        let min_top = (bottom - MAX_SIZE).max(i64::from(i32::MIN));
        let max_top = (bottom - MIN_SIZE).min(i64::from(i32::MAX));
        let new_top = (top + i64::from(dy)).clamp(min_top, max_top);
        (new_top as i32, (bottom - new_top) as u32)
    } else if moves_bottom {
        // Keep `top` fixed and move only the bottom edge.
        let new_bottom = (bottom + i64::from(dy)).clamp(top + MIN_SIZE, top + MAX_SIZE);
        (start.y, (new_bottom - top) as u32)
    } else {
        (start.y, height as u32)
    };

    Geometry {
        x,
        y,
        w,
        h,
        border: start.border,
    }
}

#[cfg(test)]
mod tests {
    use super::{interactive_move_origin, interactive_resize_geometry};
    use crate::backend::api::{Geometry, ResizeEdge};

    const START: Geometry = Geometry {
        x: 100,
        y: 200,
        w: 300,
        h: 200,
        border: 3,
    };

    fn tuple(geometry: Geometry) -> (i32, i32, u32, u32, u32) {
        (
            geometry.x,
            geometry.y,
            geometry.w,
            geometry.h,
            geometry.border,
        )
    }

    #[test]
    fn all_edges_keep_their_opposite_sides_fixed() {
        let cases = [
            (ResizeEdge::Top, (100, 170, 300, 230, 3)),
            (ResizeEdge::Bottom, (100, 200, 300, 170, 3)),
            (ResizeEdge::Left, (140, 200, 260, 200, 3)),
            (ResizeEdge::Right, (100, 200, 340, 200, 3)),
            (ResizeEdge::TopLeft, (140, 170, 260, 230, 3)),
            (ResizeEdge::TopRight, (100, 170, 340, 230, 3)),
            (ResizeEdge::BottomLeft, (140, 200, 260, 170, 3)),
            (ResizeEdge::BottomRight, (100, 200, 340, 170, 3)),
        ];

        for (edge, expected) in cases {
            let resized = interactive_resize_geometry(START, edge, 40, -30);
            assert_eq!(tuple(resized), expected, "edge={edge:?}");
        }
    }

    #[test]
    fn every_dragged_edge_stops_at_one_pixel() {
        let cases = [
            (ResizeEdge::Top, 0, i32::MAX, (100, 399, 300, 1, 3)),
            (ResizeEdge::Bottom, 0, i32::MIN, (100, 200, 300, 1, 3)),
            (ResizeEdge::Left, i32::MAX, 0, (399, 200, 1, 200, 3)),
            (ResizeEdge::Right, i32::MIN, 0, (100, 200, 1, 200, 3)),
            (ResizeEdge::TopLeft, i32::MAX, i32::MAX, (399, 399, 1, 1, 3)),
            (
                ResizeEdge::TopRight,
                i32::MIN,
                i32::MAX,
                (100, 399, 1, 1, 3),
            ),
            (
                ResizeEdge::BottomLeft,
                i32::MAX,
                i32::MIN,
                (399, 200, 1, 1, 3),
            ),
            (
                ResizeEdge::BottomRight,
                i32::MIN,
                i32::MIN,
                (100, 200, 1, 1, 3),
            ),
        ];

        for (edge, dx, dy, expected) in cases {
            let resized = interactive_resize_geometry(START, edge, dx, dy);
            assert_eq!(tuple(resized), expected, "edge={edge:?}");
        }
    }

    #[test]
    fn zero_sized_input_is_normalized_to_the_minimum() {
        let resized = interactive_resize_geometry(
            Geometry {
                x: 7,
                y: 9,
                w: 0,
                h: 0,
                border: 2,
            },
            ResizeEdge::BottomRight,
            -100,
            -100,
        );

        assert_eq!(tuple(resized), (7, 9, 1, 1, 2));
    }

    #[test]
    fn move_origin_clamps_instead_of_wrapping_at_x11_limits() {
        assert_eq!(interactive_move_origin(START, 40, -30), (140, 170));
        assert_eq!(
            interactive_move_origin(
                Geometry {
                    x: i32::MAX - 2,
                    y: i32::MIN + 2,
                    ..START
                },
                10,
                -10,
            ),
            (i32::MAX, i32::MIN)
        );
    }
}
