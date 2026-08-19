//! Wire representation and bounded JPEG decoding for remote desktop frames.

use super::RemoteResult;
use super::protocol::MAX_PAYLOAD_LEN;
use super::x11_capture::CapturedFrame;
use image::codecs::jpeg::{JpegDecoder, JpegEncoder};
use image::{ColorType, DynamicImage, ExtendedColorType, ImageDecoder, Limits, RgbImage};
use std::io::{self, Cursor, Write};
use std::sync::{Arc, Mutex, MutexGuard};

const FRAME_HEADER_LEN: usize = 16;
const ENCODE_RESERVE_SOFT_LIMIT: usize = 4 * 1024 * 1024;
const ENCODE_ERROR_RETAIN_LIMIT: usize = 8 * 1024 * 1024;
const DECODE_POOL_MAX_SLOTS: usize = 2;
const DECODE_POOL_MAX_SLOT_BYTES: usize = 32 * 1024 * 1024;
const DECODE_POOL_MAX_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const MAX_DIMENSION: u16 = 16_384;
pub(crate) const MAX_PIXELS: u64 = 64 * 1024 * 1024;

#[derive(Debug, Default)]
pub(crate) struct DecodeBufferPool {
    buffers: Vec<Vec<u8>>,
    retained_bytes: usize,
}

pub(crate) type SharedDecodeBufferPool = Arc<Mutex<DecodeBufferPool>>;

pub(crate) fn new_decode_buffer_pool() -> SharedDecodeBufferPool {
    Arc::new(Mutex::new(DecodeBufferPool::default()))
}

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

/// A decoded frame whose RGB allocation returns to the client's small pool.
///
/// This stays crate-private so the public `DecodedFrame` and `Viewer::draw`
/// APIs remain source compatible. The receiver and viewer use this ownership
/// path internally, including while the most recently presented frame is kept
/// for Expose and resize redraws.
#[derive(Debug)]
pub(crate) struct RecyclableDecodedFrame {
    sequence: u64,
    source_width: u16,
    source_height: u16,
    image: Option<RgbImage>,
    pool: SharedDecodeBufferPool,
}

impl RecyclableDecodedFrame {
    fn new(
        sequence: u64,
        source_width: u16,
        source_height: u16,
        image: RgbImage,
        pool: SharedDecodeBufferPool,
    ) -> Self {
        Self {
            sequence,
            source_width,
            source_height,
            image: Some(image),
            pool,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_decoded(frame: DecodedFrame, pool: SharedDecodeBufferPool) -> Self {
        Self::new(
            frame.sequence,
            frame.source_width,
            frame.source_height,
            frame.image,
            pool,
        )
    }

    #[must_use]
    pub(crate) fn sequence(&self) -> u64 {
        self.sequence
    }

    #[must_use]
    pub(crate) fn source_width(&self) -> u16 {
        self.source_width
    }

    #[must_use]
    pub(crate) fn source_height(&self) -> u16 {
        self.source_height
    }

    #[must_use]
    pub(crate) fn image(&self) -> &RgbImage {
        self.image
            .as_ref()
            .expect("recyclable decoded frame always owns its image before Drop")
    }

    fn into_decoded(mut self) -> DecodedFrame {
        DecodedFrame {
            sequence: self.sequence,
            source_width: self.source_width,
            source_height: self.source_height,
            image: self
                .image
                .take()
                .expect("recyclable decoded frame always owns its image before conversion"),
        }
    }
}

impl Drop for RecyclableDecodedFrame {
    fn drop(&mut self) {
        if let Some(image) = self.image.take() {
            recycle_decode_buffer(&self.pool, image.into_raw());
        }
    }
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
    decode_frame_recyclable(payload, new_decode_buffer_pool())
        .map(RecyclableDecodedFrame::into_decoded)
}

pub(crate) fn decode_frame_recyclable(
    payload: &[u8],
    pool: SharedDecodeBufferPool,
) -> RemoteResult<RecyclableDecodedFrame> {
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
    let mut decoder = JpegDecoder::new(Cursor::new(&payload[FRAME_HEADER_LEN..]))?;
    let expected_dimensions = (u32::from(encoded_width), u32::from(encoded_height));
    if decoder.dimensions() != expected_dimensions {
        return Err(
            invalid_data("JPEG dimensions do not match the authenticated frame header").into(),
        );
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(u32::from(encoded_width));
    limits.max_image_height = Some(u32::from(encoded_height));
    limits.max_alloc = Some(allocation);
    decoder.set_limits(limits)?;

    let image = if decoder.color_type() == ColorType::Rgb8 {
        let expected_len_u64 = u64::from(encoded_width)
            .checked_mul(u64::from(encoded_height))
            .and_then(|pixels| pixels.checked_mul(3))
            .ok_or_else(|| invalid_data("decoded RGB frame length overflow"))?;
        if decoder.total_bytes() != expected_len_u64 {
            return Err(invalid_data("JPEG RGB byte length does not match its dimensions").into());
        }
        let expected_len = usize::try_from(expected_len_u64)
            .map_err(|_| invalid_data("decoded RGB frame does not fit in memory"))?;
        decode_rgb8_into_pool(
            u32::from(encoded_width),
            u32::from(encoded_height),
            expected_len,
            &pool,
            |pixels| decoder.read_image(pixels),
        )?
    } else {
        // Grayscale and other JPEG colorspaces are rare for screen frames but
        // were accepted by the previous ImageReader path. Keep that behavior;
        // this decoder already carries the same allocation/dimension limits.
        DynamicImage::from_decoder(decoder)?.into_rgb8()
    };
    if image.dimensions() != expected_dimensions {
        return Err(
            invalid_data("JPEG dimensions do not match the authenticated frame header").into(),
        );
    }

    Ok(RecyclableDecodedFrame::new(
        sequence,
        source_width,
        source_height,
        image,
        pool,
    ))
}

fn decode_rgb8_into_pool(
    width: u32,
    height: u32,
    expected_len: usize,
    pool: &SharedDecodeBufferPool,
    decode: impl FnOnce(&mut [u8]) -> image::ImageResult<()>,
) -> RemoteResult<RgbImage> {
    let dimensions_len = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(3))
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or_else(|| invalid_data("decoded RGB frame length overflow"))?;
    if expected_len != dimensions_len {
        return Err(invalid_data(
            "decoded RGB byte length does not match the authenticated dimensions",
        )
        .into());
    }
    let mut pixels = prepare_decode_buffer(pool, expected_len)?;
    pixels.resize(expected_len, 0);
    if let Err(error) = decode(&mut pixels) {
        // A decoder can fail after partially filling the destination. The
        // buffer remains private in the pool; no frame publishes those bytes,
        // and a later successful read_image overwrites its complete slice.
        recycle_decode_buffer(pool, pixels);
        return Err(error.into());
    }
    RgbImage::from_raw(width, height, pixels).ok_or_else(|| {
        invalid_data("decoded RGB bytes do not match the authenticated frame dimensions").into()
    })
}

fn prepare_decode_buffer(
    pool: &SharedDecodeBufferPool,
    expected_len: usize,
) -> RemoteResult<Vec<u8>> {
    let mut pixels = take_decode_buffer(pool, expected_len);
    if pixels.capacity() < expected_len
        // `try_reserve_exact` is relative to Vec::len(), not its capacity. A
        // retained buffer can have a shorter previous-frame length, so reserve
        // exactly the additional elements needed from that logical length.
        && let Err(error) = pixels.try_reserve_exact(expected_len.saturating_sub(pixels.len()))
    {
        recycle_decode_buffer(pool, pixels);
        return Err(invalid_data(format!(
            "cannot allocate {expected_len} bytes for a decoded RGB frame: {error}"
        ))
        .into());
    }
    Ok(pixels)
}

fn take_decode_buffer(pool: &SharedDecodeBufferPool, required_len: usize) -> Vec<u8> {
    if required_len > DECODE_POOL_MAX_SLOT_BYTES {
        return Vec::new();
    }

    let mut pool = lock_decode_pool(pool);
    let fitting = pool
        .buffers
        .iter()
        .enumerate()
        .filter(|(_, buffer)| buffer.capacity() >= required_len)
        .min_by_key(|(_, buffer)| buffer.capacity())
        .map(|(index, _)| index);
    // When every retained allocation is too small, grow the largest one and
    // leave the other slot available for a genuinely smaller frame size.
    let selected = fitting.or_else(|| {
        pool.buffers
            .iter()
            .enumerate()
            .max_by_key(|(_, buffer)| buffer.capacity())
            .map(|(index, _)| index)
    });
    let Some(index) = selected else {
        return Vec::new();
    };
    let buffer = pool.buffers.swap_remove(index);
    pool.retained_bytes = pool.retained_bytes.saturating_sub(buffer.capacity());
    buffer
}

fn recycle_decode_buffer(pool: &SharedDecodeBufferPool, buffer: Vec<u8>) {
    let capacity = buffer.capacity();
    if capacity == 0 || capacity > DECODE_POOL_MAX_SLOT_BYTES {
        return;
    }

    // Keep any rejected allocation outside the guard so releasing tens of
    // MiB never happens while another decoder is waiting on the pool mutex.
    let mut rejected = Some(buffer);
    {
        let mut pool = lock_decode_pool(pool);
        let fits_slots = pool.buffers.len() < DECODE_POOL_MAX_SLOTS;
        let fits_bytes = pool
            .retained_bytes
            .checked_add(capacity)
            .is_some_and(|bytes| bytes <= DECODE_POOL_MAX_BYTES);
        if fits_slots && fits_bytes {
            pool.retained_bytes += capacity;
            pool.buffers.push(
                rejected
                    .take()
                    .expect("accepted decode buffer must still be available"),
            );
        }
    }
    drop(rejected);
}

fn lock_decode_pool(pool: &SharedDecodeBufferPool) -> MutexGuard<'_, DecodeBufferPool> {
    match pool.lock() {
        Ok(pool) => pool,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
pub(crate) fn decode_pool_snapshot(pool: &SharedDecodeBufferPool) -> (usize, usize, Vec<usize>) {
    let pool = lock_decode_pool(pool);
    let mut capacities = pool.buffers.iter().map(Vec::capacity).collect::<Vec<_>>();
    capacities.sort_unstable();
    (pool.buffers.len(), pool.retained_bytes, capacities)
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
    use std::panic::{AssertUnwindSafe, catch_unwind};

    fn encoded_test_payload(sequence: u64, width: u32, height: u32) -> Vec<u8> {
        encode_frame(
            sequence,
            &CapturedFrame {
                image: RgbImage::from_fn(width, height, |x, y| {
                    Rgb([
                        (x.wrapping_mul(31) ^ y) as u8,
                        (y.wrapping_mul(17) ^ x) as u8,
                        x.wrapping_add(y.wrapping_mul(3)) as u8,
                    ])
                }),
                source_width: u16::try_from(width).unwrap(),
                source_height: u16::try_from(height).unwrap(),
            },
            85,
        )
        .unwrap()
    }

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
    fn header_jpeg_mismatch_is_rejected_before_pool_checkout() {
        let pool = new_decode_buffer_pool();
        let valid = encoded_test_payload(1, 64, 32);
        let decoded = decode_frame_recyclable(&valid, Arc::clone(&pool)).unwrap();
        let pointer = decoded.image().as_raw().as_ptr();
        let capacity = decoded.image().as_raw().capacity();
        drop(decoded);

        let mut mismatch = valid.clone();
        mismatch[12..14].copy_from_slice(&63_u16.to_be_bytes());
        assert!(decode_frame_recyclable(&mismatch, Arc::clone(&pool)).is_err());
        assert_eq!(decode_pool_snapshot(&pool), (1, capacity, vec![capacity]));

        let decoded = decode_frame_recyclable(&valid, Arc::clone(&pool)).unwrap();
        assert_eq!(decoded.image().as_raw().as_ptr(), pointer);
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

    #[test]
    fn rgb_decode_reuses_the_same_allocation_after_frame_drop() {
        let pool = new_decode_buffer_pool();
        let payload = encoded_test_payload(1, 320, 180);
        let first = decode_frame_recyclable(&payload, Arc::clone(&pool)).unwrap();
        let pointer = first.image().as_raw().as_ptr();
        let capacity = first.image().as_raw().capacity();
        assert_eq!(decode_pool_snapshot(&pool).0, 0);

        drop(first);
        assert_eq!(decode_pool_snapshot(&pool), (1, capacity, vec![capacity]));

        let second = decode_frame_recyclable(&payload, Arc::clone(&pool)).unwrap();
        assert_eq!(second.image().as_raw().as_ptr(), pointer);
        assert_eq!(second.image().as_raw().capacity(), capacity);
        assert_eq!(decode_pool_snapshot(&pool).0, 0);
    }

    #[test]
    fn simultaneously_live_frames_never_share_rgb_storage() {
        let pool = new_decode_buffer_pool();
        let payload = encoded_test_payload(2, 160, 90);
        let first = decode_frame_recyclable(&payload, Arc::clone(&pool)).unwrap();
        let second = decode_frame_recyclable(&payload, Arc::clone(&pool)).unwrap();

        assert_ne!(
            first.image().as_raw().as_ptr(),
            second.image().as_raw().as_ptr()
        );
        assert_eq!(decode_pool_snapshot(&pool).0, 0);
        drop(first);
        drop(second);
        assert_eq!(decode_pool_snapshot(&pool).0, 2);
    }

    #[test]
    fn pool_selects_the_smallest_fitting_buffer_and_stays_bounded() {
        let pool = new_decode_buffer_pool();
        recycle_decode_buffer(&pool, Vec::with_capacity(4_096));
        recycle_decode_buffer(&pool, Vec::with_capacity(1_024));
        assert_eq!(decode_pool_snapshot(&pool).0, DECODE_POOL_MAX_SLOTS);

        let small = take_decode_buffer(&pool, 800);
        assert_eq!(small.capacity(), 1_024);
        let large = take_decode_buffer(&pool, 2_000);
        assert_eq!(large.capacity(), 4_096);
        recycle_decode_buffer(&pool, small);
        recycle_decode_buffer(&pool, large);

        // A third free allocation and any exceptional single allocation are
        // released instead of making the free list or its byte total grow.
        recycle_decode_buffer(&pool, Vec::with_capacity(2_048));
        recycle_decode_buffer(
            &pool,
            Vec::with_capacity(DECODE_POOL_MAX_SLOT_BYTES.saturating_add(1)),
        );
        let (slots, bytes, capacities) = decode_pool_snapshot(&pool);
        assert_eq!(slots, DECODE_POOL_MAX_SLOTS);
        assert!(bytes <= DECODE_POOL_MAX_BYTES);
        assert!(
            capacities
                .iter()
                .all(|capacity| *capacity <= DECODE_POOL_MAX_SLOT_BYTES)
        );
    }

    #[test]
    fn poisoned_pool_recovers_for_recycle_and_checkout() {
        let pool = new_decode_buffer_pool();
        let poisoned = Arc::clone(&pool);
        assert!(
            catch_unwind(AssertUnwindSafe(move || {
                let guard = poisoned.lock().unwrap();
                assert!(guard.buffers.is_empty());
                panic!("synthetic decode-pool poison");
            }))
            .is_err()
        );

        recycle_decode_buffer(&pool, Vec::with_capacity(2_048));
        assert_eq!(decode_pool_snapshot(&pool).0, 1);
        assert!(take_decode_buffer(&pool, 1_024).capacity() >= 2_048);
        assert_eq!(decode_pool_snapshot(&pool).0, 0);
    }

    #[test]
    fn grayscale_jpeg_keeps_the_compatible_rgb_fallback() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&41_u64.to_be_bytes());
        payload.extend_from_slice(&8_u16.to_be_bytes());
        payload.extend_from_slice(&4_u16.to_be_bytes());
        payload.extend_from_slice(&8_u16.to_be_bytes());
        payload.extend_from_slice(&4_u16.to_be_bytes());
        let luminance = (0_u8..32).collect::<Vec<_>>();
        JpegEncoder::new_with_quality(&mut payload, 90)
            .encode(&luminance, 8, 4, ExtendedColorType::L8)
            .unwrap();

        let decoded = decode_frame_recyclable(&payload, new_decode_buffer_pool()).unwrap();
        assert_eq!(decoded.sequence(), 41);
        assert_eq!(decoded.image().dimensions(), (8, 4));
        assert!(decoded.image().pixels().all(|pixel| {
            let [red, green, blue] = pixel.0;
            red == green && green == blue
        }));
    }

    #[test]
    fn failed_rgb_decode_remains_private_and_returns_its_checked_out_pool_slot() {
        let pool = new_decode_buffer_pool();
        let valid = encoded_test_payload(7, 128, 72);
        let first = decode_frame_recyclable(&valid, Arc::clone(&pool)).unwrap();
        let capacity = first.image().as_raw().capacity();
        drop(first);

        let expected_len = 128 * 72 * 3;
        assert!(
            decode_rgb8_into_pool(128, 72, expected_len, &pool, |pixels| {
                pixels.fill(0xa5);
                Err(image::ImageError::IoError(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "synthetic partial JPEG",
                )))
            })
            .is_err()
        );

        let (slots, bytes, capacities) = decode_pool_snapshot(&pool);
        assert_eq!((slots, bytes), (1, capacity));
        assert_eq!(capacities, [capacity]);
        let partial = take_decode_buffer(&pool, expected_len);
        assert_eq!(partial.len(), expected_len);
        assert!(partial.iter().all(|byte| *byte == 0xa5));
        recycle_decode_buffer(&pool, partial);
        let recovered =
            decode_frame_recyclable(&encoded_test_payload(8, 128, 72), Arc::clone(&pool)).unwrap();
        assert_eq!(recovered.image().as_raw().capacity(), capacity);
    }

    #[test]
    fn same_size_reuse_skips_zero_fill_and_growth_initializes_only_the_delta() {
        let pool = new_decode_buffer_pool();
        let seeded = vec![0x5a; 24];
        let pointer = seeded.as_ptr();
        recycle_decode_buffer(&pool, seeded);

        let same_size = decode_rgb8_into_pool(4, 2, 24, &pool, |pixels| {
            assert!(pixels.iter().all(|byte| *byte == 0x5a));
            pixels.fill(0x11);
            Ok(())
        })
        .unwrap();
        assert_eq!(same_size.as_raw().as_ptr(), pointer);

        let mut shorter = same_size.into_raw();
        shorter.truncate(12);
        shorter.fill(0x33);
        recycle_decode_buffer(&pool, shorter);
        let grown = decode_rgb8_into_pool(4, 2, 24, &pool, |pixels| {
            assert!(pixels[..12].iter().all(|byte| *byte == 0x33));
            assert!(pixels[12..].iter().all(|byte| *byte == 0));
            pixels.fill(0x22);
            Ok(())
        })
        .unwrap();
        assert_eq!(grown.as_raw().as_ptr(), pointer);
    }

    #[test]
    fn impossible_rgb_allocation_fails_without_running_decoder_or_leaking_pool_state() {
        let pool = new_decode_buffer_pool();
        recycle_decode_buffer(&pool, Vec::with_capacity(1_024));
        let before = decode_pool_snapshot(&pool);
        let result = prepare_decode_buffer(&pool, usize::MAX);

        assert!(result.is_err());
        assert_eq!(decode_pool_snapshot(&pool), before);
    }
}
