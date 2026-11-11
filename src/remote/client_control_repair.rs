//! Control-only repair authority. Never shares the pointer lease or sequence.
use super::*;
use crate::remote::control_repair_wire::{
    ActionReceipt, RepairStatus, RepairStatusKind, Scope, Token,
};
use std::collections::VecDeque;
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
type Observer = std::sync::Arc<std::sync::Mutex<Vec<(&'static str, Option<Token>, u64, usize)>>>;
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
thread_local! { static OBSERVER:std::cell::RefCell<Option<Observer>>=const {std::cell::RefCell::new(None)}; }
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
pub(crate) fn with_control_repair_observer<T>(observer: Observer, run: impl FnOnce() -> T) -> T {
    struct Restore(Option<Observer>);
    impl Drop for Restore {
        fn drop(&mut self) {
            OBSERVER.with(|slot| {
                slot.replace(self.0.take());
            });
        }
    }
    let _restore = Restore(OBSERVER.with(|slot| slot.replace(Some(observer))));
    run()
}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
type LifetimeObserver = std::sync::Arc<std::sync::Mutex<Option<(Instant, Instant)>>>;
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
thread_local! { static LIFETIME:std::cell::RefCell<Option<LifetimeObserver>>=const{std::cell::RefCell::new(None)}; }
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
pub(crate) fn with_control_lifetime_observer<T>(
    observer: LifetimeObserver,
    run: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<LifetimeObserver>);
    impl Drop for Restore {
        fn drop(&mut self) {
            LIFETIME.with(|slot| {
                slot.replace(self.0.take());
            });
        }
    }
    let _restore = Restore(LIFETIME.with(|slot| slot.replace(Some(observer))));
    run()
}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
pub(super) fn observe_lifetime(now: Instant) {
    LIFETIME.with(|slot| {
        if let Some(observer) = slot.borrow().as_ref() {
            let mut v = observer.lock().unwrap();
            assert!(v.is_none());
            *v = Some((now, now + LEASE_DURATION));
        }
    });
}
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Requested,
    Accepted,
    Cancelled,
}
#[derive(Clone, Copy)]
struct Phase {
    token: Token,
    stage: Stage,
    deadline: Instant,
    visual: Option<Scope>,
    proof: Option<Scope>,
    confirmed: Option<Scope>,
    after: u64,
}
#[derive(Default)]
pub(in crate::remote::client) struct ControlRepair {
    #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
    observer: Option<Observer>,
    pub(in crate::remote::client) enabled: bool,
    pending: VecDeque<(ActionReceipt, Instant)>,
    completed: u64,
    offer: Option<Token>,
    declining: Option<(Token, Instant)>,
    phase: Option<Phase>,
    retired: Vec<Token>,
    cancelled_seen: Vec<Token>,
    terminal_receipts: VecDeque<ActionReceipt>,
    nonce: u64,
    last_input: Option<Instant>,
    decoded: Option<Scope>,
}
fn failure() -> RemoteError {
    invalid_data("control repair ordering, scope or deadline invalid").into()
}
fn scoped(s: DrawnScope) -> Scope {
    Scope {
        source: s.source_epoch,
        geometry: s.geometry_epoch,
        sequence: s.frame_sequence,
    }
}
fn drawn(s: Scope) -> DrawnScope {
    DrawnScope {
        source_epoch: s.source,
        geometry_epoch: s.geometry,
        frame_sequence: s.sequence,
    }
}
impl ClientControl {
    #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
    pub(in crate::remote::client) fn capture_repair_observer(&mut self) {
        self.repair.observer = OBSERVER.with(|slot| slot.borrow().clone());
    }
    #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
    fn observe_repair(&self, kind: &'static str, token: Option<Token>) {
        if let Some(observer) = &self.repair.observer {
            let mut rows = observer.lock().unwrap();
            assert!(rows.len() < 2048);
            rows.push((kind, token, self.repair.completed, self.held.len()));
        }
    }

    pub(in crate::remote::client) fn note_decoded(
        &mut self,
        purpose: FramePurpose,
        scope: DrawnScope,
    ) -> RemoteResult<()> {
        self.latest_decoded = Some(scope);
        self.repair_frame(purpose, scoped(scope))
    }

    fn lease_deadline(&self) -> RemoteResult<Instant> {
        self.request
            .map(|(_, at)| at + LEASE_DURATION)
            .ok_or_else(failure)
    }
    fn deadline_failure(&self, now: Instant) -> Option<&'static str> {
        let lease = self.request.map(|(_, at)| at + LEASE_DURATION);
        let first = [
            self.repair.pending.front().map(|(_, at)| *at),
            self.repair.declining.map(|(_, at)| at),
            self.repair.phase.map(|p| p.deadline),
        ]
        .into_iter()
        .flatten()
        .min();
        // A five-second protocol timeout cannot be relabelled as natural TTL
        // just because the event loop observes it after the later lease end.
        if first.is_some_and(|at| now >= at && lease.is_none_or(|end| at < end)) {
            return Some("control repair ordering, scope or deadline invalid");
        }
        if lease.is_some_and(|at| now >= at) {
            return Some("control lease expired");
        }
        None
    }
    pub(in crate::remote::client) fn record_host_terminal(&mut self, now: Instant) {
        if self.repair.enabled {
            self.terminal_failure = Some(
                self.deadline_failure(now)
                    .unwrap_or("control lease revoked"),
            );
        }
    }
    pub(in crate::remote::client) fn repair_check(&self, now: Instant) -> RemoteResult<()> {
        if !self.repair.enabled || self.ended {
            return Ok(());
        }
        if let Some(reason) = self.deadline_failure(now) {
            return Err(invalid_data(reason).into());
        }
        Ok(())
    }
    pub(in crate::remote::client) fn repair_blocked(&self) -> bool {
        self.repair.enabled && (self.repair.phase.is_some() || self.repair.declining.is_some())
    }
    pub(in crate::remote::client) fn revoke_local(&mut self) -> Option<ControlRevoke> {
        if self.ended {
            return None;
        }
        let pending = self
            .repair
            .pending
            .iter()
            .map(|(receipt, _)| *receipt)
            .collect();
        let revoke = self.revoke();
        if self.repair.enabled && !self.terminal_seen {
            self.repair.terminal_receipts = pending;
        }
        revoke
    }
    pub(super) fn repair_end(&mut self) {
        self.repair.pending.clear();
        self.repair.terminal_receipts.clear();
        self.repair.phase = None;
        self.repair.offer = None;
        self.repair.declining = None;
    }
    fn note_cancelled(&mut self, token: Token) -> RemoteResult<()> {
        if self.repair.cancelled_seen.contains(&token) || self.repair.cancelled_seen.len() >= 128 {
            return Err(failure());
        }
        self.repair.cancelled_seen.push(token);
        Ok(())
    }
    fn repair_retire(&mut self, token: Token) -> RemoteResult<()> {
        if !self.repair.retired.contains(&token) {
            if self.repair.retired.len() >= 128 {
                return Err(failure());
            }
            self.repair.retired.push(token);
        }
        Ok(())
    }
    pub(in crate::remote::client) fn repair_sent(
        &mut self,
        action: &ControlAction,
        now: Instant,
    ) -> RemoteResult<()> {
        if !self.repair.enabled {
            return Ok(());
        }
        self.repair_check(now)?;
        if self.ended || self.repair_blocked() || self.repair.pending.len() >= 32 {
            return Err(failure());
        }
        let expected = self
            .repair
            .pending
            .back()
            .map_or(self.repair.completed, |(r, _)| r.sequence)
            .checked_add(1)
            .ok_or_else(failure)?;
        if action.sequence != expected || Some(action.lease_epoch) != self.epoch {
            return Err(failure());
        }
        let deadline = (now + COMMAND_TIMEOUT).min(self.lease_deadline()?);
        self.repair.pending.push_back((
            ActionReceipt {
                lease: action.lease_epoch,
                sequence: action.sequence,
                consumed: scoped(action.scope),
            },
            deadline,
        ));
        Ok(())
    }
    pub(in crate::remote::client) fn repair_completed(
        &mut self,
        receipt: ActionReceipt,
        now: Instant,
    ) -> RemoteResult<()> {
        self.repair_check(now)?;
        if self.repair.enabled && self.ended && !self.terminal_seen {
            if self.repair.terminal_receipts.front() == Some(&receipt) {
                self.repair.terminal_receipts.pop_front();
                return Ok(());
            }
            return Err(failure());
        }
        if !self.repair.enabled
            || self.ended
            || self.repair.pending.front().map(|(r, _)| *r) != Some(receipt)
        {
            return Err(failure());
        }
        self.repair.pending.pop_front();
        self.repair.completed = receipt.sequence;
        #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
        self.observe_repair("completed", None);
        Ok(())
    }
    pub(in crate::remote::client) fn repair_offer(
        &mut self,
        token: Token,
        now: Instant,
    ) -> RemoteResult<()> {
        self.repair_check(now)?;
        let initial = self.request.ok_or_else(failure)?.0.scope;
        if !self.repair.enabled
            || self.ended
            || self.epoch != Some(token.lease)
            || token.phase != self.repair.nonce.checked_add(1).ok_or_else(failure)?
            || self.repair.offer.is_some()
            || self.repair.phase.is_some()
            || self.repair.declining.is_some()
            || (token.ready.source, token.ready.geometry)
                != (initial.source_epoch, initial.geometry_epoch)
        {
            return Err(failure());
        }
        self.repair.nonce = token.phase;
        self.repair.offer = Some(token);
        #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
        self.observe_repair("offered", Some(token));
        Ok(())
    }
    pub(in crate::remote::client) fn repair_outgoing(
        &mut self,
        now: Instant,
        viewer_busy: bool,
    ) -> RemoteResult<Option<(MessageKind, Token)>> {
        self.repair_check(now)?;
        if !self.repair.enabled
            || self.ended
            || viewer_busy
            || !self.repair.pending.is_empty()
            || self.repair.phase.is_some()
            || self.repair.declining.is_some()
        {
            return Ok(None);
        }
        let Some(token) = self.repair.offer.take() else {
            return Ok(None);
        };
        let deadline = (now + COMMAND_TIMEOUT).min(self.lease_deadline()?);
        let idle = self
            .repair
            .last_input
            .or(self.request.map(|(_, at)| at))
            .is_some_and(|at| now.saturating_duration_since(at) >= Duration::from_secs(2));
        if self.held.is_empty()
            && !self.left_down
            && self.click.is_none()
            && idle
            && self.ready_sequence() == Some(token.ready.sequence)
            && self.ready.map(scoped) == Some(token.ready)
            && self.repair.completed == token.completed_action
        {
            self.ready = None;
            self.click = None;
            self.repair.phase = Some(Phase {
                token,
                stage: Stage::Requested,
                deadline,
                visual: None,
                proof: None,
                confirmed: None,
                after: token.ready.sequence,
            });
            #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
            self.observe_repair("requested", Some(token));
            Ok(Some((MessageKind::WaylandControlRepairRequest, token)))
        } else {
            // Do not put a Decline in front of a held key's real release.
            // Keep the advisory offer until the physical held set empties.
            if !self.held.is_empty() || self.left_down || self.click.is_some() {
                self.repair.offer = Some(token);
                return Ok(None);
            }
            self.repair.declining = Some((token, deadline));
            Ok(Some((MessageKind::WaylandControlRepairDecline, token)))
        }
    }
    pub(in crate::remote::client) fn repair_frame(
        &mut self,
        purpose: FramePurpose,
        scope: Scope,
    ) -> RemoteResult<()> {
        if !self.repair.enabled {
            return Ok(());
        }
        if purpose == FramePurpose::PopupFamilyProof {
            return Err(failure());
        }
        self.repair.decoded = Some(scope);
        if self.ended {
            return Ok(());
        }
        let Some(phase) = self.repair.phase.as_mut() else {
            return if purpose == FramePurpose::VisualRepair {
                Err(failure())
            } else {
                Ok(())
            };
        };
        if (scope.source, scope.geometry) != (phase.token.ready.source, phase.token.ready.geometry)
        {
            return Err(failure());
        }
        match phase.stage {
            Stage::Accepted => match purpose {
                FramePurpose::VisualRepair | FramePurpose::Video => {
                    if phase.visual.is_some() || scope.sequence <= phase.token.ready.sequence {
                        return Err(failure());
                    }
                    phase.visual = Some(scope);
                }
                FramePurpose::PopupFamilyProof => return Err(failure()),
                FramePurpose::NativeProof => {
                    if phase.proof.is_some()
                        || phase.visual.is_none_or(|v| scope.sequence <= v.sequence)
                    {
                        return Err(failure());
                    }
                    phase.proof = Some(scope);
                }
            },
            Stage::Requested if purpose == FramePurpose::VisualRepair => return Err(failure()),
            Stage::Cancelled => {
                if purpose == FramePurpose::VisualRepair {
                    return Err(failure());
                }
                if purpose == FramePurpose::NativeProof && scope.sequence > phase.after {
                    phase.proof = Some(scope);
                }
            }
            _ => {}
        }
        Ok(())
    }
    pub(in crate::remote::client) fn repair_status(
        &mut self,
        value: RepairStatus,
        now: Instant,
    ) -> RemoteResult<()> {
        self.repair_check(now)?;
        if !self.repair.enabled || self.ended {
            return Err(failure());
        }
        if value.kind == RepairStatusKind::Declined
            && self.repair.declining.map(|(t, _)| t) == Some(value.token)
        {
            self.repair.declining = None;
            self.repair_retire(value.token)?;
            return Ok(());
        }
        if value.kind == RepairStatusKind::Cancelled {
            if self.repair.cancelled_seen.contains(&value.token) {
                return Err(failure());
            }
            if self.repair.offer == Some(value.token) {
                self.repair.offer = None;
                self.repair_retire(value.token)?;
                self.note_cancelled(value.token)?;
                return Ok(());
            }
            if self.repair.retired.contains(&value.token)
                || self.repair.declining.map(|(t, _)| t) == Some(value.token)
            {
                self.note_cancelled(value.token)?;
                return Ok(());
            }
        }
        let phase = self.repair.phase.as_mut().ok_or_else(failure)?;
        if phase.token != value.token {
            return Err(failure());
        }
        match (phase.stage, value.kind) {
            (Stage::Requested, RepairStatusKind::Accepted) => phase.stage = Stage::Accepted,
            (Stage::Requested, RepairStatusKind::Cancelled) => {} // advisory, not dequeue evidence
            (Stage::Requested, RepairStatusKind::RequestCancelled) => {
                phase.stage = Stage::Cancelled;
                phase.visual = None;
                phase.proof = None;
                phase.confirmed = None;
                phase.after = self
                    .repair
                    .decoded
                    .map_or(phase.token.ready.sequence, |s| s.sequence);
            }
            (Stage::Accepted, RepairStatusKind::Ready)
                if value.proof.is_some()
                    && value.proof == phase.proof
                    && phase.confirmed.is_none() =>
            {
                phase.confirmed = value.proof
            }
            _ => return Err(failure()),
        }
        if value.kind == RepairStatusKind::Cancelled {
            self.note_cancelled(value.token)?;
        }
        #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
        if value.kind == RepairStatusKind::Accepted {
            self.observe_repair("accepted", Some(value.token));
        }
        self.repair_resolve(now)
    }
    pub(in crate::remote::client) fn repair_ordinary_ready(
        &mut self,
        scope: DrawnScope,
        now: Instant,
    ) -> RemoteResult<bool> {
        if let Some(phase) = self.repair.phase.as_mut() {
            if phase.stage == Stage::Cancelled && phase.proof == Some(scoped(scope)) {
                phase.confirmed = phase.proof;
                self.repair_resolve(now)?;
            }
            return Ok(true);
        }
        Ok(self.repair.declining.is_some())
    }
    pub(in crate::remote::client) fn repair_resolve(&mut self, now: Instant) -> RemoteResult<()> {
        let Some(phase) = self.repair.phase else {
            return Ok(());
        };
        let Some(proof) = phase.confirmed else {
            return Ok(());
        };
        if !self.ended
            && self.held.is_empty()
            && !self.left_down
            && self.click.is_none()
            && self.repair.decoded == Some(proof)
            && phase.proof == Some(proof)
            && now < phase.deadline
            && self.presented.is_some_and(|(scope, at)| {
                scoped(scope) == proof
                    && now.saturating_duration_since(at)
                        < crate::remote::wayland_pointer_wire::FRAME_MAX_AGE
            })
        {
            self.repair_retire(phase.token)?;
            self.repair.phase = None;
            self.ready = Some(drawn(proof));
        }
        Ok(())
    }
    pub(in crate::remote::client) fn repair_event(
        &mut self,
        event: &ViewerEvent,
        now: Instant,
    ) -> RemoteResult<()> {
        if !self.repair.enabled || self.ended {
            return Ok(());
        }
        self.repair_check(now)?;
        let activity = match event {
            ViewerEvent::WaylandPointerButtonCancel { pressed } => *pressed != self.left_down,
            ViewerEvent::WaylandPointer { .. }
            | ViewerEvent::Key { .. }
            | ViewerEvent::WaylandPointerCancel
            | ViewerEvent::Button { .. } => true,
            _ => false,
        };
        if activity {
            self.repair.last_input = Some(now);
            if self.repair_blocked() {
                return Err(failure()); // outer cleanup performs native release/revoke, never replay
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn scope(sequence: u64) -> DrawnScope {
        DrawnScope {
            source_epoch: 1,
            geometry_epoch: 2,
            frame_sequence: sequence,
        }
    }
    fn fixture(now: Instant) -> (ClientControl, Token) {
        let mut c = ClientControl {
            digest: Some([3; 32]),
            activated: true,
            ..Default::default()
        };
        c.repair.enabled = true;
        c.presented(scope(0), now - Duration::from_secs(3)).unwrap();
        c.receive(ControlStatus {
            scope: scope(0),
            lease_epoch: 7,
            status: ControlStatusKind::Granted,
        })
        .unwrap();
        c.note_decoded(FramePurpose::NativeProof, scope(1)).unwrap();
        c.presented(scope(1), now);
        c.receive(ControlStatus {
            scope: scope(1),
            lease_epoch: 7,
            status: ControlStatusKind::PointerReady,
        })
        .unwrap();
        (
            c,
            Token {
                lease: 7,
                phase: 1,
                ready: scoped(scope(1)),
                completed_action: 0,
            },
        )
    }
    fn receipt(action: ControlAction) -> ActionReceipt {
        ActionReceipt {
            lease: action.lease_epoch,
            sequence: action.sequence,
            consumed: scoped(action.scope),
        }
    }
    #[test]
    fn control_host_terminal_freezes_exact_ttl_without_relabelling_early_revoke() {
        let now = Instant::now();
        for delta in [None, Some(Duration::from_nanos(1))] {
            let (mut c, _) = fixture(now);
            let end = c.lease_deadline().unwrap();
            c.record_host_terminal(delta.map_or(end, |early| end - early));
            c.ended = true;
            c.repair_end();
            let expected = if delta.is_none() {
                "control lease expired"
            } else {
                "control lease revoked"
            };
            assert_eq!(
                c.check((1, 2), end + Duration::from_secs(1))
                    .unwrap_err()
                    .to_string(),
                expected
            );
            assert!(c.ready_sequence().is_none());
        }
        let (mut c, t) = fixture(now);
        let end = c.lease_deadline().unwrap();
        c.repair.declining = Some((t, now + COMMAND_TIMEOUT));
        c.record_host_terminal(end);
        c.ended = true;
        c.repair_end();
        assert_eq!(
            c.check((1, 2), end).unwrap_err().to_string(),
            "control repair ordering, scope or deadline invalid"
        );
        let (mut legacy, _) = fixture(now);
        legacy.repair.enabled = false;
        legacy.record_host_terminal(legacy.lease_deadline().unwrap());
        legacy.ended = true;
        assert!(
            legacy.check((1, 2), now).is_ok(),
            "legacy terminal behavior unchanged"
        );
    }
    #[test]
    fn control_natural_ttl_is_distinct_from_an_earlier_protocol_failure() {
        let now = Instant::now();
        for state in 0..4 {
            let (mut c, t) = fixture(now);
            let end = c.lease_deadline().unwrap();
            match state {
                0 => {}
                1 => c.repair.declining = Some((t, end)),
                2 => {
                    c.repair.phase = Some(Phase {
                        token: t,
                        stage: Stage::Accepted,
                        deadline: end,
                        visual: None,
                        proof: None,
                        confirmed: None,
                        after: 1,
                    })
                }
                _ => c.repair.pending.push_back((
                    ActionReceipt {
                        lease: 7,
                        sequence: 1,
                        consumed: t.ready,
                    },
                    end,
                )),
            }
            assert!(c.repair_check(end - Duration::from_nanos(1)).is_ok());
            assert_eq!(
                c.repair_check(end).unwrap_err().to_string(),
                "control lease expired"
            );
        }
        let (mut c, t) = fixture(now);
        let end = c.lease_deadline().unwrap();
        c.repair.declining = Some((t, now + COMMAND_TIMEOUT));
        assert_eq!(
            c.repair_check(end).unwrap_err().to_string(),
            "control repair ordering, scope or deadline invalid"
        );
    }
    #[test]
    fn control_local_revoke_drains_only_frozen_fifo_before_terminal() {
        let now = Instant::now();
        for terminal_first in [false, true] {
            let (mut c, _) = fixture(now);
            let mut pending = Vec::new();
            for pressed in [true, false] {
                let a = c
                    .event(
                        &ViewerEvent::Key {
                            keycode: 38,
                            pressed,
                        },
                        now,
                    )
                    .unwrap()
                    .unwrap();
                c.repair_sent(&a, now).unwrap();
                pending.push(receipt(a));
            }
            c.revoke_local().unwrap();
            assert_eq!(c.repair.completed, 0);
            if terminal_first {
                c.receive(ControlStatus {
                    scope: scope(0),
                    lease_epoch: 7,
                    status: ControlStatusKind::Revoked,
                })
                .unwrap();
                assert!(c.repair_completed(pending[0], now).is_err());
            } else {
                assert!(c.repair_completed(pending[1], now).is_err());
                c.repair_completed(pending[0], now).unwrap();
                assert!(c.repair_completed(pending[0], now).is_err());
                c.repair_completed(pending[1], now).unwrap();
                assert!(c.repair_completed(pending[1], now).is_err());
                assert_eq!(c.repair.completed, 0);
                assert!(c.ended && c.ready.is_none());
            }
        }
    }
    #[test]
    fn control_advisory_cancel_is_once_and_never_releases_request_queue() {
        let now = Instant::now();
        let (mut c, t) = fixture(now);
        c.repair_offer(t, now).unwrap();
        c.repair_outgoing(now, false).unwrap();
        let status = RepairStatus {
            kind: RepairStatusKind::Cancelled,
            token: t,
            proof: None,
        };
        c.repair_status(status, now).unwrap();
        assert!(c.repair_blocked());
        assert!(c.repair_status(status, now).is_err());
        assert!(c.repair_blocked());
        let (mut c, t) = fixture(now);
        c.repair_offer(t, now).unwrap();
        c.repair_status(status, now).unwrap();
        assert!(c.repair_status(status, now).is_err());
    }
    #[test]
    fn control_keys_release_in_order_and_all_native_completions_precede_repair() {
        let now = Instant::now();
        let (mut c, t) = fixture(now);
        let deadline = c.lease_deadline().unwrap();
        let mut actions = Vec::new();
        for (keycode, pressed) in [(50, true), (38, true), (38, false), (50, false)] {
            let a = c
                .event(&ViewerEvent::Key { keycode, pressed }, now)
                .unwrap()
                .unwrap();
            c.repair_sent(&a, now).unwrap();
            actions.push(a);
        }
        assert!(c.held.is_empty());
        assert_eq!(c.repair.pending.len(), 4);
        c.repair_offer(t, now).unwrap();
        assert!(c.repair_outgoing(now, false).unwrap().is_none());
        for (i, action) in actions.into_iter().enumerate() {
            c.repair_completed(receipt(action), now).unwrap();
            assert_eq!(c.repair.completed, i as u64 + 1);
        }
        assert_eq!(c.lease_deadline().unwrap(), deadline);
        assert_eq!(
            c.repair_outgoing(now, false).unwrap(),
            Some((MessageKind::WaylandControlRepairDecline, t))
        );
    }
    #[test]
    fn control_held_and_half_group_delay_without_synthetic_release() {
        let now = Instant::now();
        let (mut c, t) = fixture(now);
        c.repair_offer(t, now).unwrap();
        assert!(c.repair_outgoing(now, true).unwrap().is_none());
        let down = c
            .event(
                &ViewerEvent::Key {
                    keycode: 50,
                    pressed: true,
                },
                now,
            )
            .unwrap()
            .unwrap();
        c.repair_sent(&down, now).unwrap();
        c.repair_completed(receipt(down), now).unwrap();
        assert!(c.repair_outgoing(now, false).unwrap().is_none());
        assert_eq!(c.held.len(), 1);
        let up = c
            .event(
                &ViewerEvent::Key {
                    keycode: 50,
                    pressed: false,
                },
                now,
            )
            .unwrap()
            .unwrap();
        c.repair_sent(&up, now).unwrap();
        assert!(c.repair_outgoing(now, false).unwrap().is_none());
        c.repair_completed(receipt(up), now).unwrap();
        assert_eq!(c.repair.completed, 2);
    }
    #[test]
    fn control_png_and_normal_ready_do_not_unlock_until_exact_new_proof_presented() {
        let now = Instant::now();
        let (mut c, t) = fixture(now);
        c.repair_offer(t, now).unwrap();
        assert_eq!(
            c.repair_outgoing(now, false).unwrap(),
            Some((MessageKind::WaylandControlRepairRequest, t))
        );
        c.repair_status(
            RepairStatus {
                kind: RepairStatusKind::Accepted,
                token: t,
                proof: None,
            },
            now,
        )
        .unwrap();
        c.note_decoded(FramePurpose::VisualRepair, scope(2))
            .unwrap();
        c.before_frame();
        assert_eq!(c.ready_sequence(), None);
        c.receive(ControlStatus {
            scope: scope(1),
            lease_epoch: 7,
            status: ControlStatusKind::PointerReady,
        })
        .unwrap();
        assert_eq!(c.ready_sequence(), None);
        c.note_decoded(FramePurpose::NativeProof, scope(3)).unwrap();
        c.before_frame();
        c.repair_status(
            RepairStatus {
                kind: RepairStatusKind::Ready,
                token: t,
                proof: Some(scoped(scope(3))),
            },
            now,
        )
        .unwrap();
        assert_eq!(c.ready_sequence(), None);
        c.presented(scope(3), now);
        assert_eq!(c.ready_sequence(), Some(3));
        assert!(
            c.repair_status(
                RepairStatus {
                    kind: RepairStatusKind::Ready,
                    token: t,
                    proof: Some(scoped(scope(3)))
                },
                now
            )
            .is_err()
        );
    }
    #[test]
    fn control_accepted_input_is_rejected_and_completion_scope_order_is_exact() {
        let now = Instant::now();
        let (mut c, t) = fixture(now);
        c.repair_offer(t, now).unwrap();
        c.repair_outgoing(now, false).unwrap();
        c.repair_status(
            RepairStatus {
                kind: RepairStatusKind::Accepted,
                token: t,
                proof: None,
            },
            now,
        )
        .unwrap();
        assert!(
            c.event(
                &ViewerEvent::Key {
                    keycode: 38,
                    pressed: true
                },
                now
            )
            .is_err()
        );
        assert!(c.held.is_empty());
        assert_eq!(c.sequence, 0);
        let (mut c, _) = fixture(now);
        let action = c
            .event(
                &ViewerEvent::Key {
                    keycode: 38,
                    pressed: true,
                },
                now,
            )
            .unwrap()
            .unwrap();
        c.repair_sent(&action, now).unwrap();
        let mut wrong = receipt(action);
        wrong.consumed.sequence += 1;
        assert!(c.repair_completed(wrong, now).is_err());
        assert_eq!(c.repair.completed, 0);
        assert!(c.repair_check(now + COMMAND_TIMEOUT).is_err());
    }
}
