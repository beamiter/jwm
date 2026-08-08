//! Backend-owned coordination for true X11 ICCCM Iconic transitions.
//!
//! Capturing pixels and issuing X requests are deliberately outside this
//! module.  It only records which managed window is waiting for a durable
//! compositor admission, which checked `UnmapWindow` is in flight, and which
//! generation the server has acknowledged.  Keeping the state machine pure
//! makes rapid restore/re-minimize races testable without an X server.

use crate::backend::common_define::WindowId;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IconifyPhase {
    AwaitingAdmission,
    UnmapSent { generation: u64 },
    Iconic { generation: u64 },
}

impl IconifyPhase {
    pub(crate) const fn generation(self) -> Option<u64> {
        match self {
            Self::AwaitingAdmission => None,
            Self::UnmapSent { generation } | Self::Iconic { generation } => Some(generation),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CancelPlan {
    Nothing,
    RemovedAwaiting,
    MapBeforeRemoving { generation: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AcknowledgeDisposition {
    Confirmed,
    Duplicate,
    Ignored,
}

/// Result of the ordered attributes query following a checked `MapWindow`.
///
/// A negative reply and a failed query are deliberately distinct. The former
/// proves that another `UnmapWindow` would be a no-op (and therefore produce
/// no `UnmapNotify` to consume its managed-unmap marker), while the latter
/// leaves open the possibility that the checked map made the client viewable
/// and must be rolled back conservatively.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ViewabilityVerification<E> {
    ConfirmedViewable,
    ConfirmedNotViewable(E),
    QueryError(E),
}

#[derive(Debug, Default)]
pub(crate) struct IconifyCoordinator {
    phases: HashMap<WindowId, IconifyPhase>,
}

impl IconifyCoordinator {
    /// Register an iconify request. Returns true while admission should be
    /// attempted; requests already sent or acknowledged are idempotent.
    pub(crate) fn request(&mut self, window: WindowId) -> bool {
        match self.phases.entry(window) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(IconifyPhase::AwaitingAdmission);
                true
            }
            std::collections::hash_map::Entry::Occupied(entry) => {
                *entry.get() == IconifyPhase::AwaitingAdmission
            }
        }
    }

    pub(crate) fn phase(&self, window: WindowId) -> Option<IconifyPhase> {
        self.phases.get(&window).copied()
    }

    pub(crate) fn awaiting_windows(&self) -> Vec<WindowId> {
        let mut windows = self
            .phases
            .iter()
            .filter_map(|(&window, &phase)| {
                (phase == IconifyPhase::AwaitingAdmission).then_some(window)
            })
            .collect::<Vec<_>>();
        windows.sort_unstable_by_key(|window| window.raw());
        windows
    }

    /// Commit the result of a checked `UnmapWindow` request.
    pub(crate) fn mark_unmap_sent(&mut self, window: WindowId, generation: u64) -> bool {
        if generation == 0 || self.phases.get(&window) != Some(&IconifyPhase::AwaitingAdmission) {
            return false;
        }
        self.phases
            .insert(window, IconifyPhase::UnmapSent { generation });
        true
    }

    /// Consume the first normalized notification for a checked unmap. The
    /// generation comparison is what prevents an old acknowledgement from
    /// confirming a later restore/re-minimize incarnation of the same XID.
    pub(crate) fn acknowledge(
        &mut self,
        window: WindowId,
        generation: u64,
    ) -> AcknowledgeDisposition {
        match self.phases.get(&window).copied() {
            Some(IconifyPhase::UnmapSent {
                generation: expected,
            }) if generation == expected => {
                self.phases
                    .insert(window, IconifyPhase::Iconic { generation });
                AcknowledgeDisposition::Confirmed
            }
            Some(IconifyPhase::Iconic {
                generation: expected,
            }) if generation == expected => AcknowledgeDisposition::Duplicate,
            _ => AcknowledgeDisposition::Ignored,
        }
    }

    /// Begin a normal restore/cancel. Awaiting admission owns no physical
    /// unmap and can disappear immediately. Sent/Iconic windows must first be
    /// checked-mapped and confirmed viewable; their pinned snapshot
    /// deliberately remains owned by the compositor until live import
    /// succeeds.
    pub(crate) fn begin_cancel(&mut self, window: WindowId) -> CancelPlan {
        match self.phases.get(&window).copied() {
            None => CancelPlan::Nothing,
            Some(IconifyPhase::AwaitingAdmission) => {
                self.phases.remove(&window);
                CancelPlan::RemovedAwaiting
            }
            Some(phase) => CancelPlan::MapBeforeRemoving {
                generation: phase
                    .generation()
                    .expect("sent/iconic phases always carry a generation"),
            },
        }
    }

    /// Complete cancellation only after checked `MapWindow` succeeded and an
    /// ordered attributes reply reported `IsViewable`.
    pub(crate) fn finish_mapped_cancel(&mut self, window: WindowId, generation: u64) -> bool {
        let matches = self
            .phases
            .get(&window)
            .and_then(|phase| phase.generation())
            == Some(generation);
        if matches {
            self.phases.remove(&window);
        }
        matches
    }

    /// Windows that must be remapped before an active compositor, and its
    /// pinned snapshot cache, may be destroyed.
    pub(crate) fn remap_before_compositor_loss(&self) -> Vec<(WindowId, u64)> {
        let mut windows = self
            .phases
            .iter()
            .filter_map(|(&window, &phase)| {
                phase.generation().map(|generation| (window, generation))
            })
            .collect::<Vec<_>>();
        windows.sort_unstable_by_key(|(window, _)| window.raw());
        windows
    }

    /// After checked remap and reservation release, desired-state replay owns
    /// a fresh capture/admission attempt when the compositor returns.
    pub(crate) fn mark_awaiting_after_compositor_loss(
        &mut self,
        window: WindowId,
        generation: u64,
    ) -> bool {
        if self
            .phases
            .get(&window)
            .and_then(|phase| phase.generation())
            != Some(generation)
        {
            return false;
        }
        self.phases.insert(window, IconifyPhase::AwaitingAdmission);
        true
    }

    pub(crate) fn retire(&mut self, window: WindowId) -> Option<IconifyPhase> {
        self.phases.remove(&window)
    }
}

/// Commit one already-attempted checked unmap. A failed X request keeps the
/// coordinator awaiting and gives the caller exactly one opportunity to
/// release the reservation acquired for that attempt.
pub(crate) fn finish_checked_unmap<E>(
    coordinator: &mut IconifyCoordinator,
    window: WindowId,
    generation: u64,
    unmap_result: Result<(), E>,
    release_failed_reservation: impl FnOnce(),
) -> Result<(), E> {
    if let Err(error) = unmap_result {
        release_failed_reservation();
        return Err(error);
    }

    let marked = coordinator.mark_unmap_sent(window, generation);
    debug_assert!(
        marked,
        "checked IconifyRetain unmap lost its coordinator request"
    );
    Ok(())
}

/// Complete a normal deiconify transaction. Once the checked `MapWindow`
/// request succeeds, an ordered query error is no longer a plain map failure:
/// the request may already have made the client viewable, so put that same
/// generation back through the managed-unmap path. An explicit not-viewable
/// reply proves that rollback would be a notification-free no-op and skips it.
/// Both failures retain the coordinator phase and snapshot pin.
pub(crate) fn finish_checked_cancel<E>(
    coordinator: &mut IconifyCoordinator,
    window: WindowId,
    generation: u64,
    map_result: Result<(), E>,
    verify_viewable: impl FnOnce() -> ViewabilityVerification<E>,
    rollback_unmap: impl FnOnce(WindowId, u64),
) -> Result<(), E> {
    map_result?;
    match verify_viewable() {
        ViewabilityVerification::ConfirmedViewable => {}
        ViewabilityVerification::ConfirmedNotViewable(error) => return Err(error),
        ViewabilityVerification::QueryError(error) => {
            rollback_unmap(window, generation);
            return Err(error);
        }
    }

    let removed = coordinator.finish_mapped_cancel(window, generation);
    debug_assert!(removed, "checked Iconic restore lost its generation");
    Ok(())
}

/// Remap every physically Iconic client before compositor destruction. No
/// reservation or coordinator phase is changed until every checked map and
/// its ordered viewability query have succeeded. If a map fails, earlier
/// confirmed-viewable maps are re-unmapped. If the query after a successful
/// map errors, that current map is included too. An explicit not-viewable
/// reply excludes the current no-op map, whose rollback could never produce
/// the notification needed to consume a managed-unmap marker. Rollback is
/// always reverse ordered, and the caller retains both the compositor and all
/// snapshot pins.
pub(crate) fn prepare_compositor_loss_transaction<E>(
    coordinator: &mut IconifyCoordinator,
    resolved: &[(WindowId, u32, u64)],
    mut map_window: impl FnMut(WindowId) -> Result<(), E>,
    mut verify_viewable: impl FnMut(WindowId) -> ViewabilityVerification<E>,
    mut rollback_unmap: impl FnMut(WindowId, u64),
    mut release_reservation: impl FnMut(u32, u64),
) -> Result<(), E> {
    let mut confirmed_viewable = Vec::with_capacity(resolved.len());
    for &(window, x11_window, generation) in resolved {
        if let Err(error) = map_window(window) {
            for &(mapped_window, _, mapped_generation) in confirmed_viewable.iter().rev() {
                rollback_unmap(mapped_window, mapped_generation);
            }
            return Err(error);
        }
        match verify_viewable(window) {
            ViewabilityVerification::ConfirmedViewable => {
                confirmed_viewable.push((window, x11_window, generation));
            }
            ViewabilityVerification::ConfirmedNotViewable(error) => {
                for &(mapped_window, _, mapped_generation) in confirmed_viewable.iter().rev() {
                    rollback_unmap(mapped_window, mapped_generation);
                }
                return Err(error);
            }
            ViewabilityVerification::QueryError(error) => {
                rollback_unmap(window, generation);
                for &(mapped_window, _, mapped_generation) in confirmed_viewable.iter().rev() {
                    rollback_unmap(mapped_window, mapped_generation);
                }
                return Err(error);
            }
        }
    }

    for &(_, x11_window, generation) in &confirmed_viewable {
        release_reservation(x11_window, generation);
    }
    for &(window, _, generation) in &confirmed_viewable {
        let transitioned = coordinator.mark_awaiting_after_compositor_loss(window, generation);
        debug_assert!(transitioned);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeSet;

    fn window(raw: u64) -> WindowId {
        WindowId::from_raw(raw)
    }

    #[test]
    fn request_is_idempotent_in_every_phase() {
        let win = window(7);
        let mut coordinator = IconifyCoordinator::default();
        assert!(coordinator.request(win));
        assert!(coordinator.request(win));
        assert!(coordinator.mark_unmap_sent(win, 11));
        assert!(!coordinator.request(win));
        assert_eq!(
            coordinator.acknowledge(win, 11),
            AcknowledgeDisposition::Confirmed
        );
        assert!(!coordinator.request(win));
    }

    #[test]
    fn old_generation_cannot_confirm_a_new_incarnation() {
        let win = window(7);
        let mut coordinator = IconifyCoordinator::default();
        coordinator.request(win);
        assert!(coordinator.mark_unmap_sent(win, 1));
        assert_eq!(
            coordinator.begin_cancel(win),
            CancelPlan::MapBeforeRemoving { generation: 1 }
        );
        assert!(coordinator.finish_mapped_cancel(win, 1));

        coordinator.request(win);
        assert!(coordinator.mark_unmap_sent(win, 2));
        assert_eq!(
            coordinator.acknowledge(win, 1),
            AcknowledgeDisposition::Ignored
        );
        assert_eq!(
            coordinator.phase(win),
            Some(IconifyPhase::UnmapSent { generation: 2 })
        );
        assert_eq!(
            coordinator.acknowledge(win, 2),
            AcknowledgeDisposition::Confirmed
        );
    }

    #[test]
    fn late_ack_after_checked_cancel_does_not_revive_the_phase() {
        let win = window(7);
        let mut coordinator = IconifyCoordinator::default();
        coordinator.request(win);
        coordinator.mark_unmap_sent(win, 5);
        assert_eq!(
            coordinator.begin_cancel(win),
            CancelPlan::MapBeforeRemoving { generation: 5 }
        );
        assert!(coordinator.finish_mapped_cancel(win, 5));

        assert_eq!(
            coordinator.acknowledge(win, 5),
            AcknowledgeDisposition::Ignored
        );
        assert_eq!(coordinator.phase(win), None);
    }

    #[test]
    fn failed_async_unmap_releases_pin_and_remains_retryable() {
        let win = window(7);
        let mut coordinator = IconifyCoordinator::default();
        coordinator.request(win);
        let pinned = RefCell::new(BTreeSet::from([(77_u32, 10_u64)]));

        let first = finish_checked_unmap(
            &mut coordinator,
            win,
            10,
            Err("checked unmap failed"),
            || {
                pinned.borrow_mut().remove(&(77, 10));
            },
        );
        assert_eq!(first, Err("checked unmap failed"));
        assert_eq!(
            coordinator.phase(win),
            Some(IconifyPhase::AwaitingAdmission)
        );
        assert!(pinned.borrow().is_empty());

        pinned.borrow_mut().insert((77, 11));
        finish_checked_unmap(&mut coordinator, win, 11, Ok::<(), &str>(()), || {
            pinned.borrow_mut().remove(&(77, 11));
        })
        .unwrap();
        assert_eq!(
            coordinator.phase(win),
            Some(IconifyPhase::UnmapSent { generation: 11 })
        );
        assert_eq!(&*pinned.borrow(), &BTreeSet::from([(77, 11)]));
    }

    #[test]
    fn cancel_awaiting_needs_no_map_but_sent_and_iconic_do() {
        let awaiting = window(1);
        let sent = window(2);
        let iconic = window(3);
        let mut coordinator = IconifyCoordinator::default();
        coordinator.request(awaiting);
        coordinator.request(sent);
        coordinator.request(iconic);
        coordinator.mark_unmap_sent(sent, 20);
        coordinator.mark_unmap_sent(iconic, 30);
        coordinator.acknowledge(iconic, 30);

        assert_eq!(
            coordinator.begin_cancel(awaiting),
            CancelPlan::RemovedAwaiting
        );
        assert_eq!(
            coordinator.begin_cancel(sent),
            CancelPlan::MapBeforeRemoving { generation: 20 }
        );
        assert_eq!(
            coordinator.begin_cancel(iconic),
            CancelPlan::MapBeforeRemoving { generation: 30 }
        );
        assert!(!coordinator.finish_mapped_cancel(sent, 21));
        assert!(coordinator.finish_mapped_cancel(sent, 20));
        assert!(coordinator.finish_mapped_cancel(iconic, 30));
        assert_eq!(coordinator.phase(awaiting), None);
    }

    #[test]
    fn cancel_keeps_physical_phase_until_map_is_confirmed_viewable() {
        let win = window(4);
        let mut coordinator = IconifyCoordinator::default();
        coordinator.request(win);
        coordinator.mark_unmap_sent(win, 40);
        coordinator.acknowledge(win, 40);

        assert_eq!(
            coordinator.begin_cancel(win),
            CancelPlan::MapBeforeRemoving { generation: 40 }
        );
        // A checked MapWindow followed by a false/error attributes reply must
        // not call finish_mapped_cancel.
        assert_eq!(
            coordinator.phase(win),
            Some(IconifyPhase::Iconic { generation: 40 })
        );
        assert!(coordinator.finish_mapped_cancel(win, 40));
    }

    #[test]
    fn cancel_attribute_query_error_rolls_back_current_generation_and_keeps_pin() {
        let win = window(4);
        let mut coordinator = IconifyCoordinator::default();
        coordinator.request(win);
        coordinator.mark_unmap_sent(win, 40);
        coordinator.acknowledge(win, 40);
        let rolled_back = RefCell::new(Vec::new());
        let pins = BTreeSet::from([(104_u32, 40_u64)]);

        let result = finish_checked_cancel(
            &mut coordinator,
            win,
            40,
            Ok::<(), &str>(()),
            || ViewabilityVerification::QueryError("attributes query failed"),
            |window, generation| rolled_back.borrow_mut().push((window, generation)),
        );

        assert_eq!(result, Err("attributes query failed"));
        assert_eq!(&*rolled_back.borrow(), &[(win, 40)]);
        assert_eq!(
            coordinator.phase(win),
            Some(IconifyPhase::Iconic { generation: 40 })
        );
        assert_eq!(pins, BTreeSet::from([(104_u32, 40_u64)]));
    }

    #[test]
    fn cancel_confirmed_not_viewable_skips_noop_rollback_without_consuming_capacity() {
        use crate::backend::api::ManagedUnmapReason;
        use crate::backend::x11::wm::managed_unmap::ManagedUnmapTracker;

        let win = window(4);
        let x11_window = 104_u32;
        let generation = 40_u64;
        let mut coordinator = IconifyCoordinator::default();
        coordinator.request(win);
        coordinator.mark_unmap_sent(win, generation);
        coordinator.acknowledge(win, generation);
        let tracker = RefCell::new(ManagedUnmapTracker::default());
        let pins = BTreeSet::from([(x11_window, generation)]);

        // More retries than the per-XID tracker capacity must remain safe:
        // each ordered false reply proves that rollback UnmapWindow would be
        // a no-op with no UnmapNotify available to retire its marker.
        for sequence in 0..16_u64 {
            let result = finish_checked_cancel(
                &mut coordinator,
                win,
                generation,
                Ok::<(), &str>(()),
                || ViewabilityVerification::ConfirmedNotViewable("window was not viewable"),
                |_, rollback_generation| {
                    tracker
                        .borrow_mut()
                        .record(
                            x11_window,
                            sequence,
                            ManagedUnmapReason::IconifyRetain {
                                generation: rollback_generation,
                            },
                        )
                        .unwrap();
                },
            );
            assert_eq!(result, Err("window was not viewable"));
        }

        // All eight slots are still available after the false-query retries.
        for sequence in 100..108_u64 {
            tracker
                .borrow_mut()
                .record(
                    x11_window,
                    sequence,
                    ManagedUnmapReason::IconifyRetain { generation },
                )
                .unwrap();
        }
        assert!(tracker.borrow().ensure_capacity(x11_window).is_err());
        assert_eq!(
            coordinator.phase(win),
            Some(IconifyPhase::Iconic { generation })
        );
        assert_eq!(pins, BTreeSet::from([(x11_window, generation)]));
    }

    #[test]
    fn compositor_loss_remaps_only_physical_iconic_phases() {
        let awaiting = window(3);
        let sent = window(2);
        let iconic = window(1);
        let mut coordinator = IconifyCoordinator::default();
        coordinator.request(awaiting);
        coordinator.request(sent);
        coordinator.request(iconic);
        coordinator.mark_unmap_sent(sent, 20);
        coordinator.mark_unmap_sent(iconic, 10);
        coordinator.acknowledge(iconic, 10);

        assert_eq!(
            coordinator.remap_before_compositor_loss(),
            vec![(iconic, 10), (sent, 20)]
        );
        assert!(coordinator.mark_awaiting_after_compositor_loss(iconic, 10));
        assert!(coordinator.mark_awaiting_after_compositor_loss(sent, 20));
        assert_eq!(coordinator.awaiting_windows(), vec![iconic, sent, awaiting]);
    }

    #[test]
    fn compositor_loss_confirmed_not_viewable_rolls_back_only_previous_without_commit() {
        let first = window(1);
        let failing = window(2);
        let mut coordinator = IconifyCoordinator::default();
        coordinator.request(first);
        coordinator.request(failing);
        coordinator.mark_unmap_sent(first, 10);
        coordinator.acknowledge(first, 10);
        coordinator.mark_unmap_sent(failing, 20);
        let resolved = vec![(first, 101, 10), (failing, 102, 20)];
        let operations = RefCell::new(Vec::new());
        let rolled_back = RefCell::new(Vec::new());
        let pins = RefCell::new(BTreeSet::from([(101_u32, 10_u64), (102, 20)]));

        let result = prepare_compositor_loss_transaction(
            &mut coordinator,
            &resolved,
            |window| {
                operations.borrow_mut().push(("map", window));
                Ok::<(), &str>(())
            },
            |window| {
                if window == failing {
                    operations.borrow_mut().push(("attributes-false", window));
                    ViewabilityVerification::ConfirmedNotViewable("window was not viewable")
                } else {
                    operations.borrow_mut().push(("attributes-true", window));
                    ViewabilityVerification::ConfirmedViewable
                }
            },
            |window, generation| rolled_back.borrow_mut().push((window, generation)),
            |x11_window, generation| {
                pins.borrow_mut().remove(&(x11_window, generation));
            },
        );

        assert_eq!(result, Err("window was not viewable"));
        assert_eq!(
            &*operations.borrow(),
            &[
                ("map", first),
                ("attributes-true", first),
                ("map", failing),
                ("attributes-false", failing),
            ]
        );
        assert_eq!(&*rolled_back.borrow(), &[(first, 10)]);
        assert_eq!(
            coordinator.phase(first),
            Some(IconifyPhase::Iconic { generation: 10 })
        );
        assert_eq!(
            coordinator.phase(failing),
            Some(IconifyPhase::UnmapSent { generation: 20 })
        );
        assert_eq!(
            &*pins.borrow(),
            &BTreeSet::from([(101_u32, 10_u64), (102, 20)])
        );
    }

    #[test]
    fn compositor_loss_attribute_query_error_rolls_back_the_current_successful_map() {
        let win = window(3);
        let mut coordinator = IconifyCoordinator::default();
        coordinator.request(win);
        coordinator.mark_unmap_sent(win, 30);
        coordinator.acknowledge(win, 30);
        let resolved = vec![(win, 103, 30)];
        let operations = RefCell::new(Vec::new());
        let pins = BTreeSet::from([(103_u32, 30_u64)]);

        let result = prepare_compositor_loss_transaction(
            &mut coordinator,
            &resolved,
            |window| {
                operations.borrow_mut().push(("map", window));
                Ok::<(), &str>(())
            },
            |window| {
                operations.borrow_mut().push(("attributes-error", window));
                ViewabilityVerification::QueryError("attributes query failed")
            },
            |window, _| operations.borrow_mut().push(("rollback", window)),
            |_, _| panic!("query failure must not release a snapshot pin"),
        );

        assert_eq!(result, Err("attributes query failed"));
        assert_eq!(
            &*operations.borrow(),
            &[("map", win), ("attributes-error", win), ("rollback", win)]
        );
        assert_eq!(
            coordinator.phase(win),
            Some(IconifyPhase::Iconic { generation: 30 })
        );
        assert_eq!(pins, BTreeSet::from([(103_u32, 30_u64)]));
    }

    #[test]
    fn compositor_loss_releases_only_after_all_maps_then_requeues_admission() {
        let first = window(1);
        let second = window(2);
        let mut coordinator = IconifyCoordinator::default();
        coordinator.request(first);
        coordinator.request(second);
        coordinator.mark_unmap_sent(first, 10);
        coordinator.mark_unmap_sent(second, 20);
        let resolved = vec![(first, 101, 10), (second, 102, 20)];
        let operations = RefCell::new(Vec::new());
        let pins = RefCell::new(BTreeSet::from([(101_u32, 10_u64), (102, 20)]));

        prepare_compositor_loss_transaction(
            &mut coordinator,
            &resolved,
            |window| {
                operations.borrow_mut().push(("map", window.raw()));
                Ok::<(), &str>(())
            },
            |window| {
                operations.borrow_mut().push(("attributes", window.raw()));
                ViewabilityVerification::ConfirmedViewable
            },
            |window, _| operations.borrow_mut().push(("rollback", window.raw())),
            |x11_window, generation| {
                operations
                    .borrow_mut()
                    .push(("release", u64::from(x11_window)));
                pins.borrow_mut().remove(&(x11_window, generation));
            },
        )
        .unwrap();

        assert_eq!(
            &*operations.borrow(),
            &[
                ("map", 1),
                ("attributes", 1),
                ("map", 2),
                ("attributes", 2),
                ("release", 101),
                ("release", 102),
            ]
        );
        assert!(pins.borrow().is_empty());
        assert_eq!(
            coordinator.phase(first),
            Some(IconifyPhase::AwaitingAdmission)
        );
        assert_eq!(
            coordinator.phase(second),
            Some(IconifyPhase::AwaitingAdmission)
        );
    }

    #[test]
    fn destroy_or_external_withdraw_retires_any_phase() {
        for phase in [
            IconifyPhase::AwaitingAdmission,
            IconifyPhase::UnmapSent { generation: 4 },
            IconifyPhase::Iconic { generation: 4 },
        ] {
            let win = window(9);
            let mut coordinator = IconifyCoordinator::default();
            coordinator.phases.insert(win, phase);
            assert_eq!(coordinator.retire(win), Some(phase));
            assert_eq!(coordinator.phase(win), None);
        }
    }

    #[test]
    fn zero_generation_is_never_sent_or_used_to_finish_cancel() {
        let win = window(9);
        let mut coordinator = IconifyCoordinator::default();
        coordinator.request(win);
        assert!(!coordinator.mark_unmap_sent(win, 0));
        assert!(!coordinator.finish_mapped_cancel(win, 0));
        assert_eq!(
            coordinator.phase(win),
            Some(IconifyPhase::AwaitingAdmission)
        );
    }
}
