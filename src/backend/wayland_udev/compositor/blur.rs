use super::*;
use smithay::backend::renderer::gles::ffi;

impl WaylandCompositor {
    /// Run dual-Kawase blur passes on the source texture.
    /// Downsamples through blur_fbos levels then upsamples back.
    /// Result texture is blur_fbos[0].texture.
    pub(crate) fn run_blur_passes(&self, gl: &ffi::Gles2, source_texture: u32, projection: &[f32; 16]) {
        if self.blur_fbos.is_empty() {
            return;
        }
        let levels = (self.blur_strength as usize).min(self.blur_fbos.len());
        if levels == 0 {
            return;
        }

        unsafe {
            gl.Disable(ffi::BLEND);

            // --- Downsample pass ---
            gl.UseProgram(self.blur_down_program);
            let mut prev_tex = source_texture;

            for i in 0..levels {
                let level = &self.blur_fbos[i];
                gl.BindFramebuffer(ffi::FRAMEBUFFER, level.fbo);
                gl.Viewport(0, 0, level.width as i32, level.height as i32);

                // Set uniforms
                gl.Uniform4f(self.blur_uniforms.rect, 0.0, 0.0, level.width as f32, level.height as f32);
                let blur_proj = ortho(0.0, level.width as f32, level.height as f32, 0.0);
                gl.UniformMatrix4fv(self.blur_uniforms.projection, 1, ffi::FALSE as u8, blur_proj.as_ptr());
                gl.Uniform2f(
                    self.blur_uniforms.halfpixel,
                    0.5 / level.width as f32,
                    0.5 / level.height as f32,
                );

                gl.ActiveTexture(ffi::TEXTURE0);
                gl.BindTexture(ffi::TEXTURE_2D, prev_tex);
                gl.Uniform1i(self.blur_uniforms.texture, 0);

                gl.BindVertexArray(self.quad_vao);
                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);

                prev_tex = level.texture;
            }

            // --- Upsample pass ---
            gl.UseProgram(self.blur_up_program);
            // Reuse blur_uniforms since upsample shader has same uniforms

            for i in (0..levels - 1).rev() {
                let level = &self.blur_fbos[i];
                gl.BindFramebuffer(ffi::FRAMEBUFFER, level.fbo);
                gl.Viewport(0, 0, level.width as i32, level.height as i32);

                gl.Uniform4f(self.blur_uniforms.rect, 0.0, 0.0, level.width as f32, level.height as f32);
                let blur_proj = ortho(0.0, level.width as f32, level.height as f32, 0.0);
                gl.UniformMatrix4fv(self.blur_uniforms.projection, 1, ffi::FALSE as u8, blur_proj.as_ptr());
                gl.Uniform2f(
                    self.blur_uniforms.halfpixel,
                    0.5 / level.width as f32,
                    0.5 / level.height as f32,
                );

                gl.ActiveTexture(ffi::TEXTURE0);
                gl.BindTexture(ffi::TEXTURE_2D, prev_tex);
                gl.Uniform1i(self.blur_uniforms.texture, 0);

                gl.BindVertexArray(self.quad_vao);
                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);

                prev_tex = level.texture;
            }

            // Restore blend
            gl.Enable(ffi::BLEND);
        }
    }

    /// Capture the current output FBO content into the scene FBO for blur source
    pub(crate) fn capture_scene_for_blur(&self, gl: &ffi::Gles2) {
        unsafe {
            gl.BindFramebuffer(ffi::READ_FRAMEBUFFER, self.output_fbo);
            gl.BindFramebuffer(ffi::DRAW_FRAMEBUFFER, self.scene_fbo);
            gl.BlitFramebuffer(
                0, 0, self.screen_w as i32, self.screen_h as i32,
                0, 0, self.screen_w as i32, self.screen_h as i32,
                ffi::COLOR_BUFFER_BIT, ffi::NEAREST,
            );
            gl.BindFramebuffer(ffi::FRAMEBUFFER, self.output_fbo);
        }
    }

    /// Get the blurred texture result (first blur level after passes)
    pub(crate) fn blur_result_texture(&self) -> u32 {
        self.blur_fbos.first().map(|l| l.texture).unwrap_or(0)
    }
}
