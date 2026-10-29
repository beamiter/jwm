//! Deterministic actor state; callers supply monotonic milliseconds.
use crate::protocol::AssistantPermit;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Idle,
    Connecting,
    Listening,
    Planning,
    Confirming,
    Executing,
    Responding,
    FollowUp,
    Closing,
    Error,
}

#[derive(Debug, Clone)]
pub struct Limits {
    pub connect_ms: u64,
    pub first_speech_ms: u64,
    pub utterance_ms: u64,
    pub plan_ms: u64,
    pub confirmation_ms: u64,
    pub follow_up_ms: u64,
    pub start_grace_ms: u64,
    pub session_ms: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            connect_ms: 8000,
            first_speech_ms: 8000,
            utterance_ms: 20000,
            plan_ms: 12000,
            confirmation_ms: 15000,
            follow_up_ms: 5000,
            start_grace_ms: 1500,
            session_ms: 300000,
        }
    }
}

#[derive(Debug)]
pub struct Session {
    pub state: State,
    pub session_epoch: u64,
    pub turn_epoch: u64,
    pub permit: Option<AssistantPermit>,
    pub limits: Limits,
    pub close_reason: Option<String>,
    started_at: u64,
    entered_at: u64,
    speech_at: Option<u64>,
    follow_up_deadline: Option<u64>,
    candidate: Option<(u64, u64)>,
    plan_claimed: bool,
}
impl Default for Session {
    fn default() -> Self {
        Self::new(Limits::default())
    }
}
impl Session {
    pub fn new(limits: Limits) -> Self {
        Self {
            state: State::Idle,
            session_epoch: 0,
            turn_epoch: 0,
            permit: None,
            limits,
            close_reason: None,
            started_at: 0,
            entered_at: 0,
            speech_at: None,
            follow_up_deadline: None,
            candidate: None,
            plan_claimed: false,
        }
    }
    pub fn activate(&mut self, permit: AssistantPermit, now: u64) -> Result<u64, String> {
        if self.state != State::Idle {
            return Err("session_already_active".into());
        }
        self.session_epoch = self.session_epoch.wrapping_add(1);
        self.turn_epoch = self.turn_epoch.wrapping_add(1);
        self.permit = Some(permit);
        self.started_at = now;
        self.enter(State::Connecting, now);
        self.close_reason = None;
        self.speech_at = None;
        self.candidate = None;
        self.plan_claimed = false;
        Ok(self.session_epoch)
    }
    pub fn ready(&mut self, epoch: u64, now: u64) -> bool {
        if !self.matches(epoch, self.turn_epoch) || self.state != State::Connecting {
            return false;
        }
        self.enter(State::Listening, now);
        true
    }
    pub fn capture_allowed(&self) -> bool {
        matches!(
            self.state,
            State::Connecting | State::Listening | State::FollowUp
        )
    }
    pub fn upload_allowed(&self) -> bool {
        matches!(self.state, State::Listening | State::FollowUp)
    }
    pub fn execution_allowed(&self) -> bool {
        self.state == State::Executing && self.permit.is_some()
    }
    /// Presentation only: the original follow-up deadline never moves with
    /// microphone levels, heartbeats or the bounded speech-start grace.
    pub fn follow_up_remaining_ms(&self, now: u64) -> Option<u64> {
        (self.state == State::FollowUp)
            .then_some(self.follow_up_deadline)
            .flatten()
            .map(|deadline| deadline.saturating_sub(now))
    }
    pub fn matches(&self, epoch: u64, turn: u64) -> bool {
        self.session_epoch == epoch
            && self.turn_epoch == turn
            && self.permit.is_some()
            && !matches!(self.state, State::Idle | State::Closing | State::Error)
    }
    pub fn local_speech_candidate(&mut self, epoch: u64, captured_at: u64, now: u64) -> bool {
        if !self.matches(epoch, self.turn_epoch)
            || !self.upload_allowed()
            || self.candidate.is_some()
        {
            return false;
        }
        if self.state == State::FollowUp && self.follow_up_deadline.is_none_or(|d| captured_at > d)
        {
            return false;
        }
        // A single candidate can postpone expiry only once; noise never resets it.
        self.candidate = Some((captured_at, now.saturating_add(self.limits.start_grace_ms)));
        true
    }
    pub fn speech_started(&mut self, epoch: u64, now: u64) -> bool {
        if !self.matches(epoch, self.turn_epoch) || !self.upload_allowed() {
            return false;
        }
        let captured = self
            .candidate
            .take()
            .filter(|(_, expiry)| now <= *expiry)
            .map_or(now, |(capture, _)| capture);
        if self.state == State::FollowUp && self.follow_up_deadline.is_none_or(|d| captured > d) {
            return false;
        }
        if self.state == State::FollowUp {
            self.turn_epoch = self.turn_epoch.wrapping_add(1);
            self.plan_claimed = false;
            self.enter(State::Listening, now);
        }
        self.speech_at = Some(captured);
        self.follow_up_deadline = None;
        true
    }
    pub fn input_committed(&mut self, epoch: u64, turn: u64, now: u64) -> bool {
        if !self.matches(epoch, turn) || self.state != State::Listening || self.speech_at.is_none()
        {
            return false;
        }
        self.enter(State::Planning, now);
        true
    }
    pub fn claim_plan(&mut self, epoch: u64, turn: u64, confirmation: bool, now: u64) -> bool {
        if !self.matches(epoch, turn) || self.state != State::Planning || self.plan_claimed {
            return false;
        }
        self.plan_claimed = true;
        self.enter(
            if confirmation {
                State::Confirming
            } else {
                State::Executing
            },
            now,
        );
        true
    }
    pub fn confirm(&mut self, epoch: u64, turn: u64, now: u64) -> bool {
        if !self.matches(epoch, turn)
            || self.state != State::Confirming
            || now.saturating_sub(self.entered_at) > self.limits.confirmation_ms
        {
            return false;
        }
        self.enter(State::Executing, now);
        true
    }
    pub fn respond(&mut self, epoch: u64, turn: u64, now: u64) -> bool {
        if !self.matches(epoch, turn) || !matches!(self.state, State::Planning | State::Executing) {
            return false;
        }
        self.enter(State::Responding, now);
        true
    }
    /// Called only by the local renderer after the device and queue are drained.
    pub fn feedback_completed(&mut self, epoch: u64, turn: u64, now: u64) -> bool {
        if !self.matches(epoch, turn) || self.state != State::Responding {
            return false;
        }
        if now.saturating_sub(self.started_at) >= self.limits.session_ms {
            self.cancel("session_timeout", now);
            return true;
        }
        self.enter(State::FollowUp, now);
        self.follow_up_deadline = Some(now.saturating_add(self.limits.follow_up_ms));
        self.speech_at = None;
        self.candidate = None;
        true
    }
    pub fn cancel(&mut self, reason: &str, now: u64) {
        self.permit = None;
        self.session_epoch = self.session_epoch.wrapping_add(1);
        self.turn_epoch = self.turn_epoch.wrapping_add(1);
        self.candidate = None;
        self.speech_at = None;
        self.follow_up_deadline = None;
        self.plan_claimed = true;
        self.close_reason = Some(reason.into());
        self.enter(State::Closing, now);
    }
    pub fn closed(&mut self, now: u64) {
        if matches!(self.state, State::Closing | State::Error) {
            self.enter(State::Idle, now);
        }
    }
    pub fn tick(&mut self, now: u64) -> Option<String> {
        let elapsed = now.saturating_sub(self.entered_at);
        let reason = match self.state {
            State::Connecting if elapsed >= self.limits.connect_ms => Some("connect_timeout"),
            State::Listening
                if self.speech_at.is_none() && elapsed >= self.limits.first_speech_ms =>
            {
                Some("first_speech_timeout")
            }
            State::Listening
                if self
                    .speech_at
                    .is_some_and(|start| now.saturating_sub(start) >= self.limits.utterance_ms) =>
            {
                Some("utterance_too_long")
            }
            State::Planning if elapsed >= self.limits.plan_ms => Some("plan_timeout"),
            State::Confirming if elapsed >= self.limits.confirmation_ms => {
                Some("confirmation_expired")
            }
            State::FollowUp
                if self
                    .follow_up_deadline
                    .is_some_and(|d| now > d.saturating_add(20)) =>
            {
                if self.candidate.is_some_and(|(_, expiry)| now <= expiry) {
                    None
                } else {
                    Some("follow_up_timeout")
                }
            }
            _ => None,
        };
        reason.map(|reason| {
            self.cancel(reason, now);
            reason.into()
        })
    }
    fn enter(&mut self, state: State, now: u64) {
        self.state = state;
        self.entered_at = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn listening() -> Session {
        let mut s = Session::default();
        let e = s
            .activate(
                AssistantPermit {
                    assistant_session_id: "s1".into(),
                    generation: 1,
                },
                0,
            )
            .unwrap();
        assert!(s.ready(e, 1));
        s
    }
    #[test]
    fn idle_and_processing_release_capture_and_old_events_cannot_reopen() {
        let mut s = listening();
        let e = s.session_epoch;
        let t = s.turn_epoch;
        assert!(s.speech_started(e, 10));
        assert!(s.input_committed(e, t, 100));
        assert!(!s.capture_allowed());
        s.cancel("esc", 101);
        assert!(!s.execution_allowed());
        assert!(!s.ready(e, 102));
        assert!(!s.speech_started(e, 102));
        s.closed(103);
        assert!(!s.capture_allowed());
    }
    fn followup() -> Session {
        let mut s = listening();
        let e = s.session_epoch;
        let t = s.turn_epoch;
        s.speech_started(e, 1);
        s.input_committed(e, t, 2);
        s.respond(e, t, 3);
        s.feedback_completed(e, t, 4);
        s
    }
    #[test]
    fn near_deadline_speech_survives_remote_vad_delay() {
        let mut s = followup();
        let e = s.session_epoch;
        assert!(s.local_speech_candidate(e, 5003, 5010));
        assert!(s.tick(5100).is_none());
        assert!(s.speech_started(e, 5200));
        assert_eq!(s.state, State::Listening);
        assert!(s.tick(8000).is_none());
    }
    #[test]
    fn noise_candidate_has_bounded_grace() {
        let mut s = followup();
        let e = s.session_epoch;
        s.local_speech_candidate(e, 4999, 5000);
        assert!(!s.local_speech_candidate(e, 5000, 6000));
        assert_eq!(s.tick(6501).as_deref(), Some("follow_up_timeout"));
    }
    #[test]
    fn follow_up_countdown_does_not_extend_for_speech_start_grace() {
        let mut s = followup();
        assert_eq!(s.follow_up_remaining_ms(4), Some(5000));
        assert_eq!(s.follow_up_remaining_ms(4004), Some(1000));
        assert!(s.local_speech_candidate(s.session_epoch, 5003, 5004));
        assert_eq!(s.follow_up_remaining_ms(5100), Some(0));
        assert!(s.tick(5100).is_none());
        assert!(s.speech_started(s.session_epoch, 5200));
        assert_eq!(s.follow_up_remaining_ms(5200), None);
        s.cancel("test", 5201);
        assert_eq!(s.follow_up_remaining_ms(5201), None);
    }
}
