use super::*;
use crate::backend::compositor_common::effects::finite_clamp;
use crate::backend::compositor_common::wallpaper::{
    parse_wallpaper_mode, resolve_wallpaper_for_tag,
};
use crate::config::CONFIG;

#[allow(clippy::too_many_arguments)]
fn postprocess_is_active(
    color_temperature: f32,
    saturation: f32,
    brightness: f32,
    contrast: f32,
    invert_colors: bool,
    grayscale: bool,
    magnifier_enabled: bool,
    colorblind_mode: i32,
    hdr_enabled: bool,
) -> bool {
    color_temperature != 0.0
        || saturation != 1.0
        || brightness != 1.0
        || contrast != 1.0
        || invert_colors
        || grayscale
        || magnifier_enabled
        || colorblind_mode != 0
        || hdr_enabled
}

fn mouse_position_requires_render(
    old_position: (f32, f32),
    new_position: (f32, f32),
    magnifier_enabled: bool,
    edge_glow_visible: bool,
    window_tilt_enabled: bool,
) -> bool {
    old_position != new_position && (magnifier_enabled || edge_glow_visible || window_tilt_enabled)
}

fn collect_absent_auxiliary_window_ids(
    known_ids: impl Iterator<Item = u64>,
    live_ids: &HashSet<u64>,
    retired_ids: &mut Vec<u64>,
) {
    retired_ids.clear();
    retired_ids
        .extend(known_ids.filter(|id| is_auxiliary_window_id(*id) && !live_ids.contains(id)));
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WindowRetirement {
    Closed,
    ExplicitlyMinimized,
}

fn retirement_uses_genie(reason: WindowRetirement, genie_enabled: bool) -> bool {
    genie_enabled && reason == WindowRetirement::ExplicitlyMinimized
}

impl WaylandCompositor {
    fn recompute_postprocess_active(&mut self) {
        self.postprocess_active = postprocess_is_active(
            self.color_temperature,
            self.saturation,
            self.brightness,
            self.contrast,
            self.invert_colors,
            self.grayscale,
            self.magnifier_enabled,
            self.colorblind_mode,
            self.hdr_enabled,
        );
        self.needs_render = true;
    }

    pub(crate) fn set_system_ui(&mut self, overlay: Option<crate::backend::api::SystemUiOverlay>) {
        self.system_ui = overlay;
        self.needs_render = true;
    }

    pub(crate) fn has_system_ui(&self) -> bool {
        self.system_ui.is_some()
    }
    pub(crate) fn apply_config(&mut self) {
        let cfg = CONFIG.load();
        let b = cfg.behavior();
        let disabling_fading = self.fading_enabled && !b.fading;
        let disabling_window_animation = self.window_animation_enabled && !b.window_animation;
        let disabling_wobbly = self.wobbly_enabled && !b.wobbly_windows;
        let disabling_motion_trail = self.motion_trail_enabled && !b.motion_trail;
        let disabling_genie = self.genie_minimize_enabled && !b.genie_minimize;
        let disabling_ripple = self.ripple_on_open_enabled && !b.ripple_on_open;
        let disabling_particles = self.particle_effects_enabled && !b.particle_effects;
        let disabling_tilt = self.window_tilt_enabled && !b.window_tilt;

        // --- Static visual settings ---
        self.corner_radius = b.corner_radius;
        self.shadow_enabled = b.shadow_enabled;
        self.shadow_radius = b.shadow_radius;
        self.shadow_offset = b.shadow_offset;
        self.shadow_color = b.shadow_color;
        self.blur_enabled = b.blur_enabled;
        self.blur_strength = b.blur_strength;
        self.inactive_opacity = finite_clamp(b.inactive_opacity, 0.0, 1.0, 0.9);
        self.active_opacity = finite_clamp(b.active_opacity, 0.0, 1.0, 1.0);
        self.inactive_dim = finite_clamp(b.inactive_dim, 0.0, 1.0, 1.0);
        self.fade_in_step = finite_clamp(b.fade_in_step, 0.0001, 1.0, 0.03);
        self.fade_out_step = finite_clamp(b.fade_out_step, 0.0001, 1.0, 0.03);

        // --- Post-processing pipeline ---
        self.color_temperature = b.color_temperature;
        self.saturation = b.saturation;
        self.brightness = b.brightness;
        self.contrast = b.contrast;
        self.invert_colors = b.invert_colors;
        self.grayscale = b.grayscale;
        self.magnifier_enabled = b.magnifier_enabled;
        self.magnifier_zoom = finite_clamp(b.magnifier_zoom, 1.0, 32.0, 2.0);
        self.magnifier_radius = finite_clamp(b.magnifier_radius, 1.0, 4096.0, 200.0);
        self.hdr_enabled = b.hdr_enabled;
        self.hdr_peak_nits = b.hdr_peak_nits;
        self.scene_linear_requested = b.scene_linear_compositing;
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

        self.recompute_postprocess_active();

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
        self.transition_mode = TransitionMode::from_name_or_none(b.transition_mode.as_str());
        if matches!(self.transition_mode, TransitionMode::None) {
            self.transition_active = false;
            self.transition_snapshot_pending = false;
            self.transition_start = None;
        }

        // --- Border config ---
        self.border_enabled = b.border_enabled;
        self.border_width = b.border_width;
        self.border_color_focused = b.border_color_focused;
        self.border_color_unfocused = b.border_color_unfocused;

        // --- Fullscreen unredirect ---
        // Note: the `fullscreen_unredirect` behavior flag is consumed directly
        // in udev_kms.rs at the KMS direct-scanout eligibility check; no
        // compositor field is needed here.
        self.direct_scanout_mgr
            .set_enabled(b.direct_scanout_enabled);

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
        self.shadow_exclude.clone_from(&b.shadow_exclude);
        self.blur_exclude.clone_from(&b.blur_exclude);
        self.rounded_corners_exclude
            .clone_from(&b.rounded_corners_exclude);
        self.detect_client_opacity = b.detect_client_opacity;
        self.blur_use_frame_extents = b.blur_use_frame_extents;
        self.shadow_bottom_extra = b.shadow_bottom_extra;

        // --- Window tabs ---
        self.window_tabs_enabled = b.window_tabs;
        self.tab_bar_height = finite_clamp(b.tab_bar_height, 1.0, 256.0, 24.0);
        self.tab_bar_color = b.tab_bar_color;
        self.tab_active_color = b.tab_active_color;

        // --- Debug HUD extended ---
        self.debug_hud_extended = b.debug_hud_extended;
        self.frame_profiler.set_enabled(self.debug_hud_extended);

        // --- Animation parameters ---
        self.edge_glow_color = b.edge_glow_color;
        self.edge_glow_width = finite_clamp(b.edge_glow_width, 0.0, 512.0, 8.0);
        self.attention_color = b.attention_color;
        self.snap_preview_color = b.snap_preview_color;
        self.snap_animation_duration_ms = b.snap_animation_duration_ms;
        self.peek_exclude.clone_from(&b.peek_exclude);
        self.expose_gap = finite_clamp(b.expose_gap, 0.0, 512.0, 20.0);
        self.particle_count = b
            .particle_count
            .min(crate::backend::compositor_common::effects::MAX_PARTICLES_PER_BURST);
        self.particle_lifetime = finite_clamp(b.particle_lifetime, 0.001, 30.0, 1.0);
        self.particle_gravity = finite_clamp(b.particle_gravity, -10_000.0, 10_000.0, 300.0);
        self.motion_trail_frames = b
            .motion_trail_frames
            .min(crate::backend::compositor_common::effects::MAX_MOTION_TRAIL_SAMPLES);
        self.motion_trail_opacity = finite_clamp(b.motion_trail_opacity, 0.0, 1.0, 0.3);
        self.tilt_speed = finite_clamp(b.tilt_speed, 0.1, 100.0, 8.0);
        self.tilt_grid = b.tilt_grid.clamp(1, 64);
        self.tilt_amount = finite_clamp(b.tilt_amount, 0.0, 0.35, 0.08);
        self.tilt_perspective = finite_clamp(b.tilt_perspective, 100.0, 10_000.0, 1_000.0);
        self.wobbly_stiffness = finite_clamp(b.wobbly_stiffness, 0.1, 10_000.0, 600.0);
        self.wobbly_damping = finite_clamp(b.wobbly_damping, 0.1, 1_000.0, 30.0);
        self.wobbly_restore_stiffness =
            finite_clamp(b.wobbly_restore_stiffness, 0.1, 10_000.0, 200.0);
        self.wobbly_grid_size = b
            .wobbly_grid_size
            .min(crate::backend::compositor_common::effects::MAX_WOBBLY_SUBDIVISIONS);
        self.genie_duration_ms = b.genie_duration_ms.clamp(1, 30_000);
        self.ripple_duration = finite_clamp(b.ripple_duration, 0.001, 30.0, 0.4);
        self.ripple_amplitude = finite_clamp(b.ripple_amplitude, 0.0, 0.1, 0.015);
        self.focus_highlight_color = b.focus_highlight_color;
        self.focus_highlight_duration_ms = b.focus_highlight_duration_ms.clamp(1, 30_000);
        self.pip_border_color = b.pip_border_color;
        self.pip_border_width = b.pip_border_width;
        self.window_animation_scale = finite_clamp(b.window_animation_scale, 0.1, 2.0, 0.92);

        // --- Wallpaper ---
        self.wallpaper_crossfade = b.wallpaper_crossfade;
        self.wallpaper_crossfade_duration_ms = b.wallpaper_crossfade_duration_ms.clamp(1, 30_000);
        if b.wallpaper != self.wallpaper_path
            || parse_wallpaper_mode(&b.wallpaper_mode) != self.wallpaper_mode
        {
            self.set_wallpaper(&b.wallpaper.clone(), &b.wallpaper_mode.clone());
        }

        if disabling_fading {
            self.windows.retain(|_, win| !win.fading_out);
            for win in self.windows.values_mut() {
                win.fade_opacity = 1.0;
            }
        }
        if disabling_window_animation {
            for win in self.windows.values_mut() {
                win.anim_scale = 1.0;
                win.anim_scale_target = 1.0;
            }
        }
        if disabling_wobbly {
            for win in self.windows.values_mut() {
                win.wobbly = None;
            }
        }
        if disabling_motion_trail {
            for win in self.windows.values_mut() {
                win.motion_trail.clear();
            }
        }
        if disabling_genie {
            for animation in self.genie_active.drain(..) {
                self.windows.remove(&animation.window_id);
            }
        }
        if disabling_ripple {
            for win in self.windows.values_mut() {
                win.ripple_active = false;
                win.ripple_progress = 0.0;
            }
        }
        if disabling_particles {
            self.particle_systems.clear();
        }
        if disabling_tilt {
            self.tilt_x = 0.0;
            self.tilt_y = 0.0;
            self.tilt_target_x = 0.0;
            self.tilt_target_y = 0.0;
        }

        self.needs_render = true;
    }

    pub(crate) fn set_color_temperature(&mut self, temp: f32) {
        let temp = finite_clamp(temp, -10.0, 10.0, 0.0);
        if self.color_temperature == temp {
            return;
        }
        self.color_temperature = temp;
        self.recompute_postprocess_active();
    }

    pub(crate) fn set_saturation(&mut self, sat: f32) {
        let sat = finite_clamp(sat, 0.0, 10.0, 1.0);
        if self.saturation == sat {
            return;
        }
        self.saturation = sat;
        self.recompute_postprocess_active();
    }

    pub(crate) fn set_brightness(&mut self, val: f32) {
        let val = finite_clamp(val, 0.0, 10.0, 1.0);
        if self.brightness == val {
            return;
        }
        self.brightness = val;
        self.recompute_postprocess_active();
    }

    pub(crate) fn set_contrast(&mut self, val: f32) {
        let val = finite_clamp(val, 0.0, 10.0, 1.0);
        if self.contrast == val {
            return;
        }
        self.contrast = val;
        self.recompute_postprocess_active();
    }

    pub(crate) fn set_invert_colors(&mut self, invert: bool) {
        if self.invert_colors == invert {
            return;
        }
        self.invert_colors = invert;
        self.recompute_postprocess_active();
    }

    pub(crate) fn set_grayscale(&mut self, gs: bool) {
        if self.grayscale == gs {
            return;
        }
        self.grayscale = gs;
        self.recompute_postprocess_active();
    }

    pub(crate) fn set_debug_hud(&mut self, enabled: bool) {
        self.debug_hud_enabled = enabled;
        self.needs_render = true;
    }

    pub(crate) fn set_debug_hud_extended(&mut self, enabled: bool) {
        self.debug_hud_extended = enabled;
        self.frame_profiler.set_enabled(enabled);
        self.needs_render = true;
    }

    pub(crate) fn set_transition_mode(&mut self, mode: &str) {
        let mode = TransitionMode::from_name_or_none(mode);
        if self.transition_mode != mode {
            self.transition_mode = mode;
            if matches!(mode, TransitionMode::None) {
                self.transition_active = false;
                self.transition_snapshot_pending = false;
                self.transition_start = None;
            }
            self.needs_render = true;
        }
    }

    pub(crate) fn set_magnifier(&mut self, enabled: bool) {
        if self.magnifier_enabled == enabled {
            return;
        }
        self.magnifier_enabled = enabled;
        self.recompute_postprocess_active();
    }

    pub(crate) fn set_colorblind_mode(&mut self, mode: &str) {
        let mode = match mode {
            "deuteranopia" => 1,
            "protanopia" => 2,
            "tritanopia" => 3,
            _ => 0,
        };
        if self.colorblind_mode == mode {
            return;
        }
        self.colorblind_mode = mode;
        self.recompute_postprocess_active();
    }

    pub(crate) fn set_mouse_position(&mut self, x: f32, y: f32) {
        let moved = self.mouse_x != x || self.mouse_y != y;
        let requires_render = mouse_position_requires_render(
            (self.mouse_x, self.mouse_y),
            (x, y),
            self.magnifier_enabled,
            super::render::edge_glow_requires_continuous_frames(
                self.edge_glow_enabled,
                self.edge_glow_width,
                self.edge_glow_active,
                self.edge_glow_suppressed,
            ),
            self.window_tilt_enabled,
        );
        self.mouse_x = x;
        self.mouse_y = y;
        if requires_render {
            self.needs_render = true;
        }
        if moved && self.expose_active {
            self.set_expose_hover(x, y);
        }
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

    pub(crate) fn set_frame_extents(
        &mut self,
        window: u64,
        left: u32,
        right: u32,
        top: u32,
        bottom: u32,
    ) {
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

    pub(crate) fn set_window_fullscreen(&mut self, window: u64, fullscreen: bool) {
        if let Some(win) = self.windows.get_mut(&window)
            && win.is_fullscreen != fullscreen
        {
            win.is_fullscreen = fullscreen;
            self.needs_render = true;
        }
    }

    pub(crate) fn set_overview_mode(
        &mut self,
        active: bool,
        windows: &[(u64, f32, f32, f32, f32, bool, String)],
    ) {
        self.overview_active = active;
        self.overview_entries = windows
            .iter()
            .map(|(id, x, y, w, h, focused, title)| OverviewEntry {
                window_id: *id,
                x: *x,
                y: *y,
                w: *w,
                h: *h,
                focused: *focused,
                title: title.clone(),
            })
            .collect();
        self.overview_selection = if active {
            self.overview_entries
                .iter()
                .find(|entry| entry.focused)
                .or_else(|| self.overview_entries.first())
                .map(|entry| entry.window_id)
        } else {
            None
        };
        self.needs_render = true;
    }

    pub(crate) fn set_overview_selection(&mut self, window: u64) {
        self.overview_selection = Some(window);
        self.needs_render = true;
    }

    pub(crate) fn set_overview_monitor(&mut self, x: i32, y: i32, w: u32, h: u32) {
        self.overview_monitor = (x, y, w, h);
    }

    pub(crate) fn set_expose_mode(
        &mut self,
        active: bool,
        windows: Vec<(u64, i32, i32, u32, u32)>,
    ) {
        if active {
            if windows.is_empty() {
                self.expose_active = false;
                self.expose_entries.clear();
                self.needs_render = true;
                return;
            }

            self.expose_entries = crate::backend::compositor_common::expose::build_expose_entries(
                self.screen_w as f32,
                self.screen_h as f32,
                20.0,
                &windows,
            );

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
        if self.window_groups == groups {
            return;
        }
        self.window_groups = groups;
        self.needs_render = true;
    }

    pub(crate) fn set_monitors(&mut self, monitors: &[(u32, i32, i32, u32, u32, u32)]) {
        // Detect topology change: if monitor count or geometry differs we have to
        // tear down existing per-monitor wallpaper textures. If only `active_tags`
        // changed (typical view/toggleview path), keep existing textures and just
        // re-resolve paths so unchanged monitors don't trigger a reload.
        let geometry_changed = self.monitors.len() != monitors.len()
            || self
                .monitors
                .iter()
                .zip(monitors.iter())
                .any(|(a, b)| (a.0, a.1, a.2, a.3, a.4) != (b.0, b.1, b.2, b.3, b.4));

        self.monitors = monitors.to_vec();
        self.per_monitor_renderer.set_monitors(monitors);

        let cfg = CONFIG.load();
        let behavior = cfg.behavior();

        if geometry_changed {
            self.retired_wallpaper_textures.extend(
                self.monitor_wallpapers
                    .drain(..)
                    .filter_map(|wallpaper| wallpaper.texture),
            );
            self.pending_monitor_wallpapers.clear();
        }

        for (slot, &(idx, x, y, w, h, active_tags)) in monitors.iter().enumerate() {
            let (path, mode_str) = resolve_wallpaper_for_tag(behavior, idx, active_tags);
            let path = path.to_string();
            let mode = parse_wallpaper_mode(mode_str);

            if geometry_changed {
                if !path.is_empty() {
                    let rx = Self::load_wallpaper_async(&path, w, h, mode);
                    self.pending_monitor_wallpapers.push((slot, rx));
                }
                self.monitor_wallpapers.push(MonitorWallpaper {
                    mon_x: x,
                    mon_y: y,
                    mon_w: w,
                    mon_h: h,
                    texture: None,
                    mode,
                    img_w: 0,
                    img_h: 0,
                    current_path: path,
                });
            } else if let Some(mw) = self.monitor_wallpapers.get_mut(slot) {
                if mw.current_path != path || mw.mode != mode {
                    // A newer request supersedes any decode still in flight for
                    // this monitor; otherwise the older result can win the race.
                    self.pending_monitor_wallpapers
                        .retain(|(mon_idx, _)| *mon_idx != slot);
                    mw.mode = mode;
                    mw.current_path = path.clone();
                    if !path.is_empty() {
                        let rx = Self::load_wallpaper_async(&path, w, h, mode);
                        self.pending_monitor_wallpapers.push((slot, rx));
                    } else {
                        if let Some(texture) = mw.texture.take() {
                            self.retired_wallpaper_textures.push(texture);
                        }
                        mw.img_w = 0;
                        mw.img_h = 0;
                    }
                }
            }
        }

        self.needs_render = true;
    }

    pub(crate) fn notify_window_move_start(&mut self, window: u64) {
        if let Some(win) = self.windows.get_mut(&window) {
            win.is_moving = true;
            win.motion_trail.clear();
            if self.wobbly_enabled {
                let grid_n = crate::backend::compositor_common::effects::wobbly_node_count(
                    self.wobbly_grid_size,
                );
                let (anchor_row, anchor_col) = self
                    .prev_scene
                    .iter()
                    .find(|&&(id, _, _, _, _)| id == window)
                    .map(|&(_, x, y, w, h)| {
                        WobblyState::anchor_for_point(
                            grid_n,
                            self.mouse_x - x as f32,
                            self.mouse_y - y as f32,
                            w as f32,
                            h as f32,
                        )
                    })
                    .unwrap_or((0, grid_n / 2));
                win.wobbly = Some(WobblyState::new(grid_n, anchor_row, anchor_col));
            }
        }
    }

    pub(crate) fn notify_window_move_delta(&mut self, window: u64, dx: f32, dy: f32) {
        if let Some(win) = self.windows.get_mut(&window) {
            if let Some(wobbly) = win.wobbly.as_mut() {
                // The window geometry has already moved; apply inverse inertia
                // to the remaining nodes just like the X11 backend.
                wobbly.apply_window_move_delta(dx, dy);
            }
        }
    }

    pub(crate) fn notify_window_move_end(&mut self, window: u64) {
        if let Some(win) = self.windows.get_mut(&window) {
            win.is_moving = false;
            if let Some(wobbly) = win.wobbly.as_mut() {
                wobbly.end_drag();
            }
        }
    }

    pub(crate) fn deactivate_edge_glow(&mut self) {
        if !self.edge_glow_suppressed {
            self.edge_glow_suppressed = true;
            // Produce one cleanup frame to erase the previously rendered glow;
            // suppressed state must not keep the loop armed after that frame.
            self.needs_render = true;
        }
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

    #[allow(dead_code)]
    pub(crate) fn set_annotation_color(&mut self, r: f32, g: f32, b: f32, a: f32) {
        self.annotation_color = [r, g, b, a];
    }

    #[allow(dead_code)]
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
        let fading_enabled = self.fading_enabled;
        let window_animation_enabled = self.window_animation_enabled;
        let window_animation_scale = self.window_animation_scale;
        let ripple_enabled = self.ripple_on_open_enabled;
        let mut inserted = false;
        self.windows.entry(window_id).or_insert_with(|| {
            inserted = true;
            WindowState {
                gl_texture: None,
                texture_owner: None,
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
                last_motion_position: None,
                opacity_override: None,
                corner_radius_override: None,
                frame_extents: [0; 4],
                is_shaped: false,
                is_fullscreen: false,
                is_urgent: false,
                is_pip: false,
                is_moving: false,
                is_frosted: false,
                frosted_strength: 0.0,
                class_name: String::new(),
                scale: 1.0,
                audio_sync_target: None,
                ripple_progress: 0.0,
                ripple_active: false,
                content_uv: [0.0, 0.0, 1.0, 1.0],
                closing_rect: None,
                is_genie_minimizing: false,
                color_transform: None,
            }
        });
        if inserted && let Some(win) = self.windows.get_mut(&window_id) {
            win.fade_opacity = if fading_enabled { 0.0 } else { 1.0 };
            win.anim_scale = if window_animation_enabled {
                window_animation_scale
            } else {
                1.0
            };
            win.ripple_active = ripple_enabled;
            win.ripple_progress = 0.0;
        }
        self.predictive_render_mgr.register_window(window_id);
        self.needs_render = true;
    }

    /// Retire a window whose client surface was unmapped or destroyed.
    ///
    /// Ordinary surface retirement is a close, not a minimize request, so it
    /// may use the close fade but never targets the Dock with a genie effect.
    pub(crate) fn remove_window(&mut self, window_id: u64) {
        self.retire_window(window_id, WindowRetirement::Closed);
    }

    /// Retire a window after an explicit foreign-toplevel minimize request.
    ///
    /// This is the only retirement path allowed to start the genie effect.
    /// Strong `GlesTexture` handles keep either animation path safe after the
    /// live surface/offscreen cache releases its owner.
    pub(crate) fn minimize_window(&mut self, window_id: u64) {
        self.retire_window(window_id, WindowRetirement::ExplicitlyMinimized);
    }

    fn retire_window(&mut self, window_id: u64, reason: WindowRetirement) {
        if !self.windows.contains_key(&window_id) {
            self.predictive_render_mgr.remove_window(window_id);
            self.is_game_window.remove(&window_id);
            return;
        }

        // Unmap and destruction notifications can both arrive for the same
        // surface. Retirement is idempotent so the second notification cannot
        // duplicate particles/genie entries or restart a close fade.
        if self
            .windows
            .get(&window_id)
            .is_some_and(|win| win.fading_out || win.is_genie_minimizing)
        {
            return;
        }

        let closing_scene_rect = self
            .prev_scene
            .iter()
            .find(|&&(id, _, _, _, _)| id == window_id)
            .map(|&(_, x, y, w, h)| (x, y, w, h));
        let closing_rect =
            closing_scene_rect.map(|(x, y, w, h)| (x as f32, y as f32, w as f32, h as f32));

        if let Some((x, y, w, h)) = closing_scene_rect {
            self.spawn_particles_for_window(x, y, w, h);
        }

        let mut started_genie = false;
        if retirement_uses_genie(reason, self.genie_minimize_enabled) {
            if let Some((x, y, w, h)) = closing_rect {
                if let Some(win) = self.windows.get_mut(&window_id) {
                    if let Some(texture_owner) = win.texture_owner.clone() {
                        win.is_genie_minimizing = true;
                        win.closing_rect = Some((x, y, w, h));
                        self.genie_active.push(super::GenieAnimation {
                            window_id,
                            start: Instant::now(),
                            x,
                            y,
                            w,
                            h,
                            texture_owner,
                            has_alpha: win.has_alpha,
                            y_inverted: win.y_inverted,
                            content_uv: win.content_uv,
                        });
                        started_genie = true;
                    }
                }
            }
        }
        if !started_genie {
            if let Some(win) = self.windows.get_mut(&window_id) {
                win.fading_out = true;
                win.closing_rect = closing_rect;
                if self.window_animation_enabled {
                    win.anim_scale_target = self.window_animation_scale;
                }
                if win.texture_owner.is_none() || win.closing_rect.is_none() {
                    // There is nothing safe or visible to animate. Let the
                    // normal fade cleanup retire the metadata this frame.
                    win.fade_opacity = 0.0;
                }
            }
        }
        self.predictive_render_mgr.remove_window(window_id);
        self.is_game_window.remove(&window_id);
        self.needs_render = true;
    }

    /// Retire synthetic xdg/IME popup states that no longer occur in the
    /// backend-provided scene.
    ///
    /// `remove_window` leaves the strong texture owner on the close-fade
    /// WindowState, so the backend may release an associated offscreen cache
    /// entry after this returns.
    pub(crate) fn retire_absent_auxiliary_windows(&mut self, scene: &[(u64, i32, i32, u32, u32)]) {
        self.scratch_curr_ids.clear();
        self.scratch_curr_ids
            .extend(scene.iter().map(|&(id, _, _, _, _)| id));
        collect_absent_auxiliary_window_ids(
            self.windows.keys().copied(),
            &self.scratch_curr_ids,
            &mut self.scratch_retired_aux_ids,
        );

        let mut retired_ids = std::mem::take(&mut self.scratch_retired_aux_ids);
        for window_id in retired_ids.iter().copied() {
            self.remove_window(window_id);
        }
        retired_ids.clear();
        self.scratch_retired_aux_ids = retired_ids;
    }

    /// Update window texture info, auto-creating the entry if not yet present
    pub(crate) fn update_window_texture(
        &mut self,
        window_id: u64,
        texture_owner: GlesTexture,
        w: u32,
        h: u32,
        has_alpha: bool,
        y_inverted: bool,
        content_uv: [f32; 4],
    ) {
        let fading_enabled = self.fading_enabled;
        let window_animation_enabled = self.window_animation_enabled;
        let window_animation_scale = self.window_animation_scale;
        let ripple_enabled = self.ripple_on_open_enabled;
        let was_retiring = self
            .windows
            .get(&window_id)
            .is_some_and(|win| win.fading_out || win.is_genie_minimizing);
        if was_retiring {
            // A Wayland/XWayland surface may attach a new buffer with the same
            // id after unmapping. Cancel the stale retirement before updating
            // its texture so tick_fades/tick_genie cannot delete the remap.
            self.genie_active
                .retain(|animation| animation.window_id != window_id);
        }
        let mut inserted = false;
        let win = self.windows.entry(window_id).or_insert_with(|| {
            inserted = true;
            WindowState {
                gl_texture: None,
                texture_owner: None,
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
                last_motion_position: None,
                opacity_override: None,
                corner_radius_override: None,
                frame_extents: [0; 4],
                is_shaped: false,
                is_fullscreen: false,
                is_urgent: false,
                is_pip: false,
                is_moving: false,
                is_frosted: false,
                frosted_strength: 0.0,
                class_name: String::new(),
                scale: 1.0,
                audio_sync_target: None,
                ripple_progress: 0.0,
                ripple_active: false,
                content_uv: [0.0, 0.0, 1.0, 1.0],
                closing_rect: None,
                is_genie_minimizing: false,
                color_transform: None,
            }
        });
        if inserted {
            win.fade_opacity = if fading_enabled { 0.0 } else { 1.0 };
            win.anim_scale = if window_animation_enabled {
                window_animation_scale
            } else {
                1.0
            };
            win.ripple_active = ripple_enabled;
            win.ripple_progress = 0.0;
        } else if was_retiring {
            win.fading_out = false;
            win.is_genie_minimizing = false;
            win.closing_rect = None;
            win.fade_opacity = if fading_enabled {
                win.fade_opacity.max(0.0)
            } else {
                1.0
            };
            win.anim_scale_target = 1.0;
            win.ripple_active = ripple_enabled;
            win.ripple_progress = 0.0;
        }
        if inserted || was_retiring {
            self.predictive_render_mgr.register_window(window_id);
        }
        let tex_id = texture_owner.tex_id();
        win.gl_texture = Some(tex_id);
        win.texture_owner = Some(texture_owner);
        win.width = w;
        win.height = h;
        win.has_alpha = has_alpha;
        win.y_inverted = y_inverted;
        win.content_uv = content_uv;
        self.needs_render = true;

        // Record content damage for partial-damage (scissored) redraw.
        self.content_dirty_ids.insert(window_id);

        // Feed performance infrastructure
        self.predictive_render_mgr.record_window_damage(window_id);
    }

    /// Set window class/app_id and apply per-class rules (frosted glass, opacity, etc.)
    ///
    /// Called once per window every frame from the render dispatch, so the
    /// class-unchanged fast path must do zero work: the (allocating) rule
    /// lookups only run when the class actually changes, which is essentially
    /// only at window map time.
    pub(crate) fn set_window_class(&mut self, window_id: u64, class_name: &str) {
        // Fast path: bail before any rule lookups if nothing changed.
        match self.windows.get(&window_id) {
            Some(win) if win.class_name == class_name => return,
            None => return,
            _ => {}
        }

        let frosted = self.lookup_frosted_glass_rule(class_name);
        let opacity_override = self.lookup_opacity_rule(class_name);
        let corner_radius_override = self.lookup_corner_radius_rule(class_name);
        let scale = self.lookup_scale_rule(class_name);

        if let Some(win) = self.windows.get_mut(&window_id) {
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

    /// Set the per-window surface→output color transform, used by the window
    /// fragment shader when `behavior.color_management_render_path` is enabled.
    pub(crate) fn set_window_color_transform(
        &mut self,
        window_id: u64,
        xform: Option<crate::backend::wayland_udev::color_pipeline::ColorTransform>,
    ) {
        let mut removed_active_transform = false;
        if let Some(win) = self.windows.get_mut(&window_id) {
            removed_active_transform = win.color_transform.is_some() && xform.is_none();
            win.color_transform = xform;
            if xform.is_some() {
                self.any_color_transform_active = true;
            }
        }
        if removed_active_transform {
            self.any_color_transform_active = self
                .windows
                .values()
                .any(|win| win.color_transform.is_some());
        }
    }

    /// Clear every window's color transform in a single pass and reset the
    /// "any active" flag before rebuilding the current frame's snapshot.
    pub(crate) fn clear_all_color_transforms(&mut self) {
        if !self.any_color_transform_active {
            return;
        }
        for win in self.windows.values_mut() {
            win.color_transform = None;
        }
        self.any_color_transform_active = false;
    }

    /// Notify a tag/workspace switch for transition animation
    pub(crate) fn notify_tag_switch(
        &mut self,
        duration: std::time::Duration,
        direction: i32,
        exclude_top: u32,
        mon_rect: (i32, i32, u32, u32),
    ) {
        let exclude_top = exclude_top.min(mon_rect.3);
        if matches!(self.transition_mode, TransitionMode::None)
            || duration.is_zero()
            || super::transitions::transition_layout(
                self.screen_w,
                self.screen_h,
                mon_rect,
                exclude_top,
            )
            .is_none()
        {
            self.transition_active = false;
            self.transition_snapshot_pending = false;
            self.transition_start = None;
            self.transition_mon = None;
            return;
        }
        self.transition_mon = Some(mon_rect);
        self.transition_exclude_top = exclude_top;
        self.transition_active = true;
        self.transition_snapshot_pending = true;
        self.transition_start = Some(std::time::Instant::now());
        self.transition_duration = duration;
        self.transition_direction = if direction < 0 { -1 } else { 1 };
        self.needs_render = true;
    }

    /// Expose click - find which window was clicked
    pub(crate) fn expose_click(&self, x: f32, y: f32) -> Option<u64> {
        for entry in &self.expose_entries {
            if x >= entry.current_x
                && x <= entry.current_x + entry.current_w
                && y >= entry.current_y
                && y <= entry.current_y + entry.current_h
            {
                return Some(entry.id);
            }
        }
        None
    }

    /// Tick expose animation via the shared platform-neutral implementation.
    pub(crate) fn tick_expose(&mut self, dt: f32) {
        if self.expose_entries.is_empty() && self.expose_opacity <= 0.0 {
            return;
        }

        let result = crate::backend::compositor_common::expose::tick_expose_entries(
            &mut self.expose_entries,
            self.expose_active,
            &mut self.expose_opacity,
            dt,
        );
        if result.clear_entries {
            self.expose_entries.clear();
        }
        self.needs_render = true;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        IME_POPUP_WINDOW_ID_PREFIX, WindowRetirement, XDG_POPUP_WINDOW_ID_PREFIX,
        collect_absent_auxiliary_window_ids, is_auxiliary_window_id,
        mouse_position_requires_render, postprocess_is_active, retirement_uses_genie,
    };
    use std::collections::HashSet;

    #[test]
    fn postprocess_activation_tracks_runtime_controls() {
        let neutral = (0.0, 1.0, 1.0, 1.0, false, false, false, 0, false);

        assert!(!postprocess_is_active(
            neutral.0, neutral.1, neutral.2, neutral.3, neutral.4, neutral.5, neutral.6, neutral.7,
            neutral.8,
        ));
        assert!(postprocess_is_active(
            0.1, neutral.1, neutral.2, neutral.3, neutral.4, neutral.5, neutral.6, neutral.7,
            neutral.8,
        ));
        assert!(postprocess_is_active(
            neutral.0, 0.9, neutral.2, neutral.3, neutral.4, neutral.5, neutral.6, neutral.7,
            neutral.8,
        ));
        assert!(postprocess_is_active(
            neutral.0, neutral.1, 0.9, neutral.3, neutral.4, neutral.5, neutral.6, neutral.7,
            neutral.8,
        ));
        assert!(postprocess_is_active(
            neutral.0, neutral.1, neutral.2, 0.9, neutral.4, neutral.5, neutral.6, neutral.7,
            neutral.8,
        ));
        assert!(postprocess_is_active(
            neutral.0, neutral.1, neutral.2, neutral.3, true, neutral.5, neutral.6, neutral.7,
            neutral.8,
        ));
        assert!(postprocess_is_active(
            neutral.0, neutral.1, neutral.2, neutral.3, neutral.4, true, neutral.6, neutral.7,
            neutral.8,
        ));
        assert!(postprocess_is_active(
            neutral.0, neutral.1, neutral.2, neutral.3, neutral.4, neutral.5, true, neutral.7,
            neutral.8,
        ));
        assert!(postprocess_is_active(
            neutral.0, neutral.1, neutral.2, neutral.3, neutral.4, neutral.5, neutral.6, 1,
            neutral.8,
        ));
        assert!(postprocess_is_active(
            neutral.0, neutral.1, neutral.2, neutral.3, neutral.4, neutral.5, neutral.6, neutral.7,
            true,
        ));
    }

    #[test]
    fn mouse_position_only_dirties_pointer_driven_effects_on_change() {
        let old = (10.0, 20.0);
        let moved = (11.0, 20.0);

        assert!(!mouse_position_requires_render(old, old, true, true, true));
        assert!(!mouse_position_requires_render(
            old, moved, false, false, false
        ));
        assert!(mouse_position_requires_render(
            old, moved, true, false, false
        ));
        assert!(mouse_position_requires_render(
            old, moved, false, true, false
        ));
        assert!(mouse_position_requires_render(
            old, moved, false, false, true
        ));
    }

    #[test]
    fn absent_auxiliary_cleanup_ignores_live_and_real_windows() {
        let live_xdg = XDG_POPUP_WINDOW_ID_PREFIX | 11;
        let dead_xdg = XDG_POPUP_WINDOW_ID_PREFIX | 12;
        let dead_ime = IME_POPUP_WINDOW_ID_PREFIX | 13;
        let real_window = 42;
        let live_ids = HashSet::from([live_xdg, real_window]);
        let mut retired_ids = Vec::new();

        collect_absent_auxiliary_window_ids(
            [live_xdg, dead_xdg, dead_ime, real_window].into_iter(),
            &live_ids,
            &mut retired_ids,
        );
        retired_ids.sort_unstable();

        let mut expected = vec![dead_xdg, dead_ime];
        expected.sort_unstable();
        assert_eq!(retired_ids, expected);
        assert!(is_auxiliary_window_id(live_xdg));
        assert!(is_auxiliary_window_id(dead_ime));
        assert!(!is_auxiliary_window_id(real_window));
    }

    #[test]
    fn genie_is_reserved_for_explicit_minimize_retirement() {
        assert!(!retirement_uses_genie(WindowRetirement::Closed, true));
        assert!(!retirement_uses_genie(
            WindowRetirement::ExplicitlyMinimized,
            false
        ));
        assert!(retirement_uses_genie(
            WindowRetirement::ExplicitlyMinimized,
            true
        ));
    }
}
