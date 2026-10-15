//! Bounded, display-independent timing for the optional X11 wallpaper layer.
use std::time::{Duration, Instant};

/// Shared by picker admission and the compositor. Explicit paths are
/// authoritative: a missing override never silently selects a bundled image.
pub(crate) fn resolve_earth_texture(configured: &str) -> Option<String> {
    use std::path::PathBuf;
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|path| path.join(".local/share"))
        });
    let executable = std::env::current_exe().ok();
    let data_dirs = std::env::var_os("XDG_DATA_DIRS")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    let data_dirs: Vec<PathBuf> = std::env::split_paths(&data_dirs)
        .filter(|path| path.is_absolute())
        .collect();
    // A source-tree fallback is useful for cargo run, but must not leak a
    // builder's checkout path into a production release's lookup policy.
    let source = if cfg!(debug_assertions) {
        option_env!("CARGO_MANIFEST_DIR").map(PathBuf::from)
    } else {
        None
    };
    let candidates = earth_texture_candidates(
        data_home.as_deref(),
        executable.as_deref(),
        &data_dirs,
        source.as_deref(),
    );
    resolve_earth_texture_from(configured, &candidates, std::path::Path::is_file)
        .and_then(|path| path.into_os_string().into_string().ok())
}

fn earth_texture_candidates(
    data_home: Option<&std::path::Path>,
    executable: Option<&std::path::Path>,
    data_dirs: &[std::path::PathBuf],
    source: Option<&std::path::Path>,
) -> Vec<std::path::PathBuf> {
    const ASSET: &str = "jwm/astronomy/earth-nasa-january.jpg";
    let mut paths = Vec::new();
    if let Some(home) = data_home {
        paths.push(home.join(ASSET));
    }
    if let Some(prefix) = executable
        .and_then(|path| path.parent())
        .and_then(|path| path.parent())
    {
        paths.push(prefix.join("share").join(ASSET));
    }
    paths.extend(data_dirs.iter().map(|path| path.join(ASSET)));
    if let Some(source) = source {
        paths.push(source.join("assets/astronomy/earth-nasa-january.jpg"));
    }
    paths
}

fn resolve_earth_texture_from(
    configured: &str,
    candidates: &[std::path::PathBuf],
    mut is_file: impl FnMut(&std::path::Path) -> bool,
) -> Option<std::path::PathBuf> {
    if !configured.is_empty() {
        let path = std::path::Path::new(configured);
        return is_file(path).then(|| path.to_path_buf());
    }
    candidates.iter().find(|path| is_file(path)).cloned()
}

pub(crate) const FRAME_INTERVAL: Duration = Duration::from_nanos(66_666_667);
pub(crate) const POWER_QUERY_INTERVAL: Duration = Duration::from_secs(2);
#[derive(Debug)]
pub(crate) struct WallpaperClock {
    last_frame: Option<Instant>,
    paused: bool,
}

impl Default for WallpaperClock {
    fn default() -> Self {
        Self {
            last_frame: None,
            paused: true,
        }
    }
}

impl WallpaperClock {
    pub(crate) fn set_paused(&mut self, paused: bool) {
        if self.paused != paused {
            self.paused = paused;
            self.last_frame = None;
        }
    }
    pub(crate) fn deadline(&self, now: Instant) -> Option<Duration> {
        if self.paused {
            return None;
        }
        Some(self.last_frame.map_or(Duration::ZERO, |last| {
            FRAME_INTERVAL.saturating_sub(now.saturating_duration_since(last))
        }))
    }
    /// Admit only a due frame; never schedule catch-up bursts after a pause.
    pub(crate) fn tick(&mut self, now: Instant) -> bool {
        if self.deadline(now) != Some(Duration::ZERO) {
            return false;
        }
        self.last_frame = Some(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capped_frame_clock_does_not_follow_high_refresh_damage() {
        let t = Instant::now();
        let mut c = WallpaperClock::default();
        assert_eq!(c.deadline(t), None);
        assert_eq!(POWER_QUERY_INTERVAL, Duration::from_secs(2));
        c.set_paused(false);
        assert!(c.tick(t));
        for ms in 1..67 {
            assert!(!c.tick(t + Duration::from_millis(ms)));
        }
        assert!(c.tick(t + FRAME_INTERVAL));
        assert_eq!(c.deadline(t + FRAME_INTERVAL), Some(FRAME_INTERVAL));
    }
    #[test]
    fn pause_and_resume_do_not_schedule_catchup_frames() {
        let t = Instant::now();
        let mut c = WallpaperClock::default();
        c.set_paused(false);
        assert!(c.tick(t));
        assert!(c.tick(t + FRAME_INTERVAL));
        c.set_paused(true);
        assert!(!c.tick(t + Duration::from_secs(100)));
        c.set_paused(false);
        assert!(c.tick(t + Duration::from_secs(200)));
        assert_eq!(
            c.deadline(t + Duration::from_secs(200)),
            Some(FRAME_INTERVAL)
        );
    }
    #[test]
    fn stalled_frame_is_bounded() {
        let t = Instant::now();
        let mut c = WallpaperClock::default();
        c.set_paused(false);
        c.tick(t);
        c.tick(t + Duration::from_secs(1000));
        assert_eq!(
            c.deadline(t + Duration::from_secs(1000)),
            Some(FRAME_INTERVAL)
        );
    }
}

/// Approximate astronomical solar direction in Earth-fixed coordinates.
/// UTC determines illumination. The configured display timezone only sets a viewing angle.
pub(crate) fn solar_direction(unix_seconds: f64) -> [f32; 3] {
    let days = unix_seconds / 86400.0 - 10957.5; // days since J2000 noon
    let mean_longitude = (280.460 + 0.9856474 * days).to_radians();
    let anomaly = (357.528 + 0.9856003 * days).to_radians();
    let longitude =
        mean_longitude + (1.915 * anomaly.sin() + 0.020 * (2.0 * anomaly).sin()).to_radians();
    let obliquity = (23.439 - 0.0000004 * days).to_radians();
    let declination = (obliquity.sin() * longitude.sin()).asin();
    let right_ascension = (obliquity.cos() * longitude.sin()).atan2(longitude.cos());
    let equation = (mean_longitude - right_ascension + std::f64::consts::PI)
        .rem_euclid(std::f64::consts::TAU)
        - std::f64::consts::PI;
    let utc_angle = unix_seconds.rem_euclid(86400.0) / 86400.0 * std::f64::consts::TAU;
    let subsolar = std::f64::consts::PI - utc_angle - equation;
    [
        (declination.cos() * subsolar.sin()) as f32,
        declination.sin() as f32,
        (declination.cos() * subsolar.cos()) as f32,
    ]
}

/// Timezone-derived central meridian, plus one gentle orbit per day. This is
/// a presentation choice, never an inferred GPS location or physical spin rate.
pub(crate) fn viewing_longitude(unix_seconds: f64, utc_offset_seconds: i32) -> f32 {
    let zone = f64::from(utc_offset_seconds.clamp(-86400, 86400)) / 86400.0;
    ((zone + unix_seconds.rem_euclid(86400.0) / 86400.0) * std::f64::consts::TAU) as f32
}

#[cfg(test)]
mod astronomy_tests {
    use super::*;
    #[test]
    fn noon_equinox_lights_greenwich_and_midnight_opposes() {
        // 2026-03-20 12:00 UTC
        let noon = solar_direction(1774008000.0);
        assert!(noon[2] > 0.99, "{noon:?}");
        assert!(noon[1].abs() < 0.02, "{noon:?}");
        let midnight = solar_direction(1774051200.0);
        assert!(midnight[2] < -0.99);
    }
    #[test]
    fn solstices_tilt_solar_light_and_vectors_are_normalized() {
        for (epoch, sign) in [(1782043200.0, 1.0), (1797854400.0, -1.0)] {
            let v = solar_direction(epoch);
            assert!(v[1] * sign > 0.39, "{v:?}");
            assert!((v.iter().map(|v| v * v).sum::<f32>() - 1.0).abs() < 0.00001);
        }
    }
    #[test]
    fn timezone_changes_view_but_not_solar_physics() {
        let t = 1774008000.0;
        let delta = viewing_longitude(t, 28800) - viewing_longitude(t, 0);
        assert!((delta - 2.0 * std::f32::consts::PI / 3.0).abs() < 0.0001);
        let minute = viewing_longitude(t + 60.0, 0) - viewing_longitude(t, 0);
        assert!((minute - std::f32::consts::TAU / 1440.0).abs() < 0.0001);
    }
}

/// Conservative bounded union coverage, including adjacent opaque monitors.
/// Rounded, translucent, shaped or transformed clients must be filtered out.
pub(crate) fn fully_covered(width: u32, height: u32, rects: &[(i32, i32, u32, u32)]) -> bool {
    if width == 0 || height == 0 || rects.len() > 128 {
        return false;
    }
    let (w, h) = (i64::from(width), i64::from(height));
    let clipped: Vec<_> = rects
        .iter()
        .filter_map(|&(x, y, rw, rh)| {
            let (x, y) = (i64::from(x), i64::from(y));
            let r = (
                x.max(0),
                y.max(0),
                (x + i64::from(rw)).min(w),
                (y + i64::from(rh)).min(h),
            );
            (r.0 < r.2 && r.1 < r.3).then_some(r)
        })
        .collect();
    let mut edges = vec![0, w];
    for r in &clipped {
        edges.extend([r.0, r.2]);
    }
    edges.sort_unstable();
    edges.dedup();
    edges.windows(2).all(|edge| {
        let mut spans: Vec<_> = clipped
            .iter()
            .filter(|r| r.0 <= edge[0] && r.2 >= edge[1])
            .map(|r| (r.1, r.3))
            .collect();
        spans.sort_unstable();
        let mut end = 0;
        for (start, next) in spans {
            if start > end {
                return false;
            }
            end = end.max(next);
        }
        end >= h
    })
}
#[cfg(test)]
mod coverage_tests {
    use super::*;
    #[test]
    fn adjacent_outputs_and_negative_clipping() {
        assert!(fully_covered(
            5120,
            1440,
            &[(0, 0, 2560, 1440), (2560, 0, 2560, 1440)]
        ));
        assert!(fully_covered(100, 100, &[(-100, -100, 200, 200)]));
        assert!(!fully_covered(
            5120,
            1440,
            &[(0, 0, 2559, 1440), (2560, 0, 2560, 1440)]
        ));
        assert!(!fully_covered(100, 100, &[(0, 0, 100, 99)]));
    }
    #[test]
    fn empty_and_unbounded_sets_never_pause() {
        assert!(!fully_covered(1, 1, &[]));
        assert!(!fully_covered(1, 1, &vec![(0, 0, 1, 1); 129]));
    }
}

#[cfg(test)]
mod asset_tests {
    use super::*;
    use std::path::{Path, PathBuf};
    #[test]
    fn earth_asset_order_tracks_user_override_real_executable_and_data_dirs() {
        let paths = earth_texture_candidates(
            Some(Path::new("/home/u/.local/share")),
            Some(Path::new("/usr/local/lib/jwm/versions/2/bin/jwm")),
            &[
                PathBuf::from("/usr/local/share"),
                PathBuf::from("/usr/share"),
            ],
            Some(Path::new("/source")),
        );
        assert_eq!(
            paths,
            [
                "/home/u/.local/share/jwm/astronomy/earth-nasa-january.jpg",
                "/usr/local/lib/jwm/versions/2/share/jwm/astronomy/earth-nasa-january.jpg",
                "/usr/local/share/jwm/astronomy/earth-nasa-january.jpg",
                "/usr/share/jwm/astronomy/earth-nasa-january.jpg",
                "/source/assets/astronomy/earth-nasa-january.jpg",
            ]
            .map(PathBuf::from)
        );
        assert_eq!(
            resolve_earth_texture_from("", &paths, |_| true),
            Some(paths[0].clone())
        );
        assert_eq!(
            resolve_earth_texture_from("", &paths, |p| p == paths[1]),
            Some(paths[1].clone())
        );
        assert_eq!(resolve_earth_texture_from("", &paths, |_| false), None);
    }
    #[test]
    fn explicit_earth_override_never_falls_back_even_when_missing() {
        let paths = [PathBuf::from("/bundled.jpg")];
        assert_eq!(
            resolve_earth_texture_from("/custom.jpg", &paths, |_| true),
            Some(PathBuf::from("/custom.jpg"))
        );
        let mut checked = Vec::new();
        assert_eq!(
            resolve_earth_texture_from("/bad.jpg", &paths, |p| {
                checked.push(p.to_path_buf());
                p == paths[0]
            }),
            None
        );
        assert_eq!(checked, [PathBuf::from("/bad.jpg")]);
        assert_eq!(
            resolve_earth_texture_from(" ", &paths, |p| p == paths[0]),
            None
        );
    }
    #[test]
    fn installed_asset_selection_changes_with_release_prefix() {
        for version in ["1", "2", "1"] {
            let exe = PathBuf::from(format!("/releases/{version}/bin/jwm"));
            let paths = earth_texture_candidates(None, Some(&exe), &[], None);
            assert_eq!(
                resolve_earth_texture_from("", &paths, |_| true),
                Some(PathBuf::from(format!(
                    "/releases/{version}/share/jwm/astronomy/earth-nasa-january.jpg"
                )))
            );
        }
        assert!(earth_texture_candidates(None, None, &[], None).is_empty());
    }
}

#[cfg(test)]
mod timezone_boundary_tests {
    use super::*;
    #[test]
    fn utc_and_utc8_midnight_are_continuous_without_double_offset() {
        // UTC midnight and UTC+8 midnight are different instants; solar input
        // is always the original epoch, never the presentation-clock value.
        for midnight in [1774051200.0, 1774051200.0 - 28800.0] {
            let before = solar_direction(midnight - 1.0);
            let after = solar_direction(midnight + 1.0);
            assert!(
                before
                    .iter()
                    .zip(after)
                    .all(|(a, b)| (*a - b).abs() < 0.001)
            );
            for offset in [0, 28800] {
                let a = viewing_longitude(midnight - 1.0, offset);
                let b = viewing_longitude(midnight + 1.0, offset);
                assert!((a.cos() - b.cos()).abs() < 0.001);
                assert!((a.sin() - b.sin()).abs() < 0.001);
            }
            let delta = viewing_longitude(midnight, 28800) - viewing_longitude(midnight, 0);
            assert!((delta - std::f32::consts::TAU / 3.0).abs() < 0.00001);
        }
    }
    #[test]
    fn wall_clock_jump_and_resume_recompute_from_actual_instant() {
        let now = Instant::now();
        let mut clock = WallpaperClock::default();
        clock.set_paused(false);
        assert!(clock.tick(now));
        let t = 1774051200.0;
        let initial = (solar_direction(t), viewing_longitude(t, 28800));
        for jump in [86400.0 * 90.0, -86400.0 * 90.0] {
            clock.set_paused(true);
            assert!(!clock.tick(now + Duration::from_secs(3600)));
            clock.set_paused(false);
            assert!(clock.tick(now + Duration::from_secs(3600)));
            assert_ne!(solar_direction(t + jump), initial.0);
            // Reversing a wall-clock correction restores the original sample.
            assert_eq!((solar_direction(t), viewing_longitude(t, 28800)), initial);
        }
    }
}

/// Coverage is inspected before this frame's effect ticks. A currently opaque
/// window is not a safe occluder if that tick can fade or shrink it.
pub(crate) fn occluder_stable_before_effects(
    fading_out: bool,
    animation_scale: f32,
    target_scale: f32,
    peek_pending: bool,
) -> bool {
    !fading_out
        && !peek_pending
        && animation_scale.is_finite()
        && target_scale.is_finite()
        // Match tick_fades' settled-target tolerance, avoiding a permanent
        // wake for a residual scale difference that it will never animate.
        && (animation_scale - target_scale).abs() <= 0.001
}

#[cfg(test)]
mod first_effect_frame_tests {
    use super::*;

    fn covered(fading_out: bool, scale: f32, target: f32, peek: bool) -> bool {
        let rects = if occluder_stable_before_effects(fading_out, scale, target, peek) {
            vec![(0, 0, 1920, 1080)]
        } else {
            vec![]
        };
        fully_covered(1920, 1080, &rects)
    }
    #[test]
    fn close_fade_and_scale_first_frame_do_not_hide_background() {
        assert!(covered(false, 1.0, 1.0, false));
        assert!(!covered(true, 1.0, 1.0, false));
        assert!(!covered(false, 1.0, 0.8, false));
    }
    #[test]
    fn peek_first_frame_resumes_and_completion_allows_idle_again() {
        let now = Instant::now();
        let mut clock = WallpaperClock::default();
        clock.set_paused(covered(false, 1.0, 1.0, false));
        assert_eq!(clock.deadline(now), None);
        // set_peek_mode starts a transition while opacity is still exactly 1.
        clock.set_paused(covered(false, 1.0, 1.0, true));
        assert!(clock.tick(now));
        // After peek returns to fully opaque and the transition is gone,
        // the same coverage may pause animation again (no permanent wakeups).
        clock.set_paused(covered(false, 1.0, 1.0, false));
        assert_eq!(clock.deadline(now + Duration::from_secs(1)), None);
    }
    #[test]
    fn cancelled_close_and_invalid_scale_are_conservative() {
        assert!(!covered(false, f32::NAN, 1.0, false));
        assert!(!covered(false, 1.0, f32::INFINITY, false));
        assert!(covered(false, 1.0, 1.0, false));
        assert!(covered(false, 1.0, 1.0005, false));
    }
}

/// One canvas in root framebuffer coordinates. Clip is GL bottom-left;
/// origin and size retain the original canvas even when only part is visible.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AstronomyDraw {
    pub(crate) origin: [f32; 2],
    pub(crate) size: [f32; 2],
    pub(crate) radius: f32,
    pub(crate) clip: (i32, i32, i32, i32),
}

impl AstronomyDraw {
    /// Intersection used by the production draw loop without allocating.
    pub(crate) fn repair_clip(
        &self,
        repair: Option<(i32, i32, i32, i32)>,
    ) -> Option<(i32, i32, i32, i32)> {
        let a = self.clip;
        let Some(b) = repair else {
            return Some(a);
        };
        if b.2 <= 0 || b.3 <= 0 {
            return None;
        }
        let x = i64::from(a.0).max(i64::from(b.0));
        let y = i64::from(a.1).max(i64::from(b.1));
        let right = (i64::from(a.0) + i64::from(a.2)).min(i64::from(b.0) + i64::from(b.2));
        let top = (i64::from(a.1) + i64::from(a.3)).min(i64::from(b.1) + i64::from(b.3));
        (right > x && top > y).then_some((x as i32, y as i32, (right - x) as i32, (top - y) as i32))
    }
}

/// Bounded, deterministic layout. Span, absent geometry, and excessive output
/// counts use the compatible root canvas. Known but invisible outputs do not.
/// Outputs use X11 top-left coordinates; repair uses GL bottom-left coordinates.
pub(crate) fn astronomy_draw_plan(
    screen: (u32, u32),
    outputs: &[(i32, i32, u32, u32)],
    per_output: bool,
    repair: Option<(i32, i32, i32, i32)>,
) -> Vec<AstronomyDraw> {
    let (sw, sh) = (i64::from(screen.0), i64::from(screen.1));
    if sw == 0 || sh == 0 || sw > i64::from(i32::MAX) || sh > i64::from(i32::MAX) {
        return Vec::new();
    }
    let independent = per_output && !outputs.is_empty() && outputs.len() <= 128;
    let mut canvases = if independent {
        outputs.to_vec()
    } else {
        vec![(0, 0, screen.0, screen.1)]
    };
    // Mirrors draw once. Partial overlaps have stable lexicographic painter
    // order, independent of server enumeration; later canvases win.
    canvases.sort_unstable();
    canvases.dedup();
    canvases
        .into_iter()
        .filter_map(|(x, y, w, h)| {
            if w == 0 || h == 0 {
                return None;
            }
            let (x, y, w64, h64) = (i64::from(x), i64::from(y), i64::from(w), i64::from(h));
            let bottom = sh - y - h64;
            let (left, low, right, high) = (
                x.max(0),
                bottom.max(0),
                (x + w64).min(sw),
                (bottom + h64).min(sh),
            );
            if left >= right || low >= high {
                return None;
            }
            let mut draw = AstronomyDraw {
                origin: [x as f32, bottom as f32],
                size: [w as f32, h as f32],
                radius: if independent {
                    w.min(h) as f32 * 0.42
                } else {
                    h as f32 * 0.42
                },
                clip: (
                    left as i32,
                    low as i32,
                    (right - left) as i32,
                    (high - low) as i32,
                ),
            };
            draw.clip = draw.repair_clip(repair)?;
            Some(draw)
        })
        .collect()
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    #[test]
    fn span_and_missing_geometry_preserve_root_canvas() {
        let span = astronomy_draw_plan((5120, 1440), &[], false, None);
        assert_eq!(span[0].origin, [0.0, 0.0]);
        assert_eq!(span[0].size, [5120.0, 1440.0]);
        assert_eq!(span[0].radius, 1440.0 * 0.42);
        assert_eq!(span, astronomy_draw_plan((5120, 1440), &[], true, None));
        assert_eq!(
            span,
            astronomy_draw_plan((5120, 1440), &vec![(0, 0, 1, 1); 129], true, None)
        );
    }
    #[test]
    fn dual_and_portrait_keep_independent_centers() {
        let p = astronomy_draw_plan(
            (4000, 2560),
            &[(0, 0, 2560, 1440), (2560, 0, 1440, 2560)],
            true,
            None,
        );
        assert_eq!(p[0].origin, [0.0, 1120.0]);
        assert_eq!(p[1].origin, [2560.0, 0.0]);
        assert_eq!(p[0].radius, p[1].radius);
        assert_eq!(p[1].radius, 1440.0 * 0.42);
    }
    #[test]
    fn clipped_output_and_repair_never_recenter_canvas() {
        let p = astronomy_draw_plan(
            (100, 100),
            &[(-50, -20, 100, 100)],
            true,
            Some((10, 30, 20, 40)),
        );
        assert_eq!(p[0].origin, [-50.0, 20.0]);
        assert_eq!(p[0].size, [100.0, 100.0]);
        assert_eq!(p[0].clip, (10, 30, 20, 40));
        assert!(
            astronomy_draw_plan(
                (100, 100),
                &[(200, 0, 100, 100), (0, 0, 0, 100)],
                true,
                None
            )
            .is_empty()
        );
        assert!(astronomy_draw_plan((100, 100), &[], true, Some((0, 0, -1, 10))).is_empty());
    }
    #[test]
    fn mirrors_and_overlap_have_stable_order() {
        let a = [(0, 0, 80, 80), (20, 20, 80, 80), (0, 0, 80, 80)];
        let b = [a[1], a[0], a[2]];
        let p = astronomy_draw_plan((100, 100), &a, true, None);
        assert_eq!(p.len(), 2);
        assert_eq!(p, astronomy_draw_plan((100, 100), &b, true, None));
        assert_eq!(p[1].origin, [20.0, 0.0]);
    }
    #[test]
    fn holes_stay_empty_and_arithmetic_is_bounded() {
        let p = astronomy_draw_plan(
            (200, 200),
            &[(0, 0, 100, 200), (100, 100, 100, 100)],
            true,
            Some((100, 100, 100, 100)),
        );
        assert!(p.is_empty());
        assert!(astronomy_draw_plan((0, 100), &[], true, None).is_empty());
        assert!(astronomy_draw_plan((u32::MAX, 100), &[], true, None).is_empty());
        let p = astronomy_draw_plan(
            (100, 100),
            &[(i32::MIN, i32::MIN, u32::MAX, u32::MAX)],
            true,
            None,
        );
        assert_eq!(p[0].clip, (0, 0, 100, 100));
    }
}
