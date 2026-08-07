use super::Compositor;
use crate::backend::compositor_common::annotation_overlay::{AnnotationLabel, AnnotationQuad};
use crate::backend::compositor_font;
use crate::backend::x11::compositor_common::annotations::{AnnotationPoint, AnnotationStroke};
use glow::HasContext;

use super::CompositorConnection;

impl<C: CompositorConnection> Compositor<C> {
    pub(crate) fn set_annotation_mode(&mut self, active: bool) {
        self.annotation_active = active;
        if !active {
            self.annotation_strokes.clear();
            self.annotation_quads.clear();
            if !self.annotation_labels.is_empty() {
                self.annotation_labels.clear();
                self.annotation_labels_dirty = true;
            }
        }
        self.needs_render = true;
    }

    pub(crate) fn annotation_add_quad(&mut self, quad: AnnotationQuad) {
        if !self.annotation_active || !quad.is_drawable() {
            return;
        }
        self.annotation_quads.push(quad);
        self.needs_render = true;
    }

    pub(crate) fn annotation_add_text(&mut self, label: AnnotationLabel) {
        if !self.annotation_active || !label.is_drawable() {
            return;
        }
        self.annotation_labels.push(label);
        self.annotation_labels_dirty = true;
        self.needs_render = true;
    }

    /// Rasterise and upload every label, once per change.
    pub(super) fn refresh_annotation_labels(&mut self) {
        if !self.annotation_labels_dirty {
            return;
        }
        self.annotation_labels_dirty = false;

        let stale = std::mem::take(&mut self.annotation_label_textures);
        unsafe {
            for (texture, _, _) in stale.into_iter().flatten() {
                self.gl.delete_texture(texture);
            }
        }

        let config = crate::config::CONFIG.load();
        let font = config.system_ui_font();
        let mut textures = Vec::with_capacity(self.annotation_labels.len());
        for label in &self.annotation_labels {
            let ink = [
                (label.color[0] * 255.0).clamp(0.0, 255.0) as u8,
                (label.color[1] * 255.0).clamp(0.0, 255.0) as u8,
                (label.color[2] * 255.0).clamp(0.0, 255.0) as u8,
                (label.color[3] * 255.0).clamp(0.0, 255.0) as u8,
            ];
            let (pixels, w, h) =
                compositor_font::render_ui_text_to_rgba(&label.text, font, label.size, ink);
            textures.push(if w == 0 || h == 0 {
                None
            } else {
                unsafe { self.upload_overlay_texture(&pixels, w, h) }.map(|t| (t, w, h))
            });
        }
        self.annotation_label_textures = textures;
    }

    /// Paint the filled shapes, then the labels over them.
    pub(super) fn render_annotation_shapes(&mut self, proj: &[f32; 16]) {
        if self.annotation_quads.is_empty() && self.annotation_label_textures.is_empty() {
            return;
        }
        unsafe {
            self.gl.bind_vertex_array(Some(self.quad_vao));
            // `sysui_fill_rounded` fills through the border program and takes
            // the binding as given — the tab bar gets it from the
            // `ui_fill_island` that draws its track. Nothing binds it for us,
            // so a redaction bar drew in whatever colour the last pass left
            // behind and a counter bubble did not appear at all.
            if !self.annotation_quads.is_empty() {
                self.gl.use_program(Some(self.border_program));
                self.gl.uniform_matrix_4_f32_slice(
                    self.border_uniforms.projection.as_ref(),
                    false,
                    proj,
                );
                for quad in &self.annotation_quads {
                    self.sysui_fill_rounded(
                        quad.x,
                        quad.y,
                        quad.w,
                        quad.h,
                        quad.radius,
                        quad.color,
                    );
                }
            }

            if !self.annotation_label_textures.is_empty() {
                self.gl.use_program(Some(self.hud_text_program));
                self.gl.uniform_matrix_4_f32_slice(
                    self.hud_text_uniforms.projection.as_ref(),
                    false,
                    proj,
                );
                self.gl
                    .uniform_1_i32(self.hud_text_uniforms.texture.as_ref(), 0);
                self.gl
                    .uniform_1_f32(self.hud_text_uniforms.opacity.as_ref(), 1.0);
                self.gl.active_texture(glow::TEXTURE0);
                for (label, slot) in self
                    .annotation_labels
                    .iter()
                    .zip(self.annotation_label_textures.iter())
                {
                    let Some((texture, w, h)) = slot else {
                        continue;
                    };
                    let (w, h) = (*w as f32, *h as f32);
                    let (x, y) = label.origin(w, h);
                    self.gl
                        .uniform_4_f32(self.hud_text_uniforms.rect.as_ref(), x, y, w, h);
                    self.gl.bind_texture(glow::TEXTURE_2D, Some(*texture));
                    self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                }
            }

            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
        }
    }

    /// Upload a straight-alpha RGBA buffer as a linearly filtered texture.
    /// Shared by the overlay's labels and the toolbar's icons.
    pub(super) unsafe fn upload_overlay_texture(
        &self,
        pixels: &[u8],
        w: u32,
        h: u32,
    ) -> Option<glow::Texture> {
        unsafe {
            let texture = self.gl.create_texture().ok()?;
            self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            self.gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                w as i32,
                h as i32,
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

    pub(crate) fn annotation_add_point(&mut self, x: f32, y: f32) {
        if !self.annotation_active {
            return;
        }
        if self.annotation_strokes.is_empty() {
            self.annotation_strokes.push(AnnotationStroke {
                points: Vec::new(),
                color: self.annotation_color,
                width: self.annotation_line_width,
            });
        }
        if let Some(stroke) = self.annotation_strokes.last_mut() {
            stroke.points.push(AnnotationPoint { x, y });
        }
        self.needs_render = true;
    }

    pub(crate) fn annotation_new_stroke(&mut self) {
        if !self.annotation_active {
            return;
        }
        self.annotation_strokes.push(AnnotationStroke {
            points: Vec::new(),
            color: self.annotation_color,
            width: self.annotation_line_width,
        });
    }

    pub(crate) fn set_annotation_color(&mut self, rgba: [f32; 4]) {
        self.annotation_color = rgba;
    }

    pub(crate) fn set_annotation_line_width(&mut self, width: f32) {
        self.annotation_line_width = width.max(1.0);
    }

    /// Render all annotation strokes as GL_LINES.
    pub(super) fn render_annotations(&self, proj: &[f32; 16]) {
        if self.annotation_strokes.is_empty() {
            return;
        }

        unsafe {
            self.gl.use_program(Some(self.annotation_line_program));
            self.gl.uniform_matrix_4_f32_slice(
                self.annotation_line_uniforms.projection.as_ref(),
                false,
                proj,
            );
            self.gl.enable(glow::BLEND);
            self.gl
                .blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);

            for stroke in &self.annotation_strokes {
                if stroke.points.len() < 2 {
                    continue;
                }
                self.gl.line_width(stroke.width);

                // Build vertex data for GL_LINES
                let mut vertices: Vec<f32> = Vec::new();
                for i in 0..stroke.points.len() - 1 {
                    let p0 = &stroke.points[i];
                    let p1 = &stroke.points[i + 1];
                    vertices.extend_from_slice(&[p0.x, p0.y, p1.x, p1.y]);
                }

                // Draw using a temp VBO
                let vbo = match self.gl.create_buffer() {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                let vao = match self.gl.create_vertex_array() {
                    Ok(v) => v,
                    Err(_) => {
                        self.gl.delete_buffer(vbo);
                        continue;
                    }
                };

                self.gl.bind_vertex_array(Some(vao));
                self.gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
                let byte_data: &[u8] =
                    std::slice::from_raw_parts(vertices.as_ptr() as *const u8, vertices.len() * 4);
                self.gl
                    .buffer_data_u8_slice(glow::ARRAY_BUFFER, byte_data, glow::STREAM_DRAW);
                self.gl
                    .vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 8, 0);
                self.gl.enable_vertex_attrib_array(0);

                self.gl.uniform_4_f32(
                    self.annotation_line_uniforms.color.as_ref(),
                    stroke.color[0],
                    stroke.color[1],
                    stroke.color[2],
                    stroke.color[3],
                );

                let num_verts = (stroke.points.len() - 1) * 2;
                self.gl.draw_arrays(glow::LINES, 0, num_verts as i32);

                self.gl.bind_vertex_array(None);
                self.gl.delete_vertex_array(vao);
                self.gl.delete_buffer(vbo);
            }

            self.gl.line_width(1.0);
            self.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
            self.gl.use_program(None);
        }
    }
}
