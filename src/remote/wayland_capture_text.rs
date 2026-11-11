//! Lease creation precedes the first captured proof; no context rebinding.
use super::*;
use crate::remote_text_protocol::client::{
    jwm_remote_text_lease_v1::{self as text_lease, JwmRemoteTextLeaseV1},
    jwm_remote_text_manager_v1::JwmRemoteTextManagerV1,
};
#[derive(Default)]
pub(super) struct NativeText {
    pub(super) global: Option<u32>,
    pub(super) manager: Option<JwmRemoteTextManagerV1>,
    pub(super) enabled: bool,
    pub(super) ended: bool,
    pub(super) revoked: bool,
    epoch: Option<u32>,
    deadline: Option<Instant>,
    accepted: bool,
    lease: Option<JwmRemoteTextLeaseV1>,
}
impl WaylandCapture {
    pub(in crate::remote) fn enable_text(&mut self) -> RemoteResult<()> {
        if self.state.interactive_popup.enabled
            || self.state.popups
            || self.state.resize_follow
            || self.state.frame_serial != 0
            || self.state.text.enabled
            || self.state.keyboard.enabled
            || self.state.pointer.enabled
            || self.state.control.enabled
        {
            return Err(invalid("text requires an unused exclusive native source").into());
        }
        let manager = self
            .state
            .text
            .manager
            .as_ref()
            .ok_or_else(|| invalid("native one-shot text capability unavailable"))?;
        self.state.text.enabled = true;
        self.state.text.lease =
            Some(manager.create_lease(self.source.as_ref().unwrap(), &self.queue.handle(), ()));
        let deadline = Instant::now() + PROTOCOL_TIMEOUT;
        self.drive_until(deadline, |s| s.text.epoch.is_some() || s.text.revoked)?;
        if self.state.text.revoked || self.state.text.epoch.is_none() {
            return Err(invalid("native text context denied").into());
        }
        Ok(())
    }
    pub(in crate::remote) fn text_frame(&self) -> Option<(u64, Instant)> {
        if self.state.text.enabled
            && !self.state.text.ended
            && !self.state.text.revoked
            && self.retained_frame.is_some()
        {
            self.pointer_ready_at
                .map(|at| (self.state.frame_serial, at))
        } else {
            None
        }
    }
    pub(in crate::remote) fn text_commit(
        &mut self,
        text: &crate::remote::wayland_text_wire::ExplicitText,
        serial: u64,
    ) -> RemoteResult<()> {
        // Consume local attempt before every check, including stale-frame failures.
        let available = self.state.text.enabled && !self.state.text.ended;
        self.state.text.ended = true;
        let result = (|| {
            self.sync_until(Instant::now() + PROTOCOL_TIMEOUT)?;
            if !available
                || self.state.text.revoked
                || self
                    .state
                    .text
                    .deadline
                    .is_none_or(|at| Instant::now() >= at)
                || serial != self.state.frame_serial
                || self
                    .pointer_ready_at
                    .is_none_or(|at| at.elapsed() >= Duration::from_secs(1))
            {
                return Err(invalid("native text lease or proof no longer valid").into());
            }
            let frame = self
                .retained_frame
                .as_ref()
                .ok_or_else(|| invalid("native text proof absent"))?;
            let epoch = self
                .state
                .text
                .epoch
                .ok_or_else(|| invalid("native text epoch absent"))?;
            self.state
                .text
                .lease
                .as_ref()
                .unwrap()
                .commit(epoch, frame, text.as_bytes().to_vec());
            self.drive_until(Instant::now() + PROTOCOL_TIMEOUT, |s| {
                s.text.accepted || s.text.revoked
            })?;
            if !self.state.text.accepted {
                return Err(invalid("native text commit rejected").into());
            }
            Ok(())
        })();
        self.finish_text();
        result
    }
    pub(in crate::remote) fn finish_text(&mut self) {
        if !self.state.text.enabled {
            return;
        }
        self.state.text.ended = true;
        if let Some(lease) = self.state.text.lease.take() {
            lease.destroy();
        }
        if let Some(frame) = self.retained_frame.take() {
            frame.destroy();
        }
        self.pointer_ready_at = None;
    }
    pub(super) fn expire_text(&mut self) {
        if self.state.text.enabled
            && !self.state.text.ended
            && (self.state.text.revoked
                || self
                    .state
                    .text
                    .deadline
                    .is_some_and(|at| Instant::now() >= at))
        {
            self.finish_text();
        }
        // Fresh proof expiry releases storage only; the frozen context lease is never recreated.
        if self.state.text.enabled
            && !self.state.text.ended
            && self
                .pointer_ready_at
                .is_some_and(|at| at.elapsed() >= Duration::from_secs(1))
        {
            if let Some(frame) = self.retained_frame.take() {
                frame.destroy();
            }
            self.pointer_ready_at = None;
        }
    }
}
wayland_client::delegate_noop!(State: ignore JwmRemoteTextManagerV1);
impl Dispatch<JwmRemoteTextLeaseV1, ()> for State {
    fn event(
        s: &mut Self,
        _: &JwmRemoteTextLeaseV1,
        e: text_lease::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            text_lease::Event::Granted { id, epoch, ttl_ms } => {
                if !s.text.enabled
                    || s.text.ended
                    || s.text.revoked
                    || s.text.epoch.is_some()
                    || id == 0
                    || epoch == 0
                    || ttl_ms != 30000
                {
                    s.text.revoked = true;
                } else {
                    s.text.epoch = Some(epoch);
                    s.text.deadline =
                        Some(Instant::now() + Duration::from_millis(u64::from(ttl_ms)));
                }
            }
            text_lease::Event::Accepted { epoch } => {
                if s.text.ended
                    && s.text.epoch == Some(epoch)
                    && !s.text.accepted
                    && !s.text.revoked
                {
                    s.text.accepted = true;
                } else {
                    s.text.revoked = true;
                }
            }
            text_lease::Event::Revoked { .. } => s.text.revoked = true,
        }
    }
}
