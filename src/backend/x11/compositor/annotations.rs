use glow::HasContext;
use super::Compositor;

/// A point in an annotation stroke.
#[derive(Clone, Copy)]
pub(super) struct AnnotationPoint {
    pub x: f32,
    pub y: f32,
}

/// A single annotation stroke (line segment sequence).
pub(super) struct AnnotationStroke {
    pub points: Vec<AnnotationPoint>,
    pub color: [f32; 4],
    pub width: f32,
}

impl Compositor {
    pub(in crate::backend::x11) fn set_annotation_mode(&mut self, active: bool) {
        self.annotation_active = active;
        if !active {
            self.annotation_strokes.clear();
        }
        self.needs_render = true;
    }

    pub(in crate::backend::x11) fn annotation_add_point(&mut self, x: f32, y: f32) {
        if !self.annotation_active { return; }
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

    /// Render all annotation strokes as GL_LINES.
    pub(super) fn render_annotations(&self, proj: &[f32; 16]) {
        if self.annotation_strokes.is_empty() { return; }

        unsafe {
            // Use the HUD program with a solid color for drawing lines
            self.gl.use_program(Some(self.hud_program));
            self.gl.uniform_matrix_4_f32_slice(
                self.hud_uniforms.projection.as_ref(), false, proj,
            );

            for stroke in &self.annotation_strokes {
                if stroke.points.len() < 2 { continue; }
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
                    Err(_) => { self.gl.delete_buffer(vbo); continue; }
                };

                self.gl.bind_vertex_array(Some(vao));
                self.gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
                let byte_data: &[u8] = std::slice::from_raw_parts(
                    vertices.as_ptr() as *const u8,
                    vertices.len() * 4,
                );
                self.gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, byte_data, glow::STREAM_DRAW);
                self.gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 8, 0);
                self.gl.enable_vertex_attrib_array(0);

                // Set color via bg_color uniform (reusing HUD shader)
                self.gl.uniform_4_f32(
                    self.hud_uniforms.bg_color.as_ref(),
                    stroke.color[0], stroke.color[1], stroke.color[2], stroke.color[3],
                );
                self.gl.uniform_2_f32(self.hud_uniforms.size.as_ref(), 1.0, 1.0);
                self.gl.uniform_4_f32(self.hud_uniforms.rect.as_ref(), 0.0, 0.0, 1.0, 1.0);

                let num_verts = (stroke.points.len() - 1) * 2;
                self.gl.draw_arrays(glow::LINES, 0, num_verts as i32);

                self.gl.bind_vertex_array(None);
                self.gl.delete_vertex_array(vao);
                self.gl.delete_buffer(vbo);
            }

            self.gl.line_width(1.0);
            self.gl.use_program(None);
        }
    }
}
