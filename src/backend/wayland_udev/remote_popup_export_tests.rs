//! Native v2 proof exports come from actual rendered capture resources.
use super::*;
fn export(f: &mut Fixture, frame: u32, epoch: u32, cookie: u32) -> Vec<Event> {
    f.capture
        .client
        .request(f.lease, 4, &[epoch, frame, cookie]);
    fence(&mut f.server, &mut f.capture.client)
}
fn metadata(f: &Fixture, events: &[Event], frame: u32, cookie: u32) -> Vec<u32> {
    assert!(!events.iter().any(|e| e.sender == f.lease && e.opcode == 3));
    let proofs: Vec<_> = events
        .iter()
        .filter(|e| e.sender == f.lease && e.opcode == 4)
        .collect();
    assert_eq!(proofs.len(), 1);
    let args = &proofs[0].args;
    assert_eq!(args.len(), 14);
    assert_eq!(&args[..3], &[f.epoch, frame, cookie]);
    assert_eq!(&args[7..9], &[W, H]);
    assert!(number(&args[3..5]) > 0);
    assert!(number(&args[5..7]) > 0);
    args.clone()
}
fn number(words: &[u32]) -> u64 {
    (u64::from(words[0]) << 32) | u64::from(words[1])
}
fn rejected(f: &mut Fixture, events: &[Event]) {
    assert!(!events.iter().any(|e| e.sender == f.lease && e.opcode == 4));
    f.revoked(events);
}
#[test]
fn actual_v2_root_and_popup_exports_preserve_arm_action_and_original_deadline() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new_with_version(2);
    let timing = super::super::lease_timing_for_test(&f.server.state).unwrap();
    let (frame, _) = f.frame(&mut gl);
    let epoch = f.epoch;
    let events = export(&mut f, frame, epoch, 41);
    let root = metadata(&f, &events, frame, 41);
    assert_eq!(&root[9..], &[0, 0, 0, 0, 0]);
    f.arm(frame);
    let (frame, _) = f.frame(&mut gl);
    let events = export(&mut f, frame, epoch, 42);
    let second = metadata(&f, &events, frame, 42);
    assert!(number(&second[3..5]) > number(&root[3..5]));
    let events = f.action(frame, 1, 1, 8, 8);
    f.accepted(&events, 1);
    let events = fence(&mut f.server, &mut f.app.client);
    let serial = events
        .iter()
        .find(|e| e.sender == f.pointer && e.opcode == 3 && e.args[3] == 1)
        .expect("actual press")
        .args[0];
    popup(&mut f, serial);
    let (frame, _) = f.frame(&mut gl);
    let events = export(&mut f, frame, epoch, 43);
    let menu = metadata(&f, &events, frame, 43);
    assert_eq!(&menu[9..], &[1, 4, 3, 12, 10]);
    assert!(number(&menu[5..7]) > number(&root[5..7]));
    let events = f.action(frame, 2, 1, 7, 8);
    f.accepted(&events, 2);
    assert_eq!(
        super::super::lease_timing_for_test(&f.server.state),
        Some(timing)
    );
}
#[test]
fn actual_v2_duplicate_consumed_and_wrong_epoch_exports_revoke_without_metadata() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for case in 0..3 {
        let mut f = Fixture::new_with_version(2);
        let (frame, _) = f.frame(&mut gl);
        let epoch = f.epoch;
        if case == 0 {
            let events = export(&mut f, frame, epoch, 1);
            metadata(&f, &events, frame, 1);
        } else if case == 1 {
            f.arm(frame);
        }
        let events = export(&mut f, frame, if case == 2 { epoch + 1 } else { epoch }, 2);
        rejected(&mut f, &events);
    }
}
#[test]
fn actual_v2_unready_wrong_source_and_commit_aba_exports_cannot_describe_current_layout() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for case in 0..3 {
        let mut f = Fixture::new_with_version(2);
        let frame = if case == 0 {
            let session = f.capture.client.new_id();
            f.capture
                .client
                .request(f.capture.manager, 0, &[session, f.source, 0]);
            let frame = f.capture.client.new_id();
            f.capture.client.request(session, 0, &[frame]);
            fence(&mut f.server, &mut f.capture.client);
            frame
        } else if case == 1 {
            let family = f.source;
            f.source = f.capture.source;
            let (frame, _) = f.frame(&mut gl);
            f.source = family;
            frame
        } else {
            let (frame, _) = f.frame(&mut gl);
            f.app.client.request(f.app.xdg, 3, &[1, 0, W - 1, H]);
            f.app.client.request(f.app.surface, 6, &[]);
            fence(&mut f.server, &mut f.app.client);
            f.app.client.request(f.app.xdg, 3, &[0, 0, W, H]);
            f.app.client.request(f.app.surface, 6, &[]);
            fence(&mut f.server, &mut f.app.client);
            frame
        };
        let epoch = f.epoch;
        let events = export(&mut f, frame, epoch, 7);
        rejected(&mut f, &events);
    }
}

#[test]
fn actual_v2_foreign_denied_export_cannot_revoke_or_export_current_owner() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new_with_version(2);
    let timing = super::super::lease_timing_for_test(&f.server.state).unwrap();
    let mut foreign = Capture::new_with_size(&mut f.server, (W, H));
    let manager = foreign.client.bind("jwm_remote_popup_manager_v1", 2);
    let denied = foreign.client.new_id();
    foreign
        .client
        .request(manager, 1, &[denied, foreign.source]);
    let events = fence(&mut f.server, &mut foreign.client);
    assert!(events.iter().any(|e| e.sender == denied && e.opcode == 3));
    assert!(!events.iter().any(|e| e.sender == denied && e.opcode == 0));
    let frame = foreign.client.new_id();
    foreign.client.request(foreign.session, 0, &[frame]);
    foreign.client.request(denied, 4, &[f.epoch, frame, 9]);
    let events = fence(&mut f.server, &mut foreign.client);
    assert!(!events.iter().any(|e| e.sender == denied && e.opcode == 4));
    assert_eq!(
        super::super::lease_timing_for_test(&f.server.state),
        Some(timing)
    );
    let (frame, _) = f.frame(&mut gl);
    let epoch = f.epoch;
    let events = export(&mut f, frame, epoch, 10);
    metadata(&f, &events, frame, 10);
    f.arm(frame);
    let (frame, _) = f.frame(&mut gl);
    let events = f.action(frame, 1, 1, 8, 8);
    f.accepted(&events, 1);
}

#[test]
fn actual_v2_export_quota_is_bounded_without_advancing_lease_clock() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::new_with_version(2);
    let epoch = f.epoch;
    let timing = super::super::lease_timing_for_test(&f.server.state).unwrap();
    let (buffer, _file) = buffer(&mut f.server, &mut f.capture.client, (W, H), [0x5a; 4]);
    let session = f.capture.client.new_id();
    f.capture
        .client
        .request(f.capture.manager, 0, &[session, f.source, 0]);
    fence(&mut f.server, &mut f.capture.client);
    let mut previous = 0;
    for index in 0..=1024 {
        assert!(
            Instant::now() < timing.2,
            "real quota exercise must finish within original 30s, not confuse TTL with quota"
        );
        let frame = f.capture.client.new_id();
        f.capture.client.request(session, 0, &[frame]);
        f.capture.client.request(frame, 1, &[buffer]);
        f.capture.client.request(frame, 3, &[]);
        fence(&mut f.server, &mut f.capture.client);
        let queue = f
            .server
            .state
            .image_capture_pending
            .as_ref()
            .unwrap()
            .clone();
        let capture_started = Instant::now();
        fulfill_test_toplevel_capture(&mut gl.renderer, &mut None, &f.server.state, &queue);
        let events = fence(&mut f.server, &mut f.capture.client);
        assert!(events.iter().any(|e| e.sender == frame && e.opcode == 3));
        let events = export(&mut f, frame, epoch, index + 1);
        assert!(
            capture_started.elapsed() <= Duration::from_secs(1),
            "quota export must remain within proof freshness, including actual capture"
        );
        assert!(
            Instant::now() < timing.2,
            "quota rejection must precede original expiry"
        );
        if index == 1024 {
            assert!(
                events.iter().any(|e| e.sender == f.lease
                    && e.opcode == 3
                    && e.args == [super::super::DENIED])
            );
            rejected(&mut f, &events);
        } else {
            let proof = metadata(&f, &events, frame, index + 1);
            let serial = number(&proof[3..5]);
            assert!(serial > previous);
            previous = serial;
            assert_eq!(
                super::super::lease_timing_for_test(&f.server.state),
                Some(timing)
            );
        }
        f.capture.client.request(frame, 0, &[]);
        fence(&mut f.server, &mut f.capture.client);
    }
}

#[test]
fn actual_v2_old_capture_and_original_ttl_never_export_new_authority() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for ttl in [false, true] {
        let mut f = Fixture::new_with_version(2);
        let timing = super::super::lease_timing_for_test(&f.server.state).unwrap();
        let (frame, _) = f.frame(&mut gl);
        let epoch = f.epoch;
        let target = if ttl {
            timing.2
        } else {
            Instant::now() + Duration::from_millis(1001)
        };
        let deadline = target + Duration::from_secs(2);
        let mut expired = false;
        while Instant::now() < target || (ttl && !expired) {
            f.server.dispatch_timers(Duration::from_millis(10));
            let events = fence(&mut f.server, &mut f.capture.client);
            for event in events
                .iter()
                .filter(|e| e.sender == f.lease && e.opcode == 3)
            {
                assert!(ttl && Instant::now() >= timing.2);
                assert_eq!(event.args, [5]);
                assert!(!expired);
                expired = true;
            }
            if let Some(current) = super::super::lease_timing_for_test(&f.server.state) {
                assert_eq!(current, timing);
            }
            assert!(Instant::now() < deadline, "bounded original timer expiry");
        }
        let events = export(&mut f, frame, epoch, 99);
        assert!(!events.iter().any(|e| e.sender == f.lease && e.opcode == 4));
        if ttl {
            assert!(expired);
            assert!(super::super::lease_timing_for_test(&f.server.state).is_none());
        } else {
            assert!(
                Instant::now() < timing.2,
                "freshness rejection must precede original TTL"
            );
            rejected(&mut f, &events);
        }
    }
}
