//! Real owned Wayland recipients and production frame proofs; no physical DRM claim.
use super::*;

#[test]
fn wire_control_horizontal_scroll_exact_axis_versions_and_target_only() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for version in [4, 5, 7, 8, 9] {
        let mut f = Fixture::with_scroll_axes(version, 3, false, Some(2));
        let frame = f.frame(&mut gl);
        f.arm(frame);
        for (seq, kind, step) in [(1, 4, -1_i32), (2, 5, 1)] {
            let frame = if seq == 1 { frame } else { f.frame(&mut gl) };
            let e = f.action(seq, frame, kind, 40, 30);
            f.accepted(&e, seq);
            let all = events(&mut f.app.client);
            assert!(!all.iter().any(|e| e.sender == f.keyboard));
            let delivered: Vec<_> = all.iter().filter(|e| e.sender == f.pointer).collect();
            let expected: &[u16] = if version >= 8 {
                &[2, 6, 9, 4, 5]
            } else if version >= 5 {
                &[2, 6, 8, 4, 5]
            } else {
                &[2, 4]
            };
            assert_eq!(
                delivered.iter().map(|e| e.opcode).collect::<Vec<_>>(),
                expected
            );
            assert_eq!(delivered[0].args[1..], [40 * 256, 30 * 256]);
            if version >= 5 {
                assert_eq!(delivered[1].args, [0]);
                assert_eq!(
                    delivered[2].args,
                    [1, (step * if version >= 8 { 120 } else { 1 }) as u32]
                );
            }
            let axis = delivered.iter().find(|e| e.opcode == 4).unwrap();
            assert_eq!(axis.args[1..], [1, (step * 10 * 256) as u32]);
            f.other_empty();
        }
    }
}

#[test]
fn wire_control_horizontal_requires_new_grant_and_fresh_frame_per_axis() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for (manager, vertical, axes, kind) in [
        (2, true, None, 4),
        (3, true, None, 5),
        (3, false, None, 4),
        (3, false, Some(1), 4),
        (3, false, Some(2), 2),
    ] {
        let mut f = Fixture::with_scroll_axes(9, manager, vertical, axes);
        let frame = f.frame(&mut gl);
        f.arm(frame);
        let e = f.action(1, frame, kind, 10, 10);
        f.denied(&e);
    }
    let mut f = Fixture::with_scroll_axes(9, 3, false, Some(3));
    let frame = f.frame(&mut gl);
    f.arm(frame);
    let e = f.action(1, frame, 2, 10, 10);
    f.accepted(&e, 1);
    assert!(
        pointer_events(&mut f.app, f.pointer)
            .iter()
            .any(|e| e.0 == 4 && e.1[1] == 0)
    );
    let e = f.action(2, frame, 4, 10, 10);
    f.denied(&e);
}

#[test]
fn wire_control_horizontal_held_shift_revokes_with_exact_cleanup() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for kind in [4, 5] {
        let mut f = Fixture::with_scroll_axes(9, 3, false, Some(2));
        let frame = f.frame(&mut gl);
        f.arm(frame);
        let e = f.key(1, 42, true);
        f.accepted(&e, 1);
        assert_eq!(f.target_keys(), vec![(42, 1)]);
        let frame = f.frame(&mut gl);
        let e = f.action(2, frame, kind, 10, 10);
        assert!(e.iter().any(|e| e.sender == f.lease && e.opcode == 4));
        let all = events(&mut f.app.client);
        assert!(!all.iter().any(|e| e.sender == f.pointer));
        let keys: Vec<_> = all
            .iter()
            .filter(|e| e.sender == f.keyboard && e.opcode == 3)
            .map(|e| (e.args[2], e.args[3]))
            .collect();
        assert_eq!(keys, vec![(42, 0)]);
        f.other_empty();
    }
}

#[test]
fn wire_control_horizontal_focus_unmap_and_local_revoke_are_terminal() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for mode in 0..3 {
        let mut f = Fixture::with_scroll_axes(9, 3, false, Some(2));
        let frame = f.frame(&mut gl);
        f.arm(frame);
        match mode {
            0 => {
                focus(&mut f.server, &f.other);
                focus(&mut f.server, &f.app);
            }
            1 => {
                f.app.unmap(&mut f.server);
                f.app.remap(&mut f.server);
            }
            _ => super::super::revoke(&mut f.server.state, super::super::SEAT),
        }
        events(&mut f.app.client);
        events(&mut f.other.client);
        let e = f.action(1, frame, 5, 10, 10);
        f.denied(&e);
    }
}

#[test]
fn wire_control_malformed_axes_consume_attempt_and_cannot_retry_expanded_grant() {
    for axes in [0, 4, u32::MAX] {
        let mut f = Fixture::with_scroll_axes(9, 3, false, Some(axes));
        assert_eq!(f.epoch, 0);
        assert!(!super::super::active(&f.server.state));
        let manager = f.capture.client.bind("jwm_remote_control_manager_v1", 3);
        for (opcode, mask) in [(3, Some(3)), (2, None), (1, None)] {
            let lease = f.capture.client.new_id();
            if let Some(mask) = mask {
                f.capture
                    .client
                    .request(manager, opcode, &[lease, f.capture.source, mask]);
            } else {
                f.capture
                    .client
                    .request(manager, opcode, &[lease, f.capture.source]);
            }
            sync(&mut f.server);
            let e = events(&mut f.capture.client);
            assert!(e.iter().any(|e| e.sender == lease && e.opcode == 4));
            assert!(!e.iter().any(|e| e.sender == lease && e.opcode != 4));
            assert!(!super::super::active(&f.server.state));
        }
        assert!(events(&mut f.app.client).is_empty());
        f.other_empty();
    }
}
