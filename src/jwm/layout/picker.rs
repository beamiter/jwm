//! The interactive layout picker: opening it, driving it, committing it.
//!
//! The panel itself is drawn by the compositors from
//! [`crate::backend::compositor_common::layout_strip`]; everything that
//! decides *what* it shows and what a keystroke means lives here.
//!
//! Browsing applies each layout for real, so the strip is a caption over a
//! live desktop rather than a preview of one. That is also why cancelling has
//! to put the original layout back.

use crate::backend::api::Backend;
use crate::backend::compositor_common::layout_strip;
use crate::config::CONFIG;
use crate::core::layout::LayoutEnum;
use crate::jwm::Jwm;
use crate::jwm::features::SystemUiState;
use crate::jwm::features::layout_picker::LayoutPickerState;
use crate::jwm::features::toggles::SystemUiPointerGrab;
use crate::jwm::types::WMArgEnum;
use log::info;
use std::rc::Rc;
use std::time::Instant;

impl Jwm {
    /// Open the picker, or step it if it is already open.
    ///
    /// Bound as an action in its own right; `cyclelayout` also routes here so
    /// that the familiar cycle key grows the panel without changing what
    /// repeated presses do.
    pub(crate) fn layout_picker(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let delta = match arg {
            WMArgEnum::Int(i) => *i,
            _ => 1,
        };
        if self.step_layout_picker(backend, delta)? {
            return Ok(());
        }
        // Opening on a step of 0 is "show me where I am"; any other step also
        // moves, so a tap of the cycle key still switches layouts.
        self.open_layout_picker(backend, delta)
    }

    /// Whether the film-strip picker should stand in for a silent cycle.
    fn layout_picker_enabled(&self, _backend: &dyn Backend) -> bool {
        CONFIG.load().behavior().layout_picker
            && !self.features.system_ui.is_active()
            && self.state.sel_mon.is_some()
    }

    /// Route a plain layout cycle through the picker when it is available.
    /// Returns false when the caller should just cycle the layout itself.
    pub(crate) fn cycle_through_layout_picker(
        &mut self,
        backend: &mut dyn Backend,
        delta: i32,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        if self.step_layout_picker(backend, delta)? {
            return Ok(true);
        }
        if !self.layout_picker_enabled(backend) {
            return Ok(false);
        }
        match self.open_layout_picker(backend, delta) {
            Ok(()) => Ok(true),
            Err(error) => {
                // Losing the grab is not a reason to lose the layout switch.
                info!("[layout_picker] could not open the picker: {error}");
                Ok(false)
            }
        }
    }

    fn open_layout_picker(
        &mut self,
        backend: &mut dyn Backend,
        delta: i32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let sel_mon_key = self.state.sel_mon.ok_or("No selected monitor")?;
        let current = self.current_selected_layout(sel_mon_key)?;

        self.prepare_system_ui(backend, "layout picker", SystemUiPointerGrab::Buttons)?;
        let mut picker = LayoutPickerState::new(&current);
        let target = if delta == 0 {
            picker.selected_layout()
        } else {
            picker.step(delta)
        };
        self.features.system_ui = SystemUiState::LayoutPicker(picker);
        self.apply_picked_layout(backend, target);
        self.sync_system_ui(backend);
        Ok(())
    }

    /// Step an already-open picker. Returns false when none is open.
    fn step_layout_picker(
        &mut self,
        backend: &mut dyn Backend,
        delta: i32,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(picker) = self.features.system_ui.layout_picker_mut() else {
            return Ok(false);
        };
        let target = if delta == 0 {
            picker.touch();
            picker.selected_layout()
        } else {
            picker.step(delta)
        };
        self.apply_picked_layout(backend, target);
        self.sync_system_ui(backend);
        Ok(true)
    }

    /// Highlight the cell under the pointer, following the mouse.
    pub(crate) fn hover_layout_picker(&mut self, backend: &mut dyn Backend, x: f64, y: f64) {
        let Some(index) = self.layout_picker_cell_at(x, y) else {
            return;
        };
        let Some(picker) = self.features.system_ui.layout_picker_mut() else {
            return;
        };
        let Some(target) = picker.select(index) else {
            // Same cell: the pointer moving is still someone deciding, so the
            // countdown restarts, but nothing needs re-applying or redrawing.
            return;
        };
        self.apply_picked_layout(backend, target);
        self.sync_system_ui(backend);
    }

    /// A click commits. Clicking a cell picks that one first; clicking
    /// anywhere else commits what is highlighted.
    pub(crate) fn click_layout_picker(&mut self, backend: &mut dyn Backend, x: f64, y: f64) {
        if let Some(index) = self.layout_picker_cell_at(x, y) {
            if let Some(picker) = self.features.system_ui.layout_picker_mut() {
                if let Some(target) = picker.select(index) {
                    self.apply_picked_layout(backend, target);
                }
            }
        }
        self.confirm_layout_picker(backend);
    }

    /// Commit the highlighted layout: it is already applied, so this only
    /// takes the panel down.
    pub(crate) fn confirm_layout_picker(&mut self, backend: &mut dyn Backend) {
        if let Some(picker) = self.features.system_ui.layout_picker() {
            info!(
                "[layout_picker] applied {}",
                picker.selected_layout().label()
            );
        }
        self.close_system_ui(backend);
    }

    /// Put back the layout that was current when the picker opened.
    pub(crate) fn cancel_layout_picker(&mut self, backend: &mut dyn Backend) {
        self.restore_layout_picker_origin(backend);
        self.close_system_ui(backend);
    }

    /// The half of a cancel that is not the panel: undo the preview.
    ///
    /// The strip applies each layout as it is browsed, so a picker that goes
    /// away without being confirmed leaves the screen in a layout the user
    /// only looked at. Split out from [`Self::cancel_layout_picker`] because a
    /// hand-over to another shell panel has to undo the preview too, without
    /// dropping the grabs the incoming panel is about to inherit.
    pub(crate) fn restore_layout_picker_origin(&mut self, backend: &mut dyn Backend) {
        let restore =
            self.features.system_ui.layout_picker().and_then(|picker| {
                (picker.selected != picker.origin).then(|| picker.origin_layout())
            });
        if let Some(layout) = restore {
            self.apply_picked_layout(backend, layout);
        }
    }

    /// Commit on the user's behalf once they stop interacting.
    pub(crate) fn tick_layout_picker(&mut self, backend: &mut dyn Backend, now: Instant) {
        let Some(picker) = self.features.system_ui.layout_picker() else {
            return;
        };
        if picker.expired(now) {
            self.confirm_layout_picker(backend);
        } else {
            // The countdown is part of the panel, so it has to be pushed for
            // the bar to advance.
            self.mark_system_ui_dirty();
        }
    }

    /// Time until the picker commits by itself, for the event loop's next
    /// wakeup.
    pub(crate) fn layout_picker_wakeup(&self, now: Instant) -> Option<std::time::Duration> {
        self.features
            .system_ui
            .layout_picker()
            .map(|picker| picker.remaining(now))
    }

    fn layout_picker_cell_at(&self, x: f64, y: f64) -> Option<usize> {
        let picker = self.features.system_ui.layout_picker()?;
        let geometry =
            layout_strip::strip_geometry(self.system_ui_viewport().rect(), picker.layouts.len());
        layout_strip::cell_at(&geometry, x as f32, y as f32)
    }

    /// Switch the selected monitor to `layout`. Re-selecting the highlighted
    /// cell is a no-op, same as [`Jwm::setlayout`] — the early return here
    /// also skips the fullscreen/arrange side effects of a real change.
    fn apply_picked_layout(&mut self, backend: &mut dyn Backend, layout: &'static LayoutEnum) {
        let Some(sel_mon_key) = self.state.sel_mon else {
            return;
        };
        if self
            .current_selected_layout(sel_mon_key)
            .map(|current| *current == *layout)
            .unwrap_or(false)
        {
            return;
        }
        let result = self.apply_layout_change(backend, sel_mon_key, |this, mon_key| {
            let cur_tag = this
                .state
                .monitors
                .get(mon_key)
                .and_then(|m| m.pertag.as_ref())
                .map(|p| p.cur_tag)
                .ok_or("No pertag")?;
            let next = Rc::new(layout.clone());
            this.set_new_layout(mon_key, &next, cur_tag);
            Ok(next)
        });
        if let Err(error) = result {
            info!(
                "[layout_picker] could not apply {}: {error}",
                layout.label()
            );
        }
    }
}
