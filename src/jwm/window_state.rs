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

    /// Float a client whose size hints pin it to a single size.
    ///
    /// dwm decides this in `manage()` (`c->isfloating = trans != None ||
    /// c->isfixed`), but jwm learns `is_fixed` only in `updatesizehints`,
    /// which runs after `applyrules_by_key` has already forced
    /// `is_floating = false`. Without this pass a min==max window is tiled: it
    /// occupies a layout slot it cannot fill, so `applysizehints` clamps it
    /// back to its own size and parks it at the tile origin while the rest of
    /// the layout is laid out around a rectangle nothing ever covers.
    /// Feishu's 780x659 "飞书会议" pre-join window is exactly this shape.
    pub(super) fn float_if_fixed_size(&mut self, client_key: ClientKey) {
        let Some(client) = self.state.clients.get(client_key) else {
            return;
        };
        if !client.state.is_fixed || client.state.is_floating {
            return;
        }
        log::info!(
            "[float_if_fixed_size] {:?} has min==max size hints ({}x{}); floating it",
            client.win,
            client.size_hints.min_w,
            client.size_hints.min_h,
        );
        if let Some(client) = self.state.clients.get_mut(client_key) {
            client.state.is_floating = true;
        }
        self.reorder_client_in_monitor_groups(client_key);
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

    /// Minimise or restore a client, whoever asked. Every route into this —
    /// the `minimize` command, an ICCCM `WM_CHANGE_STATE` from a toolkit's own
    /// minimise button, a pager's `_NET_WM_STATE_HIDDEN`, a Wayland taskbar's
    /// foreign-toplevel request — has to run the same steps in the same order,
    /// so they all run these.
    ///
    /// The order is the part that matters. Minimising must detach the still
    /// visible compositor texture *before* `arrange` moves the X window off
    /// screen, or the genie animation has nothing left to animate; restoring
    /// is the exact inverse, with `arrange` re-establishing live geometry
    /// before the compositor rebuilds its entry.
    ///
    /// Returns false when the client was already in that state.
    pub(crate) fn set_client_minimized(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        minimized: bool,
    ) -> bool {
        let Some(client) = self.state.clients.get(client_key) else {
            return false;
        };
        if client.state.is_hidden == minimized {
            return false;
        }
        let win = client.win;
        let monitor = client.mon;
        let was_selected = self.is_client_selected(client_key);

        if let Some(client) = self.state.clients.get_mut(client_key) {
            client.state.is_hidden = minimized;
        }
        let _ = backend.property_ops().set_net_wm_state_flag(
            win,
            crate::backend::api::NetWmState::Hidden,
            minimized,
        );

        if minimized {
            backend.compositor_set_window_minimized(win, true);
        }
        if minimized_window_relinquishes_focus(minimized, was_selected) {
            let _ = self.focus(backend, None);
        }
        let _ = self.arrange(backend, monitor);
        if !minimized {
            backend.compositor_set_window_minimized(win, false);
            // Restoring is always someone asking for *this* window back, so it
            // takes the focus. `focusin` cannot do that job — it is the
            // FocusIn handler, and re-asserts focus on whatever is already
            // selected, which left a restored window unfocused behind the
            // window that replaced it.
            let _ = self.focus(backend, Some(client_key));
            let _ = self.restack(backend, self.state.sel_mon);
        }
        true
    }
}

/// A minimised window cannot stay focused — nothing on screen would show where
/// the keyboard is going — but minimising some *other* window must not steal
/// focus away from whatever has it.
fn minimized_window_relinquishes_focus(minimized: bool, is_selected: bool) -> bool {
    minimized && is_selected
}

#[cfg(test)]
mod tests {
    use super::minimized_window_relinquishes_focus;

    #[test]
    fn only_the_selected_window_relinquishes_focus_when_minimised() {
        assert!(minimized_window_relinquishes_focus(true, true));
        assert!(!minimized_window_relinquishes_focus(true, false));
        assert!(!minimized_window_relinquishes_focus(false, true));
    }
}
