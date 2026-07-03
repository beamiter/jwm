// State accessors, setters, apply_config
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
    pub(crate) fn needs_render(&self) -> bool {
        if self.needs_render || self.recording_active {
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
        // Need render if overview or expose is active (or expose exit animation in progress)
        if self.overview_active || self.expose_active || !self.expose_entries.is_empty() {
            return true;
        }
        // Need render if particles are active
        if !self.particle_systems.is_empty() {
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
        // Need render if magnifier is active (tracking mouse)
        if self.magnifier_enabled {
            return true;
        }
        // Need render if edge glow is active (mouse near screen edge)
        if self.edge_glow && self.edge_glow_active {
            return true;
        }
        // Need render if window tilt is animating
        if self.window_tilt {
            let epsilon = 0.0001;
            if (self.tilt_current_x - self.tilt_target_x).abs() > epsilon
                || (self.tilt_current_y - self.tilt_target_y).abs() > epsilon
                || self.tilt_current_x.abs() > epsilon
                || self.tilt_current_y.abs() > epsilon
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
        // Need render to poll async wallpaper loading
        if self.pending_wallpaper.is_some() || !self.pending_monitor_wallpapers.is_empty() {
            return true;
        }
        false
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

    pub(crate) fn clear_needs_render(&mut self) {
        self.needs_render = false;
    }

    // =====================================================================
    // Feature 8/9/10: Runtime post-processing toggles
    // =====================================================================
    pub(crate) fn set_color_temperature(&mut self, temp: f32) {
        if (self.color_temperature - temp).abs() > f32::EPSILON {
            self.color_temperature = temp;
            self.ensure_postprocess_fbo();
            self.needs_render = true;
        }
    }

    pub(crate) fn set_saturation(&mut self, sat: f32) {
        if (self.saturation - sat).abs() > f32::EPSILON {
            self.saturation = sat;
            self.ensure_postprocess_fbo();
            self.needs_render = true;
        }
    }

    pub(crate) fn set_brightness(&mut self, val: f32) {
        if (self.brightness - val).abs() > f32::EPSILON {
            self.brightness = val;
            self.ensure_postprocess_fbo();
            self.needs_render = true;
        }
    }

    pub(crate) fn set_contrast(&mut self, val: f32) {
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
        self.fade_in_step = anim_speed.apply_fade_step(behavior.fade_in_step);
        self.fade_out_step = anim_speed.apply_fade_step(behavior.fade_out_step);
        self.detect_client_opacity = behavior.detect_client_opacity;
        self.fullscreen_unredirect = behavior.fullscreen_unredirect;

        // --- VSync method (runtime change support) ---
        let new_vsync_method = match behavior.vsync_method.as_str() {
            "oml_sync_control" => {
                if self.oml.is_some()
                    || oml_sync_control::OmlSyncControl::load(self.xlib_display, self.glx_drawable)
                        .is_some()
                {
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
                self.oml =
                    oml_sync_control::OmlSyncControl::load(self.xlib_display, self.glx_drawable);
            }
        }

        self.blur_use_frame_extents = behavior.blur_use_frame_extents;
        self.blur_quality_auto = behavior.blur_quality_auto;

        // --- Blur (may need FBO rebuild) ---
        if self.blur_enabled != behavior.blur_enabled
            || self.blur_strength != behavior.blur_strength
        {
            // Tear down old blur FBOs
            unsafe {
                for level in self.blur_fbos.drain(..) {
                    self.gl.delete_framebuffer(level.fbo);
                    self.gl.delete_texture(level.texture);
                }
            }
            self.blur_enabled = behavior.blur_enabled;
            self.blur_strength = behavior.blur_strength;
            // Recreate if enabled
            if self.blur_enabled {
                self.blur_fbos = unsafe {
                    Self::create_blur_fbos(
                        &self.gl,
                        self.screen_w,
                        self.screen_h,
                        self.blur_strength,
                    )
                };
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
        self.window_animation_scale = behavior.window_animation_scale;

        // --- Dim inactive ---
        self.inactive_dim = behavior.inactive_dim;

        // --- Edge glow ---
        self.edge_glow = behavior.edge_glow;
        self.edge_glow_color = behavior.edge_glow_color;
        self.edge_glow_width = behavior.edge_glow_width;

        // --- Attention animation ---
        self.attention_animation = behavior.attention_animation;
        self.attention_color = behavior.attention_color;

        // --- PiP ---
        self.pip_border_color = behavior.pip_border_color;
        self.pip_border_width = behavior.pip_border_width;

        // --- Magnifier ---
        self.magnifier_enabled = behavior.magnifier_enabled;
        self.magnifier_radius = behavior.magnifier_radius;
        self.magnifier_zoom = behavior.magnifier_zoom;

        // --- Window tilt ---
        self.window_tilt = behavior.window_tilt;
        self.tilt_amount = behavior.tilt_amount;
        self.tilt_perspective = behavior.tilt_perspective;
        self.tilt_speed = behavior.tilt_speed;
        self.tilt_grid = behavior.tilt_grid.max(1);

        // --- Frosted glass ---
        self.frosted_glass_rules
            .clone_from(&behavior.frosted_glass_rules);
        self.frosted_glass_strength = behavior.frosted_glass_strength;

        // --- Wobbly windows ---
        self.wobbly_windows = behavior.wobbly_windows;
        self.wobbly_stiffness = behavior.wobbly_stiffness;
        self.wobbly_damping = behavior.wobbly_damping;
        self.wobbly_restore_stiffness = behavior.wobbly_restore_stiffness;
        self.wobbly_grid_size = behavior.wobbly_grid_size;

        // --- Expose ---
        self.expose_enabled = behavior.expose_enabled;
        self.expose_gap = behavior.expose_gap;

        // --- Snap preview ---
        self.snap_preview_enabled = behavior.snap_preview;
        self.snap_preview_color = behavior.snap_preview_color;
        self.snap_animation_duration_ms = behavior.snap_animation_duration_ms;

        // --- Peek ---
        self.peek_enabled = behavior.peek_enabled;
        self.peek_exclude.clone_from(&behavior.peek_exclude);

        // --- Window tabs ---
        self.window_tabs_enabled = behavior.window_tabs;
        self.tab_bar_height = behavior.tab_bar_height;
        self.tab_bar_color = behavior.tab_bar_color;
        self.tab_active_color = behavior.tab_active_color;

        // --- Particle effects ---
        self.particle_effects = behavior.particle_effects;
        self.particle_count = behavior.particle_count;
        self.particle_lifetime = behavior.particle_lifetime;
        self.particle_gravity = behavior.particle_gravity;

        // --- Motion trail ---
        self.motion_trail_enabled = behavior.motion_trail;
        self.motion_trail_frames = behavior.motion_trail_frames;
        self.motion_trail_opacity = behavior.motion_trail_opacity;

        // --- Genie minimize ---
        self.genie_minimize = behavior.genie_minimize;
        self.genie_duration_ms = behavior.genie_duration_ms;

        // --- Ripple on open ---
        self.ripple_on_open = behavior.ripple_on_open;
        self.ripple_duration = behavior.ripple_duration;
        self.ripple_amplitude = behavior.ripple_amplitude;

        // --- Focus highlight ---
        self.focus_highlight = behavior.focus_highlight;
        self.focus_highlight_color = behavior.focus_highlight_color;
        self.focus_highlight_duration_ms = behavior.focus_highlight_duration_ms;

        // --- Wallpaper crossfade ---
        self.wallpaper_crossfade = behavior.wallpaper_crossfade;
        self.wallpaper_crossfade_duration_ms = behavior.wallpaper_crossfade_duration_ms;

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
        if behavior.wallpaper != self.wallpaper_path || new_mode != self.wallpaper_mode {
            self.wallpaper_mode = new_mode;
            self.wallpaper_path.clone_from(&behavior.wallpaper);
            if !self.wallpaper_path.is_empty() {
                self.pending_wallpaper = Some(Self::load_wallpaper_async(
                    &self.wallpaper_path,
                    self.screen_w,
                    self.screen_h,
                    self.wallpaper_mode,
                ));
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

        self.needs_render = true;
    }

    // =====================================================================
    // Benchmark API
    // =====================================================================

    pub(crate) fn benchmark_start(&mut self, frames: u32, warmup: u32) {
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
        self.benchmark.start(frames, warmup);
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
