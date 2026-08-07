//! Shared policy for the urgent-window attention border.
//!
//! Both compositors draw this signal with different GL APIs, but the configured
//! colour, pulse cadence, and minimum visible width are backend-independent.

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct AttentionBorderStyle {
    pub(crate) color: [f32; 4],
    pub(crate) width: f32,
}

#[inline]
pub(crate) fn attention_signal_active(animation_enabled: bool, urgent: bool) -> bool {
    animation_enabled && urgent
}

/// Resolve the border drawn for one urgent window at `elapsed_seconds`.
///
/// The four-radians-per-second cadence preserves JWM's established X11 pulse
/// (one cycle every roughly 1.57 seconds). An attention signal stays at least
/// two pixels wide even when ordinary borders are disabled. `opacity` lets a
/// backend preserve any window-level fade envelope around the shared pulse.
pub(crate) fn attention_border_style(
    mut color: [f32; 4],
    elapsed_seconds: f32,
    opacity: f32,
    ordinary_border_enabled: bool,
    ordinary_border_width: f32,
) -> AttentionBorderStyle {
    let pulse = (elapsed_seconds * 4.0).sin() * 0.5 + 0.5;
    color[3] *= pulse * opacity;
    let width = if ordinary_border_enabled {
        ordinary_border_width.max(2.0)
    } else {
        2.0
    };
    AttentionBorderStyle { color, width }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 1.0e-6,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn pulse_preserves_rgb_and_modulates_configured_alpha() {
        let configured = [0.2, 0.4, 0.6, 0.8];
        let start = attention_border_style(configured, 0.0, 1.0, true, 1.0);
        let peak = attention_border_style(configured, std::f32::consts::PI / 8.0, 1.0, true, 1.0);
        let trough =
            attention_border_style(configured, 3.0 * std::f32::consts::PI / 8.0, 1.0, true, 1.0);
        assert_eq!(&start.color[..3], &configured[..3]);
        assert_close(start.color[3], 0.4);
        assert_close(peak.color[3], 0.8);
        assert_close(trough.color[3], 0.0);
    }

    #[test]
    fn window_fade_scales_the_shared_attention_pulse() {
        let faded = attention_border_style(
            [0.2, 0.4, 0.6, 0.8],
            std::f32::consts::PI / 8.0,
            0.25,
            true,
            1.0,
        );

        assert_close(faded.color[3], 0.2);
    }

    #[test]
    fn disabled_or_non_urgent_windows_have_no_attention_signal() {
        assert!(attention_signal_active(true, true));
        assert!(!attention_signal_active(false, true));
        assert!(!attention_signal_active(true, false));
        assert!(!attention_signal_active(false, false));
    }

    #[test]
    fn attention_border_remains_visible_without_ordinary_borders() {
        assert_close(
            attention_border_style([1.0; 4], 0.0, 1.0, false, 20.0).width,
            2.0,
        );
        assert_close(
            attention_border_style([1.0; 4], 0.0, 1.0, true, 1.0).width,
            2.0,
        );
        assert_close(
            attention_border_style([1.0; 4], 0.0, 1.0, true, 3.5).width,
            3.5,
        );
    }
}
