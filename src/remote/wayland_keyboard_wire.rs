//! Fixed-size keyboard lease records. Native authorization and keymap proof
//! remain mandatory at the caller; successful admission is not native delivery.
use super::RemoteResult;
use std::collections::BTreeSet;
use std::io;
use std::time::{Duration, Instant};

pub const LEASE_REQUEST_LEN: usize = 56;
pub const LEASE_STATUS_LEN: usize = 33;
pub const KEY_LEN: usize = 43;
pub const LEASE_DURATION: Duration = Duration::from_secs(30);
const BURST: f64 = 32.0;
const EDGES_PER_SECOND: f64 = 120.0;

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn read(bytes: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes(bytes[offset..offset + 8].try_into().unwrap())
}
fn put(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_be_bytes());
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DrawnScope {
    pub source_epoch: u64,
    pub geometry_epoch: u64,
    pub frame_sequence: u64,
}
impl DrawnScope {
    fn validate(self) -> RemoteResult<()> {
        if self.source_epoch == 0 || self.geometry_epoch == 0 {
            return Err(invalid("zero keyboard source or geometry epoch").into());
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeaseRequest {
    pub keymap_digest: [u8; 32],
    pub source_epoch: u64,
    pub geometry_epoch: u64,
    pub frame_sequence: u64,
}
impl LeaseRequest {
    pub fn scope(self) -> DrawnScope {
        DrawnScope {
            source_epoch: self.source_epoch,
            geometry_epoch: self.geometry_epoch,
            frame_sequence: self.frame_sequence,
        }
    }
    fn validate(self) -> RemoteResult<()> {
        self.scope().validate()?;
        if self.keymap_digest == [0; 32] {
            return Err(invalid("zero keyboard keymap digest").into());
        }
        Ok(())
    }
    pub fn encode(self) -> RemoteResult<[u8; LEASE_REQUEST_LEN]> {
        self.validate()?;
        let mut bytes = [0; LEASE_REQUEST_LEN];
        put(&mut bytes, 0, self.source_epoch);
        put(&mut bytes, 8, self.geometry_epoch);
        put(&mut bytes, 16, self.frame_sequence);
        bytes[24..56].copy_from_slice(&self.keymap_digest);
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> RemoteResult<Self> {
        if bytes.len() != LEASE_REQUEST_LEN {
            return Err(invalid("invalid keyboard lease request length").into());
        }
        let value = Self {
            keymap_digest: bytes[24..56].try_into().unwrap(),
            source_epoch: read(bytes, 0),
            geometry_epoch: read(bytes, 8),
            frame_sequence: read(bytes, 16),
        };
        value.validate()?;
        Ok(value)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum LeaseStatusKind {
    Granted = 1,
    Denied = 2,
    Revoked = 3,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeaseStatus {
    pub lease_epoch: u64,
    pub source_epoch: u64,
    pub geometry_epoch: u64,
    pub frame_sequence: u64,
    pub status: LeaseStatusKind,
}
impl LeaseStatus {
    fn validate(self) -> RemoteResult<()> {
        DrawnScope {
            source_epoch: self.source_epoch,
            geometry_epoch: self.geometry_epoch,
            frame_sequence: self.frame_sequence,
        }
        .validate()?;
        if self.status == LeaseStatusKind::Granted && self.lease_epoch == 0 {
            return Err(invalid("zero granted keyboard epoch").into());
        }
        Ok(())
    }
    pub fn encode(self) -> RemoteResult<[u8; LEASE_STATUS_LEN]> {
        self.validate()?;
        let mut bytes = [0; LEASE_STATUS_LEN];
        put(&mut bytes, 0, self.lease_epoch);
        put(&mut bytes, 8, self.source_epoch);
        put(&mut bytes, 16, self.geometry_epoch);
        put(&mut bytes, 24, self.frame_sequence);
        bytes[32] = self.status as u8;
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> RemoteResult<Self> {
        if bytes.len() != LEASE_STATUS_LEN {
            return Err(invalid("invalid keyboard lease status length").into());
        }
        let status = match bytes[32] {
            1 => LeaseStatusKind::Granted,
            2 => LeaseStatusKind::Denied,
            3 => LeaseStatusKind::Revoked,
            _ => return Err(invalid("unknown keyboard lease status").into()),
        };
        let value = Self {
            lease_epoch: read(bytes, 0),
            source_epoch: read(bytes, 8),
            geometry_epoch: read(bytes, 16),
            frame_sequence: read(bytes, 24),
            status,
        };
        value.validate()?;
        Ok(value)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    pub lease_epoch: u64,
    pub sequence: u64,
    pub source_epoch: u64,
    pub geometry_epoch: u64,
    pub frame_sequence: u64,
    pub keycode: u16,
    pub pressed: bool,
}
impl Key {
    pub fn scope(self) -> DrawnScope {
        DrawnScope {
            source_epoch: self.source_epoch,
            geometry_epoch: self.geometry_epoch,
            frame_sequence: self.frame_sequence,
        }
    }
    fn validate(self) -> RemoteResult<()> {
        self.scope().validate()?;
        if self.lease_epoch == 0
            || self.sequence == 0
            || self.sequence > 60_000
            || !(1..=247).contains(&self.keycode)
        {
            return Err(invalid("invalid keyboard key identity or code").into());
        }
        Ok(())
    }
    pub fn encode(self) -> RemoteResult<[u8; KEY_LEN]> {
        self.validate()?;
        let mut bytes = [0; KEY_LEN];
        put(&mut bytes, 0, self.lease_epoch);
        put(&mut bytes, 8, self.sequence);
        put(&mut bytes, 16, self.source_epoch);
        put(&mut bytes, 24, self.geometry_epoch);
        put(&mut bytes, 32, self.frame_sequence);
        bytes[40..42].copy_from_slice(&self.keycode.to_be_bytes());
        bytes[42] = u8::from(self.pressed);
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> RemoteResult<Self> {
        if bytes.len() != KEY_LEN {
            return Err(invalid("invalid keyboard key length").into());
        }
        let pressed = match bytes[42] {
            0 => false,
            1 => true,
            _ => return Err(invalid("invalid keyboard boolean").into()),
        };
        let value = Self {
            lease_epoch: read(bytes, 0),
            sequence: read(bytes, 8),
            source_epoch: read(bytes, 16),
            geometry_epoch: read(bytes, 24),
            frame_sequence: read(bytes, 32),
            keycode: u16::from_be_bytes([bytes[40], bytes[41]]),
            pressed,
        };
        value.validate()?;
        Ok(value)
    }
}

pub const KEYBOARD_REVOKE_LEN: usize = 8;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyboardRevoke {
    pub lease_epoch: u64,
}
impl KeyboardRevoke {
    pub fn encode(self) -> RemoteResult<[u8; KEYBOARD_REVOKE_LEN]> {
        if self.lease_epoch == 0 {
            return Err(invalid("zero keyboard revoke epoch").into());
        }
        Ok(self.lease_epoch.to_be_bytes())
    }
    pub fn decode(bytes: &[u8]) -> RemoteResult<Self> {
        if bytes.len() != KEYBOARD_REVOKE_LEN {
            return Err(invalid("invalid keyboard revoke length").into());
        }
        let value = Self {
            lease_epoch: read(bytes, 0),
        };
        value.encode()?;
        Ok(value)
    }
}

/// One request and one native grant per connection. Revoke before releasing the
/// returned own-held keys through the native lease, never global key-up injection.
pub(crate) struct HostKeyboardState {
    requested: bool,
    revoked: bool,
    scope: Option<DrawnScope>,
    lease_epoch: u64,
    deadline: Option<Instant>,
    last_time: Option<Instant>,
    sequence: u64,
    tokens: f64,
    held: BTreeSet<u16>,
}
impl Default for HostKeyboardState {
    fn default() -> Self {
        Self::new()
    }
}
impl HostKeyboardState {
    pub(crate) fn new() -> Self {
        Self {
            requested: false,
            revoked: false,
            scope: None,
            lease_epoch: 0,
            deadline: None,
            last_time: None,
            sequence: 0,
            tokens: BURST,
            held: BTreeSet::new(),
        }
    }
    fn fail<T>(&mut self, message: &'static str) -> RemoteResult<T> {
        self.revoked = true;
        Err(invalid(message).into())
    }
    pub(crate) fn request(
        &mut self,
        request: LeaseRequest,
        authoritative: DrawnScope,
    ) -> RemoteResult<()> {
        if self.requested || self.revoked {
            return self.fail("keyboard lease request already consumed");
        }
        self.requested = true;
        if request.validate().is_err() || authoritative != request.scope() {
            return self.fail("keyboard lease scope not drawn");
        }
        self.scope = Some(authoritative);
        Ok(())
    }
    pub(crate) fn grant(&mut self, native_epoch: u64, now: Instant) -> RemoteResult<LeaseStatus> {
        if self.revoked || self.scope.is_none() || self.lease_epoch != 0 || native_epoch == 0 {
            return self.fail("invalid native keyboard grant");
        }
        let Some(deadline) = now.checked_add(LEASE_DURATION) else {
            return self.fail("keyboard deadline overflow");
        };
        self.lease_epoch = native_epoch;
        self.deadline = Some(deadline);
        self.last_time = Some(now);
        self.status()
            .ok_or_else(|| invalid("missing keyboard grant scope").into())
    }
    pub(crate) fn check_scope(&mut self, scope: DrawnScope, now: Instant) -> RemoteResult<()> {
        if self.revoked
            || self.lease_epoch == 0
            || self.scope.is_none_or(|original| {
                original.source_epoch != scope.source_epoch
                    || original.geometry_epoch != scope.geometry_epoch
            })
            || self.deadline.is_none_or(|deadline| now >= deadline)
            || self.last_time.is_some_and(|last| now < last)
        {
            return self.fail("keyboard lease revoked, expired, or stale");
        }
        Ok(())
    }
    /// Must be called only with the host's verified native keymap whitelist.
    pub(crate) fn key(
        &mut self,
        key: Key,
        authoritative: DrawnScope,
        verified: &BTreeSet<u16>,
        now: Instant,
    ) -> RemoteResult<()> {
        self.check_scope(authoritative, now)?;
        if key.validate().is_err()
            || Some(key.scope()) != self.scope
            || key.lease_epoch != self.lease_epoch
            || self.sequence.checked_add(1) != Some(key.sequence)
            || !verified.contains(&key.keycode)
        {
            return self.fail("invalid or unverified keyboard edge");
        }
        let elapsed = now.duration_since(self.last_time.unwrap()).as_secs_f64();
        self.tokens = (self.tokens + elapsed * EDGES_PER_SECOND).min(BURST);
        self.last_time = Some(now);
        if self.tokens < 1.0 {
            return self.fail("keyboard rate exceeded");
        }
        if key.pressed {
            if self.held.len() >= 32 || !self.held.insert(key.keycode) {
                return self.fail("duplicate press or held-key limit");
            }
        } else if !self.held.remove(&key.keycode) {
            return self.fail("unpaired keyboard release");
        }
        self.tokens -= 1.0;
        self.sequence = key.sequence;
        Ok(())
    }
    /// Idempotent; failed admissions retain held keys until caller drains them.
    pub(crate) fn revoke(&mut self) -> Vec<u16> {
        self.revoked = true;
        std::mem::take(&mut self.held).into_iter().collect()
    }
    pub(crate) fn status(&self) -> Option<LeaseStatus> {
        self.scope.map(|scope| LeaseStatus {
            lease_epoch: self.lease_epoch,
            source_epoch: scope.source_epoch,
            geometry_epoch: scope.geometry_epoch,
            frame_sequence: scope.frame_sequence,
            status: if self.revoked {
                LeaseStatusKind::Revoked
            } else if self.lease_epoch != 0 {
                LeaseStatusKind::Granted
            } else {
                LeaseStatusKind::Denied
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn scope() -> DrawnScope {
        DrawnScope {
            source_epoch: 1,
            geometry_epoch: 2,
            frame_sequence: 0,
        }
    }
    fn request() -> LeaseRequest {
        LeaseRequest {
            keymap_digest: [1; 32],
            source_epoch: 1,
            geometry_epoch: 2,
            frame_sequence: 0,
        }
    }
    fn key(sequence: u64, code: u16, pressed: bool) -> Key {
        Key {
            lease_epoch: 7,
            sequence,
            source_epoch: 1,
            geometry_epoch: 2,
            frame_sequence: 0,
            keycode: code,
            pressed,
        }
    }
    fn granted(now: Instant) -> HostKeyboardState {
        let mut state = HostKeyboardState::new();
        state.request(request(), scope()).unwrap();
        state.grant(7, now).unwrap();
        state
    }
    #[test]
    fn fixed_wire_roundtrip_and_strict_lengths() {
        let request = request();
        assert_eq!(
            LeaseRequest::decode(&request.encode().unwrap()).unwrap(),
            request
        );
        let edge = key(1, 30, true);
        assert_eq!(Key::decode(&edge.encode().unwrap()).unwrap(), edge);
        let status = granted(Instant::now()).status().unwrap();
        assert_eq!(
            LeaseStatus::decode(&status.encode().unwrap()).unwrap(),
            status
        );
        for n in 0..KEY_LEN {
            assert!(Key::decode(&vec![0; n]).is_err());
        }
        assert!(Key::decode(&[0; KEY_LEN + 1]).is_err());
        let mut bytes = edge.encode().unwrap();
        bytes[42] = 2;
        assert!(Key::decode(&bytes).is_err());
        let mut bytes = status.encode().unwrap();
        bytes[32] = 4;
        assert!(LeaseStatus::decode(&bytes).is_err());
        assert!(key(0, 30, true).encode().is_err());
        assert!(key(60_001, 30, true).encode().is_err());
        assert!(key(60_000, 30, true).encode().is_ok());
    }
    #[test]
    fn duplicate_press_revokes_and_retains_only_own_held() {
        let now = Instant::now();
        let mut state = granted(now);
        let verified = BTreeSet::from([30]);
        state
            .key(key(1, 30, true), scope(), &verified, now)
            .unwrap();
        assert!(
            state
                .key(key(2, 30, true), scope(), &verified, now)
                .is_err()
        );
        assert_eq!(state.revoke(), vec![30]);
        assert!(state.revoke().is_empty());
        assert!(state.grant(8, now).is_err());
    }
    #[test]
    fn later_frame_ack_keeps_fixed_lease_frame() {
        let now = Instant::now();
        let mut state = granted(now);
        let current = DrawnScope {
            frame_sequence: 9,
            ..scope()
        };
        state.check_scope(current, now).unwrap();
        state
            .key(key(1, 30, true), current, &BTreeSet::from([30]), now)
            .unwrap();
        assert!(
            state
                .key(
                    Key {
                        frame_sequence: 9,
                        ..key(2, 30, false)
                    },
                    current,
                    &BTreeSet::from([30]),
                    now
                )
                .is_err()
        );
    }
    #[test]
    fn digest_and_revoke_are_strict() {
        assert!(
            LeaseRequest {
                keymap_digest: [0; 32],
                ..request()
            }
            .encode()
            .is_err()
        );
        let revoke = KeyboardRevoke { lease_epoch: 7 };
        assert_eq!(
            KeyboardRevoke::decode(&revoke.encode().unwrap()).unwrap(),
            revoke
        );
        assert!(KeyboardRevoke::decode(&[0; 8]).is_err());
        assert!(KeyboardRevoke::decode(&[1; 9]).is_err());
    }
    #[test]
    fn expiration_is_hard_and_request_is_once() {
        let now = Instant::now();
        let mut state = granted(now);
        assert!(state.check_scope(scope(), now + LEASE_DURATION).is_err());
        assert!(state.request(request(), scope()).is_err());
        assert!(state.grant(8, now).is_err());
    }
    #[test]
    fn bad_scope_sequence_and_whitelist_fail_closed() {
        let now = Instant::now();
        for value in [
            key(2, 30, true),
            Key {
                lease_epoch: 8,
                ..key(1, 30, true)
            },
            Key {
                frame_sequence: 1,
                ..key(1, 30, true)
            },
            key(1, 31, true),
            key(1, 30, false),
        ] {
            let mut state = granted(now);
            assert!(
                state
                    .key(value, scope(), &BTreeSet::from([30]), now)
                    .is_err()
            );
            assert_eq!(state.status().unwrap().status, LeaseStatusKind::Revoked);
        }
    }
    #[test]
    fn burst_is_bounded_even_with_paired_edges() {
        let now = Instant::now();
        let mut state = granted(now);
        let verified = BTreeSet::from([30]);
        for seq in 1..=32 {
            state
                .key(key(seq, 30, seq % 2 == 1), scope(), &verified, now)
                .unwrap();
        }
        assert!(
            state
                .key(key(33, 30, true), scope(), &verified, now)
                .is_err()
        );
    }
    #[test]
    fn scope_change_and_unverified_zero_are_rejected() {
        let now = Instant::now();
        let mut state = granted(now);
        assert!(
            state
                .check_scope(
                    DrawnScope {
                        geometry_epoch: 3,
                        ..scope()
                    },
                    now
                )
                .is_err()
        );
        assert!(key(1, 0, true).encode().is_err());
        assert!(key(1, 248, true).encode().is_err());
        let mut state = HostKeyboardState::new();
        assert!(
            state
                .request(
                    request(),
                    DrawnScope {
                        frame_sequence: 1,
                        ..scope()
                    }
                )
                .is_err()
        );
        assert!(state.request(request(), scope()).is_err());
    }
}
