//! Private real-wire, production-capture receiver tests. Not physical DRM input.
use super::*;
use smithay::reexports::wayland_server::Resource;
use std::time::{Duration, Instant};

#[test]
fn wire_scroll_versions_deliver_one_notch_only_to_original_receiver() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for version in [4, 5, 7, 8, 9] {
        let mut f = Fixture::new_with_scroll(version, true);
        let pointer = f.server.state.seat.get_pointer().unwrap();
        let keyboard = f.server.state.seat.get_keyboard().unwrap();
        let before = pointer.current_location();
        let focus = pointer.current_focus();
        let keyboard_focus = keyboard.current_focus();
        for (sequence, kind, direction) in [(1, 2, -1i32), (2, 3, 1i32)] {
            let frame = f.frame(&mut gl);
            let result = f.action(sequence, frame, kind, 40, 30);
            f.accepted(&result, sequence);
            let delivered = pointer_events(&mut f.app, f.pointer);
            let expected: &[u16] = if version >= 8 {
                &[2, 6, 9, 4, 5]
            } else if version >= 5 {
                &[2, 6, 8, 4, 5]
            } else {
                &[2, 4]
            };
            assert_eq!(
                delivered.iter().map(|e| e.0).collect::<Vec<_>>(),
                expected,
                "version {version}: no enter/leave/button/axis_stop or extra frame"
            );
            assert_eq!(delivered[0].1[1..], [40 * 256, 30 * 256]);
            if version >= 5 {
                assert_eq!(delivered[1].1, [0], "Wheel source");
                assert_eq!(
                    delivered[2].1,
                    [0, (direction * if version >= 8 { 120 } else { 1 }) as u32]
                );
                assert!(delivered.last().unwrap().1.is_empty());
            }
            let axis = delivered.iter().find(|e| e.0 == 4).unwrap();
            assert_eq!(axis.1.len(), 3);
            assert_eq!(axis.1[1..], [0, (direction * 10 * 256) as u32]);
            assert!(pointer_events(&mut f.other, f.other_pointer).is_empty());
            assert_eq!(pointer.current_location(), before);
            assert_eq!(pointer.current_focus(), focus);
            assert_eq!(keyboard.current_focus(), keyboard_focus);
            assert!(!pointer.is_grabbed());
            assert!(!keyboard.is_grabbed());
        }
    }
}

#[test]
fn wire_scroll_version_two_legacy_grant_does_not_expand_permission() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for kind in [2, 3] {
        let mut f = Fixture::new_with_scroll(9, false);
        let frame = f.frame(&mut gl);
        let result = f.action(1, frame, kind, 10, 10);
        f.denied(&result);
    }
}

#[test]
fn wire_scroll_replay_prequeued_frame_and_wrong_epoch_are_terminal() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for case in 0..4 {
        let mut f = Fixture::new_with_scroll(9, true);
        let frame = f.frame(&mut gl);
        let queued = if case == 1 { f.frame(&mut gl) } else { frame };
        if case == 3 {
            f.epoch += 1;
        }
        if case != 3 {
            let result = f.action(1, frame, 2, 10, 10);
            f.accepted(&result, 1);
            pointer_events(&mut f.app, f.pointer);
        }
        let result = f.action(
            if case == 2 || case == 3 { 1 } else { 2 },
            queued,
            3,
            10,
            10,
        );
        f.denied(&result);
        let again = f.action(2, queued, 2, 10, 10);
        assert!(!again.iter().any(|e| e.sender == f.lease && e.opcode == 1));
        assert!(pointer_events(&mut f.app, f.pointer).is_empty());
    }
}

#[test]
fn wire_scroll_wrong_source_and_expired_ready_are_rejected() {
    use smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1;
    use crate::backend::wayland_udev::image_copy_capture::CaptureFrameData;
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for expired in [false, true] {
        let mut f = Fixture::new_with_scroll(9, true);
        let original = f.capture.source;
        if !expired {
            let source = f.capture.client.new_id();
            f.capture
                .client
                .request(f.capture.sources, 0, &[source, f.capture.handle]);
            sync(&mut f.server);
            f.capture.source = source;
        }
        let frame = f.frame(&mut gl);
        f.capture.source = original;
        if expired {
            let lease = f.server.state.remote_pointer.active.as_ref().unwrap();
            let native: ExtImageCopyCaptureFrameV1 = lease
                .resource
                .client()
                .unwrap()
                .object_from_protocol_id(&f.server.display.handle(), frame)
                .unwrap();
            let mut stamp = native
                .data::<CaptureFrameData>()
                .unwrap()
                .ready_stamp
                .lock()
                .unwrap();
            let (serial, _) = stamp.unwrap();
            *stamp = Some((serial, Instant::now() - Duration::from_secs(2)));
        }
        let result = f.action(1, frame, 2, 10, 10);
        f.denied(&result);
    }
}

#[test]
fn wire_scroll_focus_local_motion_lock_unmap_and_input_region_revoke() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for case in 0..6 {
        let mut f = Fixture::new_with_scroll(9, true);
        let frame = f.frame(&mut gl);
        let mut locker = None;
        match case {
            0 => {
                focus(&mut f.server, &f.other);
                focus(&mut f.server, &f.app);
            }
            // Same-focus local motion still invalidates the native authority.
            1 => focus(&mut f.server, &f.app),
            2 => {
                f.app.unmap(&mut f.server);
                f.app.remap(&mut f.server);
            }
            3 => {
                f.server.state.session_lock_supported = true;
                let mut client = f.server.connect();
                let manager = client.bind("ext_session_lock_manager_v1", 1);
                let lock = client.new_id();
                client.request(manager, 1, &[lock]);
                sync(&mut f.server);
                assert!(f.server.state.session_locked);
                client.request(lock, 2, &[]);
                sync(&mut f.server);
                assert!(!f.server.state.session_locked);
                locker = Some(client);
            }
            4 => {
                let compositor = f.app.client.bind("wl_compositor", 6);
                let region = f.app.client.new_id();
                f.app.client.request(compositor, 1, &[region]);
                f.app.client.request(f.app.surface, 5, &[region]);
                f.app.client.request(f.app.surface, 6, &[]);
                sync(&mut f.server);
                f.app.client.request(f.app.surface, 5, &[0]);
                f.app.client.request(f.app.surface, 6, &[]);
                sync(&mut f.server);
            }
            _ => super::super::expire_for_test(&mut f.server.state),
        }
        events(&mut f.app.client);
        events(&mut f.other.client);
        let result = f.action(1, frame, 3, 10, 10);
        // Some lifecycle paths actively delivered revoked before action. Keep
        // capture events queued until action() so explicit revocation is required.
        f.denied(&result);
        drop(locker);
    }
}

#[test]
fn wire_scroll_other_client_cannot_address_authorized_lease() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new_with_scroll(9, true);
    let frame = f.frame(&mut gl);
    let mut outsider = f.server.connect();
    outsider.request(f.lease, 1, &[f.epoch, 1, frame, 2, 10, 10]);
    sync(&mut f.server);
    assert!(
        outsider
            .events()
            .iter()
            .any(|e| e.sender == 1 && e.opcode == 0),
        "foreign object id must fail at the real Wayland ownership boundary"
    );
    assert!(pointer_events(&mut f.app, f.pointer).is_empty());
    assert!(pointer_events(&mut f.other, f.other_pointer).is_empty());
    // An invalid unrelated connection cannot acquire or consume the owner's ticket.
    let result = f.action(1, frame, 2, 10, 10);
    f.accepted(&result, 1);
}

#[test]
fn wire_scroll_out_of_bounds_and_unready_frame_never_deliver() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for case in 0..4 {
        let mut f = Fixture::new_with_scroll(9, true);
        let frame = if case == 3 {
            let frame = f.capture.client.new_id();
            f.capture.client.request(f.capture.session, 0, &[frame]);
            sync(&mut f.server);
            frame
        } else {
            f.frame(&mut gl)
        };
        let (x, y) = match case {
            0 => (800, 10),
            1 => (10, 600),
            2 => (u32::MAX, 10),
            _ => (10, 10),
        };
        let result = f.action(1, frame, 2, x, y);
        f.denied(&result);
    }
}

#[test]
fn wire_scroll_same_client_other_surface_is_not_authorized_target() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new_with_scroll(9, true);
    let frame = f.frame(&mut gl);
    let compositor = f.app.client.bind("wl_compositor", 6);
    let id = f.app.client.new_id();
    f.app.client.request(compositor, 0, &[id]);
    sync(&mut f.server);
    let original = f.server.state.surface_for_window(f.app.window).unwrap();
    let second = original
        .client()
        .unwrap()
        .object_from_protocol_id(&f.server.display.handle(), id)
        .unwrap();
    let pointer = f.server.state.seat.get_pointer().unwrap();
    pointer.motion(
        &mut f.server.state,
        Some((second, (0.0, 0.0).into())),
        &MotionEvent {
            location: (5.0, 5.0).into(),
            serial: SERIAL_COUNTER.next_serial(),
            time: InputTime::now(),
        },
    );
    pointer.frame(&mut f.server.state);
    sync(&mut f.server);
    events(&mut f.app.client);
    let result = f.action(1, frame, 2, 10, 10);
    f.denied(&result);
}
