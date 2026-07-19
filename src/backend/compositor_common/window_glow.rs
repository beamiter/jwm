//! Shared policy and sanitization for compositor-drawn client window glows.
//!
//! The renderer-specific code only owns GL setup. Matching, focus policy, safe
//! ranges, fade modulation, and damage reach stay identical across X11 and
//! Wayland.

use crate::backend::compositor_common::rules::contains_ignore_case;
use crate::config::BehaviorConfig;

pub(crate) const MAX_WINDOW_GLOW_RADIUS: f32 = 512.0;
pub(crate) const MAX_WINDOW_GLOW_INTENSITY: f32 = 4.0;
const WINDOW_GLOW_DAMAGE_PAD: i32 = 2;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct WindowGlowStyle {
    pub(crate) radius: f32,
    pub(crate) color: [f32; 4],
}

impl WindowGlowStyle {
    /// Stable words for backdrop-blur dependency hashes.
    pub(crate) fn hash_words(self) -> [u64; 3] {
        [
            u64::from(self.radius.to_bits()),
            (u64::from(self.color[0].to_bits()) << 32) | u64::from(self.color[1].to_bits()),
            (u64::from(self.color[2].to_bits()) << 32) | u64::from(self.color[3].to_bits()),
        ]
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct WindowGlowTarget<'a> {
    pub(crate) focused: bool,
    pub(crate) fullscreen: bool,
    pub(crate) override_redirect: bool,
    pub(crate) shaped: bool,
    pub(crate) class_name: &'a str,
    pub(crate) fade: f32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct WindowGlowSettings<'a> {
    enabled: bool,
    focused_only: bool,
    radius: f32,
    intensity: f32,
    color: [f32; 4],
    include: &'a [String],
    exclude: &'a [String],
}

impl<'a> WindowGlowSettings<'a> {
    pub(crate) fn from_behavior(behavior: &'a BehaviorConfig) -> Self {
        Self {
            enabled: behavior.border_glow_enabled,
            focused_only: behavior.border_glow_focused_only,
            radius: behavior.border_glow_radius,
            intensity: behavior.border_glow_intensity,
            color: behavior.border_glow_color,
            include: &behavior.border_glow_include,
            exclude: &behavior.border_glow_exclude,
        }
    }

    /// Conservative decoration reach used by partial-damage implementations.
    pub(crate) fn damage_margin(self) -> i32 {
        let radius = sanitize_radius(self.radius);
        let intensity = sanitize_intensity(self.intensity);
        let alpha = sanitize_unit(self.color[3]);
        if !self.enabled || radius <= 0.0 || intensity <= 0.0 || alpha <= 0.0 {
            return 0;
        }
        (radius.ceil() as i32).saturating_add(WINDOW_GLOW_DAMAGE_PAD)
    }

    /// Resolve one window's effective style, or suppress the pass entirely.
    pub(crate) fn style_for(self, target: WindowGlowTarget<'_>) -> Option<WindowGlowStyle> {
        // Fullscreen surfaces deliberately keep direct-scanout eligibility. A
        // shaped client needs an alpha-mask-aware glow, which this rounded-rect
        // SDF pass does not pretend to provide. Override-redirect surfaces are
        // client-owned popups/tooltips and likewise keep their own decoration.
        if !self.enabled
            || target.fullscreen
            || target.override_redirect
            || target.shaped
            || (self.focused_only && !target.focused)
            || !class_is_selected(target.class_name, self.include, self.exclude)
        {
            return None;
        }

        let radius = sanitize_radius(self.radius);
        let intensity = sanitize_intensity(self.intensity);
        let fade = sanitize_unit(target.fade);
        let mut color = self.color.map(sanitize_unit);
        color[3] = (color[3] * intensity * fade).clamp(0.0, 1.0);

        if radius <= 0.0 || color[3] <= 0.0 {
            return None;
        }

        Some(WindowGlowStyle { radius, color })
    }
}

fn sanitize_radius(value: f32) -> f32 {
    finite_clamp(value, 0.0, MAX_WINDOW_GLOW_RADIUS)
}

fn sanitize_intensity(value: f32) -> f32 {
    finite_clamp(value, 0.0, MAX_WINDOW_GLOW_INTENSITY)
}

fn sanitize_unit(value: f32) -> f32 {
    finite_clamp(value, 0.0, 1.0)
}

fn finite_clamp(value: f32, min: f32, max: f32) -> f32 {
    if value.is_finite() {
        value.clamp(min, max)
    } else {
        0.0
    }
}

fn class_is_selected(class_name: &str, include: &[String], exclude: &[String]) -> bool {
    let excluded = exclude.iter().any(|pattern| {
        let pattern = pattern.trim();
        !pattern.is_empty() && contains_ignore_case(class_name, pattern)
    });
    if excluded {
        return false;
    }

    let mut has_include_pattern = false;
    let included = include.iter().any(|pattern| {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            return false;
        }
        has_include_pattern = true;
        contains_ignore_case(class_name, pattern)
    });
    !has_include_pattern || included
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_settings<'a>(include: &'a [String], exclude: &'a [String]) -> WindowGlowSettings<'a> {
        WindowGlowSettings {
            enabled: true,
            focused_only: true,
            radius: 28.0,
            intensity: 1.0,
            color: [0.0, 0.55, 1.0, 0.38],
            include,
            exclude,
        }
    }

    fn target(class_name: &str) -> WindowGlowTarget<'_> {
        WindowGlowTarget {
            focused: true,
            fullscreen: false,
            override_redirect: false,
            shaped: false,
            class_name,
            fade: 1.0,
        }
    }

    #[test]
    fn focus_and_surface_policy_suppresses_unsafe_targets() {
        let settings = make_settings(&[], &[]);
        assert!(settings.style_for(target("JTerm4")).is_some());

        let mut unfocused = target("JTerm4");
        unfocused.focused = false;
        assert!(settings.style_for(unfocused).is_none());

        let mut fullscreen = target("JTerm4");
        fullscreen.fullscreen = true;
        assert!(settings.style_for(fullscreen).is_none());

        let mut popup = target("JTerm4");
        popup.override_redirect = true;
        assert!(settings.style_for(popup).is_none());

        let mut shaped = target("JTerm4");
        shaped.shaped = true;
        assert!(settings.style_for(shaped).is_none());
    }

    #[test]
    fn include_is_optional_and_exclude_takes_precedence() {
        let include = vec!["term".to_string()];
        let exclude = vec!["scratch".to_string()];
        let settings = make_settings(&include, &exclude);

        assert!(settings.style_for(target("JTerm4")).is_some());
        assert!(settings.style_for(target("Firefox")).is_none());
        assert!(settings.style_for(target("JTerm4 Scratch")).is_none());

        let empty: Vec<String> = Vec::new();
        assert!(
            make_settings(&empty, &empty)
                .style_for(target("Firefox"))
                .is_some()
        );
    }

    #[test]
    fn values_are_clamped_and_damage_covers_the_shader_quad() {
        let mut settings = make_settings(&[], &[]);
        settings.radius = 900.0;
        settings.intensity = 8.0;
        settings.color = [-1.0, 0.5, 2.0, 0.5];

        let style = settings.style_for(target("JTerm4")).unwrap();
        assert_eq!(style.radius, MAX_WINDOW_GLOW_RADIUS);
        assert_eq!(style.color, [0.0, 0.5, 1.0, 1.0]);
        assert_eq!(
            settings.damage_margin(),
            MAX_WINDOW_GLOW_RADIUS as i32 + WINDOW_GLOW_DAMAGE_PAD
        );
    }

    #[test]
    fn invalid_or_invisible_settings_do_not_draw() {
        let mut settings = make_settings(&[], &[]);
        settings.radius = f32::NAN;
        assert!(settings.style_for(target("JTerm4")).is_none());
        assert_eq!(settings.damage_margin(), 0);

        settings.radius = 28.0;
        settings.color[3] = 0.0;
        assert!(settings.style_for(target("JTerm4")).is_none());
        assert_eq!(settings.damage_margin(), 0);
    }

    #[test]
    fn hash_words_change_with_visible_style() {
        let settings = make_settings(&[], &[]);
        let first = settings.style_for(target("JTerm4")).unwrap();
        let mut faded = target("JTerm4");
        faded.fade = 0.5;
        let second = settings.style_for(faded).unwrap();
        assert_ne!(first.hash_words(), second.hash_words());
    }
}
