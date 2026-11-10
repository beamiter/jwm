//! One host authority and action order for sequential keyboard/pointer control.
use super::*;
use crate::remote::wayland_control_wire::{
    ControlAction, ControlActionKind, ControlRequest, ControlStatus, ControlStatusKind,
    LEASE_DURATION, MAX_SEQUENCE,
};
pub(super) enum ControlCommand {
    Request(ControlRequest),
    Action(ControlAction),
}
#[derive(Default)]
pub(super) struct HostControl {
    request: Option<ControlRequest>,
    epoch: Option<u64>,
    ended: bool,
    started: Option<Instant>,
    sequence: u64,
    held: std::collections::BTreeSet<u16>,
    tokens: f64,
    rate_at: Option<Instant>,
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
fn stop(
    sources: &SourceControl,
    capture: &mut HostCapture,
    writer: &SharedWriter,
    state: &mut HostControl,
) -> RemoteResult<()> {
    if state.ended {
        return Ok(());
    }
    state.ended = true;
    state.held.clear();
    sources.control_revoke.store(true, Ordering::Release);
    sources.control_enabled.store(false, Ordering::Release);
    sources.control_queue.lock().unwrap().clear();
    sources.pointer_frame.lock().unwrap().take();
    capture.revoke_control();
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
        return stop(sources, capture, writer, state);
    }
    let tracked = *sources.pointer_frame.lock().unwrap();
    let drawn = sources.state.lock().unwrap().presented_frame;
    let command = sources.control_queue.lock().unwrap().pop_front();
    if let Some(command) = command {
        let result = (|| -> RemoteResult<()> {
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
                    sources.pointer_frame.lock().unwrap().take();
                    status(
                        writer,
                        action.scope,
                        action.lease_epoch,
                        ControlStatusKind::Waiting,
                    )?;
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
        }
    }
    Ok(())
}
