//! Source picker.
//!
//! Lookup order:
//! 1. `JWM_PORTAL_OUTPUT=<name|substring of description>` env override.
//!    `JWM_PORTAL_WINDOW=class:<app_id>` or `title:<substring>`.
//! 2. `JWM_PORTAL_PICKER=rofi|wofi|<custom>` to spawn an external picker
//!    that reads a `\n`-delimited list on stdin and writes the chosen
//!    line(s) on stdout. `auto` picks rofi if found, otherwise wofi.
//! 3. Auto-pick: first available source for single-pick; all for multi.
//!
//! The external picker is invoked with `-dmenu -p "Select source"` (rofi) or
//! `--dmenu --prompt "Select source"` (wofi). Custom binaries get the
//! literal value as argv[0] and no extra flags — wrap your own if you need
//! something exotic.

use std::io::Write;
use std::process::{Command, Stdio};

use crate::wayland::{OutputInfo, ToplevelInfo};

#[derive(Debug, Default, Clone)]
pub struct SourceSelection {
    pub outputs: Vec<OutputInfo>,
    pub toplevels: Vec<ToplevelInfo>,
}

pub fn pick_outputs(available: &[OutputInfo], multiple: bool) -> Vec<OutputInfo> {
    if let Ok(name) = std::env::var("JWM_PORTAL_OUTPUT") {
        let filtered: Vec<_> = available
            .iter()
            .filter(|o| o.name == name || o.description.contains(&name))
            .cloned()
            .collect();
        if !filtered.is_empty() {
            return filtered;
        }
        log::warn!("JWM_PORTAL_OUTPUT={name} matched no output; trying picker");
    }
    if let Some(picked) = run_external_picker_outputs(available, multiple) {
        return picked;
    }
    if multiple {
        available.to_vec()
    } else {
        available.first().cloned().into_iter().collect()
    }
}

pub fn pick_windows(available: &[ToplevelInfo], multiple: bool) -> Vec<ToplevelInfo> {
    if let Ok(spec) = std::env::var("JWM_PORTAL_WINDOW") {
        let (kind, needle) = spec.split_once(':').unwrap_or(("title", spec.as_str()));
        let filtered: Vec<_> = available
            .iter()
            .filter(|t| match kind {
                "class" | "app_id" => t.app_id == needle,
                _ => t.title.contains(needle),
            })
            .cloned()
            .collect();
        if !filtered.is_empty() {
            return filtered;
        }
        log::warn!("JWM_PORTAL_WINDOW={spec} matched no window; trying picker");
    }
    if let Some(picked) = run_external_picker_toplevels(available, multiple) {
        return picked;
    }
    if multiple {
        available.to_vec()
    } else {
        available.first().cloned().into_iter().collect()
    }
}

/// Format an output as the picker-displayed label. Reversible via
/// [`label_to_output`] (returns `Some` only if the label matches one of the
/// `available` entries, defending against renamed/closed sources between
/// list-time and choice-time).
fn output_label(o: &OutputInfo) -> String {
    if o.description.is_empty() {
        format!("[Monitor] {}", o.name)
    } else {
        format!("[Monitor] {} ({})", o.name, o.description)
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

fn run_external_picker_outputs(available: &[OutputInfo], multiple: bool) -> Option<Vec<OutputInfo>> {
    if available.is_empty() {
        return None;
    }
    let labels: Vec<String> = available.iter().map(output_label).collect();
    let chosen = run_external_picker(&labels, multiple)?;
    let picked: Vec<OutputInfo> = chosen
        .into_iter()
        .filter_map(|c| {
            available
                .iter()
                .find(|o| output_label(o) == c)
                .cloned()
        })
        .collect();
    if picked.is_empty() { None } else { Some(picked) }
}

fn run_external_picker_toplevels(
    available: &[ToplevelInfo],
    multiple: bool,
) -> Option<Vec<ToplevelInfo>> {
    if available.is_empty() {
        return None;
    }
    let labels: Vec<String> = available.iter().map(toplevel_label).collect();
    let chosen = run_external_picker(&labels, multiple)?;
    let picked: Vec<ToplevelInfo> = chosen
        .into_iter()
        .filter_map(|c| {
            // Match by trailing NUL-delimited identifier — defends against
            // title edits between display and selection.
            let id = c.rsplit_once('\u{0000}').map(|(_, id)| id.to_string())?;
            available.iter().find(|t| t.identifier == id).cloned()
        })
        .collect();
    if picked.is_empty() { None } else { Some(picked) }
}

fn run_external_picker(labels: &[String], multiple: bool) -> Option<Vec<String>> {
    let picker = std::env::var("JWM_PORTAL_PICKER").ok()?;
    let (cmd, args) = resolve_picker(&picker, multiple)?;
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
            log::warn!("picker: failed to spawn `{cmd}`: {e}");
            return None;
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        for l in labels {
            // Strip our internal NUL marker from the displayed slice — many
            // pickers refuse to read NULs. We reconstruct via lookup post-hoc.
            let display = l.split('\u{0000}').next().unwrap_or(l);
            let _ = writeln!(stdin, "{display}");
        }
    }

    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            log::warn!("picker: wait failed: {e}");
            return None;
        }
    };
    if !output.status.success() {
        log::info!("picker: user cancelled (exit {:?})", output.status.code());
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut chosen: Vec<String> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            // Re-attach the identifier suffix by searching the original
            // labels for one whose display prefix matches.
            labels
                .iter()
                .find(|orig| orig.split('\u{0000}').next() == Some(l))
                .cloned()
                .unwrap_or_else(|| l.to_string())
        })
        .collect();
    if !multiple {
        chosen.truncate(1);
    }
    if chosen.is_empty() { None } else { Some(chosen) }
}

fn resolve_picker(spec: &str, multiple: bool) -> Option<(String, Vec<String>)> {
    match spec {
        "" => None,
        "rofi" => Some(rofi_cmd(multiple)),
        "wofi" => Some(wofi_cmd(multiple)),
        "auto" => {
            if which("rofi").is_some() {
                Some(rofi_cmd(multiple))
            } else if which("wofi").is_some() {
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
    let args = vec!["--dmenu".to_string(), "--prompt".into(), "Select source".into()];
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
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
