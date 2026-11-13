//! One native combined authority; neither old independent lease is enabled.
use super::*;
use crate::remote_control_protocol::client::{
    jwm_remote_control_lease_v1::{self as control_lease, JwmRemoteControlLeaseV1},
    jwm_remote_control_manager_v1::JwmRemoteControlManagerV1,
};
#[derive(Default)]
pub(super) struct NativeControl {
    pub(super) manager: Option<JwmRemoteControlManagerV1>,
    pub(super) global: Option<u32>,
    pub(super) enabled: bool,
    pub(super) revoked: bool,
    requested: bool,
    resumable: bool,
    paused: bool,
    generation: u32,
    transition: Option<(bool, u32, u32)>,
    digest: Option<[u8; 32]>,
    epoch: Option<u32>,
    armed: bool,
    arm_sent: bool,
    accepted: u32,
    expected: Option<u32>,
    deadline: Option<Instant>,
    scroll_enabled: bool,
    horizontal_enabled: bool,
}
impl NativeControl {
    fn accept_transition(&mut self, resume: bool, epoch: u32, sequence: u32, generation: u32) {
        if self.revoked
            || !self.resumable
            || !self.armed
            || self.paused != resume
            || self.epoch != Some(epoch)
            || self.transition != Some((resume, sequence, generation))
            || self.expected.is_some()
            || self.accepted.checked_add(1) != Some(sequence)
            || if resume {
                self.generation.checked_add(1) != Some(generation)
            } else {
                self.generation != generation
            }
        {
            self.revoked = true;
        } else {
            self.paused = !resume;
            self.generation = generation;
            self.accepted = sequence;
            self.transition = None;
        }
    }
}
impl WaylandCapture {
    pub(in crate::remote) fn enable_control(
        &mut self,
        scroll_enabled: bool,
        horizontal_enabled: bool,
    ) -> RemoteResult<()> {
        if self.state.interactive_popup.enabled
            || self.state.text.enabled
            || self.state.resize_follow
            || self.state.popups
            || self.state.frame_serial != 0
            || self.state.control.enabled
            || self.state.keyboard.enabled
            || self.state.pointer.enabled
            || self.state.control.revoked
            || self.state.control.manager.is_none()
        {
            return Err(invalid("native control manager unavailable or enable too late").into());
        }
        if scroll_enabled && self.state.control.manager.as_ref().unwrap().version() < 2 {
            return Err(invalid("control scroll requires native manager version 2").into());
        }
        if horizontal_enabled && self.state.control.manager.as_ref().unwrap().version() < 3 {
            return Err(
                invalid("control horizontal scroll requires native manager version 3").into(),
            );
        }
        self.state.control.horizontal_enabled = horizontal_enabled;
        self.state.control.scroll_enabled = scroll_enabled;
        self.state.control.enabled = true;
        Ok(())
    }

    /// Must be called after combined control is explicitly enabled and before
    /// any capture or native lease request. An old manager cannot downgrade it.
    pub(in crate::remote) fn enable_resumable_control(&mut self) -> RemoteResult<()> {
        if !self.state.control.enabled
            || self.state.control.resumable
            || self.state.control.requested
            || self.state.control.revoked
            || self.state.frame_serial != 0
            || self
                .state
                .control
                .manager
                .as_ref()
                .is_none_or(|m| m.version() < 4)
        {
            return Err(invalid("resumable control requires fresh native v4 opt-in").into());
        }
        self.state.control.resumable = true;
        Ok(())
    }

    pub(in crate::remote) fn control_revoked(&self) -> bool {
        self.state.control.enabled
            && (self.state.control.revoked
                || self
                    .arm_deadline
                    .is_some_and(|deadline| Instant::now() >= deadline)
                || self
                    .state
                    .control
                    .deadline
                    .is_some_and(|deadline| Instant::now() >= deadline))
    }

    pub(in crate::remote) fn revoke_control(&mut self) {
        self.state.control.revoked = true;
        if let Some(lease) = self.control_lease.take() {
            lease.destroy();
        }
        if let Some(frame) = self.retained_frame.take() {
            frame.destroy();
        }
        self.arm_deadline = None;
        let _ = self.conn.backend().flush();
    }

    pub(in crate::remote) fn expire_control(&mut self) {
        if self.control_revoked() {
            self.revoke_control();
        }
    }

    fn await_control(
        &mut self,
        deadline: Instant,
        done: impl Fn(&NativeControl) -> bool,
    ) -> RemoteResult<()> {
        loop {
            self.expire_control();
            self.state.check()?;
            if self.state.control.revoked {
                return Err(invalid("native control lease revoked").into());
            }
            if done(&self.state.control) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(invalid("native control response timed out").into());
            }
            self.pump(deadline)?;
        }
    }

    pub(in crate::remote) fn arm_control(&mut self, digest: [u8; 32]) -> RemoteResult<u64> {
        let result = self.arm_control_inner(digest);
        if result.is_err() {
            self.revoke_control();
        }
        result
    }
    fn arm_control_inner(&mut self, digest: [u8; 32]) -> RemoteResult<u64> {
        self.expire_control();
        if !self.state.control.enabled
            || self.state.control.revoked
            || self.state.control.requested
            || self.retained_frame.is_none()
            || digest == [0; 32]
        {
            return Err(invalid("native control arm not permitted").into());
        }
        self.state.control.requested = true;
        let deadline = (Instant::now() + PROTOCOL_TIMEOUT).min(self.arm_deadline.unwrap());
        let manager = self.state.control.manager.as_ref().unwrap();
        let lease = if self.state.control.resumable {
            manager.create_resumable_lease(
                self.source.as_ref().unwrap(),
                u32::from(self.state.control.scroll_enabled)
                    | (u32::from(self.state.control.horizontal_enabled) << 1),
                &self.queue.handle(),
                (),
            )
        } else if self.state.control.horizontal_enabled {
            manager.create_lease_with_scroll_axes(
                self.source.as_ref().unwrap(),
                2 | u32::from(self.state.control.scroll_enabled),
                &self.queue.handle(),
                (),
            )
        } else if self.state.control.scroll_enabled {
            manager.create_lease_with_scroll(
                self.source.as_ref().unwrap(),
                &self.queue.handle(),
                (),
            )
        } else {
            manager.create_lease(self.source.as_ref().unwrap(), &self.queue.handle(), ())
        };
        self.control_lease = Some(lease);
        self.await_control(deadline, |state| {
            state.digest.is_some() && state.epoch.is_some()
        })?;
        if self.state.control.digest != Some(digest) {
            return Err(invalid("native and viewer full keymaps differ").into());
        }
        let epoch = self.state.control.epoch.unwrap();
        self.state.control.arm_sent = true;
        self.control_lease
            .as_ref()
            .unwrap()
            .arm(epoch, self.retained_frame.as_ref().unwrap());
        self.await_control(deadline, |state| state.armed)?;
        self.discard_control_frame();
        self.arm_deadline = None;
        Ok(u64::from(epoch))
    }

    pub(in crate::remote) fn control_transition(
        &mut self,
        epoch: u64,
        sequence: u64,
        generation: u64,
        resume_serial: Option<u64>,
    ) -> RemoteResult<()> {
        let result = self.control_transition_inner(epoch, sequence, generation, resume_serial);
        if result.is_err() {
            self.revoke_control();
        }
        result
    }

    fn control_transition_inner(
        &mut self,
        epoch: u64,
        sequence: u64,
        generation: u64,
        resume_serial: Option<u64>,
    ) -> RemoteResult<()> {
        self.expire_control();
        self.sync_until(Instant::now() + PROTOCOL_TIMEOUT)?;
        let (epoch, sequence, generation) = (
            u32::try_from(epoch)?,
            u32::try_from(sequence)?,
            u32::try_from(generation)?,
        );
        let resume = resume_serial.is_some();
        let state = &self.state.control;
        if self.control_revoked()
            || !state.resumable
            || !state.armed
            || state.transition.is_some()
            || state.expected.is_some()
            || state.epoch != Some(epoch)
            || state.accepted.checked_add(1) != Some(sequence)
            || sequence == 0
            || sequence > 60000
            || state.paused != resume
            || if resume {
                state.generation.checked_add(1) != Some(generation)
            } else {
                state.generation != generation
            }
        {
            return Err(invalid("invalid native resumable transition").into());
        }
        if let Some(serial) = resume_serial {
            let (actual, at) = self
                .control_frame()
                .ok_or_else(|| invalid("resume requires fresh native proof"))?;
            if serial != actual || at.elapsed() >= super::super::wayland_pointer_wire::FRAME_MAX_AGE
            {
                return Err(invalid("stale resume proof").into());
            }
        }
        self.state.control.transition = Some((resume, sequence, generation));
        let lease = self
            .control_lease
            .as_ref()
            .ok_or_else(|| invalid("missing control lease"))?;
        if resume {
            lease.resume(
                epoch,
                sequence,
                generation,
                self.retained_frame.as_ref().unwrap(),
            );
        } else {
            lease.pause(epoch, sequence, generation);
        }
        let deadline =
            (Instant::now() + PROTOCOL_TIMEOUT).min(self.state.control.deadline.unwrap());
        // The event is sent by the native backend only after actual cleanup or
        // proof validation. Sending the request is not a release acknowledgement.
        self.await_control(deadline, |s| {
            s.transition.is_none() && s.accepted == sequence
        })?;
        self.discard_control_frame();
        Ok(())
    }

    pub(in crate::remote) fn control_frame(&self) -> Option<(u64, Instant)> {
        if !self.state.control.enabled || self.retained_frame.is_none() {
            return None;
        }
        self.pointer_ready_at
            .map(|at| (self.state.frame_serial, at))
    }
    pub(in crate::remote) fn discard_control_frame(&mut self) {
        if self.state.control.enabled {
            if let Some(frame) = self.retained_frame.take() {
                frame.destroy();
            }
            self.pointer_ready_at = None;
        }
    }
    pub(in crate::remote) fn control_action(
        &mut self,
        action: &super::super::wayland_control_wire::ControlAction,
        serial: Option<u64>,
    ) -> RemoteResult<()> {
        let result = self.control_action_inner(action, serial);
        if result.is_err() {
            self.revoke_control();
        }
        result
    }
    fn control_action_inner(
        &mut self,
        action: &super::super::wayland_control_wire::ControlAction,
        serial: Option<u64>,
    ) -> RemoteResult<()> {
        use super::super::wayland_control_wire::ControlActionKind;
        self.expire_control();
        self.sync_until(Instant::now() + PROTOCOL_TIMEOUT)?;
        let epoch = u32::try_from(action.lease_epoch)?;
        let sequence = u32::try_from(action.sequence)?;
        if self.control_revoked()
            || !self.state.control.armed
            || self.state.control.paused
            || self.state.control.transition.is_some()
            || self.state.control.epoch != Some(epoch)
            || self.state.control.accepted.checked_add(1) != Some(sequence)
            || sequence == 0
            || sequence > 60000
        {
            return Err(invalid("native control action not permitted").into());
        }
        self.state.control.expected = Some(sequence);
        let lease = self
            .control_lease
            .as_ref()
            .ok_or_else(|| invalid("missing control lease"))?;
        match action.kind {
            ControlActionKind::Key { code, pressed } => {
                if !crate::remote_keyboard_protocol::keyboard_key_allowed(u32::from(code)) {
                    return Err(invalid("control key not allowed").into());
                }
                lease.key(epoch, sequence, u32::from(code), u32::from(pressed));
            }
            ControlActionKind::Motion { x, y }
            | ControlActionKind::ClickLeft { x, y }
            | ControlActionKind::ScrollUp { x, y }
            | ControlActionKind::ScrollDown { x, y }
            | ControlActionKind::ScrollLeft { x, y }
            | ControlActionKind::ScrollRight { x, y } => {
                if matches!(
                    action.kind,
                    ControlActionKind::ScrollUp { .. } | ControlActionKind::ScrollDown { .. }
                ) && !self.state.control.scroll_enabled
                {
                    return Err(invalid("control scroll not authorized").into());
                }
                if matches!(
                    action.kind,
                    ControlActionKind::ScrollLeft { .. } | ControlActionKind::ScrollRight { .. }
                ) && !self.state.control.horizontal_enabled
                {
                    return Err(invalid("control horizontal scroll not authorized").into());
                }
                let (actual, at) = self
                    .control_frame()
                    .ok_or_else(|| invalid("missing control pointer frame"))?;
                if serial != Some(actual)
                    || at.elapsed() >= super::super::wayland_pointer_wire::FRAME_MAX_AGE
                {
                    return Err(invalid("stale control pointer frame").into());
                }
                lease.action(
                    epoch,
                    sequence,
                    self.retained_frame.as_ref().unwrap(),
                    match action.kind {
                        ControlActionKind::Motion { .. } => 0,
                        ControlActionKind::ClickLeft { .. } => 1,
                        ControlActionKind::ScrollUp { .. } => 2,
                        ControlActionKind::ScrollDown { .. } => 3,
                        ControlActionKind::ScrollLeft { .. } => 4,
                        ControlActionKind::ScrollRight { .. } => 5,
                        _ => unreachable!(),
                    },
                    u32::from(x),
                    u32::from(y),
                );
            }
        }
        let deadline =
            (Instant::now() + PROTOCOL_TIMEOUT).min(self.state.control.deadline.unwrap());
        self.await_control(deadline, |s| s.accepted == sequence)?;
        self.state.control.expected = None;
        // Every action invalidates the prior native proof, including both key edges.
        self.discard_control_frame();
        Ok(())
    }
}
wayland_client::delegate_noop!(State: ignore JwmRemoteControlManagerV1);
impl Dispatch<JwmRemoteControlLeaseV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &JwmRemoteControlLeaseV1,
        event: control_lease::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let keyboard = &mut state.control;
        if keyboard.revoked {
            return;
        }
        match event {
            control_lease::Event::Keymap { format, fd, size } => {
                let read = || -> RemoteResult<[u8; 32]> {
                    if format != 1 || size == 0 || size > 1024 * 1024 {
                        return Err(invalid("invalid native keyboard map size or format").into());
                    }
                    let file = File::from(fd);
                    if !file.metadata()?.is_file() {
                        return Err(invalid("native keyboard map is not a regular file").into());
                    }
                    let mut bytes = vec![0; size as usize];
                    file.read_exact_at(&mut bytes, 0)?;
                    super::super::keyboard_keymap::canonical_digest(&bytes)
                };
                if keyboard.digest.is_some() {
                    keyboard.revoked = true;
                    return;
                }
                match read() {
                    Ok(digest) => keyboard.digest = Some(digest),
                    Err(_) => keyboard.revoked = true,
                }
            }
            control_lease::Event::Granted { id, epoch, ttl_ms } => {
                if id == 0
                    || epoch == 0
                    || ttl_ms == 0
                    || ttl_ms > 30_000
                    || keyboard.epoch.is_some()
                {
                    keyboard.revoked = true;
                    return;
                }
                keyboard.epoch = Some(epoch);
                keyboard.deadline = Some(Instant::now() + Duration::from_millis(u64::from(ttl_ms)));
            }
            control_lease::Event::Armed { epoch } => {
                if keyboard.epoch != Some(epoch)
                    || !keyboard.arm_sent
                    || keyboard.armed
                    || keyboard.digest.is_none()
                {
                    keyboard.revoked = true;
                } else {
                    keyboard.armed = true;
                    keyboard.generation = 1;
                }
            }
            control_lease::Event::Accepted { epoch, sequence } => {
                if !keyboard.armed
                    || keyboard.paused
                    || keyboard.transition.is_some()
                    || keyboard.epoch != Some(epoch)
                    || keyboard.expected != Some(sequence)
                    || keyboard.accepted.checked_add(1) != Some(sequence)
                {
                    keyboard.revoked = true;
                } else {
                    keyboard.accepted = sequence;
                }
            }
            control_lease::Event::Paused {
                epoch,
                sequence,
                generation,
            } => {
                keyboard.accept_transition(false, epoch, sequence, generation);
            }
            control_lease::Event::Resumed {
                epoch,
                sequence,
                generation,
            } => {
                keyboard.accept_transition(true, epoch, sequence, generation);
            }
            control_lease::Event::Revoked { .. } => keyboard.revoked = true,
        }
    }
}

#[cfg(test)]
mod resumable_tests {
    use super::*;

    fn armed() -> NativeControl {
        NativeControl {
            enabled: true,
            resumable: true,
            armed: true,
            epoch: Some(7),
            generation: 1,
            deadline: Some(Instant::now() + Duration::from_secs(10)),
            ..Default::default()
        }
    }

    #[test]
    fn native_pause_resume_ack_is_ordered_and_never_renews_deadline() {
        let mut state = armed();
        let deadline = state.deadline;
        state.transition = Some((false, 1, 1));
        assert!(
            !state.paused,
            "queued pause is not applied release evidence"
        );
        state.accept_transition(false, 7, 1, 1);
        assert!(state.paused && !state.revoked);
        assert_eq!(state.accepted, 1);
        assert_eq!(state.deadline, deadline);
        state.transition = Some((true, 2, 2));
        assert!(state.paused, "queued resume does not enable input");
        state.accept_transition(true, 7, 2, 2);
        assert!(!state.paused && !state.revoked);
        assert_eq!((state.accepted, state.generation), (2, 2));
        assert_eq!(state.deadline, deadline);
        state.accept_transition(true, 7, 2, 2);
        assert!(state.revoked, "duplicate ack is terminal");
    }

    #[test]
    fn unsolicited_stale_or_unnegotiated_native_transition_is_terminal() {
        for case in 0..6 {
            let mut state = armed();
            state.transition = Some((false, 1, 1));
            match case {
                0 => state.resumable = false,
                1 => state.transition = None,
                2 => state.epoch = Some(8),
                3 => state.generation = 2,
                4 => state.accepted = 1,
                _ => state.revoked = true,
            }
            state.accept_transition(false, 7, 1, 1);
            assert!(state.revoked, "case {case}");
            assert!(!state.paused, "invalid ack must not commit transition");
        }
    }
}
