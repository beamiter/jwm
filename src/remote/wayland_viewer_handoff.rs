//! Explicit JWM-only local focus transaction. This never grants remote authority.
use super::*;
use crate::viewer_handoff_protocol::client::{
    jwm_remote_viewer_handoff_manager_v1::{self as manager, JwmRemoteViewerHandoffManagerV1},
    jwm_remote_viewer_handoff_session_v1::{self as session, JwmRemoteViewerHandoffSessionV1},
};
use wayland_client::Proxy;
const CLEANUP_FLUSH_TIMEOUT: Duration = Duration::from_millis(50);
const TRANSITION_TIMEOUT: Duration = Duration::from_secs(2);
#[derive(Default)]
pub(super) struct Handoff {
    pub(super) manager: Option<JwmRemoteViewerHandoffManagerV1>,
    pub(super) global: Option<u32>,
    pub(super) session: Option<JwmRemoteViewerHandoffSessionV1>,
    pub(super) enabled: bool,
    granted: bool,
    requested_at: Option<Instant>,
    requested_ttl_ms: u32,
    deadline: Option<Instant>,
    pending: Option<(u32, bool, bool, Instant)>,
    sequence: u32,
    active: bool,
    inside: bool,
    exit_required: bool,
    enter_serial: Option<u32>,
    consumed_serial: Option<u32>,
    pub(super) failure: Option<&'static str>,
    cleanup_result: Option<Result<(), String>>,
}
impl Handoff {
    fn cleanup(&mut self, flush: impl FnOnce() -> RemoteResult<()>) -> RemoteResult<()> {
        if let Some(result) = &self.cleanup_result {
            return result.clone().map_err(Into::into);
        }
        // Terminal even for a viewer that never enabled handoff. Its default
        // forwarding policy must not authorize input after cleanup begins.
        self.cleanup_result = Some(Ok(()));
        if !self.enabled {
            return Ok(());
        }
        // Terminal before any I/O: late ACKs and input cannot renew authority.
        self.fail("local handoff ended during session cleanup");
        // A previous fail may already have taken session, leaving its destroy
        // queued. Always flush even when no session proxy remains.
        let result = flush();
        self.cleanup_result = Some(result.as_ref().map(|_| ()).map_err(ToString::to_string));
        result
    }
    pub(super) fn forwarding(&self) -> bool {
        self.cleanup_result.is_none()
            && (!self.enabled
                || (self.active
                    && !self.exit_required
                    && self.inside
                    && self.pending.is_none()
                    && self.failure.is_none()))
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
        if !self.enabled || self.cleanup_result.is_some() {
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
/// No event dispatch or state.check: either can reject the already-terminal
/// handoff before its queued destroy reaches the compositor. Both flush and
/// writable waits share one deadline, including interruptions.
fn flush_teardown(
    mut flush: impl FnMut() -> io::Result<()>,
    mut wait_writable: impl FnMut(Duration) -> io::Result<()>,
    shutdown: impl FnOnce() -> io::Result<()>,
    mut now: impl FnMut() -> Instant,
) -> RemoteResult<()> {
    let deadline = now() + CLEANUP_FLUSH_TIMEOUT;
    let result = loop {
        let remaining = deadline.saturating_duration_since(now());
        if remaining.is_zero() {
            break Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "local handoff destroy flush expired",
            ));
        }
        match flush() {
            Ok(()) => break Ok(()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                let remaining = deadline.saturating_duration_since(now());
                if remaining.is_zero() {
                    continue;
                }
                match wait_writable(remaining) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => break Err(error),
                }
            }
            Err(error) => break Err(error),
        }
    };
    if let Err(error) = result {
        // The connection is private to this terminating viewer. EOF lets the
        // compositor revoke even when a destroy cannot fit in its socket.
        return match shutdown() {
            Ok(()) => Err(error.into()),
            Err(shutdown_error) => Err(format!(
                "local handoff teardown failed: {error}; connection shutdown failed: {shutdown_error}"
            ).into()),
        };
    }
    Ok(())
}

impl WaylandViewer {
    pub(in crate::remote) fn begin_local_cleanup(&mut self) -> RemoteResult<()> {
        let backend = self.conn.backend();
        self.state.handoff.cleanup(|| {
            flush_teardown(
                || {
                    backend.flush().map_err(|error| match error {
                        WaylandError::Io(error) => error,
                        error => io::Error::other(error),
                    })
                },
                |remaining| {
                    let mut fds = [PollFd::new(backend.poll_fd(), PollFlags::POLLOUT)];
                    let timeout = PollTimeout::try_from(remaining).map_err(io::Error::other)?;
                    poll(&mut fds, timeout).map_err(io::Error::from)?;
                    if fds[0]
                        .revents()
                        .unwrap_or(PollFlags::empty())
                        .intersects(PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL)
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "local Wayland connection closed",
                        ));
                    }
                    Ok(())
                },
                || {
                    use std::os::fd::AsRawFd;
                    // SAFETY: poll_fd is the live socket of this viewer's own
                    // backend. Shutdown changes neither fd ownership nor its
                    // lifetime; Connection/its proxies still close it normally.
                    if unsafe { libc::shutdown(backend.poll_fd().as_raw_fd(), libc::SHUT_RDWR) }
                        == 0
                    {
                        Ok(())
                    } else {
                        let error = io::Error::last_os_error();
                        if error.kind() == io::ErrorKind::NotConnected {
                            Ok(())
                        } else {
                            Err(error)
                        }
                    }
                },
                Instant::now,
            )
        })
    }
    pub(in crate::remote) fn enable_wayland_seamless_input(
        &mut self,
        deadline: Instant,
        remaining_ms: u32,
        extended: bool,
    ) -> RemoteResult<()> {
        if !self.state.control_mode
            || self.state.handoff.enabled
            || self.state.handoff.cleanup_result.is_some()
        {
            return Err("native seamless input requires fresh combined control".into());
        }
        let manager = self
            .state
            .handoff
            .manager
            .as_ref()
            .ok_or("JWM local handoff protocol v2 required; update and restart JWM (udev backend), with no fallback")?;
        if remaining_ms == 0 || remaining_ms > if extended { 300_000 } else { 30_000 } {
            return Err("local handoff remaining budget is invalid".into());
        }
        if extended && manager.version() < 3 {
            return Err(
                "extended handoff requires local protocol v3; update and restart JWM".into(),
            );
        }
        let seat = self
            .state
            .keyboard
            .seat
            .as_ref()
            .ok_or("local handoff seat unavailable")?;
        // The caller's budget began before ClientHello, not at this grant.
        // ACKs can only shorten it; delayed native creation cannot renew it.
        let started = Instant::now();
        let remaining_ms = remaining_ms.min(
            deadline
                .saturating_duration_since(started)
                .as_millis()
                .min(u128::from(u32::MAX)) as u32,
        );
        if remaining_ms == 0 {
            return Err("local handoff original budget expired".into());
        }
        let native_deadline = started + Duration::from_millis(u64::from(remaining_ms));
        self.state.handoff.deadline = Some(deadline.min(native_deadline));
        self.state.handoff.requested_at = Some(started);
        self.state.handoff.requested_ttl_ms = remaining_ms;
        self.state.handoff.enabled = true;
        self.state.handoff.session = Some(if manager.version() >= 3 {
            manager.create_timed(&self.surface, seat, remaining_ms, &self.queue.handle(), ())
        } else {
            manager.create(&self.surface, seat, remaining_ms, &self.queue.handle(), ())
        });
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
        if self.state.closed {
            return Ok(false);
        }
        let inside = self.state.pointer.inside_image();
        self.state.handoff.drive(may_enter, inside);
        self.state.handoff.check()?;
        Ok(self.state.handoff.forwarding())
    }
    pub(in crate::remote) fn wayland_handoff_active(&self) -> bool {
        !self.state.closed && self.state.handoff.forwarding()
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
                if !h.enabled
                    || h.granted
                    || ttl_ms == 0
                    || ttl_ms > h.requested_ttl_ms
                    || h.requested_at.is_none()
                {
                    h.fail("invalid local handoff grant");
                    return;
                }
                h.granted = true;
                h.deadline = h.deadline.map(|deadline| {
                    deadline.min(h.requested_at.unwrap() + Duration::from_millis(u64::from(ttl_ms)))
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
    fn terminal_cleanup_is_idempotent_and_never_reauthorizes_input() {
        for enabled in [false, true] {
            let mut h = Handoff {
                enabled,
                active: enabled,
                granted: enabled,
                inside: true,
                ..Default::default()
            };
            let mut flushes = 0;
            h.cleanup(|| {
                flushes += 1;
                Ok(())
            })
            .unwrap();
            h.cleanup(|| panic!("cleanup must not repeat I/O")).unwrap();
            h.pointer_enter(9, true);
            h.drive(true, true);
            assert!(!h.forwarding());
            assert!(h.pending.is_none());
            assert_eq!(flushes, usize::from(enabled));
        }
    }

    #[test]
    fn terminal_cleanup_flushes_after_prior_failure_and_preserves_error() {
        let mut h = Handoff {
            enabled: true,
            ..Default::default()
        };
        h.fail("previous failure already queued destroy");
        assert!(h.session.is_none());
        let error = h.cleanup(|| Err("flush failure".into())).unwrap_err();
        assert_eq!(error.to_string(), "flush failure");
        assert_eq!(
            h.cleanup(|| panic!("failed cleanup must not retry I/O"))
                .unwrap_err()
                .to_string(),
            "flush failure"
        );
        assert_eq!(h.failure, Some("previous failure already queued destroy"));
        h.drive(true, true);
        assert!(!h.forwarding());
    }

    #[test]
    fn teardown_flush_retry_shares_deadline_and_success_does_not_shutdown() {
        use std::cell::Cell;
        let start = Instant::now();
        let clock = Cell::new(start);
        let mut flushes = 0;
        let mut waits = Vec::new();
        flush_teardown(
            || {
                flushes += 1;
                if flushes < 3 {
                    Err(io::ErrorKind::WouldBlock.into())
                } else {
                    Ok(())
                }
            },
            |remaining| {
                waits.push(remaining);
                clock.set(clock.get() + Duration::from_millis(10));
                Err(io::ErrorKind::Interrupted.into())
            },
            || panic!("successful flush cannot disconnect viewer"),
            || clock.get(),
        )
        .unwrap();
        assert_eq!(flushes, 3);
        assert_eq!(
            waits,
            [
                CLEANUP_FLUSH_TIMEOUT,
                CLEANUP_FLUSH_TIMEOUT - Duration::from_millis(10)
            ]
        );
    }

    #[test]
    fn teardown_flush_timeout_and_errors_shutdown_once_and_remain_errors() {
        use std::cell::Cell;
        for interrupted_flush in [false, true] {
            let clock = Cell::new(Instant::now());
            let mut shutdowns = 0;
            let error = flush_teardown(
                || {
                    if interrupted_flush {
                        clock.set(clock.get() + Duration::from_millis(10));
                        Err(io::ErrorKind::Interrupted.into())
                    } else {
                        Err(io::ErrorKind::WouldBlock.into())
                    }
                },
                |remaining| {
                    clock.set(clock.get() + remaining);
                    Ok(())
                },
                || {
                    shutdowns += 1;
                    Ok(())
                },
                || clock.get(),
            )
            .unwrap_err();
            assert_eq!(shutdowns, 1);
            assert!(error.to_string().contains("expired"));
        }
        let error = flush_teardown(
            || Err(io::ErrorKind::BrokenPipe.into()),
            |_| panic!("hard flush failure must not wait"),
            || Err(io::Error::other("shutdown failed too")),
            Instant::now,
        )
        .unwrap_err();
        assert!(error.to_string().contains("shutdown failed too"));
        assert!(error.to_string().contains("teardown failed"));
    }

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
