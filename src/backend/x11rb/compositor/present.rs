use std::collections::HashMap;
use std::sync::Arc;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::present::{self, ConnectionExt as PresentExt};
use x11rb::rust_connection::RustConnection;

/// Present extension support for per-window independent presentation
pub struct PresentManager {
    conn: Arc<RustConnection>,
    available: bool,
    event_base: u8,
    window_events: HashMap<u32, u32>, // x11_win -> Event ID
}

impl PresentManager {
    /// Load and initialize Present extension
    pub fn load(conn: Arc<RustConnection>) -> Option<Self> {
        // Query Present extension version
        let query_result = conn.present_query_version(1, 0).ok()?;
        let reply = query_result.reply().ok()?;

        log::info!(
            "compositor: Present extension available - version {}.{}",
            reply.major_version,
            reply.minor_version
        );

        // Get extension information for event base
        let ext_info = match conn.extension_information(present::X11_EXTENSION_NAME) {
            Ok(Some(info)) => info,
            _ => {
                log::warn!("compositor: Present extension info not available");
                return None;
            }
        };

        log::info!(
            "compositor: Present event base: {}, first_error: {}",
            ext_info.first_event,
            ext_info.first_error
        );

        Some(PresentManager {
            conn,
            available: true,
            event_base: ext_info.first_event,
            window_events: HashMap::new(),
        })
    }

    pub fn is_available(&self) -> bool {
        self.available
    }

    pub fn get_event_base(&self) -> u8 {
        self.event_base
    }

    /// Register a window for Present events
    pub fn register_window(&mut self, x11_win: u32) -> Result<(), String> {
        if !self.available {
            return Err("Present extension not available".to_string());
        }

        // Allocate an event ID
        match self.conn.generate_id() {
            Ok(event_id) => {
                // Register for Present events: CompleteNotify and IdleNotify
                let event_mask =
                    present::EventMask::COMPLETE_NOTIFY | present::EventMask::IDLE_NOTIFY;

                match self
                    .conn
                    .present_select_input(event_id, x11_win, event_mask)
                {
                    Ok(_cookie) => {
                        if let Err(e) = self.conn.flush() {
                            log::error!(
                                "compositor: flush failed after Present select_input: {}",
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
                        log::error!("compositor: Present select_input failed: {}", e);
                        Err(format!("select_input failed: {}", e))
                    }
                }
            }
            Err(e) => {
                log::error!("compositor: generate_id failed: {}", e);
                Err(format!("generate_id failed: {}", e))
            }
        }
    }

    /// Unregister a window
    pub fn unregister_window(&mut self, x11_win: u32) {
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
    pub fn present_pixmap(
        &self,
        x11_win: u32,
        pixmap: u32,
        target_msc: u64,
        serial: u32,
    ) -> Result<(), String> {
        if !self.available {
            return Err("Present extension not available".to_string());
        }

        // Simple presentation: no regions, no fences, just the basic pixmap
        // MSC = 0 means present immediately
        // For advanced use: would specify target_msc, divisor, remainder for precise timing
        match self.conn.present_pixmap(
            x11_win,    // window
            pixmap,     // pixmap to present
            serial,     // serial number for tracking
            0,          // valid region (0 = entire pixmap)
            0,          // update region (0 = entire pixmap)
            0,          // x_off
            0,          // y_off
            0,          // target_crtc
            0,          // wait_fence
            0,          // idle_fence
            0,          // options
            target_msc, // target MSC (0 for immediate)
            1,          // divisor (1 = any MSC)
            0,          // remainder
            &[],        // notifies (empty for now)
        ) {
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
                log::error!("compositor: present_pixmap failed: {}", e);
                Err(format!("present_pixmap failed: {}", e))
            }
        }
    }

    /// Request notification at a specific MSC
    pub fn notify_msc(&self, x11_win: u32, serial: u32, target_msc: u64) -> Result<(), String> {
        if !self.available {
            return Err("Present extension not available".to_string());
        }

        match self.conn.present_notify_msc(
            x11_win,    // window
            serial,     // serial for identification
            target_msc, // target MSC
            1,          // divisor
            0,          // remainder
        ) {
            Ok(_) => Ok(()),
            Err(e) => {
                log::error!("compositor: notify_msc failed: {}", e);
                Err(format!("notify_msc failed: {}", e))
            }
        }
    }

    /// Get the number of registered windows
    pub fn window_count(&self) -> usize {
        self.window_events.len()
    }

    /// Check if a window is registered for Present
    pub fn is_window_registered(&self, x11_win: u32) -> bool {
        self.window_events.contains_key(&x11_win)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
