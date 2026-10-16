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
            let d = (b - a).rem_euclid(std::f32::consts::TAU);
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
        ((if i == 2 { 5.0 } else { k * 2.39996 }) + seconds * (0.12 / k.powf(0.65)))
            .rem_euclid(std::f64::consts::TAU) as f32
    });
    (
        phases,
        (seconds * 0.015).rem_euclid(std::f64::consts::TAU) as f32,
    )
}
