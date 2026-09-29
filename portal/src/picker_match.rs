//! Match `JWM_PORTAL_WINDOW` against Wayland toplevels, optionally enriched
//! by jwm's IPC `get_windows` list so class/instance/pid queries are not
//! limited to the Wayland foreign-toplevel `app_id`.

use crate::ipc::WindowInfo;
use crate::wayland::ToplevelInfo;

/// Filter `available` toplevels by a `JWM_PORTAL_WINDOW` spec.
///
/// Lookup order:
/// 1. Wayland `app_id` / title (existing behaviour).
/// 2. When that yields nothing and `ipc_windows` is `Some`, match against
///    jwm `get_windows` class / instance / name / pid and map hits back onto
///    toplevels by title or app_id ≈ class/instance.
///
/// An empty result means the caller should fall through to the interactive
/// picker (or its auto-pick fallback).
pub fn filter_portal_window_spec(
    spec: &str,
    available: &[ToplevelInfo],
    ipc_windows: Option<&[WindowInfo]>,
) -> Vec<ToplevelInfo> {
    let (kind, needle) = spec.split_once(':').unwrap_or(("title", spec));
    let wayland = available
        .iter()
        .filter(|t| match kind {
            "class" | "app_id" => t.app_id == needle,
            "pid" => false, // pid lives on the IPC window list only
            _ => t.title.contains(needle),
        })
        .cloned()
        .collect::<Vec<_>>();
    if !wayland.is_empty() {
        return wayland;
    }

    let Some(windows) = ipc_windows else {
        return Vec::new();
    };
    let pid_needle = (kind == "pid").then(|| needle.parse::<u32>().ok()).flatten();
    let hits: Vec<&WindowInfo> = windows
        .iter()
        .filter(|w| match kind {
            "class" | "app_id" => {
                w.class.eq_ignore_ascii_case(needle) || w.instance.eq_ignore_ascii_case(needle)
            }
            "pid" => pid_needle.is_some_and(|pid| w.pid == Some(pid)),
            _ => w.name.contains(needle),
        })
        .collect();
    if hits.is_empty() {
        return Vec::new();
    }

    let mut matched = Vec::new();
    for window in hits {
        if let Some(toplevel) = available.iter().find(|t| ipc_window_matches_toplevel(window, t))
            && !matched
                .iter()
                .any(|existing: &ToplevelInfo| existing.identifier == toplevel.identifier)
        {
            matched.push(toplevel.clone());
        }
    }
    matched
}

fn ipc_window_matches_toplevel(window: &WindowInfo, toplevel: &ToplevelInfo) -> bool {
    (!window.name.is_empty() && toplevel.title == window.name)
        || (!window.class.is_empty() && toplevel.app_id == window.class)
        || (!window.instance.is_empty() && toplevel.app_id == window.instance)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toplevel(id: &str, app_id: &str, title: &str) -> ToplevelInfo {
        ToplevelInfo {
            identifier: id.into(),
            app_id: app_id.into(),
            title: title.into(),
            ..Default::default()
        }
    }

    fn window(class: &str, instance: &str, name: &str) -> WindowInfo {
        WindowInfo {
            id: 1,
            name: name.into(),
            class: class.into(),
            instance: instance.into(),
            tags: 1,
            pid: None,
        }
    }

    fn window_with_pid(class: &str, instance: &str, name: &str, pid: u32) -> WindowInfo {
        WindowInfo {
            pid: Some(pid),
            ..window(class, instance, name)
        }
    }

    #[test]
    fn wayland_app_id_match_wins_without_ipc() {
        let available = vec![
            toplevel("a", "firefox", "Mozilla Firefox"),
            toplevel("b", "kitty", "term"),
        ];
        let matched = filter_portal_window_spec("class:firefox", &available, None);
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].identifier, "a");
    }

    #[test]
    fn ipc_class_match_maps_when_wayland_app_id_differs() {
        // Wayland app_id is empty / wrong; jwm get_windows still knows WM_CLASS.
        let available = vec![toplevel("a", "", "Mozilla Firefox")];
        let ipc = vec![window("firefox", "Navigator", "Mozilla Firefox")];
        let matched = filter_portal_window_spec("class:firefox", &available, Some(&ipc));
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].identifier, "a");
        assert_eq!(matched[0].title, "Mozilla Firefox");
    }

    #[test]
    fn ipc_miss_keeps_empty_so_picker_fallback_runs() {
        let available = vec![toplevel("a", "kitty", "term")];
        let ipc = vec![window("firefox", "Navigator", "Mozilla Firefox")];
        let matched = filter_portal_window_spec("class:firefox", &available, Some(&ipc));
        assert!(matched.is_empty());
    }

    #[test]
    fn title_substring_still_matches_wayland_first() {
        let available = vec![toplevel("a", "x", "Hello World")];
        let matched = filter_portal_window_spec("title:World", &available, None);
        assert_eq!(matched.len(), 1);
    }

    #[test]
    fn ipc_pid_match_maps_when_wayland_has_no_pid() {
        let available = vec![toplevel("a", "", "Mozilla Firefox")];
        let ipc = vec![window_with_pid("firefox", "Navigator", "Mozilla Firefox", 4242)];
        let matched = filter_portal_window_spec("pid:4242", &available, Some(&ipc));
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].identifier, "a");
        let miss = filter_portal_window_spec("pid:9999", &available, Some(&ipc));
        assert!(miss.is_empty());
    }
}
