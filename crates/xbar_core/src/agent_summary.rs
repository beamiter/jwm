//! Small, content-free Agent activity for desktop chrome. Registry state, not
//! terminal titles or log text, supplies the counts. All IPC stays off UI threads.
use serde_json::Value;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, SyncSender},
};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AgentSummary {
    pub available: bool,
    pub picker_available: bool,
    pub revision: u64,
    pub running: u32,
    pub waiting: u32,
    pub unknown: u32,
    pub total: u32,
}
impl AgentSummary {
    pub fn parse(value: &Value) -> Option<Self> {
        if value["protocol_version"].as_u64()? != 1 {
            return None;
        }
        let revision = value["revision"].as_u64()?;
        if let Some(sessions) = value["sessions"].as_array() {
            if sessions.len() > 1000 {
                return None;
            }
            let mut summary = Self {
                available: true,
                picker_available: value["picker_available"].as_bool().unwrap_or(false),
                revision,
                total: sessions.len() as u32,
                ..Self::default()
            };
            for entry in sessions {
                match entry["status"].as_str()? {
                    "running" => summary.running += 1,
                    "waiting" => summary.waiting += 1,
                    "succeeded" | "failed" => {}
                    "unknown" | "disconnected" => summary.unknown += 1,
                    _ => return None,
                }
            }
            return Some(summary);
        }
        let count = |key| {
            value[key]
                .as_u64()
                .filter(|count| *count <= 1000)
                .map(|count| count as u32)
        };
        let disconnected = if value.get("disconnected").is_some() {
            count("disconnected")?
        } else {
            0
        };
        let summary = Self {
            available: value["available"].as_bool().unwrap_or(true),
            picker_available: value["picker_available"].as_bool().unwrap_or(false),
            revision,
            running: count("running")?,
            waiting: count("waiting")?,
            unknown: count("unknown")? + disconnected,
            total: count("total")? + disconnected,
        };
        (summary.total <= 1000
            && summary.running + summary.waiting + summary.unknown <= summary.total)
            .then_some(summary)
    }
    pub fn label(self) -> String {
        if !self.available {
            return "Agents · unavailable".into();
        }
        if self.unknown > 0 {
            format!(
                "Agents {} run · {} wait · {} unknown",
                self.running, self.waiting, self.unknown
            )
        } else {
            format!("Agents {} run · {} wait", self.running, self.waiting)
        }
    }
}

impl crate::presentation::Scene {
    /// Reuse the existing title region so the summary cannot overlap status
    /// controls. The hit route is the same canonical picker used by sidebars.
    pub fn add_agent_summary(&mut self, summary: AgentSummary, theme: crate::ThemeMode) {
        use crate::presentation::{
            HitRegion, NodeId, Palette, Rect, SceneNode, TextAlign, VisualState,
        };
        if summary.available && summary.total == 0 {
            return;
        }
        let mut target = None;
        for node in &mut self.nodes {
            if let SceneNode::Text {
                id: NodeId::Client,
                text,
                bounds,
                ..
            } = node
            {
                *text = format!("{} · {}", summary.label(), text);
                target = Some(*bounds);
                break;
            }
        }
        if target.is_none() && self.viewport.width >= 640.0 {
            let bounds = Rect::new(
                self.viewport.width * 0.25,
                4.0,
                self.viewport.width * 0.25,
                (self.viewport.height - 8.0).max(1.0),
            );
            self.nodes.push(SceneNode::Text {
                id: NodeId::ShellHub(crate::ShellRoute::AgentSessions),
                bounds,
                text: summary.label(),
                size: 12.0,
                color: Palette::for_theme(theme).text,
                align: TextAlign::Start,
                state: VisualState::default(),
            });
            target = Some(bounds);
        }
        if let Some(bounds) = target.filter(|_| summary.picker_available) {
            self.hits.push(HitRegion {
                id: NodeId::ShellHub(crate::ShellRoute::AgentSessions),
                bounds,
                primary: Some(crate::UserAction::OpenShellHub(
                    crate::ShellRoute::AgentSessions,
                )),
                secondary: None,
                scroll_up: None,
                scroll_down: None,
            });
        }
    }
}

pub struct AgentWatcher {
    refresh: SyncSender<()>,
    latest: Arc<Mutex<Option<AgentSummary>>>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl AgentWatcher {
    pub fn start() -> Self {
        let (refresh, requests) = mpsc::sync_channel(1);
        let latest = Arc::new(Mutex::new(None));
        let output = Arc::clone(&latest);
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let worker = std::thread::Builder::new()
            .name("agent-summary".into())
            .spawn(move || {
                let mut previous = None;
                while requests.recv().is_ok() {
                    if stopped.load(Ordering::Acquire) {
                        break;
                    }
                    let summary = fetch("get_agent_summary", 32 * 1024)
                        .or_else(|| fetch("get_agent_sessions", 1024 * 1024))
                        .unwrap_or_default();
                    if previous.is_none_or(|previous: AgentSummary| {
                        (
                            previous.available,
                            previous.picker_available,
                            previous.running,
                            previous.waiting,
                            previous.unknown,
                            previous.total,
                        ) != (
                            summary.available,
                            summary.picker_available,
                            summary.running,
                            summary.waiting,
                            summary.unknown,
                            summary.total,
                        )
                    }) {
                        *output.lock().unwrap_or_else(|p| p.into_inner()) = Some(summary);
                        previous = Some(summary);
                    }
                }
            })
            .ok();
        let watcher = Self {
            refresh,
            latest,
            stop,
            worker,
        };
        watcher.request_refresh();
        watcher
    }
    pub fn request_refresh(&self) {
        let _ = self.refresh.try_send(());
    }
    pub fn take(&self) -> Option<AgentSummary> {
        match self.latest.try_lock() {
            Ok(mut value) => value.take(),
            Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner().take(),
            Err(_) => None,
        }
    }
}
impl Drop for AgentWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.refresh.try_send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn fetch(query: &str, limit: usize) -> Option<AgentSummary> {
    let path = std::env::var_os("JWM_SOCKET")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("XDG_RUNTIME_DIR")
                .map(std::path::PathBuf::from)
                .filter(|path| path.is_absolute())
                .unwrap_or_else(|| {
                    std::path::PathBuf::from(format!("/tmp/jwm-{}", unsafe { libc::geteuid() }))
                })
                .join("jwm-ipc.sock")
        });
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut stream = jwm_ipc_transport::connect(&path, Duration::from_millis(500)).ok()?;
    jwm_ipc_transport::verify_same_user(&stream).ok()?;
    let mut request = serde_json::to_vec(&serde_json::json!({"query":query})).ok()?;
    request.push(b'\n');
    jwm_ipc_transport::write_all_with_timeout(
        &mut stream,
        &request,
        deadline.saturating_duration_since(Instant::now()),
    )
    .ok()?;
    let frame =
        jwm_ipc_transport::read_frame_until(&mut std::io::BufReader::new(stream), limit, deadline)
            .ok()?;
    let response: Value = serde_json::from_slice(&frame).ok()?;
    if response["success"].as_bool() != Some(true) {
        return None;
    }
    AgentSummary::parse(response.get("data")?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn counts_require_registered_states_and_never_parse_titles() {
        let value = serde_json::json!({"protocol_version":1,"revision":4,"sessions":[{"title":"running","status":"waiting"},{"status":"running"},{"status":"disconnected"}]});
        let summary = AgentSummary::parse(&value).unwrap();
        assert_eq!(
            (summary.running, summary.waiting, summary.unknown),
            (1, 1, 1)
        );
        assert!(summary.label().contains("1 wait"));
        assert!(AgentSummary::parse(&serde_json::json!({"protocol_version":1,"revision":0,"running":5,"waiting":2,"unknown":0,"total":1})).is_none());
    }
    #[test]
    fn title_summary_uses_the_existing_slot_and_a_stable_session_route() {
        use crate::presentation::*;
        let bounds = Rect::new(100.0, 0.0, 200.0, 40.0);
        let mut scene = Scene {
            viewport: Size::new(800.0, 40.0),
            clip: Rect::new(0.0, 0.0, 800.0, 40.0),
            nodes: vec![SceneNode::Text {
                id: NodeId::Client,
                bounds,
                text: "project".into(),
                size: 13.0,
                color: Palette::for_theme(crate::ThemeMode::Light).text,
                align: TextAlign::Start,
                state: VisualState::default(),
            }],
            hits: Vec::new(),
        };
        scene.add_agent_summary(
            AgentSummary {
                available: true,
                picker_available: true,
                running: 2,
                waiting: 1,
                total: 3,
                ..Default::default()
            },
            crate::ThemeMode::Light,
        );
        assert_eq!(scene.nodes[0].bounds(), bounds);
        assert!(
            matches!(&scene.nodes[0],SceneNode::Text{text,..} if text.contains("2 run") && text.contains("1 wait"))
        );
        assert_eq!(scene.hits[0].bounds, bounds);
        assert_eq!(
            scene.hits[0].primary,
            Some(crate::UserAction::OpenShellHub(
                crate::ShellRoute::AgentSessions
            ))
        );
    }
}
