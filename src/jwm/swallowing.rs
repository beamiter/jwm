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
use crate::backend::api::Backend;
use crate::config::CONFIG;
use crate::jwm::ClientKey;

impl Jwm {
    /// Try to swallow an ancestor terminal. Called from `manage_regular_client`
    /// after rules and class info have been applied.
    pub(crate) fn try_swallow(&mut self, backend: &mut dyn Backend, child_key: ClientKey) {
        let cfg = CONFIG.load();
        let beh = cfg.behavior();
        if !beh.swallow_enabled || beh.swallow_terminals.is_empty() {
            return;
        }

        let (child_class, child_instance, child_pid) = match self.state.clients.get(child_key) {
            Some(c) => (c.class.clone(), c.instance.clone(), c.pid),
            None => return,
        };

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
            if c.state.is_swallowed || k == child_key {
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

        // Mark relationships, hide the parent.
        if let Some(parent) = self.state.clients.get_mut(parent_key) {
            parent.state.is_swallowed = true;
        }
        if let Some(child) = self.state.clients.get_mut(child_key) {
            child.swallowing = Some(parent_key);
        }
        let parent_win = self.state.clients.get(parent_key).map(|c| c.win);
        if let Some(win) = parent_win {
            if let Err(e) = backend.window_ops().unmap_window(win) {
                log::warn!("[swallow] failed to unmap parent window: {e:?}");
            }
        }
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

        if let Some(parent) = self.state.clients.get_mut(parent_key) {
            parent.state.is_swallowed = false;
        }
        let parent_win = self.state.clients.get(parent_key).map(|c| c.win);
        if let Some(win) = parent_win {
            if let Err(e) = backend.window_ops().map_window(win) {
                log::warn!("[swallow] failed to remap parent window: {e:?}");
            }
        }
    }
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
