//! Color-space conversion math for the wp-color-management render path.
//!
//! This module produces, for a given (surface description, output description)
//! pair, the inputs the shader pipeline would need to:
//!   1. Decode the surface's encoded RGB into scene-linear (inverse EOTF).
//!   2. Convert RGB primaries via the chromaticity-adapted 3x3 matrix
//!      `M_out_from_in = M_xyz_to_rgb(out) · CAT(in_white→out_white) · M_rgb_to_xyz(in)`.
//!   3. Re-encode for the output (forward EOTF) — handled by the existing
//!      postprocess stage; not produced here.
//!
//! It is intentionally *math only*: no GL state, no shader bindings. The render
//! loop builds a `ColorTransform`, then a future slice will plumb it into the
//! GLES surface element. Keeping the math here lets us unit-test gamut math
//! without a display, which is the only kind of verification we can do
//! without HDR HW.

use crate::backend::wayland_udev::color_management::ParametricParams;

/// CIE xy chromaticities of a single primary (or the white point), in normalized
/// space (i.e. raw xy, not the wp-color-management ×1_000_000 scaling).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Chromaticity {
    pub x: f32,
    pub y: f32,
}

/// RGB primaries (red, green, blue) plus the white point xy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorSpacePrimaries {
    pub r: Chromaticity,
    pub g: Chromaticity,
    pub b: Chromaticity,
    pub w: Chromaticity,
}

impl ColorSpacePrimaries {
    pub const SRGB_D65: Self = Self {
        r: Chromaticity { x: 0.640, y: 0.330 },
        g: Chromaticity { x: 0.300, y: 0.600 },
        b: Chromaticity { x: 0.150, y: 0.060 },
        w: Chromaticity { x: 0.3127, y: 0.3290 },
    };
    pub const BT2020_D65: Self = Self {
        r: Chromaticity { x: 0.708, y: 0.292 },
        g: Chromaticity { x: 0.170, y: 0.797 },
        b: Chromaticity { x: 0.131, y: 0.046 },
        w: Chromaticity { x: 0.3127, y: 0.3290 },
    };

    /// Reconstruct primaries from the wp-color-management ParametricParams.
    /// Falls back to sRGB when neither explicit `primaries` nor a known named
    /// primary is set.
    pub fn from_params(p: &ParametricParams) -> Self {
        // Explicit chromaticities take precedence — wp-color-management says
        // `primaries` is authoritative when both fields are set.
        if let Some(prim) = p.primaries {
            let f = |raw: i32| raw as f32 / 1_000_000.0;
            return Self {
                r: Chromaticity { x: f(prim[0]), y: f(prim[1]) },
                g: Chromaticity { x: f(prim[2]), y: f(prim[3]) },
                b: Chromaticity { x: f(prim[4]), y: f(prim[5]) },
                w: Chromaticity { x: f(prim[6]), y: f(prim[7]) },
            };
        }
        match p.primaries_named {
            // wp_color_manager_v1::Primaries::Bt2020 = 6
            Some(6) => Self::BT2020_D65,
            // Srgb = 1 (also the default for everything else we'd recognize)
            _ => Self::SRGB_D65,
        }
    }
}

/// Electro-optical transfer functions a surface can carry. Stored as a kind
/// rather than a closure so the resulting struct is `Copy` + can be uploaded
/// to a shader as an int uniform later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferKind {
    /// Linear — no decode needed.
    Linear,
    /// y = x ^ gamma. `gamma_x10000` = gamma × 10_000 (matches wp-cm encoding).
    Power { gamma_x10000: u32 },
    /// BT.1886 ≈ pure 2.4 power.
    Bt1886,
    /// Gamma 2.2 (legacy SDR display reference).
    Gamma22,
    /// Perceptual Quantizer (SMPTE ST 2084). Output is normalized 0..1 → 0..10000 cd/m².
    St2084Pq,
    /// Hybrid Log-Gamma (Rec. ITU-R BT.2100 / ARIB STD-B67).
    Hlg,
}

impl TransferKind {
    /// Map a wp-color-management ParametricParams to a single TransferKind.
    /// Prefers named TF; falls back to tf_power; defaults to Gamma22 when
    /// neither is present (matches our srgb_params() fallback).
    pub fn from_params(p: &ParametricParams) -> Self {
        if let Some(tf) = p.tf_named {
            return match tf {
                // wp_color_manager_v1::TransferFunction values
                1 => Self::Bt1886,
                2 => Self::Gamma22,
                5 => Self::Linear,
                11 => Self::St2084Pq,
                13 => Self::Hlg,
                _ => Self::Gamma22,
            };
        }
        if let Some(g) = p.tf_power {
            return Self::Power { gamma_x10000: g };
        }
        Self::Gamma22
    }

    /// Shader-side discriminant. The numeric assignment is part of the public
    /// API contract between Rust and the GLSL window shader and MUST be kept
    /// in lockstep with the `if` chain in `decode_eotf`/`encode_eotf`.
    pub fn shader_id(self) -> i32 {
        match self {
            Self::Linear => 0,
            Self::Power { .. } => 1,
            Self::Bt1886 => 2,
            Self::Gamma22 => 3,
            Self::St2084Pq => 4,
            Self::Hlg => 5,
        }
    }

    /// Companion gamma value for the `Power` variant. For every other variant
    /// returns `1.0` so the corresponding uniform always has a defined value
    /// (GLSL undefined-uniform reads are implementation-defined; binding 1.0
    /// makes the value harmless if a TF branch accidentally consults it).
    pub fn gamma_for_shader(self) -> f32 {
        match self {
            Self::Power { gamma_x10000 } => (gamma_x10000 as f32 / 10_000.0).max(1e-3),
            _ => 1.0,
        }
    }

    /// Apply this curve's inverse to a value in the curve's encoded range.
    /// Returns scene-linear light, normalized to 1.0 = display reference white
    /// for SDR-style curves, or 1.0 = 10000 cd/m² for PQ. HLG is normalized so
    /// 1.0 corresponds to the system-defined nominal peak.
    pub fn inverse(self, x: f32) -> f32 {
        let x = x.clamp(0.0, 1.0);
        match self {
            Self::Linear => x,
            Self::Power { gamma_x10000 } => {
                let g = gamma_x10000 as f32 / 10_000.0;
                x.powf(g.max(1e-3))
            }
            // Both BT.1886 and Gamma22 are well-modeled as pure powers at the
            // precision we care about for shader inversion; BT.1886 is 2.4,
            // Gamma22 is 2.2. The black-lift compensation in true BT.1886 is
            // tiny at typical display contrast and irrelevant for our purpose.
            Self::Bt1886 => x.powf(2.4),
            Self::Gamma22 => x.powf(2.2),
            Self::St2084Pq => pq_inverse(x),
            Self::Hlg => hlg_inverse(x),
        }
    }
}

/// PQ (SMPTE ST 2084) inverse: encoded 0..1 → linear 0..1 representing 0..10000 cd/m².
fn pq_inverse(e: f32) -> f32 {
    const M1: f32 = 0.1593017578125;
    const M2: f32 = 78.84375;
    const C1: f32 = 0.8359375;
    const C2: f32 = 18.8515625;
    const C3: f32 = 18.6875;
    let ep_m2 = e.powf(1.0 / M2);
    let num = (ep_m2 - C1).max(0.0);
    let den = C2 - C3 * ep_m2;
    if den.abs() < 1e-12 { 0.0 } else { (num / den).powf(1.0 / M1) }
}

/// HLG inverse: encoded 0..1 → linear 0..1 (system-relative).
fn hlg_inverse(e: f32) -> f32 {
    const A: f32 = 0.17883277;
    const B: f32 = 1.0 - 4.0 * A; // 0.28466892
    // C = 0.5 - A * ln(4A); precomputed to keep this a plain fn
    const C: f32 = 0.559_910_7;
    if e <= 0.5 {
        (e * e) / 3.0
    } else {
        (((e - C) / A).exp() + B) / 12.0
    }
}

/// Result of building a surface→output color transform: the inverse EOTF the
/// renderer should apply to surface samples, the 3x3 matrix that takes linear
/// surface RGB into linear output RGB, and the forward EOTF kind for the
/// output. Stored row-major; intended to be uploaded as a `mat3` to GLSL.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorTransform {
    pub inverse_eotf: TransferKind,
    pub matrix_row_major: [f32; 9],
    pub forward_eotf: TransferKind,
}

impl ColorTransform {
    /// Build the transform that maps surface-described colors into the output's
    /// linear color space. Returns `None` when the transform is functionally an
    /// identity (same primaries, same EOTF) — the renderer can skip the pass
    /// entirely in that case.
    pub fn build(surface: &ParametricParams, output: &ParametricParams) -> Option<Self> {
        let surface_prim = ColorSpacePrimaries::from_params(surface);
        let output_prim = ColorSpacePrimaries::from_params(output);
        let in_tf = TransferKind::from_params(surface);
        let out_tf = TransferKind::from_params(output);

        let same_primaries = primaries_match(&surface_prim, &output_prim);
        let same_eotf = in_tf == out_tf;
        if same_primaries && same_eotf {
            return None;
        }

        let matrix = if same_primaries {
            IDENTITY_3X3
        } else {
            rgb_to_rgb_matrix(&surface_prim, &output_prim)
        };
        Some(Self {
            inverse_eotf: in_tf,
            matrix_row_major: matrix,
            forward_eotf: out_tf,
        })
    }
}

const IDENTITY_3X3: [f32; 9] = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];

fn primaries_match(a: &ColorSpacePrimaries, b: &ColorSpacePrimaries) -> bool {
    const TOL: f32 = 0.001;
    let close = |p: Chromaticity, q: Chromaticity| {
        (p.x - q.x).abs() < TOL && (p.y - q.y).abs() < TOL
    };
    close(a.r, b.r) && close(a.g, b.g) && close(a.b, b.b) && close(a.w, b.w)
}

/// Compute the 3x3 RGB→XYZ matrix for the given primaries.
/// Derived from the standard "primary matrix" construction: choose scaling
/// factors S_r, S_g, S_b so that [S_r, S_g, S_b] · 1 = whitepoint XYZ.
fn rgb_to_xyz_matrix(p: &ColorSpacePrimaries) -> [f32; 9] {
    let to_xyz = |c: Chromaticity| -> (f32, f32, f32) {
        // X = x/y, Y = 1, Z = (1-x-y)/y. Use Y=1 by convention.
        if c.y.abs() < 1e-9 {
            return (0.0, 0.0, 0.0);
        }
        (c.x / c.y, 1.0, (1.0 - c.x - c.y) / c.y)
    };
    let (xr, yr, zr) = to_xyz(p.r);
    let (xg, yg, zg) = to_xyz(p.g);
    let (xb, yb, zb) = to_xyz(p.b);
    let (xw, _yw, zw) = to_xyz(p.w);
    // Solve M · [S_r, S_g, S_b]^T = [Xw, Yw=1, Zw]^T where
    //   M = [[xr xg xb], [yr yg yb], [zr zg zb]]
    let det = xr * (yg * zb - yb * zg)
        - xg * (yr * zb - yb * zr)
        + xb * (yr * zg - yg * zr);
    if det.abs() < 1e-12 {
        return IDENTITY_3X3;
    }
    let inv_det = 1.0 / det;
    // Inverse of 3x3 columns is the matrix of cofactors transposed × 1/det.
    let inv = [
        (yg * zb - yb * zg) * inv_det,
        -(xg * zb - xb * zg) * inv_det,
        (xg * yb - xb * yg) * inv_det,
        -(yr * zb - yb * zr) * inv_det,
        (xr * zb - xb * zr) * inv_det,
        -(xr * yb - xb * yr) * inv_det,
        (yr * zg - yg * zr) * inv_det,
        -(xr * zg - xg * zr) * inv_det,
        (xr * yg - xg * yr) * inv_det,
    ];
    // S = M^{-1} · whitepoint_XYZ
    let sr = inv[0] * xw + inv[1] * 1.0 + inv[2] * zw;
    let sg = inv[3] * xw + inv[4] * 1.0 + inv[5] * zw;
    let sb = inv[6] * xw + inv[7] * 1.0 + inv[8] * zw;
    [
        sr * xr, sg * xg, sb * xb,
        sr * yr, sg * yg, sb * yb,
        sr * zr, sg * zg, sb * zb,
    ]
}

fn invert_3x3(m: &[f32; 9]) -> [f32; 9] {
    let a = m[0]; let b = m[1]; let c = m[2];
    let d = m[3]; let e = m[4]; let f = m[5];
    let g = m[6]; let h = m[7]; let i = m[8];
    let det = a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g);
    if det.abs() < 1e-12 {
        return IDENTITY_3X3;
    }
    let inv_det = 1.0 / det;
    [
        (e * i - f * h) * inv_det,
        -(b * i - c * h) * inv_det,
        (b * f - c * e) * inv_det,
        -(d * i - f * g) * inv_det,
        (a * i - c * g) * inv_det,
        -(a * f - c * d) * inv_det,
        (d * h - e * g) * inv_det,
        -(a * h - b * g) * inv_det,
        (a * e - b * d) * inv_det,
    ]
}

fn mat3_mul(a: &[f32; 9], b: &[f32; 9]) -> [f32; 9] {
    let mut out = [0.0f32; 9];
    for r in 0..3 {
        for c in 0..3 {
            let mut s = 0.0;
            for k in 0..3 {
                s += a[r * 3 + k] * b[k * 3 + c];
            }
            out[r * 3 + c] = s;
        }
    }
    out
}

/// RGB→RGB matrix taking linear surface RGB to linear output RGB. Assumes both
/// spaces share the same white point; if they don't, a Bradford CAT would be
/// applied between the two halves. For the v1 protocol both sRGB and BT.2020
/// use D65, so the no-CAT path covers our two named primaries. If a client
/// supplies explicit primaries with a different white point, the result is a
/// pure rotation in XYZ — sufficient correctness for the V1 slice; a future
/// pass can fold in a Bradford CAT if real clients hit it.
fn rgb_to_rgb_matrix(
    surface: &ColorSpacePrimaries,
    output: &ColorSpacePrimaries,
) -> [f32; 9] {
    let m_in = rgb_to_xyz_matrix(surface);
    let m_out = rgb_to_xyz_matrix(output);
    let m_out_inv = invert_3x3(&m_out);
    mat3_mul(&m_out_inv, &m_in)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx_eq(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() < eps
    }
    fn approx_mat(a: &[f32; 9], b: &[f32; 9], eps: f32) -> bool {
        a.iter().zip(b.iter()).all(|(x, y)| approx_eq(*x, *y, eps))
    }

    #[test]
    fn identity_transform_for_matching_descriptions() {
        let p = ParametricParams {
            primaries_named: Some(1 /* sRGB */),
            tf_named: Some(2 /* Gamma22 */),
            ..Default::default()
        };
        assert!(ColorTransform::build(&p, &p).is_none());
    }

    #[test]
    fn primaries_difference_alone_produces_matrix() {
        let surface = ParametricParams {
            primaries_named: Some(6 /* Bt2020 */),
            tf_named: Some(2 /* Gamma22 */),
            ..Default::default()
        };
        let output = ParametricParams {
            primaries_named: Some(1 /* sRGB */),
            tf_named: Some(2 /* Gamma22 */),
            ..Default::default()
        };
        let t = ColorTransform::build(&surface, &output).expect("non-identity");
        // BT.2020 → sRGB primary matrix has a recognizable sign pattern:
        // [+,+,+; -,+,-; +,-,+] roughly (negative off-diagonals because the
        // wide-gamut "blue" partially aliases out of sRGB).
        assert!(t.matrix_row_major[0] > 1.0); // R channel expansion
        // Off-diagonals can flip sign in either direction; we mainly verify
        // it's not identity and the round-trip works.
        let roundtrip = rgb_to_rgb_matrix(
            &ColorSpacePrimaries::SRGB_D65,
            &ColorSpacePrimaries::BT2020_D65,
        );
        let composed = mat3_mul(&roundtrip, &t.matrix_row_major);
        assert!(approx_mat(&composed, &IDENTITY_3X3, 1e-3),
            "BT.2020→sRGB→BT.2020 should round-trip to identity, got {composed:?}");
    }

    #[test]
    fn pq_inverse_known_points() {
        // PQ encoded=0.0 → 0 cd/m²; encoded=1.0 → 10000 cd/m² (normalized to 1).
        assert!(approx_eq(pq_inverse(0.0), 0.0, 1e-6));
        assert!(approx_eq(pq_inverse(1.0), 1.0, 1e-3));
        // Known reference: 100 cd/m² ⇒ encoded ≈ 0.5081 (SMPTE ST 2084 spec).
        // Verify inverse: encoded=0.5081 → linear ≈ 0.01 (100/10000).
        assert!(approx_eq(pq_inverse(0.5081), 0.01, 5e-4));
    }

    #[test]
    fn hlg_inverse_known_points() {
        // HLG encoded=0 → linear=0; encoded=1 → linear=1.
        assert!(approx_eq(hlg_inverse(0.0), 0.0, 1e-6));
        assert!(approx_eq(hlg_inverse(1.0), 1.0, 1e-3));
        // Lower-half quadratic region: encoded=0.5 → linear = 0.25/3 ≈ 0.08333.
        assert!(approx_eq(hlg_inverse(0.5), 0.083333, 1e-4));
    }

    #[test]
    fn srgb_to_xyz_d65_row_sums_match_white() {
        // For an RGB-to-XYZ matrix with D65 normalization, multiplying by
        // [1,1,1] (encoded white) must give the white point XYZ where Y=1.
        let m = rgb_to_xyz_matrix(&ColorSpacePrimaries::SRGB_D65);
        let xw = m[0] + m[1] + m[2];
        let yw = m[3] + m[4] + m[5];
        let zw = m[6] + m[7] + m[8];
        // D65: x=0.3127, y=0.3290 ⇒ X = x/y ≈ 0.9504, Y = 1, Z = (1-x-y)/y ≈ 1.0888
        assert!(approx_eq(xw, 0.9504, 5e-4));
        assert!(approx_eq(yw, 1.0, 5e-4));
        assert!(approx_eq(zw, 1.0888, 5e-4));
    }

    #[test]
    fn power_curve_inverse_round_trips() {
        let tf = TransferKind::Power { gamma_x10000: 22_000 };
        // Encoding (forward) is x^(1/2.2); inverse is x^2.2. Composition is identity.
        let encoded = 0.5_f32.powf(1.0 / 2.2);
        let linear = tf.inverse(encoded);
        assert!(approx_eq(linear, 0.5, 1e-4));
    }

    #[test]
    fn transferkind_from_params_resolves_named() {
        let p = ParametricParams { tf_named: Some(11 /* PQ */), ..Default::default() };
        assert_eq!(TransferKind::from_params(&p), TransferKind::St2084Pq);
        let p = ParametricParams { tf_named: Some(13 /* HLG */), ..Default::default() };
        assert_eq!(TransferKind::from_params(&p), TransferKind::Hlg);
        let p = ParametricParams { tf_power: Some(18_000), ..Default::default() };
        assert_eq!(TransferKind::from_params(&p), TransferKind::Power { gamma_x10000: 18_000 });
    }

    #[test]
    fn eotf_difference_alone_produces_transform_with_identity_matrix() {
        let surface = ParametricParams {
            primaries_named: Some(1 /* sRGB */),
            tf_named: Some(11 /* PQ */),
            ..Default::default()
        };
        let output = ParametricParams {
            primaries_named: Some(1 /* sRGB */),
            tf_named: Some(2 /* Gamma22 */),
            ..Default::default()
        };
        let t = ColorTransform::build(&surface, &output).expect("non-identity");
        assert_eq!(t.inverse_eotf, TransferKind::St2084Pq);
        assert_eq!(t.forward_eotf, TransferKind::Gamma22);
        // Primaries match → matrix is identity.
        assert!(approx_mat(&t.matrix_row_major, &IDENTITY_3X3, 1e-6));
    }

    #[test]
    fn shader_id_is_stable_and_distinct() {
        // The shader's if-chain in decode_eotf/encode_eotf depends on these
        // exact integer values. Renumbering breaks the GL contract.
        assert_eq!(TransferKind::Linear.shader_id(), 0);
        assert_eq!(TransferKind::Power { gamma_x10000: 22_000 }.shader_id(), 1);
        assert_eq!(TransferKind::Bt1886.shader_id(), 2);
        assert_eq!(TransferKind::Gamma22.shader_id(), 3);
        assert_eq!(TransferKind::St2084Pq.shader_id(), 4);
        assert_eq!(TransferKind::Hlg.shader_id(), 5);
    }

    #[test]
    fn gamma_for_shader_defined_for_every_variant() {
        // Power's gamma comes from the variant. Other variants return 1.0 so
        // the matching shader uniform is always defined, even on a TF branch
        // that never consults the value — undefined-uniform reads are
        // implementation-defined and we don't want stale data leaking in.
        assert_eq!(
            TransferKind::Power { gamma_x10000: 24_000 }.gamma_for_shader(),
            2.4
        );
        assert_eq!(TransferKind::Linear.gamma_for_shader(), 1.0);
        assert_eq!(TransferKind::Bt1886.gamma_for_shader(), 1.0);
        assert_eq!(TransferKind::Gamma22.gamma_for_shader(), 1.0);
        assert_eq!(TransferKind::St2084Pq.gamma_for_shader(), 1.0);
        assert_eq!(TransferKind::Hlg.gamma_for_shader(), 1.0);
        // Zero gamma must not become a divide-by-zero or NaN producer.
        let g = TransferKind::Power { gamma_x10000: 0 }.gamma_for_shader();
        assert!(g.is_finite() && g > 0.0);
    }
}
