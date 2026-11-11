//! Unwired pointer-only repair protocol core. No CLI capability is enabled.
//!
//! This is sequencing, NOT a native authority validator. Integration must retain
//! every native focus/instance/local-input/lock/held check and the original lease
//! deadline. All request/action messages share the existing ordered command
//! queue. A sent action sequence is NOT a completed action sequence.
//!
//! Offer is advisory: host stable debt and input-idle>=2s; the client additionally
//! needs no held button/key, click candidate or unacknowledged action. It closes
//! its input gate BEFORE writing Request. Host accepts only the exact current
//! Ready scope and completed-action watermark. Accepted means Waiting, retiring
//! the original native frame before any repair can be submitted. One bounded
//! visual record (PNG Repair, or JPEG Video on new damage) then requires a NEW
//! post-flush native capture and exact presentation ACK. No cached repair ACK,
//! same-scope rewrap, timeout or phase replay can restore input.
//!
//! Existing proof expiry refreshes real captures independently of human input:
//! an idle client can therefore be idle for 2s with a current proof younger than
//! 1s. This module does not implement that refresh or an automatic trigger.
use std::time::{Duration, Instant};
const IDLE: Duration = Duration::from_secs(2);
const PROOF_AGE: Duration = Duration::from_secs(1);
const PHASE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Scope {
    pub source: u64,
    pub geometry: u64,
    pub sequence: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Token {
    pub lease: u64,
    pub phase: u64,
    pub ready: Scope,
    pub completed_action: u64,
}
/// Proposed explicit completion receipt. Existing PointerStatus::Waiting is
/// intentionally insufficient: proof expiry emits it without executing action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ActionReceipt {
    pub lease: u64,
    pub sequence: u64,
    pub consumed: Scope,
}

/// Local ordering fence, not an input authorization. A future integration must
/// mark `sent` before publishing the action on the ordered writer, and emit the
/// matching receipt on the host ONLY after native Accepted and proof cleanup.
/// Offers, generic Waiting, frame ACKs and enqueue success never call complete.
/// This fence starts with the lease and covers ALL actions, not just repair
/// attempts. Missing receipts must use the existing action deadline truncated
/// by the original lease deadline; this core supplies no renewal or timeout reset.
/// Send/transport failure must revoke rather than roll back and retry a sequence.
pub(crate) struct ActionCompletionFence {
    lease: u64,
    completed: u64,
    pending: Option<ActionReceipt>,
    ended: bool,
}
impl ActionCompletionFence {
    pub(crate) fn new(lease: u64) -> Self {
        Self {
            lease,
            completed: 0,
            pending: None,
            ended: false,
        }
    }
    pub(crate) fn sent(&mut self, action: ActionReceipt) -> Result<(), Error> {
        if self.ended {
            return Err(Error::Ended);
        }
        if self.pending.is_some()
            || action.lease != self.lease
            || self.completed.checked_add(1) != Some(action.sequence)
        {
            self.revoke();
            return Err(Error::Replay);
        }
        self.pending = Some(action);
        Ok(())
    }
    pub(crate) fn complete(&mut self, receipt: ActionReceipt) -> Result<(), Error> {
        if self.ended {
            return Err(Error::Ended);
        }
        if self.pending != Some(receipt) {
            self.revoke();
            return Err(Error::Replay);
        }
        self.completed = receipt.sequence;
        self.pending = None;
        Ok(())
    }
    /// A usable ordering watermark, not proof that scope/focus remains valid.
    pub(crate) fn repair_watermark(&self) -> Option<u64> {
        (!self.ended && self.pending.is_none()).then_some(self.completed)
    }
    pub(crate) fn revoke(&mut self) {
        self.ended = true;
        self.pending = None;
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ClientIdle {
    pub held: bool,
    pub candidate: bool,
    pub pending_action: bool,
    pub last_input: Instant,
}
impl ClientIdle {
    pub(crate) fn eligible(self, now: Instant) -> bool {
        !self.held
            && !self.candidate
            && !self.pending_action
            && now.saturating_duration_since(self.last_input) >= IDLE
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VisualKind {
    Repair,
    DynamicVideo,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    Ended,
    Deadline,
    Replay,
    OfferCancelled,
    Scope,
    Busy,
    StaleProof,
    CaptureOrder,
    NonceExhausted,
}
#[derive(Clone, Copy, Debug)]
enum Stage {
    Ready,
    Offered {
        token: Token,
        deadline: Instant,
    },
    Quiesced {
        token: Token,
        deadline: Instant,
    },
    VisualFlushed {
        token: Token,
        deadline: Instant,
        scope: Scope,
        flushed: Instant,
    },
    AwaitProofAck {
        token: Token,
        deadline: Instant,
        scope: Scope,
        captured: Instant,
    },
    Ended,
}
pub(crate) struct RepairPhase {
    lease: u64,
    lease_deadline: Instant,
    next_phase: u64,
    retired: Vec<Token>,
    last_repair: Option<Instant>,
    stage: Stage,
}
impl RepairPhase {
    pub(crate) fn new(lease: u64, lease_deadline: Instant) -> Self {
        Self {
            lease,
            lease_deadline,
            next_phase: 0,
            retired: Vec::new(),
            last_repair: None,
            stage: Stage::Ready,
        }
    }
    fn reject<T>(&mut self, error: Error) -> Result<T, Error> {
        self.stage = Stage::Ended;
        Err(error)
    }
    pub(crate) fn check(&mut self, now: Instant) -> Result<(), Error> {
        if matches!(self.stage, Stage::Ended) {
            return Err(Error::Ended);
        }
        if now >= self.lease_deadline {
            return self.reject(Error::Deadline);
        }
        if let Stage::Offered { deadline, .. } = self.stage {
            if now >= deadline {
                self.cancel_offer();
            }
            return Ok(());
        }
        let deadline = match self.stage {
            Stage::Quiesced { deadline, .. }
            | Stage::VisualFlushed { deadline, .. }
            | Stage::AwaitProofAck { deadline, .. } => deadline,
            _ => self.lease_deadline,
        };
        if now >= deadline {
            return self.reject(Error::Deadline);
        }
        Ok(())
    }
    /// An ignored/declined offer, local input, or automatic proof refresh may
    /// cancel only an advisory offer. Ready here is a scheduling state, NOT a
    /// restoration of any native ticket. Integration must still use its actual
    /// current proof and keep a requesting client gated until fresh Ready.
    pub(crate) fn cancel_offer(&mut self) -> Option<Token> {
        let Stage::Offered { token, .. } = self.stage else {
            return None;
        };
        self.retired.push(token);
        self.stage = Stage::Ready;
        Some(token)
    }
    /// Remote Decline must name the exact advisory token. Never route a
    /// network message to the tokenless local cancel_offer helper. Retired
    /// declines are idempotent and cannot cancel a newer offer; an accepted
    /// phase cannot be returned to Ready by any decline.
    pub(crate) fn decline(&mut self, request: Token, now: Instant) -> Result<(), Error> {
        self.check(now)?;
        if self.retired.contains(&request) {
            return Ok(());
        }
        match self.stage {
            Stage::Offered { token, .. } if token == request => {
                self.cancel_offer();
                Ok(())
            }
            _ => self.reject(Error::Replay),
        }
    }
    pub(crate) fn offer(
        &mut self,
        ready: Scope,
        completed_action: u64,
        proof_at: Instant,
        last_input: Instant,
        stable_debt: bool,
        now: Instant,
    ) -> Result<Option<Token>, Error> {
        self.check(now)?;
        if !matches!(self.stage, Stage::Ready) {
            return Ok(None);
        }
        if !stable_debt
            || proof_at > now
            || now.saturating_duration_since(last_input) < IDLE
            || self
                .last_repair
                .is_some_and(|at| now.saturating_duration_since(at) < IDLE)
            || now.saturating_duration_since(proof_at) >= PROOF_AGE
        {
            return Ok(None);
        }
        // Never evict token identity: a late message must match the complete
        // retired token, not merely a nonce high-water mark.
        if self.retired.len() >= 128 {
            return self.reject(Error::NonceExhausted);
        }
        let Some(nonce) = self.next_phase.checked_add(1) else {
            return self.reject(Error::NonceExhausted);
        };
        self.next_phase = nonce;
        let token = Token {
            lease: self.lease,
            phase: nonce,
            ready,
            completed_action,
        };
        self.stage = Stage::Offered {
            token,
            deadline: (proof_at + PROOF_AGE).min(self.lease_deadline),
        };
        Ok(Some(token))
    }
    /// Caller has dequeued Request AFTER all earlier actions have completed.
    /// `clean` comes from native/local state, never a remote claim.
    pub(crate) fn request(
        &mut self,
        request: Token,
        current: Scope,
        completed_action: u64,
        clean: bool,
        proof_at: Instant,
        now: Instant,
    ) -> Result<(), Error> {
        self.check(now)?;
        // Retired advisory requests can arrive behind a proof refresh. They
        // may not execute repair or cancel a newer offer. No ticket is restored.
        if matches!(self.stage, Stage::Ready | Stage::Offered { .. })
            && self.retired.contains(&request)
        {
            return Err(Error::OfferCancelled);
        }
        let Stage::Offered { token, .. } = self.stage else {
            return self.reject(Error::Replay);
        };
        if request != token {
            return self.reject(Error::Replay);
        }
        if current != token.ready || completed_action != token.completed_action {
            self.cancel_offer();
            return Err(Error::Scope);
        }
        if !clean {
            self.cancel_offer();
            return Err(Error::Busy);
        }
        if proof_at > now || now.saturating_duration_since(proof_at) >= PROOF_AGE {
            self.cancel_offer();
            return Err(Error::StaleProof);
        }
        self.stage = Stage::Quiesced {
            token,
            deadline: (now + PHASE_TIMEOUT).min(self.lease_deadline),
        };
        Ok(())
    }
    /// Exactly one successfully flushed visual record; neither kind is proof.
    pub(crate) fn visual_flushed(
        &mut self,
        request: Token,
        scope: Scope,
        _kind: VisualKind,
        now: Instant,
    ) -> Result<(), Error> {
        self.check(now)?;
        let Stage::Quiesced { token, deadline } = self.stage else {
            return self.reject(Error::Replay);
        };
        if request != token {
            return self.reject(Error::Replay);
        }
        if scope.source != token.ready.source
            || scope.geometry != token.ready.geometry
            || scope.sequence <= token.ready.sequence
        {
            return self.reject(Error::Scope);
        }
        self.stage = Stage::VisualFlushed {
            token,
            deadline,
            scope,
            flushed: now,
        };
        Ok(())
    }
    /// Integration calls only for actual native capture provenance, never JPEG
    /// codec identity or a cached RGB buffer. Invoke only after the NativeProof
    /// record successfully flushes and enters the authoritative credit ledger.
    /// Timestamp alone is not provenance or evidence of successful publication.
    pub(crate) fn native_proof(
        &mut self,
        request: Token,
        scope: Scope,
        captured: Instant,
        now: Instant,
    ) -> Result<(), Error> {
        self.check(now)?;
        let Stage::VisualFlushed {
            token,
            deadline,
            scope: visual,
            flushed,
        } = self.stage
        else {
            return self.reject(Error::Replay);
        };
        if request != token {
            return self.reject(Error::Replay);
        }
        if captured <= flushed || captured > now {
            return self.reject(Error::CaptureOrder);
        }
        if scope.source != visual.source
            || scope.geometry != visual.geometry
            || scope.sequence <= visual.sequence
        {
            return self.reject(Error::Scope);
        }
        self.stage = Stage::AwaitProofAck {
            token,
            deadline,
            scope,
            captured,
        };
        Ok(())
    }
    pub(crate) fn native_proof_ack(
        &mut self,
        request: Token,
        scope: Scope,
        now: Instant,
    ) -> Result<(), Error> {
        self.check(now)?;
        let Stage::AwaitProofAck {
            token,
            scope: proof,
            captured,
            ..
        } = self.stage
        else {
            return self.reject(Error::Replay);
        };
        if request != token || scope != proof {
            return self.reject(Error::Scope);
        }
        if now.saturating_duration_since(captured) >= PROOF_AGE {
            return self.reject(Error::StaleProof);
        }
        self.last_repair = Some(now);
        self.retired.push(token);
        self.stage = Stage::Ready;
        Ok(())
    }
    pub(crate) fn offered_token(&self) -> Option<Token> {
        match self.stage {
            Stage::Offered { token, .. } => Some(token),
            _ => None,
        }
    }
    pub(crate) fn revoke(&mut self) {
        self.stage = Stage::Ended;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup() -> (RepairPhase, Token, Instant) {
        let t = Instant::now();
        let mut p = RepairPhase::new(7, t + Duration::from_secs(30));
        let token = p
            .offer(
                Scope {
                    source: 1,
                    geometry: 3,
                    sequence: 8,
                },
                4,
                t + Duration::from_secs(2),
                t,
                true,
                t + Duration::from_millis(2200),
            )
            .unwrap()
            .unwrap();
        (p, token, t)
    }
    fn accepted() -> (RepairPhase, Token, Instant) {
        let (mut p, k, t) = setup();
        p.request(
            k,
            k.ready,
            4,
            true,
            t + Duration::from_secs(2),
            t + Duration::from_millis(2300),
        )
        .unwrap();
        (p, k, t)
    }
    #[test]
    fn no_held_candidate_or_unconfirmed_action_can_request() {
        let t = Instant::now();
        for (held, candidate, pending_action) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            assert!(
                !ClientIdle {
                    held,
                    candidate,
                    pending_action,
                    last_input: t
                }
                .eligible(t + IDLE)
            );
        }
        assert!(
            ClientIdle {
                held: false,
                candidate: false,
                pending_action: false,
                last_input: t
            }
            .eligible(t + IDLE)
        );
    }
    #[test]
    fn independent_human_idle_and_fresh_refresh_make_offer_reachable() {
        let (_, _, _) = setup();
    }
    #[test]
    fn earlier_inflight_click_changes_scope_and_cannot_be_overtaken() {
        let (mut p, k, t) = setup();
        let current = Scope {
            sequence: 9,
            ..k.ready
        };
        assert_eq!(
            p.request(
                k,
                current,
                5,
                true,
                t + Duration::from_secs(2),
                t + Duration::from_millis(2300)
            ),
            Err(Error::Scope)
        );
    }
    #[test]
    fn busy_native_never_quiesces_by_synthetic_release() {
        let (mut p, k, t) = setup();
        assert_eq!(
            p.request(
                k,
                k.ready,
                4,
                false,
                t + Duration::from_secs(2),
                t + Duration::from_millis(2300)
            ),
            Err(Error::Busy)
        );
    }
    #[test]
    fn both_visual_paths_require_post_flush_proof_and_original_lease() {
        for kind in [VisualKind::Repair, VisualKind::DynamicVideo] {
            let (mut p, k, t) = accepted();
            let deadline = p.lease_deadline;
            let visual = Scope {
                sequence: 9,
                ..k.ready
            };
            let proof = Scope {
                sequence: 10,
                ..k.ready
            };
            p.visual_flushed(k, visual, kind, t + Duration::from_millis(2400))
                .unwrap();
            p.native_proof(
                k,
                proof,
                t + Duration::from_millis(2500),
                t + Duration::from_millis(2600),
            )
            .unwrap();
            p.native_proof_ack(k, proof, t + Duration::from_millis(2700))
                .unwrap();
            assert_eq!(p.lease_deadline, deadline);
            assert!(matches!(p.stage, Stage::Ready));
            assert!(
                p.offer(
                    proof,
                    4,
                    t + Duration::from_millis(2500),
                    t,
                    true,
                    t + Duration::from_millis(2800)
                )
                .unwrap()
                .is_none()
            );
            assert_eq!(p.check(deadline), Err(Error::Deadline));
        }
    }
    #[test]
    fn png_ack_cannot_restore_input() {
        let (mut p, k, t) = accepted();
        let visual = Scope {
            sequence: 9,
            ..k.ready
        };
        p.visual_flushed(
            k,
            visual,
            VisualKind::Repair,
            t + Duration::from_millis(2400),
        )
        .unwrap();
        assert_eq!(
            p.native_proof_ack(k, visual, t + Duration::from_millis(2500)),
            Err(Error::Replay)
        );
    }
    #[test]
    fn cached_pre_repair_capture_and_same_sequence_are_not_new_proof() {
        for old_time in [true, false] {
            let (mut p, k, t) = accepted();
            let visual = Scope {
                sequence: 9,
                ..k.ready
            };
            p.visual_flushed(
                k,
                visual,
                VisualKind::Repair,
                t + Duration::from_millis(2400),
            )
            .unwrap();
            let scope = if old_time {
                Scope {
                    sequence: 10,
                    ..visual
                }
            } else {
                visual
            };
            let captured = t + Duration::from_millis(if old_time { 2300 } else { 2500 });
            assert!(
                p.native_proof(k, scope, captured, t + Duration::from_millis(2600))
                    .is_err()
            );
        }
    }
    #[test]
    fn replay_expiry_and_local_revoke_are_terminal() {
        let (mut p, k, t) = accepted();
        assert_eq!(
            p.request(
                k,
                k.ready,
                4,
                true,
                t + Duration::from_secs(2),
                t + Duration::from_millis(2400)
            ),
            Err(Error::Replay)
        );
        let (mut p, k, t) = setup();
        assert_eq!(
            p.request(
                k,
                k.ready,
                4,
                true,
                t + Duration::from_secs(2),
                t + Duration::from_secs(3)
            ),
            Err(Error::OfferCancelled)
        );
        assert!(matches!(p.stage, Stage::Ready));
        let (mut p, _, t) = accepted();
        assert_eq!(p.check(t + Duration::from_secs(8)), Err(Error::Deadline));
        let (mut p, _, t) = accepted();
        p.revoke();
        assert_eq!(p.check(t + Duration::from_secs(3)), Err(Error::Ended));
    }
    #[test]
    fn nonce_exhaustion_does_not_wrap() {
        let (mut p, k, t) = setup();
        p.stage = Stage::Ready;
        p.next_phase = u64::MAX;
        assert_eq!(
            p.offer(
                k.ready,
                4,
                t + Duration::from_secs(2),
                t,
                true,
                t + Duration::from_millis(2300)
            ),
            Err(Error::NonceExhausted)
        );
    }
    #[test]
    fn ignored_offer_and_automatic_proof_refresh_do_not_end_lease() {
        let (mut p, k, t) = setup();
        let deadline = p.lease_deadline;
        p.check(t + Duration::from_secs(3)).unwrap();
        assert!(matches!(p.stage, Stage::Ready));
        assert_eq!(p.lease_deadline, deadline);
        let new_scope = Scope {
            sequence: 9,
            ..k.ready
        };
        let next = p
            .offer(
                new_scope,
                4,
                t + Duration::from_secs(3),
                t,
                true,
                t + Duration::from_millis(3100),
            )
            .unwrap()
            .unwrap();
        assert!(next.phase > k.phase);
        assert_eq!(
            p.request(
                k,
                k.ready,
                4,
                true,
                t + Duration::from_secs(3),
                t + Duration::from_millis(3150)
            ),
            Err(Error::OfferCancelled)
        );
        assert!(matches!(p.stage,Stage::Offered{token,..} if token==next));
        p.request(
            next,
            new_scope,
            4,
            true,
            t + Duration::from_secs(3),
            t + Duration::from_millis(3200),
        )
        .unwrap();
        assert!(matches!(p.stage, Stage::Quiesced { .. }));
    }
    #[test]
    fn declining_for_input_or_held_state_does_not_restore_a_ticket() {
        let (mut p, k, t) = setup();
        assert_eq!(p.cancel_offer(), Some(k));
        assert_eq!(p.cancel_offer(), None);
        assert_eq!(
            p.request(
                k,
                k.ready,
                4,
                true,
                t + Duration::from_secs(2),
                t + Duration::from_millis(2300)
            ),
            Err(Error::OfferCancelled)
        );
        assert!(matches!(p.stage, Stage::Ready));
        let (mut p, k, t) = setup();
        assert_eq!(
            p.request(
                k,
                k.ready,
                4,
                false,
                t + Duration::from_secs(2),
                t + Duration::from_millis(2300)
            ),
            Err(Error::Busy)
        );
        assert!(matches!(p.stage, Stage::Ready));
        assert_eq!(
            p.request(
                k,
                k.ready,
                4,
                true,
                t + Duration::from_secs(2),
                t + Duration::from_millis(2400)
            ),
            Err(Error::OfferCancelled)
        );
    }
    #[test]
    fn action_completion_not_enqueue_or_waiting_unblocks_repair_request() {
        let (_, token, _) = setup();
        let mut fence = ActionCompletionFence::new(token.lease);
        assert_eq!(fence.repair_watermark(), Some(0));
        let action = ActionReceipt {
            lease: token.lease,
            sequence: 1,
            consumed: token.ready,
        };
        fence.sent(action).unwrap();
        // Offer/Waiting/ACK have no completion operation. Merely seeing them
        // leaves the action pending; the native queue may still be running.
        assert_eq!(fence.repair_watermark(), None);
        assert!(
            !ClientIdle {
                held: false,
                candidate: false,
                pending_action: fence.pending.is_some(),
                last_input: Instant::now() - IDLE
            }
            .eligible(Instant::now())
        );
        fence.complete(action).unwrap();
        assert_eq!(fence.repair_watermark(), Some(1));
        assert_eq!(fence.complete(action), Err(Error::Replay));
        assert_eq!(fence.repair_watermark(), None);
    }
    #[test]
    fn completion_requires_exact_lease_sequence_and_consumed_scope() {
        let (_, token, _) = setup();
        let action = ActionReceipt {
            lease: token.lease,
            sequence: 1,
            consumed: token.ready,
        };
        for mismatch in 0..5 {
            let mut fence = ActionCompletionFence::new(token.lease);
            fence.sent(action).unwrap();
            let mut wrong = action;
            match mismatch {
                0 => wrong.lease += 1,
                1 => wrong.sequence += 1,
                2 => wrong.consumed.source += 1,
                3 => wrong.consumed.geometry += 1,
                _ => wrong.consumed.sequence += 1,
            }
            assert_eq!(fence.complete(wrong), Err(Error::Replay));
            assert_eq!(fence.repair_watermark(), None);
            assert_eq!(fence.complete(action), Err(Error::Ended));
        }
    }
    #[test]
    fn action_single_slot_and_revoke_never_restore_prior_ticket() {
        let (_, token, _) = setup();
        let action = ActionReceipt {
            lease: token.lease,
            sequence: 1,
            consumed: token.ready,
        };
        let mut fence = ActionCompletionFence::new(token.lease);
        fence.sent(action).unwrap();
        assert_eq!(
            fence.sent(ActionReceipt {
                sequence: 2,
                ..action
            }),
            Err(Error::Replay)
        );
        assert_eq!(fence.repair_watermark(), None);
        let mut fence = ActionCompletionFence::new(token.lease);
        fence.sent(action).unwrap();
        fence.revoke();
        assert_eq!(fence.complete(action), Err(Error::Ended));
        assert_eq!(fence.repair_watermark(), None);
        let (mut phase, offer, t) = setup();
        phase.cancel_offer();
        assert_eq!(
            phase.request(
                offer,
                offer.ready,
                offer.completed_action,
                true,
                t + Duration::from_secs(2),
                t + Duration::from_millis(2300)
            ),
            Err(Error::OfferCancelled)
        );
        assert_eq!(
            fence.repair_watermark(),
            None,
            "decline is not completion or input restoration"
        );
    }
    #[test]
    fn tokenized_old_decline_cannot_cancel_new_offer() {
        let (mut phase, old, t) = setup();
        phase.decline(old, t + Duration::from_millis(2300)).unwrap();
        let scope = Scope {
            sequence: 9,
            ..old.ready
        };
        let next = phase
            .offer(
                scope,
                4,
                t + Duration::from_millis(2350),
                t,
                true,
                t + Duration::from_millis(2400),
            )
            .unwrap()
            .unwrap();
        phase.decline(old, t + Duration::from_millis(2450)).unwrap();
        assert!(matches!(phase.stage, Stage::Offered { token, .. } if token == next));
        phase
            .request(
                next,
                scope,
                4,
                true,
                t + Duration::from_millis(2350),
                t + Duration::from_millis(2500),
            )
            .unwrap();
        phase.decline(old, t + Duration::from_millis(2550)).unwrap();
        assert!(matches!(phase.stage, Stage::Quiesced { token, .. } if token == next));
        assert_eq!(
            phase.decline(next, t + Duration::from_millis(2600)),
            Err(Error::Replay)
        );
        assert!(matches!(phase.stage, Stage::Ended));
    }
    #[test]
    fn decline_same_nonce_wrong_scope_or_lease_is_not_local_cancel() {
        for wrong_lease in [true, false] {
            let (mut phase, token, t) = setup();
            let mut forged = token;
            if wrong_lease {
                forged.lease += 1;
            } else {
                forged.ready.sequence += 1;
            }
            assert_eq!(
                phase.decline(forged, t + Duration::from_millis(2300)),
                Err(Error::Replay)
            );
            assert!(matches!(phase.stage, Stage::Ended));
        }
    }
    #[test]
    fn retired_request_preserves_new_offer_and_rejects_forged_old_token() {
        for forge in [false, true] {
            let (mut phase, old, t) = setup();
            phase.cancel_offer();
            let next = phase
                .offer(
                    Scope {
                        sequence: 9,
                        ..old.ready
                    },
                    4,
                    t + Duration::from_millis(2350),
                    t,
                    true,
                    t + Duration::from_millis(2400),
                )
                .unwrap()
                .unwrap();
            let mut request = old;
            if forge {
                request.completed_action += 1;
            }
            let result = phase.request(
                request,
                next.ready,
                4,
                true,
                t + Duration::from_millis(2350),
                t + Duration::from_millis(2450),
            );
            if forge {
                assert_eq!(result, Err(Error::Replay));
                assert!(matches!(phase.stage, Stage::Ended));
            } else {
                assert_eq!(result, Err(Error::OfferCancelled));
                assert_eq!(phase.offered_token(), Some(next));
            }
        }
    }
    #[test]
    fn retired_decline_requires_complete_identity_and_history_is_bounded() {
        let (mut phase, old, t) = setup();
        phase.cancel_offer();
        let mut forged = old;
        forged.ready.sequence += 1;
        assert_eq!(
            phase.decline(forged, t + Duration::from_millis(2400)),
            Err(Error::Replay)
        );
        let (mut phase, old, t) = setup();
        phase.cancel_offer();
        for n in 1..128 {
            let token = phase
                .offer(
                    old.ready,
                    n,
                    t + Duration::from_secs(2),
                    t,
                    true,
                    t + Duration::from_millis(2400),
                )
                .unwrap()
                .unwrap();
            assert_eq!(phase.cancel_offer(), Some(token));
        }
        assert_eq!(phase.retired.len(), 128);
        assert_eq!(
            phase.offer(
                old.ready,
                129,
                t + Duration::from_secs(2),
                t,
                true,
                t + Duration::from_millis(2400)
            ),
            Err(Error::NonceExhausted)
        );
    }
}
