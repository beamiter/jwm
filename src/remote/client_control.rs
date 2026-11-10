//! A single client bundle state, not two parallel legacy leases.
use super::*;
use crate::remote::wayland_control_wire::*;
#[derive(Default)]
pub(super) struct ClientControl {
    pub(super) scroll_enabled: bool,
    pub(super) horizontal_enabled: bool,
    pub(super) digest: Option<[u8; 32]>,
    request: Option<(ControlRequest, Instant)>,
    pub(super) epoch: Option<u64>,
    pub(super) activated: bool,
    pub(super) ended: bool,
    sequence: u64,
    held: std::collections::BTreeSet<u16>,
    presented: Option<(DrawnScope, Instant)>,
    ready: Option<DrawnScope>,
    consumed: Option<DrawnScope>,
    left_down: bool,
    click: Option<(DrawnScope, i16, i16, Instant)>,
}
impl ClientControl {
    pub(super) fn configured(&self) -> bool {
        self.digest.is_some()
    }
    pub(super) fn needs_request(&self) -> bool {
        self.configured() && !self.ended && self.request.is_none()
    }
    pub(super) fn check(&self, scope: (u64, u64), now: Instant) -> RemoteResult<()> {
        if self.ended {
            return Ok(());
        }
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
        self.presented = Some((scope, now));
        self.ready = None;
        self.click = None;
        if self.request.is_none() {
            let request = ControlRequest {
                scope,
                keymap_digest: self.digest.unwrap(),
            };
            self.request = Some((request, now));
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
                    let (scope, at) = self
                        .presented
                        .ok_or_else(|| invalid_data("control Ready before presentation"))?;
                    if status.scope.frame_sequence > scope.frame_sequence {
                        return Err(invalid_data("control Ready from future frame").into());
                    }
                    if status.scope == scope {
                        if self.ready == Some(scope) || self.consumed == Some(scope) {
                            return Err(invalid_data("control Ready replay").into());
                        }
                        if self.held.is_empty()
                            && at.elapsed() < super::super::wayland_pointer_wire::FRAME_MAX_AGE
                        {
                            self.ready = Some(scope);
                        }
                    }
                }
            }
        }
        if self.ended {
            self.ready = None;
            self.click = None;
            self.held.clear();
        }
        Ok(())
    }
    pub(super) fn ready_sequence(&self) -> Option<u64> {
        if !self.activated || self.ended || !self.held.is_empty() {
            return None;
        }
        self.presented
            .filter(|(scope, at)| {
                Some(*scope) == self.ready
                    && at.elapsed() < super::super::wayland_pointer_wire::FRAME_MAX_AGE
            })
            .map(|(s, _)| s.frame_sequence)
    }
    pub(super) fn revoke(&mut self) -> Option<ControlRevoke> {
        if self.ended {
            return None;
        }
        self.ended = true;
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
        if self.ended {
            return Ok(None);
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
}
