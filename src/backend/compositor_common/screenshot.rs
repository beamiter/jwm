//! Backend-neutral screenshot file output helpers.

use crate::backend::error::BackendErrorContext;
use std::collections::VecDeque;

/// Sidecar path used while encoding a screenshot before its atomic publish.
///
/// The command layer uses the same derivation for its synchronous writeability
/// preflight, so a request is not accepted when this file cannot even be
/// created in the destination directory.
pub(crate) fn screenshot_staging_path(path: &std::path::Path) -> std::path::PathBuf {
    path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("png")
    ))
}

fn remove_owned_staging_file(path: &std::path::Path) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!(
            "compositor: could not remove screenshot staging file '{}': {error}",
            path.display()
        );
    }
}

/// Encode and atomically publish one RGBA PNG without following a pre-existing
/// staging symlink or replacing an existing destination.
pub(crate) fn save_png_atomically(
    path: &std::path::Path,
    pixels: &[u8],
    width: u32,
    height: u32,
) -> Result<(), image::ImageError> {
    let expected_len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixel_count| pixel_count.checked_mul(4))
        .ok_or_else(|| {
            image::ImageError::IoError(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "screenshot dimensions overflow the RGBA buffer length",
            ))
        })?;
    if pixels.len() != expected_len {
        return Err(image::ImageError::IoError(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "invalid screenshot RGBA buffer length: expected {expected_len}, got {}",
                pixels.len()
            ),
        )));
    }

    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(image::ImageError::IoError)?;
    }

    let staging_path = screenshot_staging_path(path);
    let mut staging_file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staging_path)
        .map_err(image::ImageError::IoError)?;
    let write_result = image::write_buffer_with_format(
        &mut staging_file,
        pixels,
        width,
        height,
        image::ColorType::Rgba8,
        image::ImageFormat::Png,
    )
    .and_then(|_| staging_file.sync_all().map_err(image::ImageError::IoError));
    drop(staging_file);
    if let Err(error) = write_result {
        remove_owned_staging_file(&staging_path);
        return Err(error);
    }

    // Both names live in the same directory, so linking atomically publishes
    // the completed inode and fails with AlreadyExists instead of clobbering a
    // file or following a symlink that appeared after command preflight.
    if let Err(error) = std::fs::hard_link(&staging_path, path) {
        remove_owned_staging_file(&staging_path);
        return Err(image::ImageError::IoError(error));
    }
    remove_owned_staging_file(&staging_path);
    Ok(())
}

/// A screenshot request expressed in compositor coordinates (top-left origin).
pub enum ScreenshotRequest {
    Full(std::path::PathBuf),
    Region {
        path: std::path::PathBuf,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    },
}

/// Ordered, allocation-stable request queue shared by all compositors.
#[derive(Default)]
pub struct ScreenshotQueue {
    requests: VecDeque<ScreenshotRequest>,
}

impl ScreenshotQueue {
    pub fn request_full(&mut self, path: std::path::PathBuf) {
        self.requests.push_back(ScreenshotRequest::Full(path));
    }

    pub fn request_region(
        &mut self,
        path: std::path::PathBuf,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) {
        self.requests.push_back(ScreenshotRequest::Region {
            path,
            x,
            y,
            width,
            height,
        });
    }

    pub fn has_pending(&self) -> bool {
        !self.requests.is_empty()
    }

    /// Transfer all current requests without cloning their paths or pixels.
    pub fn take_all(&mut self) -> VecDeque<ScreenshotRequest> {
        std::mem::take(&mut self.requests)
    }
}

/// Encode RGBA pixels off the render thread and atomically publish the PNG.
/// Consumers therefore only ever observe a complete image at `path`.
///
/// `context` tags the asynchronous failure with the requesting backend and
/// operation, since by the time encoding fails the capture call has returned.
pub fn save_png_async(
    path: std::path::PathBuf,
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    context: BackendErrorContext,
) {
    std::thread::spawn(move || {
        let result = save_png_atomically(&path, &pixels, width, height);
        if let Err(e) = result {
            log::warn!("{context}: {e}");
        } else {
            log::info!("compositor: screenshot saved to {}", path.display());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(label: &str) -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "jwm-common-screenshot-{label}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn preserves_request_order() {
        let mut queue = ScreenshotQueue::default();
        queue.request_full("first.png".into());
        queue.request_region("second.png".into(), 1, 2, 3, 4);

        let mut requests = queue.take_all();
        assert!(!queue.has_pending());
        assert!(
            matches!(requests.pop_front(), Some(ScreenshotRequest::Full(path)) if path == std::path::Path::new("first.png"))
        );
        assert!(
            matches!(requests.pop_front(), Some(ScreenshotRequest::Region { path, x: 1, y: 2, width: 3, height: 4 }) if path == std::path::Path::new("second.png"))
        );
    }

    #[test]
    fn preserves_multiple_fullscreen_requests_without_overwrite() {
        let mut queue = ScreenshotQueue::default();
        queue.request_full("first.png".into());
        queue.request_full("second.png".into());

        let mut requests = queue.take_all();
        assert!(
            matches!(requests.pop_front(), Some(ScreenshotRequest::Full(path)) if path == std::path::Path::new("first.png"))
        );
        assert!(
            matches!(requests.pop_front(), Some(ScreenshotRequest::Full(path)) if path == std::path::Path::new("second.png"))
        );
        assert!(requests.is_empty());
    }

    #[test]
    fn atomic_png_publish_never_exposes_staging_or_replaces_destination() {
        let scratch = scratch_dir("atomic");
        let path = scratch.join("shot.png");
        let first = [255, 0, 0, 255];
        save_png_atomically(&path, &first, 1, 1).unwrap();

        assert!(path.is_file());
        assert!(!screenshot_staging_path(&path).exists());
        assert_eq!(image::open(&path).unwrap().to_rgba8().as_raw(), &first);

        let second = [0, 0, 255, 255];
        assert!(save_png_atomically(&path, &second, 1, 1).is_err());
        assert_eq!(image::open(&path).unwrap().to_rgba8().as_raw(), &first);
        assert!(!screenshot_staging_path(&path).exists());
        let _ = std::fs::remove_dir_all(scratch);
    }

    #[test]
    fn invalid_pixel_buffer_is_rejected_without_a_partial_file() {
        let scratch = scratch_dir("invalid-buffer");
        let path = scratch.join("shot.png");

        assert!(save_png_atomically(&path, &[0, 1, 2], 1, 1).is_err());
        assert!(!path.exists());
        assert!(!screenshot_staging_path(&path).exists());
        let _ = std::fs::remove_dir_all(scratch);
    }
}
