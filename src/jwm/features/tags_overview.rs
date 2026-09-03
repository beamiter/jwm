//! The tags overview: a GNOME-style grid showing every tag of the selected
//! monitor at once.
//!
//! One cell per tag, each carrying the tag's number and a line-drawn
//! wireframe of its windows. The panel itself is drawn by the compositors
//! from [`crate::backend::compositor_common::tags_grid`]; everything that
//! decides *what* it shows and what a keystroke means lives here.
//!
//! Unlike the layout picker nothing is previewed: the desktop behind the
//! panel never changes while the grid is up, so cancelling is simply closing
//! and confirming is an ordinary tag jump (`Jwm::view`).

use crate::backend::api::{Backend, ExposeNavDirection, TagsGridCell};
use crate::backend::compositor_common::expose::{expose_grid_cols, move_expose_selection};
use crate::config::CONFIG;
use crate::core::models::MonitorKey;
use crate::core::types::Rect;
use crate::jwm::Jwm;
use crate::jwm::features::SystemUiState;
use crate::jwm::features::toggles::configured_feature_toggle_allowed;
use crate::jwm::types::WMArgEnum;

/// One client's slice of the snapshot, narrow enough for the cell builder to
/// be tested without a window manager.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TagClientFrame {
    /// Raw `ClientState::tags`. Sticky windows have theirs rewritten to the
    /// active mask by `Jwm::update_sticky_tags`, which is why stickiness is
    /// carried separately.
    pub tags: u32,
    /// `ClientState::is_sticky`: the window floats above every tag.
    pub sticky: bool,
    /// Minimized (`ClientState::is_hidden`): draws no outline, but the tag
    /// still reads occupied.
    pub minimized: bool,
    /// Swallowed by a terminal child (`ClientState::is_swallowed`): same
    /// treatment as minimized — it owns no screen real estate.
    pub swallowed: bool,
    /// The window's *visible* rectangle in global coordinates. Callers must
    /// resolve parked windows through `ClientGeometry::hidden_restore_rect`
    /// before filling this in; the builder takes the rect verbatim.
    pub rect: [i32; 4],
}

/// The open panel's state: a snapshot of the monitor's tags plus the
/// keyboard highlight.
#[derive(Debug, Clone)]
pub struct TagsOverviewState {
    /// One cell per tag, in tag order.
    pub cells: Vec<TagsGridCell>,
    /// Columns of the grid the cells are walked (and drawn) in.
    pub cols: u32,
    /// Highlighted cell: a tag index, stable across rebuilds.
    pub selected: usize,
}

impl TagsOverviewState {
    /// Open the overview on the monitor's current tag. A multi-tag view
    /// pre-selects its lowest set bit, matching `Jwm::primary_tag_index`.
    pub fn new(
        clients: &[TagClientFrame],
        active_tags: u32,
        work: [i32; 4],
        tags_length: usize,
    ) -> Self {
        let cells = build_cells(clients, active_tags, work, tags_length);
        let cols = grid_cols(cells.len(), work);
        let selected = if active_tags == 0 || active_tags == u32::MAX {
            0
        } else {
            (active_tags.trailing_zeros() as usize).min(cells.len().saturating_sub(1))
        };
        Self {
            cells,
            cols,
            selected,
        }
    }

    /// Move the highlight one step through the grid. Movement clamps at row
    /// and grid edges instead of wrapping — the exact semantics of the expose
    /// walk, shared through [`move_expose_selection`]. Returns whether the
    /// selection actually moved, so a clamped arrow costs no redraw.
    pub fn move_selection(&mut self, direction: ExposeNavDirection) -> bool {
        match move_expose_selection(Some(self.selected), direction, self.cells.len(), self.cols) {
            Some(next) if next != self.selected => {
                self.selected = next;
                true
            }
            _ => false,
        }
    }
}

/// Columns of the overview grid: the expose grid's aspect-driven shape over
/// the monitor's work area, so the keyboard walk and the drawn grid share
/// one shape source. Clamped to the cell count — a lone cell has one column,
/// never the two the raw aspect formula suggests.
pub fn grid_cols(tags_length: usize, work: [i32; 4]) -> u32 {
    let count = tags_length.max(1);
    expose_grid_cols(count, work[2] as f32, work[3] as f32)
        .min(count as u32)
        .max(1)
}

/// The tag mask a commit jumps to. `None` when a config reload shrank the
/// tag list out from under the open panel; the caller treats that as a
/// cancel.
pub fn commit_mask(selected: usize, tags_length: usize) -> Option<u32> {
    if selected >= tags_length {
        return None;
    }
    1u32.checked_shl(selected as u32)
}

/// Build one cell per tag from the snapshot.
///
/// Occupied follows the status bar's `calculate_tag_masks`: any window on
/// the tag counts, minimized and swallowed ones included, while windows
/// floating above the tag axis — sticky ones, or clients spanning every tag
/// — draw everywhere but mark nothing occupied. Outlines are normalized to
/// the work area and clipped to `0.0..=1.0`: a floating window may hang off
/// the area, but its wireframe may not poke through the cell.
pub fn build_cells(
    clients: &[TagClientFrame],
    active_tags: u32,
    work: [i32; 4],
    tags_length: usize,
) -> Vec<TagsGridCell> {
    let tags_length = tags_length.clamp(1, 31);
    let [wx, wy, ww, wh] = work;
    let full_mask = (1u32 << tags_length) - 1;

    let mut cells: Vec<TagsGridCell> = (0..tags_length)
        .map(|tag_index| TagsGridCell {
            tag_index,
            windows: Vec::new(),
            occupied: false,
            active: (active_tags >> tag_index) & 1 != 0,
        })
        .collect();

    for client in clients {
        let effective_tags = client.tags & full_mask;
        let floats_everywhere = client.sticky || effective_tags == full_mask;
        let draws_outline = !client.minimized && !client.swallowed;
        for (index, cell) in cells.iter_mut().enumerate() {
            let on_tag = client.sticky || (effective_tags >> index) & 1 != 0;
            if !on_tag {
                continue;
            }
            if !floats_everywhere {
                cell.occupied = true;
            }
            if !draws_outline {
                continue;
            }
            if let Some(outline) = normalize_rect(client.rect, wx, wy, ww, wh) {
                cell.windows.push(outline);
            }
        }
    }
    cells
}

/// Map a global window rectangle into a cell's `0.0..=1.0` frame, clipping
/// at the work-area edges. `None` when nothing of the window is inside.
fn normalize_rect(rect: [i32; 4], wx: i32, wy: i32, ww: i32, wh: i32) -> Option<[f32; 4]> {
    let [x, y, w, h] = rect;
    // Geometry can legitimately sit near either end of the signed coordinate
    // space, so the far edges are computed in i64 before the float divide.
    let ww = i64::from(ww.max(1)) as f32;
    let wh = i64::from(wh.max(1)) as f32;
    let left = (i64::from(x) - i64::from(wx)) as f32 / ww;
    let top = (i64::from(y) - i64::from(wy)) as f32 / wh;
    let right = (i64::from(x) + i64::from(w) - i64::from(wx)) as f32 / ww;
    let bottom = (i64::from(y) + i64::from(h) - i64::from(wy)) as f32 / wh;

    let left = left.clamp(0.0, 1.0);
    let top = top.clamp(0.0, 1.0);
    let right = right.clamp(0.0, 1.0);
    let bottom = bottom.clamp(0.0, 1.0);
    let (clipped_w, clipped_h) = (right - left, bottom - top);
    (clipped_w > 0.0 && clipped_h > 0.0).then_some([left, top, clipped_w, clipped_h])
}

impl Jwm {
    /// Open the grid on the selected monitor, or close it when it is already
    /// up — the key is a toggle, like every other shell panel key.
    pub(crate) fn toggle_tags_overview(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // The config flag gates entry only; an open overview must always
        // retain its exit path so it can release the input grabs.
        if !configured_feature_toggle_allowed(
            self.features.system_ui.is_tags_overview(),
            CONFIG.load().behavior().tags_overview_enabled,
        ) {
            return Ok(());
        }
        if self.toggle_off_system_ui(backend, SystemUiState::is_tags_overview) {
            return Ok(());
        }
        let sel_mon_key = self.state.sel_mon.ok_or("No selected monitor")?;
        self.prepare_system_ui(backend, "tags overview", false)?;
        self.features.system_ui =
            SystemUiState::TagsOverview(self.tags_overview_state(sel_mon_key));
        self.sync_system_ui(backend);
        Ok(())
    }

    /// Move the highlight one cell in the grid, clamping at the edges.
    pub(crate) fn move_tags_overview_selection(
        &mut self,
        backend: &mut dyn Backend,
        direction: ExposeNavDirection,
    ) {
        let Some(overview) = self.features.system_ui.tags_overview_mut() else {
            return;
        };
        if overview.move_selection(direction) {
            self.sync_system_ui(backend);
        }
    }

    /// Commit the highlighted tag: an ordinary `view` jump, then the panel
    /// goes away. The view itself early-outs on the current tag, so opening
    /// and confirming is a safe no-op round trip.
    pub(crate) fn confirm_tags_overview(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(overview) = self.features.system_ui.tags_overview() else {
            return Ok(());
        };
        let Some(mask) = commit_mask(overview.selected, CONFIG.load().tags_length()) else {
            // A config reload shrank the tag list under the open panel: there
            // is no tag left to jump to, so the commit degrades to a cancel.
            self.close_system_ui(backend);
            return Ok(());
        };
        log::info!("[tags_overview] jumping to tag {}", overview.selected + 1);
        self.view(backend, &WMArgEnum::UInt(mask))?;
        self.close_system_ui(backend);
        Ok(())
    }

    /// Cancel is a plain close: the overview never previews anything, so
    /// there is nothing to undo (unlike the layout picker's restore).
    pub(crate) fn cancel_tags_overview(&mut self, backend: &mut dyn Backend) {
        self.close_system_ui(backend);
    }

    /// A digit jumps straight to its tag and commits, which is exactly what
    /// the global `Mod1+N` bindings do — except those are unreachable while
    /// the panel holds the keyboard grab, so the digit is handled here.
    pub(crate) fn jump_tags_overview(
        &mut self,
        backend: &mut dyn Backend,
        index: usize,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(overview) = self.features.system_ui.tags_overview_mut() else {
            return Ok(());
        };
        if index >= overview.cells.len() {
            return Ok(());
        }
        overview.selected = index;
        self.confirm_tags_overview(backend)
    }

    /// Rebuild the cells after window changes while the panel is open (the
    /// `arrange` tail calls here). The selection is a tag index, so a rebuild
    /// cannot shift what it means; a tag list shrunk by a config reload
    /// clamps it instead.
    pub(crate) fn refresh_tags_overview(&mut self) {
        if !self.features.system_ui.is_tags_overview() {
            return;
        }
        let Some(sel_mon_key) = self.state.sel_mon else {
            return;
        };
        let rebuilt = self.tags_overview_state(sel_mon_key);
        if let Some(overview) = self.features.system_ui.tags_overview_mut() {
            let selected = overview.selected;
            *overview = rebuilt;
            overview.selected = selected.min(overview.cells.len().saturating_sub(1));
        }
        self.mark_system_ui_dirty();
    }

    /// The panel's contents for one monitor: the current tags snapshot in
    /// grid form.
    fn tags_overview_state(&self, mon_key: MonitorKey) -> TagsOverviewState {
        let tags_length = CONFIG.load().tags_length();
        let (active_tags, work) = self
            .state
            .monitors
            .get(mon_key)
            .map(|monitor| {
                let geometry = &monitor.geometry;
                (
                    monitor.get_active_tags(),
                    [geometry.w_x, geometry.w_y, geometry.w_w, geometry.w_h],
                )
            })
            .unwrap_or((0, [0, 0, 1, 1]));
        TagsOverviewState::new(
            &self.tags_overview_frames(mon_key),
            active_tags,
            work,
            tags_length,
        )
    }

    /// Snapshot the monitor's clients as wireframe frames, back to front.
    fn tags_overview_frames(&self, mon_key: MonitorKey) -> Vec<TagClientFrame> {
        let Some(stack) = self.state.monitor_stack.get(mon_key) else {
            return Vec::new();
        };
        // monitor_stack is top-to-bottom; the grid draws back to front.
        stack
            .iter()
            .rev()
            .filter_map(|&client_key| {
                let client = self.state.clients.get(client_key)?;
                // Shell chrome (the bars) is not workspace content.
                if client.state.is_dock {
                    return None;
                }
                let geometry = &client.geometry;
                // Windows on other tags are parked off-screen; the visible
                // rectangle lives in the restore slot.
                let visible = geometry
                    .hidden_restore_rect
                    .unwrap_or(Rect::new(geometry.x, geometry.y, geometry.w, geometry.h));
                Some(TagClientFrame {
                    tags: client.state.tags,
                    sticky: client.state.is_sticky,
                    minimized: client.state.is_hidden,
                    swallowed: client.state.is_swallowed,
                    rect: [visible.x, visible.y, visible.w, visible.h],
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORK: [i32; 4] = [0, 0, 1920, 1080];

    fn frame(tags: u32, rect: [i32; 4]) -> TagClientFrame {
        TagClientFrame {
            tags,
            sticky: false,
            minimized: false,
            swallowed: false,
            rect,
        }
    }

    #[test]
    fn a_window_on_several_tags_appears_in_each_of_their_cells() {
        let cells = build_cells(&[frame(0b101, [100, 100, 400, 300])], 0b001, WORK, 9);
        assert_eq!(cells[0].windows.len(), 1);
        assert_eq!(cells[1].windows.len(), 0);
        assert_eq!(cells[2].windows.len(), 1);
        assert!(cells[0].occupied);
        assert!(!cells[1].occupied);
        assert!(cells[2].occupied);
    }

    #[test]
    fn a_sticky_window_draws_everywhere_but_occupies_nothing() {
        let sticky = TagClientFrame {
            // update_sticky_tags rewrites the mask to the active tag; the
            // flag, not the mask, is what makes it sticky.
            tags: 0b001,
            sticky: true,
            ..frame(0, [100, 100, 400, 300])
        };
        let cells = build_cells(&[sticky], 0b001, WORK, 9);
        for (index, cell) in cells.iter().enumerate() {
            assert_eq!(
                cell.windows.len(),
                1,
                "sticky window missing from cell {index}"
            );
            assert!(!cell.occupied, "sticky window must not occupy tag {index}");
        }
    }

    #[test]
    fn minimized_and_swallowed_windows_count_occupied_without_an_outline() {
        let minimized = TagClientFrame {
            minimized: true,
            ..frame(0b001, [100, 100, 400, 300])
        };
        let swallowed = TagClientFrame {
            swallowed: true,
            ..frame(0b010, [200, 200, 400, 300])
        };
        let cells = build_cells(&[minimized, swallowed], 0b001, WORK, 9);
        assert!(cells[0].occupied);
        assert!(cells[0].windows.is_empty());
        assert!(cells[1].occupied);
        assert!(cells[1].windows.is_empty());
        assert!(!cells[2].occupied);
    }

    #[test]
    fn a_client_spanning_every_tag_marks_none_occupied() {
        // The status bar's own convention: an all-tags client (its effective
        // mask is the full mask) must not light up the whole strip.
        let cells = build_cells(&[frame(0x1ff, [0, 0, 1920, 24])], 0b001, WORK, 9);
        for (index, cell) in cells.iter().enumerate() {
            assert_eq!(
                cell.windows.len(),
                1,
                "all-tags client missing from cell {index}"
            );
            assert!(!cell.occupied);
        }
    }

    #[test]
    fn outlines_are_normalized_to_the_work_area_and_clipped_to_the_cell() {
        let cells = build_cells(
            &[
                // Fully inside.
                frame(0b001, [960, 540, 960, 540]),
                // Hanging off the left/top: clipped, not dropped.
                frame(0b001, [-100, -50, 300, 200]),
                // Floating past the right/bottom edge.
                frame(0b001, [1800, 1000, 400, 400]),
                // Entirely outside: no outline at all.
                frame(0b001, [5000, 5000, 100, 100]),
            ],
            0b001,
            WORK,
            9,
        );
        let windows = &cells[0].windows;
        assert_eq!(windows.len(), 3);
        let close = |actual: [f32; 4], expected: [f32; 4]| {
            actual
                .iter()
                .zip(expected.iter())
                .all(|(a, e)| (a - e).abs() < 0.0001)
        };
        assert!(close(windows[0], [0.5, 0.5, 0.5, 0.5]), "{:?}", windows[0]);
        assert!(
            close(windows[1], [0.0, 0.0, 200.0 / 1920.0, 150.0 / 1080.0]),
            "{:?}",
            windows[1]
        );
        assert!(
            close(
                windows[2],
                [
                    1800.0 / 1920.0,
                    1000.0 / 1080.0,
                    120.0 / 1920.0,
                    80.0 / 1080.0,
                ]
            ),
            "{:?}",
            windows[2]
        );
        for window in windows {
            let [x, y, w, h] = *window;
            assert!(x >= 0.0 && y >= 0.0 && x + w <= 1.001 && y + h <= 1.001);
        }
    }

    #[test]
    fn the_snapshot_rect_is_used_verbatim() {
        // The caller resolves a parked window through hidden_restore_rect;
        // whatever rect arrives is what the cell shows. Pin that down with a
        // window whose rect sits on a negative-origin monitor's work area.
        let work = [-1920, 40, 1920, 1040];
        let cells = build_cells(&[frame(0b001, [-1920, 40, 960, 520])], 0b001, work, 9);
        assert_eq!(cells[0].windows, vec![[0.0, 0.0, 0.5, 0.5]]);
    }

    #[test]
    fn active_marks_every_visible_tag() {
        let cells = build_cells(&[], 0b0110, WORK, 9);
        assert!(!cells[0].active);
        assert!(cells[1].active);
        assert!(cells[2].active);
        assert!(!cells[3].active);
        // The dwm "view all" mask makes every tag active.
        let all = build_cells(&[], u32::MAX, WORK, 9);
        assert!(all.iter().all(|cell| cell.active));
    }

    #[test]
    fn opening_preselects_the_lowest_active_tag() {
        let state = TagsOverviewState::new(&[], 0b1010, WORK, 9);
        assert_eq!(state.selected, 1);
        let none_active = TagsOverviewState::new(&[], 0, WORK, 9);
        assert_eq!(none_active.selected, 0);
    }

    #[test]
    fn movement_clamps_at_the_grid_edges_instead_of_wrapping() {
        let mut state = TagsOverviewState::new(&[], 0b001, WORK, 9);
        assert_eq!(state.cols, expose_grid_cols(9, 1920.0, 1080.0));
        let cols = state.cols as usize;

        assert!(
            !state.move_selection(ExposeNavDirection::Left),
            "column 0 clamps left"
        );
        assert!(state.move_selection(ExposeNavDirection::Right));
        assert_eq!(state.selected, 1);
        assert!(
            !state.move_selection(ExposeNavDirection::Up),
            "the top row clamps up"
        );
        assert_eq!(state.selected, 1);

        // Walk to the last row, then Down onto the short row stays put.
        state.selected = 8;
        assert!(!state.move_selection(ExposeNavDirection::Down));
        assert_eq!(state.selected, 8);

        // Down moves a full column when the row below exists.
        state.selected = 0;
        assert!(state.move_selection(ExposeNavDirection::Down));
        assert_eq!(state.selected, cols);
    }

    #[test]
    fn movement_matches_the_drawn_grid_shape() {
        // The consistency property expose pins between expose_grid_cols and
        // build_expose_entries, here between grid_cols, move_selection and
        // the geometry both compositors draw.
        for (n, w, h) in [
            (1usize, 1920, 1080),
            (9, 1920, 1080),
            (31, 3440, 1440),
            (7, 1080, 1920),
        ] {
            let work = [0, 0, w, h];
            let cols = grid_cols(n, work);
            let geometry = crate::backend::compositor_common::tags_grid::grid_geometry(
                [0.0, 0.0, w as f32, h as f32],
                n,
                cols,
            );
            assert_eq!(
                geometry.cols, cols,
                "{n} tags: renderer and walker disagree"
            );

            let mut state = TagsOverviewState::new(&[], 1, work, n);
            if n > cols as usize {
                assert!(state.move_selection(ExposeNavDirection::Down));
                assert_eq!(
                    geometry.cells[0].cell[0], geometry.cells[state.selected].cell[0],
                    "{n} tags: Down must stay in the column"
                );
            }
        }
    }

    #[test]
    fn commit_mask_rejects_an_out_of_range_selection() {
        assert_eq!(commit_mask(0, 9), Some(1));
        assert_eq!(commit_mask(3, 9), Some(0b1000));
        assert_eq!(commit_mask(30, 31), Some(1 << 30));
        // A config reload shrank the list under the open panel.
        assert_eq!(commit_mask(9, 9), None);
        assert_eq!(commit_mask(2, 0), None);
        assert_eq!(commit_mask(40, 64), None);
    }
}
