//! A request-owned result for deferred recording initialization.
//!
//! This does not report encoded frames or file finalization. Dropping a
//! receiver cancels observation of that request without affecting another.

use std::sync::mpsc::{Receiver, TryRecvError};

/// Audio intent owned by one screen-recording request. Availability fallback
/// may still produce video-only output; this is not proof of device ownership.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordingAudioOptions {
    pub enabled: bool,
    pub device: String,
    pub bitrate: String,
}

impl RecordingAudioOptions {
    pub(crate) fn from_behavior(behavior: &crate::config::BehaviorConfig) -> Self {
        Self {
            enabled: behavior.recording_audio_enabled,
            device: behavior.recording_audio_device.clone(),
            bitrate: behavior.recording_audio_bitrate.clone(),
        }
    }

    pub(crate) fn snapshot() -> Self {
        Self::from_behavior(crate::config::CONFIG.load().behavior())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordingStartOutcome {
    Accepted,
    Rejected,
    /// The producer disappeared without reporting an initialization result.
    /// This must not be interpreted as proof that recording has stopped.
    Unavailable,
}

#[derive(Debug)]
pub struct RecordingStartReceipt {
    receiver: Option<Receiver<bool>>,
}

impl RecordingStartReceipt {
    /// Return one terminal outcome, without waiting or guessing a timeout.
    pub(crate) fn poll(&mut self) -> Option<RecordingStartOutcome> {
        let result = match self.receiver.as_ref()?.try_recv() {
            Ok(true) => RecordingStartOutcome::Accepted,
            Ok(false) => RecordingStartOutcome::Rejected,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => RecordingStartOutcome::Unavailable,
        };
        self.receiver = None;
        Some(result)
    }
}

#[cfg(any(feature = "backend-wayland-udev", test))]
#[derive(Debug)]
pub(crate) struct RecordingStartReporter(std::sync::mpsc::Sender<bool>);

#[cfg(any(feature = "backend-wayland-udev", test))]
impl RecordingStartReporter {
    /// Consume the only reporting capability after the actual start returns.
    pub(crate) fn complete(self, accepted: bool) {
        let _ = self.0.send(accepted);
    }
}

#[cfg(any(feature = "backend-wayland-udev", test))]
pub(crate) fn recording_start_channel() -> (RecordingStartReporter, RecordingStartReceipt) {
    let (sender, receiver) = std::sync::mpsc::channel();
    (
        RecordingStartReporter(sender),
        RecordingStartReceipt {
            receiver: Some(receiver),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_options_own_all_three_fields_from_one_behavior_snapshot() {
        let mut behavior = crate::config::CONFIG.load().behavior().clone();
        for enabled in [false, true] {
            behavior.recording_audio_enabled = enabled;
            behavior.recording_audio_device = "request-device".into();
            behavior.recording_audio_bitrate = "96k".into();
            let audio = RecordingAudioOptions::from_behavior(&behavior);
            behavior.recording_audio_enabled = !enabled;
            behavior.recording_audio_device = "next-device".into();
            behavior.recording_audio_bitrate = "192k".into();
            assert_eq!(
                audio,
                RecordingAudioOptions {
                    enabled,
                    device: "request-device".into(),
                    bitrate: "96k".into()
                }
            );
        }
    }

    #[test]
    fn explicit_outcomes_are_observed_once_without_waiting() {
        for (accepted, expected) in [
            (true, RecordingStartOutcome::Accepted),
            (false, RecordingStartOutcome::Rejected),
        ] {
            let (reporter, mut receipt) = recording_start_channel();
            assert_eq!(receipt.poll(), None);
            reporter.complete(accepted);
            assert_eq!(receipt.poll(), Some(expected));
            assert_eq!(receipt.poll(), None);
        }
    }

    #[test]
    fn a_dropped_reporter_is_unknown_instead_of_a_rejection() {
        let (reporter, mut receipt) = recording_start_channel();
        drop(reporter);
        assert_eq!(receipt.poll(), Some(RecordingStartOutcome::Unavailable));
        assert_eq!(receipt.poll(), None);
    }

    #[test]
    fn late_old_results_cannot_complete_a_new_request() {
        for accepted in [false, true] {
            let (old, receipt) = recording_start_channel();
            drop(receipt);
            let (new, mut receipt) = recording_start_channel();
            old.complete(accepted);
            assert_eq!(receipt.poll(), None);
            new.complete(!accepted);
            assert_eq!(
                receipt.poll(),
                Some(if accepted {
                    RecordingStartOutcome::Rejected
                } else {
                    RecordingStartOutcome::Accepted
                })
            );
        }
    }
}
