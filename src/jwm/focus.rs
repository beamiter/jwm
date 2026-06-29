// Focus management operations: window focus, monitor selection, and EWMH updates

use crate::Jwm;
use crate::backend::api::Backend;
use crate::config::CONFIG;
use crate::core::models::ClientKey;

impl Jwm {
    pub(crate) fn find_visible_client(&self) -> Option<ClientKey> {
        let sel_mon_key = self.state.sel_mon?;

        if let Some(stack_clients) = self.state.monitor_stack.get(sel_mon_key) {
            for &client_key in stack_clients {
                if self.is_client_visible_by_key(client_key) {
                    return Some(client_key);
                }
            }
        }

        None
    }

    pub(crate) fn handle_focus_change_by_key(
        &mut self,
        backend: &mut dyn Backend,
        new_focus: &Option<ClientKey>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let current_sel = self.get_selected_client_key();

        if current_sel.is_some() && current_sel != *new_focus {
            if let Some(current_key) = current_sel {
                self.unfocus_client(backend, current_key, false)?;
            }
        }

        Ok(())
    }

    pub(crate) fn set_client_focus_by_key(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let client_monitor_key = if let Some(client) = self.state.clients.get(client_key) {
            client.mon
        } else {
            return Err("Client not found".into());
        };

        if let Some(client_mon_key) = client_monitor_key {
            if Some(client_mon_key) != self.state.sel_mon {
                self.state.sel_mon = Some(client_mon_key);
            }
        }

        if let Some(client) = self.state.clients.get_mut(client_key) {
            if client.state.is_urgent {
                client.state.is_urgent = false;
                let win = client.win;
                let _ = self.seturgent(backend, client_key, false);
                if backend.has_compositor() {
                    backend.compositor_set_window_urgent(win, false);
                }
            }
        }
        self.detachstack(client_key);
        self.attachstack(client_key);
        self.update_client_decoration(backend, client_key, true)?;
        self.grabbuttons(backend, client_key, true);
        self.setfocus(backend, client_key)?;
        Ok(())
    }

    pub(crate) fn update_monitor_selection_by_key(&mut self, client_key_opt: Option<ClientKey>) {
        if let Some(sel_mon_key) = self.state.sel_mon {
            if let Some(monitor) = self.state.monitors.get_mut(sel_mon_key) {
                // 使用新方法
                monitor.set_selected_client_for_current_tag(client_key_opt);
            }
        }
    }

    pub(crate) fn unfocus_client(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        setfocus: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(_client) = self.state.clients.get(client_key) {
            self.update_client_decoration(backend, client_key, false)?;
            self.grabbuttons(backend, client_key, false);
            if setfocus {
                backend.on_focused_client_changed(None)?;
            }
        }
        Ok(())
    }

    pub(crate) fn setfocus(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(client) = self.state.clients.get(client_key) {
            backend.on_focused_client_changed(Some(client.win))?;
        }
        Ok(())
    }

    pub(crate) fn set_root_focus(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        backend.window_ops().set_input_focus_root()?;
        Ok(backend.on_focused_client_changed(None)?)
    }

    pub(crate) fn update_ewmh_desktop(
        &self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let total = CONFIG.load().tags_length() as u32;
        let current = if let Some(sel_mon_key) = self.state.sel_mon {
            if let Some(monitor) = self.state.monitors.get(sel_mon_key) {
                let tagset = monitor.get_active_tags();
                if tagset > 0 {
                    tagset.trailing_zeros()
                } else {
                    0
                }
            } else {
                0
            }
        } else {
            0
        };
        let names: Vec<String> = (1..=total).map(|i| i.to_string()).collect();
        let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        backend.on_desktop_changed(current, total, &name_refs)?;

        // _NET_WORKAREA: EWMH expects one rect per desktop. We publish the
        // bounding box of every monitor's strut-adjusted workarea (w_*),
        // repeated for each desktop, so maximizing clients avoid the bar.
        if total > 0 {
            let mut bounds: Option<(i32, i32, i32, i32)> = None; // x0,y0,x1,y1
            for monitor in self.state.monitors.values() {
                let g = &monitor.geometry;
                if g.w_w <= 0 || g.w_h <= 0 {
                    continue;
                }
                let (x0, y0, x1, y1) = (g.w_x, g.w_y, g.w_x + g.w_w, g.w_y + g.w_h);
                bounds = Some(match bounds {
                    Some((bx0, by0, bx1, by1)) => {
                        (bx0.min(x0), by0.min(y0), bx1.max(x1), by1.max(y1))
                    }
                    None => (x0, y0, x1, y1),
                });
            }
            if let Some((x0, y0, x1, y1)) = bounds {
                let rect = (x0, y0, (x1 - x0).max(1) as u32, (y1 - y0).max(1) as u32);
                let areas = vec![rect; total as usize];
                backend.set_workarea(&areas)?;
            }
        }
        Ok(())
    }
}
