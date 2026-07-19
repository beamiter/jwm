use super::*;
use smithay::backend::renderer::gles::ffi;

/// Geometry shared by every workspace-transition mode.
///
/// Draw coordinates use the compositor's top-left origin, while `scissor`
/// and `viewport` use OpenGL's bottom-left origin. `uv_rect` addresses the
/// matching workspace pixels in the full-screen transition snapshot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct TransitionLayout {
    draw_rect: [f32; 4],
    uv_rect: [f32; 4],
    scissor: [i32; 4],
}

pub(super) fn transition_layout(
    screen_w: u32,
    screen_h: u32,
    mon_rect: (i32, i32, u32, u32),
    exclude_top: u32,
) -> Option<TransitionLayout> {
    if screen_w == 0
        || screen_h == 0
        || screen_w > i32::MAX as u32
        || screen_h > i32::MAX as u32
        || mon_rect.2 == 0
        || mon_rect.3 == 0
    {
        return None;
    }

    let (mon_x, mon_y, mon_w, mon_h) = mon_rect;
    let workspace_top = i64::from(mon_y) + i64::from(exclude_top.min(mon_h));
    let workspace_bottom = i64::from(mon_y) + i64::from(mon_h);
    let workspace_left = i64::from(mon_x);
    let workspace_right = i64::from(mon_x) + i64::from(mon_w);

    // A malformed or completely off-screen monitor must never produce a
    // negative/overflowing GL scissor rectangle.
    let x0 = workspace_left.clamp(0, i64::from(screen_w));
    let x1 = workspace_right.clamp(0, i64::from(screen_w));
    let y0 = workspace_top.clamp(0, i64::from(screen_h));
    let y1 = workspace_bottom.clamp(0, i64::from(screen_h));
    if x1 <= x0 || y1 <= y0 {
        return None;
    }

    let x = x0 as i32;
    let y = y0 as i32;
    let width = (x1 - x0) as i32;
    let height = (y1 - y0) as i32;
    let gl_y = screen_h as i32 - (y + height);
    let inv_w = 1.0 / screen_w as f32;
    let inv_h = 1.0 / screen_h as f32;

    Some(TransitionLayout {
        draw_rect: [x as f32, y as f32, width as f32, height as f32],
        uv_rect: [
            x as f32 * inv_w,
            gl_y as f32 * inv_h,
            width as f32 * inv_w,
            height as f32 * inv_h,
        ],
        scissor: [x, gl_y, width, height],
    })
}

fn blinds_strip_layout(
    layout: TransitionLayout,
    strip: u32,
    strip_count: u32,
) -> Option<(f32, f32, [f32; 4], [i32; 4])> {
    if strip_count == 0 || strip >= strip_count {
        return None;
    }
    let f0 = strip as f32 / strip_count as f32;
    let f1 = (strip + 1) as f32 / strip_count as f32;
    let x0 = layout.draw_rect[0] + layout.draw_rect[2] * f0;
    let x1 = layout.draw_rect[0] + layout.draw_rect[2] * f1;
    let outer_right = layout.scissor[0] + layout.scissor[2];
    let scissor_x0 = (x0.floor() as i32).max(layout.scissor[0]);
    let scissor_x1 = (x1.ceil() as i32).min(outer_right);
    if scissor_x1 <= scissor_x0 {
        return None;
    }
    Some((
        x0,
        x1 - x0,
        [
            layout.uv_rect[0] + layout.uv_rect[2] * f0,
            layout.uv_rect[1],
            layout.uv_rect[2] * (f1 - f0),
            layout.uv_rect[3],
        ],
        [
            scissor_x0,
            layout.scissor[1],
            scissor_x1 - scissor_x0,
            layout.scissor[3],
        ],
    ))
}

impl WaylandCompositor {
    /// Render the workspace transition overlay.
    /// Called from render_frame when transition_active is true.
    pub(crate) fn render_transition(&mut self, gl: &ffi::Gles2, projection: &[f32; 16]) {
        let start = match self.transition_start {
            Some(s) => s,
            None => {
                self.transition_active = false;
                return;
            }
        };

        let elapsed = start.elapsed();
        let duration = self.transition_duration.as_secs_f32();
        if !duration.is_finite() || duration <= f32::EPSILON {
            self.transition_active = false;
            self.transition_start = None;
            return;
        }
        let progress = (elapsed.as_secs_f32() / duration).min(1.0);

        // Ease-out cubic
        let t = 1.0 - (1.0 - progress).powi(3);

        if progress >= 1.0 {
            self.transition_active = false;
            self.transition_start = None;
            self.transition_mon = None;
            return;
        }

        let Some(mon_rect) = self.transition_mon else {
            self.transition_active = false;
            self.transition_start = None;
            return;
        };
        let Some(layout) = transition_layout(
            self.screen_w,
            self.screen_h,
            mon_rect,
            self.transition_exclude_top,
        ) else {
            self.transition_active = false;
            self.transition_start = None;
            self.transition_mon = None;
            return;
        };

        unsafe {
            gl.Enable(ffi::SCISSOR_TEST);
            gl.Scissor(
                layout.scissor[0],
                layout.scissor[1],
                layout.scissor[2],
                layout.scissor[3],
            );
        }

        match self.transition_mode {
            TransitionMode::Slide => self.render_slide_transition(gl, projection, layout, t),
            TransitionMode::Cube => self.render_cube_transition(gl, layout, t),
            TransitionMode::Flip => self.render_flip_transition(gl, projection, layout, t),
            TransitionMode::Fade => self.render_fade_transition(gl, projection, layout, t),
            TransitionMode::Zoom => self.render_zoom_transition(gl, projection, layout, t),
            TransitionMode::Portal => self.render_portal_transition(gl, projection, layout, t),
            TransitionMode::Stack => self.render_stack_transition(gl, projection, layout, t),
            TransitionMode::Blinds => self.render_blinds_transition(gl, projection, layout, t),
            TransitionMode::CoverFlow => self.render_coverflow_transition(gl, layout, t),
            TransitionMode::Helix => self.render_helix_transition(gl, layout, t),
            TransitionMode::None => {}
        }

        // 3D modes temporarily render into the selected monitor viewport, and
        // blinds replaces the outer workspace scissor with per-strip scissors.
        // Return the shared GL state to the full-output contract expected by
        // all overlays rendered after workspace transitions.
        unsafe {
            gl.BindVertexArray(0);
            gl.BindTexture(ffi::TEXTURE_2D, 0);
            gl.UseProgram(0);
            gl.Disable(ffi::SCISSOR_TEST);
            gl.Viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
        }

        self.needs_render = true;
    }

    fn render_slide_transition(
        &self,
        gl: &ffi::Gles2,
        projection: &[f32; 16],
        layout: TransitionLayout,
        t: f32,
    ) {
        unsafe {
            gl.UseProgram(self.transition_program);
            gl.UniformMatrix4fv(
                self.transition_uniforms.projection,
                1,
                ffi::FALSE as u8,
                projection.as_ptr(),
            );
            gl.Uniform4f(
                self.transition_uniforms.uv_rect,
                layout.uv_rect[0],
                layout.uv_rect[1],
                layout.uv_rect[2],
                layout.uv_rect[3],
            );

            // Old scene slides out
            let dir = self.transition_direction as f32;
            let offset_x = -dir * t * layout.draw_rect[2];
            gl.Uniform4f(
                self.transition_uniforms.rect,
                layout.draw_rect[0] + offset_x,
                layout.draw_rect[1],
                layout.draw_rect[2],
                layout.draw_rect[3],
            );
            gl.Uniform1f(self.transition_uniforms.opacity, 1.0 - t);

            gl.ActiveTexture(ffi::TEXTURE0);
            gl.BindTexture(ffi::TEXTURE_2D, self.transition_texture);
            gl.Uniform1i(
                gl.GetUniformLocation(self.transition_program, b"u_texture\0".as_ptr() as *const _),
                0,
            );

            gl.BindVertexArray(self.quad_vao);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
        }
    }

    fn render_cube_transition(&self, gl: &ffi::Gles2, layout: TransitionLayout, t: f32) {
        unsafe {
            gl.Viewport(
                layout.scissor[0],
                layout.scissor[1],
                layout.scissor[2],
                layout.scissor[3],
            );
            gl.UseProgram(self.cube_program);

            let dir = self.transition_direction as f32;
            let angle = dir * t * std::f32::consts::FRAC_PI_2; // 0 to 90 degrees

            let aspect = layout.draw_rect[2] / layout.draw_rect[3];
            gl.Uniform1f(self.cube_uniforms.aspect, aspect);

            // Perspective matrix
            let fov = 1.0f32; // ~57 degrees
            let near = 0.1f32;
            let far = 100.0f32;
            let f = 1.0 / fov.tan();
            let mut persp = [0.0f32; 16];
            persp[0] = f / aspect;
            persp[5] = f;
            persp[10] = (far + near) / (near - far);
            persp[11] = -1.0;
            persp[14] = (2.0 * far * near) / (near - far);

            // Rotation matrix (around Y axis)
            let cos_a = angle.cos();
            let sin_a = angle.sin();
            let mut rot = [0.0f32; 16];
            rot[0] = cos_a;
            rot[2] = sin_a;
            rot[5] = 1.0;
            rot[8] = -sin_a;
            rot[10] = cos_a;
            rot[15] = 1.0;

            // Translation (push back)
            let mut trans = [0.0f32; 16];
            trans[0] = 1.0;
            trans[5] = 1.0;
            trans[10] = 1.0;
            trans[15] = 1.0;
            trans[14] = -2.5; // z offset

            // MVP = persp * trans * rot
            let view = mat4_mul(&trans, &rot);
            let mvp = mat4_mul(&persp, &view);

            gl.UniformMatrix4fv(self.cube_uniforms.mvp, 1, ffi::FALSE as u8, mvp.as_ptr());
            gl.Uniform4f(
                self.cube_uniforms.uv_rect,
                layout.uv_rect[0],
                layout.uv_rect[1],
                layout.uv_rect[2],
                layout.uv_rect[3],
            );

            // Old face (brighter when facing camera)
            let brightness = cos_a.max(0.3);
            gl.Uniform1f(self.cube_uniforms.brightness, brightness);

            gl.ActiveTexture(ffi::TEXTURE0);
            gl.BindTexture(ffi::TEXTURE_2D, self.transition_texture);
            gl.Uniform1i(
                gl.GetUniformLocation(self.cube_program, b"u_texture\0".as_ptr() as *const _),
                0,
            );

            gl.BindVertexArray(self.quad_vao);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
        }
    }

    fn render_flip_transition(
        &self,
        gl: &ffi::Gles2,
        projection: &[f32; 16],
        layout: TransitionLayout,
        t: f32,
    ) {
        // Card flip: scale X from 1 to 0 (first half) then 0 to 1 (second half showing new)
        // We only render the old scene (first half of flip)
        unsafe {
            if t < 0.5 {
                let scale_x = 1.0 - t * 2.0;
                let center_x = layout.draw_rect[0] + layout.draw_rect[2] * 0.5;
                let w = layout.draw_rect[2] * scale_x;
                let x = center_x - w * 0.5;

                gl.UseProgram(self.transition_program);
                gl.UniformMatrix4fv(
                    self.transition_uniforms.projection,
                    1,
                    ffi::FALSE as u8,
                    projection.as_ptr(),
                );
                gl.Uniform4f(
                    self.transition_uniforms.rect,
                    x,
                    layout.draw_rect[1],
                    w,
                    layout.draw_rect[3],
                );
                gl.Uniform4f(
                    self.transition_uniforms.uv_rect,
                    layout.uv_rect[0],
                    layout.uv_rect[1],
                    layout.uv_rect[2],
                    layout.uv_rect[3],
                );
                gl.Uniform1f(self.transition_uniforms.opacity, 1.0);

                gl.ActiveTexture(ffi::TEXTURE0);
                gl.BindTexture(ffi::TEXTURE_2D, self.transition_texture);
                gl.Uniform1i(
                    gl.GetUniformLocation(
                        self.transition_program,
                        b"u_texture\0".as_ptr() as *const _,
                    ),
                    0,
                );

                gl.BindVertexArray(self.quad_vao);
                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
            }
        }
    }

    fn render_fade_transition(
        &self,
        gl: &ffi::Gles2,
        projection: &[f32; 16],
        layout: TransitionLayout,
        t: f32,
    ) {
        unsafe {
            gl.UseProgram(self.transition_program);
            gl.UniformMatrix4fv(
                self.transition_uniforms.projection,
                1,
                ffi::FALSE as u8,
                projection.as_ptr(),
            );
            gl.Uniform4f(
                self.transition_uniforms.rect,
                layout.draw_rect[0],
                layout.draw_rect[1],
                layout.draw_rect[2],
                layout.draw_rect[3],
            );
            gl.Uniform4f(
                self.transition_uniforms.uv_rect,
                layout.uv_rect[0],
                layout.uv_rect[1],
                layout.uv_rect[2],
                layout.uv_rect[3],
            );
            gl.Uniform1f(self.transition_uniforms.opacity, 1.0 - t);

            gl.ActiveTexture(ffi::TEXTURE0);
            gl.BindTexture(ffi::TEXTURE_2D, self.transition_texture);
            gl.Uniform1i(
                gl.GetUniformLocation(self.transition_program, b"u_texture\0".as_ptr() as *const _),
                0,
            );

            gl.BindVertexArray(self.quad_vao);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
        }
    }

    fn render_zoom_transition(
        &self,
        gl: &ffi::Gles2,
        projection: &[f32; 16],
        layout: TransitionLayout,
        t: f32,
    ) {
        unsafe {
            let scale = 1.0 + t * 0.3; // zoom out slightly
            let w = layout.draw_rect[2] * scale;
            let h = layout.draw_rect[3] * scale;
            let x = layout.draw_rect[0] + (layout.draw_rect[2] - w) * 0.5;
            let y = layout.draw_rect[1] + (layout.draw_rect[3] - h) * 0.5;

            gl.UseProgram(self.transition_program);
            gl.UniformMatrix4fv(
                self.transition_uniforms.projection,
                1,
                ffi::FALSE as u8,
                projection.as_ptr(),
            );
            gl.Uniform4f(self.transition_uniforms.rect, x, y, w, h);
            gl.Uniform4f(
                self.transition_uniforms.uv_rect,
                layout.uv_rect[0],
                layout.uv_rect[1],
                layout.uv_rect[2],
                layout.uv_rect[3],
            );
            gl.Uniform1f(self.transition_uniforms.opacity, 1.0 - t);

            gl.ActiveTexture(ffi::TEXTURE0);
            gl.BindTexture(ffi::TEXTURE_2D, self.transition_texture);
            gl.Uniform1i(
                gl.GetUniformLocation(self.transition_program, b"u_texture\0".as_ptr() as *const _),
                0,
            );

            gl.BindVertexArray(self.quad_vao);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
        }
    }

    fn render_portal_transition(
        &self,
        gl: &ffi::Gles2,
        projection: &[f32; 16],
        layout: TransitionLayout,
        t: f32,
    ) {
        unsafe {
            gl.UseProgram(self.portal_program);
            gl.UniformMatrix4fv(
                self.portal_uniforms.projection,
                1,
                ffi::FALSE as u8,
                projection.as_ptr(),
            );
            gl.Uniform4f(
                self.portal_uniforms.rect,
                layout.draw_rect[0],
                layout.draw_rect[1],
                layout.draw_rect[2],
                layout.draw_rect[3],
            );
            gl.Uniform4f(
                self.portal_uniforms.uv_rect,
                layout.uv_rect[0],
                layout.uv_rect[1],
                layout.uv_rect[2],
                layout.uv_rect[3],
            );
            gl.Uniform1f(self.portal_uniforms.progress, t);
            gl.Uniform1f(self.portal_uniforms.glow, 1.0 - t);
            gl.Uniform2f(self.portal_uniforms.center, 0.5, 0.5);

            gl.ActiveTexture(ffi::TEXTURE0);
            gl.BindTexture(ffi::TEXTURE_2D, self.transition_texture);
            gl.Uniform1i(
                gl.GetUniformLocation(self.portal_program, b"u_texture\0".as_ptr() as *const _),
                0,
            );

            gl.BindVertexArray(self.quad_vao);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
        }
    }

    fn render_stack_transition(
        &self,
        gl: &ffi::Gles2,
        projection: &[f32; 16],
        layout: TransitionLayout,
        t: f32,
    ) {
        unsafe {
            let dir = self.transition_direction as f32;
            let scale = 1.0 - t * 0.15; // scale from 1.0 to 0.85
            let opacity = 1.0 - t;

            let w = layout.draw_rect[2] * scale;
            let h = layout.draw_rect[3] * scale;
            let x = layout.draw_rect[0]
                + (layout.draw_rect[2] - w) * 0.5
                + dir * t * layout.draw_rect[2] * 0.05;
            let y = layout.draw_rect[1] + (layout.draw_rect[3] - h) * 0.5;

            gl.UseProgram(self.transition_program);
            gl.UniformMatrix4fv(
                self.transition_uniforms.projection,
                1,
                ffi::FALSE as u8,
                projection.as_ptr(),
            );
            gl.Uniform4f(self.transition_uniforms.rect, x, y, w, h);
            gl.Uniform4f(
                self.transition_uniforms.uv_rect,
                layout.uv_rect[0],
                layout.uv_rect[1],
                layout.uv_rect[2],
                layout.uv_rect[3],
            );
            gl.Uniform1f(self.transition_uniforms.opacity, opacity);

            gl.ActiveTexture(ffi::TEXTURE0);
            gl.BindTexture(ffi::TEXTURE_2D, self.transition_texture);
            gl.Uniform1i(
                gl.GetUniformLocation(self.transition_program, b"u_texture\0".as_ptr() as *const _),
                0,
            );

            gl.BindVertexArray(self.quad_vao);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
        }
    }

    fn render_blinds_transition(
        &self,
        gl: &ffi::Gles2,
        projection: &[f32; 16],
        layout: TransitionLayout,
        t: f32,
    ) {
        unsafe {
            gl.UseProgram(self.transition_program);
            gl.UniformMatrix4fv(
                self.transition_uniforms.projection,
                1,
                ffi::FALSE as u8,
                projection.as_ptr(),
            );
            gl.Uniform1f(self.transition_uniforms.opacity, 1.0);

            gl.ActiveTexture(ffi::TEXTURE0);
            gl.BindTexture(ffi::TEXTURE_2D, self.transition_texture);
            gl.Uniform1i(
                gl.GetUniformLocation(self.transition_program, b"u_texture\0".as_ptr() as *const _),
                0,
            );

            let num_strips: u32 = 8;
            let stagger = 0.3f32;

            for i in 0..num_strips {
                let strip_delay = (i as f32 / (num_strips - 1).max(1) as f32) * stagger;
                let strip_progress = ((t - strip_delay) / (1.0 - stagger)).clamp(0.0, 1.0);

                if strip_progress >= 0.5 {
                    // Second half: new scene shows through, don't draw old
                    continue;
                }

                // First half: old scene strip squeezes horizontally toward center of strip
                let squeeze = strip_progress * 2.0; // 0.0 to 1.0
                let Some((strip_x, strip_w, strip_uv, strip_scissor)) =
                    blinds_strip_layout(layout, i, num_strips)
                else {
                    continue;
                };
                gl.Scissor(
                    strip_scissor[0],
                    strip_scissor[1],
                    strip_scissor[2],
                    strip_scissor[3],
                );

                let center_x = strip_x + strip_w * 0.5;
                let w = strip_w * (1.0 - squeeze);
                let x = center_x - w * 0.5;

                // Draw old scene with UV covering only this strip
                gl.Uniform4f(
                    self.transition_uniforms.uv_rect,
                    strip_uv[0],
                    strip_uv[1],
                    strip_uv[2],
                    strip_uv[3],
                );
                gl.Uniform4f(
                    self.transition_uniforms.rect,
                    x,
                    layout.draw_rect[1],
                    w,
                    layout.draw_rect[3],
                );

                gl.BindVertexArray(self.quad_vao);
                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
            }
        }
    }

    fn render_coverflow_transition(&self, gl: &ffi::Gles2, layout: TransitionLayout, t: f32) {
        unsafe {
            gl.Viewport(
                layout.scissor[0],
                layout.scissor[1],
                layout.scissor[2],
                layout.scissor[3],
            );
            gl.UseProgram(self.cube_program);

            let dir = self.transition_direction as f32;
            let max_rotation = 70.0_f32.to_radians();
            let angle = t * max_rotation * dir;

            let aspect = layout.draw_rect[2] / layout.draw_rect[3];
            gl.Uniform1f(self.cube_uniforms.aspect, aspect);

            // Perspective matrix
            let fov = 1.0f32;
            let near = 0.1f32;
            let far = 100.0f32;
            let f = 1.0 / fov.tan();
            let mut persp = [0.0f32; 16];
            persp[0] = f / aspect;
            persp[5] = f;
            persp[10] = (far + near) / (near - far);
            persp[11] = -1.0;
            persp[14] = (2.0 * far * near) / (near - far);

            // Translation (push back in Z)
            let mut trans = [0.0f32; 16];
            trans[0] = 1.0;
            trans[5] = 1.0;
            trans[10] = 1.0;
            trans[15] = 1.0;
            trans[14] = -2.5;

            // Rotation around Y axis
            let cos_a = angle.cos();
            let sin_a = angle.sin();
            let mut rot = [0.0f32; 16];
            rot[0] = cos_a;
            rot[2] = sin_a;
            rot[5] = 1.0;
            rot[8] = -sin_a;
            rot[10] = cos_a;
            rot[15] = 1.0;

            // Lateral offset translation
            let mut lateral = [0.0f32; 16];
            lateral[0] = 1.0;
            lateral[5] = 1.0;
            lateral[10] = 1.0;
            lateral[15] = 1.0;
            lateral[12] = t * 1.5 * dir;

            // MVP = persp * trans * rot * lateral
            let model = mat4_mul(&rot, &lateral);
            let view = mat4_mul(&trans, &model);
            let mvp = mat4_mul(&persp, &view);

            gl.UniformMatrix4fv(self.cube_uniforms.mvp, 1, ffi::FALSE as u8, mvp.as_ptr());
            gl.Uniform4f(
                self.cube_uniforms.uv_rect,
                layout.uv_rect[0],
                layout.uv_rect[1],
                layout.uv_rect[2],
                layout.uv_rect[3],
            );

            let brightness = (1.0 - t * 0.7).max(0.3);
            gl.Uniform1f(self.cube_uniforms.brightness, brightness);

            gl.ActiveTexture(ffi::TEXTURE0);
            gl.BindTexture(ffi::TEXTURE_2D, self.transition_texture);
            gl.Uniform1i(
                gl.GetUniformLocation(self.cube_program, b"u_texture\0".as_ptr() as *const _),
                0,
            );

            gl.BindVertexArray(self.quad_vao);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
        }
    }

    fn render_helix_transition(&self, gl: &ffi::Gles2, layout: TransitionLayout, t: f32) {
        unsafe {
            gl.Viewport(
                layout.scissor[0],
                layout.scissor[1],
                layout.scissor[2],
                layout.scissor[3],
            );
            gl.UseProgram(self.cube_program);

            let dir = self.transition_direction as f32;
            let theta = t * std::f32::consts::PI;
            let r = 0.8f32;

            let x = r * theta.sin() * dir;
            let z = -r * (1.0 - theta.cos());
            let s = 1.0 - t * 0.3;

            let aspect = layout.draw_rect[2] / layout.draw_rect[3];
            gl.Uniform1f(self.cube_uniforms.aspect, aspect);

            // Perspective matrix
            let fov = 1.0f32;
            let near = 0.1f32;
            let far = 100.0f32;
            let f = 1.0 / fov.tan();
            let mut persp = [0.0f32; 16];
            persp[0] = f / aspect;
            persp[5] = f;
            persp[10] = (far + near) / (near - far);
            persp[11] = -1.0;
            persp[14] = (2.0 * far * near) / (near - far);

            // Translation (push back + helix Z offset)
            let mut trans = [0.0f32; 16];
            trans[0] = 1.0;
            trans[5] = 1.0;
            trans[10] = 1.0;
            trans[15] = 1.0;
            trans[12] = x;
            trans[14] = -2.5 + z;

            // Rotation around Y axis (theta * direction)
            let rot_angle = theta * dir;
            let cos_a = rot_angle.cos();
            let sin_a = rot_angle.sin();
            let mut rot = [0.0f32; 16];
            rot[0] = cos_a;
            rot[2] = sin_a;
            rot[5] = 1.0;
            rot[8] = -sin_a;
            rot[10] = cos_a;
            rot[15] = 1.0;

            // Scale matrix
            let mut scale = [0.0f32; 16];
            scale[0] = s;
            scale[5] = s;
            scale[10] = 1.0;
            scale[15] = 1.0;

            // MVP = persp * trans * rot * scale
            let model = mat4_mul(&rot, &scale);
            let view = mat4_mul(&trans, &model);
            let mvp = mat4_mul(&persp, &view);

            gl.UniformMatrix4fv(self.cube_uniforms.mvp, 1, ffi::FALSE as u8, mvp.as_ptr());
            gl.Uniform4f(
                self.cube_uniforms.uv_rect,
                layout.uv_rect[0],
                layout.uv_rect[1],
                layout.uv_rect[2],
                layout.uv_rect[3],
            );

            let brightness = (1.0 - t * 0.6).max(0.3);
            gl.Uniform1f(self.cube_uniforms.brightness, brightness);

            gl.ActiveTexture(ffi::TEXTURE0);
            gl.BindTexture(ffi::TEXTURE_2D, self.transition_texture);
            gl.Uniform1i(
                gl.GetUniformLocation(self.cube_program, b"u_texture\0".as_ptr() as *const _),
                0,
            );

            gl.BindVertexArray(self.quad_vao);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
        }
    }

    /// Capture current frame to transition FBO (called before tag switch)
    #[allow(dead_code)]
    pub(crate) fn capture_transition_snapshot(&self, gl: &ffi::Gles2) {
        unsafe {
            gl.BindFramebuffer(ffi::READ_FRAMEBUFFER, self.output_fbo);
            gl.BindFramebuffer(ffi::DRAW_FRAMEBUFFER, self.transition_fbo);
            gl.BlitFramebuffer(
                0,
                0,
                self.screen_w as i32,
                self.screen_h as i32,
                0,
                0,
                self.screen_w as i32,
                self.screen_h as i32,
                ffi::COLOR_BUFFER_BIT,
                ffi::NEAREST,
            );
            gl.BindFramebuffer(ffi::FRAMEBUFFER, self.output_fbo);
        }
    }
}

/// 4x4 matrix multiplication
fn mat4_mul(a: &[f32; 16], b: &[f32; 16]) -> [f32; 16] {
    let mut r = [0.0f32; 16];
    for i in 0..4 {
        for j in 0..4 {
            r[i * 4 + j] = a[i * 4 + 0] * b[0 * 4 + j]
                + a[i * 4 + 1] * b[1 * 4 + j]
                + a[i * 4 + 2] * b[2 * 4 + j]
                + a[i * 4 + 3] * b[3 * 4 + j];
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(actual: f32, expected: f32) {
        assert!((actual - expected).abs() < 1.0e-6, "{actual} != {expected}");
    }

    #[test]
    fn layout_targets_only_selected_monitor_workspace() {
        let layout =
            transition_layout(3840, 1080, (1920, 0, 1920, 1080), 30).expect("valid layout");

        assert_eq!(layout.draw_rect, [1920.0, 30.0, 1920.0, 1050.0]);
        assert_eq!(layout.scissor, [1920, 0, 1920, 1050]);
        assert_close(layout.uv_rect[0], 0.5);
        assert_close(layout.uv_rect[1], 0.0);
        assert_close(layout.uv_rect[2], 0.5);
        assert_close(layout.uv_rect[3], 1050.0 / 1080.0);
    }

    #[test]
    fn layout_clips_malformed_monitor_without_overflowing_gl_bounds() {
        let layout =
            transition_layout(1920, 1080, (-100, -20, 800, 600), 40).expect("visible workspace");

        assert_eq!(layout.draw_rect, [0.0, 20.0, 700.0, 560.0]);
        assert_eq!(layout.scissor, [0, 500, 700, 560]);
        assert_close(layout.uv_rect[0], 0.0);
        assert_close(layout.uv_rect[1], 500.0 / 1080.0);
        assert_close(layout.uv_rect[2], 700.0 / 1920.0);
        assert_close(layout.uv_rect[3], 560.0 / 1080.0);
    }

    #[test]
    fn layout_rejects_empty_or_fully_excluded_workspaces() {
        assert!(transition_layout(1920, 1080, (0, 0, 0, 1080), 0).is_none());
        assert!(transition_layout(1920, 1080, (0, 0, 1920, 1080), 1080).is_none());
        assert!(transition_layout(1920, 1080, (2500, 0, 100, 100), 0).is_none());
    }

    #[test]
    fn blind_strip_scissors_are_intersections_of_workspace_scissor() {
        let layout =
            transition_layout(3840, 1200, (1920, 50, 1920, 1080), 30).expect("valid layout");
        let first = blinds_strip_layout(layout, 0, 8).expect("first strip");
        let last = blinds_strip_layout(layout, 7, 8).expect("last strip");

        assert_eq!(first.3, [1920, 70, 240, 1050]);
        assert_eq!(last.3, [3600, 70, 240, 1050]);
        assert_close(first.2[0], 0.5);
        assert_close(last.2[0] + last.2[2], 1.0);
        assert_eq!(first.3[1], layout.scissor[1]);
        assert_eq!(first.3[3], layout.scissor[3]);
    }
}
