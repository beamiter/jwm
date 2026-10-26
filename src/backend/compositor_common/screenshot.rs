//! Backend-neutral screenshot file and bounded in-memory output helpers.

use crate::backend::error::BackendErrorContext;
use std::collections::VecDeque;

/// Encoded clipboard images have their own bound in addition to the RGBA
/// readback budget. A noisy capture may legitimately exceed this limit.
const MAX_SCREENSHOT_PNG_BYTES: usize = 64 * 1024 * 1024;
const SCREENSHOT_CANCEL_POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// Cancellation is shared by the capture, result waiter and main-thread owner.
/// The gate serializes cancellation with the final clipboard offer, so an old
/// request cannot replace the clipboard after cancellation has returned.
#[derive(Clone, Debug, Default)]
pub struct ScreenshotCancellation {
    gate: std::sync::Arc<std::sync::Mutex<bool>>,
}

impl ScreenshotCancellation {
    pub fn cancel(&self) {
        *self.gate.lock().unwrap_or_else(|error| error.into_inner()) = true;
    }

    pub fn is_cancelled(&self) -> bool {
        self.gate.lock().map(|cancelled| *cancelled).unwrap_or(true)
    }

    /// Run one short final action only while this request is still active.
    /// Do not wait, encode, or call cancellation methods from inside `work`.
    /// A panicking action poisons the gate and future actions fail closed.
    pub fn with_active<T>(&self, work: impl FnOnce() -> T) -> Option<T> {
        let cancelled = self.gate.lock().ok()?;
        if *cancelled {
            return None;
        }
        let result = work();
        drop(cancelled);
        Some(result)
    }
}

/// Cloneable so a synchronous backend fallback can reuse the same result sink.
/// Dropping the last sender disconnects a waiter, including capture/spawn errors.
#[derive(Clone, Debug)]
pub struct ScreenshotMemorySender {
    sender: std::sync::mpsc::SyncSender<Result<Vec<u8>, String>>,
    cancellation: ScreenshotCancellation,
    completed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl ScreenshotMemorySender {
    fn try_send(self, result: Result<Vec<u8>, String>) -> std::io::Result<()> {
        self.cancellation
            .with_active(|| {
                // Capacity one alone is insufficient: a fallback clone could
                // otherwise enqueue again once the receiver takes the result.
                // Claim the terminal state under the cancellation gate before
                // trying delivery, including delivery failures.
                if self
                    .completed
                    .swap(true, std::sync::atomic::Ordering::Relaxed)
                {
                    return Err(screenshot_already_completed_error());
                }
                self.sender.try_send(result).map_err(|error| match error {
                    std::sync::mpsc::TrySendError::Full(_) => std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "screenshot result slot is already occupied",
                    ),
                    std::sync::mpsc::TrySendError::Disconnected(_) => std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "screenshot result receiver disconnected",
                    ),
                })
            })
            .ok_or_else(screenshot_cancelled_error)?
    }
}

#[derive(Debug)]
pub struct ScreenshotMemoryReceiver {
    receiver: std::sync::mpsc::Receiver<Result<Vec<u8>, String>>,
    cancellation: ScreenshotCancellation,
    delivered: std::cell::Cell<bool>,
}

impl ScreenshotMemoryReceiver {
    /// Wait in short slices so cancellation also interrupts a stalled capture.
    /// A successful result transfers final-offer cancellation to the main owner.
    pub fn recv_timeout(&self, timeout: std::time::Duration) -> Result<Vec<u8>, String> {
        if self.delivered.get() {
            return Err("screenshot result was already received".to_owned());
        }
        let started = std::time::Instant::now();
        loop {
            if self.cancellation.is_cancelled() {
                return self.fail("screenshot cancelled");
            }
            match self.receiver.try_recv() {
                Ok(result) => return self.receive(result),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return self.fail("screenshot capture disconnected before producing a PNG");
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return self.fail("screenshot capture timed out");
            }
            match self
                .receiver
                .recv_timeout(remaining.min(SCREENSHOT_CANCEL_POLL))
            {
                Ok(result) => return self.receive(result),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return self.fail("screenshot capture disconnected before producing a PNG");
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    }

    fn receive(&self, result: Result<Vec<u8>, String>) -> Result<Vec<u8>, String> {
        match self.cancellation.with_active(|| result) {
            Some(Ok(bytes)) => {
                self.delivered.set(true);
                Ok(bytes)
            }
            Some(Err(error)) => self.fail(&error),
            None => self.fail("screenshot cancelled"),
        }
    }

    fn fail(&self, message: &str) -> Result<Vec<u8>, String> {
        self.cancellation.cancel();
        // Discard an already queued payload too; a timed-out receiver must not
        // retain private image bytes until its eventual owner cleanup.
        let _ = self.receiver.try_recv();
        Err(message.to_owned())
    }
}

impl Drop for ScreenshotMemoryReceiver {
    fn drop(&mut self) {
        if !self.delivered.get() {
            self.cancellation.cancel();
        }
    }
}

/// A single encoded result slot; producers never block waiting for the owner.
pub fn screenshot_memory_channel() -> (
    ScreenshotMemorySender,
    ScreenshotMemoryReceiver,
    ScreenshotCancellation,
) {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let cancellation = ScreenshotCancellation {
        gate: std::sync::Arc::new(std::sync::Mutex::new(false)),
    };
    (
        ScreenshotMemorySender {
            sender,
            cancellation: cancellation.clone(),
            completed: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        },
        ScreenshotMemoryReceiver {
            receiver,
            cancellation: cancellation.clone(),
            delivered: std::cell::Cell::new(false),
        },
        cancellation,
    )
}

/// File captures keep atomic no-clobber publication. Clipboard captures never
/// acquire a filesystem path and deliver the encoded PNG directly to memory.
#[derive(Clone, Debug)]
pub enum ScreenshotDestination {
    File(std::path::PathBuf),
    Memory(ScreenshotMemorySender),
}

impl ScreenshotDestination {
    pub fn is_cancelled(&self) -> bool {
        match self {
            Self::File(_) => false,
            Self::Memory(sender) => sender.cancellation.is_cancelled(),
        }
    }

    pub fn display(&self) -> &Self {
        self
    }
}

impl std::fmt::Display for ScreenshotDestination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(path) => write!(f, "{}", path.display()),
            Self::Memory(_) => f.write_str("clipboard memory"),
        }
    }
}

impl From<std::path::PathBuf> for ScreenshotDestination {
    fn from(path: std::path::PathBuf) -> Self {
        Self::File(path)
    }
}

impl From<&str> for ScreenshotDestination {
    fn from(path: &str) -> Self {
        Self::File(path.into())
    }
}

fn screenshot_already_completed_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "screenshot result was already produced",
    )
}

fn screenshot_cancelled_error() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Interrupted, "screenshot cancelled")
}

struct BoundedPngWriter {
    bytes: Vec<u8>,
    limit: usize,
    failure: Option<std::io::Error>,
}

impl BoundedPngWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            failure: None,
        }
    }

    fn into_bytes(self) -> std::io::Result<Vec<u8>> {
        match self.failure {
            Some(error) => Err(error),
            None => Ok(self.bytes),
        }
    }

    fn write_bounded(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let new_len = self.bytes.len().checked_add(buffer.len()).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "PNG output size overflow")
        })?;
        if new_len > self.limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "screenshot PNG exceeds the 64 MiB in-memory output budget",
            ));
        }
        self.bytes
            .try_reserve_exact(buffer.len())
            .map_err(std::io::Error::other)?;
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }
}

impl std::io::Write for BoundedPngWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let result = self.write_bounded(buffer);
        if let Err(error) = &result
            && self.failure.is_none()
        {
            // png::Writer finishes IEND in Drop and can ignore that write's
            // error. Preserve it so a truncated PNG is never a successful result.
            self.failure = Some(std::io::Error::new(error.kind(), error.to_string()));
        }
        result
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Validate the shared 512 MiB RGBA bound before encoding, and bound every
/// output allocation rather than checking only after a PNG has been built.
pub(crate) fn encode_png_bounded(
    pixels: &[u8],
    width: u32,
    height: u32,
) -> Result<Vec<u8>, image::ImageError> {
    use image::ImageEncoder as _;

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
    let mut writer = BoundedPngWriter::new(MAX_SCREENSHOT_PNG_BYTES);
    image::codecs::png::PngEncoder::new(&mut writer).write_image(
        pixels,
        width,
        height,
        image::ColorType::Rgba8.into(),
    )?;
    writer.into_bytes().map_err(image::ImageError::IoError)
}

/// Small borrowed-buffer adapter for deterministic synchronous tests. Production
/// writers use the owned adapter to release raw pixels before result delivery.
#[cfg(test)]
pub(crate) fn save_screenshot(
    destination: ScreenshotDestination,
    pixels: &[u8],
    width: u32,
    height: u32,
) -> Result<(), image::ImageError> {
    save_screenshot_buffer(destination, pixels, width, height)
}

/// Release the original RGBA allocation before publishing an in-memory PNG, so
/// a clipboard annotation/decode worker cannot overlap its next RGBA allocation
/// with this readback buffer. File output keeps the existing atomic helper.
pub(crate) fn save_screenshot_owned(
    destination: ScreenshotDestination,
    pixels: Vec<u8>,
    width: u32,
    height: u32,
) -> Result<(), image::ImageError> {
    save_screenshot_buffer(destination, pixels, width, height)
}

fn save_screenshot_buffer(
    destination: ScreenshotDestination,
    pixels: impl AsRef<[u8]>,
    width: u32,
    height: u32,
) -> Result<(), image::ImageError> {
    match destination {
        ScreenshotDestination::File(path) => {
            save_png_atomically(&path, pixels.as_ref(), width, height)
        }
        ScreenshotDestination::Memory(sender) => {
            if sender.cancellation.is_cancelled() {
                return Err(image::ImageError::IoError(screenshot_cancelled_error()));
            }
            if sender.completed.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(image::ImageError::IoError(
                    screenshot_already_completed_error(),
                ));
            }
            let result = encode_png_bounded(pixels.as_ref(), width, height);
            drop(pixels);
            match result {
                Ok(bytes) => sender
                    .try_send(Ok(bytes))
                    .map_err(image::ImageError::IoError),
                Err(error) => {
                    // Report encoder failures promptly even if a backend still
                    // holds a fallback clone. The original error remains useful
                    // to the writer's backend-context logger.
                    let _ = sender.try_send(Err(error.to_string()));
                    Err(error)
                }
            }
        }
    }
}

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
        path: ScreenshotDestination,
        permit: ScreenshotPermit,
    },
    Region {
        path: ScreenshotDestination,
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
    pub fn request_full(&mut self, path: ScreenshotDestination) -> Result<(), ScreenshotBusy> {
        let permit = self.admission.try_acquire()?;
        self.requests
            .push_back(ScreenshotRequest::Full { path, permit });
        Ok(())
    }

    pub fn request_region(
        &mut self,
        path: ScreenshotDestination,
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

/// Encode RGBA pixels off the render thread into a file or memory result sink.
/// Consumers only ever observe a complete PNG.
///
/// `context` tags the asynchronous failure with the requesting backend and
/// operation, since by the time encoding fails the capture call has returned.
pub fn save_png_async(
    path: ScreenshotDestination,
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    context: BackendErrorContext,
    permit: ScreenshotPermit,
) {
    let spawn_context = context.clone();
    if let Err(error) = spawn_screenshot_worker("jwm-screenshot-png", permit, move || {
        let destination_label = path.to_string();
        let result = save_screenshot_owned(path, pixels, width, height);
        if let Err(e) = result {
            log::warn!("{context}: {e}");
        } else {
            log::info!("compositor: screenshot saved to {destination_label}");
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
    fn owned_memory_capture_releases_raw_pixels_before_result_delivery() {
        struct DropProbe<'a> {
            pixels: Vec<u8>,
            receiver: &'a ScreenshotMemoryReceiver,
            released: &'a std::cell::Cell<bool>,
        }

        impl AsRef<[u8]> for DropProbe<'_> {
            fn as_ref(&self) -> &[u8] {
                &self.pixels
            }
        }

        impl Drop for DropProbe<'_> {
            fn drop(&mut self) {
                assert!(matches!(
                    self.receiver.receiver.try_recv(),
                    Err(std::sync::mpsc::TryRecvError::Empty)
                ));
                self.released.set(true);
            }
        }

        for pixels in [vec![1, 2, 3, 255], vec![1, 2, 3]] {
            let valid = pixels.len() == 4;
            let (sender, receiver, _) = screenshot_memory_channel();
            let released = std::cell::Cell::new(false);
            let result = save_screenshot_buffer(
                ScreenshotDestination::Memory(sender),
                DropProbe {
                    pixels,
                    receiver: &receiver,
                    released: &released,
                },
                1,
                1,
            );
            assert!(released.get());
            assert_eq!(result.is_ok(), valid);
            assert_eq!(
                receiver.recv_timeout(std::time::Duration::ZERO).is_ok(),
                valid
            );
        }
    }

    #[test]
    fn memory_capture_delivers_png_without_creating_any_file() {
        let scratch = scratch_dir("memory-only");
        let (sender, receiver, cancellation) = screenshot_memory_channel();
        let pixels = [255, 0, 127, 255];
        save_screenshot(ScreenshotDestination::Memory(sender), &pixels, 1, 1).unwrap();
        let png = receiver.recv_timeout(std::time::Duration::ZERO).unwrap();
        assert_eq!(
            image::load_from_memory_with_format(&png, image::ImageFormat::Png)
                .unwrap()
                .to_rgba8()
                .as_raw(),
            &pixels
        );
        assert_eq!(std::fs::read_dir(&scratch).unwrap().count(), 0);
        assert!(!cancellation.is_cancelled());
        // The result can outlive its waiter while the main owner still controls
        // whether the complete PNG may become the clipboard offer.
        drop(receiver);
        assert_eq!(cancellation.with_active(|| "offer"), Some("offer"));
        cancellation.cancel();
        assert_eq!(cancellation.with_active(|| "late offer"), None);
        std::fs::remove_dir(scratch).unwrap();
    }

    #[test]
    fn memory_capture_sender_clones_support_fallback_and_disconnect_on_last_drop() {
        let (sender, receiver, _) = screenshot_memory_channel();
        let fallback = sender.clone();
        drop(sender);
        fallback.try_send(Ok(vec![1, 2, 3])).unwrap();
        assert_eq!(
            receiver.recv_timeout(std::time::Duration::ZERO).unwrap(),
            [1, 2, 3]
        );

        let (sender, receiver, cancellation) = screenshot_memory_channel();
        drop(sender.clone());
        drop(sender);
        let error = receiver
            .recv_timeout(std::time::Duration::from_secs(60))
            .unwrap_err();
        assert!(error.contains("disconnected"));
        assert!(cancellation.is_cancelled());
    }

    #[test]
    fn memory_capture_result_slot_never_blocks_a_second_sender() {
        let (sender, receiver, _) = screenshot_memory_channel();
        sender.clone().try_send(Ok(vec![1])).unwrap();
        let error = sender.try_send(Ok(vec![2])).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            receiver.recv_timeout(std::time::Duration::ZERO).unwrap(),
            [1]
        );
    }

    #[test]
    fn memory_capture_rejects_fallback_result_after_first_result_was_received() {
        let (sender, receiver, cancellation) = screenshot_memory_channel();
        save_screenshot(
            ScreenshotDestination::Memory(sender.clone()),
            &[1, 2, 3, 255],
            1,
            1,
        )
        .unwrap();
        assert!(
            !receiver
                .recv_timeout(std::time::Duration::ZERO)
                .unwrap()
                .is_empty()
        );
        assert!(!cancellation.is_cancelled());
        let error = save_screenshot(ScreenshotDestination::Memory(sender), &[4, 5, 6, 255], 1, 1)
            .unwrap_err();
        assert!(error.to_string().contains("already produced"));
        assert!(matches!(
            receiver.receiver.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn memory_capture_encoding_error_wakes_waiter_with_fallback_still_alive() {
        let (sender, receiver, cancellation) = screenshot_memory_channel();
        let fallback = sender.clone();
        assert!(save_screenshot(ScreenshotDestination::Memory(sender), &[1, 2, 3], 1, 1).is_err());
        let error = receiver
            .recv_timeout(std::time::Duration::ZERO)
            .unwrap_err();
        assert!(error.contains("RGBA buffer length"));
        assert!(cancellation.is_cancelled());
        assert_eq!(
            fallback.try_send(Ok(vec![])).unwrap_err().kind(),
            std::io::ErrorKind::Interrupted
        );
    }

    #[test]
    fn memory_capture_cancellation_discards_queued_result_and_rejects_late_writer() {
        let (sender, receiver, cancellation) = screenshot_memory_channel();
        sender.clone().try_send(Ok(vec![1, 2, 3])).unwrap();
        cancellation.cancel();
        assert_eq!(
            receiver
                .recv_timeout(std::time::Duration::ZERO)
                .unwrap_err(),
            "screenshot cancelled"
        );
        assert!(matches!(
            receiver.receiver.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        let destination = ScreenshotDestination::Memory(sender);
        assert!(destination.is_cancelled());
        assert!(save_screenshot(destination, &[1, 2, 3, 255], 1, 1).is_err());
        assert_eq!(
            cancellation.with_active(|| panic!("cancelled request offered clipboard")),
            None::<()>
        );
    }

    #[test]
    fn memory_capture_cancellation_interrupts_wait_without_waiting_for_sender() {
        let (sender, receiver, cancellation) = screenshot_memory_channel();
        let (ready_sender, ready_receiver) = std::sync::mpsc::channel();
        let (done_sender, done_receiver) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            ready_sender.send(()).unwrap();
            done_sender
                .send(receiver.recv_timeout(std::time::Duration::from_secs(60)))
                .unwrap();
        });
        ready_receiver.recv().unwrap();
        cancellation.cancel();
        let result = done_receiver.recv_timeout(std::time::Duration::from_secs(2));
        // Release the live producer even on an assertion failure, so a broken
        // cancellation implementation cannot strand the test waiter.
        drop(sender);
        waiter.join().unwrap();
        assert_eq!(result.unwrap().unwrap_err(), "screenshot cancelled");
    }

    #[test]
    fn memory_capture_timeout_and_receiver_drop_cancel_late_writes() {
        let (sender, receiver, cancellation) = screenshot_memory_channel();
        assert!(
            receiver
                .recv_timeout(std::time::Duration::ZERO)
                .unwrap_err()
                .contains("timed out")
        );
        assert!(cancellation.is_cancelled());
        assert_eq!(
            sender.try_send(Ok(vec![1])).unwrap_err().kind(),
            std::io::ErrorKind::Interrupted
        );

        let (sender, receiver, cancellation) = screenshot_memory_channel();
        drop(receiver);
        assert!(cancellation.is_cancelled());
        assert_eq!(
            sender.try_send(Ok(vec![1])).unwrap_err().kind(),
            std::io::ErrorKind::Interrupted
        );
    }

    #[test]
    fn memory_capture_panicking_offer_closes_cancellation_gate() {
        let (sender, receiver, cancellation) = screenshot_memory_channel();
        assert!(
            std::panic::catch_unwind(|| cancellation.with_active(|| panic!("offer failed")))
                .is_err()
        );
        assert!(cancellation.is_cancelled());
        assert_eq!(cancellation.with_active(|| "second offer"), None);
        cancellation.cancel();
        drop((sender, receiver));
    }

    #[test]
    fn bounded_png_writer_rejects_oversize_before_allocating_or_extending() {
        use std::io::Write as _;
        let mut writer = BoundedPngWriter::new(8);
        assert!(writer.write_all(&[0; 9]).is_err());
        assert_eq!(writer.bytes.capacity(), 0);
        writer.write_all(&[1; 4]).unwrap();
        assert!(writer.write_all(&[2; 5]).is_err());
        assert_eq!(writer.bytes, [1; 4]);
        writer.write_all(&[3; 4]).unwrap();
        assert_eq!(writer.bytes.len(), 8);
        assert!(writer.write_all(&[4]).is_err());
        assert_eq!(writer.bytes.len(), 8);
    }

    #[test]
    fn bounded_png_writer_preserves_errors_from_encoder_finalization() {
        use image::ImageEncoder as _;
        let pixels = [0, 0, 0, 255];
        let complete = encode_png_bounded(&pixels, 1, 1).unwrap();
        let mut writer = BoundedPngWriter::new(complete.len() - 1);
        let _ = image::codecs::png::PngEncoder::new(&mut writer).write_image(
            &pixels,
            1,
            1,
            image::ColorType::Rgba8.into(),
        );
        assert!(writer.bytes.len() < complete.len());
        assert!(
            writer
                .into_bytes()
                .unwrap_err()
                .to_string()
                .contains("64 MiB")
        );
    }

    #[test]
    fn bounded_png_encoder_propagates_output_limit_and_rejects_invalid_rgba() {
        use image::ImageEncoder as _;
        let mut writer = BoundedPngWriter::new(8);
        assert!(
            image::codecs::png::PngEncoder::new(&mut writer)
                .write_image(&[0, 0, 0, 255], 1, 1, image::ColorType::Rgba8.into())
                .is_err()
        );
        assert!(writer.bytes.len() <= 8);
        assert!(encode_png_bounded(&[1, 2, 3], 1, 1).is_err());
        for (width, height) in [(0, 1), (1, 0), (16385, 8192), (u32::MAX, u32::MAX)] {
            assert!(encode_png_bounded(&[], width, height).is_err());
        }
    }

    #[test]
    fn file_destination_keeps_atomic_no_clobber_publication() {
        let scratch = scratch_dir("file-destination");
        let path = scratch.join("shot.png");
        let first = [1, 2, 3, 255];
        save_screenshot(path.clone().into(), &first, 1, 1).unwrap();
        assert!(save_screenshot(path.clone().into(), &[4, 5, 6, 255], 1, 1).is_err());
        assert_eq!(image::open(&path).unwrap().to_rgba8().as_raw(), &first);
        assert!(!screenshot_staging_path(&path).exists());
        std::fs::remove_dir_all(scratch).unwrap();
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
            matches!(requests.pop_front(), Some(ScreenshotRequest::Full { path: ScreenshotDestination::File(path), .. }) if path == std::path::Path::new("first.png"))
        );
        assert!(
            matches!(requests.pop_front(), Some(ScreenshotRequest::Region { path: ScreenshotDestination::File(path), x: 1, y: 2, width: 3, height: 4, .. }) if path == std::path::Path::new("second.png"))
        );
    }

    #[test]
    fn preserves_multiple_fullscreen_requests_without_overwrite() {
        let mut queue = ScreenshotQueue::isolated_for_test();
        queue.request_full("first.png".into()).unwrap();
        queue.request_full("second.png".into()).unwrap();

        let mut requests = queue.take_all();
        assert!(
            matches!(requests.pop_front(), Some(ScreenshotRequest::Full { path: ScreenshotDestination::File(path), .. }) if path == std::path::Path::new("first.png"))
        );
        assert!(
            matches!(requests.pop_front(), Some(ScreenshotRequest::Full { path: ScreenshotDestination::File(path), .. }) if path == std::path::Path::new("second.png"))
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
