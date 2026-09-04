//! Color-space conversion math for the wp-color-management render path.
//!
//! The scene-linear path is explicitly two-stage:
//!   1. Decode each described surface and map its primaries into the
//!      compositor's normalized linear-sRGB working space.
//!   2. At delivery, map that common scene into each physical output's
//!      primaries and apply its transfer function in a shader or paired CRTC
//!      CTM/GAMMA_LUT.
//!
//! The legacy encoded path can still build a direct surface→output plan. Both
//! stages use `M_out_from_in = M_xyz_to_rgb(out) · CAT(out ← in) ·
//! M_rgb_to_xyz(in)`. The Bradford chromatic-adaptation transform keeps neutral
//! colors neutral when a client supplies custom primaries whose white point is
//! not the compositor's D65 working white.
//!
//! The working space has one absolute anchor: linear 1.0 is
//! [`SDR_REFERENCE_WHITE_NITS`] (203 cd/m², the BT.2408 HDR reference white).
//! [`working_space_scale`] re-anchors decoded PQ/HLG content onto that scale,
//! and [`ToneMapPolicy`] defines how content whose dynamic range exceeds an
//! output's is remapped at delivery. Both are wired into the render decision
//! points: ingress (`ColorTransform::build_to_linear_srgb` folds the scale
//! factor into the gamut matrix, bitwise-identical for the SDR family whose
//! scale is 1.0) and the per-output delivery plan ([`OutputToneMapPlan`],
//! applied by the scene-linear encode shader before the output OETF and baked
//! into the hardware GAMMA_LUT curve). HDR signalling stays fail-closed.
//!
//! It intentionally owns math and render plans only: GL state and uniform
//! bindings stay in the compositor adapters. Keeping the calculations here
//! gives CPU coverage for gamut/transfer math while strict headless GLES tests
//! verify the uploaded plans and shader pixels without HDR hardware.

use crate::backend::wayland_udev::color_management::{ParametricParams, SDR_REFERENCE_WHITE_NITS};

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
        w: Chromaticity {
            x: 0.3127,
            y: 0.3290,
        },
    };
    pub const BT2020_D65: Self = Self {
        r: Chromaticity { x: 0.708, y: 0.292 },
        g: Chromaticity { x: 0.170, y: 0.797 },
        b: Chromaticity { x: 0.131, y: 0.046 },
        w: Chromaticity {
            x: 0.3127,
            y: 0.3290,
        },
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
                r: Chromaticity {
                    x: f(prim[0]),
                    y: f(prim[1]),
                },
                g: Chromaticity {
                    x: f(prim[2]),
                    y: f(prim[3]),
                },
                b: Chromaticity {
                    x: f(prim[4]),
                    y: f(prim[5]),
                },
                w: Chromaticity {
                    x: f(prim[6]),
                    y: f(prim[7]),
                },
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
    /// IEC 61966-2-1 sRGB — piecewise (linear segment near black, ≈2.4 power
    /// above). Distinct from `Bt1886` and `Gamma22`: the SOTA #3 GAMMA_LUT
    /// offload path needs the exact piecewise OETF, not a power approximation.
    Srgb,
}

impl TransferKind {
    /// Map a wp-color-management ParametricParams to a single TransferKind.
    /// Prefers named TF; falls back to tf_power; defaults to exact sRGB when
    /// neither is present (matches our srgb_params() fallback).
    pub fn from_params(p: &ParametricParams) -> Self {
        if let Some(tf) = p.tf_named {
            return match tf {
                // wp_color_manager_v1::TransferFunction values
                1 => Self::Bt1886,
                2 => Self::Gamma22,
                5 => Self::Linear,
                9 => Self::Srgb,
                11 => Self::St2084Pq,
                13 => Self::Hlg,
                _ => Self::Srgb,
            };
        }
        if let Some(g) = p.tf_power {
            return Self::Power { gamma_x10000: g };
        }
        Self::Srgb
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
            Self::Srgb => 6,
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
            Self::Srgb => srgb_inverse(x),
        }
    }

    /// Apply this curve's forward OETF to a scene-linear value in 0..1.
    /// Returns the encoded value in 0..1, suitable for quantizing into a
    /// hardware GAMMA_LUT entry.
    pub fn forward(self, x: f32) -> f32 {
        let x = x.clamp(0.0, 1.0);
        match self {
            Self::Linear => x,
            Self::Srgb => srgb_forward(x),
            Self::Power { gamma_x10000 } => {
                let g = (gamma_x10000 as f32 / 10_000.0).max(1e-3);
                x.powf(1.0 / g)
            }
            Self::Bt1886 => x.powf(1.0 / 2.4),
            Self::Gamma22 => x.powf(1.0 / 2.2),
            Self::St2084Pq => pq_forward(x),
            Self::Hlg => hlg_forward(x),
        }
    }
}

/// IEC 61966-2-1 sRGB OETF (linear → encoded), piecewise.
fn srgb_forward(l: f32) -> f32 {
    if l <= 0.003_130_8 {
        12.92 * l
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    }
}

/// IEC 61966-2-1 sRGB EOTF (encoded → linear), piecewise.
fn srgb_inverse(e: f32) -> f32 {
    if e <= 0.040_45 {
        e / 12.92
    } else {
        ((e + 0.055) / 1.055).powf(2.4)
    }
}

#[cfg(feature = "backend-wayland-udev")]
pub use drm_ffi::drm_color_ctm as DrmColorCtm;
#[cfg(feature = "backend-wayland-udev")]
pub use drm_ffi::drm_color_lut as DrmColorLut;

/// Identity 3×3 color matrix, row-major. Public mirror of the private `IDENTITY_3X3`
/// used by `ColorTransform`; exposed so callers (e.g. the KMS CTM install path)
/// can request an explicit no-op transform.
pub const IDENTITY_CTM: [f32; 9] = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];

/// Pack a row-major 3×3 f32 matrix into the kernel's `drm_color_ctm` layout
/// (9 × s31.32 fixed-point as `u64`, sign in bit 63, magnitude in bits 62..0).
/// Matrix values are clamped to the representable magnitude range before packing.
#[cfg(feature = "backend-wayland-udev")]
pub fn build_ctm(matrix: [f32; 9]) -> DrmColorCtm {
    let mut out = DrmColorCtm { matrix: [0; 9] };
    for (i, &v) in matrix.iter().enumerate() {
        let magnitude = (v.abs() * (1u64 << 32) as f32).round();
        let mag_bits = (magnitude as u64).min(0x7FFF_FFFF_FFFF_FFFF);
        let sign_bit = if v.is_sign_negative() && magnitude > 0.0 {
            1u64 << 63
        } else {
            0
        };
        out.matrix[i] = sign_bit | mag_bits;
    }
    out
}

/// Build a hardware GAMMA_LUT for a given transfer function. The output is a
/// gray ramp (R == G == B at every entry) of `size` entries. Each entry encodes
/// `tf.forward(i / (size - 1))` scaled into the 16-bit unsigned fixed-point
/// range expected by the kernel. Caller guarantees `size >= 2`.
#[cfg(feature = "backend-wayland-udev")]
pub fn build_gamma_lut(tf: TransferKind, size: usize) -> Vec<DrmColorLut> {
    build_gamma_lut_from(&mut |linear| tf.forward(linear), size)
}

/// Bake a full delivery plan into a hardware GAMMA_LUT: each entry tone-maps
/// the framebuffer's working-linear value per the plan's policy, re-anchors
/// onto `tf`'s native scale, and applies its OETF. Tone mapping is
/// per-channel nonlinear, so it cannot fold into the paired CTM and must live
/// in this curve.
#[cfg(feature = "backend-wayland-udev")]
pub fn build_gamma_lut_delivery(
    tf: TransferKind,
    plan: OutputToneMapPlan,
    size: usize,
) -> Vec<DrmColorLut> {
    build_gamma_lut_from(
        &mut |linear| tf.forward(plan.map_to_output_scale(linear)),
        size,
    )
}

/// The canonical scanout LUT for an output transfer: the rescaled OETF that
/// anchors working-linear 1.0 at 203 cd/m² instead of the transfer's own
/// maximum. No per-frame source peak is needed: over the framebuffer-normalized
/// LUT domain [0, 1], every policy [`ToneMapPolicy::for_peaks`] can select
/// coincides with this curve — `ReferenceWhite` is a pass-through, and `Clip`
/// at a target peak ≥ 1.0 never engages inside the domain — while values
/// beyond the domain are clipped by the hardware's own index clamp, which is
/// exactly the `Clip` policy. The installed LUT can therefore stay keyed by
/// `TransferKind` alone. Wiring a peak-dependent `ReinhardShoulder` selection
/// to hardware must extend that key first.
#[cfg(feature = "backend-wayland-udev")]
pub fn build_gamma_lut_scanout(tf: TransferKind, size: usize) -> Vec<DrmColorLut> {
    build_gamma_lut_delivery(tf, OutputToneMapPlan::for_output(1.0, tf), size)
}

#[cfg(feature = "backend-wayland-udev")]
fn build_gamma_lut_from(curve: &mut dyn FnMut(f32) -> f32, size: usize) -> Vec<DrmColorLut> {
    let denom = (size - 1) as f32;
    (0..size)
        .map(|i| {
            let linear = i as f32 / denom;
            let encoded = curve(linear).clamp(0.0, 1.0);
            let q = (encoded * 65535.0 + 0.5) as u32;
            let v = q.min(65535) as u16;
            DrmColorLut {
                red: v,
                green: v,
                blue: v,
                reserved: 0,
            }
        })
        .collect()
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
    if den.abs() < 1e-12 {
        0.0
    } else {
        (num / den).powf(1.0 / M1)
    }
}

/// HLG inverse: encoded 0..1 → linear 0..1 (system-relative).
fn hlg_inverse(e: f32) -> f32 {
    if e <= 0.5 {
        (e * e) / 3.0
    } else {
        (((e - HLG_C) / HLG_A).exp() + HLG_B) / 12.0
    }
}

/// PQ (SMPTE ST 2084) forward (OETF): linear 0..1 → encoded 0..1. Inverse of `pq_inverse`.
fn pq_forward(l: f32) -> f32 {
    const M1: f32 = 0.1593017578125;
    const M2: f32 = 78.84375;
    const C1: f32 = 0.8359375;
    const C2: f32 = 18.8515625;
    const C3: f32 = 18.6875;
    let lm = l.max(0.0).powf(M1);
    ((C1 + C2 * lm) / (1.0 + C3 * lm)).powf(M2)
}

const HLG_A: f32 = 0.17883277;
const HLG_B: f32 = 1.0 - 4.0 * HLG_A; // 0.28466892
const HLG_C: f32 = 0.559_910_7; // 0.5 - A * ln(4A)

/// HLG (BT.2100) forward (OETF): linear 0..1 → encoded 0..1. Inverse of `hlg_inverse`.
fn hlg_forward(l: f32) -> f32 {
    if l <= 1.0 / 12.0 {
        (3.0 * l.max(0.0)).sqrt()
    } else {
        HLG_A * (12.0 * l - HLG_B).max(1e-12).ln() + HLG_C
    }
}

// --- Absolute luminance scales and tone mapping ---
//
// The working space's absolute anchor and the delivery-stage remapping. The
// ingress rescale lives in `ColorTransform::build_to_linear_srgb`; the
// delivery side is `OutputToneMapPlan` carried by every `OutputColorRegion`.
// HDR enable stays fail-closed until the KMS atomic commit chain validates a
// full 10-bit scanout path, so every reachable output still targets an SDR
// transfer today.

/// SMPTE ST 2084 absolute range: PQ-encoded 1.0 is defined as 10 000 cd/m².
pub const PQ_MAX_LUMINANCE_NITS: f32 = 10_000.0;

/// BT.2100 HLG nominal peak luminance of the reference system, in cd/m². HLG
/// is display-relative — a real display's peak follows its own OOTF — but
/// 1 000 cd/m² is the fixed reference-system anchor used whenever an absolute
/// scale is needed (e.g. placing HLG content into the working space).
pub const HLG_NOMINAL_PEAK_NITS: f32 = 1_000.0;

/// Convert an absolute luminance (cd/m²) into working-space linear units
/// (1.0 = [`SDR_REFERENCE_WHITE_NITS`]).
pub fn nits_to_working_linear(nits: f32) -> f32 {
    nits / SDR_REFERENCE_WHITE_NITS
}

/// Convert a working-space linear value into absolute luminance (cd/m²).
/// Inverse of [`nits_to_working_linear`].
pub fn working_linear_to_nits(linear: f32) -> f32 {
    linear * SDR_REFERENCE_WHITE_NITS
}

/// Absolute PQ decode: encoded 0..1 → cd/m².
pub fn pq_decode_nits(encoded: f32) -> f32 {
    pq_inverse(encoded.clamp(0.0, 1.0)) * PQ_MAX_LUMINANCE_NITS
}

/// Absolute PQ encode: cd/m² → encoded 0..1. Luminance outside the ST 2084
/// range clamps to the range ends. Inverse of [`pq_decode_nits`].
pub fn pq_encode_nits(nits: f32) -> f32 {
    pq_forward((nits / PQ_MAX_LUMINANCE_NITS).clamp(0.0, 1.0))
}

/// HLG decode on the BT.2100 reference system: encoded 0..1 → cd/m² at the
/// nominal peak. Real displays rescale the result via their own OOTF.
pub fn hlg_decode_nits(encoded: f32) -> f32 {
    hlg_inverse(encoded.clamp(0.0, 1.0)) * HLG_NOMINAL_PEAK_NITS
}

/// HLG encode on the BT.2100 reference system: cd/m² → encoded 0..1, clamped
/// to the nominal range. Inverse of [`hlg_decode_nits`].
pub fn hlg_encode_nits(nits: f32) -> f32 {
    hlg_forward((nits / HLG_NOMINAL_PEAK_NITS).clamp(0.0, 1.0))
}

/// Scale factor re-anchoring a decoded source value into working-space linear.
///
/// Every [`TransferKind::inverse`] yields a normalized range whose 1.0 is the
/// source's own reference: display white for SDR-style curves, 10 000 cd/m²
/// for PQ, the BT.2100 nominal peak for HLG. Multiplying the decoded value by
/// this factor expresses it in working-space units whose 1.0 is
/// [`SDR_REFERENCE_WHITE_NITS`]. SDR curves map 1:1, which is why decoded SDR
/// content already composites correctly without an explicit rescale.
pub fn working_space_scale(tf: TransferKind) -> f32 {
    match tf {
        TransferKind::St2084Pq => PQ_MAX_LUMINANCE_NITS / SDR_REFERENCE_WHITE_NITS,
        TransferKind::Hlg => HLG_NOMINAL_PEAK_NITS / SDR_REFERENCE_WHITE_NITS,
        _ => 1.0,
    }
}

/// Tone-mapping policy between content and output dynamic ranges, expressed in
/// working-space linear units: 1.0 is the SDR reference white on both sides
/// and HDR headroom is values above 1.0. Mapping is defined per component
/// (R/G/B independently), keeping primaries and hue untouched.
///
/// The selection decision belongs to the per-output delivery plan: source
/// peaks are the per-frame aggregation of the committed surface image
/// descriptions visible on that output, the output's peak follows from its
/// transfer function via [`working_space_scale`], and
/// [`ToneMapPolicy::for_peaks`] picks the default. `OutputToneMapPlan`
/// carries the selected pair to the encode shader and the LUT bake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToneMapPolicy {
    /// Reference-white mapping for content that fits the target range (the
    /// SDR→HDR case). Working-space values pass through unchanged: SDR white
    /// (1.0 = 203 cd/m²) already sits at the BT.2408 HDR reference white, so
    /// SDR content keeps its look while the output's headroom above 1.0 stays
    /// available to native HDR content. A no-op remap by design.
    ReferenceWhite,
    /// Hard clip at the target peak. Preserves the SDR range exactly and is
    /// fully predictable; the default for HDR content delivered to SDR
    /// outputs.
    Clip,
    /// Extended-Reinhard shoulder compressing `[0, source_peak]` onto
    /// `[0, target_peak]`. Trades a slight compression of the reference range
    /// for preserved highlight gradation; the alternative for HDR→SDR when
    /// clipping is judged too harsh.
    ReinhardShoulder,
}

impl ToneMapPolicy {
    /// Default policy for content with the given source peak delivered to an
    /// output with the given target peak, both in working-space units.
    /// Content that fits the target needs no compression.
    pub fn for_peaks(source_peak_working: f32, target_peak_working: f32) -> Self {
        if source_peak_working <= target_peak_working {
            Self::ReferenceWhite
        } else {
            Self::Clip
        }
    }

    /// Shader-side discriminant for the scene-linear encode pass's
    /// `u_tone_map` uniform. Part of the Rust↔GLSL contract and MUST be kept
    /// in lockstep with the tone-map branch in `SCENE_LINEAR_ENCODE_FRAGMENT`.
    /// `ReferenceWhite` is 0 so the GL zero-initialized default (a caller that
    /// never binds the uniform) is the pass-through — the exact pre-tone-map
    /// shader behavior.
    pub fn shader_id(self) -> i32 {
        match self {
            Self::ReferenceWhite => 0,
            Self::Clip => 1,
            Self::ReinhardShoulder => 2,
        }
    }

    /// Apply the policy to one working-space linear component. Peaks are the
    /// source content's and target output's peaks in working units. Non-finite
    /// input has undefined colorimetry per the wp-color-management spec and
    /// propagates as-is.
    pub fn map_working_linear(self, x: f32, source_peak: f32, target_peak: f32) -> f32 {
        match self {
            Self::ReferenceWhite => x,
            Self::Clip => x.clamp(0.0, target_peak.max(0.0)),
            Self::ReinhardShoulder => reinhard_shoulder(x, source_peak, target_peak),
        }
    }
}

/// Extended Reinhard with a white point: keeps 0 at 0 and maps `source_peak`
/// exactly onto `target_peak`. Values above the source peak clip, and
/// below-black input maps to 0. Content that already fits the target, and
/// degenerate (non-positive or non-finite) peaks, fall back to a plain clip.
fn reinhard_shoulder(x: f32, source_peak: f32, target_peak: f32) -> f32 {
    if !(source_peak > 0.0) || !(target_peak > 0.0) || source_peak <= target_peak {
        return x.clamp(0.0, target_peak.max(0.0));
    }
    // y = x(1 + x/w²)/(1+x) maps w → 1 with y'(x) = (1 + x/w²)²/(1+x)² > 0;
    // the curve is only defined for x ≥ 0, so below-black input clamps first.
    let x = x.max(0.0);
    let w = source_peak;
    let y = x * (1.0 + x / (w * w)) / (1.0 + x);
    (y * target_peak).clamp(0.0, target_peak)
}

/// Per-output delivery tone-map plan: the selected policy plus both peaks in
/// working-space linear units (1.0 = [`SDR_REFERENCE_WHITE_NITS`] on both
/// sides). Every [`OutputColorRegion`] carries one to the scene-linear encode
/// pass, and the hardware delivery LUT bake derives from the same fields, so
/// shader uniforms, LUT curves, and CPU test oracles all share this single
/// selection point.
///
/// The target peak doubles as the delivery rescale divisor: after the policy
/// remap, working values are divided by it to re-anchor onto the output
/// transfer's native normalized scale (where 1.0 is display white for SDR
/// curves, 10 000 cd/m² for PQ, the nominal peak for HLG) before the OETF.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputToneMapPlan {
    pub policy: ToneMapPolicy,
    /// Aggregated source peak of the surfaces visible on the output.
    pub source_peak_working: f32,
    /// The output's own peak, `working_space_scale(output_tf)`.
    pub target_peak_working: f32,
}

impl OutputToneMapPlan {
    /// Pass-through with unit rescale: the exact pre-tone-map delivery
    /// behavior. Used where no peak aggregation applies — the whole-frame
    /// sRGB fallback encode, the canonical sRGB capture view, and the shader
    /// no-op substituted when the hardware CRTC pair owns delivery.
    pub const IDENTITY: Self = Self {
        policy: ToneMapPolicy::ReferenceWhite,
        source_peak_working: 1.0,
        target_peak_working: 1.0,
    };

    /// Select the delivery plan for one output from the aggregated source
    /// peak of its visible surfaces and the output's transfer function.
    pub fn for_output(source_peak_working: f32, output_tf: TransferKind) -> Self {
        let target_peak_working = working_space_scale(output_tf);
        Self {
            policy: ToneMapPolicy::for_peaks(source_peak_working, target_peak_working),
            source_peak_working,
            target_peak_working,
        }
    }

    /// Map one working-space linear component per this plan and re-anchor it
    /// onto the output transfer's native scale. The result feeds
    /// `TransferKind::forward`. A non-positive target (only constructible by
    /// hand; `for_output` targets are always ≥ 1.0) falls back to the unit
    /// divisor, mirroring the shader's unset-uniform legacy behavior. The
    /// `IDENTITY` plan is a bitwise no-op: `ReferenceWhite` returns `x` and
    /// `x / 1.0 == x` exactly in IEEE-754, which keeps every SDR delivery
    /// route pixel-identical to the pre-tone-map pipeline.
    pub fn map_to_output_scale(&self, x: f32) -> f32 {
        let target = if self.target_peak_working > 0.0 {
            self.target_peak_working
        } else {
            1.0
        };
        self.policy
            .map_working_linear(x, self.source_peak_working, target)
            / target
    }
}

/// Whether a source carrying this description can use the legacy sRGB ingress
/// unchanged: sRGB transfer plus sRGB/D65 primaries. Undescribed content is
/// sRGB by convention; anything else (PQ/HLG, wide gamut, custom primaries)
/// needs a described transform, which paths without per-element color plans —
/// currently the KMS external-element adapter — must refuse by staying on the
/// encoded fallback instead of guessing.
pub fn description_is_srgb_default(p: &ParametricParams) -> bool {
    TransferKind::from_params(p) == TransferKind::Srgb
        && ColorSpacePrimaries::from_params(p) == ColorSpacePrimaries::SRGB_D65
}

/// A surface color transform: inverse source EOTF, linear-light 3×3 gamut map,
/// and optional target EOTF. The target may be the common linear-sRGB working
/// space (`forward_eotf = Linear`) or a legacy encoded output. Stored
/// row-major; intended to be uploaded as a `mat3` to GLSL.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorTransform {
    pub inverse_eotf: TransferKind,
    pub matrix_row_major: [f32; 9],
    pub forward_eotf: TransferKind,
}

/// One physical, top-left-origin region of the compositor's global framebuffer
/// and the final color conversion required by the output that consumes it.
///
/// The scene entering this pass is always common linear sRGB. `rect` is
/// `[x, y, width, height]` in physical framebuffer pixels; widths and heights
/// are stored as `i32` so validation can reject malformed/non-positive KMS
/// geometry before any GLES unsigned conversion occurs. `tone_map` is the
/// delivery tone-map plan applied between the gamut matrix and the output
/// OETF (per-channel nonlinear, so it cannot fold into the matrix).
#[derive(Clone, Debug, PartialEq)]
pub struct OutputColorRegion {
    pub rect: [i32; 4],
    pub output_tf: TransferKind,
    pub working_to_output_row_major: [f32; 9],
    pub tone_map: OutputToneMapPlan,
}

/// Convert a row-major 3×3 matrix into the column-major memory order expected
/// by `glUniformMatrix3fv(..., transpose = GL_FALSE, ...)`.
///
/// Both per-surface transforms and the final per-output encode pass use this
/// contract. Keeping the conversion in the color-math module prevents the two
/// shader adapters from developing subtly different upload orders.
pub fn matrix_to_column_major(matrix_row_major: [f32; 9]) -> [f32; 9] {
    let m = matrix_row_major;
    [m[0], m[3], m[6], m[1], m[4], m[7], m[2], m[5], m[8]]
}

impl ColorTransform {
    /// Return the gamut matrix in column-major memory order for
    /// `glUniformMatrix3fv(..., transpose = GL_FALSE, ...)`.
    ///
    /// Keeping CPU color math row-major while normalizing every runtime upload
    /// to column-major/`GL_FALSE` gives one testable layout and matches the
    /// column-major values returned by uniform capture/restore.
    pub fn matrix_column_major(self) -> [f32; 9] {
        matrix_to_column_major(self.matrix_row_major)
    }

    /// Build a source-owned transform into the compositor's canonical
    /// linear-sRGB working space.
    ///
    /// Unlike [`Self::build`], this never elides an identity: an explicitly
    /// described PQ/HLG surface still needs its inverse transfer function even
    /// when its primaries are already sRGB. `forward_eotf` is deliberately
    /// `Linear`, documenting that the result is not output-encoded; the final
    /// per-output pass owns both the sRGB→output gamut map and output OETF.
    ///
    /// The absolute-luminance re-anchoring (`working_space_scale`) folds into
    /// the gamut matrix: the scalar is uniform across channels, so the matrix
    /// product absorbs it and the shader needs no second multiply. Decoded PQ
    /// content thus lands in the working space at 10 000/203 per unit and HLG
    /// at 1 000/203 per unit. The SDR family's scale is exactly 1.0, and
    /// `x * 1.0 == x` bitwise in IEEE-754, so every SDR ingress route keeps
    /// the pre-rescale matrix bit for bit.
    pub fn build_to_linear_srgb(surface: &ParametricParams) -> Self {
        let surface_prim = ColorSpacePrimaries::from_params(surface);
        let tf = TransferKind::from_params(surface);
        let matrix = if primaries_match(&surface_prim, &ColorSpacePrimaries::SRGB_D65) {
            IDENTITY_3X3
        } else {
            rgb_to_rgb_matrix(&surface_prim, &ColorSpacePrimaries::SRGB_D65)
        };
        let scale = working_space_scale(tf);
        Self {
            inverse_eotf: tf,
            matrix_row_major: matrix.map(|component| component * scale),
            forward_eotf: TransferKind::Linear,
        }
    }

    /// Build an explicit surface-description plan even when source and target
    /// descriptions match.
    ///
    /// `build` intentionally collapses a mathematical identity to `None` for
    /// older callers. A renderer writing a scene-linear intermediate cannot do
    /// that: an explicitly described PQ/HLG surface still needs its inverse
    /// EOTF, whereas `None` means an undescribed legacy-sRGB surface.
    pub fn build_explicit(surface: &ParametricParams, output: &ParametricParams) -> Self {
        let surface_prim = ColorSpacePrimaries::from_params(surface);
        let output_prim = ColorSpacePrimaries::from_params(output);
        let matrix = if primaries_match(&surface_prim, &output_prim) {
            IDENTITY_3X3
        } else {
            rgb_to_rgb_matrix(&surface_prim, &output_prim)
        };
        Self {
            inverse_eotf: TransferKind::from_params(surface),
            matrix_row_major: matrix,
            forward_eotf: TransferKind::from_params(output),
        }
    }

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

        Some(Self::build_explicit(surface, output))
    }

    /// Build the plan used by a compositor render path.
    ///
    /// Encoded-space compositing keeps the historical identity elision, which
    /// preserves direct-scanout and effect fast paths. Scene-linear compositing
    /// maps every described source into the canonical linear-sRGB working
    /// space so geometry/output assignment cannot change its meaning.
    pub fn build_for_render_path(
        surface: &ParametricParams,
        output: &ParametricParams,
        scene_linear: bool,
    ) -> Option<Self> {
        if scene_linear {
            Some(Self::build_to_linear_srgb(surface))
        } else {
            Self::build(surface, output)
        }
    }
}

const IDENTITY_3X3: [f32; 9] = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];

// Color matrices generated from real display primaries are normally close to
// unity (sRGB/BT.2020 conversions stay below 2). This deliberately generous
// ceiling still rejects malformed chromaticities that amplify a channel by
// orders of magnitude before such a matrix reaches a shader or KMS CTM.
const MAX_REASONABLE_MATRIX_COMPONENT: f32 = 64.0;
const MIN_RELATIVE_MATRIX_DETERMINANT: f32 = 1.0e-6;

fn primaries_match(a: &ColorSpacePrimaries, b: &ColorSpacePrimaries) -> bool {
    const TOL: f32 = 0.001;
    let close =
        |p: Chromaticity, q: Chromaticity| (p.x - q.x).abs() < TOL && (p.y - q.y).abs() < TOL;
    close(a.r, b.r) && close(a.g, b.g) && close(a.b, b.b) && close(a.w, b.w)
}

/// RGB primaries may lie on the spectral-locus boundary. In particular,
/// BT.2020 red has `x + y == 1` (Z == 0), so that boundary must not be rejected.
fn valid_rgb_primary(primary: Chromaticity) -> bool {
    primary.x.is_finite()
        && primary.y.is_finite()
        && primary.x >= 0.0
        && primary.y > 0.0
        && primary.x + primary.y <= 1.0
}

/// A usable reference white needs positive X, Y and Z tristimulus components.
/// Unlike an individual RGB primary, a white point on `x + y == 1` has Z == 0
/// and is not a physically useful adaptation target.
fn valid_white_point(white: Chromaticity) -> bool {
    white.x.is_finite()
        && white.y.is_finite()
        && white.x > 0.0
        && white.y > 0.0
        && white.x + white.y < 1.0
}

fn matrix_determinant(matrix: &[f32; 9]) -> f32 {
    let [a, b, c, d, e, f, g, h, i] = *matrix;
    a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g)
}

/// Reject non-finite, near-singular, or implausibly large color matrices.
/// Comparing the determinant to the cube of the largest entry makes the
/// singularity test scale-relative instead of depending on one absolute
/// epsilon.
fn matrix_is_reasonable(matrix: &[f32; 9]) -> bool {
    if matrix.iter().any(|component| {
        !component.is_finite() || component.abs() > MAX_REASONABLE_MATRIX_COMPONENT
    }) {
        return false;
    }

    let scale = matrix
        .iter()
        .fold(0.0_f32, |largest, component| largest.max(component.abs()));
    if scale == 0.0 {
        return false;
    }

    let determinant = matrix_determinant(matrix);
    determinant.is_finite() && determinant.abs() > MIN_RELATIVE_MATRIX_DETERMINANT * scale.powi(3)
}

/// Compute the 3x3 RGB→XYZ matrix for the given primaries.
/// Derived from the standard "primary matrix" construction: choose scaling
/// factors S_r, S_g, S_b so that [S_r, S_g, S_b] · 1 = whitepoint XYZ.
fn rgb_to_xyz_matrix(p: &ColorSpacePrimaries) -> Option<[f32; 9]> {
    if !valid_rgb_primary(p.r)
        || !valid_rgb_primary(p.g)
        || !valid_rgb_primary(p.b)
        || !valid_white_point(p.w)
    {
        return None;
    }

    let to_xyz = |c: Chromaticity| -> (f32, f32, f32) {
        // X = x/y, Y = 1, Z = (1-x-y)/y. Use Y=1 by convention.
        (c.x / c.y, 1.0, (1.0 - c.x - c.y) / c.y)
    };
    let (xr, yr, zr) = to_xyz(p.r);
    let (xg, yg, zg) = to_xyz(p.g);
    let (xb, yb, zb) = to_xyz(p.b);
    let (xw, _yw, zw) = to_xyz(p.w);
    // Solve M · [S_r, S_g, S_b]^T = [Xw, Yw=1, Zw]^T where
    //   M = [[xr xg xb], [yr yg yb], [zr zg zb]]
    let primary_matrix = [xr, xg, xb, yr, yg, yb, zr, zg, zb];
    let inv = invert_3x3(&primary_matrix)?;
    // S = M^{-1} · whitepoint_XYZ
    let sr = inv[0] * xw + inv[1] * 1.0 + inv[2] * zw;
    let sg = inv[3] * xw + inv[4] * 1.0 + inv[5] * zw;
    let sb = inv[6] * xw + inv[7] * 1.0 + inv[8] * zw;
    // A white point outside the RGB triangle produces a negative channel
    // scale. Treat that description as unusable instead of emitting a matrix
    // with surprising sign/amplification behavior.
    if [sr, sg, sb]
        .iter()
        .any(|scale| !scale.is_finite() || *scale <= 0.0)
    {
        return None;
    }

    let matrix = [
        sr * xr,
        sg * xg,
        sb * xb,
        sr * yr,
        sg * yg,
        sb * yb,
        sr * zr,
        sg * zg,
        sb * zb,
    ];
    matrix_is_reasonable(&matrix).then_some(matrix)
}

fn invert_3x3(m: &[f32; 9]) -> Option<[f32; 9]> {
    if !matrix_is_reasonable(m) {
        return None;
    }
    let a = m[0];
    let b = m[1];
    let c = m[2];
    let d = m[3];
    let e = m[4];
    let f = m[5];
    let g = m[6];
    let h = m[7];
    let i = m[8];
    let det = matrix_determinant(m);
    let inv_det = 1.0 / det;
    let inverse = [
        (e * i - f * h) * inv_det,
        -(b * i - c * h) * inv_det,
        (b * f - c * e) * inv_det,
        -(d * i - f * g) * inv_det,
        (a * i - c * g) * inv_det,
        -(a * f - c * d) * inv_det,
        (d * h - e * g) * inv_det,
        -(a * h - b * g) * inv_det,
        (a * e - b * d) * inv_det,
    ];
    matrix_is_reasonable(&inverse).then_some(inverse)
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

fn mat3_mul_vec(matrix: &[f32; 9], vector: [f32; 3]) -> [f32; 3] {
    [
        matrix[0] * vector[0] + matrix[1] * vector[1] + matrix[2] * vector[2],
        matrix[3] * vector[0] + matrix[4] * vector[1] + matrix[5] * vector[2],
        matrix[6] * vector[0] + matrix[7] * vector[1] + matrix[8] * vector[2],
    ]
}

fn white_xyz(white: Chromaticity) -> Option<[f32; 3]> {
    if !valid_white_point(white) {
        return None;
    }
    let xyz = [white.x / white.y, 1.0, (1.0 - white.x - white.y) / white.y];
    xyz.iter()
        .all(|component| component.is_finite())
        .then_some(xyz)
}

/// Bradford chromatic adaptation in XYZ space, from `source_white` to
/// `destination_white`.
///
/// Invalid custom white points return `None`; the public RGB conversion then
/// fails closed to identity. Protocol validation is expected to reject them
/// earlier, but color planning must never turn a bad description into NaN
/// uniforms or a non-finite KMS CTM.
fn bradford_adaptation_matrix(
    source_white: Chromaticity,
    destination_white: Chromaticity,
) -> Option<[f32; 9]> {
    const BRADFORD: [f32; 9] = [
        0.8951, 0.2664, -0.1614, -0.7502, 1.7135, 0.0367, 0.0389, -0.0685, 1.0296,
    ];
    const BRADFORD_INVERSE: [f32; 9] = [
        0.986_992_9,
        -0.147_054_3,
        0.159_962_7,
        0.432_305_3,
        0.518_360_3,
        0.049_291_2,
        -0.008_528_7,
        0.040_042_8,
        0.968_486_7,
    ];

    // Validate before the equality fast path: two identical invalid whites are
    // still an invalid description, not a successful identity adaptation.
    if !valid_white_point(source_white) || !valid_white_point(destination_white) {
        return None;
    }
    if (source_white.x - destination_white.x).abs() < 1e-7
        && (source_white.y - destination_white.y).abs() < 1e-7
    {
        return Some(IDENTITY_3X3);
    }
    let (Some(source_xyz), Some(destination_xyz)) =
        (white_xyz(source_white), white_xyz(destination_white))
    else {
        return None;
    };
    let source_cone = mat3_mul_vec(&BRADFORD, source_xyz);
    let destination_cone = mat3_mul_vec(&BRADFORD, destination_xyz);
    if source_cone
        .iter()
        .any(|component| !component.is_finite() || *component <= 1e-6)
        || destination_cone
            .iter()
            .any(|component| !component.is_finite() || *component <= 1e-6)
    {
        return None;
    }

    let scale = [
        destination_cone[0] / source_cone[0],
        destination_cone[1] / source_cone[1],
        destination_cone[2] / source_cone[2],
    ];
    if scale
        .iter()
        .any(|component| !component.is_finite() || *component <= 0.0)
    {
        return None;
    }
    let diagonal = [scale[0], 0.0, 0.0, 0.0, scale[1], 0.0, 0.0, 0.0, scale[2]];
    let adaptation = mat3_mul(&BRADFORD_INVERSE, &mat3_mul(&diagonal, &BRADFORD));
    matrix_is_reasonable(&adaptation).then_some(adaptation)
}

/// Validate explicit parametric primaries using the exact same domain,
/// invertibility, and matrix-safety checks as the render pipeline.
///
/// Named primaries are protocol enums backed by known-safe built-ins, so a
/// description without an explicit `primaries` payload needs no additional
/// mathematical validation here. When both named and explicit primaries are
/// present, the explicit payload is authoritative and is always checked.
#[must_use]
pub fn parametric_primaries_are_valid(params: &ParametricParams) -> bool {
    params.primaries.is_none()
        || rgb_to_xyz_matrix(&ColorSpacePrimaries::from_params(params)).is_some()
}

/// RGB→RGB matrix taking linear surface RGB to linear output RGB. A Bradford
/// CAT is folded between the RGB→XYZ and XYZ→RGB halves when their white
/// points differ. Named sRGB and BT.2020 are both D65, so their established
/// matrices stay byte-for-byte on the identity-CAT path. Invalid or
/// numerically unsafe descriptions fail closed to identity rather than
/// exposing a non-finite, singular, or unreasonably amplified matrix to the
/// renderer/KMS pipeline.
pub fn rgb_to_rgb_matrix(surface: &ColorSpacePrimaries, output: &ColorSpacePrimaries) -> [f32; 9] {
    checked_rgb_to_rgb_matrix(surface, output).unwrap_or(IDENTITY_3X3)
}

fn checked_rgb_to_rgb_matrix(
    surface: &ColorSpacePrimaries,
    output: &ColorSpacePrimaries,
) -> Option<[f32; 9]> {
    let m_in = rgb_to_xyz_matrix(surface)?;
    let m_out = rgb_to_xyz_matrix(output)?;
    let m_out_inv = invert_3x3(&m_out)?;
    let adaptation = bradford_adaptation_matrix(surface.w, output.w)?;
    let matrix = mat3_mul(&m_out_inv, &mat3_mul(&adaptation, &m_in));
    matrix_is_reasonable(&matrix).then_some(matrix)
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
    fn srgb_default_gate_accepts_only_srgb_transfer_and_primaries() {
        assert!(description_is_srgb_default(&ParametricParams::default()));
        assert!(description_is_srgb_default(
            &crate::backend::color_policy::srgb_params()
        ));
        // Gamma22/BT.1886 are legacy SDR curves but not the sRGB piecewise
        // transfer — an adapter without per-element plans must not guess.
        for tf in [2 /* Gamma22 */, 11 /* PQ */, 13 /* HLG */] {
            assert!(!description_is_srgb_default(&ParametricParams {
                tf_named: Some(tf),
                ..Default::default()
            }));
        }
        assert!(!description_is_srgb_default(&ParametricParams {
            primaries_named: Some(6 /* BT.2020 */),
            ..Default::default()
        }));
        // tf_power likewise steps off the exact sRGB curve.
        assert!(!description_is_srgb_default(&ParametricParams {
            tf_power: Some(22_000),
            ..Default::default()
        }));
    }

    #[test]
    fn scene_linear_hdr_plan_preserves_decode_and_targets_common_srgb() {
        for (named, expected) in [(11, TransferKind::St2084Pq), (13, TransferKind::Hlg)] {
            let params = ParametricParams {
                primaries_named: Some(6),
                tf_named: Some(named),
                ..Default::default()
            };
            assert!(ColorTransform::build_for_render_path(&params, &params, false).is_none());
            let transform = ColorTransform::build_for_render_path(&params, &params, true)
                .expect("scene-linear paths retain explicit HDR decode");
            assert_eq!(transform.inverse_eotf, expected);
            assert_eq!(transform.forward_eotf, TransferKind::Linear);
            assert!(!approx_mat(
                &transform.matrix_row_major,
                &IDENTITY_3X3,
                1e-6
            ));
        }
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
        assert!(
            approx_mat(&composed, &IDENTITY_3X3, 1e-3),
            "BT.2020→sRGB→BT.2020 should round-trip to identity, got {composed:?}"
        );
    }

    #[test]
    fn color_transform_matrix_upload_order_is_column_major() {
        let transform = ColorTransform {
            inverse_eotf: TransferKind::Linear,
            matrix_row_major: [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0],
            forward_eotf: TransferKind::Linear,
        };
        assert_eq!(
            transform.matrix_column_major(),
            [1.0, 4.0, 7.0, 2.0, 5.0, 8.0, 3.0, 6.0, 9.0]
        );
        assert_eq!(
            matrix_to_column_major(transform.matrix_row_major),
            transform.matrix_column_major()
        );
    }

    #[test]
    fn linear_srgb_plan_is_output_independent_and_keeps_explicit_decode() {
        let pq_bt2020 = ParametricParams {
            primaries_named: Some(6),
            tf_named: Some(11),
            ..Default::default()
        };
        let transform = ColorTransform::build_to_linear_srgb(&pq_bt2020);
        assert_eq!(transform.inverse_eotf, TransferKind::St2084Pq);
        assert_eq!(transform.forward_eotf, TransferKind::Linear);
        assert!(!approx_mat(
            &transform.matrix_row_major,
            &IDENTITY_3X3,
            1e-4
        ));

        let srgb_hlg = ParametricParams {
            primaries_named: Some(1),
            tf_named: Some(13),
            ..Default::default()
        };
        let hlg = ColorTransform::build_to_linear_srgb(&srgb_hlg);
        assert_eq!(hlg.inverse_eotf, TransferKind::Hlg);
        assert_eq!(hlg.forward_eotf, TransferKind::Linear);
        // sRGB primaries give an identity gamut map, but the ingress rescale
        // still folds in: the matrix is the HLG working-space scale times I.
        let scale = working_space_scale(TransferKind::Hlg);
        for (index, component) in hlg.matrix_row_major.iter().enumerate() {
            let expected = if [0, 4, 8].contains(&index) {
                scale
            } else {
                0.0
            };
            assert!(
                approx_eq(*component, expected, 1e-6),
                "matrix[{index}] = {component}, expected {expected}"
            );
        }
    }

    #[test]
    fn linear_srgb_ingress_rescale_is_bitwise_identity_for_the_sdr_family() {
        // Every SDR-family transfer keeps scale 1.0, so the folded matrix is
        // bitwise identical to the pre-rescale gamut matrix — the pixel
        // identity guarantee for all existing SDR routes.
        for tf_named in [
            9, /* sRGB */
            2, /* Gamma22 */
            1, /* BT.1886 */
        ] {
            for primaries_named in [1 /* sRGB */, 6 /* BT.2020 */] {
                let params = ParametricParams {
                    primaries_named: Some(primaries_named),
                    tf_named: Some(tf_named),
                    ..Default::default()
                };
                let transform = ColorTransform::build_to_linear_srgb(&params);
                let expected_gamut = if primaries_named == 1 {
                    IDENTITY_3X3
                } else {
                    rgb_to_rgb_matrix(
                        &ColorSpacePrimaries::BT2020_D65,
                        &ColorSpacePrimaries::SRGB_D65,
                    )
                };
                assert_eq!(
                    transform.matrix_row_major, expected_gamut,
                    "SDR ingress matrix must stay bitwise identical (tf={tf_named}, primaries={primaries_named})"
                );
            }
        }
        // tf_power and Linear are SDR-family too.
        let power = ParametricParams {
            tf_power: Some(24_000),
            ..Default::default()
        };
        assert_eq!(
            ColorTransform::build_to_linear_srgb(&power).matrix_row_major,
            IDENTITY_3X3
        );

        // PQ/HLG fold their absolute scale into the same matrix.
        for (tf_named, tf) in [(11, TransferKind::St2084Pq), (13, TransferKind::Hlg)] {
            let params = ParametricParams {
                primaries_named: Some(1),
                tf_named: Some(tf_named),
                ..Default::default()
            };
            let transform = ColorTransform::build_to_linear_srgb(&params);
            let scale = working_space_scale(tf);
            assert_eq!(
                transform.matrix_row_major,
                IDENTITY_3X3.map(|c| c * scale),
                "HDR ingress matrix must be the scale times the sRGB gamut map"
            );
        }
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
        let m = rgb_to_xyz_matrix(&ColorSpacePrimaries::SRGB_D65)
            .expect("the built-in sRGB primaries must be valid");
        let xw = m[0] + m[1] + m[2];
        let yw = m[3] + m[4] + m[5];
        let zw = m[6] + m[7] + m[8];
        // D65: x=0.3127, y=0.3290 ⇒ X = x/y ≈ 0.9504, Y = 1, Z = (1-x-y)/y ≈ 1.0888
        assert!(approx_eq(xw, 0.9504, 5e-4));
        assert!(approx_eq(yw, 1.0, 5e-4));
        assert!(approx_eq(zw, 1.0888, 5e-4));
    }

    #[test]
    fn bradford_d65_to_d50_matches_reference_matrix() {
        let d50 = Chromaticity {
            x: 0.34567,
            y: 0.35850,
        };
        let adaptation = bradford_adaptation_matrix(ColorSpacePrimaries::SRGB_D65.w, d50)
            .expect("D65 and D50 are valid white points");
        let expected = [
            1.047_81, 0.022_89, -0.050_13, 0.029_54, 0.990_48, -0.017_05, -0.009_23, 0.015_04,
            0.752_13,
        ];
        assert!(
            approx_mat(&adaptation, &expected, 5e-4),
            "unexpected D65→D50 Bradford matrix: {adaptation:?}"
        );
    }

    #[test]
    fn non_d65_rgb_conversion_preserves_neutral_and_round_trips() {
        let d50_srgb_primaries = ColorSpacePrimaries {
            w: Chromaticity {
                x: 0.34567,
                y: 0.35850,
            },
            ..ColorSpacePrimaries::SRGB_D65
        };
        let d50_to_d65 = rgb_to_rgb_matrix(&d50_srgb_primaries, &ColorSpacePrimaries::SRGB_D65);
        // A neutral RGB triplet describes the source white. Chromatic
        // adaptation must carry it to the destination white, which is also
        // neutral in destination RGB.
        for row in 0..3 {
            let sum = d50_to_d65[row * 3..row * 3 + 3].iter().sum::<f32>();
            assert!(
                approx_eq(sum, 1.0, 5e-4),
                "neutral row {row} mapped to {sum}: {d50_to_d65:?}"
            );
        }

        let d65_to_d50 = rgb_to_rgb_matrix(&ColorSpacePrimaries::SRGB_D65, &d50_srgb_primaries);
        let roundtrip = mat3_mul(&d65_to_d50, &d50_to_d65);
        assert!(
            approx_mat(&roundtrip, &IDENTITY_3X3, 2e-3),
            "D50→D65→D50 should round-trip, got {roundtrip:?}"
        );
    }

    #[test]
    fn invalid_custom_white_never_produces_non_finite_matrix_entries() {
        let invalid = ColorSpacePrimaries {
            w: Chromaticity { x: 0.3, y: 0.0 },
            ..ColorSpacePrimaries::SRGB_D65
        };
        assert!(checked_rgb_to_rgb_matrix(&invalid, &ColorSpacePrimaries::SRGB_D65).is_none());
        let matrix = rgb_to_rgb_matrix(&invalid, &ColorSpacePrimaries::SRGB_D65);
        assert!(matrix.iter().all(|component| component.is_finite()));
        assert_eq!(matrix, IDENTITY_3X3);
    }

    #[test]
    fn bt2020_red_spectral_boundary_remains_valid() {
        let red = ColorSpacePrimaries::BT2020_D65.r;
        assert_eq!(red.x + red.y, 1.0, "test must exercise the Z=0 boundary");
        assert!(valid_rgb_primary(red));
        assert!(rgb_to_xyz_matrix(&ColorSpacePrimaries::BT2020_D65).is_some());

        let conversion = checked_rgb_to_rgb_matrix(
            &ColorSpacePrimaries::BT2020_D65,
            &ColorSpacePrimaries::SRGB_D65,
        )
        .expect("BT.2020 must produce a usable conversion matrix");
        assert!(!approx_mat(&conversion, &IDENTITY_3X3, 1e-6));
    }

    #[test]
    fn negative_y_chromaticities_fail_closed() {
        let negative_primary = ColorSpacePrimaries {
            r: Chromaticity { x: 0.64, y: -0.01 },
            ..ColorSpacePrimaries::SRGB_D65
        };
        assert!(!valid_rgb_primary(negative_primary.r));
        assert!(rgb_to_xyz_matrix(&negative_primary).is_none());
        assert_eq!(
            rgb_to_rgb_matrix(&negative_primary, &ColorSpacePrimaries::SRGB_D65),
            IDENTITY_3X3
        );

        let negative_white = ColorSpacePrimaries {
            w: Chromaticity { x: 0.3127, y: -0.1 },
            ..ColorSpacePrimaries::SRGB_D65
        };
        assert!(!valid_white_point(negative_white.w));
        assert!(rgb_to_xyz_matrix(&negative_white).is_none());
        assert_eq!(
            rgb_to_rgb_matrix(&negative_white, &ColorSpacePrimaries::SRGB_D65),
            IDENTITY_3X3
        );
    }

    #[test]
    fn white_on_or_outside_xyz_boundary_fails_closed() {
        for white in [
            Chromaticity { x: 0.4, y: 0.6 },
            Chromaticity { x: 0.5, y: 0.6 },
        ] {
            let invalid = ColorSpacePrimaries {
                w: white,
                ..ColorSpacePrimaries::SRGB_D65
            };
            assert!(!valid_white_point(white));
            assert!(checked_rgb_to_rgb_matrix(&invalid, &ColorSpacePrimaries::SRGB_D65).is_none());
            assert_eq!(
                rgb_to_rgb_matrix(&invalid, &ColorSpacePrimaries::SRGB_D65),
                IDENTITY_3X3
            );
        }
    }

    #[test]
    fn collinear_and_near_singular_primaries_fail_closed() {
        let collinear = ColorSpacePrimaries {
            r: Chromaticity { x: 0.640, y: 0.330 },
            g: Chromaticity { x: 0.395, y: 0.195 },
            b: Chromaticity { x: 0.150, y: 0.060 },
            w: Chromaticity { x: 0.395, y: 0.195 },
        };
        let near_singular = ColorSpacePrimaries {
            g: Chromaticity {
                x: 0.395,
                y: 0.195_000_1,
            },
            w: Chromaticity {
                x: 0.395,
                y: 0.195_000_04,
            },
            ..collinear
        };

        for invalid in [collinear, near_singular] {
            assert!(valid_rgb_primary(invalid.r));
            assert!(valid_rgb_primary(invalid.g));
            assert!(valid_rgb_primary(invalid.b));
            assert!(valid_white_point(invalid.w));
            assert!(rgb_to_xyz_matrix(&invalid).is_none());
            assert_eq!(
                rgb_to_rgb_matrix(&invalid, &ColorSpacePrimaries::SRGB_D65),
                IDENTITY_3X3
            );
        }
    }

    #[test]
    fn non_finite_singular_and_huge_matrices_are_unreasonable() {
        assert!(matrix_is_reasonable(&IDENTITY_3X3));

        let mut non_finite = IDENTITY_3X3;
        non_finite[4] = f32::NAN;
        assert!(!matrix_is_reasonable(&non_finite));

        let singular = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0];
        assert!(!matrix_is_reasonable(&singular));

        let huge = [65.0, 0.0, 0.0, 0.0, 65.0, 0.0, 0.0, 0.0, 65.0];
        assert!(!matrix_is_reasonable(&huge));
    }

    #[test]
    fn production_builders_keep_transfer_semantics_but_drop_invalid_gamut_matrix() {
        let invalid_surface = ParametricParams {
            primaries_named: Some(6),
            primaries: Some([
                640_000, -10_000, 300_000, 600_000, 150_000, 60_000, 312_700, 329_000,
            ]),
            tf_named: Some(11 /* PQ */),
            ..Default::default()
        };
        let output = ParametricParams {
            primaries_named: Some(6 /* BT.2020 */),
            tf_named: Some(13 /* HLG */),
            ..Default::default()
        };

        let scene_linear = ColorTransform::build_to_linear_srgb(&invalid_surface);
        assert_eq!(scene_linear.inverse_eotf, TransferKind::St2084Pq);
        assert_eq!(scene_linear.forward_eotf, TransferKind::Linear);
        // The invalid gamut map fails closed to identity; the PQ ingress
        // rescale still folds in, so the matrix is scale·I rather than I.
        assert_eq!(
            scene_linear.matrix_row_major,
            IDENTITY_3X3.map(|c| c * working_space_scale(TransferKind::St2084Pq))
        );

        let legacy = ColorTransform::build_explicit(&invalid_surface, &output);
        assert_eq!(legacy.inverse_eotf, TransferKind::St2084Pq);
        assert_eq!(legacy.forward_eotf, TransferKind::Hlg);
        assert_eq!(legacy.matrix_row_major, IDENTITY_3X3);
    }

    #[test]
    fn power_curve_inverse_round_trips() {
        let tf = TransferKind::Power {
            gamma_x10000: 22_000,
        };
        // Encoding (forward) is x^(1/2.2); inverse is x^2.2. Composition is identity.
        let encoded = 0.5_f32.powf(1.0 / 2.2);
        let linear = tf.inverse(encoded);
        assert!(approx_eq(linear, 0.5, 1e-4));
    }

    #[test]
    fn transferkind_from_params_resolves_named() {
        let p = ParametricParams {
            tf_named: Some(11 /* PQ */),
            ..Default::default()
        };
        assert_eq!(TransferKind::from_params(&p), TransferKind::St2084Pq);
        let p = ParametricParams {
            tf_named: Some(13 /* HLG */),
            ..Default::default()
        };
        assert_eq!(TransferKind::from_params(&p), TransferKind::Hlg);
        let p = ParametricParams {
            tf_named: Some(9 /* sRGB */),
            ..Default::default()
        };
        assert_eq!(TransferKind::from_params(&p), TransferKind::Srgb);
        let p = ParametricParams {
            tf_power: Some(18_000),
            ..Default::default()
        };
        assert_eq!(
            TransferKind::from_params(&p),
            TransferKind::Power {
                gamma_x10000: 18_000
            }
        );
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
        assert_eq!(
            TransferKind::Power {
                gamma_x10000: 22_000
            }
            .shader_id(),
            1
        );
        assert_eq!(TransferKind::Bt1886.shader_id(), 2);
        assert_eq!(TransferKind::Gamma22.shader_id(), 3);
        assert_eq!(TransferKind::St2084Pq.shader_id(), 4);
        assert_eq!(TransferKind::Hlg.shader_id(), 5);
        assert_eq!(TransferKind::Srgb.shader_id(), 6);
    }

    #[test]
    fn gamma_for_shader_defined_for_every_variant() {
        // Power's gamma comes from the variant. Other variants return 1.0 so
        // the matching shader uniform is always defined, even on a TF branch
        // that never consults the value — undefined-uniform reads are
        // implementation-defined and we don't want stale data leaking in.
        assert_eq!(
            TransferKind::Power {
                gamma_x10000: 24_000
            }
            .gamma_for_shader(),
            2.4
        );
        assert_eq!(TransferKind::Linear.gamma_for_shader(), 1.0);
        assert_eq!(TransferKind::Bt1886.gamma_for_shader(), 1.0);
        assert_eq!(TransferKind::Gamma22.gamma_for_shader(), 1.0);
        assert_eq!(TransferKind::St2084Pq.gamma_for_shader(), 1.0);
        assert_eq!(TransferKind::Hlg.gamma_for_shader(), 1.0);
        assert_eq!(TransferKind::Srgb.gamma_for_shader(), 1.0);
        // Zero gamma must not become a divide-by-zero or NaN producer.
        let g = TransferKind::Power { gamma_x10000: 0 }.gamma_for_shader();
        assert!(g.is_finite() && g > 0.0);
    }

    #[test]
    fn srgb_roundtrip_endpoints_and_midpoint() {
        // Endpoints must hit exactly.
        assert!(approx_eq(TransferKind::Srgb.forward(0.0), 0.0, 1e-7));
        assert!(approx_eq(TransferKind::Srgb.forward(1.0), 1.0, 1e-5));
        assert!(approx_eq(TransferKind::Srgb.inverse(0.0), 0.0, 1e-7));
        assert!(approx_eq(TransferKind::Srgb.inverse(1.0), 1.0, 1e-5));
        // forward ∘ inverse ≈ identity at a few interior points.
        for &e in &[0.05f32, 0.18, 0.5, 0.75] {
            let r = TransferKind::Srgb.forward(TransferKind::Srgb.inverse(e));
            assert!(approx_eq(r, e, 1e-5), "srgb round-trip at {e} got {r}");
        }
    }

    #[cfg(feature = "backend-wayland-udev")]
    #[test]
    fn build_gamma_lut_linear_is_identity() {
        let lut = build_gamma_lut(TransferKind::Linear, 256);
        assert_eq!(lut.len(), 256);
        assert_eq!(lut[0].red, 0);
        assert_eq!(lut[0].green, 0);
        assert_eq!(lut[0].blue, 0);
        assert_eq!(lut[255].red, 65535);
        assert_eq!(lut[255].green, 65535);
        assert_eq!(lut[255].blue, 65535);
        // Channels stay equal (gray ramp), reserved is zero.
        for e in &lut {
            assert_eq!(e.red, e.green);
            assert_eq!(e.green, e.blue);
            assert_eq!(e.reserved, 0);
        }
        // Linear ramp: entry i ≈ i * 257 (255 * 257 = 65535).
        assert!((lut[128].red as i32 - 32896).abs() <= 1);
    }

    #[cfg(feature = "backend-wayland-udev")]
    #[test]
    fn build_gamma_lut_srgb_endpoints_and_known_midpoint() {
        let lut = build_gamma_lut(TransferKind::Srgb, 256);
        assert_eq!(lut[0].red, 0);
        assert_eq!(lut[255].red, 65535);
        // Reference: linear 0.5 → sRGB encoded ≈ 0.7353569; ×65535 ≈ 48196.
        let expected = (srgb_forward(0.5) * 65535.0 + 0.5) as u16;
        // Use the lut entry whose linear x is exactly 0.5 (i = 127.5 isn't integer;
        // check the nearest two are within 1 LSB of the analytic curve).
        for i in [127usize, 128] {
            let lin = i as f32 / 255.0;
            let analytic = (srgb_forward(lin) * 65535.0 + 0.5) as u16;
            assert!(
                (lut[i].red as i32 - analytic as i32).abs() <= 1,
                "i={i} lut={} analytic={analytic}",
                lut[i].red
            );
        }
        // Sanity: at index 127 the value is in the expected neighborhood.
        assert!((lut[127].red as i32 - expected as i32).abs() <= 200);
    }

    #[test]
    fn pq_forward_endpoints_and_known_point() {
        // L=0 ⇒ encoded = (C1)^M2 ≈ 7.3e-7 (mathematically nonzero by spec
        // but quantizes to 0 in 16-bit). L=1 ⇒ encoded = 1.0.
        assert!(pq_forward(0.0) < 1e-5);
        assert!(approx_eq(pq_forward(1.0), 1.0, 1e-6));
        // SMPTE ST 2084 reference: linear 0.01 (100 cd/m²) ⇒ encoded ≈ 0.5081.
        assert!(approx_eq(pq_forward(0.01), 0.5081, 5e-4));
    }

    #[test]
    fn pq_forward_inverse_round_trips() {
        for &l in &[0.001_f32, 0.01, 0.1, 0.5, 0.9, 1.0] {
            let r = pq_inverse(pq_forward(l));
            assert!(approx_eq(r, l, 5e-4), "pq round-trip at {l} got {r}");
        }
    }

    #[test]
    fn hlg_forward_endpoints_and_known_points() {
        assert!(approx_eq(hlg_forward(0.0), 0.0, 1e-7));
        assert!(approx_eq(hlg_forward(1.0), 1.0, 1e-5));
        // Continuity at the L=1/12 piecewise breakpoint: both branches return 0.5.
        assert!(approx_eq(hlg_forward(1.0 / 12.0), 0.5, 1e-5));
        // Quadratic-region check: L = 0.25/3 ⇒ encoded = sqrt(0.25) = 0.5.
        assert!(approx_eq(hlg_forward(0.25 / 3.0), 0.5, 1e-5));
    }

    #[test]
    fn hlg_forward_inverse_round_trips() {
        for &l in &[0.0_f32, 0.01, 0.05, 1.0 / 12.0, 0.1, 0.5, 0.9, 1.0] {
            let r = hlg_inverse(hlg_forward(l));
            assert!(approx_eq(r, l, 5e-5), "hlg round-trip at {l} got {r}");
        }
    }

    #[test]
    fn luminance_anchors_match_documented_conventions() {
        assert_eq!(PQ_MAX_LUMINANCE_NITS, 10_000.0);
        assert_eq!(HLG_NOMINAL_PEAK_NITS, 1_000.0);
        assert_eq!(SDR_REFERENCE_WHITE_NITS, 203.0);
    }

    #[test]
    fn pq_nits_known_reference_points() {
        // ST 2084 endpoints: encoded 1.0 ↔ 10 000 cd/m², 0 ↔ 0.
        assert!(approx_eq(pq_decode_nits(1.0), 10_000.0, 1.0));
        assert!(approx_eq(pq_decode_nits(0.0), 0.0, 1e-6));
        // SMPTE reference: encoded ≈0.5081 ↔ 100 cd/m².
        assert!(approx_eq(pq_decode_nits(0.5081), 100.0, 5.0));
        // BT.2408 reference white: 203 cd/m² encodes to ≈0.5807.
        assert!(approx_eq(pq_encode_nits(203.0), 0.5807, 1e-3));
    }

    #[test]
    fn pq_nits_round_trips() {
        for &nits in &[0.0_f32, 0.05, 1.0, 100.0, 203.0, 1_000.0, 4_000.0, 10_000.0] {
            let r = pq_decode_nits(pq_encode_nits(nits));
            assert!(
                approx_eq(r, nits, nits.max(1.0) * 1e-3),
                "pq nits round-trip at {nits} got {r}"
            );
        }
        // Out-of-range luminance clamps to the ST 2084 ceiling / floor.
        assert_eq!(pq_encode_nits(20_000.0), 1.0);
        assert_eq!(pq_encode_nits(-1.0), pq_encode_nits(0.0));
    }

    #[test]
    fn hlg_nits_known_point_and_round_trips() {
        // BT.2100 reference system: encoded 1.0 ↔ the 1000 cd/m² nominal peak;
        // encoded 0.75 ↔ linear 0.265 → 265 cd/m².
        assert!(approx_eq(hlg_decode_nits(1.0), 1_000.0, 1e-2));
        assert!(approx_eq(hlg_decode_nits(0.0), 0.0, 1e-6));
        assert!(approx_eq(hlg_decode_nits(0.75), 265.0, 1.0));
        for &nits in &[0.0_f32, 1.0, 100.0, 203.0, 500.0, 1_000.0] {
            let r = hlg_decode_nits(hlg_encode_nits(nits));
            assert!(
                approx_eq(r, nits, nits.max(1.0) * 1e-3),
                "hlg nits round-trip at {nits} got {r}"
            );
        }
        assert!(approx_eq(hlg_encode_nits(2_000.0), 1.0, 1e-5));
    }

    #[test]
    fn working_linear_nits_anchor_at_reference_white() {
        assert!(approx_eq(working_linear_to_nits(1.0), 203.0, 1e-6));
        assert!(approx_eq(nits_to_working_linear(203.0), 1.0, 1e-6));
        for &nits in &[0.0_f32, 50.0, 203.0, 1_000.0, 10_000.0] {
            let r = working_linear_to_nits(nits_to_working_linear(nits));
            assert!(approx_eq(r, nits, 1e-2), "working↔nits at {nits} got {r}");
        }
    }

    #[test]
    fn working_space_scale_anchors_each_transfer() {
        assert_eq!(working_space_scale(TransferKind::Srgb), 1.0);
        assert_eq!(working_space_scale(TransferKind::Linear), 1.0);
        assert_eq!(working_space_scale(TransferKind::Gamma22), 1.0);
        assert_eq!(working_space_scale(TransferKind::Bt1886), 1.0);
        assert_eq!(
            working_space_scale(TransferKind::Power {
                gamma_x10000: 22_000
            }),
            1.0
        );
        assert!(approx_eq(
            working_space_scale(TransferKind::St2084Pq),
            10_000.0 / 203.0,
            1e-3
        ));
        assert!(approx_eq(
            working_space_scale(TransferKind::Hlg),
            1_000.0 / 203.0,
            1e-4
        ));
        // Decoded PQ white 1.0 lands at 10 000 cd/m² in working units.
        let pq_white_nits = working_linear_to_nits(working_space_scale(TransferKind::St2084Pq));
        assert!(approx_eq(pq_white_nits, 10_000.0, 1.0));
    }

    #[test]
    fn tone_map_policy_default_selection() {
        let hdr_output_peak = 1_000.0 / 203.0; // 1000-nit HDR output in working units
        let pq_content_peak = 10_000.0 / 203.0; // full-range PQ content
        // SDR content into an HDR output fits: reference-white mapping.
        assert_eq!(
            ToneMapPolicy::for_peaks(1.0, hdr_output_peak),
            ToneMapPolicy::ReferenceWhite
        );
        // PQ content into an SDR output exceeds it: clip by default.
        assert_eq!(
            ToneMapPolicy::for_peaks(pq_content_peak, 1.0),
            ToneMapPolicy::Clip
        );
        // Equal peaks need no compression.
        assert_eq!(
            ToneMapPolicy::for_peaks(1.0, 1.0),
            ToneMapPolicy::ReferenceWhite
        );
        // Non-finite input must not silently pick the pass-through policy.
        assert_eq!(ToneMapPolicy::for_peaks(f32::NAN, 1.0), ToneMapPolicy::Clip);
    }

    #[test]
    fn tone_map_shader_id_is_stable_and_zero_is_passthrough() {
        // The scene-linear encode shader's tone-map branch and the GL
        // zero-initialized default both depend on these exact values.
        assert_eq!(ToneMapPolicy::ReferenceWhite.shader_id(), 0);
        assert_eq!(ToneMapPolicy::Clip.shader_id(), 1);
        assert_eq!(ToneMapPolicy::ReinhardShoulder.shader_id(), 2);
    }

    #[test]
    fn output_tone_map_plan_selection_matrix() {
        // SDR content onto any output fits: pass-through at the output's own
        // target peak.
        for tf in [
            TransferKind::Srgb,
            TransferKind::Gamma22,
            TransferKind::St2084Pq,
            TransferKind::Hlg,
        ] {
            let plan = OutputToneMapPlan::for_output(1.0, tf);
            assert_eq!(plan.policy, ToneMapPolicy::ReferenceWhite);
            assert_eq!(plan.target_peak_working, working_space_scale(tf));
        }
        // HDR content onto an SDR output clips at the SDR reference white.
        let pq_peak = working_space_scale(TransferKind::St2084Pq);
        let plan = OutputToneMapPlan::for_output(pq_peak, TransferKind::Srgb);
        assert_eq!(plan.policy, ToneMapPolicy::Clip);
        assert_eq!(plan.source_peak_working, pq_peak);
        assert_eq!(plan.target_peak_working, 1.0);
    }

    #[test]
    fn output_tone_map_plan_identity_is_bitwise_noop() {
        // The identity plan (SDR content, SDR output) must reproduce the
        // pre-tone-map delivery exactly, including signs and subnormals:
        // ReferenceWhite returns x and x / 1.0 == x in IEEE-754.
        for &x in &[
            0.0_f32,
            -0.0,
            1.0,
            -0.375,
            49.25,
            1.0e-30,
            f32::MIN_POSITIVE,
        ] {
            assert_eq!(OutputToneMapPlan::IDENTITY.map_to_output_scale(x), x);
        }
    }

    #[test]
    fn output_tone_map_plan_rescales_and_clips() {
        // SDR content onto a PQ output re-anchors onto the 10 000 cd/m² scale:
        // working 1.0 (203 cd/m² reference white) lands at 203/10 000.
        let pq_plan = OutputToneMapPlan::for_output(1.0, TransferKind::St2084Pq);
        assert!(approx_eq(
            pq_plan.map_to_output_scale(1.0),
            203.0 / 10_000.0,
            1e-6
        ));
        // PQ content onto an SDR output clips at the SDR reference white.
        let sdr_plan = OutputToneMapPlan::for_output(
            working_space_scale(TransferKind::St2084Pq),
            TransferKind::Srgb,
        );
        assert_eq!(sdr_plan.map_to_output_scale(12.0), 1.0);
        assert_eq!(sdr_plan.map_to_output_scale(0.5), 0.5);
        // A hand-built degenerate target falls back to the unit divisor.
        let degenerate = OutputToneMapPlan {
            policy: ToneMapPolicy::ReferenceWhite,
            source_peak_working: 1.0,
            target_peak_working: 0.0,
        };
        assert_eq!(degenerate.map_to_output_scale(0.25), 0.25);
    }

    #[test]
    fn tone_map_reference_white_is_identity() {
        for &x in &[0.0_f32, 0.5, 1.0, 4.9, 49.3] {
            assert_eq!(
                ToneMapPolicy::ReferenceWhite.map_working_linear(x, 1.0, 49.3),
                x
            );
        }
    }

    #[test]
    fn tone_map_clip_boundaries() {
        let clip = ToneMapPolicy::Clip;
        assert_eq!(clip.map_working_linear(-0.5, 49.3, 1.0), 0.0);
        assert_eq!(clip.map_working_linear(0.5, 49.3, 1.0), 0.5);
        assert_eq!(clip.map_working_linear(1.0, 49.3, 1.0), 1.0);
        assert_eq!(clip.map_working_linear(12.0, 49.3, 1.0), 1.0);
    }

    #[test]
    fn tone_map_reinhard_endpoints_monotonic_and_gradation() {
        let src = 10_000.0 / 203.0; // full-range PQ content in working units
        let dst = 1.0; // SDR output
        let m = |x| ToneMapPolicy::ReinhardShoulder.map_working_linear(x, src, dst);
        assert!(approx_eq(m(0.0), 0.0, 1e-6));
        assert!(
            approx_eq(m(src), dst, 1e-4),
            "source peak must land on target peak"
        );
        let mut prev = m(0.0);
        for i in 1..=64 {
            let y = m(src * i as f32 / 64.0);
            assert!(
                y >= prev && y <= dst,
                "reinhard must be monotone and in-range: {prev} -> {y}"
            );
            prev = y;
        }
        // Unlike clip, highlight gradation above reference white survives.
        assert!(m(2.0) > m(1.0));
        // Anything beyond the source peak still clips to the target peak.
        assert_eq!(m(src * 2.0), dst);
    }

    #[test]
    fn tone_map_reinhard_degenerate_peaks_fall_back_to_clip() {
        let m = ToneMapPolicy::ReinhardShoulder;
        assert_eq!(m.map_working_linear(2.0, f32::NAN, 1.0), 1.0);
        assert_eq!(m.map_working_linear(2.0, 0.0, 1.0), 1.0);
        assert_eq!(m.map_working_linear(-3.0, 49.3, 1.0), 0.0);
        // Content that already fits the target is clamped, not compressed.
        assert_eq!(m.map_working_linear(0.8, 1.0, 4.9), 0.8);
        assert_eq!(m.map_working_linear(5.5, 1.0, 4.9), 4.9);
    }

    #[cfg(feature = "backend-wayland-udev")]
    #[test]
    fn build_gamma_lut_pq_monotonic_and_endpoints() {
        let lut = build_gamma_lut(TransferKind::St2084Pq, 1024);
        assert_eq!(lut[0].red, 0);
        assert_eq!(lut[lut.len() - 1].red, 65535);
        for w in lut.windows(2) {
            assert!(w[1].red >= w[0].red, "PQ LUT non-monotonic at {}", w[0].red);
        }
    }

    #[cfg(feature = "backend-wayland-udev")]
    #[test]
    fn build_gamma_lut_hlg_monotonic_and_endpoints() {
        let lut = build_gamma_lut(TransferKind::Hlg, 1024);
        assert_eq!(lut[0].red, 0);
        assert_eq!(lut[lut.len() - 1].red, 65535);
        for w in lut.windows(2) {
            assert!(
                w[1].red >= w[0].red,
                "HLG LUT non-monotonic at {}",
                w[0].red
            );
        }
    }

    #[cfg(feature = "backend-wayland-udev")]
    #[test]
    fn build_ctm_identity_packs_to_one_and_zero() {
        let ctm = build_ctm(IDENTITY_CTM);
        let one = 1u64 << 32;
        assert_eq!(ctm.matrix, [one, 0, 0, 0, one, 0, 0, 0, one]);
    }

    #[cfg(feature = "backend-wayland-udev")]
    #[test]
    fn build_ctm_negative_sets_sign_bit() {
        let m = [-0.5_f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let ctm = build_ctm(m);
        let entry = ctm.matrix[0];
        assert_eq!(entry >> 63, 1, "sign bit must be set for negative");
        assert_eq!(
            entry & 0x7FFF_FFFF_FFFF_FFFF,
            1u64 << 31,
            "magnitude 0.5 → 2^31"
        );
        // Zero entries stay 0 (no sign bit on +0.0).
        for &v in &ctm.matrix[1..] {
            assert_eq!(v, 0);
        }
    }

    #[cfg(feature = "backend-wayland-udev")]
    #[test]
    fn build_ctm_round_trip_unpack() {
        // A non-trivial real matrix: sRGB→BT.2020 gamut.
        let m = rgb_to_rgb_matrix(
            &ColorSpacePrimaries::SRGB_D65,
            &ColorSpacePrimaries::BT2020_D65,
        );
        let ctm = build_ctm(m);
        // Unpack each u64 back to f32 and verify within 1 LSB (~2.3e-10).
        for (i, &packed) in ctm.matrix.iter().enumerate() {
            let neg = packed >> 63 == 1;
            let mag = (packed & 0x7FFF_FFFF_FFFF_FFFF) as f64 / (1u64 << 32) as f64;
            let unpacked = if neg { -mag } else { mag } as f32;
            assert!(
                (unpacked - m[i]).abs() < 1e-9,
                "entry {i}: packed→{unpacked} vs source→{}",
                m[i]
            );
        }
    }

    #[cfg(feature = "backend-wayland-udev")]
    #[test]
    fn build_gamma_lut_srgb_monotonic() {
        let lut = build_gamma_lut(TransferKind::Srgb, 1024);
        for w in lut.windows(2) {
            assert!(w[1].red >= w[0].red, "non-monotonic at value {}", w[0].red);
        }
    }

    #[cfg(feature = "backend-wayland-udev")]
    #[test]
    fn scanout_lut_is_byte_identical_to_legacy_ramp_for_the_sdr_family() {
        // The delivery rescale divides by working_space_scale(tf) == 1.0 for
        // every SDR curve, so the wired scanout bake must reproduce the legacy
        // OETF-only ramp entry for entry. This is the hardware-path half of
        // the SDR pixel-identity guarantee.
        for tf in [
            TransferKind::Linear,
            TransferKind::Srgb,
            TransferKind::Gamma22,
            TransferKind::Bt1886,
            TransferKind::Power {
                gamma_x10000: 22_000,
            },
        ] {
            let legacy = build_gamma_lut(tf, 256);
            let scanout = build_gamma_lut_scanout(tf, 256);
            assert_eq!(
                legacy
                    .iter()
                    .map(|e| (e.red, e.green, e.blue))
                    .collect::<Vec<_>>(),
                scanout
                    .iter()
                    .map(|e| (e.red, e.green, e.blue))
                    .collect::<Vec<_>>(),
                "scanout LUT diverged from the legacy ramp for {tf:?}"
            );
        }
    }

    #[cfg(feature = "backend-wayland-udev")]
    #[test]
    fn scanout_lut_reanchors_hdr_transfers_onto_reference_white() {
        // PQ: the last entry (working 1.0 = 203 cd/m²) must encode 203 nits,
        // not 10 000. Quantized: pq_encode_nits(203) ≈ 0.5807 × 65535.
        let lut = build_gamma_lut_scanout(TransferKind::St2084Pq, 1024);
        let last = lut[1023].red;
        let expected = (pq_encode_nits(203.0) * 65535.0 + 0.5) as u16;
        assert!(
            (i32::from(last) - i32::from(expected)).abs() <= 2,
            "PQ scanout LUT must anchor working 1.0 at 203 nits: got {last}, expected {expected}"
        );
        assert_eq!(lut[0].red, 0);
        for w in lut.windows(2) {
            assert!(w[1].red >= w[0].red, "PQ scanout LUT non-monotonic");
        }

        let lut = build_gamma_lut_scanout(TransferKind::Hlg, 1024);
        let expected = (hlg_encode_nits(203.0) * 65535.0 + 0.5) as u16;
        assert!(
            (i32::from(lut[1023].red) - i32::from(expected)).abs() <= 2,
            "HLG scanout LUT must anchor working 1.0 at 203 nits"
        );
    }

    #[cfg(feature = "backend-wayland-udev")]
    #[test]
    fn delivery_lut_clip_matches_scanout_curve_over_the_fb_domain() {
        // The coincidence the KMS tracked state relies on: for every
        // for_peaks-selected policy (ReferenceWhite, or Clip at a target
        // peak ≥ 1.0), the baked curve over the framebuffer-normalized domain
        // [0, 1] is the plain rescaled OETF, independent of the source peak.
        let pq_peak = working_space_scale(TransferKind::St2084Pq);
        for tf in [
            TransferKind::Srgb,
            TransferKind::St2084Pq,
            TransferKind::Hlg,
        ] {
            let canonical = build_gamma_lut_scanout(tf, 256);
            for plan in [
                OutputToneMapPlan::for_output(1.0, tf),
                OutputToneMapPlan::for_output(pq_peak, tf),
            ] {
                let baked = build_gamma_lut_delivery(tf, plan, 256);
                assert_eq!(
                    canonical.iter().map(|e| e.red).collect::<Vec<_>>(),
                    baked.iter().map(|e| e.red).collect::<Vec<_>>(),
                    "policy {:?} changed the LUT for {tf:?}",
                    plan.policy
                );
            }
        }
    }

    #[cfg(feature = "backend-wayland-udev")]
    #[test]
    fn delivery_lut_bakes_reinhard_shoulder_with_source_peak() {
        // The peak-dependent policy: an SDR-output bake with full-range PQ
        // content compresses highlights instead of clipping at the domain end.
        let source_peak = working_space_scale(TransferKind::St2084Pq);
        let plan = OutputToneMapPlan {
            policy: ToneMapPolicy::ReinhardShoulder,
            source_peak_working: source_peak,
            target_peak_working: 1.0,
        };
        let lut = build_gamma_lut_delivery(TransferKind::Srgb, plan, 1024);
        assert_eq!(lut[0].red, 0);
        for w in lut.windows(2) {
            assert!(w[1].red >= w[0].red, "Reinhard LUT non-monotonic");
        }
        // Every entry matches the CPU composition of the definition-layer
        // map and the sRGB OETF.
        for (i, entry) in lut.iter().enumerate() {
            let x = i as f32 / 1023.0;
            let expected = (TransferKind::Srgb
                .forward(plan.map_to_output_scale(x))
                .clamp(0.0, 1.0)
                * 65535.0
                + 0.5) as u32;
            assert!(
                (u32::from(entry.red)).abs_diff(expected) <= 1,
                "entry {i}: lut={} expected={expected}",
                entry.red
            );
        }
        // The shoulder compresses: reference white no longer maps to the top.
        assert!(lut[1023].red < 65535);
    }
}
