//! Pointer-only repair coordination. Locks never cross native waits or writer acquisition.
use super::*;
use crate::remote::pointer_repair_phase::{ActionReceipt, RepairPhase, Scope, Token, VisualKind};
use crate::remote::pointer_repair_wire::{RepairStatus, RepairStatusKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Intent {
    Normal,
    Visual(Token),
    PostRepair(Token),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Stamp {
    pub generation: u64,
    pub intent: Intent,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Normal,
    NeedVisual(Token),
    VisualQueued(Token),
    NeedProof(Token),
    ProofQueued(Token),
    AwaitAck(Token, Scope),
    Stopped,
}
pub(super) struct Control {
    phase: Option<RepairPhase>,
    mode: Mode,
    generation: u64,
    completed_action: u64,
    last_input: Instant,
    stable_debt: bool,
}
impl Default for Control {
    fn default() -> Self {
        Self {
            phase: None,
            mode: Mode::Normal,
            generation: 0,
            completed_action: 0,
            last_input: Instant::now(),
            stable_debt: false,
        }
    }
}
fn enabled(s: &SourceControl) -> bool {
    s.static_lossless.load(Ordering::Acquire) && s.pointer_enabled.load(Ordering::Acquire)
}
fn phase_error() -> io::Error {
    invalid_data("pointer repair phase invariant failed")
}
fn status(
    writer: &SharedWriter,
    kind: RepairStatusKind,
    token: Token,
    proof: Option<Scope>,
) -> RemoteResult<()> {
    let payload = RepairStatus { kind, token, proof }.encode()?;
    write_session_record(
        &mut lock_writer(writer),
        MessageKind::WaylandPointerRepairStatus,
        &payload,
        FRAME_WRITE_DEADLINE,
    )?;
    Ok(())
}
pub(super) fn granted(s: &SourceControl, epoch: u64, now: Instant) {
    if enabled(s) {
        let mut r = s.pointer_repair.lock().unwrap();
        r.phase = Some(RepairPhase::new(
            epoch,
            now + super::super::wayland_pointer_wire::LEASE_DURATION,
        ));
        r.last_input = now;
    }
}
pub(super) fn stopped(s: &SourceControl) {
    let mut r = s.pointer_repair.lock().unwrap();
    if let Some(p) = r.phase.as_mut() {
        p.revoke();
    }
    r.mode = Mode::Stopped;
}
pub(super) fn after_action(
    s: &SourceControl,
    writer: &SharedWriter,
    action: PointerAction,
) -> RemoteResult<()> {
    if !enabled(s) {
        return Ok(());
    }
    let cancelled = {
        let mut r = s.pointer_repair.lock().unwrap();
        if r.mode != Mode::Normal || r.completed_action.checked_add(1) != Some(action.sequence) {
            return Err(phase_error().into());
        }
        r.completed_action = action.sequence;
        r.last_input = Instant::now();
        r.phase.as_mut().and_then(RepairPhase::cancel_offer)
    };
    if let Some(t) = cancelled {
        status(writer, RepairStatusKind::Cancelled, t, None)?;
    }
    let receipt = ActionReceipt {
        lease: action.lease_epoch,
        sequence: action.sequence,
        consumed: action.scope.into(),
    };
    write_session_record(
        &mut lock_writer(writer),
        MessageKind::WaylandPointerActionCompleted,
        &receipt.encode()?,
        FRAME_WRITE_DEADLINE,
    )?;
    Ok(())
}
pub(super) fn intercept(
    s: &SourceControl,
    capture: &mut HostCapture,
    writer: &SharedWriter,
    state: &mut HostPointerState,
    command: PointerCommand,
) -> RemoteResult<Option<PointerCommand>> {
    let (token, decline) = match command {
        PointerCommand::RepairRequest(t) => (t, false),
        PointerCommand::RepairDecline(t) => (t, true),
        other => {
            if enabled(s) && s.pointer_repair.lock().unwrap().mode != Mode::Normal {
                return Err(phase_error().into());
            }
            return Ok(Some(other));
        }
    };
    if !enabled(s) {
        return Err(invalid_data("pointer repair not negotiated").into());
    }
    if decline {
        {
            let mut r = s.pointer_repair.lock().unwrap();
            r.phase
                .as_mut()
                .ok_or_else(phase_error)?
                .decline(token, Instant::now())
                .map_err(|_| phase_error())?;
        }
        status(writer, RepairStatusKind::Declined, token, None)?;
        return Ok(None);
    }
    let tracked = *s.pointer_frame.lock().unwrap();
    let drawn = s.state.lock().unwrap().presented_native_proof;
    let (accepted, invalidate_ticket) = {
        let mut r = s.pointer_repair.lock().unwrap();
        let completed = r.completed_action;
        let p = r.phase.as_mut().ok_or_else(phase_error)?;
        let was_current_offer = p.offered_token() == Some(token);
        let current = tracked.and_then(|f| f.scope).unwrap_or(token.ready.into());
        let at = tracked
            .map(|f| f.at)
            .unwrap_or(Instant::now() - FRAME_MAX_AGE);
        let clean = tracked.is_some_and(|f| {
            f.announced
                && f.scope == drawn
                && capture.pointer_frame().map(|x| x.0) == Some(f.serial)
        }) && !capture.pointer_revoked();
        match p.request(token, current.into(), completed, clean, at, Instant::now()) {
            Ok(()) => {
                r.generation = token.phase;
                r.mode = Mode::NeedVisual(token);
                (true, true)
            }
            Err(crate::remote::pointer_repair_phase::Error::OfferCancelled) => {
                // Exact retired requests are idempotent. In particular, they
                // cannot cancel a newer advisory offer or destroy its proof.
                // Only a current offer that expired during request processing
                // needs a new ticket for the requesting client.
                (false, was_current_offer)
            }
            Err(
                crate::remote::pointer_repair_phase::Error::Scope
                | crate::remote::pointer_repair_phase::Error::Busy
                | crate::remote::pointer_repair_phase::Error::StaleProof,
            ) => (false, true),
            Err(_) => return Err(phase_error().into()),
        }
    };
    if !invalidate_ticket {
        status(writer, RepairStatusKind::RequestCancelled, token, None)?;
        return Ok(None);
    }
    state.clear_ready();
    s.pointer_frame.lock().unwrap().take();
    capture.discard_pointer_frame();
    {
        let mut source = s.state.lock().unwrap();
        source.presented_frame = None;
        source.presented_native_proof = None;
        source.presented_geometry = None;
    }
    status(
        writer,
        if accepted {
            RepairStatusKind::Accepted
        } else {
            RepairStatusKind::RequestCancelled
        },
        token,
        None,
    )?;
    Ok(None)
}
/// Returns true when the regular Ready path must be suppressed.
pub(super) fn poll_active(
    s: &SourceControl,
    capture: &mut HostCapture,
    writer: &SharedWriter,
    state: &mut HostPointerState,
) -> RemoteResult<bool> {
    if !enabled(s) {
        return Ok(false);
    }
    let now = Instant::now();
    let tracked = *s.pointer_frame.lock().unwrap();
    let drawn = s.state.lock().unwrap().presented_native_proof;
    let (mode, cancelled) = {
        let mut r = s.pointer_repair.lock().unwrap();
        let old = r.phase.as_ref().and_then(RepairPhase::offered_token);
        if let Some(p) = r.phase.as_mut() {
            p.check(now).map_err(|_| phase_error())?;
        }
        let cancelled = old.filter(|_| {
            r.phase
                .as_ref()
                .and_then(RepairPhase::offered_token)
                .is_none()
        });
        (r.mode, cancelled)
    };
    if let Some(t) = cancelled {
        status(writer, RepairStatusKind::Cancelled, t, None)?;
    }
    if let Mode::AwaitAck(token, scope) = mode {
        if let Some(frame) = tracked {
            if now.saturating_duration_since(frame.at) >= FRAME_MAX_AGE {
                return Err(phase_error().into());
            }
            if frame.scope == Some(scope.into())
                && drawn == Some(scope.into())
                && capture.pointer_frame().map(|f| f.0) == Some(frame.serial)
            {
                {
                    let mut r = s.pointer_repair.lock().unwrap();
                    r.phase
                        .as_mut()
                        .ok_or_else(phase_error)?
                        .native_proof_ack(token, scope, now)
                        .map_err(|_| phase_error())?;
                    r.mode = Mode::Normal;
                }
                state.ready(scope.into(), frame.serial, frame.at, now)?;
                if let Some(f) = s.pointer_frame.lock().unwrap().as_mut() {
                    f.announced = true;
                }
                status(writer, RepairStatusKind::Ready, token, Some(scope))?;
            }
        }
    }
    Ok(mode != Mode::Normal)
}
pub(super) fn maybe_offer(s: &SourceControl, writer: &SharedWriter) -> RemoteResult<()> {
    if !enabled(s) {
        return Ok(());
    }
    let tracked = *s.pointer_frame.lock().unwrap();
    let drawn = s.state.lock().unwrap().presented_native_proof;
    let Some(frame) = tracked.filter(|f| f.announced && f.scope.is_some() && f.scope == drawn)
    else {
        return Ok(());
    };
    let token = {
        let mut r = s.pointer_repair.lock().unwrap();
        if r.mode != Mode::Normal {
            return Ok(());
        }
        let completed = r.completed_action;
        let last = r.last_input;
        let debt = r.stable_debt;
        let Some(p) = r.phase.as_mut() else {
            return Ok(());
        };
        p.offer(
            frame.scope.unwrap().into(),
            completed,
            frame.at,
            last,
            debt,
            Instant::now(),
        )
        .map_err(|_| phase_error())?
    };
    if let Some(t) = token {
        write_session_record(
            &mut lock_writer(writer),
            MessageKind::WaylandPointerRepairOffer,
            &t.encode()?,
            FRAME_WRITE_DEADLINE,
        )?;
    }
    Ok(())
}
pub(super) fn capture_stamp(s: &SourceControl) -> Option<Result<Stamp, ()>> {
    if !enabled(s) {
        return None;
    }
    let r = s.pointer_repair.lock().unwrap();
    let intent = match r.mode {
        Mode::Normal => Intent::Normal,
        Mode::NeedVisual(t) => Intent::Visual(t),
        Mode::NeedProof(t) => Intent::PostRepair(t),
        _ => return Some(Err(())),
    };
    Some(Ok(Stamp {
        generation: r.generation,
        intent,
    }))
}
pub(super) fn capture_queued(s: &SourceControl, stamp: Stamp) -> RemoteResult<()> {
    let mut r = s.pointer_repair.lock().unwrap();
    if r.generation != stamp.generation {
        return Err(phase_error().into());
    }
    match stamp.intent {
        Intent::Normal => {}
        Intent::Visual(t) => {
            if r.mode != Mode::NeedVisual(t) {
                return Err(phase_error().into());
            }
            r.mode = Mode::VisualQueued(t);
        }
        Intent::PostRepair(t) => {
            if r.mode != Mode::NeedProof(t) {
                return Err(phase_error().into());
            }
            r.mode = Mode::ProofQueued(t);
        }
    }
    Ok(())
}
pub(super) fn permits_write(s: &SourceControl, stamp: Stamp) -> bool {
    let r = s.pointer_repair.lock().unwrap();
    r.generation == stamp.generation
        && match stamp.intent {
            Intent::Normal => r.mode == Mode::Normal,
            Intent::Visual(t) => r.mode == Mode::VisualQueued(t),
            Intent::PostRepair(t) => r.mode == Mode::ProofQueued(t),
        }
}
pub(super) fn observe_debt(s: &SourceControl, eligible: bool) {
    if enabled(s) {
        s.pointer_repair.lock().unwrap().stable_debt = eligible;
    }
}
pub(super) fn flushed(
    s: &SourceControl,
    stamp: Stamp,
    scope: DrawnScope,
    purpose: FramePurpose,
    captured: Instant,
    flushed: Instant,
) -> RemoteResult<()> {
    let mut r = s.pointer_repair.lock().unwrap();
    // An old normal frame may have flushed under the writer barrier just
    // before Request was accepted. It precedes Accepted on the same wire and
    // cannot advance this repair phase. The prewriter stamp blocks later ones.
    if stamp.intent == Intent::Normal {
        return Ok(());
    }
    if r.generation != stamp.generation {
        return Err(phase_error().into());
    }
    match stamp.intent {
        Intent::Normal => {}
        Intent::Visual(t) => {
            if r.mode != Mode::VisualQueued(t) || purpose == FramePurpose::NativeProof {
                return Err(phase_error().into());
            }
            r.phase
                .as_mut()
                .ok_or_else(phase_error)?
                .visual_flushed(
                    t,
                    scope.into(),
                    if purpose == FramePurpose::VisualRepair {
                        VisualKind::Repair
                    } else {
                        VisualKind::DynamicVideo
                    },
                    flushed,
                )
                .map_err(|_| phase_error())?;
            r.mode = Mode::NeedProof(t);
        }
        Intent::PostRepair(t) => {
            if r.mode != Mode::ProofQueued(t) || purpose != FramePurpose::NativeProof {
                return Err(phase_error().into());
            }
            r.phase
                .as_mut()
                .ok_or_else(phase_error)?
                .native_proof(t, scope.into(), captured, flushed)
                .map_err(|_| phase_error())?;
            r.mode = Mode::AwaitAck(t, scope.into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn quiesced() -> (SourceControl, Token, Instant) {
        let source =
            SourceControl::new(vec![(SourceId(1), CaptureArea::Root)], (32, 32).into(), 1).unwrap();
        source.static_lossless.store(true, Ordering::Release);
        source.pointer_enabled.store(true, Ordering::Release);
        let at = Instant::now();
        granted(&source, 7, at);
        let mut r = source.pointer_repair.lock().unwrap();
        let scope = Scope {
            source: 1,
            geometry: 1,
            sequence: 0,
        };
        let p = r.phase.as_mut().unwrap();
        let token = p
            .offer(
                scope,
                0,
                at + Duration::from_secs(2),
                at,
                true,
                at + Duration::from_millis(2100),
            )
            .unwrap()
            .unwrap();
        p.request(
            token,
            scope,
            0,
            true,
            at + Duration::from_secs(2),
            at + Duration::from_millis(2200),
        )
        .unwrap();
        r.mode = Mode::NeedVisual(token);
        r.generation = token.phase;
        drop(r);
        (source, token, at)
    }
    #[test]
    fn phase_stamp_blocks_preaccepted_queued_proof_and_multiple_visual_captures() {
        let (s, t, _) = quiesced();
        assert!(!permits_write(
            &s,
            Stamp {
                generation: 0,
                intent: Intent::Normal
            }
        ));
        let visual = capture_stamp(&s).unwrap().unwrap();
        assert_eq!(visual.intent, Intent::Visual(t));
        capture_queued(&s, visual).unwrap();
        assert!(permits_write(&s, visual));
        assert!(matches!(capture_stamp(&s), Some(Err(()))));
        stopped(&s);
        assert!(!permits_write(&s, visual));
    }
    #[test]
    fn visual_flush_is_required_before_postrepair_capture_and_wire_proof_registration() {
        let (s, t, at) = quiesced();
        let visual = capture_stamp(&s).unwrap().unwrap();
        capture_queued(&s, visual).unwrap();
        assert!(matches!(capture_stamp(&s), Some(Err(()))));
        let scope = DrawnScope {
            source_epoch: 1,
            geometry_epoch: 1,
            frame_sequence: 1,
        };
        flushed(
            &s,
            visual,
            scope,
            FramePurpose::VisualRepair,
            at + Duration::from_millis(2250),
            at + Duration::from_millis(2300),
        )
        .unwrap();
        let proof = capture_stamp(&s).unwrap().unwrap();
        assert_eq!(proof.intent, Intent::PostRepair(t));
        capture_queued(&s, proof).unwrap();
        assert!(
            flushed(
                &s,
                proof,
                DrawnScope {
                    frame_sequence: 2,
                    ..scope
                },
                FramePurpose::NativeProof,
                at + Duration::from_millis(2290),
                at + Duration::from_millis(2400)
            )
            .is_err(),
            "pre-flush capture cannot become postrepair proof"
        );
    }
    #[test]
    fn postrepair_proof_requires_correct_purpose_and_published_new_scope() {
        let (s, t, at) = quiesced();
        let visual = capture_stamp(&s).unwrap().unwrap();
        capture_queued(&s, visual).unwrap();
        let scope = DrawnScope {
            source_epoch: 1,
            geometry_epoch: 1,
            frame_sequence: 1,
        };
        flushed(
            &s,
            visual,
            scope,
            FramePurpose::Video,
            at + Duration::from_millis(2250),
            at + Duration::from_millis(2300),
        )
        .unwrap();
        let proof = capture_stamp(&s).unwrap().unwrap();
        capture_queued(&s, proof).unwrap();
        let fresh = DrawnScope {
            frame_sequence: 2,
            ..scope
        };
        assert!(
            flushed(
                &s,
                proof,
                fresh,
                FramePurpose::VisualRepair,
                at + Duration::from_millis(2350),
                at + Duration::from_millis(2400)
            )
            .is_err()
        );
        flushed(
            &s,
            proof,
            fresh,
            FramePurpose::NativeProof,
            at + Duration::from_millis(2350),
            at + Duration::from_millis(2400),
        )
        .unwrap();
        assert_eq!(
            s.pointer_repair.lock().unwrap().mode,
            Mode::AwaitAck(t, fresh.into())
        );
        assert!(matches!(capture_stamp(&s), Some(Err(()))));
    }
}
