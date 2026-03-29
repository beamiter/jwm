use glow::HasContext;
use super::Compositor;
use super::math::{perspective_matrix, translate_matrix, rotate_y_matrix, mat4_mul};

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

        let diff = self.overview_prism_target_angle - self.overview_prism_current_angle;
        if diff.abs() < 0.001 {
            self.overview_prism_current_angle = self.overview_prism_target_angle;
        } else {
            // Exponential ease-out: close 88% of the remaining gap per 0.1s.
            // t = 1 - e^(-speed * dt), speed=20 gives ~86% per 0.1s frame,
            // feels snappy at any frame rate.
            let t = 1.0 - (-20.0_f32 * dt).exp();
            self.overview_prism_current_angle += diff * t;
            self.needs_render = true;
        }
    }

    /// Render overview overlay (Alt-Tab preview) as a 3D hexagonal prism carousel.
    /// Rendering is confined to the monitor that owns the overview.
    pub(super) fn render_overview(&self, proj: &[f32; 16], _focused: Option<u32>) {
        if self.overview_windows.is_empty() { return; }

        // Monitor-local dimensions
        let mon_x = self.overview_mon_x;
        let mon_y = self.overview_mon_y;
        let mon_w = self.overview_mon_w;
        let mon_h = self.overview_mon_h;
        let mw = mon_w as f32;
        let mh = mon_h as f32;

        unsafe {
            // === 1. Dark background overlay (only on this monitor) ===
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

            // === 2. Scissor + viewport to this monitor for the 3D prism ===
            let scissor_gl_y = self.screen_h as i32 - (mon_y + mon_h as i32);
            self.gl.enable(glow::SCISSOR_TEST);
            self.gl.scissor(mon_x, scissor_gl_y, mon_w as i32, mon_h as i32);
            self.gl.viewport(mon_x, scissor_gl_y, mon_w as i32, mon_h as i32);

            // === 3. Compute hexagonal prism geometry ===
            let face_w = mw * 0.8;
            let face_h = mh * 0.8;
            let face_aspect = face_w / face_h;
            let apothem = face_aspect * 3.0_f32.sqrt();

            let fov_y = std::f32::consts::FRAC_PI_4; // 45 degrees
            let camera_z = (apothem + 1.0 / (fov_y * 0.5).tan()) * 1.2;
            let mon_aspect = mw / mh;

            let persp = perspective_matrix(fov_y, mon_aspect, 0.1, camera_z * 4.0);
            let view = translate_matrix(0.0, 0.0, -camera_z);

            let global_rot = rotate_y_matrix(self.overview_prism_current_angle);

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

                let model = mat4_mul(&global_rot, &face_model);
                let mv = mat4_mul(&view, &model);
                let mvp = mat4_mul(&persp, &mv);

                let z_depth = mv[14];

                let total_angle = face_angle + self.overview_prism_current_angle;
                let cos_facing = total_angle.cos();
                let brightness = if entry.is_selected {
                    0.35 + 0.35 * cos_facing.max(0.0)
                } else {
                    0.25 + 0.30 * cos_facing.max(0.0)
                };

                faces.push(FaceDrawInfo {
                    mvp,
                    z_depth,
                    brightness,
                    entry_idx: idx,
                });
            }

            // === 5. Painter's algorithm: sort by Z-depth ascending (farthest first) ===
            faces.sort_by(|a, b| a.z_depth.partial_cmp(&b.z_depth).unwrap_or(std::cmp::Ordering::Equal));

            // === 6. Draw faces using cube_program ===
            self.gl.use_program(Some(self.cube_program));
            self.gl.uniform_1_f32(self.cube_uniforms.aspect.as_ref(), face_aspect);
            self.gl.uniform_1_i32(self.cube_uniforms.texture.as_ref(), 0);
            // Snapshot textures have top-left origin (X11/image convention)
            // but GL texture coords have bottom-left origin.  Flip Y by
            // setting uv_rect to (0, 1, 1, -1): start at v=1, height=-1.
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

            // Restore full-screen viewport and disable scissor
            self.gl.disable(glow::SCISSOR_TEST);
            self.gl.viewport(0, 0, self.screen_w as i32, self.screen_h as i32);

            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
        }
    }
}
