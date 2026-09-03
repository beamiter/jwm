// Lifecycle management: cleanup, config reload, and resource management

use crate::Jwm;
use crate::backend::api::{Backend, Geometry, ManagedUnmapReason, WindowChanges};
use crate::backend::common_define::{ArgbColor, ColorScheme, EventMaskBits, SchemeType, WindowId};
use crate::config::CONFIG;
use crate::core::models::{ClientKey, MonitorKey, WMClient};
use crate::core::types::Rect;
use crate::ipc::IpcResponse;
use crate::jwm::statusbar::StatusBarBuilder;
use crate::jwm::visibility::restore_hidden_geometry;
use crate::jwm::window_state::x11_geometry_fully_left_of_desktop;
use log::{info, warn};
use std::error::Error;
use std::fmt;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};

const CONFIG_RELOAD_DEBOUNCE: Duration = Duration::from_millis(300);
const CONFIG_RELOAD_POLL_INTERVAL: Duration = Duration::from_secs(1);

type CleanupResult = Result<(), Box<dyn Error>>;

#[derive(Debug)]
struct CleanupFailure {
    stage: String,
    error: Box<dyn Error>,
}

#[derive(Debug)]
struct CleanupError {
    failures: Vec<CleanupFailure>,
}

impl fmt::Display for CleanupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} cleanup stage(s) failed", self.failures.len())?;
        for failure in &self.failures {
            write!(formatter, "; {}: {}", failure.stage, failure.error)?;
        }
        Ok(())
    }
}

impl Error for CleanupError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.failures.first().map(|failure| failure.error.as_ref())
    }
}

#[derive(Debug, Default)]
struct CleanupFailures {
    failures: Vec<CleanupFailure>,
}

impl CleanupFailures {
    fn record(&mut self, stage: impl Into<String>, result: CleanupResult) {
        if let Err(error) = result {
            let stage = stage.into();
            warn!("[cleanup] {stage} failed: {error}");
            self.failures.push(CleanupFailure { stage, error });
        }
    }

    fn finish(self) -> CleanupResult {
        if self.failures.is_empty() {
            Ok(())
        } else {
            Err(Box::new(CleanupError {
                failures: self.failures,
            }))
        }
    }
}

fn boxed_cleanup_result<E>(result: Result<(), E>) -> CleanupResult
where
    E: Error + 'static,
{
    result.map_err(|error| Box::new(error) as Box<dyn Error>)
}

fn run_best_effort_cleanup<S: Copy>(
    stages: &[(S, &'static str)],
    mut run_stage: impl FnMut(S) -> CleanupResult,
) -> CleanupResult {
    let mut failures = CleanupFailures::default();
    for &(stage, label) in stages {
        failures.record(label, run_stage(stage));
    }
    failures.finish()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EssentialCleanupStage {
    X11Resources,
    SystemResources,
    ThemePixels,
    DisplayFlush,
}

const ESSENTIAL_CLEANUP_STAGES: &[(EssentialCleanupStage, &str)] = &[
    (EssentialCleanupStage::X11Resources, "X11 resources"),
    (EssentialCleanupStage::SystemResources, "system resources"),
    (EssentialCleanupStage::ThemePixels, "theme pixels"),
    (EssentialCleanupStage::DisplayFlush, "display flush"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum X11CleanupStage {
    ClientState,
    KeyGrabs,
    InputFocus,
    Backend,
    Cursor,
}

const X11_CLEANUP_STAGES: &[(X11CleanupStage, &str)] = &[
    (X11CleanupStage::ClientState, "client X11 state"),
    (X11CleanupStage::KeyGrabs, "key grabs"),
    (X11CleanupStage::InputFocus, "input focus"),
    (X11CleanupStage::Backend, "backend resources"),
    (X11CleanupStage::Cursor, "cursor resources"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SystemCleanupStage {
    StatusBars,
    SharedMemory,
}

const SYSTEM_CLEANUP_STAGES: &[(SystemCleanupStage, &str)] = &[
    (SystemCleanupStage::StatusBars, "status bar processes"),
    (SystemCleanupStage::SharedMemory, "shared memory"),
];

/// Build shutdown work from the authoritative client registry, not monitor
/// stacks. Parked scratchpads deliberately have no monitor and therefore do
/// not occur in any stack, but a normal WM exit must still return their real
/// windows from JWM's off-screen hiding coordinate and clear public state.
fn x11_client_cleanup_plan(
    client_order: &[ClientKey],
    clients: &slotmap::SlotMap<ClientKey, WMClient>,
) -> Vec<(WindowId, i32, ClientKey)> {
    client_order
        .iter()
        .filter_map(|&client_key| {
            clients
                .get(client_key)
                .map(|client| (client.win, client.geometry.old_border_w, client_key))
        })
        .collect()
}

/// Proof that normal-exit Phase A completed for every managed client and for
/// every swallowed parent.  Its private field prevents a caller from entering
/// destructive Phase B without going through the checked handoff transaction.
pub(crate) struct NormalExitHandoff {
    _private: (),
}

/// A failed normal-exit preflight is not automatically permission to resume
/// the ordinary event loop.  `resume_safe` is granted only after the rollback
/// postconditions were queried from the display server for every client that
/// Phase A touched.
#[derive(Debug)]
pub(crate) struct NormalExitPrepareError {
    primary: String,
    rollback_diagnostics: Vec<String>,
    resume_safe: bool,
}

impl NormalExitPrepareError {
    fn new(
        primary: impl Into<String>,
        rollback_diagnostics: Vec<String>,
        resume_safe: bool,
    ) -> Self {
        Self {
            primary: primary.into(),
            rollback_diagnostics,
            resume_safe,
        }
    }

    /// True only when every touched client was observed in a state from which
    /// the existing JWM can safely continue managing it.
    pub(crate) const fn resume_safe(&self) -> bool {
        self.resume_safe
    }
}

impl fmt::Display for NormalExitPrepareError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.primary)?;
        if !self.rollback_diagnostics.is_empty() {
            write!(
                formatter,
                "; rollback diagnostics: {}",
                self.rollback_diagnostics.join("; ")
            )?;
        }
        if !self.resume_safe {
            write!(
                formatter,
                "; rollback postconditions are unsafe for event-loop resume"
            )?;
        }
        Ok(())
    }
}

impl Error for NormalExitPrepareError {}

#[derive(Debug, Clone)]
struct PreparedNormalExitClient {
    client_key: ClientKey,
    previous_client: WMClient,
    server_geometry: Geometry,
    was_viewable: bool,
}

fn geometry_matches(actual: Geometry, expected: Geometry) -> bool {
    actual.x == expected.x
        && actual.y == expected.y
        && actual.w == expected.w
        && actual.h == expected.h
        && actual.border == expected.border
}

fn geometry_intersects_rect(geometry: Geometry, rect: Rect) -> bool {
    if rect.w <= 0 || rect.h <= 0 || geometry.w == 0 || geometry.h == 0 {
        return false;
    }
    let geometry_right = i64::from(geometry.x)
        .saturating_add(i64::from(geometry.w))
        .saturating_add(i64::from(geometry.border).saturating_mul(2));
    let geometry_bottom = i64::from(geometry.y)
        .saturating_add(i64::from(geometry.h))
        .saturating_add(i64::from(geometry.border).saturating_mul(2));
    let rect_right = i64::from(rect.x).saturating_add(i64::from(rect.w));
    let rect_bottom = i64::from(rect.y).saturating_add(i64::from(rect.h));

    i64::from(geometry.x) < rect_right
        && geometry_right > i64::from(rect.x)
        && i64::from(geometry.y) < rect_bottom
        && geometry_bottom > i64::from(rect.y)
}

fn clamp_geometry_to_rect(mut geometry: Geometry, rect: Rect) -> Geometry {
    let border2 = i32::try_from(geometry.border)
        .unwrap_or(i32::MAX)
        .saturating_mul(2);
    let total_width = i32::try_from(geometry.w)
        .unwrap_or(i32::MAX)
        .saturating_add(border2)
        .max(1);
    let total_height = i32::try_from(geometry.h)
        .unwrap_or(i32::MAX)
        .saturating_add(border2)
        .max(1);
    let max_x = rect
        .x
        .saturating_add(rect.w.max(1).saturating_sub(total_width).max(0));
    let max_y = rect
        .y
        .saturating_add(rect.h.max(1).saturating_sub(total_height).max(0));
    geometry.x = geometry.x.clamp(rect.x, max_x);
    geometry.y = geometry.y.clamp(rect.y, max_y);
    geometry
}

#[derive(Debug, Default)]
struct NormalExitRollbackOutcome {
    diagnostics: Vec<String>,
    resume_safe: bool,
}

#[derive(Debug, Clone, Copy)]
struct PendingConfigReload {
    revision: SystemTime,
    changed_at: Instant,
}

/// Shared state for event-driven and polling-based config reload detection.
///
/// A revision is its file modification time. Attempts are recorded before the
/// parser runs, so a malformed revision cannot produce an error on every
/// update tick; editing the file gives it a new revision and enables one new
/// attempt.
#[derive(Debug)]
pub(crate) struct ConfigReloadTracker {
    last_observed: Option<SystemTime>,
    last_attempted: Option<SystemTime>,
    pending: Option<PendingConfigReload>,
    last_poll_at: Option<Instant>,
}

impl ConfigReloadTracker {
    pub(crate) fn new(initial_revision: Option<SystemTime>) -> Self {
        Self {
            last_observed: initial_revision,
            // Loading CONFIG during startup settles the initial revision even
            // when parsing falls back to defaults. Wait for the next edit.
            last_attempted: initial_revision,
            pending: None,
            last_poll_at: None,
        }
    }

    fn should_poll(&mut self, now: Instant) -> bool {
        if self.last_poll_at.is_some_and(|last_poll| {
            now.saturating_duration_since(last_poll) < CONFIG_RELOAD_POLL_INTERVAL
        }) {
            return false;
        }

        self.last_poll_at = Some(now);
        true
    }

    fn observe(&mut self, revision: SystemTime, now: Instant) -> bool {
        if self.last_observed == Some(revision) {
            return false;
        }

        self.last_observed = Some(revision);
        if self.last_attempted == Some(revision) {
            self.pending = None;
            return false;
        }

        // A new revision restarts the debounce period. Repeated notifications
        // for the same revision are handled by the early return above and do
        // not postpone a stable reload indefinitely.
        self.pending = Some(PendingConfigReload {
            revision,
            changed_at: now,
        });
        true
    }

    fn take_due_attempt(&mut self, now: Instant) -> Option<SystemTime> {
        let pending = self.pending?;
        if !self.pending_is_due(now) {
            return None;
        }

        self.pending = None;
        self.last_attempted = Some(pending.revision);
        Some(pending.revision)
    }

    fn pending_is_due(&self, now: Instant) -> bool {
        self.pending.is_some_and(|pending| {
            now.saturating_duration_since(pending.changed_at) >= CONFIG_RELOAD_DEBOUNCE
        })
    }

    fn next_wakeup_in(&self, now: Instant) -> Duration {
        let poll_in = self.last_poll_at.map_or(Duration::ZERO, |last_poll| {
            CONFIG_RELOAD_POLL_INTERVAL.saturating_sub(now.saturating_duration_since(last_poll))
        });
        let debounce_in = self.pending.map(|pending| {
            CONFIG_RELOAD_DEBOUNCE.saturating_sub(now.saturating_duration_since(pending.changed_at))
        });

        debounce_in.map_or(poll_in, |debounce_in| poll_in.min(debounce_in))
    }

    #[cfg(test)]
    fn deadline_is_due(&self, now: Instant) -> bool {
        self.next_wakeup_in(now).is_zero()
    }

    fn mark_attempted(&mut self, revision: SystemTime) {
        self.last_observed = Some(revision);
        self.last_attempted = Some(revision);
        self.pending = None;
    }

    fn has_pending(&self) -> bool {
        self.pending.is_some()
    }
}

impl Jwm {
    fn normal_exit_geometry_intersects_output(&self, geometry: Geometry) -> bool {
        self.state.monitors.values().any(|monitor| {
            geometry_intersects_rect(
                geometry,
                Rect::new(
                    monitor.geometry.m_x,
                    monitor.geometry.m_y,
                    monitor.geometry.m_w,
                    monitor.geometry.m_h,
                ),
            )
        })
    }

    fn normal_exit_monitor_area(&self, monitor: MonitorKey) -> Option<Rect> {
        self.monitor_work_area(monitor)
            .filter(|area| area.w > 0 && area.h > 0)
            .or_else(|| {
                self.state.monitors.get(monitor).and_then(|monitor| {
                    let output = Rect::new(
                        monitor.geometry.m_x,
                        monitor.geometry.m_y,
                        monitor.geometry.m_w,
                        monitor.geometry.m_h,
                    );
                    (output.w > 0 && output.h > 0).then_some(output)
                })
            })
    }

    fn normal_exit_fallback_area(&self, client: &WMClient) -> Option<Rect> {
        client
            .mon
            .and_then(|monitor| self.normal_exit_monitor_area(monitor))
            .or_else(|| {
                self.state
                    .sel_mon
                    .and_then(|monitor| self.normal_exit_monitor_area(monitor))
            })
            .or_else(|| {
                self.state
                    .monitor_order
                    .iter()
                    .find_map(|&monitor| self.normal_exit_monitor_area(monitor))
            })
    }

    fn client_needs_normal_exit_handoff(&self, client: &WMClient) -> bool {
        let represented = Geometry {
            x: client.geometry.x,
            y: client.geometry.y,
            w: u32::try_from(client.geometry.w.max(1)).unwrap_or(u32::MAX),
            h: u32::try_from(client.geometry.h.max(1)).unwrap_or(u32::MAX),
            border: u32::try_from(client.geometry.border_w.max(0)).unwrap_or(u32::MAX),
        };
        client.state.is_swallowed
            || client.state.is_hidden
            || client.geometry.hidden_x.is_some()
            || client.geometry.hidden_restore_rect.is_some()
            || x11_geometry_fully_left_of_desktop(represented, self.desktop_left_edge())
    }

    fn normal_exit_target_geometry(
        &self,
        client: &WMClient,
        server_border: u32,
    ) -> Option<Geometry> {
        if !self.client_needs_normal_exit_handoff(client) {
            return None;
        }

        let desktop_left = self.desktop_left_edge();
        let mut geometry = client.geometry.clone();
        let legacy_fallback_x = client
            .mon
            .and_then(|monitor| self.monitor_work_area(monitor))
            .map_or(desktop_left, |area| area.x);
        restore_hidden_geometry(&mut geometry, desktop_left, legacy_fallback_x);
        let target = Geometry {
            x: geometry.x,
            y: geometry.y,
            w: u32::try_from(geometry.w.max(1)).unwrap_or(u32::MAX),
            h: u32::try_from(geometry.h.max(1)).unwrap_or(u32::MAX),
            // Phase A must not release JWM's decoration ownership. Preserve
            // the server's exact current border until the global barrier.
            border: server_border,
        };
        if self.normal_exit_geometry_intersects_output(target) {
            return Some(target);
        }

        // A restore rectangle can still name an output that disappeared
        // while the client was parked (off-tag clients do not participate in
        // minimized-output migration). Put the complete window into a live
        // work area when it fits, or anchor its top-left there when it does
        // not, before any MapWindow can expose it.
        self.normal_exit_fallback_area(client)
            .map_or(Some(target), |area| {
                Some(clamp_geometry_to_rect(target, area))
            })
    }

    /// Restore every Phase-A mutation in reverse order.  Public Hidden,
    /// WM_STATE and V1 properties were deliberately untouched, so rollback
    /// only needs to restore server geometry, JWM's snapshot and the backend's
    /// true-Iconic desired owner.
    fn rollback_normal_exit_client_handoff(
        &mut self,
        backend: &mut dyn Backend,
        prepared: &[PreparedNormalExitClient],
    ) -> NormalExitRollbackOutcome {
        let mut outcome = NormalExitRollbackOutcome {
            diagnostics: Vec::new(),
            resume_safe: true,
        };
        for entry in prepared.iter().rev() {
            let win = entry.previous_client.win;
            if let Some(client) = self.state.clients.get_mut(entry.client_key) {
                *client = entry.previous_client.clone();
            } else {
                outcome
                    .diagnostics
                    .push(format!("managed client {win:?} vanished during rollback"));
                outcome.resume_safe = false;
                continue;
            }

            let geometry = entry.server_geometry;
            if let Err(error) = backend.window_ops().configure(
                win,
                geometry.x,
                geometry.y,
                geometry.w,
                geometry.h,
                geometry.border,
            ) {
                // The request result is diagnostic, not the safety verdict:
                // a checked request can report an error after the server has
                // already reached the desired state.  The final readback
                // below is authoritative.
                outcome
                    .diagnostics
                    .push(format!("restore server geometry for {win:?}: {error}"));
            }

            let mut hidden_owner_restored = true;
            if entry.previous_client.state.is_swallowed {
                // The swallowed batch already attempted its own reverse
                // unmaps. Retry only a parent that is still viewable (or whose
                // state cannot be queried): a second UnmapWindow against an
                // already-unmapped client produces no UnmapNotify, leaving an
                // unconsumable managed-unmap sequence marker behind.
                let needs_unmap_retry = match backend.window_ops().get_window_attributes(win) {
                    Ok(attributes) => attributes.map_state_viewable,
                    Err(error) => {
                        outcome.diagnostics.push(format!(
                            "query swallowed-parent rollback state for {win:?}: {error}"
                        ));
                        true
                    }
                };
                if needs_unmap_retry
                    && let Err(error) = backend
                        .window_ops()
                        .unmap_managed_window(win, ManagedUnmapReason::SwallowDiscard)
                {
                    outcome.diagnostics.push(format!(
                        "restore swallowed-parent unmap for {win:?}: {error}"
                    ));
                }
            } else if entry.previous_client.state.is_hidden {
                let eligible = StatusBarBuilder::is_minimized_dock_eligible(&entry.previous_client);
                let policy_result =
                    self.request_iconify_for_hidden_dock_client(backend, entry.client_key);
                hidden_owner_restored = policy_result.is_ok();
                if let Err(policy_error) = policy_result {
                    outcome.diagnostics.push(format!(
                        "restore checked true-Iconic owner for {win:?}: {policy_error}"
                    ));
                }

                // The policy helper deliberately no-ops for Dock-ineligible
                // clients.  A client that was genuinely unmapped before Phase
                // A still needs its backend generation retained, even after a
                // mid-flight eligibility change made it targetless.
                if !eligible && !entry.was_viewable || !hidden_owner_restored {
                    match backend.compositor_request_window_iconify(win) {
                        Ok(()) => hidden_owner_restored = true,
                        Err(error) => {
                            hidden_owner_restored = false;
                            outcome.diagnostics.push(format!(
                                "restore backend true-Iconic owner for {win:?}: {error}"
                            ));
                        }
                    }
                }
            }

            let geometry_confirmed = match backend.window_ops().get_geometry(win) {
                Ok(actual) if geometry_matches(actual, geometry) => true,
                Ok(actual) => {
                    outcome.diagnostics.push(format!(
                        "server geometry rollback for {win:?} was not confirmed: expected {geometry:?}, observed {actual:?}"
                    ));
                    false
                }
                Err(error) => {
                    outcome.diagnostics.push(format!(
                        "query server geometry after rolling back {win:?}: {error}"
                    ));
                    false
                }
            };

            let physical_state_confirmed = match backend.window_ops().get_window_attributes(win) {
                Ok(attributes) if entry.previous_client.state.is_swallowed => {
                    if attributes.map_state_viewable {
                        outcome
                            .diagnostics
                            .push(format!("rollback left swallowed parent {win:?} viewable"));
                        false
                    } else {
                        true
                    }
                }
                Ok(attributes) if entry.previous_client.state.is_hidden => {
                    if !attributes.map_state_viewable {
                        if !hidden_owner_restored {
                            outcome.diagnostics.push(format!(
                                    "rollback left hidden client {win:?} unmapped without a confirmed Iconic owner"
                                ));
                        }
                        hidden_owner_restored
                    } else {
                        match backend.window_ops().get_geometry(win) {
                            Ok(actual)
                                if x11_geometry_fully_left_of_desktop(
                                    actual,
                                    self.desktop_left_edge(),
                                ) =>
                            {
                                true
                            }
                            Ok(actual) => {
                                outcome.diagnostics.push(format!(
                                        "rollback left hidden client {win:?} viewable inside the desktop at {actual:?}"
                                    ));
                                false
                            }
                            Err(error) => {
                                outcome.diagnostics.push(format!(
                                    "query mapped hidden geometry for {win:?}: {error}"
                                ));
                                false
                            }
                        }
                    }
                }
                Ok(attributes) => {
                    if attributes.map_state_viewable != entry.was_viewable {
                        outcome.diagnostics.push(format!(
                                "rollback map state for {win:?} changed from viewable={} to viewable={}",
                                entry.was_viewable, attributes.map_state_viewable
                            ));
                        false
                    } else {
                        true
                    }
                }
                Err(error) => {
                    outcome.diagnostics.push(format!(
                        "query map state after rolling back {win:?}: {error}"
                    ));
                    false
                }
            };

            if !geometry_confirmed || !physical_state_confirmed {
                outcome.resume_safe = false;
            }
        }
        outcome
    }

    fn normal_exit_handoff_error(
        &mut self,
        backend: &mut dyn Backend,
        prepared: &[PreparedNormalExitClient],
        primary: impl Into<String>,
    ) -> NormalExitPrepareError {
        let primary = primary.into();
        let rollback = self.rollback_normal_exit_client_handoff(backend, prepared);
        NormalExitPrepareError::new(primary, rollback.diagnostics, rollback.resume_safe)
    }

    /// Phase A of normal shutdown. Every true-Iconic or off-screen parked
    /// managed client is made viewable at its saved geometry and synchronously
    /// verified before any event mask, grab, border or public state is touched.
    fn prepare_normal_exit_clients(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<Vec<PreparedNormalExitClient>, NormalExitPrepareError> {
        // True ICCCM ownership and server-side parking are X11 concepts.
        // Wayland backends retain their ordinary surface teardown and must not
        // be asked to emulate synchronous X11 GetGeometry/MapState replies.
        if !backend.capabilities().supports_client_list {
            return Ok(Vec::new());
        }
        let client_keys = self.state.client_order.clone();
        let desktop_left = self.desktop_left_edge();
        let mut prepared = Vec::new();

        for client_key in client_keys {
            let Some(mut previous_client) = self.state.clients.get(client_key).cloned() else {
                continue;
            };
            if !self.client_needs_normal_exit_handoff(&previous_client) {
                continue;
            }
            let win = previous_client.win;

            // Snapshot the server before the first possible mutation. Merely
            // querying an unmapped true-Iconic/swallowed window cannot expose
            // it, while doing this after parking repair would make a failed
            // repair impossible to roll back exactly.
            let before_attributes = match backend.window_ops().get_window_attributes(win) {
                Ok(attributes) => attributes,
                Err(error) => {
                    return Err(self.normal_exit_handoff_error(
                        backend,
                        &prepared,
                        format!("query pre-handoff map state for {win:?}: {error}"),
                    ));
                }
            };
            let server_geometry = match backend.window_ops().get_geometry(win) {
                Ok(geometry) => geometry,
                Err(error) => {
                    return Err(self.normal_exit_handoff_error(
                        backend,
                        &prepared,
                        format!("query pre-handoff geometry for {win:?}: {error}"),
                    ));
                }
            };
            let original_client = previous_client.clone();
            prepared.push(PreparedNormalExitClient {
                client_key,
                previous_client: original_client,
                server_geometry,
                was_viewable: before_attributes.map_state_viewable,
            });

            // Repair the parking endpoint while a true-Iconic client is still
            // unmapped. A hotplug failure may have left its remembered X
            // coordinate inside the new desktop; mapping before this check
            // would expose a live input surface during shutdown.
            if previous_client.state.is_hidden {
                if let Err(error) = self.retry_x11_minimized_client_park(backend, client_key) {
                    return Err(self.normal_exit_handoff_error(
                        backend,
                        &prepared,
                        format!("repair safe Iconic parking for {win:?}: {error}"),
                    ));
                }
                let Some(repaired_client) = self.state.clients.get(client_key).cloned() else {
                    return Err(self.normal_exit_handoff_error(
                        backend,
                        &prepared,
                        format!("managed client {win:?} vanished after parking repair"),
                    ));
                };
                previous_client = repaired_client;
                // A successful topology repair is itself a safe, durable
                // improvement. If a later client aborts the global handoff,
                // roll back to this freshly verified parking endpoint rather
                // than resurrecting a coordinate from a disconnected output.
                let repaired_server_geometry = match backend.window_ops().get_geometry(win) {
                    Ok(geometry) => geometry,
                    Err(error) => {
                        return Err(self.normal_exit_handoff_error(
                            backend,
                            &prepared,
                            format!("query repaired Iconic parking for {win:?}: {error}"),
                        ));
                    }
                };
                if let Some(entry) = prepared.last_mut() {
                    entry.previous_client = previous_client.clone();
                    entry.server_geometry = repaired_server_geometry;
                }
            }
            let Some(target) =
                self.normal_exit_target_geometry(&previous_client, server_geometry.border)
            else {
                continue;
            };
            if !self.normal_exit_geometry_intersects_output(target) {
                return Err(self.normal_exit_handoff_error(
                    backend,
                    &prepared,
                    format!(
                        "saved visible geometry for {win:?} does not intersect any current output"
                    ),
                ));
            }

            // Swallowed parents must remain unmapped until the bounded global
            // swallowed-parent batch, which is deliberately the final Phase-A
            // operation. Configure their saved visible geometry now and let
            // that batch perform MapWindow + IsViewable verification later.
            if !previous_client.state.is_swallowed {
                if previous_client.state.is_hidden
                    && let Err(error) = backend.compositor_cancel_window_iconify(win)
                {
                    return Err(self.normal_exit_handoff_error(
                        backend,
                        &prepared,
                        format!("map true-Iconic client {win:?} for normal exit: {error}"),
                    ));
                }
                match backend.window_ops().get_window_attributes(win) {
                    Ok(attributes) if attributes.map_state_viewable => {}
                    Ok(_) => {
                        return Err(self.normal_exit_handoff_error(
                            backend,
                            &prepared,
                            format!("normal-exit client {win:?} is not viewable after mapping"),
                        ));
                    }
                    Err(error) => {
                        return Err(self.normal_exit_handoff_error(
                            backend,
                            &prepared,
                            format!("confirm mapped normal-exit client {win:?}: {error}"),
                        ));
                    }
                }
            }

            if let Err(error) = backend.window_ops().configure(
                win,
                target.x,
                target.y,
                target.w,
                target.h,
                target.border,
            ) {
                return Err(self.normal_exit_handoff_error(
                    backend,
                    &prepared,
                    format!("restore visible server geometry for {win:?}: {error}"),
                ));
            }
            match backend.window_ops().get_geometry(win) {
                Ok(actual) if geometry_matches(actual, target) => {}
                Ok(actual) => {
                    return Err(self.normal_exit_handoff_error(
                        backend,
                        &prepared,
                        format!(
                            "visible geometry for {win:?} was not confirmed: expected {target:?}, observed {actual:?}"
                        ),
                    ));
                }
                Err(error) => {
                    return Err(self.normal_exit_handoff_error(
                        backend,
                        &prepared,
                        format!("confirm visible geometry for {win:?}: {error}"),
                    ));
                }
            }

            let mut committed_geometry = previous_client.geometry.clone();
            let legacy_fallback_x = previous_client
                .mon
                .and_then(|monitor| self.monitor_work_area(monitor))
                .map_or(desktop_left, |area| area.x);
            restore_hidden_geometry(&mut committed_geometry, desktop_left, legacy_fallback_x);
            // `target` may have been clamped away from a stale output. Keep
            // JWM's semantic snapshot identical to the server geometry that
            // the Phase-A proof actually confirmed.
            committed_geometry.x = target.x;
            committed_geometry.y = target.y;
            committed_geometry.w = i32::try_from(target.w).unwrap_or(i32::MAX);
            committed_geometry.h = i32::try_from(target.h).unwrap_or(i32::MAX);
            if let Some(client) = self.state.clients.get_mut(client_key) {
                client.geometry = committed_geometry;
            } else {
                return Err(self.normal_exit_handoff_error(
                    backend,
                    &prepared,
                    format!("managed client {win:?} vanished while committing handoff geometry"),
                ));
            }
        }

        Ok(prepared)
    }

    /// Prepare the complete normal-exit handoff transaction. Swallowed
    /// parents are deliberately last: their own helper is transactional, and
    /// if it fails all earlier client maps/geometries can still be reversed.
    pub(crate) fn prepare_normal_exit_handoff(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<NormalExitHandoff, NormalExitPrepareError> {
        if self.is_restarting.load(Ordering::SeqCst) {
            return Err(NormalExitPrepareError::new(
                "normal-exit handoff requested while seamless restart is active",
                Vec::new(),
                true,
            ));
        }
        let prepared = self.prepare_normal_exit_clients(backend)?;
        if let Err(error) = self.prepare_swallowed_parents_for_handoff(backend) {
            return Err(self.normal_exit_handoff_error(
                backend,
                &prepared,
                format!("swallowed-parent handoff failed after client preflight: {error}"),
            ));
        }
        Ok(NormalExitHandoff { _private: () })
    }

    fn record_config_reload_result(&mut self, success: bool, error: Option<String>) {
        self.config_reload_count = self.config_reload_count.saturating_add(1);
        self.config_reload_last_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX));
        self.config_reload_last_success = Some(success);
        self.config_reload_last_error = error;
    }

    pub fn cleanup(&mut self, backend: &mut dyn Backend) -> Result<(), Box<dyn Error>> {
        if self.is_restarting.load(Ordering::SeqCst) {
            // Application preflight already handed swallowed parents off as
            // its final fallible client stage. This is a defensive idempotent
            // no-op for that path and preserves the direct cleanup() contract
            // used outside the composition root. True-Iconic and parked
            // clients remain restart-preserved for the replacement to adopt.
            self.prepare_swallowed_parents_for_handoff(backend)?;
            self.cleanup_after_handoff(backend, false)
        } else {
            let handoff = self.prepare_normal_exit_handoff(backend)?;
            self.cleanup_after_normal_exit_handoff(backend, handoff)
        }
    }

    /// Enter destructive normal-exit Phase B only with proof that the global
    /// physical handoff barrier succeeded. The application obtains this proof
    /// before leaving its cancellable run loop.
    pub(crate) fn cleanup_after_normal_exit_handoff(
        &mut self,
        backend: &mut dyn Backend,
        _handoff: NormalExitHandoff,
    ) -> Result<(), Box<dyn Error>> {
        self.cleanup_after_handoff(backend, true)
    }

    fn cleanup_after_handoff(
        &mut self,
        backend: &mut dyn Backend,
        normal_exit_prepared: bool,
    ) -> Result<(), Box<dyn Error>> {
        info!("[cleanup] Starting essential cleanup (letting Rust handle memory)");
        // Before anything can fail: a layout changed seconds before a restart
        // is exactly the one the next process has to come back to.
        if let Err(error) = self.flush_layout_persistence_on_exit() {
            // Normal-exit cleanup is already beyond its physical handoff
            // commit point, while restart has validated this write before
            // entering cleanup. Record a late failure without skipping the
            // remaining X11/system teardown stages.
            warn!("[cleanup] could not flush pending layout persistence: {error}");
        }
        // Shut down IPC server (also handled by Drop, but explicit is clearer)
        if let Some(ref mut ipc) = self.ipc_server {
            ipc.shutdown();
        }
        self.ipc_server = None;
        let result = run_best_effort_cleanup(ESSENTIAL_CLEANUP_STAGES, |stage| match stage {
            EssentialCleanupStage::X11Resources => {
                self.cleanup_x11_resources_after_handoff(backend, normal_exit_prepared)
            }
            EssentialCleanupStage::SystemResources => self.cleanup_system_resources(),
            EssentialCleanupStage::ThemePixels => {
                boxed_cleanup_result(backend.color_allocator().free_all_theme_pixels())
            }
            EssentialCleanupStage::DisplayFlush => {
                boxed_cleanup_result(backend.window_ops().flush())
            }
        });
        if result.is_ok() {
            info!("[cleanup] Essential cleanup completed (Rust will handle the rest)");
        } else {
            warn!("[cleanup] Essential cleanup completed with failures");
        }
        result
    }

    fn cleanup_x11_resources_after_handoff(
        &mut self,
        backend: &mut dyn Backend,
        normal_exit_prepared: bool,
    ) -> Result<(), Box<dyn Error>> {
        info!("[cleanup_x11_resources] Cleaning X11 resources");

        // Stop recording on shutdown. We do NOT cross-restart resume: previously
        // that caused silent runaway recordings spanning many restarts. The
        // compositor now writes directly to the final Videos path, so no
        // temporary segment has to be recovered or moved after shutdown.
        if self.features.recording.active {
            backend.compositor_stop_recording();
            self.features.recording.stop();
            let target = self
                .features
                .recording
                .output_path
                .as_deref()
                .unwrap_or("(unset)");
            info!("[cleanup_x11_resources] Recording stopped on shutdown; output is at {target}");
        }

        if self.features.audio_recording.active {
            let path = self.features.audio_recording.output_path.clone();
            if let Err(error) = self.features.audio_recording.stop() {
                warn!("[cleanup_x11_resources] Failed to stop audio recording: {error}");
            } else {
                info!(
                    "[cleanup_x11_resources] Audio recording finalized: {}",
                    path.as_deref().unwrap_or("(unset)")
                );
            }
        }

        let result = run_best_effort_cleanup(X11_CLEANUP_STAGES, |stage| match stage {
            X11CleanupStage::ClientState => {
                if normal_exit_prepared {
                    self.commit_normal_exit_client_state(backend)
                } else {
                    self.cleanup_restarting_clients_x11_state(backend)
                }
            }
            X11CleanupStage::KeyGrabs => self.cleanup_key_grabs(backend),
            X11CleanupStage::InputFocus => self.reset_input_focus(backend),
            X11CleanupStage::Backend => boxed_cleanup_result(backend.cleanup()),
            X11CleanupStage::Cursor => boxed_cleanup_result(backend.cursor_provider().cleanup()),
        });
        if result.is_ok() {
            info!("[cleanup_x11_resources] X11 resources cleaned");
        } else {
            warn!("[cleanup_x11_resources] X11 cleanup completed with failures");
        }
        result
    }

    pub(crate) fn cleanup_system_resources(&mut self) -> Result<(), Box<dyn Error>> {
        info!("[cleanup_system_resources] Cleaning system resources");

        let result = run_best_effort_cleanup(SYSTEM_CLEANUP_STAGES, |stage| match stage {
            SystemCleanupStage::StatusBars => self.cleanup_statusbar_processes(),
            SystemCleanupStage::SharedMemory => self.cleanup_shared_memory_resources(),
        });
        if result.is_ok() {
            info!("[cleanup_system_resources] System resources cleaned");
        } else {
            warn!("[cleanup_system_resources] System cleanup completed with failures");
        }
        result
    }

    #[cfg(test)]
    pub(crate) fn cleanup_all_clients_x11_state(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn Error>> {
        info!("[cleanup_all_clients_x11_state]");
        if self.is_restarting.load(Ordering::SeqCst) {
            self.cleanup_restarting_clients_x11_state(backend)
        } else {
            let _handoff = self.prepare_normal_exit_handoff(backend)?;
            self.commit_normal_exit_client_state(backend)
        }
    }

    fn cleanup_restarting_clients_x11_state(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn Error>> {
        let mut failures = CleanupFailures::default();
        let clients_to_process =
            x11_client_cleanup_plan(&self.state.client_order, &self.state.clients);
        for (win, _, client_key) in clients_to_process {
            if !self.state.clients.contains_key(client_key) {
                continue;
            }
            if let Err(error) = self.persist_minimized_restore_state(backend, client_key) {
                // Keep the previous valid property if refreshing it fails.
                // Restart adoption may still use legacy state.
                warn!("Failed to refresh minimized restore state for {win:?}: {error}");
            }
            failures.record(
                format!("ungrab client buttons for {win:?}"),
                boxed_cleanup_result(backend.window_ops().ungrab_all_buttons(win)),
            );
        }
        failures.finish()
    }

    /// Phase B. The ownership-release pass is intentionally global: no client
    /// loses events, grabs or borders until Phase A has verified every parked
    /// client. Public protocol state is then retired in Hidden -> Withdrawn ->
    /// V1 order; later failures are reported while teardown remains best-effort.
    fn commit_normal_exit_client_state(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn Error>> {
        let clients_to_process =
            x11_client_cleanup_plan(&self.state.client_order, &self.state.clients);
        let mut failures = CleanupFailures::default();
        backend.compositor_set_minimized_window_preview(None, None);

        // Ownership release is one batch after the global physical barrier.
        for (win, old_border_w, client_key) in &clients_to_process {
            if !self.state.clients.contains_key(*client_key) {
                continue;
            }
            failures.record(
                format!("clear client event mask for {win:?}"),
                boxed_cleanup_result(
                    backend
                        .window_ops()
                        .change_event_mask(*win, EventMaskBits::NONE.bits()),
                ),
            );
            failures.record(
                format!("restore client border for {win:?}"),
                boxed_cleanup_result(backend.window_ops().apply_window_changes(
                    *win,
                    WindowChanges {
                        border_width: Some(
                            u32::try_from((*old_border_w).max(0)).unwrap_or(u32::MAX),
                        ),
                        ..Default::default()
                    },
                )),
            );
            failures.record(
                format!("ungrab client buttons for {win:?}"),
                boxed_cleanup_result(backend.window_ops().ungrab_all_buttons(*win)),
            );
        }

        // Retire public state only after every client crossed the ownership
        // barrier. Keep V1 when Withdrawn could not be written so another JWM
        // still has recovery metadata rather than a half-retired client.
        for (win, _, client_key) in clients_to_process {
            if !self.state.clients.contains_key(client_key) {
                continue;
            }
            failures.record(
                format!("clear EWMH Hidden for {win:?}"),
                boxed_cleanup_result(backend.property_ops().set_net_wm_state_flag(
                    win,
                    crate::backend::api::NetWmState::Hidden,
                    false,
                )),
            );
            let withdrew =
                match self.setclientstate(backend, win, crate::jwm::WITHDRAWN_STATE as i64) {
                    Ok(()) => true,
                    Err(error) => {
                        failures.record(format!("write WithdrawnState for {win:?}"), Err(error));
                        false
                    }
                };

            if withdrew {
                failures.record(
                    format!("clear minimized restore state for {win:?}"),
                    boxed_cleanup_result(backend.property_ops().clear_minimized_restore_state(win)),
                );
            }
            if let Some(client) = self.state.clients.get_mut(client_key) {
                client.state.is_hidden = false;
                client.state.minimized_order = 0;
            }
            backend.compositor_set_window_dock_geometry(win, None);
        }

        failures.finish()
    }

    pub(crate) fn cleanup_statusbar_processes(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // Clean up secondary bars
        self.cleanup_secondary_bars()?;
        Ok(())
    }

    pub(crate) fn cleanup_secondary_bars(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let bars = std::mem::take(&mut self.secondary_bars);
        for (mon_id, mut bar) in bars {
            self.unregister_secondary_bar_readiness(&bar);
            match super::monitor_management::terminate_secondary_bar_child(
                &mut bar.child,
                Duration::from_secs(3),
            ) {
                Ok(status) => info!("Secondary bar {} exited: {:?}", mon_id, status),
                Err(error) => warn!(
                    "Could not stop and reap secondary bar {}: {}",
                    mon_id, error
                ),
            }
        }
        Ok(())
    }

    pub(crate) fn cleanup_shared_memory_resources(
        &mut self,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Clean up all monitor bars shared memory
        let bars = std::mem::take(&mut self.secondary_bars);
        for (mon_id, mut bar) in bars {
            self.unregister_secondary_bar_readiness(&bar);
            if let Err(error) = super::monitor_management::terminate_secondary_bar_child(
                &mut bar.child,
                Duration::from_secs(3),
            ) {
                warn!(
                    "Could not stop and reap secondary bar {} before shared-memory cleanup: {}",
                    mon_id, error
                );
            }
            drop(bar);
            #[cfg(unix)]
            {
                let path = format!("/dev/shm/jwm_bar_mon_{}", mon_id);
                if std::path::Path::new(&path).exists() {
                    if let Err(e) = std::fs::remove_file(&path) {
                        warn!("Failed to remove {}: {}", path, e);
                    }
                }
            }
        }

        Ok(())
    }

    pub(crate) fn cleanup_key_grabs(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn Error>> {
        backend
            .key_ops()
            .clear_key_grabs(backend.root_window().expect("no root window"))?;
        Ok(())
    }
    fn perform_config_reload(&mut self, backend: &mut dyn Backend) -> IpcResponse {
        match crate::config::reload_global() {
            Ok(()) => {
                self.record_config_reload_result(true, None);
                self.apply_config_changes(backend);
                backend.compositor_push_toast(crate::backend::api::ToastNotification {
                    title: "\u{f021}  Configuration reloaded".into(),
                    body: String::new(),
                    urgency: 1,
                    timeout_ms: 2500,
                    ..Default::default()
                });
                self.broadcast_ipc_event(
                    "config/reload",
                    serde_json::json!({
                        "success": true,
                        "reload_count": self.config_reload_count,
                        "last_reload_unix_ms": self.config_reload_last_unix_ms,
                    }),
                );
                IpcResponse::ok(None)
            }
            Err(e) => {
                let error = format!("config reload failed: {e}");
                self.record_config_reload_result(false, Some(error.clone()));
                backend.compositor_push_toast(crate::backend::api::ToastNotification {
                    title: "\u{f071}  Configuration reload failed".into(),
                    body: e.to_string(),
                    urgency: 2,
                    timeout_ms: 8000,
                    ..Default::default()
                });
                self.broadcast_ipc_event(
                    "config/reload",
                    serde_json::json!({
                        "success": false,
                        "reload_count": self.config_reload_count,
                        "last_reload_unix_ms": self.config_reload_last_unix_ms,
                        "error": error,
                    }),
                );
                IpcResponse::err(error)
            }
        }
    }

    /// Explicit reloads (for example IPC) bypass the debounce but still settle
    /// the current revision so the polling fallback cannot reload it again.
    pub(crate) fn do_config_reload(&mut self, backend: &mut dyn Backend) -> IpcResponse {
        if let Ok(revision) = crate::config::Config::get_config_modified_time() {
            self.config_reload_tracker.mark_attempted(revision);
            self.config_last_modified = Some(revision);
        }
        self.config_reload_debounce = None;
        self.perform_config_reload(backend)
    }

    /// Fast-path notification used by backends with inotify support. The
    /// periodic poll below uses the same state, preventing duplicate reloads.
    pub(crate) fn observe_config_reload(&mut self, now: Instant, source: &str) {
        let Ok(revision) = crate::config::Config::get_config_modified_time() else {
            // Atomic replacement can briefly make the path unavailable. The
            // next update tick will observe the completed file.
            return;
        };

        if self.config_reload_tracker.observe(revision, now) {
            self.config_last_modified = Some(revision);
            self.config_reload_debounce = Some(now);
            info!("[config] file change detected via {source}; waiting for the revision to settle");
        }
    }

    /// Backend-neutral fallback run from every backend's periodic update.
    pub(crate) fn poll_config_reload(&mut self, backend: &mut dyn Backend, now: Instant) {
        let polled_now = self.config_reload_tracker.should_poll(now);
        if polled_now {
            self.observe_config_reload(now, "mtime poll");
        }

        // Re-stat once at the debounce boundary even within the one-second
        // polling interval. If an atomic-save burst produced a newer revision,
        // observing it here restarts the debounce instead of loading an older
        // revision and then loading the final revision again on the next poll.
        if !polled_now && self.config_reload_tracker.pending_is_due(now) {
            self.observe_config_reload(now, "debounce verification");
        }

        if self.config_reload_tracker.take_due_attempt(now).is_none() {
            return;
        }
        self.config_reload_debounce = None;

        info!("[config] debounced config revision is stable, reloading");
        let response = self.perform_config_reload(backend);
        if response.success {
            info!("[config] reload successful");
        } else {
            warn!("[config] reload failed: {:?}", response.error);
        }
    }

    /// Whether an edit to the config file is waiting out its debounce.
    pub(crate) fn config_reload_is_pending(&self) -> bool {
        self.config_reload_tracker.has_pending()
    }

    /// Record a revision JWM wrote itself — the per-tag layout save is the
    /// only one today. Without this the watcher would see the new mtime as an
    /// edit and reload the config a second or two after every layout change.
    pub(crate) fn note_config_written_by_us(&mut self, revision: SystemTime) {
        self.config_reload_tracker.mark_attempted(revision);
        self.config_last_modified = Some(revision);
    }

    pub(crate) fn config_reload_next_wakeup(&self, now: Instant) -> Duration {
        self.config_reload_tracker.next_wakeup_in(now)
    }

    pub(crate) fn apply_config_changes(&mut self, backend: &mut dyn Backend) {
        let cfg = CONFIG.load();

        // 1. Rebind keys
        self.key_bindings = cfg.get_keys();
        self.chord_compiled = cfg.compile_chord();
        self.chord_armed_until = None;
        if let Err(e) = self.grabkeys(backend) {
            warn!("[config] failed to re-grab keys: {e}");
        }
        // Pick up DND default from config (without overriding a runtime toggle: only
        // when the config value differs from our default-on-startup, refresh).
        // Simpler: trust config — reload reflects user's saved preference.
        self.do_not_disturb = cfg.behavior().do_not_disturb;

        // Config hot-disable must close already-active modal features as well
        // as gate future entry. Otherwise JWM can keep an invisible keyboard
        // or pointer grab after the compositor has correctly dropped its
        // corresponding visual state.
        if self.features.overview.active && !cfg.behavior().overview_enabled {
            self.features.overview.deactivate();
            backend.compositor_set_overview_mode(false, &[]);
            let _ = backend.key_ops().ungrab_keyboard();
            info!("[config] closed overview after overview_enabled=false");
        }
        if self.features.expose_active && !cfg.behavior().expose_enabled {
            self.features.expose_active = false;
            backend.compositor_set_expose_mode(false, Vec::new());
            let _ = backend.key_ops().ungrab_keyboard();
            let _ = backend.input_ops().ungrab_pointer();
            info!("[config] closed expose after expose_enabled=false");
        }
        if self.features.peek_active && !cfg.behavior().peek_enabled {
            self.features.peek_active = false;
            backend.compositor_set_peek_mode(false);
            info!("[config] closed peek after peek_enabled=false");
        }
        if self.features.screenshot.active {
            // The selector can remain open across a config reload. Apply the
            // new scene policy immediately instead of waiting for the next
            // screenshot session.
            backend.compositor_set_screenshot_freeze(cfg.behavior().screenshot_freeze_enabled);
        }

        // 2. Re-apply color schemes
        let colors = cfg.colors();
        let alloc = backend.color_allocator();
        let _ = alloc.free_all_theme_pixels();
        if let (Ok(norm_fg), Ok(norm_bg), Ok(norm_border)) = (
            ArgbColor::from_hex(&colors.dark_sea_green1, colors.opaque),
            ArgbColor::from_hex(&colors.light_sky_blue1, colors.opaque),
            ArgbColor::from_hex(&colors.light_sky_blue1, colors.opaque),
        ) {
            alloc.set_scheme(
                SchemeType::Norm,
                ColorScheme::new(norm_fg, norm_bg, norm_border),
            );
        }
        if let (Ok(sel_fg), Ok(sel_bg), Ok(sel_border)) = (
            ArgbColor::from_hex(&colors.dark_sea_green2, colors.opaque),
            ArgbColor::from_hex(&colors.pale_turquoise1, colors.opaque),
            ArgbColor::from_hex(&colors.cyan, colors.opaque),
        ) {
            alloc.set_scheme(
                SchemeType::Sel,
                ColorScheme::new(sel_fg, sel_bg, sel_border),
            );
        }
        if let (Ok(urgent_fg), Ok(urgent_bg)) = (
            ArgbColor::from_hex(&colors.dark_sea_green1, colors.opaque),
            ArgbColor::from_hex(&colors.light_sky_blue1, colors.opaque),
        ) {
            alloc.set_scheme(
                SchemeType::Urgent,
                ColorScheme::new(
                    urgent_fg,
                    urgent_bg,
                    ArgbColor::from_rgba_f32(cfg.behavior().attention_color),
                ),
            );
        }
        let _ = alloc.allocate_schemes_pixels();

        // 3. Re-arrange all monitors (border/gap changes take effect)
        let mon_keys: Vec<MonitorKey> = self.state.monitor_order.clone();
        for mk in &mon_keys {
            self.arrange(backend, Some(*mk));
        }

        // 4. Update decoration on all visible clients
        let sel_ck = self.get_selected_client_key();

        // 5. Apply settings to an already-running compositor before any mode
        // transition. A newly-created compositor applies config inside the
        // reconciled hand-off and then replays runtime state; applying config
        // again afterwards would overwrite Night Light/HUD/idle-dim state.
        backend.compositor_apply_config();

        // 6. Toggle compositor if config changed
        let compositor_wanted = if matches!(self.runtime_backend.as_str(), "x11rb" | "xcb") {
            crate::config::effective_x11_compositor_enabled(cfg.compositor_enabled())
        } else {
            cfg.compositor_enabled()
        };
        let compositor_active = backend.has_compositor();
        if compositor_wanted {
            // A config change to ON converts any system-UI lease into the
            // user's persistent compositor; closing the panel must keep it.
            self.features.system_ui_temporary_compositor = false;
        }
        let defer_compositor_disable =
            !compositor_wanted && compositor_active && self.features.system_ui.is_active();
        if defer_compositor_disable {
            // Never tear the renderer out from under a modal launcher or lock
            // screen. The common close path applies the requested OFF state.
            self.features.system_ui_temporary_compositor = true;
            log::info!("Deferring compositor disable until the system UI closes");
        } else if !compositor_wanted
            && compositor_active
            && let Err(error) = self.prepare_for_compositor_disable(backend)
        {
            // Config reload is another compositor-loss entry point. Keep the
            // active renderer when a modal cleanup, recording finalization or
            // hidden-client parking barrier could not complete safely.
            log::warn!(
                "Compositor remains ON after config reload; disable preparation failed: {error}"
            );
        } else if compositor_wanted != compositor_active {
            match self.set_compositor_enabled_reconciled(backend, compositor_wanted) {
                Ok(true) => log::info!(
                    "Compositor {}",
                    if compositor_wanted {
                        "enabled"
                    } else {
                        "disabled"
                    }
                ),
                Ok(false) => {}
                Err(e) => log::warn!("Failed to set compositor: {e}"),
            }
        }

        // 7. Hot-reload the cursor theme/size on backends that install a themed
        // pointer (X11RB/XCB). When it changed, re-apply the default arrow to the
        // root window so the new shape/size becomes visible immediately.
        match backend.cursor_provider().reload_theme() {
            Ok(true) => {
                if let Some(root) = backend.root_window() {
                    if let Err(e) = backend
                        .cursor_provider()
                        .apply(root, crate::backend::common_define::StdCursorKind::LeftPtr)
                    {
                        warn!("[config] re-applying root cursor failed: {e}");
                    }
                }
            }
            Ok(false) => {}
            Err(e) => warn!("[config] cursor theme reload failed: {e}"),
        }

        let client_keys: Vec<ClientKey> = self.state.client_order.clone();
        for ck in client_keys {
            if let Some(_client) = self.state.clients.get(ck) {
                let is_sel = sel_ck == Some(ck);
                let _ = self.update_client_decoration(backend, ck, is_sel);
            }
        }

        // 8. A new wallpaper means new accent colours, when the user asked for
        // them. The decode runs on a worker; the frame tick adopts the result.
        self.refresh_wallpaper_theme();

        // 9. Config application/recreation may resend configured brightness;
        // preserve an idle dim that is still meant to be in effect.
        self.reapply_idle_dim(backend);
    }
}

#[cfg(test)]
mod config_reload_tests {
    use super::*;

    fn revision(seconds: u64) -> SystemTime {
        std::time::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn unchanged_revision_does_not_schedule_reload() {
        let now = Instant::now();
        let original = revision(1);
        let mut tracker = ConfigReloadTracker::new(Some(original));

        assert!(!tracker.observe(original, now));
        assert_eq!(tracker.take_due_attempt(now + CONFIG_RELOAD_DEBOUNCE), None);
        assert!(tracker.pending.is_none());
    }

    #[test]
    fn mtime_poll_is_gated_until_interval_expires() {
        let now = Instant::now();
        let mut tracker = ConfigReloadTracker::new(Some(revision(1)));
        let just_before_interval = (now + CONFIG_RELOAD_POLL_INTERVAL)
            .checked_sub(Duration::from_millis(1))
            .unwrap();

        assert!(tracker.should_poll(now));
        assert!(!tracker.should_poll(now + Duration::from_millis(250)));
        assert!(!tracker.should_poll(just_before_interval));
        assert!(tracker.should_poll(now + CONFIG_RELOAD_POLL_INTERVAL));
        assert!(!tracker.should_poll(now + CONFIG_RELOAD_POLL_INTERVAL));
    }

    #[test]
    fn poll_deadline_counts_down_without_continuous_ticks() {
        let now = Instant::now();
        let mut tracker = ConfigReloadTracker::new(Some(revision(1)));

        assert_eq!(tracker.next_wakeup_in(now), Duration::ZERO);
        assert!(tracker.deadline_is_due(now));

        assert!(tracker.should_poll(now));
        assert_eq!(
            tracker.next_wakeup_in(now + Duration::from_millis(250)),
            Duration::from_millis(750)
        );
        let just_before_interval = (now + CONFIG_RELOAD_POLL_INTERVAL)
            .checked_sub(Duration::from_millis(1))
            .unwrap();
        assert!(!tracker.deadline_is_due(just_before_interval));
        assert!(tracker.deadline_is_due(now + CONFIG_RELOAD_POLL_INTERVAL));
    }

    #[test]
    fn next_wakeup_uses_earliest_poll_or_debounce_deadline() {
        let now = Instant::now();
        let mut tracker = ConfigReloadTracker::new(Some(revision(1)));
        assert!(tracker.should_poll(now));

        let changed_at = now + Duration::from_millis(800);
        assert!(tracker.observe(revision(2), changed_at));
        // The next mtime poll is 200ms away, earlier than the 300ms debounce.
        assert_eq!(
            tracker.next_wakeup_in(changed_at),
            Duration::from_millis(200)
        );

        assert!(tracker.should_poll(now + CONFIG_RELOAD_POLL_INTERVAL));
        // Once the earlier poll is serviced, the remaining debounce deadline
        // becomes the next wakeup.
        assert_eq!(
            tracker.next_wakeup_in(now + CONFIG_RELOAD_POLL_INTERVAL),
            Duration::from_millis(100)
        );
        assert!(tracker.deadline_is_due(changed_at + CONFIG_RELOAD_DEBOUNCE));
    }

    #[test]
    fn changed_revision_reloads_once_after_debounce() {
        let now = Instant::now();
        let changed = revision(2);
        let mut tracker = ConfigReloadTracker::new(Some(revision(1)));
        let just_before_debounce = (now + CONFIG_RELOAD_DEBOUNCE)
            .checked_sub(Duration::from_millis(1))
            .unwrap();

        assert!(tracker.observe(changed, now));
        assert!(!tracker.observe(changed, now + Duration::from_millis(100)));
        assert_eq!(tracker.take_due_attempt(just_before_debounce), None);
        assert_eq!(
            tracker.take_due_attempt(now + CONFIG_RELOAD_DEBOUNCE),
            Some(changed)
        );

        assert!(!tracker.observe(changed, now + CONFIG_RELOAD_DEBOUNCE));
        assert_eq!(
            tracker.take_due_attempt(now + CONFIG_RELOAD_DEBOUNCE * 2),
            None
        );
    }

    #[test]
    fn newer_revision_restarts_debounce_window() {
        let now = Instant::now();
        let first_change = revision(2);
        let final_change = revision(3);
        let burst_gap = Duration::from_millis(100);
        let mut tracker = ConfigReloadTracker::new(Some(revision(1)));

        assert!(tracker.observe(first_change, now));
        assert!(tracker.observe(final_change, now + burst_gap));
        assert_eq!(tracker.take_due_attempt(now + CONFIG_RELOAD_DEBOUNCE), None);
        assert_eq!(
            tracker.take_due_attempt(now + burst_gap + CONFIG_RELOAD_DEBOUNCE),
            Some(final_change)
        );
    }

    #[test]
    fn failed_attempt_waits_for_next_revision() {
        let now = Instant::now();
        let malformed = revision(2);
        let fixed = revision(3);
        let mut tracker = ConfigReloadTracker::new(Some(revision(1)));

        assert!(tracker.observe(malformed, now));
        // Taking the due revision records the attempt before parsing. Simulate
        // a parse failure by intentionally providing no success callback.
        assert_eq!(
            tracker.take_due_attempt(now + CONFIG_RELOAD_DEBOUNCE),
            Some(malformed)
        );
        assert!(!tracker.observe(malformed, now + CONFIG_RELOAD_DEBOUNCE * 2));
        assert_eq!(
            tracker.take_due_attempt(now + CONFIG_RELOAD_DEBOUNCE * 2),
            None
        );

        assert!(tracker.observe(fixed, now + CONFIG_RELOAD_DEBOUNCE * 2));
        assert_eq!(
            tracker.take_due_attempt(now + CONFIG_RELOAD_DEBOUNCE * 3),
            Some(fixed)
        );
    }
}

#[cfg(test)]
mod client_cleanup_plan_tests {
    use super::*;

    #[test]
    fn normal_shutdown_includes_clients_parked_outside_monitor_stacks() {
        let mut clients = slotmap::SlotMap::with_key();
        let attached = clients.insert(WMClient::new(WindowId::from_raw(41)));
        let parked = clients.insert(WMClient::new(WindowId::from_raw(42)));

        // A parked scratchpad has no monitor/stack membership. `client_order`
        // remains the authoritative registry for both ordinary and parked
        // managed clients.
        assert!(clients[parked].mon.is_none());
        let plan = x11_client_cleanup_plan(&[attached, parked], &clients);

        assert_eq!(
            plan.iter()
                .map(|(window, ..)| window.raw())
                .collect::<Vec<_>>(),
            vec![41, 42]
        );
    }
}

#[cfg(test)]
mod cleanup_failure_tests {
    use super::{
        CleanupResult, ESSENTIAL_CLEANUP_STAGES, EssentialCleanupStage, X11_CLEANUP_STAGES,
        X11CleanupStage, run_best_effort_cleanup,
    };
    use std::error::Error;
    use std::io;

    fn injected_failure(message: &'static str) -> CleanupResult {
        Err(Box::new(io::Error::other(message)))
    }

    #[test]
    fn essential_cleanup_reaches_system_and_final_stages_after_failures() {
        let mut calls = Vec::new();
        let result = run_best_effort_cleanup(ESSENTIAL_CLEANUP_STAGES, |stage| {
            calls.push(stage);
            match stage {
                EssentialCleanupStage::X11Resources => injected_failure("injected X11 failure"),
                EssentialCleanupStage::ThemePixels => injected_failure("injected theme failure"),
                EssentialCleanupStage::SystemResources | EssentialCleanupStage::DisplayFlush => {
                    Ok(())
                }
            }
        });

        assert_eq!(
            calls,
            vec![
                EssentialCleanupStage::X11Resources,
                EssentialCleanupStage::SystemResources,
                EssentialCleanupStage::ThemePixels,
                EssentialCleanupStage::DisplayFlush,
            ]
        );
        let error = result.expect_err("injected cleanup failures must be returned");
        let rendered = error.to_string();
        assert!(rendered.contains("X11 resources: injected X11 failure"));
        assert!(rendered.contains("theme pixels: injected theme failure"));
        assert_eq!(
            Error::source(error.as_ref()).map(ToString::to_string),
            Some("injected X11 failure".to_string()),
            "the primary failure remains the aggregate error source"
        );
    }

    #[test]
    fn x11_cleanup_continues_after_client_state_failure() {
        let mut calls = Vec::new();
        let result = run_best_effort_cleanup(X11_CLEANUP_STAGES, |stage| {
            calls.push(stage);
            if stage == X11CleanupStage::ClientState {
                injected_failure("injected client cleanup failure")
            } else {
                Ok(())
            }
        });

        assert!(result.is_err());
        assert_eq!(
            calls,
            vec![
                X11CleanupStage::ClientState,
                X11CleanupStage::KeyGrabs,
                X11CleanupStage::InputFocus,
                X11CleanupStage::Backend,
                X11CleanupStage::Cursor,
            ]
        );
    }
}

#[cfg(test)]
mod normal_exit_transaction_tests {
    use super::*;
    use crate::backend::api::{
        BackendDiagnostics, Capabilities, CloseResult, ColorAllocator, CompositorAnnotation,
        CompositorBenchmark, CompositorControl, CompositorMedia, CompositorWindowEffects,
        CompositorWorkspaceEffects, CursorProvider, DisplayControl, InputOps, KeyOps, OutputOps,
        PropertyOps, RenderScheduler, WindowAttributes, WindowOps,
    };
    use crate::backend::common_define::Pixel;
    use crate::backend::error::BackendError;
    use crate::backend::wayland_dummy_ops::{
        DummyColorAllocator, DummyCursorProvider, DummyInputOps, DummyKeyOps, DummyOutputOps,
        DummyPropertyOps,
    };
    use crate::core::types::Rect;
    use std::any::Any;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ExitOperation {
        Configure(WindowId, i32),
        Map(WindowId),
        UnmapSwallowed(WindowId),
        CancelIconic(WindowId),
        RequestIconic(WindowId),
        ReleaseOwnership(WindowId),
        BackendCleanup,
    }

    #[derive(Clone, Copy, Debug)]
    struct ServerWindow {
        geometry: Geometry,
        viewable: bool,
    }

    struct ExitWindowOps {
        windows: Mutex<HashMap<WindowId, ServerWindow>>,
        operations: Arc<Mutex<Vec<ExitOperation>>>,
        fail_visible_configure_once_for: AtomicU64,
        fail_rollback_configure_once_for: AtomicU64,
        fail_map_once_for: AtomicU64,
        fail_unmap_once_for: AtomicU64,
        fail_attributes_for: AtomicU64,
        fail_attributes_on_call: AtomicU64,
        attribute_calls: Mutex<HashMap<WindowId, u64>>,
    }

    impl ExitWindowOps {
        fn new(operations: Arc<Mutex<Vec<ExitOperation>>>) -> Self {
            Self {
                windows: Mutex::new(HashMap::new()),
                operations,
                fail_visible_configure_once_for: AtomicU64::new(0),
                fail_rollback_configure_once_for: AtomicU64::new(0),
                fail_map_once_for: AtomicU64::new(0),
                fail_unmap_once_for: AtomicU64::new(0),
                fail_attributes_for: AtomicU64::new(0),
                fail_attributes_on_call: AtomicU64::new(0),
                attribute_calls: Mutex::new(HashMap::new()),
            }
        }

        fn insert(&self, window: WindowId, geometry: Geometry, viewable: bool) {
            self.windows
                .lock()
                .expect("server windows lock")
                .insert(window, ServerWindow { geometry, viewable });
        }

        fn snapshot(&self, window: WindowId) -> ServerWindow {
            self.windows.lock().expect("server windows lock")[&window]
        }

        fn set_viewable(&self, window: WindowId, viewable: bool) -> Result<(), BackendError> {
            let mut windows = self.windows.lock().expect("server windows lock");
            let state = windows
                .get_mut(&window)
                .ok_or_else(|| BackendError::Message(format!("unknown test window {window:?}")))?;
            state.viewable = viewable;
            Ok(())
        }
    }

    impl WindowOps for ExitWindowOps {
        fn set_position(&self, window: WindowId, x: i32, y: i32) -> Result<(), BackendError> {
            let geometry = self.get_geometry(window)?;
            self.configure(window, x, y, geometry.w, geometry.h, geometry.border)
        }

        fn configure(
            &self,
            window: WindowId,
            x: i32,
            y: i32,
            width: u32,
            height: u32,
            border: u32,
        ) -> Result<(), BackendError> {
            self.operations
                .lock()
                .expect("exit operations lock")
                .push(ExitOperation::Configure(window, x));
            if x >= 0
                && self
                    .fail_visible_configure_once_for
                    .compare_exchange(
                        window.raw(),
                        0,
                        AtomicOrdering::SeqCst,
                        AtomicOrdering::SeqCst,
                    )
                    .is_ok()
            {
                self.fail_rollback_configure_once_for
                    .store(window.raw(), AtomicOrdering::SeqCst);
                return Err(BackendError::Message(
                    "injected visible geometry failure".into(),
                ));
            }
            if x < 0
                && self
                    .fail_rollback_configure_once_for
                    .compare_exchange(
                        window.raw(),
                        0,
                        AtomicOrdering::SeqCst,
                        AtomicOrdering::SeqCst,
                    )
                    .is_ok()
            {
                return Err(BackendError::Message(
                    "injected rollback geometry failure".into(),
                ));
            }
            let mut windows = self.windows.lock().expect("server windows lock");
            let state = windows
                .get_mut(&window)
                .ok_or_else(|| BackendError::Message(format!("unknown test window {window:?}")))?;
            state.geometry = Geometry {
                x,
                y,
                w: width,
                h: height,
                border,
            };
            Ok(())
        }

        fn set_decoration_style(
            &self,
            _window: WindowId,
            _border_width: u32,
            _border_color: Pixel,
        ) -> Result<(), BackendError> {
            Ok(())
        }

        fn raise_window(&self, _window: WindowId) -> Result<(), BackendError> {
            Ok(())
        }

        fn map_window(&self, window: WindowId) -> Result<(), BackendError> {
            self.operations
                .lock()
                .expect("exit operations lock")
                .push(ExitOperation::Map(window));
            if self
                .fail_map_once_for
                .compare_exchange(
                    window.raw(),
                    0,
                    AtomicOrdering::SeqCst,
                    AtomicOrdering::SeqCst,
                )
                .is_ok()
            {
                return Err(BackendError::Message("injected map failure".into()));
            }
            self.set_viewable(window, true)
        }

        fn unmap_window(&self, window: WindowId) -> Result<(), BackendError> {
            self.set_viewable(window, false)
        }

        fn unmap_managed_window(
            &self,
            window: WindowId,
            reason: ManagedUnmapReason,
        ) -> Result<(), BackendError> {
            if reason == ManagedUnmapReason::SwallowDiscard {
                self.operations
                    .lock()
                    .expect("exit operations lock")
                    .push(ExitOperation::UnmapSwallowed(window));
            }
            if self
                .fail_unmap_once_for
                .compare_exchange(
                    window.raw(),
                    0,
                    AtomicOrdering::SeqCst,
                    AtomicOrdering::SeqCst,
                )
                .is_ok()
            {
                return Err(BackendError::Message("injected unmap failure".into()));
            }
            self.unmap_window(window)
        }

        fn close_window(&self, _window: WindowId) -> Result<CloseResult, BackendError> {
            Ok(CloseResult::Graceful)
        }

        fn set_input_focus(&self, _window: WindowId) -> Result<(), BackendError> {
            Ok(())
        }

        fn set_input_focus_root(&self) -> Result<(), BackendError> {
            Ok(())
        }

        fn get_window_attributes(
            &self,
            window: WindowId,
        ) -> Result<WindowAttributes, BackendError> {
            let call = {
                let mut calls = self.attribute_calls.lock().expect("attribute calls lock");
                let call = calls.entry(window).or_insert(0);
                *call = call.saturating_add(1);
                *call
            };
            if self.fail_attributes_for.load(AtomicOrdering::SeqCst) == window.raw()
                && self.fail_attributes_on_call.load(AtomicOrdering::SeqCst) == call
            {
                return Err(BackendError::Message(
                    "injected attributes query failure".into(),
                ));
            }
            let state = self.snapshot(window);
            Ok(WindowAttributes {
                override_redirect: false,
                map_state_viewable: state.viewable,
            })
        }

        fn get_geometry(&self, window: WindowId) -> Result<Geometry, BackendError> {
            Ok(self.snapshot(window).geometry)
        }

        fn scan_windows(&self) -> Result<Vec<WindowId>, BackendError> {
            Ok(Vec::new())
        }

        fn flush(&self) -> Result<(), BackendError> {
            Ok(())
        }

        fn kill_client(&self, _window: WindowId) -> Result<(), BackendError> {
            Ok(())
        }

        fn apply_window_changes(
            &self,
            _window: WindowId,
            _changes: WindowChanges,
        ) -> Result<(), BackendError> {
            Ok(())
        }

        fn change_event_mask(&self, window: WindowId, _mask: u32) -> Result<(), BackendError> {
            self.operations
                .lock()
                .expect("exit operations lock")
                .push(ExitOperation::ReleaseOwnership(window));
            Ok(())
        }
    }

    struct ExitBackend {
        window_ops: ExitWindowOps,
        input_ops: DummyInputOps,
        property_ops: DummyPropertyOps,
        output_ops: DummyOutputOps,
        key_ops: DummyKeyOps,
        cursor_provider: DummyCursorProvider,
        color_allocator: DummyColorAllocator,
        operations: Arc<Mutex<Vec<ExitOperation>>>,
    }

    impl ExitBackend {
        fn new() -> Self {
            let operations = Arc::new(Mutex::new(Vec::new()));
            Self {
                window_ops: ExitWindowOps::new(operations.clone()),
                input_ops: DummyInputOps,
                property_ops: DummyPropertyOps,
                output_ops: DummyOutputOps,
                key_ops: DummyKeyOps,
                cursor_provider: DummyCursorProvider,
                color_allocator: DummyColorAllocator,
                operations,
            }
        }
    }

    impl CompositorBenchmark for ExitBackend {}
    impl BackendDiagnostics for ExitBackend {}
    impl CompositorControl for ExitBackend {}
    impl CompositorMedia for ExitBackend {}
    impl CompositorWorkspaceEffects for ExitBackend {}
    impl CompositorAnnotation for ExitBackend {}
    impl DisplayControl for ExitBackend {}
    impl RenderScheduler for ExitBackend {
        fn has_compositor(&self) -> bool {
            true
        }
    }
    impl CompositorWindowEffects for ExitBackend {
        fn compositor_cancel_window_iconify(
            &mut self,
            window: WindowId,
        ) -> Result<(), BackendError> {
            self.operations
                .lock()
                .expect("exit operations lock")
                .push(ExitOperation::CancelIconic(window));
            self.window_ops.set_viewable(window, true)
        }

        fn compositor_request_window_iconify(
            &mut self,
            window: WindowId,
        ) -> Result<(), BackendError> {
            self.operations
                .lock()
                .expect("exit operations lock")
                .push(ExitOperation::RequestIconic(window));
            self.window_ops.set_viewable(window, false)
        }
    }

    impl Backend for ExitBackend {
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                supports_client_list: true,
                ..Capabilities::default()
            }
        }

        fn root_window(&self) -> Option<WindowId> {
            Some(WindowId::from_raw(1))
        }

        fn as_any(&self) -> &dyn Any {
            self
        }

        fn check_existing_wm(&self) -> Result<(), BackendError> {
            Ok(())
        }

        fn window_ops(&self) -> &dyn WindowOps {
            &self.window_ops
        }

        fn input_ops(&self) -> &dyn InputOps {
            &self.input_ops
        }

        fn property_ops(&self) -> &dyn PropertyOps {
            &self.property_ops
        }

        fn output_ops(&self) -> &dyn OutputOps {
            &self.output_ops
        }

        fn key_ops(&self) -> &dyn KeyOps {
            &self.key_ops
        }

        fn key_ops_mut(&mut self) -> &mut dyn KeyOps {
            &mut self.key_ops
        }

        fn cursor_provider(&mut self) -> &mut dyn CursorProvider {
            &mut self.cursor_provider
        }

        fn color_allocator(&mut self) -> &mut dyn ColorAllocator {
            &mut self.color_allocator
        }

        fn cleanup(&mut self) -> Result<(), BackendError> {
            self.operations
                .lock()
                .expect("exit operations lock")
                .push(ExitOperation::BackendCleanup);
            Ok(())
        }

        fn run(
            &mut self,
            _handler: &mut dyn crate::backend::api::EventHandler,
        ) -> Result<(), BackendError> {
            Ok(())
        }
    }

    fn add_iconic_client(
        jwm: &mut Jwm,
        backend: &ExitBackend,
        window: WindowId,
        hidden_x: i32,
        visible_x: i32,
    ) -> ClientKey {
        let monitor = jwm.state.monitor_order[0];
        let mut client = WMClient::new(window);
        client.mon = Some(monitor);
        client.state.tags = 1;
        client.state.is_hidden = true;
        client.state.minimized_order = window.raw();
        client.geometry.x = hidden_x;
        client.geometry.y = 70;
        client.geometry.w = 640;
        client.geometry.h = 480;
        client.geometry.hidden_x = Some(hidden_x);
        client.geometry.hidden_restore_rect = Some(Rect::new(visible_x, 70, 640, 480));
        let client_key = jwm.state.clients.insert(client);
        jwm.state.client_order.push(client_key);
        jwm.state.win_to_client.insert(window, client_key);
        backend.window_ops.insert(
            window,
            Geometry {
                x: hidden_x,
                y: 70,
                w: 640,
                h: 480,
                border: 0,
            },
            false,
        );
        client_key
    }

    fn add_swallowed_offtag_client(
        jwm: &mut Jwm,
        backend: &ExitBackend,
        window: WindowId,
        hidden_x: i32,
        visible_x: i32,
    ) -> ClientKey {
        let monitor = jwm.state.monitor_order[0];
        let mut client = WMClient::new(window);
        client.mon = Some(monitor);
        client.state.tags = 1 << 4;
        client.state.is_swallowed = true;
        client.geometry.x = hidden_x;
        client.geometry.y = 90;
        client.geometry.w = 600;
        client.geometry.h = 420;
        client.geometry.hidden_x = Some(hidden_x);
        client.geometry.hidden_restore_rect = Some(Rect::new(visible_x, 90, 600, 420));
        let client_key = jwm.state.clients.insert(client);
        jwm.state.client_order.push(client_key);
        jwm.state.win_to_client.insert(window, client_key);
        backend.window_ops.insert(
            window,
            Geometry {
                x: hidden_x,
                y: 90,
                w: 600,
                h: 420,
                border: 0,
            },
            false,
        );
        client_key
    }

    fn add_parked_offtag_client(
        jwm: &mut Jwm,
        backend: &ExitBackend,
        window: WindowId,
        hidden_x: i32,
        restore: Rect,
    ) -> ClientKey {
        let monitor = jwm.state.monitor_order[0];
        let mut client = WMClient::new(window);
        client.mon = Some(monitor);
        client.state.tags = 1 << 5;
        client.geometry.x = hidden_x;
        client.geometry.y = restore.y;
        client.geometry.w = restore.w;
        client.geometry.h = restore.h;
        client.geometry.hidden_x = Some(hidden_x);
        client.geometry.hidden_restore_rect = Some(restore);
        let client_key = jwm.state.clients.insert(client);
        jwm.state.client_order.push(client_key);
        jwm.state.win_to_client.insert(window, client_key);
        backend.window_ops.insert(
            window,
            Geometry {
                x: hidden_x,
                y: restore.y,
                w: restore.w.max(1) as u32,
                h: restore.h.max(1) as u32,
                border: 0,
            },
            true,
        );
        client_key
    }

    fn two_iconic_clients() -> (
        Jwm,
        ExitBackend,
        (WindowId, ClientKey, i32, i32),
        (WindowId, ClientKey, i32, i32),
    ) {
        let mut backend = ExitBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test-x11").unwrap();
        let first = WindowId::from_raw(0x9101);
        let second = WindowId::from_raw(0x9102);
        let first_hidden = -2400;
        let second_hidden = -2500;
        let first_visible = 120;
        let second_visible = 820;
        let first_key = add_iconic_client(&mut jwm, &backend, first, first_hidden, first_visible);
        let second_key =
            add_iconic_client(&mut jwm, &backend, second, second_hidden, second_visible);
        (
            jwm,
            backend,
            (first, first_key, first_hidden, first_visible),
            (second, second_key, second_hidden, second_visible),
        )
    }

    #[test]
    fn second_client_phase_a_failure_rolls_back_every_client_and_blocks_teardown() {
        let (mut jwm, mut backend, first, second) = two_iconic_clients();
        backend
            .window_ops
            .fail_visible_configure_once_for
            .store(second.0.raw(), AtomicOrdering::SeqCst);

        let error = jwm
            .cleanup(&mut backend)
            .expect_err("the global handoff must fail closed");
        assert!(
            error
                .to_string()
                .contains("injected visible geometry failure")
        );
        assert!(
            error
                .to_string()
                .contains("injected rollback geometry failure")
        );

        for (window, client_key, _, _) in [first, second] {
            let client = &jwm.state.clients[client_key];
            assert!(client.state.is_hidden);
            assert!(client.geometry.hidden_restore_rect.is_some());
            let server = backend.window_ops.snapshot(window);
            assert_eq!(server.geometry.x, client.geometry.x);
            assert!(x11_geometry_fully_left_of_desktop(
                server.geometry,
                jwm.desktop_left_edge()
            ));
            assert!(!server.viewable, "rollback must restore true Iconic state");
        }

        let operations = backend
            .operations
            .lock()
            .expect("exit operations lock")
            .clone();
        assert!(
            !operations.iter().any(|operation| matches!(
                operation,
                ExitOperation::ReleaseOwnership(_) | ExitOperation::BackendCleanup
            )),
            "Phase A failure must not cross into event/grab/border or backend teardown: {operations:?}"
        );
        let reiconified = operations
            .iter()
            .filter_map(|operation| match operation {
                ExitOperation::RequestIconic(window) => Some(*window),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(reiconified, vec![second.0, first.0]);
    }

    #[test]
    fn successful_phase_a_is_a_global_barrier_before_ownership_release() {
        let (mut jwm, mut backend, first, second) = two_iconic_clients();

        jwm.cleanup(&mut backend).unwrap();

        let operations = backend
            .operations
            .lock()
            .expect("exit operations lock")
            .clone();
        let first_release = operations
            .iter()
            .position(|operation| matches!(operation, ExitOperation::ReleaseOwnership(_)))
            .expect("Phase B must release client ownership");
        for (window, _, _, visible_x) in [first, second] {
            let handoff = operations
                .iter()
                .position(|operation| *operation == ExitOperation::Configure(window, visible_x))
                .expect("each client must reach its visible geometry");
            assert!(
                handoff < first_release,
                "every visible handoff must precede the first destructive release: {operations:?}"
            );
            let server = backend.window_ops.snapshot(window);
            assert_eq!(server.geometry.x, visible_x);
            assert!(server.viewable);
        }
        assert!(operations.contains(&ExitOperation::BackendCleanup));
        assert!(!jwm.state.clients[first.1].state.is_hidden);
        assert!(!jwm.state.clients[second.1].state.is_hidden);
    }

    #[test]
    fn stale_restore_rects_right_above_and_below_outputs_are_clamped_before_proof() {
        for case in ["right", "above", "below"] {
            let mut backend = ExitBackend::new();
            let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test-x11").unwrap();
            let monitor = jwm.state.monitor_order[0];
            let monitor_geometry = &jwm.state.monitors[monitor].geometry;
            let output = Rect::new(
                monitor_geometry.m_x,
                monitor_geometry.m_y,
                monitor_geometry.m_w,
                monitor_geometry.m_h,
            );
            let stale = match case {
                "right" => Rect::new(
                    output.x.saturating_add(output.w).saturating_add(700),
                    output.y.saturating_add(100),
                    600,
                    420,
                ),
                "above" => Rect::new(
                    output.x.saturating_add(100),
                    output.y.saturating_sub(1120),
                    600,
                    420,
                ),
                "below" => Rect::new(
                    output.x.saturating_add(100),
                    output.y.saturating_add(output.h).saturating_add(700),
                    600,
                    420,
                ),
                _ => unreachable!(),
            };
            assert!(!geometry_intersects_rect(
                Geometry {
                    x: stale.x,
                    y: stale.y,
                    w: stale.w as u32,
                    h: stale.h as u32,
                    border: 0,
                },
                output,
            ));
            let window = WindowId::from_raw(match case {
                "right" => 0x9251,
                "above" => 0x9252,
                "below" => 0x9253,
                _ => unreachable!(),
            });
            let hidden_x = output.x.saturating_sub(2400);
            let client_key = add_parked_offtag_client(&mut jwm, &backend, window, hidden_x, stale);

            let _handoff = jwm
                .prepare_normal_exit_handoff(&mut backend)
                .unwrap_or_else(|error| panic!("{case} stale restore must be repaired: {error}"));

            let actual = backend.window_ops.snapshot(window).geometry;
            assert!(
                jwm.normal_exit_geometry_intersects_output(actual),
                "{case} target must intersect a live output: {actual:?}"
            );
            assert_ne!((actual.x, actual.y), (stale.x, stale.y));
            assert_eq!(jwm.state.clients[client_key].geometry.x, actual.x);
            assert_eq!(jwm.state.clients[client_key].geometry.y, actual.y);
        }
    }

    #[test]
    fn swallowed_offtag_parent_gets_visible_geometry_before_the_final_map_batch() {
        let mut backend = ExitBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test-x11").unwrap();
        let window = WindowId::from_raw(0x9201);
        let hidden_x = -2600;
        let visible_x = 360;
        let client_key =
            add_swallowed_offtag_client(&mut jwm, &backend, window, hidden_x, visible_x);

        let handoff = jwm
            .prepare_normal_exit_handoff(&mut backend)
            .expect("swallowed parent handoff must succeed");

        let server = backend.window_ops.snapshot(window);
        assert!(server.viewable);
        assert_eq!(server.geometry.x, visible_x);
        assert!(!jwm.state.clients[client_key].state.is_swallowed);
        assert_eq!(jwm.state.clients[client_key].geometry.x, visible_x);
        assert!(
            jwm.state.clients[client_key]
                .geometry
                .hidden_restore_rect
                .is_none()
        );

        let operations = backend
            .operations
            .lock()
            .expect("exit operations lock")
            .clone();
        let configured = operations
            .iter()
            .position(|operation| *operation == ExitOperation::Configure(window, visible_x))
            .expect("parent visible geometry configure");
        let mapped = operations
            .iter()
            .position(|operation| *operation == ExitOperation::Map(window))
            .expect("final swallowed-parent map");
        assert!(
            configured < mapped,
            "an unmapped swallowed parent must be moved before it is exposed: {operations:?}"
        );

        jwm.cleanup_after_normal_exit_handoff(&mut backend, handoff)
            .unwrap();
        assert!(backend.window_ops.snapshot(window).viewable);
    }

    #[test]
    fn swallowed_batch_query_and_first_unmap_failure_are_retried_to_resume_safe() {
        let mut backend = ExitBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test-x11").unwrap();
        let window = WindowId::from_raw(0x9202);
        let hidden_x = -2700;
        let client_key = add_swallowed_offtag_client(&mut jwm, &backend, window, hidden_x, 420);
        backend
            .window_ops
            .fail_attributes_for
            .store(window.raw(), AtomicOrdering::SeqCst);
        backend
            .window_ops
            .fail_attributes_on_call
            .store(2, AtomicOrdering::SeqCst);
        backend
            .window_ops
            .fail_unmap_once_for
            .store(window.raw(), AtomicOrdering::SeqCst);

        let error = match jwm.prepare_normal_exit_handoff(&mut backend) {
            Ok(_) => panic!("the swallowed batch query was injected to fail"),
            Err(error) => error,
        };

        assert!(error.resume_safe(), "{error}");
        assert!(
            error
                .to_string()
                .contains("injected attributes query failure")
        );
        assert!(error.to_string().contains("injected unmap failure"));
        let client = &jwm.state.clients[client_key];
        assert!(client.state.is_swallowed);
        assert_eq!(client.geometry.x, hidden_x);
        assert!(!backend.window_ops.snapshot(window).viewable);
        let unmaps = backend
            .operations
            .lock()
            .expect("exit operations lock")
            .iter()
            .filter(|operation| **operation == ExitOperation::UnmapSwallowed(window))
            .count();
        assert_eq!(
            unmaps, 2,
            "outer rollback must retry the swallowed helper's failed unmap"
        );
    }

    #[test]
    fn successful_swallowed_rollback_is_not_reunmapped_without_an_event() {
        let mut backend = ExitBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test-x11").unwrap();
        let window = WindowId::from_raw(0x9204);
        add_swallowed_offtag_client(&mut jwm, &backend, window, -2850, 540);
        backend
            .window_ops
            .fail_attributes_for
            .store(window.raw(), AtomicOrdering::SeqCst);
        backend
            .window_ops
            .fail_attributes_on_call
            .store(2, AtomicOrdering::SeqCst);

        let error = match jwm.prepare_normal_exit_handoff(&mut backend) {
            Ok(_) => panic!("the swallowed batch query was injected to fail"),
            Err(error) => error,
        };

        assert!(error.resume_safe(), "{error}");
        assert!(!backend.window_ops.snapshot(window).viewable);
        let unmaps = backend
            .operations
            .lock()
            .expect("exit operations lock")
            .iter()
            .filter(|operation| **operation == ExitOperation::UnmapSwallowed(window))
            .count();
        assert_eq!(
            unmaps, 1,
            "an already-unmapped parent has no second UnmapNotify to consume, so outer rollback must not issue a duplicate managed unmap"
        );
    }

    #[test]
    fn rollback_attributes_failure_is_typed_as_unsafe_to_resume() {
        let mut backend = ExitBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test-x11").unwrap();
        let window = WindowId::from_raw(0x9203);
        add_swallowed_offtag_client(&mut jwm, &backend, window, -2800, 500);
        backend
            .window_ops
            .fail_map_once_for
            .store(window.raw(), AtomicOrdering::SeqCst);
        backend
            .window_ops
            .fail_attributes_for
            .store(window.raw(), AtomicOrdering::SeqCst);
        backend
            .window_ops
            .fail_attributes_on_call
            .store(3, AtomicOrdering::SeqCst);

        let error = match jwm.prepare_normal_exit_handoff(&mut backend) {
            Ok(_) => panic!("the swallowed map was injected to fail"),
            Err(error) => error,
        };

        assert!(!error.resume_safe(), "{error}");
        assert!(
            error
                .to_string()
                .contains("query map state after rolling back")
        );
        assert!(!backend.window_ops.snapshot(window).viewable);
    }
}
