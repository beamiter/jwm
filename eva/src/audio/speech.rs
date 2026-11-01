//! Local neural speech evidence. This runs on the ordinary consumer thread,
//! never in PipeWire/PulseAudio's realtime capture callback.
//!
//! RNNoise's embedded model sees 48 kHz PCM. A pair of stateful, band-limited
//! FIR resamplers adapts Eva's 24 kHz stream without introducing the images or
//! aliasing of sample duplication/decimation. Each 20 ms input supplies two
//! 10 ms neural decisions; the result is evidence for the *current input*.
//!
//! Denoised PCM is also returned for explicit offline evaluation. It is delayed
//! by `OUTPUT_DELAY_SAMPLES` at 24 kHz and must not replace a capture frame
//! without adjusting its provenance. The live gate may use the probability
//! while retaining the original PCM and its native capture timestamp.
use nnnoiseless::DenoiseState;
use realfft::{RealFftPlanner, RealToComplex, num_complex::Complex32};
use std::{
    io,
    sync::{Arc, OnceLock},
};

const INPUT_SAMPLES: usize = 480;
const INPUT_BYTES: usize = INPUT_SAMPLES * 2;
const MODEL_SAMPLES: usize = DenoiseState::FRAME_SIZE;
const FILTER_TAPS: usize = 65;
const SPECTRUM_SAMPLES: usize = 512;
/// RNNoise overlap-add delays output by 480 samples at 48 kHz. The two
/// 65-tap linear-phase FIRs add 32 + 32 samples at 48 kHz: 272 at 24 kHz.
pub const OUTPUT_DELAY_SAMPLES: usize = (MODEL_SAMPLES + FILTER_TAPS - 1) / 2;

/// No transcript, identity, or command authority is inferred here. A TV,
/// another person, and Eva's echoed voice are all possible speech; the caller
/// still needs echo, wake-phrase, provenance, and execution controls.
pub struct SpeechFrame {
    /// Denoised 24 kHz S16LE, exactly 960 bytes; see `delay_samples`.
    pub pcm: Vec<u8>,
    /// Mean of the current two 10 ms RNNoise decisions, after the pure-tone
    /// and broadband-noise veto. The analysis windows end at the current
    /// input, unlike delayed output PCM. This is not speaker confidence.
    pub probability: f32,
    /// Unmodified mean neural probability, for aggregate diagnostics only.
    pub raw_probability: f32,
    /// Fixed reason for a non-speech spectral veto; contains no audio/text.
    pub suppression: Option<&'static str>,
    /// False only for the first 20 ms frame after creation/reset, while the
    /// synthesis history fills. Keep an onset pre-roll even during this frame.
    pub ready: bool,
    /// Fixed output delay in 24 kHz samples; applies to `pcm`, not probability.
    pub delay_samples: usize,
}

/// Per-stream state and embedded neural weights. No filesystem or network
/// access occurs in construction, processing, or reset.
pub struct SpeechDetector {
    denoiser: Box<DenoiseState<'static>>,
    upsample: LowPass,
    downsample: LowPass,
    spectrum: Spectrum,
    frames: u64,
}

impl Default for SpeechDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl SpeechDetector {
    pub fn new() -> Self {
        Self {
            denoiser: DenoiseState::new(),
            upsample: LowPass::default(),
            downsample: LowPass::default(),
            spectrum: Spectrum::new(),
            frames: 0,
        }
    }

    /// Call on capture discontinuity or owner/device change. A different
    /// stream must not inherit recurrent speech evidence or resampler history.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Process one complete native frame. Invalid input or nonfinite model
    /// output resets all state and returns an error: never fall back to an
    /// energy-only decision or silently upload an unclassified frame.
    pub fn process(&mut self, pcm: &[u8]) -> io::Result<SpeechFrame> {
        if pcm.len() != INPUT_BYTES {
            self.reset();
            return Err(io::Error::other("speech_frame_format_invalid"));
        }
        let suppression = match self.spectrum.classify(pcm) {
            Ok(suppression) => suppression,
            Err(error) => {
                self.reset();
                return Err(error);
            }
        };
        let mut input = [0.0; MODEL_SAMPLES * 2];
        for (index, sample) in pcm.chunks_exact(2).enumerate() {
            // The model expects i16-range floats, not normalized [-1, 1].
            let sample = f32::from(i16::from_le_bytes([sample[0], sample[1]]));
            input[index * 2] = self.upsample.push(sample) * 2.0;
            input[index * 2 + 1] = self.upsample.push(0.0) * 2.0;
        }
        let mut output = [0.0; MODEL_SAMPLES * 2];
        let mut probability = 0.0;
        for (input, output) in input
            .chunks_exact(MODEL_SAMPLES)
            .zip(output.chunks_exact_mut(MODEL_SAMPLES))
        {
            probability += self.denoiser.process_frame(output, input) * 0.5;
        }
        if !probability.is_finite() || output.iter().any(|sample| !sample.is_finite()) {
            self.reset();
            return Err(io::Error::other("speech_detector_nonfinite"));
        }
        let mut pcm = Vec::with_capacity(INPUT_BYTES);
        for pair in output.chunks_exact(2) {
            let sample = self.downsample.push(pair[0]);
            self.downsample.push(pair[1]);
            pcm.extend_from_slice(
                &(sample.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16).to_le_bytes(),
            );
        }
        let ready = self.frames > 0;
        self.frames = self.frames.saturating_add(1);
        Ok(SpeechFrame {
            pcm,
            probability: if suppression.is_some() {
                0.0
            } else {
                probability.clamp(0.0, 1.0)
            },
            raw_probability: probability.clamp(0.0, 1.0),
            suppression,
            ready,
            delay_samples: OUTPUT_DELAY_SAMPLES,
        })
    }
}

/// RNNoise's VAD head is useful evidence, but it can assign a high probability
/// to a sine wave or very loud hiss. A narrow spectral veto catches those
/// cases independently of amplitude; it does not try to classify speakers.
/// Speech onsets still need a multi-frame decision and a pre-roll in the gate.
struct Spectrum {
    fft: Arc<dyn RealToComplex<f32>>,
    input: Vec<f32>,
    output: Vec<Complex32>,
    scratch: Vec<Complex32>,
    window: [f32; INPUT_SAMPLES],
}

impl Spectrum {
    fn new() -> Self {
        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(SPECTRUM_SAMPLES);
        let window = std::array::from_fn(|index| {
            let phase = std::f32::consts::TAU * index as f32 / (INPUT_SAMPLES - 1) as f32;
            0.5 * (1.0 - phase.cos())
        });
        Self {
            input: fft.make_input_vec(),
            output: fft.make_output_vec(),
            scratch: fft.make_scratch_vec(),
            fft,
            window,
        }
    }

    fn classify(&mut self, pcm: &[u8]) -> io::Result<Option<&'static str>> {
        let mean = pcm
            .chunks_exact(2)
            .map(|sample| f32::from(i16::from_le_bytes([sample[0], sample[1]])))
            .sum::<f32>()
            / INPUT_SAMPLES as f32;
        for ((output, window), sample) in self.input[..INPUT_SAMPLES]
            .iter_mut()
            .zip(self.window)
            .zip(pcm.chunks_exact(2))
        {
            *output = (f32::from(i16::from_le_bytes([sample[0], sample[1]])) - mean) * window;
        }
        self.input[INPUT_SAMPLES..].fill(0.0);
        self.fft
            .process_with_scratch(&mut self.input, &mut self.output, &mut self.scratch)
            .map_err(|_| io::Error::other("speech_spectrum_invalid"))?;
        let power: [f64; SPECTRUM_SAMPLES / 2 + 1] =
            std::array::from_fn(|index| f64::from(self.output[index].norm_sqr()));
        if power.iter().any(|power| !power.is_finite()) {
            return Err(io::Error::other("speech_spectrum_nonfinite"));
        }
        // A 20 ms Hann window spreads one pure tone over several FFT bins.
        // Requiring 97% of non-DC energy in five neighbouring bins leaves
        // room for normal multi-harmonic sustained vowels.
        let total: f64 = power[1..].iter().sum();
        if total <= 1.0 {
            return Ok(Some("silence"));
        }
        let strongest = power[1..]
            .windows(5)
            .map(|bins| bins.iter().sum::<f64>())
            .fold(0.0, f64::max);
        if strongest / total >= 0.97 {
            return Ok(Some("narrow_tone"));
        }
        // Flatness in approximately 140 Hz–8 kHz. For white noise, the
        // geometric/arithmetic ratio of power is about exp(-gamma)=0.56;
        // voiced speech has harmonic/formant structure and a smaller ratio.
        // Fricatives can also be flat, so this only removes *onset evidence*:
        // pre-roll and the gate's speech hangover retain their actual PCM.
        let band = &power[3..171];
        let arithmetic = band.iter().sum::<f64>() / band.len() as f64;
        if arithmetic > 1.0 {
            let geometric = (band.iter().map(|value| value.max(1e-12).ln()).sum::<f64>()
                / band.len() as f64)
                .exp();
            if geometric / arithmetic >= 0.45 {
                return Ok(Some("broadband_noise"));
            }
        }
        Ok(None)
    }
}

/// Blackman-windowed sinc at 10.8 kHz (48 kHz domain). The transition band
/// leaves speech bandwidth intact while rejecting images/aliases above the
/// 24 kHz stream's Nyquist limit. Coefficients are immutable and shared; all
/// sample history is private to one detector.
fn coefficients() -> &'static [f32; FILTER_TAPS] {
    static COEFFICIENTS: OnceLock<[f32; FILTER_TAPS]> = OnceLock::new();
    COEFFICIENTS.get_or_init(|| {
        let mut taps = [0.0; FILTER_TAPS];
        let middle = (FILTER_TAPS - 1) as f64 / 2.0;
        let cutoff = 10_800.0 / 48_000.0;
        for (index, tap) in taps.iter_mut().enumerate() {
            let offset = index as f64 - middle;
            let ideal = if offset == 0.0 {
                2.0 * cutoff
            } else {
                (std::f64::consts::TAU * cutoff * offset).sin() / (std::f64::consts::PI * offset)
            };
            let phase = std::f64::consts::TAU * index as f64 / (FILTER_TAPS - 1) as f64;
            let window = 0.42 - 0.5 * phase.cos() + 0.08 * (2.0 * phase).cos();
            *tap = (ideal * window) as f32;
        }
        let gain: f32 = taps.iter().sum();
        for tap in &mut taps {
            *tap /= gain;
        }
        taps
    })
}

struct LowPass {
    history: [f32; FILTER_TAPS],
    next: usize,
}

impl Default for LowPass {
    fn default() -> Self {
        Self {
            history: [0.0; FILTER_TAPS],
            next: 0,
        }
    }
}

impl LowPass {
    fn push(&mut self, sample: f32) -> f32 {
        self.history[self.next] = sample;
        let mut result = 0.0;
        let mut at = self.next;
        for coefficient in coefficients() {
            result += coefficient * self.history[at];
            at = if at == 0 { FILTER_TAPS - 1 } else { at - 1 };
        }
        self.next = (self.next + 1) % FILTER_TAPS;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(samples: impl Iterator<Item = i16>) -> Vec<u8> {
        samples.flat_map(i16::to_le_bytes).collect()
    }

    fn white_noise(seed: &mut u32, amplitude: i16) -> Vec<u8> {
        frame((0..INPUT_SAMPLES).map(|_| {
            *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((*seed >> 16) as i16 as f32 * f32::from(amplitude) / 32768.0) as i16
        }))
    }

    /// Exercise the default onset contract on actual neural scores, not
    /// synthetic booleans: 160 ms of speech evidence within a 240 ms window.
    fn first_onset(pcm: &[u8]) -> Option<usize> {
        let mut detector = SpeechDetector::new();
        let mut window = 0u16;
        for (index, frame) in pcm.chunks_exact(INPUT_BYTES).enumerate() {
            let result = detector.process(frame).unwrap();
            assert!(result.raw_probability.is_finite());
            assert!((0.0..=1.0).contains(&result.probability));
            window =
                ((window << 1) | u16::from(result.ready && result.probability >= 0.65)) & 0x0fff;
            if window.count_ones() >= 8 {
                return Some((index + 1) * 20);
            }
        }
        None
    }

    #[test]
    fn malformed_frames_reset_neural_and_filter_history() {
        let mut detector = SpeechDetector::new();
        let silent = vec![0; INPUT_BYTES];
        assert!(!detector.process(&silent).unwrap().ready);
        assert!(detector.process(&silent).unwrap().ready);
        for len in [0, 1, INPUT_BYTES - 1, INPUT_BYTES + 2] {
            assert!(detector.process(&vec![0; len]).is_err());
            assert!(!detector.process(&silent).unwrap().ready);
        }
    }

    #[test]
    fn silence_never_becomes_voice_and_has_exact_bounded_output() {
        let mut detector = SpeechDetector::new();
        let silent = vec![0; INPUT_BYTES];
        for _ in 0..100 {
            let result = detector.process(&silent).unwrap();
            assert_eq!(result.pcm, silent);
            assert_eq!(result.probability, 0.0);
            assert_eq!(result.delay_samples, 272);
        }
    }

    #[test]
    fn resampler_preserves_speech_band_and_rejects_aliases() {
        fn response(hz: f64) -> f64 {
            let mut filter = LowPass::default();
            let mut energy = 0.0;
            for index in 0..4_800 {
                let input = (std::f64::consts::TAU * hz * index as f64 / 48_000.0).sin();
                let output = filter.push(input as f32);
                if index >= 480 {
                    energy += f64::from(output).powi(2);
                }
            }
            (energy / 4_320.0 * 2.0).sqrt()
        }
        for hz in [150.0, 1_000.0, 4_000.0, 8_000.0] {
            assert!((response(hz) - 1.0).abs() < 0.005, "{hz} Hz");
        }
        for hz in [13_000.0, 16_000.0, 20_000.0] {
            assert!(response(hz) < 0.001, "{hz} Hz aliases into speech");
        }
    }

    #[test]
    fn filter_state_is_continuous_and_reset_isolates_recordings() {
        let mut detector = SpeechDetector::new();
        let mut seed = 17;
        for _ in 0..20 {
            detector.process(&white_noise(&mut seed, 6_000)).unwrap();
        }
        detector.reset();
        let after_reset = detector.process(&vec![0; INPUT_BYTES]).unwrap();
        let fresh = SpeechDetector::new()
            .process(&vec![0; INPUT_BYTES])
            .unwrap();
        assert_eq!(after_reset.pcm, fresh.pcm);
        assert_eq!(after_reset.probability, fresh.probability);
        assert!(!after_reset.ready);
    }

    #[test]
    fn embedded_model_keeps_short_quiet_speech() {
        let recording = include_bytes!("../../tests/fixtures/rnnoise-speech.s16le");
        // A 600 ms excerpt with continuous speech. No optional downloader,
        // external speech service, real microphone, or confidence stub.
        let short = &recording[28_800..57_600];
        for gain in [1.0, 0.5, 0.2, 0.1] {
            let quiet = frame(short.chunks_exact(2).map(|sample| {
                (f32::from(i16::from_le_bytes([sample[0], sample[1]])) * gain) as i16
            }));
            let onset = first_onset(&quiet).expect("a short quiet phrase must remain detectable");
            assert!(onset <= 400, "gain={gain}, onset={onset}");
        }
    }

    #[test]
    fn loud_stationary_noise_and_tones_never_supply_sustained_voice_evidence() {
        for amplitude in [500, 3_000, 10_000] {
            let mut seed = 17;
            let pcm: Vec<u8> = (0..100)
                .flat_map(|_| white_noise(&mut seed, amplitude))
                .collect();
            assert_eq!(first_onset(&pcm), None, "hiss amplitude={amplitude}");
        }
        for hz in [60.0, 200.0, 440.0, 1_000.0, 6_000.0] {
            let pcm = frame((0..48_000).map(|index| {
                let phase = std::f64::consts::TAU * hz * index as f64 / 24_000.0;
                (5_000.0 * phase.sin()) as i16
            }));
            assert_eq!(first_onset(&pcm), None, "pure {hz} Hz tone");
        }
    }

    #[test]
    fn fan_hum_key_clicks_and_dual_tones_do_not_open_a_turn() {
        for kind in ["fan", "keys", "dual"] {
            let mut seed = 17u32;
            let mut low_pass = 0.0;
            let pcm = frame((0..48_000).map(|index| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let white = f64::from((seed >> 16) as i16) / 32_768.0;
                let t = index as f64 / 24_000.0;
                let sample = match kind {
                    "fan" => {
                        low_pass = 0.85 * low_pass + 0.15 * white;
                        12_000.0 * low_pass + 2_500.0 * (std::f64::consts::TAU * 120.0 * t).sin()
                    }
                    "keys" => {
                        let since_click = index % 3_840;
                        if since_click < 960 {
                            20_000.0 * white * (-(since_click as f64) / 100.0).exp()
                        } else {
                            0.0
                        }
                    }
                    _ => {
                        4_000.0
                            * ((std::f64::consts::TAU * 697.0 * t).sin()
                                + (std::f64::consts::TAU * 1_209.0 * t).sin())
                    }
                };
                sample.clamp(-32_768.0, 32_767.0) as i16
            }));
            assert_eq!(first_onset(&pcm), None, "{kind}");
        }
    }

    #[test]
    fn sustained_multi_harmonic_vowel_is_not_mistaken_for_a_beep() {
        // Clearly synthetic source-filter vowel: a moving glottal pitch and
        // three broad formants. This guards against a "reject all pitched
        // sound" regression, not a claim about every speaker or language.
        let mut phase = 0.0;
        let pcm = frame((0..24_000).map(|index| {
            let t = index as f64 / 24_000.0;
            let f0 = 145.0 + 5.0 * (std::f64::consts::TAU * 3.0 * t).sin();
            phase += std::f64::consts::TAU * f0 / 24_000.0;
            let mut sample = 0.0;
            for harmonic in 1..50 {
                let frequency = f64::from(harmonic) * f0;
                let formants: f64 = [(750.0, 120.0), (1_200.0, 150.0), (2_600.0, 220.0)]
                    .into_iter()
                    .map(|(centre, width)| (-0.5 * ((frequency - centre) / width).powi(2)).exp())
                    .sum();
                sample +=
                    18_000.0 * formants / f64::from(harmonic) * (f64::from(harmonic) * phase).sin();
            }
            sample.clamp(-32_768.0, 32_767.0) as i16
        }));
        let onset = first_onset(&pcm).expect("harmonic vowels remain speech candidates");
        assert!(onset <= 500, "onset={onset}");
    }
}
