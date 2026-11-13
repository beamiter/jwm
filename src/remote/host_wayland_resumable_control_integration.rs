//! Programmatic, explicit two-sided opt-in through the production Noise/TCP,
//! host, native v4 adapter and owned Wayland viewer. No hover/focus policy is
//! enabled by this fixture; commands only request real lifecycle transitions.
use super::*;
use crate::remote::resumable_control::Kind;

#[test]
fn actual_resumable_control_pause_ack_releases_keys_and_preserves_original_native_lease() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new_resumable(&mut gl, true, true);
    h.activate(&mut gl);
    let native_original =
        remote_control::lease_timing_for_test(&h.native.state).expect("real native grant");
    h.key(100, 42, true);
    h.wait(&mut gl, "real held shift press before pause", true, |s| {
        s.received.keys == [(42, 1)]
    });
    h.resumable_commands.lock().unwrap().push_back(false);
    h.wait(
        &mut gl,
        "native release and actual TCP pause acknowledgement",
        true,
        |s| {
            s.resumable_acks
                .lock()
                .unwrap()
                .last()
                .is_some_and(|ack| ack.kind == Kind::Paused)
                && s.received.keys == [(42, 1), (42, 0)]
                && s.ready.lock().unwrap().is_none()
        },
    );
    let paused = *h.resumable_acks.lock().unwrap().last().unwrap();
    assert_eq!(paused.generation, 1);
    assert_eq!(paused.sequence, 2, "key and pause share one order");
    assert_eq!(
        remote_control::lease_timing_for_test(&h.native.state),
        Some(native_original)
    );
    assert_eq!(remote_control::attempts_for_test(&h.native.state), 1);
    // The actual local key remains held until this edge. Paused polling must
    // consume it locally, not transmit a second release to the cleared host.
    h.key(102, 42, false);
    let until = Instant::now() + Duration::from_millis(150);
    while Instant::now() < until {
        h.tick(&mut gl, true);
        assert_eq!(h.received.keys, [(42, 1), (42, 0)]);
        thread::sleep(Duration::from_millis(1));
    }
    h.resumable_commands.lock().unwrap().push_back(true);
    h.wait(
        &mut gl,
        "fresh actual-presented proof and TCP resume acknowledgement",
        true,
        |s| {
            s.resumable_acks
                .lock()
                .unwrap()
                .last()
                .is_some_and(|ack| ack.kind == Kind::Resumed)
        },
    );
    let resumed = *h.resumable_acks.lock().unwrap().last().unwrap();
    assert_eq!((resumed.generation, resumed.sequence), (2, 3));
    assert_ne!(resumed.proof, 0);
    assert_eq!(
        remote_control::lease_timing_for_test(&h.native.state),
        Some(native_original)
    );
    // A network ACK alone does not authorize the batch already collected
    // locally. Wait for the client's post-ACK clean-focus fence and a newly
    // presented input-ready proof, not a stale pre-pause observer value.
    h.wait(
        &mut gl,
        "post-resume local clean gate and new input-ready proof",
        true,
        |s| {
            s.ready.lock().unwrap().is_some_and(|sequence| {
                sequence > resumed.proof && s.frames.lock().unwrap().1 == Some(sequence)
            })
        },
    );
    h.key(110, 30, true);
    h.wait(&mut gl, "fresh generation key down", true, |s| {
        s.received.keys.len() == 3
    });
    h.key(112, 30, false);
    h.wait(&mut gl, "fresh generation key up", true, |s| {
        s.received.keys.len() == 4
    });
    assert_eq!(h.received.keys, [(42, 1), (42, 0), (30, 1), (30, 0)]);
    h.resumable_commands.lock().unwrap().push_back(false);
    h.wait(
        &mut gl,
        "second acknowledged pause without new lease",
        true,
        |s| s.resumable_acks.lock().unwrap().len() == 3,
    );
    h.resumable_commands.lock().unwrap().push_back(true);
    h.wait(
        &mut gl,
        "second acknowledged resume without deadline renewal",
        true,
        |s| s.resumable_acks.lock().unwrap().len() == 4,
    );
    let final_ack = *h.resumable_acks.lock().unwrap().last().unwrap();
    assert_eq!(
        (final_ack.kind, final_ack.generation, final_ack.sequence),
        (Kind::Resumed, 3, 7)
    );
    assert_eq!(
        remote_control::lease_timing_for_test(&h.native.state),
        Some(native_original)
    );
    assert_eq!(remote_control::attempts_for_test(&h.native.state), 1);
    h.wait(&mut gl, "second post-resume local clean gate", true, |s| {
        s.ready.lock().unwrap().is_some_and(|sequence| {
            sequence > final_ack.proof && s.frames.lock().unwrap().1 == Some(sequence)
        })
    });
    h.unexpected.assert_no_input();
    // Real local leave is still terminal in this stage. There is no hidden
    // automatic permission renewal or enabled compositor hover implementation.
    let surface = viewer_surface(&h.local);
    h.pointer.as_ref().unwrap().leave(120, &surface);
    h.pointer.as_ref().unwrap().frame();
    h.local.roundtrip();
    let expected_pointer = h.received.pointer_actions.clone();
    h.rejected(
        &mut gl,
        &[(42, 1), (42, 0), (30, 1), (30, 0)],
        &expected_pointer,
        1,
        "Wayland pointer left own surface",
    );
}

#[test]
fn actual_resumable_control_held_local_key_queued_before_resume_ack_never_reaches_remote() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let (reached_tx, reached_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::channel();
    let mut h = Harness::new_resumable_with_ack_barrier(&mut gl, reached_tx, release_rx);
    h.activate(&mut gl);
    h.resumable_commands.lock().unwrap().push_back(false);
    h.wait(&mut gl, "real paused ACK before resume race", true, |s| {
        s.resumable_acks
            .lock()
            .unwrap()
            .last()
            .is_some_and(|ack| ack.kind == Kind::Paused)
            && s.ready.lock().unwrap().is_none()
    });
    h.resumable_commands.lock().unwrap().push_back(true);
    h.wait(
        &mut gl,
        "real resume ACK decoded but not admitted",
        true,
        |_| match reached_rx.try_recv() {
            Ok(()) => true,
            Err(mpsc::TryRecvError::Empty) => false,
            Err(mpsc::TryRecvError::Disconnected) => panic!("resume ACK barrier disconnected"),
        },
    );
    assert_eq!(h.resumable_acks.lock().unwrap().len(), 1);
    assert!(h.received.keys.is_empty());
    // Flush an actual local wl_keyboard press before admitting the actual
    // network ACK. The post-ACK same-connection fence must observe this edge,
    // whether the ordinary event poll happened before or after the ACK. No
    // sleep, synthetic held bit or forged presentation is used as evidence.
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
fn actual_resumable_control_requires_both_opt_ins_and_preserves_legacy_host_only_mode() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    // A client request cannot silently downgrade when the actual authenticated
    // host has not opted in. Both real workers must finish, not merely stop
    // producing observations or time out in the test.
    let mut denied = Harness::new_resumable(&mut gl, false, true);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut client = None;
    let mut host = None;
    loop {
        denied.tick(&mut gl, false);
        if client.is_none() {
            match denied.client_done.try_recv() {
                Ok(result) => client = Some(result),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => panic!("denied client worker panicked"),
            }
        }
        if host.is_none() {
            match denied.host_done.try_recv() {
                Ok(result) => host = Some(result),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => panic!("denied host worker panicked"),
            }
        }
        assert_eq!(remote_control::attempts_for_test(&denied.native.state), 0);
        assert!(!remote_control::active(&denied.native.state));
        denied.received.assert_no_input();
        denied.unexpected.assert_no_input();
        if client.is_some() && host.is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "explicit opt-in denial must finish handshake"
        );
        thread::yield_now();
    }
    assert!(
        client.unwrap().is_err(),
        "viewer must not report a downgraded session"
    );
    assert_eq!(
        host.unwrap()
            .expect_err("host lacks explicit resumable permission")
            .to_string(),
        "resumable control requires both explicit permissions",
    );
    assert!(denied.resumable_acks.lock().unwrap().is_empty());
    denied.client.take().unwrap().join().unwrap();
    denied.host.take().unwrap().join().unwrap();
    drop(denied);

    // Host capability/permission alone does not force a legacy viewer to use
    // the new protocol. The ordinary single lease still controls and then
    // terminates on leave without sending a resumable acknowledgement.
    let mut legacy = Harness::new_resumable(&mut gl, true, false);
    legacy.activate(&mut gl);
    legacy.key(300, 30, true);
    legacy.wait(
        &mut gl,
        "legacy key press with host-only opt-in",
        true,
        |s| s.received.keys.len() == 1,
    );
    legacy.key(302, 30, false);
    legacy.wait(
        &mut gl,
        "legacy key release with host-only opt-in",
        true,
        |s| s.received.keys.len() == 2,
    );
    assert_eq!(legacy.received.keys, [(30, 1), (30, 0)]);
    assert!(legacy.resumable_acks.lock().unwrap().is_empty());
    assert_eq!(remote_control::attempts_for_test(&legacy.native.state), 1);
    let surface = viewer_surface(&legacy.local);
    legacy.pointer.as_ref().unwrap().leave(310, &surface);
    legacy.pointer.as_ref().unwrap().frame();
    legacy.local.roundtrip();
    let expected_pointer = legacy.received.pointer_actions.clone();
    legacy.rejected(
        &mut gl,
        &[(30, 1), (30, 0)],
        &expected_pointer,
        1,
        "Wayland pointer left own surface",
    );
}
