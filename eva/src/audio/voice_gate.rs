//! Local speech admission before any PCM leaves the process. A local neural model is used
//! for its neural speech evidence; the original PCM is retained for the API's
//! configured noise reducer. This avoids double denoising and preserves the
//! native sample timestamps used to bind desktop focus.
use super::{FRAME_BYTES, Frame, local_voice::LocalVoiceDetector, rms};
use crate::config::Config;
use std::{
    collections::VecDeque,
    io,
    time::{Duration, Instant},
};

const FRAME_MS: u64 = 20;

pub struct VoiceEvidence {
    detector: LocalVoiceDetector,
    threshold: f32,
    minimum: u32,
    mask: u64,
    window: u64,
    pub probability: f32,
    pub candidate: bool,
    pub confirmed: bool,
}
impl VoiceEvidence {
    pub fn new(c: &Config) -> Self {
        Self::with_enabled(c, c.audio.local_voice_detection)
    }
    fn with_enabled(c: &Config, enabled: bool) -> Self {
        let minimum = (c.audio.local_voice_min_ms / FRAME_MS) as u32;
        // Permit a few unvoiced consonants inside the confirmation window.
        let width = (minimum + 4).min(63);
        let detector = if enabled {
            LocalVoiceDetector::new(c)
        } else {
            LocalVoiceDetector::disabled(c)
        };
        let threshold = if detector.status.active == "silero" {
            c.audio.silero_voice_threshold
        } else {
            c.audio.local_voice_threshold
        };
        Self {
            detector,
            threshold,
            minimum,
            mask: (1u64 << width) - 1,
            window: 0,
            probability: 0.0,
            candidate: false,
            confirmed: false,
        }
    }
    pub fn reset(&mut self) {
        self.detector.reset();
        self.reset_evidence();
        self.probability = 0.0;
    }
    pub fn reset_evidence(&mut self) {
        self.window = 0;
        self.candidate = false;
        self.confirmed = false;
    }
    pub fn detector_status(&self) -> &super::local_voice::DetectorStatus {
        &self.detector.status
    }
    pub fn observe(&mut self, frame: &Frame) -> io::Result<()> {
        let score = self.detector.process(&frame.pcm)?;
        if !score.probability.is_finite() || !(0.0..=1.0).contains(&score.probability) {
            return Err(io::Error::other("local_voice_invalid_probability"));
        }
        self.probability = score.probability;
        // Do not trust the UI level supplied alongside PCM. A very low
        // absolute floor rejects digital residue without imposing a loudness
        // requirement on soft speech already recognized by the neural model.
        self.candidate = score.ready
            && score.probability >= self.threshold
            && score.evidence_rms.is_finite()
            && score.evidence_rms >= 0.0003;
        self.window = ((self.window << 1) | u64::from(self.candidate)) & self.mask;
        self.confirmed = self.candidate && self.window.count_ones() >= self.minimum;
        Ok(())
    }
    pub(crate) fn continuing(&self) -> bool {
        self.candidate || self.probability >= (self.threshold - 0.20).max(0.10)
    }
}

/// Shared by hotkey conversations, follow-up turns and cloud wake standby.
/// Cloud VAD remains the only committer. Its open item keeps receiving tail
/// silence so either server or semantic VAD can finish without a second commit.
pub struct UploadGate {
    pub enabled: bool,
    pub open: bool,
    pub total_frames: u64,
    pub uploaded_frames: u64,
    pub openings: u64,
    pub silenced_frames: u64,
    pub rejected_frames: u64,
    voice: VoiceEvidence,
    preroll: VecDeque<Frame>,
    preroll_limit: usize,
    source_start: Option<Instant>,
    source_end: Option<Instant>,
    /// A server boundary can split a native frame during wake handoff. Keep
    /// at most one incomplete frame until the following native PCM arrives.
    fragment: Option<Frame>,
    onset: Option<Instant>,
    quiet_ms: u64,
    hangover_ms: u64,
}
impl UploadGate {
    pub fn new(c: &Config) -> Self {
        Self::with_enabled(c, c.audio.local_voice_detection)
    }
    pub fn with_enabled(c: &Config, enabled: bool) -> Self {
        Self {
            enabled,
            open: false,
            total_frames: 0,
            uploaded_frames: 0,
            openings: 0,
            silenced_frames: 0,
            rejected_frames: 0,
            voice: VoiceEvidence::with_enabled(c, enabled),
            preroll: VecDeque::new(),
            // Pre-roll precedes the whole confirmation window, including its
            // permitted unvoiced frames. Server padding must fit as well.
            preroll_limit: ((c
                .audio
                .local_voice_preroll_ms
                .max(c.turn_detection.prefix_padding_ms)
                + c.audio.local_voice_min_ms
                + 4 * FRAME_MS)
                .div_ceil(FRAME_MS)) as usize,
            source_start: None,
            source_end: None,
            fragment: None,
            onset: None,
            quiet_ms: 0,
            hangover_ms: c
                .turn_detection
                .silence_duration_ms
                .saturating_add(400)
                .max(1000),
        }
    }
    pub fn suppressed_frames(&self) -> u64 {
        self.total_frames.saturating_sub(self.uploaded_frames)
    }
    pub fn probability(&self) -> f32 {
        self.voice.probability
    }
    pub fn take_onset(&mut self) -> Option<Instant> {
        self.onset.take()
    }
    fn wipe_preroll(&mut self) {
        for frame in &mut self.preroll {
            wipe_frame(frame);
        }
        self.preroll.clear();
    }
    fn wipe_fragment(&mut self) {
        if let Some(mut fragment) = self.fragment.take() {
            wipe_frame(&mut fragment);
        }
    }
    /// A new input epoch cannot inherit the preceding turn's speech evidence,
    /// incomplete handoff fragment or pre-roll, even after a late cloud event.
    pub fn reset(&mut self) {
        self.wipe_preroll();
        self.wipe_fragment();
        self.voice.reset();
        self.open = false;
        self.quiet_ms = 0;
        self.source_start = None;
        self.source_end = None;
        self.onset = None;
    }
    pub fn statistics(&self) -> serde_json::Value {
        serde_json::json!({"input_frames":self.total_frames,
            "voice_backend":self.voice.detector.status.active,
            "voice_backend_requested":self.voice.detector.status.requested,
            "voice_backend_degraded_reason":self.voice.detector.status.degraded_reason,
            "uploaded_frames":self.uploaded_frames,"suppressed_frames":self.suppressed_frames(),
            "gate_openings":self.openings,"silenced_frames":self.silenced_frames,
            "rejected_source_frames":self.rejected_frames,"voice_probability":self.probability()})
    }
    fn reject(&mut self, frame: &mut Frame, reason: &'static str) -> io::Error {
        self.rejected_frames = self.rejected_frames.saturating_add(1);
        wipe_frame(frame);
        self.reset();
        io::Error::other(reason)
    }
    /// Reassemble a legal short handoff boundary without dropping the first
    /// syllable or asking the detector to accept a non-native frame size. The native
    /// timing tolerance matches Framer; arrival time never supplies provenance.
    fn complete_frame(
        &mut self,
        mut frame: Frame,
        server_item_open: bool,
    ) -> io::Result<Option<Frame>> {
        if frame.pcm.is_empty()
            || frame.pcm.len() > FRAME_BYTES
            || !frame.pcm.len().is_multiple_of(2)
        {
            return Err(self.reject(&mut frame, "local_voice_invalid_audio_source"));
        }
        if !frame.timestamp_verified {
            // The first native buffers may precede the graph's clock report.
            // Keep them local; a verified source can start within the existing
            // speech timeout. An already admitted stream must not bridge a gap.
            let active = self.open || server_item_open;
            let error = self.reject(&mut frame, "local_voice_audio_discontinuity");
            return if active { Err(error) } else { Ok(None) };
        }
        if self.source_start.is_some_and(|last| frame.captured <= last) {
            return Err(self.reject(&mut frame, "local_voice_audio_out_of_order"));
        }
        if let Some(expected) = self.source_end {
            if expected
                .checked_duration_since(frame.captured)
                .is_some_and(|early| early > Duration::from_millis(5))
            {
                return Err(self.reject(&mut frame, "local_voice_audio_out_of_order"));
            }
            if frame
                .captured
                .checked_duration_since(expected)
                .is_some_and(|late| late > Duration::from_millis(5))
            {
                if self.open || server_item_open {
                    return Err(self.reject(&mut frame, "local_voice_audio_discontinuity"));
                }
                // A new local-only source segment can begin, but never reuse
                // a fragment or speech evidence from before the gap.
                self.reset();
            }
        }
        self.source_start = Some(frame.captured);
        self.source_end = Some(frame.captured + pcm_duration(frame.pcm.len()));
        if let Some(mut fragment) = self.fragment.take() {
            let needed = FRAME_BYTES - fragment.pcm.len();
            if frame.pcm.len() < needed {
                fragment.pcm.extend_from_slice(&frame.pcm);
                wipe_frame(&mut frame);
                self.fragment = Some(fragment);
                return Ok(None);
            }
            fragment.pcm.extend_from_slice(&frame.pcm[..needed]);
            // Keep the original length until it is erased; split_off would
            // leave copied tail samples behind in its unused allocation.
            let mut remaining = Vec::with_capacity(FRAME_BYTES);
            remaining.extend_from_slice(&frame.pcm[needed..]);
            if !remaining.is_empty() {
                self.fragment = Some(Frame {
                    captured: frame.captured + pcm_duration(needed),
                    timestamp_verified: true,
                    level: rms(&remaining),
                    pcm: remaining,
                });
            }
            wipe_frame(&mut frame);
            fragment.level = rms(&fragment.pcm);
            return Ok(Some(fragment));
        }
        if frame.pcm.len() < FRAME_BYTES {
            // Reserve once so completing a fragment never frees a smaller
            // allocation containing an unwiped copy of the speech prefix.
            let mut pcm = Vec::with_capacity(FRAME_BYTES);
            pcm.extend_from_slice(&frame.pcm);
            wipe_frame(&mut frame);
            frame.pcm = pcm;
            self.fragment = Some(frame);
            Ok(None)
        } else {
            Ok(Some(frame))
        }
    }
    pub fn admit(&mut self, frame: Frame, server_item_open: bool) -> io::Result<Vec<Frame>> {
        if !self.enabled {
            // Continuous upload is an explicit configuration choice, never
            // a fallback for a detector error or an invalid source.
            self.total_frames = self.total_frames.saturating_add(1);
            self.open = true;
            self.uploaded_frames = self.uploaded_frames.saturating_add(1);
            return Ok(vec![frame]);
        }
        let Some(mut frame) = self.complete_frame(frame, server_item_open)? else {
            return Ok(vec![]);
        };
        self.total_frames = self.total_frames.saturating_add(1);
        if let Err(error) = self.voice.observe(&frame) {
            let reason = if error.to_string() == "local_voice_invalid_probability" {
                "local_voice_invalid_probability"
            } else {
                "local_voice_detector_failed"
            };
            return Err(self.reject(&mut frame, reason));
        }
        if self.open {
            self.quiet_ms = if self.voice.continuing() {
                0
            } else {
                self.quiet_ms.saturating_add(FRAME_MS)
            };
            // Keep short consonants and pauses intact. Once sustained
            // non-speech is established, send silence instead of more noise;
            // the provider still sees enough samples to complete the turn.
            if self.quiet_ms >= 240 {
                wipe_frame(&mut frame);
                frame.level = 0.0;
                self.silenced_frames = self.silenced_frames.saturating_add(1);
            }
            self.uploaded_frames = self.uploaded_frames.saturating_add(1);
            if !server_item_open && self.quiet_ms >= self.hangover_ms {
                self.open = false;
                self.voice.reset_evidence();
                self.quiet_ms = 0;
            }
            return Ok(vec![frame]);
        }
        if self.voice.confirmed {
            self.open = true;
            self.openings = self.openings.saturating_add(1);
            self.quiet_ms = 0;
            // Take the oldest positive frame inside this actual confirmation
            // window. Sparse old candidates cannot pin an onset from seconds
            // ago; source timestamps come from retained frames, not estimates.
            let back = (u64::BITS - 1 - self.voice.window.leading_zeros()) as usize;
            self.onset = Some(if back == 0 {
                frame.captured
            } else {
                self.preroll
                    .iter()
                    .rev()
                    .nth(back - 1)
                    .map_or(frame.captured, |frame| frame.captured)
            });
            let mut frames: Vec<_> = self.preroll.drain(..).collect();
            frames.push(frame);
            self.uploaded_frames = self.uploaded_frames.saturating_add(frames.len() as u64);
            return Ok(frames);
        }
        self.preroll.push_back(frame);
        let mut tail = Vec::new();
        while self.preroll.len() > self.preroll_limit {
            if let Some(mut old) = self.preroll.pop_front() {
                wipe_frame(&mut old);
                if server_item_open {
                    // A late server start may arrive after local closure.
                    // Feed only synthetic silence until new speech is locally
                    // confirmed. Retain a bounded decision window so its
                    // onset is not first uploaded as silence and then replayed.
                    // Each source range is emitted at most once, retaining its
                    // actual capture time even across locally suppressed gaps.
                    old.level = 0.0;
                    self.silenced_frames = self.silenced_frames.saturating_add(1);
                    self.uploaded_frames = self.uploaded_frames.saturating_add(1);
                    tail.push(old);
                }
            }
        }
        Ok(tail)
    }
}
impl Drop for UploadGate {
    fn drop(&mut self) {
        self.wipe_preroll();
        self.wipe_fragment();
    }
}

fn pcm_duration(bytes: usize) -> Duration {
    Duration::from_nanos(bytes as u64 * 1_000_000_000 / 48_000)
}

fn wipe_frame(frame: &mut Frame) {
    for byte in &mut frame.pcm {
        // SAFETY: each pointer is to a live, exclusively borrowed byte. A
        // volatile store preserves erasure even immediately before Drop.
        unsafe {
            std::ptr::write_volatile(byte, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These existing acoustic regressions are calibrated for RNNoise. Real
    // Silero has its own explicit offline suite, including handoff and resets.
    fn gate_config() -> Config {
        let mut c = Config::default();
        c.audio.local_voice_backend = "rnnoise".into();
        c
    }

    const SPEECH: &[u8] = include_bytes!("../../tests/fixtures/rnnoise-speech.s16le");

    #[test]
    fn threshold_uses_actual_neural_backend_and_preserves_legacy_rnnoise_tuning() {
        let mut c = Config::default();
        c.audio.silero_voice_threshold = 0.4;
        c.audio.local_voice_threshold = 0.8;
        c.audio.silero_model_path = Some("/nonexistent/eva-test-silero.onnx".into());
        let evidence = VoiceEvidence::new(&c);
        assert_eq!(evidence.detector_status().active, "rnnoise");
        assert_eq!(evidence.threshold, 0.8);
        c.audio.local_voice_backend = "rnnoise".into();
        assert_eq!(VoiceEvidence::new(&c).threshold, 0.8);
    }

    fn frame(pcm: &[u8], captured: Instant) -> Frame {
        Frame {
            pcm: pcm.to_vec(),
            captured,
            timestamp_verified: true,
            // Deliberately unrelated to PCM: a UI meter cannot open the gate.
            level: 1.0,
        }
    }

    fn feed(gate: &mut UploadGate, at: &mut Instant, pcm: &[u8], pending: bool) -> Vec<Frame> {
        let source = frame(pcm, *at);
        *at += pcm_duration(pcm.len());
        gate.admit(source, pending).unwrap()
    }

    fn speech_onset() -> Vec<u8> {
        let mut pcm = vec![0; FRAME_BYTES * 20];
        pcm.extend_from_slice(&SPEECH[28_800..57_600]);
        pcm
    }

    #[test]
    fn handoff_fragment_reassembly_preserves_every_sample_and_source_timestamp() {
        let pcm = speech_onset();
        let start = Instant::now();
        let mut at = start;
        let mut gate = UploadGate::new(&gate_config());
        let mut complete = Vec::new();
        // A server boundary left half of the first native frame. Following
        // native frames retain their original 20 ms shape and source clock.
        for part in std::iter::once(&pcm[..480]).chain(pcm[480..].chunks(FRAME_BYTES)) {
            let source = frame(part, at);
            at += pcm_duration(part.len());
            if let Some(frame) = gate.complete_frame(source, false).unwrap() {
                complete.push(frame);
            }
            assert!(
                gate.fragment
                    .as_ref()
                    .is_none_or(|frame| frame.pcm.len() < FRAME_BYTES)
            );
        }
        assert!(gate.fragment.is_none());
        assert_eq!(complete.len(), pcm.len() / FRAME_BYTES);
        for (index, output) in complete.iter().enumerate() {
            assert!(output.timestamp_verified);
            assert_eq!(
                output.captured,
                start + Duration::from_millis(index as u64 * FRAME_MS)
            );
            assert_eq!(
                output.pcm,
                pcm[index * FRAME_BYTES..(index + 1) * FRAME_BYTES]
            );
        }
    }

    #[test]
    fn real_speech_with_a_partial_handoff_keeps_the_first_syllable() {
        let pcm = speech_onset();
        let start = Instant::now();
        let mut at = start;
        let mut gate = UploadGate::new(&gate_config());
        let mut admitted = Vec::new();
        for part in std::iter::once(&pcm[..480]).chain(pcm[480..].chunks(FRAME_BYTES)) {
            admitted.extend(feed(&mut gate, &mut at, part, false));
        }
        assert!(gate.open);
        assert_eq!(gate.openings, 1);
        assert!(!admitted.is_empty());
        assert!(admitted[0].captured <= start + Duration::from_millis(400));
        assert!(gate.take_onset().is_some());
        for output in &admitted {
            let offset = (output.captured.duration_since(start).as_millis() as usize) * 48;
            assert_eq!(output.pcm, pcm[offset..offset + FRAME_BYTES]);
            assert!(output.timestamp_verified);
        }
        for pair in admitted.windows(2) {
            assert_eq!(
                pair[1].captured - pair[0].captured,
                Duration::from_millis(FRAME_MS)
            );
        }
    }

    fn noise(seed: &mut u32) -> Vec<u8> {
        (0..480)
            .flat_map(|_| {
                *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (((*seed >> 16) as i16) / 3).to_le_bytes()
            })
            .collect()
    }

    #[test]
    fn real_neural_gate_rejects_noise_tones_clicks_and_a_misleading_ui_meter() {
        let mut gate = UploadGate::new(&gate_config());
        let mut at = Instant::now();
        let mut seed = 23;
        let tone: Vec<u8> = (0..480)
            .flat_map(|index| {
                (((std::f32::consts::TAU * 1_000.0 * index as f32 / 24_000.0).sin() * 18_000.0)
                    as i16)
                    .to_le_bytes()
            })
            .collect();
        let mut click = vec![0; FRAME_BYTES];
        click[100..102].copy_from_slice(&30_000i16.to_le_bytes());
        for kind in 0..4 {
            for index in 0..100 {
                let pcm = match kind {
                    0 => vec![0; FRAME_BYTES],
                    1 => noise(&mut seed),
                    2 => tone.clone(),
                    _ if index % 20 == 0 => click.clone(),
                    _ => vec![0; FRAME_BYTES],
                };
                assert!(
                    feed(&mut gate, &mut at, &pcm, false).is_empty(),
                    "kind {kind} frame {index}"
                );
            }
        }
        assert!(!gate.open);
        assert_eq!(gate.openings, 0);
        assert_eq!(gate.uploaded_frames, 0);
        assert!(gate.preroll.len() <= gate.preroll_limit);
    }

    #[test]
    fn late_server_start_gets_silence_until_new_speech_is_confirmed_without_replaying_sources() {
        let mut gate = UploadGate::new(&gate_config());
        let mut at = Instant::now();
        let mut emitted = Vec::new();
        for pcm in speech_onset().chunks(FRAME_BYTES) {
            emitted.extend(feed(&mut gate, &mut at, pcm, false));
        }
        assert!(gate.open);
        for _ in 0..100 {
            emitted.extend(feed(&mut gate, &mut at, &[0; FRAME_BYTES], false));
        }
        assert!(!gate.open, "local hangover has already expired");
        gate.take_onset();
        let mut seed = 41;
        let mut silent_tail = 0;
        for _ in 0..100 {
            let output = feed(&mut gate, &mut at, &noise(&mut seed), true);
            assert!(
                output
                    .iter()
                    .all(|frame| frame.pcm.iter().all(|byte| *byte == 0))
            );
            silent_tail += output.len();
            emitted.extend(output);
            assert!(!gate.open, "server state alone cannot authorize raw noise");
        }
        assert!(
            silent_tail >= 50,
            "the remote item can finish despite the late event"
        );
        let new_speech_start = at;
        for pcm in SPEECH.chunks_exact(FRAME_BYTES) {
            let output = feed(&mut gate, &mut at, pcm, true);
            if !gate.open {
                assert!(
                    output
                        .iter()
                        .all(|frame| frame.pcm.iter().all(|byte| *byte == 0))
                );
            }
            emitted.extend(output);
        }
        assert!(gate.open);
        assert_eq!(gate.openings, 2);
        let onset = gate.take_onset().unwrap();
        assert!(onset >= new_speech_start && onset < at);
        // Suppressed gaps remain real gaps. In particular, PCM is never first
        // emitted as synthetic silence and then replayed under the same source.
        for pair in emitted.windows(2) {
            assert!(pair[1].captured >= pair[0].captured + pcm_duration(pair[0].pcm.len()));
        }
    }

    #[test]
    fn source_failures_clear_audio_and_never_disable_the_gate() {
        let start = Instant::now();
        let mut gate = UploadGate::new(&gate_config());
        let mut unknown = frame(&[1; FRAME_BYTES], start);
        unknown.timestamp_verified = false;
        assert!(gate.admit(unknown, false).unwrap().is_empty());
        assert!(gate.enabled && !gate.open);
        assert!(
            gate.admit(frame(&[1; 480], start + Duration::from_millis(20)), false)
                .unwrap()
                .is_empty()
        );
        assert!(gate.fragment.is_some());
        // Reordered source time cannot be repaired with the arrival clock.
        let error = gate
            .admit(
                frame(&[1; FRAME_BYTES], start + Duration::from_millis(20)),
                false,
            )
            .err()
            .unwrap();
        assert_eq!(error.to_string(), "local_voice_audio_out_of_order");
        assert!(gate.fragment.is_none() && gate.preroll.is_empty());
        for len in [0, 1, FRAME_BYTES + 1, FRAME_BYTES + 2, 2 * FRAME_BYTES] {
            assert!(gate.admit(frame(&vec![7; len], start), false).is_err());
            assert!(gate.enabled && !gate.open);
            assert!(gate.fragment.is_none() && gate.preroll.is_empty());
        }
        let mut at = start + Duration::from_secs(1);
        for pcm in speech_onset().chunks(FRAME_BYTES) {
            feed(&mut gate, &mut at, pcm, false);
        }
        assert!(gate.open);
        let mut unknown = frame(&[1; FRAME_BYTES], at);
        unknown.timestamp_verified = false;
        assert!(gate.admit(unknown, true).is_err());
        assert!(gate.enabled && !gate.open);
        assert!(gate.fragment.is_none() && gate.preroll.is_empty());
    }

    #[test]
    fn reset_erases_bounded_preroll_and_pending_fragment() {
        let mut gate = UploadGate::new(&gate_config());
        let mut at = Instant::now();
        for _ in 0..150 {
            feed(&mut gate, &mut at, &[0; FRAME_BYTES], false);
            assert!(gate.preroll.len() <= gate.preroll_limit);
        }
        feed(&mut gate, &mut at, &[3; 480], false);
        assert!(gate.fragment.is_some());
        assert!(!gate.preroll.is_empty());
        gate.reset();
        assert!(gate.fragment.is_none() && gate.preroll.is_empty());
        assert!(gate.source_start.is_none() && gate.source_end.is_none());
        assert!(gate.take_onset().is_none());
        assert_eq!(gate.probability(), 0.0);
        let mut secret = frame(&[3; FRAME_BYTES], at);
        wipe_frame(&mut secret);
        assert!(secret.pcm.iter().all(|byte| *byte == 0));
    }
}
