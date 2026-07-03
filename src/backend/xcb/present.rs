use crate::backend::compositor_common::present::PresentController;
use std::collections::HashMap;
use std::sync::Arc;
use xcb::{Xid, XidNew, present, x};

pub(crate) struct XcbPresentManager {
    conn: Arc<xcb::Connection>,
    available: bool,
    event_base: u8,
    window_events: HashMap<u32, u32>,
}

impl XcbPresentManager {
    pub(crate) fn load(conn: Arc<xcb::Connection>) -> Option<Self> {
        let cookie = conn.send_request(&present::QueryVersion {
            major_version: 1,
            minor_version: 0,
        });
        let reply = conn.wait_for_reply(cookie).ok()?;

        log::info!(
            "compositor: Present extension available via xcb - version {}.{}",
            reply.major_version(),
            reply.minor_version()
        );

        let ext_info = present::get_extension_data(&conn)?;
        log::info!(
            "compositor: Present event base via xcb: {}, first_error: {}",
            ext_info.first_event,
            ext_info.first_error
        );

        Some(Self {
            conn,
            available: true,
            event_base: ext_info.first_event,
            window_events: HashMap::new(),
        })
    }
}

impl PresentController for XcbPresentManager {
    fn is_available(&self) -> bool {
        self.available
    }

    fn get_event_base(&self) -> u8 {
        self.event_base
    }

    fn register_window(&mut self, x11_win: u32) -> Result<(), String> {
        if !self.available {
            return Err("Present extension not available".to_string());
        }

        let event_id: present::EventXid = self.conn.generate_id();
        let event_mask = present::EventMask::COMPLETE_NOTIFY | present::EventMask::IDLE_NOTIFY;
        self.conn.send_request(&present::SelectInput {
            eid: event_id,
            window: x::Window::new(x11_win),
            event_mask,
        });
        self.conn
            .flush()
            .map_err(|e| format!("flush failed after Present select_input: {e}"))?;
        log::info!(
            "compositor: Present events registered via xcb for window 0x{:x}",
            x11_win
        );
        self.window_events.insert(x11_win, event_id.resource_id());
        Ok(())
    }

    fn unregister_window(&mut self, x11_win: u32) {
        if self.window_events.remove(&x11_win).is_some() {
            log::info!(
                "compositor: Present events unregistered via xcb for window 0x{:x}",
                x11_win
            );
        }
    }

    fn present_pixmap(
        &self,
        x11_win: u32,
        pixmap: u32,
        target_msc: u64,
        serial: u32,
    ) -> Result<(), String> {
        if !self.available {
            return Err("Present extension not available".to_string());
        }

        self.conn.send_request(&present::Pixmap {
            window: x::Window::new(x11_win),
            pixmap: x::Pixmap::new(pixmap),
            serial,
            valid: xcb::xfixes::Region::none(),
            update: xcb::xfixes::Region::none(),
            x_off: 0,
            y_off: 0,
            target_crtc: xcb::randr::Crtc::none(),
            wait_fence: xcb::sync::Fence::none(),
            idle_fence: xcb::sync::Fence::none(),
            options: 0,
            target_msc,
            divisor: 1,
            remainder: 0,
            notifies: &[],
        });
        Ok(())
    }

    fn notify_msc(&self, x11_win: u32, serial: u32, target_msc: u64) -> Result<(), String> {
        if !self.available {
            return Err("Present extension not available".to_string());
        }

        self.conn.send_request(&present::NotifyMsc {
            window: x::Window::new(x11_win),
            serial,
            target_msc,
            divisor: 1,
            remainder: 0,
        });
        Ok(())
    }

    fn window_count(&self) -> usize {
        self.window_events.len()
    }

    fn is_window_registered(&self, x11_win: u32) -> bool {
        self.window_events.contains_key(&x11_win)
    }
}

pub(crate) fn load_present_manager(
    conn: Arc<xcb::Connection>,
) -> Option<Box<dyn PresentController>> {
    XcbPresentManager::load(conn).map(|mgr| Box::new(mgr) as Box<dyn PresentController>)
}
