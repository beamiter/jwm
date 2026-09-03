// render_frame and rendering helpers for the Wayland udev compositor
#[allow(unused_imports)]
use super::*;
use crate::backend::compositor_common::attention::{
    attention_border_style, attention_signal_active,
};
use crate::backend::compositor_common::capture::clip_region;
use crate::backend::compositor_common::debug_hud as hud;
use crate::backend::compositor_common::dynamic_island::{IslandDock, clip_bar_to_viewport};
use crate::backend::compositor_common::effects::MotionTrailParams;
use crate::backend::compositor_common::genie::{
    dock_item_preview_target, genie_progress, output_bounds_for_anchor, preview_rect,
};
use crate::backend::compositor_common::minimized_thumbnail::ThumbnailPurpose;
use crate::backend::compositor_common::system_ui_panel as panel;
use crate::backend::compositor_common::ui_theme::{self, UiPalette};
use crate::backend::compositor_common::window_glow::{WindowGlowSettings, WindowGlowTarget};
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

/// Upload one line of toast text as a texture; `None` for empty text.
unsafe fn rasterize_toast_text(
    gl: &ffi::Gles2,
    text: &str,
    description: &str,
    size: f32,
    color: [u8; 4],
) -> Option<(u32, u32, u32)> {
    if text.is_empty() {
        return None;
    }
    let (pixels, w, h) =
        crate::backend::compositor_font::render_ui_text_to_rgba(text, description, size, color);
    if w == 0 || h == 0 {
        return None;
    }
    unsafe {
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
        Some((tex, w, h))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OverviewRenderRoute {
    LegacyEncoded,
    DirectLinear,
    SoftwareReentry,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FrameOutputRoute {
    LegacyEncoded,
    EarlySrgbFallback,
    DeferredHardware,
    DeferredRegions,
}

const fn frame_output_route(
    scene_linear_active: bool,
    linear_tail_safe: bool,
    hw_encode_active: bool,
    hw_ctm_active: bool,
    software_regions_available: bool,
) -> FrameOutputRoute {
    if !scene_linear_active {
        return FrameOutputRoute::LegacyEncoded;
    }
    if !linear_tail_safe {
        return FrameOutputRoute::EarlySrgbFallback;
    }
    if hw_encode_active && hw_ctm_active {
        return FrameOutputRoute::DeferredHardware;
    }
    if software_regions_available {
        return FrameOutputRoute::DeferredRegions;
    }
    FrameOutputRoute::EarlySrgbFallback
}

const fn overview_render_route(output_route: FrameOutputRoute) -> OverviewRenderRoute {
    match output_route {
        FrameOutputRoute::LegacyEncoded => OverviewRenderRoute::LegacyEncoded,
        FrameOutputRoute::EarlySrgbFallback => OverviewRenderRoute::SoftwareReentry,
        FrameOutputRoute::DeferredHardware | FrameOutputRoute::DeferredRegions => {
            OverviewRenderRoute::DirectLinear
        }
    }
}

fn previous_frame_requires_srgb_transition_snapshot(state: Option<&OutputColorFrameState>) -> bool {
    state.is_some_and(|state| {
        state.linear_tail_safe
            && ((state.hw_encode_active && state.hw_ctm_active) || state.software_regions.is_some())
    })
}

/// Clip a top-left-origin compositor rectangle to the global framebuffer and
/// return the matching bottom-left-origin GLES scissor rectangle.
fn overview_monitor_scissor(
    monitor: (i32, i32, u32, u32),
    screen_w: u32,
    screen_h: u32,
) -> Option<[i32; 4]> {
    let screen_w = i64::from(screen_w.min(i32::MAX as u32));
    let screen_h = i64::from(screen_h.min(i32::MAX as u32));
    let x0 = i64::from(monitor.0).clamp(0, screen_w);
    let y0 = i64::from(monitor.1).clamp(0, screen_h);
    let x1 = (i64::from(monitor.0) + i64::from(monitor.2)).clamp(0, screen_w);
    let y1 = (i64::from(monitor.1) + i64::from(monitor.3)).clamp(0, screen_h);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }

    Some([
        i32::try_from(x0).ok()?,
        i32::try_from(screen_h - y1).ok()?,
        i32::try_from(x1 - x0).ok()?,
        i32::try_from(y1 - y0).ok()?,
    ])
}

fn intersect_scissors(a: [i32; 4], b: [i32; 4]) -> Option<[i32; 4]> {
    let ax1 = i64::from(a[0]) + i64::from(a[2].max(0));
    let ay1 = i64::from(a[1]) + i64::from(a[3].max(0));
    let bx1 = i64::from(b[0]) + i64::from(b[2].max(0));
    let by1 = i64::from(b[1]) + i64::from(b[3].max(0));
    let x0 = i64::from(a[0]).max(i64::from(b[0]));
    let y0 = i64::from(a[1]).max(i64::from(b[1]));
    let x1 = ax1.min(bx1);
    let y1 = ay1.min(by1);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some([
        i32::try_from(x0).ok()?,
        i32::try_from(y0).ok()?,
        i32::try_from(x1 - x0).ok()?,
        i32::try_from(y1 - y0).ok()?,
    ])
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RetainedTexturePass {
    CloseFade,
    Genie,
    StaticDockItem,
    DockPreview,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RetainedTextureProgram {
    Window,
    Genie,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct RetainedColorPlan {
    program: RetainedTextureProgram,
    transform: Option<crate::backend::wayland_udev::color_pipeline::ColorTransform>,
    scene_linear: bool,
}

/// Produce the exact same color-management policy for every path that samples
/// a retained minimized-window texture. Keeping pass selection in this pure
/// plan prevents Genie, the static Dock item and hover preview from silently
/// drifting apart as their draw loops evolve.
fn retained_color_plan(
    pass: RetainedTexturePass,
    transform: Option<crate::backend::wayland_udev::color_pipeline::ColorTransform>,
    scene_linear: bool,
) -> RetainedColorPlan {
    RetainedColorPlan {
        program: match pass {
            RetainedTexturePass::Genie => RetainedTextureProgram::Genie,
            RetainedTexturePass::CloseFade
            | RetainedTexturePass::StaticDockItem
            | RetainedTexturePass::DockPreview => RetainedTextureProgram::Window,
        },
        transform,
        scene_linear,
    }
}

pub(super) fn transform_for_encoded_srgb(
    mut transform: crate::backend::wayland_udev::color_pipeline::ColorTransform,
) -> crate::backend::wayland_udev::color_pipeline::ColorTransform {
    transform.forward_eotf = crate::backend::wayland_udev::color_pipeline::TransferKind::Srgb;
    transform
}

#[derive(Clone, Copy)]
struct ColorUniformLocations {
    managed: i32,
    matrix: i32,
    decode_tf: i32,
    decode_gamma: i32,
    encode_tf: i32,
    encode_gamma: i32,
    scene_linear: i32,
}

fn postprocess_requires_continuous_frames(
    postprocess_active: bool,
    has_time_varying_input: bool,
) -> bool {
    postprocess_active && has_time_varying_input
}

fn attention_requires_continuous_frames(animation_enabled: bool, has_urgent_window: bool) -> bool {
    attention_signal_active(animation_enabled, has_urgent_window)
}

fn snap_preview_allows_partial_damage(preview_present: bool, opacity: f32) -> bool {
    !preview_present && opacity <= 0.0001
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
        FrameOutputRoute, OcclusionCandidate, OutputColorFrameState, OverviewRenderRoute,
        RetainedTexturePass, RetainedTextureProgram, attention_requires_continuous_frames,
        edge_glow_requires_continuous_frames, frame_output_route, intersect_scissors,
        is_opaque_output_occluder, oriented_content_uv, overview_monitor_scissor,
        overview_render_route, postprocess_requires_continuous_frames, premultiplied_blend_factors,
        previous_frame_requires_srgb_transition_snapshot, retained_color_plan,
        snap_preview_allows_partial_damage, transform_for_encoded_srgb,
    };
    use crate::backend::wayland_udev::color_pipeline::{ColorTransform, TransferKind};
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
    fn output_route_covers_legacy_fallback_hardware_and_region_delivery() {
        assert_eq!(
            frame_output_route(false, true, true, true, true),
            FrameOutputRoute::LegacyEncoded
        );
        assert_eq!(
            frame_output_route(true, false, false, false, true),
            FrameOutputRoute::EarlySrgbFallback
        );
        assert_eq!(
            frame_output_route(true, true, true, true, false),
            FrameOutputRoute::DeferredHardware
        );
        assert_eq!(
            frame_output_route(true, true, false, false, true),
            FrameOutputRoute::DeferredRegions
        );
        assert_eq!(
            frame_output_route(true, true, true, false, true),
            FrameOutputRoute::DeferredRegions
        );
        assert_eq!(
            frame_output_route(true, true, false, false, false),
            FrameOutputRoute::EarlySrgbFallback
        );

        assert_eq!(
            overview_render_route(FrameOutputRoute::LegacyEncoded),
            OverviewRenderRoute::LegacyEncoded
        );
        assert_eq!(
            overview_render_route(FrameOutputRoute::EarlySrgbFallback),
            OverviewRenderRoute::SoftwareReentry
        );
        assert_eq!(
            overview_render_route(FrameOutputRoute::DeferredHardware),
            OverviewRenderRoute::DirectLinear
        );
        assert_eq!(
            overview_render_route(FrameOutputRoute::DeferredRegions),
            OverviewRenderRoute::DirectLinear
        );
    }

    #[test]
    fn encoded_srgb_overlay_plan_preserves_source_decode_and_gamut() {
        let transform = ColorTransform {
            inverse_eotf: TransferKind::St2084Pq,
            matrix_row_major: [0.8, 0.1, 0.1, 0.2, 0.7, 0.1, 0.0, 0.2, 0.8],
            forward_eotf: TransferKind::Linear,
        };
        let encoded = transform_for_encoded_srgb(transform);
        assert_eq!(encoded.inverse_eotf, transform.inverse_eotf);
        assert_eq!(encoded.matrix_row_major, transform.matrix_row_major);
        assert_eq!(encoded.forward_eotf, TransferKind::Srgb);
    }

    #[test]
    fn deferred_previous_frame_normalizes_transition_snapshot_to_srgb() {
        let software = OutputColorFrameState {
            linear_tail_safe: true,
            hw_encode_active: false,
            hw_ctm_active: false,
            software_regions: Some(Vec::new()),
        };
        let hardware = OutputColorFrameState {
            linear_tail_safe: true,
            hw_encode_active: true,
            hw_ctm_active: true,
            software_regions: None,
        };
        let fallback = OutputColorFrameState {
            linear_tail_safe: false,
            hw_encode_active: false,
            hw_ctm_active: false,
            software_regions: Some(Vec::new()),
        };

        assert!(previous_frame_requires_srgb_transition_snapshot(Some(
            &software
        )));
        assert!(previous_frame_requires_srgb_transition_snapshot(Some(
            &hardware
        )));
        assert!(!previous_frame_requires_srgb_transition_snapshot(Some(
            &fallback
        )));
        assert!(!previous_frame_requires_srgb_transition_snapshot(None));
    }

    #[test]
    fn overview_monitor_scissor_clips_and_flips_to_gles_coordinates() {
        assert_eq!(
            overview_monitor_scissor((0, 0, 1920, 1080), 1920, 1080),
            Some([0, 0, 1920, 1080])
        );
        assert_eq!(
            overview_monitor_scissor((1920, 0, 1920, 1080), 3840, 1080),
            Some([1920, 0, 1920, 1080])
        );
        assert_eq!(
            overview_monitor_scissor((100, 200, 800, 600), 1920, 1080),
            Some([100, 280, 800, 600])
        );
        assert_eq!(
            overview_monitor_scissor((-100, -50, 400, 300), 1920, 1080),
            Some([0, 830, 300, 250])
        );
        assert_eq!(
            overview_monitor_scissor((1800, 1000, 500, 500), 1920, 1080),
            Some([1800, 0, 120, 80])
        );
        assert_eq!(overview_monitor_scissor((0, 0, 0, 100), 1920, 1080), None);
        assert_eq!(
            overview_monitor_scissor((-500, 0, 100, 100), 1920, 1080),
            None
        );
        assert_eq!(
            overview_monitor_scissor((0, 1200, 100, 100), 1920, 1080),
            None
        );
    }

    #[test]
    fn output_region_scissors_intersect_damage_without_overflow() {
        assert_eq!(
            intersect_scissors([100, 50, 400, 300], [250, 0, 400, 200]),
            Some([250, 50, 250, 150])
        );
        assert_eq!(
            intersect_scissors([0, 0, 100, 100], [100, 0, 100, 100]),
            None
        );
        assert_eq!(
            intersect_scissors([0, 0, i32::MAX, i32::MAX], [1, 2, 3, 4]),
            Some([1, 2, 3, 4])
        );
        assert_eq!(intersect_scissors([0, 0, -1, 20], [0, 0, 10, 10]), None);
    }

    #[test]
    fn every_retained_dock_texture_path_consumes_nonidentity_color_transform() {
        let transform = ColorTransform {
            inverse_eotf: TransferKind::St2084Pq,
            matrix_row_major: [0.63, 0.29, 0.08, 0.07, 0.92, 0.01, 0.02, 0.08, 0.90],
            forward_eotf: TransferKind::Srgb,
        };

        for (pass, expected_program) in [
            (RetainedTexturePass::Genie, RetainedTextureProgram::Genie),
            (
                RetainedTexturePass::StaticDockItem,
                RetainedTextureProgram::Window,
            ),
            (
                RetainedTexturePass::DockPreview,
                RetainedTextureProgram::Window,
            ),
        ] {
            let plan = retained_color_plan(pass, Some(transform), true);
            assert_eq!(plan.program, expected_program);
            assert_eq!(plan.transform, Some(transform));
            assert!(plan.scene_linear);
        }
    }

    #[test]
    fn static_postprocess_does_not_request_continuous_frames() {
        assert!(!postprocess_requires_continuous_frames(false, false));
        assert!(!postprocess_requires_continuous_frames(true, false));
        assert!(!postprocess_requires_continuous_frames(false, true));
        assert!(postprocess_requires_continuous_frames(true, true));
    }

    #[test]
    fn urgent_attention_keeps_the_frame_scheduler_live_only_while_visible() {
        assert!(attention_requires_continuous_frames(true, true));
        assert!(!attention_requires_continuous_frames(false, true));
        assert!(!attention_requires_continuous_frames(true, false));
    }

    #[test]
    fn snap_preview_blocks_partial_damage_until_its_overlay_is_fully_gone() {
        assert!(snap_preview_allows_partial_damage(false, 0.0));
        assert!(!snap_preview_allows_partial_damage(true, 0.0));
        assert!(!snap_preview_allows_partial_damage(true, 1.0));
        assert!(!snap_preview_allows_partial_damage(false, 0.25));
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

    pub(super) unsafe fn upload_window_color_transform(
        &self,
        gl: &ffi::Gles2,
        transform: Option<crate::backend::wayland_udev::color_pipeline::ColorTransform>,
        scene_linear: bool,
    ) {
        unsafe {
            gl.Uniform1i(self.win_uniforms.scene_linear, i32::from(scene_linear));
            if let Some(transform) = transform {
                let matrix = transform.matrix_column_major();
                gl.Uniform1i(self.win_uniforms.color_managed, 1);
                gl.UniformMatrix3fv(
                    self.win_uniforms.color_matrix,
                    1,
                    ffi::FALSE,
                    matrix.as_ptr(),
                );
                gl.Uniform1i(
                    self.win_uniforms.decode_tf,
                    transform.inverse_eotf.shader_id(),
                );
                gl.Uniform1f(
                    self.win_uniforms.decode_gamma,
                    transform.inverse_eotf.gamma_for_shader(),
                );
                gl.Uniform1i(
                    self.win_uniforms.encode_tf,
                    transform.forward_eotf.shader_id(),
                );
                gl.Uniform1f(
                    self.win_uniforms.encode_gamma,
                    transform.forward_eotf.gamma_for_shader(),
                );
            } else {
                gl.Uniform1i(self.win_uniforms.color_managed, 0);
            }
        }
    }

    pub(super) unsafe fn reset_window_color_transform(&self, gl: &ffi::Gles2) {
        unsafe {
            gl.Uniform1i(self.win_uniforms.color_managed, 0);
        }
    }

    unsafe fn upload_retained_color_plan(&self, gl: &ffi::Gles2, plan: RetainedColorPlan) {
        let locations = match plan.program {
            RetainedTextureProgram::Window => ColorUniformLocations {
                managed: self.win_uniforms.color_managed,
                matrix: self.win_uniforms.color_matrix,
                decode_tf: self.win_uniforms.decode_tf,
                decode_gamma: self.win_uniforms.decode_gamma,
                encode_tf: self.win_uniforms.encode_tf,
                encode_gamma: self.win_uniforms.encode_gamma,
                scene_linear: self.win_uniforms.scene_linear,
            },
            RetainedTextureProgram::Genie => ColorUniformLocations {
                managed: self.genie_uniforms.color_managed,
                matrix: self.genie_uniforms.color_matrix,
                decode_tf: self.genie_uniforms.decode_tf,
                decode_gamma: self.genie_uniforms.decode_gamma,
                encode_tf: self.genie_uniforms.encode_tf,
                encode_gamma: self.genie_uniforms.encode_gamma,
                scene_linear: self.genie_uniforms.scene_linear,
            },
        };

        unsafe {
            gl.Uniform1i(locations.scene_linear, i32::from(plan.scene_linear));
            if let Some(transform) = plan.transform {
                let matrix = transform.matrix_column_major();
                gl.Uniform1i(locations.managed, 1);
                gl.UniformMatrix3fv(locations.matrix, 1, ffi::FALSE, matrix.as_ptr());
                gl.Uniform1i(locations.decode_tf, transform.inverse_eotf.shader_id());
                gl.Uniform1f(
                    locations.decode_gamma,
                    transform.inverse_eotf.gamma_for_shader(),
                );
                gl.Uniform1i(locations.encode_tf, transform.forward_eotf.shader_id());
                gl.Uniform1f(
                    locations.encode_gamma,
                    transform.forward_eotf.gamma_for_shader(),
                );
            } else {
                gl.Uniform1i(locations.managed, 0);
            }
        }
    }

    unsafe fn reset_retained_color_plan(&self, gl: &ffi::Gles2, plan: RetainedColorPlan) {
        let managed = match plan.program {
            RetainedTextureProgram::Window => self.win_uniforms.color_managed,
            RetainedTextureProgram::Genie => self.genie_uniforms.color_managed,
        };
        unsafe {
            // Reset after every retained item, including identity items. This
            // makes the loop robust against reordering and early additions.
            gl.Uniform1i(managed, 0);
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

    pub(super) fn set_rect_uniform(
        &self,
        gl: &ffi::Gles2,
        loc: i32,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    ) {
        unsafe {
            gl.Uniform4f(loc, x, y, w, h);
        }
    }

    // =========================================================================
    // Helper: set a mat4 uniform (u_projection, etc.)
    // =========================================================================

    pub(super) fn set_projection_uniform(&self, gl: &ffi::Gles2, loc: i32, proj: &[f32; 16]) {
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
            // Retired surfaces share the active scene domain. In a common
            // linear frame their retained source plan decodes/maps before
            // blending; the legacy encoded path keeps its direct target plan.
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

                let anim = self.window_animation_frame_for(win);
                let scale = anim.scale.max(0.01);
                let draw_w = w * scale;
                let draw_h = h * scale;
                let draw_x = x + (w - draw_w) * 0.5;
                let draw_y = y + (h - draw_h) * 0.5 + anim.dy;
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
                gl.Uniform1f(self.win_uniforms.desat, 0.0);
                gl.Uniform1f(self.win_uniforms.radius, radius);
                gl.Uniform4f(self.win_uniforms.uv_rect, uv_x, uv_y, uv_w, uv_h);
                let color_plan = retained_color_plan(
                    RetainedTexturePass::CloseFade,
                    win.color_transform,
                    scene_linear_output,
                );
                self.upload_retained_color_plan(gl, color_plan);
                gl.ActiveTexture(ffi::TEXTURE0);
                self.bind_window_texture(gl, texture_owner.tex_id());
                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                self.reset_retained_color_plan(gl, color_plan);
            }

            gl.BindVertexArray(0);
            gl.UseProgram(0);
        }
    }

    fn render_minimized_dock_items(
        &mut self,
        gl: &ffi::Gles2,
        projection: &[f32; 16],
        scene_linear_output: bool,
    ) {
        let preview = self
            .dock_preview
            .as_ref()
            .map(|preview| (preview.window_id, preview.anchor, preview.opacity));
        let targets = self
            .genie_targets
            .iter()
            .filter(|(window_id, _)| {
                self.minimized_windows.contains(window_id)
                    && !self
                        .genie_active
                        .iter()
                        .any(|animation| animation.window_id == **window_id)
                    && self.minimized_static_source_available(**window_id)
            })
            .map(|(&window_id, &target)| (window_id, target))
            .collect::<Vec<_>>();
        if targets.is_empty() {
            return;
        }
        unsafe {
            gl.UseProgram(self.program);
            self.set_projection_uniform(gl, self.win_uniforms.projection, projection);
            gl.Uniform1i(self.win_uniforms.texture, 0);
            gl.Uniform1i(self.win_uniforms.color_managed, 0);
            gl.Uniform1i(
                self.win_uniforms.scene_linear,
                i32::from(scene_linear_output),
            );
            gl.Uniform1f(self.win_uniforms.dim, 1.0);
            gl.Uniform1f(self.win_uniforms.desat, 0.0);
            gl.Uniform1f(self.win_uniforms.ripple_progress, 0.0);
            gl.Uniform1f(self.win_uniforms.ripple_amplitude, 0.0);
            gl.BindVertexArray(self.quad_vao);
            gl.ActiveTexture(ffi::TEXTURE0);

            for (window_id, stable_target) in targets {
                let Some(target) = dock_item_preview_target(window_id, stable_target, preview)
                else {
                    continue;
                };
                // Resolve and draw one item before resolving the next. A lazy
                // CPU upload may evict another raw GPU snapshot; retaining a
                // batch of bare GLuints across that eviction would make an
                // earlier item stale before its draw call.
                let Some(source) =
                    self.minimized_render_source(gl, window_id, ThumbnailPurpose::StaticDockCard)
                else {
                    continue;
                };
                if source.width <= 0.0 || source.height <= 0.0 {
                    continue;
                }
                let fit = (target.width / source.width).min(target.height / source.height);
                let width = (source.width * fit).max(1.0);
                let height = (source.height * fit).max(1.0);
                let x = target.x + (target.width - width) * 0.5;
                let y = target.y + (target.height - height) * 0.5;
                self.set_rect_uniform(gl, self.win_uniforms.rect, x, y, width, height);
                gl.Uniform2f(self.win_uniforms.size, width, height);
                gl.Uniform1f(
                    self.win_uniforms.opacity,
                    if source.has_alpha { -1.0 } else { 1.0 },
                );
                gl.Uniform1f(self.win_uniforms.radius, 5.0_f32.min(height * 0.5));
                gl.Uniform4f(
                    self.win_uniforms.uv_rect,
                    source.uv_rect[0],
                    source.uv_rect[1],
                    source.uv_rect[2],
                    source.uv_rect[3],
                );
                let color_plan = retained_color_plan(
                    RetainedTexturePass::StaticDockItem,
                    source.color_transform,
                    scene_linear_output,
                );
                self.upload_retained_color_plan(gl, color_plan);
                self.bind_window_texture(gl, source.texture);
                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                self.reset_retained_color_plan(gl, color_plan);
            }

            gl.BindVertexArray(0);
            gl.UseProgram(0);
        }
    }

    fn render_dock_preview(
        &mut self,
        gl: &ffi::Gles2,
        projection: &[f32; 16],
        scene_linear_output: bool,
    ) {
        let Some(preview) = self.dock_preview.clone() else {
            return;
        };
        if preview.opacity <= 0.001 {
            return;
        }
        let Some(source) =
            self.minimized_render_source(gl, preview.window_id, ThumbnailPurpose::HoverPreview)
        else {
            return;
        };
        let output_bounds = output_bounds_for_anchor(
            preview.anchor,
            self.monitors.iter().map(|&(_, x, y, w, h, _)| {
                crate::backend::api::CompositorRect::new(x as f32, y as f32, w as f32, h as f32)
            }),
            crate::backend::api::CompositorRect::new(
                0.0,
                0.0,
                self.screen_w as f32,
                self.screen_h as f32,
            ),
        );
        let Some(rect) = preview_rect(
            preview.anchor,
            source.width,
            source.height,
            output_bounds,
            preview.scale,
        ) else {
            return;
        };
        unsafe {
            let spread = 16.0;
            gl.UseProgram(self.shadow_program);
            self.set_projection_uniform(gl, self.shadow_uniforms.projection, projection);
            gl.Uniform1f(self.shadow_uniforms.spread, spread);
            gl.Uniform4f(
                self.shadow_uniforms.shadow_color,
                0.0,
                0.0,
                0.0,
                0.32 * preview.opacity,
            );
            gl.Uniform1f(self.shadow_uniforms.radius, 14.0);
            gl.Uniform2f(self.shadow_uniforms.size, rect.width, rect.height);
            self.set_rect_uniform(
                gl,
                self.shadow_uniforms.rect,
                rect.x - spread,
                rect.y - spread,
                rect.width + spread * 2.0,
                rect.height + spread * 2.0,
            );
            gl.BindVertexArray(self.quad_vao);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);

            gl.UseProgram(self.program);
            self.set_projection_uniform(gl, self.win_uniforms.projection, projection);
            gl.Uniform1i(self.win_uniforms.texture, 0);
            let color_plan = retained_color_plan(
                RetainedTexturePass::DockPreview,
                source.color_transform,
                scene_linear_output,
            );
            self.upload_retained_color_plan(gl, color_plan);
            self.set_rect_uniform(
                gl,
                self.win_uniforms.rect,
                rect.x,
                rect.y,
                rect.width,
                rect.height,
            );
            gl.Uniform2f(self.win_uniforms.size, rect.width, rect.height);
            gl.Uniform1f(
                self.win_uniforms.opacity,
                if source.has_alpha {
                    -preview.opacity
                } else {
                    preview.opacity
                },
            );
            gl.Uniform1f(self.win_uniforms.radius, 14.0);
            gl.Uniform1f(self.win_uniforms.dim, 1.0);
            gl.Uniform1f(self.win_uniforms.desat, 0.0);
            gl.Uniform4f(
                self.win_uniforms.uv_rect,
                source.uv_rect[0],
                source.uv_rect[1],
                source.uv_rect[2],
                source.uv_rect[3],
            );
            gl.Uniform1f(self.win_uniforms.ripple_progress, 0.0);
            gl.Uniform1f(self.win_uniforms.ripple_amplitude, 0.0);
            gl.ActiveTexture(ffi::TEXTURE0);
            self.bind_window_texture(gl, source.texture);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
            self.reset_retained_color_plan(gl, color_plan);
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

    // Linearize selected encoded output_fbo storage into the common working
    // target so subsequent u_scene_linear draws blend correctly. Initial scene
    // ingress uses the legacy sRGB default; fallback overview re-entry uses the
    // preceding conservative encode. Blending is disabled because every
    // selected pixel is a full overwrite.
    fn dispatch_scene_linear_decode_pass(
        &self,
        gl: &ffi::Gles2,
        projection: &[f32; 16],
        decode_tf: i32,
        decode_gamma: f32,
        scissor: Option<[i32; 4]>,
    ) {
        unsafe {
            gl.BindFramebuffer(ffi::FRAMEBUFFER, self.linear_fbo);
            gl.Viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
            if let Some([x, y, width, height]) = scissor {
                gl.Enable(ffi::SCISSOR_TEST);
                gl.Scissor(x, y, width, height);
            }
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
            gl.Uniform1i(self.scene_linear_decode_uniforms.decode_tf, decode_tf);
            gl.Uniform1f(self.scene_linear_decode_uniforms.decode_gamma, decode_gamma);
            gl.ActiveTexture(ffi::TEXTURE0);
            gl.BindTexture(ffi::TEXTURE_2D, self.output_texture);
            gl.BindVertexArray(self.quad_vao);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
            gl.BindVertexArray(0);
            gl.UseProgram(0);
            gl.Enable(ffi::BLEND);
            if scissor.is_some() {
                gl.Disable(ffi::SCISSOR_TEST);
            }
        }
    }

    // Finalize common linear-sRGB into output_fbo with the supplied gamut
    // matrix and forward transfer. This serves a physical output region or the
    // whole-frame sRGB fallback. encode_tf < 0 means "sRGB default";
    // encode_gamma is consulted only for TF_POWER. Blending is disabled.
    fn dispatch_scene_linear_encode_pass(
        &self,
        gl: &ffi::Gles2,
        projection: &[f32; 16],
        encode_tf: i32,
        encode_gamma: f32,
        color_matrix_row_major: [f32; 9],
        scissor: Option<[i32; 4]>,
    ) {
        unsafe {
            gl.BindFramebuffer(ffi::FRAMEBUFFER, self.output_fbo);
            gl.Viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
            if let Some([x, y, width, height]) = scissor {
                gl.Enable(ffi::SCISSOR_TEST);
                gl.Scissor(x, y, width, height);
            }
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
            let color_matrix = crate::backend::wayland_udev::color_pipeline::matrix_to_column_major(
                color_matrix_row_major,
            );
            gl.UniformMatrix3fv(
                self.scene_linear_encode_uniforms.color_matrix,
                1,
                ffi::FALSE as u8,
                color_matrix.as_ptr(),
            );
            gl.ActiveTexture(ffi::TEXTURE0);
            gl.BindTexture(ffi::TEXTURE_2D, self.linear_texture);
            gl.BindVertexArray(self.quad_vao);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
            gl.BindVertexArray(0);
            gl.UseProgram(0);
            gl.Enable(ffi::BLEND);
            if scissor.is_some() {
                gl.Disable(ffi::SCISSOR_TEST);
            }
        }
    }

    /// Finalize a common linear-sRGB frame into independently described output
    /// regions. The full-frame path clears layout gaps to opaque black; partial
    /// repair touches only damage intersections and preserves prior encoded
    /// pixels elsewhere. KMS owns the final OETF/CTM stages when their flags
    /// are active, so the corresponding shader operation becomes identity.
    fn dispatch_output_color_regions(
        &self,
        gl: &ffi::Gles2,
        projection: &[f32; 16],
        regions: &[crate::backend::wayland_udev::color_pipeline::OutputColorRegion],
        hw_encode_active: bool,
        hw_ctm_active: bool,
        damage_scissor: Option<[i32; 4]>,
    ) {
        use crate::backend::wayland_udev::color_pipeline::{IDENTITY_CTM, TransferKind};

        unsafe {
            gl.BindFramebuffer(ffi::FRAMEBUFFER, self.output_fbo);
            gl.Viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
            gl.Disable(ffi::SCISSOR_TEST);
            if damage_scissor.is_none() {
                gl.Disable(ffi::BLEND);
                gl.ClearColor(0.0, 0.0, 0.0, 1.0);
                gl.Clear(ffi::COLOR_BUFFER_BIT);
                gl.Enable(ffi::BLEND);
            }
        }

        for region in regions {
            let [x, y, width, height] = region.rect;
            let Some(mut scissor) = overview_monitor_scissor(
                (x, y, width.max(0) as u32, height.max(0) as u32),
                self.screen_w,
                self.screen_h,
            ) else {
                continue;
            };
            if let Some(damage) = damage_scissor {
                let Some(intersection) = intersect_scissors(scissor, damage) else {
                    continue;
                };
                scissor = intersection;
            }

            let transfer = if hw_encode_active {
                TransferKind::Linear
            } else {
                region.output_tf
            };
            let matrix = if hw_ctm_active {
                IDENTITY_CTM
            } else {
                region.working_to_output_row_major
            };
            self.dispatch_scene_linear_encode_pass(
                gl,
                projection,
                transfer.shader_id(),
                transfer.gamma_for_shader(),
                matrix,
                Some(scissor),
            );
        }

        unsafe {
            gl.BindFramebuffer(ffi::FRAMEBUFFER, self.output_fbo);
            gl.Viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
            gl.Disable(ffi::SCISSOR_TEST);
            self.enable_premultiplied_blend(gl);
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

        // Expand each window rect to cover every compositor decoration.
        let border_and_shadow_margin = self.border_width
            + if self.shadow_enabled && self.shadow_radius > 0.0 {
                self.shadow_spread
                    + self.shadow_radius
                    + self.shadow_offset[0].abs().max(self.shadow_offset[1].abs())
            } else {
                0.0
            };
        let glow_margin = {
            let config = crate::config::CONFIG.load();
            WindowGlowSettings::from_behavior(config.behavior()).damage_margin() as f32
        };
        let margin = border_and_shadow_margin.max(glow_margin);

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

    pub(crate) fn sync_scene_linear_target(&mut self, gl: &ffi::Gles2) {
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

    /// Refresh compositor-side direct-scanout eligibility diagnostics after
    /// effect ticks have committed their state for this frame.
    fn update_direct_scanout_diagnostics(
        &mut self,
        scene: &[(u64, i32, i32, u32, u32)],
        focused: Option<u64>,
    ) {
        let diagnostic_output_rect = crate::backend::api::CompositorRect::new(
            0.0,
            0.0,
            self.screen_w as f32,
            self.screen_h as f32,
        );
        if let Some(reason) = self.direct_scanout_block_reason(diagnostic_output_rect) {
            self.direct_scanout_mgr.block_for_composition(reason);
            return;
        }
        if self.has_system_ui() {
            self.direct_scanout_mgr
                .block_for_composition("JWM system UI requires composition");
            return;
        }
        if self.recording_requires_composition() {
            self.direct_scanout_mgr
                .block_for_composition("screen recording requires composition");
            return;
        }

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

    /// Main rendering function. Composites the entire scene into the output FBO.
    /// `scene` is a list of (window_id, x, y, w, h) in bottom-to-top order.
    /// `focused` is the currently focused window.
    /// `linear_tail_safe` means every visible late overlay can consume the
    /// common linear-sRGB working domain. Such frames defer output conversion
    /// until immediately before capture/scanout. Unsafe frames explicitly fall
    /// back to the historical global sRGB domain.
    ///
    /// `hw_encode_active` / `hw_ctm_active` identify output stages owned by KMS.
    /// `software_output_regions` supplies the physical framebuffer partitions
    /// and remaining gamut/transfer work for shader delivery.
    /// Returns true if a frame was rendered (false if skipped due to no changes).
    pub(crate) fn render_frame(
        &mut self,
        gl: &ffi::Gles2,
        scene: &[(u64, i32, i32, u32, u32)],
        focused: Option<u64>,
        linear_tail_safe: bool,
        hw_encode_active: bool,
        hw_ctm_active: bool,
        software_output_regions: Option<
            &[crate::backend::wayland_udev::color_pipeline::OutputColorRegion],
        >,
    ) -> bool {
        let previous_frame_was_deferred = previous_frame_requires_srgb_transition_snapshot(
            self.last_output_color_frame_state.as_ref(),
        );
        let output_color_state = OutputColorFrameState {
            linear_tail_safe,
            hw_encode_active,
            hw_ctm_active,
            software_regions: software_output_regions.map(|regions| regions.to_vec()),
        };
        if self.last_output_color_frame_state.as_ref() != Some(&output_color_state) {
            self.last_output_color_frame_state = Some(output_color_state);
            self.force_full_damage_next = true;
            self.needs_render = true;
        }

        // Hidden imports arrive as ordinary WindowState owners. Capture the
        // bounded tier before full-retained admission can evict an older
        // source, then run once more for any fallback armed while settling.
        self.capture_pending_minimized_snapshots(gl);
        self.settle_pending_minimized_visuals();
        self.capture_pending_minimized_snapshots(gl);
        self.start_pending_genie_restores(scene);
        // Last frame's frosted-glass backdrop describes a framebuffer that is
        // about to be overwritten; the first panel that needs one recaptures.
        self.glass_backdrop = None;
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
        let attention_active = attention_requires_continuous_frames(
            self.attention_animation_enabled,
            self.windows.values().any(|window| window.is_urgent),
        );
        if !self.needs_render
            && !self.screenshot_requests.has_pending()
            && !self.screenshot_readback.has_pending()
            && !self.recording.is_active()
            && !recording_transition_pending
            && !postprocess_continuous
            && !edge_glow_continuous
            && !attention_active
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
            if previous_frame_was_deferred && self.linear_fbo != 0 {
                use crate::backend::wayland_udev::color_pipeline::{IDENTITY_CTM, TransferKind};
                // Deferred delivery leaves the canonical previous workspace
                // intact in linear_fbo while output_fbo may hold linear KMS
                // pixels or independently encoded output regions. Normalize a
                // temporary global-sRGB copy before the legacy transition
                // shader captures it.
                let projection = ortho(0.0, self.screen_w as f32, self.screen_h as f32, 0.0);
                let fallback = TransferKind::Srgb;
                self.dispatch_scene_linear_encode_pass(
                    gl,
                    &projection,
                    fallback.shader_id(),
                    fallback.gamma_for_shader(),
                    IDENTITY_CTM,
                    None,
                );
            }
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
        // Tick first so a restore that reaches progress zero can return to the
        // ordinary scene in this same frame. Filtering before the tick would
        // otherwise leave a one-frame hole between mesh retirement and the
        // live window draw.
        let filtered_scene;
        let scene = if self.genie_active.iter().any(|animation| {
            animation.direction == crate::backend::compositor_common::genie::GenieDirection::Restore
        }) {
            filtered_scene = scene
                .iter()
                .copied()
                .filter(|(window_id, ..)| {
                    !self.genie_active.iter().any(|animation| {
                        animation.window_id == *window_id
                            && animation.direction
                                == crate::backend::compositor_common::genie::GenieDirection::Restore
                    })
                })
                .collect::<Vec<_>>();
            filtered_scene.as_slice()
        } else {
            scene
        };
        self.tick_wobbly(effect_dt);
        self.tick_particles(effect_dt);
        self.tick_motion_trails();
        self.tick_snap_preview(effect_dt);
        self.tick_overview(effect_dt);
        self.tick_overview_prism(effect_dt);
        self.tick_peek(effect_dt);
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

        // Direct scanout eligibility is diagnostics only; the actual zero-copy
        // decision lives in udev_kms.rs. Evaluate it after terminal animation
        // cleanup and one-shot activation so this frame's telemetry describes
        // exactly the visual state that is about to be drawn.
        self.update_direct_scanout_diagnostics(scene, focused);

        // Motion trail keeps the loop ticking until trails drain to empty,
        // even if the user has stopped moving the window.
        let motion_trail_active =
            self.motion_trail_enabled && self.windows.values().any(|w| !w.motion_trail.is_empty());

        // Determine if anything needs rendering
        let any_animating = self.has_active_animations()
            || self.transition_active
            || focus_highlight_active
            || motion_trail_active
            || attention_active
            || !self.genie_active.is_empty();

        // These operations need a frame even on an otherwise static desktop.
        // Keep the demand calculation next to the other compositor work so
        // KMS scheduling, screenshots, and recording all agree on liveness.
        let screenshot_pending =
            self.screenshot_requests.has_pending() || self.screenshot_readback.has_pending();
        // Not "recording is on" but "a recording frame is due": the encoder
        // consumes `recording_fps` frames a second, and compositing the whole
        // screen more often than that is work no one ever reads.
        let recording_frame_due = self.recording.frame_due();

        let force_render = any_animating
            || postprocess_continuous
            || self.debug_hud_enabled
            || edge_glow_continuous
            || screenshot_pending
            || recording_frame_due;

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
        // Recording deliberately does not re-arm the flag: `next_wakeup` carries
        // its deadline to the event loop, which sleeps until the next capture
        // instead of re-rendering the screen at display rate in between.
        self.needs_render = any_animating
            || self.has_active_animations()
            || postprocess_continuous
            || edge_glow_continuous
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
            && self.peek_opacity <= 0.0001
            // Snap preview is drawn after the scene scissor is released. A
            // stable visible preview must therefore keep the whole frame out
            // of partial repair, or unchanged pixels would be alpha-blended a
            // second time and the previous rectangle could survive a move.
            && snap_preview_allows_partial_damage(
                self.snap_preview.is_some(),
                self.snap_preview_opacity,
            )
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
        let frame_config = crate::config::CONFIG.load();
        let glow_settings = WindowGlowSettings::from_behavior(frame_config.behavior());

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

                    // Modulate shadow alpha by fade; unfocused windows can
                    // cast a weaker shadow so the focused one reads deeper.
                    let fade = wt.fade_opacity;
                    let focus_scale = if focused == Some(win_id) {
                        1.0
                    } else {
                        self.shadow_inactive_opacity
                    };
                    let sa_faded = sa * fade * focus_scale;
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
        // 7b. Directional client-window glow underlay
        // =================================================================
        self.frame_profiler.zone_start("window_glow");
        if glow_settings.damage_margin() > 0 {
            unsafe {
                gl.UseProgram(self.border_program);
                self.set_projection_uniform(gl, self.border_uniforms.projection, &projection);
                // This pass is written into the encoded output FBO before the
                // optional scene-linear decode, matching wallpaper and shadows.
                gl.Uniform1i(self.border_uniforms.scene_linear, 0);
                gl.BindVertexArray(self.quad_vao);

                for &(win_id, x, y, w, h) in visible_scene {
                    let Some(wt) = self.windows.get(&win_id) else {
                        continue;
                    };
                    if wt.gl_texture.is_none() {
                        continue;
                    }
                    let Some(style) = glow_settings.style_for(WindowGlowTarget {
                        focused: focused == Some(win_id),
                        fullscreen: wt.is_fullscreen,
                        override_redirect: false,
                        shaped: wt.is_shaped,
                        class_name: &wt.class_name,
                        fade: wt.fade_opacity,
                    }) else {
                        continue;
                    };

                    let radius = if !wt.class_name.is_empty()
                        && Self::class_matches_exclude(
                            &wt.class_name,
                            &self.rounded_corners_exclude,
                        ) {
                        0.0
                    } else {
                        wt.corner_radius_override
                            .or_else(|| self.lookup_corner_radius_rule(&wt.class_name))
                            .unwrap_or(self.corner_radius)
                    };
                    let anim = self.window_animation_frame_for(wt);
                    let scale = anim.scale;
                    let draw_w = w as f32 * scale;
                    let draw_h = h as f32 * scale;
                    let draw_x = x as f32 + (w as f32 - draw_w) * 0.5;
                    let draw_y = y as f32 + (h as f32 - draw_h) * 0.5 + anim.dy;
                    if draw_w <= 0.0 || draw_h <= 0.0 {
                        continue;
                    }

                    gl.Uniform4f(
                        self.border_uniforms.border_color,
                        style.color[0],
                        style.color[1],
                        style.color[2],
                        style.color[3],
                    );
                    gl.Uniform1f(self.border_uniforms.border_width, -style.radius);
                    gl.Uniform1f(self.border_uniforms.radius, radius.max(0.0));
                    gl.Uniform1f(self.border_uniforms.radius_top, radius.max(0.0));
                    // In glow mode u_size is the unexpanded client rectangle.
                    gl.Uniform2f(self.border_uniforms.size, draw_w, draw_h);
                    self.set_rect_uniform(
                        gl,
                        self.border_uniforms.rect,
                        draw_x - style.radius,
                        draw_y - style.radius,
                        draw_w + 2.0 * style.radius,
                        draw_h + 2.0 * style.radius,
                    );
                    gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                }

                // Avoid leaking the negative glow-mode sentinel into later
                // border-program users that do not otherwise need an outline.
                gl.Uniform1f(self.border_uniforms.border_width, 0.0);
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

            const FNV_PRIME: u64 = 0x100000001b3;
            let glow_hash = {
                let mut hash = 0xcbf29ce484222325u64;
                let mut any_visible = false;
                for &(win_id, _, _, _, _) in visible_scene {
                    let Some(wt) = self.windows.get(&win_id) else {
                        continue;
                    };
                    if wt.gl_texture.is_none() {
                        continue;
                    }
                    let Some(style) = glow_settings.style_for(WindowGlowTarget {
                        focused: focused == Some(win_id),
                        fullscreen: wt.is_fullscreen,
                        override_redirect: false,
                        shaped: wt.is_shaped,
                        class_name: &wt.class_name,
                        fade: wt.fade_opacity,
                    }) else {
                        continue;
                    };
                    any_visible = true;
                    hash ^= win_id;
                    hash = hash.wrapping_mul(FNV_PRIME);
                    for word in style.hash_words() {
                        hash ^= word;
                        hash = hash.wrapping_mul(FNV_PRIME);
                    }
                }
                if any_visible { hash } else { 0 }
            };
            let current_hash = self
                .compute_window_positions_hash()
                .wrapping_mul(FNV_PRIME)
                .wrapping_add(glow_hash);
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
        // window state. Unlike X11 there is no per-delta move hook here, so the
        // scene position of the previous frame is what gets recorded; the
        // shared ring buffer applies the same distance spacing either way.
        if self.motion_trail_enabled && self.motion_trail_frames > 0 {
            let frames = self.motion_trail_frames;
            let opacity = self.motion_trail_opacity;
            for &(win_id, x, y, w, h) in visible_scene {
                if let Some(wt) = self.windows.get_mut(&win_id) {
                    if wt.is_moving {
                        let params = MotionTrailParams::new(frames, opacity, w as f32, h as f32);
                        wt.motion_trail.record_position(x as f32, y as f32, &params);
                    } else {
                        wt.motion_trail.sync_position(x as f32, y as f32);
                    }
                }
            }
        }

        // =================================================================
        // 9. Draw windows (back-to-front)
        // =================================================================
        // When scene-linear compositing is active, decode the currently
        // encoded output_fbo (wallpaper + shadows + blur) into linear_fbo,
        // then route the window-draw pass there. The frame boundary either
        // runs the output encode shader or leaves the scene linear for a CRTC
        // OETF; linear-aware overlays bind the resulting domain explicitly.
        let scene_linear_active = self.linear_fbo != 0;
        if scene_linear_active {
            // Wallpaper, shadows and the other pre-window producers are legacy
            // sRGB. A negative discriminant deliberately selects the decoder's
            // sRGB fallback while preserving any active damage scissor.
            self.dispatch_scene_linear_decode_pass(gl, &projection, -1, 1.0, None);
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
                let desat = if is_focused {
                    0.0
                } else {
                    self.inactive_desaturate
                };
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
                let anim = self.window_animation_frame_for(wt);
                let scale = anim.scale;
                let (draw_x, draw_y, draw_w, draw_h) = if (scale - 1.0).abs() > f32::EPSILON {
                    let cw = w as f32 * scale;
                    let ch = h as f32 * scale;
                    let cx = x as f32 + (w as f32 - cw) * 0.5;
                    let cy = y as f32 + (h as f32 - ch) * 0.5 + anim.dy;
                    (cx, cy, cw, ch)
                } else {
                    (x as f32, y as f32 + anim.dy, w as f32, h as f32)
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
                    gl.Uniform1f(self.win_uniforms.desat, 0.0);
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
                if self.motion_trail_enabled && !wt.motion_trail.is_empty() {
                    let trail_params = MotionTrailParams::new(
                        self.motion_trail_frames,
                        self.motion_trail_opacity,
                        draw_w,
                        draw_h,
                    );
                    gl.Uniform4f(self.win_uniforms.uv_rect, uv_x, uv_y, uv_w, uv_h);
                    gl.ActiveTexture(ffi::TEXTURE0);
                    gl.BindTexture(ffi::TEXTURE_2D, texture);
                    gl.Uniform1f(self.win_uniforms.radius, radius);
                    gl.Uniform1f(self.win_uniforms.dim, 0.7);
                    gl.Uniform1f(self.win_uniforms.desat, 0.0);
                    self.upload_window_color_transform(gl, wt.color_transform, scene_linear_active);
                    for ghost in wt.motion_trail.ghosts(
                        std::time::Instant::now(),
                        &trail_params,
                        draw_w,
                        draw_h,
                    ) {
                        let ghost_layer = (ghost.opacity * layer_opacity).clamp(0.0, 1.0);
                        gl.Uniform1f(
                            self.win_uniforms.opacity,
                            if use_texture_alpha {
                                -ghost_layer
                            } else {
                                ghost_layer
                            },
                        );
                        gl.Uniform2f(self.win_uniforms.size, ghost.width, ghost.height);
                        self.set_rect_uniform(
                            gl,
                            self.win_uniforms.rect,
                            ghost.x,
                            ghost.y,
                            ghost.width,
                            ghost.height,
                        );
                        gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                    }
                    self.reset_window_color_transform(gl);
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
                    gl.Uniform1f(self.win_uniforms.desat, desat);
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
                    // Normalize runtime uploads to the same column-major/FALSE
                    // layout used by uniform capture and restore.
                    if let Some(t) = wt.color_transform.as_ref() {
                        let matrix = t.matrix_column_major();
                        gl.Uniform1i(self.win_uniforms.color_managed, 1);
                        gl.UniformMatrix3fv(
                            self.win_uniforms.color_matrix,
                            1,
                            ffi::FALSE,
                            matrix.as_ptr(),
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

        let output_route = frame_output_route(
            scene_linear_active,
            linear_tail_safe,
            hw_encode_active,
            hw_ctm_active,
            software_output_regions.is_some(),
        );
        // Retained-window and border passes are color-domain aware, so keep
        // them in the common FP16 target even when a later encoded-only effect
        // will force the global-sRGB fallback. Encoding at the old main-window
        // boundary made Genie/Dock transforms write linear RGB into an encoded
        // target because their canonical surface plans intentionally end in
        // the common working space.
        let overlay_scene_linear = scene_linear_active;
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
            self.render_close_fades(gl, &projection, overlay_scene_linear);
            self.frame_profiler.zone_end();
        }

        // =================================================================
        // 9c. Genie minimize animations (mirror X11 pass 2b)
        // =================================================================
        if !self.genie_active.is_empty() {
            self.frame_profiler.zone_start("genie");
            let genie_duration_secs = self.genie_duration_ms.max(1) as f32 / 1000.0;
            unsafe {
                gl.UseProgram(self.genie_program);
                self.set_projection_uniform(gl, self.genie_uniforms.projection, &projection);
                gl.Uniform1i(self.genie_uniforms.texture, 0);
                gl.Uniform1f(self.genie_uniforms.radius, 0.0);
                gl.Uniform1i(self.genie_uniforms.color_managed, 0);
                gl.Uniform1i(
                    self.genie_uniforms.scene_linear,
                    i32::from(overlay_scene_linear),
                );
                gl.Uniform1f(self.genie_uniforms.ripple_progress, 0.0);
                gl.Uniform1f(self.genie_uniforms.ripple_amplitude, 0.0);
                let grid = 12i32;
                gl.Uniform1i(self.genie_uniforms.grid_size, grid);
                gl.BindVertexArray(self.quad_vao);

                for ga in &self.genie_active {
                    let color_plan = retained_color_plan(
                        RetainedTexturePass::Genie,
                        ga.color_transform,
                        overlay_scene_linear,
                    );
                    self.upload_retained_color_plan(gl, color_plan);
                    let (progress, _) = genie_progress(
                        ga.start_progress,
                        ga.direction,
                        ga.start.elapsed().as_secs_f32(),
                        genie_duration_secs,
                    );
                    let opacity = 1.0 - progress;
                    self.set_rect_uniform(gl, self.genie_uniforms.rect, ga.x, ga.y, ga.w, ga.h);
                    gl.Uniform2f(self.genie_uniforms.size, ga.w, ga.h);
                    gl.Uniform1f(self.genie_uniforms.progress, progress);
                    let (dock_x, dock_y) = ga.target.center();
                    gl.Uniform2f(self.genie_uniforms.dock_pos, dock_x, dock_y);
                    gl.Uniform2f(
                        self.genie_uniforms.dock_size,
                        ga.target.width,
                        ga.target.height,
                    );
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
                    self.reset_retained_color_plan(gl, color_plan);
                }

                gl.BindVertexArray(0);
                gl.UseProgram(0);
            }
            self.frame_profiler.zone_end();
        }

        self.render_minimized_dock_items(gl, &projection, overlay_scene_linear);
        self.render_dock_preview(gl, &projection, overlay_scene_linear);

        // =================================================================
        // 10. Draw borders (focused and urgent windows)
        // =================================================================
        self.frame_profiler.zone_start("borders");
        if self.border_enabled || attention_active {
            unsafe {
                gl.UseProgram(self.border_program);
                self.set_projection_uniform(gl, self.border_uniforms.projection, &projection);
                gl.Uniform1i(
                    self.border_uniforms.scene_linear,
                    i32::from(overlay_scene_linear),
                );
                gl.BindVertexArray(self.quad_vao);

                for &(win_id, x, y, w, h) in visible_scene {
                    let wt = match self.windows.get(&win_id) {
                        Some(wt) => wt,
                        None => continue,
                    };

                    let is_focused = focused == Some(win_id);
                    let attention_active_for_win =
                        attention_signal_active(self.attention_animation_enabled, wt.is_urgent);
                    // An attention border is an accessibility/status signal,
                    // not ordinary decoration. It remains visible when normal
                    // borders are disabled, without accidentally enabling the
                    // focused border on unrelated windows.
                    if !self.border_enabled && !attention_active_for_win {
                        continue;
                    }
                    if !is_focused && !attention_active_for_win {
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

                    let anim = self.window_animation_frame_for(wt);
                    let scale = anim.scale;
                    let (draw_x, draw_y, draw_w, draw_h) = if (scale - 1.0).abs() > f32::EPSILON {
                        let cw = w as f32 * scale;
                        let ch = h as f32 * scale;
                        let cx = x as f32 + (w as f32 - cw) * 0.5;
                        let cy = y as f32 + (h as f32 - ch) * 0.5 + anim.dy;
                        (cx, cy, cw, ch)
                    } else {
                        (x as f32, y as f32 + anim.dy, w as f32, h as f32)
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
                    let attention_style = attention_active_for_win.then(|| {
                        attention_border_style(
                            self.attention_color,
                            self.compositor_start_time.elapsed().as_secs_f32(),
                            fade,
                            self.border_enabled,
                            self.border_width,
                        )
                    });

                    let border_color = if highlight_for_win {
                        let (_, start) = self.focus_highlight_start.unwrap();
                        let elapsed_ms = start.elapsed().as_millis() as f32;
                        let dur = self.focus_highlight_duration_ms.max(1) as f32;
                        let pulse = ((elapsed_ms / dur * std::f32::consts::PI).sin()).abs();
                        let [r, g, b, a] = self.focus_highlight_color;
                        [r, g, b, a * pulse * fade]
                    } else if let Some(style) = attention_style {
                        style.color
                    } else {
                        let c = self.border_color_focused;
                        [c[0], c[1], c[2], c[3] * fade]
                    };
                    let border_width = if highlight_for_win {
                        (self.border_width + 2.0).max(3.0)
                    } else if let Some(style) = attention_style {
                        style.width
                    } else {
                        self.border_width
                    };

                    let bdr_x = draw_x - border_width;
                    let bdr_y = draw_y - border_width;
                    let bdr_w = draw_w + 2.0 * border_width;
                    let bdr_h = draw_h + 2.0 * border_width;

                    // Concentric corners: the ring's inner edge sits border_width
                    // inside the outer rect, so the outer radius must be
                    // radius + border_width for the inner curve to match the
                    // window's radius (no wedge gap at corners).
                    let outer_radius = if radius > 0.0 {
                        radius + border_width
                    } else {
                        0.0
                    };

                    // The focused window's ordinary border upgrades to the
                    // two-color gradient ring. Focus pulse and urgent borders
                    // keep their flat signal colors.
                    let use_gradient = self.border_gradient_enabled
                        && !highlight_for_win
                        && !attention_active_for_win;

                    if use_gradient {
                        let angle = (self.border_gradient_angle
                            + self.border_gradient_speed
                                * self.compositor_start_time.elapsed().as_secs_f32())
                        .to_radians();
                        let [ar, ag, ab, aa] = self.border_gradient_color_a;
                        let [br, bg, bb, ba] = self.border_gradient_color_b;
                        gl.UseProgram(self.gradient_border_program);
                        self.set_projection_uniform(
                            gl,
                            self.gradient_border_uniforms.projection,
                            &projection,
                        );
                        gl.Uniform1i(
                            self.gradient_border_uniforms.scene_linear,
                            i32::from(overlay_scene_linear),
                        );
                        gl.Uniform4f(self.gradient_border_uniforms.color_a, ar, ag, ab, aa * fade);
                        gl.Uniform4f(self.gradient_border_uniforms.color_b, br, bg, bb, ba * fade);
                        gl.Uniform1f(self.gradient_border_uniforms.gradient_angle, angle);
                        gl.Uniform1f(self.gradient_border_uniforms.border_width, border_width);
                        gl.Uniform1f(self.gradient_border_uniforms.radius, outer_radius);
                        gl.Uniform1f(self.gradient_border_uniforms.radius_top, outer_radius);
                        gl.Uniform2f(self.gradient_border_uniforms.size, bdr_w, bdr_h);
                        self.set_rect_uniform(
                            gl,
                            self.gradient_border_uniforms.rect,
                            bdr_x,
                            bdr_y,
                            bdr_w,
                            bdr_h,
                        );
                        gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                        // Restore the flat border program; its projection and
                        // scene_linear uniforms are per-program state and stay
                        // valid from the pre-loop setup.
                        gl.UseProgram(self.border_program);
                    } else {
                        gl.Uniform4f(
                            self.border_uniforms.border_color,
                            border_color[0],
                            border_color[1],
                            border_color[2],
                            border_color[3],
                        );
                        gl.Uniform1f(self.border_uniforms.border_width, border_width);
                        gl.Uniform1f(self.border_uniforms.radius, outer_radius);
                        gl.Uniform1f(self.border_uniforms.radius_top, outer_radius);
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

        if output_route == FrameOutputRoute::EarlySrgbFallback {
            use crate::backend::wayland_udev::color_pipeline::{IDENTITY_CTM, TransferKind};
            // The passes above are linear-aware. Convert only at the first
            // encoded-only layer so their existing z-order remains intact and
            // all following effects see the historical global-sRGB domain.
            let fallback = TransferKind::Srgb;
            self.dispatch_scene_linear_encode_pass(
                gl,
                &projection,
                fallback.shader_id(),
                fallback.gamma_for_shader(),
                IDENTITY_CTM,
                None,
            );
            unsafe {
                self.sync_overlay_color_domain(gl, false);
            }
        }

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
        if self.overview_entries.is_empty() {
            if !self.overview_title_textures.is_empty() {
                self.clear_overview_textures(gl);
            }
        } else if self.overview_opacity > 0.0 {
            match overview_render_route(output_route) {
                OverviewRenderRoute::LegacyEncoded => {
                    self.render_overview(gl, &projection, false);
                }
                OverviewRenderRoute::DirectLinear => {
                    self.render_overview(gl, &projection, true);
                }
                OverviewRenderRoute::SoftwareReentry => {
                    use crate::backend::wayland_udev::color_pipeline::{
                        IDENTITY_CTM, TransferKind,
                    };
                    // An incompatible late overlay selected the explicit global
                    // sRGB fallback. Re-enter common linear light only over the
                    // overview monitor, then restore that same sRGB region.
                    if let Some(scissor) = overview_monitor_scissor(
                        self.overview_monitor,
                        self.screen_w,
                        self.screen_h,
                    ) {
                        let fallback = TransferKind::Srgb;
                        self.dispatch_scene_linear_decode_pass(
                            gl,
                            &projection,
                            fallback.shader_id(),
                            fallback.gamma_for_shader(),
                            Some(scissor),
                        );
                        self.render_overview(gl, &projection, true);
                        self.dispatch_scene_linear_encode_pass(
                            gl,
                            &projection,
                            fallback.shader_id(),
                            fallback.gamma_for_shader(),
                            IDENTITY_CTM,
                            Some(scissor),
                        );
                        unsafe {
                            // The title and strip deliberately selected the
                            // temporary linear domain. Restore the persistent
                            // shared programs before Expose/Peek/system UI draw
                            // into the now target-encoded output FBO.
                            self.sync_overlay_color_domain(gl, false);
                        }
                    }
                }
            }
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
        if self.peek_opacity > 0.0 {
            self.render_peek_mode(gl, &projection, focused, scene);
        }

        // =================================================================
        // 15c. Tab bar for window groups
        // =================================================================
        if self.window_tabs_enabled && !self.window_groups.is_empty() {
            self.refresh_tab_titles(gl);
            self.render_tab_bar(gl, &projection);
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

        // =================================================================
        // 18b. Output delivery
        // =================================================================
        // Linear-tail-safe frames remain in the common FP16 target through
        // every compatible late overlay. Convert only now, immediately before
        // screenshots/recording and KMS consume output_texture.
        match output_route {
            FrameOutputRoute::DeferredHardware => {
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
                    gl.Disable(ffi::SCISSOR_TEST);
                    self.enable_premultiplied_blend(gl);
                }
            }
            FrameOutputRoute::DeferredRegions => {
                if let Some(regions) = software_output_regions {
                    self.dispatch_output_color_regions(
                        gl,
                        &projection,
                        regions,
                        hw_encode_active,
                        hw_ctm_active,
                        damage_scissor,
                    );
                } else {
                    debug_assert!(false, "deferred output route requires software regions");
                }
            }
            FrameOutputRoute::LegacyEncoded | FrameOutputRoute::EarlySrgbFallback => {}
        }

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
        // 19b. Debug HUD — `debug_hud_extended` only adds sections to the
        // card, so the basic HUD must draw on its own like it does on X11.
        // =================================================================
        if self.debug_hud_enabled {
            unsafe {
                gl.BindFramebuffer(ffi::FRAMEBUFFER, self.output_fbo);
                self.render_debug_hud(gl, &projection);
            }
        }

        // =================================================================
        // 19c. Annotations overlay
        // =================================================================
        // Shapes first, then strokes over them: a redaction bar must not land
        // on top of the arrow that points at it. The screenshot toolbar comes
        // last of all, since it floats above everything it edits.
        if self.annotation_active {
            unsafe {
                gl.BindFramebuffer(ffi::FRAMEBUFFER, self.output_fbo);
            }
            self.refresh_annotation_labels(gl);
            self.render_annotation_shapes(gl, &projection);
            if !self.annotation_strokes.is_empty() {
                unsafe {
                    self.render_annotations(gl, &projection);
                }
            }
        }
        if self.screenshot_toolbar.is_some() {
            unsafe {
                gl.BindFramebuffer(ffi::FRAMEBUFFER, self.output_fbo);
            }
            self.refresh_screenshot_toolbar(gl);
            self.render_screenshot_toolbar(gl, &projection);
        }

        // Toast cards sit above clients but under the modal system UI (its
        // scrim dims them; the lock screen hides them).
        unsafe {
            gl.BindFramebuffer(ffi::FRAMEBUFFER, self.output_fbo);
            self.render_toasts(gl, &projection);
            self.render_osd(gl, &projection);
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
        let mut rows = hud::HudRows::default();
        rows.section("Frame");
        rows.stat("FPS", format!("{:.1}", self.fps));
        rows.stat("Frame time", format!("{frame_ms:.2} ms"));
        rows.stat("Frames", self.frame_count);
        rows.stat("VRR", if self.vrr_active { "on" } else { "off" });
        rows.section("Scene");
        rows.stat("Windows", self.windows.len());
        rows.stat("Monitors", self.monitors.len());
        rows.stat(
            "Blur reuse",
            format!(
                "{} / {}",
                self.temporal_blur_reuse_count, self.temporal_blur_total_count
            ),
        );
        rows.section("System");
        rows.stat("Memory", format!("{:.1} MiB RSS", self.sys_stats.rss_mib()));
        rows.stat("CPU", format!("{:.1} %", self.sys_stats.cpu_pct()));
        rows.stat("Uptime", format!("{uptime} s"));

        if self.debug_hud_extended {
            let p95_ms = self.perf_metrics.frame_time_percentile(0.95).as_secs_f32() * 1000.0;
            let p99_ms = self.perf_metrics.frame_time_percentile(0.99).as_secs_f32() * 1000.0;
            rows.section("Frame tail");
            rows.stat("p95 / p99", format!("{p95_ms:.2} / {p99_ms:.2} ms"));
            let zones = self.frame_profiler.all_zone_stats();
            if !zones.is_empty() {
                rows.section("Profiler (avg / min / max ms, last 120 frames)");
                for (name, stats) in zones {
                    rows.stat(
                        name,
                        format!(
                            "{:.2} / {:.2} / {:.2}  (n={})",
                            stats.avg_ms, stats.min_ms, stats.max_ms, stats.sample_count
                        ),
                    );
                }
            }
        }

        let title = format!("{}  JWM Compositor", hud::TITLE_ICON);
        let chip = if self.debug_hud_extended {
            "wayland · extended"
        } else {
            "wayland"
        };
        unsafe { self.update_hud_textures(gl, &title, chip, &rows) };
        let target = self
            .monitor_refresh_rates
            .values()
            .copied()
            .max()
            .unwrap_or(60) as f32;
        let (meter, tone) = hud::fps_meter(self.fps, target);
        unsafe { self.render_debug_hud_card(gl, projection, meter, tone) };
    }

    /// Rasterize the four HUD text sections — title, state chip, stat labels,
    /// stat values — each in its own tone. Skips the upload entirely when
    /// nothing in the HUD changed since the previous frame.
    #[allow(unsafe_op_in_unsafe_fn)]
    unsafe fn update_hud_textures(
        &mut self,
        gl: &ffi::Gles2,
        title: &str,
        chip: &str,
        rows: &hud::HudRows,
    ) {
        let config = crate::config::CONFIG.load();
        let description = config.system_ui_font();
        let size = crate::backend::compositor_font::ui_font_pixel_size(description);
        let ui = ui_theme::palette();
        let (labels, values) = rows.columns();
        // The theme is part of the key: glass inks are brighter, so a live
        // theme switch has to re-rasterize even when the text is unchanged.
        let cache_key = format!(
            "{description}\0{size}\0{:?}\0{title}\0{chip}\0{labels}\0{values}",
            ui.title_ink
        );
        if cache_key == self.hud_text_cache && self.hud_textures.iter().any(Option::is_some) {
            return;
        }
        let colors: [[u8; 4]; 4] = [ui.title_ink, ui.chip_ink, ui.label_ink, ui.value_ink];
        let texts = [title, chip, labels.as_str(), values.as_str()];
        for (slot, text) in texts.into_iter().enumerate() {
            if let Some((old, _, _)) = self.hud_textures[slot].take() {
                gl.DeleteTextures(1, &old);
            }
            if text.is_empty() {
                continue;
            }
            let (pixels, w, h) = crate::backend::compositor_font::render_ui_text_to_rgba(
                text,
                description,
                size,
                colors[slot],
            );
            if w == 0 || h == 0 {
                continue;
            }
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
            gl.BindTexture(ffi::TEXTURE_2D, 0);
            self.hud_textures[slot] = Some((tex, w, h));
        }
        self.hud_text_cache = cache_key;
    }

    /// Draw the HUD card: shadow, surface, state chip, frame-rate meter, and
    /// the two-tone stat columns, in the active theme's tones.
    #[allow(unsafe_op_in_unsafe_fn)]
    unsafe fn render_debug_hud_card(
        &mut self,
        gl: &ffi::Gles2,
        projection: &[f32; 16],
        meter: f32,
        tone: [f32; 4],
    ) {
        let ui = ui_theme::palette();
        self.ensure_glass_backdrop(gl, ui, projection);
        let dims = |slot: usize| -> (f32, f32) {
            self.hud_textures[slot]
                .map(|(_, w, h)| (w as f32, h as f32))
                .unwrap_or((0.0, 0.0))
        };
        let dock = self.island_dock();
        let layout = hud::HudLayout::docked(ui, &dock, dims(0), dims(1), dims(2), dims(3), meter);
        let motion_enabled = crate::config::CONFIG.load().motion_enabled();
        let (cw, ch) = self.hud_island.advance_with_motion(
            std::time::Instant::now(),
            layout.card.2,
            layout.card.3,
            motion_enabled,
        );
        // A static desktop produces no damage and therefore no frames, so the
        // spring has to keep asking for them until it settles — the HUD does
        // not redraw on its own just because it is on screen.
        if self.hud_island.animating(layout.card.2, layout.card.3) {
            self.needs_render = true;
        }
        let [cx, cy, ..] = dock.rect(cw, ch, 0.0);
        let (radius_top, radius) = dock.radii(ch, ui.card_radius, 0.0);

        gl.BindVertexArray(self.quad_vao);

        // No ambient shadow: the card's top edge is flush with the bar, and a
        // shadow spreading up over it is the seam the dock removes.
        // Card surface, then chip and meter on the border program's
        // rounded-fill mode. The overlay draws onto the display-encoded
        // output, so scene-linear conversion stays off.
        self.ui_fill_island(
            gl, projection, ui, cx, cy, cw, ch, radius, radius_top, ui.card, 1.0,
        );
        if layout.chip_pill.2 > 0.0 {
            let (px, py, pw, ph) = layout.chip_pill;
            self.sysui_fill_rounded(gl, px, py, pw, ph, ui.chip_radius, ui.chip);
        }
        let (tx, ty, tw, th) = layout.meter_track;
        self.sysui_fill_rounded(gl, tx, ty, tw, th, th * 0.5, ui.track);
        let (fx, fy, fw, fh) = layout.meter_fill;
        self.sysui_fill_rounded(gl, fx, fy, fw, fh, fh * 0.5, tone);

        // The ring is a circular rounded rect, so a theme whose surfaces are
        // squircles asks for none of it and relies on the shader's own rim.
        if ui.ring_alpha > 0.0 {
            // Hairline accent ring, matching the focused window's gradient.
            gl.UseProgram(self.gradient_border_program);
            self.set_projection_uniform(gl, self.gradient_border_uniforms.projection, projection);
            gl.Uniform1i(self.gradient_border_uniforms.scene_linear, 0);
            let ring = ui.ring_width;
            let [ar, ag, ab, aa] = self.border_gradient_color_a;
            let [br, bg, bb, ba] = self.border_gradient_color_b;
            gl.Uniform1f(self.gradient_border_uniforms.border_width, ring);
            gl.Uniform4f(
                self.gradient_border_uniforms.color_a,
                ar,
                ag,
                ab,
                aa * ui.ring_alpha,
            );
            gl.Uniform4f(
                self.gradient_border_uniforms.color_b,
                br,
                bg,
                bb,
                ba * ui.ring_alpha,
            );
            gl.Uniform1f(
                self.gradient_border_uniforms.gradient_angle,
                self.border_gradient_angle.to_radians(),
            );
            // The ring follows the card: square across the top where the card
            // meets the bar, curved everywhere the card curves.
            let ring_top = if radius_top > 0.0 {
                radius_top + ring
            } else {
                0.0
            };
            gl.Uniform1f(self.gradient_border_uniforms.radius, radius + ring);
            gl.Uniform1f(self.gradient_border_uniforms.radius_top, ring_top);
            gl.Uniform2f(
                self.gradient_border_uniforms.size,
                cw + 2.0 * ring,
                ch + 2.0 * ring,
            );
            self.set_rect_uniform(
                gl,
                self.gradient_border_uniforms.rect,
                cx - ring,
                cy - ring,
                cw + 2.0 * ring,
                ch + 2.0 * ring,
            );
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
        }

        // Text sections.
        let text_rect = super::get_uniform_loc(gl, self.sysui_text_program, "u_rect");
        let text_proj = super::get_uniform_loc(gl, self.sysui_text_program, "u_projection");
        let text_tex = super::get_uniform_loc(gl, self.sysui_text_program, "u_texture");
        let text_opacity = super::get_uniform_loc(gl, self.sysui_text_program, "u_opacity");
        gl.UseProgram(self.sysui_text_program);
        gl.UniformMatrix4fv(text_proj, 1, ffi::FALSE as u8, projection.as_ptr());
        gl.Uniform1i(text_tex, 0);
        gl.Uniform1f(text_opacity, 1.0);
        gl.ActiveTexture(ffi::TEXTURE0);
        let positions = [layout.title, layout.chip_text, layout.labels, layout.values];
        for (slot, (px, py)) in positions.into_iter().enumerate() {
            let Some((tex, w, h)) = self.hud_textures[slot] else {
                continue;
            };
            gl.Uniform4f(text_rect, px, py, w as f32, h as f32);
            gl.BindTexture(ffi::TEXTURE_2D, tex);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
        }

        gl.BindVertexArray(0);
        gl.UseProgram(0);
    }

    #[allow(unsafe_op_in_unsafe_fn)]
    /// Rasterize the four text sections of the system-UI panel (title, query
    /// line, list items, footer hint), each with its own tone so the styled
    /// card reads with clear hierarchy.
    unsafe fn update_system_ui_textures(
        &mut self,
        gl: &ffi::Gles2,
        overlay: &crate::backend::api::SystemUiOverlay,
    ) {
        if !self.sysui_text_dirty {
            return;
        }
        let config = crate::config::CONFIG.load();
        let description = config.system_ui_font();
        let size = crate::backend::compositor_font::ui_font_pixel_size(description);
        let ui = ui_theme::palette();
        let viewport = overlay.effective_viewport(self.screen_w as i32, self.screen_h as i32);
        let content_width = panel::max_content_width(viewport[2]);
        let query_width = panel::max_query_text_width(viewport[2]);
        let title_text = crate::backend::compositor_font::fit_ui_text_lines(
            &overlay.title,
            description,
            size,
            content_width,
        );
        let query_text = overlay.query.as_ref().map(|q| {
            crate::backend::compositor_font::fit_ui_text_tail(
                &format!("\u{f002}  {q}_"),
                description,
                size,
                query_width,
            )
        });
        let items_text = crate::backend::compositor_font::fit_ui_text_lines(
            &overlay.items.join("\n"),
            description,
            size,
            content_width,
        );
        let hint_text = crate::backend::compositor_font::fit_ui_text_lines(
            &overlay.hint,
            description,
            size,
            content_width,
        );
        // Title, query, list body, footer hint — brightest first.
        let colors: [[u8; 4]; 4] = [ui.panel_title_ink, ui.query_ink, ui.item_ink, ui.hint_ink];
        let texts: [Option<&str>; 4] = [
            (!title_text.is_empty()).then_some(title_text.as_str()),
            query_text.as_deref(),
            (!items_text.is_empty()).then_some(items_text.as_str()),
            (!hint_text.is_empty()).then_some(hint_text.as_str()),
        ];
        for (slot, text) in texts.into_iter().enumerate() {
            if let Some((old, _, _)) = self.sysui_textures[slot].take() {
                unsafe { gl.DeleteTextures(1, &old) };
            }
            let Some(text) = text else { continue };
            let (pixels, w, h) = crate::backend::compositor_font::render_ui_text_to_rgba(
                text,
                description,
                size,
                colors[slot],
            );
            if w == 0 || h == 0 {
                continue;
            }
            unsafe {
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
                self.sysui_textures[slot] = Some((tex, w, h));
            }
        }
        self.sysui_text_dirty = false;
    }

    /// Filled rounded rectangle through the border program (a border wider
    /// than the rect fills it). The program and projection must be bound.
    /// Capture a blurred copy of the frame for the frosted-glass panels to
    /// sample.
    ///
    /// Reads `output_fbo`, i.e. the composited scene as it will be scanned out,
    /// so the glass shows the desktop the user sees. No blur chain (a driver
    /// that refused the FBOs) leaves the backdrop unset and the panels fall
    /// back to flat translucent fills.
    fn capture_glass_backdrop(&mut self, gl: &ffi::Gles2, palette: &UiPalette, proj: &[f32; 16]) {
        self.glass_backdrop = None;
        if palette.glass.is_none() || self.blur_fbos.is_empty() || self.scene_fbo == 0 {
            return;
        }
        self.blit_fbo(
            gl,
            self.output_fbo,
            self.scene_fbo,
            self.screen_w,
            self.screen_h,
        );
        self.run_blur_passes(gl, self.scene_texture, proj, BlurQuality::Full);
        self.glass_backdrop = Some(self.blur_fbos[0].texture);
        // run_blur_passes leaves its last level bound; overlays keep drawing
        // into the output.
        unsafe {
            gl.BindFramebuffer(ffi::FRAMEBUFFER, self.output_fbo);
            gl.Viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
        }
    }

    /// Capture the backdrop unless this frame already has one. Panels drawn
    /// back to back share a single capture: re-blurring the whole screen per
    /// card would cost more than the parallax it buys, and the only thing the
    /// later cards miss is the earlier cards themselves.
    /// Where the compositor's own panels dock: under the status bar.
    ///
    /// The bar's class is matched against the tracked windows and its rect read
    /// from the scene laid out this frame. A bar that is hidden or on another
    /// output leaves the panels hanging from the top of the screen instead.
    fn island_dock(&self) -> IslandDock {
        self.island_dock_in([0.0, 0.0, self.screen_w as f32, self.screen_h as f32])
    }

    fn island_dock_in(&self, viewport: [f32; 4]) -> IslandDock {
        let cfg = crate::config::CONFIG.load();
        let bar_name = cfg.status_bar_name();
        let bar = self
            .prev_scene
            .iter()
            .filter(|&&(id, _, _, w, h)| {
                w > 0
                    && h > 0
                    && !bar_name.is_empty()
                    && self.windows.get(&id).is_some_and(|win| {
                        win.class_name == bar_name || win.class_name.contains(bar_name)
                    })
            })
            .filter_map(|&(_, x, y, w, h)| {
                clip_bar_to_viewport([x as f32, y as f32, w as f32, h as f32], viewport)
            })
            // Only bars intersecting this output remain. Prefer its topmost
            // segment if a client exposed more than one dock-like surface.
            .min_by(|left, right| left[1].total_cmp(&right[1]));
        IslandDock::for_bar(bar, viewport)
    }

    pub(super) fn ensure_glass_backdrop(
        &mut self,
        gl: &ffi::Gles2,
        palette: &UiPalette,
        proj: &[f32; 16],
    ) {
        if self.glass_backdrop.is_none() {
            self.capture_glass_backdrop(gl, palette, proj);
        }
    }

    /// Draw one frosted-glass surface. Binds its own program, so callers that
    /// follow up with flat fills must re-bind the border program afterwards.
    #[allow(clippy::too_many_arguments)]
    unsafe fn glass_fill_rounded(
        &self,
        gl: &ffi::Gles2,
        proj: &[f32; 16],
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        r: f32,
        r_top: f32,
        tint: [f32; 4],
        alpha: f32,
        params: &crate::backend::compositor_common::ui_theme::GlassParams,
    ) {
        let Some(backdrop) = self.glass_backdrop else {
            return;
        };
        unsafe {
            let u = &self.glass_uniforms;
            gl.UseProgram(self.glass_program);
            self.set_projection_uniform(gl, u.projection, proj);
            gl.ActiveTexture(ffi::TEXTURE0);
            gl.BindTexture(ffi::TEXTURE_2D, backdrop);
            gl.Uniform1i(u.backdrop, 0);
            gl.Uniform2f(u.screen_size, self.screen_w as f32, self.screen_h as f32);
            gl.Uniform4f(u.tint, tint[0], tint[1], tint[2], tint[3]);
            gl.Uniform2f(u.size, w, h);
            gl.Uniform1f(u.radius, r);
            gl.Uniform1f(u.radius_top, r_top);
            gl.Uniform1f(u.corner_exp, params.corner_exponent);
            gl.Uniform1f(u.saturation, params.saturation);
            gl.Uniform1f(u.luminance, params.luminance);
            gl.Uniform1f(u.bevel_width, params.bevel_width);
            gl.Uniform1f(u.refraction, params.refraction);
            gl.Uniform1f(u.rim_width, params.rim_width);
            gl.Uniform1f(u.rim_intensity, params.rim_intensity);
            gl.Uniform3f(
                u.rim_tint,
                params.rim_tint[0],
                params.rim_tint[1],
                params.rim_tint[2],
            );
            gl.Uniform1f(u.sheen, params.sheen);
            gl.Uniform1f(u.edge_shade, params.edge_shade);
            gl.Uniform1f(u.grain, params.grain);
            gl.Uniform1f(u.alpha, alpha.clamp(0.0, 1.0));
            // Overlays draw onto the display-encoded output, so scene-linear
            // conversion stays off — matching the border programs.
            gl.Uniform1i(u.scene_linear, 0);
            self.set_rect_uniform(gl, u.rect, x, y, w, h);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
        }
    }

    /// Card base fill for a self-drawn panel: frosted glass when the theme asks
    /// for it and a backdrop exists, otherwise the flat rounded fill.
    ///
    /// Leaves the border program bound with `proj` set, so the caller can keep
    /// filling chips, tracks and pills without re-binding.
    #[allow(clippy::too_many_arguments)]
    unsafe fn ui_fill_surface(
        &self,
        gl: &ffi::Gles2,
        proj: &[f32; 16],
        palette: &UiPalette,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        r: f32,
        surface: [f32; 4],
        alpha: f32,
    ) {
        unsafe { self.ui_fill_island(gl, proj, palette, x, y, w, h, r, r, surface, alpha) }
    }

    /// As [`Self::ui_fill_surface`], but the top two corners take their own
    /// radius. A docked panel passes zero so it merges with the bar above it.
    #[allow(clippy::too_many_arguments)]
    pub(super) unsafe fn ui_fill_island(
        &self,
        gl: &ffi::Gles2,
        proj: &[f32; 16],
        palette: &UiPalette,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        r: f32,
        r_top: f32,
        surface: [f32; 4],
        alpha: f32,
    ) {
        unsafe {
            let drew_glass = match palette.glass {
                Some(params) if self.glass_backdrop.is_some() => {
                    self.glass_fill_rounded(
                        gl, proj, x, y, w, h, r, r_top, surface, alpha, &params,
                    );
                    true
                }
                _ => false,
            };
            gl.UseProgram(self.border_program);
            self.set_projection_uniform(gl, self.border_uniforms.projection, proj);
            gl.Uniform1i(self.border_uniforms.scene_linear, 0);
            if !drew_glass {
                self.sysui_fill_island(gl, x, y, w, h, r, r_top, UiPalette::faded(surface, alpha));
            }
        }
    }

    pub(super) unsafe fn sysui_fill_rounded(
        &self,
        gl: &ffi::Gles2,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        r: f32,
        color: [f32; 4],
    ) {
        unsafe { self.sysui_fill_island(gl, x, y, w, h, r, r, color) }
    }

    /// As [`Self::sysui_fill_rounded`], but the top two corners take their own
    /// radius so a docked panel meets the bar with a straight edge.
    #[allow(clippy::too_many_arguments)]
    unsafe fn sysui_fill_island(
        &self,
        gl: &ffi::Gles2,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        r_bottom: f32,
        r_top: f32,
        color: [f32; 4],
    ) {
        unsafe {
            gl.Uniform1f(self.border_uniforms.border_width, w.max(h));
            gl.Uniform4f(
                self.border_uniforms.border_color,
                color[0],
                color[1],
                color[2],
                color[3],
            );
            gl.Uniform1f(self.border_uniforms.radius, r_bottom);
            gl.Uniform1f(self.border_uniforms.radius_top, r_top);
            gl.Uniform2f(self.border_uniforms.size, w, h);
            self.set_rect_uniform(gl, self.border_uniforms.rect, x, y, w, h);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
        }
    }

    /// Rounded outline through the border program: the line-drawn boxes the
    /// layout thumbnails are made of. The program and projection must be
    /// bound.
    #[allow(clippy::too_many_arguments)]
    unsafe fn sysui_stroke_rounded(
        &self,
        gl: &ffi::Gles2,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        r: f32,
        width: f32,
        color: [f32; 4],
    ) {
        unsafe {
            gl.Uniform1f(self.border_uniforms.border_width, width);
            gl.Uniform4f(
                self.border_uniforms.border_color,
                color[0],
                color[1],
                color[2],
                color[3],
            );
            gl.Uniform1f(self.border_uniforms.radius, r);
            gl.Uniform1f(self.border_uniforms.radius_top, r);
            gl.Uniform2f(self.border_uniforms.size, w, h);
            self.set_rect_uniform(gl, self.border_uniforms.rect, x, y, w, h);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
        }
    }

    /// The layout picker: a strip of 35mm film across the panel, one cell per
    /// layout, each holding a line-drawn thumbnail of what that layout does
    /// with a screenful of windows. The selected cell lifts out of the strip
    /// and the countdown under it shows how long until it commits itself.
    unsafe fn render_layout_filmstrip(
        &mut self,
        gl: &ffi::Gles2,
        projection: &[f32; 16],
        strip: &crate::backend::api::LayoutFilmstrip,
        viewport: [f32; 4],
    ) {
        use crate::backend::compositor_common::layout_strip as film;

        let ui = ui_theme::palette();
        let [viewport_x, viewport_y, viewport_w, viewport_h] = viewport;
        let geometry = film::strip_geometry(viewport, strip.cells.len());
        let [panel_x, panel_y, panel_w, panel_h] = geometry.panel;
        let accent = self.border_gradient_color_a;

        unsafe {
            gl.BindVertexArray(self.quad_vao);

            // Scrim: dim the desktop the strip is describing.
            let rect = super::get_uniform_loc(gl, self.hud_program, "u_rect");
            let proj = super::get_uniform_loc(gl, self.hud_program, "u_projection");
            let bg = super::get_uniform_loc(gl, self.hud_program, "u_bg_color");
            let size = super::get_uniform_loc(gl, self.hud_program, "u_size");
            gl.UseProgram(self.hud_program);
            gl.UniformMatrix4fv(proj, 1, ffi::FALSE as u8, projection.as_ptr());
            gl.Uniform4f(bg, ui.scrim[0], ui.scrim[1], ui.scrim[2], ui.scrim[3]);
            gl.Uniform2f(size, viewport_w, viewport_h);
            gl.Uniform4f(rect, viewport_x, viewport_y, viewport_w, viewport_h);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
        }

        self.capture_glass_backdrop(gl, ui, projection);

        unsafe {
            // Drop shadow, then the card.
            gl.BindVertexArray(self.quad_vao);
            gl.UseProgram(self.shadow_program);
            self.set_projection_uniform(gl, self.shadow_uniforms.projection, projection);
            let spread = ui.spread(48.0);
            gl.Uniform1f(self.shadow_uniforms.spread, spread);
            gl.Uniform4f(
                self.shadow_uniforms.shadow_color,
                ui.shadow[0],
                ui.shadow[1],
                ui.shadow[2],
                ui.shadow[3],
            );
            gl.Uniform1f(self.shadow_uniforms.radius, film::PANEL_RADIUS);
            gl.Uniform2f(self.shadow_uniforms.size, panel_w, panel_h);
            self.set_rect_uniform(
                gl,
                self.shadow_uniforms.rect,
                panel_x - spread,
                panel_y - spread + 14.0,
                panel_w + 2.0 * spread,
                panel_h + 2.0 * spread,
            );
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);

            self.ui_fill_surface(
                gl,
                projection,
                ui,
                panel_x,
                panel_y,
                panel_w,
                panel_h,
                film::PANEL_RADIUS,
                ui.panel,
                1.0,
            );

            // The film base: the palette's recessed tone, which reads darker
            // than the card under a light theme and lighter under a dark one —
            // either way as a band lying in the panel rather than on it.
            let [sx, sy, sw, sh] = geometry.strip;
            let base = ui.track;
            self.sysui_fill_rounded(gl, sx - 4.0, sy, sw + 8.0, sh, 4.0, base);

            let line = UiPalette::ink(ui.item_ink, 0.72);
            let dim_line = UiPalette::ink(ui.hint_ink, 0.55);
            let bar_line = UiPalette::ink(ui.hint_ink, 0.4);
            // The perforation is punched back to the card's own tone, so the
            // holes read as gaps in the film rather than marks on it.
            let hole = UiPalette::faded(ui.panel, 1.0);

            for (index, cell) in geometry.cells.iter().enumerate() {
                let selected = index == strip.selected;
                let scale = if selected { film::SELECTED_SCALE } else { 1.0 };
                let pivot = film::center(cell.cell);
                let cell_rect = film::scaled_about(cell.cell, pivot, scale);
                let frame = film::scaled_about(cell.frame, pivot, scale);

                self.sysui_fill_rounded(
                    gl,
                    cell_rect[0],
                    cell_rect[1],
                    cell_rect[2],
                    cell_rect[3],
                    film::CELL_RADIUS,
                    if selected {
                        UiPalette::faded(ui.chip, 1.0)
                    } else {
                        UiPalette::faded(ui.chip, 0.7)
                    },
                );
                let ink = if selected { line } else { dim_line };
                self.sysui_stroke_rounded(
                    gl,
                    frame[0],
                    frame[1],
                    frame[2],
                    frame[3],
                    film::WINDOW_RADIUS,
                    film::LINE_WIDTH * scale,
                    UiPalette::faded(ink, 0.6),
                );

                let Some(content) = strip.cells.get(index) else {
                    continue;
                };
                // A rule across the top stands in for the status bar, which is
                // what tells Monocle and Fullscreen apart.
                if content.shows_bar {
                    let bar_h = (frame[3] * 0.08).max(1.0);
                    self.sysui_fill_rounded(
                        gl,
                        frame[0],
                        frame[1] + bar_h,
                        frame[2],
                        film::LINE_WIDTH,
                        0.0,
                        if selected { ink } else { bar_line },
                    );
                }
                for window in &content.windows {
                    let rect = film::window_rect(frame, *window);
                    self.sysui_stroke_rounded(
                        gl,
                        rect[0],
                        rect[1],
                        rect[2],
                        rect[3],
                        film::WINDOW_RADIUS,
                        film::LINE_WIDTH * scale,
                        ink,
                    );
                }

                if selected {
                    // The gate: an accent ring around the frame being shown.
                    self.sysui_stroke_rounded(
                        gl,
                        cell_rect[0] - 2.0,
                        cell_rect[1] - 2.0,
                        cell_rect[2] + 4.0,
                        cell_rect[3] + 4.0,
                        film::CELL_RADIUS + 2.0,
                        1.8,
                        [accent[0], accent[1], accent[2], 0.95],
                    );
                }
            }

            // Perforation last, so it punches through the cells it crosses.
            for [hx, hy, hw, hh] in &geometry.sprockets {
                self.sysui_fill_rounded(gl, *hx, *hy, *hw, *hh, hh * 0.4, hole);
            }

            // Countdown to the automatic commit.
            let [cx, cy, cw, ch] = geometry.countdown;
            self.sysui_fill_rounded(
                gl,
                cx,
                cy,
                cw,
                ch,
                ch * 0.5,
                UiPalette::faded(ui.track, 0.8),
            );
            let filled = cw * strip.countdown.clamp(0.0, 1.0);
            if filled > 1.0 {
                self.sysui_fill_rounded(
                    gl,
                    cx,
                    cy,
                    filled,
                    ch,
                    ch * 0.5,
                    [accent[0], accent[1], accent[2], 0.9],
                );
            }

            // Title, the selected layout's name centred under the strip, and
            // the footer hint.
            let text_rect = super::get_uniform_loc(gl, self.sysui_text_program, "u_rect");
            let text_proj = super::get_uniform_loc(gl, self.sysui_text_program, "u_projection");
            let text_tex = super::get_uniform_loc(gl, self.sysui_text_program, "u_texture");
            let text_opacity = super::get_uniform_loc(gl, self.sysui_text_program, "u_opacity");
            gl.UseProgram(self.sysui_text_program);
            gl.UniformMatrix4fv(text_proj, 1, ffi::FALSE as u8, projection.as_ptr());
            gl.Uniform1i(text_tex, 0);
            gl.Uniform1f(text_opacity, 1.0);
            gl.ActiveTexture(ffi::TEXTURE0);
            let caption = geometry.caption_center;
            for (slot, pos) in [
                (0usize, Some(geometry.title)),
                (2, None),
                (3, Some(geometry.hint)),
            ] {
                let Some((tex, w, h)) = self.sysui_textures[slot] else {
                    continue;
                };
                let (tx, ty) = match pos {
                    Some([x, y]) => (x, y),
                    // The caption is centred on the strip rather than aligned
                    // to the panel's text column.
                    None => (caption[0] - w as f32 * 0.5, caption[1] - h as f32 * 0.5),
                };
                gl.Uniform4f(text_rect, tx, ty, w as f32, h as f32);
                gl.BindTexture(ffi::TEXTURE_2D, tex);
                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
            }
            gl.BindVertexArray(0);
            gl.UseProgram(0);
        }
    }

    /// The tags overview: one cell per tag of the selected monitor, each with
    /// the tag's number and a line-drawn wireframe of its windows, back to
    /// front. The selected cell lifts out of the grid; tags currently on
    /// screen carry a persistent accent frame inside the cell, kept distinct
    /// from the selection gate around it.
    unsafe fn render_tags_grid(
        &mut self,
        gl: &ffi::Gles2,
        projection: &[f32; 16],
        grid: &crate::backend::api::TagsGrid,
        viewport: [f32; 4],
    ) {
        use crate::backend::compositor_common::layout_strip as film;
        use crate::backend::compositor_common::tags_grid as grid_layout;

        let ui = ui_theme::palette();
        let [viewport_x, viewport_y, viewport_w, viewport_h] = viewport;
        let geometry = grid_layout::grid_geometry(viewport, grid.cells.len(), grid.cols);
        let [panel_x, panel_y, panel_w, panel_h] = geometry.panel;
        let accent = self.border_gradient_color_a;

        // The panel repaints only when the overview changes, so the cell
        // labels are rasterized per redraw instead of living in a cache.
        let config = crate::config::CONFIG.load();
        let description = config.system_ui_font();
        let text_size = crate::backend::compositor_font::ui_font_pixel_size(description);
        let mut labels: Vec<(usize, u32, u32, u32)> = Vec::new();
        for (index, content) in grid.cells.iter().enumerate() {
            if index >= geometry.cells.len() {
                break;
            }
            let color = if content.occupied {
                ui.item_ink
            } else {
                ui.hint_ink
            };
            let text = format!("{}", content.tag_index + 1);
            if let Some((tex, w, h)) =
                unsafe { rasterize_toast_text(gl, &text, description, text_size, color) }
            {
                labels.push((index, tex, w, h));
            }
        }

        unsafe {
            gl.BindVertexArray(self.quad_vao);

            // Scrim: dim the desktop the grid is describing.
            let rect = super::get_uniform_loc(gl, self.hud_program, "u_rect");
            let proj = super::get_uniform_loc(gl, self.hud_program, "u_projection");
            let bg = super::get_uniform_loc(gl, self.hud_program, "u_bg_color");
            let size = super::get_uniform_loc(gl, self.hud_program, "u_size");
            gl.UseProgram(self.hud_program);
            gl.UniformMatrix4fv(proj, 1, ffi::FALSE as u8, projection.as_ptr());
            gl.Uniform4f(bg, ui.scrim[0], ui.scrim[1], ui.scrim[2], ui.scrim[3]);
            gl.Uniform2f(size, viewport_w, viewport_h);
            gl.Uniform4f(rect, viewport_x, viewport_y, viewport_w, viewport_h);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
        }

        self.capture_glass_backdrop(gl, ui, projection);

        unsafe {
            // Drop shadow, then the card.
            gl.BindVertexArray(self.quad_vao);
            gl.UseProgram(self.shadow_program);
            self.set_projection_uniform(gl, self.shadow_uniforms.projection, projection);
            let spread = ui.spread(48.0);
            gl.Uniform1f(self.shadow_uniforms.spread, spread);
            gl.Uniform4f(
                self.shadow_uniforms.shadow_color,
                ui.shadow[0],
                ui.shadow[1],
                ui.shadow[2],
                ui.shadow[3],
            );
            gl.Uniform1f(self.shadow_uniforms.radius, film::PANEL_RADIUS);
            gl.Uniform2f(self.shadow_uniforms.size, panel_w, panel_h);
            self.set_rect_uniform(
                gl,
                self.shadow_uniforms.rect,
                panel_x - spread,
                panel_y - spread + 14.0,
                panel_w + 2.0 * spread,
                panel_h + 2.0 * spread,
            );
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);

            self.ui_fill_surface(
                gl,
                projection,
                ui,
                panel_x,
                panel_y,
                panel_w,
                panel_h,
                film::PANEL_RADIUS,
                ui.panel,
                1.0,
            );

            let line = UiPalette::ink(ui.item_ink, 0.72);
            let dim_line = UiPalette::ink(ui.hint_ink, 0.55);

            for (index, cell) in geometry.cells.iter().enumerate() {
                let selected = index == grid.selected;
                let scale = if selected { film::SELECTED_SCALE } else { 1.0 };
                let pivot = film::center(cell.cell);
                let cell_rect = film::scaled_about(cell.cell, pivot, scale);
                let frame = film::scaled_about(cell.frame, pivot, scale);

                self.sysui_fill_rounded(
                    gl,
                    cell_rect[0],
                    cell_rect[1],
                    cell_rect[2],
                    cell_rect[3],
                    film::CELL_RADIUS,
                    if selected {
                        UiPalette::faded(ui.chip, 1.0)
                    } else {
                        UiPalette::faded(ui.chip, 0.7)
                    },
                );

                let Some(content) = grid.cells.get(index) else {
                    continue;
                };
                // Occupied tags (minimized windows included) draw in the
                // bright ink; empty ones sit back in the dim tone.
                let ink = if content.occupied { line } else { dim_line };
                let live = grid.live_for_cell(index);
                if let Some(live) = live {
                    // The on-screen tag's cell swaps its wireframes for the
                    // windows' own textures, scaled into the same rectangles.
                    self.render_tags_grid_live_cell(gl, projection, frame, live, ink, scale);
                }
                // The frame's border sits above the cell's content, so a live
                // thumbnail ends exactly at the frame edge.
                self.sysui_stroke_rounded(
                    gl,
                    frame[0],
                    frame[1],
                    frame[2],
                    frame[3],
                    film::WINDOW_RADIUS,
                    film::LINE_WIDTH * scale,
                    UiPalette::faded(ink, 0.6),
                );
                if live.is_none() {
                    for window in &content.windows {
                        let rect = film::window_rect(frame, *window);
                        self.sysui_stroke_rounded(
                            gl,
                            rect[0],
                            rect[1],
                            rect[2],
                            rect[3],
                            film::WINDOW_RADIUS,
                            film::LINE_WIDTH * scale,
                            ink,
                        );
                    }
                }

                if content.active {
                    // The persistent "on screen now" marker, inside the cell
                    // so the selection gate outside it stays readable when a
                    // tag is both current and highlighted.
                    self.sysui_stroke_rounded(
                        gl,
                        cell_rect[0] + 2.0,
                        cell_rect[1] + 2.0,
                        cell_rect[2] - 4.0,
                        cell_rect[3] - 4.0,
                        film::CELL_RADIUS,
                        1.6,
                        [accent[0], accent[1], accent[2], 0.75],
                    );
                }
                if selected {
                    // The gate: an accent ring around the cell being shown.
                    self.sysui_stroke_rounded(
                        gl,
                        cell_rect[0] - 2.0,
                        cell_rect[1] - 2.0,
                        cell_rect[2] + 4.0,
                        cell_rect[3] + 4.0,
                        film::CELL_RADIUS + 2.0,
                        1.8,
                        [accent[0], accent[1], accent[2], 0.95],
                    );
                }
            }

            // Tag numbers ride their cell's presentation scale, so the
            // selected cell's lift does not leave its label behind.
            let text_rect = super::get_uniform_loc(gl, self.sysui_text_program, "u_rect");
            let text_proj = super::get_uniform_loc(gl, self.sysui_text_program, "u_projection");
            let text_tex = super::get_uniform_loc(gl, self.sysui_text_program, "u_texture");
            let text_opacity = super::get_uniform_loc(gl, self.sysui_text_program, "u_opacity");
            gl.UseProgram(self.sysui_text_program);
            gl.UniformMatrix4fv(text_proj, 1, ffi::FALSE as u8, projection.as_ptr());
            gl.Uniform1i(text_tex, 0);
            gl.Uniform1f(text_opacity, 1.0);
            gl.ActiveTexture(ffi::TEXTURE0);
            for (index, tex, w, h) in &labels {
                let cell = &geometry.cells[*index];
                let scale = if *index == grid.selected {
                    film::SELECTED_SCALE
                } else {
                    1.0
                };
                let pivot = film::center(cell.cell);
                let cell_rect = film::scaled_about(cell.cell, pivot, scale);
                let [tx, ty] = grid_layout::label_origin(cell, cell_rect, scale);
                gl.Uniform4f(text_rect, tx, ty, *w as f32, *h as f32);
                gl.BindTexture(ffi::TEXTURE_2D, *tex);
                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
            }

            // Title, the highlighted tag's name centred under the grid, and
            // the footer hint.
            let caption = geometry.caption_center;
            for (slot, pos) in [
                (0usize, Some(geometry.title)),
                (2, None),
                (3, Some(geometry.hint)),
            ] {
                let Some((tex, w, h)) = self.sysui_textures[slot] else {
                    continue;
                };
                let (tx, ty) = match pos {
                    Some([x, y]) => (x, y),
                    // The caption is centred on the grid rather than aligned
                    // to the panel's text column.
                    None => (caption[0] - w as f32 * 0.5, caption[1] - h as f32 * 0.5),
                };
                gl.Uniform4f(text_rect, tx, ty, w as f32, h as f32);
                gl.BindTexture(ffi::TEXTURE_2D, tex);
                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
            }
            gl.BindVertexArray(0);
            gl.UseProgram(0);

            for (_, tex, _, _) in labels {
                gl.DeleteTextures(1, &tex);
            }
        }
    }

    /// The on-screen tag's live cell content: every window's texture scaled
    /// into the rectangle its wireframe would occupy — the identical
    /// [`crate::backend::compositor_common::layout_strip::window_rect`]
    /// mapping, so live and line-drawn cells never disagree about placement.
    /// Drawn through the ordinary window program, the expose thumbnails'
    /// shader path — content UV, ripple-off and color transform included;
    /// a window without a ready texture keeps its outline as a fail-safe.
    unsafe fn render_tags_grid_live_cell(
        &self,
        gl: &ffi::Gles2,
        projection: &[f32; 16],
        frame: [f32; 4],
        live: &crate::backend::api::LiveTagsCell,
        ink: [f32; 4],
        scale: f32,
    ) {
        use crate::backend::compositor_common::layout_strip as film;

        unsafe {
            gl.UseProgram(self.program);
            gl.UniformMatrix4fv(
                self.win_uniforms.projection,
                1,
                ffi::FALSE as u8,
                projection.as_ptr(),
            );

            let mut outlines = Vec::new();
            for (window, norm) in &live.windows {
                let rect = film::window_rect(frame, *norm);
                let Some(win) = self.windows.get(&window.raw()) else {
                    outlines.push(rect);
                    continue;
                };
                let Some(tex) = win.gl_texture else {
                    outlines.push(rect);
                    continue;
                };

                gl.Uniform4f(self.win_uniforms.rect, rect[0], rect[1], rect[2], rect[3]);
                gl.Uniform1f(self.win_uniforms.opacity, 1.0);
                gl.Uniform1f(self.win_uniforms.radius, 6.0);
                gl.Uniform2f(self.win_uniforms.size, rect[2], rect[3]);
                gl.Uniform1f(self.win_uniforms.dim, 1.0);

                // Use content_uv to crop out CSD shadows/decorations.
                let [cu, cv, cw, ch] = win.content_uv;
                let (uv_x, uv_y, uv_w, uv_h) = if win.y_inverted {
                    (cu, cv + ch, cw, -ch)
                } else {
                    (cu, cv, cw, ch)
                };
                gl.Uniform4f(self.win_uniforms.uv_rect, uv_x, uv_y, uv_w, uv_h);
                gl.Uniform1f(self.win_uniforms.ripple_progress, -1.0);
                gl.Uniform1f(self.win_uniforms.ripple_amplitude, 0.0);

                let color_transform = win.color_transform.map(|transform| {
                    if self.scene_linear_color_path_active() {
                        transform_for_encoded_srgb(transform)
                    } else {
                        transform
                    }
                });
                self.upload_window_color_transform(gl, color_transform, false);

                gl.ActiveTexture(ffi::TEXTURE0);
                self.bind_window_texture(gl, tex);
                gl.Uniform1i(self.win_uniforms.texture, 0);

                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                self.reset_window_color_transform(gl);
            }

            // Back to the border program the cell loop's strokes draw with,
            // then the fail-safe outlines.
            gl.UseProgram(self.border_program);
            self.set_projection_uniform(gl, self.border_uniforms.projection, projection);
            for rect in outlines {
                self.sysui_stroke_rounded(
                    gl,
                    rect[0],
                    rect[1],
                    rect[2],
                    rect[3],
                    film::WINDOW_RADIUS,
                    film::LINE_WIDTH * scale,
                    ink,
                );
            }
        }
    }

    /// Modal system UI drawn as a material-style card: dimmed scrim, drop
    /// shadow, rounded panel with a gradient accent ring, a search-field bar,
    /// and a selection pill under the highlighted list row.
    unsafe fn render_system_ui(&mut self, gl: &ffi::Gles2, projection: &[f32; 16]) {
        let Some(overlay) = self.system_ui.clone() else {
            return;
        };
        self.system_ui_hit_geometry = None;
        unsafe { self.update_system_ui_textures(gl, &overlay) };
        let viewport = overlay.effective_viewport(self.screen_w as i32, self.screen_h as i32);
        if let Some(strip) = &overlay.filmstrip {
            unsafe { self.render_layout_filmstrip(gl, projection, strip, viewport) };
            return;
        }
        if let Some(grid) = &overlay.tags_grid {
            unsafe { self.render_tags_grid(gl, projection, grid, viewport) };
            return;
        }
        let dims = |slot: usize| -> (f32, f32) {
            self.sysui_textures[slot]
                .map(|(_, w, h)| (w as f32, h as f32))
                .unwrap_or((0.0, 0.0))
        };
        let (title_w, title_h) = dims(0);
        let (query_w, query_h) = dims(1);
        let (items_w, items_h) = dims(2);
        let (hint_w, hint_h) = dims(3);

        let ui = ui_theme::palette();
        let radius = ui.panel_radius;
        let [viewport_x, viewport_y, viewport_w, viewport_h] = viewport;

        let sizes = panel::SectionSizes {
            title: (title_w, title_h),
            query: (query_w, query_h),
            items: (items_w, items_h),
            hint: (hint_w, hint_h),
        };
        // The lock card centres on its own backdrop and has nothing to jitter
        // against, so it hugs its content instead of carrying a floor.
        let width_floor = if overlay.locked {
            0.0
        } else {
            self.system_ui_width_floor
        };
        let (panel_w, panel_h) = panel::target_size(&sizes, viewport_w, width_floor);
        if !overlay.locked {
            self.system_ui_width_floor = panel_w;
        }

        // The lock card owns the whole screen and centres on its own opaque
        // backdrop; every other panel drops out of the bar like the OSD.
        let dock = self.island_dock_in(viewport);
        let motion_enabled = crate::config::CONFIG.load().motion_enabled();
        let (panel_w, panel_h, radius_top, radius, content_a) = if overlay.locked {
            (panel_w, panel_h, radius, radius, 1.0)
        } else {
            let (w, h) = self.system_ui_island.advance_with_motion(
                std::time::Instant::now(),
                panel_w,
                panel_h,
                motion_enabled,
            );
            // Unlike the OSD, a modal panel only redraws when something asks
            // it to, so the spring has to keep asking until it settles.
            if self.system_ui_island.animating(panel_w, panel_h) {
                self.needs_render = true;
            }
            let (r_top, r) = dock.radii(h, radius, 0.0);
            // Contents appear as the card makes room for them rather than
            // overflowing one that is still only a seed wide.
            let opened = (w / panel_w.max(1.0)).clamp(0.0, 1.0);
            (w, h, r_top, r, opened * opened)
        };
        let (x, y) = if overlay.locked {
            (
                viewport_x + ((viewport_w - panel_w) * 0.5).max(16.0),
                viewport_y + ((viewport_h - panel_h) * 0.5).max(16.0),
            )
        } else {
            let [x, y, ..] = dock.contained_rect(panel_w, panel_h, 0.0);
            (x, y)
        };

        let accent = self.border_gradient_color_a;
        // The lock card hides the desktop by design, and the clear below makes
        // the captured backdrop describe nothing that is still on screen — so
        // it draws solid even under the glass theme.
        let mut panel_fill = ui.panel;
        if overlay.locked {
            self.glass_backdrop = None;
            panel_fill[3] = 1.0;
        }
        unsafe {
            gl.BindVertexArray(self.quad_vao);

            if overlay.locked {
                gl.ClearColor(
                    ui.lock_backdrop[0],
                    ui.lock_backdrop[1],
                    ui.lock_backdrop[2],
                    ui.lock_backdrop[3],
                );
                gl.Clear(ffi::COLOR_BUFFER_BIT);
            } else {
                // Scrim: dim the desktop behind the panel.
                let rect = super::get_uniform_loc(gl, self.hud_program, "u_rect");
                let proj = super::get_uniform_loc(gl, self.hud_program, "u_projection");
                let bg = super::get_uniform_loc(gl, self.hud_program, "u_bg_color");
                let size = super::get_uniform_loc(gl, self.hud_program, "u_size");
                gl.UseProgram(self.hud_program);
                gl.UniformMatrix4fv(proj, 1, ffi::FALSE as u8, projection.as_ptr());
                gl.Uniform4f(bg, ui.scrim[0], ui.scrim[1], ui.scrim[2], ui.scrim[3]);
                gl.Uniform2f(size, viewport_w, viewport_h);
                gl.Uniform4f(rect, viewport_x, viewport_y, viewport_w, viewport_h);
                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
            }
        }

        // The scrim is part of what the panel covers, so the backdrop is taken
        // after it — otherwise the glass would show an undimmed desktop inside
        // a dimmed one. Forced rather than lazy for the same reason.
        if !overlay.locked {
            self.capture_glass_backdrop(gl, ui, projection);
        }

        unsafe {
            gl.BindVertexArray(self.quad_vao);
            // Drop shadow behind the card — the lock card only. A docked
            // panel's top edge is flush with the bar, and a shadow spreading up
            // over it is exactly the seam the dock removes.
            if overlay.locked {
                gl.UseProgram(self.shadow_program);
                self.set_projection_uniform(gl, self.shadow_uniforms.projection, projection);
                let spread = ui.spread(48.0);
                gl.Uniform1f(self.shadow_uniforms.spread, spread);
                gl.Uniform4f(
                    self.shadow_uniforms.shadow_color,
                    ui.shadow[0],
                    ui.shadow[1],
                    ui.shadow[2],
                    ui.shadow[3],
                );
                gl.Uniform1f(self.shadow_uniforms.radius, radius);
                gl.Uniform2f(self.shadow_uniforms.size, panel_w, panel_h);
                self.set_rect_uniform(
                    gl,
                    self.shadow_uniforms.rect,
                    x - spread,
                    y - spread + 14.0,
                    panel_w + 2.0 * spread,
                    panel_h + 2.0 * spread,
                );
                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
            }

            // Card surface, then the query-field bar and selection pill on the
            // border program's rounded-fill mode. The overlay draws onto the
            // display-encoded output, so scene-linear conversion stays off.
            self.ui_fill_island(
                gl, projection, ui, x, y, panel_w, panel_h, radius, radius_top, panel_fill, 1.0,
            );

            let layout = panel::contents(
                [x, y, panel_w, panel_h],
                &sizes,
                overlay.items.len(),
                overlay.selected,
                overlay.scroll.map(|s| panel::Scroll {
                    first: s.first,
                    visible: s.visible,
                    total: s.total,
                }),
            );
            let hover_selection = self
                .system_ui_hovered
                .filter(|row| overlay.selected != Some(*row))
                .and_then(|row| {
                    panel::contents(
                        [x, y, panel_w, panel_h],
                        &sizes,
                        overlay.items.len(),
                        Some(row),
                        None,
                    )
                    .selection
                });
            self.system_ui_hit_geometry = Some(panel::HitGeometry::new(
                [x, y, panel_w, panel_h],
                &layout,
                overlay.items.len(),
            ));

            if let Some([fx, fy, fw, fh]) = layout.query_field {
                self.sysui_fill_rounded(
                    gl,
                    fx,
                    fy,
                    fw,
                    fh,
                    panel::QUERY_RADIUS,
                    UiPalette::faded(ui.field, content_a),
                );
            }
            if let Some(hover) = hover_selection {
                // Hover is a quiet preview, distinct from the keyboard's
                // persistent selection below. Keeping both visible avoids a
                // stationary pointer stealing focus from arrow-key input.
                self.sysui_fill_rounded(
                    gl,
                    hover[0],
                    hover[1],
                    hover[2],
                    hover[3],
                    panel::SELECTION_RADIUS,
                    [
                        accent[0],
                        accent[1],
                        accent[2],
                        ui.selection_alpha * 0.32 * content_a,
                    ],
                );
            }
            if let Some(target) = layout.selection {
                // The pill slides between rows rather than teleporting, so the
                // list reads as one object being moved through. It only asks
                // for another frame while it is actually travelling.
                let pill = self.system_ui_highlight.advance_with_motion(
                    std::time::Instant::now(),
                    target,
                    motion_enabled,
                );
                if self.system_ui_highlight.animating(target) {
                    self.needs_render = true;
                }
                self.sysui_fill_rounded(
                    gl,
                    pill[0],
                    pill[1],
                    pill[2],
                    pill[3],
                    panel::SELECTION_RADIUS,
                    [
                        accent[0],
                        accent[1],
                        accent[2],
                        ui.selection_alpha * content_a,
                    ],
                );
            }
            if let Some([dx, dy, dw, dh]) = layout.divider {
                // A hairline is all the footer needs to stop reading as one
                // more row of the list.
                self.sysui_fill_rounded(
                    gl,
                    dx,
                    dy,
                    dw,
                    dh,
                    0.0,
                    UiPalette::ink(ui.hint_ink, 0.35 * content_a),
                );
            }
            if let (Some([tx, ty, tw, th]), Some([hx, hy, hw, hh])) =
                (layout.scroll_track, layout.scroll_thumb)
            {
                // Without this a windowed list looks exactly like a complete
                // one: the window manager sends a slice, and nothing else on
                // the card says so.
                self.sysui_fill_rounded(
                    gl,
                    tx,
                    ty,
                    tw,
                    th,
                    panel::SCROLLBAR_RADIUS,
                    UiPalette::faded(ui.track, content_a),
                );
                self.sysui_fill_rounded(
                    gl,
                    hx,
                    hy,
                    hw,
                    hh,
                    panel::SCROLLBAR_RADIUS,
                    UiPalette::ink(ui.item_ink, 0.55 * content_a),
                );
            }
            let query_text_pos = layout.query_text.map(|[qx, qy]| (qx, qy));
            let items_pos = layout.items.map(|[ix, iy]| (ix, iy));
            let hint_pos = layout.hint.map(|[hx, hy]| (hx, hy));

            // The ring is a circular rounded rect, so a theme whose surfaces are
            // squircles asks for none of it and relies on the shader's own rim.
            if ui.panel_ring_alpha > 0.0 {
                // Gradient accent ring around the card, matching the focused
                // window's border gradient.
                gl.UseProgram(self.gradient_border_program);
                self.set_projection_uniform(
                    gl,
                    self.gradient_border_uniforms.projection,
                    projection,
                );
                gl.Uniform1i(self.gradient_border_uniforms.scene_linear, 0);
                let ring = 1.5 * ui.ring_width;
                let [ar, ag, ab, aa] = self.border_gradient_color_a;
                let [br, bg, bb, ba] = self.border_gradient_color_b;
                gl.Uniform1f(self.gradient_border_uniforms.border_width, ring);
                gl.Uniform4f(
                    self.gradient_border_uniforms.color_a,
                    ar,
                    ag,
                    ab,
                    aa * ui.panel_ring_alpha,
                );
                gl.Uniform4f(
                    self.gradient_border_uniforms.color_b,
                    br,
                    bg,
                    bb,
                    ba * ui.panel_ring_alpha,
                );
                gl.Uniform1f(
                    self.gradient_border_uniforms.gradient_angle,
                    self.border_gradient_angle.to_radians(),
                );
                let ring_top = if radius_top > 0.0 {
                    radius_top + ring
                } else {
                    0.0
                };
                gl.Uniform1f(self.gradient_border_uniforms.radius, radius + ring);
                gl.Uniform1f(self.gradient_border_uniforms.radius_top, ring_top);
                gl.Uniform2f(
                    self.gradient_border_uniforms.size,
                    panel_w + 2.0 * ring,
                    panel_h + 2.0 * ring,
                );
                self.set_rect_uniform(
                    gl,
                    self.gradient_border_uniforms.rect,
                    x - ring,
                    y - ring,
                    panel_w + 2.0 * ring,
                    panel_h + 2.0 * ring,
                );
                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
            }

            // Text sections.
            let text_rect = super::get_uniform_loc(gl, self.sysui_text_program, "u_rect");
            let text_proj = super::get_uniform_loc(gl, self.sysui_text_program, "u_projection");
            let text_tex = super::get_uniform_loc(gl, self.sysui_text_program, "u_texture");
            let text_opacity = super::get_uniform_loc(gl, self.sysui_text_program, "u_opacity");
            gl.UseProgram(self.sysui_text_program);
            gl.UniformMatrix4fv(text_proj, 1, ffi::FALSE as u8, projection.as_ptr());
            gl.Uniform1i(text_tex, 0);
            gl.Uniform1f(text_opacity, content_a);
            gl.ActiveTexture(ffi::TEXTURE0);
            let positions = [
                Some((layout.title[0], layout.title[1])),
                query_text_pos,
                items_pos,
                hint_pos,
            ];
            for (slot, pos) in positions.into_iter().enumerate() {
                let (Some((tex, w, h)), Some((tx, ty))) = (self.sysui_textures[slot], pos) else {
                    continue;
                };
                gl.Uniform4f(text_rect, tx, ty, w as f32, h as f32);
                gl.BindTexture(ffi::TEXTURE_2D, tex);
                gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
            }
            gl.BindVertexArray(0);
            gl.UseProgram(0);
        }
    }

    /// Rasterize (and cache) one toast's title/body and action-label textures.
    unsafe fn update_toast_textures(
        &mut self,
        gl: &ffi::Gles2,
        id: u64,
        toast: &crate::backend::api::ToastNotification,
    ) {
        if self.toast_textures.contains_key(&id) {
            return;
        }
        let config = crate::config::CONFIG.load();
        let description = config.system_ui_font();
        let size = crate::backend::compositor_font::ui_font_pixel_size(description);
        let ui = ui_theme::palette();
        // Title in the brightest ink, body one step down.
        let colors: [[u8; 4]; 2] = [ui.value_ink, ui.label_ink];
        let mut set = ToastTextureSet {
            text: [None, None],
            buttons: Vec::with_capacity(toast.actions.len()),
        };
        let texts = [&toast.title, &toast.body];
        for (slot, (text, color)) in texts.into_iter().zip(colors).enumerate() {
            let text = crate::backend::compositor_font::fit_ui_text_lines(
                text,
                description,
                size,
                crate::backend::compositor_common::toast::MAX_TEXT_WIDTH_PX,
            );
            set.text[slot] = unsafe { rasterize_toast_text(gl, &text, description, size, color) };
        }
        for action in &toast.actions {
            let text = crate::backend::compositor_font::fit_ui_text_lines(
                &action.label,
                description,
                size,
                crate::backend::compositor_common::toast::MAX_ACTION_LABEL_WIDTH_PX,
            );
            set.buttons
                .push(unsafe { rasterize_toast_text(gl, &text, description, size, ui.chip_ink) });
        }
        self.toast_textures.insert(id, set);
    }

    /// Delete cached textures for the given retired toast ids.
    unsafe fn free_toast_textures(&mut self, gl: &ffi::Gles2, ids: &[u64]) {
        for id in ids {
            if let Some(set) = self.toast_textures.remove(id) {
                for slot in set.text.into_iter().chain(set.buttons).flatten() {
                    unsafe { gl.DeleteTextures(1, &slot.0) };
                }
            }
        }
    }

    /// Transient notification cards stacked in the top-right corner: rounded
    /// card, drop shadow, urgency accent stripe, title over dimmer body, an
    /// optional row of action chips, and a fade in/out envelope shared with
    /// the X11 backend.
    unsafe fn render_toasts(&mut self, gl: &ffi::Gles2, projection: &[f32; 16]) {
        let now = std::time::Instant::now();
        let mut removed = std::mem::take(&mut self.toast_retired);
        removed.extend(self.toast_stack.prune(now));
        unsafe { self.free_toast_textures(gl, &removed) };
        if self.toast_stack.is_empty() {
            self.toast_rects.clear();
            return;
        }
        // Rebuilt below from the cards actually drawn this frame, so
        // hover/click hit-testing never sees stale geometry.
        self.toast_rects.clear();

        let toasts: Vec<(u64, crate::backend::api::ToastNotification, f32)> = self
            .toast_stack
            .iter()
            .map(|toast| (toast.id, toast.notification.clone(), toast.alpha(now)))
            .collect();
        for (id, notification, _) in &toasts {
            unsafe { self.update_toast_textures(gl, *id, notification) };
        }

        let ui = ui_theme::palette();
        self.ensure_glass_backdrop(gl, ui, projection);
        let motion_enabled = crate::config::CONFIG.load().motion_enabled();
        let button_hover = self.toast_button_hover;
        let pad = 18.0;
        let pad_left = 30.0;
        let stripe_w = 3.0;

        use crate::backend::compositor_common::toast;

        // The stack hangs off the bar; the shared geometry owns the OSD slot
        // reservation and the per-card offsets so both backends place the
        // stack identically.
        let dock = self.island_dock();
        let mut top = toast::stack_start(self.osd_slot.get().is_some());

        unsafe {
            gl.BindVertexArray(self.quad_vao);
            let text_rect = super::get_uniform_loc(gl, self.sysui_text_program, "u_rect");
            let text_proj = super::get_uniform_loc(gl, self.sysui_text_program, "u_projection");
            let text_tex = super::get_uniform_loc(gl, self.sysui_text_program, "u_texture");
            let text_opacity = super::get_uniform_loc(gl, self.sysui_text_program, "u_opacity");

            for (id, notification, alpha) in &toasts {
                let slots = self
                    .toast_textures
                    .get(id)
                    .map(|set| set.text)
                    .unwrap_or([None, None]);
                let button_slots: Vec<Option<(u32, u32, u32)>> = self
                    .toast_textures
                    .get(id)
                    .map(|set| set.buttons.clone())
                    .unwrap_or_default();
                let (title_w, title_h) = slots[0]
                    .map(|(_, w, h)| (w as f32, h as f32))
                    .unwrap_or((0.0, 0.0));
                let (body_w, body_h) = slots[1]
                    .map(|(_, w, h)| (w as f32, h as f32))
                    .unwrap_or((0.0, 0.0));
                let button_widths: Vec<f32> = button_slots
                    .iter()
                    .map(|slot| slot.map(|(_, w, _)| w as f32).unwrap_or(0.0))
                    .collect();
                let content_w = title_w
                    .max(body_w)
                    .max(toast::action_row_width(&button_widths))
                    .clamp(
                        220.0,
                        crate::backend::compositor_common::toast::MAX_TEXT_WIDTH_PX as f32,
                    );
                let target_w = content_w + pad_left + pad;
                let mut target_h = 2.0 * pad + title_h;
                if body_h > 0.0 {
                    target_h += 6.0 + body_h;
                }
                if !button_slots.is_empty() {
                    target_h += toast::ACTIONS_ROW_EXTRA_H;
                }

                let (card_w, card_h) = self
                    .toast_stack
                    .motion_for(*id)
                    .map_or((target_w, target_h), |motion| {
                        motion.advance_with_motion(now, target_w, target_h, motion_enabled)
                    });
                let [x, y, ..] = dock.rect(card_w, card_h, top);
                // The chip row hangs under the text block, aligned with it.
                let text_bottom = pad + title_h + if body_h > 0.0 { 6.0 + body_h } else { 0.0 };
                let button_rects = toast::action_row_layout(
                    &button_widths,
                    x + pad_left,
                    y + text_bottom + toast::ACTION_ROW_TOP_GAP,
                );
                self.toast_rects.push(toast::ToastRects {
                    id: *id,
                    card: [x, y, card_w, card_h],
                    buttons: button_rects.clone(),
                });
                // Only the card actually touching the bar squares off; the
                // dock also refuses to square anything when there is no bar.
                let (radius_top, radius) = dock.radii(card_h, ui.toast_radius, top);
                let a = *alpha;
                let opened = (card_w / target_w.max(1.0)).clamp(0.0, 1.0);
                let content_a = a * opened * opened;
                let accent = match notification.urgency {
                    2 => [0.95, 0.30, 0.30, 1.0],
                    0 => [0.45, 0.50, 0.62, 1.0],
                    _ => self.border_gradient_color_a,
                };

                // No drop shadow: the top edge is flush with the bar, and a
                // shadow spreading up over it is the seam this removes.
                self.ui_fill_island(
                    gl, projection, ui, x, y, card_w, card_h, radius, radius_top, ui.toast, a,
                );
                self.sysui_fill_rounded(
                    gl,
                    x + 13.0,
                    y + 13.0,
                    stripe_w,
                    (card_h - 26.0).max(0.0),
                    1.5,
                    [accent[0], accent[1], accent[2], 0.9 * content_a],
                );
                // Action chips: raised chip fill with an accent hairline; the
                // hovered chip trades its fill for an accent wash.
                for (index, rect) in button_rects.iter().enumerate() {
                    let hovered = button_hover == Some((*id, index));
                    let fill = if hovered {
                        [accent[0], accent[1], accent[2], 0.45 * content_a]
                    } else {
                        UiPalette::faded(ui.chip, content_a)
                    };
                    self.sysui_fill_rounded(
                        gl,
                        rect[0],
                        rect[1],
                        rect[2],
                        rect[3],
                        ui.chip_radius,
                        fill,
                    );
                    self.sysui_stroke_rounded(
                        gl,
                        rect[0],
                        rect[1],
                        rect[2],
                        rect[3],
                        ui.chip_radius,
                        1.0,
                        [accent[0], accent[1], accent[2], 0.8 * content_a],
                    );
                }

                gl.UseProgram(self.sysui_text_program);
                gl.UniformMatrix4fv(text_proj, 1, ffi::FALSE as u8, projection.as_ptr());
                gl.Uniform1i(text_tex, 0);
                gl.Uniform1f(text_opacity, content_a);
                gl.ActiveTexture(ffi::TEXTURE0);
                if let Some((tex, w, h)) = slots[0] {
                    gl.Uniform4f(text_rect, x + pad_left, y + pad, w as f32, h as f32);
                    gl.BindTexture(ffi::TEXTURE_2D, tex);
                    gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                }
                if let Some((tex, w, h)) = slots[1] {
                    gl.Uniform4f(
                        text_rect,
                        x + pad_left,
                        y + pad + title_h + 6.0,
                        w as f32,
                        h as f32,
                    );
                    gl.BindTexture(ffi::TEXTURE_2D, tex);
                    gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                }
                for (index, rect) in button_rects.iter().enumerate() {
                    if let Some((tex, w, h)) = button_slots.get(index).copied().flatten() {
                        // Centered in the chip.
                        gl.Uniform4f(
                            text_rect,
                            rect[0] + (rect[2] - w as f32) / 2.0,
                            rect[1] + (rect[3] - h as f32) / 2.0,
                            w as f32,
                            h as f32,
                        );
                        gl.BindTexture(ffi::TEXTURE_2D, tex);
                        gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
                    }
                }

                top = toast::stack_next(top, target_h);
            }
            gl.BindVertexArray(0);
            gl.UseProgram(0);
        }
    }

    /// Rasterize (and cache) the OSD label texture; re-render only when the
    /// text changed (key repeat updates the percent every event).
    unsafe fn update_osd_texture(&mut self, gl: &ffi::Gles2, text: &str) {
        if self
            .osd_texture
            .as_ref()
            .is_some_and(|(cached, _, _, _)| cached == text)
        {
            return;
        }
        if let Some((_, tex, _, _)) = self.osd_texture.take() {
            unsafe { gl.DeleteTextures(1, &tex) };
        }
        let config = crate::config::CONFIG.load();
        let description = config.system_ui_font();
        let size = crate::backend::compositor_font::ui_font_pixel_size(description);
        let (pixels, w, h) = crate::backend::compositor_font::render_ui_text_to_rgba(
            text,
            description,
            size,
            ui_theme::palette().osd_ink,
        );
        if w == 0 || h == 0 {
            return;
        }
        unsafe {
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
            self.osd_texture = Some((text.to_string(), tex, w, h));
        }
    }

    /// Volume/brightness OSD: one replace-in-place pill card at the bottom
    /// center — icon+percent label on the left, progress bar on the right —
    /// with the hold+fade envelope shared with the X11 backend.
    unsafe fn render_osd(&mut self, gl: &ffi::Gles2, projection: &[f32; 16]) {
        let now = std::time::Instant::now();
        if self.osd_slot.prune(now) {
            if let Some((_, tex, _, _)) = self.osd_texture.take() {
                unsafe { gl.DeleteTextures(1, &tex) };
            }
        }
        let Some(osd) = self.osd_slot.get() else {
            return;
        };
        let a = osd.alpha(now);
        let (icon, label) = osd.icon_and_label();
        let fill = osd.fill();
        let target_w = osd.card_width();
        let text = format!("{icon}  {label}");
        unsafe { self.update_osd_texture(gl, &text) };
        let Some((tex, text_w, text_h)) =
            self.osd_texture.as_ref().map(|&(_, tex, w, h)| (tex, w, h))
        else {
            return;
        };

        let ui = ui_theme::palette();
        self.ensure_glass_backdrop(gl, ui, projection);
        let target_h = crate::backend::compositor_common::osd::OSD_CARD_HEIGHT;
        let pad = 24.0;
        // Fixed label zone so the bar does not shift as digits change.
        let label_zone = 118.0;
        let accent = self.border_gradient_color_a;

        let dock = self.island_dock();
        let motion_enabled = crate::config::CONFIG.load().motion_enabled();
        let (card_w, card_h) =
            self.osd_slot
                .motion_mut()
                .advance_with_motion(now, target_w, target_h, motion_enabled);
        let [x, y, ..] = dock.rect(card_w, card_h, 0.0);
        let (radius_top, radius) = dock.radii(card_h, ui.osd_radius, 0.0);
        // Contents appear as the card makes room for them, rather than
        // overflowing a card that is still only a seed wide.
        let opened = (card_w / target_w.max(1.0)).clamp(0.0, 1.0);
        let content_a = a * opened * opened;

        unsafe {
            gl.BindVertexArray(self.quad_vao);

            // No drop shadow: the top edge is flush with the bar, and a shadow
            // spreading up over it is exactly the seam the effect removes.
            self.ui_fill_island(
                gl, projection, ui, x, y, card_w, card_h, radius, radius_top, ui.osd, a,
            );

            // Progress bar: dim track + accent fill. Label-only kinds (media)
            // report no fill and give the whole card to the text.
            let bar_w = card_w - label_zone - pad;
            if let Some(fill) = fill
                && bar_w > 0.0
            {
                let bar_x = x + label_zone;
                let bar_h = 6.0;
                let bar_y = y + (card_h - bar_h) / 2.0;
                self.sysui_fill_rounded(
                    gl,
                    bar_x,
                    bar_y,
                    bar_w,
                    bar_h,
                    bar_h / 2.0,
                    UiPalette::faded(ui.slider_track, content_a),
                );
                if fill > 0.0 {
                    self.sysui_fill_rounded(
                        gl,
                        bar_x,
                        bar_y,
                        (bar_w * fill).max(bar_h),
                        bar_h,
                        bar_h / 2.0,
                        [accent[0], accent[1], accent[2], 0.95 * content_a],
                    );
                }
            }

            gl.UseProgram(self.sysui_text_program);
            let text_rect = super::get_uniform_loc(gl, self.sysui_text_program, "u_rect");
            let text_proj = super::get_uniform_loc(gl, self.sysui_text_program, "u_projection");
            let text_tex = super::get_uniform_loc(gl, self.sysui_text_program, "u_texture");
            let text_opacity = super::get_uniform_loc(gl, self.sysui_text_program, "u_opacity");
            gl.UniformMatrix4fv(text_proj, 1, ffi::FALSE as u8, projection.as_ptr());
            gl.Uniform1i(text_tex, 0);
            gl.Uniform1f(text_opacity, content_a);
            gl.ActiveTexture(ffi::TEXTURE0);
            gl.Uniform4f(
                text_rect,
                x + pad,
                y + (card_h - text_h as f32) / 2.0,
                text_w as f32,
                text_h as f32,
            );
            gl.BindTexture(ffi::TEXTURE_2D, tex);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);

            gl.BindVertexArray(0);
            gl.UseProgram(0);
        }
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
