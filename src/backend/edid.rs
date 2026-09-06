#[derive(Debug, Clone)]
pub struct EdidHdrCapabilities {
    pub max_luminance_nits: f32,
    pub min_luminance_nits: f32,
    /// Desired Content Max Frame-average Luminance, in cd/m².
    ///
    /// `0.0` means the display did not state one. CTA-861.3 defines a zero
    /// code value in that byte as "not indicated", and a short block that
    /// stops before the byte says the same thing, so consumers must treat
    /// `0.0` as *unknown* rather than as a claim of zero nits — see
    /// [`crate::backend::hdr_metadata::build_from_edid`], which forwards the
    /// unknown to the sink instead of inventing a value.
    pub max_frame_average_nits: f32,
    pub supports_bt2020: bool,
    pub supports_pq: bool,
    pub supports_hlg: bool,
}

/// Compositor colour settings derived from a display's HDR EDID block.
///
/// `None` fields mean "leave unchanged"; a present value is the setting to
/// apply. Deriving this is pure and identical across the X11 transports, so
/// both feed their fetched [`EdidHdrCapabilities`] through
/// [`hdr_compositor_plan`] and hand the result to the shared compositor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HdrCompositorPlan {
    /// Peak luminance in nits, when the display advertises a positive value.
    pub peak_nits: Option<f32>,
    /// EOTF mode: `1` for PQ, `2` for HLG; `None` keeps the SDR EOTF.
    pub eotf_mode: Option<i32>,
    /// Output colour space: `1` for BT.2020 when supported.
    pub colorspace: Option<i32>,
    /// Whether to drive a 10-bit output; set whenever HDR metadata exists.
    pub output_10bit: bool,
}

/// Map EDID HDR capabilities to the compositor colour settings to apply.
///
/// PQ takes precedence over HLG when a display claims both, matching how the
/// two X11 backends previously open-coded this decision.
#[must_use]
pub fn hdr_compositor_plan(caps: &EdidHdrCapabilities) -> HdrCompositorPlan {
    let eotf_mode = if caps.supports_pq {
        Some(1)
    } else if caps.supports_hlg {
        Some(2)
    } else {
        None
    };
    HdrCompositorPlan {
        peak_nits: (caps.max_luminance_nits > 0.0).then_some(caps.max_luminance_nits),
        eotf_mode,
        colorspace: caps.supports_bt2020.then_some(1),
        output_10bit: true,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdidIdentity {
    pub vendor: String,
    pub product_code: u16,
    pub serial_number: u32,
    pub monitor_name: Option<String>,
    pub monitor_serial: Option<String>,
}

pub fn parse_edid_identity_from_bytes(edid: &[u8]) -> Option<EdidIdentity> {
    if edid.len() < 128 {
        return None;
    }
    let header = &edid[0..8];
    if header != [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00] {
        return None;
    }

    let vendor_raw = u16::from_be_bytes([edid[8], edid[9]]);
    let vendor = [
        (((vendor_raw >> 10) & 0x1F) as u8 + b'A' - 1) as char,
        (((vendor_raw >> 5) & 0x1F) as u8 + b'A' - 1) as char,
        ((vendor_raw & 0x1F) as u8 + b'A' - 1) as char,
    ]
    .iter()
    .collect::<String>();

    let product_code = u16::from_le_bytes([edid[10], edid[11]]);
    let serial_number = u32::from_le_bytes([edid[12], edid[13], edid[14], edid[15]]);
    let mut monitor_name = None;
    let mut monitor_serial = None;

    for descriptor in edid[54..126].chunks_exact(18) {
        if descriptor[0..3] != [0, 0, 0] {
            continue;
        }
        let text = parse_descriptor_text(&descriptor[5..18]);
        match descriptor[3] {
            0xFC => monitor_name = text,
            0xFF => monitor_serial = text,
            _ => {}
        }
    }

    Some(EdidIdentity {
        vendor,
        product_code,
        serial_number,
        monitor_name,
        monitor_serial,
    })
}

fn parse_descriptor_text(bytes: &[u8]) -> Option<String> {
    let end = bytes
        .iter()
        .position(|b| *b == b'\n' || *b == b'\r' || *b == 0)
        .unwrap_or(bytes.len());
    let text = bytes[..end]
        .iter()
        .map(|b| {
            if b.is_ascii_graphic() || *b == b' ' {
                *b
            } else {
                b' '
            }
        })
        .collect::<Vec<_>>();
    let text = String::from_utf8_lossy(&text).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

pub fn parse_edid_hdr_from_bytes(edid: &[u8]) -> Option<EdidHdrCapabilities> {
    if edid.len() < 128 {
        return None;
    }
    let header = &edid[0..8];
    if header != [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00] {
        return None;
    }

    let mut caps = EdidHdrCapabilities {
        max_luminance_nits: 0.0,
        min_luminance_nits: 0.0,
        max_frame_average_nits: 0.0,
        supports_bt2020: false,
        supports_pq: false,
        supports_hlg: false,
    };

    let num_extensions = edid[126] as usize;
    if num_extensions == 0 || edid.len() < 128 + 128 {
        return None;
    }

    for ext_idx in 0..num_extensions {
        let offset = 128 + ext_idx * 128;
        if offset + 128 > edid.len() {
            break;
        }

        let ext_tag = edid[offset];
        if ext_tag != 0x02 {
            continue;
        }

        let dtd_offset = edid[offset + 2] as usize;
        if dtd_offset < 4 || dtd_offset > 127 {
            continue;
        }

        let mut pos = offset + 4;
        while pos < offset + dtd_offset {
            let block_header = edid[pos];
            let block_tag = (block_header >> 5) & 0x07;
            let block_len = (block_header & 0x1F) as usize;

            if pos + 1 + block_len > offset + dtd_offset {
                break;
            }

            if block_tag == 7 && block_len >= 1 {
                let ext_tag_code = edid[pos + 1];
                let block_data = &edid[pos + 2..pos + 1 + block_len];

                match ext_tag_code {
                    6 if block_data.len() >= 2 => {
                        let eotf_bitmap = block_data[0];
                        caps.supports_pq = (eotf_bitmap & 0x04) != 0;
                        caps.supports_hlg = (eotf_bitmap & 0x08) != 0;

                        // CTA-861.3 HDR Static Metadata Data Block, counting
                        // from the EOTF byte that `block_data` starts at:
                        // [0] EOTF bitmap, [1] static metadata descriptors,
                        // [2] Desired Content Max Luminance, [3] Desired
                        // Content Max Frame-average Luminance, [4] Desired
                        // Content Min Luminance. (The kernel's
                        // `drm_parse_hdr_metadata_block` counts from the block
                        // header instead: db[4], db[5], db[6].)
                        if block_data.len() >= 3 {
                            let max_lum_raw = block_data[2];
                            if max_lum_raw > 0 {
                                caps.max_luminance_nits =
                                    50.0 * 2.0_f32.powf(max_lum_raw as f32 / 32.0);
                            }
                        }
                        // [3]: Desired Content Max Frame-average Luminance,
                        // encoded like the peak. A zero code value — or a
                        // block that stops before this byte — means "not
                        // indicated", and stays 0.0 so the metadata builder
                        // reports unknown instead of fabricating a value.
                        if block_data.len() >= 4 {
                            let max_fall_raw = block_data[3];
                            if max_fall_raw > 0 {
                                caps.max_frame_average_nits =
                                    50.0 * 2.0_f32.powf(max_fall_raw as f32 / 32.0);
                            }
                        }
                        // [4], not [3]: reading the max-frame-average byte
                        // through the min-luminance formula made a typical
                        // panel report black at ~1.5 cd/m² instead of ~0.13,
                        // and that value is what the HDR_OUTPUT_METADATA blob
                        // and the client-facing `min_lum` are built from.
                        if block_data.len() >= 5 {
                            let min_lum_raw = block_data[4];
                            if min_lum_raw > 0 && caps.max_luminance_nits > 0.0 {
                                let ratio = min_lum_raw as f32 / 255.0;
                                caps.min_luminance_nits =
                                    caps.max_luminance_nits * ratio * ratio / 100.0;
                            }
                        }
                    }
                    5 if block_data.len() >= 2 => {
                        let colorimetry = block_data[0];
                        caps.supports_bt2020 = (colorimetry & 0xE0) != 0;
                    }
                    _ => {}
                }
            }

            pos += 1 + block_len;
        }
    }

    if caps.supports_pq || caps.supports_hlg || caps.max_luminance_nits > 0.0 {
        Some(caps)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A CTA extension carrying a colorimetry block and an HDR static
    /// metadata block, laid out as CTA-861.3 specifies it: EOTF, static
    /// metadata descriptors, max luminance, max frame-average luminance, min
    /// luminance. The fixture used to put the min-luminance code value where
    /// max-frame-average belongs, mirroring the parser's own off-by-one, so
    /// the parity test could not see it.
    fn build_edid_with_hdr_block(
        eotf: u8,
        max_lum_cv: u8,
        max_fall_cv: u8,
        min_lum_cv: u8,
        colorimetry: u8,
    ) -> Vec<u8> {
        let mut edid = vec![0u8; 256];
        edid[0..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        edid[126] = 1;

        let cta = 128;
        edid[cta] = 0x02;
        edid[cta + 1] = 3;
        let dtd_offset_pos = cta + 4;
        let colorimetry_block_pos = dtd_offset_pos;
        edid[colorimetry_block_pos] = (7 << 5) | 3;
        edid[colorimetry_block_pos + 1] = 5;
        edid[colorimetry_block_pos + 2] = colorimetry;
        edid[colorimetry_block_pos + 3] = 0;

        let hdr_pos = colorimetry_block_pos + 4;
        edid[hdr_pos] = (7 << 5) | 6;
        edid[hdr_pos + 1] = 6;
        edid[hdr_pos + 2] = eotf;
        edid[hdr_pos + 3] = 0;
        edid[hdr_pos + 4] = max_lum_cv;
        edid[hdr_pos + 5] = max_fall_cv;
        edid[hdr_pos + 6] = min_lum_cv;

        edid[cta + 2] = (hdr_pos + 7 - cta) as u8;
        edid
    }

    fn encode_vendor(vendor: &str) -> [u8; 2] {
        let bytes = vendor.as_bytes();
        let raw = (((bytes[0] - b'A' + 1) as u16) << 10)
            | (((bytes[1] - b'A' + 1) as u16) << 5)
            | ((bytes[2] - b'A' + 1) as u16);
        raw.to_be_bytes()
    }

    #[test]
    fn rejects_short_input() {
        assert!(parse_edid_hdr_from_bytes(&[]).is_none());
        assert!(parse_edid_hdr_from_bytes(&[0u8; 64]).is_none());
    }

    #[test]
    fn rejects_bad_header() {
        let mut edid = vec![0u8; 256];
        edid[0..8].copy_from_slice(&[0xAA; 8]);
        edid[126] = 1;
        assert!(parse_edid_hdr_from_bytes(&edid).is_none());
    }

    #[test]
    fn returns_none_for_sdr_only_edid() {
        let edid = build_edid_with_hdr_block(0x01, 0, 0, 0, 0);
        assert!(parse_edid_hdr_from_bytes(&edid).is_none());
    }

    #[test]
    fn parses_pq_and_bt2020() {
        let edid = build_edid_with_hdr_block(0x04, 0xA0, 0x70, 0x20, 0x80);
        let caps = parse_edid_hdr_from_bytes(&edid).expect("HDR caps parsed");
        assert!(caps.supports_pq);
        assert!(!caps.supports_hlg);
        assert!(caps.supports_bt2020);
        assert!(caps.max_luminance_nits > 0.0);
        assert!(caps.min_luminance_nits > 0.0);
    }

    #[test]
    fn min_luminance_comes_from_the_min_luminance_byte_not_the_frame_average_one() {
        // A typical panel: max cv 0x80 -> 50 * 2^(128/32) = 800 cd/m², a
        // max-frame-average cv of 0x70, and a min cv of 0x20. The min formula
        // is max * (cv/255)² / 100, so the two candidate bytes give answers an
        // order of magnitude apart — which is what a sink's tone mapping sees.
        let edid = build_edid_with_hdr_block(0x04, 0x80, 0x70, 0x20, 0x80);
        let caps = parse_edid_hdr_from_bytes(&edid).expect("HDR caps parsed");
        assert!((caps.max_luminance_nits - 800.0).abs() < 0.01);

        let expected = 800.0_f32 * (32.0 / 255.0) * (32.0 / 255.0) / 100.0;
        let from_the_wrong_byte = 800.0_f32 * (112.0 / 255.0) * (112.0 / 255.0) / 100.0;
        assert!(
            (caps.min_luminance_nits - expected).abs() < 1e-4,
            "expected {expected}, got {}",
            caps.min_luminance_nits
        );
        assert!(
            (caps.min_luminance_nits - from_the_wrong_byte).abs() > 1e-3,
            "the max-frame-average byte must not be read as the minimum"
        );
    }

    #[test]
    fn all_three_luminance_values_decode_from_their_own_bytes() {
        // A real-shaped block: peak cv 0x80, max-frame-average cv 0x70, min
        // cv 0x20. The three answers are far enough apart that reading any
        // one of them out of a neighbour's byte is visible here.
        let edid = build_edid_with_hdr_block(0x04, 0x80, 0x70, 0x20, 0x80);
        let caps = parse_edid_hdr_from_bytes(&edid).expect("HDR caps parsed");

        let expected_peak = 800.0_f32; // 50 * 2^(128/32)
        let expected_fall = 50.0_f32 * 2.0_f32.powf(112.0 / 32.0); // ~565.7
        let expected_min = expected_peak * (32.0 / 255.0) * (32.0 / 255.0) / 100.0;

        assert!((caps.max_luminance_nits - expected_peak).abs() < 0.01);
        assert!(
            (caps.max_frame_average_nits - expected_fall).abs() < 0.01,
            "expected {expected_fall}, got {}",
            caps.max_frame_average_nits
        );
        assert!((caps.min_luminance_nits - expected_min).abs() < 1e-4);

        // The frame average is its own byte, not a share of the peak and not
        // the byte on either side of it.
        assert!(
            (caps.max_frame_average_nits - expected_peak / 2.0).abs() > 1.0,
            "the frame average must not be derived from the peak"
        );
        assert!((caps.max_frame_average_nits - expected_peak).abs() > 1.0);
        let from_the_min_byte = 50.0_f32 * 2.0_f32.powf(32.0 / 32.0); // 100
        assert!((caps.max_frame_average_nits - from_the_min_byte).abs() > 1.0);
    }

    #[test]
    fn a_zero_frame_average_code_value_is_unspecified() {
        // CTA-861.3: code value 0 means "not indicated". It must not become a
        // claim that the panel averages 0 nits, and it must not be filled in
        // from the peak either.
        let edid = build_edid_with_hdr_block(0x04, 0x80, 0x00, 0x20, 0x80);
        let caps = parse_edid_hdr_from_bytes(&edid).expect("HDR caps parsed");
        assert!(
            (caps.max_luminance_nits - 800.0).abs() < 0.01,
            "block parsed"
        );
        assert_eq!(
            caps.max_frame_average_nits, 0.0,
            "an unstated frame average stays unstated"
        );
    }

    #[test]
    fn the_metadata_blob_carries_the_frame_average_the_edid_stated() {
        // End to end: EDID bytes -> caps -> HDR_OUTPUT_METADATA payload. The
        // MaxFALL field at offset 28 is what a real sink tone-maps against,
        // and it used to be half the peak regardless of what the panel said.
        let edid = build_edid_with_hdr_block(0x04, 0x80, 0x70, 0x20, 0x80);
        let caps = parse_edid_hdr_from_bytes(&edid).expect("HDR caps parsed");
        let blob = crate::backend::hdr_metadata::build_from_edid(&caps, 400);

        let expected = caps.max_frame_average_nits.round() as u16;
        assert_eq!(u16::from_ne_bytes([blob[28], blob[29]]), expected);
        assert!(
            (560..=570).contains(&expected),
            "cv 0x70 decodes to ~566 nits, got {expected}"
        );
        assert_eq!(
            u16::from_ne_bytes([blob[22], blob[23]]),
            800,
            "peak from the EDID, not the configured fallback"
        );
        assert_ne!(
            u16::from_ne_bytes([blob[28], blob[29]]),
            400,
            "half the peak is not the frame average"
        );
    }

    #[test]
    fn a_block_that_stops_before_the_minimum_still_yields_its_peak() {
        // CTA-861.3 lets the block end early. The peak sits at payload index
        // 2, so a three-byte payload carries it; the old guard demanded four
        // and silently dropped the peak of a short block, and the min guard
        // has to keep demanding five because index 4 is the byte it reads.
        let mut edid = build_edid_with_hdr_block(0x04, 0x80, 0x70, 0x20, 0x80);
        let hdr_pos = 128 + 4 + 4;
        edid[hdr_pos] = (7 << 5) | 4;
        let caps = parse_edid_hdr_from_bytes(&edid).expect("HDR caps parsed");
        assert!((caps.max_luminance_nits - 800.0).abs() < 0.01);
        assert_eq!(caps.min_luminance_nits, 0.0);
        assert_eq!(
            caps.max_frame_average_nits, 0.0,
            "payload index 3 is absent from a three-byte payload"
        );
    }

    #[test]
    fn parses_hlg() {
        let edid = build_edid_with_hdr_block(0x08, 0x80, 0, 0, 0);
        let caps = parse_edid_hdr_from_bytes(&edid).expect("HDR caps parsed");
        assert!(!caps.supports_pq);
        assert!(caps.supports_hlg);
    }

    #[test]
    fn parses_identity_from_base_block() {
        let mut edid = vec![0u8; 128];
        edid[0..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        edid[8..10].copy_from_slice(&encode_vendor("JWM"));
        edid[10..12].copy_from_slice(&0x1234u16.to_le_bytes());
        edid[12..16].copy_from_slice(&0xAABBCCDDu32.to_le_bytes());
        edid[54..72].copy_from_slice(&[
            0x00, 0x00, 0x00, 0xFC, 0x00, b'J', b'W', b'M', b' ', b'D', b'i', b's', b'p', b'l',
            b'a', b'y', b'\n', b' ',
        ]);
        edid[72..90].copy_from_slice(&[
            0x00, 0x00, 0x00, 0xFF, 0x00, b'S', b'E', b'R', b'1', b'2', b'3', b'\n', b' ', b' ',
            b' ', b' ', b' ', b' ',
        ]);

        let identity = parse_edid_identity_from_bytes(&edid).expect("identity parsed");
        assert_eq!(identity.vendor, "JWM");
        assert_eq!(identity.product_code, 0x1234);
        assert_eq!(identity.serial_number, 0xAABBCCDD);
        assert_eq!(identity.monitor_name.as_deref(), Some("JWM Display"));
        assert_eq!(identity.monitor_serial.as_deref(), Some("SER123"));
    }

    fn caps(max: f32, pq: bool, hlg: bool, bt2020: bool) -> EdidHdrCapabilities {
        EdidHdrCapabilities {
            max_luminance_nits: max,
            min_luminance_nits: 0.1,
            max_frame_average_nits: 0.0,
            supports_bt2020: bt2020,
            supports_pq: pq,
            supports_hlg: hlg,
        }
    }

    #[test]
    fn hdr_plan_selects_pq_over_hlg_and_sets_bt2020_and_peak() {
        let plan = hdr_compositor_plan(&caps(1000.0, true, true, true));
        assert_eq!(plan.peak_nits, Some(1000.0));
        assert_eq!(
            plan.eotf_mode,
            Some(1),
            "PQ wins when both PQ and HLG exist"
        );
        assert_eq!(plan.colorspace, Some(1));
        assert!(plan.output_10bit);
    }

    #[test]
    fn hdr_plan_falls_back_to_hlg_and_omits_bt2020() {
        let plan = hdr_compositor_plan(&caps(600.0, false, true, false));
        assert_eq!(plan.eotf_mode, Some(2));
        assert_eq!(plan.colorspace, None);
        assert_eq!(plan.peak_nits, Some(600.0));
    }

    #[test]
    fn hdr_plan_keeps_sdr_eotf_and_drops_zero_peak() {
        let plan = hdr_compositor_plan(&caps(0.0, false, false, false));
        assert_eq!(plan.eotf_mode, None, "no PQ/HLG keeps the SDR EOTF");
        assert_eq!(plan.peak_nits, None, "a zero peak is left unset");
        assert_eq!(plan.colorspace, None);
        // 10-bit output is still requested whenever HDR metadata was present.
        assert!(plan.output_10bit);
    }
}
