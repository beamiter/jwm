//! Explicit spawn pins and named dropdown sizes, independent of close history.

use crate::backend::api::{Backend, ToastNotification};
use crate::config::CONFIG;
use crate::core::models::ClientKey;
use crate::core::types::Rect;
use crate::jwm::closed_placement::{PlacementIdentity, atomic_write_closed_placement};
use crate::jwm::scratchpad_pending::valid_scratchpad_name;
use crate::jwm::{Jwm, RuleMatcher, WMArgEnum};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAX_PINS: usize = 256;
const MAX_SCRATCHPADS: usize = 64;
const MAX_BYTES: u64 = 1024 * 1024;
const SAVE_DELAY: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SpawnPlacement {
    identity: PlacementIdentity,
    monitor_num: i32,
    connector: Option<String>,
    tags: u32,
}

/// Fractions of the usable output area, so the dropdown fits another screen.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct ScratchpadSize {
    width: f64,
    height: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Preferences {
    version: u32,
    #[serde(default)]
    spawn: Vec<SpawnPlacement>,
    #[serde(default)]
    scratchpads: HashMap<String, ScratchpadSize>,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            version: 1,
            spawn: Vec::new(),
            scratchpads: HashMap::new(),
        }
    }
}

impl Preferences {
    fn validate(&self) -> io::Result<()> {
        let mut seen = HashSet::new();
        if self.version != 1
            || self.spawn.len() > MAX_PINS
            || self.scratchpads.len() > MAX_SCRATCHPADS
        {
            return Err(io::Error::other(
                "unsupported or oversized window preferences",
            ));
        }
        for pin in &self.spawn {
            let class = &pin.identity.class;
            let instance = &pin.identity.instance;
            if PlacementIdentity::new(class, instance).is_none()
                || class.len() > 65_536
                || instance.len() > 65_536
                || pin.connector.as_ref().is_some_and(|key| key.len() > 65_536)
                || pin.tags == 0
                || pin.monitor_num < 0
                || !seen.insert(&pin.identity)
            {
                return Err(io::Error::other("invalid or duplicate spawn placement"));
            }
        }
        for (name, size) in &self.scratchpads {
            if !valid_scratchpad_name(name)
                || !size.width.is_finite()
                || !size.height.is_finite()
                || size.width <= 0.0
                || size.width > 1.0
                || size.height <= 0.0
                || size.height > 1.0
            {
                return Err(io::Error::other("invalid scratchpad size"));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
pub(crate) struct WindowPreferences {
    data: Preferences,
    // None is the in-memory store used by unit tests; never touch real XDG state.
    path: Option<PathBuf>,
    dirty: Option<Instant>,
    reported_save_error: bool,
}

impl WindowPreferences {
    pub(crate) fn load() -> Self {
        #[cfg(test)]
        {
            Self::default()
        }
        #[cfg(not(test))]
        {
            Self::load_from_path(
                crate::jwm::session::session_file_path().with_file_name("window_preferences.json"),
            )
        }
    }

    fn load_from_path(path: PathBuf) -> Self {
        let data = match Self::read(&path) {
            Ok(data) => data,
            Err(error) => {
                if error.kind() != io::ErrorKind::NotFound {
                    log::warn!(
                        "[window-preferences] could not load {}: {error}",
                        path.display()
                    );
                }
                Preferences::default()
            }
        };
        Self {
            data,
            path: Some(path),
            ..Self::default()
        }
    }

    fn read(path: &Path) -> io::Result<Preferences> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o022 != 0
            || metadata.len() > MAX_BYTES
        {
            return Err(io::Error::other(
                "unsafe or oversized window preferences file",
            ));
        }
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(io::Error::other("window preferences exceed 1 MiB"));
        }
        let data: Preferences = serde_json::from_slice(&bytes)?;
        data.validate()?;
        Ok(data)
    }

    fn save(&self, data: &Preferences) -> io::Result<()> {
        data.validate()?;
        if let Some(path) = &self.path {
            atomic_write_closed_placement(path, &serde_json::to_vec(data)?)?;
        }
        Ok(())
    }

    fn pin(&self, identity: &PlacementIdentity) -> Option<&SpawnPlacement> {
        self.data.spawn.iter().find(|pin| &pin.identity == identity)
    }

    fn toggle_pin(&mut self, pin: SpawnPlacement) -> io::Result<bool> {
        let mut next = self.data.clone();
        let enabled = if let Some(index) = next
            .spawn
            .iter()
            .position(|old| old.identity == pin.identity)
        {
            next.spawn.remove(index);
            false
        } else {
            if next.spawn.len() >= MAX_PINS {
                return Err(io::Error::other(
                    "spawn pin limit reached (256 applications)",
                ));
            }
            next.spawn.push(pin);
            true
        };
        self.save(&next)?;
        self.data = next;
        self.dirty = None;
        self.reported_save_error = false;
        Ok(enabled)
    }

    /// Retarget an existing pin without enabling an unpinned identity. Keep
    /// the live rule current immediately; the shared debounce persists it.
    fn retarget_pin(&mut self, pin: SpawnPlacement) {
        let Some(saved) = self
            .data
            .spawn
            .iter_mut()
            .find(|old| old.identity == pin.identity)
        else {
            return;
        };
        if pin.tags == 0
            || pin.monitor_num < 0
            || (saved.tags == pin.tags
                && saved.monitor_num == pin.monitor_num
                && saved.connector == pin.connector)
        {
            return;
        }
        *saved = pin;
        self.dirty = Some(Instant::now());
    }

    fn remember_size(&mut self, name: &str, rect: Rect, area: Rect) {
        if !valid_scratchpad_name(name) || rect.w <= 0 || rect.h <= 0 || area.w <= 0 || area.h <= 0
        {
            return;
        }
        let size = ScratchpadSize {
            width: (f64::from(rect.w) / f64::from(area.w)).min(1.0),
            height: (f64::from(rect.h) / f64::from(area.h)).min(1.0),
        };
        if self.data.scratchpads.get(name) == Some(&size)
            || (!self.data.scratchpads.contains_key(name)
                && self.data.scratchpads.len() >= MAX_SCRATCHPADS)
        {
            return;
        }
        self.data.scratchpads.insert(name.to_owned(), size);
        self.dirty = Some(Instant::now());
    }

    pub(crate) fn scratchpad_rect(&self, name: &str, area: Rect) -> Rect {
        let (w, h) = if let Some(size) = self.data.scratchpads.get(name) {
            (
                (f64::from(area.w) * size.width).round() as i32,
                (f64::from(area.h) * size.height).round() as i32,
            )
        } else {
            (area.w.saturating_mul(4) / 5, area.h.saturating_mul(4) / 5)
        };
        let (w, h) = (w.clamp(1, area.w.max(1)), h.clamp(1, area.h.max(1)));
        Rect::new(area.x + (area.w - w) / 2, area.y + (area.h - h) / 2, w, h)
    }
}

impl Jwm {
    pub(crate) fn client_spawn_placement_pinned(&self, key: ClientKey) -> Option<bool> {
        let client = self.state.clients.get(key)?;
        let identity = PlacementIdentity::new(&client.class, &client.instance)?;
        Some(self.window_preferences.pin(&identity).is_some())
    }
    pub fn toggle_spawn_placement(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(key) = self.get_selected_client_key() else {
            return Ok(());
        };
        let result = self.toggle_client_spawn_placement(backend, key);
        let (title, body, urgency) = match result {
            Ok((enabled, body)) => (
                if enabled {
                    "Spawn location pinned"
                } else {
                    "Spawn location unpinned"
                },
                body,
                1,
            ),
            Err(error) => ("Spawn location not changed", error.to_string(), 2),
        };
        self.push_system_toast(
            backend,
            ToastNotification {
                title: title.into(),
                body,
                urgency,
                timeout_ms: 4000,
                ..Default::default()
            },
        );
        Ok(())
    }

    fn toggle_client_spawn_placement(
        &mut self,
        backend: &mut dyn Backend,
        key: ClientKey,
    ) -> io::Result<(bool, String)> {
        let client = self
            .state
            .clients
            .get(key)
            .ok_or_else(|| io::Error::other("Window closed"))?;
        let identity = PlacementIdentity::new(&client.class, &client.instance)
            .ok_or_else(|| io::Error::other("This window has no application identity"))?;
        let types = backend.property_ops().get_window_types(client.win);
        if client.is_status_bar(CONFIG.load().status_bar_name())
            || client.state.is_dock
            || client.state.is_desktop
            || client.state.is_sticky
            || self.scratchpads.values().any(|&value| value == key)
            || self.get_transient_for(backend, client.win).is_some()
            || RuleMatcher::types_are_popup_like(&types)
            || RuleMatcher::is_structurally_borderless(&types)
        {
            return Err(io::Error::other("Choose a regular application window"));
        }
        let monitor = client
            .mon
            .and_then(|key| self.state.monitors.get(key))
            .ok_or_else(|| io::Error::other("Window has no monitor"))?;
        let tags = client.state.tags & CONFIG.load().tagmask();
        if tags == 0 {
            return Err(io::Error::other("Window has no active tags"));
        }
        let connector = client
            .mon
            .and_then(|mon| self.output_key_for_monitor(backend, mon));
        let label = self
            .output_connector_for_monitor(backend, client.mon.unwrap())
            .unwrap_or_else(|| format!("Monitor {}", monitor.num));
        let tag_names: Vec<_> = (0..CONFIG.load().tags_length())
            .filter(|index| tags & (1 << index) != 0)
            .map(|index| (index + 1).to_string())
            .collect();
        let enabled = self.window_preferences.toggle_pin(SpawnPlacement {
            identity: identity.clone(),
            monitor_num: monitor.num,
            connector,
            tags,
        })?;
        log::info!(
            "[spawn-placement] {identity} pin {}",
            if enabled { "enabled" } else { "removed" }
        );
        Ok((
            enabled,
            if enabled {
                format!("{identity} · {label} · tag {}", tag_names.join(", "))
            } else {
                identity.to_string()
            },
        ))
    }

    /// An explicit tag move chooses a new home for an already pinned app.
    /// Do not call from manage, session restore, or automatic output migration.
    pub(crate) fn retarget_client_spawn_pin(&mut self, backend: &mut dyn Backend, key: ClientKey) {
        let Some(client) = self.state.clients.get(key) else {
            return;
        };
        let Some(identity) = PlacementIdentity::new(&client.class, &client.instance) else {
            return;
        };
        if self.window_preferences.pin(&identity).is_none()
            || client.state.is_sticky
            || client.state.is_dock
            || client.state.is_desktop
            || self.scratchpads.values().any(|&value| value == key)
            || self.get_transient_for(backend, client.win).is_some()
        {
            return;
        }
        let types = backend.property_ops().get_window_types(client.win);
        if RuleMatcher::types_are_popup_like(&types)
            || RuleMatcher::is_structurally_borderless(&types)
        {
            return;
        }
        let Some(mon) = client.mon else {
            return;
        };
        let Some(monitor) = self.state.monitors.get(mon) else {
            return;
        };
        let pin = SpawnPlacement {
            identity,
            monitor_num: monitor.num,
            connector: self.output_key_for_monitor(backend, mon),
            tags: client.state.tags & CONFIG.load().tagmask(),
        };
        self.window_preferences.retarget_pin(pin);
    }

    /// Some means this identity is explicitly pinned, even when it was already
    /// at the destination. Callers must then skip automatic close history.
    pub(crate) fn adopt_pinned_spawn_placement(
        &mut self,
        backend: &dyn Backend,
        key: ClientKey,
    ) -> Option<bool> {
        let client = self.state.clients.get(key)?;
        if self.scratchpads.values().any(|&value| value == key)
            || client
                .pid
                .is_some_and(|pid| self.scratchpad_pending.contains_pid(pid))
        {
            return None;
        }
        let identity = PlacementIdentity::new(&client.class, &client.instance)?;
        let pin = self.window_preferences.pin(&identity)?.clone();
        let live = self.live_monitor_identities(backend);
        let live: Vec<_> = live
            .iter()
            .map(|(num, connector, stable)| (*num, connector.as_str(), stable.as_str()))
            .collect();
        let number = crate::jwm::closed_placement::resolve_monitor_num_by_connector(
            pin.monitor_num,
            pin.connector.as_deref(),
            &live,
        );
        let mon = self.get_monitor_by_id(number);
        let tags = pin.tags & CONFIG.load().tagmask();
        let client = self.state.clients.get_mut(key)?;
        let mut moved = false;
        if let Some(mon) = mon {
            moved |= client.mon != Some(mon);
            client.mon = Some(mon);
        }
        if tags != 0 {
            moved |= client.state.tags != tags;
            client.state.tags = tags;
        }
        log::info!("[spawn-placement] {identity} opens on monitor {number} tags {tags:#b}");
        Some(moved || !self.shares_focused_view(key))
    }

    pub(crate) fn scratchpad_reveal_rect(&self, key: ClientKey, area: Rect) -> Rect {
        let name = self
            .scratchpads
            .iter()
            .find(|(_, value)| **value == key)
            .map_or("term", |(name, _)| name.as_str());
        self.window_preferences.scratchpad_rect(name, area)
    }

    pub(crate) fn remember_scratchpad_size(&mut self, key: ClientKey) {
        let Some((name, _)) = self.scratchpads.iter().find(|(_, value)| **value == key) else {
            return;
        };
        let Some(client) = self.state.clients.get(key) else {
            return;
        };
        if !client.state.is_floating
            || client.state.is_fullscreen
            || client.state.maximized_axes().any()
            || client.state.is_pip
            || client.state.is_hidden
            || client.state.tags == 0
        {
            return;
        }
        let Some(area) = client.mon.and_then(|mon| self.monitor_work_area(mon)) else {
            return;
        };
        let rect = Rect::new(
            client.geometry.x,
            client.geometry.y,
            client.geometry.w,
            client.geometry.h,
        );
        self.window_preferences.remember_size(name, rect, area);
    }

    pub(crate) fn flush_window_preferences(
        &mut self,
        backend: &mut dyn Backend,
        now: Instant,
        force: bool,
    ) {
        let store = &mut self.window_preferences;
        let Some(changed) = store.dirty else {
            return;
        };
        if !force && now.saturating_duration_since(changed) < SAVE_DELAY {
            return;
        }
        match store.save(&store.data) {
            Ok(()) => {
                store.dirty = None;
                store.reported_save_error = false;
            }
            Err(error) => {
                // A transient failure is retried on the next debounce. One
                // that would fail again (the file refused as a symlink, a
                // read-only or full disk, permissions) waits for the next
                // change instead of rewriting and warning every half second.
                store.dirty = save_failure_is_transient(&error).then_some(now);
                log::warn!("[window-preferences] could not save dropdown size: {error}");
                if !std::mem::replace(&mut store.reported_save_error, true) {
                    self.push_system_toast(
                        backend,
                        ToastNotification {
                            title: "Dropdown size not saved".into(),
                            body: error.to_string(),
                            urgency: 2,
                            timeout_ms: 6000,
                            ..Default::default()
                        },
                    );
                }
            }
        }
    }

    pub(crate) fn window_preferences_next_wakeup(&self, now: Instant) -> Option<Duration> {
        self.window_preferences
            .dirty
            .map(|changed| (changed + SAVE_DELAY).saturating_duration_since(now))
    }
}

/// Whether a failed preferences write is worth retrying on the next
/// debounce; see `layout::persist` for the same rule on layouts.
fn save_failure_is_transient(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::Interrupted
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::TimedOut
            | io::ErrorKind::AlreadyExists
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "jwm-window-preferences-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> PathBuf {
            self.0.join("window_preferences.json")
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn pin() -> SpawnPlacement {
        SpawnPlacement {
            identity: PlacementIdentity::new("Firefox", "Navigator").unwrap(),
            monitor_num: 1,
            connector: Some("DP-2".into()),
            tags: 0b10100,
        }
    }

    #[test]
    fn an_unwritable_preferences_file_is_not_retried_every_debounce() {
        use crate::jwm::monitor::test_support::{DisplaySpyBackend, output};
        let fixture = Fixture::new();
        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").expect("test jwm");
        // The store's path is a directory: every write fails the same way.
        jwm.window_preferences = WindowPreferences::load_from_path(fixture.0.clone());
        let area = Rect::new(0, 0, 1920, 1080);
        jwm.window_preferences
            .remember_size("term", Rect::new(100, 100, 800, 500), area);
        let now = Instant::now();
        assert!(jwm.window_preferences_next_wakeup(now).is_some());

        jwm.flush_window_preferences(&mut backend, now, true);

        assert_eq!(jwm.window_preferences_next_wakeup(now), None);
        assert!(save_failure_is_transient(&io::Error::from(
            io::ErrorKind::Interrupted
        )));
        assert!(!save_failure_is_transient(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
    }

    #[test]
    fn preferences_round_trip_pins_and_independent_dropdown_sizes() {
        let fixture = Fixture::new();
        let mut store = WindowPreferences::load_from_path(fixture.path());
        let area = Rect::new(-1920, 40, 1920, 1000);
        store.remember_size("term", Rect::new(-1500, 300, 960, 400), area);
        assert!(store.toggle_pin(pin()).unwrap());
        let mut restored = WindowPreferences::load_from_path(fixture.path());
        let saved = restored.pin(&pin().identity).unwrap();
        assert_eq!(
            (saved.monitor_num, saved.tags, saved.connector.as_deref()),
            (1, 0b10100, Some("DP-2"))
        );
        assert_eq!(
            restored.scratchpad_rect("term", area),
            Rect::new(-1440, 340, 960, 400)
        );
        assert_eq!(
            restored.scratchpad_rect("music", area),
            Rect::new(-1728, 140, 1536, 800)
        );
        assert_eq!(
            restored.scratchpad_rect("term", Rect::new(0, 0, 1000, 500)),
            Rect::new(250, 150, 500, 200)
        );
        assert!(!restored.toggle_pin(pin()).unwrap());
        let restored = WindowPreferences::load_from_path(fixture.path());
        assert!(restored.pin(&pin().identity).is_none());
        assert_eq!(restored.scratchpad_rect("term", area).w, 960);
        assert_eq!(
            fs::metadata(fixture.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn retargeted_pin_survives_reload_and_does_not_reenable_after_unpin() {
        let fixture = Fixture::new();
        let mut store = WindowPreferences::load_from_path(fixture.path());
        let mut destination = pin();
        destination.tags = 1 << 7;
        store.retarget_pin(destination.clone());
        assert!(store.pin(&destination.identity).is_none());
        assert!(store.dirty.is_none());
        store.toggle_pin(pin()).unwrap();
        store.retarget_pin(destination.clone());
        assert_eq!(store.pin(&destination.identity).unwrap().tags, 1 << 7);
        assert!(store.dirty.is_some());
        store.save(&store.data).unwrap();
        let mut restored = WindowPreferences::load_from_path(fixture.path());
        assert_eq!(restored.pin(&destination.identity).unwrap().tags, 1 << 7);
        assert!(!restored.toggle_pin(destination.clone()).unwrap());
        restored.retarget_pin(pin());
        assert!(restored.pin(&destination.identity).is_none());
        assert!(restored.dirty.is_none());
    }

    #[test]
    fn failed_pin_write_keeps_the_previous_pin() {
        let fixture = Fixture::new();
        let mut store = WindowPreferences::load_from_path(fixture.path());
        store.toggle_pin(pin()).unwrap();
        fs::remove_file(fixture.path()).unwrap();
        fs::create_dir(fixture.path()).unwrap();
        assert!(store.toggle_pin(pin()).is_err());
        assert!(store.pin(&pin().identity).is_some());
    }

    #[test]
    fn pins_match_exact_identity_and_do_not_evict_another_application() {
        let mut store = WindowPreferences::default();
        for index in 0..MAX_PINS {
            let mut entry = pin();
            entry.identity = PlacementIdentity::new("App", &index.to_string()).unwrap();
            assert!(store.toggle_pin(entry).unwrap());
        }
        assert!(store.toggle_pin(pin()).is_err());
        assert_eq!(store.data.spawn.len(), MAX_PINS);
        assert!(
            store
                .pin(&PlacementIdentity::new("App", "0").unwrap())
                .is_some()
        );
        assert!(
            store
                .pin(&PlacementIdentity::new("app", "0").unwrap())
                .is_none()
        );
    }

    #[test]
    fn malformed_or_unsafe_preferences_are_ignored() {
        let fixture = Fixture::new();
        for json in [
            r#"{"version":2}"#,
            r#"{"version":1,"scratchpads":{"term":{"width":0,"height":0.5}}}"#,
            r#"{"version":1,"scratchpads":{"term":{"width":1.1,"height":0.5}}}"#,
            "{",
        ] {
            fs::write(fixture.path(), json).unwrap();
            assert!(WindowPreferences::read(&fixture.path()).is_err());
            assert!(
                WindowPreferences::load_from_path(fixture.path())
                    .data
                    .spawn
                    .is_empty()
            );
        }
        fs::remove_file(fixture.path()).unwrap();
        let target = fixture.0.join("target");
        fs::write(&target, b"preserved").unwrap();
        std::os::unix::fs::symlink(&target, fixture.path()).unwrap();
        assert!(WindowPreferences::read(&fixture.path()).is_err());
        assert!(
            WindowPreferences::load_from_path(fixture.path())
                .toggle_pin(pin())
                .is_err()
        );
        assert_eq!(fs::read(target).unwrap(), b"preserved");
    }

    #[test]
    fn keyboard_dropdown_resize_is_debounced_and_survives_a_fresh_wm() {
        use crate::backend::common_define::WindowId;
        use crate::core::models::WMClient;
        use crate::jwm::monitor::test_support::{DisplaySpyBackend, output};

        let fixture = Fixture::new();
        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        jwm.window_preferences = WindowPreferences::load_from_path(fixture.path());
        let mon = jwm.state.monitor_order[0];
        let area = jwm.monitor_work_area(mon).unwrap();
        let mut client = WMClient::new(WindowId::from_raw(0xbe01));
        client.mon = Some(mon);
        client.state.tags = 1;
        client.state.is_floating = true;
        (
            client.geometry.x,
            client.geometry.y,
            client.geometry.w,
            client.geometry.h,
        ) = (200, 200, 800, 400);
        let key = jwm.insert_client(client);
        jwm.attach_to_monitor(key, mon);
        jwm.state.monitors[mon].sel = Some(key);
        jwm.scratchpads.insert("term".into(), key);
        jwm.resize_horizontal(&mut backend, &WMArgEnum::Float(0.05))
            .unwrap();
        jwm.resize_vertical(&mut backend, &WMArgEnum::Float(-0.05))
            .unwrap();
        let client = &jwm.state.clients[key];
        let expected = (client.geometry.w, client.geometry.h);
        assert_ne!(expected, (800, 400));
        jwm.flush_window_preferences(&mut backend, Instant::now(), false);
        assert!(
            !fixture.path().exists(),
            "held resize keys do not fsync every repeat"
        );
        jwm.flush_window_preferences(&mut backend, Instant::now() + SAVE_DELAY, false);
        let mut restarted = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        restarted.window_preferences = WindowPreferences::load_from_path(fixture.path());
        let rect = restarted.window_preferences.scratchpad_rect("term", area);
        assert_eq!((rect.w, rect.h), expected);
        assert_eq!(rect.x, area.x + (area.w - rect.w) / 2);
    }
}
