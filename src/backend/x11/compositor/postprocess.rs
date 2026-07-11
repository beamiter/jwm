use super::math::ortho;
use super::{Compositor, SlimeWaveSimulation};
use glow::HasContext;

use super::CompositorConnection;
use crate::backend::compositor_common::capture::{clip_region, flip_rgba_vertical};
use crate::backend::compositor_common::screenshot::save_png_async;

impl<C: CompositorConnection> Compositor<C> {
    unsafe fn create_slime_wave_target(
        gl: &glow::Context,
        width: u32,
        height: u32,
    ) -> Result<(glow::Framebuffer, glow::Texture), String> {
        unsafe {
            let texture = gl.create_texture().map_err(|err| err.to_string())?;
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            const GL_RG16F: u32 = 0x822f;
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                GL_RG16F as i32,
                width as i32,
                height as i32,
                0,
                glow::RG,
                glow::HALF_FLOAT,
                glow::PixelUnpackData::Slice(None),
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::LINEAR as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::LINEAR as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_S,
                glow::CLAMP_TO_EDGE as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_T,
                glow::CLAMP_TO_EDGE as i32,
            );
            let fbo = gl.create_framebuffer().map_err(|err| err.to_string())?;
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(texture),
                0,
            );
            if gl.check_framebuffer_status(glow::FRAMEBUFFER) != glow::FRAMEBUFFER_COMPLETE {
                gl.delete_framebuffer(fbo);
                gl.delete_texture(texture);
                return Err("slime wave RG16F framebuffer is incomplete".to_string());
            }
            gl.clear_color(0.0, 0.0, 0.0, 0.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            Ok((fbo, texture))
        }
    }

    fn ensure_slime_wave_simulation(&mut self) -> bool {
        if self.slime_wave_simulation.is_some() {
            return true;
        }
        let width = (self.screen_w / 2).max(1);
        let height = (self.screen_h / 2).max(1);
        let created = unsafe {
            let first = Self::create_slime_wave_target(&self.gl, width, height);
            let second = Self::create_slime_wave_target(&self.gl, width, height);
            match (first, second) {
                (Ok((fbo_a, tex_a)), Ok((fbo_b, tex_b))) => Some(SlimeWaveSimulation {
                    fbos: [fbo_a, fbo_b],
                    textures: [tex_a, tex_b],
                    front: 0,
                    width,
                    height,
                    last_tick: std::time::Instant::now(),
                    accumulated_time: 1.0 / 120.0,
                }),
                (Ok((fbo, texture)), Err(err)) | (Err(err), Ok((fbo, texture))) => {
                    self.gl.delete_framebuffer(fbo);
                    self.gl.delete_texture(texture);
                    log::warn!("compositor: slime wave target creation failed: {err}");
                    None
                }
                (Err(first), Err(second)) => {
                    log::warn!("compositor: slime wave targets unavailable: {first}; {second}");
                    None
                }
            }
        };
        self.slime_wave_simulation = created;
        self.slime_wave_simulation.is_some()
    }

    pub(super) fn run_slime_wave_simulation(&mut self) {
        if !self.ensure_slime_wave_simulation() {
            return;
        }
        let (segments, injection_params, injection_count) = self.slime_state.take_wave_injections();
        let mut simulation = self.slime_wave_simulation.take().unwrap();
        unsafe {
            self.gl.use_program(Some(self.slime_wave_program));
            let projection = ortho(
                0.0,
                simulation.width as f32,
                simulation.height as f32,
                0.0,
                -1.0,
                1.0,
            );
            self.gl.uniform_matrix_4_f32_slice(
                self.slime_wave_uniforms.projection.as_ref(),
                false,
                &projection,
            );
            self.gl.uniform_4_f32(
                self.slime_wave_uniforms.rect.as_ref(),
                0.0,
                0.0,
                simulation.width as f32,
                simulation.height as f32,
            );
            self.gl
                .uniform_1_i32(self.slime_wave_uniforms.state.as_ref(), 0);
            self.gl.uniform_2_f32(
                self.slime_wave_uniforms.texel.as_ref(),
                1.0 / simulation.width as f32,
                1.0 / simulation.height as f32,
            );
            self.gl.uniform_1_f32(
                self.slime_wave_uniforms.aspect.as_ref(),
                simulation.width as f32 / simulation.height as f32,
            );
            self.gl.bind_vertex_array(Some(self.quad_vao));
            self.gl
                .viewport(0, 0, simulation.width as i32, simulation.height as i32);
            const FIXED_STEP: f32 = 1.0 / 120.0;
            let now = std::time::Instant::now();
            simulation.accumulated_time += now
                .duration_since(simulation.last_tick)
                .as_secs_f32()
                .min(0.05);
            simulation.last_tick = now;
            if injection_count > 0 {
                simulation.accumulated_time = simulation.accumulated_time.max(FIXED_STEP);
            }
            let steps = (simulation.accumulated_time / FIXED_STEP).floor().min(6.0) as usize;
            simulation.accumulated_time -= steps as f32 * FIXED_STEP;
            self.gl
                .uniform_1_f32(self.slime_wave_uniforms.time_step.as_ref(), FIXED_STEP);
            for step in 0..steps {
                let back = 1 - simulation.front;
                self.gl
                    .bind_framebuffer(glow::FRAMEBUFFER, Some(simulation.fbos[back]));
                self.gl.active_texture(glow::TEXTURE0);
                self.gl.bind_texture(
                    glow::TEXTURE_2D,
                    Some(simulation.textures[simulation.front]),
                );
                let count = if step == 0 { injection_count } else { 0 };
                self.gl
                    .uniform_1_i32(self.slime_wave_uniforms.injection_count.as_ref(), count);
                if count > 0 {
                    self.gl.uniform_4_f32_slice(
                        self.slime_wave_uniforms.injections.as_ref(),
                        &segments[..count as usize * 4],
                    );
                    self.gl.uniform_2_f32_slice(
                        self.slime_wave_uniforms.injection_params.as_ref(),
                        &injection_params[..count as usize * 2],
                    );
                }
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                simulation.front = back;
            }
            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.gl
                .viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
        }
        self.slime_wave_simulation = Some(simulation);
    }

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
            || self.slime_state.is_visible()
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
        save_png_async(path.to_path_buf(), pixels, w, h);
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
            log::warn!("compositor: screenshot region is empty");
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
        save_png_async(path.to_path_buf(), pixels, w, h);
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
            let tex = self.gl.create_texture().ok()?;
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
            let fbo = self.gl.create_framebuffer().ok()?;
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
