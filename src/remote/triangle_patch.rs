//! Patch-local RGB Triangle sampling, compatible with image 0.25.10.
//!
//! The sampling arithmetic follows image's imageops/sample.rs (MIT licensed;
//! see licenses/image-MIT.txt). Global phase, edge normalization, vertical-first
//! f32 accumulation and final-only rounding are intentional compatibility rules.
use image::RgbImage;

type Rect = (u16, u16, u16, u16);
const SCRATCH_LIMIT: usize = 16 * 1024 * 1024;

#[derive(Default)]
pub(super) struct TrianglePatch {
    output: RgbImage,
    row: Vec<[f32; 3]>,
    horizontal: Vec<(u32, usize, usize)>,
    weights: Vec<f32>,
    vertical: Vec<f32>,
}

fn bounds(source: u32, destination: u32, position: u32) -> (u32, u32, f32, f32) {
    let ratio = source as f32 / destination as f32;
    let scale = if ratio < 1.0 { 1.0 } else { ratio };
    let center = (position as f32 + 0.5) * ratio;
    let start = ((center - scale).floor() as i64).clamp(0, i64::from(source) - 1) as u32;
    let end =
        ((center + scale).ceil() as i64).clamp(i64::from(start) + 1, i64::from(source)) as u32;
    (start, end, center - 0.5, scale)
}

fn append_weights(weights: &mut Vec<f32>, range: (u32, u32, f32, f32)) {
    let (start, end, center, scale) = range;
    let offset = weights.len();
    let mut sum = 0.0;
    for pixel in start..end {
        let distance = ((pixel as f32 - center) / scale).abs();
        let weight = if distance < 1.0 { 1.0 - distance } else { 0.0 };
        weights.push(weight);
        sum += weight;
    }
    for weight in &mut weights[offset..] {
        *weight /= sum;
    }
}

fn reserve<T>(buffer: &mut Vec<T>, size: usize) -> bool {
    buffer
        .try_reserve_exact(size.saturating_sub(buffer.len()))
        .is_ok()
}

impl TrianglePatch {
    fn retained_bytes(&self) -> u64 {
        self.output.as_raw().capacity() as u64
            + self.row.capacity() as u64 * 12
            + self.horizontal.capacity() as u64 * size_of::<(u32, usize, usize)>() as u64
            + self.weights.capacity() as u64 * 4
            + self.vertical.capacity() as u64 * 4
    }

    /// Reserve every required buffer before the first external upload. Invalid,
    /// full-frame or over-budget plans use the caller's existing full resize.
    pub(super) fn prepare(
        &mut self,
        source: (u32, u32),
        destination: (u16, u16),
        regions: &[Rect],
    ) -> bool {
        let (sw, sh) = source;
        let (dw, dh) = destination;
        if sw == 0
            || sh == 0
            || sw > u32::from(u16::MAX)
            || sh > u32::from(u16::MAX)
            || dw == 0
            || dh == 0
        {
            return false;
        }
        let mut pixels = 0_usize;
        let mut columns = 0;
        let mut taps = 0;
        for &(x, y, w, h) in regions {
            if w == 0
                || h == 0
                || u32::from(x) + u32::from(w) > u32::from(dw)
                || u32::from(y) + u32::from(h) > u32::from(dh)
                || (x, y, w, h) == (0, 0, dw, dh)
            {
                return false;
            }
            let bytes = u64::from(w) * u64::from(h) * 3;
            if bytes > SCRATCH_LIMIT as u64 {
                return false;
            }
            pixels = pixels.max(bytes as usize);
            columns = columns.max(usize::from(w));
            let count = (u32::from(x)..u32::from(x) + u32::from(w))
                .map(|column| {
                    let (start, end, _, _) = bounds(sw, u32::from(dw), column);
                    (end - start) as usize
                })
                .sum::<usize>();
            taps = taps.max(count);
        }
        // The protocol's u16 dimension bound makes these products safe even
        // on 32-bit targets; use u64 for the combined budget calculation.
        let required = pixels as u64
            + u64::from(sw) * 12
            + columns as u64 * size_of::<(u32, usize, usize)>() as u64
            + taps as u64 * 4
            + u64::from(sh) * 4;
        if required > SCRATCH_LIMIT as u64 {
            return false;
        }
        // Release old capacity before a differently shaped allocation could
        // exceed the same retained-memory budget.
        let retained = self.retained_bytes();
        if retained + required > SCRATCH_LIMIT as u64 {
            *self = Self::default();
        }
        let mut output = std::mem::take(&mut self.output).into_raw();
        let ok = reserve(&mut output, pixels)
            && reserve(&mut self.row, sw as usize)
            && reserve(&mut self.horizontal, columns)
            && reserve(&mut self.weights, taps)
            && reserve(&mut self.vertical, sh as usize);
        output.clear();
        self.output = RgbImage::from_raw(0, 0, output).expect("empty RGB buffer");
        // Allocators may provide more capacity than requested by reserve_exact.
        let ok = ok && self.retained_bytes() <= SCRATCH_LIMIT as u64;
        if !ok {
            *self = Self::default();
        }
        ok
    }

    /// Must follow successful prepare with the same source/destination/regions.
    pub(super) fn render(
        &mut self,
        source: &RgbImage,
        destination: (u16, u16),
        rect: Rect,
    ) -> &RgbImage {
        let (x, y, w, h) = rect;
        let (sw, sh) = source.dimensions();
        let (dw, dh) = destination;
        self.horizontal.clear();
        self.weights.clear();
        for column in u32::from(x)..u32::from(x) + u32::from(w) {
            let range = bounds(sw, u32::from(dw), column);
            let offset = self.weights.len();
            append_weights(&mut self.weights, range);
            self.horizontal
                .push((range.0, offset, (range.1 - range.0) as usize));
        }
        let first = self.horizontal[0].0;
        let &(last, _, count) = self.horizontal.last().expect("nonempty patch");
        let end = last + count as u32;
        self.row.resize((end - first) as usize, [0.0; 3]);
        let mut output = std::mem::take(&mut self.output).into_raw();
        output.resize(usize::from(w) * usize::from(h) * 3, 0);
        for row in 0..u32::from(h) {
            let range = bounds(sh, u32::from(dh), u32::from(y) + row);
            self.vertical.clear();
            append_weights(&mut self.vertical, range);
            for sx in first..end {
                let mut value = [0.0; 3];
                for (index, weight) in self.vertical.iter().enumerate() {
                    let pixel = source.get_pixel(sx, range.0 + index as u32).0;
                    for channel in 0..3 {
                        value[channel] += f32::from(pixel[channel]) * weight;
                    }
                }
                self.row[(sx - first) as usize] = value;
            }
            for (column, &(start, offset, count)) in self.horizontal.iter().enumerate() {
                let mut value = [0.0_f32; 3];
                for index in 0..count {
                    let pixel = self.row[(start - first) as usize + index];
                    let weight = self.weights[offset + index];
                    for channel in 0..3 {
                        value[channel] += pixel[channel] * weight;
                    }
                }
                let target = (row as usize * usize::from(w) + column) * 3;
                for channel in 0..3 {
                    output[target + channel] = value[channel].clamp(0.0, 255.0).round() as u8;
                }
            }
        }
        self.output = RgbImage::from_raw(u32::from(w), u32::from(h), output)
            .expect("prepared RGB dimensions");
        &self.output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{
        Rgb,
        imageops::{FilterType, crop_imm, resize},
    };

    fn fixture(w: u32, h: u32, seed: &mut u32) -> RgbImage {
        RgbImage::from_fn(w, h, |_, _| {
            Rgb(std::array::from_fn(|_| {
                *seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                (*seed >> 24) as u8
            }))
        })
    }

    fn compare(
        scratch: &mut TrianglePatch,
        source: &RgbImage,
        destination: (u16, u16),
        rectangles: &[Rect],
    ) {
        let full = resize(
            source,
            u32::from(destination.0),
            u32::from(destination.1),
            FilterType::Triangle,
        );
        assert!(scratch.prepare(source.dimensions(), destination, rectangles));
        for &rect @ (x, y, w, h) in rectangles {
            let expected = crop_imm(
                &full,
                u32::from(x),
                u32::from(y),
                u32::from(w),
                u32::from(h),
            )
            .to_image();
            assert_eq!(
                scratch.render(source, destination, rect).as_raw(),
                expected.as_raw(),
                "{:?} -> {destination:?}: {rect:?}",
                source.dimensions()
            );
        }
    }

    #[test]
    fn triangle_patch_exhaustive_small_pixels() {
        let mut scratch = TrianglePatch::default();
        let mut seed = 71;
        for sw in 1..=8 {
            for sh in 1..=8 {
                let source = fixture(sw, sh, &mut seed);
                for dw in 1..=8 {
                    for dh in 1..=8 {
                        if dw == 1 && dh == 1 {
                            continue;
                        } // Full frame uses existing resize.
                        let regions: Vec<_> = (0..dh)
                            .flat_map(|y| (0..dw).map(move |x| (x, y, 1, 1)))
                            .collect();
                        compare(&mut scratch, &source, (dw, dh), &regions);
                    }
                }
            }
        }
    }

    #[test]
    fn triangle_patch_exhaustive_small_rectangles() {
        let mut scratch = TrianglePatch::default();
        let mut seed = 93;
        for sw in 1..=4 {
            for sh in 1..=4 {
                let source = fixture(sw, sh, &mut seed);
                for dw in 1..=4 {
                    for dh in 1..=4 {
                        let mut regions = Vec::new();
                        for x in 0..dw {
                            for y in 0..dh {
                                for w in 1..=dw - x {
                                    for h in 1..=dh - y {
                                        if (x, y, w, h) != (0, 0, dw, dh) {
                                            regions.push((x, y, w, h));
                                        }
                                    }
                                }
                            }
                        }
                        compare(&mut scratch, &source, (dw, dh), &regions);
                    }
                }
            }
        }
    }

    #[test]
    fn triangle_patch_random_edges_reuse_and_rounding() {
        let mut scratch = TrianglePatch::default();
        let mut seed = 15_u32;
        for iteration in 0..160 {
            let sw = 1 + (iteration * 37 % 193);
            let sh = 1 + (iteration * 29 % 137);
            let dw = (2 + iteration * 17 % 211) as u16;
            let dh = (2 + iteration * 41 % 173) as u16;
            let source = fixture(sw, sh, &mut seed);
            let regions = [
                (0, 0, dw, 1),
                (0, dh - 1, dw, 1),
                (0, 0, 1, dh),
                (dw - 1, 0, 1, dh),
                (dw / 3, dh / 3, dw / 2, dh / 2),
            ];
            compare(&mut scratch, &source, (dw, dh), &regions);
        }
        for value in [0, 1, 2, 127, 128, 254, 255] {
            let source = RgbImage::from_fn(7, 5, |x, y| {
                Rgb([if (x + y) % 2 == 0 { value } else { 255 - value }; 3])
            });
            compare(
                &mut scratch,
                &source,
                (14, 10),
                &[(0, 0, 13, 10), (13, 0, 1, 10)],
            );
        }
    }

    #[test]
    fn triangle_patch_reconstructs_full_output() {
        let mut scratch = TrianglePatch::default();
        let source = fixture(101, 73, &mut 19);
        for (dw, dh) in [(49, 35), (203, 147), (101, 73), (1, 9), (9, 1)] {
            let full = resize(&source, u32::from(dw), u32::from(dh), FilterType::Triangle);
            let regions = if dw > 1 {
                vec![(0, 0, dw / 2, dh), (dw / 2, 0, dw - dw / 2, dh)]
            } else {
                vec![(0, 0, dw, dh / 2), (0, dh / 2, dw, dh - dh / 2)]
            };
            assert!(scratch.prepare(source.dimensions(), (dw, dh), &regions));
            let mut reconstructed = RgbImage::new(u32::from(dw), u32::from(dh));
            for rect @ (x, y, w, h) in regions {
                let patch = scratch.render(&source, (dw, dh), rect);
                for py in 0..h {
                    for px in 0..w {
                        reconstructed.put_pixel(
                            u32::from(x + px),
                            u32::from(y + py),
                            *patch.get_pixel(u32::from(px), u32::from(py)),
                        );
                    }
                }
            }
            assert_eq!(reconstructed, full);
        }
    }

    #[test]
    fn triangle_patch_preflight_reuses_reserved_buffers() {
        let mut scratch = TrianglePatch::default();
        let source = fixture(80, 60, &mut 77);
        let rectangles = [(0, 0, 79, 59), (13, 17, 3, 5), (79, 59, 1, 1)];
        assert!(scratch.prepare(source.dimensions(), (80, 60), &rectangles));
        let capacities = |s: &TrianglePatch| {
            (
                s.output.as_raw().capacity(),
                s.row.capacity(),
                s.horizontal.capacity(),
                s.weights.capacity(),
                s.vertical.capacity(),
            )
        };
        let reserved = capacities(&scratch);
        for _ in 0..3 {
            for &rect in &rectangles {
                scratch.render(&source, (80, 60), rect);
                assert_eq!(capacities(&scratch), reserved);
            }
            assert!(scratch.prepare(source.dimensions(), (80, 60), &rectangles));
            assert_eq!(capacities(&scratch), reserved);
        }
    }

    #[test]
    fn triangle_patch_bounds_retained_capacity_across_shapes() {
        let mut scratch = TrianglePatch::default();
        for _ in 0..3 {
            assert!(scratch.prepare((65535, 65535), (65535, 2), &[(0, 0, 65534, 1)]));
            assert!(scratch.retained_bytes() <= SCRATCH_LIMIT as u64);
            assert!(scratch.prepare((2200, 2200), (2200, 2200), &[(0, 0, 2199, 2200)]));
            assert!(scratch.retained_bytes() <= SCRATCH_LIMIT as u64);
            assert!(scratch.prepare((1, 1), (2, 2), &[(0, 0, 1, 1)]));
            assert!(scratch.retained_bytes() <= SCRATCH_LIMIT as u64);
        }
    }

    #[test]
    fn triangle_patch_rejects_invalid_full_and_over_budget_plans() {
        let mut scratch = TrianglePatch::default();
        for rect in [
            (0, 0, 0, 1),
            (0, 0, 1, 0),
            (2, 0, 1, 1),
            (0, 2, 1, 1),
            (0, 0, 2, 2),
            (u16::MAX, 0, 1, 1),
        ] {
            assert!(!scratch.prepare((4, 4), (2, 2), &[rect]));
        }
        for source in [(0, 4), (4, 0), (65536, 1), (1, 65536)] {
            assert!(!scratch.prepare(source, (2, 2), &[(0, 0, 1, 1)]));
        }
        assert!(!scratch.prepare((4, 4), (0, 2), &[]));
        assert!(!scratch.prepare((4, 4), (2, 0), &[]));
        assert!(!scratch.prepare((65535, 65535), (65535, 65535), &[(0, 0, 65534, 65535)]));
        // A later invalid region must reject the entire plan.
        assert!(!scratch.prepare((4, 4), (2, 2), &[(0, 0, 1, 1), (2, 2, 1, 1)]));
        assert!(scratch.prepare((4, 4), (2, 2), &[]));
    }
}
