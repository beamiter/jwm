//! Non-blocking frame handoff from the compositor to the recording encoder.
//!
//! Both backends used to `write_all` a whole captured frame into ffmpeg's stdin
//! from inside the render loop. A 1080p RGBA frame is 8.3 MB and a Linux pipe
//! holds 64 KiB, so that single call is ~127 writes that each block until
//! ffmpeg drains them. Whenever the encoder falls behind — software libx264, a
//! busy CPU, a 4K region — the compositor thread parks inside `write`, and with
//! it every repaint and every input event for every client on the session. That
//! is the "the whole desktop freezes while recording" report.
//!
//! The sink moves the write onto its own thread behind a shallow bounded queue
//! and **drops** the newest frame when that queue is full. Recording quality
//! degrades to a lower effective capture rate under load, which is what the
//! user wants; the compositor never waits on the encoder.

use std::io::Write;
use std::process::Child;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;

/// Frames allowed to queue ahead of the encoder. Input timestamps come from
/// `-use_wallclock_as_timestamps 1`, i.e. they are stamped when ffmpeg *reads*
/// the frame, so every queued frame adds that much lag between the pixels and
/// the timestamp they get. Two frames (~66 ms at 30 fps) absorbs an encoder
/// hiccup; anything deeper just buys drift, because a backlog past two frames
/// means the encoder cannot sustain the capture rate at all.
const QUEUE_DEPTH: usize = 2;

/// Frame buffers recycled between captures. Recording at 30 fps otherwise
/// allocates and frees 8 MB thirty times a second, which fragments the heap and
/// shows up as its own periodic stall.
const POOL_DEPTH: usize = QUEUE_DEPTH + 2;

/// Ask the kernel for a bigger pipe to the encoder.
///
/// A pipe holds 64 KiB by default, so a 1080p frame is 127 separate blocking
/// writes and a 4K frame is 507. Widening it to the usual 1 MiB ceiling cuts
/// that to 8 and 32, and gives the encoder that much more slack to absorb a
/// hiccup before the sink has to start dropping frames. Best effort: the limit
/// is `/proc/sys/fs/pipe-max-size` and an unprivileged process may not reach it.
fn widen_pipe(pipe: &std::process::ChildStdin, label: &'static str) {
    use std::os::fd::AsRawFd;
    const F_SETPIPE_SZ: i32 = 1031;
    const TARGET: i32 = 1024 * 1024;
    // SAFETY: `pipe` owns the descriptor for the duration of the call, and
    // F_SETPIPE_SZ only resizes the kernel buffer behind it.
    let applied = unsafe { libc::fcntl(pipe.as_raw_fd(), F_SETPIPE_SZ, TARGET) };
    if applied < 0 {
        log::debug!("{label}: could not widen the encoder pipe; keeping the default size");
    }
}

/// Owns the encoder child process and the thread that feeds it.
pub struct RecordingSink {
    frames: Option<SyncSender<Vec<u8>>>,
    recycled: Receiver<Vec<u8>>,
    pool: Vec<Vec<u8>>,
    frame_size: usize,
    broken: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
    submitted: u64,
    writer: Option<JoinHandle<()>>,
}

impl RecordingSink {
    /// Take ownership of `child` and start feeding its stdin from a worker
    /// thread. `frame_size` is the exact byte length of one captured frame;
    /// the encoder reads raw video, so short or long writes desynchronize it.
    pub fn spawn(mut child: Child, frame_size: usize, label: &'static str) -> Self {
        let (frames_tx, frames_rx) = sync_channel::<Vec<u8>>(QUEUE_DEPTH);
        let (recycled_tx, recycled_rx) = sync_channel::<Vec<u8>>(POOL_DEPTH);
        let broken = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicU64::new(0));

        // The child is handed over only once the thread exists. Capturing it in
        // the closure instead would move it into a closure that a failed spawn
        // drops — and dropping a `Child` neither kills nor reaps it, leaving an
        // ffmpeg behind that waits forever on a pipe nobody will ever write to.
        let (child_tx, child_rx) = std::sync::mpsc::channel::<Child>();

        let writer_broken = Arc::clone(&broken);
        let writer = std::thread::Builder::new()
            .name("jwm-recording-writer".into())
            .spawn(move || {
                let Ok(mut child) = child_rx.recv() else {
                    return;
                };
                let mut stdin = child.stdin.take();
                if let Some(pipe) = stdin.as_ref() {
                    widen_pipe(pipe, label);
                }
                while let Ok(frame) = frames_rx.recv() {
                    if let Some(pipe) = stdin.as_mut()
                        && let Err(error) = pipe.write_all(&frame)
                    {
                        log::warn!("{label}: recording encoder write failed: {error}");
                        writer_broken.store(true, Ordering::Relaxed);
                        // Close the pipe but keep draining the queue. A sink
                        // that stops receiving would make `submit` report the
                        // channel full forever, and the compositor would spend
                        // every capture copying a frame nobody reads.
                        stdin = None;
                    }
                    let _ = recycled_tx.try_send(frame);
                }
                // Dropping stdin is what tells ffmpeg to finalize: it flushes
                // the encoder, writes the moov atom and, with `+faststart`,
                // rewrites the file front-to-back. Doing that here rather than
                // on the compositor thread is why stopping a long recording no
                // longer freezes the desktop for seconds.
                drop(stdin);
                match child.wait() {
                    Ok(status) if !status.success() => {
                        log::warn!("{label}: ffmpeg exited with {status}");
                    }
                    Err(error) => log::warn!("{label}: failed waiting for ffmpeg: {error}"),
                    Ok(_) => {}
                }
            });

        let writer = match writer {
            Ok(handle) => {
                let _ = child_tx.send(child);
                Some(handle)
            }
            Err(error) => {
                // Without the writer thread nothing would ever read `frames_rx`,
                // so every submit would be dropped. Stop the encoder we just
                // started, report broken, and let the caller tear the recording
                // down rather than feed a process that can never be read.
                log::warn!("{label}: failed to spawn recording writer thread: {error}");
                let _ = child.kill();
                let _ = child.wait();
                broken.store(true, Ordering::Relaxed);
                None
            }
        };

        Self {
            frames: Some(frames_tx),
            recycled: recycled_rx,
            pool: Vec::with_capacity(POOL_DEPTH),
            frame_size,
            broken,
            dropped,
            submitted: 0,
            writer,
        }
    }

    /// A buffer already `frame_size` bytes long, recycled when the writer has
    /// finished with an earlier one. Its contents are whatever the previous
    /// frame left behind; callers overwrite all of it from the mapped pixel
    /// buffer. Handing it back at full length rather than empty is deliberate —
    /// a `resize` here would zero 8 MB per frame that the copy overwrites
    /// immediately afterwards.
    pub fn take_buffer(&mut self) -> Vec<u8> {
        // Bounded by the same cap `recycle` honors: draining without it would
        // let the pool hold POOL_DEPTH buffers of its own plus a full recycled
        // channel, i.e. twice the frame memory the constant advertises.
        while self.pool.len() < POOL_DEPTH {
            let Ok(buffer) = self.recycled.try_recv() else {
                break;
            };
            self.pool.push(buffer);
        }
        match self.pool.pop() {
            Some(mut buffer) => {
                buffer.resize(self.frame_size, 0);
                buffer
            }
            None => vec![0; self.frame_size],
        }
    }

    /// Hand a filled frame to the encoder, or drop it if the encoder is behind.
    /// Never blocks. Returns whether the frame was queued.
    pub fn submit(&mut self, frame: Vec<u8>) -> bool {
        debug_assert_eq!(frame.len(), self.frame_size);
        let Some(sender) = self.frames.as_ref() else {
            self.recycle(frame);
            return false;
        };
        match sender.try_send(frame) {
            Ok(()) => {
                self.submitted += 1;
                true
            }
            Err(TrySendError::Full(frame)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                self.recycle(frame);
                false
            }
            Err(TrySendError::Disconnected(frame)) => {
                self.broken.store(true, Ordering::Relaxed);
                self.recycle(frame);
                false
            }
        }
    }

    /// Give back a buffer that was never filled, so a failed capture does not
    /// force the next one to allocate.
    pub fn return_buffer(&mut self, buffer: Vec<u8>) {
        self.recycle(buffer);
    }

    fn recycle(&mut self, buffer: Vec<u8>) {
        if self.pool.len() < POOL_DEPTH {
            self.pool.push(buffer);
        }
    }

    /// True once the encoder pipe has failed. The compositor polls this instead
    /// of learning about the failure from a blocking write.
    pub fn is_broken(&self) -> bool {
        self.broken.load(Ordering::Relaxed)
    }

    pub fn dropped_frames(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn submitted_frames(&self) -> u64 {
        self.submitted
    }

    /// Close the encoder input and return immediately, leaving the writer
    /// thread to drain the queue, close stdin and reap ffmpeg.
    ///
    /// Never joins, on any path. Every caller — stopping a recording, switching
    /// compositing off at runtime, compositor teardown — runs on the thread that
    /// serves input and repaints, and ffmpeg's exit work (flushing the encoder,
    /// then rewriting the whole file for `+faststart`) is exactly the multi-
    /// second stall this type exists to keep off that thread. Detaching is safe:
    /// the writer finalizes on its own, and if the process exits first the
    /// kernel closes ffmpeg's stdin, which it reads as EOF just the same.
    pub fn finish(mut self) -> RecordingSinkStats {
        let stats = self.stats();
        self.frames = None;
        drop(self.writer.take());
        stats
    }

    fn stats(&self) -> RecordingSinkStats {
        RecordingSinkStats {
            submitted: self.submitted,
            dropped: self.dropped_frames(),
            broken: self.is_broken(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RecordingSinkStats {
    pub submitted: u64,
    pub dropped: u64,
    pub broken: bool,
}

impl std::fmt::Display for RecordingSinkStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} frames encoded, {} dropped (encoder behind){}",
            self.submitted,
            self.dropped,
            if self.broken { ", pipe broken" } else { "" }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    /// `cat` stands in for ffmpeg: it reads stdin at whatever rate the writer
    /// thread offers, so the sink's queue never backs up.
    fn sink_over_cat(frame_size: usize) -> RecordingSink {
        let child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cat");
        RecordingSink::spawn(child, frame_size, "test")
    }

    #[test]
    fn buffers_are_recycled_rather_than_reallocated() {
        const FRAME: usize = 16;
        let mut sink = sink_over_cat(FRAME);

        // Tag a buffer, submit it, and wait for that exact allocation to come
        // back from the writer thread. Identity is the pointer: a recycled
        // buffer is the same allocation, a fresh one is not.
        let mut frame = sink.take_buffer();
        assert_eq!(frame.len(), FRAME, "buffers arrive ready to fill");
        let submitted_ptr = frame.as_ptr();
        frame.fill(7);
        assert!(sink.submit(frame));

        let mut recycled_ptr = None;
        for _ in 0..500 {
            // Pull the recycled channel into the pool without consuming it.
            let candidate = sink.take_buffer();
            if candidate.as_ptr() == submitted_ptr {
                recycled_ptr = Some(candidate.as_ptr());
                sink.return_buffer(candidate);
                break;
            }
            sink.return_buffer(candidate);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(
            recycled_ptr,
            Some(submitted_ptr),
            "the writer must hand the frame buffer back for reuse"
        );

        let stats = sink.finish();
        assert_eq!(stats.submitted, 1);
        assert_eq!(stats.dropped, 0);
    }

    #[test]
    fn the_buffer_pool_stays_bounded_under_churn() {
        let mut sink = sink_over_cat(16);
        for _ in 0..200 {
            let frame = sink.take_buffer();
            sink.submit(frame);
        }
        // Let the writer return everything it can, then drain into the pool.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let held = sink.take_buffer();
        sink.return_buffer(held);
        assert!(
            sink.pool.len() <= POOL_DEPTH,
            "pool grew to {} buffers, past the {POOL_DEPTH} cap",
            sink.pool.len()
        );
        sink.finish();
    }

    #[test]
    fn a_stalled_encoder_drops_frames_instead_of_blocking() {
        // `sleep` never reads its stdin, so the pipe fills and the writer
        // thread parks inside write_all. The compositor-side submit must stay
        // non-blocking regardless.
        let child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        // One frame larger than a pipe buffer guarantees the writer blocks.
        let frame_size = 256 * 1024;
        let mut sink = RecordingSink::spawn(child, frame_size, "test");

        let started = std::time::Instant::now();
        for _ in 0..64 {
            let frame = sink.take_buffer();
            assert_eq!(frame.len(), frame_size, "buffers arrive ready to fill");
            sink.submit(frame);
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "submit blocked for {elapsed:?}; it must never wait on the encoder"
        );
        assert!(
            sink.dropped_frames() > 0,
            "a stalled encoder must show up as dropped frames"
        );
        sink.finish();
    }

    #[test]
    fn stats_render_for_the_stop_log() {
        let stats = RecordingSinkStats {
            submitted: 900,
            dropped: 12,
            broken: false,
        };
        assert_eq!(
            stats.to_string(),
            "900 frames encoded, 12 dropped (encoder behind)"
        );
    }
}
