//! Bounded, display-independent timing for the optional X11 wallpaper layer.
use std::time::{Duration, Instant};

/// Shared by picker admission and the compositor. Explicit paths are
/// authoritative: a missing override never silently selects a bundled image.
pub(crate) fn resolve_earth_texture(configured: &str) -> Option<String> {
    resolve_body_texture(configured, "earth-nasa-january.jpg")
}

pub(crate) fn resolve_moon_texture(configured: &str) -> Option<String> {
    resolve_body_texture(configured, "moon-nasa-lroc.jpg")
}

pub(crate) fn resolve_cloud_texture() -> Option<String> {
    resolve_body_texture("", "earth-nasa-clouds.jpg")
}

fn resolve_body_texture(configured: &str, asset: &str) -> Option<String> {
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
    let candidates = body_texture_candidates(
        asset,
        data_home.as_deref(),
        executable.as_deref(),
        &data_dirs,
        source.as_deref(),
    );
    resolve_earth_texture_from(configured, &candidates, std::path::Path::is_file)
        .and_then(|path| path.into_os_string().into_string().ok())
}

#[cfg(test)]
fn earth_texture_candidates(
    data_home: Option<&std::path::Path>,
    executable: Option<&std::path::Path>,
    data_dirs: &[std::path::PathBuf],
    source: Option<&std::path::Path>,
) -> Vec<std::path::PathBuf> {
    body_texture_candidates(
        "earth-nasa-january.jpg",
        data_home,
        executable,
        data_dirs,
        source,
    )
}

fn body_texture_candidates(
    asset: &str,
    data_home: Option<&std::path::Path>,
    executable: Option<&std::path::Path>,
    data_dirs: &[std::path::PathBuf],
    source: Option<&std::path::Path>,
) -> Vec<std::path::PathBuf> {
    let asset_path = std::path::Path::new("jwm/astronomy").join(asset);
    let mut paths = Vec::new();
    if let Some(home) = data_home {
        paths.push(home.join(&asset_path));
    }
    if let Some(prefix) = executable
        .and_then(|path| path.parent())
        .and_then(|path| path.parent())
    {
        paths.push(prefix.join("share").join(&asset_path));
    }
    paths.extend(data_dirs.iter().map(|path| path.join(&asset_path)));
    if let Some(source) = source {
        paths.push(source.join("assets/astronomy").join(asset));
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

/// Whether every animated canvas pixel is hidden. Span (including the draw
/// fallback for missing/excessive output geometry) retains whole-root semantics:
/// its sky in desktop gaps can be visible in a root screenshot or recording.
/// Per-output gaps are static, so only the current draw-plan clips are targets.
pub(crate) fn astronomy_fully_covered(
    screen: (u32, u32),
    outputs: &[(i32, i32, u32, u32)],
    per_output: bool,
    occluders: &[(i32, i32, u32, u32)],
) -> bool {
    if occluders.len() > 128 {
        return false;
    }
    if !per_output || outputs.is_empty() || outputs.len() > 128 {
        return fully_covered(screen.0, screen.1, occluders);
    }
    let plan = astronomy_draw_plan(screen, outputs, true, None);
    if plan.is_empty() {
        // Known but invalid/outside geometry must not assert invisible output.
        return false;
    }
    type Rect = (i64, i64, i64, i64);
    let (width, height) = (i64::from(screen.0), i64::from(screen.1));
    // Draw clips use GL bottom-left. Convert to X11 top-left once so windows
    // and targets share one coordinate system, without shifting root origins.
    let mut targets: Vec<Rect> = plan
        .iter()
        .map(|draw| {
            let (x, y, w, h) = draw.clip;
            let top = height - i64::from(y) - i64::from(h);
            (
                i64::from(x),
                top,
                i64::from(x) + i64::from(w),
                top + i64::from(h),
            )
        })
        .collect();
    let mut covers: Vec<Rect> = occluders
        .iter()
        .filter_map(|&(x, y, w, h)| {
            let (x, y) = (i64::from(x), i64::from(y));
            let r = (
                x.max(0),
                y.max(0),
                (x + i64::from(w)).min(width),
                (y + i64::from(h)).min(height),
            );
            (r.0 < r.2 && r.1 < r.3).then_some(r)
        })
        .collect();
    let mut edges = Vec::with_capacity((targets.len() + covers.len()) * 2);
    for r in targets.iter().chain(&covers) {
        edges.extend([r.0, r.2]);
    }
    edges.sort_unstable();
    edges.dedup();
    // Sort by y only once. Each x slab filters this ordering and merges in
    // linear time; no per-output coverage recursion or per-slab sorting.
    targets.sort_unstable_by_key(|r| (r.1, r.3));
    covers.sort_unstable_by_key(|r| (r.1, r.3));
    fn merged_spans(rects: &[Rect], left: i64, right: i64, spans: &mut Vec<(i64, i64)>) {
        spans.clear();
        for r in rects.iter().filter(|r| r.0 <= left && r.2 >= right) {
            match spans.last_mut() {
                Some(last) if r.1 <= last.1 => last.1 = last.1.max(r.3),
                _ => spans.push((r.1, r.3)),
            }
        }
    }
    let mut wanted = Vec::with_capacity(targets.len());
    let mut covered = Vec::with_capacity(covers.len());
    for edge in edges.windows(2) {
        merged_spans(&targets, edge[0], edge[1], &mut wanted);
        if wanted.is_empty() {
            continue;
        }
        merged_spans(&covers, edge[0], edge[1], &mut covered);
        let mut index = 0;
        for &(start, end) in &wanted {
            while index < covered.len() && covered[index].1 <= start {
                index += 1;
            }
            if index == covered.len() || covered[index].0 > start || covered[index].1 < end {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod output_coverage_tests {
    use super::*;
    const L: [(i32, i32, u32, u32); 2] = [(0, 0, 100, 200), (100, 100, 100, 100)];
    #[test]
    fn per_output_pauses_in_gaps_but_span_preserves_root_capture() {
        assert!(astronomy_fully_covered((200, 200), &L, true, &L));
        assert!(!astronomy_fully_covered((200, 200), &L, false, &L));
        assert!(!astronomy_fully_covered((200, 200), &[], true, &L));
        assert!(!astronomy_fully_covered(
            (200, 200),
            &vec![L[0]; 129],
            true,
            &L
        ));
        assert!(astronomy_fully_covered(
            (200, 200),
            &vec![L[0]; 129],
            true,
            &[(0, 0, 200, 200)]
        ));
    }
    #[test]
    fn current_topology_and_one_pixel_holes_resume() {
        assert!(astronomy_fully_covered((200, 200), &L, true, &L));
        let exposed = [(0, 0, 100, 200), (100, 99, 100, 101)];
        assert!(!astronomy_fully_covered((200, 200), &exposed, true, &L));
        let hole = [(0, 0, 100, 200), (100, 101, 100, 99)];
        assert!(!astronomy_fully_covered((200, 200), &L, true, &hole));
        assert!(!astronomy_fully_covered(
            (200, 200),
            &L,
            true,
            &vec![L[0]; 129]
        ));
    }
    #[test]
    fn negative_origins_mirrors_and_overlaps_are_clipped_without_translation() {
        let outputs = [(-50, -50, 100, 100), (0, 0, 100, 100), (0, 0, 100, 100)];
        assert!(astronomy_fully_covered(
            (100, 100),
            &outputs,
            true,
            &[(0, 0, 50, 100), (50, 0, 50, 100)]
        ));
        assert!(!astronomy_fully_covered(
            (100, 100),
            &outputs,
            true,
            &[(0, 0, 49, 100), (50, 0, 50, 100)]
        ));
        assert!(!astronomy_fully_covered(
            (100, 100),
            &[(100, 100, 1, 1), (0, 0, 0, 100)],
            true,
            &[(0, 0, 100, 100)]
        ));
        assert!(!astronomy_fully_covered((0, 100), &outputs, true, &outputs));
    }
    #[test]
    fn adjacent_window_unions_cover_one_output_and_disjoint_targets() {
        assert!(astronomy_fully_covered(
            (100, 100),
            &[(0, 0, 100, 100)],
            true,
            &[(0, 0, 100, 50), (0, 50, 50, 50), (50, 50, 50, 50)]
        ));
        assert!(astronomy_fully_covered(
            (100, 100),
            &[(0, 0, 100, 20), (0, 80, 100, 20)],
            true,
            &[(0, 0, 100, 100)]
        ));
        assert!(!astronomy_fully_covered(
            (100, 100),
            &[(0, 0, 100, 100)],
            true,
            &[(0, 0, 100, 49), (0, 50, 100, 50)]
        ));
    }
    #[test]
    fn bounded_sweep_matches_small_pixel_oracle() {
        // Deterministic pseudo-random rectangles exercise unions and clipping.
        let mut seed = 19u64;
        for _ in 0..1000 {
            let mut next = || {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                (seed >> 32) as u32
            };
            let mut outputs = Vec::new();
            let mut covers = Vec::new();
            for _ in 0..1 + next() % 4 {
                outputs.push((
                    (next() % 12) as i32 - 2,
                    (next() % 12) as i32 - 2,
                    next() % 12,
                    next() % 12,
                ));
            }
            for _ in 0..next() % 8 {
                covers.push((
                    (next() % 12) as i32 - 2,
                    (next() % 12) as i32 - 2,
                    next() % 12,
                    next() % 12,
                ));
            }
            let inside = |x: i32, y: i32, r: &(i32, i32, u32, u32)| {
                x >= r.0
                    && y >= r.1
                    && i64::from(x) < i64::from(r.0) + i64::from(r.2)
                    && i64::from(y) < i64::from(r.1) + i64::from(r.3)
            };
            let mut any = false;
            let mut expected = true;
            for y in 0..8 {
                for x in 0..8 {
                    if outputs.iter().any(|r| inside(x, y, r)) {
                        any = true;
                        expected &= covers.iter().any(|r| inside(x, y, r));
                    }
                }
            }
            assert_eq!(
                astronomy_fully_covered((8, 8), &outputs, true, &covers),
                any && expected,
                "outputs={outputs:?}, covers={covers:?}"
            );
        }
    }
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

/// Decorative, independently rotating materials; not a lunar ephemeris.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum CelestialBody {
    #[default]
    Earth,
    Moon,
}

/// Connector identity is independent of enumeration and desktop coordinates.
/// Explicit assignments also survive a different set of connected displays.
pub(crate) fn celestial_assignments(
    names: &[String],
    overrides: &std::collections::BTreeMap<String, String>,
) -> Vec<CelestialBody> {
    let mut sorted: Vec<&str> = names.iter().take(128).map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.dedup();
    names
        .iter()
        .take(128)
        .map(|name| match overrides.get(name).map(String::as_str) {
            Some("moon") => CelestialBody::Moon,
            Some("earth") => CelestialBody::Earth,
            _ => {
                if sorted.binary_search(&name.as_str()).unwrap_or(0) % 2 == 0 {
                    CelestialBody::Earth
                } else {
                    CelestialBody::Moon
                }
            }
        })
        .collect()
}

/// Reduce in f64 before casting, avoiding precision loss at Unix timestamps.
pub(crate) fn visual_rotation(unix: f64, minutes: u32) -> f32 {
    let period = f64::from(minutes.clamp(1, 525_600)) * 60.0;
    if !unix.is_finite() {
        return 0.0;
    }
    (unix.rem_euclid(period) / period * std::f64::consts::TAU) as f32
}

/// The timezone is a fixed observer meridian, not a second clock correction.
/// UTC alone advances rotation; offset is added once as 15 degrees per hour.
pub(crate) fn celestial_longitude(unix: f64, minutes: u32, utc_offset_seconds: i32) -> f32 {
    let phase = f64::from(visual_rotation(unix, minutes));
    let meridian =
        f64::from(utc_offset_seconds.clamp(-86400, 86400)) / 86400.0 * std::f64::consts::TAU;
    (phase + meridian).rem_euclid(std::f64::consts::TAU) as f32
}

pub(crate) fn presentation_offset(
    use_system: bool,
    configured_hours: i32,
    system_offset: Option<i32>,
) -> i32 {
    if use_system {
        if let Some(offset) = system_offset.filter(|o| (-86400..=86400).contains(o)) {
            return offset;
        }
    }
    configured_hours.clamp(-12, 14) * 3600
}

pub(crate) fn wall_clock_seconds(time: std::time::SystemTime) -> f64 {
    match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => duration.as_secs_f64(),
        Err(error) => -error.duration().as_secs_f64(),
    }
}

#[cfg(test)]
mod celestial_tests {
    use super::*;
    use std::collections::BTreeMap;
    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }
    #[test]
    fn moon_asset_has_distinct_version_local_path_and_authoritative_override() {
        let exe = std::path::Path::new("/versions/2/bin/jwm");
        let paths = body_texture_candidates("moon-nasa-lroc.jpg", None, Some(exe), &[], None);
        assert_eq!(
            paths,
            [std::path::PathBuf::from(
                "/versions/2/share/jwm/astronomy/moon-nasa-lroc.jpg"
            )]
        );
        assert_eq!(
            resolve_earth_texture_from("/missing-moon.jpg", &paths, |p| p == paths[0]),
            None
        );
    }
    #[test]
    fn connector_order_and_explicit_hotplug_assignments() {
        use CelestialBody::*;
        let map = BTreeMap::new();
        assert_eq!(
            celestial_assignments(&names(&["HDMI-1", "DP-1"]), &map),
            [Moon, Earth]
        );
        assert_eq!(
            celestial_assignments(&names(&["DP-1", "HDMI-1"]), &map),
            [Earth, Moon]
        );
        assert_eq!(celestial_assignments(&names(&["HDMI-1"]), &map), [Earth]);
        assert_eq!(
            celestial_assignments(&names(&["A", "B", "C", "D"]), &map),
            [Earth, Moon, Earth, Moon]
        );
        let map = BTreeMap::from([
            ("HDMI-1".into(), "moon".into()),
            ("DP-1".into(), "earth".into()),
        ]);
        assert_eq!(celestial_assignments(&names(&["HDMI-1"]), &map), [Moon]);
        assert_eq!(
            celestial_assignments(&names(&["HDMI-1", "DP-1"]), &map),
            [Moon, Earth]
        );
    }
    #[test]
    fn wall_clock_periods_offsets_and_restart_are_independent() {
        let tau = std::f32::consts::TAU;
        let distance = |a: f32, b: f32| {
            ((a - b + std::f32::consts::PI).rem_euclid(tau) - std::f32::consts::PI).abs()
        };
        for (period, offset) in [
            (1440, 0),
            (1440, 28800),
            (39343, 0),
            (20, 20700),
            (525600, -12600),
        ] {
            let t = 1_791_000_123.25;
            let initial = celestial_longitude(t, period, offset);
            assert_eq!(initial, celestial_longitude(t, period, offset)); // fresh process has no uptime origin
            assert!(
                distance(
                    initial,
                    celestial_longitude(t + f64::from(period) * 60.0, period, offset)
                ) < 1e-5
            );
            assert!(
                distance(
                    initial + tau / 4.0,
                    celestial_longitude(t + f64::from(period) * 15.0, period, offset)
                ) < 1e-5
            );
        }
        // One timezone change is 15 degrees, independent of custom spin period.
        for period in [20, 1440, 39343] {
            let a = celestial_longitude(1000.0, period, 19800);
            let b = celestial_longitude(1000.0, period, 23400);
            assert!(distance(a + tau / 24.0, b) < 1e-5);
        }
        assert!(distance(celestial_longitude(-21600.0, 1440, 0), tau * 0.75) < 1e-5);
        for t in [-1.0e12, 1.0e12, f64::MAX, f64::NAN, f64::INFINITY] {
            assert!(celestial_longitude(t, 39343, 20700).is_finite());
        }
        assert_eq!(presentation_offset(true, 8, Some(19800)), 19800);
        assert_eq!(presentation_offset(true, 8, Some(20700)), 20700);
        assert_eq!(presentation_offset(true, 8, Some(-12600)), -12600);
        assert_eq!(presentation_offset(true, 8, None), 28800);
        assert_eq!(presentation_offset(true, 8, Some(i32::MAX)), 28800);
        assert_eq!(presentation_offset(false, 8, Some(0)), 28800);
        let epoch = std::time::UNIX_EPOCH;
        assert_eq!(
            wall_clock_seconds(epoch - std::time::Duration::from_millis(1500)),
            -1.5
        );
        assert_eq!(
            wall_clock_seconds(epoch + std::time::Duration::from_millis(1500)),
            1.5
        );
    }

    #[test]
    fn independent_rotation_is_bounded_periodic_and_visible() {
        let t = 1_791_000_000.0;
        assert_eq!(visual_rotation(t, 20), visual_rotation(t + 1200.0, 20));
        assert_ne!(visual_rotation(t + 10.0, 20), visual_rotation(t, 20));
        assert_ne!(visual_rotation(t + 10.0, 20), visual_rotation(t + 10.0, 30));
        assert_eq!(visual_rotation(f64::NAN, 0), 0.0);
    }
}

/// Required-image adoption waits for full repair. Optional-image readiness must
/// never suppress drawing an already-visible material on ordinary client damage.
pub(crate) fn material_load_frame(
    required_pending: bool,
    optional_pending: bool,
    frame_changed: bool,
) -> (bool, bool) {
    (
        !required_pending || frame_changed,
        optional_pending && frame_changed,
    )
}

#[cfg(test)]
mod material_load_tests {
    use super::*;
    #[test]
    fn pending_clouds_never_expose_static_patches_between_ticks() {
        assert_eq!(material_load_frame(false, true, true), (true, true));
        assert_eq!(material_load_frame(false, true, false), (true, false));
        assert_eq!(material_load_frame(true, true, false), (false, false));
        assert_eq!(material_load_frame(true, true, true), (true, true));
        assert_eq!(material_load_frame(false, false, false), (true, false));
    }
}
