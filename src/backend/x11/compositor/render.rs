// render_frame and rendering helpers
use super::features::recording_capture_warranted;
#[allow(unused_imports)]
use super::math::ortho;
#[allow(unused_imports)]
use super::*;
use crate::backend::compositor_common::attention::{
    attention_border_style, attention_signal_active,
};
use crate::backend::compositor_common::debug_hud as hud;
use crate::backend::compositor_common::dynamic_island::{IslandDock, clip_bar_to_viewport};
use crate::backend::compositor_common::genie::{
    GenieDirection, dock_item_preview_target, output_bounds_for_anchor, preview_rect,
};
use crate::backend::compositor_common::minimized_thumbnail::{ThumbnailPurpose, ThumbnailSource};
use crate::backend::compositor_common::system_ui_panel as panel;
use crate::backend::compositor_common::ui_theme::{self, UiPalette};
use crate::backend::compositor_common::window_glow::{
    WindowGlowSettings, WindowGlowStyle, WindowGlowTarget,
};
#[allow(unused_imports)]
use glow::HasContext;
#[allow(unused_imports)]
use std::collections::HashMap;
#[allow(unused_imports)]
use std::ffi::CString;
#[allow(unused_imports)]
use std::sync::Arc;
#[allow(unused_imports)]
use std::sync::mpsc;

type GlScissor = (i32, i32, i32, i32);

#[derive(Clone, Copy)]
struct MinimizedRenderSource {
    texture: glow::Texture,
    has_alpha: bool,
    width: f32,
    height: f32,
    uv_rect: [f32; 4],
}

fn transformed_overlays_require_full_redraw(
    overview_active: bool,
    overview_closing: bool,
    expose_active: bool,
    has_expose_entries: bool,
) -> bool {
    overview_active || overview_closing || expose_active || has_expose_entries
}

fn minimized_dock_requires_composition(
    has_targeted_cached_visual: bool,
    has_preview: bool,
    iconic_recapture_pending: bool,
) -> bool {
    has_targeted_cached_visual || has_preview || iconic_recapture_pending
}

fn screenshot_freeze_requires_composition(capture_pending: bool, scene_captured: bool) -> bool {
    capture_pending || scene_captured
}

fn screenshot_freeze_change_needed(
    requested: bool,
    capture_pending: bool,
    scene_captured: bool,
) -> bool {
    requested != (capture_pending || scene_captured)
}

/// Resolve and consume each transient render source before resolving the next
/// one. Minimized thumbnail sources contain bare GL object names: resolving a
/// later CPU-only item may upload it and evict an older GPU-LRU entry, so a
/// batch of resolved sources cannot safely outlive another resolution step.
fn resolve_and_draw_each<State, Item, Source>(
    state: &mut State,
    items: impl IntoIterator<Item = Item>,
    mut resolve: impl FnMut(&mut State, Item) -> Option<Source>,
    mut draw: impl FnMut(&mut State, Source),
) {
    for item in items {
        let Some(source) = resolve(state, item) else {
            continue;
        };
        draw(state, source);
    }
}

/// Whether a composited window participates in the smart-border rule (a lone
/// client draws no border) and may itself receive one.
///
/// The status bar is chrome, and override-redirect windows are unmanaged
/// overlays the WM never tiles: IME candidate lists and the input-method
/// switcher (fcitx5 creates those with `override_redirect`), menus, tooltips
/// and drag icons. Counting them would draw a border around the single client
/// of a tag for as long as the popup is up — e.g. the whole time a user types
/// Chinese — and drop it again when the popup closes.
fn counts_for_smart_borders(class_name: &str, status_bar_name: &str, is_or: bool) -> bool {
    if is_or {
        return false;
    }
    !(class_name == status_bar_name || class_name.contains(status_bar_name))
}

fn is_status_bar_class(class_name: &str, status_bar_name: &str) -> bool {
    !status_bar_name.is_empty()
        && (class_name == status_bar_name || class_name.contains(status_bar_name))
}

fn tfp_refresh_is_latency_critical(
    window: u32,
    focused: Option<u32>,
    class_name: &str,
    status_bar_name: &str,
) -> bool {
    Some(window) == focused || is_status_bar_class(class_name, status_bar_name)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TransitionCapturePlan {
    src: (i32, i32, i32, i32),
    dst: (i32, i32, i32, i32),
}

/// Plan an unscaled GL blit from a full-output snapshot into a monitor-sized
/// transition target. Coordinates outside the root output are clipped without
/// stretching the visible portion.
fn transition_capture_plan(
    screen_w: u32,
    screen_h: u32,
    mon_x: i32,
    mon_y: i32,
    mon_w: u32,
    mon_h: u32,
    exclude_top: u32,
) -> Option<TransitionCapturePlan> {
    let screen_w = i32::try_from(screen_w).ok()?;
    let screen_h = i32::try_from(screen_h).ok()?;
    let mon_w = i32::try_from(mon_w).ok()?;
    let mon_h = i32::try_from(mon_h).ok()?;
    let exclude_top = i32::try_from(exclude_top.min(mon_h as u32)).ok()?;
    let workspace_h = mon_h.checked_sub(exclude_top)?;
    if screen_w <= 0 || screen_h <= 0 || mon_w <= 0 || workspace_h <= 0 {
        return None;
    }

    // GL's origin is at the bottom-left. Excluding a top bar therefore keeps
    // the lower `workspace_h` rows starting at the monitor's GL-space bottom.
    let source_x0 = mon_x;
    let source_y0 =
        i64::from(screen_h).checked_sub(i64::from(mon_y).checked_add(i64::from(mon_h))?)?;
    let source_x1 = i64::from(mon_x).checked_add(i64::from(mon_w))?;
    let source_y1 = source_y0.checked_add(i64::from(workspace_h))?;

    let clipped_x0 = i64::from(source_x0).clamp(0, i64::from(screen_w));
    let clipped_y0 = source_y0.clamp(0, i64::from(screen_h));
    let clipped_x1 = source_x1.clamp(0, i64::from(screen_w));
    let clipped_y1 = source_y1.clamp(0, i64::from(screen_h));
    if clipped_x1 <= clipped_x0 || clipped_y1 <= clipped_y0 {
        return None;
    }

    let dst_x0 = clipped_x0.checked_sub(i64::from(source_x0))?;
    let dst_y0 = clipped_y0.checked_sub(source_y0)?;
    let width = clipped_x1.checked_sub(clipped_x0)?;
    let height = clipped_y1.checked_sub(clipped_y0)?;

    Some(TransitionCapturePlan {
        src: (
            i32::try_from(clipped_x0).ok()?,
            i32::try_from(clipped_y0).ok()?,
            i32::try_from(clipped_x1).ok()?,
            i32::try_from(clipped_y1).ok()?,
        ),
        dst: (
            i32::try_from(dst_x0).ok()?,
            i32::try_from(dst_y0).ok()?,
            i32::try_from(dst_x0.checked_add(width)?).ok()?,
            i32::try_from(dst_y0.checked_add(height)?).ok()?,
        ),
    })
}

fn full_output_copy_extent(width: u32, height: u32) -> Option<(i32, i32)> {
    let width = i32::try_from(width).ok()?;
    let height = i32::try_from(height).ok()?;
    (width > 0 && height > 0).then_some((width, height))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PresentedSceneCopyPlan {
    Disabled,
    Full,
    Region(GlScissor),
}

/// Choose how much of the persistent scene texture must be synchronized.
///
/// An absent/invalid snapshot needs one full copy. Once valid, a repaired
/// partial-damage frame only changes pixels inside the GL-space repair box, so
/// updating that same rectangle keeps a complete snapshot without a full 4K
/// blit on every frame.
fn presented_scene_copy_plan(
    transitions_enabled: bool,
    snapshot_usable: bool,
    repair_scissor: Option<GlScissor>,
    width: u32,
    height: u32,
) -> PresentedSceneCopyPlan {
    if !transitions_enabled {
        return PresentedSceneCopyPlan::Disabled;
    }
    let Some((width, height)) = full_output_copy_extent(width, height) else {
        return PresentedSceneCopyPlan::Disabled;
    };
    if !snapshot_usable {
        return PresentedSceneCopyPlan::Full;
    }
    let Some(repair) = repair_scissor else {
        return PresentedSceneCopyPlan::Full;
    };
    let output = (0, 0, width, height);
    match intersect_gl_scissors(output, repair) {
        Some(region) if region != output => PresentedSceneCopyPlan::Region(region),
        _ => PresentedSceneCopyPlan::Full,
    }
}

fn intersect_gl_scissors(a: GlScissor, b: GlScissor) -> Option<GlScissor> {
    let x0 = a.0.max(b.0);
    let y0 = a.1.max(b.1);
    let x1 = a.0.saturating_add(a.2).min(b.0.saturating_add(b.2));
    let y1 = a.1.saturating_add(a.3).min(b.1.saturating_add(b.3));
    (x1 > x0 && y1 > y0).then_some((x0, y0, x1 - x0, y1 - y0))
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct WallpaperBlendPlan {
    old_global_opacity: Option<f32>,
    current_opacity: Option<f32>,
}

/// Select the layers for one output.
///
/// An output with a monitor override never participates in the global
/// transition. Global fallbacks draw the old image opaque and the new image
/// over it at `progress`; this is the correct interpolation for ordinary
/// source-over alpha blending and avoids dimming halfway through the fade.
fn wallpaper_blend_plan(
    has_monitor_override: bool,
    has_current_global: bool,
    has_old_global: bool,
    transition_progress: Option<f32>,
) -> WallpaperBlendPlan {
    if has_monitor_override {
        return WallpaperBlendPlan {
            old_global_opacity: None,
            current_opacity: Some(1.0),
        };
    }

    if has_old_global && transition_progress.is_some() {
        let progress = transition_progress
            .filter(|progress| progress.is_finite())
            .unwrap_or(1.0)
            .clamp(0.0, 1.0);
        WallpaperBlendPlan {
            old_global_opacity: Some(1.0),
            current_opacity: has_current_global.then_some(progress),
        }
    } else {
        WallpaperBlendPlan {
            old_global_opacity: None,
            current_opacity: has_current_global.then_some(1.0),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct FocusHighlightStyle {
    color: [f32; 4],
    width: f32,
}

/// Blend the transient focus indication into the ordinary focused border.
///
/// Keeping both endpoints identical to the stable border avoids a transparent
/// first/last animation frame.  The client texture itself is deliberately not
/// transformed: scaling terminal content made text and the insertion cursor
/// appear to flash every time focus changed.
fn focus_highlight_style(
    focused_color: [f32; 4],
    highlight_color: [f32; 4],
    focused_width: f32,
    progress: f32,
) -> FocusHighlightStyle {
    let progress = progress
        .is_finite()
        .then_some(progress)
        .unwrap_or(1.0)
        .clamp(0.0, 1.0);
    let pulse = (progress * std::f32::consts::PI).sin().max(0.0);
    let mut color = focused_color;
    for (channel, highlight) in color.iter_mut().zip(highlight_color) {
        *channel += (highlight - *channel) * pulse;
    }
    let highlight_width = (focused_width + 2.0).max(3.0);
    FocusHighlightStyle {
        color,
        width: focused_width + (highlight_width - focused_width) * pulse,
    }
}

fn enclosing_dirty_rect(x: f32, y: f32, w: f32, h: f32) -> DirtyRect {
    let left = x.floor() as i32;
    let top = y.floor() as i32;
    let right = (x + w.max(0.0)).ceil() as i32;
    let bottom = (y + h.max(0.0)).ceil() as i32;
    DirtyRect::new(
        left,
        top,
        right.saturating_sub(left) as u32,
        bottom.saturating_sub(top) as u32,
    )
}

fn rect_covers_output(x: i32, y: i32, width: u32, height: u32, sw: u32, sh: u32) -> bool {
    x <= 0
        && y <= 0
        && i64::from(x) + i64::from(width) >= i64::from(sw)
        && i64::from(y) + i64::from(height) >= i64::from(sh)
}

/// Whether a window's role permits direct X presentation.
///
/// Fullscreen windows retain the existing automatic optimization. EWMH value
/// `1` additionally permits a non-fullscreen window that covers the output;
/// value `2` explicitly inhibits bypass, including for fullscreen windows.
fn window_prefers_direct_presentation(is_fullscreen: bool, bypass_compositor: u8) -> bool {
    match bypass_compositor {
        1 => true,
        2 => false,
        _ => is_fullscreen,
    }
}

/// A single XComposite redirect owner cannot be replaced by assigning a new
/// marker. Restore the previous client before considering direct presentation
/// for a different focused window; otherwise the old window remains physically
/// unredirected with no state left from which to recover it.
fn direct_presentation_owner_changed(previous: u32, focused: Option<u32>) -> bool {
    Some(previous) != focused
}

fn edge_effects_require_composition(
    direct_candidate: bool,
    is_fullscreen: bool,
    shadow_enabled: bool,
    border_enabled: bool,
    border_width: f32,
    corner_radius: f32,
) -> bool {
    !direct_candidate
        && ((shadow_enabled && !is_fullscreen)
            || (border_enabled && border_width > 0.0)
            || corner_radius > 0.0)
}

/// Whether a window can safely hide every lower layer.
///
/// This is intentionally conservative. A fullscreen source rectangle is not
/// an occluder when its final draw can expose even one destination pixel via
/// alpha, rounded/shaped edges, scaling, or a deformation shader.
fn is_opaque_occluder(
    has_rgba: bool,
    layer_opacity: f32,
    corner_radius: f32,
    is_shaped: bool,
    window_scale: f32,
    animation_scale: f32,
    geometry_deformation_active: bool,
) -> bool {
    let identity_scale = |scale: f32| scale.is_finite() && (scale - 1.0).abs() <= f32::EPSILON;

    !has_rgba
        && layer_opacity.is_finite()
        && layer_opacity >= 1.0
        && corner_radius.is_finite()
        && corner_radius <= 0.0
        && !is_shaped
        && identity_scale(window_scale)
        && identity_scale(animation_scale)
        && !geometry_deformation_active
}

/// Conservative screen-space reach of the dual-Kawase filter.
///
/// Every extra level doubles the source-pixel footprint.  Keeping a slightly
/// wider margin than the exact kernel avoids stale blur along adjoining window
/// edges without coupling distant tiled clients.
fn blur_sampling_margin(blur_levels: usize) -> i32 {
    1i32 << (blur_levels.min(6) as u32 + 2)
}

fn blur_sampling_rect(backdrop: DirtyRect, blur_levels: usize) -> DirtyRect {
    let margin = blur_sampling_margin(blur_levels);
    DirtyRect::new(
        backdrop.x.saturating_sub(margin),
        backdrop.y.saturating_sub(margin),
        backdrop
            .width
            .saturating_add((margin as u32).saturating_mul(2)),
        backdrop
            .height
            .saturating_add((margin as u32).saturating_mul(2)),
    )
}

fn dirty_below_affects_backdrop(
    dirty_below: &[DirtyRect],
    backdrop: DirtyRect,
    blur_levels: usize,
) -> bool {
    let sampling_rect = blur_sampling_rect(backdrop, blur_levels);
    dirty_below
        .iter()
        .any(|dirty| sampling_rect.intersects(dirty))
}

/// Return whether a damaged lower window can affect a later blur consumer.
///
/// `scene` is ordered bottom-to-top and `dirty_windows` must be sorted. Damage
/// above a consumer is deliberately ignored because it is not part of that
/// consumer's backdrop.
fn dirty_below_requires_full_blur_redraw(
    scene: &[(u32, i32, i32, u32, u32)],
    dirty_windows: &[u32],
    blur_levels: usize,
    mut is_blur_consumer: impl FnMut(u32) -> bool,
) -> bool {
    scene
        .iter()
        .enumerate()
        .any(|(consumer_index, &(win, x, y, w, h))| {
            if !is_blur_consumer(win) {
                return false;
            }
            let sampling_rect = blur_sampling_rect(DirtyRect::new(x, y, w, h), blur_levels);
            scene[..consumer_index].iter().any(
                |&(below_win, below_x, below_y, below_w, below_h)| {
                    dirty_windows.binary_search(&below_win).is_ok()
                        && sampling_rect
                            .intersects(&DirtyRect::new(below_x, below_y, below_w, below_h))
                },
            )
        })
}

impl<C: CompositorConnection> Compositor<C> {
    /// Arm or disarm the interactive screenshot scene freeze. The actual copy
    /// is deferred until the next completed scene, so the selection overlay
    /// can never become part of the frozen image.
    pub(crate) fn set_screenshot_freeze(&mut self, active: bool) {
        if !screenshot_freeze_change_needed(
            active,
            self.screenshot_freeze_pending,
            self.screenshot_freeze_fbo.is_some(),
        ) {
            return;
        }
        self.screenshot_freeze_pending = active;
        if active {
            // The freeze target is a complete full-output image. Do not let
            // its source depend on the contents retained by an EGL/GLX
            // partial-damage back buffer: the next frame must rebuild every
            // pixel before capture.
            self.damage_tracker.mark_all_dirty();
            self.dirty_region_tracker.mark_all_dirty();
            self.buffer_age_damage_history.clear();
        } else {
            if let Some((fbo, texture)) = self.screenshot_freeze_fbo.take() {
                unsafe {
                    self.gl.delete_framebuffer(fbo);
                    self.gl.delete_texture(texture);
                }
            }
            self.screenshot_freeze_size = None;
        }
        self.needs_render = true;
    }

    fn capture_screenshot_freeze(&mut self) {
        if !self.screenshot_freeze_pending {
            return;
        }
        let size_changed = self.screenshot_freeze_size != Some((self.screen_w, self.screen_h));
        if size_changed {
            if let Some((fbo, texture)) = self.screenshot_freeze_fbo.take() {
                unsafe {
                    self.gl.delete_framebuffer(fbo);
                    self.gl.delete_texture(texture);
                }
            }
            self.screenshot_freeze_size = None;
        }
        if self.screenshot_freeze_fbo.is_none() {
            match unsafe { Self::create_scene_fbo(&self.gl, self.screen_w, self.screen_h) } {
                Ok(target) => {
                    self.screenshot_freeze_fbo = Some(target);
                    self.screenshot_freeze_size = Some((self.screen_w, self.screen_h));
                }
                Err(error) => {
                    log::warn!(
                        "{}: {error}",
                        self.renderer_ctx("screenshot-freeze: allocate scene FBO")
                    );
                    // Freezing is a visual convenience, not a prerequisite
                    // for the editor. Fail open to the live scene instead of
                    // retaining a request that blocks fullscreen bypass and
                    // retries the same allocation on every later damage frame.
                    self.screenshot_freeze_pending = false;
                    self.screenshot_freeze_size = None;
                    return;
                }
            }
        }
        let Some((freeze_fbo, _)) = self.screenshot_freeze_fbo else {
            return;
        };
        unsafe {
            // set_screenshot_freeze forced this frame to redraw the complete
            // output. Capture that freshly rendered scene, not the persistent
            // transition snapshot: the latter is maintained incrementally and
            // may still contain undefined pixels from an older partial-damage
            // back buffer. A stale GL scissor must not clip this full copy.
            let scissor_enabled = self.gl.is_enabled(glow::SCISSOR_TEST);
            if scissor_enabled {
                self.gl.disable(glow::SCISSOR_TEST);
            }
            self.gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
            self.gl
                .bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(freeze_fbo));
            self.gl.blit_framebuffer(
                0,
                0,
                self.screen_w as i32,
                self.screen_h as i32,
                0,
                0,
                self.screen_w as i32,
                self.screen_h as i32,
                glow::COLOR_BUFFER_BIT,
                glow::NEAREST,
            );
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.gl
                .viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
            if scissor_enabled {
                self.gl.enable(glow::SCISSOR_TEST);
            }
        }
        self.screenshot_freeze_pending = false;
    }

    fn render_screenshot_freeze(&self, projection: &[f32; 16]) {
        let Some((_, texture)) = self.screenshot_freeze_fbo else {
            return;
        };
        unsafe {
            let scissor_enabled = self.gl.is_enabled(glow::SCISSOR_TEST);
            let blend_enabled = self.gl.is_enabled(glow::BLEND);
            if scissor_enabled {
                self.gl.disable(glow::SCISSOR_TEST);
            }
            // This texture is the already-composited final scene. Replace the
            // live scene exactly instead of interpreting its stored alpha as
            // another translucent layer.
            if blend_enabled {
                self.gl.disable(glow::BLEND);
            }
            self.gl.use_program(Some(self.transition_program));
            self.gl.uniform_matrix_4_f32_slice(
                self.transition_uniforms.projection.as_ref(),
                false,
                projection,
            );
            self.gl
                .uniform_1_i32(self.transition_uniforms.texture.as_ref(), 0);
            self.gl.uniform_4_f32(
                self.transition_uniforms.rect.as_ref(),
                0.0,
                0.0,
                self.screen_w as f32,
                self.screen_h as f32,
            );
            self.gl
                .uniform_1_f32(self.transition_uniforms.opacity.as_ref(), 1.0);
            self.gl.uniform_4_f32(
                self.transition_uniforms.uv_rect.as_ref(),
                0.0,
                0.0,
                1.0,
                1.0,
            );
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            self.gl.bind_vertex_array(Some(self.quad_vao));
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            self.gl.bind_vertex_array(None);
            self.gl.bind_texture(glow::TEXTURE_2D, None);
            self.gl.use_program(None);
            if blend_enabled {
                self.gl.enable(glow::BLEND);
            }
            if scissor_enabled {
                self.gl.enable(glow::SCISSOR_TEST);
            }
        }
    }

    // =====================================================================
    // Tag-switch slide transition
    // =====================================================================

    /// Called just before a tag switch. Crops the compositor-owned copy of the
    /// last successfully presented scene into a monitor-sized transition
    /// texture so `render_frame` can animate the old scene out.
    ///
    /// A platform back buffer is deliberately never read here: after
    /// SwapBuffers its contents are undefined, while waiting for the next
    /// render would capture the already-switched tag.
    /// `mon_rect` is (x, y, w, h) of the monitor where the switch happens.
    pub(crate) fn notify_tag_switch(
        &mut self,
        duration: std::time::Duration,
        direction: i32,
        exclude_top: u32,
        mon_rect: (i32, i32, u32, u32),
    ) {
        // Ensure the selected graphics context is current.
        if !self.context_current {
            if let Err(error) = self.graphics.make_current() {
                log::error!(
                    "{}: {error}",
                    self.renderer_ctx("transition: make context current")
                );
                return;
            }
            self.context_current = true;
        }

        let (mon_x, mon_y, mon_w, mon_h) = mon_rect;
        let mon_w = mon_w.max(1);
        let mon_h = mon_h.max(1);
        if full_output_copy_extent(mon_w, mon_h).is_none() {
            self.transition_start = None;
            self.retire_transition_targets();
            self.force_full_redraw();
            log::warn!(
                "compositor: tag-switch transition skipped (monitor dimensions overflow GL)"
            );
            return;
        }

        let Some(source_fbo) = self.presented_scene_fbo.as_ref().and_then(|(fbo, _)| {
            self.presented_scene_status
                .is_usable(self.screen_w, self.screen_h)
                .then_some(*fbo)
        }) else {
            // This is expected before the compositor's first successful
            // frame, after a resize, or after a failed swap. Switch tags
            // immediately instead of animating undefined/stale pixels.
            self.transition_start = None;
            self.retire_transition_targets();
            self.force_full_redraw();
            log::debug!(
                "compositor: tag-switch transition skipped (no stable presented-scene snapshot)"
            );
            return;
        };

        // Recreate FBOs if monitor size changed
        let size_changed = self.transition_fbo.as_ref().map_or(true, |_| {
            self.transition_mon_w != mon_w || self.transition_mon_h != mon_h
        });
        if size_changed {
            if let Some((fbo, tex)) = self.transition_fbo.take() {
                unsafe {
                    self.gl.delete_framebuffer(fbo);
                    self.gl.delete_texture(tex);
                }
            }
            if let Some((fbo, tex)) = self.transition_new_fbo.take() {
                unsafe {
                    self.gl.delete_framebuffer(fbo);
                    self.gl.delete_texture(tex);
                }
            }
        }

        // Create snapshot FBO at monitor size
        if self.transition_fbo.is_none() {
            match unsafe { Self::create_scene_fbo(&self.gl, mon_w, mon_h) } {
                Ok(target) => self.transition_fbo = Some(target),
                Err(error) => {
                    self.transition_start = None;
                    self.retire_transition_targets();
                    self.force_full_redraw();
                    log::warn!(
                        "{}: {error}",
                        self.renderer_ctx("transition: allocate target FBO")
                    );
                    return;
                }
            }
        }

        // Create new-scene FBO for modes that need both old and new textures
        let needs_new_fbo = self.transition_mode.needs_new_scene_fbo();
        if needs_new_fbo && self.transition_new_fbo.is_none() {
            match unsafe { Self::create_scene_fbo(&self.gl, mon_w, mon_h) } {
                Ok(target) => self.transition_new_fbo = Some(target),
                Err(error) => {
                    self.transition_start = None;
                    self.retire_transition_targets();
                    self.force_full_redraw();
                    log::warn!(
                        "{}: {error}",
                        self.renderer_ctx("transition: allocate secondary target FBO")
                    );
                    return;
                }
            }
        }

        // Store monitor rect for rendering
        self.transition_mon_x = mon_x;
        self.transition_mon_y = mon_y;
        self.transition_mon_w = mon_w;
        self.transition_mon_h = mon_h;

        if let Some((snap_fbo, snap_tex)) = &self.transition_fbo {
            let snap_fbo = *snap_fbo;
            let snap_tex = *snap_tex;
            self.transition_exclude_top = exclude_top.min(mon_h.saturating_sub(1));
            if !self.capture_transition_scene_from(
                Some(source_fbo),
                snap_fbo,
                mon_x,
                mon_y,
                mon_w,
                mon_h,
            ) {
                self.transition_start = None;
                self.retire_transition_targets();
                self.force_full_redraw();
                log::warn!(
                    "compositor: tag-switch transition skipped (monitor outside stable snapshot)"
                );
                return;
            }
            self.build_transition_mipmaps(snap_tex);
            self.transition_start = Some(std::time::Instant::now());
            // Solid-object modes need more time than a flat wipe to read.
            self.transition_duration = self.transition_mode.stretch_duration(duration);
            self.transition_new_ready = false;
            self.transition_direction = if direction >= 0 { 1.0 } else { -1.0 };
            // Tag switch can radically change visible scene; force a full redraw
            // to avoid stale pixels from partial-damage scissor regions.
            self.damage_tracker.mark_all_dirty();
            self.dirty_region_tracker.mark_all_dirty(); // P5C: Sync rect tracker
            self.needs_render = true;
            log::debug!(
                "compositor: tag-switch slide transition started ({:?}, dir={}, mon={}x{}+{}+{})",
                duration,
                direction,
                mon_w,
                mon_h,
                mon_x,
                mon_y,
            );
        }
    }

    pub(super) fn capture_transition_scene(
        &self,
        dst_fbo: glow::Framebuffer,
        mon_x: i32,
        mon_y: i32,
        mon_w: u32,
        mon_h: u32,
    ) -> bool {
        self.capture_transition_scene_from(None, dst_fbo, mon_x, mon_y, mon_w, mon_h)
    }

    pub(super) fn capture_transition_scene_from(
        &self,
        source_fbo: Option<glow::Framebuffer>,
        dst_fbo: glow::Framebuffer,
        mon_x: i32,
        mon_y: i32,
        mon_w: u32,
        mon_h: u32,
    ) -> bool {
        let exclude_top = self.transition_exclude_top.min(mon_h);
        let Some(plan) = transition_capture_plan(
            self.screen_w,
            self.screen_h,
            mon_x,
            mon_y,
            mon_w,
            mon_h,
            exclude_top,
        ) else {
            return false;
        };

        unsafe {
            let scissor_enabled = self.gl.is_enabled(glow::SCISSOR_TEST);
            if scissor_enabled {
                self.gl.disable(glow::SCISSOR_TEST);
            }
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(dst_fbo));
            self.gl.viewport(0, 0, mon_w as i32, mon_h as i32);
            self.gl.clear_color(0.0, 0.0, 0.0, 0.0);
            self.gl.clear(glow::COLOR_BUFFER_BIT);
            self.gl.clear_color(0.0, 0.0, 0.0, 1.0);

            self.gl.bind_framebuffer(glow::READ_FRAMEBUFFER, source_fbo);
            self.gl
                .bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(dst_fbo));
            self.gl.blit_framebuffer(
                plan.src.0,
                plan.src.1,
                plan.src.2,
                plan.src.3,
                plan.dst.0,
                plan.dst.1,
                plan.dst.2,
                plan.dst.3,
                glow::COLOR_BUFFER_BIT,
                glow::NEAREST,
            );

            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.gl
                .viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
            if scissor_enabled {
                self.gl.enable(glow::SCISSOR_TEST);
            }
        }
        true
    }

    fn retire_transition_targets(&mut self) {
        if let Some((fbo, texture)) = self.transition_fbo.take() {
            unsafe {
                self.gl.delete_framebuffer(fbo);
                self.gl.delete_texture(texture);
            }
        }
        if let Some((fbo, texture)) = self.transition_new_fbo.take() {
            unsafe {
                self.gl.delete_framebuffer(fbo);
                self.gl.delete_texture(texture);
            }
        }
    }

    /// Delete the persistent last-presented target and invalidate its metadata.
    /// Used on output resize and compositor teardown.
    pub(super) fn retire_presented_scene_snapshot(&mut self) {
        if let Some((fbo, texture)) = self.presented_scene_fbo.take() {
            unsafe {
                self.gl.delete_framebuffer(fbo);
                self.gl.delete_texture(texture);
            }
        }
        self.presented_scene_status.reset();
    }

    /// Synchronize the final default framebuffer into a stable compositor
    /// texture. The first/invalid frame copies the complete repaired output;
    /// subsequent partial-damage frames only copy their repair rectangle.
    ///
    /// The caller only commits validity after the following buffer swap
    /// succeeds. A failed swap must invalidate the overwritten candidate.
    fn capture_presented_scene_candidate(&mut self, repair_scissor: Option<GlScissor>) -> bool {
        if self.transition_mode == TransitionMode::None {
            self.retire_presented_scene_snapshot();
            return false;
        }
        let Some((width, height)) = full_output_copy_extent(self.screen_w, self.screen_h) else {
            self.presented_scene_status.invalidate();
            return false;
        };

        let snapshot_usable = self.presented_scene_fbo.is_some()
            && self
                .presented_scene_status
                .is_usable(self.screen_w, self.screen_h);
        let copy_plan = presented_scene_copy_plan(
            true,
            snapshot_usable,
            repair_scissor,
            self.screen_w,
            self.screen_h,
        );
        if copy_plan == PresentedSceneCopyPlan::Disabled {
            self.retire_presented_scene_snapshot();
            return false;
        }

        if self.presented_scene_fbo.is_some()
            && !self
                .presented_scene_status
                .has_dimensions(self.screen_w, self.screen_h)
        {
            self.retire_presented_scene_snapshot();
        }

        if self.presented_scene_fbo.is_none() {
            // RGB10_A2 support does not change between frames. After one
            // allocation failure, use the no-transition fallback until the
            // effect is disabled or the output is resized instead of
            // allocating and logging at refresh rate.
            if self
                .presented_scene_status
                .allocation_failed_for(self.screen_w, self.screen_h)
            {
                return false;
            }
            match unsafe { Self::create_scene_fbo(&self.gl, self.screen_w, self.screen_h) } {
                Ok(target) => {
                    self.presented_scene_fbo = Some(target);
                    self.presented_scene_status
                        .record_allocation(self.screen_w, self.screen_h);
                }
                Err(error) => {
                    self.presented_scene_status
                        .record_allocation_failure(self.screen_w, self.screen_h);
                    log::warn!(
                        "{}: {error}",
                        self.renderer_ctx("presented-scene: allocate snapshot FBO")
                    );
                    return false;
                }
            }
        }

        let Some((dst_fbo, _)) = self.presented_scene_fbo else {
            self.presented_scene_status.invalidate();
            return false;
        };
        let copy_rect = match copy_plan {
            PresentedSceneCopyPlan::Disabled => unreachable!(),
            PresentedSceneCopyPlan::Full => (0, 0, width, height),
            PresentedSceneCopyPlan::Region(region) => region,
        };

        unsafe {
            let scissor_enabled = self.gl.is_enabled(glow::SCISSOR_TEST);
            if scissor_enabled {
                self.gl.disable(glow::SCISSOR_TEST);
            }
            self.gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
            self.gl
                .bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(dst_fbo));
            self.gl.blit_framebuffer(
                copy_rect.0,
                copy_rect.1,
                copy_rect.0 + copy_rect.2,
                copy_rect.1 + copy_rect.3,
                copy_rect.0,
                copy_rect.1,
                copy_rect.0 + copy_rect.2,
                copy_rect.1 + copy_rect.3,
                glow::COLOR_BUFFER_BIT,
                glow::NEAREST,
            );
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.gl.viewport(0, 0, width, height);
            if scissor_enabled {
                self.gl.enable(glow::SCISSOR_TEST);
            }
        }
        true
    }

    pub(crate) fn force_full_redraw(&mut self) {
        self.damage_tracker.mark_all_dirty();
        self.dirty_region_tracker.mark_all_dirty();
        self.needs_render = true;
    }

    pub(crate) fn ensure_scene_windows_tracked(
        &mut self,
        scene: &[(u32, i32, i32, u32, u32)],
        root: u32,
        original_scene_len: usize,
    ) {
        let backend_label = self.conn.backend_name();
        if original_scene_len != 0 && scene.is_empty() {
            log::warn!(
                "[{backend_label} compositor] scene has {original_scene_len} entries but x11_scene is empty (ID lookup failed)"
            );
        }

        for &(x11w, x, y, w, h) in scene {
            if !self.has_window(x11w) && x11w != root {
                log::info!(
                    "[{backend_label} compositor] lazily adding untracked window 0x{:x} {}x{} at ({},{})",
                    x11w,
                    w,
                    h,
                    x,
                    y
                );
                self.add_window(x11w, x, y, w, h);
            }
        }
    }

    // =====================================================================
    // Feature 11: Debug HUD toggle
    // =====================================================================
    pub(crate) fn set_transition_mode(&mut self, mode: &str) {
        let mode = TransitionMode::from_name_or_none(mode);
        if self.transition_mode != mode {
            self.transition_mode = mode;
            if mode == TransitionMode::None {
                self.transition_start = None;
            }
            if mode == TransitionMode::None
                && (self.presented_scene_fbo.is_some()
                    || self.transition_fbo.is_some()
                    || self.transition_new_fbo.is_some())
            {
                if !self.context_current {
                    match self.graphics.make_current() {
                        Ok(()) => self.context_current = true,
                        Err(error) => log::warn!(
                            "{}: deferring snapshot cleanup: {error}",
                            self.renderer_ctx("transition cleanup: make context current")
                        ),
                    }
                }
                if self.context_current {
                    self.retire_presented_scene_snapshot();
                    self.retire_transition_targets();
                } else {
                    self.presented_scene_status.invalidate();
                }
            }
            self.needs_render = true;
        }
    }

    pub(crate) fn set_debug_hud(&mut self, enabled: bool) {
        if self.debug_hud != enabled {
            // Toggling forgets the card's geometry, so showing it again
            // springs it out of the bar rather than resuming mid-open.
            self.hud_island.close();
        }
        self.debug_hud = enabled;
        self.needs_render = true;
    }

    pub(crate) fn set_debug_hud_extended(&mut self, enabled: bool) {
        self.debug_hud_extended = enabled;
        self.frame_profiler.set_enabled(enabled);
        self.needs_render = true;
    }

    #[allow(dead_code)]
    pub(crate) fn debug_hud_enabled(&self) -> bool {
        self.debug_hud
    }

    pub(crate) fn frame_stats_fps(&self) -> f32 {
        self.frame_stats.fps
    }

    pub(crate) fn get_metrics(&self) -> crate::backend::api::CompositorMetrics {
        let frame_times_vec: Vec<f32> = self.frame_stats.frame_times.iter().copied().collect();
        let avg_frame_time = if frame_times_vec.is_empty() {
            0.0
        } else {
            frame_times_vec.iter().sum::<f32>() / frame_times_vec.len() as f32
        };
        let max_frame_time = frame_times_vec.iter().copied().fold(0.0, f32::max);
        let min_frame_time = frame_times_vec.iter().copied().fold(f32::MAX, f32::min);
        let min_frame_time = if min_frame_time == f32::MAX {
            0.0
        } else {
            min_frame_time
        };

        let blur_hit_rate =
            if self.frame_stats.blur_cache_hits + self.frame_stats.blur_cache_misses > 0 {
                100.0 * self.frame_stats.blur_cache_hits as f32
                    / (self.frame_stats.blur_cache_hits + self.frame_stats.blur_cache_misses) as f32
            } else {
                0.0
            };

        let temporal_blur_reuse_rate = if self.temporal_blur_total_count > 0 {
            100.0 * self.temporal_blur_reuse_count as f32 / self.temporal_blur_total_count as f32
        } else {
            0.0
        };

        let dirty_tiles_count = self.damage_tracker.dirty_tile_count();
        let dirty_fraction = self.damage_tracker.dirty_fraction();

        let latency_stats = self.compute_latency_stats();

        crate::backend::api::CompositorMetrics {
            renderer_api: self.graphics.api_name().to_string(),
            fps: self.frame_stats.fps,
            frame_count: self.frame_stats.frame_count,
            avg_frame_time_ms: avg_frame_time,
            max_frame_time_ms: max_frame_time,
            min_frame_time_ms: min_frame_time,
            frame_time_p95_ms: 0.0,
            frame_time_p99_ms: 0.0,
            gpu_load_percent: 0, // To be updated from perf_metrics
            cpu_load_percent: 0, // To be updated from perf_metrics
            draw_calls: self.frame_stats.draw_calls,
            texture_memory_bytes: self.frame_stats.texture_memory_bytes,
            blur_cache_hits: self.frame_stats.blur_cache_hits,
            blur_cache_misses: self.frame_stats.blur_cache_misses,
            blur_cache_hit_rate: blur_hit_rate,
            temporal_blur_reuse_count: self.temporal_blur_reuse_count,
            temporal_blur_total_count: self.temporal_blur_total_count,
            temporal_blur_reuse_rate,
            dirty_regions_count: dirty_tiles_count,
            dirty_fraction_percent: dirty_fraction * 100.0,
            window_count: self.windows.len(),
            blur_quality: format!("{:?}", self.blur_quality),
            vrr_enabled: self.vrr_active,
            vrr_active: self.vrr_active,
            current_refresh_rate: self.get_vrr_refresh_rate(),
            input_latency_avg_ms: latency_stats.0,
            input_latency_p50_ms: latency_stats.1,
            input_latency_p95_ms: latency_stats.2,
            input_latency_p99_ms: latency_stats.3,
            // Phase 2-3: Optimization statistics
            direct_scanout_active: self.direct_scanout_mgr.is_active(),
            direct_scanout_count: self.direct_scanout_mgr.stats().scanout_count,
            direct_scanout_bypass_time_ms: self.direct_scanout_mgr.stats().total_bypass_time_ms,
            gl_state_changes_avoided: self.gl_state_tracker.redundant_changes_avoided(),
            profiling_enabled: self.frame_profiler.is_enabled(),
            dirty_region_merge_count: self.dirty_region_tracker.region_count(),
        }
    }

    /// Rasterize the four HUD text sections — title, state chip, stat labels,
    /// stat values — each in its own tone. Skips the upload entirely when
    /// nothing in the HUD changed since the previous frame.
    pub(super) fn update_hud_textures(&mut self, title: &str, chip: &str, rows: &hud::HudRows) {
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
            unsafe {
                if let Some((old, _, _)) = self.hud_textures[slot].take() {
                    self.gl.delete_texture(old);
                }
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
            unsafe {
                if let Ok(tex) = self.gl.create_texture() {
                    self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                    self.gl.tex_image_2d(
                        glow::TEXTURE_2D,
                        0,
                        glow::RGBA8 as i32,
                        w as i32,
                        h as i32,
                        0,
                        glow::RGBA,
                        glow::UNSIGNED_BYTE,
                        glow::PixelUnpackData::Slice(Some(&pixels)),
                    );
                    for filter in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
                        self.gl
                            .tex_parameter_i32(glow::TEXTURE_2D, filter, glow::LINEAR as i32);
                    }
                    for wrap in [glow::TEXTURE_WRAP_S, glow::TEXTURE_WRAP_T] {
                        self.gl.tex_parameter_i32(
                            glow::TEXTURE_2D,
                            wrap,
                            glow::CLAMP_TO_EDGE as i32,
                        );
                    }
                    self.gl.bind_texture(glow::TEXTURE_2D, None);
                    self.hud_textures[slot] = Some((tex, w, h));
                }
            }
        }
        self.hud_text_cache = cache_key;
    }

    /// Draw the HUD card: shadow, surface, state chip, frame-rate meter, and
    /// the two-tone stat columns, in the active theme's tones.
    fn render_debug_hud_card(&mut self, proj: &[f32; 16], meter: f32, tone: [f32; 4]) {
        let ui = ui_theme::palette();
        self.ensure_glass_backdrop(ui);
        let dims = |slot: usize| -> (f32, f32) {
            self.hud_textures[slot]
                .map(|(_, w, h)| (w as f32, h as f32))
                .unwrap_or((0.0, 0.0))
        };
        let dock = self.island_dock();
        let layout = hud::HudLayout::docked(ui, &dock, dims(0), dims(1), dims(2), dims(3), meter);
        let motion_enabled = crate::config::CONFIG.load().motion_enabled();
        let (card_w, card_h) = self.hud_island.advance_with_motion(
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
        let [cx, cy, ..] = dock.rect(card_w, card_h, 0.0);
        let (radius_top, radius) = dock.radii(card_h, ui.card_radius, 0.0);
        let opened = (card_w / layout.card.2.max(1.0)).clamp(0.0, 1.0);
        let content_a = opened * opened;

        unsafe {
            self.gl.bind_vertex_array(Some(self.quad_vao));

            // No ambient shadow: the card's top edge is flush with the bar, and
            // a shadow spreading up over it is the seam the dock removes.
            // Card surface, then chip and meter on the rounded-fill path.
            let (cw, ch) = (card_w, card_h);
            self.ui_fill_island(proj, ui, cx, cy, cw, ch, radius, radius_top, ui.card, 1.0);
            if layout.chip_pill.2 > 0.0 {
                let (px, py, pw, ph) = layout.chip_pill;
                self.sysui_fill_rounded(
                    px,
                    py,
                    pw,
                    ph,
                    ui.chip_radius,
                    UiPalette::faded(ui.chip, content_a),
                );
            }
            let (tx, ty, tw, th) = layout.meter_track;
            self.sysui_fill_rounded(
                tx,
                ty,
                tw,
                th,
                th * 0.5,
                UiPalette::faded(ui.track, content_a),
            );
            let (fx, fy, fw, fh) = layout.meter_fill;
            self.sysui_fill_rounded(fx, fy, fw, fh, fh * 0.5, UiPalette::faded(tone, content_a));

            // The ring is a circular rounded rect, so a theme whose surfaces are
            // squircles asks for none of it and relies on the shader's own rim.
            if ui.ring_alpha > 0.0 {
                // Hairline accent ring, matching the focused window's gradient.
                self.gl.use_program(Some(self.gradient_border_program));
                self.gl.uniform_matrix_4_f32_slice(
                    self.gradient_border_uniforms.projection.as_ref(),
                    false,
                    proj,
                );
                let ring = ui.ring_width;
                let [ar, ag, ab, aa] = self.border_gradient_color_a;
                let [br, bg, bb, ba] = self.border_gradient_color_b;
                self.gl
                    .uniform_1_f32(self.gradient_border_uniforms.border_width.as_ref(), ring);
                self.gl.uniform_4_f32(
                    self.gradient_border_uniforms.color_a.as_ref(),
                    ar,
                    ag,
                    ab,
                    aa * ui.ring_alpha,
                );
                self.gl.uniform_4_f32(
                    self.gradient_border_uniforms.color_b.as_ref(),
                    br,
                    bg,
                    bb,
                    ba * ui.ring_alpha,
                );
                self.gl.uniform_1_f32(
                    self.gradient_border_uniforms.gradient_angle.as_ref(),
                    self.border_gradient_angle.to_radians(),
                );
                let ring_top = if radius_top > 0.0 {
                    radius_top + ring
                } else {
                    0.0
                };
                self.set_gradient_border_radii(radius + ring, ring_top);
                self.gl.uniform_2_f32(
                    self.gradient_border_uniforms.size.as_ref(),
                    cw + 2.0 * ring,
                    ch + 2.0 * ring,
                );
                self.gl.uniform_4_f32(
                    self.gradient_border_uniforms.rect.as_ref(),
                    cx - ring,
                    cy - ring,
                    cw + 2.0 * ring,
                    ch + 2.0 * ring,
                );
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            }

            // Text sections.
            self.gl.use_program(Some(self.hud_text_program));
            self.gl.uniform_matrix_4_f32_slice(
                self.hud_text_uniforms.projection.as_ref(),
                false,
                proj,
            );
            self.gl
                .uniform_1_i32(self.hud_text_uniforms.texture.as_ref(), 0);
            self.gl
                .uniform_1_f32(self.hud_text_uniforms.opacity.as_ref(), content_a);
            self.gl.active_texture(glow::TEXTURE0);
            let positions = [layout.title, layout.chip_text, layout.labels, layout.values];
            for (slot, (px, py)) in positions.into_iter().enumerate() {
                let Some((tex, w, h)) = self.hud_textures[slot] else {
                    continue;
                };
                self.gl.uniform_4_f32(
                    self.hud_text_uniforms.rect.as_ref(),
                    px,
                    py,
                    w as f32,
                    h as f32,
                );
                self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            }

            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
        }
    }

    /// Rasterize the four text sections of the system-UI panel (title, query
    /// line, list items, footer hint), each with its own tone so the styled
    /// card reads with clear hierarchy.
    fn update_system_ui_textures(&mut self, overlay: &crate::backend::api::SystemUiOverlay) {
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
            unsafe {
                if let Some((old, _, _)) = self.sysui_textures[slot].take() {
                    self.gl.delete_texture(old);
                }
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
                if let Ok(tex) = self.gl.create_texture() {
                    self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                    self.gl.tex_image_2d(
                        glow::TEXTURE_2D,
                        0,
                        glow::RGBA8 as i32,
                        w as i32,
                        h as i32,
                        0,
                        glow::RGBA,
                        glow::UNSIGNED_BYTE,
                        glow::PixelUnpackData::Slice(Some(&pixels)),
                    );
                    for filter in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
                        self.gl
                            .tex_parameter_i32(glow::TEXTURE_2D, filter, glow::LINEAR as i32);
                    }
                    self.gl.bind_texture(glow::TEXTURE_2D, None);
                    self.sysui_textures[slot] = Some((tex, w, h));
                }
            }
        }
        self.sysui_text_dirty = false;
    }

    /// Capture a blurred copy of the frame for the frosted-glass panels to
    /// sample.
    ///
    /// Runs against the default framebuffer, i.e. the fully composited scene
    /// including post-processing, so the glass shows the desktop as the user
    /// sees it. A missing blur chain (no GL memory, or a driver that refused
    /// the FBOs) leaves the backdrop unset and the panels fall back to flat
    /// translucent fills.
    fn capture_glass_backdrop(&mut self, palette: &UiPalette) {
        let Some(glass) = palette.glass else {
            self.glass_backdrop = None;
            return;
        };
        if self.blur_fbos.is_empty() || self.scene_fbo.is_none() {
            self.glass_backdrop = None;
            return;
        }
        let levels = (glass.blur_levels as usize).clamp(1, self.blur_fbos.len());
        // A partial-redraw frame leaves the repair scissor armed, and both the
        // capture blit and the filter passes obey it. Clipping them would feed
        // the card stale texels wherever its blur kernel reaches outside the
        // repair region, so the whole screen is captured either way.
        let scissor = unsafe { self.gl.is_enabled(glow::SCISSOR_TEST) };
        if scissor {
            unsafe { self.gl.disable(glow::SCISSOR_TEST) };
        }
        self.glass_backdrop = self.run_blur_passes_from_fbo(None, levels);
        if scissor {
            unsafe { self.gl.enable(glow::SCISSOR_TEST) };
        }
    }

    /// Capture the backdrop unless this frame already has one. Panels drawn
    /// back to back share a single capture: re-blurring the whole screen per
    /// card would cost more than the parallax it buys, and the only thing the
    /// later cards miss is the earlier cards themselves.
    pub(super) fn ensure_glass_backdrop(&mut self, palette: &UiPalette) {
        if self.glass_backdrop.is_none() {
            self.capture_glass_backdrop(palette);
        }
    }

    /// Draw one frosted-glass surface. Binds its own program, so callers that
    /// follow up with flat fills must re-bind the border program afterwards.
    ///
    /// `tint` is the palette's surface entry (RGB veil + coverage) and `alpha`
    /// the caller's fade envelope.
    #[allow(clippy::too_many_arguments)]
    unsafe fn glass_fill_rounded(
        &self,
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
            self.gl.use_program(Some(self.glass_program));
            self.gl
                .uniform_matrix_4_f32_slice(u.projection.as_ref(), false, proj);
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(backdrop));
            self.gl.uniform_1_i32(u.backdrop.as_ref(), 0);
            self.gl.uniform_2_f32(
                u.screen_size.as_ref(),
                self.screen_w as f32,
                self.screen_h as f32,
            );
            self.gl
                .uniform_4_f32(u.tint.as_ref(), tint[0], tint[1], tint[2], tint[3]);
            self.gl.uniform_2_f32(u.size.as_ref(), w, h);
            self.gl.uniform_1_f32(u.radius.as_ref(), r);
            self.gl.uniform_1_f32(u.radius_top.as_ref(), r_top);
            self.gl
                .uniform_1_f32(u.corner_exp.as_ref(), params.corner_exponent);
            self.gl
                .uniform_1_f32(u.saturation.as_ref(), params.saturation);
            self.gl
                .uniform_1_f32(u.luminance.as_ref(), params.luminance);
            self.gl
                .uniform_1_f32(u.bevel_width.as_ref(), params.bevel_width);
            self.gl
                .uniform_1_f32(u.refraction.as_ref(), params.refraction);
            self.gl
                .uniform_1_f32(u.rim_width.as_ref(), params.rim_width);
            self.gl
                .uniform_1_f32(u.rim_intensity.as_ref(), params.rim_intensity);
            self.gl.uniform_3_f32(
                u.rim_tint.as_ref(),
                params.rim_tint[0],
                params.rim_tint[1],
                params.rim_tint[2],
            );
            self.gl.uniform_1_f32(u.sheen.as_ref(), params.sheen);
            self.gl
                .uniform_1_f32(u.edge_shade.as_ref(), params.edge_shade);
            self.gl.uniform_1_f32(u.grain.as_ref(), params.grain);
            self.gl
                .uniform_1_f32(u.alpha.as_ref(), alpha.clamp(0.0, 1.0));
            self.gl.uniform_4_f32(u.rect.as_ref(), x, y, w, h);
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
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
        unsafe { self.ui_fill_island(proj, palette, x, y, w, h, r, r, surface, alpha) }
    }

    /// As [`Self::ui_fill_surface`], but the top two corners take their own
    /// radius. A docked panel passes zero so it merges with the bar above it.
    #[allow(clippy::too_many_arguments)]
    pub(super) unsafe fn ui_fill_island(
        &self,
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
                    self.glass_fill_rounded(proj, x, y, w, h, r, r_top, surface, alpha, &params);
                    true
                }
                _ => false,
            };
            self.gl.use_program(Some(self.border_program));
            self.gl.uniform_matrix_4_f32_slice(
                self.border_uniforms.projection.as_ref(),
                false,
                proj,
            );
            if !drew_glass {
                self.sysui_fill_island(x, y, w, h, r, r_top, UiPalette::faded(surface, alpha));
            }
        }
    }

    /// Where the compositor's own panels dock: under the status bar.
    ///
    /// The bar is found by class among the tracked windows rather than tracked
    /// separately, because that is already how every other feature identifies
    /// it. A bar that is hidden, unmapped, or on another output leaves the
    /// panels hanging from the top of the screen instead.
    fn island_dock(&self) -> IslandDock {
        self.island_dock_in([0.0, 0.0, self.screen_w as f32, self.screen_h as f32])
    }

    fn island_dock_in(&self, viewport: [f32; 4]) -> IslandDock {
        let cfg = crate::config::CONFIG.load();
        let bar_name = cfg.status_bar_name();
        let bar = self
            .windows
            .values()
            .filter(|wt| {
                !bar_name.is_empty()
                    && (wt.class_name == bar_name || wt.class_name.contains(bar_name))
                    && wt.w > 0
                    && wt.h > 0
            })
            .filter_map(|wt| {
                clip_bar_to_viewport(
                    [wt.x as f32, wt.y as f32, wt.w as f32, wt.h as f32],
                    viewport,
                )
            })
            // Only bars intersecting this output remain. Prefer its topmost
            // segment if a client exposed more than one dock-like surface.
            .min_by(|left, right| left[1].total_cmp(&right[1]));
        IslandDock::for_bar(bar, viewport)
    }

    /// Set the gradient-ring program's corner radii, the top two separately,
    /// so a ring around a docked panel follows its squared top corners.
    pub(super) fn set_gradient_border_radii(&self, bottom: f32, top: f32) {
        unsafe {
            self.gl
                .uniform_1_f32(self.gradient_border_uniforms.radius.as_ref(), bottom);
            self.gl
                .uniform_1_f32(self.gradient_border_uniforms.radius_top.as_ref(), top);
        }
    }

    /// Set the border program's corner radii, the top two separately.
    ///
    /// Every write goes through here. An unwritten uniform location reads back
    /// as zero, so a call site that set only `u_radius` would silently square
    /// the top corners of whatever it drew — window borders included.
    pub(super) fn set_border_radii(&self, bottom: f32, top: f32) {
        unsafe {
            self.gl
                .uniform_1_f32(self.border_uniforms.radius.as_ref(), bottom);
            self.gl
                .uniform_1_f32(self.border_uniforms.radius_top.as_ref(), top);
        }
    }

    /// Filled rounded rectangle through the border program (a border wider
    /// than the rect fills it). The program and projection must be bound.
    pub(super) unsafe fn sysui_fill_rounded(
        &self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        r: f32,
        color: [f32; 4],
    ) {
        unsafe { self.sysui_fill_island(x, y, w, h, r, r, color) }
    }

    /// As [`Self::sysui_fill_rounded`], but the top two corners take their own
    /// radius so a docked panel meets the bar with a straight edge.
    #[allow(clippy::too_many_arguments)]
    unsafe fn sysui_fill_island(
        &self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        r_bottom: f32,
        r_top: f32,
        color: [f32; 4],
    ) {
        unsafe {
            self.gl
                .uniform_1_f32(self.border_uniforms.border_width.as_ref(), w.max(h));
            self.gl.uniform_4_f32(
                self.border_uniforms.border_color.as_ref(),
                color[0],
                color[1],
                color[2],
                color[3],
            );
            self.set_border_radii(r_bottom, r_top);
            self.gl
                .uniform_2_f32(self.border_uniforms.size.as_ref(), w, h);
            self.gl
                .uniform_4_f32(self.border_uniforms.rect.as_ref(), x, y, w, h);
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        }
    }

    /// Rounded outline through the border program: the line-drawn boxes the
    /// layout thumbnails are made of. The program and projection must be
    /// bound.
    #[allow(clippy::too_many_arguments)]
    unsafe fn sysui_stroke_rounded(
        &self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        r: f32,
        width: f32,
        color: [f32; 4],
    ) {
        unsafe {
            self.gl
                .uniform_1_f32(self.border_uniforms.border_width.as_ref(), width);
            self.gl.uniform_4_f32(
                self.border_uniforms.border_color.as_ref(),
                color[0],
                color[1],
                color[2],
                color[3],
            );
            self.set_border_radii(r, r);
            self.gl
                .uniform_2_f32(self.border_uniforms.size.as_ref(), w, h);
            self.gl
                .uniform_4_f32(self.border_uniforms.rect.as_ref(), x, y, w, h);
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        }
    }

    /// The layout picker: a strip of 35mm film across the panel, one cell per
    /// layout, each holding a line-drawn thumbnail of what that layout does
    /// with a screenful of windows. The selected cell lifts out of the strip
    /// and the countdown under it shows how long until it commits itself.
    fn render_layout_filmstrip(
        &mut self,
        proj: &[f32; 16],
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
            self.gl.bind_vertex_array(Some(self.quad_vao));

            // Scrim: dim the desktop the strip is describing.
            self.gl.use_program(Some(self.hud_program));
            self.gl
                .uniform_matrix_4_f32_slice(self.hud_uniforms.projection.as_ref(), false, proj);
            self.gl.uniform_4_f32(
                self.hud_uniforms.bg_color.as_ref(),
                ui.scrim[0],
                ui.scrim[1],
                ui.scrim[2],
                ui.scrim[3],
            );
            self.gl
                .uniform_2_f32(self.hud_uniforms.size.as_ref(), viewport_w, viewport_h);
            self.gl.uniform_4_f32(
                self.hud_uniforms.rect.as_ref(),
                viewport_x,
                viewport_y,
                viewport_w,
                viewport_h,
            );
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        }

        self.capture_glass_backdrop(ui);

        unsafe {
            // Drop shadow, then the card.
            self.gl.use_program(Some(self.shadow_program));
            self.gl.uniform_matrix_4_f32_slice(
                self.shadow_uniforms.projection.as_ref(),
                false,
                proj,
            );
            let spread = ui.spread(48.0);
            self.gl
                .uniform_1_f32(self.shadow_uniforms.spread.as_ref(), spread);
            self.gl.uniform_4_f32(
                self.shadow_uniforms.shadow_color.as_ref(),
                ui.shadow[0],
                ui.shadow[1],
                ui.shadow[2],
                ui.shadow[3],
            );
            self.gl
                .uniform_1_f32(self.shadow_uniforms.radius.as_ref(), film::PANEL_RADIUS);
            self.gl
                .uniform_2_f32(self.shadow_uniforms.size.as_ref(), panel_w, panel_h);
            self.gl.uniform_4_f32(
                self.shadow_uniforms.rect.as_ref(),
                panel_x - spread,
                panel_y - spread + 14.0,
                panel_w + 2.0 * spread,
                panel_h + 2.0 * spread,
            );
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

            self.ui_fill_surface(
                proj,
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
            self.sysui_fill_rounded(sx - 4.0, sy, sw + 8.0, sh, 4.0, base);

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

                // The exposed frame: the selected one is lit, the rest sit
                // back in the emulsion.
                self.sysui_fill_rounded(
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
                self.sysui_fill_rounded(*hx, *hy, *hw, *hh, hh * 0.4, hole);
            }

            // Countdown to the automatic commit.
            let [cx, cy, cw, ch] = geometry.countdown;
            self.sysui_fill_rounded(cx, cy, cw, ch, ch * 0.5, UiPalette::faded(ui.track, 0.8));
            let filled = cw * strip.countdown.clamp(0.0, 1.0);
            if filled > 1.0 {
                self.sysui_fill_rounded(
                    cx,
                    cy,
                    filled,
                    ch,
                    ch * 0.5,
                    [accent[0], accent[1], accent[2], 0.9],
                );
            }
        }

        // Title, the selected layout's name centred under the strip, and the
        // footer hint.
        let title = geometry.title;
        let caption = geometry.caption_center;
        let hint = geometry.hint;
        unsafe {
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
            for (slot, pos) in [(0usize, Some(title)), (2, None), (3, Some(hint))] {
                let Some((tex, w, h)) = self.sysui_textures[slot] else {
                    continue;
                };
                let (tx, ty) = match pos {
                    Some([x, y]) => (x, y),
                    // The caption is centred on the strip rather than aligned
                    // to the panel's text column.
                    None => (caption[0] - w as f32 * 0.5, caption[1] - h as f32 * 0.5),
                };
                self.gl.uniform_4_f32(
                    self.hud_text_uniforms.rect.as_ref(),
                    tx,
                    ty,
                    w as f32,
                    h as f32,
                );
                self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            }
            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
        }
    }

    /// Modal system UI drawn as a material-style card: dimmed scrim, drop
    /// shadow, rounded panel with a gradient accent ring, a search-field bar,
    /// and a selection pill under the highlighted list row.
    fn render_system_ui(&mut self, proj: &[f32; 16]) {
        let Some(overlay) = self.system_ui.clone() else {
            return;
        };
        self.system_ui_hit_geometry = None;
        self.update_system_ui_textures(&overlay);
        let viewport = overlay.effective_viewport(self.screen_w as i32, self.screen_h as i32);
        if let Some(strip) = &overlay.filmstrip {
            self.render_layout_filmstrip(proj, strip, viewport);
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
            self.gl.bind_vertex_array(Some(self.quad_vao));

            if overlay.locked {
                self.gl.clear_color(
                    ui.lock_backdrop[0],
                    ui.lock_backdrop[1],
                    ui.lock_backdrop[2],
                    ui.lock_backdrop[3],
                );
                self.gl.clear(glow::COLOR_BUFFER_BIT);
            } else {
                // Scrim: dim the desktop behind the panel.
                self.gl.use_program(Some(self.hud_program));
                self.gl.uniform_matrix_4_f32_slice(
                    self.hud_uniforms.projection.as_ref(),
                    false,
                    proj,
                );
                self.gl.uniform_4_f32(
                    self.hud_uniforms.bg_color.as_ref(),
                    ui.scrim[0],
                    ui.scrim[1],
                    ui.scrim[2],
                    ui.scrim[3],
                );
                self.gl
                    .uniform_2_f32(self.hud_uniforms.size.as_ref(), viewport_w, viewport_h);
                self.gl.uniform_4_f32(
                    self.hud_uniforms.rect.as_ref(),
                    viewport_x,
                    viewport_y,
                    viewport_w,
                    viewport_h,
                );
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            }
        }

        // The scrim is part of what the panel covers, so the backdrop is taken
        // after it — otherwise the glass would show an undimmed desktop inside
        // a dimmed one. Forced rather than lazy for the same reason.
        if !overlay.locked {
            self.capture_glass_backdrop(ui);
        }

        unsafe {
            self.gl.bind_vertex_array(Some(self.quad_vao));
            // Drop shadow behind the card — the lock card only. A docked
            // panel's top edge is flush with the bar, and a shadow spreading up
            // over it is exactly the seam the dock removes.
            if overlay.locked {
                self.gl.use_program(Some(self.shadow_program));
                self.gl.uniform_matrix_4_f32_slice(
                    self.shadow_uniforms.projection.as_ref(),
                    false,
                    proj,
                );
                let spread = ui.spread(48.0);
                self.gl
                    .uniform_1_f32(self.shadow_uniforms.spread.as_ref(), spread);
                self.gl.uniform_4_f32(
                    self.shadow_uniforms.shadow_color.as_ref(),
                    ui.shadow[0],
                    ui.shadow[1],
                    ui.shadow[2],
                    ui.shadow[3],
                );
                self.gl
                    .uniform_1_f32(self.shadow_uniforms.radius.as_ref(), radius);
                self.gl
                    .uniform_2_f32(self.shadow_uniforms.size.as_ref(), panel_w, panel_h);
                self.gl.uniform_4_f32(
                    self.shadow_uniforms.rect.as_ref(),
                    x - spread,
                    y - spread + 14.0,
                    panel_w + 2.0 * spread,
                    panel_h + 2.0 * spread,
                );
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            }

            // Card surface, then the query-field bar and selection pill on the
            // border program's rounded-fill mode.
            self.ui_fill_island(
                proj, ui, x, y, panel_w, panel_h, radius, radius_top, panel_fill, 1.0,
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
                    tx,
                    ty,
                    tw,
                    th,
                    panel::SCROLLBAR_RADIUS,
                    UiPalette::faded(ui.track, content_a),
                );
                self.sysui_fill_rounded(
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
                self.gl.use_program(Some(self.gradient_border_program));
                self.gl.uniform_matrix_4_f32_slice(
                    self.gradient_border_uniforms.projection.as_ref(),
                    false,
                    proj,
                );
                let ring = 1.5 * ui.ring_width;
                let [ar, ag, ab, aa] = self.border_gradient_color_a;
                let [br, bg, bb, ba] = self.border_gradient_color_b;
                self.gl
                    .uniform_1_f32(self.gradient_border_uniforms.border_width.as_ref(), ring);
                self.gl.uniform_4_f32(
                    self.gradient_border_uniforms.color_a.as_ref(),
                    ar,
                    ag,
                    ab,
                    aa * ui.panel_ring_alpha,
                );
                self.gl.uniform_4_f32(
                    self.gradient_border_uniforms.color_b.as_ref(),
                    br,
                    bg,
                    bb,
                    ba * ui.panel_ring_alpha,
                );
                self.gl.uniform_1_f32(
                    self.gradient_border_uniforms.gradient_angle.as_ref(),
                    self.border_gradient_angle.to_radians(),
                );
                // The ring follows the card: square across the top where the
                // card meets the bar, curved everywhere the card curves.
                let ring_top = if radius_top > 0.0 {
                    radius_top + ring
                } else {
                    0.0
                };
                self.set_gradient_border_radii(radius + ring, ring_top);
                self.gl.uniform_2_f32(
                    self.gradient_border_uniforms.size.as_ref(),
                    panel_w + 2.0 * ring,
                    panel_h + 2.0 * ring,
                );
                self.gl.uniform_4_f32(
                    self.gradient_border_uniforms.rect.as_ref(),
                    x - ring,
                    y - ring,
                    panel_w + 2.0 * ring,
                    panel_h + 2.0 * ring,
                );
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            }

            // Text sections.
            self.gl.use_program(Some(self.hud_text_program));
            self.gl.uniform_matrix_4_f32_slice(
                self.hud_text_uniforms.projection.as_ref(),
                false,
                proj,
            );
            self.gl
                .uniform_1_i32(self.hud_text_uniforms.texture.as_ref(), 0);
            self.gl
                .uniform_1_f32(self.hud_text_uniforms.opacity.as_ref(), content_a);
            self.gl.active_texture(glow::TEXTURE0);
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
                self.gl.uniform_4_f32(
                    self.hud_text_uniforms.rect.as_ref(),
                    tx,
                    ty,
                    w as f32,
                    h as f32,
                );
                self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            }
            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
        }
    }

    /// Rasterize (and cache) one toast's title/body textures.
    fn update_toast_textures(&mut self, id: u64, title: &str, body: &str) {
        if self.toast_textures.contains_key(&id) {
            return;
        }
        let config = crate::config::CONFIG.load();
        let description = config.system_ui_font();
        let size = crate::backend::compositor_font::ui_font_pixel_size(description);
        let ui = ui_theme::palette();
        // Title in the brightest ink, body one step down.
        let colors: [[u8; 4]; 2] = [ui.value_ink, ui.label_ink];
        let mut slots = [None, None];
        for (slot, text) in [title, body].into_iter().enumerate() {
            let text = crate::backend::compositor_font::fit_ui_text_lines(
                text,
                description,
                size,
                crate::backend::compositor_common::toast::MAX_TEXT_WIDTH_PX,
            );
            if text.is_empty() {
                continue;
            }
            let (pixels, w, h) = crate::backend::compositor_font::render_ui_text_to_rgba(
                &text,
                description,
                size,
                colors[slot],
            );
            if w == 0 || h == 0 {
                continue;
            }
            unsafe {
                if let Ok(tex) = self.gl.create_texture() {
                    self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                    self.gl.tex_image_2d(
                        glow::TEXTURE_2D,
                        0,
                        glow::RGBA8 as i32,
                        w as i32,
                        h as i32,
                        0,
                        glow::RGBA,
                        glow::UNSIGNED_BYTE,
                        glow::PixelUnpackData::Slice(Some(&pixels)),
                    );
                    for filter in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
                        self.gl
                            .tex_parameter_i32(glow::TEXTURE_2D, filter, glow::LINEAR as i32);
                    }
                    self.gl.bind_texture(glow::TEXTURE_2D, None);
                    slots[slot] = Some((tex, w, h));
                }
            }
        }
        self.toast_textures.insert(id, slots);
    }

    /// Transient notification cards stacked in the top-right corner: rounded
    /// card, drop shadow, urgency accent stripe, title over dimmer body, and
    /// a fade in/out envelope shared with the Wayland backend.
    fn render_toasts(&mut self, proj: &[f32; 16]) {
        let now = std::time::Instant::now();
        let removed = self.toast_stack.prune(now);
        self.free_toast_textures(&removed);
        if self.toast_stack.is_empty() {
            self.toast_rects.clear();
            return;
        }
        // Rebuilt below from the cards actually drawn this frame, so
        // hover/click hit-testing never sees stale geometry.
        self.toast_rects.clear();

        let toasts: Vec<(u64, String, String, u8, f32)> = self
            .toast_stack
            .iter()
            .map(|toast| {
                (
                    toast.id,
                    toast.notification.title.clone(),
                    toast.notification.body.clone(),
                    toast.notification.urgency,
                    toast.alpha(now),
                )
            })
            .collect();
        for (id, title, body, _, _) in &toasts {
            self.update_toast_textures(*id, title, body);
        }

        let ui = ui_theme::palette();
        self.ensure_glass_backdrop(ui);
        let motion_enabled = crate::config::CONFIG.load().motion_enabled();
        let pad = 18.0;
        let pad_left = 30.0;
        let gap = 12.0;
        let stripe_w = 3.0;

        // The stack hangs off the bar. An OSD owns the slot directly under it,
        // so reserve its full height rather than its current sprung height —
        // otherwise every toast below would jitter while the OSD opens.
        let dock = self.island_dock();
        let mut top = if self.osd_slot.get().is_some() {
            crate::backend::compositor_common::osd::OSD_CARD_HEIGHT + gap
        } else {
            0.0
        };

        unsafe {
            self.gl.bind_vertex_array(Some(self.quad_vao));
            for (id, _, _, urgency, alpha) in &toasts {
                let slots = self.toast_textures.get(id).copied().unwrap_or([None, None]);
                let (title_w, title_h) = slots[0]
                    .map(|(_, w, h)| (w as f32, h as f32))
                    .unwrap_or((0.0, 0.0));
                let (body_w, body_h) = slots[1]
                    .map(|(_, w, h)| (w as f32, h as f32))
                    .unwrap_or((0.0, 0.0));
                let content_w = title_w.max(body_w).clamp(
                    220.0,
                    crate::backend::compositor_common::toast::MAX_TEXT_WIDTH_PX as f32,
                );
                let target_w = content_w + pad_left + pad;
                let mut target_h = 2.0 * pad + title_h;
                if body_h > 0.0 {
                    target_h += 6.0 + body_h;
                }

                let (card_w, card_h) = self
                    .toast_stack
                    .motion_for(*id)
                    .map_or((target_w, target_h), |motion| {
                        motion.advance_with_motion(now, target_w, target_h, motion_enabled)
                    });
                let [x, y, ..] = dock.rect(card_w, card_h, top);
                self.toast_rects.push((*id, [x, y, card_w, card_h]));
                // Only the card actually touching the bar squares off; the
                // dock also refuses to square anything when there is no bar.
                let (radius_top, radius) = dock.radii(card_h, ui.toast_radius, top);
                let a = *alpha;
                let opened = (card_w / target_w.max(1.0)).clamp(0.0, 1.0);
                let content_a = a * opened * opened;
                let accent = match urgency {
                    2 => [0.95, 0.30, 0.30, 1.0],
                    0 => [0.45, 0.50, 0.62, 1.0],
                    _ => self.border_gradient_color_a,
                };

                self.ui_fill_island(
                    proj, ui, x, y, card_w, card_h, radius, radius_top, ui.toast, a,
                );
                if card_h > 26.0 {
                    self.sysui_fill_rounded(
                        x + 13.0,
                        y + 13.0,
                        stripe_w,
                        card_h - 26.0,
                        1.5,
                        [accent[0], accent[1], accent[2], 0.9 * content_a],
                    );
                }

                self.gl.use_program(Some(self.hud_text_program));
                self.gl.uniform_matrix_4_f32_slice(
                    self.hud_text_uniforms.projection.as_ref(),
                    false,
                    proj,
                );
                self.gl
                    .uniform_1_i32(self.hud_text_uniforms.texture.as_ref(), 0);
                self.gl
                    .uniform_1_f32(self.hud_text_uniforms.opacity.as_ref(), content_a);
                self.gl.active_texture(glow::TEXTURE0);
                if let Some((tex, w, h)) = slots[0] {
                    self.gl.uniform_4_f32(
                        self.hud_text_uniforms.rect.as_ref(),
                        x + pad_left,
                        y + pad,
                        w as f32,
                        h as f32,
                    );
                    self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                    self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                }
                if let Some((tex, w, h)) = slots[1] {
                    self.gl.uniform_4_f32(
                        self.hud_text_uniforms.rect.as_ref(),
                        x + pad_left,
                        y + pad + title_h + 6.0,
                        w as f32,
                        h as f32,
                    );
                    self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                    self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                }

                top += target_h + gap;
            }
            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
        }
    }

    /// Rasterize (and cache) the OSD label texture; re-render only when the
    /// text changed (key repeat updates the percent every event).
    fn update_osd_texture(&mut self, text: &str) {
        if self
            .osd_texture
            .as_ref()
            .is_some_and(|(cached, _, _, _)| cached == text)
        {
            return;
        }
        if let Some((_, tex, _, _)) = self.osd_texture.take() {
            unsafe { self.gl.delete_texture(tex) };
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
            if let Ok(tex) = self.gl.create_texture() {
                self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                self.gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA8 as i32,
                    w as i32,
                    h as i32,
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(Some(&pixels)),
                );
                for filter in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
                    self.gl
                        .tex_parameter_i32(glow::TEXTURE_2D, filter, glow::LINEAR as i32);
                }
                self.gl.bind_texture(glow::TEXTURE_2D, None);
                self.osd_texture = Some((text.to_string(), tex, w, h));
            }
        }
    }

    /// Volume/brightness OSD: one replace-in-place card docked under the status
    /// bar — icon+percent label on the left, progress bar on the right — with
    /// the hold+fade envelope shared with the Wayland backend.
    ///
    /// The card springs open out of the bar and morphs when its content changes
    /// size, so a volume slider replaced by a wider media card travels to the
    /// new width instead of being swapped for a second card. Its top corners
    /// are square: that is what makes it read as part of the bar rather than a
    /// rectangle parked underneath one.
    fn render_osd(&mut self, proj: &[f32; 16]) {
        let now = std::time::Instant::now();
        if self.osd_slot.prune(now) {
            if let Some((_, tex, _, _)) = self.osd_texture.take() {
                unsafe { self.gl.delete_texture(tex) };
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
        self.update_osd_texture(&text);
        let Some((tex, text_w, text_h)) =
            self.osd_texture.as_ref().map(|&(_, tex, w, h)| (tex, w, h))
        else {
            return;
        };

        let ui = ui_theme::palette();
        self.ensure_glass_backdrop(ui);
        let target_h = 64.0f32;
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
            self.gl.bind_vertex_array(Some(self.quad_vao));

            // No drop shadow: the top edge is flush with the bar, and a shadow
            // spreading up over it is exactly the seam the effect removes.
            self.ui_fill_island(
                proj, ui, x, y, card_w, card_h, radius, radius_top, ui.osd, a,
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
                    bar_x,
                    bar_y,
                    bar_w,
                    bar_h,
                    bar_h / 2.0,
                    UiPalette::faded(ui.slider_track, content_a),
                );
                if fill > 0.0 {
                    self.sysui_fill_rounded(
                        bar_x,
                        bar_y,
                        (bar_w * fill).max(bar_h),
                        bar_h,
                        bar_h / 2.0,
                        [accent[0], accent[1], accent[2], 0.95 * content_a],
                    );
                }
            }

            self.gl.use_program(Some(self.hud_text_program));
            self.gl.uniform_matrix_4_f32_slice(
                self.hud_text_uniforms.projection.as_ref(),
                false,
                proj,
            );
            self.gl
                .uniform_1_i32(self.hud_text_uniforms.texture.as_ref(), 0);
            self.gl
                .uniform_1_f32(self.hud_text_uniforms.opacity.as_ref(), content_a);
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.uniform_4_f32(
                self.hud_text_uniforms.rect.as_ref(),
                x + pad,
                y + (card_h - text_h as f32) / 2.0,
                text_w as f32,
                text_h as f32,
            );
            self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
        }
    }

    // =====================================================================
    // Feature 12: Screenshot
    // =====================================================================
    pub(crate) fn request_screenshot(&mut self, path: std::path::PathBuf) {
        self.screenshot_requests.request_full(path);
        self.needs_render = true;
    }

    pub(crate) fn request_screenshot_region(
        &mut self,
        path: std::path::PathBuf,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    ) {
        self.screenshot_requests.request_region(path, x, y, w, h);
        self.needs_render = true;
    }

    /// Check if there's a single fullscreen opaque window covering the screen.
    /// If so, and fullscreen_unredirect is enabled, we can skip compositing.
    fn scene_requires_composition(
        &self,
        scene: &[(u32, i32, i32, u32, u32)],
        focused: Option<u32>,
    ) -> bool {
        if self.needs_postprocess()
            || self.screenshot_requests.has_pending()
            || screenshot_freeze_requires_composition(
                self.screenshot_freeze_pending,
                self.screenshot_freeze_fbo.is_some(),
            )
            || self.system_ui.is_some()
            || self.debug_hud
            || self.recording_active
            || self.remote_capture_active
            || self.recording_region_overlay.is_some()
            || self.waterlily_visible()
            || self.waterlily_layer_dirty
            || self.transition_start.is_some()
            || self.overview_active
            || self.overview_closing
            || self.expose_active
            || !self.expose_entries.is_empty()
            || self.snap_target.is_some()
            || self.peek_active
            || self.annotation_active
            || !self.annotation_strokes.is_empty()
            || self.screenshot_toolbar.is_some()
            || self.edge_glow_active
            || self.zoom_to_fit_window.is_some()
            || !self.particle_systems.is_empty()
            || !self.genie_active.is_empty()
            // An explicit Dock request may promote a durable CPU thumbnail
            // once.  This is a one-frame render demand, not a persistent
            // drawable-source claim: the marker is consumed even on failure.
            || self.genie_targets.keys().any(|x11_win| {
                self.minimized_windows.contains(x11_win)
                    && self.minimized_gpu_upload_pending(*x11_win)
            })
            || self.dock_preview.is_some_and(|preview| {
                self.minimized_gpu_upload_pending(preview.x11_win)
            })
            || minimized_dock_requires_composition(
                self.genie_targets.keys().any(|x11_win| {
                    self.minimized_windows.contains(x11_win)
                        && self.minimized_preview_drawable_source_available(*x11_win)
                }),
                self.dock_preview.is_some_and(|preview| {
                    !preview.awaiting_source
                        && self.minimized_preview_drawable_source_available(preview.x11_win)
                }),
                // A due retained-source CPU recapture needs exactly one frame
                // past fullscreen-unredirect and the make-current barrier.
                self.iconic_snapshot_recapture_pending(),
            )
            || !self.ripple_active.is_empty()
            || self.tickless_focus_or_wallpaper_animation_active()
            || self.pending_wallpaper.is_some()
            || !self.pending_monitor_wallpapers.is_empty()
            || (self.window_tabs_enabled
                && self.window_groups.iter().any(|group| {
                    crate::backend::compositor_common::window_tabs::wants_bar(group.tabs.len())
                }))
        {
            return true;
        }

        // Tilt can react to the next pointer event even while its current
        // target is neutral. Keeping composition enabled avoids a one-frame
        // hole where a fullscreen client remains unredirected as tilt starts.
        if self.window_tilt {
            return true;
        }

        // A fullscreen top-level window completely occludes the clients below
        // it, so only the candidate at the top of the stack can require
        // per-window composition here.  Looking at every scene entry made an
        // unrelated translucent window underneath a game permanently disable
        // fullscreen unredirect.
        scene.last().is_some_and(|&(win, _, _, _, _)| {
            self.windows.get(&win).is_some_and(|wt| {
                let direct_candidate =
                    window_prefers_direct_presentation(wt.is_fullscreen, wt.bypass_compositor);
                let radius = if direct_candidate {
                    0.0
                } else {
                    wt.corner_radius_override.unwrap_or(self.corner_radius)
                };
                let base_opacity = if focused == Some(win) {
                    self.active_opacity
                } else {
                    self.inactive_opacity
                };
                let opacity = wt.opacity_override.unwrap_or(base_opacity)
                    * wt.fade_opacity
                    * self.peek_opacity_for(&wt.class_name);
                wt.has_rgba
                    || wt.is_frosted
                    || wt.is_shaped
                    // Shadows, borders, and rounded corners live outside or
                    // clip the edge of an output-covering performance window.
                    // Suppress them for direct candidates so the composited
                    // fallback (while an overlay is visible) matches bypass.
                    || edge_effects_require_composition(
                        direct_candidate,
                        wt.is_fullscreen,
                        self.shadow_enabled,
                        self.border_enabled,
                        self.border_width,
                        radius,
                    )
                    || opacity < 1.0
                    || (wt.scale - 1.0).abs() > 0.001
                    || (wt.anim_scale - 1.0).abs() > 0.001
                    || wt.wobbly.is_some()
                    || !wt.motion_trail.is_empty()
                    || (self.attention_animation && wt.is_urgent)
            })
        })
    }

    /// Restore compositor ownership of a manually-unredirected window.
    ///
    /// On failure the window is still being presented by X directly, so keep
    /// the state and tell the caller to continue bypassing this frame. Drawing
    /// into the overlay while the server still owns the client would otherwise
    /// produce a blank/frozen frame and lose the only handle needed to retry.
    pub(super) fn restore_unredirected_window(&mut self, window: u32, reason: &str) -> bool {
        let confirm_pixmap_on_damage = self.graphics.is_gles();
        let result = self
            .conn
            .redirect_window_manual(window)
            .and_then(|_| self.conn.flush_x11());
        match result {
            Ok(()) => {
                if let Some(wt) = self.windows.get_mut(&window) {
                    // The server allocated a new backing pixmap while the
                    // window was unredirected; the old TFP binding is stale.
                    wt.pixmap_refresh.backing_changed(confirm_pixmap_on_damage);
                }
                self.needs_render = true;
                log::info!(
                    "compositor: re-redirected window 0x{:x} ({})",
                    window,
                    reason
                );
                true
            }
            Err(error) => {
                self.unredirected_window = Some(window);
                self.needs_render = true;
                log::warn!(
                    "{}: window 0x{:x} ({}): {}",
                    self.display_ctx("fullscreen: re-redirect window"),
                    window,
                    reason,
                    error
                );
                false
            }
        }
    }

    pub(super) fn check_fullscreen_unredirect(
        &mut self,
        scene: &[(u32, i32, i32, u32, u32)],
        focused: Option<u32>,
    ) -> bool {
        // The simulation layer cannot run while the X server presents a
        // fullscreen client directly. Restore redirection on its first frame.
        if self.scene_requires_composition(scene, focused) {
            if let Some(previous) = self.unredirected_window.take() {
                if !self.restore_unredirected_window(previous, "compositor effect became active") {
                    return true;
                }
            }
            return false;
        }
        if !self.fullscreen_unredirect {
            if let Some(previous) = self.unredirected_window.take() {
                if !self.restore_unredirected_window(previous, "feature disabled") {
                    return true;
                }
            }
            return false;
        }
        // Focus can move directly from one output-covering client to another,
        // and minimizing a directly-presented fullscreen client removes it
        // from the drawable scene before its replacement capture is ready.
        // In both cases restore the old owner first. The prior implementation
        // could overwrite `unredirected_window` with the new candidate and
        // permanently lose the only retry handle for the old client.
        if let Some(previous) = self.unredirected_window
            && direct_presentation_owner_changed(previous, focused)
        {
            self.unredirected_window = None;
            if !self.restore_unredirected_window(previous, "direct-presentation owner changed") {
                return true;
            }
        }
        // Only unredirect if the top, focused window is an opaque fullscreen
        // client or explicitly carries `_NET_WM_BYPASS_COMPOSITOR = 1`.
        if let Some(focused_win) = focused {
            if let Some((is_fullscreen, bypass_compositor, has_rgba, is_shaped)) =
                self.windows.get(&focused_win).map(|wt| {
                    (
                        wt.is_fullscreen,
                        wt.bypass_compositor,
                        wt.has_rgba,
                        wt.is_shaped,
                    )
                })
            {
                if window_prefers_direct_presentation(is_fullscreen, bypass_compositor)
                    && !has_rgba
                    && !is_shaped
                    && scene.last().is_some_and(|entry| entry.0 == focused_win)
                {
                    // Check if it covers the full screen
                    if let Some(&(_, x, y, w, h)) =
                        scene.iter().rfind(|&&(win, _, _, _, _)| win == focused_win)
                    {
                        if i64::from(x) <= 0
                            && i64::from(y) <= 0
                            && i64::from(x) + i64::from(w) >= i64::from(self.screen_w)
                            && i64::from(y) + i64::from(h) >= i64::from(self.screen_h)
                        {
                            // Unredirect: the X server draws directly
                            if self.unredirected_window == Some(focused_win) {
                                if let Err(error) = self.conn.flush_x11() {
                                    self.needs_render = true;
                                    log::warn!(
                                        "{}: retrying for 0x{:x}: {}",
                                        self.display_ctx("fullscreen unredirect: flush"),
                                        focused_win,
                                        error
                                    );
                                }
                                return true;
                            }
                            match self.conn.unredirect_window_manual(focused_win) {
                                Ok(()) => {
                                    // Once accepted by the connection, treat
                                    // the request as authoritative even if the
                                    // flush reports a transient error. Drawing
                                    // concurrently would be unsafe; the next
                                    // frame retries the flush above.
                                    self.unredirected_window = Some(focused_win);
                                    // Frames presented directly by X bypass the
                                    // compositor-owned snapshot. Retaining the
                                    // previous composited frame here would
                                    // mislabel stale pixels as the last scene.
                                    self.presented_scene_status.invalidate();
                                    if let Err(error) = self.conn.flush_x11() {
                                        self.needs_render = true;
                                        log::warn!(
                                            "{}: window 0x{:x}: {}",
                                            self.display_ctx("fullscreen unredirect: flush"),
                                            focused_win,
                                            error
                                        );
                                    } else {
                                        log::info!(
                                            "compositor: directly presenting window 0x{:x} (fullscreen={}, bypass_hint={})",
                                            focused_win,
                                            is_fullscreen,
                                            bypass_compositor,
                                        );
                                    }
                                    return true;
                                }
                                Err(error) => {
                                    self.needs_render = true;
                                    log::warn!(
                                        "{}: window 0x{:x}: {}",
                                        self.display_ctx("fullscreen: unredirect window"),
                                        focused_win,
                                        error
                                    );
                                    return false;
                                }
                            }
                        }
                    }
                }
            }
        }
        // Re-redirect if the window no longer covers the output, loses focus,
        // withdraws its bypass request, or explicitly inhibits bypass.
        if let Some(prev) = self.unredirected_window.take() {
            if !self.restore_unredirected_window(prev, "window no longer eligible") {
                return true;
            }
        }
        false
    }

    // ----- Rendering -----

    /// Compute a simple hash of the scene + focused window for skip-unchanged detection.
    pub(super) fn scene_hash(scene: &[(u32, i32, i32, u32, u32)], focused: Option<u32>) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        scene.hash(&mut hasher);
        focused.hash(&mut hasher);
        hasher.finish()
    }

    fn minimized_render_source(
        &mut self,
        x11_win: u32,
        purpose: ThumbnailPurpose,
    ) -> Option<MinimizedRenderSource> {
        // A CPU hit is promoted lazily after an independent GPU LRU eviction.
        // Restore never enters this helper, and its policy rejects both tiers
        // even if a future call site accidentally tries to use it.
        if purpose != ThumbnailPurpose::RestoreAnimation
            && self.consume_minimized_gpu_upload(x11_win)
            && self.ensure_minimized_gpu_snapshot(x11_win)
        {
            self.resume_minimized_preview_after_capture(x11_win);
        }
        let retained_visual = self.minimized_visuals.get(&x11_win);
        let retained_animation = self
            .genie_active
            .iter()
            .find(|animation| animation.x11_win == x11_win);
        let source = select_minimized_thumbnail_source(
            purpose,
            MinimizedThumbnailAvailability {
                live: self.windows.contains_key(&x11_win),
                retained: retained_visual.is_some() || retained_animation.is_some(),
                gpu: self.current_minimized_gpu_snapshot_available(x11_win),
                cpu: self.current_minimized_cpu_snapshot_available(x11_win),
            },
        )?;
        match source {
            ThumbnailSource::GpuSnapshot => {
                let snapshot = self.minimized_gpu_snapshots.get(&x11_win)?;
                Some(MinimizedRenderSource {
                    texture: snapshot.texture,
                    has_alpha: snapshot.has_alpha,
                    width: snapshot.width as f32,
                    height: snapshot.height as f32,
                    uv_rect: snapshot.uv_rect(),
                })
            }
            ThumbnailSource::RetainedVisual => retained_visual
                .map(|visual| MinimizedRenderSource {
                    texture: visual.gl_texture,
                    has_alpha: visual.has_rgba,
                    width: visual.w,
                    height: visual.h,
                    uv_rect: [0.0, 0.0, 1.0, 1.0],
                })
                .or_else(|| {
                    retained_animation.map(|animation| MinimizedRenderSource {
                        texture: animation.gl_texture,
                        has_alpha: animation.has_rgba,
                        width: animation.w,
                        height: animation.h,
                        uv_rect: [0.0, 0.0, 1.0, 1.0],
                    })
                }),
            ThumbnailSource::LiveMappedTexture => {
                self.windows
                    .get(&x11_win)
                    .map(|window| MinimizedRenderSource {
                        texture: window.gl_texture,
                        has_alpha: window.has_rgba,
                        width: window.w as f32,
                        height: window.h as f32,
                        uv_rect: [0.0, 0.0, 1.0, 1.0],
                    })
            }
            // A failed upload cannot be drawn directly by OpenGL. Keep the CPU
            // entry for a later retry and let the bar's icon remain visible.
            ThumbnailSource::CpuSnapshot | ThumbnailSource::Placeholder => None,
        }
    }

    fn render_dock_preview(&mut self, projection: &[f32; 16]) {
        let Some(preview) = self.dock_preview else {
            return;
        };
        if preview.opacity <= 0.001 {
            return;
        }
        let Some(source) =
            self.minimized_render_source(preview.x11_win, ThumbnailPurpose::HoverPreview)
        else {
            return;
        };
        let output_bounds = output_bounds_for_anchor(
            preview.anchor,
            self.monitor_rects.iter().map(|&(_, x, y, w, h)| {
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
        let (x, y, w, h) = (
            rect.x as f32,
            rect.y as f32,
            rect.width as f32,
            rect.height as f32,
        );

        unsafe {
            // A compact shadow separates the preview from both light and dark
            // wallpapers while retaining the rounded macOS floating-card
            // silhouette.
            self.gl.use_program(Some(self.shadow_program));
            self.gl.uniform_matrix_4_f32_slice(
                self.shadow_uniforms.projection.as_ref(),
                false,
                projection,
            );
            self.gl.uniform_4_f32(
                self.shadow_uniforms.shadow_color.as_ref(),
                0.0,
                0.0,
                0.0,
                0.32 * preview.opacity,
            );
            self.gl
                .uniform_1_f32(self.shadow_uniforms.spread.as_ref(), 16.0);
            self.gl
                .uniform_1_f32(self.shadow_uniforms.radius.as_ref(), 14.0);
            self.gl.uniform_4_f32(
                self.shadow_uniforms.rect.as_ref(),
                x - 16.0,
                y - 12.0,
                w + 32.0,
                h + 32.0,
            );
            self.gl
                .uniform_2_f32(self.shadow_uniforms.size.as_ref(), w, h);
            self.gl.bind_vertex_array(Some(self.quad_vao));
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

            self.gl.use_program(Some(self.program));
            self.gl.uniform_matrix_4_f32_slice(
                self.win_uniforms.projection.as_ref(),
                false,
                projection,
            );
            self.gl
                .uniform_4_f32(self.win_uniforms.rect.as_ref(), x, y, w, h);
            self.gl.uniform_2_f32(self.win_uniforms.size.as_ref(), w, h);
            self.gl.uniform_1_i32(self.win_uniforms.texture.as_ref(), 0);
            self.gl.uniform_1_f32(
                self.win_uniforms.opacity.as_ref(),
                if source.has_alpha {
                    -preview.opacity
                } else {
                    preview.opacity
                },
            );
            self.gl
                .uniform_1_f32(self.win_uniforms.radius.as_ref(), 14.0);
            self.gl.uniform_1_f32(self.win_uniforms.dim.as_ref(), 1.0);
            self.gl.uniform_1_f32(self.win_uniforms.desat.as_ref(), 0.0);
            self.gl.uniform_4_f32(
                self.win_uniforms.uv_rect.as_ref(),
                source.uv_rect[0],
                source.uv_rect[1],
                source.uv_rect[2],
                source.uv_rect[3],
            );
            self.gl
                .uniform_1_f32(self.win_uniforms.ripple_progress.as_ref(), 0.0);
            self.gl
                .uniform_1_f32(self.win_uniforms.ripple_amplitude.as_ref(), 0.0);
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(source.texture));
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            self.gl.bind_texture(glow::TEXTURE_2D, None);
            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
        }
    }

    fn render_minimized_dock_items(&mut self, projection: &[f32; 16]) {
        let preview = self
            .dock_preview
            .map(|preview| (u64::from(preview.x11_win), preview.anchor, preview.opacity));
        let targets: Vec<_> = self
            .genie_targets
            .iter()
            .filter(|(x11_win, _)| {
                self.minimized_windows.contains(x11_win)
                    && !self
                        .genie_active
                        .iter()
                        .any(|animation| animation.x11_win == **x11_win)
            })
            .map(|(&x11_win, &target)| (x11_win, target))
            .collect();
        if targets.is_empty() {
            return;
        }
        unsafe {
            self.gl.use_program(Some(self.program));
            self.gl.uniform_matrix_4_f32_slice(
                self.win_uniforms.projection.as_ref(),
                false,
                projection,
            );
            self.gl.uniform_1_i32(self.win_uniforms.texture.as_ref(), 0);
            self.gl.uniform_1_f32(self.win_uniforms.dim.as_ref(), 1.0);
            self.gl.uniform_1_f32(self.win_uniforms.desat.as_ref(), 0.0);
            self.gl
                .uniform_1_f32(self.win_uniforms.ripple_progress.as_ref(), 0.0);
            self.gl
                .uniform_1_f32(self.win_uniforms.ripple_amplitude.as_ref(), 0.0);
            self.gl.bind_vertex_array(Some(self.quad_vao));
            self.gl.active_texture(glow::TEXTURE0);

            resolve_and_draw_each(
                self,
                targets,
                |compositor, (x11_win, stable_target)| {
                    let target =
                        dock_item_preview_target(u64::from(x11_win), stable_target, preview)?;
                    let source = compositor
                        .minimized_render_source(x11_win, ThumbnailPurpose::StaticDockCard)?;
                    (source.width > 0.0 && source.height > 0.0).then_some((source, target))
                },
                |compositor, (source, target)| {
                    // Draw before resolving the next item. That next resolve
                    // may perform a lazy CPU upload and delete this texture as
                    // the independent minimized-GPU cache's LRU victim.
                    let fit = (target.width / source.width).min(target.height / source.height);
                    let width = (source.width * fit).max(1.0);
                    let height = (source.height * fit).max(1.0);
                    let x = target.x + (target.width - width) * 0.5;
                    let y = target.y + (target.height - height) * 0.5;
                    compositor.gl.uniform_4_f32(
                        compositor.win_uniforms.rect.as_ref(),
                        x,
                        y,
                        width,
                        height,
                    );
                    compositor.gl.uniform_2_f32(
                        compositor.win_uniforms.size.as_ref(),
                        width,
                        height,
                    );
                    compositor.gl.uniform_1_f32(
                        compositor.win_uniforms.opacity.as_ref(),
                        if source.has_alpha { -1.0 } else { 1.0 },
                    );
                    compositor.gl.uniform_1_f32(
                        compositor.win_uniforms.radius.as_ref(),
                        5.0_f32.min(height * 0.5),
                    );
                    compositor.gl.uniform_4_f32(
                        compositor.win_uniforms.uv_rect.as_ref(),
                        source.uv_rect[0],
                        source.uv_rect[1],
                        source.uv_rect[2],
                        source.uv_rect[3],
                    );
                    compositor
                        .gl
                        .bind_texture(glow::TEXTURE_2D, Some(source.texture));
                    compositor.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                },
            );
            self.gl.bind_texture(glow::TEXTURE_2D, None);
            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
        }
    }

    fn draw_wallpaper_layer(
        &self,
        texture: glow::Texture,
        mode: WallpaperMode,
        img_w: u32,
        img_h: u32,
        area: (f32, f32, f32, f32),
        opacity: f32,
    ) {
        if opacity <= 0.0 {
            return;
        }
        let (rx, ry, rw, rh) = compute_wallpaper_rect(mode, area, img_w, img_h);
        unsafe {
            self.gl
                .uniform_1_f32(self.win_uniforms.opacity.as_ref(), opacity);
            self.gl
                .uniform_4_f32(self.win_uniforms.rect.as_ref(), rx, ry, rw, rh);
            self.gl
                .uniform_2_f32(self.win_uniforms.size.as_ref(), rw, rh);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        }
    }

    /// Render a composited frame.
    ///
    /// `scene` is an ordered list of (x11_win, x, y, w, h) from bottom to top.
    /// `focused` is the X11 window ID of the focused window (if any).
    /// Returns true if a frame was rendered.
    pub(crate) fn render_frame(
        &mut self,
        scene: &[(u32, i32, i32, u32, u32)],
        focused: Option<u32>,
    ) -> bool {
        let bench_frame_start = std::time::Instant::now();
        // Last frame's frosted-glass backdrop describes a framebuffer that is
        // about to be overwritten; the first panel that needs one recaptures.
        self.glass_backdrop = None;

        // The WM removes an unmapped client from its live stacking list before
        // the compositor's fade-out finishes. Keep such compositor-owned
        // textures in a small closing layer so the fade is actually visible
        // instead of ticking an off-screen state until it is freed.
        let mut closing_scene = Vec::new();
        let has_detached_fade = self.fading
            && self.windows.iter().any(|(&id, wt)| {
                wt.fading_out && !scene.iter().any(|&(scene_id, ..)| scene_id == id)
            });
        let scene = if has_detached_fade {
            closing_scene.extend_from_slice(scene);
            closing_scene.extend(self.windows.iter().filter_map(|(&id, wt)| {
                (wt.fading_out
                    && wt.w > 0
                    && wt.h > 0
                    && !scene.iter().any(|&(scene_id, ..)| scene_id == id))
                .then_some((id, wt.x, wt.y, wt.w, wt.h))
            }));
            closing_scene.as_slice()
        } else {
            scene
        };

        // Consume the wakeup that brought us here before any fullscreen bypass
        // can return early. Otherwise a direct-scanout/unredirected client
        // leaves this flag permanently armed and both X11 loops poll at 1 ms.
        // Requests generated while preparing this frame are folded in again at
        // the unchanged-frame gate below.
        let mut explicit_render = std::mem::take(&mut self.needs_render);
        let mut damage_wakeup = std::mem::take(&mut self.damage_render_pending);

        // Auto-enable profiler when benchmark is running
        if self.benchmark.is_running() && !self.frame_profiler.is_enabled() {
            self.frame_profiler.set_enabled(true);
        }

        // Phase 2: Begin frame profiling
        self.frame_profiler.begin_frame();

        // Consume the newest completed simulation frame before deciding whether
        // fullscreen may bypass the compositor.
        if self
            .waterlily_ipc
            .as_ref()
            .is_some_and(WaterlilyIpc::has_pending)
            && !self.context_current
        {
            if let Err(error) = self.graphics.make_current() {
                log::error!(
                    "{}: {error}",
                    self.renderer_ctx("waterlily: make context current")
                );
                self.needs_render = true;
                return false;
            }
            self.context_current = true;
        }
        self.poll_waterlily_frame();
        let waterlily_layer_dirty = self.waterlily_layer_dirty;

        // P6A: Process deferred X11 operations at start of render frame
        self.process_deferred_x11_ops();

        // Update GPU load cache with hysteresis: update if delta > 5% or elapsed > 0.5s
        let current_gpu_load = {
            let target_frame_time_ms = 1000.0 / 60.0;
            if self.frame_stats.frame_times.is_empty() {
                0
            } else {
                let avg_frame_time_ms = self.frame_stats.frame_times.iter().sum::<f32>()
                    / self.frame_stats.frame_times.len() as f32;
                let load = (avg_frame_time_ms / target_frame_time_ms * 100.0) as u32;
                load.min(100)
            }
        };

        if current_gpu_load > self.last_gpu_load + 5
            || current_gpu_load + 5 < self.last_gpu_load
            || self.last_gpu_load_update.elapsed().as_millis() > 500
        {
            self.last_gpu_load = current_gpu_load;
            self.last_gpu_load_update = std::time::Instant::now();
        }

        let periodic_60_frame = self.frame_stats.frame_count % 60 == 0;

        // Shader hot-reload: poll every 60 frames (~1s at 60fps)
        if self.shader_hot_reload_enabled && periodic_60_frame {
            self.poll_shader_hot_reload();
        }

        // VRR state update: check every 60 frames (~1s at 60fps)
        if periodic_60_frame {
            self.update_vrr_state(focused);
        }

        // Track render diagnostics only when info logging is enabled; default
        // runs avoid the atomic counters and realtime-clock read entirely.
        if log::log_enabled!(log::Level::Info) {
            static RENDER_LOG_COUNT: std::sync::atomic::AtomicU32 =
                std::sync::atomic::AtomicU32::new(0);
            let count = RENDER_LOG_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if count < 5 || count % 500 == 0 {
                log::info!(
                    "[compositor::render_frame] frame={} scene={} tracked={}",
                    count,
                    scene.len(),
                    self.windows.len()
                );
            }

            static RENDER_FREQ_COUNT: std::sync::atomic::AtomicU32 =
                std::sync::atomic::AtomicU32::new(0);
            static RENDER_FREQ_EPOCH: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            let epoch = RENDER_FREQ_EPOCH.load(std::sync::atomic::Ordering::Relaxed);
            if epoch == 0 {
                RENDER_FREQ_EPOCH.store(now_ms, std::sync::atomic::Ordering::Relaxed);
            }
            let fc = RENDER_FREQ_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if now_ms - epoch >= 2000 {
                let elapsed = (now_ms - epoch) as f64 / 1000.0;
                log::info!(
                    "[compositor::render_freq] {:.1} renders/sec (needs_render={}, focused={:?}, dmg raw={} marked={} unresolved={} untracked={})",
                    fc as f64 / elapsed,
                    self.needs_render,
                    focused,
                    crate::backend::damage_diag::RAW.load(std::sync::atomic::Ordering::Relaxed),
                    crate::backend::damage_diag::MARKED.load(std::sync::atomic::Ordering::Relaxed),
                    crate::backend::damage_diag::UNRESOLVED
                        .load(std::sync::atomic::Ordering::Relaxed),
                    crate::backend::damage_diag::UNTRACKED
                        .load(std::sync::atomic::Ordering::Relaxed),
                );
                RENDER_FREQ_COUNT.store(0, std::sync::atomic::Ordering::Relaxed);
                RENDER_FREQ_EPOCH.store(now_ms, std::sync::atomic::Ordering::Relaxed);
            }
        }

        // Arm focus highlighting before either fullscreen bypass decision.
        // Otherwise the first focus change can enter unredirect before the
        // highlight exists, so no later frame ever starts the animation.
        if self.focus_highlight {
            if let Some(fw) = focused
                && self.last_focused_window != Some(fw)
            {
                self.focus_highlight_start = Some((fw, std::time::Instant::now()));
            }
            self.last_focused_window = focused;
        }

        // Phase 2.3: Direct scanout check - bypass compositor for eligible fullscreen windows
        // This provides -8-12ms latency reduction for fullscreen games/video
        if self.scene_requires_composition(scene, focused) {
            // Recording and compositor-owned visual layers need frames produced
            // by the compositor. End a previously active bypass immediately so
            // a fullscreen client cannot hide them. WaterLily is not window
            // blur, so keep this overlay constraint separate from client flags.
            let _ = self.direct_scanout_mgr.check_scene(&[], None);
        } else {
            let mut scene_info = std::mem::take(&mut self.scratch_scene_info);
            scene_info.clear();
            scene_info.reserve(scene.len());
            scene_info.extend(scene.iter().filter_map(|&(win, x, y, w, h)| {
                self.windows.get(&win).map(|wt| {
                    let direct_candidate =
                        window_prefers_direct_presentation(wt.is_fullscreen, wt.bypass_compositor);
                    let corner_radius = if direct_candidate {
                        0.0
                    } else {
                        wt.corner_radius_override.unwrap_or(self.corner_radius)
                    };
                    (
                        win,
                        WindowScanoutInfo {
                            x,
                            y,
                            width: w,
                            height: h,
                            is_fullscreen: direct_candidate,
                            has_alpha: wt.has_rgba,
                            has_blur: wt.is_frosted,
                            has_shadow: self.shadow_enabled && !direct_candidate,
                            has_corner_radius: corner_radius > 0.0,
                            opacity: wt.fade_opacity,
                        },
                    )
                })
            }));

            // X11 has no KMS plane commit here; this manager is eligibility
            // telemetry only. The real bypass below is Composite unredirect.
            // Returning on this in-memory result would freeze the last frame.
            let _ = self.direct_scanout_mgr.check_scene(&scene_info, focused);
            self.scratch_scene_info = scene_info;
        }

        // Fullscreen unredirect check
        if self.check_fullscreen_unredirect(scene, focused) {
            return false;
        }

        // Delta-driven effects use a clock that only runs across consecutive
        // active frames. `frame_stats.last_frame_time` can predate a newly
        // spawned effect by minutes after compositor idle, which would make a
        // fresh fade or particle burst finish before its first draw.
        let incremental_effects_active = self.incremental_effects_active();
        let effect_dt = self
            .effect_tick_clock
            .delta(std::time::Instant::now(), incremental_effects_active);

        // Tick fade animations
        let fade_tick = self.tick_fades(effect_dt);
        let fades_active = fade_tick.any;

        // Tick wobbly spring physics
        let wobbly_active = self.tick_wobbly();

        // Tick particle and motion-trail lifetimes before the unchanged-frame
        // gate so their state cannot get stuck behind that optimization.
        let particles_active = self.tick_particles(effect_dt);
        let motion_trails_active = self.tick_motion_trails();
        let tilt_pending = super::effects::tilt_animation_pending(
            self.window_tilt,
            self.tilt_current_x,
            self.tilt_current_y,
            self.tilt_target_x,
            self.tilt_target_y,
        );
        let attention_active =
            self.attention_animation && self.windows.values().any(|wt| wt.is_urgent);
        // A rotating gradient border needs continuous frames while a border
        // can actually be drawn (smart borders require >1 client window).
        let gradient_border_animating = self.border_gradient_enabled
            && self.border_gradient_speed != 0.0
            && self.border_enabled
            && self.border_width > 0.0
            && self.windows.len() > 1;
        let overview_animating = self.overview_animation_pending();
        // Toasts and the OSD fade on a wall-clock envelope; keep frames coming
        // while any card is visible (bounded by the toast/OSD timeout).
        let toasts_active = !self.toast_stack.is_empty() || !self.osd_slot.is_empty();

        // Tick Phase 5 animations
        let expose_animating = self.tick_expose();
        let snap_animating = self.tick_snap_preview();
        let peek_animating = self.tick_peek();

        // Tick Phase 3 animations
        let genie_active = self.tick_genie();
        let ripples_active = self.tick_ripples();
        let focus_highlight_active = self.tick_focus_highlight();
        let wallpaper_crossfade_active = self.tick_wallpaper_crossfade();

        // Update damage tracker scene state for dynamic thresholds
        let any_animating = fades_active
            || wobbly_active
            || particles_active
            || motion_trails_active
            || tilt_pending
            || overview_animating
            || expose_animating
            || snap_animating
            || peek_animating
            || genie_active
            || ripples_active
            || focus_highlight_active
            || wallpaper_crossfade_active
            || attention_active
            || gradient_border_animating
            || toasts_active;
        self.damage_tracker
            .update_state(self.windows.len(), any_animating);

        // Poll for async wallpaper decode results and upload to GPU if ready.
        let mut wallpaper_just_loaded = false;
        let wallpaper_result = self.pending_wallpaper.as_ref().map(|rx| rx.try_recv());
        match wallpaper_result {
            Some(Ok(data)) => {
                if let Some((tex, w, h)) = Self::upload_wallpaper_texture(&self.gl, &data) {
                    unsafe {
                        if self.wallpaper_crossfade {
                            if let Some(stale) = self.old_wallpaper_texture.take() {
                                self.gl.delete_texture(stale);
                            }
                            if self.wallpaper_texture.is_some() {
                                self.old_wallpaper_mode = self.wallpaper_mode;
                                self.old_wallpaper_img_w = self.wallpaper_img_w;
                                self.old_wallpaper_img_h = self.wallpaper_img_h;
                            } else {
                                self.old_wallpaper_img_w = 0;
                                self.old_wallpaper_img_h = 0;
                            }
                            self.old_wallpaper_texture = self.wallpaper_texture.take();
                            self.wallpaper_transition_start = self
                                .old_wallpaper_texture
                                .map(|_| std::time::Instant::now());
                        } else {
                            if let Some(previous) = self.wallpaper_texture.take() {
                                self.gl.delete_texture(previous);
                            }
                            if let Some(stale) = self.old_wallpaper_texture.take() {
                                self.gl.delete_texture(stale);
                            }
                            self.old_wallpaper_img_w = 0;
                            self.old_wallpaper_img_h = 0;
                            self.wallpaper_transition_start = None;
                        }
                    }
                    self.wallpaper_texture = Some(tex);
                    self.wallpaper_img_w = w;
                    self.wallpaper_img_h = h;
                    self.wallpaper_mode = data.mode;
                    wallpaper_just_loaded = true;
                    log::info!("compositor: async wallpaper ready ({}x{})", w, h);
                }
                self.pending_wallpaper = None;
            }
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                log::warn!("compositor: async wallpaper loader disconnected");
                self.pending_wallpaper = None;
            }
            Some(Err(std::sync::mpsc::TryRecvError::Empty)) | None => {}
        }
        // Poll per-monitor wallpaper results
        self.pending_monitor_wallpapers.retain_mut(|(idx, rx)| {
            match rx.try_recv() {
                Ok(data) => {
                    if let Some(mw) = self.monitor_wallpapers.get_mut(*idx)
                        && let Some((tex, w, h)) = Self::upload_wallpaper_texture(&self.gl, &data)
                    {
                        if let Some(previous) = mw.texture.replace(tex) {
                            unsafe {
                                self.gl.delete_texture(previous);
                            }
                        }
                        mw.img_w = w;
                        mw.img_h = h;
                        mw.mode = data.mode;
                        wallpaper_just_loaded = true;
                        log::info!(
                            "compositor: async monitor wallpaper [{}] ready ({}x{})",
                            idx,
                            w,
                            h
                        );
                    }
                    false // remove from pending list
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    log::warn!(
                        "compositor: async monitor wallpaper loader [{}] disconnected",
                        idx
                    );
                    false
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => true,
            }
        });
        if wallpaper_just_loaded {
            self.needs_render = true;
        }

        // Skip-unchanged-frame: if scene hasn't changed and no textures are
        // dirty, we can skip the entire GL render (unless screenshot pending or HUD active).
        // While scanning, also feed the precise dirty-rect tracker so we do not
        // walk the scene a second time later in the frame.
        let pixmap_refresh_now = std::time::Instant::now();
        let pixmap_refresh_ready = self
            .windows
            .values()
            .any(|wt| wt.pixmap_refresh.needs_refresh_at(pixmap_refresh_now));
        // Refreshes are global rather than scene-local: an off-scene window
        // with an expired retry deadline must still get one attempt, otherwise
        // needs_render() would remain armed and spin without reaching
        // refresh_pixmaps().
        let mut has_dirty = pixmap_refresh_ready;
        let mut needs_native_texture_sync = pixmap_refresh_ready;
        for &(win, _, _, _, _) in scene {
            let Some(wt) = self.windows.get(&win) else {
                continue;
            };
            // Both EGLImage and GLX texture-from-pixmap imports need native X
            // rendering to complete before sampling. The GLX extension has no
            // implicit X/GL synchronization; omitting this on NVIDIA can show
            // an older client frame (most visibly terminal cursor/text damage).
            if (wt.dirty && wt.binding.is_some())
                || wt.pixmap_refresh.needs_refresh_at(pixmap_refresh_now)
            {
                needs_native_texture_sync = true;
            }
            if wt.dirty || wt.pixmap_refresh.needs_refresh_at(pixmap_refresh_now) {
                has_dirty = true;
                let dirty_rect = DirtyRect::new(wt.x, wt.y, wt.w, wt.h);
                self.dirty_region_tracker.mark_dirty(dirty_rect);
            }
        }
        // A WaterLily publication changes only its native-size overlay region,
        // but it is still sufficient reason to render when no client texture
        // changed.
        has_dirty |= waterlily_layer_dirty;
        explicit_render |= std::mem::take(&mut self.needs_render);
        damage_wakeup |= std::mem::take(&mut self.damage_render_pending);
        // XDamage is a reason to enter the frame, but not a request to redraw
        // every pixel. Visible dirty windows populated the precise region
        // above; keeping the wakeup separate from `explicit_render` lets the
        // buffer-age repair path remain incremental.
        has_dirty |= damage_wakeup;
        let force_render = self.screenshot_requests.has_pending()
            || self.screenshot_freeze_pending
            || self.debug_hud
            || self.transition_active()
            || overview_animating
            || expose_animating
            || snap_animating
            || peek_animating
            || genie_active
            || ripples_active
            || focus_highlight_active
            || wallpaper_crossfade_active
            // Recording forces a composite only when it will capture something
            // that is not already on tape. Client damage and animations reach
            // the gate through `has_dirty`/`scene_changed` below and capture at
            // the recording rate on their own; this term covers the two things
            // that produce a new frame without producing damage — the cursor
            // sprite moving, and the idle heartbeat that keeps the encoded
            // timeline from ending early on a motionless screen.
            || recording_capture_warranted(
                self.recording_frame_due(),
                self.recording_heartbeat_due(),
                self.recording_cursor_moved(),
            )
            || self.annotation_active
            || wallpaper_just_loaded
            || wobbly_active
            || particles_active
            || motion_trails_active
            || tilt_pending
            || attention_active
            // Toast cards, the OSD, and the modal system UI animate on their
            // own timeline and own no window, so no client damage marks them
            // dirty. Without them here the push frame draws the card at the
            // very start of its fade and the gate below throws every later
            // frame away, leaving it frozen at ~0 alpha and invisible.
            || !self.toast_stack.is_empty()
            || !self.osd_slot.is_empty()
            || self.system_ui.is_some()
            || explicit_render;
        let hash = Self::scene_hash(scene, focused);
        let scene_changed = hash != self.last_scene_hash;
        if !has_dirty && !fades_active && !force_render && !scene_changed {
            return false;
        }
        self.last_scene_hash = hash;

        // Snapshot config once for the whole frame. status_bar_name / border_px
        // were previously loaded 4× per frame from separate ArcSwap guards.
        let frame_cfg = crate::config::CONFIG.load();
        let frame_status_bar_name = frame_cfg.status_bar_name();

        // Reset tilt targets — the render loop will set them if a focused window
        // uses tilt; otherwise they stay at 0 so the tilt smoothly returns to rest.
        if self.window_tilt {
            self.tilt_target_x = 0.0;
            self.tilt_target_y = 0.0;
        }

        // Invalidate backdrop results only for changes that can alter pixels
        // without producing client damage. Ordinary XDamage wakeups are kept
        // out of `explicit_render`, so continuously-rendering clients can use
        // both the blur cache and partial framebuffer repair.
        let uncached_blur_source_changed = waterlily_layer_dirty
            || self.transition_active()
            || self.overview_active
            || self.expose_active
            || expose_animating
            || snap_animating
            || peek_animating
            || genie_active
            || ripples_active
            || focus_highlight_active
            || wallpaper_crossfade_active
            || wallpaper_just_loaded
            || wobbly_active
            || motion_trails_active
            || tilt_pending
            || attention_active;
        // Scene structure, focus, and per-window animation state are encoded
        // into each consumer's running below-scene hash. A topmost input-method
        // popup therefore cannot invalidate unrelated clients underneath it.
        if uncached_blur_source_changed {
            self.invalidate_window_blur_caches();
        }

        // Ensure the selected graphics context is current.
        if !self.context_current {
            if let Err(error) = self.graphics.make_current() {
                log::error!(
                    "{}: {error}",
                    self.renderer_ctx("frame: make context current")
                );
                self.needs_render = true;
                return false;
            }
            self.context_current = true;
        }

        // True-Iconic admission is serviced by the backend after render_frame
        // returns. Rebuild any missing CPU owner only here, after a successful
        // make-current; WM-facing ensure/admission paths merely arm the gate.
        self.service_iconic_snapshot_recaptures_current_context();

        // Explicit Dock demand gets one CPU-to-GPU promotion attempt per arm.
        // Service it before preview/card early returns so failure always
        // consumes the gate and subsequent unrelated frames may unredirect.
        self.service_minimized_gpu_uploads();

        // Recreate pixmaps for windows that were resized (batched, single XSync)
        let pixmaps_native_synced = self.refresh_pixmaps();

        // Collect which windows are dirty this frame (before TFP refresh clears
        // the flags).  Used by the blur cache to skip expensive blur passes when
        // only the frosted window itself updated (e.g. fcitx candidate list).
        let mut blur_dirty_wins = std::mem::take(&mut self.scratch_blur_dirty);
        blur_dirty_wins.clear();
        blur_dirty_wins.reserve(scene.len());
        blur_dirty_wins.extend(scene.iter().filter_map(|&(win, _, _, _, _)| {
            self.windows
                .get(&win)
                .and_then(|wt| if wt.dirty { Some(win) } else { None })
        }));
        blur_dirty_wins.sort_unstable();

        // Refresh TFP textures for dirty windows with per-frame time budget.
        // Focused window always updates; others update within 3ms budget.
        // NOTE: We intentionally do NOT call glGetError() here.
        // Genuine pixmap invalidation is handled by update_geometry → pixmap_refresh.
        let tfp_budget = std::time::Duration::from_micros(3000); // 3ms
        let tfp_start = std::time::Instant::now();

        // Build priority-ordered window list: focused first, then status bars,
        // then the rest of the scene. A bar update is direct feedback for the
        // focus action and must not sit behind the ordinary 3 ms TFP budget.
        let mut tfp_order = std::mem::take(&mut self.scratch_tfp_order);
        tfp_order.clear();
        tfp_order.reserve(scene.len());
        let mut focused_in_scene = false;
        if let Some(fw) = focused {
            tfp_order.push(fw);
        }
        for &(win, _, _, _, _) in scene {
            if Some(win) != focused
                && self
                    .windows
                    .get(&win)
                    .is_some_and(|wt| is_status_bar_class(&wt.class_name, frame_status_bar_name))
            {
                tfp_order.push(win);
            }
        }
        for &(win, _, _, _, _) in scene {
            if Some(win) == focused {
                focused_in_scene = true;
            } else if !self
                .windows
                .get(&win)
                .is_some_and(|wt| is_status_bar_class(&wt.class_name, frame_status_bar_name))
            {
                tfp_order.push(win);
            }
        }
        if focused.is_some() && !focused_in_scene {
            tfp_order.remove(0);
        }

        let mut tfp_budget_exhausted = false;
        if needs_native_texture_sync && !pixmaps_native_synced {
            if let Err(error) = self.graphics.sync_x11() {
                log::warn!(
                    "{}: {error}",
                    self.renderer_ctx("frame: synchronize native textures")
                );
            }
        }
        for win in &tfp_order {
            let win = *win;
            let latency_critical = self.windows.get(&win).is_some_and(|wt| {
                tfp_refresh_is_latency_critical(win, focused, &wt.class_name, frame_status_bar_name)
            });
            // Focused windows and status bars always update. Otherwise a busy
            // focused client can exhaust the budget every frame and starve the
            // bar indefinitely, leaving its previous title on screen.
            if tfp_budget_exhausted && !latency_critical {
                continue;
            }
            if let Some(wt) = self.windows.get_mut(&win) {
                if wt.dirty && wt.binding.is_some() {
                    // Audio sync: skip texture update if this window's audio timing
                    // says it's not yet time to present the next frame.
                    // This prevents forcing all video windows into the compositor's
                    // frame rate, which was the root cause of audio-video desync.
                    if wt.audio_sync_target.is_some() {
                        if !self.audio_sync.should_render(win) {
                            continue;
                        }
                        // Check for stale audio streams — fall back to normal rendering
                        if self.audio_sync.should_fallback(win) {
                            self.audio_sync.unregister_stream(win);
                            wt.audio_sync_target = None;
                            log::debug!("compositor: audio sync fallback for 0x{:x} (stale)", win);
                        }
                    }

                    if let Some(binding) = wt.binding.as_ref() {
                        if let Err(error) =
                            self.graphics
                                .refresh_pixmap_binding(&self.gl, wt.gl_texture, binding)
                        {
                            log::warn!(
                                "{}: {} binding for 0x{win:x}: {error}",
                                self.renderer_ctx("frame: refresh pixmap binding"),
                                self.graphics.api_name()
                            );
                            continue;
                        }
                    }
                    wt.dirty = false;

                    // Mark frame rendered in audio sync manager
                    if wt.audio_sync_target.is_some() {
                        self.audio_sync.mark_frame_rendered(win);
                    }

                    // Check budget only for ordinary windows. Latency-critical
                    // entries remain outside the starvation-prone budget.
                    if !latency_critical && tfp_start.elapsed() > tfp_budget {
                        tfp_budget_exhausted = true;
                    }
                }
            }
        }

        // --- Occlusion culling ---
        let mut first_visible = 0usize;
        {
            for i in (0..scene.len()).rev() {
                let (win, x, y, w, h) = scene[i];
                let Some(wt) = self.windows.get(&win) else {
                    continue;
                };
                let is_focused = focused == Some(win);
                let is_statusbar = wt.class_name == frame_status_bar_name
                    || wt.class_name.contains(frame_status_bar_name);
                let base_opacity = if is_statusbar {
                    1.0
                } else if is_focused {
                    self.active_opacity
                } else {
                    self.inactive_opacity
                };
                let layer_opacity = wt.opacity_override.unwrap_or(base_opacity)
                    * wt.fade_opacity
                    * self.peek_opacity_for(&wt.class_name);
                let direct_candidate =
                    window_prefers_direct_presentation(wt.is_fullscreen, wt.bypass_compositor);
                let corner_radius = if direct_candidate {
                    0.0
                } else {
                    wt.corner_radius_override.unwrap_or_else(|| {
                        if class_matches_exclude(&wt.class_name, &self.rounded_corners_exclude) {
                            0.0
                        } else {
                            self.corner_radius
                        }
                    })
                };
                let geometry_deformation_active = (self.wobbly_windows && wt.wobbly.is_some())
                    || (self.window_tilt && is_focused && !is_statusbar);

                if rect_covers_output(x, y, w, h, self.screen_w, self.screen_h)
                    && is_opaque_occluder(
                        wt.has_rgba,
                        layer_opacity,
                        corner_radius,
                        wt.is_shaped,
                        wt.scale,
                        wt.anim_scale,
                        geometry_deformation_active,
                    )
                {
                    first_visible = i;
                    break;
                }
            }
        }

        // Feature 8/9/10: If postprocessing is active, render into postprocess FBO
        let postprocess_active = self.needs_postprocess() && self.postprocess_fbo.is_some();
        if postprocess_active {
            let (pp_fbo, _) = self.postprocess_fbo.as_ref().unwrap();
            unsafe {
                self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(*pp_fbo));
            }
        }

        // A blur cache miss captures the complete framebuffer midway through
        // the bottom-to-top window pass. On a scissored repair frame, pixels
        // outside the repair region still contain the previous *final* scene,
        // including consumers that have not been reached yet. Never promote
        // that self-contaminated snapshot into a backdrop cache: reconstruct
        // the full scene whenever dirty lower content can affect a blur window.
        let blur_damage_requires_full_redraw = self.blur_enabled
            && !self.blur_fbos.is_empty()
            && dirty_below_requires_full_blur_redraw(
                &scene[first_visible..],
                &blur_dirty_wins,
                self.blur_fbos.len(),
                |win| {
                    let Some(wt) = self.windows.get(&win) else {
                        return false;
                    };
                    wt.fade_opacity > 0.0 && self.needs_backdrop_blur(wt, frame_status_bar_name)
                },
            );

        // Apply a scissor using the current scene changes plus any intervening
        // damage missing from a recycled GLX/EGL back buffer. Unknown/undefined
        // buffers safely fall back to a full redraw while still building useful
        // history for subsequent frames.
        let current_damage = self.dirty_region_tracker.merged();
        let transformed_overlay_active = transformed_overlays_require_full_redraw(
            self.overview_active,
            self.overview_closing,
            self.expose_active,
            !self.expose_entries.is_empty(),
        );
        let incremental_frame = !force_render
            && !scene_changed
            && !fades_active
            && !blur_damage_requires_full_redraw
            && !transformed_overlay_active;
        let buffer_age =
            if self.partial_damage_enabled && incremental_frame && current_damage.is_some() {
                self.graphics.partial_redraw_buffer_age()
            } else {
                0
            };
        let repair_damage = current_damage.and_then(|damage| {
            self.buffer_age_damage_history
                .repair_region(damage, buffer_age)
        });
        let mut use_scissor = repair_damage.is_some();
        let full_frame_damage = DirtyRect::new(0, 0, self.screen_w, self.screen_h);
        let frame_damage = if incremental_frame {
            current_damage.unwrap_or(full_frame_damage)
        } else {
            full_frame_damage
        };
        let mut damage_scissor = (0i32, 0i32, self.screen_w as i32, self.screen_h as i32);
        let mut swap_damage_rects = std::mem::take(&mut self.scratch_swap_damage);
        swap_damage_rects.clear();
        if let Some(rect) = repair_damage {
            unsafe {
                self.gl.enable(glow::SCISSOR_TEST);
                // GL scissor uses bottom-left origin
                let gl_y = self.screen_h as i32 - rect.y - rect.height as i32;
                damage_scissor = (rect.x, gl_y, rect.width as i32, rect.height as i32);
                self.gl.scissor(
                    damage_scissor.0,
                    damage_scissor.1,
                    damage_scissor.2,
                    damage_scissor.3,
                );
            }

            // Keep GL repair to one scissor bounding box, but submit only the
            // current frame's disjoint scene changes to EGL. Damage from older
            // frames is needed to repair this back buffer, but already matches
            // the current front buffer and does not need to be presented again.
            // The Vec is compositor scratch storage and normally allocates no
            // memory here.
            if self.graphics.supports_swap_with_damage() {
                swap_damage_rects.reserve(self.dirty_region_tracker.region_count() * 4);
                for dirty in self.dirty_region_tracker.iter() {
                    append_egl_damage_rect(&mut swap_damage_rects, self.screen_h, dirty);
                }
                if swap_damage_rects.is_empty() {
                    swap_damage_rects.extend_from_slice(&[
                        damage_scissor.0,
                        damage_scissor.1,
                        damage_scissor.2,
                        damage_scissor.3,
                    ]);
                }
            }
        }
        if use_scissor
            && !self.graphics.set_damage_region(&[
                damage_scissor.0,
                damage_scissor.1,
                damage_scissor.2,
                damage_scissor.3,
            ])
        {
            // KHR_partial_update consumes buffer damage (the full repair area),
            // unlike swap-with-damage which consumes only this frame's surface
            // changes. A KHR-only implementation must confirm this before any
            // drawing; otherwise the contents inside its default full damage
            // region are undefined and this frame must be redrawn completely.
            unsafe {
                self.gl.disable(glow::SCISSOR_TEST);
            }
            use_scissor = false;
        }
        self.damage_tracker.clear();
        self.dirty_region_tracker.clear(); // P5C: Clear rect tracker

        // Clear
        unsafe {
            self.gl
                .viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
            self.gl.clear_color(0.0, 0.0, 0.0, 0.0);
            self.gl.clear(glow::COLOR_BUFFER_BIT);
        }

        // Build orthographic projection matrix (column-major)
        let proj = ortho(
            0.0,
            self.screen_w as f32,
            self.screen_h as f32,
            0.0,
            -1.0,
            1.0,
        );

        // Draw wallpaper background (per-monitor or global fallback)
        // Skip if a fully-opaque window already covers the entire screen (occluded).
        {
            let wallpaper_occluded = first_visible > 0;
            let global_transition_progress = self.wallpaper_transition_start.map(|start| {
                let elapsed = start.elapsed().as_millis() as f32;
                let duration = self.wallpaper_crossfade_duration_ms.max(1) as f32;
                (elapsed / duration).clamp(0.0, 1.0)
            });
            let has_wallpaper = !wallpaper_occluded
                && (!self.monitor_wallpapers.is_empty()
                    || self.wallpaper_texture.is_some()
                    || (self.old_wallpaper_texture.is_some()
                        && global_transition_progress.is_some()));
            if has_wallpaper {
                unsafe {
                    self.gl.use_program(Some(self.program));
                    self.gl.uniform_matrix_4_f32_slice(
                        self.win_uniforms.projection.as_ref(),
                        false,
                        &proj,
                    );
                    self.gl.uniform_1_i32(self.win_uniforms.texture.as_ref(), 0);
                    self.gl.bind_vertex_array(Some(self.quad_vao));
                    self.gl
                        .uniform_1_f32(self.win_uniforms.opacity.as_ref(), 1.0);
                    self.gl
                        .uniform_1_f32(self.win_uniforms.radius.as_ref(), 0.0);
                    self.gl.uniform_1_f32(self.win_uniforms.dim.as_ref(), 1.0);
                    self.gl.uniform_1_f32(self.win_uniforms.desat.as_ref(), 0.0);
                    self.gl
                        .uniform_4_f32(self.win_uniforms.uv_rect.as_ref(), 0.0, 0.0, 1.0, 1.0);
                    self.gl.active_texture(glow::TEXTURE0);

                    if !self.monitor_wallpapers.is_empty() {
                        // Per-monitor wallpaper rendering uses the intersection
                        // of its monitor and the frame repair region. Previously
                        // this disabled damage scissoring and redrew every monitor
                        // for even a tiny window update.
                        for mw in &self.monitor_wallpapers {
                            let has_monitor_override = mw.texture.is_some();
                            let blend = wallpaper_blend_plan(
                                has_monitor_override,
                                self.wallpaper_texture.is_some(),
                                self.old_wallpaper_texture.is_some(),
                                global_transition_progress,
                            );
                            if blend.old_global_opacity.is_none() && blend.current_opacity.is_none()
                            {
                                continue;
                            }

                            // Scissor to this monitor's portion of the repair area.
                            let gl_y = self.screen_h as i32 - (mw.mon_y + mw.mon_h as i32);
                            let monitor_scissor =
                                (mw.mon_x, gl_y, mw.mon_w as i32, mw.mon_h as i32);
                            let Some(scissor) = (if use_scissor {
                                intersect_gl_scissors(monitor_scissor, damage_scissor)
                            } else {
                                Some(monitor_scissor)
                            }) else {
                                continue;
                            };
                            self.gl.enable(glow::SCISSOR_TEST);
                            self.gl.scissor(scissor.0, scissor.1, scissor.2, scissor.3);

                            let area = (
                                mw.mon_x as f32,
                                mw.mon_y as f32,
                                mw.mon_w as f32,
                                mw.mon_h as f32,
                            );

                            // A monitor override is independent of the global
                            // transition. Fallback outputs draw the old global
                            // image first, clipped to this monitor, then blend
                            // the new image over it.
                            if let Some(opacity) = blend.old_global_opacity
                                && let Some(texture) = self.old_wallpaper_texture
                            {
                                self.draw_wallpaper_layer(
                                    texture,
                                    self.old_wallpaper_mode,
                                    self.old_wallpaper_img_w,
                                    self.old_wallpaper_img_h,
                                    area,
                                    opacity,
                                );
                            }
                            if let Some(opacity) = blend.current_opacity {
                                if let Some(texture) = mw.texture {
                                    self.draw_wallpaper_layer(
                                        texture, mw.mode, mw.img_w, mw.img_h, area, opacity,
                                    );
                                } else if let Some(texture) = self.wallpaper_texture {
                                    self.draw_wallpaper_layer(
                                        texture,
                                        self.wallpaper_mode,
                                        self.wallpaper_img_w,
                                        self.wallpaper_img_h,
                                        area,
                                        opacity,
                                    );
                                }
                            }
                        }
                        if use_scissor {
                            self.gl.scissor(
                                damage_scissor.0,
                                damage_scissor.1,
                                damage_scissor.2,
                                damage_scissor.3,
                            );
                        } else {
                            self.gl.disable(glow::SCISSOR_TEST);
                        }
                    } else {
                        // Single global wallpaper (no monitors set yet)
                        let area = (0.0, 0.0, self.screen_w as f32, self.screen_h as f32);
                        let blend = wallpaper_blend_plan(
                            false,
                            self.wallpaper_texture.is_some(),
                            self.old_wallpaper_texture.is_some(),
                            global_transition_progress,
                        );
                        if let Some(opacity) = blend.old_global_opacity
                            && let Some(texture) = self.old_wallpaper_texture
                        {
                            self.draw_wallpaper_layer(
                                texture,
                                self.old_wallpaper_mode,
                                self.old_wallpaper_img_w,
                                self.old_wallpaper_img_h,
                                area,
                                opacity,
                            );
                        }
                        if let Some(opacity) = blend.current_opacity
                            && let Some(texture) = self.wallpaper_texture
                        {
                            self.draw_wallpaper_layer(
                                texture,
                                self.wallpaper_mode,
                                self.wallpaper_img_w,
                                self.wallpaper_img_h,
                                area,
                                opacity,
                            );
                        }
                    }

                    // Restore the shared window program state for subsequent
                    // scene draws.
                    self.gl
                        .uniform_1_f32(self.win_uniforms.opacity.as_ref(), 1.0);
                    self.gl.bind_texture(glow::TEXTURE_2D, None);
                    self.gl.bind_vertex_array(None);
                    self.gl.use_program(None);
                }
            }
        }

        // A restored X window is already at its final server-side geometry so
        // input works, but its live texture must not be drawn underneath the
        // reverse Genie mesh. Filtering once here covers shadows, blur,
        // borders and the main pass without sprinkling state checks through
        // every renderer stage.
        let restoring_scene;
        let has_restoring = self
            .genie_active
            .iter()
            .any(|animation| animation.direction == GenieDirection::Restore);
        let visible_scene = if has_restoring {
            restoring_scene = scene[first_visible..]
                .iter()
                .copied()
                .filter(|(window, ..)| {
                    !self.genie_active.iter().any(|animation| {
                        animation.x11_win == *window
                            && animation.direction == GenieDirection::Restore
                    })
                })
                .collect::<Vec<_>>();
            restoring_scene.as_slice()
        } else {
            &scene[first_visible..]
        };

        // When overview is active, skip rendering windows that belong to the
        // overview monitor — they would be hidden behind the opaque overview
        // background anyway and their presence can visually compete with the
        // 3D prism thumbnails.
        // Copy fields out so the closure does not borrow `self` across cache
        // allocation and the later render passes.
        let ov_active = self.overview_active;
        let ov_mx = self.overview_mon_x;
        let ov_my = self.overview_mon_y;
        let ov_mw = self.overview_mon_w as i32;
        let ov_mh = self.overview_mon_h as i32;
        let overview_skip = move |x: i32, y: i32, w: u32, h: u32| -> bool {
            if !ov_active {
                return false;
            }
            let cx = x + w as i32 / 2;
            let cy = y + h as i32 / 2;
            cx >= ov_mx && cx < ov_mx + ov_mw && cy >= ov_my && cy < ov_my + ov_mh
        };

        // Wallpaper and later effect passes use raw GL bindings.  Normalize
        // the actual draw state before entering tracker-managed passes so a
        // stale cached VAO/program can never suppress a required bind.
        unsafe {
            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
        }
        self.gl_state_tracker.reset_draw_bindings();

        // === Pass 1: Draw shadows (feature 14: improved shape) ===
        if self.shadow_enabled && self.shadow_radius > 0.0 {
            unsafe {
                // Phase 2: Use state tracker for shadow pass
                self.gl_state_tracker
                    .use_program(&self.gl, Some(self.shadow_program));
                self.gl.uniform_matrix_4_f32_slice(
                    self.shadow_uniforms.projection.as_ref(),
                    false,
                    &proj,
                );
                self.gl_state_tracker
                    .bind_vertex_array(&self.gl, Some(self.quad_vao));

                let spread = self.shadow_radius;
                let [ox, oy] = self.shadow_offset;
                let [sr, sg, sb, sa] = self.shadow_color;
                let bottom_extra = self.shadow_bottom_extra;

                self.gl
                    .uniform_1_f32(self.shadow_uniforms.spread.as_ref(), spread);

                let status_bar_name = frame_status_bar_name;

                for &(win, x, y, w, h) in visible_scene {
                    if overview_skip(x, y, w, h) {
                        continue;
                    }
                    let wt = match self.windows.get(&win) {
                        Some(wt) => wt,
                        None => continue,
                    };
                    // Fullscreen and explicitly bypassed clients must look
                    // identical when an overlay temporarily re-enables
                    // composition. Their shadow would also be entirely
                    // outside an output-covering rectangle.
                    if window_prefers_direct_presentation(wt.is_fullscreen, wt.bypass_compositor) {
                        continue;
                    }
                    // Skip shadow for statusbar
                    if wt.class_name == status_bar_name || wt.class_name.contains(status_bar_name) {
                        continue;
                    }
                    // Per-window shadow exclude
                    if class_matches_exclude(&wt.class_name, &self.shadow_exclude) {
                        continue;
                    }
                    // Feature 14: Skip shadow for shaped windows (non-rectangular)
                    if wt.is_shaped {
                        continue;
                    }
                    // Skip compositor shadow for RGBA windows — they manage their own shadow
                    if wt.has_rgba {
                        continue;
                    }
                    // Fade: modulate shadow alpha; unfocused windows can
                    // cast a weaker shadow so the focused one reads deeper.
                    let fade = wt.fade_opacity;
                    let focus_scale = if focused == Some(win) {
                        1.0
                    } else {
                        self.shadow_inactive_opacity
                    };
                    let sa_faded = sa * fade * focus_scale;
                    if sa_faded <= 0.0 {
                        continue;
                    }

                    self.gl.uniform_4_f32(
                        self.shadow_uniforms.shadow_color.as_ref(),
                        sr,
                        sg,
                        sb,
                        sa_faded,
                    );

                    // Feature 3: Per-window corner radius for shadow
                    let win_radius = wt.corner_radius_override.unwrap_or(
                        if class_matches_exclude(&wt.class_name, &self.rounded_corners_exclude) {
                            0.0
                        } else {
                            self.corner_radius
                        },
                    );
                    self.gl
                        .uniform_1_f32(self.shadow_uniforms.radius.as_ref(), win_radius);

                    // Feature 14: Non-uniform shadow offset (heavier bottom)
                    let sy_offset = oy + bottom_extra;
                    let anim_s = wt.anim_scale;
                    let win_w = w as f32 * anim_s;
                    let win_h = h as f32 * anim_s;
                    let cx = x as f32 + (w as f32 - win_w) * 0.5;
                    let cy = y as f32 + (h as f32 - win_h) * 0.5;
                    let mut sx = cx + ox - spread;
                    let mut sy = cy + sy_offset - spread;
                    let mut sw = win_w + 2.0 * spread;
                    let mut sh = win_h + 2.0 * spread + bottom_extra;

                    // Dynamic shadow offset for tilted focused window
                    if self.window_tilt && focused == Some(win) {
                        let tilt_mag =
                            (self.tilt_current_x.powi(2) + self.tilt_current_y.powi(2)).sqrt();
                        let extra = tilt_mag * 15.0;
                        sx += self.tilt_current_y * 30.0 - extra;
                        sy += self.tilt_current_x * 30.0 - extra;
                        sw += extra * 2.0;
                        sh += extra * 2.0;
                    }
                    self.gl
                        .uniform_4_f32(self.shadow_uniforms.rect.as_ref(), sx, sy, sw, sh);
                    self.gl
                        .uniform_2_f32(self.shadow_uniforms.size.as_ref(), win_w, win_h);
                    self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                }

                self.gl_state_tracker.bind_vertex_array(&self.gl, None);
                self.gl_state_tracker.use_program(&self.gl, None);
            }
        }

        // === Pass 1.25: Directional client-window glow underlay ===
        let glow_settings = WindowGlowSettings::from_behavior(frame_cfg.behavior());
        if glow_settings.damage_margin() > 0 {
            unsafe {
                self.gl_state_tracker
                    .use_program(&self.gl, Some(self.border_program));
                self.gl.uniform_matrix_4_f32_slice(
                    self.border_uniforms.projection.as_ref(),
                    false,
                    &proj,
                );
                self.gl_state_tracker
                    .bind_vertex_array(&self.gl, Some(self.quad_vao));

                for &(win, x, y, w, h) in visible_scene {
                    if overview_skip(x, y, w, h) {
                        continue;
                    }
                    let Some(wt) = self.windows.get(&win) else {
                        continue;
                    };
                    let is_statusbar = wt.class_name == frame_status_bar_name
                        || wt.class_name.contains(frame_status_bar_name);
                    if is_statusbar {
                        continue;
                    }

                    let fade = wt.fade_opacity * self.peek_opacity_for(&wt.class_name);
                    let Some(style) = glow_settings.style_for(WindowGlowTarget {
                        focused: focused == Some(win),
                        fullscreen: window_prefers_direct_presentation(
                            wt.is_fullscreen,
                            wt.bypass_compositor,
                        ),
                        override_redirect: wt.is_override_redirect,
                        shaped: wt.is_shaped,
                        class_name: &wt.class_name,
                        fade,
                    }) else {
                        continue;
                    };

                    let radius = wt.corner_radius_override.unwrap_or_else(|| {
                        if class_matches_exclude(&wt.class_name, &self.rounded_corners_exclude) {
                            0.0
                        } else {
                            self.corner_radius
                        }
                    });
                    let scale = wt.scale * wt.anim_scale;
                    let draw_w = w as f32 * scale;
                    let draw_h = h as f32 * scale;
                    let draw_x = x as f32 + (w as f32 - draw_w) * 0.5;
                    let draw_y = y as f32 + (h as f32 - draw_h) * 0.5;
                    if draw_w <= 0.0 || draw_h <= 0.0 {
                        continue;
                    }

                    self.gl
                        .uniform_1_f32(self.border_uniforms.border_width.as_ref(), -style.radius);
                    self.gl.uniform_4_f32(
                        self.border_uniforms.border_color.as_ref(),
                        style.color[0],
                        style.color[1],
                        style.color[2],
                        style.color[3],
                    );
                    self.set_border_radii(radius.max(0.0), radius.max(0.0));
                    // Negative border width switches the shared shader to glow
                    // mode, where u_size is the unexpanded client rectangle.
                    self.gl
                        .uniform_2_f32(self.border_uniforms.size.as_ref(), draw_w, draw_h);
                    self.gl.uniform_4_f32(
                        self.border_uniforms.rect.as_ref(),
                        draw_x - style.radius,
                        draw_y - style.radius,
                        draw_w + 2.0 * style.radius,
                        draw_h + 2.0 * style.radius,
                    );
                    self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                }

                // Avoid leaking the negative glow-mode sentinel into later
                // border-program users that do not otherwise need an outline.
                self.gl
                    .uniform_1_f32(self.border_uniforms.border_width.as_ref(), 0.0);
                self.gl_state_tracker.bind_vertex_array(&self.gl, None);
                self.gl_state_tracker.use_program(&self.gl, None);
            }
        }

        // Phase 2.2: Auto blur quality downgrade during animations/transitions.
        // Only fades on managed clients count: an override-redirect overlay
        // (fcitx5's candidate list, menus, tooltips) fading on every keystroke
        // must not pump every frosted client between Full and Reduced blur —
        // the same rule that keeps IME popups out of the smart-border count.
        if self.blur_quality_auto {
            self.blur_quality = if self.transition_active() || self.overview_active {
                BlurQuality::Minimal
            } else if fade_tick.on_clients || wobbly_active {
                BlurQuality::Reduced
            } else {
                BlurQuality::Full
            };
        }

        // === Pass 1.5: Background blur (now computed per-window in Pass 2) ===
        let blur_available =
            self.blur_enabled && !self.blur_fbos.is_empty() && self.scene_fbo.is_some();
        let mut blur_windows = Vec::new();
        if blur_available {
            blur_windows.extend(visible_scene.iter().filter_map(|&(win, x, y, w, h)| {
                if overview_skip(x, y, w, h) {
                    return None;
                }
                self.windows.get(&win).and_then(|wt| {
                    (wt.fade_opacity > 0.0 && self.needs_backdrop_blur(wt, frame_status_bar_name))
                        .then_some(win)
                })
            }));
            blur_windows.sort_unstable();
            blur_windows.dedup();
        }
        self.ensure_window_blur_caches(&blur_windows);

        // === Pass 2: Draw window textures ===
        let wm_border_px = frame_cfg.border_px() as f32;

        // Count actual client windows to apply smart borders. Only windows
        // that can carry a border themselves count, so transient overlays
        // (IME candidate lists, menus, tooltips) cannot flip a lone tiled
        // client into the bordered multi-window case.
        let status_bar_name = frame_status_bar_name;
        let client_window_count = visible_scene
            .iter()
            .filter(|&&(win, _, _, _, _)| {
                self.windows
                    .get(&win)
                    .map(|wt| {
                        counts_for_smart_borders(
                            &wt.class_name,
                            status_bar_name,
                            wt.is_override_redirect,
                        )
                    })
                    .unwrap_or(false)
            })
            .count();

        let effective_border_enabled =
            (self.border_enabled || wm_border_px > 0.0) && client_window_count > 1;
        let base_border_width = if self.border_enabled {
            self.border_width
        } else {
            wm_border_px
        };

        // Track the below-scene for blur caching. Geometry/visual state uses a
        // running hash; client texture damage stays as screen-space rectangles
        // so activity in one tiled client cannot invalidate distant clients.
        let mut blur_below_hash: u64 = 0u64;
        let mut blur_damage_below = std::mem::take(&mut self.scratch_blur_damage);
        blur_damage_below.clear();
        blur_damage_below.reserve(blur_dirty_wins.len());

        unsafe {
            // Phase 2: Use state tracker for main window rendering pass
            self.gl_state_tracker
                .use_program(&self.gl, Some(self.program));
            self.gl
                .uniform_matrix_4_f32_slice(self.win_uniforms.projection.as_ref(), false, &proj);
            self.gl.uniform_1_i32(self.win_uniforms.texture.as_ref(), 0);
            self.gl
                .uniform_4_f32(self.win_uniforms.uv_rect.as_ref(), 0.0, 0.0, 1.0, 1.0);
            self.gl
                .uniform_1_f32(self.win_uniforms.ripple_amplitude.as_ref(), 0.0);
            self.gl_state_tracker
                .bind_vertex_array(&self.gl, Some(self.quad_vao));

            let status_bar_name_main = frame_status_bar_name;

            for &(win, x, y, w, h) in visible_scene {
                if overview_skip(x, y, w, h) {
                    continue;
                }
                if let Some(wt) = self.windows.get(&win) {
                    let is_focused = focused == Some(win);
                    let fade = wt.fade_opacity;
                    if fade <= 0.0 {
                        continue;
                    }
                    let direct_candidate =
                        window_prefers_direct_presentation(wt.is_fullscreen, wt.bypass_compositor);
                    let focus_highlight_active_for_win =
                        if let Some((hw, start)) = self.focus_highlight_start {
                            hw == win
                                && start.elapsed().as_millis()
                                    < self.focus_highlight_duration_ms as u128
                        } else {
                            false
                        };
                    let attention_active_for_win =
                        attention_signal_active(self.attention_animation, wt.is_urgent);
                    let has_special_border = attention_active_for_win || wt.is_pip;

                    // Phase 5.3: Peek opacity multiplier
                    let peek_mul = self.peek_opacity_for(&wt.class_name);

                    // Feature 3: Per-window corner radius
                    // Skip compositor rounding for override-redirect RGBA windows
                    // (popups, menus, tooltips) — they manage their own shape.
                    let radius = if direct_candidate || (wt.is_override_redirect && wt.has_rgba) {
                        0.0
                    } else {
                        wt.corner_radius_override.unwrap_or(
                            if class_matches_exclude(&wt.class_name, &self.rounded_corners_exclude)
                            {
                                0.0
                            } else {
                                self.corner_radius
                            },
                        )
                    };
                    self.gl
                        .uniform_1_f32(self.win_uniforms.radius.as_ref(), radius);

                    // Compute effective opacity
                    let is_statusbar = wt.class_name == status_bar_name_main
                        || wt.class_name.contains(status_bar_name_main);

                    let base_opacity = if is_statusbar {
                        1.0
                    } else if is_focused {
                        self.active_opacity
                    } else {
                        self.inactive_opacity
                    };
                    let rule_opacity = wt.opacity_override.unwrap_or(base_opacity);
                    let inactive_dim_factor =
                        if is_statusbar || is_focused || wt.is_override_redirect {
                            1.0
                        } else {
                            self.inactive_dim
                        };
                    let dim = inactive_dim_factor;
                    let desat = if is_statusbar || is_focused || wt.is_override_redirect {
                        0.0
                    } else {
                        self.inactive_desaturate
                    };
                    let layer_opacity = (rule_opacity * fade * peek_mul).clamp(0.0, 1.0);

                    // detect_client_opacity: if window manages its own alpha, don't force opacity.
                    // The sign selects texture-alpha vs forced-opaque sampling;
                    // the magnitude always carries the complete layer opacity.
                    // This keeps premultiplied RGB and alpha on the same fade.
                    // Override-redirect RGBA windows (popups, menus, tooltips) always
                    // use their own alpha — they render their own shadows/borders.
                    let use_texture_alpha =
                        wt.has_rgba && (self.detect_client_opacity || wt.is_override_redirect);
                    let opacity = if use_texture_alpha {
                        -layer_opacity
                    } else {
                        layer_opacity
                    };
                    // Feature 4: Apply configured/per-animation scale only.
                    // Focus highlighting must not resample client content:
                    // terminal text and its insertion cursor otherwise appear
                    // to flicker on every focus transition.
                    let scale = wt.scale * wt.anim_scale;
                    let (draw_x, draw_y, draw_w, draw_h) = if (scale - 1.0).abs() > f32::EPSILON {
                        let cw = w as f32 * scale;
                        let ch = h as f32 * scale;
                        let cx = x as f32 + (w as f32 - cw) * 0.5;
                        let cy = y as f32 + (h as f32 - ch) * 0.5;
                        (cx, cy, cw, ch)
                    } else {
                        (x as f32, y as f32, w as f32, h as f32)
                    };

                    // Feature 13: Draw blurred background behind translucent windows (with frame extents mask)
                    // Blur is captured per-window so it includes all windows drawn below.
                    if blur_available {
                        if self.needs_backdrop_blur(wt, status_bar_name_main) {
                            // Compute the effective filter depth before looking
                            // up the cache: an automatic quality change must
                            // not reuse a result produced at a different level.
                            let base_levels = if wt.is_frosted {
                                self.frosted_glass_strength as usize
                            } else {
                                let monitor_id = self.get_window_monitor_id(wt.x, wt.y, wt.w, wt.h);
                                let monitor_hz = self.get_monitor_refresh_hz(monitor_id);
                                self.get_blur_strength_for_hz(monitor_hz)
                                    .unwrap_or(self.blur_strength)
                                    as usize
                            }
                            .clamp(1, self.blur_fbos.len());
                            let window_quality = self.compute_window_blur_quality(wt, focused);
                            let blur_levels = match window_quality {
                                BlurQuality::Full => base_levels,
                                BlurQuality::Reduced => (base_levels / 2).max(1),
                                BlurQuality::Minimal => 1,
                            };

                            // Feature 13: If blur_use_frame_extents, crop blur to client area.
                            // RGBA windows always use the full rect so transparent areas show blur.
                            let (bx, by, bw, bh) = if self.blur_use_frame_extents && !wt.has_rgba {
                                let [fl, fr, ft, fb] = wt.frame_extents;
                                let bx = draw_x + fl as f32;
                                let by = draw_y + ft as f32;
                                let bw = (draw_w - fl as f32 - fr as f32).max(1.0);
                                let bh = (draw_h - ft as f32 - fb as f32).max(1.0);
                                (bx, by, bw, bh)
                            } else {
                                (draw_x, draw_y, draw_w, draw_h)
                            };
                            let backdrop_dirty = dirty_below_affects_backdrop(
                                &blur_damage_below,
                                enclosing_dirty_rect(bx, by, bw, bh),
                                blur_levels,
                            );

                            // Reuse this consumer's private result when its
                            // actual backdrop sampling area is unchanged.
                            let cache_hit = self.window_blur_cache_hit(
                                win,
                                blur_below_hash,
                                blur_levels,
                                backdrop_dirty,
                            );

                            // Track blur cache statistics for diagnostics
                            if cache_hit {
                                self.frame_stats.blur_cache_hits += 1;
                            } else {
                                self.frame_stats.blur_cache_misses += 1;
                            }

                            let mut blur_tex = if cache_hit {
                                self.window_blur_cache_texture(win)
                            } else {
                                let blur_bench_start = if self.benchmark.is_running() {
                                    Some(std::time::Instant::now())
                                } else {
                                    None
                                };

                                // Temporarily break out of the window shader to run blur passes.
                                // Capture the current framebuffer (which includes all windows
                                // drawn so far) and produce a blurred texture from it.
                                self.gl_state_tracker.bind_vertex_array(&self.gl, None);
                                self.gl_state_tracker.use_program(&self.gl, None);
                                if use_scissor {
                                    self.gl.disable(glow::SCISSOR_TEST);
                                }

                                let tex = self.run_blur_passes_from_fbo(
                                    if postprocess_active {
                                        self.postprocess_fbo.as_ref().map(|(fbo, _)| *fbo)
                                    } else {
                                        None
                                    },
                                    blur_levels,
                                );

                                if let Some(start) = blur_bench_start {
                                    let pixel_count: u64 = self
                                        .blur_fbos
                                        .iter()
                                        .take(blur_levels)
                                        .map(|l| l.w as u64 * l.h as u64)
                                        .sum();
                                    self.benchmark.record_blur_cost(
                                        pixel_count,
                                        start.elapsed().as_secs_f32() * 1000.0,
                                    );
                                }

                                // Restore state for window drawing
                                if use_scissor {
                                    self.gl.enable(glow::SCISSOR_TEST);
                                }
                                if postprocess_active {
                                    let (pp_fbo, _) = self.postprocess_fbo.as_ref().unwrap();
                                    self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(*pp_fbo));
                                } else {
                                    self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                                }
                                self.gl
                                    .viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
                                // Phase 2: Restore state via tracker after blur
                                self.gl_state_tracker
                                    .use_program(&self.gl, Some(self.program));
                                self.gl.uniform_matrix_4_f32_slice(
                                    self.win_uniforms.projection.as_ref(),
                                    false,
                                    &proj,
                                );
                                self.gl.uniform_1_i32(self.win_uniforms.texture.as_ref(), 0);
                                self.gl.uniform_4_f32(
                                    self.win_uniforms.uv_rect.as_ref(),
                                    0.0,
                                    0.0,
                                    1.0,
                                    1.0,
                                );
                                self.gl_state_tracker
                                    .bind_vertex_array(&self.gl, Some(self.quad_vao));
                                self.gl
                                    .uniform_1_f32(self.win_uniforms.radius.as_ref(), radius);

                                tex
                            };

                            // Copy the result into this window's private cache.
                            // Temporal mixing, when enabled, only reads that
                            // same window's previous result.
                            if let Some(blurred) = blur_tex {
                                let final_blur = if !cache_hit {
                                    let (cached, temporal_reused) = self.update_window_blur_cache(
                                        win,
                                        blurred,
                                        blur_below_hash,
                                        blur_levels,
                                    );
                                    if self.temporal_blur_enabled {
                                        self.temporal_blur_total_count += 1;
                                        if temporal_reused {
                                            self.temporal_blur_reuse_count += 1;
                                        }
                                    }
                                    // update_window_blur_cache deliberately
                                    // leaves the raw program/VAO unbound.
                                    self.gl_state_tracker.reset_draw_bindings();
                                    // Restore framebuffer + window-shader state for the
                                    // backdrop-quad draw that follows: the mix function
                                    // changes program/VAO/active framebuffer.
                                    if postprocess_active {
                                        let (pp_fbo, _) = self.postprocess_fbo.as_ref().unwrap();
                                        self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(*pp_fbo));
                                    } else {
                                        self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                                    }
                                    self.gl.viewport(
                                        0,
                                        0,
                                        self.screen_w as i32,
                                        self.screen_h as i32,
                                    );
                                    self.gl_state_tracker
                                        .use_program(&self.gl, Some(self.program));
                                    self.gl.uniform_matrix_4_f32_slice(
                                        self.win_uniforms.projection.as_ref(),
                                        false,
                                        &proj,
                                    );
                                    self.gl.uniform_1_i32(self.win_uniforms.texture.as_ref(), 0);
                                    self.gl.uniform_4_f32(
                                        self.win_uniforms.uv_rect.as_ref(),
                                        0.0,
                                        0.0,
                                        1.0,
                                        1.0,
                                    );
                                    self.gl_state_tracker
                                        .bind_vertex_array(&self.gl, Some(self.quad_vao));
                                    self.gl
                                        .uniform_1_f32(self.win_uniforms.radius.as_ref(), radius);
                                    cached
                                } else {
                                    blurred
                                };
                                blur_tex = Some(final_blur);
                            }

                            if let Some(blur_tex) = blur_tex {
                                self.gl.active_texture(glow::TEXTURE0);
                                self.gl.bind_texture(glow::TEXTURE_2D, Some(blur_tex));
                                let uv_x = (bx / self.screen_w as f32).clamp(0.0, 1.0);
                                let uv_w = (bw / self.screen_w as f32).clamp(0.0, 1.0);
                                let uv_y_top = (by / self.screen_h as f32).clamp(0.0, 1.0);
                                let uv_h = (bh / self.screen_h as f32).clamp(0.0, 1.0);
                                self.gl.uniform_4_f32(
                                    self.win_uniforms.uv_rect.as_ref(),
                                    uv_x,
                                    uv_y_top,
                                    uv_w,
                                    uv_h,
                                );
                                self.gl
                                    .uniform_1_f32(self.win_uniforms.opacity.as_ref(), fade);
                                self.gl.uniform_1_f32(self.win_uniforms.dim.as_ref(), 1.0);
                                self.gl.uniform_1_f32(self.win_uniforms.desat.as_ref(), 0.0);
                                self.gl
                                    .uniform_2_f32(self.win_uniforms.size.as_ref(), bw, bh);
                                self.gl.uniform_4_f32(
                                    self.win_uniforms.rect.as_ref(),
                                    bx,
                                    by,
                                    bw,
                                    bh,
                                );
                                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                                // Restore default UV for regular window textures.
                                self.gl.uniform_4_f32(
                                    self.win_uniforms.uv_rect.as_ref(),
                                    0.0,
                                    0.0,
                                    1.0,
                                    1.0,
                                );
                            }
                        }
                    }

                    // Phase 3.1: Motion trail ghost copies at historical positions
                    if self.motion_trail_enabled && !wt.motion_trail.is_empty() {
                        let trail_params = self.motion_trail_params(wt);
                        self.gl.uniform_1_f32(self.win_uniforms.dim.as_ref(), 0.7);
                        self.gl.uniform_1_f32(self.win_uniforms.desat.as_ref(), 0.0);
                        self.gl.active_texture(glow::TEXTURE0);
                        self.gl.bind_texture(glow::TEXTURE_2D, Some(wt.gl_texture));
                        for ghost in wt.motion_trail.ghosts(
                            std::time::Instant::now(),
                            &trail_params,
                            draw_w,
                            draw_h,
                        ) {
                            let trail_layer = (ghost.opacity * layer_opacity).clamp(0.0, 1.0);
                            self.gl.uniform_1_f32(
                                self.win_uniforms.opacity.as_ref(),
                                if use_texture_alpha {
                                    -trail_layer
                                } else {
                                    trail_layer
                                },
                            );
                            self.gl.uniform_4_f32(
                                self.win_uniforms.rect.as_ref(),
                                ghost.x,
                                ghost.y,
                                ghost.width,
                                ghost.height,
                            );
                            self.gl.uniform_2_f32(
                                self.win_uniforms.size.as_ref(),
                                ghost.width,
                                ghost.height,
                            );
                            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                        }
                    }

                    self.gl.active_texture(glow::TEXTURE0);
                    self.gl.bind_texture(glow::TEXTURE_2D, Some(wt.gl_texture));

                    // The regular window shader owns the ripple distortion.
                    // Prefer it for the short open animation rather than
                    // silently advancing an invisible ripple behind the
                    // mutually-exclusive wobbly/tilt geometry passes.
                    let ripple_prog =
                        self.ripple_active
                            .iter()
                            .find(|r| r.x11_win == win)
                            .map(|r| {
                                let elapsed = r.start.elapsed().as_secs_f32();
                                (elapsed / self.ripple_duration.max(f32::EPSILON)).min(1.0)
                            });

                    // Wobbly windows: use grid spring-mass deformation shader
                    if self.wobbly_windows && wt.wobbly.is_some() && ripple_prog.is_none() {
                        let wobbly = wt.wobbly.as_ref().unwrap();
                        self.gl.use_program(Some(self.wobbly_program));
                        self.gl.uniform_matrix_4_f32_slice(
                            self.wobbly_uniforms.projection.as_ref(),
                            false,
                            &proj,
                        );
                        self.gl.uniform_4_f32(
                            self.wobbly_uniforms.rect.as_ref(),
                            draw_x,
                            draw_y,
                            draw_w,
                            draw_h,
                        );
                        self.gl
                            .uniform_1_i32(self.wobbly_uniforms.texture.as_ref(), 0);
                        self.gl
                            .uniform_1_f32(self.wobbly_uniforms.opacity.as_ref(), opacity);
                        self.gl
                            .uniform_1_f32(self.wobbly_uniforms.radius.as_ref(), radius);
                        self.gl
                            .uniform_2_f32(self.wobbly_uniforms.size.as_ref(), draw_w, draw_h);
                        self.gl
                            .uniform_1_f32(self.wobbly_uniforms.dim.as_ref(), dim);
                        self.gl.uniform_4_f32(
                            self.wobbly_uniforms.uv_rect.as_ref(),
                            0.0,
                            0.0,
                            1.0,
                            1.0,
                        );
                        // Upload grid offsets as flat vec2 array
                        self.scratch_wobbly_flat.clear();
                        self.scratch_wobbly_flat.reserve(wobbly.offsets.len() * 2);
                        for offset in &wobbly.offsets {
                            self.scratch_wobbly_flat.push(offset[0]);
                            self.scratch_wobbly_flat.push(offset[1]);
                        }
                        self.gl.uniform_2_f32_slice(
                            self.wobbly_uniforms.grid_offsets.as_ref(),
                            &self.scratch_wobbly_flat,
                        );
                        let grid_n = wobbly.grid_n as i32;
                        self.gl
                            .uniform_1_i32(self.wobbly_uniforms.grid_n.as_ref(), grid_n);
                        // Grid: (grid_n-1)^2 quads, 6 verts each
                        let quads = grid_n - 1;
                        self.gl.draw_arrays(glow::TRIANGLES, 0, quads * quads * 6);

                        // Restore standard window program
                        self.gl.use_program(Some(self.program));
                        self.gl.uniform_matrix_4_f32_slice(
                            self.win_uniforms.projection.as_ref(),
                            false,
                            &proj,
                        );
                        self.gl.uniform_1_i32(self.win_uniforms.texture.as_ref(), 0);
                        self.gl.uniform_4_f32(
                            self.win_uniforms.uv_rect.as_ref(),
                            0.0,
                            0.0,
                            1.0,
                            1.0,
                        );
                        self.gl
                            .uniform_1_f32(self.win_uniforms.radius.as_ref(), radius);
                    } else if self.window_tilt
                        && is_focused
                        && !is_statusbar
                        && ripple_prog.is_none()
                    {
                        // Update tilt target from mouse position (clamped)
                        let cx = draw_x + draw_w * 0.5;
                        let cy = draw_y + draw_h * 0.5;
                        let rel_x = ((self.mouse_x - cx) / (draw_w * 0.5)).clamp(-1.0, 1.0);
                        let rel_y = ((self.mouse_y - cy) / (draw_h * 0.5)).clamp(-1.0, 1.0);
                        self.tilt_target_x = (-rel_y * self.tilt_amount).clamp(-0.35, 0.35);
                        self.tilt_target_y = (rel_x * self.tilt_amount).clamp(-0.35, 0.35);

                        self.gl.use_program(Some(self.tilt_program));
                        self.gl.uniform_matrix_4_f32_slice(
                            self.tilt_uniforms.projection.as_ref(),
                            false,
                            &proj,
                        );
                        self.gl.uniform_4_f32(
                            self.tilt_uniforms.rect.as_ref(),
                            draw_x,
                            draw_y,
                            draw_w,
                            draw_h,
                        );
                        self.gl
                            .uniform_1_i32(self.tilt_uniforms.texture.as_ref(), 0);
                        self.gl
                            .uniform_1_f32(self.tilt_uniforms.opacity.as_ref(), opacity);
                        self.gl
                            .uniform_1_f32(self.tilt_uniforms.radius.as_ref(), radius);
                        self.gl
                            .uniform_2_f32(self.tilt_uniforms.size.as_ref(), draw_w, draw_h);
                        self.gl.uniform_1_f32(self.tilt_uniforms.dim.as_ref(), dim);
                        self.gl.uniform_4_f32(
                            self.tilt_uniforms.uv_rect.as_ref(),
                            0.0,
                            0.0,
                            1.0,
                            1.0,
                        );
                        self.gl.uniform_2_f32(
                            self.tilt_uniforms.tilt.as_ref(),
                            self.tilt_current_x,
                            self.tilt_current_y,
                        );
                        self.gl.uniform_1_f32(
                            self.tilt_uniforms.perspective.as_ref(),
                            self.tilt_perspective,
                        );
                        let grid = self.tilt_grid as i32;
                        self.gl
                            .uniform_1_i32(self.tilt_uniforms.grid_size.as_ref(), grid);
                        self.gl
                            .uniform_2_f32(self.tilt_uniforms.light_dir.as_ref(), 0.0, -1.0);
                        // Grid: grid^2 quads, 6 verts each
                        self.gl.draw_arrays(glow::TRIANGLES, 0, grid * grid * 6);

                        // Restore standard window program
                        self.gl.use_program(Some(self.program));
                        self.gl.uniform_matrix_4_f32_slice(
                            self.win_uniforms.projection.as_ref(),
                            false,
                            &proj,
                        );
                        self.gl.uniform_1_i32(self.win_uniforms.texture.as_ref(), 0);
                        self.gl.uniform_4_f32(
                            self.win_uniforms.uv_rect.as_ref(),
                            0.0,
                            0.0,
                            1.0,
                            1.0,
                        );
                        self.gl
                            .uniform_1_f32(self.win_uniforms.radius.as_ref(), radius);
                    } else {
                        self.gl
                            .uniform_1_f32(self.win_uniforms.opacity.as_ref(), opacity);
                        self.gl.uniform_1_f32(self.win_uniforms.dim.as_ref(), dim);
                        self.gl
                            .uniform_1_f32(self.win_uniforms.desat.as_ref(), desat);
                        self.gl
                            .uniform_2_f32(self.win_uniforms.size.as_ref(), draw_w, draw_h);
                        self.gl.uniform_4_f32(
                            self.win_uniforms.rect.as_ref(),
                            draw_x,
                            draw_y,
                            draw_w,
                            draw_h,
                        );

                        // Window-open ripple: set per-window distortion uniforms
                        if let Some(progress) = ripple_prog {
                            self.gl.uniform_1_f32(
                                self.win_uniforms.ripple_progress.as_ref(),
                                progress,
                            );
                            self.gl.uniform_1_f32(
                                self.win_uniforms.ripple_amplitude.as_ref(),
                                self.ripple_amplitude,
                            );
                        } else {
                            self.gl
                                .uniform_1_f32(self.win_uniforms.ripple_amplitude.as_ref(), 0.0);
                        }

                        self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                        // Reset ripple for next window
                        if ripple_prog.is_some() {
                            self.gl
                                .uniform_1_f32(self.win_uniforms.ripple_amplitude.as_ref(), 0.0);
                        }
                    }

                    // Same predicate that fed `client_window_count`: only
                    // windows counted for smart borders can receive one.
                    if counts_for_smart_borders(
                        &wt.class_name,
                        status_bar_name_main,
                        wt.is_override_redirect,
                    ) && !direct_candidate
                        && ((effective_border_enabled && base_border_width > 0.0)
                            || has_special_border)
                    {
                        let focus_style = focus_highlight_active_for_win.then(|| {
                            let elapsed_ms =
                                self.focus_highlight_start.unwrap().1.elapsed().as_millis() as f32;
                            let dur = self.focus_highlight_duration_ms as f32;
                            focus_highlight_style(
                                self.border_color_focused,
                                self.focus_highlight_color,
                                base_border_width,
                                elapsed_ms / dur,
                            )
                        });
                        let attention_style = attention_active_for_win.then(|| {
                            attention_border_style(
                                self.attention_color,
                                self.compositor_start_time.elapsed().as_secs_f32(),
                                1.0,
                                effective_border_enabled,
                                base_border_width,
                            )
                        });
                        let color = if let Some(style) = focus_style {
                            style.color
                        } else if let Some(style) = attention_style {
                            style.color
                        } else if wt.is_pip {
                            self.pip_border_color
                        } else if is_focused {
                            self.border_color_focused
                        } else {
                            self.border_color_unfocused
                        };

                        let bw = if let Some(style) = focus_style {
                            style.width
                        } else if let Some(style) = attention_style {
                            style.width
                        } else if wt.is_pip {
                            self.pip_border_width
                        } else {
                            base_border_width
                        };

                        if bw > 0.0 {
                            let bdr_x = draw_x - bw;
                            let bdr_y = draw_y - bw;
                            let bdr_w = draw_w + 2.0 * bw;
                            let bdr_h = draw_h + 2.0 * bw;
                            // Concentric corners: the ring's inner edge sits bw
                            // inside the outer rect, so the outer radius must be
                            // radius + bw for the inner curve to match the
                            // window's radius (no wedge gap at corners).
                            let outer_radius = if radius > 0.0 { radius + bw } else { 0.0 };

                            // The focused window's ordinary border upgrades to
                            // the two-color gradient ring. Special borders
                            // (focus pulse, attention, PiP) keep their flat
                            // signal colors.
                            let use_gradient = self.border_gradient_enabled
                                && is_focused
                                && focus_style.is_none()
                                && !attention_active_for_win
                                && !wt.is_pip;

                            if use_gradient {
                                let angle = (self.border_gradient_angle
                                    + self.border_gradient_speed
                                        * self.compositor_start_time.elapsed().as_secs_f32())
                                .to_radians();
                                let [ar, ag, ab, aa] = self.border_gradient_color_a;
                                let [br, bg, bb, ba] = self.border_gradient_color_b;
                                self.gl.use_program(Some(self.gradient_border_program));
                                self.gl.uniform_matrix_4_f32_slice(
                                    self.gradient_border_uniforms.projection.as_ref(),
                                    false,
                                    &proj,
                                );
                                self.gl.uniform_1_f32(
                                    self.gradient_border_uniforms.border_width.as_ref(),
                                    bw,
                                );
                                self.gl.uniform_4_f32(
                                    self.gradient_border_uniforms.color_a.as_ref(),
                                    ar,
                                    ag,
                                    ab,
                                    aa * fade,
                                );
                                self.gl.uniform_4_f32(
                                    self.gradient_border_uniforms.color_b.as_ref(),
                                    br,
                                    bg,
                                    bb,
                                    ba * fade,
                                );
                                self.gl.uniform_1_f32(
                                    self.gradient_border_uniforms.gradient_angle.as_ref(),
                                    angle,
                                );
                                self.set_gradient_border_radii(outer_radius, outer_radius);
                                self.gl.uniform_2_f32(
                                    self.gradient_border_uniforms.size.as_ref(),
                                    bdr_w,
                                    bdr_h,
                                );
                                self.gl.uniform_4_f32(
                                    self.gradient_border_uniforms.rect.as_ref(),
                                    bdr_x,
                                    bdr_y,
                                    bdr_w,
                                    bdr_h,
                                );
                            } else {
                                self.gl.use_program(Some(self.border_program));
                                self.gl.uniform_matrix_4_f32_slice(
                                    self.border_uniforms.projection.as_ref(),
                                    false,
                                    &proj,
                                );
                                self.gl
                                    .uniform_1_f32(self.border_uniforms.border_width.as_ref(), bw);
                                self.gl.uniform_4_f32(
                                    self.border_uniforms.border_color.as_ref(),
                                    color[0],
                                    color[1],
                                    color[2],
                                    color[3] * fade,
                                );
                                self.set_border_radii(outer_radius, outer_radius);
                                self.gl.uniform_2_f32(
                                    self.border_uniforms.size.as_ref(),
                                    bdr_w,
                                    bdr_h,
                                );
                                self.gl.uniform_4_f32(
                                    self.border_uniforms.rect.as_ref(),
                                    bdr_x,
                                    bdr_y,
                                    bdr_w,
                                    bdr_h,
                                );
                            }
                            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                            self.gl.use_program(Some(self.program));
                            self.gl.uniform_matrix_4_f32_slice(
                                self.win_uniforms.projection.as_ref(),
                                false,
                                &proj,
                            );
                            self.gl.uniform_1_i32(self.win_uniforms.texture.as_ref(), 0);
                            self.gl.uniform_4_f32(
                                self.win_uniforms.uv_rect.as_ref(),
                                0.0,
                                0.0,
                                1.0,
                                1.0,
                            );
                            self.gl
                                .uniform_1_f32(self.win_uniforms.radius.as_ref(), radius);
                        }
                    }

                    // Update the dependency key seen by blur consumers above
                    // this window. Besides stacking/geometry, include visual
                    // state that can change without texture damage (focus,
                    // opacity, dimming, fades, scale, rounded clipping and glow).
                    let glow_style = if is_statusbar {
                        None
                    } else {
                        glow_settings.style_for(WindowGlowTarget {
                            focused: is_focused,
                            fullscreen: window_prefers_direct_presentation(
                                wt.is_fullscreen,
                                wt.bypass_compositor,
                            ),
                            override_redirect: wt.is_override_redirect,
                            shaped: wt.is_shaped,
                            class_name: &wt.class_name,
                            fade: fade * peek_mul,
                        })
                    };
                    let glow_hash = glow_style
                        .map(WindowGlowStyle::hash_words)
                        .unwrap_or([0; 3]);
                    for value in [
                        win as u64,
                        ((x as u64) << 32) | y as u32 as u64,
                        ((w as u64) << 32) | h as u64,
                        ((draw_x.to_bits() as u64) << 32) | draw_y.to_bits() as u64,
                        ((draw_w.to_bits() as u64) << 32) | draw_h.to_bits() as u64,
                        ((opacity.to_bits() as u64) << 32) | dim.to_bits() as u64,
                        ((fade.to_bits() as u64) << 32) | radius.to_bits() as u64,
                        u64::from(is_focused) | (u64::from(has_special_border) << 1),
                        glow_hash[0],
                        glow_hash[1],
                        glow_hash[2],
                    ] {
                        blur_below_hash = blur_below_hash
                            .wrapping_mul(6364136223846793005)
                            .wrapping_add(value);
                    }
                    if blur_dirty_wins.binary_search(&win).is_ok() {
                        blur_damage_below
                            .push(enclosing_dirty_rect(draw_x, draw_y, draw_w, draw_h));
                    }
                }
            }

            self.gl_state_tracker.bind_vertex_array(&self.gl, None);
            self.gl_state_tracker.use_program(&self.gl, None);
        }

        // === Pass 2b: Genie minimize animations ===
        let genie_frame_sample_time = (!self.genie_active.is_empty()).then(std::time::Instant::now);
        if let Some(now) = genie_frame_sample_time {
            let duration_secs = self.genie_duration_ms.max(1) as f32 / 1000.0;
            unsafe {
                self.gl.use_program(Some(self.genie_program));
                self.gl.uniform_matrix_4_f32_slice(
                    self.genie_uniforms.projection.as_ref(),
                    false,
                    &proj,
                );
                self.gl
                    .uniform_1_i32(self.genie_uniforms.texture.as_ref(), 0);
                self.gl
                    .uniform_4_f32(self.genie_uniforms.uv_rect.as_ref(), 0.0, 0.0, 1.0, 1.0);
                self.gl
                    .uniform_1_f32(self.genie_uniforms.radius.as_ref(), 0.0);
                let grid = 12i32;
                self.gl
                    .uniform_1_i32(self.genie_uniforms.grid_size.as_ref(), grid);
                self.gl.bind_vertex_array(Some(self.quad_vao));

                for ga in &self.genie_active {
                    let (progress, _) =
                        super::effects::genie_animation_progress(ga, now, duration_secs);
                    let opacity = 1.0 - progress;
                    let dock = ga.target.center();
                    self.gl.uniform_4_f32(
                        self.genie_uniforms.rect.as_ref(),
                        ga.x,
                        ga.y,
                        ga.w,
                        ga.h,
                    );
                    self.gl
                        .uniform_2_f32(self.genie_uniforms.size.as_ref(), ga.w, ga.h);
                    self.gl
                        .uniform_1_f32(self.genie_uniforms.progress.as_ref(), progress);
                    self.gl
                        .uniform_2_f32(self.genie_uniforms.dock_pos.as_ref(), dock.0, dock.1);
                    self.gl.uniform_2_f32(
                        self.genie_uniforms.dock_size.as_ref(),
                        ga.target.width as f32,
                        ga.target.height as f32,
                    );
                    self.gl.uniform_1_f32(
                        self.genie_uniforms.opacity.as_ref(),
                        if ga.has_rgba { -opacity } else { opacity },
                    );
                    self.gl.uniform_1_f32(self.genie_uniforms.dim.as_ref(), 1.0);
                    self.gl.active_texture(glow::TEXTURE0);
                    self.gl.bind_texture(glow::TEXTURE_2D, Some(ga.gl_texture));
                    self.gl.draw_arrays(glow::TRIANGLES, 0, grid * grid * 6);
                }

                self.gl.bind_vertex_array(None);
                self.gl.use_program(None);
            }
        }

        self.render_minimized_dock_items(&proj);
        self.render_dock_preview(&proj);

        // === Pass 3c: Window tab bars ===
        if self.window_tabs_enabled && !self.window_groups.is_empty() {
            self.refresh_tab_titles();
            self.render_tab_bar(&proj);
        }

        // === Pass 4: Post-processing (features 8/9/10) ===
        if postprocess_active {
            let (_, pp_tex) = self.postprocess_fbo.as_ref().unwrap();
            let pp_tex = *pp_tex;
            unsafe {
                // Switch back to default framebuffer
                self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                self.gl
                    .viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
                self.gl.clear(glow::COLOR_BUFFER_BIT);

                self.gl.use_program(Some(self.postprocess_program));
                // Set up fullscreen quad
                let pp_proj = ortho(
                    0.0,
                    self.screen_w as f32,
                    self.screen_h as f32,
                    0.0,
                    -1.0,
                    1.0,
                );
                // P5F.1: Use cached uniform locations (no per-frame driver call)
                self.gl.uniform_matrix_4_f32_slice(
                    self.postprocess_uniforms.projection.as_ref(),
                    false,
                    &pp_proj,
                );
                self.gl.uniform_4_f32(
                    self.postprocess_uniforms.rect.as_ref(),
                    0.0,
                    0.0,
                    self.screen_w as f32,
                    self.screen_h as f32,
                );

                self.gl
                    .uniform_1_i32(self.postprocess_uniforms.texture.as_ref(), 0);
                self.gl.uniform_1_f32(
                    self.postprocess_uniforms.color_temp.as_ref(),
                    self.color_temperature,
                );
                self.gl.uniform_1_f32(
                    self.postprocess_uniforms.saturation.as_ref(),
                    self.saturation,
                );
                self.gl.uniform_1_f32(
                    self.postprocess_uniforms.brightness.as_ref(),
                    self.brightness,
                );
                self.gl
                    .uniform_1_f32(self.postprocess_uniforms.contrast.as_ref(), self.contrast);
                self.gl.uniform_1_i32(
                    self.postprocess_uniforms.invert.as_ref(),
                    if self.invert_colors { 1 } else { 0 },
                );
                self.gl.uniform_1_i32(
                    self.postprocess_uniforms.grayscale.as_ref(),
                    if self.grayscale { 1 } else { 0 },
                );

                // HDR tone mapping uniforms
                self.gl.uniform_1_i32(
                    self.postprocess_uniforms.hdr_enabled.as_ref(),
                    if self.hdr_enabled { 1 } else { 0 },
                );
                self.gl.uniform_1_f32(
                    self.postprocess_uniforms.hdr_peak_nits.as_ref(),
                    self.hdr_peak_nits,
                );
                self.gl.uniform_1_i32(
                    self.postprocess_uniforms.tone_mapping_method.as_ref(),
                    self.tone_mapping_method,
                );
                self.gl
                    .uniform_1_i32(self.postprocess_uniforms.eotf_mode.as_ref(), self.eotf_mode);
                self.gl.uniform_1_i32(
                    self.postprocess_uniforms.output_colorspace.as_ref(),
                    self.output_colorspace,
                );

                // Magnifier uniforms
                self.gl.uniform_1_i32(
                    self.magnifier_uniforms.magnifier_enabled.as_ref(),
                    if self.magnifier_enabled { 1 } else { 0 },
                );
                if self.magnifier_enabled {
                    let cx = self.mouse_x / self.screen_w as f32;
                    let cy = self.mouse_y / self.screen_h as f32;
                    // The fragment shader flips Y (uv.y = 1.0 - v_uv.y) so that
                    // uv.y=1 corresponds to the top of the screen.  Flip cy to match.
                    self.gl.uniform_2_f32(
                        self.magnifier_uniforms.magnifier_center.as_ref(),
                        cx,
                        1.0 - cy,
                    );
                    self.gl.uniform_1_f32(
                        self.magnifier_uniforms.magnifier_radius.as_ref(),
                        self.magnifier_radius,
                    );
                    self.gl.uniform_1_f32(
                        self.magnifier_uniforms.magnifier_zoom.as_ref(),
                        self.magnifier_zoom,
                    );
                }

                // Colorblind correction uniform
                self.gl.uniform_1_i32(
                    self.magnifier_uniforms.colorblind_mode.as_ref(),
                    self.colorblind_mode,
                );

                self.gl.active_texture(glow::TEXTURE0);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(pp_tex));
                self.gl.bind_vertex_array(Some(self.quad_vao));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                self.gl.bind_vertex_array(None);
                self.gl.use_program(None);
            }
        }

        // === Pass 4b: WaterLily native-size compositor layer ===
        // Draw after client post-processing so the simulation never changes
        // client sampling, magnification, accessibility filters, or HDR state.
        // The Composite Overlay Window has an empty input shape, so the quad
        // remains click-through and cannot take keyboard focus.
        let waterlily_backdrop = self.prepare_waterlily_backdrop(use_scissor);
        self.render_waterlily_layer(&proj, waterlily_backdrop);

        // Tick tilt after the render loop has set tilt_target from the focused window.
        // If no focused window set tilt_target this frame, it keeps 0 from the reset
        // at the start of the loop (see the tilt branch which sets tilt_target_x/y).
        let tilt_animating = self.tick_tilt(effect_dt);
        self.effect_tick_clock.finish_frame(
            std::time::Instant::now(),
            fades_active || particles_active || tilt_animating,
        );
        if tilt_animating {
            self.needs_render = true;
        }

        // === Always update frame stats (decoupled from HUD rendering) ===
        {
            let now = std::time::Instant::now();
            let dt = now
                .duration_since(self.frame_stats.last_frame_time)
                .as_secs_f32();
            self.frame_stats.last_frame_time = now;
            self.frame_stats.frame_count += 1;
            self.frame_stats.frame_times.push_back(dt);
            if self.frame_stats.frame_times.len() > 120 {
                self.frame_stats.frame_times.pop_front();
            }
            let elapsed = now
                .duration_since(self.frame_stats.last_fps_update)
                .as_secs_f32();
            if elapsed >= 1.0 {
                self.frame_stats.fps = self.frame_stats.frame_times.len() as f32 / elapsed;
                self.frame_stats.frame_times.clear();
                self.frame_stats.last_fps_update = now;
            }
            self.record_latency_sample();
        }

        // === Pass 5: Debug HUD (feature 11) ===
        if self.debug_hud {
            self.sys_stats.maybe_sample();

            // Format HUD text
            let avg_dt = if self.frame_stats.frame_times.is_empty() {
                0.0
            } else {
                self.frame_stats.frame_times.iter().sum::<f32>()
                    / self.frame_stats.frame_times.len() as f32
            };
            let max_dt = self
                .frame_stats
                .frame_times
                .iter()
                .copied()
                .fold(0.0, f32::max);
            let min_dt = self
                .frame_stats
                .frame_times
                .iter()
                .copied()
                .fold(f32::MAX, f32::min);
            let min_dt = if min_dt == f32::MAX { 0.0 } else { min_dt };

            let mut rows = hud::HudRows::default();
            rows.section("Frame");
            rows.stat("FPS", format!("{:.1}", self.frame_stats.fps));
            rows.stat(
                "Frame time",
                format!(
                    "{:.1} ms  ({:.1} / {:.1} min-max)",
                    avg_dt * 1000.0,
                    min_dt * 1000.0,
                    max_dt * 1000.0
                ),
            );
            rows.section("Scene");
            rows.stat("Windows", self.windows.len());
            rows.stat("Damage tiles", self.damage_tracker.tile_count());
            rows.stat(
                "Dirty area",
                format!("{:.0} %", self.damage_tracker.dirty_fraction() * 100.0),
            );
            rows.section("System");
            rows.stat("Memory", format!("{:.1} MiB RSS", self.sys_stats.rss_mib()));
            rows.stat("CPU", format!("{:.1} %", self.sys_stats.cpu_pct()));
            if self.debug_hud_extended {
                let tex_mem_kb = self.frame_stats.texture_memory_bytes / 1024;
                let blur_hit_rate =
                    if self.frame_stats.blur_cache_hits + self.frame_stats.blur_cache_misses > 0 {
                        100.0 * self.frame_stats.blur_cache_hits as f32
                            / (self.frame_stats.blur_cache_hits
                                + self.frame_stats.blur_cache_misses)
                                as f32
                    } else {
                        0.0
                    };
                rows.section("Render");
                rows.stat("Draw calls", self.frame_stats.draw_calls);
                rows.stat("Texture memory", format!("{tex_mem_kb} KB"));
                rows.stat(
                    "Blur cache",
                    format!(
                        "{:.0} % hit ({}/{})",
                        blur_hit_rate,
                        self.frame_stats.blur_cache_hits,
                        self.frame_stats.blur_cache_misses
                    ),
                );
                rows.stat("Blur quality", format!("{:?}", self.blur_quality));

                // Add input latency stats if available
                let (avg, p50, p95, p99) = self.compute_latency_stats();
                if avg > 0.0 {
                    rows.section("Input latency");
                    rows.stat("Average", format!("{avg:.1} ms"));
                    rows.stat(
                        "p50 / p95 / p99",
                        format!("{p50:.1} / {p95:.1} / {p99:.1} ms"),
                    );
                }

                // Per-zone profiler breakdown
                let zones_map = self.frame_profiler.all_zone_stats();
                if !zones_map.is_empty() {
                    rows.section("Profiler (avg / min / max ms)");
                    let mut zones: Vec<_> = zones_map.into_iter().collect();
                    zones.sort_by(|a, b| a.0.cmp(b.0));
                    for (name, zs) in zones {
                        rows.stat(
                            name,
                            format!("{:.2} / {:.2} / {:.2}", zs.avg_ms, zs.min_ms, zs.max_ms),
                        );
                    }
                }
            }

            // Rasterize the sections (skips upload if nothing changed), then
            // draw the Material card around them.
            let title = format!("{}  JWM Compositor", hud::TITLE_ICON);
            let chip = if self.debug_hud_extended {
                "x11 · extended"
            } else {
                "x11"
            };
            self.update_hud_textures(&title, chip, &rows);
            let target = self
                .monitor_refresh_rates
                .values()
                .copied()
                .max()
                .unwrap_or(60) as f32;
            let (meter, tone) = hud::fps_meter(self.frame_stats.fps, target);
            self.render_debug_hud_card(&proj, meter, tone);

            // Log stats periodically
            if self.frame_stats.frame_count % 60 == 0 {
                if self.debug_hud_extended {
                    let tex_mem_kb = self.frame_stats.texture_memory_bytes / 1024;
                    log::info!(
                        "[HUD] FPS: {:.1}, frame_time: {:.2}ms, windows: {}, draw_calls: {}, tex_mem: {}KB, blur_hits: {}, blur_misses: {}",
                        self.frame_stats.fps,
                        avg_dt * 1000.0,
                        self.windows.len(),
                        self.frame_stats.draw_calls,
                        tex_mem_kb,
                        self.frame_stats.blur_cache_hits,
                        self.frame_stats.blur_cache_misses,
                    );
                    self.frame_stats.draw_calls = 0;
                } else {
                    log::info!(
                        "[HUD] FPS: {:.1}, frame_time: {:.2}ms, windows: {}",
                        self.frame_stats.fps,
                        avg_dt * 1000.0,
                        self.windows.len()
                    );
                }
            }
        }

        // === Pass 5b: Screen edge glow ===
        // Tick the countdown so the glow expires even without new mouse events.
        if self.edge_glow {
            self.edge_glow_tick(self.mouse_x, self.mouse_y);
        }
        if self.edge_glow_active && self.edge_glow_width > 0.0 {
            unsafe {
                self.gl.use_program(Some(self.edge_glow_program));
                self.gl.uniform_matrix_4_f32_slice(
                    self.edge_glow_uniforms.projection.as_ref(),
                    false,
                    &proj,
                );
                self.gl.uniform_4_f32(
                    self.edge_glow_uniforms.rect.as_ref(),
                    0.0,
                    0.0,
                    self.screen_w as f32,
                    self.screen_h as f32,
                );
                self.gl.uniform_4_f32(
                    self.edge_glow_uniforms.glow_color.as_ref(),
                    self.edge_glow_color[0],
                    self.edge_glow_color[1],
                    self.edge_glow_color[2],
                    self.edge_glow_color[3],
                );
                self.gl.uniform_1_f32(
                    self.edge_glow_uniforms.glow_width.as_ref(),
                    self.edge_glow_width,
                );
                self.gl.uniform_2_f32(
                    self.edge_glow_uniforms.mouse.as_ref(),
                    self.mouse_x,
                    self.mouse_y,
                );
                self.gl.uniform_2_f32(
                    self.edge_glow_uniforms.screen_size.as_ref(),
                    self.screen_w as f32,
                    self.screen_h as f32,
                );
                self.gl.uniform_1_f32(
                    self.edge_glow_uniforms.time.as_ref(),
                    self.compositor_start_time.elapsed().as_secs_f32(),
                );
                self.gl.bind_vertex_array(Some(self.quad_vao));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                self.gl.bind_vertex_array(None);
                self.gl.use_program(None);
            }
        }

        // === Pass 5c: Particle effects ===
        if !self.particle_systems.is_empty() {
            self.render_particles(&proj);
        }

        // === Pass 5d: Overview overlay ===
        if self.overview_active {
            self.tick_overview_prism();
            self.render_overview(&proj, focused);
        }

        // === Pass 5f: Expose/Mission Control overlay ===
        if !self.expose_entries.is_empty() {
            self.render_expose(&proj);
        }

        // A lock screen is sensitive content: remote/IPC captures must see the
        // opaque lock UI, never the client scene underneath it.
        if self
            .system_ui
            .as_ref()
            .is_some_and(|overlay| overlay.locked)
        {
            self.render_system_ui(&proj);
        }

        // === Feature 12: Screenshot capture (after all rendering, before overlays) ===
        // Capture BEFORE rendering snap preview / annotations so the screenshot
        // doesn't include the selection overlay or annotation strokes.
        self.capture_screenshot_freeze();
        let has_pending_screenshot = self.screenshot_requests.has_pending();
        for request in self.screenshot_requests.take_all() {
            match request {
                crate::backend::compositor_common::screenshot::ScreenshotRequest::Full(path) => {
                    self.capture_screenshot(&path);
                }
                crate::backend::compositor_common::screenshot::ScreenshotRequest::Region {
                    path,
                    x,
                    y,
                    width,
                    height,
                } => {
                    self.capture_screenshot_region(&path, x, y, width, height);
                }
            }
        }

        // === Pass 5g: Snap preview ===
        // Skip on the frame that captured a screenshot (overlay was already cleared
        // logically; rendering it would leave a ghost on the next visible frame).
        // The frozen scene is deliberately below screenshot annotations and
        // the toolbar, but above the live window scene.
        self.render_screenshot_freeze(&proj);

        if !has_pending_screenshot {
            self.render_snap_preview(&proj);
        }

        // === Pass 5e: Annotations overlay ===
        // Shapes first, then strokes over them: a redaction bar must not land
        // on top of the arrow that points at it. The toolbar comes last of
        // all, since it floats above everything it edits.
        if self.annotation_active {
            self.refresh_annotation_labels();
            self.render_annotation_shapes(&proj);
            if !self.annotation_strokes.is_empty() {
                self.render_annotations(&proj);
            }
        }
        if !has_pending_screenshot && self.screenshot_toolbar.is_some() {
            self.refresh_screenshot_toolbar();
            self.render_screenshot_toolbar(&proj);
        }

        // === Tag-switch transition overlay ===
        let transition_still_active = if let Some(progress) =
            self.transition_progress(std::time::Instant::now())
        {
            // Monitor-local geometry for the transition
            let mon_x = self.transition_mon_x;
            let mon_y = self.transition_mon_y;
            let mon_w = self.transition_mon_w;
            let mon_h = self.transition_mon_h;
            let exclude_top = self.transition_exclude_top.min(mon_h);
            let draw_y = (mon_y as u32 + exclude_top) as f32; // Y in screen coords
            let draw_h = (mon_h - exclude_top) as f32;
            let draw_x = mon_x as f32;
            let top_frac = if mon_h == 0 {
                0.0
            } else {
                exclude_top as f32 / mon_h as f32
            };
            // OpenGL scissor Y is flipped
            let scissor_gl_y = self.screen_h as i32 - (mon_y + mon_h as i32);

            match self.transition_mode {
                TransitionMode::None => {}
                TransitionMode::Slide => {
                    // --- Slide mode: old scene slides out + fades ---
                    // New scene is already in the back-buffer at final position.
                    // Old snapshot slides in transition_direction while fading out,
                    // giving the effect of current windows sliding away to reveal
                    // the target windows underneath.
                    if let Some((_, snap_tex)) = &self.transition_fbo {
                        let snap_tex = *snap_tex;

                        // Slide offset: old scene moves in the transition direction
                        let slide_offset = progress * self.transition_direction * mon_w as f32;

                        // Fade out smoothly over the full duration
                        let fade_opacity = (1.0 - progress).max(0.0);

                        unsafe {
                            if draw_h > 0.0 && fade_opacity > 0.0 {
                                self.gl.enable(glow::SCISSOR_TEST);
                                self.gl.scissor(
                                    mon_x,
                                    scissor_gl_y,
                                    mon_w as i32,
                                    (mon_h - exclude_top) as i32,
                                );

                                self.gl
                                    .blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);

                                self.gl.use_program(Some(self.transition_program));
                                self.gl.uniform_matrix_4_f32_slice(
                                    self.transition_uniforms.projection.as_ref(),
                                    false,
                                    &proj,
                                );
                                self.gl
                                    .uniform_1_i32(self.transition_uniforms.texture.as_ref(), 0);
                                self.gl.active_texture(glow::TEXTURE0);

                                let uv = [0.0f32, 0.0, 1.0, 1.0 - top_frac];

                                self.gl.uniform_4_f32(
                                    self.transition_uniforms.rect.as_ref(),
                                    draw_x + slide_offset,
                                    draw_y,
                                    mon_w as f32,
                                    draw_h,
                                );
                                self.gl.uniform_1_f32(
                                    self.transition_uniforms.opacity.as_ref(),
                                    fade_opacity,
                                );
                                self.gl.uniform_4_f32(
                                    self.transition_uniforms.uv_rect.as_ref(),
                                    uv[0],
                                    uv[1],
                                    uv[2],
                                    uv[3],
                                );
                                self.gl.bind_texture(glow::TEXTURE_2D, Some(snap_tex));
                                self.gl.bind_vertex_array(Some(self.quad_vao));
                                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                                self.gl.bind_vertex_array(None);
                                self.gl.use_program(None);

                                self.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
                                self.gl.disable(glow::SCISSOR_TEST);
                            }
                        }
                    }
                }
                TransitionMode::Cube => {
                    // --- Cube mode: 3D rotating cube transition ---
                    self.render_cube_transition(progress, &proj);
                }
                TransitionMode::Fade => {
                    // --- Fade mode: old scene fades out, new scene fades in ---
                    if let Some((_, snap_tex)) = &self.transition_fbo {
                        let snap_tex = *snap_tex;
                        let fade_opacity = (1.0 - progress).max(0.0);
                        unsafe {
                            if draw_h > 0.0 && fade_opacity > 0.0 {
                                self.gl.enable(glow::SCISSOR_TEST);
                                self.gl.scissor(
                                    mon_x,
                                    scissor_gl_y,
                                    mon_w as i32,
                                    (mon_h - exclude_top) as i32,
                                );
                                self.gl
                                    .blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
                                self.gl.use_program(Some(self.transition_program));
                                self.gl.uniform_matrix_4_f32_slice(
                                    self.transition_uniforms.projection.as_ref(),
                                    false,
                                    &proj,
                                );
                                self.gl
                                    .uniform_1_i32(self.transition_uniforms.texture.as_ref(), 0);
                                self.gl.active_texture(glow::TEXTURE0);
                                let uv = [0.0f32, 0.0, 1.0, 1.0 - top_frac];
                                self.gl.uniform_4_f32(
                                    self.transition_uniforms.rect.as_ref(),
                                    draw_x,
                                    draw_y,
                                    mon_w as f32,
                                    draw_h,
                                );
                                self.gl.uniform_1_f32(
                                    self.transition_uniforms.opacity.as_ref(),
                                    fade_opacity,
                                );
                                self.gl.uniform_4_f32(
                                    self.transition_uniforms.uv_rect.as_ref(),
                                    uv[0],
                                    uv[1],
                                    uv[2],
                                    uv[3],
                                );
                                self.gl.bind_texture(glow::TEXTURE_2D, Some(snap_tex));
                                self.gl.bind_vertex_array(Some(self.quad_vao));
                                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                                self.gl.bind_vertex_array(None);
                                self.gl.use_program(None);
                                self.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
                                self.gl.disable(glow::SCISSOR_TEST);
                            }
                        }
                    }
                }
                TransitionMode::Flip => {
                    // --- Flip mode: card-flip around Y axis ---
                    self.render_flip_transition(progress, &proj);
                }
                TransitionMode::Zoom => {
                    // --- Zoom mode: old scene shrinks + fades, new scene grows in ---
                    if let Some((_, snap_tex)) = &self.transition_fbo {
                        let snap_tex = *snap_tex;
                        let fade_opacity = (1.0 - progress).max(0.0);
                        // Old scene shrinks toward center
                        let scale = 1.0 - progress * 0.5; // 1.0 → 0.5
                        let scaled_w = mon_w as f32 * scale;
                        let scaled_h = draw_h * scale;
                        let offset_x = draw_x + (mon_w as f32 - scaled_w) * 0.5;
                        let offset_y = draw_y + (draw_h - scaled_h) * 0.5;
                        unsafe {
                            if draw_h > 0.0 && fade_opacity > 0.0 {
                                self.gl.enable(glow::SCISSOR_TEST);
                                self.gl.scissor(
                                    mon_x,
                                    scissor_gl_y,
                                    mon_w as i32,
                                    (mon_h - exclude_top) as i32,
                                );
                                self.gl
                                    .blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
                                self.gl.use_program(Some(self.transition_program));
                                self.gl.uniform_matrix_4_f32_slice(
                                    self.transition_uniforms.projection.as_ref(),
                                    false,
                                    &proj,
                                );
                                self.gl
                                    .uniform_1_i32(self.transition_uniforms.texture.as_ref(), 0);
                                self.gl.active_texture(glow::TEXTURE0);
                                let uv = [0.0f32, 0.0, 1.0, 1.0 - top_frac];
                                self.gl.uniform_4_f32(
                                    self.transition_uniforms.rect.as_ref(),
                                    offset_x,
                                    offset_y,
                                    scaled_w,
                                    scaled_h,
                                );
                                self.gl.uniform_1_f32(
                                    self.transition_uniforms.opacity.as_ref(),
                                    fade_opacity,
                                );
                                self.gl.uniform_4_f32(
                                    self.transition_uniforms.uv_rect.as_ref(),
                                    uv[0],
                                    uv[1],
                                    uv[2],
                                    uv[3],
                                );
                                self.gl.bind_texture(glow::TEXTURE_2D, Some(snap_tex));
                                self.gl.bind_vertex_array(Some(self.quad_vao));
                                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                                self.gl.bind_vertex_array(None);
                                self.gl.use_program(None);
                                self.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
                                self.gl.disable(glow::SCISSOR_TEST);
                            }
                        }
                    }
                }
                TransitionMode::Stack => {
                    // --- Stack mode: new scene slides over old with depth effect ---
                    if let Some((_, snap_tex)) = &self.transition_fbo {
                        let snap_tex = *snap_tex;
                        // Old scene stays in place but darkens and scales down slightly
                        let dim = 1.0 - progress * 0.3; // 1.0 → 0.7
                        let old_scale = 1.0 - progress * 0.05; // 1.0 → 0.95
                        let old_w = mon_w as f32 * old_scale;
                        let old_h = draw_h * old_scale;
                        let old_x = draw_x + (mon_w as f32 - old_w) * 0.5;
                        let old_y = draw_y + (draw_h - old_h) * 0.5;
                        unsafe {
                            if draw_h > 0.0 {
                                self.gl.enable(glow::SCISSOR_TEST);
                                self.gl.scissor(
                                    mon_x,
                                    scissor_gl_y,
                                    mon_w as i32,
                                    (mon_h - exclude_top) as i32,
                                );

                                // First: clear workspace area and redraw wallpaper behind
                                self.gl.clear_color(0.0, 0.0, 0.0, 1.0);
                                self.gl.clear(glow::COLOR_BUFFER_BIT);
                                self.gl
                                    .viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
                                self.draw_wallpaper_in_region(&proj, mon_x, mon_y, mon_w, mon_h);

                                // Draw dimmed/scaled old scene
                                self.gl
                                    .blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
                                self.gl.use_program(Some(self.transition_program));
                                self.gl.uniform_matrix_4_f32_slice(
                                    self.transition_uniforms.projection.as_ref(),
                                    false,
                                    &proj,
                                );
                                self.gl
                                    .uniform_1_i32(self.transition_uniforms.texture.as_ref(), 0);
                                self.gl.active_texture(glow::TEXTURE0);
                                let uv = [0.0f32, 0.0, 1.0, 1.0 - top_frac];
                                self.gl.uniform_4_f32(
                                    self.transition_uniforms.rect.as_ref(),
                                    old_x,
                                    old_y,
                                    old_w,
                                    old_h,
                                );
                                self.gl
                                    .uniform_1_f32(self.transition_uniforms.opacity.as_ref(), dim);
                                self.gl.uniform_4_f32(
                                    self.transition_uniforms.uv_rect.as_ref(),
                                    uv[0],
                                    uv[1],
                                    uv[2],
                                    uv[3],
                                );
                                self.gl.bind_texture(glow::TEXTURE_2D, Some(snap_tex));
                                self.gl.bind_vertex_array(Some(self.quad_vao));
                                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                                // Draw new scene sliding in from the transition direction
                                // New scene is already rendered in the back-buffer; we blit
                                // from transition_new_fbo if available, otherwise approximate
                                // by drawing the back-buffer content as a sliding overlay.
                                // For Stack, capture new scene like cube does.
                                if self.transition_new_fbo.is_none() {
                                    self.transition_new_fbo =
                                        Self::create_scene_fbo(&self.gl, mon_w, mon_h).ok();
                                }
                                if let Some((new_fbo, new_tex)) = &self.transition_new_fbo {
                                    let new_fbo = *new_fbo;
                                    let new_tex = *new_tex;
                                    self.capture_transition_scene(
                                        new_fbo, mon_x, mon_y, mon_w, mon_h,
                                    );

                                    // New scene slides in from the side
                                    let new_slide =
                                        (1.0 - progress) * self.transition_direction * mon_w as f32;
                                    self.gl.uniform_4_f32(
                                        self.transition_uniforms.rect.as_ref(),
                                        draw_x + new_slide,
                                        draw_y,
                                        mon_w as f32,
                                        draw_h,
                                    );
                                    self.gl.uniform_1_f32(
                                        self.transition_uniforms.opacity.as_ref(),
                                        1.0,
                                    );
                                    self.gl.uniform_4_f32(
                                        self.transition_uniforms.uv_rect.as_ref(),
                                        uv[0],
                                        uv[1],
                                        uv[2],
                                        uv[3],
                                    );
                                    self.gl.bind_texture(glow::TEXTURE_2D, Some(new_tex));
                                    self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                                }

                                self.gl.bind_vertex_array(None);
                                self.gl.use_program(None);
                                self.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
                                self.gl.disable(glow::SCISSOR_TEST);
                            }
                        }
                    }
                }
                TransitionMode::Blinds => {
                    // --- Blinds mode: vertical strips flip to reveal new scene ---
                    self.render_blinds_transition(progress, &proj);
                }
                TransitionMode::CoverFlow => {
                    self.render_coverflow_transition(progress, &proj);
                }
                TransitionMode::Helix => {
                    self.render_helix_transition(progress, &proj);
                }
                TransitionMode::Portal => {
                    self.render_portal_transition(progress, &proj);
                }
                TransitionMode::Book => {
                    self.render_book_transition(progress, &proj);
                }
            }
            true
        } else {
            // Transition finished — clean up
            if self.transition_start.is_some() {
                self.transition_start = None;
                // Release the monitor-sized snapshot FBOs/textures instead of
                // letting them sit idle in VRAM until the next transition (or
                // Drop) reclaims them.
                if let Some((fbo, tex)) = self.transition_fbo.take() {
                    unsafe {
                        self.gl.delete_framebuffer(fbo);
                        self.gl.delete_texture(tex);
                    }
                }
                if let Some((fbo, tex)) = self.transition_new_fbo.take() {
                    unsafe {
                        self.gl.delete_framebuffer(fbo);
                        self.gl.delete_texture(tex);
                    }
                }
                log::debug!("compositor: tag-switch transition completed");
            }
            false
        };

        // Toast cards sit above clients but under the modal system UI (its
        // scrim dims them; the lock screen hides them).
        self.render_toasts(&proj);
        self.render_osd(&proj);

        // System UI is always the final visual layer, above transitions and clients.
        if self.system_ui.is_some() {
            self.render_system_ui(&proj);
        }

        // Keep the repair scissor active through post-processing and overlays,
        // then reset it before capture/swap so the next full frame starts from a
        // known state.
        if use_scissor {
            unsafe {
                self.gl.disable(glow::SCISSOR_TEST);
            }
        }

        // Capture before swapping: the graphics back buffer's contents are no
        // longer defined after SwapBuffers, which caused intermittent black or
        // corrupted frames in both X11RB and XCB backends.
        if self.recording_active {
            self.capture_recording_frame();
        }

        // Recording crop controls are a local-only overlay. Rendering them
        // after the PBO capture keeps the handles out of the encoded video.
        if self.recording_region_overlay.is_some() {
            self.render_recording_region_overlay(&proj);
        }

        // Preserve the exact final composited image while the default back
        // buffer is still defined. A valid persistent texture follows partial
        // repair damage incrementally; first/invalid snapshots copy the full
        // output so they never depend on EGL/GLX buffer-age history.
        let presented_scene_copied =
            self.capture_presented_scene_candidate(use_scissor.then_some(damage_scissor));

        // Swap the selected platform surface. EGL receives the original damage
        // rectangles converted to its bottom-left coordinate convention.
        let swap_damage = (!swap_damage_rects.is_empty()).then_some(swap_damage_rects.as_slice());
        // OML remains a GLX-only pacing optimization; EGL/GLES uses
        // eglSwapInterval(1) or X Present pacing.
        let swap_result = match self.vsync_method {
            VsyncMethod::OmlSyncControl => {
                if self
                    .oml
                    .as_ref()
                    .and_then(|oml| oml.swap_buffers_msc(0))
                    .is_some()
                {
                    Ok(())
                } else {
                    self.graphics.swap_buffers(swap_damage)
                }
            }
            VsyncMethod::Present | VsyncMethod::Global => self.graphics.swap_buffers(swap_damage),
        };
        self.scratch_swap_damage = swap_damage_rects;
        if let Err(error) = swap_result {
            // The candidate overwrote the previous stable texture, but this
            // frame was not presented. Never expose it as an "old scene".
            self.presented_scene_status.invalidate();
            log::error!(
                "{}: {} swap failed: {error}",
                self.renderer_ctx("frame: swap buffers"),
                self.graphics.api_name()
            );
            self.buffer_age_damage_history.clear();
            self.context_current = false;
            self.needs_render = true;
            return false;
        }
        if presented_scene_copied {
            self.presented_scene_status
                .record_capture(self.screen_w, self.screen_h);
        } else {
            self.presented_scene_status.invalidate();
        }
        self.buffer_age_damage_history.record(frame_damage);
        self.waterlily_layer_dirty = false;

        // Genie completion owns native texture teardown/cache transfer. Do it
        // only after the exact terminal mesh sampled above has reached the
        // front buffer; an early return or failed swap keeps the owner intact
        // for a retry instead of deleting an animation before it is visible.
        if let Some(frame_sample_time) = genie_frame_sample_time {
            self.finish_genie_frame(frame_sample_time);
        }

        // Schedule re-render if fades or transition are still in progress
        if fades_active
            || transition_still_active
            || wobbly_active
            || particles_active
            || motion_trails_active
            || self.overview_animation_pending()
            || genie_active
            || ripples_active
            || focus_highlight_active
            || wallpaper_crossfade_active
            || expose_animating
            || snap_animating
            || peek_animating
        {
            self.needs_render = true;
        }

        // Recording deliberately does not re-arm the flag here. It paces itself:
        // `needs_render()` reports true when the next capture is due and
        // `compositor_frame_deadline()` tells the event loop how long to sleep
        // until then. Re-arming instead forced the frame gate above to draw on
        // every loop iteration, whether or not a capture would follow.

        // Animate zoom-to-fit scale
        if (self.zoom_to_fit_scale - self.zoom_to_fit_target).abs() > 0.001 {
            self.zoom_to_fit_scale += (self.zoom_to_fit_target - self.zoom_to_fit_scale) * 0.15;
            if (self.zoom_to_fit_scale - self.zoom_to_fit_target).abs() < 0.001 {
                self.zoom_to_fit_scale = self.zoom_to_fit_target;
            }
            self.needs_render = true;
        }

        // Phase 2: End frame profiling
        let frame_time_ms = self.frame_profiler.end_frame();

        // Benchmark: record frame data
        if self.benchmark.is_running() {
            let frame_us = bench_frame_start.elapsed().as_micros() as u64;
            self.benchmark.record_frame(frame_us);

            // Feed latest input latency
            if let Some(&last_latency) = self.frame_stats.latency_samples.back() {
                self.benchmark.record_input_latency(last_latency);
            }

            // Feed zone stats from profiler
            for (zone, zs) in self.frame_profiler.all_zone_stats() {
                self.benchmark.record_zone(zone, zs.avg_ms);
            }

            // Feed GL stats
            self.benchmark.record_gl_stats(
                self.frame_stats.draw_calls,
                0, // state changes tracked elsewhere
                0, // texture binds tracked elsewhere
            );

            // Feed blur cache stats
            self.benchmark.blur_cache_hits = self.frame_stats.blur_cache_hits;
            self.benchmark.blur_cache_misses = self.frame_stats.blur_cache_misses;
        }

        // Log profiler stats every 300 frames (~5s at 60fps)
        if self.frame_stats.frame_count % 300 == 0 && self.frame_profiler.is_enabled() {
            let stats = self.frame_profiler.all_zone_stats();
            if !stats.is_empty() {
                log::info!("[profiler] Frame time: {:.2}ms", frame_time_ms);
                for (zone, zs) in stats {
                    log::info!(
                        "[profiler]   {}: avg={:.2}ms min={:.2}ms max={:.2}ms",
                        zone,
                        zs.avg_ms,
                        zs.min_ms,
                        zs.max_ms
                    );
                }
            }
        }

        // Return the per-frame scratch buffers to their fields for reuse.
        self.scratch_blur_dirty = blur_dirty_wins;
        self.scratch_blur_damage = blur_damage_below;
        self.scratch_tfp_order = tfp_order;

        true
    }

    // =====================================================================
    // New feature methods
    // =====================================================================
}

#[cfg(test)]
mod tests {
    use super::{
        DirtyRect, PresentedSceneCopyPlan, PresentedSceneStatus, TransitionCapturePlan,
        blur_sampling_margin, counts_for_smart_borders, direct_presentation_owner_changed,
        dirty_below_affects_backdrop, dirty_below_requires_full_blur_redraw,
        edge_effects_require_composition, focus_highlight_style, intersect_gl_scissors,
        is_opaque_occluder, minimized_dock_requires_composition, presented_scene_copy_plan,
        rect_covers_output, resolve_and_draw_each, screenshot_freeze_change_needed,
        screenshot_freeze_requires_composition, tfp_refresh_is_latency_critical,
        transformed_overlays_require_full_redraw, transition_capture_plan, wallpaper_blend_plan,
        window_prefers_direct_presentation,
    };

    #[test]
    fn status_bar_texture_refresh_is_latency_critical() {
        assert!(tfp_refresh_is_latency_critical(
            7,
            Some(3),
            "tao_softbuffer_bar",
            "tao_softbuffer_bar",
        ));
        assert!(tfp_refresh_is_latency_critical(
            7,
            Some(3),
            "prefix-tao_softbuffer_bar",
            "tao_softbuffer_bar",
        ));
        assert!(tfp_refresh_is_latency_critical(
            3,
            Some(3),
            "terminal",
            "tao_softbuffer_bar",
        ));
        assert!(!tfp_refresh_is_latency_critical(
            7,
            Some(3),
            "terminal",
            "tao_softbuffer_bar",
        ));
        assert!(!tfp_refresh_is_latency_critical(7, Some(3), "terminal", "",));
    }

    #[test]
    fn dock_sources_are_drawn_before_a_later_resolution_can_evict_them() {
        #[derive(Default)]
        struct GpuLruModel {
            resident: std::collections::HashSet<u8>,
            trace: Vec<(&'static str, u8)>,
        }

        let mut model = GpuLruModel::default();
        resolve_and_draw_each(
            &mut model,
            [1_u8, 2],
            |model, item| {
                model.trace.push(("resolve", item));
                let texture = item * 10;
                if item == 2 {
                    // The second lazy upload needs room and evicts the first
                    // item's raw texture. Interleaving must already have drawn
                    // the first handle before this mutation can occur.
                    model.resident.remove(&10);
                }
                model.resident.insert(texture);
                Some(texture)
            },
            |model, texture| {
                assert!(
                    model.resident.contains(&texture),
                    "a later resolve evicted a bare texture before its draw"
                );
                model.trace.push(("draw", texture / 10));
            },
        );

        assert_eq!(
            model.trace,
            [("resolve", 1), ("draw", 1), ("resolve", 2), ("draw", 2),]
        );
    }

    #[test]
    fn ime_popups_do_not_count_toward_smart_borders() {
        // A lone tiled client is the only window that counts, so the smart
        // border stays off while an fcitx5 candidate list or the input-method
        // switcher (both override-redirect) is on screen.
        assert!(counts_for_smart_borders("Alacritty", "jwm-bar", false));
        assert!(!counts_for_smart_borders("fcitx", "jwm-bar", true));
        assert!(!counts_for_smart_borders("jwm-bar", "jwm-bar", false));
    }

    #[test]
    fn hidden_bar_geometry_does_not_keep_x11_cache_composited() {
        assert!(minimized_dock_requires_composition(true, false, false));
        assert!(minimized_dock_requires_composition(false, true, false));
        assert!(!minimized_dock_requires_composition(false, false, false));
    }

    #[test]
    fn pending_iconic_recapture_blocks_fullscreen_early_return() {
        assert!(
            minimized_dock_requires_composition(false, false, true),
            "a due retained recapture must reach the make-current barrier even without Dock drawing"
        );
    }

    #[test]
    fn screenshot_freeze_blocks_fullscreen_bypass_before_and_after_capture() {
        assert!(screenshot_freeze_requires_composition(true, false));
        assert!(screenshot_freeze_requires_composition(false, true));
        assert!(!screenshot_freeze_requires_composition(false, false));
    }

    #[test]
    fn repeated_screenshot_freeze_requests_do_not_recapture_the_scene() {
        assert!(screenshot_freeze_change_needed(true, false, false));
        assert!(!screenshot_freeze_change_needed(true, true, false));
        assert!(!screenshot_freeze_change_needed(true, false, true));
        assert!(screenshot_freeze_change_needed(false, true, false));
        assert!(screenshot_freeze_change_needed(false, false, true));
        assert!(!screenshot_freeze_change_needed(false, false, false));
    }

    #[test]
    fn presented_scene_only_becomes_usable_after_a_successful_capture() {
        let mut status = PresentedSceneStatus::default();
        assert!(!status.is_usable(3840, 2160));

        status.record_allocation(3840, 2160);
        assert!(!status.is_usable(3840, 2160));

        status.record_capture(3840, 2160);
        assert!(status.is_usable(3840, 2160));
        assert!(!status.is_usable(1920, 1080));

        // A failed swap invalidates the overwritten candidate without losing
        // its allocation dimensions, so the next frame can reuse the FBO.
        status.invalidate();
        assert!(!status.is_usable(3840, 2160));
        assert!(status.has_dimensions(3840, 2160));

        status.record_allocation_failure(3840, 2160);
        assert!(status.allocation_failed_for(3840, 2160));
        assert!(!status.allocation_failed_for(1920, 1080));

        status.reset();
        assert_eq!(status, PresentedSceneStatus::default());
    }

    #[test]
    fn presented_scene_copy_is_disabled_with_no_transition_effect() {
        assert_eq!(
            presented_scene_copy_plan(false, true, Some((10, 20, 30, 40)), 3840, 2160),
            PresentedSceneCopyPlan::Disabled
        );
    }

    #[test]
    fn presented_scene_copy_is_full_first_then_incremental() {
        assert_eq!(
            presented_scene_copy_plan(true, false, Some((10, 20, 30, 40)), 3840, 2160),
            PresentedSceneCopyPlan::Full
        );
        assert_eq!(
            presented_scene_copy_plan(true, true, Some((10, 20, 30, 40)), 3840, 2160),
            PresentedSceneCopyPlan::Region((10, 20, 30, 40))
        );
        assert_eq!(
            presented_scene_copy_plan(true, true, None, 3840, 2160),
            PresentedSceneCopyPlan::Full
        );
    }

    #[test]
    fn transition_capture_crops_workspace_from_stable_full_output() {
        assert_eq!(
            transition_capture_plan(3840, 1080, 1920, 0, 1920, 1080, 30),
            Some(TransitionCapturePlan {
                src: (1920, 0, 3840, 1050),
                dst: (0, 0, 1920, 1050),
            })
        );

        // A monitor rectangle partly outside the root is clipped, preserving
        // its offset in the monitor-sized destination instead of stretching.
        assert_eq!(
            transition_capture_plan(1920, 1080, -100, 0, 1920, 1080, 0),
            Some(TransitionCapturePlan {
                src: (0, 0, 1820, 1080),
                dst: (100, 0, 1920, 1080),
            })
        );
    }

    #[test]
    fn fullscreen_occlusion_requires_an_opaque_untransformed_draw() {
        assert!(is_opaque_occluder(false, 1.0, 0.0, false, 1.0, 1.0, false,));

        assert!(!is_opaque_occluder(true, 1.0, 0.0, false, 1.0, 1.0, false,));
        assert!(!is_opaque_occluder(
            false, 0.99, 0.0, false, 1.0, 1.0, false,
        ));
        assert!(!is_opaque_occluder(
            false,
            f32::NAN,
            0.0,
            false,
            1.0,
            1.0,
            false,
        ));
        assert!(!is_opaque_occluder(false, 1.0, 8.0, false, 1.0, 1.0, false,));
        assert!(!is_opaque_occluder(false, 1.0, 0.0, true, 1.0, 1.0, false,));
        assert!(!is_opaque_occluder(
            false, 1.0, 0.0, false, 0.98, 1.0, false,
        ));
        assert!(!is_opaque_occluder(
            false, 1.0, 0.0, false, 1.0, 0.98, false,
        ));
        assert!(!is_opaque_occluder(false, 1.0, 0.0, false, 1.0, 1.0, true,));
    }

    #[test]
    fn fullscreen_coverage_uses_wide_coordinate_arithmetic() {
        assert!(rect_covers_output(0, 0, 1920, 1080, 1920, 1080));
        assert!(!rect_covers_output(0, 0, 1919, 1080, 1920, 1080));
        assert!(rect_covers_output(
            -1,
            -1,
            u32::MAX,
            u32::MAX,
            u32::MAX - 1,
            u32::MAX - 1,
        ));
    }

    #[test]
    fn ewmh_bypass_policy_extends_and_can_inhibit_fullscreen_unredirect() {
        assert!(window_prefers_direct_presentation(true, 0));
        assert!(!window_prefers_direct_presentation(false, 0));
        assert!(window_prefers_direct_presentation(false, 1));
        assert!(!window_prefers_direct_presentation(true, 2));
        // Reserved values are neutral.
        assert!(window_prefers_direct_presentation(true, 9));
        assert!(!window_prefers_direct_presentation(false, 9));
    }

    #[test]
    fn focus_change_restores_the_previous_direct_presentation_owner_first() {
        assert!(!direct_presentation_owner_changed(7, Some(7)));
        assert!(direct_presentation_owner_changed(7, Some(8)));
        assert!(direct_presentation_owner_changed(7, None));
    }

    #[test]
    fn default_edge_effects_do_not_make_fullscreen_unredirect_unreachable() {
        assert!(edge_effects_require_composition(
            false, false, true, true, 2.0, 12.0,
        ));
        assert!(!edge_effects_require_composition(
            true, true, true, true, 2.0, 12.0,
        ));
        assert!(!edge_effects_require_composition(
            true, false, true, true, 2.0, 12.0,
        ));
    }

    #[test]
    fn focus_highlight_returns_to_the_stable_border_at_both_ends() {
        let focused = [0.1, 0.2, 0.3, 0.8];
        let highlight = [0.4, 0.7, 1.0, 0.9];

        let start = focus_highlight_style(focused, highlight, 1.0, 0.0);
        let end = focus_highlight_style(focused, highlight, 1.0, 1.0);
        assert_eq!(start.color, focused);
        assert_eq!(start.width, 1.0);
        assert_eq!(end.color, focused);
        assert_eq!(end.width, 1.0);
    }

    #[test]
    fn focus_highlight_smoothly_reaches_the_configured_peak() {
        let highlight = [0.4, 0.7, 1.0, 0.9];
        let peak = focus_highlight_style([0.1, 0.2, 0.3, 0.8], highlight, 1.0, 0.5);

        assert_eq!(peak.color, highlight);
        assert_eq!(peak.width, 3.0);
    }

    #[test]
    fn transformed_overlays_turn_damage_frames_into_full_redraws() {
        assert!(!transformed_overlays_require_full_redraw(
            false, false, false, false,
        ));
        assert!(transformed_overlays_require_full_redraw(
            true, false, false, false,
        ));
        assert!(transformed_overlays_require_full_redraw(
            false, true, false, false,
        ));
        assert!(transformed_overlays_require_full_redraw(
            false, false, true, true,
        ));
        assert!(transformed_overlays_require_full_redraw(
            false, false, false, true,
        ));
    }

    #[test]
    fn intersects_monitor_and_damage_scissors() {
        assert_eq!(
            intersect_gl_scissors((0, 0, 1920, 1080), (1800, 900, 300, 300)),
            Some((1800, 900, 120, 180))
        );
        assert_eq!(
            intersect_gl_scissors((1920, 0, 1920, 1080), (100, 100, 50, 50)),
            None
        );
        assert_eq!(
            intersect_gl_scissors((0, 0, 1920, 1080), (100, 100, 50, 50)),
            Some((100, 100, 50, 50))
        );
    }

    #[test]
    fn monitor_override_is_not_covered_by_global_crossfade() {
        let plan = wallpaper_blend_plan(true, true, true, Some(0.4));
        assert_eq!(plan.old_global_opacity, None);
        assert_eq!(plan.current_opacity, Some(1.0));
    }

    #[test]
    fn global_fallback_draws_opaque_old_then_progressive_new() {
        let plan = wallpaper_blend_plan(false, true, true, Some(0.4));
        assert_eq!(plan.old_global_opacity, Some(1.0));
        assert_eq!(plan.current_opacity, Some(0.4));

        // This is also the plan for the global-only path when no monitor
        // wallpaper entries have been installed yet.
        let global_only = wallpaper_blend_plan(false, true, true, Some(0.0));
        assert_eq!(global_only.old_global_opacity, Some(1.0));
        assert_eq!(global_only.current_opacity, Some(0.0));
    }

    #[test]
    fn stable_global_wallpaper_is_drawn_once_at_full_opacity() {
        let plan = wallpaper_blend_plan(false, true, false, None);
        assert_eq!(plan.old_global_opacity, None);
        assert_eq!(plan.current_opacity, Some(1.0));
    }

    #[test]
    fn distant_client_damage_does_not_invalidate_backdrop() {
        let backdrop = DirtyRect::new(1400, 100, 900, 600);
        let dirty_below = [DirtyRect::new(100, 100, 900, 600)];
        assert!(!dirty_below_affects_backdrop(&dirty_below, backdrop, 3));
    }

    #[test]
    fn damage_inside_blur_sampling_margin_invalidates_backdrop() {
        let margin = blur_sampling_margin(3);
        let backdrop = DirtyRect::new(1000, 100, 500, 500);
        let dirty_below = [DirtyRect::new(1000 - margin, 200, margin as u32, 100)];
        assert!(dirty_below_affects_backdrop(&dirty_below, backdrop, 3));

        let outside = [DirtyRect::new(900 - margin, 200, 100, 100)];
        assert!(!dirty_below_affects_backdrop(&outside, backdrop, 3));
    }

    #[test]
    fn current_client_damage_does_not_redraw_distant_blur_client() {
        let scene = [(10, 100, 100, 900, 600), (20, 1400, 100, 900, 600)];
        assert!(!dirty_below_requires_full_blur_redraw(
            &scene,
            &[10],
            3,
            |win| win == 20,
        ));
    }

    #[test]
    fn intersecting_lower_client_damage_forces_full_blur_redraw() {
        let scene = [(10, 100, 100, 900, 600), (20, 1000, 100, 900, 600)];
        assert!(dirty_below_requires_full_blur_redraw(
            &scene,
            &[10],
            3,
            |win| win == 20,
        ));
    }

    #[test]
    fn damage_above_blur_client_does_not_change_its_backdrop() {
        let scene = [(20, 1000, 100, 900, 600), (10, 100, 100, 900, 600)];
        assert!(!dirty_below_requires_full_blur_redraw(
            &scene,
            &[10],
            3,
            |win| win == 20,
        ));
    }
}
