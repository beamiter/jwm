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

// CRC covers the chunk type and data, never the preceding length. The fixed
// 1 KiB table removes eight polynomial rounds per byte on both encode validation
// and decode. It adds no input-sized allocation and preserves the IEEE CRC.
const CRC_TABLE: [u32; 256] = {
    let mut table = [0; 256];
    let mut index = 0;
    while index < table.len() {
        let mut crc = index as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
            bit += 1;
        }
        table[index] = crc;
        index += 1;
    }
    table
};

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc = (crc >> 8) ^ CRC_TABLE[((crc ^ u32::from(byte)) & 255) as usize];
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

#[cfg(test)]
pub(super) fn decode_rgb8(bytes: &[u8], width: u32, height: u32) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    decode_rgb8_into(bytes, width, height, &mut output)?;
    Ok(output)
}

/// Reuse the tile decoder's private atlas allocation. No partially decoded
/// bytes are exposed on failure, and the caller may scatter only after success.
/// After success, logical output and retained scratch obey the PNG budget.
/// Drop an oversized preceding JPEG allocation before decoding a valid PNG.
pub(super) fn decode_rgb8_into(
    bytes: &[u8],
    width: u32,
    height: u32,
    output: &mut Vec<u8>,
) -> io::Result<()> {
    let result = (|| {
        let size = validate(bytes, width, height)?;
        let mut decoder = png::Decoder::new(Cursor::new(bytes));
        decoder.set_limits(png::Limits {
            bytes: DECODER_ALLOCATION_LIMIT,
        });
        let mut reader = decoder.read_info().map_err(io::Error::other)?;
        if reader.output_buffer_size() != Some(size) {
            return Err(invalid("PNG output size mismatch"));
        }
        if output.capacity() > MAX_RGB_BYTES {
            *output = Vec::new();
        }
        if output.capacity() < size {
            output
                .try_reserve_exact(size.saturating_sub(output.len()))
                .map_err(io::Error::other)?;
        }
        output.resize(size, 0);
        let info = reader.next_frame(output).map_err(io::Error::other)?;
        if info.width != width
            || info.height != height
            || info.color_type != png::ColorType::Rgb
            || info.bit_depth != png::BitDepth::Eight
            || info.buffer_size() != size
        {
            return Err(invalid("PNG decoded atlas layout mismatch"));
        }
        reader.finish().map_err(io::Error::other)?;
        Ok(())
    })();
    if result.is_err() {
        output.clear();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn reference_crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
            }
        }
        !crc
    }

    #[test]
    fn crc_table_matches_reference_and_known_vectors() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        for byte in 0..=255 {
            assert_eq!(crc32(&[byte]), reference_crc32(&[byte]));
        }
        let data: Vec<_> = (0..MAX_COMPRESSED_BYTES)
            .map(|i| (i.wrapping_mul(131) ^ (i >> 7)) as u8)
            .collect();
        for size in [
            0,
            1,
            3,
            7,
            8,
            15,
            256,
            1023,
            MAX_RGB_BYTES,
            MAX_COMPRESSED_BYTES,
        ] {
            for offset in 0..8 {
                let end = (offset + size).min(data.len());
                assert_eq!(
                    crc32(&data[offset..end]),
                    reference_crc32(&data[offset..end])
                );
            }
        }
    }

    #[test]
    fn reusable_png_output_retains_allocation_and_overwrites_all_pixels() {
        let mut output = Vec::new();
        let mut saved = None;
        for (w, h, byte) in [(256, 256, 213), (3, 5, 17), (1, 1, 0), (256, 256, 42)] {
            let pixels = vec![byte; rgb_len(w, h).unwrap()];
            let encoded = encode_rgb8(w, h, &pixels).unwrap();
            decode_rgb8_into(&encoded, w, h, &mut output).unwrap();
            assert_eq!(output, pixels);
            let allocation = (output.as_ptr(), output.capacity());
            if let Some(previous) = saved {
                assert_eq!(allocation, previous);
            }
            saved = Some(allocation);
        }
    }

    #[test]
    fn png_decode_releases_oversized_jpeg_scratch() {
        let encoded = encode_rgb8(3, 5, &[17; 45]).unwrap();
        let mut output = vec![99; MAX_RGB_BYTES + 1];
        decode_rgb8_into(&encoded, 3, 5, &mut output).unwrap();
        assert_eq!(output, [17; 45]);
        assert!(output.capacity() <= MAX_RGB_BYTES);
    }

    #[test]
    fn failed_reusable_decode_clears_output_and_recovers() {
        let good = encode_rgb8(17, 33, &[173; 17 * 33 * 3]).unwrap();
        let mut output = vec![99; MAX_RGB_BYTES];
        let allocation = (output.as_ptr(), output.capacity());
        let mut malformed = good[..33].to_vec();
        // Valid envelope/CRC, invalid zlib: exercise failure after validation.
        malformed.extend(chunk(b"IDAT", &[0, 1, 2, 3]));
        malformed.extend(chunk(b"IEND", &[]));
        for bad in [&[][..], &good[..good.len() - 1], &malformed] {
            output.resize(MAX_RGB_BYTES, 99);
            assert!(decode_rgb8_into(bad, 17, 33, &mut output).is_err());
            assert!(output.is_empty());
            assert_eq!((output.as_ptr(), output.capacity()), allocation);
            decode_rgb8_into(&good, 17, 33, &mut output).unwrap();
            assert_eq!(output, [173; 17 * 33 * 3]);
        }
        assert!(decode_rgb8_into(&good, 0, 33, &mut output).is_err());
        assert!(output.is_empty());
    }

    #[test]
    fn every_single_bit_png_corruption_is_rejected_before_returning_pixels() {
        let good = encode_rgb8(3, 5, &[127; 45]).unwrap();
        let mut output = Vec::new();
        for index in 0..good.len() {
            for bit in 0..8 {
                let mut bad = good.clone();
                bad[index] ^= 1 << bit;
                assert!(
                    decode_rgb8_into(&bad, 3, 5, &mut output).is_err(),
                    "{index}:{bit}"
                );
                assert!(output.is_empty());
            }
        }
    }

    #[test]
    #[ignore = "isolated CRC microbenchmark; not end-to-end latency"]
    fn benchmark_png_crc_validation() {
        use std::hint::black_box;
        use std::time::Instant;
        for size in [4096, MAX_RGB_BYTES, MAX_COMPRESSED_BYTES] {
            let data: Vec<_> = (0..size)
                .map(|i| (i.wrapping_mul(131) ^ (i >> 7)) as u8)
                .collect();
            let repetitions = 32;
            let start = Instant::now();
            for _ in 0..repetitions {
                black_box(reference_crc32(black_box(&data)));
            }
            let reference = start.elapsed();
            let start = Instant::now();
            for _ in 0..repetitions {
                black_box(crc32(black_box(&data)));
            }
            let table = start.elapsed();
            assert_eq!(crc32(&data), reference_crc32(&data));
            eprintln!(
                "png_crc bytes={size} iterations={repetitions} reference_us={} table_us={}",
                reference.as_micros(),
                table.as_micros()
            );
        }
    }

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
