//! Tagged video records sent only after both peers select a hardware profile.

use super::RemoteResult;
use super::frame::{invalid_data, validate_dimensions};
use super::messages::{FrameCodec, FrameLimits};
use super::source_wire::MAX_FRAME_INNER_PAYLOAD_LEN;

const JPEG_TAG: u8 = 1;
const H264_TAG: u8 = 2;
const HEVC444_TAG: u8 = 3;
const HEADER_LEN: usize = 24;
pub(crate) const MAX_H264_ACCESS_UNIT_LEN: usize = 1024 * 1024;
pub(crate) const MAX_H264_DIMENSION: u16 = 4096;
pub(crate) const MAX_H264_PIXELS: u32 = 4096 * 2160;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HardwareFrameHeader {
    pub(crate) sequence: u64,
    pub(crate) source_width: u16,
    pub(crate) source_height: u16,
    pub(crate) encoded_width: u16,
    pub(crate) encoded_height: u16,
    pub(crate) geometry_epoch: u64,
}

impl HardwareFrameHeader {
    fn validate(self, limits: FrameLimits) -> RemoteResult<()> {
        validate_dimensions(self.source_width, self.source_height)?;
        limits.validate_dimensions(self.encoded_width, self.encoded_height)?;
        h264_padded_dimensions(self.encoded_width, self.encoded_height)?;
        if self.encoded_width > self.source_width || self.encoded_height > self.source_height {
            return Err(invalid_data("video frame exceeds its source dimensions").into());
        }
        Ok(())
    }
}

/// Logical odd extents use a right/bottom pad in the hardware surface.
pub(crate) fn h264_padded_dimensions(width: u16, height: u16) -> RemoteResult<(u16, u16)> {
    if !(2..=MAX_H264_DIMENSION).contains(&width) || !(2..=MAX_H264_DIMENSION).contains(&height) {
        return Err(
            invalid_data("video frame exceeds the supported hardware surface dimensions").into(),
        );
    }
    let (width, height) = (width + width % 2, height + height % 2);
    if u32::from(width) * u32::from(height) > MAX_H264_PIXELS {
        return Err(invalid_data("video frame exceeds the supported hardware surface area").into());
    }
    Ok((width, height))
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum VideoPayload<'a> {
    Jpeg(&'a [u8]),
    Hardware {
        codec: FrameCodec,
        header: HardwareFrameHeader,
        annex_b: &'a [u8],
    },
}

pub(crate) fn encode_hardware_frame(
    payload: &mut Vec<u8>,
    codec: FrameCodec,
    header: HardwareFrameHeader,
    annex_b: &[u8],
    limits: FrameLimits,
) -> RemoteResult<()> {
    let tag = match codec {
        FrameCodec::H264 => H264_TAG,
        FrameCodec::Hevc444 => HEVC444_TAG,
        FrameCodec::JpegTiles => return Err(invalid_data("JPEG has no Annex B access unit").into()),
    };
    header.validate(limits)?;
    validate_access_unit(codec, annex_b)?;
    payload.clear();
    payload.push(tag);
    payload.extend_from_slice(&header.sequence.to_be_bytes());
    payload.extend_from_slice(&header.source_width.to_be_bytes());
    payload.extend_from_slice(&header.source_height.to_be_bytes());
    payload.extend_from_slice(&header.encoded_width.to_be_bytes());
    payload.extend_from_slice(&header.encoded_height.to_be_bytes());
    payload.extend_from_slice(&header.geometry_epoch.to_be_bytes());
    payload.extend_from_slice(annex_b);
    Ok(())
}

pub(crate) fn tag_jpeg_frame(payload: &mut Vec<u8>) -> RemoteResult<()> {
    if payload.is_empty() || payload.len() >= MAX_FRAME_INNER_PAYLOAD_LEN {
        return Err(invalid_data("JPEG fallback exceeds its tagged record budget").into());
    }
    payload.insert(0, JPEG_TAG);
    Ok(())
}

pub(crate) fn decode_video_payload(
    payload: &[u8],
    limits: FrameLimits,
) -> RemoteResult<VideoPayload<'_>> {
    if payload.len() > MAX_FRAME_INNER_PAYLOAD_LEN {
        return Err(invalid_data("tagged video payload exceeds its record budget").into());
    }
    match payload.split_first() {
        Some((&JPEG_TAG, jpeg)) if !jpeg.is_empty() => Ok(VideoPayload::Jpeg(jpeg)),
        Some((&tag, frame)) if matches!(tag, H264_TAG | HEVC444_TAG) => {
            let codec = if tag == H264_TAG {
                FrameCodec::H264
            } else {
                FrameCodec::Hevc444
            };
            let Some((bytes, annex_b)) = frame.split_at_checked(HEADER_LEN) else {
                return Err(invalid_data("truncated hardware video frame header").into());
            };
            let header = HardwareFrameHeader {
                sequence: u64::from_be_bytes(bytes[..8].try_into().unwrap()),
                source_width: u16::from_be_bytes(bytes[8..10].try_into().unwrap()),
                source_height: u16::from_be_bytes(bytes[10..12].try_into().unwrap()),
                encoded_width: u16::from_be_bytes(bytes[12..14].try_into().unwrap()),
                encoded_height: u16::from_be_bytes(bytes[14..16].try_into().unwrap()),
                geometry_epoch: u64::from_be_bytes(bytes[16..24].try_into().unwrap()),
            };
            header.validate(limits)?;
            validate_access_unit(codec, annex_b)?;
            Ok(VideoPayload::Hardware {
                codec,
                header,
                annex_b,
            })
        }
        _ => Err(invalid_data("unsupported or empty tagged video payload").into()),
    }
}

fn validate_access_unit(codec: FrameCodec, annex_b: &[u8]) -> RemoteResult<()> {
    let first_nal = annex_b
        .strip_prefix(&[0, 0, 0, 1])
        .or_else(|| annex_b.strip_prefix(&[0, 0, 1]));
    if annex_b.len() > MAX_H264_ACCESS_UNIT_LEN
        || first_nal.and_then(|nal| nal_type(codec, nal)).is_none()
    {
        return Err(invalid_data("invalid or oversized Annex B access unit").into());
    }
    Ok(())
}

fn nal_type(codec: FrameCodec, nal: &[u8]) -> Option<u8> {
    let first = *nal.first()?;
    if first & 0x80 != 0 {
        return None;
    }
    match codec {
        FrameCodec::H264 if first & 0x1f != 0 => Some(first & 0x1f),
        FrameCodec::Hevc444 => {
            let second = *nal.get(1)?;
            // The base-layer profile has a zero layer ID and a nonzero
            // temporal_id_plus1. A HEVC header occupies two bytes.
            (first & 1 == 0 && second != 0 && second <= 7).then_some((first >> 1) & 0x3f)
        }
        _ => None,
    }
}

pub(crate) fn hardware_has_idr(codec: FrameCodec, annex_b: &[u8]) -> bool {
    // Four-byte start codes contain this three-byte suffix. Valid Annex B NAL
    // bodies use emulation prevention, so the pattern delimits a real NAL.
    annex_b.windows(3).enumerate().any(|(start, prefix)| {
        prefix == [0, 0, 1]
            && matches!(
                (codec, nal_type(codec, &annex_b[start + 3..])),
                (FrameCodec::H264, Some(5)) | (FrameCodec::Hevc444, Some(19 | 20))
            )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: HardwareFrameHeader = HardwareFrameHeader {
        sequence: 0,
        source_width: 2560,
        source_height: 1440,
        encoded_width: 1392,
        encoded_height: 783,
        geometry_epoch: 7,
    };
    const IDR: &[u8] = &[0, 0, 0, 1, 0x67, 0x10, 0, 0, 1, 0x65, 0x10];
    const HEVC_IDR: &[u8] = &[0, 0, 0, 1, 0x40, 1, 0x10, 0, 0, 1, 0x26, 1, 0x10];

    #[test]
    fn hevc_444_has_its_own_tag_and_two_byte_nal_header() {
        let limits = FrameLimits::default();
        let codec = FrameCodec::Hevc444;
        let mut payload = Vec::new();
        encode_hardware_frame(&mut payload, codec, FRAME, HEVC_IDR, limits).unwrap();
        assert_eq!(payload[0], HEVC444_TAG);
        assert_eq!(
            decode_video_payload(&payload, limits).unwrap(),
            VideoPayload::Hardware {
                codec,
                header: FRAME,
                annex_b: HEVC_IDR
            }
        );
        assert!(hardware_has_idr(codec, HEVC_IDR));
        assert!(hardware_has_idr(codec, &[0, 0, 1, 0x28, 1, 0x10]));
        assert!(!hardware_has_idr(codec, &[0, 0, 1, 0x2a, 1, 0x10]));
        assert!(!hardware_has_idr(codec, &[0, 0, 1, 0x26, 0, 0x10]));
        assert!(!hardware_has_idr(codec, IDR));
        assert!(!hardware_has_idr(FrameCodec::H264, HEVC_IDR));
        assert!(
            encode_hardware_frame(&mut payload, codec, FRAME, &[0, 0, 1, 0x26], limits).is_err()
        );
        assert!(
            encode_hardware_frame(&mut payload, FrameCodec::JpegTiles, FRAME, HEVC_IDR, limits)
                .is_err()
        );
    }

    #[test]
    fn h264_header_round_trips_logical_odd_extent_and_annex_b() {
        let limits = FrameLimits::default();
        let mut payload = Vec::new();
        encode_hardware_frame(&mut payload, FrameCodec::H264, FRAME, IDR, limits).unwrap();
        assert_eq!(payload[0], H264_TAG);
        assert_eq!(&payload[HEADER_LEN + 1..], IDR);
        assert_eq!(
            decode_video_payload(&payload, limits).unwrap(),
            VideoPayload::Hardware {
                codec: FrameCodec::H264,
                header: FRAME,
                annex_b: IDR
            }
        );
        assert_eq!(h264_padded_dimensions(1392, 783).unwrap(), (1392, 784));
        assert!(hardware_has_idr(FrameCodec::H264, IDR));
        assert!(!hardware_has_idr(FrameCodec::H264, &[0, 0, 1, 0x61, 0x10]));
        assert!(!hardware_has_idr(FrameCodec::H264, &[0, 0, 1, 0xe5, 0x10]));
    }

    #[test]
    fn jpeg_fallback_remains_an_untouched_legacy_body() {
        let mut payload = vec![0, 2, 3];
        tag_jpeg_frame(&mut payload).unwrap();
        assert_eq!(
            decode_video_payload(&payload, FrameLimits::default()).unwrap(),
            VideoPayload::Jpeg(&[0, 2, 3])
        );
    }

    #[test]
    fn rejects_malformed_tags_packets_and_geometry() {
        let limits = FrameLimits::default();
        let mut payload = Vec::new();
        for invalid in [
            HardwareFrameHeader {
                source_width: 0,
                ..FRAME
            },
            HardwareFrameHeader {
                encoded_width: 1,
                ..FRAME
            },
            HardwareFrameHeader {
                encoded_height: 1441,
                ..FRAME
            },
        ] {
            assert!(
                encode_hardware_frame(&mut payload, FrameCodec::H264, invalid, IDR, limits)
                    .is_err()
            );
        }
        for invalid in [&[][..], &[0, 0, 1], &[0, 0, 1, 0xe5], &[0, 0, 0, 2, 0x65]] {
            assert!(
                encode_hardware_frame(&mut payload, FrameCodec::H264, FRAME, invalid, limits)
                    .is_err()
            );
        }
        let mut oversized = vec![0; MAX_H264_ACCESS_UNIT_LEN + 1];
        oversized[..IDR.len()].copy_from_slice(IDR);
        assert!(
            encode_hardware_frame(&mut payload, FrameCodec::H264, FRAME, &oversized, limits)
                .is_err()
        );
        for invalid in [&[][..], &[0, 1], &[JPEG_TAG], &[H264_TAG, 0], &[4, 1]] {
            assert!(decode_video_payload(invalid, limits).is_err());
        }
        assert!(h264_padded_dimensions(4097, 100).is_err());
        assert!(h264_padded_dimensions(4096, 2161).is_err());
        assert_eq!(h264_padded_dimensions(2159, 4095).unwrap(), (2160, 4096));
        encode_hardware_frame(&mut payload, FrameCodec::H264, FRAME, IDR, limits).unwrap();
        assert!(
            decode_video_payload(
                &payload,
                FrameLimits {
                    max_width: 1391,
                    ..limits
                }
            )
            .is_err()
        );
    }
}
