//! Observed projected fields and local volume cameras, in light-years from the Sun.
//! Image direction, angular extent and north angle come from the source WCS
//! metadata. Local volume depth is an illustrative model, not measured gas.

use super::animated_wallpaper::CelestialRotation;
use super::panorama::{PanoramaDrag, PanoramaView};

pub(crate) const GALAXY_ENTRY_FOV: f32 = 2.0;
pub(crate) const NEBULA_FOV: f32 = 50.0;

/// Locators keep a constant angular extent until they reach the entrance size.
/// Picking uses the same radius as the GLSL pass, including during a flight.
pub(crate) fn locator_radius(fov: f32, height: f64) -> f64 {
    (5.5 * (30.0_f64.to_radians().tan() / (f64::from(fov) * 0.5).to_radians().tan()))
        .clamp(5.5, (height * 0.42).max(5.5))
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct NebulaView {
    pub orientation: CelestialRotation,
    pub distance: f32,
}

impl Default for NebulaView {
    fn default() -> Self {
        Self {
            orientation: CelestialRotation::default(),
            distance: 3.2,
        }
    }
}

impl NebulaView {
    pub(crate) fn fitted(target: DeepSkyTarget, aspect: f64) -> Self {
        let width = target.half_size_ly[0] / target.half_size_ly[1];
        Self {
            distance: ((1.0_f64.max(width / aspect.max(0.01)) * 1.12
                / (f64::from(NEBULA_FOV) * 0.5).to_radians().tan())
                + 0.5) as f32,
            ..Self::default()
        }
    }

    pub(crate) fn zoom(&mut self, steps: f64) -> bool {
        if !steps.is_finite() || steps == 0.0 {
            return false;
        }
        let next = (f64::from(self.distance).ln() - steps.clamp(-64.0, 64.0) * 0.14)
            .clamp(0.7_f64.ln(), 30.0_f64.ln())
            .exp() as f32;
        let changed = (next - self.distance).abs() > 1e-6;
        self.distance = next;
        changed
    }

    pub(crate) fn begin_drag(self, point: [f64; 2], size: [f64; 2]) -> Option<PanoramaDrag> {
        let mut view = PanoramaView::default();
        view.set_orientation(self.orientation);
        view.set_fov_degrees(NEBULA_FOV);
        view.begin_drag(point, size)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DeepSkyTarget {
    pub position_ly: [f64; 3],
    pub right: [f64; 3],
    pub up: [f64; 3],
    pub half_size_ly: [f64; 2],
}

include!("deep_sky_catalog.rs");

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a.iter().zip(b).map(|(a, b)| a * b).sum()
}

impl DeepSkyTarget {
    pub(crate) fn distance_ly(self) -> f64 {
        dot(self.position_ly, self.position_ly).sqrt()
    }

    pub(crate) fn direction(self) -> [f64; 3] {
        let distance = self.distance_ly();
        self.position_ly.map(|v| v / distance)
    }

    /// Perspective ray/plane intersection. The same physical field, including
    /// its image orientation and near/far side, is used by rendering and picking.
    pub(crate) fn image_uv(self, ray: [f64; 3]) -> Option<[f64; 2]> {
        let depth = dot(ray, self.direction());
        if !depth.is_finite() || depth <= 0.0 {
            return None;
        }
        let t = self.distance_ly() / depth;
        Some([
            0.5 + dot(ray, self.right) * t / (2.0 * self.half_size_ly[0]),
            0.5 - dot(ray, self.up) * t / (2.0 * self.half_size_ly[1]),
        ])
    }

    pub(crate) fn closeup_fov(self, aspect: f64) -> f32 {
        let half_height = self.half_size_ly[1].max(self.half_size_ly[0] / aspect.max(0.01));
        (2.0 * (half_height * 1.12 / self.distance_ly()).atan()).to_degrees() as f32
    }
}

pub(crate) fn galactic_to_world(q: [f64; 3]) -> [f64; 3] {
    [
        -0.3746065934 * q[1] - 0.9271838546 * q[2],
        0.9271838546 * q[1] - 0.3746065934 * q[2],
        q[0],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locator_tracks_magnification_and_stays_bounded() {
        assert_eq!(locator_radius(60.0, 1080.0), 5.5);
        assert!(locator_radius(8.0, 1080.0) > 40.0);
        assert!(locator_radius(2.0, 1080.0) > 180.0);
        assert_eq!(locator_radius(0.001, 1080.0), 1080.0 * 0.42);
    }

    #[test]
    fn volume_camera_can_orbit_every_side_and_dolly_inside_without_invalid_uniforms() {
        let mut camera = NebulaView::fitted(DEEP_SKY_TARGETS[0], 16.0 / 9.0);
        let original = camera;
        assert!(camera.zoom(3.0));
        assert!(camera.distance < original.distance);
        assert!(camera.zoom(-3.0));
        assert!((camera.distance - original.distance).abs() < 1e-5);
        assert!(!camera.zoom(f64::NAN));
        for i in 0..2000 {
            camera.orientation = camera
                .begin_drag([0.0, 0.0], [800.0, 600.0])
                .unwrap()
                .orientation_at([i as f64 * 13.0, i as f64 * -7.0])
                .unwrap();
        }
        let m = camera.orientation.matrix();
        assert!(m.iter().all(|v| v.is_finite()));
        for column in 0..3 {
            assert!(
                ((0..3).map(|row| m[column * 3 + row].powi(2)).sum::<f32>() - 1.0).abs() < 1e-5
            );
        }
        camera.zoom(1e100);
        assert!((camera.distance - 0.7).abs() < 1e-5);
        camera.zoom(-1e100);
        assert_eq!(camera.distance, 30.0);
    }

    #[test]
    fn observed_fields_keep_distance_north_angle_and_physical_extent() {
        for (index, target) in DEEP_SKY_TARGETS.iter().enumerate() {
            let centre = target.image_uv(target.direction()).unwrap();
            assert!(centre.iter().all(|v| (v - 0.5).abs() < 1e-6), "{}", index);
            assert!(target.image_uv(target.direction().map(|v| -v)).is_none());
            assert!(dot(target.right, target.up).abs() < 1e-8);
            assert!((dot(target.right, target.right) - 1.0).abs() < 1e-8);
            let ray = std::array::from_fn(|axis| {
                target.position_ly[axis] + target.right[axis] * target.half_size_ly[0]
            });
            let edge = target.image_uv(ray).unwrap();
            assert!((edge[0] - 1.0).abs() < 1e-6);
            assert!((edge[1] - 0.5).abs() < 1e-6);
        }
        assert!((DEEP_SKY_TARGETS[0].distance_ly() - 1400.0).abs() < 1e-6);
        assert!((DEEP_SKY_TARGETS[4].distance_ly() - 2_500_000.0).abs() < 1e-4);
        assert!(DEEP_SKY_TARGETS[1].closeup_fov(16.0 / 9.0) < 0.1);
    }
}
