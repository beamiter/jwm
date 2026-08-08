//! One-exec handoff for established and still-launching named scratchpads.
//!
//! `ClientKey` values only make sense inside one [`Jwm`](crate::Jwm), while
//! X11 window ids survive JWM replacing itself with `exec`.  This module
//! carries the small name -> window identity table across that one exec and
//! resolves it against the newly adopted clients. Pending launches carry the
//! exact spawned PID, Linux process start time when observable, and remaining
//! deadline so they cannot turn back into a global "next window" guess.

use crate::Jwm;
use crate::backend::api::{Backend, WindowHandoffIdentity};
use crate::backend::common_define::WindowId;
use crate::core::models::ClientKey;
use crate::core::state::WMState;
use crate::jwm::scratchpad_pending::{
    MAX_PENDING_REMAINING_MS, MAX_PENDING_SCRATCHPADS, MAX_UNVERIFIED_PENDING_REMAINING_MS,
    PendingScratchpadSnapshot, ScratchpadPendingRegistry, linux_process_start_time,
    valid_scratchpad_name,
};
use log::{info, warn};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(crate) const SCRATCHPAD_HANDOFF_ENV: &str = "JWM_SCRATCHPAD_HANDOFF_V2";
pub(crate) const LEGACY_SCRATCHPAD_HANDOFF_ENV: &str = "JWM_SCRATCHPAD_HANDOFF_V1";

const HANDOFF_VERSION: u32 = 2;
const MAX_HANDOFF_BYTES: usize = 32 * 1024;
const MAX_SCRATCHPADS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireHandoff {
    version: u32,
    issuer_pid: u32,
    captured_unix_ms: u64,
    entries: Vec<WireEntry>,
    pending: Vec<WirePendingEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireEntry {
    name: String,
    identity: WireWindowIdentity,
    pid: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum WireWindowIdentity {
    X11 { xid: u32 },
}

impl From<WindowHandoffIdentity> for WireWindowIdentity {
    fn from(identity: WindowHandoffIdentity) -> Self {
        match identity {
            WindowHandoffIdentity::X11(xid) => Self::X11 { xid },
        }
    }
}

impl From<WireWindowIdentity> for WindowHandoffIdentity {
    fn from(identity: WireWindowIdentity) -> Self {
        match identity {
            WireWindowIdentity::X11 { xid } => Self::X11(xid),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WirePendingEntry {
    name: String,
    pid: u32,
    process_start_time: Option<u64>,
    remaining_ms: u64,
}

/// Validated restart data. Construction is restricted to capture or strict
/// decoding, so adoption never has to handle an unbounded or ambiguous table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ScratchpadRestartHandoff {
    issuer_pid: u32,
    captured_unix_ms: u64,
    entries: Vec<WireEntry>,
    pending: Vec<WirePendingEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ScratchpadHandoffError {
    PayloadTooLarge { bytes: usize },
    TooManyEntries { entries: usize },
    TooManyPendingEntries { entries: usize },
    InvalidJson(String),
    UnsupportedVersion(u32),
    InvalidIssuerPid(u32),
    InvalidCaptureTime,
    IssuerPidMismatch { expected: u32, actual: u32 },
    InvalidName { index: usize },
    InvalidWindowIdentity { index: usize },
    InvalidWindowPid { index: usize },
    DuplicateName(String),
    DuplicateWindowIdentity(WindowHandoffIdentity),
    MissingWindowIdentity(String),
    InvalidPendingPid { index: usize },
    InvalidPendingStartTime { index: usize },
    InvalidPendingLifetime { index: usize },
    DuplicatePendingPid(u32),
    MissingClient(String),
}

impl fmt::Display for ScratchpadHandoffError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PayloadTooLarge { bytes } => write!(
                f,
                "scratchpad restart handoff is {bytes} bytes (maximum {MAX_HANDOFF_BYTES})"
            ),
            Self::TooManyEntries { entries } => write!(
                f,
                "scratchpad restart handoff has {entries} entries (maximum {MAX_SCRATCHPADS})"
            ),
            Self::TooManyPendingEntries { entries } => write!(
                f,
                "scratchpad restart handoff has {entries} pending entries (maximum {MAX_PENDING_SCRATCHPADS})"
            ),
            Self::InvalidJson(error) => {
                write!(f, "malformed scratchpad restart handoff: {error}")
            }
            Self::UnsupportedVersion(version) => {
                write!(
                    f,
                    "unsupported scratchpad restart handoff version {version}"
                )
            }
            Self::InvalidIssuerPid(pid) => {
                write!(f, "invalid scratchpad restart issuer pid {pid}")
            }
            Self::InvalidCaptureTime => write!(f, "invalid scratchpad handoff capture time"),
            Self::IssuerPidMismatch { expected, actual } => write!(
                f,
                "scratchpad restart issuer pid {actual} does not match this process {expected}"
            ),
            Self::InvalidName { index } => {
                write!(f, "invalid scratchpad name at handoff entry {index}")
            }
            Self::InvalidWindowIdentity { index } => {
                write!(f, "invalid window identity at handoff entry {index}")
            }
            Self::InvalidWindowPid { index } => {
                write!(f, "invalid window pid at handoff entry {index}")
            }
            Self::DuplicateName(name) => {
                write!(f, "duplicate scratchpad name {name:?} in restart handoff")
            }
            Self::DuplicateWindowIdentity(identity) => write!(
                f,
                "window identity {identity:?} occurs more than once in scratchpad restart handoff"
            ),
            Self::MissingWindowIdentity(name) => write!(
                f,
                "scratchpad {name:?} has no backend identity safe for exec handoff"
            ),
            Self::InvalidPendingPid { index } => {
                write!(f, "invalid pending PID at handoff entry {index}")
            }
            Self::InvalidPendingStartTime { index } => {
                write!(
                    f,
                    "invalid pending process start time at handoff entry {index}"
                )
            }
            Self::InvalidPendingLifetime { index } => {
                write!(f, "invalid pending lifetime at handoff entry {index}")
            }
            Self::DuplicatePendingPid(pid) => {
                write!(
                    f,
                    "pending PID {pid} occurs more than once in restart handoff"
                )
            }
            Self::MissingClient(name) => write!(
                f,
                "scratchpad {name:?} no longer refers to a managed client"
            ),
        }
    }
}

impl std::error::Error for ScratchpadHandoffError {}

fn validate_wire(wire: &WireHandoff) -> Result<(), ScratchpadHandoffError> {
    if wire.version != HANDOFF_VERSION {
        return Err(ScratchpadHandoffError::UnsupportedVersion(wire.version));
    }
    if wire.issuer_pid == 0 {
        return Err(ScratchpadHandoffError::InvalidIssuerPid(wire.issuer_pid));
    }
    if wire.captured_unix_ms == 0 {
        return Err(ScratchpadHandoffError::InvalidCaptureTime);
    }
    if wire.entries.len() > MAX_SCRATCHPADS {
        return Err(ScratchpadHandoffError::TooManyEntries {
            entries: wire.entries.len(),
        });
    }
    if wire.pending.len() > MAX_PENDING_SCRATCHPADS {
        return Err(ScratchpadHandoffError::TooManyPendingEntries {
            entries: wire.pending.len(),
        });
    }

    let mut names = HashSet::with_capacity(wire.entries.len() + wire.pending.len());
    let mut windows = HashSet::with_capacity(wire.entries.len());
    for (index, entry) in wire.entries.iter().enumerate() {
        if !valid_scratchpad_name(&entry.name) {
            return Err(ScratchpadHandoffError::InvalidName { index });
        }
        if matches!(entry.identity, WireWindowIdentity::X11 { xid: 0 }) {
            return Err(ScratchpadHandoffError::InvalidWindowIdentity { index });
        }
        if entry.pid == Some(0) {
            return Err(ScratchpadHandoffError::InvalidWindowPid { index });
        }
        if !names.insert(entry.name.clone()) {
            return Err(ScratchpadHandoffError::DuplicateName(entry.name.clone()));
        }
        if !windows.insert(entry.identity) {
            return Err(ScratchpadHandoffError::DuplicateWindowIdentity(
                entry.identity.into(),
            ));
        }
    }
    let mut pending_pids = HashSet::with_capacity(wire.pending.len());
    for (index, entry) in wire.pending.iter().enumerate() {
        if !valid_scratchpad_name(&entry.name) {
            return Err(ScratchpadHandoffError::InvalidName {
                index: wire.entries.len() + index,
            });
        }
        if entry.pid == 0 {
            return Err(ScratchpadHandoffError::InvalidPendingPid { index });
        }
        if entry.process_start_time == Some(0) {
            return Err(ScratchpadHandoffError::InvalidPendingStartTime { index });
        }
        let maximum = if entry.process_start_time.is_some() {
            MAX_PENDING_REMAINING_MS
        } else {
            MAX_UNVERIFIED_PENDING_REMAINING_MS
        };
        if entry.remaining_ms == 0 || entry.remaining_ms > maximum {
            return Err(ScratchpadHandoffError::InvalidPendingLifetime { index });
        }
        if !names.insert(entry.name.clone()) {
            return Err(ScratchpadHandoffError::DuplicateName(entry.name.clone()));
        }
        if !pending_pids.insert(entry.pid) {
            return Err(ScratchpadHandoffError::DuplicatePendingPid(entry.pid));
        }
    }
    Ok(())
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(1)
        .max(1)
}

impl ScratchpadRestartHandoff {
    /// Test-only fail-closed entry point for payloads without established
    /// mappings. Production capture always supplies a backend identity
    /// exporter and never interprets a backend-local raw id.
    #[cfg(test)]
    pub(crate) fn capture(
        scratchpads: &HashMap<String, ClientKey>,
        state: &WMState,
        pending: &ScratchpadPendingRegistry,
        now: Instant,
        issuer_pid: u32,
    ) -> Result<Self, ScratchpadHandoffError> {
        Self::capture_with_window_identity(scratchpads, state, pending, now, issuer_pid, |_| None)
    }

    pub(crate) fn capture_with_window_identity(
        scratchpads: &HashMap<String, ClientKey>,
        state: &WMState,
        pending: &ScratchpadPendingRegistry,
        now: Instant,
        issuer_pid: u32,
        mut identity_for: impl FnMut(WindowId) -> Option<WindowHandoffIdentity>,
    ) -> Result<Self, ScratchpadHandoffError> {
        let mut named_clients = scratchpads.iter().collect::<Vec<_>>();
        named_clients.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));

        let entries = named_clients
            .into_iter()
            .map(|(name, &client_key)| {
                let client = state
                    .clients
                    .get(client_key)
                    .ok_or_else(|| ScratchpadHandoffError::MissingClient(name.clone()))?;
                let identity = identity_for(client.win)
                    .ok_or_else(|| ScratchpadHandoffError::MissingWindowIdentity(name.clone()))?;
                Ok(WireEntry {
                    name: name.clone(),
                    identity: identity.into(),
                    pid: client.pid,
                })
            })
            .collect::<Result<Vec<_>, ScratchpadHandoffError>>()?;
        let pending = pending
            .snapshots(now)
            .into_iter()
            .map(|entry| WirePendingEntry {
                name: entry.name,
                pid: entry.pid,
                process_start_time: entry.process_start_time,
                remaining_ms: entry.remaining_ms,
            })
            .collect();
        let handoff = Self {
            issuer_pid,
            captured_unix_ms: unix_time_ms(),
            entries,
            pending,
        };
        handoff.validate()?;
        Ok(handoff)
    }

    pub(crate) fn encode(&self) -> Result<String, ScratchpadHandoffError> {
        self.validate()?;
        let payload = serde_json::to_string(&WireHandoff {
            version: HANDOFF_VERSION,
            issuer_pid: self.issuer_pid,
            captured_unix_ms: self.captured_unix_ms,
            entries: self.entries.clone(),
            pending: self.pending.clone(),
        })
        .map_err(|error| ScratchpadHandoffError::InvalidJson(error.to_string()))?;
        if payload.len() > MAX_HANDOFF_BYTES {
            return Err(ScratchpadHandoffError::PayloadTooLarge {
                bytes: payload.len(),
            });
        }
        Ok(payload)
    }

    pub(crate) fn decode_for_pid(
        payload: &str,
        current_pid: u32,
    ) -> Result<Self, ScratchpadHandoffError> {
        if payload.len() > MAX_HANDOFF_BYTES {
            return Err(ScratchpadHandoffError::PayloadTooLarge {
                bytes: payload.len(),
            });
        }
        let wire = serde_json::from_str::<WireHandoff>(payload)
            .map_err(|error| ScratchpadHandoffError::InvalidJson(error.to_string()))?;
        validate_wire(&wire)?;
        if wire.issuer_pid != current_pid {
            return Err(ScratchpadHandoffError::IssuerPidMismatch {
                expected: current_pid,
                actual: wire.issuer_pid,
            });
        }
        Ok(Self {
            issuer_pid: wire.issuer_pid,
            captured_unix_ms: wire.captured_unix_ms,
            entries: wire.entries,
            pending: wire.pending,
        })
    }

    fn validate(&self) -> Result<(), ScratchpadHandoffError> {
        validate_wire(&WireHandoff {
            version: HANDOFF_VERSION,
            issuer_pid: self.issuer_pid,
            captured_unix_ms: self.captured_unix_ms,
            entries: self.entries.clone(),
            pending: self.pending.clone(),
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PendingHandoffReport {
    pub installed: usize,
    pub expired: usize,
    pub identity_mismatches: usize,
    pub conflicts: usize,
}

fn install_pending_handoff(
    registry: &mut ScratchpadPendingRegistry,
    handoff: &ScratchpadRestartHandoff,
    now: Instant,
    current_unix_ms: u64,
    mut process_start_time: impl FnMut(u32) -> Option<u64>,
) -> PendingHandoffReport {
    let mut report = PendingHandoffReport::default();
    let elapsed_ms = current_unix_ms.saturating_sub(handoff.captured_unix_ms);
    for entry in &handoff.pending {
        let remaining_ms = entry.remaining_ms.saturating_sub(elapsed_ms);
        if remaining_ms == 0 {
            report.expired += 1;
            warn!(
                "[scratchpad-handoff] pending {:?} for PID {} expired during restart",
                entry.name, entry.pid
            );
            continue;
        }
        if let Some(expected) = entry.process_start_time {
            let observed = process_start_time(entry.pid);
            if observed != Some(expected) {
                report.identity_mismatches += 1;
                warn!(
                    "[scratchpad-handoff] pending {:?} PID {} changed/disappeared during restart (starttime expected {}, observed {:?})",
                    entry.name, entry.pid, expected, observed
                );
                continue;
            }
        }

        let snapshot = PendingScratchpadSnapshot {
            name: entry.name.clone(),
            pid: entry.pid,
            process_start_time: entry.process_start_time,
            remaining_ms,
        };
        let result = registry.register_with_remaining(
            snapshot.pid,
            snapshot.name.clone(),
            snapshot.process_start_time,
            now,
            Duration::from_millis(snapshot.remaining_ms),
        );
        match result {
            Ok(()) => report.installed += 1,
            Err(error) => {
                report.conflicts += 1;
                warn!(
                    "[scratchpad-handoff] rejecting pending {:?} for PID {}: {}",
                    snapshot.name, snapshot.pid, error
                );
            }
        }
    }
    report
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ScratchpadAdoptionReport {
    pub adopted: usize,
    pub disappeared: usize,
    pub pid_mismatches: usize,
    pub conflicts: usize,
}

fn adopt_handoff_with_window_identity(
    scratchpads: &mut HashMap<String, ClientKey>,
    state: &WMState,
    handoff: ScratchpadRestartHandoff,
    mut resolve_identity: impl FnMut(WindowHandoffIdentity) -> Option<WindowId>,
) -> ScratchpadAdoptionReport {
    let mut report = ScratchpadAdoptionReport::default();
    for entry in handoff.entries {
        let identity: WindowHandoffIdentity = entry.identity.into();
        let Some(win) = resolve_identity(identity) else {
            report.disappeared += 1;
            warn!(
                "[scratchpad-handoff] rejecting {:?}: window {identity:?} was not discovered after restart",
                entry.name
            );
            continue;
        };
        let Some(&client_key) = state.win_to_client.get(&win) else {
            report.disappeared += 1;
            warn!(
                "[scratchpad-handoff] rejecting {:?}: window {win:?} disappeared during restart",
                entry.name
            );
            continue;
        };
        let Some(client) = state
            .clients
            .get(client_key)
            .filter(|client| client.win == win)
        else {
            report.disappeared += 1;
            warn!(
                "[scratchpad-handoff] rejecting {:?}: adopted window index is inconsistent",
                entry.name
            );
            continue;
        };
        if let Some(expected_pid) = entry.pid
            && client.pid != Some(expected_pid)
        {
            report.pid_mismatches += 1;
            warn!(
                "[scratchpad-handoff] rejecting {:?}: window {win:?} PID changed from {expected_pid} to {:?}",
                entry.name, client.pid
            );
            continue;
        }
        if scratchpads.contains_key(&entry.name)
            || scratchpads.values().any(|&existing| existing == client_key)
        {
            report.conflicts += 1;
            warn!(
                "[scratchpad-handoff] rejecting conflicting mapping for {:?} ({win:?})",
                entry.name
            );
            continue;
        }

        scratchpads.insert(entry.name, client_key);
        report.adopted += 1;
    }
    report
}

impl Jwm {
    pub(crate) fn capture_scratchpad_restart_handoff(
        &self,
        backend: &dyn Backend,
    ) -> Result<ScratchpadRestartHandoff, ScratchpadHandoffError> {
        ScratchpadRestartHandoff::capture_with_window_identity(
            &self.scratchpads,
            &self.state,
            &self.scratchpad_pending,
            Instant::now(),
            std::process::id(),
            |window| backend.window_handoff_identity(window),
        )
    }

    /// Restore pending launch identities before initial-window scanning. This
    /// is separate from established mapping adoption because `manage` must be
    /// able to claim an already-mapped child by PID during that scan.
    pub(crate) fn install_pending_scratchpad_restart_handoff(
        &mut self,
        handoff: &ScratchpadRestartHandoff,
    ) -> PendingHandoffReport {
        let report = install_pending_handoff(
            &mut self.scratchpad_pending,
            handoff,
            Instant::now(),
            unix_time_ms(),
            linux_process_start_time,
        );
        info!(
            "[scratchpad-handoff] pending installed {}, expired {}, identity-mismatch {}, conflict {}",
            report.installed, report.expired, report.identity_mismatches, report.conflicts
        );
        report
    }

    pub(crate) fn adopt_scratchpad_restart_handoff(
        &mut self,
        backend: &dyn Backend,
        handoff: ScratchpadRestartHandoff,
    ) -> ScratchpadAdoptionReport {
        let report = adopt_handoff_with_window_identity(
            &mut self.scratchpads,
            &self.state,
            handoff,
            |identity| backend.resolve_window_handoff_identity(identity),
        );
        info!(
            "[scratchpad-handoff] adopted {}, disappeared {}, PID-mismatch {}, conflict {}",
            report.adopted, report.disappeared, report.pid_mismatches, report.conflicts
        );
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::models::WMClient;

    fn entry(name: &str, xid: u32, pid: Option<u32>) -> WireEntry {
        WireEntry {
            name: name.to_owned(),
            identity: WireWindowIdentity::X11 { xid },
            pid,
        }
    }

    fn handoff(entries: Vec<WireEntry>) -> ScratchpadRestartHandoff {
        ScratchpadRestartHandoff {
            issuer_pid: 4242,
            captured_unix_ms: 1_000_000,
            entries,
            pending: Vec::new(),
        }
    }

    fn pending_entry(
        name: &str,
        pid: u32,
        process_start_time: Option<u64>,
        remaining_ms: u64,
    ) -> WirePendingEntry {
        WirePendingEntry {
            name: name.into(),
            pid,
            process_start_time,
            remaining_ms,
        }
    }

    fn insert_client(
        state: &mut WMState,
        window_id: u64,
        pid: Option<u32>,
        tags: u32,
        hidden: bool,
    ) -> ClientKey {
        let win = WindowId::from_raw(window_id);
        let mut client = WMClient::new(win);
        client.pid = pid;
        client.state.tags = tags;
        client.state.is_hidden = hidden;
        let key = state.clients.insert(client);
        state.win_to_client.insert(win, key);
        key
    }

    fn local_raw_for_x11(identity: WindowHandoffIdentity) -> Option<WindowId> {
        match identity {
            WindowHandoffIdentity::X11(xid) => Some(WindowId::from_raw(u64::from(xid))),
        }
    }

    fn adopt_using_xid_as_local_for_test(
        scratchpads: &mut HashMap<String, ClientKey>,
        state: &WMState,
        handoff: ScratchpadRestartHandoff,
    ) -> ScratchpadAdoptionReport {
        adopt_handoff_with_window_identity(scratchpads, state, handoff, local_raw_for_x11)
    }

    #[test]
    fn codec_round_trip_is_versioned_and_pid_bound() {
        let original = handoff(vec![
            entry("music", 0x200001, Some(101)),
            entry("term", 0x200002, None),
        ]);
        let payload = original.encode().unwrap();

        assert!(
            payload.contains(r#""identity":{"kind":"x11","xid":2097153}"#),
            "the V2 wire format must carry the server-owned XID"
        );
        assert!(
            !payload.contains("window_id"),
            "backend-local WindowId values must never enter the handoff payload"
        );

        assert_eq!(
            ScratchpadRestartHandoff::decode_for_pid(&payload, 4242).unwrap(),
            original
        );
        assert!(matches!(
            ScratchpadRestartHandoff::decode_for_pid(&payload, 4243),
            Err(ScratchpadHandoffError::IssuerPidMismatch { .. })
        ));
    }

    #[test]
    fn decoder_rejects_malformed_unknown_duplicate_and_unbounded_inputs() {
        assert!(matches!(
            ScratchpadRestartHandoff::decode_for_pid("not-json", 1),
            Err(ScratchpadHandoffError::InvalidJson(_))
        ));
        assert!(matches!(
            ScratchpadRestartHandoff::decode_for_pid(
                r#"{"version":3,"issuer_pid":1,"captured_unix_ms":1,"entries":[],"pending":[]}"#,
                1
            ),
            Err(ScratchpadHandoffError::UnsupportedVersion(3))
        ));
        assert!(matches!(
            ScratchpadRestartHandoff::decode_for_pid(
                r#"{"version":2,"issuer_pid":1,"captured_unix_ms":1,"entries":[],"pending":[],"future":true}"#,
                1
            ),
            Err(ScratchpadHandoffError::InvalidJson(_))
        ));
        assert!(matches!(
            ScratchpadRestartHandoff::decode_for_pid(
                r#"{"version":2,"issuer_pid":1,"captured_unix_ms":1,"entries":[{"name":"term","window_id":1,"pid":null}],"pending":[]}"#,
                1
            ),
            Err(ScratchpadHandoffError::InvalidJson(_))
        ));

        let zero_xid = handoff(vec![entry("term", 0, None)]);
        assert!(matches!(
            zero_xid.encode(),
            Err(ScratchpadHandoffError::InvalidWindowIdentity { index: 0 })
        ));

        let duplicate_name = handoff(vec![entry("term", 1, None), entry("term", 2, None)]);
        assert!(matches!(
            duplicate_name.encode(),
            Err(ScratchpadHandoffError::DuplicateName(name)) if name == "term"
        ));
        let duplicate_window = handoff(vec![entry("term", 1, None), entry("music", 1, None)]);
        assert!(matches!(
            duplicate_window.encode(),
            Err(ScratchpadHandoffError::DuplicateWindowIdentity(
                WindowHandoffIdentity::X11(1)
            ))
        ));

        let too_many = handoff(
            (0..=MAX_SCRATCHPADS)
                .map(|index| entry(&format!("sp-{index}"), index as u32 + 1, None))
                .collect(),
        );
        assert!(matches!(
            too_many.encode(),
            Err(ScratchpadHandoffError::TooManyEntries { .. })
        ));
        let mut too_many_pending = handoff(Vec::new());
        too_many_pending.pending = (0..=MAX_PENDING_SCRATCHPADS)
            .map(|index| {
                pending_entry(
                    &format!("pending-{index}"),
                    index as u32 + 1,
                    Some(index as u64 + 1),
                    1_000,
                )
            })
            .collect();
        assert!(matches!(
            too_many_pending.encode(),
            Err(ScratchpadHandoffError::TooManyPendingEntries { .. })
        ));
        let mut unsafe_unverified_lifetime = handoff(Vec::new());
        unsafe_unverified_lifetime.pending = vec![pending_entry(
            "term",
            101,
            None,
            MAX_UNVERIFIED_PENDING_REMAINING_MS + 1,
        )];
        assert!(matches!(
            unsafe_unverified_lifetime.encode(),
            Err(ScratchpadHandoffError::InvalidPendingLifetime { index: 0 })
        ));
        assert!(matches!(
            ScratchpadRestartHandoff::decode_for_pid(&"x".repeat(MAX_HANDOFF_BYTES + 1), 1),
            Err(ScratchpadHandoffError::PayloadTooLarge { .. })
        ));
    }

    #[test]
    fn minimized_tags_zero_scratchpad_is_rebound_to_new_client_key() {
        let mut state = WMState::new();
        let new_key = insert_client(&mut state, 0x300001, Some(707), 0, true);
        let mut scratchpads = HashMap::new();

        let report = adopt_using_xid_as_local_for_test(
            &mut scratchpads,
            &state,
            handoff(vec![entry("term", 0x300001, Some(707))]),
        );

        assert_eq!(report.adopted, 1);
        assert_eq!(scratchpads.get("term"), Some(&new_key));
        let client = &state.clients[new_key];
        assert_eq!(client.state.tags, 0);
        assert!(client.state.is_hidden);
    }

    #[test]
    fn capture_is_non_destructive_and_reordered_fresh_ids_rebind_by_xid() {
        let mut old_state = WMState::new();
        let old_local = WindowId::from_raw(2);
        let old_key = insert_client(&mut old_state, old_local.raw(), Some(808), 0, true);
        let old_mapping = HashMap::from([("term".to_owned(), old_key)]);
        let stable_xid = 0x300101;

        let captured = ScratchpadRestartHandoff::capture_with_window_identity(
            &old_mapping,
            &old_state,
            &ScratchpadPendingRegistry::default(),
            Instant::now(),
            4242,
            |window| (window == old_local).then_some(WindowHandoffIdentity::X11(stable_xid)),
        )
        .unwrap();
        assert_eq!(old_mapping.get("term"), Some(&old_key));

        let mut new_state = WMState::new();
        // Model a fresh backend that interns the same QueryTree in another
        // order: the scratchpad moved from local WindowId 2 to 3 even though
        // its X server-owned XID is unchanged.
        insert_client(&mut new_state, 2, None, 1, false);
        let new_local = WindowId::from_raw(3);
        let new_key = insert_client(&mut new_state, new_local.raw(), Some(808), 0, true);
        assert_ne!(new_key, old_key);
        let mut fallback_mapping = HashMap::new();

        let report = adopt_handoff_with_window_identity(
            &mut fallback_mapping,
            &new_state,
            captured,
            |identity| (identity == WindowHandoffIdentity::X11(stable_xid)).then_some(new_local),
        );

        assert_eq!(report.adopted, 1);
        assert_eq!(fallback_mapping.get("term"), Some(&new_key));
    }

    #[test]
    fn established_capture_without_backend_identity_fails_closed() {
        let mut state = WMState::new();
        let key = insert_client(&mut state, 7, Some(808), 0, true);
        let scratchpads = HashMap::from([("term".to_owned(), key)]);

        assert!(matches!(
            ScratchpadRestartHandoff::capture(
                &scratchpads,
                &state,
                &ScratchpadPendingRegistry::default(),
                Instant::now(),
                4242,
            ),
            Err(ScratchpadHandoffError::MissingWindowIdentity(name)) if name == "term"
        ));
    }

    #[test]
    fn unresolved_identity_is_not_reinterpreted_as_backend_local_raw() {
        let mut state = WMState::new();
        insert_client(&mut state, 0x300001, Some(707), 0, true);
        let mut scratchpads = HashMap::new();

        // A backend that cannot resolve the stable identity must fail closed.
        // Even though a local id numerically equals the XID, it must not bind.
        let report = adopt_handoff_with_window_identity(
            &mut scratchpads,
            &state,
            handoff(vec![entry("term", 0x300001, Some(707))]),
            |_| None,
        );

        assert!(scratchpads.is_empty());
        assert_eq!(report.disappeared, 1);
    }

    #[test]
    fn disappeared_or_pid_reused_xids_are_not_adopted() {
        let mut state = WMState::new();
        insert_client(&mut state, 0x400001, Some(999), 1, false);
        let mut scratchpads = HashMap::new();

        let report = adopt_using_xid_as_local_for_test(
            &mut scratchpads,
            &state,
            handoff(vec![
                entry("gone", 0x400002, Some(100)),
                entry("reused", 0x400001, Some(100)),
            ]),
        );

        assert!(scratchpads.is_empty());
        assert_eq!(report.disappeared, 1);
        assert_eq!(report.pid_mismatches, 1);
    }

    #[test]
    fn pid_is_optional_but_known_pid_must_remain_observable() {
        let mut state = WMState::new();
        let without_guard = insert_client(&mut state, 0x500001, Some(123), 0, true);
        insert_client(&mut state, 0x500002, None, 0, true);
        let mut scratchpads = HashMap::new();

        let report = adopt_using_xid_as_local_for_test(
            &mut scratchpads,
            &state,
            handoff(vec![
                entry("unguarded", 0x500001, None),
                entry("lost-pid", 0x500002, Some(456)),
            ]),
        );

        assert_eq!(scratchpads.get("unguarded"), Some(&without_guard));
        assert!(!scratchpads.contains_key("lost-pid"));
        assert_eq!(report.adopted, 1);
        assert_eq!(report.pid_mismatches, 1);
    }

    #[test]
    fn pending_registry_round_trips_and_subtracts_restart_elapsed_time() {
        let mut original = handoff(Vec::new());
        original.pending = vec![
            pending_entry("term", 101, Some(7001), 10_000),
            pending_entry("music", 102, None, 4_000),
        ];
        let payload = original.encode().unwrap();
        let decoded = ScratchpadRestartHandoff::decode_for_pid(&payload, 4242).unwrap();
        let mut registry = ScratchpadPendingRegistry::default();
        let now = Instant::now();

        let report = install_pending_handoff(
            &mut registry,
            &decoded,
            now,
            original.captured_unix_ms + 1_500,
            |pid| (pid == 101).then_some(7001),
        );

        assert_eq!(report.installed, 2);
        assert_eq!(registry.len(), 2);
        let snapshots = registry.snapshots(now);
        let term = snapshots.iter().find(|entry| entry.name == "term").unwrap();
        let music = snapshots
            .iter()
            .find(|entry| entry.name == "music")
            .unwrap();
        assert_eq!(term.remaining_ms, 8_500);
        assert_eq!(music.remaining_ms, 2_500);
    }

    #[test]
    fn pending_handoff_rejects_cross_name_duplicates_and_reused_processes() {
        let mut duplicate = handoff(vec![entry("term", 0x600001, Some(10))]);
        duplicate.pending = vec![pending_entry("term", 101, Some(7001), 10_000)];
        assert!(matches!(
            duplicate.encode(),
            Err(ScratchpadHandoffError::DuplicateName(name)) if name == "term"
        ));

        let mut reused = handoff(Vec::new());
        reused.pending = vec![pending_entry("term", 101, Some(7001), 10_000)];
        let mut registry = ScratchpadPendingRegistry::default();
        let report = install_pending_handoff(
            &mut registry,
            &reused,
            Instant::now(),
            reused.captured_unix_ms,
            |_| Some(9999),
        );
        assert_eq!(report.identity_mismatches, 1);
        assert!(registry.is_empty());
    }
}
