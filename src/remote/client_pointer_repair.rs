//! Client-side ordering and presentation gate for explicitly negotiated repair.
//! Neither a visual ACK nor an ordinary Ready can complete an accepted phase.
use super::*;
use crate::remote::pointer_repair_phase::{
    ActionCompletionFence, ActionReceipt, ClientIdle, Scope, Token,
};
use crate::remote::pointer_repair_wire::{RepairStatus, RepairStatusKind};
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
type LifetimeObserver = std::sync::Arc<std::sync::Mutex<Option<(Instant, Instant)>>>;
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
thread_local! {static LIFETIME_OBSERVER: std::cell::RefCell<Option<LifetimeObserver>> = const { std::cell::RefCell::new(None) };}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
pub(crate) fn with_pointer_lifetime_observer<T>(
    observer: LifetimeObserver,
    run: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<LifetimeObserver>);
    impl Drop for Restore {
        fn drop(&mut self) {
            LIFETIME_OBSERVER.with(|slot| {
                slot.replace(self.0.take());
            });
        }
    }
    let _restore = Restore(LIFETIME_OBSERVER.with(|slot| slot.replace(Some(observer))));
    run()
}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
pub(super) fn observe_pointer_lifetime(requested: Instant) {
    LIFETIME_OBSERVER.with(|slot| {
        if let Some(observer) = slot.borrow().as_ref() {
            let mut value = observer.lock().unwrap();
            assert!(value.is_none(), "one pointer request per original lease");
            *value = Some((
                requested,
                requested + crate::remote::wayland_pointer_wire::LEASE_DURATION,
            ));
        }
    });
}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
type PhaseObserver = std::sync::Arc<std::sync::Mutex<Vec<(&'static str, Token)>>>;
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
thread_local! { static PHASE_OBSERVER: std::cell::RefCell<Option<PhaseObserver>> = const { std::cell::RefCell::new(None) }; }
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
pub(crate) fn with_pointer_repair_observer<T>(
    observer: PhaseObserver,
    run: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<PhaseObserver>);
    impl Drop for Restore {
        fn drop(&mut self) {
            PHASE_OBSERVER.with(|slot| {
                slot.replace(self.0.take());
            });
        }
    }
    let _restore = Restore(PHASE_OBSERVER.with(|slot| slot.replace(Some(observer))));
    run()
}
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
    #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
    observer: Option<PhaseObserver>,
    pub(super) enabled: bool,
    lease: Option<(u64, Instant)>,
    fence: Option<ActionCompletionFence>,
    terminal_receipt: Option<ActionReceipt>,
    natural_expiry: bool,
    expiry: Option<ExpiryDrain>,
    pending_expiry_revoke: Option<crate::remote::wayland_pointer_wire::PointerRevoke>,
    action_deadline: Option<Instant>,
    last_input: Option<Instant>,
    held_keys: std::collections::BTreeSet<u8>,
    latest_decoded_sequence: Option<u64>,
    offer: Option<Token>,
    declining: Option<(Token, Instant)>,
    phase: Option<Phase>,
    retired: Vec<Token>,
    advisory_cancelled: Vec<Token>,
    last_ready_status: Option<Scope>,
    last_waiting_status: Option<Scope>,
    last_phase: u64,
}
// Frozen protocol evidence only. This cannot restore a completion fence,
// candidate, native mapping, Ready ticket, or lease after natural expiry.
struct ExpiryDrain {
    late_proof_seen: bool,
    late_proof_after: Option<Scope>,
    late_offer: Option<Token>,
    late_cancel: Option<Token>,
    receipt: Option<ActionReceipt>,
    offer: Option<Token>,
    declining: Option<Token>,
    phase: Option<Phase>,
    cancelled: Vec<Token>,
    scopes: Vec<Scope>,
    statuses: Vec<(
        Scope,
        crate::remote::wayland_pointer_wire::PointerStatusKind,
    )>,
}
impl ExpiryDrain {
    fn status(&mut self, value: RepairStatus) -> RemoteResult<()> {
        if value.kind == RepairStatusKind::Declined && self.declining == Some(value.token) {
            self.declining = None;
            return Ok(());
        }
        if value.kind == RepairStatusKind::Cancelled {
            if self.late_cancel == Some(value.token) {
                self.late_cancel = None;
                self.cancelled.push(value.token);
                return Ok(());
            }
            if self.cancelled.contains(&value.token) {
                return Err(repair_error());
            }
            if self.offer == Some(value.token) {
                self.offer = None;
                self.cancelled.push(value.token);
                return Ok(());
            }
            if self.declining == Some(value.token)
                || self
                    .phase
                    .is_some_and(|p| p.token == value.token && p.kind == PhaseKind::Requested)
            {
                self.cancelled.push(value.token);
                return Ok(());
            }
            return Err(repair_error());
        }
        let phase = self.phase.as_mut().ok_or_else(repair_error)?;
        if phase.token != value.token {
            return Err(repair_error());
        }
        match (phase.kind, value.kind) {
            (PhaseKind::Requested, RepairStatusKind::Accepted) => phase.kind = PhaseKind::Accepted,
            (PhaseKind::Requested, RepairStatusKind::RequestCancelled) => self.phase = None,
            (PhaseKind::Accepted, RepairStatusKind::Ready)
                if value.proof.is_some()
                    && value.proof == phase.proof
                    && phase.confirmed.is_none() =>
            {
                self.phase = None
            }
            _ => return Err(repair_error()),
        }
        Ok(())
    }
    fn frame(&mut self, purpose: FramePurpose, scope: Scope) -> RemoteResult<()> {
        if let Some(phase) = self.phase.as_mut() {
            if (scope.source, scope.geometry)
                != (phase.token.ready.source, phase.token.ready.geometry)
            {
                return Err(repair_error());
            }
            if phase.kind == PhaseKind::Accepted {
                match purpose {
                    FramePurpose::VisualRepair | FramePurpose::Video if phase.visual.is_none() => {
                        if scope.sequence <= phase.token.ready.sequence {
                            return Err(repair_error());
                        }
                        phase.visual = Some(scope);
                    }
                    FramePurpose::NativeProof => {
                        if self.late_proof_seen
                            || phase.proof.is_some()
                            || phase.visual.is_none_or(|v| scope.sequence <= v.sequence)
                        {
                            return Err(repair_error());
                        }
                        phase.proof = Some(scope);
                        self.late_proof_seen = true;
                        self.late_proof_after = None;
                        if !self.scopes.contains(&scope) {
                            self.scopes.push(scope);
                        }
                    }
                    _ => {}
                }
                return Ok(());
            }
        }
        if purpose == FramePurpose::NativeProof {
            if self.late_proof_seen {
                return Err(repair_error());
            }
            let previous = self.late_proof_after.ok_or_else(repair_error)?;
            if (scope.source, scope.geometry) != (previous.source, previous.geometry)
                || scope.sequence <= previous.sequence
            {
                return Err(repair_error());
            }
            self.late_proof_after = None;
            self.late_proof_seen = true;
            if !self.scopes.contains(&scope) {
                self.scopes.push(scope);
            }
        }
        Ok(())
    }
    fn pointer_status(
        &mut self,
        value: crate::remote::wayland_pointer_wire::PointerStatus,
    ) -> RemoteResult<()> {
        let scope = value.scope.into();
        if !self.scopes.contains(&scope) || self.statuses.contains(&(scope, value.status)) {
            return Err(repair_error());
        }
        self.statuses.push((scope, value.status));
        Ok(())
    }
}
fn repair_error() -> RemoteError {
    invalid_data("pointer repair ordering, scope or deadline invalid").into()
}
impl ClientPointer {
    #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
    pub(super) fn capture_repair_observer(&mut self) {
        self.repair.observer = PHASE_OBSERVER.with(|slot| slot.borrow().clone());
    }
    #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
    fn observe_repair(&self, kind: &'static str, token: Token) {
        if let Some(observer) = &self.repair.observer {
            let mut rows = observer.lock().unwrap();
            assert!(rows.len() < 512, "bounded repair observation ledger");
            rows.push((kind, token));
        }
    }

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
        self.repair.expiry = None;
        self.repair.pending_expiry_revoke = None;
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
    pub(super) fn repair_check(&mut self, now: Instant) -> RemoteResult<bool> {
        if !self.repair.enabled {
            return Ok(!self.ended);
        }
        if self.ended {
            return Ok(false);
        }
        let lease_deadline = self.repair.lease.map(|(_, at)| at);
        let first = [
            self.repair.action_deadline,
            self.repair.declining.map(|(_, at)| at),
            self.repair.phase.map(|p| p.deadline),
        ]
        .into_iter()
        .flatten()
        .min();
        // A real earlier protocol timeout remains a failure even when this
        // thread is scheduled only after the later lease deadline.
        if first.is_some_and(|at| now >= at && lease_deadline.is_none_or(|lease| at < lease)) {
            self.revoke();
            return Err(repair_error());
        }
        if lease_deadline.is_some_and(|at| now >= at) {
            let mut scopes = Vec::new();
            for scope in [self.presented.map(|(s, _)| s), self.ready, self.consumed]
                .into_iter()
                .flatten()
            {
                if !scopes.contains(&Scope::from(scope)) {
                    scopes.push(scope.into());
                }
            }
            if let Some(scope) = self.repair.phase.and_then(|p| p.proof) {
                if !scopes.contains(&scope) {
                    scopes.push(scope);
                }
            }
            // Only a fully derivable next advisory offer can cross this local
            // expiry boundary. Freeze freshness now, never at its later arrival.
            let late_offer = (|| {
                if self.repair.offer.is_some()
                    || self.repair.declining.is_some()
                    || self.repair.phase.is_some()
                    || self.repair.action_deadline.is_some()
                    || self.newer_frame
                {
                    return None;
                }
                let (scope, at) = self.presented?;
                let initial = self.requested?.0;
                if self.consumed == Some(scope)
                    || (scope.source_epoch, scope.geometry_epoch)
                        != (initial.source_epoch, initial.geometry_epoch)
                    || now.saturating_duration_since(at)
                        >= crate::remote::wayland_pointer_wire::FRAME_MAX_AGE
                {
                    return None;
                }
                Some(Token {
                    lease: self.repair.lease?.0,
                    phase: self.repair.last_phase.checked_add(1)?,
                    ready: scope.into(),
                    completed_action: self.repair.fence.as_ref()?.repair_watermark()?,
                })
            })();
            let drain = ExpiryDrain {
                late_proof_seen: self.repair.phase.is_some_and(|p| p.proof.is_some()),
                late_proof_after: self.requested.and_then(|(initial, _)| {
                    self.repair.latest_decoded_sequence.map(|sequence| Scope {
                        source: initial.source_epoch,
                        geometry: initial.geometry_epoch,
                        sequence,
                    })
                }),
                late_offer,
                late_cancel: None,
                receipt: self
                    .repair
                    .fence
                    .as_ref()
                    .and_then(ActionCompletionFence::pending_receipt),
                offer: self.repair.offer,
                declining: self.repair.declining.map(|(t, _)| t),
                phase: self.repair.phase,
                cancelled: self.repair.advisory_cancelled.clone(),
                scopes,
                statuses: [
                    self.repair.last_ready_status.map(|s| {
                        (
                            s,
                            crate::remote::wayland_pointer_wire::PointerStatusKind::Ready,
                        )
                    }),
                    self.repair.last_waiting_status.map(|s| {
                        (
                            s,
                            crate::remote::wayland_pointer_wire::PointerStatusKind::Waiting,
                        )
                    }),
                ]
                .into_iter()
                .flatten()
                .collect(),
            };
            let revoke = self.revoke();
            self.repair.natural_expiry = true;
            self.repair.expiry = Some(drain);
            self.repair.pending_expiry_revoke = revoke;
            return Ok(false);
        }
        Ok(true)
    }
    pub(super) fn take_expiry_revoke(
        &mut self,
    ) -> Option<crate::remote::wayland_pointer_wire::PointerRevoke> {
        self.repair.pending_expiry_revoke.take()
    }
    pub(super) fn repair_note_pointer_status(
        &mut self,
        value: crate::remote::wayland_pointer_wire::PointerStatus,
    ) {
        if !self.repair.enabled || self.ended {
            return;
        }
        match value.status {
            crate::remote::wayland_pointer_wire::PointerStatusKind::Ready => {
                self.repair.last_ready_status = Some(value.scope.into())
            }
            crate::remote::wayland_pointer_wire::PointerStatusKind::Waiting => {
                self.repair.last_waiting_status = Some(value.scope.into())
            }
            _ => {}
        }
    }
    fn repair_note_cancelled(&mut self, token: Token) {
        if !self.repair.advisory_cancelled.contains(&token) {
            // Tokens are bounded by the same retired-token fence (plus current).
            self.repair.advisory_cancelled.push(token);
        }
    }
    pub(super) fn repair_expired_status(
        &mut self,
        value: crate::remote::wayland_pointer_wire::PointerStatus,
    ) -> Option<RemoteResult<()>> {
        self.repair.natural_expiry.then(|| {
            self.repair
                .expiry
                .as_mut()
                .ok_or_else(repair_error)?
                .pointer_status(value)
        })
    }
    pub(super) fn repair_scroll_held(&self) -> bool {
        self.repair.enabled && !self.repair.held_keys.is_empty()
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
    ) -> RemoteResult<bool> {
        if !self.repair.enabled {
            return Ok(true);
        }
        if !self.repair_check(now)? {
            return Ok(false);
        }
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
        Ok(true)
    }
    pub(super) fn repair_completed(
        &mut self,
        receipt: ActionReceipt,
        now: Instant,
    ) -> RemoteResult<()> {
        if !self.repair.enabled {
            return Err(repair_error());
        }
        self.repair_check(now)?;
        if self.repair.natural_expiry {
            let drain = self.repair.expiry.as_mut().ok_or_else(repair_error)?;
            if drain.receipt != Some(receipt) {
                return Err(repair_error());
            }
            drain.receipt = None;
            return Ok(());
        }
        if self.ended {
            if !self.terminal_seen && self.repair.terminal_receipt == Some(receipt) {
                self.repair.terminal_receipt = None;
                return Ok(());
            }
            return Err(repair_error());
        }
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
        if self.repair.natural_expiry {
            let drain = self.repair.expiry.as_mut().ok_or_else(repair_error)?;
            if drain.late_offer != Some(token) {
                return Err(repair_error());
            }
            drain.late_offer = None;
            drain.late_cancel = Some(token);
            return Ok(());
        }
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
        #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
        self.observe_repair("offered", token);
        Ok(())
    }
    /// Called after draining this pass's local events, before enqueueing Request.
    pub(super) fn repair_outgoing(
        &mut self,
        now: Instant,
        viewer_busy: bool,
    ) -> RemoteResult<Option<(MessageKind, Token)>> {
        if !self.repair_check(now)? {
            return Ok(None);
        }
        // An open native axis frame has not emitted a ViewerEvent yet. Keep
        // its offer untouched until the real group finishes or is discarded.
        if viewer_busy {
            return Ok(None);
        }
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
            #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
            self.observe_repair("requested", token);
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
        if purpose == FramePurpose::PopupFamilyProof {
            return Err(repair_error());
        }
        if self.repair.natural_expiry {
            if let Some(drain) = self.repair.expiry.as_mut() {
                drain.frame(purpose, scope)?;
            }
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
                FramePurpose::PopupFamilyProof => return Err(repair_error()),
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
        if !self.repair.enabled {
            return Err(repair_error());
        }
        self.repair_check(now)?;
        if self.repair.natural_expiry {
            return self
                .repair
                .expiry
                .as_mut()
                .ok_or_else(repair_error)?
                .status(status);
        }
        if self.ended {
            return Err(repair_error());
        }
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
            self.repair_note_cancelled(status.token);
            return Ok(());
        }
        if status.kind == RepairStatusKind::Cancelled && self.repair.retired.contains(&status.token)
        {
            self.repair_note_cancelled(status.token);
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
                if !self.repair.advisory_cancelled.contains(&status.token) {
                    self.repair.advisory_cancelled.push(status.token);
                }
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
        #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
        if status.kind == RepairStatusKind::Accepted {
            self.observe_repair("accepted", status.token);
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
            p.repair_outgoing(now, false).unwrap(),
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
                p.repair_outgoing(now, false).unwrap(),
                Some((MessageKind::WaylandPointerRepairDecline, t))
            );
            assert!(p.event(&motion(5), now).unwrap().is_none());
            assert_eq!(p.sequence, 0);
            assert!(p.repair_outgoing(now, false).unwrap().is_none());
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
        p.repair_outgoing(now, false).unwrap();
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
        p.repair_outgoing(now, false).unwrap();
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
    fn wheel(sequence: u64, up: bool) -> ViewerEvent {
        let mut event = motion(sequence);
        if let ViewerEvent::WaylandPointer { kind, .. } = &mut event {
            *kind = if up {
                crate::remote::x11_viewer::WaylandPointerEventKind::ScrollUp
            } else {
                crate::remote::x11_viewer::WaylandPointerEventKind::ScrollDown
            };
        }
        event
    }
    #[test]
    fn repair_unclosed_native_input_group_defers_offer_without_dequeue_or_renewal() {
        let now = Instant::now();
        let (mut p, t) = fixture(now);
        p.repair_offer(t, now).unwrap();
        let lease = p.repair.lease;
        assert!(p.repair_outgoing(now, true).unwrap().is_none());
        assert_eq!(p.repair.offer, Some(t));
        assert!(p.repair.phase.is_none() && p.repair.declining.is_none());
        assert_eq!(p.repair.lease, lease);
        assert_eq!(
            p.repair_outgoing(now, false).unwrap(),
            Some((MessageKind::WaylandPointerRepairRequest, t))
        );
    }
    fn horizontal_wheel(sequence: u64, left: bool) -> ViewerEvent {
        let mut event = motion(sequence);
        if let ViewerEvent::WaylandPointer { kind, .. } = &mut event {
            *kind = if left {
                crate::remote::x11_viewer::WaylandPointerEventKind::ScrollLeft
            } else {
                crate::remote::x11_viewer::WaylandPointerEventKind::ScrollRight
            };
        }
        event
    }
    #[test]
    fn repair_scroll_axes_are_independent_and_all_directions_complete_exactly_once() {
        let now = Instant::now();
        for vertical in [false, true] {
            for horizontal in [false, true] {
                for direction in 0..4 {
                    let (mut p, _) = fixture(now);
                    p.scroll_enabled = vertical;
                    p.horizontal_enabled = horizontal;
                    let event = match direction {
                        0 => wheel(5, true),
                        1 => wheel(5, false),
                        2 => horizontal_wheel(5, true),
                        _ => horizontal_wheel(5, false),
                    };
                    let action = p.event(&event, now).unwrap();
                    if !(if direction < 2 { vertical } else { horizontal }) {
                        assert!(action.is_none());
                        assert_eq!(p.sequence, 0);
                        assert_eq!(p.repair.fence.as_ref().unwrap().repair_watermark(), Some(0));
                        continue;
                    }
                    let action = action.unwrap();
                    assert_eq!(
                        action.kind,
                        [
                            PointerActionKind::ScrollUp,
                            PointerActionKind::ScrollDown,
                            PointerActionKind::ScrollLeft,
                            PointerActionKind::ScrollRight
                        ][direction]
                    );
                    let deadline = p.repair.lease;
                    assert!(p.repair_sent(&action, now).unwrap());
                    assert!(p.event(&event, now).unwrap().is_none());
                    assert_eq!(p.repair.fence.as_ref().unwrap().repair_watermark(), None);
                    let receipt = ActionReceipt {
                        lease: action.lease_epoch,
                        sequence: action.sequence,
                        consumed: action.scope.into(),
                    };
                    p.repair_completed(receipt, now).unwrap();
                    assert_eq!(p.repair.fence.as_ref().unwrap().repair_watermark(), Some(1));
                    assert_eq!(p.repair.lease, deadline);
                    assert!(!p.fresh(now));
                    assert!(p.repair_completed(receipt, now).is_err());
                }
            }
        }
    }
    #[test]
    fn repair_horizontal_wheels_during_phase_hold_or_expiry_are_never_replayed() {
        let now = Instant::now();
        for state in 0..6 {
            let (mut p, t) = match state {
                0 => requested(now),
                1 => accepted(now),
                _ => fixture(now),
            };
            p.horizontal_enabled = true;
            match state {
                2 => {
                    p.repair.last_input = Some(now);
                    p.repair_offer(t, now).unwrap();
                    assert_eq!(
                        p.repair_outgoing(now, false).unwrap().unwrap().0,
                        MessageKind::WaylandPointerRepairDecline
                    );
                }
                3 => {
                    p.event(
                        &ViewerEvent::Key {
                            keycode: 38,
                            pressed: true,
                        },
                        now,
                    )
                    .unwrap();
                }
                4 => {
                    p.left_down = true;
                }
                5 => {
                    p.repair.lease.as_mut().unwrap().1 = now;
                }
                _ => {}
            }
            for left in [false, true] {
                assert!(p.event(&horizontal_wheel(5, left), now).unwrap().is_none());
            }
            assert_eq!(p.sequence, 0);
            assert!(p.repair.action_deadline.is_none());
            assert_eq!(p.repair.last_input, Some(now));
            if state == 3 {
                p.event(
                    &ViewerEvent::Key {
                        keycode: 38,
                        pressed: false,
                    },
                    now,
                )
                .unwrap();
                assert_eq!(p.sequence, 0);
                assert!(p.event(&horizontal_wheel(5, true), now).unwrap().is_some());
            }
            if state == 5 {
                assert!(p.ended);
                p.presented(scope(6).into(), now);
                assert!(p.event(&horizontal_wheel(6, false), now).unwrap().is_none());
            }
        }
    }
    #[test]
    fn repair_vertical_scroll_needs_permission_and_exact_action_completion() {
        let now = Instant::now();
        for up in [false, true] {
            let (mut p, t) = fixture(now);
            assert!(p.event(&wheel(5, up), now).unwrap().is_none());
            p.scroll_enabled = true;
            let action = p.event(&wheel(5, up), now).unwrap().unwrap();
            assert_eq!(
                action.kind,
                if up {
                    PointerActionKind::ScrollUp
                } else {
                    PointerActionKind::ScrollDown
                }
            );
            assert!(p.repair_sent(&action, now).unwrap());
            assert!(p.event(&wheel(5, !up), now).unwrap().is_none());
            assert_eq!(p.repair.fence.as_ref().unwrap().repair_watermark(), None);
            p.repair_offer(t, now).unwrap();
            assert!(p.repair_outgoing(now, false).unwrap().is_none());
            let receipt = ActionReceipt {
                lease: action.lease_epoch,
                sequence: action.sequence,
                consumed: action.scope.into(),
            };
            // A malformed receipt terminally poisons its completion fence;
            // test that negative session separately, never recover it in-place.
            let (mut invalid, _) = fixture(now);
            invalid.scroll_enabled = true;
            let invalid_action = invalid.event(&wheel(5, up), now).unwrap().unwrap();
            invalid.repair_sent(&invalid_action, now).unwrap();
            assert!(
                invalid
                    .repair_completed(
                        ActionReceipt {
                            sequence: receipt.sequence + 1,
                            ..receipt
                        },
                        now
                    )
                    .is_err()
            );
            assert!(invalid.repair_completed(receipt, now).is_err());
            p.repair_completed(receipt, now).unwrap();
            assert_eq!(p.repair.fence.as_ref().unwrap().repair_watermark(), Some(1));
            assert!(p.repair_completed(receipt, now).is_err());
            assert!(!p.fresh(now));
        }
    }
    #[test]
    fn repair_vertical_scroll_during_phase_hold_or_expiry_is_dropped_not_queued() {
        let now = Instant::now();
        for state in 0..6 {
            let (mut p, t) = match state {
                0 => requested(now),
                1 => accepted(now),
                _ => fixture(now),
            };
            p.scroll_enabled = true;
            match state {
                2 => {
                    p.repair.last_input = Some(now);
                    p.repair_offer(t, now).unwrap();
                    assert_eq!(
                        p.repair_outgoing(now, false).unwrap().unwrap().0,
                        MessageKind::WaylandPointerRepairDecline
                    );
                }
                3 => {
                    p.event(
                        &ViewerEvent::Key {
                            keycode: 38,
                            pressed: true,
                        },
                        now,
                    )
                    .unwrap();
                }
                4 => {
                    p.left_down = true;
                }
                5 => {
                    p.repair.lease.as_mut().unwrap().1 = now;
                }
                _ => {}
            }
            let before = p.sequence;
            assert!(p.event(&wheel(5, true), now).unwrap().is_none());
            assert!(p.event(&wheel(5, false), now).unwrap().is_none());
            assert_eq!(p.sequence, before);
            assert_eq!(p.repair.last_input, Some(now));
            assert!(p.repair.action_deadline.is_none());
            if state == 3 {
                p.event(
                    &ViewerEvent::Key {
                        keycode: 38,
                        pressed: false,
                    },
                    now,
                )
                .unwrap();
                assert_eq!(p.sequence, before, "release does not replay dropped wheels");
                assert!(p.event(&wheel(5, true), now).unwrap().is_some());
            }
            if state == 5 {
                assert!(p.ended);
                p.presented(scope(6).into(), now);
                assert!(p.event(&wheel(6, true), now).unwrap().is_none());
            }
        }
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
        assert_eq!(p.repair_outgoing(now, false).unwrap(), None);
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
            p.repair_outgoing(now, false).unwrap(),
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
    fn natural_ttl_one_decoded_late_proof_is_only_terminal_status_evidence() {
        let now = Instant::now();
        let (mut p, token) = fixture(now);
        p.repair.latest_decoded_sequence = Some(token.ready.sequence);
        p.repair.lease.as_mut().unwrap().1 = now;
        assert!(!p.repair_check(now).unwrap());
        let next = Scope {
            sequence: token.ready.sequence + 1,
            ..token.ready
        };
        let drain = p.repair.expiry.as_mut().unwrap();
        assert!(drain.frame(FramePurpose::NativeProof, token.ready).is_err());
        assert!(
            drain
                .frame(FramePurpose::NativeProof, Scope { source: 99, ..next })
                .is_err()
        );
        drain.frame(FramePurpose::NativeProof, next).unwrap();
        assert!(drain.frame(FramePurpose::NativeProof, next).is_err());
        assert!(
            drain
                .frame(
                    FramePurpose::NativeProof,
                    Scope {
                        sequence: next.sequence + 1,
                        ..next
                    }
                )
                .is_err()
        );
        for status in [PointerStatusKind::Ready, PointerStatusKind::Waiting] {
            let value = PointerStatus {
                lease_epoch: token.lease,
                scope: next.into(),
                status,
            };
            drain.pointer_status(value).unwrap();
            assert!(drain.pointer_status(value).is_err());
        }
        drain.frame(FramePurpose::Video, next).unwrap();
        drain.frame(FramePurpose::VisualRepair, next).unwrap();
        assert!(p.ended && p.ready.is_none());
        p.repair_end();
        assert!(p.repair.expiry.is_none());
    }

    #[test]
    fn natural_ttl_late_offer_is_one_frozen_exact_advisory_only() {
        let now = Instant::now();
        let make = || {
            let (mut p, t) = fixture(now);
            p.repair.lease.as_mut().unwrap().1 = now;
            (p, t)
        };
        for field in 0..6 {
            let (mut p, mut t) = make();
            assert!(!p.repair_check(now).unwrap());
            match field {
                0 => t.lease += 1,
                1 => t.phase += 1,
                2 => t.ready.source += 1,
                3 => t.ready.geometry += 1,
                4 => t.ready.sequence += 1,
                _ => t.completed_action += 1,
            }
            assert!(p.repair_offer(t, now).is_err());
        }
        let (mut p, t) = make();
        assert!(!p.repair_check(now).unwrap());
        let cancelled = RepairStatus {
            kind: RepairStatusKind::Cancelled,
            token: t,
            proof: None,
        };
        assert!(p.repair_status(cancelled, now).is_err());
        p.repair_offer(t, now + Duration::from_secs(2)).unwrap();
        assert!(p.repair_offer(t, now).is_err());
        p.repair_status(cancelled, now).unwrap();
        assert!(p.repair_status(cancelled, now).is_err());
        assert!(p.ended && p.ready.is_none() && p.repair.phase.is_none());
        assert!(p.repair_outgoing(now, false).unwrap().is_none());
        assert!(
            p.repair
                .fence
                .as_ref()
                .unwrap()
                .repair_watermark()
                .is_none()
        );
    }

    #[test]
    fn natural_ttl_does_not_guess_offer_without_fresh_complete_evidence() {
        let now = Instant::now();
        for missing in 0..9 {
            let (mut p, t) = fixture(now);
            p.repair.lease.as_mut().unwrap().1 = now;
            match missing {
                0 => {
                    p.presented.as_mut().unwrap().1 =
                        now - crate::remote::wayland_pointer_wire::FRAME_MAX_AGE
                }
                1 => p.newer_frame = true,
                2 => p.consumed = p.presented.map(|(s, _)| s),
                3 => p.repair.offer = Some(t),
                4 => p.repair.declining = Some((t, now)),
                5 => p.repair.action_deadline = Some(now),
                6 => p.repair.last_phase = u64::MAX,
                7 => p.presented = None,
                _ => {
                    p.repair.phase = Some(Phase {
                        token: t,
                        kind: PhaseKind::Requested,
                        deadline: now,
                        visual: None,
                        proof: None,
                        confirmed: None,
                        recovery_after: 5,
                    })
                }
            }
            assert!(!p.repair_check(now).unwrap());
            assert!(p.repair_offer(t, now).is_err());
        }
    }

    #[test]
    fn natural_ttl_closes_each_pending_authority_once_but_keeps_readonly_frames() {
        let now = Instant::now();
        for mode in 0..4 {
            let (mut p, t) = fixture(now);
            let lease = p.repair.lease.unwrap().1;
            let near = lease - Duration::from_secs(2);
            p.presented = Some((t.ready.into(), near));
            match mode {
                0 => {
                    p.repair_sent(
                        &PointerAction {
                            lease_epoch: 9,
                            sequence: 1,
                            scope: t.ready.into(),
                            kind: PointerActionKind::Motion,
                            x: 0,
                            y: 0,
                        },
                        near,
                    )
                    .unwrap();
                }
                1 => {
                    p.repair.last_input = Some(near);
                    p.repair_offer(t, near).unwrap();
                    p.repair_outgoing(near, false).unwrap();
                }
                _ => {
                    p.repair_offer(t, near).unwrap();
                    p.repair_outgoing(near, false).unwrap();
                    if mode == 3 {
                        p.repair_status(
                            RepairStatus {
                                kind: RepairStatusKind::Accepted,
                                token: t,
                                proof: None,
                            },
                            near,
                        )
                        .unwrap();
                    }
                }
            }
            assert!(p.repair_check(lease - Duration::from_nanos(1)).unwrap());
            assert!(!p.repair_check(lease).unwrap());
            assert!(p.ended && p.repair.natural_expiry);
            assert!(p.ready.is_none() && p.click.is_none());
            assert!(!p.fresh(lease));
            assert_eq!(p.take_expiry_revoke().unwrap().lease_epoch, 9);
            assert!(p.take_expiry_revoke().is_none());
            assert_eq!(p.repair.lease.unwrap().1, lease);
            assert_eq!(p.repair.fence.as_ref().unwrap().repair_watermark(), None);
            assert!(p.event(&motion(5), lease).unwrap().is_none());
            p.repair_frame(FramePurpose::VisualRepair, scope(6))
                .unwrap();
            assert!(p.presented(scope(7).into(), lease).is_none());
            assert!(p.ready.is_none());
            p.receive(PointerStatus {
                lease_epoch: 9,
                scope: t.ready.into(),
                status: PointerStatusKind::Revoked,
            })
            .unwrap();
            assert!(p.repair.expiry.is_none());
            assert!(p.take_expiry_revoke().is_none());
            p.repair_frame(FramePurpose::Video, scope(8)).unwrap();
            p.repair_frame(FramePurpose::VisualRepair, scope(9))
                .unwrap();
            assert!(
                p.repair_status(
                    RepairStatus {
                        kind: RepairStatusKind::Cancelled,
                        token: t,
                        proof: None
                    },
                    lease
                )
                .is_err()
            );
        }
    }
    #[test]
    fn ttl_drain_consumes_only_frozen_exact_phase_and_receipt_without_ready() {
        let now = Instant::now();
        let (mut p, t) = fixture(now);
        let lease = p.repair.lease.unwrap().1;
        let near = lease - Duration::from_secs(2);
        let receipt = ActionReceipt {
            lease: 9,
            sequence: 1,
            consumed: t.ready,
        };
        p.repair_sent(
            &PointerAction {
                lease_epoch: 9,
                sequence: 1,
                scope: t.ready.into(),
                kind: PointerActionKind::Motion,
                x: 0,
                y: 0,
            },
            near,
        )
        .unwrap();
        p.repair_completed(receipt, lease).unwrap(); // Receiver discovers natural TTL first.
        assert!(p.ended && p.repair.natural_expiry);
        assert!(p.take_expiry_revoke().is_some());
        assert!(p.repair_completed(receipt, lease).is_err());
        let (mut p, t) = fixture(now);
        p.presented = Some((t.ready.into(), near));
        p.repair_offer(t, near).unwrap();
        p.repair_outgoing(near, false).unwrap();
        p.repair_check(lease).unwrap();
        let mut wrong = t;
        wrong.phase += 1;
        assert!(
            p.repair_status(
                RepairStatus {
                    kind: RepairStatusKind::Accepted,
                    token: wrong,
                    proof: None
                },
                lease
            )
            .is_err()
        );
        p.repair_status(
            RepairStatus {
                kind: RepairStatusKind::Accepted,
                token: t,
                proof: None,
            },
            lease,
        )
        .unwrap();
        assert!(
            p.repair_status(
                RepairStatus {
                    kind: RepairStatusKind::Accepted,
                    token: t,
                    proof: None
                },
                lease
            )
            .is_err()
        );
        p.repair_frame(FramePurpose::VisualRepair, scope(6))
            .unwrap();
        p.repair_frame(FramePurpose::NativeProof, scope(7)).unwrap();
        p.repair_status(
            RepairStatus {
                kind: RepairStatusKind::Ready,
                token: t,
                proof: Some(scope(7)),
            },
            lease,
        )
        .unwrap();
        assert!(p.ready.is_none() && p.repair.phase.is_none());
        assert!(!p.fresh(lease));
        assert!(
            p.repair_status(
                RepairStatus {
                    kind: RepairStatusKind::Ready,
                    token: t,
                    proof: Some(scope(7))
                },
                lease
            )
            .is_err()
        );
        p.receive(PointerStatus {
            lease_epoch: 9,
            scope: t.ready.into(),
            status: PointerStatusKind::Revoked,
        })
        .unwrap();
        assert!(
            p.repair_status(
                RepairStatus {
                    kind: RepairStatusKind::Ready,
                    token: t,
                    proof: Some(scope(7))
                },
                lease
            )
            .is_err()
        );
    }
    #[test]
    fn earlier_five_second_timeout_is_never_relabelled_as_natural_ttl() {
        let now = Instant::now();
        for after_ttl in [false, true] {
            let (mut p, t) = fixture(now);
            let lease = p.repair.lease.unwrap().1;
            let sent = lease - Duration::from_secs(7);
            p.repair_sent(
                &PointerAction {
                    lease_epoch: 9,
                    sequence: 1,
                    scope: t.ready.into(),
                    kind: PointerActionKind::Motion,
                    x: 0,
                    y: 0,
                },
                sent,
            )
            .unwrap();
            let timeout = sent + DEADLINE;
            assert!(p.repair_check(timeout - Duration::from_nanos(1)).unwrap());
            assert!(
                p.repair_check(if after_ttl {
                    lease + Duration::from_nanos(1)
                } else {
                    timeout
                })
                .is_err()
            );
            assert!(p.ended);
            assert!(!p.repair.natural_expiry);
            assert!(p.take_expiry_revoke().is_none());
        }
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
                p.repair_outgoing(now, false).unwrap(),
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
        p.repair_outgoing(now, false).unwrap();
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
