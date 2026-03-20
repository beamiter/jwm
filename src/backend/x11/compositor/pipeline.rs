use super::*;

impl Compositor {
    /// Create the dual Kawase blur FBO mipmap chain.
    /// Each level is half the size of the previous.
    pub(super) unsafe fn create_blur_fbos(gl: &glow::Context, w: u32, h: u32, levels: u32) -> Vec<BlurFboLevel> {
        let levels = levels.clamp(1, 6);
        let mut fbos = Vec::new();
        let mut cur_w = w / 2;
        let mut cur_h = h / 2;
        unsafe {
            for _ in 0..levels {
                if cur_w == 0 { cur_w = 1; }
                if cur_h == 0 { cur_h = 1; }
                let tex = match gl.create_texture() {
                    Ok(t) => t,
                    Err(_) => break,
                };
                gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                gl.tex_image_2d(
                    glow::TEXTURE_2D, 0, glow::RGBA8 as i32,
                    cur_w as i32, cur_h as i32, 0,
                    glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(None),
                );
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);

                let fbo = match gl.create_framebuffer() {
                    Ok(f) => f,
                    Err(_) => {
                        gl.delete_texture(tex);
                        break;
                    }
                };
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
                gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(tex), 0);

                fbos.push(BlurFboLevel { fbo, texture: tex, w: cur_w, h: cur_h });
                cur_w /= 2;
                cur_h /= 2;
            }
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.bind_texture(glow::TEXTURE_2D, None);
        }
        log::info!("compositor: created {} blur FBO levels", fbos.len());
        fbos
    }

    /// Create the scene capture FBO used as blur source.
    pub(super) unsafe fn create_scene_fbo(gl: &glow::Context, w: u32, h: u32) -> Result<(glow::Framebuffer, glow::Texture), String> {
        unsafe {
            let tex = gl.create_texture().map_err(|e| format!("scene_fbo tex: {e}"))?;
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.tex_image_2d(
                glow::TEXTURE_2D, 0, glow::RGBA8 as i32,
                w as i32, h as i32, 0,
                glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(None),
            );
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);

            let fbo = gl.create_framebuffer().map_err(|e| format!("scene_fbo: {e}"))?;
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(tex), 0);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.bind_texture(glow::TEXTURE_2D, None);
            Ok((fbo, tex))
        }
    }

    /// Capture the current framebuffer into scene_fbo, then run dual Kawase blur passes.
    /// `source_fbo` specifies which FBO to read from (None = default back buffer).
    pub(super) fn run_blur_passes_from_fbo(&self, source_fbo: Option<glow::Framebuffer>, max_levels: usize) -> Option<glow::Texture> {
        let (scene_fbo, scene_tex) = self.scene_fbo.as_ref()?;
        if self.blur_fbos.is_empty() {
            return None;
        }
        let levels = max_levels.min(self.blur_fbos.len()).max(1);

        unsafe {
            // Copy current framebuffer to scene FBO
            self.gl.bind_framebuffer(glow::READ_FRAMEBUFFER, source_fbo);
            self.gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(*scene_fbo));
            self.gl.blit_framebuffer(
                0, 0, self.screen_w as i32, self.screen_h as i32,
                0, 0, self.screen_w as i32, self.screen_h as i32,
                glow::COLOR_BUFFER_BIT,
                glow::LINEAR,
            );

            // === Downsample passes ===
            self.gl.use_program(Some(self.blur_down_program));
            self.gl.uniform_1_i32(self.blur_down_uniforms.texture.as_ref(), 0);
            self.gl.bind_vertex_array(Some(self.quad_vao));

            let mut src_tex = *scene_tex;
            let mut src_w = self.screen_w;
            let mut src_h = self.screen_h;

            for level in &self.blur_fbos[..levels] {
                self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(level.fbo));
                self.gl.viewport(0, 0, level.w as i32, level.h as i32);

                let hp_proj = ortho(0.0, level.w as f32, level.h as f32, 0.0, -1.0, 1.0);
                self.gl.uniform_matrix_4_f32_slice(
                    self.blur_down_uniforms.projection.as_ref(), false, &hp_proj,
                );
                self.gl.uniform_4_f32(
                    self.blur_down_uniforms.rect.as_ref(),
                    0.0, 0.0, level.w as f32, level.h as f32,
                );
                self.gl.uniform_2_f32(
                    self.blur_down_uniforms.halfpixel.as_ref(),
                    0.5 / src_w as f32, 0.5 / src_h as f32,
                );

                self.gl.active_texture(glow::TEXTURE0);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(src_tex));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                src_tex = level.texture;
                src_w = level.w;
                src_h = level.h;
            }

            // === Upsample passes ===
            self.gl.use_program(Some(self.blur_up_program));
            self.gl.uniform_1_i32(self.blur_up_uniforms.texture.as_ref(), 0);

            // Upsample from smallest to largest (reverse order, stopping before the last)
            for i in (0..levels - 1).rev() {
                let target = &self.blur_fbos[i];
                let source_tex = if i + 1 < self.blur_fbos.len() {
                    self.blur_fbos[i + 1].texture
                } else {
                    src_tex
                };

                self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(target.fbo));
                self.gl.viewport(0, 0, target.w as i32, target.h as i32);

                let hp_proj = ortho(0.0, target.w as f32, target.h as f32, 0.0, -1.0, 1.0);
                self.gl.uniform_matrix_4_f32_slice(
                    self.blur_up_uniforms.projection.as_ref(), false, &hp_proj,
                );
                self.gl.uniform_4_f32(
                    self.blur_up_uniforms.rect.as_ref(),
                    0.0, 0.0, target.w as f32, target.h as f32,
                );

                let src_level = &self.blur_fbos[i + 1];
                self.gl.uniform_2_f32(
                    self.blur_up_uniforms.halfpixel.as_ref(),
                    0.5 / src_level.w as f32, 0.5 / src_level.h as f32,
                );

                self.gl.active_texture(glow::TEXTURE0);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(source_tex));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            }

            // Bind back to default framebuffer
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.gl.viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
        }

        // Return the first (largest) blur level texture as the blurred result
        Some(self.blur_fbos[0].texture)
    }
}
