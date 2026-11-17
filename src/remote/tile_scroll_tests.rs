//! Offline production-codec regressions; no display, socket or graphics context.
use super::*;
use crate::remote::frame::new_decode_buffer_pool;
use image::Rgb;

fn source(width: u32, height: u32) -> CapturedFrame {
    CapturedFrame {
        image: RgbImage::from_fn(width, height, |x, y| {
            let value = (x.wrapping_mul(37) ^ y.wrapping_mul(101) ^ (x * y)) as u8;
            Rgb([value, value.wrapping_add(53), value.wrapping_add(151)])
        }),
        geometry_epoch: 3,
        source_width: width as u16,
        source_height: height as u16,
    }
}

fn shifted(previous: &CapturedFrame, dy: i16, salt: u8) -> CapturedFrame {
    let mut next = source(previous.image.width(), previous.image.height());
    next.geometry_epoch = previous.geometry_epoch;
    next.source_width = previous.source_width;
    next.source_height = previous.source_height;
    for (x, y, pixel) in next.image.enumerate_pixels_mut() {
        let from = i64::from(y) + i64::from(dy);
        *pixel = if (0..i64::from(previous.image.height())).contains(&from) {
            *previous.image.get_pixel(x, from as u32)
        } else {
            Rgb([salt, salt.wrapping_add(x as u8), (y as u8).wrapping_mul(17)])
        };
    }
    next
}

fn encode_frame(
    encoder: &mut TileEncoder,
    frame: &CapturedFrame,
    sequence: u64,
    quality: u8,
) -> Vec<u8> {
    assert!(encoder.plan(frame, TileEncodeRequest::Delta).unwrap().emit);
    let mut payload = Vec::new();
    encoder
        .encode_into(&mut payload, sequence, frame, quality)
        .unwrap();
    payload
}

fn pair() -> (TileEncoder, TileDecoder) {
    let mut encoder = TileEncoder::new();
    encoder.set_scroll_copy_enabled(true);
    let mut decoder = TileDecoder::new();
    decoder.set_scroll_copy_enabled(true);
    (encoder, decoder)
}

fn scroll_dy(payload: &[u8]) -> i16 {
    assert_eq!(
        payload[17] & 2,
        2,
        "fixture must exercise production scroll detection"
    );
    assert_eq!(payload[17] & 1, 0);
    i16::from_be_bytes(payload[40..42].try_into().unwrap())
}

#[track_caller]
fn assert_rejected_unchanged(decoder: &mut TileDecoder, payload: &[u8]) {
    let image = decoder.canvas().cloned();
    let sequence = decoder.last_sequence;
    let epoch = decoder.geometry_epoch;
    let source = decoder.source;
    let result = decoder.decode_into(payload, new_decode_buffer_pool());
    assert!(
        result.is_err(),
        "malformed record accepted: len={} header={:?} prior_sequence={sequence:?} prior_epoch={epoch} prior_source={source:?}",
        payload.len(),
        &payload[..payload.len().min(42)]
    );

    assert_eq!(decoder.canvas(), image.as_ref());
    assert_eq!(decoder.last_sequence, sequence);
    assert_eq!(decoder.geometry_epoch, epoch);
    assert_eq!(decoder.source, source);
}

#[test]
fn scroll_copy_both_directions_odd_edges_preserve_decoded_overlap_and_snapshots() {
    for dy in [-16, -7, 7, 16] {
        let (mut encoder, mut decoder) = pair();
        let first = source(65, 97);
        let initial = encode_frame(&mut encoder, &first, 1, 37);
        let old = decoder
            .decode_into(&initial, new_decode_buffer_pool())
            .unwrap();
        // Do not materialize `old`: its first image() after the copy must
        // reconstruct from retained immutable tiles, not a cached RGB image.
        let old_pixels = decoder.canvas().unwrap().clone();
        encoder.commit(&first);
        let next = shifted(&first, dy, 231);
        let payload = encode_frame(&mut encoder, &next, 2, 37);
        assert_eq!(scroll_dy(&payload), dy);
        assert_eq!(u64::from_be_bytes(payload[32..40].try_into().unwrap()), 1);
        let current = decoder
            .decode_into(&payload, new_decode_buffer_pool())
            .unwrap();
        assert!(
            current.damage().is_none(),
            "copy must force full immutable snapshot upload"
        );
        assert_eq!(current.image().dimensions(), (65, 97));
        let mut copied = 0;
        for y in 0..97_u32 {
            for x in 0..65_u32 {
                let tile = (y / 16 * 65_u32.div_ceil(16) + x / 16) as usize;
                if payload[46 + tile / 8] & (1 << (tile % 8)) == 0 {
                    let from = i32::try_from(y).unwrap() + i32::from(dy);
                    assert!(
                        (0..97).contains(&from),
                        "exposed rows must be supplied by atlas"
                    );
                    assert_eq!(
                        current.image().get_pixel(x, y),
                        old_pixels.get_pixel(x, from as u32)
                    );
                    copied += 1;
                }
            }
        }
        assert!(copied > 0);
        assert_eq!(
            old.image(),
            &old_pixels,
            "decoder must not mutate retained previous snapshot"
        );
        encoder.commit(&next);
    }
}

#[test]
fn scroll_copy_malformed_extension_and_jpeg_leave_decoder_unchanged() {
    let (mut encoder, mut decoder) = pair();
    let first = source(65, 97);
    let initial = encode_frame(&mut encoder, &first, 1, 65);
    decoder
        .decode_into(&initial, new_decode_buffer_pool())
        .unwrap();
    encoder.commit(&first);
    let next = shifted(&first, 7, 219);
    let valid = encode_frame(&mut encoder, &next, 2, 65);
    assert_eq!(scroll_dy(&valid), 7);
    for base in [0_u64, 2, u64::MAX] {
        let mut bad = valid.clone();
        eprintln!("scroll malformed case: base={base}");
        bad[32..40].copy_from_slice(&base.to_be_bytes());
        assert_rejected_unchanged(&mut decoder, &bad);
    }
    for dy in [0_i16, -65, 65, i16::MIN, i16::MAX] {
        let mut bad = valid.clone();
        eprintln!("scroll malformed case: dy={dy}");
        bad[40..42].copy_from_slice(&dy.to_be_bytes());
        assert_rejected_unchanged(&mut decoder, &bad);
    }
    for end in [32, 39, 41, 42, valid.len() / 2] {
        eprintln!(
            "scroll malformed case: truncation end={end}, full_len={}",
            valid.len()
        );
        assert_rejected_unchanged(&mut decoder, &valid[..end]);
    }
    assert_eq!(&valid[valid.len() - 2..], &[0xff, 0xd9]);
    eprintln!("scroll malformed case: missing terminal EOI");
    assert_rejected_unchanged(&mut decoder, &valid[..valid.len() - 2]);
    let mut trailing = valid.clone();
    trailing.extend_from_slice(b"trailing garbage");
    eprintln!("scroll malformed case: bytes after terminal EOI");
    assert_rejected_unchanged(&mut decoder, &trailing);
    for offset in [8_usize, 12, 24] {
        let mut bad = valid.clone();
        eprintln!("scroll malformed case: scope offset={offset}");
        bad[offset] ^= 1;
        assert_rejected_unchanged(&mut decoder, &bad);
    }
    let mut bad = valid.clone();
    bad[17] |= 1;
    assert_rejected_unchanged(&mut decoder, &bad);
    let mut bad = valid.clone();
    bad[..8].copy_from_slice(&1_u64.to_be_bytes());
    assert_rejected_unchanged(&mut decoder, &bad);
    let recovered = decoder
        .decode_into(&valid, new_decode_buffer_pool())
        .unwrap();
    assert_eq!(recovered.sequence(), 2);
    assert_eq!(decoder.last_sequence, Some(2));
    assert_eq!(decoder.geometry_epoch, next.geometry_epoch);
    assert_eq!(decoder.source, (next.source_width, next.source_height));
    assert_rejected_unchanged(&mut decoder, &valid); // replay
    let mut unnegotiated = TileDecoder::new();
    unnegotiated
        .decode_into(&initial, new_decode_buffer_pool())
        .unwrap();
    assert_rejected_unchanged(&mut unnegotiated, &valid);
}

#[test]
fn scroll_copy_discard_does_not_advance_base_or_reference_and_toolbar_uses_bounded_copy() {
    let (mut encoder, mut decoder) = pair();
    let first = source(65, 97);
    let initial = encode_frame(&mut encoder, &first, 1, 40);
    decoder
        .decode_into(&initial, new_decode_buffer_pool())
        .unwrap();
    encoder.commit(&first);
    let next = shifted(&first, 7, 204);
    let discarded = encode_frame(&mut encoder, &next, 2, 40);
    assert_eq!(scroll_dy(&discarded), 7);
    encoder.discard();
    assert_eq!(encoder.reference(), Some(&first.image));
    let retry = encode_frame(&mut encoder, &next, 3, 40);
    assert_eq!(scroll_dy(&retry), 7);
    assert_eq!(u64::from_be_bytes(retry[32..40].try_into().unwrap()), 1);
    decoder
        .decode_into(&retry, new_decode_buffer_pool())
        .unwrap();
    encoder.discard();
    let mut toolbar = next;
    for x in 0..65 {
        toolbar.image.put_pixel(x, 0, *first.image.get_pixel(x, 0));
    }
    let fallback = encode_frame(&mut encoder, &toolbar, 4, 40);
    assert_eq!(
        fallback[17] & 2,
        2,
        "fixed toolbar permits a bounded full-width overlap"
    );
}

#[test]
fn scroll_copy_resize_source_and_reused_size_force_keyframes() {
    let (mut encoder, mut decoder) = pair();
    let mut frame = source(65, 97);
    for sequence in 1..=4 {
        if sequence == 2 {
            frame = source(49, 81);
            frame.geometry_epoch = 4;
        }
        if sequence == 3 {
            frame = source(65, 97);
            frame.geometry_epoch = 5;
        }
        if sequence == 4 {
            frame.source_width = 130;
        }
        let payload = encode_frame(&mut encoder, &frame, sequence, 65);
        assert_eq!(payload[17] & 3, 1);
        decoder
            .decode_into(&payload, new_decode_buffer_pool())
            .unwrap();
        encoder.commit(&frame);
    }
}

#[test]
fn scroll_copy_repeated_moves_keep_immutable_history_and_quality_debt_until_repair() {
    let (mut encoder, mut decoder) = pair();
    let mut frame = source(65, 97);
    let first = encode_frame(&mut encoder, &frame, 1, 30);
    let retained = decoder
        .decode_into(&first, new_decode_buffer_pool())
        .unwrap();
    // Keep this snapshot unmaterialized through every copy and the repair.
    let original = decoder.canvas().unwrap().clone();
    encoder.commit(&frame);
    for sequence in 2..=12 {
        let old = decoder.canvas().unwrap().clone();
        let next = shifted(&frame, 7, sequence as u8 * 17);
        let payload = encode_frame(&mut encoder, &next, sequence, 30);
        assert_eq!(scroll_dy(&payload), 7);
        let decoded = decoder
            .decode_into(&payload, new_decode_buffer_pool())
            .unwrap();
        // This row lies in a copied tile, not a partially exposed tile. Compare
        // decoded pixels, not the original RGB: no second JPEG quantization.
        assert_eq!(decoded.image().get_pixel(20, 16), old.get_pixel(20, 23));
        assert!(decoded.damage().is_none());
        encoder.commit(&next);
        assert!(encoder.has_quality_debt(90));
        frame = next;
        // Intentionally skip all intermediate presentations of the retained
        // snapshot. Materialize it for the first time only after final repair.
    }
    let plan = encoder
        .plan_with_refinement(&frame, TileEncodeRequest::Delta, Some((90, 10_000)))
        .unwrap();
    assert!(plan.emit);
    let mut repair = Vec::new();
    encoder.encode_into(&mut repair, 13, &frame, 90).unwrap();
    decoder
        .decode_into(&repair, new_decode_buffer_pool())
        .unwrap();
    encoder.commit(&frame);
    assert!(!encoder.has_quality_debt(90));
    assert_eq!(retained.image(), &original);
}

#[test]
fn scroll_copy_requires_opt_in_and_complete_exposed_strip() {
    let first = source(65, 145);
    let next = shifted(&first, 64, 221);
    let mut disabled = TileEncoder::new();
    encode_frame(&mut disabled, &first, 1, 60);
    disabled.commit(&first);
    let plain = encode_frame(&mut disabled, &next, 2, 60);
    assert_eq!(plain[17] & 2, 0);
    let (mut encoder, mut decoder) = pair();
    let initial = encode_frame(&mut encoder, &first, 1, 60);
    decoder
        .decode_into(&initial, new_decode_buffer_pool())
        .unwrap();
    encoder.commit(&first);
    let valid = encode_frame(&mut encoder, &next, 2, 60);
    assert_eq!(scroll_dy(&valid), 64);
    // Clear one exposed tile and adjust the count so bitmap accounting still
    // matches: rejection must enforce exposed coverage, not merely popcount.
    let mut bad = valid.clone();
    let tiles = 65_usize.div_ceil(16) * 145_usize.div_ceil(16);
    let tile = (0..tiles)
        .find(|i| bad[46 + i / 8] & (1 << (i % 8)) != 0)
        .unwrap();
    bad[46 + tile / 8] &= !(1 << (tile % 8));
    let count = u32::from_be_bytes(bad[20..24].try_into().unwrap());
    bad[20..24].copy_from_slice(&(count - 1).to_be_bytes());
    assert_rejected_unchanged(&mut decoder, &bad);
    decoder
        .decode_into(&valid, new_decode_buffer_pool())
        .unwrap();
}

fn band_shifted(previous: &CapturedFrame, dy: i16, top: u32, bottom: u32) -> CapturedFrame {
    let mut next = CapturedFrame {
        image: previous.image.clone(),
        geometry_epoch: previous.geometry_epoch,
        source_width: previous.source_width,
        source_height: previous.source_height,
    };
    for y in top..bottom {
        let from = i64::from(y) + i64::from(dy);
        for x in 0..previous.image.width() {
            let pixel = if (i64::from(top)..i64::from(bottom)).contains(&from) {
                *previous.image.get_pixel(x, from as u32)
            } else {
                Rgb([231, x as u8, y as u8])
            };
            next.image.put_pixel(x, y, pixel);
        }
    }
    next
}

fn band_bounds(payload: &[u8]) -> (u32, u32) {
    (
        u16::from_be_bytes(payload[42..44].try_into().unwrap()) as u32,
        u16::from_be_bytes(payload[44..46].try_into().unwrap()) as u32,
    )
}

#[test]
fn scroll_band_odd_edges_headers_footers_and_unmaterialized_history() {
    for dy in [-16, -7, 7, 16] {
        let (mut encoder, mut decoder) = pair();
        let first = source(97, 195);
        let initial = encode_frame(&mut encoder, &first, 1, 37);
        let retained = decoder
            .decode_into(&initial, new_decode_buffer_pool())
            .unwrap();
        let old = decoder.canvas().unwrap().clone();
        encoder.commit(&first);
        let next = band_shifted(&first, dy, 17, 178);
        let payload = encode_frame(&mut encoder, &next, 2, 37);
        assert_eq!(scroll_dy(&payload), dy);
        let (top, bottom) = band_bounds(&payload);
        assert!(top < bottom && bottom <= 195);
        let decoded = decoder
            .decode_into(&payload, new_decode_buffer_pool())
            .unwrap();
        assert!(decoded.damage().is_none());
        let mut copied = 0;
        let mut outside = 0;
        for y in 0..195 {
            for x in 0..97 {
                let tile = (y / 16 * 97_u32.div_ceil(16) + x / 16) as usize;
                if payload[46 + tile / 8] & (1 << (tile % 8)) != 0 {
                    continue;
                }
                let from = i64::from(y) + i64::from(dy);
                let copy = (top..bottom).contains(&y)
                    && (i64::from(top)..i64::from(bottom)).contains(&from);
                let expected_y = if copy {
                    copied += 1;
                    from as u32
                } else {
                    outside += 1;
                    y
                };
                assert_eq!(
                    decoded.image().get_pixel(x, y),
                    old.get_pixel(x, expected_y),
                    "dy={dy} at ({x},{y}), band={top}..{bottom}"
                );
            }
        }
        assert!(copied > 0 && outside > 0);
        assert_eq!(retained.image(), &old);
        encoder.discard();
        assert_eq!(encoder.reference(), Some(&first.image));
        let retry = encode_frame(&mut encoder, &next, 3, 37);
        assert_eq!(u64::from_be_bytes(retry[32..40].try_into().unwrap()), 1);
        assert_eq!(band_bounds(&retry), (top, bottom));
    }
}

#[test]
fn scroll_band_malformed_bounds_are_transactional() {
    let (mut encoder, mut decoder) = pair();
    let first = source(97, 195);
    let initial = encode_frame(&mut encoder, &first, 1, 60);
    decoder
        .decode_into(&initial, new_decode_buffer_pool())
        .unwrap();
    encoder.commit(&first);
    let next = band_shifted(&first, 7, 17, 178);
    let valid = encode_frame(&mut encoder, &next, 2, 60);
    assert_eq!(scroll_dy(&valid), 7);
    for (top, bottom) in [
        (0_u16, 0_u16),
        (178, 17),
        (17, 17),
        (0, 196),
        (0, u16::MAX),
        (194, 195),
        (17, 24),
        (17, 114),
    ] {
        let mut bad = valid.clone();
        bad[42..44].copy_from_slice(&top.to_be_bytes());
        bad[44..46].copy_from_slice(&bottom.to_be_bytes());
        assert_rejected_unchanged(&mut decoder, &bad);
    }
    // Remove an actually exposed boundary tile while keeping popcount valid.
    // Decoder must reject before changing its base canvas, not merely because
    // the original dirty-count field disagreed with the altered bitmap.
    let mut missing = valid.clone();
    let (top, bottom) = band_bounds(&valid);
    let exposed_row = bottom - 1;
    assert!(exposed_row >= top);
    let tile = (exposed_row / 16 * 97_u32.div_ceil(16)) as usize;
    assert_ne!(missing[46 + tile / 8] & (1 << (tile % 8)), 0);
    missing[46 + tile / 8] &= !(1 << (tile % 8));
    let count = u32::from_be_bytes(missing[20..24].try_into().unwrap());
    missing[20..24].copy_from_slice(&(count - 1).to_be_bytes());
    assert_rejected_unchanged(&mut decoder, &missing);
    for end in [42, 43, 44, 45, 46, valid.len() / 2] {
        assert_rejected_unchanged(&mut decoder, &valid[..end]);
    }
    decoder
        .decode_into(&valid, new_decode_buffer_pool())
        .unwrap();
    assert_eq!(decoder.last_sequence, Some(2));
}

#[test]
fn scroll_band_fragmented_fixed_rows_do_not_claim_large_overlap() {
    let (mut encoder, _) = pair();
    let first = source(97, 195);
    encode_frame(&mut encoder, &first, 1, 60);
    encoder.commit(&first);
    let mut next = shifted(&first, 7, 231);
    for y in (0..195).step_by(8) {
        for x in 0..97 {
            next.image.put_pixel(x, y, *first.image.get_pixel(x, y));
        }
    }
    let payload = encode_frame(&mut encoder, &next, 2, 60);
    assert_eq!(
        payload[17] & 2,
        0,
        "no contiguous half-image overlap exists"
    );
}

#[test]
fn scroll_band_touching_image_edges_preserves_outside_dirty_updates() {
    for (top, bottom, dy, outside_y) in [(0, 178, 7, 194), (17, 195, -7, 0)] {
        let (mut encoder, mut decoder) = pair();
        let first = source(97, 195);
        let initial = encode_frame(&mut encoder, &first, 1, 60);
        decoder
            .decode_into(&initial, new_decode_buffer_pool())
            .unwrap();
        encoder.commit(&first);
        let mut next = band_shifted(&first, dy, top, bottom);
        for x in 0..97 {
            next.image.put_pixel(x, outside_y, Rgb([250, 10, 90]));
        }
        let payload = encode_frame(&mut encoder, &next, 2, 60);
        assert_eq!(scroll_dy(&payload), dy);
        let outside_tile = (outside_y / 16 * 97_u32.div_ceil(16)) as usize;
        assert_ne!(
            payload[46 + outside_tile / 8] & (1 << (outside_tile % 8)),
            0,
            "ordinary changes outside band cannot be omitted"
        );
        decoder
            .decode_into(&payload, new_decode_buffer_pool())
            .unwrap();
        encoder.commit(&next);
        assert_eq!(encoder.reference(), Some(&next.image));
        assert!(encoder.has_quality_debt(90));
    }
}
