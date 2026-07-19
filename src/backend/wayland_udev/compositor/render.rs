// render_frame and rendering helpers for the Wayland udev compositor
#[allow(unused_imports)]
use super::*;
use crate::backend::compositor_common::capture::clip_region;
use smithay::backend::renderer::gles::ffi;

fn oriented_content_uv(content_uv: [f32; 4], y_inverted: bool) -> [f32; 4] {
    let [u, v, w, h] = content_uv;
    if y_inverted {
        [u, v + h, w, -h]
    } else {
        content_uv
    }
}

fn premultiplied_blend_factors() -> (u32, u32) {
    (ffi::ONE, ffi::ONE_MINUS_SRC_ALPHA)
}

fn overlay_output_is_scene_linear(scene_linear_active: bool, hw_encode_active: bool) -> bool {
    scene_linear_active && hw_encode_active
}

fn postprocess_requires_continuous_frames(
    postprocess_active: bool,
    has_time_varying_input: bool,
) -> bool {
    postprocess_active && has_time_varying_input
}

pub(super) fn edge_glow_requires_continuous_frames(
    enabled: bool,
    width: f32,
    active: bool,
    suppressed: bool,
) -> bool {
    enabled && width > 0.0 && active && !suppressed
}

#[derive(Clone, Copy, Debug)]
struct OcclusionCandidate {
    rect: (i32, i32, u32, u32),
    screen_size: (u32, u32),
    has_alpha: bool,
    fade_opacity: f32,
    effective_opacity: f32,
    anim_scale: f32,
    window_scale: f32,
    corner_radius: f32,
    is_shaped: bool,
    has_wobbly_deformation: bool,
    ripple_active: bool,
    focused_tilt_active: bool,
    samples_background: bool,
}

/// Occlusion culling is valid only when the candidate provably overwrites
/// every output pixel with alpha one. Any deformation or rounded/shaped mask
/// can expose the scene below even when the undeformed window rectangle covers
/// the output.
fn is_opaque_output_occluder(candidate: OcclusionCandidate) -> bool {
    let (x, y, width, height) = candidate.rect;
    let (screen_width, screen_height) = candidate.screen_size;

    !candidate.has_alpha
        && candidate.fade_opacity >= 1.0
        && candidate.effective_opacity >= 1.0
        && (candidate.anim_scale - 1.0).abs() <= f32::EPSILON
        && (candidate.window_scale - 1.0).abs() <= f32::EPSILON
        && candidate.corner_radius.is_finite()
        && candidate.corner_radius <= 0.0
        && !candidate.is_shaped
        && !candidate.has_wobbly_deformation
        && !candidate.ripple_active
        && !candidate.focused_tilt_active
        && !candidate.samples_background
        && x <= 0
        && y <= 0
        && i64::from(x) + i64::from(width) >= i64::from(screen_width)
        && i64::from(y) + i64::from(height) >= i64::from(screen_height)
}

#[cfg(test)]
mod tests {
    use super::{
        OcclusionCandidate, edge_glow_requires_continuous_frames, is_opaque_output_occluder,
        oriented_content_uv, overlay_output_is_scene_linear,
        postprocess_requires_continuous_frames, premultiplied_blend_factors,
    };
    use smithay::backend::renderer::gles::ffi;

    #[test]
    fn content_uv_preserves_non_inverted_subrect() {
        assert_eq!(
            oriented_content_uv([0.1, 0.2, 0.6, 0.5], false),
            [0.1, 0.2, 0.6, 0.5]
        );
    }

    #[test]
    fn content_uv_flips_only_the_selected_subrect() {
        assert_eq!(
            oriented_content_uv([0.1, 0.2, 0.6, 0.5], true),
            [0.1, 0.7, 0.6, -0.5]
        );
    }

    #[test]
    fn premultiplied_passes_use_one_source_blending() {
        assert_eq!(
            premultiplied_blend_factors(),
            (ffi::ONE, ffi::ONE_MINUS_SRC_ALPHA)
        );
        assert_ne!(premultiplied_blend_factors().0, ffi::SRC_ALPHA);
    }

    #[test]
    fn overlay_domain_is_linear_only_with_hardware_output_encoding() {
        assert!(!overlay_output_is_scene_linear(false, false));
        assert!(!overlay_output_is_scene_linear(false, true));
        assert!(!overlay_output_is_scene_linear(true, false));
        assert!(overlay_output_is_scene_linear(true, true));
    }

    #[test]
    fn static_postprocess_does_not_request_continuous_frames() {
        assert!(!postprocess_requires_continuous_frames(false, false));
        assert!(!postprocess_requires_continuous_frames(true, false));
        assert!(!postprocess_requires_continuous_frames(false, true));
        assert!(postprocess_requires_continuous_frames(true, true));
    }

    #[test]
    fn edge_glow_ticks_only_while_it_is_actually_drawn() {
        assert!(edge_glow_requires_continuous_frames(true, 8.0, true, false));
        assert!(!edge_glow_requires_continuous_frames(
            false, 8.0, true, false
        ));
        assert!(!edge_glow_requires_continuous_frames(
            true, 0.0, true, false
        ));
        assert!(!edge_glow_requires_continuous_frames(
            true, 8.0, false, false
        ));
        assert!(!edge_glow_requires_continuous_frames(true, 8.0, true, true));
        assert!(!edge_glow_requires_continuous_frames(
            true,
            f32::NAN,
            true,
            false
        ));
    }

    fn opaque_fullscreen_candidate() -> OcclusionCandidate {
        OcclusionCandidate {
            rect: (0, 0, 1920, 1080),
            screen_size: (1920, 1080),
            has_alpha: false,
            fade_opacity: 1.0,
            effective_opacity: 1.0,
            anim_scale: 1.0,
            window_scale: 1.0,
            corner_radius: 0.0,
            is_shaped: false,
            has_wobbly_deformation: false,
            ripple_active: false,
            focused_tilt_active: false,
            samples_background: false,
        }
    }

    #[test]
    fn only_provably_opaque_fullscreen_window_culls_lower_layers() {
        assert!(is_opaque_output_occluder(opaque_fullscreen_candidate()));

        for mutate in [
            |c: &mut OcclusionCandidate| c.has_alpha = true,
            |c: &mut OcclusionCandidate| c.fade_opacity = 0.9,
            |c: &mut OcclusionCandidate| c.effective_opacity = 0.9,
            |c: &mut OcclusionCandidate| c.anim_scale = 0.95,
            |c: &mut OcclusionCandidate| c.window_scale = 0.9,
            |c: &mut OcclusionCandidate| c.corner_radius = 8.0,
            |c: &mut OcclusionCandidate| c.is_shaped = true,
            |c: &mut OcclusionCandidate| c.has_wobbly_deformation = true,
            |c: &mut OcclusionCandidate| c.ripple_active = true,
            |c: &mut OcclusionCandidate| c.focused_tilt_active = true,
            |c: &mut OcclusionCandidate| c.samples_background = true,
        ] {
            let mut candidate = opaque_fullscreen_candidate();
            mutate(&mut candidate);
            assert!(!is_opaque_output_occluder(candidate));
        }
    }

    #[test]
    fn occluder_must_cover_the_entire_output() {
        let mut candidate = opaque_fullscreen_candidate();
        candidate.rect = (1, 0, 1920, 1080);
        assert!(!is_opaque_output_occluder(candidate));

        candidate = opaque_fullscreen_candidate();
        candidate.rect = (0, 0, 1919, 1080);
        assert!(!is_opaque_output_occluder(candidate));
    }

    #[test]
    fn non_finite_occluder_properties_never_cull_lower_layers() {
        for mutate in [
            |c: &mut OcclusionCandidate| c.fade_opacity = f32::NAN,
            |c: &mut OcclusionCandidate| c.effective_opacity = f32::NAN,
            |c: &mut OcclusionCandidate| c.anim_scale = f32::NAN,
            |c: &mut OcclusionCandidate| c.window_scale = f32::NAN,
            |c: &mut OcclusionCandidate| c.corner_radius = f32::NAN,
        ] {
            let mut candidate = opaque_fullscreen_candidate();
            mutate(&mut candidate);
            assert!(!is_opaque_output_occluder(candidate));
        }
    }
}

impl WaylandCompositor {
    // =========================================================================
    // Helper: draw a fullscreen quad
    // =========================================================================

    pub(crate) unsafe fn bind_quad_vao(&self, gl: &ffi::Gles2) {
        unsafe {
            gl.BindVertexArray(self.quad_vao);
            gl.BindBuffer(ffi::ARRAY_BUFFER, self.quad_vbo);
            gl.EnableVertexAttribArray(0);
            gl.VertexAttribPointer(0, 2, ffi::FLOAT, ffi::FALSE as u8, 8, std::ptr::null());
            gl.BindBuffer(ffi::ARRAY_BUFFER, 0);
        }
    }

    /// Restore the compositor's canonical premultiplied-alpha blend state.
    ///
    /// Every overlay shader that emits RGB multiplied by alpha shares this
    /// contract. Keeping the state identical across passes also prevents an
    /// overlay from silently changing how the following pass is composited.
    pub(crate) unsafe fn enable_premultiplied_blend(&self, gl: &ffi::Gles2) {
        let (src, dst) = premultiplied_blend_factors();
        unsafe {
            gl.Enable(ffi::BLEND);
            gl.BlendFunc(src, dst);
        }
    }

    /// Set the persistent window/border shader uniforms for passes rendered
    /// after the scene-linear FBO has been copied or encoded into output_fbo.
    ///
    /// The output remains linear only when KMS performs the final OETF. In all
    /// other cases output_fbo is already encoded and legacy overlay textures
    /// must not be decoded a second time.
    unsafe fn sync_overlay_color_domain(&self, gl: &ffi::Gles2, scene_linear_output: bool) {
        unsafe {
            gl.UseProgram(self.program);
            gl.Uniform1i(self.win_uniforms.color_managed, 0);
            gl.Uniform1i(
                self.win_uniforms.scene_linear,
                i32::from(scene_linear_output),
            );
            gl.Uniform1f(self.win_uniforms.ripple_progress, 0.0);
            gl.Uniform1f(self.win_uniforms.ripple_amplitude, 0.0);

            // Snap, expose, overview and recording overlays reuse the border
            // program even when ordinary window borders are disabled.
            gl.UseProgram(self.border_program);
            gl.Uniform1i(
                self.border_uniforms.scene_linear,
                i32::from(scene_linear_output),
            );
            gl.UseProgram(0);
        }
    }

    #[allow(dead_code)]
    fn draw_quad(&self, gl: &ffi::Gles2) {
        unsafe {
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
        }
    }

    unsafe fn reset_external_gl_state(&self, gl: &ffi::Gles2) {
        unsafe {
            gl.UseProgram(0);
            gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
            gl.BindVertexArray(0);
            gl.BindBuffer(ffi::ARRAY_BUFFER, 0);
            gl.BindBuffer(ffi::ELEMENT_ARRAY_BUFFER, 0);
            for attr in 0..8 {
                gl.DisableVertexAttribArray(attr);
            }
        }
    }

    pub(super) fn bind_window_texture(&self, gl: &ffi::Gles2, texture: u32) {
        unsafe {
            gl.BindTexture(ffi::TEXTURE_2D, texture);
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, ffi::LINEAR as i32);
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, ffi::LINEAR as i32);
            gl.TexParameteri(
                ffi::TEXTURE_2D,
                ffi::TEXTURE_WRAP_S,
                ffi::CLAMP_TO_EDGE as i32,
            );
            gl.TexParameteri(
                ffi::TEXTURE_2D,
                ffi::TEXTURE_WRAP_T,
                ffi::CLAMP_TO_EDGE as i32,
            );
        }
    }

    // =========================================================================
    // Helper: set a vec4 uniform (u_rect, etc.)
    // =========================================================================

    fn set_rect_uniform(&self, gl: &ffi::Gles2, loc: i32, x: f32, y: f32, w: f32, h: f32) {
        unsafe {
            gl.Uniform4f(loc, x, y, w, h);
        }
    }

    // =========================================================================
    // Helper: set a mat4 uniform (u_projection, etc.)
    // =========================================================================

    fn set_projection_uniform(&self, gl: &ffi::Gles2, loc: i32, proj: &[f32; 16]) {
        unsafe {
            gl.UniformMatrix4fv(loc, 1, ffi::FALSE as u8, proj.as_ptr());
        }
    }

    /// Draw windows that have left the live scene but are still fading out.
    ///
    /// Their `WindowState` owns a strong `GlesTexture`, so sampling remains
    /// valid after the Wayland surface and backend offscreen cache are gone.
    /// This is deliberately a separate overlay pass: retired windows no longer
    /// occur in `visible_scene` and therefore cannot use the main window loop.
    fn render_close_fades(
        &self,
        gl: &ffi::Gles2,
        projection: &[f32; 16],
        scene_linear_output: bool,
    ) {
        unsafe {
            gl.UseProgram(self.program);
            self.set_projection_uniform(gl, self.win_uniforms.projection, projection);
            gl.Uniform1i(self.win_uniforms.texture, 0);
            gl.Uniform1i(self.win_uniforms.color_managed, 0);
            // This pass runs after the optional scene-linear encode/blit. When
            // hardware will encode at scanout the output FBO is still linear,
            // so decode legacy sRGB texture data before blending into it.
            gl.Uniform1i(
                self.win_uniforms.scene_linear,
                if scene_linear_output { 1 } else { 0 },
            );
            gl.Uniform1f(self.win_uniforms.ripple_progress, 0.0);
            gl.Uniform1f(self.win_uniforms.ripple_amplitude, 0.0);
            gl.BindVertexArray(self.quad_vao);

            for win in self.windows.values() {
                if !win.fading_out || win.is_genie_minimizing || win.fade_opacity <= 0.0 {
                    continue;
                }
                let Some(texture_owner) = win.texture_owner.as_ref() else {
                    continue;
                };
                let Some((x, y, w, h)) = win.closing_rect else {
                    continue;
                };
                if w <= 0.0 || h <= 0.0 {
                    continue;
                }

                let layer_opacity = win
                    .opacity_override
                    .or_else(|| self.lookup_opacity_rule(&win.class_name))
                    .unwrap_or(self.active_opacity)
                    * win.fade_opacity;
                let layer_opacity = layer_opacity.clamp(0.0, 1.0);
                if layer_opacity <= 0.0 {
                    continue;
                }
                // Negative opacity tells the shared fragment shader to honor
                // texture alpha. RGB and alpha are both scaled by layer
                // opacity, matching GL_ONE/ONE_MINUS_SRC_ALPHA blending.
                let opacity = if win.has_alpha {
                    -layer_opacity
                } else {
                    layer_opacity
                };

                let scale = win.anim_scale.max(0.01);
                let draw_w = w * scale;
                let draw_h = h * scale;
                let draw_x = x + (w - draw_w) * 0.5;
                let draw_y = y + (h - draw_h) * 0.5;
                let radius = if win.is_shaped || win.is_fullscreen {
                    0.0
                } else {
                    win.corner_radius_override
                        .or_else(|| self.lookup_corner_radius_rule(&win.class_name))
                        .unwrap_or(self.corner_radius)
                };
                let [uv_x, uv_y, uv_w, uv_h] = oriented_content_uv(win.content_uv, win.y_inverted);

                self.set_rect_uniform(gl, self.win_uniforms.rect, draw_x, draw_y, draw_w, draw_h);
                gl.Uniform2f(self.win_uniforms.size, draw_w, draw_h);
                gl.Uniform1f(self.win_uniforms.opacity, opacity);
                gl.Uniform1f(self.win_uniforms.dim, 1.0);
                gl.Uniform1f(self.win_uniforms.radius, radius);
                gl.Uniform4f(self.win_uniforms.uv_rect, uv_x, uv_y, uv_w, uv_h);
                gl.ActiveTexture(ffi::TEXTURE0);
                self.bind_window_texture(gl, texture_owner.tex_id());
                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
            }

            gl.BindVertexArray(0);
            gl.UseProgram(0);
        }
    }

    // =========================================================================
    // Helper: blit one FBO into another
    // =========================================================================

    fn blit_fbo(&self, gl: &ffi::Gles2, src_fbo: u32, dst_fbo: u32, w: u32, h: u32) {
        unsafe {
            gl.BindFramebuffer(ffi::READ_FRAMEBUFFER, src_fbo);
            gl.BindFramebuffer(ffi::DRAW_FRAMEBUFFER, dst_fbo);
            gl.BlitFramebuffer(
                0,
                0,
                w as i32,
                h as i32,
                0,
                0,
                w as i32,
                h as i32,
                ffi::COLOR_BUFFER_BIT,
                ffi::NEAREST,
            );
        }
    }

    // SOTA #2 Phase 2.3: Linearize the currently-encoded output_fbo into
    // linear_fbo so subsequent window draws (with u_scene_linear=1) blend
    // correctly over the wallpaper/shadows. Called only when self.linear_fbo
    // != 0. Disables blending — this is a full overwrite.
    fn dispatch_scene_linear_decode_pass(&self, gl: &ffi::Gles2, projection: &[f32; 16]) {
        unsafe {
            gl.BindFramebuffer(ffi::FRAMEBUFFER, self.linear_fbo);
            gl.Viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
            gl.Disable(ffi::BLEND);
            gl.UseProgram(self.scene_linear_decode_program);
            self.set_projection_uniform(
                gl,
                self.scene_linear_decode_uniforms.projection,
                projection,
            );
            self.set_rect_uniform(
                gl,
                self.scene_linear_decode_uniforms.rect,
                0.0,
                0.0,
                self.screen_w as f32,
                self.screen_h as f32,
            );
            gl.Uniform1i(self.scene_linear_decode_uniforms.texture, 0);
            gl.ActiveTexture(ffi::TEXTURE0);
            gl.BindTexture(ffi::TEXTURE_2D, self.output_texture);
            gl.BindVertexArray(self.quad_vao);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
            gl.BindVertexArray(0);
            gl.UseProgram(0);
            gl.Enable(ffi::BLEND);
        }
    }

    // SOTA #2 Phase 2.3: Encode the FP16 linear_fbo back into output_fbo
    // using the output's forward EOTF. encode_tf < 0 means "sRGB default";
    // the shader's else-branch covers it. encode_gamma is only consulted
    // for TF_POWER. Disables blending.
    fn dispatch_scene_linear_encode_pass(
        &self,
        gl: &ffi::Gles2,
        projection: &[f32; 16],
        encode_tf: i32,
        encode_gamma: f32,
    ) {
        unsafe {
            gl.BindFramebuffer(ffi::FRAMEBUFFER, self.output_fbo);
            gl.Viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
            gl.Disable(ffi::BLEND);
            gl.UseProgram(self.scene_linear_encode_program);
            self.set_projection_uniform(
                gl,
                self.scene_linear_encode_uniforms.projection,
                projection,
            );
            self.set_rect_uniform(
                gl,
                self.scene_linear_encode_uniforms.rect,
                0.0,
                0.0,
                self.screen_w as f32,
                self.screen_h as f32,
            );
            gl.Uniform1i(self.scene_linear_encode_uniforms.texture, 0);
            gl.Uniform1i(self.scene_linear_encode_uniforms.encode_tf, encode_tf);
            gl.Uniform1f(self.scene_linear_encode_uniforms.encode_gamma, encode_gamma);
            gl.ActiveTexture(ffi::TEXTURE0);
            gl.BindTexture(ffi::TEXTURE_2D, self.linear_texture);
            gl.BindVertexArray(self.quad_vao);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
            gl.BindVertexArray(0);
            gl.UseProgram(0);
            gl.Enable(ffi::BLEND);
        }
    }

    /// Bounding box (top-left logical px) of everything that changed since the
    /// previous frame, or `None` to request a full redraw.
    ///
    /// SAFETY INVARIANT: the returned box must be a *superset* of every pixel of
    /// `output_fbo` that differs from the previous frame. Pixels outside it are
    /// left persisted from prior frames, so under-reporting shows stale content.
    /// Callers only invoke this on provably "calm" frames (no animation, blur,
    /// or effect overlays); here we additionally cover window geometry changes,
    /// content updates, and focus-driven border/opacity changes.
    fn compute_partial_damage_box(
        &self,
        scene: &[(u64, i32, i32, u32, u32)],
        focused: Option<u64>,
    ) -> Option<dirty_region::DirtyRect> {
        use dirty_region::DirtyRect;

        // Expand each window rect to cover its border and shadow footprint.
        let margin = self.border_width
            + if self.shadow_enabled && self.shadow_radius > 0.0 {
                self.shadow_spread
                    + self.shadow_radius
                    + self.shadow_offset[0].abs().max(self.shadow_offset[1].abs())
            } else {
                0.0
            };

        fn fold(acc: &mut Option<DirtyRect>, r: DirtyRect) {
            *acc = Some(match *acc {
                Some(a) => a.union(&r),
                None => r,
            });
        }
        let win_rect = |id: u64| -> Option<DirtyRect> {
            scene
                .iter()
                .find(|&&(wid, ..)| wid == id)
                .map(|&(_, x, y, w, h)| {
                    DirtyRect::new(x as f32, y as f32, w as f32, h as f32).expand(margin)
                })
        };

        let mut acc: Option<DirtyRect> = None;

        // Geometry changes (appear/disappear/move/resize), already tracked.
        for r in self.dirty_region_tracker.regions() {
            fold(&mut acc, r.expand(margin));
        }
        // Window content updates committed this frame.
        for &id in &self.content_dirty_ids {
            if let Some(r) = win_rect(id) {
                fold(&mut acc, r);
            }
        }
        // Focus change: border/opacity/dim differ on old and new focused windows.
        if focused != self.prev_focused {
            for fid in [focused, self.prev_focused].into_iter().flatten() {
                if let Some(r) = win_rect(fid) {
                    fold(&mut acc, r);
                }
            }
        }
        // Urgent windows draw an attention border that may toggle independently
        // of content; keep them in the box so it never goes stale.
        for &(id, x, y, w, h) in scene {
            if self.windows.get(&id).map_or(false, |ws| ws.is_urgent) {
                fold(
                    &mut acc,
                    DirtyRect::new(x as f32, y as f32, w as f32, h as f32).expand(margin),
                );
            }
        }

        let bbox = acc?;
        // Clamp to screen bounds.
        let x0 = bbox.x.max(0.0);
        let y0 = bbox.y.max(0.0);
        let x1 = (bbox.x + bbox.width).min(self.screen_w as f32);
        let y1 = (bbox.y + bbox.height).min(self.screen_h as f32);
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        let clamped = DirtyRect::new(x0, y0, x1 - x0, y1 - y0);
        // Scissoring a near-full-screen box is not worth the bookkeeping.
        let screen_area = (self.screen_w as f32) * (self.screen_h as f32);
        if clamped.area() >= 0.7 * screen_area {
            return None;
        }
        Some(clamped)
    }

    fn sync_scene_linear_target(&mut self, gl: &ffi::Gles2) {
        let allocated = self.linear_fbo != 0;
        if allocated == self.scene_linear_requested {
            return;
        }

        unsafe {
            if allocated {
                gl.DeleteFramebuffers(1, &self.linear_fbo);
                gl.DeleteTextures(1, &self.linear_texture);
                self.linear_fbo = 0;
                self.linear_texture = 0;
            } else {
                match create_fbo_texture_fp16(gl, self.screen_w.max(1), self.screen_h.max(1)) {
                    Ok((fbo, texture)) => {
                        self.linear_fbo = fbo;
                        self.linear_texture = texture;
                    }
                    Err(status) => {
                        // Do not retry every frame, and more importantly do not
                        // let damage/KMS color-offload code mistake an
                        // incomplete target for an active linear pipeline.
                        self.scene_linear_requested = false;
                        log::warn!(
                            "[udev/compositor] scene-linear hot-enable failed \
                             (RGBA16F FBO status=0x{status:x}); keeping encoded-space pipeline"
                        );
                    }
                }
            }
        }
        // The storage and color domain changed, so no previous partial-damage
        // contents can be reused across this boundary.
        self.force_full_damage_next = true;
        self.needs_render = true;
    }

    /// Main rendering function. Composites the entire scene into the output FBO.
    /// `scene` is a list of (window_id, x, y, w, h) in bottom-to-top order.
    /// `focused` is the currently focused window.
    /// `hw_encode_active`: when true, the per-output CRTC `GAMMA_LUT` will
    /// OETF-encode at scanout, so the shader-side encode pass must be skipped
    /// (otherwise we double-encode).
    /// `shader_encode_tf` / `shader_encode_gamma`: TF parameters used by the
    /// shader encode pass when `hw_encode_active` is false. Ignored otherwise.
    /// Returns true if a frame was rendered (false if skipped due to no changes).
    pub(crate) fn render_frame(
        &mut self,
        gl: &ffi::Gles2,
        scene: &[(u64, i32, i32, u32, u32)],
        focused: Option<u64>,
        hw_encode_active: bool,
        shader_encode_tf: i32,
        shader_encode_gamma: f32,
    ) -> bool {
        // A calm desktop must be cheap even when the backend asks us to check
        // for a frame.  Do this before profiler/fence/hot-reload bookkeeping:
        // those are useful only when a frame can actually be produced.  The
        // animation check keeps time-based effects live without relying on a
        // separate caller to keep `needs_render` armed.
        let recording_transition_pending =
            self.pending_recording_start.is_some() || self.pending_recording_stop;
        // Static post-processing is damage-driven. Magnifier pointer changes
        // explicitly dirty the compositor in set_mouse_position, so no current
        // post-process input advances merely because time passes.
        let postprocess_continuous =
            postprocess_requires_continuous_frames(self.postprocess_active, false);
        // Edge glow is genuinely time-based (`u_time`) and therefore keeps
        // ticking, but only while the draw pass can produce visible pixels.
        let edge_glow_continuous = edge_glow_requires_continuous_frames(
            self.edge_glow_enabled,
            self.edge_glow_width,
            self.edge_glow_active,
            self.edge_glow_suppressed,
        );
        if !self.needs_render
            && !self.screenshot_requests.has_pending()
            && !self.screenshot_readback.has_pending()
            && !self.recording.is_active()
            && !recording_transition_pending
            && !postprocess_continuous
            && !edge_glow_continuous
            && !self.has_active_animations()
        {
            return false;
        }

        self.sync_scene_linear_target(gl);

        // output_fbo still contains the previous workspace at this point. Take
        // the transition snapshot before any clear or scene pass overwrites it;
        // deferring this to the transition overlay would capture the new
        // workspace and make every transition sample an uninitialized/stale FBO.
        if self.transition_snapshot_pending {
            self.capture_transition_snapshot(gl);
            self.transition_snapshot_pending = false;
        }

        // =================================================================
        // 0. Performance infrastructure - frame start
        // =================================================================
        self.frame_profiler.begin_frame();
        self.gl_state_tracker.reset();

        // GPU fence sync: poll pending fences, cleanup old ones
        unsafe {
            self.gpu_fence_sync_mgr.update_fence_states(gl);
            self.gpu_fence_sync_mgr.cleanup_old_fences(gl);
        }

        // Power saving: periodic update (every 5s)
        if self.power_saving_mgr.update() {
            let recs = self.power_saving_mgr.get_recommendations();
            self.adaptive_frame_rate
                .limiter_mut()
                .set_target_fps(recs.fps_limit);
        }

        // Shader hot-reload: check for modified shader files
        let reloaded_shaders = self.shader_hot_reload.poll();
        if !reloaded_shaders.is_empty() {
            log::info!(
                "[compositor] Shader hot-reload: {} shaders changed",
                reloaded_shaders.len()
            );
        }

        // Direct scanout eligibility tracking (stats only).
        //
        // The actual zero-copy bypass happens at the KMS level in udev_kms.rs
        // (`direct_scanout_eligible`): when one fullscreen window owns the
        // output, smithay's DrmCompositor skips our FBO entirely and assigns
        // the client surface to the primary plane. Our GL composite work is
        // still done here because we don't know in advance whether KMS will
        // actually accept the plane assignment (format/modifier mismatch
        // would force smithay's GL fallback — which uses our FBO).
        //
        // Previously this site also returned early when eligibility held,
        // skipping the GL composite. That was unsafe: if KMS could not take
        // the fast path (e.g. cursor moved between this decision and the
        // KMS render), smithay would scan out a stale FBO. SOTA #4 Phase 4.1
        // removed the early return; we now only track eligibility for metrics.
        if !self.transition_active
            && !self.overview_active
            && !self.expose_active
            && !self.postprocess_active
            && !self.recording_requires_composition()
        {
            let mut scanout_windows = std::mem::take(&mut self.scratch_scanout);
            scanout_windows.clear();
            for &(win_id, x, y, w, h) in scene {
                if let Some(ws) = self.windows.get(&win_id) {
                    scanout_windows.push((
                        win_id,
                        direct_scanout::WindowScanoutInfo {
                            x,
                            y,
                            width: w,
                            height: h,
                            is_fullscreen: ws.is_fullscreen,
                            has_alpha: ws.has_alpha,
                            has_blur: ws.is_frosted,
                            has_shadow: self.shadow_enabled,
                            corner_radius: ws.corner_radius_override.unwrap_or(self.corner_radius),
                            opacity: ws.fade_opacity,
                        },
                    ));
                }
            }
            let _ = self
                .direct_scanout_mgr
                .check_scene(&scanout_windows, focused);
            self.scratch_scanout = scanout_windows;
        }

        // =================================================================
        // 1. Frame timing
        // =================================================================
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame_time).as_secs_f32();
        self.last_frame_time = now;
        let effect_dt = crate::backend::compositor_common::effects::continuing_effect_dt(
            self.effect_clock_active,
            dt,
        );

        // Update FPS counter and perf metrics
        self.frame_count += 1;
        if self.frame_count % 60 == 0 {
            self.fps = if dt > 0.0 { 1.0 / dt } else { 0.0 };
        }
        self.perf_metrics
            .record_frame(std::time::Duration::from_secs_f32(dt));

        // =================================================================
        // 1b. Dirty region tracking: compare current scene vs previous frame
        // =================================================================
        {
            // Reuse persistent scratch buffers: current-frame id set + previous
            // geometry-by-id map. Avoids two per-frame HashSet allocations and
            // turns the move/resize lookup from O(N^2) linear scan into O(N).
            self.scratch_curr_ids.clear();
            self.scratch_curr_ids
                .extend(scene.iter().map(|&(id, _, _, _, _)| id));

            self.scratch_prev_geom.clear();
            for &(id, x, y, w, h) in &self.prev_scene {
                self.scratch_prev_geom.insert(id, (x, y, w, h));
            }

            // Windows that disappeared — mark their old rect dirty
            for &(id, x, y, w, h) in &self.prev_scene {
                if !self.scratch_curr_ids.contains(&id) {
                    self.dirty_region_tracker
                        .mark_dirty(dirty_region::DirtyRect::new(
                            x as f32, y as f32, w as f32, h as f32,
                        ));
                }
            }

            // Windows that appeared or moved/resized
            for &(id, x, y, w, h) in scene {
                match self.scratch_prev_geom.get(&id) {
                    None => {
                        // New window
                        self.dirty_region_tracker
                            .mark_dirty(dirty_region::DirtyRect::new(
                                x as f32, y as f32, w as f32, h as f32,
                            ));
                    }
                    Some(&(px, py, pw, ph)) => {
                        if x != px || y != py || w != pw || h != ph {
                            // Moved or resized — mark both old and new rects
                            self.dirty_region_tracker
                                .mark_dirty(dirty_region::DirtyRect::new(
                                    px as f32, py as f32, pw as f32, ph as f32,
                                ));
                            self.dirty_region_tracker
                                .mark_dirty(dirty_region::DirtyRect::new(
                                    x as f32, y as f32, w as f32, h as f32,
                                ));
                        }
                    }
                }
            }

            self.prev_scene.clear();
            self.prev_scene.extend_from_slice(scene);
        }

        // Feed dirty regions to per-monitor renderer
        {
            // Borrow the tracker's deque directly instead of collecting into a
            // fresh Vec every frame. VecDeque exposes its (up to two) contiguous
            // slices; marking from each is equivalent to one combined call.
            let regions = self.dirty_region_tracker.regions();
            if regions.is_empty() {
                // No tracked dirty regions yet — mark all monitors dirty (full redraw)
                self.per_monitor_renderer.mark_all_dirty();
            } else {
                let (front, back) = regions.as_slices();
                self.per_monitor_renderer.mark_dirty_from_regions(front);
                if !back.is_empty() {
                    self.per_monitor_renderer.mark_dirty_from_regions(back);
                }
            }
            self.per_monitor_renderer.next_frame();
        }

        // =================================================================
        // 2. Animation ticks
        // =================================================================
        self.tick_fades(effect_dt);
        self.tick_genie();
        self.tick_wobbly(effect_dt);
        self.tick_particles(effect_dt);
        self.tick_motion_trails();
        self.tick_snap_preview(effect_dt);
        self.tick_overview(effect_dt);
        self.tick_overview_prism(effect_dt);
        self.tilt_target_x = 0.0;
        self.tilt_target_y = 0.0;
        if self.window_tilt_enabled
            && let Some(focused_id) = focused
            && let Some(&(_, x, y, w, h)) = scene.iter().find(|&&(id, _, _, _, _)| id == focused_id)
        {
            let draw_w = w.max(1) as f32;
            let draw_h = h.max(1) as f32;
            let inside = self.mouse_x >= x as f32
                && self.mouse_x <= x as f32 + draw_w
                && self.mouse_y >= y as f32
                && self.mouse_y <= y as f32 + draw_h;
            if inside {
                let cx = x as f32 + draw_w * 0.5;
                let cy = y as f32 + draw_h * 0.5;
                let rel_x = ((self.mouse_x - cx) / (draw_w * 0.5)).clamp(-1.0, 1.0);
                let rel_y = ((self.mouse_y - cy) / (draw_h * 0.5)).clamp(-1.0, 1.0);
                self.tilt_target_x = (-rel_y * self.tilt_amount).clamp(-0.35, 0.35);
                self.tilt_target_y = (rel_x * self.tilt_amount).clamp(-0.35, 0.35);
            }
        }
        self.tick_tilt(effect_dt);
        self.tick_expose(effect_dt);
        self.effect_clock_active = self.has_active_animations();

        // Focus highlight: arm a one-shot pulse on the new focus.
        // Done before any_animating so the highlight keeps the loop ticking
        // until the duration expires, instead of stalling on the first frame.
        if self.focus_highlight_enabled && focused != self.prev_focused {
            if let Some(fw) = focused {
                self.focus_highlight_start = Some((fw, Instant::now()));
            }
        }
        let focus_highlight_active = self.focus_highlight_enabled
            && self
                .focus_highlight_start
                .map(|(_, start)| {
                    (start.elapsed().as_millis() as u64) < self.focus_highlight_duration_ms
                })
                .unwrap_or(false);
        // Motion trail keeps the loop ticking until trails drain to empty,
        // even if the user has stopped moving the window.
        let motion_trail_active =
            self.motion_trail_enabled && self.windows.values().any(|w| !w.motion_trail.is_empty());

        // Determine if anything needs rendering
        let any_animating = self.has_active_animations()
            || self.transition_active
            || focus_highlight_active
            || motion_trail_active
            || !self.genie_active.is_empty();

        // These operations need a frame even on an otherwise static desktop.
        // Keep the demand calculation next to the other compositor work so
        // KMS scheduling, screenshots, and recording all agree on liveness.
        let screenshot_pending =
            self.screenshot_requests.has_pending() || self.screenshot_readback.has_pending();
        let recording_active = self.recording.is_active();

        let force_render = any_animating
            || postprocess_continuous
            || self.debug_hud_enabled
            || edge_glow_continuous
            || screenshot_pending
            || recording_active;

        // Texture existence is stable after a window's first frame and must
        // not keep the render loop alive. Only content committed since the
        // previous frame is dirty here; geometry damage is tracked above.
        let has_dirty =
            !self.content_dirty_ids.is_empty() || !self.dirty_region_tracker.regions().is_empty();

        // Skip frame if nothing changed
        if !self.needs_render && !force_render && !has_dirty {
            return false;
        }
        // If animations are still running, keep the flag set so the next
        // tick_animations call re-invokes compositor_render_frame automatically.
        self.needs_render = any_animating
            || self.has_active_animations()
            || postprocess_continuous
            || edge_glow_continuous
            || recording_active
            || self.screenshot_readback.has_pending();

        // Rate-limited diagnostic logging (once per second when scene is non-empty)
        static LAST_RF_LOG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let rf_log_this = log::log_enabled!(log::Level::Debug) && !scene.is_empty() && {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let prev = LAST_RF_LOG.load(std::sync::atomic::Ordering::Relaxed);
            if now > prev {
                LAST_RF_LOG.store(now, std::sync::atomic::Ordering::Relaxed);
                true
            } else {
                false
            }
        };
        if rf_log_this {
            log::debug!(
                "[rf] windows={} scene={} force={force_render} dirty={has_dirty}",
                self.windows.len(),
                scene.len()
            );
            for &(win_id, x, y, w, h) in scene {
                if let Some(ws) = self.windows.get(&win_id) {
                    log::debug!(
                        "[rf] win={win_id:#x} tex={:?} fade={:.3} pos=({x},{y}) size={w}x{h} y_inv={}",
                        ws.gl_texture,
                        ws.fade_opacity,
                        ws.y_inverted
                    );
                } else {
                    log::debug!(
                        "[rf] win={win_id:#x} NOT in compositor.windows pos=({x},{y}) size={w}x{h}"
                    );
                }
            }
        }

        // =================================================================
        // 2b. Partial-damage decision (experimental, default off)
        // =================================================================
        // Only scissor on provably "calm" frames: no animation, no blur, no
        // effect overlays, no tilt. Everything excluded here either redraws the
        // whole screen continuously or samples regions outside any damage box.
        let blur_would_run = self.blur_enabled
            && scene
                .iter()
                .any(|&(win_id, ..)| self.windows.get(&win_id).map_or(false, |ws| ws.is_frosted));
        let allow_partial = self.partial_damage_enabled
            && !self.force_full_damage_next
            && !any_animating
            && !force_render
            && !self.peek_active
            && !self.postprocess_active
            && self.overview_opacity <= 0.0001
            && self.expose_opacity <= 0.0001
            && self.expose_entries.is_empty()
            && (!self.window_tabs_enabled || self.window_groups.is_empty())
            && !self.annotation_active
            && self.tilt_x.abs() <= 0.001
            && self.tilt_y.abs() <= 0.001
            && !blur_would_run;
        let partial_box = if allow_partial {
            self.compute_partial_damage_box(scene, focused)
        } else {
            None
        };
        // Consumed for this frame; next frame may go partial again.
        self.force_full_damage_next = false;

        // =================================================================
        // 3. Setup projection matrix
        // =================================================================
        let projection = ortho(0.0, self.screen_w as f32, self.screen_h as f32, 0.0);

        // =================================================================
        // 4. Bind output FBO and clear
        // =================================================================
        unsafe {
            gl.BindFramebuffer(ffi::FRAMEBUFFER, self.output_fbo);
            gl.Viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
            self.enable_premultiplied_blend(gl);
        }

        // Restrict all output_fbo passes (clear, wallpaper, shadows, windows,
        // borders) to the damage box. Regions outside persist from prior frames.
        // GL scissor uses a bottom-left origin; our draw coords are top-left.
        let damage_scissor = partial_box.map(|b| {
            let sx = b.x.floor().max(0.0) as i32;
            let sw = b.width.ceil() as i32;
            let sh = b.height.ceil() as i32;
            let sy = ((self.screen_h as i32) - (b.y.floor() as i32) - sh).max(0);
            [sx, sy, sw.max(0), sh.max(0)]
        });
        let scissor_active = if let Some(scissor) = damage_scissor {
            unsafe {
                gl.Enable(ffi::SCISSOR_TEST);
                gl.Scissor(scissor[0], scissor[1], scissor[2], scissor[3]);
            }
            true
        } else {
            false
        };

        // =================================================================
        // 5. Draw background (dark blue-grey) + wallpaper
        // =================================================================
        unsafe {
            gl.ClearColor(0.1, 0.15, 0.25, 1.0);
            gl.Clear(ffi::COLOR_BUFFER_BIT);
        }

        // Poll pending wallpaper loads and render wallpaper if set
        unsafe {
            self.poll_pending_wallpapers(gl);
        }
        if self.wallpaper_texture.is_some() || !self.monitor_wallpapers.is_empty() {
            unsafe {
                self.render_wallpaper(gl, &projection, damage_scissor);
            }
        }

        // VRR: update state based on focused window
        self.update_vrr_state(focused);

        // =================================================================
        // 6. Occlusion culling - find lowest fully-opaque window covering screen
        // =================================================================
        let mut first_visible = 0usize;
        {
            for i in (0..scene.len()).rev() {
                let (win_id, x, y, w, h) = scene[i];
                let Some(ws) = self.windows.get(&win_id) else {
                    continue;
                };
                let is_focused = focused == Some(win_id);
                let base_opacity = if is_focused {
                    self.active_opacity
                } else {
                    self.inactive_opacity
                };
                let effective_opacity = ws
                    .opacity_override
                    .or_else(|| self.lookup_opacity_rule(&ws.class_name))
                    .unwrap_or(base_opacity)
                    * ws.fade_opacity;
                let corner_radius = if ws.is_shaped || ws.is_fullscreen {
                    0.0
                } else if !ws.class_name.is_empty()
                    && Self::class_matches_exclude(&ws.class_name, &self.rounded_corners_exclude)
                {
                    0.0
                } else {
                    ws.corner_radius_override
                        .or_else(|| self.lookup_corner_radius_rule(&ws.class_name))
                        .unwrap_or(self.corner_radius)
                };
                let focused_tilt_active =
                    is_focused && (self.tilt_x.abs() > 0.001 || self.tilt_y.abs() > 0.001);

                if is_opaque_output_occluder(OcclusionCandidate {
                    rect: (x, y, w, h),
                    screen_size: (self.screen_w, self.screen_h),
                    has_alpha: ws.has_alpha,
                    fade_opacity: ws.fade_opacity,
                    effective_opacity,
                    anim_scale: ws.anim_scale,
                    window_scale: ws.scale,
                    corner_radius,
                    is_shaped: ws.is_shaped,
                    has_wobbly_deformation: ws.wobbly.is_some(),
                    ripple_active: ws.ripple_active,
                    focused_tilt_active,
                    // Frosted windows require the complete lower scene as
                    // their blur source even if their own output is opaque.
                    samples_background: self.blur_enabled && ws.is_frosted,
                }) {
                    first_visible = i;
                    break;
                }
            }
        }

        let visible_scene = &scene[first_visible..];

        // =================================================================
        // 7. Draw shadows
        // =================================================================
        self.frame_profiler.zone_start("shadows");
        if self.shadow_enabled && self.shadow_radius > 0.0 {
            unsafe {
                gl.UseProgram(self.shadow_program);
                self.set_projection_uniform(gl, self.shadow_uniforms.projection, &projection);
                gl.BindVertexArray(self.quad_vao);

                let spread = self.shadow_spread;
                let [ox, oy] = self.shadow_offset;
                let [sr, sg, sb, sa] = self.shadow_color;

                gl.Uniform1f(self.shadow_uniforms.spread, spread);

                for &(win_id, x, y, w, h) in visible_scene {
                    let wt = match self.windows.get(&win_id) {
                        Some(wt) => wt,
                        None => continue,
                    };

                    // Skip shaped / fullscreen windows
                    if wt.is_shaped || wt.is_fullscreen {
                        continue;
                    }

                    // Skip windows in shadow_exclude list
                    if !wt.class_name.is_empty()
                        && Self::class_matches_exclude(&wt.class_name, &self.shadow_exclude)
                    {
                        continue;
                    }

                    // Modulate shadow alpha by fade
                    let fade = wt.fade_opacity;
                    let sa_faded = sa * fade;
                    if sa_faded <= 0.0 {
                        continue;
                    }

                    gl.Uniform4f(self.shadow_uniforms.shadow_color, sr, sg, sb, sa_faded);

                    // Per-window corner radius
                    let win_radius = wt.corner_radius_override.unwrap_or(self.corner_radius);
                    gl.Uniform1f(self.shadow_uniforms.radius, win_radius);

                    // Shadow rect: expanded by spread + offset
                    let sx = x as f32 + ox - spread;
                    let sy = y as f32 + oy - spread;
                    let sw = w as f32 + 2.0 * spread;
                    let sh = h as f32 + 2.0 * spread;

                    self.set_rect_uniform(gl, self.shadow_uniforms.rect, sx, sy, sw, sh);
                    gl.Uniform2f(self.shadow_uniforms.size, w as f32, h as f32);

                    gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                }

                gl.BindVertexArray(0);
                gl.UseProgram(0);
            }
        }

        self.frame_profiler.zone_end();

        // =================================================================
        // 8. Blur pass (for frosted/translucent windows)
        // =================================================================
        self.frame_profiler.zone_start("blur");
        let has_frosted = visible_scene.iter().any(|&(win_id, _, _, _, _)| {
            self.windows.get(&win_id).map_or(false, |ws| {
                ws.is_frosted
                    && (ws.class_name.is_empty()
                        || !Self::class_matches_exclude(&ws.class_name, &self.blur_exclude))
            })
        });

        let blur_result_tex = if self.blur_enabled && has_frosted && !self.blur_fbos.is_empty() {
            self.temporal_blur_total_count += 1;

            let current_hash = self.compute_window_positions_hash();
            let can_reuse = self.temporal_blur_enabled
                && current_hash == self.prev_window_positions_hash
                && self.prev_blur_fbo.is_some();

            let tex = if can_reuse {
                self.temporal_blur_reuse_count += 1;
                self.prev_blur_fbo.unwrap().1
            } else {
                // Capture current scene to scene_fbo
                self.blit_fbo(
                    gl,
                    self.output_fbo,
                    self.scene_fbo,
                    self.screen_w,
                    self.screen_h,
                );

                // Run blur downsample/upsample passes. Per-window quality:
                // pick the highest quality among visible frosted windows so
                // focused windows stay sharp while unfocused/off-screen ones
                // don't drive cost up.
                let blur_quality = self.compute_max_visible_blur_quality(visible_scene, focused);
                self.run_blur_passes(gl, self.scene_texture, &projection, blur_quality);

                // Record blur operation for cache warmup statistics
                self.cache_warmup_mgr
                    .record_blur_operation(self.screen_w, self.screen_h);

                let result = self.blur_fbos[0].texture;

                // Temporal mix: blend a motion-scaled amount of the previous
                // blur into the fresh result to reduce frame-to-frame shimmer.
                // On large motion the ratio decays to ~0 (pure current) to avoid
                // ghosting. The displayed result is fed back as the new history
                // (exponential moving average).
                let display_tex = if self.temporal_blur_enabled {
                    let ratio = self.temporal_mix_ratio_for_motion(visible_scene);
                    let mixed = match self.prev_blur_fbo {
                        Some((_, prev_tex)) if ratio > 0.001 => unsafe {
                            self.run_temporal_mix(gl, result, prev_tex, ratio)
                        },
                        _ => result,
                    };
                    unsafe {
                        self.copy_blur_to_prev_fbo(gl, mixed);
                    }
                    mixed
                } else {
                    result
                };

                self.prev_window_positions_hash = current_hash;
                display_tex
            };

            // Re-bind output FBO for further drawing
            unsafe {
                gl.BindFramebuffer(ffi::FRAMEBUFFER, self.output_fbo);
                gl.Viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
            }
            Some(tex)
        } else {
            None
        };

        self.frame_profiler.zone_end();

        // Motion trail: sample per-window position into a ring buffer.
        // Pre-pass before the immutable draw loop so we can take &mut on the
        // window state. When the position is unchanged we pop one entry per
        // frame so the trail naturally drains to empty after the window stops
        // (mirroring X11 effects.rs::update_motion_trail semantics — only there
        // it relies on geometry-sync side effects, here we do it inline).
        if self.motion_trail_enabled && self.motion_trail_frames > 0 {
            let cap = crate::backend::compositor_common::effects::motion_trail_capacity(
                self.motion_trail_frames,
            );
            for &(win_id, x, y, _, _) in visible_scene {
                if let Some(wt) = self.windows.get_mut(&win_id) {
                    let current = (x, y);
                    if wt.is_moving
                        && let Some((previous_x, previous_y)) = wt.last_motion_position
                        && (previous_x, previous_y) != current
                    {
                        wt.motion_trail.push_back(
                            crate::backend::compositor_common::effects::MotionTrailSample::new(
                                previous_x, previous_y,
                            ),
                        );
                        while wt.motion_trail.len() > cap {
                            wt.motion_trail.pop_front();
                        }
                    }
                    wt.last_motion_position = Some(current);
                }
            }
        }

        // =================================================================
        // 9. Draw windows (back-to-front)
        // =================================================================
        // SOTA #2 Phase 2.3: when scene-linear compositing is active, decode
        // the currently-encoded output_fbo (wallpaper + shadows + blur) into
        // linear_fbo, then route the window-draw pass there. The encode pass
        // after the loop converts back to encoded space for genie/borders/
        // effects (which still draw in encoded space in v2.3).
        let scene_linear_active = self.linear_fbo != 0;
        if scene_linear_active {
            self.dispatch_scene_linear_decode_pass(gl, &projection);
        }
        self.frame_profiler.zone_start("windows");
        unsafe {
            gl.UseProgram(self.program);
            if scene_linear_active {
                gl.BindFramebuffer(ffi::FRAMEBUFFER, self.linear_fbo);
                gl.Viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
            }
            self.set_projection_uniform(gl, self.win_uniforms.projection, &projection);
            gl.Uniform1i(self.win_uniforms.texture, 0);
            gl.Uniform4f(self.win_uniforms.uv_rect, 0.0, 0.0, 1.0, 1.0);
            gl.Uniform1f(self.win_uniforms.ripple_amplitude, 0.0);
            // Default off — only the per-window standard draw path conditionally
            // enables color management. Ancillary draws (blur/ghost) share this
            // program and must not inherit a stale transform.
            gl.Uniform1i(self.win_uniforms.color_managed, 0);
            gl.Uniform1i(
                self.win_uniforms.scene_linear,
                if scene_linear_active { 1 } else { 0 },
            );
            gl.BindVertexArray(self.quad_vao);

            for &(win_id, x, y, w, h) in visible_scene {
                let wt = match self.windows.get(&win_id) {
                    Some(wt) => wt,
                    None => continue,
                };

                let texture = match wt.gl_texture {
                    Some(tex) => tex,
                    None => continue,
                };

                let is_focused = focused == Some(win_id);
                let fade = wt.fade_opacity;
                if fade <= 0.0 {
                    continue;
                }

                // --- Compute effective opacity (per-window rules override) ---
                let base_opacity = if is_focused {
                    self.active_opacity
                } else {
                    self.inactive_opacity
                };
                let class_opacity = self.lookup_opacity_rule(&wt.class_name);
                let rule_opacity = wt
                    .opacity_override
                    .or(class_opacity)
                    .unwrap_or(base_opacity);
                let has_explicit_transparency = rule_opacity < 1.0;
                let use_texture_alpha =
                    wt.has_alpha && !(wt.is_moving && !has_explicit_transparency);

                // --- Compute dim factor ---
                let inactive_dim_factor = if is_focused { 1.0 } else { self.inactive_dim };
                let dim = inactive_dim_factor;
                let layer_opacity = (rule_opacity * fade).clamp(0.0, 1.0);
                let opacity = if use_texture_alpha {
                    -layer_opacity
                } else {
                    layer_opacity
                };

                // --- Compute corner radius (per-window rules override) ---
                let radius = if wt.is_shaped || wt.is_fullscreen {
                    0.0
                } else if !wt.class_name.is_empty()
                    && Self::class_matches_exclude(&wt.class_name, &self.rounded_corners_exclude)
                {
                    0.0
                } else {
                    wt.corner_radius_override
                        .or_else(|| self.lookup_corner_radius_rule(&wt.class_name))
                        .unwrap_or(self.corner_radius)
                };

                // --- Compute scale from animation ---
                let scale = wt.anim_scale;
                let (draw_x, draw_y, draw_w, draw_h) = if (scale - 1.0).abs() > f32::EPSILON {
                    let cw = w as f32 * scale;
                    let ch = h as f32 * scale;
                    let cx = x as f32 + (w as f32 - cw) * 0.5;
                    let cy = y as f32 + (h as f32 - ch) * 0.5;
                    (cx, cy, cw, ch)
                } else {
                    (x as f32, y as f32, w as f32, h as f32)
                };

                // --- UV rect: use content_uv (accounts for CSD geometry offset) ---
                let [uv_x, uv_y, uv_w, uv_h] = oriented_content_uv(wt.content_uv, wt.y_inverted);

                // --- Draw blur behind frosted window ---
                if wt.is_frosted && self.blur_enabled && blur_result_tex.is_some() {
                    let blur_tex = blur_result_tex.unwrap();
                    gl.ActiveTexture(ffi::TEXTURE0);
                    gl.BindTexture(ffi::TEXTURE_2D, blur_tex);

                    // UV coordinates for the window's screen region
                    let uv_sx = draw_x / self.screen_w as f32;
                    let uv_sy = draw_y / self.screen_h as f32;
                    let uv_sw = draw_w / self.screen_w as f32;
                    let uv_sh = draw_h / self.screen_h as f32;

                    // Per-window frosted strength modulates blur opacity
                    let blur_opacity = fade * wt.frosted_strength.max(0.1);

                    gl.Uniform4f(self.win_uniforms.uv_rect, uv_sx, uv_sy, uv_sw, uv_sh);
                    gl.Uniform1f(self.win_uniforms.opacity, blur_opacity);
                    gl.Uniform1f(self.win_uniforms.dim, 1.0);
                    gl.Uniform1f(self.win_uniforms.radius, radius);
                    gl.Uniform2f(self.win_uniforms.size, draw_w, draw_h);
                    self.set_rect_uniform(
                        gl,
                        self.win_uniforms.rect,
                        draw_x,
                        draw_y,
                        draw_w,
                        draw_h,
                    );
                    gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);

                    // Restore UV for the actual window texture
                    gl.Uniform4f(self.win_uniforms.uv_rect, uv_x, uv_y, uv_w, uv_h);
                }

                // --- Motion trail ghost copies (Phase 3.1, mirrors X11) ---
                // Draw historical positions with decreasing opacity *before* the
                // main texture so the live window paints on top of its trail.
                // Skips wobbly/tilt windows because the ghost would not match the
                // deformed shader output; trails on plain moving windows are the
                // common case and visually consistent with X11.
                if self.motion_trail_enabled && !wt.motion_trail.is_empty() && wt.wobbly.is_none() {
                    let trail_len = wt.motion_trail.len();
                    let trail_now = std::time::Instant::now();
                    let trail_lifetime =
                        crate::backend::compositor_common::effects::motion_trail_lifetime(
                            self.motion_trail_frames,
                        );
                    gl.Uniform4f(self.win_uniforms.uv_rect, uv_x, uv_y, uv_w, uv_h);
                    gl.ActiveTexture(ffi::TEXTURE0);
                    gl.BindTexture(ffi::TEXTURE_2D, texture);
                    gl.Uniform1f(self.win_uniforms.radius, radius);
                    gl.Uniform2f(self.win_uniforms.size, draw_w, draw_h);
                    for (i, sample) in wt.motion_trail.iter().enumerate() {
                        let ghost_opacity = self.motion_trail_opacity * (i as f32 + 1.0)
                            / trail_len as f32
                            * sample.opacity_at(trail_now, trail_lifetime);
                        if ghost_opacity <= 0.001 {
                            continue;
                        }
                        let ghost_layer = (ghost_opacity * layer_opacity).clamp(0.0, 1.0);
                        gl.Uniform1f(
                            self.win_uniforms.opacity,
                            if use_texture_alpha {
                                -ghost_layer
                            } else {
                                ghost_layer
                            },
                        );
                        gl.Uniform1f(self.win_uniforms.dim, 0.7);
                        self.set_rect_uniform(
                            gl,
                            self.win_uniforms.rect,
                            sample.x as f32,
                            sample.y as f32,
                            draw_w,
                            draw_h,
                        );
                        gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                    }
                    // Restore main-pass uniforms; opacity/dim are written below
                    // anyway, but keep the texture bound for the standard draw.
                }

                // --- Choose shader: wobbly, tilt, or standard ---
                if wt.wobbly.is_some() && !wt.ripple_active && wt.color_transform.is_none() {
                    // Wobbly windows: switch to wobbly program
                    let wobbly = wt.wobbly.as_ref().unwrap();
                    gl.UseProgram(self.wobbly_program);
                    self.set_projection_uniform(gl, self.wobbly_uniforms.projection, &projection);
                    self.set_rect_uniform(
                        gl,
                        self.wobbly_uniforms.rect,
                        draw_x,
                        draw_y,
                        draw_w,
                        draw_h,
                    );
                    gl.Uniform1i(self.wobbly_uniforms.texture, 0);
                    gl.Uniform1f(self.wobbly_uniforms.opacity, opacity);
                    gl.Uniform1f(self.wobbly_uniforms.radius, radius);
                    gl.Uniform2f(self.wobbly_uniforms.size, draw_w, draw_h);
                    gl.Uniform1f(self.wobbly_uniforms.dim, dim);
                    gl.Uniform4f(self.wobbly_uniforms.uv_rect, uv_x, uv_y, uv_w, uv_h);
                    gl.Uniform1i(self.wobbly_uniforms.color_managed, 0);
                    gl.Uniform1i(
                        self.wobbly_uniforms.scene_linear,
                        if scene_linear_active { 1 } else { 0 },
                    );

                    // Upload grid offsets as flat vec2 array, reusing a
                    // persistent scratch buffer instead of allocating per frame.
                    let flat = &mut self.scratch_wobbly_flat;
                    flat.clear();
                    flat.reserve(wobbly.offsets.len() * 2);
                    for o in &wobbly.offsets {
                        flat.push(o[0]);
                        flat.push(o[1]);
                    }
                    gl.Uniform2fv(
                        self.wobbly_uniforms.grid_offsets,
                        flat.len() as i32 / 2,
                        flat.as_ptr(),
                    );
                    let grid_n = wobbly.grid_n as i32;
                    gl.Uniform1i(self.wobbly_uniforms.grid_n, grid_n);

                    gl.ActiveTexture(ffi::TEXTURE0);
                    self.bind_window_texture(gl, texture);
                    // Grid: (grid_n-1)^2 quads, 6 verts each
                    let quads = grid_n - 1;
                    gl.DrawArrays(ffi::TRIANGLES, 0, quads * quads * 6);

                    // Restore standard program
                    gl.UseProgram(self.program);
                    self.set_projection_uniform(gl, self.win_uniforms.projection, &projection);
                    gl.Uniform1i(self.win_uniforms.texture, 0);
                    gl.Uniform4f(self.win_uniforms.uv_rect, 0.0, 0.0, 1.0, 1.0);
                    gl.Uniform1f(self.win_uniforms.ripple_amplitude, 0.0);
                } else if is_focused
                    && !wt.ripple_active
                    && wt.color_transform.is_none()
                    && (self.tilt_x.abs() > 0.001 || self.tilt_y.abs() > 0.001)
                {
                    // Tilt: switch to tilt program for focused window
                    gl.UseProgram(self.tilt_program);
                    self.set_projection_uniform(gl, self.tilt_uniforms.projection, &projection);
                    self.set_rect_uniform(
                        gl,
                        self.tilt_uniforms.rect,
                        draw_x,
                        draw_y,
                        draw_w,
                        draw_h,
                    );
                    gl.Uniform1i(self.tilt_uniforms.texture, 0);
                    gl.Uniform1f(self.tilt_uniforms.opacity, opacity);
                    gl.Uniform1f(self.tilt_uniforms.radius, radius);
                    gl.Uniform2f(self.tilt_uniforms.size, draw_w, draw_h);
                    gl.Uniform1f(self.tilt_uniforms.dim, dim);
                    gl.Uniform4f(self.tilt_uniforms.uv_rect, uv_x, uv_y, uv_w, uv_h);
                    gl.Uniform2f(self.tilt_uniforms.tilt, self.tilt_x, self.tilt_y);
                    gl.Uniform1f(self.tilt_uniforms.perspective, self.tilt_perspective);
                    let grid = self.tilt_grid.clamp(1, 64) as i32;
                    gl.Uniform1i(self.tilt_uniforms.grid_size, grid);
                    gl.Uniform2f(self.tilt_uniforms.light_dir, 0.0, -1.0);
                    gl.Uniform1i(
                        self.tilt_uniforms.scene_linear,
                        if scene_linear_active { 1 } else { 0 },
                    );

                    gl.ActiveTexture(ffi::TEXTURE0);
                    self.bind_window_texture(gl, texture);
                    // Grid: grid^2 quads, 6 verts each
                    gl.DrawArrays(ffi::TRIANGLES, 0, grid * grid * 6);

                    // Restore standard program
                    gl.UseProgram(self.program);
                    self.set_projection_uniform(gl, self.win_uniforms.projection, &projection);
                    gl.Uniform1i(self.win_uniforms.texture, 0);
                    gl.Uniform4f(self.win_uniforms.uv_rect, 0.0, 0.0, 1.0, 1.0);
                    gl.Uniform1f(self.win_uniforms.ripple_amplitude, 0.0);
                } else {
                    // Standard window draw
                    gl.Uniform1f(self.win_uniforms.opacity, opacity);
                    gl.Uniform1f(self.win_uniforms.dim, dim);
                    gl.Uniform1f(self.win_uniforms.radius, radius);
                    gl.Uniform2f(self.win_uniforms.size, draw_w, draw_h);
                    self.set_rect_uniform(
                        gl,
                        self.win_uniforms.rect,
                        draw_x,
                        draw_y,
                        draw_w,
                        draw_h,
                    );
                    gl.Uniform4f(self.win_uniforms.uv_rect, uv_x, uv_y, uv_w, uv_h);

                    // Ripple animation
                    if wt.ripple_active {
                        gl.Uniform1f(self.win_uniforms.ripple_progress, wt.ripple_progress);
                        gl.Uniform1f(self.win_uniforms.ripple_amplitude, self.ripple_amplitude);
                    }

                    // wp-color-management transform for this surface, if any.
                    // GLSL's mat3 is column-major; ColorTransform stores
                    // matrix_row_major, so pass GL_TRUE for transpose.
                    if let Some(t) = wt.color_transform.as_ref() {
                        gl.Uniform1i(self.win_uniforms.color_managed, 1);
                        gl.UniformMatrix3fv(
                            self.win_uniforms.color_matrix,
                            1,
                            ffi::TRUE,
                            t.matrix_row_major.as_ptr(),
                        );
                        gl.Uniform1i(self.win_uniforms.decode_tf, t.inverse_eotf.shader_id());
                        gl.Uniform1f(
                            self.win_uniforms.decode_gamma,
                            t.inverse_eotf.gamma_for_shader(),
                        );
                        gl.Uniform1i(self.win_uniforms.encode_tf, t.forward_eotf.shader_id());
                        gl.Uniform1f(
                            self.win_uniforms.encode_gamma,
                            t.forward_eotf.gamma_for_shader(),
                        );
                    }

                    gl.ActiveTexture(ffi::TEXTURE0);
                    self.bind_window_texture(gl, texture);
                    gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);

                    // Reset to default off so the next iteration's blur/ghost
                    // draws don't inherit this window's transform.
                    if wt.color_transform.is_some() {
                        gl.Uniform1i(self.win_uniforms.color_managed, 0);
                    }

                    // Reset ripple
                    if wt.ripple_active {
                        gl.Uniform1f(self.win_uniforms.ripple_amplitude, 0.0);
                    }
                }
            }

            gl.BindVertexArray(0);
            gl.UseProgram(0);
        }

        if scene_linear_active {
            if hw_encode_active {
                // The CRTC LUT expects linear input, but KMS always consumes
                // output_texture. Copy the completed linear window pass into
                // output_fbo before encoded-space overlays are drawn. Without
                // this copy output_texture remains the previous frame and every
                // genie/border/particle pass is accidentally left bound to the
                // private linear FBO.
                self.blit_fbo(
                    gl,
                    self.linear_fbo,
                    self.output_fbo,
                    self.screen_w,
                    self.screen_h,
                );
                unsafe {
                    gl.BindFramebuffer(ffi::FRAMEBUFFER, self.output_fbo);
                    gl.Viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
                }
            } else {
                // Encode linear_fbo → output_fbo using the uniform participating
                // TF (sRGB fallback when outputs are mixed).
                self.dispatch_scene_linear_encode_pass(
                    gl,
                    &projection,
                    shader_encode_tf,
                    shader_encode_gamma,
                );
            }
        }

        // Every remaining window-program draw is an overlay on output_fbo.
        // Synchronize the domain once at that boundary so expose, peek,
        // overview labels, debug HUD and system UI cannot inherit the main
        // scene's u_scene_linear=1 after a shader encode pass.
        let overlay_scene_linear =
            overlay_output_is_scene_linear(scene_linear_active, hw_encode_active);
        unsafe {
            self.sync_overlay_color_domain(gl, overlay_scene_linear);
        }

        self.frame_profiler.zone_end();

        // =================================================================
        // 9b. Close fade overlay for windows retired from visible_scene
        // =================================================================
        if self.windows.values().any(|win| {
            win.fading_out
                && !win.is_genie_minimizing
                && win.fade_opacity > 0.0
                && win.closing_rect.is_some()
                && win.texture_owner.is_some()
        }) {
            self.frame_profiler.zone_start("close_fade");
            self.render_close_fades(gl, &projection, scene_linear_active && hw_encode_active);
            self.frame_profiler.zone_end();
        }

        // =================================================================
        // 9c. Genie minimize animations (mirror X11 pass 2b)
        // =================================================================
        if !self.genie_active.is_empty() {
            self.frame_profiler.zone_start("genie");
            let genie_duration_ms = self.genie_duration_ms.max(1);
            let dock = (self.dock_x, self.dock_y);
            unsafe {
                gl.UseProgram(self.genie_program);
                self.set_projection_uniform(gl, self.genie_uniforms.projection, &projection);
                gl.Uniform1i(self.genie_uniforms.texture, 0);
                gl.Uniform1f(self.genie_uniforms.radius, 0.0);
                gl.Uniform1i(self.genie_uniforms.color_managed, 0);
                gl.Uniform1i(
                    self.genie_uniforms.scene_linear,
                    if scene_linear_active && hw_encode_active {
                        1
                    } else {
                        0
                    },
                );
                gl.Uniform1f(self.genie_uniforms.ripple_progress, 0.0);
                gl.Uniform1f(self.genie_uniforms.ripple_amplitude, 0.0);
                let grid = 12i32;
                gl.Uniform1i(self.genie_uniforms.grid_size, grid);
                gl.BindVertexArray(self.quad_vao);

                for ga in &self.genie_active {
                    let elapsed = ga.start.elapsed().as_millis() as f32;
                    let progress = (elapsed / genie_duration_ms as f32).min(1.0);
                    let opacity = 1.0 - progress;
                    self.set_rect_uniform(gl, self.genie_uniforms.rect, ga.x, ga.y, ga.w, ga.h);
                    gl.Uniform2f(self.genie_uniforms.size, ga.w, ga.h);
                    gl.Uniform1f(self.genie_uniforms.progress, progress);
                    gl.Uniform2f(self.genie_uniforms.dock_pos, dock.0, dock.1);
                    let [uv_x, uv_y, uv_w, uv_h] =
                        oriented_content_uv(ga.content_uv, ga.y_inverted);
                    gl.Uniform4f(self.genie_uniforms.uv_rect, uv_x, uv_y, uv_w, uv_h);
                    // Sign of opacity encodes "premultiplied alpha" path in shader
                    // (matches X11 convention: negative for RGBA buffers).
                    gl.Uniform1f(
                        self.genie_uniforms.opacity,
                        if ga.has_alpha { -opacity } else { opacity },
                    );
                    gl.Uniform1f(self.genie_uniforms.dim, 1.0);
                    gl.ActiveTexture(ffi::TEXTURE0);
                    self.bind_window_texture(gl, ga.texture_owner.tex_id());
                    gl.DrawArrays(ffi::TRIANGLES, 0, grid * grid * 6);
                }

                gl.BindVertexArray(0);
                gl.UseProgram(0);
            }
            self.frame_profiler.zone_end();
        }

        // =================================================================
        // 10. Draw borders (focused and urgent windows)
        // =================================================================
        self.frame_profiler.zone_start("borders");
        if self.border_enabled {
            unsafe {
                gl.UseProgram(self.border_program);
                self.set_projection_uniform(gl, self.border_uniforms.projection, &projection);
                gl.Uniform1i(
                    self.border_uniforms.scene_linear,
                    if scene_linear_active && hw_encode_active {
                        1
                    } else {
                        0
                    },
                );
                gl.BindVertexArray(self.quad_vao);

                for &(win_id, x, y, w, h) in visible_scene {
                    let wt = match self.windows.get(&win_id) {
                        Some(wt) => wt,
                        None => continue,
                    };

                    let is_focused = focused == Some(win_id);
                    if !is_focused && !wt.is_urgent {
                        continue;
                    }

                    let fade = wt.fade_opacity;
                    if fade <= 0.0 {
                        continue;
                    }

                    let radius = if wt.is_shaped || wt.is_fullscreen {
                        0.0
                    } else {
                        wt.corner_radius_override.unwrap_or(self.corner_radius)
                    };

                    let scale = wt.anim_scale;
                    let (draw_x, draw_y, draw_w, draw_h) = if (scale - 1.0).abs() > f32::EPSILON {
                        let cw = w as f32 * scale;
                        let ch = h as f32 * scale;
                        let cx = x as f32 + (w as f32 - cw) * 0.5;
                        let cy = y as f32 + (h as f32 - ch) * 0.5;
                        (cx, cy, cw, ch)
                    } else {
                        (x as f32, y as f32, w as f32, h as f32)
                    };

                    // Focus highlight: temporary pulse + thicker border on the
                    // window that just became focused. Mirrors the X11 behavior
                    // (effects.rs::tick_focus_highlight) so the visual is the same
                    // on both backends.
                    let highlight_for_win = focus_highlight_active
                        && self
                            .focus_highlight_start
                            .map(|(hw, _)| hw == win_id)
                            .unwrap_or(false);

                    let border_color = if highlight_for_win {
                        let (_, start) = self.focus_highlight_start.unwrap();
                        let elapsed_ms = start.elapsed().as_millis() as f32;
                        let dur = self.focus_highlight_duration_ms.max(1) as f32;
                        let pulse = ((elapsed_ms / dur * std::f32::consts::PI).sin()).abs();
                        let [r, g, b, a] = self.focus_highlight_color;
                        [r, g, b, a * pulse * fade]
                    } else if wt.is_urgent {
                        [1.0f32, 0.2, 0.2, 0.9 * fade]
                    } else {
                        let c = self.border_color_focused;
                        [c[0], c[1], c[2], c[3] * fade]
                    };
                    let border_width = if highlight_for_win {
                        (self.border_width + 2.0).max(3.0)
                    } else {
                        self.border_width
                    };

                    let bdr_x = draw_x - border_width;
                    let bdr_y = draw_y - border_width;
                    let bdr_w = draw_w + 2.0 * border_width;
                    let bdr_h = draw_h + 2.0 * border_width;

                    gl.Uniform4f(
                        self.border_uniforms.border_color,
                        border_color[0],
                        border_color[1],
                        border_color[2],
                        border_color[3],
                    );
                    gl.Uniform1f(self.border_uniforms.border_width, border_width);
                    gl.Uniform1f(self.border_uniforms.radius, radius);
                    gl.Uniform2f(self.border_uniforms.size, bdr_w, bdr_h);
                    self.set_rect_uniform(
                        gl,
                        self.border_uniforms.rect,
                        bdr_x,
                        bdr_y,
                        bdr_w,
                        bdr_h,
                    );

                    gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                }

                gl.BindVertexArray(0);
                gl.UseProgram(0);
            }
        } // border_enabled
        self.frame_profiler.zone_end();

        // End of scissored output_fbo passes. Effect overlays below always run
        // full-screen, and allow_partial already excludes every one of them, so
        // disabling here keeps the scissor strictly around the calm-frame draws.
        if scissor_active {
            unsafe {
                gl.Disable(ffi::SCISSOR_TEST);
            }
        }

        // =================================================================
        // 11. Genie animations
        // =================================================================
        self.frame_profiler.zone_start("effects");
        // Genie minimize/unminimize animations are rendered by the effects
        // module via render_genie_animations() if any are active. That method
        // is defined in effects.rs.
        self.render_genie_animations(gl, &projection);

        // =================================================================
        // 12. Workspace transitions
        // =================================================================
        if self.transition_active {
            self.render_transition(gl, &projection);
        }

        // =================================================================
        // 13. Snap preview overlay
        // =================================================================
        self.render_snap_preview(gl, &projection);

        // =================================================================
        // 14. Overview overlay
        // =================================================================
        if self.overview_active {
            self.render_overview(gl, &projection);
        }

        // =================================================================
        // 15. Expose overlay
        // =================================================================
        if !self.expose_entries.is_empty() && self.expose_opacity > 0.0 {
            self.render_expose(gl, &projection);
        }

        // =================================================================
        // 15b. Peek mode (fade out non-focused windows)
        // =================================================================
        if self.peek_active {
            self.render_peek_mode(gl, &projection, focused, scene);
        }

        // =================================================================
        // 15c. Tab bar for window groups
        // =================================================================
        if self.window_tabs_enabled && !self.window_groups.is_empty() {
            self.render_tab_bar(gl, &projection, visible_scene);
        }

        // =================================================================
        // 16. Particles
        // =================================================================
        if !self.particle_systems.is_empty() {
            self.render_particles(gl, &projection);
        }

        // =================================================================
        // 17. Edge glow
        // =================================================================
        if edge_glow_continuous {
            unsafe {
                gl.UseProgram(self.edge_glow_program);
                self.set_projection_uniform(gl, self.edge_glow_uniforms.projection, &projection);
                self.set_rect_uniform(
                    gl,
                    self.edge_glow_uniforms.rect,
                    0.0,
                    0.0,
                    self.screen_w as f32,
                    self.screen_h as f32,
                );
                let egc = self.edge_glow_color;
                gl.Uniform4f(
                    self.edge_glow_uniforms.glow_color,
                    egc[0],
                    egc[1],
                    egc[2],
                    egc[3],
                );
                gl.Uniform1f(self.edge_glow_uniforms.glow_width, self.edge_glow_width);
                gl.Uniform2f(self.edge_glow_uniforms.mouse, self.mouse_x, self.mouse_y);
                gl.Uniform2f(
                    self.edge_glow_uniforms.screen_size,
                    self.screen_w as f32,
                    self.screen_h as f32,
                );
                // Use frame_count as a time proxy (at ~60fps, 1 frame = ~16.6ms)
                gl.Uniform1f(self.edge_glow_uniforms.time, self.frame_count as f32 / 60.0);
                gl.BindVertexArray(self.quad_vao);
                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                gl.BindVertexArray(0);
                gl.UseProgram(0);
            }
        }

        // =================================================================
        // 18. Post-processing
        // =================================================================
        if self.postprocess_active {
            // Copy output_fbo to postprocess_fbo
            self.blit_fbo(
                gl,
                self.output_fbo,
                self.postprocess_fbo,
                self.screen_w,
                self.screen_h,
            );

            unsafe {
                // Bind output FBO for final post-processed result
                gl.BindFramebuffer(ffi::FRAMEBUFFER, self.output_fbo);
                gl.Viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
                gl.Clear(ffi::COLOR_BUFFER_BIT);

                gl.UseProgram(self.postprocess_program);
                self.set_projection_uniform(gl, self.postprocess_uniforms.projection, &projection);
                self.set_rect_uniform(
                    gl,
                    self.postprocess_uniforms.rect,
                    0.0,
                    0.0,
                    self.screen_w as f32,
                    self.screen_h as f32,
                );
                gl.Uniform1i(self.postprocess_uniforms.texture, 0);
                gl.Uniform1f(self.postprocess_uniforms.color_temp, self.color_temperature);
                gl.Uniform1f(self.postprocess_uniforms.saturation, self.saturation);
                gl.Uniform1f(self.postprocess_uniforms.brightness, self.brightness);
                gl.Uniform1f(self.postprocess_uniforms.contrast, self.contrast);
                gl.Uniform1i(
                    self.postprocess_uniforms.invert,
                    if self.invert_colors { 1 } else { 0 },
                );
                gl.Uniform1i(
                    self.postprocess_uniforms.grayscale,
                    if self.grayscale { 1 } else { 0 },
                );
                gl.Uniform1i(
                    self.postprocess_uniforms.magnifier_enabled,
                    if self.magnifier_enabled { 1 } else { 0 },
                );
                if self.magnifier_enabled {
                    let cx = self.mouse_x / self.screen_w as f32;
                    let cy = self.mouse_y / self.screen_h as f32;
                    gl.Uniform2f(self.postprocess_uniforms.magnifier_center, cx, 1.0 - cy);
                    gl.Uniform1f(
                        self.postprocess_uniforms.magnifier_radius,
                        self.magnifier_radius,
                    );
                    gl.Uniform1f(
                        self.postprocess_uniforms.magnifier_zoom,
                        self.magnifier_zoom,
                    );
                }
                gl.Uniform1i(
                    self.postprocess_uniforms.colorblind_mode,
                    self.colorblind_mode,
                );
                gl.Uniform1i(
                    self.postprocess_uniforms.hdr_enabled,
                    if self.hdr_enabled { 1 } else { 0 },
                );
                gl.Uniform1f(self.postprocess_uniforms.hdr_peak_nits, self.hdr_peak_nits);
                gl.Uniform1i(
                    self.postprocess_uniforms.tone_mapping_method,
                    self.tone_mapping_method,
                );

                gl.ActiveTexture(ffi::TEXTURE0);
                gl.BindTexture(ffi::TEXTURE_2D, self.postprocess_texture);
                gl.BindVertexArray(self.quad_vao);
                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);

                gl.BindVertexArray(0);
                gl.UseProgram(0);
            }
        }

        self.frame_profiler.zone_end();

        // A locked compositor must never expose the client scene through an
        // IPC or protocol screenshot. Draw the opaque shield before readback.
        if self
            .system_ui
            .as_ref()
            .is_some_and(|overlay| overlay.locked)
        {
            unsafe {
                gl.BindFramebuffer(ffi::FRAMEBUFFER, self.output_fbo);
                self.render_system_ui(gl, &projection);
            }
        }

        // =================================================================
        // 19. Screenshot capture (region or full)
        // =================================================================
        if self.screenshot_requests.has_pending() {
            unsafe {
                gl.BindFramebuffer(ffi::FRAMEBUFFER, self.output_fbo);
                self.capture_pending_screenshots(gl);
            }
        }
        unsafe {
            self.screenshot_readback.drain_ready(gl);
        }

        // =================================================================
        // 19b. Extended Debug HUD
        // =================================================================
        if self.debug_hud_enabled && self.debug_hud_extended {
            unsafe {
                gl.BindFramebuffer(ffi::FRAMEBUFFER, self.output_fbo);
                self.render_debug_hud(gl, &projection);
            }
        }

        // =================================================================
        // 19c. Annotations overlay
        // =================================================================
        if self.annotation_active && !self.annotation_strokes.is_empty() {
            unsafe {
                gl.BindFramebuffer(ffi::FRAMEBUFFER, self.output_fbo);
                self.render_annotations(gl, &projection);
            }
        }

        if self.system_ui.is_some() {
            unsafe {
                gl.BindFramebuffer(ffi::FRAMEBUFFER, self.output_fbo);
                self.render_system_ui(gl, &projection);
            }
        }

        // =================================================================
        // 20. Finalize - unbind FBO
        // =================================================================
        unsafe {
            gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
        }

        // =================================================================
        // 21. Recording capture (async PBO readback to ffmpeg)
        // =================================================================
        if let Some((path, region)) = self.pending_recording_start.take() {
            unsafe {
                let config = crate::config::CONFIG.load();
                let recording = config.behavior();
                if let Err(e) = self.recording.start(
                    gl,
                    self.screen_w,
                    self.screen_h,
                    &path,
                    recording.recording_fps.clamp(1, 240),
                    &recording.recording_bitrate,
                    recording.recording_quality,
                    &recording.recording_encoder,
                    region,
                ) {
                    log::error!("[compositor] Failed to start recording: {}", e);
                }
            }
        }
        if self.recording.is_active() {
            unsafe {
                self.recording
                    .capture_frame(gl, self.output_fbo, (self.mouse_x, self.mouse_y));
            }
        }
        if self.pending_recording_stop {
            self.pending_recording_stop = false;
            unsafe {
                self.recording.stop(gl);
            }
        }

        // The crop outline is deliberately rendered after recording readback:
        // it is visible on the local output but never encoded into the video.
        if self.recording_region_overlay.is_some() {
            unsafe {
                gl.BindFramebuffer(ffi::FRAMEBUFFER, self.output_fbo);
                self.render_recording_region_overlay(gl, &projection);
                gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
            }
        }

        // =================================================================
        // 22. Performance infrastructure - frame end
        // =================================================================
        let frame_ms = self.frame_profiler.end_frame();
        self.perf_metrics
            .record_compositor(std::time::Duration::from_secs_f32(frame_ms / 1000.0));
        self.adaptive_scheduler
            .on_frame_completed(std::time::Duration::from_secs_f32(frame_ms / 1000.0));
        // Sampling is internally throttled and makes the IPC metric useful
        // even when the debug HUD is off.
        self.sys_stats.maybe_sample();
        self.perf_metrics
            .set_cpu_load(self.sys_stats.cpu_pct().clamp(0.0, 100.0) as u32);
        self.perf_metrics.set_gpu_load(
            self.perf_metrics
                .estimate_gpu_load(self.frame_rate_limiter.target_fps() as f32)
                .min(100),
        );
        self.dirty_region_tracker.clear();
        self.content_dirty_ids.clear();
        self.prev_focused = focused;
        unsafe {
            self.reset_external_gl_state(gl);
        }

        // Predictive render: update scene activity periodically
        self.predictive_render_mgr.update_scene_activity();

        // Schedule the next render for genuinely time-varying work. This is
        // repeated at frame end because screenshot readback may update the
        // needs_render flag while draining its queue.
        if any_animating || postprocess_continuous || edge_glow_continuous {
            self.needs_render = true;
        }

        // Mark frame for rate limiter
        self.frame_rate_limiter.mark_frame();

        true
    }

    fn render_genie_animations(&self, gl: &ffi::Gles2, projection: &[f32; 16]) {
        let _ = (gl, projection);
    }

    unsafe fn capture_pending_screenshots(&mut self, gl: &ffi::Gles2) {
        unsafe {
            for request in self.screenshot_requests.take_all() {
                match request {
                    crate::backend::compositor_common::screenshot::ScreenshotRequest::Full(
                        path,
                    ) => {
                        let w = self.screen_w;
                        let h = self.screen_h;
                        self.screenshot_readback.enqueue(gl, path, 0, 0, w, h);
                    }
                    crate::backend::compositor_common::screenshot::ScreenshotRequest::Region {
                        path,
                        x,
                        y,
                        width,
                        height,
                    } => {
                        let Some(region) =
                            clip_region(self.screen_w, self.screen_h, x, y, width, height)
                        else {
                            log::warn!("[compositor] screenshot region is empty");
                            continue;
                        };
                        let (x, y, w, h) = (region.x, region.y, region.width, region.height);
                        self.screenshot_readback.enqueue(
                            gl,
                            path,
                            x as i32,
                            self.screen_h.saturating_sub(y + h) as i32,
                            w,
                            h,
                        );
                    }
                }
            }
            // The next compositor tick polls the fence with a zero timeout.
            // Keep it armed until every queued readback has been handed to
            // the PNG worker.
            self.needs_render = self.screenshot_readback.has_pending();
        }
    }

    unsafe fn render_debug_hud(&mut self, gl: &ffi::Gles2, projection: &[f32; 16]) {
        self.sys_stats.maybe_sample();

        let uptime = self.compositor_start_time.elapsed().as_secs();
        let frame_ms = if self.fps > 0.0 {
            1000.0 / self.fps
        } else {
            0.0
        };
        let mut hud_text = format!(
            "JWM debug HUD (Alt+Shift+F12)\n\
             Backend: wayland_udev\n\
             FPS: {:.1}   Frame: {:.2} ms   Frames: {}\n\
             Windows: {}   Monitors: {}   Uptime: {}s\n\
             Memory: {:.1} MiB RSS\n\
             CPU: {:.1} %\n\
             VRR: {}   Blur reuse: {}/{}",
            self.fps,
            frame_ms,
            self.frame_count,
            self.windows.len(),
            self.monitors.len(),
            uptime,
            self.sys_stats.rss_mib(),
            self.sys_stats.cpu_pct(),
            if self.vrr_active { "ON" } else { "off" },
            self.temporal_blur_reuse_count,
            self.temporal_blur_total_count,
        );

        if self.debug_hud_extended {
            use std::fmt::Write;
            let p95_ms = self.perf_metrics.frame_time_percentile(0.95).as_secs_f32() * 1000.0;
            let p99_ms = self.perf_metrics.frame_time_percentile(0.99).as_secs_f32() * 1000.0;
            let _ = write!(
                hud_text,
                "\nFrame tail: p95={p95_ms:.2}ms p99={p99_ms:.2}ms\n--- Profiler (ms avg/min/max, last 120 frames) ---",
            );
            for (name, stats) in self.frame_profiler.all_zone_stats() {
                let _ = write!(
                    hud_text,
                    "\n{:<8}: {:>5.2} / {:>5.2} / {:>5.2}  (n={})",
                    name, stats.avg_ms, stats.min_ms, stats.max_ms, stats.sample_count,
                );
            }
        }

        if hud_text != self.hud_text_cache {
            let (pixels, w, h) = font::render_text_to_rgba(&hud_text, 2, [255, 255, 255, 220]);
            if w > 0 && h > 0 {
                unsafe {
                    // Delete old texture
                    if let Some(old) = self.hud_text_texture.take() {
                        gl.DeleteTextures(1, &old);
                    }
                    // Create and upload new texture
                    let mut tex = 0u32;
                    gl.GenTextures(1, &mut tex);
                    gl.BindTexture(ffi::TEXTURE_2D, tex);
                    gl.TexImage2D(
                        ffi::TEXTURE_2D,
                        0,
                        ffi::RGBA as i32,
                        w as i32,
                        h as i32,
                        0,
                        ffi::RGBA,
                        ffi::UNSIGNED_BYTE,
                        pixels.as_ptr() as *const _,
                    );
                    gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, ffi::LINEAR as i32);
                    gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, ffi::LINEAR as i32);
                    self.hud_text_texture = Some(tex);
                    self.hud_text_width = w;
                    self.hud_text_height = h;
                }
            }
            self.hud_text_cache = hud_text;
        }

        // Draw the HUD texture in the top-left corner
        if let Some(tex) = self.hud_text_texture {
            unsafe {
                gl.UseProgram(self.program);
                self.set_projection_uniform(gl, self.win_uniforms.projection, projection);
                gl.Uniform1i(self.win_uniforms.texture, 0);
                gl.Uniform1f(self.win_uniforms.opacity, 0.85);
                gl.Uniform1f(self.win_uniforms.dim, 1.0);
                gl.Uniform1f(self.win_uniforms.radius, 4.0);
                gl.Uniform2f(
                    self.win_uniforms.size,
                    self.hud_text_width as f32,
                    self.hud_text_height as f32,
                );
                self.set_rect_uniform(
                    gl,
                    self.win_uniforms.rect,
                    10.0,
                    10.0,
                    self.hud_text_width as f32,
                    self.hud_text_height as f32,
                );
                gl.Uniform4f(self.win_uniforms.uv_rect, 0.0, 0.0, 1.0, 1.0);
                gl.Uniform1f(self.win_uniforms.ripple_amplitude, 0.0);

                gl.ActiveTexture(ffi::TEXTURE0);
                gl.BindTexture(ffi::TEXTURE_2D, tex);
                gl.BindVertexArray(self.quad_vao);
                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                gl.BindVertexArray(0);
                gl.UseProgram(0);
            }
        }
    }

    #[allow(unsafe_op_in_unsafe_fn)]
    unsafe fn render_system_ui(&mut self, gl: &ffi::Gles2, projection: &[f32; 16]) {
        let Some(overlay) = self.system_ui.clone() else {
            return;
        };
        let config = crate::config::CONFIG.load();
        let description = config.system_ui_font();
        let size = crate::backend::compositor_font::ui_font_pixel_size(description);
        let cache_key = format!("{description}\0{size}\0{}", overlay.text);
        if cache_key != self.hud_text_cache {
            let (pixels, w, h) = crate::backend::compositor_font::render_ui_text_to_rgba(
                &overlay.text,
                description,
                size,
                [235, 240, 255, 255],
            );
            if let Some(old) = self.hud_text_texture.take() {
                gl.DeleteTextures(1, &old);
            }
            let mut tex = 0;
            gl.GenTextures(1, &mut tex);
            gl.BindTexture(ffi::TEXTURE_2D, tex);
            gl.TexImage2D(
                ffi::TEXTURE_2D,
                0,
                ffi::RGBA as i32,
                w as i32,
                h as i32,
                0,
                ffi::RGBA,
                ffi::UNSIGNED_BYTE,
                pixels.as_ptr().cast(),
            );
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, ffi::LINEAR as i32);
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, ffi::LINEAR as i32);
            self.hud_text_texture = Some(tex);
            self.hud_text_width = w;
            self.hud_text_height = h;
            self.hud_text_cache = cache_key;
        }
        let pad = 30.0;
        let tw = self.hud_text_width as f32;
        let th = self.hud_text_height as f32;
        let pw = (tw + pad * 2.0).min(self.screen_w as f32 - 32.0);
        let ph = th + pad * 2.0;
        let x = (self.screen_w as f32 - pw) * 0.5;
        let y = (self.screen_h as f32 - ph) * 0.5;
        if overlay.locked {
            gl.ClearColor(0.018, 0.022, 0.035, 1.0);
            gl.Clear(ffi::COLOR_BUFFER_BIT);
        }
        let rect = super::get_uniform_loc(gl, self.hud_program, "u_rect");
        let proj = super::get_uniform_loc(gl, self.hud_program, "u_projection");
        let bg = super::get_uniform_loc(gl, self.hud_program, "u_bg_color");
        let size = super::get_uniform_loc(gl, self.hud_program, "u_size");
        gl.UseProgram(self.hud_program);
        gl.UniformMatrix4fv(proj, 1, ffi::FALSE as u8, projection.as_ptr());
        gl.Uniform4f(
            bg,
            0.025,
            0.03,
            0.045,
            if overlay.locked { 1.0 } else { 0.94 },
        );
        gl.Uniform2f(size, self.screen_w as f32, self.screen_h as f32);
        gl.Uniform4f(rect, 0.0, 0.0, self.screen_w as f32, self.screen_h as f32);
        gl.BindVertexArray(self.quad_vao);
        gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
        gl.Uniform4f(bg, 0.08, 0.10, 0.15, 0.98);
        gl.Uniform2f(size, pw, ph);
        gl.Uniform4f(rect, x, y, pw, ph);
        gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
        if let Some(tex) = self.hud_text_texture {
            gl.UseProgram(self.program);
            self.set_projection_uniform(gl, self.win_uniforms.projection, projection);
            gl.Uniform1i(self.win_uniforms.texture, 0);
            gl.Uniform1f(self.win_uniforms.opacity, 1.0);
            gl.Uniform1f(self.win_uniforms.dim, 1.0);
            gl.Uniform1f(self.win_uniforms.radius, 0.0);
            gl.Uniform2f(self.win_uniforms.size, tw, th);
            self.set_rect_uniform(gl, self.win_uniforms.rect, x + pad, y + pad, tw, th);
            gl.Uniform4f(self.win_uniforms.uv_rect, 0.0, 0.0, 1.0, 1.0);
            gl.Uniform1f(self.win_uniforms.ripple_amplitude, 0.0);
            gl.ActiveTexture(ffi::TEXTURE0);
            gl.BindTexture(ffi::TEXTURE_2D, tex);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
        }
        gl.BindVertexArray(0);
        gl.UseProgram(0);
    }

    #[allow(dead_code)]
    pub(crate) fn request_screenshot(&mut self, path: PathBuf) {
        self.screenshot_requests.request_full(path);
        self.needs_render = true;
    }

    #[allow(dead_code)]
    pub(crate) fn request_screenshot_region(
        &mut self,
        path: PathBuf,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    ) {
        self.screenshot_requests.request_region(path, x, y, w, h);
        self.needs_render = true;
    }

    /// Render annotation strokes as GL_LINES using the line shader.
    unsafe fn render_annotations(&self, gl: &ffi::Gles2, projection: &[f32; 16]) {
        unsafe {
            gl.UseProgram(self.line_program);
            gl.UniformMatrix4fv(
                self.line_uniform_projection,
                1,
                ffi::FALSE as u8,
                projection.as_ptr(),
            );
            self.enable_premultiplied_blend(gl);

            for stroke in &self.annotation_strokes {
                if stroke.points.len() < 2 {
                    continue;
                }

                gl.LineWidth(stroke.width);
                gl.Uniform4f(
                    self.line_uniform_color,
                    stroke.color[0],
                    stroke.color[1],
                    stroke.color[2],
                    stroke.color[3],
                );

                // Build vertex data for GL_LINES (pairs of adjacent points)
                let mut vertices: Vec<f32> = Vec::with_capacity((stroke.points.len() - 1) * 4);
                for i in 0..stroke.points.len() - 1 {
                    let (x0, y0) = stroke.points[i];
                    let (x1, y1) = stroke.points[i + 1];
                    vertices.extend_from_slice(&[x0, y0, x1, y1]);
                }

                let mut vbo = 0u32;
                let mut vao = 0u32;
                gl.GenVertexArrays(1, &mut vao);
                gl.BindVertexArray(vao);
                gl.GenBuffers(1, &mut vbo);
                gl.BindBuffer(ffi::ARRAY_BUFFER, vbo);
                gl.BufferData(
                    ffi::ARRAY_BUFFER,
                    (vertices.len() * std::mem::size_of::<f32>()) as isize,
                    vertices.as_ptr() as *const _,
                    ffi::STREAM_DRAW,
                );

                gl.EnableVertexAttribArray(0);
                gl.VertexAttribPointer(0, 2, ffi::FLOAT, ffi::FALSE as u8, 8, std::ptr::null());

                let num_verts = ((stroke.points.len() - 1) * 2) as i32;
                gl.DrawArrays(ffi::LINES, 0, num_verts);

                gl.DisableVertexAttribArray(0);
                gl.BindBuffer(ffi::ARRAY_BUFFER, 0);
                gl.BindVertexArray(0);
                gl.DeleteBuffers(1, &vbo);
                gl.DeleteVertexArrays(1, &vao);
            }

            gl.LineWidth(1.0);
            gl.UseProgram(0);
        }
    }
}
