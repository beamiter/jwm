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
        let owner_tag = self
            .state
            .monitors
            .get(sel_mon_key)
            .and_then(|monitor| monitor.pertag.as_ref())
            .map(|pertag| pertag.cur_tag)
            .ok_or("No pertag")?;

        self.prepare_system_ui(backend, "layout picker", SystemUiPointerGrab::Buttons)?;
        let mut picker = LayoutPickerState::new(&current);
        picker.owner = Some((sel_mon_key, owner_tag));
        picker.origin_previous = self.layout_picker_previous(sel_mon_key, owner_tag);
        picker.preview_previous = picker.origin_previous.clone();
        let show_bar = self
            .state
            .monitors
            .get(sel_mon_key)
            .and_then(|monitor| monitor.pertag.as_ref())
            .and_then(|pertag| pertag.show_bars.get(owner_tag))
            .copied();
        picker.origin_show_bar = show_bar;
        picker.preview_show_bar = show_bar;
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

    /// Read the same visible, non-floating client set as the tiling pipeline.
    /// This is read-only: preview hints must never apply borders or size hints.
    pub(crate) fn refresh_layout_picker_context(&mut self) {
        let Some((monitor_key, _)) = self
            .features
            .system_ui
            .layout_picker()
            .and_then(|p| p.owner)
        else {
            return;
        };
        let Some(area) = self.monitor_work_area(monitor_key) else {
            return;
        };
        let Some(monitor) = self.state.monitors.get(monitor_key) else {
            return;
        };
        // Count all eligible windows without allocating one entry per window.
        // Keep only the bounded geometry sample used by the thumbnail engine.
        let mut count = 0;
        let mut clients = Vec::new();
        for &key in self.get_monitor_clients(monitor_key) {
            let Some(client) = self.state.clients.get(key) else {
                continue;
            };
            if client.state.is_floating || !self.is_client_visible_on_monitor(key, monitor_key) {
                continue;
            }
            count += 1;
            if clients.len() < crate::core::layout::MAX_PREVIEW_CLIENTS {
                clients.push((client.state.client_fact, client.geometry.border_w));
            }
        }
        let context = crate::jwm::features::layout_picker::PreviewContext {
            count,
            width: area.w.max(1),
            height: area.h.max(1),
            n_master: monitor.layout.n_master,
            m_fact: monitor.layout.m_fact,
            gap: monitor.layout.gap,
            clients,
        };
        if let Some(picker) = self.features.system_ui.layout_picker_mut() {
            picker.update_context(context);
        }
    }

    /// Step an already-open picker. Returns false when none is open.
    fn step_layout_picker(
        &mut self,
        backend: &mut dyn Backend,
        delta: i32,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        if self.dismiss_stale_layout_picker(backend) {
            return Ok(false);
        }
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

    /// Home / End on an open picker. Returns false when none is open.
    pub(crate) fn jump_layout_picker_edge(
        &mut self,
        backend: &mut dyn Backend,
        to_end: bool,
    ) -> bool {
        if self.dismiss_stale_layout_picker(backend) {
            return true;
        }
        let Some(picker) = self.features.system_ui.layout_picker_mut() else {
            return false;
        };
        let Some(target) = picker.jump_edge(to_end) else {
            return true;
        };
        self.apply_picked_layout(backend, target);
        self.sync_system_ui(backend);
        true
    }

    /// Page Up / Down on an open picker. Returns false when none is open.
    pub(crate) fn page_layout_picker(&mut self, backend: &mut dyn Backend, direction: i32) -> bool {
        if self.dismiss_stale_layout_picker(backend) {
            return true;
        }
        let Some(picker) = self.features.system_ui.layout_picker_mut() else {
            return false;
        };
        let Some(target) = picker.page(direction) else {
            return true;
        };
        self.apply_picked_layout(backend, target);
        self.sync_system_ui(backend);
        true
    }

    /// Highlight the cell under the pointer, following the mouse.
    pub(crate) fn hover_layout_picker(&mut self, backend: &mut dyn Backend, x: f64, y: f64) {
        if self.dismiss_stale_layout_picker(backend) {
            return;
        }
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
        if self.dismiss_stale_layout_picker(backend) {
            return;
        }
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
        if self.dismiss_stale_layout_picker(backend) {
            return;
        }
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
        let Some(picker) = self.features.system_ui.layout_picker() else {
            return;
        };
        let (owner, preview, origin, origin_bar, preview_bar, origin_previous, preview_previous) = (
            picker.owner,
            picker.selected_layout(),
            picker.origin_layout(),
            picker.origin_show_bar,
            picker.preview_show_bar,
            picker.origin_previous.clone(),
            picker.preview_previous.clone(),
        );
        let Some((monitor_key, tag)) = owner else {
            if preview != origin {
                self.apply_picked_layout(backend, origin);
            }
            return;
        };
        let Some(monitor) = self.state.monitors.get(monitor_key) else {
            return;
        };
        let Some(pertag) = monitor.pertag.as_ref() else {
            return;
        };
        let active = pertag.cur_tag == tag;
        let still_our_preview = pertag
            .lts
            .get(tag)
            .is_some_and(|layout| **layout == *preview)
            && (!active || *monitor.lt == *preview);
        // A newer explicit layout change owns the slot now. Cancelling this
        // older picker must not undo that command or its bar preference.
        let history_unchanged = preview_previous.as_ref().is_none_or(|(live, saved)| {
            pertag
                .prev_lts
                .get(tag)
                .is_some_and(|previous| **previous == *saved)
                && (!active || *monitor.prev_lt == *live)
        });
        if !still_our_preview || !history_unchanged {
            return;
        }
        let current_bar = pertag.show_bars.get(tag).copied();
        let restore_bar = if current_bar == preview_bar {
            origin_bar
        } else {
            current_bar
        };
        if active {
            self.apply_picked_layout_on(backend, monitor_key, origin);
        } else if preview != origin {
            // Restore only the owner's saved slot, never the unrelated live tag.
            let Some(pertag) = self
                .state
                .monitors
                .get_mut(monitor_key)
                .and_then(|monitor| monitor.pertag.as_mut())
            else {
                return;
            };
            if let Some(previous) = pertag.prev_lts.get_mut(tag) {
                *previous = Rc::new(preview.clone());
            }
            if let Some(layout) = pertag.lts.get_mut(tag) {
                *layout = Rc::new(origin.clone());
            }
        }
        // Restoring the origin through set_new_layout records the preview as
        // "previous". Put the opening history back too, even when browsing
        // already returned to the origin and the layout apply was a no-op.
        let mut history_changed = false;
        if let Some((live, saved)) = origin_previous {
            if let Some(monitor) = self.state.monitors.get_mut(monitor_key) {
                if active && *monitor.prev_lt != live {
                    monitor.prev_lt = Rc::new(live);
                    history_changed = true;
                }
                if let Some(previous) = monitor
                    .pertag
                    .as_mut()
                    .and_then(|pertag| pertag.prev_lts.get_mut(tag))
                {
                    if **previous != saved {
                        *previous = Rc::new(saved);
                        history_changed = true;
                    }
                }
            }
        }
        // Fullscreen transitions write show_bars themselves. Undo only that
        // preview side effect, including a trip that ended back at the origin.
        let mut bar_changed = false;
        if let Some(restore_bar) = restore_bar {
            if let Some(show_bar) = self
                .state
                .monitors
                .get_mut(monitor_key)
                .and_then(|monitor| monitor.pertag.as_mut())
                .and_then(|pertag| pertag.show_bars.get_mut(tag))
            {
                bar_changed = *show_bar != restore_bar;
                *show_bar = restore_bar;
            }
        }
        if active && bar_changed {
            if restore_bar == Some(false) {
                if let Some(number) = self.state.monitors.get(monitor_key).map(|m| m.num) {
                    self.clear_minimized_dock_for_monitor(backend, number);
                }
            }
            self.sync_secondary_bar_position(backend, monitor_key);
            self.arrange(backend, Some(monitor_key));
            self.broadcast_visible_window_states_on_monitor(backend, monitor_key);
        }
        if active && (bar_changed || history_changed) {
            self.broadcast_monitor_bar_ipc(backend, monitor_key);
        }
        if preview != origin || bar_changed || history_changed {
            self.mark_layout_dirty();
        }
    }

    /// A live preview belongs to its opening monitor/tag, not whichever
    /// context an IPC activation, workspace change or hotplug selected later.
    pub(crate) fn dismiss_stale_layout_picker(&mut self, backend: &mut dyn Backend) -> bool {
        let Some(picker) = self.features.system_ui.layout_picker() else {
            return false;
        };
        let stale = picker.owner.is_some_and(|(monitor_key, tag)| {
            self.state.sel_mon != Some(monitor_key)
                || self.state.monitors.get(monitor_key).is_none_or(|monitor| {
                    monitor.pertag.as_ref().is_none_or(|pertag| {
                        pertag.cur_tag != tag
                            || *monitor.lt != *picker.selected_layout()
                            || pertag
                                .lts
                                .get(tag)
                                .is_none_or(|layout| **layout != *picker.selected_layout())
                            || picker
                                .preview_previous
                                .as_ref()
                                .is_some_and(|(live, saved)| {
                                    *monitor.prev_lt != *live
                                        || pertag
                                            .prev_lts
                                            .get(tag)
                                            .is_none_or(|previous| **previous != *saved)
                                })
                    })
                })
        });
        if stale {
            self.cancel_layout_picker(backend);
        }
        stale
    }

    /// Commit on the user's behalf once they stop interacting.
    pub(crate) fn tick_layout_picker(&mut self, backend: &mut dyn Backend, now: Instant) {
        if self.dismiss_stale_layout_picker(backend) {
            return;
        }
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

    /// The cell a global pointer position sits on, if any. The hit-test reads
    /// the same geometry the compositors draw: `strip_geometry` over the
    /// viewport `sync_system_ui` pushes with the overlay and the picker's own
    /// layout count, with the highlighted cell lifted the way it is painted
    /// (`layout_strip::presented_cell`). Passing the selection is what keeps
    /// a press on the lifted card from being tested against the film's
    /// resting place twelve percent smaller.
    fn layout_picker_cell_at(&self, x: f64, y: f64) -> Option<usize> {
        let picker = self.features.system_ui.layout_picker()?;
        let geometry =
            layout_strip::strip_geometry(self.system_ui_viewport().rect(), picker.layouts.len());
        layout_strip::cell_at(&geometry, Some(picker.selected), x as f32, y as f32)
    }

    /// Switch the selected monitor to `layout`. Re-selecting the highlighted
    /// cell is a no-op, same as [`Jwm::setlayout`] — the early return here
    /// also skips the fullscreen/arrange side effects of a real change.
    fn apply_picked_layout(&mut self, backend: &mut dyn Backend, layout: &'static LayoutEnum) {
        let owner = self
            .features
            .system_ui
            .layout_picker()
            .and_then(|picker| picker.owner);
        let Some(sel_mon_key) = owner.map(|(monitor, _)| monitor).or(self.state.sel_mon) else {
            return;
        };
        let current_bar = owner.and_then(|(monitor, tag)| {
            self.state
                .monitors
                .get(monitor)
                .and_then(|monitor| monitor.pertag.as_ref())
                .and_then(|pertag| pertag.show_bars.get(tag))
                .copied()
        });
        if let Some(picker) = self.features.system_ui.layout_picker_mut() {
            if current_bar != picker.preview_show_bar {
                // A bar toggle between previews becomes the preference to
                // restore, rather than being lost by the next fullscreen step.
                picker.origin_show_bar = current_bar;
            }
        }
        self.apply_picked_layout_on(backend, sel_mon_key, layout);
        let preview_bar = owner.and_then(|(monitor, tag)| {
            self.state
                .monitors
                .get(monitor)
                .and_then(|monitor| monitor.pertag.as_ref())
                .and_then(|pertag| pertag.show_bars.get(tag))
                .copied()
        });
        let preview_previous =
            owner.and_then(|(monitor, tag)| self.layout_picker_previous(monitor, tag));
        if let Some(picker) = self.features.system_ui.layout_picker_mut() {
            picker.preview_show_bar = preview_bar;
            picker.preview_previous = preview_previous;
        }
    }

    fn layout_picker_previous(
        &self,
        monitor_key: crate::core::models::MonitorKey,
        tag: usize,
    ) -> Option<(LayoutEnum, LayoutEnum)> {
        let monitor = self.state.monitors.get(monitor_key)?;
        let saved = monitor.pertag.as_ref()?.prev_lts.get(tag)?;
        Some((monitor.prev_lt.as_ref().clone(), saved.as_ref().clone()))
    }

    fn apply_picked_layout_on(
        &mut self,
        backend: &mut dyn Backend,
        sel_mon_key: crate::core::models::MonitorKey,
        layout: &'static LayoutEnum,
    ) {
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
