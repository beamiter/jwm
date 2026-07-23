use crate::backend::x11::compositor_common::present::PresentController;
use std::collections::HashMap;
use std::sync::Arc;

use super::CompositorConnection;

/// Present extension support for per-window independent presentation
pub(crate) struct X11rbPresentManager<C: CompositorConnection> {
    conn: Arc<C>,
    available: bool,
    #[allow(dead_code)]
    event_base: u8,
    window_events: HashMap<u32, u32>, // x11_win -> Event ID
}

#[allow(dead_code)]
impl<C: CompositorConnection> X11rbPresentManager<C> {
    /// Per-operation display-boundary error context tagged with the active
    /// X11 transport, for Present protocol failures.
    fn display_ctx(&self, operation: &'static str) -> crate::backend::error::BackendErrorContext {
        crate::backend::error::BackendErrorContext::new(
            self.conn.backend_name(),
            crate::backend::error::ErrorBoundary::Display,
            operation,
        )
    }

    /// Load and initialize Present extension
    pub(crate) fn load(conn: Arc<C>) -> Option<Self> {
        let (major_version, minor_version) = conn.query_present_version().ok()?;

        log::info!(
            "compositor: Present extension available - version {}.{}",
            major_version,
            minor_version
        );

        let event_base = match conn.query_present_event_base() {
            Ok(base) => base,
            Err(_) => {
                log::warn!("compositor: Present extension info not available");
                return None;
            }
        };

        log::info!(
            "compositor: Present event base: {}, first_error: {}",
            event_base,
            0
        );

        Some(X11rbPresentManager {
            conn,
            available: true,
            event_base,
            window_events: HashMap::new(),
        })
    }

    pub(crate) fn is_available(&self) -> bool {
        self.available
    }

    pub(crate) fn get_event_base(&self) -> u8 {
        self.event_base
    }

    /// Register a window for Present events
    pub(crate) fn register_window(&mut self, x11_win: u32) -> Result<(), String> {
        if !self.available {
            return Err("Present extension not available".to_string());
        }

        // Allocate an event ID
        match self.conn.generate_xid() {
            Ok(event_id) => match self.conn.select_present_input(event_id, x11_win) {
                Ok(()) => {
                    if let Err(e) = self.conn.flush_x11() {
                        log::error!(
                            "{}: {}",
                            self.display_ctx("present: flush after select input"),
                            e
                        );
                        return Err(format!("flush failed: {}", e));
                    }
                    log::info!(
                        "compositor: Present events registered for window 0x{:x}",
                        x11_win
                    );
                    self.window_events.insert(x11_win, event_id);
                    Ok(())
                }
                Err(e) => {
                    log::error!("{}: {}", self.display_ctx("present: select input"), e);
                    Err(format!("select_input failed: {}", e))
                }
            },
            Err(e) => {
                log::error!("{}: {}", self.display_ctx("present: allocate event id"), e);
                Err(format!("generate_id failed: {}", e))
            }
        }
    }

    /// Unregister a window
    pub(crate) fn unregister_window(&mut self, x11_win: u32) {
        if let Some(_event_id) = self.window_events.remove(&x11_win) {
            log::info!(
                "compositor: Present events unregistered for window 0x{:x}",
                x11_win
            );
        }
    }

    /// Present a pixmap for a window at a target MSC
    ///
    /// This allows the window to present its content independently of the compositor's
    /// global frame rate, enabling per-window audio-video synchronization.
    pub(crate) fn present_pixmap(
        &self,
        x11_win: u32,
        pixmap: u32,
        target_msc: u64,
        serial: u32,
    ) -> Result<(), String> {
        if !self.available {
            return Err("Present extension not available".to_string());
        }

        match self
            .conn
            .present_pixmap_for_window(x11_win, pixmap, target_msc, serial)
        {
            Ok(_) => {
                log::debug!(
                    "compositor: presented pixmap for 0x{:x} serial={} msc={}",
                    x11_win,
                    serial,
                    target_msc
                );
                Ok(())
            }
            Err(e) => {
                log::error!("{}: {}", self.display_ctx("present: present pixmap"), e);
                Err(format!("present_pixmap failed: {}", e))
            }
        }
    }

    /// Request notification at a specific MSC
    pub(crate) fn notify_msc(
        &self,
        x11_win: u32,
        serial: u32,
        target_msc: u64,
    ) -> Result<(), String> {
        if !self.available {
            return Err("Present extension not available".to_string());
        }

        match self.conn.notify_present_msc(x11_win, serial, target_msc) {
            Ok(_) => Ok(()),
            Err(e) => {
                log::error!("{}: {}", self.display_ctx("present: notify msc"), e);
                Err(format!("notify_msc failed: {}", e))
            }
        }
    }

    /// Get the number of registered windows
    pub(crate) fn window_count(&self) -> usize {
        self.window_events.len()
    }

    /// Check if a window is registered for Present
    pub(crate) fn is_window_registered(&self, x11_win: u32) -> bool {
        self.window_events.contains_key(&x11_win)
    }
}

impl<C: CompositorConnection> PresentController for X11rbPresentManager<C> {
    fn is_available(&self) -> bool {
        X11rbPresentManager::is_available(self)
    }

    fn get_event_base(&self) -> u8 {
        X11rbPresentManager::get_event_base(self)
    }

    fn register_window(&mut self, x11_win: u32) -> Result<(), String> {
        X11rbPresentManager::register_window(self, x11_win)
    }

    fn unregister_window(&mut self, x11_win: u32) {
        X11rbPresentManager::unregister_window(self, x11_win)
    }

    fn present_pixmap(
        &self,
        x11_win: u32,
        pixmap: u32,
        target_msc: u64,
        serial: u32,
    ) -> Result<(), String> {
        X11rbPresentManager::present_pixmap(self, x11_win, pixmap, target_msc, serial)
    }

    fn notify_msc(&self, x11_win: u32, serial: u32, target_msc: u64) -> Result<(), String> {
        X11rbPresentManager::notify_msc(self, x11_win, serial, target_msc)
    }

    fn window_count(&self) -> usize {
        X11rbPresentManager::window_count(self)
    }

    fn is_window_registered(&self, x11_win: u32) -> bool {
        X11rbPresentManager::is_window_registered(self, x11_win)
    }
}

pub(crate) fn load_present_manager<C: CompositorConnection>(
    conn: Arc<C>,
) -> Option<Box<dyn PresentController>> {
    X11rbPresentManager::load(conn).map(|mgr| Box::new(mgr) as Box<dyn PresentController>)
}

#[cfg(test)]
mod tests {
    use x11rb_protocol::protocol::present;

    #[test]
    fn test_present_manager_creation() {
        // Note: These tests would need a real X connection to fully pass
        // For now, just verify the module compiles
    }

    #[test]
    fn test_event_mask() {
        let mask = present::EventMask::COMPLETE_NOTIFY | present::EventMask::IDLE_NOTIFY;
        // Verify the mask is non-empty (using Into<u32> or comparison)
        assert!(mask != present::EventMask::NO_EVENT);
    }
}
