//! Regular-file-only image decoding for compositor background workers.

use std::fs::OpenOptions;
use std::io::{self, BufReader};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

/// Keep image::open's extension-based format selection, but never wait for a
/// FIFO writer. Checking the opened descriptor also closes a metadata/open
/// replacement race; symlinks to ordinary image files remain supported.
pub(crate) fn open_regular_image(
    path: impl AsRef<Path>,
) -> image::ImageResult<image::DynamicImage> {
    let path = path.as_ref();
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "image source is not a regular file",
        )
        .into());
    }
    let format = image::ImageFormat::from_path(path)?;
    image::ImageReader::with_format(BufReader::new(file), format).decode()
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
            let rejected = matches!(
                open_regular_image(worker_path),
                Err(image::ImageError::IoError(error))
                    if error.kind() == io::ErrorKind::InvalidInput
            );
            let _ = sent.send(rejected);
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
