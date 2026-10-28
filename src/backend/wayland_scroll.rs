//! Preserve precision for compositor UI and fence interrupted client scrolls.

use crate::backend::api::{BackendEvent, HitTarget};
use crate::backend::scroll::{
    PointerScroll, ScrollMode, ScrollPhase, ScrollSource, ScrollTermination,
};
use crate::backend::wayland::state::JwmWaylandState;
use smithay::backend::input::{Axis, AxisSource, Device, InputBackend, PointerAxisEvent};
use smithay::input::pointer::{AxisFrame, MotionEvent, PointerHandle, PointerTarget};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

fn send_stop(state: &mut JwmWaylandState, termination: ScrollTermination<WlSurface>) {
    if termination.wm_owned {
        cancel_wm_device(state, &termination.device, termination.time);
        return;
    }
    let Some(surface) = termination.focus else {
        return;
    };
    let source = match termination.source {
        ScrollSource::Finger => AxisSource::Finger,
        ScrollSource::Continuous => AxisSource::Continuous,
        ScrollSource::Wheel => return,
    };
    let mut frame = AxisFrame::new(smithay::backend::input::InputTime::from_millis(
        termination.time,
    ))
    .source(source);
    if termination.axes.0 {
        frame = frame.stop(Axis::Horizontal);
    }
    if termination.axes.1 {
        frame = frame.stop(Axis::Vertical);
    }
    let seat = state.seat.clone();
    PointerTarget::axis(&surface, &seat, state, frame);
    PointerTarget::frame(&surface, &seat, state);
    state.request_client_flush();
}

/// A physical hold or removal also stops inertia after its axis sequence has
/// already ended. It is a WM cancellation packet, never a client axis event.
pub(crate) fn cancel_wm_device(state: &mut JwmWaylandState, device: &str, time: u32) {
    let scroll = PointerScroll {
        device: device.to_owned(),
        phase: ScrollPhase::Cancelled,
        time,
        owner_epoch: state.scroll_routes.epoch(),
        ..Default::default()
    };
    state
        .pending_events
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push_back(BackendEvent::PointerScroll {
            target: HitTarget::Background { output: None },
            state: 0,
            root_x: state.pointer_location.x,
            root_y: state.pointer_location.y,
            scroll,
        });
}

pub(crate) fn cancel(state: &mut JwmWaylandState) {
    for termination in state.scroll_routes.invalidate() {
        send_stop(state, termination);
    }
}

pub(crate) fn prepare_focus_change(state: &mut JwmWaylandState, focus: Option<&WlSurface>) {
    for termination in state.scroll_routes.change_focus(focus) {
        send_stop(state, termination);
    }
}

/// Keep an axis sequence's termination ordered before its wl_pointer.leave.
pub(crate) fn motion(
    pointer: &PointerHandle<JwmWaylandState>,
    state: &mut JwmWaylandState,
    focus: Option<(WlSurface, Point<f64, Logical>)>,
    event: &MotionEvent,
) {
    if !pointer.is_grabbed() {
        prepare_focus_change(state, focus.as_ref().map(|(surface, _)| surface));
    }
    pointer.motion(state, focus, event);
}

pub(crate) fn set_mode(state: &mut JwmWaylandState, mode: ScrollMode) {
    for termination in state.scroll_routes.set_mode(mode) {
        send_stop(state, termination);
    }
}

#[cfg(feature = "backend-wayland-udev")]
pub(crate) fn remove_device(state: &mut JwmWaylandState, device: &str) {
    if let Some(termination) = state.scroll_routes.remove(device) {
        send_stop(state, termination);
    }
    cancel_wm_device(state, device, 0);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn axis<B: InputBackend>(
    state: &mut JwmWaylandState,
    event: &impl PointerAxisEvent<B>,
    mode: ScrollMode,
    native: bool,
    pending: &Arc<Mutex<VecDeque<BackendEvent>>>,
    target: HitTarget,
    modifiers: u16,
    x: f64,
    y: f64,
) {
    set_mode(state, mode);
    let frame = crate::backend::wayland_axis::pointer_axis_frame(event, native);
    let source = match event.source() {
        AxisSource::Finger => ScrollSource::Finger,
        AxisSource::Continuous => ScrollSource::Continuous,
        _ => ScrollSource::Wheel,
    };
    let pointer = state.seat.get_pointer();
    let focus = if mode.belongs_to_wm() {
        None
    } else {
        pointer.as_ref().and_then(|p| p.current_focus())
    };
    let mut packet = PointerScroll {
        device: event.device().id(),
        delta: frame.axis,
        v120: event
            .amount_v120(Axis::Horizontal)
            .or(event.amount_v120(Axis::Vertical))
            .map(|_| {
                (
                    event.amount_v120(Axis::Horizontal).unwrap_or(0.0),
                    event.amount_v120(Axis::Vertical).unwrap_or(0.0),
                )
            }),
        source,
        stop: frame.stop,
        time: event.time().millis(),
        ..Default::default()
    };
    let deliver = state.scroll_routes.route(&mut packet, focus, !native);
    for termination in state.scroll_routes.take_terminations() {
        send_stop(state, termination);
    }
    if !deliver {
        return;
    }
    if mode.belongs_to_wm() {
        pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(BackendEvent::PointerScroll {
                target,
                state: modifiers,
                root_x: x,
                root_y: y,
                scroll: packet,
            });
    } else if let Some(pointer) = pointer {
        pointer.axis(state, frame);
        pointer.frame(state);
    }
}
