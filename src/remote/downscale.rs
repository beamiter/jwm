//! Area-average downscaling for captured frames on the CPU.
//!
//! The host scales in the X server with XRender where it can. Where it
//! cannot -- or must not, see `ScalingPolicy` -- the frame is read back at
//! full size and shrunk here. `image`'s general-purpose Triangle filter took
//! about 90 ms for a 2560x1440 monitor, which capped the whole session near
//! ten frames a second. Averaging each output pixel's exact source area is
//! the right filter for a pure reduction, needs no floating point, and splits
//! into two passes. Large frames use a persistent bounded worker pool; small
//! frames stay serial. Neither path creates threads or scratch per frame.

use image::RgbImage;
use rayon::prelude::*;

/// Fixed-point unity for coverage weights.
const ONE: u32 = 1 << 16;
const MAX_WORKERS: usize = 4;
const PARALLEL_MIN_PIXELS: usize = 512 * 1024;
const MIN_ROWS_PER_WORKER: usize = 64;

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
    pool: Option<rayon::ThreadPool>,
    pool_attempted: bool,
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
        let parallel = source_width * source_height >= PARALLEL_MIN_PIXELS
            && source_height >= MIN_ROWS_PER_WORKER * 2
            && height >= MIN_ROWS_PER_WORKER * 2;
        if parallel && !self.pool_attempted {
            // Try once per session, not once per frame, including on resource
            // exhaustion. No global Rayon pool or environment-sized pool is used.
            self.pool_attempted = true;
            let workers = std::thread::available_parallelism()
                .map_or(1, usize::from)
                .min(MAX_WORKERS);
            if workers > 1 {
                self.pool = rayon::ThreadPoolBuilder::new()
                    .num_threads(workers)
                    .thread_name(|index| format!("jwm-remote-scale-{index}"))
                    .build()
                    .ok();
            }
        }
        let workers = self
            .pool
            .as_ref()
            .map_or(1, rayon::ThreadPool::current_num_threads);
        if self.dimensions != Some(dimensions) {
            self.spans = Some((
                Spans::new(source_width, width),
                Spans::new(source_height, height),
            ));
            self.narrow.resize(source_height * width * 3, 0);
            self.accumulator.resize(width * 3 * workers, 0);
            self.dimensions = Some(dimensions);
        }
        let (columns, rows) = self.spans.as_ref().expect("scaler geometry initialized");
        output.resize(height * width * 3, 0);
        if let Some(pool) = self.pool.as_ref().filter(|_| parallel) {
            let narrow = &mut self.narrow;
            let accumulator = &mut self.accumulator;
            pool.install(|| {
                let narrow_rows = source_height.div_ceil(workers);
                narrow
                    .par_chunks_mut(narrow_rows * width * 3)
                    .enumerate()
                    .for_each(|(chunk, target)| {
                        let first = chunk * narrow_rows * source_width * 3;
                        shrink_horizontal(
                            &image.as_raw()[first..],
                            target,
                            columns,
                            source_width,
                            width,
                        );
                    });
                let output_rows = height.div_ceil(workers);
                output
                    .par_chunks_mut(output_rows * width * 3)
                    .zip(accumulator.par_chunks_mut(width * 3))
                    .enumerate()
                    .for_each(|(chunk, (target, scratch))| {
                        shrink_vertical(narrow, target, rows, scratch, width, chunk * output_rows);
                    });
            });
        } else {
            shrink_horizontal(
                image.as_raw(),
                &mut self.narrow,
                columns,
                source_width,
                width,
            );
            shrink_vertical(
                &self.narrow,
                &mut output,
                rows,
                &mut self.accumulator[..width * 3],
                width,
                0,
            );
        }
        RgbImage::from_raw(width as u32, height as u32, output)
            .expect("output buffer matches its dimensions")
    }
}

fn shrink_horizontal(
    source: &[u8],
    output: &mut [u8],
    columns: &Spans,
    source_width: usize,
    width: usize,
) {
    for (row, out_row) in source
        .chunks_exact(source_width * 3)
        .zip(output.chunks_exact_mut(width * 3))
    {
        shrink_row(row, out_row, columns);
    }
}

fn shrink_vertical(
    narrow: &[u8],
    output: &mut [u8],
    rows: &Spans,
    accumulator: &mut [u32],
    width: usize,
    first_row: usize,
) {
    for (index, out_row) in output.chunks_exact_mut(width * 3).enumerate() {
        let (start, weights) = rows.of(first_row + index);
        accumulator.fill(ONE / 2);
        for (k, &weight) in weights.iter().enumerate() {
            let row = &narrow[(start + k) * width * 3..][..width * 3];
            for (sum, &value) in accumulator.iter_mut().zip(row) {
                *sum += weight * u32::from(value);
            }
        }
        for (pixel, &sum) in out_row.iter_mut().zip(accumulator.iter()) {
            *pixel = (sum >> 16) as u8;
        }
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

    fn serial_scaler() -> CpuScaler {
        CpuScaler {
            pool_attempted: true,
            ..CpuScaler::default()
        }
    }

    #[test]
    fn persistent_pool_matches_serial_across_geometry_changes() {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .unwrap();
        let mut parallel = CpuScaler {
            pool: Some(pool),
            pool_attempted: true,
            ..CpuScaler::default()
        };
        let mut serial = serial_scaler();
        let mut output = Vec::new();
        for (w, h, target_w, target_h) in [
            (1025, 513, 769, 257),
            (1920, 1080, 960, 540),
            (9, 7, 4, 3),
            (1025, 513, 769, 257),
        ] {
            let image = RgbImage::from_fn(w, h, |x, y| {
                Rgb([
                    (x.wrapping_mul(31) ^ y) as u8,
                    (y.wrapping_mul(13) ^ x) as u8,
                    (x + y) as u8,
                ])
            });
            let scaled = parallel.scale_into(&image, target_w, target_h, output);
            assert_eq!(
                scaled,
                serial.scale_into(&image, target_w, target_h, Vec::new())
            );
            let narrow = parallel.narrow.as_ptr();
            let scratch = parallel.accumulator.as_ptr();
            let output_ptr = scaled.as_raw().as_ptr();
            let repeated = parallel.scale_into(&image, target_w, target_h, scaled.into_raw());
            assert_eq!(parallel.narrow.as_ptr(), narrow);
            assert_eq!(parallel.accumulator.as_ptr(), scratch);
            assert_eq!(repeated.as_raw().as_ptr(), output_ptr);
            assert_eq!(parallel.pool.as_ref().unwrap().current_num_threads(), 2);
            output = repeated.into_raw();
        }
    }

    #[test]
    fn small_frames_do_not_initialize_workers_and_large_pool_is_bounded() {
        let mut scaler = CpuScaler::default();
        scaler.scale_into(&RgbImage::new(32, 32), 16, 16, Vec::new());
        assert!(!scaler.pool_attempted);
        assert!(scaler.pool.is_none());
        scaler.scale_into(&RgbImage::new(1024, 768), 512, 384, Vec::new());
        assert!(scaler.pool_attempted);
        if let Some(pool) = &scaler.pool {
            assert!(pool.current_num_threads() <= MAX_WORKERS);
        }
        // A failed/disabled initialization is sticky and still yields pixels.
        let mut fallback = serial_scaler();
        assert_eq!(
            fallback
                .scale_into(
                    &RgbImage::from_pixel(1024, 768, Rgb([4, 5, 6])),
                    512,
                    384,
                    Vec::new()
                )
                .get_pixel(0, 0),
            &Rgb([4, 5, 6])
        );
        assert!(fallback.pool.is_none());
    }

    /// Deterministic CPU-only microbenchmark, not X11/network performance.
    #[test]
    #[ignore = "release CPU microbenchmark; run explicitly with --release --ignored --nocapture"]
    fn benchmark_cached_serial_and_pool() {
        use std::hint::black_box;
        use std::time::Instant;
        for (w, h, target_w, target_h) in [
            (640, 360, 320, 180),
            (1920, 1080, 1280, 720),
            (2560, 1440, 1920, 1080),
            (3840, 2160, 1920, 1080),
        ] {
            let image = RgbImage::from_fn(w, h, |x, y| {
                Rgb([
                    (x.wrapping_mul(31) ^ y) as u8,
                    (y.wrapping_mul(13) ^ x) as u8,
                    (x + y) as u8,
                ])
            });
            let mut scalers = [serial_scaler(), CpuScaler::default()];
            let mut buffers = [Vec::new(), Vec::new()];
            let mut samples = [Vec::new(), Vec::new()];
            for iteration in 0..50 {
                for index in [iteration % 2, (iteration + 1) % 2] {
                    let started = Instant::now();
                    let result = scalers[index].scale_into(
                        black_box(&image),
                        target_w,
                        target_h,
                        std::mem::take(&mut buffers[index]),
                    );
                    let elapsed = started.elapsed().as_secs_f64() * 1000.0;
                    buffers[index] = black_box(result).into_raw();
                    if iteration >= 10 {
                        samples[index].push(elapsed);
                    }
                }
            }
            assert_eq!(buffers[0], buffers[1]);
            for values in &mut samples {
                values.sort_by(f64::total_cmp);
            }
            eprintln!(
                "{w}x{h}->{target_w}x{target_h}: serial p50={:.3}ms p95={:.3}ms; selected p50={:.3}ms p95={:.3}ms workers={}",
                samples[0][20],
                samples[0][38],
                samples[1][20],
                samples[1][38],
                scalers[1]
                    .pool
                    .as_ref()
                    .map_or(1, rayon::ThreadPool::current_num_threads)
            );
        }
    }

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
