//! Unified control repair coordination. Locks never cross native waits or writer acquisition.
use super::*;
use crate::remote::control_repair_wire::{
    ActionReceipt, RepairStatus, RepairStatusKind, Scope, Token,
};
use crate::remote::pointer_repair_phase::{RepairPhase, VisualKind};

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
    stopped_flush: Option<Stamp>,
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
            stopped_flush: None,
        }
    }
}
fn enabled(s: &SourceControl) -> bool {
    s.static_lossless.load(Ordering::Acquire) && s.control_enabled.load(Ordering::Acquire)
}
fn phase_error() -> io::Error {
    invalid_data("control repair phase invariant failed")
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
        MessageKind::WaylandControlRepairStatus,
        &payload,
        FRAME_WRITE_DEADLINE,
    )?;
    Ok(())
}
pub(super) fn granted(s: &SourceControl, epoch: u64, now: Instant) {
    if enabled(s) {
        let mut r = s.control_repair.lock().unwrap();
        r.phase = Some(RepairPhase::new(
            epoch,
            now + crate::remote::wayland_control_wire::LEASE_DURATION,
        ));
        r.last_input = now;
    }
}
pub(super) fn validate_expiry(s: &SourceControl, now: Instant) -> RemoteResult<()> {
    if !enabled(s) {
        return Ok(());
    }
    let mut r = s.control_repair.lock().unwrap();
    if let Some(phase) = r.phase.as_mut() {
        if !phase.natural_expiry(now) {
            phase.check(now).map_err(|_| phase_error())?;
        }
    }
    Ok(())
}
pub(super) fn stopped(s: &SourceControl) {
    let mut r = s.control_repair.lock().unwrap();
    if let Some(p) = r.phase.as_mut() {
        p.revoke();
    }
    if r.mode != Mode::Stopped {
        r.stopped_flush = match r.mode {
            Mode::Normal => Some(Stamp {
                generation: r.generation,
                intent: Intent::Normal,
            }),
            Mode::VisualQueued(t) => Some(Stamp {
                generation: r.generation,
                intent: Intent::Visual(t),
            }),
            Mode::ProofQueued(t) => Some(Stamp {
                generation: r.generation,
                intent: Intent::PostRepair(t),
            }),
            _ => None,
        };
        r.mode = Mode::Stopped;
    }
}
// Called only after the native adapter returns Accepted and destroys the
// consumed proof. All kinds, including a vertical notch, share this watermark.
fn record_completed_action(
    s: &SourceControl,
    action: crate::remote::wayland_control_wire::ControlAction,
    now: Instant,
) -> RemoteResult<Option<Token>> {
    let mut r = s.control_repair.lock().unwrap();
    if r.mode != Mode::Normal || r.completed_action.checked_add(1) != Some(action.sequence) {
        return Err(phase_error().into());
    }
    r.completed_action = action.sequence;
    r.last_input = now;
    Ok(r.phase.as_mut().and_then(RepairPhase::cancel_offer))
}
pub(super) fn after_action(
    s: &SourceControl,
    writer: &SharedWriter,
    action: crate::remote::wayland_control_wire::ControlAction,
) -> RemoteResult<()> {
    if !enabled(s) {
        return Ok(());
    }
    let cancelled = record_completed_action(s, action, Instant::now())?;
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
        MessageKind::WaylandControlActionCompleted,
        &receipt.encode()?,
        FRAME_WRITE_DEADLINE,
    )?;
    Ok(())
}
pub(super) fn intercept(
    s: &SourceControl,
    capture: &mut HostCapture,
    writer: &SharedWriter,
    state: &mut HostControl,
    command: ControlCommand,
) -> RemoteResult<Option<ControlCommand>> {
    let (token, decline) = match command {
        ControlCommand::RepairRequest(t) => (t, false),
        ControlCommand::RepairDecline(t) => (t, true),
        other => {
            if enabled(s) && s.control_repair.lock().unwrap().mode != Mode::Normal {
                return Err(phase_error().into());
            }
            return Ok(Some(other));
        }
    };
    if !enabled(s) {
        return Err(invalid_data("control repair not negotiated").into());
    }
    if decline {
        {
            let mut r = s.control_repair.lock().unwrap();
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
        let mut r = s.control_repair.lock().unwrap();
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
                && capture.control_frame().map(|x| x.0) == Some(f.serial)
        }) && !capture.control_revoked()
            && state.held_empty();
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
    s.pointer_frame.lock().unwrap().take();
    capture.discard_control_frame();
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
    state: &mut HostControl,
) -> RemoteResult<bool> {
    if !enabled(s) {
        return Ok(false);
    }
    let now = Instant::now();
    // control::process checked earlier, but its deadline can be crossed before
    // this phase poll. Only the original lease expiry takes the readonly path;
    // an earlier accepted-phase deadline remains an error below.
    let natural_expiry = s
        .control_repair
        .lock()
        .unwrap()
        .phase
        .as_ref()
        .is_some_and(|p| p.natural_expiry(now));
    if state.epoch().is_some() && !state.active(now) && natural_expiry {
        control::stop(s, capture, writer, state)?;
        return Ok(true);
    }

    let tracked = *s.pointer_frame.lock().unwrap();
    let drawn = s.state.lock().unwrap().presented_native_proof;
    let (mode, cancelled) = {
        let mut r = s.control_repair.lock().unwrap();
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
                && capture.control_frame().map(|f| f.0) == Some(frame.serial)
            {
                {
                    let mut r = s.control_repair.lock().unwrap();
                    r.phase
                        .as_mut()
                        .ok_or_else(phase_error)?
                        .native_proof_ack(token, scope, now)
                        .map_err(|_| phase_error())?;
                    r.mode = Mode::Normal;
                }
                if let Some(f) = s.pointer_frame.lock().unwrap().as_mut() {
                    f.announced = true;
                }
                status(writer, RepairStatusKind::Ready, token, Some(scope))?;
            }
        }
    }
    Ok(mode != Mode::Normal)
}
pub(super) fn maybe_offer(
    s: &SourceControl,
    writer: &SharedWriter,
    state: &HostControl,
) -> RemoteResult<()> {
    if !enabled(s) || !state.held_empty() {
        return Ok(());
    }
    let tracked = *s.pointer_frame.lock().unwrap();
    let drawn = s.state.lock().unwrap().presented_native_proof;
    let Some(frame) = tracked.filter(|f| f.announced && f.scope.is_some() && f.scope == drawn)
    else {
        return Ok(());
    };
    let token = {
        let mut r = s.control_repair.lock().unwrap();
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
            MessageKind::WaylandControlRepairOffer,
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
    let r = s.control_repair.lock().unwrap();
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
    let mut r = s.control_repair.lock().unwrap();
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
    let r = s.control_repair.lock().unwrap();
    r.generation == stamp.generation
        && match stamp.intent {
            Intent::Normal => r.mode == Mode::Normal,
            Intent::Visual(t) => r.mode == Mode::VisualQueued(t),
            Intent::PostRepair(t) => r.mode == Mode::ProofQueued(t),
        }
}
pub(super) fn observe_debt(s: &SourceControl, eligible: bool) {
    if enabled(s) {
        s.control_repair.lock().unwrap().stable_debt = eligible;
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
    let mut r = s.control_repair.lock().unwrap();
    // An old normal frame may have flushed under the writer barrier just
    // before Request was accepted. It precedes Accepted on the same wire and
    // cannot advance this repair phase. The prewriter stamp blocks later ones.
    if r.mode == Mode::Stopped {
        let purpose_matches = match stamp.intent {
            Intent::Visual(_) => purpose != FramePurpose::NativeProof,
            Intent::PostRepair(_) => purpose == FramePurpose::NativeProof,
            Intent::Normal => true,
        };
        if r.stopped_flush == Some(stamp) && purpose_matches {
            // This exact record passed the writer fence before revocation.
            // Drain its completion once, without restoring any phase or ticket.
            r.stopped_flush = None;
            return Ok(());
        }
        return Err(phase_error().into());
    }
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
            if r.phase.as_ref().is_some_and(|p| p.natural_expiry(flushed)) {
                // The exact queued write completed across the original lease
                // boundary. Capture-loop revocation follows; never advance
                // repair, issue Ready or replace an earlier phase timeout.
                return Ok(());
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
            if r.phase.as_ref().is_some_and(|p| p.natural_expiry(flushed)) {
                // The exact queued write completed across the original lease
                // boundary. Capture-loop revocation follows; never advance
                // repair, issue Ready or replace an earlier phase timeout.
                return Ok(());
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
        source.control_enabled.store(true, Ordering::Release);
        let at = Instant::now();
        granted(&source, 7, at);
        let mut r = source.control_repair.lock().unwrap();
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
            s.control_repair.lock().unwrap().mode,
            Mode::AwaitAck(t, fresh.into())
        );
        assert!(matches!(capture_stamp(&s), Some(Err(()))));
    }
    #[test]
    fn terminal_readonly_drops_old_stamps_and_drains_only_one_known_flush() {
        for postproof in [false, true] {
            let (s, token, at) = quiesced();
            let stamp = Stamp {
                generation: token.phase,
                intent: if postproof {
                    Intent::PostRepair(token)
                } else {
                    Intent::Visual(token)
                },
            };
            s.control_repair.lock().unwrap().mode = if postproof {
                Mode::ProofQueued(token)
            } else {
                Mode::VisualQueued(token)
            };
            // The production stop_pointer disables input before marking terminal.
            s.control_enabled.store(false, Ordering::Release);
            stopped(&s);
            assert_eq!(capture_stamp(&s), None);
            assert!(!permits_write(&s, stamp));
            let scope: DrawnScope = token.ready.into();
            let purpose = if postproof {
                FramePurpose::NativeProof
            } else {
                FramePurpose::VisualRepair
            };
            let bad = Stamp {
                generation: stamp.generation + 1,
                ..stamp
            };
            assert!(flushed(&s, bad, scope, purpose, at, at).is_err());
            assert!(flushed(&s, stamp, scope, purpose, at, at).is_ok());
            assert!(flushed(&s, stamp, scope, purpose, at, at).is_err());
            assert_eq!(s.control_repair.lock().unwrap().mode, Mode::Stopped);
            assert_eq!(capture_stamp(&s), None);
        }
    }
    #[test]
    fn early_phase_timeout_is_not_a_readonly_completion() {
        let (s, token, at) = quiesced();
        let stamp = capture_stamp(&s).unwrap().unwrap();
        capture_queued(&s, stamp).unwrap();
        assert!(
            flushed(
                &s,
                stamp,
                DrawnScope {
                    frame_sequence: token.ready.sequence + 1,
                    ..token.ready.into()
                },
                FramePurpose::VisualRepair,
                at,
                at + Duration::from_secs(8)
            )
            .is_err()
        );
        assert_ne!(s.control_repair.lock().unwrap().mode, Mode::Stopped);
    }
    #[test]
    fn native_lease_expiry_validation_keeps_earlier_phase_failure() {
        let (s, _, at) = quiesced();
        assert!(validate_expiry(&s, at + Duration::from_secs(31)).is_err());
        let s =
            SourceControl::new(vec![(SourceId(1), CaptureArea::Root)], (32, 32).into(), 1).unwrap();
        s.static_lossless.store(true, Ordering::Release);
        s.control_enabled.store(true, Ordering::Release);
        granted(&s, 1, at);
        assert!(validate_expiry(&s, at + Duration::from_secs(31)).is_ok());
    }
    #[test]
    fn all_stop_triggers_preserve_earlier_fault_but_clean_local_revoke_is_ok() {
        for remote_trigger in [false, true] {
            for before_timeout in [false, true] {
                let (s, _, at) = quiesced();
                s.control_revoke.store(remote_trigger, Ordering::Release);
                // Either a local revoke or native timer may arrive while the
                // host's original 30-second lease is still active.
                let now = at + Duration::from_secs(if before_timeout { 3 } else { 29 });
                let result = validate_expiry(&s, now);
                s.control_enabled.store(false, Ordering::Release);
                stopped(&s);
                assert_eq!(result.is_ok(), before_timeout);
                assert_eq!(capture_stamp(&s), None);
                assert_eq!(s.control_repair.lock().unwrap().mode, Mode::Stopped);
            }
        }
    }
    #[test]
    fn key_edges_and_pointer_share_exact_native_completion_order() {
        use crate::remote::wayland_control_wire::{ControlAction, ControlActionKind};
        let (s, token, at) = quiesced();
        s.control_repair.lock().unwrap().mode = Mode::Normal;
        for (index, kind) in [
            ControlActionKind::Key {
                code: 30,
                pressed: true,
            },
            ControlActionKind::Key {
                code: 30,
                pressed: false,
            },
            ControlActionKind::ClickLeft { x: 4, y: 5 },
        ]
        .into_iter()
        .enumerate()
        {
            let action = ControlAction {
                lease_epoch: token.lease,
                sequence: index as u64 + 1,
                scope: token.ready.into(),
                kind,
            };
            record_completed_action(&s, action, at).unwrap();
            assert_eq!(
                s.control_repair.lock().unwrap().completed_action,
                action.sequence
            );
            assert!(record_completed_action(&s, action, at).is_err());
        }
        assert!(s.pointer_frame.lock().unwrap().is_none());
    }
    #[test]
    fn accepted_repair_blocks_key_release_completion_without_synthetic_release() {
        use crate::remote::wayland_control_wire::{ControlAction, ControlActionKind};
        let (s, token, at) = quiesced();
        let action = ControlAction {
            lease_epoch: token.lease,
            sequence: 1,
            scope: token.ready.into(),
            kind: ControlActionKind::Key {
                code: 30,
                pressed: false,
            },
        };
        assert!(record_completed_action(&s, action, at).is_err());
        assert_eq!(s.control_repair.lock().unwrap().completed_action, 0);
    }
}
