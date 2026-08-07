use super::types::WindowTexture;
use super::{Compositor, Particle};
use crate::backend::compositor_common::effects::{
    MAX_PARTICLE_SYSTEMS, MotionTrailParams, clamp_effect_dt, effect_noise, motion_trail_lifetime,
    particle_burst_count, sanitize_animation_dt, smoothing_alpha,
};
use crate::backend::compositor_common::genie::{
    GenieDirection, PreviewDirection, genie_progress, preview_motion, retarget_genie_timeline,
};
use glow::HasContext;

use super::CompositorConnection;

/// Clock for effects whose state is advanced by a frame delta.
///
/// `FrameStats::last_frame_time` measures time since the previous completed
/// compositor frame.  That interval can include an arbitrarily long idle
/// period before a new window starts fading or a close burst is spawned.  A
/// newly-active effect must start with a zero delta and only accumulate time
/// while incremental effects remain active.
#[derive(Default)]
pub(super) struct EffectTickClock {
    last_tick: Option<std::time::Instant>,
}

impl EffectTickClock {
    pub(super) fn reset(&mut self) {
        self.last_tick = None;
    }

    pub(super) fn delta(&mut self, now: std::time::Instant, active: bool) -> f32 {
        if !active {
            self.reset();
            return 0.0;
        }

        self.last_tick
            .replace(now)
            .map(|last| now.saturating_duration_since(last).as_secs_f32())
            .unwrap_or(0.0)
    }

    pub(super) fn finish_frame(&mut self, still_active: bool) {
        if !still_active {
            self.reset();
        }
    }
}

/// Per-frame result of `tick_fades`.
///
/// `any` keeps the render loop pacing frames while anything is still fading —
/// every animated pixel needs a redraw, whoever owns it. `on_clients` only
/// reports animation on windows the WM manages: override-redirect overlays
/// (fcitx5's candidate list and input-method switcher, menus, tooltips, drag
/// icons) fade in and out on every keystroke, and letting those fades feed
/// the adaptive blur-quality downgrade made every frosted client visibly pump
/// between Full and Reduced blur for as long as a user typed Chinese.
#[derive(Clone, Copy, Default)]
pub(super) struct FadeTick {
    pub(super) any: bool,
    pub(super) on_clients: bool,
}

impl FadeTick {
    /// Fold one animating window into the frame-wide flags.
    pub(super) fn record(&mut self, is_override_redirect: bool) {
        self.any = true;
        if !is_override_redirect {
            self.on_clients = true;
        }
    }
}

impl<C: CompositorConnection> Compositor<C> {
    pub(super) fn incremental_effects_active(&self) -> bool {
        (!self.particle_systems.is_empty() && self.particle_effects)
            || self.windows.values().any(|wt| {
                (self.fading && (wt.fading_out || wt.fade_opacity < 1.0))
                    || (self.window_animation
                        && (wt.anim_scale - wt.anim_scale_target).abs() > 0.001)
            })
    }

    /// Tick wobbly grid spring-mass physics. Returns true if any wobbly is active.
    pub(super) fn tick_wobbly(&mut self) -> bool {
        if !self.wobbly_windows {
            return false;
        }
        let neighbor_k = self.wobbly_stiffness;
        let restore_k = self.wobbly_restore_stiffness;
        let damping = self.wobbly_damping;
        let mut any_active = false;
        let mut to_clear = Vec::new();

        let now = std::time::Instant::now();

        for (&win, wt) in self.windows.iter_mut() {
            if let Some(ref mut w) = wt.wobbly {
                let dt = w.elapsed_dt(now);
                if w.tick_physics(dt, neighbor_k, restore_k, damping) {
                    any_active = true;
                } else {
                    to_clear.push(win);
                }
            }
        }

        for win in to_clear {
            if let Some(wt) = self.windows.get_mut(&win) {
                wt.wobbly = None;
            }
        }

        any_active
    }

    /// Tick particle systems. Removes dead particles and empty systems.
    pub(super) fn tick_particles(&mut self, dt: f32) -> bool {
        if !self.particle_effects {
            self.particle_systems.clear();
            return false;
        }
        let simulation_dt = clamp_effect_dt(dt);
        let lifetime_dt = sanitize_animation_dt(dt);
        let gravity = self.particle_gravity;

        self.particle_systems.retain_mut(|sys| {
            sys.particles.retain_mut(|p| {
                p.vy += gravity * simulation_dt;
                p.x += p.vx * simulation_dt;
                p.y += p.vy * simulation_dt;
                // Lifetime is a visual timeline, not a numerical integration:
                // catch it up after stalls while keeping motion bounded.
                p.lifetime -= lifetime_dt;
                p.lifetime > 0.0
            });
            !sys.particles.is_empty()
        });
        !self.particle_systems.is_empty()
    }

    /// Render active particle systems.
    pub(super) fn render_particles(&mut self, proj: &[f32; 16]) {
        if self.particle_systems.is_empty() {
            return;
        }

        // Collect all particles into a persistent flat buffer. Close bursts can
        // overlap, so allocating this vector on every animation frame showed up
        // prominently in allocator profiles.
        self.scratch_particle_data.clear();
        let expected_floats = self
            .particle_systems
            .iter()
            .map(|system| system.particles.len() * 7)
            .sum();
        self.scratch_particle_data.reserve(expected_floats);
        let mut count = 0u32;
        for sys in &self.particle_systems {
            for p in &sys.particles {
                let life_frac = (p.lifetime / p.max_lifetime).clamp(0.0, 1.0);
                self.scratch_particle_data.extend_from_slice(&[
                    p.x, p.y, p.color[0], p.color[1], p.color[2], p.color[3], life_frac,
                ]);
                count += 1;
            }
        }

        if count == 0 {
            return;
        }

        unsafe {
            self.gl.use_program(Some(self.particle_program));
            self.gl.uniform_matrix_4_f32_slice(
                self.particle_uniforms.projection.as_ref(),
                false,
                proj,
            );
            self.gl
                .uniform_1_f32(self.particle_uniforms.point_size.as_ref(), 4.0);

            self.gl.enable(glow::PROGRAM_POINT_SIZE);
            self.gl.bind_vertex_array(Some(self.particle_vao));
            self.gl
                .bind_buffer(glow::ARRAY_BUFFER, Some(self.particle_vbo));

            let byte_data: &[u8] = std::slice::from_raw_parts(
                self.scratch_particle_data.as_ptr() as *const u8,
                self.scratch_particle_data.len() * std::mem::size_of::<f32>(),
            );
            self.gl
                .buffer_data_u8_slice(glow::ARRAY_BUFFER, byte_data, glow::STREAM_DRAW);
            self.gl.draw_arrays(glow::POINTS, 0, count as i32);

            self.gl.disable(glow::PROGRAM_POINT_SIZE);
            self.gl.bind_buffer(glow::ARRAY_BUFFER, None);
            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
        }
    }

    /// Spawn particles when a window is removed (particle effect).
    pub(super) fn spawn_particles_for_window(&mut self, x: i32, y: i32, w: u32, h: u32) {
        if !self.particle_effects {
            return;
        }

        let count = particle_burst_count(self.particle_count);
        if count == 0 || w == 0 || h == 0 {
            return;
        }
        let lifetime = self.particle_lifetime.max(0.001);
        let mut particles = Vec::with_capacity(count);

        let cols = (count as f32).sqrt().ceil() as u32;
        let rows = (count as u32 + cols - 1) / cols;

        for i in 0..count {
            let col = i as u32 % cols;
            let row = i as u32 / cols;

            let px = x as f32 + (col as f32 + 0.5) / cols as f32 * w as f32;
            let py = y as f32 + (row as f32 + 0.5) / rows as f32 * h as f32;

            // Repeatable variation avoids pulling an RNG into the render thread
            // while still preventing visible rows of identical particles.
            let seed = (i as u32)
                .wrapping_mul(0x9e37_79b9)
                .wrapping_add((x as u32).rotate_left(11))
                .wrapping_add((y as u32).rotate_left(21));
            let vx = (effect_noise(seed) * 2.0 - 1.0) * 180.0;
            let vy = -(80.0 + effect_noise(seed ^ 0xa5a5_5a5a) * 300.0);

            // Color from window position (gradient)
            let r = (col as f32 / cols as f32 * 0.5 + 0.5).clamp(0.3, 1.0);
            let g = (row as f32 / rows as f32 * 0.5 + 0.5).clamp(0.3, 1.0);
            let b = 0.8;

            particles.push(Particle {
                x: px,
                y: py,
                vx,
                vy,
                color: [r, g, b, 1.0],
                lifetime,
                max_lifetime: lifetime,
            });
        }

        if self.particle_systems.len() >= MAX_PARTICLE_SYSTEMS {
            self.particle_systems.remove(0);
        }
        // A burst created between frames must not inherit time accumulated by
        // an older fade/particle system (for example across suspend/resume).
        self.effect_tick_clock.reset();
        self.particle_systems
            .push(super::ParticleSystem { particles });
        self.needs_render = true;
    }

    /// Advance fade animations.
    pub(super) fn tick_fades(&mut self, dt: f32) -> FadeTick {
        let frame_scale = sanitize_animation_dt(dt) * 60.0;
        let mut tick = FadeTick::default();
        let mut to_remove = Vec::new();

        for (&win, wt) in self.windows.iter_mut() {
            let mut window_active = false;

            // Fade animation
            if self.fading {
                if wt.fading_out {
                    wt.fade_opacity -= self.fade_out_step * frame_scale;
                    if wt.fade_opacity <= 0.0 {
                        wt.fade_opacity = 0.0;
                        to_remove.push(win);
                    } else {
                        window_active = true;
                    }
                } else if wt.fade_opacity < 1.0 {
                    wt.fade_opacity += self.fade_in_step * frame_scale;
                    if wt.fade_opacity >= 1.0 {
                        wt.fade_opacity = 1.0;
                    } else {
                        window_active = true;
                    }
                }
            }

            // Scale animation (window open/close zoom)
            if self.window_animation {
                let diff = wt.anim_scale_target - wt.anim_scale;
                if diff.abs() > 0.001 {
                    let step = if diff > 0.0 {
                        self.fade_in_step * frame_scale
                    } else {
                        -self.fade_out_step * frame_scale
                    };
                    wt.anim_scale += step;
                    if (wt.anim_scale_target - wt.anim_scale).abs() < 0.001
                        || (step > 0.0 && wt.anim_scale >= wt.anim_scale_target)
                        || (step < 0.0 && wt.anim_scale <= wt.anim_scale_target)
                    {
                        wt.anim_scale = wt.anim_scale_target;
                    } else {
                        window_active = true;
                    }
                }
            }

            if window_active {
                tick.record(wt.is_override_redirect);
            }
        }

        for win in to_remove {
            self.remove_window_immediate(win);
        }

        tick
    }

    // =================================================================
    // Phase 3.1: Motion trail
    // =================================================================

    /// Trail tuning for one window, combining config with the window's size.
    pub(super) fn motion_trail_params(&self, wt: &WindowTexture) -> MotionTrailParams {
        MotionTrailParams::new(
            if self.motion_trail_enabled {
                self.motion_trail_frames
            } else {
                0
            },
            self.motion_trail_opacity,
            wt.w as f32,
            wt.h as f32,
        )
    }

    /// Advance a window's motion trail by an interactive-move delta.
    pub(super) fn update_motion_trail(&mut self, x11_win: u32, dx: f32, dy: f32) {
        if !self.motion_trail_enabled {
            return;
        }
        let Some(params) = self
            .windows
            .get(&x11_win)
            .map(|wt| self.motion_trail_params(wt))
        else {
            return;
        };
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            // The window rect has already moved, so its pre-move origin seeds
            // the logical cursor when the drag was not announced through
            // move_start.
            let fallback = (wt.x as f32 - dx, wt.y as f32 - dy);
            wt.motion_trail.record_delta(dx, dy, fallback, &params);
        }
    }

    /// Expire motion-trail samples using wall-clock time.
    pub(super) fn tick_motion_trails(&mut self) -> bool {
        if !self.motion_trail_enabled || self.motion_trail_frames == 0 {
            for wt in self.windows.values_mut() {
                wt.motion_trail.clear();
            }
            return false;
        }
        let now = std::time::Instant::now();
        let lifetime = motion_trail_lifetime(self.motion_trail_frames);
        let mut active = false;
        for wt in self.windows.values_mut() {
            active |= wt.motion_trail.retain_live(now, lifetime);
        }
        active
    }

    // =================================================================
    // Phase 3.2: Genie minimize tick
    // =================================================================

    /// Tick genie animations. Returns true if any are active.
    pub(super) fn tick_genie(&mut self) -> bool {
        let preview_active = self.tick_dock_preview();
        let duration_secs = self.genie_duration_ms.max(1) as f32 / 1000.0;
        let now = std::time::Instant::now();
        let mut i = 0;
        while i < self.genie_active.len() {
            let (_, done) = genie_animation_progress(&self.genie_active[i], now, duration_secs);
            if done {
                let ga = self.genie_active.remove(i);
                match ga.direction {
                    GenieDirection::Minimize if ga.owns_resources => {
                        self.cache_minimized_visual(ga);
                    }
                    GenieDirection::Restore => {
                        if ga.owns_resources {
                            self.free_texture_resources(
                                ga.gl_texture,
                                ga.binding,
                                ga.pixmap,
                                ga.damage,
                            );
                        }
                        self.minimized_windows.remove(&ga.x11_win);
                        self.genie_targets.remove(&ga.x11_win);
                        if self
                            .dock_preview
                            .is_some_and(|preview| preview.x11_win == ga.x11_win)
                        {
                            self.set_minimized_window_preview(None);
                        }
                    }
                    GenieDirection::Minimize => {}
                }
                self.needs_render = true;
            } else {
                i += 1;
            }
        }
        !self.genie_active.is_empty() || preview_active
    }

    /// Start a genie animation for a window explicitly being minimized.
    ///
    /// Takes ownership of the window's GL texture + imported/X pixmap + damage by
    /// removing the WindowTexture from the live set and moving its resources
    /// into the animation. `tick_genie` frees them when the animation ends.
    /// This avoids both double-drawing the window and sampling a freed texture.
    pub(super) fn start_genie_animation(
        &mut self,
        x11_win: u32,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        animate: bool,
    ) {
        if let Some(wt) = self.windows.remove(&x11_win) {
            if self.unredirected_window == Some(x11_win) {
                self.unredirected_window = None;
            }
            self.needs_render = true;

            // Restore -> Minimize is a reversal of the one existing mesh, not
            // a second animation.  In the cached-visual branch the restore
            // already owns its old detached texture, so this newly imported
            // live entry must be released.  In the cache-eviction branch the
            // restore merely borrowed the live texture; removing WindowTexture
            // here transfers every native resource into that same animation.
            if let Some(index) = self
                .genie_active
                .iter()
                .position(|animation| animation.x11_win == x11_win)
            {
                let duration_secs = self.genie_duration_ms.max(1) as f32 / 1000.0;
                let now = std::time::Instant::now();
                let action =
                    reverse_restore_resource_action(self.genie_active[index].owns_resources);
                if self.genie_active[index].direction == GenieDirection::Restore {
                    let animation = &mut self.genie_active[index];
                    retarget_genie_timeline(
                        &mut animation.start,
                        &mut animation.start_progress,
                        &mut animation.direction,
                        GenieDirection::Minimize,
                        now,
                        duration_secs,
                    );
                }

                match action {
                    ReverseRestoreResourceAction::ReleaseLive => {
                        self.free_texture_resources(
                            wt.gl_texture,
                            wt.binding,
                            wt.pixmap,
                            wt.damage,
                        );
                    }
                    ReverseRestoreResourceAction::AdoptLive => {
                        let animation = &mut self.genie_active[index];
                        animation.gl_texture = wt.gl_texture;
                        animation.has_rgba = wt.has_rgba;
                        animation.binding = wt.binding;
                        animation.pixmap = wt.pixmap;
                        animation.damage = wt.damage;
                        animation.owns_resources = true;
                    }
                }
                return;
            }

            let animation = super::GenieAnimation {
                x11_win,
                start: std::time::Instant::now(),
                start_progress: 0.0,
                direction: GenieDirection::Minimize,
                x,
                y,
                w,
                h,
                gl_texture: wt.gl_texture,
                has_rgba: wt.has_rgba,
                target: self.genie_target_for(x11_win),
                owns_resources: true,
                binding: wt.binding,
                pixmap: wt.pixmap,
                damage: wt.damage,
            };
            if animate {
                self.genie_active.push(animation);
            } else {
                self.cache_minimized_visual(animation);
            }
        }
    }

    pub(super) fn genie_target_for(&self, x11_win: u32) -> crate::backend::api::CompositorRect {
        self.genie_targets
            .get(&x11_win)
            .copied()
            .unwrap_or_else(|| {
                crate::backend::api::CompositorRect::new(
                    (self.screen_w / 2) as f32,
                    self.screen_h.saturating_sub(1) as f32,
                    1.0,
                    1.0,
                )
                .normalized()
                .expect("one-pixel fallback Dock target is valid")
            })
    }

    /// Start or reverse the visual after the X window has been restored to
    /// its final live geometry by `add_window`.
    pub(super) fn start_genie_restore(&mut self, x11_win: u32, x: f32, y: f32, w: f32, h: f32) {
        if !self.minimized_windows.contains(&x11_win) {
            return;
        }
        if !self.genie_minimize {
            if let Some(visual) = self.minimized_visuals.remove(&x11_win) {
                self.free_texture_resources(
                    visual.gl_texture,
                    visual.binding,
                    visual.pixmap,
                    visual.damage,
                );
            }
            self.minimized_windows.remove(&x11_win);
            self.genie_targets.remove(&x11_win);
            if self
                .dock_preview
                .is_some_and(|preview| preview.x11_win == x11_win)
            {
                self.set_minimized_window_preview(None);
            }
            self.needs_render = true;
            return;
        }
        let now = std::time::Instant::now();
        let duration_secs = self.genie_duration_ms.max(1) as f32 / 1000.0;
        let target = self.genie_target_for(x11_win);
        if let Some(animation) = self
            .genie_active
            .iter_mut()
            .find(|animation| animation.x11_win == x11_win)
        {
            let (progress, _) = genie_animation_progress(animation, now, duration_secs);
            animation.start = now;
            animation.start_progress = progress;
            animation.direction = GenieDirection::Restore;
            animation.x = x;
            animation.y = y;
            animation.w = w;
            animation.h = h;
            animation.target = target;
            self.needs_render = true;
            return;
        }

        if let Some(visual) = self.minimized_visuals.remove(&x11_win) {
            self.genie_active.push(super::GenieAnimation {
                x11_win,
                start: now,
                start_progress: 1.0,
                direction: GenieDirection::Restore,
                x,
                y,
                w,
                h,
                gl_texture: visual.gl_texture,
                has_rgba: visual.has_rgba,
                target,
                owns_resources: true,
                binding: visual.binding,
                pixmap: visual.pixmap,
                damage: visual.damage,
            });
            self.needs_render = true;
            return;
        }

        // Cache eviction must not turn restore into a hard pop. Borrow the
        // freshly imported live texture and suppress its ordinary scene draw
        // until this animation reaches progress zero.
        if let Some(wt) = self.windows.get(&x11_win) {
            self.genie_active.push(super::GenieAnimation {
                x11_win,
                start: now,
                start_progress: 1.0,
                direction: GenieDirection::Restore,
                x,
                y,
                w,
                h,
                gl_texture: wt.gl_texture,
                has_rgba: wt.has_rgba,
                target,
                owns_resources: false,
                binding: None,
                pixmap: 0,
                damage: 0,
            });
            self.needs_render = true;
        }
    }

    pub(super) fn cache_minimized_visual(&mut self, ga: super::GenieAnimation) {
        let newest_window = ga.x11_win;
        let estimated_bytes =
            crate::backend::compositor_common::genie::estimated_visual_bytes(ga.w, ga.h);
        let visual = super::MinimizedVisual {
            w: ga.w,
            h: ga.h,
            gl_texture: ga.gl_texture,
            has_rgba: ga.has_rgba,
            target: self.genie_targets.get(&ga.x11_win).copied(),
            binding: ga.binding,
            pixmap: ga.pixmap,
            damage: ga.damage,
            cached_at: std::time::Instant::now(),
            estimated_bytes,
        };
        if let Some(old) = self.minimized_visuals.insert(newest_window, visual) {
            self.free_texture_resources(old.gl_texture, old.binding, old.pixmap, old.damage);
        }
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
                .filter(|(window, _)| **window != newest_window)
                .min_by_key(|(_, visual)| visual.cached_at)
                .map(|(&window, _)| window)
            else {
                break;
            };
            if let Some(old) = self.minimized_visuals.remove(&oldest) {
                self.free_texture_resources(old.gl_texture, old.binding, old.pixmap, old.damage);
            }
        }
    }

    pub(super) fn discard_minimized_visual(&mut self, x11_win: u32) {
        self.minimized_windows.remove(&x11_win);
        self.genie_targets.remove(&x11_win);
        if let Some(index) = self
            .genie_active
            .iter()
            .position(|animation| animation.x11_win == x11_win)
        {
            let animation = self.genie_active.remove(index);
            if animation.owns_resources {
                self.free_texture_resources(
                    animation.gl_texture,
                    animation.binding,
                    animation.pixmap,
                    animation.damage,
                );
            }
        }
        if let Some(visual) = self.minimized_visuals.remove(&x11_win) {
            self.free_texture_resources(
                visual.gl_texture,
                visual.binding,
                visual.pixmap,
                visual.damage,
            );
        }
        if self
            .dock_preview
            .is_some_and(|preview| preview.x11_win == x11_win)
        {
            self.dock_preview = None;
        }
    }

    fn tick_dock_preview(&mut self) -> bool {
        let now = std::time::Instant::now();
        if self.dock_preview.is_some_and(|preview| {
            crate::backend::compositor_common::genie::preview_lease_timeout(
                preview.direction,
                now,
                preview.lease_deadline,
            ) == Some(std::time::Duration::ZERO)
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
            now.saturating_duration_since(preview.started).as_secs_f32(),
        );
        preview.opacity = opacity;
        preview.scale = scale;
        if !done {
            return true;
        }
        if preview.direction == PreviewDirection::Hide {
            self.dock_preview = None;
        } else {
            preview.start_opacity = 1.0;
            preview.start_scale = 1.0;
            preview.opacity = 1.0;
            preview.scale = 1.0;
        }
        false
    }

    // =================================================================
    // Phase 3.3: Ripple tick
    // =================================================================

    /// Tick ripple effects. Returns true if any are active.
    pub(super) fn tick_ripples(&mut self) -> bool {
        if self.ripple_active.is_empty() {
            return false;
        }
        let duration = std::time::Duration::from_secs_f32(self.ripple_duration.max(f32::EPSILON));
        let now = std::time::Instant::now();
        self.ripple_active
            .retain(|r| now.duration_since(r.start) < duration);
        !self.ripple_active.is_empty()
    }

    // =================================================================
    // Phase 3.4: Focus highlight tick
    // =================================================================

    /// Returns true if focus highlight is currently animating.
    pub(super) fn tick_focus_highlight(&self) -> bool {
        if !self.focus_highlight {
            return false;
        }
        if let Some((_, start)) = self.focus_highlight_start {
            let elapsed = start.elapsed().as_millis() as u64;
            elapsed < self.focus_highlight_duration_ms
        } else {
            false
        }
    }

    // =================================================================
    // Phase 3.5: Wallpaper crossfade tick
    // =================================================================

    /// Tick tilt smooth interpolation. Returns true if tilt is visually active.
    pub(super) fn tick_tilt(&mut self, dt: f32) -> bool {
        if !self.window_tilt {
            return false;
        }
        let alpha = smoothing_alpha(self.tilt_speed, dt);
        self.tilt_current_x += (self.tilt_target_x - self.tilt_current_x) * alpha;
        self.tilt_current_y += (self.tilt_target_y - self.tilt_current_y) * alpha;
        let epsilon = 0.0001;
        let dx = (self.tilt_current_x - self.tilt_target_x).abs();
        let dy = (self.tilt_current_y - self.tilt_target_y).abs();
        if dx < epsilon && dy < epsilon {
            self.tilt_current_x = self.tilt_target_x;
            self.tilt_current_y = self.tilt_target_y;
        }
        dx > epsilon || dy > epsilon
    }

    /// Returns true if wallpaper crossfade is currently animating.
    pub(super) fn tick_wallpaper_crossfade(&mut self) -> bool {
        if !self.wallpaper_crossfade {
            return false;
        }
        if let Some(start) = self.wallpaper_transition_start {
            let elapsed = start.elapsed().as_millis() as u64;
            if elapsed >= self.wallpaper_crossfade_duration_ms {
                // Transition finished — clean up old texture
                if let Some(tex) = self.old_wallpaper_texture.take() {
                    unsafe {
                        self.gl.delete_texture(tex);
                    }
                }
                self.old_wallpaper_img_w = 0;
                self.old_wallpaper_img_h = 0;
                self.wallpaper_transition_start = None;
                false
            } else {
                true
            }
        } else {
            false
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReverseRestoreResourceAction {
    /// The reverse animation owns a detached minimized visual; the separate
    /// live entry removed for this minimize request must be released.
    ReleaseLive,
    /// The reverse animation borrowed this live entry; adopt all of it.
    AdoptLive,
}

const fn reverse_restore_resource_action(
    animation_owns_resources: bool,
) -> ReverseRestoreResourceAction {
    if animation_owns_resources {
        ReverseRestoreResourceAction::ReleaseLive
    } else {
        ReverseRestoreResourceAction::AdoptLive
    }
}

pub(super) fn genie_animation_progress(
    animation: &super::GenieAnimation,
    now: std::time::Instant,
    duration_secs: f32,
) -> (f32, bool) {
    genie_progress(
        animation.start_progress,
        animation.direction,
        now.saturating_duration_since(animation.start).as_secs_f32(),
        duration_secs,
    )
}

#[cfg(test)]
mod tests {
    use super::{
        EffectTickClock, FadeTick, ReverseRestoreResourceAction, reverse_restore_resource_action,
    };
    use std::time::{Duration, Instant};

    #[test]
    fn ime_popup_fades_do_not_count_as_client_fades() {
        // fcitx5's candidate list fading on every keystroke keeps the render
        // loop pacing frames (`any`) but must not feed the adaptive blur
        // downgrade (`on_clients`), or a lone frosted client pumps between
        // Full and Reduced blur the whole time a user types Chinese.
        let mut tick = FadeTick::default();
        assert!(!tick.any && !tick.on_clients);

        tick.record(true); // override-redirect IME popup
        assert!(tick.any);
        assert!(!tick.on_clients);

        tick.record(false); // managed client
        assert!(tick.any);
        assert!(tick.on_clients);
    }

    #[test]
    fn idle_time_is_not_applied_to_a_new_effect() {
        let mut clock = EffectTickClock::default();
        let start = Instant::now();

        assert_eq!(clock.delta(start, false), 0.0);
        assert_eq!(clock.delta(start + Duration::from_secs(30), true), 0.0);

        let frame_dt = clock.delta(
            start + Duration::from_secs(30) + Duration::from_millis(16),
            true,
        );
        assert!((frame_dt - 0.016).abs() < 0.000_001);
    }

    #[test]
    fn resetting_an_active_clock_protects_a_newly_spawned_effect() {
        let mut clock = EffectTickClock::default();
        let start = Instant::now();

        assert_eq!(clock.delta(start, true), 0.0);
        assert!(clock.delta(start + Duration::from_millis(16), true) > 0.0);

        clock.reset();
        assert_eq!(clock.delta(start + Duration::from_secs(10), true), 0.0);
    }

    #[test]
    fn finishing_the_last_effect_rearms_the_clock_for_the_next_one() {
        let mut clock = EffectTickClock::default();
        let start = Instant::now();

        assert_eq!(clock.delta(start, true), 0.0);
        clock.finish_frame(false);
        assert_eq!(clock.delta(start + Duration::from_secs(10), true), 0.0);
    }

    #[test]
    fn restore_reversal_releases_an_unrelated_live_texture_when_animation_owns_pixels() {
        assert_eq!(
            reverse_restore_resource_action(true),
            ReverseRestoreResourceAction::ReleaseLive
        );
    }

    #[test]
    fn restore_reversal_adopts_the_live_texture_when_animation_only_borrowed_pixels() {
        assert_eq!(
            reverse_restore_resource_action(false),
            ReverseRestoreResourceAction::AdoptLive
        );
    }
}
