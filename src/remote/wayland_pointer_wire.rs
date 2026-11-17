//! Bounded, source-scoped pointer records. No button edges, drag or axis exist.
use super::RemoteResult;
pub use super::wayland_keyboard_wire::DrawnScope;
use std::{io, time::Duration};
pub const LEASE_DURATION: Duration = Duration::from_secs(30);
pub const FRAME_MAX_AGE: Duration = Duration::from_secs(1);
pub const MAX_SEQUENCE: u64 = 60_000;
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid scoped pointer record")
}
fn read(b: &[u8], i: usize) -> u64 {
    u64::from_be_bytes(b[i..i + 8].try_into().unwrap())
}
fn put(b: &mut [u8], i: usize, v: u64) {
    b[i..i + 8].copy_from_slice(&v.to_be_bytes());
}
fn scope(b: &[u8]) -> RemoteResult<DrawnScope> {
    let s = DrawnScope {
        source_epoch: read(b, 0),
        geometry_epoch: read(b, 8),
        frame_sequence: read(b, 16),
    };
    if s.source_epoch == 0 || s.geometry_epoch == 0 {
        return Err(invalid().into());
    }
    Ok(s)
}
fn write_scope(b: &mut [u8], s: DrawnScope) -> RemoteResult<()> {
    put(b, 0, s.source_epoch);
    put(b, 8, s.geometry_epoch);
    put(b, 16, s.frame_sequence);
    scope(b)?;
    Ok(())
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PointerRequest {
    pub scope: DrawnScope,
}
impl PointerRequest {
    pub fn encode(self) -> RemoteResult<[u8; 24]> {
        let mut b = [0; 24];
        write_scope(&mut b, self.scope)?;
        Ok(b)
    }
    pub fn decode(b: &[u8]) -> RemoteResult<Self> {
        if b.len() != 24 {
            return Err(invalid().into());
        }
        Ok(Self { scope: scope(b)? })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PointerStatusKind {
    Ready = 1,
    Waiting = 2,
    Revoked = 3,
    Denied = 4,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PointerStatus {
    pub lease_epoch: u64,
    pub scope: DrawnScope,
    pub status: PointerStatusKind,
}
impl PointerStatus {
    pub fn encode(self) -> RemoteResult<[u8; 33]> {
        let mut b = [0; 33];
        put(&mut b, 0, self.lease_epoch);
        write_scope(&mut b[8..32], self.scope)?;
        b[32] = self.status as u8;
        Self::decode(&b)?;
        Ok(b)
    }
    pub fn decode(b: &[u8]) -> RemoteResult<Self> {
        if b.len() != 33 {
            return Err(invalid().into());
        }
        let status = match b[32] {
            1 => PointerStatusKind::Ready,
            2 => PointerStatusKind::Waiting,
            3 => PointerStatusKind::Revoked,
            4 => PointerStatusKind::Denied,
            _ => return Err(invalid().into()),
        };
        let lease_epoch = read(b, 0);
        if lease_epoch == 0
            && matches!(
                status,
                PointerStatusKind::Ready | PointerStatusKind::Waiting
            )
        {
            return Err(invalid().into());
        }
        Ok(Self {
            lease_epoch,
            scope: scope(&b[8..32])?,
            status,
        })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PointerActionKind {
    Motion = 0,
    ClickLeft = 1,
    ScrollUp = 2,
    ScrollDown = 3,
    ScrollLeft = 4,
    ScrollRight = 5,
}
impl PointerActionKind {
    pub(super) fn is_horizontal(self) -> bool {
        matches!(self, Self::ScrollLeft | Self::ScrollRight)
    }
    pub(super) fn is_scroll(self) -> bool {
        matches!(
            self,
            Self::ScrollUp | Self::ScrollDown | Self::ScrollLeft | Self::ScrollRight
        )
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PointerAction {
    pub lease_epoch: u64,
    pub sequence: u64,
    pub scope: DrawnScope,
    pub kind: PointerActionKind,
    pub x: u16,
    pub y: u16,
}
impl PointerAction {
    pub fn encode(self) -> RemoteResult<[u8; 45]> {
        let mut b = [0; 45];
        put(&mut b, 0, self.lease_epoch);
        put(&mut b, 8, self.sequence);
        write_scope(&mut b[16..40], self.scope)?;
        b[40] = self.kind as u8;
        b[41..43].copy_from_slice(&self.x.to_be_bytes());
        b[43..45].copy_from_slice(&self.y.to_be_bytes());
        Self::decode(&b)?;
        Ok(b)
    }
    pub fn decode(b: &[u8]) -> RemoteResult<Self> {
        if b.len() != 45 {
            return Err(invalid().into());
        }
        let lease_epoch = read(b, 0);
        let sequence = read(b, 8);
        if lease_epoch == 0 || sequence == 0 || sequence > MAX_SEQUENCE {
            return Err(invalid().into());
        }
        let kind = match b[40] {
            0 => PointerActionKind::Motion,
            1 => PointerActionKind::ClickLeft,
            2 => PointerActionKind::ScrollUp,
            3 => PointerActionKind::ScrollDown,
            4 => PointerActionKind::ScrollLeft,
            5 => PointerActionKind::ScrollRight,
            _ => return Err(invalid().into()),
        };
        Ok(Self {
            lease_epoch,
            sequence,
            scope: scope(&b[16..40])?,
            kind,
            x: u16::from_be_bytes(b[41..43].try_into().unwrap()),
            y: u16::from_be_bytes(b[43..45].try_into().unwrap()),
        })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PointerRevoke {
    pub lease_epoch: u64,
}
impl PointerRevoke {
    pub fn encode(self) -> RemoteResult<[u8; 8]> {
        if self.lease_epoch == 0 {
            return Err(invalid().into());
        }
        Ok(self.lease_epoch.to_be_bytes())
    }
    pub fn decode(b: &[u8]) -> RemoteResult<Self> {
        if b.len() != 8 || read(b, 0) == 0 {
            return Err(invalid().into());
        }
        Ok(Self {
            lease_epoch: read(b, 0),
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scoped_pointer_records_are_exact_and_have_no_button_edges() {
        let scope = DrawnScope {
            source_epoch: 1,
            geometry_epoch: 2,
            frame_sequence: 0,
        };
        let a = PointerAction {
            lease_epoch: 1,
            sequence: 1,
            scope,
            kind: PointerActionKind::ClickLeft,
            x: 10,
            y: 20,
        };
        let mut b = a.encode().unwrap();
        assert_eq!(PointerAction::decode(&b).unwrap(), a);
        for n in 0..b.len() {
            assert!(PointerAction::decode(&b[..n]).is_err());
        }
        for (byte, kind) in [
            (2, PointerActionKind::ScrollUp),
            (3, PointerActionKind::ScrollDown),
            (4, PointerActionKind::ScrollLeft),
            (5, PointerActionKind::ScrollRight),
        ] {
            b[40] = byte;
            let parsed = PointerAction::decode(&b).unwrap();
            assert_eq!(parsed.kind, kind);
            assert!(parsed.kind.is_scroll());
            assert_eq!(parsed.encode().unwrap(), b);
        }
        b[40] = 6;
        assert!(PointerAction::decode(&b).is_err());
        b[40] = 1;
        b[8..16].fill(0);
        assert!(PointerAction::decode(&b).is_err());
        let request = PointerRequest { scope };
        assert_eq!(
            PointerRequest::decode(&request.encode().unwrap()).unwrap(),
            request
        );
        for status in [
            PointerStatusKind::Ready,
            PointerStatusKind::Waiting,
            PointerStatusKind::Revoked,
            PointerStatusKind::Denied,
        ] {
            let s = PointerStatus {
                lease_epoch: 1,
                scope,
                status,
            };
            assert_eq!(PointerStatus::decode(&s.encode().unwrap()).unwrap(), s);
        }
        assert!(PointerRevoke { lease_epoch: 0 }.encode().is_err());
    }
}

/// Admission only: a successful action still requires the native frame proof.
#[derive(Default)]
pub(super) struct HostPointerState {
    scope: Option<DrawnScope>,
    attempted: bool,
    revoked: bool,
    epoch: Option<u64>,
    deadline: Option<std::time::Instant>,
    ready: Option<(DrawnScope, u64, std::time::Instant)>,
    last_serial: u64,
    sequence: u64,
}
impl HostPointerState {
    pub(super) fn request(
        &mut self,
        request: PointerRequest,
        drawn: DrawnScope,
    ) -> RemoteResult<()> {
        if self.attempted || self.revoked || request.scope != drawn {
            self.revoke();
            return Err(invalid().into());
        }
        self.scope = Some(drawn);
        self.attempted = true;
        Ok(())
    }
    pub(super) fn grant(&mut self, epoch: u64, now: std::time::Instant) -> RemoteResult<()> {
        if !self.attempted || self.revoked || self.epoch.is_some() || epoch == 0 {
            self.revoke();
            return Err(invalid().into());
        }
        self.epoch = Some(epoch);
        self.deadline = Some(now + LEASE_DURATION);
        Ok(())
    }
    pub(super) fn scope(&self) -> Option<DrawnScope> {
        self.scope
    }
    pub(super) fn epoch(&self) -> Option<u64> {
        self.epoch
    }
    pub(super) fn active(&self, now: std::time::Instant) -> bool {
        !self.revoked && self.deadline.is_some_and(|deadline| now < deadline)
    }
    pub(super) fn ready(
        &mut self,
        scope: DrawnScope,
        serial: u64,
        captured_at: std::time::Instant,
        now: std::time::Instant,
    ) -> RemoteResult<PointerStatus> {
        if !self.active(now)
            || serial <= self.last_serial
            || now.saturating_duration_since(captured_at) >= FRAME_MAX_AGE
        {
            return Err(invalid().into());
        }
        self.scope = Some(scope);
        self.last_serial = serial;
        self.ready = Some((scope, serial, captured_at));
        Ok(PointerStatus {
            lease_epoch: self.epoch.unwrap(),
            scope,
            status: PointerStatusKind::Ready,
        })
    }
    pub(super) fn consume(
        &mut self,
        action: PointerAction,
        size: (u16, u16),
        now: std::time::Instant,
    ) -> RemoteResult<u64> {
        let ready = self.ready.take();
        let valid = self.active(now)
            && action.lease_epoch == self.epoch.unwrap_or(0)
            && action.sequence == self.sequence + 1
            && action.sequence <= MAX_SEQUENCE
            && action.x < size.0
            && action.y < size.1
            && ready.is_some_and(|(scope, _, at)| {
                action.scope == scope && now.saturating_duration_since(at) < FRAME_MAX_AGE
            });
        if !valid {
            self.revoke();
            return Err(invalid().into());
        }
        self.sequence = action.sequence;
        Ok(ready.unwrap().1)
    }
    pub(super) fn clear_ready(&mut self) {
        self.ready = None;
    }
    pub(super) fn revoke(&mut self) {
        self.revoked = true;
        self.ready = None;
    }
}
#[cfg(test)]
mod admission_tests {
    use super::*;
    #[test]
    fn pointer_each_motion_consumes_a_distinct_fresh_frame() {
        let now = std::time::Instant::now();
        let scope = DrawnScope {
            source_epoch: 1,
            geometry_epoch: 1,
            frame_sequence: 0,
        };
        let mut state = HostPointerState::default();
        state.request(PointerRequest { scope }, scope).unwrap();
        state.grant(1, now).unwrap();
        state.ready(scope, 1, now, now).unwrap();
        let action = PointerAction {
            lease_epoch: 1,
            sequence: 1,
            scope,
            kind: PointerActionKind::Motion,
            x: 0,
            y: 0,
        };
        assert_eq!(state.consume(action, (10, 10), now).unwrap(), 1);
        assert!(
            state
                .consume(
                    PointerAction {
                        sequence: 2,
                        ..action
                    },
                    (10, 10),
                    now
                )
                .is_err()
        );
        assert!(!state.active(now));
        assert!(state.grant(2, now).is_err());
    }
    #[test]
    fn pointer_old_frame_and_wrong_scope_cannot_be_retargeted() {
        let now = std::time::Instant::now();
        let scope = DrawnScope {
            source_epoch: 1,
            geometry_epoch: 1,
            frame_sequence: 0,
        };
        let mut state = HostPointerState::default();
        state.request(PointerRequest { scope }, scope).unwrap();
        state.grant(1, now).unwrap();
        assert!(state.ready(scope, 1, now, now + FRAME_MAX_AGE).is_err());
        state.ready(scope, 2, now, now).unwrap();
        assert!(state.ready(scope, 2, now, now).is_err());
        let other = DrawnScope {
            frame_sequence: 1,
            ..scope
        };
        assert!(
            state
                .consume(
                    PointerAction {
                        lease_epoch: 1,
                        sequence: 1,
                        scope: other,
                        kind: PointerActionKind::ClickLeft,
                        x: 1,
                        y: 1
                    },
                    (10, 10),
                    now
                )
                .is_err()
        );
    }
}
