use super::*;

/// Simple damage tracking for the Wayland compositor.
/// Tracks whether a redraw is needed based on scene changes.
impl WaylandCompositor {
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
        }
        // Check transition
        if self.transition_active {
            return true;
        }
        // Check particles
        if !self.particle_systems.is_empty() {
            return true;
        }
        // Check overview animation
        if self.overview_active && self.overview_opacity < 1.0 {
            return true;
        }
        if !self.overview_active && self.overview_opacity > 0.0 {
            return true;
        }
        // Snap preview animation
        if self.snap_preview.is_some() && self.snap_preview_opacity < 1.0 {
            return true;
        }
        if self.snap_preview.is_none() && self.snap_preview_opacity > 0.0 {
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
