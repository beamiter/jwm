//! Wire representation and bounded JPEG decoding for remote desktop frames.

use super::RemoteResult;
use super::protocol::MAX_PAYLOAD_LEN;
use super::x11_capture::CapturedFrame;
use image::codecs::jpeg::JpegEncoder;
use image::{ExtendedColorType, ImageFormat, ImageReader, Limits, RgbImage};
use std::io::{self, Cursor, Write};

const FRAME_HEADER_LEN: usize = 16;
const ENCODE_RESERVE_SOFT_LIMIT: usize = 4 * 1024 * 1024;
const ENCODE_ERROR_RETAIN_LIMIT: usize = 8 * 1024 * 1024;
pub(crate) const MAX_DIMENSION: u16 = 16_384;
pub(crate) const MAX_PIXELS: u64 = 64 * 1024 * 1024;

struct BoundedPayloadWriter<'a> {
    payload: &'a mut Vec<u8>,
    max_len: usize,
}

impl Write for BoundedPayloadWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let remaining = self.max_len.saturating_sub(self.payload.len());
        if bytes.len() > remaining {
            return Err(invalid_data(format!(
                "encoded JPEG frame exceeds protocol limit of {} bytes",
                self.max_len
            )));
        }
        self.payload.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encode_reserve_hint(raw_len: usize) -> usize {
    FRAME_HEADER_LEN
        .saturating_add(raw_len / 4)
        .min(ENCODE_RESERVE_SOFT_LIMIT)
        .min(MAX_PAYLOAD_LEN)
}

#[derive(Debug)]
pub struct DecodedFrame {
    pub sequence: u64,
    pub source_width: u16,
    pub source_height: u16,
    pub image: RgbImage,
}

/// Encode one frame into a caller-owned payload buffer.
///
/// The buffer is cleared before every attempt and after every error. Modest
/// allocations are retained for reuse; an error after exceptional growth
/// releases allocations above 8 MiB. A new record therefore never appends to
/// stale bytes from the previous one.
pub fn encode_frame_into(
    payload: &mut Vec<u8>,
    sequence: u64,
    frame: &CapturedFrame,
    quality: u8,
) -> RemoteResult<()> {
    payload.clear();
    let result = (|| -> RemoteResult<()> {
        let encoded_width = u16::try_from(frame.image.width())
            .map_err(|_| invalid_data("encoded frame width exceeds protocol range"))?;
        let encoded_height = u16::try_from(frame.image.height())
            .map_err(|_| invalid_data("encoded frame height exceeds protocol range"))?;
        validate_dimensions(frame.source_width, frame.source_height)?;
        validate_dimensions(encoded_width, encoded_height)?;

        payload.reserve(encode_reserve_hint(frame.image.as_raw().len()));
        payload.extend_from_slice(&sequence.to_be_bytes());
        payload.extend_from_slice(&frame.source_width.to_be_bytes());
        payload.extend_from_slice(&frame.source_height.to_be_bytes());
        payload.extend_from_slice(&encoded_width.to_be_bytes());
        payload.extend_from_slice(&encoded_height.to_be_bytes());
        JpegEncoder::new_with_quality(
            BoundedPayloadWriter {
                payload,
                max_len: MAX_PAYLOAD_LEN,
            },
            quality.clamp(1, 100),
        )
        .encode(
            frame.image.as_raw(),
            frame.image.width(),
            frame.image.height(),
            ExtendedColorType::Rgb8,
        )?;

        if payload.len() > MAX_PAYLOAD_LEN {
            return Err(invalid_data(format!(
                "encoded JPEG frame is {} bytes; protocol limit is {MAX_PAYLOAD_LEN}",
                payload.len()
            ))
            .into());
        }
        Ok(())
    })();
    if result.is_err() {
        payload.clear();
        if payload.capacity() > ENCODE_ERROR_RETAIN_LIMIT {
            *payload = Vec::new();
        }
    }
    result
}

/// Encode one frame into a fresh owned payload.
pub fn encode_frame(sequence: u64, frame: &CapturedFrame, quality: u8) -> RemoteResult<Vec<u8>> {
    let mut payload = Vec::new();
    encode_frame_into(&mut payload, sequence, frame, quality)?;
    Ok(payload)
}

pub fn decode_frame(payload: &[u8]) -> RemoteResult<DecodedFrame> {
    if payload.len() <= FRAME_HEADER_LEN {
        return Err(invalid_data("remote frame is missing its JPEG body").into());
    }
    let sequence = u64::from_be_bytes(payload[0..8].try_into().unwrap());
    let source_width = u16::from_be_bytes(payload[8..10].try_into().unwrap());
    let source_height = u16::from_be_bytes(payload[10..12].try_into().unwrap());
    let encoded_width = u16::from_be_bytes(payload[12..14].try_into().unwrap());
    let encoded_height = u16::from_be_bytes(payload[14..16].try_into().unwrap());
    validate_dimensions(source_width, source_height)?;
    validate_dimensions(encoded_width, encoded_height)?;

    let allocation = u64::from(encoded_width)
        .saturating_mul(u64::from(encoded_height))
        .saturating_mul(4)
        .saturating_add(payload.len() as u64);
    let mut reader =
        ImageReader::with_format(Cursor::new(&payload[FRAME_HEADER_LEN..]), ImageFormat::Jpeg);
    let mut limits = Limits::default();
    limits.max_image_width = Some(u32::from(encoded_width));
    limits.max_image_height = Some(u32::from(encoded_height));
    limits.max_alloc = Some(allocation);
    reader.limits(limits);
    let image = reader.decode()?.into_rgb8();
    if image.width() != u32::from(encoded_width) || image.height() != u32::from(encoded_height) {
        return Err(
            invalid_data("JPEG dimensions do not match the authenticated frame header").into(),
        );
    }

    Ok(DecodedFrame {
        sequence,
        source_width,
        source_height,
        image,
    })
}

pub(crate) fn validate_dimensions(width: u16, height: u16) -> RemoteResult<()> {
    if width == 0 || height == 0 {
        return Err(invalid_data("remote frame has an empty dimension").into());
    }
    if width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(invalid_data("remote frame dimension exceeds the safety limit").into());
    }
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(invalid_data("remote frame pixel count exceeds the safety limit").into());
    }
    Ok(())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};

    #[test]
    fn frame_round_trip_keeps_metadata_and_dimensions() {
        let image = RgbImage::from_fn(8, 4, |x, y| Rgb([(x * 20) as u8, (y * 40) as u8, 90]));
        let payload = encode_frame(
            17,
            &CapturedFrame {
                image,
                source_width: 1920,
                source_height: 1080,
            },
            80,
        )
        .unwrap();
        let decoded = decode_frame(&payload).unwrap();
        assert_eq!(decoded.sequence, 17);
        assert_eq!((decoded.source_width, decoded.source_height), (1920, 1080));
        assert_eq!(decoded.image.dimensions(), (8, 4));
    }

    #[test]
    fn repeated_encoding_reuses_capacity_without_leaking_previous_bytes() {
        let large = CapturedFrame {
            image: RgbImage::from_fn(64, 48, |x, y| {
                Rgb([(x * 3) as u8, (y * 5) as u8, (x ^ y) as u8])
            }),
            source_width: 1920,
            source_height: 1080,
        };
        let small = CapturedFrame {
            image: RgbImage::from_pixel(8, 4, Rgb([20, 80, 160])),
            source_width: 800,
            source_height: 600,
        };
        let mut payload = Vec::new();
        encode_frame_into(&mut payload, 3, &large, 80).unwrap();
        let allocation = payload.as_ptr();
        let capacity = payload.capacity();

        encode_frame_into(&mut payload, 4, &large, 80).unwrap();
        assert_eq!(payload.as_ptr(), allocation);
        assert_eq!(payload.capacity(), capacity);
        assert_eq!(decode_frame(&payload).unwrap().sequence, 4);

        encode_frame_into(&mut payload, 5, &small, 75).unwrap();
        assert_eq!(payload.as_ptr(), allocation);
        assert_eq!(payload.capacity(), capacity);
        let decoded = decode_frame(&payload).unwrap();
        assert_eq!(decoded.sequence, 5);
        assert_eq!((decoded.source_width, decoded.source_height), (800, 600));
        assert_eq!(decoded.image.dimensions(), (8, 4));
    }

    #[test]
    fn failed_encoding_clears_payload_but_keeps_it_reusable() {
        let valid = CapturedFrame {
            image: RgbImage::from_pixel(16, 8, Rgb([10, 20, 30])),
            source_width: 16,
            source_height: 8,
        };
        let invalid = CapturedFrame {
            image: RgbImage::from_pixel(16, 8, Rgb([40, 50, 60])),
            source_width: 0,
            source_height: 8,
        };
        let mut payload = Vec::new();
        encode_frame_into(&mut payload, 1, &valid, 70).unwrap();
        let capacity = payload.capacity();

        assert!(encode_frame_into(&mut payload, 2, &invalid, 70).is_err());
        assert!(payload.is_empty());
        assert_eq!(payload.capacity(), capacity);

        encode_frame_into(&mut payload, 3, &valid, 70).unwrap();
        assert_eq!(decode_frame(&payload).unwrap().sequence, 3);

        let mut exceptional = Vec::with_capacity(ENCODE_ERROR_RETAIN_LIMIT + 1);
        exceptional.extend_from_slice(b"stale");
        assert!(encode_frame_into(&mut exceptional, 4, &invalid, 70).is_err());
        assert!(exceptional.is_empty());
        assert_eq!(exceptional.capacity(), 0);
    }

    #[test]
    fn bounded_payload_writer_never_appends_a_partial_over_limit_chunk() {
        let mut payload = b"head".to_vec();
        let mut writer = BoundedPayloadWriter {
            payload: &mut payload,
            max_len: 6,
        };
        assert!(writer.write(b"jpeg").is_err());
        assert_eq!(payload, b"head");
    }

    #[test]
    fn encode_reserve_hint_is_soft_capped_for_extreme_raw_frames() {
        assert_eq!(
            encode_reserve_hint((MAX_PIXELS as usize) * 3),
            ENCODE_RESERVE_SOFT_LIMIT
        );
        assert_eq!(encode_reserve_hint(usize::MAX), ENCODE_RESERVE_SOFT_LIMIT);
    }

    #[test]
    fn frame_header_and_jpeg_dimensions_must_agree() {
        let image = RgbImage::new(4, 4);
        let mut payload = encode_frame(
            1,
            &CapturedFrame {
                image,
                source_width: 4,
                source_height: 4,
            },
            75,
        )
        .unwrap();
        payload[12..14].copy_from_slice(&3_u16.to_be_bytes());
        assert!(decode_frame(&payload).is_err());
    }

    #[test]
    fn absurd_dimensions_are_rejected_before_decode() {
        let mut payload = vec![0; FRAME_HEADER_LEN + 1];
        payload[8..10].copy_from_slice(&u16::MAX.to_be_bytes());
        payload[10..12].copy_from_slice(&u16::MAX.to_be_bytes());
        payload[12..14].copy_from_slice(&1_u16.to_be_bytes());
        payload[14..16].copy_from_slice(&1_u16.to_be_bytes());
        assert!(decode_frame(&payload).is_err());
    }
}
