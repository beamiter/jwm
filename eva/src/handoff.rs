//! Local PCM ownership across the mini/standard connection boundary. Nothing
//! here comes from a mini transcript or authorizes a desktop operation.
use crate::{
    audio::{Capture, Frame},
    context::{ContextMonitor, UtteranceBinding},
};
use std::{
    collections::VecDeque,
    io,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

pub const MAX_REPLAY_MS: u64 = 15_000;
const RATE: u64 = 24_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub wm_instance_id: String,
    pub wake_generation: u64,
    pub assistant_generation: u64,
}

pub struct Seed {
    pub frames: Vec<Frame>,
    pub binding: UtteranceBinding,
    pub committed: Instant,
    pub monitor_token: String,
}
impl Drop for Seed {
    fn drop(&mut self) {
        wipe_frames(&mut self.frames);
    }
}

pub struct Handoff {
    pub source: Source,
    pub seed: Option<Seed>,
    pub desktop: Option<ContextMonitor>,
    pub capture: Option<Capture>,
    pub sender: mpsc::Sender<io::Result<Frame>>,
    pub receiver: Option<mpsc::Receiver<io::Result<Frame>>>,
    pub tail: VecDeque<Frame>,
    pub created: Instant,
}
impl Handoff {
    pub fn discard_seed(&mut self) {
        self.seed.take();
        for frame in &mut self.tail {
            wipe(&mut frame.pcm);
        }
        self.tail.clear();
        if let Some(receiver) = self.receiver.as_mut() {
            while let Ok(frame) = receiver.try_recv() {
                if let Ok(mut frame) = frame {
                    wipe(&mut frame.pcm);
                }
            }
        }
    }
    pub fn current(&self, source: &Source) -> bool {
        self.source == *source
            && self.created.elapsed() <= Duration::from_secs(20)
            && self.seed.as_ref().is_none_or(|seed| {
                self.desktop
                    .as_ref()
                    .is_some_and(|desktop| desktop.binding_current(&seed.binding))
            })
    }
}
impl Drop for Handoff {
    fn drop(&mut self) {
        for frame in &mut self.tail {
            wipe(&mut frame.pcm);
        }
        // Close the stream before clearing any frames still queued by its
        // capture callback. A cancelled handoff cannot retain a microphone.
        self.capture.take();
        if let Some(receiver) = self.receiver.as_mut() {
            while let Ok(Ok(mut frame)) = receiver.try_recv() {
                wipe(&mut frame.pcm);
            }
        }
    }
}

fn wipe(bytes: &mut [u8]) {
    for byte in bytes {
        unsafe {
            std::ptr::write_volatile(byte, 0);
        }
    }
}
pub fn wipe_frames(frames: &mut [Frame]) {
    for frame in frames {
        wipe(&mut frame.pcm);
    }
}

struct Range {
    start: u64,
    end: u64,
    frame: Frame,
}
/// Sliding memory-only history indexed by successful mini PCM uploads. Exact
/// server item boundaries are checked before moving a positive item out.
pub struct History {
    ranges: VecDeque<Range>,
    written: u64,
    maximum_samples: u64,
}
impl History {
    pub fn new(retention_ms: u64) -> Self {
        Self {
            ranges: VecDeque::new(),
            written: 0,
            maximum_samples: retention_ms * 24,
        }
    }
    pub fn push(&mut self, frame: Frame) -> io::Result<()> {
        if frame.pcm.is_empty() || !frame.pcm.len().is_multiple_of(2) {
            return Err(io::Error::other("invalid_input_pcm"));
        }
        let start = self.written;
        self.written = self
            .written
            .checked_add((frame.pcm.len() / 2) as u64)
            .ok_or_else(|| io::Error::other("input_audio_timeline_overflow"))?;
        self.ranges.push_back(Range {
            start,
            end: self.written,
            frame,
        });
        self.discard_before(self.written.saturating_sub(self.maximum_samples));
        Ok(())
    }
    pub fn discard_before(&mut self, sample: u64) {
        while self.ranges.front().is_some_and(|range| range.end <= sample) {
            let mut removed = self.ranges.pop_front().unwrap();
            wipe(&mut removed.frame.pcm);
        }
    }
    pub fn replay(&self, start_ms: u64, end_ms: u64) -> io::Result<Vec<Frame>> {
        if end_ms <= start_ms || end_ms - start_ms > MAX_REPLAY_MS {
            return Err(io::Error::other("wake_handoff_too_long"));
        }
        let (start, end) = (start_ms.checked_mul(24), end_ms.checked_mul(24));
        let (Some(start), Some(end)) = (start, end) else {
            return Err(io::Error::other("input_audio_timeline_overflow"));
        };
        if end > self.written || self.ranges.front().is_none_or(|range| range.start > start) {
            return Err(io::Error::other("wake_handoff_audio_missing"));
        }
        let mut frames = Vec::new();
        let mut next = start;
        for range in &self.ranges {
            if range.end <= start || range.start >= end {
                continue;
            }
            let from = start.max(range.start);
            let to = end.min(range.end);
            if from != next || !range.frame.timestamp_verified {
                wipe_frames(&mut frames);
                return Err(io::Error::other("audio_timestamp_unverified"));
            }
            let captured = range.frame.captured + samples_duration(from - range.start);
            if let Some(previous) = frames.last() {
                let previous: &Frame = previous;
                let expected =
                    previous.captured + samples_duration((previous.pcm.len() / 2) as u64);
                if captured
                    .saturating_duration_since(expected)
                    .max(expected.saturating_duration_since(captured))
                    > Duration::from_millis(5)
                {
                    wipe_frames(&mut frames);
                    return Err(io::Error::other("wake_handoff_audio_gap"));
                }
            }
            frames.push(Frame {
                captured,
                timestamp_verified: true,
                pcm: range.frame.pcm
                    [((from - range.start) * 2) as usize..((to - range.start) * 2) as usize]
                    .to_vec(),
                level: range.frame.level,
            });
            next = to;
        }
        if next != end {
            wipe_frames(&mut frames);
            return Err(io::Error::other("wake_handoff_audio_missing"));
        }
        Ok(frames)
    }
    pub fn tail_after(&mut self, end_ms: u64) -> VecDeque<Frame> {
        let end = end_ms.saturating_mul(24);
        self.discard_before(end);
        let mut tail = VecDeque::new();
        while let Some(mut range) = self.ranges.pop_front() {
            if range.start < end {
                let skip = ((end - range.start) * 2) as usize;
                wipe(&mut range.frame.pcm[..skip]);
                range.frame.pcm.drain(..skip);
                range.frame.captured += samples_duration(end - range.start);
            }
            tail.push_back(range.frame);
        }
        tail
    }
}
impl Drop for History {
    fn drop(&mut self) {
        for range in &mut self.ranges {
            wipe(&mut range.frame.pcm);
        }
    }
}
pub fn samples_duration(samples: u64) -> Duration {
    Duration::from_nanos((samples as u128 * 1_000_000_000 / RATE as u128) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frame(at: Instant, sample: u8, verified: bool) -> Frame {
        Frame {
            captured: at,
            timestamp_verified: verified,
            pcm: vec![sample; 960],
            level: 0.1,
        }
    }
    #[test]
    fn exact_owned_slice_includes_suffix_once_and_tail_is_separate() {
        let at = Instant::now();
        let mut history = History::new(1000);
        for n in 0..4 {
            history
                .push(frame(at + Duration::from_millis(n * 20), n as u8, true))
                .unwrap();
        }
        let frames = history.replay(10, 50).unwrap();
        assert_eq!(frames.iter().map(|f| f.pcm.len()).sum::<usize>(), 1920);
        assert_eq!(frames[0].pcm, vec![0; 480]);
        assert_eq!(frames[1].pcm, vec![1; 960]);
        assert_eq!(frames[2].pcm, vec![2; 480]);
        let tail = history.tail_after(50);
        assert_eq!(tail.iter().map(|f| f.pcm.len()).sum::<usize>(), 1440);
        assert_eq!(tail[0].pcm, vec![2; 480]);
        assert_eq!(tail[1].pcm, vec![3; 960]);
        assert!(history.replay(10, 50).is_err());
    }
    #[test]
    fn eviction_unverified_clock_discontinuity_and_oversize_never_replay_suffixes() {
        let at = Instant::now();
        let mut history = History::new(20);
        history.push(frame(at, 1, true)).unwrap();
        history
            .push(frame(at + Duration::from_millis(20), 2, true))
            .unwrap();
        assert!(history.replay(0, 40).is_err());
        let mut history = History::new(1000);
        history.push(frame(at, 1, false)).unwrap();
        assert!(history.replay(0, 20).is_err());
        let mut history = History::new(1000);
        history.push(frame(at, 1, true)).unwrap();
        history
            .push(frame(at + Duration::from_millis(50), 2, true))
            .unwrap();
        assert!(history.replay(0, 40).is_err());
        assert!(history.replay(0, MAX_REPLAY_MS + 1).is_err());
    }

    #[tokio::test]
    async fn stale_source_or_handoff_cannot_reuse_audio_and_fresh_fallback_has_no_seed() {
        let source = Source {
            wm_instance_id: "original".into(),
            wake_generation: 1,
            assistant_generation: 2,
        };
        let (sender, receiver) = mpsc::channel(2);
        let mut handoff = Handoff {
            source: source.clone(),
            seed: None,
            desktop: None,
            capture: None,
            sender,
            receiver: Some(receiver),
            tail: VecDeque::new(),
            created: Instant::now(),
        };
        assert!(handoff.current(&source));
        for changed in [
            Source {
                wm_instance_id: "restarted".into(),
                ..source.clone()
            },
            Source {
                wake_generation: 2,
                ..source.clone()
            },
            Source {
                assistant_generation: 3,
                ..source.clone()
            },
        ] {
            assert!(!handoff.current(&changed));
        }
        handoff.created = Instant::now() - Duration::from_secs(21);
        assert!(!handoff.current(&source));
        handoff.tail.push_back(frame(Instant::now(), 17, true));
        handoff
            .sender
            .send(Ok(frame(Instant::now(), 27, true)))
            .await
            .unwrap();
        handoff.discard_seed();
        assert!(handoff.seed.is_none());
        assert!(handoff.tail.is_empty());
        assert!(handoff.receiver.as_mut().unwrap().try_recv().is_err());
        let mut bytes = vec![7; 960];
        wipe(&mut bytes);
        assert_eq!(bytes, vec![0; 960]);
    }
}
