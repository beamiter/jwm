//! Wire representation and bounded JPEG decoding for remote desktop frames.

use super::RemoteResult;
use super::protocol::MAX_PAYLOAD_LEN;
use super::x11_capture::CapturedFrame;
use image::codecs::jpeg::JpegEncoder;
use image::{ExtendedColorType, ImageFormat, ImageReader, Limits, RgbImage};
use std::io::{self, Cursor};

const FRAME_HEADER_LEN: usize = 16;
pub(crate) const MAX_DIMENSION: u16 = 16_384;
pub(crate) const MAX_PIXELS: u64 = 64 * 1024 * 1024;

#[derive(Debug)]
pub struct DecodedFrame {
    pub sequence: u64,
    pub source_width: u16,
    pub source_height: u16,
    pub image: RgbImage,
}

pub fn encode_frame(sequence: u64, frame: &CapturedFrame, quality: u8) -> RemoteResult<Vec<u8>> {
    let encoded_width = u16::try_from(frame.image.width())
        .map_err(|_| invalid_data("encoded frame width exceeds protocol range"))?;
    let encoded_height = u16::try_from(frame.image.height())
        .map_err(|_| invalid_data("encoded frame height exceeds protocol range"))?;
    validate_dimensions(frame.source_width, frame.source_height)?;
    validate_dimensions(encoded_width, encoded_height)?;

    let mut payload = Vec::with_capacity(FRAME_HEADER_LEN + frame.image.as_raw().len() / 4);
    payload.extend_from_slice(&sequence.to_be_bytes());
    payload.extend_from_slice(&frame.source_width.to_be_bytes());
    payload.extend_from_slice(&frame.source_height.to_be_bytes());
    payload.extend_from_slice(&encoded_width.to_be_bytes());
    payload.extend_from_slice(&encoded_height.to_be_bytes());
    JpegEncoder::new_with_quality(&mut payload, quality.clamp(1, 100)).encode(
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
