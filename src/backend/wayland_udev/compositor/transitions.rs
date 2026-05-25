use super::*;
use smithay::backend::renderer::gles::ffi;

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
        let progress = (elapsed.as_secs_f32() / duration).min(1.0);

        // Ease-out cubic
        let t = 1.0 - (1.0 - progress).powi(3);

        if progress >= 1.0 {
            self.transition_active = false;
            self.transition_start = None;
            return;
        }

        match self.transition_mode {
            TransitionMode::Slide => self.render_slide_transition(gl, projection, t),
            TransitionMode::Cube => self.render_cube_transition(gl, projection, t),
            TransitionMode::Flip => self.render_flip_transition(gl, projection, t),
            TransitionMode::Fade => self.render_fade_transition(gl, projection, t),
            TransitionMode::Zoom => self.render_zoom_transition(gl, projection, t),
            TransitionMode::Portal => self.render_portal_transition(gl, projection, t),
            TransitionMode::Stack => self.render_stack_transition(gl, projection, t),
            TransitionMode::Blinds => self.render_blinds_transition(gl, projection, t),
            TransitionMode::CoverFlow => self.render_coverflow_transition(gl, projection, t),
            TransitionMode::Helix => self.render_helix_transition(gl, projection, t),
            TransitionMode::None => {}
        }

        self.needs_render = true;
    }

    fn render_slide_transition(&self, gl: &ffi::Gles2, projection: &[f32; 16], t: f32) {
        unsafe {
            gl.UseProgram(self.transition_program);
            gl.UniformMatrix4fv(self.transition_uniforms.projection, 1, ffi::FALSE as u8, projection.as_ptr());
            gl.Uniform4f(self.transition_uniforms.uv_rect, 0.0, 0.0, 1.0, 1.0);

            // Old scene slides out
            let dir = self.transition_direction as f32;
            let offset_x = -dir * t * self.screen_w as f32;
            gl.Uniform4f(
                self.transition_uniforms.rect,
                offset_x, 0.0,
                self.screen_w as f32, self.screen_h as f32,
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

    fn render_cube_transition(&self, gl: &ffi::Gles2, _projection: &[f32; 16], t: f32) {
        unsafe {
            gl.UseProgram(self.cube_program);

            let dir = self.transition_direction as f32;
            let angle = dir * t * std::f32::consts::FRAC_PI_2; // 0 to 90 degrees

            let aspect = self.screen_w as f32 / self.screen_h as f32;
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
            rot[0] = cos_a; rot[2] = sin_a;
            rot[5] = 1.0;
            rot[8] = -sin_a; rot[10] = cos_a;
            rot[15] = 1.0;

            // Translation (push back)
            let mut trans = [0.0f32; 16];
            trans[0] = 1.0; trans[5] = 1.0; trans[10] = 1.0; trans[15] = 1.0;
            trans[14] = -2.5; // z offset

            // MVP = persp * trans * rot
            let view = mat4_mul(&trans, &rot);
            let mvp = mat4_mul(&persp, &view);

            gl.UniformMatrix4fv(self.cube_uniforms.mvp, 1, ffi::FALSE as u8, mvp.as_ptr());
            gl.Uniform4f(self.cube_uniforms.uv_rect, 0.0, 0.0, 1.0, 1.0);

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

    fn render_flip_transition(&self, gl: &ffi::Gles2, projection: &[f32; 16], t: f32) {
        // Card flip: scale X from 1 to 0 (first half) then 0 to 1 (second half showing new)
        // We only render the old scene (first half of flip)
        unsafe {
            if t < 0.5 {
                let scale_x = 1.0 - t * 2.0;
                let center_x = self.screen_w as f32 * 0.5;
                let w = self.screen_w as f32 * scale_x;
                let x = center_x - w * 0.5;

                gl.UseProgram(self.transition_program);
                gl.UniformMatrix4fv(self.transition_uniforms.projection, 1, ffi::FALSE as u8, projection.as_ptr());
                gl.Uniform4f(self.transition_uniforms.rect, x, 0.0, w, self.screen_h as f32);
                gl.Uniform4f(self.transition_uniforms.uv_rect, 0.0, 0.0, 1.0, 1.0);
                gl.Uniform1f(self.transition_uniforms.opacity, 1.0);

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
    }

    fn render_fade_transition(&self, gl: &ffi::Gles2, projection: &[f32; 16], t: f32) {
        unsafe {
            gl.UseProgram(self.transition_program);
            gl.UniformMatrix4fv(self.transition_uniforms.projection, 1, ffi::FALSE as u8, projection.as_ptr());
            gl.Uniform4f(self.transition_uniforms.rect, 0.0, 0.0, self.screen_w as f32, self.screen_h as f32);
            gl.Uniform4f(self.transition_uniforms.uv_rect, 0.0, 0.0, 1.0, 1.0);
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

    fn render_zoom_transition(&self, gl: &ffi::Gles2, projection: &[f32; 16], t: f32) {
        unsafe {
            let scale = 1.0 + t * 0.3; // zoom out slightly
            let w = self.screen_w as f32 * scale;
            let h = self.screen_h as f32 * scale;
            let x = (self.screen_w as f32 - w) * 0.5;
            let y = (self.screen_h as f32 - h) * 0.5;

            gl.UseProgram(self.transition_program);
            gl.UniformMatrix4fv(self.transition_uniforms.projection, 1, ffi::FALSE as u8, projection.as_ptr());
            gl.Uniform4f(self.transition_uniforms.rect, x, y, w, h);
            gl.Uniform4f(self.transition_uniforms.uv_rect, 0.0, 0.0, 1.0, 1.0);
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

    fn render_portal_transition(&self, gl: &ffi::Gles2, projection: &[f32; 16], t: f32) {
        unsafe {
            gl.UseProgram(self.portal_program);
            gl.UniformMatrix4fv(self.portal_uniforms.projection, 1, ffi::FALSE as u8, projection.as_ptr());
            gl.Uniform4f(self.portal_uniforms.rect, 0.0, 0.0, self.screen_w as f32, self.screen_h as f32);
            gl.Uniform4f(self.portal_uniforms.uv_rect, 0.0, 0.0, 1.0, 1.0);
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

    fn render_stack_transition(&self, gl: &ffi::Gles2, projection: &[f32; 16], t: f32) {
        unsafe {
            let dir = self.transition_direction as f32;
            let scale = 1.0 - t * 0.15; // scale from 1.0 to 0.85
            let opacity = 1.0 - t;

            let w = self.screen_w as f32 * scale;
            let h = self.screen_h as f32 * scale;
            let x = (self.screen_w as f32 - w) * 0.5 + dir * t * self.screen_w as f32 * 0.05;
            let y = (self.screen_h as f32 - h) * 0.5;

            gl.UseProgram(self.transition_program);
            gl.UniformMatrix4fv(self.transition_uniforms.projection, 1, ffi::FALSE as u8, projection.as_ptr());
            gl.Uniform4f(self.transition_uniforms.rect, x, y, w, h);
            gl.Uniform4f(self.transition_uniforms.uv_rect, 0.0, 0.0, 1.0, 1.0);
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

    fn render_blinds_transition(&self, gl: &ffi::Gles2, projection: &[f32; 16], t: f32) {
        unsafe {
            gl.UseProgram(self.transition_program);
            gl.UniformMatrix4fv(self.transition_uniforms.projection, 1, ffi::FALSE as u8, projection.as_ptr());
            gl.Uniform1f(self.transition_uniforms.opacity, 1.0);

            gl.ActiveTexture(ffi::TEXTURE0);
            gl.BindTexture(ffi::TEXTURE_2D, self.transition_texture);
            gl.Uniform1i(
                gl.GetUniformLocation(self.transition_program, b"u_texture\0".as_ptr() as *const _),
                0,
            );

            let num_strips: i32 = 8;
            let stagger = 0.3f32;
            let screen_w = self.screen_w as f32;
            let screen_h = self.screen_h as f32;
            let strip_w_f = screen_w / num_strips as f32;

            gl.Enable(ffi::SCISSOR_TEST);

            for i in 0..num_strips {
                let strip_delay = (i as f32 / (num_strips - 1) as f32) * stagger;
                let strip_progress = ((t - strip_delay) / (1.0 - stagger)).clamp(0.0, 1.0);

                if strip_progress >= 0.5 {
                    // Second half: new scene shows through, don't draw old
                    continue;
                }

                // First half: old scene strip squeezes horizontally toward center of strip
                let squeeze = strip_progress * 2.0; // 0.0 to 1.0
                let strip_x = (i as f32 * strip_w_f) as i32;
                let strip_w_i = strip_w_f as i32;

                gl.Scissor(strip_x, 0, strip_w_i, screen_h as i32);

                let center_x = i as f32 * strip_w_f + strip_w_f * 0.5;
                let w = strip_w_f * (1.0 - squeeze);
                let x = center_x - w * 0.5;

                // Draw old scene with UV covering only this strip
                let uv_left = i as f32 / num_strips as f32;
                let uv_width = 1.0 / num_strips as f32;
                gl.Uniform4f(self.transition_uniforms.uv_rect, uv_left, 0.0, uv_width, 1.0);
                gl.Uniform4f(self.transition_uniforms.rect, x, 0.0, w, screen_h);

                gl.BindVertexArray(self.quad_vao);
                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
            }

            gl.Disable(ffi::SCISSOR_TEST);
        }
    }

    fn render_coverflow_transition(&self, gl: &ffi::Gles2, _projection: &[f32; 16], t: f32) {
        unsafe {
            gl.UseProgram(self.cube_program);

            let dir = self.transition_direction as f32;
            let max_rotation = 70.0_f32.to_radians();
            let angle = t * max_rotation * dir;

            let aspect = self.screen_w as f32 / self.screen_h as f32;
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
            trans[0] = 1.0; trans[5] = 1.0; trans[10] = 1.0; trans[15] = 1.0;
            trans[14] = -2.5;

            // Rotation around Y axis
            let cos_a = angle.cos();
            let sin_a = angle.sin();
            let mut rot = [0.0f32; 16];
            rot[0] = cos_a; rot[2] = sin_a;
            rot[5] = 1.0;
            rot[8] = -sin_a; rot[10] = cos_a;
            rot[15] = 1.0;

            // Lateral offset translation
            let mut lateral = [0.0f32; 16];
            lateral[0] = 1.0; lateral[5] = 1.0; lateral[10] = 1.0; lateral[15] = 1.0;
            lateral[12] = t * 1.5 * dir;

            // MVP = persp * trans * rot * lateral
            let model = mat4_mul(&rot, &lateral);
            let view = mat4_mul(&trans, &model);
            let mvp = mat4_mul(&persp, &view);

            gl.UniformMatrix4fv(self.cube_uniforms.mvp, 1, ffi::FALSE as u8, mvp.as_ptr());
            gl.Uniform4f(self.cube_uniforms.uv_rect, 0.0, 0.0, 1.0, 1.0);

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

    fn render_helix_transition(&self, gl: &ffi::Gles2, _projection: &[f32; 16], t: f32) {
        unsafe {
            gl.UseProgram(self.cube_program);

            let dir = self.transition_direction as f32;
            let theta = t * std::f32::consts::PI;
            let r = 0.8f32;

            let x = r * theta.sin() * dir;
            let z = -r * (1.0 - theta.cos());
            let s = 1.0 - t * 0.3;

            let aspect = self.screen_w as f32 / self.screen_h as f32;
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
            trans[0] = 1.0; trans[5] = 1.0; trans[10] = 1.0; trans[15] = 1.0;
            trans[12] = x;
            trans[14] = -2.5 + z;

            // Rotation around Y axis (theta * direction)
            let rot_angle = theta * dir;
            let cos_a = rot_angle.cos();
            let sin_a = rot_angle.sin();
            let mut rot = [0.0f32; 16];
            rot[0] = cos_a; rot[2] = sin_a;
            rot[5] = 1.0;
            rot[8] = -sin_a; rot[10] = cos_a;
            rot[15] = 1.0;

            // Scale matrix
            let mut scale = [0.0f32; 16];
            scale[0] = s; scale[5] = s; scale[10] = 1.0; scale[15] = 1.0;

            // MVP = persp * trans * rot * scale
            let model = mat4_mul(&rot, &scale);
            let view = mat4_mul(&trans, &model);
            let mvp = mat4_mul(&persp, &view);

            gl.UniformMatrix4fv(self.cube_uniforms.mvp, 1, ffi::FALSE as u8, mvp.as_ptr());
            gl.Uniform4f(self.cube_uniforms.uv_rect, 0.0, 0.0, 1.0, 1.0);

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
                0, 0, self.screen_w as i32, self.screen_h as i32,
                0, 0, self.screen_w as i32, self.screen_h as i32,
                ffi::COLOR_BUFFER_BIT, ffi::NEAREST,
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
