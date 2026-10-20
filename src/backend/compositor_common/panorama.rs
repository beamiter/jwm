//! Shared finite, pole-safe perspective navigation for celestial wallpaper.
//!
//! The camera convention is +X right, +Y up, +Z forward. Pixel positions are
//! canvas-local with +Y up, matching the fragment shader after its X11 flip.
//! A press snapshots both orientation and lens: queued motion cannot alter
//! sensitivity or accumulate integration drift.

use super::animated_wallpaper::CelestialRotation;

pub(crate) const PANORAMA_DEFAULT_FOV: f32 = 60.0;
pub(crate) const PANORAMA_MIN_FOV: f32 = 8.0;
pub(crate) const PANORAMA_MAX_FOV: f32 = 110.0;

/// The two renderers compile the same bounded sky code on desktop GL and GLES.
/// Keeping expansion explicit also lets the headless tests compile the exact
/// production source without introducing a runtime file or GLSL extension.
pub(crate) fn cosmic_shader_source(source: &str) -> String {
    source
        .replace(
            "// JWM_COSMIC_SKY_INCLUDE",
            include_str!("../x11/compositor/shaders/cosmic_sky.glsl"),
        )
        .replace(
            "// JWM_PLANET_SURFACE_INCLUDE",
            include_str!("../x11/compositor/shaders/planet_surface.glsl"),
        )
        .replace(
            "// JWM_SOLAR_OPTICS_INCLUDE",
            include_str!("../x11/compositor/shaders/solar_optics.glsl"),
        )
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PanoramaView {
    orientation: CelestialRotation,
    fov: f32,
}

impl Default for PanoramaView {
    fn default() -> Self {
        Self {
            orientation: CelestialRotation::default(),
            fov: PANORAMA_DEFAULT_FOV,
        }
    }
}

impl PanoramaView {
    pub(crate) fn matrix(self) -> [f32; 9] {
        self.orientation.matrix()
    }

    pub(crate) fn orientation(self) -> CelestialRotation {
        self.orientation
    }

    pub(crate) fn set_orientation(&mut self, rotation: CelestialRotation) {
        self.orientation = rotation;
    }

    pub(crate) fn fov_degrees(self) -> f32 {
        self.fov
    }

    /// A bounded close-up lens used by deep-sky catalogue navigation.
    pub(crate) fn set_fov_degrees(&mut self, degrees: f32) {
        if degrees.is_finite() {
            self.fov = degrees.clamp(PANORAMA_MIN_FOV, PANORAMA_MAX_FOV);
        }
    }

    /// Positive wheel detents narrow the perspective lens. Bounded logarithmic
    /// zoom makes a burst equivalent to its separate detents, except at limits.
    pub(crate) fn zoom(&mut self, steps: f64) -> bool {
        if !steps.is_finite() || steps == 0.0 {
            return false;
        }
        let tangent = (f64::from(self.fov).to_radians() * 0.5).tan();
        let low = (f64::from(PANORAMA_MIN_FOV).to_radians() * 0.5).tan();
        let high = (f64::from(PANORAMA_MAX_FOV).to_radians() * 0.5).tan();
        let next = (tangent.ln() - steps.clamp(-64.0, 64.0) * 0.16)
            .clamp(low.ln(), high.ln())
            .exp();
        let fov = (2.0 * next.atan()).to_degrees() as f32;
        if (fov - self.fov).abs() < 0.00001 {
            return false;
        }
        self.fov = fov.clamp(PANORAMA_MIN_FOV, PANORAMA_MAX_FOV);
        true
    }

    pub(crate) fn begin_drag(self, point: [f64; 2], viewport: [f64; 2]) -> Option<PanoramaDrag> {
        if !point.iter().chain(&viewport).all(|v| v.is_finite())
            || viewport[0] <= 0.0
            || viewport[1] <= 0.0
        {
            return None;
        }
        Some(PanoramaDrag {
            origin: point,
            orientation: self.orientation,
            radians_per_pixel: 2.0 * (f64::from(self.fov).to_radians() * 0.5).tan() / viewport[1],
        })
    }

    /// CPU ray used by both click picking and navigation regression tests.
    pub(crate) fn ray(self, point: [f64; 2], viewport: [f64; 2]) -> Option<[f64; 3]> {
        self.begin_drag(point, viewport)?;
        let scale = 2.0 * (f64::from(self.fov).to_radians() * 0.5).tan() / viewport[1];
        let camera = [
            (point[0] - viewport[0] * 0.5) * scale,
            (point[1] - viewport[1] * 0.5) * scale,
            1.0,
        ];
        let length = camera[0].hypot(camera[1]).hypot(1.0);
        if !length.is_finite() {
            return None;
        }
        let ray = camera.map(|v| v / length);
        let matrix = self.matrix().map(f64::from);
        Some(std::array::from_fn(|row| {
            (0..3)
                .map(|column| matrix[column * 3 + row] * ray[column])
                .sum()
        }))
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PanoramaDrag {
    origin: [f64; 2],
    orientation: CelestialRotation,
    radians_per_pixel: f64,
}

impl PanoramaDrag {
    pub(crate) fn orientation_at(self, point: [f64; 2]) -> Option<CelestialRotation> {
        if !point.iter().all(|v| v.is_finite()) {
            return None;
        }
        if point == self.origin {
            return Some(self.orientation);
        }
        let yaw = -(point[0] - self.origin[0]) * self.radians_per_pixel;
        let pitch = (point[1] - self.origin[1]) * self.radians_per_pixel;
        Some(
            self.orientation
                .compose(CelestialRotation::axis_angle([0.0, 1.0, 0.0], yaw)?)
                .compose(CelestialRotation::axis_angle([1.0, 0.0, 0.0], pitch)?),
        )
    }
}

/// Decorative sky movement still follows its original absolute UTC period.
/// This rotation has no relation to the physical Sun or planetary ephemerides.
pub(crate) fn panorama_rotation(unix: f64, minutes: u32) -> CelestialRotation {
    if minutes == 0 || !unix.is_finite() {
        return CelestialRotation::default();
    }
    let period = f64::from(minutes.min(525600)) * 60.0;
    let phase = unix.rem_euclid(period) / period * std::f64::consts::TAU;
    CelestialRotation::axis_angle([0.0, 1.0, 0.0], phase).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: &[f32; 9], b: &[f32; 9]) {
        assert!(a.iter().zip(b).all(|(a, b)| (a - b).abs() < 0.000003));
    }

    #[test]
    fn panorama_reaches_a_full_sphere_and_passes_both_poles() {
        let view = PanoramaView::default();
        let scale = 2.0 * (f64::from(view.fov_degrees()).to_radians() * 0.5).tan() / 1080.0;
        let drag = view.begin_drag([960.0, 540.0], [1920.0, 1080.0]).unwrap();
        for (dx, dy, expected) in [
            (0.0, 0.0, [0.0, 0.0, 1.0]),
            (std::f64::consts::FRAC_PI_2, 0.0, [-1.0, 0.0, 0.0]),
            (std::f64::consts::PI, 0.0, [0.0, 0.0, -1.0]),
            (0.0, std::f64::consts::FRAC_PI_2, [0.0, -1.0, 0.0]),
            (0.0, -std::f64::consts::FRAC_PI_2, [0.0, 1.0, 0.0]),
            (0.0, std::f64::consts::PI, [0.0, 0.0, -1.0]),
            (std::f64::consts::TAU, 0.0, [0.0, 0.0, 1.0]),
        ] {
            let matrix = drag
                .orientation_at([960.0 + dx / scale, 540.0 + dy / scale])
                .unwrap()
                .matrix();
            for i in 0..3 {
                assert!((matrix[6 + i] - expected[i]).abs() < 0.000003);
            }
        }
    }

    #[test]
    fn panorama_motion_is_anchored_scale_invariant_and_reversible() {
        let view = PanoramaView::default();
        let a = view.begin_drag([20.0, 30.0], [1920.0, 1080.0]).unwrap();
        let b = view.begin_drag([40.0, 60.0], [3840.0, 2160.0]).unwrap();
        close(
            &a.orientation_at([370.0, -240.0]).unwrap().matrix(),
            &b.orientation_at([740.0, -480.0]).unwrap().matrix(),
        );
        let final_pose = a.orientation_at([200.0, 100.0]).unwrap();
        for p in [[40.0, 500.0], [-10000.0, 60.0], [8000.0, -5000.0]] {
            assert!(a.orientation_at(p).is_some());
        }
        assert_eq!(final_pose, a.orientation_at([200.0, 100.0]).unwrap());
        assert_eq!(a.orientation_at([20.0, 30.0]).unwrap(), view.orientation());
    }

    #[test]
    fn lens_zoom_has_real_perspective_limits_and_round_trip() {
        let mut view = PanoramaView::default();
        let edge_before = view.ray([800.0, 600.0], [800.0, 600.0]).unwrap();
        assert!(view.zoom(3.0));
        let edge_after = view.ray([800.0, 600.0], [800.0, 600.0]).unwrap();
        assert!(edge_after[2] > edge_before[2]);
        assert!(view.zoom(-3.0));
        assert!((view.fov_degrees() - PANORAMA_DEFAULT_FOV).abs() < 0.00002);
        assert!(view.zoom(1e100));
        assert_eq!(view.fov_degrees(), PANORAMA_MIN_FOV);
        assert!(!view.zoom(1.0));
        assert!(view.zoom(-1e100));
        assert_eq!(view.fov_degrees(), PANORAMA_MAX_FOV);
        assert!(!view.zoom(-1.0));
        assert!(!view.zoom(f64::NAN));
    }

    #[test]
    fn many_pole_crossings_never_shear_or_flip_handedness() {
        let mut view = PanoramaView::default();
        for i in 0..20000 {
            let drag = view.begin_drag([0.0, 0.0], [400.0, 300.0]).unwrap();
            view.set_orientation(
                drag.orientation_at([
                    ((i * 73) % 997) as f64 - 500.0,
                    ((i * 37) % 701) as f64 - 350.0,
                ])
                .unwrap(),
            );
        }
        let m = view.matrix();
        for a in 0..3 {
            for b in 0..3 {
                let dot: f32 = (0..3).map(|r| m[a * 3 + r] * m[b * 3 + r]).sum();
                assert!((dot - if a == b { 1.0 } else { 0.0 }).abs() < 0.000003);
            }
        }
        let determinant = m[0] * (m[4] * m[8] - m[7] * m[5]) - m[3] * (m[1] * m[8] - m[7] * m[2])
            + m[6] * (m[1] * m[5] - m[4] * m[2]);
        assert!((determinant - 1.0).abs() < 0.000003);
    }

    #[test]
    fn invalid_navigation_and_clock_never_create_nonfinite_uniforms() {
        let view = PanoramaView::default();
        for viewport in [[0.0, 100.0], [100.0, -1.0], [f64::INFINITY, 100.0]] {
            assert!(view.begin_drag([0.0, 0.0], viewport).is_none());
        }
        let drag = view.begin_drag([0.0, 0.0], [100.0, 100.0]).unwrap();
        assert!(drag.orientation_at([f64::NAN, 0.0]).is_none());
        assert!(CelestialRotation::axis_angle([0.0; 3], 1.0).is_none());
        assert!(CelestialRotation::axis_angle([f64::INFINITY, 0.0, 0.0], 1.0).is_none());
        for t in [0.0, -86400.0, 1.7e9, f64::MAX, f64::NAN, f64::INFINITY] {
            assert!(
                panorama_rotation(t, 1440)
                    .matrix()
                    .iter()
                    .all(|v| v.is_finite())
            );
        }
        close(
            &panorama_rotation(1234.0, 1440).matrix(),
            &panorama_rotation(1234.0 + 86400.0, 1440).matrix(),
        );
    }
}
