//! Bounded, deterministic camera for the decorative (not ephemeris) solar scene.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SolarCamera {
    pub azimuth: f32,
    pub elevation: f32,
    pub distance: f32,
}
impl Default for SolarCamera {
    fn default() -> Self {
        Self {
            azimuth: 0.4,
            elevation: 0.65,
            distance: 35.0,
        }
    }
}
impl SolarCamera {
    /// Angles in radians; positive wheel steps zoom in. Reject invalid events atomically.
    pub(crate) fn delta(&mut self, azimuth: f32, elevation: f32, zoom_steps: f32) -> bool {
        if !azimuth.is_finite() || !elevation.is_finite() || !zoom_steps.is_finite() {
            return false;
        }
        let old = *self;
        self.azimuth = (self.azimuth + azimuth.clamp(-6.3, 6.3)).rem_euclid(std::f32::consts::TAU);
        self.elevation = (self.elevation + elevation.clamp(-3.0, 3.0)).clamp(-1.45, 1.45);
        self.distance =
            (self.distance * (-zoom_steps.clamp(-20.0, 20.0) * 0.12).exp()).clamp(5.0, 60.0);
        *self != old
    }
    pub(crate) fn eye(self) -> [f32; 3] {
        let (s, c) = self.azimuth.sin_cos();
        let (e, h) = self.elevation.sin_cos();
        [
            self.distance * h * s,
            self.distance * e,
            self.distance * h * c,
        ]
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn phases_stay_bounded_and_smooth_after_years() {
        let (a, _) = scene_phases(1e9);
        let (b, _) = scene_phases(1e9 + 1.0 / 15.0);
        for (a, b) in a.into_iter().zip(b) {
            assert!((0.0..=std::f32::consts::TAU).contains(&a));
            let d = (a - b).rem_euclid(std::f32::consts::TAU);
            assert!(d > 0.0 && d < 0.01);
        }
        assert_eq!(scene_phases(f64::NAN), scene_phases(0.0));
    }
    #[test]
    fn rejects_nonfinite_atomically() {
        for n in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut c = SolarCamera::default();
            let old = c;
            assert!(!c.delta(n, 0.0, 0.0));
            assert_eq!(c, old);
            assert!(!c.delta(0.0, n, 0.0));
            assert!(!c.delta(0.0, 0.0, n));
        }
    }
    #[test]
    fn bounded_and_non_singular() {
        let mut c = SolarCamera::default();
        for _ in 0..1000 {
            c.delta(1e30, 1e30, 1e30);
        }
        assert_eq!(c.distance, 5.0);
        assert_eq!(c.elevation, 1.45);
        assert!(c.azimuth < std::f32::consts::TAU);
        assert!(c.eye().iter().all(|v| v.is_finite()));
        c.delta(0.0, -1e30, -1e30);
        assert_eq!(c.elevation, -1.45);
        for _ in 0..20 {
            c.delta(0.0, 0.0, -20.0);
        }
        assert_eq!(c.distance, 60.0);
    }
    #[test]
    fn zero_is_unchanged_and_zoom_sign_is_correct() {
        let mut c = SolarCamera::default();
        assert!(!c.delta(0.0, 0.0, 0.0));
        assert!(c.delta(0.0, 0.0, 1.0));
        assert!(c.distance < 35.0);
        let len = c.eye().iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((len - c.distance).abs() < 0.001);
    }
}

/// Reduce in f64 before uploading floats, preserving smooth motion on long uptimes.
pub(crate) fn scene_phases(seconds: f64) -> ([f32; 8], f32) {
    let seconds = if seconds.is_finite() {
        seconds.max(0.0)
    } else {
        0.0
    };
    let phases = std::array::from_fn(|i| {
        let k = (i + 1) as f64;
        ((if i == 2 { 5.0 } else { k * 2.39996 }) - seconds * (0.12 / k.powf(0.65)))
            .rem_euclid(std::f64::consts::TAU) as f32
    });
    (
        phases,
        (seconds * 0.015).rem_euclid(std::f64::consts::TAU) as f32,
    )
}

/// NASA GSFC sidereal periods (hours), magnitudes only: retrograde is encoded
/// by the full obliquity (>90 degrees) in the shader, never by a second sign.
/// https://nssdc.gsfc.nasa.gov/planetary/factsheet/
pub(crate) const SPIN_HOURS: [f64; 8] = [1407.6, 5832.5, 23.9345, 24.6229, 9.9, 10.7, 17.2, 16.1];

/// Physical elapsed seconds, independent from the decorative orbital clock.
pub(crate) fn scene_spin_phases(seconds: f64) -> [f32; 8] {
    let seconds = if seconds.is_finite() {
        seconds.max(0.0)
    } else {
        0.0
    };
    std::array::from_fn(|i| {
        ((seconds.rem_euclid(SPIN_HOURS[i] * 3600.0) / (SPIN_HOURS[i] * 3600.0))
            * std::f64::consts::TAU) as f32
    })
}

/// Integrate the previously selected rate: changing speed or pausing never
/// reinterprets uptime and therefore cannot teleport a surface longitude.
#[derive(Default)]
pub(crate) struct SolarSpinClock {
    pub seconds: f64,
    last: Option<f64>,
    rate: f64,
}
impl SolarSpinClock {
    pub(crate) fn sample(&mut self, now: f64, rate: f64) -> bool {
        if !now.is_finite() || !rate.is_finite() || !(0.0..=10000.0).contains(&rate) {
            return false;
        }
        if let Some(last) = self.last {
            if now < last {
                return false;
            }
            let next = self.seconds + (now - last) * self.rate;
            if !next.is_finite() {
                return false;
            }
            self.seconds = next;
        }
        let changed = self.rate != rate;
        self.last = Some(now);
        self.rate = rate;
        changed
    }
}

#[cfg(test)]
mod spin_tests {
    use super::*;
    #[test]
    fn physical_periods_independent_and_continuous() {
        for i in 0..8 {
            let period = SPIN_HOURS[i] * 3600.0;
            assert!(scene_spin_phases(period)[i].abs() < 1e-5);
            assert!(
                (scene_spin_phases(period * 0.25)[i] - std::f32::consts::FRAC_PI_2).abs() < 1e-5
            );
            let before = scene_spin_phases(period - 0.001)[i];
            let after = scene_spin_phases(period + 0.001)[i];
            let step = (after as f64 - before as f64).rem_euclid(std::f64::consts::TAU);
            assert!(step.min(std::f64::consts::TAU - step) < 0.00001);
        }
        let phases = scene_spin_phases(718.035 * 30.0);
        assert!((phases[2] - std::f32::consts::FRAC_PI_2).abs() < 1e-5);
        assert!(phases[4] > phases[2] && phases[2] > phases[0] && phases[0] > phases[1]);
        for seconds in [1e9, 1e12] {
            let a = scene_spin_phases(seconds);
            let b = scene_spin_phases(seconds + 718.035 / 15.0);
            for (a, b) in a.into_iter().zip(b) {
                let step = (b - a).rem_euclid(std::f32::consts::TAU);
                assert!(step > 0.0 && step < 0.01);
            }
        }
        assert_eq!(scene_spin_phases(f64::NAN), [0.0; 8]);
    }
    #[test]
    fn changing_rate_and_pausing_preserves_phase() {
        let mut clock = SolarSpinClock::default();
        clock.sample(0.0, 718.035);
        clock.sample(30.0, 0.0);
        let held = clock.seconds;
        clock.sample(300.0, 0.0);
        assert_eq!(clock.seconds, held);
        clock.sample(300.0, 1.0);
        assert_eq!(clock.seconds, held);
        clock.sample(301.0, 1.0);
        assert_eq!(clock.seconds, held + 1.0);
        assert!(!clock.sample(f64::NAN, 1.0));
        assert!(!clock.sample(302.0, f64::INFINITY));
        assert!(!clock.sample(299.0, 1.0));
        assert_eq!(clock.seconds, held + 1.0);
        assert!(!clock.sample(302.0, 10001.0));
        clock.sample(302.0, 10000.0);
        let valid = clock.seconds;
        assert!(!clock.sample(f64::MAX, 10000.0));
        assert_eq!(clock.seconds, valid);
    }
}
