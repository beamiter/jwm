//! Explicit family capture adapter. No automatic arm or input dispatch.
use super::*;
use crate::remote_popup_protocol::client::{
    jwm_remote_popup_lease_v1::{self as lease, JwmRemotePopupLeaseV1},
    jwm_remote_popup_manager_v1::JwmRemotePopupManagerV1,
};
#[cfg(all(test, feature = "backend-wayland-udev"))]
use std::sync::Arc;
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
pub(crate) struct PopupCapturedFrame {
    pub(crate) frame: CapturedFrame,
    pub(crate) proof: PopupFrameProof,
}
/// An owned captured image and an opaque binding, never constructed from a
/// caller-supplied proof or a claimed presentation boolean.
#[cfg(all(test, feature = "backend-wayland-udev"))]
pub(crate) struct PopupPresentationCandidate {
    frame: CapturedFrame,
    binding: PopupPresentationBinding,
}
#[cfg(all(test, feature = "backend-wayland-udev"))]
impl PopupPresentationCandidate {
    pub(crate) fn proof(&self) -> &PopupFrameProof {
        &self.binding.proof
    }
    pub(crate) fn into_parts(self) -> (CapturedFrame, PopupPresentationBinding) {
        (self.frame, self.binding)
    }
}
#[cfg(all(test, feature = "backend-wayland-udev"))]
pub(crate) struct PopupPresentationBinding {
    identity: Arc<()>,
    frame: ExtImageCopyCaptureFrameV1,
    proof: PopupFrameProof,
    capture_started_at: Instant,
}
#[cfg(all(test, feature = "backend-wayland-udev"))]
impl PopupPresentationBinding {
    pub(crate) fn proof(&self) -> &PopupFrameProof {
        &self.proof
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PopupOperation {
    Arm,
    Click(u32),
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
    pub(super) transport_aborted: bool,
    cookie: u32,
    #[cfg(all(test, feature = "backend-wayland-udev"))]
    identity: Arc<()>,
    #[cfg(all(test, feature = "backend-wayland-udev"))]
    viewer_identity: Option<Arc<()>>,
    pub(super) frame_started_at: Option<Instant>,
    operation: Option<PopupOperation>,
    armed: bool,
    accepted_sequence: u32,
    last_consumed_serial: u64,
    last_completion: Option<Instant>,
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
    fn complete_operation(&mut self, epoch: u32, operation: PopupOperation) -> bool {
        if self.revoked || self.epoch != Some(epoch) || self.operation != Some(operation) {
            return false;
        }
        match operation {
            PopupOperation::Arm if !self.armed => self.armed = true,
            PopupOperation::Click(sequence)
                if self.armed && self.accepted_sequence.checked_add(1) == Some(sequence) =>
            {
                self.accepted_sequence = sequence;
            }
            _ => return false,
        }
        self.operation = None;
        true
    }
    fn terminate(&mut self) {
        self.revoked = true;
        self.pending = None;
        self.proof = None;
        self.operation = None;
    }
    pub(super) fn started(&self) -> bool {
        self.enabled && self.lease.is_some() && self.epoch.is_some() && self.deadline.is_some()
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
fn popup_click_inside(proof: &PopupFrameProof, x: u32, y: u32) -> bool {
    x < proof.root_size.0
        && y < proof.root_size.1
        && proof
            .popup_rect
            .is_none_or(|(px, py, w, h)| x >= px && y >= py && x - px < w && y - py < h)
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
        Self::connect_stream_mode(stream, 0, false, false, true, false, |_| {
            index
                .checked_sub(1)
                .filter(|i| *i < MAX_TOPLEVELS)
                .ok_or_else(|| invalid("invalid interactive popup selection").into())
        })
    }
    /// Select and retain only the original root. Lease creation is deferred
    /// until both authenticated peers have explicitly negotiated this mode.
    pub(in crate::remote) fn prepare_popup_pointer(display: Option<&str>) -> RemoteResult<Self> {
        let path = socket_path(display)?;
        let stream = jwm_ipc_transport::connect(&path, PROTOCOL_TIMEOUT)?;
        Self::connect_stream_mode(stream, 0, false, false, true, true, |capture| {
            eprint!("jwm-remote: select Wayland window index: ");
            io::stderr().flush()?;
            capture.read_selection(Instant::now() + SELECTION_TIMEOUT)
        })
    }
    #[cfg(all(test, feature = "backend-wayland-udev"))]
    pub(crate) fn connect_test_stream_popup_pointer(
        stream: UnixStream,
        index: usize,
    ) -> RemoteResult<Self> {
        Self::connect_stream_mode(stream, 0, false, false, true, true, |_| {
            index
                .checked_sub(1)
                .filter(|i| *i < MAX_TOPLEVELS)
                .ok_or_else(|| invalid("invalid interactive popup selection").into())
        })
    }
    pub(super) fn retain_popup_original(
        &mut self,
        source: ExtImageCaptureSourceV1,
    ) -> RemoteResult<()> {
        let p = &mut self.state.interactive_popup;
        if !p.enabled || p.original.is_some() || p.lease.is_some() {
            return Err(invalid("invalid prepared popup source").into());
        }
        p.original = Some(source);
        Ok(())
    }
    pub(in crate::remote) fn start_popup_pointer(&mut self) -> RemoteResult<()> {
        let result = self.start_popup_pointer_inner();
        if result.is_err() {
            self.fail_interactive_popup();
        }
        result
    }
    fn start_popup_pointer_inner(&mut self) -> RemoteResult<()> {
        self.state.check()?;
        let p = &mut self.state.interactive_popup;
        if !p.enabled || p.requested_at.is_some() || self.source.is_some() || self.session.is_some()
        {
            return Err(invalid("popup source already started or not prepared").into());
        }
        let original = p
            .original
            .take()
            .ok_or_else(|| invalid("missing prepared popup source"))?;
        let source = self.create_interactive_popup_source(original)?;
        let deadline = self.popup_protocol_deadline()?;
        self.session = Some(self.state.capture_manager.as_ref().unwrap().create_session(
            &source,
            manager::Options::empty(),
            &self.queue.handle(),
            (),
        ));
        self.source = Some(source);
        self.drive_until(deadline, |s| s.constraints_done || s.session_stopped)?;
        if self.state.session_stopped {
            return Err(invalid("Wayland image capture session stopped").into());
        }
        self.create_buffer()?;
        self.sync_until(deadline)?;
        require_before_deadline(Instant::now(), deadline)?;
        self.state.check()
    }
    pub(in crate::remote) fn popup_lease_info(&self) -> RemoteResult<(u32, Instant, Instant)> {
        self.state.check()?;
        let p = &self.state.interactive_popup;
        Ok((
            p.epoch.ok_or_else(|| invalid("missing popup epoch"))?,
            p.requested_at
                .ok_or_else(|| invalid("missing popup request clock"))?,
            p.deadline
                .ok_or_else(|| invalid("missing popup lease deadline"))?,
        ))
    }
    pub(in crate::remote) fn popup_capture_started_at(&self) -> RemoteResult<Instant> {
        self.state
            .interactive_popup
            .frame_started_at
            .ok_or_else(|| invalid("missing popup capture clock").into())
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
    pub(crate) fn capture_interactive_popup_frame(&mut self) -> RemoteResult<PopupCapturedFrame> {
        let result = self.capture_interactive_popup_frame_inner();
        if result.is_err() && self.state.interactive_popup.enabled {
            self.fail_interactive_popup();
        }
        result
    }
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
    #[cfg(all(test, feature = "backend-wayland-udev"))]
    pub(crate) fn capture_popup_presentation_candidate(
        &mut self,
    ) -> RemoteResult<PopupPresentationCandidate> {
        let captured = self.capture_interactive_popup_frame()?;
        let binding = PopupPresentationBinding {
            identity: self.state.interactive_popup.identity.clone(),
            frame: self
                .retained_frame
                .as_ref()
                .ok_or_else(|| invalid("missing retained popup frame"))?
                .clone(),
            proof: captured.proof,
            capture_started_at: self
                .state
                .interactive_popup
                .frame_started_at
                .ok_or_else(|| invalid("missing popup capture clock"))?,
        };
        Ok(PopupPresentationCandidate {
            frame: captured.frame,
            binding,
        })
    }
    #[cfg(all(test, feature = "backend-wayland-udev"))]
    pub(crate) fn arm_popup_presented(
        &mut self,
        receipt: crate::remote::wayland_viewer::popup_presentation::PopupPresentationReceipt,
    ) -> RemoteResult<()> {
        self.popup_presented_operation(receipt, None)
    }
    #[cfg(all(test, feature = "backend-wayland-udev"))]
    pub(crate) fn click_popup_presented(
        &mut self,
        receipt: crate::remote::wayland_viewer::popup_presentation::PopupPresentationReceipt,
        x: u32,
        y: u32,
    ) -> RemoteResult<()> {
        self.popup_presented_operation(receipt, Some((x, y)))
    }
    #[cfg(all(test, feature = "backend-wayland-udev"))]
    fn popup_presented_operation(
        &mut self,
        receipt: crate::remote::wayland_viewer::popup_presentation::PopupPresentationReceipt,
        point: Option<(u32, u32)>,
    ) -> RemoteResult<()> {
        let result = self.popup_presented_operation_inner(receipt, point);
        if result.is_err() {
            self.fail_interactive_popup();
        }
        result
    }
    #[cfg(all(test, feature = "backend-wayland-udev"))]
    fn popup_presented_operation_inner(
        &mut self,
        receipt: crate::remote::wayland_viewer::popup_presentation::PopupPresentationReceipt,
        point: Option<(u32, u32)>,
    ) -> RemoteResult<()> {
        let (binding, viewer_identity, presented_at, validity) = receipt.into_parts();
        self.state.check()?;
        let now = Instant::now();
        let p = &mut self.state.interactive_popup;
        if !p.enabled
            || p.operation.is_some()
            || p.pending.is_some()
            || !Arc::ptr_eq(&binding.identity, &p.identity)
            || self.retained_frame.as_ref() != Some(&binding.frame)
            || p.proof.as_ref() != Some(&binding.proof)
            || p.viewer_identity
                .as_ref()
                .is_some_and(|v| !Arc::ptr_eq(v, &viewer_identity))
            || presented_at < binding.capture_started_at
            || presented_at > now
            || binding.proof.native_ready_serial <= p.last_consumed_serial
            || p.last_completion
                .is_some_and(|t| binding.capture_started_at <= t)
        {
            return Err(invalid("invalid popup presentation binding").into());
        }
        let operation = match point {
            None if !p.armed && binding.proof.popup_rect.is_none() => PopupOperation::Arm,
            Some((x, y)) if p.armed && popup_click_inside(&binding.proof, x, y) => {
                PopupOperation::Click(
                    p.accepted_sequence
                        .checked_add(1)
                        .filter(|s| *s <= 60000)
                        .ok_or_else(|| invalid("popup action sequence exhausted"))?,
                )
            }
            _ => return Err(invalid("invalid popup presented operation").into()),
        };
        let deadline = (binding.capture_started_at + Duration::from_secs(1))
            .min(
                p.deadline
                    .ok_or_else(|| invalid("missing popup lease deadline"))?,
            )
            .min(now + PROTOCOL_TIMEOUT);
        require_before_deadline(now, deadline)?;
        // Serialize authority validation and actual request flushing against
        // viewer replacement/resize/close. Never hold this lock while waiting
        // for native processing or any protocol roundtrip.
        let presentation_guard = validity.lock_current()?;
        require_before_deadline(Instant::now(), deadline)?;
        p.operation = Some(operation); // Register before dispatch can produce a completion.
        let lease = p
            .lease
            .as_ref()
            .ok_or_else(|| invalid("missing popup lease"))?;
        let epoch = p.epoch.ok_or_else(|| invalid("missing popup epoch"))?;
        match operation {
            PopupOperation::Arm => lease.arm(epoch, &binding.frame),
            PopupOperation::Click(sequence) => {
                let (x, y) = point.unwrap();
                lease.action(epoch, sequence, &binding.frame, 1, x, y);
            }
        }
        if let Err(error) = self.conn.flush() {
            // A nonblocking flush may leave this action queued. Prevent later
            // cleanup/Drop from flushing it after presentation invalidation.
            // Delivery may already be partial: terminate, never replay.
            self.abort_interactive_popup_transport();
            return Err(error.into());
        }
        drop(presentation_guard);
        self.drive_until(deadline, |s| s.interactive_popup.operation.is_none())?;
        self.sync_until(deadline)?;
        require_before_deadline(Instant::now(), deadline)?;
        self.state.check()?;
        let p = &mut self.state.interactive_popup;
        p.viewer_identity.get_or_insert(viewer_identity);
        p.last_consumed_serial = binding.proof.native_ready_serial;
        p.last_completion = Some(Instant::now());
        self.discard_popup_frame();
        Ok(())
    }
    /// Production input boundary: only the host's committed/exact-ACK ledger
    /// can mint this opaque permit. Wire metadata alone is never sufficient.
    pub(in crate::remote) fn dispatch_host_popup(
        &mut self,
        permit: crate::remote::wayland_popup_wire::NativePermit,
    ) -> RemoteResult<()> {
        let result = self.dispatch_host_popup_inner(permit);
        if result.is_err() {
            self.fail_interactive_popup();
        }
        result
    }
    fn dispatch_host_popup_inner(
        &mut self,
        permit: crate::remote::wayland_popup_wire::NativePermit,
    ) -> RemoteResult<()> {
        use crate::remote::wayland_popup_wire::NativeOperation;
        let (metadata, captured_at, requested) = permit.into_parts();
        self.state.check()?;
        let now = Instant::now();
        let p = &mut self.state.interactive_popup;
        let proof = p
            .proof
            .as_ref()
            .ok_or_else(|| invalid("missing popup retained metadata"))?;
        let rect = metadata.popup.map(|r| (r.x, r.y, r.width, r.height));
        if !p.enabled
            || p.operation.is_some()
            || p.pending.is_some()
            || p.frame_started_at != Some(captured_at)
            || metadata.key.binding.lease_epoch != proof.epoch
            || metadata.key.geometry_epoch != self.epoch
            || metadata.key.native_frame_id != proof.capture_sequence
            || metadata.key.native_ready_serial != proof.native_ready_serial
            || metadata.key.family_generation != proof.generation
            || metadata.canvas != proof.root_size
            || rect != proof.popup_rect
            || proof.native_ready_serial <= p.last_consumed_serial
            || p.last_completion.is_some_and(|t| captured_at <= t)
        {
            return Err(invalid("host popup permit does not match retained native frame").into());
        }
        let operation = match requested {
            NativeOperation::Arm if !p.armed && proof.popup_rect.is_none() => PopupOperation::Arm,
            // This sequence is minted by the host ledger's native-accepted
            // counter, not the network processed-command counter. An explicit
            // no-dispatch stale rejection must never create a native gap.
            NativeOperation::Click { sequence, x, y }
                if p.armed
                    && p.accepted_sequence.checked_add(1) == Some(sequence)
                    && popup_click_inside(proof, x, y) =>
            {
                PopupOperation::Click(sequence)
            }
            _ => return Err(invalid("invalid host popup operation").into()),
        };
        let serial = proof.native_ready_serial;
        let deadline = captured_at
            .checked_add(Duration::from_secs(1))
            .ok_or_else(|| invalid("popup clock overflow"))?
            .min(
                p.deadline
                    .ok_or_else(|| invalid("missing popup deadline"))?,
            )
            .min(now + PROTOCOL_TIMEOUT);
        require_before_deadline(now, deadline)?;
        let frame = self
            .retained_frame
            .as_ref()
            .ok_or_else(|| invalid("missing retained native popup frame"))?;
        let lease = p
            .lease
            .as_ref()
            .ok_or_else(|| invalid("missing native popup lease"))?;
        let epoch = p
            .epoch
            .ok_or_else(|| invalid("missing native popup epoch"))?;
        p.operation = Some(operation);
        match requested {
            NativeOperation::Arm => lease.arm(epoch, frame),
            NativeOperation::Click { sequence, x, y } => {
                lease.action(epoch, sequence, frame, 1, x, y)
            }
        }
        if let Err(error) = self.conn.flush() {
            self.abort_interactive_popup_transport();
            return Err(error.into());
        }
        self.drive_until(deadline, |s| s.interactive_popup.operation.is_none())?;
        self.sync_until(deadline)?;
        require_before_deadline(Instant::now(), deadline)?;
        self.state.check()?;
        let p = &mut self.state.interactive_popup;
        p.last_consumed_serial = serial;
        p.last_completion = Some(Instant::now());
        self.discard_popup_frame();
        Ok(())
    }
    fn abort_interactive_popup_transport(&mut self) {
        if self.state.interactive_popup.transport_aborted {
            return;
        }
        // Lock out all future adapter/Drop flushes before touching the socket.
        self.state.interactive_popup.transport_aborted = true;
        use std::os::fd::AsRawFd;
        let backend = self.conn.backend();
        let fd = backend.poll_fd();
        // SAFETY: this is a borrowed descriptor of our own live native Wayland
        // connection. shutdown does not take ownership or close another fd.
        unsafe {
            nix::libc::shutdown(fd.as_raw_fd(), nix::libc::SHUT_RDWR);
        }
    }
    pub(super) fn fail_interactive_popup(&mut self) {
        self.state.interactive_popup.terminate();
        if let Some(frame) = self.inflight.take() {
            frame.destroy();
        }
        self.finish_interactive_popup();
    }
    pub(in crate::remote) fn finish_interactive_popup_confirmed(
        &mut self,
        deadline: Instant,
    ) -> RemoteResult<()> {
        self.finish_interactive_popup();
        if self.state.interactive_popup.transport_aborted {
            return Err(invalid("popup cleanup transport failed").into());
        }
        // The display callback follows lease.destroy on this exact native
        // connection. Flush alone would not prove the compositor processed it.
        self.sync_until(deadline)?;
        require_before_deadline(Instant::now(), deadline)
    }
    pub(in crate::remote) fn finish_interactive_popup(&mut self) {
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
        // Revoke promptly even when the caller retains this failed adapter.
        // A prior action-flush failure has already shut down this connection;
        // never give its queued bytes another chance during cleanup.
        if !self.state.interactive_popup.transport_aborted && self.conn.flush().is_err() {
            self.abort_interactive_popup_transport();
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
            lease::Event::Armed { epoch } => {
                if !p.complete_operation(epoch, PopupOperation::Arm) {
                    state.fail("unexpected popup arm completion");
                }
            }
            lease::Event::Accepted { epoch, sequence } => {
                if !p.complete_operation(epoch, PopupOperation::Click(sequence)) {
                    state.fail("unexpected popup action completion");
                }
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
    #[test]
    fn popup_operation_completion_is_exact_once_and_never_spontaneous() {
        let mut p = NativePopup::new(true);
        p.epoch = Some(7);
        assert!(!p.complete_operation(7, PopupOperation::Arm));
        p.operation = Some(PopupOperation::Arm);
        assert!(!p.complete_operation(8, PopupOperation::Arm));
        assert!(!p.complete_operation(7, PopupOperation::Click(1)));
        assert!(p.complete_operation(7, PopupOperation::Arm));
        assert!(!p.complete_operation(7, PopupOperation::Arm));
        p.operation = Some(PopupOperation::Click(1));
        assert!(!p.complete_operation(7, PopupOperation::Click(2)));
        assert!(p.complete_operation(7, PopupOperation::Click(1)));
        assert!(!p.complete_operation(7, PopupOperation::Click(1)));
        p.operation = Some(PopupOperation::Click(2));
        p.terminate();
        assert!(!p.complete_operation(7, PopupOperation::Click(2)));
        assert_eq!(p.accepted_sequence, 1);
    }
    #[cfg(feature = "backend-wayland-udev")]
    #[test]
    fn popup_click_bounds_never_fall_through_a_popup_to_root() {
        let mut p = PopupFrameProof {
            epoch: 1,
            cookie: 1,
            capture_sequence: 1,
            native_ready_serial: 1,
            generation: 1,
            root_size: (320, 240),
            popup_rect: None,
        };
        assert!(popup_click_inside(&p, 319, 239));
        assert!(!popup_click_inside(&p, 320, 239));
        p.popup_rect = Some((100, 50, 20, 30));
        assert!(popup_click_inside(&p, 100, 50));
        assert!(popup_click_inside(&p, 119, 79));
        for (x, y) in [(99, 50), (120, 50), (100, 49), (100, 80), (u32::MAX, 0)] {
            assert!(!popup_click_inside(&p, x, y));
        }
    }
}
