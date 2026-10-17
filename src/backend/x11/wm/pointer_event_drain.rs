//! An on-demand fence after a pointer query, shared by the native X11 loops.
//!
//! QueryPointer can read a newer release into the connection's userspace event
//! queue. A missing-button watchdog must let that queue drain before acting on
//! its snapshot. Tokens keep a late fence from ending a subsequent gesture.

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub(crate) struct PointerEventDrain {
    next: AtomicU64,
    pending: AtomicU64,
}

impl PointerEventDrain {
    pub(crate) fn request(&self) -> u64 {
        let mut token = self.next.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        if token == 0 {
            token = self.next.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        }
        self.pending.store(token, Ordering::Release);
        token
    }

    pub(crate) fn requested(&self) -> bool {
        self.pending.load(Ordering::Acquire) != 0
    }

    /// An exhausted event budget is not an empty queue. Callers also recheck
    /// after delivering a coalesced batch, because callbacks can read replies.
    pub(crate) fn complete_if_empty(&self, empty: bool) -> Option<u64> {
        if !empty {
            return None;
        }
        let token = self.pending.swap(0, Ordering::AcqRel);
        (token != 0).then_some(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_exhaustion_preserves_the_requested_fence() {
        let drain = PointerEventDrain::default();
        let token = drain.request();
        assert_eq!(drain.complete_if_empty(false), None);
        assert!(drain.requested());
        assert_eq!(drain.complete_if_empty(true), Some(token));
        assert!(!drain.requested());
        assert_eq!(drain.complete_if_empty(true), None);
    }

    #[test]
    fn later_gestures_receive_distinct_tokens() {
        let drain = PointerEventDrain::default();
        let first = drain.request();
        let second = drain.request();
        assert_ne!(first, second);
        assert_ne!(second, 0);
        assert_eq!(drain.complete_if_empty(true), Some(second));
    }

    #[test]
    fn rollover_never_uses_the_empty_sentinel() {
        let drain = PointerEventDrain::default();
        drain.next.store(u64::MAX, Ordering::Relaxed);
        let token = drain.request();
        assert_ne!(token, 0);
        assert_eq!(drain.complete_if_empty(true), Some(token));
    }
}
