//! Deliberately narrow PNG dialect for bounded RGB8 tile atlases.
//! This is not a general image/metadata decoder. The envelope (including CRCs,
//! chunk order and final IEND) is strict; zlib consumption is not canonicalized.
//! png 0.18 may ignore extra compressed streams or decompressed rows inside
//! IDAT. Such input remains under the compressed/allocation budgets and must
//! still produce exactly the declared RGB pixels before any pixels are returned.
use std::io::{self, Cursor, Write};

pub(super) const MAX_PIXELS: usize = 65_536;
pub(super) const MAX_RGB_BYTES: usize = 196_608;
pub(super) const MAX_COMPRESSED_BYTES: usize = 1_048_576;
const DECODER_ALLOCATION_LIMIT: usize = 4 * 1_048_576;
const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn rgb_len(width: u32, height: u32) -> io::Result<usize> {
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .filter(|p| width != 0 && height != 0 && *p <= MAX_PIXELS as u64)
        .ok_or_else(|| invalid("PNG atlas dimensions exceed budget"))?;
    let bytes = usize::try_from(pixels)
        .ok()
        .and_then(|p| p.checked_mul(3))
        .filter(|n| *n <= MAX_RGB_BYTES)
        .ok_or_else(|| invalid("PNG atlas RGB allocation exceeds budget"))?;
    Ok(bytes)
}

// CRC covers the chunk type and data, never the preceding length. No table or
// attacker-sized temporary allocation is needed; the whole input is <=1 MiB.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

fn validate(bytes: &[u8], width: u32, height: u32) -> io::Result<usize> {
    let size = rgb_len(width, height)?;
    if bytes.len() > MAX_COMPRESSED_BYTES || !bytes.starts_with(SIGNATURE) {
        return Err(invalid("PNG atlas signature or compressed budget invalid"));
    }
    let mut offset = SIGNATURE.len();
    let mut header = false;
    let mut data = false;
    let mut data_bytes = 0usize;
    while offset < bytes.len() {
        let prefix = bytes
            .get(
                offset
                    ..offset
                        .checked_add(8)
                        .ok_or_else(|| invalid("PNG offset overflow"))?,
            )
            .ok_or_else(|| invalid("truncated PNG chunk header"))?;
        let length = u32::from_be_bytes(prefix[..4].try_into().unwrap()) as usize;
        let end = offset
            .checked_add(12)
            .and_then(|n| n.checked_add(length))
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| invalid("truncated PNG chunk"))?;
        let payload = &bytes[offset + 8..end - 4];
        let expected_crc = u32::from_be_bytes(bytes[end - 4..end].try_into().unwrap());
        if crc32(&bytes[offset + 4..end - 4]) != expected_crc {
            return Err(invalid("PNG chunk CRC mismatch"));
        }
        match &prefix[4..8] {
            b"IHDR" if !header && offset == 8 && length == 13 => {
                if payload[..4] != width.to_be_bytes()
                    || payload[4..8] != height.to_be_bytes()
                    || payload[8..] != [8, 2, 0, 0, 0]
                {
                    return Err(invalid("PNG atlas must be exact-size noninterlaced RGB8"));
                }
                header = true;
            }
            b"IDAT" if header => {
                data = true;
                data_bytes = data_bytes
                    .checked_add(length)
                    .ok_or_else(|| invalid("PNG data overflow"))?;
            }
            b"IEND" if header && data && data_bytes != 0 && length == 0 && end == bytes.len() => {
                return Ok(size);
            }
            _ => {
                return Err(invalid(
                    "PNG atlas chunk order/type or trailing bytes invalid",
                ));
            }
        }
        offset = end;
    }
    Err(invalid("PNG atlas is missing IEND"))
}

struct BoundedBytes(Vec<u8>);
impl Write for BoundedBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self
            .0
            .len()
            .checked_add(bytes.len())
            .is_none_or(|n| n > MAX_COMPRESSED_BYTES)
        {
            return Err(invalid("encoded PNG atlas exceeds compressed budget"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn encode_rgb8(width: u32, height: u32, pixels: &[u8]) -> io::Result<Vec<u8>> {
    if pixels.len() != rgb_len(width, height)? {
        return Err(invalid("PNG RGB length mismatch"));
    }
    let mut output = BoundedBytes(Vec::new());
    {
        let mut encoder = png::Encoder::new(&mut output, width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(io::Error::other)?;
        writer.write_image_data(pixels).map_err(io::Error::other)?;
        writer.finish().map_err(io::Error::other)?;
    }
    validate(&output.0, width, height)?;
    Ok(output.0)
}

pub(super) fn decode_rgb8(bytes: &[u8], width: u32, height: u32) -> io::Result<Vec<u8>> {
    let size = validate(bytes, width, height)?;
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_limits(png::Limits {
        bytes: DECODER_ALLOCATION_LIMIT,
    });
    let mut reader = decoder.read_info().map_err(io::Error::other)?;
    if reader.output_buffer_size() != Some(size) {
        return Err(invalid("PNG output size mismatch"));
    }
    let mut output = vec![0u8; size];
    let info = reader.next_frame(&mut output).map_err(io::Error::other)?;
    if info.width != width
        || info.height != height
        || info.color_type != png::ColorType::Rgb
        || info.bit_depth != png::BitDepth::Eight
        || info.buffer_size() != size
    {
        return Err(invalid("PNG decoded atlas layout mismatch"));
    }
    reader.finish().map_err(io::Error::other)?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn chunk(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = (payload.len() as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(payload);
        let crc = crc32(&out[4..]);
        out.extend_from_slice(&crc.to_be_bytes());
        out
    }
    fn replace_header(bytes: &[u8], position: usize, value: u8) -> Vec<u8> {
        let mut header = bytes[16..29].to_vec();
        header[position] = value;
        let mut out = SIGNATURE.to_vec();
        out.extend(chunk(b"IHDR", &header));
        out.extend_from_slice(&bytes[33..]);
        out
    }
    #[test]
    fn exact_roundtrip_odd_and_budget_edges() {
        for (w, h) in [(1, 1), (17, 33), (256, 256), (65_536, 1), (1, 65_536)] {
            let input: Vec<u8> = (0..rgb_len(w, h).unwrap())
                .map(|i| (i.wrapping_mul(131) ^ (i >> 7)) as u8)
                .collect();
            let encoded = encode_rgb8(w, h, &input).unwrap();
            assert_eq!(decode_rgb8(&encoded, w, h).unwrap(), input);
        }
        for (w, h) in [(0, 1), (1, 0), (257, 256), (u32::MAX, u32::MAX)] {
            assert!(encode_rgb8(w, h, &[]).is_err());
            assert!(decode_rgb8(&[], w, h).is_err());
        }
        assert!(encode_rgb8(1, 1, &[1, 2]).is_err());
    }
    #[test]
    fn forbidden_chunks_formats_crc_and_trailing_are_rejected() {
        let good = encode_rgb8(3, 5, &[127; 45]).unwrap();
        for kind in [
            b"acTL", b"fcTL", b"fdAT", b"tEXt", b"iCCP", b"PLTE", b"tRNS", b"zzZZ",
        ] {
            let mut bad = good[..33].to_vec();
            bad.extend(chunk(kind, &[]));
            bad.extend_from_slice(&good[33..]);
            assert!(decode_rgb8(&bad, 3, 5).is_err(), "{kind:?}");
        }
        for (index, value) in [
            (8, 16),
            (9, 0),
            (9, 3),
            (9, 4),
            (9, 6),
            (10, 1),
            (11, 1),
            (12, 1),
        ] {
            assert!(decode_rgb8(&replace_header(&good, index, value), 3, 5).is_err());
        }
        let mut bad = good.clone();
        bad[29] ^= 1;
        assert!(decode_rgb8(&bad, 3, 5).is_err());
        let mut bad = good.clone();
        bad.push(0);
        assert!(decode_rgb8(&bad, 3, 5).is_err());
        assert!(decode_rgb8(&good, 5, 3).is_err());
        for n in 0..good.len() {
            assert!(decode_rgb8(&good[..n], 3, 5).is_err(), "prefix {n}");
        }
        assert!(decode_rgb8(&vec![0; MAX_COMPRESSED_BYTES + 1], 3, 5).is_err());
    }
    #[test]
    fn invalid_idat_and_chunk_order_do_not_return_partial_pixels() {
        let good = encode_rgb8(3, 5, &[127; 45]).unwrap();
        let header = &good[8..33];
        for data in [&[][..], &[0, 1, 2, 3][..]] {
            let mut bad = SIGNATURE.to_vec();
            bad.extend_from_slice(header);
            bad.extend(chunk(b"IDAT", data));
            bad.extend(chunk(b"IEND", &[]));
            assert!(decode_rgb8(&bad, 3, 5).is_err());
        }
        let mut duplicate = good[..33].to_vec();
        duplicate.extend_from_slice(&good[8..]);
        assert!(decode_rgb8(&duplicate, 3, 5).is_err());
        let mut huge = SIGNATURE.to_vec();
        huge.extend(u32::MAX.to_be_bytes());
        huge.extend(b"IDAT");
        assert!(decode_rgb8(&huge, 3, 5).is_err());
        assert_eq!(decode_rgb8(&good, 3, 5).unwrap(), [127; 45]);
    }
    #[test]
    fn tolerated_idat_internal_surplus_still_has_exact_bounded_pixels() {
        fn idat(bytes: &[u8]) -> Vec<u8> {
            let mut result = Vec::new();
            let mut at = 8;
            while at < bytes.len() {
                let length = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
                if &bytes[at + 4..at + 8] == b"IDAT" {
                    result.extend_from_slice(&bytes[at + 8..at + 8 + length]);
                }
                at += length + 12;
            }
            result
        }
        let one = encode_rgb8(3, 1, &[127; 9]).unwrap();
        let two = encode_rgb8(3, 2, &[127; 18]).unwrap();
        let mut double_stream = idat(&one);
        double_stream.extend(idat(&one));
        for (name, compressed) in [
            ("second zlib stream", double_stream),
            ("extra decompressed scanline", idat(&two)),
        ] {
            let mut bytes = one[..33].to_vec();
            bytes.extend(chunk(b"IDAT", &compressed));
            bytes.extend(chunk(b"IEND", &[]));
            // Every chunk CRC and the exact final IEND are valid. This probes
            // the library's inner stream policy, not the envelope validator.
            assert_eq!(validate(&bytes, 3, 1).unwrap(), 9);
            match decode_rgb8(&bytes, 3, 1) {
                Ok(pixels) => {
                    assert_eq!(pixels, [127; 9]);
                    eprintln!("PNG inner surplus {name}: accepted, exact bounded output");
                }
                Err(error) => eprintln!("PNG inner surplus {name}: rejected: {error}"),
            }
        }
    }

    #[test]
    fn bounded_encoder_writer_never_partially_accepts_over_budget() {
        let mut output = BoundedBytes(vec![0; MAX_COMPRESSED_BYTES - 1]);
        assert!(output.write_all(&[1, 2]).is_err());
        assert_eq!(output.0.len(), MAX_COMPRESSED_BYTES - 1);
        output.write_all(&[1]).unwrap();
        assert!(output.write_all(&[2]).is_err());
    }
}
