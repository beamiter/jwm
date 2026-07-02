use crate::backend::x11rb::Atoms;
use std::sync::Arc;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::x11_utils::Serialize;

const SYSTEM_TRAY_REQUEST_DOCK: u32 = 0;
const XEMBED_EMBEDDED_NOTIFY: u32 = 0;
const XEMBED_MAPPED: u32 = 1;
const XEMBED_PROTOCOL_VERSION: u32 = 0;

#[derive(Debug, Clone)]
pub struct TrayIcon {
    pub window: u32,
    pub width: u32,
    pub height: u32,
    pub mapped: bool,
}

pub struct SystemTray<C: Connection> {
    conn: Arc<C>,
    atoms: Atoms,
    tray_window: u32,
    selection_atom: u32,
    root: u32,
    icon_size: u32,
    icons: Vec<TrayIcon>,
    active: bool,
}

impl<C: Connection + Send + Sync + 'static> SystemTray<C> {
    pub fn new(
        conn: Arc<C>,
        atoms: Atoms,
        root: u32,
        screen_num: usize,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let selection_name = format!("_NET_SYSTEM_TRAY_S{}", screen_num);
        let selection_atom = conn
            .intern_atom(false, selection_name.as_bytes())?
            .reply()?
            .atom;

        let tray_window = conn.generate_id()?;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            tray_window,
            root,
            -1,
            -1,
            1,
            1,
            0,
            // Must be INPUT_OUTPUT: tray icons are INPUT_OUTPUT windows reparented
            // as children, and X11 forbids INPUT_OUTPUT children under an
            // INPUT_ONLY parent (BadMatch), so embedding would fail.
            WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;

        Ok(Self {
            conn,
            atoms,
            tray_window,
            selection_atom,
            root,
            icon_size: 24,
            icons: Vec::new(),
            active: false,
        })
    }

    pub fn acquire_selection(&mut self) -> Result<bool, Box<dyn std::error::Error>> {
        let current_owner = self
            .conn
            .get_selection_owner(self.selection_atom)?
            .reply()?
            .owner;
        if current_owner != x11rb::NONE {
            return Ok(false);
        }

        self.conn.set_selection_owner(
            self.tray_window,
            self.selection_atom,
            x11rb::CURRENT_TIME,
        )?;

        let owner = self
            .conn
            .get_selection_owner(self.selection_atom)?
            .reply()?
            .owner;
        if owner != self.tray_window {
            return Ok(false);
        }

        // Broadcast MANAGER client message to root
        let event = ClientMessageEvent::new(
            32,
            self.root,
            self.atoms.MANAGER,
            [
                x11rb::CURRENT_TIME,
                self.selection_atom,
                self.tray_window,
                0,
                0,
            ],
        );
        self.conn.send_event(
            false,
            self.root,
            EventMask::STRUCTURE_NOTIFY,
            event.serialize(),
        )?;

        // Set tray orientation (horizontal)
        self.conn.change_property32(
            PropMode::REPLACE,
            self.tray_window,
            self.atoms._NET_SYSTEM_TRAY_ORIENTATION,
            AtomEnum::CARDINAL,
            &[0], // horizontal
        )?;

        self.conn.flush()?;
        self.active = true;
        Ok(true)
    }

    pub fn handle_client_message(&mut self, event_window: u32, data: &[u32; 5]) -> bool {
        if !self.active {
            return false;
        }

        let opcode = data[1];
        if opcode == SYSTEM_TRAY_REQUEST_DOCK {
            let icon_window = data[2];
            if icon_window == 0 {
                return true;
            }
            let _ = self.dock_icon(icon_window);
            return true;
        }
        let _ = event_window;
        false
    }

    fn dock_icon(&mut self, icon_window: u32) -> Result<(), Box<dyn std::error::Error>> {
        if self.icons.iter().any(|i| i.window == icon_window) {
            return Ok(());
        }

        // Reparent icon into tray window
        self.conn.change_window_attributes(
            icon_window,
            &ChangeWindowAttributesAux::new()
                .event_mask(EventMask::STRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE),
        )?;

        self.conn
            .reparent_window(icon_window, self.tray_window, 0, 0)?;

        // Configure icon size
        self.conn.configure_window(
            icon_window,
            &ConfigureWindowAux::new()
                .width(self.icon_size)
                .height(self.icon_size),
        )?;

        self.conn.map_window(icon_window)?;

        // Send XEMBED_EMBEDDED_NOTIFY
        let xembed_event = ClientMessageEvent::new(
            32,
            icon_window,
            self.atoms._XEMBED,
            [
                x11rb::CURRENT_TIME,
                XEMBED_EMBEDDED_NOTIFY,
                0,
                self.tray_window,
                XEMBED_PROTOCOL_VERSION,
            ],
        );
        self.conn.send_event(
            false,
            icon_window,
            EventMask::NO_EVENT,
            xembed_event.serialize(),
        )?;

        self.icons.push(TrayIcon {
            window: icon_window,
            width: self.icon_size,
            height: self.icon_size,
            mapped: true,
        });

        self.layout_icons()?;
        self.conn.flush()?;
        Ok(())
    }

    pub fn handle_destroy(&mut self, window: u32) {
        if let Some(pos) = self.icons.iter().position(|i| i.window == window) {
            self.icons.remove(pos);
            let _ = self.layout_icons();
            let _ = self.conn.flush();
        }
    }

    pub fn handle_unmap(&mut self, window: u32) {
        if let Some(icon) = self.icons.iter_mut().find(|i| i.window == window) {
            icon.mapped = false;
            let _ = self.layout_icons();
            let _ = self.conn.flush();
        }
    }

    pub fn handle_map(&mut self, window: u32) {
        if let Some(icon) = self.icons.iter_mut().find(|i| i.window == window) {
            icon.mapped = true;
            let _ = self.layout_icons();
            let _ = self.conn.flush();
        }
    }

    fn layout_icons(&self) -> Result<(), Box<dyn std::error::Error>> {
        let mut x_offset: u32 = 0;
        for icon in &self.icons {
            if !icon.mapped {
                continue;
            }
            self.conn.configure_window(
                icon.window,
                &ConfigureWindowAux::new()
                    .x(x_offset as i32)
                    .y(0)
                    .width(self.icon_size)
                    .height(self.icon_size),
            )?;
            x_offset += self.icon_size;
        }
        // Resize tray window to fit
        let total_width = x_offset.max(1);
        self.conn.configure_window(
            self.tray_window,
            &ConfigureWindowAux::new()
                .width(total_width)
                .height(self.icon_size),
        )?;
        Ok(())
    }

    pub fn icons(&self) -> &[TrayIcon] {
        &self.icons
    }

    pub fn tray_window(&self) -> u32 {
        self.tray_window
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn is_tray_icon(&self, window: u32) -> bool {
        self.icons.iter().any(|i| i.window == window)
    }

    pub fn cleanup(&self) {
        if !self.active {
            return;
        }
        for icon in &self.icons {
            let _ = self.conn.reparent_window(icon.window, self.root, 0, 0);
            let _ = self.conn.unmap_window(icon.window);
        }
        let _ = self.conn.destroy_window(self.tray_window);
        let _ = self.conn.flush();
    }

    pub fn handle_xembed_info_change(&mut self, window: u32) {
        let mapped = self.read_xembed_mapped(window);
        if let Some(icon) = self.icons.iter_mut().find(|i| i.window == window) {
            if mapped && !icon.mapped {
                icon.mapped = true;
                let _ = self.conn.map_window(window);
            } else if !mapped && icon.mapped {
                icon.mapped = false;
                let _ = self.conn.unmap_window(window);
            }
            let _ = self.layout_icons();
            let _ = self.conn.flush();
        }
    }

    fn read_xembed_mapped(&self, window: u32) -> bool {
        let reply = match self.conn.get_property(
            false,
            window,
            self.atoms._XEMBED_INFO,
            AtomEnum::ANY,
            0,
            2,
        ) {
            Ok(cookie) => match cookie.reply() {
                Ok(r) => r,
                Err(_) => return true,
            },
            Err(_) => return true,
        };
        if reply.format != 32 {
            return true;
        }
        let data: Vec<u32> = match reply.value32() {
            Some(iter) => iter.collect(),
            None => return true,
        };
        if data.len() >= 2 {
            data[1] & XEMBED_MAPPED != 0
        } else {
            true
        }
    }
}
