//! Owned JWM compositor handoff + actual Wayland viewer + Noise/TCP + native
//! control lease. No synthetic enter/focus or test lifecycle command grants
//! authority in these tests; frames still require actual GL pixel readback.
use super::*;
use crate::backend::wayland_udev::viewer_handoff;
use crate::remote::resumable_control::Kind;

#[test]
fn actual_seamless_edges_release_held_key_and_resume_original_lease() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new_seamless(&mut gl);
    h.activate(&mut gl);
    assert!(viewer_handoff::active(&h.local.state));
    assert!(viewer_handoff::inhibits(&h.local.state));
    let original = remote_control::lease_timing_for_test(&h.native.state).unwrap();
    assert!(h.resumable_commands.lock().unwrap().is_empty());

    h.key(100, 42, true);
    h.wait(&mut gl, "actual Shift delivered before edge", true, |s| {
        s.received.keys == [(42, 1)]
    });
    h.seamless_pointer_focus(false);
    h.wait(
        &mut gl,
        "pointer leave releases native input before pause ACK",
        true,
        |s| {
            s.resumable_acks
                .lock()
                .unwrap()
                .last()
                .is_some_and(|ack| ack.kind == Kind::Paused)
                && s.received.keys == [(42, 1), (42, 0)]
                && !viewer_handoff::active(&s.local.state)
                && !viewer_handoff::inhibits(&s.local.state)
                && s.ready.lock().unwrap().is_none()
        },
    );
    assert_eq!(
        remote_control::lease_timing_for_test(&h.native.state),
        Some(original)
    );
    h.key(102, 42, false);
    // A real previous local recipient is selected while the handoff is inactive.
    // Readback already targets the identified viewer, never this added app.
    let mut prior = App::with_size_color(&mut h.local, [0, 255, 0, 255], 100, 80);
    let (prior_pointer, prior_keyboard) = bind_inputs(&mut h.local, &mut prior, 7);
    focus(&mut h.local, &prior);
    prior.client.events();
    h.seamless_pointer_focus(true);
    h.wait(
        &mut gl,
        "clean native reentry and real TCP resumed ACK",
        true,
        |s| {
            s.resumable_acks
                .lock()
                .unwrap()
                .last()
                .is_some_and(|ack| ack.kind == Kind::Resumed)
                && viewer_handoff::active(&s.local.state)
                && viewer_handoff::inhibits(&s.local.state)
        },
    );
    let resumed = *h.resumable_acks.lock().unwrap().last().unwrap();
    assert_eq!(resumed.generation, 2);
    h.wait(
        &mut gl,
        "post resume clean fence and new presented proof",
        true,
        |s| {
            s.ready
                .lock()
                .unwrap()
                .is_some_and(|seq| seq > resumed.proof && s.frames.lock().unwrap().1 == Some(seq))
        },
    );
    assert_eq!(
        h.received.keys,
        [(42, 1), (42, 0)],
        "outside release cannot leak or duplicate cleanup"
    );
    h.key(110, 30, true);
    h.key(112, 30, false);
    h.wait(&mut gl, "new generation real key pair", true, |s| {
        s.received.keys.len() == 4
    });
    assert_eq!(h.received.keys, [(42, 1), (42, 0), (30, 1), (30, 0)]);
    assert_eq!(
        remote_control::lease_timing_for_test(&h.native.state),
        Some(original)
    );
    assert_eq!(remote_control::attempts_for_test(&h.native.state), 1);
    assert!(h.resumable_commands.lock().unwrap().is_empty());
    h.key(120, 42, true);
    h.wait(
        &mut gl,
        "held key before restore-to-prior exit",
        true,
        |s| s.received.keys.len() == 5,
    );
    h.seamless_pointer_focus(false);
    h.wait(&mut gl, "second pause cleans remote held key", true, |s| {
        s.resumable_acks.lock().unwrap().len() == 3
            && s.received.keys.len() == 6
            && !viewer_handoff::inhibits(&s.local.state)
    });
    assert!(
        h.local
            .state
            .seat
            .get_keyboard()
            .unwrap()
            .current_focus()
            .is_none(),
        "prior focus must wait for physical held-key quarantine"
    );
    let mut prior_received = Recipient::default();
    prior_received.collect(&mut prior, prior_pointer, prior_keyboard);
    assert!(prior_received.keys.is_empty());
    h.key(122, 42, false);
    h.local.roundtrip();
    prior_received.collect(&mut prior, prior_pointer, prior_keyboard);
    assert!(
        prior_received.keys.is_empty(),
        "neither held press nor release may replay into prior recipient"
    );
    assert!(
        prior_received
            .keyboard_wire
            .iter()
            .filter(|(op, _)| *op == 1)
            .all(|(_, args)| args.get(2) == Some(&0)),
        "focus restore enters with empty held-key list"
    );
    assert_eq!(
        h.local.state.seat.get_keyboard().unwrap().current_focus(),
        h.local.state.surface_for_window(prior.window)
    );
    assert_eq!(remote_control::attempts_for_test(&h.native.state), 1);
    assert_eq!(
        remote_control::lease_timing_for_test(&h.native.state),
        Some(original)
    );
    h.unexpected.assert_no_input();
}

#[test]
fn actual_seamless_fast_reentry_cannot_authorize_input_before_resume_ack() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let (reached_tx, reached_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::channel();
    let mut h = Harness::new_seamless_with_ack_barrier(&mut gl, reached_tx, release_rx);
    h.activate(&mut gl);
    let original = remote_control::lease_timing_for_test(&h.native.state).unwrap();
    // Two actual seat focus edges, without waiting for a network pause ACK.
    // A new physical enter serial is evidence of intent, not input authority.
    h.seamless_pointer_focus(false);
    h.seamless_pointer_focus(true);
    h.wait(
        &mut gl,
        "fast reentry reaches actual withheld resume ACK",
        true,
        |_| match reached_rx.try_recv() {
            Ok(()) => true,
            Err(mpsc::TryRecvError::Empty) => false,
            Err(mpsc::TryRecvError::Disconnected) => panic!("resume ACK barrier disconnected"),
        },
    );
    assert_eq!(h.resumable_acks.lock().unwrap().len(), 1);
    assert_eq!(h.resumable_acks.lock().unwrap()[0].kind, Kind::Paused);
    assert_eq!(
        remote_control::lease_timing_for_test(&h.native.state),
        Some(original)
    );
    assert_eq!(remote_control::attempts_for_test(&h.native.state), 1);
    h.received.assert_no_input();
    // An actual physical press while the network ACK is withheld must never
    // become authorized merely because that ACK later arrives.
    h.key(200, 30, true);
    h.local.roundtrip();
    release_tx.send(()).unwrap();
    let expected_pointer = h.received.pointer_actions.clone();
    h.rejected(
        &mut gl,
        &[],
        &expected_pointer,
        1,
        "Wayland control requires clean dual focus and actual presentation",
    );
}

#[test]
fn actual_seamless_unrelated_focus_loss_is_terminal_not_pause() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new_seamless(&mut gl);
    h.activate(&mut gl);
    h.key(300, 30, true);
    h.wait(&mut gl, "held key before external focus theft", true, |s| {
        s.received.keys == [(30, 1)]
    });
    // No viewer exit transaction: an unrelated compositor focus mutation must
    // not be mistaken for an expected hover transition or renewed permission.
    h.local
        .state
        .set_keyboard_focus(None, SERIAL_COUNTER.next_serial());
    h.local.roundtrip();
    assert!(!viewer_handoff::active(&h.local.state));
    assert!(!viewer_handoff::inhibits(&h.local.state));
    assert!(h.resumable_acks.lock().unwrap().is_empty());
    let expected_pointer = h.received.pointer_actions.clone();
    h.rejected(
        &mut gl,
        &[(30, 1), (30, 0)],
        &expected_pointer,
        1,
        "local handoff revoked: unexpected focus change",
    );
}
