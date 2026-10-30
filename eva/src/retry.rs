//! Clock-driven wake recovery. No sleeping tasks, raw remote error text, or
//! authority to restore a disabled native wake request live in this controller.
use crate::{
    errors::{self, Recovery},
    handoff::Source,
};
use serde_json::{Value, json};
use std::io;

pub const MAX_FAILURES: u8 = 8;
pub const MAX_DELAY_MS: u64 = 60_000;
pub const RATE_LIMIT_MIN_MS: u64 = 30_000;
pub const HEALTHY_RESET_MS: u64 = 5_000;

#[derive(Clone, Copy)]
struct Failure {
    code: &'static str,
    text: &'static str,
    exhausted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Retry,
    Stop,
}

pub struct WakeRecovery {
    source: Option<Source>,
    requested: bool,
    base_ms: u64,
    random: u64,
    failures: u8,
    next_ms: Option<u64>,
    failure: Option<Failure>,
    blocked: bool,
    cancelled: bool,
    healthy_since_ms: Option<u64>,
}
impl WakeRecovery {
    pub fn new(base_ms: u64, seed: u64) -> Self {
        Self {
            source: None,
            requested: false,
            base_ms: base_ms.clamp(1000, MAX_DELAY_MS),
            random: seed.max(1),
            failures: 0,
            next_ms: None,
            failure: None,
            blocked: false,
            cancelled: false,
            healthy_since_ms: None,
        }
    }
    /// Missing IPC observations do not invent a user toggle or erase a pending
    /// delay. Full native identity changes or a new enable request reset it.
    pub fn observe(&mut self, source: Option<Source>, requested: bool) -> bool {
        let Some(source) = source else {
            self.healthy_since_ms = None;
            return false;
        };
        let changed = self.source.as_ref() != Some(&source) || self.requested != requested;
        if !requested {
            self.source = Some(source);
            self.requested = false;
            self.next_ms = None;
            self.healthy_since_ms = None;
            if !self.blocked {
                self.successful();
            }
            return changed;
        }
        // A failed native disable or temporary IPC gap cannot make the same
        // blocked source look like the user's explicit new enable request.
        let same_wake_request = self.source.as_ref().is_some_and(|old| {
            old.wm_instance_id == source.wm_instance_id
                && old.wake_generation == source.wake_generation
        });
        if (same_wake_request || (self.source.is_none() && self.cancelled))
            && (self.blocked || self.cancelled)
        {
            self.source = Some(source);
            self.requested = true;
            return changed;
        }
        if changed {
            self.successful();
        }
        self.source = Some(source);
        self.requested = true;
        changed
    }
    pub fn disable(&mut self) {
        self.requested = false;
        self.successful();
        self.cancelled = true;
    }
    pub fn source(&self) -> Option<&Source> {
        self.source.as_ref()
    }
    pub fn successful(&mut self) {
        self.failures = 0;
        self.next_ms = None;
        self.failure = None;
        self.blocked = false;
        self.cancelled = false;
        self.healthy_since_ms = None;
    }
    pub fn standby(&mut self, now_ms: u64) {
        if self.blocked || self.cancelled || !self.requested {
            return;
        }
        let since = *self.healthy_since_ms.get_or_insert(now_ms);
        if now_ms.saturating_sub(since) >= HEALTHY_RESET_MS {
            self.successful();
            self.healthy_since_ms = Some(now_ms);
        }
    }
    pub fn pause(&mut self) {
        self.healthy_since_ms = None;
    }
    pub fn ready(&self, now_ms: u64) -> bool {
        self.requested
            && !self.blocked
            && !self.cancelled
            && self.next_ms.is_none_or(|next| now_ms >= next)
    }
    pub fn started(&mut self) {
        self.next_ms = None;
        self.healthy_since_ms = None;
    }
    pub fn failed(&mut self, error: &io::Error, now_ms: u64) -> Decision {
        self.healthy_since_ms = None;
        self.failures = self.failures.saturating_add(1);
        let category = errors::recovery(error);
        let (code, text) = errors::describe(error);
        let exhausted = self.failures >= MAX_FAILURES;
        self.failure = Some(Failure {
            code,
            text,
            exhausted,
        });
        if category == Recovery::Permanent || exhausted {
            self.next_ms = None;
            self.blocked = true;
            return Decision::Stop;
        }
        let exponent = (self.failures - 1).min(16) as u32;
        let nominal = self
            .base_ms
            .saturating_mul(1u64 << exponent)
            .min(MAX_DELAY_MS);
        // Small deterministic PRNG; callers seed once from process/time entropy.
        // Tests supply a fixed seed and advance integer clocks without sleeps.
        self.random ^= self.random << 13;
        self.random ^= self.random >> 7;
        self.random ^= self.random << 17;
        let spread = nominal / 5;
        let delay = nominal.saturating_sub(spread) + (self.random % (spread * 2 + 1));
        let floor = if category == Recovery::RateLimited {
            self.base_ms.max(RATE_LIMIT_MIN_MS)
        } else {
            self.base_ms
        };
        self.next_ms = Some(now_ms.saturating_add(delay.clamp(floor, MAX_DELAY_MS)));
        Decision::Retry
    }
    pub fn status(&self, now_ms: u64, wall_ms: u64) -> Option<Value> {
        let failure = self.failure?;
        if !self.blocked && self.next_ms.is_none() {
            return None;
        }
        let remaining = self.next_ms.map(|next| next.saturating_sub(now_ms));
        let text = if let Some(remaining) = remaining {
            format!(
                "{} {} 秒后重试（第 {} 次）。",
                failure.text,
                remaining.div_ceil(1000),
                self.failures
            )
        } else if failure.exhausted {
            format!(
                "{} 连续重试失败，语音唤醒已暂停，请检查后重新开启。",
                failure.text
            )
        } else {
            format!("{} 语音唤醒已暂停，修复后请重新开启。", failure.text)
        };
        Some(
            json!({"state":"error","text":text,"error_code":failure.code,"microphone_active":false,"level":0.0,
            "retryable":!self.blocked,"retry_state":if self.blocked{"stopped"}else{"waiting"},
            "retry_attempt":self.failures,"retry_limit":MAX_FAILURES,"retry_in_ms":remaining,
            "retry_at_ms":remaining.map(|remaining|wall_ms.saturating_add(remaining))}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn source(generation: u64) -> Source {
        Source {
            wm_instance_id: "wm".into(),
            wake_generation: generation,
            assistant_generation: 0,
        }
    }
    fn controller(seed: u64) -> WakeRecovery {
        let mut retry = WakeRecovery::new(3000, seed);
        retry.observe(Some(source(1)), true);
        retry
    }
    #[test]
    fn capped_backoff_and_jitter_use_virtual_time_and_stop_after_bounded_failures() {
        let mut retry = controller(7);
        let mut now = 100;
        let mut previous = 0;
        for attempt in 1..MAX_FAILURES {
            assert_eq!(
                retry.failed(&io::Error::other("realtime_connection_failed"), now),
                Decision::Retry
            );
            let status = retry.status(now, 10_000).unwrap();
            let delay = status["retry_in_ms"].as_u64().unwrap();
            assert!((3000..=MAX_DELAY_MS).contains(&delay));
            if attempt <= 4 {
                assert!(delay > previous);
            }
            previous = delay;
            assert!(!retry.ready(now));
            assert!(retry.ready(now + delay));
            assert_eq!(status["retry_at_ms"], 10_000 + delay);
            assert_eq!(retry.status(now + delay - 1, 0).unwrap()["retry_in_ms"], 1);
            now += delay;
            retry.started();
        }
        assert_eq!(
            retry.failed(&io::Error::other("private transport failure"), now),
            Decision::Stop
        );
        assert!(!retry.ready(now + 1_000_000));
        let status = retry.status(now, 0).unwrap();
        assert_eq!(status["retry_attempt"], MAX_FAILURES);
        assert_eq!(status["retry_state"], "stopped");
        assert!(status["retry_at_ms"].is_null());
        let mut other = controller(99);
        other.failed(&io::Error::other("connect_timeout"), 0);
        let mut first = controller(7);
        first.failed(&io::Error::other("connect_timeout"), 0);
        assert_ne!(
            other.status(0, 0).unwrap()["retry_in_ms"],
            first.status(0, 0).unwrap()["retry_in_ms"]
        );
    }
    #[test]
    fn rate_limits_cool_down_and_permanent_errors_never_reconnect() {
        let mut retry = controller(1);
        retry.failed(&io::Error::other("rate_limited"), 0);
        let remaining = retry.status(0, 0).unwrap()["retry_in_ms"].as_u64().unwrap();
        assert!((RATE_LIMIT_MIN_MS..=MAX_DELAY_MS).contains(&remaining));
        for code in [
            "auth_failed",
            "model_access_denied",
            "wake_configuration_mismatch",
            "budget_exceeded",
            "quota_exceeded",
        ] {
            let mut retry = controller(1);
            assert_eq!(
                retry.failed(&io::Error::other(format!("{code}: secret-token")), 0),
                Decision::Stop
            );
            assert!(!retry.ready(u64::MAX));
            assert!(
                !retry
                    .status(0, 0)
                    .unwrap()
                    .to_string()
                    .contains("secret-token")
            );
            retry.observe(
                Some(Source {
                    assistant_generation: 99,
                    ..source(1)
                }),
                true,
            );
            assert!(
                !retry.ready(u64::MAX),
                "a manual conversation is not a new wake enable request"
            );
        }
    }
    #[test]
    fn disable_cancel_and_ipc_gaps_cannot_resurrect_a_waiting_or_blocked_source() {
        let mut retry = controller(2);
        retry.failed(&io::Error::other("connect_timeout"), 0);
        let deadline = retry.status(0, 0).unwrap()["retry_in_ms"].as_u64().unwrap();
        assert!(!retry.observe(None, false));
        assert!(!retry.observe(Some(source(1)), true));
        assert!(!retry.ready(deadline - 1));
        retry.observe(Some(source(2)), false);
        assert!(!retry.ready(u64::MAX));
        assert!(retry.status(0, 0).is_none());
        retry.observe(Some(source(3)), true);
        assert!(retry.ready(0));
        retry.failed(&io::Error::other("auth_failed"), 0);
        retry.observe(None, false);
        retry.observe(Some(source(3)), true);
        assert!(!retry.ready(u64::MAX));
        // Internal native disable preserves the terminal explanation; a new
        // user generation resets it, including after a complete WM restart.
        retry.observe(Some(source(4)), false);
        assert_eq!(retry.status(0, 0).unwrap()["retry_state"], "stopped");
        retry.observe(Some(source(5)), true);
        assert!(retry.ready(0));
        assert!(retry.status(0, 0).is_none());
        retry.failed(&io::Error::other("connect_timeout"), 0);
        retry.disable();
        assert!(!retry.ready(u64::MAX));
        assert!(retry.status(0, 0).is_none());
    }
    #[test]
    fn brief_connections_do_not_reset_failures_but_healthy_standby_or_rotation_does() {
        let mut retry = controller(11);
        retry.failed(&io::Error::other("audio_capture_device_unavailable"), 0);
        retry.started();
        retry.standby(100);
        retry.standby(100 + HEALTHY_RESET_MS - 1);
        retry.failed(&io::Error::other("audio_capture_device_unavailable"), 6000);
        assert_eq!(retry.status(6000, 0).unwrap()["retry_attempt"], 2);
        retry.started();
        retry.standby(7000);
        retry.standby(7000 + HEALTHY_RESET_MS);
        retry.failed(&io::Error::other("connect_timeout"), 13000);
        assert_eq!(retry.status(13000, 0).unwrap()["retry_attempt"], 1);
        retry.successful();
        assert!(retry.ready(0));
        assert!(retry.status(0, 0).is_none());
    }
    #[test]
    fn full_source_identity_resets_backoff_even_when_wake_generation_is_reused() {
        for changed in [
            Source {
                wm_instance_id: "new-wm".into(),
                ..source(1)
            },
            Source {
                assistant_generation: 7,
                ..source(1)
            },
        ] {
            let mut retry = controller(1);
            retry.failed(&io::Error::other("connect_timeout"), 0);
            assert!(!retry.ready(0));
            assert!(retry.observe(Some(changed), true));
            assert!(retry.ready(0));
            assert!(retry.status(0, 0).is_none());
        }
    }
    #[test]
    fn failed_native_cancel_cannot_look_like_a_new_enable_on_the_same_source() {
        let mut retry = controller(7);
        retry.disable();
        // The native write timed out: later status still says the previous
        // generation was enabled. Keep the local cancellation latched.
        retry.observe(None, false);
        retry.observe(Some(source(1)), true);
        assert!(!retry.ready(u64::MAX));
        retry.observe(
            Some(Source {
                assistant_generation: 99,
                ..source(1)
            }),
            true,
        );
        assert!(
            !retry.ready(u64::MAX),
            "manual standard sessions must not undo an explicit wake cancellation"
        );
        retry.observe(Some(source(2)), true);
        assert!(retry.ready(0));
        let mut unknown = WakeRecovery::new(3000, 7);
        unknown.disable();
        unknown.observe(Some(source(9)), true);
        assert!(!unknown.ready(u64::MAX));
        unknown.observe(Some(source(10)), false);
        unknown.observe(Some(source(11)), true);
        assert!(unknown.ready(0));
    }
}
