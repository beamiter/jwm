//! Executor for the transport-free backend-event bridge.
//!
//! `backend::x11::wm::event_bridge::compositor_event_ops` decides what a
//! backend event means for the compositor; this file is the single place
//! those decisions are carried out. Both transports call it through their
//! thin `compositor_handle_event` wrappers.

use super::{Compositor, CompositorConnection};
use crate::backend::x11::wm::event_bridge::CompositorEventOp;

impl<C: CompositorConnection> Compositor<C> {
    /// Execute one planned backend-event effect. `root` is the root window
    /// this compositor was built for; `RefreshLayout` re-reads its geometry
    /// through the compositor's own connection.
    pub(crate) fn apply_event_op(&mut self, root: u32, op: CompositorEventOp) {
        match op {
            CompositorEventOp::AddWindow {
                window,
                x,
                y,
                width,
                height,
            } => self.add_window(window, x, y, width, height),
            CompositorEventOp::SetWindowClass { window, class } => {
                self.set_window_class(window, &class);
            }
            CompositorEventOp::SetOverrideRedirect { window } => {
                self.set_window_override_redirect(window, true);
            }
            CompositorEventOp::RemoveWindow { window } => self.remove_window(window),
            CompositorEventOp::UpdateGeometry {
                window,
                x,
                y,
                width,
                height,
            } => self.update_geometry(window, x, y, width, height),
            CompositorEventOp::SetFullscreen { window, fullscreen } => {
                self.set_window_fullscreen(window, fullscreen);
            }
            CompositorEventOp::MarkDamaged { window } => self.mark_damaged(window),
            CompositorEventOp::PresentComplete {
                window,
                serial,
                msc,
                ust,
            } => {
                if let Some(oml) = self.oml_mut() {
                    oml.on_window_presented(window, msc, ust);
                }
                self.on_present_complete(window, serial, msc, ust);
            }
            CompositorEventOp::PresentIdle {
                window,
                serial,
                pixmap,
            } => self.on_present_idle(window, serial, pixmap),
            CompositorEventOp::SetMousePosition { x, y } => self.set_mouse_position(x, y),
            CompositorEventOp::RecordInput => self.record_input_event(),
            CompositorEventOp::RefreshLayout => {
                // Root resize (e.g. xrandr) must also grow the viewport so the
                // compositor covers the full virtual screen, and monitor
                // add/remove/mode-change can alter per-monitor geometry and
                // refresh rates used by blur quality and refresh lookups.
                if let Some((width, height)) = self.conn.query_window_size(root) {
                    self.resize(width, height);
                }
                self.refresh_monitor_layout(root);
            }
        }
    }
}
