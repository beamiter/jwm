//! Explicit JWM-only local focus transaction. This never grants remote authority.
use super::*;
use crate::viewer_handoff_protocol::client::{
    jwm_remote_viewer_handoff_manager_v1::{self as manager, JwmRemoteViewerHandoffManagerV1},
    jwm_remote_viewer_handoff_session_v1::{self as session, JwmRemoteViewerHandoffSessionV1},
};
const TRANSITION_TIMEOUT: Duration = Duration::from_secs(2);
#[derive(Default)]
pub(super) struct Handoff {
    pub(super) manager: Option<JwmRemoteViewerHandoffManagerV1>,
    pub(super) global: Option<u32>,
    pub(super) session: Option<JwmRemoteViewerHandoffSessionV1>,
    pub(super) enabled: bool,
    granted: bool,
    deadline: Option<Instant>,
    pending: Option<(u32, bool, bool, Instant)>,
    sequence: u32,
    active: bool,
    inside: bool,
    exit_required: bool,
    enter_serial: Option<u32>,
    consumed_serial: Option<u32>,
    pub(super) failure: Option<&'static str>,
}
impl Handoff {
    pub(super) fn forwarding(&self) -> bool {
        !self.enabled
            || (self.active
                && !self.exit_required
                && self.inside
                && self.pending.is_none()
                && self.failure.is_none())
    }
    pub(super) fn expects_focus_exit(&self) -> bool {
        self.enabled
            && self
                .pending
                .is_some_and(|(_, entering, begun, _)| !entering && begun)
    }
    pub(super) fn expects_focus_enter(&self) -> bool {
        self.enabled
            && self
                .pending
                .is_some_and(|(_, entering, begun, _)| entering && begun)
    }
    pub(super) fn fail(&mut self, reason: &'static str) {
        self.failure.get_or_insert(reason);
        self.active = false;
        if let Some(session) = self.session.take() {
            session.destroy();
        }
    }
    pub(super) fn check(&mut self) -> RemoteResult<()> {
        if self.enabled {
            let now = Instant::now();
            if self.deadline.is_some_and(|deadline| now >= deadline)
                || self
                    .pending
                    .is_some_and(|(_, _, _, at)| now.duration_since(at) >= TRANSITION_TIMEOUT)
            {
                self.fail("local handoff deadline expired");
            }
        }
        if let Some(reason) = self.failure {
            return Err(reason.into());
        }
        Ok(())
    }
    pub(super) fn pointer_enter(&mut self, serial: u32, inside: bool) {
        if !self.enabled {
            self.enter_serial = Some(serial);
            self.inside = inside;
            return;
        }
        if self.enter_serial == Some(serial) {
            self.fail("local handoff pointer enter replay");
            return;
        }
        self.enter_serial = Some(serial);
        self.inside = inside;
    }
    pub(super) fn position(&mut self, inside: bool) {
        if self.enabled {
            self.inside = inside;
            if !inside && (self.active || self.pending.is_some_and(|(_, entering, _, _)| entering))
            {
                self.exit_required = true;
            }
            self.exit_if_needed();
        }
    }
    pub(super) fn leave(&mut self) {
        if self.enabled {
            self.inside = false;
            if self.active || self.pending.is_some_and(|(_, entering, _, _)| entering) {
                self.exit_required = true;
            }
            self.exit_if_needed();
        }
    }
    fn complete_transition(&mut self, sequence: u32, entering: bool) {
        if !self.pending.is_some_and(|(expected, direction, begun, _)| {
            expected == sequence && direction == entering && begun
        }) {
            self.fail("mismatched local handoff acknowledgement");
            return;
        }
        self.pending = None;
        self.active = entering;
        if !entering {
            self.exit_required = false;
        }
    }
    fn request(&mut self, entering: bool) {
        if self.pending.is_some() || self.failure.is_some() {
            return;
        }
        let Some(sequence) = self.sequence.checked_add(1).filter(|n| *n <= 60_000) else {
            self.fail("local handoff sequence exhausted");
            return;
        };
        let Some(session) = self.session.as_ref() else {
            self.fail("local handoff session missing");
            return;
        };
        if entering {
            let Some(serial) = self
                .enter_serial
                .filter(|serial| Some(*serial) != self.consumed_serial)
            else {
                return;
            };
            self.consumed_serial = Some(serial);
            session.enter(sequence, serial);
        } else {
            session.exit(sequence);
        }
        self.sequence = sequence;
        self.pending = Some((sequence, entering, false, Instant::now()));
    }
    fn exit_if_needed(&mut self) {
        if self.granted
            && self.active
            && (self.exit_required || !self.inside)
            && self.pending.is_none()
        {
            self.request(false);
        }
    }
    pub(super) fn drive(&mut self, may_enter: bool, inside: bool) {
        if !self.enabled {
            return;
        }
        self.position(inside);
        if may_enter && self.granted && !self.active && self.inside && self.pending.is_none() {
            self.request(true);
        }
    }
}
impl Drop for WaylandViewer {
    fn drop(&mut self) {
        #[cfg(all(test, feature = "backend-wayland-udev"))]
        self.state.popup_live.end();
        if let Some(session) = self.state.handoff.session.take() {
            session.destroy();
        }
    }
}
impl WaylandViewer {
    pub(in crate::remote) fn enable_wayland_seamless_input(&mut self) -> RemoteResult<()> {
        if !self.state.control_mode || self.state.handoff.enabled {
            return Err("native seamless input requires fresh combined control".into());
        }
        let manager = self
            .state
            .handoff
            .manager
            .as_ref()
            .ok_or("JWM local handoff protocol unavailable")?;
        let seat = self
            .state
            .keyboard
            .seat
            .as_ref()
            .ok_or("local handoff seat unavailable")?;
        // This starts before the first remote ControlRequest. Its immutable
        // deadline is therefore no later than the remote request's 30s expiry.
        self.state.handoff.deadline = Some(Instant::now() + Duration::from_secs(30));
        self.state.handoff.enabled = true;
        self.state.handoff.session =
            Some(manager.create(&self.surface, seat, 30_000, &self.queue.handle(), ()));
        while !self.state.handoff.granted {
            self.pump()?;
            self.state.handoff.check()?;
            if self.state.closed {
                return Err("local handoff viewer closed".into());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }
    pub(in crate::remote) fn drive_wayland_handoff(
        &mut self,
        may_enter: bool,
    ) -> RemoteResult<bool> {
        self.state.handoff.check()?;
        let inside = self.state.pointer.inside_image();
        self.state.handoff.drive(may_enter, inside);
        self.state.handoff.check()?;
        Ok(self.state.handoff.forwarding())
    }
    pub(in crate::remote) fn wayland_handoff_active(&self) -> bool {
        self.state.handoff.forwarding()
    }
}
impl Dispatch<JwmRemoteViewerHandoffManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &JwmRemoteViewerHandoffManagerV1,
        _: manager::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<JwmRemoteViewerHandoffSessionV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &JwmRemoteViewerHandoffSessionV1,
        event: session::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let h = &mut state.handoff;
        if h.failure.is_some() {
            return;
        }
        match event {
            session::Event::Granted { ttl_ms } => {
                if !h.enabled || h.granted || ttl_ms == 0 || ttl_ms > 30_000 {
                    h.fail("invalid local handoff grant");
                    return;
                }
                h.granted = true;
                h.deadline = h.deadline.map(|deadline| {
                    deadline.min(Instant::now() + Duration::from_millis(u64::from(ttl_ms)))
                });
            }
            session::Event::Transition { sequence, entering } => {
                let Some((expected, direction, begun, _)) = h.pending.as_mut() else {
                    h.fail("unsolicited local handoff transition");
                    return;
                };
                if *expected != sequence || entering > 1 || *direction != (entering == 1) || *begun
                {
                    h.fail("mismatched local handoff transition");
                    return;
                }
                *begun = true;
            }
            session::Event::Active { sequence } | session::Event::Inactive { sequence } => {
                let entering = matches!(event, session::Event::Active { .. });
                h.complete_transition(sequence, entering);
                if entering {
                    // The compositor Active ACK proves physically clean buttons
                    // for this fresh pointer-enter serial. An outside release
                    // may not have been delivered to this Wayland client.
                    state.pointer.confirm_clean_handoff_enter();
                    h.exit_if_needed();
                }
            }
            session::Event::Revoked { reason } => h.fail(match reason {
                1 => "local handoff revoked: permission denied",
                2 => "local handoff revoked: lifetime or surface invalid",
                3 => "local handoff revoked: unexpected focus change",
                4 => "local handoff revoked: emergency escape",
                _ => "local handoff revoked: invalid sequence or unclean input",
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pending_enter_exit_is_latched_across_reentry_and_late_active_ack() {
        let mut h = Handoff {
            enabled: true,
            granted: true,
            inside: true,
            enter_serial: Some(10),
            consumed_serial: Some(10),
            pending: Some((1, true, true, Instant::now())),
            ..Default::default()
        };
        h.leave();
        h.pointer_enter(11, true);
        assert!(h.exit_required);
        h.complete_transition(1, true);
        assert!(h.active);
        assert!(
            !h.forwarding(),
            "late ACK cannot authorize the replacement entry"
        );
        h.pending = Some((2, false, true, Instant::now()));
        h.complete_transition(2, false);
        assert!(!h.exit_required);
        assert!(!h.forwarding());
        assert_eq!(h.enter_serial, Some(11));
        assert_ne!(
            h.enter_serial, h.consumed_serial,
            "fresh reentry intent survives exit ACK"
        );
    }
    #[test]
    fn letterbox_exit_stays_latched_if_pointer_returns_before_active_ack() {
        let mut h = Handoff {
            enabled: true,
            granted: true,
            inside: true,
            pending: Some((1, true, true, Instant::now())),
            ..Default::default()
        };
        h.position(false);
        h.position(true);
        h.complete_transition(1, true);
        assert!(!h.forwarding());
    }
    #[test]
    fn opaque_pointer_serial_zero_is_not_missing_authority() {
        let mut h = Handoff {
            enabled: true,
            ..Default::default()
        };
        h.pointer_enter(0, true);
        assert_eq!(h.enter_serial, Some(0));
        assert!(h.failure.is_none());
        assert!(
            !h.forwarding(),
            "the native transaction still must verify the serial"
        );
        h.pointer_enter(0, true);
        assert!(h.failure.is_some(), "replaying the same serial still fails");
    }
}
