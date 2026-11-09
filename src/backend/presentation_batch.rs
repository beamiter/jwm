//! Ownership for presentation feedback sampled from one rendered output batch.
//!
//! This module does not infer which surfaces were rendered. The backend must
//! move only feedback belonging to its actual sampled commits into `new`, before
//! dispatching another client commit, and move this owner into the DRM userdata.
//! Completion must come from that exact userdata returned by frame_submitted.
//! No callbacks are cloned; dropping queued, failed, or cancelled work discards.
use std::time::Duration;

/// Output lifetime plus submission identity, never a reusable CRTC alone.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Submission {
    pub output_generation: u64,
    pub serial: u64,
}

/// Evidence supplied by the backend only after matching the actual queued frame.
#[derive(Clone, Copy, Debug)]
pub(super) struct Completion {
    pub submission: Submission,
    pub monotonic_time: Option<Duration>,
    pub uncertain: bool,
}

/// A non-Clone owner. The discard function must not panic.
#[derive(Debug)]
pub(super) struct PresentationBatch<F> {
    submission: Submission,
    queued_after: Duration,
    callbacks: Vec<F>,
    discard: fn(F),
}

impl<F> PresentationBatch<F> {
    pub(super) fn new(
        submission: Submission,
        queued_after: Duration,
        callbacks: Vec<F>,
        discard: fn(F),
    ) -> Self {
        Self {
            submission,
            queued_after,
            callbacks,
            discard,
        }
    }

    /// Consumes the batch once. Invalid evidence discards, never defers/requeues.
    /// `present` receives the verified monotonic timestamp; refresh/flags/output
    /// still require truthful backend metadata. In particular, a successful
    /// renderer swap or an arbitrary vblank is not sufficient evidence.
    pub(super) fn complete(
        mut self,
        evidence: Completion,
        mut present: impl FnMut(F, Duration),
    ) -> bool {
        let Some(time) = evidence.monotonic_time else {
            return false;
        };
        if evidence.uncertain || evidence.submission != self.submission || time < self.queued_after
        {
            return false;
        }
        // Keep unprocessed callbacks owned by self even if a callback panics.
        while let Some(callback) = self.callbacks.pop() {
            present(callback, time);
        }
        true
    }
}

impl<F> Drop for PresentationBatch<F> {
    fn drop(&mut self) {
        for callback in self.callbacks.drain(..) {
            (self.discard)(callback);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    type Log = Rc<RefCell<Vec<(u8, bool)>>>;
    #[derive(Debug)]
    struct Callback(u8, Log);
    fn discard(cb: Callback) {
        cb.1.borrow_mut().push((cb.0, false));
    }
    fn present(cb: Callback, _: Duration) {
        cb.1.borrow_mut().push((cb.0, true));
    }
    const ID: Submission = Submission {
        output_generation: 7,
        serial: 42,
    };
    fn evidence() -> Completion {
        Completion {
            submission: ID,
            monotonic_time: Some(Duration::from_secs(2)),
            uncertain: false,
        }
    }
    fn batch(log: &Log) -> PresentationBatch<Callback> {
        PresentationBatch::new(
            ID,
            Duration::from_secs(1),
            vec![Callback(1, log.clone())],
            discard,
        )
    }
    #[test]
    fn matching_completion_presents_once_without_discard() {
        let log = Log::default();
        assert!(batch(&log).complete(evidence(), present));
        assert_eq!(*log.borrow(), [(1, true)]);
    }
    #[test]
    fn failed_submission_or_cancelled_userdata_discards_once() {
        let log = Log::default();
        drop(batch(&log));
        assert_eq!(*log.borrow(), [(1, false)]);
    }
    #[test]
    fn mismatched_output_or_submission_never_presents() {
        for id in [
            Submission {
                output_generation: 8,
                ..ID
            },
            Submission { serial: 41, ..ID },
        ] {
            let log = Log::default();
            assert!(!batch(&log).complete(
                Completion {
                    submission: id,
                    ..evidence()
                },
                present
            ));
            assert_eq!(*log.borrow(), [(1, false)]);
        }
    }
    #[test]
    fn missing_old_or_uncertain_timestamp_discards() {
        for completion in [
            Completion {
                monotonic_time: None,
                ..evidence()
            },
            Completion {
                monotonic_time: Some(Duration::ZERO),
                ..evidence()
            },
            Completion {
                uncertain: true,
                ..evidence()
            },
        ] {
            let log = Log::default();
            assert!(!batch(&log).complete(completion, present));
            assert_eq!(*log.borrow(), [(1, false)]);
        }
    }
    #[test]
    fn late_commit_remains_outside_previously_sampled_batch() {
        let log = Log::default();
        let mut current = vec![Callback(1, log.clone())];
        let queued = PresentationBatch::new(
            ID,
            Duration::from_secs(1),
            std::mem::take(&mut current),
            discard,
        );
        current.push(Callback(2, log.clone())); // client commit after queue
        assert!(queued.complete(evidence(), present));
        assert_eq!(*log.borrow(), [(1, true)]);
        assert_eq!(current.len(), 1);
        drop(PresentationBatch::new(
            Submission { serial: 43, ..ID },
            Duration::from_secs(3),
            current,
            discard,
        ));
        assert_eq!(*log.borrow(), [(1, true), (2, false)]);
    }
    #[test]
    fn replacing_queued_userdata_discards_old_not_new_batch() {
        let log = Log::default();
        let mut queued = Some(batch(&log));
        assert!(queued.is_some());
        let next = Submission { serial: 43, ..ID };
        queued = Some(PresentationBatch::new(
            next,
            Duration::from_secs(2),
            vec![Callback(2, log.clone())],
            discard,
        ));
        assert_eq!(*log.borrow(), [(1, false)]);
        assert!(queued.take().unwrap().complete(
            Completion {
                submission: next,
                monotonic_time: Some(Duration::from_secs(2)),
                uncertain: false,
            },
            present
        ));
        assert_eq!(*log.borrow(), [(1, false), (2, true)]);
    }

    #[test]
    fn separate_outputs_do_not_complete_each_others_callbacks() {
        let log = Log::default();
        let a = batch(&log);
        let b_id = Submission {
            output_generation: 8,
            ..ID
        };
        let b = PresentationBatch::new(
            b_id,
            Duration::ZERO,
            vec![Callback(2, log.clone())],
            discard,
        );
        assert!(a.complete(evidence(), present));
        assert_eq!(*log.borrow(), [(1, true)]);
        drop(b); // hot-unplug before its own flip
        assert_eq!(*log.borrow(), [(1, true), (2, false)]);
    }

    #[test]
    fn panic_during_completion_discards_remaining_owned_callbacks() {
        let log = Log::default();
        let queued = PresentationBatch::new(
            ID,
            Duration::ZERO,
            vec![Callback(1, log.clone()), Callback(2, log.clone())],
            discard,
        );
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            queued.complete(evidence(), |cb, time| {
                present(cb, time);
                panic!("consumer failed")
            });
        }));
        assert!(result.is_err());
        assert_eq!(*log.borrow(), [(2, true), (1, false)]);
    }
}
