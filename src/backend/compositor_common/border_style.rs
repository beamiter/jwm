//! Which border a window gets, shared by the X11 and Wayland compositors.
//!
//! Both backends draw the ring with their own GL wrappers, but the choice of
//! colour and width is policy: a transient focus pulse outranks the urgent
//! attention pulse, which outranks a picture-in-picture frame, which outranks
//! the ordinary focused/unfocused border. Keeping that table here is what
//! makes one config look the same on both backends.

use super::attention::AttentionBorderStyle;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct BorderStyle {
    pub(crate) color: [f32; 4],
    pub(crate) width: f32,
    /// The ordinary focused border, the only one that may upgrade to the
    /// two-colour gradient ring. Signal borders keep their flat colours.
    pub(crate) ordinary_focused: bool,
}

/// Whether a window takes part in smart borders — both in the count that
/// decides whether ordinary borders are drawn at all (more than one client)
/// and as a candidate for a border.
///
/// The status bar is chrome, and override-redirect windows are unmanaged
/// overlays the WM never tiles: IME candidate lists and the input-method
/// switcher (fcitx5 creates those with `override_redirect`), menus, tooltips
/// and drag icons. Counting them would draw a border around the single client
/// of a tag for as long as the popup is up — e.g. the whole time a user types
/// Chinese — and drop it again when the popup closes.
pub(crate) fn counts_for_smart_borders(class_name: &str, status_bar_name: &str, is_or: bool) -> bool {
    if is_or {
        return false;
    }
    // No configured bar means no window is the bar; an empty needle would
    // otherwise match every class and take every border away.
    status_bar_name.is_empty()
        || !(class_name == status_bar_name || class_name.contains(status_bar_name))
}

/// Blend the transient focus indication into the ordinary focused border.
///
/// Keeping both endpoints identical to the stable border avoids a transparent
/// first/last animation frame. The client texture itself is deliberately not
/// transformed: scaling terminal content made text and the insertion cursor
/// appear to flash every time focus changed.
pub(crate) fn focus_highlight_style(
    focused_color: [f32; 4],
    highlight_color: [f32; 4],
    focused_width: f32,
    progress: f32,
) -> BorderStyle {
    let progress = progress
        .is_finite()
        .then_some(progress)
        .unwrap_or(1.0)
        .clamp(0.0, 1.0);
    let pulse = (progress * std::f32::consts::PI).sin().max(0.0);
    let mut color = focused_color;
    for (channel, highlight) in color.iter_mut().zip(highlight_color) {
        *channel += (highlight - *channel) * pulse;
    }
    let highlight_width = (focused_width + 2.0).max(3.0);
    BorderStyle {
        color,
        width: focused_width + (highlight_width - focused_width) * pulse,
        ordinary_focused: false,
    }
}

/// Everything the border choice depends on for one window this frame.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WindowBorderInputs {
    pub(crate) is_focused: bool,
    pub(crate) is_pip: bool,
    /// Progress (0..1) of the focus pulse when it is running on this window.
    pub(crate) focus_highlight_progress: Option<f32>,
    /// The urgent-window pulse when it is active on this window.
    pub(crate) attention: Option<AttentionBorderStyle>,
    /// Ordinary borders apply this frame: enabled, and (smart borders) more
    /// than one counted client on screen.
    pub(crate) ordinary_enabled: bool,
    pub(crate) ordinary_width: f32,
    pub(crate) focused_color: [f32; 4],
    pub(crate) unfocused_color: [f32; 4],
    pub(crate) highlight_color: [f32; 4],
    pub(crate) pip_color: [f32; 4],
    pub(crate) pip_width: f32,
}

/// The border to draw around one window, or `None` when it gets none.
///
/// Attention and picture-in-picture frames are signals and survive ordinary
/// borders being off (disabled, or a lone client under smart borders); every
/// other border needs ordinary borders on with a positive width.
pub(crate) fn window_border_style(inputs: &WindowBorderInputs) -> Option<BorderStyle> {
    let special = inputs.attention.is_some() || inputs.is_pip;
    let ordinary = inputs.ordinary_enabled && inputs.ordinary_width > 0.0;
    if !ordinary && !special {
        return None;
    }
    // The pulse blends into the ordinary focused border, so it only runs
    // while that border exists. With ordinary borders off it would ramp from
    // a zero width (no border at all on its first and last frames) and hide
    // the PiP or attention frame the window keeps otherwise.
    let style = if let Some(progress) = inputs.focus_highlight_progress.filter(|_| ordinary) {
        focus_highlight_style(
            inputs.focused_color,
            inputs.highlight_color,
            inputs.ordinary_width,
            progress,
        )
    } else if let Some(attention) = inputs.attention {
        BorderStyle {
            color: attention.color,
            width: attention.width,
            ordinary_focused: false,
        }
    } else if inputs.is_pip {
        BorderStyle {
            color: inputs.pip_color,
            width: inputs.pip_width,
            ordinary_focused: false,
        }
    } else if inputs.is_focused {
        BorderStyle {
            color: inputs.focused_color,
            width: inputs.ordinary_width,
            ordinary_focused: true,
        }
    } else {
        BorderStyle {
            color: inputs.unfocused_color,
            width: inputs.ordinary_width,
            ordinary_focused: false,
        }
    };
    (style.width > 0.0).then_some(style)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOCUSED: [f32; 4] = [0.4, 0.6, 0.9, 1.0];
    const UNFOCUSED: [f32; 4] = [0.3, 0.3, 0.3, 0.6];
    const HIGHLIGHT: [f32; 4] = [1.0, 0.8, 0.2, 1.0];
    const PIP: [f32; 4] = [0.0, 0.8, 1.0, 0.8];

    fn inputs() -> WindowBorderInputs {
        WindowBorderInputs {
            is_focused: false,
            is_pip: false,
            focus_highlight_progress: None,
            attention: None,
            ordinary_enabled: true,
            ordinary_width: 2.0,
            focused_color: FOCUSED,
            unfocused_color: UNFOCUSED,
            highlight_color: HIGHLIGHT,
            pip_color: PIP,
            pip_width: 3.0,
        }
    }

    #[test]
    fn ime_popups_do_not_count_toward_smart_borders() {
        // A lone tiled client is the only window that counts, so the smart
        // border stays off while an fcitx5 candidate list or the input-method
        // switcher (both override-redirect) is on screen.
        assert!(counts_for_smart_borders("Alacritty", "jwm-bar", false));
        assert!(!counts_for_smart_borders("fcitx", "jwm-bar", true));
        assert!(!counts_for_smart_borders("jwm-bar", "jwm-bar", false));
        assert!(!counts_for_smart_borders("xbar-jwm-bar", "jwm-bar", false));
        // An unset bar name must not turn every window into the bar.
        assert!(counts_for_smart_borders("Alacritty", "", false));
        assert!(counts_for_smart_borders("", "", false));
        assert!(!counts_for_smart_borders("fcitx", "", true));
    }

    #[test]
    fn focus_highlight_returns_to_the_stable_border_at_both_ends() {
        let focused = [0.1, 0.2, 0.3, 0.8];
        let highlight = [0.4, 0.7, 1.0, 0.9];

        let start = focus_highlight_style(focused, highlight, 1.0, 0.0);
        let end = focus_highlight_style(focused, highlight, 1.0, 1.0);
        assert_eq!(start.color, focused);
        assert_eq!(start.width, 1.0);
        assert_eq!(end.color, focused);
        assert_eq!(end.width, 1.0);
    }

    #[test]
    fn focus_highlight_smoothly_reaches_the_configured_peak() {
        let highlight = [0.4, 0.7, 1.0, 0.9];
        let peak = focus_highlight_style([0.1, 0.2, 0.3, 0.8], highlight, 1.0, 0.5);

        assert_eq!(peak.color, highlight);
        assert_eq!(peak.width, 3.0);
    }

    #[test]
    fn a_non_finite_pulse_progress_rests_on_the_stable_border() {
        for progress in [f32::NAN, f32::INFINITY, -1.0, 2.0] {
            let style = focus_highlight_style(FOCUSED, HIGHLIGHT, 2.0, progress);
            assert_eq!(style.color, FOCUSED, "{progress}");
            assert_eq!(style.width, 2.0, "{progress}");
        }
    }

    #[test]
    fn ordinary_borders_split_focused_and_unfocused() {
        let focused = window_border_style(&WindowBorderInputs {
            is_focused: true,
            ..inputs()
        })
        .expect("focused border");
        assert_eq!(focused.color, FOCUSED);
        assert_eq!(focused.width, 2.0);
        assert!(focused.ordinary_focused);

        let unfocused = window_border_style(&inputs()).expect("unfocused border");
        assert_eq!(unfocused.color, UNFOCUSED);
        assert_eq!(unfocused.width, 2.0);
        assert!(!unfocused.ordinary_focused);
    }

    #[test]
    fn signal_borders_outrank_in_a_fixed_order() {
        let attention = AttentionBorderStyle {
            color: [1.0, 0.0, 0.0, 0.5],
            width: 4.0,
        };
        let all = WindowBorderInputs {
            is_focused: true,
            is_pip: true,
            focus_highlight_progress: Some(0.5),
            attention: Some(attention),
            ..inputs()
        };
        let pulse = window_border_style(&all).unwrap();
        for (got, want) in pulse.color.iter().zip(HIGHLIGHT) {
            assert!((got - want).abs() < 1.0e-6, "{:?}", pulse.color);
        }
        let no_pulse = WindowBorderInputs {
            focus_highlight_progress: None,
            ..all
        };
        assert_eq!(window_border_style(&no_pulse).unwrap().color, attention.color);
        let pip_only = WindowBorderInputs {
            attention: None,
            ..no_pulse
        };
        let pip = window_border_style(&pip_only).unwrap();
        assert_eq!((pip.color, pip.width), (PIP, 3.0));
        assert!(!pip.ordinary_focused, "a PiP frame never turns gradient");
    }

    #[test]
    fn signals_survive_ordinary_borders_being_off_and_nothing_else_does() {
        let off = WindowBorderInputs {
            ordinary_enabled: false,
            is_focused: true,
            focus_highlight_progress: Some(0.5),
            ..inputs()
        };
        assert_eq!(window_border_style(&off), None, "a lone client has no ring");
        let pip = WindowBorderInputs {
            is_pip: true,
            ..off
        };
        assert!(window_border_style(&pip).is_some());
        let zero_width = WindowBorderInputs {
            ordinary_width: 0.0,
            is_focused: true,
            ..inputs()
        };
        assert_eq!(window_border_style(&zero_width), None);
    }

    #[test]
    fn focus_pulse_leaves_signal_frames_alone_while_ordinary_borders_are_off() {
        let attention = AttentionBorderStyle {
            color: [1.0, 0.0, 0.0, 0.5],
            width: 4.0,
        };
        // Borders disabled outright, and borders "on" but with a zero width
        // (smart borders hand a lone client the same zero): both leave the
        // pulse nothing to blend into.
        for (ordinary_enabled, ordinary_width) in [(false, 2.0), (true, 0.0)] {
            for progress in [0.0, 0.25, 0.5, 1.0] {
                let pip = WindowBorderInputs {
                    is_focused: true,
                    is_pip: true,
                    focus_highlight_progress: Some(progress),
                    ordinary_enabled,
                    ordinary_width,
                    ..inputs()
                };
                let style = window_border_style(&pip)
                    .unwrap_or_else(|| panic!("PiP frame dropped at pulse {progress}"));
                assert_eq!((style.color, style.width), (PIP, 3.0), "{progress}");
                assert!(!style.ordinary_focused);

                let urgent = WindowBorderInputs {
                    attention: Some(attention),
                    ..pip
                };
                let style = window_border_style(&urgent)
                    .unwrap_or_else(|| panic!("attention frame dropped at pulse {progress}"));
                assert_eq!(
                    (style.color, style.width),
                    (attention.color, attention.width),
                    "{progress}"
                );
            }
        }
    }
}
