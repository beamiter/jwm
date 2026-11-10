//! Explicit combined authority, never two independently authorized leases.
use super::RemoteResult;
pub use super::wayland_keyboard_wire::DrawnScope;
use std::io;
pub const REQUEST_LEN: usize = 56;
pub const STATUS_LEN: usize = 33;
pub const ACTION_LEN: usize = 48;
pub const LEASE_DURATION: std::time::Duration = std::time::Duration::from_secs(30);
pub const MAX_SEQUENCE: u64 = 60_000;
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid scoped control record")
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
fn put_scope(b: &mut [u8], s: DrawnScope) -> RemoteResult<()> {
    put(b, 0, s.source_epoch);
    put(b, 8, s.geometry_epoch);
    put(b, 16, s.frame_sequence);
    scope(b)?;
    Ok(())
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlRequest {
    pub scope: DrawnScope,
    pub keymap_digest: [u8; 32],
}
impl ControlRequest {
    pub fn encode(self) -> RemoteResult<[u8; REQUEST_LEN]> {
        if self.keymap_digest == [0; 32] {
            return Err(invalid().into());
        }
        let mut b = [0; REQUEST_LEN];
        put_scope(&mut b, self.scope)?;
        b[24..].copy_from_slice(&self.keymap_digest);
        Ok(b)
    }
    pub fn decode(b: &[u8]) -> RemoteResult<Self> {
        if b.len() != REQUEST_LEN {
            return Err(invalid().into());
        }
        let s = Self {
            scope: scope(b)?,
            keymap_digest: b[24..].try_into().unwrap(),
        };
        s.encode()?;
        Ok(s)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ControlStatusKind {
    Granted = 1,
    PointerReady = 2,
    Waiting = 3,
    Denied = 4,
    Revoked = 5,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlStatus {
    pub scope: DrawnScope,
    pub lease_epoch: u64,
    pub status: ControlStatusKind,
}
impl ControlStatus {
    pub fn encode(self) -> RemoteResult<[u8; STATUS_LEN]> {
        if (self.status == ControlStatusKind::Denied) != (self.lease_epoch == 0) {
            return Err(invalid().into());
        }
        let mut b = [0; STATUS_LEN];
        put_scope(&mut b, self.scope)?;
        put(&mut b, 24, self.lease_epoch);
        b[32] = self.status as u8;
        Ok(b)
    }
    pub fn decode(b: &[u8]) -> RemoteResult<Self> {
        if b.len() != STATUS_LEN {
            return Err(invalid().into());
        }
        let status = match b[32] {
            1 => ControlStatusKind::Granted,
            2 => ControlStatusKind::PointerReady,
            3 => ControlStatusKind::Waiting,
            4 => ControlStatusKind::Denied,
            5 => ControlStatusKind::Revoked,
            _ => return Err(invalid().into()),
        };
        let s = Self {
            scope: scope(b)?,
            lease_epoch: read(b, 24),
            status,
        };
        s.encode()?;
        Ok(s)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlActionKind {
    Key { code: u16, pressed: bool },
    Motion { x: u16, y: u16 },
    ClickLeft { x: u16, y: u16 },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlAction {
    pub scope: DrawnScope,
    pub lease_epoch: u64,
    pub sequence: u64,
    pub kind: ControlActionKind,
}
impl ControlAction {
    pub fn encode(self) -> RemoteResult<[u8; ACTION_LEN]> {
        if self.lease_epoch == 0 || self.sequence == 0 || self.sequence > MAX_SEQUENCE {
            return Err(invalid().into());
        }
        let mut b = [0; ACTION_LEN];
        put_scope(&mut b, self.scope)?;
        put(&mut b, 24, self.lease_epoch);
        put(&mut b, 32, self.sequence);
        match self.kind {
            ControlActionKind::Key { code, pressed } => {
                if code == 0 || code > 247 {
                    return Err(invalid().into());
                }
                b[41..43].copy_from_slice(&code.to_be_bytes());
                b[43] = u8::from(pressed);
            }
            ControlActionKind::Motion { x, y } | ControlActionKind::ClickLeft { x, y } => {
                b[40] = if matches!(self.kind, ControlActionKind::Motion { .. }) {
                    1
                } else {
                    2
                };
                b[41..43].copy_from_slice(&x.to_be_bytes());
                b[43..45].copy_from_slice(&y.to_be_bytes());
            }
        }
        Ok(b)
    }
    pub fn decode(b: &[u8]) -> RemoteResult<Self> {
        if b.len() != ACTION_LEN {
            return Err(invalid().into());
        }
        let kind = match b[40] {
            0 if b[43] <= 1 && b[44..].iter().all(|v| *v == 0) => ControlActionKind::Key {
                code: u16::from_be_bytes([b[41], b[42]]),
                pressed: b[43] != 0,
            },
            1 | 2 if b[45..].iter().all(|v| *v == 0) => {
                let x = u16::from_be_bytes([b[41], b[42]]);
                let y = u16::from_be_bytes([b[43], b[44]]);
                if b[40] == 1 {
                    ControlActionKind::Motion { x, y }
                } else {
                    ControlActionKind::ClickLeft { x, y }
                }
            }
            _ => return Err(invalid().into()),
        };
        let s = Self {
            scope: scope(b)?,
            lease_epoch: read(b, 24),
            sequence: read(b, 32),
            kind,
        };
        s.encode()?;
        Ok(s)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlRevoke {
    pub lease_epoch: u64,
}
impl ControlRevoke {
    pub fn encode(self) -> RemoteResult<[u8; 8]> {
        if self.lease_epoch == 0 {
            return Err(invalid().into());
        }
        Ok(self.lease_epoch.to_be_bytes())
    }
    pub fn decode(b: &[u8]) -> RemoteResult<Self> {
        if b.len() != 8 {
            return Err(invalid().into());
        }
        let s = Self {
            lease_epoch: read(b, 0),
        };
        s.encode()?;
        Ok(s)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn records_are_exact_and_do_not_admit_scroll_or_noncanonical_payloads() {
        let scope = DrawnScope {
            source_epoch: 1,
            geometry_epoch: 2,
            frame_sequence: 0,
        };
        let req = ControlRequest {
            scope,
            keymap_digest: [9; 32],
        };
        assert_eq!(ControlRequest::decode(&req.encode().unwrap()).unwrap(), req);
        for kind in [
            ControlActionKind::Key {
                code: 30,
                pressed: true,
            },
            ControlActionKind::Key {
                code: 30,
                pressed: false,
            },
            ControlActionKind::Motion { x: 20, y: 30 },
            ControlActionKind::ClickLeft { x: 20, y: 30 },
        ] {
            let a = ControlAction {
                scope,
                lease_epoch: 7,
                sequence: 1,
                kind,
            };
            let b = a.encode().unwrap();
            assert_eq!(ControlAction::decode(&b).unwrap(), a);
            assert!(ControlAction::decode(&b[..47]).is_err());
            let mut bad = b;
            bad[47] = 1;
            assert!(ControlAction::decode(&bad).is_err());
            bad = b;
            bad[40] = 3;
            assert!(ControlAction::decode(&bad).is_err());
            bad = b;
            bad[32..40].fill(0);
            assert!(ControlAction::decode(&bad).is_err());
        }
        for status in [
            ControlStatusKind::Granted,
            ControlStatusKind::PointerReady,
            ControlStatusKind::Waiting,
            ControlStatusKind::Denied,
            ControlStatusKind::Revoked,
        ] {
            let s = ControlStatus {
                scope,
                lease_epoch: if status == ControlStatusKind::Denied {
                    0
                } else {
                    7
                },
                status,
            };
            assert_eq!(ControlStatus::decode(&s.encode().unwrap()).unwrap(), s);
        }
        assert!(
            ControlRequest {
                keymap_digest: [0; 32],
                ..req
            }
            .encode()
            .is_err()
        );
        assert!(ControlRevoke { lease_epoch: 0 }.encode().is_err());
    }
}
