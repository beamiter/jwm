use super::math::ortho;
use super::*;
use crate::backend::compositor_common::capture::{clip_region, flip_rgba_vertical};
use crate::backend::compositor_common::minimized_thumbnail::snapshot_shader_opacity;
use crate::backend::compositor_common::screenshot::save_png_async;
use glow::HasContext;

/// A dedicated full-target quad. `v_uv.y == 0` is the top of the source;
/// retaining the FBO attachment therefore requires a flipped UV rectangle
/// when that texture is later sampled by the compositor's top-down quads.
pub(super) const THUMBNAIL_DOWNSAMPLE_VERTEX_SHADER: &str = r#"#version 330 core
out vec2 v_uv;
void main() {
    vec2 pos = vec2(float(gl_VertexID & 1), float((gl_VertexID >> 1) & 1));
    v_uv = pos;
    gl_Position = vec4(pos.x * 2.0 - 1.0, 1.0 - pos.y * 2.0, 0.0, 1.0);
}
"#;

/// A thumbnail draw is an auxiliary pass in the middle of a compositor
/// frame. Capture and normalize every piece of mutable GL state it touches so
/// success and every allocation/FBO failure return to the exact caller state.
struct ThumbnailGlState<'a> {
    gl: &'a glow::Context,
    draw_framebuffer: Option<glow::Framebuffer>,
    read_framebuffer: Option<glow::Framebuffer>,
    viewport: [i32; 4],
    program: Option<glow::Program>,
    vertex_array: Option<glow::VertexArray>,
    active_texture: u32,
    active_texture_binding: Option<glow::Texture>,
    texture0_binding: Option<glow::Texture>,
    pixel_pack_buffer: Option<glow::Buffer>,
    pixel_unpack_buffer: Option<glow::Buffer>,
    pack_alignment: i32,
    unpack_alignment: i32,
    blend_enabled: bool,
    scissor_enabled: bool,
    depth_test_enabled: bool,
    stencil_test_enabled: bool,
    cull_face_enabled: bool,
    color_mask: [bool; 4],
    clear_color: [f32; 4],
}

impl<'a> ThumbnailGlState<'a> {
    unsafe fn begin(gl: &'a glow::Context) -> Self {
        unsafe {
            let draw_framebuffer = gl.get_parameter_framebuffer(glow::DRAW_FRAMEBUFFER_BINDING);
            let read_framebuffer = gl.get_parameter_framebuffer(glow::READ_FRAMEBUFFER_BINDING);
            let mut viewport = [0; 4];
            gl.get_parameter_i32_slice(glow::VIEWPORT, &mut viewport);
            let program = gl.get_parameter_program(glow::CURRENT_PROGRAM);
            let vertex_array = gl.get_parameter_vertex_array(glow::VERTEX_ARRAY_BINDING);
            let active_texture = gl.get_parameter_i32(glow::ACTIVE_TEXTURE) as u32;
            let active_texture_binding = gl.get_parameter_texture(glow::TEXTURE_BINDING_2D);
            gl.active_texture(glow::TEXTURE0);
            let texture0_binding = gl.get_parameter_texture(glow::TEXTURE_BINDING_2D);
            let pixel_pack_buffer = gl.get_parameter_buffer(glow::PIXEL_PACK_BUFFER_BINDING);
            let pixel_unpack_buffer = gl.get_parameter_buffer(glow::PIXEL_UNPACK_BUFFER_BINDING);
            let pack_alignment = gl.get_parameter_i32(glow::PACK_ALIGNMENT);
            let unpack_alignment = gl.get_parameter_i32(glow::UNPACK_ALIGNMENT);
            let blend_enabled = gl.is_enabled(glow::BLEND);
            let scissor_enabled = gl.is_enabled(glow::SCISSOR_TEST);
            let depth_test_enabled = gl.is_enabled(glow::DEPTH_TEST);
            let stencil_test_enabled = gl.is_enabled(glow::STENCIL_TEST);
            let cull_face_enabled = gl.is_enabled(glow::CULL_FACE);
            let color_mask = gl.get_parameter_bool_array::<4>(glow::COLOR_WRITEMASK);
            let mut clear_color = [0.0; 4];
            gl.get_parameter_f32_slice(glow::COLOR_CLEAR_VALUE, &mut clear_color);

            // Establish deterministic draw/readback state for this pass.
            gl.disable(glow::BLEND);
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::STENCIL_TEST);
            gl.disable(glow::CULL_FACE);
            gl.color_mask(true, true, true, true);
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
            gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
            gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);

            Self {
                gl,
                draw_framebuffer,
                read_framebuffer,
                viewport,
                program,
                vertex_array,
                active_texture,
                active_texture_binding,
                texture0_binding,
                pixel_pack_buffer,
                pixel_unpack_buffer,
                pack_alignment,
                unpack_alignment,
                blend_enabled,
                scissor_enabled,
                depth_test_enabled,
                stencil_test_enabled,
                cull_face_enabled,
                color_mask,
                clear_color,
            }
        }
    }
}

impl Drop for ThumbnailGlState<'_> {
    fn drop(&mut self) {
        unsafe {
            self.gl
                .bind_framebuffer(glow::DRAW_FRAMEBUFFER, self.draw_framebuffer);
            self.gl
                .bind_framebuffer(glow::READ_FRAMEBUFFER, self.read_framebuffer);
            self.gl.viewport(
                self.viewport[0],
                self.viewport[1],
                self.viewport[2],
                self.viewport[3],
            );
            self.gl.use_program(self.program);
            self.gl.bind_vertex_array(self.vertex_array);

            self.gl.active_texture(glow::TEXTURE0);
            self.gl
                .bind_texture(glow::TEXTURE_2D, self.texture0_binding);
            if self.active_texture != glow::TEXTURE0 {
                self.gl.active_texture(self.active_texture);
                self.gl
                    .bind_texture(glow::TEXTURE_2D, self.active_texture_binding);
            }
            self.gl.active_texture(self.active_texture);

            self.gl
                .bind_buffer(glow::PIXEL_PACK_BUFFER, self.pixel_pack_buffer);
            self.gl
                .bind_buffer(glow::PIXEL_UNPACK_BUFFER, self.pixel_unpack_buffer);
            self.gl
                .pixel_store_i32(glow::PACK_ALIGNMENT, self.pack_alignment);
            self.gl
                .pixel_store_i32(glow::UNPACK_ALIGNMENT, self.unpack_alignment);
            self.gl.color_mask(
                self.color_mask[0],
                self.color_mask[1],
                self.color_mask[2],
                self.color_mask[3],
            );
            self.gl.clear_color(
                self.clear_color[0],
                self.clear_color[1],
                self.clear_color[2],
                self.clear_color[3],
            );
            if self.blend_enabled {
                self.gl.enable(glow::BLEND);
            } else {
                self.gl.disable(glow::BLEND);
            }
            if self.scissor_enabled {
                self.gl.enable(glow::SCISSOR_TEST);
            } else {
                self.gl.disable(glow::SCISSOR_TEST);
            }
            if self.depth_test_enabled {
                self.gl.enable(glow::DEPTH_TEST);
            } else {
                self.gl.disable(glow::DEPTH_TEST);
            }
            if self.stencil_test_enabled {
                self.gl.enable(glow::STENCIL_TEST);
            } else {
                self.gl.disable(glow::STENCIL_TEST);
            }
            if self.cull_face_enabled {
                self.gl.enable(glow::CULL_FACE);
            } else {
                self.gl.disable(glow::CULL_FACE);
            }
        }
    }
}

unsafe fn read_uniform_f32<const N: usize>(
    gl: &glow::Context,
    program: glow::Program,
    location: Option<&glow::UniformLocation>,
) -> Option<[f32; N]> {
    let location = location?;
    let mut value = [0.0; N];
    unsafe { gl.get_uniform_f32(program, location, &mut value) };
    Some(value)
}

unsafe fn read_uniform_i32(
    gl: &glow::Context,
    program: glow::Program,
    location: Option<&glow::UniformLocation>,
) -> Option<i32> {
    let location = location?;
    let mut value = [0];
    unsafe { gl.get_uniform_i32(program, location, &mut value) };
    Some(value[0])
}

/// `use_program` restoration alone does not restore values stored in the
/// shared window program object. Preserve them so the pass is hermetic even
/// when called between two draws that intentionally reuse uniform state.
struct ThumbnailWindowUniformState<'a> {
    gl: &'a glow::Context,
    program: glow::Program,
    uniforms: &'a WindowUniforms,
    projection: Option<[f32; 16]>,
    rect: Option<[f32; 4]>,
    texture: Option<i32>,
    opacity: Option<f32>,
    radius: Option<f32>,
    size: Option<[f32; 2]>,
    dim: Option<f32>,
    desat: Option<f32>,
    uv_rect: Option<[f32; 4]>,
    ripple_progress: Option<f32>,
    ripple_amplitude: Option<f32>,
}

impl<'a> ThumbnailWindowUniformState<'a> {
    unsafe fn capture(
        gl: &'a glow::Context,
        program: glow::Program,
        uniforms: &'a WindowUniforms,
    ) -> Self {
        unsafe {
            Self {
                gl,
                program,
                uniforms,
                projection: read_uniform_f32(gl, program, uniforms.projection.as_ref()),
                rect: read_uniform_f32(gl, program, uniforms.rect.as_ref()),
                texture: read_uniform_i32(gl, program, uniforms.texture.as_ref()),
                opacity: read_uniform_f32::<1>(gl, program, uniforms.opacity.as_ref())
                    .map(|value| value[0]),
                radius: read_uniform_f32::<1>(gl, program, uniforms.radius.as_ref())
                    .map(|value| value[0]),
                size: read_uniform_f32(gl, program, uniforms.size.as_ref()),
                dim: read_uniform_f32::<1>(gl, program, uniforms.dim.as_ref())
                    .map(|value| value[0]),
                desat: read_uniform_f32::<1>(gl, program, uniforms.desat.as_ref())
                    .map(|value| value[0]),
                uv_rect: read_uniform_f32(gl, program, uniforms.uv_rect.as_ref()),
                ripple_progress: read_uniform_f32::<1>(
                    gl,
                    program,
                    uniforms.ripple_progress.as_ref(),
                )
                .map(|value| value[0]),
                ripple_amplitude: read_uniform_f32::<1>(
                    gl,
                    program,
                    uniforms.ripple_amplitude.as_ref(),
                )
                .map(|value| value[0]),
            }
        }
    }
}

impl Drop for ThumbnailWindowUniformState<'_> {
    fn drop(&mut self) {
        unsafe {
            self.gl.use_program(Some(self.program));
            if let Some(value) = self.projection {
                self.gl.uniform_matrix_4_f32_slice(
                    self.uniforms.projection.as_ref(),
                    false,
                    &value,
                );
            }
            if let Some(value) = self.rect {
                self.gl.uniform_4_f32(
                    self.uniforms.rect.as_ref(),
                    value[0],
                    value[1],
                    value[2],
                    value[3],
                );
            }
            if let Some(value) = self.texture {
                self.gl.uniform_1_i32(self.uniforms.texture.as_ref(), value);
            }
            if let Some(value) = self.opacity {
                self.gl.uniform_1_f32(self.uniforms.opacity.as_ref(), value);
            }
            if let Some(value) = self.radius {
                self.gl.uniform_1_f32(self.uniforms.radius.as_ref(), value);
            }
            if let Some(value) = self.size {
                self.gl
                    .uniform_2_f32(self.uniforms.size.as_ref(), value[0], value[1]);
            }
            if let Some(value) = self.dim {
                self.gl.uniform_1_f32(self.uniforms.dim.as_ref(), value);
            }
            if let Some(value) = self.desat {
                self.gl.uniform_1_f32(self.uniforms.desat.as_ref(), value);
            }
            if let Some(value) = self.uv_rect {
                self.gl.uniform_4_f32(
                    self.uniforms.uv_rect.as_ref(),
                    value[0],
                    value[1],
                    value[2],
                    value[3],
                );
            }
            if let Some(value) = self.ripple_progress {
                self.gl
                    .uniform_1_f32(self.uniforms.ripple_progress.as_ref(), value);
            }
            if let Some(value) = self.ripple_amplitude {
                self.gl
                    .uniform_1_f32(self.uniforms.ripple_amplitude.as_ref(), value);
            }
        }
    }
}

struct ThumbnailGlResources<'a> {
    gl: &'a glow::Context,
    texture: Option<glow::Texture>,
    framebuffer: Option<glow::Framebuffer>,
}

impl Drop for ThumbnailGlResources<'_> {
    fn drop(&mut self) {
        unsafe {
            if let Some(framebuffer) = self.framebuffer.take() {
                self.gl.delete_framebuffer(framebuffer);
            }
            if let Some(texture) = self.texture.take() {
                self.gl.delete_texture(texture);
            }
        }
    }
}

/// Transfer a freshly allocated thumbnail texture to the resident cache only
/// after attaching it proves that `TexImage2D` produced usable storage.
///
/// OpenGL reports allocation failure (notably `GL_OUT_OF_MEMORY`) through
/// context state rather than the `TexImage2D` return value.  Keeping the
/// texture in the RAII owner on every incomplete status guarantees that the
/// caller cannot mistake a non-zero object name for drawable storage.
fn take_complete_thumbnail_texture<T>(
    texture: &mut Option<T>,
    framebuffer_status: u32,
) -> Option<T> {
    (framebuffer_status == glow::FRAMEBUFFER_COMPLETE)
        .then(|| texture.take())
        .flatten()
}

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
            let _state = ThumbnailGlState::begin(&self.gl);
            let mut resources = ThumbnailGlResources {
                gl: &self.gl,
                texture: None,
                framebuffer: None,
            };

            // Create an exact RGBA8 target for the CPU snapshot contract.
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
            resources.texture = Some(tex);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            self.gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                tw as i32,
                th as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
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
                    log::warn!(
                        "{}: {error}",
                        self.renderer_ctx("thumbnail: create framebuffer")
                    );
                })
                .ok()?;
            resources.framebuffer = Some(fbo);
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            self.gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(tex),
                0,
            );
            if self.gl.check_framebuffer_status(glow::FRAMEBUFFER) != glow::FRAMEBUFFER_COMPLETE {
                log::warn!(
                    "{}: incomplete framebuffer",
                    self.renderer_ctx("thumbnail: validate framebuffer")
                );
                return None;
            }

            self.gl.viewport(0, 0, tw as i32, th as i32);
            self.gl.clear_color(0.0, 0.0, 0.0, 0.0);
            self.gl.clear(glow::COLOR_BUFFER_BIT);

            let _uniform_state =
                ThumbnailWindowUniformState::capture(&self.gl, self.program, &self.win_uniforms);
            let proj = ortho(0.0, tw as f32, th as f32, 0.0, -1.0, 1.0);
            self.gl.use_program(Some(self.program));
            self.gl
                .uniform_matrix_4_f32_slice(self.win_uniforms.projection.as_ref(), false, &proj);
            self.gl.uniform_1_i32(self.win_uniforms.texture.as_ref(), 0);
            self.gl.uniform_1_f32(
                self.win_uniforms.opacity.as_ref(),
                snapshot_shader_opacity(wt.has_rgba),
            );
            self.gl
                .uniform_1_f32(self.win_uniforms.radius.as_ref(), 0.0);
            self.gl.uniform_1_f32(self.win_uniforms.dim.as_ref(), 1.0);
            self.gl.uniform_1_f32(self.win_uniforms.desat.as_ref(), 0.0);
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
            self.gl
                .uniform_1_f32(self.win_uniforms.ripple_progress.as_ref(), -1.0);
            self.gl
                .uniform_1_f32(self.win_uniforms.ripple_amplitude.as_ref(), 0.0);
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
            flip_rgba_vertical(&mut pixels, tw, th);

            Some((pixels, tw, th))
        }
    }

    /// Downsample one authoritative X11 texture into a bounded CPU snapshot
    /// and an independent RGBA8 GPU attachment in a single pass.
    ///
    /// The caller invokes this before releasing the live/animation pixmap
    /// owner. The returned texture has no XComposite binding and remains valid
    /// when the full-resolution source is later evicted.
    pub(super) fn capture_minimized_snapshot_from_texture(
        &self,
        source_texture: glow::Texture,
        source_width: u32,
        source_height: u32,
        has_alpha: bool,
        generation: crate::backend::compositor_common::minimized_thumbnail::SnapshotGeneration,
    ) -> Option<CapturedMinimizedSnapshot> {
        let (width, height) =
            crate::backend::compositor_common::minimized_thumbnail::snapshot_dimensions(
                source_width,
                source_height,
            )?;

        unsafe {
            let _state = ThumbnailGlState::begin(&self.gl);
            let mut resources = ThumbnailGlResources {
                gl: &self.gl,
                texture: None,
                framebuffer: None,
            };
            let texture = self
                .gl
                .create_texture()
                .map_err(|error| {
                    log::warn!(
                        "{}: {error}",
                        self.renderer_ctx("minimized thumbnail: create texture")
                    );
                })
                .ok()?;
            resources.texture = Some(texture);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            self.gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                width as i32,
                height as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None),
            );
            for parameter in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
                self.gl
                    .tex_parameter_i32(glow::TEXTURE_2D, parameter, glow::LINEAR as i32);
            }
            for parameter in [glow::TEXTURE_WRAP_S, glow::TEXTURE_WRAP_T] {
                self.gl
                    .tex_parameter_i32(glow::TEXTURE_2D, parameter, glow::CLAMP_TO_EDGE as i32);
            }

            let framebuffer = self
                .gl
                .create_framebuffer()
                .map_err(|error| {
                    log::warn!(
                        "{}: {error}",
                        self.renderer_ctx("minimized thumbnail: create framebuffer")
                    );
                })
                .ok()?;
            resources.framebuffer = Some(framebuffer);
            self.gl
                .bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
            self.gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(texture),
                0,
            );
            if self.gl.check_framebuffer_status(glow::FRAMEBUFFER) != glow::FRAMEBUFFER_COMPLETE {
                log::warn!(
                    "{}: incomplete framebuffer",
                    self.renderer_ctx("minimized thumbnail: validate framebuffer")
                );
                return None;
            }

            self.gl.viewport(0, 0, width as i32, height as i32);
            self.gl.clear_color(0.0, 0.0, 0.0, 0.0);
            self.gl.clear(glow::COLOR_BUFFER_BIT);
            self.gl.use_program(Some(self.thumbnail_downsample_program));
            self.gl
                .uniform_1_i32(self.thumbnail_downsample_uniforms.texture.as_ref(), 0);
            self.gl.uniform_4_f32(
                self.thumbnail_downsample_uniforms.uv_rect.as_ref(),
                0.0,
                0.0,
                1.0,
                1.0,
            );
            self.gl.uniform_2_f32(
                self.thumbnail_downsample_uniforms.output_size.as_ref(),
                width as f32,
                height as f32,
            );
            self.gl.uniform_1_i32(
                self.thumbnail_downsample_uniforms.has_alpha.as_ref(),
                i32::from(has_alpha),
            );
            self.gl.bind_vertex_array(Some(self.quad_vao));
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(source_texture));
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

            let mut rgba = vec![0; width as usize * height as usize * 4];
            self.gl.read_pixels(
                0,
                0,
                width as i32,
                height as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut rgba)),
            );
            flip_rgba_vertical(&mut rgba, width, height);
            let cpu =
                crate::backend::compositor_common::minimized_thumbnail::MinimizedSnapshot::try_new(
                    width,
                    height,
                    generation.get(),
                    has_alpha,
                    rgba,
                )
                .ok()?;

            // The FBO itself is temporary; transfer only its independent color
            // attachment to the minimized GPU cache.
            let texture = resources
                .texture
                .take()
                .expect("completed minimized capture owns its texture");
            Some(CapturedMinimizedSnapshot {
                cpu,
                gpu: MinimizedGpuSnapshot {
                    texture,
                    width,
                    height,
                    has_alpha,
                    generation,
                    storage: SnapshotTextureStorage::FramebufferAttachment,
                    last_use: 0,
                },
            })
        }
    }

    /// Recreate an evicted low-resolution GPU texture from the durable
    /// top-left CPU copy. No vertical row rewrite is needed: the compositor's
    /// ordinary top-down quad convention samples row zero correctly.
    pub(super) fn upload_minimized_snapshot_texture(
        &self,
        snapshot: &crate::backend::compositor_common::minimized_thumbnail::MinimizedSnapshot,
    ) -> Option<MinimizedGpuSnapshot> {
        unsafe {
            let _state = ThumbnailGlState::begin(&self.gl);
            let mut resources = ThumbnailGlResources {
                gl: &self.gl,
                texture: None,
                framebuffer: None,
            };
            let texture = self
                .gl
                .create_texture()
                .map_err(|error| {
                    log::warn!(
                        "{}: {error}",
                        self.renderer_ctx("minimized thumbnail: upload texture")
                    );
                })
                .ok()?;
            resources.texture = Some(texture);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            self.gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                snapshot.width() as i32,
                snapshot.height() as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(snapshot.rgba().as_ref())),
            );
            for parameter in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
                self.gl
                    .tex_parameter_i32(glow::TEXTURE_2D, parameter, glow::LINEAR as i32);
            }
            for parameter in [glow::TEXTURE_WRAP_S, glow::TEXTURE_WRAP_T] {
                self.gl
                    .tex_parameter_i32(glow::TEXTURE_2D, parameter, glow::CLAMP_TO_EDGE as i32);
            }

            // A non-zero texture object does not prove that TexImage2D
            // allocated storage: OOM is reported through GL state.  Attach the
            // fresh image to a temporary framebuffer so an allocation failure
            // is rejected instead of becoming a permanently resident blank
            // Dock source (and direct-scanout blocker).
            let framebuffer = self
                .gl
                .create_framebuffer()
                .map_err(|error| {
                    log::warn!(
                        "{}: {error}",
                        self.renderer_ctx("minimized thumbnail: validate uploaded texture")
                    );
                })
                .ok()?;
            resources.framebuffer = Some(framebuffer);
            self.gl
                .bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
            self.gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(texture),
                0,
            );
            let framebuffer_status = self.gl.check_framebuffer_status(glow::FRAMEBUFFER);
            let Some(texture) =
                take_complete_thumbnail_texture(&mut resources.texture, framebuffer_status)
            else {
                log::warn!(
                    "{}: incomplete framebuffer (status=0x{framebuffer_status:x})",
                    self.renderer_ctx("minimized thumbnail: validate uploaded texture")
                );
                return None;
            };
            Some(MinimizedGpuSnapshot {
                texture,
                width: snapshot.width(),
                height: snapshot.height(),
                has_alpha: snapshot.has_alpha(),
                generation: snapshot.generation(),
                storage: SnapshotTextureStorage::CpuTopLeftUpload,
                last_use: 0,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::take_complete_thumbnail_texture;

    #[test]
    fn incomplete_cpu_upload_storage_never_transfers_to_the_resident_cache() {
        let mut incomplete = Some("RAII-owned texture");
        assert_eq!(
            take_complete_thumbnail_texture(
                &mut incomplete,
                glow::FRAMEBUFFER_INCOMPLETE_ATTACHMENT
            ),
            None
        );
        assert_eq!(incomplete, Some("RAII-owned texture"));

        let mut complete = Some("resident texture");
        assert_eq!(
            take_complete_thumbnail_texture(&mut complete, glow::FRAMEBUFFER_COMPLETE),
            Some("resident texture")
        );
        assert_eq!(complete, None);
    }
}
