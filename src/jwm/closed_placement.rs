//! Where a closed window goes when it comes back.
//!
//! JWM's default puts every new window on the selected monitor's active
//! tags: wherever the pointer happens to be. That is right for a window the
//! user just asked for with a keybinding or the launcher, and wrong for most
//! of the rest: a browser reopened from a shell, a viewer an agent spawns
//! inside a terminal, an application's own second top-level. Those belong
//! where the same application was last closed.
//!
//! Two bounded registries implement that:
//!
//! - [`ClosedPlacementMemory`] remembers, per WM_CLASS identity, the output
//!   (connector / stable key, with monitor number as fallback) and tag mask a
//!   regular client held when it was unmanaged, and persists that map beside
//!   the session snapshot so reopen-from-shell and agent-spawn placement
//!   survive a WM restart and hotplug renumbering. [`JwmLaunchRegistry`] stays
//!   process-only: spawn attribution is only meaningful for the current run.
//! - [`JwmLaunchRegistry`] records every child process JWM spawns itself, so
//!   a new window can be attributed either to an explicit keybinding,
//!   launcher, scratchpad or shell-panel action (which keeps the default
//!   placement) or to anything else (which gets the memory).
//!
//! Attribution walks the window's process ancestry. A managed client's PID
//! wins over a JWM spawn record on the same chain, so an application started
//! from inside a terminal that JWM spawned still counts as launched from a
//! window. A window with no PID, or whose chain reaches nothing JWM knows,
//! is blamed on the newest unclaimed JWM spawn only while that spawn is a
//! few seconds old; D-Bus-activated applications and PID-less Wayland
//! surfaces have no better evidence.
//!
//! A window the memory sends somewhere other than the focused window's
//! monitor and tag never pulls focus or the view along: it is laid out
//! where it belongs, pre-selected on its own tag, and marked urgent so the
//! bar and its border say where it went.

use crate::Jwm;
use crate::backend::api::Backend;
use crate::config::CONFIG;
use crate::core::models::ClientKey;
use crate::jwm::rules::RuleMatcher;
use crate::jwm::scratchpad_pending::linux_process_start_time;
use crate::jwm::session::{ensure_private_directory, session_file_path};
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Distinct WM_CLASS identities the memory keeps before evicting the oldest.
pub(crate) const MAX_REMEMBERED_PLACEMENTS: usize = 256;
/// JWM-spawned PIDs kept for attribution before evicting the oldest.
pub(crate) const MAX_LAUNCH_RECORDS: usize = 256;
/// How long a JWM spawn record stays matchable by exact PID. Generous, so a
/// slow-starting application launched from a keybinding is still recognised
/// when its first window finally maps.
pub(crate) const LAUNCH_RECORD_TTL: Duration = Duration::from_secs(600);
/// How long an unclaimed JWM spawn may explain a window whose process chain
/// gives no answer of its own.
pub(crate) const UNRESOLVED_LAUNCH_ATTRIBUTION_WINDOW: Duration = Duration::from_secs(10);
/// Ancestors examined above a window's own PID.
pub(crate) const MAX_ANCESTRY_DEPTH: usize = 16;

/// Current on-disk schema. v1 stored bare `monitor_num`; v2 adds an optional
/// `connector` (an [`OutputIdentity::stable_key`], falling back to the
/// connector name) so hole-fill renumbering after hotplug does not send a
/// reopen to the wrong output.
const CLOSED_PLACEMENT_VERSION: u32 = 2;
const CLOSED_PLACEMENT_MIN_SUPPORTED_VERSION: u32 = 1;
const MAX_CLOSED_PLACEMENT_BYTES: u64 = 1024 * 1024;
const MAX_IDENTITY_FIELD_BYTES: usize = 65_536;
const CLOSED_PLACEMENT_FILE: &str = "closed_placement.json";
/// Temporaries are `<prefix><pid>-<sequence>`: unique per writer, so a crash
/// between create and rename leaves one behind that nothing would ever reuse
/// or delete without the sweep in `atomic_write_closed_placement`.
const CLOSED_PLACEMENT_TEMPORARY_PREFIX: &str = ".closed_placement.json.tmp-";
const MAX_CLOSED_PLACEMENT_SWEEP_ENTRIES: usize = 1024;
const MAX_CLOSED_PLACEMENT_TEMPORARY_CREATE_ATTEMPTS: usize = 128;
static CLOSED_PLACEMENT_WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// WM_CLASS identity a placement is remembered under. Exact, case-sensitive.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PlacementIdentity {
    class: String,
    instance: String,
}

impl PlacementIdentity {
    /// `None` when both halves are empty: an anonymous window has nothing to
    /// be recognised by later.
    #[must_use]
    pub(crate) fn new(class: &str, instance: &str) -> Option<Self> {
        if class.is_empty() && instance.is_empty() {
            return None;
        }
        Some(Self {
            class: class.to_owned(),
            instance: instance.to_owned(),
        })
    }
}

impl std::fmt::Display for PlacementIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.class, self.instance)
    }
}

/// Monitor and tags a client held when it was closed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RememberedPlacement {
    pub monitor_num: i32,
    /// Prefer this over [`Self::monitor_num`] when resolving: the output's
    /// [`crate::backend::api::OutputIdentity::stable_key`] (or connector
    /// name). Absent for pre-v2 snapshots and when the output map had no
    /// identity at close time.
    pub connector: Option<String>,
    pub tags: u32,
    /// Monotonic close order; the smallest seq is evicted first when full.
    closed_seq: u64,
}

/// Versioned on-disk snapshot of [`ClosedPlacementMemory`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ClosedPlacementSnapshot {
    version: u32,
    placements: Vec<ClosedPlacementEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ClosedPlacementEntry {
    class: String,
    instance: String,
    monitor_num: i32,
    /// Output identity key; see [`RememberedPlacement::connector`]. Omitted
    /// in v1 files and when unknown at close time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    connector: Option<String>,
    tags: u32,
    seq: u64,
}

impl ClosedPlacementSnapshot {
    fn from_memory(memory: &ClosedPlacementMemory) -> Self {
        let mut placements: Vec<ClosedPlacementEntry> = memory
            .by_identity
            .iter()
            .map(|(identity, placement)| ClosedPlacementEntry {
                class: identity.class.clone(),
                instance: identity.instance.clone(),
                monitor_num: placement.monitor_num,
                connector: placement.connector.clone(),
                tags: placement.tags,
                seq: placement.closed_seq,
            })
            .collect();
        placements.sort_by_key(|entry| entry.seq);
        Self {
            version: CLOSED_PLACEMENT_VERSION,
            placements,
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.version < CLOSED_PLACEMENT_MIN_SUPPORTED_VERSION
            || self.version > CLOSED_PLACEMENT_VERSION
        {
            return Err(format!(
                "unsupported closed-placement version {}",
                self.version
            ));
        }
        if self.placements.len() > MAX_REMEMBERED_PLACEMENTS {
            return Err(format!(
                "closed-placement snapshot has {} entries; limit is {MAX_REMEMBERED_PLACEMENTS}",
                self.placements.len()
            ));
        }
        let mut seen = HashSet::with_capacity(self.placements.len());
        for (index, entry) in self.placements.iter().enumerate() {
            if entry.class.is_empty() && entry.instance.is_empty() {
                return Err(format!(
                    "closed-placement entry {index} has an empty identity"
                ));
            }
            if entry.class.len() > MAX_IDENTITY_FIELD_BYTES
                || entry.instance.len() > MAX_IDENTITY_FIELD_BYTES
            {
                return Err(format!(
                    "closed-placement entry {index} has oversized text fields"
                ));
            }
            if entry
                .connector
                .as_ref()
                .is_some_and(|connector| connector.len() > MAX_IDENTITY_FIELD_BYTES)
            {
                return Err(format!(
                    "closed-placement entry {index} has an oversized connector"
                ));
            }
            if entry.tags == 0 {
                return Err(format!(
                    "closed-placement entry {index} has an empty tag mask"
                ));
            }
            let identity = PlacementIdentity {
                class: entry.class.clone(),
                instance: entry.instance.clone(),
            };
            if !seen.insert(identity) {
                return Err(format!(
                    "closed-placement entry {index} duplicates an earlier identity"
                ));
            }
        }
        Ok(())
    }

    fn into_memory(self) -> Result<ClosedPlacementMemory, String> {
        self.validate()?;
        let mut by_identity = HashMap::with_capacity(self.placements.len());
        let mut next_seq = 0_u64;
        for entry in self.placements {
            next_seq = next_seq.max(entry.seq.saturating_add(1));
            let connector = entry.connector.filter(|connector| !connector.is_empty());
            by_identity.insert(
                PlacementIdentity {
                    class: entry.class,
                    instance: entry.instance,
                },
                RememberedPlacement {
                    monitor_num: entry.monitor_num,
                    connector,
                    tags: entry.tags,
                    closed_seq: entry.seq,
                },
            );
        }
        Ok(ClosedPlacementMemory {
            by_identity,
            next_seq,
        })
    }

    fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }
}

#[derive(Debug, Default)]
pub(crate) struct ClosedPlacementMemory {
    by_identity: HashMap<PlacementIdentity, RememberedPlacement>,
    next_seq: u64,
}

/// Path of the closed-placement snapshot: same XDG state directory as
/// [`session_file_path`], file name `closed_placement.json`.
#[must_use]
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn closed_placement_file_path() -> PathBuf {
    session_file_path().with_file_name(CLOSED_PLACEMENT_FILE)
}

fn process_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return true;
    };
    if pid <= 0 {
        return true;
    }
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn orphaned_closed_placement_temporary(
    name: &str,
    own_pid: u32,
    process_alive: impl Fn(u32) -> bool,
) -> bool {
    let Some(rest) = name.strip_prefix(CLOSED_PLACEMENT_TEMPORARY_PREFIX) else {
        return false;
    };
    let Some((pid, _sequence)) = rest.split_once('-') else {
        return false;
    };
    let Ok(pid) = pid.parse::<u32>() else {
        return false;
    };
    pid != own_pid && !process_alive(pid)
}

fn sweep_orphaned_closed_placement_temporaries(parent: &Path) {
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    let own_pid = std::process::id();
    for entry in entries.flatten().take(MAX_CLOSED_PLACEMENT_SWEEP_ENTRIES) {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !orphaned_closed_placement_temporary(name, own_pid, process_alive) {
            continue;
        }
        let path = entry.path();
        if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
            let _ = fs::remove_file(&path);
        }
    }
}

fn atomic_write_closed_placement(path: &Path, contents: &[u8]) -> io::Result<()> {
    atomic_write_closed_placement_with_sync(path, contents, |parent, _temporary| {
        fs::File::open(parent)?.sync_all()
    })
}

fn atomic_write_closed_placement_with_sync(
    path: &Path,
    contents: &[u8],
    sync_directory: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    if contents.len() as u64 > MAX_CLOSED_PLACEMENT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "closed-placement snapshot exceeds the 1 MiB limit",
        ));
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    ensure_private_directory(parent)?;
    sweep_orphaned_closed_placement_temporaries(parent);

    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "refusing to replace closed-placement symlink: {}",
                path.display()
            ),
        ));
    }

    let (temporary, mut file) =
        create_closed_placement_temporary(parent, std::process::id(), || {
            CLOSED_PLACEMENT_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed)
        })?;
    let mut renamed = false;
    let result = (|| {
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        renamed = true;
        sync_directory(parent, &temporary)?;
        Ok(())
    })();
    // After rename this pathname is free for another writer to own.
    if result.is_err() && !renamed {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn create_closed_placement_temporary(
    parent: &Path,
    pid: u32,
    mut next_sequence: impl FnMut() -> u64,
) -> io::Result<(PathBuf, fs::File)> {
    for _ in 0..MAX_CLOSED_PLACEMENT_TEMPORARY_CREATE_ATTEMPTS {
        let temporary = parent.join(format!(
            "{CLOSED_PLACEMENT_TEMPORARY_PREFIX}{pid}-{}",
            next_sequence()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
        {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "could not create a unique closed-placement temporary after {MAX_CLOSED_PLACEMENT_TEMPORARY_CREATE_ATTEMPTS} attempts"
        ),
    ))
}

fn load_closed_placement_snapshot(
    path: &Path,
) -> Result<ClosedPlacementSnapshot, Box<dyn std::error::Error>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(format!(
            "closed-placement path is not a regular file: {}",
            path.display()
        )
        .into());
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(format!(
            "closed-placement file is owned by another user: {}",
            path.display()
        )
        .into());
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(format!(
            "closed-placement file is writable by another user or group: {}",
            path.display()
        )
        .into());
    }
    if metadata.len() > MAX_CLOSED_PLACEMENT_BYTES {
        return Err(format!(
            "closed-placement file exceeds the 1 MiB limit: {}",
            path.display()
        )
        .into());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_CLOSED_PLACEMENT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CLOSED_PLACEMENT_BYTES {
        return Err(format!(
            "closed-placement file exceeds the 1 MiB limit: {}",
            path.display()
        )
        .into());
    }
    let json = String::from_utf8(bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("closed-placement file is not valid UTF-8: {error}"),
        )
    })?;
    let snapshot = ClosedPlacementSnapshot::from_json(&json)?;
    snapshot.validate()?;
    Ok(snapshot)
}

/// Remove the on-disk snapshot. Missing is fine; other errors are logged.
fn clear_closed_placement_file(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => info!(
            "[closed-placement] removed persisted placements at {}",
            path.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => warn!(
            "[closed-placement] could not remove {}: {error}",
            path.display()
        ),
    }
}

impl ClosedPlacementMemory {
    #[must_use]
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.by_identity.len()
    }

    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.by_identity.is_empty()
    }

    /// Load from the default XDG path. Missing or unreadable → empty memory;
    /// a corrupt file must not keep the WM from starting.
    #[must_use]
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn load() -> Self {
        Self::load_from_path(&closed_placement_file_path())
    }

    /// Feature disabled at startup: delete any leftover snapshot and return
    /// empty memory so a later re-enable does not revive stale history.
    #[must_use]
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn load_disabled() -> Self {
        clear_closed_placement_file(&closed_placement_file_path());
        Self::default()
    }

    #[must_use]
    pub(crate) fn load_from_path(path: &Path) -> Self {
        match load_closed_placement_snapshot(path) {
            Ok(snapshot) => match snapshot.into_memory() {
                Ok(memory) => {
                    if !memory.is_empty() {
                        info!(
                            "[closed-placement] loaded {} placements from {}",
                            memory.by_identity.len(),
                            path.display()
                        );
                    }
                    memory
                }
                Err(error) => {
                    warn!(
                        "[closed-placement] ignoring invalid snapshot {}: {error}",
                        path.display()
                    );
                    Self::default()
                }
            },
            Err(error)
                if error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) =>
            {
                Self::default()
            }
            Err(error) => {
                warn!(
                    "[closed-placement] could not load {}: {error}; starting empty",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// Persist to the default XDG path. Failures are logged and dropped.
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn save(&self) {
        self.save_to_path(&closed_placement_file_path())
    }

    pub(crate) fn save_to_path(&self, path: &Path) {
        let snapshot = ClosedPlacementSnapshot::from_memory(self);
        let json = match snapshot.to_json() {
            Ok(json) => json,
            Err(error) => {
                warn!("[closed-placement] could not serialise placements: {error}");
                return;
            }
        };
        if let Err(error) = atomic_write_closed_placement(path, json.as_bytes()) {
            warn!(
                "[closed-placement] could not save {}: {error}",
                path.display()
            );
        }
    }

    /// Remember where `identity` was just closed. A later close of the same
    /// identity replaces the earlier one; the newest word wins. Returns
    /// `false` for a tag mask with nothing in it.
    ///
    /// `connector` is the output's stable identity key when known; apply
    /// prefers it over `monitor_num` so a hotplug renumber still finds the
    /// same physical output.
    pub(crate) fn remember(
        &mut self,
        identity: PlacementIdentity,
        monitor_num: i32,
        connector: Option<String>,
        tags: u32,
    ) -> bool {
        if tags == 0 {
            return false;
        }
        if !self.by_identity.contains_key(&identity)
            && self.by_identity.len() >= MAX_REMEMBERED_PLACEMENTS
        {
            let oldest = self
                .by_identity
                .iter()
                .min_by_key(|(_, placement)| placement.closed_seq)
                .map(|(identity, _)| identity.clone());
            if let Some(oldest) = oldest {
                self.by_identity.remove(&oldest);
            }
        }
        let closed_seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        let connector = connector.filter(|connector| !connector.is_empty());
        self.by_identity.insert(
            identity,
            RememberedPlacement {
                monitor_num,
                connector,
                tags,
                closed_seq,
            },
        );
        true
    }

    #[must_use]
    pub(crate) fn lookup(&self, identity: &PlacementIdentity) -> Option<RememberedPlacement> {
        self.by_identity.get(identity).cloned()
    }

    /// Drop everything in memory. Used when the feature is switched off so a
    /// later re-enable starts from what the user does next, not from stale
    /// history.
    pub(crate) fn clear(&mut self) {
        self.by_identity.clear();
        self.next_seq = 0;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LaunchRecord {
    process_start_time: Option<u64>,
    spawned_at: Instant,
    /// Set once a window has been attributed to this spawn. Exact chain
    /// matches keep the record (one spawn may map several windows); the
    /// unresolved fallback consumes it.
    claimed: bool,
}

#[derive(Debug, Default)]
pub(crate) struct JwmLaunchRegistry {
    by_pid: HashMap<u32, LaunchRecord>,
}

/// Why a new window is, or is not, entitled to the closed-placement memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LaunchOrigin {
    /// The process chain reaches a child JWM spawned: keybinding `spawn`,
    /// launcher, scratchpad, shell panel. The user pointed at where they
    /// wanted it by acting there.
    Jwm,
    /// The chain reaches another managed client's process first: launched
    /// from a terminal, by an agent inside one, or an application's own
    /// second window.
    ManagedWindow,
    /// Nothing on the chain is known to JWM: a tmux server, a daemon, an SSH
    /// session, a D-Bus service.
    External,
    /// No PID at all and no recent JWM spawn to blame.
    Unknown,
}

impl LaunchOrigin {
    #[must_use]
    pub(crate) fn uses_memory(self) -> bool {
        !matches!(self, Self::Jwm)
    }
}

/// How the process tree is read. Injected so the policy is testable without
/// a `/proc` that happens to contain the right processes.
pub(crate) struct ProcessProbe<'a> {
    /// Ancestor PIDs of a process, nearest first, never including itself.
    pub ancestors: &'a dyn Fn(u32) -> Vec<u32>,
    /// Kernel start time of a live process, when readable.
    pub start_time: &'a dyn Fn(u32) -> Option<u64>,
}

impl JwmLaunchRegistry {
    #[must_use]
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.by_pid.len()
    }

    /// Record a child JWM just spawned.
    pub(crate) fn record(&mut self, pid: u32, process_start_time: Option<u64>, now: Instant) {
        if pid == 0 {
            return;
        }
        self.expire(now);
        if !self.by_pid.contains_key(&pid) && self.by_pid.len() >= MAX_LAUNCH_RECORDS {
            let oldest = self
                .by_pid
                .iter()
                .min_by_key(|(_, record)| record.spawned_at)
                .map(|(pid, _)| *pid);
            if let Some(oldest) = oldest {
                self.by_pid.remove(&oldest);
            }
        }
        self.by_pid.insert(
            pid,
            LaunchRecord {
                process_start_time,
                spawned_at: now,
                claimed: false,
            },
        );
    }

    fn expire(&mut self, now: Instant) {
        self.by_pid.retain(|_, record| {
            now.saturating_duration_since(record.spawned_at) < LAUNCH_RECORD_TTL
        });
    }

    /// Whether `pid` is a JWM spawn. A recycled PID is rejected when both the
    /// record and the observation carry a start time and they differ.
    fn matches(&self, pid: u32, observed_start_time: Option<u64>) -> bool {
        let Some(record) = self.by_pid.get(&pid) else {
            return false;
        };
        match (record.process_start_time, observed_start_time) {
            (Some(expected), Some(observed)) => expected == observed,
            _ => true,
        }
    }

    fn claim(&mut self, pid: u32) {
        if let Some(record) = self.by_pid.get_mut(&pid) {
            record.claimed = true;
        }
    }

    /// Consume the newest spawn that has not produced a window yet, if it is
    /// recent enough to plausibly explain one that just appeared.
    fn claim_recent_unresolved(&mut self, now: Instant) -> Option<u32> {
        let pid = self
            .by_pid
            .iter()
            .filter(|(_, record)| {
                !record.claimed
                    && now.saturating_duration_since(record.spawned_at)
                        <= UNRESOLVED_LAUNCH_ATTRIBUTION_WINDOW
            })
            .max_by_key(|(_, record)| record.spawned_at)
            .map(|(pid, _)| *pid)?;
        self.claim(pid);
        Some(pid)
    }

    /// Attribute a new window. `managed_pids` are the processes of every
    /// other managed client; they are checked before spawn records at every
    /// step of the chain, so a window opened from inside a JWM-spawned
    /// terminal is the terminal's doing, not JWM's.
    pub(crate) fn classify(
        &mut self,
        pid: Option<u32>,
        managed_pids: &HashSet<u32>,
        probe: &ProcessProbe<'_>,
        now: Instant,
    ) -> LaunchOrigin {
        self.expire(now);
        let Some(pid) = pid else {
            return if self.claim_recent_unresolved(now).is_some() {
                LaunchOrigin::Jwm
            } else {
                LaunchOrigin::Unknown
            };
        };
        let mut chain = Vec::with_capacity(MAX_ANCESTRY_DEPTH + 1);
        chain.push(pid);
        chain.extend((probe.ancestors)(pid).into_iter().take(MAX_ANCESTRY_DEPTH));
        for candidate in chain {
            if managed_pids.contains(&candidate) {
                return LaunchOrigin::ManagedWindow;
            }
            if self.matches(candidate, (probe.start_time)(candidate)) {
                self.claim(candidate);
                return LaunchOrigin::Jwm;
            }
        }
        if self.claim_recent_unresolved(now).is_some() {
            LaunchOrigin::Jwm
        } else {
            LaunchOrigin::External
        }
    }
}

/// What the memory may still decide once the matching rule has spoken: a
/// rule that pins tags keeps its tags, a rule that pins a monitor keeps its
/// monitor, and the memory fills in only what the rule left open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MemoryPlacement {
    pub monitor_num: Option<i32>,
    pub tags: Option<u32>,
}

/// Pick the current monitor number for a remembered output identity.
///
/// When `remembered_connector` matches a live output's `stable_key` or
/// `connector`, return that output's current monitor number (so hole-fill
/// renumbering after hotplug still lands on the same panel). Otherwise fall
/// back to `remembered_monitor_num` for older snapshots and missing
/// identities. Shared by closed-placement memory and session restore.
#[must_use]
pub(crate) fn resolve_monitor_num_by_connector(
    remembered_monitor_num: i32,
    remembered_connector: Option<&str>,
    live: &[(i32, &str, &str)],
) -> i32 {
    if let Some(key) = remembered_connector {
        if let Some(&(num, _, _)) = live
            .iter()
            .find(|(_, connector, stable_key)| *connector == key || *stable_key == key)
        {
            return num;
        }
    }
    remembered_monitor_num
}

#[must_use]
pub(crate) fn resolve_memory_against_rule(
    remembered: &RememberedPlacement,
    rule: Option<&crate::jwm::types::WMRule>,
    tagmask: u32,
) -> MemoryPlacement {
    let monitor_pinned = rule.is_some_and(|rule| rule.monitor >= 0);
    let tags_pinned = rule.is_some_and(|rule| rule.tags > 0);
    let tags = remembered.tags & tagmask;
    MemoryPlacement {
        monitor_num: (!monitor_pinned).then_some(remembered.monitor_num),
        tags: (!tags_pinned && tags != 0).then_some(tags),
    }
}

/// Ancestor PIDs from `/proc/<pid>/status`, nearest first. Stops at PID 1,
/// on a parse failure, or after `MAX_ANCESTRY_DEPTH` steps.
fn linux_ancestors(pid: u32) -> Vec<u32> {
    ancestors_with(pid, linux_parent_pid)
}

fn valid_process_pid(pid: u32) -> bool {
    pid > 1 && pid <= i32::MAX as u32
}

fn ancestors_with(pid: u32, mut parent_of: impl FnMut(u32) -> Option<u32>) -> Vec<u32> {
    let mut out = Vec::with_capacity(MAX_ANCESTRY_DEPTH);
    if !valid_process_pid(pid) {
        return out;
    }
    let mut current = pid;
    for _ in 0..MAX_ANCESTRY_DEPTH {
        match parent_of(current) {
            Some(parent)
                if valid_process_pid(parent) && parent != pid && !out.contains(&parent) =>
            {
                out.push(parent);
                current = parent;
            }
            _ => break,
        }
    }
    out
}

const MAX_ANCESTRY_STATUS_BYTES: u64 = 256 * 1024;

fn parent_pid_from_status(input: impl Read) -> Option<u32> {
    let mut contents = String::new();
    input
        .take(MAX_ANCESTRY_STATUS_BYTES + 1)
        .read_to_string(&mut contents)
        .ok()?;
    if contents.len() as u64 > MAX_ANCESTRY_STATUS_BYTES {
        return None;
    }
    contents
        .lines()
        .find_map(|line| line.strip_prefix("PPid:"))
        .and_then(|rest| rest.trim().parse().ok())
        .filter(|&pid| pid <= i32::MAX as u32)
}

fn linux_parent_pid(pid: u32) -> Option<u32> {
    if !valid_process_pid(pid) {
        return None;
    }
    parent_pid_from_status(fs::File::open(format!("/proc/{pid}/status")).ok()?)
}

impl Jwm {
    /// Record a child JWM just spawned so the window it maps keeps the
    /// default placement. Every JWM-owned launch passes through here.
    pub(crate) fn note_jwm_launch(&mut self, pid: u32, now: Instant) {
        self.jwm_launches
            .record(pid, linux_process_start_time(pid), now);
    }

    fn launch_origin_of(&mut self, client_key: ClientKey, now: Instant) -> LaunchOrigin {
        let pid = self
            .state
            .clients
            .get(client_key)
            .and_then(|client| client.pid);
        let managed_pids: HashSet<u32> = self
            .state
            .clients
            .iter()
            .filter(|(key, _)| *key != client_key)
            .filter_map(|(_, client)| client.pid)
            .collect();
        let probe = ProcessProbe {
            ancestors: &linux_ancestors,
            start_time: &linux_process_start_time,
        };
        self.jwm_launches.classify(pid, &managed_pids, &probe, now)
    }

    /// After rules ran for a freshly managed top-level: mark it as one whose
    /// closing is worth remembering, and, when the memory has an entry for
    /// its identity and JWM did not launch it, move it there. Rule-pinned
    /// tags or monitor are left alone. Returns whether anything moved, so
    /// the caller knows the window may have left the user's view.
    pub(crate) fn adopt_remembered_placement(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> bool {
        let Some((win, name, class, instance)) = self.state.clients.get(client_key).map(|client| {
            (
                client.win,
                client.name.clone(),
                client.class.clone(),
                client.instance.clone(),
            )
        }) else {
            return false;
        };
        let types = backend.property_ops().get_window_types(win);
        if RuleMatcher::types_are_popup_like(&types)
            || RuleMatcher::is_structurally_borderless(&types)
        {
            return false;
        }
        let Some(identity) = PlacementIdentity::new(&class, &instance) else {
            return false;
        };
        if let Some(client) = self.state.clients.get_mut(client_key) {
            client.state.remembers_closed_placement = true;
        }

        let cfg = CONFIG.load();
        if !cfg.behavior().remember_closed_placement {
            return false;
        }
        let Some(remembered) = self.closed_placements.lookup(&identity) else {
            return false;
        };
        let origin = self.launch_origin_of(client_key, Instant::now());
        if !origin.uses_memory() {
            info!(
                "[closed-placement] {win:?} ({identity}) was launched by JWM; keeping the default placement"
            );
            return false;
        }

        let mut remembered = remembered;
        remembered.monitor_num = self.resolve_remembered_monitor_num(backend, &remembered);

        let rule = RuleMatcher::find_matching_rule(&name, &class, &instance);
        let placement = resolve_memory_against_rule(&remembered, rule.as_ref(), cfg.tagmask());
        // A monitor that has gone away keeps the client on the selected one;
        // the remembered tags still apply there, because tags are the
        // user's workspaces and travel with them across outputs.
        let monitor = placement
            .monitor_num
            .and_then(|num| self.get_monitor_by_id(num));

        let mut moved = false;
        if let Some(client) = self.state.clients.get_mut(client_key) {
            if let Some(mon_key) = monitor
                && client.mon != Some(mon_key)
            {
                client.mon = Some(mon_key);
                moved = true;
            }
            if let Some(tags) = placement.tags
                && client.state.tags != tags
            {
                client.state.tags = tags;
                moved = true;
            }
        }
        if moved {
            info!(
                "[closed-placement] {win:?} ({identity}) returns to monitor {:?} tags {:?} ({origin:?})",
                placement.monitor_num, placement.tags
            );
        } else {
            debug!(
                "[closed-placement] {win:?} ({identity}) already sits where it was closed ({origin:?})"
            );
        }
        moved
    }

    /// Live outputs as `(monitor_num, connector, stable_key)`, joined through
    /// `output_map`. Shared by closed-placement apply and session restore.
    pub(crate) fn live_monitor_identities(
        &self,
        backend: &dyn Backend,
    ) -> Vec<(i32, String, String)> {
        backend
            .output_ops()
            .enumerate_outputs()
            .into_iter()
            .filter_map(|output| {
                let mon_key = self
                    .state
                    .output_map
                    .iter()
                    .find(|(_, id)| **id == output.id)
                    .map(|(key, _)| key)?;
                let num = self.state.monitors.get(mon_key)?.num;
                Some((num, output.identity.connector, output.identity.stable_key))
            })
            .collect()
    }

    /// Map a remembered output identity to the monitor number that currently
    /// owns that output. Falls back to the saved `monitor_num` when the
    /// connector is missing or no longer connected.
    fn resolve_remembered_monitor_num(
        &self,
        backend: &dyn Backend,
        remembered: &RememberedPlacement,
    ) -> i32 {
        let live = self.live_monitor_identities(backend);
        let live_refs: Vec<(i32, &str, &str)> = live
            .iter()
            .map(|(num, connector, stable_key)| (*num, connector.as_str(), stable_key.as_str()))
            .collect();
        resolve_monitor_num_by_connector(
            remembered.monitor_num,
            remembered.connector.as_deref(),
            &live_refs,
        )
    }

    /// Stable identity key for the output currently backing `mon_key`.
    pub(crate) fn output_key_for_monitor(
        &self,
        backend: &dyn Backend,
        mon_key: crate::core::models::MonitorKey,
    ) -> Option<String> {
        self.live_output_identity(backend, mon_key)
            .and_then(|identity| {
                let key = if !identity.stable_key.is_empty() {
                    identity.stable_key
                } else {
                    identity.connector
                };
                (!key.is_empty()).then_some(key)
            })
    }

    /// Physical connector name (`OutputIdentity.connector`) for the live
    /// output backing `mon_key`. Distinct from [`Self::output_key_for_monitor`]
    /// when `stable_key` is an EDID-derived identity.
    pub(crate) fn output_connector_for_monitor(
        &self,
        backend: &dyn Backend,
        mon_key: crate::core::models::MonitorKey,
    ) -> Option<String> {
        self.live_output_identity(backend, mon_key)
            .map(|identity| identity.connector)
            .filter(|name| !name.is_empty())
    }

    /// EDID monitor name for the output currently backing `mon_key`.
    pub(crate) fn output_monitor_name_for_monitor(
        &self,
        backend: &dyn Backend,
        mon_key: crate::core::models::MonitorKey,
    ) -> Option<String> {
        self.live_output_identity(backend, mon_key)
            .and_then(|identity| identity.monitor_name)
            .filter(|name| !name.is_empty())
    }

    /// Backend / wl_output name for the live output backing `mon_key`.
    pub(crate) fn output_name_for_monitor(
        &self,
        backend: &dyn Backend,
        mon_key: crate::core::models::MonitorKey,
    ) -> Option<String> {
        self.live_output_info(backend, mon_key)
            .map(|output| output.name)
            .filter(|name| !name.is_empty())
    }

    /// Backend output id (`OutputInfo.id`) for the live output backing
    /// `mon_key`.
    pub(crate) fn output_id_for_monitor(
        &self,
        backend: &dyn Backend,
        mon_key: crate::core::models::MonitorKey,
    ) -> Option<u64> {
        self.live_output_info(backend, mon_key)
            .map(|output| output.id.0)
    }

    /// EDID vendor / product / serial fields for the live output backing
    /// `mon_key`.
    pub(crate) fn output_edid_ids_for_monitor(
        &self,
        backend: &dyn Backend,
        mon_key: crate::core::models::MonitorKey,
    ) -> (Option<String>, Option<u16>, Option<u32>, Option<String>) {
        let Some(identity) = self.live_output_identity(backend, mon_key) else {
            return (None, None, None, None);
        };
        (
            identity.vendor.filter(|v| !v.is_empty()),
            identity.product_code,
            identity.serial_number,
            identity.monitor_serial.filter(|s| !s.is_empty()),
        )
    }

    /// VRR supported / currently enabled / min·max Hz for the live output
    /// backing `mon_key`. All zeros/false when the output map has no entry or
    /// the backend reports no capabilities.
    pub(crate) fn output_vrr_for_monitor(
        &self,
        backend: &dyn Backend,
        mon_key: crate::core::models::MonitorKey,
    ) -> (bool, bool, u32, u32) {
        let Some(output) = self.live_output_info(backend, mon_key) else {
            return (false, false, 0, 0);
        };
        backend
            .query_vrr_capabilities(output.id)
            .map(|caps| {
                (
                    caps.supported,
                    caps.current_enabled,
                    caps.min_refresh_hz,
                    caps.max_refresh_hz,
                )
            })
            .unwrap_or((false, false, 0, 0))
    }

    /// Fractional scale and mode refresh (mHz) for the live output backing
    /// `mon_key`. Defaults to `(1.0, 0)` when the output map has no entry.
    pub(crate) fn output_scale_refresh_for_monitor(
        &self,
        backend: &dyn Backend,
        mon_key: crate::core::models::MonitorKey,
    ) -> (f32, u32) {
        self.live_output_info(backend, mon_key)
            .map(|output| (output.scale, output.refresh_rate))
            .unwrap_or((1.0, 0))
    }

    /// Whether the live output backing `mon_key` advertised HDR capability.
    pub(crate) fn output_hdr_capable_for_monitor(
        &self,
        backend: &dyn Backend,
        mon_key: crate::core::models::MonitorKey,
    ) -> bool {
        self.live_output_info(backend, mon_key)
            .is_some_and(|output| output.hdr_capable)
    }

    /// `wl_output` transform (`0..=7`) for the live output backing `mon_key`.
    /// Defaults to `0` (normal) when the output map has no entry.
    pub(crate) fn output_transform_for_monitor(
        &self,
        backend: &dyn Backend,
        mon_key: crate::core::models::MonitorKey,
    ) -> i32 {
        self.live_output_info(backend, mon_key)
            .map(|output| output.transform)
            .unwrap_or(0)
    }

    /// Physical panel size in millimetres for the live output; `(0, 0)` when
    /// unknown.
    pub(crate) fn output_physical_mm_for_monitor(
        &self,
        backend: &dyn Backend,
        mon_key: crate::core::models::MonitorKey,
    ) -> (i32, i32) {
        self.live_output_info(backend, mon_key)
            .map(|output| (output.physical_width_mm, output.physical_height_mm))
            .unwrap_or((0, 0))
    }

    /// Preferred mode `(w, h, refresh_mhz)` for the live output; zeros when
    /// unknown.
    pub(crate) fn output_preferred_mode_for_monitor(
        &self,
        backend: &dyn Backend,
        mon_key: crate::core::models::MonitorKey,
    ) -> (i32, i32, u32) {
        self.live_output_info(backend, mon_key)
            .map(|output| {
                (
                    output.preferred_width,
                    output.preferred_height,
                    output.preferred_refresh_mhz,
                )
            })
            .unwrap_or((0, 0, 0))
    }

    /// EDID HDR static metadata subset for the live output, when advertised.
    pub(crate) fn output_hdr_metadata_for_monitor(
        &self,
        backend: &dyn Backend,
        mon_key: crate::core::models::MonitorKey,
    ) -> Option<crate::ipc::HdrMetadataIpc> {
        self.live_output_info(backend, mon_key)
            .and_then(|output| output.hdr_metadata)
            .map(|m| crate::ipc::HdrMetadataIpc {
                max_luminance_nits: m.max_luminance_nits,
                min_luminance_nits: m.min_luminance_nits,
                max_frame_average_nits: m.max_frame_average_nits,
                supports_pq: m.supports_pq,
                supports_hlg: m.supports_hlg,
                supports_bt2020: m.supports_bt2020,
            })
    }

    fn live_output_info(
        &self,
        backend: &dyn Backend,
        mon_key: crate::core::models::MonitorKey,
    ) -> Option<crate::backend::api::OutputInfo> {
        let output_id = *self.state.output_map.get(mon_key)?;
        backend
            .output_ops()
            .enumerate_outputs()
            .into_iter()
            .find(|output| output.id == output_id)
    }

    fn live_output_identity(
        &self,
        backend: &dyn Backend,
        mon_key: crate::core::models::MonitorKey,
    ) -> Option<crate::backend::api::OutputIdentity> {
        self.live_output_info(backend, mon_key)
            .map(|output| output.identity)
    }

    /// Whether a client sits on the selected monitor and inside its current
    /// view: where the user is looking right now.
    pub(crate) fn shares_focused_view(&self, client_key: ClientKey) -> bool {
        self.state
            .clients
            .get(client_key)
            .is_some_and(|client| client.mon.is_some() && client.mon == self.state.sel_mon)
            && self.is_client_visible_by_key(client_key)
    }

    /// A memory-placed client that landed on another monitor or tag than
    /// the focused window. It is laid out where it belongs and pre-selected
    /// on its own tag, so viewing that tag opens on it; it is marked urgent
    /// so the bar and its border say where it went; and focus stays exactly
    /// where it was, whatever `behavior.focus_follows_new_window` says for
    /// windows the user launched here. Do Not Disturb keeps the urgent cue
    /// quiet like every other attention request.
    pub(crate) fn settle_remembered_placement_elsewhere(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some((mon_key, tags, win)) = self
            .state
            .clients
            .get(client_key)
            .and_then(|client| Some((client.mon?, client.state.tags, client.win)))
        else {
            return Ok(());
        };
        let previous_selection = self.get_selected_client_key();

        if let Some(monitor) = self.state.monitors.get_mut(mon_key) {
            monitor.set_selected_client_for_tag_mask(tags, Some(client_key));
        }
        self.arrange(backend, Some(mon_key));
        self.broadcast_monitor_bar_ipc(backend, mon_key);
        if self.do_not_disturb {
            debug!(
                "[closed-placement] {win:?} landed away from the focused view; DND keeps it quiet"
            );
        } else if let Err(error) = self.seturgent(backend, client_key, true) {
            warn!("[closed-placement] could not mark {win:?} urgent: {error}");
        }

        // Mapping the window must not have moved focus. `focus()` rather
        // than the bare set-focus step: it also writes the monitor
        // selection back, which the focus stack shuffle would otherwise
        // leave pointing at whatever was next.
        self.focus(backend, previous_selection)?;
        info!(
            "[closed-placement] {win:?} placed away from the focused view on monitor {:?} tags {tags:#b}; focus left alone",
            self.state.monitors.get(mon_key).map(|monitor| monitor.num)
        );
        Ok(())
    }

    /// A regular client is going away: remember where it was.
    pub(crate) fn remember_closed_placement(
        &mut self,
        backend: &dyn Backend,
        client_key: ClientKey,
    ) {
        let cfg = CONFIG.load();
        if !cfg.behavior().remember_closed_placement {
            return;
        }
        let Some(client) = self.state.clients.get(client_key) else {
            return;
        };
        if !client.state.remembers_closed_placement
            || client.state.is_dock
            || client.state.is_sticky
        {
            return;
        }
        if self.scratchpads.values().any(|&key| key == client_key) {
            return;
        }
        let Some(identity) = PlacementIdentity::new(&client.class, &client.instance) else {
            return;
        };
        let tags = client.state.tags & cfg.tagmask();
        let Some(mon_key) = client.mon else {
            return;
        };
        let Some(monitor_num) = self.state.monitors.get(mon_key).map(|monitor| monitor.num) else {
            return;
        };
        let connector = self.output_key_for_monitor(backend, mon_key);
        let win = client.win;
        if self
            .closed_placements
            .remember(identity.clone(), monitor_num, connector.clone(), tags)
        {
            match connector.as_deref() {
                Some(connector) => info!(
                    "[closed-placement] {win:?} ({identity}) closed on {connector} (monitor {monitor_num}) tags {tags:#b}"
                ),
                None => info!(
                    "[closed-placement] {win:?} ({identity}) closed on monitor {monitor_num} tags {tags:#b}"
                ),
            }
            // Lib tests exercise remember through unmanage; they must not
            // write the developer's XDG state directory.
            #[cfg(not(test))]
            self.closed_placements.save();
        } else {
            warn!("[closed-placement] {win:?} ({identity}) closed without a tag; not remembered");
        }
    }

    /// Config reload: a feature switched off forgets what it learned, including
    /// the on-disk snapshot, so a later re-enable starts empty.
    pub(crate) fn reconcile_closed_placement_config(&mut self, enabled: bool) {
        if enabled {
            return;
        }
        if !self.closed_placements.is_empty() {
            self.closed_placements.clear();
            info!("[closed-placement] disabled; forgetting remembered placements");
        }
        #[cfg(not(test))]
        {
            // Delete any leftover file while the feature is off, whether or
            // not this process still held entries in memory.
            clear_closed_placement_file(&closed_placement_file_path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn identity(class: &str) -> PlacementIdentity {
        PlacementIdentity::new(class, class).expect("non-empty identity")
    }

    fn probe_with(
        tree: &'static [(u32, u32)],
        start_times: &'static [(u32, u64)],
    ) -> (impl Fn(u32) -> Vec<u32>, impl Fn(u32) -> Option<u64>) {
        let ancestors = move |pid: u32| {
            let mut out = Vec::new();
            let mut current = pid;
            for _ in 0..MAX_ANCESTRY_DEPTH {
                let Some(&(_, parent)) = tree.iter().find(|(child, _)| *child == current) else {
                    break;
                };
                if parent <= 1 {
                    break;
                }
                out.push(parent);
                current = parent;
            }
            out
        };
        let start_time = move |pid: u32| {
            start_times
                .iter()
                .find(|(candidate, _)| *candidate == pid)
                .map(|(_, start)| *start)
        };
        (ancestors, start_time)
    }

    #[test]
    fn ancestry_stops_before_cycles_invalid_pids_and_depth_limit() {
        assert_eq!(
            ancestors_with(10, |pid| Some(if pid == 10 { 20 } else { 10 })),
            vec![20]
        );
        assert_eq!(
            ancestors_with(10, |pid| Some(if pid == 10 { 20 } else { 20 })),
            vec![20]
        );
        assert!(ancestors_with(0, |_| panic!("invalid PID probed")).is_empty());
        assert!(ancestors_with(1, |_| panic!("PID 1 must not be probed")).is_empty());
        assert!(ancestors_with(10, |_| Some(0)).is_empty());
        assert!(ancestors_with(10, |_| Some(1)).is_empty());
        assert!(ancestors_with(10, |_| Some(u32::MAX)).is_empty());
        let chain = ancestors_with(100, |pid| Some(pid + 1));
        assert_eq!(chain.len(), MAX_ANCESTRY_DEPTH);
        assert_eq!(chain[0], 101);
    }

    #[test]
    fn ancestry_status_reader_rejects_oversized_and_invalid_input() {
        assert_eq!(
            parent_pid_from_status(&b"Name: app\nPPid: 42\n"[..]),
            Some(42)
        );
        assert_eq!(parent_pid_from_status(&b"PPid: 4294967295\n"[..]), None);
        assert_eq!(parent_pid_from_status(&b"PPid: -1\n"[..]), None);
        let mut oversized = b"PPid: 42\n".to_vec();
        oversized.resize(MAX_ANCESTRY_STATUS_BYTES as usize + 1, b' ');
        assert_eq!(parent_pid_from_status(oversized.as_slice()), None);
    }

    #[test]
    fn rename_failure_removes_unpublished_temporary() {
        let dir = TestDir::new("rename-failure");
        let destination = dir.0.join("existing-directory");
        fs::create_dir(&destination).unwrap();
        assert!(
            atomic_write_closed_placement_with_sync(&destination, b"snapshot", |_, _| {
                panic!("directory sync must follow a successful rename")
            })
            .is_err()
        );
        assert!(destination.is_dir());
        assert!(fs::read_dir(&dir.0).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(CLOSED_PLACEMENT_TEMPORARY_PREFIX)
        }));
    }

    #[test]
    fn directory_sync_failure_preserves_reused_temporary_path() {
        let dir = TestDir::new("post-rename-sync");
        let path = dir.0.join("snapshot.json");
        let mut replacement = None;
        let error =
            atomic_write_closed_placement_with_sync(&path, b"published", |_parent, temporary| {
                fs::write(temporary, b"new owner")?;
                replacement = Some(temporary.to_path_buf());
                Err(io::Error::other("directory sync failed"))
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(fs::read(&path).unwrap(), b"published");
        assert_eq!(fs::read(replacement.unwrap()).unwrap(), b"new owner");
    }

    #[test]
    fn anonymous_windows_have_no_identity() {
        assert!(PlacementIdentity::new("", "").is_none());
        assert!(PlacementIdentity::new("firefox", "").is_some());
        assert!(PlacementIdentity::new("", "Navigator").is_some());
        assert_ne!(
            PlacementIdentity::new("firefox", "Navigator"),
            PlacementIdentity::new("firefox", "Places")
        );
    }

    #[test]
    fn memory_keeps_the_newest_close_per_identity() {
        let mut memory = ClosedPlacementMemory::default();
        assert!(memory.remember(identity("firefox"), 1, Some("DP-2".into()), 0b100));
        assert!(memory.remember(identity("firefox"), 0, Some("DP-1".into()), 0b10));
        let placement = memory.lookup(&identity("firefox")).expect("remembered");
        assert_eq!((placement.monitor_num, placement.tags), (0, 0b10));
        assert_eq!(placement.connector.as_deref(), Some("DP-1"));
        assert_eq!(memory.len(), 1);
        assert!(memory.lookup(&identity("kitty")).is_none());
    }

    #[test]
    fn memory_rejects_an_empty_tag_mask() {
        let mut memory = ClosedPlacementMemory::default();
        assert!(!memory.remember(identity("firefox"), 0, None, 0));
        assert!(memory.is_empty());
    }

    #[test]
    fn memory_evicts_the_oldest_identity_when_full() {
        let mut memory = ClosedPlacementMemory::default();
        for index in 0..MAX_REMEMBERED_PLACEMENTS {
            let identity = PlacementIdentity::new(&format!("app{index}"), "").unwrap();
            assert!(memory.remember(identity, 0, None, 1));
        }
        assert_eq!(memory.len(), MAX_REMEMBERED_PLACEMENTS);
        assert!(memory.remember(identity("newest"), 0, None, 1));
        assert_eq!(memory.len(), MAX_REMEMBERED_PLACEMENTS);
        assert!(
            memory
                .lookup(&PlacementIdentity::new("app0", "").unwrap())
                .is_none()
        );
        assert!(
            memory
                .lookup(&PlacementIdentity::new("app1", "").unwrap())
                .is_some()
        );
        assert!(memory.lookup(&identity("newest")).is_some());
    }

    #[test]
    fn a_window_whose_own_pid_was_spawned_by_jwm_keeps_the_default_placement() {
        let now = Instant::now();
        let mut registry = JwmLaunchRegistry::default();
        registry.record(500, Some(7_000), now);
        let (ancestors, start_time) = probe_with(&[(500, 1)], &[(500, 7_000)]);
        let probe = ProcessProbe {
            ancestors: &ancestors,
            start_time: &start_time,
        };
        assert_eq!(
            registry.classify(Some(500), &HashSet::new(), &probe, now),
            LaunchOrigin::Jwm
        );
        assert!(!LaunchOrigin::Jwm.uses_memory());
    }

    #[test]
    fn a_wrapper_shell_between_jwm_and_the_window_is_still_a_jwm_launch() {
        let now = Instant::now();
        let mut registry = JwmLaunchRegistry::default();
        registry.record(500, None, now);
        // sh -c "app": the shell forked instead of exec'ing.
        let (ancestors, start_time) = probe_with(&[(501, 500), (500, 1)], &[]);
        let probe = ProcessProbe {
            ancestors: &ancestors,
            start_time: &start_time,
        };
        assert_eq!(
            registry.classify(Some(501), &HashSet::new(), &probe, now),
            LaunchOrigin::Jwm
        );
    }

    #[test]
    fn a_managed_window_on_the_chain_wins_over_the_spawn_that_created_it() {
        let now = Instant::now();
        let mut registry = JwmLaunchRegistry::default();
        // JWM spawned the terminal (500); the user ran an app inside it.
        registry.record(500, Some(7_000), now);
        let (ancestors, start_time) =
            probe_with(&[(777, 600), (600, 500), (500, 1)], &[(500, 7_000)]);
        let probe = ProcessProbe {
            ancestors: &ancestors,
            start_time: &start_time,
        };
        let managed: HashSet<u32> = [500].into_iter().collect();
        assert_eq!(
            registry.classify(Some(777), &managed, &probe, now + Duration::from_secs(60)),
            LaunchOrigin::ManagedWindow
        );
        assert!(LaunchOrigin::ManagedWindow.uses_memory());
    }

    #[test]
    fn a_second_window_of_a_running_application_is_the_applications_doing() {
        let now = Instant::now();
        let mut registry = JwmLaunchRegistry::default();
        registry.record(500, None, now);
        let (ancestors, start_time) = probe_with(&[(500, 1)], &[]);
        let probe = ProcessProbe {
            ancestors: &ancestors,
            start_time: &start_time,
        };
        let managed: HashSet<u32> = [500].into_iter().collect();
        assert_eq!(
            registry.classify(Some(500), &managed, &probe, now),
            LaunchOrigin::ManagedWindow
        );
    }

    #[test]
    fn a_recycled_pid_does_not_match_a_spawn_record() {
        let now = Instant::now();
        let mut registry = JwmLaunchRegistry::default();
        registry.record(500, Some(7_000), now);
        let (ancestors, start_time) = probe_with(&[(500, 1)], &[(500, 9_999)]);
        let probe = ProcessProbe {
            ancestors: &ancestors,
            start_time: &start_time,
        };
        assert_eq!(
            registry.classify(
                Some(500),
                &HashSet::new(),
                &probe,
                now + UNRESOLVED_LAUNCH_ATTRIBUTION_WINDOW + Duration::from_secs(1)
            ),
            LaunchOrigin::External
        );
    }

    #[test]
    fn an_unresolved_window_is_blamed_on_a_recent_spawn_only_once() {
        let now = Instant::now();
        let mut registry = JwmLaunchRegistry::default();
        // gnome-terminal: the spawned wrapper exits, a D-Bus server maps.
        registry.record(500, None, now);
        let (ancestors, start_time) = probe_with(&[(900, 1)], &[]);
        let probe = ProcessProbe {
            ancestors: &ancestors,
            start_time: &start_time,
        };
        assert_eq!(
            registry.classify(
                Some(900),
                &HashSet::new(),
                &probe,
                now + Duration::from_secs(2)
            ),
            LaunchOrigin::Jwm
        );
        assert_eq!(
            registry.classify(
                Some(901),
                &HashSet::new(),
                &probe,
                now + Duration::from_secs(3)
            ),
            LaunchOrigin::External,
            "the same spawn cannot explain two windows"
        );
    }

    #[test]
    fn an_old_spawn_no_longer_explains_an_unresolved_window() {
        let now = Instant::now();
        let mut registry = JwmLaunchRegistry::default();
        registry.record(500, None, now);
        let (ancestors, start_time) = probe_with(&[(900, 1)], &[]);
        let probe = ProcessProbe {
            ancestors: &ancestors,
            start_time: &start_time,
        };
        let later = now + UNRESOLVED_LAUNCH_ATTRIBUTION_WINDOW + Duration::from_secs(1);
        assert_eq!(
            registry.classify(Some(900), &HashSet::new(), &probe, later),
            LaunchOrigin::External
        );
        assert_eq!(
            registry.classify(None, &HashSet::new(), &probe, later),
            LaunchOrigin::Unknown
        );
        assert!(LaunchOrigin::Unknown.uses_memory());
    }

    #[test]
    fn a_pidless_window_is_blamed_on_a_recent_spawn() {
        let now = Instant::now();
        let mut registry = JwmLaunchRegistry::default();
        registry.record(500, None, now);
        let (ancestors, start_time) = probe_with(&[], &[]);
        let probe = ProcessProbe {
            ancestors: &ancestors,
            start_time: &start_time,
        };
        assert_eq!(
            registry.classify(None, &HashSet::new(), &probe, now + Duration::from_secs(1)),
            LaunchOrigin::Jwm
        );
    }

    fn remembered(monitor_num: i32, tags: u32) -> RememberedPlacement {
        RememberedPlacement {
            monitor_num,
            connector: None,
            tags,
            closed_seq: 0,
        }
    }

    fn rule(tags: usize, monitor: i32) -> crate::jwm::types::WMRule {
        crate::jwm::types::WMRule {
            class: "firefox".into(),
            instance: String::new(),
            name: String::new(),
            tags,
            is_floating: false,
            monitor,
        }
    }

    #[test]
    fn the_memory_fills_only_what_a_rule_left_open() {
        let tagmask = 0b1_1111_1111;
        assert_eq!(
            resolve_memory_against_rule(&remembered(1, 0b100), None, tagmask),
            MemoryPlacement {
                monitor_num: Some(1),
                tags: Some(0b100)
            }
        );
        assert_eq!(
            resolve_memory_against_rule(&remembered(1, 0b100), Some(&rule(0b10, -1)), tagmask),
            MemoryPlacement {
                monitor_num: Some(1),
                tags: None
            },
            "a rule's tags win; the monitor is still the memory's"
        );
        assert_eq!(
            resolve_memory_against_rule(&remembered(1, 0b100), Some(&rule(0, 0)), tagmask),
            MemoryPlacement {
                monitor_num: None,
                tags: Some(0b100)
            },
            "a rule's monitor wins; the tags are still the memory's"
        );
        assert_eq!(
            resolve_memory_against_rule(&remembered(1, 0b100), Some(&rule(0b10, 0)), tagmask),
            MemoryPlacement {
                monitor_num: None,
                tags: None
            }
        );
    }

    #[test]
    fn a_tag_that_no_longer_exists_is_not_applied() {
        assert_eq!(
            resolve_memory_against_rule(&remembered(0, 0b1_0000_0000), None, 0b1111),
            MemoryPlacement {
                monitor_num: Some(0),
                tags: None
            }
        );
    }

    #[test]
    fn spawn_records_expire_and_stay_bounded() {
        let now = Instant::now();
        let mut registry = JwmLaunchRegistry::default();
        registry.record(0, None, now);
        assert_eq!(registry.len(), 0, "PID 0 is never a child");
        for pid in 1..=MAX_LAUNCH_RECORDS as u32 + 5 {
            registry.record(pid, None, now + Duration::from_millis(u64::from(pid)));
        }
        assert_eq!(registry.len(), MAX_LAUNCH_RECORDS);
        assert!(!registry.by_pid.contains_key(&1));
        registry.record(
            9_000,
            None,
            now + LAUNCH_RECORD_TTL + Duration::from_secs(1),
        );
        assert_eq!(registry.len(), 1, "everything older than the TTL is gone");
    }

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let sequence = CLOSED_PLACEMENT_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "jwm-closed-placement-{label}-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn file(&self) -> PathBuf {
            self.0.join(CLOSED_PLACEMENT_FILE)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn closed_placement_round_trips_through_disk() {
        let dir = TestDir::new("roundtrip");
        let path = dir.file();
        let mut memory = ClosedPlacementMemory::default();
        assert!(memory.remember(identity("firefox"), 1, Some("HDMI-A-1".into()), 0b100));
        assert!(memory.remember(identity("kitty"), 0, Some("DP-1".into()), 0b10));
        memory.save_to_path(&path);

        let loaded = ClosedPlacementMemory::load_from_path(&path);
        assert_eq!(loaded.len(), 2);
        let firefox = loaded.lookup(&identity("firefox")).expect("firefox");
        assert_eq!((firefox.monitor_num, firefox.tags), (1, 0b100));
        assert_eq!(firefox.connector.as_deref(), Some("HDMI-A-1"));
        let kitty = loaded.lookup(&identity("kitty")).expect("kitty");
        assert_eq!((kitty.monitor_num, kitty.tags), (0, 0b10));
        assert_eq!(kitty.connector.as_deref(), Some("DP-1"));
        // Eviction order survived: firefox was remembered first.
        assert!(firefox.closed_seq < kitty.closed_seq);

        let json = fs::read_to_string(&path).unwrap();
        let snapshot: ClosedPlacementSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snapshot.version, CLOSED_PLACEMENT_VERSION);
        assert!(
            json.contains("\"connector\":\"HDMI-A-1\""),
            "connector must be persisted: {json}"
        );
    }

    #[test]
    fn closed_placement_load_on_missing_is_empty() {
        let dir = TestDir::new("missing");
        let path = dir.file();
        assert!(!path.exists());
        let loaded = ClosedPlacementMemory::load_from_path(&path);
        assert!(loaded.is_empty());
    }

    #[test]
    fn closed_placement_v1_without_connector_still_loads() {
        let dir = TestDir::new("v1-migrate");
        let path = dir.file();
        // Pre-wave-39 snapshot: version 1, bare monitor_num, no connector.
        // Mode must be private: the loader refuses group/other-writable files.
        fs::write(
            &path,
            r#"{"version":1,"placements":[{"class":"firefox","instance":"Navigator","monitor_num":1,"tags":4,"seq":0}]}"#,
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let loaded = ClosedPlacementMemory::load_from_path(&path);
        let firefox = loaded
            .lookup(&PlacementIdentity::new("firefox", "Navigator").unwrap())
            .expect("firefox/Navigator");
        assert_eq!(firefox.monitor_num, 1);
        assert_eq!(firefox.tags, 4);
        assert_eq!(firefox.connector, None);

        // A later save upgrades to the current schema while keeping the
        // monitor_num fallback.
        loaded.save_to_path(&path);
        let upgraded = fs::read_to_string(&path).unwrap();
        let snapshot: ClosedPlacementSnapshot = serde_json::from_str(&upgraded).unwrap();
        assert_eq!(snapshot.version, CLOSED_PLACEMENT_VERSION);
        assert_eq!(snapshot.placements[0].connector, None);
        assert_eq!(snapshot.placements[0].monitor_num, 1);
    }

    #[test]
    fn closed_placement_resolves_connector_after_monitor_renumber() {
        // Closed on HDMI-A-1 when it was monitor 1; after unplugging the
        // left panel, hole-fill renumbered HDMI-A-1 to monitor 0. The stale
        // monitor_num must not win over the connector.
        let live = [(0, "HDMI-A-1", "HDMI-A-1"), (1, "DP-2", "edid:DEL:1234")];
        assert_eq!(
            resolve_monitor_num_by_connector(1, Some("HDMI-A-1"), &live),
            0,
            "connector finds the renumbered output"
        );
        assert_eq!(
            resolve_monitor_num_by_connector(1, Some("edid:DEL:1234"), &live),
            1,
            "stable_key matches when the remembered key came from EDID"
        );
        assert_eq!(
            resolve_monitor_num_by_connector(1, Some("gone"), &live),
            1,
            "missing connector falls back to saved monitor_num"
        );
        assert_eq!(
            resolve_monitor_num_by_connector(1, None, &live),
            1,
            "pre-v2 entries without a connector keep monitor_num"
        );
    }

    #[test]
    fn closed_placement_snapshot_rejects_out_of_bounds() {
        let too_many = ClosedPlacementSnapshot {
            version: CLOSED_PLACEMENT_VERSION,
            placements: (0..=MAX_REMEMBERED_PLACEMENTS)
                .map(|index| ClosedPlacementEntry {
                    class: format!("app{index}"),
                    instance: String::new(),
                    monitor_num: 0,
                    connector: None,
                    tags: 1,
                    seq: index as u64,
                })
                .collect(),
        };
        assert!(too_many.validate().unwrap_err().contains("limit is"));

        let empty_tags = ClosedPlacementSnapshot {
            version: CLOSED_PLACEMENT_VERSION,
            placements: vec![ClosedPlacementEntry {
                class: "firefox".into(),
                instance: String::new(),
                monitor_num: 0,
                connector: None,
                tags: 0,
                seq: 0,
            }],
        };
        assert!(
            empty_tags
                .validate()
                .unwrap_err()
                .contains("empty tag mask")
        );

        let bad_version = ClosedPlacementSnapshot {
            version: CLOSED_PLACEMENT_VERSION + 1,
            placements: Vec::new(),
        };
        assert!(bad_version.validate().unwrap_err().contains("unsupported"));
    }

    #[test]
    fn closed_placement_disable_clears_memory_and_deletes_file() {
        let dir = TestDir::new("disable");
        let path = dir.file();
        let mut memory = ClosedPlacementMemory::default();
        assert!(memory.remember(identity("firefox"), 0, Some("eDP-1".into()), 1));
        memory.save_to_path(&path);
        assert!(path.is_file());

        memory.clear();
        clear_closed_placement_file(&path);
        assert!(memory.is_empty());
        assert!(!path.exists());

        // Re-enable starts empty even if somehow a file were still there —
        // load after delete is empty; and while disabled the file is gone.
        let reloaded = ClosedPlacementMemory::load_from_path(&path);
        assert!(reloaded.is_empty());
    }

    #[test]
    fn closed_placement_atomic_store_is_private() {
        let dir = TestDir::new("private");
        let path = dir.file();
        let mut memory = ClosedPlacementMemory::default();
        assert!(memory.remember(identity("firefox"), 0, None, 1));
        memory.save_to_path(&path);

        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn closed_placement_loader_rejects_a_fifo_without_waiting_for_a_writer() {
        use std::os::unix::ffi::OsStrExt as _;

        let dir = TestDir::new("fifo-load");
        let path = dir.file();
        let path_bytes = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path_bytes.as_ptr(), 0o600) }, 0);

        let error = load_closed_placement_snapshot(&path).unwrap_err();
        assert!(error.to_string().contains("not a regular file"));
    }

    #[test]
    fn closed_placement_temporary_collision_preserves_the_other_writer() {
        let dir = TestDir::new("temporary-collision");
        let collision = dir
            .0
            .join(format!("{CLOSED_PLACEMENT_TEMPORARY_PREFIX}4242-11"));
        fs::write(&collision, "other writer").unwrap();
        let mut sequences = [11, 12].into_iter();

        let (temporary, file) = create_closed_placement_temporary(&dir.0, 4242, || {
            sequences.next().expect("a fresh sequence")
        })
        .unwrap();
        drop(file);

        assert_eq!(fs::read_to_string(&collision).unwrap(), "other writer");
        assert_eq!(
            temporary.file_name().and_then(|name| name.to_str()),
            Some(".closed_placement.json.tmp-4242-12")
        );

        let exhausted = create_closed_placement_temporary(&dir.0, 4242, || 11).unwrap_err();
        assert_eq!(exhausted.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(collision).unwrap(), "other writer");
    }
}
