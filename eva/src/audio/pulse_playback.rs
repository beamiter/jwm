//! Native asynchronous PulseAudio output. Only this ordinary worker thread
//! owns the context, stream and operations; cancellation never waits in libpulse
//! simple I/O. Source history advances only on fresh native timing evidence.
use super::pulse::{PulseContext, sample_spec, stream_properties};
use super::{Fault, Output, QUEUE_BYTES, RATE, check, claim_first_played};
use libpulse_binding::{
    def::BufferAttr,
    operation::{Operation, State as OperationState},
    stream::{FlagSet, SeekMode, State, Stream},
};
use std::{
    cell::Cell,
    io,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::Notify;

const FRAME_BYTES: usize = 2;
const BYTES_PER_SECOND: u64 = RATE as u64 * FRAME_BYTES as u64;
const PREBUFFER_BYTES: usize = BYTES_PER_SECOND as usize / 10;
const TARGET_BYTES: usize = BYTES_PER_SECOND as usize / 5;
const WRITE_CHUNK_BYTES: usize = 16_384;
const TIMING_INTERVAL: Duration = Duration::from_millis(20);
const TIMING_FRESHNESS: Duration = Duration::from_millis(100);
const OPERATION_TIMEOUT: Duration = Duration::from_secs(2);

type SuccessOperation = Operation<dyn FnMut(bool)>;
struct Pending {
    operation: SuccessOperation,
    result: Rc<Cell<Option<bool>>>,
    started: Instant,
}
impl Pending {
    fn start(make: impl FnOnce(Box<dyn FnMut(bool)>) -> SuccessOperation) -> Self {
        let result = Rc::new(Cell::new(None));
        let observed = result.clone();
        Self {
            operation: make(Box::new(move |success| observed.set(Some(success)))),
            result,
            started: Instant::now(),
        }
    }
    fn completed(&self) -> io::Result<Option<bool>> {
        if let Some(result) = self.result.get() {
            return Ok(Some(result));
        }
        if self.operation.get_state() == OperationState::Cancelled {
            return Err(io::Error::other("pulse_playback_operation_cancelled"));
        }
        if self.started.elapsed() > OPERATION_TIMEOUT {
            return Err(io::Error::other("pulse_playback_operation_timeout"));
        }
        Ok(None)
    }
}
impl Drop for Pending {
    fn drop(&mut self) {
        if self.operation.get_state() == OperationState::Running {
            self.operation.cancel();
        }
    }
}

/// read_index is the native server's byte cursor. The sink FIFO is additional
/// latency, so bytes submitted, written or merely handed to the sink are never
/// counted as audible. Round the sink backlog up, then complete source frames
/// down. No wall-time interpolation or transport-delay guess is used.
fn played_floor(read_index: i64, sink_usec: u64, submitted_bytes: u64) -> Option<u64> {
    if read_index < 0 || read_index % FRAME_BYTES as i64 != 0 {
        return None;
    }
    let backlog = (sink_usec as u128 * BYTES_PER_SECOND as u128).div_ceil(1_000_000);
    // Some servers advance the native read cursor through trailing silence,
    // others stop it at the last source byte. Never add since_underrun here:
    // that could count the same device progress twice.
    let audible = (read_index as u128).saturating_sub(backlog);
    Some(audible.min(submitted_bytes as u128) as u64 / FRAME_BYTES as u64)
}

/// After DRAIN has acknowledged an exhausted source queue, some servers stop
/// the read index at the last source byte. Their native underrun counter still
/// measures silence rendered after that byte. It proves complete playback
/// only once it covers the entire currently reported sink FIFO; it must never
/// be added to the partial source cursor.
fn drained_tail(read_index: i64, sink_usec: u64, since_underrun: i64, submitted: u64) -> bool {
    let (Ok(read), Ok(silence)) = (u64::try_from(read_index), u64::try_from(since_underrun)) else {
        return false;
    };
    read.is_multiple_of(FRAME_BYTES as u64)
        && submitted > 0
        && submitted.is_multiple_of(FRAME_BYTES as u64)
        && read >= submitted
        && u128::from(silence)
            >= (u128::from(sink_usec) * u128::from(BYTES_PER_SECOND)).div_ceil(1_000_000)
}

fn publish_clock(
    cx: &PulseContext,
    stream: &mut Stream,
    state: &Arc<Mutex<Output>>,
    drain_ack: bool,
) -> Option<u64> {
    let (time, at) = cx.timing(stream)?;
    if at > Instant::now()
        || at.elapsed() > TIMING_FRESHNESS
        || time.read_index_corrupt != 0
        || time.write_index_corrupt != 0
        || time.write_index < 0
    {
        return None;
    }
    let running = drain_ack
        && time.playing == 0
        && stream.is_corked().ok() == Some(false)
        && stream.is_suspended().ok() == Some(false);
    let (first_played, proven) = {
        let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
        let samples = if running
            && drained_tail(
                time.read_index,
                time.sink_usec.0,
                time.since_underrun,
                state.submitted,
            ) {
            state.submitted / FRAME_BYTES as u64
        } else {
            played_floor(time.read_index, time.sink_usec.0, state.submitted)?
        };
        let previous = state.clock.map_or(0, |(_, samples)| samples);
        let proven = samples.max(previous);
        state.clock = Some((at, proven));
        (claim_first_played(&mut state, Some(samples)), proven)
    };
    if let Some((labels, samples)) = first_played {
        // Not a libpulse callback and no Output lock held here.
        let mut fields = labels.as_object().cloned().unwrap_or_default();
        fields.insert("result".into(), serde_json::json!("played"));
        fields.insert("played_samples".into(), serde_json::json!(samples));
        crate::telemetry::event(
            "first_feedback_playback",
            &serde_json::Value::Object(fields),
        );
    }
    Some(proven)
}

fn check_stream(cx: &PulseContext, stream: &Stream) -> io::Result<()> {
    cx.ready()?;
    if stream.get_state() != State::Ready {
        return Err(io::Error::other("pulse_playback_unavailable"));
    }
    Ok(())
}

/// Cork promptly, then take one bounded native timing update before flush can
/// invalidate read_index. This freezes only proven source history. If the
/// server is gone or does not answer, the old clock expires normally.
fn close_stream(cx: &mut PulseContext, stream: &mut Stream, state: &Arc<Mutex<Output>>) {
    let drained = state
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .drained;
    if !drained && check_stream(cx, stream).is_ok() {
        let cork = Pending::start(|callback| stream.cork(Some(callback)));
        let deadline = Instant::now() + Duration::from_millis(40);
        while Instant::now() < deadline && cork.result.get().is_none() {
            if cx.iterate().is_err() || check_stream(cx, stream).is_err() {
                break;
            }
        }
        if cork.result.get() == Some(true) && check_stream(cx, stream).is_ok() {
            let timing = Pending::start(|callback| stream.update_timing_info(Some(callback)));
            let deadline = Instant::now() + Duration::from_millis(30);
            while Instant::now() < deadline && timing.result.get().is_none() {
                if cx.iterate().is_err() || check_stream(cx, stream).is_err() {
                    break;
                }
            }
            if timing.result.get() == Some(true) {
                let _ = publish_clock(cx, stream, state, false);
            }
        }
    }
    if check_stream(cx, stream).is_ok() {
        // There is no callback allocation to retain for this final discard.
        let _flush = stream.flush(None);
        let _ = stream.disconnect();
    } else {
        let _ = stream.disconnect();
    }
    // PulseContext drops immediately after Stream and closes the transport;
    // no process or detached worker keeps a sink-input alive after return.
}

pub(super) fn playback_loop(
    stop: &AtomicBool,
    state: &Arc<Mutex<Output>>,
    fault: &Fault,
    notify: &Arc<Notify>,
) -> io::Result<()> {
    let mut cx = PulseContext::connect(stop)?;
    let spec = sample_spec();
    let mut properties = stream_properties(false)?;
    let mut stream = Stream::new_with_proplist(
        &mut cx.context,
        "Eva feedback",
        &spec,
        None,
        &mut properties,
    )
    .ok_or_else(|| io::Error::other("pulse_playback_stream_creation_failed"))?;
    let underflow = Rc::new(Cell::new(false));
    let flag = underflow.clone();
    stream.set_underflow_callback(Some(Box::new(move || flag.set(true))));
    let moved = Rc::new(Cell::new(false));
    let flag = moved.clone();
    stream.set_moved_callback(Some(Box::new(move || flag.set(true))));
    let sink = std::env::var("PULSE_SINK").ok();
    if sink
        .as_ref()
        .is_some_and(|sink| sink.is_empty() || sink.len() > 512 || sink.as_bytes().contains(&0))
    {
        return Err(io::Error::other("invalid_pulse_playback_target"));
    }
    if stop.load(Ordering::Acquire) {
        return Ok(());
    }
    stream
        .connect_playback(
            sink.as_deref(),
            Some(&BufferAttr {
                maxlength: QUEUE_BYTES as u32,
                tlength: TARGET_BYTES as u32,
                prebuf: PREBUFFER_BYTES as u32,
                minreq: (BYTES_PER_SECOND / 50) as u32,
                fragsize: u32::MAX,
            }),
            FlagSet::START_CORKED
                | FlagSet::AUTO_TIMING_UPDATE
                | FlagSet::ADJUST_LATENCY
                | FlagSet::DONT_MOVE,
            None,
            None,
        )
        .map_err(|_| io::Error::other("pulse_playback_connect_failed"))?;
    if let Err(error) = cx.wait_stream(&stream, stop) {
        let _ = stream.disconnect();
        return if stop.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(error)
        };
    }
    if stream.get_sample_spec() != Some(&spec) {
        let _ = stream.disconnect();
        return Err(io::Error::other("pulse_playback_format_mismatch"));
    }

    let result = (|| {
        let mut timing: Option<Pending> = None;
        let mut start_operations: Vec<Pending> = Vec::with_capacity(2);
        let mut drain: Option<Pending> = None;
        let mut next_timing = Instant::now();
        let mut started = false;
        let mut scratch = Vec::with_capacity(WRITE_CHUNK_BYTES);
        while !stop.load(Ordering::Acquire) {
            cx.iterate()?;
            check(fault)?;
            check_stream(&cx, &stream)?;
            if moved.get() {
                return Err(io::Error::other("pulse_playback_device_changed"));
            }
            if underflow.replace(false) && started && drain.is_none() {
                // Only an already requested drain has submitted every source
                // byte. Merely setting ending can leave bytes in our queue.
                // Never append those bytes behind native underrun silence:
                // that would destroy the mapping from source items to samples.
                return Err(io::Error::other("pulse_playback_underrun"));
            }
            for operation in &start_operations {
                if operation.completed()? == Some(false) {
                    return Err(io::Error::other("pulse_playback_start_failed"));
                }
            }
            start_operations.retain(|operation| operation.result.get().is_none());
            if let Some(operation) = &timing
                && let Some(success) = operation.completed()?
            {
                if success {
                    let _ = publish_clock(&cx, &mut stream, state, false);
                }
                timing = None;
            }
            if timing.is_none() && Instant::now() >= next_timing {
                timing = Some(Pending::start(|callback| {
                    stream.update_timing_info(Some(callback))
                }));
                next_timing = Instant::now() + TIMING_INTERVAL;
            }
            if stop.load(Ordering::Acquire) {
                break;
            }
            let writable = stream
                .writable_size()
                .ok_or_else(|| io::Error::other("pulse_playback_unwritable"))?;
            let (submitted, ending, empty) = {
                let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
                let count =
                    writable.min(WRITE_CHUNK_BYTES).min(state.queue.len()) & !(FRAME_BYTES - 1);
                if count > 0 && !stop.load(Ordering::Acquire) {
                    scratch.clear();
                    scratch.extend(state.queue.iter().take(count).copied());
                    stream
                        .write_copy(&scratch, 0, SeekMode::Relative)
                        .map_err(|_| io::Error::other("pulse_playback_write_failed"))?;
                    drop(state.queue.drain(..count));
                    state.submitted = state
                        .submitted
                        .checked_add(count as u64)
                        .ok_or_else(|| io::Error::other("pulse_playback_counter_overflow"))?;
                    scratch.fill(0);
                }
                (state.submitted, state.ending, state.queue.is_empty())
            };
            if !started && (submitted >= PREBUFFER_BYTES as u64 || (ending && submitted > 0)) {
                start_operations.push(Pending::start(|callback| stream.uncork(Some(callback))));
                // A short local tone must start even below the normal prebuffer.
                start_operations.push(Pending::start(|callback| stream.trigger(Some(callback))));
                started = true;
            }
            if ending && empty && drain.is_none() {
                if submitted == 0 {
                    state
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .drained = true;
                    notify.notify_waiters();
                    break;
                }
                state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .drain_started = true;
                drain = Some(Pending::start(|callback| stream.drain(Some(callback))));
            }
            if let Some(operation) = &drain
                && let Some(success) = operation.completed()?
            {
                if !success {
                    return Err(io::Error::other("pulse_playback_drain_failed"));
                }
                let proven = publish_clock(&cx, &mut stream, state, true);
                let mut output = state.lock().unwrap_or_else(|error| error.into_inner());
                if proven.is_some_and(|samples| samples >= output.submitted / FRAME_BYTES as u64) {
                    output.drained = true;
                    drop(output);
                    notify.notify_waiters();
                    break;
                }
            }
        }
        scratch.fill(0);
        Ok(())
    })();
    close_stream(&mut cx, &mut stream, state);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_cursor_counts_only_samples_past_the_sink_fifo() {
        assert_eq!(played_floor(48_000, 100_000, 48_000), Some(21_600));
        assert_eq!(played_floor(4_800, 100_001, 48_000), Some(0));
        assert_eq!(played_floor(96_000, 0, 48_000), Some(24_000));
        assert_eq!(played_floor(-2, 0, 48_000), None);
        assert_eq!(played_floor(3, 0, 48_000), None);
        assert_eq!(played_floor(2, u64::MAX, 48_000), Some(0));
        assert_eq!(played_floor(i64::MAX - 1, u64::MAX, 48_000), Some(24_000));
        // Drain acknowledgement alone is not proof that hardware has played
        // the FIFO tail. Its native read cursor must advance past that FIFO.
        assert_eq!(played_floor(12_000, 100_000, 12_000), Some(3_600));
        assert_eq!(played_floor(14_400, 100_000, 12_000), Some(4_800));
        assert_eq!(played_floor(16_800, 100_000, 12_000), Some(6_000));
    }

    #[test]
    fn native_drain_tail_requires_silence_covering_the_entire_sink_fifo() {
        assert!(!drained_tail(12_000, 100_000, 4_799, 12_000));
        assert!(drained_tail(12_000, 100_000, 4_800, 12_000));
        assert!(!drained_tail(12_000, 100_001, 4_800, 12_000));
        assert!(drained_tail(12_000, 100_001, 4_801, 12_000));
        assert!(!drained_tail(11_998, 100_000, 4_800, 12_000));
        assert!(!drained_tail(-1, 0, 4_800, 12_000));
        assert!(!drained_tail(12_000, 0, -1, 12_000));
        assert!(!drained_tail(12_001, 0, 4_800, 12_000));
        assert!(!drained_tail(12_000, 0, 4_800, 0));
        assert!(!drained_tail(i64::MAX - 1, u64::MAX, 4_800, 12_000));
        // The independent full-tail proof never changes a partial cursor.
        assert_eq!(played_floor(12_000, 100_000, 12_000), Some(3_600));
    }

    #[test]
    #[ignore = "requires the private PulseAudio fixture from tests/native-pulse.sh"]
    fn native_pulse_playback_drains_and_freezes_partial_history() {
        use std::{os::unix::fs::MetadataExt, path::Path, thread};
        assert_eq!(std::env::var("JWM_EVA_PULSE_TEST").as_deref(), Ok("1"));
        let server = std::env::var("PULSE_SERVER").expect("private server required");
        let socket = Path::new(server.strip_prefix("unix:").expect("Unix fixture required"));
        assert!(socket.is_absolute());
        assert_ne!(
            socket,
            Path::new(&format!("/run/user/{}/pulse/native", unsafe {
                libc::geteuid()
            }))
        );
        let metadata = socket.parent().unwrap().metadata().unwrap();
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
        assert_eq!(metadata.mode() & 0o077, 0);
        assert_eq!(
            std::env::var("PULSE_SINK").as_deref(),
            Ok("eva_test_output")
        );

        struct Worker {
            stop: Arc<AtomicBool>,
            done: Arc<AtomicBool>,
            task: Option<thread::JoinHandle<io::Result<()>>>,
        }
        impl Worker {
            fn start(state: Arc<Mutex<Output>>) -> Self {
                let stop = Arc::new(AtomicBool::new(false));
                let run = stop.clone();
                let done = Arc::new(AtomicBool::new(false));
                let finished = done.clone();
                let task = thread::spawn(move || {
                    let result =
                        playback_loop(&run, &state, &Fault::default(), &Arc::new(Notify::new()));
                    finished.store(true, Ordering::Release);
                    result
                });
                Self {
                    stop,
                    done,
                    task: Some(task),
                }
            }
            fn finish(mut self) -> io::Result<()> {
                // The production facade has its own eight-second timeout;
                // this direct-loop fixture must also stop on a broken drain.
                let deadline = Instant::now() + Duration::from_secs(4);
                while !self.done.load(Ordering::Acquire) && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(5));
                }
                if !self.done.load(Ordering::Acquire) {
                    self.stop.store(true, Ordering::Release);
                    let _ = self.task.take().unwrap().join();
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "native playback fixture watchdog",
                    ));
                }
                self.task.take().unwrap().join().unwrap()
            }
        }
        impl Drop for Worker {
            fn drop(&mut self) {
                self.stop.store(true, Ordering::Release);
                if let Some(task) = self.task.take() {
                    let _ = task.join();
                }
            }
        }
        let state = Arc::new(Mutex::new(Output {
            queue: vec![0; BYTES_PER_SECOND as usize / 4].into(),
            received_samples: RATE as u64 / 4,
            ending: true,
            telemetry: Some(crate::telemetry::fields(
                &serde_json::json!({"phase":"pulse_native_test"}),
            )),
            ..Output::default()
        }));
        let started = Instant::now();
        Worker::start(state.clone()).finish().unwrap();
        let drain_ms = started.elapsed().as_millis();
        assert!(
            started.elapsed() >= Duration::from_millis(180),
            "drain completed before audible duration"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(state.lock().unwrap().drained);

        let state = Arc::new(Mutex::new(Output {
            queue: vec![0; BYTES_PER_SECOND as usize].into(),
            received_samples: RATE as u64,
            ending: false,
            segments: vec![
                super::super::Segment {
                    item_id: "heard".into(),
                    content_index: 0,
                    start: 0,
                    end: 2400,
                },
                super::super::Segment {
                    item_id: "partial".into(),
                    content_index: 0,
                    start: 2400,
                    end: 21600,
                },
                super::super::Segment {
                    item_id: "unheard".into(),
                    content_index: 0,
                    start: 21600,
                    end: 24000,
                },
            ],
            ..Output::default()
        }));
        let worker = Worker::start(state.clone());
        thread::sleep(Duration::from_millis(350));
        let stopped = Instant::now();
        worker.stop.store(true, Ordering::Release);
        worker.finish().unwrap();
        assert!(stopped.elapsed() < Duration::from_millis(200));
        let state = state.lock().unwrap();
        let (_, played) = state.clock.expect("native played position required");
        assert!(
            played > 2400 && played < 21600,
            "invalid played position {played}"
        );
        let edits = super::super::edits(&state, Instant::now());
        assert!(!edits.iter().any(|edit| matches!(edit, super::super::HistoryEdit::Delete { item_id } if item_id == "heard")));
        assert!(edits.iter().any(|edit| matches!(edit, super::super::HistoryEdit::Truncate { item_id, .. } if item_id == "partial")));
        assert!(edits.iter().any(|edit| matches!(edit, super::super::HistoryEdit::Delete { item_id } if item_id == "unheard")));
        eprintln!(
            "native PulseAudio: drain={}ms stop={}ms partial={}ms",
            drain_ms,
            stopped.elapsed().as_millis(),
            played * 1000 / RATE as u64
        );
    }
}
