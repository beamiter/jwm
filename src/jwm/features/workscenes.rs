//! Named, previewed desktop scenes. Layout restoration and application/session
//! restoration are separate explicit actions. No scene stores or matches a PID.
use super::system_ui::{LaunchEntry, ListKind, ListRow, RowData, SystemUiState};
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

fn declared_identity_matches(
    class: &str,
    instance: &str,
    desktop_id: Option<&str>,
    startup_wm_class: Option<&str>,
) -> bool {
    // JWM's X11 property layer normalizes WM_CLASS to lowercase. Compare
    // the whole declared identity, not a case-sensitive form or a suffix.
    [class, instance].iter().any(|identity| {
        [desktop_id, startup_wm_class]
            .into_iter()
            .flatten()
            .any(|declared| identity.eq_ignore_ascii_case(declared))
    })
}

fn catalog_identity_matches(class: &str, instance: &str, entry: &LaunchEntry) -> bool {
    declared_identity_matches(
        class,
        instance,
        entry.desktop_id.as_deref(),
        entry.startup_wm_class.as_deref(),
    ) || app_identity_matches(
        class,
        instance,
        &entry.name,
        entry
            .command
            .first()
            .and_then(|program| std::path::Path::new(program).file_name())
            .and_then(|program| program.to_str())
            .unwrap_or(""),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorksceneOutcome {
    LayoutApplied,
    SessionRevealRequested,
    ApplicationLaunchRequested,
}

impl WorksceneOutcome {
    fn message(self) -> &'static str {
        match self {
            Self::LayoutApplied => "Workspace layout applied",
            Self::SessionRevealRequested => {
                "Terminal focused; session reveal requested, awaiting provider confirmation"
            }
            Self::ApplicationLaunchRequested => "Application launch requested",
        }
    }
}

pub(crate) struct WorkscenePlan {
    config: Arc<Config>,
    outputs: Vec<(MonitorKey, u32, Option<LayoutEnum>)>,
    sessions: Vec<super::launcher::AgentEntry>,
    applications: Vec<LaunchEntry>,
    pending_applications: Vec<(usize, String)>,
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
        let mut pending_applications = Vec::new();
        let indexing = self.features.launcher_catalog_refreshed_at.is_none()
            && super::connectivity::job_in_flight(self.features.launcher_catalog_job.as_ref());
        let mut rows = Vec::new();
        if !outputs.is_empty() {
            rows.push(ListRow {
                key: "layout".into(),
                text: format!("Apply workspace layout · {}", details.join("; ")),
                data: RowData::Workscene,
            });
        }
        // Preserve the first occurrence. A repeated entry in configuration is
        // not a second explicit request to reveal the same session.
        let mut seen_sessions = std::collections::HashSet::new();
        for id in &scene.sessions {
            if !seen_sessions.insert(id.as_str()) {
                continue;
            }
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
        // In particular, duplicates must share the same single-use launch row
        // even before the application's first window becomes visible.
        let mut seen_applications = std::collections::HashSet::new();
        for name in &scene.applications {
            if !seen_applications.insert(name.as_str()) {
                continue;
            }
            if indexing {
                let index = rows.len();
                rows.push(ListRow {
                    key: "indexing".into(),
                    text: format!("Looking up {name} · indexing applications"),
                    data: RowData::Workscene,
                });
                pending_applications.push((index, name.clone()));
            } else {
                rows.push(self.workscene_application_row(name, &mut applications));
            }
        }
        let selected = rows
            .iter()
            .position(|row| {
                row.key == "layout"
                    || row.key.starts_with("session:")
                    || row.key.starts_with("application:")
            })
            .unwrap_or(0);
        self.features.workscene_preview = Some(WorkscenePlan {
            config,
            outputs,
            sessions,
            applications,
            pending_applications,
        });
        self.features.system_ui = SystemUiState::ListPanel {
            list_scroll: None,
            kind: ListKind::Workscene,
            rows,
            row_icons: Vec::new(),
            source_rows: None,
            selected,
            message: format!(
                "{} · preview only · choose each action; Esc cancels",
                scene.name
            ),
            prompt: None,
            query: String::new(),
            empty: "No actions configured for this scene".into(),
        };
        self.sync_system_ui(backend);
        Ok(())
    }

    fn workscene_application_row(
        &self,
        name: &str,
        applications: &mut Vec<LaunchEntry>,
    ) -> ListRow {
        let entry = self
            .features
            .launcher_catalog
            .iter()
            .find(|entry| entry.name == name);
        if self.state.clients.values().any(|client| {
            entry.map_or_else(
                || app_identity_matches(&client.class, &client.instance, name, ""),
                |entry| catalog_identity_matches(&client.class, &client.instance, entry),
            )
        }) {
            return ListRow {
                key: "present".into(),
                text: format!("Already open: {name} · process preserved"),
                data: RowData::Workscene,
            };
        }
        if let Some(entry) = entry {
            let index = applications.len();
            applications.push(entry.clone());
            ListRow {
                key: format!("application:{index}"),
                text: format!("Start {name} · normal window placement"),
                data: RowData::Workscene,
            }
        } else {
            ListRow {
                key: "missing".into(),
                text: format!("Missing catalog entry: {name} · refresh applications first"),
                data: RowData::Workscene,
            }
        }
    }

    pub(crate) fn resolve_pending_workscene_applications(&mut self) -> bool {
        if !self.features.system_ui.is_workscene_preview() {
            return false;
        }
        let Some(mut plan) = self.features.workscene_preview.take() else {
            return false;
        };
        if !Arc::ptr_eq(&plan.config, &CONFIG.load_full()) || plan.pending_applications.is_empty() {
            self.features.workscene_preview = Some(plan);
            return false;
        }
        // Resolve just the rows that could not be classified before the
        // initial scan. Keep the panel, existing actions and selection intact.
        let replacements: Vec<_> = std::mem::take(&mut plan.pending_applications)
            .into_iter()
            .map(|(index, name)| {
                let row = self.workscene_application_row(&name, &mut plan.applications);
                (index, row)
            })
            .collect();
        self.features.workscene_preview = Some(plan);
        let SystemUiState::ListPanel { rows, .. } = &mut self.features.system_ui else {
            return false;
        };
        let mut changed = false;
        for (index, row) in replacements {
            if let Some(pending) = rows.get_mut(index)
                && pending.key == "indexing"
            {
                *pending = row;
                changed = true;
            }
        }
        changed
    }

    pub(crate) fn perform_workscene_selection(&mut self, backend: &mut dyn Backend) {
        let result = self.activate_workscene_action(backend);
        let body = match result {
            Ok(outcome) => outcome.message().into(),
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
    ) -> Result<WorksceneOutcome, String> {
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
                if plan.outputs.is_empty() {
                    return Err("No workspace layout in this scene".into());
                }
                if plan
                    .outputs
                    .iter()
                    .any(|(key, _, _)| self.monitor_key_is_locked(*key))
                {
                    return Err("Unlock scene outputs first".into());
                }
                for (key, mask, layout) in &plan.outputs {
                    self.view_on_monitor(backend, *key, *mask)
                        .map_err(|e| e.to_string())?;
                    if let Some(layout) = layout {
                        self.set_layout_on_monitor(backend, *key, Rc::new(layout.clone()))
                            .map_err(|e| e.to_string())?;
                    }
                }
                Ok(WorksceneOutcome::LayoutApplied)
            } else if let Some(index) = action
                .strip_prefix("session:")
                .and_then(|index| index.parse::<usize>().ok())
            {
                let entry = plan.sessions.get(index).ok_or("Session preview expired")?;
                self.activate_scene_agent_session(backend, entry)?;
                Ok(WorksceneOutcome::SessionRevealRequested)
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
                if self
                    .state
                    .clients
                    .values()
                    .any(|client| catalog_identity_matches(&client.class, &client.instance, entry))
                {
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
                Ok(WorksceneOutcome::ApplicationLaunchRequested)
            } else if action == "indexing" {
                Err("Still indexing applications; wait for the catalog to load".into())
            } else {
                Err("This object is unavailable; no action was dispatched".into())
            }
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
        assert_eq!(
            jwm.activate_workscene_action(&mut backend).unwrap(),
            WorksceneOutcome::LayoutApplied
        );
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
    fn scene_without_outputs_selects_the_first_action_and_cannot_apply_a_layout() {
        use crate::jwm::monitor::test_support::{DisplaySpyBackend, output};

        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.sel_mon.unwrap();
        let notes = LaunchEntry::new(
            "JWM Notes".into(),
            vec!["jwm-notes".into()],
            false,
            None,
            "jwm notes".into(),
        );
        jwm.features.launcher_catalog = Arc::from([notes]);
        let mut scene = Workscene {
            name: "Notes only".into(),
            outputs: Vec::new(),
            sessions: Vec::new(),
            applications: vec!["Missing catalog entry".into(), "JWM Notes".into()],
        };

        jwm.open_workscene_preview(&mut backend, scene.clone())
            .unwrap();
        let SystemUiState::ListPanel { rows, .. } = &jwm.features.system_ui else {
            panic!("expected workscene preview");
        };
        assert!(rows.iter().all(|row| row.key != "layout"));
        assert_eq!(
            jwm.features.system_ui.selected_workscene_action(),
            Some("application:0")
        );

        scene.applications.clear();
        jwm.open_workscene_preview(&mut backend, scene).unwrap();
        assert_eq!(jwm.features.system_ui.selected_workscene_action(), None);
        assert_eq!(
            jwm.activate_workscene_action(&mut backend).unwrap_err(),
            "No scene action"
        );
        assert_eq!(jwm.state.monitors[monitor].get_active_tags(), 1);
    }

    #[test]
    fn scene_lists_each_exact_reference_once_in_first_occurrence_order() {
        use crate::jwm::monitor::test_support::{DisplaySpyBackend, output};

        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let entry = |name: &str, program: &str| {
            LaunchEntry::new(name.into(), vec![program.into()], false, None, name.into())
        };
        jwm.features.launcher_catalog = Arc::from([
            entry("JWM Notes", "jwm-notes"),
            entry("Terminal", "jwm-test-terminal"),
        ]);
        let scene = Workscene {
            name: "Repeated references".into(),
            outputs: Vec::new(),
            sessions: vec!["first".into(), "second".into(), "first".into()],
            applications: vec![
                "Terminal".into(),
                "JWM Notes".into(),
                "Terminal".into(),
                "JWM Notes".into(),
            ],
        };
        jwm.open_workscene_preview(&mut backend, scene).unwrap();
        let SystemUiState::ListPanel { rows, selected, .. } = &jwm.features.system_ui else {
            panic!("expected workscene preview");
        };
        assert_eq!(rows.len(), 4);
        assert!(rows[0].text.starts_with("Unavailable session first ·"));
        assert!(rows[1].text.starts_with("Unavailable session second ·"));
        assert_eq!(rows[2].key, "application:0");
        assert_eq!(rows[3].key, "application:1");
        assert_eq!(*selected, 2);
        let plan = jwm.features.workscene_preview.as_ref().unwrap();
        assert_eq!(plan.applications.len(), 2);
        assert_eq!(plan.applications[0].name, "Terminal");
        assert_eq!(plan.applications[1].name, "JWM Notes");
    }

    #[test]
    fn cold_scene_catalog_updates_pending_rows_after_worker_publication() {
        use crate::jwm::features::connectivity::BackgroundJob;
        use crate::jwm::monitor::test_support::{DisplaySpyBackend, output};

        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let (job, publish) = BackgroundJob::controlled_for_test();
        jwm.features.launcher_catalog_job = Some(job);
        let scene = Workscene {
            name: "Cold catalog".into(),
            outputs: Vec::new(),
            sessions: Vec::new(),
            applications: vec!["JWM Notes".into(), "Not installed".into()],
        };
        jwm.open_workscene_preview(&mut backend, scene).unwrap();
        jwm.poll_launcher_catalog_job();
        let SystemUiState::ListPanel { rows, selected, .. } = &jwm.features.system_ui else {
            panic!("expected workscene preview");
        };
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.key == "indexing"));
        assert_eq!(*selected, 0);
        assert_eq!(
            jwm.activate_workscene_action(&mut backend).unwrap_err(),
            "Still indexing applications; wait for the catalog to load"
        );

        publish(Arc::from([LaunchEntry::new(
            "JWM Notes".into(),
            vec!["jwm-notes".into()],
            false,
            None,
            "jwm notes".into(),
        )]));
        jwm.poll_launcher_catalog_job();
        assert!(jwm.features.launcher_catalog_refreshed_at.is_some());
        assert_eq!(
            jwm.features.system_ui.selected_workscene_action(),
            Some("application:0")
        );
        let SystemUiState::ListPanel { rows, .. } = &jwm.features.system_ui else {
            panic!("expected workscene preview");
        };
        assert_eq!(rows.len(), 2);
        assert!(rows[0].text.starts_with("Start JWM Notes"));
        assert_eq!(rows[1].key, "missing");
        assert!(
            rows[1]
                .text
                .starts_with("Missing catalog entry: Not installed")
        );
        let plan = jwm.features.workscene_preview.as_ref().unwrap();
        assert!(plan.pending_applications.is_empty());
        assert_eq!(plan.applications.len(), 1);
    }

    #[test]
    fn scene_outcomes_distinguish_local_success_from_requested_actions() {
        assert_eq!(
            WorksceneOutcome::LayoutApplied.message(),
            "Workspace layout applied"
        );
        assert_eq!(
            WorksceneOutcome::ApplicationLaunchRequested.message(),
            "Application launch requested"
        );
        assert!(
            WorksceneOutcome::SessionRevealRequested
                .message()
                .contains("awaiting provider confirmation")
        );
    }

    #[test]
    fn scene_layout_keeps_the_active_screen_and_uses_its_visible_fallback() {
        use crate::backend::common_define::WindowId;
        use crate::core::models::WMClient;
        use crate::jwm::monitor::test_support::{DisplaySpyBackend, output};

        // The output order should not determine where the next key goes.
        for reverse in [false, true] {
            let mut backend = DisplaySpyBackend::new(vec![
                output(1, 0, 0, 1920, 1080),
                output(2, 1920, 0, 1280, 800),
            ]);
            let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
            let active = jwm.state.sel_mon.unwrap();
            let background = *jwm
                .state
                .monitor_order
                .iter()
                .find(|&&m| m != active)
                .unwrap();
            let mut add_window = |id, monitor, tags| {
                let mut client = WMClient::new(WindowId::from_raw(id));
                client.mon = Some(monitor);
                client.state.tags = tags;
                let key = jwm.insert_client(client);
                jwm.attach_to_monitor(key, monitor);
                key
            };
            let old = add_window(20, active, 1);
            let visible = add_window(21, active, 2);
            let other = add_window(22, background, 4);
            jwm.focus(&mut backend, Some(old)).unwrap();
            assert_eq!(jwm.state.monitors[active].sel, Some(old));
            let mut outputs = vec![
                SceneOutput {
                    output: jwm.output_connector_for_monitor(&backend, active).unwrap(),
                    tag: 2,
                    layout: "float".into(),
                },
                SceneOutput {
                    output: jwm
                        .output_connector_for_monitor(&backend, background)
                        .unwrap(),
                    tag: 3,
                    layout: "monocle".into(),
                },
            ];
            if reverse {
                outputs.reverse();
            }
            let scene = Workscene {
                name: "Preserve focus".into(),
                outputs,
                sessions: Vec::new(),
                applications: Vec::new(),
            };
            let expected = Some(jwm.state.clients[visible].win);
            let never = Some(jwm.state.clients[other].win);
            for reapply in [false, true] {
                jwm.open_workscene_preview(&mut backend, scene.clone())
                    .unwrap();
                backend.focus_requests.clear();
                jwm.activate_workscene_action(&mut backend).unwrap();
                assert_eq!(jwm.state.sel_mon, Some(active));
                assert_eq!(jwm.state.monitors[active].sel, Some(visible));
                assert_eq!(jwm.state.monitors[active].get_active_tags(), 2);
                assert_eq!(jwm.state.monitors[background].get_active_tags(), 4);
                assert_eq!(*jwm.state.monitors[active].lt, LayoutEnum::FLOAT);
                assert_eq!(*jwm.state.monitors[background].lt, LayoutEnum::MONOCLE);
                assert!(!backend.focus_requests.contains(&never));
                if reapply {
                    assert!(backend.focus_requests.is_empty());
                } else {
                    assert!(backend.focus_requests.contains(&expected));
                }
            }
        }
    }

    #[test]
    fn scene_rechecks_the_exact_application_id_at_activation() {
        use crate::backend::common_define::WindowId;
        use crate::core::models::WMClient;
        use crate::jwm::monitor::test_support::{DisplaySpyBackend, output};

        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let mut catalog = LaunchEntry::new(
            "JWM Notes".into(),
            vec!["jwm-notes".into()],
            false,
            None,
            "jwm notes".into(),
        );
        catalog.desktop_id = Some("org.jwm.Notes".into());
        jwm.features.launcher_catalog = Arc::from([catalog]);
        let scene = Workscene {
            name: "Notebook test scene".into(),
            outputs: vec![SceneOutput {
                output: String::new(),
                tag: 1,
                layout: String::new(),
            }],
            sessions: Vec::new(),
            applications: vec!["JWM Notes".into()],
        };

        let mut lookalike = WMClient::new(WindowId::from_raw(11));
        lookalike.class = "org.unrelated.Notes".into();
        jwm.state.clients.insert(lookalike);
        jwm.open_workscene_preview(&mut backend, scene.clone())
            .unwrap();
        let SystemUiState::ListPanel { rows, selected, .. } = &mut jwm.features.system_ui else {
            panic!("expected workscene preview");
        };
        *selected = rows
            .iter()
            .position(|row| row.key == "application:0")
            .unwrap();

        let mut notes = WMClient::new(WindowId::from_raw(12));
        notes.class = "org.jwm.notes".into();
        jwm.state.clients.insert(notes);
        assert!(
            jwm.activate_workscene_action(&mut backend)
                .unwrap_err()
                .contains("Application opened since preview")
        );

        jwm.open_workscene_preview(&mut backend, scene).unwrap();
        let SystemUiState::ListPanel { rows, .. } = &jwm.features.system_ui else {
            panic!("expected workscene preview");
        };
        assert!(
            rows.iter()
                .any(|row| row.key == "present" && row.text.contains("JWM Notes"))
        );
        assert!(!rows.iter().any(|row| row.key.starts_with("application:")));
    }

    #[test]
    fn exact_desktop_identity_matches_notes_without_a_fuzzy_dotted_suffix() {
        let notes_id = Some("org.jwm.Notes");
        assert!(!app_identity_matches(
            "org.jwm.Notes",
            "",
            "JWM Notes",
            "jwm-notes"
        ));
        assert!(declared_identity_matches(
            "org.jwm.notes",
            "",
            notes_id,
            None
        ));
        assert!(declared_identity_matches(
            "LegacyClass",
            "org.jwm.notes",
            None,
            notes_id
        ));
        assert!(!declared_identity_matches(
            "org.unrelated.Notes",
            "Notes",
            notes_id,
            notes_id
        ));
        assert!(!declared_identity_matches("", "", None, None));
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
