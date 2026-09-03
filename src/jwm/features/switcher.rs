//! The Alt-Tab MRU window switcher.
//!
//! Hold Alt, tap Tab to walk the most-recently-used windows, let go of Alt
//! to switch to the highlighted one; Escape or a click elsewhere cancels.
//! This file carries the gesture's pure logic — eligibility, the first
//! selection, row text, commit validation — and the `Jwm` snapshot builder.
//! The panel is an ordinary system-UI list panel; the grabs and the key
//! routing live in `navigation.rs`, `input_handler.rs` and
//! `event_dispatcher.rs`.

use crate::backend::common_define::{Mods, keys};
use crate::core::models::MonitorKey;
use crate::jwm::Jwm;

/// One window the gesture can land on. The list is built once when the
/// switcher opens: a window created mid-gesture gets no row, and one that
/// dies mid-gesture is caught by [`commit_disposition`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SwitcherEntry {
    /// Raw window id; the commit path resolves it back through `wintoclient`.
    pub window: u64,
    pub title: String,
    pub class: String,
    /// The owning monitor's number, for the "screen N" marker on other heads.
    pub monitor: i32,
    pub on_selected_monitor: bool,
    /// Minimized windows keep their row: committing one restores it.
    pub minimized: bool,
}

/// The modifiers whose release commits the gesture. Shift is deliberately
/// absent: in Alt+Shift+Tab it is normal to let Shift go a moment before
/// Alt, and that must not end the gesture early.
pub(crate) fn release_commit_mods() -> Mods {
    Mods::ALT | Mods::SUPER | Mods::CONTROL
}

/// Which modifier a released key stands for, when it is one the gesture can
/// be committed by. Anything else — Tab included — is not a commit signal.
pub(crate) fn modifier_of_keysym(keysym: u32) -> Option<Mods> {
    match keysym {
        keys::KEY_Alt_L | keys::KEY_Alt_R => Some(Mods::ALT),
        keys::KEY_Super_L | keys::KEY_Super_R => Some(Mods::SUPER),
        keys::KEY_Control_L | keys::KEY_Control_R => Some(Mods::CONTROL),
        _ => None,
    }
}

/// Whether a client earns a row: not swallowed, and on one of its monitor's
/// active tags — or sticky, which shows everywhere. Minimized clients keep
/// their tags, so they stay eligible and interleave with the visible ones
/// in MRU order; committing such a row restores the window. A scratchpad
/// parked on no tag has `tags == 0` and drops out here.
pub(crate) fn switcher_eligible(
    swallowed: bool,
    sticky: bool,
    tags: u32,
    active_tags: u32,
) -> bool {
    !swallowed && (sticky || tags & active_tags != 0)
}

/// Where the highlight starts. Forward opens on the *previous* window — one
/// tap of Alt+Tab is the classic "go back" — backward on the oldest. `None`
/// means the gesture is a no-op: nothing to list, or a directionless call.
pub(crate) fn initial_selection(len: usize, direction: i32) -> Option<usize> {
    if len == 0 || direction == 0 {
        return None;
    }
    Some(if direction > 0 { 1 % len } else { len - 1 })
}

/// One row's text, in the launcher's window-row format: icon, the title
/// (capped), the class when it adds information, and where the window is
/// when that is not "right here" — a "minimised" marker on rows a commit
/// restores, a "screen N" marker on the other heads.
pub(crate) fn switcher_row(entry: &SwitcherEntry) -> String {
    crate::jwm::features::launcher::window_row(&crate::jwm::features::launcher::WindowEntry {
        id: entry.window,
        title: entry.title.clone(),
        class: entry.class.clone(),
        instance: String::new(),
        tag: None,
        monitor: entry.monitor,
        visible: !entry.minimized,
        on_selected_monitor: entry.on_selected_monitor,
        minimized: entry.minimized,
    })
}

/// The commit-time state of a snapshotted window, as the live session
/// answers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommitWindowState {
    /// Still showing — focus it directly.
    Visible,
    /// Still minimized — restore it through the shared transition, then focus.
    Minimized,
}

/// What a commit should do with the highlighted row. The snapshot is frozen
/// at activation, so a window that closed or moved off every active tag
/// while the modifier was held fails the re-check and the gesture degrades
/// to a cancel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommitDisposition {
    Focus(u64),
    RestoreAndFocus(u64),
    Cancel,
}

/// Resolve the highlighted row against live state: `window_state` answers
/// [`CommitWindowState`] for a snapshotted window, `None` when it no longer
/// resolves to anything switchable.
pub(crate) fn commit_disposition(
    selected: Option<u64>,
    window_state: impl Fn(u64) -> Option<CommitWindowState>,
) -> CommitDisposition {
    match selected.and_then(|window| window_state(window).map(|state| (window, state))) {
        Some((window, CommitWindowState::Visible)) => CommitDisposition::Focus(window),
        Some((window, CommitWindowState::Minimized)) => CommitDisposition::RestoreAndFocus(window),
        None => CommitDisposition::Cancel,
    }
}

impl Jwm {
    /// The most-recently-used windows, selected monitor first — the same
    /// ordering the launcher's window list uses, minus only the windows the
    /// switcher cannot jump to (swallowed, or on an inactive tag). Minimized
    /// windows keep their MRU place and are restored on commit.
    pub(crate) fn window_switcher_snapshot(&self) -> Vec<SwitcherEntry> {
        let mut ordered: Vec<MonitorKey> = Vec::new();
        // The monitor in front of the user first, so its windows rank ahead
        // of the ones on the other screen.
        ordered.extend(self.state.sel_mon);
        ordered.extend(
            self.state
                .monitor_order
                .iter()
                .copied()
                .filter(|key| Some(*key) != self.state.sel_mon),
        );

        let mut entries = Vec::new();
        for monitor_key in ordered {
            let Some(monitor) = self.state.monitors.get(monitor_key) else {
                continue;
            };
            let active_tags = monitor.get_active_tags();
            let Some(stack) = self.state.monitor_stack.get(monitor_key) else {
                continue;
            };
            for &client_key in stack {
                let Some(client) = self.state.clients.get(client_key) else {
                    continue;
                };
                if !switcher_eligible(
                    client.state.is_swallowed,
                    client.state.is_sticky,
                    client.state.tags,
                    active_tags,
                ) {
                    continue;
                }
                entries.push(SwitcherEntry {
                    window: client.win.raw(),
                    title: client.name.clone(),
                    class: client.class.clone(),
                    monitor: monitor.num,
                    on_selected_monitor: Some(monitor_key) == self.state.sel_mon,
                    minimized: client.state.is_hidden,
                });
            }
        }
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jwm::features::system_ui::{ListRow, RowData, SystemUiState};

    fn entry(window: u64, title: &str, class: &str) -> SwitcherEntry {
        SwitcherEntry {
            window,
            title: title.to_string(),
            class: class.to_string(),
            monitor: 0,
            on_selected_monitor: true,
            minimized: false,
        }
    }

    fn switcher_panel(count: u64, selected: usize) -> SystemUiState {
        let rows = (0..count)
            .map(|n| ListRow {
                key: n.to_string(),
                text: format!("window {n}"),
                data: RowData::WindowSwitcher { window: n },
            })
            .collect();
        SystemUiState::window_switcher(rows, selected)
    }

    fn selected_window(panel: &SystemUiState) -> Option<u64> {
        panel.selected_switcher_window()
    }

    #[test]
    fn eligibility_excludes_swallowed_and_off_tag_windows() {
        // Visible on an active tag.
        assert!(switcher_eligible(false, false, 0b001, 0b001));
        // Sticky shows regardless of the tag mask.
        assert!(switcher_eligible(false, true, 0, 0b001));
        // On another tag only.
        assert!(!switcher_eligible(false, false, 0b010, 0b001));
        // Swallowed by its terminal.
        assert!(!switcher_eligible(true, false, 0b001, 0b001));
        // Scratchpad parked on no tag.
        assert!(!switcher_eligible(false, false, 0, 0b001));
        // Minimized state is deliberately not an input: a minimized client
        // keeps its tags, so it passes the same rule and commit restores it.
    }

    #[test]
    fn initial_selection_points_one_back_or_at_the_oldest() {
        assert_eq!(initial_selection(0, 1), None);
        assert_eq!(initial_selection(3, 0), None);
        assert_eq!(initial_selection(1, 1), Some(0));
        assert_eq!(initial_selection(3, 1), Some(1));
        assert_eq!(initial_selection(3, -1), Some(2));
        assert_eq!(initial_selection(1, -1), Some(0));
    }

    #[test]
    fn selection_steps_wrap_around_both_ends() {
        let mut panel = switcher_panel(3, 1);
        panel.move_selection(1);
        assert_eq!(selected_window(&panel), Some(2));
        // Past the tail comes back to the head.
        panel.move_selection(1);
        assert_eq!(selected_window(&panel), Some(0));
        // Past the head goes to the tail.
        panel.move_selection(-1);
        assert_eq!(selected_window(&panel), Some(2));
    }

    #[test]
    fn selection_stays_put_on_a_single_or_empty_list() {
        let mut single = switcher_panel(1, 0);
        single.move_selection(1);
        assert_eq!(selected_window(&single), Some(0));
        single.move_selection(-1);
        assert_eq!(selected_window(&single), Some(0));

        let mut empty = switcher_panel(0, 0);
        empty.move_selection(1);
        assert_eq!(selected_window(&empty), None);
    }

    #[test]
    fn switcher_row_shows_title_then_class_only_when_it_adds_something() {
        let row = switcher_row(&entry(7, "Firefox", "firefox"));
        assert!(row.contains("Firefox"));
        assert!(
            !row.contains('\u{2014}'),
            "redundant class stays off: {row}"
        );

        let row = switcher_row(&entry(7, "Document.pdf", "Evince"));
        assert!(row.contains("Document.pdf"));
        assert!(row.contains("Evince"), "distinct class is shown: {row}");
        assert!(
            row.contains('\u{2014}'),
            "distinct class joins with a dash: {row}"
        );

        // A window without a title falls back to its class.
        let row = switcher_row(&entry(7, "", "xterm"));
        assert!(row.contains("xterm"));
    }

    #[test]
    fn switcher_row_collapses_newlines_and_caps_long_titles() {
        let row = switcher_row(&entry(7, "line one\nline two", "app"));
        assert!(!row.contains('\n'), "one row is one line: {row:?}");

        let long = "x".repeat(400);
        let row = switcher_row(&entry(7, &long, "app"));
        assert!(row.contains('\u{2026}'), "long titles ellipsize: {row}");
        assert!(row.chars().count() < 100);
    }

    #[test]
    fn switcher_row_marks_windows_on_another_screen() {
        let mut other_head = entry(7, "Chat", "chat");
        other_head.on_selected_monitor = false;
        other_head.monitor = 1;
        let row = switcher_row(&other_head);
        assert!(row.contains("screen 1"), "{row}");
    }

    #[test]
    fn switcher_row_marks_a_minimized_window() {
        let mut minimized = entry(7, "Mail", "mail");
        minimized.minimized = true;
        let row = switcher_row(&minimized);
        assert!(row.contains("minimised"), "{row}");
    }

    #[test]
    fn commit_disposition_focuses_visible_restores_minimized_and_cancels_the_gone() {
        let state = |window: u64| match window {
            2 => Some(CommitWindowState::Visible),
            3 => Some(CommitWindowState::Minimized),
            _ => None,
        };
        assert_eq!(
            commit_disposition(Some(2), state),
            CommitDisposition::Focus(2)
        );
        assert_eq!(
            commit_disposition(Some(3), state),
            CommitDisposition::RestoreAndFocus(3)
        );
        // The window died — or moved off every active tag — mid-gesture.
        assert_eq!(
            commit_disposition(Some(9), state),
            CommitDisposition::Cancel
        );
        assert_eq!(commit_disposition(None, state), CommitDisposition::Cancel);
    }

    #[test]
    fn only_gesture_modifiers_resolve_to_a_commit_signal() {
        assert_eq!(modifier_of_keysym(keys::KEY_Alt_L), Some(Mods::ALT));
        assert_eq!(modifier_of_keysym(keys::KEY_Alt_R), Some(Mods::ALT));
        assert_eq!(modifier_of_keysym(keys::KEY_Super_L), Some(Mods::SUPER));
        assert_eq!(modifier_of_keysym(keys::KEY_Control_R), Some(Mods::CONTROL));
        // Tab walks the list; it never commits.
        assert_eq!(modifier_of_keysym(keys::KEY_Tab), None);
        // Shift is never armed: releasing it first must not end Alt+Shift+Tab.
        assert_eq!(modifier_of_keysym(keys::KEY_Shift_L), None);
        assert!(!release_commit_mods().contains(Mods::SHIFT));
    }
}
