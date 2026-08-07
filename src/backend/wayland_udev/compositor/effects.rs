use super::*;
use crate::backend::compositor_common::effects::{
    MAX_PARTICLE_SYSTEMS, advance_progress, clamp_effect_dt, effect_noise, motion_trail_lifetime,
    particle_burst_count, sanitize_animation_dt, smoothing_alpha,
};
use crate::backend::compositor_common::genie::{
    GenieDirection, PreviewDirection, genie_progress, preview_lease_timeout, preview_motion,
    retarget_genie_timeline,
};
use smithay::backend::renderer::gles::ffi;

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

impl WaylandCompositor {
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
                        removed_live_window |= self.windows.remove(&window_id).is_some();
                    }
                    CompletedGenieAction::ClearMinimizedAndRestoreMarkers => {
                        self.minimized_windows.remove(&animation.window_id);
                        self.pending_genie_restores.remove(&animation.window_id);
                        self.genie_targets.remove(&animation.window_id);
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
        let estimated_bytes = crate::backend::compositor_common::genie::estimated_visual_bytes(
            animation.w,
            animation.h,
        );
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
        while crate::backend::compositor_common::genie::minimized_cache_over_budget(
            self.minimized_visuals.len(),
            self.minimized_visuals
                .values()
                .map(|visual| visual.estimated_bytes)
                .fold(0u64, u64::saturating_add),
        ) {
            let Some(oldest) = self
                .minimized_visuals
                .iter()
                .filter(|(candidate, _)| **candidate != window_id)
                .min_by_key(|(_, visual)| visual.cached_at)
                .map(|(&window_id, _)| window_id)
            else {
                break;
            };
            self.minimized_visuals.remove(&oldest);
        }
        self.force_full_damage_next = true;
        self.needs_render = true;
    }

    pub(crate) fn discard_minimized_visual(&mut self, window_id: u64) {
        self.minimized_windows.remove(&window_id);
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

    fn tick_dock_preview(&mut self) -> bool {
        let now = Instant::now();
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

#[cfg(test)]
mod tests {
    use super::{
        CompletedGenieAction, GenieDirection, MinimizeRestoreDisposition, completed_genie_action,
        take_minimize_restore_disposition,
    };
    use std::collections::HashSet;

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
}
