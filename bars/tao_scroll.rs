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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::time::Instant;
    use xbar_core::{ScrollAccumulator, ScrollMode, ScrollStep};

    #[test]
    fn gtk_ended_notches_dispatch_while_real_gesture_endings_do_not() {
        let point = Point::new(10.0, 10.0);
        let mut scroll = ScrollAccumulator::default();
        let now = Instant::now();
        for _ in 0..3 {
            let PointerInput::Wheel { delta, phase, .. } = pointer_scroll(
                point,
                MouseScrollDelta::LineDelta(0.0, 1.0),
                TouchPhase::Ended,
                2.0,
            ) else {
                panic!("wheel translation")
            };
            assert_eq!(
                scroll.step(1, delta, phase, ScrollMode::Navigation, true, now),
                Some(ScrollStep::Up)
            );
        }
        for delta in [
            MouseScrollDelta::LineDelta(0.0, 0.0),
            MouseScrollDelta::LineDelta(0.0, 0.2),
            MouseScrollDelta::PixelDelta(tao::dpi::PhysicalPosition::new(0.0, 40.0)),
        ] {
            let PointerInput::Wheel { delta, phase, .. } =
                pointer_scroll(point, delta, TouchPhase::Ended, 2.0)
            else {
                panic!("wheel translation")
            };
            assert_eq!(phase, ScrollPhase::Ended);
            assert_eq!(
                scroll.step(1, delta, phase, ScrollMode::Navigation, true, now),
                None
            );
        }
    }
}
