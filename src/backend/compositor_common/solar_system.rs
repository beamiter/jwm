//! Deterministic camera, navigation and picking for the illustrative solar scene.

/// The readable illustration uses compressed orbital distances and enlarged
/// bodies. Keep these values aligned with the analytic spheres in the shader.
pub(crate) const ORBIT_RADII: [f32; 8] = [2.5, 3.6, 4.8, 6.0, 7.5, 9.3, 11.0, 12.7];
pub(crate) const BODY_RADII: [f32; 9] = [1.12, 0.23, 0.40, 0.46, 0.33, 0.90, 0.76, 0.58, 0.55];
/// Polar/equatorial radii of the giant planets' 1-bar reference surfaces.
/// NASA GSFC fact sheets: 66854/71492, 54364/60268, 24973/25559, 24341/24764.
/// Keep aligned with `polar_ratio` in solar_system.frag. The smaller rocky
/// bodies retain spheres; these ratios do not imply resolved terrain.
pub(crate) const BODY_POLAR_RATIOS: [f32; 9] = [
    1.0, 1.0, 1.0, 1.0, 1.0, 0.9351256, 0.90203756, 0.97707266, 0.98291874,
];

pub(crate) fn body_center(body: u8, phases: &[f32; 8]) -> Option<[f32; 3]> {
    if body == 0 {
        return Some([0.0; 3]);
    }
    let index = usize::from(body - 1);
    let orbit = *ORBIT_RADII.get(index)?;
    let phase = phases[index];
    if !phase.is_finite() {
        return None;
    }
    let (sin, cos) = phase.sin_cos();
    Some([cos * orbit, sin * 0.025 * f32::from(body), sin * orbit])
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a.into_iter().zip(b).map(|(a, b)| a * b).sum()
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    std::array::from_fn(|i| a[i] - b[i])
}

fn normalize(v: [f32; 3]) -> Option<[f32; 3]> {
    let length = dot(v, v).sqrt();
    (length.is_finite() && length > f32::EPSILON).then(|| v.map(|v| v / length))
}

fn spheroid_distance(
    eye: [f32; 3],
    ray: [f32; 3],
    center: [f32; 3],
    radius: f32,
    axis: [f32; 3],
    polar_ratio: f32,
) -> Option<f32> {
    let q = sub(eye, center);
    let flattening = polar_ratio.recip().powi(2) - 1.0;
    let metric = |v: [f32; 3]| {
        let axial = dot(v, axis) * flattening;
        std::array::from_fn(|i| v[i] + axis[i] * axial)
    };
    let a = dot(ray, metric(ray));
    let b = dot(q, metric(ray));
    let c = dot(q, metric(q)) - radius * radius;
    let discriminant = b * b - a * c;
    if discriminant < 0.0 {
        return None;
    }
    let root = discriminant.sqrt();
    [(-b - root) / a, (-b + root) / a]
        .into_iter()
        .find(|distance| *distance > 0.0 && distance.is_finite())
}

/// All values are frozen together after a displayed frame. Picking a moving
/// planet must use the geometry the user saw, not a later decorative orbit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SolarProjection {
    pub eye: [f32; 3],
    /// Columns are camera right, up and forward (positive camera Z looks ahead).
    pub basis: [f32; 9],
    pub viewport: [u32; 2],
    pub phases: [f32; 8],
    /// Optional date-derived positions, frozen with the displayed pixels.
    /// The legacy illustrative phases remain available for old configurations.
    pub centers: Option<[[f32; 3]; 9]>,
    /// Isolate an inspected body so compressed neighbours cannot obscure it.
    pub focus: Option<u8>,
    /// Date-derived axial tilts, in the same frame as the rendered orbit planes.
    pub body_axes: Option<[[f32; 3]; 9]>,
    pub focal: f32,
}

impl SolarProjection {
    pub(crate) fn center(&self, body: u8) -> Option<[f32; 3]> {
        if let Some(centers) = &self.centers {
            centers
                .get(usize::from(body))
                .copied()
                .filter(|center| center.iter().all(|v| v.is_finite()))
        } else {
            body_center(body, &self.phases)
        }
    }

    fn axis(&self, body: u8) -> Option<[f32; 3]> {
        if let Some(axes) = &self.body_axes {
            return normalize(*axes.get(usize::from(body))?);
        }
        if body == 0 {
            return Some([0.0, 1.0, 0.0]);
        }
        let index = usize::from(body - 1);
        let obliquities = [0.034_f32, 177.4, 23.44, 25.19, 3.1, 26.7, 97.8, 28.3];
        let orbit_normal = normalize([0.0, *ORBIT_RADII.get(index)?, -0.025 * f32::from(body)])?;
        let (sin, cos) = obliquities[index].to_radians().sin_cos();
        normalize([sin, cos * orbit_normal[1], cos * orbit_normal[2]])
    }

    pub(crate) fn default_focal(viewport: [u32; 2]) -> f32 {
        let width = viewport[0] as f32;
        let height = (viewport[1] as f32).max(1.0);
        if width >= height * 1.25 {
            1.5
        } else {
            1.2 * (width / height).max(1.0)
        }
    }

    /// Match gl_FragCoord's pixel centre while accepting X11's top-left origin.
    pub(crate) fn ray_at(&self, x: f64, y: f64) -> Option<[f32; 3]> {
        if !x.is_finite()
            || !y.is_finite()
            || self.viewport.contains(&0)
            || x < 0.0
            || y < 0.0
            || x >= f64::from(self.viewport[0])
            || y >= f64::from(self.viewport[1])
            || !self
                .eye
                .iter()
                .chain(self.basis.iter())
                .all(|v| v.is_finite())
            || !self.focal.is_finite()
            || self.focal <= 0.0
        {
            return None;
        }
        let pixels = f64::from(self.viewport[0].min(self.viewport[1]));
        let u = ((x + 0.5 - f64::from(self.viewport[0]) * 0.5) / pixels) as f32;
        let v = ((f64::from(self.viewport[1]) * 0.5 - y - 0.5) / pixels) as f32;
        normalize(std::array::from_fn(|i| {
            self.basis[i] * u + self.basis[3 + i] * v + self.basis[6 + i] * self.focal
        }))
    }

    pub(crate) fn project(&self, point: [f32; 3]) -> Option<[f64; 2]> {
        let offset = sub(point, self.eye);
        let forward = [self.basis[6], self.basis[7], self.basis[8]];
        let depth = dot(offset, forward);
        if self.viewport.contains(&0) || !depth.is_finite() || depth <= 0.0 {
            return None;
        }
        let right = [self.basis[0], self.basis[1], self.basis[2]];
        let up = [self.basis[3], self.basis[4], self.basis[5]];
        let pixels = f64::from(self.viewport[0].min(self.viewport[1]));
        let scale = f64::from(self.focal / depth) * pixels;
        let point = [
            f64::from(self.viewport[0]) * 0.5 + f64::from(dot(offset, right)) * scale - 0.5,
            f64::from(self.viewport[1]) * 0.5 - f64::from(dot(offset, up)) * scale - 0.5,
        ];
        point.iter().all(|v| v.is_finite()).then_some(point)
    }

    /// Pick the nearest visible spheroid or Saturn's translucent ring. A ring
    /// pixel opens Saturn as well; the empty annulus never steals a sky drag.
    pub(crate) fn pick(&self, x: f64, y: f64) -> Option<u8> {
        let ray = self.ray_at(x, y)?;
        let mut nearest = f32::INFINITY;
        let mut selected = None;
        for (index, radius) in BODY_RADII.into_iter().enumerate() {
            let body = index as u8;
            if self.focus.is_some_and(|focus| focus != body) {
                continue;
            }
            let center = self.center(body)?;
            if let Some(distance) = spheroid_distance(
                self.eye,
                ray,
                center,
                radius,
                self.axis(body)?,
                BODY_POLAR_RATIOS[index],
            ) && distance < nearest
            {
                nearest = distance;
                selected = Some(body);
            }
        }
        if self.focus.is_some_and(|body| body != 6) {
            return selected;
        }
        let saturn = self.center(6)?;
        let normal = self.axis(6)?;
        let denominator = dot(ray, normal);
        if denominator.abs() > 0.0001 {
            let distance = dot(sub(saturn, self.eye), normal) / denominator;
            if distance > 0.0 && distance < nearest {
                let point = std::array::from_fn(|i| self.eye[i] + ray[i] * distance - saturn[i]);
                let radius = dot(point, point).sqrt();
                if ring_opacity(radius) > 0.001 {
                    selected = Some(6);
                }
            }
        }
        selected
    }
}

fn ring_opacity(radius: f32) -> f32 {
    let radius = radius / BODY_RADII[6];
    if radius <= 1.24 || radius >= 2.269 {
        return 0.0;
    }
    let smooth = |lo: f32, hi: f32| {
        let x = ((radius - lo) / (hi - lo)).clamp(0.0, 1.0);
        x * x * (3.0 - 2.0 * x)
    };
    let c = smooth(1.24, 1.25) * (1.0 - smooth(1.516, 1.526));
    let b = smooth(1.516, 1.526) * (1.0 - smooth(1.941, 1.951));
    let a = smooth(2.027, 2.037) * (1.0 - smooth(2.259, 2.269));
    0.12 * c + 0.78 * b + 0.44 * a
}

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
            elevation: 0.38,
            distance: 27.5,
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

    /// Proper rotation with +Z pointing out from the orbit target to the eye.
    /// The rendered camera's forward column is its negation, so this matrix can
    /// be passed directly to the same quaternion arcball used by Earth/Moon.
    pub(crate) fn orbit_matrix(self) -> [f32; 9] {
        let (sin, cos) = self.azimuth.sin_cos();
        let (elevation, horizontal) = self.elevation.sin_cos();
        [
            cos,
            0.0,
            -sin,
            -sin * elevation,
            horizontal,
            -cos * elevation,
            horizontal * sin,
            elevation,
            horizontal * cos,
        ]
    }

    pub(crate) fn focus(&mut self, body: u8, phases: &[f32; 8]) -> bool {
        let Some(center) = body_center(body, phases) else {
            return false;
        };
        self.focus_at(body, center)
    }

    /// Frame the illuminated hemisphere using the displayed body's position.
    pub(crate) fn focus_at(&mut self, body: u8, center: [f32; 3]) -> bool {
        let Some(radius) = BODY_RADII.get(usize::from(body)).copied() else {
            return false;
        };
        if !center.iter().all(|v| v.is_finite()) {
            return false;
        }
        // Approach the illuminated hemisphere. This is a navigation choice;
        // illumination and the orbit itself remain unchanged in world space.
        if body != 0 {
            let toward_sun = normalize(center.map(|v| -v)).unwrap_or([0.0, 0.0, 1.0]);
            let tangent =
                normalize([toward_sun[2], 0.0, -toward_sun[0]]).unwrap_or([1.0, 0.0, 0.0]);
            // A sunlit three-quarter portrait exposes the terminator and cloud
            // curvature. Looking straight down the light vector made the old
            // close-ups appear flat even with detailed observed textures.
            let view = normalize([
                toward_sun[0] * 0.86 + tangent[0] * 0.48,
                toward_sun[1] * 0.86 + 0.24,
                toward_sun[2] * 0.86 + tangent[2] * 0.48,
            ])
            .unwrap_or([0.0, 0.0, 1.0]);
            self.azimuth = view[0].atan2(view[2]);
            self.elevation = view[1].asin();
        }
        self.distance = radius * if body == 6 { 8.8 } else { 4.0 };
        true
    }

    pub(crate) fn zoom_body(&mut self, body: u8, steps: f64) -> bool {
        let Some(radius) = BODY_RADII.get(usize::from(body)).copied() else {
            return false;
        };
        if !steps.is_finite() {
            return false;
        }
        let old = self.distance;
        self.distance = (self.distance * (-steps.clamp(-20.0, 20.0) * 0.12).exp() as f32)
            .clamp(radius * 1.12, radius * 40.0);
        self.distance != old
    }

    pub(crate) fn projection(
        self,
        target: [f32; 3],
        orbit: [f32; 9],
        look: [f32; 9],
        viewport: [u32; 2],
        phases: [f32; 8],
        lens_scale: f32,
    ) -> SolarProjection {
        let mut basis = orbit;
        for component in &mut basis[6..9] {
            *component = -*component;
        }
        let mut viewed = [0.0; 9];
        for column in 0..3 {
            for row in 0..3 {
                viewed[column * 3 + row] = (0..3)
                    .map(|i| basis[i * 3 + row] * look[column * 3 + i])
                    .sum();
            }
        }
        let offset = if orbit == self.orbit_matrix() {
            self.eye()
        } else {
            std::array::from_fn(|i| orbit[6 + i] * self.distance)
        };
        SolarProjection {
            eye: std::array::from_fn(|i| target[i] + offset[i]),
            basis: viewed,
            viewport,
            phases,
            centers: None,
            focus: None,
            body_axes: None,
            focal: SolarProjection::default_focal(viewport) * lens_scale,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    const IDENTITY: [f32; 9] = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];

    #[test]
    fn tilted_oblate_silhouette_does_not_pick_the_old_spherical_polar_cap() {
        let ratio = BODY_POLAR_RATIOS[6];
        let hit = |eye, axis| spheroid_distance(eye, [0.0, 0.0, -1.0], [0.0; 3], 1.0, axis, ratio);
        assert!(hit([0.0, 0.95, 3.0], [0.0, 1.0, 0.0]).is_none());
        assert!(hit([0.0, 0.88, 3.0], [0.0, 1.0, 0.0]).is_some());
        assert!(hit([0.0, 0.95, 3.0], [1.0, 0.0, 0.0]).is_some());
        let pole = hit([0.0, 0.0, 3.0], [0.0, 0.0, 1.0]).unwrap();
        assert!((pole - (3.0 - ratio)).abs() < 0.00001);

        let projection = SolarProjection {
            eye: [0.0, 0.0, 3.0],
            basis: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, -1.0],
            viewport: [800, 600],
            phases: [0.0; 8],
            centers: Some([[0.0; 3]; 9]),
            focus: Some(6),
            body_axes: Some([[0.0, 1.0, 0.0]; 9]),
            focal: 1.5,
        };
        // The old circumscribed sphere covers this pixel. Neither the actual
        // polar silhouette nor a ray through the equatorial ring does.
        let sky = projection
            .project([0.0, BODY_RADII[6] * 0.98, 0.0])
            .unwrap();
        assert_eq!(projection.pick(sky[0], sky[1]), None);
        let surface = projection
            .project([0.0, BODY_RADII[6] * 0.82, 0.0])
            .unwrap();
        assert_eq!(projection.pick(surface[0], surface[1]), Some(6));
    }

    #[test]
    fn focused_planet_picking_matches_the_displayed_pixel_and_scales() {
        for viewport in [[1920, 1080], [600, 900], [512, 512]] {
            let phases = scene_phases(0.0).0;
            for body in 0..=8 {
                let mut camera = SolarCamera::default();
                assert!(camera.focus(body, &phases));
                let center = body_center(body, &phases).unwrap();
                let projection = camera.projection(
                    center,
                    camera.orbit_matrix(),
                    IDENTITY,
                    viewport,
                    phases,
                    1.0,
                );
                let pixel = projection.project(center).unwrap();
                assert_eq!(
                    projection.pick(pixel[0], pixel[1]),
                    Some(body),
                    "body {body}, viewport {viewport:?}"
                );
                assert!((pixel[0] - (f64::from(viewport[0]) * 0.5 - 0.5)).abs() < 0.002);
                assert!((pixel[1] - (f64::from(viewport[1]) * 0.5 - 0.5)).abs() < 0.002);
                let ray = projection.ray_at(pixel[0], pixel[1]).unwrap();
                let expected = normalize(sub(center, projection.eye)).unwrap();
                assert!(
                    ray.into_iter()
                        .zip(expected)
                        .all(|(a, b)| (a - b).abs() < 0.00001)
                );
            }
        }
    }

    #[test]
    fn free_look_turns_the_whole_scene_and_has_no_pole_singularity() {
        let phases = scene_phases(0.0).0;
        for elevation in [
            -std::f32::consts::FRAC_PI_2,
            0.0,
            std::f32::consts::FRAC_PI_2,
        ] {
            let camera = SolarCamera {
                elevation,
                ..SolarCamera::default()
            };
            let facing = camera.projection(
                [0.0; 3],
                camera.orbit_matrix(),
                IDENTITY,
                [800, 600],
                phases,
                1.0,
            );
            assert_eq!(facing.pick(399.5, 299.5), Some(0));
            let away = camera.projection(
                [0.0; 3],
                camera.orbit_matrix(),
                [-1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, -1.0],
                [800, 600],
                phases,
                1.0,
            );
            assert_eq!(away.pick(399.5, 299.5), None);
            assert!(away.project([0.0; 3]).is_none());
            assert!(
                away.ray_at(399.5, 299.5)
                    .unwrap()
                    .iter()
                    .all(|v| v.is_finite())
            );
        }
    }

    #[test]
    fn pending_orbit_time_cannot_change_a_displayed_hit() {
        let camera = SolarCamera::default();
        let phases = scene_phases(0.0).0;
        let frozen = camera.projection(
            [0.0; 3],
            camera.orbit_matrix(),
            IDENTITY,
            [1920, 1080],
            phases,
            1.0,
        );
        let earth_pixel = frozen.project(body_center(3, &phases).unwrap()).unwrap();
        assert_eq!(frozen.pick(earth_pixel[0], earth_pixel[1]), Some(3));
        let future = SolarProjection {
            phases: scene_phases(300.0).0,
            ..frozen
        };
        assert_ne!(future.pick(earth_pixel[0], earth_pixel[1]), Some(3));
        assert_eq!(frozen.pick(earth_pixel[0], earth_pixel[1]), Some(3));
    }

    #[test]
    fn body_zoom_never_enters_the_sphere_or_changes_the_orbit_phase() {
        let phases = scene_phases(321.0).0;
        for body in 0..=8 {
            let mut camera = SolarCamera::default();
            camera.focus(body, &phases);
            let initial = camera;
            assert!(camera.zoom_body(body, 4.0));
            assert!(camera.distance < initial.distance);
            assert!(camera.zoom_body(body, -4.0));
            assert!((camera.distance - initial.distance).abs() < 0.00001);
            for _ in 0..8 {
                camera.zoom_body(body, 1e100);
            }
            assert_eq!(camera.distance, BODY_RADII[usize::from(body)] * 1.12);
            for _ in 0..8 {
                camera.zoom_body(body, -1e100);
            }
            assert_eq!(camera.distance, BODY_RADII[usize::from(body)] * 40.0);
            assert_eq!(camera.azimuth, initial.azimuth);
            assert_eq!(camera.elevation, initial.elevation);
            let valid = camera;
            assert!(!camera.zoom_body(body, f64::NAN));
            assert!(!camera.focus(9, &phases));
            assert!(!camera.zoom_body(255, 3.0));
            assert_eq!(camera, valid);
        }
    }

    #[test]
    fn picking_rejects_invalid_coordinates_and_invisible_ring_gaps() {
        let camera = SolarCamera::default();
        let projection = camera.projection(
            [0.0; 3],
            camera.orbit_matrix(),
            IDENTITY,
            [800, 600],
            scene_phases(0.0).0,
            1.0,
        );
        for point in [
            [f64::NAN, 0.0],
            [0.0, f64::INFINITY],
            [-1.0, 10.0],
            [800.0, 0.0],
            [0.0, 600.0],
        ] {
            assert!(projection.pick(point[0], point[1]).is_none());
        }
        assert_eq!(ring_opacity(1.23 * BODY_RADII[6]), 0.0);
        assert_eq!(ring_opacity(2.27 * BODY_RADII[6]), 0.0);
        assert!(ring_opacity(1.2) > 0.1);
        assert_eq!(ring_opacity(2.0 * BODY_RADII[6]), 0.0); // Cassini division
        assert!(
            SolarProjection {
                viewport: [0, 600],
                ..projection
            }
            .ray_at(0.0, 0.0)
            .is_none()
        );
        assert!(
            SolarProjection {
                focal: f32::NAN,
                ..projection
            }
            .ray_at(0.0, 0.0)
            .is_none()
        );
    }

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
pub(crate) const SPIN_HOURS: [f64; 8] = [
    1407.6, 5832.6, 23.9345, 24.6229, 9.9250, 10.656, 17.24, 16.11,
];

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
