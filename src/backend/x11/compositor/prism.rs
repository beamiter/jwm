//! Shared 3D prism: geometry, framing and drawing.
//!
//! Two effects stand on this — the Alt+Ctrl+Tab overview carousel and the
//! `cube` tag-switch transition. Both want the same Compiz-style object: lit
//! faces with beveled edges, polygon caps, a mirrored copy fading into a floor,
//! and a see-through body while it spins. Keeping one implementation is what
//! makes them look like the same cube rather than two similar ones.

use super::Compositor;
use super::CompositorConnection;
pub(super) use crate::backend::compositor_common::prism::{
    MAX_PRISM_SIDES, MIN_PRISM_SIDES, PrismCamera, PrismKind, PrismPiece, build_prism_pieces,
    mat4_apply, mirror_matrix,
};
use glow::HasContext;

/// Accent shared by the skydome glow, the face bevels and the caps, so the
/// whole overlay reads as one lighting environment.
pub(super) const PRISM_ACCENT: [f32; 3] = [0.32, 0.62, 1.0];

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
