//! Private restore-data persistence for backend ScreenCast sessions.
//!
//! When a caller passes `persist_mode > 0` to `SelectSources`, the spec lets
//! the backend hand back `restore_data` in the `Start` response. The portal
//! frontend owns public restore tokens and their app/lifetime binding. Our
//! `(suv)` payload wraps a private opaque key into the existing local store. We keep the mapping
//! `token → selection` on disk at `~/.config/jwm-portal/sessions.json` so the
//! token survives portal restarts (OBS users in particular expect "use last
//! selection" to keep working across compositor reloads).
//!
//! Stored selection identifiers are *names* (wl_output name, foreign-toplevel
//! identifier). At restore time we resolve them against the current snapshot —
//! anything that no longer exists is silently dropped. If nothing resolves, we
//! fall back to the regular picker path.
//!
//! The store keeps at most 64 selections (`MAX_SESSIONS`), evicting transient
//! (`persist_mode` 1) grants before permanent ones and the least recently
//! used first, and replaces its file atomically, so a full disk or a crash
//! mid-save keeps the previous contents.

use std::collections::HashMap;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use log::{info, warn};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};

use crate::picker::SourceSelection;
use crate::wayland::{OutputInfo, ToplevelInfo};

const RESTORE_VENDOR: &str = "JWM";
const RESTORE_VERSION: u32 = 1;

/// Saved selections kept at most. Every `Start` with a `persist_mode` and no
/// still-valid token saves a new one, and frontends drop tokens on their own
/// (a transient grant when its app exits, an app that never stores its
/// token), so without a cap the store, and each rewrite of its file, would
/// grow for as long as the account is used.
const MAX_SESSIONS: usize = 64;

/// `persist_mode` of a grant that outlives its application.
const PERSIST_PERMANENT: u32 = 2;

/// Backend wire format, distinct from the frontend's public restore_token.
pub fn encode_data(token: &str) -> Result<zvariant::OwnedValue, zvariant::Error> {
    zvariant::Value::from((
        RESTORE_VENDOR,
        RESTORE_VERSION,
        zvariant::Value::from(token),
    ))
    .try_into()
}

/// Reject other implementations/versions and every malformed private value.
pub fn decode_data(value: &zvariant::OwnedValue) -> Option<String> {
    let structure = <&zvariant::Structure<'_>>::try_from(value).ok()?;
    let fields = structure.fields();
    if fields.len() != 3 {
        return None;
    }
    let vendor = <&str>::try_from(&fields[0]).ok()?;
    let version = u32::try_from(&fields[1]).ok()?;
    let zvariant::Value::Value(private) = &fields[2] else {
        return None;
    };
    let token = <&str>::try_from(private.as_ref()).ok()?;
    (vendor == RESTORE_VENDOR && version == RESTORE_VERSION && !token.is_empty())
        .then(|| token.to_string())
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct RestoredSelection {
    pub outputs: Vec<String>,
    pub toplevels: Vec<String>,
    pub persist_mode: u32,
    /// Unix seconds of the last save or reuse; decides what the cap evicts.
    /// Files written before the cap existed read as 0, the oldest.
    #[serde(default)]
    pub last_used: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    sessions: HashMap<String, RestoredSelection>,
}

static STORE: Lazy<Mutex<Store>> = Lazy::new(|| Mutex::new(load().unwrap_or_default()));

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Drop saved selections beyond [`MAX_SESSIONS`]: transient grants first,
/// then the least recently used, so a permanent grant an application relies
/// on (OBS's "use last selection") outlasts a stream of one-off shares.
fn enforce_cap(store: &mut Store) {
    let excess = store.sessions.len().saturating_sub(MAX_SESSIONS);
    if excess == 0 {
        return;
    }
    let mut candidates: Vec<(bool, u64, String)> = store
        .sessions
        .iter()
        .map(|(token, entry)| {
            (
                entry.persist_mode >= PERSIST_PERMANENT,
                entry.last_used,
                token.clone(),
            )
        })
        .collect();
    candidates.sort_unstable();
    for (_, _, token) in candidates.into_iter().take(excess) {
        store.sessions.remove(&token);
    }
}

/// Save `entry` under `token` as used now, keeping the store within its cap.
fn insert_entry(store: &mut Store, token: String, mut entry: RestoredSelection) {
    entry.last_used = now_unix_secs();
    store.sessions.insert(token, entry);
    enforce_cap(store);
}

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
        Ok(mut s) => {
            info!(
                "restore: loaded {} session(s) from {path:?}",
                s.sessions.len()
            );
            enforce_cap(&mut s);
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
    if let Err(e) = write_store(&path, store) {
        warn!("restore: write {path:?}: {e}");
    }
}

/// Replace the store file in one step: write a sibling temporary file, then
/// rename it over the old one. Rewriting in place truncates first, so a full
/// disk or a crash mid-write left a file that no longer parses, and the
/// next start silently forgot every saved selection.
fn write_store(path: &Path, store: &Store) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(store).map_err(io::Error::other)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut temp_name = path.file_name().unwrap_or_default().to_os_string();
    temp_name.push(format!(".tmp-{}", std::process::id()));
    let temp = path.with_file_name(temp_name);
    let written = std::fs::File::create(&temp).and_then(|mut file| {
        file.write_all(&bytes)?;
        file.sync_all()
    });
    let result = written.and_then(|()| std::fs::rename(&temp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
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
        .filter_map(|key| {
            available_outputs
                .iter()
                .find(|o| &o.name == key || o.connector.as_deref() == Some(key.as_str()))
                .cloned()
        })
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
    Some((SourceSelection { outputs, toplevels }, entry.persist_mode))
}

fn output_restore_key(output: &OutputInfo) -> String {
    output
        .connector
        .as_deref()
        .filter(|c| !c.is_empty())
        .unwrap_or(output.name.as_str())
        .to_string()
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
        outputs: selection.outputs.iter().map(output_restore_key).collect(),
        toplevels: selection
            .toplevels
            .iter()
            .map(|t| t.identifier.clone())
            .collect(),
        persist_mode,
        last_used: 0,
    };
    let mut store = STORE.lock().expect("restore store mutex");
    insert_entry(&mut store, token.clone(), entry);
    flush(&store);
    info!("restore: saved selection under token `{token}` (persist_mode={persist_mode})");
    token
}

/// Re-save under an existing token (e.g. caller passed a still-valid token
/// and we just confirmed it; spec says to hand the same token back).
pub fn touch(token: &str, selection: &SourceSelection, persist_mode: u32) {
    let entry = RestoredSelection {
        outputs: selection.outputs.iter().map(output_restore_key).collect(),
        toplevels: selection
            .toplevels
            .iter()
            .map(|t| t.identifier.clone())
            .collect(),
        persist_mode,
        last_used: 0,
    };
    let mut store = STORE.lock().expect("restore store mutex");
    insert_entry(&mut store, token.to_string(), entry);
    flush(&store);
}

fn new_token() -> String {
    let buf: [u8; 16] = rand::random();
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
            connector: None,
        }
    }

    fn out_with_connector(name: &str, connector: &str) -> OutputInfo {
        OutputInfo {
            connector: Some(connector.into()),
            ..out(name)
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
            last_used: 0,
        }
    }

    fn used_at(persist_mode: u32, last_used: u64) -> RestoredSelection {
        RestoredSelection {
            last_used,
            ..entry(&["DP-1"], &[], persist_mode)
        }
    }

    #[test]
    fn the_store_is_capped_and_evicts_transient_grants_first() {
        let mut store = Store::default();
        // OBS's permanent grant, used long ago, outlives a stream of one-off
        // transient shares.
        store.sessions.insert("obs".into(), used_at(2, 1));
        for index in 0..MAX_SESSIONS + 10 {
            store
                .sessions
                .insert(format!("share-{index:03}"), used_at(1, 100 + index as u64));
            enforce_cap(&mut store);
        }
        assert_eq!(store.sessions.len(), MAX_SESSIONS);
        assert!(store.sessions.contains_key("obs"));
        assert!(!store.sessions.contains_key("share-000"));
        assert!(!store.sessions.contains_key("share-010"));
        assert!(store.sessions.contains_key("share-011"));
        assert!(
            store
                .sessions
                .contains_key(&format!("share-{:03}", MAX_SESSIONS + 9))
        );

        // Among permanent grants alone, the least recently used goes.
        let mut store = Store::default();
        for index in 0..=MAX_SESSIONS {
            store.sessions.insert(
                format!("grant-{index:03}"),
                used_at(2, 1_000 - index as u64),
            );
        }
        enforce_cap(&mut store);
        assert_eq!(store.sessions.len(), MAX_SESSIONS);
        assert!(
            !store
                .sessions
                .contains_key(&format!("grant-{MAX_SESSIONS:03}"))
        );
        assert!(store.sessions.contains_key("grant-000"));
    }

    #[test]
    fn saving_replaces_the_file_instead_of_rewriting_it_in_place() {
        let directory =
            std::env::temp_dir().join(format!("jwm-portal-restore-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let path = directory.join("jwm-portal").join("sessions.json");
        let mut store = Store::default();
        store.sessions.insert("old-token".into(), used_at(2, 1));
        write_store(&path, &store).expect("first save");

        // A second name for the old file sees whether the save truncated and
        // rewrote it (what a failed write would have left half-done) or put
        // a complete new file in its place.
        let witness = directory.join("witness.json");
        std::fs::hard_link(&path, &witness).expect("link old file");
        store.sessions.clear();
        store.sessions.insert("new-token".into(), used_at(2, 2));
        write_store(&path, &store).expect("second save");

        let old = std::fs::read_to_string(&witness).expect("read old file");
        let new = std::fs::read_to_string(&path).expect("read new file");
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .expect("list store directory")
            .map(|entry| entry.unwrap().file_name())
            .collect();
        let _ = std::fs::remove_dir_all(&directory);

        assert!(old.contains("old-token") && !old.contains("new-token"));
        assert!(new.contains("new-token") && !new.contains("old-token"));
        assert_eq!(leftovers, ["sessions.json"], "no temporary file is left");
    }

    #[test]
    fn all_sources_still_present_resolves_fully() {
        let e = entry(&["DP-1", "HDMI-A-1"], &["t-firefox"], 2);
        let outs = vec![out("DP-1"), out("HDMI-A-1"), out("eDP-1")];
        let tops = vec![top("t-firefox"), top("t-terminal")];
        let (sel, mode) = resolve_in(&e, &outs, &tops).expect("resolves");
        assert_eq!(
            sel.outputs.iter().map(|o| &o.name).collect::<Vec<_>>(),
            vec!["DP-1", "HDMI-A-1"]
        );
        assert_eq!(
            sel.toplevels
                .iter()
                .map(|t| &t.identifier)
                .collect::<Vec<_>>(),
            vec!["t-firefox"]
        );
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
    fn restore_matches_stored_connector_when_wl_name_changed() {
        let e = entry(&["DP-1"], &[], 1);
        // wl_output renamed but DRM connector still DP-1.
        let outs = vec![out_with_connector("WL-1", "DP-1")];
        let (sel, _) = resolve_in(&e, &outs, &[]).expect("connector match");
        assert_eq!(sel.outputs[0].name, "WL-1");
        assert_eq!(sel.outputs[0].connector.as_deref(), Some("DP-1"));
    }

    #[test]
    fn output_restore_key_prefers_connector() {
        assert_eq!(output_restore_key(&out("DP-1")), "DP-1");
        assert_eq!(
            output_restore_key(&out_with_connector("WL-1", "DP-1")),
            "DP-1"
        );
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

#[cfg(test)]
mod backend_wire_tests {
    use super::*;
    use zvariant::{OwnedValue, Value};

    #[test]
    fn restore_data_has_backend_signature_and_round_trips() {
        let data = encode_data("private-key").unwrap();
        assert_eq!(data.value_signature().to_string(), "(suv)");
        assert_eq!(decode_data(&data).as_deref(), Some("private-key"));
    }

    #[test]
    fn foreign_or_malformed_restore_data_is_not_honored() {
        let values = [
            Value::from(("OTHER", RESTORE_VERSION, Value::from("key"))),
            Value::from((RESTORE_VENDOR, 999u32, Value::from("key"))),
            Value::from((RESTORE_VENDOR, RESTORE_VERSION, Value::from(42u32))),
            Value::from((RESTORE_VENDOR, RESTORE_VERSION, "unwrapped")),
            Value::from((RESTORE_VENDOR, RESTORE_VERSION)),
            Value::from("not-a-structure"),
        ];
        for value in values {
            let value: OwnedValue = value.try_into().unwrap();
            assert!(decode_data(&value).is_none());
        }
        assert!(decode_data(&encode_data("").unwrap()).is_none());
    }
}
