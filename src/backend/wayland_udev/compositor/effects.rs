use super::*;
use crate::backend::compositor_common::effects::{
    MAX_PARTICLE_SYSTEMS, advance_progress, clamp_effect_dt, effect_noise, motion_trail_lifetime,
    particle_burst_count, sanitize_animation_dt, smoothing_alpha,
};
use crate::backend::compositor_common::genie::{
    GenieDirection, PreviewDirection, genie_progress, preview_lease_timeout, preview_motion,
    retarget_genie_timeline,
};
use smithay::backend::{
    allocator::format::get_bpp,
    renderer::{Texture, gles::ffi},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MinimizeRestoreDisposition {
    ReverseActiveRestore,
    AlreadyMinimizing,
    StartFresh { cancelled_pending_restore: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompletedGenieAction {
    CacheMinimizedAndRemoveLive,
    ClearMinimizedAndRestoreMarkers,
}

const fn completed_genie_action(direction: GenieDirection) -> CompletedGenieAction {
    match direction {
        GenieDirection::Minimize => CompletedGenieAction::CacheMinimizedAndRemoveLive,
        GenieDirection::Restore => CompletedGenieAction::ClearMinimizedAndRestoreMarkers,
    }
}

fn take_minimize_restore_disposition(
    pending_restores: &mut HashSet<u64>,
    window_id: u64,
    active_direction: Option<GenieDirection>,
) -> MinimizeRestoreDisposition {
    let cancelled_pending_restore = pending_restores.remove(&window_id);
    match active_direction {
        Some(GenieDirection::Restore) => MinimizeRestoreDisposition::ReverseActiveRestore,
        Some(GenieDirection::Minimize) => MinimizeRestoreDisposition::AlreadyMinimizing,
        None => MinimizeRestoreDisposition::StartFresh {
            cancelled_pending_restore,
        },
    }
}

fn late_minimized_visual_dimensions(
    texture_width: u32,
    texture_height: u32,
    content_uv: [f32; 4],
) -> Option<(f32, f32)> {
    let width = texture_width as f32 * content_uv[2];
    let height = texture_height as f32 * content_uv[3];
    (texture_width > 0
        && texture_height > 0
        && width.is_finite()
        && height.is_finite()
        && width > 0.0
        && height > 0.0)
        .then_some((width, height))
}

fn should_settle_pending_minimized_visual(
    pending: bool,
    restore_pending: bool,
    cached: bool,
    active_animation: bool,
    has_texture: bool,
) -> bool {
    pending && !restore_pending && !cached && !active_animation && has_texture
}

fn touch_retained_visual(cached_at: Option<&mut Instant>, now: Instant) -> bool {
    let Some(cached_at) = cached_at else {
        return false;
    };
    *cached_at = now;
    true
}

fn retained_lru_candidate<K: Copy + Eq>(
    entries: impl IntoIterator<Item = (K, Instant)>,
    protected: K,
) -> Option<K> {
    entries
        .into_iter()
        .filter(|(window, _)| *window != protected)
        .min_by_key(|(_, cached_at)| *cached_at)
        .map(|(window, _)| window)
}

const fn preview_loses_source_after_full_eviction(has_low_resolution: bool) -> bool {
    !has_low_resolution
}

// `GlesTexture::format()` describes the GL view when Smithay can expose it,
// but it is not always the allocation format retained by the handle. In the
// pinned Smithay revision, unsupported dma-buf formats are represented by an
// RGBA8 GL view and EGL external buffers may not expose a format at all. The
// same revision's GLES/import format table tops out at RGBA16F (64 bpp), so use
// that as the allocation upper bound while still honoring any larger format a
// future renderer reports.
const CONSERVATIVE_RETAINED_TEXTURE_BITS_PER_PIXEL: usize = 64;

fn retained_texture_allocation_bytes(
    buffer_width: u32,
    buffer_height: u32,
    reported_bits_per_pixel: Option<usize>,
) -> u64 {
    let bits_per_pixel = reported_bits_per_pixel
        .unwrap_or(CONSERVATIVE_RETAINED_TEXTURE_BITS_PER_PIXEL)
        .max(CONSERVATIVE_RETAINED_TEXTURE_BITS_PER_PIXEL);
    let bytes_per_pixel = u64::try_from(bits_per_pixel.div_ceil(8)).unwrap_or(u64::MAX);
    u64::from(buffer_width)
        .saturating_mul(u64::from(buffer_height))
        .saturating_mul(bytes_per_pixel)
}

fn retained_gles_texture_allocation_bytes(texture: &GlesTexture) -> u64 {
    retained_texture_allocation_bytes(
        texture.width(),
        texture.height(),
        texture.format().and_then(get_bpp),
    )
}

impl WaylandCompositor {
    /// Record an externally-observable use of a retained Dock texture. This
    /// deliberately runs on geometry, preview, and restore requests instead
    /// of render/tick, avoiding a write to the cache on every frame.
    pub(super) fn touch_minimized_visual(&mut self, window_id: u64, now: Instant) -> bool {
        touch_retained_visual(
            self.minimized_visuals
                .get_mut(&window_id)
                .map(|visual| &mut visual.cached_at),
            now,
        )
    }

    /// Convert late-arriving hidden client textures into retained Dock pixels
    /// without inventing a source rectangle for a Genie animation. This is
    /// primarily the compositor-create/recreate path: JWM's drawable scene no
    /// longer contains minimized windows, but the backend performs a one-shot
    /// hidden-surface import for ids in pending_minimized_visuals.
    pub(crate) fn settle_pending_minimized_visuals(&mut self) {
        if self.pending_minimized_visuals.is_empty() {
            return;
        }

        let candidates = self
            .pending_minimized_visuals
            .iter()
            .copied()
            .filter(|window_id| {
                should_settle_pending_minimized_visual(
                    true,
                    self.pending_genie_restores.contains(window_id),
                    self.minimized_visuals.contains_key(window_id),
                    self.genie_active
                        .iter()
                        .any(|animation| animation.window_id == *window_id),
                    self.windows
                        .get(window_id)
                        .is_some_and(|window| window.texture_owner.is_some()),
                )
            })
            .collect::<Vec<_>>();

        let mut removed_live_window = false;
        for window_id in candidates {
            let Some((texture_owner, has_alpha, y_inverted, content_uv, color_transform, w, h)) =
                self.windows.get(&window_id).and_then(|window| {
                    let texture_owner = window.texture_owner.clone()?;
                    let (w, h) = late_minimized_visual_dimensions(
                        window.width,
                        window.height,
                        window.content_uv,
                    )?;
                    Some((
                        texture_owner,
                        window.has_alpha,
                        window.y_inverted,
                        window.content_uv,
                        window.color_transform,
                        w,
                        h,
                    ))
                })
            else {
                continue;
            };
            let animation = super::GenieAnimation {
                window_id,
                start: Instant::now(),
                start_progress: 1.0,
                direction: GenieDirection::Minimize,
                // Static cache construction does not consume x/y. Avoid using
                // the client's deliberately off-screen hidden coordinates.
                x: 0.0,
                y: 0.0,
                w,
                h,
                texture_owner,
                has_alpha,
                y_inverted,
                content_uv,
                color_transform,
                target: self.genie_target_for(window_id),
            };
            self.cache_minimized_visual(animation);
            removed_live_window |= self
                .take_live_window_preserving_metadata(window_id)
                .is_some();
        }
        if removed_live_window {
            self.refresh_any_color_transform_active();
        }
    }

    /// Cancel a queued restore or reverse its active mesh before the ordinary
    /// minimize retirement path runs. Returns true when an existing animation
    /// already owns the request, guaranteeing one Genie per window.
    pub(crate) fn prepare_genie_minimize(&mut self, window_id: u64) -> bool {
        let active_index = self
            .genie_active
            .iter()
            .position(|animation| animation.window_id == window_id);
        let active_direction = active_index.map(|index| self.genie_active[index].direction);
        let disposition = take_minimize_restore_disposition(
            &mut self.pending_genie_restores,
            window_id,
            active_direction,
        );

        match disposition {
            MinimizeRestoreDisposition::ReverseActiveRestore => {
                let index = active_index.expect("active restore index disappeared");
                let now = Instant::now();
                let duration_secs = self.genie_duration_ms.max(1) as f32 / 1000.0;
                let animation = &mut self.genie_active[index];
                retarget_genie_timeline(
                    &mut animation.start,
                    &mut animation.start_progress,
                    &mut animation.direction,
                    GenieDirection::Minimize,
                    now,
                    duration_secs,
                );
                if let Some(window) = self.windows.get_mut(&window_id) {
                    window.fading_out = false;
                    window.is_genie_restoring = false;
                    window.is_genie_minimizing = true;
                    window.closing_rect =
                        Some((animation.x, animation.y, animation.w, animation.h));
                    window.fade_opacity = 1.0;
                }
                self.force_full_damage_next = true;
                self.needs_render = true;
                true
            }
            MinimizeRestoreDisposition::AlreadyMinimizing => {
                if let Some(window) = self.windows.get_mut(&window_id) {
                    window.is_genie_restoring = false;
                    window.is_genie_minimizing = true;
                }
                true
            }
            MinimizeRestoreDisposition::StartFresh {
                cancelled_pending_restore,
            } => {
                // update_window_texture marks a pending restore as restoring
                // before scene geometry is available.  Once that request is
                // cancelled, clear the provisional marker so retire_window is
                // allowed to start the new minimize.
                if cancelled_pending_restore && let Some(window) = self.windows.get_mut(&window_id)
                {
                    window.is_genie_restoring = false;
                    window.is_genie_minimizing = false;
                }
                false
            }
        }
    }

    /// Tick fade animations (fade-in on map, fade-out on unmap)
    pub(crate) fn tick_fades(&mut self, dt: f32) {
        let dt = sanitize_animation_dt(dt);
        for (_id, win) in self.windows.iter_mut() {
            if self.fading_enabled {
                if win.fading_out {
                    win.fade_opacity -= self.fade_out_step * dt * 60.0;
                    if win.fade_opacity <= 0.0 {
                        win.fade_opacity = 0.0;
                    }
                } else if win.fade_opacity < 1.0 {
                    win.fade_opacity += self.fade_in_step * dt * 60.0;
                    if win.fade_opacity > 1.0 {
                        win.fade_opacity = 1.0;
                    }
                }
            } else {
                // Snap immediately: no fade transition
                win.fade_opacity = if win.fading_out { 0.0 } else { 1.0 };
            }

            // Scale animation
            if self.window_animation_enabled && win.anim_scale != win.anim_scale_target {
                let alpha = smoothing_alpha(8.0, dt);
                win.anim_scale += (win.anim_scale_target - win.anim_scale) * alpha;
                if (win.anim_scale - win.anim_scale_target).abs() < 0.001 {
                    win.anim_scale = win.anim_scale_target;
                }
            } else if !self.window_animation_enabled {
                win.anim_scale = 1.0;
                win.anim_scale_target = 1.0;
            }

            // Ripple
            if self.ripple_on_open_enabled && win.ripple_active {
                win.ripple_progress =
                    advance_progress(win.ripple_progress, dt, self.ripple_duration);
                if win.ripple_progress >= 1.0 {
                    win.ripple_active = false;
                    win.ripple_progress = 0.0;
                }
            } else if !self.ripple_on_open_enabled {
                win.ripple_active = false;
                win.ripple_progress = 0.0;
            }
        }
        // Remove fully faded-out windows. Keep the color-management fast-path
        // flag in sync because retired WindowState values are the only owners
        // it describes (retained effects have separate composition blockers).
        let live_window_count = self.windows.len();
        self.windows
            .retain(|_id, win| !(win.fading_out && win.fade_opacity <= 0.0));
        if self.windows.len() != live_window_count {
            self.refresh_any_color_transform_active();
        }
    }

    pub(crate) fn genie_target_for(&self, window_id: u64) -> crate::backend::api::CompositorRect {
        self.genie_targets
            .get(&window_id)
            .copied()
            .unwrap_or_else(|| {
                crate::backend::api::CompositorRect::new(self.dock_x, self.dock_y, 1.0, 1.0)
            })
    }

    /// Begin queued restores only once the backend has supplied both the new
    /// live texture and the final scene geometry. The live scene entry is then
    /// suppressed until the reverse mesh reaches progress zero.
    pub(crate) fn start_pending_genie_restores(&mut self, scene: &[(u64, i32, i32, u32, u32)]) {
        if self.pending_genie_restores.is_empty() || !self.genie_minimize_enabled {
            return;
        }

        let pending: Vec<u64> = self.pending_genie_restores.iter().copied().collect();
        let duration_secs = self.genie_duration_ms.max(1) as f32 / 1000.0;
        for window_id in pending {
            let Some(&(_, x, y, w, h)) = scene
                .iter()
                .find(|&&(candidate, _, _, _, _)| candidate == window_id)
            else {
                continue;
            };
            let Some((live_texture, has_alpha, y_inverted, content_uv, live_color_transform)) =
                self.windows.get(&window_id).and_then(|window| {
                    window.texture_owner.clone().map(|texture| {
                        (
                            texture,
                            window.has_alpha,
                            window.y_inverted,
                            window.content_uv,
                            window.color_transform,
                        )
                    })
                })
            else {
                continue;
            };

            let now = Instant::now();
            let target = self.genie_target_for(window_id);
            let (x, y, w, h) = (x as f32, y as f32, w as f32, h as f32);
            if let Some(index) = self
                .genie_active
                .iter()
                .position(|animation| animation.window_id == window_id)
            {
                let progress = {
                    let animation = &self.genie_active[index];
                    genie_progress(
                        animation.start_progress,
                        animation.direction,
                        now.duration_since(animation.start).as_secs_f32(),
                        duration_secs,
                    )
                    .0
                };
                let animation = &mut self.genie_active[index];
                animation.start = now;
                animation.start_progress = progress;
                animation.direction = GenieDirection::Restore;
                animation.x = x;
                animation.y = y;
                animation.w = w;
                animation.h = h;
                animation.target = target;
            } else if let Some(visual) = self.minimized_visuals.remove(&window_id) {
                self.genie_active.push(super::GenieAnimation {
                    window_id,
                    start: now,
                    start_progress: 1.0,
                    direction: GenieDirection::Restore,
                    x,
                    y,
                    w,
                    h,
                    texture_owner: visual.texture_owner,
                    has_alpha: visual.has_alpha,
                    y_inverted: visual.y_inverted,
                    content_uv: visual.content_uv,
                    color_transform: visual.color_transform,
                    target,
                });
            } else {
                // An LRU eviction must not turn restore into a hard pop. The
                // fresh live surface is already strongly owned by WindowState,
                // so a clone is sufficient for the short reverse animation.
                self.genie_active.push(super::GenieAnimation {
                    window_id,
                    start: now,
                    start_progress: 1.0,
                    direction: GenieDirection::Restore,
                    x,
                    y,
                    w,
                    h,
                    texture_owner: live_texture,
                    has_alpha,
                    y_inverted,
                    content_uv,
                    color_transform: live_color_transform,
                    target,
                });
            }

            if let Some(window) = self.windows.get_mut(&window_id) {
                window.fading_out = false;
                window.is_genie_minimizing = false;
                window.is_genie_restoring = true;
                window.closing_rect = None;
                window.fade_opacity = 1.0;
                window.anim_scale = 1.0;
                window.anim_scale_target = 1.0;
                window.ripple_active = false;
                window.ripple_progress = 0.0;
            }
            self.pending_genie_restores.remove(&window_id);
            self.force_full_damage_next = true;
            self.needs_render = true;
        }
    }

    /// Tick reversible Genie animations and the leased Dock preview.
    pub(crate) fn tick_genie(&mut self) -> bool {
        let preview_active = self.tick_dock_preview();
        let duration_secs = self.genie_duration_ms.max(1) as f32 / 1000.0;
        let now = Instant::now();
        let mut i = 0;
        let mut removed_live_window = false;
        while i < self.genie_active.len() {
            let (_, done) = {
                let animation = &self.genie_active[i];
                genie_progress(
                    animation.start_progress,
                    animation.direction,
                    now.duration_since(animation.start).as_secs_f32(),
                    duration_secs,
                )
            };
            if done {
                let animation = self.genie_active.remove(i);
                match completed_genie_action(animation.direction) {
                    CompletedGenieAction::CacheMinimizedAndRemoveLive => {
                        let window_id = animation.window_id;
                        self.cache_minimized_visual(animation);
                        removed_live_window |= self
                            .take_live_window_preserving_metadata(window_id)
                            .is_some();
                    }
                    CompletedGenieAction::ClearMinimizedAndRestoreMarkers => {
                        self.discard_minimized_snapshot(animation.window_id);
                        self.minimized_windows.remove(&animation.window_id);
                        self.pending_minimized_visuals.remove(&animation.window_id);
                        self.pending_genie_restores.remove(&animation.window_id);
                        self.genie_targets.remove(&animation.window_id);
                        self.minimized_window_metadata.remove(&animation.window_id);
                        if let Some(window) = self.windows.get_mut(&animation.window_id) {
                            window.is_genie_minimizing = false;
                            window.is_genie_restoring = false;
                            window.fade_opacity = 1.0;
                        }
                        if self
                            .dock_preview
                            .as_ref()
                            .is_some_and(|preview| preview.window_id == animation.window_id)
                        {
                            self.set_minimized_window_preview(None);
                        }
                    }
                }
                self.force_full_damage_next = true;
                self.needs_render = true;
            } else {
                i += 1;
            }
        }
        if removed_live_window {
            self.refresh_any_color_transform_active();
        }
        !self.genie_active.is_empty() || preview_active
    }

    pub(crate) fn cache_minimized_visual(&mut self, animation: super::GenieAnimation) {
        let window_id = animation.window_id;
        self.arm_minimized_snapshot_capture(window_id);
        self.pending_minimized_visuals.remove(&window_id);
        let estimated_bytes = retained_gles_texture_allocation_bytes(&animation.texture_owner);
        self.minimized_visuals.insert(
            window_id,
            super::MinimizedVisual {
                w: animation.w,
                h: animation.h,
                texture_owner: animation.texture_owner,
                has_alpha: animation.has_alpha,
                y_inverted: animation.y_inverted,
                content_uv: animation.content_uv,
                color_transform: animation.color_transform,
                target: self.genie_targets.get(&window_id).copied(),
                cached_at: Instant::now(),
                estimated_bytes,
            },
        );
        self.resume_minimized_preview_after_capture(window_id);
        let mut recapture_after_eviction = Vec::new();
        while crate::backend::compositor_common::genie::minimized_cache_over_budget(
            self.minimized_visuals.len(),
            self.minimized_visuals
                .values()
                .map(|visual| visual.estimated_bytes)
                .fold(0u64, u64::saturating_add),
        ) {
            let Some(oldest) = retained_lru_candidate(
                self.minimized_visuals
                    .iter()
                    .map(|(&candidate, visual)| (candidate, visual.cached_at)),
                window_id,
            ) else {
                break;
            };
            if self.minimized_visuals.remove(&oldest).is_some() {
                self.pending_minimized_visuals.remove(&oldest);
                let preview_matches = self
                    .dock_preview
                    .as_ref()
                    .is_some_and(|preview| preview.window_id == oldest);
                let low_resolution_available =
                    self.minimized_low_resolution_source_available(oldest);
                if low_resolution_available {
                    // If the bounded tier is CPU-only because its raw texture
                    // was independently evicted, losing the full owner is a
                    // new display demand and permits one lazy re-upload.
                    self.touch_minimized_snapshot(oldest);
                }
                let loses_source =
                    preview_loses_source_after_full_eviction(low_resolution_available);
                if loses_source {
                    suspend_preview_for_eviction(
                        self.dock_preview.as_mut(),
                        |preview| preview.window_id == oldest,
                        |preview| {
                            let now = Instant::now();
                            preview.started = now;
                            preview.start_opacity = 0.0;
                            preview.start_scale = 0.86;
                            preview.opacity = 0.0;
                            preview.scale = 0.86;
                            preview.awaiting_source = true;
                        },
                    );
                }
                if loses_source || preview_matches {
                    recapture_after_eviction.push(oldest);
                }
            }
        }
        for window_id in recapture_after_eviction {
            // A low-resolution snapshot keeps the card/preview drawable. An
            // active hover still requests full pixels in the background; a
            // rare missing low-res tier rearms the existing static fallback.
            self.arm_static_minimized_capture(window_id);
        }
        self.force_full_damage_next = true;
        self.needs_render = true;
    }

    pub(crate) fn discard_minimized_visual(&mut self, window_id: u64) {
        self.discard_minimized_snapshot(window_id);
        self.minimized_windows.remove(&window_id);
        self.pending_minimized_visuals.remove(&window_id);
        self.pending_genie_restores.remove(&window_id);
        self.genie_targets.remove(&window_id);
        self.genie_active
            .retain(|animation| animation.window_id != window_id);
        self.minimized_visuals.remove(&window_id);
        if self
            .dock_preview
            .as_ref()
            .is_some_and(|preview| preview.window_id == window_id)
        {
            self.dock_preview = None;
        }
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.is_genie_minimizing = false;
            window.is_genie_restoring = false;
        }
        self.force_full_damage_next = true;
        self.needs_render = true;
    }

    /// Drop every compositor-owned representation of a client that remains
    /// hidden in JWM but has left the Dock projection. This is deliberately
    /// stronger than geometry withdrawal and deliberately weaker than a
    /// restore: no reverse Genie or visible live surface is created.
    pub(crate) fn forget_minimized_window_visual(&mut self, window_id: u64) {
        let preview_window = self.dock_preview.as_ref().map(|preview| preview.window_id);
        let removed_restore = self.pending_genie_restores.remove(&window_id);
        let mut resources =
            crate::backend::compositor_common::genie::take_forgotten_minimized_resources(
                window_id,
                &mut self.minimized_windows,
                &mut self.pending_minimized_visuals,
                &mut self.genie_targets,
                &mut self.genie_active,
                |animation| animation.window_id,
                &mut self.minimized_visuals,
                preview_window,
            );
        resources.state_changed |= removed_restore;
        resources.state_changed |= self.discard_minimized_snapshot(window_id);
        let live = self.take_live_window_preserving_metadata(window_id);
        resources.state_changed |= live.is_some();
        if resources.preview_removed {
            self.dock_preview = None;
        }
        self.pending_window_urgency.discard(window_id);
        self.predictive_render_mgr.remove_window(window_id);
        self.is_game_window.remove(&window_id);
        if live.is_some() {
            self.refresh_any_color_transform_active();
        }
        if resources.state_changed {
            self.force_full_damage_next = true;
            self.needs_render = true;
        }
        // Dropping `resources` releases the animation, retained visual, and
        // target-less live WindowState strong texture owners exactly once.
    }

    fn tick_dock_preview(&mut self) -> bool {
        let now = Instant::now();
        if let Some(window_id) = self
            .dock_preview
            .as_ref()
            .filter(|preview| preview.awaiting_source)
            .map(|preview| preview.window_id)
        {
            if !self.minimized_preview_source_available(window_id) {
                // Surface commits drive the pending import. Holding here
                // avoids a busy animation loop and preserves the full show
                // transition/lease for the first frame with real pixels.
                return false;
            }
            self.resume_minimized_preview_after_capture(window_id);
        }
        if self.dock_preview.as_ref().is_some_and(|preview| {
            preview_lease_timeout(preview.direction, now, preview.lease_deadline)
                == Some(std::time::Duration::ZERO)
        }) {
            self.set_minimized_window_preview(None);
        }
        let Some(preview) = self.dock_preview.as_mut() else {
            return false;
        };
        let (opacity, scale, done) = preview_motion(
            preview.start_opacity,
            preview.start_scale,
            preview.direction,
            now.duration_since(preview.started).as_secs_f32(),
        );
        preview.opacity = opacity;
        preview.scale = scale;
        if done {
            match preview.direction {
                PreviewDirection::Show => false,
                PreviewDirection::Hide => {
                    self.dock_preview = None;
                    self.force_full_damage_next = true;
                    self.needs_render = true;
                    false
                }
            }
        } else {
            true
        }
    }

    /// Tick wobbly window physics (spring-mass grid)
    pub(crate) fn tick_wobbly(&mut self, dt: f32) {
        if !self.wobbly_enabled {
            for (_id, win) in self.windows.iter_mut() {
                win.wobbly = None;
            }
            return;
        }
        let spring_k = self.wobbly_stiffness;
        let damping = self.wobbly_damping;
        let restore_k = self.wobbly_restore_stiffness;

        for (_id, win) in self.windows.iter_mut() {
            let wobbly = match win.wobbly.as_mut() {
                Some(w) => w,
                None => continue,
            };
            if !wobbly.tick_physics(dt, spring_k, restore_k, damping) {
                win.wobbly = None;
            }
        }
    }

    /// Tick particle systems
    pub(crate) fn tick_particles(&mut self, dt: f32) {
        if !self.particle_effects_enabled {
            self.particle_systems.clear();
            return;
        }
        let simulation_dt = clamp_effect_dt(dt);
        let lifetime_dt = sanitize_animation_dt(dt);
        let gravity = self.particle_gravity;
        for system in self.particle_systems.iter_mut() {
            system.age += lifetime_dt;
            for p in system.particles.iter_mut() {
                p.vy += gravity * simulation_dt;
                p.x += p.vx * simulation_dt;
                p.y += p.vy * simulation_dt;
                p.lifetime -= lifetime_dt;
            }
            system.particles.retain(|p| p.lifetime > 0.0);
        }
        self.particle_systems.retain(|s| !s.particles.is_empty());
    }

    /// Retire expired motion-trail samples using wall-clock time.
    pub(crate) fn tick_motion_trails(&mut self) {
        if !self.motion_trail_enabled || self.motion_trail_frames == 0 {
            for win in self.windows.values_mut() {
                win.motion_trail.clear();
            }
            return;
        }
        let now = std::time::Instant::now();
        let lifetime = motion_trail_lifetime(self.motion_trail_frames);
        for win in self.windows.values_mut() {
            win.motion_trail.retain_live(now, lifetime);
        }
    }

    /// Tick snap preview opacity animation
    pub(crate) fn tick_snap_preview(&mut self, dt: f32) {
        if self.snap_preview.is_some() {
            self.snap_preview_opacity += dt * 6.0;
            if self.snap_preview_opacity > 1.0 {
                self.snap_preview_opacity = 1.0;
            }
        } else {
            self.snap_preview_opacity -= dt * 6.0;
            if self.snap_preview_opacity < 0.0 {
                self.snap_preview_opacity = 0.0;
            }
        }
    }

    /// Tick overview mode animation
    pub(crate) fn tick_overview(&mut self, dt: f32) {
        if self.overview_active {
            self.overview_opacity += dt * 5.0;
            if self.overview_opacity > 1.0 {
                self.overview_opacity = 1.0;
            }
        } else if self.overview_opacity > 0.0 {
            self.overview_opacity -= dt * 5.0;
            if self.overview_opacity < 0.0 {
                self.overview_opacity = 0.0;
            }
        }
    }

    /// Tick tilt interpolation
    pub(crate) fn tick_tilt(&mut self, dt: f32) {
        if !self.window_tilt_enabled {
            self.tilt_x = 0.0;
            self.tilt_y = 0.0;
            self.tilt_target_x = 0.0;
            self.tilt_target_y = 0.0;
            return;
        }
        let alpha = smoothing_alpha(self.tilt_speed, dt);
        self.tilt_x += (self.tilt_target_x - self.tilt_x) * alpha;
        self.tilt_y += (self.tilt_target_y - self.tilt_y) * alpha;
        if (self.tilt_x - self.tilt_target_x).abs() < 0.0001 {
            self.tilt_x = self.tilt_target_x;
        }
        if (self.tilt_y - self.tilt_target_y).abs() < 0.0001 {
            self.tilt_y = self.tilt_target_y;
        }
    }

    /// Spawn particles for a closing window
    pub(crate) fn spawn_particles_for_window(&mut self, x: i32, y: i32, w: u32, h: u32) {
        if !self.particle_effects_enabled || w == 0 || h == 0 {
            return;
        }
        let count = particle_burst_count(self.particle_count);
        if count == 0 {
            return;
        }
        let lifetime = self.particle_lifetime.max(0.001);
        let mut particles = Vec::with_capacity(count);
        let cx = x as f32 + w as f32 * 0.5;
        let cy = y as f32 + h as f32 * 0.5;
        for i in 0..count {
            let seed = (i as u32)
                .wrapping_mul(0x9e37_79b9)
                .wrapping_add((x as u32).rotate_left(11))
                .wrapping_add((y as u32).rotate_left(21));
            let angle = effect_noise(seed) * std::f32::consts::TAU;
            let speed = 100.0 + effect_noise(seed ^ 0xa5a5_5a5a) * 220.0;
            particles.push(Particle {
                x: cx + (effect_noise(seed ^ 0x1357_9bdf) - 0.5) * w as f32 * 0.8,
                y: cy + (effect_noise(seed ^ 0x2468_ace0) - 0.5) * h as f32 * 0.8,
                vx: angle.cos() * speed,
                vy: angle.sin() * speed - 150.0,
                color: [
                    0.6 + (i as f32 * 0.01) % 0.4,
                    0.3 + (i as f32 * 0.007) % 0.3,
                    0.8,
                    1.0,
                ],
                lifetime,
                max_lifetime: lifetime,
            });
        }
        if self.particle_systems.len() >= MAX_PARTICLE_SYSTEMS {
            self.particle_systems.remove(0);
        }
        self.particle_systems.push(ParticleSystem {
            particles,
            age: 0.0,
        });
    }

    /// Render particle systems
    pub(crate) fn render_particles(&mut self, gl: &ffi::Gles2, projection: &[f32; 16]) {
        if self.particle_systems.is_empty() {
            return;
        }
        unsafe {
            gl.UseProgram(self.particle_program);
            gl.UniformMatrix4fv(
                gl.GetUniformLocation(
                    self.particle_program,
                    b"u_projection\0".as_ptr() as *const _,
                ),
                1,
                ffi::FALSE as u8,
                projection.as_ptr(),
            );
            gl.Uniform1f(
                gl.GetUniformLocation(
                    self.particle_program,
                    b"u_point_size\0".as_ptr() as *const _,
                ),
                8.0,
            );

            // Build vertex data: [x, y, r, g, b, a, normalized life].
            self.scratch_particle_data.clear();
            let expected_floats = self
                .particle_systems
                .iter()
                .map(|system| system.particles.len() * 7)
                .sum();
            self.scratch_particle_data.reserve(expected_floats);
            for system in &self.particle_systems {
                for p in &system.particles {
                    self.scratch_particle_data.push(p.x);
                    self.scratch_particle_data.push(p.y);
                    self.scratch_particle_data.push(p.color[0]);
                    self.scratch_particle_data.push(p.color[1]);
                    self.scratch_particle_data.push(p.color[2]);
                    self.scratch_particle_data.push(p.color[3]);
                    self.scratch_particle_data
                        .push((p.lifetime / p.max_lifetime).clamp(0.0, 1.0));
                }
            }

            if self.scratch_particle_data.is_empty() {
                return;
            }

            gl.BindVertexArray(self.particle_vao);
            gl.BindBuffer(ffi::ARRAY_BUFFER, self.particle_vbo);
            gl.BufferData(
                ffi::ARRAY_BUFFER,
                (self.scratch_particle_data.len() * std::mem::size_of::<f32>()) as isize,
                self.scratch_particle_data.as_ptr() as *const _,
                ffi::STREAM_DRAW,
            );

            // position: location 0, vec2
            gl.EnableVertexAttribArray(0);
            gl.VertexAttribPointer(0, 2, ffi::FLOAT, ffi::FALSE as u8, 28, std::ptr::null());
            // color: location 1, vec4
            gl.EnableVertexAttribArray(1);
            gl.VertexAttribPointer(1, 4, ffi::FLOAT, ffi::FALSE as u8, 28, (2 * 4) as *const _);
            // life: location 2, float
            gl.EnableVertexAttribArray(2);
            gl.VertexAttribPointer(2, 1, ffi::FLOAT, ffi::FALSE as u8, 28, (6 * 4) as *const _);

            let count = self.scratch_particle_data.len() / 7;
            gl.DrawArrays(ffi::POINTS, 0, count as i32);

            gl.DisableVertexAttribArray(0);
            gl.DisableVertexAttribArray(1);
            gl.DisableVertexAttribArray(2);
            gl.BindVertexArray(self.quad_vao);
        }
    }
}

/// Preserve a preview request across retained-cache eviction while allowing
/// the renderer-specific state to pause until a static recapture arrives.
fn suspend_preview_for_eviction<T>(
    preview: Option<&mut T>,
    matches_evicted_source: impl FnOnce(&T) -> bool,
    suspend: impl FnOnce(&mut T),
) -> bool {
    if let Some(preview) = preview
        && matches_evicted_source(preview)
    {
        suspend(preview);
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CompletedGenieAction, GenieDirection, MinimizeRestoreDisposition, completed_genie_action,
        late_minimized_visual_dimensions, preview_loses_source_after_full_eviction,
        retained_lru_candidate, retained_texture_allocation_bytes,
        should_settle_pending_minimized_visual, suspend_preview_for_eviction,
        take_minimize_restore_disposition, touch_retained_visual,
    };
    use std::collections::{HashMap, HashSet};
    use std::time::{Duration, Instant};

    #[test]
    fn visual_forget_drops_every_wayland_strong_owner_once() {
        let window = 42_u64;
        let mut minimized = HashSet::from([window]);
        let mut pending = HashSet::from([window]);
        let mut targets = HashMap::from([(window, "target")]);
        let mut animations = vec![(window, "first"), (window, "second")];
        let mut visuals = HashMap::from([(window, "retained texture")]);
        let mut live = HashMap::from([(window, "live texture")]);
        let mut metadata = HashMap::new();

        let first = crate::backend::compositor_common::genie::take_forgotten_minimized_resources(
            window,
            &mut minimized,
            &mut pending,
            &mut targets,
            &mut animations,
            |animation| animation.0,
            &mut visuals,
            Some(window),
        );
        let first_live =
            crate::backend::compositor_common::genie::take_live_window_preserving_metadata(
                window,
                &mut live,
                &mut metadata,
                |_| ("org.example.Player", true),
            );
        assert_eq!(first.animations.len(), 2);
        assert_eq!(first.visual, Some("retained texture"));
        assert_eq!(first_live, Some("live texture"));
        let &(class_name, is_pip) = metadata.get(&window).unwrap();
        assert_eq!(class_name, "org.example.Player");
        assert!(is_pip);
        assert!(first.preview_removed);
        assert!(first.state_changed);

        let second = crate::backend::compositor_common::genie::take_forgotten_minimized_resources(
            window,
            &mut minimized,
            &mut pending,
            &mut targets,
            &mut animations,
            |animation| animation.0,
            &mut visuals,
            None,
        );
        let second_live =
            crate::backend::compositor_common::genie::take_live_window_preserving_metadata(
                window,
                &mut live,
                &mut metadata,
                |_| unreachable!("an idempotent forget has no second live owner"),
            );
        assert!(second.animations.is_empty());
        assert!(second.visual.is_none());
        assert!(second_live.is_none());
        let &(class_name, is_pip) = metadata.get(&window).unwrap();
        assert_eq!(class_name, "org.example.Player");
        assert!(is_pip);
        assert!(!second.preview_removed);
        assert!(!second.state_changed);
    }

    #[test]
    fn minimize_cancels_a_restore_that_has_not_started() {
        let window_id = 42;
        let mut pending = HashSet::from([window_id]);

        assert_eq!(
            take_minimize_restore_disposition(&mut pending, window_id, None),
            MinimizeRestoreDisposition::StartFresh {
                cancelled_pending_restore: true,
            }
        );
        assert!(!pending.contains(&window_id));
    }

    #[test]
    fn active_restore_is_reversed_instead_of_spawning_a_second_genie() {
        let window_id = 42;
        let mut pending = HashSet::from([window_id]);

        assert_eq!(
            take_minimize_restore_disposition(
                &mut pending,
                window_id,
                Some(GenieDirection::Restore),
            ),
            MinimizeRestoreDisposition::ReverseActiveRestore
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn reversed_restore_finishes_as_a_cached_minimize_without_restore_markers() {
        // A reversed timeline now has Minimize direction. Its completion path
        // caches the strong texture and removes WindowState entirely, so no
        // `is_genie_restoring` marker can survive the final tick.
        assert_eq!(
            completed_genie_action(GenieDirection::Minimize),
            CompletedGenieAction::CacheMinimizedAndRemoveLive
        );
    }

    #[test]
    fn eviction_suspends_but_preserves_preview_intent() {
        let mut preview = Some((42_u64, false));

        assert!(suspend_preview_for_eviction(
            preview.as_mut(),
            |(window, _)| *window == 42,
            |(_, awaiting_source)| *awaiting_source = true,
        ));
        assert_eq!(preview, Some((42, true)));

        let mut other_preview = Some((7_u64, false));
        assert!(!suspend_preview_for_eviction(
            other_preview.as_mut(),
            |(window, _)| *window == 42,
            |(_, awaiting_source)| *awaiting_source = true,
        ));
        assert_eq!(other_preview, Some((7, false)));
    }

    #[test]
    fn low_resolution_snapshot_keeps_preview_drawable_after_full_lru_eviction() {
        assert!(!preview_loses_source_after_full_eviction(true));
        assert!(preview_loses_source_after_full_eviction(false));
    }

    #[test]
    fn preview_or_restore_touch_protects_an_old_retained_visual_from_lru_eviction() {
        let captured = Instant::now();
        let mut active_cached_at = captured;
        let idle_cached_at = captured + Duration::from_millis(1);
        let newest_cached_at = captured + Duration::from_millis(2);

        assert!(touch_retained_visual(
            Some(&mut active_cached_at),
            captured + Duration::from_millis(3),
        ));
        assert_eq!(
            retained_lru_candidate(
                [
                    (42_u64, active_cached_at),
                    (7_u64, idle_cached_at),
                    (99_u64, newest_cached_at),
                ],
                99,
            ),
            Some(7)
        );
    }

    #[test]
    fn hidpi_retained_visual_is_charged_for_physical_buffer_pixels() {
        let logical_rgba8_bytes =
            crate::backend::compositor_common::genie::estimated_visual_bytes(1280.0, 800.0);
        let retained_bytes = retained_texture_allocation_bytes(2560, 1600, Some(32));

        assert_eq!(retained_bytes, 32_768_000);
        assert!(retained_bytes > logical_rgba8_bytes);
    }

    #[test]
    fn transformed_hidpi_buffer_keeps_its_full_allocation_charge() {
        // A 90-degree buffer transform swaps the physical axes relative to a
        // logical 800x1200 surface. Allocation is still the full 2400x1600
        // buffer and is invariant under that axis swap.
        let transformed = retained_texture_allocation_bytes(2400, 1600, Some(64));
        let axes_swapped = retained_texture_allocation_bytes(1600, 2400, Some(64));
        let old_logical_charge =
            crate::backend::compositor_common::genie::estimated_visual_bytes(800.0, 1200.0);

        assert_eq!(transformed, 30_720_000);
        assert_eq!(transformed, axes_swapped);
        assert!(transformed > old_logical_charge);
    }

    #[test]
    fn retained_allocation_estimate_never_falls_below_pinned_gles_upper_bound() {
        assert_eq!(
            retained_texture_allocation_bytes(3840, 2160, None),
            66_355_200
        );
        assert_eq!(
            retained_texture_allocation_bytes(3840, 2160, Some(32)),
            66_355_200
        );
        assert_eq!(
            retained_texture_allocation_bytes(3840, 2160, Some(64)),
            66_355_200
        );
        assert_eq!(
            retained_texture_allocation_bytes(3840, 2160, Some(96)),
            99_532_800
        );
    }

    #[test]
    fn conservative_texture_charge_drives_existing_cache_budget() {
        let retained_bytes = retained_texture_allocation_bytes(4096, 2160, Some(64));

        assert!(
            crate::backend::compositor_common::genie::minimized_cache_over_budget(
                2,
                retained_bytes.saturating_mul(2),
            )
        );
        assert!(
            !crate::backend::compositor_common::genie::minimized_cache_over_budget(
                1,
                retained_bytes.saturating_mul(2),
            )
        );
    }

    #[test]
    fn late_minimized_texture_uses_the_content_viewport_dimensions() {
        assert_eq!(
            late_minimized_visual_dimensions(1200, 800, [0.1, 0.1, 0.5, 0.75]),
            Some((600.0, 600.0))
        );
        assert_eq!(
            late_minimized_visual_dimensions(0, 800, [0.0, 0.0, 1.0, 1.0]),
            None
        );
        assert_eq!(
            late_minimized_visual_dimensions(1200, 800, [0.0, 0.0, f32::NAN, 1.0]),
            None
        );
    }

    #[test]
    fn late_minimize_settles_only_after_texture_and_before_restore() {
        assert!(!should_settle_pending_minimized_visual(
            true, false, false, false, false
        ));
        assert!(should_settle_pending_minimized_visual(
            true, false, false, false, true
        ));
        assert!(!should_settle_pending_minimized_visual(
            true, true, false, false, true
        ));
        assert!(!should_settle_pending_minimized_visual(
            true, false, true, false, true
        ));
        assert!(!should_settle_pending_minimized_visual(
            true, false, false, true, true
        ));
    }
}
