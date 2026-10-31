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
// Five native frames give a continuous 100 ms echo observation before the
// sixth voiced frame could trigger barge-in. This buffer never leaves memory.
const ECHO_PROBE_BYTES: usize = FRAME_BYTES * 5;
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
    let tail = &samples[samples.len().saturating_sub(FRAME_BYTES / 8)..];
    let tail_stats = stats(tail);
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
        // Reject most offsets using a quarter of the decimated samples first.
        // The current frame must also match at this same offset: an earlier
        // echoed prefix cannot hide new speech at the end of the probe.
        if matches(&coarse, coarse_stats, offset, 16, 0.45)
            && matches(&samples, complete_stats, offset, 4, 0.72)
            && matches(
                tail,
                tail_stats,
                offset + (samples.len() - tail.len()) * 4,
                4,
                0.72,
            )
        {
            return true;
        }
    }
    false
}

/// Local interruption listens for a human voice, not just loudness. Traffic
/// rumble, HVAC and desk bumps sit mostly below the speech band, and clicks,
/// breath and broadband noise have no pitch. A frame counts as voiced only when
/// its speech-band level clears the calibrated floor and the band-limited
/// signal repeats at a human pitch period (80–400 Hz). A sustained pitched hum
/// in that range, music or another person talking can still interrupt.
const VOICE_HIGH_PASS_HZ: f64 = 250.0;
/// Filter settling excluded from a stateless per-frame level (6 ms).
const VOICE_LEVEL_SETTLE: usize = 144;
/// 40 ms of band-limited audio decimated to 8 kHz covers two 80 Hz periods.
const VOICE_WINDOW: usize = 320;
const VOICE_DECIMATION: usize = 3;
const VOICE_MIN_LAG: usize = 20; // 400 Hz at 8 kHz.
const VOICE_MAX_LAG: usize = 100; // 80 Hz at 8 kHz.
const VOICE_PERIODICITY: f32 = 0.55;
/// Within 300 ms: 200 ms in the speech band, 140 ms of it pitched. Tolerates
/// unvoiced consonants and short gaps; rejects typing, taps, bumps, breaths
/// and brief faint mouth or chair sounds (tuned on a labelled live recording).
const SPEECH_WINDOW_MASK: u16 = 0x7fff;
const SPEECH_MIN_FRAMES: u32 = 10;
const VOICED_MIN_FRAMES: u32 = 7;
/// After a turn commit the user's own utterance may still be trailing off: a
/// semantic end of turn can precede the acoustic end. Arm only after 160 ms
/// of quiet, so the tail never cancels its own plan; a pause then new speech
/// still interrupts.
const ARM_QUIET_FRAMES: u32 = 8;

#[derive(Clone, Copy, Default)]
struct HighPass {
    state: [[f64; 4]; 2],
}
impl HighPass {
    /// Fourth-order Butterworth: two RBJ high-pass sections.
    fn coefficients() -> [[f64; 4]; 2] {
        let w = std::f64::consts::TAU * VOICE_HIGH_PASS_HZ / f64::from(RATE);
        [0.541_196_1, 1.306_563].map(|q| {
            let (alpha, cos) = (w.sin() / (2.0 * q), w.cos());
            let a0 = 1.0 + alpha;
            [
                (1.0 + cos) / 2.0 / a0,
                -(1.0 + cos) / a0,
                -2.0 * cos / a0,
                (1.0 - alpha) / a0,
            ]
        })
    }
    fn process(&mut self, coefficients: &[[f64; 4]; 2], mut sample: f64) -> f64 {
        for ([b0, b1, a1, a2], [x1, x2, y1, y2]) in coefficients.iter().zip(&mut self.state) {
            let out = b0 * sample + b1 * *x1 + b0 * *x2 - a1 * *y1 - a2 * *y2;
            (*x2, *x1, *y2, *y1) = (*x1, sample, *y1, out);
            sample = out;
        }
        sample
    }
}
fn pcm_samples(pcm: &[u8]) -> impl Iterator<Item = f64> + '_ {
    pcm.chunks_exact(2)
        .map(|sample| f64::from(i16::from_le_bytes([sample[0], sample[1]])) / 32768.0)
}
/// Speech-band RMS of one frame, used for both noise calibration and the
/// interruption threshold. Each frame starts from rest; settling is excluded.
pub fn speech_band_level(pcm: &[u8]) -> f32 {
    let coefficients = HighPass::coefficients();
    let mut filter = HighPass::default();
    let (mut sum, mut count) = (0.0, 0usize);
    for (index, sample) in pcm_samples(pcm).enumerate() {
        let out = filter.process(&coefficients, sample);
        if index >= VOICE_LEVEL_SETTLE {
            sum += out * out;
            count += 1;
        }
    }
    if count == 0 {
        0.0
    } else {
        (sum / count as f64).sqrt() as f32
    }
}
/// Pitch evidence over contiguous frames. Holds only filtered, decimated
/// samples of the last 40 ms in memory; reset at every discontinuity.
struct Voicing {
    coefficients: [[f64; 4]; 2],
    filter: HighPass,
    pending: [f64; VOICE_DECIMATION],
    pending_len: usize,
    history: VecDeque<f32>,
}
impl Default for Voicing {
    fn default() -> Self {
        Self {
            coefficients: HighPass::coefficients(),
            filter: HighPass::default(),
            pending: [0.0; VOICE_DECIMATION],
            pending_len: 0,
            history: VecDeque::with_capacity(VOICE_WINDOW),
        }
    }
}
impl Voicing {
    fn reset(&mut self) {
        self.filter = HighPass::default();
        self.pending_len = 0;
        self.history.clear();
    }
    /// Feed one frame; returns the best normalized autocorrelation at a human
    /// pitch lag, or 0 until a full window of contiguous audio is available.
    fn observe(&mut self, pcm: &[u8]) -> f32 {
        for sample in pcm_samples(pcm) {
            self.pending[self.pending_len] = self.filter.process(&self.coefficients, sample);
            self.pending_len += 1;
            if self.pending_len == VOICE_DECIMATION {
                self.pending_len = 0;
                if self.history.len() == VOICE_WINDOW {
                    self.history.pop_front();
                }
                self.history
                    .push_back((self.pending.iter().sum::<f64>() / VOICE_DECIMATION as f64) as f32);
            }
        }
        if self.history.len() < VOICE_WINDOW {
            return 0.0;
        }
        let window = self.history.make_contiguous();
        // prefix[i] = sum of squares of window[..i].
        let mut prefix = [0.0f64; VOICE_WINDOW + 1];
        for (index, value) in window.iter().enumerate() {
            prefix[index + 1] = prefix[index] + f64::from(*value) * f64::from(*value);
        }
        let mut best = 0.0f64;
        for lag in VOICE_MIN_LAG..=VOICE_MAX_LAG {
            let span = VOICE_WINDOW - lag;
            let leading = prefix[span];
            let trailing = prefix[VOICE_WINDOW] - prefix[lag];
            if leading <= 1e-12 || trailing <= 1e-12 {
                continue;
            }
            let product = window[..span]
                .iter()
                .zip(&window[lag..])
                .map(|(a, b)| f64::from(*a) * f64::from(*b))
                .sum::<f64>();
            best = best.max(product / (leading * trailing).sqrt());
        }
        best as f32
    }
}

/// Calibrated only while listening, never from Eva's own playback. A low
/// percentile tolerates the delayed server VAD marking a few onset frames as
/// background; a loud click cannot set the sensitivity for an entire reply.
pub struct InputNoise {
    levels: [f32; 50],
    next: usize,
    count: usize,
}
impl Default for InputNoise {
    fn default() -> Self {
        Self {
            levels: [0.0; 50],
            next: 0,
            count: 0,
        }
    }
}
impl InputNoise {
    pub fn observe(&mut self, level: f32) {
        if !level.is_finite() || !(0.0..=1.0).contains(&level) {
            return;
        }
        self.levels[self.next] = level;
        self.next = (self.next + 1) % self.levels.len();
        self.count = (self.count + 1).min(self.levels.len());
    }
    pub fn threshold(&self) -> f32 {
        if self.count < 10 {
            return 0.006;
        }
        let mut levels = self.levels;
        levels[..self.count].sort_unstable_by(f32::total_cmp);
        // Speech-band levels. A 6x (about 16 dB) margin over the quiet
        // percentile, never below 0.002: in a quiet room a 3x margin and a
        // 0.001 floor let faint pitched sounds (a chair, a murmur) interrupt.
        // Speech whose band level stays below 0.002 needs more input gain.
        (levels[self.count / 5] * 6.0).max(0.002)
    }
}

/// Sustained local speech interrupts pending work without waiting for ASR or
/// a model response. PCM is used only for echo comparison and never persisted.
pub struct BargeInGate {
    speech_window: u16,
    voiced_window: u16,
    voicing: Voicing,
    voiced_frames: u64,
    max_voiced_pitch: u32,
    echo_probe: Vec<u8>,
    threshold: f32,
    noise_frames: usize,
    echo_detected: bool,
    frames: u64,
    above_threshold: u64,
    echo_frames: u64,
    peak: f32,
    max_voiced: u32,
    since: Option<Instant>,
    last_source: Option<Instant>,
    sequence_source: Option<Instant>,
    rejected_frames: u64,
    discontinuities: u64,
    armed: bool,
    quiet_frames: u32,
    tail_frames: u64,
}
/// Keep replay buffering under the same source-time decision as interruption.
pub struct BargeInObservation {
    pub interrupted: bool,
    pub retain_frame: bool,
    pub reset_buffer: bool,
}
impl Default for BargeInGate {
    fn default() -> Self {
        Self::with_noise(&InputNoise::default())
    }
}
impl BargeInGate {
    pub fn with_noise(noise: &InputNoise) -> Self {
        Self {
            speech_window: 0,
            voiced_window: 0,
            voicing: Voicing::default(),
            voiced_frames: 0,
            max_voiced_pitch: 0,
            echo_probe: Vec::with_capacity(ECHO_PROBE_BYTES),
            threshold: noise.threshold(),
            noise_frames: noise.count,
            echo_detected: false,
            frames: 0,
            above_threshold: 0,
            echo_frames: 0,
            peak: 0.0,
            max_voiced: 0,
            since: None,
            last_source: None,
            sequence_source: None,
            rejected_frames: 0,
            discontinuities: 0,
            armed: true,
            quiet_frames: 0,
            tail_frames: 0,
        }
    }
    /// A turn boundary: frames before `since` are rejected, and the gate arms
    /// only after the committed utterance has fallen quiet.
    pub fn with_noise_since(noise: &InputNoise, since: Instant) -> Self {
        Self {
            since: Some(since),
            armed: false,
            ..Self::with_noise(noise)
        }
    }
    #[cfg(test)]
    pub(crate) fn arm_for_test(&mut self) {
        self.armed = true;
    }
    pub fn reset(&mut self) {
        self.speech_window = 0;
        self.voiced_window = 0;
        self.quiet_frames = 0;
        self.voicing.reset();
        self.echo_probe.clear();
        self.echo_detected = false;
        self.sequence_source = None;
    }
    pub fn echo_detected(&self) -> bool {
        self.echo_detected
    }
    /// Aggregate signal diagnostics only; never PCM, reference samples or ASR.
    pub fn statistics(&self) -> serde_json::Value {
        serde_json::json!({"input_frames":self.frames,"noise_frames":self.noise_frames,"above_threshold_frames":self.above_threshold,
            "echo_frames":self.echo_frames,"peak_level":self.peak,"threshold_level":self.threshold,
            "max_voiced_frames":self.max_voiced,"pitched_frames":self.voiced_frames,"max_pitched_frames":self.max_voiced_pitch,
            "tail_frames":self.tail_frames,
            "rejected_source_frames":self.rejected_frames,
            "source_discontinuities":self.discontinuities})
    }
    pub fn observe(&mut self, frame: &Frame) -> BargeInObservation {
        self.observe_at(frame, Instant::now())
    }
    pub(crate) fn observe_at(&mut self, frame: &Frame, now: Instant) -> BargeInObservation {
        // Arrival time cannot make queued PCM current. Only native timestamps
        // after this turn's commit may interrupt it or enter its replay buffer.
        let age = now.checked_duration_since(frame.captured);
        let delta = self
            .last_source
            .and_then(|last| frame.captured.checked_duration_since(last));
        if !frame.timestamp_verified
            || frame.pcm.len() != FRAME_BYTES
            || age.is_none_or(|age| age > Duration::from_millis(250))
            || self.since.is_some_and(|since| frame.captured < since)
            || self.last_source.is_some()
                && delta.is_none_or(|delta| delta < Duration::from_millis(15))
        {
            self.frames = self.frames.saturating_add(1);
            self.rejected_frames = self.rejected_frames.saturating_add(1);
            self.reset();
            return BargeInObservation {
                interrupted: false,
                retain_frame: false,
                reset_buffer: true,
            };
        }
        // Framer already rejects discontinuous native chunks. Check again at
        // this consumer boundary: draining, dropped work and phase changes can
        // omit frames even when each individual native chunk was valid.
        let continuous = self
            .sequence_source
            .is_some_and(|last| frame.captured.duration_since(last) <= Duration::from_millis(25));
        if !continuous {
            if self.sequence_source.is_some() {
                self.discontinuities = self.discontinuities.saturating_add(1);
            }
            self.reset();
        }
        self.last_source = Some(frame.captured);
        self.sequence_source = Some(frame.captured);
        let armed = self.armed;
        let interrupted = self.observe_signal(frame);
        if !armed {
            // The previous utterance's tail must not reach the next turn.
            return BargeInObservation {
                interrupted: false,
                retain_frame: false,
                reset_buffer: true,
            };
        }
        BargeInObservation {
            interrupted,
            retain_frame: !self.echo_detected,
            reset_buffer: !continuous || self.echo_detected,
        }
    }
    fn observe_signal(&mut self, frame: &Frame) -> bool {
        let level = speech_band_level(&frame.pcm);
        if !level.is_finite() || level <= self.threshold {
            return self.observe_with_reference(frame, &[]);
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
    /// Native framing supplies consecutive 20 ms PCM. A matching pitch in one
    /// short frame somewhere in three seconds of output is not proof of echo;
    /// require one contiguous render segment for the entire probe.
    fn push_echo_probe(&mut self, frame: &Frame) {
        if frame.pcm.len() == FRAME_BYTES {
            if self.echo_probe.len() == ECHO_PROBE_BYTES {
                self.echo_probe.drain(..FRAME_BYTES);
            }
            self.echo_probe.extend_from_slice(&frame.pcm);
        } else {
            self.echo_probe.clear();
        }
    }
    fn observe_with_reference(&mut self, frame: &Frame, reference: &[i16]) -> bool {
        self.frames = self.frames.saturating_add(1);
        let level = speech_band_level(&frame.pcm);
        if level.is_finite() {
            self.peak = self.peak.max(level.clamp(0.0, 1.0));
        }
        let candidate = level.is_finite() && level > self.threshold;
        // Pitch history must see every contiguous frame, quiet ones included.
        let periodicity = if frame.pcm.len() == FRAME_BYTES {
            self.voicing.observe(&frame.pcm)
        } else {
            self.voicing.reset();
            0.0
        };
        let pitched = candidate && periodicity >= VOICE_PERIODICITY;
        if !self.armed {
            self.tail_frames = self.tail_frames.saturating_add(1);
            self.quiet_frames = if candidate { 0 } else { self.quiet_frames + 1 };
            self.armed = self.quiet_frames >= ARM_QUIET_FRAMES;
            // Keep the echo probe contiguous; nothing else counts yet.
            self.push_echo_probe(frame);
            return false;
        }
        self.above_threshold = self.above_threshold.saturating_add(u64::from(candidate));
        self.push_echo_probe(frame);
        self.echo_detected = candidate
            && self.echo_probe.len() == ECHO_PROBE_BYTES
            && echo_matches(&self.echo_probe, reference);
        if self.echo_detected {
            self.echo_frames = self.echo_frames.saturating_add(1);
            self.speech_window = 0;
            self.voiced_window = 0;
            return false;
        }
        // Each Frame is exactly 20 ms; the native framer rejects dropped or
        // discontinuous PCM, and the windows reset at every discontinuity.
        self.speech_window =
            ((self.speech_window << 1) | u16::from(candidate)) & SPEECH_WINDOW_MASK;
        self.voiced_window = ((self.voiced_window << 1) | u16::from(pitched)) & SPEECH_WINDOW_MASK;
        self.voiced_frames = self.voiced_frames.saturating_add(u64::from(pitched));
        let voiced = self.speech_window.count_ones();
        let pitch = self.voiced_window.count_ones();
        self.max_voiced = self.max_voiced.max(voiced);
        self.max_voiced_pitch = self.max_voiced_pitch.max(pitch);
        voiced >= SPEECH_MIN_FRAMES && pitch >= VOICED_MIN_FRAMES
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
        crate::companion::capture_stopped();
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.worker.shutdown();
        crate::companion::capture_stopped();
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

            crate::companion::capture_frame(frame.captured, frame.timestamp_verified);
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
    clock_boottime_ns: Option<u64>,
    telemetry: Option<serde_json::Value>,
    companion_labels: Option<serde_json::Value>,
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
    /// Nonblocking metadata only; a contended audio queue yields no evidence.
    pub(crate) fn companion_snapshot(&self) -> Option<(u64, Option<u64>, bool, Option<u64>)> {
        let s = self.state.try_lock().ok()?;
        let played = if s.drained {
            Some(s.submitted / 2)
        } else {
            s.clock
                .filter(|(at, _)| at.elapsed() <= Duration::from_millis(100))
                .filter(|_| {
                    s.clock_boottime_ns
                        .zip(companion_voice::boottime_ns())
                        .is_some_and(|(at, now)| {
                            now.checked_sub(at).is_some_and(|age| age <= 100_000_000)
                        })
                })
                .map(|(_, n)| n)
        };
        Some((
            s.received_samples,
            played,
            s.drained,
            if s.drained {
                None
            } else {
                played.and(s.clock_boottime_ns)
            },
        ))
    }
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
        crate::companion::playback(&labels, self.history());
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.first_played {
            state.companion_labels = Some(labels.clone());
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
        {
            let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(labels) = s.companion_labels.as_ref() {
                let played = if s.drained {
                    Some(s.submitted / 2)
                } else {
                    s.clock
                        .filter(|(at, _)| at.elapsed() <= Duration::from_millis(100))
                        .map(|(_, n)| n)
                };
                crate::companion::playback_ended(labels, s.received_samples, played, s.drained);
            }
        }
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
    let completed_state = state.clone();
    let _listener = stream
        .add_local_listener_with_user_data(())
        .state_changed(move |_, _, old, new| {
            if matches!(new, pw::stream::StreamState::Error(_))
                || (matches!(new, pw::stream::StreamState::Unconnected)
                    && !matches!(old, pw::stream::StreamState::Unconnected))
            {
                // The loop disconnects after the actual drained callback.
                // That successful teardown can race finish() waking up. It
                // must not retroactively fail already confirmed playback;
                // errors before drained remain errors and are never cleared.
                if !completed_state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .drained
                {
                    fail(&state_fault, "playback_unavailable");
                }
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
                s.clock_boottime_ns = companion_voice::boottime_ns();
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
    fn companion_playback_requires_recent_boot_inclusive_timing() {
        let now = companion_voice::boottime_ns().unwrap();
        let state = Arc::new(Mutex::new(Output {
            received_samples: 2400,
            submitted: 960,
            clock: Some((Instant::now(), 240)),
            clock_boottime_ns: Some(now),
            ..Output::default()
        }));
        let history = PlaybackHistory {
            state: state.clone(),
        };
        assert_eq!(
            history.companion_snapshot(),
            Some((2400, Some(240), false, Some(now)))
        );
        for timestamp in [None, Some(now - 200_000_000), Some(now + 1_000_000_000)] {
            state.lock().unwrap().clock_boottime_ns = timestamp;
            assert_eq!(
                history.companion_snapshot(),
                Some((2400, None, false, None))
            );
        }
        // A previously verified drain remains a historical fact, never ongoing speaking.
        state.lock().unwrap().drained = true;
        assert_eq!(
            history.companion_snapshot(),
            Some((2400, Some(480), true, None))
        );
    }

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
        let (generated, observed, drained, timestamp) = history.companion_snapshot().unwrap();
        assert!(!drained && observed.is_some_and(|n| n > 0 && n <= generated));
        assert!(companion_voice::fresh(
            timestamp.unwrap(),
            companion_voice::boottime_ns().unwrap()
        ));
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

    /// Replay a local labelled WAV (PCM16 mono, 24 or 48 kHz) through the
    /// interruption gate. Noise is calibrated from the "silence" segment.
    /// Prints aggregate per-segment counts only; never PCM or transcripts.
    ///   JWM_EVA_VOICE_WAV=/path.wav
    ///   JWM_EVA_VOICE_SEGMENTS=silence:0-6,speech:6-16,keys:16-24
    #[test]
    #[ignore = "diagnostic: needs a local labelled recording"]
    fn replay_labelled_recording_through_the_interruption_gate() {
        let path = std::env::var("JWM_EVA_VOICE_WAV").expect("JWM_EVA_VOICE_WAV");
        let segments = std::env::var("JWM_EVA_VOICE_SEGMENTS").expect("JWM_EVA_VOICE_SEGMENTS");
        let bytes = std::fs::read(path).unwrap();
        let (mut offset, mut rate, mut data) = (12, 0u32, &[][..]);
        while offset + 8 <= bytes.len() {
            let id = &bytes[offset..offset + 4];
            let size =
                u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
            let body = &bytes[offset + 8..(offset + 8 + size).min(bytes.len())];
            if id == b"fmt " {
                assert_eq!(u16::from_le_bytes([body[2], body[3]]), 1, "mono only");
                assert_eq!(u16::from_le_bytes([body[14], body[15]]), 16, "PCM16 only");
                rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
            } else if id == b"data" {
                data = body;
            }
            offset += 8 + size + (size & 1);
        }
        let mut samples: Vec<i16> = data
            .chunks_exact(2)
            .map(|s| i16::from_le_bytes([s[0], s[1]]))
            .collect();
        match rate {
            24_000 => {}
            48_000 => {
                samples = samples
                    .chunks_exact(2)
                    .map(|pair| ((i32::from(pair[0]) + i32::from(pair[1])) / 2) as i16)
                    .collect()
            }
            other => panic!("unsupported rate {other}"),
        }
        let frames: Vec<Frame> = samples.chunks_exact(480).map(frame).collect();
        let parsed: Vec<(String, usize, usize)> = segments
            .split(',')
            .map(|item| {
                let (label, range) = item.split_once(':').unwrap();
                let (start, end) = range.split_once('-').unwrap();
                let at = |s: &str| ((s.parse::<f64>().unwrap() * 50.0) as usize).min(frames.len());
                (label.to_owned(), at(start), at(end))
            })
            .collect();
        let mut noise = InputNoise::default();
        for (label, start, end) in &parsed {
            if label == "silence" {
                for frame in &frames[*start..*end] {
                    noise.observe(speech_band_level(&frame.pcm));
                }
            }
        }
        println!("VOICE_REPLAY threshold {:.5}", noise.threshold());
        for (label, start, end) in &parsed {
            let mut gate = BargeInGate::with_noise(&noise);
            let (mut triggers, mut first, mut pitched, mut candidates) = (0, None, 0, 0);
            let mut levels = Vec::new();
            for (index, frame) in frames[*start..*end].iter().enumerate() {
                let level = speech_band_level(&frame.pcm);
                levels.push(level);
                let before = gate.statistics()["pitched_frames"].as_u64().unwrap();
                if gate.observe_with_reference(frame, &[]) {
                    triggers += 1;
                    first.get_or_insert(index * 20);
                    gate = BargeInGate::with_noise(&noise);
                }
                candidates += usize::from(level > gate.threshold);
                pitched +=
                    usize::from(gate.statistics()["pitched_frames"].as_u64().unwrap() > before);
            }
            levels.sort_by(f32::total_cmp);
            let pct = |p: usize| levels.get(levels.len() * p / 100).copied().unwrap_or(0.0);
            println!(
                "VOICE_REPLAY {label:8} frames {:4} triggers {triggers:2} first_ms {:>5} band p50 {:.4} p90 {:.4} above {candidates:4} pitched {pitched:4}",
                end - start,
                first.map_or("-".into(), |ms: usize| ms.to_string()),
                pct(50),
                pct(90),
            );
        }
    }

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

    /// A synthetic voiced frame: 150 Hz harmonics up to 3 kHz (three whole
    /// periods per 20 ms, so repeated frames stay phase-continuous) with
    /// seed-varied harmonic weights and a little aperiodic noise.
    fn voiced(seed: u32) -> Vec<i16> {
        let noise = voice(seed);
        let weights: Vec<f64> = (1..=20u32)
            .map(|harmonic| {
                let jitter = seed
                    .wrapping_mul(2_654_435_761)
                    .wrapping_add(harmonic * 40_503)
                    % 1000;
                0.85f64.powi(harmonic as i32) * (1.0 + 0.3 * f64::from(jitter) / 1000.0)
            })
            .collect();
        (0..480)
            .map(|index| {
                let phase = std::f64::consts::TAU * 150.0 * index as f64 / f64::from(RATE);
                let value: f64 = weights
                    .iter()
                    .enumerate()
                    .map(|(k, weight)| weight * ((k + 1) as f64 * (phase + 0.7)).sin())
                    .sum();
                (value * 4000.0 + f64::from(noise[index]) * 0.02) as i16
            })
            .collect()
    }
    /// A voiced frame scaled to an exact speech-band level.
    fn voiced_at_level(seed: u32, target: f32) -> Frame {
        let source = voiced(seed);
        let source_level = speech_band_level(&frame(&source).pcm);
        frame(
            &source
                .iter()
                .map(|sample| (f32::from(*sample) * target / source_level).round() as i16)
                .collect::<Vec<_>>(),
        )
    }

    fn signal_at_level(seed: u32, target: f32) -> Frame {
        let source = voice(seed);
        let source_level = frame(&source).level;
        frame(
            &source
                .iter()
                .map(|sample| (f32::from(*sample) * target / source_level).round() as i16)
                .collect::<Vec<_>>(),
        )
    }

    fn calibrated_noise(level: f32) -> InputNoise {
        let mut noise = InputNoise::default();
        // A changing quiet baseline, rather than an exactly repeated frame.
        for index in 0..50 {
            noise.observe(
                signal_at_level(index + 100, level * (0.9 + (index % 5) as f32 * 0.05)).level,
            );
        }
        noise
    }

    #[test]
    fn invalid_source_frames_cannot_trigger_an_interruption() {
        for kind in ["stale", "unverified", "future", "malformed"] {
            let mut gate = BargeInGate::default();
            for index in 0..6 {
                let mut input = frame(&voice(index + 500));
                match kind {
                    "stale" => input.captured -= Duration::from_secs(1),
                    "unverified" => input.timestamp_verified = false,
                    "future" => input.captured += Duration::from_secs(1),
                    "malformed" => input.pcm.truncate(2),
                    _ => unreachable!(),
                }
                let observed = gate.observe(&input);
                assert!(!observed.interrupted, "{kind} frame triggered interruption");
                assert!(!observed.retain_frame, "{kind} frame entered replay buffer");
                assert!(observed.reset_buffer);
            }
            assert_eq!(gate.statistics()["rejected_source_frames"], 6);
            assert_eq!(gate.statistics()["above_threshold_frames"], 0);
        }
    }

    #[test]
    fn source_time_accepts_fresh_batched_frames_at_the_age_boundary() {
        let now = Instant::now();
        let start = now - Duration::from_millis(250);
        let mut gate = BargeInGate::default();
        for index in 0..10 {
            let mut input = frame(&voiced(index + 500));
            input.captured = start + Duration::from_millis(u64::from(index) * 20);
            // All frames arrive in one batch, but their native clock proves
            // 200 ms of distinct PCM. Do not count callback/polling intervals.
            let observed = gate.observe_at(&input, now);
            assert_eq!(observed.interrupted, index == 9);
            assert!(observed.retain_frame);
            assert_eq!(observed.reset_buffer, index == 0);
        }
        assert_eq!(gate.statistics()["rejected_source_frames"], 0);

        let mut input = frame(&voiced(700));
        input.captured = start - Duration::from_millis(1);
        let observed = BargeInGate::default().observe_at(&input, now);
        assert!(!observed.interrupted && !observed.retain_frame && observed.reset_buffer);
    }

    #[test]
    fn duplicate_reordered_and_overlapping_source_frames_reset_evidence() {
        let start = Instant::now();
        let mut gate = BargeInGate::default();
        let mut input = frame(&voiced(600));
        for ms in [0, 20, 40, 60, 80] {
            input.captured = start + Duration::from_millis(ms);
            assert!(!gate.observe_at(&input, input.captured).interrupted);
        }
        for ms in [80, 60, 81] {
            input.captured = start + Duration::from_millis(ms);
            let observed = gate.observe_at(&input, start + Duration::from_millis(100));
            assert!(!observed.interrupted && !observed.retain_frame && observed.reset_buffer);
        }
        for index in 0..10 {
            input.captured = start + Duration::from_millis(100 + index * 20);
            let observed = gate.observe_at(&input, input.captured);
            assert_eq!(observed.interrupted, index == 9);
            assert_eq!(observed.reset_buffer, index == 0);
        }
        // Resetting accumulation must not let already consumed PCM be reused.
        gate.reset();
        let observed = gate.observe_at(&input, input.captured);
        assert!(!observed.retain_frame);
        assert_eq!(gate.statistics()["rejected_source_frames"], 4);
    }

    #[test]
    fn missing_source_frames_cannot_join_separate_speech_bursts() {
        let start = Instant::now();
        let mut gate = BargeInGate::default();
        let mut input = frame(&voiced(800));
        for ms in [0, 20, 40, 60, 80] {
            input.captured = start + Duration::from_millis(ms);
            assert!(!gate.observe_at(&input, input.captured).interrupted);
        }
        for index in 0..10 {
            input.captured = start + Duration::from_millis(200 + index * 20);
            let observed = gate.observe_at(&input, input.captured);
            assert_eq!(observed.interrupted, index == 9);
            assert_eq!(observed.reset_buffer, index == 0);
        }
        assert_eq!(gate.statistics()["source_discontinuities"], 1);
    }

    #[test]
    fn turn_boundary_rejects_previous_input_and_preserves_onset_during_context_query() {
        let commit = Instant::now();
        let query_finished = commit + Duration::from_millis(200);
        let mut gate = BargeInGate::with_noise_since(&InputNoise::default(), commit);
        let mut input = frame(&voiced(900));
        for ms in [120, 100, 80, 60, 40, 20] {
            input.captured = commit - Duration::from_millis(ms);
            let observed = gate.observe_at(&input, commit);
            assert!(!observed.interrupted && !observed.retain_frame);
        }
        // 160 ms of quiet after the commit arms the gate; the onset that
        // follows during the context query still interrupts.
        for index in 0..8 {
            let mut silence = frame(&[0; 480]);
            silence.captured = commit + Duration::from_millis(index * 20);
            let observed = gate.observe_at(&silence, query_finished);
            assert!(!observed.interrupted && !observed.retain_frame);
        }
        for index in 0..10 {
            input.captured = commit + Duration::from_millis(160 + index * 20);
            let observed = gate.observe_at(&input, input.captured);
            assert_eq!(observed.interrupted, index == 9);
            assert!(observed.retain_frame);
        }
        assert_eq!(gate.statistics()["rejected_source_frames"], 6);
        assert_eq!(gate.statistics()["tail_frames"], 8);
    }

    #[test]
    fn committed_utterance_tail_never_cancels_its_own_plan() {
        // A semantic end of turn before the acoustic end: the voice keeps
        // going straight across the commit, then fades. Only a pause arms.
        let commit = Instant::now();
        let mut gate = BargeInGate::with_noise_since(&calibrated_noise(0.0003), commit);
        for index in 0..30 {
            let mut tail = voiced_at_level(5, 0.015);
            tail.captured = commit + Duration::from_millis(index * 20);
            let observed = gate.observe_at(&tail, tail.captured);
            assert!(!observed.interrupted && !observed.retain_frame && observed.reset_buffer);
        }
        assert_eq!(gate.statistics()["max_voiced_frames"], 0);
        // Short gaps inside the tail do not arm it either.
        for index in 0..5 {
            let mut gap = frame(&[0; 480]);
            gap.captured = commit + Duration::from_millis(600 + index * 20);
            assert!(!gate.observe_at(&gap, gap.captured).interrupted);
        }
        for index in 0..20 {
            let mut tail = voiced_at_level(6, 0.01);
            tail.captured = commit + Duration::from_millis(700 + index * 20);
            assert!(!gate.observe_at(&tail, tail.captured).interrupted);
        }
        // A real pause, then new speech: interrupts on its tenth frame.
        for index in 0..8 {
            let mut gap = frame(&[0; 480]);
            gap.captured = commit + Duration::from_millis(1100 + index * 20);
            assert!(!gate.observe_at(&gap, gap.captured).interrupted);
        }
        for index in 0..10 {
            let mut speech = voiced_at_level(7, 0.03);
            speech.captured = commit + Duration::from_millis(1260 + index * 20);
            let observed = gate.observe_at(&speech, speech.captured);
            assert_eq!(observed.interrupted, index == 9);
            assert!(observed.retain_frame);
        }
    }

    fn harmonic_frame(harmonics: [f64; 3], amplitude: f64) -> Vec<i16> {
        (0..480)
            .map(|index| {
                let phase = std::f64::consts::TAU * 150.0 * index as f64 / RATE as f64;
                (amplitude
                    * (harmonics[0] * phase.sin()
                        + harmonics[1] * (phase * 2.0).sin()
                        + harmonics[2] * (phase * 3.0).sin())) as i16
            })
            .collect()
    }

    #[test]
    fn reordered_same_pitch_harmonics_are_not_contiguous_echo() {
        let vowels = [
            [1.0, 0.9, 0.7],
            [1.0, -0.9, 0.7],
            [1.0, 0.9, -0.7],
            [1.0, -0.9, -0.7],
        ];
        let mut reference = vec![0; 64];
        for index in 0..100 {
            reference.extend(harmonic_frame(vowels[index % 4], 2500.0));
        }
        let mut gate = BargeInGate::with_noise(&calibrated_noise(0.0003));
        let mut interrupted = false;
        for index in 0..12 {
            let input = frame(&harmonic_frame(vowels[[0, 2, 1, 3][index % 4]], 1500.0));
            // Every short frame matches somewhere in the output, yet this
            // reordered, continuous voiced sequence was never rendered.
            assert!(echo_matches(&input.pcm, &reference));
            interrupted |= gate.observe_with_reference(&input, &reference);
        }
        assert!(
            interrupted,
            "independent short-frame matches hid new speech"
        );
        assert_eq!(gate.statistics()["echo_frames"], 0);
    }

    #[test]
    fn nearby_voiced_pitch_cannot_match_each_frame_at_unrelated_render_offsets() {
        let voiced = |hz: f64, harmonics: [f64; 3], phase: f64, samples: usize, gain: f64| {
            (0..samples)
                .map(|index| {
                    let angle = phase + std::f64::consts::TAU * hz * index as f64 / RATE as f64;
                    (gain
                        * (harmonics[0] * angle.sin()
                            + harmonics[1] * (2.0 * angle).sin()
                            + harmonics[2] * (3.0 * angle).sin())) as i16
                })
                .collect::<Vec<_>>()
        };
        let reference = voiced(160.0, [1.0, 0.3, 0.1], 0.0, RATE as usize * 3, 2500.0);
        let input = voiced(170.0, [1.0, -0.25, 0.2], 0.7, 480 * 12, 1500.0);
        let mut gate = BargeInGate::with_noise(&calibrated_noise(0.0003));
        let mut interrupted = false;
        for samples in input.chunks_exact(480) {
            let input = frame(samples);
            assert!(echo_matches(&input.pcm, &reference));
            interrupted |= gate.observe_with_reference(&input, &reference);
        }
        assert!(interrupted);
        assert_eq!(gate.statistics()["echo_frames"], 0);
    }

    #[test]
    fn continuous_delayed_noisy_echo_is_rejected_before_an_interruption() {
        let mut phase = 0.0_f64;
        let output = (0..480 * 20)
            .map(|index| {
                phase += std::f64::consts::TAU * (120.0 + index as f64 / 100.0) / RATE as f64;
                (5000.0 * (phase.sin() + 0.25 * (2.0 * phase).sin())) as i16
            })
            .collect::<Vec<_>>();
        let mut reference = vec![0; 64];
        reference.extend_from_slice(&output);
        reference.extend_from_slice(&[0; 96]);
        let mut gate = BargeInGate::with_noise(&calibrated_noise(0.0003));
        for (index, chunk) in output.chunks_exact(480).enumerate() {
            let echoed = frame(
                &chunk
                    .iter()
                    .enumerate()
                    .map(|(sample_index, sample)| -sample / 8 + (sample_index % 3) as i16 - 1)
                    .collect::<Vec<_>>(),
            );
            assert!(!gate.observe_with_reference(&echoed, &reference));
            assert_eq!(gate.echo_detected(), index >= 4);
        }
        assert_eq!(gate.statistics()["max_voiced_frames"], 4);
        // A new voice must not be masked by the preceding 80 ms of true echo.
        for index in 0..10 {
            let new_voice = voiced_at_level(300 + index, 0.003);
            assert_eq!(
                gate.observe_with_reference(&new_voice, &reference),
                index == 9
            );
            assert!(!gate.echo_detected());
        }
    }

    #[test]
    fn calibrated_soft_syllables_interrupt_across_a_short_gap() {
        let noise = calibrated_noise(0.0003);
        let mut gate = BargeInGate::with_noise(&noise);
        assert!((gate.threshold - 0.002).abs() < f32::EPSILON);
        // Two soft voiced syllables (speech-band levels) 40 ms apart. These
        // are synthetic levels, not a recording or a claimed utterance.
        let envelope = [
            0.003, 0.005, 0.006, 0.004, 0.003, 0.0004, 0.0004, 0.003, 0.005, 0.006, 0.005, 0.004,
        ];
        for (index, level) in envelope.into_iter().enumerate() {
            let frame = voiced_at_level(1, level);
            assert!((speech_band_level(&frame.pcm) - level).abs() < 0.00002);
            assert_eq!(gate.observe_with_reference(&frame, &[]), index == 11);
        }
        assert_eq!(gate.statistics()["max_voiced_frames"], 10);
    }

    #[test]
    fn calibrated_noise_floor_still_rejects_background_and_isolated_clicks() {
        // Quiet and loud microphones must retain the same relative noise
        // rejection; reducing the absolute floor must not flatten this margin.
        for floor in [0.0002, 0.0006, 0.002, 0.02] {
            let noise = calibrated_noise(floor);
            let mut gate = BargeInGate::with_noise(&noise);
            assert!(gate.threshold >= floor * 2.6);
            for index in 0..100 {
                let background =
                    signal_at_level(index + 500, floor * (1.0 + (index % 4) as f32 * 0.15));
                assert!(!gate.observe_with_reference(&background, &[]));
            }
            for index in 0..30 {
                let level = if index % 7 == 0 {
                    gate.threshold * 4.0
                } else {
                    floor
                };
                assert!(!gate.observe_with_reference(&signal_at_level(index + 700, level), &[]));
            }
            assert!(gate.statistics()["max_voiced_frames"].as_u64().unwrap() <= 3);
            gate.reset();
            for index in 0..10 {
                let voice = voiced_at_level(index + 800, gate.threshold * 1.5);
                assert_eq!(gate.observe_with_reference(&voice, &[]), index == 9);
            }
        }
    }

    #[test]
    fn insufficient_noise_calibration_keeps_the_conservative_fallback() {
        let low_voice = voiced_at_level(5, 0.003);
        let mut noise = InputNoise::default();
        for _ in 0..9 {
            noise.observe(0.0003);
        }
        let mut gate = BargeInGate::with_noise(&noise);
        assert_eq!(gate.threshold, 0.006);
        for _ in 0..20 {
            assert!(!gate.observe_with_reference(&low_voice, &[]));
        }
        noise.observe(0.0003);
        let mut calibrated = BargeInGate::with_noise(&noise);
        assert_eq!(calibrated.threshold, 0.002);
        for index in 0..10 {
            assert_eq!(
                calibrated.observe_with_reference(&low_voice, &[]),
                index == 9
            );
        }
    }

    #[test]
    fn calibrated_low_gain_echo_stays_rejected_near_the_floor() {
        let noise = calibrated_noise(0.0003);
        let mut gate = BargeInGate::with_noise(&noise);
        let output = voice(27);
        let mut reference = vec![0; 64];
        reference.extend_from_slice(&output.repeat(20));
        reference.extend_from_slice(&[0; 96]);
        let echoed = signal_at_level(27, 0.003);
        assert!(speech_band_level(&echoed.pcm) > gate.threshold && echoed.level < 0.004);
        assert!(echo_matches(&echoed.pcm, &reference));
        for index in 0..20 {
            assert!(!gate.observe_with_reference(&echoed, &reference));
            assert_eq!(gate.echo_detected(), index >= 4);
        }
        assert_eq!(gate.statistics()["echo_frames"], 16);
        assert_eq!(gate.statistics()["max_voiced_frames"], 4);
        let new_voice = voiced_at_level(42, 0.003);
        for index in 0..10 {
            assert_eq!(
                gate.observe_with_reference(&new_voice, &reference),
                index == 9
            );
        }
    }
    #[test]
    fn sustained_new_voice_interrupts_but_clicks_and_silence_do_not() {
        let voice = frame(&voiced(1));
        let quiet = frame(&[0; 480]);
        let mut gate = BargeInGate::default();
        assert!(!gate.observe_with_reference(&voice, &[]));
        for _ in 0..20 {
            assert!(!gate.observe_with_reference(&quiet, &[]));
        }
        for _ in 0..9 {
            assert!(!gate.observe_with_reference(&voice, &[]));
        }
        assert!(gate.observe_with_reference(&voice, &[]));
        gate.reset();
        assert!(!gate.observe_with_reference(&voice, &[]));
    }
    #[test]
    fn low_gain_speech_can_interrupt_without_raising_the_microphone_volume() {
        let voice = voiced_at_level(1, 0.009);
        assert!(voice.level > 0.006 && voice.level < 0.025);
        let mut gate = BargeInGate::default();
        for _ in 0..9 {
            assert!(!gate.observe_with_reference(&voice, &[]));
        }
        assert!(gate.observe_with_reference(&voice, &[]));
    }
    #[test]
    fn syllable_gaps_do_not_erase_a_sustained_interruption() {
        let voice = frame(&voiced(1));
        let quiet = frame(&[0; 480]);
        let mut gate = BargeInGate::default();
        for _ in 0..5 {
            assert!(!gate.observe_with_reference(&voice, &[]));
        }
        for _ in 0..2 {
            assert!(!gate.observe_with_reference(&quiet, &[]));
        }
        for _ in 0..4 {
            assert!(!gate.observe_with_reference(&voice, &[]));
        }
        assert!(gate.observe_with_reference(&voice, &[]));
    }
    #[test]
    fn calibration_rejects_background_and_outliers_without_losing_quiet_speech() {
        let background = frame(
            &voice(19)
                .iter()
                .map(|sample| sample / 256)
                .collect::<Vec<_>>(),
        );
        let speech = voiced_at_level(1, 0.009);
        let mut noise = InputNoise::default();
        for _ in 0..49 {
            noise.observe(speech_band_level(&background.pcm));
        }
        noise.observe(0.9); // A click must not desensitize the next reply.
        for invalid in [f32::NAN, f32::INFINITY, -1.0] {
            noise.observe(invalid);
        }
        let mut gate = BargeInGate::with_noise(&noise);
        for _ in 0..50 {
            assert!(!gate.observe_with_reference(&background, &[]));
        }
        for _ in 0..9 {
            assert!(!gate.observe_with_reference(&speech, &[]));
        }
        assert!(gate.observe_with_reference(&speech, &[]));

        // The same quiet absolute threshold cannot be used in a louder room.
        let background = frame(
            &voice(19)
                .iter()
                .map(|sample| sample / 16)
                .collect::<Vec<_>>(),
        );
        let mut noise = InputNoise::default();
        for _ in 0..50 {
            noise.observe(speech_band_level(&background.pcm));
        }
        let mut gate = BargeInGate::with_noise(&noise);
        for _ in 0..50 {
            assert!(!gate.observe_with_reference(&background, &[]));
        }
        for _ in 0..20 {
            assert!(
                !gate.observe_with_reference(&speech, &[]),
                "quiet speech below the louder room's margin"
            );
        }
        gate.reset();
        let speech = voiced_at_level(1, 0.15);
        for _ in 0..9 {
            assert!(!gate.observe_with_reference(&speech, &[]));
        }
        assert!(gate.observe_with_reference(&speech, &[]));
    }
    #[test]
    fn echoed_output_with_delay_and_gain_does_not_interrupt() {
        let output = voice(7);
        let mut reference = vec![0; 64];
        reference.extend_from_slice(&output.repeat(20));
        reference.extend_from_slice(&[0; 96]);
        let echoed = frame(&output.iter().map(|sample| -sample / 3).collect::<Vec<_>>());
        assert!(echo_matches(&echoed.pcm, &reference));
        let mut gate = BargeInGate::default();
        for index in 0..20 {
            assert!(!gate.observe_with_reference(&echoed, &reference));
            assert_eq!(gate.echo_detected(), index >= 4);
        }
        let new_voice = frame(&voiced(42));
        for _ in 0..9 {
            assert!(!gate.observe_with_reference(&new_voice, &reference));
        }
        assert!(gate.observe_with_reference(&new_voice, &reference));
        assert!(!gate.echo_detected());

        // Lower microphone gain must not bypass echo rejection when the gate
        // is made sensitive enough to hear a quiet interruption.
        let echoed = frame(&output.iter().map(|sample| -sample / 32).collect::<Vec<_>>());
        let mut gate = BargeInGate::default();
        for index in 0..20 {
            assert!(!gate.observe_with_reference(&echoed, &reference));
            assert_eq!(gate.echo_detected(), index >= 4);
        }
        let new_voice = voiced_at_level(42, 0.009);
        for _ in 0..9 {
            assert!(!gate.observe_with_reference(&new_voice, &reference));
        }
        assert!(gate.observe_with_reference(&new_voice, &reference));
    }
    #[test]
    fn speech_band_level_removes_rumble_and_keeps_formants() {
        let tone = |hz: f64, rms: f64| {
            frame(
                &(0..480)
                    .map(|index| {
                        let angle = std::f64::consts::TAU * hz * index as f64 / f64::from(RATE);
                        (rms * std::f64::consts::SQRT_2 * 32768.0 * angle.sin()) as i16
                    })
                    .collect::<Vec<_>>(),
            )
        };
        for hz in [40.0, 60.0, 100.0] {
            assert!(
                speech_band_level(&tone(hz, 0.1).pcm) < 0.01,
                "{hz} Hz rumble"
            );
        }
        for hz in [600.0, 1000.0, 2500.0] {
            let level = speech_band_level(&tone(hz, 0.1).pcm);
            assert!((level - 0.1).abs() < 0.006, "{hz} Hz formant level {level}");
        }
    }
    #[test]
    fn broadband_noise_and_traffic_rumble_do_not_interrupt_but_a_voice_does() {
        let noise = calibrated_noise(0.0003);
        // Loud aperiodic noise (fans, typing hiss, breath) has no pitch.
        let mut gate = BargeInGate::with_noise(&noise);
        for index in 0..50 {
            let burst = signal_at_level(index + 1000, 0.05);
            assert!(speech_band_level(&burst.pcm) > gate.threshold * 10.0);
            assert!(
                !gate.observe_with_reference(&burst, &[]),
                "white noise frame {index}"
            );
        }
        assert_eq!(gate.statistics()["max_pitched_frames"], 0);
        // A passing vehicle: strong energy below the speech band, like the
        // live 0.075 rumble that interrupted a reply before this gate.
        let mut gate = BargeInGate::with_noise(&noise);
        let rumble: Vec<i16> = (0..480 * 50)
            .map(|index| {
                let t = index as f64 / f64::from(RATE);
                let hum = (std::f64::consts::TAU * 45.0 * t).sin()
                    + 0.6 * (std::f64::consts::TAU * 70.0 * t + 1.0).sin()
                    + 0.3 * (std::f64::consts::TAU * 110.0 * t + 2.0).sin();
                (hum * 0.075 / 0.85 * 32768.0) as i16 + voice(index as u32)[index % 480] / 400
            })
            .collect();
        for chunk in rumble.chunks_exact(480) {
            let input = frame(chunk);
            assert!(input.level > 0.02);
            assert!(!gate.observe_with_reference(&input, &[]));
        }
        // An ordinary voice at a fraction of that level still interrupts.
        let mut gate = BargeInGate::with_noise(&noise);
        for index in 0..10 {
            let speech = voiced_at_level(7, 0.02);
            assert_eq!(gate.observe_with_reference(&speech, &[]), index == 9);
        }
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
