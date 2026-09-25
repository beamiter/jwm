// Focus management operations: window focus, monitor selection, and EWMH updates

use crate::Jwm;
use crate::backend::api::Backend;
use crate::config::CONFIG;
use crate::core::models::ClientKey;

impl Jwm {
    pub(crate) fn find_visible_client(&self) -> Option<ClientKey> {
        let sel_mon_key = self.state.sel_mon?;
        // Every window on a locked monitor is behind its shade. The selection
        // is kept off such a monitor, but a path that selects one directly
        // and then asks `focus(None)` for a window must not get a hidden one.
        if self.monitor_key_is_locked(sel_mon_key) {
            return None;
        }

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

        if self
            .state
            .clients
            .get(client_key)
            .is_some_and(|client| client.state.is_urgent)
        {
            let _ = self.seturgent(backend, client_key, false);
        }
        // EWMH: the WM clears _NET_WM_STATE_DEMANDS_ATTENTION once the window
        // has the user's attention. Gated on the EWMH flag itself (not on
        // is_urgent, which seturgent just cleared), and run after seturgent
        // so the helper's WM_HINTS read-back already sees the hint cleared
        // and does not re-derive urgency from it.
        if self
            .state
            .clients
            .get(client_key)
            .is_some_and(|client| client.state.demands_attention)
        {
            self.set_client_demands_attention(backend, client_key, false);
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

#[cfg(test)]
mod tests {
    use crate::backend::common_define::WindowId;
    use crate::core::models::WMClient;
    use crate::jwm::Jwm;
    use crate::jwm::monitor::test_support::{DisplaySpyBackend, output};

    /// Regression: `find_visible_client` scanned the selected monitor's
    /// stack without asking whether a lock shade covers it, so a path that
    /// selected a locked monitor directly and then called `focus(None)`
    /// focused a window nobody could see.
    #[test]
    fn a_locked_selected_monitor_offers_no_window_to_focus() {
        let mut backend = DisplaySpyBackend::new(vec![
            output(1, 0, 0, 1920, 1080),
            output(2, 1920, 0, 1920, 1080),
        ]);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test")
            .expect("a spy backend builds a JWM");
        let shaded = jwm.state.monitor_order[1];
        let mut client = WMClient::new(WindowId::from_raw(0x5e10));
        client.mon = Some(shaded);
        client.state.tags = jwm.state.monitors[shaded].get_active_tags();
        let key = jwm.insert_client(client);
        jwm.attach_to_monitor(key, shaded);
        jwm.state.sel_mon = Some(shaded);
        assert_eq!(jwm.find_visible_client(), Some(key));

        let num = jwm.state.monitors[shaded].num;
        assert!(jwm.features.monitor_lock.lock(num, (1920, 0, 1920, 1080)));
        assert!(jwm.monitor_key_is_locked(shaded));
        assert_eq!(jwm.find_visible_client(), None);
        jwm.focus(&mut backend, None).expect("focus");
        assert_eq!(jwm.get_selected_client_key(), None);
    }
}
