// Input event handling: keyboard, mouse, and configure request processing

use crate::backend::api::{AllowMode, Backend, HitTarget, WindowChanges, WindowType};
use crate::backend::common_define::{keys, ConfigWindowBits, Mods, MouseButton, WindowId};
use crate::config::CONFIG;
use crate::core::models::ClientKey;
use crate::core::types::Rect;
use crate::jwm::types::{WMArgEnum, WMClickType};
use crate::Jwm;
use log::{error, info};

impl Jwm {
    pub(crate) fn on_key_press_internal(
        &mut self,
        backend: &mut dyn Backend,
        keycode: u8,
        state_bits: u16,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let debug_keys = std::env::var("JWM_DEBUG_KEYS")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        let keysym = backend.key_ops_mut().keysym_from_keycode(keycode)?;
        let clean_state = self.clean_mask(backend, state_bits);

        // Screenshot region selection mode
        if self.features.screenshot.active {
            if keysym == keys::KEY_Escape {
                self.cancel_screenshot_select(backend);
            } else if self.features.screenshot.committed {
                // Selection done — choose save action
                if keysym == keys::KEY_Return || keysym == keys::KEY_s {
                    // Enter or 's' → save to file
                    self.finish_screenshot_select(backend, false);
                } else if keysym == keys::KEY_c {
                    // 'c' → copy to clipboard
                    self.finish_screenshot_select(backend, true);
                }
                // Other keys are consumed silently
            }
            return Ok(());
        }

        if self.features.expose_active {
            if keysym == keys::KEY_Escape {
                self.features.expose_active = false;
                backend.compositor_set_expose_mode(false, vec![]);
                let _ = backend.key_ops().ungrab_keyboard();
                let _ = backend.input_ops().ungrab_pointer();
                return Ok(());
            }
            // Fall through to normal keybinding dispatch so Alt+E can toggle off
        }

        if self.features.overview.active {
            let overview_mods = clean_state
                & (Mods::SHIFT
                    | Mods::CONTROL
                    | Mods::ALT
                    | Mods::SUPER
                    | Mods::MOD2
                    | Mods::MOD3
                    | Mods::MOD5);

            // Tab / Shift+Tab / Alt+Tab / Alt+Shift+Tab → cycle forward / backward
            if keysym == keys::KEY_Tab && !overview_mods.contains(Mods::CONTROL) {
                let direction = if overview_mods.contains(Mods::SHIFT) {
                    -1
                } else {
                    1
                };
                if debug_keys {
                    info!(
                        "[overview] cycle via Tab keysym=0x{:x} mods=0x{:x} direction={}",
                        keysym,
                        overview_mods.bits(),
                        direction,
                    );
                }
                return self.cycle_overview(backend, &WMArgEnum::Int(direction));
            }
            // Alt+J → cycle forward, Alt+K → cycle backward
            if keysym == keys::KEY_j && overview_mods == Mods::ALT {
                return self.cycle_overview(backend, &WMArgEnum::Int(1));
            }
            if keysym == keys::KEY_k && overview_mods == Mods::ALT {
                return self.cycle_overview(backend, &WMArgEnum::Int(-1));
            }
            // Alt+Ctrl+Tab → confirm (close overview, focus selected)
            if keysym == keys::KEY_Tab
                && overview_mods.contains(Mods::ALT)
                && overview_mods.contains(Mods::CONTROL)
            {
                return self.toggle_overview(backend, &WMArgEnum::Int(0));
            }
            // Enter → confirm (close overview, focus selected)
            if keysym == keys::KEY_Return {
                return self.toggle_overview(backend, &WMArgEnum::Int(0));
            }
            // Escape → cancel (close overview, no focus change)
            if keysym == keys::KEY_Escape {
                self.features.overview.active = false;
                self.features.overview.clients.clear();
                self.features.overview.index = 0;
                backend.compositor_set_overview_mode(false, &[]);
                let _ = backend.key_ops().ungrab_keyboard();
                return Ok(());
            }
            // Consume all other keys while overview is active
            return Ok(());
        }

        let mut matched = false;
        for key_config in self.key_bindings.to_vec().iter() {
            let kc_mask = key_config.mask
                & (Mods::SHIFT
                    | Mods::CONTROL
                    | Mods::ALT
                    | Mods::SUPER
                    | Mods::MOD2
                    | Mods::MOD3
                    | Mods::MOD5);
            if keysym == key_config.key_sym && kc_mask == clean_state {
                matched = true;
                if debug_keys {
                    let func_name = key_config.func_opt.map(Self::func_name).unwrap_or("<none>");
                    info!(
                        "[key] matched keysym=0x{:x} mods=0x{:x} func={} arg={:?}",
                        keysym,
                        clean_state.bits(),
                        func_name,
                        key_config.arg
                    );
                }
                if let Some(func) = key_config.func_opt {
                    if let Err(e) = func(self, backend, &key_config.arg) {
                        error!("Error executing keyboard shortcut: {:?}", e);
                    }
                }
                break;
            }
        }

        if debug_keys && !matched {
            info!(
                "[key] no match keysym=0x{:x} mods=0x{:x}",
                keysym,
                clean_state.bits()
            );
        }
        Ok(())
    }

    pub(crate) fn on_button_press_internal(
        &mut self,
        backend: &mut dyn Backend,
        target: crate::backend::api::HitTarget,
        state_bits: u16,
        detail_btn: u8,
        time: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Screenshot region selection intercept
        if self.features.screenshot.active {
            let btn = MouseButton::from_u8(detail_btn);
            if btn == MouseButton::Left {
                // Start dragging
                self.features.screenshot.dragging = true;
                self.features.screenshot.start = self.last_mouse_root;
                // Immediately show a 1x1 preview to avoid animation delay
                if backend.has_compositor() {
                    let (x, y) = self.last_mouse_root;
                    backend.compositor_set_snap_preview(Some((x as f32, y as f32, 1.0, 1.0)));
                    backend.compositor_force_full_redraw();
                }
            } else {
                // Right-click or other button → cancel
                self.cancel_screenshot_select(backend);
            }
            return Ok(());
        }

        // Expose mode intercept: route clicks to compositor
        if self.features.expose_active {
            let (rx, ry) = self.last_mouse_root;
            if let Some(wid) = backend.compositor_expose_click(rx as f32, ry as f32) {
                // Compositor handled the click and already deactivated expose animation
                self.features.expose_active = false;
                let _ = backend.key_ops().ungrab_keyboard();
                let _ = backend.input_ops().ungrab_pointer();
                if let Some(ck) = self.wintoclient(wid) {
                    self.focus(backend, Some(ck))?;
                    if let Some(mon_key) = self.state.sel_mon {
                        let _ = self.restack(backend, Some(mon_key));
                    }
                }
            } else {
                // Clicked outside any exposed window — exit expose
                self.features.expose_active = false;
                backend.compositor_set_expose_mode(false, vec![]);
                let _ = backend.key_ops().ungrab_keyboard();
                let _ = backend.input_ops().ungrab_pointer();
            }
            return Ok(());
        }

        let mut click_type = WMClickType::ClickRootWin;
        let clicked_win: Option<crate::backend::common_define::WindowId> = match target {
            HitTarget::Surface(wid) => Some(wid),
            HitTarget::Background { .. } => None,
        };
        let target_mon_key = self.target_to_monitor(
            backend,
            target,
            (self.last_mouse_root.0 as i32, self.last_mouse_root.1 as i32),
        );
        if target_mon_key != self.state.sel_mon {
            if let Some(cur) = self.get_selected_client_key() {
                self.unfocus_client(backend, cur, true)?;
            }
            self.state.sel_mon = target_mon_key;
            self.focus(backend, None)?;
        }
        let mut is_client_click = false;
        let mut clicked_client_key: Option<ClientKey> = None;
        if let Some(wid) = clicked_win {
            if Some(wid) != backend.root_window() {
                if let Some(client_key) = self.wintoclient(wid) {
                    is_client_click = true;
                    clicked_client_key = Some(client_key);
                    self.focus(backend, Some(client_key))?;
                    // Invalidate stacking cache so restack always applies the
                    // new z-order when clicking a partially-obscured window.
                    if let Some(mon_key) = self.state.sel_mon {
                        self.last_stacking.remove(mon_key);
                    }
                    let _ = self.restack(backend, self.state.sel_mon);
                    click_type = WMClickType::ClickClientWin;
                }
            }
        }

        let event_mask = self.clean_mask(backend, state_bits);
        let mouse_button = MouseButton::from_u8(detail_btn);

        let mut handled_by_wm = false;
        for config in CONFIG.load().get_buttons().iter() {
            let kc_mask = config.mask
                & (Mods::SHIFT
                    | Mods::CONTROL
                    | Mods::ALT
                    | Mods::SUPER
                    | Mods::MOD2
                    | Mods::MOD3
                    | Mods::MOD5);
            if config.click_type == click_type
                && config.func.is_some()
                && config.button == mouse_button
                && kc_mask == event_mask
            {
                handled_by_wm = true;
                if let Some(ref func) = config.func {
                    if Self::debug_drag_enabled()
                        && event_mask.contains(Mods::CONTROL)
                        && mouse_button == MouseButton::Left
                        && is_client_click
                    {
                        let (px, py) = backend
                            .input_ops()
                            .get_pointer_position()
                            .unwrap_or((self.last_mouse_root.0, self.last_mouse_root.1));

                        let (win, geom) = clicked_client_key
                            .and_then(|ck| {
                                self.state
                                    .clients
                                    .get(ck)
                                    .map(|c| (c.win, c.geometry.clone()))
                            })
                            .map(|(w, g)| (Some(w), Some(g)))
                            .unwrap_or((clicked_win, None));

                        let func_name = Self::func_name(*func);
                        info!(
                            "[drag] Ctrl+Left ButtonPress: click_type={:?} win={:?} client={:?} func={} mods=0x{:x} pointer=({:.1},{:.1}) geom={:?}",
                            click_type,
                            win,
                            clicked_client_key,
                            func_name,
                            event_mask.bits(),
                            px,
                            py,
                            geom
                        );
                    }
                    let _ = func(self, backend, &config.arg);
                }
                break;
            }
        }

        if is_client_click {
            let _ = if handled_by_wm {
                backend
                    .input_ops()
                    .allow_events(AllowMode::AsyncPointer, time)
            } else {
                backend
                    .input_ops()
                    .allow_events(AllowMode::ReplayPointer, time)
            };
        }
        Ok(())
    }

    pub(crate) fn on_motion_notify_internal(
        &mut self,
        backend: &mut dyn Backend,
        _window: Option<WindowId>,
        root_x: i16,
        root_y: i16,
        _time: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // 1. 如果因为键盘操作等原因暂时阻塞了鼠标聚焦，直接返回
        if self.mouse_focus_blocked() {
            return Ok(());
        }
        // 3. 更新当前鼠标所在的显示器状态
        let new_monitor_key = self.recttomon(backend, root_x as i32, root_y as i32);
        if new_monitor_key != self.state.motion_mon {
            self.handle_monitor_switch_by_key(backend, new_monitor_key)?;
        }
        self.state.motion_mon = new_monitor_key;

        Ok(())
    }

    pub(crate) fn on_configure_request_internal(
        &mut self,
        backend: &mut dyn Backend,
        window: WindowId,
        mask_bits: u16,
        changes: WindowChanges,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(client_key) = self.wintoclient(window) {
            return self
                .handle_regular_configure_request_params(backend, client_key, mask_bits, changes);
        }

        self.handle_unmanaged_configure_request_params(backend, window, mask_bits, changes)
    }

    pub(crate) fn handle_regular_configure_request_params(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        mask_bits: u16,
        req: WindowChanges,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let is_popup = self.is_popup_like(backend, client_key);
        let mask = ConfigWindowBits::from_bits_truncate(mask_bits);

        let is_dock = self
            .state
            .clients
            .get(client_key)
            .map(|client| client.state.is_dock)
            .unwrap_or(false);

        if is_dock {
            if let Some(client) = self.state.clients.get(client_key) {
                info!(
                    "[dock_configure_request] win={:?} mask=0x{:x} req={:?} current={}x{}+{}+{}",
                    client.win,
                    mask_bits,
                    req,
                    client.geometry.w,
                    client.geometry.h,
                    client.geometry.x,
                    client.geometry.y
                );
                let changes = WindowChanges {
                    x: Some(client.geometry.x),
                    y: Some(client.geometry.y),
                    width: Some(client.geometry.w as u32),
                    height: Some(client.geometry.h as u32),
                    border_width: Some(client.geometry.border_w.max(0) as u32),
                    ..Default::default()
                };
                backend.window_ops().apply_window_changes(client.win, changes)?;
            }
            return Ok(());
        }

        if mask.contains(ConfigWindowBits::BORDER_WIDTH) {
            if let Some(border) = req.border_width {
                if !is_popup {
                    if let Some(client) = self.state.clients.get_mut(client_key) {
                        client.geometry.border_w = border as i32;
                    }
                }
            }
        }

        let (is_floating, mon_key_opt) = if let Some(client) = self.state.clients.get(client_key) {
            (client.state.is_floating, client.mon)
        } else {
            return Err("Client not found".into());
        };

        if is_floating {
            let (mx, my, mw, mh) = if let Some(mon_key) = mon_key_opt {
                let monitor = self
                    .state
                    .monitors
                    .get(mon_key)
                    .ok_or("Monitor not found")?;
                (
                    monitor.geometry.m_x,
                    monitor.geometry.m_y,
                    monitor.geometry.m_w,
                    monitor.geometry.m_h,
                )
            } else {
                return Err("Client has no monitor assigned".into());
            };

            let mut popup_apply: Option<WindowId> = None;
            let mut popup_clamp_request: Option<(i32, i32, i32, i32)> = None;
            let mut popup_is_dialog = false;

            let mut clamp_request: Option<(i32, i32, i32, i32)> = None;

            if let Some(client) = self.state.clients.get_mut(client_key) {
                if mask.contains(ConfigWindowBits::X) {
                    if let Some(x) = req.x {
                        client.geometry.old_x = client.geometry.x;
                        client.geometry.x = mx + x;
                    }
                }
                if mask.contains(ConfigWindowBits::Y) {
                    if let Some(y) = req.y {
                        client.geometry.old_y = client.geometry.y;
                        client.geometry.y = my + y;
                    }
                }
                if mask.contains(ConfigWindowBits::WIDTH) {
                    if let Some(w) = req.width {
                        client.geometry.old_w = client.geometry.w;
                        client.geometry.w = w as i32;
                    }
                }
                if mask.contains(ConfigWindowBits::HEIGHT) {
                    if let Some(h) = req.height {
                        client.geometry.old_h = client.geometry.h;
                        client.geometry.h = h as i32;
                    }
                }

                if (client.geometry.x + client.geometry.w) > mx + mw && client.state.is_floating {
                    client.geometry.x = mx + (mw / 2 - client.total_width() / 2);
                }
                if (client.geometry.y + client.geometry.h) > my + mh && client.state.is_floating {
                    client.geometry.y = my + (mh / 2 - client.total_height() / 2);
                }

                // Defer workarea clamping until after we release the mutable borrow.
                // Skip clamping for windows that cover the full monitor (e.g.
                // screenshot overlays that intentionally span strut areas).
                let covers_monitor = client.geometry.x <= mx
                    && client.geometry.y <= my
                    && client.total_width() >= mw
                    && client.total_height() >= mh;
                if client.state.is_floating && !client.state.is_fullscreen && !covers_monitor {
                    clamp_request = Some((
                        client.geometry.x,
                        client.geometry.y,
                        client.total_width(),
                        client.total_height(),
                    ));
                }

                if is_popup {
                    let types = backend.property_ops().get_window_types(client.win);
                    let should_clamp = types.contains(&WindowType::Notification)
                        || types.contains(&WindowType::Dialog);
                    popup_is_dialog = types.contains(&WindowType::Dialog);

                    if should_clamp {
                        popup_clamp_request = Some((
                            client.geometry.x,
                            client.geometry.y,
                            client.total_width(),
                            client.total_height(),
                        ));
                    }
                    popup_apply = Some(client.win);
                }
            }

            // Popup-like windows: apply workarea clamp for Dialog/Notification, then commit.
            if let Some(win) = popup_apply {
                if let (Some(mon_key), Some((x, y, total_w, total_h))) =
                    (mon_key_opt, popup_clamp_request)
                {
                    let mut clamp = self
                        .monitor_work_area(mon_key)
                        .unwrap_or(Rect::new(mx, my, mw, mh));

                    // For transient dialogs, intersect with parent bounds to avoid jumping
                    // across tiled columns.
                    if popup_is_dialog {
                        if let Some(parent_key) = self.parent_client_of(backend, client_key) {
                            if let Some(parent) = self.state.clients.get(parent_key) {
                                let parent_rect = Rect::new(
                                    parent.geometry.x,
                                    parent.geometry.y,
                                    parent.total_width(),
                                    parent.total_height(),
                                );

                                let left = clamp.x.max(parent_rect.x);
                                let top = clamp.y.max(parent_rect.y);
                                let right = (clamp.x + clamp.w).min(parent_rect.x + parent_rect.w);
                                let bottom = (clamp.y + clamp.h).min(parent_rect.y + parent_rect.h);
                                let w = (right - left).max(0);
                                let h = (bottom - top).max(0);
                                if w > 0 && h > 0 {
                                    clamp = Rect::new(left, top, w, h);
                                }
                            }
                        }
                    }

                    let min_x = clamp.x;
                    let max_x = clamp.x + clamp.w - total_w;
                    let clamped_x = if min_x <= max_x {
                        x.clamp(min_x, max_x)
                    } else {
                        min_x
                    };

                    let min_y = clamp.y;
                    let max_y = clamp.y + clamp.h - total_h;
                    let clamped_y = if min_y <= max_y {
                        y.clamp(min_y, max_y)
                    } else {
                        min_y
                    };

                    if let Some(client) = self.state.clients.get_mut(client_key) {
                        client.geometry.x = clamped_x;
                        client.geometry.y = clamped_y;
                    }
                }

                if let Some(client) = self.state.clients.get(client_key) {
                    let changes = WindowChanges {
                        x: Some(client.geometry.x),
                        y: Some(client.geometry.y),
                        width: Some(client.geometry.w as u32),
                        height: Some(client.geometry.h as u32),
                        ..Default::default()
                    };
                    backend.window_ops().apply_window_changes(win, changes)?;
                }

                return Ok(());
            }

            // Clamp floating (non-fullscreen) windows to the monitor workarea so they don't end
            // up under dock/statusbar reserved space.
            if let (Some(mon_key), Some((x, y, total_w, total_h))) = (mon_key_opt, clamp_request) {
                let clamp = self
                    .monitor_work_area(mon_key)
                    .unwrap_or(Rect::new(mx, my, mw, mh));

                let min_x = clamp.x;
                let max_x = clamp.x + clamp.w - total_w;
                let clamped_x = if min_x <= max_x {
                    x.clamp(min_x, max_x)
                } else {
                    min_x
                };

                let min_y = clamp.y;
                let max_y = clamp.y + clamp.h - total_h;
                let clamped_y = if min_y <= max_y {
                    y.clamp(min_y, max_y)
                } else {
                    min_y
                };

                if let Some(client) = self.state.clients.get_mut(client_key) {
                    if client.state.is_floating && !client.state.is_fullscreen {
                        client.geometry.x = clamped_x;
                        client.geometry.y = clamped_y;
                    }
                }
            }

            if mask.contains(ConfigWindowBits::X | ConfigWindowBits::Y)
                && !mask.contains(ConfigWindowBits::WIDTH | ConfigWindowBits::HEIGHT)
            {
                self.configure_client(backend, client_key)?;
            }

            if self.is_client_visible_by_key(client_key) {
                if let Some(client) = self.state.clients.get(client_key) {
                    let changes = WindowChanges {
                        x: Some(client.geometry.x),
                        y: Some(client.geometry.y),
                        width: Some(client.geometry.w as u32),
                        height: Some(client.geometry.h as u32),
                        ..Default::default()
                    };
                    backend
                        .window_ops()
                        .apply_window_changes(client.win, changes)?;
                }
            }
        } else {
            self.configure_client(backend, client_key)?;
        }

        Ok(())
    }

    pub(crate) fn handle_unmanaged_configure_request_params(
        &mut self,
        backend: &mut dyn Backend,
        window: WindowId,
        mask_bits: u16,
        req: WindowChanges,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!(
            "[handle_unmanaged_configure_request] unmanaged window={:?}",
            window
        );

        let mask = ConfigWindowBits::from_bits_truncate(mask_bits);
        let mut changes = WindowChanges::default();

        if mask.contains(ConfigWindowBits::X) {
            changes.x = req.x;
        }
        if mask.contains(ConfigWindowBits::Y) {
            changes.y = req.y;
        }
        if mask.contains(ConfigWindowBits::WIDTH) {
            changes.width = req.width;
        }
        if mask.contains(ConfigWindowBits::HEIGHT) {
            changes.height = req.height;
        }
        if mask.contains(ConfigWindowBits::BORDER_WIDTH) {
            changes.border_width = req.border_width;
        }
        if mask.contains(ConfigWindowBits::SIBLING) {
            changes.sibling = req.sibling;
        }
        if mask.contains(ConfigWindowBits::STACK_MODE) {
            changes.stack_mode = req.stack_mode;
        }

        backend.window_ops().apply_window_changes(window, changes)?;
        Ok(())
    }

}
