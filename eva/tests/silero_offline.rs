//! Real model tests, never microphone/API tests. Explicit opt-in prevents a
//! missing local native runtime from being confused with successful Silero use.
#![cfg(feature = "silero")]
use jwm_eva::{
    audio::{Frame, silero::SileroDetector, voice_gate::UploadGate},
    config::Config,
};
use std::time::{Duration, Instant};

const SPEECH: &[u8] = include_bytes!("fixtures/rnnoise-speech.s16le");

fn config() -> Config {
    let mut c = Config::default();
    c.audio.local_voice_fallback = "none".into();
    c
}
fn gate() -> UploadGate {
    let gate = UploadGate::new(&config());
    assert_eq!(
        gate.statistics()["voice_backend"],
        "silero",
        "real Silero backend is required"
    );
    gate
}
fn frame(pcm: &[u8], at: Instant) -> Frame {
    Frame {
        captured: at,
        timestamp_verified: true,
        pcm: pcm.to_vec(),
        level: 1.0,
    }
}

#[test]
#[ignore = "requires installed CPU ONNX Runtime 1.22; no network, microphone or API"]
fn real_silero_preserves_speech_prefix_tail_and_source_time() {
    let mut gate = gate();
    let start = Instant::now();
    let mut emitted = Vec::new();
    let mut source = vec![0; 960 * 25];
    source.extend_from_slice(SPEECH);
    source.extend_from_slice(&vec![0; 960 * 100]);
    for (index, pcm) in source.chunks_exact(960).enumerate() {
        emitted.extend(
            gate.admit(
                frame(pcm, start + Duration::from_millis(index as u64 * 20)),
                false,
            )
            .unwrap(),
        );
    }
    assert!(gate.openings > 0);
    assert!(!gate.open, "tail silence must close the local gate");
    assert!(emitted.windows(2).all(|f| f[0].captured < f[1].captured));
    // Every non-silent sample in the public phrase survives. Native timestamps
    // must still identify exactly the corresponding original 20 ms frame.
    for (index, pcm) in SPEECH.chunks_exact(960).enumerate() {
        let at = start + Duration::from_millis((index as u64 + 25) * 20);
        let uploaded = emitted
            .iter()
            .find(|f| f.captured == at)
            .expect("speech frame must be retained");
        assert_eq!(
            uploaded.pcm, pcm,
            "unmodified first/last syllables at frame {index}"
        );
    }
    assert!(gate.silenced_frames > 0);
    eprintln!("real_model_upload_gate={}", gate.statistics());
}

#[test]
#[ignore = "requires installed CPU ONNX Runtime 1.22; no network, microphone or API"]
fn real_silero_silence_tones_hiss_and_clicks_do_not_upload() {
    for kind in ["silence", "tone", "hiss", "clicks"] {
        let mut gate = gate();
        let start = Instant::now();
        let mut random = 17u32;
        for index in 0..200 {
            let pcm = (0..480)
                .flat_map(|sample| {
                    random ^= random << 13;
                    random ^= random >> 17;
                    random ^= random << 5;
                    let n = index * 480 + sample;
                    let value = match kind {
                        "tone" => {
                            ((std::f32::consts::TAU * 440.0 * n as f32 / 24000.0).sin() * 16000.0)
                                as i16
                        }
                        "hiss" => (random as i16) / 2,
                        "clicks" if n % 4800 < 8 => 25000,
                        _ => 0,
                    };
                    value.to_le_bytes()
                })
                .collect::<Vec<_>>();
            assert!(
                gate.admit(
                    frame(&pcm, start + Duration::from_millis(index as u64 * 20)),
                    false
                )
                .unwrap()
                .is_empty(),
                "{kind} opened upload gate"
            );
        }
        assert_eq!(gate.uploaded_frames, 0, "{kind}");
    }
}

#[test]
#[ignore = "requires installed CPU ONNX Runtime 1.22; no network, microphone or API"]
fn real_silero_reset_is_deterministic_and_invalid_input_erases_history() {
    let mut model = SileroDetector::new(None).unwrap();
    let scores = |model: &mut SileroDetector| {
        SPEECH
            .chunks_exact(960)
            .map(|pcm| {
                let score = model.process(pcm).unwrap();
                (score.probability, score.ready)
            })
            .collect::<Vec<_>>()
    };
    let first = scores(&mut model);
    assert!(first.iter().any(|(p, _)| *p > 0.65));
    model.reset();
    assert_eq!(first, scores(&mut model));
    assert!(model.process(&[1; 958]).is_err());
    let reset = model.process(&[0; 960]).unwrap();
    let fresh = SileroDetector::new(None)
        .unwrap()
        .process(&[0; 960])
        .unwrap();
    assert_eq!(
        (reset.probability, reset.ready),
        (fresh.probability, fresh.ready)
    );
    assert!(!reset.ready);
}

#[test]
#[ignore = "requires installed CPU ONNX Runtime 1.22; no network, microphone or API"]
fn real_silero_partial_handoff_preserves_first_syllable_and_epoch_reset_closes_upload() {
    let mut gate = gate();
    let start = Instant::now();
    let mut source = vec![0; 960 * 20];
    source.extend_from_slice(SPEECH);
    source.extend_from_slice(&vec![0; 960 * 25]);
    let mut at = start;
    let mut emitted = Vec::new();
    for part in std::iter::once(&source[..480]).chain(source[480..].chunks(960)) {
        emitted.extend(gate.admit(frame(part, at), false).unwrap());
        at += Duration::from_nanos(part.len() as u64 * 1_000_000_000 / 48_000);
    }
    for (index, pcm) in SPEECH.chunks_exact(960).enumerate() {
        let captured = start + Duration::from_millis((index as u64 + 20) * 20);
        assert_eq!(
            emitted
                .iter()
                .find(|f| f.captured == captured)
                .expect("handoff must preserve first and final syllables")
                .pcm,
            pcm
        );
    }
    gate.reset();
    assert!(!gate.open);
    for n in 0..100 {
        let frames = gate
            .admit(frame(&[0; 960], at + Duration::from_millis(n * 20)), true)
            .unwrap();
        assert!(frames.iter().all(|f| f.pcm.iter().all(|b| *b == 0)));
    }
    assert_eq!(
        gate.openings, 1,
        "late cloud events cannot re-open an old voice epoch"
    );
}

#[test]
#[ignore = "requires installed CPU ONNX Runtime 1.22; no network, microphone or API"]
fn real_silero_rejects_duplicate_late_and_unverified_frames() {
    let mut gate = gate();
    let start = Instant::now();
    let first = frame(&SPEECH[..960], start);
    assert!(gate.admit(first, false).unwrap().is_empty());
    assert!(gate.admit(frame(&SPEECH[..960], start), false).is_err());
    assert_eq!(gate.uploaded_frames, 0);
    gate.reset();
    let mut invalid = frame(&SPEECH[..960], start);
    invalid.timestamp_verified = false;
    assert!(gate.admit(invalid, false).unwrap().is_empty());
    // An old cloud start may request tail silence, never fresh raw audio.
    for n in 1..100 {
        let frames = gate
            .admit(
                frame(&[0; 960], start + Duration::from_millis(n * 20)),
                true,
            )
            .unwrap();
        assert!(frames.iter().all(|f| f.pcm.iter().all(|b| *b == 0)));
    }
    assert_eq!(gate.openings, 0);
}

#[test]
#[ignore = "requires installed CPU ONNX Runtime 1.22; no network, microphone or API"]
fn real_silero_accepts_soft_and_noisy_public_speech_and_a_short_segment() {
    for kind in ["soft", "noisy", "short"] {
        let mut c = config();
        let mut random = 31u32;
        let source = if kind == "short" {
            c.audio.local_voice_min_ms = 80;
            &SPEECH[960 * 25..960 * 37] // 240 ms of public speech, no linguistic accuracy claim.
        } else {
            SPEECH
        };
        let pcm = source
            .chunks_exact(2)
            .flat_map(|bytes| {
                let sample = i32::from(i16::from_le_bytes([bytes[0], bytes[1]]));
                random ^= random << 13;
                random ^= random >> 17;
                random ^= random << 5;
                let sample = match kind {
                    "soft" => sample / 6,
                    "noisy" => sample + (random as i16 as i32) / 64,
                    _ => sample,
                };
                (sample.clamp(i16::MIN as i32, i16::MAX as i32) as i16).to_le_bytes()
            })
            .collect::<Vec<_>>();
        let mut gate = UploadGate::new(&c);
        assert_eq!(gate.statistics()["voice_backend"], "silero");
        let start = Instant::now();
        let mut emitted = Vec::new();
        // Silence after a short phrase lets the final partial model window
        // finish while all original speech stays in the onset pre-roll.
        let mut input = pcm.clone();
        input.extend_from_slice(&vec![0; 960 * 60]);
        for (index, bytes) in input.chunks_exact(960).enumerate() {
            emitted.extend(
                gate.admit(
                    frame(bytes, start + Duration::from_millis(index as u64 * 20)),
                    false,
                )
                .unwrap(),
            );
        }
        assert!(gate.openings > 0, "{kind} speech was missed");
        for (index, bytes) in pcm.chunks_exact(960).enumerate() {
            assert_eq!(
                emitted
                    .iter()
                    .find(|f| f.captured == start + Duration::from_millis(index as u64 * 20))
                    .expect("speech prefix/tail preserved")
                    .pcm,
                bytes,
                "{kind}:{index}"
            );
        }
    }
}

#[test]
#[ignore = "requires installed CPU ONNX Runtime 1.22; no network, microphone or API"]
fn real_silero_default_threshold_retains_a_brief_phrase_after_quiet() {
    let mut gate = gate();
    let start = Instant::now();
    let mut source = vec![0; 960 * 20];
    source.extend_from_slice(&SPEECH[28_800..57_600]);
    source.extend_from_slice(&vec![0; 960 * 75]);
    let mut emitted = Vec::new();
    for (n, pcm) in source.chunks_exact(960).enumerate() {
        emitted.extend(
            gate.admit(
                frame(pcm, start + Duration::from_millis(n as u64 * 20)),
                false,
            )
            .unwrap(),
        );
    }
    assert_eq!(
        gate.openings, 1,
        "600 ms public phrase should pass Silero's own default calibration"
    );
    for n in 20..50 {
        let pcm = &source[n * 960..(n + 1) * 960];
        let captured = start + Duration::from_millis(n as u64 * 20);
        assert_eq!(
            emitted
                .iter()
                .find(|f| f.captured == captured)
                .expect("brief phrase prefix/tail")
                .pcm,
            pcm
        );
    }
}
