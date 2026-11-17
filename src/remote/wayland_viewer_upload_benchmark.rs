//! Opt-in, socket-free upload microbenchmark using the production helper.
use super::{MAX_UPLOAD_SCRATCH_BYTES, upload_xrgb};
use image::RgbImage;
use nix::sys::memfd::{MFdFlags, memfd_create};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::time::Instant;

struct CountWrites<'a> {
    file: &'a mut File,
    calls: usize,
    bytes: usize,
}
impl Write for CountWrites<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.calls += 1;
        let count = self.file.write(bytes)?;
        self.bytes += count;
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

// Historical row-at-a-time implementation, intentionally independent of the
// production chunk planner. Retained only as a byte and upload-cost reference.
fn reference_upload(
    writer: &mut impl Write,
    image: &RgbImage,
    width: u16,
    height: u16,
) -> io::Result<()> {
    let (iw, ih) = (u64::from(image.width()), u64::from(image.height()));
    let (dw, dh) = if u64::from(width) * ih <= u64::from(height) * iw {
        (u32::from(width), (u64::from(width) * ih / iw).max(1) as u32)
    } else {
        (
            (u64::from(height) * iw / ih).max(1) as u32,
            u32::from(height),
        )
    };
    let (ox, oy) = ((u32::from(width) - dw) / 2, (u32::from(height) - dh) / 2);
    let mut row = vec![0; usize::from(width) * 4];
    for y in 0..u32::from(height) {
        row.fill(0);
        if y >= oy && y < oy + dh {
            let sy = (u64::from(y - oy) * ih / u64::from(dh)) as u32;
            for x in 0..dw {
                let sx = (u64::from(x) * iw / u64::from(dw)) as u32;
                let rgb = image.get_pixel(sx, sy).0;
                let pixel =
                    (u32::from(rgb[0]) << 16) | (u32::from(rgb[1]) << 8) | u32::from(rgb[2]);
                let at = (x + ox) as usize * 4;
                row[at..at + 4].copy_from_slice(&pixel.to_ne_bytes());
            }
        }
        writer.write_all(&row)?;
    }
    Ok(())
}

fn measure(
    file: &mut File,
    image: &RgbImage,
    width: u16,
    height: u16,
    scratch: &mut [u8],
    production: bool,
) -> serde_json::Value {
    file.seek(SeekFrom::Start(0)).unwrap();
    let mut writer = CountWrites {
        file,
        calls: 0,
        bytes: 0,
    };
    let start = Instant::now();
    if production {
        upload_xrgb(&mut writer, image, width, height, scratch).unwrap();
    } else {
        reference_upload(&mut writer, image, width, height).unwrap();
    }
    let ns = start.elapsed().as_nanos();
    assert_eq!(writer.bytes, usize::from(width) * usize::from(height) * 4);
    serde_json::json!({"upload_ns": u64::try_from(ns).unwrap(),
        "write_calls":writer.calls,"written_bytes":writer.bytes})
}

fn assert_same(a: &mut File, b: &mut File, bytes: usize) {
    a.seek(SeekFrom::Start(0)).unwrap();
    b.seek(SeekFrom::Start(0)).unwrap();
    let mut av = vec![0; bytes];
    let mut bv = vec![0; bytes];
    a.read_exact(&mut av).unwrap();
    b.read_exact(&mut bv).unwrap();
    assert_eq!(
        av, bv,
        "production upload differs from historical pixel reference"
    );
}

#[test]
#[ignore = "explicit offline memfd upload measurement; no display or socket"]
fn offline_memfd_upload_samples() {
    let mut cases = Vec::new();
    for (iw, ih, width, height) in [
        (320, 180, 320u16, 180u16),
        (641, 359, 1280, 720),
        (800, 600, 1920, 1080),
    ] {
        let image = RgbImage::from_fn(iw, ih, |x, y| {
            image::Rgb([
                x.wrapping_mul(17).wrapping_add(y) as u8,
                y.wrapping_mul(31).wrapping_add(x) as u8,
                (x ^ y) as u8,
            ])
        });
        let bytes = usize::from(width) * usize::from(height) * 4;
        let mut old =
            File::from(memfd_create(c"jwm-upload-reference", MFdFlags::MFD_CLOEXEC).unwrap());
        let mut new =
            File::from(memfd_create(c"jwm-upload-production", MFdFlags::MFD_CLOEXEC).unwrap());
        old.set_len(bytes as u64).unwrap();
        new.set_len(bytes as u64).unwrap();
        let allocation_start = Instant::now();
        let mut scratch = vec![0u8; MAX_UPLOAD_SCRATCH_BYTES].into_boxed_slice();
        let allocation_ns = u64::try_from(allocation_start.elapsed().as_nanos()).unwrap();
        let first_reference = measure(&mut old, &image, width, height, &mut scratch, false);
        let first_production = measure(&mut new, &image, width, height, &mut scratch, true);
        assert_same(&mut old, &mut new, bytes);
        let mut samples = Vec::new();
        for sample in 0..5 {
            // Alternate order to avoid always giving one path the first turn.
            let (reference, production) = if sample % 2 == 0 {
                (
                    measure(&mut old, &image, width, height, &mut scratch, false),
                    measure(&mut new, &image, width, height, &mut scratch, true),
                )
            } else {
                let production = measure(&mut new, &image, width, height, &mut scratch, true);
                (
                    measure(&mut old, &image, width, height, &mut scratch, false),
                    production,
                )
            };
            assert_same(&mut old, &mut new, bytes);
            samples.push(serde_json::json!({"index":sample,
                "first":if sample % 2 == 0 {"reference"} else {"production"},
                "reference":reference,"production":production}));
        }
        cases.push(serde_json::json!({"input":[iw,ih],"output":[width,height],
            "scratch_bytes":scratch.len(),"scratch_initialization_ns":allocation_ns,
            "reference_row_bytes":usize::from(width)*4,
            "first_reference":first_reference,"first_production":first_production,
            "samples":samples,"all_bytes_equal":true}));
    }
    println!(
        "JWM_UPLOAD_BENCHMARK {}",
        serde_json::json!({"schema_version":1,
        "debug_assertions":cfg!(debug_assertions),"cases":cases,
        "scope":"CPU conversion and counted File writes to owned memfd; not presentation latency"})
    );
}
