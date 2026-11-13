//! A single client bundle state, not two parallel legacy leases.
use super::*;
use crate::remote::wayland_control_wire::*;
#[path = "client_control_repair.rs"]
mod repair;
use repair::ControlRepair;
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
pub(crate) use repair::{with_control_lifetime_observer, with_control_repair_observer};
#[derive(Default)]
pub(super) struct ClientControl {
    pub(super) repair: ControlRepair,
    pub(super) resumable_enabled: bool,
    resume_local_validation_pending: bool,
    pub(super) resumable: Option<crate::remote::resumable_control::ClientSession>,
    latest_decoded: Option<DrawnScope>,
    receipts: std::collections::VecDeque<(DrawnScope, bool)>,
    pub(super) scroll_enabled: bool,
    pub(super) horizontal_enabled: bool,
    pub(super) digest: Option<[u8; 32]>,
    request: Option<(ControlRequest, Instant)>,
    pub(super) epoch: Option<u64>,
    pub(super) activated: bool,
    pub(super) ended: bool,
    terminal_seen: bool,
    terminal_failure: Option<&'static str>,
    sequence: u64,
    held: std::collections::BTreeSet<u16>,
    presented: Option<(DrawnScope, Instant)>,
    ready: Option<DrawnScope>,
    consumed: Option<DrawnScope>,
    left_down: bool,
    click: Option<(DrawnScope, i16, i16, Instant)>,
}
impl ClientControl {
    pub(super) fn resumable_input_active(&self) -> bool {
        self.resumable.as_ref().is_none_or(|session| {
            session.phase() == crate::remote::resumable_control::Phase::Active
                && !self.resume_local_validation_pending
        })
    }
    /// Only the main viewer thread may complete the local fence after an ACK.
    /// The receiver thread cannot authorize a previously polled input batch.
    pub(super) fn active_input_generation(&self) -> Option<u64> {
        self.resumable.as_ref().and_then(|session| {
            (self.resumable_input_active() && !self.ended).then(|| session.generation())
        })
    }
    pub(super) fn resumable_local_validation_pending(&self) -> bool {
        self.resume_local_validation_pending && !self.ended
    }
    #[cfg(any(feature = "remote-wayland", test))]
    pub(super) fn complete_resumable_local_validation(&mut self, now: Instant) -> RemoteResult<()> {
        if !self.resume_local_validation_pending
            || self.ended
            || !self.held.is_empty()
            || self.left_down
            || self.click.is_some()
            || self.resumable.as_ref().is_none_or(|session| {
                session.phase() != crate::remote::resumable_control::Phase::Active
            })
        {
            self.revoke();
            return Err(
                invalid_data("resumable local validation lacks clean pending state").into(),
            );
        }
        if let Err(error) = self.resumable.as_mut().unwrap().check(now) {
            self.revoke();
            return Err(error.into());
        }
        self.resume_local_validation_pending = false;
        Ok(())
    }
    pub(super) fn receive_resumable_ack(
        &mut self,
        record: crate::remote::resumable_control::Record,
        now: Instant,
    ) -> RemoteResult<()> {
        let session = self
            .resumable
            .as_mut()
            .ok_or_else(|| invalid_data("unnegotiated resumable control response"))?;
        session.receive_ack(record, now)?;
        self.resume_local_validation_pending =
            record.kind == crate::remote::resumable_control::Kind::Resumed;
        self.sequence = record.sequence;
        self.held.clear();
        // Native pause released every delivered input. A local mouse-up may
        // have been intentionally discarded while paused; do not retain its
        // old logical click/drag candidate after acknowledging that cleanup.
        // The viewer's physical held state remains untouched and independently
        // gates local resume validation.
        self.left_down = false;
        self.ready = None;
        self.click = None;
        Ok(())
    }
    #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
    pub(super) fn resumable_resume_proof_ready(&self) -> bool {
        self.resumable.as_ref().is_some_and(|session| {
            session.phase() == crate::remote::resumable_control::Phase::Paused
        }) && self.presented.is_some_and(|(scope, at)| {
            self.ready == Some(scope)
                && self.latest_decoded.is_none_or(|decoded| decoded == scope)
                && at.elapsed() < super::super::wayland_pointer_wire::FRAME_MAX_AGE
        })
    }
    #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
    pub(super) fn resumable_test_transition(
        &mut self,
        resume: bool,
        now: Instant,
    ) -> RemoteResult<crate::remote::resumable_control::Record> {
        let record = if resume {
            let (scope, at) = self
                .presented
                .ok_or_else(|| invalid_data("resume without presentation"))?;
            if self.ready != Some(scope)
                || at.elapsed() >= super::super::wayland_pointer_wire::FRAME_MAX_AGE
                || self.latest_decoded.is_some_and(|decoded| decoded != scope)
            {
                return Err(invalid_data("resume without fresh current presentation").into());
            }
            self.resumable
                .as_mut()
                .ok_or_else(|| invalid_data("resume before grant"))?
                .begin_resume(scope.frame_sequence, now, |_| Ok(()))?
        } else {
            self.resumable
                .as_mut()
                .ok_or_else(|| invalid_data("pause before grant"))?
                .begin_pause(now)?
        };
        self.sequence = record.sequence;
        self.resume_local_validation_pending = false;
        self.ready = None;
        self.click = None;
        Ok(record)
    }
    pub(super) fn configured(&self) -> bool {
        self.digest.is_some()
    }
    pub(super) fn needs_request(&self) -> bool {
        self.configured() && !self.ended && self.request.is_none()
    }
    pub(super) fn check(&mut self, scope: (u64, u64), now: Instant) -> RemoteResult<()> {
        if let Some(reason) = self.terminal_failure {
            return Err(invalid_data(reason).into());
        }
        if self.ended {
            return Ok(());
        }
        if let Some(session) = self.resumable.as_mut() {
            session.check(now)?;
        }
        self.repair_check(now)?;
        if let Some((request, at)) = self.request {
            if (request.scope.source_epoch, request.scope.geometry_epoch) != scope
                || now.saturating_duration_since(at)
                    >= if self.epoch.is_some() {
                        LEASE_DURATION
                    } else {
                        Duration::from_secs(5)
                    }
            {
                return Err(invalid_data("control scope or deadline invalid").into());
            }
        }
        Ok(())
    }
    pub(super) fn before_frame(&mut self) {
        self.ready = None;
        self.click = None;
    }
    pub(super) fn presented(&mut self, scope: DrawnScope, now: Instant) -> Option<ControlRequest> {
        if !self.configured() || self.ended {
            return None;
        }
        if !self.receipts.iter().any(|(seen, _)| *seen == scope) {
            if self.receipts.len() == 256 {
                self.receipts.pop_front();
            }
            self.receipts.push_back((scope, false));
        }
        self.presented = Some((scope, now));
        self.ready = None;
        self.click = None;
        // Resolution uses this actual receipt; the reader may already have
        // decoded a newer frame, which must keep the pointer gate invalid.
        if self.repair_resolve(now).is_err() {
            self.revoke();
        }
        if self.request.is_none() {
            let request = ControlRequest {
                scope,
                keymap_digest: self.digest.unwrap(),
            };
            self.request = Some((request, now));
            #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
            repair::observe_lifetime(now);
            Some(request)
        } else {
            None
        }
    }
    pub(super) fn receive(&mut self, status: ControlStatus) -> RemoteResult<()> {
        let (request, _) = self
            .request
            .ok_or_else(|| invalid_data("unsolicited control status"))?;
        if self.ended
            && !self.terminal_seen
            && status.status == ControlStatusKind::Revoked
            && self.epoch == Some(status.lease_epoch)
            && (status.scope.source_epoch, status.scope.geometry_epoch)
                == (request.scope.source_epoch, request.scope.geometry_epoch)
        {
            self.terminal_seen = true;
            self.repair_end();
            return Ok(());
        }
        if self.ended
            || (status.scope.source_epoch, status.scope.geometry_epoch)
                != (request.scope.source_epoch, request.scope.geometry_epoch)
        {
            return Err(invalid_data("control status revived or changed source").into());
        }
        match status.status {
            ControlStatusKind::Granted => {
                if self.epoch.is_some() || status.scope != request.scope {
                    return Err(invalid_data("control grant replay").into());
                }
                self.check(
                    (status.scope.source_epoch, status.scope.geometry_epoch),
                    Instant::now(),
                )?;
                self.epoch = Some(status.lease_epoch);
                if self.resumable_enabled {
                    use crate::remote::resumable_control::{
                        Binding, ClientSession, LocalOptIn, Negotiated, VERSION,
                    };
                    let (_, requested_at) = self.request.unwrap();
                    self.resumable = Some(ClientSession::after_native_arm(
                        Negotiated::new(LocalOptIn::Explicit, Some(VERSION))?,
                        Binding {
                            session: status.lease_epoch,
                            source_epoch: request.scope.source_epoch,
                            geometry_epoch: request.scope.geometry_epoch,
                        },
                        Instant::now(),
                        requested_at + LEASE_DURATION,
                        request.scope.frame_sequence,
                    )?);
                }
                eprintln!(
                    "jwm-remote: control host approved; waiting for local checks, do not type or click yet"
                );
            }
            ControlStatusKind::Denied => {
                if self.epoch.is_some() {
                    return Err(invalid_data("control denial after grant").into());
                }
                self.ended = true;
            }
            ControlStatusKind::Revoked => {
                if self.epoch != Some(status.lease_epoch) {
                    return Err(invalid_data("control revoke epoch mismatch").into());
                }
                // Freeze the cause at receipt time, before clearing phase deadlines.
                // An early peer revoke must never become TTL merely by waiting.
                self.record_host_terminal(Instant::now());
                self.ended = true;
            }
            ControlStatusKind::Waiting | ControlStatusKind::PointerReady => {
                if self.epoch != Some(status.lease_epoch) {
                    return Err(invalid_data("control status epoch mismatch").into());
                }
                if status.status == ControlStatusKind::Waiting {
                    self.ready = None;
                    self.click = None;
                } else {
                    if self.repair_ordinary_ready(status.scope, Instant::now())? {
                        return Ok(());
                    }
                    let (scope, at) = self
                        .presented
                        .ok_or_else(|| invalid_data("control Ready before presentation"))?;
                    if status.scope.frame_sequence > scope.frame_sequence {
                        return Err(invalid_data("control Ready from future frame").into());
                    }
                    let receipt = self
                        .receipts
                        .iter_mut()
                        .find(|(seen, _)| *seen == status.scope)
                        .ok_or_else(|| {
                            invalid_data("control Ready lacks exact actual presentation")
                        })?;
                    if receipt.1 {
                        return Err(invalid_data("control Ready replay").into());
                    }
                    receipt.1 = true;
                    if status.scope == scope {
                        if self.ready == Some(scope) || self.consumed == Some(scope) {
                            return Err(invalid_data("control Ready replay").into());
                        }
                        if self.held.is_empty()
                            && self.latest_decoded.is_none_or(|decoded| decoded == scope)
                            && at.elapsed() < super::super::wayland_pointer_wire::FRAME_MAX_AGE
                        {
                            self.ready = Some(scope);
                        }
                    }
                }
            }
        }
        if self.ended {
            self.resume_local_validation_pending = false;
            if let Some(session) = self.resumable.as_mut() {
                session.revoke();
            }
            self.terminal_seen = true;
            self.repair_end();
            self.ready = None;
            self.click = None;
            self.held.clear();
        }
        Ok(())
    }
    pub(super) fn ready_sequence(&self) -> Option<u64> {
        if !self.activated
            || self.ended
            || !self.held.is_empty()
            || self.repair_blocked()
            || !self.resumable_input_active()
        {
            return None;
        }
        self.presented
            .filter(|(scope, at)| {
                Some(*scope) == self.ready
                    && self.latest_decoded.is_none_or(|decoded| decoded == *scope)
                    && at.elapsed() < super::super::wayland_pointer_wire::FRAME_MAX_AGE
            })
            .map(|(s, _)| s.frame_sequence)
    }
    pub(super) fn revoke(&mut self) -> Option<ControlRevoke> {
        self.resume_local_validation_pending = false;
        if let Some(session) = self.resumable.as_mut() {
            session.revoke();
        }
        if self.ended {
            return None;
        }
        self.ended = true;
        self.repair_end();
        self.ready = None;
        self.click = None;
        self.held.clear();
        self.epoch.map(|lease_epoch| ControlRevoke { lease_epoch })
    }
    pub(super) fn event(
        &mut self,
        event: &ViewerEvent,
        now: Instant,
    ) -> RemoteResult<Option<ControlAction>> {
        use super::super::x11_viewer::WaylandPointerEventKind as P;
        self.repair_event(event, now)?;
        if self.ended {
            return Ok(None);
        }
        if !self.resumable_input_active() {
            return Err(invalid_data("control input precedes resumable local validation").into());
        }
        let kind;
        let scope;
        match *event {
            ViewerEvent::Key { keycode, pressed } => {
                if !self.activated || self.epoch.is_none() || self.left_down || self.click.is_some()
                {
                    return Err(invalid_data(
                        "control key before activation or during pointer gesture",
                    )
                    .into());
                }
                let code = keycode
                    .checked_sub(8)
                    .filter(|v| *v > 0)
                    .ok_or_else(|| invalid_data("invalid control keycode"))?;
                #[cfg(feature = "remote-wayland")]
                if !crate::remote_keyboard_protocol::keyboard_key_allowed(u32::from(code)) {
                    return Err(invalid_data("control key not allowed").into());
                }
                let code = u16::from(code);
                if (pressed && (self.held.len() >= 32 || !self.held.insert(code)))
                    || (!pressed && !self.held.remove(&code))
                {
                    return Err(invalid_data("unbalanced control key").into());
                }
                kind = ControlActionKind::Key { code, pressed };
                scope = self.request.unwrap().0.scope;
                self.ready = None;
                self.click = None;
                if let Some((s, _)) = self.presented {
                    self.consumed = Some(s);
                }
            }
            ViewerEvent::WaylandPointerButtonCancel { pressed } => {
                // False cancellation can be synthesized by a new mapping, not physical input.
                if pressed && !self.held.is_empty() {
                    return Err(invalid_data("control pointer conflicts with held keys").into());
                }
                self.left_down = pressed;
                self.click = None;
                return Ok(None);
            }
            ViewerEvent::WaylandPointerCancel => {
                self.click = None;
                return Ok(None);
            }
            ViewerEvent::WaylandPointer {
                frame_sequence,
                geometry_epoch,
                x,
                y,
                local_x,
                local_y,
                kind: pk,
            } => {
                if !self.held.is_empty() {
                    return Err(invalid_data("control pointer conflicts with held keys").into());
                }
                if matches!(
                    pk,
                    P::ScrollUp | P::ScrollDown | P::ScrollLeft | P::ScrollRight
                ) && (!(if matches!(pk, P::ScrollLeft | P::ScrollRight) {
                    self.horizontal_enabled
                } else {
                    self.scroll_enabled
                }) || self.left_down
                    || self.click.is_some())
                {
                    return Err(invalid_data("control does not authorize scroll").into());
                }
                if pk == P::LeftDown {
                    self.left_down = true;
                    self.click = None;
                } else if pk == P::LeftUp {
                    self.left_down = false;
                }
                if self.ready_sequence() != Some(frame_sequence)
                    || self
                        .ready
                        .is_none_or(|s| s.geometry_epoch != geometry_epoch)
                {
                    self.click = None;
                    return Ok(None);
                }
                scope = self.ready.unwrap();
                match pk {
                    P::LeftDown => {
                        self.click = Some((scope, local_x, local_y, now));
                        return Ok(None);
                    }
                    P::LeftUp => {
                        let Some((old, cx, cy, at)) = self.click.take() else {
                            return Ok(None);
                        };
                        if old != scope
                            || local_x.abs_diff(cx) > 3
                            || local_y.abs_diff(cy) > 3
                            || now.saturating_duration_since(at) > Duration::from_millis(250)
                        {
                            return Ok(None);
                        }
                        kind = ControlActionKind::ClickLeft { x, y };
                    }
                    P::Motion if self.left_down => {
                        self.click = None;
                        return Ok(None);
                    }
                    P::Motion => kind = ControlActionKind::Motion { x, y },
                    P::ScrollUp => kind = ControlActionKind::ScrollUp { x, y },
                    P::ScrollDown => kind = ControlActionKind::ScrollDown { x, y },
                    P::ScrollLeft => kind = ControlActionKind::ScrollLeft { x, y },
                    P::ScrollRight => kind = ControlActionKind::ScrollRight { x, y },
                }
                self.ready = None;
                self.click = None;
                self.consumed = Some(scope);
            }
            _ => return Ok(None),
        }
        self.sequence = self
            .sequence
            .checked_add(1)
            .filter(|v| *v <= MAX_SEQUENCE)
            .ok_or_else(|| invalid_data("control sequence exhausted"))?;
        Ok(Some(ControlAction {
            scope,
            lease_epoch: self.epoch.unwrap(),
            sequence: self.sequence,
            kind,
        }))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn granted() -> ClientControl {
        let mut c = ClientControl {
            digest: Some([3; 32]),
            ..Default::default()
        };
        let scope = DrawnScope {
            source_epoch: 1,
            geometry_epoch: 2,
            frame_sequence: 0,
        };
        c.presented(scope, Instant::now()).unwrap();
        c.receive(ControlStatus {
            scope,
            lease_epoch: 7,
            status: ControlStatusKind::Granted,
        })
        .unwrap(); /* caller's independent local check */
        c.activated = true;
        c
    }
    fn ready(c: &mut ClientControl, frame: u64) {
        let scope = DrawnScope {
            source_epoch: 1,
            geometry_epoch: 2,
            frame_sequence: frame,
        };
        c.presented(scope, Instant::now());
        c.receive(ControlStatus {
            scope,
            lease_epoch: 7,
            status: ControlStatusKind::PointerReady,
        })
        .unwrap();
    }
    fn motion(frame: u64) -> ViewerEvent {
        ViewerEvent::WaylandPointer {
            frame_sequence: frame,
            geometry_epoch: 2,
            x: 10,
            y: 10,
            local_x: 10,
            local_y: 10,
            kind: super::super::super::x11_viewer::WaylandPointerEventKind::Motion,
        }
    }
    #[test]
    fn unified_sequence_and_key_edges_invalidate_old_pointer_ticket() {
        let mut c = granted();
        ready(&mut c, 1);
        assert_eq!(
            c.event(&motion(1), Instant::now())
                .unwrap()
                .unwrap()
                .sequence,
            1
        );
        assert_eq!(
            c.event(
                &ViewerEvent::Key {
                    keycode: 38,
                    pressed: true
                },
                Instant::now()
            )
            .unwrap()
            .unwrap()
            .sequence,
            2
        );
        assert!(c.event(&motion(1), Instant::now()).is_err());
        assert_eq!(
            c.event(
                &ViewerEvent::Key {
                    keycode: 38,
                    pressed: false
                },
                Instant::now()
            )
            .unwrap()
            .unwrap()
            .sequence,
            3
        );
        assert!(c.ready_sequence().is_none());
        assert!(c.event(&motion(1), Instant::now()).unwrap().is_none());
        ready(&mut c, 2);
        assert_eq!(
            c.event(&motion(2), Instant::now())
                .unwrap()
                .unwrap()
                .sequence,
            4
        );
    }
    #[test]
    fn held_key_survives_mapping_cancel_but_not_real_pointer_input() {
        let mut c = granted();
        c.event(
            &ViewerEvent::Key {
                keycode: 50,
                pressed: true,
            },
            Instant::now(),
        )
        .unwrap()
        .unwrap();
        c.before_frame();
        assert!(
            c.event(
                &ViewerEvent::WaylandPointerButtonCancel { pressed: false },
                Instant::now()
            )
            .unwrap()
            .is_none()
        );
        assert!(!c.ended);
        assert!(c.held.contains(&42));
        assert!(
            c.event(
                &ViewerEvent::WaylandPointerButtonCancel { pressed: true },
                Instant::now()
            )
            .is_err()
        );
        assert!(c.event(&motion(1), Instant::now()).is_err());
        assert!(
            c.event(
                &ViewerEvent::Key {
                    keycode: 50,
                    pressed: false
                },
                Instant::now()
            )
            .unwrap()
            .is_some()
        );
    }
    #[test]
    fn preactivation_keys_and_pointer_gesture_keys_are_rejected() {
        let mut c = granted();
        c.activated = false;
        assert!(
            c.event(
                &ViewerEvent::Key {
                    keycode: 38,
                    pressed: true
                },
                Instant::now()
            )
            .is_err()
        );
        c.activated = true;
        ready(&mut c, 1);
        let mut down = motion(1);
        if let ViewerEvent::WaylandPointer { kind, .. } = &mut down {
            *kind = super::super::super::x11_viewer::WaylandPointerEventKind::LeftDown;
        }
        assert!(c.event(&down, Instant::now()).unwrap().is_none());
        assert!(
            c.event(
                &ViewerEvent::Key {
                    keycode: 38,
                    pressed: true
                },
                Instant::now()
            )
            .is_err()
        );
        c.revoke();
        assert!(c.ready_sequence().is_none());
        assert!(!c.needs_request());
    }
    #[test]
    fn horizontal_control_requires_separate_permission_and_consumes_ready() {
        use super::super::super::x11_viewer::WaylandPointerEventKind as P;
        for direction in [P::ScrollLeft, P::ScrollRight] {
            let mut c = granted();
            ready(&mut c, 1);
            c.scroll_enabled = true;
            let mut event = motion(1);
            if let ViewerEvent::WaylandPointer { kind, .. } = &mut event {
                *kind = direction;
            }
            assert!(c.event(&event, Instant::now()).is_err());
            c.scroll_enabled = false;
            c.horizontal_enabled = true;
            let action = c.event(&event, Instant::now()).unwrap().unwrap();
            assert!(matches!(
                action.kind,
                ControlActionKind::ScrollLeft { .. } | ControlActionKind::ScrollRight { .. }
            ));
            assert!(c.event(&event, Instant::now()).unwrap().is_none());
        }
    }
    #[test]
    fn scroll_needs_consent_and_each_notch_consumes_a_fresh_ticket() {
        use super::super::super::x11_viewer::WaylandPointerEventKind as P;
        fn wheel(frame: u64, kind: P) -> ViewerEvent {
            let mut e = motion(frame);
            if let ViewerEvent::WaylandPointer { kind: k, .. } = &mut e {
                *k = kind;
            }
            e
        }
        let mut c = granted();
        ready(&mut c, 1);
        assert!(c.event(&wheel(1, P::ScrollUp), Instant::now()).is_err());
        c.scroll_enabled = true;
        let a = c
            .event(&wheel(1, P::ScrollUp), Instant::now())
            .unwrap()
            .unwrap();
        assert_eq!(a.kind, ControlActionKind::ScrollUp { x: 10, y: 10 });
        assert_eq!(a.sequence, 1);
        for _ in 0..3 {
            assert!(
                c.event(&wheel(1, P::ScrollDown), Instant::now())
                    .unwrap()
                    .is_none()
            );
        }
        ready(&mut c, 2);
        assert_eq!(c.sequence, 1); // Waiting notches were not queued or replayed.
        assert!(
            c.event(&wheel(1, P::ScrollDown), Instant::now())
                .unwrap()
                .is_none()
        );
        let a = c
            .event(&wheel(2, P::ScrollDown), Instant::now())
            .unwrap()
            .unwrap();
        assert_eq!(a.sequence, 2);
        assert_eq!(a.kind, ControlActionKind::ScrollDown { x: 10, y: 10 });
        ready(&mut c, 3);
        c.held.insert(42);
        assert!(c.event(&wheel(3, P::ScrollUp), Instant::now()).is_err());
        c.held.clear();
        c.left_down = true;
        assert!(c.event(&wheel(3, P::ScrollUp), Instant::now()).is_err());
    }
    fn resumable_granted() -> ClientControl {
        let mut c = ClientControl {
            digest: Some([3; 32]),
            resumable_enabled: true,
            ..Default::default()
        };
        let scope = DrawnScope {
            source_epoch: 1,
            geometry_epoch: 2,
            frame_sequence: 1,
        };
        c.presented(scope, Instant::now()).unwrap();
        c.receive(ControlStatus {
            scope,
            lease_epoch: 7,
            status: ControlStatusKind::Granted,
        })
        .unwrap();
        c.activated = true;
        c
    }
    fn pause_ack(c: &mut ClientControl) {
        use crate::remote::resumable_control::Kind;
        let pause = c
            .resumable
            .as_mut()
            .unwrap()
            .begin_pause(Instant::now())
            .unwrap();
        c.receive_resumable_ack(
            crate::remote::resumable_control::Record {
                kind: Kind::Paused,
                ..pause
            },
            Instant::now(),
        )
        .unwrap();
    }
    fn resume_ack(c: &mut ClientControl) {
        use crate::remote::resumable_control::Kind;
        let resume = c
            .resumable
            .as_mut()
            .unwrap()
            .begin_resume(3, Instant::now(), |_| Ok(()))
            .unwrap();
        c.receive_resumable_ack(
            crate::remote::resumable_control::Record {
                kind: Kind::Resumed,
                ..resume
            },
            Instant::now(),
        )
        .unwrap();
    }
    #[test]
    fn resumed_ack_keeps_local_gate_closed_until_fresh_validation() {
        let mut c = resumable_granted();
        assert_eq!(c.active_input_generation(), Some(1));
        pause_ack(&mut c);
        assert_eq!(c.active_input_generation(), None);
        resume_ack(&mut c);
        assert!(c.resumable_local_validation_pending());
        assert!(!c.resumable_input_active());
        assert_eq!(c.active_input_generation(), None);
        let key = ViewerEvent::Key {
            keycode: 38,
            pressed: true,
        };
        assert!(c.event(&key, Instant::now()).is_err());
        assert!(c.held.is_empty(), "closed gate must not consume old batch");
        c.complete_resumable_local_validation(Instant::now())
            .unwrap();
        assert_eq!(c.active_input_generation(), Some(2));
        assert!(!c.resumable_local_validation_pending());
        assert!(c.event(&key, Instant::now()).unwrap().is_some());
    }
    #[test]
    fn paused_mouse_up_discard_does_not_keep_old_logical_drag_on_resume() {
        use super::super::super::x11_viewer::WaylandPointerEventKind as P;
        let mut c = resumable_granted();
        ready(&mut c, 2);
        let mut down = motion(2);
        if let ViewerEvent::WaylandPointer { kind, .. } = &mut down {
            *kind = P::LeftDown;
        }
        assert!(c.event(&down, Instant::now()).unwrap().is_none());
        assert!(c.left_down);
        assert!(c.click.is_some());
        pause_ack(&mut c);
        assert!(!c.left_down);
        assert!(c.click.is_none());
        // Main intentionally drops the physical mouse-up while paused. Its
        // actual viewer-state update remains required before local validation.
        resume_ack(&mut c);
        c.complete_resumable_local_validation(Instant::now())
            .unwrap();
        let action = c
            .event(
                &ViewerEvent::Key {
                    keycode: 38,
                    pressed: true,
                },
                Instant::now(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            action.kind,
            ControlActionKind::Key {
                code: 30,
                pressed: true
            }
        );
    }
    #[test]
    fn pending_local_validation_cannot_revive_expired_or_revoked_session() {
        for expired in [false, true] {
            let mut c = resumable_granted();
            pause_ack(&mut c);
            resume_ack(&mut c);
            let deadline = c.resumable.as_ref().unwrap().deadline();
            if !expired {
                c.revoke();
            }
            assert!(
                c.complete_resumable_local_validation(if expired {
                    deadline
                } else {
                    Instant::now()
                })
                .is_err()
            );
            assert!(!c.resumable_local_validation_pending());
            assert_eq!(c.active_input_generation(), None);
            assert!(c.ended);
        }
    }
}
