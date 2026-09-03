//! The tags overview: a GNOME-style grid showing every tag of the selected
//! monitor at once.
//!
//! One cell per tag, each carrying the tag's number and a line-drawn
//! wireframe of its windows. The panel itself is drawn by the compositors
//! from [`crate::backend::compositor_common::tags_grid`]; everything that
//! decides *what* it shows and what a keystroke or a click means lives here.
//!
//! Unlike the layout picker nothing is previewed: the desktop behind the
//! panel never changes while the grid is up, so cancelling is simply closing
//! and confirming is an ordinary tag jump (`Jwm::view`).
//!
//! The pointer gesture is a press/release pair. A press on a cell only arms
//! a [`PendingCellPress`]; the release settles it: released on the same cell
//! it is the click's tag jump, released on another cell with a wireframe in
//! hand it drops that window onto the new tag (`Jwm::move_client_to_tag`,
//! the dwm `tag()` semantics — the mask is replaced, not merged), released
//! anywhere else it simply disarms. The compositor-facing [`TagsGridCell`]
//! carries no window identity, so the snapshot keeps a parallel id per
//! outline in [`TagsOverviewState::window_ids`] for the drag to resolve.

use crate::backend::api::{Backend, ExposeNavDirection, LiveTagsCell, TagsGridCell};
use crate::backend::common_define::WindowId;
use crate::backend::compositor_common::expose::{expose_grid_cols, move_expose_selection};
use crate::backend::compositor_common::tags_grid;
use crate::config::CONFIG;
use crate::core::models::MonitorKey;
use crate::core::types::Rect;
use crate::jwm::Jwm;
use crate::jwm::features::SystemUiState;
use crate::jwm::features::toggles::{SystemUiPointerGrab, configured_feature_toggle_allowed};
use crate::jwm::types::WMArgEnum;

/// One client's slice of the snapshot, narrow enough for the cell builder to
/// be tested without a window manager.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TagClientFrame {
    /// Raw window id. The compositor-facing cell keeps only rectangles, so
    /// the drag gesture resolves which window a wireframe is through this.
    pub win: u64,
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
    /// Demanding attention (`ClientState::is_urgent`): the tag gets the
    /// attention dot, following the status bar's urgent mask.
    pub urgent: bool,
    /// The window's *visible* rectangle in global coordinates. Callers must
    /// resolve parked windows through `ClientGeometry::hidden_restore_rect`
    /// before filling this in; the builder takes the rect verbatim.
    pub rect: [i32; 4],
}

/// A button press on a cell, held until its release settles what it meant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingCellPress {
    /// The cell the press landed on.
    pub cell: usize,
    /// The wireframe hit under the press, as the window's raw id — resolved
    /// through [`TagsOverviewState::window_ids`]. `None` on an empty cell or
    /// the gaps between wireframes: such a press can only ever settle as the
    /// click's view jump, never as a drag.
    pub window: Option<u64>,
    /// The press crossed into another cell with a window in hand: the
    /// gesture is a drag, and its release drops the window on the cell under
    /// it.
    pub dragging: bool,
}

/// The open panel's state: a snapshot of the monitor's tags plus the
/// keyboard highlight and an in-flight pointer press.
#[derive(Debug, Clone)]
pub struct TagsOverviewState {
    /// One cell per tag, in tag order.
    pub cells: Vec<TagsGridCell>,
    /// Parallel to `cells[*].windows`: the raw window id behind every
    /// outline, in the same back-to-front order. The API's [`TagsGridCell`]
    /// carries no identity, so the drag gesture keeps its map here, on the
    /// WM side of the snapshot.
    pub window_ids: Vec<Vec<u64>>,
    /// Columns of the grid the cells are walked (and drawn) in.
    pub cols: u32,
    /// Highlighted cell: a tag index, stable across rebuilds.
    pub selected: usize,
    /// The pointer press waiting for its release, if one is down.
    pub pending: Option<PendingCellPress>,
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
        let (cells, window_ids) = build_cells_with_ids(clients, active_tags, work, tags_length);
        let cols = grid_cols(cells.len(), work);
        let selected = if active_tags == 0 || active_tags == u32::MAX {
            0
        } else {
            (active_tags.trailing_zeros() as usize).min(cells.len().saturating_sub(1))
        };
        Self {
            cells,
            window_ids,
            cols,
            selected,
            pending: None,
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

/// What a resolved pointer hit means for a press on the open overview.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TagsOverviewPress {
    /// On a cell: arm it. What the gesture settles into — the click's view
    /// jump or a wireframe drag's window move — is the release's call.
    Cell(usize),
    /// On the dimmed desktop around the panel: the mouse's Escape, answered
    /// on the press itself so the panel is gone before the button comes up.
    Cancel,
    /// Inside the panel but on no cell: swallow the press.
    Keep,
}

/// Decide a press from its hit-test. A cell arms a pending press; the modal
/// scrim cancels immediately; the panel's dead space — the title, caption
/// and hint bands and the gaps between cells — swallows the press, so a
/// click can never fall through to the desktop the panel is modal over.
pub fn plan_press(hit: Option<usize>, in_panel: bool) -> TagsOverviewPress {
    match (hit, in_panel) {
        (Some(index), _) => TagsOverviewPress::Cell(index),
        (None, true) => TagsOverviewPress::Keep,
        (None, false) => TagsOverviewPress::Cancel,
    }
}

/// What a release settles the pending press into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TagsOverviewRelease {
    /// Press and release on one cell: the click — jump to that tag and
    /// close, exactly what the cell committed on press before the grid
    /// learned to drag.
    View(usize),
    /// A wireframe dragged onto another cell: move its window to that tag.
    MoveToTag {
        /// Raw window id, as [`PendingCellPress::window`] recorded it.
        window: u64,
        /// Target cell, i.e. target tag index.
        target: usize,
    },
    /// Landing nowhere meaningful — the scrim, the panel's dead space, or a
    /// cell crossed without a drag in flight: just disarm the press. In
    /// particular a release on the scrim never cancels the panel; cancelling
    /// is the scrim *press*'s job, so a misdrag that started on a cell stays
    /// harmless.
    Disarm,
}

/// Settle a release from the press it answers and the cell it lands on.
/// Same cell is always the click, whatever the press held; another cell is
/// a window drop only once the gesture actually became a drag (a window in
/// hand that crossed the cell boundary).
pub fn plan_release(pending: PendingCellPress, release_cell: Option<usize>) -> TagsOverviewRelease {
    match release_cell {
        Some(cell) if cell == pending.cell => TagsOverviewRelease::View(cell),
        Some(target) if pending.dragging => match pending.window {
            Some(window) => TagsOverviewRelease::MoveToTag { window, target },
            // A drag by definition holds a window; the arm stays total.
            None => TagsOverviewRelease::Disarm,
        },
        _ => TagsOverviewRelease::Disarm,
    }
}

/// Build one cell per tag from the snapshot.
///
/// Occupied follows the status bar's `calculate_tag_masks`: any window on
/// the tag counts, minimized and swallowed ones included, while windows
/// floating above the tag axis — sticky ones, or clients spanning every tag
/// — draw everywhere but mark nothing occupied. Urgent follows the exact
/// same rule, so a tag's attention dot can never appear on a cell that does
/// not also read occupied. Outlines are normalized to the work area and
/// clipped to `0.0..=1.0`: a floating window may hang off the area, but its
/// wireframe may not poke through the cell.
pub fn build_cells(
    clients: &[TagClientFrame],
    active_tags: u32,
    work: [i32; 4],
    tags_length: usize,
) -> Vec<TagsGridCell> {
    build_cells_with_ids(clients, active_tags, work, tags_length).0
}

/// [`build_cells`] plus, parallel to each cell's `windows`, the raw window
/// id behind every outline. The two vectors are pushed together, so
/// `window_ids[i][j]` is always the window `cells[i].windows[j]` draws.
fn build_cells_with_ids(
    clients: &[TagClientFrame],
    active_tags: u32,
    work: [i32; 4],
    tags_length: usize,
) -> (Vec<TagsGridCell>, Vec<Vec<u64>>) {
    let tags_length = tags_length.clamp(1, 31);
    let [wx, wy, ww, wh] = work;
    let full_mask = (1u32 << tags_length) - 1;

    let mut cells: Vec<TagsGridCell> = (0..tags_length)
        .map(|tag_index| TagsGridCell {
            tag_index,
            windows: Vec::new(),
            occupied: false,
            urgent: false,
            active: (active_tags >> tag_index) & 1 != 0,
        })
        .collect();
    let mut window_ids: Vec<Vec<u64>> = (0..tags_length).map(|_| Vec::new()).collect();

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
                // Like the status bar's urgent mask: the window's hidden or
                // swallowed state is irrelevant — a minimized window still
                // demands attention.
                cell.urgent |= client.urgent;
            }
            if !draws_outline {
                continue;
            }
            if let Some(outline) = normalize_rect(client.rect, wx, wy, ww, wh) {
                cell.windows.push(outline);
                window_ids[index].push(client.win);
            }
        }
    }
    (cells, window_ids)
}

/// The payload that upgrades the on-screen tag's cell to live window
/// content: the first active cell — a multi-tag view's primary tag, matching
/// the selection's pre-select — with each outline's window id paired back to
/// the normalized rect its wireframe carries, so a live thumbnail and its
/// wireframe can never disagree about placement. `None` when no tag is on
/// screen; every other cell keeps its wireframes, because a parked window's
/// texture only holds the stale image from before it left the screen.
pub fn live_cell(overview: &TagsOverviewState) -> Option<LiveTagsCell> {
    let cell = overview.cells.iter().position(|cell| cell.active)?;
    let outlines = &overview.cells[cell].windows;
    let ids = overview.window_ids.get(cell)?;
    let windows = ids
        .iter()
        .copied()
        .zip(outlines.iter().copied())
        .map(|(id, rect)| (WindowId::from_raw(id), rect))
        .collect();
    Some(LiveTagsCell { cell, windows })
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
        // Buttons and motion: the grid follows the pointer, so its grab needs
        // the expose mask rather than the click-only default.
        self.prepare_system_ui(
            backend,
            "tags overview",
            SystemUiPointerGrab::ButtonsAndMotion,
        )?;
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

    /// Highlight the cell under the pointer. Mouse and keyboard share the one
    /// `selected`, so a hover after an arrow walk continues from where the
    /// keys left off; a miss between cells leaves the selection alone. A
    /// press holding a window that crosses into another cell becomes a drag:
    /// the hover keeps highlighting the drop target, and the release drops
    /// the window on it.
    pub(crate) fn hover_tags_overview(&mut self, backend: &mut dyn Backend, x: f64, y: f64) {
        let Some(index) = self.tags_overview_cell_at(x, y) else {
            return;
        };
        let mut moved = false;
        {
            let Some(overview) = self.features.system_ui.tags_overview_mut() else {
                return;
            };
            if let Some(pending) = &mut overview.pending {
                if pending.window.is_some() && index != pending.cell {
                    pending.dragging = true;
                }
            }
            if overview.selected != index {
                overview.selected = index;
                moved = true;
            }
        }
        if moved {
            self.sync_system_ui(backend);
        }
    }

    /// A press on the grid: on a cell it arms a [`PendingCellPress`] whose
    /// release decides between the click's view jump and a wireframe drag's
    /// window move; on the scrim it cancels outright; the panel's dead space
    /// swallows it — a press never reaches the desktop underneath.
    pub(crate) fn press_tags_overview(&mut self, backend: &mut dyn Backend, x: f64, y: f64) {
        match self.tags_overview_press_target(x, y) {
            Some(TagsOverviewPress::Cell(index)) => {
                let window = self.tags_overview_window_at(index, x, y);
                if let Some(overview) = self.features.system_ui.tags_overview_mut() {
                    overview.pending = Some(PendingCellPress {
                        cell: index,
                        window,
                        dragging: false,
                    });
                }
            }
            Some(TagsOverviewPress::Cancel) => self.cancel_tags_overview(backend),
            Some(TagsOverviewPress::Keep) | None => {}
        }
    }

    /// The release answering a pending press: on the press's own cell it is
    /// the click (select that tag, commit, close); on another cell with a
    /// drag in flight it drops the window on that tag through
    /// [`Jwm::move_client_to_tag`] — the same path `Mod1+Shift+数字` takes —
    /// and keeps the panel open with refreshed cells; anywhere else it just
    /// disarms. The pointer grab delivers releases without the button
    /// number, so the first release after the button-1 press settles the
    /// gesture.
    pub(crate) fn release_tags_overview(
        &mut self,
        backend: &mut dyn Backend,
        x: f64,
        y: f64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(pending) = self
            .features
            .system_ui
            .tags_overview()
            .and_then(|overview| overview.pending)
        else {
            return Ok(());
        };
        let release_cell = self.tags_overview_cell_at(x, y);
        match plan_release(pending, release_cell) {
            TagsOverviewRelease::View(index) => {
                // The click, settled: the digit jump's path — select, view,
                // close. Closing drops the state, pending press included.
                self.jump_tags_overview(backend, index)?;
            }
            TagsOverviewRelease::MoveToTag { window, target } => {
                if let Some(overview) = self.features.system_ui.tags_overview_mut() {
                    overview.pending = None;
                }
                let mask = commit_mask(target, CONFIG.load().tags_length());
                let client_key = self.wintoclient(WindowId::from_raw(window));
                if let (Some(mask), Some(client_key)) = (mask, client_key) {
                    log::info!(
                        "[tags_overview] dragging window {window} to tag {}",
                        target + 1
                    );
                    // The arrange inside rebuilds the open panel's cells, so
                    // the drop is visible without the panel closing.
                    self.move_client_to_tag(backend, client_key, mask)?;
                }
            }
            TagsOverviewRelease::Disarm => {
                if let Some(overview) = self.features.system_ui.tags_overview_mut() {
                    overview.pending = None;
                }
            }
        }
        Ok(())
    }

    /// The cell a global pointer position sits on, if any. The hit-test reads
    /// the same geometry the compositors draw: `grid_geometry` over the
    /// viewport `sync_system_ui` pushes with the overlay and the state's own
    /// cell count and column count.
    fn tags_overview_cell_at(&self, x: f64, y: f64) -> Option<usize> {
        let overview = self.features.system_ui.tags_overview()?;
        let geometry = tags_grid::grid_geometry(
            self.system_ui_viewport().rect(),
            overview.cells.len(),
            overview.cols,
        );
        tags_grid::cell_at(&geometry, x as f32, y as f32)
    }

    /// The window whose wireframe a point inside `cell_index` lands on, if
    /// any: the drawn topmost outline under the point, resolved to its raw
    /// id through the snapshot's parallel [`TagsOverviewState::window_ids`].
    fn tags_overview_window_at(&self, cell_index: usize, x: f64, y: f64) -> Option<u64> {
        let overview = self.features.system_ui.tags_overview()?;
        let geometry = tags_grid::grid_geometry(
            self.system_ui_viewport().rect(),
            overview.cells.len(),
            overview.cols,
        );
        let grid_cell = geometry.cells.get(cell_index)?;
        let outlines = &overview.cells.get(cell_index)?.windows;
        let hit = tags_grid::frame_window_at(grid_cell.frame, outlines, x as f32, y as f32)?;
        overview.window_ids.get(cell_index)?.get(hit).copied()
    }

    /// A press's full hit-test: the cell under the point and whether the
    /// panel card contains it, resolved in one pass of the drawn geometry.
    fn tags_overview_press_target(&self, x: f64, y: f64) -> Option<TagsOverviewPress> {
        let overview = self.features.system_ui.tags_overview()?;
        let geometry = tags_grid::grid_geometry(
            self.system_ui_viewport().rect(),
            overview.cells.len(),
            overview.cols,
        );
        let (x, y) = (x as f32, y as f32);
        Some(plan_press(
            tags_grid::cell_at(&geometry, x, y),
            tags_grid::panel_contains(&geometry, x, y),
        ))
    }

    /// Rebuild the cells after window changes while the panel is open (the
    /// `arrange` tail calls here). The selection is a tag index, so a rebuild
    /// cannot shift what it means; a tag list shrunk by a config reload
    /// clamps it instead. A held pointer press survives the rebuild — an
    /// unrelated arrange must not eat a gesture in flight — clamped like the
    /// selection in case its cell went away.
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
            let pending = overview
                .pending
                .filter(|pending| pending.cell < rebuilt.cells.len());
            *overview = rebuilt;
            overview.selected = selected.min(overview.cells.len().saturating_sub(1));
            overview.pending = pending;
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
                    win: client.win.raw(),
                    tags: client.state.tags,
                    sticky: client.state.is_sticky,
                    minimized: client.state.is_hidden,
                    swallowed: client.state.is_swallowed,
                    urgent: client.state.is_urgent,
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
            win: 0x1000,
            tags,
            sticky: false,
            minimized: false,
            swallowed: false,
            urgent: false,
            rect,
        }
    }

    fn win_frame(win: u64, tags: u32, rect: [i32; 4]) -> TagClientFrame {
        TagClientFrame {
            win,
            ..frame(tags, rect)
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
    fn an_urgent_window_marks_every_tag_it_sits_on() {
        // The status bar's urgent mask ORs the window's whole effective mask;
        // the grid does the same per cell.
        let urgent = TagClientFrame {
            urgent: true,
            ..frame(0b101, [100, 100, 400, 300])
        };
        let cells = build_cells(&[urgent], 0b001, WORK, 9);
        assert!(cells[0].urgent);
        assert!(!cells[1].urgent);
        assert!(cells[2].urgent);
        // The dot never appears on a tag the window only shares with a
        // calmer sibling.
        let calm = frame(0b010, [200, 200, 400, 300]);
        let cells = build_cells(&[urgent, calm], 0b001, WORK, 9);
        assert!(cells[1].occupied);
        assert!(!cells[1].urgent);
    }

    #[test]
    fn urgent_never_appears_without_occupied() {
        // Sticky and all-tags urgent windows float above the tag axis: like
        // the status bar's full-mask skip, they mark no tag.
        let sticky = TagClientFrame {
            tags: 0b001,
            sticky: true,
            urgent: true,
            ..frame(0, [100, 100, 400, 300])
        };
        let all_tags = TagClientFrame {
            urgent: true,
            ..frame(0x1ff, [200, 200, 400, 300])
        };
        let cells = build_cells(&[sticky, all_tags], 0b001, WORK, 9);
        for (index, cell) in cells.iter().enumerate() {
            assert!(!cell.urgent, "floating urgent window marked tag {index}");
            assert!(!cell.occupied);
        }
        // The invariant holds in general: urgent is a subset of occupied.
        let mixed = build_cells(
            &[
                TagClientFrame {
                    urgent: true,
                    ..frame(0b110, [100, 100, 400, 300])
                },
                frame(0b011, [200, 200, 400, 300]),
            ],
            0b001,
            WORK,
            9,
        );
        for cell in &mixed {
            assert!(!cell.urgent || cell.occupied);
        }
    }

    #[test]
    fn a_minimized_urgent_window_still_marks_its_tag() {
        // The status bar's urgent mask never looks at the hidden state, so
        // minimizing must not silence the signal — the dot is exactly how a
        // parked-away window stays visible.
        let minimized = TagClientFrame {
            minimized: true,
            urgent: true,
            ..frame(0b010, [100, 100, 400, 300])
        };
        let swallowed = TagClientFrame {
            swallowed: true,
            urgent: true,
            ..frame(0b100, [200, 200, 400, 300])
        };
        let cells = build_cells(&[minimized, swallowed], 0b001, WORK, 9);
        assert!(cells[1].urgent);
        assert!(cells[1].windows.is_empty());
        assert!(cells[2].urgent);
        assert!(cells[2].windows.is_empty());
        assert!(!cells[0].urgent);
    }

    #[test]
    fn urgency_clears_with_the_window_state() {
        // Once no window on the tag demands attention the dot goes away;
        // nothing latches.
        let urgent = TagClientFrame {
            urgent: true,
            ..frame(0b001, [100, 100, 400, 300])
        };
        let calmed = TagClientFrame {
            urgent: false,
            ..urgent
        };
        let cells = build_cells(&[urgent], 0b001, WORK, 9);
        assert!(cells[0].urgent);
        let cells = build_cells(&[calmed], 0b001, WORK, 9);
        assert!(cells[0].occupied);
        assert!(!cells[0].urgent);
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

    #[test]
    fn a_press_arms_a_cell_cancels_the_scrim_and_dies_on_the_panel() {
        assert_eq!(plan_press(Some(2), true), TagsOverviewPress::Cell(2));
        // A cell never sits outside the panel, but the arm stays total.
        assert_eq!(plan_press(Some(2), false), TagsOverviewPress::Cell(2));
        assert_eq!(plan_press(None, false), TagsOverviewPress::Cancel);
        assert_eq!(plan_press(None, true), TagsOverviewPress::Keep);
    }

    #[test]
    fn every_outline_has_its_window_id_in_the_same_back_to_front_order() {
        let (cells, window_ids) = build_cells_with_ids(
            &[
                win_frame(11, 0b001, [100, 100, 400, 300]),
                win_frame(22, 0b001, [200, 200, 400, 300]),
                // Minimized: occupies, but draws and identifies nothing.
                TagClientFrame {
                    minimized: true,
                    ..win_frame(33, 0b001, [300, 300, 400, 300])
                },
                win_frame(44, 0b101, [400, 400, 400, 300]),
            ],
            0b001,
            WORK,
            9,
        );
        for (index, cell) in cells.iter().enumerate() {
            assert_eq!(
                cell.windows.len(),
                window_ids[index].len(),
                "cell {index}: ids must stay parallel to outlines"
            );
        }
        assert_eq!(window_ids[0], vec![11, 22, 44]);
        assert_eq!(window_ids[2], vec![44]);
    }

    #[test]
    fn the_snapshot_starts_without_a_pending_press() {
        let state =
            TagsOverviewState::new(&[win_frame(7, 0b001, [0, 0, 400, 300])], 0b001, WORK, 9);
        assert_eq!(state.window_ids[0], vec![7]);
        assert_eq!(state.pending, None);
    }

    #[test]
    fn a_release_on_the_presss_own_cell_is_the_click() {
        // Same cell is the view jump whether or not the press held a
        // wireframe, and even if the gesture dragged away and came back.
        for pending in [
            PendingCellPress {
                cell: 2,
                window: None,
                dragging: false,
            },
            PendingCellPress {
                cell: 2,
                window: Some(9),
                dragging: false,
            },
            PendingCellPress {
                cell: 2,
                window: Some(9),
                dragging: true,
            },
        ] {
            assert_eq!(
                plan_release(pending, Some(2)),
                TagsOverviewRelease::View(2),
                "{pending:?}"
            );
        }
    }

    #[test]
    fn a_drag_released_on_another_cell_moves_the_window() {
        let pending = PendingCellPress {
            cell: 0,
            window: Some(9),
            dragging: true,
        };
        assert_eq!(
            plan_release(pending, Some(4)),
            TagsOverviewRelease::MoveToTag {
                window: 9,
                target: 4
            }
        );
    }

    #[test]
    fn a_release_off_every_cell_only_disarms() {
        let drag = PendingCellPress {
            cell: 0,
            window: Some(9),
            dragging: true,
        };
        // The scrim — and the panel's dead space — commit nothing.
        assert_eq!(plan_release(drag, None), TagsOverviewRelease::Disarm);
        // A press that never became a drag settles nothing off its own cell:
        // without a wireframe in hand there is nothing to drop, and a bare
        // press-release straddling two cells is not a gesture the grid sells.
        let plain = PendingCellPress {
            cell: 0,
            window: None,
            dragging: false,
        };
        assert_eq!(plan_release(plain, Some(4)), TagsOverviewRelease::Disarm);
        let held_window_never_moved = PendingCellPress {
            cell: 0,
            window: Some(9),
            dragging: false,
        };
        assert_eq!(
            plan_release(held_window_never_moved, Some(4)),
            TagsOverviewRelease::Disarm,
            "a window drop requires the boundary-crossing motion"
        );
    }

    #[test]
    fn the_live_cell_pairs_each_outline_with_its_window_id() {
        let state = TagsOverviewState::new(
            &[
                win_frame(0x100, 0b001, [960, 540, 960, 540]),
                win_frame(0x200, 0b001, [0, 0, 480, 270]),
                win_frame(0x300, 0b010, [100, 100, 400, 300]),
            ],
            0b001,
            WORK,
            9,
        );
        let live = live_cell(&state).expect("the on-screen tag goes live");
        assert_eq!(live.cell, 0);
        // The payload reuses the wireframe's own rects, in the same order.
        let rects: Vec<[f32; 4]> = live.windows.iter().map(|(_, rect)| *rect).collect();
        assert_eq!(rects, state.cells[0].windows);
        let ids: Vec<u64> = live.windows.iter().map(|(id, _)| id.raw()).collect();
        assert_eq!(ids, state.window_ids[0]);
    }

    #[test]
    fn the_live_cell_is_the_lowest_active_tag_not_the_highlight() {
        // A multi-tag view live-draws its primary tag; the keyboard
        // highlight — which the user can move anywhere — never decides it.
        let mut state =
            TagsOverviewState::new(&[win_frame(0x100, 0b110, [0, 0, 960, 540])], 0b110, WORK, 9);
        state.selected = 2;
        let live = live_cell(&state).expect("a visible tag is live");
        assert_eq!(live.cell, 1);
        assert_eq!(live.windows.len(), 1);
    }

    #[test]
    fn nothing_visible_means_no_live_cell() {
        let state =
            TagsOverviewState::new(&[win_frame(0x100, 0b001, [0, 0, 960, 540])], 0, WORK, 9);
        assert_eq!(live_cell(&state), None);
    }

    #[test]
    fn the_live_cell_skips_outlineless_windows_like_the_wireframe_does() {
        let state = TagsOverviewState::new(
            &[
                TagClientFrame {
                    minimized: true,
                    ..win_frame(0x100, 0b001, [0, 0, 960, 540])
                },
                win_frame(0x200, 0b001, [100, 100, 400, 300]),
            ],
            0b001,
            WORK,
            9,
        );
        let live = live_cell(&state).expect("the on-screen tag goes live");
        assert_eq!(live.windows.len(), 1);
        assert_eq!(live.windows[0].0, WindowId::from_raw(0x200));
        assert!(
            state.cells[0].occupied,
            "the minimized window still occupies"
        );
    }
}
