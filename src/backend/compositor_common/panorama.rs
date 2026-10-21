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
            "// JWM_DEEP_SKY_CATALOG_INCLUDE",
            include_str!("../x11/compositor/shaders/deep_sky_catalog.glsl"),
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
    min_fov: f32,
}

impl Default for PanoramaView {
    fn default() -> Self {
        Self {
            orientation: CelestialRotation::default(),
            fov: PANORAMA_DEFAULT_FOV,
            min_fov: PANORAMA_MIN_FOV,
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
            self.fov = degrees.clamp(self.min_fov, PANORAMA_MAX_FOV);
        }
    }

    pub(crate) fn set_min_fov(&mut self, degrees: f32) {
        if degrees.is_finite() && degrees > 0.0 {
            self.min_fov = degrees.clamp(0.001, PANORAMA_MIN_FOV);
        }
    }

    pub(crate) fn interpolate(self, destination: Self, amount: f64) -> Self {
        let amount = amount.clamp(0.0, 1.0);
        if amount <= 0.0 {
            return self;
        }
        if amount >= 1.0 {
            return destination;
        }
        let mut result = destination;
        result.orientation = self
            .orientation
            .interpolate(destination.orientation, amount);
        // Logarithmic lens motion crosses arcminute fields without a final jump.
        result.fov = ((f64::from(self.fov).ln() * (1.0 - amount)
            + f64::from(destination.fov).ln() * amount)
            .exp()) as f32;
        result
    }

    pub(crate) fn zoom_at(&mut self, steps: f64, point: [f64; 2], size: [f64; 2]) -> bool {
        let Some(before) = self.ray(point, size) else {
            return false;
        };
        if !self.zoom(steps) {
            return false;
        }
        let after = self.ray(point, size).unwrap_or(before);
        let axis = [
            after[1] * before[2] - after[2] * before[1],
            after[2] * before[0] - after[0] * before[2],
            after[0] * before[1] - after[1] * before[0],
        ];
        let sine = axis.iter().map(|v| v * v).sum::<f64>().sqrt();
        let cosine = after.iter().zip(before).map(|(a, b)| a * b).sum::<f64>();
        if let Some(rotation) = CelestialRotation::axis_angle(axis, sine.atan2(cosine)) {
            self.orientation = rotation.compose(self.orientation);
        }
        true
    }

    /// Telescope fields can be only a few arcminutes wide. Keep their lens
    /// limits local to the focused view so ordinary sky/body zoom is unchanged.
    pub(crate) fn focus_field(&mut self, degrees: f32) {
        if degrees.is_finite() && degrees > 0.0 {
            self.min_fov = (degrees / 16.0).clamp(0.001, PANORAMA_MIN_FOV);
            self.set_fov_degrees(degrees);
        }
    }

    pub(crate) fn project(self, direction: [f64; 3], viewport: [f64; 2]) -> Option<[f64; 2]> {
        self.begin_drag([0.0, 0.0], viewport)?;
        let matrix = self.matrix().map(f64::from);
        let camera: [f64; 3] = std::array::from_fn(|column| {
            (0..3)
                .map(|row| matrix[column * 3 + row] * direction[row])
                .sum()
        });
        if camera.iter().any(|v| !v.is_finite()) || camera[2] <= 0.0 {
            return None;
        }
        let scale = viewport[1] / (2.0 * (f64::from(self.fov).to_radians() * 0.5).tan());
        Some([
            viewport[0] * 0.5 + camera[0] / camera[2] * scale,
            viewport[1] * 0.5 + camera[1] / camera[2] * scale,
        ])
    }

    /// Positive wheel detents narrow the perspective lens. Bounded logarithmic
    /// zoom makes a burst equivalent to its separate detents, except at limits.
    pub(crate) fn zoom(&mut self, steps: f64) -> bool {
        if !steps.is_finite() || steps == 0.0 {
            return false;
        }
        let tangent = (f64::from(self.fov).to_radians() * 0.5).tan();
        let low = (f64::from(self.min_fov).to_radians() * 0.5).tan();
        let high = (f64::from(PANORAMA_MAX_FOV).to_radians() * 0.5).tan();
        let next = (tangent.ln() - steps.clamp(-64.0, 64.0) * 0.16)
            .clamp(low.ln(), high.ln())
            .exp();
        let fov = (2.0 * next.atan()).to_degrees() as f32;
        if (fov - self.fov).abs() < 0.00001 {
            return false;
        }
        self.fov = fov.clamp(self.min_fov, PANORAMA_MAX_FOV);
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

    /// Match panorama_ray's analytic pixel derivatives and cosmic_deep_sky's
    /// footprint floor. A camera rotation preserves these lengths, so locator
    /// picking can share the shader's angular distance even near a wide lens's
    /// edges without rotating either derivative on the CPU.
    pub(crate) fn angular_pixel_size(self, point: [f64; 2], viewport: [f64; 2]) -> Option<f64> {
        self.begin_drag(point, viewport)?;
        if !self.fov.is_finite() {
            return None;
        }
        let scale = 2.0 * (f64::from(self.fov).clamp(0.001, 110.0).to_radians() * 0.5).tan()
            / viewport[1].max(1.0);
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
        let derivative_length = |axis: usize| {
            let derivative: [f64; 3] = std::array::from_fn(|component| {
                ((if component == axis { scale } else { 0.0 })
                    - ray[component] * (ray[axis] * scale))
                    / length
            });
            derivative[0].hypot(derivative[1]).hypot(derivative[2])
        };
        let footprint = derivative_length(0).max(derivative_length(1));
        footprint.is_finite().then_some(footprint.max(1e-10))
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

    #[test]
    fn pointer_anchored_zoom_and_flights_keep_the_target_under_the_cursor() {
        let mut view = PanoramaView::default();
        let size = [1920.0, 1080.0];
        let point = [1300.0, 700.0];
        let before = view.ray(point, size).unwrap();
        assert!(view.zoom_at(5.0, point, size));
        let after = view.ray(point, size).unwrap();
        assert!(before.iter().zip(after).all(|(a, b)| (a - b).abs() < 1e-6));
        let mut destination = view;
        destination.focus_field(0.03);
        destination.set_orientation(CelestialRotation::axis_angle([1.0, 0.3, 0.7], 3.0).unwrap());
        for i in 0..=100 {
            let frame = view.interpolate(destination, i as f64 / 100.0);
            let m = frame.matrix();
            for a in 0..3 {
                for b in 0..3 {
                    let dot: f32 = (0..3).map(|r| m[a * 3 + r] * m[b * 3 + r]).sum();
                    assert!((dot - if a == b { 1.0 } else { 0.0 }).abs() < 1e-5);
                }
            }
            assert!(frame.fov_degrees() >= destination.fov_degrees());
        }
    }

    #[test]
    fn angular_pixel_size_matches_ray_derivatives_at_wide_lens_edges_and_after_rotation() {
        let rotation = CelestialRotation::axis_angle([0.4, -0.8, 0.3], 2.1).unwrap();
        let step = 0.01;
        for viewport in [
            [1920.0, 1080.0],
            [5120.0, 1440.0],
            [1080.0, 1920.0],
            [800.0, 600.0],
        ] {
            for fov in [110.0, 60.0, 2.0, 0.001] {
                let mut view = PanoramaView::default();
                view.set_min_fov(0.001);
                view.set_fov_degrees(fov);
                for fraction in [[0.5, 0.5], [0.03, 0.85], [0.95, 0.05], [-0.5, 1.25]] {
                    let point = [fraction[0] * viewport[0], fraction[1] * viewport[1]];
                    let unrotated = view.angular_pixel_size(point, viewport).unwrap();
                    view.set_orientation(rotation);
                    assert_eq!(view.angular_pixel_size(point, viewport), Some(unrotated));
                    let mut derivative_lengths = [0.0_f64; 2];
                    for axis in 0..2 {
                        let mut before = point;
                        let mut after = point;
                        before[axis] -= step;
                        after[axis] += step;
                        let before = view.ray(before, viewport).unwrap();
                        let after = view.ray(after, viewport).unwrap();
                        let derivative: [f64; 3] =
                            std::array::from_fn(|i| (after[i] - before[i]) / (2.0 * step));
                        derivative_lengths[axis] =
                            derivative[0].hypot(derivative[1]).hypot(derivative[2]);
                    }
                    let numerical = derivative_lengths[0].max(derivative_lengths[1]).max(1e-10);
                    assert!(
                        (unrotated - numerical).abs() < unrotated * 2e-6 + 1e-12,
                        "{viewport:?} {fov} {point:?}: {unrotated} != {numerical}"
                    );
                    view.set_orientation(CelestialRotation::default());
                }
            }
        }
    }

    #[test]
    fn angular_pixel_size_preserves_shader_scale_limits_and_rejects_invalid_inputs() {
        let view = PanoramaView::default();
        let center = view
            .angular_pixel_size([960.0, 540.0], [1920.0, 1080.0])
            .unwrap();
        let scale = 2.0 * (30.0_f64.to_radians()).tan() / 1080.0;
        assert!((center - scale).abs() < 1e-15);
        assert!(
            view.angular_pixel_size([1920.0, 540.0], [1920.0, 1080.0])
                .unwrap()
                < center
        );
        assert_eq!(
            view.angular_pixel_size([1920.0, 1080.0], [3840.0, 2160.0]),
            Some(center * 0.5)
        );
        // GLSL protects a subpixel render height with max(resolution.y, 1).
        assert_eq!(
            view.angular_pixel_size([0.25, 0.25], [0.5, 0.5]),
            Some(2.0 * (30.0_f64.to_radians()).tan())
        );
        assert_eq!(
            view.angular_pixel_size([5e19, 5e19], [1e20, 1e20]),
            Some(1e-10)
        );
        for viewport in [
            [0.0, 100.0],
            [100.0, -1.0],
            [f64::INFINITY, 100.0],
            [100.0, f64::NAN],
        ] {
            assert!(view.angular_pixel_size([0.0, 0.0], viewport).is_none());
        }
        for point in [[f64::NAN, 0.0], [0.0, f64::INFINITY]] {
            assert!(view.angular_pixel_size(point, [800.0, 600.0]).is_none());
        }
    }

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
    fn telescope_lens_and_projection_preserve_small_fields_and_overview_limits() {
        let mut view = PanoramaView::default();
        let overview = view;
        view.focus_field(0.06);
        let before = view.fov_degrees();
        assert!(view.zoom(3.0));
        assert!(view.fov_degrees() < 0.06);
        assert!(view.zoom(-3.0));
        assert!((view.fov_degrees() - before).abs() < 1e-7);
        let size = [1920.0, 1080.0];
        for point in [[0.0, 0.0], [960.0, 540.0], [1920.0, 1080.0]] {
            let ray = view.ray(point, size).unwrap();
            let projected = view.project(ray, size).unwrap();
            assert!(
                point
                    .iter()
                    .zip(projected)
                    .all(|(a, b)| (a - b).abs() < 1e-5)
            );
        }
        assert!(view.project([0.0, 0.0, -1.0], size).is_none());
        assert!(view.project([f64::NAN, 0.0, 1.0], size).is_none());
        assert_eq!(overview.fov_degrees(), PANORAMA_DEFAULT_FOV);
        let mut ordinary = overview;
        ordinary.zoom(64.0);
        assert_eq!(ordinary.fov_degrees(), PANORAMA_MIN_FOV);
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
