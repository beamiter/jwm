//! Legacy top-bar adapter to the shared, toolkit-independent sidebar host.
use crate::UserEvent;
use anyhow::Result;
pub use jwm_shell::edge_shell::Action;
use std::sync::Arc;
use tao::event_loop::EventLoopProxy;
use xbar_core::{BarSnapshot, MonitorGeometry, config::BarConfig};
pub struct EdgeShell(jwm_shell::edge_shell::EdgeShell);
impl EdgeShell {
    pub fn start(
        config: BarConfig,
        output: MonitorGeometry,
        snapshot: BarSnapshot,
        proxy: EventLoopProxy<UserEvent>,
    ) -> Result<Option<Self>> {
        jwm_shell::edge_shell::EdgeShell::start(
            config,
            output,
            snapshot,
            Arc::new(move |action| {
                let _ = proxy.send_event(UserEvent::EdgeAction(action));
            }),
        )
        .map(|shell| shell.map(Self))
    }
    pub fn update(&mut self, output: MonitorGeometry, snapshot: BarSnapshot) {
        self.0.update(output, snapshot);
    }
}
