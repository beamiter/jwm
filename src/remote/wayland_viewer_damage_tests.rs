use super::*;
use std::io::Cursor;

fn regions(rect: (u16, u16, u16, u16)) -> DamageRegions {
    DamageRegions::from_rect(Some(rect))
}

fn pixels(size: (u16, u16)) -> Vec<u8> {
    (0..usize::from(size.0) * usize::from(size.1) * 3)
        .map(|i| (i * 17 + i / 7) as u8)
        .collect()
}

// Independent row-at-a-time reference, not the patcher's Layout or mapper.
fn reference(rgb: &[u8], source: (u16, u16), destination: (u16, u16)) -> Vec<u8> {
    let (iw, ih) = (u64::from(source.0), u64::from(source.1));
    let (width, height) = (u64::from(destination.0), u64::from(destination.1));
    let (dw, dh) = if width * ih <= height * iw {
        (width, (width * ih / iw).max(1))
    } else {
        ((height * iw / ih).max(1), height)
    };
    let (ox, oy) = ((width - dw) / 2, (height - dh) / 2);
    let mut output = vec![0; (width * height * 4) as usize];
    for y in oy..oy + dh {
        for x in ox..ox + dw {
            let (sx, sy) = ((x - ox) * iw / dw, (y - oy) * ih / dh);
            let at = ((sy * iw + sx) * 3) as usize;
            let pixel =
                (u32::from(rgb[at]) << 16) | (u32::from(rgb[at + 1]) << 8) | u32::from(rgb[at + 2]);
            let at = ((y * width + x) * 4) as usize;
            output[at..at + 4].copy_from_slice(&pixel.to_ne_bytes());
        }
    }
    output
}

#[derive(Default)]
struct Writer {
    data: Cursor<Vec<u8>>,
    writes: usize,
    bytes: usize,
    seeks: usize,
    limit: Option<usize>,
    interrupt_at: Option<usize>,
    fail_at: Option<(usize, io::ErrorKind)>,
    fail_seek: bool,
}
impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.writes += 1;
        if self.interrupt_at == Some(self.writes) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        if let Some((at, kind)) = self.fail_at
            && self.writes == at
        {
            return if kind == io::ErrorKind::WriteZero {
                Ok(0)
            } else {
                Err(kind.into())
            };
        }
        let count = bytes.len().min(self.limit.unwrap_or(usize::MAX));
        let written = self.data.write(&bytes[..count])?;
        self.bytes += written;
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Seek for Writer {
    fn seek(&mut self, offset: SeekFrom) -> io::Result<u64> {
        self.seeks += 1;
        if self.fail_seek {
            return Err(io::ErrorKind::Other.into());
        }
        self.data.seek(offset)
    }
}

#[test]
fn every_small_pixel_patch_matches_complete_upload_at_odd_scaling_boundaries() {
    let mut scratch = vec![0xa5; 64];
    for source in [(1, 1), (3, 7), (7, 3), (13, 11), (1, 37), (37, 1)] {
        for width in 1..=13 {
            for height in 1..=13 {
                let destination = (width, height);
                let original = pixels(source);
                let baseline = reference(&original, source, destination);
                for y in 0..source.1 {
                    for x in 0..source.0 {
                        let mut rgb = original.clone();
                        let at = (usize::from(y) * usize::from(source.0) + usize::from(x)) * 3;
                        for byte in &mut rgb[at..at + 3] {
                            *byte ^= 0xff;
                        }
                        let mapped = map_regions(source, destination, &regions((x, y, 1, 1)))
                            .expect("valid damage");
                        let mut writer = Cursor::new(baseline.clone());
                        upload_regions(
                            &mut writer,
                            &rgb,
                            source,
                            destination,
                            &mapped,
                            &mut scratch,
                        )
                        .unwrap();
                        assert_eq!(
                            writer.into_inner(),
                            reference(&rgb, source, destination),
                            "source={source:?} destination={destination:?} pixel=({x},{y})"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn sparse_updates_write_only_spans_and_keepalive_writes_nothing() {
    let size = (100, 80);
    let rgb = pixels(size);
    let mut scratch = vec![0xa5; MAX_UPLOAD_SCRATCH_BYTES];
    let mut writer = Writer {
        data: Cursor::new(vec![0xdd; 100 * 80 * 4]),
        ..Writer::default()
    };
    let damage = regions((11, 7, 3, 2));
    upload_regions(&mut writer, &rgb, size, size, &damage, &mut scratch).unwrap();
    assert_eq!((writer.bytes, writer.writes, writer.seeks), (24, 2, 2));
    let expected = reference(&rgb, size, size);
    for y in 0..80 {
        for x in 0..100 {
            let at = (y * 100 + x) * 4;
            let actual = &writer.data.get_ref()[at..at + 4];
            if (7..9).contains(&y) && (11..14).contains(&x) {
                assert_eq!(actual, &expected[at..at + 4]);
            } else {
                assert_eq!(actual, &[0xdd; 4]);
            }
        }
    }
    upload_regions(
        &mut writer,
        &rgb,
        size,
        size,
        &DamageRegions::default(),
        &mut scratch,
    )
    .unwrap();
    assert_eq!((writer.bytes, writer.writes, writer.seeks), (24, 2, 2));
    // Source pixel one is never sampled when three columns shrink to one.
    assert!(
        map_regions((3, 1), (1, 1), &regions((1, 0, 1, 1)))
            .unwrap()
            .as_slice()
            .is_empty()
    );
}

#[test]
fn alternating_slots_accumulate_updates_while_held_and_unknown_delta_rebuilds() {
    let source = (17, 13);
    let destination = (31, 19);
    let mut rgb = pixels(source);
    let mut scratch = vec![0xa5; 31 * 4 * 2 + 3];
    let mut slots = [SlotDamage::default(), SlotDamage::default()];
    let initial = reference(&rgb, source, destination);
    let mut storage = [Cursor::new(initial.clone()), Cursor::new(initial.clone())];
    for slot in &mut slots {
        assert!(slot.is_full());
        slot.uploaded();
    }
    for step in 0..30 {
        let x = (step * 7 % 17) as u16;
        let y = (step * 5 % 13) as u16;
        let at = (usize::from(y) * 17 + usize::from(x)) * 3;
        rgb[at..at + 3].fill(step as u8);
        let delta = map_regions(source, destination, &regions((x, y, 1, 1)));
        for slot in &mut slots {
            slot.accumulate(delta, destination);
        }
        // Slot one is held for the first ten frames, then reuse alternates.
        let index = if step < 10 { 0 } else { step % 2 };
        if step < 10 {
            assert_eq!(storage[1].get_ref(), &initial);
        }
        upload_regions(
            &mut storage[index],
            &rgb,
            source,
            destination,
            &slots[index].regions(destination),
            &mut scratch,
        )
        .unwrap();
        slots[index].uploaded();
        assert_eq!(
            storage[index].get_ref(),
            &reference(&rgb, source, destination)
        );
        assert!(slots[index].regions(destination).as_slice().is_empty());
    }
    // A missing predecessor or invalidated generation loses every baseline.
    for slot in &mut slots {
        slot.accumulate(None, destination);
        slot.accumulate(Some(DamageRegions::default()), destination);
        assert!(slot.is_full());
        assert_eq!(slot.regions(destination).as_slice(), &[(0, 0, 31, 19)]);
    }
}

#[test]
fn fragmented_slot_damage_stays_bounded_and_covers_all_changes() {
    let size = (100, 80);
    let mut slot = SlotDamage::default();
    slot.uploaded();
    for i in 0..40 {
        slot.accumulate(Some(regions((i * 2, i, 1, 1))), size);
    }
    let damage = slot.regions(size);
    assert!(damage.as_slice().len() <= 8);
    for i in 0..40 {
        assert!(
            damage
                .as_slice()
                .iter()
                .any(|&(x, y, w, h)| { x <= i * 2 && i * 2 < x + w && y <= i && i < y + h })
        );
    }
}

#[test]
fn full_width_patches_batch_rows_and_rebuild_letterboxing() {
    let source = (3, 7);
    let destination = (13, 9);
    let rgb = pixels(source);
    let mut scratch = vec![0xa5; 13 * 4 * 2 + 3];
    let mut writer = Writer {
        data: Cursor::new(vec![0xff; 13 * 9 * 4]),
        ..Writer::default()
    };
    upload_regions(
        &mut writer,
        &rgb,
        source,
        destination,
        &regions((0, 0, 13, 9)),
        &mut scratch,
    )
    .unwrap();
    assert_eq!(
        (writer.bytes, writer.writes, writer.seeks),
        (13 * 9 * 4, 5, 5)
    );
    assert_eq!(
        writer.data.into_inner(),
        reference(&rgb, source, destination)
    );
}

#[test]
fn patch_short_writes_interruptions_and_failures_do_not_clear_dirty_state() {
    let size = (13, 9);
    let rgb = pixels(size);
    let mut scratch = vec![0; 13 * 4];
    let mut writer = Writer {
        limit: Some(3),
        interrupt_at: Some(2),
        ..Writer::default()
    };
    upload_regions(
        &mut writer,
        &rgb,
        size,
        size,
        &regions((0, 0, 13, 9)),
        &mut scratch,
    )
    .unwrap();
    assert_eq!(writer.data.into_inner(), reference(&rgb, size, size));
    for kind in [io::ErrorKind::WriteZero, io::ErrorKind::Other] {
        let slot = SlotDamage::default();
        let mut writer = Writer {
            fail_at: Some((2, kind)),
            ..Writer::default()
        };
        assert_eq!(
            upload_regions(
                &mut writer,
                &rgb,
                size,
                size,
                &slot.regions(size),
                &mut scratch,
            )
            .unwrap_err()
            .kind(),
            kind
        );
        assert_eq!(writer.writes, 2);
        assert!(slot.is_full());
    }
    let mut writer = Writer {
        fail_seek: true,
        ..Writer::default()
    };
    assert!(
        upload_regions(
            &mut writer,
            &rgb,
            size,
            size,
            &regions((1, 1, 1, 1)),
            &mut scratch,
        )
        .is_err()
    );
    assert_eq!(writer.writes, 0);
}

#[test]
fn invalid_damage_or_upload_inputs_never_seek_or_write() {
    let size = (13, 9);
    let rgb = pixels(size);
    let damage = regions((12, 8, 2, 2));
    assert!(map_regions(size, size, &damage).is_none());
    assert!(map_regions((0, 1), size, &DamageRegions::default()).is_none());
    assert!(map_regions(size, (1, 0), &DamageRegions::default()).is_none());
    let mut scratch = vec![0; MAX_UPLOAD_SCRATCH_BYTES];
    let mut writer = Writer::default();
    assert!(upload_regions(&mut writer, &rgb, size, size, &damage, &mut scratch).is_err());
    let damage = regions((1, 1, 1, 1));
    assert!(upload_regions(&mut writer, &rgb[..4], size, size, &damage, &mut scratch).is_err());
    for length in [0, 13 * 4 - 1, MAX_UPLOAD_SCRATCH_BYTES + 1] {
        let mut scratch = vec![0; length];
        assert!(upload_regions(&mut writer, &rgb, size, size, &damage, &mut scratch).is_err());
    }
    assert_eq!((writer.bytes, writer.writes, writer.seeks), (0, 0, 0));
}
