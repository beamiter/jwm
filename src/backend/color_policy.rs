//! Pure colour-management parameter policy shared by the Wayland
//! wp-color-management implementation and the backend-neutral IPC
//! diagnostics.
//!
//! Values use the wp_color_manager_v1 protocol encoding (named enums as
//! `u32`, CIE 1931 xy chromaticities scaled by 1_000_000) so the Wayland
//! protocol module can hand them to clients verbatim, but this module itself
//! depends on no protocol bindings — the numeric constants below are frozen
//! by a parity test against the generated protocol enums in
//! `wayland_udev::color_management`.

use crate::backend::edid::EdidHdrCapabilities;

/// `wp_color_manager_v1::transfer_function` values used by the policy.
pub const TF_BT1886: u32 = 1;
pub const TF_GAMMA22: u32 = 2;
pub const TF_ST2084_PQ: u32 = 11;
pub const TF_HLG: u32 = 13;

/// `wp_color_manager_v1::primaries` values used by the policy.
pub const PRIMARIES_NAMED_SRGB: u32 = 1;
pub const PRIMARIES_NAMED_BT2020: u32 = 6;

// CIE 1931 xy chromaticities scaled by 1_000_000 (the protocol's encoding).
pub const PRIMARIES_BT709: [i32; 8] = [
    640_000, 330_000, 300_000, 600_000, 150_000, 60_000, 312_700, 329_000,
];
pub const PRIMARIES_BT2020: [i32; 8] = [
    708_000, 292_000, 170_000, 797_000, 131_000, 46_000, 312_700, 329_000,
];

#[must_use]
pub fn advanced_color_management_enabled() -> bool {
    std::env::var_os("JWM_COLOR_MANAGEMENT_ADVANCED").as_deref() == Some(std::ffi::OsStr::new("1"))
}

/// Accumulated parametric properties (collected by a creator object before
/// `create`, then frozen into an ImageDescription).
#[derive(Debug, Clone, Default)]
pub struct ParametricParams {
    pub tf_named: Option<u32>,
    pub tf_power: Option<u32>,
    pub primaries_named: Option<u32>,
    pub primaries: Option<[i32; 8]>,
    pub min_lum: Option<u32>,
    pub max_lum: Option<u32>,
    pub reference_lum: Option<u32>,
    pub mastering_primaries: Option<[i32; 8]>,
    pub mastering_min_lum: Option<u32>,
    pub mastering_max_lum: Option<u32>,
    pub max_cll: Option<u32>,
    pub max_fall: Option<u32>,
}

impl ParametricParams {
    /// A creator may only `create` once both a transfer characteristic and a
    /// primaries description have been supplied.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        (self.tf_named.is_some() || self.tf_power.is_some())
            && (self.primaries_named.is_some() || self.primaries.is_some())
    }
}

/// Whether two descriptions agree on the fields that affect rendering
/// (transfer function, primaries, peak luminance). Mastering metadata is
/// advisory and deliberately ignored.
#[must_use]
pub fn params_match(a: &ParametricParams, b: &ParametricParams) -> bool {
    a.tf_named == b.tf_named
        && a.primaries_named == b.primaries_named
        && a.primaries == b.primaries
        && a.max_lum == b.max_lum
}

/// Build a default sRGB parametric description (used when no HDR caps are
/// known for an output).
#[must_use]
pub fn srgb_params() -> ParametricParams {
    ParametricParams {
        primaries_named: Some(PRIMARIES_NAMED_SRGB),
        tf_named: Some(TF_GAMMA22),
        ..ParametricParams::default()
    }
}

/// Translate an EDID HDR Static Metadata block (CTA-861) into a parametric
/// image description. Mirrors the policy used by `hdr_metadata::build_from_edid`
/// for the kernel-side blob so the wp-color-management answer and the
/// HDR_OUTPUT_METADATA push agree on EOTF and gamut.
#[must_use]
pub fn params_from_edid(caps: &EdidHdrCapabilities) -> ParametricParams {
    let mut p = ParametricParams::default();

    // EOTF: prefer PQ > HLG > BT.1886.
    p.tf_named = Some(if caps.supports_pq {
        TF_ST2084_PQ
    } else if caps.supports_hlg {
        TF_HLG
    } else {
        TF_BT1886
    });

    // Container primaries: BT.2020 for any HDR-signalled display, sRGB otherwise.
    let hdr = caps.supports_pq || caps.supports_hlg || caps.supports_bt2020;
    if hdr {
        p.primaries_named = Some(PRIMARIES_NAMED_BT2020);
        p.primaries = Some(PRIMARIES_BT2020);
    } else {
        p.primaries_named = Some(PRIMARIES_NAMED_SRGB);
        p.primaries = Some(PRIMARIES_BT709);
    }

    // Luminance range (cd/m²). Spec scales min_lum by 10000, max_lum unscaled.
    if caps.max_luminance_nits > 0.0 {
        let max_lum = caps.max_luminance_nits.round().max(1.0) as u32;
        let min_lum_scaled = (caps.min_luminance_nits.max(0.0) * 10_000.0).round() as u32;
        // Reference white for HDR: 203 cd/m² per BT.2408. For SDR fall back to max.
        let reference_lum = if hdr { 203 } else { max_lum };
        p.min_lum = Some(min_lum_scaled);
        p.max_lum = Some(max_lum);
        p.reference_lum = Some(reference_lum);

        // Mastering display volume (target color volume) mirrors the container.
        if hdr {
            p.mastering_primaries = Some(PRIMARIES_BT2020);
        }
        p.mastering_min_lum = Some(min_lum_scaled);
        p.mastering_max_lum = Some(max_lum);

        // Surface-as-display: max_cll matches the display's peak.
        p.max_cll = Some(max_lum);
    }

    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(
        pq: bool,
        hlg: bool,
        bt2020: bool,
        max_nits: f32,
        min_nits: f32,
    ) -> EdidHdrCapabilities {
        EdidHdrCapabilities {
            max_luminance_nits: max_nits,
            min_luminance_nits: min_nits,
            supports_bt2020: bt2020,
            supports_pq: pq,
            supports_hlg: hlg,
        }
    }

    #[test]
    fn sdr_only_edid_maps_to_bt709_bt1886() {
        let p = params_from_edid(&caps(false, false, false, 0.0, 0.0));
        assert_eq!(p.tf_named, Some(TF_BT1886));
        assert_eq!(p.primaries_named, Some(PRIMARIES_NAMED_SRGB));
        assert_eq!(p.primaries, Some(PRIMARIES_BT709));
        // No luminance block → no mastering metadata.
        assert!(p.min_lum.is_none());
        assert!(p.max_cll.is_none());
    }

    #[test]
    fn pq_hdr_edid_maps_to_bt2020_pq_with_mastering() {
        let p = params_from_edid(&caps(true, false, true, 1000.0, 0.05));
        assert_eq!(p.tf_named, Some(TF_ST2084_PQ));
        assert_eq!(p.primaries_named, Some(PRIMARIES_NAMED_BT2020));
        assert_eq!(p.primaries, Some(PRIMARIES_BT2020));
        assert_eq!(p.max_lum, Some(1000));
        // min_lum scaled by 10_000: 0.05 → 500.
        assert_eq!(p.min_lum, Some(500));
        // BT.2408 reference white for HDR.
        assert_eq!(p.reference_lum, Some(203));
        assert_eq!(p.mastering_primaries, Some(PRIMARIES_BT2020));
        assert_eq!(p.mastering_max_lum, Some(1000));
        assert_eq!(p.max_cll, Some(1000));
    }

    #[test]
    fn hlg_preferred_over_bt1886_when_only_hlg_set() {
        let p = params_from_edid(&caps(false, true, true, 1000.0, 0.0));
        assert_eq!(p.tf_named, Some(TF_HLG));
    }

    #[test]
    fn pq_wins_when_both_pq_and_hlg_advertised() {
        let p = params_from_edid(&caps(true, true, true, 4000.0, 0.0));
        assert_eq!(p.tf_named, Some(TF_ST2084_PQ));
        assert_eq!(p.max_lum, Some(4000));
    }

    #[test]
    fn params_match_ignores_mastering_fields() {
        let mut a = params_from_edid(&caps(true, false, true, 1000.0, 0.0));
        let mut b = a.clone();
        // tf/primaries/max_lum match → match=true even if mastering differs.
        a.mastering_max_lum = Some(1000);
        b.mastering_max_lum = Some(4000);
        assert!(params_match(&a, &b));
        // But primaries change → no match (a surface migrating SDR→HDR).
        let p_sdr = srgb_params();
        assert!(!params_match(&a, &p_sdr));
    }
}
