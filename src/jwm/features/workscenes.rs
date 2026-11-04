//! Named, previewed desktop scenes. Layout restoration and application/session
//! restoration are separate explicit actions. No scene stores or matches a PID.
use super::system_ui::{ListKind, ListRow, RowData, SystemUiState};
use crate::backend::api::Backend;
use crate::config::{CONFIG, Config};
use crate::core::layout::LayoutEnum;
use crate::core::models::MonitorKey;
use crate::jwm::Jwm;
use crate::jwm::types::WMArgEnum;
use serde::{Deserialize, Serialize};
use std::rc::Rc;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SceneOutput {
    /// EDID stable key or connector. Empty means the output selected at preview time.
    #[serde(default)]
    pub output: String,
    /// One-based workspace number, independent of monitor enumeration.
    pub tag: u8,
    #[serde(default)]
    pub layout: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Workscene {
    pub name: String,
    #[serde(default)]
    pub outputs: Vec<SceneOutput>,
    #[serde(default)]
    pub sessions: Vec<String>,
    /// Exact desktop catalog entry names. Each missing app requires its own click.
    #[serde(default)]
    pub applications: Vec<String>,
}

pub fn defaults() -> Vec<Workscene> {
    ["JWM 开发", "机器人训练", "沟通与文档", "专注"]
        .into_iter()
        .enumerate()
        .map(|(index, name)| Workscene {
            name: name.into(),
            outputs: vec![SceneOutput {
                output: String::new(),
                tag: index as u8 + 1,
                layout: String::new(),
            }],
            sessions: Vec::new(),
            applications: Vec::new(),
        })
        .collect()
}

fn app_identity_matches(class: &str, instance: &str, name: &str, program: &str) -> bool {
    [class, instance].iter().any(|identity| {
        identity.eq_ignore_ascii_case(name)
            || (!program.is_empty()
                && (identity.eq_ignore_ascii_case(program)
                    || identity
                        .rsplit('.')
                        .next()
                        .is_some_and(|tail| tail.eq_ignore_ascii_case(program))))
    })
}

pub(crate) struct WorkscenePlan {
    config: Arc<Config>,
    outputs: Vec<(MonitorKey, u32, Option<LayoutEnum>)>,
    sessions: Vec<super::launcher::AgentEntry>,
    applications: Vec<super::system_ui::LaunchEntry>,
}
impl std::fmt::Debug for WorkscenePlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkscenePlan")
            .field("outputs", &self.outputs)
            .finish_non_exhaustive()
    }
}

impl Jwm {
    pub(crate) fn open_workscene_preview(
        &mut self,
        backend: &mut dyn Backend,
        scene: Workscene,
    ) -> Result<(), String> {
        if scene.name.is_empty()
            || scene.name.len() > 256
            || scene.outputs.len() > 64
            || scene.sessions.len() > 128
            || scene.applications.len() > 128
        {
            return Err("Scene exceeds its identity or size limits".into());
        }
        let config = CONFIG.load_full();
        let mut outputs = Vec::new();
        let mut details = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for target in &scene.outputs {
            let monitor = if target.output.is_empty() {
                self.state.sel_mon.ok_or("No selected output")?
            } else {
                let candidates: Vec<_> = self
                    .state
                    .monitor_order
                    .iter()
                    .copied()
                    .filter(|key| {
                        self.output_key_for_monitor(backend, *key).as_deref()
                            == Some(&target.output)
                            || self.output_connector_for_monitor(backend, *key).as_deref()
                                == Some(&target.output)
                    })
                    .collect();
                if candidates.len() != 1 {
                    return Err(format!("Output {} is absent or ambiguous", target.output));
                }
                candidates[0]
            };
            if !seen.insert(monitor) {
                return Err("A scene cannot assign two workspaces to one output".into());
            }
            let tag = usize::from(target.tag)
                .checked_sub(1)
                .filter(|tag| *tag < config.tags_length())
                .ok_or("Scene workspace no longer exists")?;
            let mask = 1u32
                .checked_shl(tag as u32)
                .ok_or("Scene workspace is out of range")?;
            let layout = if target.layout.is_empty() {
                None
            } else {
                Some(
                    LayoutEnum::from_name(&target.layout)
                        .ok_or("Unknown scene layout")?
                        .clone(),
                )
            };
            let number = self.state.monitors[monitor].num;
            details.push(format!(
                "Display {} → Tag {}{}",
                number + 1,
                target.tag,
                layout
                    .as_ref()
                    .map(|layout| format!(" / {}", layout.label()))
                    .unwrap_or_default()
            ));
            outputs.push((monitor, mask, layout));
        }
        let snapshot = self.agent_sessions_snapshot();
        let registered = super::launcher::AgentEntry::from_snapshot(&snapshot);
        let mut sessions = Vec::new();
        let mut applications = Vec::new();
        let mut rows = vec![ListRow {
            key: "layout".into(),
            text: format!("Apply workspace layout · {}", details.join("; ")),
            data: RowData::Workscene,
        }];
        for id in &scene.sessions {
            if let Some(entry) = registered.iter().find(|entry| &entry.session_id == id) {
                let index = sessions.len();
                rows.push(ListRow {
                    key: format!("session:{index}"),
                    text: format!("Reveal {}", entry.row()),
                    data: RowData::Workscene,
                });
                sessions.push(entry.clone());
            } else {
                rows.push(ListRow {
                    key: "missing".into(),
                    text: format!("Unavailable session {id} · provider has not registered it"),
                    data: RowData::Workscene,
                });
            }
        }
        for name in &scene.applications {
            let program = self
                .features
                .launcher_catalog
                .iter()
                .find(|entry| &entry.name == name)
                .and_then(|entry| entry.command.first())
                .and_then(|program| std::path::Path::new(program).file_name())
                .and_then(|program| program.to_str())
                .unwrap_or("");
            if self
                .state
                .clients
                .values()
                .any(|client| app_identity_matches(&client.class, &client.instance, name, program))
            {
                rows.push(ListRow {
                    key: "present".into(),
                    text: format!("Already open: {name} · process preserved"),
                    data: RowData::Workscene,
                });
                continue;
            }
            if let Some(entry) = self
                .features
                .launcher_catalog
                .iter()
                .find(|entry| &entry.name == name)
            {
                let index = applications.len();
                rows.push(ListRow {
                    key: format!("application:{index}"),
                    text: format!("Start missing application: {name} · only when selected"),
                    data: RowData::Workscene,
                });
                applications.push(entry.clone());
            } else {
                rows.push(ListRow {
                    key: "missing".into(),
                    text: format!("Missing catalog entry: {name} · refresh applications first"),
                    data: RowData::Workscene,
                });
            }
        }
        self.features.workscene_preview = Some(WorkscenePlan {
            config,
            outputs,
            sessions,
            applications,
        });
        self.features.system_ui = SystemUiState::ListPanel {
            list_scroll: None,
            kind: ListKind::Workscene,
            rows,
            row_icons: Vec::new(),
            source_rows: None,
            selected: 0,
            message: format!(
                "{} · preview only · choose each action; Esc cancels",
                scene.name
            ),
            prompt: None,
            query: String::new(),
            empty: "Empty scene".into(),
        };
        self.sync_system_ui(backend);
        Ok(())
    }

    pub(crate) fn perform_workscene_selection(&mut self, backend: &mut dyn Backend) {
        let result = self.activate_workscene_action(backend);
        let body = match result {
            Ok(()) => {
                "Selected action dispatched; provider sessions require their own reveal receipt"
                    .into()
            }
            Err(error) => error,
        };
        backend.compositor_push_toast(crate::backend::api::ToastNotification {
            title: "Work scene".into(),
            body,
            app: "JWM".into(),
            ..Default::default()
        });
    }

    pub(crate) fn activate_workscene_action(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), String> {
        let action = self
            .features
            .system_ui
            .selected_workscene_action()
            .ok_or("No scene action")?
            .to_owned();
        let mut plan = self
            .features
            .workscene_preview
            .take()
            .ok_or("Scene preview expired")?;
        let result = (|| {
            if !Arc::ptr_eq(&plan.config, &CONFIG.load_full())
                || plan
                    .outputs
                    .iter()
                    .any(|(key, _, _)| !self.state.monitors.contains_key(*key))
            {
                return Err("Scene changed or an output disconnected; open a new preview".into());
            }
            if action == "layout" {
                if plan
                    .outputs
                    .iter()
                    .any(|(key, _, _)| self.monitor_key_is_locked(*key))
                {
                    return Err("Unlock scene outputs first".into());
                }
                for (key, mask, layout) in &plan.outputs {
                    self.switch_to_monitor(backend, *key)
                        .map_err(|e| e.to_string())?;
                    self.view(backend, &WMArgEnum::UInt(*mask))
                        .map_err(|e| e.to_string())?;
                    if let Some(layout) = layout {
                        self.setlayout(backend, &WMArgEnum::Layout(Rc::new(layout.clone())))
                            .map_err(|e| e.to_string())?;
                    }
                }
            } else if let Some(index) = action
                .strip_prefix("session:")
                .and_then(|index| index.parse::<usize>().ok())
            {
                let entry = plan.sessions.get(index).ok_or("Session preview expired")?;
                self.activate_agent_session(backend, &entry.session_id, entry.revision)?;
            } else if let Some(index) = action
                .strip_prefix("application:")
                .and_then(|index| index.parse::<usize>().ok())
            {
                let entry = plan
                    .applications
                    .get_mut(index)
                    .ok_or("Application preview expired")?;
                if entry.command.is_empty() {
                    return Err("Application already requested; awaiting its window".into());
                }
                let program = entry
                    .command
                    .first()
                    .and_then(|program| std::path::Path::new(program).file_name())
                    .and_then(|program| program.to_str())
                    .unwrap_or("");
                if self.state.clients.values().any(|client| {
                    app_identity_matches(&client.class, &client.instance, &entry.name, program)
                }) {
                    return Err(
                        "Application opened since preview; use window search to return to it"
                            .into(),
                    );
                }
                let command = super::launcher::launch_command(
                    &Config::get_terminal_exec_prefix(),
                    &entry.command,
                    entry.terminal,
                );
                self.spawn(backend, &WMArgEnum::StringVec(command))
                    .map_err(|e| e.to_string())?;
                // An accepted spawn is single-use even before the new window appears.
                entry.command.clear();
            } else {
                return Err("This object is unavailable; no action was dispatched".into());
            }
            Ok(())
        })();
        self.features.workscene_preview = Some(plan);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scene_preview_has_no_side_effects_and_disconnected_targets_are_rejected() {
        use crate::jwm::monitor::test_support::{DisplaySpyBackend, output};
        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.sel_mon.unwrap();
        let scene = Workscene {
            name: "Preview test".into(),
            outputs: vec![SceneOutput {
                output: String::new(),
                tag: 2,
                layout: String::new(),
            }],
            sessions: Vec::new(),
            applications: Vec::new(),
        };
        jwm.open_workscene_preview(&mut backend, scene.clone())
            .unwrap();
        assert_eq!(jwm.state.monitors[monitor].get_active_tags(), 1);
        jwm.activate_workscene_action(&mut backend).unwrap();
        assert_eq!(jwm.state.monitors[monitor].get_active_tags(), 2);
        jwm.open_workscene_preview(&mut backend, scene).unwrap();
        jwm.state.monitors.remove(monitor);
        assert!(
            jwm.activate_workscene_action(&mut backend)
                .unwrap_err()
                .contains("disconnected")
        );
    }

    #[test]
    fn matching_application_identity_avoids_repeat_terminal_spawns() {
        assert!(app_identity_matches(
            "org.jwm.Forge",
            "forge",
            "Forge",
            "forge"
        ));
        assert!(app_identity_matches(
            "org.mozilla.firefox",
            "Navigator",
            "Firefox Web Browser",
            "firefox"
        ));
        assert!(!app_identity_matches(
            "browser", "browser", "Forge", "forge"
        ));
    }

    #[test]
    fn scene_schema_contains_stable_references_and_no_process_identity() {
        let scenes = defaults();
        assert_eq!(scenes.len(), 4);
        let json = serde_json::to_value(&scenes).unwrap();
        assert!(!json.to_string().contains("pid"));
        assert_eq!(json[0]["outputs"][0]["tag"], 1);
        assert!(serde_json::from_value::<Vec<Workscene>>(json).is_ok());
    }
}
