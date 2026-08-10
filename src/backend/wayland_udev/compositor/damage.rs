use super::*;
use crate::backend::compositor_common::attention::attention_signal_active;

fn inactive_window_styling_requires_composition(opacity: f32, dim: f32) -> bool {
    const EPSILON: f32 = 0.0001;
    (opacity - 1.0).abs() > EPSILON || (dim - 1.0).abs() > EPSILON
}

fn border_requires_composition(enabled: bool, width: f32) -> bool {
    enabled && width > 0.0001
}

fn attention_requires_composition(enabled: bool, has_urgent_window: bool) -> bool {
    attention_signal_active(enabled, has_urgent_window)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OverlayColorDomain {
    SceneLinearAware,
    EncodedOnly,
}

fn visible_overlay_blocks_color_pipeline(visible: bool, domain: OverlayColorDomain) -> bool {
    visible && domain == OverlayColorDomain::EncodedOnly
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

fn peek_animation_pending(active: bool, opacity: f32) -> bool {
    let target_opacity = if active { 1.0 } else { 0.0 };
    !opacity.is_finite() || (target_opacity - opacity).abs() > 0.0001
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

        // Genie and both minimized-Dock passes are intentionally absent here:
        // they use the shared window fragment shader with `u_scene_linear`
        // wired to the hardware-OETF state. The preview shadow is fixed black,
        // whose RGB value is identical in encoded and linear domains.
        let encoded_overlay_active = self.transition_active
            || self.snap_preview.is_some()
            || self.snap_preview_opacity > 0.0
            // Overview is intentionally absent: skydome, solid/reflection,
            // title and strip all bind their output color domain explicitly.
            || visible_overlay_blocks_color_pipeline(
                self.overview_active || self.overview_opacity > 0.0,
                OverlayColorDomain::SceneLinearAware,
            )
            || visible_overlay_blocks_color_pipeline(
                self.expose_active || !self.expose_entries.is_empty(),
                OverlayColorDomain::EncodedOnly,
            )
            || visible_overlay_blocks_color_pipeline(
                self.peek_active || self.peek_opacity > 0.0,
                OverlayColorDomain::EncodedOnly,
            )
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
            || self.screenshot_toolbar.is_some()
            || self.system_ui.is_some()
            || !self.toast_stack.is_empty()
            || !self.osd_slot.is_empty()
            || self.recording_region_overlay.is_some();

        !encoded_overlay_active
    }

    /// Return the compositor-owned visual that currently prevents KMS direct
    /// scanout.
    ///
    /// This deliberately checks *live state*, rather than merely checking
    /// whether an effect is enabled in the configuration.  A fullscreen
    /// surface may therefore return to direct scanout as soon as its fade,
    /// deformation, trail, or overlay has fully drained. Dock rectangles are
    /// the one output-local exception: their global physical geometry is
    /// intersected with `output_rect_global_physical`; effects without an
    /// output assignment remain conservatively global.
    pub(crate) fn direct_scanout_block_reason(
        &self,
        output_rect_global_physical: CompositorRect,
    ) -> Option<&'static str> {
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
        // Retained Dock targets and preview anchors already carry global
        // physical geometry, so unlike the active Genie animation they can be
        // scoped safely to the output that actually contains their pixels.
        if minimized_dock_requires_composition(
            self.genie_targets
                .iter()
                .filter_map(|(&window_id, &target)| {
                    (self.minimized_windows.contains(&window_id)
                        && self.minimized_static_drawable_source_available(window_id))
                    .then_some(target)
                }),
            self.dock_preview
                .as_ref()
                .filter(|preview| {
                    !preview.awaiting_source
                        && self.minimized_preview_drawable_source_available(preview.window_id)
                })
                .map(|preview| preview.anchor),
            output_rect_global_physical,
        ) {
            return Some("minimized Dock visual requires composition");
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
        if self.peek_active || self.peek_opacity > EPSILON {
            return Some("peek mode requires composition");
        }
        if !self.toast_stack.is_empty() {
            return Some("toast notifications require composition");
        }
        if !self.osd_slot.is_empty() {
            return Some("volume/brightness OSD requires composition");
        }
        if self.edge_glow_enabled && self.edge_glow_active && !self.edge_glow_suppressed {
            return Some("edge glow requires composition");
        }
        if attention_requires_composition(
            self.attention_animation_enabled,
            self.windows.values().any(|window| window.is_urgent),
        ) {
            return Some("urgent-window attention requires composition");
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
        if self.screenshot_toolbar.is_some() {
            return Some("the screenshot toolbar requires composition");
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
        if self.dock_preview.as_ref().is_some_and(|preview| {
            !preview.awaiting_source
                && (preview.direction
                    == crate::backend::compositor_common::genie::PreviewDirection::Hide
                    || preview.started.elapsed().as_secs_f32() < 0.22
                    || crate::backend::compositor_common::genie::preview_lease_timeout(
                        preview.direction,
                        std::time::Instant::now(),
                        preview.lease_deadline,
                    ) == Some(std::time::Duration::ZERO))
        }) {
            return true;
        }
        // Check transition
        if self.transition_active {
            return true;
        }
        // Toast cards fade on a wall-clock envelope; keep frames coming
        // while any card is visible (bounded by the toast timeout).
        if !self.toast_stack.is_empty() {
            return true;
        }
        // Same for the volume/brightness OSD card.
        if !self.osd_slot.is_empty() {
            return true;
        }
        // A rotating gradient border needs continuous frames while any
        // window that could carry a border is mapped.
        if self.border_gradient_enabled
            && self.border_gradient_speed != 0.0
            && self.border_enabled
            && self.border_width > 0.0
            && !self.windows.is_empty()
        {
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
        if peek_animation_pending(self.peek_active, self.peek_opacity) {
            return true;
        }
        // Snap preview animation
        if self.snap_preview_target_visible && self.snap_preview_opacity < 1.0 {
            return true;
        }
        if !self.snap_preview_target_visible && self.snap_preview_opacity > 0.0 {
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

fn compositor_rects_overlap(a: CompositorRect, b: CompositorRect) -> bool {
    let (Some(a), Some(b)) = (a.normalized(), b.normalized()) else {
        return false;
    };

    a.x < b.x + b.width && a.x + a.width > b.x && a.y < b.y + b.height && a.y + a.height > b.y
}

fn minimized_dock_requires_composition(
    targeted_cached_visuals: impl Iterator<Item = CompositorRect>,
    preview_anchor: Option<CompositorRect>,
    output_rect_global_physical: CompositorRect,
) -> bool {
    targeted_cached_visuals
        .chain(preview_anchor)
        .any(|rect| compositor_rects_overlap(rect, output_rect_global_physical))
}

#[cfg(test)]
mod tests {
    use super::{
        CompositorRect, OverlayColorDomain, attention_requires_composition,
        border_requires_composition, expose_animation_pending,
        inactive_window_styling_requires_composition, minimized_dock_requires_composition,
        overview_animation_pending, peek_animation_pending, rect_animation_pending,
        visible_overlay_blocks_color_pipeline,
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
    fn minimized_dock_target_only_blocks_the_output_it_overlaps() {
        let left = CompositorRect::new(0.0, 0.0, 1920.0, 1080.0);
        let right = CompositorRect::new(1920.0, 0.0, 2560.0, 1440.0);
        let target = CompositorRect::new(1810.0, 1010.0, 80.0, 50.0);

        assert!(minimized_dock_requires_composition(
            [target].into_iter(),
            None,
            left,
        ));
        assert!(!minimized_dock_requires_composition(
            [target].into_iter(),
            None,
            right,
        ));
        assert!(!minimized_dock_requires_composition(
            std::iter::empty(),
            None,
            left,
        ));
    }

    #[test]
    fn dock_preview_anchor_only_blocks_the_output_it_overlaps() {
        let left = CompositorRect::new(0.0, 0.0, 1920.0, 1080.0);
        let right = CompositorRect::new(1920.0, 0.0, 2560.0, 1440.0);
        let anchor = CompositorRect::new(120.0, 1020.0, 48.0, 40.0);

        assert!(minimized_dock_requires_composition(
            std::iter::empty(),
            Some(anchor),
            left,
        ));
        assert!(!minimized_dock_requires_composition(
            std::iter::empty(),
            Some(anchor),
            right,
        ));
    }

    #[test]
    fn dock_rect_crossing_an_output_edge_blocks_both_outputs() {
        let left = CompositorRect::new(0.0, 0.0, 1920.0, 1080.0);
        let right = CompositorRect::new(1920.0, 0.0, 2560.0, 1440.0);
        let crossing_target = CompositorRect::new(1900.0, 1000.0, 40.0, 60.0);

        assert!(minimized_dock_requires_composition(
            [crossing_target].into_iter(),
            None,
            left,
        ));
        assert!(minimized_dock_requires_composition(
            [crossing_target].into_iter(),
            None,
            right,
        ));
    }

    #[test]
    fn dock_rect_touching_an_output_edge_does_not_overlap_it() {
        let left = CompositorRect::new(0.0, 0.0, 1920.0, 1080.0);
        let right = CompositorRect::new(1920.0, 0.0, 2560.0, 1440.0);
        let left_only = CompositorRect::new(1880.0, 1000.0, 40.0, 60.0);

        assert!(minimized_dock_requires_composition(
            [left_only].into_iter(),
            None,
            left,
        ));
        assert!(!minimized_dock_requires_composition(
            [left_only].into_iter(),
            None,
            right,
        ));
    }

    #[test]
    fn urgent_attention_blocks_direct_scanout_even_without_ordinary_borders() {
        assert!(attention_requires_composition(true, true));
        assert!(!attention_requires_composition(false, true));
        assert!(!attention_requires_composition(true, false));
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
    fn scene_linear_overview_does_not_block_hardware_color_pipeline() {
        assert!(!visible_overlay_blocks_color_pipeline(
            true,
            OverlayColorDomain::SceneLinearAware,
        ));
        assert!(visible_overlay_blocks_color_pipeline(
            true,
            OverlayColorDomain::EncodedOnly,
        ));
        assert!(!visible_overlay_blocks_color_pipeline(
            false,
            OverlayColorDomain::EncodedOnly,
        ));
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

    #[test]
    fn peek_animation_liveness_covers_activation_and_release() {
        assert!(!peek_animation_pending(false, 0.0));
        assert!(!peek_animation_pending(true, 1.0));
        assert!(peek_animation_pending(true, 0.0));
        assert!(peek_animation_pending(false, 1.0));
        assert!(peek_animation_pending(false, f32::NAN));
    }
}
