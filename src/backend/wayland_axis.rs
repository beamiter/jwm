//! Shared translation from backend scroll events to Wayland axis frames.
use smithay::backend::input::{Axis, AxisSource, InputBackend, PointerAxisEvent};
use smithay::input::pointer::AxisFrame;

/// `continuous_stops` is enabled for libinput, whose present zero axes denote
/// stops. Nested winit can instead expose zero on an unused continuous axis.
pub(crate) fn pointer_axis_frame<B: InputBackend>(
    event: &impl PointerAxisEvent<B>,
    continuous_stops: bool,
) -> AxisFrame {
    let source = event.source();
    let mut frame = AxisFrame::new(event.time()).source(source);
    for axis in [Axis::Horizontal, Axis::Vertical] {
        frame = append_axis(
            frame,
            axis,
            source,
            continuous_stops,
            event.amount(axis),
            event.amount_v120(axis),
        );
        frame = frame.relative_direction(axis, event.relative_direction(axis));
    }
    frame
}

fn append_axis(
    mut frame: AxisFrame,
    axis: Axis,
    source: AxisSource,
    continuous_stops: bool,
    amount: Option<f64>,
    v120: Option<f64>,
) -> AxisFrame {
    // Nested wheel backends can supply only v120; Wayland still requires an
    // axis value, including for clients older than axis_value120. Preserve an
    // explicit zero amount so libinput termination remains distinguishable.
    if let Some(value) = amount.or_else(|| v120.map(|value| value * 15.0 / 120.0)) {
        // libinput reports finger lift as a present axis with zero movement.
        // Merely forwarding value(0) drops that notification on the wire.
        // Other sources can expose zero for an unused axis (notably winit),
        // so continuous stops are enabled only for the libinput path.
        frame = if (source == AxisSource::Finger
            || (continuous_stops && source == AxisSource::Continuous))
            && value == 0.0
        {
            frame.stop(axis)
        } else {
            frame.value(axis, value)
        };
    }
    if let Some(value) = v120 {
        frame = frame.v120(axis, value as i32);
    }
    frame
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::backend::input::InputTime;

    fn frame() -> AxisFrame {
        AxisFrame::new(InputTime::from_millis(42))
    }

    #[test]
    fn finger_lift_stops_only_the_present_axis() {
        let frame = append_axis(
            frame(),
            Axis::Vertical,
            AxisSource::Finger,
            false,
            Some(0.0),
            None,
        );
        let frame = append_axis(
            frame,
            Axis::Horizontal,
            AxisSource::Finger,
            false,
            None,
            None,
        );
        assert_eq!(frame.stop, (false, true));
        assert_eq!(frame.axis, (0.0, 0.0));
        assert_eq!(frame.v120, None);
    }

    #[test]
    fn finger_motion_preserves_values_without_stopping() {
        let frame = append_axis(
            frame(),
            Axis::Horizontal,
            AxisSource::Finger,
            false,
            Some(-3.5),
            None,
        );
        assert_eq!(frame.axis, (-3.5, 0.0));
        assert_eq!(frame.stop, (false, false));
    }

    #[test]
    fn wheel_preserves_high_resolution_steps_without_stopping() {
        let frame = append_axis(
            frame(),
            Axis::Vertical,
            AxisSource::Wheel,
            false,
            Some(15.0),
            Some(60.0),
        );
        assert_eq!(frame.axis, (0.0, 15.0));
        assert_eq!(frame.v120, Some((0, 60)));
        assert_eq!(frame.stop, (false, false));
    }

    #[test]
    fn fractional_wheel_steps_supply_a_legacy_axis_value() {
        let frame = append_axis(
            frame(),
            Axis::Vertical,
            AxisSource::Wheel,
            false,
            None,
            Some(-60.0),
        );
        assert_eq!(frame.axis, (0.0, -7.5));
        assert_eq!(frame.v120, Some((0, -60)));
        assert_eq!(frame.stop, (false, false));
    }

    #[test]
    fn explicit_pixel_amount_takes_precedence_over_wheel_steps() {
        let frame = append_axis(
            frame(),
            Axis::Vertical,
            AxisSource::Wheel,
            false,
            Some(4.0),
            Some(120.0),
        );
        assert_eq!(frame.axis, (0.0, 4.0));
        assert_eq!(frame.v120, Some((0, 120)));
    }

    #[test]
    fn missing_wheel_axis_emits_neither_values_nor_stops() {
        let frame = append_axis(
            frame(),
            Axis::Horizontal,
            AxisSource::Wheel,
            true,
            None,
            None,
        );
        assert_eq!(frame.axis, (0.0, 0.0));
        assert_eq!(frame.v120, None);
        assert_eq!(frame.stop, (false, false));
    }

    #[test]
    fn explicit_zero_is_not_replaced_by_wheel_fallback() {
        let frame = append_axis(
            frame(),
            Axis::Vertical,
            AxisSource::Wheel,
            true,
            Some(0.0),
            Some(120.0),
        );
        assert_eq!(frame.axis, (0.0, 0.0));
        assert_eq!(frame.stop, (false, false));
    }

    #[test]
    fn libinput_continuous_zero_stops_the_present_axis() {
        let frame = append_axis(
            frame(),
            Axis::Vertical,
            AxisSource::Continuous,
            true,
            Some(0.0),
            None,
        );
        assert_eq!(frame.stop, (false, true));
    }

    #[test]
    fn zero_continuous_axis_does_not_invent_a_stop() {
        let frame = append_axis(
            frame(),
            Axis::Horizontal,
            AxisSource::Continuous,
            false,
            Some(0.0),
            None,
        );
        assert_eq!(frame.stop, (false, false));
    }
}
