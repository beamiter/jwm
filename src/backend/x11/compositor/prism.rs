//! Shared 3D prism: geometry, framing and drawing.
//!
//! Two effects stand on this — the Alt+Ctrl+Tab overview carousel and the
//! `cube` tag-switch transition. Both want the same Compiz-style object: lit
//! faces with beveled edges, polygon caps, a mirrored copy fading into a floor,
//! and a see-through body while it spins. Keeping one implementation is what
//! makes them look like the same cube rather than two similar ones.

use super::Compositor;
use super::CompositorConnection;
use super::math::{
    mat4_mul, perspective_matrix, rotate_x_matrix, rotate_y_matrix, translate_matrix,
};
use glow::HasContext;
use std::f32::consts::{FRAC_PI_4, PI, TAU};

/// Fewest sides that still enclose a volume.
pub(super) const MIN_PRISM_SIDES: usize = 3;
/// Most sides before the front face gets too small to recognize a window on.
pub(super) const MAX_PRISM_SIDES: usize = 6;

/// Accent shared by the skydome glow, the face bevels and the caps, so the
/// whole overlay reads as one lighting environment.
pub(super) const PRISM_ACCENT: [f32; 3] = [0.32, 0.62, 1.0];

/// Camera and polygon metrics for one prism, framed so the front face always
/// covers the same share of the monitor whatever the side count is.
pub(super) struct PrismCamera {
    pub(super) view: [f32; 16],
    pub(super) persp: [f32; 16],
    /// Camera position in world space, for the per-fragment lighting.
    pub(super) eye: [f32; 3],
    /// Half-width of a face; the half-height is always 1.0.
    pub(super) face_aspect: f32,
    /// Distance from the prism axis to a face plane.
    pub(super) apothem: f32,
    /// Distance from the prism axis to a face edge.
    pub(super) circumradius: f32,
    pub(super) sides: usize,
    /// Vanishing line of every horizontal plane, 0 = top of the monitor.
    pub(super) horizon: f32,
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
    pub(super) fn frame(face_aspect: f32, sides: usize, fill: f32, pitch: f32, dolly: f32) -> Self {
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
    pub(super) fn card(face_aspect: f32, fill: f32, pitch: f32) -> Self {
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
    pub(super) fn mvp(&self, model: &[f32; 16]) -> [f32; 16] {
        mat4_mul(&self.persp, &mat4_mul(&self.view, model))
    }

    /// How far to raise the prism so its front bottom edge lands on `base_line`
    /// (0 = top of the monitor, 1 = bottom).
    ///
    /// Solving for this beats a constant offset: a six-sided prism sits further
    /// from the camera than a cube, and a fixed lift would push its base off
    /// the bottom of the screen.
    pub(super) fn lift_for_base_line(&self, base_line: f32) -> f32 {
        let ndc = 1.0 - 2.0 * base_line;
        let (sin_pitch, cos_pitch) = self.pitch.sin_cos();
        let divisor = self.fit * cos_pitch + ndc * sin_pitch;
        if divisor.abs() <= 1.0e-4 {
            return 0.0;
        }
        let bottom = (ndc * (self.distance - self.apothem * cos_pitch)
            + self.fit * self.apothem * sin_pitch)
            / divisor;
        (bottom + 1.0).clamp(-0.5, 1.2)
    }

    /// Project a model-space point to monitor-normalized screen coordinates
    /// (0,0 = top left, 1,1 = bottom right).
    pub(super) fn project(&self, model: &[f32; 16], point: [f32; 3]) -> (f32, f32) {
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

/// What one face slot shows.
#[derive(Clone, Copy)]
pub(super) struct PrismFace {
    /// `None` draws the slot as a tinted glass panel instead of a window.
    pub(super) texture: Option<glow::Texture>,
    /// Sub-rectangle of the texture to map onto the face.
    pub(super) uv_rect: [f32; 4],
    /// Accent strength: 1.0 lights the whole rim, marking the selection.
    pub(super) accent: f32,
    /// Desaturation applied when the face is front-facing.
    pub(super) desat: f32,
    /// Base exposure. A workspace mid-transition is the user's actual desktop
    /// and should look like it; a switcher dims the faces it is not offering.
    pub(super) brightness: f32,
    /// How much rounded corner and lit bevel the face carries. A transition
    /// dials this in as it lifts off and back out as it lands, so the workspace
    /// it hands back has square corners again.
    pub(super) edge: f32,
    /// Whether `texture` carries a mipmap chain. Steeply angled cards minify
    /// hard, and sampling a full-resolution snapshot there aliases into stripes
    /// — but asking for a mipmap filter on a texture without one renders black,
    /// so the caller has to say.
    pub(super) mipmapped: bool,
}

impl Default for PrismFace {
    fn default() -> Self {
        Self {
            texture: None,
            uv_rect: [0.0, 0.0, 1.0, 1.0],
            accent: 0.0,
            desat: 0.0,
            brightness: 1.0,
            edge: 1.0,
            mipmapped: false,
        }
    }
}

/// One drawable piece of the prism, resolved into clip space and depth-sorted
/// with the painter's algorithm (the compositor has no depth buffer).
pub(super) struct PrismPiece {
    model: [f32; 16],
    pub(super) mvp: [f32; 16],
    /// View-space z of the piece center; ascending order draws back to front.
    depth: f32,
    /// 1.0 when the piece points straight at the camera, negative when it faces
    /// away.
    pub(super) facing: f32,
    pub(super) kind: PrismKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PrismKind {
    /// Face slot around the prism.
    Face { slot: usize },
    /// Polygon cap closing the prism, at model-space y = ±1.
    Cap { top: bool },
}

/// How one pass of the prism should be drawn.
pub(super) struct PrismPass {
    /// Global fade, e.g. the overview's entry/exit animation.
    pub(super) fade: f32,
    /// 0..1 rotation energy: turns the body see-through and dims the caps.
    pub(super) spin: f32,
    /// World-space height of the mirror the prism stands on.
    pub(super) floor_y: f32,
    /// Draw the mirrored copy rather than the solid prism.
    pub(super) reflect: bool,
}

/// Transform a column-major matrix by a homogeneous vector, keeping xyz.
fn mat4_apply(m: &[f32; 16], v: [f32; 4]) -> [f32; 3] {
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
pub(super) fn mirror_matrix(floor_y: f32) -> [f32; 16] {
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
pub(super) fn build_prism_pieces(
    camera: &PrismCamera,
    model_base: &[f32; 16],
    sides: usize,
) -> Vec<PrismPiece> {
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

impl<C: CompositorConnection> Compositor<C> {
    /// Push the uniforms that hold for every piece of every pass. Uniform state
    /// is per program, so it survives the program switches during a pass.
    pub(super) fn bind_prism_programs(&self, camera: &PrismCamera, time: f32) {
        unsafe {
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.use_program(Some(self.overview_face_program));
            self.gl
                .uniform_1_i32(self.overview_face_uniforms.texture.as_ref(), 0);
            self.gl.uniform_1_f32(
                self.overview_face_uniforms.aspect.as_ref(),
                camera.face_aspect,
            );
            self.gl.uniform_3_f32(
                self.overview_face_uniforms.camera.as_ref(),
                camera.eye[0],
                camera.eye[1],
                camera.eye[2],
            );
            self.gl
                .uniform_1_f32(self.overview_face_uniforms.time.as_ref(), time);

            self.gl.use_program(Some(self.overview_cap_program));
            self.gl.uniform_1_f32(
                self.overview_cap_uniforms.sides.as_ref(),
                camera.sides as f32,
            );
            self.gl.uniform_1_f32(
                self.overview_cap_uniforms.radius.as_ref(),
                camera.circumradius,
            );
            self.gl.uniform_3_f32(
                self.overview_cap_uniforms.accent.as_ref(),
                PRISM_ACCENT[0],
                PRISM_ACCENT[1],
                PRISM_ACCENT[2],
            );
            self.gl
                .uniform_1_f32(self.overview_cap_uniforms.time.as_ref(), time);
        }
    }

    /// Draw one lit card: a face of the prism, or a standalone workspace
    /// panel for the transitions that fly a single quad around.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn draw_prism_face(
        &self,
        mvp: &[f32; 16],
        model: &[f32; 16],
        face: &PrismFace,
        texture: glow::Texture,
        brightness: f32,
        alpha: f32,
        desat: f32,
        reflect: bool,
    ) {
        unsafe {
            self.gl.use_program(Some(self.overview_face_program));
            self.gl.uniform_matrix_4_f32_slice(
                self.overview_face_uniforms.mvp.as_ref(),
                false,
                mvp,
            );
            self.gl.uniform_matrix_4_f32_slice(
                self.overview_face_uniforms.model.as_ref(),
                false,
                model,
            );
            self.gl.uniform_4_f32(
                self.overview_face_uniforms.uv_rect.as_ref(),
                face.uv_rect[0],
                face.uv_rect[1],
                face.uv_rect[2],
                face.uv_rect[3],
            );
            self.gl.uniform_4_f32(
                self.overview_face_uniforms.accent.as_ref(),
                PRISM_ACCENT[0],
                PRISM_ACCENT[1],
                PRISM_ACCENT[2],
                face.accent,
            );
            self.gl
                .uniform_1_f32(self.overview_face_uniforms.brightness.as_ref(), brightness);
            self.gl
                .uniform_1_f32(self.overview_face_uniforms.alpha.as_ref(), alpha);
            self.gl
                .uniform_1_f32(self.overview_face_uniforms.desat.as_ref(), desat);
            self.gl.uniform_1_f32(
                self.overview_face_uniforms.reflect.as_ref(),
                if reflect { 1.0 } else { 0.0 },
            );
            self.gl.uniform_1_f32(
                self.overview_face_uniforms.glass.as_ref(),
                if face.texture.is_some() { 0.0 } else { 1.0 },
            );
            self.gl
                .uniform_1_f32(self.overview_face_uniforms.edge.as_ref(), face.edge);

            // Window textures are sampled with NEAREST everywhere else; these
            // effects scale them, so they need LINEAR.
            let min_filter = if face.mipmapped {
                glow::LINEAR_MIPMAP_LINEAR
            } else {
                glow::LINEAR
            };
            self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                min_filter as i32,
            );
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::LINEAR as i32,
            );
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::NEAREST as i32,
            );
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::NEAREST as i32,
            );
        }
    }

    /// Draw one depth-sorted pass of the prism.
    pub(super) fn draw_prism_pass(
        &self,
        camera: &PrismCamera,
        pieces: &[PrismPiece],
        faces: &[PrismFace],
        filler: Option<glow::Texture>,
        pass: &PrismPass,
    ) {
        // The reflection is a hint, not a second prism: it never shows the
        // see-through back faces and it disappears as the prism scales away.
        if pass.reflect && pass.fade < 0.35 {
            return;
        }

        for piece in pieces {
            match piece.kind {
                PrismKind::Face { slot } => {
                    let front = piece.facing > 0.0;
                    if pass.reflect && !front {
                        continue;
                    }
                    // Rotating the prism turns it to glass, so the faces behind
                    // show through — Compiz's transparent cube.
                    let alpha = if front {
                        1.0 - 0.38 * pass.spin
                    } else {
                        0.40 * pass.spin
                    } * pass.fade;
                    if alpha < 0.015 {
                        continue;
                    }

                    let face = faces.get(slot).copied().unwrap_or_default();
                    let Some(texture) = face.texture.or(filler) else {
                        continue;
                    };
                    // Faces turning away lose exposure, which is what gives the
                    // solid its shape without darkening the one being read.
                    let (brightness, desat) = if front {
                        (face.brightness * (0.70 + 0.30 * piece.facing), face.desat)
                    } else {
                        (face.brightness * 0.50, (face.desat + 0.55).min(1.0))
                    };
                    self.draw_prism_face(
                        &piece.mvp,
                        &piece.model,
                        &face,
                        texture,
                        brightness,
                        alpha,
                        desat,
                        pass.reflect,
                    );
                }
                PrismKind::Cap { top } => {
                    if piece.facing <= 0.02 {
                        continue;
                    }
                    let y = if top { 1.0 } else { -1.0 };
                    let mut alpha = (0.90 - 0.35 * pass.spin) * pass.fade;
                    if pass.reflect {
                        // Reflected caps fade with their distance below the
                        // floor: the underside stays, the far one vanishes.
                        let world_y = mat4_apply(&piece.model, [0.0, y, 0.0, 1.0])[1];
                        alpha *= 0.42 * (-(world_y - pass.floor_y).abs() * 1.6).exp();
                    }
                    if alpha < 0.015 {
                        continue;
                    }

                    unsafe {
                        self.gl.use_program(Some(self.overview_cap_program));
                        self.gl.uniform_matrix_4_f32_slice(
                            self.overview_cap_uniforms.mvp.as_ref(),
                            false,
                            &piece.mvp,
                        );
                        self.gl
                            .uniform_1_f32(self.overview_cap_uniforms.y.as_ref(), y);
                        self.gl.uniform_4_f32(
                            self.overview_cap_uniforms.color.as_ref(),
                            0.085,
                            0.105,
                            0.155,
                            alpha,
                        );
                        self.gl.uniform_1_f32(
                            self.overview_cap_uniforms.reflect.as_ref(),
                            if pass.reflect { 1.0 } else { 0.0 },
                        );
                        // Fan: center vertex plus one rim vertex per side, with
                        // the first rim vertex repeated to close the polygon.
                        let vertices = i32::try_from(camera.sides).unwrap_or(6) + 2;
                        self.gl.draw_arrays(glow::TRIANGLE_FAN, 0, vertices);
                    }
                }
            }
        }
    }

    /// Draw the skydome the prism lives in, over the given monitor rectangle.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn draw_prism_skydome(
        &self,
        proj: &[f32; 16],
        rect: [f32; 4],
        camera: &PrismCamera,
        ground: f32,
        angle: f32,
        opacity: f32,
        time: f32,
    ) {
        unsafe {
            self.gl.use_program(Some(self.overview_bg_program));
            self.gl.uniform_matrix_4_f32_slice(
                self.overview_bg_uniforms.projection.as_ref(),
                false,
                proj,
            );
            self.gl.uniform_4_f32(
                self.overview_bg_uniforms.rect.as_ref(),
                rect[0],
                rect[1],
                rect[2],
                rect[3],
            );
            self.gl
                .uniform_1_f32(self.overview_bg_uniforms.opacity.as_ref(), opacity);
            self.gl
                .uniform_1_f32(self.overview_bg_uniforms.angle.as_ref(), angle);
            self.gl
                .uniform_1_f32(self.overview_bg_uniforms.time.as_ref(), time);
            self.gl.uniform_2_f32(
                self.overview_bg_uniforms.ground.as_ref(),
                camera.horizon,
                ground,
            );
            self.gl.uniform_3_f32(
                self.overview_bg_uniforms.accent.as_ref(),
                PRISM_ACCENT[0],
                PRISM_ACCENT[1],
                PRISM_ACCENT[2],
            );
            self.gl.bind_vertex_array(Some(self.quad_vao));
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        }
    }
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
    fn pieces_are_sorted_back_to_front_with_the_front_face_last() {
        let cam = camera(4);
        let pieces = build_prism_pieces(&cam, &translate_matrix(0.0, 0.0, 0.0), 4);
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
        let pieces = build_prism_pieces(&cam, &translate_matrix(0.0, 0.0, 0.0), 4);
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
