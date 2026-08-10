//! Protocol- and renderer-independent 3D prism geometry.
//!
//! The X11 compositor owns the GPU programs and draw policy. This module owns
//! only the camera, polygon metrics, transforms, and painter-order pieces so
//! other compositors can reuse the same geometry without depending on X11 or
//! `glow` types.

use super::math::{
    mat4_mul, perspective_matrix, rotate_x_matrix, rotate_y_matrix, translate_matrix,
};
use std::f32::consts::{FRAC_PI_4, PI, TAU};

/// Fewest sides that still enclose a volume.
pub(crate) const MIN_PRISM_SIDES: usize = 3;
/// Most sides before the front face gets too small to recognize a window on.
pub(crate) const MAX_PRISM_SIDES: usize = 6;

/// Camera and polygon metrics for one prism, framed so the front face always
/// covers the same share of the monitor whatever the side count is.
pub(crate) struct PrismCamera {
    pub(crate) view: [f32; 16],
    pub(crate) persp: [f32; 16],
    /// Camera position in world space, for the per-fragment lighting.
    pub(crate) eye: [f32; 3],
    /// Half-width of a face; the half-height is always 1.0.
    pub(crate) face_aspect: f32,
    /// Distance from the prism axis to a face plane.
    pub(crate) apothem: f32,
    /// Distance from the prism axis to a face edge.
    pub(crate) circumradius: f32,
    pub(crate) sides: usize,
    /// Vanishing line of every horizontal plane, 0 = top of the monitor.
    pub(crate) horizon: f32,
    /// How far the camera is from the prism axis.
    distance: f32,
    /// Focal length in half-heights, i.e. `1 / tan(fov / 2)`.
    fit: f32,
    pitch: f32,
}

impl PrismCamera {
    /// Frame a `sides`-gon whose faces have the given aspect.
    ///
    /// `fill` is the share of the monitor height the front face should cover,
    /// `pitch` how far the eye is tipped down (it has to clear the top cap or
    /// the prism looks like a flat card), and `dolly` an extra pull-back used
    /// while the prism spins.
    pub(crate) fn frame(face_aspect: f32, sides: usize, fill: f32, pitch: f32, dolly: f32) -> Self {
        let sides = sides.clamp(MIN_PRISM_SIDES, MAX_PRISM_SIDES);
        let half_step = PI / sides as f32;
        let apothem = face_aspect / half_step.tan();
        let circumradius = face_aspect / half_step.sin();

        let fov_y = FRAC_PI_4;
        let fit = 1.0 / (fov_y * 0.5).tan();
        let distance = apothem + (fit / fill.max(0.05)) * (1.0 + dolly);
        let persp = perspective_matrix(fov_y, face_aspect, 0.1, distance * 6.0);
        let view = mat4_mul(
            &translate_matrix(0.0, 0.0, -distance),
            &rotate_x_matrix(pitch),
        );

        Self {
            view,
            persp,
            eye: [0.0, distance * pitch.sin(), distance * pitch.cos()],
            face_aspect,
            apothem,
            circumradius,
            sides,
            horizon: (0.5 - 0.5 * fit * pitch.tan()).clamp(0.05, 0.95),
            distance,
            fit,
            pitch,
        }
    }

    /// Frame a single card centered on the origin, for the transitions that fly
    /// one workspace panel around instead of turning a closed solid.
    pub(crate) fn card(face_aspect: f32, fill: f32, pitch: f32) -> Self {
        let mut camera = Self::frame(face_aspect, 4, fill, pitch, 0.0);
        // Pull the camera in by the apothem the prism would have had: the card
        // sits on the axis, not on a face plane.
        camera.distance -= camera.apothem;
        camera.apothem = 0.0;
        camera.circumradius = face_aspect;
        camera.view = mat4_mul(
            &translate_matrix(0.0, 0.0, -camera.distance),
            &rotate_x_matrix(pitch),
        );
        camera.eye = [
            0.0,
            camera.distance * pitch.sin(),
            camera.distance * pitch.cos(),
        ];
        camera
    }

    /// Clip-space transform for a model placed in this camera.
    pub(crate) fn mvp(&self, model: &[f32; 16]) -> [f32; 16] {
        mat4_mul(&self.persp, &mat4_mul(&self.view, model))
    }

    /// How far to raise the prism so its front bottom edge lands on `base_line`
    /// (0 = top of the monitor, 1 = bottom).
    ///
    /// Solving for this beats a constant offset: a six-sided prism sits further
    /// from the camera than a cube, and a fixed lift would push its base off
    /// the bottom of the screen.
    pub(crate) fn lift_for_base_line(&self, base_line: f32) -> f32 {
        let ndc = 1.0 - 2.0 * base_line;
        let (sin_pitch, cos_pitch) = self.pitch.sin_cos();
        let divisor = self.fit * cos_pitch + ndc * sin_pitch;
        if divisor.abs() <= 1.0e-4 {
            return 0.0;
        }
        let bottom = (ndc * (self.distance - self.apothem * cos_pitch)
            + self.fit * self.apothem * sin_pitch)
            / divisor;
        // This is an exact projection solve, not an animation input. The old
        // `1.2` ceiling clipped the solution for a six-sided prism on a 32:9
        // monitor, moving its intended 84% baseline down to about 89% and
        // crowding the reflection/title. All inputs are already finite and
        // the near-singular divisor is guarded above, so preserving the solve
        // is both safer and visually stable across output aspect ratios.
        bottom + 1.0
    }

    /// Project a model-space point to monitor-normalized screen coordinates
    /// (0,0 = top left, 1,1 = bottom right).
    pub(crate) fn project(&self, model: &[f32; 16], point: [f32; 3]) -> (f32, f32) {
        let mvp = mat4_mul(&self.persp, &mat4_mul(&self.view, model));
        let [x, y, z] = point;
        let clip_x = mvp[0] * x + mvp[4] * y + mvp[8] * z + mvp[12];
        let clip_y = mvp[1] * x + mvp[5] * y + mvp[9] * z + mvp[13];
        let clip_w = mvp[3] * x + mvp[7] * y + mvp[11] * z + mvp[15];
        if clip_w.abs() <= 1.0e-6 {
            return (0.5, 0.5);
        }
        ((clip_x / clip_w) * 0.5 + 0.5, 0.5 - (clip_y / clip_w) * 0.5)
    }
}

/// One drawable piece of the prism, resolved into clip space and depth-sorted
/// with the painter's algorithm (the compositor has no depth buffer).
pub(crate) struct PrismPiece {
    pub(crate) model: [f32; 16],
    pub(crate) mvp: [f32; 16],
    /// View-space z of the piece center; ascending order draws back to front.
    pub(crate) depth: f32,
    /// 1.0 when the piece points straight at the camera, negative when it faces
    /// away.
    pub(crate) facing: f32,
    pub(crate) kind: PrismKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PrismKind {
    /// Face slot around the prism.
    Face { slot: usize },
    /// Polygon cap closing the prism, at model-space y = ±1.
    Cap { top: bool },
}

/// Transform a column-major matrix by a homogeneous vector, keeping xyz.
pub(crate) fn mat4_apply(m: &[f32; 16], v: [f32; 4]) -> [f32; 3] {
    [
        m[0] * v[0] + m[4] * v[1] + m[8] * v[2] + m[12] * v[3],
        m[1] * v[0] + m[5] * v[1] + m[9] * v[2] + m[13] * v[3],
        m[2] * v[0] + m[6] * v[1] + m[10] * v[2] + m[14] * v[3],
    ]
}

/// Cosine-like facing term of a surface, given its center and normal in view
/// space. The camera sits at the view-space origin.
fn facing_term(center: [f32; 3], normal: [f32; 3]) -> f32 {
    let center_len = (center[0] * center[0] + center[1] * center[1] + center[2] * center[2]).sqrt();
    let normal_len = (normal[0] * normal[0] + normal[1] * normal[1] + normal[2] * normal[2]).sqrt();
    if center_len <= f32::EPSILON || normal_len <= f32::EPSILON {
        return 0.0;
    }
    let dot = center[0] * normal[0] + center[1] * normal[1] + center[2] * normal[2];
    -dot / (center_len * normal_len)
}

/// Mirror through the horizontal plane at `floor_y`.
pub(crate) fn mirror_matrix(floor_y: f32) -> [f32; 16] {
    #[rustfmt::skip]
    let m = [
        1.0, 0.0,  0.0, 0.0,
        0.0, -1.0, 0.0, 0.0,
        0.0, 0.0,  1.0, 0.0,
        0.0, 2.0 * floor_y, 0.0, 1.0,
    ];
    m
}

/// Build every face and cap of the prism for one pass.
///
/// `model_base` already carries the global rotation, any scaling and, for the
/// reflection pass, the mirror about the floor plane.
pub(crate) fn build_prism_pieces(camera: &PrismCamera, model_base: &[f32; 16]) -> Vec<PrismPiece> {
    // Camera framing and polygon construction must use one canonical side
    // count. Accepting it twice allowed callers to frame a cube but build a
    // hexagon around the cube's apothem, recreating the very seam drift this
    // shared module is meant to prevent.
    let sides = camera.sides;
    let step = TAU / sides as f32;
    let mut pieces: Vec<PrismPiece> = Vec::with_capacity(sides + 2);

    for slot in 0..sides {
        let face_model = mat4_mul(
            model_base,
            &mat4_mul(
                &rotate_y_matrix(slot as f32 * step),
                &translate_matrix(0.0, 0.0, camera.apothem),
            ),
        );
        let mv = mat4_mul(&camera.view, &face_model);
        let center = [mv[12], mv[13], mv[14]];
        let normal = [mv[8], mv[9], mv[10]];
        pieces.push(PrismPiece {
            mvp: mat4_mul(&camera.persp, &mv),
            model: face_model,
            depth: mv[14],
            facing: facing_term(center, normal),
            kind: PrismKind::Face { slot },
        });
    }

    for top in [true, false] {
        let y = if top { 1.0 } else { -1.0 };
        let mv = mat4_mul(&camera.view, model_base);
        let center = mat4_apply(&mv, [0.0, y, 0.0, 1.0]);
        let normal = [mv[4] * y, mv[5] * y, mv[6] * y];
        pieces.push(PrismPiece {
            mvp: mat4_mul(&camera.persp, &mv),
            model: *model_base,
            depth: center[2],
            facing: facing_term(center, normal),
            kind: PrismKind::Cap { top },
        });
    }

    pieces.sort_by(|a, b| {
        a.depth
            .partial_cmp(&b.depth)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    fn camera(sides: usize) -> PrismCamera {
        PrismCamera::frame(16.0 / 9.0, sides, 0.56, 0.27, 0.0)
    }

    #[test]
    fn polygon_metrics_follow_the_side_count() {
        // A four-sided prism with square-ish faces is a box: the distance to a
        // face plane equals the face half-width.
        let cube = camera(4);
        assert!((cube.apothem - cube.face_aspect).abs() < 1.0e-4);
        // Corners are always further out than face centers, and adding sides
        // pushes both away from the axis.
        for sides in MIN_PRISM_SIDES..=MAX_PRISM_SIDES {
            let cam = camera(sides);
            assert!(cam.circumradius > cam.apothem);
        }
        assert!(camera(6).apothem > camera(3).apothem);
    }

    #[test]
    fn adjacent_face_edges_close_for_every_supported_solid() {
        let identity = translate_matrix(0.0, 0.0, 0.0);
        for sides in MIN_PRISM_SIDES..=MAX_PRISM_SIDES {
            let cam = camera(sides);
            let pieces = build_prism_pieces(&cam, &identity);
            for slot in 0..sides {
                let face = pieces
                    .iter()
                    .find(|piece| piece.kind == PrismKind::Face { slot })
                    .expect("face slot");
                let next_slot = (slot + 1) % sides;
                let next = pieces
                    .iter()
                    .find(|piece| piece.kind == PrismKind::Face { slot: next_slot })
                    .expect("next face slot");
                let right = mat4_apply(&face.model, [cam.face_aspect, 0.0, 0.0, 1.0]);
                let next_left = mat4_apply(&next.model, [-cam.face_aspect, 0.0, 0.0, 1.0]);
                let gap = ((right[0] - next_left[0]).powi(2)
                    + (right[1] - next_left[1]).powi(2)
                    + (right[2] - next_left[2]).powi(2))
                .sqrt();
                assert!(
                    gap < 1.0e-4,
                    "{sides}-gon seam {slot}->{next_slot} left a {gap} world-unit gap"
                );
            }
        }
    }

    #[test]
    fn side_count_is_clamped_to_a_drawable_range() {
        assert_eq!(camera(1).sides, MIN_PRISM_SIDES);
        assert_eq!(camera(99).sides, MAX_PRISM_SIDES);
    }

    #[test]
    fn looking_down_puts_the_horizon_above_the_center() {
        let level = PrismCamera::frame(1.6, 4, 0.56, 0.0, 0.0);
        assert!((level.horizon - 0.5).abs() < 1.0e-4);
        assert!(camera(4).horizon < 0.5);
    }

    #[test]
    fn the_lift_lands_the_front_bottom_edge_on_the_base_line() {
        for sides in MIN_PRISM_SIDES..=MAX_PRISM_SIDES {
            let cam = camera(sides);
            let lift = cam.lift_for_base_line(0.84);
            let model = translate_matrix(0.0, lift, 0.0);
            let (_, screen_y) = cam.project(&model, [0.0, -1.0, cam.apothem]);
            assert!(
                (screen_y - 0.84).abs() < 1.0e-3,
                "{sides} sides landed at {screen_y}"
            );
        }
    }

    #[test]
    fn framing_stays_finite_and_lands_on_portrait_and_ultrawide_views() {
        for aspect in [9.0 / 16.0, 1.0, 16.0 / 9.0, 32.0 / 9.0] {
            for sides in MIN_PRISM_SIDES..=MAX_PRISM_SIDES {
                let cam = PrismCamera::frame(aspect, sides, 0.56, 0.27, 0.0);
                assert!(cam.view.into_iter().all(f32::is_finite));
                assert!(cam.persp.into_iter().all(f32::is_finite));
                assert!(cam.eye.into_iter().all(f32::is_finite));
                let lift = cam.lift_for_base_line(0.84);
                assert!(lift.is_finite());
                let model = translate_matrix(0.0, lift, 0.0);
                let (screen_x, screen_y) = cam.project(&model, [0.0, -1.0, cam.apothem]);
                assert!(screen_x.is_finite() && screen_y.is_finite());
                assert!(
                    (screen_y - 0.84).abs() < 1.0e-3,
                    "aspect {aspect}, {sides} sides landed at {screen_y}"
                );
            }
        }
    }

    #[test]
    fn pieces_are_sorted_back_to_front_with_the_front_face_last() {
        let cam = camera(4);
        let pieces = build_prism_pieces(&cam, &translate_matrix(0.0, 0.0, 0.0));
        assert_eq!(pieces.len(), 6, "four faces and two caps");
        for pair in pieces.windows(2) {
            assert!(pair[0].depth <= pair[1].depth);
        }
        let last = pieces.last().expect("non-empty");
        assert_eq!(last.kind, PrismKind::Face { slot: 0 });
        assert!(last.facing > 0.9, "slot 0 faces the camera");
    }

    #[test]
    fn only_the_top_cap_faces_a_downward_looking_camera() {
        let cam = camera(4);
        let pieces = build_prism_pieces(&cam, &translate_matrix(0.0, 0.0, 0.0));
        let top = pieces
            .iter()
            .find(|p| p.kind == PrismKind::Cap { top: true })
            .expect("top cap");
        let bottom = pieces
            .iter()
            .find(|p| p.kind == PrismKind::Cap { top: false })
            .expect("bottom cap");
        assert!(top.facing > 0.0, "the eye clears the top cap");
        assert!(bottom.facing < 0.0);
    }

    #[test]
    fn mirroring_flips_y_about_the_floor_plane() {
        let mirror = mirror_matrix(-0.75);
        assert_eq!(mat4_apply(&mirror, [0.0, -0.75, 0.0, 1.0])[1], -0.75);
        assert_eq!(mat4_apply(&mirror, [0.0, 1.0, 0.0, 1.0])[1], -2.5);
    }
}
