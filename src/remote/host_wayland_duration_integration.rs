//! Actual advertised native versions, authenticated TCP and native lease timers.
//! No synthetic time advancement or creation of replacement authority.
use super::*;
use crate::remote::resumable_control::Kind;

#[test]
fn actual_native_v4_default_duration_remains_compatible() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new_duration(&mut gl, 4, 30);
    h.activate(&mut gl);
    let original = remote_control::lease_timing_for_test(&h.native.state).unwrap();
    assert_eq!(
        original.2.duration_since(original.1),
        Duration::from_secs(30),
        "old native opcode remains fixed30; host/client effective caps are earlier"
    );
    h.resumable_commands.lock().unwrap().push_back(false);
    h.wait(&mut gl, "v4 real pause acknowledgement", true, |s| {
        s.resumable_acks
            .lock()
            .unwrap()
            .last()
            .is_some_and(|ack| ack.kind == Kind::Paused)
    });
    h.resumable_commands.lock().unwrap().push_back(true);
    h.wait(&mut gl, "v4 real resume acknowledgement", true, |s| {
        s.resumable_acks
            .lock()
            .unwrap()
            .last()
            .is_some_and(|ack| ack.kind == Kind::Resumed)
    });
    assert_eq!(
        remote_control::lease_timing_for_test(&h.native.state),
        Some(original)
    );
    assert_eq!(remote_control::attempts_for_test(&h.native.state), 1);
}

#[test]
fn actual_native_v4_rejects_extended_duration_without_input_attempt() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new_duration(&mut gl, 4, 31);
    let deadline = Instant::now() + Duration::from_secs(5);
    let (mut host, mut client) = (None, None);
    while host.is_none() || client.is_none() {
        h.tick(&mut gl, false);
        if host.is_none() {
            match h.host_done.try_recv() {
                Ok(value) => host = Some(value),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(error) => panic!("host channel: {error}"),
            }
        }
        if client.is_none() {
            match h.client_done.try_recv() {
                Ok(value) => client = Some(value),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(error) => panic!("viewer channel: {error}"),
            }
        }
        h.received.assert_no_input();
        h.unexpected.assert_no_input();
        assert_eq!(remote_control::attempts_for_test(&h.native.state), 0);
        assert!(
            Instant::now() < deadline,
            "explicit old-manager rejection must finish"
        );
        thread::yield_now();
    }
    assert!(
        host.unwrap()
            .unwrap_err()
            .to_string()
            .contains("extended control duration requires native manager v5")
    );
    assert!(client.unwrap().is_err());
    h.host.take().unwrap().join().unwrap();
    h.client.take().unwrap().join().unwrap();
}

#[test]
fn actual_native_v5_receives_only_remaining_duration_and_never_renews() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new_duration(&mut gl, 5, 300);
    // activate deliberately withholds actual presentation for >=150ms, after
    // authenticated permission. Native creation must consume that elapsed time.
    h.activate(&mut gl);
    let original = remote_control::lease_timing_for_test(&h.native.state).unwrap();
    let native_ttl = original.2.duration_since(original.1);
    assert!(native_ttl < Duration::from_secs(300));
    assert!(
        native_ttl > Duration::from_secs(30),
        "explicit extended native authority"
    );
    h.seamless_pointer_focus(false);
    h.wait(
        &mut gl,
        "v5 pause under same original duration",
        true,
        |s| {
            s.resumable_acks
                .lock()
                .unwrap()
                .last()
                .is_some_and(|ack| ack.kind == Kind::Paused)
        },
    );
    h.seamless_pointer_focus(true);
    h.wait(
        &mut gl,
        "v5 resume does not refresh native duration",
        true,
        |s| {
            s.resumable_acks
                .lock()
                .unwrap()
                .last()
                .is_some_and(|ack| ack.kind == Kind::Resumed)
        },
    );
    assert_eq!(
        remote_control::lease_timing_for_test(&h.native.state),
        Some(original)
    );
    assert_eq!(remote_control::attempts_for_test(&h.native.state), 1);
    h.received.assert_no_input();
    h.unexpected.assert_no_input();
}
