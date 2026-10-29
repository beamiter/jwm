//! Native PulseAudio streams live only on the owning audio loop thread.
//! The ordinary mainloop has a finite poll, so stop never waits for a blocking
//! libpulse-simple read. Native timing/index evidence supplies source time.
use super::{Fault, Frame, Framer, QUEUE_BYTES, RATE, check, fail};
use libpulse_binding as pulse;
use pulse::{
    callbacks::ListResult,
    context::{Context, FlagSet as ContextFlags, State as ContextState},
    def::{BufferAttr, TimingInfo},
    mainloop::standard::Mainloop,
    proplist::Proplist,
    sample::{Format, Spec},
    stream::{FlagSet, PeekResult, State as StreamState, Stream},
    time::{MicroSeconds, UnixTs},
};
use std::{
    cell::{Cell, RefCell},
    io,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::PathBuf,
    rc::Rc,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const TIMING_MAX_AGE: Duration = Duration::from_millis(250);
const CLOCK_TOLERANCE: Duration = Duration::from_millis(5);

fn error(code: &str) -> io::Error {
    io::Error::other(code.to_owned())
}
fn stopped(stop: &AtomicBool) -> io::Result<()> {
    if stop.load(Ordering::Acquire) {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "audio_cancelled",
        ))
    } else {
        Ok(())
    }
}

fn server_address() -> io::Result<String> {
    let path = match std::env::var("PULSE_SERVER") {
        Ok(value) if !value.is_empty() => PathBuf::from(
            value
                .strip_prefix("unix:")
                .ok_or_else(|| error("pulse_remote_server_unsupported"))?,
        ),
        _ => PathBuf::from(
            std::env::var_os("XDG_RUNTIME_DIR")
                .ok_or_else(|| error("pulse_runtime_unavailable"))?,
        )
        .join("pulse/native"),
    };
    if !path.is_absolute() {
        return Err(error("pulse_server_must_be_absolute"));
    }
    let metadata = std::fs::symlink_metadata(&path)?;
    if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(error("pulse_server_not_owned_socket"));
    }
    let path = path
        .to_str()
        .filter(|path| !path.contains('\0'))
        .ok_or_else(|| error("pulse_server_path_invalid"))?;
    Ok(format!("unix:{path}"))
}

struct ClockAnchor {
    wall: UnixTs,
    monotonic: Instant,
    trustworthy: bool,
}
fn bracketed_clock(before: Instant, wall: UnixTs, after: Instant) -> Option<(UnixTs, Instant)> {
    let interval = after.checked_duration_since(before)?;
    (interval <= Duration::from_millis(1)).then(|| (wall, before + interval / 2))
}
fn clock_sample() -> Option<(UnixTs, Instant)> {
    let before = Instant::now();
    let wall = UnixTs::now();
    let after = Instant::now();
    bracketed_clock(before, wall, after)
}
impl ClockAnchor {
    fn new() -> Self {
        let before = Instant::now();
        let wall = UnixTs::now();
        let after = Instant::now();
        let interval = after.saturating_duration_since(before);
        Self {
            wall,
            monotonic: before + interval / 2,
            trustworthy: interval <= Duration::from_millis(1),
        }
    }
    fn map(&self, timestamp: UnixTs) -> Option<Instant> {
        let (wall, now) = clock_sample()?;
        self.map_at(timestamp, wall, now)
    }
    fn changed_now(&self) -> Option<bool> {
        let (wall, now) = clock_sample()?;
        Some(self.changed_at(wall, now))
    }
    fn map_at(&self, timestamp: UnixTs, wall_now: UnixTs, now: Instant) -> Option<Instant> {
        if !self.trustworthy || self.changed_at(wall_now, now) || timestamp > wall_now {
            return None;
        }
        if Duration::from_micros(UnixTs::diff(&wall_now, &timestamp).0) > TIMING_MAX_AGE {
            return None;
        }
        let distance = Duration::from_micros(UnixTs::diff(&timestamp, &self.wall).0);
        if timestamp >= self.wall {
            self.monotonic.checked_add(distance)
        } else {
            self.monotonic.checked_sub(distance)
        }
    }
    fn changed_at(&self, wall_now: UnixTs, now: Instant) -> bool {
        if !self.trustworthy {
            return false;
        }
        if wall_now < self.wall {
            return true;
        }
        let Some(monotonic_elapsed) = now.checked_duration_since(self.monotonic) else {
            return true;
        };
        let wall_elapsed = Duration::from_micros(UnixTs::diff(&wall_now, &self.wall).0);
        wall_elapsed.abs_diff(monotonic_elapsed) > CLOCK_TOLERANCE
    }
}

/// Shared by capture/playback. It must remain on its creating loop thread.
pub(super) struct PulseContext {
    pub(super) context: Context,
    mainloop: Mainloop,
    pub(super) local: bool,
    clock: ClockAnchor,
}
impl PulseContext {
    pub(super) fn connect(stop: &AtomicBool) -> io::Result<Self> {
        stopped(stop)?;
        let server = server_address()?;
        let mainloop = Mainloop::new().ok_or_else(|| error("pulse_mainloop_unavailable"))?;
        let mut props = stream_properties(false)?;
        let mut context = Context::new_with_proplist(&mainloop, "JWM Eva", &props)
            .ok_or_else(|| error("pulse_context_unavailable"))?;
        context
            .connect(Some(&server), ContextFlags::NOAUTOSPAWN, None)
            .map_err(|_| error("pulse_connect_failed"))?;
        let mut this = Self {
            context,
            mainloop,
            local: false,
            clock: ClockAnchor::new(),
        };
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        loop {
            stopped(stop)?;
            match this.context.get_state() {
                ContextState::Ready => break,
                ContextState::Failed | ContextState::Terminated => {
                    return Err(error("pulse_context_failed"));
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                return Err(error("pulse_connect_timeout"));
            }
            this.iterate()?;
        }
        this.local = this.context.is_local() == Some(true);
        if !this.local {
            return Err(error("pulse_remote_server_unsupported"));
        }
        // Keep the property allocation entirely before audio callbacks begin.
        props.clear();
        Ok(this)
    }
    pub(super) fn ready(&self) -> io::Result<()> {
        if self.context.get_state() == ContextState::Ready {
            Ok(())
        } else {
            Err(error("pulse_context_failed"))
        }
    }
    pub(super) fn iterate(&mut self) -> io::Result<()> {
        self.mainloop
            .prepare(Some(MicroSeconds(10_000)))
            .map_err(|_| error("pulse_mainloop_failed"))?;
        self.mainloop
            .poll()
            .map_err(|_| error("pulse_mainloop_failed"))?;
        self.mainloop
            .dispatch()
            .map_err(|_| error("pulse_mainloop_failed"))?;
        Ok(())
    }
    pub(super) fn wait_stream(&mut self, stream: &Stream, stop: &AtomicBool) -> io::Result<()> {
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        loop {
            stopped(stop)?;
            self.ready()?;
            match stream.get_state() {
                StreamState::Ready => return Ok(()),
                StreamState::Failed | StreamState::Terminated => {
                    return Err(error("pulse_stream_failed"));
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                return Err(error("pulse_stream_timeout"));
            }
            self.iterate()?;
        }
    }
    pub(super) fn timing(&self, stream: &mut Stream) -> Option<(TimingInfo, Instant)> {
        let info = *stream.get_timing_info()?;
        if !self.local
            || info.synchronized_clocks == 0
            || info.read_index_corrupt != 0
            || info.write_index_corrupt != 0
        {
            return None;
        }
        Some((info, self.clock.map(info.timestamp)?))
    }
    fn server_info(&mut self, stop: &AtomicBool) -> io::Result<ServerInfo> {
        let output = Rc::new(RefCell::new(None));
        let received = output.clone();
        let _operation = self.context.introspect().get_server_info(move |info| {
            *received.borrow_mut() = Some(ServerInfo {
                pipewire: info
                    .server_name
                    .as_deref()
                    .is_some_and(|name| name.to_ascii_lowercase().contains("pipewire")),
                source: info.default_source_name.as_deref().map(str::to_owned),
                sink: info.default_sink_name.as_deref().map(str::to_owned),
            });
        });
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        while output.borrow().is_none() {
            stopped(stop)?;
            self.ready()?;
            if Instant::now() >= deadline {
                return Err(error("pulse_server_info_timeout"));
            }
            self.iterate()?;
        }
        let value = output.borrow_mut().take();
        value.ok_or_else(|| error("pulse_server_info_unavailable"))
    }
    fn source_available(&mut self, name: &str, stop: &AtomicBool) -> io::Result<bool> {
        let output = Rc::new(Cell::new(None));
        let received = output.clone();
        let _operation = self
            .context
            .introspect()
            .get_source_info_by_name(name, move |result| match result {
                ListResult::Item(info) => received.set(Some(info.monitor_of_sink.is_none())),
                ListResult::End => {
                    if received.get().is_none() {
                        received.set(Some(false));
                    }
                }
                ListResult::Error => received.set(Some(false)),
            });
        self.wait_flag(&output, stop)?;
        Ok(output.get().unwrap_or(false))
    }
    fn sink_available(&mut self, name: &str, stop: &AtomicBool) -> io::Result<bool> {
        let output = Rc::new(Cell::new(None));
        let received = output.clone();
        let _operation = self
            .context
            .introspect()
            .get_sink_info_by_name(name, move |result| match result {
                ListResult::Item(_) => received.set(Some(true)),
                ListResult::End => {
                    if received.get().is_none() {
                        received.set(Some(false));
                    }
                }
                ListResult::Error => received.set(Some(false)),
            });
        self.wait_flag(&output, stop)?;
        Ok(output.get().unwrap_or(false))
    }
    fn wait_flag(&mut self, flag: &Cell<Option<bool>>, stop: &AtomicBool) -> io::Result<()> {
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        while flag.get().is_none() {
            stopped(stop)?;
            self.ready()?;
            if Instant::now() >= deadline {
                return Err(error("pulse_introspection_timeout"));
            }
            self.iterate()?;
        }
        Ok(())
    }
}
impl Drop for PulseContext {
    fn drop(&mut self) {
        self.context.disconnect();
    }
}
struct ServerInfo {
    pipewire: bool,
    source: Option<String>,
    sink: Option<String>,
}
pub(super) struct Probe {
    pub(super) is_pipewire_server: bool,
    pub(super) source_available: bool,
    pub(super) sink_available: bool,
}
pub(super) fn probe(device: &str, stop: &AtomicBool) -> io::Result<Probe> {
    validate_device(device)?;
    let mut pulse = PulseContext::connect(stop)?;
    let info = pulse.server_info(stop)?;
    let source = if device == "default" {
        info.source.as_deref()
    } else {
        Some(device)
    };
    let source_available = match source {
        Some(name) => pulse.source_available(name, stop)?,
        None => false,
    };
    let sink_available = match info.sink.as_deref() {
        Some(name) => pulse.sink_available(name, stop)?,
        None => false,
    };
    Ok(Probe {
        is_pipewire_server: info.pipewire,
        source_available,
        sink_available,
    })
}
fn validate_device(device: &str) -> io::Result<()> {
    if device.is_empty() || device.len() > 512 || device.contains('\0') {
        Err(error("invalid_input_device"))
    } else {
        Ok(())
    }
}
pub(super) fn sample_spec() -> Spec {
    Spec {
        format: Format::S16le,
        channels: 1,
        rate: RATE,
    }
}
pub(super) fn stream_properties(capture: bool) -> io::Result<Proplist> {
    let mut props = Proplist::new().ok_or_else(|| error("pulse_properties_unavailable"))?;
    for (key, value) in [
        (pulse::proplist::properties::APPLICATION_NAME, "JWM Eva"),
        (pulse::proplist::properties::APPLICATION_ID, "org.jwm.eva"),
        (pulse::proplist::properties::MEDIA_ROLE, "phone"),
        (
            pulse::proplist::properties::MEDIA_NAME,
            if capture {
                "jwm-eva.capture"
            } else {
                "jwm-eva.playback"
            },
        ),
    ] {
        props
            .set_str(key, value)
            .map_err(|_| error("pulse_properties_invalid"))?;
    }
    Ok(props)
}

/// pa_timing_info timestamps the server observation. The official record
/// latency is source + buffer + transport - sink; its corresponding receipt
/// instant is timestamp + transport, not this loop's callback arrival time.
fn capture_timestamp(info: &TimingInfo, at: Instant) -> Option<Instant> {
    if info.write_index < 0
        || info.read_index < 0
        || info.write_index < info.read_index
        || info.write_index % 2 != 0
        || info.read_index % 2 != 0
        || info.sink_usec.0 != 0
    {
        return None;
    }
    let buffered = (info.write_index - info.read_index) as u128;
    let buffer_ns = buffered.checked_mul(1_000_000_000)? / (u128::from(RATE) * 2);
    let latency_ns = u128::from(info.source_usec.0)
        .checked_add(u128::from(info.transport_usec.0))?
        .checked_mul(1000)?
        .checked_add(buffer_ns)?;
    if latency_ns > 5_000_000_000 {
        return None;
    }
    let receipt = at.checked_add(Duration::from_micros(info.transport_usec.0))?;
    receipt.checked_sub(Duration::from_nanos(u64::try_from(latency_ns).ok()?))
}

fn refresh_timing(
    pulse: &mut PulseContext,
    stream: &mut Stream,
    stop: &AtomicBool,
) -> io::Result<()> {
    let done = Rc::new(Cell::new(None));
    let completed = done.clone();
    let _operation =
        stream.update_timing_info(Some(Box::new(move |success| completed.set(Some(success)))));
    let deadline = Instant::now() + TIMING_MAX_AGE;
    while done.get().is_none() && Instant::now() < deadline {
        stopped(stop)?;
        pulse.ready()?;
        if stream.get_state() != StreamState::Ready {
            return Err(error("pulse_stream_failed"));
        }
        pulse.iterate()?;
    }
    Ok(())
}
pub(super) fn capture_loop(
    device: &str,
    tx: &mpsc::Sender<io::Result<Frame>>,
    stop: &AtomicBool,
    fault: &Fault,
) -> io::Result<()> {
    let result = capture_run(device, tx, stop, fault);
    match result {
        Err(error)
            if stop.load(Ordering::Acquire) && error.kind() == io::ErrorKind::Interrupted =>
        {
            Ok(())
        }
        result => result,
    }
}
fn capture_run(
    device: &str,
    tx: &mpsc::Sender<io::Result<Frame>>,
    stop: &AtomicBool,
    fault: &Fault,
) -> io::Result<()> {
    if stop.load(Ordering::Acquire) {
        return Ok(());
    }
    validate_device(device)?;
    let mut pulse = PulseContext::connect(stop)?;
    let info = pulse.server_info(stop)?;
    let source = if device == "default" {
        info.source
            .ok_or_else(|| error("pulse_source_unavailable"))?
    } else {
        device.to_owned()
    };
    if !pulse.source_available(&source, stop)? {
        return Err(error("pulse_source_unavailable_or_monitor"));
    }
    let mut properties = stream_properties(true)?;
    let mut stream = Stream::new_with_proplist(
        &mut pulse.context,
        "jwm-eva.capture",
        &sample_spec(),
        None,
        &mut properties,
    )
    .ok_or_else(|| error("pulse_stream_unavailable"))?;
    let overflow = Rc::new(Cell::new(false));
    let overflowed = overflow.clone();
    stream.set_overflow_callback(Some(Box::new(move || overflowed.set(true))));
    let moved = Rc::new(Cell::new(false));
    let changed = moved.clone();
    stream.set_moved_callback(Some(Box::new(move || changed.set(true))));
    let result = (|| {
        let attrs = BufferAttr {
            maxlength: QUEUE_BYTES as u32,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: 960,
        };
        stopped(stop)?;
        stream
            .connect_record(
                Some(&source),
                Some(&attrs),
                FlagSet::AUTO_TIMING_UPDATE | FlagSet::ADJUST_LATENCY | FlagSet::DONT_MOVE,
            )
            .map_err(|_| error("pulse_capture_connect_failed"))?;
        pulse.wait_stream(&stream, stop)?;
        if stream.get_sample_spec().copied() != Some(sample_spec()) {
            return Err(error("audio_format_mismatch"));
        }
        if stream.get_device_name().as_deref() != Some(source.as_str()) {
            return Err(error("pulse_capture_device_changed"));
        }
        let mut framer = Framer::default();
        let mut received = false;
        let started = Instant::now();
        while !stop.load(Ordering::Acquire) {
            pulse.iterate()?;
            pulse.ready()?;
            check(fault)?;
            let clock_change = pulse.clock.changed_now();
            if clock_change == Some(true) {
                return Err(error(
                    "audio_timestamp_discontinuity:pulse_wall_clock_changed",
                ));
            }
            if stream.get_state() != StreamState::Ready {
                return Err(error("pulse_stream_failed"));
            }
            if overflow.get() {
                return Err(error("audio_buffer_overflow:pulse_server_overflow"));
            }
            if moved.get() {
                return Err(error("pulse_capture_device_changed"));
            }
            if stream
                .is_suspended()
                .map_err(|_| error("pulse_stream_failed"))?
            {
                if received || started.elapsed() > CONNECT_TIMEOUT {
                    return Err(error("pulse_source_suspended"));
                }
                continue;
            }
            let available = stream
                .readable_size()
                .ok_or_else(|| error("pulse_capture_read_failed"))?;
            if available > QUEUE_BYTES {
                return Err(error(&format!(
                    "audio_buffer_overflow:pulse_readable_bytes={available}"
                )));
            }
            if available == 0 {
                if !received && started.elapsed() > CONNECT_TIMEOUT {
                    return Err(error("pulse_capture_timeout"));
                }
                continue;
            }
            let mut timing = pulse.timing(&mut stream);
            if timing.as_ref().is_none_or(|(info, _)| {
                info.write_index
                    .checked_sub(info.read_index)
                    .is_none_or(|bytes| bytes < available as i64)
            }) {
                refresh_timing(&mut pulse, &mut stream, stop)?;
                timing = pulse.timing(&mut stream);
            }
            let after_refresh = pulse.clock.changed_now();
            if after_refresh == Some(true) {
                return Err(error(
                    "audio_timestamp_discontinuity:pulse_wall_clock_changed",
                ));
            }
            // A wide read-clock bracket reflects local scheduling uncertainty,
            // not proof that the wall clock jumped. This frame remains unbound.
            if clock_change.is_none() || after_refresh.is_none() {
                timing = None;
            }
            let timestamp = timing
                .as_ref()
                .and_then(|(info, at)| capture_timestamp(info, *at));
            match stream
                .peek()
                .map_err(|_| error("pulse_capture_read_failed"))?
            {
                PeekResult::Empty => continue,
                PeekResult::Hole(_) => return Err(error("audio_capture_hole")),
                PeekResult::Data(bytes) => {
                    framer.push(bytes, timestamp, timestamp.is_some(), tx).map_err(|error|io::Error::other(format!(
                        "{error}:pulse_fragment_bytes={} readable_bytes={available} channel_capacity={} remaining_capacity={}",
                        bytes.len(),tx.max_capacity(),tx.capacity()
                    )))?
                }
            }
            stream
                .discard()
                .map_err(|_| error("pulse_capture_discard_failed"))?;
            received = true;
        }
        Ok(())
    })();
    let _ = stream.disconnect();
    if let Err(error) = &result
        && error.kind() != io::ErrorKind::Interrupted
    {
        fail(fault, error.to_string());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn timing() -> TimingInfo {
        TimingInfo {
            timestamp: UnixTs::now(),
            synchronized_clocks: 1,
            sink_usec: MicroSeconds(0),
            source_usec: MicroSeconds(20_000),
            transport_usec: MicroSeconds(5_000),
            playing: 0,
            write_index_corrupt: 0,
            write_index: 4800,
            read_index_corrupt: 0,
            read_index: 0,
            configured_sink_usec: MicroSeconds(0),
            configured_source_usec: MicroSeconds(20_000),
            since_underrun: 0,
        }
    }
    #[test]
    fn native_record_clock_counts_buffer_and_device_without_arrival_time() {
        let at = Instant::now();
        let mut info = timing();
        assert_eq!(
            capture_timestamp(&info, at),
            at.checked_sub(Duration::from_millis(120))
        );
        info.read_index = 960;
        assert_eq!(
            capture_timestamp(&info, at),
            at.checked_sub(Duration::from_millis(100))
        );
        info.read_index = info.write_index + 2;
        assert!(capture_timestamp(&info, at).is_none());
        info = timing();
        info.sink_usec = MicroSeconds(1);
        assert!(capture_timestamp(&info, at).is_none());
    }
    #[test]
    fn wall_clock_jumps_and_stale_timing_are_not_source_timestamps() {
        let anchor = ClockAnchor::new();
        let now = anchor.monotonic + Duration::from_secs(1);
        let wall = anchor.wall + MicroSeconds(1_000_000);
        assert!(anchor.map_at(wall, wall, now).is_some());
        assert!(
            anchor
                .map_at(wall, wall + MicroSeconds(20_000), now)
                .is_none()
        );
        assert!(anchor.changed_at(wall + MicroSeconds(20_000), now));
        assert!(anchor.changed_at(anchor.wall - MicroSeconds(1), now));
        assert!(!anchor.changed_at(wall, now));
        assert!(anchor.map_at(anchor.wall, wall, now).is_none());
        assert!(
            bracketed_clock(now, wall, now + Duration::from_millis(20)).is_none(),
            "a preemption between clocks is unknown timing, not a wall-clock jump"
        );
        assert!(bracketed_clock(now, wall, now + Duration::from_micros(100)).is_some());
    }
    #[test]
    fn cancelled_probe_does_not_wait_for_server_or_devices() {
        let started = Instant::now();
        let cancelled = AtomicBool::new(true);
        let error = probe("default", &cancelled)
            .err()
            .expect("cancelled probe must stop");
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(started.elapsed() < Duration::from_millis(100));
    }
    fn stream_counts() -> (usize, usize) {
        let stop = AtomicBool::new(false);
        let mut pulse = PulseContext::connect(&stop).unwrap();
        let sources = Rc::new(Cell::new(0usize));
        let count = sources.clone();
        let done = Rc::new(Cell::new(None));
        let done2 = done.clone();
        let operation = pulse
            .context
            .introspect()
            .get_source_output_info_list(move |item| match item {
                ListResult::Item(_) => count.set(count.get() + 1),
                ListResult::End => done2.set(Some(true)),
                ListResult::Error => done2.set(Some(false)),
            });
        pulse.wait_flag(&done, &stop).unwrap();
        assert_eq!(done.get(), Some(true));
        drop(operation);
        let sinks = Rc::new(Cell::new(0usize));
        let count = sinks.clone();
        let done = Rc::new(Cell::new(None));
        let done2 = done.clone();
        let operation =
            pulse
                .context
                .introspect()
                .get_sink_input_info_list(move |item| match item {
                    ListResult::Item(_) => count.set(count.get() + 1),
                    ListResult::End => done2.set(Some(true)),
                    ListResult::Error => done2.set(Some(false)),
                });
        pulse.wait_flag(&done, &stop).unwrap();
        assert_eq!(done.get(), Some(true));
        drop(operation);
        (sources.get(), sinks.get())
    }
    #[tokio::test]
    #[ignore = "requires the isolated native PulseAudio fixture from tests/native-pulse.sh"]
    async fn native_capture_has_source_timing_and_releases() {
        use super::super::{Capture, inspect};
        assert_eq!(std::env::var("JWM_EVA_PULSE_TEST").as_deref(), Ok("1"));
        let device = std::env::var("JWM_EVA_TEST_SOURCE").unwrap_or_else(|_| "default".into());
        assert_eq!(stream_counts(), (0, 0));
        let status = inspect(&device).unwrap();
        assert_eq!(status.backend, "pulseaudio");
        assert!(status.input_available && status.output_available);
        assert_eq!(
            stream_counts(),
            (0, 0),
            "backend probe must not create streams"
        );
        for unavailable in [
            "eva-source-must-not-exist".to_owned(),
            format!("{}.monitor", std::env::var("JWM_EVA_TEST_SINK").unwrap()),
        ] {
            let status = inspect(&unavailable).unwrap();
            assert_eq!(status.backend, "pulseaudio");
            assert!(!status.input_available);
            let (tx, mut rx) = mpsc::channel(4);
            let capture = Capture::start(&unavailable, tx).unwrap();
            let result = tokio::time::timeout(CONNECT_TIMEOUT * 2, rx.recv())
                .await
                .expect("invalid source should report an asynchronous error")
                .expect("capture worker must report its failure");
            assert!(
                result.is_err(),
                "missing/monitor source must never fall back to default"
            );
            capture.stop().await;
        }
        assert_eq!(stream_counts(), (0, 0));
        // module-sine-source has a fixed two-second native quantum. Accommodate
        // that entire synthetic burst in this fixture; production queue limits
        // and the Framer's two-second hard buffer bound remain unchanged.
        let (tx, mut rx) = mpsc::channel(128);
        let capture = Capture::start(&device, tx).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut frames = Vec::new();
        while frames.len() < 10 && Instant::now() < deadline {
            match rx.try_recv() {
                Ok(frame) => frames.push(frame.unwrap()),
                Err(mpsc::error::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(error) => panic!("capture stopped: {error}"),
            }
        }
        capture.check().unwrap();
        assert_eq!(stream_counts(), (1, 0));
        let stopping = Instant::now();
        capture.stop().await;
        assert!(stopping.elapsed() < Duration::from_millis(500));
        let release_deadline = Instant::now() + Duration::from_millis(500);
        while stream_counts() != (0, 0) {
            assert!(
                Instant::now() < release_deadline,
                "capture stop must disconnect its stream"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(frames.len(), 10);
        assert!(frames.iter().all(|frame| frame.pcm.len() == 960
            && frame.timestamp_verified
            && frame.level > 0.001));
        for pair in frames.windows(2) {
            assert!(
                pair[1]
                    .captured
                    .duration_since(pair[0].captured)
                    .abs_diff(Duration::from_millis(20))
                    <= CLOCK_TOLERANCE
            );
        }
    }
}
