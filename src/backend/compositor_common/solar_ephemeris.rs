//! Display geometry and a bounded playback clock over the JPL approximation.
//! The AU result is retained upstream. Each orbit is uniformly rescaled here
//! for a readable desktop; its eccentricity, inclination and direction survive.
use super::celestial_catalog::BODIES;
use super::kepler_ephemeris::{
    heliocentric_positions, julian_date_from_unix, orbit_position, semi_major_axes,
};
use super::solar_system::ORBIT_RADII;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SolarEphemeris {
    pub(crate) centers: [[f32; 3]; 9],
    /// Unit spin axes. Planetary obliquity is relative to the actual fitted
    /// orbit plane; the in-plane azimuth is a reproducible presentation choice,
    /// not an IAU pole-orientation solution. The Sun uses display north.
    pub(crate) axes: [[f32; 3]; 9],
    pub(crate) ellipse_centers: [[f32; 3]; 8],
    pub(crate) ellipse_u: [[f32; 3]; 8],
    pub(crate) ellipse_v: [[f32; 3]; 8],
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn unit(v: [f64; 3]) -> Option<[f64; 3]> {
    let length = v.iter().map(|v| v * v).sum::<f64>().sqrt();
    (length.is_finite() && length > f64::EPSILON).then(|| v.map(|v| v / length))
}

impl SolarEphemeris {
    pub(crate) fn at_unix(unix: f64) -> Option<Self> {
        let jd = julian_date_from_unix(unix)?;
        let positions = heliocentric_positions(jd)?;
        let axes = semi_major_axes(jd)?;
        let mut result = Self {
            centers: [[0.0; 3]; 9],
            axes: [[0.0, 1.0, 0.0]; 9],
            ellipse_centers: [[0.0; 3]; 8],
            ellipse_u: [[0.0; 3]; 8],
            ellipse_v: [[0.0; 3]; 8],
        };
        for i in 0..8 {
            let scale = f64::from(ORBIT_RADII[i]) / axes[i];
            // Right-handed renderer: +X is J2000 equinox, +Y ecliptic north.
            let map = |p: [f64; 3]| {
                [
                    (p[0] * scale) as f32,
                    (p[2] * scale) as f32,
                    (-p[1] * scale) as f32,
                ]
            };
            result.centers[i + 1] = map(positions[i]);
            let peri = map(orbit_position(jd, i, 0.0)?);
            let apo = map(orbit_position(jd, i, std::f64::consts::PI)?);
            let quarter = map(orbit_position(jd, i, std::f64::consts::FRAC_PI_2)?);
            result.ellipse_centers[i] = std::array::from_fn(|axis| (peri[axis] + apo[axis]) * 0.5);
            result.ellipse_u[i] = std::array::from_fn(|axis| (peri[axis] - apo[axis]) * 0.5);
            result.ellipse_v[i] =
                std::array::from_fn(|axis| quarter[axis] - result.ellipse_centers[i][axis]);
            let normal = unit(cross(
                result.ellipse_u[i].map(f64::from),
                result.ellipse_v[i].map(f64::from),
            ))?;
            // Project display +X into this planet's orbit plane. The fitted
            // planetary planes are far from a +X normal throughout the valid
            // interval, so this defines a stable azimuth without a pole switch.
            let tangent = unit([
                1.0 - normal[0] * normal[0],
                -normal[0] * normal[1],
                -normal[0] * normal[2],
            ])?;
            let (sin, cos) = BODIES[i + 1].obliquity_degrees.to_radians().sin_cos();
            result.axes[i + 1] =
                std::array::from_fn(|axis| (normal[axis] * cos + tangent[axis] * sin) as f32);
        }
        Some(result)
    }
}

/// Playback is never allowed to leave the verified UTC-conversion / fitted
/// interval. The upper margin keeps TT/TDB inside 2050-01-01 even after UTC
/// conversion. It stops at the boundary rather than fabricating an ephemeris.
const MIN_UNIX: f64 = 63_072_000.0; // 1972-01-01T00:00:00 UTC
const MAX_UNIX: f64 = 2_524_607_880.0; // 2049-12-31T23:58:00 UTC

#[derive(Default)]
pub(crate) struct SolarDateClock {
    unix: f64,
    last: Option<f64>,
    rate: f64,
    offset_days: f64,
}

impl SolarDateClock {
    /// Rate changes integrate the previous rate first; pause/resume never
    /// jumps. Selecting real time or changing the offset explicitly reanchors
    /// to the current UTC instant. Return only setting/initialization changes;
    /// the compositor's ordinary clock determines frame cadence.
    pub(crate) fn sample(&mut self, now: f64, wall_unix: f64, rate: f64, offset_days: f64) -> bool {
        if !now.is_finite()
            || !wall_unix.is_finite()
            || !rate.is_finite()
            || !offset_days.is_finite()
            || !(0.0..=1_000_000.0).contains(&rate)
            || self.last.is_some_and(|last| now < last)
        {
            return false;
        }
        let changed = self.last.is_none() || self.rate != rate || self.offset_days != offset_days;
        let next = if self.last.is_none() || self.offset_days != offset_days || rate == 1.0 {
            wall_unix + offset_days * 86400.0
        } else {
            self.unix + (now - self.last.unwrap()) * self.rate
        };
        if !next.is_finite() {
            return false;
        }
        self.unix = next.clamp(MIN_UNIX, MAX_UNIX);
        self.last = Some(now);
        self.rate = rate;
        self.offset_days = offset_days;
        changed
    }

    pub(crate) fn unix_seconds(&self) -> f64 {
        self.unix
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
        a.into_iter().zip(b).map(|(x, y)| x * y).sum()
    }

    #[test]
    fn date_positions_are_on_the_same_ellipses_as_the_rendered_guides() {
        for unix in [946728000.0, 1792249200.0, MAX_UNIX] {
            let frame = SolarEphemeris::at_unix(unix).unwrap();
            assert_eq!(frame.centers[0], [0.0; 3]);
            for i in 0..8 {
                let q = std::array::from_fn(|axis| {
                    frame.centers[i + 1][axis] - frame.ellipse_centers[i][axis]
                });
                let u = frame.ellipse_u[i];
                let v = frame.ellipse_v[i];
                assert!(dot(u, v).abs() < 0.0001);
                let x = dot(q, u) / dot(u, u);
                let y = dot(q, v) / dot(v, v);
                assert!((x * x + y * y - 1.0).abs() < 0.00001);
                assert!((dot(u, u).sqrt() - ORBIT_RADII[i]).abs() < 0.00001);
            }
        }
        assert_ne!(
            SolarEphemeris::at_unix(1792249200.0),
            SolarEphemeris::at_unix(1792335600.0)
        );
        assert!(SolarEphemeris::at_unix(f64::NAN).is_none());
    }

    #[test]
    fn spin_axes_keep_catalogue_obliquity_to_each_actual_orbit_plane() {
        // Mercury's 0.034-degree tilt is especially sensitive to accidentally
        // using the old display plane instead of its approximately 7-degree
        // inclined orbit. Saturn's ring axis must obey the same contract.
        for unix in [MIN_UNIX, 946728000.0, 1792249200.0, MAX_UNIX] {
            let frame = SolarEphemeris::at_unix(unix).unwrap();
            assert_eq!(frame.axes[0], [0.0, 1.0, 0.0]);
            for i in 0..8 {
                let axis = frame.axes[i + 1].map(f64::from);
                let length = axis.iter().map(|v| v * v).sum::<f64>().sqrt();
                assert!((length - 1.0).abs() < 0.000001);
                let normal = unit(cross(
                    frame.ellipse_u[i].map(f64::from),
                    frame.ellipse_v[i].map(f64::from),
                ))
                .unwrap();
                let cosine = axis.iter().zip(normal).map(|(a, n)| a * n).sum::<f64>();
                let sine = cross(axis, normal)
                    .iter()
                    .map(|v| v * v)
                    .sum::<f64>()
                    .sqrt();
                // atan2 remains well-conditioned for Mercury's tiny tilt and
                // correctly distinguishes retrograde axes beyond 90 degrees.
                let angle = sine.atan2(cosine).to_degrees();
                assert!(
                    (angle - BODIES[i + 1].obliquity_degrees).abs() < 0.00001,
                    "{} at {unix}: {angle}",
                    BODIES[i + 1].id,
                );
                assert_eq!(
                    cosine < 0.0,
                    BODIES[i + 1].rotation_hours < 0.0,
                    "retrograde axis for {}",
                    BODIES[i + 1].id,
                );
            }
        }
        // Independently reconstruct two J2000 plane normals directly from
        // JPL Table 1 inclination/node, rather than from our ellipse vectors.
        let j2000 = SolarEphemeris::at_unix(946728000.0).unwrap();
        for (body, inclination, node) in [
            (1, 7.00497902_f64, 48.33076593_f64),
            (6, 2.48599187_f64, 113.66242448_f64),
        ] {
            let (sin_i, cos_i) = inclination.to_radians().sin_cos();
            let (sin_node, cos_node) = node.to_radians().sin_cos();
            let normal = [sin_i * sin_node, cos_i, sin_i * cos_node];
            let axis = j2000.axes[body].map(f64::from);
            let cosine = axis.iter().zip(normal).map(|(a, n)| a * n).sum::<f64>();
            let sine = cross(axis, normal)
                .iter()
                .map(|v| v * v)
                .sum::<f64>()
                .sqrt();
            let angle = sine.atan2(cosine).to_degrees();
            assert!((angle - BODIES[body].obliquity_degrees).abs() < 0.00001);
        }
    }

    #[test]
    fn playback_pause_rate_changes_and_return_to_now_have_explicit_semantics() {
        let wall = 1_792_249_200.0;
        let mut clock = SolarDateClock::default();
        assert!(clock.sample(0.0, wall, 1.0, 0.0));
        clock.sample(1.0, wall + 1.0, 86400.0, 0.0);
        clock.sample(2.0, wall + 2.0, 0.0, 0.0);
        assert_eq!(clock.unix_seconds(), wall + 1.0 + 86400.0);
        clock.sample(200.0, wall + 200.0, 0.0, 0.0);
        assert_eq!(clock.unix_seconds(), wall + 1.0 + 86400.0);
        clock.sample(201.0, wall + 201.0, 1.0, 0.0);
        assert_eq!(clock.unix_seconds(), wall + 201.0);
        clock.sample(202.0, wall + 202.0, 1.0, 30.0);
        assert_eq!(clock.unix_seconds(), wall + 202.0 + 30.0 * 86400.0);
    }

    #[test]
    fn timeline_boundaries_and_invalid_events_cannot_poison_geometry() {
        let mut clock = SolarDateClock::default();
        clock.sample(0.0, MAX_UNIX - 10.0, 1_000_000.0, 0.0);
        clock.sample(1.0, MAX_UNIX - 9.0, 1_000_000.0, 0.0);
        assert_eq!(clock.unix_seconds(), MAX_UNIX);
        assert!(SolarEphemeris::at_unix(clock.unix_seconds()).is_some());
        assert!(!clock.sample(f64::NAN, MAX_UNIX, 0.0, 0.0));
        assert!(!clock.sample(0.5, MAX_UNIX, 0.0, 0.0));
        assert_eq!(clock.unix_seconds(), MAX_UNIX);
        clock.sample(2.0, MIN_UNIX - 100.0, 1.0, 0.0);
        assert_eq!(clock.unix_seconds(), MIN_UNIX);
    }
}
