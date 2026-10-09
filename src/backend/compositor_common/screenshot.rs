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
    let expected_len = crate::backend::compositor_common::capture::rgba_capture_len(width, height)
        .map_err(image::ImageError::IoError)?;
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
    let mut staging_options = std::fs::OpenOptions::new();
    staging_options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // Screenshots can contain credentials and private conversations. The
        // completed path is a hard link to this inode, so setting the mode at
        // creation protects both clipboard staging files and saved captures.
        staging_options.mode(0o600);
    }
    let mut staging_file = staging_options
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

/// Process-wide bound covering queued requests, readback buffers and PNG workers.
const MAX_SCREENSHOTS_IN_FLIGHT: usize = 4;

#[derive(Debug)]
struct ScreenshotAdmission {
    in_flight: std::sync::atomic::AtomicUsize,
}

impl ScreenshotAdmission {
    fn new() -> Self {
        Self {
            in_flight: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn try_acquire(self: &std::sync::Arc<Self>) -> Result<ScreenshotPermit, ScreenshotBusy> {
        use std::sync::atomic::Ordering::Relaxed;
        let mut count = self.in_flight.load(Relaxed);
        loop {
            if count >= MAX_SCREENSHOTS_IN_FLIGHT {
                return Err(ScreenshotBusy);
            }
            match self
                .in_flight
                .compare_exchange_weak(count, count + 1, Relaxed, Relaxed)
            {
                Ok(_) => break,
                Err(observed) => count = observed,
            }
        }
        Ok(ScreenshotPermit {
            admission: std::sync::Arc::clone(self),
        })
    }
}

/// An accepted screenshot keeps this non-cloneable token until its last worker
/// finishes. Draining a queue or replacing a compositor must not release it.
#[derive(Debug)]
pub struct ScreenshotPermit {
    admission: std::sync::Arc<ScreenshotAdmission>,
}

impl Drop for ScreenshotPermit {
    fn drop(&mut self) {
        let previous = self
            .admission
            .in_flight
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        debug_assert!(previous > 0);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenshotBusy;

impl std::fmt::Display for ScreenshotBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("screenshot busy: four captures are already in flight")
    }
}

impl std::error::Error for ScreenshotBusy {}

/// A screenshot request expressed in compositor coordinates (top-left origin).
pub enum ScreenshotRequest {
    Full {
        path: std::path::PathBuf,
        permit: ScreenshotPermit,
    },
    Region {
        path: std::path::PathBuf,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        permit: ScreenshotPermit,
    },
}

/// Ordered request queue sharing a process-wide admission budget.
pub struct ScreenshotQueue {
    requests: VecDeque<ScreenshotRequest>,
    admission: std::sync::Arc<ScreenshotAdmission>,
}

impl Default for ScreenshotQueue {
    fn default() -> Self {
        static ADMISSION: std::sync::OnceLock<std::sync::Arc<ScreenshotAdmission>> =
            std::sync::OnceLock::new();
        Self {
            requests: VecDeque::new(),
            admission: std::sync::Arc::clone(
                ADMISSION.get_or_init(|| std::sync::Arc::new(ScreenshotAdmission::new())),
            ),
        }
    }
}

impl ScreenshotQueue {
    pub fn request_full(&mut self, path: std::path::PathBuf) -> Result<(), ScreenshotBusy> {
        let permit = self.admission.try_acquire()?;
        self.requests
            .push_back(ScreenshotRequest::Full { path, permit });
        Ok(())
    }

    pub fn request_region(
        &mut self,
        path: std::path::PathBuf,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) -> Result<(), ScreenshotBusy> {
        let permit = self.admission.try_acquire()?;
        self.requests.push_back(ScreenshotRequest::Region {
            path,
            x,
            y,
            width,
            height,
            permit,
        });
        Ok(())
    }

    pub fn has_pending(&self) -> bool {
        !self.requests.is_empty()
    }

    pub fn clear(&mut self) {
        self.requests.clear();
    }

    /// Transfer requests and their permits without releasing the admission budget.
    pub fn take_all(&mut self) -> VecDeque<ScreenshotRequest> {
        std::mem::take(&mut self.requests)
    }

    #[cfg(test)]
    pub(crate) fn isolated_for_test() -> Self {
        Self {
            requests: VecDeque::new(),
            admission: std::sync::Arc::new(ScreenshotAdmission::new()),
        }
    }

    #[cfg(test)]
    pub(crate) fn in_flight_for_test(&self) -> usize {
        self.admission
            .in_flight
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

fn screenshot_worker(
    permit: ScreenshotPermit,
    work: impl FnOnce() + Send + 'static,
) -> impl FnOnce() + Send + 'static {
    move || {
        let _permit = permit;
        work();
    }
}

/// The worker owns admission through encoding and publication, including error
/// cleanup. A failed thread spawn drops the closure and returns the permit.
pub(crate) fn spawn_screenshot_worker(
    name: &str,
    permit: ScreenshotPermit,
    work: impl FnOnce() + Send + 'static,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(screenshot_worker(permit, work))
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
    permit: ScreenshotPermit,
) {
    let spawn_context = context.clone();
    if let Err(error) = spawn_screenshot_worker("jwm-screenshot-png", permit, move || {
        let result = save_png_atomically(&path, &pixels, width, height);
        if let Err(e) = result {
            log::warn!("{context}: {e}");
        } else {
            log::info!("compositor: screenshot saved to {}", path.display());
        }
    }) {
        log::warn!("{spawn_context}: could not spawn PNG writer: {error}");
    }
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
    fn screenshot_admission_is_shared_by_default_queues() {
        let first = ScreenshotQueue::default();
        let second = ScreenshotQueue::default();
        assert!(std::sync::Arc::ptr_eq(&first.admission, &second.admission));
    }
    #[test]
    fn screenshot_admission_stays_held_after_drain_and_rejects_the_fifth_request() {
        let mut queue = ScreenshotQueue::isolated_for_test();
        for _ in 0..MAX_SCREENSHOTS_IN_FLIGHT {
            queue.request_full("unused.png".into()).unwrap();
        }
        assert_eq!(
            queue.request_full("rejected.png".into()),
            Err(ScreenshotBusy)
        );
        let mut requests = queue.take_all();
        assert!(!queue.has_pending());
        assert_eq!(queue.in_flight_for_test(), 4);
        assert_eq!(
            queue.request_region("rejected.png".into(), 0, 0, 1, 1),
            Err(ScreenshotBusy)
        );
        drop(requests.pop_front());
        queue
            .request_region("replacement.png".into(), 0, 0, 1, 1)
            .unwrap();
        assert_eq!(queue.in_flight_for_test(), 4);
        queue.clear();
        assert_eq!(queue.in_flight_for_test(), 3);
        drop(requests);
        assert_eq!(queue.in_flight_for_test(), 0);
    }
    #[test]
    fn screenshot_admission_survives_queue_replacement_and_combines_backends() {
        let mut first = ScreenshotQueue::isolated_for_test();
        let admission = std::sync::Arc::clone(&first.admission);
        let mut second = ScreenshotQueue {
            requests: VecDeque::new(),
            admission: admission.clone(),
        };
        for _ in 0..2 {
            first.request_full("first.png".into()).unwrap();
            second.request_full("second.png".into()).unwrap();
        }
        let readbacks = first.take_all();
        drop(first);
        let mut replacement = ScreenshotQueue {
            requests: VecDeque::new(),
            admission,
        };
        assert_eq!(
            replacement.request_full("replacement.png".into()),
            Err(ScreenshotBusy)
        );
        drop(second);
        replacement.request_full("replacement.png".into()).unwrap();
        assert_eq!(replacement.in_flight_for_test(), 3);
        drop(readbacks);
        assert_eq!(replacement.in_flight_for_test(), 1);
        replacement.clear();
        assert_eq!(replacement.in_flight_for_test(), 0);
    }
    #[test]
    fn screenshot_admission_covers_synthetic_encoding_until_workers_finish() {
        use std::sync::mpsc;
        use std::time::Duration;
        let mut queue = ScreenshotQueue::isolated_for_test();
        let mut workers = Vec::new();
        let mut releases = Vec::new();
        for _ in 0..4 {
            queue.request_full("unused.png".into()).unwrap();
            let ScreenshotRequest::Full { permit, .. } = queue.take_all().pop_front().unwrap()
            else {
                unreachable!()
            };
            let (ready_tx, ready_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel::<()>();
            let pixels = Vec::from([0u8; 4]);
            let worker = spawn_screenshot_worker("screenshot-test-encoder", permit, move || {
                // A four-byte encoder stub: no files, display or real capture.
                assert_eq!(pixels.len(), 4);
                ready_tx.send(()).unwrap();
                let _ = release_rx.recv();
            })
            .unwrap();
            releases.push(release_tx);
            workers.push(worker);
            ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        let fifth = queue.request_full("rejected.png".into());
        let occupied = queue.in_flight_for_test();
        // Always release and join workers before checking the regression result.
        drop(releases);
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(fifth, Err(ScreenshotBusy));
        assert_eq!(occupied, 4);
        assert_eq!(queue.in_flight_for_test(), 0);
        queue.request_full("after.png".into()).unwrap();
    }
    #[test]
    fn screenshot_admission_is_returned_when_spawn_refuses_the_owned_closure() {
        let queue = ScreenshotQueue::isolated_for_test();
        let permit = queue.admission.try_acquire().unwrap();
        let worker = screenshot_worker(permit, || panic!("a refused worker must not run"));
        // Builder::spawn owns its closure on both success and failure. Simulate
        // refusal without changing process-wide thread or memory limits.
        fn refuse(worker: impl FnOnce()) -> std::io::Result<()> {
            drop(worker);
            Err(std::io::Error::other("synthetic thread creation failure"))
        }
        assert!(refuse(worker).is_err());
        assert_eq!(queue.in_flight_for_test(), 0);
    }
    #[test]
    fn screenshot_admission_is_returned_after_worker_error_or_unwind() {
        let queue = ScreenshotQueue::isolated_for_test();
        let permit = queue.admission.try_acquire().unwrap();
        screenshot_worker(permit, || {
            let _ = Err::<(), _>("synthetic encode failure");
        })();
        assert_eq!(queue.in_flight_for_test(), 0);
        let permit = queue.admission.try_acquire().unwrap();
        assert!(
            std::panic::catch_unwind(screenshot_worker(permit, || panic!(
                "synthetic encoder panic"
            )))
            .is_err()
        );
        assert_eq!(queue.in_flight_for_test(), 0);
    }

    #[test]
    fn preserves_request_order() {
        let mut queue = ScreenshotQueue::isolated_for_test();
        queue.request_full("first.png".into()).unwrap();
        queue
            .request_region("second.png".into(), 1, 2, 3, 4)
            .unwrap();

        let mut requests = queue.take_all();
        assert!(!queue.has_pending());
        assert!(
            matches!(requests.pop_front(), Some(ScreenshotRequest::Full { path, .. }) if path == std::path::Path::new("first.png"))
        );
        assert!(
            matches!(requests.pop_front(), Some(ScreenshotRequest::Region { path, x: 1, y: 2, width: 3, height: 4, .. }) if path == std::path::Path::new("second.png"))
        );
    }

    #[test]
    fn preserves_multiple_fullscreen_requests_without_overwrite() {
        let mut queue = ScreenshotQueue::isolated_for_test();
        queue.request_full("first.png".into()).unwrap();
        queue.request_full("second.png".into()).unwrap();

        let mut requests = queue.take_all();
        assert!(
            matches!(requests.pop_front(), Some(ScreenshotRequest::Full { path, .. }) if path == std::path::Path::new("first.png"))
        );
        assert!(
            matches!(requests.pop_front(), Some(ScreenshotRequest::Full { path, .. }) if path == std::path::Path::new("second.png"))
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
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
