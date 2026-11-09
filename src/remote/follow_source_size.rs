//! Opt-in local viewport -> source resize. Presentation, never decode, gates requests.
use super::RemoteResult;
use super::window_resize::{ResizeRequest, ResizeResult, ResizeStatus};
use std::time::{Duration, Instant};
const INTERVAL: Duration = Duration::from_millis(250);
const TIMEOUT: Duration = Duration::from_secs(5);
#[derive(Clone, Copy)]
struct Presented {
    scope: (u64, u64),
    size: (u16, u16),
    revision: u64,
}
struct Pending {
    request: ResizeRequest,
    revision: u64,
    started: Instant,
    applied: Option<ResizeResult>,
}
#[derive(Default)]
pub(super) struct FollowSourceSize {
    viewport: Option<(u16, u16)>,
    revision: u64,
    served: u64,
    presented: Option<Presented>,
    pending: Option<Pending>,
    last_request: Option<Instant>,
    next_id: u64,
    stopped: bool,
}
impl FollowSourceSize {
    pub(super) fn revision(&self) -> u64 {
        self.revision
    }
    pub(super) fn viewport(&mut self, size: (u16, u16)) {
        if self.stopped || self.viewport == Some(size) {
            return;
        }
        if super::frame::validate_dimensions(size.0, size.1).is_err() {
            self.stop("invalid viewport");
            return;
        }
        let Some(revision) = self.revision.checked_add(1) else {
            self.stop("viewport revision exhausted");
            return;
        };
        self.viewport = Some(size);
        self.revision = revision;
    }
    pub(super) fn stop(&mut self, reason: &str) {
        if !self.stopped {
            eprintln!("jwm-remote: follow stopped: {reason}");
        }
        self.stopped = true;
        // Keep pending identity to validate a late response; never re-arm.
    }
    pub(super) fn presented(
        &mut self,
        scope: (u64, u64),
        size: (u16, u16),
        revision: u64,
        now: Instant,
    ) {
        self.expire(now);
        if self.stopped {
            return;
        }
        if self.presented.is_some_and(|p| p.scope.0 != scope.0) {
            self.stop("source changed");
            return;
        }
        self.presented = Some(Presented {
            scope,
            size,
            revision,
        });
        self.confirm(now);
    }
    fn confirm(&mut self, now: Instant) {
        let Some(pending) = self.pending.as_ref() else {
            return;
        };
        let (Some(result), Some(presented)) = (pending.applied, self.presented) else {
            return;
        };
        if presented.scope == (result.source_epoch, result.observed_geometry_epoch)
            && presented.size == (result.width, result.height)
        {
            self.served = pending.revision;
            self.last_request = Some(now);
            eprintln!(
                "jwm-remote: follow confirmed Applied {}x{} geometry {} request {}",
                result.width, result.height, result.observed_geometry_epoch, result.request_id
            );
            self.pending = None;
        }
    }
    pub(super) fn receive(&mut self, result: ResizeResult, now: Instant) -> RemoteResult<()> {
        result.validate()?;
        self.expire(now);
        let pending = self
            .pending
            .as_mut()
            .ok_or("unsolicited follow resize result")?;
        let request = pending.request;
        if (
            result.request_id,
            result.source_epoch,
            result.geometry_epoch,
        ) != (
            request.request_id,
            request.source_epoch,
            request.geometry_epoch,
        ) || pending.applied.is_some()
        {
            return Err("follow resize result scope mismatch or duplicate".into());
        }
        if self.stopped {
            self.pending = None;
            return Ok(());
        }
        if result.status != ResizeStatus::Applied {
            self.stop(&format!("resize {:?}", result.status));
            self.pending = None;
            return Ok(());
        }
        pending.applied = Some(result);
        self.confirm(now);
        Ok(())
    }
    fn expire(&mut self, now: Instant) {
        if self
            .pending
            .as_ref()
            .is_some_and(|p| now.saturating_duration_since(p.started) >= TIMEOUT)
        {
            self.stop("timeout");
        }
    }
    pub(super) fn next(&mut self, now: Instant) -> Option<ResizeRequest> {
        self.expire(now);
        if self.stopped {
            return None;
        }
        if self.pending.is_some() || self.served == self.revision {
            return None;
        }
        let presented = self.presented?;
        if presented.revision != self.revision {
            return None;
        }
        let size = self.viewport?;
        if size == presented.size {
            self.served = self.revision;
            return None;
        }
        if self
            .last_request
            .is_some_and(|last| now.saturating_duration_since(last) < INTERVAL)
        {
            return None;
        }
        let Some(id) = self.next_id.checked_add(1) else {
            self.stop("request identity exhausted");
            return None;
        };
        let request = ResizeRequest {
            request_id: id,
            source_epoch: presented.scope.0,
            geometry_epoch: presented.scope.1,
            width: size.0,
            height: size.1,
        };
        if request.validate().is_err() {
            self.stop("invalid presented scope");
            return None;
        }
        self.next_id = id;
        self.last_request = Some(now);
        self.pending = Some(Pending {
            request,
            revision: self.revision,
            started: now,
            applied: None,
        });
        eprintln!(
            "jwm-remote: follow request {} {}x{} geometry {}",
            id, size.0, size.1, presented.scope.1
        );
        Some(request)
    }
    pub(super) fn deadline(&self) -> Option<Instant> {
        if self.stopped {
            return None;
        }
        if let Some(p) = &self.pending {
            return Some(p.started + TIMEOUT);
        }
        if self.served != self.revision
            && self.presented.is_some_and(|p| p.revision == self.revision)
        {
            return self.last_request.map(|last| last + INTERVAL);
        }
        None
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn ready() -> (FollowSourceSize, Instant) {
        let now = Instant::now();
        let mut s = FollowSourceSize::default();
        s.viewport((800, 600));
        s.presented((1, 1), (640, 480), s.revision(), now);
        (s, now)
    }
    fn applied(r: ResizeRequest) -> ResizeResult {
        ResizeResult {
            request_id: r.request_id,
            source_epoch: r.source_epoch,
            geometry_epoch: r.geometry_epoch,
            observed_geometry_epoch: r.geometry_epoch + 1,
            width: r.width,
            height: r.height,
            status: ResizeStatus::Applied,
        }
    }
    #[test]
    fn latest_only_waits_for_matching_presented_geometry() {
        let (mut s, n) = ready();
        let r = s.next(n).unwrap();
        s.viewport((900, 700));
        s.viewport((1000, 800));
        s.receive(applied(r), n).unwrap();
        assert!(s.next(n + INTERVAL).is_none());
        s.presented((1, 2), (800, 600), s.revision(), n);
        let r2 = s.next(n + INTERVAL).unwrap();
        assert_eq!((r2.width, r2.height, r2.request_id), (1000, 800, 2));
    }
    #[test]
    fn old_viewport_presentation_and_rate_limit_cannot_issue() {
        let (mut s, n) = ready();
        let old = s.revision();
        s.viewport((900, 700));
        s.presented((1, 1), (640, 480), old, n);
        assert!(s.next(n).is_none());
        s.presented((1, 1), (640, 480), s.revision(), n);
        let r = s.next(n).unwrap();
        s.receive(applied(r), n).unwrap();
        s.presented((1, 2), (900, 700), s.revision(), n);
        s.viewport((1000, 800));
        s.presented((1, 2), (900, 700), s.revision(), n);
        assert!(s.next(n + INTERVAL - Duration::from_nanos(1)).is_none());
        assert!(s.next(n + INTERVAL).is_some());
    }
    #[test]
    fn hinted_size_is_served_not_retried() {
        let (mut s, n) = ready();
        let r = s.next(n).unwrap();
        let mut a = applied(r);
        a.width = 790;
        s.presented((1, 2), (790, 600), s.revision(), n);
        s.receive(a, n).unwrap();
        assert!(s.next(n + INTERVAL).is_none());
    }
    #[test]
    fn terminal_results_and_timeout_never_retry() {
        for status in [
            ResizeStatus::Unchanged,
            ResizeStatus::Denied,
            ResizeStatus::Busy,
            ResizeStatus::Stale,
            ResizeStatus::Failed,
        ] {
            let (mut s, n) = ready();
            let r = s.next(n).unwrap();
            let mut a = applied(r);
            a.status = status;
            s.receive(a, n).unwrap();
            s.viewport((1000, 800));
            s.presented((1, 2), (800, 600), s.revision(), n);
            assert!(s.next(n + TIMEOUT).is_none());
        }
        let (mut s, n) = ready();
        let r = s.next(n).unwrap();
        assert!(s.next(n + TIMEOUT).is_none());
        s.receive(applied(r), n).unwrap();
        assert!(s.next(n + TIMEOUT + INTERVAL).is_none());
    }
    #[test]
    fn source_change_and_bad_result_fail_closed() {
        let (mut s, n) = ready();
        let r = s.next(n).unwrap();
        let mut a = applied(r);
        a.request_id += 1;
        assert!(s.receive(a, n).is_err());
        s.presented((2, 1), (640, 480), s.revision(), n);
        assert!(s.next(n + TIMEOUT).is_none());
    }
    #[test]
    fn applied_requires_actual_source_size_and_epoch_and_keeps_timeout() {
        let (mut s, n) = ready();
        let r = s.next(n).unwrap();
        s.receive(applied(r), n).unwrap();
        s.presented((1, 2), (400, 300), s.revision(), n); // encoded dimensions are not source dimensions
        assert!(s.pending.is_some());
        s.presented((1, 1), (800, 600), s.revision(), n);
        assert!(s.pending.is_some());
        s.presented((1, 2), (800, 600), s.revision(), n + TIMEOUT);
        assert!(s.stopped);
        assert!(s.next(n + TIMEOUT + INTERVAL).is_none());
    }
    #[test]
    fn confirmation_not_send_starts_cooldown() {
        let (mut s, n) = ready();
        let r = s.next(n).unwrap();
        s.presented((1, 2), (800, 600), s.revision(), n + Duration::from_secs(1));
        let confirmed = n + Duration::from_secs(2);
        s.receive(applied(r), confirmed).unwrap();
        s.viewport((900, 700));
        s.presented((1, 2), (800, 600), s.revision(), confirmed);
        assert!(
            s.next(confirmed + INTERVAL - Duration::from_nanos(1))
                .is_none()
        );
        assert!(s.next(confirmed + INTERVAL).is_some());
    }
    #[test]
    fn initial_same_size_and_coalesced_viewport_do_not_retry() {
        let (mut s, n) = ready();
        s.presented((1, 1), (800, 600), s.revision(), n);
        assert!(s.next(n).is_none());
        assert_eq!(s.served, s.revision());
        let revision = s.revision();
        s.viewport((800, 600));
        assert_eq!(s.revision(), revision);
        s.viewport((900, 700));
        s.viewport((800, 600));
        assert!(s.revision() > revision);
        assert!(s.next(n).is_none()); // requires presentation for new revision
    }
    #[test]
    fn late_result_cannot_clear_expired_confirmation_barrier() {
        let (mut s, n) = ready();
        let r = s.next(n).unwrap();
        s.presented((1, 2), (800, 600), s.revision(), n);
        s.receive(applied(r), n + TIMEOUT).unwrap();
        assert!(s.stopped);
        assert!(s.next(n + TIMEOUT + INTERVAL).is_none());
    }
}
