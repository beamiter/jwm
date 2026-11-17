//! v27 pointer-only repair records. These never authorize input on their own.
use super::wayland_pointer_wire::{DrawnScope, MAX_SEQUENCE};
use super::{
    RemoteResult,
    pointer_repair_phase::{ActionReceipt, Scope, Token},
};
use std::io;
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid pointer repair record")
}
fn read(b: &[u8], i: usize) -> u64 {
    u64::from_be_bytes(b[i..i + 8].try_into().unwrap())
}
fn put(b: &mut [u8], i: usize, v: u64) {
    b[i..i + 8].copy_from_slice(&v.to_be_bytes());
}
fn scope(b: &[u8]) -> RemoteResult<Scope> {
    let v = Scope {
        source: read(b, 0),
        geometry: read(b, 8),
        sequence: read(b, 16),
    };
    if v.source == 0 || v.geometry == 0 {
        return Err(invalid().into());
    }
    Ok(v)
}
fn write_scope(b: &mut [u8], s: Scope) {
    put(b, 0, s.source);
    put(b, 8, s.geometry);
    put(b, 16, s.sequence);
}
impl From<DrawnScope> for Scope {
    fn from(s: DrawnScope) -> Self {
        Self {
            source: s.source_epoch,
            geometry: s.geometry_epoch,
            sequence: s.frame_sequence,
        }
    }
}
impl From<Scope> for DrawnScope {
    fn from(s: Scope) -> Self {
        Self {
            source_epoch: s.source,
            geometry_epoch: s.geometry,
            frame_sequence: s.sequence,
        }
    }
}
impl Token {
    pub(crate) fn encode(self) -> RemoteResult<[u8; 48]> {
        let mut b = [0; 48];
        put(&mut b, 0, self.lease);
        put(&mut b, 8, self.phase);
        write_scope(&mut b[16..40], self.ready);
        put(&mut b, 40, self.completed_action);
        Self::decode(&b)?;
        Ok(b)
    }
    pub(crate) fn decode(b: &[u8]) -> RemoteResult<Self> {
        if b.len() != 48 {
            return Err(invalid().into());
        }
        let t = Self {
            lease: read(b, 0),
            phase: read(b, 8),
            ready: scope(&b[16..40])?,
            completed_action: read(b, 40),
        };
        if t.lease == 0 || t.phase == 0 || t.completed_action > MAX_SEQUENCE {
            return Err(invalid().into());
        }
        Ok(t)
    }
}
impl ActionReceipt {
    pub(crate) fn encode(self) -> RemoteResult<[u8; 40]> {
        let mut b = [0; 40];
        put(&mut b, 0, self.lease);
        put(&mut b, 8, self.sequence);
        write_scope(&mut b[16..], self.consumed);
        Self::decode(&b)?;
        Ok(b)
    }
    pub(crate) fn decode(b: &[u8]) -> RemoteResult<Self> {
        if b.len() != 40 {
            return Err(invalid().into());
        }
        let r = Self {
            lease: read(b, 0),
            sequence: read(b, 8),
            consumed: scope(&b[16..])?,
        };
        if r.lease == 0 || r.sequence == 0 || r.sequence > MAX_SEQUENCE {
            return Err(invalid().into());
        }
        Ok(r)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum RepairStatusKind {
    Accepted = 1,
    Cancelled = 2,
    Ready = 3,
    /// Exact Decline was dequeued and processed; not an advisory cancellation.
    Declined = 4,
    /// Exact Request was dequeued but not accepted; never an advisory event.
    RequestCancelled = 5,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RepairStatus {
    pub kind: RepairStatusKind,
    pub token: Token,
    pub proof: Option<Scope>,
}
impl RepairStatus {
    pub(crate) fn encode(self) -> RemoteResult<[u8; 73]> {
        let mut b = [0; 73];
        b[0] = self.kind as u8;
        b[1..49].copy_from_slice(&self.token.encode()?);
        if let Some(s) = self.proof {
            write_scope(&mut b[49..], s);
        }
        Self::decode(&b)?;
        Ok(b)
    }
    pub(crate) fn decode(b: &[u8]) -> RemoteResult<Self> {
        if b.len() != 73 {
            return Err(invalid().into());
        }
        let kind = match b[0] {
            1 => RepairStatusKind::Accepted,
            2 => RepairStatusKind::Cancelled,
            3 => RepairStatusKind::Ready,
            4 => RepairStatusKind::Declined,
            5 => RepairStatusKind::RequestCancelled,
            _ => return Err(invalid().into()),
        };
        let token = Token::decode(&b[1..49])?;
        let proof = if kind == RepairStatusKind::Ready {
            let p = scope(&b[49..])?;
            if p.source != token.ready.source
                || p.geometry != token.ready.geometry
                || p.sequence <= token.ready.sequence
            {
                return Err(invalid().into());
            }
            Some(p)
        } else {
            if b[49..].iter().any(|v| *v != 0) {
                return Err(invalid().into());
            }
            None
        };
        Ok(Self { kind, token, proof })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn token() -> Token {
        Token {
            lease: 1,
            phase: 1,
            ready: Scope {
                source: 1,
                geometry: 1,
                sequence: 0,
            },
            completed_action: 0,
        }
    }
    #[test]
    fn records_roundtrip_and_zero_frame_is_valid() {
        let t = token();
        assert_eq!(Token::decode(&t.encode().unwrap()).unwrap(), t);
        let a = ActionReceipt {
            lease: 1,
            sequence: 1,
            consumed: t.ready,
        };
        assert_eq!(ActionReceipt::decode(&a.encode().unwrap()).unwrap(), a);
        for kind in [
            RepairStatusKind::Accepted,
            RepairStatusKind::Cancelled,
            RepairStatusKind::Ready,
            RepairStatusKind::Declined,
            RepairStatusKind::RequestCancelled,
        ] {
            let s = RepairStatus {
                kind,
                token: t,
                proof: (kind == RepairStatusKind::Ready).then_some(Scope {
                    sequence: 1,
                    ..t.ready
                }),
            };
            assert_eq!(RepairStatus::decode(&s.encode().unwrap()).unwrap(), s);
        }
    }
    #[test]
    fn wrong_length_reserved_scope_and_unknown_kind_fail() {
        let t = token().encode().unwrap();
        for cut in 0..48 {
            assert!(Token::decode(&t[..cut]).is_err());
        }
        let s = RepairStatus {
            kind: RepairStatusKind::Accepted,
            token: token(),
            proof: None,
        }
        .encode()
        .unwrap();
        for cut in 0..73 {
            assert!(RepairStatus::decode(&s[..cut]).is_err());
        }
        let mut bad = s;
        bad[72] = 1;
        assert!(RepairStatus::decode(&bad).is_err());
        bad = s;
        bad[0] = 6;
        assert!(RepairStatus::decode(&bad).is_err());
        assert!(
            RepairStatus {
                kind: RepairStatusKind::Ready,
                token: token(),
                proof: Some(token().ready)
            }
            .encode()
            .is_err()
        );
    }
    #[test]
    fn declined_is_distinct_from_cancelled_and_cannot_carry_proof() {
        let declined = RepairStatus {
            kind: RepairStatusKind::Declined,
            token: token(),
            proof: None,
        };
        let encoded = declined.encode().unwrap();
        assert_eq!(encoded[0], 4);
        assert_eq!(RepairStatus::decode(&encoded).unwrap(), declined);
        let mut cancelled = encoded;
        cancelled[0] = 2;
        assert_eq!(
            RepairStatus::decode(&cancelled).unwrap().kind,
            RepairStatusKind::Cancelled
        );
        for offset in 49..73 {
            let mut bad = encoded;
            bad[offset] = 1;
            assert!(RepairStatus::decode(&bad).is_err());
        }
        assert!(
            RepairStatus {
                proof: Some(Scope {
                    sequence: 1,
                    ..token().ready
                }),
                ..declined
            }
            .encode()
            .is_err()
        );
    }
    #[test]
    fn request_cancelled_is_a_separate_zero_proof_receipt() {
        let receipt = RepairStatus {
            kind: RepairStatusKind::RequestCancelled,
            token: token(),
            proof: None,
        };
        let encoded = receipt.encode().unwrap();
        assert_eq!(encoded[0], 5);
        assert_eq!(RepairStatus::decode(&encoded).unwrap(), receipt);
        for other in [RepairStatusKind::Cancelled, RepairStatusKind::Declined] {
            assert_ne!(
                receipt,
                RepairStatus {
                    kind: other,
                    ..receipt
                }
            );
        }
        for offset in 49..73 {
            let mut bad = encoded;
            bad[offset] = 1;
            assert!(RepairStatus::decode(&bad).is_err());
        }
        assert!(
            RepairStatus {
                proof: Some(Scope {
                    sequence: 1,
                    ..token().ready
                }),
                ..receipt
            }
            .encode()
            .is_err()
        );
    }
    #[test]
    fn zero_authority_and_out_of_budget_completion_fail() {
        let mut t = token();
        t.lease = 0;
        assert!(t.encode().is_err());
        t = token();
        t.phase = 0;
        assert!(t.encode().is_err());
        t = token();
        t.completed_action = MAX_SEQUENCE + 1;
        assert!(t.encode().is_err());
        assert!(
            ActionReceipt {
                lease: 1,
                sequence: 0,
                consumed: token().ready
            }
            .encode()
            .is_err()
        );
    }
}
