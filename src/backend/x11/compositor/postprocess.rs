use super::math::ortho;
use super::*;
use crate::backend::compositor_common::capture::{clip_region, flip_rgba_vertical};
use crate::backend::compositor_common::screenshot::save_png_async;
use glow::HasContext;

impl<C: CompositorConnection> Compositor<C> {
    /// Lazily create postprocess FBO if it doesn't exist yet.
    pub(super) fn ensure_postprocess_fbo(&mut self) {
        if self.postprocess_fbo.is_none() {
            self.postprocess_fbo =
                unsafe { Self::create_scene_fbo(&self.gl, self.screen_w, self.screen_h).ok() };
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
            || self.hdr_enabled
    }

    /// Capture the current framebuffer to a PNG file.
    pub(super) fn capture_screenshot(&mut self, path: &std::path::Path) -> bool {
        let w = self.screen_w;
        let h = self.screen_h;
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        unsafe {
            self.gl.read_pixels(
                0,
                0,
                w as i32,
                h as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut pixels)),
            );
        }
        flip_rgba_vertical(&mut pixels, w, h);
        save_png_async(
            path.to_path_buf(),
            pixels,
            w,
            h,
            self.renderer_ctx("screenshot: save PNG"),
        );
        true
    }

    /// Capture a region of the current framebuffer to a PNG file.
    pub(super) fn capture_screenshot_region(
        &mut self,
        path: &std::path::Path,
        rx: i32,
        ry: i32,
        rw: u32,
        rh: u32,
    ) -> bool {
        let Some(region) = clip_region(self.screen_w, self.screen_h, rx, ry, rw, rh) else {
            log::warn!(
                "{}: requested region is empty",
                self.renderer_ctx("screenshot-region: clip region")
            );
            return false;
        };
        let (x, y, w, h) = (region.x, region.y, region.width, region.height);
        // OpenGL Y is flipped: GL origin is bottom-left
        let gl_y = self.screen_h.saturating_sub(y + h);
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        unsafe {
            self.gl.read_pixels(
                x as i32,
                gl_y as i32,
                w as i32,
                h as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut pixels)),
            );
        }
        flip_rgba_vertical(&mut pixels, w, h);
        save_png_async(
            path.to_path_buf(),
            pixels,
            w,
            h,
            self.renderer_ctx("screenshot-region: save PNG"),
        );
        log::info!(
            "compositor: region screenshot queued to {} ({}x{} at {},{})",
            path.display(),
            w,
            h,
            x,
            y
        );
        true
    }

    /// Render a specific window to an off-screen FBO and return RGBA pixel data.
    /// Returns None if the window isn't tracked. Dimensions are (width, height).
    pub(crate) fn capture_window_thumbnail(
        &self,
        x11_win: u32,
        max_size: u32,
    ) -> Option<(Vec<u8>, u32, u32)> {
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
            let tex = self
                .gl
                .create_texture()
                .map_err(|error| {
                    log::warn!(
                        "{}: {error}",
                        self.renderer_ctx("thumbnail: create texture")
                    );
                })
                .ok()?;
            self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            // Use 10-bit internal format for HDR-ready pipeline
            const GL_RGB10_A2: u32 = 0x8059;
            self.gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                GL_RGB10_A2 as i32,
                tw as i32,
                th as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_INT_2_10_10_10_REV,
                glow::PixelUnpackData::Slice(None),
            );
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::LINEAR as i32,
            );
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::LINEAR as i32,
            );
            let fbo = self
                .gl
                .create_framebuffer()
                .map_err(|error| {
                    // Release the texture the FBO would have owned.
                    self.gl.delete_texture(tex);
                    log::warn!(
                        "{}: {error}",
                        self.renderer_ctx("thumbnail: create framebuffer")
                    );
                })
                .ok()?;
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            self.gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(tex),
                0,
            );

            self.gl.viewport(0, 0, tw as i32, th as i32);
            self.gl.clear_color(0.0, 0.0, 0.0, 0.0);
            self.gl.clear(glow::COLOR_BUFFER_BIT);

            let proj = ortho(0.0, tw as f32, th as f32, 0.0, -1.0, 1.0);
            self.gl.use_program(Some(self.program));
            self.gl
                .uniform_matrix_4_f32_slice(self.win_uniforms.projection.as_ref(), false, &proj);
            self.gl.uniform_1_i32(self.win_uniforms.texture.as_ref(), 0);
            self.gl
                .uniform_1_f32(self.win_uniforms.opacity.as_ref(), 1.0);
            self.gl
                .uniform_1_f32(self.win_uniforms.radius.as_ref(), 0.0);
            self.gl.uniform_1_f32(self.win_uniforms.dim.as_ref(), 1.0);
            self.gl
                .uniform_4_f32(self.win_uniforms.uv_rect.as_ref(), 0.0, 0.0, 1.0, 1.0);
            self.gl
                .uniform_2_f32(self.win_uniforms.size.as_ref(), tw as f32, th as f32);
            self.gl.uniform_4_f32(
                self.win_uniforms.rect.as_ref(),
                0.0,
                0.0,
                tw as f32,
                th as f32,
            );
            self.gl.bind_vertex_array(Some(self.quad_vao));
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(wt.gl_texture));
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

            // Read pixels
            let mut pixels = vec![0u8; (tw * th * 4) as usize];
            self.gl.read_pixels(
                0,
                0,
                tw as i32,
                th as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut pixels)),
            );

            // Cleanup temp FBO
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.gl.delete_framebuffer(fbo);
            self.gl.delete_texture(tex);
            self.gl
                .viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);

            Some((pixels, tw, th))
        }
    }
}
