//! PID-bound registry for scratchpad launches that have not mapped a window.
//!
//! A name alone is not an identity: while an application is starting, any
//! unrelated MapRequest may arrive first. Entries here bind the requested
//! scratchpad name to the exact child PID returned by `spawn`, and, on Linux,
//! to `/proc/<pid>/stat`'s process start time so a recycled PID cannot claim
//! the name. A wrapper that forks a differently-PIDed daemon is deliberately
//! not followed: its entry expires or is reaped instead of guessing identity.
//! Entries are small, bounded, and expire automatically.

use crate::Jwm;
use log::{info, warn};
use std::collections::HashMap;
#[cfg(test)]
use std::collections::HashSet;
use std::fmt;
use std::time::{Duration, Instant};

pub(crate) const MAX_PENDING_SCRATCHPADS: usize = 64;
pub(crate) const MAX_SCRATCHPAD_NAME_BYTES: usize = 128;
pub(crate) const PENDING_SCRATCHPAD_TTL: Duration = Duration::from_secs(30);
pub(crate) const UNVERIFIED_PENDING_SCRATCHPAD_TTL: Duration = Duration::from_secs(5);
pub(crate) const MAX_PENDING_REMAINING_MS: u64 = PENDING_SCRATCHPAD_TTL.as_millis() as u64;
pub(crate) const MAX_UNVERIFIED_PENDING_REMAINING_MS: u64 =
    UNVERIFIED_PENDING_SCRATCHPAD_TTL.as_millis() as u64;

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingScratchpad {
    name: String,
    process_start_time: Option<u64>,
    deadline: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingScratchpadSnapshot {
    pub name: String,
    pub pid: u32,
    pub process_start_time: Option<u64>,
    pub remaining_ms: u64,
}

#[derive(Debug, Default)]
pub(crate) struct ScratchpadPendingRegistry {
    by_pid: HashMap<u32, PendingScratchpad>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PendingRegistrationError {
    InvalidName,
    InvalidPid,
    InvalidProcessStartTime,
    InvalidLifetime,
    DuplicateName { name: String, pid: u32 },
    DuplicatePid(u32),
    Full,
}

impl fmt::Display for PendingRegistrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName => write!(f, "scratchpad name is empty, unsafe, or too long"),
            Self::InvalidPid => write!(f, "scratchpad child PID must be non-zero"),
            Self::InvalidProcessStartTime => {
                write!(f, "scratchpad process start time must be non-zero")
            }
            Self::InvalidLifetime => write!(f, "scratchpad pending lifetime is invalid"),
            Self::DuplicateName { name, pid } => {
                write!(f, "scratchpad {name:?} is already pending for PID {pid}")
            }
            Self::DuplicatePid(pid) => {
                write!(f, "PID {pid} already owns a pending scratchpad")
            }
            Self::Full => write!(
                f,
                "pending scratchpad registry is full ({MAX_PENDING_SCRATCHPADS} entries)"
            ),
        }
    }
}

impl std::error::Error for PendingRegistrationError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PendingClaim {
    NoMatch,
    Claimed(String),
    Expired(String),
    ProcessIdentityMismatch {
        name: String,
        expected: u64,
        observed: Option<u64>,
    },
}

#[must_use]
pub(crate) fn valid_scratchpad_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SCRATCHPAD_NAME_BYTES
        && !name.chars().any(char::is_control)
}

fn deadline_from_remaining(
    now: Instant,
    remaining: Duration,
) -> Result<Instant, PendingRegistrationError> {
    if remaining.is_zero() || remaining > PENDING_SCRATCHPAD_TTL {
        return Err(PendingRegistrationError::InvalidLifetime);
    }
    now.checked_add(remaining)
        .ok_or(PendingRegistrationError::InvalidLifetime)
}

impl ScratchpadPendingRegistry {
    #[must_use]
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.by_pid.len()
    }

    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.by_pid.is_empty()
    }

    #[must_use]
    pub(crate) fn pending_pid_for_name(&self, name: &str) -> Option<u32> {
        self.by_pid
            .iter()
            .find_map(|(&pid, pending)| (pending.name == name).then_some(pid))
    }

    #[must_use]
    pub(crate) fn contains_pid(&self, pid: u32) -> bool {
        self.by_pid.contains_key(&pid)
    }

    pub(crate) fn ensure_name_can_spawn(&self, name: &str) -> Result<(), PendingRegistrationError> {
        if !valid_scratchpad_name(name) {
            return Err(PendingRegistrationError::InvalidName);
        }
        if let Some(pid) = self.pending_pid_for_name(name) {
            return Err(PendingRegistrationError::DuplicateName {
                name: name.to_owned(),
                pid,
            });
        }
        if self.by_pid.len() >= MAX_PENDING_SCRATCHPADS {
            return Err(PendingRegistrationError::Full);
        }
        Ok(())
    }

    pub(crate) fn register_spawned(
        &mut self,
        pid: u32,
        name: String,
        process_start_time: Option<u64>,
        now: Instant,
    ) -> Result<(), PendingRegistrationError> {
        let lifetime = if process_start_time.is_some() {
            PENDING_SCRATCHPAD_TTL
        } else {
            UNVERIFIED_PENDING_SCRATCHPAD_TTL
        };
        self.register_with_remaining(pid, name, process_start_time, now, lifetime)
    }

    pub(crate) fn register_with_remaining(
        &mut self,
        pid: u32,
        name: String,
        process_start_time: Option<u64>,
        now: Instant,
        remaining: Duration,
    ) -> Result<(), PendingRegistrationError> {
        self.ensure_name_can_spawn(&name)?;
        if pid == 0 {
            return Err(PendingRegistrationError::InvalidPid);
        }
        if process_start_time == Some(0) {
            return Err(PendingRegistrationError::InvalidProcessStartTime);
        }
        if self.by_pid.contains_key(&pid) {
            return Err(PendingRegistrationError::DuplicatePid(pid));
        }
        let maximum = if process_start_time.is_some() {
            PENDING_SCRATCHPAD_TTL
        } else {
            UNVERIFIED_PENDING_SCRATCHPAD_TTL
        };
        if remaining > maximum {
            return Err(PendingRegistrationError::InvalidLifetime);
        }
        let deadline = deadline_from_remaining(now, remaining)?;
        self.by_pid.insert(
            pid,
            PendingScratchpad {
                name,
                process_start_time,
                deadline,
            },
        );
        Ok(())
    }

    pub(crate) fn claim(
        &mut self,
        pid: u32,
        observed_start_time: Option<u64>,
        now: Instant,
    ) -> PendingClaim {
        let Some(pending) = self.by_pid.get(&pid) else {
            return PendingClaim::NoMatch;
        };
        if now >= pending.deadline {
            let name = pending.name.clone();
            self.by_pid.remove(&pid);
            return PendingClaim::Expired(name);
        }
        if let Some(expected) = pending.process_start_time
            && observed_start_time != Some(expected)
        {
            let name = pending.name.clone();
            self.by_pid.remove(&pid);
            return PendingClaim::ProcessIdentityMismatch {
                name,
                expected,
                observed: observed_start_time,
            };
        }

        let name = self
            .by_pid
            .remove(&pid)
            .expect("pending entry was checked above")
            .name;
        PendingClaim::Claimed(name)
    }

    pub(crate) fn remove_exited(&mut self, pid: u32) -> Option<String> {
        self.by_pid.remove(&pid).map(|pending| pending.name)
    }

    pub(crate) fn expire(&mut self, now: Instant) -> Vec<(u32, String)> {
        let expired = self
            .by_pid
            .iter()
            .filter_map(|(&pid, pending)| {
                (now >= pending.deadline).then(|| (pid, pending.name.clone()))
            })
            .collect::<Vec<_>>();
        for (pid, _) in &expired {
            self.by_pid.remove(pid);
        }
        expired
    }

    #[must_use]
    pub(crate) fn snapshots(&self, now: Instant) -> Vec<PendingScratchpadSnapshot> {
        let mut snapshots = self
            .by_pid
            .iter()
            .filter_map(|(&pid, pending)| {
                let remaining = pending.deadline.checked_duration_since(now)?;
                if remaining.is_zero() {
                    return None;
                }
                let remaining_ms = u64::try_from(remaining.as_millis())
                    .unwrap_or(u64::MAX)
                    .max(1);
                Some(PendingScratchpadSnapshot {
                    name: pending.name.clone(),
                    pid,
                    process_start_time: pending.process_start_time,
                    remaining_ms,
                })
            })
            .collect::<Vec<_>>();
        snapshots.sort_unstable_by_key(|pending| pending.pid);
        snapshots
    }

    #[must_use]
    #[cfg(test)]
    pub(crate) fn names_are_unique(&self) -> bool {
        let mut names = HashSet::with_capacity(self.by_pid.len());
        self.by_pid
            .values()
            .all(|pending| names.insert(&pending.name))
    }
}

/// Parse Linux `/proc/<pid>/stat` field 22 (`starttime`). The executable name
/// is parenthesized and may itself contain spaces or `)`, so split only after
/// the final `) ` delimiter rather than tokenizing the whole line.
#[must_use]
#[cfg(test)]
pub(crate) fn parse_linux_proc_stat_start_time(stat: &str) -> Option<u64> {
    let fields = stat.rsplit_once(") ")?.1;
    fields.split_whitespace().nth(19)?.parse().ok()
}

fn parse_linux_proc_stat_identity(stat: &str) -> Option<(char, u64)> {
    let fields = stat.rsplit_once(") ")?.1;
    let mut fields = fields.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let start_time = fields.nth(18)?.parse().ok()?;
    Some((state, start_time))
}

#[must_use]
pub(crate) fn linux_process_start_time(pid: u32) -> Option<u64> {
    if pid == 0 {
        return None;
    }
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| parse_linux_proc_stat_identity(&stat))
        .filter(|(state, _)| !matches!(state, 'Z' | 'X' | 'x'))
        .map(|(_, start_time)| start_time)
        .filter(|&start_time| start_time != 0)
}

impl Jwm {
    pub(crate) fn expire_pending_scratchpads(&mut self, now: Instant) {
        for (pid, name) in self.scratchpad_pending.expire(now) {
            warn!(
                "[scratchpad-pending] {:?} for PID {} timed out; a later toggle may retry",
                name, pid
            );
        }
    }

    pub(crate) fn claim_pending_scratchpad(&mut self, pid: u32, now: Instant) -> Option<String> {
        if !self.scratchpad_pending.contains_pid(pid) {
            if !self.scratchpad_pending.is_empty() {
                info!(
                    "[scratchpad-pending] window PID {} matches no pending direct child; not guessing by title/class",
                    pid
                );
            }
            return None;
        }
        let observed_start_time = linux_process_start_time(pid);
        match self.scratchpad_pending.claim(pid, observed_start_time, now) {
            PendingClaim::Claimed(name) => {
                info!(
                    "[scratchpad-pending] claimed {:?} from exact child PID {}",
                    name, pid
                );
                Some(name)
            }
            PendingClaim::Expired(name) => {
                warn!(
                    "[scratchpad-pending] {:?} for PID {} expired before its window mapped",
                    name, pid
                );
                None
            }
            PendingClaim::ProcessIdentityMismatch {
                name,
                expected,
                observed,
            } => {
                warn!(
                    "[scratchpad-pending] refusing {:?} for recycled/unverifiable PID {} (starttime expected {}, observed {:?})",
                    name, pid, expected, observed
                );
                None
            }
            PendingClaim::NoMatch => None,
        }
    }

    pub(crate) fn remove_exited_pending_scratchpad(&mut self, pid: u32) {
        if let Some(name) = self.scratchpad_pending.remove_exited(pid) {
            warn!(
                "[scratchpad-pending] direct child PID {} for {:?} exited before an exact window match; forked descendants will not be guessed",
                pid, name
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_stat_parser_handles_spaces_and_closing_parentheses_in_comm() {
        let stat = "123 (odd ) process) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 424242 20";
        assert_eq!(parse_linux_proc_stat_start_time(stat), Some(424242));
        assert_eq!(parse_linux_proc_stat_identity(stat), Some(('S', 424242)));
        let zombie = stat.replacen(" S ", " Z ", 1);
        assert_eq!(parse_linux_proc_stat_identity(&zombie), Some(('Z', 424242)));
        assert_eq!(parse_linux_proc_stat_start_time("malformed"), None);
        assert!(linux_process_start_time(std::process::id()).is_some());
    }

    #[test]
    fn same_name_is_pending_once_but_two_names_can_start_in_parallel() {
        let now = Instant::now();
        let mut registry = ScratchpadPendingRegistry::default();
        registry
            .register_spawned(101, "term".into(), Some(1001), now)
            .unwrap();

        assert!(matches!(
            registry.ensure_name_can_spawn("term"),
            Err(PendingRegistrationError::DuplicateName { pid: 101, .. })
        ));
        registry
            .register_spawned(102, "music".into(), Some(1002), now)
            .unwrap();
        assert_eq!(registry.len(), 2);
        assert!(registry.names_are_unique());
    }

    #[test]
    fn only_matching_pid_and_process_identity_consumes_a_name() {
        let now = Instant::now();
        let mut registry = ScratchpadPendingRegistry::default();
        registry
            .register_spawned(101, "term".into(), Some(1001), now)
            .unwrap();

        assert_eq!(registry.claim(999, Some(1001), now), PendingClaim::NoMatch);
        assert_eq!(registry.len(), 1, "unrelated window must not consume it");
        assert_eq!(
            registry.claim(101, Some(1001), now),
            PendingClaim::Claimed("term".into())
        );
        assert!(registry.is_empty());
    }

    #[test]
    fn pid_reuse_is_rejected_when_start_time_changes() {
        let now = Instant::now();
        let mut registry = ScratchpadPendingRegistry::default();
        registry
            .register_spawned(101, "term".into(), Some(1001), now)
            .unwrap();

        assert_eq!(
            registry.claim(101, Some(2002), now),
            PendingClaim::ProcessIdentityMismatch {
                name: "term".into(),
                expected: 1001,
                observed: Some(2002),
            }
        );
        assert!(registry.is_empty());
    }

    #[test]
    fn dead_and_timed_out_entries_are_removed_and_can_retry() {
        let now = Instant::now();
        let mut registry = ScratchpadPendingRegistry::default();
        registry
            .register_spawned(101, "dead".into(), Some(1001), now)
            .unwrap();
        assert_eq!(registry.remove_exited(101), Some("dead".into()));
        assert!(registry.ensure_name_can_spawn("dead").is_ok());

        registry
            .register_with_remaining(
                102,
                "slow".into(),
                Some(1002),
                now,
                Duration::from_millis(10),
            )
            .unwrap();
        assert_eq!(
            registry.expire(now + Duration::from_millis(10)),
            vec![(102, "slow".into())]
        );
        assert!(registry.ensure_name_can_spawn("slow").is_ok());
    }

    #[test]
    fn missing_start_time_gets_only_the_short_fallback_deadline() {
        let now = Instant::now();
        let mut registry = ScratchpadPendingRegistry::default();
        registry
            .register_spawned(101, "term".into(), None, now)
            .unwrap();
        let snapshot = registry.snapshots(now).pop().unwrap();
        assert!(snapshot.remaining_ms <= MAX_UNVERIFIED_PENDING_REMAINING_MS);
        assert_eq!(
            registry.claim(101, Some(9999), now),
            PendingClaim::Claimed("term".into()),
            "the documented fallback is exact PID plus a short deadline"
        );
    }

    #[test]
    fn registry_capacity_is_a_hard_pre_spawn_bound() {
        let now = Instant::now();
        let mut registry = ScratchpadPendingRegistry::default();
        for index in 0..MAX_PENDING_SCRATCHPADS {
            registry
                .register_spawned(
                    index as u32 + 1,
                    format!("scratch-{index}"),
                    Some(index as u64 + 1),
                    now,
                )
                .unwrap();
        }
        assert_eq!(registry.len(), MAX_PENDING_SCRATCHPADS);
        assert_eq!(
            registry.ensure_name_can_spawn("one-too-many"),
            Err(PendingRegistrationError::Full)
        );
    }
}
