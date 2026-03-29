use glow::HasContext;
use super::Compositor;
use super::math::{perspective_matrix, translate_matrix, rotate_y_matrix, scale_matrix, mat4_mul};

impl Compositor {
    pub(super) fn clear_overview_snapshots(&mut self) {
        unsafe {
            for entry in &mut self.overview_windows {
                if let Some(texture) = entry.snapshot_texture.take() {
                    self.gl.delete_texture(texture);
                }
            }
        }
    }

    pub(super) fn clear_overview_title_textures(&mut self) {
        unsafe {
            for entry in &mut self.overview_windows {
                if let Some((texture, _, _)) = entry.title_texture.take() {
                    self.gl.delete_texture(texture);
                }
            }
        }
    }

    /// Render a title string into an RGBA pixel buffer using a simple embedded bitmap font.
    /// Returns (pixels, width, height) or None if the title is empty.
    pub(super) fn render_title_to_pixels(title: &str, max_width: u32) -> Option<(Vec<u8>, u32, u32)> {
        if title.is_empty() {
            return None;
        }

        use super::font::{FONT_6X10, GLYPH_W, GLYPH_H};
        const SCALE: u32 = 2; // render at 2x for readability
        const CHAR_W: u32 = GLYPH_W * SCALE;
        const CHAR_H: u32 = GLYPH_H * SCALE;
        const PAD_X: u32 = 8;  // horizontal padding
        const PAD_Y: u32 = 4;  // vertical padding

        // Truncate to fit max_width
        let max_chars = ((max_width.saturating_sub(PAD_X * 2)) / CHAR_W) as usize;
        if max_chars == 0 {
            return None;
        }

        let display_title: String = title
            .chars()
            .take(max_chars)
            .map(|c| if c.is_ascii_graphic() || c == ' ' { c } else { '?' })
            .collect();

        let text_w = display_title.len() as u32 * CHAR_W;
        let img_w = text_w + PAD_X * 2;
        let img_h = CHAR_H + PAD_Y * 2;
        let mut pixels = vec![0u8; (img_w * img_h * 4) as usize];

        // Draw semi-transparent dark background (rounded pill shape)
        for py in 0..img_h {
            for px in 0..img_w {
                let idx = ((py * img_w + px) * 4) as usize;
                // Simple rounded rect: check if inside pill shape
                let radius = (img_h / 2) as f32;
                let cx = px as f32;
                let cy = py as f32;
                let inside = if cx < radius {
                    let dx = radius - cx;
                    let dy = cy - radius;
                    dx * dx + dy * dy <= radius * radius
                } else if cx > (img_w as f32 - radius) {
                    let dx = cx - (img_w as f32 - radius);
                    let dy = cy - radius;
                    dx * dx + dy * dy <= radius * radius
                } else {
                    true
                };
                if inside {
                    pixels[idx] = 15;     // R
                    pixels[idx + 1] = 15; // G
                    pixels[idx + 2] = 20; // B
                    pixels[idx + 3] = 200; // A (semi-transparent dark)
                }
            }
        }

        // Draw text glyphs
        for (ci, ch) in display_title.chars().enumerate() {
            let glyph_idx = if (32..=126).contains(&(ch as u32)) {
                (ch as u32 - 32) as usize
            } else {
                ('?' as u32 - 32) as usize
            };
            let glyph = &FONT_6X10[glyph_idx * 10..(glyph_idx + 1) * 10];

            let base_x = PAD_X + ci as u32 * CHAR_W;
            let base_y = PAD_Y;

            for row in 0..GLYPH_H {
                let bits = glyph[row as usize];
                for col in 0..GLYPH_W {
                    if bits & (0x80 >> col) != 0 {
                        // Draw scaled pixel
                        for sy in 0..SCALE {
                            for sx in 0..SCALE {
                                let px = base_x + col * SCALE + sx;
                                let py = base_y + row * SCALE + sy;
                                if px < img_w && py < img_h {
                                    let idx = ((py * img_w + px) * 4) as usize;
                                    pixels[idx] = 240;     // R
                                    pixels[idx + 1] = 240; // G
                                    pixels[idx + 2] = 245; // B
                                    pixels[idx + 3] = 255; // A
                                }
                            }
                        }
                    }
                }
            }
        }

        Some((pixels, img_w, img_h))
    }

    pub(super) fn create_overview_title_textures(&mut self) {
        let entries: Vec<(String, f32)> = self
            .overview_windows
            .iter()
            .map(|e| (e.title.clone(), e.target_w))
            .collect();

        let textures: Vec<Option<(glow::Texture, u32, u32)>> = entries
            .iter()
            .map(|(title, target_w)| {
                let max_w = (*target_w as u32).max(120);
                let (pixels, w, h) = Self::render_title_to_pixels(title, max_w)?;
                unsafe {
                    let tex = self.gl.create_texture().ok()?;
                    self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                    self.gl.tex_image_2d(
                        glow::TEXTURE_2D, 0, glow::RGBA8 as i32,
                        w as i32, h as i32, 0,
                        glow::RGBA, glow::UNSIGNED_BYTE,
                        glow::PixelUnpackData::Slice(Some(&pixels)),
                    );
                    self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
                    self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
                    self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
                    self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
                    self.gl.bind_texture(glow::TEXTURE_2D, None);
                    Some((tex, w, h))
                }
            })
            .collect();

        for (entry, title_tex) in self.overview_windows.iter_mut().zip(textures.into_iter()) {
            entry.title_texture = title_tex;
        }
    }

    pub(super) fn upload_overview_snapshot_texture(
        &self,
        pixels: &[u8],
        width: u32,
        height: u32,
    ) -> Option<glow::Texture> {
        unsafe {
            let texture = self.gl.create_texture().ok()?;
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
                glow::PixelUnpackData::Slice(Some(pixels)),
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
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_S,
                glow::CLAMP_TO_EDGE as i32,
            );
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_T,
                glow::CLAMP_TO_EDGE as i32,
            );
            self.gl.bind_texture(glow::TEXTURE_2D, None);
            Some(texture)
        }
    }

    pub(super) fn create_overview_snapshot_texture(
        &self,
        x11_win: u32,
        max_size: u32,
    ) -> Option<glow::Texture> {
        let (pixels, width, height) = self.capture_window_thumbnail(x11_win, max_size)?;
        let row_bytes = (width * 4) as usize;
        let mut flipped = vec![0u8; pixels.len()];
        for y in 0..height as usize {
            let src_row = (height as usize - 1 - y) * row_bytes;
            let dst_row = y * row_bytes;
            flipped[dst_row..dst_row + row_bytes]
                .copy_from_slice(&pixels[src_row..src_row + row_bytes]);
        }
        self.upload_overview_snapshot_texture(&flipped, width, height)
    }

    pub(super) fn refresh_overview_snapshots(&mut self) {
        self.clear_overview_snapshots();

        let requests: Vec<(u32, u32)> = self
            .overview_windows
            .iter()
            .map(|entry| {
                let desired = (entry.target_w.max(entry.target_h) * 2.0).ceil() as u32;
                let max_size = desired.clamp(256, 1024);
                (entry.x11_win, max_size)
            })
            .collect();

        let snapshots: Vec<Option<glow::Texture>> = requests
            .into_iter()
            .map(|(x11_win, max_size)| self.create_overview_snapshot_texture(x11_win, max_size))
            .collect();

        for (entry, snapshot_texture) in self.overview_windows.iter_mut().zip(snapshots.into_iter()) {
            entry.snapshot_texture = snapshot_texture;
        }
    }

    /// Tick the overview prism rotation animation (exponential ease-out).
    pub(super) fn tick_overview_prism(&mut self) {
        let now = std::time::Instant::now();
        let dt = if let Some(last) = self.overview_prism_last_tick {
            now.duration_since(last).as_secs_f32().min(0.1)
        } else {
            1.0 / 60.0
        };
        self.overview_prism_last_tick = Some(now);

        // Prism rotation animation
        let diff = self.overview_prism_target_angle - self.overview_prism_current_angle;
        if diff.abs() < 0.001 {
            self.overview_prism_current_angle = self.overview_prism_target_angle;
        } else {
            let t = 1.0 - (-20.0_f32 * dt).exp();
            self.overview_prism_current_angle += diff * t;
            self.needs_render = true;
        }

        // Entry animation (scale + fade in)
        if self.overview_entry_progress < 1.0 {
            let t = 1.0 - (-10.0_f32 * dt).exp();
            self.overview_entry_progress += (1.0 - self.overview_entry_progress) * t;
            if (1.0 - self.overview_entry_progress).abs() < 0.002 {
                self.overview_entry_progress = 1.0;
            }
            self.overview_opacity = self.overview_entry_progress;
            self.needs_render = true;
        }

        // Exit animation (scale + fade out)
        if self.overview_closing {
            let t = 1.0 - (-12.0_f32 * dt).exp();
            self.overview_exit_progress -= self.overview_exit_progress * t;
            self.overview_opacity = self.overview_exit_progress;
            if self.overview_exit_progress < 0.01 {
                // Animation complete: actually deactivate
                self.overview_active = false;
                self.overview_closing = false;
                self.overview_exit_progress = 1.0;
                self.overview_opacity = 0.0;
                self.clear_overview_snapshots();
                self.clear_overview_title_textures();
                self.overview_windows.clear();
            }
            self.needs_render = true;
        }
    }

    /// Project a point in model space through the MVP matrix to screen coordinates.
    fn project_to_screen(mvp: &[f32; 16], model_pt: [f32; 3], vp_w: f32, vp_h: f32, vp_x: f32, vp_y: f32) -> (f32, f32) {
        let [mx, my, mz] = model_pt;
        let clip_x = mvp[0]*mx + mvp[4]*my + mvp[8]*mz  + mvp[12];
        let clip_y = mvp[1]*mx + mvp[5]*my + mvp[9]*mz  + mvp[13];
        let clip_w = mvp[3]*mx + mvp[7]*my + mvp[11]*mz + mvp[15];
        let ndc_x = clip_x / clip_w;
        let ndc_y = clip_y / clip_w;
        let sx = (ndc_x * 0.5 + 0.5) * vp_w + vp_x;
        let sy = (1.0 - (ndc_y * 0.5 + 0.5)) * vp_h + vp_y;
        (sx, sy)
    }

    /// Render overview overlay (Alt-Tab preview) as a 3D hexagonal prism carousel.
    /// Rendering is confined to the monitor that owns the overview.
    pub(super) fn render_overview(&self, proj: &[f32; 16], _focused: Option<u32>) {
        if self.overview_windows.is_empty() { return; }

        let mon_x = self.overview_mon_x;
        let mon_y = self.overview_mon_y;
        let mon_w = self.overview_mon_w;
        let mon_h = self.overview_mon_h;
        let mw = mon_w as f32;
        let mh = mon_h as f32;

        // Combined scale for entry/exit animation
        let anim_scale = self.overview_entry_progress * self.overview_exit_progress;

        unsafe {
            // === 1. Dark background overlay ===
            self.gl.use_program(Some(self.overview_bg_program));
            self.gl.uniform_matrix_4_f32_slice(
                self.overview_bg_uniforms.projection.as_ref(), false, proj,
            );
            self.gl.uniform_4_f32(
                self.overview_bg_uniforms.rect.as_ref(),
                mon_x as f32, mon_y as f32, mw, mh,
            );
            self.gl.uniform_1_f32(self.overview_bg_uniforms.opacity.as_ref(), self.overview_opacity);
            self.gl.bind_vertex_array(Some(self.quad_vao));
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

            // === 2. Scissor + viewport ===
            let scissor_gl_y = self.screen_h as i32 - (mon_y + mon_h as i32);
            self.gl.enable(glow::SCISSOR_TEST);
            self.gl.scissor(mon_x, scissor_gl_y, mon_w as i32, mon_h as i32);
            self.gl.viewport(mon_x, scissor_gl_y, mon_w as i32, mon_h as i32);

            // === 3. Hexagonal prism geometry ===
            let face_w = mw * 0.8;
            let face_h = mh * 0.8;
            let face_aspect = face_w / face_h;
            let apothem = face_aspect * 3.0_f32.sqrt();

            let fov_y = std::f32::consts::FRAC_PI_4;
            let camera_z = (apothem + 1.0 / (fov_y * 0.5).tan()) * 1.2;
            let mon_aspect = mw / mh;

            let persp = perspective_matrix(fov_y, mon_aspect, 0.1, camera_z * 4.0);
            let view = translate_matrix(0.0, 0.0, -camera_z);
            let global_rot = rotate_y_matrix(self.overview_prism_current_angle);

            // Scale matrix for entry/exit animation
            let scale_mat = scale_matrix(anim_scale, anim_scale, anim_scale);

            // === 4. Build per-face draw info ===
            struct FaceDrawInfo {
                mvp: [f32; 16],
                z_depth: f32,
                brightness: f32,
                entry_idx: usize,
            }

            let pi_over_3 = std::f32::consts::FRAC_PI_3;
            let mut faces: Vec<FaceDrawInfo> = Vec::new();

            for (idx, entry) in self.overview_windows.iter().enumerate() {
                let face_i = entry.face_index;
                let face_angle = face_i as f32 * pi_over_3;

                let face_rot = rotate_y_matrix(face_angle);
                let face_translate = translate_matrix(0.0, 0.0, apothem);
                let face_model = mat4_mul(&face_rot, &face_translate);

                // Apply animation scale before global rotation
                let model = mat4_mul(&scale_mat, &face_model);
                let model = mat4_mul(&global_rot, &model);
                let mv = mat4_mul(&view, &model);
                let mvp = mat4_mul(&persp, &mv);

                let z_depth = mv[14];

                let total_angle = face_angle + self.overview_prism_current_angle;
                let cos_facing = total_angle.cos();
                let brightness = if entry.is_selected {
                    (0.50 + 0.50 * cos_facing.max(0.0)) * anim_scale
                } else {
                    (0.25 + 0.30 * cos_facing.max(0.0)) * anim_scale
                };

                faces.push(FaceDrawInfo {
                    mvp,
                    z_depth,
                    brightness,
                    entry_idx: idx,
                });
            }

            // === 5. Painter's algorithm ===
            faces.sort_by(|a, b| a.z_depth.partial_cmp(&b.z_depth).unwrap_or(std::cmp::Ordering::Equal));

            // === 6. Draw faces ===
            self.gl.use_program(Some(self.cube_program));
            self.gl.uniform_1_f32(self.cube_uniforms.aspect.as_ref(), face_aspect);
            self.gl.uniform_1_i32(self.cube_uniforms.texture.as_ref(), 0);
            self.gl.uniform_4_f32(
                self.cube_uniforms.uv_rect.as_ref(),
                0.0, 1.0, 1.0, -1.0,
            );
            self.gl.active_texture(glow::TEXTURE0);

            for face in &faces {
                let entry = &self.overview_windows[face.entry_idx];
                if face.brightness < 0.05 { continue; }

                let texture = if let Some(tex) = entry.snapshot_texture {
                    tex
                } else {
                    match self.windows.get(&entry.x11_win) {
                        Some(wt) => wt.gl_texture,
                        None => continue,
                    }
                };

                self.gl.uniform_matrix_4_f32_slice(
                    self.cube_uniforms.mvp.as_ref(), false, &face.mvp,
                );
                self.gl.uniform_1_f32(
                    self.cube_uniforms.brightness.as_ref(), face.brightness,
                );
                self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                self.gl.tex_parameter_i32(
                    glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32,
                );
                self.gl.tex_parameter_i32(
                    glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32,
                );
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                self.gl.tex_parameter_i32(
                    glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::NEAREST as i32,
                );
                self.gl.tex_parameter_i32(
                    glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::NEAREST as i32,
                );
            }

            // === 7. Restore viewport for 2D overlays (titles, border) ===
            self.gl.disable(glow::SCISSOR_TEST);
            self.gl.viewport(0, 0, self.screen_w as i32, self.screen_h as i32);

            // === 8. Draw title labels and selection border ===
            let vp_x = mon_x as f32;
            let vp_y = mon_y as f32;

            // Draw selection border on the selected face
            for face in faces.iter().rev() {
                let entry = &self.overview_windows[face.entry_idx];
                if !entry.is_selected || face.brightness < 0.05 { continue; }

                // Project four corners to screen space
                let corners = [
                    [-face_aspect, -1.0, 0.0],
                    [ face_aspect, -1.0, 0.0],
                    [-face_aspect,  1.0, 0.0],
                    [ face_aspect,  1.0, 0.0],
                ];
                let mut min_x = f32::MAX;
                let mut min_y = f32::MAX;
                let mut max_x = f32::MIN;
                let mut max_y = f32::MIN;
                for c in &corners {
                    let (sx, sy) = Self::project_to_screen(&face.mvp, *c, mw, mh, vp_x, vp_y);
                    min_x = min_x.min(sx);
                    min_y = min_y.min(sy);
                    max_x = max_x.max(sx);
                    max_y = max_y.max(sy);
                }
                let bw = 3.0;
                let pad = bw + 2.0;
                let rx = min_x - pad;
                let ry = min_y - pad;
                let rw = max_x - min_x + pad * 2.0;
                let rh = max_y - min_y + pad * 2.0;

                self.gl.use_program(Some(self.border_program));
                self.gl.uniform_matrix_4_f32_slice(
                    self.border_uniforms.projection.as_ref(), false, proj,
                );
                self.gl.uniform_4_f32(
                    self.border_uniforms.rect.as_ref(), rx, ry, rw, rh,
                );
                self.gl.uniform_2_f32(
                    self.border_uniforms.size.as_ref(), rw, rh,
                );
                self.gl.uniform_1_f32(
                    self.border_uniforms.border_width.as_ref(), bw,
                );
                self.gl.uniform_4_f32(
                    self.border_uniforms.border_color.as_ref(),
                    0.3, 0.6, 1.0, 0.8 * anim_scale,
                );
                self.gl.uniform_1_f32(
                    self.border_uniforms.radius.as_ref(), 8.0,
                );
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            }

            // Draw title labels below each face (front-to-back)
            for face in faces.iter().rev() {
                let entry = &self.overview_windows[face.entry_idx];
                if face.brightness < 0.05 { continue; }

                let (tex, tw, th) = match entry.title_texture {
                    Some((t, w, h)) => (t, w, h),
                    None => continue,
                };

                // Project bottom-center of face to screen
                let (bcx, bcy) = Self::project_to_screen(&face.mvp, [0.0, -1.0, 0.0], mw, mh, vp_x, vp_y);
                let title_x = bcx - tw as f32 * 0.5;
                let title_y = bcy + 10.0;
                let title_alpha = (face.brightness / 0.55).min(1.0) * anim_scale;

                self.gl.use_program(Some(self.hud_text_program));
                self.gl.uniform_matrix_4_f32_slice(
                    self.hud_text_uniforms.projection.as_ref(), false, proj,
                );
                self.gl.uniform_4_f32(
                    self.hud_text_uniforms.rect.as_ref(),
                    title_x, title_y, tw as f32, th as f32,
                );
                self.gl.uniform_1_i32(
                    self.hud_text_uniforms.texture.as_ref(), 0,
                );
                self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                let _ = title_alpha; // opacity is baked into the title texture already
            }

            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
        }
    }
}
