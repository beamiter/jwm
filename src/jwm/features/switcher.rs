//! The Alt-Tab MRU window switcher.
//!
//! Hold Alt, tap Tab to walk the most-recently-used windows, let go of Alt
//! to switch to the highlighted one; Escape or a click elsewhere cancels,
//! and Delete (or a middle-click on a row) closes that window without
//! leaving the gesture. This file carries the gesture's pure logic —
//! eligibility, the first selection, row text, commit validation, row
//! removal — and the `Jwm` snapshot builder. The panel is an ordinary
//! system-UI list panel; the grabs and the key routing live in
//! `navigation.rs`, `input_handler.rs` and `event_dispatcher.rs`.

use crate::backend::common_define::{Mods, keys};
use crate::core::models::MonitorKey;
use crate::jwm::Jwm;
use crate::jwm::features::system_ui::{ListKind, RowData, SystemUiState};

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
///
/// "Previous" is relative to the focused window, which heads the list when
/// there is one (`focused_is_first`). When nothing is focused — the
/// selected monitor's tag is empty, or every window on it is minimized —
/// the head is not the current window but the most recent one, and that is
/// where a forward tap lands.
pub(crate) fn initial_selection(
    len: usize,
    direction: i32,
    focused_is_first: bool,
) -> Option<usize> {
    if len == 0 || direction == 0 {
        return None;
    }
    Some(if direction > 0 {
        if focused_is_first { 1 % len } else { 0 }
    } else {
        len - 1
    })
}

/// Whether a key release ends the gesture, given the modifiers `held` when
/// the panel opened. A gesture modifier the keysym table knows commits when
/// it was held. Any *other* modifier keysym — `Meta_L` for a Win key under
/// `altwin:meta_win`, `Hyper_L`, the macintosh layouts' Meta on Mod1 — is
/// decided by the live modifier mask through `live_mods`, which no layout
/// can disguise: a modifier held at open that is no longer down commits.
/// Non-modifier keys never change the mask and never commit, so the mask is
/// only queried for a modifier keysym; a failed query commits nothing.
pub(crate) fn release_commits(
    held: Mods,
    keysym: u32,
    live_mods: impl FnOnce() -> Option<Mods>,
) -> bool {
    if held.is_empty() {
        return false;
    }
    if let Some(modifier) = modifier_of_keysym(keysym) {
        return held.contains(modifier);
    }
    if !(keys::KEY_Shift_L..=keys::KEY_Hyper_R).contains(&keysym) {
        return false;
    }
    live_mods().is_some_and(|down| (held & down) != held)
}

/// What a button press over the open switcher panel does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SwitcherPress {
    /// Button 1: commit the row under the pointer, or cancel when the press
    /// landed on no row at all.
    PickRow,
    /// Button 2: close the row under the pointer without leaving the gesture —
    /// the pointer twin of Delete / BackSpace. A miss on blank is inert (the
    /// expose grid's middle-click shape), not a cancel.
    CloseRow,
    /// The wheel: step the highlight by this much without committing, the
    /// way it browses every other panel the grab hands presses to.
    Browse(isize),
    /// The horizontal wheel: not a click, and nothing to browse with.
    Inert,
    /// Any other real click: the gesture ends.
    Cancel,
}

/// Decide a press from its X11 button number.
///
/// The panel holds a button grab, so X11 delivers the wheel (buttons 4-7)
/// here as presses alongside the real clicks. Folding those into "anything
/// but button 1 cancels" would let a stray scroll — a touchpad flick while
/// the modifier is still held — throw away the switch the user is in the
/// middle of making; a scroll asks to *browse*, which is exactly what the
/// same wheel does over the control center, the launcher and the layout
/// picker. Same rule, same reason as `input_handler::toast_press`: the
/// wheel is not a click. Middle-click closes the pointed row the way Delete
/// closes the highlight and expose's middle-click closes a cell; right-click
/// and other buttons still cancel.
pub(crate) fn switcher_press(button: u8) -> SwitcherPress {
    match button {
        1 => SwitcherPress::PickRow,
        2 => SwitcherPress::CloseRow,
        4 => SwitcherPress::Browse(-1),
        5 => SwitcherPress::Browse(1),
        6 | 7 => SwitcherPress::Inert,
        _ => SwitcherPress::Cancel,
    }
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

/// One row's icon: the window's class resolved through the shared cached
/// resolver — the same source the bars draw the focused window's icon from.
/// A miss is cached there, so a window without a desktop entry costs one
/// bounded lookup per session, and the row keeps its generic glyph prefix:
/// no empty hole.
pub(crate) fn switcher_row_icon(entry: &SwitcherEntry) -> Option<String> {
    crate::jwm::features::launcher::resolve_window_icon(&entry.class, "")
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

impl SystemUiState {
    /// Drop the highlighted row because Delete closed its window mid-gesture.
    /// The highlight keeps its index, so the next-oldest window slides under
    /// it — a highlight on the closed tail clamps onto the new tail — and
    /// the surviving rows keep their MRU-snapshot order. Returns the row's
    /// window and whether that emptied the list: the opener refuses an empty
    /// list, so the caller ends the gesture rather than show one. `None`
    /// means this panel is not the switcher and nothing was touched.
    pub(crate) fn remove_selected_switcher_row(&mut self) -> Option<(u64, bool)> {
        let Self::ListPanel {
            kind,
            rows,
            row_icons,
            selected,
            ..
        } = self
        else {
            return None;
        };
        if *kind != ListKind::WindowSwitcher {
            return None;
        }
        let window = match rows.get(*selected).map(|row| &row.data) {
            Some(RowData::WindowSwitcher { window }) => *window,
            _ => return None,
        };
        rows.remove(*selected);
        // The icons ride beside the rows; dropping one without the other
        // would misalign every row below it.
        if *selected < row_icons.len() {
            row_icons.remove(*selected);
        }
        if *selected >= rows.len() {
            *selected = rows.len().saturating_sub(1);
        }
        Some((window, rows.is_empty()))
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
        assert_eq!(initial_selection(0, 1, true), None);
        assert_eq!(initial_selection(3, 0, true), None);
        assert_eq!(initial_selection(1, 1, true), Some(0));
        assert_eq!(initial_selection(3, 1, true), Some(1));
        assert_eq!(initial_selection(3, -1, true), Some(2));
        assert_eq!(initial_selection(1, -1, true), Some(0));
    }

    #[test]
    fn initial_selection_starts_at_the_head_when_nothing_is_focused() {
        // The selected monitor's tag is empty, or everything on it is
        // minimized: the head of the list is already the previous window,
        // so one tap must land on it rather than skip it.
        assert_eq!(initial_selection(3, 1, false), Some(0));
        assert_eq!(initial_selection(1, 1, false), Some(0));
        // Backward still opens on the oldest.
        assert_eq!(initial_selection(3, -1, false), Some(2));
        assert_eq!(initial_selection(0, 1, false), None);
    }

    #[test]
    fn the_wheel_browses_the_panel_and_only_a_click_can_end_the_gesture() {
        // The panel took a button grab so a click on a row can commit it;
        // on X11 that same grab delivers the wheel as buttons 4-7. Losing an
        // Alt+Tab to a touchpad flick is the regression this pins.
        assert_eq!(switcher_press(1), SwitcherPress::PickRow);
        assert_eq!(switcher_press(2), SwitcherPress::CloseRow);
        assert_eq!(switcher_press(4), SwitcherPress::Browse(-1));
        assert_eq!(switcher_press(5), SwitcherPress::Browse(1));
        assert_eq!(switcher_press(6), SwitcherPress::Inert);
        assert_eq!(switcher_press(7), SwitcherPress::Inert);
        // A real click that is not pick or close still ends it.
        for button in [3u8, 8, 9] {
            assert_eq!(
                switcher_press(button),
                SwitcherPress::Cancel,
                "button {button} is a click"
            );
        }
    }

    #[test]
    fn a_release_commits_by_keysym_or_failing_that_by_the_live_mask() {
        let held = Mods::SUPER;
        let never = || -> Option<Mods> { panic!("the mask is not queried for a known keysym") };
        // A known gesture modifier decides on its own.
        assert!(release_commits(held, keys::KEY_Super_L, never));
        assert!(!release_commits(held, keys::KEY_Alt_L, never));
        // A non-modifier key never commits and never costs a query.
        assert!(!release_commits(held, keys::KEY_Tab, never));
        assert!(!release_commits(held, keys::KEY_j, never));
        // altwin:meta_win — the Win key's keysym is Meta_L. Layout-neutral
        // evidence decides: Super is no longer down, so it commits...
        assert!(release_commits(held, keys::KEY_Meta_L, || Some(
            Mods::empty()
        )));
        assert!(release_commits(held, keys::KEY_Hyper_R, || Some(Mods::ALT)));
        // ...and while Super is still down it does not.
        assert!(!release_commits(held, keys::KEY_Meta_L, || Some(
            Mods::SUPER
        )));
        // Ctrl+Alt+Tab: letting go of either held modifier commits, as the
        // keysym path already promises.
        let both = Mods::ALT | Mods::CONTROL;
        assert!(release_commits(both, keys::KEY_Meta_R, || Some(
            Mods::CONTROL
        )));
        assert!(!release_commits(both, keys::KEY_Meta_R, || Some(both)));
        // No answer from the server is no commit; nothing held is no commit.
        assert!(!release_commits(held, keys::KEY_Meta_L, || None));
        assert!(!release_commits(Mods::empty(), keys::KEY_Super_L, never));
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
    fn removing_a_row_slides_the_next_oldest_window_under_the_highlight() {
        // The highlight keeps its index, so the row after the closed one —
        // the next-oldest window — is where a commit now lands.
        let mut panel = switcher_panel(3, 1);
        assert_eq!(panel.remove_selected_switcher_row(), Some((1, false)));
        assert_eq!(selected_window(&panel), Some(2));

        let mut panel = switcher_panel(3, 0);
        assert_eq!(panel.remove_selected_switcher_row(), Some((0, false)));
        assert_eq!(selected_window(&panel), Some(1));

        // The survivors keep their MRU-snapshot order: nothing re-sorts
        // mid-gesture, whatever the close goes on to focus.
        let SystemUiState::ListPanel { rows, .. } = &panel else {
            panic!("the switcher is a list panel");
        };
        let order: Vec<u64> = rows
            .iter()
            .map(|row| match &row.data {
                RowData::WindowSwitcher { window } => *window,
                _ => panic!("switcher rows carry windows"),
            })
            .collect();
        assert_eq!(order, [1, 2]);
    }

    #[test]
    fn removing_the_tail_row_clamps_the_highlight_onto_the_new_tail() {
        let mut panel = switcher_panel(3, 2);
        assert_eq!(panel.remove_selected_switcher_row(), Some((2, false)));
        assert_eq!(selected_window(&panel), Some(1));
    }

    #[test]
    fn removing_the_only_row_reports_the_gesture_empty() {
        let mut panel = switcher_panel(1, 0);
        assert_eq!(panel.remove_selected_switcher_row(), Some((0, true)));
        assert_eq!(selected_window(&panel), None);
        // Until the caller closes the panel it still steps without panicking
        // and without inventing a selection.
        panel.move_selection(1);
        assert_eq!(selected_window(&panel), None);
    }

    #[test]
    fn only_the_switcher_loses_a_row_to_a_close() {
        let mut inactive = SystemUiState::default();
        assert_eq!(inactive.remove_selected_switcher_row(), None);

        let mut notifications = SystemUiState::ListPanel {
            kind: ListKind::Notifications,
            rows: vec![ListRow {
                key: "1".to_string(),
                text: "notification".to_string(),
                data: RowData::Notification {
                    id: 1,
                    actions: Vec::new(),
                    cursor: 0,
                },
            }],
            row_icons: Vec::new(),
            selected: 0,
            message: String::new(),
            prompt: None,
            query: String::new(),
            empty: "No notifications".to_string(),
        };
        assert_eq!(notifications.remove_selected_switcher_row(), None);
        assert!(
            matches!(&notifications, SystemUiState::ListPanel { rows, .. } if rows.len() == 1),
            "another panel's rows are untouched"
        );
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
