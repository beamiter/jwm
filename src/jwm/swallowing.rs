// Window swallowing: hide a terminal when it spawns a graphical child.
//
// Mechanism: each managed window stores its PID (via _NET_WM_PID on X11). When
// a new window is mapped, walk up its `/proc/<pid>/status` parent chain. If
// any ancestor PID matches a currently-managed window whose class is in the
// `swallow_terminals` allowlist, that ancestor is "swallowed" — unmapped and
// hidden from arrange/visibility queries until the swallowing child unmaps.
//
// Wayland backends return `None` from `get_window_pid` so swallowing simply
// never activates there.

use crate::Jwm;
use crate::backend::api::{Backend, ManagedUnmapReason, WindowOps};
use crate::backend::error::BackendError;
use crate::config::CONFIG;
use crate::core::models::WMClient;
use crate::jwm::ClientKey;
use crate::jwm::statusbar::StatusBarBuilder;
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SwallowedParentHandoff {
    parent_key: ClientKey,
    window: crate::backend::common_define::WindowId,
}

/// Derive the exact set of hidden parents from JWM's managed-client state.
///
/// This intentionally never scans the server's unmapped windows. A live
/// `child.swallowing` edge is authoritative, while `is_swallowed` also covers
/// an orphaned parent whose child disappeared after an earlier remap error.
fn swallowed_parent_handoff_plan(
    client_order: &[ClientKey],
    clients: &slotmap::SlotMap<ClientKey, WMClient>,
) -> Vec<SwallowedParentHandoff> {
    let mut seen = HashSet::new();
    let mut plan = Vec::new();

    for &child_key in client_order {
        let Some(parent_key) = clients.get(child_key).and_then(|child| child.swallowing) else {
            continue;
        };
        let Some(parent) = clients.get(parent_key) else {
            continue;
        };
        if seen.insert(parent_key) {
            plan.push(SwallowedParentHandoff {
                parent_key,
                window: parent.win,
            });
        }
    }

    for &parent_key in client_order {
        let Some(parent) = clients.get(parent_key) else {
            continue;
        };
        if parent.state.is_swallowed && seen.insert(parent_key) {
            plan.push(SwallowedParentHandoff {
                parent_key,
                window: parent.win,
            });
        }
    }

    plan
}

fn rollback_swallowed_parent_maps(
    window_ops: &dyn WindowOps,
    attempted: &[SwallowedParentHandoff],
) -> Vec<String> {
    let mut failures = Vec::new();
    for target in attempted.iter().rev() {
        if let Err(error) =
            window_ops.unmap_managed_window(target.window, ManagedUnmapReason::SwallowDiscard)
        {
            // A failed rollback can expose a parent, but cannot lose it. Keep
            // the in-memory relationship so the resumed JWM can retry the
            // handoff on the next quit/restart request.
            log::warn!(
                "[swallow] failed to roll back parent {:?} after handoff failure: {error}",
                target.window
            );
            failures.push(format!(
                "unmap swallowed parent {:?}: {error}",
                target.window
            ));
        }
    }
    if let Err(error) = window_ops.flush() {
        log::warn!("[swallow] failed to flush handoff rollback: {error}");
        failures.push(format!("flush swallowed-parent rollback: {error}"));
    }
    failures
}

fn rollback_failure_suffix(failures: &[String]) -> String {
    if failures.is_empty() {
        String::new()
    } else {
        format!("; rollback failures: {}", failures.join("; "))
    }
}

/// Transactionally make every managed swallowed parent discoverable by the
/// next WM instance (or by the desktop after a normal WM exit).
///
/// State is committed only after every bounded target was mapped, flushed,
/// and observed as viewable. On failure, attempted maps are rolled back and
/// all swallow edges/flags remain intact, so the caller can safely cancel the
/// exit and resume the current event loop.
fn map_swallowed_parents_for_handoff(
    client_order: &[ClientKey],
    clients: &mut slotmap::SlotMap<ClientKey, WMClient>,
    window_ops: &dyn WindowOps,
) -> Result<Vec<ClientKey>, BackendError> {
    let plan = swallowed_parent_handoff_plan(client_order, clients);
    if plan.is_empty() {
        // Only stale links to already-removed parents can remain here. They
        // require no display-server operation and must not make an unrelated
        // shutdown depend on a redundant flush.
        for client in clients.values_mut() {
            client.swallowing = None;
        }
        return Ok(Vec::new());
    }
    let mut attempted = Vec::with_capacity(plan.len());

    for target in &plan {
        if let Err(error) = window_ops.map_window(target.window) {
            let rollback_failures = rollback_swallowed_parent_maps(window_ops, &attempted);
            return Err(BackendError::Message(format!(
                "failed to map swallowed parent {:?}: {error}{}",
                target.window,
                rollback_failure_suffix(&rollback_failures)
            )));
        }
        attempted.push(*target);
    }

    if let Err(error) = window_ops.flush() {
        let rollback_failures = rollback_swallowed_parent_maps(window_ops, &attempted);
        return Err(BackendError::Message(format!(
            "failed to flush swallowed-parent handoff: {error}{}",
            rollback_failure_suffix(&rollback_failures)
        )));
    }

    for target in &plan {
        match window_ops.get_window_attributes(target.window) {
            Ok(attributes) if attributes.map_state_viewable => {}
            Ok(_) => {
                let rollback_failures = rollback_swallowed_parent_maps(window_ops, &attempted);
                return Err(BackendError::Message(format!(
                    "swallowed parent {:?} was not viewable after mapping{}",
                    target.window,
                    rollback_failure_suffix(&rollback_failures)
                )));
            }
            Err(error) => {
                let rollback_failures = rollback_swallowed_parent_maps(window_ops, &attempted);
                return Err(BackendError::Message(format!(
                    "failed to confirm swallowed parent {:?} after mapping: {error}{}",
                    target.window,
                    rollback_failure_suffix(&rollback_failures)
                )));
            }
        }
    }

    // Commit only after every X11 request above has been confirmed. Clear all
    // links, including a stale link to a parent that has already disappeared.
    for client in clients.values_mut() {
        client.swallowing = None;
    }
    for target in &plan {
        if let Some(parent) = clients.get_mut(target.parent_key) {
            parent.state.is_swallowed = false;
        }
    }

    Ok(plan.into_iter().map(|target| target.parent_key).collect())
}

impl Jwm {
    /// Prepare swallowed windows before either seamless restart or normal
    /// shutdown. This is a bounded handoff over the managed-client registry;
    /// it never adopts arbitrary unmapped server windows.
    pub(crate) fn prepare_swallowed_parents_for_handoff(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), BackendError> {
        let plan = swallowed_parent_handoff_plan(&self.state.client_order, &self.state.clients);
        let previous_dock_eligibility: Vec<_> = plan
            .iter()
            .map(|target| {
                (
                    target.parent_key,
                    self.state
                        .clients
                        .get(target.parent_key)
                        .is_some_and(StatusBarBuilder::is_minimized_dock_eligible),
                )
            })
            .collect();

        let restored = map_swallowed_parents_for_handoff(
            &self.state.client_order,
            &mut self.state.clients,
            backend.window_ops(),
        )?;

        for parent_key in restored {
            let was_eligible = previous_dock_eligibility
                .iter()
                .find_map(|(key, eligible)| (*key == parent_key).then_some(*eligible))
                .unwrap_or(false);
            self.reconcile_minimized_dock_eligibility(backend, parent_key, was_eligible);
        }
        Ok(())
    }

    /// Try to swallow an ancestor terminal. Called from `manage_regular_client`
    /// after rules and class info have been applied.
    pub(crate) fn try_swallow(&mut self, backend: &mut dyn Backend, child_key: ClientKey) {
        let cfg = CONFIG.load();
        let beh = cfg.behavior();
        if !beh.swallow_enabled || beh.swallow_terminals.is_empty() {
            return;
        }

        let (child_class, child_instance, child_pid, child_is_hidden) =
            match self.state.clients.get(child_key) {
                Some(c) => (
                    c.class.clone(),
                    c.instance.clone(),
                    c.pid,
                    c.state.is_hidden,
                ),
                None => return,
            };

        // A child adopted in ICCCM IconicState is already intentionally absent
        // from the desktop. Swallowing its terminal as well would leave no
        // visible representative until the user discovers the child in the
        // Dock. Defer swallowing to a future, newly managed visible child;
        // restoring this existing one must not retroactively unmap its parent.
        if !can_trigger_swallow(child_is_hidden) {
            return;
        }

        // Don't let popups / launchers swallow.
        if matches_any(&beh.swallow_exceptions, &child_class, &child_instance) {
            return;
        }

        let child_pid = match child_pid {
            Some(p) => p,
            None => return,
        };

        // Walk parent process chain looking for a managed terminal.
        let ancestors = walk_ppids(child_pid, 16);
        if ancestors.is_empty() {
            return;
        }

        let parent_key = self.state.client_order.iter().copied().find(|&k| {
            let c = match self.state.clients.get(k) {
                Some(c) => c,
                None => return false,
            };
            // A minimized terminal already belongs to the Iconic lifecycle.
            // Unmapping it again can be a server no-op, and unswallow must
            // never map it behind the user's back.
            if !can_enter_swallowed_state(c.state.is_swallowed, c.state.is_hidden) || k == child_key
            {
                return false;
            }
            let pid = match c.pid {
                Some(p) => p,
                None => return false,
            };
            if !ancestors.contains(&pid) {
                return false;
            }
            matches_any(&beh.swallow_terminals, &c.class, &c.instance)
        });

        let parent_key = match parent_key {
            Some(k) => k,
            None => return,
        };

        let was_dock_eligible = self
            .state
            .clients
            .get(parent_key)
            .is_some_and(StatusBarBuilder::is_minimized_dock_eligible);

        // Mark relationships before issuing UnmapWindow. The X11 transport
        // records the exact request sequence so neither the root/client event
        // duplicate nor a client withdrawal can remove this managed parent.
        if let Some(parent) = self.state.clients.get_mut(parent_key) {
            parent.state.is_swallowed = true;
        }
        if let Some(child) = self.state.clients.get_mut(child_key) {
            child.swallowing = Some(parent_key);
        }
        let parent_win = self.state.clients.get(parent_key).map(|c| c.win);
        if let Some(win) = parent_win {
            if let Err(e) = backend
                .window_ops()
                .unmap_managed_window(win, ManagedUnmapReason::SwallowDiscard)
            {
                log::warn!("[swallow] failed to unmap parent window: {e:?}");
                if let Some(parent) = self.state.clients.get_mut(parent_key) {
                    parent.state.is_swallowed = false;
                }
                if let Some(child) = self.state.clients.get_mut(child_key) {
                    child.swallowing = None;
                }
                return;
            }
        }
        self.reconcile_minimized_dock_eligibility(backend, parent_key, was_dock_eligible);
        log::info!(
            "[swallow] '{}' swallowed by '{}'",
            self.state
                .clients
                .get(parent_key)
                .map(|c| c.class.as_str())
                .unwrap_or(""),
            child_class
        );
    }

    /// Restore a swallowed parent when its swallowing child unmaps. Called
    /// from `unmanage_regular_client`.
    pub(crate) fn try_unswallow(&mut self, backend: &mut dyn Backend, child_key: ClientKey) {
        let parent_key = match self.state.clients.get(child_key).and_then(|c| c.swallowing) {
            Some(k) => k,
            None => return,
        };

        let was_dock_eligible = self
            .state
            .clients
            .get(parent_key)
            .is_some_and(StatusBarBuilder::is_minimized_dock_eligible);
        if let Some(parent) = self.state.clients.get_mut(parent_key) {
            parent.state.is_swallowed = false;
        }
        let parent_to_map = self.state.clients.get(parent_key).and_then(|parent| {
            should_map_after_unswallow(parent.state.is_hidden).then_some(parent.win)
        });
        if let Some(win) = parent_to_map {
            if let Err(e) = backend.window_ops().map_window(win) {
                log::warn!("[swallow] failed to remap parent window: {e:?}");
            }
        }
        self.reconcile_minimized_dock_eligibility(backend, parent_key, was_dock_eligible);
    }
}

fn can_enter_swallowed_state(is_swallowed: bool, is_hidden: bool) -> bool {
    !is_swallowed && !is_hidden
}

fn can_trigger_swallow(child_is_hidden: bool) -> bool {
    !child_is_hidden
}

fn should_map_after_unswallow(is_hidden: bool) -> bool {
    !is_hidden
}

fn matches_any(patterns: &[String], class: &str, instance: &str) -> bool {
    patterns.iter().any(|p| {
        let p = p.as_str();
        p.eq_ignore_ascii_case(class) || p.eq_ignore_ascii_case(instance)
    })
}

/// Walk up the process tree from `pid`, returning ancestor PIDs (not including
/// `pid` itself). Stops at PID 1, on parse failure, or after `max_depth` steps.
fn walk_ppids(pid: u32, max_depth: usize) -> Vec<u32> {
    let mut out = Vec::with_capacity(max_depth);
    let mut cur = pid;
    for _ in 0..max_depth {
        match read_ppid(cur) {
            Some(ppid) if ppid > 1 && ppid != cur => {
                out.push(ppid);
                cur = ppid;
            }
            _ => break,
        }
    }
    out
}

fn read_ppid(pid: u32) -> Option<u32> {
    // /proc/<pid>/status has a "PPid:\t<num>" line; less fragile than parsing
    // /proc/<pid>/stat (whose comm field can contain spaces and parens).
    let path = format!("/proc/{pid}/status");
    let contents = std::fs::read_to_string(&path).ok()?;
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("PPid:") {
            return rest.trim().parse::<u32>().ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{
        can_enter_swallowed_state, can_trigger_swallow, map_swallowed_parents_for_handoff,
        should_map_after_unswallow,
    };
    use crate::backend::api::{CloseResult, Geometry, WindowAttributes, WindowChanges, WindowOps};
    use crate::backend::common_define::{Pixel, WindowId};
    use crate::backend::error::BackendError;
    use crate::core::models::WMClient;
    use std::collections::HashSet;
    use std::sync::Mutex;

    struct HandoffWindowOps {
        fail_map: Option<WindowId>,
        never_viewable: Option<WindowId>,
        map_attempts: Mutex<Vec<WindowId>>,
        unmap_attempts: Mutex<Vec<WindowId>>,
        viewable: Mutex<HashSet<WindowId>>,
    }

    impl HandoffWindowOps {
        fn new(fail_map: Option<WindowId>, never_viewable: Option<WindowId>) -> Self {
            Self {
                fail_map,
                never_viewable,
                map_attempts: Mutex::new(Vec::new()),
                unmap_attempts: Mutex::new(Vec::new()),
                viewable: Mutex::new(HashSet::new()),
            }
        }

        fn map_attempts(&self) -> Vec<WindowId> {
            self.map_attempts.lock().unwrap().clone()
        }

        fn unmap_attempts(&self) -> Vec<WindowId> {
            self.unmap_attempts.lock().unwrap().clone()
        }
    }

    impl WindowOps for HandoffWindowOps {
        fn set_position(&self, _win: WindowId, _x: i32, _y: i32) -> Result<(), BackendError> {
            Ok(())
        }

        fn configure(
            &self,
            _win: WindowId,
            _x: i32,
            _y: i32,
            _w: u32,
            _h: u32,
            _border: u32,
        ) -> Result<(), BackendError> {
            Ok(())
        }

        fn set_decoration_style(
            &self,
            _win: WindowId,
            _border_width: u32,
            _border_color: Pixel,
        ) -> Result<(), BackendError> {
            Ok(())
        }

        fn raise_window(&self, _win: WindowId) -> Result<(), BackendError> {
            Ok(())
        }

        fn map_window(&self, win: WindowId) -> Result<(), BackendError> {
            self.map_attempts.lock().unwrap().push(win);
            if self.fail_map == Some(win) {
                return Err(BackendError::Message("injected map failure".into()));
            }
            self.viewable.lock().unwrap().insert(win);
            Ok(())
        }

        fn unmap_window(&self, win: WindowId) -> Result<(), BackendError> {
            self.unmap_attempts.lock().unwrap().push(win);
            self.viewable.lock().unwrap().remove(&win);
            Ok(())
        }

        fn close_window(&self, _win: WindowId) -> Result<CloseResult, BackendError> {
            Ok(CloseResult::Graceful)
        }

        fn set_input_focus(&self, _win: WindowId) -> Result<(), BackendError> {
            Ok(())
        }

        fn set_input_focus_root(&self) -> Result<(), BackendError> {
            Ok(())
        }

        fn get_window_attributes(&self, win: WindowId) -> Result<WindowAttributes, BackendError> {
            let viewable =
                self.never_viewable != Some(win) && self.viewable.lock().unwrap().contains(&win);
            Ok(WindowAttributes {
                override_redirect: false,
                map_state_viewable: viewable,
            })
        }

        fn get_geometry(&self, _win: WindowId) -> Result<Geometry, BackendError> {
            Ok(Geometry::default())
        }

        fn scan_windows(&self) -> Result<Vec<WindowId>, BackendError> {
            panic!("swallowed-parent handoff must not scan server windows")
        }

        fn flush(&self) -> Result<(), BackendError> {
            Ok(())
        }

        fn kill_client(&self, _win: WindowId) -> Result<(), BackendError> {
            Ok(())
        }

        fn apply_window_changes(
            &self,
            _win: WindowId,
            _changes: WindowChanges,
        ) -> Result<(), BackendError> {
            Ok(())
        }
    }

    fn client(window: u64) -> WMClient {
        WMClient::new(WindowId::from_raw(window))
    }

    #[test]
    fn minimized_parent_cannot_enter_the_swallow_unmap_lifecycle() {
        assert!(!can_enter_swallowed_state(false, true));
        assert!(can_enter_swallowed_state(false, false));
    }

    #[test]
    fn initially_minimized_child_cannot_hide_its_visible_terminal() {
        assert!(!can_trigger_swallow(true));
        assert!(can_trigger_swallow(false));
    }

    #[test]
    fn parent_minimized_while_swallowed_is_not_remapped_on_unswallow() {
        assert!(!should_map_after_unswallow(true));
        assert!(should_map_after_unswallow(false));
    }

    #[test]
    fn handoff_maps_only_managed_swallow_targets_and_commits_after_confirmation() {
        let parent_window = WindowId::from_raw(0x51);
        let orphan_window = WindowId::from_raw(0x52);
        let unrelated_window = WindowId::from_raw(0x53);
        let mut clients = slotmap::SlotMap::with_key();
        let mut parent = client(parent_window.raw());
        parent.state.is_swallowed = true;
        let parent_key = clients.insert(parent);
        let mut child = client(0x61);
        child.swallowing = Some(parent_key);
        let child_key = clients.insert(child);
        let mut orphan = client(orphan_window.raw());
        orphan.state.is_swallowed = true;
        let orphan_key = clients.insert(orphan);
        let unrelated_key = clients.insert(client(unrelated_window.raw()));
        let order = vec![parent_key, child_key, orphan_key, unrelated_key];
        let window_ops = HandoffWindowOps::new(None, None);

        let restored =
            map_swallowed_parents_for_handoff(&order, &mut clients, &window_ops).unwrap();

        assert_eq!(restored, vec![parent_key, orphan_key]);
        assert_eq!(
            window_ops.map_attempts(),
            vec![parent_window, orphan_window],
            "an unrelated managed window must not be mapped by the handoff"
        );
        assert!(window_ops.unmap_attempts().is_empty());
        assert!(!clients[parent_key].state.is_swallowed);
        assert!(!clients[orphan_key].state.is_swallowed);
        assert_eq!(clients[child_key].swallowing, None);
        assert!(!clients[unrelated_key].state.is_swallowed);
    }

    #[test]
    fn map_failure_rolls_back_requests_without_committing_swallow_state() {
        let first_window = WindowId::from_raw(0x71);
        let failed_window = WindowId::from_raw(0x72);
        let mut clients = slotmap::SlotMap::with_key();
        let mut first_parent = client(first_window.raw());
        first_parent.state.is_swallowed = true;
        let first_parent_key = clients.insert(first_parent);
        let mut first_child = client(0x81);
        first_child.swallowing = Some(first_parent_key);
        let first_child_key = clients.insert(first_child);
        let mut failed_parent = client(failed_window.raw());
        failed_parent.state.is_swallowed = true;
        let failed_parent_key = clients.insert(failed_parent);
        let order = vec![first_child_key, first_parent_key, failed_parent_key];
        let window_ops = HandoffWindowOps::new(Some(failed_window), None);

        let error = map_swallowed_parents_for_handoff(&order, &mut clients, &window_ops)
            .unwrap_err()
            .to_string();

        assert!(error.contains("failed to map swallowed parent"));
        assert_eq!(window_ops.map_attempts(), vec![first_window, failed_window]);
        assert_eq!(window_ops.unmap_attempts(), vec![first_window]);
        assert!(clients[first_parent_key].state.is_swallowed);
        assert!(clients[failed_parent_key].state.is_swallowed);
        assert_eq!(clients[first_child_key].swallowing, Some(first_parent_key));
        assert!(window_ops.viewable.lock().unwrap().is_empty());
    }

    #[test]
    fn unconfirmed_map_rolls_back_without_committing_swallow_state() {
        let parent_window = WindowId::from_raw(0x91);
        let mut clients = slotmap::SlotMap::with_key();
        let mut parent = client(parent_window.raw());
        parent.state.is_swallowed = true;
        let parent_key = clients.insert(parent);
        let order = vec![parent_key];
        let window_ops = HandoffWindowOps::new(None, Some(parent_window));

        let error = map_swallowed_parents_for_handoff(&order, &mut clients, &window_ops)
            .unwrap_err()
            .to_string();

        assert!(error.contains("was not viewable after mapping"));
        assert!(clients[parent_key].state.is_swallowed);
        assert_eq!(window_ops.unmap_attempts(), vec![parent_window]);
        assert!(window_ops.viewable.lock().unwrap().is_empty());
    }
}
