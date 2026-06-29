#[derive(Debug, Clone)]
pub struct EdidHdrCapabilities {
    pub max_luminance_nits: f32,
    pub min_luminance_nits: f32,
    pub supports_bt2020: bool,
    pub supports_pq: bool,
    pub supports_hlg: bool,
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
}
