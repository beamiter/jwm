use glow::HasContext;
use super::{Compositor, Particle};

impl Compositor {
    /// Tick wobbly spring physics. Returns true if any wobbly is active.
    pub(super) fn tick_wobbly(&mut self) -> bool {
        if !self.wobbly_windows { return false; }
        let dt = 1.0 / 60.0; // approximate frame time
        let stiffness = self.wobbly_stiffness;
        let damping = self.wobbly_damping;
        let mut any_active = false;
        let mut to_clear = Vec::new();

        for (&win, wt) in self.windows.iter_mut() {
            if let Some(ref mut w) = wt.wobbly {
                let mut all_settled = true;
                for i in 0..4 {
                    for axis in 0..2 {
                        let offset = w.corner_offsets[i][axis];
                        let vel = w.corner_velocities[i][axis];
                        let accel = -stiffness * offset - damping * vel;
                        let new_vel = vel + accel * dt;
                        let new_offset = offset + new_vel * dt;
                        w.corner_offsets[i][axis] = new_offset;
                        w.corner_velocities[i][axis] = new_vel;
                        if new_offset.abs() > 0.1 || new_vel.abs() > 0.1 {
                            all_settled = false;
                        }
                    }
                }
                if all_settled {
                    to_clear.push(win);
                } else {
                    any_active = true;
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
    pub(super) fn tick_particles(&mut self) {
        let dt = 1.0 / 60.0;
        let gravity = self.particle_gravity;

        self.particle_systems.retain_mut(|sys| {
            sys.particles.retain_mut(|p| {
                p.vy += gravity * dt;
                p.x += p.vx * dt;
                p.y += p.vy * dt;
                p.lifetime -= dt;
                p.lifetime > 0.0
            });
            !sys.particles.is_empty()
        });
    }

    /// Render active particle systems.
    pub(super) fn render_particles(&self, proj: &[f32; 16]) {
        if self.particle_systems.is_empty() { return; }

        // Collect all particles into a flat buffer
        let mut data: Vec<f32> = Vec::new();
        let mut count = 0u32;
        for sys in &self.particle_systems {
            for p in &sys.particles {
                let life_frac = (p.lifetime / p.max_lifetime).clamp(0.0, 1.0);
                data.extend_from_slice(&[p.x, p.y, p.color[0], p.color[1], p.color[2], p.color[3], life_frac]);
                count += 1;
            }
        }

        if count == 0 { return; }

        unsafe {
            self.gl.use_program(Some(self.particle_program));
            self.gl.uniform_matrix_4_f32_slice(
                self.particle_uniforms.projection.as_ref(), false, proj,
            );
            self.gl.uniform_1_f32(self.particle_uniforms.point_size.as_ref(), 4.0);

            self.gl.enable(glow::PROGRAM_POINT_SIZE);
            self.gl.bind_vertex_array(Some(self.particle_vao));
            self.gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.particle_vbo));

            let byte_data: &[u8] = std::slice::from_raw_parts(
                data.as_ptr() as *const u8,
                data.len() * 4,
            );
            self.gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, byte_data, glow::DYNAMIC_DRAW);
            self.gl.draw_arrays(glow::POINTS, 0, count as i32);

            self.gl.disable(glow::PROGRAM_POINT_SIZE);
            self.gl.bind_buffer(glow::ARRAY_BUFFER, None);
            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
        }
    }

    /// Spawn particles when a window is removed (particle effect).
    pub(super) fn spawn_particles_for_window(&mut self, x: i32, y: i32, w: u32, h: u32) {
        if !self.particle_effects { return; }

        let count = self.particle_count as usize;
        let lifetime = self.particle_lifetime;
        let mut particles = Vec::with_capacity(count);

        let cols = (count as f32).sqrt().ceil() as u32;
        let rows = (count as u32 + cols - 1) / cols;

        for i in 0..count {
            let col = i as u32 % cols;
            let row = i as u32 / cols;

            let px = x as f32 + (col as f32 + 0.5) / cols as f32 * w as f32;
            let py = y as f32 + (row as f32 + 0.5) / rows as f32 * h as f32;

            // Random velocity (using simple deterministic hash)
            let hash = ((i * 2654435761) ^ (col as usize * 1597334677)) as f32;
            let vx = (hash % 200.0) - 100.0;
            let vy = -((hash / 200.0) % 300.0) - 50.0; // upward bias

            // Color from window position (gradient)
            let r = (col as f32 / cols as f32 * 0.5 + 0.5).clamp(0.3, 1.0);
            let g = (row as f32 / rows as f32 * 0.5 + 0.5).clamp(0.3, 1.0);
            let b = 0.8;

            particles.push(Particle {
                x: px, y: py,
                vx, vy,
                color: [r, g, b, 1.0],
                lifetime,
                max_lifetime: lifetime,
            });
        }

        self.particle_systems.push(super::ParticleSystem { particles });
        self.needs_render = true;
    }

    /// Advance fade animations. Returns true if any fades are still in progress.
    pub(super) fn tick_fades(&mut self) -> bool {
        let mut any_active = false;
        let mut to_remove = Vec::new();

        for (&win, wt) in self.windows.iter_mut() {
            // Fade animation
            if self.fading {
                if wt.fading_out {
                    wt.fade_opacity -= self.fade_out_step;
                    if wt.fade_opacity <= 0.0 {
                        wt.fade_opacity = 0.0;
                        to_remove.push(win);
                    } else {
                        any_active = true;
                    }
                } else if wt.fade_opacity < 1.0 {
                    wt.fade_opacity += self.fade_in_step;
                    if wt.fade_opacity >= 1.0 {
                        wt.fade_opacity = 1.0;
                    } else {
                        any_active = true;
                    }
                }
            }

            // Scale animation (window open/close zoom)
            if self.window_animation {
                let diff = wt.anim_scale_target - wt.anim_scale;
                if diff.abs() > 0.001 {
                    let step = if diff > 0.0 { self.fade_in_step } else { -self.fade_out_step };
                    wt.anim_scale += step;
                    if (wt.anim_scale_target - wt.anim_scale).abs() < 0.001
                        || (step > 0.0 && wt.anim_scale >= wt.anim_scale_target)
                        || (step < 0.0 && wt.anim_scale <= wt.anim_scale_target)
                    {
                        wt.anim_scale = wt.anim_scale_target;
                    } else {
                        any_active = true;
                    }
                }
            }
        }

        for win in to_remove {
            self.remove_window_immediate(win);
        }

        any_active
    }
}
