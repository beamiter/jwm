//! Pure production codec/decoded-canvas regressions; no display or transport.
use super::*;
use crate::remote::frame::new_decode_buffer_pool;
use image::Rgb;

fn frame() -> CapturedFrame {
    CapturedFrame {
        image: RgbImage::from_fn(65, 97, |x, y| {
            let v = (x * 37 ^ y * 101 ^ x * y) as u8;
            Rgb([v, v.wrapping_add(53), v.wrapping_add(151)])
        }),
        geometry_epoch: 3,
        source_width: 65,
        source_height: 97,
    }
}
fn clone_frame(f: &CapturedFrame) -> CapturedFrame {
    CapturedFrame {
        image: f.image.clone(),
        geometry_epoch: f.geometry_epoch,
        source_width: f.source_width,
        source_height: f.source_height,
    }
}
fn shifted(old: &CapturedFrame, dy: i16) -> CapturedFrame {
    let mut next = clone_frame(old);
    for (x, y, p) in next.image.enumerate_pixels_mut() {
        let from = i64::from(y) + i64::from(dy);
        *p = if (0..97).contains(&from) {
            *old.image.get_pixel(x, from as u32)
        } else {
            Rgb([231, x as u8, (y * 17) as u8])
        };
    }
    next
}
fn send(
    e: &mut TileEncoder,
    d: &mut TileDecoder,
    f: &CapturedFrame,
    seq: u64,
    at: Instant,
    request: TileEncodeRequest,
) -> Vec<u8> {
    assert!(e.plan_at(f, request, at).unwrap().emit);
    let mut bytes = Vec::new();
    e.encode_into(&mut bytes, seq, f, 75).unwrap();
    d.decode_into(&bytes, new_decode_buffer_pool()).unwrap();
    e.commit(f);
    bytes
}
fn exact_pair() -> (TileEncoder, TileDecoder, CapturedFrame, Instant) {
    let mut e = TileEncoder::new();
    e.set_lossless_enabled(true);
    e.set_scroll_copy_enabled(true);
    let mut d = TileDecoder::new();
    d.set_lossless_enabled(true);
    d.set_scroll_copy_enabled(true);
    let f = frame();
    let at = Instant::now();
    send(&mut e, &mut d, &f, 1, at, TileEncodeRequest::Delta);
    assert!(e.committed_exact.iter().all(|v| !v));
    send(
        &mut e,
        &mut d,
        &f,
        2,
        at + Duration::from_secs(3),
        TileEncodeRequest::LosslessRepair,
    );
    assert!(e.committed_exact.iter().all(|v| *v));
    assert_eq!(d.canvas().unwrap(), &f.image);
    (e, d, f, at)
}
fn assert_exact_tiles(e: &TileEncoder, d: &TileDecoder, f: &CapturedFrame) {
    let decoded = d.canvas().unwrap();
    for (i, exact) in e.exact.iter().enumerate() {
        if !exact {
            continue;
        }
        let (x0, y0, x1, y1) = tile_bounds(i, e.grid.0, e.tile(), 65, 97);
        for y in y0..y1 {
            for x in x0..x1 {
                assert_eq!(
                    decoded.get_pixel(x as u32, y as u32),
                    f.image.get_pixel(x as u32, y as u32),
                    "exact tile {i}"
                );
            }
        }
    }
}
#[test]
fn scroll_exact_committed_pixels_and_repair_debt_ab() {
    for dy in [-7, 7] {
        let mut evidence = Vec::new();
        for legacy in [true, false] {
            let (e, mut d, f, at) = exact_pair();
            let mut e = e;
            let next = shifted(&f, dy);
            let start = Instant::now();
            let bytes = send(
                &mut e,
                &mut d,
                &next,
                3,
                at + Duration::from_secs(4),
                TileEncodeRequest::Delta,
            );
            assert!(TileHeader::parse(&bytes).unwrap().scroll.is_some());
            assert_exact_tiles(&e, &d, &next);
            if legacy {
                e.exact.fill(false);
                e.committed_exact.fill(false);
            }
            let debt = e.exact.iter().filter(|v| !**v).count();
            let repair = send(
                &mut e,
                &mut d,
                &next,
                4,
                at + Duration::from_secs(7),
                TileEncodeRequest::LosslessRepair,
            );
            assert_eq!(d.canvas().unwrap(), &next.image); // Both arms end with SSE exactly zero.
            evidence.push((debt, repair.len(), start.elapsed().as_nanos()));
        }
        eprintln!(
            "scroll exact dy={dy} legacy/new (repair_tiles,repair_bytes,scroll_and_repair_cpu_ns)={evidence:?}"
        );
        assert!(evidence[1].0 < evidence[0].0);
        assert!(evidence[1].1 < evidence[0].1);
    }
}
#[test]
fn scroll_exact_uncommitted_png_and_lossy_source_never_promote() {
    let (mut e, mut d, mut f, at) = exact_pair();
    // A real JPEG100 transaction, not a fabricated exactness bit.
    for y in 16..32 {
        for x in 32..48 {
            f.image.get_pixel_mut(x, y).0[0] ^= 0x80;
        }
    }
    e.plan_at(&f, TileEncodeRequest::Delta, at + Duration::from_secs(4))
        .unwrap();
    let mut jpeg = Vec::new();
    e.encode_into(&mut jpeg, 3, &f, 100).unwrap();
    assert!(!TileHeader::parse(&jpeg).unwrap().lossless);
    d.decode_into(&jpeg, new_decode_buffer_pool()).unwrap();
    e.commit(&f);
    assert!(!e.committed_exact[7]);
    let next = shifted(&f, 7);
    let bytes = send(
        &mut e,
        &mut d,
        &next,
        4,
        at + Duration::from_secs(5),
        TileEncodeRequest::Delta,
    );
    assert!(TileHeader::parse(&bytes).unwrap().scroll.is_some());
    // Source index 7 (row 1, col 2) contributes to destination rows 0 and 1.
    assert!(!e.exact[2]);
    assert!(!e.exact[7]);
    assert_exact_tiles(&e, &d, &next);
    let mut fresh = TileEncoder::new();
    fresh.set_lossless_enabled(true);
    fresh.set_scroll_copy_enabled(true);
    let mut dec = TileDecoder::new();
    dec.set_lossless_enabled(true);
    dec.set_scroll_copy_enabled(true);
    send(&mut fresh, &mut dec, &f, 1, at, TileEncodeRequest::Delta);
    fresh
        .plan_at(
            &f,
            TileEncodeRequest::LosslessRepair,
            at + Duration::from_secs(3),
        )
        .unwrap();
    let mut pending = Vec::new();
    fresh.encode_into(&mut pending, 2, &f, 75).unwrap();
    fresh.discard();
    assert!(fresh.committed_exact.iter().all(|v| !v));
    send(
        &mut fresh,
        &mut dec,
        &next,
        2,
        at + Duration::from_secs(4),
        TileEncodeRequest::Delta,
    );
    assert!(fresh.exact.iter().all(|v| !v));
}
#[test]
fn scroll_exact_failed_capture_identity_and_geometry_never_promote() {
    let (mut e, mut d, f, at) = exact_pair();
    let next = shifted(&f, 7);
    e.plan_at(&next, TileEncodeRequest::Delta, at + Duration::from_secs(4))
        .unwrap();
    let mut bytes = Vec::new();
    let mut wrong = clone_frame(&next);
    wrong.image.as_mut()[0] ^= 1;
    assert!(e.encode_into(&mut bytes, 3, &wrong, 75).is_err());
    assert_eq!(e.last_sequence, Some(2));
    e.plan_at(&next, TileEncodeRequest::Delta, at + Duration::from_secs(4))
        .unwrap();
    e.encode_into(&mut bytes, 3, &next, 75).unwrap();
    e.commit(&wrong);
    assert!(e.reference.is_none());
    assert!(e.committed_exact.is_empty());
    let mut geometry = clone_frame(&next);
    geometry.geometry_epoch += 1;
    let bytes = send(
        &mut e,
        &mut d,
        &geometry,
        4,
        at + Duration::from_secs(5),
        TileEncodeRequest::Delta,
    );
    assert!(TileHeader::parse(&bytes).unwrap().keyframe);
    assert!(e.committed_exact.iter().all(|v| !v));
}
#[test]
fn scroll_exact_partial_band_and_subtolerance_toolbar_remain_debt() {
    let (mut e, mut d, f, at) = exact_pair();
    let mut next = clone_frame(&f);
    for y in 16..90 {
        for x in 0..65 {
            *next.image.get_pixel_mut(x, y) = *f.image.get_pixel(x, y + 7);
        }
    }
    next.image.as_mut()[0] = next.image.as_raw()[0].wrapping_add(1);
    let bytes = send(
        &mut e,
        &mut d,
        &next,
        3,
        at + Duration::from_secs(4),
        TileEncodeRequest::Delta,
    );
    assert!(TileHeader::parse(&bytes).unwrap().scroll.is_some());
    assert!(!e.exact[0]);
    assert_ne!(d.canvas().unwrap().as_raw()[0], next.image.as_raw()[0]);
    assert_exact_tiles(&e, &d, &next);
}

#[test]
fn scroll_exact_cached_keepalive_discard_and_commit_preserve_committed_canvas() {
    let (mut encoder, mut decoder, frame, at) = exact_pair();
    let exact = encoder.committed_exact.clone();
    let canvas = decoder.canvas().unwrap().clone();
    let now = at + Duration::from_secs(4);
    let plan = encoder
        .plan_at(&frame, TileEncodeRequest::Keepalive, now)
        .unwrap();
    assert!(plan.emit);
    assert!(!plan.keyframe);
    assert_eq!(plan.dirty_tiles, 0);
    let mut discarded = Vec::new();
    encoder.encode_into(&mut discarded, 3, &frame, 75).unwrap();
    encoder.discard();
    assert_eq!(encoder.last_sequence, Some(2));
    assert_eq!(encoder.committed_exact, exact);
    assert_eq!(decoder.canvas(), Some(&canvas));
    // Reusing the same cached RGB/scope after discarding an unsent transaction
    // remains legal. Only the successfully delivered record advances sequence.
    let kept = send(
        &mut encoder,
        &mut decoder,
        &frame,
        3,
        now,
        TileEncodeRequest::Keepalive,
    );
    let header = TileHeader::parse(&kept).unwrap();
    assert_eq!(header.dirty_tiles, 0);
    assert!(!header.lossless && header.scroll.is_none());
    assert_eq!(encoder.last_sequence, Some(3));
    assert_eq!(encoder.committed_exact, exact);
    assert_eq!(decoder.canvas(), Some(&canvas));
    let next = send(
        &mut encoder,
        &mut decoder,
        &frame,
        4,
        now + Duration::from_secs(1),
        TileEncodeRequest::Keepalive,
    );
    assert_eq!(TileHeader::parse(&next).unwrap().dirty_tiles, 0);
    assert_eq!(encoder.last_sequence, Some(4));
    assert_eq!(encoder.committed_exact, exact);
    assert_eq!(decoder.canvas(), Some(&canvas));
}

#[test]
fn scroll_exact_new_detection_invalidates_pending_identity_without_committing() {
    let (mut encoder, _, frame, at) = exact_pair();
    let next = shifted(&frame, 7);
    encoder
        .plan_at(&next, TileEncodeRequest::Delta, at + Duration::from_secs(4))
        .unwrap();
    let mut bytes = Vec::new();
    encoder.encode_into(&mut bytes, 3, &next, 75).unwrap();
    assert!(
        encoder.pending_hash.is_none(),
        "JPEG must not hash the full capture"
    );
    let pending_dirty = encoder.dirty.clone();
    assert!(encoder.select_refinement(Some((100, 1))).is_err());
    assert_eq!(encoder.dirty, pending_dirty);
    assert!(encoder.pending);
    let generation = encoder.observation_generation;
    let committed = encoder.committed_exact.clone();
    encoder
        .plan_at(
            &frame,
            TileEncodeRequest::Keepalive,
            at + Duration::from_secs(5),
        )
        .unwrap();
    assert!(encoder.observation_generation > generation);
    encoder.commit(&next);
    assert_eq!(encoder.last_sequence, Some(2));
    assert_eq!(encoder.committed_exact, committed);
    assert_eq!(encoder.reference.as_ref(), Some(&frame.image));
    encoder.encode_into(&mut bytes, 3, &frame, 75).unwrap();
    assert!(encoder.pending_hash.is_none());
    let mut wrong = clone_frame(&frame);
    wrong.source_width += 1;
    encoder.commit(&wrong);
    assert!(encoder.reference.is_none());
    assert!(encoder.committed_exact.is_empty());
}

#[test]
fn scroll_exact_generation_exhaustion_is_not_wrapping_authority() {
    let (mut encoder, _, frame, at) = exact_pair();
    encoder.observation_generation = u64::MAX;
    assert!(
        encoder
            .plan_at(
                &frame,
                TileEncodeRequest::Keepalive,
                at + Duration::from_secs(4)
            )
            .is_err()
    );
    assert!(!encoder.pending);
    assert_eq!(encoder.last_sequence, Some(2));
}
