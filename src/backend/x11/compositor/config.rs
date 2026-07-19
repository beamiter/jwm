// State accessors, setters, apply_config
#[allow(unused_imports)]
use super::math::ortho;
#[allow(unused_imports)]
use super::*;
use crate::backend::compositor_common::effects::finite_clamp;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WallpaperConfigUpdate {
    Unchanged,
    Clear,
    Reload,
}

fn wallpaper_config_update(
    current_path: &str,
    current_requested_mode: WallpaperMode,
    new_path: &str,
    new_mode: WallpaperMode,
) -> WallpaperConfigUpdate {
    if current_path == new_path && current_requested_mode == new_mode {
        WallpaperConfigUpdate::Unchanged
    } else if new_path.is_empty() {
        WallpaperConfigUpdate::Clear
    } else {
        WallpaperConfigUpdate::Reload
    }
}

fn overview_animation_pending_state(
    active: bool,
    closing: bool,
    entry_progress: f32,
    opacity: f32,
    current_angle: f32,
    target_angle: f32,
) -> bool {
    closing
        || (active
            && (entry_progress < 1.0
                || opacity < 1.0
                || (target_angle - current_angle).abs() >= 0.001))
}

fn expose_rect_pending(current: (f32, f32, f32, f32), target: (f32, f32, f32, f32)) -> bool {
    (target.0 - current.0).abs() > 0.5
        || (target.1 - current.1).abs() > 0.5
        || (target.2 - current.2).abs() > 0.5
        || (target.3 - current.3).abs() > 0.5
}

impl<C: CompositorConnection> Compositor<C> {
    pub(super) fn overview_animation_pending(&self) -> bool {
        overview_animation_pending_state(
            self.overview_active,
            self.overview_closing,
            self.overview_entry_progress,
            self.overview_opacity,
            self.overview_prism_current_angle,
            self.overview_prism_target_angle,
        )
    }

    pub(super) fn expose_animation_pending(&self) -> bool {
        if self.expose_entries.is_empty() {
            return false;
        }
        if !self.expose_active {
            // Exit entries remain live until tick_expose has returned them to
            // their source rectangles, faded the overlay, and cleared them.
            return true;
        }

        self.expose_opacity < 1.0
            || self.expose_entries.iter().any(|entry| {
                expose_rect_pending(
                    (
                        entry.current_x,
                        entry.current_y,
                        entry.current_w,
                        entry.current_h,
                    ),
                    (
                        entry.target_x,
                        entry.target_y,
                        entry.target_w,
                        entry.target_h,
                    ),
                )
            })
    }

    pub(crate) fn needs_render(&self) -> bool {
        if self.needs_render || self.damage_render_pending || self.recording_active {
            return true;
        }
        // Also need render if any fade animations are in progress
        if self.fading {
            for wt in self.windows.values() {
                if wt.fading_out || wt.fade_opacity < 1.0 {
                    return true;
                }
            }
        }
        // A steady overview/expose remains composited, but it does not need a
        // fresh frame until client damage or input changes it.
        if self.overview_animation_pending() || self.expose_animation_pending() {
            return true;
        }
        // Need render if particles are active
        if !self.particle_systems.is_empty() {
            return true;
        }
        if !self.genie_active.is_empty() || !self.ripple_active.is_empty() {
            return true;
        }
        if self.motion_trail_enabled && self.windows.values().any(|wt| !wt.motion_trail.is_empty())
        {
            return true;
        }
        // Need render if any window has active wobbly
        if self.wobbly_windows {
            for wt in self.windows.values() {
                if let Some(ref w) = wt.wobbly {
                    if w.dragging
                        || w.offsets
                            .iter()
                            .any(|o| o[0].abs() > 0.1 || o[1].abs() > 0.1)
                        || w.velocities
                            .iter()
                            .any(|v| v[0].abs() > 0.1 || v[1].abs() > 0.1)
                    {
                        return true;
                    }
                }
            }
        }
        // Need render if attention animation is active for any window
        if self.attention_animation {
            for wt in self.windows.values() {
                if wt.is_urgent {
                    return true;
                }
            }
        }
        // The worker wake thread must be observable even while fullscreen
        // unredirect/direct-scanout has stopped regular XDamage rendering.
        if self
            .waterlily_ipc
            .as_ref()
            .is_some_and(WaterlilyIpc::has_pending)
        {
            return true;
        }
        // The native-size WaterLily layer follows smooth random waypoints even
        // when the worker publishes more slowly than the display refresh.
        if self.waterlily_visible() {
            return true;
        }
        // Magnifier and edge glow are pointer-driven. Their setters arm one
        // frame whenever coordinates or active state change; keeping the loop
        // alive while their pixels are otherwise static wastes a full redraw
        // every refresh.
        // Need render if window tilt is animating
        if self.window_tilt {
            let epsilon = 0.0001;
            if (self.tilt_current_x - self.tilt_target_x).abs() > epsilon
                || (self.tilt_current_y - self.tilt_target_y).abs() > epsilon
            {
                return true;
            }
        }
        // Need render if scale animation active
        if self.window_animation {
            for wt in self.windows.values() {
                if (wt.anim_scale - wt.anim_scale_target).abs() > 0.001 {
                    return true;
                }
            }
        }
        if self.tickless_focus_or_wallpaper_animation_active() {
            return true;
        }
        // Need render to poll async wallpaper loading
        if self.pending_wallpaper.is_some() || !self.pending_monitor_wallpapers.is_empty() {
            return true;
        }
        false
    }

    pub(super) fn tickless_focus_or_wallpaper_animation_active(&self) -> bool {
        let focus_active = self.focus_highlight
            && self.focus_highlight_start.is_some_and(|(_, start)| {
                start.elapsed().as_millis() < self.focus_highlight_duration_ms as u128
            });
        focus_active || self.wallpaper_transition_start.is_some()
    }

    pub(crate) fn overlay_window(&self) -> u32 {
        self.overlay_window
    }

    /// Mutable access to OML for syncing
    pub(crate) fn oml_mut(&mut self) -> Option<&mut oml_sync_control::OmlSyncControl> {
        self.oml.as_mut()
    }

    /// Handle Present CompleteNotify event
    pub(crate) fn on_present_complete(&mut self, x11_win: u32, _serial: u32, msc: u64, ust: u64) {
        // Update audio sync tracking
        self.audio_sync.mark_frame_rendered(x11_win);

        // Update OML tracking
        if let Some(oml) = &mut self.oml {
            oml.on_window_presented(x11_win, msc, ust);
        }

        log::debug!(
            "compositor: Present complete for 0x{:x} msc={} ust={}",
            x11_win,
            msc,
            ust
        );
    }

    /// Handle Present IdleNotify event
    pub(crate) fn on_present_idle(&mut self, x11_win: u32, _serial: u32, _pixmap: u32) {
        // Window is idle and ready for next presentation
        log::debug!("compositor: Present idle for 0x{:x}", x11_win);
    }

    pub(crate) fn set_present_manager(&mut self, present_mgr: Option<Box<dyn PresentController>>) {
        self.present_mgr = present_mgr;
    }

    // =====================================================================
    // Feature 8/9/10: Runtime post-processing toggles
    // =====================================================================
    pub(crate) fn set_color_temperature(&mut self, temp: f32) {
        let temp = finite_clamp(temp, -10.0, 10.0, 0.0);
        if (self.color_temperature - temp).abs() > f32::EPSILON {
            self.color_temperature = temp;
            self.ensure_postprocess_fbo();
            self.needs_render = true;
        }
    }

    pub(crate) fn set_saturation(&mut self, sat: f32) {
        let sat = finite_clamp(sat, 0.0, 10.0, 1.0);
        if (self.saturation - sat).abs() > f32::EPSILON {
            self.saturation = sat;
            self.ensure_postprocess_fbo();
            self.needs_render = true;
        }
    }

    pub(crate) fn set_brightness(&mut self, val: f32) {
        let val = finite_clamp(val, 0.0, 10.0, 1.0);
        if (self.brightness - val).abs() > f32::EPSILON {
            self.brightness = val;
            self.ensure_postprocess_fbo();
            self.needs_render = true;
        }
    }

    pub(crate) fn set_contrast(&mut self, val: f32) {
        let val = finite_clamp(val, 0.0, 10.0, 1.0);
        if (self.contrast - val).abs() > f32::EPSILON {
            self.contrast = val;
            self.ensure_postprocess_fbo();
            self.needs_render = true;
        }
    }

    pub(crate) fn set_invert_colors(&mut self, invert: bool) {
        if self.invert_colors != invert {
            self.invert_colors = invert;
            self.ensure_postprocess_fbo();
            self.needs_render = true;
        }
    }

    pub(crate) fn set_grayscale(&mut self, gs: bool) {
        if self.grayscale != gs {
            self.grayscale = gs;
            self.ensure_postprocess_fbo();
            self.needs_render = true;
        }
    }

    // =====================================================================
    // Hot-reload: apply all config changes at once
    // =====================================================================

    /// Re-sync all cached compositor fields from the current config.
    /// Called on config file hot-reload so users don't need to restart.
    pub(crate) fn apply_config(&mut self) {
        let cfg = crate::config::CONFIG.load();
        let behavior = cfg.behavior();
        let anim_speed = cfg.animation_speed();
        let disabling_fading = self.fading && !behavior.fading;
        let disabling_window_animation = self.window_animation && !behavior.window_animation;
        let disabling_wobbly = self.wobbly_windows && !behavior.wobbly_windows;
        let disabling_particles = self.particle_effects && !behavior.particle_effects;
        let disabling_motion_trail = self.motion_trail_enabled && !behavior.motion_trail;
        let disabling_genie = self.genie_minimize && !behavior.genie_minimize;
        let disabling_ripple = self.ripple_on_open && !behavior.ripple_on_open;
        let disabling_tilt = self.window_tilt && !behavior.window_tilt;
        let disabling_focus_highlight = self.focus_highlight && !behavior.focus_highlight;
        let disabling_wallpaper_crossfade =
            self.wallpaper_crossfade && !behavior.wallpaper_crossfade;
        let disabling_edge_glow = self.edge_glow && !behavior.edge_glow;
        let disabling_expose = self.expose_enabled && !behavior.expose_enabled;
        let disabling_snap_preview = self.snap_preview_enabled && !behavior.snap_preview;
        let disabling_peek = self.peek_enabled && !behavior.peek_enabled;

        // --- Core visual settings ---
        self.corner_radius = behavior.corner_radius;
        self.shadow_enabled = behavior.shadow_enabled;
        self.shadow_radius = behavior.shadow_radius;
        self.shadow_offset = behavior.shadow_offset;
        self.shadow_color = behavior.shadow_color;
        self.shadow_bottom_extra = behavior.shadow_bottom_extra;
        self.inactive_opacity = behavior.inactive_opacity;
        self.active_opacity = behavior.active_opacity;
        self.fading = behavior.fading;
        self.fade_in_step = finite_clamp(
            anim_speed.apply_fade_step(behavior.fade_in_step),
            0.0001,
            1.0,
            0.03,
        );
        self.fade_out_step = finite_clamp(
            anim_speed.apply_fade_step(behavior.fade_out_step),
            0.0001,
            1.0,
            0.03,
        );
        self.detect_client_opacity = behavior.detect_client_opacity;
        self.fullscreen_unredirect = behavior.fullscreen_unredirect;
        self.direct_scanout_mgr
            .set_enabled(behavior.direct_scanout_enabled);

        // --- VSync method (runtime change support) ---
        let new_vsync_method = match behavior.vsync_method.as_str() {
            "oml_sync_control" => {
                if self.oml.is_none() {
                    self.oml = self.graphics.load_oml();
                }
                if self.oml.is_some() {
                    VsyncMethod::OmlSyncControl
                } else {
                    log::warn!("vsync_method: OML_sync_control not available, using global vsync");
                    VsyncMethod::Global
                }
            }
            "present" => {
                // Initialize Present if not already available
                if self.present_mgr.is_none() {
                    log::info!("vsync_method: attempting to load Present extension");
                    // Note: Would need conn available here to load Present
                    // For now, just set the method if Present is already loaded
                }
                VsyncMethod::Present
            }
            _ => VsyncMethod::Global,
        };

        if new_vsync_method != self.vsync_method {
            log::info!("compositor: VSync method changed to {:?}", new_vsync_method);
            self.vsync_method = new_vsync_method;
            if new_vsync_method == VsyncMethod::OmlSyncControl && self.oml.is_none() {
                // Try to load OML if not already loaded
                self.oml = self.graphics.load_oml();
            }
        }

        self.blur_use_frame_extents = behavior.blur_use_frame_extents;
        self.blur_quality_auto = behavior.blur_quality_auto;

        // --- Blur (may need FBO rebuild) ---
        let blur_changed = self.blur_enabled != behavior.blur_enabled
            || self.blur_strength != behavior.blur_strength;
        if blur_changed {
            self.clear_window_blur_caches();
            // Tear down old blur FBOs
            unsafe {
                for level in self.blur_fbos.drain(..) {
                    self.gl.delete_framebuffer(level.fbo);
                    self.gl.delete_texture(level.texture);
                }
                if let Some((fbo, tex)) = self.scene_fbo.take() {
                    self.gl.delete_framebuffer(fbo);
                    self.gl.delete_texture(tex);
                }
            }
            self.blur_enabled = behavior.blur_enabled;
            self.blur_strength = behavior.blur_strength;
            // Recreate both the filter chain and its full-size capture target.
            // Previously a false -> true hot reload created only blur_fbos, so
            // blur stayed unavailable until the compositor restarted.
            if self.blur_enabled {
                self.blur_fbos = unsafe {
                    Self::create_blur_fbos(
                        &self.gl,
                        self.screen_w,
                        self.screen_h,
                        self.blur_strength,
                    )
                };
                self.scene_fbo =
                    unsafe { Self::create_scene_fbo(&self.gl, self.screen_w, self.screen_h).ok() };
            }
        } else if self.blur_enabled && self.scene_fbo.is_none() {
            self.scene_fbo =
                unsafe { Self::create_scene_fbo(&self.gl, self.screen_w, self.screen_h).ok() };
        }

        let temporal_changed = self.temporal_blur_enabled != behavior.blur_temporal_enabled
            || (self.temporal_blur_mix_ratio - behavior.blur_temporal_mix_ratio).abs()
                > f32::EPSILON;
        self.temporal_blur_enabled = behavior.blur_temporal_enabled;
        self.temporal_blur_mix_ratio = behavior.blur_temporal_mix_ratio.clamp(0.0, 1.0);
        if temporal_changed {
            self.invalidate_window_blur_caches();
        }
        if !self.blur_enabled {
            self.clear_window_blur_caches();
        } else if !self.temporal_blur_enabled {
            unsafe {
                if let Some((fbo, tex)) = self.temporal_blur_fbo.take() {
                    self.gl.delete_framebuffer(fbo);
                    self.gl.delete_texture(tex);
                }
            }
        }

        // --- Per-window rules (re-parse from strings) ---
        self.shadow_exclude.clone_from(&behavior.shadow_exclude);
        self.blur_exclude.clone_from(&behavior.blur_exclude);
        self.rounded_corners_exclude
            .clone_from(&behavior.rounded_corners_exclude);

        self.opacity_rules = parse_opacity_rules(&behavior.opacity_rules);
        self.corner_radius_rules = parse_corner_radius_rules(&behavior.corner_radius_rules);
        self.scale_rules = parse_scale_rules(&behavior.scale_rules);

        // --- Borders ---
        self.border_enabled = behavior.border_enabled;
        self.border_width = behavior.border_width;
        self.border_color_focused = behavior.border_color_focused;
        self.border_color_unfocused = behavior.border_color_unfocused;

        // --- Color post-processing (use existing setters for postprocess FBO management) ---
        self.set_color_temperature(behavior.color_temperature);
        self.set_saturation(behavior.saturation);
        self.set_brightness(behavior.brightness);
        self.set_contrast(behavior.contrast);
        self.set_invert_colors(behavior.invert_colors);
        self.set_grayscale(behavior.grayscale);
        self.set_colorblind_mode(&behavior.colorblind_mode);

        // --- Debug HUD ---
        self.debug_hud = behavior.debug_hud;
        self.debug_hud_extended = behavior.debug_hud_extended;

        // --- Transition mode ---
        self.set_transition_mode(&behavior.transition_mode);
        self.transition_duration = std::time::Duration::from_millis(anim_speed.apply_duration(150));

        // --- Window animation ---
        self.window_animation = behavior.window_animation;
        self.window_animation_scale = finite_clamp(behavior.window_animation_scale, 0.1, 2.0, 0.92);

        // --- Dim inactive ---
        self.inactive_dim = finite_clamp(behavior.inactive_dim, 0.0, 1.0, 1.0);

        // --- Edge glow ---
        self.edge_glow = behavior.edge_glow;
        self.edge_glow_color = behavior.edge_glow_color;
        self.edge_glow_width = finite_clamp(behavior.edge_glow_width, 0.0, 512.0, 8.0);

        // --- Attention animation ---
        self.attention_animation = behavior.attention_animation;
        self.attention_color = behavior.attention_color;

        // --- PiP ---
        self.pip_border_color = behavior.pip_border_color;
        self.pip_border_width = behavior.pip_border_width;

        // --- Magnifier ---
        self.magnifier_enabled = behavior.magnifier_enabled;
        self.magnifier_radius = finite_clamp(behavior.magnifier_radius, 1.0, 4096.0, 200.0);
        self.magnifier_zoom = finite_clamp(behavior.magnifier_zoom, 1.0, 32.0, 2.0);

        // --- Window tilt ---
        self.window_tilt = behavior.window_tilt;
        self.tilt_amount = finite_clamp(behavior.tilt_amount, 0.0, 0.35, 0.08);
        self.tilt_perspective = finite_clamp(behavior.tilt_perspective, 100.0, 10_000.0, 1_000.0);
        self.tilt_speed = finite_clamp(behavior.tilt_speed, 0.1, 100.0, 8.0);
        self.tilt_grid = behavior.tilt_grid.clamp(1, 64);

        // --- Frosted glass ---
        self.frosted_glass_rules
            .clone_from(&behavior.frosted_glass_rules);
        self.frosted_glass_strength = behavior.frosted_glass_strength;

        // --- Wobbly windows ---
        self.wobbly_windows = behavior.wobbly_windows;
        self.wobbly_stiffness = finite_clamp(behavior.wobbly_stiffness, 0.1, 10_000.0, 600.0);
        self.wobbly_damping = finite_clamp(behavior.wobbly_damping, 0.1, 1_000.0, 30.0);
        self.wobbly_restore_stiffness =
            finite_clamp(behavior.wobbly_restore_stiffness, 0.1, 10_000.0, 200.0);
        self.wobbly_grid_size = behavior
            .wobbly_grid_size
            .min(crate::backend::compositor_common::effects::MAX_WOBBLY_SUBDIVISIONS);

        // --- Expose ---
        self.expose_enabled = behavior.expose_enabled;
        self.expose_gap = finite_clamp(behavior.expose_gap, 0.0, 512.0, 20.0);

        // --- Snap preview ---
        self.snap_preview_enabled = behavior.snap_preview;
        self.snap_preview_color = behavior.snap_preview_color;
        self.snap_animation_duration_ms = behavior.snap_animation_duration_ms;

        // --- Peek ---
        self.peek_enabled = behavior.peek_enabled;
        self.peek_exclude.clone_from(&behavior.peek_exclude);

        // --- Window tabs ---
        self.window_tabs_enabled = behavior.window_tabs;
        self.tab_bar_height = finite_clamp(behavior.tab_bar_height, 1.0, 256.0, 24.0);
        self.tab_bar_color = behavior.tab_bar_color;
        self.tab_active_color = behavior.tab_active_color;

        // --- Particle effects ---
        self.particle_effects = behavior.particle_effects;
        self.particle_count = behavior
            .particle_count
            .min(crate::backend::compositor_common::effects::MAX_PARTICLES_PER_BURST);
        self.particle_lifetime = finite_clamp(behavior.particle_lifetime, 0.001, 30.0, 1.0);
        self.particle_gravity = finite_clamp(behavior.particle_gravity, -10_000.0, 10_000.0, 300.0);

        // --- Motion trail ---
        self.motion_trail_enabled = behavior.motion_trail;
        self.motion_trail_frames = behavior
            .motion_trail_frames
            .min(crate::backend::compositor_common::effects::MAX_MOTION_TRAIL_SAMPLES);
        self.motion_trail_opacity = finite_clamp(behavior.motion_trail_opacity, 0.0, 1.0, 0.3);

        // --- Genie minimize ---
        self.genie_minimize = behavior.genie_minimize;
        self.genie_duration_ms = behavior.genie_duration_ms.clamp(1, 30_000);

        // --- Ripple on open ---
        self.ripple_on_open = behavior.ripple_on_open;
        self.ripple_duration = finite_clamp(behavior.ripple_duration, 0.001, 30.0, 0.4);
        self.ripple_amplitude = finite_clamp(behavior.ripple_amplitude, 0.0, 0.1, 0.015);

        // --- Focus highlight ---
        self.focus_highlight = behavior.focus_highlight;
        self.focus_highlight_color = behavior.focus_highlight_color;
        self.focus_highlight_duration_ms = behavior.focus_highlight_duration_ms.clamp(1, 30_000);

        // --- Wallpaper crossfade ---
        self.wallpaper_crossfade = behavior.wallpaper_crossfade;
        self.wallpaper_crossfade_duration_ms =
            behavior.wallpaper_crossfade_duration_ms.clamp(1, 30_000);

        // --- Annotations ---
        self.annotation_color = behavior.annotation_color;
        self.annotation_line_width = behavior.annotation_line_width;

        // --- Recording ---
        self.recording_fps = behavior.recording_fps;
        self.recording_bitrate
            .clone_from(&behavior.recording_bitrate);
        self.recording_quality = behavior.recording_quality;
        self.recording_encoder
            .clone_from(&behavior.recording_encoder);
        self.recording_output_dir
            .clone_from(&behavior.recording_output_dir);

        // --- Wallpaper (trigger async reload if path or mode changed) ---
        let new_mode = parse_wallpaper_mode(&behavior.wallpaper_mode);
        match wallpaper_config_update(
            &self.wallpaper_path,
            self.wallpaper_requested_mode,
            &behavior.wallpaper,
            new_mode,
        ) {
            WallpaperConfigUpdate::Unchanged => {}
            WallpaperConfigUpdate::Reload => {
                // Dropping the receiver invalidates any older decode. Its
                // worker may finish, but can no longer publish into this
                // compositor after a path or mode change.
                self.pending_wallpaper = None;
                self.wallpaper_path.clone_from(&behavior.wallpaper);
                self.wallpaper_requested_mode = new_mode;
                self.pending_wallpaper = Some(Self::load_wallpaper_async(
                    &self.wallpaper_path,
                    self.screen_w,
                    self.screen_h,
                    self.wallpaper_requested_mode,
                ));
            }
            WallpaperConfigUpdate::Clear => {
                // Clearing the path is an immediate state transition: cancel
                // the decode and retire both sides of any active crossfade.
                self.pending_wallpaper = None;
                self.wallpaper_path.clear();
                self.wallpaper_requested_mode = new_mode;
                self.wallpaper_mode = new_mode;
                self.wallpaper_transition_start = None;
                unsafe {
                    if let Some(texture) = self.wallpaper_texture.take() {
                        self.gl.delete_texture(texture);
                    }
                    if let Some(texture) = self.old_wallpaper_texture.take() {
                        self.gl.delete_texture(texture);
                    }
                }
                self.wallpaper_img_w = 0;
                self.wallpaper_img_h = 0;
                self.old_wallpaper_img_w = 0;
                self.old_wallpaper_img_h = 0;
                self.old_wallpaper_mode = new_mode;
            }
        }

        // --- HDR / tone mapping (hot-reload safe fields only) ---
        self.hdr_peak_nits = behavior.hdr_peak_nits;
        self.tone_mapping_method = match behavior.tone_mapping_method.as_str() {
            "reinhard" => 1,
            "aces" => 2,
            _ => 0,
        };

        // --- Shader hot-reload ---
        if behavior.shader_hot_reload && !behavior.shader_dir.is_empty() {
            self.enable_shader_hot_reload(&behavior.shader_dir);
        } else if !behavior.shader_hot_reload && self.shader_hot_reload_enabled {
            self.shader_hot_reload_enabled = false;
            self.shader_file_mtimes.clear();
        }

        // Hot-disable is a state transition, not just a boolean assignment.
        // Normalize or retire in-flight state so re-enabling an effect cannot
        // resurrect stale meshes/trails and disabled fades cannot retain dead
        // windows indefinitely.
        if disabling_fading {
            let fading_out: Vec<u32> = self
                .windows
                .iter()
                .filter_map(|(&win, wt)| wt.fading_out.then_some(win))
                .collect();
            for wt in self.windows.values_mut() {
                wt.fade_opacity = 1.0;
            }
            for win in fading_out {
                self.remove_window_immediate(win);
            }
        }
        if disabling_window_animation {
            for wt in self.windows.values_mut() {
                wt.anim_scale = 1.0;
                wt.anim_scale_target = 1.0;
            }
        }
        if disabling_wobbly {
            for wt in self.windows.values_mut() {
                wt.wobbly = None;
            }
        }
        if disabling_particles {
            self.particle_systems.clear();
        }
        if disabling_motion_trail {
            for wt in self.windows.values_mut() {
                wt.motion_trail.clear();
                wt.motion_trail_cursor = None;
            }
        }
        if disabling_genie {
            let animations = std::mem::take(&mut self.genie_active);
            for animation in animations {
                self.free_texture_resources(
                    animation.gl_texture,
                    animation.binding,
                    animation.pixmap,
                    animation.damage,
                );
            }
        }
        if disabling_ripple {
            self.ripple_active.clear();
        }
        if disabling_tilt {
            self.tilt_current_x = 0.0;
            self.tilt_current_y = 0.0;
            self.tilt_target_x = 0.0;
            self.tilt_target_y = 0.0;
        }
        if disabling_focus_highlight {
            self.focus_highlight_start = None;
        }
        if disabling_wallpaper_crossfade {
            self.wallpaper_transition_start = None;
            if let Some(texture) = self.old_wallpaper_texture.take() {
                unsafe {
                    self.gl.delete_texture(texture);
                }
            }
            self.old_wallpaper_img_w = 0;
            self.old_wallpaper_img_h = 0;
        }
        if disabling_edge_glow {
            self.edge_glow_active = false;
            self.edge_glow_suppressed = false;
        }
        if disabling_expose {
            self.expose_active = false;
            self.expose_entries.clear();
            self.expose_opacity = 0.0;
            self.expose_start = None;
        }
        if disabling_snap_preview {
            self.snap_target = None;
        }
        if disabling_peek {
            self.peek_active = false;
            self.peek_opacity = 1.0;
            self.peek_start = None;
        }

        self.needs_render = true;
    }

    // =====================================================================
    // Benchmark API
    // =====================================================================

    pub(crate) fn benchmark_start(&mut self, frames: u32, warmup: u32) -> bool {
        if let Err(error) = self.benchmark.try_start(frames, warmup) {
            log::warn!("benchmark: refused to start: {error}");
            return false;
        }
        self.benchmark.system_info = super::benchmark::SystemInfo {
            gpu: String::new(),
            driver: String::new(),
            resolution: format!("{}x{}", self.screen_w, self.screen_h),
        };
        self.benchmark.bench_config = super::benchmark::BenchmarkConfig {
            blur_enabled: self.blur_enabled,
            blur_strength: self.blur_strength,
            window_count: self.windows.len(),
            hdr_enabled: self.hdr_enabled,
            vrr_active: self.vrr_active,
        };
        true
    }

    pub(crate) fn benchmark_stop(&mut self) -> Option<String> {
        self.benchmark
            .stop()
            .map(|r| serde_json::to_string_pretty(&r).unwrap_or_default())
    }

    pub(crate) fn benchmark_report(&self) -> Option<String> {
        if self.benchmark.is_complete() {
            Some(
                serde_json::to_string_pretty(&self.benchmark.generate_report()).unwrap_or_default(),
            )
        } else {
            None
        }
    }

    pub(crate) fn benchmark_is_complete(&self) -> bool {
        self.benchmark.is_complete()
    }

    pub(crate) fn set_hdr_peak_nits(&mut self, nits: f32) {
        self.hdr_peak_nits = nits;
    }

    pub(crate) fn set_eotf_mode(&mut self, mode: i32) {
        self.eotf_mode = mode;
    }

    pub(crate) fn set_output_colorspace(&mut self, cs: i32) {
        self.output_colorspace = cs;
    }

    pub(crate) fn set_hdr_output_10bit(&mut self, enabled: bool) {
        self.hdr_output_10bit = enabled;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        WallpaperConfigUpdate, expose_rect_pending, overview_animation_pending_state,
        wallpaper_config_update,
    };
    use crate::backend::compositor_common::wallpaper::WallpaperMode;

    #[test]
    fn wallpaper_request_change_reloads_for_path_or_mode() {
        assert_eq!(
            wallpaper_config_update(
                "old.png",
                WallpaperMode::Fill,
                "new.png",
                WallpaperMode::Fill
            ),
            WallpaperConfigUpdate::Reload
        );
        assert_eq!(
            wallpaper_config_update(
                "same.png",
                WallpaperMode::Fill,
                "same.png",
                WallpaperMode::Fit
            ),
            WallpaperConfigUpdate::Reload
        );
    }

    #[test]
    fn empty_wallpaper_path_is_an_explicit_clear() {
        assert_eq!(
            wallpaper_config_update("old.png", WallpaperMode::Center, "", WallpaperMode::Stretch),
            WallpaperConfigUpdate::Clear
        );
        assert_eq!(
            wallpaper_config_update("", WallpaperMode::Fit, "", WallpaperMode::Fit),
            WallpaperConfigUpdate::Unchanged
        );
    }

    #[test]
    fn steady_overview_does_not_request_continuous_frames() {
        assert!(!overview_animation_pending_state(
            true, false, 1.0, 1.0, 0.0, 0.0,
        ));
        assert!(overview_animation_pending_state(
            true, false, 0.8, 0.8, 0.0, 0.0,
        ));
        assert!(overview_animation_pending_state(
            true, false, 1.0, 1.0, 0.0, 0.2,
        ));
        assert!(overview_animation_pending_state(
            true, true, 1.0, 1.0, 0.0, 0.0,
        ));
        assert!(overview_animation_pending_state(
            true, false, 1.0, 0.8, 0.0, 0.0,
        ));
    }

    #[test]
    fn expose_geometry_only_ticks_until_it_converges() {
        assert!(!expose_rect_pending(
            (10.0, 20.0, 300.0, 200.0),
            (10.0, 20.0, 300.0, 200.0),
        ));
        assert!(expose_rect_pending(
            (10.0, 20.0, 300.0, 200.0),
            (10.6, 20.0, 300.0, 200.0),
        ));
    }
}
