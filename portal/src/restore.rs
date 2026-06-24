//! `restore_token` persistence for ScreenCast sessions.
//!
//! When a caller passes `persist_mode > 0` to `SelectSources`, the spec lets
//! the portal hand back a `restore_token` in the `Start` response so the same
//! caller can later pass it back to skip the picker UI. We keep the mapping
//! `token → selection` on disk at `~/.config/jwm-portal/sessions.json` so the
//! token survives portal restarts (OBS users in particular expect "use last
//! selection" to keep working across compositor reloads).
//!
//! Stored selection identifiers are *names* (wl_output name, foreign-toplevel
//! identifier). At restore time we resolve them against the current snapshot —
//! anything that no longer exists is silently dropped. If nothing resolves, we
//! fall back to the regular picker path.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use log::{info, warn};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};

use crate::picker::SourceSelection;
use crate::wayland::{OutputInfo, ToplevelInfo};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct RestoredSelection {
    pub outputs: Vec<String>,
    pub toplevels: Vec<String>,
    pub persist_mode: u32,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    sessions: HashMap<String, RestoredSelection>,
}

static STORE: Lazy<Mutex<Store>> = Lazy::new(|| Mutex::new(load().unwrap_or_default()));

fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("jwm-portal").join("sessions.json")
}

fn load() -> Option<Store> {
    let path = config_path();
    let bytes = std::fs::read(&path).ok()?;
    match serde_json::from_slice::<Store>(&bytes) {
        Ok(s) => {
            info!("restore: loaded {} session(s) from {path:?}", s.sessions.len());
            Some(s)
        }
        Err(e) => {
            warn!("restore: parse {path:?} failed: {e}; ignoring");
            None
        }
    }
}

fn flush(store: &Store) {
    let path = config_path();
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            warn!("restore: mkdir {parent:?}: {e}");
            return;
        }
    }
    match serde_json::to_vec_pretty(store) {
        Ok(bytes) => {
            if let Err(e) = std::fs::write(&path, &bytes) {
                warn!("restore: write {path:?}: {e}");
            }
        }
        Err(e) => warn!("restore: serialize: {e}"),
    }
}

/// Pure resolution step: given a stored entry and the currently-available
/// sources, drop names that no longer exist and return the surviving selection.
/// Returns None when nothing in `entry` is still present — callers should treat
/// this the same as "token unknown" (fall back to the picker).
///
/// Split out so it can be unit-tested without touching the global STORE.
pub fn resolve_in(
    entry: &RestoredSelection,
    available_outputs: &[OutputInfo],
    available_toplevels: &[ToplevelInfo],
) -> Option<(SourceSelection, u32)> {
    let outputs: Vec<OutputInfo> = entry
        .outputs
        .iter()
        .filter_map(|name| available_outputs.iter().find(|o| &o.name == name).cloned())
        .collect();
    let toplevels: Vec<ToplevelInfo> = entry
        .toplevels
        .iter()
        .filter_map(|id| {
            available_toplevels
                .iter()
                .find(|t| &t.identifier == id)
                .cloned()
        })
        .collect();
    if outputs.is_empty() && toplevels.is_empty() {
        return None;
    }
    Some((
        SourceSelection { outputs, toplevels },
        entry.persist_mode,
    ))
}

/// Look up a token; resolve against the *current* available outputs / toplevels.
/// Returns None if the token is unknown or nothing in the stored selection
/// still exists.
pub fn resolve(
    token: &str,
    available_outputs: &[OutputInfo],
    available_toplevels: &[ToplevelInfo],
) -> Option<(SourceSelection, u32)> {
    let store = STORE.lock().ok()?;
    let entry = store.sessions.get(token)?.clone();
    drop(store);
    let resolved = resolve_in(&entry, available_outputs, available_toplevels);
    if resolved.is_none() {
        info!("restore: token `{token}` matched but no stored sources still exist");
    }
    resolved
}

/// Store the given selection under a freshly-generated token; returns the new
/// token so it can be handed back in the Start response.
pub fn save_new(selection: &SourceSelection, persist_mode: u32) -> String {
    let token = new_token();
    let entry = RestoredSelection {
        outputs: selection.outputs.iter().map(|o| o.name.clone()).collect(),
        toplevels: selection.toplevels.iter().map(|t| t.identifier.clone()).collect(),
        persist_mode,
    };
    let mut store = STORE.lock().expect("restore store mutex");
    store.sessions.insert(token.clone(), entry);
    flush(&store);
    info!("restore: saved selection under token `{token}` (persist_mode={persist_mode})");
    token
}

/// Re-save under an existing token (e.g. caller passed a still-valid token
/// and we just confirmed it; spec says to hand the same token back).
pub fn touch(token: &str, selection: &SourceSelection, persist_mode: u32) {
    let entry = RestoredSelection {
        outputs: selection.outputs.iter().map(|o| o.name.clone()).collect(),
        toplevels: selection.toplevels.iter().map(|t| t.identifier.clone()).collect(),
        persist_mode,
    };
    let mut store = STORE.lock().expect("restore store mutex");
    store.sessions.insert(token.to_string(), entry);
    flush(&store);
}

fn new_token() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 16];
    rand::rng().fill_bytes(&mut buf);
    let mut s = String::with_capacity(32);
    for b in buf {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(name: &str) -> OutputInfo {
        OutputInfo {
            name: name.into(),
            description: format!("desc-{name}"),
            width: 1920,
            height: 1080,
            refresh_mhz: 60_000,
        }
    }

    fn top(id: &str) -> ToplevelInfo {
        ToplevelInfo {
            identifier: id.into(),
            app_id: format!("app-{id}"),
            title: format!("title-{id}"),
        }
    }

    fn entry(outs: &[&str], tops: &[&str], persist_mode: u32) -> RestoredSelection {
        RestoredSelection {
            outputs: outs.iter().map(|s| s.to_string()).collect(),
            toplevels: tops.iter().map(|s| s.to_string()).collect(),
            persist_mode,
        }
    }

    #[test]
    fn all_sources_still_present_resolves_fully() {
        let e = entry(&["DP-1", "HDMI-A-1"], &["t-firefox"], 2);
        let outs = vec![out("DP-1"), out("HDMI-A-1"), out("eDP-1")];
        let tops = vec![top("t-firefox"), top("t-terminal")];
        let (sel, mode) = resolve_in(&e, &outs, &tops).expect("resolves");
        assert_eq!(sel.outputs.iter().map(|o| &o.name).collect::<Vec<_>>(),
                   vec!["DP-1", "HDMI-A-1"]);
        assert_eq!(sel.toplevels.iter().map(|t| &t.identifier).collect::<Vec<_>>(),
                   vec!["t-firefox"]);
        assert_eq!(mode, 2);
    }

    #[test]
    fn missing_outputs_are_silently_dropped_partial_match_still_ok() {
        let e = entry(&["DP-1", "GONE-1"], &[], 1);
        let outs = vec![out("DP-1")];
        let (sel, _) = resolve_in(&e, &outs, &[]).expect("partial still resolves");
        assert_eq!(sel.outputs.len(), 1);
        assert_eq!(sel.outputs[0].name, "DP-1");
        assert!(sel.toplevels.is_empty());
    }

    #[test]
    fn all_outputs_gone_and_no_toplevels_returns_none() {
        let e = entry(&["DP-1"], &[], 1);
        let outs = vec![out("HDMI-A-1")];
        assert!(resolve_in(&e, &outs, &[]).is_none());
    }

    #[test]
    fn all_sources_gone_returns_none_not_empty_selection() {
        // Caller depends on this distinction — None means "fall back to picker",
        // Some(empty) would mean "use empty selection" which is a bug.
        let e = entry(&["DP-1"], &["t-firefox"], 2);
        let outs = vec![out("eDP-1")];
        let tops = vec![top("t-other")];
        assert!(resolve_in(&e, &outs, &tops).is_none());
    }

    #[test]
    fn empty_stored_selection_returns_none() {
        let e = entry(&[], &[], 0);
        assert!(resolve_in(&e, &[out("DP-1")], &[top("t-x")]).is_none());
    }

    #[test]
    fn persist_mode_is_preserved_through_resolve() {
        for mode in [0u32, 1, 2] {
            let e = entry(&["DP-1"], &[], mode);
            let outs = vec![out("DP-1")];
            let (_, m) = resolve_in(&e, &outs, &[]).expect("resolves");
            assert_eq!(m, mode);
        }
    }

    #[test]
    fn toplevels_only_works_without_outputs() {
        let e = entry(&[], &["t-firefox"], 1);
        let tops = vec![top("t-firefox"), top("t-other")];
        let (sel, _) = resolve_in(&e, &[], &tops).expect("resolves toplevel-only");
        assert!(sel.outputs.is_empty());
        assert_eq!(sel.toplevels.len(), 1);
        assert_eq!(sel.toplevels[0].identifier, "t-firefox");
    }

    #[test]
    fn resolution_preserves_stored_order_not_available_order() {
        // The picker UI shows sources in the order the caller selected them.
        // resolve_in must walk stored names in stored order, not iterate the
        // available list — otherwise OBS users see their multi-monitor
        // selections re-ordered after a restore.
        let e = entry(&["B", "A"], &[], 0);
        let outs = vec![out("A"), out("B"), out("C")];
        let (sel, _) = resolve_in(&e, &outs, &[]).unwrap();
        assert_eq!(
            sel.outputs.iter().map(|o| &o.name).collect::<Vec<_>>(),
            vec!["B", "A"]
        );
    }
}
