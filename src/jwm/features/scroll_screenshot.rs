//! Passive, browser-independent scrolling capture. The page keeps its real
//! pointer input; only the screenshot confirmation keys stay grabbed by JWM.
//! Capture, decode and matching are serialized, with no private PNG files.

use super::connectivity::BackgroundJob;
use super::screenshot::{
    ScreenshotCompletion, ScreenshotCompletionJob, screenshot_completion_toast,
};
use crate::backend::api::Backend;
use crate::backend::compositor_common::capture::{MAX_RGBA_CAPTURE_BYTES, rgba_capture_len};
use crate::backend::compositor_common::screenshot::{
    ScreenshotCancellation, encode_png_bounded, save_png_atomically, screenshot_memory_channel,
};
use crate::core::types::Rect;
use crate::jwm::Jwm;
use image::{ImageDecoder, RgbaImage};
use std::time::{Duration, Instant};

const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
const SAMPLE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_HEIGHT: u32 = 65_535;
// Bound spare capacity so captures don't keep a large unused buffer.
const MAX_GROWTH_HEADROOM: usize = 8 * 1024 * 1024;

fn scroll_capacity_target(current: usize, needed: usize, limit: usize) -> usize {
    current
        .saturating_add(current / 2)
        .max(needed)
        .min(needed.saturating_add(MAX_GROWTH_HEADROOM))
        .min(limit)
}

#[derive(Debug)]
struct ScrollImage {
    pixels: Vec<u8>,
    previous: RgbaImage,
    /// The page reached the height or memory limit. What was stitched so far
    /// is kept and can still be saved; later viewports add nothing.
    full: bool,
}

impl ScrollImage {
    fn new(frame: RgbaImage) -> Result<Self, String> {
        rgba_capture_len(frame.width(), frame.height()).map_err(|e| e.to_string())?;
        let mut pixels = Vec::new();
        pixels
            .try_reserve_exact(frame.len())
            .map_err(|e| e.to_string())?;
        pixels.extend_from_slice(frame.as_raw());
        Ok(Self {
            pixels,
            previous: frame,
            full: false,
        })
    }

    fn dimensions(&self) -> (u32, u32) {
        let width = self.previous.width();
        (width, (self.pixels.len() / (width as usize * 4)) as u32)
    }

    /// An unmatched frame leaves the last accepted viewport intact. Scrolling
    /// back to an overlapping position can therefore recover without a seam.
    /// A viewport that would pass the height or memory limit marks the image
    /// full and is otherwise ignored, so the page captured so far survives.
    fn append(&mut self, frame: RgbaImage) -> Result<bool, String> {
        if frame.dimensions() != self.previous.dimensions() {
            return Err("The capture size changed; select the page again".into());
        }
        // Once full, nothing more is stitched, and any viewport — including
        // the final one taken on Enter — leaves the image ready to save.
        if self.full {
            return Ok(true);
        }
        if alignment_error(&self.previous, &frame, 0, 96, 96) < 0.25 {
            return Ok(true); // paused, at the bottom, or returned to the last viewport
        }
        let Some(offset) = scroll_offset(&self.previous, &frame) else {
            return Ok(false);
        };
        let (width, height) = self.dimensions();
        let reserved = height
            .checked_add(offset)
            .filter(|h| *h <= MAX_HEIGHT)
            .and_then(|height| rgba_capture_len(width, height).ok())
            .is_some_and(|len| self.reserve_append(width, len));
        if !reserved {
            self.full = true;
            return Ok(true);
        }
        let start = ((frame.height() - offset) as usize) * width as usize * 4;
        self.pixels.extend_from_slice(&frame.as_raw()[start..]);
        self.previous = frame;
        Ok(true)
    }

    fn reserve_append(&mut self, width: u32, needed: usize) -> bool {
        let current = self.pixels.capacity();
        if needed <= current {
            return true;
        }
        let limit = (width as usize)
            .saturating_mul(4)
            .saturating_mul(MAX_HEIGHT as usize)
            .min(MAX_RGBA_CAPTURE_BYTES);
        let planned = scroll_capacity_target(current, needed, limit);
        let len = self.pixels.len();
        // If there is only enough memory for the incoming rows, still accept
        // them rather than declaring the capture full for spare capacity.
        self.pixels.try_reserve_exact(planned - len).is_ok()
            || (planned > needed && self.pixels.try_reserve_exact(needed - len).is_ok())
    }
}

/// Trim the sides for scrollbar/edge noise. A clipped per-pixel RGB distance
/// tolerates small rendering changes while rejecting unrelated page content.
fn alignment_error(old: &RgbaImage, new: &RgbaImage, offset: u32, cols: u32, rows: u32) -> f64 {
    let margin = old.width() / 20;
    let width = old.width() - margin * 2;
    let overlap = old.height() - offset;
    let mut error = 0u64;
    let cols = cols.min(width);
    let rows = rows.min(overlap);
    for row in 0..rows {
        let y = row * overlap / rows;
        for col in 0..cols {
            let x = margin + col * width / cols;
            let a = old.get_pixel(x, y + offset).0;
            let b = new.get_pixel(x, y).0;
            let distance: u32 = (0..3).map(|c| u32::from(a[c].abs_diff(b[c]))).sum();
            error += u64::from(distance.min(96));
        }
    }
    error as f64 / f64::from(cols * rows)
}

fn scroll_offset(old: &RgbaImage, new: &RgbaImage) -> Option<u32> {
    let min_overlap = (old.height() / 4).max(32);
    let max_offset = old.height().checked_sub(min_overlap)?;
    let mut best = (f64::INFINITY, 0);
    let mut scores = Vec::with_capacity(max_offset as usize);
    for offset in 1..=max_offset {
        let score = alignment_error(old, new, offset, 32, 24);
        scores.push((score, offset));
        if score < best.0 {
            best = (score, offset);
        }
    }
    // Blank or repeating content has no unique alignment. Do not invent a
    // displacement that would silently duplicate or omit part of the page.
    if best.0 > 2.0
        || scores
            .iter()
            .any(|&(score, offset)| offset.abs_diff(best.1) > 2 && score <= best.0 + 0.5)
    {
        return None;
    }
    (alignment_error(old, new, best.1, 96, 96) <= 2.0).then_some(best.1)
}

fn decode_frame(png: Vec<u8>) -> Result<RgbaImage, String> {
    let mut reader =
        image::ImageReader::with_format(std::io::Cursor::new(png), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_RGBA_CAPTURE_BYTES as u64);
    reader.limits(limits);
    let decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let (width, height) = decoder.dimensions();
    rgba_capture_len(width, height).map_err(|e| e.to_string())?;
    image::DynamicImage::from_decoder(decoder)
        .map(|image| image.into_rgba8())
        .map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Copy)]
enum ScrollFinish {
    Complete { to_clipboard: bool },
    Partial { to_clipboard: bool },
}

#[derive(Debug)]
pub(crate) struct ScrollCapture {
    region: Rect,
    image: Option<ScrollImage>,
    job: Option<BackgroundJob<Result<(ScrollImage, bool), String>>>,
    cancellation: Option<ScreenshotCancellation>,
    next_sample: Instant,
    pointer_released: bool,
    finish: Option<ScrollFinish>,
    final_sample: bool,
    match_failed: bool,
}

impl ScrollCapture {
    pub(crate) fn next_wakeup(&self, now: Instant) -> Option<Duration> {
        if self.job.is_some() {
            Some(Duration::from_millis(20))
        } else if self.finish.is_some() {
            Some(Duration::ZERO)
        } else if self.image.as_ref().is_some_and(|image| image.full) {
            None
        } else {
            Some(self.next_sample.saturating_duration_since(now))
        }
    }
}

impl Drop for ScrollCapture {
    fn drop(&mut self) {
        if let Some(cancellation) = &self.cancellation {
            cancellation.cancel();
        }
    }
}

impl Jwm {
    pub(crate) fn start_scroll_screenshot(&mut self, backend: &mut dyn Backend) {
        if !self.features.screenshot.active
            || !self.features.screenshot.committed
            || self.features.screenshot.scrolling
        {
            return;
        }
        let Some(region) = self
            .features
            .screenshot
            .get_selection_rect()
            .filter(|r| r.w >= 32 && r.h >= 64)
        else {
            backend.compositor_set_capture_hint(Some(
                "Long screenshot · select a larger page area first".into(),
            ));
            return;
        };
        if self.ensure_screenshot_completion_capacity(backend).is_err() {
            return;
        }
        self.features.screenshot.commit_annotation();
        self.features.screenshot.commit_text_draft();
        self.features.screenshot.scrolling = true;
        backend.compositor_set_scroll_capture_active(true);
        self.features.screenshot.toolbar = None;
        self.features.capture.clear_confirm_double_click();
        backend.compositor_set_screenshot_freeze(false);
        backend.compositor_set_capture_selection_active(false);
        backend.compositor_set_annotation_mode(false);
        backend.compositor_set_screenshot_toolbar(None);
        backend.compositor_clear_snap_preview_immediate();
        backend
            .compositor_set_capture_hint(Some("Long screenshot · preparing first frame…".into()));
        self.features.scroll_capture = Some(ScrollCapture {
            region,
            image: None,
            job: None,
            cancellation: None,
            next_sample: Instant::now(),
            pointer_released: false,
            finish: None,
            final_sample: false,
            match_failed: false,
        });
    }

    pub(crate) fn finish_scroll_screenshot(
        &mut self,
        backend: &mut dyn Backend,
        to_clipboard: bool,
    ) {
        if let Some(capture) = &mut self.features.scroll_capture {
            // Unless the image is full, take one more sample to include
            // a scroll just before Enter. Repeated keys cannot enqueue exports.
            if capture.finish.is_none() {
                capture.finish = Some(ScrollFinish::Complete { to_clipboard });
                capture.next_sample = Instant::now();
                backend.compositor_set_capture_hint(Some("Long screenshot · finishing…".into()));
            }
        }
    }

    pub(crate) fn finish_partial_scroll_screenshot(
        &mut self,
        backend: &mut dyn Backend,
        to_clipboard: bool,
    ) {
        if let Some(capture) = &mut self.features.scroll_capture
            && capture.match_failed
            && capture.finish.is_none()
        {
            capture.finish = Some(ScrollFinish::Partial { to_clipboard });
            backend.compositor_set_capture_hint(Some(
                "Long screenshot · exporting captured portion…".into(),
            ));
        }
    }

    pub(crate) fn tick_scroll_screenshot(&mut self, backend: &mut dyn Backend, now: Instant) {
        let Some(mut capture) = self.features.scroll_capture.take() else {
            return;
        };
        if let Some(job) = &capture.job {
            match job.take() {
                Some(Ok((image, matched))) => {
                    capture.job = None;
                    capture.cancellation = None;
                    let (width, height) = image.dimensions();
                    capture.image = Some(image);
                    if !capture.pointer_released {
                        if let Err(error) = backend.input_ops().ungrab_pointer() {
                            self.scroll_screenshot_failed(backend, error.to_string());
                            return;
                        }
                        capture.pointer_released = true;
                    }
                    if capture.final_sample
                        && matched
                        && let Some(ScrollFinish::Complete { to_clipboard }) = capture.finish
                    {
                        let image = capture.image.take().expect("completed sample owns image");
                        self.export_scroll_screenshot(backend, image, to_clipboard);
                        return;
                    }
                    if capture.final_sample {
                        capture.finish = None;
                        capture.final_sample = false;
                    }
                    capture.match_failed = !matched;
                    let full = capture.image.as_ref().is_some_and(|image| image.full);
                    backend.compositor_set_capture_hint(Some(if full {
                        format!("Long screenshot {width}×{height} · reached the size limit · Enter save · Ctrl+C copy · Esc cancel")
                    } else if matched {
                        format!("Long screenshot {width}×{height} · scroll down slowly · Enter save · Ctrl+C copy · Esc cancel")
                    } else {
                        "No overlap · Shift+Enter save partial · Ctrl+Shift+C copy · Scroll back / Esc".into()
                    }));
                    capture.next_sample = now + SAMPLE_INTERVAL;
                }
                Some(Err(error)) => {
                    self.scroll_screenshot_failed(backend, error);
                    return;
                }
                None if !job.started() => {
                    self.scroll_screenshot_failed(
                        backend,
                        "Could not start the long screenshot worker".into(),
                    );
                    return;
                }
                None => {
                    self.features.scroll_capture = Some(capture);
                    return;
                }
            }
        }
        // A partial finish is explicit. Use the last accepted pixels, after
        // collecting any already-running sample, but never start a new one.
        if let Some(ScrollFinish::Partial { to_clipboard }) = capture.finish {
            let image = capture.image.take().expect("failed match retains an image");
            self.export_scroll_screenshot(backend, image, to_clipboard);
            return;
        }
        // A full image cannot accept another viewport. Wait without polling
        // the capture backend, then export the retained pixels on confirmation.
        if capture.image.as_ref().is_some_and(|image| image.full) {
            if let Some(ScrollFinish::Complete { to_clipboard }) = capture.finish {
                let image = capture.image.take().expect("full capture owns its image");
                self.export_scroll_screenshot(backend, image, to_clipboard);
            } else {
                self.features.scroll_capture = Some(capture);
            }
            return;
        }
        if capture.job.is_none() && (capture.finish.is_some() || now >= capture.next_sample) {
            let (sender, receiver, cancellation) = screenshot_memory_channel();
            let region = capture.region;
            match backend.take_screenshot_region_to_memory(
                sender,
                region.x,
                region.y,
                region.w as u32,
                region.h as u32,
            ) {
                Ok(true) => {}
                result => {
                    self.scroll_screenshot_failed(
                        backend,
                        match result {
                            Err(error) => error.to_string(),
                            _ => "This backend does not support in-memory region capture".into(),
                        },
                    );
                    return;
                }
            }
            let image = capture.image.take();
            capture.final_sample = capture.finish.is_some();
            capture.cancellation = Some(cancellation);
            capture.job = Some(self.track_background_job(BackgroundJob::spawn(move || {
                let frame = decode_frame(receiver.recv_timeout(SAMPLE_TIMEOUT)?)?;
                match image {
                    Some(mut image) => {
                        let matched = image.append(frame)?;
                        Ok((image, matched))
                    }
                    None => Ok((ScrollImage::new(frame)?, true)),
                }
            })));
        }
        self.features.scroll_capture = Some(capture);
    }

    fn scroll_screenshot_failed(&mut self, backend: &mut dyn Backend, error: String) {
        self.cancel_screenshot_select(backend);
        self.push_system_toast(
            backend,
            screenshot_completion_toast(&ScreenshotCompletion::Failed(error)),
        );
    }

    fn export_scroll_screenshot(
        &mut self,
        backend: &mut dyn Backend,
        image: ScrollImage,
        to_clipboard: bool,
    ) {
        let path = self.features.screenshot.output_path.take();
        let annotations = self.features.screenshot.annotations.clone();
        let origin = self
            .features
            .screenshot
            .get_selection_rect()
            .map_or((0, 0), |r| (r.x, r.y));
        self.cancel_screenshot_select(backend);
        if self.ensure_screenshot_completion_capacity(backend).is_err() {
            return;
        }
        let cancellation = ScreenshotCancellation::default();
        let worker_cancel = cancellation.clone();
        let job = self.track_background_job(BackgroundJob::spawn(move || {
            let result = (|| -> Result<ScreenshotCompletion, String> {
                if worker_cancel.is_cancelled() {
                    return Err("Screenshot cancelled".into());
                }
                let (width, height) = image.dimensions();
                let mut rgba = RgbaImage::from_raw(width, height, image.pixels)
                    .ok_or("Invalid long screenshot dimensions")?;
                for annotation in &annotations {
                    Jwm::draw_annotation(&mut rgba, origin, annotation);
                }
                if worker_cancel.is_cancelled() {
                    return Err("Screenshot cancelled".into());
                }
                if to_clipboard {
                    let png = encode_png_bounded(rgba.as_raw(), width, height)
                        .map_err(|e| e.to_string())?;
                    Ok(ScreenshotCompletion::CopiedToClipboard {
                        png,
                        offered: false,
                        annotations_lost: false,
                    })
                } else {
                    let path = path.ok_or("Missing screenshot output path")?;
                    save_png_atomically(std::path::Path::new(&path), rgba.as_raw(), width, height)
                        .map_err(|e| e.to_string())?;
                    Ok(ScreenshotCompletion::Saved(path))
                }
            })();
            result.unwrap_or_else(ScreenshotCompletion::Failed)
        }));
        self.features
            .screenshot_completions
            .push(ScreenshotCompletionJob::private(job, cancellation));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    fn page() -> RgbaImage {
        RgbaImage::from_fn(120, 900, |x, y| {
            let n = x.wrapping_mul(7919) ^ y.wrapping_mul(104729);
            Rgba([(n >> 8) as u8, n as u8, (n >> 16) as u8, 255])
        })
    }

    fn viewport(page: &RgbaImage, y: u32) -> RgbaImage {
        image::imageops::crop_imm(page, 0, y, page.width(), 240).to_image()
    }

    #[test]
    fn multiple_scrolls_and_bottom_duplicates_reproduce_the_page() {
        let page = page();
        let mut image = ScrollImage::new(viewport(&page, 0)).unwrap();
        for y in [73, 73, 201, 365, 500, 660, 660] {
            assert!(image.append(viewport(&page, y)).unwrap(), "offset {y}");
        }
        assert_eq!(image.dimensions(), page.dimensions());
        assert_eq!(image.pixels, page.into_raw());
    }

    #[test]
    fn long_scrolls_grow_in_batches_and_keep_identical_pixels() {
        let page = page();
        let mut image = ScrollImage::new(viewport(&page, 0)).unwrap();
        let mut growths = 0;
        for y in (30..=660).step_by(30) {
            let before = image.pixels.capacity();
            assert!(image.append(viewport(&page, y)).unwrap(), "offset {y}");
            if image.pixels.capacity() > before {
                growths += 1;
            }
        }
        assert!(growths <= 5, "capacity grew {growths} times for 22 samples");
        assert_eq!(image.pixels, page.into_raw());
    }

    #[test]
    fn scroll_capacity_never_requests_unbounded_spare_space() {
        let limit = MAX_RGBA_CAPTURE_BYTES;
        assert_eq!(scroll_capacity_target(1024, 1030, limit), 1536);
        assert_eq!(
            scroll_capacity_target(limit / 2, limit / 2 + 16, limit),
            limit / 2 + 16 + MAX_GROWTH_HEADROOM
        );
        assert_eq!(scroll_capacity_target(limit - 32, limit - 16, limit), limit);
    }

    #[test]
    fn missed_overlap_and_upward_scroll_can_recover_without_mutating_output() {
        let page = page();
        let mut image = ScrollImage::new(viewport(&page, 100)).unwrap();
        let original = image.pixels.clone();
        assert!(!image.append(viewport(&page, 500)).unwrap());
        assert!(!image.append(viewport(&page, 0)).unwrap());
        assert_eq!(image.pixels, original);
        assert!(image.append(viewport(&page, 180)).unwrap());
        assert_eq!(image.dimensions(), (120, 320));
    }

    #[test]
    fn reaching_the_height_limit_keeps_the_page_captured_so_far() {
        let page = page();
        let mut image = ScrollImage::new(viewport(&page, 0)).unwrap();
        assert!(image.append(viewport(&page, 125)).unwrap());
        let kept = image.pixels.clone();
        // Pretend the stitched page already sits just under the limit.
        let tall_previous = image.previous.clone();
        image.pixels.resize(120 * 4 * (MAX_HEIGHT as usize - 10), 0);
        let near_limit = image.pixels.clone();
        image.previous = tall_previous;

        // The next viewport would pass the limit: the image is kept, marked
        // full, and still saveable; before, the whole capture was discarded.
        assert!(image.append(viewport(&page, 250)).unwrap());
        assert!(image.full);
        assert_eq!(image.pixels, near_limit);
        assert!(image.append(viewport(&page, 375)).unwrap());
        assert_eq!(image.pixels.len(), near_limit.len());
        assert_eq!(&image.pixels[..kept.len()], &kept[..]);
    }

    fn capture_with_sample(
        image: ScrollImage,
        matched: bool,
        now: Instant,
        final_sample: bool,
    ) -> ScrollCapture {
        ScrollCapture {
            region: Rect {
                x: 0,
                y: 0,
                w: 120,
                h: 240,
            },
            image: None,
            job: Some(BackgroundJob::completed_for_test(Ok((image, matched)))),
            cancellation: None,
            next_sample: now,
            pointer_released: true,
            finish: final_sample.then_some(ScrollFinish::Complete {
                to_clipboard: false,
            }),
            final_sample,
            match_failed: false,
        }
    }

    fn take_scroll_completion(jwm: &Jwm) -> ScreenshotCompletion {
        assert_eq!(jwm.features.screenshot_completions.len(), 1);
        let job = &jwm.features.screenshot_completions[0];
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            if let Some(completion) = job.take() {
                return completion;
            }
            assert!(
                Instant::now() < deadline,
                "long screenshot export did not finish"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn an_unmatched_final_frame_can_export_the_accepted_prefix_explicitly() {
        use crate::jwm::monitor::test_support::{DisplaySpyBackend, output};

        let page = page();
        let expected = image::imageops::crop_imm(&page, 0, 0, 120, 313).to_image();
        // Isolate the file destination from both the user's files and parallel tests.
        let dir = std::env::temp_dir().join(format!(
            "jwm-partial-scroll-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("partial.png");

        for to_clipboard in [true, false] {
            let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 800, 600)]);
            let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
            let mut image = ScrollImage::new(viewport(&page, 0)).unwrap();
            assert!(image.append(viewport(&page, 73)).unwrap());
            let matched = image.append(viewport(&page, 500)).unwrap();
            assert!(!matched);
            let now = Instant::now();
            jwm.features.screenshot.scrolling = true;
            jwm.features.screenshot.output_path = Some(path.to_string_lossy().into_owned());
            jwm.features.scroll_capture = Some(capture_with_sample(image, matched, now, true));

            jwm.tick_scroll_screenshot(&mut backend, now);
            assert!(jwm.features.screenshot_completions.is_empty());
            let capture = jwm.features.scroll_capture.as_ref().unwrap();
            assert!(capture.match_failed);
            assert!(capture.finish.is_none());
            assert_eq!(
                capture.image.as_ref().unwrap().pixels.as_slice(),
                expected.as_raw().as_slice()
            );

            // This backend cannot read the screen. No extra sample is needed,
            // and a second key cannot change the chosen destination.
            jwm.finish_partial_scroll_screenshot(&mut backend, to_clipboard);
            jwm.finish_partial_scroll_screenshot(&mut backend, !to_clipboard);
            jwm.tick_scroll_screenshot(&mut backend, now);
            assert!(jwm.features.scroll_capture.is_none());
            let png = match take_scroll_completion(&jwm) {
                ScreenshotCompletion::CopiedToClipboard { png, .. } if to_clipboard => {
                    assert!(!path.exists());
                    png
                }
                ScreenshotCompletion::Saved(saved) if !to_clipboard => {
                    assert_eq!(std::path::Path::new(&saved), path.as_path());
                    std::fs::read(&path).unwrap()
                }
                _ => panic!("expected partial export to the first chosen destination"),
            };
            assert_eq!(decode_frame(png).unwrap(), expected);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn partial_export_is_unavailable_before_a_miss_after_recovery_and_after_cancel() {
        use crate::jwm::monitor::test_support::{DisplaySpyBackend, output};

        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 800, 600)]);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let now = Instant::now();
        let page = page();
        let image = ScrollImage::new(viewport(&page, 0)).unwrap();
        jwm.features.screenshot.scrolling = true;
        jwm.features.scroll_capture = Some(capture_with_sample(image, true, now, false));
        jwm.finish_partial_scroll_screenshot(&mut backend, true);
        assert!(
            jwm.features
                .scroll_capture
                .as_ref()
                .unwrap()
                .finish
                .is_none()
        );
        jwm.tick_scroll_screenshot(&mut backend, now);

        let mut capture = jwm.features.scroll_capture.take().unwrap();
        let mut image = capture.image.take().unwrap();
        let matched = image.append(viewport(&page, 500)).unwrap();
        assert!(!matched);
        jwm.features.scroll_capture = Some(capture_with_sample(image, matched, now, false));
        jwm.tick_scroll_screenshot(&mut backend, now);
        assert!(jwm.features.scroll_capture.as_ref().unwrap().match_failed);

        let mut capture = jwm.features.scroll_capture.take().unwrap();
        let mut image = capture.image.take().unwrap();
        let matched = image.append(viewport(&page, 73)).unwrap();
        assert!(matched);
        capture.job = Some(BackgroundJob::completed_for_test(Ok((image, matched))));
        jwm.features.scroll_capture = Some(capture);
        jwm.tick_scroll_screenshot(&mut backend, now);
        assert!(!jwm.features.scroll_capture.as_ref().unwrap().match_failed);
        jwm.finish_partial_scroll_screenshot(&mut backend, true);
        assert!(
            jwm.features
                .scroll_capture
                .as_ref()
                .unwrap()
                .finish
                .is_none()
        );

        jwm.cancel_screenshot_select(&mut backend);
        jwm.finish_partial_scroll_screenshot(&mut backend, true);
        jwm.tick_scroll_screenshot(&mut backend, now);
        assert!(jwm.features.scroll_capture.is_none());
        assert!(jwm.features.screenshot_completions.is_empty());
    }

    #[test]
    fn a_full_scroll_capture_waits_without_sampling_then_exports_the_retained_pixels() {
        use crate::jwm::monitor::test_support::{DisplaySpyBackend, output};

        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 800, 600)]);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let frame = viewport(&page(), 0);
        let image = ScrollImage::new(frame.clone()).unwrap();
        let now = Instant::now();
        let mut capture = ScrollCapture {
            region: Rect {
                x: 0,
                y: 0,
                w: 120,
                h: 240,
            },
            image: Some(image),
            job: None,
            cancellation: None,
            next_sample: now + SAMPLE_INTERVAL,
            pointer_released: true,
            finish: None,
            final_sample: false,
            match_failed: false,
        };
        assert_eq!(capture.next_wakeup(now), Some(SAMPLE_INTERVAL));
        capture.image.as_mut().unwrap().full = true;
        assert_eq!(capture.next_wakeup(now), None);
        jwm.features.scroll_capture = Some(capture);
        jwm.features.screenshot.scrolling = true;

        // This backend has no region-capture support. Attempting a new
        // capture once the deadline passed would abort instead of waiting.
        jwm.tick_scroll_screenshot(&mut backend, now + SAMPLE_INTERVAL);
        let capture = jwm.features.scroll_capture.as_ref().unwrap();
        assert!(capture.job.is_none());
        assert!(capture.image.as_ref().unwrap().full);

        jwm.finish_scroll_screenshot(&mut backend, true);
        jwm.finish_scroll_screenshot(&mut backend, false);
        let capture = jwm.features.scroll_capture.as_ref().unwrap();
        assert_eq!(capture.next_wakeup(now), Some(Duration::ZERO));
        jwm.tick_scroll_screenshot(&mut backend, now + SAMPLE_INTERVAL);
        assert!(jwm.features.scroll_capture.is_none());
        assert_eq!(jwm.features.screenshot_completions.len(), 1);
        let job = &jwm.features.screenshot_completions[0];
        let deadline = Instant::now() + Duration::from_secs(4);
        let completion = loop {
            if let Some(completion) = job.take() {
                break completion;
            }
            assert!(Instant::now() < deadline, "clipboard export did not finish");
            std::thread::sleep(Duration::from_millis(2));
        };
        let ScreenshotCompletion::CopiedToClipboard { png, .. } = completion else {
            panic!("expected the first finish request to keep the clipboard destination");
        };
        assert_eq!(decode_frame(png).unwrap(), frame);
    }

    #[test]
    fn resize_is_rejected_and_repeating_content_is_ambiguous() {
        let mut image = ScrollImage::new(viewport(&page(), 0)).unwrap();
        assert!(image.append(RgbaImage::new(121, 240)).is_err());
        let stripes = RgbaImage::from_fn(120, 400, |_, y| Rgba([(y % 10 * 20) as u8, 0, 0, 255]));
        assert_eq!(
            scroll_offset(&viewport(&stripes, 0), &viewport(&stripes, 4)),
            None
        );
    }

    #[test]
    fn bounded_png_roundtrip_preserves_stitched_pixels() {
        let page = page();
        let mut image = ScrollImage::new(viewport(&page, 0)).unwrap();
        assert!(image.append(viewport(&page, 125)).unwrap());
        let (w, h) = image.dimensions();
        let png = encode_png_bounded(&image.pixels, w, h).unwrap();
        let decoded = decode_frame(png).unwrap();
        assert_eq!(decoded.dimensions(), (120, 365));
        assert_eq!(decoded.into_raw(), image.pixels);
    }

    #[test]
    fn text_on_white_and_a_changing_scrollbar_stitch_at_pixel_precision() {
        let page = RgbaImage::from_fn(480, 1700, |x, y| {
            let line = y / 19;
            let word = x / 53;
            let seed = line.wrapping_mul(7919) ^ word.wrapping_mul(104729);
            let ink =
                y % 19 < 11 && x > 15 && x < 470 && x % 53 < 15 + seed % 32 && (x + seed) % 7 < 5;
            Rgba(if ink {
                [35, 35, 35, 255]
            } else {
                [255, 255, 255, 255]
            })
        });
        let frame = |top| {
            let mut image = image::imageops::crop_imm(&page, 0, top, 480, 480).to_image();
            for y in 0..480 {
                image.put_pixel(479, y, Rgba([top as u8, 128, 128, 255]));
            }
            image
        };
        let mut image = ScrollImage::new(frame(0)).unwrap();
        for top in [93, 218, 401, 638, 915, 1220] {
            assert!(image.append(frame(top)).unwrap(), "text viewport {top}");
        }
        assert_eq!(image.dimensions(), page.dimensions());
        let stitched = RgbaImage::from_raw(480, 1700, image.pixels).unwrap();
        for y in 0..1700 {
            for x in 0..479 {
                assert_eq!(stitched.get_pixel(x, y), page.get_pixel(x, y));
            }
        }
    }
}
