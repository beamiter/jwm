//! Source picker.
//!
//! Lookup order:
//! 1. `JWM_PORTAL_OUTPUT=<name|substring of description>` env override.
//!    `JWM_PORTAL_WINDOW=class:<app_id>` or `title:<substring>`.
//! 2. `JWM_PORTAL_PICKER=rofi|wofi|<custom>` to spawn an external picker
//!    that reads a `\n`-delimited list on stdin and writes the chosen
//!    line(s) on stdout. `auto` picks rofi if found, otherwise wofi.
//! 3. With no picker override, auto-detect rofi, then wofi. If neither
//!    can be used, cancel. Never substitute an unselected source.
//!
//! The external picker is invoked with `-dmenu -p "Select source"` (rofi) or
//! `--dmenu --prompt "Select source"` (wofi). Custom binaries get the
//! literal value as argv[0] and no extra flags — wrap your own if you need
//! something exotic.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

use crate::picker_match::filter_portal_window_spec;
use crate::wayland::{OutputInfo, ToplevelInfo};

#[derive(Debug, Default, Clone)]
pub struct SourceSelection {
    pub outputs: Vec<OutputInfo>,
    pub toplevels: Vec<ToplevelInfo>,
}

/// Outcome of an interactive source-picking attempt.
///
/// `NoPicker` means no usable picker is available. Public picking entry points
/// convert it to cancellation; unattended use requires an explicit source override.
///
/// `Cancelled` means a picker was actually invoked and the user dismissed it
/// (ESC / closed the window / picked nothing). The portal layer must
/// propagate this as a real cancellation (response=1) rather than silently
/// defaulting to "share everything" — that would defeat the consent dialog.
#[derive(Debug)]
pub enum PickerOutcome<T> {
    Picked(Vec<T>),
    Cancelled,
    NoPicker,
}

/// Snapshot environment-derived policy once at the production boundary.
/// Selection tests pass values directly and never mutate process-global env.
#[derive(Debug, Clone)]
struct PickerConfig {
    output: Option<String>,
    window: Option<String>,
    picker: String,
}

impl PickerConfig {
    fn from_env() -> Self {
        Self::from_values(
            std::env::var("JWM_PORTAL_OUTPUT").ok(),
            std::env::var("JWM_PORTAL_WINDOW").ok(),
            std::env::var("JWM_PORTAL_PICKER").ok(),
        )
    }

    fn from_values(output: Option<String>, window: Option<String>, picker: Option<String>) -> Self {
        Self {
            output,
            window,
            picker: configured_picker_spec(picker),
        }
    }
}

/// Present all permitted source types in one dialog. Environment overrides are
/// explicit unattended selections; ambiguous single-source overrides cancel.
pub fn pick_sources(
    outputs: &[OutputInfo],
    toplevels: &[ToplevelInfo],
    multiple: bool,
) -> Option<SourceSelection> {
    let config = PickerConfig::from_env();
    let ipc = config
        .window
        .as_ref()
        .and_then(|_| crate::ipc::query_windows().ok());
    match pick_sources_from_config(outputs, toplevels, multiple, &config, ipc.as_deref()) {
        Ok(selection) => Some(selection),
        Err(reason) => {
            log::warn!("source selection cancelled: {reason}");
            None
        }
    }
}

fn pick_sources_from_config(
    outputs: &[OutputInfo],
    toplevels: &[ToplevelInfo],
    multiple: bool,
    config: &PickerConfig,
    ipc_windows: Option<&[crate::ipc::WindowInfo]>,
) -> Result<SourceSelection, &'static str> {
    pick_sources_with(
        outputs,
        toplevels,
        multiple,
        config.output.as_deref(),
        config.window.as_deref(),
        ipc_windows,
        |labels, multiple| run_external_picker(labels, multiple, &config.picker),
    )
}

pub fn selection_fits_request(selection: &SourceSelection, multiple: bool) -> bool {
    let count = selection.outputs.len() + selection.toplevels.len();
    count > 0 && (multiple || count == 1)
}

fn pick_sources_with(
    outputs: &[OutputInfo],
    toplevels: &[ToplevelInfo],
    multiple: bool,
    output_override: Option<&str>,
    window_override: Option<&str>,
    ipc_windows: Option<&[crate::ipc::WindowInfo]>,
    choose: impl FnOnce(&[String], bool) -> PickerOutcome<String>,
) -> Result<SourceSelection, &'static str> {
    if output_override.is_some() || window_override.is_some() {
        let mut selection = SourceSelection::default();
        if let Some(name) = output_override {
            if name.is_empty() {
                return Err("empty output override");
            }
            selection.outputs = outputs
                .iter()
                .filter(|o| {
                    o.name == name
                        || o.description.contains(name)
                        || o.connector.as_deref() == Some(name)
                })
                .cloned()
                .collect();
            if selection.outputs.is_empty() {
                return Err("output override matched no permitted output");
            }
        }
        if let Some(spec) = window_override {
            if spec.is_empty()
                || spec
                    .split_once(':')
                    .is_some_and(|(_, needle)| needle.is_empty())
            {
                return Err("empty window override");
            }
            selection.toplevels = filter_portal_window_spec(spec, toplevels, ipc_windows);
            if selection.toplevels.is_empty() {
                return Err("window override matched no permitted window");
            }
        }
        return if selection_fits_request(&selection, multiple) {
            Ok(selection)
        } else {
            Err("source overrides select multiple sources for a single-source request")
        };
    }

    let labels: Vec<String> = outputs
        .iter()
        .map(output_label)
        .chain(toplevels.iter().map(toplevel_label))
        .collect();
    if labels.is_empty() {
        return Err("no permitted sources are available");
    }
    let chosen = match choose(&labels, multiple) {
        PickerOutcome::Picked(chosen) => chosen,
        PickerOutcome::Cancelled => return Err("picker dismissed or failed"),
        PickerOutcome::NoPicker => {
            return Err("no usable picker; install rofi/wofi or configure JWM_PORTAL_PICKER");
        }
    };
    let mut selection = SourceSelection::default();
    let mut seen = std::collections::HashSet::new();
    for label in chosen {
        let Some(index) = labels.iter().position(|known| known == &label) else {
            return Err("picker returned a source it was not shown");
        };
        if !seen.insert(index) {
            continue;
        }
        if let Some(output) = outputs.get(index) {
            selection.outputs.push(output.clone());
        } else if let Some(toplevel) = toplevels.get(index - outputs.len()) {
            selection.toplevels.push(toplevel.clone());
        }
    }
    if selection_fits_request(&selection, multiple) {
        Ok(selection)
    } else {
        Err("picker did not return the requested number of sources")
    }
}

/// Format an output label; selection only accepts labels from the displayed
/// source list, so unknown or renamed sources cannot become a fallback.
fn output_label(o: &OutputInfo) -> String {
    let connector = o
        .connector
        .as_deref()
        .filter(|c| !c.is_empty() && *c != o.name.as_str());
    match (o.description.is_empty(), connector) {
        (true, None) => format!("[Monitor] {}", o.name),
        (true, Some(c)) => format!("[Monitor] {} [{c}]", o.name),
        (false, None) => format!("[Monitor] {} ({})", o.name, o.description),
        (false, Some(c)) => format!("[Monitor] {} ({}) [{c}]", o.name, o.description),
    }
}

fn toplevel_label(t: &ToplevelInfo) -> String {
    let title = if t.title.is_empty() {
        "<no title>".to_string()
    } else {
        t.title.clone()
    };
    let app = if t.app_id.is_empty() {
        "?".to_string()
    } else {
        t.app_id.clone()
    };
    // Carry the identifier so we can look it up even if title/app changes
    // between the picker exiting and us indexing back.
    format!("[Window] {app} — {title}\u{0000}{}", t.identifier)
}

fn run_external_picker(labels: &[String], multiple: bool, picker: &str) -> PickerOutcome<String> {
    let (cmd, args) = match resolve_picker(picker, multiple) {
        Some(c) => c,
        None => return PickerOutcome::NoPicker,
    };
    log::info!("picker: invoking `{cmd}` ({} options)", labels.len());

    let mut child = match Command::new(&cmd)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            // A missing or broken consent UI must never grant capture.
            log::warn!("picker: failed to spawn `{cmd}`: {e}");
            return PickerOutcome::NoPicker;
        }
    };

    let displays = picker_display_lines(labels);
    if let Some(mut stdin) = child.stdin.take() {
        for display in &displays {
            let _ = writeln!(stdin, "{display}");
        }
    }

    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            // The picker spawned but we lost track of it — treat as cancellation
            // rather than silently falling through to "share everything".
            log::warn!("picker: wait failed: {e}");
            return PickerOutcome::Cancelled;
        }
    };
    if !output.status.success() {
        log::info!("picker: user cancelled (exit {:?})", output.status.code());
        return PickerOutcome::Cancelled;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let chosen = selected_picker_labels(labels, &displays, &stdout);
    if chosen.is_empty() || (!multiple && chosen.len() != 1) {
        PickerOutcome::Cancelled
    } else {
        PickerOutcome::Picked(chosen)
    }
}

/// A generated ordinal disambiguates even equal app/title pairs. Strip
/// control characters so client-provided titles cannot inject picker rows.
fn picker_display_lines(labels: &[String]) -> Vec<String> {
    labels
        .iter()
        .enumerate()
        .map(|(index, label)| {
            let display = label.split('\u{0000}').next().unwrap_or(label);
            let display: String = display
                .chars()
                .map(|ch| if ch.is_control() { ' ' } else { ch })
                .collect();
            format!("[{}] {display}", index + 1)
        })
        .collect()
}

/// Only a byte-for-byte displayed entry can select an original source.
fn selected_picker_labels(labels: &[String], displays: &[String], stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|line| {
            displays
                .iter()
                .position(|display| display == line)
                .and_then(|index| labels.get(index))
                .cloned()
        })
        .collect()
}

fn configured_picker_spec(configured: Option<String>) -> String {
    configured.unwrap_or_else(|| "auto".to_string())
}

fn resolve_picker(spec: &str, multiple: bool) -> Option<(String, Vec<String>)> {
    resolve_picker_with(spec, multiple, |name| which(name).is_some())
}

fn resolve_picker_with(
    spec: &str,
    multiple: bool,
    available: impl Fn(&str) -> bool,
) -> Option<(String, Vec<String>)> {
    match spec {
        "" => None,
        "rofi" => Some(rofi_cmd(multiple)),
        "wofi" => Some(wofi_cmd(multiple)),
        "auto" => {
            if available("rofi") {
                Some(rofi_cmd(multiple))
            } else if available("wofi") {
                Some(wofi_cmd(multiple))
            } else {
                log::warn!("picker: JWM_PORTAL_PICKER=auto but neither rofi nor wofi found");
                None
            }
        }
        custom => Some((custom.to_string(), Vec::new())),
    }
}

fn rofi_cmd(multiple: bool) -> (String, Vec<String>) {
    let mut args = vec!["-dmenu".to_string(), "-p".into(), "Select source".into()];
    if multiple {
        args.push("-multi-select".into());
    }
    ("rofi".into(), args)
}

fn wofi_cmd(multiple: bool) -> (String, Vec<String>) {
    let args = vec![
        "--dmenu".to_string(),
        "--prompt".into(),
        "Select source".into(),
    ];
    if multiple {
        // wofi has no native multi-select; document the limitation.
        log::warn!("picker: wofi does not support multi-select; first match wins");
    }
    ("wofi".into(), args)
}

fn which(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() && candidate.metadata().ok()?.permissions().mode() & 0o111 != 0 {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample_outputs() -> Vec<OutputInfo> {
        vec![
            OutputInfo {
                name: "DP-1".into(),
                description: "Dell".into(),
                ..Default::default()
            },
            OutputInfo {
                name: "HDMI-A-1".into(),
                description: "LG".into(),
                ..Default::default()
            },
        ]
    }

    fn select_with_values(
        outputs: &[OutputInfo],
        windows: &[ToplevelInfo],
        multiple: bool,
        output: Option<&str>,
        window: Option<&str>,
        picker: Option<&str>,
    ) -> Result<SourceSelection, &'static str> {
        let config = PickerConfig::from_values(
            output.map(str::to_owned),
            window.map(str::to_owned),
            picker.map(str::to_owned),
        );
        pick_sources_from_config(outputs, windows, multiple, &config, None)
    }

    #[test]
    fn no_usable_picker_cancels_instead_of_selecting_sources() {
        for multiple in [false, true] {
            assert!(
                select_with_values(&sample_outputs(), &[], multiple, None, None, Some("")).is_err()
            );
            let windows = vec![ToplevelInfo {
                identifier: "window-a".into(),
                app_id: "example".into(),
                title: "Private".into(),
            }];
            assert!(select_with_values(&[], &windows, multiple, None, None, Some("")).is_err());
        }
    }

    #[test]
    fn default_picker_auto_detects_rofi_then_wofi_without_granting_access() {
        let spec = PickerConfig::from_values(None, None, None).picker;
        assert_eq!(spec, "auto");
        assert_eq!(
            resolve_picker_with(&spec, false, |_| true).unwrap().0,
            "rofi"
        );
        assert_eq!(
            resolve_picker_with(&spec, false, |name| name == "wofi")
                .unwrap()
                .0,
            "wofi"
        );
        assert!(resolve_picker_with(&spec, false, |_| false).is_none());
        assert_eq!(
            configured_picker_spec(Some("custom-picker".into())),
            "custom-picker"
        );
    }

    #[test]
    fn picker_binary_failing_with_nonzero_returns_cancelled() {
        // Retain the subprocess boundary regression, without changing env.
        let result = select_with_values(
            &sample_outputs(),
            &[],
            false,
            None,
            None,
            Some("/bin/false"),
        );
        assert!(result.is_err(), "expected cancellation, got {result:?}");
    }

    #[test]
    fn picker_binary_missing_returns_cancelled() {
        let result = select_with_values(
            &sample_outputs(),
            &[],
            false,
            None,
            None,
            Some("/definitely/not/a/real/picker/binary"),
        );
        assert!(result.is_err(), "expected cancellation, got {result:?}");
    }

    #[test]
    fn jwm_portal_output_override_skips_picker() {
        let result = select_with_values(
            &sample_outputs(),
            &[],
            false,
            Some("DP-1"),
            None,
            Some("/bin/false"),
        )
        .unwrap();
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(result.outputs[0].name, "DP-1");
        assert!(result.toplevels.is_empty());
    }

    #[test]
    fn empty_available_sources_cancel_without_invoking_picker() {
        // The unified production selector cancels an empty source set. The
        // old empty-Picked assertion covered an obsolete cfg(test) helper.
        assert_eq!(
            select_with_values(&[], &[], false, None, None, Some("/bin/false")).unwrap_err(),
            "no permitted sources are available",
        );
    }

    #[test]
    fn unmatched_or_empty_output_override_never_falls_back() {
        for name in ["not-an-output", ""] {
            assert!(
                select_with_values(
                    &sample_outputs(),
                    &[],
                    false,
                    Some(name),
                    None,
                    Some("/bin/cat"),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn unmatched_window_override_never_falls_back() {
        let windows = vec![ToplevelInfo {
            identifier: "window-a".into(),
            app_id: "example".into(),
            title: "Private".into(),
        }];
        assert!(
            select_with_values(
                &[],
                &windows,
                false,
                None,
                Some("title:not-a-window"),
                Some("/bin/cat"),
            )
            .is_err()
        );
    }

    #[test]
    fn identical_window_titles_keep_distinct_source_identity() {
        let labels = vec![
            "[Window] browser — New Tab\0window-a".to_string(),
            "[Window] browser — New Tab\0window-b".to_string(),
        ];
        let displays = picker_display_lines(&labels);
        assert_ne!(displays[0], displays[1]);
        assert_eq!(
            selected_picker_labels(&labels, &displays, &displays[1]),
            vec![labels[1].clone()]
        );
        assert!(selected_picker_labels(&labels, &displays, "[3] hidden source").is_empty());
        assert!(
            selected_picker_labels(&labels, &displays, "[Window] browser — New Tab").is_empty()
        );
    }

    #[test]
    fn window_control_characters_cannot_inject_picker_entries() {
        let labels = vec!["[Window] app — title\n[2] forged\r\trow\0window-a".to_string()];
        let displays = picker_display_lines(&labels);
        assert_eq!(displays.len(), 1);
        assert!(!displays[0].chars().any(char::is_control));
        assert_eq!(
            selected_picker_labels(&labels, &displays, &displays[0]),
            labels
        );
        assert!(selected_picker_labels(&labels, &displays, "[2] forged").is_empty());
    }
}

#[cfg(test)]
mod unified_source_tests {
    use super::*;

    fn outputs() -> Vec<OutputInfo> {
        vec![OutputInfo {
            name: "DP-1".into(),
            description: "Display".into(),
            ..Default::default()
        }]
    }
    fn windows() -> Vec<ToplevelInfo> {
        vec![ToplevelInfo {
            identifier: "window-1".into(),
            app_id: "editor".into(),
            title: "Draft".into(),
            ..Default::default()
        }]
    }

    #[test]
    fn mixed_type_single_source_request_uses_one_combined_choice() {
        let selected = pick_sources_with(
            &outputs(),
            &windows(),
            false,
            None,
            None,
            None,
            |labels, multiple| {
                assert!(!multiple);
                assert_eq!(labels.len(), 2);
                PickerOutcome::Picked(vec![labels[1].clone()])
            },
        )
        .unwrap();
        assert!(selected.outputs.is_empty());
        assert_eq!(selected.toplevels[0].identifier, "window-1");
    }

    #[test]
    fn conflicting_overrides_cancel_instead_of_silently_dropping_a_source() {
        assert!(
            pick_sources_with(
                &outputs(),
                &windows(),
                false,
                Some("DP-1"),
                Some("class:editor"),
                None,
                |_, _| panic!("explicit selections must not invoke picker")
            )
            .is_err()
        );
        let selected = pick_sources_with(
            &outputs(),
            &windows(),
            true,
            Some("DP-1"),
            Some("class:editor"),
            None,
            |_, _| panic!("explicit selections must not invoke picker"),
        )
        .unwrap();
        assert_eq!(selected.outputs.len() + selected.toplevels.len(), 2);
        assert!(!selection_fits_request(&selected, false));
    }

    #[test]
    fn malformed_or_ambiguous_selection_cannot_bypass_single_source_limit() {
        for spec in ["", "title:", "class:", "title:missing"] {
            assert!(
                pick_sources_with(
                    &outputs(),
                    &windows(),
                    false,
                    None,
                    Some(spec),
                    None,
                    |_, _| panic!("invalid override must cancel")
                )
                .is_err()
            );
        }
        assert!(
            pick_sources_with(
                &outputs(),
                &windows(),
                false,
                None,
                None,
                None,
                |labels, _| PickerOutcome::Picked(labels.to_vec())
            )
            .is_err()
        );
        assert!(
            pick_sources_with(&outputs(), &windows(), false, None, None, None, |_, _| {
                PickerOutcome::Picked(vec!["unshown-source".into()])
            })
            .is_err()
        );
    }
}
