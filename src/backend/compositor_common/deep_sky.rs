//! Observed image planes at catalogue positions, in light-years from the Sun.
//! Image direction, angular extent and north angle come from the source WCS
//! metadata; approximate distance estimates do not reconstruct internal gas.

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
