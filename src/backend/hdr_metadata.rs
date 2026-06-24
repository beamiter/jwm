use crate::backend::edid::EdidHdrCapabilities;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eotf {
    TraditionalSdr = 0,
    TraditionalHdr = 1,
    Pq = 2,
    Hlg = 3,
}

#[derive(Clone, Copy, Debug)]
pub struct Chromaticity {
    pub r: (u16, u16),
    pub g: (u16, u16),
    pub b: (u16, u16),
    pub w: (u16, u16),
}

pub const PRIMARIES_BT2020: Chromaticity = Chromaticity {
    r: (35400, 14600),
    g: (8500, 39850),
    b: (6550, 2300),
    w: (15635, 16450),
};

pub const PRIMARIES_BT709: Chromaticity = Chromaticity {
    r: (32000, 16500),
    g: (15000, 30000),
    b: (7500, 3000),
    w: (15635, 16450),
};

pub fn pick_eotf(caps: &EdidHdrCapabilities) -> Eotf {
    if caps.supports_pq {
        Eotf::Pq
    } else if caps.supports_hlg {
        Eotf::Hlg
    } else {
        Eotf::TraditionalHdr
    }
}

pub fn pick_primaries(caps: &EdidHdrCapabilities) -> Chromaticity {
    if caps.supports_bt2020 {
        PRIMARIES_BT2020
    } else {
        PRIMARIES_BT709
    }
}

pub fn build_hdr_static_metadata_blob(
    eotf: Eotf,
    primaries: Chromaticity,
    max_display_mastering_nits: u16,
    min_display_mastering_nits_units: u16,
    max_cll: u16,
    max_fall: u16,
) -> [u8; 32] {
    let mut buf = [0u8; 32];
    let metadata_type_u32: u32 = 0;
    buf[0..4].copy_from_slice(&metadata_type_u32.to_ne_bytes());

    buf[4] = eotf as u8;
    buf[5] = 0;

    let mut off = 6;
    for (x, y) in [primaries.r, primaries.g, primaries.b, primaries.w] {
        buf[off..off + 2].copy_from_slice(&x.to_ne_bytes());
        buf[off + 2..off + 4].copy_from_slice(&y.to_ne_bytes());
        off += 4;
    }

    buf[off..off + 2].copy_from_slice(&max_display_mastering_nits.to_ne_bytes());
    off += 2;
    buf[off..off + 2].copy_from_slice(&min_display_mastering_nits_units.to_ne_bytes());
    off += 2;
    buf[off..off + 2].copy_from_slice(&max_cll.to_ne_bytes());
    off += 2;
    buf[off..off + 2].copy_from_slice(&max_fall.to_ne_bytes());

    buf
}

pub fn build_from_edid(caps: &EdidHdrCapabilities, configured_peak_nits: u16) -> [u8; 32] {
    let eotf = pick_eotf(caps);
    let primaries = pick_primaries(caps);
    let max_nits = if caps.max_luminance_nits > 0.0 {
        caps.max_luminance_nits.round() as u16
    } else {
        configured_peak_nits
    };
    let min_lum_units = (caps.min_luminance_nits * 10_000.0).round().max(0.0) as u16;
    build_hdr_static_metadata_blob(eotf, primaries, max_nits, min_lum_units, max_nits, max_nits / 2)
}

pub fn build_sdr_clear_blob() -> [u8; 32] {
    build_hdr_static_metadata_blob(
        Eotf::TraditionalSdr,
        PRIMARIES_BT709,
        0,
        0,
        0,
        0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_is_32_bytes_and_zero_padded() {
        let blob = build_hdr_static_metadata_blob(
            Eotf::Pq,
            PRIMARIES_BT2020,
            1000,
            50,
            1000,
            400,
        );
        assert_eq!(blob.len(), 32);
        assert_eq!(&blob[30..32], &[0, 0], "trailing 2 bytes must be padding");
    }

    #[test]
    fn metadata_type_prefix_is_zero() {
        let blob = build_sdr_clear_blob();
        assert_eq!(&blob[0..4], &[0, 0, 0, 0]);
    }

    #[test]
    fn eotf_byte_at_offset_4() {
        let blob = build_hdr_static_metadata_blob(Eotf::Pq, PRIMARIES_BT709, 0, 0, 0, 0);
        assert_eq!(blob[4], 2, "PQ encodes to 2");
        let blob = build_hdr_static_metadata_blob(Eotf::Hlg, PRIMARIES_BT709, 0, 0, 0, 0);
        assert_eq!(blob[4], 3, "HLG encodes to 3");
        let blob = build_hdr_static_metadata_blob(Eotf::TraditionalSdr, PRIMARIES_BT709, 0, 0, 0, 0);
        assert_eq!(blob[4], 0);
    }

    #[test]
    fn type1_metadata_byte_at_offset_5_is_zero() {
        let blob = build_hdr_static_metadata_blob(Eotf::Pq, PRIMARIES_BT2020, 0, 0, 0, 0);
        assert_eq!(blob[5], 0, "static metadata type 1 -> byte = 0");
    }

    #[test]
    fn primaries_encode_at_offset_6() {
        let blob = build_hdr_static_metadata_blob(
            Eotf::Pq,
            PRIMARIES_BT2020,
            0,
            0,
            0,
            0,
        );
        assert_eq!(u16::from_ne_bytes([blob[6], blob[7]]), 35400);
        assert_eq!(u16::from_ne_bytes([blob[8], blob[9]]), 14600);
        assert_eq!(u16::from_ne_bytes([blob[10], blob[11]]), 8500);
        assert_eq!(u16::from_ne_bytes([blob[12], blob[13]]), 39850);
        assert_eq!(u16::from_ne_bytes([blob[14], blob[15]]), 6550);
        assert_eq!(u16::from_ne_bytes([blob[16], blob[17]]), 2300);
        assert_eq!(u16::from_ne_bytes([blob[18], blob[19]]), 15635);
        assert_eq!(u16::from_ne_bytes([blob[20], blob[21]]), 16450);
    }

    #[test]
    fn luminance_fields_at_correct_offsets() {
        let blob = build_hdr_static_metadata_blob(
            Eotf::Pq,
            PRIMARIES_BT709,
            1000,
            50,
            900,
            400,
        );
        assert_eq!(u16::from_ne_bytes([blob[22], blob[23]]), 1000);
        assert_eq!(u16::from_ne_bytes([blob[24], blob[25]]), 50);
        assert_eq!(u16::from_ne_bytes([blob[26], blob[27]]), 900);
        assert_eq!(u16::from_ne_bytes([blob[28], blob[29]]), 400);
    }

    #[test]
    fn build_from_edid_uses_pq_when_supported() {
        let caps = EdidHdrCapabilities {
            max_luminance_nits: 1000.0,
            min_luminance_nits: 0.005,
            supports_bt2020: true,
            supports_pq: true,
            supports_hlg: false,
        };
        let blob = build_from_edid(&caps, 400);
        assert_eq!(blob[4], 2, "PQ");
        assert_eq!(u16::from_ne_bytes([blob[22], blob[23]]), 1000, "EDID nits override config peak");
        assert_eq!(u16::from_ne_bytes([blob[6], blob[7]]), 35400, "BT2020 R.x");
    }

    #[test]
    fn build_from_edid_falls_back_to_config_peak() {
        let caps = EdidHdrCapabilities {
            max_luminance_nits: 0.0,
            min_luminance_nits: 0.0,
            supports_bt2020: false,
            supports_pq: false,
            supports_hlg: true,
        };
        let blob = build_from_edid(&caps, 600);
        assert_eq!(blob[4], 3, "HLG");
        assert_eq!(u16::from_ne_bytes([blob[22], blob[23]]), 600, "config peak used");
        assert_eq!(u16::from_ne_bytes([blob[6], blob[7]]), 32000, "BT709 R.x");
    }
}
