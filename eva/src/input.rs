//! Input items have independent identities from model responses. A delayed VAD
//! event cannot claim a new turn just because the microphone was reopened.
use std::{
    collections::{BTreeSet, VecDeque},
    io,
    time::{Duration, Instant},
};

/// Optional ASR is display data only. It never changes plan admission or the
/// user's audio item identity and is never passed to telemetry or persistence.
pub fn caption(event: &serde_json::Value, item: &str, turn: u64) -> Option<serde_json::Value> {
    if event["type"] != "conversation.item.input_audio_transcription.completed"
        || event["item_id"].as_str() != Some(item)
        || event["content_index"].as_u64() != Some(0)
    {
        return None;
    }
    let text: String = event["transcript"]
        .as_str()?
        .chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(*c, '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
        })
        .take(160)
        .collect();
    (!text.is_empty()).then(|| serde_json::json!({"item_id":item,"turn_epoch":turn,"text":text}))
}
/// Earliest uploaded audio carried by a VAD item, including server padding.
/// Unverified native timing stays unverified; it cannot authorize a guessed
/// desktop focus binding.
#[derive(Debug, Clone, Copy)]
pub struct UploadedOnset {
    pub captured: Instant,
    pub timestamp_verified: bool,
}
struct UploadedFrame {
    start: u64,
    end: u64,
    captured: Instant,
    timestamp_verified: bool,
}
const MAX_UPLOADED_RANGES: usize = 32_768;
#[derive(Default)]
pub struct InputGate {
    pending: Option<(String, u64)>,
    pending_start_sample: Option<u64>,
    committed: BTreeSet<String>,
    awaiting_clear: bool,
    written: u64,
    floor: u64,
    frames: VecDeque<UploadedFrame>,
}
impl InputGate {
    pub fn cleared(&mut self) {
        self.awaiting_clear = false;
    }
    pub fn request_clear(&mut self) {
        self.pending = None;
        self.pending_start_sample = None;
        self.awaiting_clear = true;
        // The server's audio_start_ms remains cumulative across clear. Only
        // the admissible range advances; never reset the session sample count.
        self.floor = self.written;
        self.frames.clear();
    }
    /// Call only after the matching PCM append succeeded. Retain metadata,
    /// never audio, and do not turn eviction into permission to guess timing.
    pub fn uploaded(
        &mut self,
        samples: usize,
        captured: Instant,
        timestamp_verified: bool,
    ) -> io::Result<()> {
        let samples = u64::try_from(samples)
            .map_err(|_| io::Error::other("input_audio_timeline_overflow"))?;
        if samples == 0 {
            return Err(io::Error::other("invalid_uploaded_audio"));
        }
        let end = self
            .written
            .checked_add(samples)
            .ok_or_else(|| io::Error::other("input_audio_timeline_overflow"))?;
        if self.frames.len() == MAX_UPLOADED_RANGES {
            self.frames.pop_front();
        }
        self.frames.push_back(UploadedFrame {
            start: self.written,
            end,
            captured,
            timestamp_verified,
        });
        self.written = end;
        Ok(())
    }
    /// Bound the complete owned utterance using successful PCM appends, not
    /// when its delayed VAD event happened to arrive. The caller includes any
    /// allowed server prefix padding in max_ms and checks before each append.
    /// Silence before the owned start and audio of cleared turns do not count.
    pub fn ensure_utterance_limit(&self, additional_samples: usize, max_ms: u64) -> io::Result<()> {
        let Some(start) = self.pending_start_sample else {
            return Ok(());
        };
        let additional = u64::try_from(additional_samples)
            .map_err(|_| io::Error::other("input_audio_timeline_overflow"))?;
        let end = self
            .written
            .checked_add(additional)
            .ok_or_else(|| io::Error::other("input_audio_timeline_overflow"))?;
        let samples = end
            .checked_sub(start)
            .ok_or_else(|| io::Error::other("input_audio_timeline_overflow"))?;
        let limit = max_ms
            .checked_mul(24)
            .ok_or_else(|| io::Error::other("input_audio_timeline_overflow"))?;
        if samples > limit {
            return Err(io::Error::other("utterance_too_long"));
        }
        Ok(())
    }
    /// A server event must point into successfully uploaded audio of this
    /// clear epoch. IDs and clear acknowledgements alone do not establish that.
    pub fn start_at(
        &mut self,
        id: &str,
        turn: u64,
        audio_start_ms: u64,
    ) -> io::Result<Option<UploadedOnset>> {
        let sample = audio_start_ms
            .checked_mul(24)
            .ok_or_else(|| io::Error::other("input_audio_timeline_overflow"))?;
        if sample < self.floor || sample >= self.written {
            return Ok(None);
        }
        let Some(frame) = self
            .frames
            .iter()
            .find(|frame| frame.start <= sample && sample < frame.end)
        else {
            return Ok(None);
        };
        let offset = sample - frame.start;
        let duration = Duration::new(
            offset / 24_000,
            ((offset % 24_000) * 1_000_000_000 / 24_000) as u32,
        );
        let onset = UploadedOnset {
            captured: frame
                .captured
                .checked_add(duration)
                .ok_or_else(|| io::Error::other("input_audio_timestamp_overflow"))?,
            timestamp_verified: frame.timestamp_verified,
        };
        if self.start(id, turn)? {
            self.pending_start_sample = Some(sample);
            Ok(Some(onset))
        } else {
            Ok(None)
        }
    }
    // Production callers must use start_at, which proves PCM provenance first.
    fn start(&mut self, id: &str, turn: u64) -> io::Result<bool> {
        if id.is_empty() || id.len() > 128 || id.chars().any(char::is_control) {
            return Err(io::Error::other("invalid_input_item"));
        }
        if self.awaiting_clear || self.committed.contains(id) {
            return Ok(false);
        }
        if let Some((pending, _)) = self.pending.as_ref() {
            return if pending == id {
                Ok(false)
            } else {
                Err(io::Error::other("input_out_of_order"))
            };
        }
        self.pending = Some((id.into(), turn));
        Ok(true)
    }
    pub fn matches(&self, id: &str, turn: u64) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|(pending, t)| pending == id && *t == turn)
    }
    pub fn commit(&mut self, id: &str, turn: u64) -> io::Result<bool> {
        if self.committed.contains(id) {
            return Ok(false);
        }
        if !self.matches(id, turn) || self.awaiting_clear {
            return Err(io::Error::other("input_out_of_order"));
        }
        if self.committed.len() >= 1024 {
            return Err(io::Error::other("input_capacity_exceeded"));
        }
        self.pending = None;
        self.pending_start_sample = None;
        self.committed.insert(id.into());
        Ok(true)
    }
}
/// After a connection overflow, discard the rest of that sentence. A complete
/// local silence interval is required before any fresh audio reaches the model.
pub struct QuietGate {
    quiet_ms: u64,
    required_ms: u64,
}
impl QuietGate {
    pub fn new(required_ms: u64) -> Self {
        Self {
            quiet_ms: 0,
            required_ms: required_ms.max(600),
        }
    }
    pub fn observe(&mut self, level: f32) -> bool {
        if level.is_finite() && level < 0.025 {
            self.quiet_ms = self.quiet_ms.saturating_add(20);
        } else {
            self.quiet_ms = 0;
        }
        self.quiet_ms >= self.required_ms
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn captions_require_the_committed_item_and_strip_invisible_controls() {
        let event = serde_json::json!({"type":"conversation.item.input_audio_transcription.completed","item_id":"current","content_index":0,"transcript":format!("切换\u{202e}\n{}", "第三个tag".repeat(100))});
        assert!(caption(&event, "old", 3).is_none());
        let value = caption(&event, "current", 3).unwrap();
        assert_eq!(value["turn_epoch"], 3);
        let text = value["text"].as_str().unwrap();
        assert_eq!(text.chars().count(), 160);
        assert!(!text.contains(['\u{202e}', '\n']));
    }
    #[test]
    fn overflow_cannot_upload_an_utterance_suffix() {
        let mut gate = QuietGate::new(600);
        for _ in 0..20 {
            assert!(!gate.observe(0.2));
        }
        for _ in 0..29 {
            assert!(!gate.observe(0.0));
        }
        assert!(!gate.observe(0.2));
        for _ in 0..29 {
            assert!(!gate.observe(0.0));
        }
        assert!(gate.observe(0.0));
    }
    #[test]
    fn stale_vad_and_duplicate_commits_cannot_claim_another_turn() {
        let mut g = InputGate::default();
        let captured = Instant::now();
        g.uploaded(480, captured, true).unwrap();
        assert!(g.start_at("old", 1, 0).unwrap().is_some());
        assert!(!g.matches("other", 1));
        assert!(!g.matches("old", 2));
        assert!(g.commit("old", 1).unwrap());
        g.request_clear();
        g.uploaded(480, captured + Duration::from_secs(5), true)
            .unwrap();
        assert!(g.start_at("late", 2, 20).unwrap().is_none());
        g.cleared();
        assert!(g.start_at("old", 2, 20).unwrap().is_none());
        assert!(!g.commit("old", 2).unwrap());
        assert!(g.start_at("new", 2, 20).unwrap().is_some());
        assert!(g.commit("late", 2).is_err());
        assert!(g.commit("new", 2).unwrap());
    }
    #[test]
    fn unknown_old_items_cannot_claim_new_audio_after_a_clear_ack() {
        let captured = Instant::now();
        let mut gate = InputGate::default();
        gate.uploaded(480, captured, true).unwrap();
        gate.request_clear();
        // A fresh local onset and a clear ACK do not make an unknown old ID new.
        let fresh = captured + Duration::from_secs(5);
        gate.uploaded(960, fresh, true).unwrap();
        gate.cleared();
        assert!(gate.start_at("unknown-old-item", 2, 5).unwrap().is_none());
        assert!(!gate.matches("unknown-old-item", 2));
        let onset = gate.start_at("fresh-item", 2, 25).unwrap().unwrap();
        assert_eq!(onset.captured, fresh + Duration::from_millis(5));
        assert!(gate.matches("fresh-item", 2));
    }
    #[test]
    fn successive_clear_epochs_keep_the_session_audio_offset() {
        let captured = Instant::now();
        let mut gate = InputGate::default();
        for (turn, start_ms) in [(1, 0), (2, 20), (3, 40)] {
            let native = captured + Duration::from_secs(turn);
            gate.uploaded(480, native, true).unwrap();
            let onset = gate
                .start_at(&format!("item-{turn}"), turn, start_ms + 7)
                .unwrap()
                .unwrap();
            assert_eq!(onset.captured, native + Duration::from_millis(7));
            assert!(gate.commit(&format!("item-{turn}"), turn).unwrap());
            gate.request_clear();
            gate.cleared();
        }
    }
    #[test]
    fn padding_is_allowed_at_zero_but_never_guessed_across_a_clear_floor() {
        let native = Instant::now();
        let mut gate = InputGate::default();
        gate.uploaded(480, native, true).unwrap();
        assert_eq!(
            gate.start_at("first-padding", 1, 0)
                .unwrap()
                .unwrap()
                .captured,
            native
        );
        gate.commit("first-padding", 1).unwrap();
        gate.request_clear();
        gate.cleared();
        gate.uploaded(480, native + Duration::from_secs(3), true)
            .unwrap();
        assert!(
            gate.start_at("padding-crosses-floor", 2, 19)
                .unwrap()
                .is_none()
        );
        assert!(gate.start_at("new-floor", 2, 20).unwrap().is_some());
    }
    #[test]
    fn future_missing_and_overflowed_audio_ranges_never_claim_an_item() {
        let native = Instant::now();
        let mut gate = InputGate::default();
        assert!(gate.start_at("no-upload", 1, 0).unwrap().is_none());
        gate.uploaded(480, native, true).unwrap();
        assert!(gate.start_at("future", 1, 20).unwrap().is_none());
        assert!(gate.start_at("overflow", 1, u64::MAX).is_err());
        assert!(gate.pending.is_none());
        for _ in 0..MAX_UPLOADED_RANGES {
            gate.uploaded(24, native, true).unwrap();
        }
        assert_eq!(gate.frames.len(), MAX_UPLOADED_RANGES);
        assert!(gate.start_at("evicted", 1, 0).unwrap().is_none());
        assert!(gate.start_at("retained", 1, 20).unwrap().is_some());
        let mut overflow = InputGate {
            written: u64::MAX - 1,
            ..Default::default()
        };
        assert!(overflow.uploaded(2, native, true).is_err());
        assert!(overflow.frames.is_empty());
        assert!(InputGate::default().uploaded(0, native, true).is_err());
    }
    #[test]
    fn source_time_and_submillisecond_offsets_preserve_unverified_timing() {
        let native = Instant::now();
        let mut gate = InputGate::default();
        gate.uploaded(13, native, false).unwrap();
        let new_source = native + Duration::from_secs(4);
        gate.uploaded(480, new_source, false).unwrap();
        // Millisecond 1 is sample24, eleven samples into this native frame.
        let onset = gate.start_at("uncertain-source", 1, 1).unwrap().unwrap();
        assert_eq!(onset.captured, new_source + Duration::from_nanos(458_333));
        assert!(!onset.timestamp_verified);
    }
    #[test]
    fn pcm_limit_excludes_initial_silence_and_rejects_the_frame_after_twenty_seconds() {
        let native = Instant::now();
        let mut gate = InputGate::default();
        gate.uploaded(240_000, native, false).unwrap();
        gate.ensure_utterance_limit(480, 20_000).unwrap();
        gate.uploaded(480, native + Duration::from_secs(10), false)
            .unwrap();
        assert!(gate.start_at("quiet-voice", 1, 10_000).unwrap().is_some());
        // Native timing quality affects context binding, never this PCM bound.
        gate.ensure_utterance_limit(479_520, 20_000).unwrap();
        gate.uploaded(479_520, native + Duration::from_millis(10_020), false)
            .unwrap();
        gate.ensure_utterance_limit(0, 20_000).unwrap();
        assert_eq!(
            gate.ensure_utterance_limit(480, 20_000)
                .unwrap_err()
                .to_string(),
            "utterance_too_long"
        );
        assert!(gate.ensure_utterance_limit(1, 20_000).is_err());
    }
    #[test]
    fn late_vad_checks_already_uploaded_audio_and_clear_cannot_reclaim_the_old_prefix() {
        let native = Instant::now();
        let mut gate = InputGate::default();
        gate.uploaded(480_480, native, true).unwrap();
        assert!(gate.start_at("late-vad", 1, 0).unwrap().is_some());
        assert_eq!(
            gate.ensure_utterance_limit(0, 20_000)
                .unwrap_err()
                .to_string(),
            "utterance_too_long"
        );
        gate.request_clear();
        gate.ensure_utterance_limit(0, 20_000).unwrap();
        gate.uploaded(480, native + Duration::from_secs(30), true)
            .unwrap();
        gate.cleared();
        assert!(gate.start_at("old-prefix-new-id", 2, 0).unwrap().is_none());
        assert!(gate.start_at("fresh", 2, 20_020).unwrap().is_some());
        gate.ensure_utterance_limit(479_520, 20_000).unwrap();
        assert!(gate.ensure_utterance_limit(480_000, 20_000).is_err());
    }
    #[test]
    fn committed_turn_releases_its_pcm_bound_and_limit_arithmetic_never_wraps() {
        let native = Instant::now();
        let mut gate = InputGate::default();
        gate.uploaded(480_000, native, true).unwrap();
        gate.start_at("owned", 1, 0).unwrap().unwrap();
        assert!(gate.commit("other", 1).is_err());
        assert!(gate.ensure_utterance_limit(1, 20_000).is_err());
        assert!(gate.ensure_utterance_limit(0, u64::MAX).is_err());
        gate.commit("owned", 1).unwrap();
        gate.ensure_utterance_limit(480_000, 20_000).unwrap();
        let overflow = InputGate {
            pending: Some(("owned".into(), 1)),
            pending_start_sample: Some(u64::MAX - 480),
            written: u64::MAX,
            ..Default::default()
        };
        assert!(overflow.ensure_utterance_limit(1, 20_000).is_err());
    }
}
