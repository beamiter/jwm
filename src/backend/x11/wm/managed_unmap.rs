//! Classification for X11 `UnmapNotify` events initiated by JWM itself.
//!
//! A managed client selects `StructureNotify` on its own window while JWM
//! selects `SubstructureNotify` on the root. One `UnmapWindow` request can
//! therefore yield two notifications to the same WM connection: one whose
//! `event` field is the client and one whose `event` field is the root. The
//! first copy must produce one lifecycle event and the second must disappear;
//! neither may be mistaken for a client withdrawal.
//!
//! Matching by window alone is unsafe. A client can send the ICCCM synthetic
//! `UnmapNotify` used to enter WithdrawnState while a WM request is pending,
//! and a server-generated unmap caused by `UnmapGravity` is likewise not the
//! acknowledgement of JWM's request. Both transports therefore record the
//! request sequence returned by their void cookie and pass the complete raw
//! notification metadata through this small transport-independent classifier.

use crate::backend::api::ManagedUnmapReason;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

/// Enough outstanding requests to cover rapid state reversals without letting
/// a no-op request retain unbounded state. In normal operation swallowing has
/// one entry and consumes it on the next pair of X events.
const MAX_PENDING_PER_WINDOW: usize = 8;

pub(crate) type SharedManagedUnmaps = Arc<Mutex<ManagedUnmapTracker>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ManagedUnmapCapacityError {
    window: u32,
}

impl std::fmt::Display for ManagedUnmapCapacityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "managed-unmap queue for X11 window 0x{:x} already has {MAX_PENDING_PER_WINDOW} pending requests",
            self.window
        )
    }
}

impl std::error::Error for ManagedUnmapCapacityError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ManagedUnmapDisposition {
    /// The first root/client delivery for one JWM-owned request.
    ManagerOwned(ManagedUnmapReason),
    /// A second delivery (or an exact replay) of an already-emitted request.
    ManagerDuplicate,
    /// A client/server transition that must follow ordinary withdrawal rules.
    External,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DeliveryCopies {
    client: bool,
    parent: bool,
}

impl DeliveryCopies {
    fn complete(self) -> bool {
        self.client && self.parent
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingManagedUnmap {
    full_sequence: u64,
    reason: ManagedUnmapReason,
    copies: DeliveryCopies,
    emitted: bool,
    /// The owning lifecycle already ended independently (ICCCM synthetic
    /// withdrawal), so remaining server copies may only be swallowed.
    suppressed: bool,
}

impl PendingManagedUnmap {
    fn wire_sequence(self) -> u16 {
        self.full_sequence as u16
    }
}

/// Per-XID queue of JWM-issued `UnmapWindow` requests awaiting notification.
///
/// The protocol carries only the low 16 bits in an event. Keeping a short
/// queue bounds wraparound ambiguity to a window of at most eight JWM-owned
/// requests, many orders of magnitude below a sequence wrap.
#[derive(Debug, Default)]
pub(crate) struct ManagedUnmapTracker {
    pending: HashMap<u32, VecDeque<PendingManagedUnmap>>,
}

impl ManagedUnmapTracker {
    /// Check that another request can be issued without displacing a request
    /// whose `UnmapNotify` acknowledgement has not arrived yet.
    ///
    /// Transports hold the tracker mutex from this admission check through
    /// the checked X request and [`Self::record`], so another sender cannot
    /// consume the last slot between admission and commit.
    pub(crate) fn ensure_capacity(&self, window: u32) -> Result<(), ManagedUnmapCapacityError> {
        if self.pending.get(&window).map_or(0, VecDeque::len) >= MAX_PENDING_PER_WINDOW {
            return Err(ManagedUnmapCapacityError { window });
        }
        Ok(())
    }

    /// Record a successfully checked transport request. Callers must not add
    /// an entry for a request that failed before reaching the X server. Full
    /// queues are rejected rather than evicting an unacknowledged marker.
    pub(crate) fn record(
        &mut self,
        window: u32,
        full_sequence: u64,
        reason: ManagedUnmapReason,
    ) -> Result<(), ManagedUnmapCapacityError> {
        self.ensure_capacity(window)?;
        let queue = self.pending.entry(window).or_default();
        queue.push_back(PendingManagedUnmap {
            full_sequence,
            reason,
            copies: DeliveryCopies::default(),
            emitted: false,
            suppressed: false,
        });
        Ok(())
    }

    /// Classify one raw `UnmapNotify` before the compositor or JWM sees it.
    ///
    /// `synthetic` is the core event's SendEvent bit. `event_window` is the
    /// raw XID in its `event` field, not the XID that was unmapped.
    pub(crate) fn classify(
        &mut self,
        root: u32,
        window: u32,
        event_window: u32,
        sequence: u16,
        synthetic: bool,
        from_configure: bool,
    ) -> ManagedUnmapDisposition {
        // A synthetic event is the ICCCM withdrawal signal. It ends WM
        // ownership, but the server-generated copies of an already-checked
        // UnmapWindow can still be queued behind it. Keep their exact
        // sequence records as suppression-only tombstones: otherwise those
        // late copies would be reclassified as external lifecycle events (or
        // clear a newer generation recorded for the same still-live XID).
        // DestroyNotify is the separate hard boundary that drops tombstones.
        if synthetic {
            if let Some(queue) = self.pending.get_mut(&window) {
                for pending in queue {
                    pending.emitted = true;
                    pending.suppressed = true;
                }
            }
            return ManagedUnmapDisposition::External;
        }

        // `from_configure` describes a server gravity transition. It cannot
        // acknowledge UnmapWindow, even when its low sequence coincides, but
        // it also must not consume the independent WM request that is still
        // expected to produce its own notification.
        if from_configure {
            return ManagedUnmapDisposition::External;
        }

        let is_client_copy = event_window == window;
        let is_parent_copy = event_window == root;
        if !is_client_copy && !is_parent_copy {
            self.clear(window);
            return ManagedUnmapDisposition::External;
        }

        let Some(index) = self.pending.get(&window).and_then(|queue| {
            // A tombstone can survive long enough for the 16-bit event
            // sequence to wrap. Prefer the newest live request with the same
            // wire value; only fall back to an old suppression record after
            // every live candidate has consumed its two deliveries.
            queue
                .iter()
                .rposition(|pending| pending.wire_sequence() == sequence && !pending.suppressed)
                .or_else(|| {
                    queue
                        .iter()
                        .position(|pending| pending.wire_sequence() == sequence)
                })
        }) else {
            let had_pending = self.pending.contains_key(&window);
            if had_pending {
                // The window is now physically unmapped for a different
                // reason; an outstanding WM request can no longer create that
                // transition.
                self.clear(window);
            }
            return ManagedUnmapDisposition::External;
        };

        let queue = self
            .pending
            .get_mut(&window)
            .expect("matched managed-unmap queue disappeared");
        let pending = &mut queue[index];
        if is_client_copy {
            pending.copies.client = true;
        }
        if is_parent_copy {
            pending.copies.parent = true;
        }

        let disposition = if pending.emitted {
            ManagedUnmapDisposition::ManagerDuplicate
        } else {
            pending.emitted = true;
            ManagedUnmapDisposition::ManagerOwned(pending.reason)
        };

        if pending.copies.complete() {
            queue.remove(index);
        }
        if queue.is_empty() {
            self.pending.remove(&window);
        }
        disposition
    }

    /// Destruction is authoritative and invalidates every pending transition
    /// for the XID. A subsequently delivered notification cannot inherit the
    /// old reason, including after the server reuses the numeric XID.
    pub(crate) fn clear(&mut self, window: u32) {
        self.pending.remove(&window);
    }

    #[cfg(test)]
    fn pending_for(&self, window: u32) -> usize {
        self.pending.get(&window).map_or(0, VecDeque::len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: u32 = 1;
    const WINDOW: u32 = 0x420;
    const REASONS: [ManagedUnmapReason; 2] = [
        ManagedUnmapReason::SwallowDiscard,
        ManagedUnmapReason::IconifyRetain { generation: 7 },
    ];

    fn classify(
        tracker: &mut ManagedUnmapTracker,
        event_window: u32,
        sequence: u16,
    ) -> ManagedUnmapDisposition {
        tracker.classify(ROOT, WINDOW, event_window, sequence, false, false)
    }

    #[test]
    fn root_and_client_copies_emit_one_manager_transition() {
        for reason in REASONS {
            let mut tracker = ManagedUnmapTracker::default();
            tracker.record(WINDOW, 41, reason).unwrap();

            assert_eq!(
                classify(&mut tracker, WINDOW, 41),
                ManagedUnmapDisposition::ManagerOwned(reason)
            );
            assert_eq!(
                classify(&mut tracker, ROOT, 41),
                ManagedUnmapDisposition::ManagerDuplicate
            );
            assert_eq!(tracker.pending_for(WINDOW), 0);
        }
    }

    #[test]
    fn root_copy_can_arrive_first_without_changing_the_result() {
        for reason in REASONS {
            let mut tracker = ManagedUnmapTracker::default();
            tracker.record(WINDOW, 42, reason).unwrap();

            assert_eq!(
                classify(&mut tracker, ROOT, 42),
                ManagedUnmapDisposition::ManagerOwned(reason)
            );
            assert_eq!(
                classify(&mut tracker, WINDOW, 42),
                ManagedUnmapDisposition::ManagerDuplicate
            );
        }
    }

    #[test]
    fn synthetic_withdrawal_tombstones_late_checked_unmap_copies() {
        for reason in REASONS {
            let mut tracker = ManagedUnmapTracker::default();
            tracker.record(WINDOW, 43, reason).unwrap();

            assert_eq!(
                tracker.classify(ROOT, WINDOW, ROOT, 43, true, false),
                ManagedUnmapDisposition::External
            );
            assert_eq!(tracker.pending_for(WINDOW), 1);
            assert_eq!(
                classify(&mut tracker, WINDOW, 43),
                ManagedUnmapDisposition::ManagerDuplicate
            );
            assert_eq!(
                classify(&mut tracker, ROOT, 43),
                ManagedUnmapDisposition::ManagerDuplicate
            );
            assert_eq!(tracker.pending_for(WINDOW), 0);
        }
    }

    #[test]
    fn withdrawal_tombstone_consumes_only_its_generation() {
        let mut tracker = ManagedUnmapTracker::default();
        tracker
            .record(
                WINDOW,
                43,
                ManagedUnmapReason::IconifyRetain { generation: 1 },
            )
            .unwrap();
        assert_eq!(
            tracker.classify(ROOT, WINDOW, ROOT, 77, true, false),
            ManagedUnmapDisposition::External
        );

        tracker
            .record(
                WINDOW,
                44,
                ManagedUnmapReason::IconifyRetain { generation: 2 },
            )
            .unwrap();
        assert_eq!(
            classify(&mut tracker, WINDOW, 43),
            ManagedUnmapDisposition::ManagerDuplicate
        );
        assert_eq!(
            classify(&mut tracker, ROOT, 43),
            ManagedUnmapDisposition::ManagerDuplicate
        );
        assert_eq!(
            classify(&mut tracker, WINDOW, 44),
            ManagedUnmapDisposition::ManagerOwned(ManagedUnmapReason::IconifyRetain {
                generation: 2
            })
        );
        assert_eq!(
            classify(&mut tracker, ROOT, 44),
            ManagedUnmapDisposition::ManagerDuplicate
        );
        assert_eq!(tracker.pending_for(WINDOW), 0);
    }

    #[test]
    fn wrapped_sequence_prefers_new_generation_over_withdrawal_tombstone() {
        let mut tracker = ManagedUnmapTracker::default();
        let old_sequence = 43_u64;
        let wrapped_sequence = old_sequence + u64::from(u16::MAX) + 1;
        tracker
            .record(
                WINDOW,
                old_sequence,
                ManagedUnmapReason::IconifyRetain { generation: 1 },
            )
            .unwrap();
        assert_eq!(
            tracker.classify(ROOT, WINDOW, ROOT, 70, true, false),
            ManagedUnmapDisposition::External
        );
        tracker
            .record(
                WINDOW,
                wrapped_sequence,
                ManagedUnmapReason::IconifyRetain { generation: 2 },
            )
            .unwrap();

        assert_eq!(
            classify(&mut tracker, WINDOW, old_sequence as u16),
            ManagedUnmapDisposition::ManagerOwned(ManagedUnmapReason::IconifyRetain {
                generation: 2
            })
        );
        assert_eq!(
            classify(&mut tracker, ROOT, old_sequence as u16),
            ManagedUnmapDisposition::ManagerDuplicate
        );
        assert_eq!(
            classify(&mut tracker, WINDOW, old_sequence as u16),
            ManagedUnmapDisposition::ManagerDuplicate
        );
        assert_eq!(
            classify(&mut tracker, ROOT, old_sequence as u16),
            ManagedUnmapDisposition::ManagerDuplicate
        );
        assert_eq!(tracker.pending_for(WINDOW), 0);
    }

    #[test]
    fn unmap_gravity_never_consumes_a_matching_sequence_or_marker() {
        let mut tracker = ManagedUnmapTracker::default();
        tracker
            .record(WINDOW, 44, ManagedUnmapReason::SwallowDiscard)
            .unwrap();

        assert_eq!(
            tracker.classify(ROOT, WINDOW, WINDOW, 44, false, true),
            ManagedUnmapDisposition::External
        );
        assert_eq!(tracker.pending_for(WINDOW), 1);
        assert_eq!(
            classify(&mut tracker, WINDOW, 44),
            ManagedUnmapDisposition::ManagerOwned(ManagedUnmapReason::SwallowDiscard)
        );
    }

    #[test]
    fn wrong_sequence_is_external_and_retires_the_stale_marker() {
        let mut tracker = ManagedUnmapTracker::default();
        tracker
            .record(WINDOW, 45, ManagedUnmapReason::SwallowDiscard)
            .unwrap();

        assert_eq!(
            classify(&mut tracker, ROOT, 46),
            ManagedUnmapDisposition::External
        );
        assert_eq!(tracker.pending_for(WINDOW), 0);
    }

    #[test]
    fn one_window_cannot_steal_another_windows_marker() {
        let mut tracker = ManagedUnmapTracker::default();
        tracker
            .record(WINDOW, 47, ManagedUnmapReason::SwallowDiscard)
            .unwrap();

        assert_eq!(
            tracker.classify(ROOT, WINDOW + 1, ROOT, 47, false, false),
            ManagedUnmapDisposition::External
        );
        assert_eq!(tracker.pending_for(WINDOW), 1);
    }

    #[test]
    fn rapid_iconify_generations_are_correlated_by_sequence_not_a_boolean() {
        let mut tracker = ManagedUnmapTracker::default();
        tracker
            .record(
                WINDOW,
                48,
                ManagedUnmapReason::IconifyRetain { generation: 1 },
            )
            .unwrap();
        tracker
            .record(
                WINDOW,
                50,
                ManagedUnmapReason::IconifyRetain { generation: 2 },
            )
            .unwrap();

        assert_eq!(
            classify(&mut tracker, WINDOW, 50),
            ManagedUnmapDisposition::ManagerOwned(ManagedUnmapReason::IconifyRetain {
                generation: 2
            })
        );
        assert_eq!(
            classify(&mut tracker, ROOT, 50),
            ManagedUnmapDisposition::ManagerDuplicate
        );
        assert_eq!(
            classify(&mut tracker, ROOT, 48),
            ManagedUnmapDisposition::ManagerOwned(ManagedUnmapReason::IconifyRetain {
                generation: 1
            })
        );
        assert_eq!(
            classify(&mut tracker, WINDOW, 48),
            ManagedUnmapDisposition::ManagerDuplicate
        );
        assert_eq!(tracker.pending_for(WINDOW), 0);
    }

    #[test]
    fn destroy_clear_beats_a_late_matching_notification() {
        for old_reason in REASONS {
            let mut tracker = ManagedUnmapTracker::default();
            tracker.record(WINDOW, 51, old_reason).unwrap();
            tracker.clear(WINDOW);

            assert_eq!(
                classify(&mut tracker, ROOT, 51),
                ManagedUnmapDisposition::External
            );

            // Reusing the same numeric XID starts with a clean lifecycle: a
            // request for the new client carries only its own reason.
            let new_reason = match old_reason {
                ManagedUnmapReason::SwallowDiscard => {
                    ManagedUnmapReason::IconifyRetain { generation: 8 }
                }
                ManagedUnmapReason::IconifyRetain { .. } => ManagedUnmapReason::SwallowDiscard,
            };
            tracker.record(WINDOW, 52, new_reason).unwrap();
            assert_eq!(
                classify(&mut tracker, WINDOW, 52),
                ManagedUnmapDisposition::ManagerOwned(new_reason)
            );
            assert_eq!(
                classify(&mut tracker, ROOT, 52),
                ManagedUnmapDisposition::ManagerDuplicate
            );
        }
    }

    #[test]
    fn low_wire_sequence_matches_a_full_cookie_sequence() {
        let mut tracker = ManagedUnmapTracker::default();
        let full = u64::from(u16::MAX) + 52;
        tracker
            .record(WINDOW, full, ManagedUnmapReason::SwallowDiscard)
            .unwrap();

        assert!(matches!(
            classify(&mut tracker, WINDOW, full as u16),
            ManagedUnmapDisposition::ManagerOwned(_)
        ));
    }

    #[test]
    fn full_queue_rejects_new_request_without_evicting_sent_markers() {
        let mut tracker = ManagedUnmapTracker::default();
        for sequence in 0..MAX_PENDING_PER_WINDOW as u64 {
            let reason = if sequence + 1 == MAX_PENDING_PER_WINDOW as u64 {
                ManagedUnmapReason::IconifyRetain { generation: 8 }
            } else {
                ManagedUnmapReason::SwallowDiscard
            };
            tracker.record(WINDOW, sequence, reason).unwrap();
        }

        assert!(tracker.ensure_capacity(WINDOW).is_err());
        assert!(
            tracker
                .record(
                    WINDOW,
                    MAX_PENDING_PER_WINDOW as u64,
                    ManagedUnmapReason::IconifyRetain { generation: 9 },
                )
                .is_err()
        );
        assert_eq!(tracker.pending_for(WINDOW), MAX_PENDING_PER_WINDOW);

        assert_eq!(
            classify(&mut tracker, WINDOW, 0),
            ManagedUnmapDisposition::ManagerOwned(ManagedUnmapReason::SwallowDiscard)
        );
        assert_eq!(
            classify(&mut tracker, ROOT, 0),
            ManagedUnmapDisposition::ManagerDuplicate
        );
        assert_eq!(tracker.pending_for(WINDOW), MAX_PENDING_PER_WINDOW - 1);

        let newest_sequence = MAX_PENDING_PER_WINDOW as u16 - 1;
        assert_eq!(
            classify(&mut tracker, WINDOW, newest_sequence),
            ManagedUnmapDisposition::ManagerOwned(ManagedUnmapReason::IconifyRetain {
                generation: 8
            })
        );
        assert_eq!(
            classify(&mut tracker, ROOT, newest_sequence),
            ManagedUnmapDisposition::ManagerDuplicate
        );
    }
}
