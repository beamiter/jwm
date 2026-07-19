use super::*;
use crate::backend::compositor_common::math::{
    mat4_mul, perspective_matrix, rotate_x_matrix, rotate_y_matrix, scale_matrix, translate_matrix,
};
use crate::backend::x11::compositor_common::transitions::normalized_transition_progress;
use smithay::backend::renderer::gles::ffi;

/// Geometry shared by every workspace-transition mode.
///
/// Draw coordinates use the compositor's top-left origin, while `scissor`
/// and `viewport` use OpenGL's bottom-left origin. `uv_rect` addresses the
/// matching workspace pixels in the full-screen transition snapshot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct TransitionLayout {
    draw_rect: [f32; 4],
    uv_rect: [f32; 4],
    scissor: [i32; 4],
}

pub(super) fn transition_layout(
    screen_w: u32,
    screen_h: u32,
    mon_rect: (i32, i32, u32, u32),
    exclude_top: u32,
) -> Option<TransitionLayout> {
    if screen_w == 0
        || screen_h == 0
        || screen_w > i32::MAX as u32
        || screen_h > i32::MAX as u32
        || mon_rect.2 == 0
        || mon_rect.3 == 0
    {
        return None;
    }

    let (mon_x, mon_y, mon_w, mon_h) = mon_rect;
    let workspace_top = i64::from(mon_y) + i64::from(exclude_top.min(mon_h));
    let workspace_bottom = i64::from(mon_y) + i64::from(mon_h);
    let workspace_left = i64::from(mon_x);
    let workspace_right = i64::from(mon_x) + i64::from(mon_w);

    let x0 = workspace_left.clamp(0, i64::from(screen_w));
    let x1 = workspace_right.clamp(0, i64::from(screen_w));
    let y0 = workspace_top.clamp(0, i64::from(screen_h));
    let y1 = workspace_bottom.clamp(0, i64::from(screen_h));
    if x1 <= x0 || y1 <= y0 {
        return None;
    }

    let x = x0 as i32;
    let y = y0 as i32;
    let width = (x1 - x0) as i32;
    let height = (y1 - y0) as i32;
    let gl_y = screen_h as i32 - (y + height);
    let inv_w = 1.0 / screen_w as f32;
    let inv_h = 1.0 / screen_h as f32;

    Some(TransitionLayout {
        draw_rect: [x as f32, y as f32, width as f32, height as f32],
        uv_rect: [
            x as f32 * inv_w,
            gl_y as f32 * inv_h,
            width as f32 * inv_w,
            height as f32 * inv_h,
        ],
        scissor: [x, gl_y, width, height],
    })
}

fn blinds_strip_layout(
    layout: TransitionLayout,
    strip: u32,
    strip_count: u32,
) -> Option<(f32, f32, [f32; 4], [i32; 4])> {
    if strip_count == 0 || strip >= strip_count {
        return None;
    }
    let f0 = strip as f32 / strip_count as f32;
    let f1 = (strip + 1) as f32 / strip_count as f32;
    let x0 = layout.draw_rect[0] + layout.draw_rect[2] * f0;
    let x1 = layout.draw_rect[0] + layout.draw_rect[2] * f1;
    let outer_right = layout.scissor[0] + layout.scissor[2];
    let scissor_x0 = (x0.floor() as i32).max(layout.scissor[0]);
    let scissor_x1 = (x1.ceil() as i32).min(outer_right);
    if scissor_x1 <= scissor_x0 {
        return None;
    }
    Some((
        x0,
        x1 - x0,
        [
            layout.uv_rect[0] + layout.uv_rect[2] * f0,
            layout.uv_rect[1],
            layout.uv_rect[2] * (f1 - f0),
            layout.uv_rect[3],
        ],
        [
            scissor_x0,
            layout.scissor[1],
            scissor_x1 - scissor_x0,
            layout.scissor[3],
        ],
    ))
}

impl WaylandCompositor {
    fn finish_transition(&mut self) {
        self.transition_active = false;
        self.transition_start = None;
        self.transition_mon = None;
    }

    fn prepare_flat_transition(
        &self,
        gl: &ffi::Gles2,
        projection: &[f32; 16],
        layout: TransitionLayout,
    ) {
        unsafe {
            gl.UseProgram(self.transition_program);
            gl.UniformMatrix4fv(
                self.transition_uniforms.projection,
                1,
                ffi::FALSE as u8,
                projection.as_ptr(),
            );
            gl.Uniform4f(
                self.transition_uniforms.uv_rect,
                layout.uv_rect[0],
                layout.uv_rect[1],
                layout.uv_rect[2],
                layout.uv_rect[3],
            );
            // Samplers are initialized to texture unit zero when a program is
            // linked. This program never changes that binding, so avoid a
            // GetUniformLocation round trip on every animation frame.
            gl.ActiveTexture(ffi::TEXTURE0);
            gl.BindTexture(ffi::TEXTURE_2D, self.transition_texture);
            gl.BindVertexArray(self.quad_vao);
        }
    }

    fn draw_flat_transition_rect(
        &self,
        gl: &ffi::Gles2,
        rect: [f32; 4],
        uv_rect: [f32; 4],
        opacity: f32,
    ) {
        unsafe {
            gl.Uniform4f(
                self.transition_uniforms.rect,
                rect[0],
                rect[1],
                rect[2],
                rect[3],
            );
            gl.Uniform4f(
                self.transition_uniforms.uv_rect,
                uv_rect[0],
                uv_rect[1],
                uv_rect[2],
                uv_rect[3],
            );
            gl.Uniform1f(self.transition_uniforms.opacity, opacity.clamp(0.0, 1.0));
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
        }
    }

    fn draw_3d_transition_face(
        &self,
        gl: &ffi::Gles2,
        layout: TransitionLayout,
        mvp: &[f32; 16],
        brightness: f32,
    ) {
        unsafe {
            gl.Viewport(
                layout.scissor[0],
                layout.scissor[1],
                layout.scissor[2],
                layout.scissor[3],
            );
            gl.UseProgram(self.cube_program);
            gl.UniformMatrix4fv(self.cube_uniforms.mvp, 1, ffi::FALSE as u8, mvp.as_ptr());
            gl.Uniform1f(
                self.cube_uniforms.aspect,
                layout.draw_rect[2] / layout.draw_rect[3],
            );
            gl.Uniform1f(self.cube_uniforms.brightness, brightness.clamp(0.0, 1.0));
            gl.Uniform4f(
                self.cube_uniforms.uv_rect,
                layout.uv_rect[0],
                layout.uv_rect[1],
                layout.uv_rect[2],
                layout.uv_rect[3],
            );
            gl.ActiveTexture(ffi::TEXTURE0);
            gl.BindTexture(ffi::TEXTURE_2D, self.transition_texture);
            gl.Uniform1i(self.cube_uniforms.texture, 0);
            gl.BindVertexArray(self.quad_vao);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
        }
    }

    /// Render the workspace transition overlay.
    /// Called from render_frame when transition_active is true.
    pub(crate) fn render_transition(&mut self, gl: &ffi::Gles2, projection: &[f32; 16]) {
        let Some(start) = self.transition_start else {
            self.finish_transition();
            return;
        };
        let Some(progress) = normalized_transition_progress(
            start,
            std::time::Instant::now(),
            self.transition_duration,
        ) else {
            self.finish_transition();
            return;
        };
        let Some(mon_rect) = self.transition_mon else {
            self.finish_transition();
            return;
        };
        let Some(layout) = transition_layout(
            self.screen_w,
            self.screen_h,
            mon_rect,
            self.transition_exclude_top,
        ) else {
            self.finish_transition();
            return;
        };
        let t = self.transition_mode.eased_progress(progress);

        unsafe {
            gl.Enable(ffi::SCISSOR_TEST);
            gl.Scissor(
                layout.scissor[0],
                layout.scissor[1],
                layout.scissor[2],
                layout.scissor[3],
            );
        }

        match self.transition_mode {
            TransitionMode::Slide => self.render_slide_transition(gl, projection, layout, t),
            TransitionMode::Cube => self.render_cube_transition(gl, layout, t),
            TransitionMode::Flip => self.render_flip_transition(gl, layout, t),
            TransitionMode::Fade => self.render_fade_transition(gl, projection, layout, t),
            TransitionMode::Zoom => self.render_zoom_transition(gl, projection, layout, t),
            TransitionMode::Portal => self.render_portal_transition(gl, projection, layout, t),
            TransitionMode::Stack => self.render_stack_transition(gl, projection, layout, t),
            TransitionMode::Blinds => self.render_blinds_transition(gl, projection, layout, t),
            TransitionMode::CoverFlow => self.render_coverflow_transition(gl, layout, t),
            TransitionMode::Helix => self.render_helix_transition(gl, layout, t),
            TransitionMode::None => {}
        }

        // 3D modes select a monitor viewport, and blinds replaces the outer
        // scissor with per-strip rectangles. Restore the full-output contract
        // expected by overlays rendered after the workspace transition.
        unsafe {
            gl.BindVertexArray(0);
            gl.BindTexture(ffi::TEXTURE_2D, 0);
            gl.UseProgram(0);
            gl.Disable(ffi::SCISSOR_TEST);
            gl.Viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
        }

        self.needs_render = true;
    }

    fn render_slide_transition(
        &self,
        gl: &ffi::Gles2,
        projection: &[f32; 16],
        layout: TransitionLayout,
        t: f32,
    ) {
        let direction = self.transition_direction as f32;
        let offset_x = -direction * t * layout.draw_rect[2];
        let rect = [
            layout.draw_rect[0] + offset_x,
            layout.draw_rect[1],
            layout.draw_rect[2],
            layout.draw_rect[3],
        ];

        self.prepare_flat_transition(gl, projection, layout);
        self.draw_flat_transition_rect(gl, rect, layout.uv_rect, 1.0 - 0.1 * t);
    }

    fn render_fade_transition(
        &self,
        gl: &ffi::Gles2,
        projection: &[f32; 16],
        layout: TransitionLayout,
        t: f32,
    ) {
        self.prepare_flat_transition(gl, projection, layout);
        self.draw_flat_transition_rect(gl, layout.draw_rect, layout.uv_rect, 1.0 - t);
    }

    fn render_zoom_transition(
        &self,
        gl: &ffi::Gles2,
        projection: &[f32; 16],
        layout: TransitionLayout,
        t: f32,
    ) {
        let scale = 1.0 + 0.24 * t;
        let width = layout.draw_rect[2] * scale;
        let height = layout.draw_rect[3] * scale;
        let rect = [
            layout.draw_rect[0] + (layout.draw_rect[2] - width) * 0.5,
            layout.draw_rect[1] + (layout.draw_rect[3] - height) * 0.5,
            width,
            height,
        ];

        self.prepare_flat_transition(gl, projection, layout);
        self.draw_flat_transition_rect(gl, rect, layout.uv_rect, 1.0 - t);
    }

    fn render_stack_transition(
        &self,
        gl: &ffi::Gles2,
        projection: &[f32; 16],
        layout: TransitionLayout,
        t: f32,
    ) {
        let direction = self.transition_direction as f32;
        let scale = 1.0 - 0.16 * t;
        let width = layout.draw_rect[2] * scale;
        let height = layout.draw_rect[3] * scale;
        let rect = [
            layout.draw_rect[0]
                + (layout.draw_rect[2] - width) * 0.5
                + direction * t * layout.draw_rect[2] * 0.07,
            layout.draw_rect[1] + (layout.draw_rect[3] - height) * 0.5,
            width,
            height,
        ];

        self.prepare_flat_transition(gl, projection, layout);
        self.draw_flat_transition_rect(gl, rect, layout.uv_rect, 1.0 - t);
    }

    fn render_cube_transition(&self, gl: &ffi::Gles2, layout: TransitionLayout, t: f32) {
        let aspect = layout.draw_rect[2] / layout.draw_rect[3];
        let direction = self.transition_direction as f32;
        let half_pi = std::f32::consts::FRAC_PI_2;
        let depth = aspect;
        let zoom = 1.0 + 0.22 * (t * std::f32::consts::PI).sin();
        let camera_z = (1.0 + depth) * zoom;
        let perspective = perspective_matrix(half_pi, aspect, 0.1, camera_z * 4.0);
        let view = translate_matrix(0.0, 0.0, -camera_z);
        let angle = direction * t * half_pi;
        let tilt = rotate_x_matrix(-0.055 * (t * std::f32::consts::PI).sin());
        let model = mat4_mul(
            &rotate_y_matrix(angle),
            &mat4_mul(&tilt, &translate_matrix(0.0, 0.0, depth)),
        );
        let mvp = mat4_mul(&perspective, &mat4_mul(&view, &model));
        let brightness = 0.22 + 0.78 * angle.cos().abs();

        self.draw_3d_transition_face(gl, layout, &mvp, brightness);
    }

    fn render_flip_transition(&self, gl: &ffi::Gles2, layout: TransitionLayout, t: f32) {
        let aspect = layout.draw_rect[2] / layout.draw_rect[3];
        let direction = self.transition_direction as f32;
        let pulse = (t * std::f32::consts::PI).sin();
        let camera_z = 1.0 + 0.16 * pulse;
        let perspective = perspective_matrix(std::f32::consts::FRAC_PI_2, aspect, 0.1, 6.0);
        let view = translate_matrix(0.0, 0.0, -camera_z);
        let angle = direction * t * std::f32::consts::FRAC_PI_2;
        let model = mat4_mul(
            &rotate_y_matrix(angle),
            &mat4_mul(
                &rotate_x_matrix(-0.08 * pulse),
                &scale_matrix(1.0 - 0.06 * pulse, 1.0 - 0.06 * pulse, 1.0),
            ),
        );
        let mvp = mat4_mul(&perspective, &mat4_mul(&view, &model));
        let brightness = 0.18 + 0.82 * angle.cos().abs();

        self.draw_3d_transition_face(gl, layout, &mvp, brightness);
    }

    fn render_blinds_transition(
        &self,
        gl: &ffi::Gles2,
        projection: &[f32; 16],
        layout: TransitionLayout,
        t: f32,
    ) {
        const STRIP_COUNT: u32 = 8;
        const STAGGER: f32 = 0.32;
        let reverse = self.transition_direction < 0;

        self.prepare_flat_transition(gl, projection, layout);
        for strip in 0..STRIP_COUNT {
            let wave_index = if reverse {
                STRIP_COUNT - 1 - strip
            } else {
                strip
            };
            let delay = wave_index as f32 / (STRIP_COUNT - 1) as f32 * STAGGER;
            let local = ((t - delay) / (1.0 - STAGGER)).clamp(0.0, 1.0);
            let Some((strip_x, strip_w, strip_uv, strip_scissor)) =
                blinds_strip_layout(layout, strip, STRIP_COUNT)
            else {
                continue;
            };

            let width = strip_w * (1.0 - local);
            if width < 0.5 {
                continue;
            }
            let rect = [
                strip_x + (strip_w - width) * 0.5,
                layout.draw_rect[1],
                width,
                layout.draw_rect[3],
            ];

            unsafe {
                gl.Scissor(
                    strip_scissor[0],
                    strip_scissor[1],
                    strip_scissor[2],
                    strip_scissor[3],
                );
            }
            self.draw_flat_transition_rect(gl, rect, strip_uv, 1.0 - local * 0.12);
        }
    }

    fn render_coverflow_transition(&self, gl: &ffi::Gles2, layout: TransitionLayout, t: f32) {
        let aspect = layout.draw_rect[2] / layout.draw_rect[3];
        let direction = self.transition_direction as f32;
        let angle = direction * t * 72.0f32.to_radians();
        let scale = 1.0 - 0.2 * t;
        let x = -direction * t * aspect * 0.92;
        let y = -0.06 * (t * std::f32::consts::PI).sin();
        let z = -0.48 * t;
        let perspective = perspective_matrix(std::f32::consts::FRAC_PI_2, aspect, 0.1, 8.0);
        let view = translate_matrix(0.0, 0.0, -1.0);
        let model = mat4_mul(
            &translate_matrix(x, y, z),
            &mat4_mul(&rotate_y_matrix(angle), &scale_matrix(scale, scale, 1.0)),
        );
        let mvp = mat4_mul(&perspective, &mat4_mul(&view, &model));
        let brightness = (1.0 - 0.58 * t).max(0.28);

        self.draw_3d_transition_face(gl, layout, &mvp, brightness);
    }

    fn render_helix_transition(&self, gl: &ffi::Gles2, layout: TransitionLayout, t: f32) {
        let aspect = layout.draw_rect[2] / layout.draw_rect[3];
        let direction = self.transition_direction as f32;
        let theta = direction * t * std::f32::consts::PI * 1.25;
        let radius = aspect * 0.58;
        let x = radius * theta.sin();
        let y = -0.34 * t + 0.08 * (t * std::f32::consts::PI * 2.0).sin();
        let z = -radius * (1.0 - theta.cos().abs()) - 0.22 * t;
        let scale = (1.0 - 0.44 * t).max(0.5);
        let perspective = perspective_matrix(std::f32::consts::FRAC_PI_2, aspect, 0.1, 10.0);
        let view = translate_matrix(0.0, 0.0, -1.0);
        let model = mat4_mul(
            &translate_matrix(x, y, z),
            &mat4_mul(
                &rotate_y_matrix(theta),
                &mat4_mul(
                    &rotate_x_matrix(-0.18 * t),
                    &scale_matrix(scale, scale, 1.0),
                ),
            ),
        );
        let mvp = mat4_mul(&perspective, &mat4_mul(&view, &model));
        let brightness = (1.0 - 0.68 * t).max(0.2);

        self.draw_3d_transition_face(gl, layout, &mvp, brightness);
    }

    fn render_portal_transition(
        &self,
        gl: &ffi::Gles2,
        projection: &[f32; 16],
        layout: TransitionLayout,
        t: f32,
    ) {
        let glow = (t * std::f32::consts::PI).sin().max(0.0) * 1.65;
        let center_x = 0.5 + 0.07 * self.transition_direction as f32;
        let center_y = 0.5 - 0.035 * (t * std::f32::consts::PI).sin();

        unsafe {
            gl.UseProgram(self.portal_program);
            gl.UniformMatrix4fv(
                self.portal_uniforms.projection,
                1,
                ffi::FALSE as u8,
                projection.as_ptr(),
            );
            gl.Uniform4f(
                self.portal_uniforms.rect,
                layout.draw_rect[0],
                layout.draw_rect[1],
                layout.draw_rect[2],
                layout.draw_rect[3],
            );
            gl.Uniform4f(
                self.portal_uniforms.uv_rect,
                layout.uv_rect[0],
                layout.uv_rect[1],
                layout.uv_rect[2],
                layout.uv_rect[3],
            );
            gl.Uniform1f(self.portal_uniforms.progress, t);
            gl.Uniform1f(self.portal_uniforms.glow, glow);
            gl.Uniform2f(self.portal_uniforms.center, center_x, center_y);
            gl.ActiveTexture(ffi::TEXTURE0);
            gl.BindTexture(ffi::TEXTURE_2D, self.transition_texture);
            gl.Uniform1i(self.portal_uniforms.texture, 0);
            gl.BindVertexArray(self.quad_vao);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
        }
    }

    /// Capture current frame to transition FBO (called before tag switch).
    #[allow(dead_code)]
    pub(crate) fn capture_transition_snapshot(&self, gl: &ffi::Gles2) {
        unsafe {
            gl.BindFramebuffer(ffi::READ_FRAMEBUFFER, self.output_fbo);
            gl.BindFramebuffer(ffi::DRAW_FRAMEBUFFER, self.transition_fbo);
            gl.BlitFramebuffer(
                0,
                0,
                self.screen_w as i32,
                self.screen_h as i32,
                0,
                0,
                self.screen_w as i32,
                self.screen_h as i32,
                ffi::COLOR_BUFFER_BIT,
                ffi::NEAREST,
            );
            gl.BindFramebuffer(ffi::FRAMEBUFFER, self.output_fbo);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(actual: f32, expected: f32) {
        assert!((actual - expected).abs() < 1.0e-6, "{actual} != {expected}");
    }

    #[test]
    fn layout_targets_only_selected_monitor_workspace() {
        let layout =
            transition_layout(3840, 1080, (1920, 0, 1920, 1080), 30).expect("valid layout");

        assert_eq!(layout.draw_rect, [1920.0, 30.0, 1920.0, 1050.0]);
        assert_eq!(layout.scissor, [1920, 0, 1920, 1050]);
        assert_close(layout.uv_rect[0], 0.5);
        assert_close(layout.uv_rect[1], 0.0);
        assert_close(layout.uv_rect[2], 0.5);
        assert_close(layout.uv_rect[3], 1050.0 / 1080.0);
    }

    #[test]
    fn layout_clips_malformed_monitor_without_overflowing_gl_bounds() {
        let layout =
            transition_layout(1920, 1080, (-100, -20, 800, 600), 40).expect("visible workspace");

        assert_eq!(layout.draw_rect, [0.0, 20.0, 700.0, 560.0]);
        assert_eq!(layout.scissor, [0, 500, 700, 560]);
        assert_close(layout.uv_rect[0], 0.0);
        assert_close(layout.uv_rect[1], 500.0 / 1080.0);
        assert_close(layout.uv_rect[2], 700.0 / 1920.0);
        assert_close(layout.uv_rect[3], 560.0 / 1080.0);
    }

    #[test]
    fn layout_rejects_empty_or_fully_excluded_workspaces() {
        assert!(transition_layout(1920, 1080, (0, 0, 0, 1080), 0).is_none());
        assert!(transition_layout(1920, 1080, (0, 0, 1920, 1080), 1080).is_none());
        assert!(transition_layout(1920, 1080, (2500, 0, 100, 100), 0).is_none());
    }

    #[test]
    fn blind_strip_scissors_are_intersections_of_workspace_scissor() {
        let layout =
            transition_layout(3840, 1200, (1920, 50, 1920, 1080), 30).expect("valid layout");
        let first = blinds_strip_layout(layout, 0, 8).expect("first strip");
        let last = blinds_strip_layout(layout, 7, 8).expect("last strip");

        assert_eq!(first.3, [1920, 70, 240, 1050]);
        assert_eq!(last.3, [3600, 70, 240, 1050]);
        assert_close(first.2[0], 0.5);
        assert_close(last.2[0] + last.2[2], 1.0);
        assert_eq!(first.3[1], layout.scissor[1]);
        assert_eq!(first.3[3], layout.scissor[3]);
    }
}
