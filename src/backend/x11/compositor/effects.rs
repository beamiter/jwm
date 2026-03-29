use glow::HasContext;
use super::{Compositor, Particle};

impl Compositor {
    /// Tick wobbly grid spring-mass physics. Returns true if any wobbly is active.
    pub(super) fn tick_wobbly(&mut self) -> bool {
        if !self.wobbly_windows { return false; }
        let neighbor_k = self.wobbly_stiffness;
        let restore_k = self.wobbly_restore_stiffness;
        let damping = self.wobbly_damping;
        let mut any_active = false;
        let mut to_clear = Vec::new();

        let now = std::time::Instant::now();

        for (&win, wt) in self.windows.iter_mut() {
            if let Some(ref mut w) = wt.wobbly {
                let raw_dt = now.duration_since(w.last_tick).as_secs_f32();
                let dt = raw_dt.clamp(0.001, 0.033);
                w.last_tick = now;

                let n = w.grid_n;
                let sub_steps = 3;
                let sub_dt = dt / sub_steps as f32;

                for _ in 0..sub_steps {
                    // Compute forces for all nodes into a temporary buffer
                    let count = n * n;
                    let mut forces = vec![[0.0f32; 2]; count];

                    for row in 0..n {
                        for col in 0..n {
                            // Skip anchor node during drag
                            if w.dragging && row == w.anchor_row && col == w.anchor_col {
                                continue;
                            }
                            let idx = row * n + col;
                            let off = w.offsets[idx];
                            let vel = w.velocities[idx];

                            let mut fx = 0.0f32;
                            let mut fy = 0.0f32;

                            // Neighbor spring forces (up, down, left, right)
                            if row > 0 {
                                let ni = (row - 1) * n + col;
                                fx += neighbor_k * (w.offsets[ni][0] - off[0]);
                                fy += neighbor_k * (w.offsets[ni][1] - off[1]);
                            }
                            if row + 1 < n {
                                let ni = (row + 1) * n + col;
                                fx += neighbor_k * (w.offsets[ni][0] - off[0]);
                                fy += neighbor_k * (w.offsets[ni][1] - off[1]);
                            }
                            if col > 0 {
                                let ni = row * n + (col - 1);
                                fx += neighbor_k * (w.offsets[ni][0] - off[0]);
                                fy += neighbor_k * (w.offsets[ni][1] - off[1]);
                            }
                            if col + 1 < n {
                                let ni = row * n + (col + 1);
                                fx += neighbor_k * (w.offsets[ni][0] - off[0]);
                                fy += neighbor_k * (w.offsets[ni][1] - off[1]);
                            }

                            // Restore spring (pull back to rest position [0,0])
                            fx += -restore_k * off[0];
                            fy += -restore_k * off[1];

                            // Velocity damping
                            fx += -damping * vel[0];
                            fy += -damping * vel[1];

                            forces[idx] = [fx, fy];
                        }
                    }

                    // Symplectic Euler: update velocity then position
                    for row in 0..n {
                        for col in 0..n {
                            if w.dragging && row == w.anchor_row && col == w.anchor_col {
                                continue;
                            }
                            let idx = row * n + col;
                            w.velocities[idx][0] += forces[idx][0] * sub_dt;
                            w.velocities[idx][1] += forces[idx][1] * sub_dt;
                            w.offsets[idx][0] += w.velocities[idx][0] * sub_dt;
                            w.offsets[idx][1] += w.velocities[idx][1] * sub_dt;
                        }
                    }
                }

                // Check if settled
                let all_settled = w.offsets.iter().zip(w.velocities.iter()).all(|(o, v)| {
                    o[0].abs() < 0.1 && o[1].abs() < 0.1
                        && v[0].abs() < 0.1 && v[1].abs() < 0.1
                });

                if all_settled && !w.dragging {
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

    // =================================================================
    // Phase 3.1: Motion trail
    // =================================================================

    /// Record the current window position into the motion trail ring buffer.
    pub(super) fn update_motion_trail(&mut self, x11_win: u32, x: i32, y: i32) {
        if !self.motion_trail_enabled { return; }
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            wt.motion_trail.push_back((x, y));
            while wt.motion_trail.len() > self.motion_trail_frames as usize {
                wt.motion_trail.pop_front();
            }
        }
    }

    /// Clear the motion trail for a window (called on move end).
    pub(super) fn clear_motion_trail(&mut self, x11_win: u32) {
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            wt.motion_trail.clear();
        }
    }

    // =================================================================
    // Phase 3.2: Genie minimize tick
    // =================================================================

    /// Tick genie animations. Returns true if any are active.
    pub(super) fn tick_genie(&mut self) -> bool {
        if self.genie_active.is_empty() { return false; }
        let duration = std::time::Duration::from_millis(self.genie_duration_ms);
        let now = std::time::Instant::now();
        // Remove completed animations and clean up their textures
        self.genie_active.retain(|ga| {
            now.duration_since(ga.start) < duration
        });
        !self.genie_active.is_empty()
    }

    /// Start a genie animation for a window being removed.
    pub(super) fn start_genie_animation(&mut self, x11_win: u32, x: f32, y: f32, w: f32, h: f32) {
        if !self.genie_minimize { return; }
        // We need the window's GL texture — if it still exists, grab it
        // The texture won't be deleted because we prevent remove_window_immediate
        if let Some(wt) = self.windows.get(&x11_win) {
            self.genie_active.push(super::GenieAnimation {
                x11_win,
                start: std::time::Instant::now(),
                x, y, w, h,
                gl_texture: wt.gl_texture,
                has_rgba: wt.has_rgba,
            });
        }
    }

    // =================================================================
    // Phase 3.3: Ripple tick
    // =================================================================

    /// Tick ripple effects. Returns true if any are active.
    pub(super) fn tick_ripples(&mut self) -> bool {
        if self.ripple_active.is_empty() { return false; }
        let duration = std::time::Duration::from_secs_f32(self.ripple_duration);
        let now = std::time::Instant::now();
        self.ripple_active.retain(|r| {
            now.duration_since(r.start) < duration
        });
        !self.ripple_active.is_empty()
    }

    // =================================================================
    // Phase 3.4: Focus highlight tick
    // =================================================================

    /// Returns true if focus highlight is currently animating.
    pub(super) fn tick_focus_highlight(&self) -> bool {
        if !self.focus_highlight { return false; }
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
        if !self.window_tilt { return false; }
        let dt = dt.clamp(0.001, 0.05);
        let alpha = 1.0 - (-dt * self.tilt_speed).exp();
        self.tilt_current_x += (self.tilt_target_x - self.tilt_current_x) * alpha;
        self.tilt_current_y += (self.tilt_target_y - self.tilt_current_y) * alpha;
        let epsilon = 0.0001;
        let dx = (self.tilt_current_x - self.tilt_target_x).abs();
        let dy = (self.tilt_current_y - self.tilt_target_y).abs();
        if dx < epsilon && dy < epsilon {
            self.tilt_current_x = self.tilt_target_x;
            self.tilt_current_y = self.tilt_target_y;
        }
        self.tilt_current_x.abs() > epsilon || self.tilt_current_y.abs() > epsilon
            || dx > epsilon || dy > epsilon
    }

    /// Returns true if wallpaper crossfade is currently animating.
    pub(super) fn tick_wallpaper_crossfade(&mut self) -> bool {
        if !self.wallpaper_crossfade { return false; }
        if let Some(start) = self.wallpaper_transition_start {
            let elapsed = start.elapsed().as_millis() as u64;
            if elapsed >= self.wallpaper_crossfade_duration_ms {
                // Transition finished — clean up old texture
                if let Some(tex) = self.old_wallpaper_texture.take() {
                    unsafe { self.gl.delete_texture(tex); }
                }
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
