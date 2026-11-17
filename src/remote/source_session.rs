//! Host-only source grants and fail-closed switching policy.
//!
//! Host integration serializes this state with
//! input/capture publication, tag input with its presented source version, and
//! release held input plus discard old video/input/cursor queues before install.
use super::x11_capture::CaptureArea;
use std::collections::BTreeMap;
use std::fmt;

pub const MAX_GRANTED_SOURCES: usize = 32;

/// Opaque session-local ID; the peer cannot supply a capture rectangle.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SourceId(pub u32);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceVersion {
    pub source: SourceId,
    pub epoch: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceError {
    InvalidGrants,
    NotGranted,
    StaleEpoch,
    TransitionPending,
    EpochExhausted,
    InvalidFrame,
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidGrants => "invalid host source grants",
            Self::NotGranted => "source was not granted by the host",
            Self::StaleEpoch => "stale source epoch",
            Self::TransitionPending => "source transition is not ready",
            Self::EpochExhausted => "source epoch exhausted; reconnect required",
            Self::InvalidFrame => "invalid first source frame",
        })
    }
}
impl std::error::Error for SourceError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Quiescing,
    Failed,
    WaitingForFrame,
    WaitingForPresentation(u64),
    Ready,
}

/// Immutable grants from explicit host configuration, never a peer catalog.
/// No API expands permissions or restores grants from an earlier connection.
#[derive(Debug)]
pub struct SourceSession {
    grants: BTreeMap<SourceId, CaptureArea>,
    current: SourceVersion,
    phase: Phase,
    last_frame: Option<u64>,
}

impl SourceSession {
    /// Every connection starts at epoch 1 and requires a presented keyframe.
    pub fn new(
        grants: impl IntoIterator<Item = (SourceId, CaptureArea)>,
        initial: SourceId,
    ) -> Result<Self, SourceError> {
        let mut granted = BTreeMap::new();
        for (id, area) in grants {
            let valid = match &area {
                CaptureArea::Root => true,
                CaptureArea::Window(id) => *id != 0,
                CaptureArea::Monitor(name) => {
                    !name.is_empty() && name.len() <= 256 && !name.chars().any(char::is_control)
                }
                CaptureArea::Region(region) => region.width != 0 && region.height != 0,
            };
            if id.0 == 0
                || granted.len() == MAX_GRANTED_SOURCES
                || !valid
                || granted.insert(id, area).is_some()
            {
                return Err(SourceError::InvalidGrants);
            }
        }
        if !granted.contains_key(&initial) {
            return Err(SourceError::NotGranted);
        }
        Ok(Self {
            grants: granted,
            current: SourceVersion {
                source: initial,
                epoch: 1,
            },
            phase: Phase::WaitingForFrame,
            last_frame: None,
        })
    }

    #[must_use]
    pub fn current(&self) -> SourceVersion {
        self.current
    }

    #[must_use]
    pub fn selected_area(&self) -> &CaptureArea {
        &self.grants[&self.current.source]
    }

    /// Fence old input before any native changes. Even selecting the same
    /// source reserves a fresh epoch. Caller must serialize against injection.
    pub fn begin_switch(
        &mut self,
        expected: u64,
        target: SourceId,
    ) -> Result<SourceVersion, SourceError> {
        if expected != self.current.epoch {
            return Err(SourceError::StaleEpoch);
        }
        if !self.grants.contains_key(&target) {
            return Err(SourceError::NotGranted);
        }
        if self.phase != Phase::Ready {
            return Err(SourceError::TransitionPending);
        }
        let epoch = self
            .current
            .epoch
            .checked_add(1)
            .ok_or(SourceError::EpochExhausted)?;
        self.current = SourceVersion {
            source: target,
            epoch,
        };
        self.phase = Phase::Quiescing;
        Ok(self.current)
    }

    /// Run the complete native release/invalidation/install operation. An
    /// error terminally blocks all capture/input. Close on uncertain native
    /// outcomes; do not roll back to a now-obsolete source.
    pub fn finish_switch<E>(
        &mut self,
        version: SourceVersion,
        quiesce_and_install: impl FnOnce(&CaptureArea) -> Result<(), E>,
    ) -> Result<Result<(), E>, SourceError> {
        self.check_version(version)?;
        if self.phase != Phase::Quiescing {
            return Err(SourceError::TransitionPending);
        }
        match quiesce_and_install(self.selected_area()) {
            Ok(()) => {
                self.phase = Phase::WaitingForFrame;
                Ok(Ok(()))
            }
            Err(error) => {
                self.phase = Phase::Failed;
                Ok(Err(error))
            }
        }
    }

    /// Call after publishing a full keyframe. Frame IDs remain monotonic
    /// across transitions; ordinary deltas cannot open the presentation gate.
    pub fn publish_keyframe(
        &mut self,
        version: SourceVersion,
        frame_id: u64,
    ) -> Result<(), SourceError> {
        self.check_version(version)?;
        if self.phase != Phase::WaitingForFrame {
            return Err(SourceError::TransitionPending);
        }
        if self.last_frame.is_some_and(|last| frame_id <= last) {
            return Err(SourceError::InvalidFrame);
        }
        self.last_frame = Some(frame_id);
        self.phase = Phase::WaitingForPresentation(frame_id);
        Ok(())
    }

    /// Only the validated drawn ACK of this first frame opens input, never
    /// network receipt, decode completion, or queued asynchronous rendering.
    pub fn acknowledge_presentation(
        &mut self,
        version: SourceVersion,
        frame_id: u64,
    ) -> Result<(), SourceError> {
        self.check_version(version)?;
        if self.phase != Phase::WaitingForPresentation(frame_id) {
            return Err(SourceError::InvalidFrame);
        }
        self.phase = Phase::Ready;
        Ok(())
    }

    #[must_use]
    pub fn accepts_input(&self, version: SourceVersion) -> bool {
        self.current == version && self.phase == Phase::Ready
    }

    /// Capture/cursor publication is blocked while native state changes.
    /// First video publication must additionally call publish_keyframe.
    #[must_use]
    pub fn accepts_capture(&self, version: SourceVersion) -> bool {
        self.current == version
            && matches!(
                self.phase,
                Phase::WaitingForFrame | Phase::WaitingForPresentation(_) | Phase::Ready
            )
    }

    #[must_use]
    pub fn needs_keyframe(&self) -> bool {
        self.phase == Phase::WaitingForFrame
    }

    #[must_use]
    pub fn awaiting_presentation(&self) -> bool {
        matches!(self.phase, Phase::WaitingForPresentation(_))
    }

    fn check_version(&self, version: SourceVersion) -> Result<(), SourceError> {
        if self.current != version {
            Err(SourceError::StaleEpoch)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::x11_capture::CaptureRegion;
    use super::*;
    fn session() -> SourceSession {
        SourceSession::new(
            [
                (SourceId(1), CaptureArea::Monitor("DP-1".into())),
                (SourceId(2), CaptureArea::Monitor("DP-2".into())),
            ],
            SourceId(1),
        )
        .unwrap()
    }
    fn ready(s: &mut SourceSession, frame: u64) -> SourceVersion {
        let v = s.current();
        s.publish_keyframe(v, frame).unwrap();
        s.acknowledge_presentation(v, frame).unwrap();
        v
    }
    #[test]
    fn initial_connection_requires_presented_keyframe() {
        let mut s = session();
        let v = s.current();
        assert!(!s.accepts_input(v));
        // Frame sequence zero is the first frame of a connection.
        s.publish_keyframe(v, 1).unwrap();
        assert!(!s.accepts_input(v));
        assert_eq!(
            s.acknowledge_presentation(v, 2),
            Err(SourceError::InvalidFrame)
        );
        s.acknowledge_presentation(v, 1).unwrap();
        assert!(s.accepts_input(v));
    }
    #[test]
    fn peer_cannot_expand_host_grants() {
        let mut s = session();
        let old = ready(&mut s, 1);
        assert_eq!(
            s.begin_switch(old.epoch, SourceId(3)),
            Err(SourceError::NotGranted)
        );
        assert!(s.accepts_input(old));
        assert_eq!(s.selected_area(), &CaptureArea::Monitor("DP-1".into()));
    }
    #[test]
    fn switch_fences_old_input_frames_cursor_and_ack() {
        let mut s = session();
        let old = ready(&mut s, 10);
        let new = s.begin_switch(old.epoch, SourceId(2)).unwrap();
        assert_eq!(new.epoch, 2);
        for v in [old, new] {
            assert!(!s.accepts_capture(v));
            assert!(!s.accepts_input(v));
        }
        assert_eq!(
            s.acknowledge_presentation(old, 10),
            Err(SourceError::StaleEpoch)
        );
        assert_eq!(
            s.begin_switch(new.epoch, SourceId(1)),
            Err(SourceError::TransitionPending)
        );
        s.finish_switch(new, |area| {
            assert_eq!(area, &CaptureArea::Monitor("DP-2".into()));
            Ok::<(), ()>(())
        })
        .unwrap()
        .unwrap();
        assert!(s.accepts_capture(new));
        assert_eq!(s.publish_keyframe(new, 10), Err(SourceError::InvalidFrame));
        s.publish_keyframe(new, 11).unwrap();
        assert!(!s.accepts_input(new));
        s.acknowledge_presentation(new, 11).unwrap();
        assert!(s.accepts_input(new));
        assert!(!s.accepts_input(old));
    }
    #[test]
    fn failed_release_or_install_keeps_every_source_blocked() {
        let mut s = session();
        let old = ready(&mut s, 1);
        let new = s.begin_switch(old.epoch, SourceId(2)).unwrap();
        assert_eq!(
            s.finish_switch(new, |_| Err("release failed")),
            Ok(Err("release failed"))
        );
        for v in [old, new] {
            assert!(!s.accepts_input(v));
            assert!(!s.accepts_capture(v));
        }
        assert_eq!(
            s.finish_switch::<()>(new, |_| panic!("failed transition cannot retry")),
            Err(SourceError::TransitionPending)
        );
        assert_eq!(
            s.publish_keyframe(new, 2),
            Err(SourceError::TransitionPending)
        );
    }
    #[test]
    fn same_source_never_reuses_epoch() {
        let mut s = session();
        let old = ready(&mut s, 1);
        let new = s.begin_switch(old.epoch, old.source).unwrap();
        assert_ne!(old.epoch, new.epoch);
        assert_eq!(
            s.begin_switch(old.epoch, old.source),
            Err(SourceError::StaleEpoch)
        );
        assert_eq!(
            s.finish_switch(old, |_| Ok::<(), ()>(())),
            Err(SourceError::StaleEpoch)
        );
    }
    #[test]
    fn epoch_exhaustion_cannot_wrap() {
        let mut s = session();
        s.current.epoch = u64::MAX;
        let old = ready(&mut s, 1);
        assert_eq!(
            s.begin_switch(old.epoch, SourceId(2)),
            Err(SourceError::EpochExhausted)
        );
        assert_eq!(s.current(), old);
        assert!(!s.accepts_input(SourceVersion { epoch: 1, ..old }));
    }
    #[test]
    fn invalid_and_duplicate_grants_fail_closed() {
        let root = (SourceId(1), CaptureArea::Root);
        assert!(SourceSession::new([], SourceId(1)).is_err());
        assert!(SourceSession::new([root.clone(), root], SourceId(1)).is_err());
        for area in [
            CaptureArea::Monitor(String::new()),
            CaptureArea::Monitor("DP-1\nRoot".into()),
            CaptureArea::Monitor("a".repeat(257)),
            CaptureArea::Region(CaptureRegion {
                x: 0,
                y: 0,
                width: 0,
                height: 1,
            }),
        ] {
            assert!(SourceSession::new([(SourceId(1), area)], SourceId(1)).is_err());
        }
        assert!(SourceSession::new([(SourceId(0), CaptureArea::Root)], SourceId(0)).is_err());
        assert!(
            SourceSession::new(
                (1..=33).map(|i| (SourceId(i), CaptureArea::Root)),
                SourceId(1)
            )
            .is_err()
        );
        assert!(
            SourceSession::new(
                (1..=32).map(|i| (SourceId(i), CaptureArea::Root)),
                SourceId(1)
            )
            .is_ok()
        );
    }
}
