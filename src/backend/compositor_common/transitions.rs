use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransitionMode {
    None,
    Slide,
    Cube,
    Fade,
    Flip,
    Zoom,
    Stack,
    Blinds,
    CoverFlow,
    Helix,
    Portal,
}

impl TransitionMode {
    /// Parse a configured transition name.
    ///
    /// Configuration is intentionally forgiving about surrounding whitespace
    /// and ASCII case. Unknown values keep the historical `slide` fallback.
    pub fn from_name(mode: &str) -> Self {
        match mode.trim().to_ascii_lowercase().as_str() {
            "none" => Self::None,
            "slide" => Self::Slide,
            "cube" => Self::Cube,
            "fade" => Self::Fade,
            "flip" => Self::Flip,
            "zoom" => Self::Zoom,
            "stack" => Self::Stack,
            "blinds" => Self::Blinds,
            "coverflow" => Self::CoverFlow,
            "helix" => Self::Helix,
            "portal" => Self::Portal,
            _ => Self::Slide,
        }
    }

    pub fn from_name_or_none(mode: &str) -> Self {
        if mode.trim().is_empty() {
            Self::None
        } else {
            Self::from_name(mode)
        }
    }

    pub const fn canonical_name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Slide => "slide",
            Self::Cube => "cube",
            Self::Fade => "fade",
            Self::Flip => "flip",
            Self::Zoom => "zoom",
            Self::Stack => "stack",
            Self::Blinds => "blinds",
            Self::CoverFlow => "coverflow",
            Self::Helix => "helix",
            Self::Portal => "portal",
        }
    }

    /// Whether the X11 renderer must preallocate a second scene target.
    ///
    /// Dedicated transition renderers now composite the old snapshot over the
    /// already-rendered destination workspace, so none of them require an
    /// additional monitor-sized texture before the transition starts.
    pub const fn needs_new_scene_fbo(self) -> bool {
        false
    }

    /// Apply a mode-appropriate, refresh-rate-independent easing curve.
    ///
    /// Motion-heavy effects use symmetric curves so their midpoint remains
    /// readable and the first/last frame never jumps.
    pub fn eased_progress(self, progress: f32) -> f32 {
        let t = finite_progress(progress);
        match self {
            Self::Slide | Self::Stack => smoothstep(t),
            Self::Fade | Self::Zoom | Self::Portal => smootherstep(t),
            Self::Cube | Self::Flip | Self::Blinds | Self::CoverFlow | Self::Helix => {
                ease_in_out_cubic(t)
            }
            Self::None => t,
        }
    }
}

/// Return normalized transition time while the animation is active.
///
/// A zero duration completes immediately, and clock anomalies are handled with
/// saturating arithmetic so a renderer never feeds NaN/Inf into shader state.
pub fn normalized_transition_progress(
    start: Instant,
    now: Instant,
    duration: Duration,
) -> Option<f32> {
    if duration.is_zero() {
        return None;
    }

    let elapsed = now.saturating_duration_since(start);
    if elapsed >= duration {
        return None;
    }

    Some(finite_progress(
        elapsed.as_secs_f32() / duration.as_secs_f32(),
    ))
}

#[inline]
fn finite_progress(progress: f32) -> f32 {
    if progress.is_finite() {
        progress.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

#[inline]
fn smoothstep(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

#[inline]
fn ease_in_out_cubic(t: f32) -> f32 {
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        let inv = -2.0 * t + 2.0;
        1.0 - inv * inv * inv * 0.5
    }
}

#[inline]
fn smootherstep(t: f32) -> f32 {
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODES: [TransitionMode; 11] = [
        TransitionMode::None,
        TransitionMode::Slide,
        TransitionMode::Cube,
        TransitionMode::Fade,
        TransitionMode::Flip,
        TransitionMode::Zoom,
        TransitionMode::Stack,
        TransitionMode::Blinds,
        TransitionMode::CoverFlow,
        TransitionMode::Helix,
        TransitionMode::Portal,
    ];

    #[test]
    fn transition_mode_from_name_defaults_to_slide() {
        assert_eq!(TransitionMode::from_name("cube"), TransitionMode::Cube);
        assert_eq!(
            TransitionMode::from_name(" Portal "),
            TransitionMode::Portal
        );
        assert_eq!(TransitionMode::from_name("NONE"), TransitionMode::None);
        assert_eq!(TransitionMode::from_name("unknown"), TransitionMode::Slide);
        assert_eq!(
            TransitionMode::from_name_or_none("unknown"),
            TransitionMode::Slide
        );
    }

    #[test]
    fn explicit_none_disables_transitions() {
        assert_eq!(
            TransitionMode::from_name_or_none("none"),
            TransitionMode::None
        );
        assert_eq!(
            TransitionMode::from_name_or_none("  "),
            TransitionMode::None
        );
    }

    #[test]
    fn canonical_names_round_trip() {
        for mode in MODES {
            assert_eq!(TransitionMode::from_name(mode.canonical_name()), mode);
        }
    }

    #[test]
    fn transition_renderers_do_not_preallocate_a_destination_scene() {
        for mode in MODES {
            assert!(!mode.needs_new_scene_fbo(), "{}", mode.canonical_name());
        }
    }

    #[test]
    fn normalized_progress_rejects_zero_and_completed_durations() {
        let start = Instant::now();
        assert_eq!(
            normalized_transition_progress(start, start, Duration::ZERO),
            None
        );
        assert_eq!(
            normalized_transition_progress(
                start,
                start + Duration::from_millis(100),
                Duration::from_millis(100)
            ),
            None
        );
        assert_eq!(
            normalized_transition_progress(start, start, Duration::from_millis(100)),
            Some(0.0)
        );
        assert_eq!(
            normalized_transition_progress(
                start,
                start.checked_sub(Duration::from_millis(1)).unwrap_or(start),
                Duration::from_millis(100),
            ),
            Some(0.0)
        );
        let halfway = normalized_transition_progress(
            start,
            start + Duration::from_millis(50),
            Duration::from_millis(100),
        )
        .expect("active transition");
        assert!((halfway - 0.5).abs() < 1.0e-6);
    }

    #[test]
    fn easing_curves_are_bounded_monotonic_and_hit_endpoints() {
        for mode in MODES {
            assert_eq!(mode.eased_progress(0.0), 0.0);
            assert_eq!(mode.eased_progress(1.0), 1.0);
            assert_eq!(mode.eased_progress(f32::NAN), 0.0);

            let mut previous = 0.0;
            for step in 0..=100 {
                let value = mode.eased_progress(step as f32 / 100.0);
                assert!((0.0..=1.0).contains(&value));
                assert!(value + 1.0e-6 >= previous);
                previous = value;
            }
        }
    }
}
