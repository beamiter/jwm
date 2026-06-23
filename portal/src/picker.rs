//! MVP picker: env-var override + auto-pick first source.
//!
//! `JWM_PORTAL_OUTPUT=<name|substring of description>` selects an output.
//! `JWM_PORTAL_WINDOW=class:<app_id>` or `title:<substring>` selects a window.
//! Otherwise: first output / focused window for single-pick; all for multi.

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
        log::warn!("JWM_PORTAL_OUTPUT={name} matched no output; falling back to default pick");
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
        log::warn!("JWM_PORTAL_WINDOW={spec} matched no window; falling back to default pick");
    }
    if multiple {
        available.to_vec()
    } else {
        available.first().cloned().into_iter().collect()
    }
}
