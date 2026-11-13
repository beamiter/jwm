//! One host authority and action order for sequential keyboard/pointer control.
use super::*;
use crate::remote::resumable_control::{
    Binding, HostSession, Kind as ResumableKind, LocalOptIn, Message as ResumableMessage,
    Negotiated, Record as ResumableRecord, VERSION,
};
use crate::remote::wayland_control_wire::{
    ControlAction, ControlActionKind, ControlRequest, ControlStatus, ControlStatusKind,
    LEASE_DURATION, MAX_SEQUENCE,
};
pub(super) enum ControlCommand {
    Request(ControlRequest),
    Action(ControlAction),
    Resumable(ResumableMessage),
    RepairRequest(super::super::pointer_repair_phase::Token),
    RepairDecline(super::super::pointer_repair_phase::Token),
}
#[derive(Default)]
pub(super) struct HostControl {
    request: Option<ControlRequest>,
    epoch: Option<u64>,
    ended: bool,
    started: Option<Instant>,
    resumable: Option<HostSession>,
    sequence: u64,
    held: std::collections::BTreeSet<u16>,
    tokens: f64,
    rate_at: Option<Instant>,
}
impl HostControl {
    pub(super) fn held_empty(&self) -> bool {
        self.held.is_empty()
    }
    pub(super) fn epoch(&self) -> Option<u64> {
        self.epoch
    }
    pub(super) fn active(&self, now: Instant) -> bool {
        !self.ended
            && self
                .started
                .is_some_and(|at| now.saturating_duration_since(at) < LEASE_DURATION)
    }
}
fn status(
    writer: &SharedWriter,
    scope: DrawnScope,
    epoch: u64,
    status: ControlStatusKind,
) -> RemoteResult<()> {
    let mut writer = lock_writer(writer);
    writer.write_message(
        MessageKind::WaylandControlStatus,
        &ControlStatus {
            scope,
            lease_epoch: epoch,
            status,
        }
        .encode()?,
    )?;
    writer.flush()?;
    Ok(())
}
fn resumable_status(writer: &SharedWriter, record: ResumableRecord) -> RemoteResult<()> {
    let mut writer = lock_writer(writer);
    writer.write_message(MessageKind::WaylandResumableControl, &record.encode()?)?;
    writer.flush()?;
    Ok(())
}
pub(super) fn stop(
    sources: &SourceControl,
    capture: &mut HostCapture,
    writer: &SharedWriter,
    state: &mut HostControl,
) -> RemoteResult<()> {
    if state.ended {
        return Ok(());
    }
    state.ended = true;
    if let Some(session) = &mut state.resumable {
        session.revoke();
    }
    state.held.clear();
    sources.control_revoke.store(true, Ordering::Release);
    sources.control_enabled.store(false, Ordering::Release);
    sources
        .resumable_control_enabled
        .store(false, Ordering::Release);
    sources.control_queue.lock().unwrap().clear();
    sources.pointer_frame.lock().unwrap().take();
    capture.revoke_control();
    control_repair::stopped(sources);
    {
        let mut s = sources.state.lock().unwrap();
        s.presented_frame = None;
        s.presented_native_proof = None;
        s.presented_geometry = None;
    }
    if let Some(request) = state.request {
        status(
            writer,
            request.scope,
            state.epoch.unwrap_or(0),
            if state.epoch.is_some() {
                ControlStatusKind::Revoked
            } else {
                ControlStatusKind::Denied
            },
        )?;
    }
    Ok(())
}
pub(super) fn process(
    sources: &SourceControl,
    capture: &mut HostCapture,
    writer: &SharedWriter,
    state: &mut HostControl,
    size: (u16, u16),
) -> RemoteResult<()> {
    if !sources.control_enabled.load(Ordering::Acquire) {
        return Ok(());
    }
    let now = Instant::now();
    if sources.control_revoke.load(Ordering::Acquire)
        || capture.control_revoked()
        || state
            .started
            .is_some_and(|at| now.saturating_duration_since(at) >= LEASE_DURATION)
    {
        let validation = control_repair::validate_expiry(sources, now);
        stop(sources, capture, writer, state)?;
        return validation;
    }
    if let Some(session) = &mut state.resumable
        && let Err(error) = session.check(now)
    {
        stop(sources, capture, writer, state)?;
        return Err(error.into());
    }
    let tracked = *sources.pointer_frame.lock().unwrap();
    let drawn = sources.state.lock().unwrap().presented_native_proof;
    let command = sources.control_queue.lock().unwrap().pop_front();
    if let Some(command) = command {
        let command = match control_repair::intercept(sources, capture, writer, state, command) {
            Ok(Some(command)) => command,
            Ok(None) => return Ok(()),
            Err(error) => {
                stop(sources, capture, writer, state)?;
                return Err(error);
            }
        };
        let result = (|| -> RemoteResult<()> {
            // Decode and generation-check every new-mode action before it may
            // enter the existing bounded native action path. Legacy actions
            // are never accepted as an untagged fallback in resumable mode.
            let command = match command {
                ControlCommand::Resumable(ResumableMessage::Action(envelope)) => {
                    if !sources.resumable_control_enabled.load(Ordering::Acquire) {
                        return Err(invalid_data("resumable control not negotiated").into());
                    }
                    state
                        .resumable
                        .as_mut()
                        .ok_or_else(|| invalid_data("resumable action before grant"))?
                        .accept_action(&envelope, now)?;
                    ControlCommand::Action(ControlAction::decode(envelope.control_action_bytes())?)
                }
                ControlCommand::Action(_)
                    if sources.resumable_control_enabled.load(Ordering::Acquire) =>
                {
                    return Err(invalid_data("untagged action in resumable control session").into());
                }
                other => other,
            };
            match command {
                ControlCommand::Request(request) => {
                    if state.request.is_some() || state.ended {
                        return Err(invalid_data("control grant cannot repeat").into());
                    }
                    state.request = Some(request);
                    let frame =
                        tracked.ok_or_else(|| invalid_data("missing control capture proof"))?;
                    if frame.scope != Some(request.scope)
                        || drawn != Some(request.scope)
                        || capture.control_frame().map(|f| f.0) != Some(frame.serial)
                        || now.saturating_duration_since(frame.at) >= FRAME_MAX_AGE
                    {
                        return Err(
                            invalid_data("control grant requires exact fresh drawn frame").into(),
                        );
                    }
                    let epoch = capture.arm_control(request.keymap_digest)?;
                    state.epoch = Some(epoch);
                    state.started = Some(now);
                    if sources.resumable_control_enabled.load(Ordering::Acquire) {
                        // This bit is set only after independent local and peer
                        // opt-ins have both passed the hello negotiation.
                        let permission = Negotiated::new(LocalOptIn::Explicit, Some(VERSION))?;
                        state.resumable = Some(HostSession::after_native_arm(
                            permission,
                            Binding {
                                session: epoch,
                                source_epoch: request.scope.source_epoch,
                                geometry_epoch: request.scope.geometry_epoch,
                            },
                            now,
                            now + LEASE_DURATION,
                            request.scope.frame_sequence,
                        )?);
                    }
                    control_repair::granted(sources, epoch, now);
                    state.tokens = 32.0;
                    state.rate_at = Some(now);
                    // Initial native arm consumed its frame; keyboard approval is not pointer Ready.
                    sources.pointer_frame.lock().unwrap().take();
                    status(writer, request.scope, epoch, ControlStatusKind::Granted)?;
                }
                ControlCommand::Action(action) => {
                    let request = state
                        .request
                        .ok_or_else(|| invalid_data("control action before request"))?;
                    if state.epoch != Some(action.lease_epoch)
                        || action.sequence != state.sequence + 1
                        || action.sequence > MAX_SEQUENCE
                        || (action.scope.source_epoch, action.scope.geometry_epoch)
                            != (request.scope.source_epoch, request.scope.geometry_epoch)
                    {
                        return Err(
                            invalid_data("control action epoch or global order mismatch").into(),
                        );
                    }
                    let elapsed = now
                        .saturating_duration_since(state.rate_at.unwrap())
                        .as_secs_f64();
                    state.tokens = (state.tokens + elapsed * 120.0).min(32.0);
                    state.rate_at = Some(now);
                    if state.tokens < 1.0 {
                        return Err(invalid_data("control action rate exceeded").into());
                    }
                    state.tokens -= 1.0;
                    let serial = match action.kind {
                        ControlActionKind::Key { code, pressed } => {
                            if action.scope != request.scope {
                                return Err(invalid_data("control key arm scope mismatch").into());
                            }
                            #[cfg(feature = "remote-wayland")]
                            if !crate::remote_keyboard_protocol::keyboard_key_allowed(u32::from(
                                code,
                            )) {
                                return Err(invalid_data("control key not allowed").into());
                            }
                            if (pressed && (state.held.len() >= 32 || !state.held.insert(code)))
                                || (!pressed && !state.held.remove(&code))
                            {
                                return Err(invalid_data("unbalanced control key").into());
                            }
                            None
                        }
                        ControlActionKind::Motion { x, y }
                        | ControlActionKind::ClickLeft { x, y }
                        | ControlActionKind::ScrollUp { x, y }
                        | ControlActionKind::ScrollDown { x, y }
                        | ControlActionKind::ScrollLeft { x, y }
                        | ControlActionKind::ScrollRight { x, y } => {
                            if matches!(
                                action.kind,
                                ControlActionKind::ScrollUp { .. }
                                    | ControlActionKind::ScrollDown { .. }
                            ) && !sources.scroll_enabled.load(Ordering::Acquire)
                            {
                                return Err(invalid_data("control scroll not authorized").into());
                            }
                            if matches!(
                                action.kind,
                                ControlActionKind::ScrollLeft { .. }
                                    | ControlActionKind::ScrollRight { .. }
                            ) && !sources.horizontal_scroll_enabled.load(Ordering::Acquire)
                            {
                                return Err(invalid_data(
                                    "control horizontal scroll not authorized",
                                )
                                .into());
                            }
                            if !state.held.is_empty() || x >= size.0 || y >= size.1 {
                                return Err(invalid_data(
                                    "control pointer conflicts with keys or bounds",
                                )
                                .into());
                            }
                            let frame = tracked
                                .ok_or_else(|| invalid_data("control pointer lacks proof"))?;
                            if !frame.announced
                                || frame.scope != Some(action.scope)
                                || drawn != Some(action.scope)
                                || capture.control_frame().map(|f| f.0) != Some(frame.serial)
                                || now.saturating_duration_since(frame.at) >= FRAME_MAX_AGE
                            {
                                return Err(invalid_data(
                                    "control pointer lacks exact fresh Ready",
                                )
                                .into());
                            }
                            Some(frame.serial)
                        }
                    };
                    capture.control_action(&action, serial)?;
                    state.sequence = action.sequence;
                    control_repair::after_action(sources, writer, action)?;
                    sources.pointer_frame.lock().unwrap().take();
                    status(
                        writer,
                        action.scope,
                        action.lease_epoch,
                        ControlStatusKind::Waiting,
                    )?;
                }
                ControlCommand::Resumable(ResumableMessage::Lifecycle(record)) => {
                    if !sources.resumable_control_enabled.load(Ordering::Acquire) {
                        return Err(invalid_data("resumable control not negotiated").into());
                    }
                    let session = state
                        .resumable
                        .as_mut()
                        .ok_or_else(|| invalid_data("resumable transition before grant"))?;
                    let ack = match record.kind {
                        ResumableKind::Pause => {
                            let ticket = session.begin_pause(record, now)?;
                            sources.pointer_frame.lock().unwrap().take();
                            capture.discard_control_frame();
                            {
                                let mut proof = sources.state.lock().unwrap();
                                proof.presented_native_proof = None;
                                proof.presented_frame = None;
                                proof.presented_geometry = None;
                            }
                            let released = capture
                                .control_transition(
                                    record.binding.session,
                                    record.sequence,
                                    record.generation,
                                    None,
                                )
                                .map_err(|error| io::Error::other(error.to_string()));
                            let ack = session.finish_pause(ticket, Instant::now(), released)?;
                            // The native paused event confirms actual releases;
                            // neither local queue disposal nor sending pause does.
                            state.held.clear();
                            ack
                        }
                        ResumableKind::Resume => {
                            let frame = tracked
                                .ok_or_else(|| invalid_data("resume lacks fresh captured proof"))?;
                            let scope = frame
                                .scope
                                .ok_or_else(|| invalid_data("resume lacks frame scope"))?;
                            if scope.source_epoch != record.binding.source_epoch
                                || scope.geometry_epoch != record.binding.geometry_epoch
                                || scope.frame_sequence != record.proof
                                || drawn != Some(scope)
                                || capture.control_frame().map(|proof| proof.0)
                                    != Some(frame.serial)
                                || now.saturating_duration_since(frame.at) >= FRAME_MAX_AGE
                            {
                                return Err(invalid_data(
                                    "resume requires exact fresh drawn proof",
                                )
                                .into());
                            }
                            session.resume(record, now, |_| {
                                capture
                                    .control_transition(
                                        record.binding.session,
                                        record.sequence,
                                        record.generation,
                                        Some(frame.serial),
                                    )
                                    .map_err(|error| io::Error::other(error.to_string()))
                            })?
                        }
                        _ => {
                            return Err(
                                invalid_data("unexpected resumable lifecycle record").into()
                            );
                        }
                    };
                    // Waiting for the native event must not let the original
                    // host deadline lapse before an acknowledgement is sent.
                    session.check(Instant::now())?;
                    state.sequence = record.sequence;
                    sources.pointer_frame.lock().unwrap().take();
                    capture.discard_control_frame();
                    resumable_status(writer, ack)?;
                }
                ControlCommand::Resumable(ResumableMessage::Action(_)) => {
                    unreachable!("resumable actions normalized above")
                }
                ControlCommand::RepairRequest(_) | ControlCommand::RepairDecline(_) => {
                    unreachable!("repair commands intercepted")
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            stop(sources, capture, writer, state)?;
            return Err(error);
        }
        return Ok(());
    }
    // PointerReady remains a proof-availability announcement while paused,
    // not input authority. The resumable session gate rejects every action
    // until an explicit Resume consumes the exact fresh drawn proof.
    match control_repair::poll_active(sources, capture, writer, state) {
        Ok(true) => return Ok(()),
        Ok(false) => {}
        Err(error) => {
            stop(sources, capture, writer, state)?;
            return Err(error);
        }
    }
    if let Some(frame) = tracked {
        if now.saturating_duration_since(frame.at) >= FRAME_MAX_AGE {
            sources.pointer_frame.lock().unwrap().take();
            capture.discard_control_frame();
            if let (Some(epoch), Some(scope)) = (state.epoch, frame.scope) {
                status(writer, scope, epoch, ControlStatusKind::Waiting)?;
            }
        } else if state.epoch.is_some()
            && state.held.is_empty()
            && !frame.announced
            && frame.scope.is_some()
            && drawn == frame.scope
        {
            sources
                .pointer_frame
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .announced = true;
            status(
                writer,
                frame.scope.unwrap(),
                state.epoch.unwrap(),
                ControlStatusKind::PointerReady,
            )?;
            #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
            if let Some((_, probe)) = &sources.proof_delta {
                probe.record_stage(
                    frame.serial,
                    frame.scope.unwrap(),
                    Instant::now(),
                    ProofStageKind::ReadyPublished,
                );
            }
        }
    }
    control_repair::maybe_offer(sources, writer, state)?;
    Ok(())
}
