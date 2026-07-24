#[derive(Debug, Clone)]
pub struct EdidHdrCapabilities {
    pub max_luminance_nits: f32,
    pub min_luminance_nits: f32,
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

                        if block_data.len() >= 4 {
                            let max_lum_raw = block_data[2];
                            if max_lum_raw > 0 {
                                caps.max_luminance_nits =
                                    50.0 * 2.0_f32.powf(max_lum_raw as f32 / 32.0);
                            }
                        }
                        if block_data.len() >= 5 {
                            let min_lum_raw = block_data[3];
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

    fn build_edid_with_hdr_block(
        eotf: u8,
        max_lum_cv: u8,
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
        edid[hdr_pos + 5] = min_lum_cv;
        edid[hdr_pos + 6] = 0;

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
        let edid = build_edid_with_hdr_block(0x01, 0, 0, 0);
        assert!(parse_edid_hdr_from_bytes(&edid).is_none());
    }

    #[test]
    fn parses_pq_and_bt2020() {
        let edid = build_edid_with_hdr_block(0x04, 0xA0, 0x20, 0x80);
        let caps = parse_edid_hdr_from_bytes(&edid).expect("HDR caps parsed");
        assert!(caps.supports_pq);
        assert!(!caps.supports_hlg);
        assert!(caps.supports_bt2020);
        assert!(caps.max_luminance_nits > 0.0);
        assert!(caps.min_luminance_nits > 0.0);
    }

    #[test]
    fn parses_hlg() {
        let edid = build_edid_with_hdr_block(0x08, 0x80, 0, 0);
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
