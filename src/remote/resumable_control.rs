//! Version-one resumable-control primitives, separate from one-shot authority.
//!
//! This module does not grant compositor focus or implement a transport. The
//! authenticated connection owns one session; reconnect must not reuse it. The
//! caller must terminate transport and release native input on every error or
//! terminal transition. `finish_pause` may only be called from the native
//! release completion path, never from a command-enqueue acknowledgement.
use std::io;
use std::time::{Duration, Instant};

pub const VERSION: u16 = 1;
pub const RECORD_LEN: usize = 56;
pub const ACTION_PAYLOAD_LEN: usize = 48;
pub const ACTION_ENVELOPE_LEN: usize = RECORD_LEN + ACTION_PAYLOAD_LEN;
pub const MAX_SEQUENCE: u64 = 60_000;
pub const MAX_LEASE: Duration = Duration::from_secs(30);
pub const TRANSITION_TIMEOUT: Duration = Duration::from_secs(2);

fn invalid(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

/// Local configuration evidence. Never deserialize this from a peer record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalOptIn {
    Disabled,
    Explicit,
}

/// A peer must explicitly advertise this version on its authenticated channel.
/// Capability absence, an old version or local refusal cannot produce this token.
#[derive(Debug)]
pub struct Negotiated(());
impl Negotiated {
    pub fn new(local: LocalOptIn, peer_opted_in_version: Option<u16>) -> io::Result<Self> {
        if local != LocalOptIn::Explicit || peer_opted_in_version != Some(VERSION) {
            return Err(invalid(
                "resumable control requires both explicit version-one opt-ins",
            ));
        }
        Ok(Self(()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Binding {
    pub session: u64,
    pub source_epoch: u64,
    pub geometry_epoch: u64,
}
impl Binding {
    fn validate(self) -> io::Result<()> {
        if self.session == 0 || self.source_epoch == 0 || self.geometry_epoch == 0 {
            return Err(invalid("zero resumable control binding"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Resume = 1,
    Resumed = 2,
    Pause = 3,
    Paused = 4,
    Revoke = 5,
    Action = 6,
}

/// A lifecycle record. Input payloads require their own authenticated envelope
/// carrying `InputStamp`; this record alone never forwards an input action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    pub kind: Kind,
    pub binding: Binding,
    pub generation: u64,
    pub sequence: u64,
    pub proof: u64,
}
impl Record {
    pub fn encode(self) -> io::Result<[u8; RECORD_LEN]> {
        self.binding.validate()?;
        if self.generation == 0
            || self.generation > MAX_SEQUENCE
            || self.sequence == 0
            || self.sequence > MAX_SEQUENCE
            || (matches!(self.kind, Kind::Resume | Kind::Resumed) != (self.proof != 0))
        {
            return Err(invalid("invalid resumable control record fields"));
        }
        let mut bytes = [0; RECORD_LEN];
        bytes[..2].copy_from_slice(&VERSION.to_be_bytes());
        bytes[2] = self.kind as u8;
        for (slot, value) in bytes[8..].chunks_exact_mut(8).zip([
            self.binding.session,
            self.binding.source_epoch,
            self.binding.geometry_epoch,
            self.generation,
            self.sequence,
            self.proof,
        ]) {
            slot.copy_from_slice(&value.to_be_bytes());
        }
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() != RECORD_LEN
            || u16::from_be_bytes([bytes[0], bytes[1]]) != VERSION
            || bytes[3..8] != [0; 5]
        {
            return Err(invalid("invalid resumable control version or length"));
        }
        let read = |offset| u64::from_be_bytes(bytes[offset..offset + 8].try_into().unwrap());
        let record = Self {
            kind: match bytes[2] {
                1 => Kind::Resume,
                2 => Kind::Resumed,
                3 => Kind::Pause,
                4 => Kind::Paused,
                5 => Kind::Revoke,
                6 => Kind::Action,
                _ => return Err(invalid("unknown resumable control record kind")),
            },
            binding: Binding {
                session: read(8),
                source_epoch: read(16),
                geometry_epoch: read(24),
            },
            generation: read(32),
            sequence: read(40),
            proof: read(48),
        };
        record.encode()?;
        Ok(record)
    }

    fn acknowledgement(self) -> Self {
        Self {
            kind: match self.kind {
                Kind::Resume => Kind::Resumed,
                Kind::Pause => Kind::Paused,
                _ => unreachable!("only accepted lifecycle requests are acknowledged"),
            },
            ..self
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputStamp {
    pub binding: Binding,
    pub generation: u64,
    pub sequence: u64,
}

/// Generation-stamped canonical legacy `ControlAction`. All scope/lease/order
/// fields must agree in both layers; an old queued action cannot be re-labelled
/// with a new generation or sequence without failing the host session gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActionEnvelope {
    stamp: InputStamp,
    payload: [u8; ACTION_PAYLOAD_LEN],
}
impl ActionEnvelope {
    pub fn new(stamp: InputStamp, payload: &[u8]) -> io::Result<Self> {
        if payload.len() != ACTION_PAYLOAD_LEN {
            return Err(invalid("invalid resumable control action length"));
        }
        let read = |offset| u64::from_be_bytes(payload[offset..offset + 8].try_into().unwrap());
        let header = Record {
            kind: Kind::Action,
            binding: stamp.binding,
            generation: stamp.generation,
            sequence: stamp.sequence,
            proof: 0,
        };
        header.encode()?;
        if read(0) != stamp.binding.source_epoch
            || read(8) != stamp.binding.geometry_epoch
            || read(24) != stamp.binding.session
            || read(32) != stamp.sequence
        {
            return Err(invalid("resumable action inner and outer binding differ"));
        }
        // Exactly the canonical 48-byte ControlAction layout; no opaque or
        // trailing payload can bypass validation before session admission.
        let canonical = match payload[40] {
            0 => {
                let code = u16::from_be_bytes([payload[41], payload[42]]);
                (1..=247).contains(&code)
                    && payload[43] <= 1
                    && payload[44..].iter().all(|byte| *byte == 0)
            }
            1..=6 => payload[45..].iter().all(|byte| *byte == 0),
            _ => false,
        };
        if !canonical {
            return Err(invalid("noncanonical resumable control action"));
        }
        Ok(Self {
            stamp,
            payload: payload.try_into().unwrap(),
        })
    }
    pub fn stamp(self) -> InputStamp {
        self.stamp
    }
    pub fn control_action_bytes(&self) -> &[u8; ACTION_PAYLOAD_LEN] {
        &self.payload
    }
    pub fn encode(self) -> [u8; ACTION_ENVELOPE_LEN] {
        let mut bytes = [0; ACTION_ENVELOPE_LEN];
        let header = Record {
            kind: Kind::Action,
            binding: self.stamp.binding,
            generation: self.stamp.generation,
            sequence: self.stamp.sequence,
            proof: 0,
        };
        bytes[..RECORD_LEN].copy_from_slice(&header.encode().expect("validated action envelope"));
        bytes[RECORD_LEN..].copy_from_slice(&self.payload);
        bytes
    }
    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() != ACTION_ENVELOPE_LEN {
            return Err(invalid("invalid resumable action envelope length"));
        }
        let record = Record::decode(&bytes[..RECORD_LEN])?;
        if record.kind != Kind::Action {
            return Err(invalid("not a resumable action envelope"));
        }
        Self::new(
            InputStamp {
                binding: record.binding,
                generation: record.generation,
                sequence: record.sequence,
            },
            &bytes[RECORD_LEN..],
        )
    }
}

/// One transport message kind, two exact canonical layouts. No trailing bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Message {
    Lifecycle(Record),
    Action(ActionEnvelope),
}
impl Message {
    pub fn encode(self) -> io::Result<Vec<u8>> {
        match self {
            Self::Lifecycle(record) if record.kind != Kind::Action => Ok(record.encode()?.to_vec()),
            Self::Lifecycle(_) => Err(invalid("action requires full envelope")),
            Self::Action(action) => Ok(action.encode().to_vec()),
        }
    }
    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        match bytes.len() {
            RECORD_LEN => {
                let record = Record::decode(bytes)?;
                if record.kind == Kind::Action {
                    return Err(invalid("action payload missing"));
                }
                Ok(Self::Lifecycle(record))
            }
            ACTION_ENVELOPE_LEN => Ok(Self::Action(ActionEnvelope::decode(bytes)?)),
            _ => Err(invalid("invalid resumable control message length")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Paused,
    Active,
    Releasing,
    Resuming,
    Terminal,
}

/// Unforgeable pause completion handle, bound to the accepted request. It does
/// not mean release has happened. Native release completion must consume it.
#[derive(Debug)]
pub struct ReleaseTicket(Record);

#[derive(Debug)]
pub struct HostSession {
    binding: Binding,
    deadline: Instant,
    last_now: Instant,
    phase: Phase,
    generation: u64,
    sequence: u64,
    last_proof: u64,
    release: Option<(Record, Instant)>,
}
impl HostSession {
    /// The deadline comes from the original authorization, not from the peer or
    /// a resume request. This constructor is called once per connection grant.
    pub fn new(
        _permission: Negotiated,
        binding: Binding,
        now: Instant,
        deadline: Instant,
    ) -> io::Result<Self> {
        binding.validate()?;
        if deadline <= now || deadline.duration_since(now) > MAX_LEASE {
            return Err(invalid("invalid absolute resumable control deadline"));
        }
        Ok(Self {
            binding,
            deadline,
            last_now: now,
            phase: Phase::Paused,
            generation: 0,
            sequence: 0,
            last_proof: 0,
            release: None,
        })
    }

    /// Construct only after the original native Arm completed successfully.
    /// This preserves native v4's initial active generation one / sequence zero;
    /// it is not a second grant or a reauthorization constructor.
    pub fn after_native_arm(
        permission: Negotiated,
        binding: Binding,
        now: Instant,
        deadline: Instant,
        initial_proof: u64,
    ) -> io::Result<Self> {
        // The first genuinely presented frame may have sequence zero. Native
        // Arm has already validated its actual proof; zero is not "missing".
        // Later Resume records still require a nonzero, strictly newer proof.
        let mut session = Self::new(permission, binding, now, deadline)?;
        session.phase = Phase::Active;
        session.generation = 1;
        session.last_proof = initial_proof;
        Ok(session)
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
    /// Invalidate authority first; callers then release native input and close
    /// transport. No subsequent record can reverse this state.
    pub fn revoke(&mut self) {
        self.phase = Phase::Terminal;
        self.release = None;
    }
    fn fail<T>(&mut self, reason: &'static str) -> io::Result<T> {
        self.revoke();
        Err(invalid(reason))
    }
    pub fn check(&mut self, now: Instant) -> io::Result<()> {
        if self.phase == Phase::Terminal {
            return Err(invalid("resumable control is terminal"));
        }
        if now < self.last_now || now >= self.deadline {
            return self.fail("resumable control absolute deadline or clock invalid");
        }
        self.last_now = now;
        if self
            .release
            .is_some_and(|(_, started)| now.duration_since(started) >= TRANSITION_TIMEOUT)
        {
            return self.fail("resumable control release acknowledgement timed out");
        }
        Ok(())
    }
    fn ordered(&mut self, binding: Binding, sequence: u64, now: Instant) -> io::Result<()> {
        self.check(now)?;
        if binding != self.binding || sequence != self.sequence + 1 || sequence > MAX_SEQUENCE {
            return self.fail("resumable control binding or sequence changed");
        }
        Ok(())
    }

    /// `activate` must verify fresh actual presentation, clean native input and
    /// the unchanged local surface/seat/keymap binding before applying resume.
    /// It receives only the proof ID, not authority to mint a new lease.
    pub fn resume(
        &mut self,
        request: Record,
        now: Instant,
        activate: impl FnOnce(u64) -> io::Result<()>,
    ) -> io::Result<Record> {
        self.ordered(request.binding, request.sequence, now)?;
        if request.encode().is_err()
            || request.kind != Kind::Resume
            || self.phase != Phase::Paused
            || request.generation != self.generation + 1
            || request.proof <= self.last_proof
        {
            return self.fail("invalid resumable control resume");
        }
        if let Err(error) = activate(request.proof) {
            self.revoke();
            return Err(error);
        }
        // Native activation may itself take time. Never emit an ACK or regain
        // authority after the original deadline while an adapter was blocked.
        if Instant::now() >= self.deadline {
            return self.fail("native activation exceeded original absolute deadline");
        }
        self.generation = request.generation;
        self.sequence = request.sequence;
        self.last_proof = request.proof;
        self.phase = Phase::Active;
        Ok(request.acknowledgement())
    }

    /// Immediately stops accepting input. No ACK exists until `finish_pause`.
    pub fn begin_pause(&mut self, request: Record, now: Instant) -> io::Result<ReleaseTicket> {
        self.ordered(request.binding, request.sequence, now)?;
        if request.encode().is_err()
            || request.kind != Kind::Pause
            || self.phase != Phase::Active
            || request.generation != self.generation
        {
            return self.fail("invalid resumable control pause");
        }
        self.sequence = request.sequence;
        self.phase = Phase::Releasing;
        self.release = Some((request, now));
        Ok(ReleaseTicket(request))
    }

    /// Completion must represent *applied* native releases, not an enqueue.
    /// An adapter can move the ticket across its dispatch boundary, then call
    /// this with its release/fence result. Any failure is terminal, not paused.
    pub fn finish_pause(
        &mut self,
        ticket: ReleaseTicket,
        now: Instant,
        applied_release: io::Result<()>,
    ) -> io::Result<Record> {
        self.check(now)?;
        if self.phase != Phase::Releasing
            || self.release.map(|(record, _)| record) != Some(ticket.0)
        {
            return self.fail("stale resumable control release completion");
        }
        if let Err(error) = applied_release {
            self.revoke();
            return Err(error);
        }
        self.release = None;
        self.phase = Phase::Paused;
        Ok(ticket.0.acknowledgement())
    }

    /// Validate the authenticated envelope before forwarding its input payload.
    /// Generation and sequence cannot be omitted or reset across pauses.
    pub fn accept_action(&mut self, action: &ActionEnvelope, now: Instant) -> io::Result<()> {
        self.accept_input(action.stamp(), now)
    }

    pub fn accept_input(&mut self, stamp: InputStamp, now: Instant) -> io::Result<()> {
        self.ordered(stamp.binding, stamp.sequence, now)?;
        if self.phase != Phase::Active || stamp.generation != self.generation {
            return self.fail("input outside active resumable control generation");
        }
        self.sequence = stamp.sequence;
        Ok(())
    }
}

/// Client-side ACK gate. Sending resume never enables input; only an exact
/// authenticated Resumed acknowledgement does. Sending pause disables input
/// immediately, and a fast re-entry cannot overtake its Paused acknowledgement.
#[derive(Debug)]
pub struct ClientSession {
    binding: Binding,
    deadline: Instant,
    last_now: Instant,
    phase: Phase,
    generation: u64,
    sequence: u64,
    last_proof: u64,
    pending: Option<(Record, Instant)>,
}
impl ClientSession {
    pub fn after_granted(
        permission: Negotiated,
        binding: Binding,
        now: Instant,
        deadline: Instant,
        initial_proof: u64,
    ) -> io::Result<Self> {
        Self::after_native_arm(permission, binding, now, deadline, initial_proof)
    }

    pub fn after_native_arm(
        permission: Negotiated,
        binding: Binding,
        now: Instant,
        deadline: Instant,
        initial_proof: u64,
    ) -> io::Result<Self> {
        let host =
            HostSession::after_native_arm(permission, binding, now, deadline, initial_proof)?;
        Ok(Self {
            binding,
            deadline,
            last_now: now,
            phase: Phase::Active,
            generation: host.generation,
            sequence: 0,
            last_proof: initial_proof,
            pending: None,
        })
    }
    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
    pub fn revoke(&mut self) {
        self.phase = Phase::Terminal;
        self.pending = None;
    }
    fn fail<T>(&mut self, reason: &'static str) -> io::Result<T> {
        self.revoke();
        Err(invalid(reason))
    }
    pub fn check(&mut self, now: Instant) -> io::Result<()> {
        if self.phase == Phase::Terminal {
            return Err(invalid("resumable client is terminal"));
        }
        if now < self.last_now || now >= self.deadline {
            return self.fail("resumable client absolute deadline or clock invalid");
        }
        self.last_now = now;
        if self
            .pending
            .is_some_and(|(_, started)| now.duration_since(started) >= TRANSITION_TIMEOUT)
        {
            return self.fail("resumable client acknowledgement timed out");
        }
        Ok(())
    }
    fn next_sequence(&mut self) -> io::Result<u64> {
        if self.sequence >= MAX_SEQUENCE {
            return self.fail("resumable client sequence exhausted");
        }
        self.sequence += 1;
        Ok(self.sequence)
    }
    pub fn next_input(&mut self, now: Instant) -> io::Result<InputStamp> {
        self.check(now)?;
        if self.phase != Phase::Active {
            return self.fail("resumable client input is not active");
        }
        let sequence = self.next_sequence()?;
        Ok(InputStamp {
            binding: self.binding,
            generation: self.generation,
            sequence,
        })
    }
    /// Wrap the existing action without changing its native sequence. A caller
    /// must not enqueue the returned envelope until all ordinary action checks
    /// also pass. Malformed or already-used inner sequence is terminal.
    pub fn wrap_action(&mut self, payload: &[u8], now: Instant) -> io::Result<ActionEnvelope> {
        self.check(now)?;
        if self.phase != Phase::Active || self.sequence >= MAX_SEQUENCE {
            return self.fail("resumable client action is not active or sequence exhausted");
        }
        let stamp = InputStamp {
            binding: self.binding,
            generation: self.generation,
            sequence: self.sequence + 1,
        };
        let action = match ActionEnvelope::new(stamp, payload) {
            Ok(action) => action,
            Err(error) => {
                self.revoke();
                return Err(error);
            }
        };
        self.sequence = stamp.sequence;
        Ok(action)
    }
    pub fn begin_pause(&mut self, now: Instant) -> io::Result<Record> {
        self.request_pause(now)
    }
    pub fn begin_resume(
        &mut self,
        proof: u64,
        now: Instant,
        validate_local: impl FnOnce(u64) -> io::Result<()>,
    ) -> io::Result<Record> {
        self.request_resume(proof, now, validate_local)
    }
    pub fn request_pause(&mut self, now: Instant) -> io::Result<Record> {
        self.check(now)?;
        if self.phase != Phase::Active {
            return self.fail("resumable client pause is not active");
        }
        let request = Record {
            kind: Kind::Pause,
            binding: self.binding,
            generation: self.generation,
            sequence: self.next_sequence()?,
            proof: 0,
        };
        self.phase = Phase::Releasing;
        self.pending = Some((request, now));
        Ok(request)
    }
    /// `validate_local` checks actual same-connection presentation, clean local
    /// focus/input and unchanged surface/seat/keymap before emitting a request.
    pub fn request_resume(
        &mut self,
        proof: u64,
        now: Instant,
        validate_local: impl FnOnce(u64) -> io::Result<()>,
    ) -> io::Result<Record> {
        self.check(now)?;
        if self.phase != Phase::Paused
            || proof <= self.last_proof
            || self.generation >= MAX_SEQUENCE
        {
            return self.fail("invalid resumable client resume");
        }
        if let Err(error) = validate_local(proof) {
            self.revoke();
            return Err(error);
        }
        if Instant::now() >= self.deadline {
            return self.fail("local resume validation exceeded deadline");
        }
        let request = Record {
            kind: Kind::Resume,
            binding: self.binding,
            generation: self.generation + 1,
            sequence: self.next_sequence()?,
            proof,
        };
        self.phase = Phase::Resuming;
        self.pending = Some((request, now));
        Ok(request)
    }
    pub fn receive_ack(&mut self, acknowledgement: Record, now: Instant) -> io::Result<()> {
        self.check(now)?;
        let Some((request, _)) = self.pending else {
            return self.fail("unsolicited resumable client acknowledgement");
        };
        if acknowledgement.encode().is_err() || acknowledgement != request.acknowledgement() {
            return self.fail("mismatched resumable client acknowledgement");
        }
        match (self.phase, acknowledgement.kind) {
            (Phase::Releasing, Kind::Paused) => self.phase = Phase::Paused,
            (Phase::Resuming, Kind::Resumed) => {
                self.phase = Phase::Active;
                self.generation = request.generation;
                self.last_proof = request.proof;
            }
            _ => return self.fail("invalid resumable client acknowledgement phase"),
        }
        self.pending = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn binding() -> Binding {
        Binding {
            session: 7,
            source_epoch: 11,
            geometry_epoch: 13,
        }
    }
    fn session(now: Instant) -> HostSession {
        HostSession::new(
            Negotiated::new(LocalOptIn::Explicit, Some(VERSION)).unwrap(),
            binding(),
            now,
            now + MAX_LEASE,
        )
        .unwrap()
    }
    fn record(kind: Kind, generation: u64, sequence: u64, proof: u64) -> Record {
        Record {
            kind,
            binding: binding(),
            generation,
            sequence,
            proof,
        }
    }
    #[test]
    fn both_opt_ins_and_exact_version_are_required() {
        for local in [LocalOptIn::Disabled, LocalOptIn::Explicit] {
            for peer in [None, Some(0), Some(1), Some(2)] {
                assert_eq!(
                    Negotiated::new(local, peer).is_ok(),
                    local == LocalOptIn::Explicit && peer == Some(1)
                );
            }
        }
    }
    #[test]
    fn wire_is_exact_versioned_and_canonical() {
        for kind in [
            Kind::Resume,
            Kind::Resumed,
            Kind::Pause,
            Kind::Paused,
            Kind::Revoke,
        ] {
            let r = record(
                kind,
                1,
                1,
                if matches!(kind, Kind::Resume | Kind::Resumed) {
                    20
                } else {
                    0
                },
            );
            let bytes = r.encode().unwrap();
            assert_eq!(Record::decode(&bytes).unwrap(), r);
            for len in 0..RECORD_LEN {
                assert!(Record::decode(&bytes[..len]).is_err());
            }
            let mut too_long = bytes.to_vec();
            too_long.push(0);
            assert!(Record::decode(&too_long).is_err());
            for index in 3..8 {
                let mut bad = bytes;
                bad[index] = 1;
                assert!(Record::decode(&bad).is_err());
            }
            let mut bad = bytes;
            bad[1] = 2;
            assert!(Record::decode(&bad).is_err());
            let mut bad = bytes;
            bad[2] = 0;
            assert!(Record::decode(&bad).is_err());
            for offset in [8, 16, 24, 32, 40] {
                let mut bad = bytes;
                bad[offset..offset + 8].fill(0);
                assert!(Record::decode(&bad).is_err());
            }
        }
    }
    #[test]
    fn release_ack_waits_for_applied_native_release_and_never_extends_deadline() {
        let now = Instant::now();
        let mut s = session(now);
        let original = s.deadline();
        let mut native_held = false;
        assert_eq!(
            s.resume(record(Kind::Resume, 1, 1, 20), now, |_| Ok(()))
                .unwrap()
                .kind,
            Kind::Resumed
        );
        s.accept_input(
            InputStamp {
                binding: binding(),
                generation: 1,
                sequence: 2,
            },
            now,
        )
        .unwrap();
        native_held = !native_held;
        let ticket = s
            .begin_pause(record(Kind::Pause, 1, 3, 0), now + Duration::from_secs(1))
            .unwrap();
        assert_eq!(s.phase(), Phase::Releasing);
        assert!(native_held);
        // The queued request is not an ACK. The native dispatcher now applies
        // release and verifies its recipient/fence before returning completion.
        native_held = false;
        let ack = s
            .finish_pause(
                ticket,
                now + Duration::from_millis(1001),
                if !native_held {
                    Ok(())
                } else {
                    Err(invalid("held native input"))
                },
            )
            .unwrap();
        assert_eq!(ack, record(Kind::Paused, 1, 3, 0));
        assert_eq!(s.phase(), Phase::Paused);
        assert_eq!(s.deadline(), original);
        assert_eq!(
            s.resume(
                record(Kind::Resume, 2, 4, 21),
                now + Duration::from_secs(2),
                |_| Ok(())
            )
            .unwrap()
            .kind,
            Kind::Resumed
        );
        assert_eq!(s.deadline(), original);
        assert!(s.check(original).is_err());
        assert_eq!(s.phase(), Phase::Terminal);
        assert!(
            s.resume(record(Kind::Resume, 3, 5, 22), original, |_| panic!(
                "must not regrant"
            ))
            .is_err()
        );
    }
    #[test]
    fn release_failure_and_deadline_are_terminal() {
        let now = Instant::now();
        for timed_out in [false, true] {
            let mut s = session(now);
            s.resume(record(Kind::Resume, 1, 1, 20), now, |_| Ok(()))
                .unwrap();
            let ticket = s.begin_pause(record(Kind::Pause, 1, 2, 0), now).unwrap();
            let at = if timed_out {
                now + TRANSITION_TIMEOUT
            } else {
                now
            };
            assert!(
                s.finish_pause(
                    ticket,
                    at,
                    if timed_out {
                        Ok(())
                    } else {
                        Err(invalid("release failed"))
                    }
                )
                .is_err()
            );
            assert_eq!(s.phase(), Phase::Terminal);
            assert!(
                s.resume(record(Kind::Resume, 2, 3, 21), at, |_| panic!(
                    "terminal session revived"
                ))
                .is_err()
            );
        }
    }
    #[test]
    fn early_resume_input_replay_and_scope_changes_fail_closed() {
        let now = Instant::now();
        for case in 0..6 {
            let mut s = session(now);
            s.resume(record(Kind::Resume, 1, 1, 20), now, |_| Ok(()))
                .unwrap();
            if case == 0 {
                let _ticket = s.begin_pause(record(Kind::Pause, 1, 2, 0), now).unwrap();
                assert!(
                    s.resume(record(Kind::Resume, 2, 3, 21), now, |_| panic!(
                        "before release ack"
                    ))
                    .is_err()
                );
            } else {
                let mut stamp = InputStamp {
                    binding: binding(),
                    generation: 1,
                    sequence: 2,
                };
                match case {
                    1 => stamp.sequence = 1,
                    2 => stamp.sequence = 3,
                    3 => stamp.generation = 2,
                    4 => stamp.binding.source_epoch += 1,
                    _ => stamp.binding.geometry_epoch += 1,
                }
                assert!(s.accept_input(stamp, now).is_err());
            }
            assert_eq!(s.phase(), Phase::Terminal);
        }
    }
    #[test]
    fn old_proof_failed_activation_and_terminal_revoke_cannot_resume() {
        let now = Instant::now();
        for case in 0..3 {
            let mut s = session(now);
            s.resume(record(Kind::Resume, 1, 1, 20), now, |_| Ok(()))
                .unwrap();
            let ticket = s.begin_pause(record(Kind::Pause, 1, 2, 0), now).unwrap();
            s.finish_pause(ticket, now, Ok(())).unwrap();
            if case == 2 {
                s.revoke();
            }
            assert!(
                s.resume(
                    record(Kind::Resume, 2, 3, if case == 0 { 20 } else { 21 }),
                    now,
                    |_| if case == 1 {
                        Err(invalid("unclean focus"))
                    } else {
                        panic!("must reject before native activation")
                    }
                )
                .is_err()
            );
            assert_eq!(s.phase(), Phase::Terminal);
        }
    }
    #[test]
    fn paused_expiry_and_clock_reversal_are_terminal() {
        let now = Instant::now();
        let mut s = session(now);
        assert!(s.check(now + MAX_LEASE).is_err());
        let mut s = session(now);
        s.check(now + Duration::from_secs(1)).unwrap();
        assert!(s.check(now).is_err());
        assert_eq!(s.phase(), Phase::Terminal);
    }
    fn permission() -> Negotiated {
        Negotiated::new(LocalOptIn::Explicit, Some(VERSION)).unwrap()
    }
    fn action_payload(sequence: u64) -> [u8; ACTION_PAYLOAD_LEN] {
        let mut payload = [0; ACTION_PAYLOAD_LEN];
        for (offset, value) in [(0, 11_u64), (8, 13), (16, 20), (24, 7), (32, sequence)] {
            payload[offset..offset + 8].copy_from_slice(&value.to_be_bytes());
        }
        payload[42] = 30;
        payload[43] = 1;
        payload
    }
    #[test]
    fn canonical_action_envelope_has_exact_inner_binding_and_sequence() {
        let stamp = InputStamp {
            binding: binding(),
            generation: 1,
            sequence: 1,
        };
        let action = ActionEnvelope::new(stamp, &action_payload(1)).unwrap();
        let bytes = Message::Action(action).encode().unwrap();
        assert_eq!(Message::decode(&bytes).unwrap(), Message::Action(action));
        for len in 0..bytes.len() {
            assert!(Message::decode(&bytes[..len]).is_err());
        }
        let mut long = bytes.clone();
        long.push(0);
        assert!(Message::decode(&long).is_err());
        for offset in [0, 8, 24, 32] {
            let mut bad = action_payload(1);
            bad[offset + 7] ^= 1;
            assert!(ActionEnvelope::new(stamp, &bad).is_err());
        }
        for (offset, value) in [(40, 7), (41, 1), (42, 0), (43, 2), (44, 1), (47, 1)] {
            let mut bad = action_payload(1);
            bad[offset] = value;
            assert!(ActionEnvelope::new(stamp, &bad).is_err());
        }
        for kind in 1..=6 {
            let mut motion = action_payload(1);
            motion[40] = kind;
            motion[41..45].fill(255);
            assert!(ActionEnvelope::new(stamp, &motion).is_ok());
            motion[45] = 1;
            assert!(ActionEnvelope::new(stamp, &motion).is_err());
        }
    }
    #[test]
    fn client_and_host_share_action_order_and_ack_gates_without_renewal() {
        let now = Instant::now();
        let deadline = now + MAX_LEASE;
        let mut client =
            ClientSession::after_granted(permission(), binding(), now, deadline, 20).unwrap();
        let mut host =
            HostSession::after_native_arm(permission(), binding(), now, deadline, 20).unwrap();
        let action = client.wrap_action(&action_payload(1), now).unwrap();
        host.accept_action(&action, now).unwrap();
        let pause = client.begin_pause(now).unwrap();
        assert_eq!(pause.sequence, 2);
        assert_eq!(client.phase(), Phase::Releasing);
        let ticket = host.begin_pause(pause, now).unwrap();
        assert_eq!(host.phase(), Phase::Releasing);
        let ack = host.finish_pause(ticket, now, Ok(())).unwrap();
        assert_eq!(client.phase(), Phase::Releasing);
        client.receive_ack(ack, now).unwrap();
        assert_eq!(client.phase(), Phase::Paused);
        let resume = client.begin_resume(21, now, |_| Ok(())).unwrap();
        assert_eq!(resume.generation, 2);
        assert_eq!(resume.sequence, 3);
        assert_eq!(client.phase(), Phase::Resuming);
        let ack = host.resume(resume, now, |_| Ok(())).unwrap();
        assert_eq!(client.phase(), Phase::Resuming);
        client.receive_ack(ack, now).unwrap();
        let action = client.wrap_action(&action_payload(4), now).unwrap();
        assert_eq!(action.stamp().generation, 2);
        host.accept_action(&action, now).unwrap();
        assert_eq!(client.deadline(), deadline);
        assert_eq!(host.deadline(), deadline);
        assert!(client.check(deadline).is_err());
        assert!(host.check(deadline).is_err());
    }
    #[test]
    fn client_rejects_early_input_resume_mismatched_ack_and_late_ack() {
        let now = Instant::now();
        for case in 0..6 {
            let mut client =
                ClientSession::after_granted(permission(), binding(), now, now + MAX_LEASE, 20)
                    .unwrap();
            let pause = client.begin_pause(now).unwrap();
            match case {
                0 => assert!(client.wrap_action(&action_payload(2), now).is_err()),
                1 => assert!(
                    client
                        .begin_resume(21, now, |_| panic!("before pause ACK"))
                        .is_err()
                ),
                2 => {
                    let mut ack = pause.acknowledgement();
                    ack.sequence += 1;
                    assert!(client.receive_ack(ack, now).is_err());
                }
                3 => {
                    let mut ack = pause.acknowledgement();
                    ack.binding.session += 1;
                    assert!(client.receive_ack(ack, now).is_err());
                }
                4 => assert!(
                    client
                        .receive_ack(pause.acknowledgement(), now + TRANSITION_TIMEOUT)
                        .is_err()
                ),
                _ => {
                    client.revoke();
                    assert!(client.receive_ack(pause.acknowledgement(), now).is_err());
                }
            }
            assert_eq!(client.phase(), Phase::Terminal);
        }
    }
    #[test]
    fn initial_arm_and_replay_bounds_are_strict() {
        let now = Instant::now();
        assert!(HostSession::new(permission(), binding(), now, now).is_err());
        assert!(
            HostSession::new(
                permission(),
                binding(),
                now,
                now + MAX_LEASE + Duration::from_nanos(1)
            )
            .is_err()
        );
        let mut host =
            HostSession::after_native_arm(permission(), binding(), now, now + MAX_LEASE, 20)
                .unwrap();
        let old = ActionEnvelope::new(
            InputStamp {
                binding: binding(),
                generation: 1,
                sequence: 1,
            },
            &action_payload(1),
        )
        .unwrap();
        host.accept_action(&old, now).unwrap();
        assert!(host.accept_action(&old, now).is_err());
        assert_eq!(host.phase(), Phase::Terminal);
        let mut client =
            ClientSession::after_granted(permission(), binding(), now, now + MAX_LEASE, 20)
                .unwrap();
        client.sequence = MAX_SEQUENCE;
        assert!(client.begin_pause(now).is_err());
        assert_eq!(client.phase(), Phase::Terminal);
    }
    #[test]
    fn native_arm_frame_zero_is_valid_without_allowing_zero_action_or_resume() {
        let now = Instant::now();
        let deadline = now + MAX_LEASE;
        let mut host =
            HostSession::after_native_arm(permission(), binding(), now, deadline, 0).unwrap();
        let mut client =
            ClientSession::after_granted(permission(), binding(), now, deadline, 0).unwrap();
        assert_eq!(host.phase(), Phase::Active);
        assert_eq!(client.phase(), Phase::Active);
        assert_eq!(client.generation(), 1);
        // Legacy ControlAction's drawn frame zero remains canonical. Its
        // independent global action sequence must still start at one.
        let mut payload = action_payload(1);
        payload[16..24].fill(0);
        let action = client.wrap_action(&payload, now).unwrap();
        host.accept_action(&action, now).unwrap();
        let pause = client.begin_pause(now).unwrap();
        let ticket = host.begin_pause(pause, now).unwrap();
        client
            .receive_ack(host.finish_pause(ticket, now, Ok(())).unwrap(), now)
            .unwrap();
        assert!(record(Kind::Resume, 2, 3, 0).encode().is_err());
        let resume = client.begin_resume(1, now, |_| Ok(())).unwrap();
        client
            .receive_ack(host.resume(resume, now, |_| Ok(())).unwrap(), now)
            .unwrap();
        assert_eq!(client.generation(), 2);
        assert_eq!(host.deadline(), deadline);
        assert_eq!(client.deadline(), deadline);
        let mut fresh =
            HostSession::after_native_arm(permission(), binding(), now, deadline, 0).unwrap();
        assert!(
            fresh
                .accept_input(
                    InputStamp {
                        binding: binding(),
                        generation: 1,
                        sequence: 0
                    },
                    now
                )
                .is_err()
        );
        assert_eq!(fresh.phase(), Phase::Terminal);
        let mut fresh_client =
            ClientSession::after_granted(permission(), binding(), now, deadline, 0).unwrap();
        let pause = fresh_client.begin_pause(now).unwrap();
        fresh_client
            .receive_ack(pause.acknowledgement(), now)
            .unwrap();
        assert!(
            fresh_client
                .begin_resume(0, now, |_| panic!("zero cannot reauthorize"))
                .is_err()
        );
        assert_eq!(fresh_client.phase(), Phase::Terminal);
    }
}
