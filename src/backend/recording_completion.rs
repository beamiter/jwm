//! Request-owned, post-stop writer observations. A successful encoder exit
//! is deliberately silent: it is not authority over a public output pathname.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

/// Failure-only feedback for one logical recording session. Old receipts keep
/// their own signal when the WM starts or clears a different session.
#[derive(Debug, Default, Clone)]
pub(crate) struct RecordingFailureSignal(Arc<AtomicBool>);

impl RecordingFailureSignal {
    pub(crate) fn failed(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    fn record(&self, outcome: WriterOutcome) {
        if matches!(outcome, WriterOutcome::Failed(_)) {
            self.0.store(true, Ordering::Release);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriterFailure {
    Spawn,
    Handoff,
    MissingStdin,
    PipeSetup,
    FrameWrite,
    FinalizeTimeout,
    EncoderExit,
    EncoderWait,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EncoderWaitOutcome {
    Exited(bool),
    TimedOut,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriterOutcome {
    Finished,
    Failed(WriterFailure),
    Unknown,
}

fn terminal_outcome(
    earlier_failure: Option<WriterFailure>,
    waited: Option<EncoderWaitOutcome>,
) -> WriterOutcome {
    if let Some(reason) = earlier_failure {
        return WriterOutcome::Failed(reason);
    }
    match waited {
        Some(EncoderWaitOutcome::Exited(true)) => WriterOutcome::Finished,
        Some(EncoderWaitOutcome::Exited(false)) => {
            WriterOutcome::Failed(WriterFailure::EncoderExit)
        }
        Some(EncoderWaitOutcome::TimedOut) => WriterOutcome::Failed(WriterFailure::FinalizeTimeout),
        Some(EncoderWaitOutcome::Failed) => WriterOutcome::Failed(WriterFailure::EncoderWait),
        None => WriterOutcome::Unknown,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WriterCompletion {
    outcome: WriterOutcome,
    encoder_exited: bool,
}

impl WriterCompletion {
    const UNKNOWN: Self = Self {
        outcome: WriterOutcome::Unknown,
        encoder_exited: false,
    };
    // This is not a saved-file result: a deferred request was cancelled before
    // it could create an encoder. Only its owning queue may publish this proof.
    const NEVER_STARTED: Self = Self {
        outcome: WriterOutcome::Unknown,
        encoder_exited: true,
    };
}

#[derive(Debug)]
pub(crate) struct WriterCompletionReceipt(Option<Receiver<WriterCompletion>>);

impl WriterCompletionReceipt {
    fn poll_completion(&mut self) -> Option<WriterCompletion> {
        let outcome = match self.0.as_ref()?.try_recv() {
            Ok(outcome) => outcome,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => WriterCompletion::UNKNOWN,
        };
        self.0 = None;
        Some(outcome)
    }
}

/// The unique capability moves with the encoder Child, including its spawn
/// handoff. Dropping it never fabricates a successful process result.
pub(crate) struct WriterCompletionReporter(Sender<WriterCompletion>);

impl WriterCompletionReporter {
    #[cfg(test)]
    pub(crate) fn finish(
        self,
        earlier_failure: Option<WriterFailure>,
        waited: Option<EncoderWaitOutcome>,
    ) {
        self.finish_with_exit(
            earlier_failure,
            waited,
            matches!(waited, Some(EncoderWaitOutcome::Exited(_))),
        );
    }

    pub(crate) fn finish_with_exit(
        self,
        earlier_failure: Option<WriterFailure>,
        waited: Option<EncoderWaitOutcome>,
        encoder_exited: bool,
    ) {
        let _ = self.0.send(WriterCompletion {
            outcome: terminal_outcome(earlier_failure, waited),
            encoder_exited,
        });
    }
}

pub(crate) fn writer_completion_channel() -> (WriterCompletionReporter, WriterCompletionReceipt) {
    let (sender, receiver) = std::sync::mpsc::channel();
    (
        WriterCompletionReporter(sender),
        WriterCompletionReceipt(Some(receiver)),
    )
}

/// An optional tracked-stop result. The deferred stop and the exact writer it
/// retires each transfer ownership once. This observes neither file identity
/// nor successful publication and cannot authorize a Saved notification.
#[derive(Debug)]
pub struct RecordingStopReceipt {
    stop: Option<Receiver<StopCompletion>>,
    writer: Option<WriterCompletionReceipt>,
    consumed: bool,
    never_started: bool,
    failure_signal: Option<RecordingFailureSignal>,
}

impl RecordingStopReceipt {
    #[cfg(any(feature = "x11-backends", test))]
    pub(crate) fn from_writer(writer: Option<WriterCompletionReceipt>) -> Self {
        Self {
            stop: None,
            writer,
            consumed: false,
            never_started: false,
            failure_signal: None,
        }
    }

    pub(crate) fn with_failure_signal(mut self, signal: RecordingFailureSignal) -> Self {
        self.failure_signal = Some(signal);
        self
    }

    pub(crate) fn poll(&mut self) -> Option<WriterOutcome> {
        self.poll_completion().map(|completion| completion.outcome)
    }

    fn poll_completion(&mut self) -> Option<WriterCompletion> {
        if self.consumed {
            return None;
        }
        if let Some(stop) = self.stop.as_ref() {
            match stop.try_recv() {
                Ok(StopCompletion::Writer(writer)) => {
                    self.writer = writer;
                    self.stop = None;
                }
                Ok(StopCompletion::NeverStarted) => {
                    self.never_started = true;
                    self.stop = None;
                }
                Err(TryRecvError::Empty) => return None,
                Err(TryRecvError::Disconnected) => {
                    self.stop = None;
                    self.consumed = true;
                    return Some(WriterCompletion::UNKNOWN);
                }
            }
        }
        let completion = match self.writer.as_mut() {
            Some(writer) => writer.poll_completion()?,
            None if self.never_started => WriterCompletion::NEVER_STARTED,
            None => WriterCompletion::UNKNOWN,
        };
        if let Some(signal) = &self.failure_signal {
            signal.record(completion.outcome);
        }
        self.writer = None;
        self.consumed = true;
        Some(completion)
    }
}

#[cfg_attr(not(any(feature = "backend-wayland-udev", test)), allow(dead_code))]
enum StopCompletion {
    Writer(Option<WriterCompletionReceipt>),
    NeverStarted,
}

#[cfg(any(feature = "backend-wayland-udev", test))]
pub(crate) struct RecordingStopReporter(Sender<StopCompletion>);

#[cfg(any(feature = "backend-wayland-udev", test))]
impl RecordingStopReporter {
    pub(crate) fn complete(self, writer: Option<WriterCompletionReceipt>) {
        let _ = self.0.send(StopCompletion::Writer(writer));
    }

    /// Only valid when the same tracked, unconsumed start was removed from
    /// its queue. A missing current sink alone never provides this authority.
    pub(crate) fn complete_without_encoder(self) {
        let _ = self.0.send(StopCompletion::NeverStarted);
    }
}

#[cfg(any(feature = "backend-wayland-udev", test))]
pub(crate) fn recording_stop_channel() -> (RecordingStopReporter, RecordingStopReceipt) {
    let (sender, receiver) = std::sync::mpsc::channel();
    (
        RecordingStopReporter(sender),
        RecordingStopReceipt {
            stop: Some(receiver),
            writer: None,
            consumed: false,
            never_started: false,
            failure_signal: None,
        },
    )
}

const MAX_STOP_OBSERVATIONS: usize = 8;

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct StopNotices {
    pub(crate) failed: bool,
    pub(crate) unknown: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioRelease {
    Available,
    Pending,
    Unconfirmed,
}

impl AudioRelease {
    pub(crate) fn refusal(self) -> Option<&'static str> {
        match self {
            Self::Available => None,
            Self::Pending => {
                Some("The previous recording encoder is still finishing; try again when it exits.")
            }
            Self::Unconfirmed => Some(
                "The previous recording encoder's exit could not be confirmed. Check its log and any remaining recording encoder, close that recording safely, then restart JWM before recording audio again.",
            ),
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct RecordingStopObservations {
    pending: VecDeque<(RecordingStopReceipt, bool, bool)>,
    audio_unconfirmed: bool,
    notices: StopNotices,
}

impl RecordingStopObservations {
    fn record(
        &mut self,
        completion: WriterCompletion,
        protects_audio: bool,
        failure_reported: bool,
    ) {
        if protects_audio && !completion.encoder_exited {
            self.audio_unconfirmed = true;
            self.notices.unknown = true;
        }
        match completion.outcome {
            WriterOutcome::Finished => {}
            WriterOutcome::Failed(_) => self.notices.failed |= !failure_reported,
            WriterOutcome::Unknown => self.notices.unknown = true,
        }
    }

    /// Poll ready receipts without waiting or consuming their UI notices.
    pub(crate) fn collect(&mut self) {
        // Each old observation is polled once; pending entries keep their age.
        for _ in 0..self.pending.len() {
            let (mut receipt, protects_audio, failure_reported) =
                self.pending.pop_front().expect("bounded observation count");
            match receipt.poll_completion() {
                Some(outcome) => self.record(outcome, protects_audio, failure_reported),
                None => self
                    .pending
                    .push_back((receipt, protects_audio, failure_reported)),
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn observe(&mut self, receipt: Option<RecordingStopReceipt>) {
        self.observe_with_audio(receipt, false);
    }

    pub(crate) fn observe_with_audio(
        &mut self,
        receipt: Option<RecordingStopReceipt>,
        protects_audio: bool,
    ) {
        self.observe_with_notice(receipt, protects_audio, false);
    }

    /// An already reported runtime failure still needs its exact exit proof,
    /// especially when the encoder also owns audio. Suppress only that repeat
    /// failure; a later unknown exit remains a distinct actionable warning.
    pub(crate) fn observe_with_notice(
        &mut self,
        receipt: Option<RecordingStopReceipt>,
        protects_audio: bool,
        failure_reported: bool,
    ) {
        self.collect();
        let Some(mut receipt) = receipt else {
            self.notices.unknown = true;
            self.audio_unconfirmed |= protects_audio;
            return;
        };
        if let Some(outcome) = receipt.poll_completion() {
            self.record(outcome, protects_audio, failure_reported);
            return;
        }
        if self.pending.len() == MAX_STOP_OBSERVATIONS {
            // Prefer retiring notification-only work. If every bounded slot
            // protects audio, retain a conservative unconfirmed reservation;
            // discarding a receiver does not terminate its encoder.
            let non_audio = self.pending.iter().position(|(_, audio, _)| !audio);
            if non_audio.is_none() && !protects_audio {
                self.notices.unknown = true;
                return;
            }
            let index = non_audio.unwrap_or(0);
            let (_, removed_audio, _) = self
                .pending
                .remove(index)
                .expect("bounded observation count");
            self.audio_unconfirmed |= removed_audio;
            self.notices.unknown = true;
        }
        self.pending
            .push_back((receipt, protects_audio, failure_reported));
    }

    pub(crate) fn audio_release(&mut self) -> AudioRelease {
        self.collect();
        if self.audio_unconfirmed {
            AudioRelease::Unconfirmed
        } else if self.pending.iter().any(|(_, audio, _)| *audio) {
            AudioRelease::Pending
        } else {
            AudioRelease::Available
        }
    }

    pub(crate) fn take_notices(&mut self) -> StopNotices {
        self.collect();
        std::mem::take(&mut self.notices)
    }

    pub(crate) fn clear(&mut self) {
        // Reset UI observations without pretending a still-owned encoder was
        // released. The bounded audio responsibility survives feature resets
        // and is finally dropped with the WM itself on shutdown.
        self.pending.retain(|(_, audio, _)| *audio);
        self.notices = StopNotices::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopped_failure_feedback_is_owned_and_does_not_consume_notices() {
        for deferred in [false, true] {
            let signal = RecordingFailureSignal::default();
            let other = RecordingFailureSignal::default();
            let (writer, completion) = writer_completion_channel();
            let mut observations = RecordingStopObservations::default();
            let receipt = if deferred {
                let (stop, receipt) = recording_stop_channel();
                stop.complete(Some(completion));
                receipt
            } else {
                RecordingStopReceipt::from_writer(Some(completion))
            };
            observations
                .observe_with_audio(Some(receipt.with_failure_signal(signal.clone())), true);
            observations.collect();
            assert!(!signal.failed());
            assert_eq!(observations.audio_release(), AudioRelease::Pending);

            writer.finish(
                Some(WriterFailure::FrameWrite),
                Some(EncoderWaitOutcome::Exited(false)),
            );
            observations.collect();
            assert!(signal.failed());
            assert!(!other.failed());
            assert_eq!(observations.audio_release(), AudioRelease::Available);
            assert!(observations.take_notices().failed);
            assert_eq!(observations.take_notices(), StopNotices::default());
            observations.collect();
            assert!(signal.failed());
        }
    }

    #[test]
    fn stopped_success_unknown_and_cancelled_start_do_not_signal_failure() {
        for outcome in [Some(EncoderWaitOutcome::Exited(true)), None] {
            let signal = RecordingFailureSignal::default();
            let (writer, completion) = writer_completion_channel();
            let mut receipt = RecordingStopReceipt::from_writer(Some(completion))
                .with_failure_signal(signal.clone());
            writer.finish(None, outcome);
            assert!(receipt.poll().is_some());
            assert!(!signal.failed());
        }
        let signal = RecordingFailureSignal::default();
        let (stop, receipt) = recording_stop_channel();
        let mut receipt = receipt.with_failure_signal(signal.clone());
        stop.complete_without_encoder();
        assert_eq!(receipt.poll(), Some(WriterOutcome::Unknown));
        assert!(!signal.failed());
    }

    #[test]
    fn reported_runtime_failure_suppresses_only_its_own_repeat_notice() {
        for immediate in [false, true] {
            let mut observations = RecordingStopObservations::default();
            let (writer, receipt) = writer_completion_channel();
            if immediate {
                writer.finish(
                    Some(WriterFailure::FrameWrite),
                    Some(EncoderWaitOutcome::Exited(false)),
                );
                observations.observe_with_notice(
                    Some(RecordingStopReceipt::from_writer(Some(receipt))),
                    true,
                    true,
                );
            } else {
                observations.observe_with_notice(
                    Some(RecordingStopReceipt::from_writer(Some(receipt))),
                    true,
                    true,
                );
                assert_eq!(observations.audio_release(), AudioRelease::Pending);
                writer.finish(
                    Some(WriterFailure::FrameWrite),
                    Some(EncoderWaitOutcome::Exited(false)),
                );
            }
            assert_eq!(observations.audio_release(), AudioRelease::Available);
            assert_eq!(observations.take_notices(), StopNotices::default());
            let (writer, receipt) = writer_completion_channel();
            writer.finish(
                Some(WriterFailure::FrameWrite),
                Some(EncoderWaitOutcome::Exited(false)),
            );
            observations.observe_with_audio(
                Some(RecordingStopReceipt::from_writer(Some(receipt))),
                false,
            );
            assert!(
                observations.take_notices().failed,
                "an unrelated failure must still be reported"
            );
        }
    }

    #[test]
    fn reported_runtime_failure_keeps_unconfirmed_audio_release_actionable() {
        let mut observations = RecordingStopObservations::default();
        let (writer, receipt) = writer_completion_channel();
        observations.observe_with_notice(
            Some(RecordingStopReceipt::from_writer(Some(receipt))),
            true,
            true,
        );
        drop(writer);
        assert_eq!(observations.audio_release(), AudioRelease::Unconfirmed);
        let notices = observations.take_notices();
        assert!(notices.unknown);
        assert!(!notices.failed);
        assert_eq!(observations.take_notices(), StopNotices::default());
    }

    #[test]
    fn audio_release_waits_for_exact_writer_even_when_file_result_failed() {
        for failure in [
            None,
            Some(WriterFailure::FrameWrite),
            Some(WriterFailure::EncoderExit),
        ] {
            let mut observations = RecordingStopObservations::default();
            let (writer, receipt) = writer_completion_channel();
            observations
                .observe_with_audio(Some(RecordingStopReceipt::from_writer(Some(receipt))), true);
            assert_eq!(observations.audio_release(), AudioRelease::Pending);
            writer.finish_with_exit(failure, Some(EncoderWaitOutcome::Exited(false)), true);
            assert_eq!(observations.audio_release(), AudioRelease::Available);
            assert!(observations.take_notices().failed);
        }
    }

    #[test]
    fn audio_release_unknown_is_sticky_and_never_borrowed_from_another_request() {
        for missing in 0..3 {
            let mut observations = RecordingStopObservations::default();
            let receipt = match missing {
                0 => None,
                1 => Some(RecordingStopReceipt::from_writer(None)),
                _ => {
                    let (writer, receipt) = writer_completion_channel();
                    drop(writer);
                    Some(RecordingStopReceipt::from_writer(Some(receipt)))
                }
            };
            observations.observe_with_audio(receipt, true);
            assert_eq!(observations.audio_release(), AudioRelease::Unconfirmed);
            let (writer, receipt) = writer_completion_channel();
            observations.observe_with_audio(
                Some(RecordingStopReceipt::from_writer(Some(receipt))),
                false,
            );
            writer.finish_with_exit(None, Some(EncoderWaitOutcome::Exited(true)), true);
            assert_eq!(observations.audio_release(), AudioRelease::Unconfirmed);
            assert!(
                observations
                    .audio_release()
                    .refusal()
                    .unwrap()
                    .contains("restart JWM")
            );
        }
    }

    #[test]
    fn never_started_proof_releases_only_its_own_audio_observation() {
        let mut observations = RecordingStopObservations::default();
        let (old_writer, old) = writer_completion_channel();
        observations.observe_with_audio(Some(RecordingStopReceipt::from_writer(Some(old))), true);
        let (stop, cancelled) = recording_stop_channel();
        observations.observe_with_audio(Some(cancelled), true);
        stop.complete_without_encoder();
        assert_eq!(observations.audio_release(), AudioRelease::Pending);
        old_writer.finish_with_exit(Some(WriterFailure::FrameWrite), None, true);
        assert_eq!(observations.audio_release(), AudioRelease::Available);
    }

    #[test]
    fn bounded_audio_observations_prefer_non_audio_and_fail_closed_on_forced_overflow() {
        let mut observations = RecordingStopObservations::default();
        let (audio_writer, receipt) = writer_completion_channel();
        observations
            .observe_with_audio(Some(RecordingStopReceipt::from_writer(Some(receipt))), true);
        let mut writers = Vec::new();
        for _ in 0..MAX_STOP_OBSERVATIONS * 3 {
            let (writer, receipt) = writer_completion_channel();
            writers.push(writer);
            observations.observe_with_audio(
                Some(RecordingStopReceipt::from_writer(Some(receipt))),
                false,
            );
            assert!(observations.pending.len() <= MAX_STOP_OBSERVATIONS);
            assert_eq!(observations.audio_release(), AudioRelease::Pending);
        }
        audio_writer.finish_with_exit(None, Some(EncoderWaitOutcome::Exited(true)), true);
        assert_eq!(observations.audio_release(), AudioRelease::Available);
        let mut forced = RecordingStopObservations::default();
        for _ in 0..MAX_STOP_OBSERVATIONS + 1 {
            let (writer, receipt) = writer_completion_channel();
            writers.push(writer);
            forced.observe_with_audio(Some(RecordingStopReceipt::from_writer(Some(receipt))), true);
        }
        assert_eq!(forced.pending.len(), MAX_STOP_OBSERVATIONS);
        assert_eq!(forced.audio_release(), AudioRelease::Unconfirmed);
        for writer in writers {
            writer.finish_with_exit(None, Some(EncoderWaitOutcome::Exited(true)), true);
        }
        assert_eq!(forced.audio_release(), AudioRelease::Unconfirmed);
    }

    #[test]
    fn incoming_non_audio_cannot_evict_a_full_set_of_audio_owners() {
        let mut observations = RecordingStopObservations::default();
        let mut writers = Vec::new();
        for _ in 0..MAX_STOP_OBSERVATIONS {
            let (writer, receipt) = writer_completion_channel();
            writers.push(writer);
            observations
                .observe_with_audio(Some(RecordingStopReceipt::from_writer(Some(receipt))), true);
        }
        let (_unrelated_writer, receipt) = writer_completion_channel();
        observations.observe_with_audio(
            Some(RecordingStopReceipt::from_writer(Some(receipt))),
            false,
        );
        assert_eq!(observations.pending.len(), MAX_STOP_OBSERVATIONS);
        assert_eq!(observations.audio_release(), AudioRelease::Pending);
        for writer in writers {
            writer.finish_with_exit(None, Some(EncoderWaitOutcome::Exited(true)), true);
        }
        assert_eq!(observations.audio_release(), AudioRelease::Available);
    }

    #[test]
    fn successful_file_outcome_without_exit_proof_does_not_release_audio() {
        let mut observations = RecordingStopObservations::default();
        let (writer, receipt) = writer_completion_channel();
        observations
            .observe_with_audio(Some(RecordingStopReceipt::from_writer(Some(receipt))), true);
        writer.finish_with_exit(None, Some(EncoderWaitOutcome::Exited(true)), false);
        assert_eq!(observations.audio_release(), AudioRelease::Unconfirmed);
    }

    #[test]
    fn earlier_failures_and_timeout_cannot_become_successful_exit() {
        for reason in [
            WriterFailure::Spawn,
            WriterFailure::Handoff,
            WriterFailure::MissingStdin,
            WriterFailure::PipeSetup,
            WriterFailure::FrameWrite,
        ] {
            for waited in [
                None,
                Some(EncoderWaitOutcome::Exited(true)),
                Some(EncoderWaitOutcome::Exited(false)),
                Some(EncoderWaitOutcome::TimedOut),
                Some(EncoderWaitOutcome::Failed),
            ] {
                assert_eq!(
                    terminal_outcome(Some(reason), waited),
                    WriterOutcome::Failed(reason)
                );
            }
        }
        for (waited, expected) in [
            (
                Some(EncoderWaitOutcome::Exited(true)),
                WriterOutcome::Finished,
            ),
            (
                Some(EncoderWaitOutcome::Exited(false)),
                WriterOutcome::Failed(WriterFailure::EncoderExit),
            ),
            (
                Some(EncoderWaitOutcome::TimedOut),
                WriterOutcome::Failed(WriterFailure::FinalizeTimeout),
            ),
            (
                Some(EncoderWaitOutcome::Failed),
                WriterOutcome::Failed(WriterFailure::EncoderWait),
            ),
            (None, WriterOutcome::Unknown),
        ] {
            assert_eq!(terminal_outcome(None, waited), expected);
        }
    }

    #[test]
    fn stop_delivery_and_writer_completion_each_transfer_ownership_once() {
        let (stop, mut receipt) = recording_stop_channel();
        let (writer, completion) = writer_completion_channel();
        assert_eq!(receipt.poll(), None);
        stop.complete(Some(completion));
        assert_eq!(receipt.poll(), None);
        writer.finish(
            Some(WriterFailure::FrameWrite),
            Some(EncoderWaitOutcome::Exited(true)),
        );
        assert_eq!(
            receipt.poll(),
            Some(WriterOutcome::Failed(WriterFailure::FrameWrite))
        );
        assert_eq!(receipt.poll(), None);
    }

    #[test]
    fn missing_stop_sink_and_dropped_producers_are_unknown() {
        let (stop, mut receipt) = recording_stop_channel();
        drop(stop);
        assert_eq!(receipt.poll(), Some(WriterOutcome::Unknown));
        assert_eq!(receipt.poll(), None);
        let mut receipt = RecordingStopReceipt::from_writer(None);
        assert_eq!(receipt.poll(), Some(WriterOutcome::Unknown));
        let (writer, completion) = writer_completion_channel();
        let mut receipt = RecordingStopReceipt::from_writer(Some(completion));
        drop(writer);
        assert_eq!(receipt.poll(), Some(WriterOutcome::Unknown));
        assert_eq!(receipt.poll(), None);
    }

    #[test]
    fn stopped_observers_are_bounded_without_canceling_their_writers() {
        let mut observations = RecordingStopObservations::default();
        let mut writers = Vec::new();
        for _ in 0..MAX_STOP_OBSERVATIONS * 3 {
            let (writer, completion) = writer_completion_channel();
            writers.push(writer);
            observations.observe(Some(RecordingStopReceipt::from_writer(Some(completion))));
            assert!(observations.pending.len() <= MAX_STOP_OBSERVATIONS);
        }
        assert_eq!(observations.pending.len(), MAX_STOP_OBSERVATIONS);
        assert_eq!(
            observations.take_notices(),
            StopNotices {
                failed: false,
                unknown: true
            }
        );
        assert_eq!(observations.take_notices(), StopNotices::default());
        // Eviction dropped receivers only: all independent producers still
        // exist and can settle, including those no longer observed.
        for writer in writers {
            writer.finish(None, Some(EncoderWaitOutcome::Exited(true)));
        }
        assert_eq!(observations.take_notices(), StopNotices::default());
        assert!(observations.pending.is_empty());
    }

    #[test]
    fn ready_old_observations_are_collected_before_admitting_another_stop() {
        let mut observations = RecordingStopObservations::default();
        let mut writers = VecDeque::new();
        for _ in 0..MAX_STOP_OBSERVATIONS {
            let (writer, completion) = writer_completion_channel();
            writers.push_back(writer);
            observations.observe(Some(RecordingStopReceipt::from_writer(Some(completion))));
        }
        writers
            .pop_front()
            .unwrap()
            .finish(None, Some(EncoderWaitOutcome::Exited(true)));
        let (writer, completion) = writer_completion_channel();
        writers.push_back(writer);
        observations.observe(Some(RecordingStopReceipt::from_writer(Some(completion))));
        assert_eq!(observations.pending.len(), MAX_STOP_OBSERVATIONS);
        assert_eq!(observations.take_notices(), StopNotices::default());
    }

    #[test]
    fn failure_and_unknown_notices_coalesce_and_success_stays_silent() {
        let mut observations = RecordingStopObservations::default();
        for _ in 0..12 {
            observations.observe(None);
        }
        let (writer, completion) = writer_completion_channel();
        observations.observe(Some(RecordingStopReceipt::from_writer(Some(completion))));
        writer.finish(Some(WriterFailure::EncoderExit), None);
        assert_eq!(
            observations.take_notices(),
            StopNotices {
                failed: true,
                unknown: true
            }
        );
        assert_eq!(observations.take_notices(), StopNotices::default());
        let (writer, completion) = writer_completion_channel();
        observations.observe(Some(RecordingStopReceipt::from_writer(Some(completion))));
        observations.clear();
        writer.finish(Some(WriterFailure::FrameWrite), None);
        assert_eq!(observations.take_notices(), StopNotices::default());
    }

    #[test]
    fn real_writer_failure_paths_use_owned_terminal_reporting_after_cleanup() {
        let source = include_str!("compositor_common/recording_sink.rs");
        let shipped = source.split_once("#[cfg(test)]").unwrap().0;
        assert!(shipped.contains("channel::<(Child, WriterCompletionReporter)>()"));
        assert!(shipped.contains("child_tx.send((child, completion_reporter))"));
        for (start, end, reason) in [
            (
                "let Some(mut stdin) = child.stdin.take() else {",
                "widen_pipe(&stdin",
                "MissingStdin",
            ),
            (
                "if let Err(error) = set_nonblocking",
                "let mut write_failed",
                "PipeSetup",
            ),
            ("let writer = match writer", "        Self {", "Spawn"),
        ] {
            let branch = shipped
                .split_once(start)
                .unwrap()
                .1
                .split_once(end)
                .unwrap()
                .0;
            // The spawn arm follows the handoff arm; isolate its own cleanup.
            let branch = if reason == "Spawn" {
                branch.split_once("Err(error) => {").unwrap().1
            } else {
                branch
            };
            let report = format!("WriterFailure::{reason}");
            assert!(branch.find("child.kill()").unwrap() < branch.find("child.wait()").unwrap());
            assert!(branch.find("child.wait()").unwrap() < branch.find(&report).unwrap());
            assert!(branch.contains("let encoder_exited = child.wait().is_ok()"));
            assert!(branch.contains("completion_reporter.finish_with_exit("));
            assert!(branch.contains("encoder_exited"));
        }
        let handoff = shipped
            .split_once("let (mut child, completion_reporter) = error.0;")
            .unwrap()
            .1;
        assert!(handoff.find("child.kill()").unwrap() < handoff.find("child.wait()").unwrap());
        assert!(
            handoff.find("child.wait()").unwrap() < handoff.find("WriterFailure::Handoff").unwrap()
        );
        assert!(shipped.contains("write_failed.then_some(WriterFailure::FrameWrite)"));
        assert!(shipped.contains("let outcome = if waited.timed_out"));
        let timeout = shipped
            .split_once("let kill_error = child")
            .unwrap()
            .1
            .split_once("/// Owns the encoder child")
            .unwrap()
            .0;
        assert!(timeout.contains("timed_out: true"));
        let finish = shipped
            .split_once("pub(crate) fn finish_tracked")
            .unwrap()
            .1
            .split_once("fn stats(&self)")
            .unwrap()
            .0;
        assert!(!finish.contains(".join("));
        assert!(
            finish.find("self.frames = None").unwrap()
                < finish.find("drop(self.writer.take())").unwrap()
        );
        assert!(finish.contains("(stats, self.completion)"));
    }

    #[test]
    fn completion_capabilities_can_move_between_their_owners() {
        fn assert_send<T: Send>() {}
        assert_send::<WriterCompletionReporter>();
        assert_send::<WriterCompletionReceipt>();
        assert_send::<RecordingStopReceipt>();
        assert_send::<RecordingStopReporter>();
        assert_send::<RecordingStopObservations>();
    }
}
