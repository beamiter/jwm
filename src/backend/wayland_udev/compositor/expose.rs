use super::*;
use smithay::backend::renderer::gles::ffi;

impl WaylandCompositor {
    /// Render the expose (mission control) mode overlay.
    /// Shows all windows arranged in a grid layout.
    pub(crate) fn render_expose(&self, gl: &ffi::Gles2, projection: &[f32; 16]) {
        if self.expose_entries.is_empty() {
            return;
        }

        unsafe {
            // Dark backdrop
            gl.UseProgram(self.overview_bg_program);
            let rect_loc = gl.GetUniformLocation(self.overview_bg_program, b"u_rect\0".as_ptr() as *const _);
            let proj_loc = gl.GetUniformLocation(self.overview_bg_program, b"u_projection\0".as_ptr() as *const _);
            let opacity_loc = gl.GetUniformLocation(self.overview_bg_program, b"u_opacity\0".as_ptr() as *const _);

            if rect_loc >= 0 {
                gl.Uniform4f(rect_loc, 0.0, 0.0, self.screen_w as f32, self.screen_h as f32);
            }
            if proj_loc >= 0 {
                gl.UniformMatrix4fv(proj_loc, 1, ffi::FALSE as u8, projection.as_ptr());
            }
            if opacity_loc >= 0 {
                gl.Uniform1f(opacity_loc, 0.85);
            }

            gl.BindVertexArray(self.quad_vao);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);

            // Draw each expose window at its target position
            gl.UseProgram(self.program);
            gl.UniformMatrix4fv(self.win_uniforms.projection, 1, ffi::FALSE as u8, projection.as_ptr());

            for entry in &self.expose_entries {
                let win = match self.windows.get(&entry.window_id) {
                    Some(w) => w,
                    None => continue,
                };
                let tex = match win.gl_texture {
                    Some(t) => t,
                    None => continue,
                };

                let x = entry.x as f32;
                let y = entry.y as f32;
                let w = entry.w as f32;
                let h = entry.h as f32;

                // Draw shadow behind each window
                gl.UseProgram(self.shadow_program);
                gl.UniformMatrix4fv(self.shadow_uniforms.projection, 1, ffi::FALSE as u8, projection.as_ptr());
                let spread = 15.0f32;
                gl.Uniform4f(self.shadow_uniforms.rect, x - spread, y - spread, w + spread * 2.0, h + spread * 2.0);
                gl.Uniform4f(self.shadow_uniforms.shadow_color, 0.0, 0.0, 0.0, 0.5);
                gl.Uniform2f(self.shadow_uniforms.size, w, h);
                gl.Uniform1f(self.shadow_uniforms.radius, 6.0);
                gl.Uniform1f(self.shadow_uniforms.spread, spread);
                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);

                // Draw window
                gl.UseProgram(self.program);
                gl.UniformMatrix4fv(self.win_uniforms.projection, 1, ffi::FALSE as u8, projection.as_ptr());
                gl.Uniform4f(self.win_uniforms.rect, x, y, w, h);
                gl.Uniform1f(self.win_uniforms.opacity, 1.0);
                gl.Uniform1f(self.win_uniforms.radius, 6.0);
                gl.Uniform2f(self.win_uniforms.size, w, h);
                gl.Uniform1f(self.win_uniforms.dim, 1.0);
                gl.Uniform4f(self.win_uniforms.uv_rect, 0.0, 0.0, 1.0, 1.0);
                gl.Uniform1f(self.win_uniforms.ripple_progress, -1.0);
                gl.Uniform1f(self.win_uniforms.ripple_amplitude, 0.0);

                gl.ActiveTexture(ffi::TEXTURE0);
                gl.BindTexture(ffi::TEXTURE_2D, tex);
                gl.Uniform1i(self.win_uniforms.texture, 0);

                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
            }
        }
    }
}
