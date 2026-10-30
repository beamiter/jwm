//! Native desktop audio. Only an activated Capture/Playback owns a stream.
//!
//! The realtime capture callback writes to a preallocated SPSC ring. Framing,
//! allocation and event delivery run on a dedicated ordinary loop thread. The
//! negotiated format is raw S16LE/24kHz/mono, so no file/container header can
//! become speech. Interrupted output is truncated only with a fresh native
//! clock; unavailable timing deletes unproven audio from conversation history.
use pipewire as pw;
use pw::{properties::properties, spa};
use std::{
    collections::VecDeque,
    io,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tokio::sync::{Notify, mpsc};

mod pulse;
mod pulse_playback;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AudioBackend {
    PipeWire,
    PulseAudio,
}

pub struct AudioStatus {
    pub backend: &'static str,
    pub input_available: bool,
    pub output_available: bool,
}

fn check_cancelled(stop: &AtomicBool) -> io::Result<()> {
    if stop.load(Ordering::Acquire) {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "audio_cancelled",
        ))
    } else {
        Ok(())
    }
}

fn selected_audio(device: &str, stop: &AtomicBool) -> io::Result<(AudioBackend, AudioStatus)> {
    check_cancelled(stop)?;
    let pulse = match pulse::probe(device, stop) {
        Ok(probe) => Some(probe),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => return Err(error),
        Err(error) if std::env::var_os("PULSE_SERVER").is_some_and(|value| !value.is_empty()) => {
            return Err(error);
        }
        Err(_) => None,
    };
    check_cancelled(stop)?;
    // A real PulseAudio server is the current desktop's audio owner. A
    // PipeWire compatibility server leaves the native PipeWire path preferred.
    if let Some(info) = pulse.as_ref().filter(|info| !info.is_pipewire_server) {
        return Ok((
            AudioBackend::PulseAudio,
            AudioStatus {
                backend: "pulseaudio",
                input_available: info.source_available,
                output_available: info.sink_available,
            },
        ));
    }
    if let Ok((input_available, output_available)) = pipewire_devices(device, stop)
        && (input_available && output_available || pulse.is_none())
    {
        return Ok((
            AudioBackend::PipeWire,
            AudioStatus {
                backend: "pipewire",
                input_available,
                output_available,
            },
        ));
    }
    check_cancelled(stop)?;
    if let Some(info) = pulse {
        return Ok((
            AudioBackend::PulseAudio,
            AudioStatus {
                backend: "pulseaudio",
                input_available: info.source_available,
                output_available: info.sink_available,
            },
        ));
    }
    Err(io::Error::other("audio_server_unavailable"))
}

pub fn inspect(device: &str) -> io::Result<AudioStatus> {
    selected_audio(device, &AtomicBool::new(false)).map(|(_, status)| status)
}

const RATE: u32 = 24_000;
const FRAME_BYTES: usize = 960; // 20 ms, mono PCM16.
const QUEUE_BYTES: usize = RATE as usize * 2 * 2; // Two seconds.
const LOOP_INTERVAL: Duration = Duration::from_millis(10);

type Fault = Arc<Mutex<Option<String>>>;
fn fail(fault: &Fault, text: impl Into<String>) {
    let mut slot = fault.lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_none() {
        *slot = Some(text.into());
    }
}
fn check(fault: &Fault) -> io::Result<()> {
    match fault.lock().unwrap_or_else(|e| e.into_inner()).as_deref() {
        Some(error) => Err(io::Error::other(error.to_owned())),
        None => Ok(()),
    }
}
fn io_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

fn ensure_runtime() -> io::Result<()> {
    // Native pw_buffer layout and the bundled libspa bindings are compiled
    // against >=1.2 headers. Check the *loaded client library*, independently
    // of the server protocol version, before dereferencing its buffer layout.
    let raw = unsafe { pw::sys::pw_get_library_version() };
    if raw.is_null() {
        return Err(io::Error::other("pipewire_runtime_version_unknown"));
    }
    let version = unsafe { std::ffi::CStr::from_ptr(raw) }.to_string_lossy();
    let mut parts = version.split('.').take(3).map(|part| {
        part.split('-')
            .next()
            .unwrap_or("")
            .parse::<u32>()
            .unwrap_or(0)
    });
    let loaded = (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    );
    if loaded < (1, 2, 0) {
        return Err(io::Error::other("pipewire_client_runtime_requires_1_2"));
    }
    Ok(())
}

struct Worker {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Worker {
    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        // The private loop has a 10ms finite poll and never waits on a full
        // channel. Joining is the resource-release boundary, not a hope that
        // a detached process will eventually close the microphone.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub struct Frame {
    /// First sample's capture timestamp, mapped from verified native audio timing.
    pub captured: Instant,
    pub timestamp_verified: bool,
    pub pcm: Vec<u8>,
    pub level: f32,
}
/// A bounded, in-memory rendering reference for local barge-in detection.
/// Native session-manager AEC remains the primary echo treatment. Correlation
/// rejects recognizably echoed Eva output when no filter is available; it
/// cannot remove room reverberation or replace an adaptive echo canceller.
#[derive(Default)]
struct RenderReference {
    samples: VecDeque<i16>,
    queued_at: Option<Instant>,
}
const ECHO_REFERENCE_SAMPLES: usize = RATE as usize * 3;
static RENDER_REFERENCE: OnceLock<Mutex<RenderReference>> = OnceLock::new();
fn rendered_reference(pcm: &[u8]) {
    let mut reference = RENDER_REFERENCE
        .get_or_init(|| Mutex::new(RenderReference::default()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if reference
        .queued_at
        .is_some_and(|at| at.elapsed() > Duration::from_millis(2500))
    {
        reference.samples.clear();
    }
    for sample in pcm.chunks_exact(2) {
        if reference.samples.len() == ECHO_REFERENCE_SAMPLES {
            reference.samples.pop_front();
        }
        reference
            .samples
            .push_back(i16::from_le_bytes([sample[0], sample[1]]));
    }
    reference.queued_at = Some(Instant::now());
}
fn echo_matches(pcm: &[u8], reference: &[i16]) -> bool {
    // Compare decimated, mean-centred waveforms across possible device and
    // acoustic delays. Normalization tolerates speaker/microphone gain and a
    // phase inversion. Deliberately require a strong match so overlapping new
    // speech can interrupt rather than being hidden by output activity alone.
    let samples: Vec<f64> = pcm
        .chunks_exact(2)
        .step_by(4)
        .map(|sample| f64::from(i16::from_le_bytes([sample[0], sample[1]])))
        .collect();
    if samples.len() < 40 || reference.len() < pcm.len() / 2 {
        return false;
    }
    let coarse = samples.iter().step_by(4).copied().collect::<Vec<_>>();
    let stats = |input: &[f64]| {
        let count = input.len() as f64;
        let sum = input.iter().sum::<f64>();
        let energy = input.iter().map(|sample| sample * sample).sum::<f64>() - sum * sum / count;
        (count, sum, energy)
    };
    let complete_stats = stats(&samples);
    let coarse_stats = stats(&coarse);
    if complete_stats.2 <= 1.0 {
        return false;
    }
    let matches = |input: &[f64],
                   (count, sum, energy): (f64, f64, f64),
                   offset: usize,
                   step: usize,
                   threshold: f64| {
        let mut other_sum = 0.0;
        let mut other_square = 0.0;
        let mut product = 0.0;
        for (index, sample) in input.iter().enumerate() {
            let other = f64::from(reference[offset + index * step]);
            other_sum += other;
            other_square += other * other;
            product += sample * other;
        }
        let other_energy = other_square - other_sum * other_sum / count;
        let covariance = product - sum * other_sum / count;
        other_energy > 1.0
            && covariance * covariance >= threshold * threshold * energy * other_energy
    };
    for offset in (0..=reference.len() - pcm.len() / 2).step_by(16) {
        // Reject most offsets using only 30 points before evaluating all 120.
        // This keeps the voice gate inexpensive even across a full render tail.
        if matches(&coarse, coarse_stats, offset, 16, 0.45)
            && matches(&samples, complete_stats, offset, 4, 0.72)
        {
            return true;
        }
    }
    false
}

/// Sustained local speech interrupts pending work without waiting for ASR or
/// a model response. PCM is used only for echo comparison and never persisted.
#[derive(Default)]
pub struct BargeInGate {
    speech_frames: u8,
    echo_detected: bool,
}
impl BargeInGate {
    pub fn reset(&mut self) {
        self.speech_frames = 0;
        self.echo_detected = false;
    }
    pub fn echo_detected(&self) -> bool {
        self.echo_detected
    }
    pub fn observe(&mut self, frame: &Frame) -> bool {
        if !frame.level.is_finite() || frame.level <= 0.025 {
            self.echo_detected = false;
            self.speech_frames = self.speech_frames.saturating_sub(2);
            return false;
        }
        let reference = RENDER_REFERENCE.get().and_then(|reference| {
            let reference = reference.lock().unwrap_or_else(|error| error.into_inner());
            reference
                .queued_at
                .filter(|at| at.elapsed() <= Duration::from_millis(2500))
                .map(|_| reference.samples.iter().copied().collect::<Vec<_>>())
        });
        self.observe_with_reference(frame, reference.as_deref().unwrap_or(&[]))
    }
    fn observe_with_reference(&mut self, frame: &Frame, reference: &[i16]) -> bool {
        self.echo_detected =
            frame.level.is_finite() && frame.level > 0.025 && echo_matches(&frame.pcm, reference);
        if !frame.level.is_finite() || frame.level <= 0.025 || self.echo_detected {
            self.speech_frames = self.speech_frames.saturating_sub(2);
            return false;
        }
        self.speech_frames = self.speech_frames.saturating_add(1);
        self.speech_frames >= 6 // six 20 ms frames reject clicks and short noise.
    }
}

pub struct Capture {
    worker: Worker,
    fault: Fault,
}
impl Capture {
    pub fn start(device: &str, tx: mpsc::Sender<io::Result<Frame>>) -> io::Result<Self> {
        if device.is_empty() || device.len() > 512 || device.contains('\0') {
            return Err(io::Error::other("invalid_input_device"));
        }
        let fault = Fault::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (fault2, stop2, device) = (fault.clone(), stop.clone(), device.to_owned());
        let thread = thread::Builder::new()
            .name("eva-capture".into())
            .spawn(move || {
                let result = (|| {
                    let (backend, status) = selected_audio(&device, &stop2)?;
                    check_cancelled(&stop2)?;
                    if !status.input_available {
                        return Err(io::Error::other("audio_capture_device_unavailable"));
                    }
                    match backend {
                        AudioBackend::PipeWire => capture_loop(&device, &tx, &stop2, &fault2),
                        AudioBackend::PulseAudio => {
                            pulse::capture_loop(&device, &tx, &stop2, &fault2)
                        }
                    }
                })();
                if let Err(error) = result
                    && !stop2.load(Ordering::Acquire)
                {
                    fail(&fault2, error.to_string());
                    let _ = tx.try_send(Err(error));
                }
            })?;
        Ok(Self {
            worker: Worker {
                stop,
                thread: Some(thread),
            },
            fault,
        })
    }
    pub fn check(&self) -> io::Result<()> {
        check(&self.fault)
    }
    pub async fn stop(mut self) {
        self.worker.shutdown();
    }
}

/// Check the native server roundtrip without creating a capture/playback node.
pub fn probe() -> io::Result<()> {
    let status = inspect("default")?;
    if !status.input_available || !status.output_available {
        return Err(io::Error::other("audio_device_unavailable"));
    }
    Ok(())
}

fn pipewire_devices(device: &str, stop: &AtomicBool) -> io::Result<(bool, bool)> {
    check_cancelled(stop)?;
    ensure_runtime()?;
    use std::{cell::Cell, rc::Rc};
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(io_error)?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(io_error)?;
    let core = context.connect_rc(None).map_err(io_error)?;
    let registry = core.get_registry().map_err(io_error)?;
    let done = Rc::new(Cell::new(false));
    let flag = done.clone();
    let input = Rc::new(Cell::new(false));
    let output = Rc::new(Cell::new(false));
    let (input_seen, output_seen) = (input.clone(), output.clone());
    let device = device.to_owned();
    let _globals = registry
        .add_listener_local()
        .global(move |global| {
            let Some(props) = global.props.as_ref() else {
                return;
            };
            match props.get("media.class") {
                Some("Audio/Source") => {
                    let name = props.get("node.name").unwrap_or("");
                    if !name.ends_with(".monitor") && (device == "default" || name == device) {
                        input_seen.set(true);
                    }
                }
                Some("Audio/Sink") => output_seen.set(true),
                _ => {}
            }
        })
        .register();
    // The server's initial core.info is the handshake acknowledgement. Do
    // not attach pipewire-rs's core.done adapter: older servers can send a
    // legitimate seq=0 which that adapter converts to AsyncSeq by panicking.
    let _listener = core
        .add_listener_local()
        .info(move |_| flag.set(true))
        .register();
    let until = Instant::now() + Duration::from_millis(500);
    while !done.get() || !input.get() || !output.get() {
        check_cancelled(stop)?;
        if Instant::now() >= until {
            if !done.get() {
                return Err(io::Error::other("pipewire_probe_timeout"));
            }
            break;
        }
        mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(LOOP_INTERVAL));
    }
    Ok((input.get(), output.get()))
}

fn props(capture: bool, device: &str) -> pw::properties::PropertiesBox {
    let mut p = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => if capture {"Capture"} else {"Playback"},
        *pw::keys::MEDIA_ROLE => "Communication",
        *pw::keys::NODE_NAME => if capture {"jwm-eva.capture"} else {"jwm-eva.playback"},
        *pw::keys::NODE_DONT_RECONNECT => "true",
        *pw::keys::NODE_LATENCY => "480/24000",
        "stream.capture.sink" => "false",
        // Ask the session manager for its communication echo canceller. This
        // is a preference, not evidence that an AEC filter was negotiated.
        "filter.want" => "echo-cancel",
    };
    if device != "default" {
        p.insert(*pw::keys::TARGET_OBJECT, device);
        p.insert("node.dont-fallback", "true");
        p.insert("node.dont-move", "true");
    }
    p
}
fn format_pod() -> io::Result<Vec<u8>> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::S16LE);
    info.set_rate(RATE);
    info.set_channels(1);
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
            id: spa::param::ParamType::EnumFormat.as_raw(),
            properties: info.into(),
        }),
    )
    .map(|(cursor, _)| cursor.into_inner())
    .map_err(io_error)
}
fn header_pod() -> io::Result<Vec<u8>> {
    use spa::pod::{Object, Property, Value};
    let object = Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamMeta,
        id: spa::param::ParamType::Meta.as_raw(),
        properties: vec![
            Property::new(
                spa::sys::SPA_PARAM_META_type,
                Value::Id(spa::utils::Id(spa::sys::SPA_META_Header)),
            ),
            Property::new(
                spa::sys::SPA_PARAM_META_size,
                Value::Int(std::mem::size_of::<spa::sys::spa_meta_header>() as i32),
            ),
        ],
    };
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(object),
    )
    .map(|(cursor, _)| cursor.into_inner())
    .map_err(io_error)
}

fn validate_format(id: u32, param: Option<&spa::pod::Pod>, fault: &Fault) {
    if id != spa::param::ParamType::Format.as_raw() {
        return;
    }
    let Some(param) = param else {
        return;
    };
    let mut info = spa::param::audio::AudioInfoRaw::new();
    if info.parse(param).is_err()
        || info.format() != spa::param::audio::AudioFormat::S16LE
        || info.rate() != RATE
        || info.channels() != 1
    {
        fail(fault, "audio_format_mismatch");
    }
}

/// Convert a CLOCK_MONOTONIC presentation timestamp to Rust Instant. Reject
/// timestamps far away from this stream instead of relabelling delayed audio
/// with its callback arrival time.
fn monotonic_instant(timestamp: i64) -> Option<Instant> {
    if timestamp <= 0 {
        return None;
    }
    let mut clock = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid writable timespec, standard monotonic clock.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut clock) } != 0 {
        return None;
    }
    let now_ns = (clock.tv_sec as i128) * 1_000_000_000 + clock.tv_nsec as i128;
    let age = now_ns - timestamp as i128;
    // A driver may schedule the current cycle slightly ahead of callback
    // delivery (especially a synthetic source). Preserve that native future
    // timestamp instead of substituting callback arrival time.
    if !(-100_000_000..=5_000_000_000).contains(&age) {
        return None;
    }
    if age >= 0 {
        Instant::now().checked_sub(Duration::from_nanos(age as u64))
    } else {
        Instant::now().checked_add(Duration::from_nanos((-age) as u64))
    }
}

#[derive(Default)]
struct Framer {
    bytes: VecDeque<u8>,
    first: Option<Instant>,
    verified: bool,
    seen: bool,
    expected_next: Option<Instant>,
}
impl Framer {
    fn push(
        &mut self,
        bytes: &[u8],
        timestamp: Option<Instant>,
        verified: bool,
        tx: &mpsc::Sender<io::Result<Frame>>,
    ) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        if !bytes.len().is_multiple_of(2)
            || self.bytes.len().saturating_add(bytes.len()) > QUEUE_BYTES
        {
            return Err(io::Error::other("audio_buffer_overflow_or_bad_format"));
        }
        if verified {
            if let (Some(actual), Some(expected)) = (timestamp, self.expected_next) {
                let drift = actual
                    .checked_duration_since(expected)
                    .unwrap_or_else(|| expected.duration_since(actual));
                if drift > Duration::from_millis(5) {
                    let direction = if actual >= expected { "late" } else { "early" };
                    return Err(io::Error::other(format!(
                        "audio_timestamp_discontinuity: drift_us={} direction={direction}",
                        drift.as_micros()
                    )));
                }
            }
            self.expected_next = timestamp.and_then(|at| {
                at.checked_add(Duration::from_nanos(
                    (bytes.len() as u64) * 1_000_000_000 / (RATE as u64 * 2),
                ))
            });
        } else {
            self.expected_next = None;
        }
        if self.bytes.is_empty() {
            self.first = Some(timestamp.unwrap_or_else(Instant::now));
            self.verified = verified;
        } else {
            self.verified &= verified;
        }
        self.bytes.extend(bytes);
        while self.bytes.len() >= FRAME_BYTES {
            // Leave one slot for an immediately visible terminal error.
            if tx.capacity() <= 1 {
                return Err(io::Error::other("audio_buffer_overflow"));
            }
            let pcm: Vec<u8> = self.bytes.drain(..FRAME_BYTES).collect();
            let captured = self.first.unwrap_or_else(Instant::now);
            let frame = Frame {
                captured,
                timestamp_verified: self.verified,
                level: rms(&pcm),
                pcm,
            };

            tx.try_send(Ok(frame))
                .map_err(|_| io::Error::other("audio_buffer_overflow"))?;
            // Every older partial chunk has now been consumed. All remaining
            // bytes belong to this push, so re-anchor to its own native time
            // and trust instead of carrying an unknown startup timestamp.
            let consumed = bytes.len() - self.bytes.len();
            self.first = match timestamp {
                Some(start) => start.checked_add(Duration::from_nanos(
                    (consumed as u64) * 1_000_000_000 / (RATE as u64 * 2),
                )),
                None => captured.checked_add(Duration::from_millis(20)),
            };
            self.verified = verified && self.first.is_some();
        }
        self.seen = true;
        Ok(())
    }
}
/// Recover the first source sample from the native buffer's graph-cycle
/// timestamp, input device/filter delay, and resampler buffered frames. The
/// per-buffer timestamp already accounts for older buffers waiting in the
/// application queue, unlike callback arrival time or the latest graph now.
fn capture_cycle_time(
    cycle: i64,
    graph_now: i64,
    delay: i64,
    num: u32,
    denom: u32,
    buffered: u64,
) -> Option<Instant> {
    if cycle <= 0
        || graph_now <= 0
        || num == 0
        || denom == 0
        || (cycle as i128 - graph_now as i128).unsigned_abs() > 2_000_000_000
    {
        return None;
    }
    monotonic_instant(graph_now)?; // Reject stale or foreign-domain graph clocks.
    let device_ns = (delay.max(0) as u128)
        .saturating_mul(num as u128)
        .saturating_mul(1_000_000_000)
        .div_ceil(denom as u128);
    let converter_ns = (buffered as u128)
        .saturating_mul(1_000_000_000)
        .div_ceil(RATE as u128);
    let sample =
        (cycle as i128).checked_sub(device_ns.checked_add(converter_ns)?.try_into().ok()?)?;
    monotonic_instant(sample.try_into().ok()?)
}

const NATIVE_CHUNK_BYTES: usize = 16_384;
#[derive(Clone, Copy, Default)]
struct CaptureTiming {
    header_pts: i64,
    cycle: i64,
    now: i64,
    delay: i64,
    rate_num: u32,
    rate_denom: u32,
    buffered: u64,
    quantum_frames: u64,
    ticks: u64,
}
/// The cycle stamps buffer delivery, while size describes the samples for
/// this graph quantum. Startup/resampling can put more than one quantum in
/// a buffer, so its first sample precedes the ordinary cycle anchor.
fn capture_buffer_time(timing: CaptureTiming, samples: u64) -> Option<Instant> {
    let maximum_samples = (NATIVE_CHUNK_BYTES / 2) as u64;
    if samples == 0
        || samples > maximum_samples
        || timing.quantum_frames == 0
        || timing.quantum_frames > maximum_samples
        // A shorter buffer's position inside the quantum is unspecified.
        || samples < timing.quantum_frames
    {
        return None;
    }
    let anchor = capture_cycle_time(
        timing.cycle,
        timing.now,
        timing.delay,
        timing.rate_num,
        timing.rate_denom,
        timing.buffered,
    )?;
    let extra = samples - timing.quantum_frames;
    let offset =
        Duration::from_nanos((extra as u128 * 1_000_000_000).div_ceil(RATE as u128) as u64);
    let captured = anchor.checked_sub(offset)?;
    let now = Instant::now();
    if captured > now + Duration::from_millis(100)
        || now.saturating_duration_since(captured) > Duration::from_secs(5)
    {
        return None;
    }
    Some(captured)
}
#[derive(Clone, Copy)]
struct NativeChunk {
    bytes: [u8; NATIVE_CHUNK_BYTES],
    len: usize,
    timestamp: Option<Instant>,
    timing: CaptureTiming,
}
/// One RT producer and one ordinary-loop consumer. All slots are allocated
/// before stream activation; publish/read acquire-release pairs own each slot.
struct CaptureRing {
    slots: Box<[std::cell::UnsafeCell<std::mem::MaybeUninit<NativeChunk>>]>,
    written: AtomicUsize,
    read: AtomicUsize,
}
// SAFETY: exactly one producer writes a slot only after the consumer released
// it, and exactly one consumer reads only after the producer published it.
unsafe impl Sync for CaptureRing {}
impl CaptureRing {
    fn new() -> Self {
        Self {
            slots: (0..4)
                .map(|_| {
                    // Allocate and touch every page before the RT callback.
                    std::cell::UnsafeCell::new(std::mem::MaybeUninit::new(NativeChunk {
                        bytes: [0; NATIVE_CHUNK_BYTES],
                        len: 0,
                        timestamp: None,
                        timing: CaptureTiming::default(),
                    }))
                })
                .collect(),
            written: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
        }
    }
    fn push(&self, chunk: NativeChunk) -> bool {
        let write = self.written.load(Ordering::Relaxed);
        let read = self.read.load(Ordering::Acquire);
        if write.wrapping_sub(read) >= self.slots.len() {
            return false;
        }
        unsafe { (*self.slots[write % self.slots.len()].get()).write(chunk) };
        self.written.store(write.wrapping_add(1), Ordering::Release);
        true
    }
    fn pop(&self) -> Option<NativeChunk> {
        let read = self.read.load(Ordering::Relaxed);
        let write = self.written.load(Ordering::Acquire);
        if read == write {
            return None;
        }
        let chunk = unsafe { (*self.slots[read % self.slots.len()].get()).assume_init_read() };
        self.read.store(read.wrapping_add(1), Ordering::Release);
        Some(chunk)
    }
}

fn capture_loop(
    device: &str,
    tx: &mpsc::Sender<io::Result<Frame>>,
    stop: &AtomicBool,
    fault: &Fault,
) -> io::Result<()> {
    check_cancelled(stop)?;
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(io_error)?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(io_error)?;
    let core = context.connect_rc(None).map_err(io_error)?;
    let stream = pw::stream::StreamBox::new(&core, "Eva microphone", props(true, device))
        .map_err(io_error)?;
    let ring = Arc::new(CaptureRing::new());
    let process_ring = ring.clone();
    let rt_fault = Arc::new(AtomicU8::new(0));
    let process_fault = rt_fault.clone();
    let (state_fault, param_fault) = (fault.clone(), fault.clone());
    let mut seen = false;
    let mut framer = Framer::default();
    let mut previous_timing: Option<(CaptureTiming, usize)> = None;
    let _listener = stream
        .add_local_listener_with_user_data(())
        .state_changed(move |_, _, old, new| {
            if matches!(new, pw::stream::StreamState::Error(_))
                || (matches!(new, pw::stream::StreamState::Unconnected)
                    && !matches!(old, pw::stream::StreamState::Unconnected))
            {
                fail(&state_fault, "audio_unavailable");
            }
        })
        .param_changed(move |stream, _, id, param| {
            validate_format(id, param, &param_fault);
            if id == spa::param::ParamType::Format.as_raw() && param.is_some() {
                let result = header_pod().and_then(|bytes| {
                    let pod = spa::pod::Pod::from_bytes(&bytes)
                        .ok_or_else(|| io::Error::other("invalid_header_param"))?;
                    stream.update_params(&mut [pod]).map_err(io_error)
                });
                if result.is_err() {
                    fail(&param_fault, "capture_timestamp_negotiation_failed");
                }
            }
        })
        .process(move |stream, _| {
            // RT callback: no allocation, blocking locks, channel send or I/O.
            while let Some(mut buffer) = StreamBuffer::dequeue(stream) {
                let header = buffer.header();
                if header.is_some_and(|(_, flags)| {
                    flags & spa::sys::SPA_META_HEADER_FLAG_CORRUPTED != 0
                        || (seen && flags & spa::sys::SPA_META_HEADER_FLAG_DISCONT != 0)
                }) {
                    process_fault.store(2, Ordering::Release);
                    return;
                }
                let graph = stream.time().ok();
                let timing = CaptureTiming {
                    header_pts: header.map_or(0, |(pts, _)| pts),
                    cycle: buffer.cycle_time(),
                    now: graph.as_ref().map_or(0, |time| time.now()),
                    delay: graph.as_ref().map_or(0, |time| time.delay()),
                    rate_num: graph.as_ref().map_or(0, |time| time.rate().num),
                    rate_denom: graph.as_ref().map_or(0, |time| time.rate().denom),
                    buffered: graph.as_ref().map_or(0, |time| time.buffered()),
                    quantum_frames: graph.as_ref().map_or(0, |time| time.as_raw().size),
                    ticks: graph.as_ref().map_or(0, |time| time.ticks()),
                };
                let Some(data) = buffer.data() else {
                    process_fault.store(1, Ordering::Release);
                    return;
                };
                let (offset, len) = (data.chunk().offset() as usize, data.chunk().size() as usize);
                if len == 0 {
                    continue;
                }
                if len > NATIVE_CHUNK_BYTES || !len.is_multiple_of(2) {
                    process_fault.store(1, Ordering::Release);
                    return;
                }
                let Some(bytes) = data.data().and_then(|bytes| {
                    offset
                        .checked_add(len)
                        .and_then(|end| bytes.get(offset..end))
                }) else {
                    process_fault.store(1, Ordering::Release);
                    return;
                };
                let timestamp = header
                    .and_then(|(pts, _)| monotonic_instant(pts))
                    .or_else(|| capture_buffer_time(timing, (len / 2) as u64));
                let mut chunk = NativeChunk {
                    bytes: [0; NATIVE_CHUNK_BYTES],
                    len,
                    timestamp,
                    timing,
                };
                chunk.bytes[..len].copy_from_slice(bytes);
                buffer.frames((len / 2) as u64);
                if !process_ring.push(chunk) {
                    process_fault.store(3, Ordering::Release);
                    return;
                }
                seen = true;
            }
        })
        .register()
        .map_err(io_error)?;
    let pod = format_pod()?;
    let mut params =
        [spa::pod::Pod::from_bytes(&pod)
            .ok_or_else(|| io::Error::other("audio_format_invalid"))?];
    check_cancelled(stop)?;
    stream
        .connect(
            spa::utils::Direction::Input,
            None,
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(io_error)?;
    while !stop.load(Ordering::Acquire) && !tx.is_closed() {
        mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(LOOP_INTERVAL));
        match rt_fault.load(Ordering::Acquire) {
            0 => {}
            1 => return Err(io::Error::other("audio_bad_buffer")),
            2 => return Err(io::Error::other("audio_discontinuity")),
            _ => return Err(io::Error::other("audio_buffer_overflow")),
        }
        check(fault)?;
        while let Some(chunk) = ring.pop() {
            framer.push(
                &chunk.bytes[..chunk.len],
                chunk.timestamp,
                chunk.timestamp.is_some(),
                tx,
            ).map_err(|error| {
                if error.to_string().starts_with("audio_timestamp_discontinuity") {
                    let t = chunk.timing;
                    io::Error::other(format!(
                            "{error} header_pts={} cycle={} graph_now={} graph_delay={} graph_rate={}/{} buffered={} quantum_frames={} ticks={} chunk_bytes={} previous={:?}",
                            t.header_pts, t.cycle, t.now, t.delay, t.rate_num, t.rate_denom, t.buffered, t.quantum_frames, t.ticks, chunk.len,
                            previous_timing.map(|(p, len)| (p.header_pts, p.cycle, p.now, p.delay, p.rate_num, p.rate_denom, p.buffered, p.quantum_frames, p.ticks, len))
                    ))
                } else {
                    error
                }
            })?;
            previous_timing = Some((chunk.timing, chunk.len));
        }
    }
    let _ = stream.set_active(false);
    let _ = stream.disconnect();
    Ok(())
}

pub fn rms(pcm: &[u8]) -> f32 {
    let mut sum = 0.0f64;
    let mut n = 0;
    for sample in pcm.chunks_exact(2) {
        let v = i16::from_le_bytes([sample[0], sample[1]]) as f64 / 32768.0;
        sum += v * v;
        n += 1;
    }
    if n == 0 {
        0.0
    } else {
        (sum / n as f64).sqrt() as f32
    }
}

/// A buffer owned by one native stream. pipewire-rs's safe Buffer does not
/// expose pw_buffer.size; this small RAII adapter sets its unit to *frames*
/// before queueing, making pw_time.queued compatible with buffered/delay.
struct StreamBuffer<'a> {
    raw: std::ptr::NonNull<pw::sys::pw_buffer>,
    stream: &'a pw::stream::Stream,
}
impl<'a> StreamBuffer<'a> {
    fn dequeue(stream: &'a pw::stream::Stream) -> Option<Self> {
        // SAFETY: the null result is checked and Drop returns the buffer to
        // exactly this stream, after all borrowed mapped data has expired.
        let raw = std::ptr::NonNull::new(unsafe { stream.dequeue_raw_buffer() })?;
        Some(Self { raw, stream })
    }
    fn requested(&self) -> usize {
        unsafe { self.raw.as_ref().requested as usize }
    }
    fn cycle_time(&self) -> i64 {
        unsafe { self.raw.as_ref().time as i64 }
    }
    fn header(&self) -> Option<(i64, u32)> {
        let buffer = unsafe { self.raw.as_ref().buffer };
        if buffer.is_null() {
            return None;
        }
        let header = unsafe {
            spa::sys::spa_buffer_find_meta_data(
                buffer,
                spa::sys::SPA_META_Header,
                std::mem::size_of::<spa::sys::spa_meta_header>(),
            )
            .cast::<spa::sys::spa_meta_header>()
            .as_ref()
        }?;
        Some((header.pts, header.flags))
    }

    fn data(&mut self) -> Option<&mut spa::buffer::Data> {
        // SAFETY: PipeWire owns this dequeued spa_buffer and its mapped data.
        // Data is repr(transparent); the returned borrow cannot outlive self.
        let buffer = unsafe { self.raw.as_ref().buffer.as_mut() }?;
        if buffer.n_datas == 0 || buffer.datas.is_null() {
            return None;
        }
        Some(unsafe { &mut *(buffer.datas as *mut spa::buffer::Data) })
    }
    fn frames(&mut self, frames: u64) {
        unsafe {
            self.raw.as_mut().size = frames;
        }
    }
}
impl Drop for StreamBuffer<'_> {
    fn drop(&mut self) {
        unsafe { self.stream.queue_raw_buffer(self.raw.as_ptr()) };
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryEdit {
    Delete {
        item_id: String,
    },
    Truncate {
        item_id: String,
        content_index: u32,
        audio_end_ms: u64,
    },
}
#[derive(Clone)]
pub struct PlaybackHistory {
    state: Arc<Mutex<Output>>,
}
#[derive(Debug)]
struct Segment {
    item_id: String,
    content_index: u32,
    start: u64,
    end: u64,
}
#[derive(Default)]
struct Output {
    queue: VecDeque<u8>,
    ending: bool,
    drain_started: bool,
    drained: bool,
    submitted: u64,
    received_samples: u64,
    segments: Vec<Segment>,
    clock: Option<(Instant, u64)>,
    telemetry: Option<serde_json::Value>,
    first_played: bool,
}
/// Claim only a fresh native played position. Moving the prefiltered labels
/// out of Output keeps JSON construction and logging outside the audio lock.
fn claim_first_played(
    state: &mut Output,
    samples: Option<u64>,
) -> Option<(serde_json::Value, u64)> {
    let samples = samples.filter(|samples| *samples > 0)?;
    if state.first_played {
        return None;
    }
    let labels = state.telemetry.take()?;
    state.first_played = true;
    Some((labels, samples))
}

/// Official pw_time formula. All queued/buffered values and our counters are
/// source frames; delay is in graph rate ticks. Round backlog up, so the
/// reported played offset never retains an unplayed source sample.
fn played_floor(
    submitted: u64,
    queued: u64,
    buffered: u64,
    delay: i64,
    num: u32,
    denom: u32,
    elapsed_ns: i64,
) -> Option<u64> {
    if num == 0 || denom == 0 {
        return None;
    }
    let delay_ns = (delay.max(0) as u128)
        .saturating_mul(num as u128)
        .saturating_mul(1_000_000_000)
        .div_ceil(denom as u128);
    let remaining_ns = (delay_ns.min(i128::MAX as u128) as i128)
        .saturating_sub(elapsed_ns as i128)
        .max(0) as u128;
    let device_frames = remaining_ns
        .saturating_mul(RATE as u128)
        .div_ceil(1_000_000_000);
    let backlog = queued as u128 + buffered as u128 + device_frames;
    Some(submitted.saturating_sub(backlog.min(u64::MAX as u128) as u64))
}
fn clock_snapshot(stream: &pw::stream::Stream, submitted: u64) -> Option<u64> {
    let time = stream.time().ok()?;
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } != 0 {
        return None;
    }
    let now_ns = now.tv_sec as i128 * 1_000_000_000 + now.tv_nsec as i128;
    let elapsed = now_ns - time.now() as i128;
    // Stale/foreign-domain clock reports do not authorize history retention.
    if !(-100_000_000..=300_000_000).contains(&elapsed) {
        return None;
    }
    let rate = time.rate();
    played_floor(
        submitted,
        time.queued(),
        time.buffered(),
        time.delay(),
        rate.num,
        rate.denom,
        elapsed as i64,
    )
}
fn edits(state: &Output, now: Instant) -> Vec<HistoryEdit> {
    if state.drained {
        return Vec::new();
    }
    let played = state
        .clock
        .filter(|(at, _)| now.saturating_duration_since(*at) <= Duration::from_millis(100))
        .map(|(_, samples)| samples);
    state
        .segments
        .iter()
        .filter_map(|segment| {
            let Some(played) = played else {
                return Some(HistoryEdit::Delete {
                    item_id: segment.item_id.clone(),
                });
            };
            if played >= segment.end {
                None
            } else if played <= segment.start {
                Some(HistoryEdit::Delete {
                    item_id: segment.item_id.clone(),
                })
            } else {
                let milliseconds = (played - segment.start) * 1000 / RATE as u64;
                if milliseconds == 0 {
                    Some(HistoryEdit::Delete {
                        item_id: segment.item_id.clone(),
                    })
                } else {
                    Some(HistoryEdit::Truncate {
                        item_id: segment.item_id.clone(),
                        content_index: segment.content_index,
                        audio_end_ms: milliseconds,
                    })
                }
            }
        })
        .collect()
}
impl PlaybackHistory {
    pub fn items(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .segments
            .iter()
            .map(|s| s.item_id.clone())
            .collect()
    }
    pub fn edits(&self) -> Vec<HistoryEdit> {
        edits(
            &self.state.lock().unwrap_or_else(|e| e.into_inner()),
            Instant::now(),
        )
    }
    pub fn played_samples(&self) -> Option<u64> {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.drained {
            Some(s.submitted / 2)
        } else {
            s.clock
                .filter(|(at, _)| at.elapsed() <= Duration::from_millis(100))
                .map(|(_, p)| p)
        }
    }
}
pub struct Playback {
    worker: Worker,
    state: Arc<Mutex<Output>>,
    fault: Fault,
    notify: Arc<Notify>,
}
impl Playback {
    pub fn start() -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(Output::default()));
        let fault = Fault::default();
        let notify = Arc::new(Notify::new());
        let (stop2, state2, fault2, notify2) =
            (stop.clone(), state.clone(), fault.clone(), notify.clone());
        let thread = thread::Builder::new()
            .name("eva-playback".into())
            .spawn(move || {
                let result = (|| {
                    let (backend, status) = selected_audio("default", &stop2)?;
                    check_cancelled(&stop2)?;
                    if !status.output_available {
                        return Err(io::Error::other("audio_playback_device_unavailable"));
                    }
                    match backend {
                        AudioBackend::PipeWire => playback_loop(&stop2, &state2, &fault2, &notify2),
                        AudioBackend::PulseAudio => {
                            pulse_playback::playback_loop(&stop2, &state2, &fault2, &notify2)
                        }
                    }
                })();
                if let Err(error) = result
                    && !stop2.load(Ordering::Acquire)
                {
                    fail(&fault2, error.to_string());
                }
                notify2.notify_waiters();
            })?;
        Ok(Self {
            worker: Worker {
                stop,
                thread: Some(thread),
            },
            state,
            fault,
            notify,
        })
    }
    /// Associate the actual-playback milestone with this feedback turn.
    /// Filter before taking the queue lock; no caller-supplied private fields
    /// can enter the stored telemetry labels or a later emitted event.
    pub fn set_telemetry(&mut self, labels: serde_json::Value) {
        let labels = crate::telemetry::fields(&labels);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.first_played {
            state.telemetry = Some(labels);
        }
    }
    pub async fn write(&mut self, pcm: &[u8]) -> io::Result<()> {
        self.enqueue(None, pcm).await
    }
    pub async fn write_item(
        &mut self,
        item_id: &str,
        content_index: u32,
        pcm: &[u8],
    ) -> io::Result<()> {
        if item_id.is_empty() || item_id.len() > 256 || item_id.chars().any(char::is_control) {
            return Err(io::Error::other("invalid_audio_item"));
        }
        self.enqueue(Some((item_id, content_index)), pcm).await
    }
    async fn enqueue(&mut self, item: Option<(&str, u32)>, pcm: &[u8]) -> io::Result<()> {
        if !pcm.len().is_multiple_of(2) || pcm.is_empty() {
            return Err(io::Error::other("invalid_playback_frame"));
        }
        if pcm.len() > QUEUE_BYTES {
            return Err(io::Error::other("playback_frame_too_large"));
        }
        // Realtime audio can arrive faster than it is played. Apply bounded,
        // cancellable backpressure instead of discarding a valid reply when
        // the two-second application/native backlog is full. Missing timing
        // or a stalled device must still fail within a finite deadline.
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if self.enqueue_ready(item, pcm)? {
                    return Ok(());
                }
                tokio::time::sleep(LOOP_INTERVAL).await;
            }
        })
        .await
        .map_err(|_| io::Error::other("playback_queue_stalled"))?
    }
    fn enqueue_ready(&mut self, item: Option<(&str, u32)>, pcm: &[u8]) -> io::Result<bool> {
        check(&self.fault)?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.ending {
            return Err(io::Error::other("playback_already_ending"));
        }
        let played = state
            .clock
            .filter(|(at, _)| at.elapsed() <= Duration::from_millis(100))
            .map_or(0, |(_, p)| p);
        let pending = state
            .received_samples
            .saturating_sub(played)
            .saturating_add((pcm.len() / 2) as u64);
        if state.queue.len().saturating_add(pcm.len()) > QUEUE_BYTES || pending > RATE as u64 * 2 {
            return Ok(false);
        }
        let end = state
            .received_samples
            .saturating_add((pcm.len() / 2) as u64);
        if let Some((id, index)) = item {
            if let Some(last) = state
                .segments
                .last_mut()
                .filter(|s| s.item_id == id && s.content_index == index)
            {
                last.end = end;
            } else {
                if state.segments.len() >= 32 || state.segments.iter().any(|s| s.item_id == id) {
                    return Err(io::Error::other("playback_item_interleaving_or_limit"));
                }
                let start = state.received_samples;
                state.segments.push(Segment {
                    item_id: id.into(),
                    content_index: index,
                    start,
                    end,
                });
            }
        }
        state.received_samples = end;
        state.queue.extend(pcm);
        drop(state);
        rendered_reference(pcm);
        Ok(true)
    }
    pub fn history(&self) -> PlaybackHistory {
        PlaybackHistory {
            state: self.state.clone(),
        }
    }
    /// Trusted recent PipeWire snapshot, never inferred from bytes submitted.
    /// None asks the caller to delete the item instead of inventing an offset.
    pub fn played_samples(&self) -> Option<u64> {
        self.history().played_samples()
    }
    pub async fn finish(mut self) -> io::Result<()> {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).ending = true;
        let done = async {
            loop {
                let notification = self.notify.notified();
                check(&self.fault)?;
                if self.state.lock().unwrap_or_else(|e| e.into_inner()).drained {
                    return io::Result::Ok(());
                }
                notification.await;
            }
        };
        let result = tokio::time::timeout(Duration::from_secs(8), done)
            .await
            .map_err(|_| io::Error::other("playback_drain_timeout"))?;
        self.worker.shutdown();
        result
    }
}
impl Drop for Playback {
    fn drop(&mut self) {
        self.worker.shutdown();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        for byte in &mut state.queue {
            *byte = 0;
        }
        state.queue.clear();
        state.queue.shrink_to_fit();
        if let Some(reference) = RENDER_REFERENCE.get() {
            let mut reference = reference.lock().unwrap_or_else(|error| error.into_inner());
            for sample in &mut reference.samples {
                *sample = 0;
            }
            reference.samples.clear();
            reference.queued_at = None;
        }
    }
}
fn playback_loop(
    stop: &AtomicBool,
    state: &Arc<Mutex<Output>>,
    fault: &Fault,
    notify: &Arc<Notify>,
) -> io::Result<()> {
    check_cancelled(stop)?;
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(io_error)?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(io_error)?;
    let core = context.connect_rc(None).map_err(io_error)?;
    let stream = pw::stream::StreamBox::new(&core, "Eva feedback", props(false, "default"))
        .map_err(io_error)?;
    let (state_fault, param_fault, process_state, drain_state, drain_notify) = (
        fault.clone(),
        fault.clone(),
        state.clone(),
        state.clone(),
        notify.clone(),
    );
    let _listener = stream
        .add_local_listener_with_user_data(())
        .state_changed(move |_, _, old, new| {
            if matches!(new, pw::stream::StreamState::Error(_))
                || (matches!(new, pw::stream::StreamState::Unconnected)
                    && !matches!(old, pw::stream::StreamState::Unconnected))
            {
                fail(&state_fault, "playback_unavailable");
            }
        })
        .param_changed(move |_, _, id, param| validate_format(id, param, &param_fault))
        .drained(move |_, _| {
            let mut state = drain_state.lock().unwrap_or_else(|e| e.into_inner());
            if state.drain_started && state.ending && state.queue.is_empty() {
                state.drained = true;
                drain_notify.notify_waiters();
            }
        })
        .process(move |stream, _| {
            let Some(mut buffer) = StreamBuffer::dequeue(stream) else {
                return;
            };
            let requested = buffer.requested();
            let Some(data) = buffer.data() else {
                return;
            };
            let Ok(mut state) = process_state.try_lock() else {
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = 2;
                *chunk.size_mut() = 0;
                buffer.frames(0);
                return;
            };
            let size = if let Some(bytes) = data.data() {
                let wanted = if requested > 0 {
                    requested.saturating_mul(2)
                } else {
                    bytes.len()
                };
                let size = (bytes.len().min(wanted).min(state.queue.len())) & !1;
                for byte in &mut bytes[..size] {
                    *byte = state.queue.pop_front().unwrap();
                }
                state.submitted += size as u64;
                size
            } else {
                0
            };
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = 2;
            *chunk.size_mut() = size as u32;
            buffer.frames((size / 2) as u64);
            // Publish queued native frames before exposing submitted to the
            // ordinary-loop clock snapshot/drain decision.
            drop(buffer);
            drop(state);
        })
        .register()
        .map_err(io_error)?;
    let pod = format_pod()?;
    let mut params =
        [spa::pod::Pod::from_bytes(&pod)
            .ok_or_else(|| io::Error::other("audio_format_invalid"))?];
    check_cancelled(stop)?;
    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(io_error)?;
    while !stop.load(Ordering::Acquire) {
        mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(LOOP_INTERVAL));
        check(fault)?;
        let first_played = {
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            let samples = clock_snapshot(&stream, s.submitted / 2);
            if let Some(samples) = samples {
                let previous = s.clock.map_or(0, |(_, p)| p);
                s.clock = Some((Instant::now(), previous.max(samples)));
            }
            claim_first_played(&mut s, samples)
        };
        if let Some((labels, samples)) = first_played {
            // Ordinary loop thread, after releasing Output. The RT process
            // callback only moves PCM and never allocates or logs telemetry.
            let mut fields = labels.as_object().cloned().unwrap_or_default();
            fields.insert("result".into(), serde_json::json!("played"));
            fields.insert("played_samples".into(), serde_json::json!(samples));
            crate::telemetry::event(
                "first_feedback_playback",
                &serde_json::Value::Object(fields),
            );
        }
        let drain = {
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            let drain = s.ending && s.queue.is_empty() && !s.drain_started;
            if drain {
                s.drain_started = true;
            }
            drain
        };
        if drain {
            stream.flush(true).map_err(io_error)?;
        }
        if state.lock().unwrap_or_else(|e| e.into_inner()).drained {
            break;
        }
    }
    let _ = stream.set_active(false);
    let _ = stream.flush(false);
    let _ = stream.disconnect();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn verified_chunks_recover_after_an_unaligned_unknown_startup_prefix() {
        let source = Instant::now();
        let (tx, mut rx) = mpsc::channel(8);
        let mut framer = Framer::default();
        framer.push(&[0; 196], None, false, &tx).unwrap();
        for index in 0..12 {
            let samples = 98 + index * 128;
            let timestamp =
                source + Duration::from_nanos(samples * 1_000_000_000 / u64::from(RATE));
            framer.push(&[0; 256], Some(timestamp), true, &tx).unwrap();
        }
        let first = rx.try_recv().unwrap().unwrap();
        let second = rx.try_recv().unwrap().unwrap();
        let third = rx.try_recv().unwrap().unwrap();
        assert!(!first.timestamp_verified);
        assert!(second.timestamp_verified && third.timestamp_verified);
        assert!(
            second
                .captured
                .duration_since(source)
                .abs_diff(Duration::from_millis(20))
                <= Duration::from_nanos(2)
        );
        assert!(
            third
                .captured
                .duration_since(second.captured)
                .abs_diff(Duration::from_millis(20))
                <= Duration::from_nanos(2)
        );
    }
    #[test]
    fn startup_buffer_span_maps_to_contiguous_source_samples() {
        let now = crate::context::monotonic_ns().unwrap() as i64;
        let first = CaptureTiming {
            cycle: now - 100_000_000,
            now: now - 100_000_000,
            delay: 512,
            rate_num: 1,
            rate_denom: 48_000,
            buffered: 32,
            quantum_frames: 255,
            ..CaptureTiming::default()
        };
        let second = CaptureTiming {
            cycle: first.cycle + 10_666_667,
            now: first.now + 10_666_667,
            ..first
        };
        let a = capture_buffer_time(first, 480).unwrap();
        let b = capture_buffer_time(second, 256).unwrap();
        assert!(
            b.duration_since(a).abs_diff(Duration::from_millis(20)) < Duration::from_micros(100)
        );
        let (tx, mut rx) = mpsc::channel(4);
        let mut framer = Framer::default();
        framer.push(&[0; 960], Some(a), true, &tx).unwrap();
        framer.push(&[0; 512], Some(b), true, &tx).unwrap();
        assert!(rx.try_recv().unwrap().unwrap().timestamp_verified);
        assert!(
            capture_buffer_time(
                CaptureTiming {
                    quantum_frames: 0,
                    ..first
                },
                480
            )
            .is_none()
        );
        assert!(capture_buffer_time(first, 0).is_none());
        assert!(capture_buffer_time(first, 254).is_none());
        let old = CaptureTiming {
            cycle: now - 4_900_000_000,
            now: now - 4_900_000_000,
            quantum_frames: 1,
            ..first
        };
        assert!(capture_buffer_time(old, 8_192).is_none());
    }
    #[test]
    fn capture_ring_is_bounded_and_preserves_order_across_wrap() {
        let ring = CaptureRing::new();
        for round in 0..20 {
            for index in 0..4 {
                let mut chunk = NativeChunk {
                    bytes: [0; NATIVE_CHUNK_BYTES],
                    len: 2,
                    timestamp: None,
                    timing: CaptureTiming::default(),
                };
                chunk.bytes[0] = round * 4 + index;
                assert!(ring.push(chunk));
            }
            assert!(!ring.push(NativeChunk {
                bytes: [0; NATIVE_CHUNK_BYTES],
                len: 2,
                timestamp: None,
                timing: CaptureTiming::default(),
            }));
            for index in 0..4 {
                assert_eq!(ring.pop().unwrap().bytes[0], round * 4 + index);
            }
            assert!(ring.pop().is_none());
        }
    }
    #[test]
    fn queued_or_unknown_audio_never_claims_an_actual_playback_milestone() {
        let mut state = Output {
            submitted: 96_000,
            telemetry: Some(crate::telemetry::fields(
                &serde_json::json!({"generation":7,"phase":"feedback","transcript":"must never be retained"}),
            )),
            ..Default::default()
        };
        assert!(claim_first_played(&mut state, None).is_none());
        assert!(claim_first_played(&mut state, Some(0)).is_none());
        assert!(!state.first_played);
        let (labels, samples) = claim_first_played(&mut state, Some(480)).unwrap();
        assert_eq!(samples, 480);
        assert_eq!(labels["generation"], 7);
        assert!(labels.get("transcript").is_none());
        assert!(state.first_played);
        assert!(claim_first_played(&mut state, Some(960)).is_none());
        let mut unlabeled = Output::default();
        assert!(claim_first_played(&mut unlabeled, Some(480)).is_none());
        assert!(!unlabeled.first_played);
    }
    #[test]
    fn pcm_level_is_samples_not_container() {
        assert_eq!(rms(&[0; 960]), 0.0);
        assert!(rms(&[255, 127, 0, 128]) > 0.99);
    }
    #[test]
    fn framer_preserves_sample_timestamps_and_rejects_overflow_before_loss() {
        let (tx, mut rx) = mpsc::channel(4);
        let mut f = Framer::default();
        let at = Instant::now();
        f.push(&[1; 480], Some(at), true, &tx).unwrap();
        f.push(&[2; 1440], Some(at + Duration::from_millis(10)), true, &tx)
            .unwrap();
        let a = rx.try_recv().unwrap().unwrap();
        let b = rx.try_recv().unwrap().unwrap();
        assert_eq!(a.captured, at);
        assert_eq!(b.captured, at + Duration::from_millis(20));
        assert!(a.timestamp_verified && b.timestamp_verified);
        assert_eq!(a.pcm.len(), 960);
        f.push(&[0; 960], None, false, &tx).unwrap();
        f.push(&[0; 960], None, false, &tx).unwrap();
        f.push(&[0; 960], None, false, &tx).unwrap();
        assert!(f.push(&[0; 960], None, false, &tx).is_err());
    }
    #[test]
    fn timing_uses_queue_converter_device_delay_and_rounds_down_played_audio() {
        assert_eq!(
            played_floor(24_000, 2_400, 1_200, 4_800, 1, 48_000, 0),
            Some(18_000)
        );
        assert_eq!(
            played_floor(24_000, 2_400, 1_200, 4_800, 1, 48_000, 50_000_000),
            Some(19_200)
        );
        assert_eq!(played_floor(100, 200, 0, 0, 1, 24_000, 0), Some(0));
        assert_eq!(played_floor(100, 0, 0, 0, 0, 24_000, 0), None);
    }
    #[test]
    fn clock_maps_to_each_audio_item_and_unknown_timing_deletes() {
        let now = Instant::now();
        let mut state = Output {
            segments: vec![
                Segment {
                    item_id: "a".into(),
                    content_index: 0,
                    start: 0,
                    end: 24_000,
                },
                Segment {
                    item_id: "b".into(),
                    content_index: 1,
                    start: 24_000,
                    end: 48_000,
                },
                Segment {
                    item_id: "c".into(),
                    content_index: 0,
                    start: 48_000,
                    end: 72_000,
                },
            ],
            clock: Some((now, 36_000)),
            ..Default::default()
        };
        assert_eq!(
            edits(&state, now),
            vec![
                HistoryEdit::Truncate {
                    item_id: "b".into(),
                    content_index: 1,
                    audio_end_ms: 500
                },
                HistoryEdit::Delete {
                    item_id: "c".into()
                }
            ]
        );
        assert_eq!(edits(&state, now + Duration::from_millis(101)).len(), 3);
        state.drained = true;
        assert!(edits(&state, now).is_empty());
    }
    #[test]
    fn synthetic_header_and_graph_cycle_are_monotonic_not_callback_time() {
        let mut clock = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut clock) },
            0
        );
        let ns = clock.tv_sec * 1_000_000_000 + clock.tv_nsec;
        let header = monotonic_instant(ns - 40_000_000).unwrap();
        let derived = capture_cycle_time(ns, ns, 960, 1, 48_000, 480).unwrap();
        assert!(header.elapsed() >= Duration::from_millis(39));
        assert!(derived.elapsed() >= Duration::from_millis(39));
        assert!(
            derived
                .checked_duration_since(header)
                .unwrap_or_else(|| header.duration_since(derived))
                < Duration::from_millis(2)
        );
        assert!(capture_cycle_time(0, ns, 0, 1, 24_000, 0).is_none());
    }
    #[tokio::test]
    async fn output_queue_is_bounded_and_submission_is_not_playback() {
        let state = Arc::new(Mutex::new(Output::default()));
        let mut p = Playback {
            worker: Worker {
                stop: Arc::new(AtomicBool::new(false)),
                thread: None,
            },
            state,
            fault: Fault::default(),
            notify: Arc::new(Notify::new()),
        };
        p.write(&vec![0; QUEUE_BYTES]).await.unwrap();
        assert_eq!(
            p.write(&[0; 2]).await.unwrap_err().to_string(),
            "playback_queue_stalled"
        );
        assert_eq!(p.played_samples(), None);
        p.state.lock().unwrap().submitted = 48_000;
        assert_eq!(p.played_samples(), None);
        p.state.lock().unwrap().drained = true;
        assert_eq!(p.played_samples(), Some(24_000));
    }

    #[tokio::test]
    async fn faster_than_playback_input_waits_without_dropping_or_duplicating_samples() {
        let state = Arc::new(Mutex::new(Output::default()));
        let fault = Fault::default();
        let mut p = Playback {
            worker: Worker {
                stop: Arc::new(AtomicBool::new(false)),
                thread: None,
            },
            state: state.clone(),
            fault: fault.clone(),
            notify: Arc::new(Notify::new()),
        };
        p.write_item("reply", 0, &vec![0; QUEUE_BYTES])
            .await
            .unwrap();
        // Dropping a blocked write must not append any part of its chunk.
        assert!(
            tokio::time::timeout(
                Duration::from_millis(20),
                p.write_item("reply", 0, &[0; 960])
            )
            .await
            .is_err()
        );
        {
            let mut output = state.lock().unwrap();
            assert_eq!(output.received_samples, u64::from(RATE) * 2);
            drop(output.queue.drain(..960));
            output.submitted = 960;
            output.clock = Some((Instant::now(), 480));
        }
        p.write_item("reply", 0, &[0; 960]).await.unwrap();
        {
            let output = state.lock().unwrap();
            assert_eq!(output.received_samples, u64::from(RATE) * 2 + 480);
            assert_eq!(output.queue.len(), QUEUE_BYTES);
            assert_eq!(output.segments.len(), 1);
            assert_eq!(output.segments[0].end, output.received_samples);
        }
        // A native device failure must interrupt backpressure promptly.
        let mut blocked = Box::pin(p.write_item("reply", 0, &[0; 960]));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut blocked)
                .await
                .is_err()
        );
        fail(&fault, "pulse_playback_unavailable");
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(100), blocked)
                .await
                .unwrap()
                .unwrap_err()
                .to_string(),
            "pulse_playback_unavailable"
        );
        assert_eq!(
            state.lock().unwrap().received_samples,
            u64::from(RATE) * 2 + 480
        );
    }
}

#[cfg(test)]
mod native_tests {
    use super::*;
    /// Run with eva/tests/native-audio.sh: the graph contains only a synthetic
    /// source and a null sink, never the user's actual microphone or speakers.
    #[tokio::test]
    #[ignore = "requires the isolated synthetic graph from eva/tests/native-audio.sh"]
    async fn native_virtual_graph_roundtrip() {
        assert_eq!(std::env::var("JWM_EVA_AUDIO_TEST").as_deref(), Ok("1"));
        let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap();
        assert!(
            runtime.starts_with("/tmp/jwm-eva-pw-"),
            "refusing a non-isolated audio graph"
        );
        probe().unwrap();
        let (tx, mut rx) = mpsc::channel(100);
        let capture = Capture::start("jwm-eva-test-source", tx).unwrap();
        let mut skipped_startup_frames = 0;
        let frame = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let frame = rx
                    .recv()
                    .await
                    .expect("capture closed")
                    .expect("native capture failed");
                capture.check().unwrap();
                if frame.timestamp_verified && frame.level > 0.001 {
                    return frame;
                }
                // Initial buffers may precede the graph's first clock report.
                // They retain false provenance in production; never relabel
                // them with arrival time to make a window command executable.
                skipped_startup_frames += 1;
            }
        })
        .await
        .expect("synthetic source never produced a timed nonzero signal");
        assert_eq!(frame.pcm.len(), FRAME_BYTES);
        assert!(
            frame.timestamp_verified,
            "native graph-cycle timing must support trustworthy source timestamps"
        );
        assert!(
            frame.level > 0.001,
            "synthetic sine source should not be silent"
        );
        capture.check().unwrap();
        let stopped = Instant::now();
        capture.stop().await;
        let release_ms = stopped.elapsed().as_millis();
        assert!(
            stopped.elapsed() < Duration::from_millis(300),
            "microphone release exceeded target"
        );
        let mut playback = Playback::start().unwrap();
        playback.set_telemetry(serde_json::json!({"phase":"native_test","generation":1}));
        let playback_state = playback.state.clone();
        playback
            .write(&vec![0; RATE as usize * 2 / 5])
            .await
            .unwrap(); // 200 ms.
        assert_eq!(playback.played_samples(), None);
        let started = Instant::now();
        playback.finish().await.expect("native drain failed");
        let drain_ms = started.elapsed().as_millis();
        assert!(
            playback_state.lock().unwrap().first_played,
            "actual graph progress never reported its first-played milestone"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(100),
            "finish returned before the graph consumed the signal"
        );
        let mut interrupted = Playback::start().unwrap();
        interrupted
            .write_item("heard", 0, &vec![0; 9_600])
            .await
            .unwrap();
        interrupted
            .write_item("partial", 1, &vec![0; 38_400])
            .await
            .unwrap();
        interrupted
            .write_item("unheard", 0, &vec![0; 48_000])
            .await
            .unwrap();
        let history = interrupted.history();
        let progress_start = Instant::now();
        tokio::time::sleep(Duration::from_millis(350)).await;
        let played = history
            .played_samples()
            .expect("native device clock was unavailable");
        assert!(
            played > 4_800 && played < 24_000,
            "played offset must come from device progress, got {played}"
        );
        assert!(
            played * 1000 / RATE as u64 <= progress_start.elapsed().as_millis() as u64 + 40,
            "played ahead of the graph clock"
        );
        let stopped_output = Instant::now();
        drop(interrupted);
        assert!(stopped_output.elapsed() < Duration::from_millis(100));
        let edits = history.edits();
        eprintln!(
            "native active played={}ms, interruption={}ms, edits={edits:?}",
            played * 1000 / RATE as u64,
            stopped_output.elapsed().as_millis()
        );
        assert!(edits.iter().any(|e|matches!(e,HistoryEdit::Truncate{item_id,content_index:1,audio_end_ms}if item_id=="partial"&&*audio_end_ms>0&&*audio_end_ms<800)));
        assert!(edits.contains(&HistoryEdit::Delete {
            item_id: "unheard".into()
        }));
        assert!(!edits.iter().any(|e|matches!(e,HistoryEdit::Delete{item_id}|HistoryEdit::Truncate{item_id,..}if item_id=="heard")));
        eprintln!("startup frames skipped: {skipped_startup_frames}");
        eprintln!(
            "native source: PCM16LE/24kHz/mono, timestamp_verified={}, release={}ms, drain={}ms",
            frame.timestamp_verified, release_ms, drain_ms
        );
    }
}

#[cfg(test)]
mod barge_in_tests {
    use super::*;

    fn frame(samples: &[i16]) -> Frame {
        let pcm = samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect::<Vec<_>>();
        Frame {
            captured: Instant::now(),
            timestamp_verified: true,
            level: rms(&pcm),
            pcm,
        }
    }
    fn voice(seed: u32) -> Vec<i16> {
        // A deterministic aperiodic reference avoids a sine wave's unrealistic
        // perfect correlation at arbitrary delays.
        let mut value = seed;
        (0..480)
            .map(|_| {
                value = value.wrapping_mul(1664525).wrapping_add(1013904223);
                ((value >> 17) as i16).wrapping_sub(16000)
            })
            .collect()
    }
    #[test]
    fn sustained_new_voice_interrupts_but_clicks_and_silence_do_not() {
        let voice = frame(&voice(1));
        let quiet = frame(&[0; 480]);
        let mut gate = BargeInGate::default();
        assert!(!gate.observe_with_reference(&voice, &[]));
        for _ in 0..20 {
            assert!(!gate.observe_with_reference(&quiet, &[]));
        }
        for _ in 0..5 {
            assert!(!gate.observe_with_reference(&voice, &[]));
        }
        assert!(gate.observe_with_reference(&voice, &[]));
        gate.reset();
        assert!(!gate.observe_with_reference(&voice, &[]));
    }
    #[test]
    fn echoed_output_with_delay_and_gain_does_not_interrupt() {
        let output = voice(7);
        let mut reference = vec![0; 64];
        reference.extend_from_slice(&output);
        reference.extend_from_slice(&[0; 96]);
        let echoed = frame(&output.iter().map(|sample| -sample / 3).collect::<Vec<_>>());
        assert!(echo_matches(&echoed.pcm, &reference));
        let mut gate = BargeInGate::default();
        for _ in 0..20 {
            assert!(!gate.observe_with_reference(&echoed, &reference));
            assert!(gate.echo_detected());
        }
        let new_voice = frame(&voice(42));
        for _ in 0..5 {
            assert!(!gate.observe_with_reference(&new_voice, &reference));
        }
        assert!(gate.observe_with_reference(&new_voice, &reference));
        assert!(!gate.echo_detected());
    }
    #[test]
    fn native_streams_request_echo_treatment_without_changing_device_pin() {
        let capture = props(true, "my-microphone");
        assert_eq!(capture.get("filter.want"), Some("echo-cancel"));
        assert_eq!(capture.get(*pw::keys::TARGET_OBJECT), Some("my-microphone"));
        assert_eq!(capture.get("node.dont-fallback"), Some("true"));
        assert_eq!(
            props(false, "default").get("filter.want"),
            Some("echo-cancel")
        );
    }
}
