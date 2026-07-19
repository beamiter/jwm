use super::*;
use crate::backend::compositor_common::effects::{
    MAX_PARTICLE_SYSTEMS, advance_progress, clamp_effect_dt, effect_noise, motion_trail_lifetime,
    particle_burst_count, sanitize_animation_dt, smoothing_alpha,
};
use smithay::backend::renderer::gles::ffi;

impl WaylandCompositor {
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
        // Remove fully faded-out windows
        self.windows
            .retain(|_id, win| !(win.fading_out && win.fade_opacity <= 0.0));
    }

    /// Tick genie minimize animations. Returns true if any are still active.
    ///
    /// When an animation completes, the corresponding WindowState is removed
    /// (it was kept alive past surface destruction so the genie pass could
    /// sample its EGL-imported texture).
    pub(crate) fn tick_genie(&mut self) -> bool {
        if self.genie_active.is_empty() {
            return false;
        }
        let duration = std::time::Duration::from_millis(self.genie_duration_ms.max(1));
        let now = std::time::Instant::now();
        let mut i = 0;
        while i < self.genie_active.len() {
            if now.duration_since(self.genie_active[i].start) >= duration {
                let ga = self.genie_active.remove(i);
                self.windows.remove(&ga.window_id);
                self.needs_render = true;
            } else {
                i += 1;
            }
        }
        !self.genie_active.is_empty()
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
            if !wobbly.tick_physics(dt, spring_k, restore_k, damping, 0.5) {
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
            win.motion_trail
                .retain(|sample| sample.opacity_at(now, lifetime) > 0.0);
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
