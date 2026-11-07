//! Area-average downscaling for captured frames on the CPU.
//!
//! The host scales in the X server with XRender where it can. Where it
//! cannot -- or must not, see `ScalingPolicy` -- the frame is read back at
//! full size and shrunk here. `image`'s general-purpose Triangle filter took
//! about 90 ms for a 2560x1440 monitor, which capped the whole session near
//! ten frames a second. Averaging each output pixel's exact source area is
//! the right filter for a pure reduction, needs no floating point, and splits
//! into two passes. The reusable scaler uses a serial path, avoiding thread
//! creation and per-frame scratch allocations.

use image::RgbImage;

/// Fixed-point unity for coverage weights.
const ONE: u32 = 1 << 16;

/// The source samples one output sample averages, with their coverage.
struct Spans {
    /// First source index of each output sample.
    starts: Vec<usize>,
    /// Offset of each output sample's weights in `weights`; one extra entry
    /// closes the last.
    offsets: Vec<usize>,
    /// Coverage of consecutive source samples, in `ONE` units, summing to
    /// exactly `ONE` per output sample.
    weights: Vec<u32>,
}

impl Spans {
    /// Spans that shrink `source` samples to `output`.
    fn new(source: usize, output: usize) -> Self {
        debug_assert!(output > 0 && output <= source);
        let mut starts = Vec::with_capacity(output);
        let mut offsets = Vec::with_capacity(output + 1);
        let mut weights = Vec::with_capacity(source + output);
        // Work in units of 1/output source samples: output sample i covers
        // [i*source, (i+1)*source) and source sample j covers
        // [j*output, (j+1)*output). Everything stays an exact integer.
        let (source_u, output_u) = (source as u64, output as u64);
        for index in 0..output {
            let begin = index as u64 * source_u;
            let end = begin + source_u;
            let first = (begin / output_u) as usize;
            let last = end.div_ceil(output_u) as usize;
            starts.push(first);
            offsets.push(weights.len());
            let mut assigned = 0;
            for sample in first..last.min(source) {
                let covered =
                    end.min((sample as u64 + 1) * output_u) - begin.max(sample as u64 * output_u);
                let weight = (covered * u64::from(ONE) / source_u) as u32;
                weights.push(weight);
                assigned += weight;
            }
            // Rounding leaves a remainder of a few units; the last sample
            // takes it so a flat colour stays exactly flat.
            if let Some(last) = weights.last_mut() {
                *last += ONE - assigned;
            }
        }
        offsets.push(weights.len());
        Self {
            starts,
            offsets,
            weights,
        }
    }

    fn of(&self, output: usize) -> (usize, &[u32]) {
        (
            self.starts[output],
            &self.weights[self.offsets[output]..self.offsets[output + 1]],
        )
    }
}

/// Reusable two-pass CPU scaler. The cache is bounded to one geometry, so
/// repeated monitor/quality changes cannot accumulate weight tables.
#[derive(Default)]
pub(crate) struct CpuScaler {
    dimensions: Option<(u32, u32, u32, u32)>,
    spans: Option<(Spans, Spans)>,
    narrow: Vec<u8>,
    accumulator: Vec<u32>,
}

impl CpuScaler {
    /// Shrink into caller-owned storage. The returned image owns its pixels;
    /// callers may recycle its allocation only after all readers release it.
    pub(crate) fn scale_into(
        &mut self,
        image: &RgbImage,
        width: u32,
        height: u32,
        mut output: Vec<u8>,
    ) -> RgbImage {
        let dimensions = (image.width(), image.height(), width, height);
        let (source_width, source_height) = (image.width() as usize, image.height() as usize);
        let (width, height) = (width as usize, height as usize);
        assert!(
            width > 0 && height > 0 && width <= source_width && height <= source_height,
            "area_downscale only shrinks"
        );
        if self.dimensions != Some(dimensions) {
            self.spans = Some((
                Spans::new(source_width, width),
                Spans::new(source_height, height),
            ));
            self.narrow.resize(source_height * width * 3, 0);
            self.accumulator.resize(width * 3, 0);
            self.dimensions = Some(dimensions);
        }
        let (columns, rows) = self.spans.as_ref().expect("scaler geometry initialized");
        for (row, out_row) in image
            .as_raw()
            .chunks_exact(source_width * 3)
            .zip(self.narrow.chunks_exact_mut(width * 3))
        {
            shrink_row(row, out_row, columns);
        }
        output.resize(height * width * 3, 0);
        for (index, out_row) in output.chunks_exact_mut(width * 3).enumerate() {
            let (start, weights) = rows.of(index);
            self.accumulator.fill(ONE / 2);
            for (k, &weight) in weights.iter().enumerate() {
                let row = &self.narrow[(start + k) * width * 3..][..width * 3];
                for (sum, &value) in self.accumulator.iter_mut().zip(row) {
                    *sum += weight * u32::from(value);
                }
            }
            for (pixel, &sum) in out_row.iter_mut().zip(&self.accumulator) {
                *pixel = (sum >> 16) as u8;
            }
        }
        RgbImage::from_raw(width as u32, height as u32, output)
            .expect("output buffer matches its dimensions")
    }
}

#[cfg(test)]
fn area_downscale(image: &RgbImage, width: u32, height: u32) -> RgbImage {
    CpuScaler::default().scale_into(image, width, height, Vec::new())
}

fn shrink_row(row: &[u8], out_row: &mut [u8], columns: &Spans) {
    for (output, pixel) in out_row.chunks_exact_mut(3).enumerate() {
        let (start, weights) = columns.of(output);
        let mut sums = [ONE / 2; 3];
        for (k, &weight) in weights.iter().enumerate() {
            let source = &row[(start + k) * 3..][..3];
            sums[0] += weight * u32::from(source[0]);
            sums[1] += weight * u32::from(source[1]);
            sums[2] += weight * u32::from(source[2]);
        }
        pixel[0] = (sums[0] >> 16) as u8;
        pixel[1] = (sums[1] >> 16) as u8;
        pixel[2] = (sums[2] >> 16) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    #[test]
    fn scaler_reuses_storage_and_invalidates_weights_on_geometry_changes() {
        let mut scaler = CpuScaler::default();
        let image = RgbImage::from_pixel(7, 5, Rgb([15, 80, 240]));
        let first = scaler.scale_into(&image, 3, 2, Vec::new());
        let output_ptr = first.as_raw().as_ptr();
        let narrow_ptr = scaler.narrow.as_ptr();
        let accumulator_ptr = scaler.accumulator.as_ptr();
        let weights_ptr = scaler.spans.as_ref().unwrap().0.weights.as_ptr();
        let second = scaler.scale_into(&image, 3, 2, first.into_raw());
        assert_eq!(second.as_raw().as_ptr(), output_ptr);
        assert_eq!(scaler.narrow.as_ptr(), narrow_ptr);
        assert_eq!(scaler.accumulator.as_ptr(), accumulator_ptr);
        assert_eq!(
            scaler.spans.as_ref().unwrap().0.weights.as_ptr(),
            weights_ptr
        );
        let changed = RgbImage::from_fn(9, 6, |x, y| Rgb([(x * 20) as u8, (y * 30) as u8, 7]));
        let third = scaler.scale_into(&changed, 3, 2, second.into_raw());
        assert_eq!(scaler.dimensions, Some((9, 6, 3, 2)));
        assert_eq!(third, area_downscale(&changed, 3, 2));
        // Shrinking the scratch buffers must not leave stale trailing rows.
        assert_eq!(
            scaler.scale_into(&image, 2, 1, third.into_raw()),
            area_downscale(&image, 2, 1)
        );
    }

    #[test]
    fn retained_output_is_not_modified_by_the_next_frame() {
        let mut scaler = CpuScaler::default();
        let first = scaler.scale_into(
            &RgbImage::from_pixel(4, 4, Rgb([1, 2, 3])),
            2,
            2,
            Vec::new(),
        );
        let second = scaler.scale_into(
            &RgbImage::from_pixel(4, 4, Rgb([7, 8, 9])),
            2,
            2,
            Vec::new(),
        );
        assert!(first.pixels().all(|pixel| *pixel == Rgb([1, 2, 3])));
        assert!(second.pixels().all(|pixel| *pixel == Rgb([7, 8, 9])));
    }

    #[test]
    fn coverage_weights_sum_to_unity_for_every_ratio() {
        for (source, output) in [
            (2560, 2304),
            (2560, 1440),
            (1440, 810),
            (7, 3),
            (5, 5),
            (3, 1),
        ] {
            let spans = Spans::new(source, output);
            for index in 0..output {
                let (start, weights) = spans.of(index);
                assert_eq!(
                    weights.iter().sum::<u32>(),
                    ONE,
                    "{source}->{output} #{index}"
                );
                assert!(start + weights.len() <= source);
            }
        }
    }

    #[test]
    fn halving_averages_each_two_by_two_block() {
        let mut image = RgbImage::new(4, 2);
        for (x, value) in [(0, 0), (1, 100), (2, 200), (3, 40)] {
            image.put_pixel(x, 0, Rgb([value, 255 - value, 7]));
            image.put_pixel(x, 1, Rgb([value, 255 - value, 9]));
        }
        let half = area_downscale(&image, 2, 1);
        assert_eq!(half.get_pixel(0, 0), &Rgb([50, 205, 8]));
        assert_eq!(half.get_pixel(1, 0), &Rgb([120, 135, 8]));
    }

    #[test]
    fn a_flat_frame_stays_exactly_flat_at_any_ratio() {
        let image = RgbImage::from_pixel(2560, 1440, Rgb([12, 200, 255]));
        let small = area_downscale(&image, 2304, 1296);
        assert!(small.pixels().all(|pixel| *pixel == Rgb([12, 200, 255])));
    }

    #[test]
    fn a_fractional_ratio_blends_by_covered_area() {
        // Three source pixels into two: the middle one is split evenly.
        let mut image = RgbImage::new(3, 1);
        image.put_pixel(0, 0, Rgb([0, 0, 0]));
        image.put_pixel(1, 0, Rgb([90, 90, 90]));
        image.put_pixel(2, 0, Rgb([180, 180, 180]));
        let two = area_downscale(&image, 2, 1);
        // (0 * 1 + 90 * 0.5) / 1.5 = 30, (90 * 0.5 + 180 * 1) / 1.5 = 150.
        assert_eq!(two.get_pixel(0, 0), &Rgb([30, 30, 30]));
        assert_eq!(two.get_pixel(1, 0), &Rgb([150, 150, 150]));
    }

    #[test]
    fn identity_size_returns_the_same_pixels() {
        let mut image = RgbImage::new(5, 3);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            *pixel = Rgb([(x * 40) as u8, (y * 80) as u8, ((x + y) * 20) as u8]);
        }
        assert_eq!(area_downscale(&image, 5, 3), image);
    }
}
