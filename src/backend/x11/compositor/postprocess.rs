use glow::HasContext;
use super::Compositor;
use super::math::ortho;

impl Compositor {
    /// Lazily create postprocess FBO if it doesn't exist yet.
    pub(super) fn ensure_postprocess_fbo(&mut self) {
        if self.postprocess_fbo.is_none() {
            self.postprocess_fbo = unsafe {
                Self::create_scene_fbo(&self.gl, self.screen_w, self.screen_h).ok()
            };
        }
    }

    /// Whether post-processing is active.
    pub(super) fn needs_postprocess(&self) -> bool {
        self.color_temperature != 0.0
            || self.saturation != 1.0
            || self.brightness != 1.0
            || self.contrast != 1.0
            || self.invert_colors
            || self.grayscale
            || self.magnifier_enabled
            || self.colorblind_mode != 0
    }

    /// Capture the current framebuffer to a PNG file.
    pub(super) fn capture_screenshot(&mut self, path: &std::path::Path) -> bool {
        let w = self.screen_w;
        let h = self.screen_h;
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        unsafe {
            self.gl.read_pixels(
                0, 0, w as i32, h as i32,
                glow::RGBA, glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut pixels)),
            );
        }
        // OpenGL reads bottom-to-top, flip vertically
        let row_bytes = (w * 4) as usize;
        let mut flipped = vec![0u8; pixels.len()];
        for y in 0..h as usize {
            let src_row = (h as usize - 1 - y) * row_bytes;
            let dst_row = y * row_bytes;
            flipped[dst_row..dst_row + row_bytes].copy_from_slice(&pixels[src_row..src_row + row_bytes]);
        }
        // Write PNG
        let file = match std::fs::File::create(path) {
            Ok(f) => f,
            Err(e) => {
                log::warn!("compositor: screenshot create failed: {e}");
                return false;
            }
        };
        let writer = std::io::BufWriter::new(file);
        let mut encoder = png::Encoder::new(writer, w, h);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        match encoder.write_header().and_then(|mut w| w.write_image_data(&flipped)) {
            Ok(_) => {
                log::info!("compositor: screenshot saved to {}", path.display());
                true
            }
            Err(e) => {
                log::warn!("compositor: screenshot encode failed: {e}");
                false
            }
        }
    }

    /// Render a specific window to an off-screen FBO and return RGBA pixel data.
    /// Returns None if the window isn't tracked. Dimensions are (width, height).
    pub(in crate::backend::x11) fn capture_window_thumbnail(&self, x11_win: u32, max_size: u32) -> Option<(Vec<u8>, u32, u32)> {
        let wt = self.windows.get(&x11_win)?;
        if wt.w == 0 || wt.h == 0 {
            return None;
        }

        // Calculate thumbnail size preserving aspect ratio
        let aspect = wt.w as f32 / wt.h as f32;
        let (tw, th) = if wt.w >= wt.h {
            let tw = max_size.min(wt.w);
            (tw, (tw as f32 / aspect) as u32)
        } else {
            let th = max_size.min(wt.h);
            ((th as f32 * aspect) as u32, th)
        };
        let tw = tw.max(1);
        let th = th.max(1);

        unsafe {
            // Create temp FBO
            let tex = self.gl.create_texture().ok()?;
            self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            self.gl.tex_image_2d(
                glow::TEXTURE_2D, 0, glow::RGBA8 as i32,
                tw as i32, th as i32, 0,
                glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(None),
            );
            self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            let fbo = self.gl.create_framebuffer().ok()?;
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            self.gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(tex), 0);

            self.gl.viewport(0, 0, tw as i32, th as i32);
            self.gl.clear_color(0.0, 0.0, 0.0, 0.0);
            self.gl.clear(glow::COLOR_BUFFER_BIT);

            let proj = ortho(0.0, tw as f32, th as f32, 0.0, -1.0, 1.0);
            self.gl.use_program(Some(self.program));
            self.gl.uniform_matrix_4_f32_slice(self.win_uniforms.projection.as_ref(), false, &proj);
            self.gl.uniform_1_i32(self.win_uniforms.texture.as_ref(), 0);
            self.gl.uniform_1_f32(self.win_uniforms.opacity.as_ref(), 1.0);
            self.gl.uniform_1_f32(self.win_uniforms.radius.as_ref(), 0.0);
            self.gl.uniform_1_f32(self.win_uniforms.dim.as_ref(), 1.0);
            self.gl.uniform_4_f32(self.win_uniforms.uv_rect.as_ref(), 0.0, 0.0, 1.0, 1.0);
            self.gl.uniform_2_f32(self.win_uniforms.size.as_ref(), tw as f32, th as f32);
            self.gl.uniform_4_f32(self.win_uniforms.rect.as_ref(), 0.0, 0.0, tw as f32, th as f32);
            self.gl.bind_vertex_array(Some(self.quad_vao));
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(wt.gl_texture));
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

            // Read pixels
            let mut pixels = vec![0u8; (tw * th * 4) as usize];
            self.gl.read_pixels(
                0, 0, tw as i32, th as i32,
                glow::RGBA, glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut pixels)),
            );

            // Cleanup temp FBO
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.gl.delete_framebuffer(fbo);
            self.gl.delete_texture(tex);
            self.gl.viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);

            Some((pixels, tw, th))
        }
    }
}
