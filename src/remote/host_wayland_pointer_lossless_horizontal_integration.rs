//! Horizontal permission is independent of vertical permission and PNG repair.
use super::*;

#[test]
fn actual_pointer_horizontal_lossless_800x600_multibatch_exact_then_opposite_notch() {
    exercise_800x600_multibatch_exact_then_opposite_notch(true);
}

#[test]
fn actual_pointer_horizontal_lossless_waiting_half_group_never_replays() {
    exercise_waiting_half_group_never_replays(true);
}

#[test]
fn actual_pointer_horizontal_lossless_disabled_or_held_wheel_revokes_without_delivery() {
    exercise_disabled_or_held_wheel_revokes_without_delivery(true);
}

#[test]
fn actual_pointer_horizontal_lossless_requires_host_scroll_permission() {
    exercise_requires_host_scroll_permission(true);
}

#[test]
fn actual_pointer_horizontal_lossless_lock_and_unmap_aba_cannot_restore_authority() {
    exercise_lock_and_unmap_aba_cannot_restore_authority(true);
}

#[test]
fn actual_pointer_horizontal_lossless_offer_partial_and_accepted_groups_never_replay() {
    exercise_offer_partial_and_accepted_groups_never_replay(true);
}

#[test]
fn actual_pointer_horizontal_lossless_mixed_axes_revoke_without_native_input() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for horizontal_first in [false, true] {
        let mut h = Harness::new_sized(
            &mut gl,
            (true, true),
            (true, true),
            true,
            true,
            (SW, SH),
            (VW, VH),
        );
        h.ready_after(&mut gl, None);
        let p = h.pointer.as_ref().unwrap().clone();
        assert_eq!(p.version(), 7, "viewer uses real v7 discrete groups");
        let first = wheel_axis_kind(horizontal_first);
        let second = wheel_axis_kind(!horizontal_first);
        p.motion(50, 160.0, 120.0);
        p.axis_source(AxisSource::Wheel);
        p.axis_discrete(first, 1);
        p.axis(50, first, 10.0);
        p.axis_discrete(second, -1);
        p.axis(50, second, -10.0);
        p.frame(); // both individually permitted axes, forbidden in one group
        h.local.roundtrip();
        h.wait(&mut gl, "mixed-axis frame permanently revokes", true, |s| {
            assert!(
                s.wire_events.is_empty(),
                "mixed frame must deliver no partial action"
            );
            !remote_pointer::active(&s.native.state) && s.ready.lock().unwrap().is_none()
        });
        h.wheel_axis(-1, Axis::HorizontalScroll);
        h.repaint(true);
        let previous = h.presentation_count;
        h.wait(
            &mut gl,
            "video continues without reviving mixed-axis lease",
            true,
            |s| {
                assert!(s.wire_events.is_empty());
                assert!(!remote_pointer::active(&s.native.state));
                assert!(s.ready.lock().unwrap().is_none());
                s.presentation_count > previous && s.blue_presented
            },
        );
        h.close(&mut gl);
    }
}
