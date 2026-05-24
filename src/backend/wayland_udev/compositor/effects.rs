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
        self.windows.retain(|_id, win| !(win.fading_out && win.fade_opacity <= 0.0));
    }

    /// Tick wobbly window physics (spring-mass grid)
    pub(crate) fn tick_wobbly(&mut self, dt: f32) {
        if !self.wobbly_enabled {
            for (_id, win) in self.windows.iter_mut() {
                win.wobbly = None;
            }
            return;
        }
        // Implementation: for each window with wobbly state, iterate 3 sub-steps:
        // For each node, compute spring forces from neighbors + restore force toward rest position
        // Apply velocity damping, integrate with symplectic euler
        let sub_steps = 3;
        let sub_dt = dt / sub_steps as f32;
        let spring_k = 800.0f32;
        let damping = 12.0f32;
        let restore_k = 200.0f32;

        for (_id, win) in self.windows.iter_mut() {
            let wobbly = match win.wobbly.as_mut() {
                Some(w) => w,
                None => continue,
            };
            let grid_n = wobbly.grid_n;

            for _ in 0..sub_steps {
                let mut forces: Vec<[f32; 2]> = vec![[0.0, 0.0]; grid_n * grid_n];

                for row in 0..grid_n {
                    for col in 0..grid_n {
                        let idx = row * grid_n + col;
                        let pos = wobbly.offsets[idx];

                        // Neighbor spring forces
                        let neighbors = [
                            if col > 0 { Some(idx - 1) } else { None },
                            if col < grid_n - 1 { Some(idx + 1) } else { None },
                            if row > 0 { Some(idx - grid_n) } else { None },
                            if row < grid_n - 1 { Some(idx + grid_n) } else { None },
                        ];

                        for neighbor_idx in neighbors.iter().flatten() {
                            let neighbor_pos = wobbly.offsets[*neighbor_idx];
                            let dx = neighbor_pos[0] - pos[0];
                            let dy = neighbor_pos[1] - pos[1];
                            forces[idx][0] += dx * spring_k;
                            forces[idx][1] += dy * spring_k;
                        }

                        // Restore force toward zero (rest position)
                        forces[idx][0] -= pos[0] * restore_k;
                        forces[idx][1] -= pos[1] * restore_k;

                        // Damping
                        forces[idx][0] -= wobbly.velocities[idx][0] * damping;
                        forces[idx][1] -= wobbly.velocities[idx][1] * damping;
                    }
                }

                // Symplectic Euler integration
                for i in 0..grid_n * grid_n {
                    // Skip anchor point if dragging
                    if wobbly.dragging {
                        let anchor_idx = wobbly.anchor_row * grid_n + wobbly.anchor_col;
                        if i == anchor_idx {
                            continue;
                        }
                    }
                    wobbly.velocities[i][0] += forces[i][0] * sub_dt;
                    wobbly.velocities[i][1] += forces[i][1] * sub_dt;
                    wobbly.offsets[i][0] += wobbly.velocities[i][0] * sub_dt;
                    wobbly.offsets[i][1] += wobbly.velocities[i][1] * sub_dt;
                }
            }

            // Check if wobbly has settled (all velocities and offsets near zero)
            if !wobbly.dragging {
                let settled = wobbly.offsets.iter().zip(wobbly.velocities.iter()).all(|(o, v)| {
                    o[0].abs() < 0.1 && o[1].abs() < 0.1 && v[0].abs() < 0.5 && v[1].abs() < 0.5
                });
                if settled {
                    // Clear wobbly state
                    win.wobbly = None;
                }
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
                color: [0.6 + (i as f32 * 0.01) % 0.4, 0.3 + (i as f32 * 0.007) % 0.3, 0.8, 1.0],
                life: 1.0,
            });
        }
        self.particle_systems.push(ParticleSystem { particles, age: 0.0 });
    }

    /// Render particle systems
    pub(crate) fn render_particles(&self, gl: &ffi::Gles2, projection: &[f32; 16]) {
        if self.particle_systems.is_empty() {
            return;
        }
        unsafe {
            gl.UseProgram(self.particle_program);
            gl.UniformMatrix4fv(
                gl.GetUniformLocation(self.particle_program, b"u_projection\0".as_ptr() as *const _),
                1, ffi::FALSE as u8, projection.as_ptr(),
            );
            gl.Uniform1f(
                gl.GetUniformLocation(self.particle_program, b"u_point_size\0".as_ptr() as *const _),
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
