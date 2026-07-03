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
