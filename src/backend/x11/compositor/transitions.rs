use super::Compositor;
use super::CompositorConnection;
use super::compute_wallpaper_rect;
use super::math::{mat4_mul, rotate_x_matrix, rotate_y_matrix, scale_matrix, translate_matrix};
use super::prism::{PrismCamera, PrismFace, PrismPass, build_prism_pieces, mirror_matrix};
use crate::backend::x11::compositor_common::transitions::normalized_transition_progress;
use glow::HasContext;

/// A tag switch is always a quarter turn between two neighbouring faces.
const CUBE_SIDES: usize = 4;

#[derive(Clone, Copy, Debug)]
struct TransitionWorkspace {
    mon_x: i32,
    mon_w: u32,
    workspace_h: u32,
    scissor_gl_y: i32,
    aspect: f32,
    draw_rect: [f32; 4],
    uv_rect: [f32; 4],
}

impl<C: CompositorConnection> Compositor<C> {
    /// Returns true if a tag-switch transition is in progress.
    pub(crate) fn transition_active(&self) -> bool {
        self.transition_start.is_some()
    }

    /// Compute eased transition progress (0.0 → 1.0).
    /// Returns None when there is no active transition or it has completed.
    pub(crate) fn transition_progress(&self, now: std::time::Instant) -> Option<f32> {
        let start = self.transition_start?;
        let progress = normalized_transition_progress(start, now, self.transition_duration)?;
        Some(self.transition_mode.eased_progress(progress))
    }

    fn old_transition_texture(&self) -> Option<glow::Texture> {
        self.transition_fbo.as_ref().map(|(_, texture)| *texture)
    }

    fn transition_workspace(&self) -> Option<TransitionWorkspace> {
        let mon_w = self.transition_mon_w;
        let mon_h = self.transition_mon_h;
        if mon_w == 0 || mon_h == 0 {
            return None;
        }

        let exclude_top = self.transition_exclude_top.min(mon_h);
        let workspace_h = mon_h.checked_sub(exclude_top)?;
        if workspace_h == 0 || mon_w > i32::MAX as u32 || workspace_h > i32::MAX as u32 {
            return None;
        }

        let screen_h = i32::try_from(self.screen_h).ok()?;
        let monitor_bottom = i64::from(self.transition_mon_y) + i64::from(mon_h);
        let scissor_gl_y = i32::try_from(i64::from(screen_h) - monitor_bottom).ok()?;
        let aspect = mon_w as f32 / workspace_h as f32;
        if !aspect.is_finite() || aspect <= 0.0 {
            return None;
        }

        Some(TransitionWorkspace {
            mon_x: self.transition_mon_x,
            mon_w,
            workspace_h,
            scissor_gl_y,
            aspect,
            draw_rect: [
                self.transition_mon_x as f32,
                self.transition_mon_y as f32 + exclude_top as f32,
                mon_w as f32,
                workspace_h as f32,
            ],
            uv_rect: [0.0, 0.0, 1.0, workspace_h as f32 / mon_h as f32],
        })
    }

    fn begin_3d_transition(&self, workspace: TransitionWorkspace) {
        unsafe {
            self.gl.enable(glow::SCISSOR_TEST);
            self.gl.scissor(
                workspace.mon_x,
                workspace.scissor_gl_y,
                workspace.mon_w as i32,
                workspace.workspace_h as i32,
            );
            self.gl.viewport(
                workspace.mon_x,
                workspace.scissor_gl_y,
                workspace.mon_w as i32,
                workspace.workspace_h as i32,
            );
            self.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_vertex_array(Some(self.quad_vao));
        }
    }

    fn end_3d_transition(&self) {
        unsafe {
            self.gl.bind_texture(glow::TEXTURE_2D, None);
            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
            self.gl.disable(glow::SCISSOR_TEST);
            self.gl
                .viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
        }
    }

    fn begin_flat_transition(
        &self,
        projection: &[f32; 16],
        workspace: TransitionWorkspace,
        texture: glow::Texture,
    ) {
        unsafe {
            self.gl.enable(glow::SCISSOR_TEST);
            self.gl.scissor(
                workspace.mon_x,
                workspace.scissor_gl_y,
                workspace.mon_w as i32,
                workspace.workspace_h as i32,
            );
            self.gl
                .blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
            self.gl.use_program(Some(self.transition_program));
            self.gl.uniform_matrix_4_f32_slice(
                self.transition_uniforms.projection.as_ref(),
                false,
                projection,
            );
            self.gl
                .uniform_1_i32(self.transition_uniforms.texture.as_ref(), 0);
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            self.gl.bind_vertex_array(Some(self.quad_vao));
        }
    }

    fn draw_flat_transition_quad(&self, rect: [f32; 4], uv_rect: [f32; 4], opacity: f32) {
        unsafe {
            self.gl.uniform_4_f32(
                self.transition_uniforms.rect.as_ref(),
                rect[0],
                rect[1],
                rect[2],
                rect[3],
            );
            self.gl.uniform_1_f32(
                self.transition_uniforms.opacity.as_ref(),
                opacity.clamp(0.0, 1.0),
            );
            self.gl.uniform_4_f32(
                self.transition_uniforms.uv_rect.as_ref(),
                uv_rect[0],
                uv_rect[1],
                uv_rect[2],
                uv_rect[3],
            );
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        }
    }

    fn end_flat_transition(&self) {
        unsafe {
            self.gl.bind_texture(glow::TEXTURE_2D, None);
            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
            self.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
            self.gl.disable(glow::SCISSOR_TEST);
            self.gl
                .viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
        }
    }

    /// Re-draw the wallpaper in a specific monitor region using the given
    /// ortho projection. Used by transition paths that deliberately expose the
    /// compositor background instead of the already-rendered destination tag.
    pub(crate) fn draw_wallpaper_in_region(
        &self,
        proj: &[f32; 16],
        mon_x: i32,
        mon_y: i32,
        mon_w: u32,
        mon_h: u32,
    ) {
        let (tex, mode, iw, ih) = if let Some(mw) = self.monitor_wallpapers.iter().find(|mw| {
            mw.mon_x == mon_x && mw.mon_y == mon_y && mw.mon_w == mon_w && mw.mon_h == mon_h
        }) {
            if let Some(t) = mw.texture {
                (t, mw.mode, mw.img_w, mw.img_h)
            } else if let Some(t) = self.wallpaper_texture {
                (
                    t,
                    self.wallpaper_mode,
                    self.wallpaper_img_w,
                    self.wallpaper_img_h,
                )
            } else {
                return;
            }
        } else if let Some(t) = self.wallpaper_texture {
            (
                t,
                self.wallpaper_mode,
                self.wallpaper_img_w,
                self.wallpaper_img_h,
            )
        } else {
            return;
        };

        let area = (mon_x as f32, mon_y as f32, mon_w as f32, mon_h as f32);
        let (rx, ry, rw, rh) = compute_wallpaper_rect(mode, area, iw, ih);

        unsafe {
            self.gl.use_program(Some(self.program));
            self.gl
                .uniform_matrix_4_f32_slice(self.win_uniforms.projection.as_ref(), false, proj);
            self.gl.uniform_1_i32(self.win_uniforms.texture.as_ref(), 0);
            self.gl.bind_vertex_array(Some(self.quad_vao));
            self.gl
                .uniform_1_f32(self.win_uniforms.opacity.as_ref(), 1.0);
            self.gl
                .uniform_1_f32(self.win_uniforms.radius.as_ref(), 0.0);
            self.gl.uniform_1_f32(self.win_uniforms.dim.as_ref(), 1.0);
            self.gl
                .uniform_4_f32(self.win_uniforms.uv_rect.as_ref(), 0.0, 0.0, 1.0, 1.0);
            self.gl.active_texture(glow::TEXTURE0);

            self.gl
                .uniform_4_f32(self.win_uniforms.rect.as_ref(), rx, ry, rw, rh);
            self.gl
                .uniform_2_f32(self.win_uniforms.size.as_ref(), rw, rh);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

            self.gl.bind_texture(glow::TEXTURE_2D, None);
            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
        }
    }

    /// Build mipmaps for a transition snapshot.
    ///
    /// The 3D modes minify these hard — a card seen nearly edge-on samples one
    /// texel column in ten — so without a mipmap chain they alias into stripes.
    /// One chain per capture, not per frame.
    pub(super) fn build_transition_mipmaps(&self, texture: glow::Texture) {
        unsafe {
            self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            self.gl.generate_mipmap(glow::TEXTURE_2D);
            self.gl.bind_texture(glow::TEXTURE_2D, None);
        }
    }

    /// Grab the destination workspace once per transition.
    ///
    /// The compositor has already composited it into the back buffer by the
    /// time the transition overlay runs, so one blit per transition — not per
    /// frame — is enough to map it onto a cube face.
    fn transition_destination_texture(&mut self) -> Option<glow::Texture> {
        let (fbo, texture) = self.transition_new_fbo?;
        if !self.transition_new_ready {
            if !self.capture_transition_scene_from(
                None,
                fbo,
                self.transition_mon_x,
                self.transition_mon_y,
                self.transition_mon_w,
                self.transition_mon_h,
            ) {
                return None;
            }
            self.build_transition_mipmaps(texture);
            self.transition_new_ready = true;
        }
        Some(texture)
    }

    /// Turn both workspaces on a solid cube, Compiz style.
    ///
    /// The outgoing tag is one face and the incoming tag the neighbouring one,
    /// so the switch is a single rotation of one object rather than a card
    /// flipping over a backdrop. The camera starts square to the front face —
    /// which makes the first and last frame pixel-identical to the flat
    /// workspace — then pulls back, tips down and returns.
    pub(crate) fn render_cube_transition(&mut self, progress: f32, ortho_proj: &[f32; 16]) {
        let Some(old_texture) = self.old_transition_texture() else {
            return;
        };
        let Some(workspace) = self.transition_workspace() else {
            return;
        };
        let new_texture = self.transition_destination_texture();

        let t = progress.clamp(0.0, 1.0);
        // Symmetric envelope: zero at both ends, so the cube grows out of the
        // flat workspace and settles back into it.
        let pulse = (t * std::f32::consts::PI).sin();
        let direction = self.transition_direction;
        let time = self.compositor_start_time.elapsed().as_secs_f32();

        let camera = PrismCamera::frame(
            workspace.aspect,
            CUBE_SIDES,
            1.0 - 0.34 * pulse,
            0.30 * pulse,
            0.0,
        );
        let angle = direction * t * std::f32::consts::FRAC_PI_2;
        let lift = camera.lift_for_base_line(0.86) * pulse;
        let base_model = mat4_mul(&translate_matrix(0.0, lift, 0.0), &rotate_y_matrix(angle));
        let floor_y = lift - 1.0;
        let ground = camera
            .project(&translate_matrix(0.0, 0.0, 0.0), [0.0, floor_y, 0.0])
            .1;

        // The face that has to be square to the camera at the end is the one
        // the rotation brings to the front.
        let incoming = if direction > 0.0 { CUBE_SIDES - 1 } else { 1 };
        let mut faces = vec![PrismFace::default(); CUBE_SIDES];
        faces[0] = PrismFace {
            texture: Some(old_texture),
            uv_rect: workspace.uv_rect,
            edge: pulse,
            mipmapped: true,
            ..PrismFace::default()
        };
        faces[incoming] = PrismFace {
            texture: new_texture,
            uv_rect: workspace.uv_rect,
            edge: pulse,
            mipmapped: true,
            ..PrismFace::default()
        };
        let filler = Some(old_texture);

        // Skydome only during the rotation, so the flat workspace is untouched
        // at both ends of the animation. It is drawn flat, in screen space, so
        // it needs the workspace scissor but not the 3D viewport — and the
        // scissor has to be set explicitly: earlier passes leave their own
        // damage rectangle behind.
        unsafe {
            self.gl.enable(glow::SCISSOR_TEST);
            self.gl.scissor(
                workspace.mon_x,
                workspace.scissor_gl_y,
                workspace.mon_w as i32,
                workspace.workspace_h as i32,
            );
            self.gl
                .viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
            self.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
        }
        self.draw_prism_skydome(
            ortho_proj,
            workspace.draw_rect,
            &camera,
            ground,
            angle,
            pulse,
            time,
        );

        self.begin_3d_transition(workspace);
        self.bind_prism_programs(&camera, time);
        let pass = |reflect| PrismPass {
            fade: 1.0,
            // A tag switch keeps its faces opaque: the two neighbouring
            // workspaces are the whole story, and the empty faces behind them
            // have nothing worth showing through.
            spin: 0.0,
            floor_y,
            reflect,
        };
        let mirrored = mat4_mul(&mirror_matrix(floor_y), &base_model);
        self.draw_prism_pass(
            &camera,
            &build_prism_pieces(&camera, &mirrored, CUBE_SIDES),
            &faces,
            filler,
            &pass(true),
        );
        self.draw_prism_pass(
            &camera,
            &build_prism_pieces(&camera, &base_model, CUBE_SIDES),
            &faces,
            filler,
            &pass(false),
        );
        self.end_3d_transition();
    }

    /// Flip the old workspace away like a card, lit by the same shader the
    /// cube uses so its edges catch the light as it turns.
    pub(crate) fn render_flip_transition(&mut self, progress: f32, _ortho_proj: &[f32; 16]) {
        let Some(texture) = self.old_transition_texture() else {
            return;
        };
        let Some(workspace) = self.transition_workspace() else {
            return;
        };

        let t = progress.clamp(0.0, 1.0);
        let direction = self.transition_direction;
        let pulse = (t * std::f32::consts::PI).sin();
        let time = self.compositor_start_time.elapsed().as_secs_f32();

        let camera = PrismCamera::card(workspace.aspect, 1.0 - 0.14 * pulse, 0.0);
        let angle = direction * t * std::f32::consts::FRAC_PI_2;
        let model = mat4_mul(
            &rotate_y_matrix(angle),
            &mat4_mul(
                &rotate_x_matrix(-0.08 * pulse),
                &scale_matrix(1.0 - 0.06 * pulse, 1.0 - 0.06 * pulse, 1.0),
            ),
        );
        let brightness = 0.30 + 0.70 * angle.cos().abs();

        self.begin_3d_transition(workspace);
        self.bind_prism_programs(&camera, time);
        self.draw_prism_face(
            &camera.mvp(&model),
            &model,
            &PrismFace {
                texture: Some(texture),
                uv_rect: workspace.uv_rect,
                edge: pulse,
                mipmapped: true,
                ..PrismFace::default()
            },
            texture,
            brightness,
            1.0,
            0.0,
            false,
        );
        self.end_3d_transition();
    }

    /// Collapse strips of the old workspace in a direction-aware wave.
    pub(crate) fn render_blinds_transition(&mut self, progress: f32, ortho_proj: &[f32; 16]) {
        let Some(texture) = self.old_transition_texture() else {
            return;
        };
        let Some(workspace) = self.transition_workspace() else {
            return;
        };

        const STRIP_COUNT: u32 = 8;
        const STAGGER: f32 = 0.32;
        let t = progress.clamp(0.0, 1.0);
        let reverse = self.transition_direction < 0.0;

        self.begin_flat_transition(ortho_proj, workspace, texture);
        for strip in 0..STRIP_COUNT {
            let wave_index = if reverse {
                STRIP_COUNT - 1 - strip
            } else {
                strip
            };
            let delay = wave_index as f32 / (STRIP_COUNT - 1) as f32 * STAGGER;
            let local = ((t - delay) / (1.0 - STAGGER)).clamp(0.0, 1.0);

            let left_px =
                (u64::from(strip) * u64::from(workspace.mon_w) / u64::from(STRIP_COUNT)) as u32;
            let right_px =
                (u64::from(strip + 1) * u64::from(workspace.mon_w) / u64::from(STRIP_COUNT)) as u32;
            let source_w = right_px.saturating_sub(left_px);
            if source_w == 0 {
                continue;
            }

            let width = source_w as f32 * (1.0 - local);
            if width < 0.5 {
                continue;
            }
            let source_x = workspace.mon_x as f32 + left_px as f32;
            let rect = [
                source_x + (source_w as f32 - width) * 0.5,
                workspace.draw_rect[1],
                width,
                workspace.draw_rect[3],
            ];
            let uv_rect = [
                left_px as f32 / workspace.mon_w as f32,
                workspace.uv_rect[1],
                source_w as f32 / workspace.mon_w as f32,
                workspace.uv_rect[3],
            ];

            unsafe {
                self.gl.scissor(
                    workspace.mon_x + left_px as i32,
                    workspace.scissor_gl_y,
                    source_w as i32,
                    workspace.workspace_h as i32,
                );
            }
            self.draw_flat_transition_quad(rect, uv_rect, 1.0 - local * 0.12);
        }
        self.end_flat_transition();
    }

    /// Move the old workspace into a tilted side-card position, exposing the
    /// new workspace with a CoverFlow-like depth cue — reflection included, the
    /// way the album art it is named after always had one.
    pub(crate) fn render_coverflow_transition(&mut self, progress: f32, _ortho_proj: &[f32; 16]) {
        let Some(texture) = self.old_transition_texture() else {
            return;
        };
        let Some(workspace) = self.transition_workspace() else {
            return;
        };

        let t = progress.clamp(0.0, 1.0);
        let direction = self.transition_direction;
        let time = self.compositor_start_time.elapsed().as_secs_f32();

        let camera = PrismCamera::card(workspace.aspect, 1.0, 0.10 * t);
        let angle = direction * t * 72.0f32.to_radians();
        let scale = 1.0 - 0.2 * t;
        let placement = mat4_mul(
            &translate_matrix(
                -direction * t * workspace.aspect * 0.92,
                -0.06 * (t * std::f32::consts::PI).sin(),
                -0.48 * t,
            ),
            &mat4_mul(&rotate_y_matrix(angle), &scale_matrix(scale, scale, 1.0)),
        );
        let brightness = (1.0 - 0.48 * t).max(0.34);
        let face = PrismFace {
            texture: Some(texture),
            uv_rect: workspace.uv_rect,
            mipmapped: true,
            ..PrismFace::default()
        };

        self.begin_3d_transition(workspace);
        self.bind_prism_programs(&camera, time);
        // The card slides across a floor at its own bottom edge.
        let floor_y = -scale;
        let mirrored = mat4_mul(&mirror_matrix(floor_y), &placement);
        self.draw_prism_face(
            &camera.mvp(&mirrored),
            &mirrored,
            &face,
            texture,
            brightness,
            1.0,
            0.25,
            true,
        );
        self.draw_prism_face(
            &camera.mvp(&placement),
            &placement,
            &face,
            texture,
            brightness,
            1.0,
            0.25 * t,
            false,
        );
        self.end_3d_transition();
    }

    /// Spiral the old workspace away, tumbling out of frame with the lit-card
    /// shading the other 3D transitions use.
    pub(crate) fn render_helix_transition(&mut self, progress: f32, _ortho_proj: &[f32; 16]) {
        let Some(texture) = self.old_transition_texture() else {
            return;
        };
        let Some(workspace) = self.transition_workspace() else {
            return;
        };

        let t = progress.clamp(0.0, 1.0);
        let direction = self.transition_direction;
        let time = self.compositor_start_time.elapsed().as_secs_f32();

        let camera = PrismCamera::card(workspace.aspect, 1.0, 0.14 * t);
        let theta = direction * t * std::f32::consts::PI * 1.25;
        let radius = workspace.aspect * 0.58;
        let scale = (1.0 - 0.44 * t).max(0.5);
        let model = mat4_mul(
            &translate_matrix(
                radius * theta.sin(),
                -0.34 * t + 0.08 * (t * std::f32::consts::PI * 2.0).sin(),
                -radius * (1.0 - theta.cos().abs()) - 0.22 * t,
            ),
            &mat4_mul(
                &rotate_y_matrix(theta),
                &mat4_mul(
                    &rotate_x_matrix(-0.18 * t),
                    &scale_matrix(scale, scale, 1.0),
                ),
            ),
        );
        let brightness = (1.0 - 0.58 * t).max(0.26);

        self.begin_3d_transition(workspace);
        self.bind_prism_programs(&camera, time);
        self.draw_prism_face(
            &camera.mvp(&model),
            &model,
            &PrismFace {
                texture: Some(texture),
                uv_rect: workspace.uv_rect,
                mipmapped: true,
                ..PrismFace::default()
            },
            texture,
            brightness,
            // Fade the card out as it tumbles away instead of leaving it to
            // pop off at the end of the animation.
            (1.0 - t * 0.55).clamp(0.0, 1.0),
            0.4 * t,
            false,
        );
        self.end_3d_transition();
    }

    /// Reveal the destination through an expanding iris with a short glow
    /// pulse. Only the old snapshot is sampled; the live destination stays as
    /// the base layer already produced by the compositor.
    pub(crate) fn render_portal_transition(&mut self, progress: f32, ortho_proj: &[f32; 16]) {
        let Some(texture) = self.old_transition_texture() else {
            return;
        };
        let Some(workspace) = self.transition_workspace() else {
            return;
        };

        let t = progress.clamp(0.0, 1.0);
        let glow = (t * std::f32::consts::PI).sin().max(0.0) * 1.65;
        let center_x = 0.5 + 0.07 * self.transition_direction;
        let center_y = 0.5 - 0.035 * (t * std::f32::consts::PI).sin();

        unsafe {
            self.gl.enable(glow::SCISSOR_TEST);
            self.gl.scissor(
                workspace.mon_x,
                workspace.scissor_gl_y,
                workspace.mon_w as i32,
                workspace.workspace_h as i32,
            );
            self.gl
                .blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
            self.gl.use_program(Some(self.portal_program));
            self.gl.uniform_matrix_4_f32_slice(
                self.portal_uniforms.projection.as_ref(),
                false,
                ortho_proj,
            );
            self.gl.uniform_4_f32(
                self.portal_uniforms.rect.as_ref(),
                workspace.draw_rect[0],
                workspace.draw_rect[1],
                workspace.draw_rect[2],
                workspace.draw_rect[3],
            );
            self.gl
                .uniform_1_i32(self.portal_uniforms.texture.as_ref(), 0);
            self.gl
                .uniform_1_f32(self.portal_uniforms.progress.as_ref(), t);
            self.gl
                .uniform_1_f32(self.portal_uniforms.glow.as_ref(), glow);
            self.gl
                .uniform_2_f32(self.portal_uniforms.center.as_ref(), center_x, center_y);
            self.gl.uniform_4_f32(
                self.portal_uniforms.uv_rect.as_ref(),
                workspace.uv_rect[0],
                workspace.uv_rect[1],
                workspace.uv_rect[2],
                workspace.uv_rect[3],
            );
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            self.gl.bind_vertex_array(Some(self.quad_vao));
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

            self.gl.bind_texture(glow::TEXTURE_2D, None);
            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
            self.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
            self.gl.disable(glow::SCISSOR_TEST);
            self.gl
                .viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
        }
    }
}
