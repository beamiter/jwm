//! 会话保存 / 恢复（Session save / restore）
//!
//! 将当前所有客户端的标签（tags）与浮动布局快照写入磁盘，便于在重启
//! 窗口管理器（或重新启动应用）后，把窗口重新归位到原来的标签 / 浮动状态。
//!
//! 由于重启后窗口是全新的 `WindowId`，恢复通过 class + instance 匹配实现：
//! `save_session` 写出快照，`restore_session` 读取快照并把保存的状态套用到
//! 当前已打开、且 class/instance 匹配的客户端上。

use crate::backend::api::Backend;
use crate::config::CONFIG;
use crate::core::models::ClientKey;
use crate::core::state::WMState;
use crate::core::types::Rect;
use crate::jwm::Jwm;
use crate::jwm::geometry::GeometryConstraints;
use crate::jwm::types::WMArgEnum;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const SESSION_VERSION: u32 = 2;
const MIN_SUPPORTED_SESSION_VERSION: u32 = 1;
const MAX_SESSION_BYTES: u64 = 4 * 1024 * 1024;
const MAX_SESSION_CLIENTS: usize = 16_384;
static SESSION_WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 单个客户端的会话条目（按 class/instance 匹配，不持久化 WindowId）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEntry {
    pub class: String,
    pub instance: String,
    pub name: String,
    pub tags: u32,
    pub is_floating: bool,
    pub monitor_num: u32,
    /// 浮动几何 (x, y, w, h)；仅当窗口为浮动时记录。
    pub floating: Option<(i32, i32, i32, i32)>,
}

/// 整个会话快照。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub version: u32,
    pub clients: Vec<SessionEntry>,
}

/// 恢复时套用到某个客户端的计划（纯数据，便于单元测试）。
#[derive(Debug, Clone, PartialEq)]
pub struct RestorePlan {
    pub tags: u32,
    pub is_floating: bool,
    pub floating: Option<(i32, i32, i32, i32)>,
}

/// Extra persisted placement used by JWM itself. Keeping this separate from
/// the public `RestorePlan` preserves its existing source-compatible shape.
#[derive(Debug, Clone, PartialEq)]
struct DetailedRestorePlan {
    restore: RestorePlan,
    monitor_num: u32,
}

impl SessionSnapshot {
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    fn validate(&self) -> Result<(), String> {
        if !(MIN_SUPPORTED_SESSION_VERSION..=SESSION_VERSION).contains(&self.version) {
            return Err(format!(
                "unsupported session version {}; supported versions are {}..={}",
                self.version, MIN_SUPPORTED_SESSION_VERSION, SESSION_VERSION
            ));
        }
        if self.clients.len() > MAX_SESSION_CLIENTS {
            return Err(format!(
                "session contains {} clients, exceeding the limit of {MAX_SESSION_CLIENTS}",
                self.clients.len()
            ));
        }
        for (index, entry) in self.clients.iter().enumerate() {
            if entry.class.len() > 65_536
                || entry.instance.len() > 65_536
                || entry.name.len() > 65_536
            {
                return Err(format!(
                    "session client {index} contains oversized text fields"
                ));
            }
            if let Some((_, _, width, height)) = entry.floating
                && (width <= 0 || height <= 0)
            {
                return Err(format!(
                    "session client {index} has invalid floating size {width}x{height}"
                ));
            }
            if !entry.is_floating && entry.floating.is_some() {
                return Err(format!(
                    "session client {index} has floating geometry but is not floating"
                ));
            }
        }
        Ok(())
    }
}

/// 会话是需要跨重启保留的用户状态，优先写入 XDG state 目录；没有可用的
/// home/state 目录时使用按 uid 隔离的私有临时目录。
pub fn session_file_path() -> PathBuf {
    if let Some(path) = absolute_env_path("XDG_STATE_HOME") {
        return path.join("jwm").join("session.json");
    }
    if let Some(home) = absolute_env_path("HOME") {
        return home
            .join(".local")
            .join("state")
            .join("jwm")
            .join("session.json");
    }
    let uid = unsafe { libc::geteuid() };
    PathBuf::from(format!("/tmp/jwm-session-{uid}")).join("session.json")
}

fn absolute_env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

fn legacy_session_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(cache) = absolute_env_path("XDG_CACHE_HOME") {
        paths.push(cache.join("jwm").join("session.json"));
    }
    if let Some(home) = absolute_env_path("HOME") {
        paths.push(home.join(".cache").join("jwm").join("session.json"));
    }
    // JWM 0.1 used this global fallback when no cache/home directory was
    // configured. The secure loader below accepts it only when it is a regular,
    // current-user-owned file that is not writable by group or other users.
    paths.push(PathBuf::from("/tmp/jwm-session.json"));
    paths
}

fn session_read_path() -> PathBuf {
    let current = session_file_path();
    if path_entry_exists(&current) {
        return current;
    }
    legacy_session_paths()
        .into_iter()
        .find(|path| path_entry_exists(path))
        .unwrap_or(current)
}

/// Treat errors other than `NotFound` as an existing entry so the subsequent
/// secure loader reports them instead of silently falling back to older state.
fn path_entry_exists(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(error) => error.kind() != io::ErrorKind::NotFound,
    }
}

fn ensure_private_directory(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "session directory is not a real directory: {}",
                        path.display()
                    ),
                ));
            }
            if metadata.uid() != unsafe { libc::geteuid() } {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "session directory is owned by another user: {}",
                        path.display()
                    ),
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir_all(path)?,
        Err(error) => return Err(error),
    }

    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("session directory is unsafe: {}", path.display()),
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

fn atomic_write_session(path: &Path, contents: &[u8]) -> io::Result<()> {
    if contents.len() as u64 > MAX_SESSION_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session snapshot exceeds the 4 MiB limit",
        ));
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    ensure_private_directory(parent)?;

    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to replace session symlink: {}", path.display()),
        ));
    }

    let sequence = SESSION_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".session.json.tmp-{}-{sequence}",
        std::process::id()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn load_session_snapshot(path: &Path) -> Result<SessionSnapshot, Box<dyn std::error::Error>> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!("session path is not a regular file: {}", path.display()).into());
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(format!("session file is owned by another user: {}", path.display()).into());
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(format!(
            "session file is writable by another user or group: {}",
            path.display()
        )
        .into());
    }
    if metadata.len() > MAX_SESSION_BYTES {
        return Err(format!("session file exceeds the 4 MiB limit: {}", path.display()).into());
    }
    let json = fs::read_to_string(path)?;
    let snapshot = SessionSnapshot::from_json(&json)?;
    snapshot
        .validate()
        .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
    Ok(snapshot)
}

/// 从窗口状态构建快照，跳过状态栏与 dock。
pub fn capture_snapshot(state: &WMState, status_bar_name: &str) -> SessionSnapshot {
    let mut clients = Vec::new();
    for key in &state.client_order {
        let Some(c) = state.clients.get(*key) else {
            continue;
        };
        if c.state.is_dock || c.is_status_bar(status_bar_name) {
            continue;
        }
        let floating =
            if c.state.is_floating && c.geometry.floating_w > 0 && c.geometry.floating_h > 0 {
                Some((
                    c.geometry.floating_x,
                    c.geometry.floating_y,
                    c.geometry.floating_w,
                    c.geometry.floating_h,
                ))
            } else if c.state.is_floating && c.geometry.w > 0 && c.geometry.h > 0 {
                Some((c.geometry.x, c.geometry.y, c.geometry.w, c.geometry.h))
            } else {
                None
            };
        let monitor_num = c
            .mon
            .and_then(|monitor_key| state.monitors.get(monitor_key))
            .and_then(|monitor| u32::try_from(monitor.num).ok())
            .unwrap_or(0);
        clients.push(SessionEntry {
            class: c.class.clone(),
            instance: c.instance.clone(),
            name: c.name.clone(),
            tags: c.state.tags,
            is_floating: c.state.is_floating,
            monitor_num,
            floating,
        });
    }
    SessionSnapshot {
        version: SESSION_VERSION,
        clients,
    }
}

/// 把快照匹配到当前客户端，生成恢复计划。
///
/// 匹配规则：class 必须忽略大小写相等；若双方都有 instance，则 instance 也需
/// 相等。每个保存条目最多匹配一个客户端（已用过的条目不再匹配），从而让同一
/// 应用的多个实例尽量映射到不同的保存条目。具有精确 instance 匹配的条目优先。
pub fn plan_restore<'a, I>(snapshot: &SessionSnapshot, clients: I) -> Vec<(ClientKey, RestorePlan)>
where
    I: IntoIterator<Item = (ClientKey, &'a str, &'a str)>,
{
    plan_restore_detailed(snapshot, clients)
        .into_iter()
        .map(|(key, plan)| (key, plan.restore))
        .collect()
}

fn plan_restore_detailed<'a, I>(
    snapshot: &SessionSnapshot,
    clients: I,
) -> Vec<(ClientKey, DetailedRestorePlan)>
where
    I: IntoIterator<Item = (ClientKey, &'a str, &'a str)>,
{
    let mut used = vec![false; snapshot.clients.len()];
    let mut out = Vec::new();

    for (key, class, instance) in clients {
        let mut fallback: Option<usize> = None;
        let mut exact: Option<usize> = None;

        for (i, e) in snapshot.clients.iter().enumerate() {
            if used[i] || !e.class.eq_ignore_ascii_case(class) {
                continue;
            }
            let both_have_instance = !e.instance.is_empty() && !instance.is_empty();
            if both_have_instance {
                if e.instance.eq_ignore_ascii_case(instance) {
                    exact = Some(i);
                    break;
                }
                // class matches but instance differs — not a candidate.
                continue;
            }
            if fallback.is_none() {
                fallback = Some(i);
            }
        }

        if let Some(i) = exact.or(fallback) {
            used[i] = true;
            let e = &snapshot.clients[i];
            out.push((
                key,
                DetailedRestorePlan {
                    restore: RestorePlan {
                        tags: e.tags,
                        is_floating: e.is_floating,
                        floating: e.floating,
                    },
                    monitor_num: e.monitor_num,
                },
            ));
        }
    }

    out
}

impl Jwm {
    /// 保存当前会话（窗口标签 / 浮动布局）到磁盘。
    pub fn save_session(
        &mut self,
        _backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let status_bar_name = CONFIG.load().status_bar_name().to_string();
        let snapshot = capture_snapshot(&self.state, &status_bar_name);
        snapshot
            .validate()
            .map_err(|error| format!("cannot save invalid session: {error}"))?;
        let json = snapshot.to_json()?;

        let path = session_file_path();
        atomic_write_session(&path, json.as_bytes())?;
        log::info!(
            "session saved: {} clients -> {}",
            snapshot.clients.len(),
            path.display()
        );
        Ok(())
    }

    /// 从磁盘恢复会话：把保存的标签 / 浮动状态套用到当前匹配的客户端。
    pub fn restore_session(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let path = session_read_path();
        let snapshot = match load_session_snapshot(&path) {
            Ok(snapshot) => snapshot,
            Err(error)
                if error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) =>
            {
                log::info!("session restore skipped: no snapshot at {}", path.display());
                return Ok(());
            }
            Err(error) => {
                return Err(format!("cannot restore session {}: {error}", path.display()).into());
            }
        };

        // Build (key, class, instance) view of current clients.
        let current: Vec<(ClientKey, String, String)> = self
            .state
            .client_order
            .iter()
            .filter_map(|k| {
                self.state
                    .clients
                    .get(*k)
                    .map(|c| (*k, c.class.clone(), c.instance.clone()))
            })
            .collect();

        let plans = plan_restore_detailed(
            &snapshot,
            current.iter().map(|(k, c, i)| (*k, c.as_str(), i.as_str())),
        );

        for (key, plan) in &plans {
            let target_monitor = self
                .state
                .monitor_order
                .iter()
                .copied()
                .find(|monitor_key| {
                    self.state
                        .monitors
                        .get(*monitor_key)
                        .is_some_and(|monitor| u32::try_from(monitor.num) == Ok(plan.monitor_num))
                });
            if let Some(target_monitor) = target_monitor
                && self.state.clients.get(*key).and_then(|client| client.mon)
                    != Some(target_monitor)
            {
                self.sendmon(backend, Some(*key), Some(target_monitor));
            }
        }

        let tagmask = CONFIG.load().tagmask();
        let mut floats: Vec<(ClientKey, (i32, i32, i32, i32))> = Vec::new();
        for (key, plan) in &plans {
            let restore = &plan.restore;
            let monitor_key = self.state.clients.get(*key).and_then(|client| client.mon);
            let fallback_tags = monitor_key
                .and_then(|key| self.state.monitors.get(key))
                .map(|monitor| monitor.get_active_tags() & tagmask)
                .filter(|tags| *tags != 0)
                .unwrap_or(1);
            let restored_tags = sanitize_tags(restore.tags, tagmask, fallback_tags);
            let floating = restore.floating.and_then(|floating| {
                monitor_key
                    .and_then(|key| self.monitor_work_area(key))
                    .map(|area| clamp_floating_rect(floating, area))
            });
            if let Some(c) = self.state.clients.get_mut(*key) {
                c.state.tags = restored_tags;
                c.state.is_floating = restore.is_floating;
                if let Some((x, y, w, h)) = floating {
                    c.geometry.floating_x = x;
                    c.geometry.floating_y = y;
                    c.geometry.floating_w = w;
                    c.geometry.floating_h = h;
                    floats.push((*key, (x, y, w, h)));
                }
            }
            self.reorder_client_in_monitor_groups(*key);
            if let Err(error) = self.setclienttagprop(backend, *key) {
                log::warn!("session restore could not update client metadata: {error}");
            }
        }

        for (key, (x, y, w, h)) in floats {
            self.resize_client(backend, key, x, y, w, h, false);
        }

        let monitor_keys: Vec<_> = self.state.monitor_order.clone();
        for mk in monitor_keys {
            self.arrange(backend, Some(mk));
        }

        log::info!("session restored: {} clients matched", plans.len());
        Ok(())
    }
}

fn clamp_floating_rect(
    (mut x, mut y, width, height): (i32, i32, i32, i32),
    boundary: Rect,
) -> (i32, i32, i32, i32) {
    let width = width.clamp(1, boundary.w.max(1));
    let height = height.clamp(1, boundary.h.max(1));
    GeometryConstraints::clamp_rect_to_boundary(&mut x, &mut y, width, height, &boundary);
    (x, y, width, height)
}

fn sanitize_tags(saved: u32, tagmask: u32, fallback: u32) -> u32 {
    let saved = saved & tagmask;
    if saved != 0 {
        return saved;
    }
    let fallback = fallback & tagmask;
    if fallback != 0 { fallback } else { tagmask & 1 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::common_define::WindowId;
    use crate::core::models::{WMClient, WMMonitor};
    use slotmap::SlotMap;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let sequence = SESSION_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "jwm-session-{label}-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn entry(class: &str, instance: &str, tags: u32) -> SessionEntry {
        SessionEntry {
            class: class.to_string(),
            instance: instance.to_string(),
            name: String::new(),
            tags,
            is_floating: false,
            monitor_num: 0,
            floating: None,
        }
    }

    fn keys(n: usize) -> Vec<ClientKey> {
        let mut sm: SlotMap<ClientKey, ()> = SlotMap::new();
        (0..n).map(|_| sm.insert(())).collect()
    }

    #[test]
    fn json_round_trip_is_lossless() {
        let snap = SessionSnapshot {
            version: SESSION_VERSION,
            clients: vec![
                SessionEntry {
                    class: "Firefox".into(),
                    instance: "Navigator".into(),
                    name: "title".into(),
                    tags: 0b101,
                    is_floating: true,
                    monitor_num: 1,
                    floating: Some((10, 20, 800, 600)),
                },
                entry("Alacritty", "alacritty", 0b1),
            ],
        };
        let json = snap.to_json().unwrap();
        let back = SessionSnapshot::from_json(&json).unwrap();
        assert_eq!(snap, back);
    }

    #[test]
    fn validation_accepts_v1_and_rejects_unknown_or_invalid_snapshots() {
        let mut snapshot = SessionSnapshot {
            version: 1,
            clients: vec![entry("Term", "kitty", 1)],
        };
        assert!(snapshot.validate().is_ok());

        snapshot.version = SESSION_VERSION + 1;
        assert!(snapshot.validate().unwrap_err().contains("unsupported"));

        snapshot.version = SESSION_VERSION;
        snapshot.clients[0].floating = Some((0, 0, -1, 100));
        assert!(snapshot.validate().unwrap_err().contains("floating size"));
    }

    #[test]
    fn atomic_store_is_private_roundtrips_and_rejects_symlinks() {
        let root = TestDir::new("atomic");
        let path = root.0.join("state").join("session.json");
        atomic_write_session(&path, br#"{"version":2,"clients":[]}"#).unwrap();

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
        assert_eq!(
            load_session_snapshot(&path).unwrap().version,
            SESSION_VERSION
        );

        let victim = root.0.join("victim");
        fs::write(&victim, "unchanged").unwrap();
        let link = root.0.join("state").join("linked.json");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        assert!(atomic_write_session(&link, b"replacement").is_err());
        assert_eq!(fs::read_to_string(victim).unwrap(), "unchanged");
    }

    #[test]
    fn missing_snapshot_remains_distinguishable_from_an_invalid_snapshot() {
        let root = TestDir::new("missing");
        let missing = root.0.join("missing.json");
        let error = load_session_snapshot(&missing).unwrap_err();
        assert!(
            error
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
        );

        let invalid = root.0.join("invalid.json");
        fs::write(&invalid, "not JSON").unwrap();
        let error = load_session_snapshot(&invalid).unwrap_err();
        assert!(error.downcast_ref::<io::Error>().is_none());
    }

    #[test]
    fn capture_uses_live_monitor_relation() {
        let mut state = WMState::new();
        let mut monitor = WMMonitor::new();
        monitor.num = 7;
        let monitor_key = state.monitors.insert(monitor);
        state.monitor_order.push(monitor_key);

        let mut client = WMClient::new(WindowId::from_raw(42));
        client.class = "Term".into();
        client.instance = "kitty".into();
        client.state.tags = 1;
        client.mon = Some(monitor_key);
        let client_key = state.clients.insert(client);
        state.client_order.push(client_key);

        let snapshot = capture_snapshot(&state, "status-bar");
        assert_eq!(snapshot.clients[0].monitor_num, 7);
    }

    #[test]
    fn restored_tags_are_masked_and_have_a_safe_fallback() {
        assert_eq!(sanitize_tags(0b1_0000, 0b1111, 0b0100), 0b0100);
        assert_eq!(sanitize_tags(0b1010, 0b0111, 0b0001), 0b0010);
        assert_eq!(sanitize_tags(0, 0b1111, 0), 1);
    }

    #[test]
    fn floating_geometry_is_resized_and_clamped_to_monitor() {
        assert_eq!(
            clamp_floating_rect((-500, 900, 2000, 0), Rect::new(100, 200, 800, 600)),
            (100, 799, 800, 1)
        );
    }

    #[test]
    fn plan_restore_matches_by_class_and_instance() {
        let snap = SessionSnapshot {
            version: SESSION_VERSION,
            clients: vec![entry("Firefox", "Navigator", 0b100)],
        };
        let k = keys(1);
        let plans = plan_restore(&snap, vec![(k[0], "firefox", "navigator")]);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].0, k[0]);
        assert_eq!(plans[0].1.tags, 0b100);
        assert_eq!(
            plan_restore_detailed(&snap, vec![(k[0], "firefox", "navigator")])[0]
                .1
                .monitor_num,
            0
        );
    }

    #[test]
    fn plan_restore_no_match_returns_empty() {
        let snap = SessionSnapshot {
            version: SESSION_VERSION,
            clients: vec![entry("Firefox", "Navigator", 0b100)],
        };
        let k = keys(1);
        let plans = plan_restore(&snap, vec![(k[0], "Alacritty", "alacritty")]);
        assert!(plans.is_empty());
    }

    #[test]
    fn plan_restore_each_entry_used_at_most_once() {
        // Two terminals saved on different tags; two open terminals should map
        // to distinct entries rather than both matching the first.
        let snap = SessionSnapshot {
            version: SESSION_VERSION,
            clients: vec![entry("Term", "", 0b1), entry("Term", "", 0b10)],
        };
        let k = keys(2);
        let plans = plan_restore(&snap, vec![(k[0], "Term", ""), (k[1], "Term", "")]);
        assert_eq!(plans.len(), 2);
        let tags: Vec<u32> = plans.iter().map(|(_, p)| p.tags).collect();
        assert!(tags.contains(&0b1) && tags.contains(&0b10));
    }

    #[test]
    fn plan_restore_prefers_exact_instance_over_fallback() {
        // Entry 0: class matches, no instance (fallback). Entry 1: exact instance.
        let snap = SessionSnapshot {
            version: SESSION_VERSION,
            clients: vec![entry("Term", "", 0b1), entry("Term", "kitty", 0b1000)],
        };
        let k = keys(1);
        let plans = plan_restore(&snap, vec![(k[0], "Term", "kitty")]);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].1.tags, 0b1000, "should pick exact-instance entry");
    }

    #[test]
    fn plan_restore_class_match_but_instance_differs_is_skipped_when_both_present() {
        let snap = SessionSnapshot {
            version: SESSION_VERSION,
            clients: vec![entry("Term", "kitty", 0b1)],
        };
        let k = keys(1);
        // Both have instance, but they differ -> no match.
        let plans = plan_restore(&snap, vec![(k[0], "Term", "alacritty")]);
        assert!(plans.is_empty());
    }
}
