//! Silero VAD v5.1.2, CPU ONNX Runtime, entirely local. Eva's 24 kHz S16LE
//! capture is band-limited to 16 kHz and accumulated into exact 512-sample
//! windows. The model also consumes the preceding 64 samples and recurrent
//! state [2, 1, 128], as specified by the pinned upstream streaming wrapper.
//! Only probability is used; original capture PCM/timestamps remain untouched.
use super::local_voice::Detection;
use libloading::Library;
use ort::{
    session::{Session, builder::GraphOptimizationLevel},
    value::Tensor,
};
use sha2::{Digest, Sha256};
use std::{collections::VecDeque, io, path::Path, sync::OnceLock};

pub const MODEL_SHA256: &str = "2623a2953f6ff3d2c1e61740c6cdb7168133479b267dfef114a4a3cc5bdd788f";
const MODEL: &[u8] = include_bytes!("../../models/silero-v5.1.2/silero_vad.onnx");
const WINDOW: usize = 512;
const CONTEXT: usize = 64;
const TAPS: usize = 63;

/// Retain the library for the process lifetime: ORT sessions and its global
/// environment contain function pointers into it. A failed load is cached;
/// installing/changing the runtime requires an Eva restart.
fn initialize_runtime() -> Result<(), &'static str> {
    static RUNTIME: OnceLock<Result<Library, &'static str>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            let path = std::env::var_os("ORT_DYLIB_PATH");
            // SAFETY: this is an explicitly installed native runtime. No audio,
            // remote download or model-supplied path reaches the library loader.
            let library = unsafe {
                if let Some(path) = path {
                    Library::new(path)
                } else {
                    Library::new("libonnxruntime.so")
                        .or_else(|_| Library::new("libonnxruntime.so.1"))
                }
            }
            .map_err(|_| "silero_runtime_unavailable")?;
            // Preflight the C API ourselves. ort's default loader panics for a
            // missing/old library; those are ordinary readiness failures here.
            // SAFETY: OrtGetApiBase has this ABI in ONNX Runtime. All pointers are
            // checked before dereferencing, and the owning Library is kept above.
            unsafe {
                let getter = library
                    .get::<unsafe extern "C" fn() -> *const ort::sys::OrtApiBase>(
                        b"OrtGetApiBase\0",
                    )
                    .map_err(|_| "silero_runtime_incompatible")?;
                let base = getter();
                if base.is_null() {
                    return Err("silero_runtime_incompatible");
                }
                let api = ((*base).GetApi)(ort::sys::ORT_API_VERSION);
                if api.is_null() {
                    return Err("silero_runtime_incompatible");
                }
                ort::set_api(api.read());
            }
            Ok(library)
        })
        .as_ref()
        .map_err(|reason| *reason)?;
    ort::init()
        .with_telemetry(false)
        .commit()
        .map_err(|_| "silero_runtime_initialization_failed")?;
    Ok(())
}

fn verify_model(model: &[u8]) -> Result<(), &'static str> {
    let hash: String = Sha256::digest(model)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if model.len() != MODEL.len() || hash != MODEL_SHA256 {
        return Err("silero_model_checksum_mismatch");
    }
    Ok(())
}

pub struct SileroDetector {
    session: Session,
    resampler: Resampler,
    pending: VecDeque<f32>,
    state: [f32; 256],
    context: [f32; CONTEXT],
    probability: f32,
    ready: bool,
}

impl SileroDetector {
    pub fn new(path: Option<&Path>) -> Result<Self, &'static str> {
        let model = if let Some(path) = path {
            // A configured FIFO/device is a readiness error, not permission
            // to block the session actor waiting for a writer. Check the
            // opened descriptor (not a racy pre-open metadata lookup).
            use std::os::unix::fs::OpenOptionsExt;
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(path)
                .map_err(|_| "silero_model_unavailable")?;
            let metadata = file.metadata().map_err(|_| "silero_model_unavailable")?;
            if !metadata.is_file() {
                return Err("silero_model_invalid_file");
            }
            if metadata.len() != MODEL.len() as u64 {
                return Err("silero_model_checksum_mismatch");
            }
            use std::io::Read;
            let mut bytes = Vec::with_capacity(MODEL.len());
            file.take(MODEL.len() as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| "silero_model_unavailable")?;
            verify_model(&bytes)?;
            bytes
        } else {
            static VERIFIED: OnceLock<Result<(), &'static str>> = OnceLock::new();
            VERIFIED
                .get_or_init(|| verify_model(MODEL))
                .as_ref()
                .map_err(|reason| *reason)?;
            MODEL.to_vec()
        };
        initialize_runtime()?;
        let session = Session::builder()
            .and_then(|b| b.with_intra_threads(1))
            .and_then(|b| b.with_inter_threads(1))
            .and_then(|b| b.with_optimization_level(GraphOptimizationLevel::Level3))
            .and_then(|b| b.commit_from_memory(&model))
            .map_err(|_| "silero_model_load_failed")?;
        Ok(Self {
            session,
            resampler: Resampler::default(),
            pending: VecDeque::with_capacity(WINDOW * 2),
            state: [0.0; 256],
            context: [0.0; CONTEXT],
            probability: 0.0,
            ready: false,
        })
    }

    pub fn reset(&mut self) {
        self.resampler.reset();
        self.pending.iter_mut().for_each(|sample| *sample = 0.0);
        self.pending.clear();
        self.state.fill(0.0);
        self.context.fill(0.0);
        self.probability = 0.0;
        self.ready = false;
    }

    pub fn process(&mut self, pcm: &[u8]) -> io::Result<Detection> {
        let result = self.process_inner(pcm);
        if result.is_err() {
            self.reset();
        }
        result
    }

    fn process_inner(&mut self, pcm: &[u8]) -> io::Result<Detection> {
        if pcm.len() != 960 {
            return Err(io::Error::other("speech_frame_format_invalid"));
        }
        self.resampler.process(pcm, &mut self.pending);
        while self.pending.len() >= WINDOW {
            let mut input = Vec::with_capacity(CONTEXT + WINDOW);
            input.extend_from_slice(&self.context);
            input.extend(self.pending.drain(..WINDOW));
            let error = |_| io::Error::other("silero_inference_failed");
            let outputs = self.session.run(ort::inputs! {
                "input" => Tensor::from_array(([1usize, CONTEXT + WINDOW], input.clone())).map_err(error)?,
                "state" => Tensor::from_array(([2usize, 1, 128], self.state.to_vec())).map_err(error)?,
                "sr" => Tensor::from_array(([0usize; 0], vec![16_000i64])).map_err(error)?,
            }).map_err(error)?;
            let (_, probability) = outputs
                .get("output")
                .ok_or_else(|| io::Error::other("silero_output_invalid"))?
                .try_extract_tensor::<f32>()
                .map_err(error)?;
            let (_, state) = outputs
                .get("stateN")
                .ok_or_else(|| io::Error::other("silero_output_invalid"))?
                .try_extract_tensor::<f32>()
                .map_err(error)?;
            if probability.len() != 1
                || state.len() != 256
                || !probability[0].is_finite()
                || !(0.0..=1.0).contains(&probability[0])
                || state.iter().any(|x| !x.is_finite())
            {
                return Err(io::Error::other("silero_output_invalid"));
            }
            self.probability = probability[0];
            self.state.copy_from_slice(state);
            self.context
                .copy_from_slice(&input[input.len() - CONTEXT..]);
            self.ready = true;
        }
        // A 20 ms native frame yields 320 samples; reuse the latest decision
        // between 32 ms model boundaries (less than one model window old). Never zero-pad a
        // live partial window or count the 64-sample context as new speech.
        Ok(Detection {
            probability: self.probability,
            ready: self.ready,
        })
    }
}

/// Rational 2/3 conversion: interpolate at 48 kHz, a normalized windowed-sinc
/// FIR removes images and frequencies above 8 kHz, then decimate by 3. Phase
/// and history survive arbitrary native frame boundaries; delay is 31/48000 s.
struct Resampler {
    coefficients: [f32; TAPS],
    history: [f32; TAPS],
    cursor: usize,
    phase: usize,
}
impl Default for Resampler {
    fn default() -> Self {
        let mut coefficients = std::array::from_fn(|index| {
            let x = index as f32 - (TAPS / 2) as f32;
            let cutoff = 0.15;
            let sinc = if x == 0.0 {
                2.0 * cutoff
            } else {
                (std::f32::consts::TAU * cutoff * x).sin() / (std::f32::consts::PI * x)
            };
            let phase = std::f32::consts::TAU * index as f32 / (TAPS - 1) as f32;
            sinc * (0.42 - 0.5 * phase.cos() + 0.08 * (2.0 * phase).cos())
        });
        let sum: f32 = coefficients.iter().sum();
        coefficients.iter_mut().for_each(|x| *x /= sum);
        Self {
            coefficients,
            history: [0.0; TAPS],
            cursor: 0,
            phase: 0,
        }
    }
}
impl Resampler {
    fn reset(&mut self) {
        self.history.fill(0.0);
        self.cursor = 0;
        self.phase = 0;
    }
    fn process(&mut self, pcm: &[u8], output: &mut VecDeque<f32>) {
        for bytes in pcm.chunks_exact(2) {
            let sample = f32::from(i16::from_le_bytes([bytes[0], bytes[1]])) / 32768.0;
            for sample in [sample * 2.0, 0.0] {
                self.history[self.cursor] = sample;
                if self.phase == 0 {
                    let mut value = 0.0;
                    for (index, coefficient) in self.coefficients.iter().enumerate() {
                        value += coefficient * self.history[(self.cursor + TAPS - index) % TAPS];
                    }
                    output.push_back(value);
                }
                self.cursor = (self.cursor + 1) % TAPS;
                self.phase = (self.phase + 1) % 3;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bundled_model_is_pinned_and_modified_files_are_rejected() {
        verify_model(MODEL).unwrap();
        let mut changed = MODEL.to_vec();
        changed[100] ^= 1;
        assert_eq!(
            verify_model(&changed),
            Err("silero_model_checksum_mismatch")
        );
        assert!(verify_model(&MODEL[..MODEL.len() - 1]).is_err());
    }
    #[test]
    fn resampling_is_continuous_and_rejects_aliases() {
        let tone = |hz: f32| {
            (0..24000)
                .flat_map(|n| {
                    (((std::f32::consts::TAU * hz * n as f32 / 24000.0).sin() * 10000.0) as i16)
                        .to_le_bytes()
                })
                .collect::<Vec<_>>()
        };
        let input = tone(1000.0);
        let mut whole = VecDeque::new();
        Resampler::default().process(&input, &mut whole);
        let mut split = VecDeque::new();
        let mut resampler = Resampler::default();
        for frame in input.chunks(960) {
            resampler.process(frame, &mut split);
        }
        assert_eq!(whole.len(), 16000);
        assert_eq!(whole, split);
        let mut high = VecDeque::new();
        Resampler::default().process(&tone(10000.0), &mut high);
        let power = |samples: &VecDeque<f32>| samples.iter().skip(100).map(|x| x * x).sum::<f32>();
        assert!(power(&high) < power(&whole) * 0.001);
        resampler.reset();
        let mut zero = VecDeque::new();
        resampler.process(&[0; 960], &mut zero);
        assert!(zero.iter().all(|x| *x == 0.0));
    }
}
