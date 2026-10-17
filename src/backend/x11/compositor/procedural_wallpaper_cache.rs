//! Full-resolution procedural output, reused for unrelated client damage.
//! Astronomy copies only its canvases: holes retain the underlying static layer.
use glow::HasContext;

/// SDR color effects also render through the compositor's RGB10_A2 scene FBO.
/// Match that destination before quantizing procedural shader output.
pub(super) fn uses_ten_bit(hdr: bool, output_ten_bit: bool, postprocess_active: bool) -> bool {
    hdr || output_ten_bit || postprocess_active
}

#[derive(Default)]
pub(super) struct ProceduralFrameCache {
    target: Option<(glow::Framebuffer, glow::Texture)>,
    size: (u32, u32, bool),
    failed_size: Option<(u32, u32, bool)>,
    valid: bool,
}

impl ProceduralFrameCache {
    pub(super) fn invalidate(&mut self) {
        self.valid = false;
    }

    /// Requires the owning GL context current, including during destruction.
    pub(super) unsafe fn cleanup(&mut self, gl: &glow::Context) {
        if let Some((fbo, texture)) = self.target.take() {
            unsafe {
                gl.delete_framebuffer(fbo);
                gl.delete_texture(texture);
            }
        }
        self.valid = false;
        self.failed_size = None;
    }

    /// Preserve bindings even on allocation failure. A failed size is not retried
    /// on every repaint; rendering directly remains a correct fallback.
    pub(super) unsafe fn ensure(
        &mut self,
        gl: &glow::Context,
        w: u32,
        h: u32,
        ten_bit: bool,
    ) -> bool {
        let size = (w, h, ten_bit);
        if self.target.is_some() && self.size == size {
            return true;
        }
        if self.failed_size == Some(size) {
            return false;
        }
        unsafe {
            self.cleanup(gl);
        }
        self.size = size;
        self.failed_size = Some(size);
        unsafe {
            let max = gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE).max(0) as u32;
            if w == 0 || h == 0 || w > max || h > max || w > i32::MAX as u32 || h > i32::MAX as u32
            {
                return false;
            }
            let old_texture = gl.get_parameter_texture(glow::TEXTURE_BINDING_2D);
            let old_unpack = gl.get_parameter_buffer(glow::PIXEL_UNPACK_BUFFER_BINDING);
            let old_draw = gl.get_parameter_framebuffer(glow::DRAW_FRAMEBUFFER_BINDING);
            let Ok(texture) = gl.create_texture() else {
                return false;
            };
            let Ok(fbo) = gl.create_framebuffer() else {
                gl.delete_texture(texture);
                return false;
            };
            gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            let (format, kind) = storage_format(ten_bit);
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                format as i32,
                w as i32,
                h as i32,
                0,
                glow::RGBA,
                kind,
                glow::PixelUnpackData::Slice(None),
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::NEAREST as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::NEAREST as i32,
            );
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(
                glow::DRAW_FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(texture),
                0,
            );
            let complete =
                gl.check_framebuffer_status(glow::DRAW_FRAMEBUFFER) == glow::FRAMEBUFFER_COMPLETE;
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, old_draw);
            gl.bind_texture(glow::TEXTURE_2D, old_texture);
            gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, old_unpack);
            if !complete {
                gl.delete_framebuffer(fbo);
                gl.delete_texture(texture);
                return false;
            }
            self.target = Some((fbo, texture));
            self.failed_size = None;
            true
        }
    }

    pub(super) fn valid(&self) -> bool {
        self.valid && self.target.is_some()
    }

    /// Render with global gl_FragCoord unchanged and no damage clipping.
    pub(super) unsafe fn begin<'a>(&self, gl: &'a glow::Context) -> CacheRenderState<'a> {
        unsafe {
            let state = CacheRenderState::capture(gl);
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, self.target.map(|t| t.0));
            gl.viewport(0, 0, self.size.0 as i32, self.size.1 as i32);
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::BLEND);
            state
        }
    }

    pub(super) fn finish(&mut self) {
        self.valid = true;
    }

    /// The caller's scissor restricts repairs; only initialized canvas pixels
    /// are copied. Blits avoid shader reruns, texture filtering and alpha loss.
    pub(super) unsafe fn present(
        &self,
        gl: &glow::Context,
        clips: impl IntoIterator<Item = (i32, i32, i32, i32)>,
    ) {
        let Some((fbo, _)) = self.target else {
            return;
        };
        unsafe {
            let old_read = gl.get_parameter_framebuffer(glow::READ_FRAMEBUFFER_BINDING);
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(fbo));
            for (x, y, w, h) in clips {
                gl.blit_framebuffer(
                    x,
                    y,
                    x + w,
                    y + h,
                    x,
                    y,
                    x + w,
                    y + h,
                    glow::COLOR_BUFFER_BIT,
                    glow::NEAREST,
                );
            }
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, old_read);
        }
    }
}

fn storage_format(ten_bit: bool) -> (u32, u32) {
    if ten_bit {
        (glow::RGB10_A2, glow::UNSIGNED_INT_2_10_10_10_REV)
    } else {
        (glow::RGBA8, glow::UNSIGNED_BYTE)
    }
}

pub(super) struct CacheRenderState<'a> {
    gl: &'a glow::Context,
    draw: Option<glow::Framebuffer>,
    viewport: [i32; 4],
    scissor: [i32; 4],
    scissor_enabled: bool,
    blend: bool,
}
impl<'a> CacheRenderState<'a> {
    unsafe fn capture(gl: &'a glow::Context) -> Self {
        unsafe {
            let mut viewport = [0; 4];
            let mut scissor = [0; 4];
            gl.get_parameter_i32_slice(glow::VIEWPORT, &mut viewport);
            gl.get_parameter_i32_slice(glow::SCISSOR_BOX, &mut scissor);
            Self {
                gl,
                draw: gl.get_parameter_framebuffer(glow::DRAW_FRAMEBUFFER_BINDING),
                viewport,
                scissor,
                scissor_enabled: gl.is_enabled(glow::SCISSOR_TEST),
                blend: gl.is_enabled(glow::BLEND),
            }
        }
    }
}
impl Drop for CacheRenderState<'_> {
    fn drop(&mut self) {
        unsafe {
            self.gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, self.draw);
            let [x, y, w, h] = self.viewport;
            self.gl.viewport(x, y, w, h);
            let [x, y, w, h] = self.scissor;
            self.gl.scissor(x, y, w, h);
            for (cap, enabled) in [
                (glow::SCISSOR_TEST, self.scissor_enabled),
                (glow::BLEND, self.blend),
            ] {
                if enabled {
                    self.gl.enable(cap);
                } else {
                    self.gl.disable(cap);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cache_precision_matches_sdr_postprocess_destination() {
        assert!(!uses_ten_bit(false, false, false));
        assert!(uses_ten_bit(false, false, true));
        assert!(uses_ten_bit(true, false, false));
        assert!(uses_ten_bit(false, true, false));
    }
    #[test]
    fn cache_invalidation_and_cleanup_identity() {
        let mut cache = ProceduralFrameCache::default();
        assert!(!cache.valid());
        cache.target = Some((
            glow::NativeFramebuffer(std::num::NonZeroU32::new(1).unwrap()),
            glow::NativeTexture(std::num::NonZeroU32::new(2).unwrap()),
        ));
        cache.finish();
        assert!(cache.valid());
        cache.invalidate();
        assert!(!cache.valid());
    }
    #[test]
    fn cache_storage_preserves_output_precision_with_legal_types() {
        assert_eq!(storage_format(false), (glow::RGBA8, glow::UNSIGNED_BYTE));
        assert_eq!(
            storage_format(true),
            (glow::RGB10_A2, glow::UNSIGNED_INT_2_10_10_10_REV)
        );
    }
}
