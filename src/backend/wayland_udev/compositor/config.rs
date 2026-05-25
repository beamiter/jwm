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

        // --- Border config ---
        self.border_enabled = b.border_enabled;
        self.border_width = b.border_width;
        self.border_color_focused = b.border_color_focused;
        self.border_color_unfocused = b.border_color_unfocused;

        // --- Fullscreen unredirect ---
        self.fullscreen_unredirect = b.fullscreen_unredirect;

        // --- VRR ---
        // vrr_active is managed by update_vrr_state(), we just note config is read

        // --- Temporal blur ---
        self.temporal_blur_enabled = b.blur_temporal_enabled;
        self.temporal_blur_mix_ratio = b.blur_temporal_mix_ratio;

        // --- Blur quality ---
        self.blur_quality_auto = b.blur_quality_auto;
        self.blur_strength_by_hz = Self::parse_blur_strength_by_hz(&b.blur_strength_by_hz);
        self.blur_quality_by_monitor =
            Self::parse_blur_quality_by_monitor(&b.blur_quality_by_monitor);

        // --- Subpixel rendering ---
        self.subpixel_mgr.set_enabled(b.blur_enabled);

        // --- Per-window rules ---
        self.opacity_rules = Self::parse_opacity_rules(&b.opacity_rules);
        self.corner_radius_rules = Self::parse_corner_radius_rules(&b.corner_radius_rules);
        self.scale_rules = Self::parse_scale_rules(&b.scale_rules);
        self.frosted_glass_rules = Self::parse_frosted_glass_rules(&b.frosted_glass_rules);
        self.shadow_exclude = b.shadow_exclude.clone();
        self.blur_exclude = b.blur_exclude.clone();
        self.rounded_corners_exclude = b.rounded_corners_exclude.clone();
        self.detect_client_opacity = b.detect_client_opacity;
        self.blur_use_frame_extents = b.blur_use_frame_extents;
        self.shadow_bottom_extra = b.shadow_bottom_extra;

        // --- Window tabs ---
        self.window_tabs_enabled = b.window_tabs;
        self.tab_bar_height = b.tab_bar_height;
        self.tab_bar_color = b.tab_bar_color;
        self.tab_active_color = b.tab_active_color;

        // --- Debug HUD extended ---
        self.debug_hud_extended = b.debug_hud_extended;
        self.frame_profiler.set_enabled(self.debug_hud_extended);

        // --- Animation parameters ---
        self.edge_glow_color = b.edge_glow_color;
        self.edge_glow_width = b.edge_glow_width;
        self.attention_color = b.attention_color;
        self.snap_preview_color = b.snap_preview_color;
        self.snap_animation_duration_ms = b.snap_animation_duration_ms;
        self.peek_exclude = b.peek_exclude.clone();
        self.expose_gap = b.expose_gap;
        self.particle_count = b.particle_count;
        self.particle_lifetime = b.particle_lifetime;
        self.particle_gravity = b.particle_gravity;
        self.motion_trail_frames = b.motion_trail_frames;
        self.motion_trail_opacity = b.motion_trail_opacity;
        self.tilt_speed = b.tilt_speed;
        self.tilt_grid = b.tilt_grid;
        self.wobbly_stiffness = b.wobbly_stiffness;
        self.wobbly_damping = b.wobbly_damping;
        self.wobbly_restore_stiffness = b.wobbly_restore_stiffness;
        self.wobbly_grid_size = b.wobbly_grid_size;
        self.genie_duration_ms = b.genie_duration_ms;
        self.ripple_duration = b.ripple_duration;
        self.ripple_amplitude = b.ripple_amplitude;
        self.focus_highlight_color = b.focus_highlight_color;
        self.focus_highlight_duration_ms = b.focus_highlight_duration_ms;
        self.pip_border_color = b.pip_border_color;
        self.pip_border_width = b.pip_border_width;
        self.window_animation_scale = b.window_animation_scale;

        // --- Wallpaper ---
        self.wallpaper_crossfade = b.wallpaper_crossfade;
        self.wallpaper_crossfade_duration_ms = b.wallpaper_crossfade_duration_ms;
        if !b.wallpaper.is_empty() && b.wallpaper != self.wallpaper_path {
            self.set_wallpaper(&b.wallpaper.clone(), &b.wallpaper_mode.clone());
        }

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
        if active {
            let n = windows.len();
            if n == 0 {
                self.expose_active = false;
                self.expose_entries.clear();
                self.needs_render = true;
                return;
            }

            let sw = self.screen_w as f32;
            let sh = self.screen_h as f32;
            let gap = 20.0f32;
            let screen_aspect = sw / sh;

            let cols = ((n as f32 * screen_aspect).sqrt()).ceil() as u32;
            let cols = cols.max(1);
            let rows = ((n as u32 + cols - 1) / cols).max(1);

            let cell_w = (sw - gap * (cols as f32 + 1.0)) / cols as f32;
            let cell_h = (sh - gap * (rows as f32 + 1.0)) / rows as f32;

            self.expose_entries = windows
                .iter()
                .enumerate()
                .map(|(i, &(id, x, y, w, h))| {
                    let col = i as u32 % cols;
                    let row = i as u32 / cols;

                    let cell_x = gap + col as f32 * (cell_w + gap);
                    let cell_y = gap + row as f32 * (cell_h + gap);

                    let win_aspect = w as f32 / h.max(1) as f32;
                    let cell_aspect = cell_w / cell_h;
                    let (tw, th) = if win_aspect > cell_aspect {
                        (cell_w, cell_w / win_aspect)
                    } else {
                        (cell_h * win_aspect, cell_h)
                    };
                    let tx = cell_x + (cell_w - tw) * 0.5;
                    let ty = cell_y + (cell_h - th) * 0.5;

                    ExposeEntry {
                        window_id: id,
                        orig_x: x as f32,
                        orig_y: y as f32,
                        orig_w: w as f32,
                        orig_h: h as f32,
                        target_x: tx,
                        target_y: ty,
                        target_w: tw,
                        target_h: th,
                        current_x: x as f32,
                        current_y: y as f32,
                        current_w: w as f32,
                        current_h: h as f32,
                        is_hovered: false,
                    }
                })
                .collect();

            self.expose_active = true;
            self.expose_opacity = 0.0;
        } else {
            self.expose_active = false;
        }
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
        self.per_monitor_renderer.set_monitors(monitors);
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
            self.annotation_strokes.clear();
        }
        self.needs_render = true;
    }

    pub(crate) fn annotation_add_point(&mut self, x: f32, y: f32) {
        if !self.annotation_active {
            return;
        }
        if self.annotation_strokes.is_empty() {
            self.annotation_strokes.push(super::AnnotationStroke {
                points: Vec::new(),
                color: self.annotation_color,
                width: self.annotation_line_width,
            });
        }
        if let Some(stroke) = self.annotation_strokes.last_mut() {
            stroke.points.push((x, y));
        }
        self.needs_render = true;
    }

    pub(crate) fn annotation_new_stroke(&mut self) {
        if !self.annotation_active {
            return;
        }
        self.annotation_strokes.push(super::AnnotationStroke {
            points: Vec::new(),
            color: self.annotation_color,
            width: self.annotation_line_width,
        });
    }

    pub(crate) fn set_annotation_color(&mut self, r: f32, g: f32, b: f32, a: f32) {
        self.annotation_color = [r, g, b, a];
    }

    pub(crate) fn set_annotation_line_width(&mut self, width: f32) {
        self.annotation_line_width = width.max(1.0);
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
            frosted_strength: 0.0,
            class_name: String::new(),
            scale: 1.0,
            audio_sync_target: None,
            ripple_progress: 0.0,
            ripple_active: false,
            content_uv: [0.0, 0.0, 1.0, 1.0],
        });
        self.predictive_render_mgr.register_window(window_id);
        self.needs_render = true;
    }

    /// Remove a window (start fade-out)
    #[allow(dead_code)]
    pub(crate) fn remove_window(&mut self, window_id: u64) {
        if let Some(win) = self.windows.get_mut(&window_id) {
            win.fading_out = true;
        }
        self.predictive_render_mgr.remove_window(window_id);
        self.needs_render = true;
    }

    /// Update window texture info, auto-creating the entry if not yet present
    pub(crate) fn update_window_texture(&mut self, window_id: u64, tex_id: u32, w: u32, h: u32, has_alpha: bool, y_inverted: bool, content_uv: [f32; 4]) {
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
            frosted_strength: 0.0,
            class_name: String::new(),
            scale: 1.0,
            audio_sync_target: None,
            ripple_progress: 0.0,
            ripple_active: false,
            content_uv: [0.0, 0.0, 1.0, 1.0],
        });
        win.gl_texture = Some(tex_id);
        win.width = w;
        win.height = h;
        win.has_alpha = has_alpha;
        win.y_inverted = y_inverted;
        win.content_uv = content_uv;
        self.needs_render = true;

        // Feed performance infrastructure
        self.predictive_render_mgr.record_window_damage(window_id);
    }

    /// Set window class/app_id and apply per-class rules (frosted glass, opacity, etc.)
    pub(crate) fn set_window_class(&mut self, window_id: u64, class_name: &str) {
        let frosted = self.lookup_frosted_glass_rule(class_name);
        let opacity_override = self.lookup_opacity_rule(class_name);
        let corner_radius_override = self.lookup_corner_radius_rule(class_name);
        let scale = self.lookup_scale_rule(class_name);

        if let Some(win) = self.windows.get_mut(&window_id) {
            if win.class_name != class_name {
                win.class_name = class_name.to_string();
                win.is_frosted = frosted.is_some();
                win.frosted_strength = frosted.unwrap_or(0.0);
                win.opacity_override = opacity_override;
                win.corner_radius_override = corner_radius_override;
                if let Some(s) = scale {
                    win.scale = s;
                }
                self.subpixel_mgr.register_window(window_id, class_name);
                self.needs_render = true;
            }
        }
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
            if x >= entry.current_x && x <= entry.current_x + entry.current_w
                && y >= entry.current_y && y <= entry.current_y + entry.current_h
            {
                return Some(entry.window_id);
            }
        }
        None
    }

    /// Tick expose animation. Returns true if still animating.
    pub(crate) fn tick_expose(&mut self, dt: f32) {
        if self.expose_entries.is_empty() && self.expose_opacity <= 0.0 {
            return;
        }

        let ease_speed = 12.0f32;
        let t = 1.0 - (-ease_speed * dt).exp();

        if self.expose_active {
            self.expose_opacity = (self.expose_opacity + dt * 4.0).min(1.0);

            for entry in &mut self.expose_entries {
                let dx = entry.target_x - entry.current_x;
                let dy = entry.target_y - entry.current_y;
                let dw = entry.target_w - entry.current_w;
                let dh = entry.target_h - entry.current_h;

                if dx.abs() > 0.5 || dy.abs() > 0.5 || dw.abs() > 0.5 || dh.abs() > 0.5 {
                    entry.current_x += dx * t;
                    entry.current_y += dy * t;
                    entry.current_w += dw * t;
                    entry.current_h += dh * t;
                } else {
                    entry.current_x = entry.target_x;
                    entry.current_y = entry.target_y;
                    entry.current_w = entry.target_w;
                    entry.current_h = entry.target_h;
                }
            }
        } else {
            for entry in &mut self.expose_entries {
                let dx = entry.orig_x - entry.current_x;
                let dy = entry.orig_y - entry.current_y;
                let dw = entry.orig_w - entry.current_w;
                let dh = entry.orig_h - entry.current_h;

                if dx.abs() > 0.5 || dy.abs() > 0.5 || dw.abs() > 0.5 || dh.abs() > 0.5 {
                    entry.current_x += dx * t;
                    entry.current_y += dy * t;
                    entry.current_w += dw * t;
                    entry.current_h += dh * t;
                } else {
                    entry.current_x = entry.orig_x;
                    entry.current_y = entry.orig_y;
                    entry.current_w = entry.orig_w;
                    entry.current_h = entry.orig_h;
                }
            }

            let fade_speed = 8.0;
            self.expose_opacity = (self.expose_opacity - dt * fade_speed).max(0.0);

            if self.expose_opacity <= 0.0 {
                self.expose_entries.clear();
            }
        }
        self.needs_render = true;
    }
}
