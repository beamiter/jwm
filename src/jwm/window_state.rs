use crate::backend::api::Backend;
use crate::backend::api::{StackMode, WindowChanges, WindowType};
use crate::backend::common_define::{SchemeType, WindowId};
use crate::core::models::ClientKey;

use super::Jwm;

impl Jwm {
    pub(super) fn update_client_decoration(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        is_focused: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (win, border_w) = if let Some(client) = self.state.clients.get(client_key) {
            (client.win, client.geometry.border_w)
        } else {
            return Err("Client not found".into());
        };

        let x11_bw = if backend.has_compositor() {
            0
        } else {
            border_w as u32
        };

        let scheme = if is_focused {
            SchemeType::Sel
        } else {
            SchemeType::Norm
        };
        if let Ok(pixel) = backend.color_allocator().get_border_pixel_of(scheme) {
            backend
                .window_ops()
                .set_decoration_style(win, x11_bw, pixel)?;
        }
        Ok(())
    }

    pub(super) fn setfullscreen(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        fullscreen: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let win = if let Some(client) = self.state.clients.get(client_key) {
            client.win
        } else {
            return Err("Client not found".into());
        };

        let is_fullscreen = self
            .state
            .clients
            .get(client_key)
            .map(|c| c.state.is_fullscreen)
            .unwrap_or(false);

        if fullscreen && !is_fullscreen {
            backend.property_ops().set_fullscreen_state(win, true)?;

            if let Some(client) = self.state.clients.get_mut(client_key) {
                client.state.is_fullscreen = true;
                client.state.old_state = client.state.is_floating;
                client.geometry.old_border_w = client.geometry.border_w;
                client.geometry.border_w = 0;
                client.state.is_floating = true;
            }
            self.reorder_client_in_monitor_groups(client_key);
            if let Some(mon_key) = self.state.clients.get(client_key).and_then(|c| c.mon) {
                if let Some(monitor) = self.state.monitors.get(mon_key) {
                    let (mx, my, mw, mh) = (
                        monitor.geometry.m_x,
                        monitor.geometry.m_y,
                        monitor.geometry.m_w,
                        monitor.geometry.m_h,
                    );
                    self.resizeclient(backend, client_key, mx, my, mw, mh)?;
                }
            }
            let changes = WindowChanges {
                stack_mode: Some(StackMode::Above),
                ..Default::default()
            };
            backend.window_ops().apply_window_changes(win, changes)?;
        } else if !fullscreen && is_fullscreen {
            backend.property_ops().set_fullscreen_state(win, false)?;

            if let Some(client) = self.state.clients.get_mut(client_key) {
                client.state.is_fullscreen = false;
                client.state.is_floating = client.state.old_state;
                client.geometry.border_w = client.geometry.old_border_w;
                client.geometry.x = client.geometry.old_x;
                client.geometry.y = client.geometry.old_y;
                client.geometry.w = client.geometry.old_w;
                client.geometry.h = client.geometry.old_h;
            }
            self.reorder_client_in_monitor_groups(client_key);
            let (x, y, w, h) = if let Some(client) = self.state.clients.get(client_key) {
                (
                    client.geometry.x,
                    client.geometry.y,
                    client.geometry.w,
                    client.geometry.h,
                )
            } else {
                return Ok(());
            };
            self.resizeclient(backend, client_key, x, y, w, h)?;
            if let Some(mon_key) = self.state.clients.get(client_key).and_then(|c| c.mon) {
                self.arrange(backend, Some(mon_key));
            }
        }
        Ok(())
    }

    pub(super) fn seturgent(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        urgent: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(client) = self.state.clients.get_mut(client_key) {
            client.state.is_urgent = urgent;
        } else {
            return Err("Client not found".into());
        }

        let win = self
            .state
            .clients
            .get(client_key)
            .map(|c| c.win)
            .ok_or("Client not found")?;
        Ok(backend.property_ops().set_urgent_hint(win, urgent)?)
    }

    pub(super) fn setclientstate(
        &self,
        backend: &mut dyn Backend,
        win: WindowId,
        state: i64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        Ok(backend.property_ops().set_wm_state(win, state)?)
    }

    pub(super) fn updatewindowtype(&mut self, backend: &mut dyn Backend, client_key: ClientKey) {
        let (win, is_popup_like) = if let Some(client) = self.state.clients.get(client_key) {
            (client.win, self.is_popup_like(backend, client_key))
        } else {
            return;
        };

        let was_floating = self
            .state
            .clients
            .get(client_key)
            .map(|client| client.state.is_floating)
            .unwrap_or(false);

        if backend.property_ops().is_fullscreen(win) {
            let _ = self.setfullscreen(backend, client_key, true);
        }

        let types = backend.property_ops().get_window_types(win);
        let is_desktop = types.contains(&WindowType::Desktop);
        let is_dock = types.contains(&WindowType::Dock);
        let is_transient = backend.property_ops().transient_for(win).is_some();

        let layer_info = backend.property_ops().get_layer_surface_info(win);

        if let Some(c) = self.state.clients.get_mut(client_key) {
            c.state.is_dock = is_dock;
            c.state.dock_layer_info = if is_dock { layer_info } else { None };

            if is_popup_like || is_desktop {
                c.state.is_floating = true;

                if types.contains(&WindowType::Notification)
                    || types.contains(&WindowType::Tooltip)
                    || types.contains(&WindowType::Dock)
                    || types.contains(&WindowType::Desktop)
                {
                    if !is_transient {
                        c.state.tags = crate::config::CONFIG.load().tagmask();
                        c.state.never_focus = true;
                    }
                }
            }
        }

        let is_floating_now = self
            .state
            .clients
            .get(client_key)
            .map(|client| client.state.is_floating)
            .unwrap_or(was_floating);
        if is_floating_now != was_floating {
            self.reorder_client_in_monitor_groups(client_key);
        }
    }

    pub(super) fn updatewmhints(&mut self, backend: &mut dyn Backend, client_key: ClientKey) {
        let win = match self.state.clients.get(client_key) {
            Some(c) => c.win,
            None => return,
        };
        if let Some(hints) = backend.property_ops().get_wm_hints(win) {
            if hints.urgent {
                let is_focused = self.is_client_selected(client_key);
                // Under DND, suppress urgency on unfocused clients to silence
                // taskbar/tag highlights and prevent focus-stealing chains.
                if is_focused || self.do_not_disturb {
                    let _ = backend.property_ops().set_urgent_hint(win, false);
                    if let Some(c) = self.state.clients.get_mut(client_key) {
                        c.state.is_urgent = false;
                    }
                    if backend.has_compositor() {
                        backend.compositor_set_window_urgent(win, false);
                    }
                } else {
                    if let Some(c) = self.state.clients.get_mut(client_key) {
                        c.state.is_urgent = true;
                    }
                    if backend.has_compositor() {
                        backend.compositor_set_window_urgent(win, true);
                    }
                }
            } else {
                if let Some(c) = self.state.clients.get_mut(client_key) {
                    c.state.is_urgent = false;
                }
                if backend.has_compositor() {
                    backend.compositor_set_window_urgent(win, false);
                }
            }
            if let Some(input_ok) = hints.input {
                if let Some(c) = self.state.clients.get_mut(client_key) {
                    c.state.never_focus = !input_ok;
                }
            } else {
                if let Some(c) = self.state.clients.get_mut(client_key) {
                    c.state.never_focus = false;
                }
            }
        }
    }
}
