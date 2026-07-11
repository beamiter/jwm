//! Backend-neutral screenshot file output helpers.

use std::collections::VecDeque;

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
pub fn save_png_async(path: std::path::PathBuf, pixels: Vec<u8>, width: u32, height: u32) {
    std::thread::spawn(move || {
        let tmp_path = path.with_extension(format!(
            "{}.tmp",
            path.extension().and_then(|ext| ext.to_str()).unwrap_or("png")
        ));
        let result = image::save_buffer_with_format(
            &tmp_path,
            &pixels,
            width,
            height,
            image::ColorType::Rgba8,
            image::ImageFormat::Png,
        )
        .and_then(|_| std::fs::rename(&tmp_path, &path).map_err(image::ImageError::IoError));
        if let Err(e) = result {
            let _ = std::fs::remove_file(&tmp_path);
            log::warn!("compositor: screenshot save failed: {e}");
        } else {
            log::info!("compositor: screenshot saved to {}", path.display());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_request_order() {
        let mut queue = ScreenshotQueue::default();
        queue.request_full("first.png".into());
        queue.request_region("second.png".into(), 1, 2, 3, 4);

        let mut requests = queue.take_all();
        assert!(!queue.has_pending());
        assert!(matches!(requests.pop_front(), Some(ScreenshotRequest::Full(path)) if path == std::path::Path::new("first.png")));
        assert!(matches!(requests.pop_front(), Some(ScreenshotRequest::Region { path, x: 1, y: 2, width: 3, height: 4 }) if path == std::path::Path::new("second.png")));
    }
}
