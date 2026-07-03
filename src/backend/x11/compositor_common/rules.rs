//! Backend-independent compositor rule helpers.

use std::collections::HashMap;

use crate::renderer::types::BlurQuality;

/// Parsed opacity rule: "opacity_percent:class_name".
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OpacityRule {
    /// Opacity as 0.0..1.0.
    pub(crate) opacity: f32,
    pub(crate) class_name: String,
}

/// Parsed corner radius rule: "radius:class_name".
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CornerRadiusRule {
    pub(crate) radius: f32,
    pub(crate) class_name: String,
}

/// Parsed scale rule: "scale_percent:class_name".
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ScaleRule {
    /// Scale as a multiplier.
    pub(crate) scale: f32,
    pub(crate) class_name: String,
}

/// ASCII case-insensitive substring test that performs no heap allocation.
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
            .all(|(a, b)| a.to_ascii_lowercase() == b.to_ascii_lowercase())
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
    rules
        .iter()
        .filter_map(|rule| {
            let (pct_str, class_name) = rule.split_once(':')?;
            let pct = pct_str.trim().parse::<f32>().ok()?;
            Some(ScaleRule {
                scale: (pct / 100.0).clamp(0.1, 2.0),
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

pub(crate) fn scale_rule_for_class(rules: &[ScaleRule], class_name: &str) -> Option<f32> {
    if class_name.is_empty() {
        return None;
    }
    rules
        .iter()
        .find(|rule| rule.class_name.eq_ignore_ascii_case(class_name))
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
            if i > 0 {
                return Some(blur_strength_by_hz[i - 1].1);
            }
            return Some(strength);
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
    let wx2 = x + w as i32;
    let wy2 = y + h as i32;
    let mut best: Option<(u32, i64)> = None;
    for &(id, mx, my, mw, mh) in monitors {
        let mx2 = mx + mw as i32;
        let my2 = my + mh as i32;
        let ix = (wx2.min(mx2) - x.max(mx)).max(0) as i64;
        let iy = (wy2.min(my2) - y.max(my)).max(0) as i64;
        let area = ix * iy;
        if area > 0 && best.map_or(true, |(_, ba)| area > ba) {
            best = Some((id, area));
        }
    }
    if let Some((id, _)) = best {
        return Some(id);
    }
    let cx = x + w as i32 / 2;
    let cy = y + h as i32 / 2;
    for &(id, mx, my, mw, mh) in monitors {
        if cx >= mx && cx < mx + mw as i32 && cy >= my && cy < my + mh as i32 {
            return Some(id);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_exclude_handles_empty_inputs() {
        let list = vec!["firefox".to_string()];
        assert!(!class_matches_exclude("", &list));
        assert!(!class_matches_exclude("firefox", &[]));
    }

    #[test]
    fn class_exclude_is_case_insensitive() {
        let list = vec!["Firefox".to_string(), "chromium".to_string()];
        assert!(class_matches_exclude("firefox", &list));
        assert!(class_matches_exclude("FIREFOX", &list));
        assert!(class_matches_exclude("chromium", &list));
        assert!(!class_matches_exclude("alacritty", &list));
    }

    #[test]
    fn flameshot_is_always_excluded() {
        assert!(class_matches_exclude("flameshot", &[]));
        assert!(class_matches_exclude("Flameshot", &[]));
        assert!(class_matches_exclude("FLAMESHOT", &[]));
    }

    #[test]
    fn parses_window_rules_and_clamps_values() {
        let opacity = parse_opacity_rules(&[
            "85:Firefox".to_string(),
            "150:over".to_string(),
            "bad".to_string(),
        ]);
        assert_eq!(
            opacity,
            vec![
                OpacityRule {
                    opacity: 0.85,
                    class_name: "Firefox".to_string(),
                },
                OpacityRule {
                    opacity: 1.0,
                    class_name: "over".to_string(),
                },
            ]
        );

        let radius = parse_corner_radius_rules(&["-4:kitty".to_string(), "8:mpv".to_string()]);
        assert_eq!(
            radius,
            vec![
                CornerRadiusRule {
                    radius: 0.0,
                    class_name: "kitty".to_string(),
                },
                CornerRadiusRule {
                    radius: 8.0,
                    class_name: "mpv".to_string(),
                },
            ]
        );

        let scale = parse_scale_rules(&[
            "5:tiny".to_string(),
            "125:normal".to_string(),
            "250:large".to_string(),
        ]);
        assert_eq!(
            scale,
            vec![
                ScaleRule {
                    scale: 0.1,
                    class_name: "tiny".to_string(),
                },
                ScaleRule {
                    scale: 1.25,
                    class_name: "normal".to_string(),
                },
                ScaleRule {
                    scale: 2.0,
                    class_name: "large".to_string(),
                },
            ]
        );
    }

    #[test]
    fn window_rule_lookup_is_exact_case_insensitive() {
        let opacity = parse_opacity_rules(&["80:Firefox".to_string()]);
        assert_eq!(opacity_rule_for_class(&opacity, "firefox"), Some(0.8));
        assert_eq!(
            opacity_rule_for_class(&opacity, "org.mozilla.firefox"),
            None
        );
        assert_eq!(opacity_rule_for_class(&opacity, ""), None);

        let radius = parse_corner_radius_rules(&["12:kitty".to_string()]);
        assert_eq!(corner_radius_rule_for_class(&radius, "KITTY"), Some(12.0));

        let scale = parse_scale_rules(&["90:mpv".to_string()]);
        assert_eq!(scale_rule_for_class(&scale, "MPV"), Some(0.9));
    }

    #[test]
    fn parse_blur_strength_skips_invalid_entries_and_sorts() {
        let result = parse_blur_strength_by_hz("144:3,60:2,bad,75:2,120:2.9");
        assert_eq!(result, vec![(60, 2), (75, 2), (120, 2), (144, 3)]);
    }

    #[test]
    fn blur_strength_lookup_uses_exact_lower_or_nearest() {
        let table = vec![(60, 2), (144, 4)];
        assert_eq!(blur_strength_for_hz(&[], 60), None);
        assert_eq!(blur_strength_for_hz(&table, 60), Some(2));
        assert_eq!(blur_strength_for_hz(&table, 75), Some(2));
        assert_eq!(blur_strength_for_hz(&table, 30), Some(2));
        assert_eq!(blur_strength_for_hz(&table, 240), Some(4));
    }

    #[test]
    fn parse_blur_quality_maps_known_monitor_names() {
        let result = parse_blur_quality_by_monitor(
            "primary:Full,secondary:Reduced,tertiary:Minimal,sixth:Full,primary:Ultra",
        );
        assert_eq!(result.get(&0), Some(&BlurQuality::Full));
        assert_eq!(result.get(&1), Some(&BlurQuality::Reduced));
        assert_eq!(result.get(&2), Some(&BlurQuality::Minimal));
        assert!(!result.contains_key(&5));
    }

    #[test]
    fn monitor_overlap_prefers_largest_intersection() {
        let monitors = vec![(0, 0, 0, 1920, 1080), (1, 1920, 0, 1920, 1080)];
        assert_eq!(
            monitor_id_by_overlap(&monitors, 2000, 100, 400, 300),
            Some(1)
        );
        assert_eq!(
            monitor_id_by_overlap(&monitors, 1340, 100, 1000, 500),
            Some(0)
        );
        assert_eq!(monitor_id_by_overlap(&monitors, 100, 5000, 200, 200), None);
        assert_eq!(monitor_id_by_overlap(&[], 0, 0, 100, 100), None);
    }
}
