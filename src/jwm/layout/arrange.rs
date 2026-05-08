// Top-level layout dispatch functions

use crate::backend::api::Backend;
use crate::core::layout::LayoutEnum;
use crate::core::models::MonitorKey;
use crate::jwm::Jwm;
use log::{info, warn};

impl Jwm {
    pub(crate) fn arrangemon(&mut self, backend: &mut dyn Backend, mon_key: MonitorKey) {
        info!("[arrangemon]");

        let (layout_type, layout_symbol) = if let Some(monitor) = self.state.monitors.get(mon_key) {
            let sel_lt = monitor.sel_lt;
            let layout = &monitor.lt[sel_lt];
            (layout.clone(), layout.symbol().to_string())
        } else {
            warn!("Monitor {:?} not found", mon_key);
            return;
        };

        if let Some(monitor) = self.state.monitors.get_mut(mon_key) {
            monitor.lt_symbol = layout_symbol;
            info!(
                "sel_lt: {}, ltsymbol: {:?}",
                monitor.sel_lt, monitor.lt_symbol
            );
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
        info!("[arrange]");

        let monitors_to_process: Vec<MonitorKey> = match m_target {
            Some(monitor_key) => vec![monitor_key],
            None => self.state.monitor_order.clone(),
        };

        for &mon_key in &monitors_to_process {
            self.showhide_monitor(backend, mon_key);
        }

        for &mon_key in &monitors_to_process {
            self.arrangemon(backend, mon_key);
            let _ = self.restack(backend, Some(mon_key));
        }
        let _ = backend.window_ops().flush();
    }
}
