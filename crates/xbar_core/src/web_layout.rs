//! One orientation and accessibility adapter for every HTML bar frontend.
//!
//! The host installs this after frontend readiness (and again after a page
//! reload). Frameworks retain ownership of their state and actions; this
//! adapter only supplies layout, focus navigation, and readable short labels.

use crate::presentation::PresentationConfig;

pub const STYLESHEET: &str = include_str!("web_layout.css");
const INSTALLER: &str = include_str!("web_layout.js");

/// An idempotent installer. Only a closed enum and a validated number enter
/// JavaScript source; configuration strings and model text are never eval'd.
#[must_use]
pub fn install_script(config: &PresentationConfig, scale_factor: f64) -> String {
    let thickness = config.logical_thickness(scale_factor);
    let thickness = if thickness.is_finite() {
        thickness.clamp(1.0, 65535.0)
    } else {
        42.0
    };
    let policy = config.scroll.valid();
    let adjustment = policy.adjustment_pixels;
    let idle_ms = policy.idle.as_millis().clamp(1, 10_000);
    format!(
        "(function() {{ const edge = '{}'; const thickness = {thickness}; const configuredScroll = {{ adjustment_pixels: {adjustment}, idle_ms: {idle_ms} }};\n{INSTALLER}\n}})()",
        config.edge.as_str()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::placement::BarEdge;

    #[test]
    fn host_installer_preserves_physical_thickness_at_fractional_scale() {
        let config = PresentationConfig {
            edge: BarEdge::Right,
            physical_thickness: Some(60),
            ..PresentationConfig::default()
        };
        let script = install_script(&config, 1.5);
        assert!(script.contains("const edge = 'right'"));
        assert!(script.contains("const thickness = 40;"));
        assert!(script.contains("previous.dispose()"));
    }

    #[test]
    fn host_installer_transmits_the_same_adjustment_policy_to_html_bars() {
        let mut config = PresentationConfig::default();
        config.scroll.adjustment_pixels = 24.0;
        config.scroll.idle = std::time::Duration::from_millis(400);
        let script = install_script(&config, 2.0);
        assert!(script.contains("adjustment_pixels: 24, idle_ms: 400"));
        // Programmatic/serialized configuration gets the same finite fallback
        // as native input and cannot inject invalid JavaScript numbers.
        config.scroll.adjustment_pixels = f64::NAN;
        assert!(install_script(&config, 1.0).contains("adjustment_pixels: 16"));
    }
}
