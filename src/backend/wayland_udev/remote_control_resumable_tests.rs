//! Native v4 protocol and actual keyboard recipients, not compositor hover.
use super::*;
use std::time::Instant;

impl Fixture {
    fn pause(&mut self, sequence: u32, generation: u32) -> Vec<Event> {
        self.capture
            .client
            .request(self.lease, 4, &[self.epoch, sequence, generation]);
        sync(&mut self.server);
        events(&mut self.capture.client)
    }
    fn resume(&mut self, sequence: u32, generation: u32, frame: u32) -> Vec<Event> {
        self.capture
            .client
            .request(self.lease, 5, &[self.epoch, sequence, generation, frame]);
        sync(&mut self.server);
        events(&mut self.capture.client)
    }
    fn transition(&self, ev: &[Event], opcode: u16, sequence: u32, generation: u32) {
        assert!(ev.iter().any(|e| e.sender == self.lease
            && e.opcode == opcode
            && e.args == [self.epoch, sequence, generation]));
        assert!(!ev.iter().any(|e| e.sender == self.lease && e.opcode == 4));
    }
}

#[test]
fn wire_control_resumable_pause_releases_real_keys_and_keeps_original_lifetime() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for ttl in [None, Some(300_000)] {
        let mut f = Fixture::with_duration(7, 5, false, Some(0), true, ttl);
        let frame = f.frame(&mut gl);
        f.arm(frame);
        let created = f
            .server
            .state
            .remote_control
            .active
            .as_ref()
            .unwrap()
            .created;
        let deadline = f
            .server
            .state
            .remote_control
            .active
            .as_ref()
            .unwrap()
            .deadline;
        let result = f.key(1, 42, true);
        f.accepted(&result, 1);
        assert_eq!(f.target_keys(), vec![(42, 1)]);
        let result = f.pause(2, 1);
        f.transition(&result, 5, 2, 1);
        assert_eq!(
            f.target_keys(),
            vec![(42, 0)],
            "pause must balance actual delivered Shift"
        );
        let lease = f.server.state.remote_control.active.as_ref().unwrap();
        assert!(!lease.armed && lease.held.is_empty());
        assert_eq!(lease.created, created);
        assert_eq!(lease.deadline, deadline);
        assert_eq!(lease.sequence, 2);
        let frame = f.frame(&mut gl);
        let result = f.resume(3, 2, frame);
        f.transition(&result, 6, 3, 2);
        assert!(
            f.target_keys().is_empty(),
            "resume is not a synthetic input event"
        );
        let lease = f.server.state.remote_control.active.as_ref().unwrap();
        assert!(lease.armed && lease.paused_at.is_none());
        assert_eq!(lease.created, created);
        assert_eq!(lease.deadline, deadline);
        let result = f.key(4, 30, true);
        f.accepted(&result, 4);
        let delivered = events(&mut f.app.client);
        assert!(
            delivered
                .iter()
                .any(|e| e.sender == f.keyboard && e.opcode == 3 && e.args[2..] == [30, 1])
        );
        assert!(
            delivered
                .iter()
                .filter(|e| e.sender == f.keyboard && e.opcode == 4)
                .all(|e| e.args[1..] == [0, 0, 0, 0]),
            "Shift must not survive in lease XKB state"
        );
        let result = f.pause(5, 2);
        f.transition(&result, 5, 5, 2);
        assert_eq!(f.target_keys(), vec![(30, 0)]);
        f.other_empty();
    }
}

#[test]
fn wire_control_resumable_opt_in_and_transition_replays_fail_closed() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    // A v4 bind using the old creation request is still one-shot.
    let mut old = Fixture::with_scroll(7, 4, false);
    let frame = old.frame(&mut gl);
    old.arm(frame);
    let result = old.pause(1, 1);
    old.denied(&result);
    for mode in 0..6 {
        let mut f = Fixture::resumable();
        let frame = f.frame(&mut gl);
        f.arm(frame);
        let result = f.pause(1, 1);
        f.transition(&result, 5, 1, 1);
        let result = match mode {
            0 => f.pause(2, 1),         // duplicate pause
            1 => f.key(2, 30, true),    // input while paused
            2 => f.resume(2, 2, frame), // pre-pause proof
            3 => {
                let new = f.frame(&mut gl);
                f.resume(2, 1, new)
            } // stale generation
            4 => {
                let new = f.frame(&mut gl);
                f.resume(3, 2, new)
            } // sequence gap
            _ => {
                // Legacy arm must not bypass the new resume checks.
                f.capture.client.request(f.lease, 1, &[f.epoch, frame]);
                sync(&mut f.server);
                events(&mut f.capture.client)
            }
        };
        f.denied(&result);
        assert!(f.server.state.remote_control.active.is_none());
    }
}

#[test]
fn wire_control_resumable_focus_source_expiry_and_revoke_remain_terminal() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for mode in 0..4 {
        let mut f = Fixture::resumable();
        let frame = f.frame(&mut gl);
        f.arm(frame);
        let result = f.pause(1, 1);
        f.transition(&result, 5, 1, 1);
        match mode {
            0 => {
                focus(&mut f.server, &f.other);
                focus(&mut f.server, &f.app);
            }
            1 => {
                f.app.unmap(&mut f.server);
                f.app.remap(&mut f.server);
            }
            2 => {
                // Deterministic boundary check, not natural-expiry evidence.
                f.server
                    .state
                    .remote_control
                    .active
                    .as_mut()
                    .unwrap()
                    .deadline = Instant::now();
            }
            _ => super::super::revoke(&mut f.server.state, super::super::SEAT),
        }
        events(&mut f.app.client);
        events(&mut f.other.client);
        let result = f.resume(2, 2, frame);
        f.denied(&result);
        assert!(f.server.state.remote_control.active.is_none());
        let result = f.key(3, 30, true);
        assert!(
            !result
                .iter()
                .any(|e| e.sender == f.lease && matches!(e.opcode, 3 | 5 | 6))
        );
        assert!(f.target_keys().is_empty());
        f.other_empty();
    }
}

#[test]
fn wire_control_resumable_invalid_axes_and_exhausted_budget_never_acknowledge() {
    let mut malformed = Fixture::with_mode(7, 4, false, Some(4), true);
    assert_eq!(malformed.epoch, 0);
    assert!(malformed.server.state.remote_control.active.is_none());
    malformed.other_empty();
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::resumable();
    let frame = f.frame(&mut gl);
    f.arm(frame);
    let result = f.key(1, 42, true);
    f.accepted(&result, 1);
    assert_eq!(f.target_keys(), vec![(42, 1)]);
    // Force an exhausted token budget with a far-negative balance so scheduler
    // jitter cannot refill a token. Denial must still release the held Shift.
    f.server
        .state
        .remote_control
        .active
        .as_mut()
        .unwrap()
        .tokens = -1_000_000.0;
    let result = f.pause(2, 1);
    assert!(result.iter().any(|e| e.sender == f.lease && e.opcode == 4));
    assert!(!result.iter().any(|e| e.sender == f.lease && e.opcode == 5));
    assert_eq!(f.target_keys(), vec![(42, 0)]);
    assert!(f.server.state.remote_control.active.is_none());
    f.other_empty();
}

#[test]
fn wire_control_timed_expiry_releases_held_keys_and_cannot_resume() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut f = Fixture::with_duration(7, 5, false, Some(0), true, Some(300_000));
    let frame = f.frame(&mut gl);
    f.arm(frame);
    let result = f.key(1, 42, true);
    f.accepted(&result, 1);
    assert_eq!(f.target_keys(), vec![(42, 1)]);
    // Deterministic deadline boundary, not evidence of real elapsed 300 seconds.
    f.server
        .state
        .remote_control
        .active
        .as_mut()
        .unwrap()
        .deadline = Instant::now();
    let result = f.pause(2, 1);
    // Expiry must balance exactly the delivered press before we assert that
    // revocation produced no additional input on either recipient.
    assert_eq!(f.target_keys(), vec![(42, 0)]);
    f.denied(&result);
    assert!(f.server.state.remote_control.active.is_none());
    let result = f.resume(3, 2, frame);
    assert!(
        !result
            .iter()
            .any(|e| e.sender == f.lease && matches!(e.opcode, 3 | 5 | 6))
    );
    assert!(f.target_keys().is_empty());
}
