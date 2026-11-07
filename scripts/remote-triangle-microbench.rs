// CPU-only smoke measurement; not a Cargo target or a live-X11 benchmark.
// Build image with Cargo's release profile first, then compile this harness:
// rustc --edition=2024 -O scripts/remote-triangle-microbench.rs \
//   --extern image=target/release/deps/libimage-<matching-hash>.rlib \
//   -L dependency=target/release/deps -o /tmp/remote-triangle-microbench
// /tmp/remote-triangle-microbench
// Near-full sampling bypasses DamageRegions, whose >=60% coverage collapse
// selects the full-region fallback in the real viewer.
#[path = "../src/remote/triangle_patch.rs"]
mod triangle_patch;
use image::{
    Rgb, RgbImage,
    imageops::{FilterType, crop_imm, resize},
};
use std::{hint::black_box, time::Instant};
fn main() {
    let iterations = 40;
    println!(
        "CPU only; rustc -O; image 0.25.10 release dependency; iterations={iterations}; alternating full/partial order; one warmup each; no X/upload/network; elapsed microseconds"
    );
    for (sw, sh, dw, dh) in [(320, 180, 640_u16, 360_u16), (640, 360, 320, 180)] {
        let image = RgbImage::from_fn(sw, sh, |x, y| {
            Rgb([
                (x * 71 + y * 23) as u8,
                (x * 13 + y * 97) as u8,
                (x * 31 + y * 11) as u8,
            ])
        });
        for (name, regions) in [
            ("tiny", vec![(dw / 2, dh / 2, 8, 8)]),
            (
                "scattered",
                vec![
                    (0, 0, 8, 8),
                    (dw - 8, 0, 8, 8),
                    (0, dh - 8, 8, 8),
                    (dw - 8, dh - 8, 8, 8),
                ],
            ),
            ("near_full", vec![(0, 0, dw - 1, dh)]),
        ] {
            let mut patch = triangle_patch::TrianglePatch::default();
            let reference = resize(&image, u32::from(dw), u32::from(dh), FilterType::Triangle);
            assert!(patch.prepare((sw, sh), (dw, dh), &regions));
            for &(x, y, w, h) in &regions {
                assert_eq!(
                    patch.render(&image, (dw, dh), (x, y, w, h)).as_raw(),
                    crop_imm(
                        &reference,
                        u32::from(x),
                        u32::from(y),
                        u32::from(w),
                        u32::from(h)
                    )
                    .to_image()
                    .as_raw()
                );
            }
            let mut full = 0;
            let mut partial = 0;
            for iteration in 0..iterations {
                for step in 0..2 {
                    let start = Instant::now();
                    if (iteration + step) % 2 == 0 {
                        black_box(resize(
                            black_box(&image),
                            u32::from(dw),
                            u32::from(dh),
                            FilterType::Triangle,
                        ));
                        full += start.elapsed().as_micros();
                    } else {
                        assert!(patch.prepare((sw, sh), (dw, dh), &regions));
                        for &rect in &regions {
                            black_box(patch.render(black_box(&image), (dw, dh), rect));
                        }
                        partial += start.elapsed().as_micros();
                    }
                }
            }
            println!(
                "{sw}x{sh}->{dw}x{dh} {name}: full_us={full} partial_with_preflight_us={partial}"
            );
        }
        let mut patch = triangle_patch::TrianglePatch::default();
        assert!(!patch.prepare((sw, sh), (dw, dh), &[(0, 0, dw, dh)]));
        let start = Instant::now();
        for _ in 0..iterations {
            assert!(!patch.prepare((sw, sh), (dw, dh), &[(0, 0, dw, dh)]));
            black_box(resize(
                black_box(&image),
                u32::from(dw),
                u32::from(dh),
                FilterType::Triangle,
            ));
        }
        println!(
            "{sw}x{sh}->{dw}x{dh} full_region_fallback_us={}",
            start.elapsed().as_micros()
        );
    }
}
