//! Staged popup network authority model, not an enabled protocol capability.
//!
//! The host owns every identity below. Numeric IDs must map to retained native
//! resources, never peer-selected surfaces. Only a successful write AND its exact
//! presentation ACK permit an operation. A cumulative credit retirement is not
//! presentation. The future integration must still perform native current-proof
//! validation and use real native completion events; this model cannot attest
//! display hardware or replace native ownership/generation checks. Native v2
//! additionally enforces its 1024-export quota; this ledger does not replace it.
//! Timeout after dispatch leaves execution uncertain: terminate, never retry.
#[cfg(any(test, feature = "remote-wayland"))]
use std::sync::Arc;
#[cfg(any(test, feature = "remote-wayland"))]
use std::time::{Duration, Instant};
#[cfg(any(test, feature = "remote-wayland"))]
const TTL: Duration = Duration::from_secs(30);
#[cfg(any(test, feature = "remote-wayland"))]
const FRESH: Duration = Duration::from_secs(1);
#[cfg(any(test, feature = "remote-wayland"))]
const OPERATION_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_ACTIONS: u32 = 60_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Binding {
    pub(crate) source_instance: u64,
    pub(crate) source_epoch: u64,
    pub(crate) lease_epoch: u32,
}
impl Binding {
    fn valid(self) -> bool {
        self.source_instance != 0 && self.source_epoch != 0 && self.lease_epoch != 0
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Region {
    pub(crate) x: u32,
    pub(crate) y: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
}
impl Region {
    fn valid(self, canvas: (u32, u32)) -> bool {
        self.width != 0
            && self.height != 0
            && self
                .x
                .checked_add(self.width)
                .is_some_and(|x| x <= canvas.0)
            && self
                .y
                .checked_add(self.height)
                .is_some_and(|y| y <= canvas.1)
    }
    #[cfg(any(test, feature = "remote-wayland"))]
    pub(crate) fn contains(self, x: u32, y: u32) -> bool {
        x >= self.x && y >= self.y && x - self.x < self.width && y - self.y < self.height
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FrameKey {
    pub(crate) binding: Binding,
    pub(crate) geometry_epoch: u64,
    pub(crate) wire_sequence: u64,
    pub(crate) proof_id: u64,
    /// Host-local identity of the retained resource, not an application surface.
    pub(crate) native_frame_id: u64,
    pub(crate) native_ready_serial: u64,
    pub(crate) family_generation: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Metadata {
    pub(crate) key: FrameKey,
    pub(crate) canvas: (u32, u32),
    pub(crate) popup: Option<Region>,
}
impl Metadata {
    pub(crate) fn valid(self) -> bool {
        self.key.binding.valid()
            && self.key.geometry_epoch != 0
            && self.key.proof_id != 0
            && self.key.native_frame_id != 0
            && self.key.native_ready_serial != 0
            && self.key.family_generation != 0
            && self.canvas.0 > 0
            && self.canvas.1 > 0
            && self.canvas.0 <= 16384
            && self.canvas.1 <= 16384
            && u64::from(self.canvas.0) * u64::from(self.canvas.1) <= 64 * 1024 * 1024
            && self.popup.is_none_or(|p| p.valid(self.canvas))
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    #[cfg(any(test, feature = "remote-wayland"))]
    Ended,
    #[cfg(any(test, feature = "remote-wayland"))]
    Deadline,
    InvalidMetadata,
    #[cfg(any(test, feature = "remote-wayland"))]
    Identity,
    Ordering,
    #[cfg(any(test, feature = "remote-wayland"))]
    Duplicate,
    #[cfg(any(test, feature = "remote-wayland"))]
    Stale,
    #[cfg(any(test, feature = "remote-wayland"))]
    Bounds,
    #[cfg(any(test, feature = "remote-wayland"))]
    Overflow,
    #[cfg(any(test, feature = "remote-wayland"))]
    WriteFailed,
    #[cfg(test)]
    Retired,
}
type Result<T> = std::result::Result<T, Failure>;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(any(test, feature = "remote-wayland"))]
pub(crate) enum Opportunity {
    Arm(FrameKey),
    Click(FrameKey),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Click {
    pub(crate) key: FrameKey,
    pub(crate) sequence: u32,
    pub(crate) x: u32,
    pub(crate) y: u32,
}
#[derive(Clone, Copy)]
#[cfg(any(test, feature = "remote-wayland"))]
struct Slot {
    metadata: Metadata,
    captured_at: Instant,
    committed: bool,
    acked: bool,
}
#[derive(Clone, Copy)]
#[cfg(any(test, feature = "remote-wayland"))]
enum Pending {
    Arm(FrameKey),
    Click(Click),
}
#[cfg(any(test, feature = "remote-wayland"))]
impl Pending {
    fn key(self) -> FrameKey {
        match self {
            Self::Arm(k) => k,
            Self::Click(c) => c.key,
        }
    }
}
/// One retained native opportunity, one pending native operation, no unbounded
/// history or queued clicks. Failure is permanent; constructing a replacement
/// ledger does NOT authorize creating a replacement native lease.
#[cfg(any(test, feature = "remote-wayland"))]
pub(crate) struct Ledger {
    identity: Arc<()>,
    binding: Binding,
    canvas: (u32, u32),
    created: Instant,
    deadline: Instant,
    ended: bool,
    armed: bool,
    slot: Option<Slot>,
    last: Option<Metadata>,
    last_completion: Option<Instant>,
    pending: Option<(Pending, Instant)>,
    sequence: u32,        // processed network commands, including explicit rejection
    native_sequence: u32, // only successfully accepted native clicks
    retired_refresh: Option<Metadata>,
    discarded_ack: Option<FrameKey>,
    capture_after: Option<Instant>,
}
#[cfg(any(test, feature = "remote-wayland"))]
impl Ledger {
    pub(crate) fn new(binding: Binding, canvas: (u32, u32), created: Instant) -> Result<Self> {
        let probe = Metadata {
            key: FrameKey {
                binding,
                geometry_epoch: 1,
                wire_sequence: 0,
                proof_id: 1,
                native_frame_id: 1,
                native_ready_serial: 1,
                family_generation: 1,
            },
            canvas,
            popup: None,
        };
        if !probe.valid() {
            return Err(Failure::InvalidMetadata);
        }
        Ok(Self {
            identity: Arc::new(()),
            binding,
            canvas,
            created,
            deadline: created.checked_add(TTL).ok_or(Failure::Overflow)?,
            ended: false,
            armed: false,
            slot: None,
            last: None,
            last_completion: None,
            pending: None,
            sequence: 0,
            native_sequence: 0,
            retired_refresh: None,
            discarded_ack: None,
            capture_after: None,
        })
    }
    fn run<T>(&mut self, now: Instant, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        let result = if self.ended {
            Err(Failure::Ended)
        } else if now < self.created
            || now >= self.deadline
            || self.pending.is_some_and(|(_, d)| now >= d)
        {
            Err(Failure::Deadline)
        } else {
            f(self)
        };
        if result.is_err() {
            self.revoke();
        }
        result
    }
    pub(crate) fn revoke(&mut self) {
        self.ended = true;
        self.slot = None;
        self.pending = None;
        self.retired_refresh = None;
        self.discarded_ack = None;
    }
    pub(crate) fn reserve(
        &mut self,
        metadata: Metadata,
        captured_at: Instant,
        now: Instant,
    ) -> Result<()> {
        self.run(now, |s| {
            if s.slot.is_some() || s.pending.is_some() {
                return Err(Failure::Ordering);
            }
            if !metadata.valid() {
                return Err(Failure::InvalidMetadata);
            }
            if metadata.key.binding != s.binding || metadata.canvas != s.canvas {
                return Err(Failure::Identity);
            }
            if captured_at < s.created
                || captured_at > now
                || now >= captured_at.checked_add(FRESH).ok_or(Failure::Overflow)?
                || s.last_completion.is_some_and(|t| captured_at <= t)
                || s.capture_after.is_some_and(|t| captured_at <= t)
            {
                return Err(Failure::Stale);
            }
            let expected = s
                .last
                .map_or(Some(1), |m| m.key.proof_id.checked_add(1))
                .ok_or(Failure::Overflow)?;
            if metadata.key.proof_id != expected {
                return Err(Failure::Ordering);
            }
            if let Some(last) = s.last {
                if metadata.key.geometry_epoch != last.key.geometry_epoch
                    || metadata.key.wire_sequence <= last.key.wire_sequence
                    || metadata.key.native_ready_serial <= last.key.native_ready_serial
                    || metadata.key.native_frame_id == last.key.native_frame_id
                    || metadata.key.family_generation < last.key.family_generation
                    || (metadata.key.family_generation == last.key.family_generation
                        && metadata.popup != last.popup)
                {
                    return Err(Failure::Stale);
                }
            }
            s.last = Some(metadata);
            s.slot = Some(Slot {
                metadata,
                captured_at,
                committed: false,
                acked: false,
            });
            Ok(())
        })
    }
    fn slot(&mut self, key: FrameKey, now: Instant) -> Result<&mut Slot> {
        let slot = self.slot.as_mut().ok_or(Failure::Ordering)?;
        if slot.metadata.key != key {
            return Err(Failure::Identity);
        }
        if now
            >= slot
                .captured_at
                .checked_add(FRESH)
                .ok_or(Failure::Overflow)?
        {
            return Err(Failure::Stale);
        }
        Ok(slot)
    }
    /// Invoke only after the corresponding write AND flush actually succeed.
    pub(crate) fn commit_write(&mut self, key: FrameKey, now: Instant) -> Result<()> {
        self.run(now, |s| {
            let slot = s.slot(key, now)?;
            if slot.committed {
                return Err(Failure::Duplicate);
            }
            slot.committed = true;
            Ok(())
        })
    }
    pub(crate) fn write_failed(&mut self) -> Result<()> {
        self.revoke();
        Err(Failure::WriteFailed)
    }
    /// Match the ACK's actual target sequence, never earlier cumulatively
    /// retired frames. The authenticated session supplies this fixed binding.
    pub(crate) fn acknowledge_exact(
        &mut self,
        binding: Binding,
        wire_sequence: u64,
        now: Instant,
    ) -> Result<()> {
        self.run(now, |s| {
            let key = s.slot.as_ref().ok_or(Failure::Ordering)?.metadata.key;
            if binding != s.binding || key.wire_sequence != wire_sequence {
                return Err(Failure::Identity);
            }
            let slot = s.slot(key, now)?;
            if slot.acked {
                return Err(Failure::Duplicate);
            }
            slot.acked = true;
            Ok(())
        })
    }
    /// A superseded proof can free credit, but must not become input authority.
    /// This conservative slice terminates rather than retaining replay history.
    #[cfg(test)]
    pub(crate) fn retire_without_presentation(&mut self) -> Result<()> {
        self.revoke();
        Err(Failure::Retired)
    }
    fn opportunity(&self) -> Option<Opportunity> {
        self.slot.filter(|f| f.committed && f.acked).map(|f| {
            if self.armed {
                Opportunity::Click(f.metadata.key)
            } else {
                Opportunity::Arm(f.metadata.key)
            }
        })
    }
    pub(crate) fn ready(&mut self, now: Instant) -> Result<Option<Opportunity>> {
        self.run(now, |s| {
            if let Some(f) = s.slot {
                s.slot(f.metadata.key, now)?;
            }
            Ok(s.opportunity())
        })
    }
    pub(crate) fn begin_arm(&mut self, key: FrameKey, now: Instant) -> Result<()> {
        self.run(now, |s| {
            s.slot(key, now)?;
            if s.opportunity() != Some(Opportunity::Arm(key))
                || s.slot.unwrap().metadata.popup.is_some()
            {
                return Err(Failure::Ordering);
            }
            let fresh_deadline = s
                .slot
                .unwrap()
                .captured_at
                .checked_add(FRESH)
                .ok_or(Failure::Overflow)?;
            s.slot = None;
            s.pending = Some((
                Pending::Arm(key),
                now.checked_add(OPERATION_TIMEOUT)
                    .ok_or(Failure::Overflow)?
                    .min(s.deadline)
                    .min(fresh_deadline),
            ));
            Ok(())
        })
    }
    /// Called only for the actual native Armed event, never merely an ACK.
    pub(crate) fn complete_arm(&mut self, binding: Binding, now: Instant) -> Result<()> {
        self.run(now, |s| {
            if binding != s.binding || !matches!(s.pending, Some((Pending::Arm(_), _))) || s.armed {
                return Err(Failure::Ordering);
            }
            // Completion must consume the exact immutable proof that started
            // this operation, not merely a matching lease epoch.
            if s.pending_key() != s.last.map(|m| m.key) {
                return Err(Failure::Identity);
            }
            s.pending = None;
            s.armed = true;
            s.last_completion = Some(now);
            Ok(())
        })
    }
    pub(crate) fn begin_click(
        &mut self,
        key: FrameKey,
        x: u32,
        y: u32,
        now: Instant,
    ) -> Result<Click> {
        self.run(now, |s| {
            let metadata = s.slot(key, now)?.metadata;
            if s.opportunity() != Some(Opportunity::Click(key)) {
                return Err(Failure::Ordering);
            }
            if x >= s.canvas.0
                || y >= s.canvas.1
                || metadata.popup.is_some_and(|p| !p.contains(x, y))
            {
                return Err(Failure::Bounds);
            }
            let sequence = s
                .sequence
                .checked_add(1)
                .filter(|v| *v <= MAX_ACTIONS)
                .ok_or(Failure::Overflow)?;
            s.retired_refresh = None; // any dispatched operation retires cancellation eligibility
            let click = Click {
                key,
                sequence,
                x,
                y,
            };
            let fresh_deadline = s
                .slot
                .unwrap()
                .captured_at
                .checked_add(FRESH)
                .ok_or(Failure::Overflow)?;
            s.slot = None;
            s.pending = Some((
                Pending::Click(click),
                now.checked_add(OPERATION_TIMEOUT)
                    .ok_or(Failure::Overflow)?
                    .min(s.deadline)
                    .min(fresh_deadline),
            ));
            Ok(click)
        })
    }
    /// Native accepted means dispatch, not application persistence or rendering.
    pub(crate) fn complete_click(
        &mut self,
        binding: Binding,
        sequence: u32,
        now: Instant,
    ) -> Result<()> {
        self.run(now, |s| {
            let Some((Pending::Click(click), _)) = s.pending else {
                return Err(Failure::Ordering);
            };
            if binding != s.binding
                || sequence != click.sequence
                || s.pending_key() != s.last.map(|m| m.key)
            {
                return Err(Failure::Identity);
            }
            s.native_sequence = s
                .native_sequence
                .checked_add(1)
                .filter(|v| *v <= MAX_ACTIONS)
                .ok_or(Failure::Overflow)?;
            s.sequence = sequence;
            s.pending = None;
            s.last_completion = Some(now);
            Ok(())
        })
    }
    pub(crate) fn pending_key(&self) -> Option<FrameKey> {
        self.pending.map(|(p, _)| p.key())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn binding() -> Binding {
        Binding {
            source_instance: 7,
            source_epoch: 3,
            lease_epoch: 9,
        }
    }
    fn metadata(n: u64) -> Metadata {
        Metadata {
            key: FrameKey {
                binding: binding(),
                geometry_epoch: 1,
                wire_sequence: n - 1,
                proof_id: n,
                native_frame_id: 100 + n,
                native_ready_serial: (1u64 << 40) + n,
                family_generation: 1,
            },
            canvas: (320, 240),
            popup: None,
        }
    }
    fn new(now: Instant) -> Ledger {
        Ledger::new(binding(), (320, 240), now).unwrap()
    }
    fn reserve(l: &mut Ledger, n: u64, t: Instant) -> FrameKey {
        let m = metadata(n);
        l.reserve(m, t, t).unwrap();
        m.key
    }
    fn displayed(l: &mut Ledger, n: u64, t: Instant) -> FrameKey {
        let key = reserve(l, n, t);
        l.commit_write(key, t).unwrap();
        l.acknowledge_exact(binding(), key.wire_sequence, t)
            .unwrap();
        key
    }
    fn armed(now: Instant) -> Ledger {
        let mut l = new(now);
        let key = displayed(&mut l, 1, now);
        l.begin_arm(key, now).unwrap();
        assert_eq!(l.pending_key(), Some(key));
        l.complete_arm(binding(), now).unwrap();
        l
    }
    #[test]
    fn exact_ack_and_successful_write_are_both_required_in_either_order() {
        let now = Instant::now();
        for ack_first in [false, true] {
            let mut l = new(now);
            let key = reserve(&mut l, 1, now);
            assert_eq!(l.ready(now).unwrap(), None);
            if ack_first {
                l.acknowledge_exact(binding(), 0, now).unwrap();
            } else {
                l.commit_write(key, now).unwrap();
            }
            assert_eq!(l.ready(now).unwrap(), None);
            if ack_first {
                l.commit_write(key, now).unwrap();
            } else {
                l.acknowledge_exact(binding(), 0, now).unwrap();
            }
            assert_eq!(l.ready(now).unwrap(), Some(Opportunity::Arm(key)));
            assert!(l.pending_key().is_none()); // ACK does not arm or click.
        }
    }
    #[test]
    fn early_ack_then_failed_write_or_retirement_never_grants() {
        let now = Instant::now();
        for failed_write in [false, true] {
            let mut l = new(now);
            let key = reserve(&mut l, 1, now);
            l.acknowledge_exact(binding(), 0, now).unwrap();
            if failed_write {
                assert_eq!(l.write_failed(), Err(Failure::WriteFailed));
            } else {
                assert_eq!(l.retire_without_presentation(), Err(Failure::Retired));
            }
            assert_eq!(l.commit_write(key, now), Err(Failure::Ended));
            assert_eq!(l.ready(now), Err(Failure::Ended));
        }
    }
    #[test]
    fn cumulative_future_unknown_or_duplicate_ack_cannot_present_an_earlier_frame() {
        let now = Instant::now();
        let mut l = new(now);
        reserve(&mut l, 1, now);
        assert_eq!(
            l.acknowledge_exact(binding(), 1, now),
            Err(Failure::Identity)
        );
        assert_eq!(l.acknowledge_exact(binding(), 0, now), Err(Failure::Ended));
        let mut l = new(now);
        displayed(&mut l, 1, now);
        assert_eq!(
            l.acknowledge_exact(binding(), 0, now),
            Err(Failure::Duplicate)
        );
        let mut l = new(now);
        assert_eq!(
            l.acknowledge_exact(binding(), 0, now),
            Err(Failure::Ordering)
        );
    }
    #[test]
    fn full_host_identity_and_write_completion_are_not_interchangeable() {
        let now = Instant::now();
        for field in 0..8 {
            let mut l = new(now);
            let mut key = reserve(&mut l, 1, now);
            match field {
                0 => key.binding.source_instance += 1,
                1 => key.binding.source_epoch += 1,
                2 => key.binding.lease_epoch += 1,
                3 => key.geometry_epoch += 1,
                4 => key.native_frame_id += 1,
                5 => key.native_ready_serial += 1,
                6 => key.family_generation += 1,
                _ => key.proof_id += 1,
            }
            assert_eq!(l.commit_write(key, now), Err(Failure::Identity));
        }
        let mut l = new(now);
        let key = reserve(&mut l, 1, now);
        l.commit_write(key, now).unwrap();
        assert_eq!(l.commit_write(key, now), Err(Failure::Duplicate));
        for b in [
            Binding {
                source_instance: 8,
                ..binding()
            },
            Binding {
                source_epoch: 4,
                ..binding()
            },
            Binding {
                lease_epoch: 10,
                ..binding()
            },
        ] {
            let mut l = new(now);
            reserve(&mut l, 1, now);
            assert_eq!(l.acknowledge_exact(b, 0, now), Err(Failure::Identity));
        }
    }
    #[test]
    fn freshness_and_original_lease_are_absolute_no_late_commit_revival() {
        let now = Instant::now();
        let mut l = new(now);
        let key = reserve(&mut l, 1, now);
        assert_eq!(l.commit_write(key, now + FRESH), Err(Failure::Stale));
        assert_eq!(l.commit_write(key, now), Err(Failure::Ended));
        let mut l = new(now);
        let t = now + TTL - Duration::from_millis(100);
        let key = reserve(&mut l, 1, t);
        l.acknowledge_exact(binding(), 0, t).unwrap();
        assert_eq!(l.commit_write(key, now + TTL), Err(Failure::Deadline));
        let mut l = new(now);
        assert_eq!(
            l.reserve(metadata(1), now + Duration::from_nanos(1), now),
            Err(Failure::Stale)
        );
    }
    #[test]
    fn arm_consumes_first_frame_and_needs_post_completion_capture_before_click() {
        let now = Instant::now();
        let mut l = armed(now);
        assert_eq!(l.ready(now).unwrap(), None);
        let later = now + Duration::from_millis(1);
        let key = displayed(&mut l, 2, later);
        assert_eq!(l.ready(later).unwrap(), Some(Opportunity::Click(key)));
        let click = l.begin_click(key, 10, 10, later).unwrap();
        assert_eq!(click.sequence, 1);
        assert_eq!(l.pending_key(), Some(key));
        l.complete_click(binding(), 1, later).unwrap();
        assert_eq!(l.ready(later).unwrap(), None);
        let mut l = armed(now);
        assert_eq!(l.reserve(metadata(2), now, now), Err(Failure::Stale));
        let mut l = new(now);
        let key = displayed(&mut l, 1, now);
        l.begin_arm(key, now).unwrap();
        assert_eq!(l.reserve(metadata(2), later, later), Err(Failure::Ordering));
        assert_eq!(l.complete_arm(binding(), later), Err(Failure::Ended));
    }
    #[test]
    fn wrong_duplicate_or_late_native_completions_end_without_regrant() {
        let now = Instant::now();
        let mut l = new(now);
        assert_eq!(l.complete_arm(binding(), now), Err(Failure::Ordering));
        let mut l = armed(now);
        assert_eq!(l.complete_arm(binding(), now), Err(Failure::Ordering));
        let mut l = armed(now);
        let t = now + Duration::from_millis(1);
        let k = displayed(&mut l, 2, t);
        l.begin_click(k, 1, 1, t).unwrap();
        assert_eq!(l.complete_click(binding(), 2, t), Err(Failure::Identity));
        let mut l = new(now);
        let k = displayed(&mut l, 1, now);
        l.begin_arm(k, now).unwrap();
        assert_eq!(
            l.complete_arm(binding(), now + OPERATION_TIMEOUT),
            Err(Failure::Deadline)
        );
    }
    #[test]
    fn metadata_bounds_generation_and_counter_exhaustion_are_strict() {
        let now = Instant::now();
        for region in [
            Region {
                x: 0,
                y: 0,
                width: 0,
                height: 1,
            },
            Region {
                x: u32::MAX,
                y: 0,
                width: 2,
                height: 1,
            },
            Region {
                x: 319,
                y: 0,
                width: 2,
                height: 1,
            },
        ] {
            let mut l = new(now);
            let mut m = metadata(1);
            m.popup = Some(region);
            assert_eq!(l.reserve(m, now, now), Err(Failure::InvalidMetadata));
        }
        let mut l = armed(now);
        let mut m = metadata(2);
        m.popup = Some(Region {
            x: 10,
            y: 10,
            width: 20,
            height: 20,
        });
        let t = now + Duration::from_millis(1);
        assert_eq!(l.reserve(m, t, t), Err(Failure::Stale));
        let mut l = armed(now);
        let mut last = l.last.unwrap();
        last.key.proof_id = u64::MAX;
        l.last = Some(last);
        assert_eq!(l.reserve(metadata(2), t, t), Err(Failure::Overflow));
        let mut l = armed(now);
        let k = displayed(&mut l, 2, t);
        l.sequence = MAX_ACTIONS;
        assert_eq!(l.begin_click(k, 1, 1, t), Err(Failure::Overflow));
    }
    #[test]
    fn popup_target_does_not_fall_through_and_native_invalidation_is_terminal() {
        let now = Instant::now();
        let t = now + Duration::from_millis(1);
        let mut l = armed(now);
        let mut m = metadata(2);
        m.key.family_generation = 2;
        m.popup = Some(Region {
            x: 10,
            y: 10,
            width: 20,
            height: 20,
        });
        l.reserve(m, t, t).unwrap();
        l.commit_write(m.key, t).unwrap();
        l.acknowledge_exact(binding(), 1, t).unwrap();
        assert_eq!(l.begin_click(m.key, 9, 10, t), Err(Failure::Bounds));
        let mut l = armed(now);
        displayed(&mut l, 2, t);
        l.revoke();
        assert_eq!(l.ready(t), Err(Failure::Ended));
    }
    #[test]
    fn decoded_click_cannot_substitute_any_retained_scope_field() {
        let start = Instant::now();
        let now = start + Duration::from_millis(1);
        for field in 0..9 {
            let mut ledger = armed(start);
            let key = displayed(&mut ledger, 2, now);
            let mut request = Click {
                key,
                sequence: 1,
                x: 1,
                y: 1,
            };
            match field {
                0 => request.key.binding.source_instance += 1,
                1 => request.key.binding.source_epoch += 1,
                2 => request.key.binding.lease_epoch += 1,
                3 => request.key.geometry_epoch += 1,
                4 => request.key.wire_sequence += 1,
                5 => request.key.proof_id += 1,
                6 => request.key.native_frame_id += 1,
                7 => request.key.native_ready_serial += 1,
                8 => request.key.family_generation += 1,
                _ => unreachable!(),
            }
            // Every mutation is structurally valid on the real wire; only the
            // host's retained full-key comparison can reject substitution.
            let decoded = Click::decode(&request.encode().unwrap()).unwrap();
            assert!(
                matches!(ledger.authorize_click(decoded, now), Err(Failure::Identity)),
                "field {field}"
            );
            assert_eq!(ledger.ready(now), Err(Failure::Ended));
            assert!(matches!(
                ledger.authorize_click(
                    Click {
                        key,
                        sequence: 1,
                        x: 1,
                        y: 1
                    },
                    now
                ),
                Err(Failure::Ended)
            ));
        }
    }

    fn refresh_with_replacement(
        start: Instant,
        acked: bool,
    ) -> (Ledger, FrameKey, FrameKey, Instant) {
        let mut l = armed(start);
        let t = start + Duration::from_millis(1);
        let old = displayed(&mut l, 2, t);
        l.refresh_presented(old, t).unwrap();
        let b = reserve(&mut l, 3, t);
        l.commit_write(b, t).unwrap();
        if acked {
            l.acknowledge_exact(binding(), b.wire_sequence, t).unwrap();
        }
        (l, old, b, t + Duration::from_millis(1))
    }
    fn stale_request(key: FrameKey) -> Click {
        Click {
            key,
            sequence: 1,
            x: 1,
            y: 1,
        }
    }
    fn reject(l: &mut Ledger, old: FrameKey, t: Instant) {
        let request = Click::decode(&stale_request(old).encode().unwrap()).unwrap();
        assert!(
            matches!(l.resolve_click(request, t), Ok(ClickResolution::RejectedStale { metadata, sequence: 1 }) if metadata.key == old)
        );
        assert_eq!(l.native_sequence, 0);
        assert!(l.pending.is_none());
        assert_eq!(l.sequence, 1);
    }
    #[test]
    fn stale_refresh_rejection_never_dispatches_and_native_sequence_has_no_gap() {
        let start = Instant::now();
        let (mut l, old, b, t) = refresh_with_replacement(start, false);
        reject(&mut l, old, t);
        assert_eq!(l.ready(t).unwrap(), None);
        assert_eq!(
            l.acknowledge_frame(binding(), b.wire_sequence, t).unwrap(),
            AckDisposition::Discarded
        );
        assert_eq!(l.ready(t).unwrap(), None);
        let later = t + Duration::from_nanos(1);
        let c = displayed(&mut l, 4, later);
        let request = Click {
            key: c,
            sequence: 2,
            x: 1,
            y: 1,
        };
        let ClickResolution::Dispatch(permit) = l.resolve_click(request, later).unwrap() else {
            panic!("new click must dispatch");
        };
        assert_eq!(
            permit.into_parts().2,
            NativeOperation::Click {
                sequence: 1,
                x: 1,
                y: 1
            }
        );
        l.complete_click(binding(), 2, later).unwrap();
        assert_eq!((l.sequence, l.native_sequence), (2, 1));
    }
    #[test]
    fn discarded_ack_after_new_ready_only_clears_its_one_slot() {
        let start = Instant::now();
        let (mut l, old, b, t) = refresh_with_replacement(start, false);
        reject(&mut l, old, t);
        let later = t + Duration::from_nanos(1);
        let c = displayed(&mut l, 4, later);
        let deadline = l.deadline;
        let completed = l.last_completion;
        assert_eq!(
            l.acknowledge_frame(binding(), b.wire_sequence, later)
                .unwrap(),
            AckDisposition::Discarded
        );
        assert_eq!(l.ready(later).unwrap(), Some(Opportunity::Click(c)));
        assert_eq!((l.deadline, l.last_completion), (deadline, completed));
        assert_eq!(
            l.acknowledge_frame(binding(), b.wire_sequence, later),
            Err(Failure::Identity)
        );
        assert_eq!(l.ready(later), Err(Failure::Ended));
    }
    #[test]
    fn already_acked_replacement_has_no_second_ack_allowance_and_capture_floor_is_strict() {
        let start = Instant::now();
        let (mut l, old, b, t) = refresh_with_replacement(start, true);
        reject(&mut l, old, t);
        assert!(l.discarded_ack.is_none());
        assert!(l.acknowledge_frame(binding(), b.wire_sequence, t).is_err());
        let (mut l, old, _, t) = refresh_with_replacement(start, false);
        reject(&mut l, old, t);
        assert_eq!(l.reserve(metadata(4), t, t), Err(Failure::Stale));
    }
    #[test]
    fn cancellation_unknown_duplicate_expired_and_unsent_proofs_fail_closed() {
        let start = Instant::now();
        let (mut l, old, _, t) = refresh_with_replacement(start, false);
        let mut wrong = stale_request(old);
        wrong.key.family_generation += 1;
        assert!(l.resolve_click(wrong, t).is_err());
        let (mut l, old, _, t) = refresh_with_replacement(start, false);
        reject(&mut l, old, t);
        assert!(l.resolve_click(stale_request(old), t).is_err());
        let (mut l, old, b, t) = refresh_with_replacement(start, false);
        reject(&mut l, old, t);
        assert_eq!(
            l.acknowledge_frame(binding(), b.wire_sequence, start + TTL),
            Err(Failure::Deadline)
        );
        let (mut l, old, _, t) = refresh_with_replacement(start, false);
        l.slot.as_mut().unwrap().committed = false;
        assert!(l.resolve_click(stale_request(old), t).is_err());
        assert!(l.discarded_ack.is_none());
    }
    #[test]
    fn cancellation_never_overwrites_an_unconsumed_discarded_ack() {
        let start = Instant::now();
        let (mut l, old, _, t) = refresh_with_replacement(start, false);
        reject(&mut l, old, t);
        let later = t + Duration::from_nanos(1);
        let c = displayed(&mut l, 4, later);
        l.refresh_presented(c, later).unwrap();
        let request = Click {
            key: c,
            sequence: 2,
            x: 1,
            y: 1,
        };
        assert!(matches!(
            l.resolve_click(request, later),
            Err(Failure::Ordering)
        ));
        assert_eq!(l.ready(later), Err(Failure::Ended));
    }

    #[test]
    fn terminal_delivery_is_exact_instance_pending_click_and_permanently_ended() {
        let start = Instant::now();
        let t = start + Duration::from_millis(1);
        let mut l = armed(start);
        let key = displayed(&mut l, 2, t);
        let permit = l
            .authorize_click(
                Click {
                    key,
                    sequence: 1,
                    x: 1,
                    y: 1,
                },
                t,
            )
            .unwrap();
        let identity = permit.identity.clone();
        let (metadata, _, _) = permit.into_parts();
        assert_eq!(
            l.terminal_delivery(&identity, metadata, 1, t, t).unwrap(),
            (metadata, 1)
        );
        assert_eq!(l.ready(t), Err(Failure::Ended));
        assert_eq!(
            l.terminal_delivery(&identity, metadata, 1, t, t),
            Err(Failure::Ended)
        );
    }
    #[test]
    fn terminal_delivery_does_not_rescue_foreign_late_arm_or_wrong_receipts() {
        let start = Instant::now();
        let t = start + Duration::from_millis(1);
        for case in 0..5 {
            let mut l = armed(start);
            let key = displayed(&mut l, 2, t);
            l.authorize_click(
                Click {
                    key,
                    sequence: 1,
                    x: 1,
                    y: 1,
                },
                t,
            )
            .unwrap();
            let identity = if case == 0 {
                Arc::new(())
            } else {
                l.identity.clone()
            };
            let mut metadata = metadata(2);
            if case == 1 {
                metadata.key.family_generation += 1;
            }
            let native = if case == 2 { 2 } else { 1 };
            let accepted = if case == 3 { t + FRESH } else { t };
            let observed = if case == 4 { t + FRESH } else { t };
            assert!(
                l.terminal_delivery(&identity, metadata, native, accepted, observed)
                    .is_err()
            );
            assert_eq!(l.ready(t), Err(Failure::Ended));
        }
        let mut l = new(start);
        let key = displayed(&mut l, 1, start);
        l.begin_arm(key, start).unwrap();
        let identity = l.identity.clone();
        assert_eq!(
            l.terminal_delivery(&identity, metadata(1), 1, start, start),
            Err(Failure::Ordering)
        );
    }

    fn terminal_receipt_cutoff_case(start: Instant, captured: Instant, cutoff: Instant) {
        let accepted_at = cutoff - Duration::from_nanos(1);
        for observed_at in [accepted_at, cutoff, cutoff + Duration::from_nanos(1)] {
            let mut l = armed(start);
            let original_deadline = l.deadline;
            let key = displayed(&mut l, 2, captured);
            let permit = l
                .authorize_click(
                    Click {
                        key,
                        sequence: 1,
                        x: 1,
                        y: 1,
                    },
                    captured,
                )
                .unwrap();
            assert_eq!(l.pending.unwrap().1, cutoff);
            let identity = permit.identity.clone();
            let (metadata, _, _) = permit.into_parts();
            // These are evidence arguments to the real ledger consumer, not a
            // fabricated NativeDeliveryReceipt or a claim of native delivery.
            let result = l.terminal_delivery(&identity, metadata, 1, accepted_at, observed_at);
            if observed_at < cutoff {
                assert_eq!(result, Ok((metadata, 1)));
            } else {
                assert_eq!(result, Err(Failure::Deadline));
            }
            // Both a terminal fact and a rejected late fact permanently retire
            // authority; neither advances counters or extends the lease.
            assert!(l.ended && l.pending.is_none() && l.slot.is_none());
            assert_eq!(l.deadline, original_deadline);
            assert_eq!((l.sequence, l.native_sequence), (0, 0));
            assert_eq!(l.ready(observed_at), Err(Failure::Ended));
            assert_eq!(
                l.terminal_delivery(&identity, metadata, 1, accepted_at, observed_at),
                Err(Failure::Ended)
            );
        }
    }

    #[test]
    fn terminal_receipt_timely_accepted_cannot_cross_capture_deadline() {
        let start = Instant::now();
        let captured = start + Duration::from_millis(1);
        terminal_receipt_cutoff_case(start, captured, captured + FRESH);
    }

    #[test]
    fn terminal_receipt_timely_accepted_cannot_cross_original_lease_deadline() {
        let start = Instant::now();
        let lease_end = start + TTL;
        let captured = lease_end - Duration::from_millis(500);
        terminal_receipt_cutoff_case(start, captured, lease_end);
    }

    #[test]
    fn operation_completion_keeps_original_capture_freshness_boundary() {
        let start = Instant::now();
        for delta in [Duration::from_nanos(1), Duration::ZERO] {
            let mut l = new(start);
            let key = displayed(&mut l, 1, start);
            l.begin_arm(key, start + FRESH - Duration::from_millis(1))
                .unwrap();
            let result = l.complete_arm(binding(), start + FRESH - delta);
            if delta.is_zero() {
                assert_eq!(result, Err(Failure::Deadline));
                assert_eq!(
                    l.complete_arm(binding(), start + FRESH),
                    Err(Failure::Ended)
                );
            } else {
                assert_eq!(result, Ok(()));
            }
        }
        let captured = start + Duration::from_millis(1);
        for late in [false, true] {
            let mut l = armed(start);
            let key = displayed(&mut l, 2, captured);
            l.begin_click(key, 1, 1, captured + FRESH - Duration::from_millis(1))
                .unwrap();
            let completion = captured + FRESH
                - if late {
                    Duration::ZERO
                } else {
                    Duration::from_nanos(1)
                };
            let result = l.complete_click(binding(), 1, completion);
            assert_eq!(result, if late { Err(Failure::Deadline) } else { Ok(()) });
            if late {
                assert_eq!(
                    l.complete_click(binding(), 1, completion),
                    Err(Failure::Ended)
                );
            }
        }
        let mut l = new(start);
        let key = displayed(&mut l, 1, start);
        l.begin_arm(key, start + FRESH - Duration::from_millis(1))
            .unwrap();
        assert_eq!(
            l.complete_arm(binding(), start + Duration::from_secs(4)),
            Err(Failure::Deadline)
        );
    }
}

pub(crate) const METADATA_LEN: usize = 93;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum StatusKind {
    Waiting = 0,
    Armed = 1,
    Ready = 2,
    Accepted = 3,
    Revoked = 4,
    Denied = 5,
    RejectedStale = 6,
    DeliveredTerminal = 7,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Status {
    pub(crate) kind: StatusKind,
    pub(crate) metadata: Metadata,
    pub(crate) action_sequence: u32,
    pub(crate) ttl_ms: u32,
}
fn u64_at(b: &[u8], i: usize) -> u64 {
    u64::from_be_bytes(b[i..i + 8].try_into().unwrap())
}
fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes(b[i..i + 4].try_into().unwrap())
}
impl Binding {
    pub(crate) fn encode(self) -> Result<[u8; 20]> {
        if !self.valid() {
            return Err(Failure::InvalidMetadata);
        }
        let mut b = [0; 20];
        b[..8].copy_from_slice(&self.source_instance.to_be_bytes());
        b[8..16].copy_from_slice(&self.source_epoch.to_be_bytes());
        b[16..].copy_from_slice(&self.lease_epoch.to_be_bytes());
        Ok(b)
    }
    pub(crate) fn decode(b: &[u8]) -> Result<Self> {
        if b.len() != 20 {
            return Err(Failure::InvalidMetadata);
        }
        let value = Self {
            source_instance: u64_at(b, 0),
            source_epoch: u64_at(b, 8),
            lease_epoch: u32_at(b, 16),
        };
        value.encode()?;
        Ok(value)
    }
}
impl FrameKey {
    pub(crate) fn encode(self) -> Result<[u8; 68]> {
        if self.geometry_epoch == 0
            || self.proof_id == 0
            || self.native_frame_id == 0
            || self.native_ready_serial == 0
            || self.family_generation == 0
        {
            return Err(Failure::InvalidMetadata);
        }
        let mut b = [0; 68];
        b[..20].copy_from_slice(&self.binding.encode()?);
        for (i, value) in [
            self.geometry_epoch,
            self.wire_sequence,
            self.proof_id,
            self.native_frame_id,
            self.native_ready_serial,
            self.family_generation,
        ]
        .into_iter()
        .enumerate()
        {
            b[20 + i * 8..28 + i * 8].copy_from_slice(&value.to_be_bytes());
        }
        Ok(b)
    }
    pub(crate) fn decode(b: &[u8]) -> Result<Self> {
        if b.len() != 68 {
            return Err(Failure::InvalidMetadata);
        }
        let value = Self {
            binding: Binding::decode(&b[..20])?,
            geometry_epoch: u64_at(b, 20),
            wire_sequence: u64_at(b, 28),
            proof_id: u64_at(b, 36),
            native_frame_id: u64_at(b, 44),
            native_ready_serial: u64_at(b, 52),
            family_generation: u64_at(b, 60),
        };
        value.encode()?;
        Ok(value)
    }
}
impl Metadata {
    pub(crate) fn encode(self) -> Result<[u8; METADATA_LEN]> {
        if !self.valid() {
            return Err(Failure::InvalidMetadata);
        }
        let mut b = [0; METADATA_LEN];
        b[..68].copy_from_slice(&self.key.encode()?);
        b[68..72].copy_from_slice(&self.canvas.0.to_be_bytes());
        b[72..76].copy_from_slice(&self.canvas.1.to_be_bytes());
        if let Some(p) = self.popup {
            b[76] = 1;
            for (i, n) in [p.x, p.y, p.width, p.height].into_iter().enumerate() {
                b[77 + i * 4..81 + i * 4].copy_from_slice(&n.to_be_bytes());
            }
        }
        Ok(b)
    }
    pub(crate) fn decode(b: &[u8]) -> Result<Self> {
        if b.len() != METADATA_LEN || b[76] > 1 || (b[76] == 0 && b[77..].iter().any(|v| *v != 0)) {
            return Err(Failure::InvalidMetadata);
        }
        let value = Self {
            key: FrameKey::decode(&b[..68])?,
            canvas: (u32_at(b, 68), u32_at(b, 72)),
            popup: (b[76] == 1).then(|| Region {
                x: u32_at(b, 77),
                y: u32_at(b, 81),
                width: u32_at(b, 85),
                height: u32_at(b, 89),
            }),
        };
        value.encode()?;
        Ok(value)
    }
}
impl Click {
    pub(crate) fn encode(self) -> Result<[u8; 80]> {
        if self.sequence == 0 || self.sequence > MAX_ACTIONS {
            return Err(Failure::Ordering);
        }
        let mut b = [0; 80];
        b[..68].copy_from_slice(&self.key.encode()?);
        b[68..72].copy_from_slice(&self.sequence.to_be_bytes());
        b[72..76].copy_from_slice(&self.x.to_be_bytes());
        b[76..80].copy_from_slice(&self.y.to_be_bytes());
        Ok(b)
    }
    #[cfg(any(test, feature = "remote-wayland"))]
    pub(crate) fn decode(b: &[u8]) -> Result<Self> {
        if b.len() != 80 {
            return Err(Failure::InvalidMetadata);
        }
        let value = Self {
            key: FrameKey::decode(&b[..68])?,
            sequence: u32_at(b, 68),
            x: u32_at(b, 72),
            y: u32_at(b, 76),
        };
        value.encode()?;
        Ok(value)
    }
}
impl Status {
    pub(crate) fn encode(self) -> Result<[u8; 102]> {
        if self.action_sequence > MAX_ACTIONS
            || if self.kind == StatusKind::DeliveredTerminal {
                self.ttl_ms != 0 || self.action_sequence == 0
            } else {
                self.ttl_ms == 0 || self.ttl_ms > 30000
            }
        {
            return Err(Failure::InvalidMetadata);
        }
        let mut b = [0; 102];
        b[0] = self.kind as u8;
        b[1..94].copy_from_slice(&self.metadata.encode()?);
        b[94..98].copy_from_slice(&self.action_sequence.to_be_bytes());
        b[98..102].copy_from_slice(&self.ttl_ms.to_be_bytes());
        Ok(b)
    }
    pub(crate) fn decode(b: &[u8]) -> Result<Self> {
        if b.len() != 102 {
            return Err(Failure::InvalidMetadata);
        }
        let kind = match b[0] {
            0 => StatusKind::Waiting,
            1 => StatusKind::Armed,
            2 => StatusKind::Ready,
            3 => StatusKind::Accepted,
            4 => StatusKind::Revoked,
            5 => StatusKind::Denied,
            6 => StatusKind::RejectedStale,
            7 => StatusKind::DeliveredTerminal,
            _ => return Err(Failure::InvalidMetadata),
        };
        let value = Self {
            kind,
            metadata: Metadata::decode(&b[1..94])?,
            action_sequence: u32_at(b, 94),
            ttl_ms: u32_at(b, 98),
        };
        value.encode()?;
        Ok(value)
    }
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid popup authority: {self:?}")
    }
}
impl std::error::Error for Failure {}

/// Opaque, single-use authorization minted only by this ledger's two-phase
/// send/presentation gate. It is never encoded or reconstructed from the wire.
#[cfg(any(test, feature = "remote-wayland"))]
pub(crate) struct NativePermit {
    identity: Arc<()>,
    metadata: Metadata,
    captured_at: Instant,
    operation: NativeOperation,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(any(test, feature = "remote-wayland"))]
pub(crate) enum NativeOperation {
    Arm,
    Click { sequence: u32, x: u32, y: u32 },
}
#[cfg(any(test, feature = "remote-wayland"))]
impl NativePermit {
    #[cfg(feature = "remote-wayland")]
    pub(crate) fn identity(&self) -> Arc<()> {
        self.identity.clone()
    }
    pub(crate) fn into_parts(self) -> (Metadata, Instant, NativeOperation) {
        let _identity = self.identity;
        (self.metadata, self.captured_at, self.operation)
    }
}
#[cfg(any(test, feature = "remote-wayland"))]
impl Ledger {
    pub(crate) fn authorize_arm(&mut self, key: FrameKey, now: Instant) -> Result<NativePermit> {
        let slot = self.run(now, |s| s.slot(key, now).copied())?;
        self.begin_arm(key, now)?;
        Ok(NativePermit {
            identity: self.identity.clone(),
            metadata: slot.metadata,
            captured_at: slot.captured_at,
            operation: NativeOperation::Arm,
        })
    }
    pub(crate) fn authorize_click(&mut self, request: Click, now: Instant) -> Result<NativePermit> {
        let slot = self.run(now, |s| {
            if s.sequence.checked_add(1) != Some(request.sequence) {
                return Err(Failure::Ordering);
            }
            s.slot(request.key, now).copied()
        })?;
        let native_sequence = self.run(now, |s| {
            s.native_sequence
                .checked_add(1)
                .filter(|n| *n <= MAX_ACTIONS)
                .ok_or(Failure::Overflow)
        })?;
        let click = self.begin_click(request.key, request.x, request.y, now)?;
        Ok(NativePermit {
            identity: self.identity.clone(),
            metadata: slot.metadata,
            captured_at: slot.captured_at,
            operation: NativeOperation::Click {
                sequence: native_sequence,
                x: click.x,
                y: click.y,
            },
        })
    }
    /// Refresh only an already exactly presented, committed proof while idle.
    /// The host must publish Waiting before discarding the native frame. No
    /// operation or old opportunity survives; the next proof still needs both
    /// successful write and its own exact ACK. Late old clicks remain rejected.
    pub(crate) fn refresh_presented(&mut self, key: FrameKey, now: Instant) -> Result<()> {
        self.run(now, |s| {
            s.slot(key, now)?;
            if s.pending.is_some() || s.opportunity() != Some(Opportunity::Click(key)) {
                return Err(Failure::Ordering);
            }
            s.retired_refresh = s.slot.map(|slot| slot.metadata);
            s.slot = None;
            Ok(())
        })
    }
}

/// An explicit no-dispatch result is distinct from a native permit. It never
/// asserts that an already dispatched or uncertain action did not execute.
#[cfg(any(test, feature = "remote-wayland"))]
pub(crate) enum ClickResolution {
    Dispatch(NativePermit),
    RejectedStale { metadata: Metadata, sequence: u32 },
}
#[cfg(any(test, feature = "remote-wayland"))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AckDisposition {
    Current,
    Discarded,
}
#[cfg(any(test, feature = "remote-wayland"))]
impl Ledger {
    /// Consume only an adapter-produced, exact native delivery receipt. This
    /// ends the ledger even on success; no new capture/permit can follow.
    #[cfg(feature = "remote-wayland")]
    pub(crate) fn complete_terminal_delivery(
        &mut self,
        receipt: crate::remote::wayland_capture::interactive_popup::NativeDeliveryReceipt,
        now: Instant,
    ) -> Result<(Metadata, u32)> {
        let (identity, metadata, native_sequence, accepted_at) = receipt.into_evidence();
        self.terminal_delivery(&identity, metadata, native_sequence, accepted_at, now)
    }
    fn terminal_delivery(
        &mut self,
        identity: &Arc<()>,
        metadata: Metadata,
        native_sequence: u32,
        accepted_at: Instant,
        now: Instant,
    ) -> Result<(Metadata, u32)> {
        let result = self.run(now, |s| {
            let Some((Pending::Click(click), deadline)) = s.pending else {
                return Err(Failure::Ordering);
            };
            if !Arc::ptr_eq(identity, &s.identity)
                || click.key != metadata.key
                || s.last != Some(metadata)
                || s.native_sequence.checked_add(1) != Some(native_sequence)
                || accepted_at < s.created
                || accepted_at > now
                || accepted_at >= deadline
            {
                return Err(Failure::Identity);
            }
            Ok((metadata, click.sequence))
        });
        self.revoke();
        result
    }
    pub(crate) fn resolve_click(
        &mut self,
        request: Click,
        now: Instant,
    ) -> Result<ClickResolution> {
        let retired = self.run(now, |s| {
            if s.pending.is_some()
                || !s.armed
                || request.sequence > MAX_ACTIONS
                || s.sequence.checked_add(1) != Some(request.sequence)
            {
                return Err(Failure::Ordering);
            }
            let Some(metadata) = s.retired_refresh.filter(|m| m.key == request.key) else {
                return Ok(None);
            };
            if s.discarded_ack.is_some() {
                return Err(Failure::Ordering);
            }
            if request.x >= metadata.canvas.0
                || request.y >= metadata.canvas.1
                || metadata
                    .popup
                    .is_some_and(|r| !r.contains(request.x, request.y))
            {
                return Err(Failure::Bounds);
            }
            // Only a fully sent replacement can produce a legitimate delayed
            // ACK. Never manufacture an ACK allowance for an unsent frame.
            if let Some(slot) = s.slot {
                if !slot.committed {
                    return Err(Failure::Ordering);
                }
                if !slot.acked {
                    s.discarded_ack = Some(slot.metadata.key);
                }
            }
            s.slot = None;
            s.retired_refresh = None;
            s.sequence = request.sequence;
            s.capture_after = Some(now); // actual dequeue; never a peer timestamp
            Ok(Some(metadata))
        })?;
        match retired {
            Some(metadata) => Ok(ClickResolution::RejectedStale {
                metadata,
                sequence: request.sequence,
            }),
            None => self
                .authorize_click(request, now)
                .map(ClickResolution::Dispatch),
        }
    }
    pub(crate) fn acknowledge_frame(
        &mut self,
        binding: Binding,
        sequence: u64,
        now: Instant,
    ) -> Result<AckDisposition> {
        let discarded = self.run(now, |s| {
            if let Some(key) = s.discarded_ack {
                if key.binding == binding && key.wire_sequence == sequence {
                    s.discarded_ack = None;
                    return Ok(true);
                }
            }
            Ok(false)
        })?;
        if discarded {
            return Ok(AckDisposition::Discarded);
        }
        self.acknowledge_exact(binding, sequence, now)?;
        Ok(AckDisposition::Current)
    }
}

#[cfg(test)]
mod codec_tests {
    use super::*;
    fn sample() -> Metadata {
        Metadata {
            key: FrameKey {
                binding: Binding {
                    source_instance: 1,
                    source_epoch: 2,
                    lease_epoch: 3,
                },
                geometry_epoch: 4,
                wire_sequence: 0,
                proof_id: 5,
                native_frame_id: 6,
                native_ready_serial: (1u64 << 40) + 7,
                family_generation: (1u64 << 41) + 8,
            },
            canvas: (320, 240),
            popup: Some(Region {
                x: 10,
                y: 20,
                width: 30,
                height: 40,
            }),
        }
    }
    #[test]
    fn popup_wire_exact_layout_roundtrips_without_truncating_scope() {
        let m = sample();
        assert_eq!(Metadata::decode(&m.encode().unwrap()).unwrap(), m);
        assert_eq!(FrameKey::decode(&m.key.encode().unwrap()).unwrap(), m.key);
        assert_eq!(
            Binding::decode(&m.key.binding.encode().unwrap()).unwrap(),
            m.key.binding
        );
        let c = Click {
            key: m.key,
            sequence: 1,
            x: 10,
            y: 20,
        };
        assert_eq!(Click::decode(&c.encode().unwrap()).unwrap(), c);
        for kind in [
            StatusKind::Waiting,
            StatusKind::Armed,
            StatusKind::Ready,
            StatusKind::Accepted,
            StatusKind::Revoked,
            StatusKind::Denied,
            StatusKind::RejectedStale,
        ] {
            let s = Status {
                kind,
                metadata: m,
                action_sequence: 0,
                ttl_ms: 1234,
            };
            assert_eq!(Status::decode(&s.encode().unwrap()).unwrap(), s);
        }
    }
    #[test]
    fn popup_metadata_edge_rect_and_each_absent_rect_byte_are_canonical() {
        let mut m = sample();
        m.popup = Some(Region {
            x: 319,
            y: 239,
            width: 1,
            height: 1,
        });
        let edge = m.encode().unwrap();
        assert_eq!(Metadata::decode(&edge).unwrap(), m);
        for (offset, value) in [
            (77, 320u32),
            (81, 240),
            (85, 2),
            (89, 2),
            (85, 0),
            (89, 0),
            (85, u32::MAX),
            (89, u32::MAX),
        ] {
            let mut malformed = edge;
            malformed[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
            assert_eq!(Metadata::decode(&malformed), Err(Failure::InvalidMetadata));
        }
        m.popup = None;
        let absent = m.encode().unwrap();
        assert_eq!(Metadata::decode(&absent).unwrap(), m);
        for offset in 77..METADATA_LEN {
            let mut malformed = absent;
            malformed[offset] = 1;
            assert_eq!(Metadata::decode(&malformed), Err(Failure::InvalidMetadata));
        }
    }

    #[test]
    fn terminal_delivery_status_has_no_live_ttl_and_requires_a_command() {
        let terminal = Status {
            kind: StatusKind::DeliveredTerminal,
            metadata: sample(),
            action_sequence: 1,
            ttl_ms: 0,
        };
        assert_eq!(
            Status::decode(&terminal.encode().unwrap()).unwrap(),
            terminal
        );
        assert!(
            Status {
                ttl_ms: 1,
                ..terminal
            }
            .encode()
            .is_err()
        );
        assert!(
            Status {
                action_sequence: 0,
                ..terminal
            }
            .encode()
            .is_err()
        );
        assert!(
            Status {
                kind: StatusKind::Accepted,
                ..terminal
            }
            .encode()
            .is_err()
        );
    }

    #[test]
    fn popup_wire_rejects_noncanonical_rect_unknown_kind_and_lengths() {
        let m = sample();
        let bytes = m.encode().unwrap();
        for length in 0..METADATA_LEN {
            assert!(Metadata::decode(&bytes[..length]).is_err());
        }
        let mut extra = bytes.to_vec();
        extra.push(0);
        assert!(Metadata::decode(&extra).is_err());
        let mut b = bytes;
        b[76] = 0;
        assert!(Metadata::decode(&b).is_err());
        b[76] = 2;
        assert!(Metadata::decode(&b).is_err());
        b = bytes;
        b[77..81].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(Metadata::decode(&b).is_err());
        let s = Status {
            kind: StatusKind::Ready,
            metadata: m,
            action_sequence: 1,
            ttl_ms: 1,
        };
        let mut b = s.encode().unwrap();
        b[0] = 8;
        assert!(Status::decode(&b).is_err());
        b = s.encode().unwrap();
        b[98..].fill(0);
        assert!(Status::decode(&b).is_err());
        let c = Click {
            key: m.key,
            sequence: 0,
            x: 10,
            y: 20,
        };
        assert!(c.encode().is_err());
    }
}
#[cfg(test)]
mod permit_tests {
    use super::*;
    #[test]
    fn refresh_is_only_idle_exactly_presented_and_never_an_operation() {
        let t = Instant::now();
        let binding = Binding {
            source_instance: 1,
            source_epoch: 1,
            lease_epoch: 1,
        };
        let metadata = |n| Metadata {
            key: FrameKey {
                binding,
                geometry_epoch: 1,
                wire_sequence: n - 1,
                proof_id: n,
                native_frame_id: n,
                native_ready_serial: n,
                family_generation: 1,
            },
            canvas: (32, 24),
            popup: None,
        };
        let mut l = Ledger::new(binding, (32, 24), t).unwrap();
        let m = metadata(1);
        l.reserve(m, t, t).unwrap();
        l.commit_write(m.key, t).unwrap();
        l.acknowledge_exact(binding, 0, t).unwrap();
        let permit = l.authorize_arm(m.key, t).unwrap();
        let (actual, at, operation) = permit.into_parts();
        assert_eq!((actual, at, operation), (m, t, NativeOperation::Arm));
        l.complete_arm(binding, t).unwrap();
        let t2 = t + Duration::from_millis(1);
        let m2 = metadata(2);
        l.reserve(m2, t2, t2).unwrap();
        l.commit_write(m2.key, t2).unwrap();
        l.acknowledge_exact(binding, 1, t2).unwrap();
        l.refresh_presented(m2.key, t2).unwrap();
        assert!(l.pending_key().is_none());
        assert_eq!(l.ready(t2).unwrap(), None);
        let t3 = t2 + Duration::from_millis(1);
        let m3 = metadata(3);
        l.reserve(m3, t3, t3).unwrap();
        assert_eq!(l.ready(t3).unwrap(), None);
        l.commit_write(m3.key, t3).unwrap();
        assert_eq!(l.ready(t3).unwrap(), None);
        l.acknowledge_exact(binding, 2, t3).unwrap();
        let request = Click {
            key: m3.key,
            sequence: 1,
            x: 1,
            y: 1,
        };
        let permit = l.authorize_click(request, t3).unwrap();
        assert_eq!(
            permit.into_parts().2,
            NativeOperation::Click {
                sequence: 1,
                x: 1,
                y: 1
            }
        );
    }
}
