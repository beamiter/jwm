use super::*;
use smithay::backend::renderer::gles::ffi;

impl WaylandCompositor {
    /// Tick fade animations (fade-in on map, fade-out on unmap)
    pub(crate) fn tick_fades(&mut self, dt: f32) {
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
                let speed = 8.0 * dt;
                win.anim_scale += (win.anim_scale_target - win.anim_scale) * speed;
                if (win.anim_scale - win.anim_scale_target).abs() < 0.001 {
                    win.anim_scale = win.anim_scale_target;
                }
            } else if !self.window_animation_enabled {
                win.anim_scale = 1.0;
                win.anim_scale_target = 1.0;
            }

            // Ripple
            if self.ripple_on_open_enabled && win.ripple_active {
                win.ripple_progress += dt * 2.0;
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
        let duration = std::time::Duration::from_millis(self.genie_duration_ms);
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
        let spring_k = 800.0f32;
        let damping = 12.0f32;
        let restore_k = 200.0f32;

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
        let gravity = 500.0f32;
        for system in self.particle_systems.iter_mut() {
            system.age += dt;
            for p in system.particles.iter_mut() {
                p.vy += gravity * dt;
                p.x += p.vx * dt;
                p.y += p.vy * dt;
                p.life -= dt * 0.8;
            }
            system.particles.retain(|p| p.life > 0.0);
        }
        self.particle_systems.retain(|s| !s.particles.is_empty());
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
        let speed = 8.0 * dt;
        self.tilt_x += (self.tilt_target_x - self.tilt_x) * speed;
        self.tilt_y += (self.tilt_target_y - self.tilt_y) * speed;
    }

    /// Spawn particles for a closing window
    #[allow(dead_code)]
    pub(crate) fn spawn_particles_for_window(&mut self, x: i32, y: i32, w: u32, h: u32) {
        let mut particles = Vec::with_capacity(60);
        let cx = x as f32 + w as f32 * 0.5;
        let cy = y as f32 + h as f32 * 0.5;
        for i in 0..60 {
            let angle = (i as f32 / 60.0) * std::f32::consts::TAU;
            let speed = 100.0 + (i as f32 * 7.0) % 200.0;
            particles.push(Particle {
                x: cx + (i as f32 * 3.7).sin() * w as f32 * 0.3,
                y: cy + (i as f32 * 2.3).cos() * h as f32 * 0.3,
                vx: angle.cos() * speed,
                vy: angle.sin() * speed - 150.0,
                color: [
                    0.6 + (i as f32 * 0.01) % 0.4,
                    0.3 + (i as f32 * 0.007) % 0.3,
                    0.8,
                    1.0,
                ],
                life: 1.0,
            });
        }
        self.particle_systems.push(ParticleSystem {
            particles,
            age: 0.0,
        });
    }

    /// Render particle systems
    pub(crate) fn render_particles(&self, gl: &ffi::Gles2, projection: &[f32; 16]) {
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

            // Build vertex data: [x, y, r, g, b, a, life] per particle
            let mut data: Vec<f32> = Vec::new();
            for system in &self.particle_systems {
                for p in &system.particles {
                    data.push(p.x);
                    data.push(p.y);
                    data.push(p.color[0]);
                    data.push(p.color[1]);
                    data.push(p.color[2]);
                    data.push(p.color[3]);
                    data.push(p.life);
                }
            }

            if data.is_empty() {
                return;
            }

            gl.BindVertexArray(self.particle_vao);
            gl.BindBuffer(ffi::ARRAY_BUFFER, self.particle_vbo);
            gl.BufferData(
                ffi::ARRAY_BUFFER,
                (data.len() * 4) as isize,
                data.as_ptr() as *const _,
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

            let count = data.len() / 7;
            gl.DrawArrays(ffi::POINTS, 0, count as i32);

            gl.DisableVertexAttribArray(0);
            gl.DisableVertexAttribArray(1);
            gl.DisableVertexAttribArray(2);
            gl.BindVertexArray(self.quad_vao);
        }
    }
}
