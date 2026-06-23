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
        info!("restore: token `{token}` matched but no stored sources still exist");
        return None;
    }
    Some((
        SourceSelection { outputs, toplevels },
        entry.persist_mode,
    ))
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
