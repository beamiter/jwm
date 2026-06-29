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
use crate::jwm::Jwm;
use crate::jwm::types::WMArgEnum;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const SESSION_VERSION: u32 = 1;

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

impl SessionSnapshot {
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}

/// 会话文件路径：优先 `$XDG_CACHE_HOME/jwm/session.json`，
/// 否则 `$HOME/.cache/jwm/session.json`，再否则 `/tmp/jwm-session.json`。
pub fn session_file_path() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_CACHE_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("jwm").join("session.json");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home)
                .join(".cache")
                .join("jwm")
                .join("session.json");
        }
    }
    PathBuf::from("/tmp/jwm-session.json")
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
        let floating = if c.state.is_floating {
            Some((
                c.geometry.floating_x,
                c.geometry.floating_y,
                c.geometry.floating_w,
                c.geometry.floating_h,
            ))
        } else {
            None
        };
        clients.push(SessionEntry {
            class: c.class.clone(),
            instance: c.instance.clone(),
            name: c.name.clone(),
            tags: c.state.tags,
            is_floating: c.state.is_floating,
            monitor_num: c.monitor_num,
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
                RestorePlan {
                    tags: e.tags,
                    is_floating: e.is_floating,
                    floating: e.floating,
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
        let json = snapshot.to_json()?;

        let path = session_file_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, json)?;
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
        let path = session_file_path();
        let json = match std::fs::read_to_string(&path) {
            Ok(j) => j,
            Err(e) => {
                log::warn!(
                    "session restore skipped: cannot read {}: {e}",
                    path.display()
                );
                return Ok(());
            }
        };
        let snapshot = match SessionSnapshot::from_json(&json) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("session restore skipped: invalid session file: {e}");
                return Ok(());
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

        let plans = plan_restore(
            &snapshot,
            current.iter().map(|(k, c, i)| (*k, c.as_str(), i.as_str())),
        );

        let mut floats: Vec<(ClientKey, (i32, i32, i32, i32))> = Vec::new();
        for (key, plan) in &plans {
            if let Some(c) = self.state.clients.get_mut(*key) {
                c.state.tags = plan.tags;
                c.state.is_floating = plan.is_floating;
                if let Some((x, y, w, h)) = plan.floating {
                    c.geometry.floating_x = x;
                    c.geometry.floating_y = y;
                    c.geometry.floating_w = w;
                    c.geometry.floating_h = h;
                    floats.push((*key, (x, y, w, h)));
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use slotmap::SlotMap;

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
