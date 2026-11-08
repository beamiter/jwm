//! Bounded runtime clipboard restriction records and ordered dispatch policy.
//! Application v14 makes revocation explicit without pretending to erase data
//! already published by either endpoint.
use super::RemoteResult;
use super::clipboard_policy::{next_content, strictly_narrows, valid_content};
use super::clipboard_sync::{TRANSFER_CHUNK_BYTES, TransferKind, TransferOp};
use super::protocol::MessageKind;
use std::io;
use std::time::{Duration, Instant};

pub const RESTRICTION_TIMEOUT: Duration = Duration::from_secs(10);
pub const RESTRICTION_RECORD_BYTES: usize = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipboardRestriction {
    pub revision: u64,
    pub content: u8,
}

impl ClipboardRestriction {
    pub fn encode(self) -> [u8; RESTRICTION_RECORD_BYTES] {
        let mut bytes = [0; RESTRICTION_RECORD_BYTES];
        bytes[0] = 1;
        bytes[1..9].copy_from_slice(&self.revision.to_be_bytes());
        bytes[9] = self.content;
        bytes
    }

    pub fn decode(bytes: &[u8]) -> RemoteResult<Self> {
        if bytes.len() != RESTRICTION_RECORD_BYTES || bytes[0] != 1 {
            return Err(invalid("invalid clipboard restriction version or length"));
        }
        let revision = u64::from_be_bytes(bytes[1..9].try_into().expect("checked length"));
        if revision == 0 || !valid_content(bytes[9]) {
            return Err(invalid("invalid clipboard restriction revision or content"));
        }
        Ok(Self {
            revision,
            content: bytes[9],
        })
    }
}

pub fn validate_successor(
    revision: u64,
    content: u8,
    request: ClipboardRestriction,
) -> RemoteResult<()> {
    if revision.checked_add(1) != Some(request.revision)
        || !strictly_narrows(content, request.content)
    {
        return Err(invalid(
            "clipboard restriction expands consent or has an unexpected revision",
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct Pending {
    request: ClipboardRestriction,
    started: Instant,
    local: bool,
    peer: bool,
}

/// The viewer never restores its effective mask when confirmation fails.
#[derive(Debug)]
pub struct RestrictionState {
    content: u8,
    revision: u64,
    pending: Option<Pending>,
}

impl RestrictionState {
    pub fn new(content: u8) -> Self {
        assert!(valid_content(content));
        Self {
            content,
            revision: 0,
            pending: None,
        }
    }
    pub fn content(&self) -> u8 {
        self.content
    }
    pub fn pending(&self) -> Option<ClipboardRestriction> {
        self.pending.as_ref().map(|p| p.request)
    }
    pub fn begin_next(&mut self, now: Instant) -> RemoteResult<Option<ClipboardRestriction>> {
        if self.pending.is_some() {
            return Ok(None);
        }
        let Some(content) = next_content(self.content) else {
            return Ok(None);
        };
        let revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid("clipboard restriction revision exhausted"))?;
        let request = ClipboardRestriction { revision, content };
        self.accept_request(request, now)?;
        Ok(Some(request))
    }
    pub fn accept_request(
        &mut self,
        request: ClipboardRestriction,
        now: Instant,
    ) -> RemoteResult<()> {
        if self.pending.is_some() {
            return Err(invalid("another clipboard restriction is pending"));
        }
        validate_successor(self.revision, self.content, request)?;
        self.content = request.content;
        self.revision = request.revision;
        self.pending = Some(Pending {
            request,
            started: now,
            local: false,
            peer: false,
        });
        Ok(())
    }
    pub fn confirm_local(&mut self, revision: u64) -> RemoteResult<()> {
        let pending = self
            .pending
            .as_mut()
            .filter(|p| p.request.revision == revision && !p.local)
            .ok_or_else(|| invalid("unexpected local clipboard restriction barrier"))?;
        pending.local = true;
        Ok(())
    }
    pub fn local_complete(&mut self, request: ClipboardRestriction) -> RemoteResult<()> {
        if self.pending() != Some(request) {
            return Err(invalid(
                "local clipboard restriction barrier does not match",
            ));
        }
        self.confirm_local(request.revision)
    }
    pub fn acknowledge(&mut self, request: ClipboardRestriction) -> RemoteResult<()> {
        let pending = self
            .pending
            .as_mut()
            .filter(|p| p.request == request && !p.peer)
            .ok_or_else(|| {
                invalid(
                    "unsolicited, duplicate or mismatched clipboard restriction acknowledgement",
                )
            })?;
        pending.peer = true;
        Ok(())
    }
    pub fn take_confirmed(&mut self) -> Option<ClipboardRestriction> {
        if self.pending.as_ref().is_some_and(|p| p.local && p.peer) {
            return self.pending.take().map(|p| p.request);
        }
        None
    }
    pub fn check_timeout(&self, now: Instant) -> RemoteResult<()> {
        if self
            .pending
            .as_ref()
            .is_some_and(|p| now.saturating_duration_since(p.started) >= RESTRICTION_TIMEOUT)
        {
            return Err(io::Error::new(io::ErrorKind::TimedOut,
                "clipboard restriction confirmation timed out; ending session without restoring sharing").into());
        }
        Ok(())
    }
}

/// Used only by the final single writer, not by producer-side snapshots.
/// A skipped Data record carries its original logical byte count, preserving
/// session-wide flow credit without sending the revoked contents. This state
/// has one transfer slot because each endpoint's clipboard worker emits only
/// one transfer at a time, with ordered Begin/Cancel boundaries.
#[derive(Default, Debug)]
pub struct OutboundGate {
    transfer: Option<(u32, u8)>,
}

impl OutboundGate {
    pub fn filter(
        &mut self,
        kind: MessageKind,
        payload: &[u8],
        content: u8,
    ) -> RemoteResult<Option<Vec<u8>>> {
        if !valid_content(content) {
            return Err(invalid("invalid effective clipboard policy"));
        }
        if kind == MessageKind::Clipboard {
            return Ok((content & 1 != 0).then(|| payload.to_vec()));
        }
        if kind != MessageKind::ClipboardTransfer {
            return Ok(Some(payload.to_vec()));
        }
        let op = TransferOp::decode(payload)?;
        let op = match op {
            TransferOp::Begin { id, kind, .. } => {
                let bit = match kind {
                    TransferKind::Image => 2,
                    TransferKind::Files => 4,
                };
                self.transfer = Some((id, bit));
                if content & bit == 0 {
                    TransferOp::Cancel { id }
                } else {
                    op
                }
            }
            TransferOp::Data { id, ref bytes } => {
                let (_, bit) = self
                    .transfer
                    .filter(|(current, _)| *current == id)
                    .ok_or_else(|| invalid("outbound clipboard data has no matching Begin"))?;
                if content & bit == 0 {
                    TransferOp::Skip {
                        id,
                        bytes: bytes.len() as u64,
                    }
                } else {
                    op
                }
            }
            TransferOp::End { id } => {
                let (_, bit) = self
                    .transfer
                    .filter(|(current, _)| *current == id)
                    .ok_or_else(|| invalid("outbound clipboard end has no matching Begin"))?;
                if content & bit == 0 {
                    TransferOp::Cancel { id }
                } else {
                    op
                }
            }
            TransferOp::Skip { bytes, .. } if bytes == 0 || bytes > TRANSFER_CHUNK_BYTES as u64 => {
                return Err(invalid("outbound clipboard skip exceeds one chunk"));
            }
            _ => op,
        };
        Ok(Some(op.encode()))
    }
}

fn invalid(message: &str) -> super::RemoteError {
    io::Error::new(io::ErrorKind::InvalidData, message).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restriction_codec_is_bounded_and_versioned() {
        for content in [0, 1, 3, 7] {
            let r = ClipboardRestriction {
                revision: 42,
                content,
            };
            assert_eq!(ClipboardRestriction::decode(&r.encode()).unwrap(), r);
        }
        let valid = ClipboardRestriction {
            revision: 1,
            content: 3,
        }
        .encode();
        for len in 0..10 {
            assert!(ClipboardRestriction::decode(&valid[..len]).is_err());
        }
        let mut long = valid.to_vec();
        long.push(0);
        assert!(ClipboardRestriction::decode(&long).is_err());
        for (at, value) in [(0, 0), (0, 2), (8, 0), (9, 2), (9, 255)] {
            let mut bad = valid;
            bad[at] = value;
            assert!(ClipboardRestriction::decode(&bad).is_err());
        }
    }
    #[test]
    fn runtime_content_never_expands_and_off_is_permanent() {
        for initial in [0, 1, 3, 7] {
            let mut state = RestrictionState::new(initial);
            let mut previous = initial;
            while let Some(request) = state.begin_next(Instant::now()).unwrap() {
                assert_eq!(request.content & !previous, 0);
                assert!(request.content < previous);
                assert!(state.begin_next(Instant::now()).unwrap().is_none());
                state.confirm_local(request.revision).unwrap();
                assert!(state.take_confirmed().is_none());
                state.acknowledge(request).unwrap();
                assert_eq!(state.take_confirmed(), Some(request));
                assert!(state.acknowledge(request).is_err());
                previous = request.content;
            }
            assert_eq!(state.content(), 0);
            assert!(
                state
                    .accept_request(
                        ClipboardRestriction {
                            revision: 4,
                            content: 7
                        },
                        Instant::now()
                    )
                    .is_err()
            );
        }
    }
    #[test]
    fn early_ack_needs_local_barrier_and_duplicates_fail_closed() {
        let mut state = RestrictionState::new(7);
        let request = state.begin_next(Instant::now()).unwrap().unwrap();
        state.acknowledge(request).unwrap();
        assert!(state.take_confirmed().is_none());
        assert!(state.acknowledge(request).is_err());
        assert!(state.confirm_local(request.revision + 1).is_err());
        state.confirm_local(request.revision).unwrap();
        assert_eq!(state.take_confirmed(), Some(request));
        assert!(state.confirm_local(request.revision).is_err());
    }
    #[test]
    fn invalid_successors_and_timeout_keep_reduced_mask() {
        for request in [
            ClipboardRestriction {
                revision: 0,
                content: 1,
            },
            ClipboardRestriction {
                revision: 2,
                content: 1,
            },
            ClipboardRestriction {
                revision: 1,
                content: 7,
            },
            ClipboardRestriction {
                revision: 1,
                content: 2,
            },
        ] {
            assert!(validate_successor(0, 3, request).is_err());
        }
        let now = Instant::now();
        let mut state = RestrictionState::new(7);
        state.begin_next(now).unwrap();
        assert!(
            state
                .check_timeout(now + RESTRICTION_TIMEOUT - Duration::from_nanos(1))
                .is_ok()
        );
        assert!(state.check_timeout(now + RESTRICTION_TIMEOUT).is_err());
        assert_eq!(state.content(), 3);
    }
    #[test]
    fn dispatch_rechecks_policy_after_begin_preserving_credit_controls() {
        let mut gate = OutboundGate::default();
        let begin = TransferOp::Begin {
            id: 1,
            kind: TransferKind::Files,
            length: 3,
            entries: 1,
            items: 1,
        };
        assert_eq!(
            gate.filter(MessageKind::ClipboardTransfer, &begin.encode(), 7)
                .unwrap()
                .unwrap(),
            begin.encode()
        );
        let data = TransferOp::Data {
            id: 1,
            bytes: vec![10, 20, 30],
        };
        assert_eq!(
            TransferOp::decode(
                &gate
                    .filter(MessageKind::ClipboardTransfer, &data.encode(), 3)
                    .unwrap()
                    .unwrap()
            )
            .unwrap(),
            TransferOp::Skip { id: 1, bytes: 3 }
        );
        let end = TransferOp::End { id: 1 };
        assert_eq!(
            TransferOp::decode(
                &gate
                    .filter(MessageKind::ClipboardTransfer, &end.encode(), 3)
                    .unwrap()
                    .unwrap()
            )
            .unwrap(),
            TransferOp::Cancel { id: 1 }
        );
        for op in [
            TransferOp::Ack { id: 1, consumed: 3 },
            TransferOp::Refuse { id: 1 },
            TransferOp::Cancel { id: 1 },
        ] {
            assert_eq!(
                gate.filter(MessageKind::ClipboardTransfer, &op.encode(), 0)
                    .unwrap()
                    .unwrap(),
                op.encode()
            );
        }
        assert!(
            gate.filter(MessageKind::Clipboard, b"private", 0)
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn transfer_classification_survives_cancel_and_late_old_data_fails_closed() {
        let mut gate = OutboundGate::default();
        let begin = |id, kind| TransferOp::Begin {
            id,
            kind,
            length: 3,
            entries: 1,
            items: 1,
        };
        gate.filter(
            MessageKind::ClipboardTransfer,
            &begin(1, TransferKind::Files).encode(),
            3,
        )
        .unwrap();
        gate.filter(
            MessageKind::ClipboardTransfer,
            &TransferOp::Cancel { id: 1 }.encode(),
            3,
        )
        .unwrap();
        let data = TransferOp::Data {
            id: 1,
            bytes: vec![9, 8, 7],
        };
        let skipped = gate
            .filter(MessageKind::ClipboardTransfer, &data.encode(), 3)
            .unwrap()
            .unwrap();
        assert_eq!(
            TransferOp::decode(&skipped).unwrap(),
            TransferOp::Skip { id: 1, bytes: 3 }
        );
        assert_eq!(
            gate.filter(MessageKind::ClipboardTransfer, &skipped, 0)
                .unwrap()
                .unwrap(),
            skipped
        );
        gate.filter(
            MessageKind::ClipboardTransfer,
            &begin(2, TransferKind::Image).encode(),
            3,
        )
        .unwrap();
        // The sole clipboard worker orders all old Data before a newer Begin.
        // Reject a broken producer rather than relabel old file bytes as image.
        assert!(
            gate.filter(MessageKind::ClipboardTransfer, &data.encode(), 3)
                .is_err()
        );
        assert!(
            gate.filter(
                MessageKind::ClipboardTransfer,
                &TransferOp::End { id: 1 }.encode(),
                3
            )
            .is_err()
        );
    }

    #[test]
    fn already_queued_begin_is_cancelled_and_later_allowed_transfer_survives() {
        let mut gate = OutboundGate::default();
        for (id, kind, allowed) in [
            (1, TransferKind::Files, false),
            (2, TransferKind::Image, true),
        ] {
            let begin = TransferOp::Begin {
                id,
                kind,
                length: 1,
                entries: 1,
                items: 1,
            };
            let wire = gate
                .filter(MessageKind::ClipboardTransfer, &begin.encode(), 3)
                .unwrap()
                .unwrap();
            assert_eq!(
                TransferOp::decode(&wire).unwrap(),
                if allowed {
                    begin
                } else {
                    TransferOp::Cancel { id }
                }
            );
            let data = TransferOp::Data { id, bytes: vec![1] };
            let wire = gate
                .filter(MessageKind::ClipboardTransfer, &data.encode(), 3)
                .unwrap()
                .unwrap();
            assert_eq!(
                TransferOp::decode(&wire).unwrap(),
                if allowed {
                    data
                } else {
                    TransferOp::Skip { id, bytes: 1 }
                }
            );
        }
    }
}
