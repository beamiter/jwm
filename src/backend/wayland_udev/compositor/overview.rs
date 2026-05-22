use super::*;
use smithay::backend::renderer::gles::ffi;

impl WaylandCompositor {
    /// Render the overview (alt-tab) mode overlay.
    /// Shows a dark backdrop with window thumbnails arranged in a grid or prism.
    pub(crate) fn render_overview(&self, gl: &ffi::Gles2, projection: &[f32; 16]) {
        if self.overview_opacity <= 0.0 {
            return;
        }

        unsafe {
            // 1. Draw dark backdrop with vignette
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
                gl.Uniform1f(opacity_loc, self.overview_opacity);
            }

            gl.BindVertexArray(self.quad_vao);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);

            // 2. Draw each window thumbnail
            gl.UseProgram(self.program);
            gl.UniformMatrix4fv(self.win_uniforms.projection, 1, ffi::FALSE as u8, projection.as_ptr());

            for entry in &self.overview_entries {
                let win = match self.windows.get(&entry.window_id) {
                    Some(w) => w,
                    None => continue,
                };
                let tex = match win.gl_texture {
                    Some(t) => t,
                    None => continue,
                };

                let x = entry.x;
                let y = entry.y;
                let w = entry.w;
                let h = entry.h;

                // Scale by overview opacity for smooth reveal
                let scale = 0.8 + 0.2 * self.overview_opacity;
                let cx = x + w * 0.5;
                let cy = y + h * 0.5;
                let sx = cx - w * 0.5 * scale;
                let sy = cy - h * 0.5 * scale;
                let sw = w * scale;
                let sh = h * scale;

                gl.Uniform4f(self.win_uniforms.rect, sx, sy, sw, sh);
                gl.Uniform1f(self.win_uniforms.opacity, self.overview_opacity);
                gl.Uniform1f(self.win_uniforms.radius, 8.0);
                gl.Uniform2f(self.win_uniforms.size, sw, sh);
                gl.Uniform1f(self.win_uniforms.dim, 1.0);
                gl.Uniform4f(self.win_uniforms.uv_rect, 0.0, 0.0, 1.0, 1.0);
                gl.Uniform1f(self.win_uniforms.ripple_progress, -1.0);
                gl.Uniform1f(self.win_uniforms.ripple_amplitude, 0.0);

                gl.ActiveTexture(ffi::TEXTURE0);
                gl.BindTexture(ffi::TEXTURE_2D, tex);
                gl.Uniform1i(self.win_uniforms.texture, 0);

                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);

                // Draw selection border if this is the selected window
                let is_selected = self.overview_selection == Some(entry.window_id) || entry.focused;
                if is_selected {
                    gl.UseProgram(self.border_program);
                    gl.UniformMatrix4fv(self.border_uniforms.projection, 1, ffi::FALSE as u8, projection.as_ptr());
                    gl.Uniform4f(self.border_uniforms.rect, sx - 3.0, sy - 3.0, sw + 6.0, sh + 6.0);
                    gl.Uniform4f(self.border_uniforms.border_color, 0.4, 0.6, 1.0, self.overview_opacity);
                    gl.Uniform2f(self.border_uniforms.size, sw + 6.0, sh + 6.0);
                    gl.Uniform1f(self.border_uniforms.radius, 10.0);
                    gl.Uniform1f(self.border_uniforms.border_width, 3.0);

                    gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                    gl.UseProgram(self.program);
                    gl.UniformMatrix4fv(self.win_uniforms.projection, 1, ffi::FALSE as u8, projection.as_ptr());
                }
            }
        }
    }
}
