//! Window open/close animation styles shared by the X11 and Wayland
//! compositors.
//!
//! Both backends keep the historical per-window carriers (`anim_scale` for the
//! zoom, `fade_opacity` for alpha) and only differ in how far a frame has
//! progressed. This module owns the mapping from that normalized progress to
//! the concrete render transform so the two renderers cannot drift apart.

/// Vertical travel, in physical pixels, for the `slide` style: a window opens
/// from this far below its rest position and slides back down to it on close.
pub const SLIDE_OFFSET_PX: f32 = 24.0;

/// Style of the window open/close animation (`window_animation_style`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowAnimationStyle {
    /// Zoom from `window_animation_scale` to full size (historical behavior).
    Scale,
    /// Pure alpha fade, no geometry change.
    Fade,
    /// Alpha fade combined with a short vertical slide.
    Slide,
}

impl WindowAnimationStyle {
    /// Parse a configured style name.
    ///
    /// Configuration is intentionally forgiving about surrounding whitespace
    /// and ASCII case, mirroring `TransitionMode::from_name`. Unknown values
    /// fall back to `Scale`, the historical behavior.
    pub fn from_name(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "fade" => Self::Fade,
            "slide" => Self::Slide,
            _ => Self::Scale,
        }
    }

    pub const fn canonical_name(self) -> &'static str {
        match self {
            Self::Scale => "scale",
            Self::Fade => "fade",
            Self::Slide => "slide",
        }
    }

    /// Whether the style animates window alpha through the fade carrier
    /// (`fade_opacity`). Such styles need the open/close fade machinery even
    /// when the standalone `fading` feature is disabled.
    pub const fn uses_fade(self) -> bool {
        matches!(self, Self::Fade | Self::Slide)
    }

    /// Whether the style animates the scale carrier (`anim_scale`).
    pub const fn uses_scale(self) -> bool {
        matches!(self, Self::Scale)
    }
}

/// Per-frame render transform produced by a window open/close animation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowAnimationFrame {
    /// Scale factor applied around the window center (1.0 = unscaled).
    pub scale: f32,
    /// Alpha multiplier contributed by the animation (1.0 = fully opaque).
    ///
    /// Both backends route animation alpha through the existing
    /// `fade_opacity` carrier, which every opacity/shadow/glow computation
    /// already multiplies in; renderers must not apply this a second time.
    /// It is reported for tests and for callers that drive opacity directly.
    pub alpha: f32,
    /// Downward y offset in physical pixels (0.0 = rest position).
    pub dy: f32,
}

impl WindowAnimationFrame {
    /// A settled window renders with the identity transform.
    pub const REST: Self = Self {
        scale: 1.0,
        alpha: 1.0,
        dy: 0.0,
    };
}

/// Map normalized animation progress to the render transform for `style`.
///
/// `progress` is 0.0 at the "hidden" extreme (freshly opened, or fully
/// closed) and 1.0 once the window has settled; `scale_from` is the
/// configured `window_animation_scale` start factor. Displacement and alpha
/// share this single progress value, so they always run on the same easing.
pub fn window_animation_frame(
    style: WindowAnimationStyle,
    progress: f32,
    scale_from: f32,
) -> WindowAnimationFrame {
    let p = if progress.is_finite() {
        progress.clamp(0.0, 1.0)
    } else {
        0.0
    };
    match style {
        WindowAnimationStyle::Scale => WindowAnimationFrame {
            scale: scale_from + (1.0 - scale_from) * p,
            alpha: 1.0,
            dy: 0.0,
        },
        WindowAnimationStyle::Fade => WindowAnimationFrame {
            scale: 1.0,
            alpha: p,
            dy: 0.0,
        },
        WindowAnimationStyle::Slide => WindowAnimationFrame {
            scale: 1.0,
            alpha: p,
            dy: (1.0 - p) * SLIDE_OFFSET_PX,
        },
    }
}

/// Recover normalized progress from the scale carrier (`anim_scale`), which
/// runs from `scale_from` (hidden) to 1.0 (settled) on open and back on close.
///
/// A degenerate span (`scale_from == 1.0`) carries no animation at all, so the
/// window is reported settled; non-finite carriers are treated the same.
pub fn scale_carrier_progress(anim_scale: f32, scale_from: f32) -> f32 {
    let span = 1.0 - scale_from;
    if !anim_scale.is_finite() || span.abs() < 1.0e-6 {
        return 1.0;
    }
    ((anim_scale - scale_from) / span).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STYLES: [WindowAnimationStyle; 3] = [
        WindowAnimationStyle::Scale,
        WindowAnimationStyle::Fade,
        WindowAnimationStyle::Slide,
    ];

    #[test]
    fn style_from_name_defaults_to_scale() {
        assert_eq!(
            WindowAnimationStyle::from_name("scale"),
            WindowAnimationStyle::Scale
        );
        assert_eq!(
            WindowAnimationStyle::from_name(" Fade "),
            WindowAnimationStyle::Fade
        );
        assert_eq!(
            WindowAnimationStyle::from_name("SLIDE"),
            WindowAnimationStyle::Slide
        );
        assert_eq!(
            WindowAnimationStyle::from_name("unknown"),
            WindowAnimationStyle::Scale
        );
        assert_eq!(
            WindowAnimationStyle::from_name(""),
            WindowAnimationStyle::Scale
        );
    }

    #[test]
    fn style_canonical_names_round_trip() {
        for style in STYLES {
            assert_eq!(
                WindowAnimationStyle::from_name(style.canonical_name()),
                style
            );
        }
    }

    #[test]
    fn style_carriers_match_the_mapping() {
        assert!(WindowAnimationStyle::Scale.uses_scale());
        assert!(!WindowAnimationStyle::Scale.uses_fade());
        for style in [WindowAnimationStyle::Fade, WindowAnimationStyle::Slide] {
            assert!(style.uses_fade(), "{}", style.canonical_name());
            assert!(!style.uses_scale(), "{}", style.canonical_name());
        }
    }

    #[test]
    fn scale_style_zooms_in_without_alpha_or_offset() {
        let frame = window_animation_frame(WindowAnimationStyle::Scale, 0.0, 0.85);
        assert_eq!(
            frame,
            WindowAnimationFrame {
                scale: 0.85,
                alpha: 1.0,
                dy: 0.0,
            }
        );
        let half = window_animation_frame(WindowAnimationStyle::Scale, 0.5, 0.85);
        assert!((half.scale - 0.925).abs() < 1.0e-6);
        assert_eq!(half.alpha, 1.0);
        assert_eq!(half.dy, 0.0);
        assert_eq!(
            window_animation_frame(WindowAnimationStyle::Scale, 1.0, 0.85),
            WindowAnimationFrame::REST
        );
    }

    #[test]
    fn fade_style_only_animates_alpha() {
        let frame = window_animation_frame(WindowAnimationStyle::Fade, 0.0, 0.85);
        assert_eq!(
            frame,
            WindowAnimationFrame {
                scale: 1.0,
                alpha: 0.0,
                dy: 0.0
            }
        );
        let half = window_animation_frame(WindowAnimationStyle::Fade, 0.5, 0.85);
        assert_eq!(
            half,
            WindowAnimationFrame {
                scale: 1.0,
                alpha: 0.5,
                dy: 0.0
            }
        );
        assert_eq!(
            window_animation_frame(WindowAnimationStyle::Fade, 1.0, 0.85),
            WindowAnimationFrame::REST
        );
        // Fade never scales, whatever the progress or configured zoom factor.
        for step in 0..=100 {
            let frame =
                window_animation_frame(WindowAnimationStyle::Fade, step as f32 / 100.0, 1.5);
            assert_eq!(frame.scale, 1.0);
            assert_eq!(frame.dy, 0.0);
        }
    }

    #[test]
    fn slide_style_pairs_alpha_with_a_bounded_vertical_slide() {
        let frame = window_animation_frame(WindowAnimationStyle::Slide, 0.0, 0.85);
        assert_eq!(
            frame,
            WindowAnimationFrame {
                scale: 1.0,
                alpha: 0.0,
                dy: SLIDE_OFFSET_PX,
            }
        );
        let half = window_animation_frame(WindowAnimationStyle::Slide, 0.5, 0.85);
        assert!((half.dy - SLIDE_OFFSET_PX * 0.5).abs() < 1.0e-6);
        assert!((half.alpha - 0.5).abs() < 1.0e-6);
        assert_eq!(
            window_animation_frame(WindowAnimationStyle::Slide, 1.0, 0.85),
            WindowAnimationFrame::REST
        );
        // Opening runs the offset monotonically down to zero as alpha rises.
        let mut previous = SLIDE_OFFSET_PX;
        for step in 0..=100 {
            let frame =
                window_animation_frame(WindowAnimationStyle::Slide, step as f32 / 100.0, 0.85);
            assert!(frame.dy <= previous + 1.0e-6);
            assert!((0.0..=SLIDE_OFFSET_PX).contains(&frame.dy));
            previous = frame.dy;
        }
    }

    #[test]
    fn progress_is_clamped_and_non_finite_progress_hides() {
        for style in STYLES {
            assert_eq!(
                window_animation_frame(style, f32::NAN, 0.85),
                window_animation_frame(style, 0.0, 0.85),
                "{}",
                style.canonical_name()
            );
            assert_eq!(
                window_animation_frame(style, 2.0, 0.85),
                window_animation_frame(style, 1.0, 0.85),
                "{}",
                style.canonical_name()
            );
            assert_eq!(
                window_animation_frame(style, -1.0, 0.85),
                window_animation_frame(style, 0.0, 0.85),
                "{}",
                style.canonical_name()
            );
        }
    }

    #[test]
    fn scale_carrier_progress_inverts_the_zoom() {
        assert_eq!(scale_carrier_progress(0.85, 0.85), 0.0);
        assert_eq!(scale_carrier_progress(1.0, 0.85), 1.0);
        assert!((scale_carrier_progress(0.925, 0.85) - 0.5).abs() < 1.0e-6);
        // Zoom-out factors above 1.0 run the carrier downwards.
        assert_eq!(scale_carrier_progress(1.5, 1.5), 0.0);
        assert_eq!(scale_carrier_progress(1.0, 1.5), 1.0);
        // A degenerate or broken carrier reports a settled window.
        assert_eq!(scale_carrier_progress(1.0, 1.0), 1.0);
        assert_eq!(scale_carrier_progress(f32::NAN, 0.85), 1.0);
        // Out-of-range carriers clamp instead of extrapolating.
        assert_eq!(scale_carrier_progress(0.5, 0.85), 0.0);
        assert_eq!(scale_carrier_progress(1.2, 0.85), 1.0);
    }

    #[test]
    fn scale_frame_matches_the_carrier_it_was_derived_from() {
        // Feeding the recovered progress back through the mapping must return
        // the carrier itself, keeping the scale style bit-identical to the
        // historical direct use of `anim_scale`.
        for scale_from in [0.5, 0.85, 1.5] {
            for step in 0..=100 {
                let carrier = scale_from + (1.0 - scale_from) * step as f32 / 100.0;
                let frame = window_animation_frame(
                    WindowAnimationStyle::Scale,
                    scale_carrier_progress(carrier, scale_from),
                    scale_from,
                );
                assert!((frame.scale - carrier).abs() < 1.0e-5);
            }
        }
    }
}
