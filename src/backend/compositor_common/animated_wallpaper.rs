//! Bounded, display-independent timing for the optional X11 wallpaper layer.
use std::time::{Duration, Instant};

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
/// UTC determines illumination. Local timezone is only a coarse viewing angle.
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
