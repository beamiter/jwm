use glow::HasContext;
use super::Compositor;
use super::math::{perspective_matrix, translate_matrix, rotate_y_matrix, rotate_x_matrix, mat4_mul};

impl Compositor {
    /// Returns true if a tag-switch transition is in progress.
    pub(crate) fn transition_active(&self) -> bool {
        self.transition_start.is_some()
    }

    /// Compute transition progress (0.0 → 1.0). Returns None if no transition.
    pub(crate) fn transition_progress(&self, now: std::time::Instant) -> Option<f32> {
        let start = self.transition_start?;
        let elapsed = now.duration_since(start);
        if elapsed >= self.transition_duration {
            None // transition complete
        } else {
            let t = elapsed.as_secs_f32() / self.transition_duration.as_secs_f32();
            // EaseOut cubic for smooth slide deceleration.
            let inv = 1.0 - t;
            Some(1.0 - inv * inv * inv)
        }
    }

    /// Re-draw the wallpaper in a specific monitor region using the given
    /// ortho projection.  Used by transitions to keep the wallpaper static
    /// behind the animated content.
    pub(crate) fn draw_wallpaper_in_region(&self, proj: &[f32; 16], mon_x: i32, mon_y: i32, mon_w: u32, mon_h: u32) {
        // Find the matching monitor wallpaper entry
        let (tex, mode, iw, ih) = if let Some(mw) = self.monitor_wallpapers.iter().find(|mw| {
            mw.mon_x == mon_x && mw.mon_y == mon_y && mw.mon_w == mon_w && mw.mon_h == mon_h
        }) {
            if let Some(t) = mw.texture {
                (t, mw.mode, mw.img_w, mw.img_h)
            } else if let Some(t) = self.wallpaper_texture {
                (t, self.wallpaper_mode, self.wallpaper_img_w, self.wallpaper_img_h)
            } else {
                return;
            }
        } else if let Some(t) = self.wallpaper_texture {
            (t, self.wallpaper_mode, self.wallpaper_img_w, self.wallpaper_img_h)
        } else {
            return;
        };

        let area = (mon_x as f32, mon_y as f32, mon_w as f32, mon_h as f32);
        let (rx, ry, rw, rh) = Self::compute_wallpaper_rect(mode, area, iw, ih);

        unsafe {
            self.gl.use_program(Some(self.program));
            self.gl.uniform_matrix_4_f32_slice(
                self.win_uniforms.projection.as_ref(), false, proj,
            );
            self.gl.uniform_1_i32(self.win_uniforms.texture.as_ref(), 0);
            self.gl.bind_vertex_array(Some(self.quad_vao));
            self.gl.uniform_1_f32(self.win_uniforms.opacity.as_ref(), 1.0);
            self.gl.uniform_1_f32(self.win_uniforms.radius.as_ref(), 0.0);
            self.gl.uniform_1_f32(self.win_uniforms.dim.as_ref(), 1.0);
            self.gl.uniform_4_f32(self.win_uniforms.uv_rect.as_ref(), 0.0, 0.0, 1.0, 1.0);
            self.gl.active_texture(glow::TEXTURE0);

            self.gl.uniform_4_f32(self.win_uniforms.rect.as_ref(), rx, ry, rw, rh);
            self.gl.uniform_2_f32(self.win_uniforms.size.as_ref(), rw, rh);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

            self.gl.bind_texture(glow::TEXTURE_2D, None);
            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
        }
    }

    /// Render the 3D cube transition overlay.
    /// `progress` goes from 0.0 (old scene fully visible) to 1.0 (new scene fully visible).
    ///
    /// The two tags are adjacent faces of a cube. The cube rotates 90° around
    /// its vertical (Y) axis so the old front face turns away and the new side
    /// face turns in.  During the rotation both faces share an edge that is
    /// visible as a vertical line where the two tag contents meet.
    pub(crate) fn render_cube_transition(&mut self, progress: f32, ortho_proj: &[f32; 16]) {
        let old_tex = match &self.transition_fbo {
            Some((_, tex)) => *tex,
            None => return,
        };

        let mon_x = self.transition_mon_x;
        let mon_y = self.transition_mon_y;
        let mon_w = self.transition_mon_w;
        let mon_h = self.transition_mon_h;

        // Capture the current back-buffer (new scene) into transition_new_fbo
        if self.transition_new_fbo.is_none() {
            self.transition_new_fbo = unsafe {
                Self::create_scene_fbo(&self.gl, mon_w, mon_h).ok()
            };
        }
        let new_tex = match &self.transition_new_fbo {
            Some((fbo, tex)) => {
                let fbo = *fbo;
                let tex = *tex;
                let blit_gl_y = self.screen_h as i32 - (mon_y + mon_h as i32);
                unsafe {
                    self.gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
                    self.gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(fbo));
                    self.gl.blit_framebuffer(
                        mon_x, blit_gl_y,
                        mon_x + mon_w as i32, blit_gl_y + mon_h as i32,
                        0, 0, mon_w as i32, mon_h as i32,
                        glow::COLOR_BUFFER_BIT,
                        glow::NEAREST,
                    );
                    self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                }
                tex
            }
            None => return,
        };

        let exclude_top = self.transition_exclude_top.min(mon_h);
        let workspace_h = mon_h.saturating_sub(exclude_top);
        if workspace_h == 0 {
            return;
        }

        let aspect = mon_w as f32 / workspace_h as f32;
        let top_frac = if mon_h == 0 {
            0.0
        } else {
            exclude_top as f32 / mon_h as f32
        };
        // UV rect: workspace portion of the FBO texture (below status bar)
        let uv_rect = [0.0f32, 0.0, 1.0, 1.0 - top_frac];

        // --- Cube geometry ---
        // The face quad spans [-aspect, -1] to [+aspect, +1] in model space
        // (vertex shader: (pos * 2 - 1) * aspect for X, (pos * 2 - 1) for Y).
        // For a square cross-section (cube viewed from above), the half-depth
        // from center to each face equals the face half-width = aspect.
        let d = aspect;

        // Camera distance: face exactly fills screen when face-on at z=d,
        // fov_y=90° ⟹ camera_z = 1 + d.  Zoom out slightly at the midpoint
        // to keep the rotating cube corners within the viewport.
        let half_pi = std::f32::consts::FRAC_PI_2;
        let zoom = 1.0 + 0.25 * (progress * std::f32::consts::PI).sin();
        let camera_z = (1.0 + d) * zoom;

        let persp = perspective_matrix(half_pi, aspect, 0.1, camera_z * 3.0);
        let view = translate_matrix(0.0, 0.0, -camera_z);

        // Global rotation applied to the whole cube as a rigid body.
        // direction=+1 (forward): positive Y-rotation moves the front face
        //   left and brings the right face to the front.
        // direction=-1 (backward): vice-versa.
        let angle = self.transition_direction * progress * half_pi;
        let cube_rot = rotate_y_matrix(angle);

        // Old face: front face of the cube, at z = +d
        let old_model = mat4_mul(&cube_rot, &translate_matrix(0.0, 0.0, d));
        let old_mvp = mat4_mul(&persp, &mat4_mul(&view, &old_model));

        // New face: adjacent side face.  Start from the face template at z=+d,
        // then rotate it ∓90° so it sits on the appropriate side of the cube.
        // For direction=+1 the new face sits at x=+d (right side);
        // for direction=-1 it sits at x=-d (left side).
        let new_base = mat4_mul(
            &rotate_y_matrix(-self.transition_direction * half_pi),
            &translate_matrix(0.0, 0.0, d),
        );
        let new_model = mat4_mul(&cube_rot, &new_base);
        let new_mvp = mat4_mul(&persp, &mat4_mul(&view, &new_model));

        // Simulate directional lighting: the face that points more towards the
        // camera is brighter, the one turning away is dimmer.
        let old_brightness = (0.35 + 0.65 * (progress * half_pi).cos()).max(0.0);
        let new_brightness = (0.35 + 0.65 * (progress * half_pi).sin()).max(0.0);

        // OpenGL Y for the monitor's workspace region
        let scissor_gl_y = self.screen_h as i32 - (mon_y + mon_h as i32);
        unsafe {
            // Restrict rendering to the monitor's workspace area (below status bar)
            self.gl.enable(glow::SCISSOR_TEST);
            self.gl.scissor(mon_x, scissor_gl_y, mon_w as i32, workspace_h as i32);
            self.gl.viewport(mon_x, scissor_gl_y, mon_w as i32, workspace_h as i32);

            // Clear workspace area and draw wallpaper behind the cube so
            // the background stays static instead of showing black gaps.
            self.gl.clear(glow::COLOR_BUFFER_BIT);
            // Temporarily restore full viewport for wallpaper drawing
            self.gl.viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
            self.draw_wallpaper_in_region(ortho_proj, mon_x, mon_y, mon_w, mon_h);
            // Re-set viewport for cube 3D rendering
            self.gl.viewport(mon_x, scissor_gl_y, mon_w as i32, workspace_h as i32);

            self.gl.use_program(Some(self.cube_program));
            self.gl.uniform_1_f32(self.cube_uniforms.aspect.as_ref(), aspect);
            self.gl.uniform_1_i32(self.cube_uniforms.texture.as_ref(), 0);
            self.gl.uniform_4_f32(
                self.cube_uniforms.uv_rect.as_ref(),
                uv_rect[0], uv_rect[1], uv_rect[2], uv_rect[3],
            );
            self.gl.bind_vertex_array(Some(self.quad_vao));
            self.gl.active_texture(glow::TEXTURE0);

            // Painter's algorithm: draw the farther face first so the closer
            // face correctly occludes it.  At progress < 0.5 the old face is
            // closer; at progress > 0.5 the new face is closer.
            if progress < 0.5 {
                // New face farther → draw first
                self.gl.uniform_matrix_4_f32_slice(self.cube_uniforms.mvp.as_ref(), false, &new_mvp);
                self.gl.uniform_1_f32(self.cube_uniforms.brightness.as_ref(), new_brightness);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(new_tex));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                // Old face closer → draw second
                self.gl.uniform_matrix_4_f32_slice(self.cube_uniforms.mvp.as_ref(), false, &old_mvp);
                self.gl.uniform_1_f32(self.cube_uniforms.brightness.as_ref(), old_brightness);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(old_tex));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            } else {
                // Old face farther → draw first
                self.gl.uniform_matrix_4_f32_slice(self.cube_uniforms.mvp.as_ref(), false, &old_mvp);
                self.gl.uniform_1_f32(self.cube_uniforms.brightness.as_ref(), old_brightness);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(old_tex));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                // New face closer → draw second
                self.gl.uniform_matrix_4_f32_slice(self.cube_uniforms.mvp.as_ref(), false, &new_mvp);
                self.gl.uniform_1_f32(self.cube_uniforms.brightness.as_ref(), new_brightness);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(new_tex));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            }

            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);

            // Restore viewport and disable scissor
            self.gl.disable(glow::SCISSOR_TEST);
            self.gl.viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
        }
    }

    /// Flip transition: card-flip around Y axis (180° rotation).
    /// First half shows old scene flipping away, second half shows new scene
    /// flipping in. Uses 3D perspective via the cube shader infrastructure.
    pub(crate) fn render_flip_transition(&mut self, progress: f32, ortho_proj: &[f32; 16]) {
        let old_tex = match &self.transition_fbo {
            Some((_, tex)) => *tex,
            None => return,
        };

        let mon_x = self.transition_mon_x;
        let mon_y = self.transition_mon_y;
        let mon_w = self.transition_mon_w;
        let mon_h = self.transition_mon_h;

        // Capture new scene
        if self.transition_new_fbo.is_none() {
            self.transition_new_fbo = unsafe {
                Self::create_scene_fbo(&self.gl, mon_w, mon_h).ok()
            };
        }
        let new_tex = match &self.transition_new_fbo {
            Some((fbo, tex)) => {
                let fbo = *fbo;
                let tex = *tex;
                let blit_gl_y = self.screen_h as i32 - (mon_y + mon_h as i32);
                unsafe {
                    self.gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
                    self.gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(fbo));
                    self.gl.blit_framebuffer(
                        mon_x, blit_gl_y,
                        mon_x + mon_w as i32, blit_gl_y + mon_h as i32,
                        0, 0, mon_w as i32, mon_h as i32,
                        glow::COLOR_BUFFER_BIT, glow::NEAREST,
                    );
                    self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                }
                tex
            }
            None => return,
        };

        let exclude_top = self.transition_exclude_top.min(mon_h);
        let workspace_h = mon_h.saturating_sub(exclude_top);
        if workspace_h == 0 { return; }

        let aspect = mon_w as f32 / workspace_h as f32;
        let top_frac = if mon_h == 0 { 0.0 } else { exclude_top as f32 / mon_h as f32 };
        let uv_rect = [0.0f32, 0.0, 1.0, 1.0 - top_frac];

        let pi = std::f32::consts::PI;
        let half_pi = std::f32::consts::FRAC_PI_2;

        // Full 180° flip
        let angle = self.transition_direction * progress * pi;

        // Camera setup
        let d = 0.01; // face sits at z=d (nearly flat card)
        let camera_z = 1.0 + aspect;
        let persp = perspective_matrix(half_pi, aspect, 0.1, camera_z * 3.0);
        let view = translate_matrix(0.0, 0.0, -camera_z);

        let scissor_gl_y = self.screen_h as i32 - (mon_y + mon_h as i32);

        unsafe {
            self.gl.enable(glow::SCISSOR_TEST);
            self.gl.scissor(mon_x, scissor_gl_y, mon_w as i32, workspace_h as i32);
            self.gl.viewport(mon_x, scissor_gl_y, mon_w as i32, workspace_h as i32);

            // Draw wallpaper behind
            self.gl.clear(glow::COLOR_BUFFER_BIT);
            self.gl.viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
            self.draw_wallpaper_in_region(ortho_proj, mon_x, mon_y, mon_w, mon_h);
            self.gl.viewport(mon_x, scissor_gl_y, mon_w as i32, workspace_h as i32);

            self.gl.use_program(Some(self.cube_program));
            self.gl.uniform_1_f32(self.cube_uniforms.aspect.as_ref(), aspect);
            self.gl.uniform_1_i32(self.cube_uniforms.texture.as_ref(), 0);
            self.gl.uniform_4_f32(
                self.cube_uniforms.uv_rect.as_ref(),
                uv_rect[0], uv_rect[1], uv_rect[2], uv_rect[3],
            );
            self.gl.bind_vertex_array(Some(self.quad_vao));
            self.gl.active_texture(glow::TEXTURE0);

            // First half (0..0.5): show old face rotating away
            // Second half (0.5..1): show new face rotating in
            if progress < 0.5 {
                // Old face rotating away
                let model = mat4_mul(&rotate_y_matrix(angle), &translate_matrix(0.0, 0.0, d));
                let mvp = mat4_mul(&persp, &mat4_mul(&view, &model));
                let brightness = (1.0 - progress * 0.6).max(0.2);
                self.gl.uniform_matrix_4_f32_slice(self.cube_uniforms.mvp.as_ref(), false, &mvp);
                self.gl.uniform_1_f32(self.cube_uniforms.brightness.as_ref(), brightness);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(old_tex));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            } else {
                // New face rotating in from the back (pre-rotated 180°)
                let new_angle = angle - self.transition_direction * pi;
                let model = mat4_mul(&rotate_y_matrix(new_angle), &translate_matrix(0.0, 0.0, d));
                let mvp = mat4_mul(&persp, &mat4_mul(&view, &model));
                let brightness = (0.4 + (progress - 0.5) * 1.2).min(1.0);
                self.gl.uniform_matrix_4_f32_slice(self.cube_uniforms.mvp.as_ref(), false, &mvp);
                self.gl.uniform_1_f32(self.cube_uniforms.brightness.as_ref(), brightness);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(new_tex));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            }

            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
            self.gl.disable(glow::SCISSOR_TEST);
            self.gl.viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
        }
    }

    /// Blinds transition: screen split into vertical strips that flip
    /// individually with staggered timing to reveal the new scene.
    pub(crate) fn render_blinds_transition(&mut self, progress: f32, ortho_proj: &[f32; 16]) {
        let old_tex = match &self.transition_fbo {
            Some((_, tex)) => *tex,
            None => return,
        };

        let mon_x = self.transition_mon_x;
        let mon_y = self.transition_mon_y;
        let mon_w = self.transition_mon_w;
        let mon_h = self.transition_mon_h;

        // Capture new scene
        if self.transition_new_fbo.is_none() {
            self.transition_new_fbo = unsafe {
                Self::create_scene_fbo(&self.gl, mon_w, mon_h).ok()
            };
        }
        let new_tex = match &self.transition_new_fbo {
            Some((fbo, tex)) => {
                let fbo = *fbo;
                let tex = *tex;
                let blit_gl_y = self.screen_h as i32 - (mon_y + mon_h as i32);
                unsafe {
                    self.gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
                    self.gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(fbo));
                    self.gl.blit_framebuffer(
                        mon_x, blit_gl_y,
                        mon_x + mon_w as i32, blit_gl_y + mon_h as i32,
                        0, 0, mon_w as i32, mon_h as i32,
                        glow::COLOR_BUFFER_BIT, glow::NEAREST,
                    );
                    self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                }
                tex
            }
            None => return,
        };

        let exclude_top = self.transition_exclude_top.min(mon_h);
        let workspace_h = mon_h.saturating_sub(exclude_top);
        if workspace_h == 0 { return; }

        let top_frac = if mon_h == 0 { 0.0 } else { exclude_top as f32 / mon_h as f32 };
        let scissor_gl_y = self.screen_h as i32 - (mon_y + mon_h as i32);

        let num_blinds: u32 = 8;
        let strip_w = mon_w as f32 / num_blinds as f32;
        let stagger = 0.3; // how much strips are staggered (0 = all at once)

        unsafe {
            self.gl.enable(glow::SCISSOR_TEST);

            self.gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
            self.gl.use_program(Some(self.transition_program));
            self.gl.uniform_matrix_4_f32_slice(self.transition_uniforms.projection.as_ref(), false, ortho_proj);
            self.gl.uniform_1_i32(self.transition_uniforms.texture.as_ref(), 0);
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_vertex_array(Some(self.quad_vao));

            let draw_y = (mon_y as u32 + exclude_top) as f32;
            let draw_h = workspace_h as f32;

            for i in 0..num_blinds {
                // Staggered progress per strip
                let strip_delay = (i as f32 / (num_blinds - 1).max(1) as f32) * stagger;
                let strip_progress = ((progress - strip_delay) / (1.0 - stagger)).clamp(0.0, 1.0);

                let strip_x = mon_x as f32 + i as f32 * strip_w;

                // UV for this strip
                let uv_x = i as f32 / num_blinds as f32;
                let uv_w = 1.0 / num_blinds as f32;

                // Scissor to this strip
                let strip_scissor_x = mon_x + (i as f32 * strip_w) as i32;
                let strip_scissor_w = strip_w.ceil() as i32;
                self.gl.scissor(
                    strip_scissor_x,
                    scissor_gl_y,
                    strip_scissor_w,
                    (mon_h - exclude_top) as i32,
                );

                if strip_progress < 0.5 {
                    // Show old scene with horizontal squeeze (simulate flip first half)
                    let squeeze = 1.0 - strip_progress * 2.0; // 1.0 → 0.0
                    let squeezed_w = strip_w * squeeze;
                    let offset_x = strip_x + (strip_w - squeezed_w) * 0.5;
                    let old_uv = [uv_x, 0.0f32, uv_w, 1.0 - top_frac];
                    self.gl.uniform_4_f32(self.transition_uniforms.rect.as_ref(), offset_x, draw_y, squeezed_w.max(1.0), draw_h);
                    self.gl.uniform_1_f32(self.transition_uniforms.opacity.as_ref(), 1.0);
                    self.gl.uniform_4_f32(self.transition_uniforms.uv_rect.as_ref(), old_uv[0], old_uv[1], old_uv[2], old_uv[3]);
                    self.gl.bind_texture(glow::TEXTURE_2D, Some(old_tex));
                    self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                } else {
                    // Show new scene expanding (simulate flip second half)
                    let expand = (strip_progress - 0.5) * 2.0; // 0.0 → 1.0
                    let expanded_w = strip_w * expand;
                    let offset_x = strip_x + (strip_w - expanded_w) * 0.5;
                    let new_uv = [uv_x, 0.0f32, uv_w, 1.0 - top_frac];
                    self.gl.uniform_4_f32(self.transition_uniforms.rect.as_ref(), offset_x, draw_y, expanded_w.max(1.0), draw_h);
                    self.gl.uniform_1_f32(self.transition_uniforms.opacity.as_ref(), 1.0);
                    self.gl.uniform_4_f32(self.transition_uniforms.uv_rect.as_ref(), new_uv[0], new_uv[1], new_uv[2], new_uv[3]);
                    self.gl.bind_texture(glow::TEXTURE_2D, Some(new_tex));
                    self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                }
            }

            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
            self.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
            self.gl.disable(glow::SCISSOR_TEST);
        }
    }

    // =====================================================================
    // CoverFlow transition: 3D arc arrangement
    // =====================================================================

    /// CoverFlow transition: windows arranged in a 3D arc.
    /// Old scene on one side, new scene on the other; progress controls
    /// a camera slide along the arc.
    pub(crate) fn render_coverflow_transition(&mut self, progress: f32, ortho_proj: &[f32; 16]) {
        let old_tex = match &self.transition_fbo {
            Some((_, tex)) => *tex,
            None => return,
        };

        let mon_x = self.transition_mon_x;
        let mon_y = self.transition_mon_y;
        let mon_w = self.transition_mon_w;
        let mon_h = self.transition_mon_h;

        // Capture new scene
        if self.transition_new_fbo.is_none() {
            self.transition_new_fbo = unsafe {
                Self::create_scene_fbo(&self.gl, mon_w, mon_h).ok()
            };
        }
        let new_tex = match &self.transition_new_fbo {
            Some((fbo, tex)) => {
                let fbo = *fbo;
                let tex = *tex;
                let blit_gl_y = self.screen_h as i32 - (mon_y + mon_h as i32);
                unsafe {
                    self.gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
                    self.gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(fbo));
                    self.gl.blit_framebuffer(
                        mon_x, blit_gl_y,
                        mon_x + mon_w as i32, blit_gl_y + mon_h as i32,
                        0, 0, mon_w as i32, mon_h as i32,
                        glow::COLOR_BUFFER_BIT, glow::NEAREST,
                    );
                    self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                }
                tex
            }
            None => return,
        };

        let exclude_top = self.transition_exclude_top.min(mon_h);
        let workspace_h = mon_h.saturating_sub(exclude_top);
        if workspace_h == 0 { return; }

        let aspect = mon_w as f32 / workspace_h as f32;
        let top_frac = if mon_h == 0 { 0.0 } else { exclude_top as f32 / mon_h as f32 };
        let uv_rect = [0.0f32, 0.0, 1.0, 1.0 - top_frac];

        let half_pi = std::f32::consts::FRAC_PI_2;

        // CoverFlow geometry:
        // Center card faces the camera. Side cards are rotated 70 degrees
        // and offset laterally. Progress slides the view from old to new.
        let side_angle = 70.0f32.to_radians(); // side card tilt
        let side_offset = aspect * 0.8; // lateral displacement
        let d = 0.01; // face depth (nearly flat)
        let camera_z = 1.0 + aspect;
        let persp = perspective_matrix(half_pi, aspect, 0.1, camera_z * 3.0);
        let view = translate_matrix(0.0, 0.0, -camera_z);

        // Interpolate old card from center to left side, new card from right side to center
        let dir = self.transition_direction;

        // Old card: starts at center (angle=0, offset=0), moves to side
        let old_rot_angle = dir * progress * side_angle;
        let old_x_offset = -dir * progress * side_offset;
        let old_z_offset = -progress * 0.3; // push back slightly as it moves to side

        // New card: starts at side, moves to center
        let new_rot_angle = -dir * (1.0 - progress) * side_angle;
        let new_x_offset = dir * (1.0 - progress) * side_offset;
        let new_z_offset = -(1.0 - progress) * 0.3;

        let old_model = mat4_mul(
            &translate_matrix(old_x_offset, 0.0, d + old_z_offset),
            &rotate_y_matrix(old_rot_angle),
        );
        let old_mvp = mat4_mul(&persp, &mat4_mul(&view, &old_model));

        let new_model = mat4_mul(
            &translate_matrix(new_x_offset, 0.0, d + new_z_offset),
            &rotate_y_matrix(new_rot_angle),
        );
        let new_mvp = mat4_mul(&persp, &mat4_mul(&view, &new_model));

        // Brightness based on how centered the card is
        let old_brightness = (1.0 - progress * 0.4).max(0.3);
        let new_brightness = (0.6 + progress * 0.4).min(1.0);

        let scissor_gl_y = self.screen_h as i32 - (mon_y + mon_h as i32);

        unsafe {
            self.gl.enable(glow::SCISSOR_TEST);
            self.gl.scissor(mon_x, scissor_gl_y, mon_w as i32, workspace_h as i32);
            self.gl.viewport(mon_x, scissor_gl_y, mon_w as i32, workspace_h as i32);

            // Draw wallpaper behind
            self.gl.clear(glow::COLOR_BUFFER_BIT);
            self.gl.viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
            self.draw_wallpaper_in_region(ortho_proj, mon_x, mon_y, mon_w, mon_h);
            self.gl.viewport(mon_x, scissor_gl_y, mon_w as i32, workspace_h as i32);

            self.gl.use_program(Some(self.cube_program));
            self.gl.uniform_1_f32(self.cube_uniforms.aspect.as_ref(), aspect);
            self.gl.uniform_1_i32(self.cube_uniforms.texture.as_ref(), 0);
            self.gl.uniform_4_f32(
                self.cube_uniforms.uv_rect.as_ref(),
                uv_rect[0], uv_rect[1], uv_rect[2], uv_rect[3],
            );
            self.gl.bind_vertex_array(Some(self.quad_vao));
            self.gl.active_texture(glow::TEXTURE0);

            // Painter's order: draw farther card first
            if progress < 0.5 {
                // New card is farther (on the side), draw first
                self.gl.uniform_matrix_4_f32_slice(self.cube_uniforms.mvp.as_ref(), false, &new_mvp);
                self.gl.uniform_1_f32(self.cube_uniforms.brightness.as_ref(), new_brightness);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(new_tex));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                self.gl.uniform_matrix_4_f32_slice(self.cube_uniforms.mvp.as_ref(), false, &old_mvp);
                self.gl.uniform_1_f32(self.cube_uniforms.brightness.as_ref(), old_brightness);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(old_tex));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            } else {
                // Old card is farther, draw first
                self.gl.uniform_matrix_4_f32_slice(self.cube_uniforms.mvp.as_ref(), false, &old_mvp);
                self.gl.uniform_1_f32(self.cube_uniforms.brightness.as_ref(), old_brightness);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(old_tex));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                self.gl.uniform_matrix_4_f32_slice(self.cube_uniforms.mvp.as_ref(), false, &new_mvp);
                self.gl.uniform_1_f32(self.cube_uniforms.brightness.as_ref(), new_brightness);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(new_tex));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            }

            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
            self.gl.disable(glow::SCISSOR_TEST);
            self.gl.viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
        }
    }

    // =====================================================================
    // Helix transition: spiral path
    // =====================================================================

    /// Helix transition: old scene spirals away on a helical path, new scene
    /// spirals in from the opposite direction.
    pub(crate) fn render_helix_transition(&mut self, progress: f32, ortho_proj: &[f32; 16]) {
        let old_tex = match &self.transition_fbo {
            Some((_, tex)) => *tex,
            None => return,
        };

        let mon_x = self.transition_mon_x;
        let mon_y = self.transition_mon_y;
        let mon_w = self.transition_mon_w;
        let mon_h = self.transition_mon_h;

        // Capture new scene
        if self.transition_new_fbo.is_none() {
            self.transition_new_fbo = unsafe {
                Self::create_scene_fbo(&self.gl, mon_w, mon_h).ok()
            };
        }
        let new_tex = match &self.transition_new_fbo {
            Some((fbo, tex)) => {
                let fbo = *fbo;
                let tex = *tex;
                let blit_gl_y = self.screen_h as i32 - (mon_y + mon_h as i32);
                unsafe {
                    self.gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
                    self.gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(fbo));
                    self.gl.blit_framebuffer(
                        mon_x, blit_gl_y,
                        mon_x + mon_w as i32, blit_gl_y + mon_h as i32,
                        0, 0, mon_w as i32, mon_h as i32,
                        glow::COLOR_BUFFER_BIT, glow::NEAREST,
                    );
                    self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                }
                tex
            }
            None => return,
        };

        let exclude_top = self.transition_exclude_top.min(mon_h);
        let workspace_h = mon_h.saturating_sub(exclude_top);
        if workspace_h == 0 { return; }

        let aspect = mon_w as f32 / workspace_h as f32;
        let top_frac = if mon_h == 0 { 0.0 } else { exclude_top as f32 / mon_h as f32 };
        let uv_rect = [0.0f32, 0.0, 1.0, 1.0 - top_frac];

        let half_pi = std::f32::consts::FRAC_PI_2;
        let pi = std::f32::consts::PI;

        // Helix parameters:
        // Radius of the spiral, vertical pitch, total rotation
        let helix_radius = aspect * 0.6;
        let helix_pitch = 0.4; // vertical displacement per full rotation
        let total_rotation = pi; // one half-turn over the full transition

        let dir = self.transition_direction;
        let camera_z = 1.0 + aspect * 1.2;
        let persp = perspective_matrix(half_pi, aspect, 0.1, camera_z * 4.0);
        let view = translate_matrix(0.0, 0.0, -camera_z);

        // Old scene spirals away: angle goes from 0 to total_rotation
        let old_theta = dir * progress * total_rotation;
        let old_x = helix_radius * old_theta.sin();
        let old_z = helix_radius * (1.0 - old_theta.cos());
        let old_y = -progress * helix_pitch;

        // Scale down as it moves away
        let old_scale_factor = 1.0 - progress * 0.3;

        let old_model = mat4_mul(
            &translate_matrix(old_x, old_y, -old_z),
            &mat4_mul(
                &rotate_y_matrix(old_theta),
                &rotate_x_matrix(-progress * 0.15), // slight X tilt for spiral feel
            ),
        );
        let old_mvp = mat4_mul(&persp, &mat4_mul(&view, &old_model));

        // New scene spirals in from opposite direction
        let new_progress = 1.0 - progress;
        let new_theta = -dir * new_progress * total_rotation;
        let new_x = helix_radius * new_theta.sin();
        let new_z = helix_radius * (1.0 - new_theta.cos());
        let new_y = new_progress * helix_pitch;

        let new_scale_factor = 1.0 - new_progress * 0.3;

        let new_model = mat4_mul(
            &translate_matrix(new_x, new_y, -new_z),
            &mat4_mul(
                &rotate_y_matrix(new_theta),
                &rotate_x_matrix(new_progress * 0.15),
            ),
        );
        let new_mvp = mat4_mul(&persp, &mat4_mul(&view, &new_model));

        // Brightness: closer to front = brighter
        let old_brightness = (old_scale_factor * 0.8).max(0.2);
        let new_brightness = (new_scale_factor * 0.8).max(0.2);

        let scissor_gl_y = self.screen_h as i32 - (mon_y + mon_h as i32);

        unsafe {
            self.gl.enable(glow::SCISSOR_TEST);
            self.gl.scissor(mon_x, scissor_gl_y, mon_w as i32, workspace_h as i32);
            self.gl.viewport(mon_x, scissor_gl_y, mon_w as i32, workspace_h as i32);

            // Draw wallpaper behind
            self.gl.clear(glow::COLOR_BUFFER_BIT);
            self.gl.viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
            self.draw_wallpaper_in_region(ortho_proj, mon_x, mon_y, mon_w, mon_h);
            self.gl.viewport(mon_x, scissor_gl_y, mon_w as i32, workspace_h as i32);

            self.gl.use_program(Some(self.cube_program));
            self.gl.uniform_1_f32(self.cube_uniforms.aspect.as_ref(), aspect);
            self.gl.uniform_1_i32(self.cube_uniforms.texture.as_ref(), 0);
            self.gl.uniform_4_f32(
                self.cube_uniforms.uv_rect.as_ref(),
                uv_rect[0], uv_rect[1], uv_rect[2], uv_rect[3],
            );
            self.gl.bind_vertex_array(Some(self.quad_vao));
            self.gl.active_texture(glow::TEXTURE0);

            // Draw farther face first (painter's algorithm based on z depth)
            if old_z > new_z {
                // Old is farther, draw it first
                self.gl.uniform_matrix_4_f32_slice(self.cube_uniforms.mvp.as_ref(), false, &old_mvp);
                self.gl.uniform_1_f32(self.cube_uniforms.brightness.as_ref(), old_brightness);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(old_tex));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                self.gl.uniform_matrix_4_f32_slice(self.cube_uniforms.mvp.as_ref(), false, &new_mvp);
                self.gl.uniform_1_f32(self.cube_uniforms.brightness.as_ref(), new_brightness);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(new_tex));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            } else {
                // New is farther, draw it first
                self.gl.uniform_matrix_4_f32_slice(self.cube_uniforms.mvp.as_ref(), false, &new_mvp);
                self.gl.uniform_1_f32(self.cube_uniforms.brightness.as_ref(), new_brightness);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(new_tex));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                self.gl.uniform_matrix_4_f32_slice(self.cube_uniforms.mvp.as_ref(), false, &old_mvp);
                self.gl.uniform_1_f32(self.cube_uniforms.brightness.as_ref(), old_brightness);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(old_tex));
                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            }

            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
            self.gl.disable(glow::SCISSOR_TEST);
            self.gl.viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
        }
    }

    // =====================================================================
    // Portal transition: iris wipe with glow edge
    // =====================================================================

    /// Portal transition: an expanding circle reveals the new scene through
    /// the old scene, with a glowing ring at the edge.
    pub(crate) fn render_portal_transition(&mut self, progress: f32, ortho_proj: &[f32; 16]) {
        let old_tex = match &self.transition_fbo {
            Some((_, tex)) => *tex,
            None => return,
        };

        let mon_x = self.transition_mon_x;
        let mon_y = self.transition_mon_y;
        let mon_w = self.transition_mon_w;
        let mon_h = self.transition_mon_h;

        // Capture new scene
        if self.transition_new_fbo.is_none() {
            self.transition_new_fbo = unsafe {
                Self::create_scene_fbo(&self.gl, mon_w, mon_h).ok()
            };
        }
        let new_tex = match &self.transition_new_fbo {
            Some((fbo, tex)) => {
                let fbo = *fbo;
                let tex = *tex;
                let blit_gl_y = self.screen_h as i32 - (mon_y + mon_h as i32);
                unsafe {
                    self.gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
                    self.gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(fbo));
                    self.gl.blit_framebuffer(
                        mon_x, blit_gl_y,
                        mon_x + mon_w as i32, blit_gl_y + mon_h as i32,
                        0, 0, mon_w as i32, mon_h as i32,
                        glow::COLOR_BUFFER_BIT, glow::NEAREST,
                    );
                    self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                }
                tex
            }
            None => return,
        };

        let exclude_top = self.transition_exclude_top.min(mon_h);
        let workspace_h = mon_h.saturating_sub(exclude_top);
        if workspace_h == 0 { return; }

        let top_frac = if mon_h == 0 { 0.0 } else { exclude_top as f32 / mon_h as f32 };

        let draw_x = mon_x as f32;
        let draw_y = (mon_y as u32 + exclude_top) as f32;
        let draw_w = mon_w as f32;
        let draw_h = workspace_h as f32;
        let scissor_gl_y = self.screen_h as i32 - (mon_y + mon_h as i32);

        // UV rect for the FBO textures (workspace portion below status bar)
        let uv = [0.0f32, 0.0, 1.0, 1.0 - top_frac];

        // Glow intensity: peaks at mid-transition, fades at start/end
        let glow_intensity = (progress * std::f32::consts::PI).sin() * 1.5;

        unsafe {
            self.gl.enable(glow::SCISSOR_TEST);
            self.gl.scissor(mon_x, scissor_gl_y, mon_w as i32, workspace_h as i32);
            self.gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);

            // Step 1: Draw the new scene as the base layer (fully visible)
            self.gl.use_program(Some(self.transition_program));
            self.gl.uniform_matrix_4_f32_slice(
                self.transition_uniforms.projection.as_ref(), false, ortho_proj,
            );
            self.gl.uniform_1_i32(self.transition_uniforms.texture.as_ref(), 0);
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_vertex_array(Some(self.quad_vao));

            self.gl.uniform_4_f32(
                self.transition_uniforms.rect.as_ref(),
                draw_x, draw_y, draw_w, draw_h,
            );
            self.gl.uniform_1_f32(self.transition_uniforms.opacity.as_ref(), 1.0);
            self.gl.uniform_4_f32(
                self.transition_uniforms.uv_rect.as_ref(),
                uv[0], uv[1], uv[2], uv[3],
            );
            self.gl.bind_texture(glow::TEXTURE_2D, Some(new_tex));
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

            self.gl.use_program(None);

            // Step 2: Draw the old scene on top with the portal (iris) mask.
            // The portal shader creates a circular hole that expands with progress,
            // revealing the new scene underneath. The old scene is visible outside
            // the hole (mask > 0) and fades as the circle grows.
            self.gl.use_program(Some(self.portal_program));
            self.gl.uniform_matrix_4_f32_slice(
                self.portal_uniforms.projection.as_ref(), false, ortho_proj,
            );
            self.gl.uniform_4_f32(
                self.portal_uniforms.rect.as_ref(),
                draw_x, draw_y, draw_w, draw_h,
            );
            self.gl.uniform_1_i32(self.portal_uniforms.texture.as_ref(), 0);
            self.gl.uniform_1_f32(self.portal_uniforms.progress.as_ref(), progress);
            self.gl.uniform_1_f32(self.portal_uniforms.glow.as_ref(), glow_intensity);
            self.gl.uniform_2_f32(self.portal_uniforms.center.as_ref(), 0.5, 0.5);
            self.gl.uniform_4_f32(
                self.portal_uniforms.uv_rect.as_ref(),
                uv[0], uv[1], uv[2], uv[3],
            );
            self.gl.bind_texture(glow::TEXTURE_2D, Some(old_tex));
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
            self.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
            self.gl.disable(glow::SCISSOR_TEST);
        }
    }
}
