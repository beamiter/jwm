//! Explicit native popup pointer client. No queued or replayed input.
use super::*;
use crate::remote::wayland_popup_wire::{Binding, Click, Metadata, Status, StatusKind};
const FRESH: Duration = Duration::from_secs(1);
const LEASE: Duration = Duration::from_secs(30);
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PopupLifetimeFailureReason {
    Ended,
    LeaseExpired,
    ActionTimeout,
}
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct PopupLifetimeFailure {
    pub at: Instant,
    pub ended: bool,
    pub deadline: Option<Instant>,
    pub pending_deadline: Option<Instant>,
    pub reason: PopupLifetimeFailureReason,
}
#[cfg(test)]
#[derive(Default, Debug)]
pub(crate) struct PopupLifetimeObservation {
    pub requested_at: Option<Instant>,
    pub deadline: Option<Instant>,
    pub ttl_fixed: bool,
    pub first_failure: Option<PopupLifetimeFailure>,
    pub last_rejected: Option<(super::super::wayland_popup_wire::FrameKey, u32)>,
}
#[cfg(test)]
type LifetimeObserver = Arc<Mutex<PopupLifetimeObservation>>;
#[cfg(test)]
thread_local! {
    static LIFETIME_OBSERVER: std::cell::RefCell<Option<LifetimeObserver>> = const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
pub(crate) fn with_popup_lifetime_observer<T>(
    observer: LifetimeObserver,
    run: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<LifetimeObserver>);
    impl Drop for Restore {
        fn drop(&mut self) {
            LIFETIME_OBSERVER.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(LIFETIME_OBSERVER.with(|slot| slot.replace(Some(observer))));
    run()
}
#[derive(Default)]
pub(super) struct ClientPopup {
    pub(super) enabled: bool,
    #[cfg(test)]
    lifetime_observer: Option<LifetimeObserver>,
    binding: Option<Binding>,
    requested_at: Option<Instant>,
    armed: bool,
    ttl_fixed: bool,
    ready_seen: Option<super::super::wayland_popup_wire::FrameKey>,
    first_sequence: Option<u64>,
    deadline: Option<Instant>,
    decoded: Option<(Metadata, Instant)>,
    presented: Option<Metadata>,
    ready: Option<Metadata>,
    candidate: Option<(Metadata, u16, u16, i16, i16, Instant)>,
    left_down: bool,
    sequence: u32,
    pending: Option<(Click, Instant)>,
    pending_metadata: Option<Metadata>,
    rejection_floor: Option<u64>,
    ended: bool,
}
impl ClientPopup {
    pub(super) fn enable(&mut self, requested_at: Instant) {
        self.enabled = true;
        self.requested_at = Some(requested_at);
        self.deadline = requested_at.checked_add(LEASE);
        #[cfg(test)]
        {
            self.lifetime_observer = LIFETIME_OBSERVER.with(|slot| slot.borrow().clone());
            self.observe_lifetime();
        }
    }
    pub(super) fn check(&self, now: Instant) -> RemoteResult<()> {
        if self.enabled
            && (self.ended
                || self.deadline.is_some_and(|d| now >= d)
                || self.pending.is_some_and(|(_, at)| {
                    now.saturating_duration_since(at) >= Duration::from_secs(5)
                }))
        {
            #[cfg(test)]
            if let Some(observer) = &self.lifetime_observer {
                let mut observation = observer.lock().unwrap();
                if observation.first_failure.is_none() {
                    observation.first_failure = Some(PopupLifetimeFailure {
                        at: now,
                        ended: self.ended,
                        deadline: self.deadline,
                        pending_deadline: self
                            .pending
                            .and_then(|(_, at)| at.checked_add(Duration::from_secs(5))),
                        reason: if self.ended {
                            PopupLifetimeFailureReason::Ended
                        } else if self.deadline.is_some_and(|d| now >= d) {
                            PopupLifetimeFailureReason::LeaseExpired
                        } else {
                            PopupLifetimeFailureReason::ActionTimeout
                        },
                    });
                }
            }
            return Err(invalid_data("popup pointer session ended or expired").into());
        }
        Ok(())
    }
    #[cfg(test)]
    fn observe_lifetime(&self) {
        if let Some(observer) = &self.lifetime_observer {
            let mut observation = observer.lock().unwrap();
            observation.requested_at = self.requested_at;
            observation.deadline = self.deadline;
            observation.ttl_fixed = self.ttl_fixed;
        }
    }
    pub(super) fn decoded(
        &mut self,
        meta: Metadata,
        epoch: u64,
        frame: &RecyclableDecodedFrame,
        now: Instant,
    ) -> RemoteResult<()> {
        self.check(now)?;
        if !self.enabled
            || meta.key.binding.source_epoch != epoch
            || meta.key.geometry_epoch != frame.geometry_epoch()
            || meta.key.wire_sequence != frame.sequence()
            || meta.canvas
                != (
                    u32::from(frame.source_width()),
                    u32::from(frame.source_height()),
                )
            || frame.image().dimensions() != meta.canvas
        {
            return Err(invalid_data("popup frame metadata does not match decoded canvas").into());
        }
        if let Some((previous, _)) = self.decoded {
            if previous.key.binding != meta.key.binding
                || previous.canvas != meta.canvas
                || meta.key.wire_sequence <= previous.key.wire_sequence
                || meta.key.proof_id <= previous.key.proof_id
                || meta.key.native_ready_serial <= previous.key.native_ready_serial
                || meta.key.family_generation < previous.key.family_generation
                || (meta.key.family_generation == previous.key.family_generation
                    && meta.popup != previous.popup)
            {
                return Err(invalid_data("popup frame changed identity or replayed proof").into());
            }
        } else {
            self.binding = Some(meta.key.binding);
            self.first_sequence = Some(meta.key.wire_sequence);
        }
        self.decoded = Some((meta, now));
        self.ready = None;
        self.candidate = None;
        Ok(())
    }
    pub(super) fn presented(&mut self, sequence: u64, now: Instant) -> RemoteResult<()> {
        if !self.enabled {
            return Ok(());
        }
        self.check(now)?;
        let (meta, _) = self
            .decoded
            .ok_or_else(|| invalid_data("popup presentation without metadata"))?;
        if meta.key.wire_sequence != sequence || self.presented == Some(meta) {
            return Err(invalid_data("popup presentation is stale or duplicated").into());
        }
        self.presented = Some(meta);
        Ok(())
    }
    pub(super) fn status(&mut self, value: Status, now: Instant) -> RemoteResult<()> {
        self.check(now)?;
        if !self.enabled || self.binding != Some(value.metadata.key.binding) {
            return Err(invalid_data("unsolicited popup pointer status").into());
        }
        match value.kind {
            StatusKind::Revoked | StatusKind::Denied => {
                self.ended = true;
                self.ready = None;
                self.candidate = None;
                return Err(invalid_data("host revoked popup pointer authority").into());
            }
            StatusKind::Armed => {
                if self.armed
                    || self.pending.is_some()
                    || value.action_sequence != 0
                    || self.presented != Some(value.metadata)
                    || self.first_sequence != Some(value.metadata.key.wire_sequence)
                {
                    return Err(invalid_data("popup Armed lacks exact initial presentation").into());
                }
                self.armed = true;
                self.ready = None;
                self.candidate = None;
            }
            StatusKind::Accepted => {
                let (pending, _) = self
                    .pending
                    .ok_or_else(|| invalid_data("unsolicited popup completion"))?;
                if value.action_sequence != pending.sequence || value.metadata.key != pending.key {
                    return Err(invalid_data("popup completion proof mismatch").into());
                }
                self.pending = None;
                self.pending_metadata = None;
                self.ready = None;
                self.candidate = None;
            }
            StatusKind::RejectedStale => {
                let (pending, _) = self
                    .pending
                    .ok_or_else(|| invalid_data("unsolicited popup stale rejection"))?;
                if value.action_sequence != pending.sequence
                    || value.metadata.key != pending.key
                    || self.pending_metadata != Some(value.metadata)
                {
                    return Err(invalid_data("popup stale rejection proof mismatch").into());
                }
                #[cfg(test)]
                if let Some(observer) = &self.lifetime_observer {
                    observer.lock().unwrap().last_rejected = Some((pending.key, pending.sequence));
                }
                self.rejection_floor = Some(
                    self.decoded
                        .ok_or_else(|| invalid_data("popup stale rejection without decoded proof"))?
                        .0
                        .key
                        .wire_sequence,
                );
                self.pending = None;
                self.pending_metadata = None;
                self.ready = None;
                self.candidate = None;
            }
            StatusKind::Waiting => {
                let previous_completed = self.pending.is_some_and(|(pending, _)| {
                    pending.sequence == self.sequence
                        && value.action_sequence.checked_add(1) == Some(pending.sequence)
                });
                if (value.action_sequence != self.sequence && !previous_completed)
                    || self.decoded.map(|p| p.0) != Some(value.metadata)
                {
                    return Err(invalid_data("popup Waiting lacks an issued frame").into());
                }
                self.ready = None;
                self.candidate = None;
            }
            StatusKind::Ready => {
                if !self.armed
                    || self
                        .rejection_floor
                        .is_some_and(|floor| value.metadata.key.wire_sequence <= floor)
                    || self.pending.is_some()
                    || self.presented != Some(value.metadata)
                    || self.decoded.map(|p| p.0) != Some(value.metadata)
                    || value.action_sequence != self.sequence
                    || self.ready.is_some()
                    || self.ready_seen == Some(value.metadata.key)
                    || self
                        .first_sequence
                        .is_none_or(|first| value.metadata.key.wire_sequence <= first)
                {
                    return Err(
                        invalid_data("popup Ready lacks exact post-arm presentation").into(),
                    );
                }
                self.rejection_floor = None;
                self.ready_seen = Some(value.metadata.key);
                if self.fresh_frame(now) {
                    self.ready = Some(value.metadata);
                }
            }
        }
        if value.ttl_ms == 0 || value.ttl_ms > 30_000 {
            return Err(invalid_data("invalid popup lease duration").into());
        }
        if !self.ttl_fixed {
            if let Some(remaining) = self
                .requested_at
                .and_then(|at| at.checked_add(Duration::from_millis(u64::from(value.ttl_ms))))
            {
                self.deadline = self.deadline.map(|original| original.min(remaining));
            }
            self.ttl_fixed = true;
            #[cfg(test)]
            self.observe_lifetime();
        }
        Ok(())
    }
    fn fresh_frame(&self, now: Instant) -> bool {
        self.decoded
            .is_some_and(|(_, at)| now.saturating_duration_since(at) < FRESH)
    }
    pub(super) fn ready_sequence(&self, now: Instant) -> Option<u64> {
        (!self.ended && self.check(now).is_ok() && self.fresh_frame(now))
            .then(|| self.ready.map(|m| m.key.wire_sequence))
            .flatten()
    }
    pub(super) fn deadline(&self) -> Option<Instant> {
        if !self.enabled || self.ended {
            return None;
        }
        match (
            self.deadline,
            self.pending
                .and_then(|(_, at)| at.checked_add(Duration::from_secs(5))),
        ) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
    pub(super) fn revoke(&mut self) -> Option<Binding> {
        if !self.enabled || self.ended {
            return None;
        }
        self.ended = true;
        self.ready = None;
        self.candidate = None;
        self.binding
    }
    pub(super) fn event(
        &mut self,
        event: &ViewerEvent,
        now: Instant,
    ) -> RemoteResult<Option<Click>> {
        self.check(now)?;
        use super::super::x11_viewer::WaylandPointerEventKind as E;
        match event {
            ViewerEvent::WaylandPointerButtonCancel { pressed } => {
                self.left_down = *pressed;
                self.candidate = None;
                return Ok(None);
            }
            ViewerEvent::WaylandPointerCancel => {
                self.candidate = None;
                return Ok(None);
            }
            _ => {}
        }
        let ViewerEvent::WaylandPointer {
            frame_sequence,
            geometry_epoch,
            x,
            y,
            local_x,
            local_y,
            kind,
        } = *event
        else {
            return Ok(None);
        };
        if matches!(
            kind,
            E::ScrollUp | E::ScrollDown | E::ScrollLeft | E::ScrollRight
        ) {
            return Err(invalid_data("popup pointer does not permit scrolling").into());
        }
        if kind == E::LeftDown {
            self.candidate = None;
            if self.left_down {
                return Err(invalid_data("duplicate popup button press").into());
            }
            self.left_down = true;
        }
        if kind == E::LeftUp {
            self.left_down = false;
        }
        let valid = self.ready.filter(|m| {
            self.pending.is_none()
                && self.ready_sequence(now) == Some(frame_sequence)
                && m.key.geometry_epoch == geometry_epoch
                && u32::from(x) < m.canvas.0
                && u32::from(y) < m.canvas.1
                && m.popup.is_none_or(|r| {
                    u32::from(x) >= r.x
                        && u32::from(y) >= r.y
                        && u32::from(x) - r.x < r.width
                        && u32::from(y) - r.y < r.height
                })
        });
        let Some(meta) = valid else {
            self.candidate = None;
            return Ok(None);
        };
        match kind {
            E::LeftDown => self.candidate = Some((meta, x, y, local_x, local_y, now)),
            E::Motion => {
                if self.candidate.is_some_and(|(_, _, _, lx, ly, _)| {
                    (i32::from(lx) - i32::from(local_x)).abs() > 4
                        || (i32::from(ly) - i32::from(local_y)).abs() > 4
                }) {
                    self.candidate = None;
                }
            }
            E::LeftUp => {
                let Some((down, dx, dy, lx, ly, at)) = self.candidate.take() else {
                    return Ok(None);
                };
                if down != meta
                    || now.saturating_duration_since(at) >= Duration::from_millis(250)
                    || (i32::from(lx) - i32::from(local_x)).abs() > 4
                    || (i32::from(ly) - i32::from(local_y)).abs() > 4
                {
                    return Ok(None);
                }
                self.sequence = self
                    .sequence
                    .checked_add(1)
                    .filter(|s| *s <= 60_000)
                    .ok_or_else(|| invalid_data("popup action sequence exhausted"))?;
                let click = Click {
                    key: meta.key,
                    sequence: self.sequence,
                    x: u32::from(dx),
                    y: u32::from(dy),
                };
                self.pending = Some((click, now));
                self.pending_metadata = Some(meta);
                self.ready = None;
                return Ok(Some(click));
            }
            _ => unreachable!(),
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::{frame::new_decode_buffer_pool, wayland_popup_wire::FrameKey};
    fn metadata(seq: u64) -> Metadata {
        Metadata {
            key: FrameKey {
                binding: Binding {
                    source_instance: 1,
                    source_epoch: 1,
                    lease_epoch: 1,
                },
                geometry_epoch: 1,
                wire_sequence: seq,
                proof_id: seq + 1,
                native_frame_id: seq + 1,
                native_ready_serial: seq + 1,
                family_generation: 1,
            },
            canvas: (32, 24),
            popup: None,
        }
    }
    fn decode(p: &mut ClientPopup, seq: u64, now: Instant) {
        let frame = RecyclableDecodedFrame::new(
            seq,
            32,
            24,
            image::RgbImage::new(32, 24),
            new_decode_buffer_pool(),
        )
        .with_geometry_epoch(1);
        p.decoded(metadata(seq), 1, &frame, now).unwrap();
    }
    fn status(kind: StatusKind, seq: u64, action: u32) -> Status {
        Status {
            kind,
            metadata: metadata(seq),
            action_sequence: action,
            ttl_ms: 30_000,
        }
    }
    fn ready(now: Instant) -> ClientPopup {
        let mut p = ClientPopup::default();
        p.enable(now);
        decode(&mut p, 0, now);
        p.presented(0, now).unwrap();
        p.status(status(StatusKind::Armed, 0, 0), now).unwrap();
        decode(&mut p, 1, now);
        p.presented(1, now).unwrap();
        p.status(status(StatusKind::Ready, 1, 0), now).unwrap();
        p
    }
    fn edge(
        seq: u64,
        kind: super::super::super::x11_viewer::WaylandPointerEventKind,
    ) -> ViewerEvent {
        ViewerEvent::WaylandPointer {
            frame_sequence: seq,
            geometry_epoch: 1,
            x: 7,
            y: 8,
            local_x: 7,
            local_y: 8,
            kind,
        }
    }
    #[test]
    fn popup_lifetime_observer_records_actual_first_failure_without_changing_deadline() {
        let observer = Arc::new(Mutex::new(PopupLifetimeObservation::default()));
        let now = Instant::now();
        let mut p = with_popup_lifetime_observer(Arc::clone(&observer), || ready(now));
        let deadline = p.deadline.unwrap();
        {
            let observed = observer.lock().unwrap();
            assert_eq!(observed.requested_at, Some(now));
            assert_eq!(observed.deadline, Some(deadline));
            assert!(observed.ttl_fixed);
            assert!(observed.first_failure.is_none());
        }
        assert!(p.check(deadline).is_err());
        p.revoke();
        assert!(p.check(deadline + Duration::from_secs(1)).is_err());
        let failure = observer.lock().unwrap().first_failure.unwrap();
        assert_eq!(failure.at, deadline);
        assert_eq!(failure.deadline, Some(deadline));
        assert!(!failure.ended);
        assert_eq!(failure.pending_deadline, None);
        assert_eq!(failure.reason, PopupLifetimeFailureReason::LeaseExpired);
        assert_eq!(p.deadline, Some(deadline));
    }
    #[test]
    fn popup_requires_exact_initial_and_post_arm_presentation() {
        let now = Instant::now();
        let mut p = ClientPopup::default();
        p.enable(now);
        decode(&mut p, 0, now);
        assert!(p.status(status(StatusKind::Armed, 0, 0), now).is_err());
        p.presented(0, now).unwrap();
        p.status(status(StatusKind::Armed, 0, 0), now).unwrap();
        assert!(p.status(status(StatusKind::Ready, 0, 0), now).is_err());
        decode(&mut p, 1, now);
        assert!(p.status(status(StatusKind::Ready, 1, 0), now).is_err());
        p.presented(1, now).unwrap();
        p.status(status(StatusKind::Ready, 1, 0), now).unwrap();
        assert_eq!(p.ready_sequence(now), Some(1));
    }
    #[test]
    fn popup_click_is_atomic_one_shot_and_waiting_is_not_completion() {
        use crate::remote::x11_viewer::WaylandPointerEventKind as E;
        let now = Instant::now();
        let mut p = ready(now);
        assert!(p.event(&edge(1, E::LeftUp), now).unwrap().is_none());
        p.event(&edge(1, E::LeftDown), now).unwrap();
        let click = p.event(&edge(1, E::LeftUp), now).unwrap().unwrap();
        assert_eq!(click.sequence, 1);
        assert!(p.ready_sequence(now).is_none());
        p.status(status(StatusKind::Waiting, 1, 1), now).unwrap();
        assert!(p.pending.is_some());
        assert!(p.status(status(StatusKind::Ready, 1, 1), now).is_err());
        p.status(status(StatusKind::Accepted, 1, 1), now).unwrap();
        assert!(p.status(status(StatusKind::Accepted, 1, 1), now).is_err());
        assert!(p.status(status(StatusKind::Ready, 1, 1), now).is_err());
    }
    #[test]
    fn popup_refresh_waiting_crossing_click_preserves_completion_fence() {
        use crate::remote::x11_viewer::WaylandPointerEventKind as E;
        let now = Instant::now();
        let mut p = ready(now);
        p.event(&edge(1, E::LeftDown), now).unwrap();
        let click = p.event(&edge(1, E::LeftUp), now).unwrap().unwrap();
        // This older Waiting may already be in flight when the local click
        // is sent. It is not evidence that the click completed or was denied.
        p.status(status(StatusKind::Waiting, 1, 0), now).unwrap();
        assert_eq!(p.pending.as_ref().unwrap().0, click);
        assert!(p.ready_sequence(now).is_none());
        assert!(p.event(&edge(1, E::LeftDown), now).unwrap().is_none());
        assert!(p.event(&edge(1, E::LeftUp), now).unwrap().is_none());
        assert_eq!(p.sequence, 1);
    }
    #[test]
    fn popup_stale_rejection_needs_post_receipt_frame_and_new_click() {
        use crate::remote::x11_viewer::WaylandPointerEventKind as E;
        let now = Instant::now();
        let observer = Arc::new(Mutex::new(PopupLifetimeObservation::default()));
        let mut p = with_popup_lifetime_observer(Arc::clone(&observer), || ready(now));
        let original_deadline = p.deadline;
        p.event(&edge(1, E::LeftDown), now).unwrap();
        p.event(&edge(1, E::LeftUp), now).unwrap().unwrap();
        decode(&mut p, 2, now);
        p.presented(2, now).unwrap();
        p.status(status(StatusKind::Waiting, 2, 0), now).unwrap();
        p.event(&edge(2, E::LeftDown), now).unwrap();
        p.status(status(StatusKind::RejectedStale, 1, 1), now)
            .unwrap();
        assert_eq!(
            observer.lock().unwrap().last_rejected,
            Some((status(StatusKind::RejectedStale, 1, 1).metadata.key, 1))
        );
        assert!(p.status(status(StatusKind::Ready, 2, 1), now).is_err());
        decode(&mut p, 3, now);
        assert!(p.status(status(StatusKind::Ready, 3, 1), now).is_err());
        p.presented(3, now).unwrap();
        p.status(status(StatusKind::Ready, 3, 1), now).unwrap();
        assert!(p.event(&edge(3, E::LeftUp), now).unwrap().is_none());
        p.event(&edge(3, E::LeftDown), now).unwrap();
        assert_eq!(
            p.event(&edge(3, E::LeftUp), now).unwrap().unwrap().sequence,
            2
        );
        assert_eq!(p.deadline, original_deadline);
    }
    #[test]
    fn popup_stale_rejection_rejects_unknown_duplicate_and_altered_receipts() {
        use crate::remote::x11_viewer::WaylandPointerEventKind as E;
        let now = Instant::now();
        let mut p = ready(now);
        let receipt = status(StatusKind::RejectedStale, 1, 1);
        assert!(p.status(receipt, now).is_err());
        p.event(&edge(1, E::LeftDown), now).unwrap();
        p.event(&edge(1, E::LeftUp), now).unwrap().unwrap();
        let mut wrong = receipt;
        wrong.action_sequence = 2;
        assert!(p.status(wrong, now).is_err());
        wrong = receipt;
        wrong.metadata.canvas.0 += 1;
        assert!(p.status(wrong, now).is_err());
        wrong = receipt;
        wrong.metadata.key.binding.lease_epoch += 1;
        assert!(p.status(wrong, now).is_err());
        assert!(p.pending.is_some());
        p.status(receipt, now).unwrap();
        assert!(p.status(receipt, now).is_err());
        assert!(p.status(status(StatusKind::Accepted, 1, 1), now).is_err());
        assert!(p.ready_sequence(now).is_none());
    }
    #[test]
    fn popup_new_frame_cancels_held_candidate_without_replay() {
        use crate::remote::x11_viewer::WaylandPointerEventKind as E;
        let now = Instant::now();
        let mut p = ready(now);
        p.event(&edge(1, E::LeftDown), now).unwrap();
        decode(&mut p, 2, now);
        p.presented(2, now).unwrap();
        p.status(status(StatusKind::Ready, 2, 0), now).unwrap();
        assert!(p.event(&edge(2, E::LeftUp), now).unwrap().is_none());
        assert_eq!(p.sequence, 0);
        assert!(p.event(&edge(2, E::ScrollDown), now).is_err());
    }
    #[test]
    fn popup_deadline_does_not_extend_or_restart_and_stale_frame_cannot_click() {
        use crate::remote::x11_viewer::WaylandPointerEventKind as E;
        let now = Instant::now();
        let mut p = ready(now);
        let original = p.deadline;
        let late = now + Duration::from_secs(2);
        assert!(p.event(&edge(1, E::LeftDown), late).unwrap().is_none());
        decode(&mut p, 2, late);
        p.presented(2, late).unwrap();
        p.status(status(StatusKind::Ready, 2, 0), late).unwrap();
        assert_eq!(p.deadline, original);
        assert!(p.check(now + LEASE).is_err());
        assert!(p.revoke().is_some());
        assert!(p.check(now).is_err());
    }
}
