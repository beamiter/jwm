//! Request-owned, post-stop writer observations. A successful encoder exit
//! is deliberately silent: it is not authority over a public output pathname.

use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

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

#[derive(Debug)]
pub(crate) struct WriterCompletionReceipt(Option<Receiver<WriterOutcome>>);

impl WriterCompletionReceipt {
    fn poll(&mut self) -> Option<WriterOutcome> {
        let outcome = match self.0.as_ref()?.try_recv() {
            Ok(outcome) => outcome,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => WriterOutcome::Unknown,
        };
        self.0 = None;
        Some(outcome)
    }
}

/// The unique capability moves with the encoder Child, including its spawn
/// handoff. Dropping it never fabricates a successful process result.
pub(crate) struct WriterCompletionReporter(Sender<WriterOutcome>);

impl WriterCompletionReporter {
    pub(crate) fn finish(
        self,
        earlier_failure: Option<WriterFailure>,
        waited: Option<EncoderWaitOutcome>,
    ) {
        let _ = self.0.send(terminal_outcome(earlier_failure, waited));
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
    stop: Option<Receiver<Option<WriterCompletionReceipt>>>,
    writer: Option<WriterCompletionReceipt>,
    consumed: bool,
}

impl RecordingStopReceipt {
    #[cfg(any(feature = "x11-backends", test))]
    pub(crate) fn from_writer(writer: Option<WriterCompletionReceipt>) -> Self {
        Self {
            stop: None,
            writer,
            consumed: false,
        }
    }

    fn poll(&mut self) -> Option<WriterOutcome> {
        if self.consumed {
            return None;
        }
        if let Some(stop) = self.stop.as_ref() {
            match stop.try_recv() {
                Ok(writer) => {
                    self.writer = writer;
                    self.stop = None;
                }
                Err(TryRecvError::Empty) => return None,
                Err(TryRecvError::Disconnected) => {
                    self.stop = None;
                    self.consumed = true;
                    return Some(WriterOutcome::Unknown);
                }
            }
        }
        let outcome = match self.writer.as_mut() {
            Some(writer) => writer.poll()?,
            None => WriterOutcome::Unknown,
        };
        self.writer = None;
        self.consumed = true;
        Some(outcome)
    }
}

#[cfg(any(feature = "backend-wayland-udev", test))]
pub(crate) struct RecordingStopReporter(Sender<Option<WriterCompletionReceipt>>);

#[cfg(any(feature = "backend-wayland-udev", test))]
impl RecordingStopReporter {
    pub(crate) fn complete(self, writer: Option<WriterCompletionReceipt>) {
        let _ = self.0.send(writer);
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
        },
    )
}

const MAX_STOP_OBSERVATIONS: usize = 8;

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct StopNotices {
    pub(crate) failed: bool,
    pub(crate) unknown: bool,
}

#[derive(Debug, Default)]
pub(crate) struct RecordingStopObservations {
    pending: VecDeque<RecordingStopReceipt>,
    notices: StopNotices,
}

impl RecordingStopObservations {
    fn record(&mut self, outcome: WriterOutcome) {
        match outcome {
            WriterOutcome::Finished => {}
            WriterOutcome::Failed(_) => self.notices.failed = true,
            WriterOutcome::Unknown => self.notices.unknown = true,
        }
    }

    fn collect(&mut self) {
        // Each old observation is polled once; pending entries keep their age.
        for _ in 0..self.pending.len() {
            let mut receipt = self.pending.pop_front().expect("bounded observation count");
            match receipt.poll() {
                Some(outcome) => self.record(outcome),
                None => self.pending.push_back(receipt),
            }
        }
    }

    pub(crate) fn observe(&mut self, receipt: Option<RecordingStopReceipt>) {
        self.collect();
        let Some(mut receipt) = receipt else {
            self.notices.unknown = true;
            return;
        };
        if let Some(outcome) = receipt.poll() {
            self.record(outcome);
            return;
        }
        if self.pending.len() == MAX_STOP_OBSERVATIONS {
            // Retire observation only. The writer keeps its Child and works
            // independently; Stop is never blocked by notification capacity.
            self.pending.pop_front();
            self.notices.unknown = true;
        }
        self.pending.push_back(receipt);
    }

    pub(crate) fn take_notices(&mut self) -> StopNotices {
        self.collect();
        std::mem::take(&mut self.notices)
    }

    pub(crate) fn clear(&mut self) {
        self.pending.clear();
        self.notices = StopNotices::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            let report = format!("completion_reporter.finish(Some(WriterFailure::{reason}), None)");
            assert!(branch.find("child.kill()").unwrap() < branch.find("child.wait()").unwrap());
            assert!(branch.find("child.wait()").unwrap() < branch.find(&report).unwrap());
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
