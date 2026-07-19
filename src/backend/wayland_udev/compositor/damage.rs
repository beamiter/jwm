use super::*;

fn inactive_window_styling_requires_composition(opacity: f32, dim: f32) -> bool {
    const EPSILON: f32 = 0.0001;
    (opacity - 1.0).abs() > EPSILON || (dim - 1.0).abs() > EPSILON
}

fn border_requires_composition(enabled: bool, width: f32) -> bool {
    enabled && width > 0.0001
}

fn rect_animation_pending(current: [f32; 4], target: [f32; 4]) -> bool {
    current.into_iter().zip(target).any(|(current, target)| {
        !current.is_finite() || !target.is_finite() || (target - current).abs() > f32::EPSILON
    })
}

fn expose_animation_pending(active: bool, opacity: f32, entry_geometry_pending: bool) -> bool {
    let target_opacity = if active { 1.0 } else { 0.0 };
    !opacity.is_finite() || (target_opacity - opacity).abs() > 0.0001 || entry_geometry_pending
}

fn angular_distance(current: f32, target: f32) -> f32 {
    let mut difference = target - current;
    while difference > std::f32::consts::PI {
        difference -= std::f32::consts::TAU;
    }
    while difference < -std::f32::consts::PI {
        difference += std::f32::consts::TAU;
    }
    difference.abs()
}

fn overview_animation_pending(active: bool, opacity: f32, rotation: f32, target: f32) -> bool {
    let target_opacity = if active { 1.0 } else { 0.0 };
    !opacity.is_finite()
        || (target_opacity - opacity).abs() > 0.0001
        || (active
            && (!rotation.is_finite()
                || !target.is_finite()
                || angular_distance(rotation, target) > 0.001))
}

/// Simple damage tracking for the Wayland compositor.
/// Tracks whether a redraw is needed based on scene changes.
impl WaylandCompositor {
    /// Hardware OETF/CTM offload is safe only while every pass written after
    /// the linear window scene is itself linear-aware. Encoded-space overlays
    /// would otherwise be encoded a second time by the CRTC LUT.
    pub(crate) fn kms_color_pipeline_offload_safe(&self) -> bool {
        if !self.scene_linear_requested || self.linear_fbo == 0 {
            return false;
        }

        let encoded_overlay_active = self.transition_active
            || self.snap_preview.is_some()
            || self.snap_preview_opacity > 0.0
            || self.overview_active
            || self.overview_opacity > 0.0
            || self.expose_active
            || !self.expose_entries.is_empty()
            || self.peek_active
            || (self.window_tabs_enabled && !self.window_groups.is_empty())
            || !self.particle_systems.is_empty()
            || (self.edge_glow_enabled
                && self.edge_glow_width > 0.0
                && self.edge_glow_active
                && !self.edge_glow_suppressed)
            || self.postprocess_active
            || self.debug_hud_enabled
            || self.debug_hud_extended
            || (self.annotation_active && !self.annotation_strokes.is_empty())
            || self.system_ui.is_some()
            || self.recording_region_overlay.is_some();

        !encoded_overlay_active
    }

    /// Return the compositor-owned visual that currently prevents KMS direct
    /// scanout.
    ///
    /// This deliberately checks *live state*, rather than merely checking
    /// whether an effect is enabled in the configuration.  A fullscreen
    /// surface may therefore return to direct scanout as soon as its fade,
    /// deformation, trail, or overlay has fully drained.
    pub(crate) fn direct_scanout_block_reason(&self) -> Option<&'static str> {
        const EPSILON: f32 = 0.0001;

        if self.postprocess_active {
            return Some("post-processing requires composition");
        }
        if self.any_color_transform_active {
            return Some("surface color transform requires composition");
        }
        if (self.active_opacity - 1.0).abs() > EPSILON
            || (self.inactive_opacity - 1.0).abs() > EPSILON
            || self.windows.values().any(|win| {
                win.opacity_override
                    .or_else(|| self.lookup_opacity_rule(&win.class_name))
                    .is_some_and(|opacity| (opacity - 1.0).abs() > EPSILON)
            })
        {
            return Some("window opacity requires composition");
        }
        if inactive_window_styling_requires_composition(1.0, self.inactive_dim) {
            return Some("inactive window dimming requires composition");
        }
        if self.blur_enabled && self.windows.values().any(|win| win.is_frosted) {
            return Some("window blur requires composition");
        }
        if self.transition_active {
            return Some("workspace transition requires composition");
        }

        if self
            .windows
            .values()
            .any(|win| win.fading_out || (win.fade_opacity - 1.0).abs() > EPSILON)
        {
            return Some("window fade requires composition");
        }
        if self.windows.values().any(|win| {
            (win.anim_scale - 1.0).abs() > EPSILON || (win.anim_scale_target - 1.0).abs() > EPSILON
        }) {
            return Some("window scale animation requires composition");
        }
        if self.windows.values().any(|win| win.wobbly.is_some()) {
            return Some("wobbly window deformation requires composition");
        }
        if self.windows.values().any(|win| win.ripple_active) {
            return Some("window ripple requires composition");
        }
        if self
            .windows
            .values()
            .any(|win| !win.motion_trail.is_empty())
        {
            return Some("window motion trail requires composition");
        }
        if !self.genie_active.is_empty() {
            return Some("genie minimize requires composition");
        }
        if !self.particle_systems.is_empty() {
            return Some("particle effects require composition");
        }
        if self.tilt_x.abs() > EPSILON
            || self.tilt_y.abs() > EPSILON
            || self.tilt_target_x.abs() > EPSILON
            || self.tilt_target_y.abs() > EPSILON
        {
            return Some("window tilt requires composition");
        }
        if self.window_tabs_enabled && !self.window_groups.is_empty() {
            return Some("window tabs require composition");
        }

        // Other compositor-owned overlays must follow the same rule.  Some of
        // these retain their draw state briefly after being deactivated while
        // their closing animation drains.
        if self.overview_active || self.overview_opacity > EPSILON {
            return Some("overview requires composition");
        }
        if self.expose_active || self.expose_opacity > EPSILON || !self.expose_entries.is_empty() {
            return Some("expose view requires composition");
        }
        if self.snap_preview.is_some() || self.snap_preview_opacity > EPSILON {
            return Some("snap preview requires composition");
        }
        if self.peek_active {
            return Some("peek mode requires composition");
        }
        if self.edge_glow_enabled && self.edge_glow_active && !self.edge_glow_suppressed {
            return Some("edge glow requires composition");
        }
        if border_requires_composition(self.border_enabled, self.border_width) {
            return Some("window borders require composition");
        }
        if self.border_enabled
            && self.focus_highlight_enabled
            && self.focus_highlight_start.is_some_and(|(_, start)| {
                start.elapsed().as_millis() < self.focus_highlight_duration_ms as u128
            })
        {
            return Some("focus highlight requires composition");
        }
        if self.annotation_active && !self.annotation_strokes.is_empty() {
            return Some("annotations require composition");
        }
        if self.debug_hud_enabled || self.debug_hud_extended {
            return Some("debug HUD requires composition");
        }
        if self.wallpaper_transition_start.is_some() {
            return Some("wallpaper transition requires composition");
        }
        if self.zoom_to_fit_window.is_some() {
            return Some("zoom-to-fit requires composition");
        }
        None
    }

    /// Check if any animations are still running (requiring continuous redraws)
    pub(crate) fn has_active_animations(&self) -> bool {
        // Check fade animations
        for win in self.windows.values() {
            if win.fading_out && win.fade_opacity > 0.0 {
                return true;
            }
            if !win.fading_out && win.fade_opacity < 1.0 {
                return true;
            }
            if win.anim_scale != win.anim_scale_target {
                return true;
            }
            if win.wobbly.is_some() {
                return true;
            }
            if win.ripple_active {
                return true;
            }
            if !win.motion_trail.is_empty() {
                return true;
            }
        }
        if self.window_tilt_enabled
            && ((self.tilt_x - self.tilt_target_x).abs() > 0.0001
                || (self.tilt_y - self.tilt_target_y).abs() > 0.0001)
        {
            return true;
        }
        if !self.genie_active.is_empty() {
            return true;
        }
        // Check transition
        if self.transition_active {
            return true;
        }
        // Check particles
        if !self.particle_systems.is_empty() {
            return true;
        }
        // A visible overview/expose overlay is not inherently an animation.
        // Once opacity, rotation and layout converge, content damage is enough
        // to request another frame.
        if overview_animation_pending(
            self.overview_active,
            self.overview_opacity,
            self.overview_rotation,
            self.overview_target_rotation,
        ) {
            return true;
        }
        let expose_geometry_pending = self.expose_entries.iter().any(|entry| {
            let target = if self.expose_active {
                [
                    entry.target_x,
                    entry.target_y,
                    entry.target_w,
                    entry.target_h,
                ]
            } else {
                [entry.orig_x, entry.orig_y, entry.orig_w, entry.orig_h]
            };
            rect_animation_pending(
                [
                    entry.current_x,
                    entry.current_y,
                    entry.current_w,
                    entry.current_h,
                ],
                target,
            )
        });
        if expose_animation_pending(
            self.expose_active,
            self.expose_opacity,
            expose_geometry_pending,
        ) {
            return true;
        }
        // Snap preview animation
        if self.snap_preview.is_some() && self.snap_preview_opacity < 1.0 {
            return true;
        }
        if self.snap_preview.is_none() && self.snap_preview_opacity > 0.0 {
            return true;
        }
        // Wallpaper crossfade in progress
        if self.wallpaper_transition_start.is_some() {
            return true;
        }
        // Pending wallpaper loads need polling
        if self.pending_wallpaper.is_some() {
            return true;
        }
        if !self.pending_monitor_wallpapers.is_empty() {
            return true;
        }
        false
    }

    /// Mark as needing render if there are active animations
    #[allow(dead_code)]
    pub(crate) fn schedule_animation_frame(&mut self) {
        if self.has_active_animations() {
            self.needs_render = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        border_requires_composition, expose_animation_pending,
        inactive_window_styling_requires_composition, overview_animation_pending,
        rect_animation_pending,
    };

    #[test]
    fn inactive_window_styling_blocks_composition_bypass() {
        assert!(!inactive_window_styling_requires_composition(1.0, 1.0));
        assert!(inactive_window_styling_requires_composition(0.9, 1.0));
        assert!(inactive_window_styling_requires_composition(1.0, 0.8));
    }

    #[test]
    fn ordinary_visible_border_blocks_direct_scanout() {
        assert!(border_requires_composition(true, 1.0));
        assert!(!border_requires_composition(false, 1.0));
        assert!(!border_requires_composition(true, 0.0));
        assert!(border_requires_composition(true, f32::INFINITY));
    }

    #[test]
    fn settled_overview_does_not_request_continuous_frames() {
        assert!(!overview_animation_pending(true, 1.0, 0.0, 0.0));
        assert!(!overview_animation_pending(
            true,
            1.0,
            0.0,
            std::f32::consts::TAU,
        ));
        assert!(overview_animation_pending(true, 0.9, 0.0, 0.0));
        assert!(overview_animation_pending(true, 1.0, 0.0, 0.2));
        assert!(overview_animation_pending(false, 0.2, 0.0, 0.0));
    }

    #[test]
    fn settled_expose_does_not_request_continuous_frames() {
        assert!(!expose_animation_pending(true, 1.0, false));
        assert!(expose_animation_pending(true, 0.9, false));
        assert!(expose_animation_pending(true, 1.0, true));
        assert!(expose_animation_pending(false, 0.2, false));
        assert!(!expose_animation_pending(false, 0.0, false));

        assert!(!rect_animation_pending(
            [10.0, 20.0, 300.0, 200.0],
            [10.0, 20.0, 300.0, 200.0],
        ));
        assert!(rect_animation_pending(
            [10.0, 20.0, 300.0, 200.0],
            [11.0, 20.0, 300.0, 200.0],
        ));
        assert!(rect_animation_pending(
            [10.0, 20.0, 300.0, 200.0],
            [10.25, 20.0, 300.0, 200.0],
        ));
    }
}
