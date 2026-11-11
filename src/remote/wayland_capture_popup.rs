//! Explicit family capture adapter. No automatic arm or input dispatch.
use super::*;
use crate::remote_popup_protocol::client::{
    jwm_remote_popup_lease_v1::{self as lease, JwmRemotePopupLeaseV1},
    jwm_remote_popup_manager_v1::JwmRemotePopupManagerV1,
};
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PopupFrameProof {
    pub(crate) epoch: u32,
    pub(crate) cookie: u32,
    pub(crate) capture_sequence: u64,
    pub(crate) native_ready_serial: u64,
    pub(crate) generation: u64,
    pub(crate) root_size: (u32, u32),
    pub(crate) popup_rect: Option<(u32, u32, u32, u32)>,
}
#[cfg(all(test, feature = "backend-wayland-udev"))]
pub(crate) struct PopupCapturedFrame {
    pub(crate) frame: CapturedFrame,
    pub(crate) proof: PopupFrameProof,
}
#[derive(Default)]
pub(super) struct NativePopup {
    pub(super) enabled: bool,
    pub(super) manager: Option<JwmRemotePopupManagerV1>,
    lease: Option<JwmRemotePopupLeaseV1>,
    original: Option<ExtImageCaptureSourceV1>,
    requested_at: Option<Instant>,
    deadline: Option<Instant>,
    epoch: Option<u32>,
    revoked: bool,
    cookie: u32,
    pending: Option<(ExtImageCopyCaptureFrameV1, u32, u64)>,
    proof: Option<PopupFrameProof>,
    previous: Option<PopupFrameProof>,
}
impl NativePopup {
    pub(super) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            ..Self::default()
        }
    }
    fn terminate(&mut self) {
        self.revoked = true;
        self.pending = None;
        self.proof = None;
    }
    pub(super) fn check(&self) -> RemoteResult<()> {
        self.check_at(Instant::now())
    }
    fn check_at(&self, now: Instant) -> RemoteResult<()> {
        if self.enabled && self.revoked {
            return Err(invalid("interactive popup lease revoked").into());
        }
        if self.enabled && self.deadline.is_some_and(|d| now >= d) {
            return Err(invalid("interactive popup lease expired").into());
        }
        Ok(())
    }
}
fn require_before_deadline(now: Instant, deadline: Instant) -> RemoteResult<()> {
    if now >= deadline {
        return Err(invalid("interactive popup protocol deadline expired").into());
    }
    Ok(())
}
fn valid_layout(root: (u32, u32), present: u32, rect: (u32, u32, u32, u32)) -> bool {
    if buffer_layout(root.0, root.1).is_err() {
        return false;
    }
    match present {
        0 => rect == (0, 0, 0, 0),
        1 => {
            rect.2 > 0
                && rect.3 > 0
                && rect.0.checked_add(rect.2).is_some_and(|x| x <= root.0)
                && rect.1.checked_add(rect.3).is_some_and(|y| y <= root.1)
        }
        _ => false,
    }
}
fn matches_pending(
    epoch: Option<u32>,
    cookie: u32,
    sequence: u64,
    frame_matches: bool,
    proof: &PopupFrameProof,
) -> bool {
    frame_matches
        && epoch == Some(proof.epoch)
        && cookie == proof.cookie
        && sequence == proof.capture_sequence
}
fn follows(previous: Option<&PopupFrameProof>, next: &PopupFrameProof) -> bool {
    next.epoch != 0
        && next.native_ready_serial != 0
        && next.generation != 0
        && previous.is_none_or(|p| {
            next.epoch == p.epoch
                && next.root_size == p.root_size
                && next.native_ready_serial > p.native_ready_serial
                && next.generation >= p.generation
                && (next.generation != p.generation || next.popup_rect == p.popup_rect)
        })
}
impl WaylandCapture {
    #[cfg(all(test, feature = "backend-wayland-udev"))]
    pub(crate) fn connect_test_stream_interactive_popup(
        stream: UnixStream,
        index: usize,
    ) -> RemoteResult<Self> {
        Self::connect_stream_mode(stream, 0, false, false, true, |_| {
            index
                .checked_sub(1)
                .filter(|i| *i < MAX_TOPLEVELS)
                .ok_or_else(|| invalid("invalid interactive popup selection").into())
        })
    }
    pub(super) fn popup_protocol_deadline(&self) -> RemoteResult<Instant> {
        self.state.interactive_popup.check()?;
        let deadline = Instant::now() + PROTOCOL_TIMEOUT;
        Ok(self
            .state
            .interactive_popup
            .deadline
            .map_or(deadline, |d| d.min(deadline)))
    }
    pub(super) fn create_interactive_popup_source(
        &mut self,
        original: ExtImageCaptureSourceV1,
    ) -> RemoteResult<ExtImageCaptureSourceV1> {
        let p = &mut self.state.interactive_popup;
        let manager = p
            .manager
            .as_ref()
            .filter(|m| m.version() >= 2)
            .ok_or_else(|| invalid("native interactive popup v2 unavailable"))?;
        if p.requested_at.is_some() {
            return Err(invalid("interactive popup lease already requested").into());
        }
        let now = Instant::now();
        p.requested_at = Some(now);
        p.deadline = Some(now + Duration::from_secs(30));
        p.lease = Some(manager.create_lease(&original, &self.queue.handle(), ()));
        p.original = Some(original);
        let deadline = self.popup_protocol_deadline()?;
        self.drive_until(deadline, |s| s.interactive_popup.epoch.is_some())?;
        let source = self
            .state
            .interactive_popup
            .lease
            .as_ref()
            .unwrap()
            .get_capture_source(&self.queue.handle(), ());
        Ok(source)
    }
    pub(super) fn export_interactive_popup_frame(&mut self, deadline: Instant) -> RemoteResult<()> {
        let result = self.export_interactive_popup_frame_inner(deadline);
        if result.is_err() {
            self.fail_interactive_popup();
        }
        result
    }
    fn export_interactive_popup_frame_inner(&mut self, deadline: Instant) -> RemoteResult<()> {
        let frame = self
            .retained_frame
            .as_ref()
            .ok_or_else(|| invalid("missing interactive popup frame"))?
            .clone();
        let p = &mut self.state.interactive_popup;
        if p.pending.is_some() {
            return Err(invalid("duplicate popup metadata request").into());
        }
        p.cookie = p
            .cookie
            .checked_add(1)
            .filter(|c| *c <= 1024)
            .ok_or_else(|| invalid("popup export budget exhausted"))?;
        p.proof = None;
        p.pending = Some((frame.clone(), p.cookie, self.state.frame_serial));
        p.lease
            .as_ref()
            .unwrap()
            .export_frame_proof(p.epoch.unwrap(), &frame, p.cookie);
        let deadline = deadline.min(self.popup_protocol_deadline()?);
        self.drive_until(deadline, |s| s.interactive_popup.proof.is_some())?;
        self.sync_until(deadline)?;
        require_before_deadline(Instant::now(), deadline)?;
        self.state.check()
    }
    #[cfg(all(test, feature = "backend-wayland-udev"))]
    pub(crate) fn capture_interactive_popup_frame(&mut self) -> RemoteResult<PopupCapturedFrame> {
        let result = self.capture_interactive_popup_frame_inner();
        if result.is_err() && self.state.interactive_popup.enabled {
            self.fail_interactive_popup();
        }
        result
    }
    #[cfg(all(test, feature = "backend-wayland-udev"))]
    fn capture_interactive_popup_frame_inner(&mut self) -> RemoteResult<PopupCapturedFrame> {
        if !self.state.interactive_popup.enabled || self.retained_frame.is_some() {
            return Err(invalid("interactive popup frame still retained or mode disabled").into());
        }
        let CaptureOutcome::Frame(frame) = self.frame()? else {
            return Err(invalid("interactive popup capture did not complete").into());
        };
        let proof = self
            .state
            .interactive_popup
            .proof
            .clone()
            .ok_or_else(|| invalid("missing popup metadata"))?;
        Ok(PopupCapturedFrame { frame, proof })
    }
    #[cfg(all(test, feature = "backend-wayland-udev"))]
    pub(crate) fn discard_popup_frame(&mut self) {
        if self.state.interactive_popup.enabled {
            if let Some(frame) = self.retained_frame.take() {
                frame.destroy();
            }
            self.state.interactive_popup.proof = None;
        }
    }
    #[cfg(all(test, feature = "backend-wayland-udev"))]
    pub(crate) fn destroy_original_popup_source_for_test(&mut self) {
        if let Some(source) = self.state.interactive_popup.original.take() {
            source.destroy();
        }
    }
    pub(super) fn fail_interactive_popup(&mut self) {
        self.state.interactive_popup.terminate();
        if let Some(frame) = self.inflight.take() {
            frame.destroy();
        }
        self.finish_interactive_popup();
    }
    pub(super) fn finish_interactive_popup(&mut self) {
        if !self.state.interactive_popup.enabled {
            return;
        }
        if let Some(frame) = self.retained_frame.take() {
            frame.destroy();
        }
        if let Some(lease) = self.state.interactive_popup.lease.take() {
            lease.destroy();
        }
        if let Some(source) = self.state.interactive_popup.original.take() {
            source.destroy();
        }
    }
}
impl Dispatch<JwmRemotePopupManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &JwmRemotePopupManagerV1,
        _: crate::remote_popup_protocol::client::jwm_remote_popup_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<JwmRemotePopupLeaseV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &JwmRemotePopupLeaseV1,
        event: lease::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if state.interactive_popup.lease.as_ref() != Some(proxy)
            || !state.interactive_popup.enabled
            || state.interactive_popup.revoked
            || state.interactive_popup.check().is_err()
        {
            state.fail("unexpected interactive popup lease event");
            return;
        }
        let p = &mut state.interactive_popup;
        match event {
            lease::Event::Granted { epoch, ttl_ms }
                if p.epoch.is_none() && epoch != 0 && ttl_ms == 30000 =>
            {
                p.epoch = Some(epoch);
            }
            lease::Event::Revoked { .. } => {
                p.revoked = true;
            }
            lease::Event::FrameProof {
                epoch,
                frame,
                cookie,
                ready_hi,
                ready_lo,
                generation_hi,
                generation_lo,
                root_width,
                root_height,
                popup_present,
                popup_x,
                popup_y,
                popup_width,
                popup_height,
            } => {
                let rect = (popup_x, popup_y, popup_width, popup_height);
                let Some((expected_frame, expected_cookie, capture_sequence)) = p.pending.as_ref()
                else {
                    state.fail("unsolicited popup metadata");
                    return;
                };
                let proof = PopupFrameProof {
                    epoch,
                    cookie,
                    capture_sequence: *capture_sequence,
                    native_ready_serial: (u64::from(ready_hi) << 32) | u64::from(ready_lo),
                    generation: (u64::from(generation_hi) << 32) | u64::from(generation_lo),
                    root_size: (root_width, root_height),
                    popup_rect: (popup_present == 1).then_some(rect),
                };
                if !matches_pending(
                    p.epoch,
                    *expected_cookie,
                    *capture_sequence,
                    frame == *expected_frame,
                    &proof,
                ) || state.size.map(|(w, h)| (u32::from(w), u32::from(h)))
                    != Some(proof.root_size)
                    || !valid_layout(proof.root_size, popup_present, rect)
                    || !follows(p.previous.as_ref(), &proof)
                {
                    state.fail("invalid interactive popup metadata binding");
                    return;
                }
                p.pending = None;
                p.previous = Some(proof.clone());
                p.proof = Some(proof);
            }
            _ => state.fail("unexpected interactive popup authority event"),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn popup_export_layout_is_canonical_and_bounded() {
        assert!(valid_layout((320, 240), 0, (0, 0, 0, 0)));
        assert!(valid_layout((320, 240), 1, (200, 140, 120, 100)));
        for (present, rect) in [
            (0, (1, 0, 0, 0)),
            (2, (0, 0, 1, 1)),
            (1, (0, 0, 0, 1)),
            (1, (u32::MAX, 0, 2, 1)),
            (1, (319, 0, 2, 1)),
        ] {
            assert!(!valid_layout((320, 240), present, rect));
        }
    }
    #[test]
    fn popup_export_serial_and_generation_never_rewind_or_change_same_layout() {
        let p = PopupFrameProof {
            epoch: 1,
            cookie: 1,
            capture_sequence: 1,
            native_ready_serial: 1,
            generation: 1,
            root_size: (320, 240),
            popup_rect: None,
        };
        assert!(follows(None, &p));
        assert!(!follows(Some(&p), &p));
        let mut n = p.clone();
        n.native_ready_serial = 2;
        assert!(follows(Some(&p), &n));
        n.popup_rect = Some((1, 1, 10, 10));
        assert!(!follows(Some(&p), &n));
        n.generation = 2;
        assert!(follows(Some(&p), &n));
        n.epoch = 2;
        assert!(!follows(Some(&p), &n));
    }
    #[test]
    fn popup_metadata_requires_exact_pending_identity() {
        let proof = PopupFrameProof {
            epoch: 9,
            cookie: 3,
            capture_sequence: 8,
            native_ready_serial: 15,
            generation: 2,
            root_size: (320, 240),
            popup_rect: None,
        };
        assert!(matches_pending(Some(9), 3, 8, true, &proof));
        assert!(!matches_pending(None, 3, 8, true, &proof));
        assert!(!matches_pending(Some(10), 3, 8, true, &proof));
        assert!(!matches_pending(Some(9), 4, 8, true, &proof));
        assert!(!matches_pending(Some(9), 3, 9, true, &proof));
        assert!(!matches_pending(Some(9), 3, 8, false, &proof));
    }
    #[test]
    fn popup_deadline_is_absolute_and_revoke_is_terminal() {
        let start = Instant::now();
        let end = start + Duration::from_secs(30);
        let mut state = NativePopup::new(true);
        state.requested_at = Some(start);
        state.deadline = Some(end);
        assert!(state.check_at(end - Duration::from_nanos(1)).is_ok());
        assert!(state.check_at(end).is_err());
        assert!(state.check_at(end + Duration::from_secs(1)).is_err());
        assert_eq!(state.deadline, Some(end));
        state.revoked = true;
        assert!(state.check_at(start).is_err());
    }
    #[test]
    fn popup_protocol_deadline_rejects_completed_but_late_reply() {
        let deadline = Instant::now();
        assert!(require_before_deadline(deadline - Duration::from_nanos(1), deadline).is_ok());
        assert!(require_before_deadline(deadline, deadline).is_err());
        assert!(require_before_deadline(deadline + Duration::from_nanos(1), deadline).is_err());
    }
    #[test]
    fn popup_failure_latches_even_before_lease_expiry_and_discards_metadata() {
        let mut state = NativePopup::new(true);
        state.deadline = Some(Instant::now() + Duration::from_secs(30));
        state.proof = Some(PopupFrameProof {
            epoch: 1,
            cookie: 1,
            capture_sequence: 1,
            native_ready_serial: 1,
            generation: 1,
            root_size: (320, 240),
            popup_rect: None,
        });
        state.terminate();
        assert!(state.proof.is_none() && state.pending.is_none());
        assert!(state.check().is_err());
        state.terminate();
        assert!(state.check().is_err());
    }
}
