// Client management operations: window management, lifecycle, and configuration

use crate::backend::api::{Backend, Geometry, StackMode, WindowChanges, WindowType};
use crate::backend::common_define::{EventMaskBits, Mods, WindowId};
use crate::config::CONFIG;
use crate::core::animation::AnimationKind;
use crate::core::models::{ClientKey, MonitorKey, WMClient, WMMonitor};
use crate::core::types::Rect;
use crate::jwm::geometry::GeometryConstraints;
use crate::jwm::rules::RuleMatcher;
use crate::jwm::types::{WMRule, WMClickType};
use crate::Jwm;
use log::{debug, error, info, warn};

const NORMAL_STATE: u32 = 1;
const WITHDRAWN_STATE: u32 = 0;

impl Jwm {

    pub(crate) fn manage(
        &mut self,
        backend: &mut dyn Backend,
        win: WindowId,
        geom: &Geometry,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[manage] Managing window {:?}", win);
        if self.wintoclient(win).is_some() {
            warn!("Window {:?} already managed", win);
            return Ok(());
        }
        let cfg = CONFIG.load();
        let mut client = WMClient::new(win);
        client.geometry.x = geom.x as i32;
        client.geometry.old_x = geom.x as i32;
        client.geometry.y = geom.y as i32;
        client.geometry.old_y = geom.y as i32;
        client.geometry.w = geom.w as i32;
        client.geometry.old_w = geom.w as i32;
        client.geometry.h = geom.h as i32;
        client.geometry.old_h = geom.h as i32;
        client.geometry.old_border_w = geom.border as i32;
        client.state.client_fact = 1.0;
        client.name = self.fetch_window_title(backend, client.win);
        self.update_class_info(backend, &mut client);

        info!("{}", client);
        if client.is_status_bar(cfg.status_bar_name()) {
            info!("Detected status bar window");

            // With sequential creation, the first unmanaged bar is always the one we just created
            let matched_mon_id = self
                .secondary_bars
                .iter()
                .filter(|(_, bar)| bar.window.is_none())
                .min_by_key(|(mon_id, _)| **mon_id)
                .map(|(mon_id, _)| *mon_id);

            if let Some(mon_id) = matched_mon_id {
                info!(
                    "Matched bar window to monitor {} (sequential creation)",
                    mon_id
                );
                let client_key = self.insert_client(client);
                if let Some(bar) = self.secondary_bars.get_mut(&mon_id) {
                    bar.client_key = Some(client_key);
                    bar.window = Some(win);
                }
                return self.manage_secondary_statusbar(backend, client_key, win, mon_id);
            } else {
                // Don't warn - bar may have exited and been removed while window was still being mapped
                info!("No unmanaged bar found for status bar window, ignoring");
                return Ok(());
            }
        }

        // Check for external strut (polybar, trayer, etc.)
        self.check_strut_on_manage(backend, win);

        let client_key = self.insert_client(client);
        self.manage_regular_client(backend, client_key)?;

        // Broadcast window/new event
        let new_event_data = self
            .state
            .clients
            .get(client_key)
            .map(|c| (c.win.raw(), c.name.clone(), c.class.clone()));
        if let Some((id, name, class)) = new_event_data {
            self.broadcast_ipc_event(
                "window/new",
                serde_json::json!({
                    "id": id, "name": name, "class": class,
                }),
            );
        }

        // Appear animation for new windows
        {
            // Check if this is a scratchpad before starting default animation
            let is_scratchpad = self.scratchpad_pending_name.is_some();

            if cfg.animation_enabled() && !is_scratchpad {
                if let Some(client) = self.state.clients.get(client_key) {
                    let target = Rect::new(
                        client.geometry.x,
                        client.geometry.y,
                        client.geometry.w,
                        client.geometry.h,
                    );
                    // Start from 85% scale centered on target
                    let sw = (target.w as f32 * 0.85) as i32;
                    let sh = (target.h as f32 * 0.85) as i32;
                    let sx = target.x + (target.w - sw) / 2;
                    let sy = target.y + (target.h - sh) / 2;
                    let from = Rect::new(sx, sy, sw, sh);
                    self.animations.start(
                        client_key,
                        from,
                        target,
                        cfg.animation_duration(),
                        cfg.animation_easing(),
                        AnimationKind::Appear,
                    );
                }
            }
        }

        // Detect named scratchpad window
        if let Some(sp_name) = self.scratchpad_pending_name.take() {
            self.scratchpads.insert(sp_name.clone(), client_key);
            info!(
                "[manage] detected scratchpad '{}' client {:?}",
                sp_name, client_key
            );
            let mon_key = self.state.clients.get(client_key).and_then(|c| c.mon);
            if let Some(client) = self.state.clients.get_mut(client_key) {
                client.state.is_floating = true;
            }
            if let Some(mk) = mon_key {
                if let Some(area) = self.monitor_work_area(mk) {
                    let w = (area.w as f32 * 0.8) as i32;
                    let h = (area.h as f32 * 0.8) as i32;
                    let x = area.x + (area.w - w) / 2;
                    let y = area.y + (area.h - h) / 2;

                    // Suppress animation during resize to set target position
                    let suppress_flag = self.suppress_layout_animation;
                    self.suppress_layout_animation = true;
                    self.resize_client(backend, client_key, x, y, w, h, false);
                    self.suppress_layout_animation = suppress_flag;
                }
                let _ = self.focus(backend, Some(client_key));
                self.arrange(backend, Some(mk));

                // Start downward animation on initial appearance
                if let Some(area) = self.monitor_work_area(mk) {
                    let w = (area.w as f32 * 0.8) as i32;
                    let h = (area.h as f32 * 0.8) as i32;
                    let x = area.x + (area.w - w) / 2;
                    let y = area.y + (area.h - h) / 2;

                    if cfg.animation_enabled() {
                        let from_y = area.y - h;
                        let from_rect = Rect::new(x, from_y, w, h);
                        let to_rect = Rect::new(x, y, w, h);
                        info!(
                            "[manage] scratchpad '{}' initial animation from y={} to y={}",
                            sp_name, from_y, y
                        );
                        self.animations.start(
                            client_key,
                            from_rect,
                            to_rect,
                            cfg.animation_duration(),
                            cfg.animation_easing(),
                            AnimationKind::Appear,
                        );
                    }
                }
            }
        }

        Ok(())
    }

    pub(crate) fn setup_client_window(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.is_popup_like(backend, client_key) {
            if let Some(client) = self.state.clients.get_mut(client_key) {
                client.geometry.border_w = 0;
            }
            self.update_client_decoration(backend, client_key, false)?;

            self.configure_client(backend, client_key)?;
            if let Some(client) = self.state.clients.get(client_key) {
                self.setclientstate(backend, client.win, NORMAL_STATE as i64)?;
            }
            return Ok(());
        }

        let win = if let Some(client) = self.state.clients.get(client_key) {
            client.win
        } else {
            return Err("Client not found".into());
        };

        info!("Setting up window {:?}", win);

        if let Some(client) = self.state.clients.get_mut(client_key) {
            client.geometry.border_w = CONFIG.load().border_px() as i32;
        }

        self.update_client_decoration(backend, client_key, true)?;

        self.configure_client(backend, client_key)?;

        // When the compositor is NOT active, temporarily move the window
        // off-screen to avoid visual flicker before arrange() positions it.
        // With the compositor, rendering is done via TFP from the off-screen
        // pixmap, so the actual X11 position must stay correct for input
        // event delivery.
        if !backend.has_compositor() {
            let (x, y, w, h) = if let Some(client) = self.state.clients.get(client_key) {
                let offscreen_x = client.geometry.x + 2 * self.s_w;
                (
                    offscreen_x,
                    client.geometry.y,
                    client.geometry.w,
                    client.geometry.h,
                )
            } else {
                return Err("Client not found".into());
            };
            let changes = WindowChanges {
                x: Some(x),
                y: Some(y),
                width: Some(w as u32),
                height: Some(h as u32),
                ..Default::default()
            };
            backend.window_ops().apply_window_changes(win, changes)?;
        }

        if let Some(client) = self.state.clients.get(client_key) {
            self.setclientstate(backend, client.win, NORMAL_STATE as i64)?;
        }

        Ok(())
    }

    pub(crate) fn parent_client_of(
        &self,
        backend: &mut dyn Backend,
        child_key: ClientKey,
    ) -> Option<ClientKey> {
        let child_win = self.state.clients.get(child_key).map(|c| c.win)?;
        let parent_win = self.get_transient_for(backend, child_win)?;
        self.wintoclient(parent_win)
    }

    pub(crate) fn handle_new_client_focus(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (client_win, client_mon_key, is_never_focus) =
            if let Some(c) = self.state.clients.get(client_key) {
                (c.win, c.mon, c.state.never_focus)
            } else {
                return Err("Client not found".into());
            };
        let current_sel = self.get_selected_client_key();
        let current_sel_mon = self.state.sel_mon;
        if self.is_popup_like(backend, client_key) {
            let parent_key_opt = self.parent_client_of(backend, client_key);
            let sibling = parent_key_opt
                .and_then(|pk| self.state.clients.get(pk))
                .map(|pc| pc.win);
            let changes = WindowChanges {
                sibling: sibling,
                stack_mode: Some(StackMode::Above),
                ..Default::default()
            };
            backend
                .window_ops()
                .apply_window_changes(client_win, changes)?;

            let should_focus_this = if let Some(c) = self.state.clients.get(client_key) {
                if c.state.never_focus {
                    false
                } else {
                    let types = backend.property_ops().get_window_types(c.win);
                    let is_transient = backend.property_ops().transient_for(c.win).is_some();

                    // Transient 窗口（用户交互触发的子窗口）应获得焦点
                    if is_transient {
                        true
                    } else {
                        let is_no_auto_focus = types.contains(&WindowType::Tooltip)
                            || types.contains(&WindowType::Notification)
                            || types.contains(&WindowType::Dnd)
                            || types.contains(&WindowType::Combo);
                        !is_no_auto_focus
                    }
                }
            } else {
                false
            };

            if should_focus_this {
                self.focus(backend, Some(client_key))?;
            } else {
                if let Some(pk) = parent_key_opt {
                    let _ = self.set_client_focus_by_key(backend, pk);
                } else if let Some(prev_sel) = current_sel {
                    let _ = self.set_client_focus_by_key(backend, prev_sel);
                } else {
                    let _ = self.set_root_focus(backend);
                }
            }

            // Update last_stacking so the compositor scene includes this popup.
            // Without this, the compositor overlay hides newly mapped dialogs.
            if let Some(mon_key) = client_mon_key {
                let _ = self.restack(backend, Some(mon_key));
            }

            return Ok(());
        }
        let is_on_selected_monitor = client_mon_key.is_some() && client_mon_key == current_sel_mon;
        if is_on_selected_monitor {
            if let Some(mon_key) = client_mon_key {
                if let Some(monitor) = self.state.monitors.get_mut(mon_key) {
                    monitor.sel = Some(client_key);
                }
                self.arrange(backend, Some(mon_key));
            }

            if !is_never_focus {
                if let Some(prev_sel) = current_sel {
                    if prev_sel != client_key {
                        self.unfocus_client(backend, prev_sel, false)?;
                    }
                }
                self.focus(backend, Some(client_key))?;
            } else {
                if let Some(prev_sel) = current_sel {
                    let _ = self.set_client_focus_by_key(backend, prev_sel);
                } else {
                    let _ = self.set_root_focus(backend);
                }
            }
            return Ok(());
        }

        if let Some(target_mon_key) = client_mon_key {
            if let Some(monitor) = self.state.monitors.get_mut(target_mon_key) {
                monitor.sel = Some(client_key);
            }
            self.arrange(backend, Some(target_mon_key));
        }

        if CONFIG.load().behavior().focus_follows_new_window && !is_never_focus {
            if let Some(target_mon_key) = client_mon_key {
                self.switch_to_monitor(backend, target_mon_key)?;
                self.focus(backend, Some(client_key))?;
            }
        } else {
            if let Some(prev_sel) = current_sel {
                let _ = self.set_client_focus_by_key(backend, prev_sel);
            } else {
                let _ = self.set_root_focus(backend);
            }
        }

        Ok(())
    }

    pub(crate) fn grabbuttons(&mut self, backend: &mut dyn Backend, client_key: ClientKey, focused: bool) {
        let win = if let Some(c) = self.state.clients.get(client_key) {
            c.win
        } else {
            return;
        };
        let _ = backend.window_ops().ungrab_all_buttons(win);

        if focused {
            let buttons = crate::config::CONFIG.load().get_buttons();
            let modifiers_combinations = [
                Mods::NONE,
                Mods::CAPS,
                Mods::NUMLOCK,
                Mods::CAPS | Mods::NUMLOCK,
            ];
            for btn_conf in buttons {
                if btn_conf.click_type == WMClickType::ClickClientWin {
                    let clean_conf_mask = btn_conf.mask
                        & (Mods::SHIFT
                            | Mods::CONTROL
                            | Mods::ALT
                            | Mods::SUPER
                            | Mods::MOD2
                            | Mods::MOD3
                            | Mods::MOD5);
                    for &lock_state in &modifiers_combinations {
                        let final_mask = clean_conf_mask | lock_state;
                        let _ = backend.window_ops().grab_button(
                            win,
                            btn_conf.button.to_u8(),
                            (EventMaskBits::BUTTON_PRESS | EventMaskBits::BUTTON_RELEASE).bits(),
                            final_mask,
                        );
                    }
                }
            }
        } else {
            log::info!(
                "[grabbuttons] Setting grab_button_any_anymod on unfocused window {:?}",
                win
            );
            let _ = backend.window_ops().grab_button_any_anymod(
                win,
                (EventMaskBits::BUTTON_PRESS | EventMaskBits::BUTTON_RELEASE).bits(),
            );
        }
    }

    pub(crate) fn manage_regular_client(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.handle_transient_for(backend, client_key)?;

        self.adjust_client_position(backend, client_key);

        self.setup_client_window(backend, client_key)?;

        self.updatewindowtype(backend, client_key);
        self.updatesizehints(backend, client_key)?;
        self.updatewmhints(backend, client_key);
        self.apply_motif_hints(backend, client_key);
        self.apply_gtk_frame_extents(backend, client_key);
        self.set_initial_frame_extents(backend, client_key);
        self.set_initial_allowed_actions(backend, client_key);
        self.read_sync_counter(backend, client_key);

        self.attach_back(client_key);
        self.attachstack(client_key);

        self.register_client_events(backend, client_key)?;
        self.grabbuttons(backend, client_key, false);

        let already_mapped = match self.state.clients.get(client_key) {
            Some(client) => backend
                .window_ops()
                .get_window_attributes(client.win)
                .map(|a| a.map_state_viewable)
                .unwrap_or(false),
            None => false,
        };
        if !already_mapped {
            self.map_client_window(backend, client_key)?;
        }

        self.update_net_client_list(backend)?;

        self.handle_new_client_focus(backend, client_key)?;

        self.suppress_mouse_focus_until =
            Some(std::time::Instant::now() + std::time::Duration::from_millis(300));

        Ok(())
    }

    pub(crate) fn handle_transient_for(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let win = if let Some(client) = self.state.clients.get(client_key) {
            client.win
        } else {
            return Err("Client not found".into());
        };

        match self.get_transient_for(backend, win) {
            Some(transient_for_win) => {
                if let Some(parent_client_key) = self.wintoclient(transient_for_win) {
                    let (parent_mon, parent_tags) =
                        if let Some(parent) = self.state.clients.get(parent_client_key) {
                            (parent.mon, parent.state.tags)
                        } else {
                            return Err("Parent client not found".into());
                        };

                    if let Some(client) = self.state.clients.get_mut(client_key) {
                        client.mon = parent_mon;
                        client.state.tags = parent_tags;
                        client.state.is_floating = true;
                        warn!(
                            "[handle_transient_for] Client {} is transient for parent",
                            client
                        );
                    }
                } else {
                    info!("[handle_transient_for] parent client is None, still mark floating");
                    if let Some(client) = self.state.clients.get_mut(client_key) {
                        client.mon = self.state.sel_mon;
                        client.state.is_floating = true;
                    }
                    self.applyrules_by_key(backend, client_key);
                }
            }
            None => {
                info!("no WM_TRANSIENT_FOR property");
                if let Some(client) = self.state.clients.get_mut(client_key) {
                    client.mon = self.state.sel_mon;
                }
                self.applyrules_by_key(backend, client_key);
            }
        }
        Ok(())
    }

    pub(crate) fn update_class_info(&mut self, backend: &mut dyn Backend, client: &mut WMClient) {
        if let Some((inst, cls)) = self.get_wm_class(backend, client.win) {
            client.instance = inst;
            client.class = cls;
        }
    }

    pub(crate) fn rule_matches(&self, rule: &WMRule, name: &str, class: &str, instance: &str) -> bool {
        RuleMatcher::matches(rule, name, class, instance)
    }

    pub(crate) fn apply_single_rule(&mut self, client_key: ClientKey, rule: &WMRule) {
        if let Some(client) = self.state.clients.get_mut(client_key) {
            info!("[apply_single_rule] Applying rule: {:?}", rule);
            client.state.is_floating = rule.is_floating;
            if rule.tags > 0 {
                client.state.tags |= rule.tags as u32;
            }
            if rule.monitor >= 0 {
                let target_monitor = self
                    .state
                    .monitor_order
                    .iter()
                    .find(|&&mon_key| {
                        if let Some(monitor) = self.state.monitors.get(mon_key) {
                            monitor.num == rule.monitor
                        } else {
                            false
                        }
                    })
                    .copied();
                if let Some(mon_key) = target_monitor {
                    client.mon = Some(mon_key);
                    info!(
                        "[apply_single_rule] Assigned client to monitor {}",
                        rule.monitor
                    );
                }
            }
            info!(
                "[apply_single_rule] Applied - floating: {}, tags: {}, monitor: {}",
                client.state.is_floating, client.state.tags, rule.monitor
            );
        }
    }

    pub(crate) fn set_default_tags(&mut self, client_key: ClientKey) {
        if let Some(client) = self.state.clients.get_mut(client_key) {
            let current_tags = client.state.tags & CONFIG.load().tagmask();
            if current_tags > 0 {
                client.state.tags = current_tags;
            } else {
                if let Some(mon_key) = client.mon {
                    if let Some(monitor) = self.state.monitors.get(mon_key) {
                        client.state.tags = monitor.get_active_tags();
                    }
                } else {
                    client.state.tags = 1;
                }
            }
            info!(
                "[set_default_tags] Set tags to {} for client {:?}",
                client.state.tags, client.win
            );
        }
    }

    pub(crate) fn applyrules_by_key(&mut self, backend: &mut dyn Backend, client_key: ClientKey) {
        let (win, name, mut class, mut instance) =
            if let Some(client) = self.state.clients.get(client_key) {
                (
                    client.win,
                    client.name.clone(),
                    client.class.clone(),
                    client.instance.clone(),
                )
            } else {
                return;
            };
        if class.is_empty() && instance.is_empty() {
            if let Some((inst, cls)) = self.get_wm_class(backend, win) {
                instance = inst;
                class = cls;

                if let Some(client) = self.state.clients.get_mut(client_key) {
                    client.instance = instance.clone();
                    client.class = class.clone();
                }
            }
        }
        info!(
            "[applyrules_by_key] win: {:?}, name: '{}', instance: '{}', class: '{}'",
            win, name, instance, class
        );
        if let Some(client) = self.state.clients.get_mut(client_key) {
            client.state.is_floating = false;
        }
        if RuleMatcher::should_auto_float(&name, &class, &instance) {
            if let Some(client) = self.state.clients.get_mut(client_key) {
                client.state.is_floating = true;
            }
            info!("No window info available, setting as floating");
        }
        let mut rule_applied = false;
        for rule in &CONFIG.load().get_rules() {
            if self.rule_matches(rule, &name, &class, &instance) {
                self.apply_single_rule(client_key, rule);
                rule_applied = true;
                break;
            }
        }
        if !rule_applied {
            info!("No matching rule found, using defaults");
        }
        self.set_default_tags(client_key);
        if let Some(client) = self.state.clients.get(client_key) {
            info!(
                "Final state - class: '{}', instance: '{}', name: '{}', tags: {}, floating: {}",
                client.class,
                client.instance,
                client.name,
                client.state.tags,
                client.state.is_floating
            );
        }
    }

    pub(crate) fn register_client_events(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let win = if let Some(client) = self.state.clients.get(client_key) {
            client.win
        } else {
            return Err("Client not found".into());
        };

        let mask = (EventMaskBits::ENTER_WINDOW
            | EventMaskBits::FOCUS_CHANGE
            | EventMaskBits::PROPERTY_CHANGE
            | EventMaskBits::STRUCTURE_NOTIFY
            | EventMaskBits::POINTER_MOTION)
            .bits();
        backend.window_ops().change_event_mask(win, mask)?;
        let _ = backend.window_ops().shape_select_input(win);
        if backend.window_ops().get_window_shaped(win) {
            backend.compositor_set_window_shaped(win, true);
        }
        info!(
            "[register_client_events] Events registered for window {:?}",
            win
        );
        Ok(())
    }

    pub(crate) fn map_client_window(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let win = if let Some(client) = self.state.clients.get(client_key) {
            client.win
        } else {
            return Err("Client not found".into());
        };

        backend.window_ops().map_window(win)?;
        info!("[map_client_window] Successfully mapped window {:?}", win);
        Ok(())
    }

    pub(crate) fn manage_secondary_statusbar(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        win: WindowId,
        monitor_id: i32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("Managing secondary bar for monitor {}", monitor_id);

        let mon_key = self.get_monitor_by_id(monitor_id);
        if let Some(client) = self.state.clients.get_mut(client_key) {
            client.mon = mon_key;
            client.state.never_focus = false;
            client.state.is_floating = true;
            client.state.is_dock = true;
            client.state.tags = CONFIG.load().tagmask();
            client.geometry.border_w = 0;
        }

        // Position this bar on its designated monitor
        self.position_secondary_bar_on_monitor(backend, client_key, win, monitor_id)?;

        self.setup_statusbar_window_by_key(backend, client_key)?;

        backend.window_ops().map_window(win)?;
        Ok(())
    }

    pub(crate) fn position_secondary_bar_on_monitor(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        win: WindowId,
        monitor_id: i32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mon_key = match self.get_monitor_by_id(monitor_id) {
            Some(k) => k,
            None => {
                warn!("Monitor {} not found for secondary bar", monitor_id);
                return Ok(());
            }
        };

        let monitor = match self.state.monitors.get(mon_key) {
            Some(m) => m.clone(),
            None => return Ok(()),
        };

        let show_bar = monitor
            .pertag
            .as_ref()
            .and_then(|p| p.show_bars.get(p.cur_tag))
            .copied()
            .unwrap_or(true);

        let cfg = CONFIG.load();
        let actual_bar_height = cfg.status_bar_height();
        let bar_height = if show_bar { actual_bar_height } else { 0 };

        if let Some(client) = self.state.clients.get_mut(client_key) {
            if show_bar {
                let pad = cfg.status_bar_padding();
                let border_width = client.geometry.border_w;
                client.geometry.x = monitor.geometry.m_x + pad;
                client.geometry.y = monitor.geometry.m_y + pad;
                client.geometry.w = monitor.geometry.m_w - 2 * pad - 2 * border_width;
                client.geometry.h = bar_height;
                info!(
                    "[position_secondary_bar_on_monitor] win={:?} target={}x{}+{}+{} pad={} monitor_id={}",
                    win,
                    client.geometry.w,
                    client.geometry.h,
                    client.geometry.x,
                    client.geometry.y,
                    pad,
                    monitor_id
                );

                let changes = WindowChanges {
                    x: Some(client.geometry.x),
                    y: Some(client.geometry.y),
                    width: Some(client.geometry.w as u32),
                    height: Some(client.geometry.h as u32),
                    ..Default::default()
                };
                backend.window_ops().apply_window_changes(win, changes)?;
                backend.compositor_force_full_redraw();
            } else {
                // Hide bar by moving it off-screen above the monitor
                let hidden_x = monitor.geometry.m_x;
                let hidden_y = monitor.geometry.m_y - actual_bar_height;
                if let Some(client) = self.state.clients.get_mut(client_key) {
                    client.geometry.x = hidden_x;
                    client.geometry.y = hidden_y;
                }
                let changes = WindowChanges {
                    x: Some(hidden_x),
                    y: Some(hidden_y),
                    ..Default::default()
                };
                backend.window_ops().apply_window_changes(win, changes)?;
            }
        }

        // Set strut after releasing the mutable borrow
        if show_bar {
            self.set_bar_strut(backend, win, &monitor, bar_height)?;
        } else {
            self.remove_bar_strut(backend, win)?;
        }

        Ok(())
    }

    pub(crate) fn set_bar_strut(
        &self,
        backend: &mut dyn Backend,
        bar_win: WindowId,
        mon: &WMMonitor,
        bar_height: i32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let top_amount = bar_height.max(0) as u32;
        let top_start_x = mon.geometry.m_x.max(0) as u32;
        let top_end_x = (mon.geometry.m_x + mon.geometry.m_w - 1).max(0) as u32;
        Ok(backend.property_ops().set_window_strut_top(
            bar_win,
            top_amount,
            top_start_x,
            top_end_x,
        )?)
    }

    pub(crate) fn remove_bar_strut(
        &self,
        backend: &mut dyn Backend,
        bar_win: WindowId,
    ) -> Result<(), Box<dyn std::error::Error>> {
        Ok(backend.property_ops().clear_window_strut(bar_win)?)
    }

    pub(crate) fn setup_statusbar_window_by_key(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let win = if let Some(client) = self.state.clients.get(client_key) {
            client.win
        } else {
            return Err("Client not found".into());
        };
        info!(
            "[setup_statusbar_window_by_key] Setting up statusbar window {:?}",
            win
        );

        let mask_bits = (EventMaskBits::STRUCTURE_NOTIFY
            | EventMaskBits::PROPERTY_CHANGE
            | EventMaskBits::ENTER_WINDOW
            | EventMaskBits::FOCUS_CHANGE)
            .bits();
        backend.window_ops().change_event_mask(win, mask_bits)?;
        backend.property_ops().set_window_type_dock(win)?;
        self.configure_client(backend, client_key)?;
        info!(
            "[setup_statusbar_window_by_key] Statusbar window setup completed for {:?}",
            win
        );
        Ok(())
    }

    pub(crate) fn get_monitor_by_id(&self, monitor_id: i32) -> Option<MonitorKey> {
        self.state
            .monitors
            .iter()
            .find(|(_, monitor)| monitor.num == monitor_id)
            .map(|(key, _)| key)
    }

    pub(crate) fn maprequest(
        &mut self,
        backend: &mut dyn Backend,
        window: WindowId,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let window_attr = backend.window_ops().get_window_attributes(window)?;
        if window_attr.override_redirect {
            debug!(
                "Ignoring map request for override_redirect window: {:?}",
                window
            );
            return Ok(());
        }
        if self.wintoclient(window).is_none() {
            let geom = backend.window_ops().get_geometry(window)?;
            self.manage(backend, window, &geom)?;
        } else {
            debug!(
                "Window {:?} is already managed, ignoring map request",
                window
            );
        }
        Ok(())
    }

    pub(crate) fn handle_monitor_switch_by_key(
        &mut self,
        backend: &mut dyn Backend,
        new_monitor_key: Option<MonitorKey>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let current_sel = self.get_selected_client_key();
        if let Some(sel_key) = current_sel {
            self.unfocus_client(backend, sel_key, true)?;
        }

        self.state.sel_mon = new_monitor_key;

        self.focus(backend, None)?;

        if let Some(monitor_key) = new_monitor_key {
            if let Some(monitor) = self.state.monitors.get(monitor_key) {
                debug!("Switched to monitor {} via mouse motion", monitor.num);
            }
        }

        Ok(())
    }

    pub(crate) fn unmanage(
        &mut self,
        backend: &mut dyn Backend,
        client_key: Option<ClientKey>,
        destroyed: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        debug!("unmanage");
        let client_key = match client_key {
            Some(key) => key,
            None => return Ok(()),
        };

        let win = if let Some(client) = self.state.clients.get(client_key) {
            client.win
        } else {
            warn!("[unmanage] Client {:?} not found", client_key);
            return Ok(());
        };

        // Remove any external strut reservation for this window
        self.remove_strut_on_unmanage(backend, win);

        // Broadcast window/close event before removing the client
        let close_event_data = self
            .state
            .clients
            .get(client_key)
            .map(|c| (c.win.raw(), c.name.clone()));
        if let Some((id, name)) = close_event_data {
            self.broadcast_ipc_event(
                "window/close",
                serde_json::json!({
                    "id": id, "name": name,
                }),
            );
        }

        self.unmanage_regular_client(backend, client_key, destroyed)?;
        Ok(())
    }

    pub(crate) fn is_popup_like(&self, backend: &mut dyn Backend, client_key: ClientKey) -> bool {
        let client = if let Some(client) = self.state.clients.get(client_key) {
            client
        } else {
            return false;
        };
        RuleMatcher::is_popup_like(backend, client.win)
    }

    pub(crate) fn adjust_client_position(&mut self, backend: &mut dyn Backend, client_key: ClientKey) {
        info!("[adjust_client_position]");
        let (client_total_width, client_mon_key_opt, win) =
            if let Some(client) = self.state.clients.get(client_key) {
                (client.total_width(), client.mon, client.win)
            } else {
                error!("Client {:?} not found", client_key);
                return;
            };

        // Most popup-like windows (menus/tooltips/etc.) should not be clamped by the WM.
        // Notifications are a special case: if they spawn at monitor y=0 they can end up
        // hidden under the status bar. Dialogs are another special case: apps sometimes
        // position transient dialogs at y=0, and we still want them to respect the monitor
        // workarea (i.e. avoid any top strut / status bar).
        if self.is_popup_like(backend, client_key) {
            let types = backend.property_ops().get_window_types(win);
            let should_clamp =
                types.contains(&WindowType::Notification) || types.contains(&WindowType::Dialog);

            if !should_clamp {
                info!("is_popup_like (skip position adjustment)");
                return;
            }

            if types.contains(&WindowType::Dialog) {
                info!("popup-like Dialog (clamp to workarea)");
            }
        }
        let client_mon_key = if let Some(mon_key) = client_mon_key_opt {
            mon_key
        } else {
            error!("Client has no monitor assigned!");
            return;
        };
        let (mon_wx, mon_wy, mon_ww, mon_wh) =
            if let Some(monitor) = self.state.monitors.get(client_mon_key) {
                (
                    monitor.geometry.w_x,
                    monitor.geometry.w_y,
                    monitor.geometry.w_w,
                    monitor.geometry.w_h,
                )
            } else {
                error!("Monitor {:?} not found", client_mon_key);
                return;
            };
        info!("{:?}", win);
        let (mut client_x, mut client_y, _client_w, _client_h) =
            if let Some(client) = self.state.clients.get(client_key) {
                (
                    client.geometry.x,
                    client.geometry.y,
                    client.geometry.w,
                    client.geometry.h,
                )
            } else {
                return;
            };
        let client_total_height = if let Some(client) = self.state.clients.get(client_key) {
            client.total_height()
        } else {
            return;
        };

        // Windows whose requested geometry covers the full monitor (e.g. screenshot
        // overlays) intentionally want to include areas reserved by struts/status
        // bars.  Skip all workarea clamping so they are not shifted into the
        // workarea.
        if let Some(monitor) = self.state.monitors.get(client_mon_key) {
            let window_rect = Rect::new(client_x, client_y, client_total_width, client_total_height);
            let monitor_rect = Rect::new(
                monitor.geometry.m_x,
                monitor.geometry.m_y,
                monitor.geometry.m_w,
                monitor.geometry.m_h,
            );
            if GeometryConstraints::covers_full_monitor(&window_rect, &monitor_rect) {
                info!(
                    "Window covers full monitor ({}x{} at ({},{})), skipping workarea clamping",
                    client_total_width, client_total_height, client_x, client_y
                );
                return;
            }
        }

        if client_x + client_total_width > mon_wx + mon_ww {
            client_x = mon_wx + mon_ww - client_total_width;
            info!("Adjusted X to prevent overflow: {}", client_x);
        }
        if client_y + client_total_height > mon_wy + mon_wh {
            client_y = mon_wy + mon_wh - client_total_height;
            info!("Adjusted Y to prevent overflow: {}", client_y);
        }
        if client_x < mon_wx {
            client_x = mon_wx;
            info!("Adjusted X to workarea left: {}", client_x);
        }
        if client_y < mon_wy {
            client_y = mon_wy;
            info!("Adjusted Y to workarea top: {}", client_y);
        }

        // Clamp to workarea by default (so dialogs avoid the status bar strut), and additionally
        // clamp transient dialogs to their parent window bounds so they don't jump across tiled
        // columns (e.g. right tile spawning a dialog at x=0).
        let mut clamp = self
            .monitor_work_area(client_mon_key)
            .unwrap_or(Rect::new(mon_wx, mon_wy, mon_ww, mon_wh));

        let types = backend.property_ops().get_window_types(win);
        let is_dialog = types.contains(&WindowType::Dialog);
        if is_dialog {
            if let Some(parent_key) = self.parent_client_of(backend, client_key) {
                if let Some(parent) = self.state.clients.get(parent_key) {
                    let parent_rect = Rect::new(
                        parent.geometry.x,
                        parent.geometry.y,
                        parent.total_width(),
                        parent.total_height(),
                    );

                    // Intersect clamp rect with parent rect.
                    if let Some(intersection) = GeometryConstraints::rect_intersection(&clamp, &parent_rect) {
                        clamp = intersection;
                        info!(
                            "Dialog transient clamp: parent=({},{} {}x{}) clamp=({},{} {}x{})",
                            parent_rect.x,
                            parent_rect.y,
                            parent_rect.w,
                            parent_rect.h,
                            clamp.x,
                            clamp.y,
                            clamp.w,
                            clamp.h
                        );
                    } else {
                        warn!(
                            "Skip transient parent clamp because intersection is empty; parent=({},{} {}x{}) clamp=({},{} {}x{})",
                            parent_rect.x,
                            parent_rect.y,
                            parent_rect.w,
                            parent_rect.h,
                            clamp.x,
                            clamp.y,
                            clamp.w,
                            clamp.h
                        );
                    }
                }
            }
        }

        // Clamp to the computed clamp rect (workarea or workarea∩parent).
        GeometryConstraints::clamp_rect_to_boundary(
            &mut client_x,
            &mut client_y,
            client_total_width,
            client_total_height,
            &clamp,
        );

        // Keep within the monitor bounds as a final guard.
        let monitor_bounds = Rect::new(mon_wx, mon_wy, mon_ww, mon_wh);
        GeometryConstraints::clamp_rect_to_boundary(
            &mut client_x,
            &mut client_y,
            client_total_width,
            client_total_height,
            &monitor_bounds,
        );
        if let Some(client) = self.state.clients.get_mut(client_key) {
            client.geometry.x = client_x;
            client.geometry.y = client_y;
            info!(
                "Final position: ({}, {}) {}x{}",
                client.geometry.x, client.geometry.y, client.geometry.w, client.geometry.h
            );
        }
    }

    pub(crate) fn unmanage_regular_client(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        destroyed: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.animations.remove(client_key);
        let win = self.state.clients.get(client_key).map(|c| c.win);
        if let Some(client) = self.state.clients.get(client_key) {
            info!("[unmanage_regular_client] Removing client {}", client);
        }
        self.scratchpads.retain(|_, &mut v| v != client_key);
        let mon_key = self
            .state
            .clients
            .get(client_key)
            .and_then(|client| client.mon);
        if let Some(mon_key) = mon_key {
            self.clear_pertag_references(client_key, mon_key);
        }
        self.detach(client_key);
        self.detachstack(client_key);
        if !destroyed {
            self.cleanup_window_state(backend, client_key)?;
        }
        if let Some(win) = win {
            self.state.win_to_client.remove(&win);
        }
        self.state.clients.remove(client_key);
        self.state.client_order.retain(|&k| k != client_key);
        self.state.client_stack_order.retain(|&k| k != client_key);
        self.focus(backend, None)?;
        self.update_net_client_list(backend)?;
        if let Some(mon_key) = mon_key {
            self.arrange(backend, Some(mon_key));
        }

        Ok(())
    }

    pub(crate) fn clear_pertag_references(&mut self, client_key: ClientKey, mon_key: MonitorKey) {
        if let Some(monitor) = self.state.monitors.get_mut(mon_key) {
            if let Some(ref mut pertag) = monitor.pertag {
                for i in 0..=CONFIG.load().tags_length() {
                    if pertag.sel[i] == Some(client_key) {
                        pertag.sel[i] = None;
                    }
                }
            }
        }
    }

    pub(crate) fn cleanup_window_state(
        &self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let client = if let Some(client) = self.state.clients.get(client_key) {
            client
        } else {
            return Err("Client not found".into());
        };
        let win = client.win;
        let old_border_w = client.geometry.old_border_w;
        if let Err(e) = backend
            .window_ops()
            .change_event_mask(win, EventMaskBits::NONE.bits())
        {
            warn!("[cleanup_window_state] Failed to clear event mask: {:?}", e);
        }
        let changes = WindowChanges {
            border_width: Some(old_border_w as u32),
            ..Default::default()
        };
        if let Err(e) = backend.window_ops().apply_window_changes(win, changes) {
            log::warn!(
                "[cleanup_window_state] Failed to restore border width: {:?}",
                e
            );
        }
        if let Err(e) = backend.window_ops().ungrab_all_buttons(win) {
            warn!("[cleanup_window_state] Failed to ungrab buttons: {:?}", e);
        }
        if let Err(e) = self.setclientstate(backend, win, WITHDRAWN_STATE as i64) {
            warn!("[cleanup_window_state] Failed to set client state: {:?}", e);
        }

        info!(
            "[cleanup_window_state] Window cleanup completed for {:?}",
            win
        );
        Ok(())
    }

    pub(crate) fn unmapnotify(
        &mut self,
        backend: &mut dyn Backend,
        window: WindowId,
        from_configure: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // info!("[unmapnotify]");
        if let Some(client_key) = self.wintoclient(window) {
            if from_configure {
                debug!("Unmap from configure for window {:?}", window);
                let client = if let Some(client) = self.state.clients.get(client_key) {
                    client
                } else {
                    return Ok(());
                };
                self.setclientstate(backend, client.win, WITHDRAWN_STATE as i64)?;
            } else {
                debug!("Real unmap for window {:?}, unmanaging", window);
                self.unmanage(backend, Some(client_key), false)?;
            }
        } else {
            debug!("Unmap event for unmanaged window: 0{:?}", window);
        }
        Ok(())
    }
}
