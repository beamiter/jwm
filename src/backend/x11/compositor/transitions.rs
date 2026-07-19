use super::Compositor;
use super::CompositorConnection;
use super::compute_wallpaper_rect;
use super::math::{
    mat4_mul, perspective_matrix, rotate_x_matrix, rotate_y_matrix, scale_matrix, translate_matrix,
};
use crate::backend::x11::compositor_common::transitions::normalized_transition_progress;
use glow::HasContext;

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
            self.gl.use_program(Some(self.cube_program));
            self.gl
                .uniform_1_f32(self.cube_uniforms.aspect.as_ref(), workspace.aspect);
            self.gl
                .uniform_1_i32(self.cube_uniforms.texture.as_ref(), 0);
            self.gl.uniform_4_f32(
                self.cube_uniforms.uv_rect.as_ref(),
                workspace.uv_rect[0],
                workspace.uv_rect[1],
                workspace.uv_rect[2],
                workspace.uv_rect[3],
            );
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_vertex_array(Some(self.quad_vao));
        }
    }

    fn draw_3d_transition_face(&self, texture: glow::Texture, mvp: &[f32; 16], brightness: f32) {
        unsafe {
            self.gl
                .uniform_matrix_4_f32_slice(self.cube_uniforms.mvp.as_ref(), false, mvp);
            self.gl.uniform_1_f32(
                self.cube_uniforms.brightness.as_ref(),
                brightness.clamp(0.0, 1.0),
            );
            self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
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

    /// Rotate the old workspace away as one face of a cube. The destination
    /// workspace is already present underneath, avoiding a per-frame FBO copy.
    pub(crate) fn render_cube_transition(&mut self, progress: f32, _ortho_proj: &[f32; 16]) {
        let Some(texture) = self.old_transition_texture() else {
            return;
        };
        let Some(workspace) = self.transition_workspace() else {
            return;
        };

        let t = progress.clamp(0.0, 1.0);
        let half_pi = std::f32::consts::FRAC_PI_2;
        let direction = self.transition_direction;
        let depth = workspace.aspect;
        let zoom = 1.0 + 0.22 * (t * std::f32::consts::PI).sin();
        let camera_z = (1.0 + depth) * zoom;
        let perspective = perspective_matrix(half_pi, workspace.aspect, 0.1, camera_z * 4.0);
        let view = translate_matrix(0.0, 0.0, -camera_z);
        let angle = direction * t * half_pi;
        let tilt = rotate_x_matrix(-0.055 * (t * std::f32::consts::PI).sin());
        let model = mat4_mul(
            &rotate_y_matrix(angle),
            &mat4_mul(&tilt, &translate_matrix(0.0, 0.0, depth)),
        );
        let mvp = mat4_mul(&perspective, &mat4_mul(&view, &model));
        let brightness = 0.22 + 0.78 * angle.cos().abs();

        self.begin_3d_transition(workspace);
        self.draw_3d_transition_face(texture, &mvp, brightness);
        self.end_3d_transition();
    }

    /// Flip the old workspace away like a card. A small midpoint zoom and tilt
    /// keep the silhouette legible while the destination is revealed beneath.
    pub(crate) fn render_flip_transition(&mut self, progress: f32, _ortho_proj: &[f32; 16]) {
        let Some(texture) = self.old_transition_texture() else {
            return;
        };
        let Some(workspace) = self.transition_workspace() else {
            return;
        };

        let t = progress.clamp(0.0, 1.0);
        let half_pi = std::f32::consts::FRAC_PI_2;
        let direction = self.transition_direction;
        let pulse = (t * std::f32::consts::PI).sin();
        let camera_z = 1.0 + 0.16 * pulse;
        let perspective = perspective_matrix(half_pi, workspace.aspect, 0.1, 6.0);
        let view = translate_matrix(0.0, 0.0, -camera_z);
        let angle = direction * t * half_pi;
        let model = mat4_mul(
            &rotate_y_matrix(angle),
            &mat4_mul(
                &rotate_x_matrix(-0.08 * pulse),
                &scale_matrix(1.0 - 0.06 * pulse, 1.0 - 0.06 * pulse, 1.0),
            ),
        );
        let mvp = mat4_mul(&perspective, &mat4_mul(&view, &model));
        let brightness = 0.18 + 0.82 * angle.cos().abs();

        self.begin_3d_transition(workspace);
        self.draw_3d_transition_face(texture, &mvp, brightness);
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
    /// new workspace with a CoverFlow-like depth cue.
    pub(crate) fn render_coverflow_transition(&mut self, progress: f32, _ortho_proj: &[f32; 16]) {
        let Some(texture) = self.old_transition_texture() else {
            return;
        };
        let Some(workspace) = self.transition_workspace() else {
            return;
        };

        let t = progress.clamp(0.0, 1.0);
        let direction = self.transition_direction;
        let angle = direction * t * 72.0f32.to_radians();
        let scale = 1.0 - 0.2 * t;
        let x = -direction * t * workspace.aspect * 0.92;
        let y = -0.06 * (t * std::f32::consts::PI).sin();
        let z = -0.48 * t;
        let perspective =
            perspective_matrix(std::f32::consts::FRAC_PI_2, workspace.aspect, 0.1, 8.0);
        let view = translate_matrix(0.0, 0.0, -1.0);
        let model = mat4_mul(
            &translate_matrix(x, y, z),
            &mat4_mul(&rotate_y_matrix(angle), &scale_matrix(scale, scale, 1.0)),
        );
        let mvp = mat4_mul(&perspective, &mat4_mul(&view, &model));
        let brightness = (1.0 - 0.58 * t).max(0.28);

        self.begin_3d_transition(workspace);
        self.draw_3d_transition_face(texture, &mvp, brightness);
        self.end_3d_transition();
    }

    /// Spiral the old workspace away while applying the scale that the former
    /// implementation calculated but never used.
    pub(crate) fn render_helix_transition(&mut self, progress: f32, _ortho_proj: &[f32; 16]) {
        let Some(texture) = self.old_transition_texture() else {
            return;
        };
        let Some(workspace) = self.transition_workspace() else {
            return;
        };

        let t = progress.clamp(0.0, 1.0);
        let direction = self.transition_direction;
        let theta = direction * t * std::f32::consts::PI * 1.25;
        let radius = workspace.aspect * 0.58;
        let x = radius * theta.sin();
        let y = -0.34 * t + 0.08 * (t * std::f32::consts::PI * 2.0).sin();
        let z = -radius * (1.0 - theta.cos().abs()) - 0.22 * t;
        let scale = (1.0 - 0.44 * t).max(0.5);
        let perspective =
            perspective_matrix(std::f32::consts::FRAC_PI_2, workspace.aspect, 0.1, 10.0);
        let view = translate_matrix(0.0, 0.0, -1.0);
        let model = mat4_mul(
            &translate_matrix(x, y, z),
            &mat4_mul(
                &rotate_y_matrix(theta),
                &mat4_mul(
                    &rotate_x_matrix(-0.18 * t),
                    &scale_matrix(scale, scale, 1.0),
                ),
            ),
        );
        let mvp = mat4_mul(&perspective, &mat4_mul(&view, &model));
        let brightness = (1.0 - 0.68 * t).max(0.2);

        self.begin_3d_transition(workspace);
        self.draw_3d_transition_face(texture, &mvp, brightness);
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
