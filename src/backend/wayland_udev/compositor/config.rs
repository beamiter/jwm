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

fn retained_color_plan_geometry(
    rect: crate::backend::api::CompositorRect,
) -> Option<(i32, i32, u32, u32)> {
    let rect = rect.normalized()?;
    Some((
        rect.x.round().clamp(i32::MIN as f32, i32::MAX as f32) as i32,
        rect.y.round().clamp(i32::MIN as f32, i32::MAX as f32) as i32,
        rect.width.round().clamp(1.0, u32::MAX as f32) as u32,
        rect.height.round().clamp(1.0, u32::MAX as f32) as u32,
    ))
}

fn apply_expose_terminal_cleanup<Id>(
    entries: &mut Vec<crate::backend::compositor_common::expose::ExposeEntry<Id>>,
    clear_entries: bool,
) -> bool {
    if clear_entries {
        entries.clear();
        true
    } else {
        false
    }
}

const fn should_request_static_minimized_capture(
    dock_addressable: bool,
    minimized: bool,
    cached_visual: bool,
    active_animation: bool,
    restore_pending: bool,
    static_capture_pending: bool,
) -> bool {
    dock_addressable
        && minimized
        && !cached_visual
        && !active_animation
        && !restore_pending
        && !static_capture_pending
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WindowRetirement {
    Closed,
    ExplicitlyMinimized,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DisabledGenieAction {
    CacheMinimized,
    CompleteRestore,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RetainedColorGenerationAction {
    RecaptureMinimized,
    CompleteRestore,
}

const fn retained_color_generation_action(
    direction: crate::backend::compositor_common::genie::GenieDirection,
) -> RetainedColorGenerationAction {
    match direction {
        crate::backend::compositor_common::genie::GenieDirection::Minimize => {
            RetainedColorGenerationAction::RecaptureMinimized
        }
        crate::backend::compositor_common::genie::GenieDirection::Restore => {
            RetainedColorGenerationAction::CompleteRestore
        }
    }
}

fn legacy_retained_placement_changed(
    render_path_enabled: bool,
    scene_linear_active: bool,
    previous: Option<crate::backend::api::CompositorRect>,
    next: Option<crate::backend::api::CompositorRect>,
) -> bool {
    render_path_enabled && !scene_linear_active && previous != next
}

fn legacy_retained_preview_placement_changed(
    render_path_enabled: bool,
    scene_linear_active: bool,
    stable_dock_target: Option<crate::backend::api::CompositorRect>,
    previous_preview: Option<crate::backend::api::CompositorRect>,
    next_preview: Option<crate::backend::api::CompositorRect>,
) -> bool {
    stable_dock_target.is_none()
        && legacy_retained_placement_changed(
            render_path_enabled,
            scene_linear_active,
            previous_preview,
            next_preview,
        )
}

fn retained_output_profile_at<'a>(
    outputs: &'a [RetainedOutputColorContext],
    placement: crate::backend::api::CompositorRect,
) -> Option<&'a RetainedOutputColorContext> {
    let (x, y, width, height) = retained_color_plan_geometry(placement)?;
    let left = i64::from(x);
    let top = i64::from(y);
    let right = left + i64::from(width);
    let bottom = top + i64::from(height);

    outputs
        .iter()
        .filter_map(|output| {
            let [output_x, output_y, output_width, output_height] = output.rect;
            if output_width <= 0 || output_height <= 0 {
                return None;
            }
            let output_left = i64::from(output_x);
            let output_top = i64::from(output_y);
            let output_right = output_left + i64::from(output_width);
            let output_bottom = output_top + i64::from(output_height);
            let overlap_width = right.min(output_right) - left.max(output_left);
            let overlap_height = bottom.min(output_bottom) - top.max(output_top);
            (overlap_width > 0 && overlap_height > 0)
                .then_some((overlap_width * overlap_height, output))
        })
        .max_by_key(|(area, _)| *area)
        .map(|(_, output)| output)
}

fn retained_output_profiles_compatible(
    outputs: &[RetainedOutputColorContext],
    dock_target: crate::backend::api::CompositorRect,
    preview_anchor: crate::backend::api::CompositorRect,
) -> bool {
    let Some(dock_profile) = retained_output_profile_at(outputs, dock_target) else {
        return false;
    };
    let Some(preview_profile) = retained_output_profile_at(outputs, preview_anchor) else {
        return false;
    };
    dock_profile.output_tf == preview_profile.output_tf
        && dock_profile.working_to_output_row_major == preview_profile.working_to_output_row_major
}

/// Resolve in-flight state when Genie is disabled at runtime. A queued
/// restore is the newest user intent and therefore wins over an older active
/// minimize. `None` means there is no work for this window.
fn disabled_genie_action(
    active_direction: Option<crate::backend::compositor_common::genie::GenieDirection>,
    restore_pending: bool,
) -> Option<DisabledGenieAction> {
    use crate::backend::compositor_common::genie::GenieDirection;

    match (active_direction, restore_pending) {
        (_, true) | (Some(GenieDirection::Restore), false) => {
            Some(DisabledGenieAction::CompleteRestore)
        }
        (Some(GenieDirection::Minimize), false) => Some(DisabledGenieAction::CacheMinimized),
        (None, false) => None,
    }
}

fn clear_immediate_restore_collections<Visual, Target>(
    window_id: u64,
    minimized_visuals: &mut std::collections::HashMap<u64, Visual>,
    minimized_windows: &mut HashSet<u64>,
    pending_restores: &mut HashSet<u64>,
    genie_targets: &mut std::collections::HashMap<u64, Target>,
    preview_window: Option<u64>,
) -> bool {
    minimized_visuals.remove(&window_id);
    minimized_windows.remove(&window_id);
    pending_restores.remove(&window_id);
    genie_targets.remove(&window_id);
    preview_window == Some(window_id)
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
        if overlay.is_none() || overlay.as_ref().is_some_and(|ui| ui.filmstrip.is_some()) {
            self.system_ui_hit_geometry = None;
        }
        let viewport_changed = matches!(
            (self.system_ui.as_deref(), overlay.as_ref()),
            (Some(old), Some(new)) if old.viewport != new.viewport || old.locked != new.locked
        );
        let text_changed = match (self.system_ui.as_deref(), overlay.as_ref()) {
            (Some(old), Some(new)) => {
                old.title != new.title
                    || old.query != new.query
                    || old.items != new.items
                    || old.hint != new.hint
                    || viewport_changed
            }
            (None, None) => false,
            _ => true,
        };
        self.sysui_text_dirty |= text_changed;
        if text_changed {
            self.system_ui_hit_geometry = None;
            self.system_ui_hovered = None;
        }
        // Opening a panel springs it out of the bar; closing one forgets its
        // geometry so the next open springs again rather than resuming.
        if self.system_ui.is_none() || overlay.is_none() {
            self.system_ui_island.close();
        }
        // A different panel is a different surface: its width starts over, and
        // its selection appears where it belongs instead of sliding in from a
        // row of the panel it replaced. An update to the *same* panel — a
        // keystroke, a finished scan — keeps both, which is the whole point of
        // carrying them.
        let identity = overlay.as_ref().map_or("", |o| o.title.as_str());
        if identity != self.system_ui_identity || viewport_changed {
            self.system_ui_identity = identity.to_string();
            self.system_ui_width_floor = 0.0;
            self.system_ui_highlight.reset();
            self.system_ui_hovered = None;
        }
        self.system_ui = overlay.map(Arc::new);
        self.needs_render = true;
    }

    pub(crate) fn set_system_ui_hover(&mut self, row: Option<usize>) -> bool {
        if self.system_ui_hovered != row {
            self.system_ui_hovered = row;
            self.needs_render = true;
            return true;
        }
        false
    }

    pub(crate) fn system_ui_hit_test(
        &self,
        x: f64,
        y: f64,
    ) -> crate::backend::api::SystemUiHitTarget {
        use crate::backend::api::SystemUiHitTarget;
        use crate::backend::compositor_common::system_ui_panel::Hit;

        match self
            .system_ui_hit_geometry
            .map(|geometry| geometry.hit_test(x, y))
        {
            Some(Hit::Panel) => SystemUiHitTarget::Panel,
            Some(Hit::Item(row)) => SystemUiHitTarget::Item(row),
            Some(Hit::Outside) => SystemUiHitTarget::Outside,
            None => SystemUiHitTarget::Unavailable,
        }
    }

    pub(crate) fn push_toast(&mut self, toast: crate::backend::api::ToastNotification) {
        let removed = self.toast_stack.push(toast, std::time::Instant::now());
        self.toast_retired.extend(removed);
        self.needs_render = true;
    }

    /// Hit-test last frame's toast card rects; a hit dismisses the card.
    /// Returns true whenever the point lands on a card so the WM swallows
    /// the click instead of replaying it to the window below.
    pub(crate) fn click_toast(&mut self, x: f32, y: f32) -> bool {
        let hit = self.toast_rects.iter().find_map(|(id, rect)| {
            let [rx, ry, w, h] = *rect;
            (x >= rx && x <= rx + w && y >= ry && y <= ry + h).then_some(*id)
        });
        let Some(id) = hit else {
            return false;
        };
        self.toast_stack.dismiss(id, std::time::Instant::now());
        self.needs_render = true;
        true
    }

    pub(crate) fn show_osd(&mut self, kind: crate::backend::api::OsdKind, percent: u8) {
        self.osd_slot.show(kind, percent, std::time::Instant::now());
        self.needs_render = true;
    }

    pub(crate) fn show_media_osd(&mut self, label: &str) {
        self.osd_slot.show_media(label, std::time::Instant::now());
        self.needs_render = true;
    }

    pub(crate) fn has_system_ui(&self) -> bool {
        self.system_ui.is_some()
    }

    /// Whether the window open/close animation drives window alpha, i.e. the
    /// fade machinery must run for it even when standalone `fading` is off.
    pub(crate) fn window_animation_uses_fade(&self) -> bool {
        self.window_animation_enabled && self.window_animation_style.uses_fade()
    }

    /// Resolve a window's current open/close animation transform from its
    /// carriers. Scale styles progress through `anim_scale`; alpha-driven
    /// styles progress through `fade_opacity` (which also applies the alpha).
    pub(crate) fn window_animation_frame_for(&self, win: &WindowState) -> WindowAnimationFrame {
        if !self.window_animation_enabled {
            return WindowAnimationFrame::REST;
        }
        let progress = if self.window_animation_style.uses_fade() {
            win.fade_opacity
        } else {
            scale_carrier_progress(win.anim_scale, self.window_animation_scale)
        };
        window_animation_frame(
            self.window_animation_style,
            progress,
            self.window_animation_scale,
        )
    }

    pub(crate) fn apply_config(&mut self) {
        // Font and theme changes invalidate baked text even when the overlay's
        // strings themselves did not change.
        self.sysui_text_dirty = true;
        let cfg = CONFIG.load();
        let b = cfg.behavior();
        let window_animation_style = WindowAnimationStyle::from_name(&b.window_animation_style);
        // The open/close fade machinery is driven by the standalone `fading`
        // feature and by alpha-driven animation styles. When a reload leaves
        // no driver behind, in-flight fades would freeze mid-decay (nothing
        // advances `fade_opacity` any more), so they are settled below.
        let fade_was_driven = self.fading_enabled
            || (self.window_animation_enabled && self.window_animation_style.uses_fade());
        let fade_now_driven =
            b.fading || (b.window_animation && window_animation_style.uses_fade());
        let settling_fades = fade_was_driven && !fade_now_driven;
        let disabling_window_animation = self.window_animation_enabled && !b.window_animation;
        let disabling_wobbly = self.wobbly_enabled && !b.wobbly_windows;
        let disabling_motion_trail = self.motion_trail_enabled && !b.motion_trail;
        let disabling_genie = self.genie_minimize_enabled && !b.genie_minimize;
        let disabling_ripple = self.ripple_on_open_enabled && !b.ripple_on_open;
        let disabling_particles = self.particle_effects_enabled && !b.particle_effects;
        let disabling_tilt = self.window_tilt_enabled && !b.window_tilt;
        let disabling_overview = self.overview_enabled && !b.overview_enabled;
        let disabling_expose = self.expose_enabled && !b.expose_enabled;
        let disabling_snap_preview = self.snap_preview_enabled && !b.snap_preview;
        let disabling_peek = self.peek_enabled && !b.peek_enabled;

        // --- Static visual settings ---
        self.corner_radius = b.corner_radius;
        self.shadow_enabled = b.shadow_enabled;
        self.shadow_radius = b.shadow_radius;
        self.shadow_offset = b.shadow_offset;
        self.shadow_color = b.shadow_color;
        self.shadow_inactive_opacity = finite_clamp(b.shadow_inactive_opacity, 0.0, 1.0, 1.0);
        self.blur_enabled = b.blur_enabled;
        self.blur_strength = b.blur_strength;
        self.inactive_opacity = finite_clamp(b.inactive_opacity, 0.0, 1.0, 0.9);
        self.active_opacity = finite_clamp(b.active_opacity, 0.0, 1.0, 1.0);
        self.inactive_dim = finite_clamp(b.inactive_dim, 0.0, 1.0, 1.0);
        self.inactive_desaturate = finite_clamp(b.inactive_desaturate, 0.0, 1.0, 0.0);
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
        self.scene_linear_requested = crate::config::scene_linear_render_path_requested(
            b.color_management_render_path,
            b.scene_linear_compositing,
        );
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
        self.window_animation_style = window_animation_style;
        self.edge_glow_enabled = b.edge_glow;
        self.attention_animation_enabled = b.attention_animation;
        self.wobbly_enabled = b.wobbly_windows;
        self.motion_trail_enabled = b.motion_trail;
        self.genie_minimize_enabled = b.genie_minimize;
        self.ripple_on_open_enabled = b.ripple_on_open;
        self.focus_highlight_enabled = b.focus_highlight;
        self.particle_effects_enabled = b.particle_effects;
        self.window_tilt_enabled = b.window_tilt;
        self.overview_enabled = b.overview_enabled;
        self.expose_enabled = b.expose_enabled;
        self.snap_preview_enabled = b.snap_preview;
        self.peek_enabled = b.peek_enabled;

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
        self.border_gradient_enabled = b.border_gradient_enabled;
        self.border_gradient_color_a = b.border_gradient_color_a;
        self.border_gradient_color_b = b.border_gradient_color_b;
        self.border_gradient_angle = b.border_gradient_angle;
        self.border_gradient_speed = b.border_gradient_speed;

        // --- Fullscreen unredirect ---
        // KMS consumes both flags directly. Mirror their combined gate in the
        // compositor-side diagnostic tracker so it cannot report an active
        // eligibility session while the actual fast path is configured off.
        self.direct_scanout_mgr
            .set_enabled(b.direct_scanout_enabled && b.fullscreen_unredirect);

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
        // The strip follows `appearance.ui_theme` and the system-UI font, and
        // its titles are baked into textures with the ink of whichever theme
        // was live when they were rasterized. A reload can change both, so the
        // cache is dropped rather than left carrying the old palette.
        self.tab_titles_dirty = true;

        // --- Debug HUD extended ---
        self.debug_hud_extended = b.debug_hud_extended;
        self.frame_profiler.set_enabled(self.debug_hud_extended);

        // --- Animation parameters ---
        self.edge_glow_color = b.edge_glow_color;
        self.edge_glow_width = finite_clamp(b.edge_glow_width, 0.0, 512.0, 8.0);
        self.attention_color = b.attention_color;
        self.snap_preview_color = b.snap_preview_color;
        self.snap_animation_duration_ms = b.snap_animation_duration_ms.clamp(1, 30_000);
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

        if settling_fades {
            self.windows.retain(|_, win| !win.fading_out);
            self.refresh_any_color_transform_active();
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
            // A restore may be queued before its remapped surface reaches the
            // next render. Drain that queue as part of the hot-disable
            // transition; otherwise the provisional restoring marker and Dock
            // cache can survive forever and unnecessarily block direct
            // scanout. Pending restore intent takes precedence over an active
            // minimize for the same window.
            let mut pending_restores = std::mem::take(&mut self.pending_genie_restores);
            let animations = std::mem::take(&mut self.genie_active);
            for animation in animations {
                let restore_pending = pending_restores.remove(&animation.window_id);
                match disabled_genie_action(Some(animation.direction), restore_pending) {
                    Some(DisabledGenieAction::CacheMinimized) => {
                        let window_id = animation.window_id;
                        self.cache_minimized_visual(animation);
                        self.take_live_window_preserving_metadata(window_id);
                    }
                    Some(DisabledGenieAction::CompleteRestore) => {
                        self.complete_genie_restore_immediately(animation.window_id);
                    }
                    None => unreachable!("an active Genie always needs a disable action"),
                }
            }
            for window_id in pending_restores {
                debug_assert_eq!(
                    disabled_genie_action(None, true),
                    Some(DisabledGenieAction::CompleteRestore)
                );
                self.complete_genie_restore_immediately(window_id);
            }
            self.refresh_any_color_transform_active();
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
        if disabling_overview {
            self.clear_overview_state_immediate();
        }
        if disabling_expose {
            self.clear_expose_state_immediate();
        }
        if disabling_snap_preview {
            self.clear_snap_preview_state_immediate();
        }
        if disabling_peek {
            self.clear_peek_state_immediate();
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
        if self.debug_hud_enabled != enabled {
            // Toggling forgets the card's geometry, so showing it again
            // springs it out of the bar rather than resuming mid-open.
            self.hud_island.close();
        }
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
        // The tab bar's hover cell follows the same channel: hit-test the
        // groups and repaint only when the hovered cell actually changes.
        let tab_hover =
            crate::backend::compositor_common::window_tabs::tab_hover_at(&self.window_groups, x, y);
        if tab_hover != self.tab_hover {
            self.tab_hover = tab_hover;
            self.needs_render = true;
        }
        // Hovering a toast card pauses its timeout; same compare-then-repaint
        // pattern as the tab bar above.
        let toast_hover = self.toast_rects.iter().find_map(|(id, rect)| {
            let [rx, ry, w, h] = *rect;
            (x >= rx && x <= rx + w && y >= ry && y <= ry + h).then_some(*id)
        });
        if toast_hover != self.toast_hover {
            self.toast_hover = toast_hover;
            self.toast_stack
                .set_hovered(toast_hover, std::time::Instant::now());
            self.needs_render = true;
        }
    }

    pub(crate) fn set_window_urgent(&mut self, window: u64, urgent: bool) {
        let mut changed = if let Some(win) = self.windows.get_mut(&window) {
            let changed = win.is_urgent != urgent;
            win.is_urgent = urgent;
            let pending_changed = self.pending_window_urgency.discard(window);
            changed || pending_changed
        } else {
            self.pending_window_urgency.update(window, urgent)
        };
        if let Some(metadata) = self.minimized_window_metadata.get_mut(&window) {
            changed |= metadata.is_urgent != urgent;
            metadata.is_urgent = urgent;
            self.pending_window_urgency.discard(window);
        }
        if changed {
            self.needs_render = true;
        }
    }

    pub(crate) fn set_window_pip(&mut self, window: u64, pip: bool) {
        if let Some(win) = self.windows.get_mut(&window) {
            win.is_pip = pip;
            self.needs_render = true;
        }
        if let Some(metadata) = self.minimized_window_metadata.get_mut(&window) {
            metadata.is_pip = pip;
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
        if let Some(metadata) = self.minimized_window_metadata.get_mut(&window) {
            metadata.frame_extents = [left, right, top, bottom];
        }
    }

    pub(crate) fn set_window_shaped(&mut self, window: u64, shaped: bool) {
        if let Some(win) = self.windows.get_mut(&window) {
            win.is_shaped = shaped;
            self.needs_render = true;
        }
        if let Some(metadata) = self.minimized_window_metadata.get_mut(&window) {
            metadata.is_shaped = shaped;
        }
    }

    pub(crate) fn set_window_fullscreen(&mut self, window: u64, fullscreen: bool) {
        if let Some(win) = self.windows.get_mut(&window)
            && win.is_fullscreen != fullscreen
        {
            win.is_fullscreen = fullscreen;
            self.needs_render = true;
        }
        if let Some(metadata) = self.minimized_window_metadata.get_mut(&window) {
            metadata.is_fullscreen = fullscreen;
        }
    }

    fn clear_overview_state_immediate(&mut self) {
        self.overview_active = false;
        self.overview_opacity = 0.0;
        self.overview_entries.clear();
        self.overview_selection = None;
        self.overview_rotation = 0.0;
        self.overview_target_rotation = 0.0;
        self.overview_titles_dirty = false;
        self.force_full_damage_next = true;
    }

    fn clear_expose_state_immediate(&mut self) {
        self.expose_active = false;
        self.expose_opacity = 0.0;
        self.expose_entries.clear();
        self.expose_start = None;
        self.force_full_damage_next = true;
    }

    fn clear_snap_preview_state_immediate(&mut self) {
        self.snap_preview = None;
        self.snap_preview_target_visible = false;
        self.snap_preview_opacity = 0.0;
        self.force_full_damage_next = true;
    }

    fn clear_peek_state_immediate(&mut self) {
        self.peek_active = false;
        self.peek_opacity = 0.0;
        self.peek_start = None;
        self.force_full_damage_next = true;
    }

    pub(crate) fn set_overview_mode(
        &mut self,
        active: bool,
        windows: &[(u64, f32, f32, f32, f32, bool, String)],
    ) {
        let was_active = self.overview_active;
        if !self.overview_enabled {
            self.clear_overview_state_immediate();
            self.needs_render = true;
            return;
        }

        if active {
            if windows.is_empty() {
                self.clear_overview_state_immediate();
                self.needs_render = true;
                return;
            }
            let source_selected_index = windows
                .iter()
                .position(|(_, _, _, _, _, focused, _)| *focused)
                .unwrap_or(0);
            let source_range =
                super::overview::prism_entry_range(windows.len(), source_selected_index);
            self.overview_entries = windows[source_range]
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
            self.overview_titles_dirty = true;
            self.overview_selection = self
                .overview_entries
                .iter()
                .find(|entry| entry.focused)
                .or_else(|| self.overview_entries.first())
                .map(|entry| entry.window_id);
            let selected_index = self
                .overview_selection
                .and_then(|selected| {
                    self.overview_entries
                        .iter()
                        .position(|entry| entry.window_id == selected)
                })
                .unwrap_or(0);
            let target =
                super::overview::prism_target_rotation(self.overview_entries.len(), selected_index);
            self.overview_target_rotation = target;
            if !was_active {
                // The activation fade must start on the focused face. Selection
                // changes while active still animate through tick_overview_prism.
                self.overview_rotation = target;
            }
            self.overview_active = true;
        } else {
            // Keep the entries and selection alive until tick_overview reaches
            // zero opacity, otherwise the closing frame has nothing to draw.
            self.overview_active = false;
        }
        self.needs_render = true;
    }

    pub(crate) fn set_overview_selection(&mut self, window: u64) {
        if !self.overview_enabled || !self.overview_active {
            return;
        }
        self.overview_selection = Some(window);
        self.needs_render = true;
    }

    pub(crate) fn set_overview_monitor(&mut self, x: i32, y: i32, w: u32, h: u32) {
        if self.overview_monitor.2 != w {
            self.overview_titles_dirty = true;
        }
        self.overview_monitor = (x, y, w, h);
    }

    pub(crate) fn set_expose_mode(
        &mut self,
        active: bool,
        windows: Vec<(u64, i32, i32, u32, u32)>,
    ) {
        if !self.expose_enabled {
            self.clear_expose_state_immediate();
            self.needs_render = true;
            return;
        }

        if active {
            if windows.is_empty() {
                self.clear_expose_state_immediate();
                self.needs_render = true;
                return;
            }

            self.expose_entries = crate::backend::compositor_common::expose::build_expose_entries(
                self.screen_w as f32,
                self.screen_h as f32,
                self.expose_gap,
                &windows,
            );

            self.expose_active = true;
            self.expose_opacity = 0.0;
            self.expose_start = Some(std::time::Instant::now());
        } else {
            self.expose_active = false;
            self.expose_start = Some(std::time::Instant::now());
        }
        self.needs_render = true;
    }

    pub(crate) fn set_snap_preview(&mut self, preview: Option<(f32, f32, f32, f32)>) {
        if !self.snap_preview_enabled {
            self.clear_snap_preview_state_immediate();
            self.needs_render = true;
            return;
        }

        match preview {
            Some(rect) => {
                self.snap_preview = Some(rect);
                self.snap_preview_target_visible = true;
            }
            None => {
                // Retain the last rectangle until its configured fade-out has
                // completed; clearing it here would animate invisible frames.
                self.snap_preview_target_visible = false;
            }
        }
        self.needs_render = true;
    }

    pub(crate) fn clear_snap_preview_immediate(&mut self) {
        self.clear_snap_preview_state_immediate();
        self.needs_render = true;
    }

    pub(crate) fn set_peek_mode(&mut self, active: bool) {
        if !self.peek_enabled {
            self.clear_peek_state_immediate();
            self.needs_render = true;
            return;
        }
        if self.peek_active == active && self.peek_start.is_none() {
            return;
        }
        self.peek_active = active;
        self.peek_start = Some(std::time::Instant::now());
        self.needs_render = true;
    }

    pub(crate) fn set_dock_position(&mut self, x: f32, y: f32) {
        self.dock_x = x;
        self.dock_y = y;
    }

    fn retained_color_plan_mode(&self) -> (bool, bool) {
        self.retained_color_plan_context
            .as_ref()
            .map(|context| (context.render_path_enabled, context.scene_linear_active))
            .unwrap_or((false, false))
    }

    /// Invalidate one retained owner whose legacy transform was selected from
    /// a window-specific Dock/preview placement. Returns true when a restore
    /// was completed and the caller must not recreate minimized UI state.
    fn invalidate_retained_color_plan_for_window(&mut self, window_id: u64) -> bool {
        use crate::backend::compositor_common::genie::GenieDirection;

        let active_direction = self
            .genie_active
            .iter()
            .find(|animation| animation.window_id == window_id)
            .map(|animation| animation.direction);
        if self.pending_genie_restores.contains(&window_id)
            || active_direction == Some(GenieDirection::Restore)
        {
            self.complete_genie_restore_immediately(window_id);
            self.force_full_damage_next = true;
            self.needs_render = true;
            return true;
        }

        // A minimizing mesh, full-resolution cache, low-resolution snapshot,
        // or in-progress hidden import may all own the old output-bound plan.
        // Drop every tier before the new placement is allowed to rearm it.
        self.genie_active
            .retain(|animation| animation.window_id != window_id);
        self.minimized_visuals.remove(&window_id);
        self.pending_minimized_visuals.remove(&window_id);
        self.discard_minimized_snapshot(window_id);
        let removed_live = self.minimized_windows.contains(&window_id)
            && self
                .take_live_window_preserving_metadata(window_id)
                .is_some();

        if self.minimized_windows.contains(&window_id)
            && self.minimized_color_plan_geometry(window_id).is_some()
        {
            self.arm_minimized_snapshot_capture(window_id);
            self.arm_static_minimized_capture(window_id);
        }

        if let Some(preview) = self
            .dock_preview
            .as_mut()
            .filter(|preview| preview.window_id == window_id)
        {
            let now = Instant::now();
            preview.started = now;
            preview.start_opacity = 0.0;
            preview.start_scale = 0.86;
            preview.opacity = 0.0;
            preview.scale = 0.86;
            preview.awaiting_source = true;
        }
        if removed_live {
            self.refresh_any_color_transform_active();
        }
        self.force_full_damage_next = true;
        self.needs_render = true;
        false
    }

    pub(crate) fn set_window_dock_geometry(
        &mut self,
        window_id: u64,
        target: Option<crate::backend::api::CompositorRect>,
    ) {
        let target = target.and_then(crate::backend::api::CompositorRect::normalized);
        let previous_target = self.genie_targets.get(&window_id).copied();
        let (render_path_enabled, scene_linear_active) = self.retained_color_plan_mode();
        let invalidate_legacy_plan = legacy_retained_placement_changed(
            render_path_enabled,
            scene_linear_active,
            previous_target,
            target,
        );
        match target {
            Some(target) => {
                self.genie_targets.insert(window_id, target);
                if let Some(animation) = self
                    .genie_active
                    .iter_mut()
                    .find(|animation| animation.window_id == window_id)
                {
                    // Geometry can arrive after the minimize command. Updating
                    // the live animation lets the fallback endpoint converge
                    // smoothly onto the actual Dock slot on the next frame.
                    animation.target = target;
                }
                if let Some(visual) = self.minimized_visuals.get_mut(&window_id) {
                    visual.target = Some(target);
                }
                self.touch_minimized_visual(window_id, Instant::now());
                self.touch_minimized_snapshot(window_id);
            }
            None => {
                self.genie_targets.remove(&window_id);
                // A target can disappear (overflow, hidden/crashed bar, or
                // output migration) after a recapture was armed but before a
                // hidden surface commits a usable buffer. Do not spend GPU
                // memory importing an item that is no longer addressable; a
                // later non-empty geometry deterministically rearms it.
                self.pending_minimized_visuals.remove(&window_id);
                if let Some(animation) = self
                    .genie_active
                    .iter_mut()
                    .find(|animation| animation.window_id == window_id)
                {
                    animation.target = crate::backend::api::CompositorRect::new(
                        self.dock_x,
                        self.dock_y,
                        1.0,
                        1.0,
                    );
                }
                if let Some(visual) = self.minimized_visuals.get_mut(&window_id) {
                    visual.target = None;
                }
                if self
                    .dock_preview
                    .as_ref()
                    .is_some_and(|preview| preview.window_id == window_id)
                {
                    self.set_minimized_window_preview(None);
                }
            }
        }
        if invalidate_legacy_plan && self.invalidate_retained_color_plan_for_window(window_id) {
            return;
        }
        if self.minimized_windows.contains(&window_id)
            && self.minimized_color_plan_geometry(window_id).is_some()
        {
            self.arm_minimized_snapshot_capture(window_id);
        }
        self.arm_static_minimized_capture(window_id);
        self.force_full_damage_next = true;
        self.needs_render = true;
    }

    /// Reconcile an already-hidden JWM client without replaying a Genie from
    /// its off-screen parking geometry. The Dock target is the addressability
    /// gate; if no bar has published one yet, a later geometry update arms the
    /// one-shot hidden-surface import.
    pub(crate) fn ensure_minimized_window_visual(&mut self, window_id: u64) {
        self.minimized_windows.insert(window_id);
        self.arm_minimized_snapshot_capture(window_id);
        self.arm_static_minimized_capture(window_id);
        self.force_full_damage_next = true;
        self.needs_render = true;
    }

    pub(super) fn arm_static_minimized_capture(&mut self, window_id: u64) -> bool {
        if should_request_static_minimized_capture(
            self.genie_targets.contains_key(&window_id)
                || self
                    .dock_preview
                    .as_ref()
                    .is_some_and(|preview| preview.window_id == window_id),
            self.minimized_windows.contains(&window_id),
            self.minimized_visuals.contains_key(&window_id),
            self.genie_active
                .iter()
                .any(|animation| animation.window_id == window_id),
            self.pending_genie_restores.contains(&window_id),
            self.pending_minimized_visuals.contains(&window_id),
        ) {
            return self.pending_minimized_visuals.insert(window_id);
        }
        false
    }

    pub(super) fn resume_minimized_preview_after_capture(&mut self, window_id: u64) {
        let Some(preview) = self
            .dock_preview
            .as_mut()
            .filter(|preview| preview.window_id == window_id && preview.awaiting_source)
        else {
            return;
        };
        let now = Instant::now();
        preview.started = now;
        preview.lease_deadline = now + std::time::Duration::from_secs(4);
        preview.start_opacity = 0.0;
        preview.start_scale = 0.86;
        preview.direction = crate::backend::compositor_common::genie::PreviewDirection::Show;
        preview.opacity = 0.0;
        preview.scale = 0.86;
        preview.awaiting_source = false;
        self.force_full_damage_next = true;
        self.needs_render = true;
    }

    pub(crate) fn set_minimized_window_preview(
        &mut self,
        request: Option<(u64, crate::backend::api::CompositorRect)>,
    ) {
        use crate::backend::compositor_common::genie::{
            PreviewDirection, preview_motion, preview_request_reuses_timeline,
        };

        let request = request
            .and_then(|(window_id, anchor)| anchor.normalized().map(|anchor| (window_id, anchor)))
            .filter(|(window_id, _)| {
                self.minimized_visuals.contains_key(window_id)
                    || self
                        .genie_active
                        .iter()
                        .any(|animation| animation.window_id == *window_id)
                    || self.windows.contains_key(window_id)
                    || self.minimized_windows.contains(window_id)
            });
        let now = Instant::now();
        match request {
            Some((window_id, anchor)) => {
                let stable_dock_target = self.genie_targets.get(&window_id).copied();
                let preview_profile_compatible = self
                    .retained_color_plan_context
                    .as_ref()
                    .is_none_or(|context| match stable_dock_target {
                        Some(target)
                            if context.render_path_enabled && !context.scene_linear_active =>
                        {
                            retained_output_profiles_compatible(&context.outputs, target, anchor)
                        }
                        _ => true,
                    });
                if !preview_profile_compatible {
                    // The retained Dock pixels are still valid at their stable
                    // target. Refuse only the incompatible preview projection;
                    // never mutate or recapture the Dock owner's transform.
                    if self.dock_preview.take().is_some() {
                        self.force_full_damage_next = true;
                        self.needs_render = true;
                    }
                    return;
                }
                let previous_preview_anchor = self
                    .dock_preview
                    .as_ref()
                    .filter(|preview| preview.window_id == window_id)
                    .map(|preview| preview.anchor);
                let (render_path_enabled, scene_linear_active) = self.retained_color_plan_mode();
                let invalidate_legacy_plan = legacy_retained_preview_placement_changed(
                    render_path_enabled,
                    scene_linear_active,
                    stable_dock_target,
                    previous_preview_anchor,
                    Some(anchor),
                );
                self.touch_minimized_visual(window_id, now);
                self.touch_minimized_snapshot(window_id);
                if self.dock_preview.as_ref().is_some_and(|preview| {
                    preview_request_reuses_timeline(
                        preview.window_id == window_id,
                        preview.direction,
                    )
                }) {
                    let (awaiting_source, anchor_changed) = {
                        let preview = self
                            .dock_preview
                            .as_mut()
                            .expect("matching Dock preview disappeared");
                        let anchor_changed = preview.anchor != anchor;
                        preview.anchor = anchor;
                        preview.lease_deadline = now + std::time::Duration::from_secs(4);
                        (preview.awaiting_source, anchor_changed)
                    };
                    if invalidate_legacy_plan
                        && self.invalidate_retained_color_plan_for_window(window_id)
                    {
                        return;
                    }
                    if anchor_changed {
                        // Preserve the in-flight show animation; moving an
                        // existing common-linear preview need not fade it in
                        // again at every throttled Dock anchor update. Legacy
                        // output-bound pixels were concealed above instead.
                        self.force_full_damage_next = true;
                        self.needs_render = true;
                    }
                    if awaiting_source
                        || invalidate_legacy_plan
                        || !self.minimized_full_source_available(window_id)
                    {
                        self.arm_static_minimized_capture(window_id);
                    }
                    return;
                }
                if invalidate_legacy_plan
                    && self.invalidate_retained_color_plan_for_window(window_id)
                {
                    return;
                }
                let awaiting_source = !self.minimized_preview_source_available(window_id);
                self.dock_preview = Some(super::DockPreview {
                    window_id,
                    anchor,
                    started: now,
                    lease_deadline: now + std::time::Duration::from_secs(4),
                    start_opacity: 0.0,
                    start_scale: 0.86,
                    direction: PreviewDirection::Show,
                    opacity: 0.0,
                    scale: 0.86,
                    awaiting_source,
                });
                if awaiting_source || !self.minimized_full_source_available(window_id) {
                    self.arm_static_minimized_capture(window_id);
                }
            }
            None => {
                let Some(preview) = self.dock_preview.as_mut() else {
                    return;
                };
                let window_id = preview.window_id;
                if !self.genie_targets.contains_key(&window_id) {
                    // A hover upgrade can be pending even while a low-res
                    // source is already animating. Once the hover lease ends,
                    // target-less clients no longer justify a hidden import.
                    self.pending_minimized_visuals.remove(&window_id);
                }
                if preview.direction == PreviewDirection::Hide {
                    return;
                }
                if preview.awaiting_source {
                    self.dock_preview = None;
                    // A hover-only capture lease ended before pixels became
                    // available. Do not leave a hidden surface import armed
                    // forever when there is no persistent Dock target.
                    if !self.genie_targets.contains_key(&window_id) {
                        self.pending_minimized_visuals.remove(&window_id);
                    }
                    self.force_full_damage_next = true;
                    self.needs_render = true;
                    return;
                }
                let (opacity, scale, _) = preview_motion(
                    preview.start_opacity,
                    preview.start_scale,
                    preview.direction,
                    now.duration_since(preview.started).as_secs_f32(),
                );
                preview.started = now;
                preview.start_opacity = opacity;
                preview.start_scale = scale;
                preview.opacity = opacity;
                preview.scale = scale;
                preview.direction = PreviewDirection::Hide;
            }
        }
        self.force_full_damage_next = true;
        self.needs_render = true;
    }

    /// Queue the reverse Genie until the restored surface has a live texture
    /// and its final scene geometry. This avoids animating stale minimize-time
    /// pixels to a guessed layout rectangle.
    pub(crate) fn restore_window(&mut self, window_id: u64) {
        // The bounded Dock snapshot is deliberately not a restore source.
        // Retire its CPU/GPU tiers and generation before any reverse Genie
        // borrows the exact retained or freshly mapped texture.
        self.discard_minimized_snapshot(window_id);
        // Restore is newer intent than a still-waiting late minimize capture.
        // The live scene will provide the restored texture through its normal
        // import path, so never pull the hidden surface solely for the cache.
        self.pending_minimized_visuals.remove(&window_id);
        self.touch_minimized_visual(window_id, Instant::now());
        if !self.minimized_windows.contains(&window_id)
            && !self.minimized_visuals.contains_key(&window_id)
            && !self
                .genie_active
                .iter()
                .any(|animation| animation.window_id == window_id)
        {
            return;
        }

        if !self.genie_minimize_enabled {
            self.complete_genie_restore_immediately(window_id);
        } else {
            self.pending_genie_restores.insert(window_id);
        }
        self.force_full_damage_next = true;
        self.needs_render = true;
    }

    /// Finish a restore without leaving any animation-owned state behind.
    /// This is intentionally stronger than the normal animated completion:
    /// the preview disappears immediately so a runtime feature disable can
    /// make the window eligible for direct scanout on the very next frame.
    fn complete_genie_restore_immediately(&mut self, window_id: u64) {
        self.discard_minimized_snapshot(window_id);
        self.genie_active
            .retain(|animation| animation.window_id != window_id);
        let clear_preview = clear_immediate_restore_collections(
            window_id,
            &mut self.minimized_visuals,
            &mut self.minimized_windows,
            &mut self.pending_genie_restores,
            &mut self.genie_targets,
            self.dock_preview.as_ref().map(|preview| preview.window_id),
        );
        if clear_preview {
            self.dock_preview = None;
        }
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.fading_out = false;
            window.is_genie_minimizing = false;
            window.is_genie_restoring = false;
            window.fade_opacity = 1.0;
            window.anim_scale = 1.0;
            window.anim_scale_target = 1.0;
            window.closing_rect = None;
            window.ripple_active = false;
            window.ripple_progress = 0.0;
        }
        self.refresh_any_color_transform_active();
        self.force_full_damage_next = true;
        self.needs_render = true;
    }

    pub(crate) fn set_window_groups(
        &mut self,
        groups: Vec<crate::backend::compositor_common::window_tabs::TabGroup>,
    ) {
        if self.window_groups == groups {
            return;
        }
        self.window_groups = groups;
        // Re-derive the hovered cell against the new layout: the tab under
        // the pointer may sit at another index now, or be gone entirely.
        self.tab_hover = crate::backend::compositor_common::window_tabs::tab_hover_at(
            &self.window_groups,
            self.mouse_x,
            self.mouse_y,
        );
        // A group change is the only thing that can invalidate a title: the
        // text, the cell width and the focus flag all live in it.
        self.tab_titles_dirty = true;
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
        let geometry = self
            .prev_scene
            .iter()
            .find(|&&(id, _, _, _, _)| id == window)
            .map(|&(_, x, y, w, h)| (x, y, w, h));
        let (mouse_x, mouse_y) = (self.mouse_x, self.mouse_y);
        let wobbly_enabled = self.wobbly_enabled;
        let grid_size = self.wobbly_grid_size;

        if let Some(win) = self.windows.get_mut(&window) {
            win.is_moving = true;
            match geometry {
                Some((x, y, _, _)) => win.motion_trail.begin_drag(x as f32, y as f32),
                None => win.motion_trail.clear(),
            }
            if wobbly_enabled {
                let grid_n =
                    crate::backend::compositor_common::effects::wobbly_node_count(grid_size);
                let (anchor_row, anchor_col) = geometry
                    .map(|(x, y, w, h)| {
                        WobblyState::anchor_for_point(
                            grid_n,
                            mouse_x - x as f32,
                            mouse_y - y as f32,
                            w as f32,
                            h as f32,
                        )
                    })
                    .unwrap_or((0, grid_n / 2));
                let (width, height) = geometry
                    .map(|(_, _, w, h)| (w as f32, h as f32))
                    .unwrap_or((0.0, 0.0));
                win.wobbly = Some(WobblyState::new(
                    grid_n, anchor_row, anchor_col, width, height,
                ));
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
            win.motion_trail.end_drag();
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
            self.annotation_quads.clear();
            if !self.annotation_labels.is_empty() {
                self.annotation_labels.clear();
                self.annotation_labels_dirty = true;
            }
        }
        self.needs_render = true;
    }

    pub(crate) fn annotation_add_quad(
        &mut self,
        quad: crate::backend::compositor_common::annotation_overlay::AnnotationQuad,
    ) {
        if !self.annotation_active || !quad.is_drawable() {
            return;
        }
        self.annotation_quads.push(quad);
        self.needs_render = true;
    }

    pub(crate) fn annotation_add_text(
        &mut self,
        label: crate::backend::compositor_common::annotation_overlay::AnnotationLabel,
    ) {
        if !self.annotation_active || !label.is_drawable() {
            return;
        }
        self.annotation_labels.push(label);
        self.annotation_labels_dirty = true;
        self.needs_render = true;
    }

    /// Take the screenshot editor's toolbar, or withdraw it.
    pub(crate) fn set_screenshot_toolbar(
        &mut self,
        toolbar: Option<crate::backend::compositor_common::screenshot_toolbar::ScreenshotToolbar>,
    ) {
        if self.screenshot_toolbar == toolbar {
            return;
        }
        self.screenshot_toolbar = toolbar;
        self.screenshot_toolbar_dirty = true;
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

    /// Detach a resource-bearing live entry but retain its semantic state for
    /// a later static import or restore.
    pub(super) fn take_live_window_preserving_metadata(
        &mut self,
        window_id: u64,
    ) -> Option<WindowState> {
        crate::backend::compositor_common::genie::take_live_window_preserving_metadata(
            window_id,
            &mut self.windows,
            &mut self.minimized_window_metadata,
            |window| WindowVisualMetadata::from(window),
        )
    }

    /// Add a window to the compositor.
    #[allow(dead_code)]
    pub(crate) fn add_window(&mut self, window_id: u64) {
        let fading_enabled = self.fading_enabled;
        let window_animation_enabled = self.window_animation_enabled;
        let window_animation_scale = self.window_animation_scale;
        let window_animation_style = self.window_animation_style;
        let ripple_enabled = self.ripple_on_open_enabled;
        let initial_urgency = self.pending_window_urgency.take_for_new_window(window_id);
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
                motion_trail: Default::default(),
                opacity_override: None,
                corner_radius_override: None,
                frame_extents: [0; 4],
                is_shaped: false,
                is_fullscreen: false,
                is_urgent: initial_urgency,
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
                is_genie_restoring: false,
                color_transform: None,
            }
        });
        if inserted && let Some(win) = self.windows.get_mut(&window_id) {
            if let Some(metadata) = self.minimized_window_metadata.get(&window_id) {
                metadata.apply_to(win);
            }
            win.fade_opacity = if fading_enabled
                || (window_animation_enabled && window_animation_style.uses_fade())
            {
                0.0
            } else {
                1.0
            };
            win.anim_scale = if window_animation_enabled && window_animation_style.uses_scale() {
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
        self.pending_window_urgency.discard(window_id);
        self.minimized_window_metadata.remove(&window_id);
        self.discard_minimized_visual(window_id);
        if let Some(win) = self.windows.get_mut(&window_id) {
            // A close-fading WindowState can outlive its client. Clear the
            // flag now so an ID reused before fade retirement cannot inherit
            // stale attention state.
            win.is_urgent = false;
        }
        self.retire_window(window_id, WindowRetirement::Closed);
    }

    /// Retire a window after an explicit foreign-toplevel minimize request.
    ///
    /// This is the only retirement path allowed to start the genie effect.
    /// Strong `GlesTexture` handles keep either animation path safe after the
    /// live surface/offscreen cache releases its owner.
    pub(crate) fn minimize_window(&mut self, window_id: u64) {
        self.minimized_windows.insert(window_id);
        self.arm_minimized_snapshot_capture(window_id);
        if self.prepare_genie_minimize(window_id) {
            self.pending_minimized_visuals.remove(&window_id);
            return;
        }
        self.retire_window(window_id, WindowRetirement::ExplicitlyMinimized);
    }

    /// Whether the backend should import this minimized client's surface even
    /// though it is intentionally absent from the drawable JWM scene.
    pub(crate) fn has_pending_minimized_window_textures(&self) -> bool {
        !self.pending_minimized_visuals.is_empty()
    }

    pub(crate) fn minimized_window_texture_is_pending(&self, window_id: u64) -> bool {
        self.pending_minimized_visuals.contains(&window_id)
    }

    /// Logical placement used to plan a hidden minimized import. Its parked
    /// client geometry is deliberately off-screen and cannot select a legacy
    /// output profile; the Dock target (or active preview anchor) is the first
    /// reliable place where the retained pixels will actually be drawn.
    pub(crate) fn minimized_color_plan_geometry(
        &self,
        window_id: u64,
    ) -> Option<(i32, i32, u32, u32)> {
        self.genie_targets
            .get(&window_id)
            .copied()
            .or_else(|| {
                self.dock_preview
                    .as_ref()
                    .filter(|preview| preview.window_id == window_id)
                    .map(|preview| preview.anchor)
            })
            .and_then(retained_color_plan_geometry)
    }

    pub(crate) fn needs_minimized_window_texture(&self, window_id: u64) -> bool {
        self.pending_minimized_visuals.contains(&window_id)
            && !self
                .windows
                .get(&window_id)
                .is_some_and(|window| window.texture_owner.is_some())
    }

    fn retire_window(&mut self, window_id: u64, reason: WindowRetirement) {
        if !self.windows.contains_key(&window_id) {
            if reason == WindowRetirement::ExplicitlyMinimized
                && !self.minimized_visuals.contains_key(&window_id)
            {
                self.pending_minimized_visuals.insert(window_id);
                self.needs_render = true;
            } else {
                self.pending_minimized_visuals.remove(&window_id);
            }
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
            .is_some_and(|win| win.fading_out || win.is_genie_minimizing || win.is_genie_restoring)
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

        if reason == WindowRetirement::Closed
            && let Some((x, y, w, h)) = closing_scene_rect
        {
            self.spawn_particles_for_window(x, y, w, h);
        }

        let mut started_genie = false;
        if retirement_uses_genie(reason, self.genie_minimize_enabled) {
            if let Some((x, y, w, h)) = closing_rect {
                let target = self.genie_target_for(window_id);
                if let Some(win) = self.windows.get_mut(&window_id) {
                    if let Some(texture_owner) = win.texture_owner.clone() {
                        win.is_genie_minimizing = true;
                        win.closing_rect = Some((x, y, w, h));
                        let animation = super::GenieAnimation {
                            window_id,
                            start: Instant::now(),
                            start_progress: 0.0,
                            direction:
                                crate::backend::compositor_common::genie::GenieDirection::Minimize,
                            x,
                            y,
                            w,
                            h,
                            texture_owner,
                            has_alpha: win.has_alpha,
                            y_inverted: win.y_inverted,
                            content_uv: win.content_uv,
                            color_transform: win.color_transform,
                            target,
                        };
                        self.genie_active.push(animation);
                        self.pending_minimized_visuals.remove(&window_id);
                        started_genie = true;
                    }
                }
            }
        }
        if reason == WindowRetirement::ExplicitlyMinimized && !started_genie {
            // The Dock still needs real pixels when the mesh animation is
            // disabled. Detach a strong texture clone into the same bounded
            // cache and retire the live compositor state immediately.
            let mut cached_visual = false;
            if let (Some((x, y, w, h)), Some(win)) = (closing_rect, self.windows.get(&window_id))
                && let Some(texture_owner) = win.texture_owner.clone()
            {
                let animation = super::GenieAnimation {
                    window_id,
                    start: Instant::now(),
                    start_progress: 1.0,
                    direction: crate::backend::compositor_common::genie::GenieDirection::Minimize,
                    x,
                    y,
                    w,
                    h,
                    texture_owner,
                    has_alpha: win.has_alpha,
                    y_inverted: win.y_inverted,
                    content_uv: win.content_uv,
                    color_transform: win.color_transform,
                    target: self.genie_target_for(window_id),
                };
                self.cache_minimized_visual(animation);
                cached_visual = true;
            }
            if cached_visual {
                self.take_live_window_preserving_metadata(window_id);
                self.refresh_any_color_transform_active();
            } else {
                // No previous visible geometry and/or no imported buffer yet.
                // Keep any partial WindowState and let render_frame settle a
                // static retained visual as soon as the texture arrives.
                self.pending_minimized_visuals.insert(window_id);
            }
        } else if !started_genie {
            if let Some(win) = self.windows.get_mut(&window_id) {
                win.fading_out = true;
                win.closing_rect = closing_rect;
                if self.window_animation_enabled && self.window_animation_style.uses_scale() {
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
        let window_animation_style = self.window_animation_style;
        let ripple_enabled = self.ripple_on_open_enabled;
        let initial_urgency = self.pending_window_urgency.take_for_new_window(window_id);
        let restore_requested = self.pending_genie_restores.contains(&window_id)
            || self.genie_active.iter().any(|animation| {
                animation.window_id == window_id
                    && animation.direction
                        == crate::backend::compositor_common::genie::GenieDirection::Restore
            });
        let remains_minimized = self.minimized_windows.contains(&window_id) && !restore_requested;
        let preserved_metadata = self.minimized_window_metadata.get(&window_id).cloned();
        let was_retiring = self
            .windows
            .get(&window_id)
            .is_some_and(|win| win.fading_out || win.is_genie_minimizing);
        let cancel_stale_retirement = was_retiring && !remains_minimized && !restore_requested;
        if cancel_stale_retirement {
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
                motion_trail: Default::default(),
                opacity_override: None,
                corner_radius_override: None,
                frame_extents: [0; 4],
                is_shaped: false,
                is_fullscreen: false,
                is_urgent: initial_urgency,
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
                is_genie_restoring: false,
                color_transform: None,
            }
        });
        if inserted {
            if let Some(metadata) = preserved_metadata.as_ref() {
                metadata.apply_to(win);
            }
            win.fade_opacity = if restore_requested {
                1.0
            } else if fading_enabled
                || (window_animation_enabled && window_animation_style.uses_fade())
            {
                0.0
            } else {
                1.0
            };
            win.anim_scale = if restore_requested {
                1.0
            } else if window_animation_enabled && window_animation_style.uses_scale() {
                window_animation_scale
            } else {
                1.0
            };
            win.ripple_active = ripple_enabled && !restore_requested;
            win.ripple_progress = 0.0;
        } else if cancel_stale_retirement {
            win.fading_out = false;
            win.is_genie_minimizing = false;
            win.is_genie_restoring = false;
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
        if restore_requested {
            win.fading_out = false;
            win.is_genie_minimizing = false;
            win.is_genie_restoring = true;
            win.closing_rect = None;
            win.fade_opacity = 1.0;
            win.anim_scale = 1.0;
            win.anim_scale_target = 1.0;
            win.ripple_active = false;
            win.ripple_progress = 0.0;
        }
        if inserted || cancel_stale_retirement || restore_requested {
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
        if inserted
            && let Some(metadata) = preserved_metadata.as_ref()
            && !metadata.class_name.is_empty()
        {
            self.subpixel_mgr
                .register_window(window_id, &metadata.class_name);
        }
        if inserted && preserved_metadata.is_some() && !remains_minimized {
            // The semantic snapshot has reached a real live entry. Static
            // hidden adoption keeps it because that entry is immediately
            // retired back into the Dock cache.
            self.minimized_window_metadata.remove(&window_id);
        }
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
        // Fast path: bail before any rule lookups if neither the live entry nor
        // its detached minimized metadata needs an update.
        let live_changed = self
            .windows
            .get(&window_id)
            .is_some_and(|window| window.class_name != class_name);
        let metadata_changed = self
            .minimized_window_metadata
            .get(&window_id)
            .is_some_and(|metadata| metadata.class_name != class_name);
        if !live_changed && !metadata_changed {
            return;
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
        if let Some(metadata) = self.minimized_window_metadata.get_mut(&window_id) {
            metadata.class_name = class_name.to_string();
            metadata.is_frosted = frosted.is_some();
            metadata.frosted_strength = frosted.unwrap_or(0.0);
            metadata.opacity_override = opacity_override;
            metadata.corner_radius_override = corner_radius_override;
            if let Some(s) = scale {
                metadata.scale = s;
            }
        }
    }

    /// Set the per-window surface transform used by the window fragment
    /// shader. Scene-linear frames target common linear sRGB; the legacy
    /// encoded path may still target one overlapping output directly.
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
            self.refresh_any_color_transform_active();
        }
    }

    /// Advance the generation used by color plans copied into retained raw
    /// textures. Raw client pixels remain reusable, but an output-bound plan
    /// cannot cross a scene-linear/legacy or output-profile boundary.
    ///
    /// Fail closed: close fades disappear, restores complete onto their live
    /// surface, and minimized Genie/Dock owners are dropped until the existing
    /// hidden-import path captures them again with a freshly planned source.
    pub(crate) fn reconcile_retained_color_plan_context(
        &mut self,
        render_path_enabled: bool,
        scene_linear_active: bool,
        surface_description_generation: u64,
        outputs: Vec<RetainedOutputColorContext>,
    ) -> bool {
        let next = RetainedColorPlanContext {
            render_path_enabled,
            scene_linear_active,
            surface_description_generation,
            outputs,
        };
        let changed =
            retained_color_plan_context_changed(self.retained_color_plan_context.as_ref(), &next);
        self.retained_color_plan_context = Some(next);
        if !changed {
            return false;
        }

        // Never draw an old common/output-bound plan as identity in the new
        // domain. Surfaces which no longer exist cannot be replanned, so close
        // fades end immediately instead of turning HDR pixels into sRGB.
        self.windows.retain(|_, window| !window.fading_out);

        // Retargeting a retained mesh without its source description would be
        // equally unsafe. A minimize is re-captured from the still-mapped
        // hidden surface; a restore is already live and can complete now.
        let animations = std::mem::take(&mut self.genie_active);
        let mut minimizing = Vec::new();
        let mut restoring = self
            .pending_genie_restores
            .iter()
            .copied()
            .collect::<Vec<_>>();
        for animation in animations {
            match retained_color_generation_action(animation.direction) {
                RetainedColorGenerationAction::RecaptureMinimized => {
                    minimizing.push(animation.window_id);
                }
                RetainedColorGenerationAction::CompleteRestore => {
                    restoring.push(animation.window_id);
                }
            }
        }
        restoring.sort_unstable();
        restoring.dedup();
        minimizing.retain(|window_id| restoring.binary_search(window_id).is_err());
        for window_id in restoring {
            self.complete_genie_restore_immediately(window_id);
        }
        for window_id in minimizing {
            self.minimized_windows.insert(window_id);
        }

        // Full-resolution and bounded tiers both carry the old copied plan.
        // Drop them before arming the ordinary one-shot hidden import and
        // low-resolution snapshot generation.
        self.minimized_visuals.clear();
        self.pending_minimized_visuals.clear();
        let minimized = self.minimized_windows.iter().copied().collect::<Vec<_>>();
        let mut removed_live_window = false;
        for window_id in minimized.iter().copied() {
            self.discard_minimized_snapshot(window_id);
            removed_live_window |= self
                .take_live_window_preserving_metadata(window_id)
                .is_some();
            // A parked hidden-client rectangle is not a placement. Leave the
            // visual absent until a normalized Dock/preview target exists.
            if render_path_enabled && self.minimized_color_plan_geometry(window_id).is_some() {
                self.arm_minimized_snapshot_capture(window_id);
                self.arm_static_minimized_capture(window_id);
            }
        }
        if removed_live_window {
            self.refresh_any_color_transform_active();
        }

        if let Some(preview) = self.dock_preview.as_mut()
            && self.minimized_windows.contains(&preview.window_id)
        {
            let now = Instant::now();
            preview.started = now;
            preview.start_opacity = 0.0;
            preview.start_scale = 0.86;
            preview.opacity = 0.0;
            preview.scale = 0.86;
            preview.awaiting_source = true;
        }

        // Live windows are rebuilt immediately by the backend's current-frame
        // planner. Clearing here makes the method safe even if a caller aborts
        // before that rebuild.
        for window in self.windows.values_mut() {
            window.color_transform = None;
        }
        self.any_color_transform_active = false;
        self.force_full_damage_next = true;
        self.needs_render = true;
        true
    }

    /// Whether scene-linear rendering is backed by a live intermediate.
    /// Allocation and resize failures clear the request and FBO together, so
    /// diagnostics report the encoded fallback rather than configured intent.
    pub(crate) fn scene_linear_color_path_active(&self) -> bool {
        self.scene_linear_requested && self.linear_fbo != 0
    }

    /// Rebuild the fast-path flag after live WindowState retirement. Retained
    /// Genie/Dock textures have their own composition blockers, so this flag
    /// deliberately tracks live surfaces only.
    pub(super) fn refresh_any_color_transform_active(&mut self) {
        self.any_color_transform_active = self
            .windows
            .values()
            .any(|window| window.color_transform.is_some());
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
        // Solid-object modes need more time than a flat wipe to read, exactly
        // as on the X11 compositor.
        self.transition_duration = self.transition_mode.stretch_duration(duration);
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
        if apply_expose_terminal_cleanup(&mut self.expose_entries, result.clear_entries) {
            // Expose is a full-screen overlay rendered outside the ordinary
            // client damage boxes. Its terminal disappearance must repair the
            // whole output once, otherwise a partial frame can retain stale
            // thumbnails outside the current client damage.
            self.force_full_damage_next = true;
        }
        self.needs_render = true;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DisabledGenieAction, IME_POPUP_WINDOW_ID_PREFIX, PendingWindowUrgency,
        RetainedColorGenerationAction, RetainedColorPlanContext, RetainedOutputColorContext,
        WindowRetirement, XDG_POPUP_WINDOW_ID_PREFIX, apply_expose_terminal_cleanup,
        clear_immediate_restore_collections, collect_absent_auxiliary_window_ids,
        disabled_genie_action, is_auxiliary_window_id, legacy_retained_placement_changed,
        legacy_retained_preview_placement_changed, mouse_position_requires_render,
        postprocess_is_active, retained_color_generation_action,
        retained_color_plan_context_changed, retained_color_plan_geometry,
        retained_output_profiles_compatible, retirement_uses_genie,
        should_request_static_minimized_capture,
    };
    use crate::backend::compositor_common::genie::GenieDirection;
    use crate::backend::wayland_udev::color_pipeline::TransferKind;
    use std::collections::{HashMap, HashSet};

    fn retained_context(
        render_path_enabled: bool,
        scene_linear_active: bool,
        surface_description_generation: u64,
        rect: [i32; 4],
        output_tf: TransferKind,
    ) -> RetainedColorPlanContext {
        RetainedColorPlanContext {
            render_path_enabled,
            scene_linear_active,
            surface_description_generation,
            outputs: vec![RetainedOutputColorContext {
                rect,
                output_tf,
                working_to_output_row_major: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
            }],
        }
    }

    #[test]
    fn retained_color_generation_changes_only_after_an_established_context_moves() {
        let encoded = retained_context(true, false, 7, [0, 0, 1920, 1080], TransferKind::Srgb);
        assert!(!retained_color_plan_context_changed(None, &encoded));
        assert!(!retained_color_plan_context_changed(
            Some(&encoded),
            &encoded
        ));

        let linear = retained_context(true, true, 7, [0, 0, 1920, 1080], TransferKind::Srgb);
        assert!(retained_color_plan_context_changed(Some(&encoded), &linear));

        let pq = retained_context(true, true, 7, [0, 0, 1920, 1080], TransferKind::St2084Pq);
        assert!(retained_color_plan_context_changed(Some(&linear), &pq));

        let moved = retained_context(true, true, 7, [1920, 0, 1920, 1080], TransferKind::Srgb);
        assert!(retained_color_plan_context_changed(Some(&linear), &moved));

        let disabled = retained_context(false, true, 7, [0, 0, 1920, 1080], TransferKind::Srgb);
        assert!(retained_color_plan_context_changed(
            Some(&linear),
            &disabled
        ));

        let new_surface_description =
            retained_context(true, true, 8, [0, 0, 1920, 1080], TransferKind::Srgb);
        assert!(retained_color_plan_context_changed(
            Some(&linear),
            &new_surface_description
        ));
    }

    #[test]
    fn retained_color_generation_fails_closed_for_both_genie_directions() {
        assert_eq!(
            retained_color_generation_action(GenieDirection::Minimize),
            RetainedColorGenerationAction::RecaptureMinimized
        );
        assert_eq!(
            retained_color_generation_action(GenieDirection::Restore),
            RetainedColorGenerationAction::CompleteRestore
        );
    }

    #[test]
    fn retained_color_geometry_rejects_invalid_placement_and_normalizes_rounding() {
        assert_eq!(
            retained_color_plan_geometry(crate::backend::api::CompositorRect::new(
                10.4, 20.6, 199.6, 100.2,
            )),
            Some((10, 21, 200, 100))
        );
        assert_eq!(
            retained_color_plan_geometry(crate::backend::api::CompositorRect::new(
                f32::NAN,
                0.0,
                10.0,
                10.0,
            )),
            None
        );
        assert_eq!(
            retained_color_plan_geometry(crate::backend::api::CompositorRect::new(
                0.0, 0.0, 0.0, 10.0,
            )),
            None
        );
    }

    #[test]
    fn legacy_retained_target_move_invalidates_only_output_bound_plans() {
        let output_a = crate::backend::api::CompositorRect::new(100.0, 900.0, 80.0, 50.0);
        let output_b = crate::backend::api::CompositorRect::new(2100.0, 900.0, 80.0, 50.0);

        assert!(legacy_retained_placement_changed(
            true,
            false,
            Some(output_a),
            Some(output_b),
        ));
        assert!(!legacy_retained_placement_changed(
            true,
            true,
            Some(output_a),
            Some(output_b),
        ));
        assert!(!legacy_retained_placement_changed(
            false,
            false,
            Some(output_a),
            Some(output_b),
        ));
        assert!(!legacy_retained_placement_changed(
            true,
            false,
            Some(output_a),
            Some(output_a),
        ));
    }

    #[test]
    fn legacy_preview_move_uses_anchor_only_without_a_stable_dock_target() {
        let output_a = crate::backend::api::CompositorRect::new(100.0, 900.0, 80.0, 50.0);
        let output_b = crate::backend::api::CompositorRect::new(2100.0, 900.0, 80.0, 50.0);

        assert!(legacy_retained_preview_placement_changed(
            true,
            false,
            None,
            Some(output_a),
            Some(output_b),
        ));
        assert!(!legacy_retained_preview_placement_changed(
            true,
            false,
            Some(output_a),
            Some(output_a),
            Some(output_b),
        ));
    }

    #[test]
    fn retained_preview_reuses_a_stable_dock_plan_only_for_compatible_profiles() {
        let identity = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        let shifted = [0.9, 0.1, 0.0, 0.0, 1.0, 0.0, 0.0, 0.1, 0.9];
        let dock = crate::backend::api::CompositorRect::new(100.0, 100.0, 80.0, 50.0);
        let preview = crate::backend::api::CompositorRect::new(1100.0, 100.0, 80.0, 50.0);
        let outside = crate::backend::api::CompositorRect::new(2500.0, 100.0, 80.0, 50.0);
        let left = RetainedOutputColorContext {
            rect: [0, 0, 1000, 1000],
            output_tf: TransferKind::Srgb,
            working_to_output_row_major: identity,
        };
        let compatible_right = RetainedOutputColorContext {
            rect: [1000, 0, 1000, 1000],
            output_tf: TransferKind::Srgb,
            working_to_output_row_major: identity,
        };

        assert!(retained_output_profiles_compatible(
            &[left.clone(), compatible_right],
            dock,
            preview,
        ));

        let different_tf = RetainedOutputColorContext {
            rect: [1000, 0, 1000, 1000],
            output_tf: TransferKind::St2084Pq,
            working_to_output_row_major: identity,
        };
        assert!(!retained_output_profiles_compatible(
            &[left.clone(), different_tf],
            dock,
            preview,
        ));

        let different_matrix = RetainedOutputColorContext {
            rect: [1000, 0, 1000, 1000],
            output_tf: TransferKind::Srgb,
            working_to_output_row_major: shifted,
        };
        assert!(!retained_output_profiles_compatible(
            &[left.clone(), different_matrix],
            dock,
            preview,
        ));
        assert!(!retained_output_profiles_compatible(&[left], dock, outside,));
    }

    #[test]
    fn expose_terminal_cleanup_requests_one_full_repair() {
        let mut entries = vec![crate::backend::compositor_common::expose::ExposeEntry {
            id: 1_u64,
            orig_x: 0.0,
            orig_y: 0.0,
            orig_w: 100.0,
            orig_h: 100.0,
            target_x: 10.0,
            target_y: 10.0,
            target_w: 80.0,
            target_h: 80.0,
            current_x: 0.0,
            current_y: 0.0,
            current_w: 100.0,
            current_h: 100.0,
            is_hovered: false,
        }];
        assert!(!apply_expose_terminal_cleanup(&mut entries, false));
        assert_eq!(entries.len(), 1);
        assert!(apply_expose_terminal_cleanup(&mut entries, true));
        assert!(entries.is_empty());
    }

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
    fn pending_urgency_survives_until_first_window_state_only() {
        let mut pending = PendingWindowUrgency::default();

        pending.update(42, true);

        let first_window_is_urgent = pending.take_for_new_window(42);
        assert!(first_window_is_urgent);
        assert!(!pending.take_for_new_window(42));
    }

    #[test]
    fn pending_urgency_clear_or_destroy_prevents_stale_window_ids() {
        let mut pending = PendingWindowUrgency::default();

        pending.update(42, true);
        pending.update(42, false);
        assert!(!pending.take_for_new_window(42));

        pending.update(42, true);
        pending.update(7, true);
        pending.discard(42);
        assert!(!pending.take_for_new_window(42));
        assert!(pending.take_for_new_window(7));
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

    #[test]
    fn eviction_then_hover_arms_exactly_one_static_recapture() {
        assert!(should_request_static_minimized_capture(
            true, true, false, false, false, false
        ));
        assert!(!should_request_static_minimized_capture(
            true, true, false, false, false, true
        ));
        assert!(!should_request_static_minimized_capture(
            true, true, true, false, false, false
        ));
        assert!(!should_request_static_minimized_capture(
            true, true, false, true, false, false
        ));
        assert!(!should_request_static_minimized_capture(
            false, true, false, false, false, false
        ));
        assert!(!should_request_static_minimized_capture(
            true, false, false, false, false, false
        ));
        assert!(!should_request_static_minimized_capture(
            true, true, false, false, true, false
        ));
    }

    #[test]
    fn genie_hot_disable_completes_a_pending_only_restore() {
        assert_eq!(
            disabled_genie_action(None, true),
            Some(DisabledGenieAction::CompleteRestore)
        );
    }

    #[test]
    fn immediate_restore_cleanup_removes_only_the_restored_windows_dock_state() {
        let restored = 42;
        let untouched = 7;
        let mut visuals = HashMap::from([(restored, "restored"), (untouched, "untouched")]);
        let mut minimized = HashSet::from([restored, untouched]);
        let mut pending = HashSet::from([restored, untouched]);
        let mut targets = HashMap::from([(restored, 1), (untouched, 2)]);

        assert!(clear_immediate_restore_collections(
            restored,
            &mut visuals,
            &mut minimized,
            &mut pending,
            &mut targets,
            Some(restored),
        ));
        assert_eq!(visuals, HashMap::from([(untouched, "untouched")]));
        assert_eq!(minimized, HashSet::from([untouched]));
        assert_eq!(pending, HashSet::from([untouched]));
        assert_eq!(targets, HashMap::from([(untouched, 2)]));

        assert!(!clear_immediate_restore_collections(
            restored,
            &mut visuals,
            &mut minimized,
            &mut pending,
            &mut targets,
            Some(untouched),
        ));
    }

    #[test]
    fn genie_hot_disable_prioritizes_newer_restore_over_active_minimize() {
        assert_eq!(
            disabled_genie_action(Some(GenieDirection::Minimize), true),
            Some(DisabledGenieAction::CompleteRestore)
        );
        assert_eq!(
            disabled_genie_action(Some(GenieDirection::Minimize), false),
            Some(DisabledGenieAction::CacheMinimized)
        );
    }

    #[test]
    fn genie_hot_disable_completes_an_active_restore_without_pending_marker() {
        assert_eq!(
            disabled_genie_action(Some(GenieDirection::Restore), false),
            Some(DisabledGenieAction::CompleteRestore)
        );
        assert_eq!(disabled_genie_action(None, false), None);
    }
}
