//! Regular-file-only, resource-bounded image decoding for background workers.

use image::{ImageDecoder as _, metadata::Orientation};
use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

// Accommodate large multi-monitor wallpapers without accepting arbitrary image
// dimensions or compressed input. Output includes the eventual RGBA8 conversion.
const WALLPAPER_MAX_BYTES: u64 = 64 * 1024 * 1024;
const WALLPAPER_MAX_DIMENSION: u32 = 16_384;
const WALLPAPER_MAX_OUTPUT: u64 = 256 * 1024 * 1024;
const WALLPAPER_MAX_ALLOC: u64 = 512 * 1024 * 1024;

/// Keep image::open's extension-based format selection, but never wait for a
/// FIFO writer. Checking the opened descriptor also closes a metadata/open
/// replacement race; symlinks to ordinary image files remain supported.
/// Apply embedded orientation before callers calculate wallpaper geometry.
pub(crate) fn open_regular_image(
    path: impl AsRef<Path>,
) -> image::ImageResult<image::DynamicImage> {
    open_image_with_limits(
        path.as_ref(),
        WALLPAPER_MAX_DIMENSION,
        WALLPAPER_MAX_DIMENSION,
        WALLPAPER_MAX_BYTES,
        WALLPAPER_MAX_OUTPUT,
        WALLPAPER_MAX_ALLOC,
    )
}

/// Also return the identity of the descriptor whose bytes were decoded. A
/// cache must bind its pixels to this source, not only to pathname metadata
/// that can change while a symlink or directory entry is being replaced.
#[cfg(any(feature = "x11-backends", feature = "wayland-backends"))]
pub(crate) fn open_regular_image_with_metadata(
    path: impl AsRef<Path>,
) -> image::ImageResult<(image::DynamicImage, std::fs::Metadata)> {
    open_image_source_with_limits(
        path.as_ref(),
        WALLPAPER_MAX_DIMENSION,
        WALLPAPER_MAX_DIMENSION,
        WALLPAPER_MAX_BYTES,
        WALLPAPER_MAX_OUTPUT,
        WALLPAPER_MAX_ALLOC,
    )
}

/// Strict source/dimension bounds for bundled sky images and user replacements.
/// The 64 MiB decoder budget remains best-effort for codec-internal allocations;
/// decoded output and any orientation copy are explicitly reserved below.
pub(crate) fn open_regular_image_bounded(
    path: impl AsRef<Path>,
    max_width: u32,
    max_height: u32,
    max_bytes: u64,
) -> image::ImageResult<image::DynamicImage> {
    open_image_with_limits(
        path.as_ref(),
        max_width,
        max_height,
        max_bytes,
        64 * 1024 * 1024,
        64 * 1024 * 1024,
    )
}

fn invalid_source(message: &'static str) -> image::ImageError {
    io::Error::new(io::ErrorKind::InvalidInput, message).into()
}

// Read one extra byte to reject growth after the descriptor metadata check.
// Kept separate so the growth defense can be tested without a filesystem race.
fn read_bounded(source: impl Read, max_bytes: u64) -> image::ImageResult<Vec<u8>> {
    let mut bytes = Vec::new();
    source
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(invalid_source("image grew beyond its source byte limit"));
    }
    Ok(bytes)
}

fn open_image_with_limits(
    path: &Path,
    max_width: u32,
    max_height: u32,
    max_bytes: u64,
    max_output: u64,
    max_alloc: u64,
) -> image::ImageResult<image::DynamicImage> {
    open_image_source_with_limits(
        path, max_width, max_height, max_bytes, max_output, max_alloc,
    )
    .map(|(image, _)| image)
}

fn open_image_source_with_limits(
    path: &Path,
    max_width: u32,
    max_height: u32,
    max_bytes: u64,
    max_output: u64,
    max_alloc: u64,
) -> image::ImageResult<(image::DynamicImage, std::fs::Metadata)> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        return Err(invalid_source("image source is not a bounded regular file"));
    }
    let format = image::ImageFormat::from_path(path)?;
    let bytes = read_bounded(file, max_bytes)?;
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(max_width);
    limits.max_image_height = Some(max_height);
    limits.max_alloc = Some(max_alloc);
    reader.limits(limits.clone());
    let mut decoder = reader.into_decoder()?;
    let (width, height) = decoder.dimensions();
    limits.check_dimensions(width, height)?;
    let output_bytes = decoder.total_bytes();
    // u32 dimensions fit this multiplication in u64; multiply by channels
    // saturatingly so even malicious dimensions cannot wrap into a small limit.
    let rgba_bytes = u64::from(width)
        .saturating_mul(u64::from(height))
        .saturating_mul(4);
    if width == 0 || height == 0 || output_bytes.max(rgba_bytes) > max_output {
        return Err(invalid_source("image exceeds decoded output limit"));
    }
    let orientation = decoder.orientation()?;
    let swaps_axes = matches!(
        orientation,
        Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH
    );
    if swaps_axes {
        limits.check_dimensions(height, width)?;
        // image's quarter-turn orientations allocate a second pixel buffer.
        limits.reserve(output_bytes)?;
    }
    // Match ImageReader::decode's explicit output reservation. into_decoder
    // alone does not reserve the DynamicImage output allocation.
    limits.reserve(output_bytes)?;
    decoder.set_limits(limits)?;
    let mut decoded = image::DynamicImage::from_decoder(decoder)?;
    decoded.apply_orientation(orientation);
    Ok((decoded, metadata))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "jwm-image-source-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        dir
    }
    fn exif_orientation(value: u16) -> Vec<u8> {
        // TIFF little-endian header, one SHORT Orientation entry, no next IFD.
        let mut exif = vec![
            b'I', b'I', 42, 0, 8, 0, 0, 0, 1, 0, 0x12, 0x01, 3, 0, 1, 0, 0, 0,
        ];
        exif.extend_from_slice(&value.to_le_bytes());
        exif.extend_from_slice(&[0; 6]);
        exif
    }

    #[test]
    fn all_exif_orientations_are_applied_before_returning_pixels() {
        use image::ImageEncoder as _;
        let dir = temp_dir();
        let path = dir.join("oriented.png");
        let pixels =
            image::RgbImage::from_fn(2, 3, |x, y| image::Rgb([(y * 2 + x + 1) as u8, 0, 0]));
        let expected = [
            [1, 2, 3, 4, 5, 6],
            [2, 1, 4, 3, 6, 5],
            [6, 5, 4, 3, 2, 1],
            [5, 6, 3, 4, 1, 2],
            [1, 3, 5, 2, 4, 6],
            [5, 3, 1, 6, 4, 2],
            [6, 4, 2, 5, 3, 1],
            [2, 4, 6, 1, 3, 5],
        ];
        for value in 1..=8 {
            let mut encoded = Vec::new();
            let mut encoder = image::codecs::png::PngEncoder::new(&mut encoded);
            encoder.set_exif_metadata(exif_orientation(value)).unwrap();
            encoder
                .write_image(pixels.as_raw(), 2, 3, image::ExtendedColorType::Rgb8)
                .unwrap();
            std::fs::write(&path, encoded).unwrap();
            for decoded in [
                open_regular_image(&path).unwrap(),
                open_regular_image_bounded(&path, 3, 3, 4096).unwrap(),
            ] {
                let dimensions = if value >= 5 { (3, 2) } else { (2, 3) };
                assert_eq!((decoded.width(), decoded.height()), dimensions);
                let actual: Vec<u8> = decoded.to_rgb8().pixels().map(|p| p[0]).collect();
                assert_eq!(
                    actual,
                    expected[usize::from(value - 1)],
                    "orientation {value}"
                );
            }
            if value >= 5 {
                assert!(open_regular_image_bounded(&path, 2, 3, 4096).is_err());
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn jpeg_exif_orientation_is_applied() {
        let dir = temp_dir();
        let path = dir.join("photo.jpg");
        image::RgbImage::from_fn(2, 3, |x, y| image::Rgb([(x * 60) as u8, (y * 60) as u8, 0]))
            .save(&path)
            .unwrap();
        let expected = open_regular_image(&path).unwrap().rotate90().to_rgb8();
        let original = std::fs::read(&path).unwrap();
        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend(exif_orientation(6));
        let mut encoded = vec![0xff, 0xd8, 0xff, 0xe1];
        encoded.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
        encoded.extend(app1);
        encoded.extend_from_slice(&original[2..]);
        std::fs::write(&path, encoded).unwrap();
        assert_eq!(open_regular_image(&path).unwrap().to_rgb8(), expected);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bounded_read_rejects_growth_and_stops_after_one_extra_byte() {
        let mut source = std::io::Cursor::new([0; 16]);
        assert!(read_bounded(&mut source, 4).is_err());
        assert_eq!(source.position(), 5);
        assert_eq!(read_bounded(&b"1234"[..], 4).unwrap(), b"1234");
    }

    #[test]
    fn ordinary_limits_reject_large_sources_dimensions_and_decoded_output() {
        let dir = temp_dir();
        let path = dir.join("large.png");
        // Sparse oversized input exercises rejection without allocating 64 MiB.
        std::fs::File::create(&path)
            .unwrap()
            .set_len(WALLPAPER_MAX_BYTES + 1)
            .unwrap();
        assert!(open_regular_image(&path).is_err());
        image::RgbImage::new(WALLPAPER_MAX_DIMENSION + 1, 1)
            .save(&path)
            .unwrap();
        assert!(open_regular_image(&path).is_err());
        image::RgbImage::new(4, 4).save(&path).unwrap();
        // RGB decoding needs 48 bytes but the later RGBA output needs 64.
        assert!(open_image_with_limits(&path, 4, 4, 4096, 63, 4096).is_err());
        assert!(open_image_with_limits(&path, 4, 4, 4096, 64, 47).is_err());
        assert!(open_image_with_limits(&path, 4, 4, 4096, 64, 4096).is_ok());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bounded_sky_rejects_oversize_dimensions_bytes_and_corrupt_data() {
        let dir = temp_dir();
        let path = dir.join("sky.png");
        image::RgbaImage::from_pixel(4, 2, image::Rgba([1, 2, 3, 255]))
            .save(&path)
            .unwrap();
        assert!(open_regular_image_bounded(&path, 4, 2, 4096).is_ok());
        assert!(open_regular_image_bounded(&path, 2, 2, 4096).is_err());
        assert!(open_regular_image_bounded(&path, 4, 1, 4096).is_err());
        assert!(open_regular_image_bounded(&path, 4, 2, 1).is_err());
        std::fs::write(&path, b"not an image").unwrap();
        assert!(open_regular_image_bounded(&path, 4, 2, 4096).is_err());
        assert!(open_regular_image_bounded(&dir, 4, 2, 4096).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn ordinary_image_and_symlink_keep_extension_based_decode() {
        let dir = temp_dir();
        let path = dir.join("image.png");
        let link = dir.join("alias.png");
        image::RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 255]))
            .save(&path)
            .unwrap();
        std::os::unix::fs::symlink(&path, &link).unwrap();
        for path in [&path, &link] {
            let image = open_regular_image(path).unwrap();
            assert_eq!((image.width(), image.height()), (1, 1));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn fifo_is_rejected_without_a_writer_and_directory_is_rejected() {
        use std::os::unix::ffi::OsStrExt as _;
        let dir = temp_dir();
        let fifo = dir.join("fifo.png");
        let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        // Isolate the operation so reverting the nonblocking open produces a
        // bounded failure instead of hanging the whole test process.
        let worker_path = fifo.clone();
        let (sent, received) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let bounded_rejected = matches!(
                open_regular_image_bounded(&worker_path, 4096, 2048, 16 * 1024 * 1024),
                Err(image::ImageError::IoError(error))
                    if error.kind() == io::ErrorKind::InvalidInput
            );
            let rejected = matches!(
                open_regular_image(worker_path),
                Err(image::ImageError::IoError(error))
                    if error.kind() == io::ErrorKind::InvalidInput
            );
            let _ = sent.send(rejected && bounded_rejected);
        });
        let timely = received.recv_timeout(std::time::Duration::from_millis(250));
        if matches!(timely, Err(std::sync::mpsc::RecvTimeoutError::Timeout)) {
            // A regressed blocking File::open is waiting for this writer.
            // O_NONBLOCK keeps cleanup safe if the worker completed right at
            // the deadline or has not reached open yet. Retry only while it
            // remains live, so an already-finished worker needs no partner.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while !worker.is_finished() && std::time::Instant::now() < deadline {
                if let Ok(writer) = OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&fifo)
                {
                    drop(writer); // EOF lets the old decoder unwind too.
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            // The bounded wait covers decoder teardown after the EOF; do not
            // turn an unrelated stuck worker into an unbounded join either.
            received
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("FIFO reader did not exit after cleanup released it");
        }
        worker.join().expect("FIFO reader worker panicked");
        // Remove the FIFO before asserting, including on the old-code path.
        std::fs::remove_file(&fifo).unwrap();
        if !matches!(timely, Ok(true)) {
            std::fs::remove_dir(&dir).unwrap();
            panic!("FIFO image source was not rejected promptly: {timely:?}");
        }
        assert!(
            matches!(open_regular_image(&dir), Err(image::ImageError::IoError(error)) if error.kind()==io::ErrorKind::InvalidInput)
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
