//! Preserve native Tao wheel units while accounting for GTK's pulse phases.

use tao::event::{MouseScrollDelta, TouchPhase};
use xbar_core::presentation::Point;
use xbar_core::render::cairo::PointerInput;
use xbar_core::{ScrollDelta, ScrollPhase};

pub fn pointer_scroll(
    point: Point,
    delta: MouseScrollDelta,
    phase: TouchPhase,
    scale_factor: f64,
) -> PointerInput {
    let delta = match delta {
        MouseScrollDelta::LineDelta(x, y) => ScrollDelta::Lines {
            x: f64::from(x),
            y: f64::from(y),
        },
        MouseScrollDelta::PixelDelta(position) => {
            ScrollDelta::physical_pixels(position.x, position.y, scale_factor)
        }
        _ => ScrollDelta::Lines { x: 0.0, y: 0.0 },
    };
    let phase = match phase {
        TouchPhase::Started => ScrollPhase::Started,
        TouchPhase::Moved => ScrollPhase::Moved,
        // Tao's GTK backend marks each discrete GDK scroll pulse Ended.
        // It carries the notch itself, not a zero-delta touchpad terminator.
        // Treat that packet as a fresh gesture so ordinary wheels keep working.
        TouchPhase::Ended
            if cfg!(target_os = "linux")
                && matches!(delta,
                    ScrollDelta::Lines { x, y }
                        if x.is_finite() && y.is_finite() && (x != 0.0 || y != 0.0)
                            && x.fract() == 0.0 && y.fract() == 0.0
                ) =>
        {
            ScrollPhase::Started
        }
        TouchPhase::Ended => ScrollPhase::Ended,
        TouchPhase::Cancelled => ScrollPhase::Cancelled,
        _ => ScrollPhase::Moved,
    };
    PointerInput::Wheel {
        point,
        delta,
        phase,
    }
}

// GTK discrete scroll events carry a direction and zero delta fields. Tao's
// Linux WindowEvent loses that direction. Read the original event once and
// route it through the same core normalizer as every other frontend.
#[cfg(target_os = "linux")]
#[derive(Default)]
struct GtkScrollState {
    last: Option<std::time::Instant>,
    smooth: bool,
}

#[cfg(target_os = "linux")]
fn gtk_scroll_input(
    state: &mut GtkScrollState,
    point: Point,
    direction: gtk::gdk::ScrollDirection,
    deltas: (f64, f64),
    stop: bool,
    now: std::time::Instant,
    idle: std::time::Duration,
) -> Option<PointerInput> {
    use gtk::gdk::ScrollDirection;
    let (x, y, phase, smooth) = match direction {
        ScrollDirection::Up => (0.0, 1.0, ScrollPhase::Started, false),
        ScrollDirection::Down => (0.0, -1.0, ScrollPhase::Started, false),
        ScrollDirection::Left => (1.0, 0.0, ScrollPhase::Started, false),
        ScrollDirection::Right => (-1.0, 0.0, ScrollPhase::Started, false),
        ScrollDirection::Smooth => {
            let expired = state
                .last
                .is_none_or(|last| now.saturating_duration_since(last) >= idle);
            let phase = if stop {
                ScrollPhase::Ended
            } else if !state.smooth || expired {
                ScrollPhase::Started
            } else {
                ScrollPhase::Moved
            };
            (-deltas.0, -deltas.1, phase, true)
        }
        _ => return None,
    };
    state.last = Some(now);
    state.smooth = smooth;
    if !smooth {
        return Some(pointer_scroll(
            point,
            MouseScrollDelta::LineDelta(x as f32, y as f32),
            TouchPhase::Started,
            1.0,
        ));
    }
    Some(PointerInput::Wheel {
        point,
        delta: ScrollDelta::Lines { x, y },
        phase,
    })
}

#[cfg(target_os = "linux")]
pub fn install_gtk_bridge<T: Send + 'static>(
    window: &tao::window::Window,
    proxy: tao::event_loop::EventLoopProxy<T>,
    wrap: fn(PointerInput) -> T,
    policy: xbar_core::ScrollPolicy,
) {
    use gtk::prelude::*;
    use std::{cell::RefCell, time::Instant};
    use tao::platform::unix::WindowExtUnix;
    let state = RefCell::new(GtkScrollState::default());
    let idle = if policy.idle.is_zero() {
        xbar_core::SCROLL_GESTURE_IDLE
    } else {
        policy.idle
    };
    window
        .gtk_window()
        .add_events(gtk::gdk::EventMask::SCROLL_MASK | gtk::gdk::EventMask::SMOOTH_SCROLL_MASK);
    window.gtk_window().connect_scroll_event(move |_, event| {
        let (x, y) = event.position();
        if let Some(input) = gtk_scroll_input(
            &mut state.borrow_mut(),
            Point::new(x as f32, y as f32),
            event.direction(),
            event.delta(),
            event.is_stop(),
            Instant::now(),
            idle,
        ) {
            let _ = proxy.send_event(wrap(input));
        }
        // Tao may already have queued its translated event. The application
        // ignores that Linux MouseWheel arm, so only this original event acts.
        gtk::glib::Propagation::Stop
    });
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use gtk::gdk::ScrollDirection;
    use std::time::{Duration, Instant};
    use xbar_core::{
        BarEdge, ControlScroll, TagId,
        presentation::{NodeId, PointerAction},
    };

    fn packet(
        state: &mut GtkScrollState,
        direction: ScrollDirection,
        deltas: (f64, f64),
        stop: bool,
        now: Instant,
    ) -> (ScrollDelta, ScrollPhase) {
        let Some(PointerInput::Wheel { delta, phase, .. }) = gtk_scroll_input(
            state,
            Point::new(10.0, 10.0),
            direction,
            deltas,
            stop,
            now,
            Duration::from_millis(250),
        ) else {
            panic!("GTK scroll input")
        };
        (delta, phase)
    }

    #[test]
    fn discrete_gdk_directions_survive_zero_delta_fields() {
        let mut state = GtkScrollState::default();
        for (direction, x, y) in [
            (ScrollDirection::Up, 0.0, 1.0),
            (ScrollDirection::Down, 0.0, -1.0),
            (ScrollDirection::Left, 1.0, 0.0),
            (ScrollDirection::Right, -1.0, 0.0),
        ] {
            assert_eq!(
                packet(&mut state, direction, (0.0, 0.0), false, Instant::now()),
                (ScrollDelta::Lines { x, y }, ScrollPhase::Started)
            );
        }
    }

    #[test]
    fn repeated_physical_notches_each_navigate_once() {
        let mut state = GtkScrollState::default();
        let mut core = ControlScroll::default();
        let now = Instant::now();
        for _ in 0..3 {
            let (delta, phase) = packet(&mut state, ScrollDirection::Down, (0.0, 0.0), false, now);
            assert_eq!(
                core.step(
                    NodeId::Tag(TagId::new(2).unwrap()),
                    BarEdge::Top,
                    delta,
                    phase,
                    now
                ),
                Some(PointerAction::ScrollDown)
            );
        }
    }

    #[test]
    fn smooth_gesture_preserves_threshold_and_fences_momentum() {
        let mut state = GtkScrollState::default();
        let mut core = ControlScroll::default();
        let now = Instant::now();
        let id = NodeId::Tag(TagId::new(2).unwrap());
        for (amount, expected) in [
            (0.2, None),
            (0.3, None),
            (0.5, Some(PointerAction::ScrollDown)),
            (4.0, None),
        ] {
            let (delta, phase) = packet(
                &mut state,
                ScrollDirection::Smooth,
                (0.0, amount),
                false,
                now,
            );
            assert_eq!(core.step(id, BarEdge::Top, delta, phase, now), expected);
        }
        let (delta, phase) = packet(&mut state, ScrollDirection::Smooth, (0.0, 0.0), true, now);
        assert_eq!(phase, ScrollPhase::Ended);
        assert_eq!(core.step(id, BarEdge::Top, delta, phase, now), None);
        let (delta, phase) = packet(&mut state, ScrollDirection::Smooth, (0.0, 4.0), false, now);
        assert_eq!(phase, ScrollPhase::Moved);
        assert_eq!(core.step(id, BarEdge::Top, delta, phase, now), None);
        let later = now + Duration::from_millis(300);
        let (delta, phase) = packet(
            &mut state,
            ScrollDirection::Smooth,
            (0.0, 1.0),
            false,
            later,
        );
        assert_eq!(phase, ScrollPhase::Started);
        assert_eq!(
            core.step(id, BarEdge::Top, delta, phase, later),
            Some(PointerAction::ScrollDown)
        );
    }
}
