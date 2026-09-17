//! Backend-independent compositor rule helpers.

use std::collections::HashMap;

use crate::renderer::types::BlurQuality;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OpacityRule {
    pub(crate) opacity: f32,
    pub(crate) class_name: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CornerRadiusRule {
    pub(crate) radius: f32,
    pub(crate) class_name: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ScaleRule {
    pub(crate) scale: f32,
    pub(crate) class_name: String,
}

pub(crate) fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    let n = needle.len();
    if n == 0 {
        return true;
    }
    let h = haystack.as_bytes();
    let ne = needle.as_bytes();
    if h.len() < n {
        return false;
    }
    let first = ne[0].to_ascii_lowercase();
    for start in 0..=h.len() - n {
        if h[start].to_ascii_lowercase() != first {
            continue;
        }
        if h[start..start + n]
            .iter()
            .zip(ne)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
        {
            return true;
        }
    }
    false
}

pub(crate) fn class_matches_exclude(class_name: &str, exclude_list: &[String]) -> bool {
    if class_name.is_empty() {
        return false;
    }
    if class_name.eq_ignore_ascii_case("flameshot") {
        return true;
    }
    exclude_list
        .iter()
        .any(|ex| ex.eq_ignore_ascii_case(class_name))
}

pub(crate) fn class_matches_pattern_exclude(class_name: &str, exclude_list: &[String]) -> bool {
    if class_name.is_empty() {
        return false;
    }
    if contains_ignore_case(class_name, "flameshot") {
        return true;
    }
    exclude_list
        .iter()
        .any(|pattern| contains_ignore_case(class_name, pattern))
}

pub(crate) fn parse_opacity_rules(rules: &[String]) -> Vec<OpacityRule> {
    rules
        .iter()
        .filter_map(|rule| {
            let (pct_str, class_name) = rule.split_once(':')?;
            let pct = pct_str.trim().parse::<f32>().ok()?;
            Some(OpacityRule {
                opacity: (pct / 100.0).clamp(0.0, 1.0),
                class_name: class_name.trim().to_string(),
            })
        })
        .collect()
}

pub(crate) fn parse_corner_radius_rules(rules: &[String]) -> Vec<CornerRadiusRule> {
    rules
        .iter()
        .filter_map(|rule| {
            let (radius_str, class_name) = rule.split_once(':')?;
            let radius = radius_str.trim().parse::<f32>().ok()?;
            Some(CornerRadiusRule {
                radius: radius.max(0.0),
                class_name: class_name.trim().to_string(),
            })
        })
        .collect()
}

pub(crate) fn parse_scale_rules(rules: &[String]) -> Vec<ScaleRule> {
    parse_scale_rules_with_bounds(rules, 0.1, 2.0)
}

pub(crate) fn parse_scale_rules_with_bounds(
    rules: &[String],
    min_scale: f32,
    max_scale: f32,
) -> Vec<ScaleRule> {
    rules
        .iter()
        .filter_map(|rule| {
            let (pct_str, class_name) = rule.split_once(':')?;
            let pct = pct_str.trim().parse::<f32>().ok()?;
            Some(ScaleRule {
                scale: (pct / 100.0).clamp(min_scale, max_scale),
                class_name: class_name.trim().to_string(),
            })
        })
        .collect()
}

pub(crate) fn opacity_rule_for_class(rules: &[OpacityRule], class_name: &str) -> Option<f32> {
    if class_name.is_empty() {
        return None;
    }
    rules
        .iter()
        .find(|rule| rule.class_name.eq_ignore_ascii_case(class_name))
        .map(|rule| rule.opacity)
}

pub(crate) fn opacity_rule_for_pattern(rules: &[OpacityRule], class_name: &str) -> Option<f32> {
    rules
        .iter()
        .find(|rule| contains_ignore_case(class_name, &rule.class_name))
        .map(|rule| rule.opacity)
}

pub(crate) fn corner_radius_rule_for_class(
    rules: &[CornerRadiusRule],
    class_name: &str,
) -> Option<f32> {
    if class_name.is_empty() {
        return None;
    }
    rules
        .iter()
        .find(|rule| rule.class_name.eq_ignore_ascii_case(class_name))
        .map(|rule| rule.radius)
}

pub(crate) fn corner_radius_rule_for_pattern(
    rules: &[CornerRadiusRule],
    class_name: &str,
) -> Option<f32> {
    rules
        .iter()
        .find(|rule| contains_ignore_case(class_name, &rule.class_name))
        .map(|rule| rule.radius)
}

pub(crate) fn scale_rule_for_class(rules: &[ScaleRule], class_name: &str) -> Option<f32> {
    if class_name.is_empty() {
        return None;
    }
    rules
        .iter()
        .find(|rule| rule.class_name.eq_ignore_ascii_case(class_name))
        .map(|rule| rule.scale)
}

pub(crate) fn scale_rule_for_pattern(rules: &[ScaleRule], class_name: &str) -> Option<f32> {
    rules
        .iter()
        .find(|rule| contains_ignore_case(class_name, &rule.class_name))
        .map(|rule| rule.scale)
}

pub(crate) fn parse_blur_strength_by_hz(config_str: &str) -> Vec<(u32, u32)> {
    let mut result = Vec::new();
    if config_str.is_empty() {
        return result;
    }
    for pair in config_str.split(',') {
        let parts: Vec<&str> = pair.trim().split(':').collect();
        if parts.len() == 2 {
            if let (Ok(hz), Ok(strength_f)) = (
                parts[0].trim().parse::<u32>(),
                parts[1].trim().parse::<f32>(),
            ) {
                result.push((hz, strength_f as u32));
            }
        }
    }
    result.sort_by_key(|p| p.0);
    result
}

pub(crate) fn blur_strength_for_hz(blur_strength_by_hz: &[(u32, u32)], hz: u32) -> Option<u32> {
    if blur_strength_by_hz.is_empty() {
        return None;
    }
    for (i, &(config_hz, strength)) in blur_strength_by_hz.iter().enumerate() {
        if config_hz == hz {
            return Some(strength);
        }
        if config_hz > hz {
            return Some(if i > 0 {
                blur_strength_by_hz[i - 1].1
            } else {
                strength
            });
        }
    }
    blur_strength_by_hz.last().map(|p| p.1)
}

pub(crate) fn parse_blur_quality_by_monitor(config_str: &str) -> HashMap<u32, BlurQuality> {
    let mut result = HashMap::new();
    if config_str.is_empty() {
        return result;
    }
    let monitor_names = ["primary", "secondary", "tertiary", "quaternary", "quinary"];
    for pair in config_str.split(',') {
        let parts: Vec<&str> = pair.trim().split(':').collect();
        if parts.len() == 2 {
            let monitor_name = parts[0].trim();
            let quality_str = parts[1].trim();
            if let Some(idx) = monitor_names.iter().position(|&n| n == monitor_name) {
                let quality = match quality_str {
                    "Full" => BlurQuality::Full,
                    "Reduced" => BlurQuality::Reduced,
                    "Minimal" => BlurQuality::Minimal,
                    _ => continue,
                };
                result.insert(idx as u32, quality);
            }
        }
    }
    result
}

/// Aggregate per-window displacement, in pixels since the previous frame, at
/// which the temporal blur drops its history entirely.
pub(crate) const TEMPORAL_MIX_MOTION_SPAN_PX: f32 = 12.0;

/// The same span while a frosted status bar is on screen. The bar's frost runs
/// the chrome's deep Kawase chain, whose smear lasts visibly longer than a
/// window frost's, so its history has to go sooner.
pub(crate) const TEMPORAL_MIX_MOTION_SPAN_PX_FROSTED_BAR: f32 = 8.0;

/// Ceiling on the mix while a frosted status bar's backdrop changed *in place*.
///
/// In-place change is the case window displacement cannot see: a video or
/// animated wallpaper, or a media client repainting under the bar, moves every
/// pixel of the backdrop while every window stays exactly where it was. The
/// frame is a cache miss either way, so the blur is refiltered — but mixing
/// four fifths of the previous blur back into it turns the bar into a smear of
/// the last dozen video frames. A ceiling low enough that history decays within
/// a couple of frames keeps the shimmer suppression without the ghost.
pub(crate) const TEMPORAL_MIX_CONTENT_CEILING_FROSTED_BAR: f32 = 0.3;

/// How much of the previous frame's blur to mix into this frame's.
///
/// `base` is the configured `behavior.blur_temporal_mix_ratio`, which is what a
/// still desktop gets: with nothing moving and nothing repainting, history and
/// present are the same picture, and mixing them is pure shimmer suppression at
/// no cost in fidelity. Everything below only ever takes ratio *away*, so a
/// static desktop is never charged for the motion cases.
///
/// `total_displacement_px` is the aggregate distance every window in the scene
/// moved since the previous frame; it attenuates the mix linearly to zero over
/// one of the spans above. X11 passes zero: its per-consumer below-scene hash
/// already refuses the mix outright on any geometry change, so displacement
/// never reaches this decision there.
///
/// `backdrop_content_dirty` is the in-place case
/// ([`TEMPORAL_MIX_CONTENT_CEILING_FROSTED_BAR`]).
pub(crate) fn temporal_mix_ratio(
    base: f32,
    total_displacement_px: u64,
    status_bar_frosted: bool,
    backdrop_content_dirty: bool,
) -> f32 {
    let base = if base.is_finite() {
        base.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let span = if status_bar_frosted {
        TEMPORAL_MIX_MOTION_SPAN_PX_FROSTED_BAR
    } else {
        TEMPORAL_MIX_MOTION_SPAN_PX
    };
    let attenuation = (total_displacement_px as f32 / span).min(1.0);
    let mut ratio = base * (1.0 - attenuation);
    if status_bar_frosted && backdrop_content_dirty {
        ratio = ratio.min(TEMPORAL_MIX_CONTENT_CEILING_FROSTED_BAR);
    }
    ratio.clamp(0.0, 1.0)
}

pub(crate) fn monitor_id_by_overlap(
    monitors: &[(u32, i32, i32, u32, u32)],
    x: i32,
    y: i32,
    w: u32,
    h: u32,
) -> Option<u32> {
    if monitors.is_empty() {
        return None;
    }
    let (x, y) = (i64::from(x), i64::from(y));
    let wx2 = x + i64::from(w);
    let wy2 = y + i64::from(h);
    let mut best: Option<(u32, u128)> = None;
    for &(id, mx, my, mw, mh) in monitors {
        let (mx, my) = (i64::from(mx), i64::from(my));
        let mx2 = mx + i64::from(mw);
        let my2 = my + i64::from(mh);
        let ix = (wx2.min(mx2) - x.max(mx)).max(0) as u128;
        let iy = (wy2.min(my2) - y.max(my)).max(0) as u128;
        let area = ix * iy;
        if area > 0 && best.map_or(true, |(_, ba)| area > ba) {
            best = Some((id, area));
        }
    }
    if let Some((id, _)) = best {
        return Some(id);
    }
    let cx = x + i64::from(w) / 2;
    let cy = y + i64::from(h) / 2;
    for &(id, mx, my, mw, mh) in monitors {
        let (mx, my) = (i64::from(mx), i64::from(my));
        if cx >= mx && cx < mx + i64::from(mw) && cy >= my && cy < my + i64::from(mh) {
            return Some(id);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{
        TEMPORAL_MIX_CONTENT_CEILING_FROSTED_BAR, TEMPORAL_MIX_MOTION_SPAN_PX,
        TEMPORAL_MIX_MOTION_SPAN_PX_FROSTED_BAR, monitor_id_by_overlap, temporal_mix_ratio,
    };

    /// The whole point of the temporal mix is a motionless desktop, so nothing
    /// the frosted-bar tightening adds may cost that case any stabilization.
    #[test]
    fn a_still_desktop_keeps_the_configured_mix_whatever_is_frosted() {
        for &frosted in &[false, true] {
            assert!((temporal_mix_ratio(0.8, 0, frosted, false) - 0.8).abs() < 1e-6);
        }
    }

    /// A frosted bar's frost runs the deep chrome chain, so its history has to
    /// be dropped over a shorter run of motion than a window frost's.
    #[test]
    fn a_frosted_bar_drops_its_history_sooner_than_a_window_frost() {
        assert!(TEMPORAL_MIX_MOTION_SPAN_PX_FROSTED_BAR < TEMPORAL_MIX_MOTION_SPAN_PX);
        let bar = temporal_mix_ratio(0.8, 6, true, false);
        let window = temporal_mix_ratio(0.8, 6, false, false);
        assert!(
            bar < window,
            "6px of motion left the bar at {bar} and a window at {window}"
        );
        // Both still reach zero, and neither goes negative past its span.
        assert_eq!(
            temporal_mix_ratio(
                0.8,
                TEMPORAL_MIX_MOTION_SPAN_PX_FROSTED_BAR as u64,
                true,
                false
            ),
            0.0
        );
        assert_eq!(temporal_mix_ratio(0.8, 4_000, true, false), 0.0);
        assert_eq!(temporal_mix_ratio(0.8, 4_000, false, false), 0.0);
    }

    /// A video wallpaper moves every pixel of the backdrop with every window
    /// standing still, so displacement sees nothing. The bar's frost has to
    /// stop holding a dozen frames of it anyway.
    #[test]
    fn in_place_backdrop_change_caps_a_frosted_bars_history() {
        let ghosting = temporal_mix_ratio(0.8, 0, true, true);
        assert!(
            ghosting <= TEMPORAL_MIX_CONTENT_CEILING_FROSTED_BAR,
            "a repainting backdrop left the bar mixing {ghosting} of its history"
        );
        assert!(TEMPORAL_MIX_CONTENT_CEILING_FROSTED_BAR < 0.8);
        // Only the bar's frost is tightened: a client's own backdrop blur keeps
        // the configured ratio, which is what it was tuned against.
        assert!((temporal_mix_ratio(0.8, 0, false, true) - 0.8).abs() < 1e-6);
        // The ceiling is a ceiling, not a floor: motion still wins.
        assert_eq!(temporal_mix_ratio(0.8, 100, true, true), 0.0);
        // And a configured ratio already under it passes through.
        assert!((temporal_mix_ratio(0.1, 0, true, true) - 0.1).abs() < 1e-6);
    }

    /// The ratio is a shader uniform, so a nonsense config must not reach it.
    #[test]
    fn the_mix_ratio_stays_a_ratio() {
        assert_eq!(temporal_mix_ratio(4.0, 0, false, false), 1.0);
        assert_eq!(temporal_mix_ratio(-1.0, 0, false, false), 0.0);
        assert_eq!(temporal_mix_ratio(f32::NAN, 0, true, true), 0.0);
    }

    #[test]
    fn monitor_overlap_prefers_the_largest_area_and_keeps_ties_stable() {
        let monitors = [(1, 0, 0, 100, 100), (2, 100, 0, 200, 100)];
        assert_eq!(monitor_id_by_overlap(&monitors, 80, 0, 100, 100), Some(2));
        assert_eq!(monitor_id_by_overlap(&monitors, 50, 0, 100, 100), Some(1));
    }

    #[test]
    fn monitor_overlap_handles_full_width_geometry_without_overflow() {
        let monitors = [
            (7, i32::MIN, i32::MIN, u32::MAX, u32::MAX),
            (8, 0, 0, 10, 10),
        ];
        assert_eq!(
            monitor_id_by_overlap(&monitors, i32::MIN, i32::MIN, u32::MAX, u32::MAX),
            Some(7)
        );
    }

    #[test]
    fn zero_area_window_uses_an_extreme_width_center_safely() {
        let monitors = [(9, -10, -10, 20, 20)];
        assert_eq!(
            monitor_id_by_overlap(&monitors, i32::MIN, 0, u32::MAX, 0),
            Some(9)
        );
    }
}
