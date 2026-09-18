//! Where a closed window goes when it comes back.
//!
//! JWM's default puts every new window on the selected monitor's active
//! tags: wherever the pointer happens to be. That is right for a window the
//! user just asked for with a keybinding or the launcher, and wrong for most
//! of the rest: a browser reopened from a shell, a viewer an agent spawns
//! inside a terminal, an application's own second top-level. Those belong
//! where the same application was last closed.
//!
//! Two bounded, in-memory registries implement that:
//!
//! - [`ClosedPlacementMemory`] remembers, per WM_CLASS identity, the monitor
//!   number and tag mask a regular client held when it was unmanaged.
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
//! surfaces have no better evidence. Neither registry touches the disk.
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
use log::{debug, info, warn};
use std::collections::{HashMap, HashSet};
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RememberedPlacement {
    pub monitor_num: i32,
    pub tags: u32,
    closed_at: Instant,
}

#[derive(Debug, Default)]
pub(crate) struct ClosedPlacementMemory {
    by_identity: HashMap<PlacementIdentity, RememberedPlacement>,
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

    /// Remember where `identity` was just closed. A later close of the same
    /// identity replaces the earlier one; the newest word wins. Returns
    /// `false` for a tag mask with nothing in it.
    pub(crate) fn remember(
        &mut self,
        identity: PlacementIdentity,
        monitor_num: i32,
        tags: u32,
        now: Instant,
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
                .min_by_key(|(_, placement)| placement.closed_at)
                .map(|(identity, _)| identity.clone());
            if let Some(oldest) = oldest {
                self.by_identity.remove(&oldest);
            }
        }
        self.by_identity.insert(
            identity,
            RememberedPlacement {
                monitor_num,
                tags,
                closed_at: now,
            },
        );
        true
    }

    #[must_use]
    pub(crate) fn lookup(&self, identity: &PlacementIdentity) -> Option<RememberedPlacement> {
        self.by_identity.get(identity).copied()
    }

    /// Drop everything. Used when the feature is switched off at runtime so a
    /// later re-enable starts from what the user does next, not from stale
    /// history.
    pub(crate) fn clear(&mut self) {
        self.by_identity.clear();
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

#[must_use]
pub(crate) fn resolve_memory_against_rule(
    remembered: RememberedPlacement,
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
    let mut out = Vec::with_capacity(MAX_ANCESTRY_DEPTH);
    let mut current = pid;
    for _ in 0..MAX_ANCESTRY_DEPTH {
        match linux_parent_pid(current) {
            Some(parent) if parent > 1 && parent != current => {
                out.push(parent);
                current = parent;
            }
            _ => break,
        }
    }
    out
}

fn linux_parent_pid(pid: u32) -> Option<u32> {
    let contents = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    contents
        .lines()
        .find_map(|line| line.strip_prefix("PPid:"))
        .and_then(|rest| rest.trim().parse().ok())
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

        let rule = RuleMatcher::find_matching_rule(&name, &class, &instance);
        let placement = resolve_memory_against_rule(remembered, rule.as_ref(), cfg.tagmask());
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
    pub(crate) fn remember_closed_placement(&mut self, client_key: ClientKey, now: Instant) {
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
        let Some(monitor_num) = client
            .mon
            .and_then(|key| self.state.monitors.get(key))
            .map(|monitor| monitor.num)
        else {
            return;
        };
        let win = client.win;
        if self
            .closed_placements
            .remember(identity.clone(), monitor_num, tags, now)
        {
            info!(
                "[closed-placement] {win:?} ({identity}) closed on monitor {monitor_num} tags {tags:#b}"
            );
        } else {
            warn!("[closed-placement] {win:?} ({identity}) closed without a tag; not remembered");
        }
    }

    /// Config reload: a feature switched off forgets what it learned.
    pub(crate) fn reconcile_closed_placement_config(&mut self, enabled: bool) {
        if !enabled && !self.closed_placements.is_empty() {
            self.closed_placements.clear();
            info!("[closed-placement] disabled; forgetting remembered placements");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let now = Instant::now();
        let mut memory = ClosedPlacementMemory::default();
        assert!(memory.remember(identity("firefox"), 1, 0b100, now));
        assert!(memory.remember(identity("firefox"), 0, 0b10, now + Duration::from_secs(1)));
        let placement = memory.lookup(&identity("firefox")).expect("remembered");
        assert_eq!((placement.monitor_num, placement.tags), (0, 0b10));
        assert_eq!(memory.len(), 1);
        assert!(memory.lookup(&identity("kitty")).is_none());
    }

    #[test]
    fn memory_rejects_an_empty_tag_mask() {
        let mut memory = ClosedPlacementMemory::default();
        assert!(!memory.remember(identity("firefox"), 0, 0, Instant::now()));
        assert!(memory.is_empty());
    }

    #[test]
    fn memory_evicts_the_oldest_identity_when_full() {
        let now = Instant::now();
        let mut memory = ClosedPlacementMemory::default();
        for index in 0..MAX_REMEMBERED_PLACEMENTS {
            let identity = PlacementIdentity::new(&format!("app{index}"), "").unwrap();
            assert!(memory.remember(identity, 0, 1, now + Duration::from_millis(index as u64)));
        }
        assert_eq!(memory.len(), MAX_REMEMBERED_PLACEMENTS);
        assert!(memory.remember(identity("newest"), 0, 1, now + Duration::from_secs(1)));
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
            tags,
            closed_at: Instant::now(),
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
            resolve_memory_against_rule(remembered(1, 0b100), None, tagmask),
            MemoryPlacement {
                monitor_num: Some(1),
                tags: Some(0b100)
            }
        );
        assert_eq!(
            resolve_memory_against_rule(remembered(1, 0b100), Some(&rule(0b10, -1)), tagmask),
            MemoryPlacement {
                monitor_num: Some(1),
                tags: None
            },
            "a rule's tags win; the monitor is still the memory's"
        );
        assert_eq!(
            resolve_memory_against_rule(remembered(1, 0b100), Some(&rule(0, 0)), tagmask),
            MemoryPlacement {
                monitor_num: None,
                tags: Some(0b100)
            },
            "a rule's monitor wins; the tags are still the memory's"
        );
        assert_eq!(
            resolve_memory_against_rule(remembered(1, 0b100), Some(&rule(0b10, 0)), tagmask),
            MemoryPlacement {
                monitor_num: None,
                tags: None
            }
        );
    }

    #[test]
    fn a_tag_that_no_longer_exists_is_not_applied() {
        assert_eq!(
            resolve_memory_against_rule(remembered(0, 0b1_0000_0000), None, 0b1111),
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
}
