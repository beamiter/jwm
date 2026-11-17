//! Socket-free, real memfd upload evidence. Timings describe conversion and
//! writes only, not decoding, GPU work, presentation, or input response.
use super::{MAX_UPLOAD_SCRATCH_BYTES, damage, upload_xrgb};
use crate::remote::frame::DamageRegions;
use image::RgbImage;
use nix::sys::memfd::{MFdFlags, memfd_create};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::time::Instant;

struct CountIo<'a> {
    file: &'a mut File,
    writes: usize,
    seeks: usize,
    bytes: usize,
}
impl Write for CountIo<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.writes += 1;
        let count = self.file.write(bytes)?;
        self.bytes += count;
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}
impl Seek for CountIo<'_> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.seeks += 1;
        self.file.seek(position)
    }
}
fn measure(
    file: &mut File,
    image: &RgbImage,
    destination: (u16, u16),
    dirty: Option<&DamageRegions>,
    scratch: &mut [u8],
) -> serde_json::Value {
    let mut io = CountIo {
        file,
        writes: 0,
        seeks: 0,
        bytes: 0,
    };
    let started = Instant::now();
    if let Some(dirty) = dirty {
        damage::upload_regions(
            &mut io,
            image.as_raw(),
            (image.width() as u16, image.height() as u16),
            destination,
            dirty,
            scratch,
        )
        .unwrap();
    } else {
        io.seek(SeekFrom::Start(0)).unwrap();
        upload_xrgb(&mut io, image, destination.0, destination.1, scratch).unwrap();
    }
    serde_json::json!({
        "upload_ns": u64::try_from(started.elapsed().as_nanos()).unwrap(),
        "written_bytes": io.bytes,
        "write_calls": io.writes,
        "seek_calls": io.seeks,
    })
}
fn contents(file: &mut File) -> Vec<u8> {
    file.seek(SeekFrom::Start(0)).unwrap();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    bytes
}

#[test]
#[ignore = "explicit real memfd upload measurement; no display or socket"]
fn native_memfd_sparse_damage_matches_full_canvas_and_counts_writes() {
    let mut cases = Vec::new();
    for (source, destination) in [
        ((2560u16, 1440u16), (2560u16, 1440u16)),
        ((1280, 720), (1920, 1080)),
        ((800, 600), (1920, 1080)),
    ] {
        let mut image = RgbImage::from_fn(u32::from(source.0), u32::from(source.1), |x, y| {
            image::Rgb([x as u8, y as u8, (x ^ y) as u8])
        });
        let mut full = File::from(memfd_create(c"jwm-damage-full", MFdFlags::MFD_CLOEXEC).unwrap());
        let mut patch =
            File::from(memfd_create(c"jwm-damage-patch", MFdFlags::MFD_CLOEXEC).unwrap());
        let canvas_bytes = usize::from(destination.0) * usize::from(destination.1) * 4;
        full.set_len(canvas_bytes as u64).unwrap();
        patch.set_len(canvas_bytes as u64).unwrap();
        let mut scratch = vec![0; MAX_UPLOAD_SCRATCH_BYTES];
        measure(&mut full, &image, destination, None, &mut scratch);
        measure(&mut patch, &image, destination, None, &mut scratch);
        let mut samples = Vec::new();
        for index in 0..6u16 {
            let rect = (64 + index * 64, 64 + index * 32, 64, 64);
            for y in rect.1..rect.1 + rect.3 {
                for x in rect.0..rect.0 + rect.2 {
                    for byte in &mut image.get_pixel_mut(u32::from(x), u32::from(y)).0 {
                        *byte ^= 0xff;
                    }
                }
            }
            let dirty =
                damage::map_regions(source, destination, &DamageRegions::from_rect(Some(rect)))
                    .unwrap();
            let (baseline, incremental) = if index % 2 == 0 {
                (
                    measure(&mut full, &image, destination, None, &mut scratch),
                    measure(&mut patch, &image, destination, Some(&dirty), &mut scratch),
                )
            } else {
                let incremental =
                    measure(&mut patch, &image, destination, Some(&dirty), &mut scratch);
                (
                    measure(&mut full, &image, destination, None, &mut scratch),
                    incremental,
                )
            };
            let expected_patch_bytes: usize = dirty
                .as_slice()
                .iter()
                .map(|r| usize::from(r.2) * usize::from(r.3) * 4)
                .sum();
            assert!(expected_patch_bytes < canvas_bytes / 16);
            assert_eq!(baseline["written_bytes"], canvas_bytes);
            assert_eq!(incremental["written_bytes"], expected_patch_bytes);
            assert_eq!(contents(&mut full), contents(&mut patch));
            samples.push(serde_json::json!({
                "index": index,
                "first": if index % 2 == 0 { "full" } else { "incremental" },
                "full": baseline,
                "incremental": incremental,
                "all_bytes_equal": true,
            }));
        }
        let unchanged = measure(
            &mut patch,
            &image,
            destination,
            Some(&DamageRegions::default()),
            &mut scratch,
        );
        assert_eq!(unchanged["written_bytes"], 0);
        assert_eq!(unchanged["write_calls"], 0);
        assert_eq!(unchanged["seek_calls"], 0);
        assert_eq!(contents(&mut full), contents(&mut patch));
        cases.push(serde_json::json!({
            "source": [source.0, source.1],
            "destination": [destination.0, destination.1],
            "canvas_bytes": canvas_bytes,
            "source_damage": [64, 64],
            "samples": samples,
            "unchanged": unchanged,
        }));
    }
    println!(
        "JWM_DAMAGE_UPLOAD_BENCHMARK {}",
        serde_json::json!({
            "schema_version": 1,
            "debug_assertions": cfg!(debug_assertions),
            "scope": "CPU RGB-to-XRGB conversion and real owned memfd writes, not presentation latency",
            "cases": cases,
        })
    );
}
