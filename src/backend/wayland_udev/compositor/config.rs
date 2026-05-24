use super::*;
use crate::config::CONFIG;

impl WaylandCompositor {
    pub(crate) fn apply_config(&mut self) {
        let cfg = CONFIG.load();
        let b = cfg.behavior();

        // --- Static visual settings ---
        self.corner_radius = b.corner_radius;
        self.shadow_enabled = b.shadow_enabled;
        self.shadow_radius = b.shadow_radius;
        self.shadow_offset = b.shadow_offset;
        self.shadow_color = b.shadow_color;
        self.blur_enabled = b.blur_enabled;
        self.blur_strength = b.blur_strength;
        self.inactive_opacity = b.inactive_opacity;
        self.active_opacity = b.active_opacity;
        self.inactive_dim = b.inactive_dim;
        self.fade_in_step = b.fade_in_step;
        self.fade_out_step = b.fade_out_step;

        // --- Post-processing pipeline ---
        self.color_temperature = b.color_temperature;
        self.saturation = b.saturation;
        self.brightness = b.brightness;
        self.contrast = b.contrast;
        self.invert_colors = b.invert_colors;
        self.grayscale = b.grayscale;
        self.magnifier_enabled = b.magnifier_enabled;
        self.magnifier_zoom = b.magnifier_zoom;
        self.magnifier_radius = b.magnifier_radius;
        self.hdr_enabled = b.hdr_enabled;
        self.hdr_peak_nits = b.hdr_peak_nits;
        self.tone_mapping_method = match b.tone_mapping_method.as_str() {
            "reinhard" => 1,
            "aces" => 2,
            _ => 0,
        };
        self.colorblind_mode = match b.colorblind_mode.as_str() {
            "deuteranopia" => 1,
            "protanopia" => 2,
            "tritanopia" => 3,
            _ => 0,
        };

        self.postprocess_active = self.color_temperature != 0.0
            || self.saturation != 1.0
            || self.brightness != 1.0
            || self.contrast != 1.0
            || self.invert_colors
            || self.grayscale
            || self.magnifier_enabled
            || self.colorblind_mode != 0
            || self.hdr_enabled;

        // --- Animation feature flags ---
        self.fading_enabled = b.fading;
        self.window_animation_enabled = b.window_animation;
        self.edge_glow_enabled = b.edge_glow;
        self.attention_animation_enabled = b.attention_animation;
        self.wobbly_enabled = b.wobbly_windows;
        self.motion_trail_enabled = b.motion_trail;
        self.genie_minimize_enabled = b.genie_minimize;
        self.ripple_on_open_enabled = b.ripple_on_open;
        self.focus_highlight_enabled = b.focus_highlight;
        self.particle_effects_enabled = b.particle_effects;
        self.window_tilt_enabled = b.window_tilt;

        // --- Transition mode ---
        self.transition_mode = match b.transition_mode.as_str() {
            "slide" => TransitionMode::Slide,
            "cube" => TransitionMode::Cube,
            "flip" => TransitionMode::Flip,
            "fade" => TransitionMode::Fade,
            "zoom" => TransitionMode::Zoom,
            "stack" => TransitionMode::Stack,
            "blinds" => TransitionMode::Blinds,
            "coverflow" => TransitionMode::CoverFlow,
            "helix" => TransitionMode::Helix,
            "portal" => TransitionMode::Portal,
            _ => TransitionMode::None,
        };

        self.needs_render = true;
    }

    pub(crate) fn set_color_temperature(&mut self, temp: f32) {
        self.color_temperature = temp;
        self.apply_config();
    }

    pub(crate) fn set_saturation(&mut self, sat: f32) {
        self.saturation = sat;
        self.apply_config();
    }

    pub(crate) fn set_brightness(&mut self, val: f32) {
        self.brightness = val;
        self.apply_config();
    }

    pub(crate) fn set_contrast(&mut self, val: f32) {
        self.contrast = val;
        self.apply_config();
    }

    pub(crate) fn set_invert_colors(&mut self, invert: bool) {
        self.invert_colors = invert;
        self.apply_config();
    }

    pub(crate) fn set_grayscale(&mut self, gs: bool) {
        self.grayscale = gs;
        self.apply_config();
    }

    pub(crate) fn set_debug_hud(&mut self, enabled: bool) {
        self.debug_hud_enabled = enabled;
        self.needs_render = true;
    }

    pub(crate) fn set_transition_mode(&mut self, mode: &str) {
        self.transition_mode = match mode {
            "slide" => TransitionMode::Slide,
            "cube" => TransitionMode::Cube,
            "flip" => TransitionMode::Flip,
            "fade" => TransitionMode::Fade,
            "zoom" => TransitionMode::Zoom,
            "stack" => TransitionMode::Stack,
            "blinds" => TransitionMode::Blinds,
            "coverflow" => TransitionMode::CoverFlow,
            "helix" => TransitionMode::Helix,
            "portal" => TransitionMode::Portal,
            _ => TransitionMode::Slide,
        };
    }

    pub(crate) fn set_magnifier(&mut self, enabled: bool) {
        self.magnifier_enabled = enabled;
        self.apply_config();
    }

    pub(crate) fn set_colorblind_mode(&mut self, mode: &str) {
        self.colorblind_mode = match mode {
            "deuteranopia" => 1,
            "protanopia" => 2,
            "tritanopia" => 3,
            _ => 0,
        };
        self.apply_config();
    }

    pub(crate) fn set_mouse_position(&mut self, x: f32, y: f32) {
        self.mouse_x = x;
        self.mouse_y = y;
        // Update tilt target based on mouse distance from focused window center
        // (simplified: just store for edge glow / magnifier)
        self.needs_render = true;
    }

    pub(crate) fn set_window_urgent(&mut self, window: u64, urgent: bool) {
        if let Some(win) = self.windows.get_mut(&window) {
            win.is_urgent = urgent;
            self.needs_render = true;
        }
    }

    pub(crate) fn set_window_pip(&mut self, window: u64, pip: bool) {
        if let Some(win) = self.windows.get_mut(&window) {
            win.is_pip = pip;
            self.needs_render = true;
        }
    }

    pub(crate) fn set_frame_extents(&mut self, window: u64, left: u32, right: u32, top: u32, bottom: u32) {
        if let Some(win) = self.windows.get_mut(&window) {
            win.frame_extents = [left, right, top, bottom];
        }
    }

    pub(crate) fn set_window_shaped(&mut self, window: u64, shaped: bool) {
        if let Some(win) = self.windows.get_mut(&window) {
            win.is_shaped = shaped;
            self.needs_render = true;
        }
    }

    pub(crate) fn set_overview_mode(&mut self, active: bool, windows: &[(u64, f32, f32, f32, f32, bool, String)]) {
        self.overview_active = active;
        self.overview_entries = windows.iter().map(|(id, x, y, w, h, focused, title)| {
            OverviewEntry { window_id: *id, x: *x, y: *y, w: *w, h: *h, focused: *focused, title: title.clone() }
        }).collect();
        self.needs_render = true;
    }

    pub(crate) fn set_overview_selection(&mut self, window: u64) {
        self.overview_selection = Some(window);
        self.needs_render = true;
    }

    pub(crate) fn set_overview_monitor(&mut self, x: i32, y: i32, w: u32, h: u32) {
        self.overview_monitor = (x, y, w, h);
    }

    pub(crate) fn set_expose_mode(&mut self, active: bool, windows: Vec<(u64, i32, i32, u32, u32)>) {
        self.expose_active = active;
        self.expose_entries = windows.into_iter().map(|(id, x, y, w, h)| {
            ExposeEntry { window_id: id, x, y, w, h }
        }).collect();
        self.needs_render = true;
    }

    pub(crate) fn set_snap_preview(&mut self, preview: Option<(f32, f32, f32, f32)>) {
        self.snap_preview = preview;
        self.needs_render = true;
    }

    pub(crate) fn clear_snap_preview_immediate(&mut self) {
        self.snap_preview = None;
        self.snap_preview_opacity = 0.0;
        self.needs_render = true;
    }

    pub(crate) fn set_peek_mode(&mut self, active: bool) {
        self.peek_active = active;
        self.needs_render = true;
    }

    pub(crate) fn set_dock_position(&mut self, x: f32, y: f32) {
        self.dock_x = x;
        self.dock_y = y;
    }

    pub(crate) fn set_window_groups(&mut self, groups: Vec<(u32, Vec<(u32, String, bool)>)>) {
        self.window_groups = groups;
        self.needs_render = true;
    }

    pub(crate) fn set_monitors(&mut self, monitors: &[(u32, i32, i32, u32, u32)]) {
        self.monitors = monitors.to_vec();
    }

    pub(crate) fn notify_window_move_start(&mut self, window: u64) {
        if !self.wobbly_enabled {
            return;
        }
        if let Some(win) = self.windows.get_mut(&window) {
            let grid_n = 9;
            win.wobbly = Some(WobblyState {
                grid_n,
                offsets: vec![[0.0, 0.0]; grid_n * grid_n],
                velocities: vec![[0.0, 0.0]; grid_n * grid_n],
                dragging: true,
                anchor_row: 0,
                anchor_col: grid_n / 2,
            });
        }
    }

    pub(crate) fn notify_window_move_delta(&mut self, window: u64, dx: f32, dy: f32) {
        if let Some(win) = self.windows.get_mut(&window) {
            if let Some(wobbly) = win.wobbly.as_mut() {
                let anchor_idx = wobbly.anchor_row * wobbly.grid_n + wobbly.anchor_col;
                wobbly.offsets[anchor_idx][0] += dx;
                wobbly.offsets[anchor_idx][1] += dy;
            }
        }
    }

    pub(crate) fn notify_window_move_end(&mut self, window: u64) {
        if let Some(win) = self.windows.get_mut(&window) {
            if let Some(wobbly) = win.wobbly.as_mut() {
                wobbly.dragging = false;
            }
        }
    }

    pub(crate) fn deactivate_edge_glow(&mut self) {
        self.edge_glow_suppressed = true;
    }

    pub(crate) fn unsuppress_edge_glow(&mut self) {
        self.edge_glow_suppressed = false;
        if self.edge_glow_enabled {
            self.edge_glow_active = true;
            self.needs_render = true;
        }
    }

    pub(crate) fn set_annotation_mode(&mut self, active: bool) {
        self.annotation_active = active;
        if !active {
            self.annotation_points.clear();
        }
        self.needs_render = true;
    }

    pub(crate) fn annotation_add_point(&mut self, x: f32, y: f32) {
        self.annotation_points.push((x, y));
        self.needs_render = true;
    }

    pub(crate) fn zoom_to_fit(&mut self, window: Option<u32>) {
        self.zoom_to_fit_window = window;
        self.needs_render = true;
    }

    pub(crate) fn force_full_redraw(&mut self) {
        self.needs_render = true;
    }

    pub(crate) fn fps(&self) -> f32 {
        self.fps
    }

    /// Add a window to the compositor
    #[allow(dead_code)]
    pub(crate) fn add_window(&mut self, window_id: u64) {
        self.windows.entry(window_id).or_insert_with(|| WindowState {
            gl_texture: None,
            width: 0,
            height: 0,
            has_alpha: false,
            y_inverted: false,
            fade_opacity: 0.0, // starts fading in
            fading_out: false,
            anim_scale: 1.0,
            anim_scale_target: 1.0,
            wobbly: None,
            motion_trail: std::collections::VecDeque::new(),
            opacity_override: None,
            corner_radius_override: None,
            frame_extents: [0; 4],
            is_shaped: false,
            is_fullscreen: false,
            is_urgent: false,
            is_pip: false,
            is_frosted: false,
            class_name: String::new(),
            ripple_progress: 0.0,
            ripple_active: false,
        });
        self.needs_render = true;
    }

    /// Remove a window (start fade-out)
    #[allow(dead_code)]
    pub(crate) fn remove_window(&mut self, window_id: u64) {
        if let Some(win) = self.windows.get_mut(&window_id) {
            win.fading_out = true;
            // Optionally spawn particles
            if win.width > 0 && win.height > 0 {
                // particles disabled by default, can be enabled via config
            }
        }
        self.needs_render = true;
    }

    /// Update window texture info, auto-creating the entry if not yet present
    pub(crate) fn update_window_texture(&mut self, window_id: u64, tex_id: u32, w: u32, h: u32, has_alpha: bool, y_inverted: bool) {
        let win = self.windows.entry(window_id).or_insert_with(|| WindowState {
            gl_texture: None,
            width: 0,
            height: 0,
            has_alpha: false,
            y_inverted: false,
            fade_opacity: 0.0,
            fading_out: false,
            anim_scale: 1.0,
            anim_scale_target: 1.0,
            wobbly: None,
            motion_trail: std::collections::VecDeque::new(),
            opacity_override: None,
            corner_radius_override: None,
            frame_extents: [0; 4],
            is_shaped: false,
            is_fullscreen: false,
            is_urgent: false,
            is_pip: false,
            is_frosted: false,
            class_name: String::new(),
            ripple_progress: 0.0,
            ripple_active: false,
        });
        win.gl_texture = Some(tex_id);
        win.width = w;
        win.height = h;
        win.has_alpha = has_alpha;
        win.y_inverted = y_inverted;
        self.needs_render = true;
    }

    /// Notify a tag/workspace switch for transition animation
    pub(crate) fn notify_tag_switch(&mut self, duration: std::time::Duration, direction: i32, _exclude_top: u32, _mon_rect: (i32, i32, u32, u32)) {
        if matches!(self.transition_mode, TransitionMode::None) {
            return;
        }
        self.transition_active = true;
        self.transition_start = Some(std::time::Instant::now());
        self.transition_duration = duration;
        self.transition_direction = direction;
        self.needs_render = true;
    }

    /// Expose click - find which window was clicked
    pub(crate) fn expose_click(&self, x: f32, y: f32) -> Option<u64> {
        for entry in &self.expose_entries {
            let ex = entry.x as f32;
            let ey = entry.y as f32;
            let ew = entry.w as f32;
            let eh = entry.h as f32;
            if x >= ex && x <= ex + ew && y >= ey && y <= ey + eh {
                return Some(entry.window_id);
            }
        }
        None
    }
}
