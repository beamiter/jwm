// render_frame and rendering helpers
#[allow(unused_imports)]
use super::math::ortho;
#[allow(unused_imports)]
use super::*;
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

fn transformed_overlays_require_full_redraw(
    overview_active: bool,
    overview_closing: bool,
    expose_active: bool,
    has_expose_entries: bool,
) -> bool {
    overview_active || overview_closing || expose_active || has_expose_entries
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

        if let Some((snap_fbo, _)) = &self.transition_fbo {
            let snap_fbo = *snap_fbo;
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
            self.transition_start = Some(std::time::Instant::now());
            self.transition_duration = duration;
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

    fn capture_transition_scene_from(
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

    /// Rasterize HUD text and upload as a GL texture. Skips upload when the
    /// formatted string is identical to the previous frame.
    pub(super) fn update_hud_text_texture(&mut self, text: &str) {
        if text == self.hud_text_cache && self.hud_text_texture.is_some() {
            return;
        }

        let scale = 2u32;
        let fg = [0, 230, 64, 255]; // green
        let (pixels, w, h) = font::render_text_to_rgba(text, scale, fg);
        if w == 0 || h == 0 {
            return;
        }

        unsafe {
            if let Some(old) = self.hud_text_texture.take() {
                self.gl.delete_texture(old);
            }
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
                self.gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_MIN_FILTER,
                    glow::NEAREST as i32,
                );
                self.gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_MAG_FILTER,
                    glow::NEAREST as i32,
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
                self.hud_text_texture = Some(tex);
                self.hud_text_width = w;
                self.hud_text_height = h;
            }
        }

        self.hud_text_cache = text.to_string();
    }

    fn update_system_ui_text_texture(&mut self, text: &str) {
        let config = crate::config::CONFIG.load();
        let description = config.system_ui_font();
        let size = crate::backend::compositor_font::ui_font_pixel_size(description);
        let cache_key = format!("{description}\0{size}\0{text}");
        if cache_key == self.hud_text_cache && self.hud_text_texture.is_some() {
            return;
        }
        let (pixels, w, h) = crate::backend::compositor_font::render_ui_text_to_rgba(
            text,
            description,
            size,
            [235, 240, 255, 255],
        );
        if w == 0 || h == 0 {
            return;
        }
        unsafe {
            if let Some(old) = self.hud_text_texture.take() {
                self.gl.delete_texture(old);
            }
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
                self.hud_text_texture = Some(tex);
                self.hud_text_width = w;
                self.hud_text_height = h;
            }
        }
        self.hud_text_cache = cache_key;
    }

    fn render_system_ui(&mut self, proj: &[f32; 16]) {
        let Some(overlay) = self.system_ui.clone() else {
            return;
        };
        self.update_system_ui_text_texture(&overlay.text);
        let pad = 30.0;
        let text_w = self.hud_text_width as f32;
        let text_h = self.hud_text_height as f32;
        let panel_w = (text_w + pad * 2.0).min(self.screen_w as f32 - 32.0);
        let panel_h = text_h + pad * 2.0;
        let x = (self.screen_w as f32 - panel_w) * 0.5;
        let y = (self.screen_h as f32 - panel_h) * 0.5;
        unsafe {
            if overlay.locked {
                self.gl.clear_color(0.018, 0.022, 0.035, 1.0);
                self.gl.clear(glow::COLOR_BUFFER_BIT);
            }
            self.gl.use_program(Some(self.hud_program));
            self.gl
                .uniform_matrix_4_f32_slice(self.hud_uniforms.projection.as_ref(), false, proj);
            self.gl.uniform_4_f32(
                self.hud_uniforms.bg_color.as_ref(),
                0.025,
                0.03,
                0.045,
                if overlay.locked { 1.0 } else { 0.94 },
            );
            self.gl
                .uniform_4_f32(self.hud_uniforms.fg_color.as_ref(), 0.4, 0.7, 1.0, 1.0);
            self.gl.uniform_2_f32(
                self.hud_uniforms.size.as_ref(),
                self.screen_w as f32,
                self.screen_h as f32,
            );
            self.gl.uniform_4_f32(
                self.hud_uniforms.rect.as_ref(),
                0.0,
                0.0,
                self.screen_w as f32,
                self.screen_h as f32,
            );
            self.gl.bind_vertex_array(Some(self.quad_vao));
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            self.gl
                .uniform_4_f32(self.hud_uniforms.bg_color.as_ref(), 0.08, 0.10, 0.15, 0.98);
            self.gl
                .uniform_2_f32(self.hud_uniforms.size.as_ref(), panel_w, panel_h);
            self.gl
                .uniform_4_f32(self.hud_uniforms.rect.as_ref(), x, y, panel_w, panel_h);
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            if let Some(tex) = self.hud_text_texture {
                self.gl.use_program(Some(self.hud_text_program));
                self.gl.uniform_matrix_4_f32_slice(
                    self.hud_text_uniforms.projection.as_ref(),
                    false,
                    proj,
                );
                self.gl.uniform_4_f32(
                    self.hud_text_uniforms.rect.as_ref(),
                    x + pad,
                    y + pad,
                    text_w,
                    text_h,
                );
                self.gl
                    .uniform_1_i32(self.hud_text_uniforms.texture.as_ref(), 0);
                self.gl.active_texture(glow::TEXTURE0);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            }
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
            || self.system_ui.is_some()
            || self.debug_hud
            || self.recording_active
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
            || self.edge_glow_active
            || self.zoom_to_fit_window.is_some()
            || !self.particle_systems.is_empty()
            || !self.genie_active.is_empty()
            || !self.ripple_active.is_empty()
            || self.tickless_focus_or_wallpaper_animation_active()
            || self.pending_wallpaper.is_some()
            || !self.pending_monitor_wallpapers.is_empty()
            || (self.window_tabs_enabled && self.window_groups.values().any(|tabs| tabs.len() > 1))
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
                let radius = wt.corner_radius_override.unwrap_or(self.corner_radius);
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
                    || (self.shadow_enabled && !wt.is_fullscreen)
                    || (self.border_enabled && self.border_width > 0.0)
                    || radius > 0.0
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
    fn restore_unredirected_window(&mut self, window: u32, reason: &str) -> bool {
        let result = self
            .conn
            .redirect_window_manual(window)
            .and_then(|_| self.conn.flush_x11());
        match result {
            Ok(()) => {
                if let Some(wt) = self.windows.get_mut(&window) {
                    // The server allocated a new backing pixmap while the
                    // window was unredirected; the old TFP binding is stale.
                    wt.needs_pixmap_refresh = true;
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
        // Only unredirect if the top (focused) window is fullscreen and opaque
        if let Some(focused_win) = focused {
            if let Some(wt) = self.windows.get(&focused_win) {
                if wt.is_fullscreen
                    && !wt.has_rgba
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
                                            "compositor: unredirected fullscreen window 0x{:x}",
                                            focused_win
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
        // Re-redirect if we had an unredirected window that's no longer fullscreen
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
                    "[compositor::render_freq] {:.1} renders/sec (needs_render={}, focused={:?})",
                    fc as f64 / elapsed,
                    self.needs_render,
                    focused,
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
                    let corner_radius = wt.corner_radius_override.unwrap_or(self.corner_radius);
                    (
                        win,
                        WindowScanoutInfo {
                            x,
                            y,
                            width: w,
                            height: h,
                            is_fullscreen: wt.is_fullscreen,
                            has_alpha: wt.has_rgba,
                            has_blur: wt.is_frosted,
                            has_shadow: self.shadow_enabled,
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
        let fades_active = self.tick_fades(effect_dt);

        // Tick wobbly spring physics
        let wobbly_active = self.tick_wobbly();

        // Tick particle and motion-trail lifetimes before the unchanged-frame
        // gate so their state cannot get stuck behind that optimization.
        let particles_active = self.tick_particles(effect_dt);
        self.effect_tick_clock
            .finish_frame(fades_active || particles_active);
        let motion_trails_active = self.tick_motion_trails();
        let tilt_pending = self.window_tilt
            && ((self.tilt_current_x - self.tilt_target_x).abs() > 0.0001
                || (self.tilt_current_y - self.tilt_target_y).abs() > 0.0001);
        let attention_active =
            self.attention_animation && self.windows.values().any(|wt| wt.is_urgent);
        let overview_animating = self.overview_animation_pending();

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
            || attention_active;
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
        let mut has_dirty = false;
        let mut needs_native_texture_sync = false;
        for &(win, _, _, _, _) in scene {
            let Some(wt) = self.windows.get(&win) else {
                continue;
            };
            // Both EGLImage and GLX texture-from-pixmap imports need native X
            // rendering to complete before sampling. The GLX extension has no
            // implicit X/GL synchronization; omitting this on NVIDIA can show
            // an older client frame (most visibly terminal cursor/text damage).
            if (wt.dirty && wt.binding.is_some()) || wt.needs_pixmap_refresh {
                needs_native_texture_sync = true;
            }
            if wt.dirty || wt.needs_pixmap_refresh {
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
            || self.recording_active
            || self.annotation_active
            || wallpaper_just_loaded
            || wobbly_active
            || particles_active
            || motion_trails_active
            || tilt_pending
            || attention_active
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
        // Genuine pixmap invalidation is handled by update_geometry → needs_pixmap_refresh.
        let tfp_budget = std::time::Duration::from_micros(3000); // 3ms
        let tfp_start = std::time::Instant::now();

        // Build priority-ordered window list: focused first, then rest of scene
        let mut tfp_order = std::mem::take(&mut self.scratch_tfp_order);
        tfp_order.clear();
        tfp_order.reserve(scene.len());
        let mut focused_in_scene = false;
        if let Some(fw) = focused {
            tfp_order.push(fw);
        }
        for &(win, _, _, _, _) in scene {
            if Some(win) == focused {
                focused_in_scene = true;
            } else {
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
            // Budget check: focused window (index 0) always updates
            if tfp_budget_exhausted && Some(win) != focused {
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

                    // Check budget (but not for focused window)
                    if Some(win) != focused && tfp_start.elapsed() > tfp_budget {
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
                let corner_radius = wt.corner_radius_override.unwrap_or_else(|| {
                    if class_matches_exclude(&wt.class_name, &self.rounded_corners_exclude) {
                        0.0
                    } else {
                        self.corner_radius
                    }
                });
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

        let visible_scene = &scene[first_visible..];

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
                    // Fade: modulate shadow alpha
                    let fade = wt.fade_opacity;
                    let sa_faded = sa * fade;
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
                        fullscreen: wt.is_fullscreen,
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
                    self.gl
                        .uniform_1_f32(self.border_uniforms.radius.as_ref(), radius.max(0.0));
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

        // Phase 2.2: Auto blur quality downgrade during animations/transitions
        if self.blur_quality_auto {
            self.blur_quality = if self.transition_active() || self.overview_active {
                BlurQuality::Minimal
            } else if fades_active || wobbly_active {
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

        // Count actual client windows (excluding statusbar) to apply smart borders
        let status_bar_name = frame_status_bar_name;
        let client_window_count = visible_scene
            .iter()
            .filter(|&&(win, _, _, _, _)| {
                self.windows
                    .get(&win)
                    .map(|wt| {
                        !(wt.class_name == status_bar_name
                            || wt.class_name.contains(status_bar_name))
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
                    let focus_highlight_active_for_win =
                        if let Some((hw, start)) = self.focus_highlight_start {
                            hw == win
                                && start.elapsed().as_millis()
                                    < self.focus_highlight_duration_ms as u128
                        } else {
                            false
                        };
                    let attention_active_for_win = wt.is_urgent && self.attention_animation;
                    let has_special_border = attention_active_for_win || wt.is_pip;

                    // Phase 5.3: Peek opacity multiplier
                    let peek_mul = self.peek_opacity_for(&wt.class_name);

                    // Feature 3: Per-window corner radius
                    // Skip compositor rounding for override-redirect RGBA windows
                    // (popups, menus, tooltips) — they manage their own shape.
                    let radius = if wt.is_override_redirect && wt.has_rgba {
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
                        let trail_len = wt.motion_trail.len();
                        let trail_now = std::time::Instant::now();
                        let trail_lifetime =
                            crate::backend::compositor_common::effects::motion_trail_lifetime(
                                self.motion_trail_frames,
                            );
                        for (i, sample) in wt.motion_trail.iter().enumerate() {
                            let age_opacity = sample.opacity_at(trail_now, trail_lifetime);
                            let trail_opacity = self.motion_trail_opacity * (i as f32 + 1.0)
                                / trail_len as f32
                                * age_opacity;
                            if trail_opacity <= 0.001 {
                                continue;
                            }
                            let trail_layer = (trail_opacity * layer_opacity).clamp(0.0, 1.0);
                            self.gl.uniform_1_f32(
                                self.win_uniforms.opacity.as_ref(),
                                if use_texture_alpha {
                                    -trail_layer
                                } else {
                                    trail_layer
                                },
                            );
                            self.gl.uniform_1_f32(self.win_uniforms.dim.as_ref(), 0.7);
                            self.gl.uniform_4_f32(
                                self.win_uniforms.rect.as_ref(),
                                sample.x as f32,
                                sample.y as f32,
                                draw_w,
                                draw_h,
                            );
                            self.gl
                                .uniform_2_f32(self.win_uniforms.size.as_ref(), draw_w, draw_h);
                            self.gl.active_texture(glow::TEXTURE0);
                            self.gl.bind_texture(glow::TEXTURE_2D, Some(wt.gl_texture));
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

                    if !is_statusbar
                        && !wt.is_override_redirect
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
                        let color = if let Some(style) = focus_style {
                            style.color
                        } else if attention_active_for_win {
                            let elapsed = self.compositor_start_time.elapsed().as_secs_f32();
                            let pulse = (elapsed * 4.0).sin() * 0.5 + 0.5;
                            let [r, g, b, a] = self.attention_color;
                            [r, g, b, a * pulse]
                        } else if wt.is_pip {
                            self.pip_border_color
                        } else if is_focused {
                            self.border_color_focused
                        } else {
                            self.border_color_unfocused
                        };

                        let bw = if let Some(style) = focus_style {
                            style.width
                        } else if attention_active_for_win {
                            if effective_border_enabled {
                                base_border_width.max(2.0)
                            } else {
                                2.0
                            }
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
                            self.gl
                                .uniform_1_f32(self.border_uniforms.radius.as_ref(), radius);
                            self.gl
                                .uniform_2_f32(self.border_uniforms.size.as_ref(), bdr_w, bdr_h);
                            self.gl.uniform_4_f32(
                                self.border_uniforms.rect.as_ref(),
                                bdr_x,
                                bdr_y,
                                bdr_w,
                                bdr_h,
                            );
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
                            fullscreen: wt.is_fullscreen,
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
        if !self.genie_active.is_empty() {
            let genie_duration_ms = self.genie_duration_ms.max(1);
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

                let dock = self.dock_position;
                for ga in &self.genie_active {
                    let elapsed = ga.start.elapsed().as_millis() as f32;
                    let progress = (elapsed / genie_duration_ms as f32).min(1.0);
                    let opacity = 1.0 - progress;
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

        // === Pass 3c: Window tab bars ===
        if self.window_tabs_enabled && !self.window_groups.is_empty() {
            for &(win, x, y, w, _h) in visible_scene {
                if let Some((_gid, tabs)) = self.find_window_group(win) {
                    self.render_tab_bar(&proj, x as f32, y as f32, w as f32, tabs);
                }
            }
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
        {
            let tilt_animating = self.tick_tilt(effect_dt);
            if tilt_animating {
                self.needs_render = true;
            }
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

            let mut hud_text = format!(
                "JWM debug HUD (Alt+Shift+F12)\n\
                 Backend: x11\n\
                 FPS: {:.1}  Avg: {:.1}ms  Max: {:.1}ms  Min: {:.1}ms\n\
                 Windows: {}  Tiles: {}  Dirty: {:.0}%\n\
                 Memory: {:.1} MiB RSS\n\
                 CPU: {:.1} %",
                self.frame_stats.fps,
                avg_dt * 1000.0,
                max_dt * 1000.0,
                min_dt * 1000.0,
                self.windows.len(),
                self.damage_tracker.tile_count(),
                self.damage_tracker.dirty_fraction() * 100.0,
                self.sys_stats.rss_mib(),
                self.sys_stats.cpu_pct(),
            );
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
                use std::fmt::Write;
                let _ = write!(
                    hud_text,
                    "\nDraw calls: {}  Mem: {}KB\nBlur: {:.0}% hit rate ({}/{})\nQuality: {:?}",
                    self.frame_stats.draw_calls,
                    tex_mem_kb,
                    blur_hit_rate,
                    self.frame_stats.blur_cache_hits,
                    self.frame_stats.blur_cache_misses,
                    self.blur_quality,
                );

                // Add input latency stats if available
                let (avg, p50, p95, p99) = self.compute_latency_stats();
                if avg > 0.0 {
                    let _ = write!(
                        hud_text,
                        "\nLatency: avg {:.1}ms  p50 {:.1}ms  p95 {:.1}ms  p99 {:.1}ms",
                        avg, p50, p95, p99,
                    );
                }

                // Per-zone profiler breakdown
                let zones_map = self.frame_profiler.all_zone_stats();
                if !zones_map.is_empty() {
                    let _ = write!(hud_text, "\n--- Profiler (ms avg/min/max) ---");
                    let mut zones: Vec<_> = zones_map.into_iter().collect();
                    zones.sort_by(|a, b| a.0.cmp(b.0));
                    for (name, zs) in zones {
                        let _ = write!(
                            hud_text,
                            "\n{:<8}: {:>5.2} / {:>5.2} / {:>5.2}",
                            name, zs.avg_ms, zs.min_ms, zs.max_ms,
                        );
                    }
                }
            }

            // Update text texture (skips upload if content unchanged)
            self.update_hud_text_texture(&hud_text);

            // Compute panel dimensions from text texture
            let pad = 8.0f32;
            let text_w = self.hud_text_width as f32;
            let text_h = self.hud_text_height as f32;
            let hud_w = text_w + pad * 2.0;
            let hud_h = text_h + pad * 2.0;
            let hud_x = 10.0f32;
            let hud_y = 10.0f32;

            unsafe {
                // Draw background panel
                self.gl.use_program(Some(self.hud_program));
                self.gl.uniform_matrix_4_f32_slice(
                    self.hud_uniforms.projection.as_ref(),
                    false,
                    &proj,
                );
                self.gl
                    .uniform_4_f32(self.hud_uniforms.bg_color.as_ref(), 0.0, 0.0, 0.0, 0.7);
                self.gl
                    .uniform_4_f32(self.hud_uniforms.fg_color.as_ref(), 0.0, 1.0, 0.0, 1.0);
                self.gl
                    .uniform_2_f32(self.hud_uniforms.size.as_ref(), hud_w, hud_h);
                self.gl
                    .uniform_4_f32(self.hud_uniforms.rect.as_ref(), hud_x, hud_y, hud_w, hud_h);
                self.gl.bind_vertex_array(Some(self.quad_vao));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                // Draw text overlay
                if let Some(tex) = self.hud_text_texture {
                    self.gl.use_program(Some(self.hud_text_program));
                    self.gl.uniform_matrix_4_f32_slice(
                        self.hud_text_uniforms.projection.as_ref(),
                        false,
                        &proj,
                    );
                    self.gl.uniform_4_f32(
                        self.hud_text_uniforms.rect.as_ref(),
                        hud_x + pad,
                        hud_y + pad,
                        text_w,
                        text_h,
                    );
                    self.gl
                        .uniform_1_i32(self.hud_text_uniforms.texture.as_ref(), 0);
                    self.gl.active_texture(glow::TEXTURE0);
                    self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                    self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                }

                self.gl.bind_vertex_array(None);
                self.gl.use_program(None);
            }

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
        if !has_pending_screenshot {
            self.render_snap_preview(&proj);
        }

        // === Pass 5e: Annotations overlay ===
        if self.annotation_active && !self.annotation_strokes.is_empty() {
            self.render_annotations(&proj);
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

        // Schedule re-render if recording is active (need continuous frames)
        if self.recording_active {
            self.needs_render = true;
        }

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
        blur_sampling_margin, dirty_below_affects_backdrop, dirty_below_requires_full_blur_redraw,
        focus_highlight_style, intersect_gl_scissors, is_opaque_occluder,
        presented_scene_copy_plan, rect_covers_output, transformed_overlays_require_full_redraw,
        transition_capture_plan, wallpaper_blend_plan,
    };

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
