//! Client-side ordering and presentation gate for explicitly negotiated repair.
//! Neither a visual ACK nor an ordinary Ready can complete an accepted phase.
use super::*;
use crate::remote::pointer_repair_phase::{
    ActionCompletionFence, ActionReceipt, ClientIdle, Scope, Token,
};
use crate::remote::pointer_repair_wire::{RepairStatus, RepairStatusKind};
const DEADLINE: Duration = Duration::from_secs(5);
#[derive(Clone, Copy, PartialEq, Eq)]
enum PhaseKind {
    Requested,
    Accepted,
    Cancelled,
}
#[derive(Clone, Copy)]
struct Phase {
    token: Token,
    kind: PhaseKind,
    deadline: Instant,
    visual: Option<Scope>,
    proof: Option<Scope>,
    confirmed: Option<Scope>,
    recovery_after: u64,
}
#[derive(Default)]
pub(super) struct PointerRepair {
    pub(super) enabled: bool,
    lease: Option<(u64, Instant)>,
    fence: Option<ActionCompletionFence>,
    terminal_receipt: Option<ActionReceipt>,
    action_deadline: Option<Instant>,
    last_input: Option<Instant>,
    held_keys: std::collections::BTreeSet<u8>,
    latest_decoded_sequence: Option<u64>,
    offer: Option<Token>,
    declining: Option<(Token, Instant)>,
    phase: Option<Phase>,
    retired: Vec<Token>,
    last_phase: u64,
}
fn repair_error() -> RemoteError {
    invalid_data("pointer repair ordering, scope or deadline invalid").into()
}
impl ClientPointer {
    fn repair_retire(&mut self, token: Token) -> RemoteResult<()> {
        if self.repair.retired.contains(&token) {
            return Ok(());
        }
        if self.repair.retired.len() >= 128 {
            self.revoke();
            return Err(repair_error());
        }
        self.repair.retired.push(token);
        Ok(())
    }

    pub(super) fn repair_blocked(&self) -> bool {
        self.repair.phase.is_some()
            || self.repair.action_deadline.is_some()
            || self.repair.declining.is_some()
    }
    pub(super) fn repair_end(&mut self) {
        if let Some(fence) = &mut self.repair.fence {
            fence.revoke();
        }
        self.repair.phase = None;
        self.repair.offer = None;
        self.repair.declining = None;
        self.repair.terminal_receipt = None;
    }

    pub(super) fn revoke_local(
        &mut self,
    ) -> Option<crate::remote::wayland_pointer_wire::PointerRevoke> {
        if self.ended {
            return None;
        }
        let pending = self
            .repair
            .fence
            .as_ref()
            .and_then(ActionCompletionFence::pending_receipt);
        let revoke = self.revoke();
        // Only explicit local input/focus cancellation may drain this one
        // already-sent action. TTL, protocol failure, and server terminal do not.
        if !self.terminal_seen {
            self.repair.terminal_receipt = pending;
        }
        revoke
    }
    pub(super) fn repair_granted(&mut self, lease: u64) -> RemoteResult<()> {
        if !self.repair.enabled {
            return Ok(());
        }
        let (_, at) = self.requested.ok_or_else(repair_error)?;
        if self.repair.lease.is_none() {
            self.repair.lease = Some((
                lease,
                at + super::super::wayland_pointer_wire::LEASE_DURATION,
            ));
            self.repair.fence = Some(ActionCompletionFence::new(lease));
            self.repair.last_input = Some(at);
        } else if self.repair.lease.unwrap().0 != lease {
            return Err(repair_error());
        }
        Ok(())
    }
    pub(super) fn repair_check(&mut self, now: Instant) -> RemoteResult<()> {
        if !self.repair.enabled || self.ended {
            return Ok(());
        }
        if self.repair.action_deadline.is_some_and(|at| now >= at)
            || self.repair.declining.is_some_and(|(_, at)| now >= at)
            || self.repair.phase.is_some_and(|phase| now >= phase.deadline)
        {
            self.revoke();
            return Err(repair_error());
        }
        Ok(())
    }
    pub(super) fn repair_input(&mut self, event: &ViewerEvent, now: Instant) {
        if !self.repair.enabled {
            return;
        }
        if let ViewerEvent::Key { keycode, pressed } = event {
            if *pressed {
                self.repair.held_keys.insert(*keycode);
            } else {
                self.repair.held_keys.remove(keycode);
            }
        }
        // Mapping invalidation emits an unchanged physical-button snapshot;
        // periodic proof refresh must not masquerade as human activity.
        let activity = match event {
            ViewerEvent::WaylandPointerButtonCancel { pressed } => *pressed != self.left_down,
            ViewerEvent::WaylandPointer { .. }
            | ViewerEvent::WaylandPointerCancel
            | ViewerEvent::Key { .. }
            | ViewerEvent::Button { .. } => true,
            _ => false,
        };
        if activity {
            self.repair.last_input = Some(now);
        }
    }
    pub(super) fn repair_sent(
        &mut self,
        action: &crate::remote::wayland_pointer_wire::PointerAction,
        now: Instant,
    ) -> RemoteResult<()> {
        if !self.repair.enabled {
            return Ok(());
        }
        self.repair_check(now)?;
        let (_, lease_deadline) = self.repair.lease.ok_or_else(repair_error)?;
        self.repair
            .fence
            .as_mut()
            .ok_or_else(repair_error)?
            .sent(ActionReceipt {
                lease: action.lease_epoch,
                sequence: action.sequence,
                consumed: action.scope.into(),
            })
            .map_err(|_| repair_error())?;
        self.repair.action_deadline = Some((now + DEADLINE).min(lease_deadline));
        Ok(())
    }
    pub(super) fn repair_completed(
        &mut self,
        receipt: ActionReceipt,
        now: Instant,
    ) -> RemoteResult<()> {
        if !self.repair.enabled {
            return Err(repair_error());
        }
        if self.ended {
            if !self.terminal_seen && self.repair.terminal_receipt == Some(receipt) {
                self.repair.terminal_receipt = None;
                return Ok(());
            }
            return Err(repair_error());
        }
        self.repair_check(now)?;
        self.repair
            .fence
            .as_mut()
            .ok_or_else(repair_error)?
            .complete(receipt)
            .map_err(|_| repair_error())?;
        self.repair.action_deadline = None;
        Ok(())
    }
    pub(super) fn repair_offer(&mut self, token: Token, now: Instant) -> RemoteResult<()> {
        self.repair_check(now)?;
        if !self.repair.enabled
            || self.ended
            || self.epoch != Some(token.lease)
            || token.phase <= self.repair.last_phase
            || self.repair.offer.is_some()
            || self.repair.phase.is_some()
        {
            return Err(repair_error());
        }
        let initial = self.requested.ok_or_else(repair_error)?.0;
        if (token.ready.source, token.ready.geometry)
            != (initial.source_epoch, initial.geometry_epoch)
        {
            return Err(repair_error());
        }
        self.repair.last_phase = token.phase;
        self.repair.offer = Some(token);
        Ok(())
    }
    /// Called after draining this pass's local events, before enqueueing Request.
    pub(super) fn repair_outgoing(
        &mut self,
        now: Instant,
    ) -> RemoteResult<Option<(MessageKind, Token)>> {
        self.repair_check(now)?;
        // Request AND Decline share the native action's single ordered slot.
        // Preserve the offer until the exact completion receipt frees it;
        // enqueue success or generic Waiting never permits a second command.
        if self.repair.action_deadline.is_some() || self.repair.declining.is_some() {
            return Ok(None);
        }
        let Some(token) = self.repair.offer.take() else {
            return Ok(None);
        };
        let watermark = self
            .repair
            .fence
            .as_ref()
            .and_then(ActionCompletionFence::repair_watermark);
        let idle = ClientIdle {
            held: self.left_down || !self.repair.held_keys.is_empty(),
            candidate: self.click.is_some(),
            pending_action: watermark.is_none(),
            last_input: self.repair.last_input.unwrap_or(now),
        };
        if !self.ended
            && idle.eligible(now)
            && self.fresh(now)
            && self.ready.map(Scope::from) == Some(token.ready)
            && watermark == Some(token.completed_action)
        {
            let lease_deadline = self.repair.lease.ok_or_else(repair_error)?.1;
            self.ready = None;
            self.click = None;
            self.repair.phase = Some(Phase {
                token,
                kind: PhaseKind::Requested,
                deadline: (now + DEADLINE).min(lease_deadline),
                visual: None,
                proof: None,
                confirmed: None,
                recovery_after: token.ready.sequence,
            });
            Ok(Some((MessageKind::WaylandPointerRepairRequest, token)))
        } else {
            self.repair_retire(token)?;
            let deadline = self.repair.lease.ok_or_else(repair_error)?.1;
            self.repair.declining = Some((token, (now + DEADLINE).min(deadline)));
            Ok(Some((MessageKind::WaylandPointerRepairDecline, token)))
        }
    }
    pub(super) fn repair_frame(&mut self, purpose: FramePurpose, scope: Scope) -> RemoteResult<()> {
        if !self.repair.enabled {
            return Ok(());
        }
        self.repair.latest_decoded_sequence = Some(scope.sequence);
        let Some(phase) = self.repair.phase.as_mut() else {
            return if purpose == FramePurpose::VisualRepair {
                Err(repair_error())
            } else {
                Ok(())
            };
        };
        if (scope.source, scope.geometry) != (phase.token.ready.source, phase.token.ready.geometry)
        {
            return Err(repair_error());
        }
        match phase.kind {
            PhaseKind::Accepted => match purpose {
                FramePurpose::VisualRepair | FramePurpose::Video => {
                    if phase.visual.is_some() || scope.sequence <= phase.token.ready.sequence {
                        return Err(repair_error());
                    }
                    phase.visual = Some(scope);
                }
                FramePurpose::NativeProof => {
                    if phase.proof.is_some()
                        || phase
                            .visual
                            .is_none_or(|visual| scope.sequence <= visual.sequence)
                    {
                        return Err(repair_error());
                    }
                    phase.proof = Some(scope);
                }
            },
            PhaseKind::Cancelled => {
                if purpose == FramePurpose::VisualRepair {
                    return Err(repair_error());
                }
                if purpose == FramePurpose::NativeProof && scope.sequence > phase.recovery_after {
                    phase.proof = Some(scope);
                }
            }
            PhaseKind::Requested if purpose == FramePurpose::VisualRepair => {
                return Err(repair_error());
            }
            _ => {}
        }
        Ok(())
    }
    pub(super) fn repair_status(&mut self, status: RepairStatus, now: Instant) -> RemoteResult<()> {
        if !self.repair.enabled || self.ended {
            return Err(repair_error());
        }
        self.repair_check(now)?;
        if status.kind == RepairStatusKind::Declined
            && self
                .repair
                .declining
                .is_some_and(|(token, _)| token == status.token)
        {
            // This releases ONLY queue occupancy. Never recreate ready/proof,
            // clear a physical hold, or reset the original input lease clock.
            self.repair.declining = None;
            return Ok(());
        }
        if status.kind == RepairStatusKind::Declined {
            return Err(repair_error());
        }
        if status.kind == RepairStatusKind::Cancelled && self.repair.offer == Some(status.token) {
            self.repair.offer = None;
            self.repair_retire(status.token)?;
            return Ok(());
        }
        if status.kind == RepairStatusKind::Cancelled && self.repair.retired.contains(&status.token)
        {
            return Ok(());
        }
        let phase = self.repair.phase.as_mut().ok_or_else(repair_error)?;
        if phase.token != status.token {
            return Err(repair_error());
        }
        match status.kind {
            RepairStatusKind::Accepted if phase.kind == PhaseKind::Requested => {
                phase.kind = PhaseKind::Accepted
            }
            RepairStatusKind::Cancelled if phase.kind == PhaseKind::Requested => {
                // Advisory expiry may have been sent before Request was read.
                // It is not completion of our occupied host command slot.
            }
            RepairStatusKind::RequestCancelled if phase.kind == PhaseKind::Requested => {
                phase.kind = PhaseKind::Cancelled;
                // Only post-receipt proof may recover a rejected request.
                // Old refreshes that crossed the advisory cancellation cannot.
                phase.visual = None;
                phase.proof = None;
                phase.confirmed = None;
                phase.recovery_after = self
                    .repair
                    .latest_decoded_sequence
                    .unwrap_or(phase.token.ready.sequence)
                    .max(phase.token.ready.sequence);
            }
            RepairStatusKind::Ready if phase.kind == PhaseKind::Accepted => {
                if status.proof.is_none()
                    || status.proof != phase.proof
                    || phase.confirmed.is_some()
                {
                    return Err(repair_error());
                }
                phase.confirmed = status.proof;
            }
            _ => return Err(repair_error()),
        }
        self.repair_resolve(now);
        Ok(())
    }
    /// Ordinary statuses cannot release Accepted. Cancelled needs a new native
    /// proof and its own exact actual-presentation, not the retired Ready scope.
    pub(super) fn repair_ordinary_ready(
        &mut self,
        scope: Scope,
        now: Instant,
    ) -> RemoteResult<bool> {
        let Some(phase) = self.repair.phase.as_mut() else {
            return Ok(false);
        };
        if phase.kind == PhaseKind::Cancelled && scope.sequence > phase.recovery_after {
            if phase.proof != Some(scope) {
                return Err(repair_error());
            }
            phase.confirmed = Some(scope);
            self.repair_resolve(now);
        }
        Ok(true)
    }
    pub(super) fn repair_resolve(&mut self, now: Instant) {
        let Some(phase) = self.repair.phase else {
            return;
        };
        let Some(proof) = phase.confirmed else {
            return;
        };
        if !self.ended
            && !self.newer_frame
            && phase.proof == Some(proof)
            && self.presented.is_some_and(|(scope, at)| {
                Scope::from(scope) == proof
                    && now.saturating_duration_since(at)
                        < crate::remote::wayland_pointer_wire::FRAME_MAX_AGE
            })
            && now < phase.deadline
        {
            if self.repair_retire(phase.token).is_err() {
                return;
            }
            self.repair.phase = None;
            self.ready = Some(proof.into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::wayland_pointer_wire::{
        PointerAction, PointerActionKind, PointerStatus, PointerStatusKind,
    };
    fn fixture(now: Instant) -> (ClientPointer, Token) {
        let scope = DrawnScope {
            source_epoch: 1,
            geometry_epoch: 2,
            frame_sequence: 5,
        };
        let mut pointer = ClientPointer {
            enabled: true,
            requested: Some((scope, now - Duration::from_secs(3))),
            presented: Some((scope, now)),
            ..Default::default()
        };
        pointer.repair.enabled = true;
        pointer
            .receive(PointerStatus {
                lease_epoch: 9,
                scope,
                status: PointerStatusKind::Ready,
            })
            .unwrap();
        let token = Token {
            lease: 9,
            phase: 1,
            ready: scope.into(),
            completed_action: 0,
        };
        (pointer, token)
    }
    fn requested(now: Instant) -> (ClientPointer, Token) {
        let (mut p, t) = fixture(now);
        p.repair_offer(t, now).unwrap();
        assert_eq!(
            p.repair_outgoing(now).unwrap(),
            Some((MessageKind::WaylandPointerRepairRequest, t))
        );
        assert!(!p.fresh(now));
        (p, t)
    }
    fn accepted(now: Instant) -> (ClientPointer, Token) {
        let (mut p, t) = requested(now);
        p.repair_status(
            RepairStatus {
                kind: RepairStatusKind::Accepted,
                token: t,
                proof: None,
            },
            now,
        )
        .unwrap();
        (p, t)
    }
    fn scope(sequence: u64) -> Scope {
        Scope {
            source: 1,
            geometry: 2,
            sequence,
        }
    }
    fn motion(sequence: u64) -> ViewerEvent {
        ViewerEvent::WaylandPointer {
            frame_sequence: sequence,
            geometry_epoch: 2,
            x: 4,
            y: 5,
            local_x: 4,
            local_y: 5,
            kind: crate::remote::x11_viewer::WaylandPointerEventKind::Motion,
        }
    }
    #[test]
    fn decline_occupies_queue_until_dequeued_receipt_not_advisory_cancelled() {
        let now = Instant::now();
        for invalidate in 0..3 {
            let (mut p, t) = fixture(now);
            p.repair.last_input = Some(now); // Fresh proof, but human input is not idle.
            p.repair_offer(t, now).unwrap();
            assert_eq!(
                p.repair_outgoing(now).unwrap(),
                Some((MessageKind::WaylandPointerRepairDecline, t))
            );
            assert!(p.event(&motion(5), now).unwrap().is_none());
            assert_eq!(p.sequence, 0);
            assert!(p.repair_outgoing(now).unwrap().is_none());
            match invalidate {
                1 => p.before_frame(),
                2 => p
                    .receive(PointerStatus {
                        lease_epoch: 9,
                        scope: t.ready.into(),
                        status: PointerStatusKind::Waiting,
                    })
                    .unwrap(),
                _ => {}
            }
            p.repair_status(
                RepairStatus {
                    kind: RepairStatusKind::Cancelled,
                    token: t,
                    proof: None,
                },
                now,
            )
            .unwrap();
            assert!(
                p.repair.declining.is_some(),
                "advisory cancellation may predate Decline dequeue"
            );
            assert!(p.event(&motion(5), now).unwrap().is_none());
            p.repair_status(
                RepairStatus {
                    kind: RepairStatusKind::Declined,
                    token: t,
                    proof: None,
                },
                now,
            )
            .unwrap();
            assert!(p.repair.declining.is_none());
            assert_eq!(p.event(&motion(5), now).unwrap().is_some(), invalidate == 0);
        }
    }
    #[test]
    fn decline_fence_tracks_holds_and_has_nonrenewable_bounded_deadline() {
        let now = Instant::now();
        let (mut p, t) = fixture(now);
        p.repair.last_input = Some(now);
        p.repair_offer(t, now).unwrap();
        p.repair_outgoing(now).unwrap();
        let deadline = p.repair.declining.unwrap().1;
        p.event(
            &ViewerEvent::WaylandPointerButtonCancel { pressed: true },
            now,
        )
        .unwrap();
        assert!(p.left_down);
        assert!(p.click.is_none());
        let mut wrong = t;
        wrong.completed_action += 1;
        assert!(
            p.repair_status(
                RepairStatus {
                    kind: RepairStatusKind::Declined,
                    token: wrong,
                    proof: None
                },
                now
            )
            .is_err()
        );
        assert_eq!(p.repair.declining.unwrap().1, deadline);
        p.repair_status(
            RepairStatus {
                kind: RepairStatusKind::Declined,
                token: t,
                proof: None,
            },
            now,
        )
        .unwrap();
        assert!(p.left_down);
        assert!(p.click.is_none());
        let (mut p, t) = fixture(now);
        p.repair.last_input = Some(now);
        p.repair_offer(t, now).unwrap();
        p.repair_outgoing(now).unwrap();
        assert!(p.repair_check(p.repair.declining.unwrap().1).is_err());
        assert!(p.ended);
    }
    #[test]
    fn first_visual_repair_cannot_bypass_phase_validation() {
        let mut p = ClientPointer::default();
        p.repair.enabled = true;
        assert!(
            p.repair_frame(FramePurpose::VisualRepair, scope(0))
                .is_err()
        );
        assert!(p.repair_frame(FramePurpose::NativeProof, scope(0)).is_ok());
    }
    #[test]
    fn accepted_phase_requires_exact_new_native_proof_and_actual_presentation() {
        let now = Instant::now();
        let (mut p, t) = accepted(now);
        p.receive(PointerStatus {
            lease_epoch: 9,
            scope: t.ready.into(),
            status: PointerStatusKind::Ready,
        })
        .unwrap();
        assert!(!p.fresh(now));
        p.repair_frame(FramePurpose::VisualRepair, scope(6))
            .unwrap();
        // A repair ACK is video credit only; no native-present callback.
        assert!(!p.fresh(now));
        p.repair_frame(FramePurpose::NativeProof, scope(7)).unwrap();
        p.before_frame();
        p.repair_status(
            RepairStatus {
                kind: RepairStatusKind::Ready,
                token: t,
                proof: Some(scope(7)),
            },
            now,
        )
        .unwrap();
        assert!(
            !p.fresh(now),
            "host confirmation cannot invent actual presentation"
        );
        assert!(p.presented(scope(7).into(), now).is_none());
        assert!(p.fresh(now));
        assert_eq!(p.ready, Some(scope(7).into()));
        assert_eq!(p.repair.lease.unwrap().1, now + Duration::from_secs(27));
        assert!(
            p.repair_status(
                RepairStatus {
                    kind: RepairStatusKind::Ready,
                    token: t,
                    proof: Some(scope(7))
                },
                now
            )
            .is_err()
        );
    }
    #[test]
    fn wrong_token_old_scope_duplicate_visual_and_accepted_cancellation_fail_closed() {
        let now = Instant::now();
        for case in 0..5 {
            let (mut p, t) = accepted(now);
            let result = match case {
                0 => p.repair_status(
                    RepairStatus {
                        kind: RepairStatusKind::Cancelled,
                        token: t,
                        proof: None,
                    },
                    now,
                ),
                1 => p.repair_frame(FramePurpose::NativeProof, scope(7)),
                2 => {
                    p.repair_frame(FramePurpose::VisualRepair, scope(6))
                        .unwrap();
                    p.repair_frame(FramePurpose::VisualRepair, scope(7))
                }
                3 => {
                    let mut wrong = t;
                    wrong.completed_action = 1;
                    p.repair_status(
                        RepairStatus {
                            kind: RepairStatusKind::Ready,
                            token: wrong,
                            proof: Some(scope(7)),
                        },
                        now,
                    )
                }
                _ => {
                    p.repair_frame(FramePurpose::VisualRepair, scope(6))
                        .unwrap();
                    p.repair_frame(FramePurpose::NativeProof, scope(7)).unwrap();
                    p.repair_status(
                        RepairStatus {
                            kind: RepairStatusKind::Ready,
                            token: t,
                            proof: Some(scope(8)),
                        },
                        now,
                    )
                }
            };
            assert!(result.is_err());
            assert!(!p.fresh(now));
        }
    }
    #[test]
    fn requested_cancellation_waits_for_new_proof_and_never_reuses_retired_ready() {
        let now = Instant::now();
        let (mut p, t) = requested(now);
        p.repair_status(
            RepairStatus {
                kind: RepairStatusKind::Cancelled,
                token: t,
                proof: None,
            },
            now,
        )
        .unwrap();
        // An old refresh can cross the advisory cancellation before the host
        // has consumed our Request. It cannot complete the command or phase.
        p.repair_frame(FramePurpose::NativeProof, scope(6)).unwrap();
        p.before_frame();
        p.presented(scope(6).into(), now);
        p.receive(PointerStatus {
            lease_epoch: 9,
            scope: scope(6).into(),
            status: PointerStatusKind::Ready,
        })
        .unwrap();
        assert!(!p.fresh(now));
        p.repair_status(
            RepairStatus {
                kind: RepairStatusKind::RequestCancelled,
                token: t,
                proof: None,
            },
            now,
        )
        .unwrap();
        p.receive(PointerStatus {
            lease_epoch: 9,
            scope: scope(6).into(),
            status: PointerStatusKind::Ready,
        })
        .unwrap();
        assert!(
            !p.fresh(now),
            "pre-dequeue refresh cannot release Request fence"
        );
        p.receive(PointerStatus {
            lease_epoch: 9,
            scope: t.ready.into(),
            status: PointerStatusKind::Ready,
        })
        .unwrap();
        assert!(!p.fresh(now));
        p.repair_frame(FramePurpose::NativeProof, scope(7)).unwrap();
        p.before_frame();
        p.receive(PointerStatus {
            lease_epoch: 9,
            scope: scope(7).into(),
            status: PointerStatusKind::Ready,
        })
        .unwrap();
        assert!(!p.fresh(now));
        p.presented(scope(7).into(), now);
        assert!(p.fresh(now));
    }
    #[test]
    fn every_action_needs_exact_completion_receipt_not_waiting_or_queued_send() {
        let now = Instant::now();
        let (mut p, t) = fixture(now);
        let action = PointerAction {
            lease_epoch: 9,
            sequence: 1,
            scope: t.ready.into(),
            kind: PointerActionKind::Motion,
            x: 4,
            y: 5,
        };
        p.repair_sent(&action, now).unwrap();
        p.receive(PointerStatus {
            lease_epoch: 9,
            scope: t.ready.into(),
            status: PointerStatusKind::Waiting,
        })
        .unwrap();
        assert!(
            p.repair
                .fence
                .as_ref()
                .unwrap()
                .repair_watermark()
                .is_none()
        );
        p.repair_offer(t, now).unwrap();
        assert_eq!(p.repair_outgoing(now).unwrap(), None);
        assert_eq!(
            p.repair.offer,
            Some(t),
            "offer waits behind native action completion"
        );
        let receipt = ActionReceipt {
            lease: 9,
            sequence: 1,
            consumed: t.ready,
        };
        p.repair_completed(receipt, now).unwrap();
        assert_eq!(
            p.repair_outgoing(now).unwrap(),
            Some((MessageKind::WaylandPointerRepairDecline, t)),
            "only after the actual completion may the stale offer be declined"
        );
        assert_eq!(p.repair.fence.as_ref().unwrap().repair_watermark(), Some(1));
        assert!(p.repair_completed(receipt, now).is_err());
    }
    #[test]
    fn local_revoke_drains_only_exact_pending_receipt_once_without_authority() {
        let now = Instant::now();
        for case in 0..5 {
            let (mut p, t) = fixture(now);
            let action = PointerAction {
                lease_epoch: 9,
                sequence: 1,
                scope: t.ready.into(),
                kind: PointerActionKind::ClickLeft,
                x: 4,
                y: 5,
            };
            p.repair_sent(&action, now).unwrap();
            let deadline = p.repair.lease.unwrap().1;
            let receipt = ActionReceipt {
                lease: 9,
                sequence: 1,
                consumed: t.ready,
            };
            assert!(p.revoke_local().is_some());
            assert!(p.ended);
            assert!(!p.terminal_seen);
            let mut incoming = receipt;
            match case {
                1 => incoming.sequence += 1,
                2 => incoming.consumed.sequence += 1,
                3 => incoming.lease += 1,
                4 => p
                    .receive(PointerStatus {
                        lease_epoch: 9,
                        scope: t.ready.into(),
                        status: PointerStatusKind::Revoked,
                    })
                    .unwrap(),
                _ => {}
            }
            assert_eq!(p.repair_completed(incoming, now).is_ok(), case == 0);
            assert!(p.ended);
            assert!(!p.fresh(now));
            assert!(p.ready.is_none() && p.click.is_none());
            assert_eq!(p.repair.lease.unwrap().1, deadline);
            assert_eq!(p.repair.fence.as_ref().unwrap().repair_watermark(), None);
            if case == 0 {
                assert!(
                    p.repair_completed(receipt, now).is_err(),
                    "consumed tombstone cannot replay"
                );
                p.receive(PointerStatus {
                    lease_epoch: 9,
                    scope: t.ready.into(),
                    status: PointerStatusKind::Revoked,
                })
                .unwrap();
                assert!(p.repair_completed(receipt, now).is_err());
            }
        }
        let (mut p, t) = fixture(now);
        p.revoke_local();
        assert!(
            p.repair_completed(
                ActionReceipt {
                    lease: 9,
                    sequence: 1,
                    consumed: t.ready
                },
                now
            )
            .is_err(),
            "no pending action means no terminal receipt allowance"
        );
        let (mut p, t) = fixture(now);
        let action = PointerAction {
            lease_epoch: 9,
            sequence: 1,
            scope: t.ready.into(),
            kind: PointerActionKind::ClickLeft,
            x: 4,
            y: 5,
        };
        p.repair_sent(&action, now).unwrap();
        p.revoke(); // Ordinary expiry/protocol cancellation is not local drain.
        assert!(p.repair.terminal_receipt.is_none());
        assert!(
            p.repair_completed(
                ActionReceipt {
                    lease: 9,
                    sequence: 1,
                    consumed: t.ready
                },
                now
            )
            .is_err()
        );
    }
    #[test]
    fn action_and_phase_deadlines_are_not_refreshed_by_status_or_lease_regrant() {
        let now = Instant::now();
        let (mut p, t) = fixture(now);
        let action = PointerAction {
            lease_epoch: 9,
            sequence: 1,
            scope: t.ready.into(),
            kind: PointerActionKind::Motion,
            x: 0,
            y: 0,
        };
        p.repair_sent(&action, now).unwrap();
        let deadline = p.repair.action_deadline.unwrap();
        p.repair_granted(9).unwrap();
        assert_eq!(p.repair.action_deadline, Some(deadline));
        assert!(p.repair_check(deadline).is_err());
        assert!(p.ended);
        let (mut p, t) = requested(now);
        let deadline = p.repair.phase.unwrap().deadline;
        p.repair_status(
            RepairStatus {
                kind: RepairStatusKind::Accepted,
                token: t,
                proof: None,
            },
            now + Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(p.repair.phase.unwrap().deadline, deadline);
        assert!(p.repair_check(deadline).is_err());
    }
    #[test]
    fn held_candidate_and_recent_input_decline_without_requesting_or_synthesizing_release() {
        let now = Instant::now();
        for case in 0..4 {
            let (mut p, t) = fixture(now);
            match case {
                0 => p.left_down = true,
                1 => p.click = Some((t.ready.into(), 1, 1, now)),
                2 => p.repair.last_input = Some(now - Duration::from_millis(1999)),
                _ => {
                    p.repair.held_keys.insert(42);
                }
            }
            p.repair_offer(t, now).unwrap();
            assert_eq!(
                p.repair_outgoing(now).unwrap(),
                Some((MessageKind::WaylandPointerRepairDecline, t))
            );
            assert!(p.repair.phase.is_none());
        }
        let (mut p, t) = accepted(now);
        p.event(
            &ViewerEvent::WaylandPointerButtonCancel { pressed: true },
            now,
        )
        .unwrap();
        assert!(p.left_down);
        assert!(p.click.is_none());
        p.repair_frame(FramePurpose::VisualRepair, scope(6))
            .unwrap();
        p.repair_frame(FramePurpose::NativeProof, scope(7)).unwrap();
        p.before_frame();
        p.presented(scope(7).into(), now);
        p.repair_status(
            RepairStatus {
                kind: RepairStatusKind::Ready,
                token: t,
                proof: Some(scope(7)),
            },
            now,
        )
        .unwrap();
        assert!(p.left_down);
        assert!(p.click.is_none());
        let event = ViewerEvent::WaylandPointer {
            frame_sequence: 7,
            geometry_epoch: 2,
            x: 2,
            y: 3,
            local_x: 2,
            local_y: 3,
            kind: crate::remote::x11_viewer::WaylandPointerEventKind::LeftUp,
        };
        assert!(p.event(&event, now).unwrap().is_none());
        assert!(!p.left_down);
    }
    #[test]
    fn retired_full_token_cancellation_cannot_cancel_a_new_accepted_phase() {
        let now = Instant::now();
        let (mut p, old) = fixture(now);
        p.repair_retire(old).unwrap();
        let mut current = old;
        current.phase = 2;
        p.repair_offer(current, now).unwrap();
        p.repair_outgoing(now).unwrap();
        p.repair_status(
            RepairStatus {
                kind: RepairStatusKind::Accepted,
                token: current,
                proof: None,
            },
            now,
        )
        .unwrap();
        p.repair_status(
            RepairStatus {
                kind: RepairStatusKind::Cancelled,
                token: old,
                proof: None,
            },
            now,
        )
        .unwrap();
        assert_eq!(p.repair.phase.unwrap().token, current);
        assert!(!p.fresh(now));
        let mut forged = old;
        forged.completed_action = 1;
        assert!(
            p.repair_status(
                RepairStatus {
                    kind: RepairStatusKind::Cancelled,
                    token: forged,
                    proof: None
                },
                now
            )
            .is_err()
        );
    }
    #[test]
    fn automatic_mapping_cancel_does_not_reset_human_idle_clock() {
        let now = Instant::now();
        let (mut p, _) = fixture(now);
        let before = p.repair.last_input;
        p.event(
            &ViewerEvent::WaylandPointerButtonCancel { pressed: false },
            now,
        )
        .unwrap();
        assert_eq!(p.repair.last_input, before);
        p.event(
            &ViewerEvent::WaylandPointerButtonCancel { pressed: true },
            now,
        )
        .unwrap();
        assert_eq!(p.repair.last_input, Some(now));
    }
}
