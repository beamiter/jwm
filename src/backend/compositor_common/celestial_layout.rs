//! One deterministic canvas/picking contract for all celestial wallpaper renderers.
//! Output identities are connector names; geometry only determines visible pixels.
use super::animated_wallpaper::{AstronomyDraw, astronomy_draw_plan, celestial_assignments};
use crate::config::BehaviorConfig;
use std::collections::BTreeMap;

pub(crate) type CelestialOutput = (String, i32, i32, u32, u32);

/// Every clip is disjoint from every other returned clip. The canvas is the
/// complete group rectangle, even when this output shows only one part of it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CelestialCanvas {
    pub(crate) scene: String,
    pub(crate) group_key: String,
    pub(crate) connector: String,
    pub(crate) canvas: AstronomyDraw,
}

const MAX_OUTPUTS: usize = 128;
const MAX_CLIPS: usize = 4096;

fn global_canvas(screen: (u32, u32), scene: &str) -> Vec<CelestialCanvas> {
    astronomy_draw_plan(screen, &[], false, None)
        .into_iter()
        .map(|canvas| CelestialCanvas {
            scene: scene.into(),
            group_key: format!("shared:{scene}"),
            connector: String::new(),
            canvas,
        })
        .collect()
}

/// `span` shares one desktop canvas; `mirror` shares navigation but frames each
/// output independently; `independent` uses one scene per connector, except that
/// a named group shares both its bounding canvas and its navigation. The legacy
/// `per-output` and `earth-moon` layouts retain their original assignment rules.
pub(crate) fn resolve_celestial_canvases(
    screen: (u32, u32),
    outputs: &[CelestialOutput],
    behavior: &BehaviorConfig,
) -> Vec<CelestialCanvas> {
    let scene = if matches!(
        behavior.wallpaper_astronomy_layout.as_str(),
        "per-output" | "earth-moon"
    ) {
        // These pre-catalogue layouts never selected the new global scene.
        "earth"
    } else {
        behavior.wallpaper_astronomy_scene.as_str()
    };
    if outputs.is_empty() || outputs.len() > MAX_OUTPUTS {
        return global_canvas(screen, scene);
    }
    let (sw, sh) = (i64::from(screen.0), i64::from(screen.1));
    if sw == 0 || sh == 0 || sw > i64::from(i32::MAX) || sh > i64::from(i32::MAX) {
        return Vec::new();
    }
    let mut outputs: Vec<_> = outputs.iter().filter(|o| o.3 > 0 && o.4 > 0).collect();
    // Mirrored connectors have the same framebuffer pixels. Pick the first
    // connector name deterministically, independent of XRandR enumeration.
    outputs.sort_unstable_by_key(|o| (o.1, o.2, o.3, o.4, &o.0));
    outputs.dedup_by(|a, b| (a.1, a.2, a.3, a.4) == (b.1, b.2, b.3, b.4));
    let names: Vec<_> = outputs.iter().map(|o| o.0.clone()).collect();
    let legacy = celestial_assignments(&names, &behavior.wallpaper_astronomy_bodies);
    let layout = behavior.wallpaper_astronomy_layout.as_str();
    let mut result = Vec::new();
    let mut group_bounds: BTreeMap<String, (i64, i64, i64, i64)> = BTreeMap::new();
    for (index, output) in outputs.iter().enumerate() {
        let &(ref connector, x, y, w, h) = *output;
        let mut draw = astronomy_draw_plan(screen, &[(x, y, w, h)], true, None);
        let Some(canvas) = draw.pop() else { continue };
        let (chosen, group_key) = match layout {
            "independent" => {
                let settings = behavior.wallpaper_astronomy_outputs.get(connector);
                let chosen = settings.map_or(scene, |s| s.scene.as_str());
                let key = match settings {
                    Some(s) if !s.group.is_empty() => format!("group:{}:{chosen}", s.group),
                    _ => format!("output:{connector}:{chosen}"),
                };
                (chosen, key)
            }
            "earth-moon" => {
                let chosen = if legacy[index] == super::animated_wallpaper::CelestialBody::Moon {
                    "moon"
                } else {
                    "earth"
                };
                (chosen, format!("legacy:{chosen}"))
            }
            _ => (scene, format!("shared:{scene}")),
        };
        let named_group = layout == "independent"
            && behavior
                .wallpaper_astronomy_outputs
                .get(connector)
                .is_some_and(|s| !s.group.is_empty());
        if named_group {
            let rect = (
                i64::from(x),
                sh - i64::from(y) - i64::from(h),
                i64::from(x) + i64::from(w),
                sh - i64::from(y),
            );
            group_bounds
                .entry(group_key.clone())
                .and_modify(|b| {
                    b.0 = b.0.min(rect.0);
                    b.1 = b.1.min(rect.1);
                    b.2 = b.2.max(rect.2);
                    b.3 = b.3.max(rect.3);
                })
                .or_insert(rect);
        }
        result.push(CelestialCanvas {
            scene: chosen.into(),
            group_key,
            connector: connector.clone(),
            canvas,
        });
    }
    for entry in &mut result {
        if layout == "span" {
            entry.canvas.origin = [0.0, 0.0];
            entry.canvas.size = [screen.0 as f32, screen.1 as f32];
            entry.canvas.radius = screen.1 as f32 * 0.42;
        } else if let Some(&(left, bottom, right, top)) = group_bounds.get(&entry.group_key) {
            let width = (right - left) as f32;
            let height = (top - bottom) as f32;
            entry.canvas.origin = [left as f32, bottom as f32];
            entry.canvas.size = [width, height];
            entry.canvas.radius = width.min(height) * 0.42;
        }
    }

    // Remove pixels hidden by a later output before different renderer kinds
    // split this list. Astro/solar draw order therefore cannot expose a planet
    // through another output's background, and hit testing uses identical clips.
    let mut visible = Vec::new();
    for (index, entry) in result.iter().enumerate() {
        let mut pieces = vec![entry.canvas.clip];
        for cover in &result[index + 1..] {
            let mut next = Vec::new();
            for piece in pieces {
                subtract_clip(piece, cover.canvas.clip, &mut next);
                if next.len() + visible.len() > MAX_CLIPS {
                    // Pathological overlapping topology cannot create unbounded
                    // draw calls. The ordinary shared canvas remains available.
                    return global_canvas(screen, scene);
                }
            }
            pieces = next;
        }
        for clip in pieces {
            let mut entry = entry.clone();
            entry.canvas.clip = clip;
            visible.push(entry);
        }
    }
    visible
}

fn subtract_clip(
    source: (i32, i32, i32, i32),
    cover: (i32, i32, i32, i32),
    result: &mut Vec<(i32, i32, i32, i32)>,
) {
    // Clips already fit the bounded root framebuffer, so these sums fit i32.
    let (x0, y0, x1, y1) = (source.0, source.1, source.0 + source.2, source.1 + source.3);
    let (left, bottom, right, top) = (
        x0.max(cover.0),
        y0.max(cover.1),
        x1.min(cover.0 + cover.2),
        y1.min(cover.1 + cover.3),
    );
    if left >= right || bottom >= top {
        result.push(source);
        return;
    }
    for r in [
        (x0, y0, x1 - x0, bottom - y0),
        (x0, top, x1 - x0, y1 - top),
        (x0, bottom, left - x0, top - bottom),
        (right, bottom, x1 - right, top - bottom),
    ] {
        if r.2 > 0 && r.3 > 0 {
            result.push(r);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AstronomyOutputConfig;

    fn outputs() -> Vec<CelestialOutput> {
        vec![
            ("DP-1".into(), 0, 0, 1920, 1080),
            ("HDMI-1".into(), 1920, 0, 1920, 1080),
        ]
    }
    #[test]
    fn group_shares_a_canvas_but_independent_equal_bodies_do_not_share_navigation() {
        let mut b = crate::config::Config::default().behavior().clone();
        b.wallpaper_astronomy_layout = "independent".into();
        let independent = resolve_celestial_canvases((3840, 1080), &outputs(), &b);
        assert_ne!(independent[0].group_key, independent[1].group_key);
        for (name, ..) in outputs() {
            b.wallpaper_astronomy_outputs.insert(
                name,
                AstronomyOutputConfig {
                    scene: "saturn".into(),
                    group: "observatory".into(),
                },
            );
        }
        let group = resolve_celestial_canvases((3840, 1080), &outputs(), &b);
        assert_eq!(group[0].group_key, group[1].group_key);
        assert_eq!(group[0].canvas.origin, group[1].canvas.origin);
        assert_eq!(group[0].canvas.size, [3840.0, 1080.0]);
        assert_ne!(group[0].canvas.clip, group[1].canvas.clip);
    }
    #[test]
    fn mirror_shares_pose_and_frames_each_output_without_stretching() {
        let mut b = crate::config::Config::default().behavior().clone();
        b.wallpaper_astronomy_layout = "mirror".into();
        let plan = resolve_celestial_canvases((3840, 1080), &outputs(), &b);
        assert_eq!(plan[0].group_key, plan[1].group_key);
        assert_ne!(plan[0].canvas.origin, plan[1].canvas.origin);
        assert_eq!(plan[0].canvas.size, [1920.0, 1080.0]);
    }
    #[test]
    fn legacy_per_output_stays_earth_even_with_a_new_default_scene() {
        let mut b = crate::config::Config::default().behavior().clone();
        b.wallpaper_astronomy_layout = "per-output".into();
        b.wallpaper_astronomy_scene = "saturn".into();
        let plan = resolve_celestial_canvases((3840, 1080), &outputs(), &b);
        assert_eq!(plan.len(), 2);
        for entry in &plan {
            assert_eq!(entry.scene, "earth");
            assert_eq!(entry.group_key, "shared:earth");
            assert_eq!(entry.canvas.size, [1920.0, 1080.0]);
        }
        let missing_outputs = resolve_celestial_canvases((3840, 1080), &[], &b);
        assert_eq!(missing_outputs[0].scene, "earth");
    }
    #[test]
    fn overlapping_mixed_renderers_get_disjoint_clips_and_stable_connector_identity() {
        let mut b = crate::config::Config::default().behavior().clone();
        b.wallpaper_astronomy_layout = "independent".into();
        b.wallpaper_astronomy_outputs.insert(
            "HDMI-1".into(),
            AstronomyOutputConfig {
                scene: "solar-system".into(),
                group: String::new(),
            },
        );
        let mut outputs = vec![
            ("DP-1".into(), 0, 0, 100, 100),
            ("HDMI-1".into(), 50, 30, 100, 100),
        ];
        let plan = resolve_celestial_canvases((150, 130), &outputs, &b);
        outputs.reverse();
        assert_eq!(plan, resolve_celestial_canvases((150, 130), &outputs, &b));
        for y in 0..130 {
            for x in 0..150 {
                let covers: Vec<_> = plan
                    .iter()
                    .filter(|p| {
                        let (px, py, w, h) = p.canvas.clip;
                        x >= px && x < px + w && y >= py && y < py + h
                    })
                    .collect();
                assert!(covers.len() <= 1);
                if x >= 50 && y < 100 {
                    assert_eq!(covers[0].scene, "solar-system");
                }
            }
        }
    }
    #[test]
    fn span_clips_real_outputs_and_preserves_desktop_canvas_across_gaps() {
        let b = crate::config::Config::default().behavior().clone();
        let plan = resolve_celestial_canvases(
            (300, 100),
            &[("A".into(), 0, 0, 100, 100), ("B".into(), 200, 0, 100, 100)],
            &b,
        );
        assert_eq!(plan.len(), 2);
        for entry in &plan {
            assert_eq!(entry.canvas.size, [300.0, 100.0]);
        }
        assert_eq!(plan[0].canvas.clip, (0, 0, 100, 100));
        assert_eq!(plan[1].canvas.clip, (200, 0, 100, 100));
    }
}
