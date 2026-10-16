//! Shared, tag-only contrast policy. Wallpaper luminance changes the material,
//! never the bar surface. Near-opaque neutral pills bound the influence of even
//! a busy wallpaper; a mean sample alone cannot guarantee local contrast.
use serde::{Deserialize, Serialize};

use crate::ThemeMode;
use crate::presentation::{Palette, Rgba, Stroke, VisualState};

/// `auto` follows the wallpaper; `theme` follows the application theme. Explicit
/// `dark` and `light` choices pin only the tags, leaving other controls alone.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TagContrastMode {
    #[default]
    Auto,
    Theme,
    Dark,
    Light,
}

/// Ready-to-paint colors shared by Scene and native toolkit renderers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TagColors {
    pub fill: Rgba,
    pub foreground: Rgba,
    pub stroke: Option<Stroke>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TagPalette {
    pub empty: TagColors,
    pub occupied: TagColors,
    pub hovered: TagColors,
    pub selected: TagColors,
    pub urgent: TagColors,
}

impl TagPalette {
    /// Samples are linear relative luminance encoded in 0..=255. Temporal
    /// smoothing/hysteresis belongs to the producer, not individual renderers.
    /// Missing or stale samples fall back to the application theme.
    #[must_use]
    pub fn for_wallpaper(theme: ThemeMode, luminance: Option<u8>, mode: TagContrastMode) -> Self {
        let material = match mode {
            TagContrastMode::Dark => ThemeMode::Dark,
            TagContrastMode::Light => ThemeMode::Light,
            TagContrastMode::Theme => theme,
            TagContrastMode::Auto => match luminance {
                Some(value) if value >= 46 => ThemeMode::Light,
                Some(_) => ThemeMode::Dark,
                None => theme,
            },
        };
        let (empty, occupied, hovered, foreground) = match material {
            ThemeMode::Dark => (28, 48, 60, Rgba::rgb8(255, 255, 255)),
            ThemeMode::Light => (248, 224, 208, Rgba::rgb8(0, 0, 0)),
        };
        let neutral = |level, occupied: bool| {
            let mut fill = Rgba::rgb8(level, level, level);
            fill.alpha = 0.94;
            TagColors {
                fill,
                foreground,
                stroke: Some(Stroke {
                    color: match (material, occupied) {
                        (ThemeMode::Dark, false) => Rgba::rgb8(160, 160, 160),
                        (ThemeMode::Dark, true) => Rgba::rgb8(176, 176, 176),
                        (ThemeMode::Light, false) => Rgba::rgb8(128, 128, 128),
                        (ThemeMode::Light, true) => Rgba::rgb8(88, 88, 88),
                    },
                    width: if occupied { 1.5 } else { 1.0 },
                }),
            }
        };
        // Preserve the theme's blue/red identity. Both accent fills have at
        // least 5:1 contrast with black; white fails 4.5:1 on these colors.
        let accents = Palette::for_theme(theme);
        let accent = |fill, width| TagColors {
            fill,
            foreground: Rgba::rgb8(0, 0, 0),
            stroke: Some(Stroke {
                color: Rgba::rgb8(0, 0, 0),
                width,
            }),
        };
        Self {
            empty: neutral(empty, false),
            occupied: neutral(occupied, true),
            hovered: neutral(hovered, true),
            selected: accent(accents.selected, 1.0),
            urgent: accent(accents.urgent, 2.0),
        }
    }

    /// Urgency wins over selection for both fill and text. Hover does not
    /// erase active-state identity.
    #[must_use]
    pub const fn colors(self, state: VisualState) -> TagColors {
        if state.urgent {
            self.urgent
        } else if state.selected {
            self.selected
        } else if state.hovered {
            self.hovered
        } else if state.occupied {
            self.occupied
        } else {
            self.empty
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linear(channel: f32) -> f32 {
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    }

    fn luminance(rgb: [f32; 3]) -> f32 {
        rgb[0] * 0.2126 + rgb[1] * 0.7152 + rgb[2] * 0.0722
    }

    fn rgb(color: Rgba) -> [f32; 3] {
        [color.red, color.green, color.blue]
    }

    #[test]
    fn all_states_pass_wcag_over_every_extreme_background_in_both_compositors() {
        // The eight RGB cube corners include black/white and saturated colors.
        // For opaque black/white text, channel monotonicity makes these bound
        // every possible pixel, including arbitrary busy wallpaper patterns.
        let mut minimum = f32::INFINITY;
        for theme in [ThemeMode::Dark, ThemeMode::Light] {
            for sample in [None, Some(0), Some(45), Some(46), Some(128), Some(255)] {
                for mode in [
                    TagContrastMode::Auto,
                    TagContrastMode::Theme,
                    TagContrastMode::Dark,
                    TagContrastMode::Light,
                ] {
                    let p = TagPalette::for_wallpaper(theme, sample, mode);
                    for colors in [p.empty, p.occupied, p.hovered, p.selected, p.urgent] {
                        assert_eq!(colors.foreground.alpha, 1.0);
                        for mask in 0..8 {
                            let bg = [
                                (mask & 1 != 0) as u8 as f32,
                                (mask & 2 != 0) as u8 as f32,
                                (mask & 4 != 0) as u8 as f32,
                            ];
                            for linear_blend in [false, true] {
                                let mut composed = [0.0; 3];
                                for c in 0..3 {
                                    let f = rgb(colors.fill)[c];
                                    let a = colors.fill.alpha;
                                    composed[c] = if linear_blend {
                                        linear(f) * a + bg[c] * (1.0 - a)
                                    } else {
                                        linear(f * a + bg[c] * (1.0 - a))
                                    };
                                }
                                let back = luminance(composed);
                                let front = luminance(rgb(colors.foreground).map(linear));
                                let ratio = (back.max(front) + 0.05) / (back.min(front) + 0.05);
                                if let Some(stroke) = colors.stroke {
                                    let edge = luminance(rgb(stroke.color).map(linear));
                                    let edge_ratio =
                                        (back.max(edge) + 0.05) / (back.min(edge) + 0.05);
                                    assert!(
                                        edge_ratio >= 3.0,
                                        "stroke against inner fill: {edge_ratio}"
                                    );
                                }
                                minimum = minimum.min(ratio);
                                assert!(
                                    ratio >= 4.5,
                                    "{theme:?} {sample:?} {mode:?} {colors:?}: {ratio}"
                                );
                            }
                        }
                    }
                }
            }
        }
        eprintln!("Worst-case tag text contrast: {minimum:.3}:1");
    }

    #[test]
    fn adaptive_threshold_fallback_and_manual_modes_are_deterministic() {
        let p = |sample, mode| TagPalette::for_wallpaper(ThemeMode::Dark, sample, mode);
        assert_eq!(
            p(None, TagContrastMode::Auto),
            p(Some(0), TagContrastMode::Auto)
        );
        assert_eq!(
            p(Some(45), TagContrastMode::Auto),
            p(None, TagContrastMode::Dark)
        );
        assert_eq!(
            p(Some(46), TagContrastMode::Auto),
            p(None, TagContrastMode::Light)
        );
        assert_eq!(
            p(Some(255), TagContrastMode::Theme),
            p(None, TagContrastMode::Dark)
        );
        assert_eq!(
            p(Some(255), TagContrastMode::Dark),
            p(None, TagContrastMode::Dark)
        );
    }

    #[test]
    fn state_priority_and_distinction_are_preserved() {
        for theme in [ThemeMode::Dark, ThemeMode::Light] {
            let p = TagPalette::for_wallpaper(theme, None, TagContrastMode::Auto);
            assert_ne!(p.empty.fill, p.occupied.fill);
            assert_ne!(p.occupied.fill, p.hovered.fill);
            assert_ne!(p.selected.fill, p.urgent.fill);
            assert!(p.selected.stroke.is_some());
            assert!(p.urgent.stroke.is_some());
            assert_eq!(
                p.colors(VisualState {
                    urgent: true,
                    selected: true,
                    hovered: true,
                    occupied: true
                }),
                p.urgent
            );
            assert_eq!(
                p.colors(VisualState {
                    selected: true,
                    hovered: true,
                    ..VisualState::default()
                }),
                p.selected
            );
        }
    }
}
