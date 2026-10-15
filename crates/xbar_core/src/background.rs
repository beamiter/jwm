//! Shared bar-only background policy. Foreground/pill opacity is untouched.

/// Existing frosted material veil, shared by every presenter.
pub const FROSTED_BACKGROUND_OPACITY: f64 = 0.55;

/// Resolve only the full-width background. An opaque presentation target must
/// retain a readable solid fallback unless its caller supplies a wallpaper.
#[must_use]
pub fn bar_background_opacity(
    blur_status_bar: Option<bool>,
    explicit_opacity: Option<f64>,
    transparent_backdrop: bool,
) -> f64 {
    if !transparent_backdrop {
        return 1.0;
    }
    explicit_opacity
        .filter(|value| value.is_finite())
        .map(|value| value.clamp(0.0, 1.0))
        .unwrap_or(if blur_status_bar.unwrap_or(false) {
            FROSTED_BACKGROUND_OPACITY
        } else {
            0.0
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clear_is_default_frost_is_explicit_and_override_is_preserved() {
        assert_eq!(bar_background_opacity(None, None, true), 0.0);
        assert_eq!(bar_background_opacity(Some(false), None, true), 0.0);
        assert_eq!(bar_background_opacity(Some(true), None, true), 0.55);
        for mode in [None, Some(false), Some(true)] {
            assert_eq!(bar_background_opacity(mode, Some(0.3), true), 0.3);
            assert_eq!(bar_background_opacity(mode, Some(0.0), false), 1.0);
        }
    }
    #[test]
    fn malformed_override_cannot_poison_render_alpha() {
        assert_eq!(bar_background_opacity(None, Some(f64::NAN), true), 0.0);
        assert_eq!(bar_background_opacity(None, Some(2.0), true), 1.0);
        assert_eq!(bar_background_opacity(None, Some(-1.0), true), 0.0);
    }
}
