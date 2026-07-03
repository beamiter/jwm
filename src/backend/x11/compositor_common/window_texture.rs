//! Backend-independent window texture lifecycle state.

/// Explicit state machine for window texture lifecycle.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub(crate) enum WindowTextureState {
    /// Window just mapped, pixmap/surface backing is being created.
    Initializing,
    /// Normal operation, texture ready for rendering.
    Active {
        /// Whether the texture needs refresh from the backing surface.
        dirty: bool,
    },
    /// Geometry changed, backing texture needs recreation on next render.
    PendingRefresh,
    /// Window closing, fading out opacity.
    FadingOut {
        /// Current opacity (0.0 = fully transparent).
        opacity: f32,
    },
    /// Special animation.
    Animating {
        /// Animation type/context.
        kind: String,
    },
}

impl WindowTextureState {
    #[allow(dead_code)]
    pub(crate) fn is_renderable(&self) -> bool {
        matches!(
            self,
            WindowTextureState::Active { .. }
                | WindowTextureState::FadingOut { .. }
                | WindowTextureState::Animating { .. }
        )
    }

    #[allow(dead_code)]
    pub(crate) fn needs_tfp_refresh(&self) -> bool {
        matches!(self, WindowTextureState::Active { dirty: true })
    }

    #[allow(dead_code)]
    pub(crate) fn mark_dirty(&mut self) {
        if let WindowTextureState::Active { dirty } = self {
            *dirty = true;
        }
    }

    #[allow(dead_code)]
    pub(crate) fn mark_clean(&mut self) {
        if let WindowTextureState::Active { dirty } = self {
            *dirty = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::WindowTextureState;

    #[test]
    fn active_fading_and_animating_are_renderable() {
        assert!(WindowTextureState::Active { dirty: false }.is_renderable());
        assert!(WindowTextureState::FadingOut { opacity: 0.5 }.is_renderable());
        assert!(
            WindowTextureState::Animating {
                kind: "genie".to_string()
            }
            .is_renderable()
        );
    }

    #[test]
    fn initializing_and_pending_refresh_are_not_renderable() {
        assert!(!WindowTextureState::Initializing.is_renderable());
        assert!(!WindowTextureState::PendingRefresh.is_renderable());
    }

    #[test]
    fn dirty_tracking_only_applies_to_active_state() {
        let mut active = WindowTextureState::Active { dirty: false };
        assert!(!active.needs_tfp_refresh());
        active.mark_dirty();
        assert!(active.needs_tfp_refresh());
        active.mark_clean();
        assert!(!active.needs_tfp_refresh());

        let mut pending = WindowTextureState::PendingRefresh;
        pending.mark_dirty();
        assert!(!pending.needs_tfp_refresh());
    }
}
