//! Bounded, display-independent timing for the optional X11 wallpaper layer.
use std::time::{Duration, Instant};

/// Decorative sky travel, anchored to Unix UTC rather than process uptime.
/// Reduce in f64 before trigonometry: never upload a large epoch as an f32.
/// Zero is an explicit static sentinel, preserving the original crop exactly.
pub(crate) fn sky_rotation(unix_seconds: f64, minutes: u32) -> [f32; 2] {
    if minutes == 0 || !unix_seconds.is_finite() {
        return [0.0, 0.0];
    }
    let period = f64::from(minutes.min(525600)) * 60.0;
    let phase = unix_seconds.rem_euclid(period) / period * std::f64::consts::TAU;
    let (sin, cos) = phase.sin_cos();
    [cos as f32, sin as f32]
}

/// Shared by picker admission and the compositor. Explicit paths are
/// authoritative: a missing override never silently selects a bundled image.
pub(crate) fn resolve_earth_texture(configured: &str) -> Option<String> {
    resolve_body_texture(configured, "earth-nasa-october.jpg")
}

pub(crate) fn resolve_moon_texture(configured: &str) -> Option<String> {
    resolve_body_texture(configured, "moon-nasa-lroc.jpg")
}

pub(crate) fn resolve_cloud_texture() -> Option<String> {
    resolve_body_texture("", "earth-nasa-clouds.jpg")
}

pub(crate) fn resolve_sky_texture() -> Option<String> {
    resolve_body_texture("", "milkyway-nasa-gaia.jpg")
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
        "earth-nasa-october.jpg",
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
        if asset == "earth-nasa-october.jpg" {
            // Preserve an existing user-owned legacy default override. Version
            // assets still prefer October; explicit paths remain authoritative.
            paths.push(home.join("jwm/astronomy/earth-nasa-january.jpg"));
        }
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

/// Keep the fastest visible decorative rotation below roughly half a pixel per
/// update, bounded to 1–15 Hz. Physical sunlight still refreshes at least once a
/// second. Match the shader's period clamping, including the static-sky sentinel.
pub(crate) fn astronomy_frame_interval(
    radius: f32,
    earth_minutes: Option<u32>,
    moon_minutes: Option<u32>,
    sky_minutes: u32,
) -> Duration {
    let radius = if radius.is_finite() {
        f64::from(radius.max(0.0))
    } else {
        0.0
    };
    let fastest = [
        earth_minutes,
        moon_minutes,
        (sky_minutes != 0).then_some(sky_minutes),
    ]
    .into_iter()
    .flatten()
    .map(|m| m.clamp(1, 525_600))
    .min();
    let hz = fastest.map_or(1.0, |minutes| {
        (radius * std::f64::consts::TAU / (f64::from(minutes) * 60.0) / 0.5)
            .ceil()
            .clamp(1.0, 15.0)
    });
    Duration::from_secs_f64(1.0 / hz).max(FRAME_INTERVAL)
}

#[derive(Debug)]
pub(crate) struct WallpaperClock {
    last_frame: Option<Instant>,
    interval: Duration,
    paused: bool,
}

impl Default for WallpaperClock {
    fn default() -> Self {
        Self {
            last_frame: None,
            interval: FRAME_INTERVAL,
            paused: true,
        }
    }
}

impl WallpaperClock {
    pub(crate) fn set_interval(&mut self, interval: Duration) {
        self.interval = interval.clamp(FRAME_INTERVAL, Duration::from_secs(1));
    }
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
            self.interval
                .saturating_sub(now.saturating_duration_since(last))
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
    fn astronomy_cadence_follows_visible_speed_and_bounds() {
        assert_eq!(
            astronomy_frame_interval(1000.0, None, None, 0),
            Duration::from_secs(1)
        );
        assert_eq!(
            astronomy_frame_interval(1000.0, Some(1440), None, 0),
            Duration::from_secs(1)
        );
        assert_eq!(
            astronomy_frame_interval(1000.0, Some(1), None, 0),
            FRAME_INTERVAL
        );
        let slow = astronomy_frame_interval(1000.0, Some(60), None, 0);
        let fast = astronomy_frame_interval(1000.0, Some(20), None, 0);
        assert!(fast < slow && fast > FRAME_INTERVAL);
        assert_eq!(
            astronomy_frame_interval(1000.0, None, Some(1), 0),
            FRAME_INTERVAL
        );
        assert_eq!(
            astronomy_frame_interval(1000.0, None, None, 1),
            FRAME_INTERVAL
        );
        assert_eq!(
            astronomy_frame_interval(f32::NAN, None, None, 0),
            Duration::from_secs(1)
        );
    }
    #[test]
    fn interval_changes_do_not_reset_or_burst_clock() {
        let now = Instant::now();
        let mut c = WallpaperClock::default();
        c.set_paused(false);
        c.set_interval(Duration::from_secs(1));
        assert!(c.tick(now));
        assert!(!c.tick(now + Duration::from_millis(900)));
        c.set_interval(FRAME_INTERVAL);
        assert!(c.tick(now + Duration::from_millis(900)));
        assert!(!c.tick(now + Duration::from_millis(901)));
        c.set_interval(Duration::ZERO);
        assert_eq!(c.interval, FRAME_INTERVAL);
        c.set_interval(Duration::from_secs(20));
        assert_eq!(c.interval, Duration::from_secs(1));
    }

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

// Bounded NOAA/Meeus solar ephemeris. UTC is the physical clock; neither a
// display timezone nor the optional decorative orbit enters these equations.
// References: https://gml.noaa.gov/grad/solcalc/main.js and
// https://gml.noaa.gov/grad/solcalc/calcdetails.html . The model is intended for
// wallpaper/recreational use, with greatest accuracy around 1800-2100.
const SOLAR_MIN_UNIX_SECONDS: f64 = -62_167_219_200.0; // 0000-01-01 UTC
const SOLAR_MAX_UNIX_SECONDS: f64 = 32_503_680_000.0; // 3000-01-01 UTC
const J2000_UNIX_SECONDS: f64 = 946_728_000.0;

fn valid_solar_time(unix_seconds: f64) -> bool {
    unix_seconds.is_finite()
        && (SOLAR_MIN_UNIX_SECONDS..=SOLAR_MAX_UNIX_SECONDS).contains(&unix_seconds)
}

/// Returns (solar declination in radians, equation of time in minutes).
/// Input is already validated or bounded by the caller.
fn solar_ephemeris(unix_seconds: f64) -> (f64, f64) {
    let t = (unix_seconds - J2000_UNIX_SECONDS) / (86_400.0 * 36_525.0);
    let mean_longitude = (280.46646 + t * (36000.76983 + t * 0.0003032))
        .rem_euclid(360.0)
        .to_radians();
    let anomaly = (357.52911 + t * (35999.05029 - 0.0001537 * t))
        .rem_euclid(360.0)
        .to_radians();
    let eccentricity = 0.016708634 - t * (0.000042037 + 0.0000001267 * t);
    let center = anomaly.sin() * (1.914602 - t * (0.004817 + 0.000014 * t))
        + (2.0 * anomaly).sin() * (0.019993 - 0.000101 * t)
        + (3.0 * anomaly).sin() * 0.000289;
    let omega = (125.04 - 1934.136 * t).to_radians();
    let apparent_longitude =
        mean_longitude + (center - 0.00569 - 0.00478 * omega.sin()).to_radians();
    let obliquity_seconds = 21.448 - t * (46.8150 + t * (0.00059 - t * 0.001813));
    let obliquity =
        (23.0 + (26.0 + obliquity_seconds / 60.0) / 60.0 + 0.00256 * omega.cos()).to_radians();
    let declination = (obliquity.sin() * apparent_longitude.sin()).asin();
    let y = (obliquity / 2.0).tan().powi(2);
    let equation = y * (2.0 * mean_longitude).sin() - 2.0 * eccentricity * anomaly.sin()
        + 4.0 * eccentricity * y * anomaly.sin() * (2.0 * mean_longitude).cos()
        - 0.5 * y * y * (4.0 * mean_longitude).sin()
        - 1.25 * eccentricity * eccentricity * (2.0 * anomaly).sin();
    (declination, 4.0 * equation.to_degrees())
}

fn solar_direction_f64(unix_seconds: f64) -> [f64; 3] {
    let (declination, equation_minutes) = solar_ephemeris(unix_seconds);
    let utc_minutes = unix_seconds.rem_euclid(86_400.0) / 60.0;
    let subsolar = (180.0 - (utc_minutes + equation_minutes) / 4.0).to_radians();
    [
        declination.cos() * subsolar.sin(),
        declination.sin(),
        declination.cos() * subsolar.cos(),
    ]
}

/// Unit solar direction in Earth-fixed coordinates: +X is 90°E, +Y north,
/// and +Z Greenwich. UTC alone controls the physical illumination. Invalid
/// clocks use J2000; finite extreme clocks are clamped to the supported range
/// so shaders never receive NaN/Inf. Event diagnostics reject such clocks.
pub(crate) fn solar_direction(unix_seconds: f64) -> [f32; 3] {
    let bounded = if unix_seconds.is_finite() {
        unix_seconds.clamp(SOLAR_MIN_UNIX_SECONDS, SOLAR_MAX_UNIX_SECONDS)
    } else {
        J2000_UNIX_SECONDS
    };
    solar_direction_f64(bounded).map(|component| component as f32)
}

/// Camera orientation for an Earth or Moon material, independent of its clock.
/// Angles are degrees; latitude/longitude select the central surface point and
/// positive roll turns the displayed surface clockwise. The animation axis is
/// expressed in body coordinates: +X is 90°E, +Y north and +Z the prime meridian.
/// Negating an axis reverses the tour without changing its period.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CelestialView {
    pub rotating: bool,
    pub latitude: f64,
    pub longitude: f64,
    pub roll: f64,
    pub axis: [f64; 3],
}

impl Default for CelestialView {
    fn default() -> Self {
        Self {
            rotating: false,
            latitude: 0.0,
            longitude: 0.0,
            roll: 0.0,
            axis: [0.0, 1.0, 0.0],
        }
    }
}

impl CelestialView {
    /// Column-major camera→body rotation, directly suitable for a GLSL mat3.
    ///
    /// M = R(axis, phase) · Ry(longitude) · Rx(-latitude) · Rz(roll).
    /// The default north axis preserves the existing longitude-only tour.
    /// Rodrigues rotation avoids an Euler-angle accumulator or pole singularity;
    /// every frame is derived from the same absolute UTC instant, including after
    /// pause, restart or a wall-clock correction. This is presentation motion:
    /// never use its phase as the physical time passed to the ephemerides.
    pub(crate) fn matrix(self, unix_seconds: f64, period_minutes: u32) -> [f32; 9] {
        let radians = |degrees: f64| {
            if degrees.is_finite() {
                degrees.rem_euclid(360.0).to_radians()
            } else {
                0.0
            }
        };
        let latitude = if self.latitude.is_finite() {
            self.latitude.clamp(-90.0, 90.0).to_radians()
        } else {
            0.0
        };
        let (slat, clat) = latitude.sin_cos();
        let (slon, clon) = radians(self.longitude).sin_cos();
        let (sroll, croll) = radians(self.roll).sin_cos();
        // Scaling before squaring handles both subnormal and near-f64::MAX
        // vectors. Malformed internal callers get the safe default north axis.
        let scale = self.axis.iter().map(|v| v.abs()).fold(0.0_f64, f64::max);
        let axis = if self.axis.iter().all(|v| v.is_finite()) && scale > 0.0 {
            let scaled = self.axis.map(|v| v / scale);
            let length = scaled.iter().map(|v| v * v).sum::<f64>().sqrt();
            scaled.map(|v| v / length)
        } else {
            [0.0, 1.0, 0.0]
        };
        let (sphase, cphase) = if self.rotating {
            visual_rotation_f64(unix_seconds, period_minutes).sin_cos()
        } else {
            (0.0, 1.0)
        };
        let mut matrix = [0.0; 9];
        for (column, [x, y, z]) in [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]
            .into_iter()
            .enumerate()
        {
            let rolled_x = croll * x - sroll * y;
            let rolled_y = sroll * x + croll * y;
            let tilted_y = clat * rolled_y + slat * z;
            let tilted_z = -slat * rolled_y + clat * z;
            let body = [
                clon * rolled_x + slon * tilted_z,
                tilted_y,
                -slon * rolled_x + clon * tilted_z,
            ];
            let rotated = if self.rotating {
                let dot = axis.iter().zip(body).map(|(a, b)| a * b).sum::<f64>();
                let cross = [
                    axis[1] * body[2] - axis[2] * body[1],
                    axis[2] * body[0] - axis[0] * body[2],
                    axis[0] * body[1] - axis[1] * body[0],
                ];
                std::array::from_fn(|i| {
                    body[i] * cphase + cross[i] * sphase + axis[i] * dot * (1.0 - cphase)
                })
            } else {
                body
            };
            for (row, value) in rotated.into_iter().enumerate() {
                matrix[column * 3 + row] = value as f32;
            }
        }
        matrix
    }
}

/// Transform a direction with the same column-major matrix used by the shader.
/// For the Moon, transform the UTC phase light together with the surface normals:
/// dot(M·normal, M·light) = dot(normal, light), preserving the visible real phase
/// while decorative rotation reveals any part of the texture. Earth sunlight
/// instead stays in its geographic body frame, so each place keeps real daylight.
pub(crate) fn transform_direction(matrix: &[f32; 9], vector: [f32; 3]) -> [f32; 3] {
    std::array::from_fn(|row| {
        matrix[row] * vector[0] + matrix[3 + row] * vector[1] + matrix[6 + row] * vector[2]
    })
}

/// Geographic center meridian in radians, east-positive. Fixed mode returns
/// only the configured longitude and is independent of time, period, and
/// timezone. Rotation is a deliberate presentation option, never solar time.
/// Finite longitudes wrap to [-180°, 180°); invalid ones fall back to Guangzhou (113.28°E).
#[allow(dead_code)] // Longitude-only compatibility helper and independent regression baseline.
pub(crate) fn earth_view_longitude(
    unix_seconds: f64,
    period_minutes: u32,
    longitude_degrees: f64,
    rotating: bool,
) -> f32 {
    let longitude = if longitude_degrees.is_finite() {
        (longitude_degrees.rem_euclid(360.0) + 180.0).rem_euclid(360.0) - 180.0
    } else {
        113.28
    };
    let meridian = longitude.to_radians();
    if rotating {
        (meridian + f64::from(visual_rotation(unix_seconds, period_minutes)))
            .rem_euclid(std::f64::consts::TAU) as f32
    } else {
        meridian as f32
    }
}

#[cfg(test)]
mod celestial_view_tests {
    use super::*;

    fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
        a.into_iter().zip(b).map(|(a, b)| a * b).sum()
    }

    fn assert_direction(actual: [f32; 3], expected: [f32; 3]) {
        assert!(
            actual
                .into_iter()
                .zip(expected)
                .all(|(a, b)| (a - b).abs() < 2e-6),
            "actual {actual:?}, expected {expected:?}"
        );
    }

    fn camera_direction(matrix: &[f32; 9], body: [f32; 3]) -> [f32; 3] {
        std::array::from_fn(|column| {
            dot(
                [
                    matrix[column * 3],
                    matrix[column * 3 + 1],
                    matrix[column * 3 + 2],
                ],
                body,
            )
        })
    }

    #[test]
    fn cardinal_axes_and_roll_cover_horizontal_vertical_and_screen_rotation() {
        let default = CelestialView::default();
        assert_eq!(
            default.matrix(0.0, 1),
            [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]
        );
        for (axis, vector, expected) in [
            ([1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, -1.0, 0.0]),
            ([0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]),
            ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        ] {
            let view = CelestialView {
                rotating: true,
                axis,
                ..default
            };
            assert_direction(transform_direction(&view.matrix(15.0, 1), vector), expected);
        }
        // A positive roll maps a camera-right ray to body-up: the image turns clockwise.
        let rolled = CelestialView {
            roll: 90.0,
            ..default
        }
        .matrix(0.0, 1);
        assert_direction(
            transform_direction(&rolled, [1.0, 0.0, 0.0]),
            [0.0, 1.0, 0.0],
        );
        assert_direction(
            transform_direction(&rolled, [0.0, 0.0, 1.0]),
            [0.0, 0.0, 1.0],
        );
    }

    #[test]
    fn diagonal_axis_cycles_coordinates_and_normalization_ignores_scale() {
        let view = CelestialView {
            rotating: true,
            axis: [1.0; 3],
            ..CelestialView::default()
        };
        let matrix = view.matrix(20.0, 1); // 120° around the body diagonal.
        for (vector, expected) in [
            ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
            ([0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
            ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0]),
        ] {
            assert_direction(transform_direction(&matrix, vector), expected);
        }
        for scale in [
            f64::MAX,
            1e100,
            1.0,
            1e-100,
            f64::MIN_POSITIVE,
            f64::from_bits(1),
        ] {
            assert_eq!(
                CelestialView {
                    axis: [scale; 3],
                    ..view
                }
                .matrix(20.0, 1),
                matrix
            );
        }
    }

    #[test]
    fn legacy_north_axis_keeps_existing_geography_and_rotation_direction() {
        for latitude in [-90.0_f64, -33.8, 0.0, 23.13, 90.0] {
            for longitude in [-180.0, -75.0, 113.28, 180.0] {
                for rotating in [false, true] {
                    let view = CelestialView {
                        rotating,
                        latitude,
                        longitude,
                        ..CelestialView::default()
                    };
                    for unix in [-1.0, 0.0, 1_792_148_400.125] {
                        for period in [1, 20, 1440, 39343] {
                            let matrix = view.matrix(unix, period);
                            let (s, c) =
                                earth_view_longitude(unix, period, longitude, rotating).sin_cos();
                            let (sl, cl) = (latitude.to_radians() as f32).sin_cos();
                            for normal in [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]] {
                                // Independent old shader transform, including the latitude sign.
                                let [x, y, z] = normal;
                                let (y, z) = (cl * y + sl * z, -sl * y + cl * z);
                                assert_direction(
                                    transform_direction(&matrix, normal),
                                    [c * x + s * z, y, -s * x + c * z],
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn pole_views_oblique_axes_and_roll_remain_proper_orthonormal_rotations() {
        for latitude in [-90.0, -89.999, 0.0, 23.13, 89.999, 90.0] {
            for axis in [
                [0.0, 1.0, 0.0],
                [1.0, 0.0, 0.0],
                [1.0, -2.0, 3.0],
                [f64::MAX, 0.0, f64::MAX],
            ] {
                for roll in [-180.0, -123.25, 0.0, 37.0, 180.0] {
                    let matrix = CelestialView {
                        rotating: true,
                        latitude,
                        longitude: 113.28,
                        roll,
                        axis,
                    }
                    .matrix(1_792_148_400.125, 17);
                    let columns: [[f32; 3]; 3] = std::array::from_fn(|column| {
                        [
                            matrix[column * 3],
                            matrix[column * 3 + 1],
                            matrix[column * 3 + 2],
                        ]
                    });
                    for (i, a) in columns.into_iter().enumerate() {
                        for (j, b) in columns.into_iter().enumerate() {
                            let expected = if i == j { 1.0 } else { 0.0 };
                            assert!((dot(a, b) - expected).abs() < 3e-7, "{matrix:?}");
                        }
                    }
                    let [a, b, c] = columns;
                    let cross = [
                        a[1] * b[2] - a[2] * b[1],
                        a[2] * b[0] - a[0] * b[2],
                        a[0] * b[1] - a[1] * b[0],
                    ];
                    assert!((dot(cross, c) - 1.0).abs() < 3e-7); // No mirror or scale.
                }
            }
        }
        for (latitude, expected) in [(90.0, [0.0, 1.0, 0.0]), (-90.0, [0.0, -1.0, 0.0])] {
            let matrix = CelestialView {
                latitude,
                longitude: 113.28,
                roll: 57.0,
                ..CelestialView::default()
            }
            .matrix(0.0, 1);
            assert_direction(transform_direction(&matrix, [0.0, 0.0, 1.0]), expected);
        }
    }

    #[test]
    fn reverse_axis_periodicity_and_restart_use_the_absolute_clock() {
        let view = CelestialView {
            rotating: true,
            latitude: -47.5,
            longitude: 152.0,
            roll: 63.0,
            axis: [1.0, -2.0, 0.5],
        };
        let reverse = CelestialView {
            axis: view.axis.map(|v| -v),
            ..view
        };
        for period in [1, 20, 1440, 39343, 525600] {
            let unix = 1_792_148_400.125;
            let initial = view.matrix(unix, period);
            assert_eq!(initial, view.matrix(unix, period));
            assert_eq!(
                initial,
                view.matrix(unix + f64::from(period) * 60.0, period)
            );
            assert!(
                initial
                    .into_iter()
                    .zip(reverse.matrix(-unix, period))
                    .all(|(a, b)| (a - b).abs() < 1e-7)
            );
            assert_ne!(
                initial,
                view.matrix(unix + f64::from(period) * 15.0, period)
            );
        }
    }

    #[test]
    fn invalid_internal_inputs_have_finite_deterministic_fallbacks() {
        let fixed = CelestialView {
            latitude: 23.13,
            longitude: 113.28,
            roll: 17.0,
            axis: [1.0, 2.0, 3.0],
            ..CelestialView::default()
        };
        for unix in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MIN,
            f64::MAX,
            -1.0,
            0.0,
        ] {
            for period in [0, 1, 525600, u32::MAX] {
                assert_eq!(fixed.matrix(unix, period), fixed.matrix(0.0, 1));
                assert!(
                    CelestialView {
                        rotating: true,
                        ..fixed
                    }
                    .matrix(unix, period)
                    .into_iter()
                    .all(f32::is_finite)
                );
            }
        }
        for axis in [
            [0.0; 3],
            [f64::NAN, 1.0, 0.0],
            [0.0, f64::INFINITY, 0.0],
            [f64::NEG_INFINITY, 1.0, 2.0],
        ] {
            let view = CelestialView {
                rotating: true,
                axis,
                ..fixed
            };
            assert_eq!(
                view.matrix(1792148400.125, 20),
                CelestialView {
                    axis: [0.0, 1.0, 0.0],
                    ..view
                }
                .matrix(1792148400.125, 20)
            );
        }
        for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                CelestialView {
                    latitude: invalid,
                    longitude: invalid,
                    roll: invalid,
                    ..CelestialView::default()
                }
                .matrix(0.0, 1),
                CelestialView::default().matrix(0.0, 1)
            );
        }
        for degrees in [f64::MIN, f64::MAX] {
            assert!(
                CelestialView {
                    rotating: true,
                    latitude: degrees,
                    longitude: degrees,
                    roll: degrees,
                    axis: [degrees; 3]
                }
                .matrix(f64::MAX, u32::MAX)
                .into_iter()
                .all(f32::is_finite)
            );
        }
    }

    #[test]
    fn arbitrary_tours_preserve_geographic_daylight_and_the_current_lunar_phase() {
        let geographic = [0.3, -0.4, 0.75_f32.sqrt()];
        let normals = [[0.0, 0.0, 1.0], [0.6, 0.0, 0.8], [0.0, -0.8, 0.6]];
        for unix in [
            1_774_008_000.0,
            1_782_043_200.0,
            1_792_148_400.0,
            1_792_987_920.0,
        ] {
            let sun = solar_direction(unix);
            let phase = lunar_phase_light(unix);
            let earth_light = dot(geographic, sun);
            for axis in [
                [0.0, 1.0, 0.0],
                [1.0, 0.0, 0.0],
                [0.0, 0.0, -1.0],
                [1.0, -2.0, 3.0],
            ] {
                for period in [1, 20, 39343] {
                    let matrix = CelestialView {
                        rotating: true,
                        latitude: 90.0,
                        longitude: -73.0,
                        roll: 137.0,
                        axis,
                    }
                    .matrix(unix, period);
                    let recovered =
                        transform_direction(&matrix, camera_direction(&matrix, geographic));
                    assert!((dot(recovered, sun) - earth_light).abs() < 3e-7);
                    let lunar_light = transform_direction(&matrix, phase);
                    assert_direction(camera_direction(&matrix, lunar_light), phase);
                    for normal in normals {
                        assert!(
                            (dot(transform_direction(&matrix, normal), lunar_light)
                                - dot(normal, phase))
                            .abs()
                                < 3e-7
                        );
                    }
                }
            }
        }
    }
}

fn observer_normal(latitude_degrees: f64, longitude_degrees: f64) -> Option<[f64; 3]> {
    if !latitude_degrees.is_finite()
        || !(-90.0..=90.0).contains(&latitude_degrees)
        || !longitude_degrees.is_finite()
    {
        return None;
    }
    let latitude = latitude_degrees.to_radians();
    let longitude = longitude_degrees.rem_euclid(360.0).to_radians();
    Some([
        latitude.cos() * longitude.sin(),
        latitude.sin(),
        latitude.cos() * longitude.cos(),
    ])
}

fn solar_height(unix_seconds: f64, observer: [f64; 3]) -> f64 {
    let sun = solar_direction_f64(unix_seconds);
    (observer[0] * sun[0] + observer[1] * sun[1] + observer[2] * sun[2]).clamp(-1.0, 1.0)
}

/// Geometric solar-center elevation in degrees (no refraction or terrain).
/// Diagnostics share the renderer's ephemeris. None means invalid coordinates
/// or an epoch outside the supported range; east/north coordinates are positive.
#[allow(dead_code)] // Diagnostic helper, intentionally not evaluated per frame.
pub(crate) fn solar_elevation(
    unix_seconds: f64,
    latitude_degrees: f64,
    longitude_degrees: f64,
) -> Option<f64> {
    if !valid_solar_time(unix_seconds) {
        return None;
    }
    let observer = observer_normal(latitude_degrees, longitude_degrees)?;
    Some(solar_height(unix_seconds, observer).asin().to_degrees())
}

#[allow(dead_code)] // Available to diagnostics without adding a wallpaper clock UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SolarDayStatus {
    Normal,
    PolarDay,
    PolarNight,
    /// Only one crossing occurs in this civil day near a polar transition.
    Transition,
    Invalid,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SunriseSunset {
    /// Absolute UTC Unix seconds, restricted to the selected civil day.
    pub(crate) sunrise: Option<f64>,
    pub(crate) sunset: Option<f64>,
    pub(crate) status: SolarDayStatus,
}

/// Apparent sunrise/sunset for the civil date containing `unix_seconds` at
/// `utc_offset_seconds`. Offset zero selects a UTC day. The offset affects
/// only the date interval, never solar physics; it is a fixed offset, not a
/// timezone/DST database. Returned epochs lie in [local midnight, next midnight).
/// Both coordinates use degrees, north/east positive. The -0.833° solar-center
/// horizon includes nominal refraction and solar radius, without terrain or
/// observer altitude. Actual observed events can differ with the atmosphere.
/// Reference: https://gml.noaa.gov/grad/solcalc/solareqns.PDF .
#[allow(dead_code)] // Daily diagnostic/test API; do not recompute it per frame.
pub(crate) fn sunrise_sunset(
    unix_seconds: f64,
    latitude_degrees: f64,
    longitude_degrees: f64,
    utc_offset_seconds: i32,
) -> SunriseSunset {
    let invalid = SunriseSunset {
        sunrise: None,
        sunset: None,
        status: SolarDayStatus::Invalid,
    };
    if !valid_solar_time(unix_seconds) || !(-86_400..=86_400).contains(&utc_offset_seconds) {
        return invalid;
    }
    let Some(observer) = observer_normal(latitude_degrees, longitude_degrees) else {
        return invalid;
    };
    let offset = f64::from(utc_offset_seconds);
    let start = ((unix_seconds + offset) / 86_400.0).floor() * 86_400.0 - offset;
    let end = start + 86_400.0;
    if !valid_solar_time(start - 1.0) || !valid_solar_time(end + 1.0) {
        return invalid;
    }
    let horizon = (-0.833_f64).to_radians().sin();
    let height = |time| solar_height(time, observer) - horizon;
    let slope = |time| height(time + 1.0) - height(time - 1.0);

    // Locate the day's extrema first. A coarse sign-crossing scan alone can
    // miss an arbitrarily short day/night near the polar circles. Splitting at
    // refined turning points brackets those paired crossings as well as the
    // seasonal single crossing at the geographic poles. Work is always bounded.
    let mut boundaries = vec![start];
    let mut previous_time = start;
    let mut previous_slope = slope(start);
    for step in 1..=48 {
        let time = start + f64::from(step) * 1800.0;
        let next_slope = slope(time);
        if previous_slope.is_sign_positive() != next_slope.is_sign_positive() {
            let mut left = previous_time;
            let mut right = time;
            for _ in 0..32 {
                let middle = (left + right) / 2.0;
                if slope(middle).is_sign_positive() == previous_slope.is_sign_positive() {
                    left = middle;
                } else {
                    right = middle;
                }
            }
            boundaries.push((left + right) / 2.0);
        }
        previous_time = time;
        previous_slope = next_slope;
    }
    boundaries.push(end);
    let mut result = SunriseSunset {
        sunrise: None,
        sunset: None,
        status: SolarDayStatus::Normal,
    };
    for segment in boundaries.windows(2) {
        let mut left = segment[0];
        let mut right = segment[1];
        let left_height = height(left);
        let right_height = height(right);
        if (left_height < 0.0) == (right_height < 0.0) {
            continue;
        }
        let rising = left_height < right_height;
        for _ in 0..40 {
            let middle = (left + right) / 2.0;
            if (height(middle) < 0.0) == (left_height < 0.0) {
                left = middle;
            } else {
                right = middle;
            }
        }
        let event = (left + right) / 2.0;
        if event >= start && event < end {
            if rising {
                result.sunrise.get_or_insert(event);
            } else {
                result.sunset.get_or_insert(event);
            }
        }
    }
    result.status = match (result.sunrise, result.sunset) {
        (Some(_), Some(_)) => SolarDayStatus::Normal,
        (None, None) if height((start + end) / 2.0) >= 0.0 => SolarDayStatus::PolarDay,
        (None, None) => SolarDayStatus::PolarNight,
        _ => SolarDayStatus::Transition,
    };
    result
}

/// Timezone-derived central meridian, plus one gentle orbit per day. This is
/// a presentation choice, never an inferred GPS location or physical spin rate.
#[allow(dead_code)] // Legacy view helper retained for compatibility and regressions.
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

#[cfg(test)]
mod fixed_earth_solar_tests {
    use super::*;

    #[test]
    fn fixed_geographic_view_is_exact_and_independent_of_all_clocks() {
        let expected = 120.0_f64.to_radians() as f32;
        for epoch in [
            0.0,
            -1.0,
            1773979200.0,
            1782014400.0,
            1797825600.0,
            f64::MIN,
            f64::MAX,
            f64::NAN,
            f64::INFINITY,
        ] {
            for period in [0, 1, 20, 1440, u32::MAX] {
                assert_eq!(earth_view_longitude(epoch, period, 120.0, false), expected);
            }
        }
        assert_eq!(
            earth_view_longitude(0.0, 1440, -75.0, false),
            -75.0_f64.to_radians() as f32
        );
        assert_eq!(earth_view_longitude(0.0, 1440, 480.0, false), expected);
        assert_eq!(
            earth_view_longitude(0.0, 1440, f64::NAN, false),
            113.28_f64.to_radians() as f32
        );
        let a = earth_view_longitude(0.0, 20, 120.0, true);
        let b = earth_view_longitude(300.0, 20, 120.0, true);
        assert!((b - a - std::f32::consts::FRAC_PI_2).abs() < 1e-6);
        assert_eq!(a, earth_view_longitude(1200.0, 20, 120.0, true));
    }

    #[test]
    fn guangzhou_october_16_2026_matches_noaa_reference() {
        // Guangzhou 23.13N, 113.28E, UTC+8. Original NOAA main.js outputs:
        // sunrise 06:24:00.330, sunset 18:00:39.315 on 2026-10-16 local.
        let noon = 1792123200.0; // 12:00 local = 04:00 UTC
        let events = sunrise_sunset(noon, 23.13, 113.28, 28800);
        assert_eq!(events.status, SolarDayStatus::Normal);
        assert!((events.sunrise.unwrap() - 1792103040.3304005).abs() < 2.0);
        assert!((events.sunset.unwrap() - 1792144839.315497).abs() < 2.0);
        assert!((solar_elevation(noon, 23.13, 113.28).unwrap() - 57.85385099862216).abs() < 1e-7);
        assert!(
            (solar_elevation(1792137600.0, 23.13, 113.28).unwrap() - 25.803977802722727).abs()
                < 1e-7
        );
        for epoch in [noon - 86400.0, noon, noon + 3600.0, noon + 86400.0] {
            assert_eq!(
                earth_view_longitude(epoch, 1440, 113.28, false),
                113.28_f64.to_radians() as f32
            );
        }
    }

    #[test]
    fn dated_elevations_match_independently_executed_noaa_reference() {
        // Executed NOAA's original calcAzEl (2026-10-16 fetch) with only
        // calcRefraction replaced by () => 0 to compare geometric elevation.
        // https://gml.noaa.gov/grad/solcalc/main.js
        // SHA256: 3832956f24724eafacf9e18173b9299b1554d6252bed0784d9c9aca6d9f54856
        // 2026 March equinox, June/December solstices; north and south sites.
        for (epoch, lat, lon, expected) in [
            (1773979200.0, 25.0, 120.0, 64.75910220704091),
            (1774008000.0, 51.4779, 0.0, 38.45483406531001),
            (1782014400.0, 25.0, 120.0, 88.38777021293001),
            (1797825600.0, 25.0, 120.0, 41.56144455343402),
            (1782007200.0, -33.8688, 151.2093, 32.688553110071865),
            (1797818400.0, -33.8688, 151.2093, 79.4558602767185),
            (1782043200.0, 78.2232, 15.6469, 34.757266958605335),
            (1797854400.0, 78.2232, 15.6469, -12.091992749311473),
            (1782043200.0, -77.8419, 166.6863, -35.205787132094855),
            (1797854400.0, -77.8419, 166.6863, 11.561341285591766),
            (1066419030.0, 39.742476, -105.1786, 39.87207442406391),
        ] {
            let elevation = solar_elevation(epoch, lat, lon).unwrap();
            assert!(
                (elevation - expected).abs() < 1e-7,
                "{epoch}: {elevation} != {expected}"
            );
            let n = observer_normal(lat, lon).unwrap();
            let sun = solar_direction(epoch);
            let render_elevation = n
                .iter()
                .zip(sun)
                .map(|(a, b)| a * f64::from(b))
                .sum::<f64>()
                .clamp(-1.0, 1.0)
                .asin()
                .to_degrees();
            assert!((render_elevation - expected).abs() < 0.0001);
        }
    }

    #[test]
    fn dated_sunrise_sunset_matches_noaa_and_uses_shared_horizon() {
        // Absolute UTC epochs from original NOAA calcSunriseSet, not this
        // implementation. NOAA uses a two-pass estimate; our root solve must
        // agree within two seconds and actually cross the -0.833° horizon.
        // 2026-03-20/06-21/12-21 at 25N120E, Greenwich, Sydney, Honolulu,
        // Kathmandu (+05:45), and 1960-03-20 (pre-Unix epoch).
        for (epoch, lat, lon, offset, rise, set) in [
            (
                1773979200.0,
                25.0,
                120.0,
                28800,
                1773957866.1994998,
                1774001459.714086,
            ),
            (
                1782014400.0,
                25.0,
                120.0,
                28800,
                1781989856.3846817,
                1782039153.9537055,
            ),
            (
                1797825600.0,
                25.0,
                120.0,
                28800,
                1797806421.7518003,
                1797844527.266731,
            ),
            (
                1774008000.0,
                51.4779,
                0.0,
                0,
                1773986571.9350252,
                1774030380.1133838,
            ),
            (
                1782007200.0,
                -33.8688,
                151.2093,
                36000,
                1781989197.381741,
                1782024829.9832215,
            ),
            (
                1797818400.0,
                -33.8688,
                151.2093,
                39600,
                1797792038.8622048,
                1797843925.6230288,
            ),
            (
                1790128800.0,
                -33.8688,
                151.2093,
                36000,
                1790106238.1055963,
                1790149917.526964,
            ),
            (
                1774044000.0,
                21.3069,
                -157.8583,
                -36000,
                1774024510.9442751,
                1774068155.8681252,
            ),
            (
                1773987300.0,
                27.7172,
                85.324,
                20700,
                1773966180.5554543,
                1774009789.2530448,
            ),
            (
                -308779200.0,
                25.0,
                120.0,
                28800,
                -308800530.5695675,
                -308756937.60305375,
            ),
        ] {
            let events = sunrise_sunset(epoch, lat, lon, offset);
            assert_eq!(events.status, SolarDayStatus::Normal, "{events:?}");
            assert!(
                (events.sunrise.unwrap() - rise).abs() < 2.0,
                "{events:?} vs {rise}"
            );
            assert!(
                (events.sunset.unwrap() - set).abs() < 2.0,
                "{events:?} vs {set}"
            );
            for (event, sign) in [
                (events.sunrise.unwrap(), 1.0),
                (events.sunset.unwrap(), -1.0),
            ] {
                assert!((solar_elevation(event, lat, lon).unwrap() + 0.833).abs() < 1e-7);
                assert!((solar_elevation(event + 30.0, lat, lon).unwrap() + 0.833) * sign > 0.0);
                assert!((solar_elevation(event - 30.0, lat, lon).unwrap() + 0.833) * sign < 0.0);
            }
        }
    }

    #[test]
    fn local_civil_dates_include_east_west_and_dateline_events() {
        for (epoch, lat, lon, offset) in [
            (1773979200.0, 25.0, 120.0, 28800),
            (1774044000.0, 21.3069, -157.8583, -36000),
            (1773957600.0, 1.8721, -157.4278, 50400), // Kiritimati March 20 local
        ] {
            let start =
                ((epoch + f64::from(offset)) / 86400.0_f64).floor() * 86400.0 - f64::from(offset);
            let expected = sunrise_sunset(epoch, lat, lon, offset);
            for sample in [start, start + 1.0, start + 43200.0, start + 86399.0] {
                assert_eq!(sunrise_sunset(sample, lat, lon, offset), expected);
            }
            for event in [expected.sunrise.unwrap(), expected.sunset.unwrap()] {
                assert!(event >= start && event < start + 86400.0);
            }
        }
        let kiribati = sunrise_sunset(1773957600.0, 1.8721, -157.4278, 50400);
        // NOAA evaluated on March 19 UTC gives the March 20 local events.
        assert!((kiribati.sunrise.unwrap() - 1773938045.8095474).abs() < 2.0);
        assert!((kiribati.sunset.unwrap() - 1773981632.8051462).abs() < 2.0);
    }

    #[test]
    fn polar_day_and_night_swap_hemispheres_at_solstices() {
        for (epoch, north_status, south_status) in [
            (
                1782043200.0,
                SolarDayStatus::PolarDay,
                SolarDayStatus::PolarNight,
            ),
            (
                1797854400.0,
                SolarDayStatus::PolarNight,
                SolarDayStatus::PolarDay,
            ),
        ] {
            for (lat, status) in [
                (78.2232, north_status),
                (90.0, north_status),
                (-77.8419, south_status),
                (-90.0, south_status),
            ] {
                let events = sunrise_sunset(epoch, lat, 15.0, 0);
                assert_eq!(events.status, status, "{lat}: {events:?}");
                assert_eq!(events.sunrise, None);
                assert_eq!(events.sunset, None);
            }
        }
    }

    #[test]
    fn short_polar_night_is_not_missed_between_coarse_samples() {
        // On 2026-06-21 at this polar-circle latitude, sunset and sunrise are
        // less than four minutes apart, between the half-hour sample points.
        let events = sunrise_sunset(1782043200.0, 65.729, 60.0, 0);
        assert_eq!(events.status, SolarDayStatus::Normal);
        let sunset = events.sunset.unwrap();
        let sunrise = events.sunrise.unwrap();
        assert!(sunrise > sunset && sunrise - sunset < 240.0);
        assert!(solar_elevation((sunset + sunrise) / 2.0, 65.729, 60.0).unwrap() < -0.833);
        assert!(solar_elevation(sunset - 60.0, 65.729, 60.0).unwrap() > -0.833);
        assert!(solar_elevation(sunrise + 60.0, 65.729, 60.0).unwrap() > -0.833);
    }

    #[test]
    fn geographic_pole_seasonal_crossing_has_one_event() {
        // North Pole 2026-03-18: refraction makes apparent sunrise precede
        // the equinox, and there is no sunset on this civil date.
        let events = sunrise_sunset(1773792000.0, 90.0, 0.0, 0);
        assert_eq!(events.status, SolarDayStatus::Transition);
        assert_eq!(events.sunset, None);
        let sunrise = events.sunrise.unwrap();
        assert!((solar_elevation(sunrise, 90.0, 0.0).unwrap() + 0.833).abs() < 1e-7);
        assert!(solar_elevation(sunrise - 3600.0, 90.0, 0.0).unwrap() < -0.833);
        assert!(solar_elevation(sunrise + 3600.0, 90.0, 0.0).unwrap() > -0.833);
    }

    #[test]
    fn clock_and_coordinate_failures_are_bounded_and_finite() {
        for epoch in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MAX,
            f64::MIN,
        ] {
            let sun = solar_direction(epoch);
            assert!(sun.iter().all(|c| c.is_finite()));
            assert!((sun.iter().map(|c| c * c).sum::<f32>() - 1.0).abs() < 1e-6);
            assert_eq!(
                sunrise_sunset(epoch, 25.0, 120.0, 28800).status,
                SolarDayStatus::Invalid
            );
            assert_eq!(solar_elevation(epoch, 25.0, 120.0), None);
            assert!(earth_view_longitude(epoch, 0, f64::MAX, true).is_finite());
        }
        for (lat, lon, offset) in [
            (f64::NAN, 120.0, 0),
            (91.0, 120.0, 0),
            (-91.0, 120.0, 0),
            (25.0, f64::INFINITY, 0),
            (25.0, 120.0, i32::MIN),
            (25.0, 120.0, i32::MAX),
        ] {
            assert_eq!(
                sunrise_sunset(1773979200.0, lat, lon, offset).status,
                SolarDayStatus::Invalid
            );
        }
    }

    #[test]
    fn physical_sun_has_no_timezone_or_decorative_rotation_dependency() {
        let epoch = 1773979200.0;
        let sun = solar_direction(epoch);
        for offset in [-43200, 0, 19800, 28800, 50400] {
            for period in [1, 20, 1440, 525600] {
                let _display_offset = presentation_offset(true, 0, Some(offset));
                let _decorative = earth_view_longitude(epoch, period, 120.0, true);
                assert_eq!(
                    earth_view_longitude(epoch, period, 120.0, false),
                    120.0_f64.to_radians() as f32
                );
                assert_eq!(solar_direction(epoch), sun);
            }
        }
        // Applying timezone to physics would move the lit meridian by 120°.
        let incorrectly_shifted = solar_direction(epoch + 28800.0);
        assert!(
            sun.iter()
                .zip(incorrectly_shifted)
                .map(|(a, b)| a * b)
                .sum::<f32>()
                < -0.4
        );
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
                "/home/u/.local/share/jwm/astronomy/earth-nasa-october.jpg",
                "/home/u/.local/share/jwm/astronomy/earth-nasa-january.jpg",
                "/usr/local/lib/jwm/versions/2/share/jwm/astronomy/earth-nasa-october.jpg",
                "/usr/local/share/jwm/astronomy/earth-nasa-october.jpg",
                "/usr/share/jwm/astronomy/earth-nasa-october.jpg",
                "/source/assets/astronomy/earth-nasa-october.jpg",
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
        assert_eq!(
            resolve_earth_texture_from("", &paths, |p| p == paths[2]),
            Some(paths[2].clone())
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
    fn explicit_legacy_january_asset_remains_authoritative() {
        let january = "/versions/current/share/jwm/astronomy/earth-nasa-january.jpg";
        let candidates = [PathBuf::from("/bundled/earth-nasa-october.jpg")];
        assert_eq!(
            resolve_earth_texture_from(january, &candidates, |_| true),
            Some(PathBuf::from(january))
        );
        assert_eq!(
            resolve_earth_texture_from(january, &candidates, |p| p == candidates[0]),
            None
        );
    }

    #[test]
    fn ordinary_upgrade_prefers_october_over_installed_january() {
        let paths = earth_texture_candidates(
            Some(Path::new("/home/u/.local/share")),
            Some(Path::new("/usr/local/lib/jwm/versions/2/bin/jwm")),
            &[PathBuf::from("/usr/local/share")],
            None,
        );
        let installed = [
            PathBuf::from(
                "/usr/local/lib/jwm/versions/2/share/jwm/astronomy/earth-nasa-october.jpg",
            ),
            PathBuf::from(
                "/usr/local/lib/jwm/versions/2/share/jwm/astronomy/earth-nasa-january.jpg",
            ),
            PathBuf::from("/usr/local/share/jwm/astronomy/earth-nasa-january.jpg"),
        ];
        assert_eq!(
            resolve_earth_texture_from("", &paths, |p| installed.iter().any(|item| item == p)),
            Some(installed[0].clone())
        );
        assert_eq!(
            resolve_earth_texture_from(installed[1].to_str().unwrap(), &paths, |p| installed
                .iter()
                .any(|item| item == p)),
            Some(installed[1].clone())
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
                    "/releases/{version}/share/jwm/astronomy/earth-nasa-october.jpg"
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
        let view = CelestialView {
            rotating: true,
            latitude: 23.13,
            longitude: 113.28,
            roll: 37.0,
            axis: [1.0, -2.0, 3.0],
        };
        let sample = |unix| {
            (
                solar_direction(unix),
                lunar_phase_light(unix),
                view.matrix(unix, 17),
            )
        };
        let initial = sample(t);
        for jump in [86400.0 * 90.0, -86400.0 * 90.0] {
            clock.set_paused(true);
            assert!(!clock.tick(now + Duration::from_secs(3600)));
            clock.set_paused(false);
            assert!(clock.tick(now + Duration::from_secs(3600)));
            let resumed = sample(t + jump);
            assert_ne!(resumed.0, initial.0);
            assert_ne!(resumed.1, initial.1);
            assert_ne!(resumed.2, initial.2);
            // Reversing a wall-clock correction restores the original sample.
            assert_eq!(sample(t), initial);
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
    visual_rotation_f64(unix, minutes) as f32
}

fn visual_rotation_f64(unix: f64, minutes: u32) -> f64 {
    let period = f64::from(minutes.clamp(1, 525_600)) * 60.0;
    if !unix.is_finite() {
        return 0.0;
    }
    unix.rem_euclid(period) / period * std::f64::consts::TAU
}

/// The timezone is a fixed observer meridian, not a second clock correction.
/// UTC alone advances rotation; offset is added once as 15 degrees per hour.
#[allow(dead_code)] // Longitude-only compatibility helper; current views use CelestialView.
pub(crate) fn celestial_longitude(unix: f64, minutes: u32, utc_offset_seconds: i32) -> f32 {
    let phase = f64::from(visual_rotation(unix, minutes));
    let meridian =
        f64::from(utc_offset_seconds.clamp(-86400, 86400)) / 86400.0 * std::f64::consts::TAU;
    (phase + meridian).rem_euclid(std::f64::consts::TAU) as f32
}

/// Geocentric lunar illumination in a stable waxing-right/waning-left view.
/// Short orbital model with the major lunar perturbations from Paul Schlyter:
/// https://stjarnhimlen.se/comp/ppcomp.html (sections 3–9).
/// UTC seconds, never civil date/timezone or the decorative rotation period.
/// Intended for contemporary wallpaper, not eclipse prediction: ignores TT−UT,
/// aberration, topocentric parallax, libration and local-horizon orientation.
pub(crate) fn lunar_phase_light(unix: f64) -> [f32; 3] {
    // Guard the low-precision elements and all downstream GPU uniforms. Outside
    // 1900–2100 (or with an invalid clock), use a deterministic J2000 fallback.
    let unix = if unix.is_finite() && (-2208988800.0..4133980800.0).contains(&unix) {
        unix
    } else {
        946684800.0
    };
    let d = (unix - 946598400.0) / 86400.0; // 1999-12-31 00:00 UTC
    let rad = |v: f64| v.rem_euclid(360.0).to_radians();
    let orbit = |m: f64, e: f64| {
        let mut eccentric = m + e * m.sin() * (1.0 + e * m.cos());
        for _ in 0..5 {
            eccentric -= (eccentric - e * eccentric.sin() - m) / (1.0 - e * eccentric.cos());
        }
        let x = eccentric.cos() - e;
        let y = (1.0 - e * e).sqrt() * eccentric.sin();
        (y.atan2(x), x.hypot(y))
    };
    let ms = rad(356.0470 + 0.9856002585 * d);
    let ws = rad(282.9404 + 4.70935e-5 * d);
    let (vs, rs) = orbit(ms, 0.016709 - 1.151e-9 * d);
    let sun_lon = vs + ws;
    let node = rad(125.1228 - 0.0529538083 * d);
    let inclination = rad(5.1454);
    let wm = rad(318.0634 + 0.1643573223 * d);
    let mm = rad(115.3654 + 13.0649929509 * d);
    let (vm, rm) = orbit(mm, 0.054900);
    let argument = vm + wm;
    let x = node.cos() * argument.cos() - node.sin() * argument.sin() * inclination.cos();
    let y = node.sin() * argument.cos() + node.cos() * argument.sin() * inclination.cos();
    let z = argument.sin() * inclination.sin();
    let elongation = mm + wm + node - ms - ws;
    let f = mm + wm;
    let lon_correction = -1.274 * (mm - 2.0 * elongation).sin() + 0.658 * (2.0 * elongation).sin()
        - 0.186 * ms.sin()
        - 0.059 * (2.0 * mm - 2.0 * elongation).sin()
        - 0.057 * (mm - 2.0 * elongation + ms).sin()
        + 0.053 * (mm + 2.0 * elongation).sin()
        + 0.046 * (2.0 * elongation - ms).sin()
        + 0.041 * (mm - ms).sin()
        - 0.035 * elongation.sin()
        - 0.031 * (mm + ms).sin()
        - 0.015 * (2.0 * f - 2.0 * elongation).sin()
        + 0.011 * (mm - 4.0 * elongation).sin();
    let lat_correction = -0.173 * (f - 2.0 * elongation).sin()
        - 0.055 * (mm - f - 2.0 * elongation).sin()
        - 0.046 * (mm + f - 2.0 * elongation).sin()
        + 0.033 * (f + 2.0 * elongation).sin()
        + 0.017 * (2.0 * mm + f).sin();
    let lon = y.atan2(x) + lon_correction.to_radians();
    let lat = z.atan2(x.hypot(y)) + lat_correction.to_radians();
    let distance =
        60.2666 * rm - 0.58 * (mm - 2.0 * elongation).cos() - 0.46 * (2.0 * elongation).cos();
    let sun_distance = rs * 149597870.7 / 6378.14; // AU → Earth radii
    let delta = lon - sun_lon;
    let cos_elongation = (lat.cos() * delta.cos()).clamp(-1.0, 1.0);
    // Moon→Sun dotted with Moon→Earth; retain finite solar distance.
    let light_z = ((distance - sun_distance * cos_elongation)
        / (sun_distance * sun_distance + distance * distance
            - 2.0 * sun_distance * distance * cos_elongation)
            .sqrt())
    .clamp(-1.0, 1.0);
    let light_x = (1.0 - light_z * light_z).sqrt().copysign(delta.sin());
    [light_x as f32, 0.0, light_z as f32]
}

/// Legacy latitude-0.18/longitude transform for the original shader probes.
/// Rotate the light together with normals so spin cannot rotate the terminator;
/// current rendering uses transform_direction with the complete CelestialView.
#[allow(dead_code)] // Legacy lunar transform retained as an independent regression baseline.
pub(crate) fn lunar_sun_direction(unix: f64, longitude: f32) -> [f32; 3] {
    let [x, _, z] = lunar_phase_light(unix);
    let (sl, cl) = 0.18_f32.sin_cos();
    let (s, c) = longitude.sin_cos();
    [c * x + s * cl * z, sl * z, -s * x + c * cl * z]
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

#[cfg(test)]
mod lunar_phase_tests {
    use super::*;

    #[test]
    fn usno_2026_primary_phases_and_nasa_current_illumination() {
        // USNO UTC primary phases; quarters are longitude events, not exactly
        // 50% physical illumination due to lunar latitude and solar distance.
        // https://aa.usno.navy.mil/api/moon/phases/year?year=2026
        for (unix, expected, waxing) in [
            (1791647400.0, 0.0, None),
            (1792339920.0, 0.5, Some(true)),
            (1792987920.0, 1.0, None),
            (1793564880.0, 0.5, Some(false)),
            // NASA SVS mooninfo_2026.json: 2026-10-16 11:00 UT, 29.78%.
            (1792148400.0, 0.2978, Some(true)),
        ] {
            let light = lunar_phase_light(unix);
            let fraction = (1.0 + light[2]) / 2.0;
            assert!((fraction - expected).abs() < 0.005, "{unix}: {fraction}");
            if let Some(waxing) = waxing {
                assert_eq!(light[0] > 0.0, waxing);
            }
            assert!((light.iter().map(|v| v * v).sum::<f32>() - 1.0).abs() < 1e-6);
        }
    }

    #[test]
    fn utc_and_local_midnight_are_continuous_not_phase_resets() {
        for midnight in [1792080000.0, 1792108800.0] {
            // UTC+8, UTC
            let before = lunar_phase_light(midnight - 1.0);
            let after = lunar_phase_light(midnight + 1.0);
            assert!(
                before
                    .iter()
                    .zip(after)
                    .all(|(a, b)| (a - b).abs() < 0.00002)
            );
        }
        for value in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            -1e15,
            1e15,
            f64::MAX,
            -f64::MAX,
            -1.0,
            0.0,
        ] {
            assert!(lunar_phase_light(value).iter().all(|v| v.is_finite()));
        }
    }

    #[test]
    fn artistic_spin_does_not_change_camera_relative_phase() {
        let t = 1792148400.0;
        let expected = lunar_phase_light(t);
        for period in [1, 30, 39343, 525600] {
            let angle = celestial_longitude(t, period, 0);
            let light = lunar_sun_direction(t, angle);
            let (s, c) = angle.sin_cos();
            let x = c * light[0] - s * light[2];
            let z = s * light[0] + c * light[2];
            let (sl, cl) = 0.18_f32.sin_cos();
            let recovered = [x, cl * light[1] - sl * z, sl * light[1] + cl * z];
            assert!(
                recovered
                    .iter()
                    .zip(expected)
                    .all(|(a, b)| (a - b).abs() < 1e-6)
            );
        }
    }
}

#[cfg(test)]
mod sky_motion_tests {
    use super::sky_rotation;
    #[test]
    fn sky_travel_is_bounded_periodic_and_restart_independent() {
        for t in [-2208988800.125, -0.125, 0.0, 1792108800.125, 4102444800.125] {
            for minutes in [1, 1440, 525600] {
                let a = sky_rotation(t, minutes);
                assert_eq!(a, sky_rotation(t, minutes));
                let b = sky_rotation(t + f64::from(minutes) * 60.0, minutes);
                for i in 0..2 {
                    assert!((a[i] - b[i]).abs() < 1e-6);
                }
                assert!((a[0] * a[0] + a[1] * a[1] - 1.0).abs() < 1e-6);
            }
            assert_eq!(sky_rotation(t, 0), [0.0, 0.0]);
        }
        for t in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(sky_rotation(t, 1440), [0.0, 0.0]);
        }
        let before = sky_rotation(86400.0 - 0.001, 1440);
        let after = sky_rotation(86400.0 + 0.001, 1440);
        for i in 0..2 {
            assert!((before[i] - after[i]).abs() < 1e-6);
        }
        assert!((sky_rotation(21600.0, 1440)[1] - 1.0).abs() < 1e-6);
    }
}
