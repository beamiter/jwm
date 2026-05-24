use super::*;
use smithay::backend::renderer::gles::ffi;

impl WaylandCompositor {
    /// Run the post-processing pass. Called from render_frame when postprocess_active is true.
    /// Copies the output FBO to postprocess FBO, applies effects, writes back to output FBO.
    #[allow(dead_code)]
    pub(crate) fn render_postprocess(&self, gl: &ffi::Gles2) {
        unsafe {
            // Copy output to postprocess FBO as source
            gl.BindFramebuffer(ffi::READ_FRAMEBUFFER, self.output_fbo);
            gl.BindFramebuffer(ffi::DRAW_FRAMEBUFFER, self.postprocess_fbo);
            gl.BlitFramebuffer(
                0, 0, self.screen_w as i32, self.screen_h as i32,
                0, 0, self.screen_w as i32, self.screen_h as i32,
                ffi::COLOR_BUFFER_BIT, ffi::NEAREST,
            );

            // Now render back to output FBO with post-processing shader
            gl.BindFramebuffer(ffi::FRAMEBUFFER, self.output_fbo);
            gl.Viewport(0, 0, self.screen_w as i32, self.screen_h as i32);

            // Use the magnifier version of postprocess shader (superset of basic)
            gl.UseProgram(self.postprocess_program);

            // Bind postprocess texture as input
            gl.ActiveTexture(ffi::TEXTURE0);
            gl.BindTexture(ffi::TEXTURE_2D, self.postprocess_texture);
            gl.Uniform1i(self.postprocess_uniforms.texture, 0);

            // Set all post-processing uniforms
            gl.Uniform1f(self.postprocess_uniforms.color_temp, self.color_temperature);
            gl.Uniform1f(self.postprocess_uniforms.saturation, self.saturation);
            gl.Uniform1f(self.postprocess_uniforms.brightness, self.brightness);
            gl.Uniform1f(self.postprocess_uniforms.contrast, self.contrast);
            gl.Uniform1i(self.postprocess_uniforms.invert, if self.invert_colors { 1 } else { 0 });
            gl.Uniform1i(self.postprocess_uniforms.grayscale, if self.grayscale { 1 } else { 0 });

            // Magnifier
            gl.Uniform1i(self.postprocess_uniforms.magnifier_enabled, if self.magnifier_enabled { 1 } else { 0 });
            if self.magnifier_enabled {
                let center_x = self.mouse_x / self.screen_w as f32;
                let center_y = self.mouse_y / self.screen_h as f32;
                gl.Uniform2f(self.postprocess_uniforms.magnifier_center, center_x, center_y);
                gl.Uniform1f(self.postprocess_uniforms.magnifier_radius, self.magnifier_radius);
                gl.Uniform1f(self.postprocess_uniforms.magnifier_zoom, self.magnifier_zoom);
            }

            // Colorblind correction
            gl.Uniform1i(self.postprocess_uniforms.colorblind_mode, self.colorblind_mode);

            // HDR tone mapping
            gl.Uniform1i(self.postprocess_uniforms.hdr_enabled, if self.hdr_enabled { 1 } else { 0 });
            gl.Uniform1f(self.postprocess_uniforms.hdr_peak_nits, self.hdr_peak_nits);
            gl.Uniform1i(self.postprocess_uniforms.tone_mapping_method, self.tone_mapping_method);

            // Draw fullscreen quad
            let proj = ortho(0.0, self.screen_w as f32, self.screen_h as f32, 0.0);
            let rect_loc = gl.GetUniformLocation(self.postprocess_program, b"u_rect\0".as_ptr() as *const _);
            let proj_loc = gl.GetUniformLocation(self.postprocess_program, b"u_projection\0".as_ptr() as *const _);
            if rect_loc >= 0 {
                gl.Uniform4f(rect_loc, 0.0, 0.0, self.screen_w as f32, self.screen_h as f32);
            }
            if proj_loc >= 0 {
                gl.UniformMatrix4fv(proj_loc, 1, ffi::FALSE as u8, proj.as_ptr());
            }

            gl.BindVertexArray(self.quad_vao);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
        }
    }

    /// Capture screenshot of the composited output to a PNG file.
    #[allow(dead_code)]
    pub(crate) fn capture_screenshot(&self, gl: &ffi::Gles2, path: &std::path::Path) -> bool {
        unsafe {
            let w = self.screen_w;
            let h = self.screen_h;
            let mut pixels = vec![0u8; (w * h * 4) as usize];

            gl.BindFramebuffer(ffi::FRAMEBUFFER, self.output_fbo);
            gl.ReadPixels(
                0, 0, w as i32, h as i32,
                ffi::RGBA, ffi::UNSIGNED_BYTE,
                pixels.as_mut_ptr() as *mut _,
            );

            // Flip vertically (OpenGL has origin at bottom-left)
            let row_size = (w * 4) as usize;
            let mut flipped = vec![0u8; pixels.len()];
            for y in 0..h as usize {
                let src_row = (h as usize - 1 - y) * row_size;
                let dst_row = y * row_size;
                flipped[dst_row..dst_row + row_size].copy_from_slice(&pixels[src_row..src_row + row_size]);
            }

            // Save as PNG
            if let Ok(file) = std::fs::File::create(path) {
                let writer = std::io::BufWriter::new(file);
                let mut encoder = png::Encoder::new(writer, w, h);
                encoder.set_color(png::ColorType::Rgba);
                encoder.set_depth(png::BitDepth::Eight);
                if let Ok(mut writer) = encoder.write_header() {
                    let _ = writer.write_image_data(&flipped);
                    return true;
                }
            }
        }
        false
    }

    /// Capture a region screenshot
    #[allow(dead_code)]
    pub(crate) fn capture_screenshot_region(&self, gl: &ffi::Gles2, path: &std::path::Path, x: i32, y: i32, w: u32, h: u32) -> bool {
        unsafe {
            let mut pixels = vec![0u8; (w * h * 4) as usize];

            gl.BindFramebuffer(ffi::FRAMEBUFFER, self.output_fbo);
            // Convert top-left origin y to OpenGL bottom-left origin
            let gl_y = self.screen_h as i32 - y - h as i32;
            gl.ReadPixels(
                x, gl_y, w as i32, h as i32,
                ffi::RGBA, ffi::UNSIGNED_BYTE,
                pixels.as_mut_ptr() as *mut _,
            );

            // Flip vertically
            let row_size = (w * 4) as usize;
            let mut flipped = vec![0u8; pixels.len()];
            for row in 0..h as usize {
                let src_row = (h as usize - 1 - row) * row_size;
                let dst_row = row * row_size;
                flipped[dst_row..dst_row + row_size].copy_from_slice(&pixels[src_row..src_row + row_size]);
            }

            if let Ok(file) = std::fs::File::create(path) {
                let writer = std::io::BufWriter::new(file);
                let mut encoder = png::Encoder::new(writer, w, h);
                encoder.set_color(png::ColorType::Rgba);
                encoder.set_depth(png::BitDepth::Eight);
                if let Ok(mut writer) = encoder.write_header() {
                    let _ = writer.write_image_data(&flipped);
                    return true;
                }
            }
        }
        false
    }

    /// Capture a window thumbnail (downsized to max_size)
    #[allow(dead_code)]
    pub(crate) fn capture_window_thumbnail(&self, gl: &ffi::Gles2, window: u64, max_size: u32) -> Option<(Vec<u8>, u32, u32)> {
        let win = self.windows.get(&window)?;
        let tex = win.gl_texture?;
        let w = win.width;
        let h = win.height;
        if w == 0 || h == 0 {
            return None;
        }

        // Calculate thumbnail size maintaining aspect ratio
        let (thumb_w, thumb_h) = if w > h {
            let tw = max_size.min(w);
            let th = (tw as f32 * h as f32 / w as f32) as u32;
            (tw, th.max(1))
        } else {
            let th = max_size.min(h);
            let tw = (th as f32 * w as f32 / h as f32) as u32;
            (tw.max(1), th)
        };

        unsafe {
            // Create temporary FBO for thumbnail
            let mut tmp_fbo = 0u32;
            let mut tmp_tex = 0u32;
            gl.GenTextures(1, &mut tmp_tex);
            gl.BindTexture(ffi::TEXTURE_2D, tmp_tex);
            gl.TexImage2D(
                ffi::TEXTURE_2D, 0, ffi::RGBA8 as i32,
                thumb_w as i32, thumb_h as i32, 0,
                ffi::RGBA, ffi::UNSIGNED_BYTE, std::ptr::null(),
            );
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, ffi::LINEAR as i32);
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, ffi::LINEAR as i32);

            gl.GenFramebuffers(1, &mut tmp_fbo);
            gl.BindFramebuffer(ffi::FRAMEBUFFER, tmp_fbo);
            gl.FramebufferTexture2D(ffi::FRAMEBUFFER, ffi::COLOR_ATTACHMENT0, ffi::TEXTURE_2D, tmp_tex, 0);

            // Render window texture to thumbnail FBO
            gl.Viewport(0, 0, thumb_w as i32, thumb_h as i32);
            gl.ClearColor(0.0, 0.0, 0.0, 0.0);
            gl.Clear(ffi::COLOR_BUFFER_BIT);

            gl.UseProgram(self.program);
            let proj = ortho(0.0, thumb_w as f32, thumb_h as f32, 0.0);
            gl.UniformMatrix4fv(self.win_uniforms.projection, 1, ffi::FALSE as u8, proj.as_ptr());
            gl.Uniform4f(self.win_uniforms.rect, 0.0, 0.0, thumb_w as f32, thumb_h as f32);
            gl.Uniform1f(self.win_uniforms.opacity, 1.0);
            gl.Uniform1f(self.win_uniforms.radius, 0.0);
            gl.Uniform2f(self.win_uniforms.size, thumb_w as f32, thumb_h as f32);
            gl.Uniform1f(self.win_uniforms.dim, 1.0);
            gl.Uniform4f(self.win_uniforms.uv_rect, 0.0, 0.0, 1.0, 1.0);
            gl.Uniform1f(self.win_uniforms.ripple_progress, -1.0);
            gl.Uniform1f(self.win_uniforms.ripple_amplitude, 0.0);

            gl.ActiveTexture(ffi::TEXTURE0);
            gl.BindTexture(ffi::TEXTURE_2D, tex);
            gl.Uniform1i(self.win_uniforms.texture, 0);

            gl.BindVertexArray(self.quad_vao);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);

            // Read pixels
            let mut pixels = vec![0u8; (thumb_w * thumb_h * 4) as usize];
            gl.ReadPixels(
                0, 0, thumb_w as i32, thumb_h as i32,
                ffi::RGBA, ffi::UNSIGNED_BYTE,
                pixels.as_mut_ptr() as *mut _,
            );

            // Cleanup temp resources
            gl.DeleteFramebuffers(1, &tmp_fbo);
            gl.DeleteTextures(1, &tmp_tex);

            // Restore output FBO
            gl.BindFramebuffer(ffi::FRAMEBUFFER, self.output_fbo);
            gl.Viewport(0, 0, self.screen_w as i32, self.screen_h as i32);

            // Flip Y
            let row_size = (thumb_w * 4) as usize;
            let mut flipped = vec![0u8; pixels.len()];
            for row in 0..thumb_h as usize {
                let src = (thumb_h as usize - 1 - row) * row_size;
                let dst = row * row_size;
                flipped[dst..dst + row_size].copy_from_slice(&pixels[src..src + row_size]);
            }

            Some((flipped, thumb_w, thumb_h))
        }
    }
}
