use std::sync::Arc;
use x11rb::protocol::randr::ConnectionExt as RandrExt;
use x11rb::protocol::xproto::ConnectionExt;
use x11rb::rust_connection::RustConnection;

#[derive(Debug, Clone)]
pub struct EdidHdrCapabilities {
    pub max_luminance_nits: f32,
    pub min_luminance_nits: f32,
    pub supports_bt2020: bool,
    pub supports_pq: bool,
    pub supports_hlg: bool,
}

pub fn query_edid_hdr(conn: &Arc<RustConnection>, output: u32) -> Option<EdidHdrCapabilities> {
    let edid_atom = conn.intern_atom(false, b"EDID").ok()?.reply().ok()?.atom;

    let prop = conn
        .randr_get_output_property(output, edid_atom, 0u32, 0, 256, false, false)
        .ok()?
        .reply()
        .ok()?;

    if prop.data.len() < 128 {
        return None;
    }

    parse_edid_hdr_from_bytes(&prop.data)
}

fn parse_edid_hdr_from_bytes(edid: &[u8]) -> Option<EdidHdrCapabilities> {
    // Validate EDID header
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

    // Check for extension blocks
    let num_extensions = edid[126] as usize;
    if num_extensions == 0 || edid.len() < 128 + 128 {
        return None;
    }

    // Parse CTA-861 extension blocks
    for ext_idx in 0..num_extensions {
        let offset = 128 + ext_idx * 128;
        if offset + 128 > edid.len() {
            break;
        }

        let ext_tag = edid[offset];
        if ext_tag != 0x02 {
            // Not a CTA-861 extension
            continue;
        }

        let dtd_offset = edid[offset + 2] as usize;
        if dtd_offset < 4 || dtd_offset > 127 {
            continue;
        }

        // Parse data blocks in the CTA extension
        let mut pos = offset + 4;
        while pos < offset + dtd_offset {
            let block_header = edid[pos];
            let block_tag = (block_header >> 5) & 0x07;
            let block_len = (block_header & 0x1F) as usize;

            if pos + 1 + block_len > offset + dtd_offset {
                break;
            }

            if block_tag == 7 && block_len >= 1 {
                // Extended tag block
                let ext_tag_code = edid[pos + 1];
                let block_data = &edid[pos + 2..pos + 1 + block_len];

                match ext_tag_code {
                    // HDR Static Metadata Data Block (tag code 6)
                    6 if block_data.len() >= 2 => {
                        let eotf_bitmap = block_data[0];
                        // Bit 0: Traditional gamma SDR
                        // Bit 1: Traditional gamma HDR
                        // Bit 2: SMPTE ST2084 (PQ)
                        // Bit 3: HLG
                        caps.supports_pq = (eotf_bitmap & 0x04) != 0;
                        caps.supports_hlg = (eotf_bitmap & 0x08) != 0;

                        // Static metadata type 1 content
                        if block_data.len() >= 4 {
                            // Desired Content Max Luminance (byte 3)
                            let max_lum_raw = block_data[2];
                            if max_lum_raw > 0 {
                                // Formula: 50 * 2^(CV/32)
                                caps.max_luminance_nits =
                                    50.0 * 2.0_f32.powf(max_lum_raw as f32 / 32.0);
                            }
                        }
                        if block_data.len() >= 5 {
                            // Desired Content Min Luminance (byte 4)
                            let min_lum_raw = block_data[3];
                            if min_lum_raw > 0 && caps.max_luminance_nits > 0.0 {
                                // Formula: max_lum * (CV/255)^2 / 100
                                let ratio = min_lum_raw as f32 / 255.0;
                                caps.min_luminance_nits =
                                    caps.max_luminance_nits * ratio * ratio / 100.0;
                            }
                        }
                    }
                    // Colorimetry Data Block (tag code 5)
                    5 if block_data.len() >= 2 => {
                        let colorimetry = block_data[0];
                        // Bit 5: BT2020_cYCC
                        // Bit 6: BT2020_YCC
                        // Bit 7: BT2020_RGB
                        caps.supports_bt2020 = (colorimetry & 0xE0) != 0;
                    }
                    _ => {}
                }
            }

            pos += 1 + block_len;
        }
    }

    // Only return if we found any HDR-relevant data
    if caps.supports_pq || caps.supports_hlg || caps.max_luminance_nits > 0.0 {
        Some(caps)
    } else {
        None
    }
}
