//! Minimal ICC v2/v4 display-profile parser for wp-color-management.
//!
//! Reads the ICC tags `wtpt`, `rXYZ`, `gXYZ`, `bXYZ`, and one of `rTRC`/`gTRC`/`bTRC`,
//! and converts them into chromaticity + transfer-function values matching the
//! wp-color-management-v1 parametric encoding (xy * 1_000_000, named TF enum value).
//!
//! Supported profile shape (per the protocol's `unsupported` clause):
//!   - ICC version 2.x or 4.x
//!   - device class `mntr` (display) or `spac` (color space)
//!   - color space `RGB `
//!   - 3-channel: must have rXYZ + gXYZ + bXYZ + wtpt
//!
//! Limitations (documented intentionally):
//!   - The XYZ values in `rXYZ`/`gXYZ`/`bXYZ` are PCS-encoded (D50-adapted in v4),
//!     not the actual display white. We use them as-is rather than chromatically
//!     adapting back to the media white — accuracy is sufficient to communicate the
//!     gamut shape but the absolute primaries shift slightly for D65 displays. A
//!     future pass could apply Bradford CAT⁻¹ if profile accuracy matters.
//!   - Transfer curve recognition: parametricCurve type 0 (pure power), curve
//!     count=0 (identity), curve count=1 (u8Fixed8 gamma). Other curve types map
//!     to `tf_power` extracted via a midpoint sample; if extraction fails the
//!     curve is rejected as unsupported.
//!   - We pick `rTRC` only — most well-formed display profiles share rTRC=gTRC=bTRC.

use crate::backend::wayland_udev::color_management::ParametricParams;

/// Maximum ICC payload accepted via wp_image_description_creator_icc_v1.set_icc_file.
/// The protocol caps it at 32 MiB; we use the same number here for parser bounds.
pub const ICC_MAX_BYTES: u32 = 32 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum IccError {
    /// Profile shorter than 128-byte header.
    TooShort,
    /// Missing the 'acsp' signature at byte 36.
    BadMagic,
    /// Profile version outside 2.x/4.x.
    UnsupportedVersion,
    /// Device class is not `mntr` or `spac`.
    UnsupportedClass,
    /// Color space is not `RGB `.
    UnsupportedColorSpace,
    /// One of wtpt/rXYZ/gXYZ/bXYZ is missing or has the wrong tag type.
    MissingRequiredTag,
    /// Tag offset/size points outside the profile buffer.
    TagOutOfBounds,
    /// rTRC/gTRC/bTRC could not be mapped to either a named TF or a power.
    UnsupportedTransferFunction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IccParsed {
    /// r_x, r_y, g_x, g_y, b_x, b_y, w_x, w_y (each * 1_000_000).
    pub primaries: [i32; 8],
    /// One of `TransferFunction::Gamma22 as u32` / `Bt1886` / `ExtLinear` when
    /// the curve matches a named TF within tolerance.
    pub tf_named: Option<u32>,
    /// Encoded gamma * 10_000 (matches wp_image_description_creator_params_v1::SetTfPower).
    pub tf_power: Option<u32>,
}

impl IccParsed {
    /// Convert into ParametricParams the way wp-color-management consumes them.
    /// Always sets `primaries` (explicit chromaticities); sets one of `tf_named`
    /// or `tf_power`.
    pub fn into_params(self) -> ParametricParams {
        let mut p = ParametricParams {
            primaries: Some(self.primaries),
            ..ParametricParams::default()
        };
        // Cosmetic: if the parsed white-point sits in the sRGB D65 ballpark we
        // tag the named primaries as Srgb so info-event listeners get something
        // human-readable. Bt2020 would be detected the same way but is rare in
        // ICC display profiles (HDR clients use parametric, not ICC).
        const SRGB_D65_W_X: i32 = 312_700;
        const SRGB_D65_W_Y: i32 = 329_000;
        const TOL: i32 = 10_000; // ±0.01 in xy
        if (self.primaries[6] - SRGB_D65_W_X).abs() <= TOL
            && (self.primaries[7] - SRGB_D65_W_Y).abs() <= TOL
            && (self.primaries[0] - 640_000).abs() <= 30_000
            && (self.primaries[2] - 300_000).abs() <= 30_000
        {
            p.primaries_named = Some(1 /* Primaries::Srgb */);
        }
        p.tf_named = self.tf_named;
        p.tf_power = self.tf_power;
        p
    }
}

/// Parse the ICC bytes (already extracted from the offset+length in the
/// set_icc_file request) into IccParsed, or return a structured error.
pub fn parse_icc(bytes: &[u8]) -> Result<IccParsed, IccError> {
    if bytes.len() < 128 {
        return Err(IccError::TooShort);
    }
    if &bytes[36..40] != b"acsp" {
        return Err(IccError::BadMagic);
    }
    // Profile version bytes 8-11 are BCD: byte 8 = major (0x02 for v2.x,
    // 0x04 for v4.x), byte 9 = minor.bug-fix nibbles. The protocol restricts
    // us to versions 2 and 4.
    if !matches!(bytes[8], 0x02 | 0x04) {
        return Err(IccError::UnsupportedVersion);
    }
    let class = &bytes[12..16];
    if class != b"mntr" && class != b"spac" {
        return Err(IccError::UnsupportedClass);
    }
    if &bytes[16..20] != b"RGB " {
        return Err(IccError::UnsupportedColorSpace);
    }

    // Tag table starts at byte 128: 4-byte count, then 12-byte entries.
    let tag_count = read_u32(bytes, 128).ok_or(IccError::TooShort)? as usize;
    let table_end = 128usize
        .checked_add(4 + tag_count * 12)
        .ok_or(IccError::TooShort)?;
    if bytes.len() < table_end {
        return Err(IccError::TooShort);
    }

    let find_tag = |sig: &[u8; 4]| -> Option<(usize, usize)> {
        for i in 0..tag_count {
            let entry = 132 + i * 12;
            if &bytes[entry..entry + 4] == sig {
                let off = read_u32(bytes, entry + 4)? as usize;
                let len = read_u32(bytes, entry + 8)? as usize;
                return Some((off, len));
            }
        }
        None
    };

    // White point + per-channel colorants are mandatory; rTRC is mandatory.
    let wtpt = find_tag(b"wtpt").ok_or(IccError::MissingRequiredTag)?;
    let r_xyz = find_tag(b"rXYZ").ok_or(IccError::MissingRequiredTag)?;
    let g_xyz = find_tag(b"gXYZ").ok_or(IccError::MissingRequiredTag)?;
    let b_xyz = find_tag(b"bXYZ").ok_or(IccError::MissingRequiredTag)?;
    let trc = find_tag(b"rTRC")
        .or_else(|| find_tag(b"gTRC"))
        .or_else(|| find_tag(b"bTRC"))
        .ok_or(IccError::MissingRequiredTag)?;

    let w = read_xyz_tag(bytes, wtpt)?;
    let r = read_xyz_tag(bytes, r_xyz)?;
    let g = read_xyz_tag(bytes, g_xyz)?;
    let b = read_xyz_tag(bytes, b_xyz)?;
    let (rx, ry) = xyz_to_xy(r);
    let (gx, gy) = xyz_to_xy(g);
    let (bx, by) = xyz_to_xy(b);
    let (wx, wy) = xyz_to_xy(w);

    let (tf_named, tf_power) = read_trc_tag(bytes, trc)?;

    Ok(IccParsed {
        primaries: [
            scale_xy(rx),
            scale_xy(ry),
            scale_xy(gx),
            scale_xy(gy),
            scale_xy(bx),
            scale_xy(by),
            scale_xy(wx),
            scale_xy(wy),
        ],
        tf_named,
        tf_power,
    })
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    bytes
        .get(offset..offset + 4)
        .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

fn read_s15fixed16(bytes: &[u8], offset: usize) -> Option<f64> {
    let raw = bytes.get(offset..offset + 4)?;
    let i = i32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
    Some(i as f64 / 65536.0)
}

fn read_xyz_tag(bytes: &[u8], (off, len): (usize, usize)) -> Result<(f64, f64, f64), IccError> {
    if len < 20 || off.checked_add(len).map_or(true, |e| e > bytes.len()) {
        return Err(IccError::TagOutOfBounds);
    }
    let typ = &bytes[off..off + 4];
    if typ != b"XYZ " {
        return Err(IccError::MissingRequiredTag);
    }
    let x = read_s15fixed16(bytes, off + 8).ok_or(IccError::TagOutOfBounds)?;
    let y = read_s15fixed16(bytes, off + 12).ok_or(IccError::TagOutOfBounds)?;
    let z = read_s15fixed16(bytes, off + 16).ok_or(IccError::TagOutOfBounds)?;
    Ok((x, y, z))
}

fn xyz_to_xy((x, y, z): (f64, f64, f64)) -> (f64, f64) {
    let sum = x + y + z;
    if sum.abs() < 1e-9 {
        return (0.0, 0.0);
    }
    (x / sum, y / sum)
}

fn scale_xy(v: f64) -> i32 {
    (v * 1_000_000.0).round() as i32
}

/// Map an rTRC/gTRC/bTRC curve into either a named TF (Bt1886/Gamma22/ExtLinear)
/// or a tf_power gamma (× 10_000). Returns (tf_named, tf_power) — exactly one Some.
fn read_trc_tag(
    bytes: &[u8],
    (off, len): (usize, usize),
) -> Result<(Option<u32>, Option<u32>), IccError> {
    if len < 12 || off.checked_add(len).map_or(true, |e| e > bytes.len()) {
        return Err(IccError::TagOutOfBounds);
    }
    let typ = &bytes[off..off + 4];
    match typ {
        b"curv" => {
            let count = read_u32(bytes, off + 8).ok_or(IccError::TagOutOfBounds)? as usize;
            if count == 0 {
                // Identity — linear.
                return Ok((Some(5 /* TF::ExtLinear */), None));
            }
            if count == 1 {
                // Single u8Fixed8 gamma (u16 value, divided by 256).
                if len < 14 {
                    return Err(IccError::TagOutOfBounds);
                }
                let raw = u16::from_be_bytes([bytes[off + 12], bytes[off + 13]]);
                let gamma = raw as f64 / 256.0;
                return Ok(classify_gamma(gamma));
            }
            // LUT — sample at the midpoint (x=0.5) and infer the equivalent
            // power: y = 0.5^g  ⇒  g = log(y) / log(0.5) = -log2(y).
            // 16-bit entries, big-endian, normalized so 0xFFFF = 1.0.
            let entries_offset = off + 12;
            let entries_bytes = 2usize.checked_mul(count).ok_or(IccError::TagOutOfBounds)?;
            if entries_offset
                .checked_add(entries_bytes)
                .map_or(true, |e| e > bytes.len())
            {
                return Err(IccError::TagOutOfBounds);
            }
            let mid_idx = count / 2;
            let raw = u16::from_be_bytes([
                bytes[entries_offset + mid_idx * 2],
                bytes[entries_offset + mid_idx * 2 + 1],
            ]);
            let y = raw as f64 / 65535.0;
            if y <= 0.0 || y >= 1.0 {
                return Err(IccError::UnsupportedTransferFunction);
            }
            let g = -(y.ln() / std::f64::consts::LN_2);
            Ok(classify_gamma(g))
        }
        b"para" => {
            // function type (u16) at off+8.
            if len < 16 {
                return Err(IccError::TagOutOfBounds);
            }
            let func_type = u16::from_be_bytes([bytes[off + 8], bytes[off + 9]]);
            // Parameters live at off+12 onwards, each s15Fixed16.
            // Type 0 — single parameter g (pure power).
            // Types 1..4 — sRGB-like. Approximate as gamma at midpoint using the
            // overall power parameter g (parameter 0).
            let g = read_s15fixed16(bytes, off + 12).ok_or(IccError::TagOutOfBounds)?;
            if g <= 0.0 || g > 5.0 {
                return Err(IccError::UnsupportedTransferFunction);
            }
            if func_type == 0 {
                Ok(classify_gamma(g))
            } else {
                // For types 1+ the curve isn't a pure power, but g is still the
                // dominant exponent in the high region. Apps that consume our
                // descriptor (e.g. a future render-path inverse-EOTF) treat the
                // value as a single power anyway, so we expose it as tf_power.
                Ok((None, Some((g * 10_000.0).round() as u32)))
            }
        }
        _ => Err(IccError::UnsupportedTransferFunction),
    }
}

/// Decide whether a numerical gamma matches a named TF (Bt1886≈2.4, Gamma22≈2.2)
/// or should be exposed verbatim as tf_power.
fn classify_gamma(g: f64) -> (Option<u32>, Option<u32>) {
    const NAMED_TOL: f64 = 0.05;
    if (g - 1.0).abs() <= 0.01 {
        return (Some(5 /* TF::ExtLinear */), None);
    }
    if (g - 2.2).abs() <= NAMED_TOL {
        return (Some(2 /* TF::Gamma22 */), None);
    }
    if (g - 2.4).abs() <= NAMED_TOL {
        return (Some(1 /* TF::Bt1886 */), None);
    }
    (None, Some((g * 10_000.0).round() as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-build a minimal v4 RGB display profile with the four mandatory tags
    /// plus rTRC, so the parser has something concrete to chew on.
    fn build_min_profile(
        r_xyz: (f64, f64, f64),
        g_xyz: (f64, f64, f64),
        b_xyz: (f64, f64, f64),
        w_xyz: (f64, f64, f64),
        gamma: f64,
    ) -> Vec<u8> {
        // 128 header + 4 tag-count + 5 * 12 tag entries + 5 * 20 (XYZ) + 14 (curve)
        // = 332 bytes. Pad up.
        let mut buf = vec![0u8; 332];
        // Header: version 4.3 (BCD 0x04300000 in bytes 8..12).
        buf[8] = 0x04;
        // Class 'mntr'
        buf[12..16].copy_from_slice(b"mntr");
        // Color space 'RGB '
        buf[16..20].copy_from_slice(b"RGB ");
        // PCS 'XYZ '
        buf[20..24].copy_from_slice(b"XYZ ");
        // Magic 'acsp'
        buf[36..40].copy_from_slice(b"acsp");

        let tag_count: u32 = 5;
        buf[128..132].copy_from_slice(&tag_count.to_be_bytes());

        // Layout XYZ tags starting at byte 192. Each XYZ block is 20 bytes;
        // curve block is 14 bytes (8 header + 4 count + 2 value).
        let xyz_size = 20u32;
        let curve_size = 14u32;
        let off_w: u32 = 192;
        let off_r = off_w + xyz_size;
        let off_g = off_r + xyz_size;
        let off_b = off_g + xyz_size;
        let off_trc = off_b + xyz_size;

        let mut put_entry = |slot: usize, sig: &[u8; 4], off: u32, size: u32| {
            let entry = 132 + slot * 12;
            buf[entry..entry + 4].copy_from_slice(sig);
            buf[entry + 4..entry + 8].copy_from_slice(&off.to_be_bytes());
            buf[entry + 8..entry + 12].copy_from_slice(&size.to_be_bytes());
        };
        put_entry(0, b"wtpt", off_w, xyz_size);
        put_entry(1, b"rXYZ", off_r, xyz_size);
        put_entry(2, b"gXYZ", off_g, xyz_size);
        put_entry(3, b"bXYZ", off_b, xyz_size);
        put_entry(4, b"rTRC", off_trc, curve_size);

        fn write_xyz(buf: &mut [u8], off: u32, xyz: (f64, f64, f64)) {
            let o = off as usize;
            buf[o..o + 4].copy_from_slice(b"XYZ ");
            // bytes o+4..o+8 are reserved (already zero).
            let to_s1516 = |v: f64| (v * 65536.0).round() as i32;
            buf[o + 8..o + 12].copy_from_slice(&to_s1516(xyz.0).to_be_bytes());
            buf[o + 12..o + 16].copy_from_slice(&to_s1516(xyz.1).to_be_bytes());
            buf[o + 16..o + 20].copy_from_slice(&to_s1516(xyz.2).to_be_bytes());
        }
        write_xyz(&mut buf, off_w, w_xyz);
        write_xyz(&mut buf, off_r, r_xyz);
        write_xyz(&mut buf, off_g, g_xyz);
        write_xyz(&mut buf, off_b, b_xyz);

        // curv tag with count=1, gamma encoded as u8Fixed8 (gamma * 256).
        let o = off_trc as usize;
        buf[o..o + 4].copy_from_slice(b"curv");
        buf[o + 8..o + 12].copy_from_slice(&1u32.to_be_bytes());
        let g = (gamma * 256.0).round() as u16;
        buf[o + 12..o + 14].copy_from_slice(&g.to_be_bytes());

        buf
    }

    #[test]
    fn srgb_like_profile_parses() {
        // sRGB-ish primaries (D65 white). XYZ values are illustrative; the
        // parser's job is only to ratio them.
        let profile = build_min_profile(
            (0.4361, 0.2225, 0.0139), // r in PCS XYZ
            (0.3851, 0.7169, 0.0971), // g
            (0.1431, 0.0606, 0.7141), // b
            (0.9505, 1.0000, 1.0890), // w (D65)
            2.2,
        );
        let p = parse_icc(&profile).expect("valid sRGB-ish profile");
        // White point near D65 (0.3127, 0.3290).
        assert!((p.primaries[6] - 312_700).abs() < 5_000);
        assert!((p.primaries[7] - 329_000).abs() < 5_000);
        assert_eq!(p.tf_named, Some(2 /* Gamma22 */));
    }

    #[test]
    fn bad_magic_rejected() {
        let mut profile = build_min_profile(
            (0.4, 0.2, 0.0),
            (0.4, 0.7, 0.1),
            (0.1, 0.1, 0.7),
            (0.95, 1.0, 1.09),
            2.2,
        );
        profile[36] = b'X';
        assert_eq!(parse_icc(&profile), Err(IccError::BadMagic));
    }

    #[test]
    fn wrong_class_rejected() {
        let mut profile = build_min_profile(
            (0.4, 0.2, 0.0),
            (0.4, 0.7, 0.1),
            (0.1, 0.1, 0.7),
            (0.95, 1.0, 1.09),
            2.2,
        );
        profile[12..16].copy_from_slice(b"prtr"); // printer, not display
        assert_eq!(parse_icc(&profile), Err(IccError::UnsupportedClass));
    }

    #[test]
    fn wrong_color_space_rejected() {
        let mut profile = build_min_profile(
            (0.4, 0.2, 0.0),
            (0.4, 0.7, 0.1),
            (0.1, 0.1, 0.7),
            (0.95, 1.0, 1.09),
            2.2,
        );
        profile[16..20].copy_from_slice(b"CMYK");
        assert_eq!(parse_icc(&profile), Err(IccError::UnsupportedColorSpace));
    }

    #[test]
    fn gamma_24_classified_as_bt1886() {
        let profile = build_min_profile(
            (0.4, 0.2, 0.0),
            (0.4, 0.7, 0.1),
            (0.1, 0.1, 0.7),
            (0.95, 1.0, 1.09),
            2.4,
        );
        let p = parse_icc(&profile).unwrap();
        assert_eq!(p.tf_named, Some(1 /* Bt1886 */));
    }

    #[test]
    fn unusual_gamma_exposed_as_tf_power() {
        let profile = build_min_profile(
            (0.4, 0.2, 0.0),
            (0.4, 0.7, 0.1),
            (0.1, 0.1, 0.7),
            (0.95, 1.0, 1.09),
            1.8,
        );
        let p = parse_icc(&profile).unwrap();
        assert!(p.tf_named.is_none());
        // 1.8 * 10_000 = 18_000; allow ±1 for rounding through u8Fixed8.
        assert!(p.tf_power.is_some_and(|v| (v as i64 - 18_000).abs() <= 50));
    }

    #[test]
    fn too_short_rejected() {
        assert_eq!(parse_icc(&[0u8; 40]), Err(IccError::TooShort));
    }

    #[test]
    fn srgb_white_point_tags_named_primaries() {
        let profile = build_min_profile(
            (0.4361, 0.2225, 0.0139),
            (0.3851, 0.7169, 0.0971),
            (0.1431, 0.0606, 0.7141),
            (0.9505, 1.0000, 1.0890),
            2.2,
        );
        let parsed = parse_icc(&profile).unwrap();
        let params = parsed.into_params();
        assert_eq!(params.primaries_named, Some(1 /* Primaries::Srgb */));
        assert!(params.primaries.is_some());
        assert_eq!(params.tf_named, Some(2));
    }
}
