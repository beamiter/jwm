// Feature control methods
#[allow(unused_imports)]
use super::math::ortho;
#[allow(unused_imports)]
use super::*;
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

impl<C: CompositorConnection> Compositor<C> {
    pub(crate) fn set_system_ui(&mut self, overlay: Option<crate::backend::api::SystemUiOverlay>) {
        self.system_ui = overlay;
        self.needs_render = true;
    }
    pub(crate) fn has_partial_damage(&self) -> bool {
        self.partial_damage_enabled
    }

    pub(crate) fn set_partial_damage(&mut self, enabled: bool) -> bool {
        if self.partial_damage_enabled == enabled {
            return false;
        }
        self.partial_damage_enabled = enabled;
        self.damage_tracker.mark_all_dirty();
        self.dirty_region_tracker.mark_all_dirty();
        self.needs_render = true;
        true
    }

    pub(crate) fn set_mouse_position(&mut self, x: f32, y: f32) {
        self.mouse_x = x;
        self.mouse_y = y;
        if self.edge_glow {
            self.edge_glow_tick(x, y);
        }
        if self.magnifier_enabled || self.window_tilt {
            self.needs_render = true;
        }
        if self.expose_active {
            self.expose_set_hover(x, y);
        }
    }

    /// Core edge-glow state machine (called from mouse events and render tick).
    ///
    /// - Mouse at edge (unsuppressed) → activate.
    /// - Mouse away or suppressed     → deactivate immediately.
    pub(super) fn edge_glow_tick(&mut self, mx: f32, my: f32) {
        let sw = self.screen_w as f32;
        let sh = self.screen_h as f32;
        let min_dist = mx.min(sw - mx).min(my).min(sh - my);
        let at_edge = min_dist < self.edge_glow_width;

        if at_edge && !self.edge_glow_suppressed {
            if !self.edge_glow_active {
                self.edge_glow_active = true;
                self.needs_render = true;
            }
        } else if self.edge_glow_active {
            self.edge_glow_active = false;
            self.needs_render = true;
        }
    }

    /// Immediately deactivate the edge glow and suppress re-activation
    /// until the pointer leaves the window (returns to root/desktop).
    pub(crate) fn deactivate_edge_glow(&mut self) {
        if self.edge_glow {
            self.edge_glow_suppressed = true;
            if self.edge_glow_active {
                self.edge_glow_active = false;
                self.needs_render = true;
            }
        }
    }

    /// Clear the edge-glow suppression (pointer returned to desktop).
    pub(crate) fn unsuppress_edge_glow(&mut self) {
        self.edge_glow_suppressed = false;
    }

    pub(crate) fn set_window_urgent(&mut self, x11_win: u32, urgent: bool) {
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            wt.is_urgent = urgent;
            self.needs_render = true;
        }
    }

    pub(crate) fn set_window_pip(&mut self, x11_win: u32, pip: bool) {
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            wt.is_pip = pip;
            self.needs_render = true;
        }
    }

    /// Notify the compositor about audio stream timing for a window.
    /// This lets the compositor schedule frame presentation to match
    /// each window's independent audio clock, preventing desync.
    pub(crate) fn notify_audio_timing(&mut self, x11_win: u32, fps: f32, buffer_latency_ms: u32) {
        self.audio_sync
            .register_stream(x11_win, fps, buffer_latency_ms);
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            wt.audio_sync_target = Some(fps);
        }
        // Register with OML for per-window vblank timing too
        if let Some(oml) = &mut self.oml {
            oml.register_window(x11_win, fps);
        }
    }

    /// Register a window for Present extension support
    #[allow(dead_code)]
    pub(crate) fn register_window_present(&mut self, x11_win: u32) {
        if let Some(present_mgr) = &mut self.present_mgr {
            match present_mgr.register_window(x11_win) {
                Ok(()) => {
                    log::debug!("compositor: window 0x{:x} registered with Present", x11_win);
                }
                Err(e) => {
                    log::warn!(
                        "compositor: failed to register 0x{:x} with Present: {}",
                        x11_win,
                        e
                    );
                }
            }
        }
    }

    /// Present a window's pixmap at a specific MSC (for Present-enabled windows)
    #[allow(dead_code)]
    pub(crate) fn present_pixmap(&self, x11_win: u32, pixmap: u32, target_msc: u64, serial: u32) {
        if let Some(present_mgr) = &self.present_mgr {
            match present_mgr.present_pixmap(x11_win, pixmap, target_msc, serial) {
                Ok(()) => {
                    log::debug!(
                        "compositor: presented pixmap for 0x{:x} (serial={}, msc={})",
                        x11_win,
                        serial,
                        target_msc
                    );
                }
                Err(e) => {
                    log::debug!(
                        "compositor: present_pixmap failed for 0x{:x}: {}",
                        x11_win,
                        e
                    );
                }
            }
        }
    }

    pub(crate) fn set_magnifier(&mut self, enabled: bool) {
        self.magnifier_enabled = enabled;
        self.ensure_postprocess_fbo();
        self.needs_render = true;
    }

    pub(crate) fn set_overview_monitor(&mut self, x: i32, y: i32, w: u32, h: u32) {
        self.overview_mon_x = x;
        self.overview_mon_y = y;
        self.overview_mon_w = w;
        self.overview_mon_h = h;
    }

    pub(crate) fn set_overview_mode(
        &mut self,
        active: bool,
        windows: Vec<(u32, f32, f32, f32, f32, bool, String)>,
    ) {
        if !active && self.overview_active && !self.overview_closing {
            // Begin exit animation — don't clear state yet
            self.overview_closing = true;
            self.overview_exit_progress = 1.0;
            self.needs_render = true;
            return;
        }
        self.clear_overview_snapshots();
        self.clear_overview_title_textures();
        self.overview_active = active;
        self.overview_closing = false;
        let n = windows.len();
        let face_w = self.screen_w as f32 * 0.8;
        let face_h = self.screen_h as f32 * 0.8;
        self.overview_windows = windows
            .into_iter()
            .enumerate()
            .map(|(i, (win, _x, _y, _w, _h, sel, title))| OverviewEntry {
                x11_win: win,
                target_w: face_w,
                target_h: face_h,
                is_selected: sel,
                snapshot_texture: None,
                title,
                title_texture: None,
                face_index: i.min(5),
            })
            .collect();
        self.overview_total_clients = n;
        self.overview_slide_offset = 0;
        self.overview_prism_target_angle = 0.0;
        self.overview_prism_current_angle = 0.0;
        self.overview_prism_last_tick = None;
        if active {
            self.refresh_overview_snapshots();
            self.create_overview_title_textures();
            self.overview_entry_progress = 0.0;
            self.overview_exit_progress = 1.0;
            self.overview_opacity = 0.0;
        } else {
            self.overview_entry_progress = 1.0;
            self.overview_exit_progress = 1.0;
            self.overview_opacity = 0.0;
        }
        self.needs_render = true;
    }

    pub(crate) fn set_overview_selection(&mut self, x11_win: u32) {
        let mut selected_face = 0usize;
        for entry in &mut self.overview_windows {
            let sel = entry.x11_win == x11_win;
            entry.is_selected = sel;
            if sel {
                selected_face = entry.face_index;
            }
        }
        // Rotate prism so selected face faces the camera.
        let new_target = -(selected_face as f32) * std::f32::consts::FRAC_PI_3;
        // Normalize angular difference to shortest path (within -PI..PI).
        let mut diff = new_target - self.overview_prism_target_angle;
        while diff > std::f32::consts::PI {
            diff -= 2.0 * std::f32::consts::PI;
        }
        while diff < -std::f32::consts::PI {
            diff += 2.0 * std::f32::consts::PI;
        }
        self.overview_prism_target_angle += diff;
        self.needs_render = true;
    }

    pub(crate) fn notify_window_move_start(&mut self, x11_win: u32) {
        if self.motion_trail_enabled {
            self.clear_motion_trail(x11_win);
            if let Some(wt) = self.windows.get_mut(&x11_win) {
                wt.motion_trail_cursor = Some((wt.x as f32, wt.y as f32));
            }
        }
        if !self.wobbly_windows {
            return;
        }
        let grid_n =
            crate::backend::compositor_common::effects::wobbly_node_count(self.wobbly_grid_size);
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            // Determine anchor node: closest grid node to mouse position
            let rel_x = ((self.mouse_x - wt.x as f32).max(0.0)).min(wt.w as f32);
            let rel_y = ((self.mouse_y - wt.y as f32).max(0.0)).min(wt.h as f32);
            let (anchor_row, anchor_col) =
                WobblyState::anchor_for_point(grid_n, rel_x, rel_y, wt.w as f32, wt.h as f32);

            wt.wobbly = Some(WobblyState::new(grid_n, anchor_row, anchor_col));
        } else {
            log::warn!(
                "[wobbly] move_start: window 0x{:x} not tracked by compositor",
                x11_win
            );
        }
    }

    pub(crate) fn notify_window_move_delta(&mut self, x11_win: u32, dx: f32, dy: f32) {
        // Phase 3.1: Record position for motion trail
        if self.motion_trail_enabled {
            let previous = self.windows.get_mut(&x11_win).map(|wt| {
                let (previous_x, previous_y) = wt
                    .motion_trail_cursor
                    .unwrap_or((wt.x as f32 - dx, wt.y as f32 - dy));
                wt.motion_trail_cursor = Some((previous_x + dx, previous_y + dy));
                (previous_x.round() as i32, previous_y.round() as i32)
            });
            if let Some((previous_x, previous_y)) = previous {
                self.update_motion_trail(x11_win, previous_x, previous_y);
            }
        }

        if self.wobbly_windows {
            if let Some(wt) = self.windows.get_mut(&x11_win) {
                if let Some(ref mut w) = wt.wobbly {
                    // The window has already moved to the new position.
                    // Anchor node stays at [0,0] (moves with the window).
                    // All OTHER nodes get a reverse impulse to simulate inertia.
                    w.apply_window_move_delta(dx, dy);
                }
            }
        }

        // During interactive move/resize, request full-frame redraw when backdrop
        // blur is active so translucent windows see real-time updated background.
        let blur_active =
            self.blur_enabled && self.scene_fbo.is_some() && !self.blur_fbos.is_empty() && {
                let cfg = crate::config::CONFIG.load();
                let status_bar_name = cfg.status_bar_name();
                self.windows
                    .values()
                    .any(|wt| self.needs_backdrop_blur(wt, status_bar_name))
            };
        if blur_active {
            self.damage_tracker.mark_all_dirty();
            self.dirty_region_tracker.mark_all_dirty(); // P5C: Sync rect tracker
        }
        self.needs_render = true;
    }

    pub(crate) fn notify_window_move_end(&mut self, x11_win: u32) {
        // Release anchor — let all nodes spring back via tick_wobbly
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            wt.motion_trail_cursor = None;
            if let Some(ref mut w) = wt.wobbly {
                w.end_drag();
            }
        }
        // Keep the trail alive briefly after release and let wall-clock expiry
        // fade it out instead of making it disappear on the button-up frame.
        self.needs_render = true;
    }

    #[allow(dead_code)]
    pub(crate) fn tracked_window_count(&self) -> usize {
        self.windows.len()
    }

    /// Set dock/taskbar position for genie minimize target.
    pub(crate) fn set_dock_position(&mut self, x: f32, y: f32) {
        self.dock_position = (x, y);
    }

    #[allow(dead_code)]
    pub(crate) fn has_window(&self, x11_win: u32) -> bool {
        self.windows.contains_key(&x11_win)
    }

    // =====================================================================
    // Phase 6: Accessibility & Utility
    // =====================================================================

    pub(crate) fn set_colorblind_mode(&mut self, mode: &str) {
        let m = match mode {
            "deuteranopia" => 1,
            "protanopia" => 2,
            "tritanopia" => 3,
            _ => 0,
        };
        if self.colorblind_mode != m {
            self.colorblind_mode = m;
            self.ensure_postprocess_fbo();
            self.needs_render = true;
        }
    }

    pub(crate) fn zoom_to_fit(&mut self, window: Option<u32>) {
        if let Some(win) = window {
            if self.zoom_to_fit_window == Some(win) {
                self.zoom_to_fit_window = None;
                self.zoom_to_fit_target = 1.0;
            } else {
                self.zoom_to_fit_window = Some(win);
                if let Some(wt) = self.windows.get(&win) {
                    if wt.w > 0 && wt.h > 0 {
                        let sx = self.screen_w as f32 / wt.w as f32;
                        let sy = self.screen_h as f32 / wt.h as f32;
                        self.zoom_to_fit_target = sx.min(sy);
                    }
                }
            }
            self.needs_render = true;
        } else {
            self.zoom_to_fit_window = None;
            self.zoom_to_fit_target = 1.0;
            self.needs_render = true;
        }
    }

    // =====================================================================
    // Phase 7: Diagnostics
    // =====================================================================

    pub(crate) fn reload_shader_from_file(
        &mut self,
        name: &str,
        path: &std::path::Path,
    ) -> Result<(), String> {
        // Box blur is an optimization mode implemented with the regular blur
        // programs, not a standalone GL program. Compiling a replacement for
        // it would leave the new program unowned and leak it.
        if name == "box_blur" {
            return Err(
                "box_blur has no standalone shader program; reload blur_down or blur_up instead"
                    .to_string(),
            );
        }

        let file_content =
            std::fs::read_to_string(path).map_err(|e| format!("read shader file: {e}"))?;

        let (vs_src, fs_src) = match name {
            "window" => (shaders::VERTEX_SHADER, file_content.as_str()),
            "shadow" => (shaders::VERTEX_SHADER, file_content.as_str()),
            "border" => (shaders::VERTEX_SHADER, file_content.as_str()),
            "blur_down" => (shaders::BLUR_DOWN_VERTEX, file_content.as_str()),
            "blur_up" => (shaders::BLUR_DOWN_VERTEX, file_content.as_str()),
            "postprocess" => (shaders::BLUR_DOWN_VERTEX, file_content.as_str()),
            "hud" => (shaders::VERTEX_SHADER, file_content.as_str()),
            "hud_text" => (shaders::VERTEX_SHADER, file_content.as_str()),
            "transition" => (shaders::BLUR_DOWN_VERTEX, file_content.as_str()),
            "cube" => (shaders::CUBE_VERTEX_SHADER, file_content.as_str()),
            "portal" => (shaders::BLUR_DOWN_VERTEX, file_content.as_str()),
            "edge_glow" => (shaders::VERTEX_SHADER, file_content.as_str()),
            "tilt" => (shaders::TILT_VERTEX_SHADER, file_content.as_str()),
            "wobbly" => (shaders::WOBBLY_VERTEX_SHADER, file_content.as_str()),
            "particle" => (shaders::PARTICLE_VERTEX_SHADER, file_content.as_str()),
            "genie" => (shaders::GENIE_VERTEX_SHADER, file_content.as_str()),
            "overview_bg" => (shaders::VERTEX_SHADER, file_content.as_str()),
            _ if name.ends_with("_vs") => {
                log::warn!(
                    "compositor: shader reload requires both vertex and fragment shaders to be specified"
                );
                return Err(format!(
                    "shader {} needs corresponding fragment shader",
                    name
                ));
            }
            _ => return Err(format!("unknown shader: {name}")),
        };

        match unsafe { Self::create_program(&self.gl, vs_src, fs_src) } {
            Ok(new_program) => {
                unsafe {
                    match name {
                        "window" => {
                            let uniforms = WindowUniforms {
                                projection: self
                                    .gl
                                    .get_uniform_location(new_program, "u_projection"),
                                rect: self.gl.get_uniform_location(new_program, "u_rect"),
                                texture: self.gl.get_uniform_location(new_program, "u_texture"),
                                opacity: self.gl.get_uniform_location(new_program, "u_opacity"),
                                radius: self.gl.get_uniform_location(new_program, "u_radius"),
                                size: self.gl.get_uniform_location(new_program, "u_size"),
                                dim: self.gl.get_uniform_location(new_program, "u_dim"),
                                uv_rect: self.gl.get_uniform_location(new_program, "u_uv_rect"),
                                ripple_progress: self
                                    .gl
                                    .get_uniform_location(new_program, "u_ripple_progress"),
                                ripple_amplitude: self
                                    .gl
                                    .get_uniform_location(new_program, "u_ripple_amplitude"),
                            };
                            let old_program = std::mem::replace(&mut self.program, new_program);
                            self.win_uniforms = uniforms;
                            self.gl.delete_program(old_program);
                        }
                        "shadow" => {
                            let uniforms = ShadowUniforms {
                                projection: self
                                    .gl
                                    .get_uniform_location(new_program, "u_projection"),
                                rect: self.gl.get_uniform_location(new_program, "u_rect"),
                                shadow_color: self
                                    .gl
                                    .get_uniform_location(new_program, "u_shadow_color"),
                                size: self.gl.get_uniform_location(new_program, "u_size"),
                                radius: self.gl.get_uniform_location(new_program, "u_radius"),
                                spread: self.gl.get_uniform_location(new_program, "u_spread"),
                            };
                            let old_program =
                                std::mem::replace(&mut self.shadow_program, new_program);
                            self.shadow_uniforms = uniforms;
                            self.gl.delete_program(old_program);
                        }
                        "border" => {
                            let uniforms = BorderUniforms {
                                projection: self
                                    .gl
                                    .get_uniform_location(new_program, "u_projection"),
                                rect: self.gl.get_uniform_location(new_program, "u_rect"),
                                border_color: self
                                    .gl
                                    .get_uniform_location(new_program, "u_border_color"),
                                size: self.gl.get_uniform_location(new_program, "u_size"),
                                radius: self.gl.get_uniform_location(new_program, "u_radius"),
                                border_width: self
                                    .gl
                                    .get_uniform_location(new_program, "u_border_width"),
                            };
                            let old_program =
                                std::mem::replace(&mut self.border_program, new_program);
                            self.border_uniforms = uniforms;
                            self.gl.delete_program(old_program);
                        }
                        "blur_down" => {
                            let uniforms = BlurUniforms {
                                projection: self
                                    .gl
                                    .get_uniform_location(new_program, "u_projection"),
                                rect: self.gl.get_uniform_location(new_program, "u_rect"),
                                texture: self.gl.get_uniform_location(new_program, "u_texture"),
                                halfpixel: self.gl.get_uniform_location(new_program, "u_halfpixel"),
                            };
                            let old_program =
                                std::mem::replace(&mut self.blur_down_program, new_program);
                            self.blur_down_uniforms = uniforms;
                            self.gl.delete_program(old_program);
                        }
                        "blur_up" => {
                            let uniforms = BlurUniforms {
                                projection: self
                                    .gl
                                    .get_uniform_location(new_program, "u_projection"),
                                rect: self.gl.get_uniform_location(new_program, "u_rect"),
                                texture: self.gl.get_uniform_location(new_program, "u_texture"),
                                halfpixel: self.gl.get_uniform_location(new_program, "u_halfpixel"),
                            };
                            let old_program =
                                std::mem::replace(&mut self.blur_up_program, new_program);
                            self.blur_up_uniforms = uniforms;
                            self.gl.delete_program(old_program);
                        }
                        "postprocess" => {
                            let uniforms = PostprocessUniforms {
                                projection: self
                                    .gl
                                    .get_uniform_location(new_program, "u_projection"),
                                rect: self.gl.get_uniform_location(new_program, "u_rect"),
                                texture: self.gl.get_uniform_location(new_program, "u_texture"),
                                color_temp: self
                                    .gl
                                    .get_uniform_location(new_program, "u_color_temp"),
                                saturation: self
                                    .gl
                                    .get_uniform_location(new_program, "u_saturation"),
                                brightness: self
                                    .gl
                                    .get_uniform_location(new_program, "u_brightness"),
                                contrast: self.gl.get_uniform_location(new_program, "u_contrast"),
                                invert: self.gl.get_uniform_location(new_program, "u_invert"),
                                grayscale: self.gl.get_uniform_location(new_program, "u_grayscale"),
                                hdr_enabled: self
                                    .gl
                                    .get_uniform_location(new_program, "u_hdr_enabled"),
                                hdr_peak_nits: self
                                    .gl
                                    .get_uniform_location(new_program, "u_hdr_peak_nits"),
                                tone_mapping_method: self
                                    .gl
                                    .get_uniform_location(new_program, "u_tone_mapping_method"),
                                eotf_mode: self.gl.get_uniform_location(new_program, "u_eotf_mode"),
                                output_colorspace: self
                                    .gl
                                    .get_uniform_location(new_program, "u_output_colorspace"),
                            };
                            let magnifier_uniforms = MagnifierUniforms {
                                magnifier_enabled: self
                                    .gl
                                    .get_uniform_location(new_program, "u_magnifier_enabled"),
                                magnifier_center: self
                                    .gl
                                    .get_uniform_location(new_program, "u_magnifier_center"),
                                magnifier_radius: self
                                    .gl
                                    .get_uniform_location(new_program, "u_magnifier_radius"),
                                magnifier_zoom: self
                                    .gl
                                    .get_uniform_location(new_program, "u_magnifier_zoom"),
                                colorblind_mode: self
                                    .gl
                                    .get_uniform_location(new_program, "u_colorblind_mode"),
                            };
                            let old_program =
                                std::mem::replace(&mut self.postprocess_program, new_program);
                            self.postprocess_uniforms = uniforms;
                            self.magnifier_uniforms = magnifier_uniforms;
                            self.gl.delete_program(old_program);
                        }
                        "hud" => {
                            let uniforms = HudUniforms {
                                projection: self
                                    .gl
                                    .get_uniform_location(new_program, "u_projection"),
                                rect: self.gl.get_uniform_location(new_program, "u_rect"),
                                bg_color: self.gl.get_uniform_location(new_program, "u_bg_color"),
                                fg_color: self.gl.get_uniform_location(new_program, "u_fg_color"),
                                size: self.gl.get_uniform_location(new_program, "u_size"),
                            };
                            let old_program = std::mem::replace(&mut self.hud_program, new_program);
                            self.hud_uniforms = uniforms;
                            self.gl.delete_program(old_program);
                        }
                        "hud_text" => {
                            let uniforms = HudTextUniforms {
                                projection: self
                                    .gl
                                    .get_uniform_location(new_program, "u_projection"),
                                rect: self.gl.get_uniform_location(new_program, "u_rect"),
                                texture: self.gl.get_uniform_location(new_program, "u_texture"),
                            };
                            let old_program =
                                std::mem::replace(&mut self.hud_text_program, new_program);
                            self.hud_text_uniforms = uniforms;
                            self.gl.delete_program(old_program);
                        }
                        "transition" => {
                            let uniforms = TransitionUniforms {
                                projection: self
                                    .gl
                                    .get_uniform_location(new_program, "u_projection"),
                                rect: self.gl.get_uniform_location(new_program, "u_rect"),
                                texture: self.gl.get_uniform_location(new_program, "u_texture"),
                                opacity: self.gl.get_uniform_location(new_program, "u_opacity"),
                                uv_rect: self.gl.get_uniform_location(new_program, "u_uv_rect"),
                            };
                            let old_program =
                                std::mem::replace(&mut self.transition_program, new_program);
                            self.transition_uniforms = uniforms;
                            self.gl.delete_program(old_program);
                        }
                        "cube" => {
                            let uniforms = CubeUniforms {
                                mvp: self.gl.get_uniform_location(new_program, "u_mvp"),
                                aspect: self.gl.get_uniform_location(new_program, "u_aspect"),
                                texture: self.gl.get_uniform_location(new_program, "u_texture"),
                                brightness: self
                                    .gl
                                    .get_uniform_location(new_program, "u_brightness"),
                                uv_rect: self.gl.get_uniform_location(new_program, "u_uv_rect"),
                            };
                            let old_program =
                                std::mem::replace(&mut self.cube_program, new_program);
                            self.cube_uniforms = uniforms;
                            self.gl.delete_program(old_program);
                        }
                        "portal" => {
                            let uniforms = PortalUniforms {
                                projection: self
                                    .gl
                                    .get_uniform_location(new_program, "u_projection"),
                                rect: self.gl.get_uniform_location(new_program, "u_rect"),
                                texture: self.gl.get_uniform_location(new_program, "u_texture"),
                                progress: self.gl.get_uniform_location(new_program, "u_progress"),
                                glow: self.gl.get_uniform_location(new_program, "u_glow"),
                                center: self.gl.get_uniform_location(new_program, "u_center"),
                                uv_rect: self.gl.get_uniform_location(new_program, "u_uv_rect"),
                            };
                            let old_program =
                                std::mem::replace(&mut self.portal_program, new_program);
                            self.portal_uniforms = uniforms;
                            self.gl.delete_program(old_program);
                        }
                        "edge_glow" => {
                            let uniforms = EdgeGlowUniforms {
                                projection: self
                                    .gl
                                    .get_uniform_location(new_program, "u_projection"),
                                rect: self.gl.get_uniform_location(new_program, "u_rect"),
                                glow_color: self
                                    .gl
                                    .get_uniform_location(new_program, "u_glow_color"),
                                glow_width: self
                                    .gl
                                    .get_uniform_location(new_program, "u_glow_width"),
                                mouse: self.gl.get_uniform_location(new_program, "u_mouse"),
                                screen_size: self
                                    .gl
                                    .get_uniform_location(new_program, "u_screen_size"),
                                time: self.gl.get_uniform_location(new_program, "u_time"),
                            };
                            let old_program =
                                std::mem::replace(&mut self.edge_glow_program, new_program);
                            self.edge_glow_uniforms = uniforms;
                            self.gl.delete_program(old_program);
                        }
                        "tilt" => {
                            let uniforms = TiltUniforms {
                                projection: self
                                    .gl
                                    .get_uniform_location(new_program, "u_projection"),
                                rect: self.gl.get_uniform_location(new_program, "u_rect"),
                                texture: self.gl.get_uniform_location(new_program, "u_texture"),
                                opacity: self.gl.get_uniform_location(new_program, "u_opacity"),
                                radius: self.gl.get_uniform_location(new_program, "u_radius"),
                                size: self.gl.get_uniform_location(new_program, "u_size"),
                                dim: self.gl.get_uniform_location(new_program, "u_dim"),
                                uv_rect: self.gl.get_uniform_location(new_program, "u_uv_rect"),
                                tilt: self.gl.get_uniform_location(new_program, "u_tilt"),
                                perspective: self
                                    .gl
                                    .get_uniform_location(new_program, "u_perspective"),
                                grid_size: self.gl.get_uniform_location(new_program, "u_grid_size"),
                                light_dir: self.gl.get_uniform_location(new_program, "u_light_dir"),
                            };
                            let old_program =
                                std::mem::replace(&mut self.tilt_program, new_program);
                            self.tilt_uniforms = uniforms;
                            self.gl.delete_program(old_program);
                        }
                        "wobbly" => {
                            let uniforms = WobblyUniforms {
                                projection: self
                                    .gl
                                    .get_uniform_location(new_program, "u_projection"),
                                rect: self.gl.get_uniform_location(new_program, "u_rect"),
                                texture: self.gl.get_uniform_location(new_program, "u_texture"),
                                opacity: self.gl.get_uniform_location(new_program, "u_opacity"),
                                radius: self.gl.get_uniform_location(new_program, "u_radius"),
                                size: self.gl.get_uniform_location(new_program, "u_size"),
                                dim: self.gl.get_uniform_location(new_program, "u_dim"),
                                uv_rect: self.gl.get_uniform_location(new_program, "u_uv_rect"),
                                grid_offsets: self
                                    .gl
                                    .get_uniform_location(new_program, "u_grid_offsets"),
                                grid_n: self.gl.get_uniform_location(new_program, "u_grid_n"),
                            };
                            let old_program =
                                std::mem::replace(&mut self.wobbly_program, new_program);
                            self.wobbly_uniforms = uniforms;
                            self.gl.delete_program(old_program);
                        }
                        "particle" => {
                            let uniforms = ParticleUniforms {
                                projection: self
                                    .gl
                                    .get_uniform_location(new_program, "u_projection"),
                                point_size: self
                                    .gl
                                    .get_uniform_location(new_program, "u_point_size"),
                            };
                            let old_program =
                                std::mem::replace(&mut self.particle_program, new_program);
                            self.particle_uniforms = uniforms;
                            self.gl.delete_program(old_program);
                        }
                        "genie" => {
                            let uniforms = GenieUniforms {
                                projection: self
                                    .gl
                                    .get_uniform_location(new_program, "u_projection"),
                                rect: self.gl.get_uniform_location(new_program, "u_rect"),
                                texture: self.gl.get_uniform_location(new_program, "u_texture"),
                                opacity: self.gl.get_uniform_location(new_program, "u_opacity"),
                                radius: self.gl.get_uniform_location(new_program, "u_radius"),
                                size: self.gl.get_uniform_location(new_program, "u_size"),
                                dim: self.gl.get_uniform_location(new_program, "u_dim"),
                                uv_rect: self.gl.get_uniform_location(new_program, "u_uv_rect"),
                                progress: self.gl.get_uniform_location(new_program, "u_progress"),
                                dock_pos: self.gl.get_uniform_location(new_program, "u_dock_pos"),
                                grid_size: self.gl.get_uniform_location(new_program, "u_grid_size"),
                            };
                            let old_program =
                                std::mem::replace(&mut self.genie_program, new_program);
                            self.genie_uniforms = uniforms;
                            self.gl.delete_program(old_program);
                        }
                        "overview_bg" => {
                            let uniforms = OverviewBgUniforms {
                                projection: self
                                    .gl
                                    .get_uniform_location(new_program, "u_projection"),
                                rect: self.gl.get_uniform_location(new_program, "u_rect"),
                                opacity: self.gl.get_uniform_location(new_program, "u_opacity"),
                            };
                            let old_program =
                                std::mem::replace(&mut self.overview_bg_program, new_program);
                            self.overview_bg_uniforms = uniforms;
                            self.gl.delete_program(old_program);
                        }
                        _ => {
                            // Keep this defensive arm leak-free if a new name
                            // is added above without a swap implementation.
                            self.gl.delete_program(new_program);
                            return Err(format!(
                                "shader reload is not implemented for program: {name}"
                            ));
                        }
                    }
                }
                self.needs_render = true;
                log::info!("compositor: shader reload succeeded for {name}");
                Ok(())
            }
            Err(e) => {
                log::warn!("compositor: shader reload failed for {name}: {e}");
                Err(e)
            }
        }
    }

    pub(crate) fn enable_shader_hot_reload(&mut self, shader_dir: &str) {
        if shader_dir.is_empty() {
            log::warn!("compositor: shader_dir is empty, cannot enable hot-reload");
            return;
        }
        let dir = std::path::PathBuf::from(shader_dir);
        if !dir.exists() {
            if let Err(e) = std::fs::create_dir_all(&dir) {
                log::warn!("compositor: failed to create shader_dir '{shader_dir}': {e}");
                return;
            }
        }
        self.shader_hot_reload_enabled = true;
        self.shader_dir = shader_dir.to_string();
        self.shader_file_mtimes.clear();
        log::info!("compositor: shader hot-reload enabled, watching '{shader_dir}'");
    }

    pub(crate) fn poll_shader_hot_reload(&mut self) {
        if !self.shader_hot_reload_enabled || self.shader_dir.is_empty() {
            return;
        }

        const SHADER_NAMES: &[&str] = &[
            "window",
            "shadow",
            "border",
            "blur_down",
            "blur_up",
            "postprocess",
            "hud",
            "hud_text",
            "transition",
            "cube",
            "portal",
            "edge_glow",
            "tilt",
            "wobbly",
            "particle",
            "genie",
            "overview_bg",
        ];

        let dir = std::path::PathBuf::from(&self.shader_dir);
        let mut to_reload: Vec<(String, std::path::PathBuf)> = Vec::new();

        for &name in SHADER_NAMES {
            let path = dir.join(format!("{name}.frag"));
            if !path.exists() {
                continue;
            }
            let mtime = match std::fs::metadata(&path).and_then(|m| m.modified()) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let changed = match self.shader_file_mtimes.get(name) {
                Some(&prev) => mtime != prev,
                None => true,
            };
            if changed {
                self.shader_file_mtimes.insert(name.to_string(), mtime);
                to_reload.push((name.to_string(), path));
            }
        }

        for (name, path) in to_reload {
            match self.reload_shader_from_file(&name, &path) {
                Ok(()) => log::info!("compositor: hot-reloaded shader '{name}'"),
                Err(e) => log::warn!("compositor: hot-reload failed for '{name}': {e}"),
            }
        }
    }

    pub(crate) fn start_recording(&mut self, output_path: &str) {
        self.start_recording_region(output_path, (0, 0, self.screen_w, self.screen_h));
    }

    pub(crate) fn start_recording_region(
        &mut self,
        output_path: &str,
        region: (i32, i32, u32, u32),
    ) {
        if self.recording_active {
            return;
        }
        self.set_recording_region(region);
        let (_, _, w, h) = self.recording_region;
        self.recording_output_size = (w, h);
        let fps = self.recording_fps.clamp(1, 240);

        let recording_fbo = match unsafe { Self::create_scene_fbo(&self.gl, w, h) } {
            Ok(fbo) => fbo,
            Err(error) => {
                log::warn!("compositor: failed to create recording framebuffer: {error}");
                return;
            }
        };

        let stderr_file = std::fs::File::create("/tmp/jwm-ffmpeg.log")
            .unwrap_or_else(|_| std::fs::File::create("/dev/null").unwrap());

        use crate::backend::compositor_common::media::{
            RecordingEncoder, append_recording_audio_input, append_recording_audio_output,
            recording_audio_available, select_recording_encoder,
        };
        let encoder = select_recording_encoder(&self.recording_encoder);
        let (audio_enabled, audio_device, audio_bitrate) = {
            let cfg = crate::config::CONFIG.load();
            let behavior = cfg.behavior();
            (
                behavior.recording_audio_enabled,
                behavior.recording_audio_device.clone(),
                behavior.recording_audio_bitrate.clone(),
            )
        };
        let with_audio = audio_enabled && recording_audio_available(&audio_device);
        if audio_enabled && !with_audio {
            log::warn!(
                "compositor: recording microphone '{}' unavailable; continuing video-only",
                audio_device
            );
        }
        // Ubuntu/Debian's ffmpeg builds expose the software H.264 encoder as
        // libx264.  libopenh264 is not generally compiled in and made the
        // recorder accept the IPC request while ffmpeg immediately exited.
        let codec_name = encoder.codec_name("libx264");
        let bitrate = &self.recording_bitrate;
        let quality_str = self.recording_quality.to_string();
        log::info!(
            "compositor: recording encoder={codec_name}, size={w}x{h}, fps={fps}, bitrate={bitrate}, qp={quality_str}, output={output_path}"
        );

        let size_str = format!("{w}x{h}");
        let fps_str = fps.to_string();
        let mut args: Vec<String> = Vec::new();

        if matches!(encoder, RecordingEncoder::Vaapi) {
            args.extend(["-vaapi_device", "/dev/dri/renderD128"].map(str::to_string));
        }
        // Input: use wall clock timestamps so video duration matches real time.
        // The nominal `-r` is moved to the output side; ffmpeg duplicates/drops
        // frames automatically to produce a constant-frame-rate file.
        args.extend(
            [
                "-use_wallclock_as_timestamps",
                "1",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgba",
                "-s",
                size_str.as_str(),
                "-i",
                "pipe:0",
            ]
            .map(str::to_string),
        );
        if with_audio {
            append_recording_audio_input(&mut args, &audio_device);
        }
        match encoder {
            RecordingEncoder::Nvenc => args.extend(["-vf", "vflip"].map(str::to_string)),
            RecordingEncoder::Vaapi => {
                args.extend(["-vf", "vflip,format=nv12,hwupload"].map(str::to_string))
            }
            RecordingEncoder::Software => args.extend(["-vf", "vflip"].map(str::to_string)),
        }
        args.push("-c:v".into());
        args.push(codec_name.into());
        match encoder {
            RecordingEncoder::Vaapi => {
                args.extend(["-rc_mode", "CQP", "-qp", quality_str.as_str()].map(str::to_string))
            }
            _ => args.extend(["-b:v", bitrate.as_str()].map(str::to_string)),
        }
        if with_audio {
            append_recording_audio_output(&mut args, &audio_bitrate);
        }
        args.extend(
            [
                "-r",
                fps_str.as_str(),
                "-movflags",
                "+faststart",
                "-y",
                output_path,
            ]
            .map(str::to_string),
        );

        let child = match std::process::Command::new("ffmpeg")
            .args(&args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(stderr_file)
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                log::warn!("compositor: failed to start ffmpeg: {e}");
                unsafe {
                    self.gl.delete_framebuffer(recording_fbo.0);
                    self.gl.delete_texture(recording_fbo.1);
                }
                return;
            }
        };

        unsafe {
            for pbo in &mut self.recording_pbo {
                if let Ok(buf) = self.gl.create_buffer() {
                    self.gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(buf));
                    self.gl.buffer_data_size(
                        glow::PIXEL_PACK_BUFFER,
                        (w * h * 4) as i32,
                        glow::STREAM_READ,
                    );
                    self.gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
                    *pbo = Some(buf);
                }
            }
        }

        self.recording_process = Some(child);
        self.recording_fbo = Some(recording_fbo);
        self.recording_active = true;
        self.recording_last_frame = None;
        self.recording_current_pbo = 0;
        self.recording_captured_frames = 0;
        self.recording_cursor = [None, None];
        self.recording_frame_region = [self.recording_region; 2];
        log::info!(
            "compositor: recording started to {output_path} (microphone={})",
            if with_audio {
                audio_device.as_str()
            } else {
                "off"
            }
        );
    }

    pub(crate) fn set_recording_region(&mut self, region: (i32, i32, u32, u32)) {
        let (x, y, width, height) = region;
        let x = x.clamp(0, self.screen_w.saturating_sub(1) as i32);
        let y = y.clamp(0, self.screen_h.saturating_sub(1) as i32);
        let width = width.max(1).min(self.screen_w.saturating_sub(x as u32));
        let height = height.max(1).min(self.screen_h.saturating_sub(y as u32));
        self.recording_region = (x, y, width, height);
        self.needs_render = true;
    }

    pub(crate) fn set_recording_region_overlay(&mut self, region: Option<(i32, i32, u32, u32)>) {
        self.recording_region_overlay = region;
        self.force_full_redraw();
    }

    pub(crate) fn stop_recording(&mut self) {
        // `capture_recording_frame` clears recording_active when the ffmpeg
        // pipe breaks.  The child and PBOs still need cleanup in that case;
        // returning solely on the flag leaks a zombie ffmpeg process.
        if !self.recording_active && self.recording_process.is_none() {
            return;
        }
        let was_active = self.recording_active;
        self.recording_active = false;

        // Drain the last asynchronous ReadPixels before closing ffmpeg. This
        // keeps the final frame instead of silently truncating every recording.
        if was_active && self.recording_captured_frames > 0 {
            let last_pbo = self.recording_current_pbo ^ 1;
            self.write_recording_pbo(last_pbo);
        }

        unsafe {
            for pbo in &mut self.recording_pbo {
                if let Some(buf) = pbo.take() {
                    self.gl.delete_buffer(buf);
                }
            }
        }
        self.recording_cursor = [None, None];
        if let Some((fbo, texture)) = self.recording_fbo.take() {
            unsafe {
                self.gl.delete_framebuffer(fbo);
                self.gl.delete_texture(texture);
            }
        }

        if let Some(mut child) = self.recording_process.take() {
            drop(child.stdin.take());
            if let Ok(status) = child.wait() {
                if !status.success() {
                    log::warn!("compositor: ffmpeg exited with {status}; see /tmp/jwm-ffmpeg.log");
                }
            }
        }
        log::info!("compositor: recording stopped");
    }

    pub(super) fn capture_recording_frame(&mut self) {
        if !self.recording_active {
            return;
        }

        let now = std::time::Instant::now();
        let min_interval =
            std::time::Duration::from_secs_f32(1.0 / self.recording_fps.clamp(1, 240) as f32);
        if let Some(last) = self.recording_last_frame {
            if now.duration_since(last) < min_interval {
                return;
            }
        }
        self.recording_last_frame = Some(now);

        let (w, h) = self.recording_output_size;
        let Some((recording_fbo, _)) = self.recording_fbo else {
            return;
        };
        // Overlap the current GPU readback with sending the preceding PBO to
        // ffmpeg. This avoids a GPU/CPU round-trip on every frame.
        let written_pbo = self.recording_current_pbo;
        if let Some(pbo) = self.recording_pbo[written_pbo] {
            unsafe {
                let (x, y, region_width, region_height) = self.recording_region;
                let source_bottom = self.screen_h as i32 - (y + region_height as i32);
                self.gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
                self.gl
                    .bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(recording_fbo));
                self.gl.blit_framebuffer(
                    x,
                    source_bottom,
                    x + region_width as i32,
                    source_bottom + region_height as i32,
                    0,
                    0,
                    w as i32,
                    h as i32,
                    glow::COLOR_BUFFER_BIT,
                    glow::LINEAR,
                );
                self.gl
                    .bind_framebuffer(glow::FRAMEBUFFER, Some(recording_fbo));
                self.gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(pbo));
                self.gl.read_pixels(
                    0,
                    0,
                    w as i32,
                    h as i32,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelPackData::BufferOffset(0),
                );
                // Keep cursor metadata paired with this PBO. The asynchronous
                // PBO is mapped one capture later, by which time a fast-moving
                // cursor may already be at a different position.
                self.recording_cursor[written_pbo] = self.graphics.capture_recording_cursor();
                self.recording_frame_region[written_pbo] = self.recording_region;

                self.gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
                self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            }
            self.recording_current_pbo ^= 1;
            if self.recording_captured_frames > 0 {
                self.write_recording_pbo(written_pbo ^ 1);
            }
            self.recording_captured_frames += 1;
        }
    }

    fn write_recording_pbo(&mut self, pbo_index: usize) {
        let Some(pbo) = self.recording_pbo[pbo_index] else {
            return;
        };
        let cursor = self.recording_cursor[pbo_index].take();
        let (width, height) = self.recording_output_size;
        let buf_size = (width * height * 4) as usize;
        unsafe {
            self.gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(pbo));
            let ptr = self.gl.map_buffer_range(
                glow::PIXEL_PACK_BUFFER,
                0,
                buf_size as i32,
                glow::MAP_READ_BIT,
            );
            if ptr.is_null() {
                log::warn!("compositor: recording PBO map returned null");
            } else {
                let pixels = std::slice::from_raw_parts_mut(ptr as *mut u8, buf_size);
                if let Some(cursor) = cursor.as_ref() {
                    cursor.composite_into(
                        pixels,
                        width,
                        height,
                        self.recording_frame_region[pbo_index],
                    );
                }
                if let Some(child) = self.recording_process.as_mut() {
                    if let Some(stdin) = child.stdin.as_mut() {
                        use std::io::Write;
                        if let Err(e) = stdin.write_all(pixels) {
                            log::warn!("compositor: recording write failed: {e}, stopping");
                            self.recording_active = false;
                        }
                    }
                }
                self.gl.unmap_buffer(glow::PIXEL_PACK_BUFFER);
            }
            self.gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
        }
    }

    /// P6A: Process deferred X11 operations
    /// Called at start of render_frame to batch operations
    pub(super) fn process_deferred_x11_ops(&mut self) {
        while let Some(op) = self.deferred_ops_queue.pop() {
            match op.op_type.as_str() {
                "name_pixmap" => {
                    // Deferred NameWindowPixmap operation
                    // This was originally in event handler, now batched in render thread
                    log::debug!(
                        "compositor: processing deferred name_pixmap for window 0x{:x}",
                        op.window_id
                    );
                    // Implementation would go here (currently placeholder)
                }
                "destroy_pixmap" => {
                    // Deferred pixmap destruction
                    log::debug!(
                        "compositor: processing deferred destroy_pixmap for window 0x{:x}",
                        op.window_id
                    );
                }
                _ => {
                    log::warn!("compositor: unknown deferred op type: {}", op.op_type);
                }
            }
        }
    }
}
