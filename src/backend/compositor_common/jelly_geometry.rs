//! Fine display geometry driven by the same poses as the WaterLily obstacle.
//!
//! The simulation remains a coarse three-dimensional fluid solve. None of these
//! vertices is voxelized: curved membranes, pleats and trailing fibres retain
//! their shape independently of the fluid and display-volume resolution.

use super::waterlily::JellyPose;
use std::f32::consts::{PI, TAU};

/// Interleaved GL attributes. Colors are straight RGBA; the fragment shader
/// applies view-dependent tissue lighting and premultiplies exactly once.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct JellyVertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
    pub color: [f32; 4],
}

#[derive(Clone)]
struct Triangle {
    vertices: [JellyVertex; 3],
    distance_squared: f32,
}

const MAX_JELLIES: usize = 5;
// Fixed topology bounds all allocations independently of pose values.
const MAX_TRIANGLES: usize = 550_000;

type V3 = [f32; 3];
fn add(a: V3, b: V3) -> V3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}
fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn mul(a: V3, s: f32) -> V3 {
    [a[0] * s, a[1] * s, a[2] * s]
}
fn dot(a: V3, b: V3) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn cross(a: V3, b: V3) -> V3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}
fn unit(v: V3) -> V3 {
    let norm = dot(v, v).sqrt();
    if norm > 1e-12 {
        mul(v, norm.recip())
    } else {
        [0.0, 1.0, 0.0]
    }
}
fn smooth(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

struct Builder {
    triangles: Vec<Triangle>,
    eye: V3,
}
impl Builder {
    fn triangle(&mut self, a: JellyVertex, b: JellyVertex, c: JellyVertex) {
        if self.triangles.len() >= MAX_TRIANGLES {
            return;
        }
        if dot(
            cross(sub(b.position, a.position), sub(c.position, a.position)),
            cross(sub(b.position, a.position), sub(c.position, a.position)),
        ) < 1e-24
        {
            return;
        }
        let centre = mul(add(add(a.position, b.position), c.position), 1.0 / 3.0);
        let offset = sub(centre, self.eye);
        self.triangles.push(Triangle {
            vertices: [a, b, c],
            distance_squared: dot(offset, offset),
        });
    }
    fn quad(&mut self, a: JellyVertex, b: JellyVertex, c: JellyVertex, d: JellyVertex) {
        self.triangle(a, b, c);
        self.triangle(a, c, d);
    }
    fn grid(&mut self, nu: usize, nv: usize, sample: impl Fn(f32, f32) -> JellyVertex) {
        let mut previous = (0..=nu)
            .map(|i| sample(i as f32 / nu as f32, 0.0))
            .collect::<Vec<_>>();
        let mut next = Vec::with_capacity(nu + 1);
        for j in 1..=nv {
            next.clear();
            next.extend((0..=nu).map(|i| sample(i as f32 / nu as f32, j as f32 / nv as f32)));
            for i in 0..nu {
                self.quad(previous[i], previous[i + 1], next[i + 1], next[i]);
            }
            std::mem::swap(&mut previous, &mut next);
        }
    }
    fn tube(
        &mut self,
        segments: usize,
        sides: usize,
        color: [f32; 4],
        curve: impl Fn(f32) -> (V3, f32),
    ) {
        self.grid(sides, segments, |u, q| {
            let (centre, radius) = curve(q);
            let before = curve((q - 0.002).max(0.0)).0;
            let after = curve((q + 0.002).min(1.0)).0;
            let tangent = unit(sub(after, before));
            let reference = if tangent[1].abs() < 0.9 {
                [0.0, 1.0, 0.0]
            } else {
                [1.0, 0.0, 0.0]
            };
            let right = unit(cross(tangent, reference));
            let up = cross(tangent, right);
            let (s, c) = (TAU * u).sin_cos();
            let normal = add(mul(right, c), mul(up, s));
            let mut rgba = color;
            rgba[3] *= (1.0 - smooth(0.94, 1.0, q)).max(0.02);
            JellyVertex {
                position: add(centre, mul(normal, radius)),
                normal,
                color: rgba,
            }
        });
    }
}

fn bell_limit(pose: &JellyPose) -> f32 {
    ((pose.mouth_y - pose.center[1] + pose.axis_shift) / pose.radius)
        .clamp(-0.9, 0.2)
        .acos()
}

fn bell_point(p: &JellyPose, azimuth: f32, polar: f32) -> (V3, V3) {
    let limit = bell_limit(p);
    let rim = smooth(0.70, 1.0, polar / limit);
    // The high-frequency pleats are geometric and remain attached to the
    // contracting membrane, rather than a texture sliding over a voxel dome.
    let ribs = (32.0 * azimuth + 0.38 * (5.0 * azimuth).sin() + 0.2 * p.theta.sin()).cos();
    let scallop = p.radius * 0.014 * rim * ribs;
    let organic = 0.022 * (3.0 * azimuth + p.theta).sin() * polar.sin().powi(2);
    let radius = p.radius * (1.0 + organic) + scallop;
    let (sin_p, cos_p) = polar.sin_cos();
    let (sin_a, cos_a) = azimuth.sin_cos();
    // A modest display-only aspect refinement keeps the lip registered to
    // the physical mouth while giving the bell a broader, softer silhouette.
    let width = 1.06;
    let height = 0.90;
    let x = width * radius * sin_p * cos_a / p.squeeze;
    let z = width * radius * sin_p * sin_a / p.squeeze;
    let y = radius * cos_p;
    (
        [
            p.center[0] + x,
            p.mouth_y + height * (p.center[1] + y - p.axis_shift - p.mouth_y),
            p.center[2] + z,
        ],
        unit([
            p.squeeze * p.squeeze * x / (width * width),
            y / height,
            p.squeeze * p.squeeze * z / (width * width),
        ]),
    )
}

fn add_bell(b: &mut Builder, p: &JellyPose, high: bool) {
    let (azimuths, rings) = if high { (128, 48) } else { (64, 24) };
    let limit = bell_limit(p);
    b.grid(azimuths, rings, |u, v| {
        let phi = TAU * u;
        let (position, normal) = bell_point(p, phi, limit * v);
        let rim = smooth(0.76, 1.0, v);
        let vein = ((32.0 * phi + 0.38 * (5.0 * phi).sin()).cos().max(0.0)).powi(12)
            * smooth(0.58, 0.88, v);
        let lavender = [0.78 + 0.10 * rim, 0.65 + 0.15 * rim, 0.98 + 0.02 * rim];
        JellyVertex {
            position,
            normal,
            color: [
                lavender[0] + 0.035 * vein,
                lavender[1] + 0.035 * vein,
                lavender[2],
                0.14 + 0.095 * (1.0 - v).powi(2) + 0.08 * rim,
            ],
        }
    });
    // A narrow folded skirt, not a thick opaque torus, catches the rim light.
    b.grid(azimuths, if high { 10 } else { 5 }, |u, v| {
        let phi = TAU * u;
        let (edge, _) = bell_point(p, phi, limit);
        let (sa, ca) = phi.sin_cos();
        let fold = (48.0 * phi + 0.55 * (7.0 * phi).sin() + 0.22 * p.theta.sin()).sin();
        let width = p.radius * 0.065;
        let radial = -width * v;
        let height = p.radius * (0.035 * (PI * v).sin() * fold + 0.018 * v);
        JellyVertex {
            position: add(edge, [radial * ca, height, radial * sa]),
            normal: unit([ca * 0.5, 0.6 + 0.35 * fold, sa * 0.5]),
            color: [0.86, 0.73, 1.0, 0.23],
        }
    });
    // Thin radial canals follow exact surface curves from crown to lip.
    for index in 0..if high { 32 } else { 16 } {
        let phi = TAU * index as f32 / if high { 32.0 } else { 16.0 };
        b.tube(
            if high { 40 } else { 24 },
            3,
            [0.80, 0.71, 0.98, 0.10],
            |q| {
                let start = 0.42 + 0.14 * (index as f32 * 1.73).sin();
                let polar = limit * (start + (1.0 - start) * q);
                let (point, normal) = bell_point(p, phi + 0.025 * (PI * q).sin(), polar);
                (
                    add(point, mul(normal, 0.002 * p.radius)),
                    p.radius * (0.0020 + 0.0035 * q),
                )
            },
        );
    }
}

fn add_organs(b: &mut Builder, p: &JellyPose, high: bool) {
    for organ in 0..4 {
        let angle = organ as f32 * 0.5 * PI + 0.4 + 0.15 * p.theta.sin();
        let (s, c) = angle.sin_cos();
        let centre = add(
            p.center,
            [
                0.36 * p.radius * c / p.squeeze,
                0.29 * p.radius - p.axis_shift,
                0.36 * p.radius * s / p.squeeze,
            ],
        );
        // Four rose horseshoe gonads with real gaps, visible through the bell.
        b.grid(
            if high { 36 } else { 20 },
            if high { 10 } else { 6 },
            |u, v| {
                let a = angle + 0.25 + (TAU - 0.5) * u;
                let (sa, ca) = a.sin_cos();
                let (sv, cv) = (TAU * v).sin_cos();
                let major = p.radius * (0.105 + 0.012 * (3.0 * a + organ as f32).sin());
                let minor = p.radius * 0.059 * (1.0 + 0.14 * (2.0 * a - angle).cos());
                JellyVertex {
                    position: add(
                        centre,
                        [
                            (major + minor * cv) * ca,
                            minor * 0.58 * sv,
                            (major + minor * cv) * sa,
                        ],
                    ),
                    normal: unit([cv * ca, sv / 0.58, cv * sa]),
                    color: [0.95, 0.62, 0.79, 0.34],
                }
            },
        );
    }
}

fn arm_centre(p: &JellyPose, arm: usize, q: f32) -> V3 {
    let angle = arm as f32 * 0.5 * PI + 0.785 + 0.22 * p.theta.sin();
    let arm_phase = arm as f32;
    let spatial =
        (6.7 + 1.4 * (0.9 * arm_phase).sin()) * q + 2.1 * arm_phase + 0.6 * (1.7 * arm_phase).sin();
    let wave = p.theta + spatial;
    let secondary = (2.0 * p.theta + 16.0 * q + arm as f32).sin();
    let anchor = p.radius * (0.40 - 0.08 * q);
    let sway = p.radius * (0.045 + 0.15 * q) * (0.78 + 0.22 * (2.3 * arm_phase).cos().powi(2));
    [
        p.center[0] + anchor * angle.cos() + sway * (wave.sin() + 0.24 * secondary),
        p.mouth_y - (1.95 + 0.22 * (1.7 * arm_phase + 0.4).sin()) * p.radius * q,
        p.center[2] + anchor * angle.sin() + sway * (p.theta + 0.77 * spatial).cos(),
    ]
}

fn add_arms(b: &mut Builder, p: &JellyPose, high: bool) {
    let segments = if high { 112 } else { 56 };
    for arm in 0..4 {
        b.tube(
            segments,
            if high { 8 } else { 5 },
            [0.91, 0.76, 0.93, 0.43],
            |q| (arm_centre(p, arm, q), p.radius * (0.030 * (1.0 - 0.70 * q))),
        );
        for layer in 0..if high { 4 } else { 3 } {
            // Each pleated ribbon curls around a centreline, with separately
            // phased folds and a tapered pointed end. The normal is geometric.
            let sample = |u: f32, q: f32| -> V3 {
                let centre = arm_centre(p, arm, q);
                let phi =
                    layer as f32 * 0.5 * PI + 1.1 * arm as f32 + 10.0 * q - 0.55 * p.theta.sin();
                let cross = 2.0 * u - 1.0;
                let spread = p.radius * (0.020 + 0.135 * cross) * (1.0 - 0.56 * q);
                let fold = p.radius
                    * 0.067
                    * (24.0 * q + layer as f32 + 2.3 * cross - p.theta).sin()
                    * cross;
                add(centre, [spread * phi.cos(), fold, spread * phi.sin()])
            };
            b.grid(if high { 6 } else { 3 }, segments, |u, q| {
                let du = sub(
                    sample((u + 0.002).min(1.0), q),
                    sample((u - 0.002).max(0.0), q),
                );
                let dq = sub(
                    sample(u, (q + 0.002).min(1.0)),
                    sample(u, (q - 0.002).max(0.0)),
                );
                let fade = smooth(0.0, 0.025, q) * (1.0 - smooth(0.90, 1.0, q));
                JellyVertex {
                    position: sample(u, q),
                    normal: unit(cross(du, dq)),
                    color: [0.90, 0.76, 0.96, 0.46 * fade],
                }
            });
        }
    }
}

fn add_filaments(b: &mut Builder, p: &JellyPose, high: bool) {
    let count = if high { 40 } else { 24 };
    for strand in 0..count {
        let index = strand as f32;
        let angle = TAU * index / count as f32 + 0.18 * p.theta.sin();
        let length = 2.28 + 0.25 * (index * 1.71).sin();
        let rim_radius = 1.06 * p.radius * bell_limit(p).sin() / p.squeeze;
        b.tube(
            if high { 112 } else { 56 },
            if high { 5 } else { 4 },
            [0.88, 0.79, 1.0, 0.47],
            |q| {
                let anchor =
                    rim_radius * (if strand % 2 == 0 { 0.78 } else { 0.96 }) * (1.0 - 0.24 * q);
                let spatial = 5.34 * q + 1.9 * index;
                let wave = p.theta + spatial;
                let secondary = q * (2.0 * p.theta + 11.0 * q + index).sin();
                let sway = p.radius * (0.02 + 0.14 * q * q);
                let point = [
                    p.center[0] + anchor * angle.cos() + sway * (wave.sin() + 0.24 * secondary),
                    p.mouth_y - length * p.radius * q,
                    p.center[2] + anchor * angle.sin() + sway * (p.theta + 0.83 * spatial).cos(),
                ];
                (point, p.radius * (0.014 * (1.0 - 0.65 * q)).max(0.007))
            },
        );
    }
}

/// Build a bounded, globally back-to-front triangle list. Per-object sorting
/// alone would put transparent arms in front of nearer bells at crossings.
/// Fine tessellation limits the usual triangle-centroid painter approximation.
/// Invalid direct callers fail closed; the wire reader also validates poses.
pub(crate) fn build_jelly_mesh(
    poses: &[JellyPose],
    detail: u32,
    camera_pos: V3,
) -> Vec<JellyVertex> {
    if poses.len() > MAX_JELLIES
        || !(1..=2).contains(&detail)
        || !camera_pos.iter().all(|x| x.is_finite())
    {
        return Vec::new();
    }
    let mut builder = Builder {
        triangles: Vec::with_capacity(poses.len() * if detail == 2 { 100_000 } else { 25_000 }),
        eye: camera_pos,
    };
    for pose in poses {
        let valid = pose.center.iter().all(|x| x.is_finite() && x.abs() <= 0.5)
            && pose.radius.is_finite()
            && pose.radius > 0.0
            && pose.radius <= 0.25
            && pose.squeeze.is_finite()
            && (0.8..=1.2).contains(&pose.squeeze)
            && pose.theta.is_finite()
            && pose.theta.abs() <= TAU
            && pose.axis_shift.is_finite()
            && pose.axis_shift.abs() <= 0.25
            && pose.mouth_y.is_finite()
            && pose.mouth_y.abs() <= 0.5;
        if !valid {
            return Vec::new();
        }
        add_bell(&mut builder, pose, detail == 2);
        add_organs(&mut builder, pose, detail == 2);
        add_arms(&mut builder, pose, detail == 2);
        add_filaments(&mut builder, pose, detail == 2);
    }
    // Stable sort provides deterministic output even for equal-depth surfaces.
    builder
        .triangles
        .sort_by(|a, b| b.distance_squared.total_cmp(&a.distance_squared));
    let mut vertices = Vec::with_capacity(builder.triangles.len() * 3);
    for triangle in builder.triangles {
        vertices.extend_from_slice(&triangle.vertices);
    }
    vertices
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pose() -> JellyPose {
        JellyPose {
            center: [0.0, 0.10, 0.0],
            radius: 0.06,
            squeeze: 0.97,
            theta: 0.5,
            axis_shift: 0.01,
            mouth_y: 0.09,
        }
    }
    #[test]
    fn mesh_is_finite_bounded_and_globally_sorted() {
        let eye = [0.0, 0.20, -1.5];
        let vertices = build_jelly_mesh(&[pose()], 2, eye);
        assert!(!vertices.is_empty());
        assert_eq!(vertices.len() % 3, 0);
        assert!(vertices.len() <= 3 * MAX_TRIANGLES);
        let mut last = f32::INFINITY;
        for triangle in vertices.chunks_exact(3) {
            let centre = mul(
                add(
                    add(triangle[0].position, triangle[1].position),
                    triangle[2].position,
                ),
                1.0 / 3.0,
            );
            let depth = dot(sub(centre, eye), sub(centre, eye));
            assert!(depth <= last + 1e-6);
            last = depth;
            for v in triangle {
                assert!(
                    v.position
                        .iter()
                        .chain(v.normal.iter())
                        .chain(v.color.iter())
                        .all(|x| x.is_finite())
                );
                assert!((dot(v.normal, v.normal) - 1.0).abs() < 1e-4);
                assert!(v.color.iter().all(|x| (0.0..=1.0).contains(x)));
                assert!(v.position[1] >= pose().mouth_y - 2.60 * pose().radius);
            }
        }
    }
    #[test]
    fn mesh_detail_changes_sampling_not_pose_and_repeats_exactly() {
        let eye = [0.0, 0.2, -1.5];
        let p = pose();
        let low = build_jelly_mesh(&[p], 1, eye);
        let high = build_jelly_mesh(&[p], 2, eye);
        let again = build_jelly_mesh(&[p], 2, eye);
        assert!(high.len() > low.len());
        assert_eq!(high.len(), again.len());
        for (a, b) in high.iter().zip(again) {
            assert_eq!(a.position, b.position);
            assert_eq!(a.normal, b.normal);
            assert_eq!(a.color, b.color);
        }
        assert!(build_jelly_mesh(&[], 0, eye).is_empty());
        let mut invalid = pose();
        invalid.radius = f32::NAN;
        assert!(build_jelly_mesh(&[invalid], 2, eye).is_empty());
    }
}

#[cfg(test)]
mod phase_tests {
    use super::*;
    #[test]
    fn wrapped_pose_phase_does_not_jump_any_appendage() {
        let sample = |theta| {
            let p = JellyPose {
                center: [0.0, 0.1, 0.0],
                radius: 0.06,
                squeeze: 0.97,
                theta,
                axis_shift: 0.01,
                mouth_y: 0.09,
            };
            let mut b = Builder {
                triangles: Vec::new(),
                eye: [0.0, 0.2, -1.5],
            };
            add_bell(&mut b, &p, false);
            add_organs(&mut b, &p, false);
            add_arms(&mut b, &p, false);
            add_filaments(&mut b, &p, false);
            b.triangles
        };
        let before = sample(PI - 1.0e-4);
        let after = sample(-PI + 1.0e-4);
        assert_eq!(before.len(), after.len());
        for (a, b) in before.iter().zip(after) {
            for (va, vb) in a.vertices.iter().zip(b.vertices) {
                assert!(
                    dot(sub(va.position, vb.position), sub(va.position, vb.position)).sqrt()
                        < 1.0e-4,
                    "visible pose-wrap discontinuity {:?} -> {:?}",
                    va.position,
                    vb.position
                );
            }
        }
    }
}
