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

use crate::backend::recording_completion::{
    EncoderWaitOutcome, WriterCompletionReceipt, WriterCompletionReporter, WriterFailure,
    writer_completion_channel,
};

use std::io::{self, Write};
use std::os::fd::{AsRawFd as _, RawFd};
use std::process::{Child, ExitStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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

/// A healthy encoder consumes one raw frame in far less than this. Bounding a
/// single write keeps a wedged encoder from pinning its writer thread forever.
const FRAME_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Closing stdin normally lets ffmpeg flush and rewrite even a long recording.
/// A process that ignores EOF is eventually killed rather than leaked forever.
const ENCODER_FINALIZE_TIMEOUT: Duration = Duration::from_secs(120);
const PIPE_POLL_SLICE: Duration = Duration::from_millis(20);

/// Ask the kernel for a bigger pipe to the encoder.
///
/// A pipe holds 64 KiB by default, so a 1080p frame is 127 separate blocking
/// writes and a 4K frame is 507. Widening it to the usual 1 MiB ceiling cuts
/// that to 8 and 32, and gives the encoder that much more slack to absorb a
/// hiccup before the sink has to start dropping frames. Best effort: the limit
/// is `/proc/sys/fs/pipe-max-size` and an unprivileged process may not reach it.
fn widen_pipe(pipe: &std::process::ChildStdin, label: &'static str) {
    const F_SETPIPE_SZ: i32 = 1031;
    const TARGET: i32 = 1024 * 1024;
    // SAFETY: `pipe` owns the descriptor for the duration of the call, and
    // F_SETPIPE_SZ only resizes the kernel buffer behind it.
    let applied = unsafe { libc::fcntl(pipe.as_raw_fd(), F_SETPIPE_SZ, TARGET) };
    if applied < 0 {
        log::debug!("{label}: could not widen the encoder pipe; keeping the default size");
    }
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn wait_until_writable(fd: RawFd, remaining: Duration) -> io::Result<()> {
    let wait = remaining.min(PIPE_POLL_SLICE);
    let timeout_ms = i32::try_from(wait.as_millis().max(1)).unwrap_or(i32::MAX);
    let mut poll_fd = libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
    if ready >= 0 {
        if ready > 0 && poll_fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "encoder stdin closed while waiting to write",
            ));
        }
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::Interrupted {
        // Retry the write in the outer loop, where the frame-wide deadline is
        // checked. Repeating this poll with a fresh slice could otherwise
        // extend the deadline under a signal storm.
        return Ok(());
    }
    Err(error)
}

fn write_frame_with_timeout(
    pipe: &mut std::process::ChildStdin,
    mut frame: &[u8],
    timeout: Duration,
) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    while !frame.is_empty() {
        match pipe.write(frame) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "encoder stdin accepted zero bytes",
                ));
            }
            Ok(written) => frame = &frame[written..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("encoder frame write exceeded {timeout:?}"),
                    ));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("encoder frame write exceeded {timeout:?}"),
                    ));
                }
                wait_until_writable(pipe.as_raw_fd(), deadline.saturating_duration_since(now))?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

struct EncoderWaitResult {
    result: io::Result<ExitStatus>,
    timed_out: bool,
    encoder_exited: bool,
}

// A CPU-only test seam for the exact process-wait decision tree. Production
// still owns one std::process::Child and executes the same bounded cleanup.
trait EncoderProcess {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>>;
    fn kill(&mut self) -> io::Result<()>;
    fn wait(&mut self) -> io::Result<ExitStatus>;
}

impl EncoderProcess for Child {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        Child::try_wait(self)
    }
    fn kill(&mut self) -> io::Result<()> {
        Child::kill(self)
    }
    fn wait(&mut self) -> io::Result<ExitStatus> {
        Child::wait(self)
    }
}

fn wait_for_encoder_with_timeout(
    child: &mut impl EncoderProcess,
    timeout: Duration,
) -> EncoderWaitResult {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return EncoderWaitResult {
                    result: Ok(status),
                    timed_out: false,
                    encoder_exited: true,
                };
            }
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let encoder_exited = child.wait().is_ok();
                return EncoderWaitResult {
                    result: Err(error),
                    timed_out: false,
                    encoder_exited,
                };
            }
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        std::thread::sleep(
            deadline
                .saturating_duration_since(now)
                .min(Duration::from_millis(20)),
        );
    }
    let kill_error = child
        .kill()
        .err()
        .filter(|error| error.kind() != io::ErrorKind::InvalidInput);
    let status = child.wait();
    let encoder_exited = status.is_ok();
    EncoderWaitResult {
        result: kill_error.map_or(status, Err),
        timed_out: true,
        encoder_exited,
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
    completion: WriterCompletionReceipt,
}

impl RecordingSink {
    /// Take ownership of `child` and start feeding its stdin from a worker
    /// thread. `frame_size` is the exact byte length of one captured frame;
    /// the encoder reads raw video, so short or long writes desynchronize it.
    pub fn spawn(child: Child, frame_size: usize, label: &'static str) -> Self {
        Self::spawn_with_timeouts(
            child,
            frame_size,
            label,
            FRAME_WRITE_TIMEOUT,
            ENCODER_FINALIZE_TIMEOUT,
        )
    }

    fn spawn_with_timeouts(
        mut child: Child,
        frame_size: usize,
        label: &'static str,
        write_timeout: Duration,
        finalize_timeout: Duration,
    ) -> Self {
        let (frames_tx, frames_rx) = sync_channel::<Vec<u8>>(QUEUE_DEPTH);
        let (recycled_tx, recycled_rx) = sync_channel::<Vec<u8>>(POOL_DEPTH);
        let broken = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicU64::new(0));

        // The child is handed over only once the thread exists. Capturing it in
        // the closure instead would move it into a closure that a failed spawn
        // drops — and dropping a `Child` neither kills nor reaps it, leaving an
        // ffmpeg behind that waits forever on a pipe nobody will ever write to.
        let (completion_reporter, completion) = writer_completion_channel();
        let (child_tx, child_rx) = std::sync::mpsc::channel::<(Child, WriterCompletionReporter)>();

        let writer_broken = Arc::clone(&broken);
        let writer = std::thread::Builder::new()
            .name("jwm-recording-writer".into())
            .spawn(move || {
                let Ok((mut child, completion_reporter)) = child_rx.recv() else {
                    return;
                };
                let Some(mut stdin) = child.stdin.take() else {
                    log::warn!("{label}: recording encoder stdin was not captured");
                    writer_broken.store(true, Ordering::Relaxed);
                    let _ = child.kill();
                    let encoder_exited = child.wait().is_ok();
                    completion_reporter.finish_with_exit(
                        Some(WriterFailure::MissingStdin),
                        None,
                        encoder_exited,
                    );
                    return;
                };
                widen_pipe(&stdin, label);
                if let Err(error) = set_nonblocking(stdin.as_raw_fd()) {
                    log::warn!("{label}: could not make encoder pipe nonblocking: {error}");
                    writer_broken.store(true, Ordering::Relaxed);
                    let _ = child.kill();
                    let encoder_exited = child.wait().is_ok();
                    completion_reporter.finish_with_exit(
                        Some(WriterFailure::PipeSetup),
                        None,
                        encoder_exited,
                    );
                    return;
                }
                let mut write_failed = false;
                while let Ok(frame) = frames_rx.recv() {
                    if let Err(error) = write_frame_with_timeout(&mut stdin, &frame, write_timeout)
                    {
                        log::warn!("{label}: recording encoder write failed: {error}");
                        writer_broken.store(true, Ordering::Relaxed);
                        write_failed = true;
                    }
                    let _ = recycled_tx.try_send(frame);
                    if write_failed {
                        break;
                    }
                }
                // Dropping stdin is what tells ffmpeg to finalize: it flushes
                // the encoder, writes the moov atom and, with `+faststart`,
                // rewrites the file front-to-back. Doing that here rather than
                // on the compositor thread is why stopping a long recording no
                // longer freezes the desktop for seconds.
                drop(stdin);
                if write_failed {
                    let _ = child.kill();
                }
                let waited = wait_for_encoder_with_timeout(&mut child, finalize_timeout);
                let outcome = if waited.timed_out {
                    EncoderWaitOutcome::TimedOut
                } else {
                    match &waited.result {
                        Ok(status) => EncoderWaitOutcome::Exited(status.success()),
                        Err(_) => EncoderWaitOutcome::Failed,
                    }
                };
                match waited.result {
                    Ok(status) if !status.success() => {
                        log::warn!("{label}: ffmpeg exited with {status}");
                    }
                    Err(error) => log::warn!("{label}: failed waiting for ffmpeg: {error}"),
                    Ok(_) => {}
                }
                completion_reporter.finish_with_exit(
                    write_failed.then_some(WriterFailure::FrameWrite),
                    Some(outcome),
                    waited.encoder_exited,
                );
            });

        let writer = match writer {
            Ok(handle) => {
                if let Err(error) = child_tx.send((child, completion_reporter)) {
                    // The writer exited before accepting ownership. Recover
                    // the Child from SendError instead of dropping it alive.
                    let (mut child, completion_reporter) = error.0;
                    let _ = child.kill();
                    let encoder_exited = child.wait().is_ok();
                    broken.store(true, Ordering::Relaxed);
                    completion_reporter.finish_with_exit(
                        Some(WriterFailure::Handoff),
                        None,
                        encoder_exited,
                    );
                }
                Some(handle)
            }
            Err(error) => {
                // Without the writer thread nothing would ever read `frames_rx`,
                // so every submit would be dropped. Stop the encoder we just
                // started, report broken, and let the caller tear the recording
                // down rather than feed a process that can never be read.
                log::warn!("{label}: failed to spawn recording writer thread: {error}");
                let _ = child.kill();
                let encoder_exited = child.wait().is_ok();
                broken.store(true, Ordering::Relaxed);
                completion_reporter.finish_with_exit(
                    Some(WriterFailure::Spawn),
                    None,
                    encoder_exited,
                );
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
            completion,
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

    /// Frames accepted by the writer queue, not completed encoder output.
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
    pub fn finish(self) -> RecordingSinkStats {
        self.finish_tracked().0
    }

    /// The same non-blocking stop, with an owned observation of this writer.
    /// Successful exit is not authority over a file at its former pathname.
    pub(crate) fn finish_tracked(mut self) -> (RecordingSinkStats, WriterCompletionReceipt) {
        let stats = self.stats();
        self.frames = None;
        drop(self.writer.take());
        (stats, self.completion)
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
    /// Accepted queue submissions; ffmpeg may duplicate/drop frames or fail later.
    pub submitted: u64,
    pub dropped: u64,
    pub broken: bool,
}

impl std::fmt::Display for RecordingSinkStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} frames queued, {} dropped (queue full){}",
            self.submitted,
            self.dropped,
            if self.broken { ", pipe broken" } else { "" }
        )
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn encoder_exit_proof_tracks_reaping_independently_of_cleanup_errors() {
        use std::os::unix::process::ExitStatusExt;
        struct FakeChild {
            probe: u8,
            kill_ok: bool,
            wait_ok: bool,
            kills: usize,
            waits: usize,
        }
        impl EncoderProcess for FakeChild {
            fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
                match self.probe {
                    0 => Ok(Some(ExitStatus::from_raw(0))),
                    1 => Ok(None),
                    _ => Err(io::Error::other("probe failed")),
                }
            }
            fn kill(&mut self) -> io::Result<()> {
                self.kills += 1;
                if self.kill_ok {
                    Ok(())
                } else {
                    Err(io::Error::other("kill failed"))
                }
            }
            fn wait(&mut self) -> io::Result<ExitStatus> {
                self.waits += 1;
                if self.wait_ok {
                    Ok(ExitStatus::from_raw(1 << 8))
                } else {
                    Err(io::Error::other("wait failed"))
                }
            }
        }
        for probe in 0..3 {
            for kill_ok in [false, true] {
                for wait_ok in [false, true] {
                    let mut child = FakeChild {
                        probe,
                        kill_ok,
                        wait_ok,
                        kills: 0,
                        waits: 0,
                    };
                    let result = wait_for_encoder_with_timeout(&mut child, Duration::ZERO);
                    assert_eq!(result.encoder_exited, probe == 0 || wait_ok);
                    assert_eq!(result.timed_out, probe == 1);
                    assert_eq!(child.kills, usize::from(probe != 0));
                    assert_eq!(child.waits, usize::from(probe != 0));
                    assert_eq!(
                        result.result.is_ok(),
                        probe == 0 || (probe == 1 && kill_ok && wait_ok)
                    );
                }
            }
        }
    }

    use super::*;
    use std::process::{Command, Stdio};

    fn assert_process_reaped(pid: u32) {
        let proc_path = std::path::PathBuf::from(format!("/proc/{pid}"));
        for _ in 0..100 {
            // A zombie retains its /proc entry, so disappearance proves both
            // termination and reaping without racing the writer's waitpid.
            if !proc_path.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        let mut status = 0;
        let _ = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
        panic!("encoder process {pid} was not reaped promptly");
    }

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
    fn runtime_encoder_failure_is_nonblocking_and_belongs_to_the_exact_sink() {
        let child = Command::new("sh")
            .args(["-c", "exit 7"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let mut broken = RecordingSink::spawn_with_timeouts(
            child,
            16,
            "test",
            Duration::from_millis(25),
            Duration::from_millis(25),
        );
        let healthy = sink_over_cat(16);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !broken.is_broken() {
            assert!(
                Instant::now() < deadline,
                "the closed fake encoder pipe was not observed"
            );
            let frame = broken.take_buffer();
            broken.submit(frame);
            std::thread::yield_now();
        }
        assert!(
            !healthy.is_broken(),
            "the new sink cannot inherit an old failure"
        );
        let before = Instant::now();
        let (stats, receipt) = broken.finish_tracked();
        assert!(before.elapsed() < Duration::from_secs(1));
        assert!(stats.broken);
        assert_process_reaped(pid);
        let mut observations =
            crate::backend::recording_completion::RecordingStopObservations::default();
        observations.observe_with_notice(
            Some(
                crate::backend::recording_completion::RecordingStopReceipt::from_writer(Some(
                    receipt,
                )),
            ),
            true,
            true,
        );
        while observations.audio_release()
            == crate::backend::recording_completion::AudioRelease::Pending
        {
            assert!(
                Instant::now() < deadline,
                "the fake writer did not publish its completion"
            );
            std::thread::yield_now();
        }
        assert_eq!(
            observations.audio_release(),
            crate::backend::recording_completion::AudioRelease::Available
        );
        assert!(!observations.take_notices().failed);
        assert!(!healthy.is_broken());
        healthy.finish();
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
            .arg("10")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        // One frame larger than a pipe buffer guarantees the writer blocks.
        let frame_size = 256 * 1024;
        let mut sink = RecordingSink::spawn_with_timeouts(
            child,
            frame_size,
            "test",
            Duration::from_millis(25),
            Duration::from_millis(25),
        );

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
        assert_process_reaped(pid);
    }

    #[test]
    fn finish_reaps_an_encoder_that_ignores_stdin_eof() {
        let child = Command::new("sleep")
            .arg("10")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        let sink = RecordingSink::spawn_with_timeouts(
            child,
            16,
            "test",
            Duration::from_millis(25),
            Duration::from_millis(25),
        );

        let started = Instant::now();
        sink.finish();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "finish blocked the compositor instead of detaching finalization"
        );
        assert_process_reaped(pid);
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
            "900 frames queued, 12 dropped (queue full)"
        );
    }
}
