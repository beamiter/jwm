// Top-level layout dispatch functions

use crate::backend::api::Backend;
use crate::core::layout::LayoutEnum;
use crate::core::models::MonitorKey;
use crate::jwm::Jwm;
use log::{debug, warn};

impl Jwm {
    pub(crate) fn arrangemon(&mut self, backend: &mut dyn Backend, mon_key: MonitorKey) {
        debug!("[arrangemon]");

        let (layout_type, layout_symbol) = if let Some(monitor) = self.state.monitors.get(mon_key) {
            let layout = &monitor.lt;
            (layout.clone(), layout.symbol().to_string())
        } else {
            warn!("Monitor {:?} not found", mon_key);
            return;
        };

        if let Some(monitor) = self.state.monitors.get_mut(mon_key) {
            monitor.lt_symbol = layout_symbol;
            debug!("ltsymbol: {:?}", monitor.lt_symbol);
        }

        match *layout_type {
            LayoutEnum::TILE => self.tile(backend, mon_key),
            LayoutEnum::MONOCLE => self.monocle(backend, mon_key),
            LayoutEnum::FIBONACCI => self.fibonacci(backend, mon_key),
            LayoutEnum::CENTERED_MASTER => self.centered_master(backend, mon_key),
            LayoutEnum::BSTACK => self.bstack(backend, mon_key),
            LayoutEnum::GRID => self.grid(backend, mon_key),
            LayoutEnum::DECK => self.deck(backend, mon_key),
            LayoutEnum::THREE_COL => self.three_col(backend, mon_key),
            LayoutEnum::TATAMI => self.tatami(backend, mon_key),
            LayoutEnum::FULLSCREEN => self.fullscreen_layout(backend, mon_key),
            LayoutEnum::SCROLLING => self.scrolling(backend, mon_key),
            LayoutEnum::VSTACK => self.vstack(backend, mon_key),
            LayoutEnum::FLOAT | _ => {}
        }
    }

    pub(crate) fn arrange(&mut self, backend: &mut dyn Backend, m_target: Option<MonitorKey>) {
        // A marker, not a diagnostic: this runs on every map, unmap, tag
        // switch and focus cycle, and the release default filter is `info`.
        // Keep it out of the session log.
        debug!("[arrange]");

        let monitors_to_process: Vec<MonitorKey> = match m_target {
            Some(monitor_key) => vec![monitor_key],
            None => self.state.monitor_order.clone(),
        };

        for &mon_key in &monitors_to_process {
            self.showhide_monitor(backend, mon_key);
        }

        // show_bar is per-tag but the bar window is not: switching to a tag
        // that hides it (the fullscreen layout, or a toggled-off bar) has to
        // physically move the bar, not just stop reserving its pixels.
        for &mon_key in &monitors_to_process {
            self.sync_secondary_bar_position(backend, mon_key);
        }

        for &mon_key in &monitors_to_process {
            self.arrangemon(backend, mon_key);
            let _ = self.restack(backend, Some(mon_key));
        }
        let _ = backend.window_ops().flush();

        // Window moves the arrange just made are the overview's content:
        // rebuild the open grid's cells so it never shows stale wireframes.
        self.refresh_tags_overview();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn arrange_markers_stay_below_the_default_log_level() {
        // `arrange` runs per window event and `arrangemon` per monitor of
        // it; a bare marker at `info` is a write(2) into the journal for
        // each. The needle is assembled at runtime so it cannot match here.
        const SOURCE: &str = include_str!("arrange.rs");
        let body = SOURCE
            .split_once("fn arrangemon")
            .expect("arrangemon")
            .1
            .split_once("#[cfg(test)]")
            .expect("the test module")
            .0;
        let needle = format!("{}!(", "info");
        assert!(
            !body.contains(&needle),
            "arrange/arrangemon regained an info-level marker line"
        );
    }
}
