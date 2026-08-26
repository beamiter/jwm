// Client management operations: window management, lifecycle, and configuration

use crate::Jwm;
use crate::backend::api::{
    Backend, Geometry, MinimizedRestoreState, NetWmState, StackMode, WindowChanges, WindowType,
};
use crate::backend::common_define::{EventMaskBits, Mods, WindowId};
use crate::config::{BackendFamily, CONFIG, get_backend_family};
use crate::core::animation::AnimationKind;
use crate::core::models::{ClientGeometry, ClientKey, MonitorKey, WMClient, WMMonitor};
use crate::core::types::Rect;
use crate::jwm::geometry::GeometryConstraints;
use crate::jwm::rules::RuleMatcher;
use crate::jwm::statusbar::StatusBarBuilder;
use crate::jwm::types::{
    WITHDRAWN_STATE, WMClickType, WMRule, wm_state_for_minimized, wm_state_or_ewmh_is_minimized,
};
use crate::jwm::visibility::{
    hidden_x_left_of_desktop, restore_hidden_geometry, stage_hidden_geometry,
};
use crate::jwm::window_state::{
    minimized_order_is_safe_to_recover, next_minimized_order, observe_minimized_order,
    x11_geometry_fully_left_of_desktop,
};
use log::{debug, error, info, warn};

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
        client.pid = backend.property_ops().get_window_pid(client.win);

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
                self.secondary_bar_failures.remove(&mon_id);
                self.secondary_bar_retry_after.remove(&mon_id);
                return self.manage_secondary_statusbar(backend, client_key, win, mon_id);
            } else {
                // Don't warn - bar may have exited and been removed while window was still being mapped
                info!("No unmanaged bar found for status bar window, ignoring");
                return Ok(());
            }
        }

        // Check for external strut (polybar, trayer, etc.)
        self.check_strut_on_manage(backend, win);

        // A seamless restart leaves managed clients and their state properties
        // in place. Capture both current ICCCM IconicState and the legacy JWM
        // form that only wrote EWMH Hidden before setup rewrites/normalizes it.
        let initial_wm_state = backend.property_ops().get_wm_state(win)?;
        let initial_ewmh_hidden = backend
            .property_ops()
            .has_net_wm_state_flag(win, NetWmState::Hidden)?;
        let publicly_minimized =
            wm_state_or_ewmh_is_minimized(initial_wm_state, initial_ewmh_hidden);
        client.state.is_hidden = publicly_minimized;
        let desktop_left = self.desktop_left_edge();
        let server_geometry_fully_left = x11_geometry_fully_left_of_desktop(*geom, desktop_left);
        let restore_candidate = if publicly_minimized || server_geometry_fully_left {
            backend.property_ops().get_minimized_restore_state(win)?
        } else {
            None
        };
        let mut restore_candidate = restore_candidate;
        if publicly_minimized && let Some(state) = restore_candidate.as_mut() {
            if minimized_order_is_safe_to_recover(state.minimized_order) {
                let observed = observe_minimized_order(state.minimized_order);
                debug_assert!(observed);
            } else {
                let advertised = state.minimized_order;
                state.minimized_order =
                    next_minimized_order().ok_or("minimized Dock order space exhausted")?;
                warn!(
                    "[manage] rebasing untrusted minimized order {advertised} to {} for {win:?}",
                    state.minimized_order
                );
            }
        }
        let interrupted_restore =
            !publicly_minimized && server_geometry_fully_left && restore_candidate.is_some();
        let minimized_restore = if publicly_minimized {
            if let Some(state) = restore_candidate {
                client.state.minimized_order = state.minimized_order;
                Some(state)
            } else {
                client.state.minimized_order =
                    next_minimized_order().ok_or("minimized Dock order space exhausted")?;
                None
            }
        } else if interrupted_restore {
            let state = restore_candidate.expect("interrupted restore has a snapshot");
            // The previous process published NormalState and then died before
            // it could move the real window out of the hidden parking slot.
            // Recover semantic placement, but do not resurrect a Dock item.
            client.state.minimized_order = 0;
            Some(state)
        } else {
            // A stale private property is never authoritative for a Normal
            // on-screen client. A Normal client still fully parked is handled
            // above as a crash-interrupted restore.
            if let Err(error) = backend.property_ops().clear_minimized_restore_state(win) {
                warn!("[manage] could not clear stale restore state for {win:?}: {error}");
            }
            None
        };
        if client.state.is_hidden
            && let Err(error) =
                backend
                    .property_ops()
                    .set_net_wm_state_flag(win, NetWmState::Hidden, true)
        {
            warn!("[manage] could not normalize minimized EWMH state for {win:?}: {error}");
        }

        let client_key = self.insert_client(client);
        self.manage_regular_client(backend, client_key, minimized_restore, interrupted_restore)?;

        // A pending scratchpad is an exact process identity, not a global
        // "next window" flag. Status bars returned above without consuming
        // anything, and all fallible regular-client setup has completed before
        // the registry entry is committed here.
        let claimed_scratchpad_name = self
            .state
            .clients
            .get(client_key)
            .and_then(|client| client.pid)
            .and_then(|pid| self.claim_pending_scratchpad(pid, std::time::Instant::now()));
        if claimed_scratchpad_name.is_none()
            && self
                .state
                .clients
                .get(client_key)
                .is_some_and(|client| client.pid.is_none())
            && !self.scratchpad_pending.is_empty()
        {
            debug!(
                "[scratchpad-pending] managed window {win:?} has no PID; pending names remain unconsumed"
            );
        }

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
            let is_scratchpad = claimed_scratchpad_name.is_some();

            let is_minimized = self
                .state
                .clients
                .get(client_key)
                .is_some_and(|client| client.state.is_hidden);
            if cfg.animation_enabled() && !is_scratchpad && !is_minimized {
                if let Some(client) = self.state.clients.get(client_key) {
                    let target = Rect::new(
                        client.geometry.x,
                        client.geometry.y,
                        client.geometry.w,
                        client.geometry.h,
                    );
                    let skip_wayland_dialog_probe = get_backend_family() == BackendFamily::Wayland
                        && target.w == 800
                        && target.h == 600
                        && backend
                            .property_ops()
                            .get_window_types(client.win)
                            .contains(&WindowType::Dialog);
                    if skip_wayland_dialog_probe {
                        info!(
                            "[manage] skip appear animation for provisional Wayland dialog {:?}",
                            client.win
                        );
                    } else {
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
        }

        // Detect named scratchpad window
        if let Some(sp_name) = claimed_scratchpad_name {
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
        let structurally_borderless = self.is_popup_like(backend, client_key)
            || self.state.clients.get(client_key).is_some_and(|client| {
                let types = backend.property_ops().get_window_types(client.win);
                types.contains(&WindowType::Dock) || types.contains(&WindowType::Desktop)
            });
        if structurally_borderless {
            if let Some(client) = self.state.clients.get_mut(client_key) {
                client.geometry.border_w = 0;
            }
            self.update_client_decoration(backend, client_key, false)?;

            self.configure_client(backend, client_key)?;
            if let Some(client) = self.state.clients.get(client_key) {
                self.setclientstate(
                    backend,
                    client.win,
                    i64::from(wm_state_for_minimized(client.state.is_hidden)),
                )?;
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
            client.geometry.border_w = if client.state.no_decorations {
                0
            } else {
                CONFIG.load().border_px() as i32
            };
        }

        self.update_client_decoration(backend, client_key, true)?;

        self.configure_client(backend, client_key)?;

        // When the compositor is NOT active, temporarily move the window
        // off-screen to avoid visual flicker before arrange() positions it.
        // With the compositor, rendering is done via TFP from the off-screen
        // pixmap, so the actual X11 position must stay correct for input
        // event delivery. An adopted Iconic client is already parked at the
        // safe left-hand coordinate and must never receive the ordinary
        // positive setup offset, which can cross back onto a real output.
        let keep_iconic_offscreen = self
            .state
            .clients
            .get(client_key)
            .is_some_and(|client| client.state.is_hidden);
        if !backend.has_compositor() && !keep_iconic_offscreen {
            let desktop_left = self.desktop_left_edge();
            let (x, y, w, h) = if let Some(client) = self.state.clients.get(client_key) {
                // Derive the staging slot from the complete desktop instead
                // of adding two root widths to an untrusted client x. The old
                // expression could overflow i32 and a stale `s_w` after an
                // output change did not prove the client was actually outside
                // every monitor. Reuse the same saturating left-hand parking
                // contract as hidden clients, without setting a semantic
                // hidden marker on this visible client.
                let offscreen_x =
                    hidden_x_left_of_desktop(desktop_left, client.total_width().max(1));
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
            self.setclientstate(
                backend,
                client.win,
                i64::from(wm_state_for_minimized(client.state.is_hidden)),
            )?;
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

    pub(crate) fn grabbuttons(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        focused: bool,
    ) {
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
        minimized_restore: Option<MinimizedRestoreState>,
        interrupted_restore: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let initially_minimized = self
            .state
            .clients
            .get(client_key)
            .is_some_and(|client| client.state.is_hidden);
        self.handle_transient_for(backend, client_key)?;

        if let Some(state) = minimized_restore {
            self.apply_minimized_restore_before_adjust(client_key, state);
            if interrupted_restore && let Some(client) = self.state.clients.get_mut(client_key) {
                client.state.minimized_order = 0;
            }
        }

        self.adjust_client_position(backend, client_key);
        if initially_minimized {
            self.stage_initially_minimized_geometry(client_key);
        }

        // Decoration hints are client-owned and may already be present before
        // the first MapRequest. Adopt them before setup configures the native
        // X11 border; doing it after MapWindow exposes one framed flash for
        // CSD/Motif-borderless clients when no compositor is available to
        // mask the transition.
        self.apply_motif_hints(backend, client_key);
        self.apply_gtk_frame_extents(backend, client_key);
        self.setup_client_window(backend, client_key)?;

        self.updatewindowtype(backend, client_key);
        self.updatesizehints(backend, client_key)?;
        self.float_if_fixed_size(client_key);
        if let Some(state) = minimized_restore {
            self.apply_minimized_restore_after_window_type(client_key, state);
        }
        if minimized_restore.is_some()
            && let Some(win) = self
                .state
                .clients
                .get(client_key)
                .and_then(|client| client.state.is_pip.then_some(client.win))
        {
            if let Err(error) =
                backend
                    .property_ops()
                    .set_net_wm_state_flag(win, NetWmState::Sticky, true)
            {
                warn!(
                    "[manage] could not normalize restored PiP Sticky state for {win:?}: {error}"
                );
            }
            // PiP is a semantic compositor style, not an X11 property the
            // freshly started compositor can rediscover. Replay it before the
            // minimized visual is adopted so both the Dock thumbnail and its
            // reverse transition retain the PiP presentation.
            backend.compositor_set_window_pip(win, true);
        }
        if initially_minimized
            && let Err(error) = self.persist_minimized_restore_state(backend, client_key)
        {
            // Legacy Iconic/EWMH-Hidden clients have no private snapshot, and
            // valid snapshots can need topology/rule normalization during
            // adoption. Their adoption must still succeed if this best-effort
            // self-heal cannot be written.
            let win = self.state.clients.get(client_key).map(|client| client.win);
            warn!("[manage] could not normalize minimized restore state for {win:?}: {error}");
        }
        if interrupted_restore {
            let win = self.state.clients.get(client_key).map(|client| client.win);
            if let Some(win) = win
                && let Err(error) = backend.property_ops().clear_minimized_restore_state(win)
            {
                // The window has already been configured at its semantic
                // NormalState placement. Leaving the marker behind is safe:
                // the next startup sees it on-screen and treats it as stale.
                warn!("[manage] could not clear interrupted-restore state for {win:?}: {error}");
            }
        }
        self.updatewmhints(backend, client_key);
        self.set_initial_frame_extents(backend, client_key);
        self.set_initial_allowed_actions(backend, client_key);
        self.read_sync_counter(backend, client_key);

        self.attach_new_client(client_key);
        self.attachstack(client_key);

        self.register_client_events(backend, client_key)?;
        self.grabbuttons(backend, client_key, false);

        let requires_x11_hidden_mapping_barrier =
            initially_minimized && backend.capabilities().supports_client_list;
        let already_mapped = match self.state.clients.get(client_key) {
            Some(client) => match backend.window_ops().get_window_attributes(client.win) {
                Ok(attributes) => attributes.map_state_viewable,
                Err(error) if requires_x11_hidden_mapping_barrier => return Err(error.into()),
                Err(_) => false,
            },
            None => false,
        };
        if !already_mapped {
            // A root-child Iconic window can carry an on-screen server
            // rectangle while physically unmapped. Configure it to the
            // current desktop-left parking slot and read the geometry back
            // before MapWindow: mapping first would expose one real frame if
            // the setup ConfigureWindow was lost or ignored.
            if requires_x11_hidden_mapping_barrier {
                self.retry_x11_minimized_client_park(backend, client_key)?;
            }
            self.map_client_window(backend, client_key)?;
        }
        if requires_x11_hidden_mapping_barrier {
            // These two synchronous replies deliberately follow MapWindow in
            // this order. Besides proving that the map completed, they keep
            // compositor capture behind a server-observed, still-parked
            // geometry. A failure leaves the client mapped at the pre-map
            // verified parking coordinate and admits no snapshot/iconify.
            self.verify_initially_minimized_x11_mapping(backend, client_key)?;
        }

        self.update_net_client_list(backend)?;

        let initial_minimized = self.state.clients.get(client_key).and_then(|client| {
            client.state.is_hidden.then_some((
                client.win,
                StatusBarBuilder::is_minimized_dock_eligible(client),
            ))
        });
        if let Some((win, dock_eligible)) = initial_minimized {
            // Reconcile compositor ownership before arrange moves the real
            // client back to JWM's hidden position. Eligible clients mirror
            // normal minimize ordering without letting a restarted iconic
            // client steal focus.
            if dock_eligible {
                backend.compositor_set_window_minimized(win, true);
            } else {
                // Rules and window-type adoption run before this point. A
                // hidden Dock/SKIP_TASKBAR/swallowed implementation surface
                // must not create a compositor-owned thumbnail that no bar
                // can ever address. An explicit withdrawal also removes a
                // stale target left by an earlier incarnation of this id.
                backend.compositor_set_window_dock_geometry(win, None);
            }
            let monitor = self
                .state
                .clients
                .get(client_key)
                .and_then(|client| client.mon);
            let monitor_num = monitor
                .and_then(|key| self.state.monitors.get(key))
                .map(|monitor| monitor.num);
            self.arrange(backend, monitor);
            self.mark_bar_update_needed_if_visible(monitor_num);
            if dock_eligible
                && let Err(error) = self.request_iconify_for_hidden_dock_client(backend, client_key)
            {
                // Mapping, semantic Hidden state, the restart snapshot and
                // compositor capture are already committed. Admission may be
                // retried by a repeated minimize while this safely parked
                // fallback remains managed.
                warn!("[manage] could not iconify adopted client {win:?}: {error}");
            }
        } else {
            self.handle_new_client_focus(backend, client_key)?;
        }

        self.suppress_mouse_focus_until =
            Some(std::time::Instant::now() + std::time::Duration::from_millis(300));

        self.try_swallow(backend, client_key);

        Ok(())
    }

    fn apply_minimized_restore_before_adjust(
        &mut self,
        client_key: ClientKey,
        state: MinimizedRestoreState,
    ) {
        let restored_monitor = self.get_monitor_by_id(state.monitor_num);
        let tagmask = CONFIG.load().tagmask();
        if let Some(client) = self.state.clients.get_mut(client_key) {
            if restored_monitor.is_some() {
                client.mon = restored_monitor;
            }
            client.state.tags = state.tags & tagmask;
            client.state.is_floating |= state.is_floating || state.is_pip;
            client.state.is_drag_floating = state.is_drag_floating;
            client.state.is_pip = state.is_pip;
            client.state.pip_restore_sticky = state.pip_restore_sticky;
            client.state.old_state = state.old_state;
            if state.is_pip {
                client.state.is_sticky = true;
            }
            client.state.minimized_order = state.minimized_order;
            client.geometry.x = state.visible_rect.x;
            client.geometry.y = state.visible_rect.y;
            client.geometry.w = state.visible_rect.w;
            client.geometry.h = state.visible_rect.h;
            if let Some(rect) = state.floating_rect {
                client.geometry.floating_x = rect.x;
                client.geometry.floating_y = rect.y;
                client.geometry.floating_w = rect.w;
                client.geometry.floating_h = rect.h;
            }
        }
    }

    fn apply_minimized_restore_after_window_type(
        &mut self,
        client_key: ClientKey,
        state: MinimizedRestoreState,
    ) {
        let was_floating = self
            .state
            .clients
            .get(client_key)
            .is_some_and(|client| client.state.is_floating);
        if let Some(client) = self.state.clients.get_mut(client_key) {
            // A standardized fullscreen property wins if an older process
            // left a PiP snapshot behind. `updatewindowtype` has already
            // entered fullscreen through the centralized mode transition, so
            // do not resurrect PiP and make both temporary modes own the same
            // restore slots again.
            let restore_pip = state.is_pip && !client.state.is_fullscreen;
            client.state.is_floating |= state.is_floating || restore_pip;
            client.state.is_drag_floating = state.is_drag_floating;
            client.state.is_pip = restore_pip;
            client.state.pip_restore_sticky = if restore_pip {
                state.pip_restore_sticky
            } else {
                false
            };
            if !client.state.is_fullscreen {
                client.state.old_state = state.old_state;
            }
            if restore_pip {
                client.state.is_sticky = true;
            }
            if let Some(rect) = state.floating_rect {
                client.geometry.floating_x = rect.x;
                client.geometry.floating_y = rect.y;
                client.geometry.floating_w = rect.w;
                client.geometry.floating_h = rect.h;
            }
            if client.state.is_fullscreen
                && let Some(rect) = state.fullscreen_restore_rect
            {
                client.geometry.old_x = rect.x;
                client.geometry.old_y = rect.y;
                client.geometry.old_w = rect.w;
                client.geometry.old_h = rect.h;
            }
        }
        let is_floating = self
            .state
            .clients
            .get(client_key)
            .is_some_and(|client| client.state.is_floating);
        if is_floating != was_floating {
            self.reorder_client_in_monitor_groups(client_key);
        }
    }

    /// Preserve a visible restore target while keeping an adopted Iconic
    /// client's real/input geometry outside the complete desktop throughout
    /// setup and mapping. `adjust_client_position` has already calculated the
    /// best current-monitor placement; this records that result in the
    /// dedicated hidden restore slot without configuring the client there
    /// before Dock reconstruction.
    fn stage_initially_minimized_geometry(&mut self, client_key: ClientKey) {
        let desktop_left = self.desktop_left_edge();
        let fallback_x = self
            .state
            .clients
            .get(client_key)
            .and_then(|client| client.mon)
            .and_then(|monitor| self.monitor_work_area(monitor))
            .map_or(desktop_left, |area| area.x);

        if let Some(client) = self.state.clients.get_mut(client_key) {
            let total_width = client.total_width().max(1);
            let restore_x = if client.geometry.x.saturating_add(total_width) <= desktop_left {
                fallback_x
            } else {
                client.geometry.x
            };
            let hidden_x = hidden_x_left_of_desktop(desktop_left, total_width);
            let restore = Rect::new(
                restore_x,
                client.geometry.y,
                client.geometry.w,
                client.geometry.h,
            );
            stage_hidden_geometry(&mut client.geometry, restore, hidden_x);
            if client.state.is_floating
                && (client.geometry.floating_w <= 0 || client.geometry.floating_h <= 0)
            {
                client.geometry.floating_x = restore_x;
                client.geometry.floating_y = client.geometry.y;
                client.geometry.floating_w = client.geometry.w;
                client.geometry.floating_h = client.geometry.h;
            }
        }
    }

    fn verify_initially_minimized_x11_mapping(
        &self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let win = self
            .state
            .clients
            .get(client_key)
            .map(|client| client.win)
            .ok_or("client disappeared while verifying initial Iconic mapping")?;
        if !backend
            .window_ops()
            .get_window_attributes(win)?
            .map_state_viewable
        {
            return Err(format!(
                "initially minimized window {win:?} was not viewable after MapWindow"
            )
            .into());
        }
        let geometry = backend.window_ops().get_geometry(win)?;
        if !x11_geometry_fully_left_of_desktop(geometry, self.desktop_left_edge()) {
            return Err(format!(
                "initially minimized window {win:?} left its parking region during MapWindow"
            )
            .into());
        }
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

    pub(crate) fn rule_matches(
        &self,
        rule: &WMRule,
        name: &str,
        class: &str,
        instance: &str,
    ) -> bool {
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
                    client.instance.clone_from(&instance);
                    client.class.clone_from(&class);
                }
            }
        }
        info!(
            "[applyrules_by_key] win: {:?}, name: '{}', instance: '{}', class: '{}'",
            win, name, instance, class
        );
        if let Some(client) = self.state.clients.get_mut(client_key) {
            client.state.is_floating = false;
            client.state.is_drag_floating = false;
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
        // A freshly spawned/reconnected bar must receive an authoritative
        // snapshot even if the platform never produces an Expose event. This
        // is what repopulates minimized-window metadata and lets the new bar
        // withdraw/re-report every compositor target after a crash.
        self.mark_bar_update_needed_if_visible(Some(monitor_id));
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

        // arrange() reconciles the bar on every pass, so a bar that is already
        // where this call would put it must cost nothing — no configure, no
        // strut churn, no forced compositor redraw.
        let already_placed = self
            .state
            .clients
            .get(client_key)
            .map(|c| {
                if show_bar {
                    let pad = cfg.status_bar_padding();
                    c.geometry.x == monitor.geometry.m_x + pad
                        && c.geometry.y == monitor.geometry.m_y + pad
                        && c.geometry.w == monitor.geometry.m_w - 2 * pad - 2 * c.geometry.border_w
                        && c.geometry.h == bar_height
                } else {
                    c.geometry.x == monitor.geometry.m_x
                        && c.geometry.y == monitor.geometry.m_y - actual_bar_height
                }
            })
            .unwrap_or(false);
        if already_placed {
            return Ok(());
        }

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

    /// Bring `mon_key`'s status-bar window in line with the current tag's
    /// show_bar flag. The flag is per-tag but the bar window is not, so every
    /// path that changes the effective flag — togglebar, entering or leaving
    /// the fullscreen layout, switching to a tag that remembers either — funnels
    /// through here; a bar already in place returns without touching the backend.
    pub(crate) fn sync_secondary_bar_position(
        &mut self,
        backend: &mut dyn Backend,
        mon_key: MonitorKey,
    ) {
        let Some(mon_num) = self.state.monitors.get(mon_key).map(|m| m.num) else {
            return;
        };
        let Some((client_key, win)) = self
            .secondary_bars
            .get(&mon_num)
            .and_then(|bar| bar.client_key.zip(bar.window))
        else {
            return;
        };
        let _ = self.position_secondary_bar_on_monitor(backend, client_key, win, mon_num);
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
        } else if backend.capabilities().supports_client_list
            && self
                .wintoclient(window)
                .and_then(|client_key| self.state.clients.get(client_key))
                .is_some_and(|client| client.state.is_hidden)
        {
            // On the X11 backends `WindowCreated` is the bridge's name for a
            // core MapRequest.  A request for an already-managed IconicState
            // client is therefore a deiconify request, not a second manage.
            // This is deliberately defensive rather than a complete ICCCM
            // deiconify transport: JWM keeps minimized clients mapped and
            // off-screen to retain their compositor texture, so an ordinary
            // XMapWindow on such a client is normally a server-side no-op and
            // produces no MapRequest for us to observe.
            // Native Wayland also emits `WindowCreated`, but it deliberately
            // does not advertise the EWMH `_NET_CLIENT_LIST` capability, so a
            // duplicate native lifecycle notification cannot reveal a Dock
            // item accidentally.
            debug!(
                "Restoring already-managed minimized window {:?} after X11 MapRequest",
                window
            );
            let _ = self.reveal_and_focus(backend, window)?;
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

    pub(crate) fn adjust_client_position(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) {
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
            let window_rect =
                Rect::new(client_x, client_y, client_total_width, client_total_height);
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
                    if let Some(intersection) =
                        GeometryConstraints::rect_intersection(&clamp, &parent_rect)
                    {
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
        // If this client is swallowing a parent, restore the parent first so
        // it gets remapped before we drop our reference to the swallow link.
        self.try_unswallow(backend, client_key);
        // If this client itself was swallowed, drop the dangling pointer from
        // its swallowing child (the child is still alive).
        let was_swallowed_by: Option<ClientKey> =
            self.state.client_order.iter().copied().find(|&k| {
                self.state.clients.get(k).and_then(|c| c.swallowing) == Some(client_key)
            });
        if let Some(parent_holder) = was_swallowed_by {
            if let Some(c) = self.state.clients.get_mut(parent_holder) {
                c.swallowing = None;
            }
        }
        let win = self.state.clients.get(client_key).map(|c| c.win);
        if let Some(client) = self.state.clients.get(client_key) {
            info!("[unmanage_regular_client] Removing client {}", client);
        }

        // X11 bridges send WindowUnmapped/WindowDestroyed through compositor
        // retirement before the WM handler; Wayland queues the corresponding
        // dead-surface retirement at its native lifecycle event. Those paths
        // discard the minimized texture/Genie animation. Do not call
        // `compositor_set_window_minimized(false)` here: on X11 that API
        // reimports the window and starts a restore. JWM still owns the Dock
        // target and preview lease, so withdraw those explicitly for both a
        // live withdrawal and a destroyed window.
        self.release_unmanaged_minimized_ownership(backend, client_key);

        // A normal UnmapNotify leaves a live client window that may map again.
        // Hand it back with visible geometry and neutral protocol state before
        // dropping monitor/list ownership. DestroyNotify must not issue any
        // request against the dead surface.
        if !destroyed {
            self.cleanup_window_state(backend, client_key)?;
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
        if let Some(win) = win {
            self.state.win_to_client.remove(&win);
        }
        self.clear_hidden_client_park_retry(client_key);
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

    fn release_unmanaged_minimized_ownership(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) {
        let Some((win, monitor_num)) = self.state.clients.get(client_key).map(|client| {
            let monitor_num = client
                .mon
                .and_then(|monitor| self.state.monitors.get(monitor))
                .map(|monitor| monitor.num);
            (client.win, monitor_num)
        }) else {
            return;
        };

        if let Some((preview_monitor, preview_window)) = self.active_minimized_preview
            && preview_window == win
        {
            self.clear_minimized_preview_for(backend, preview_monitor, Some(preview_window));
        }
        backend.compositor_set_window_dock_geometry(win, None);
        self.mark_bar_update_needed_if_visible(monitor_num);
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
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let desktop_left = self.desktop_left_edge();
        let monitor_left = self
            .state
            .clients
            .get(client_key)
            .and_then(|client| client.mon)
            .and_then(|monitor| self.state.monitors.get(monitor))
            .map(|monitor| monitor.geometry.w_x)
            .unwrap_or(desktop_left);
        let (win, old_border_w, restored_geometry) =
            if let Some(client) = self.state.clients.get_mut(client_key) {
                let restored_geometry =
                    restore_hidden_geometry(&mut client.geometry, desktop_left, monitor_left);
                client.state.is_hidden = false;
                client.state.minimized_order = 0;
                (client.win, client.geometry.old_border_w, restored_geometry)
            } else {
                return Err("Client not found".into());
            };
        if let Err(e) = backend
            .window_ops()
            .change_event_mask(win, EventMaskBits::NONE.bits())
        {
            warn!("[cleanup_window_state] Failed to clear event mask: {:?}", e);
        }
        let changes = WindowChanges {
            x: restored_geometry.map(|rect| rect.x),
            y: restored_geometry.map(|rect| rect.y),
            width: restored_geometry.map(|rect| rect.w.max(1) as u32),
            height: restored_geometry.map(|rect| rect.h.max(1) as u32),
            border_width: Some(old_border_w as u32),
            ..Default::default()
        };
        let geometry_handoff_succeeded =
            match backend.window_ops().apply_window_changes(win, changes) {
                Ok(()) => true,
                Err(e) => {
                    log::warn!(
                        "[cleanup_window_state] Failed to restore border/geometry: {:?}",
                        e
                    );
                    false
                }
            };
        if let Err(e) = backend.window_ops().ungrab_all_buttons(win) {
            warn!("[cleanup_window_state] Failed to ungrab buttons: {:?}", e);
        }
        if let Err(e) = backend
            .property_ops()
            .set_net_wm_state_flag(win, NetWmState::Hidden, false)
        {
            warn!(
                "[cleanup_window_state] Failed to clear hidden state: {:?}",
                e
            );
        }
        if geometry_handoff_succeeded {
            if let Err(e) = backend.property_ops().clear_minimized_restore_state(win) {
                warn!(
                    "[cleanup_window_state] Failed to clear minimized restore state: {:?}",
                    e
                );
            }
        } else {
            warn!(
                "[cleanup_window_state] Retaining minimized restore state for {:?} after failed geometry handoff",
                win
            );
        }
        if let Err(e) = self.setclientstate(backend, win, i64::from(WITHDRAWN_STATE)) {
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
                self.converge_configure_unmapped_client(backend, client_key)?;
            } else {
                debug!("Real unmap for window {:?}, unmanaging", window);
                self.unmanage(backend, Some(client_key), false)?;
            }
        } else {
            debug!("Unmap event for unmanaged window: 0{:?}", window);
        }
        Ok(())
    }

    /// Repair an UnmapGravity transition without replaying the semantic
    /// minimize/restore lifecycle. The client remains managed, so Withdrawn
    /// is never a valid resting state: a visible client is checked-mapped back
    /// to Normal, while a hidden client is mapped, re-parked and admitted to
    /// true Iconic ownership again.
    fn converge_configure_unmapped_client(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some((window, is_hidden, original_client_geometry)) = self
            .state
            .clients
            .get(client_key)
            .map(|client| (client.win, client.state.is_hidden, client.geometry.clone()))
        else {
            return Ok(());
        };
        let original_server_geometry =
            backend
                .window_ops()
                .get_geometry(window)
                .unwrap_or(Geometry {
                    x: original_client_geometry.x,
                    y: original_client_geometry.y,
                    w: original_client_geometry.w.max(1) as u32,
                    h: original_client_geometry.h.max(1) as u32,
                    border: original_client_geometry.border_w.max(0) as u32,
                });
        let original_hidden = backend
            .property_ops()
            .has_net_wm_state_flag(window, NetWmState::Hidden)
            .unwrap_or(is_hidden);
        let original_wm_state = backend
            .property_ops()
            .get_wm_state(window)
            .unwrap_or_else(|_| i64::from(wm_state_for_minimized(is_hidden)));

        let converge = (|| -> Result<(), Box<dyn std::error::Error>> {
            // Cancel first so a sent/Iconic generation retains its pin until
            // the checked map is complete. Awaiting owns no physical unmap,
            // but UnmapGravity proves that its mapped invariant also needs an
            // explicit repair.
            backend.compositor_cancel_window_iconify(window)?;
            backend.window_ops().map_window(window)?;
            let attributes = backend.window_ops().get_window_attributes(window)?;
            if !attributes.map_state_viewable {
                return Err(format!(
                    "configure-unmapped client {window:?} was not viewable after checked MapWindow"
                )
                .into());
            }

            if is_hidden {
                self.retry_x11_minimized_client_park(backend, client_key)?;
            }

            // Publish only after the physical state is safe. This is a repair
            // of the existing lifecycle, not a new transition, so it must not
            // call compositor_set_window_minimized, arrange, focus, or Genie.
            self.setclientstate(
                backend,
                window,
                i64::from(wm_state_for_minimized(is_hidden)),
            )?;
            backend
                .property_ops()
                .set_net_wm_state_flag(window, NetWmState::Hidden, is_hidden)?;

            if is_hidden {
                self.request_iconify_for_hidden_dock_client(backend, client_key)?;
            }
            Ok(())
        })();

        if let Err(error) = converge {
            self.rollback_configure_unmapped_client(
                backend,
                client_key,
                &original_client_geometry,
                original_server_geometry,
                original_wm_state,
                original_hidden,
                is_hidden,
            );
            return Err(error);
        }

        Ok(())
    }

    /// Best-effort compensation for every fallible edge in
    /// [`Self::converge_configure_unmapped_client`]. Internal semantic state
    /// never changed; restore the original server/public projection and, for
    /// an already-hidden client, re-arm the existing Iconic incarnation.
    fn rollback_configure_unmapped_client(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        original_client_geometry: &ClientGeometry,
        original_server_geometry: Geometry,
        original_wm_state: i64,
        original_hidden: bool,
        rearm_iconic: bool,
    ) {
        let Some(window) = self.state.clients.get(client_key).map(|client| client.win) else {
            return;
        };
        if let Some(client) = self.state.clients.get_mut(client_key) {
            client.geometry = original_client_geometry.clone();
        }
        if let Err(error) = backend.window_ops().configure(
            window,
            original_server_geometry.x,
            original_server_geometry.y,
            original_server_geometry.w.max(1),
            original_server_geometry.h.max(1),
            original_server_geometry.border,
        ) {
            warn!(
                "could not restore geometry after configure-unmap recovery failed for {window:?}: {error}"
            );
        }
        if !rearm_iconic {
            let remap = backend.window_ops().map_window(window).and_then(|()| {
                let attributes = backend.window_ops().get_window_attributes(window)?;
                if attributes.map_state_viewable {
                    Ok(())
                } else {
                    Err(crate::backend::error::BackendError::Message(format!(
                        "configure-unmapped client {window:?} remained unmapped during rollback"
                    )))
                }
            });
            if let Err(error) = remap {
                warn!(
                    "could not restore mapped state after configure-unmap recovery failed for {window:?}: {error}"
                );
            }
        }
        if let Err(error) = self.setclientstate(backend, window, original_wm_state) {
            warn!(
                "could not restore WM_STATE after configure-unmap recovery failed for {window:?}: {error}"
            );
        }
        if let Err(error) = backend.property_ops().set_net_wm_state_flag(
            window,
            NetWmState::Hidden,
            original_hidden,
        ) {
            warn!(
                "could not restore EWMH Hidden after configure-unmap recovery failed for {window:?}: {error}"
            );
        }
        if rearm_iconic
            && let Err(error) = self.request_iconify_for_hidden_dock_client(backend, client_key)
        {
            warn!(
                "could not re-arm Iconic ownership after configure-unmap recovery failed for {window:?}: {error}"
            );
        }
    }
}

#[cfg(test)]
mod unmanage_minimized_tests {
    use super::*;
    use crate::backend::api::{
        BackendDiagnostics, Capabilities, CloseResult, ColorAllocator, CompositorAnnotation,
        CompositorBenchmark, CompositorControl, CompositorMedia, CompositorRect,
        CompositorWindowEffects, CompositorWorkspaceEffects, CursorProvider, DisplayControl,
        InputOps, KeyOps, MinimizedRestoreRect, MinimizedRestoreState, MotifWmHints, NormalHints,
        OutputOps, PropertyOps, RenderScheduler, WindowAttributes, WindowOps, WmHints,
    };
    use crate::backend::common_define::Pixel;
    use crate::backend::error::BackendError;
    use crate::backend::wayland_dummy_ops::{
        DummyColorAllocator, DummyCursorProvider, DummyInputOps, DummyKeyOps, DummyOutputOps,
    };
    use std::any::Any;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicUsize, Ordering};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ProtocolWrite {
        Hidden(bool),
        WmState(i64),
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum InitialIconicOperation {
        Configure { x: i32, width: u32 },
        Attributes { viewable: bool },
        AttributesFailed,
        Geometry { x: i32, width: u32 },
        GeometryFailed,
        Map,
        Capture,
        Iconify,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum RestoreAccess {
        Get(WindowId),
        Set(WindowId, MinimizedRestoreState),
        Clear(WindowId),
    }

    #[derive(Default)]
    struct ClientPropertyOps {
        hidden: AtomicBool,
        wm_state: AtomicI64,
        dock_type: AtomicBool,
        fullscreen: AtomicBool,
        motif_borderless: AtomicBool,
        gtk_client_frame: AtomicBool,
        fail_restore_get: AtomicBool,
        fail_restore_set: AtomicBool,
        fail_next_wm_state_read: AtomicBool,
        fail_next_hidden_read: AtomicBool,
        fail_next_hidden_write: AtomicBool,
        fail_next_wm_state_write: AtomicBool,
        writes: Mutex<Vec<ProtocolWrite>>,
        minimized_restore: Mutex<Option<MinimizedRestoreState>>,
        restore_accesses: Mutex<Vec<RestoreAccess>>,
        window_pid: AtomicU32,
    }

    impl PropertyOps for ClientPropertyOps {
        fn get_title(&self, _win: WindowId) -> String {
            String::new()
        }

        fn get_class(&self, _win: WindowId) -> (String, String) {
            (String::new(), String::new())
        }

        fn get_window_types(&self, _win: WindowId) -> Vec<WindowType> {
            if self.dock_type.load(Ordering::Relaxed) {
                vec![WindowType::Dock]
            } else {
                vec![WindowType::Normal]
            }
        }

        fn get_motif_hints(&self, _win: WindowId) -> Option<MotifWmHints> {
            self.motif_borderless
                .load(Ordering::Relaxed)
                .then_some(MotifWmHints {
                    flags: 0x2,
                    decorations: 0,
                    ..Default::default()
                })
        }

        fn get_gtk_frame_extents(&self, _win: WindowId) -> Option<[u32; 4]> {
            self.gtk_client_frame
                .load(Ordering::Relaxed)
                .then_some([8, 8, 28, 8])
        }

        fn is_fullscreen(&self, _win: WindowId) -> bool {
            self.fullscreen.load(Ordering::Relaxed)
        }

        fn set_fullscreen_state(&self, _win: WindowId, on: bool) -> Result<(), BackendError> {
            self.fullscreen.store(on, Ordering::Relaxed);
            Ok(())
        }

        fn transient_for(&self, _win: WindowId) -> Option<WindowId> {
            None
        }

        fn get_wm_hints(&self, _win: WindowId) -> Option<WmHints> {
            None
        }

        fn set_urgent_hint(&self, _win: WindowId, _urgent: bool) -> Result<(), BackendError> {
            Ok(())
        }

        fn fetch_normal_hints(&self, _win: WindowId) -> Result<Option<NormalHints>, BackendError> {
            Ok(None)
        }

        fn set_window_strut_top(
            &self,
            _win: WindowId,
            _top: u32,
            _start_x: u32,
            _end_x: u32,
        ) -> Result<(), BackendError> {
            Ok(())
        }

        fn set_window_type_dock(&self, _win: WindowId) -> Result<(), BackendError> {
            Ok(())
        }

        fn clear_window_strut(&self, _win: WindowId) -> Result<(), BackendError> {
            Ok(())
        }

        fn get_wm_state(&self, _win: WindowId) -> Result<i64, BackendError> {
            if self.fail_next_wm_state_read.swap(false, Ordering::Relaxed) {
                return Err(BackendError::Message(
                    "injected WM_STATE read failure".into(),
                ));
            }
            Ok(self.wm_state.load(Ordering::Relaxed))
        }

        fn set_wm_state(&self, _win: WindowId, state: i64) -> Result<(), BackendError> {
            if self.fail_next_wm_state_write.swap(false, Ordering::Relaxed) {
                return Err(BackendError::Message(
                    "injected WM_STATE write failure".into(),
                ));
            }
            self.wm_state.store(state, Ordering::Relaxed);
            self.writes
                .lock()
                .expect("protocol writes lock")
                .push(ProtocolWrite::WmState(state));
            Ok(())
        }

        fn get_minimized_restore_state(
            &self,
            win: WindowId,
        ) -> Result<Option<MinimizedRestoreState>, BackendError> {
            self.restore_accesses
                .lock()
                .expect("restore accesses lock")
                .push(RestoreAccess::Get(win));
            if self.fail_restore_get.load(Ordering::Relaxed) {
                return Err(BackendError::Message(
                    "injected minimized restore read failure".into(),
                ));
            }
            Ok(*self
                .minimized_restore
                .lock()
                .expect("minimized restore lock"))
        }

        fn set_minimized_restore_state(
            &self,
            win: WindowId,
            state: MinimizedRestoreState,
        ) -> Result<(), BackendError> {
            self.restore_accesses
                .lock()
                .expect("restore accesses lock")
                .push(RestoreAccess::Set(win, state));
            if self.fail_restore_set.load(Ordering::Relaxed) {
                return Err(BackendError::Message(
                    "injected minimized restore write failure".into(),
                ));
            }
            *self
                .minimized_restore
                .lock()
                .expect("minimized restore lock") = Some(state);
            Ok(())
        }

        fn clear_minimized_restore_state(&self, win: WindowId) -> Result<(), BackendError> {
            *self
                .minimized_restore
                .lock()
                .expect("minimized restore lock") = None;
            self.restore_accesses
                .lock()
                .expect("restore accesses lock")
                .push(RestoreAccess::Clear(win));
            Ok(())
        }

        fn set_client_info_props(
            &self,
            _win: WindowId,
            _tags: u32,
            _monitor_num: u32,
        ) -> Result<(), BackendError> {
            Ok(())
        }

        fn set_net_wm_state_flag(
            &self,
            _win: WindowId,
            state: NetWmState,
            on: bool,
        ) -> Result<(), BackendError> {
            if state == NetWmState::Hidden {
                if self.fail_next_hidden_write.swap(false, Ordering::Relaxed) {
                    return Err(BackendError::Message(
                        "injected EWMH Hidden write failure".into(),
                    ));
                }
                self.hidden.store(on, Ordering::Relaxed);
                self.writes
                    .lock()
                    .expect("protocol writes lock")
                    .push(ProtocolWrite::Hidden(on));
            }
            Ok(())
        }

        fn has_net_wm_state_flag(
            &self,
            _win: WindowId,
            state: NetWmState,
        ) -> Result<bool, BackendError> {
            if state == NetWmState::Hidden
                && self.fail_next_hidden_read.swap(false, Ordering::Relaxed)
            {
                return Err(BackendError::Message(
                    "injected EWMH Hidden read failure".into(),
                ));
            }
            Ok(state == NetWmState::Hidden && self.hidden.load(Ordering::Relaxed))
        }

        fn get_window_pid(&self, _win: WindowId) -> Option<u32> {
            let pid = self.window_pid.load(Ordering::Relaxed);
            (pid != 0).then_some(pid)
        }
    }

    #[derive(Default)]
    struct ClientWindowOps {
        configures: Mutex<Vec<(WindowId, i32, i32, u32, u32)>>,
        positions: Mutex<Vec<(WindowId, i32, i32)>>,
        maps: Mutex<Vec<WindowId>>,
        focuses: Mutex<Vec<Option<WindowId>>>,
        changes: Mutex<Vec<(WindowId, WindowChanges)>>,
        decorations: Mutex<Vec<(WindowId, u32)>>,
        event_masks: Mutex<Vec<WindowId>>,
        ungrabs: Mutex<Vec<WindowId>>,
        reported_geometry: Mutex<Option<Geometry>>,
        ignore_positions: AtomicBool,
        force_unmapped: AtomicBool,
        fail_next_map: AtomicBool,
        fail_next_attributes: AtomicBool,
        fail_attributes_on_call: AtomicUsize,
        attributes_calls: AtomicUsize,
        fail_geometry_on_call: AtomicUsize,
        geometry_calls: AtomicUsize,
        fail_next_configure: AtomicBool,
        initial_iconic_operations: Mutex<Vec<InitialIconicOperation>>,
    }

    impl WindowOps for ClientWindowOps {
        fn set_position(&self, win: WindowId, x: i32, y: i32) -> Result<(), BackendError> {
            self.positions
                .lock()
                .expect("window positions lock")
                .push((win, x, y));
            if !self.ignore_positions.load(Ordering::Relaxed)
                && let Some(geometry) = self
                    .reported_geometry
                    .lock()
                    .expect("reported geometry lock")
                    .as_mut()
            {
                geometry.x = x;
                geometry.y = y;
            }
            Ok(())
        }

        fn configure(
            &self,
            win: WindowId,
            x: i32,
            y: i32,
            w: u32,
            h: u32,
            border: u32,
        ) -> Result<(), BackendError> {
            self.initial_iconic_operations
                .lock()
                .expect("initial Iconic operations lock")
                .push(InitialIconicOperation::Configure { x, width: w });
            self.configures
                .lock()
                .expect("window configures lock")
                .push((win, x, y, w, h));
            if self.fail_next_configure.swap(false, Ordering::Relaxed) {
                return Err(BackendError::Message(
                    "injected ConfigureWindow failure".into(),
                ));
            }
            let mut reported = self
                .reported_geometry
                .lock()
                .expect("reported geometry lock");
            let geometry = reported.get_or_insert(Geometry {
                x: 120,
                y: 80,
                w: 640,
                h: 480,
                border: 0,
            });
            // Model an X server/client that accepts the ConfigureWindow
            // request but leaves its position unchanged.  Parking and restore
            // now use the full configure path rather than `set_position`, so
            // the failure injection has to cover both entry points.
            if !self.ignore_positions.load(Ordering::Relaxed) {
                geometry.x = x;
                geometry.y = y;
            }
            geometry.w = w;
            geometry.h = h;
            geometry.border = border;
            Ok(())
        }

        fn set_decoration_style(
            &self,
            win: WindowId,
            border_width: u32,
            _border_color: Pixel,
        ) -> Result<(), BackendError> {
            self.decorations
                .lock()
                .expect("window decorations lock")
                .push((win, border_width));
            Ok(())
        }

        fn raise_window(&self, _win: WindowId) -> Result<(), BackendError> {
            Ok(())
        }

        fn map_window(&self, win: WindowId) -> Result<(), BackendError> {
            self.initial_iconic_operations
                .lock()
                .expect("initial Iconic operations lock")
                .push(InitialIconicOperation::Map);
            self.maps.lock().expect("window maps lock").push(win);
            if self.fail_next_map.swap(false, Ordering::Relaxed) {
                return Err(BackendError::Message("injected MapWindow failure".into()));
            }
            self.force_unmapped.store(false, Ordering::Relaxed);
            Ok(())
        }

        fn unmap_window(&self, _win: WindowId) -> Result<(), BackendError> {
            self.force_unmapped.store(true, Ordering::Relaxed);
            Ok(())
        }

        fn close_window(&self, _win: WindowId) -> Result<CloseResult, BackendError> {
            Ok(CloseResult::Graceful)
        }

        fn set_input_focus(&self, win: WindowId) -> Result<(), BackendError> {
            self.focuses
                .lock()
                .expect("window focuses lock")
                .push(Some(win));
            Ok(())
        }

        fn set_input_focus_root(&self) -> Result<(), BackendError> {
            self.focuses.lock().expect("window focuses lock").push(None);
            Ok(())
        }

        fn get_window_attributes(&self, _win: WindowId) -> Result<WindowAttributes, BackendError> {
            let call = self.attributes_calls.fetch_add(1, Ordering::Relaxed) + 1;
            if self.fail_next_attributes.swap(false, Ordering::Relaxed)
                || self.fail_attributes_on_call.load(Ordering::Relaxed) == call
            {
                self.initial_iconic_operations
                    .lock()
                    .expect("initial Iconic operations lock")
                    .push(InitialIconicOperation::AttributesFailed);
                return Err(BackendError::Message(
                    "injected GetWindowAttributes failure".into(),
                ));
            }
            let viewable = !self.force_unmapped.load(Ordering::Relaxed);
            self.initial_iconic_operations
                .lock()
                .expect("initial Iconic operations lock")
                .push(InitialIconicOperation::Attributes { viewable });
            Ok(WindowAttributes {
                override_redirect: false,
                map_state_viewable: viewable,
            })
        }

        fn get_geometry(&self, _win: WindowId) -> Result<Geometry, BackendError> {
            let call = self.geometry_calls.fetch_add(1, Ordering::Relaxed) + 1;
            if self.fail_geometry_on_call.load(Ordering::Relaxed) == call {
                self.initial_iconic_operations
                    .lock()
                    .expect("initial Iconic operations lock")
                    .push(InitialIconicOperation::GeometryFailed);
                return Err(BackendError::Message("injected GetGeometry failure".into()));
            }
            let geometry = self
                .reported_geometry
                .lock()
                .expect("reported geometry lock")
                .unwrap_or(Geometry {
                    x: 120,
                    y: 80,
                    w: 640,
                    h: 480,
                    border: 0,
                });
            self.initial_iconic_operations
                .lock()
                .expect("initial Iconic operations lock")
                .push(InitialIconicOperation::Geometry {
                    x: geometry.x,
                    width: geometry.w,
                });
            Ok(geometry)
        }

        fn scan_windows(&self) -> Result<Vec<WindowId>, BackendError> {
            Ok(Vec::new())
        }

        fn flush(&self) -> Result<(), BackendError> {
            Ok(())
        }

        fn kill_client(&self, _win: WindowId) -> Result<(), BackendError> {
            Ok(())
        }

        fn apply_window_changes(
            &self,
            win: WindowId,
            changes: WindowChanges,
        ) -> Result<(), BackendError> {
            self.changes
                .lock()
                .expect("window changes lock")
                .push((win, changes));
            Ok(())
        }

        fn change_event_mask(&self, win: WindowId, _mask: u32) -> Result<(), BackendError> {
            self.event_masks.lock().expect("event masks lock").push(win);
            Ok(())
        }

        fn ungrab_all_buttons(&self, win: WindowId) -> Result<(), BackendError> {
            self.ungrabs.lock().expect("ungrabs lock").push(win);
            Ok(())
        }
    }

    struct ClientSpyBackend {
        window_ops: ClientWindowOps,
        input_ops: DummyInputOps,
        property_ops: ClientPropertyOps,
        output_ops: DummyOutputOps,
        key_ops: DummyKeyOps,
        cursor_provider: DummyCursorProvider,
        color_allocator: DummyColorAllocator,
        minimized: Vec<(WindowId, bool)>,
        dock_targets: Vec<(WindowId, Option<CompositorRect>)>,
        previews: Vec<(Option<WindowId>, Option<CompositorRect>)>,
        pips: Vec<(WindowId, bool)>,
        iconify_requests: Vec<WindowId>,
        iconify_observations: Vec<(WindowId, bool, bool, Geometry)>,
        fail_next_iconify: AtomicBool,
        cancel_requests: Vec<WindowId>,
        fail_next_cancel: AtomicBool,
        has_compositor: bool,
        supports_client_list: bool,
    }

    impl ClientSpyBackend {
        fn new() -> Self {
            Self {
                window_ops: ClientWindowOps::default(),
                input_ops: DummyInputOps,
                property_ops: ClientPropertyOps::default(),
                output_ops: DummyOutputOps,
                key_ops: DummyKeyOps,
                cursor_provider: DummyCursorProvider,
                color_allocator: DummyColorAllocator,
                minimized: Vec::new(),
                dock_targets: Vec::new(),
                previews: Vec::new(),
                pips: Vec::new(),
                iconify_requests: Vec::new(),
                iconify_observations: Vec::new(),
                fail_next_iconify: AtomicBool::new(false),
                cancel_requests: Vec::new(),
                fail_next_cancel: AtomicBool::new(false),
                has_compositor: true,
                supports_client_list: false,
            }
        }
    }

    impl CompositorBenchmark for ClientSpyBackend {}
    impl BackendDiagnostics for ClientSpyBackend {}
    impl CompositorControl for ClientSpyBackend {}
    impl CompositorMedia for ClientSpyBackend {}
    impl CompositorWorkspaceEffects for ClientSpyBackend {}
    impl CompositorWindowEffects for ClientSpyBackend {
        fn compositor_cancel_window_iconify(
            &mut self,
            window: WindowId,
        ) -> Result<(), BackendError> {
            self.cancel_requests.push(window);
            if self.fail_next_cancel.swap(false, Ordering::Relaxed) {
                return Err(BackendError::Message(
                    "injected Iconic cancellation failure".into(),
                ));
            }
            self.window_ops
                .force_unmapped
                .store(false, Ordering::Relaxed);
            Ok(())
        }

        fn compositor_request_window_iconify(
            &mut self,
            window: WindowId,
        ) -> Result<(), BackendError> {
            self.window_ops
                .initial_iconic_operations
                .lock()
                .expect("initial Iconic operations lock")
                .push(InitialIconicOperation::Iconify);
            self.iconify_requests.push(window);
            let mapped = self
                .window_ops
                .get_window_attributes(window)?
                .map_state_viewable;
            let captured = self.minimized.contains(&(window, true));
            let geometry = self.window_ops.get_geometry(window)?;
            self.iconify_observations
                .push((window, mapped, captured, geometry));
            if self.fail_next_iconify.swap(false, Ordering::Relaxed) {
                return Err(BackendError::Message(
                    "injected Iconic admission failure".into(),
                ));
            }
            self.window_ops
                .force_unmapped
                .store(true, Ordering::Relaxed);
            Ok(())
        }

        fn compositor_set_window_minimized(&mut self, window: WindowId, minimized: bool) {
            if minimized {
                self.window_ops
                    .initial_iconic_operations
                    .lock()
                    .expect("initial Iconic operations lock")
                    .push(InitialIconicOperation::Capture);
            }
            self.minimized.push((window, minimized));
        }

        fn compositor_set_window_dock_geometry(
            &mut self,
            window: WindowId,
            target: Option<CompositorRect>,
        ) {
            self.dock_targets.push((window, target));
        }

        fn compositor_set_minimized_window_preview(
            &mut self,
            window: Option<WindowId>,
            anchor: Option<CompositorRect>,
        ) {
            self.previews.push((window, anchor));
        }

        fn compositor_set_window_pip(&mut self, window: WindowId, pip: bool) {
            self.pips.push((window, pip));
        }
    }
    impl CompositorAnnotation for ClientSpyBackend {}
    impl DisplayControl for ClientSpyBackend {}
    impl RenderScheduler for ClientSpyBackend {
        fn has_compositor(&self) -> bool {
            self.has_compositor
        }
    }

    impl Backend for ClientSpyBackend {
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                supports_client_list: self.supports_client_list,
                ..Capabilities::default()
            }
        }

        fn root_window(&self) -> Option<WindowId> {
            Some(WindowId::from_raw(0))
        }

        fn as_any(&self) -> &dyn Any {
            self
        }

        fn check_existing_wm(&self) -> Result<(), BackendError> {
            Ok(())
        }

        fn window_ops(&self) -> &dyn WindowOps {
            &self.window_ops
        }

        fn input_ops(&self) -> &dyn InputOps {
            &self.input_ops
        }

        fn property_ops(&self) -> &dyn PropertyOps {
            &self.property_ops
        }

        fn output_ops(&self) -> &dyn OutputOps {
            &self.output_ops
        }

        fn key_ops(&self) -> &dyn KeyOps {
            &self.key_ops
        }

        fn key_ops_mut(&mut self) -> &mut dyn KeyOps {
            &mut self.key_ops
        }

        fn cursor_provider(&mut self) -> &mut dyn CursorProvider {
            &mut self.cursor_provider
        }

        fn color_allocator(&mut self) -> &mut dyn ColorAllocator {
            &mut self.color_allocator
        }

        fn run(
            &mut self,
            _handler: &mut dyn crate::backend::api::EventHandler,
        ) -> Result<(), BackendError> {
            Ok(())
        }
    }

    fn add_minimized_client(jwm: &mut Jwm, window: WindowId) -> (ClientKey, MonitorKey, i32) {
        let monitor = jwm.state.monitor_order[0];
        let monitor_num = jwm.state.monitors[monitor].num;
        let mut client = WMClient::new(window);
        client.mon = Some(monitor);
        client.state.tags = 1;
        client.state.is_hidden = true;
        client.state.minimized_order = 17;
        client.geometry.x = -1600;
        client.geometry.old_x = 777;
        client.geometry.y = 80;
        client.geometry.old_y = 333;
        client.geometry.w = 640;
        client.geometry.h = 480;
        client.geometry.old_border_w = 3;
        client.geometry.hidden_x = Some(-1600);
        client.geometry.hidden_restore_rect = Some(Rect::new(420, 80, 640, 480));
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, monitor);
        (client_key, monitor, monitor_num)
    }

    fn add_configure_unmap_client(
        jwm: &mut Jwm,
        backend: &mut ClientSpyBackend,
        window: WindowId,
        hidden: bool,
    ) -> ClientKey {
        let (client_key, _, _) = add_minimized_client(jwm, window);
        if !hidden {
            let client = &mut jwm.state.clients[client_key];
            client.state.is_hidden = false;
            client.state.minimized_order = 0;
            client.geometry.x = 420;
            client.geometry.y = 80;
            client.geometry.hidden_x = None;
            client.geometry.hidden_restore_rect = None;
        }
        let client = &jwm.state.clients[client_key];
        *backend
            .window_ops
            .reported_geometry
            .lock()
            .expect("reported geometry lock") = Some(Geometry {
            x: client.geometry.x,
            y: client.geometry.y,
            w: client.geometry.w.max(1) as u32,
            h: client.geometry.h.max(1) as u32,
            border: client.geometry.border_w.max(0) as u32,
        });
        backend
            .window_ops
            .force_unmapped
            .store(true, Ordering::Relaxed);
        client_key
    }

    #[test]
    fn configure_unmap_remaps_visible_client_and_repairs_normal_public_state() {
        let mut backend = ClientSpyBackend::new();
        backend.supports_client_list = true;
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let window = WindowId::from_raw(0x8501);
        let client_key = add_configure_unmap_client(&mut jwm, &mut backend, window, false);
        backend.property_ops.hidden.store(true, Ordering::Relaxed);
        backend
            .property_ops
            .wm_state
            .store(i64::from(wm_state_for_minimized(true)), Ordering::Relaxed);
        let original_client = jwm.state.clients[client_key].clone();

        jwm.unmapnotify(&mut backend, window, true).unwrap();

        assert_eq!(jwm.state.clients[client_key], original_client);
        assert!(
            backend
                .window_ops
                .get_window_attributes(window)
                .unwrap()
                .map_state_viewable
        );
        assert_eq!(
            backend.property_ops.wm_state.load(Ordering::Relaxed),
            i64::from(wm_state_for_minimized(false))
        );
        assert!(!backend.property_ops.hidden.load(Ordering::Relaxed));
        assert_eq!(backend.cancel_requests, vec![window]);
        assert_eq!(
            *backend.window_ops.maps.lock().expect("window maps lock"),
            vec![window]
        );
        assert!(backend.iconify_requests.is_empty());
        assert!(backend.minimized.is_empty(), "must not replay Genie state");
        assert!(
            backend
                .window_ops
                .focuses
                .lock()
                .expect("window focuses lock")
                .is_empty(),
            "physical recovery must not change focus"
        );
    }

    #[test]
    fn configure_unmap_reparks_hidden_client_then_rearms_true_iconic() {
        let mut backend = ClientSpyBackend::new();
        backend.supports_client_list = true;
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let window = WindowId::from_raw(0x8502);
        let client_key = add_configure_unmap_client(&mut jwm, &mut backend, window, true);
        backend.property_ops.hidden.store(false, Ordering::Relaxed);
        backend
            .property_ops
            .wm_state
            .store(i64::from(wm_state_for_minimized(false)), Ordering::Relaxed);
        backend.minimized.push((window, true));
        let semantic_before = jwm.state.clients[client_key].state.clone();

        jwm.unmapnotify(&mut backend, window, true).unwrap();

        assert_eq!(jwm.state.clients[client_key].state, semantic_before);
        assert_eq!(
            backend.property_ops.wm_state.load(Ordering::Relaxed),
            i64::from(wm_state_for_minimized(true))
        );
        assert!(backend.property_ops.hidden.load(Ordering::Relaxed));
        assert_eq!(backend.cancel_requests, vec![window]);
        assert_eq!(backend.iconify_requests, vec![window]);
        let (_, mapped_at_admission, captured, geometry) = backend.iconify_observations[0];
        assert!(mapped_at_admission);
        assert!(captured);
        assert!(x11_geometry_fully_left_of_desktop(
            geometry,
            jwm.desktop_left_edge()
        ));
        assert!(
            !backend
                .window_ops
                .get_window_attributes(window)
                .unwrap()
                .map_state_viewable
        );
        assert_eq!(backend.minimized, vec![(window, true)]);
        assert!(
            backend
                .window_ops
                .focuses
                .lock()
                .expect("window focuses lock")
                .is_empty(),
            "Iconic re-arm must not change focus"
        );
    }

    #[derive(Clone, Copy, Debug)]
    enum ConfigureUnmapFault {
        Cancel,
        Map,
        Attributes,
        Park,
        WmStateProperty,
        HiddenProperty,
        Request,
    }

    fn inject_configure_unmap_fault(backend: &mut ClientSpyBackend, fault: ConfigureUnmapFault) {
        match fault {
            ConfigureUnmapFault::Cancel => backend.fail_next_cancel.store(true, Ordering::Relaxed),
            ConfigureUnmapFault::Map => backend
                .window_ops
                .fail_next_map
                .store(true, Ordering::Relaxed),
            ConfigureUnmapFault::Attributes => backend
                .window_ops
                .fail_next_attributes
                .store(true, Ordering::Relaxed),
            ConfigureUnmapFault::Park => backend
                .window_ops
                .fail_next_configure
                .store(true, Ordering::Relaxed),
            ConfigureUnmapFault::WmStateProperty => backend
                .property_ops
                .fail_next_wm_state_write
                .store(true, Ordering::Relaxed),
            ConfigureUnmapFault::HiddenProperty => backend
                .property_ops
                .fail_next_hidden_write
                .store(true, Ordering::Relaxed),
            ConfigureUnmapFault::Request => {
                backend.fail_next_iconify.store(true, Ordering::Relaxed)
            }
        }
    }

    #[test]
    fn configure_unmap_failure_matrix_restores_public_geometry_and_semantics() {
        let cases: &[(bool, &[ConfigureUnmapFault])] = &[
            (
                false,
                &[
                    ConfigureUnmapFault::Cancel,
                    ConfigureUnmapFault::Map,
                    ConfigureUnmapFault::Attributes,
                    ConfigureUnmapFault::WmStateProperty,
                    ConfigureUnmapFault::HiddenProperty,
                ],
            ),
            (
                true,
                &[
                    ConfigureUnmapFault::Cancel,
                    ConfigureUnmapFault::Map,
                    ConfigureUnmapFault::Attributes,
                    ConfigureUnmapFault::Park,
                    ConfigureUnmapFault::WmStateProperty,
                    ConfigureUnmapFault::HiddenProperty,
                    ConfigureUnmapFault::Request,
                ],
            ),
        ];

        for &(hidden, faults) in cases {
            for &fault in faults {
                let mut backend = ClientSpyBackend::new();
                backend.supports_client_list = true;
                let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
                let window = WindowId::from_raw(if hidden { 0x8510 } else { 0x8520 });
                let client_key = add_configure_unmap_client(&mut jwm, &mut backend, window, hidden);
                backend.property_ops.hidden.store(hidden, Ordering::Relaxed);
                backend
                    .property_ops
                    .wm_state
                    .store(i64::from(wm_state_for_minimized(hidden)), Ordering::Relaxed);
                if hidden {
                    backend.minimized.push((window, true));
                }
                let original_client = jwm.state.clients[client_key].clone();
                let original_server_geometry = backend.window_ops.get_geometry(window).unwrap();
                let original_minimized = backend.minimized.clone();
                inject_configure_unmap_fault(&mut backend, fault);

                let error = jwm
                    .unmapnotify(&mut backend, window, true)
                    .expect_err("injected configure-unmap fault must abort convergence");
                assert!(
                    !error.to_string().is_empty(),
                    "{fault:?} must retain a diagnostic"
                );
                assert_eq!(
                    jwm.state.clients[client_key], original_client,
                    "{fault:?} changed internal semantic/geometry state (hidden={hidden})"
                );
                let restored_server_geometry = backend.window_ops.get_geometry(window).unwrap();
                assert_eq!(
                    (
                        restored_server_geometry.x,
                        restored_server_geometry.y,
                        restored_server_geometry.w,
                        restored_server_geometry.h,
                        restored_server_geometry.border,
                    ),
                    (
                        original_server_geometry.x,
                        original_server_geometry.y,
                        original_server_geometry.w,
                        original_server_geometry.h,
                        original_server_geometry.border,
                    ),
                    "{fault:?} did not restore server geometry (hidden={hidden})"
                );
                assert_eq!(
                    backend.property_ops.wm_state.load(Ordering::Relaxed),
                    i64::from(wm_state_for_minimized(hidden)),
                    "{fault:?} did not restore WM_STATE (hidden={hidden})"
                );
                assert_eq!(
                    backend.property_ops.hidden.load(Ordering::Relaxed),
                    hidden,
                    "{fault:?} did not restore EWMH Hidden (hidden={hidden})"
                );
                assert_eq!(
                    backend.minimized, original_minimized,
                    "{fault:?} replayed compositor minimize/Genie (hidden={hidden})"
                );
                assert!(
                    backend
                        .window_ops
                        .focuses
                        .lock()
                        .expect("window focuses lock")
                        .is_empty(),
                    "{fault:?} changed focus (hidden={hidden})"
                );
                if hidden {
                    assert!(
                        !backend.iconify_requests.is_empty(),
                        "{fault:?} did not re-arm hidden Iconic ownership"
                    );
                } else {
                    assert!(
                        backend.iconify_requests.is_empty(),
                        "{fault:?} armed Iconic ownership for a visible client"
                    );
                    assert!(
                        backend
                            .window_ops
                            .get_window_attributes(window)
                            .unwrap()
                            .map_state_viewable,
                        "{fault:?} did not restore the visible client's mapped state"
                    );
                }
            }
        }
    }

    #[test]
    fn manage_claims_only_the_exact_pending_scratchpad_pid() {
        let mut backend = ClientSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let expected_pid = std::process::id();
        let expected_start_time =
            crate::jwm::scratchpad_pending::linux_process_start_time(expected_pid)
                .expect("test process must be observable in /proc");
        jwm.scratchpad_pending
            .register_spawned(
                expected_pid,
                "term".into(),
                Some(expected_start_time),
                std::time::Instant::now(),
            )
            .unwrap();

        let unrelated_pid = expected_pid.wrapping_add(1).max(1);
        backend
            .property_ops
            .window_pid
            .store(unrelated_pid, Ordering::Relaxed);
        let unrelated = WindowId::from_raw(0x8001);
        jwm.manage(
            &mut backend,
            unrelated,
            &Geometry {
                x: 30,
                y: 40,
                w: 500,
                h: 300,
                border: 0,
            },
        )
        .unwrap();
        assert!(jwm.scratchpads.is_empty());
        assert_eq!(
            jwm.scratchpad_pending.pending_pid_for_name("term"),
            Some(expected_pid),
            "an unrelated window must leave the pending identity intact"
        );

        backend
            .property_ops
            .window_pid
            .store(expected_pid, Ordering::Relaxed);
        let matching = WindowId::from_raw(0x8002);
        jwm.manage(
            &mut backend,
            matching,
            &Geometry {
                x: 60,
                y: 70,
                w: 500,
                h: 300,
                border: 0,
            },
        )
        .unwrap();

        let matching_key = jwm.wintoclient(matching).expect("matching client managed");
        assert_eq!(jwm.scratchpads.get("term"), Some(&matching_key));
        assert!(jwm.state.clients[matching_key].state.is_floating);
        assert!(jwm.scratchpad_pending.is_empty());
    }

    #[test]
    fn normal_unmanage_returns_a_minimized_window_to_visible_withdrawn_state() {
        let mut backend = ClientSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let window = WindowId::from_raw(0x8101);
        let (client, _monitor, monitor_num) = add_minimized_client(&mut jwm, window);
        jwm.schedule_hidden_client_park_retry(client, std::time::Instant::now());
        assert!(jwm.has_hidden_client_park_retry(client));
        jwm.active_minimized_preview = Some((monitor_num, window));
        backend.property_ops.hidden.store(true, Ordering::Relaxed);
        backend.property_ops.wm_state.store(
            i64::from(crate::jwm::types::ICONIC_STATE),
            Ordering::Relaxed,
        );

        jwm.unmanage(&mut backend, Some(client), false).unwrap();

        assert!(!jwm.state.clients.contains_key(client));
        assert!(!jwm.has_hidden_client_park_retry(client));
        assert_eq!(jwm.active_minimized_preview, None);
        assert_eq!(backend.previews, vec![(None, None)]);
        assert_eq!(backend.dock_targets, vec![(window, None)]);
        assert!(
            backend.minimized.is_empty(),
            "unmanage must not start restore"
        );
        assert!(!backend.property_ops.hidden.load(Ordering::Relaxed));
        assert_eq!(
            backend.property_ops.wm_state.load(Ordering::Relaxed),
            i64::from(WITHDRAWN_STATE)
        );
        assert!(!wm_state_or_ewmh_is_minimized(
            backend.property_ops.wm_state.load(Ordering::Relaxed),
            backend.property_ops.hidden.load(Ordering::Relaxed),
        ));
        assert_eq!(
            *backend
                .property_ops
                .writes
                .lock()
                .expect("protocol writes lock"),
            vec![
                ProtocolWrite::Hidden(false),
                ProtocolWrite::WmState(i64::from(WITHDRAWN_STATE)),
            ]
        );
        assert_eq!(
            *backend
                .property_ops
                .restore_accesses
                .lock()
                .expect("restore accesses lock"),
            vec![RestoreAccess::Clear(window)]
        );
        let changes = backend
            .window_ops
            .changes
            .lock()
            .expect("window changes lock");
        assert!(changes.iter().any(|(candidate, changes)| {
            *candidate == window && changes.x == Some(420) && changes.border_width == Some(3)
        }));
    }

    #[test]
    fn destroyed_unmanage_only_releases_jwm_and_compositor_ownership() {
        let mut backend = ClientSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let window = WindowId::from_raw(0x8102);
        let (client, _monitor, monitor_num) = add_minimized_client(&mut jwm, window);
        jwm.active_minimized_preview = Some((monitor_num, window));
        backend.property_ops.hidden.store(true, Ordering::Relaxed);
        backend.property_ops.wm_state.store(
            i64::from(crate::jwm::types::ICONIC_STATE),
            Ordering::Relaxed,
        );

        jwm.unmanage(&mut backend, Some(client), true).unwrap();

        assert!(!jwm.state.clients.contains_key(client));
        assert_eq!(jwm.active_minimized_preview, None);
        assert_eq!(backend.previews, vec![(None, None)]);
        assert_eq!(backend.dock_targets, vec![(window, None)]);
        assert!(backend.minimized.is_empty());
        assert!(backend.property_ops.hidden.load(Ordering::Relaxed));
        assert_eq!(
            backend.property_ops.wm_state.load(Ordering::Relaxed),
            i64::from(crate::jwm::types::ICONIC_STATE)
        );
        assert!(
            backend
                .property_ops
                .writes
                .lock()
                .expect("protocol writes lock")
                .is_empty()
        );
        assert!(
            backend
                .property_ops
                .restore_accesses
                .lock()
                .expect("restore accesses lock")
                .is_empty(),
            "DestroyNotify must not issue a private-property request against the dead window"
        );
        assert!(
            backend
                .window_ops
                .changes
                .lock()
                .expect("window changes lock")
                .is_empty()
        );
        assert!(
            backend
                .window_ops
                .event_masks
                .lock()
                .expect("event masks lock")
                .is_empty()
        );
        assert!(
            backend
                .window_ops
                .ungrabs
                .lock()
                .expect("ungrabs lock")
                .is_empty()
        );
    }

    #[test]
    fn x11_restore_that_remains_parked_rolls_back_and_can_be_retried() {
        let mut backend = ClientSpyBackend::new();
        backend.supports_client_list = true;
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let window = WindowId::from_raw(0x8103);
        let (client_key, _monitor, monitor_num) = add_minimized_client(&mut jwm, window);
        {
            let client = &mut jwm.state.clients[client_key];
            client.state.is_floating = true;
            client.geometry.floating_x = 420;
            client.geometry.floating_y = 80;
            client.geometry.floating_w = 640;
            client.geometry.floating_h = 480;
        }
        let snapshot = MinimizedRestoreState {
            tags: 1,
            monitor_num,
            visible_rect: MinimizedRestoreRect {
                x: 420,
                y: 80,
                w: 640,
                h: 480,
            },
            is_floating: true,
            is_drag_floating: false,
            floating_rect: Some(MinimizedRestoreRect {
                x: 420,
                y: 80,
                w: 640,
                h: 480,
            }),
            is_pip: false,
            pip_restore_sticky: false,
            old_state: false,
            fullscreen_restore_rect: None,
            minimized_order: 17,
        };
        *backend
            .property_ops
            .minimized_restore
            .lock()
            .expect("minimized restore lock") = Some(snapshot);
        backend.property_ops.hidden.store(true, Ordering::Relaxed);
        backend.property_ops.wm_state.store(
            i64::from(crate::jwm::types::ICONIC_STATE),
            Ordering::Relaxed,
        );
        *backend
            .window_ops
            .reported_geometry
            .lock()
            .expect("reported geometry lock") = Some(Geometry {
            x: -1600,
            y: 80,
            w: 640,
            h: 480,
            border: 0,
        });
        backend
            .window_ops
            .ignore_positions
            .store(true, Ordering::Relaxed);

        assert!(
            jwm.set_client_minimized(&mut backend, client_key, false)
                .is_err()
        );
        let client = &jwm.state.clients[client_key];
        assert!(client.state.is_hidden);
        assert_eq!(client.state.minimized_order, 17);
        assert_eq!(
            client.geometry.hidden_restore_rect,
            Some(Rect::new(420, 80, 640, 480))
        );
        assert_eq!(
            *backend
                .property_ops
                .minimized_restore
                .lock()
                .expect("minimized restore lock"),
            Some(snapshot),
            "failed unpark must retain the recovery snapshot"
        );
        assert!(backend.property_ops.hidden.load(Ordering::Relaxed));
        assert_eq!(
            backend.property_ops.wm_state.load(Ordering::Relaxed),
            i64::from(crate::jwm::types::ICONIC_STATE)
        );
        assert!(backend.minimized.is_empty());

        backend
            .window_ops
            .ignore_positions
            .store(false, Ordering::Relaxed);
        assert!(
            jwm.set_client_minimized(&mut backend, client_key, false)
                .unwrap()
        );
        let client = &jwm.state.clients[client_key];
        assert!(!client.state.is_hidden);
        assert_eq!(client.state.minimized_order, 0);
        assert_eq!(client.geometry.x, 420);
        assert_eq!(backend.minimized, vec![(window, false)]);
        assert!(
            backend
                .property_ops
                .minimized_restore
                .lock()
                .expect("minimized restore lock")
                .is_none()
        );
    }

    #[test]
    fn adopted_iconic_client_stays_offscreen_until_an_explicit_restore() {
        for (index, has_compositor) in [true, false].into_iter().enumerate() {
            let mut backend = ClientSpyBackend::new();
            backend.has_compositor = has_compositor;
            backend.property_ops.wm_state.store(
                i64::from(crate::jwm::types::ICONIC_STATE),
                Ordering::Relaxed,
            );
            let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
            let desktop_left = jwm.desktop_left_edge();
            let window = WindowId::from_raw(0x8200 + index as u64);
            let geometry = Geometry {
                x: desktop_left.saturating_sub(1600),
                y: 90,
                w: 640,
                h: 480,
                border: 0,
            };

            jwm.manage(&mut backend, window, &geometry).unwrap();

            let client_key = jwm.wintoclient(window).expect("managed iconic client");
            let client = &jwm.state.clients[client_key];
            let total_width = client.total_width().max(1);
            assert!(client.state.is_hidden);
            assert_ne!(client.state.minimized_order, 0);
            assert!(client.geometry.hidden_x.is_some());
            assert!(client.geometry.x.saturating_add(total_width) <= desktop_left);
            assert!(
                client
                    .geometry
                    .hidden_restore_rect
                    .is_some_and(|restore| restore.x.saturating_add(total_width) > desktop_left)
            );

            let configures = backend
                .window_ops
                .configures
                .lock()
                .expect("window configures lock");
            assert!(
                configures
                    .iter()
                    .any(|(candidate, ..)| *candidate == window)
            );
            assert!(configures.iter().all(|(candidate, x, _, width, _)| {
                *candidate != window
                    || x.saturating_add(i32::try_from(*width).unwrap_or(i32::MAX)) <= desktop_left
            }));
            drop(configures);

            let positions = backend
                .window_ops
                .positions
                .lock()
                .expect("window positions lock");
            assert!(positions.iter().all(|(candidate, x, _)| {
                *candidate != window || x.saturating_add(total_width) <= desktop_left
            }));
            drop(positions);

            let changes = backend
                .window_ops
                .changes
                .lock()
                .expect("window changes lock");
            assert!(changes.iter().all(|(candidate, changes)| {
                *candidate != window
                    || changes
                        .x
                        .is_none_or(|x| x.saturating_add(total_width) <= desktop_left)
            }));
            assert_eq!(backend.minimized, vec![(window, true)]);

            let normalized = backend
                .property_ops
                .minimized_restore
                .lock()
                .expect("minimized restore lock")
                .expect("legacy Iconic client must gain a restart snapshot");
            let restore = client
                .geometry
                .hidden_restore_rect
                .expect("legacy fallback restore slot");
            assert_eq!(normalized.minimized_order, client.state.minimized_order);
            assert_eq!(
                normalized.visible_rect,
                MinimizedRestoreRect {
                    x: restore.x,
                    y: restore.y,
                    w: restore.w,
                    h: restore.h,
                }
            );
            assert_eq!(
                *backend
                    .property_ops
                    .restore_accesses
                    .lock()
                    .expect("restore accesses lock"),
                vec![
                    RestoreAccess::Get(window),
                    RestoreAccess::Set(window, normalized),
                ]
            );
        }
    }

    #[test]
    fn native_new_window_staging_is_overflow_safe_and_outside_the_complete_desktop() {
        let mut backend = ClientSpyBackend::new();
        backend.has_compositor = false;
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        // This deliberately made the former `x + 2 * s_w` expression panic
        // in debug builds before it had a chance to stage the client.
        jwm.s_w = i32::MAX;
        let desktop_left = jwm.desktop_left_edge();
        let window = WindowId::from_raw(0x82ff);
        let mut client = WMClient::new(window);
        client.geometry.x = i32::MAX - 8;
        client.geometry.y = 90;
        client.geometry.w = 640;
        client.geometry.h = 480;
        let client_key = jwm.insert_client(client);

        jwm.setup_client_window(&mut backend, client_key).unwrap();

        let client = &jwm.state.clients[client_key];
        let total_width = client.total_width().max(1);
        let changes = backend
            .window_ops
            .changes
            .lock()
            .expect("window changes lock");
        let staging_x = changes
            .iter()
            .find_map(|(candidate, changes)| (*candidate == window).then_some(changes.x).flatten())
            .expect("native setup staging configure");
        assert!(staging_x.saturating_add(total_width) <= desktop_left);
        assert!(client.geometry.hidden_x.is_none());
        assert!(client.geometry.hidden_restore_rect.is_none());
    }

    #[test]
    fn native_decoration_hints_apply_before_map_and_reconcile_live_deletions() {
        let mut backend = ClientSpyBackend::new();
        backend.has_compositor = false;
        backend
            .property_ops
            .motif_borderless
            .store(true, Ordering::Relaxed);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let window = WindowId::from_raw(0x8300);
        let geometry = Geometry {
            x: 160,
            y: 90,
            w: 640,
            h: 480,
            border: 0,
        };

        jwm.manage(&mut backend, window, &geometry).unwrap();

        let client_key = jwm.wintoclient(window).expect("managed CSD client");
        let client = &jwm.state.clients[client_key];
        assert!(client.state.no_decorations);
        assert_eq!(client.geometry.border_w, 0);
        assert!(
            backend
                .window_ops
                .decorations
                .lock()
                .expect("window decorations lock")
                .iter()
                .filter(|(candidate, _)| *candidate == window)
                .all(|(_, border)| *border == 0),
            "the native border must never appear before or after MapWindow"
        );

        backend
            .property_ops
            .motif_borderless
            .store(false, Ordering::Relaxed);
        jwm.handle_motif_hints_change(&mut backend, client_key)
            .unwrap();
        let expected = CONFIG.load().border_px();
        assert!(!jwm.state.clients[client_key].state.no_decorations);
        assert_eq!(
            jwm.state.clients[client_key].geometry.border_w,
            expected as i32
        );
        assert_eq!(
            backend
                .window_ops
                .decorations
                .lock()
                .expect("window decorations lock")
                .last(),
            Some(&(window, expected))
        );

        backend
            .property_ops
            .gtk_client_frame
            .store(true, Ordering::Relaxed);
        jwm.handle_gtk_frame_extents_change(&mut backend, client_key)
            .unwrap();
        assert!(jwm.state.clients[client_key].state.no_decorations);
        assert_eq!(jwm.state.clients[client_key].geometry.border_w, 0);
        assert_eq!(
            backend
                .window_ops
                .decorations
                .lock()
                .expect("window decorations lock")
                .last(),
            Some(&(window, 0))
        );

        jwm.setfullscreen(&mut backend, client_key, true).unwrap();
        backend
            .property_ops
            .gtk_client_frame
            .store(false, Ordering::Relaxed);
        jwm.handle_gtk_frame_extents_change(&mut backend, client_key)
            .unwrap();
        let client = &jwm.state.clients[client_key];
        assert!(client.state.is_fullscreen);
        assert_eq!(client.geometry.border_w, 0);
        assert_eq!(client.geometry.old_border_w, expected as i32);

        jwm.setfullscreen(&mut backend, client_key, false).unwrap();
        assert_eq!(
            jwm.state.clients[client_key].geometry.border_w, expected as i32,
            "leaving fullscreen must restore the border policy adopted while fullscreen"
        );
    }

    #[test]
    fn native_external_dock_is_floating_focusless_and_never_gets_a_server_border() {
        let mut backend = ClientSpyBackend::new();
        backend.has_compositor = false;
        backend
            .property_ops
            .dock_type
            .store(true, Ordering::Relaxed);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let window = WindowId::from_raw(0x8301);

        jwm.manage(
            &mut backend,
            window,
            &Geometry {
                x: 0,
                y: 0,
                w: 1920,
                h: 32,
                border: 0,
            },
        )
        .unwrap();

        let client_key = jwm.wintoclient(window).expect("managed external Dock");
        let client = &jwm.state.clients[client_key];
        assert!(client.state.is_dock);
        assert!(client.state.is_floating);
        assert!(client.state.never_focus);
        assert_eq!(client.geometry.border_w, 0);
        assert!(
            backend
                .window_ops
                .decorations
                .lock()
                .expect("window decorations lock")
                .iter()
                .filter(|(candidate, _)| *candidate == window)
                .all(|(_, border)| *border == 0)
        );
    }

    #[test]
    fn legacy_iconic_adoption_survives_restart_snapshot_write_failure() {
        let mut backend = ClientSpyBackend::new();
        backend.property_ops.wm_state.store(
            i64::from(crate::jwm::types::ICONIC_STATE),
            Ordering::Relaxed,
        );
        backend
            .property_ops
            .fail_restore_set
            .store(true, Ordering::Relaxed);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let desktop_left = jwm.desktop_left_edge();
        let window = WindowId::from_raw(0x8299);
        let geometry = Geometry {
            x: desktop_left.saturating_sub(1600),
            y: 90,
            w: 640,
            h: 480,
            border: 0,
        };

        jwm.manage(&mut backend, window, &geometry).unwrap();

        let client_key = jwm
            .wintoclient(window)
            .expect("managed legacy Iconic client");
        let client = &jwm.state.clients[client_key];
        assert!(client.state.is_hidden);
        assert_ne!(client.state.minimized_order, 0);
        assert!(client.geometry.hidden_restore_rect.is_some());
        assert!(client.geometry.x.saturating_add(client.total_width()) <= desktop_left);
        assert!(
            backend
                .property_ops
                .minimized_restore
                .lock()
                .expect("minimized restore lock")
                .is_none()
        );
        let accesses = backend
            .property_ops
            .restore_accesses
            .lock()
            .expect("restore accesses lock");
        assert_eq!(accesses.len(), 2);
        assert_eq!(accesses[0], RestoreAccess::Get(window));
        assert!(matches!(accesses[1], RestoreAccess::Set(candidate, _) if candidate == window));
    }

    #[test]
    fn restart_snapshot_restores_floating_pip_state_and_stable_order() {
        let mut backend = ClientSpyBackend::new();
        backend.property_ops.wm_state.store(
            i64::from(crate::jwm::types::ICONIC_STATE),
            Ordering::Relaxed,
        );
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let monitor_num = jwm.state.monitors[monitor].num;
        let area = jwm.monitor_work_area(monitor).expect("monitor work area");
        let visible = MinimizedRestoreRect {
            x: area.x + 35,
            y: area.y + 45,
            w: 420,
            h: 260,
        };
        let floating = MinimizedRestoreRect {
            x: area.x + 90,
            y: area.y + 80,
            w: 960,
            h: 700,
        };
        let adopted_order = 9_000_000_000_u64;
        let snapshot = MinimizedRestoreState {
            tags: 0b100,
            monitor_num,
            visible_rect: visible,
            is_floating: true,
            is_drag_floating: true,
            floating_rect: Some(floating),
            is_pip: true,
            pip_restore_sticky: true,
            old_state: true,
            fullscreen_restore_rect: None,
            minimized_order: adopted_order,
        };
        *backend
            .property_ops
            .minimized_restore
            .lock()
            .expect("minimized restore lock") = Some(snapshot);

        let desktop_left = jwm.desktop_left_edge();
        let window = WindowId::from_raw(0x8301);
        let geometry = Geometry {
            x: desktop_left.saturating_sub(2000),
            y: visible.y,
            w: visible.w as u32,
            h: visible.h as u32,
            border: 0,
        };
        jwm.manage(&mut backend, window, &geometry).unwrap();

        let client_key = jwm.wintoclient(window).expect("adopted PiP client");
        let client = &jwm.state.clients[client_key];
        assert_eq!(client.mon, Some(monitor));
        assert_eq!(client.state.tags, snapshot.tags & CONFIG.load().tagmask());
        assert_eq!(client.state.minimized_order, adopted_order);
        assert!(client.state.is_hidden);
        assert!(client.state.is_floating);
        assert!(client.state.is_drag_floating);
        assert!(client.state.is_pip);
        assert!(client.state.is_sticky);
        assert!(client.state.pip_restore_sticky);
        assert!(client.state.old_state);
        assert_eq!(
            client.geometry.hidden_restore_rect,
            Some(Rect::new(visible.x, visible.y, visible.w, visible.h))
        );
        assert_eq!(
            (
                client.geometry.floating_x,
                client.geometry.floating_y,
                client.geometry.floating_w,
                client.geometry.floating_h,
            ),
            (floating.x, floating.y, floating.w, floating.h)
        );
        assert!(client.geometry.x.saturating_add(client.total_width()) <= desktop_left);
        assert_eq!(backend.pips, vec![(window, true)]);
        assert_eq!(backend.minimized, vec![(window, true)]);
        assert_eq!(
            *backend
                .property_ops
                .minimized_restore
                .lock()
                .expect("minimized restore lock"),
            Some(snapshot),
            "normalization must retain PiP's small visible rect and separate exit geometry"
        );
        assert!(
            crate::jwm::window_state::next_minimized_order().expect("minimized order capacity")
                > adopted_order,
            "new Dock orders must advance past an adopted restart order"
        );

        assert!(
            jwm.set_client_minimized(&mut backend, client_key, false)
                .unwrap()
        );
        let client = &jwm.state.clients[client_key];
        assert_eq!(
            Rect::new(
                client.geometry.x,
                client.geometry.y,
                client.geometry.w,
                client.geometry.h,
            ),
            Rect::new(visible.x, visible.y, visible.w, visible.h)
        );
        assert!(jwm.set_client_pip(&mut backend, client_key, false).unwrap());
        let client = &jwm.state.clients[client_key];
        assert!(!client.state.is_pip);
        assert!(client.state.is_floating);
        assert!(client.state.is_sticky);
        assert!(!client.state.pip_restore_sticky);
        assert_eq!(
            Rect::new(
                client.geometry.x,
                client.geometry.y,
                client.geometry.w,
                client.geometry.h,
            ),
            Rect::new(floating.x, floating.y, floating.w, floating.h)
        );
        assert_eq!(backend.pips, vec![(window, true), (window, false)]);
        assert!(
            backend
                .property_ops
                .minimized_restore
                .lock()
                .expect("minimized restore lock")
                .is_none()
        );
    }

    #[test]
    fn physically_unmapped_iconic_adoption_keeps_v1_restore_rect_separate_from_server_geometry() {
        let mut backend = ClientSpyBackend::new();
        backend.supports_client_list = true;
        backend.property_ops.wm_state.store(
            i64::from(crate::jwm::types::ICONIC_STATE),
            Ordering::Relaxed,
        );
        backend
            .window_ops
            .force_unmapped
            .store(true, Ordering::Relaxed);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let monitor_num = jwm.state.monitors[monitor].num;
        let area = jwm.monitor_work_area(monitor).expect("monitor work area");
        let visible = MinimizedRestoreRect {
            x: area.x + 180,
            y: area.y + 120,
            w: 760,
            h: 540,
        };
        let snapshot = MinimizedRestoreState {
            tags: jwm.state.monitors[monitor].get_active_tags(),
            monitor_num,
            visible_rect: visible,
            is_floating: true,
            is_drag_floating: false,
            floating_rect: Some(visible),
            is_pip: false,
            pip_restore_sticky: false,
            old_state: true,
            fullscreen_restore_rect: None,
            minimized_order: 9_000_000_075,
        };
        *backend
            .property_ops
            .minimized_restore
            .lock()
            .expect("minimized restore lock") = Some(snapshot);
        let desktop_left = jwm.desktop_left_edge();
        let server_geometry = Geometry {
            x: desktop_left.saturating_sub(9000),
            y: -7100,
            w: 123,
            h: 77,
            border: 5,
        };
        *backend
            .window_ops
            .reported_geometry
            .lock()
            .expect("reported geometry lock") = Some(server_geometry);
        let window = WindowId::from_raw(0x8309);

        jwm.manage(&mut backend, window, &server_geometry).unwrap();

        let client_key = jwm.wintoclient(window).expect("adopted Iconic client");
        let client = &jwm.state.clients[client_key];
        assert!(client.state.is_hidden);
        assert_eq!(
            client.geometry.hidden_restore_rect,
            Some(Rect::new(visible.x, visible.y, visible.w, visible.h)),
            "the durable V1 rect, not the server's parking rect, is the restore target"
        );
        let parked = backend.window_ops.get_geometry(window).unwrap();
        assert!(x11_geometry_fully_left_of_desktop(parked, desktop_left));
        assert_eq!(
            (parked.y, parked.w, parked.h),
            (visible.y, visible.w as u32, visible.h as u32)
        );
        assert_eq!(
            backend
                .property_ops
                .minimized_restore
                .lock()
                .expect("minimized restore lock")
                .expect("normalized V1")
                .visible_rect,
            visible
        );

        assert!(
            jwm.set_client_minimized(&mut backend, client_key, false)
                .unwrap()
        );
        let client = &jwm.state.clients[client_key];
        assert_eq!(
            Rect::new(
                client.geometry.x,
                client.geometry.y,
                client.geometry.w,
                client.geometry.h,
            ),
            Rect::new(visible.x, visible.y, visible.w, visible.h)
        );
    }

    #[test]
    fn hostile_restart_order_is_rebased_without_losing_restore_geometry() {
        let mut backend = ClientSpyBackend::new();
        backend.property_ops.wm_state.store(
            i64::from(crate::jwm::types::ICONIC_STATE),
            Ordering::Relaxed,
        );
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let monitor_num = jwm.state.monitors[monitor].num;
        let active_tags = jwm.state.monitors[monitor].get_active_tags();
        let area = jwm.monitor_work_area(monitor).expect("monitor work area");
        let visible = MinimizedRestoreRect {
            x: area.x + 73,
            y: area.y + 51,
            w: 640,
            h: 420,
        };
        let advertised_order = crate::backend::api::MAX_MINIMIZED_RESTORE_ORDER;
        let snapshot = MinimizedRestoreState {
            tags: active_tags,
            monitor_num,
            visible_rect: visible,
            is_floating: true,
            is_drag_floating: false,
            floating_rect: Some(visible),
            is_pip: false,
            pip_restore_sticky: false,
            old_state: true,
            fullscreen_restore_rect: None,
            minimized_order: advertised_order,
        };
        *backend
            .property_ops
            .minimized_restore
            .lock()
            .expect("minimized restore lock") = Some(snapshot);

        let desktop_left = jwm.desktop_left_edge();
        let window = WindowId::from_raw(0x8308);
        let geometry = Geometry {
            x: desktop_left.saturating_sub(2200),
            y: visible.y,
            w: visible.w as u32,
            h: visible.h as u32,
            border: 0,
        };
        jwm.manage(&mut backend, window, &geometry).unwrap();

        let client_key = jwm.wintoclient(window).expect("adopted Iconic client");
        let client = &jwm.state.clients[client_key];
        assert!(client.state.is_hidden);
        assert_ne!(client.state.minimized_order, advertised_order);
        assert!(
            (1..=crate::jwm::window_state::MAX_RECOVERED_MINIMIZED_ORDER)
                .contains(&client.state.minimized_order)
        );
        assert_eq!(
            client.geometry.hidden_restore_rect,
            Some(Rect::new(visible.x, visible.y, visible.w, visible.h))
        );

        let normalized = backend
            .property_ops
            .minimized_restore
            .lock()
            .expect("minimized restore lock")
            .expect("adoption rewrites the normalized snapshot");
        assert_eq!(normalized.visible_rect, visible);
        assert_eq!(normalized.minimized_order, client.state.minimized_order);
    }

    #[test]
    fn fullscreen_property_wins_over_pip_snapshot_without_exposing_iconic_geometry() {
        let mut backend = ClientSpyBackend::new();
        backend.property_ops.wm_state.store(
            i64::from(crate::jwm::types::ICONIC_STATE),
            Ordering::Relaxed,
        );
        backend
            .property_ops
            .fullscreen
            .store(true, Ordering::Relaxed);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let monitor_num = jwm.state.monitors[monitor].num;
        let area = jwm.monitor_work_area(monitor).expect("monitor work area");
        let pip_rect = MinimizedRestoreRect {
            x: area.x + area.w - 430,
            y: area.y + area.h - 280,
            w: 420,
            h: 270,
        };
        let before_pip = MinimizedRestoreRect {
            x: area.x + 110,
            y: area.y + 85,
            w: 880,
            h: 640,
        };
        let snapshot = MinimizedRestoreState {
            tags: jwm.state.monitors[monitor].get_active_tags(),
            monitor_num,
            visible_rect: pip_rect,
            is_floating: true,
            is_drag_floating: false,
            floating_rect: Some(before_pip),
            is_pip: true,
            pip_restore_sticky: true,
            old_state: false,
            fullscreen_restore_rect: None,
            minimized_order: 9_000_000_050,
        };
        *backend
            .property_ops
            .minimized_restore
            .lock()
            .expect("minimized restore lock") = Some(snapshot);

        let desktop_left = jwm.desktop_left_edge();
        let window = WindowId::from_raw(0x8306);
        let geometry = Geometry {
            x: desktop_left.saturating_sub(pip_rect.w.saturating_mul(2)),
            y: pip_rect.y,
            w: pip_rect.w as u32,
            h: pip_rect.h as u32,
            border: 0,
        };
        jwm.manage(&mut backend, window, &geometry).unwrap();

        let client_key = jwm.wintoclient(window).expect("adopted fullscreen client");
        let client = &jwm.state.clients[client_key];
        assert!(client.state.is_hidden);
        assert!(client.state.is_fullscreen);
        assert!(!client.state.is_pip);
        assert!(!client.state.pip_restore_sticky);
        assert!(client.state.is_sticky);
        assert!(!client.state.old_state);
        assert_eq!(
            Rect::new(
                client.geometry.old_x,
                client.geometry.old_y,
                client.geometry.old_w,
                client.geometry.old_h,
            ),
            Rect::new(before_pip.x, before_pip.y, before_pip.w, before_pip.h,)
        );
        assert_eq!(backend.pips, vec![(window, false)]);
        assert!(
            backend
                .window_ops
                .configures
                .lock()
                .expect("window configures lock")
                .iter()
                .all(|(candidate, x, _, width, _)| {
                    *candidate != window
                        || x.saturating_add(i32::try_from(*width).unwrap_or(i32::MAX))
                            <= desktop_left
                })
        );
        let normalized = backend
            .property_ops
            .minimized_restore
            .lock()
            .expect("minimized restore lock")
            .expect("normalized fullscreen snapshot");
        assert!(!normalized.is_pip);
        assert!(!normalized.pip_restore_sticky);
        assert_eq!(normalized.fullscreen_restore_rect, Some(before_pip));

        assert!(
            jwm.set_client_minimized(&mut backend, client_key, false)
                .unwrap()
        );
        jwm.setfullscreen(&mut backend, client_key, false).unwrap();
        let client = &jwm.state.clients[client_key];
        assert!(!client.state.is_hidden);
        assert!(!client.state.is_fullscreen);
        assert!(!client.state.is_pip);
        assert!(!client.state.is_floating);
        assert!(client.state.is_sticky);
    }

    #[test]
    fn fullscreen_snapshot_adoption_never_configures_the_iconic_window_onscreen() {
        let mut backend = ClientSpyBackend::new();
        backend.property_ops.wm_state.store(
            i64::from(crate::jwm::types::ICONIC_STATE),
            Ordering::Relaxed,
        );
        backend
            .property_ops
            .fullscreen
            .store(true, Ordering::Relaxed);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let monitor_num = jwm.state.monitors[monitor].num;
        let active_tags = jwm.state.monitors[monitor].get_active_tags();
        let mon = &jwm.state.monitors[monitor];
        let fullscreen = MinimizedRestoreRect {
            x: mon.geometry.m_x,
            y: mon.geometry.m_y,
            w: mon.geometry.m_w,
            h: mon.geometry.m_h,
        };
        let before_fullscreen = MinimizedRestoreRect {
            x: mon.geometry.w_x + 160,
            y: mon.geometry.w_y + 110,
            w: 980,
            h: 720,
        };
        let snapshot = MinimizedRestoreState {
            tags: active_tags,
            monitor_num,
            visible_rect: fullscreen,
            is_floating: true,
            is_drag_floating: false,
            floating_rect: Some(before_fullscreen),
            is_pip: false,
            pip_restore_sticky: false,
            old_state: true,
            fullscreen_restore_rect: Some(before_fullscreen),
            minimized_order: 9_000_000_100,
        };
        *backend
            .property_ops
            .minimized_restore
            .lock()
            .expect("minimized restore lock") = Some(snapshot);

        let desktop_left = jwm.desktop_left_edge();
        let window = WindowId::from_raw(0x8302);
        let geometry = Geometry {
            x: desktop_left.saturating_sub(fullscreen.w.saturating_mul(2)),
            y: fullscreen.y,
            w: fullscreen.w as u32,
            h: fullscreen.h as u32,
            border: 0,
        };
        jwm.manage(&mut backend, window, &geometry).unwrap();

        let client_key = jwm.wintoclient(window).expect("adopted fullscreen client");
        let client = &jwm.state.clients[client_key];
        assert!(client.state.is_hidden);
        assert!(client.state.is_fullscreen);
        assert_eq!(
            client.geometry.hidden_restore_rect,
            Some(Rect::new(
                fullscreen.x,
                fullscreen.y,
                fullscreen.w,
                fullscreen.h,
            ))
        );
        assert_eq!(
            Rect::new(
                client.geometry.old_x,
                client.geometry.old_y,
                client.geometry.old_w,
                client.geometry.old_h,
            ),
            Rect::new(
                before_fullscreen.x,
                before_fullscreen.y,
                before_fullscreen.w,
                before_fullscreen.h,
            )
        );
        let configures = backend
            .window_ops
            .configures
            .lock()
            .expect("window configures lock");
        assert!(
            configures
                .iter()
                .any(|(candidate, ..)| *candidate == window)
        );
        assert!(configures.iter().all(|(candidate, x, _, width, _)| {
            *candidate != window
                || x.saturating_add(i32::try_from(*width).unwrap_or(i32::MAX)) <= desktop_left
        }));
        drop(configures);
        let changes = backend
            .window_ops
            .changes
            .lock()
            .expect("window changes lock");
        assert!(changes.iter().all(|(candidate, changes)| {
            *candidate != window
                || changes
                    .x
                    .is_none_or(|x| x.saturating_add(fullscreen.w) <= desktop_left)
        }));
        drop(changes);

        assert!(
            jwm.set_client_minimized(&mut backend, client_key, false)
                .unwrap()
        );
        assert_eq!(
            jwm.state.clients[client_key].geometry.hidden_restore_rect,
            None
        );
        assert_eq!(
            Rect::new(
                jwm.state.clients[client_key].geometry.x,
                jwm.state.clients[client_key].geometry.y,
                jwm.state.clients[client_key].geometry.w,
                jwm.state.clients[client_key].geometry.h,
            ),
            Rect::new(fullscreen.x, fullscreen.y, fullscreen.w, fullscreen.h,)
        );
        jwm.setfullscreen(&mut backend, client_key, false).unwrap();
        assert_eq!(
            Rect::new(
                jwm.state.clients[client_key].geometry.x,
                jwm.state.clients[client_key].geometry.y,
                jwm.state.clients[client_key].geometry.w,
                jwm.state.clients[client_key].geometry.h,
            ),
            Rect::new(
                before_fullscreen.x,
                before_fullscreen.y,
                before_fullscreen.w,
                before_fullscreen.h,
            )
        );
    }

    #[test]
    fn normal_state_still_parked_with_a_snapshot_recovers_interrupted_restore() {
        let mut backend = ClientSpyBackend::new();
        backend.property_ops.wm_state.store(
            i64::from(crate::jwm::types::NORMAL_STATE),
            Ordering::Relaxed,
        );
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let monitor_num = jwm.state.monitors[monitor].num;
        let active_tags = jwm.state.monitors[monitor].get_active_tags();
        let area = jwm.monitor_work_area(monitor).expect("monitor work area");
        let visible = MinimizedRestoreRect {
            x: area.x + 210,
            y: area.y + 130,
            w: 780,
            h: 560,
        };
        let floating = MinimizedRestoreRect {
            x: area.x + 190,
            y: area.y + 115,
            w: 820,
            h: 590,
        };
        let snapshot = MinimizedRestoreState {
            tags: active_tags,
            monitor_num,
            visible_rect: visible,
            is_floating: true,
            is_drag_floating: true,
            floating_rect: Some(floating),
            is_pip: false,
            pip_restore_sticky: false,
            old_state: true,
            fullscreen_restore_rect: None,
            minimized_order: 9_000_000_150,
        };
        *backend
            .property_ops
            .minimized_restore
            .lock()
            .expect("minimized restore lock") = Some(snapshot);
        let desktop_left = jwm.desktop_left_edge();
        let window = WindowId::from_raw(0x8305);
        let geometry = Geometry {
            x: desktop_left.saturating_sub(1800),
            y: visible.y,
            w: visible.w as u32,
            h: visible.h as u32,
            border: 0,
        };

        jwm.manage(&mut backend, window, &geometry).unwrap();

        let client_key = jwm.wintoclient(window).expect("recovered restore client");
        let client = &jwm.state.clients[client_key];
        assert!(!client.state.is_hidden);
        assert_eq!(client.state.minimized_order, 0);
        assert_eq!(client.mon, Some(monitor));
        assert_eq!(client.state.tags, snapshot.tags & CONFIG.load().tagmask());
        assert!(client.state.is_floating);
        assert!(client.state.is_drag_floating);
        assert!(client.state.old_state);
        assert_eq!(
            Rect::new(
                client.geometry.x,
                client.geometry.y,
                client.geometry.w,
                client.geometry.h,
            ),
            Rect::new(visible.x, visible.y, visible.w, visible.h)
        );
        assert_eq!(
            (
                client.geometry.floating_x,
                client.geometry.floating_y,
                client.geometry.floating_w,
                client.geometry.floating_h,
            ),
            (floating.x, floating.y, floating.w, floating.h)
        );
        assert!(backend.minimized.is_empty());
        assert!(
            backend
                .property_ops
                .minimized_restore
                .lock()
                .expect("minimized restore lock")
                .is_none()
        );
        assert_eq!(
            *backend
                .property_ops
                .restore_accesses
                .lock()
                .expect("restore accesses lock"),
            vec![RestoreAccess::Get(window), RestoreAccess::Clear(window)]
        );
        assert!(
            backend
                .window_ops
                .configures
                .lock()
                .expect("window configures lock")
                .iter()
                .filter(|(candidate, ..)| *candidate == window)
                .all(|(_, x, _, width, _)| {
                    x.saturating_add(i32::try_from(*width).unwrap_or(i32::MAX)) > desktop_left
                }),
            "interrupted restore must configure the semantic target, not the parking x"
        );
    }

    #[test]
    fn normal_onscreen_client_discards_a_stale_snapshot_without_adopting_it() {
        let mut backend = ClientSpyBackend::new();
        backend.property_ops.wm_state.store(
            i64::from(crate::jwm::types::NORMAL_STATE),
            Ordering::Relaxed,
        );
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let monitor_num = jwm.state.monitors[monitor].num;
        let area = jwm.monitor_work_area(monitor).expect("monitor work area");
        let stale = MinimizedRestoreState {
            tags: 0b1000,
            monitor_num,
            visible_rect: MinimizedRestoreRect {
                x: area.x + 400,
                y: area.y + 300,
                w: 500,
                h: 350,
            },
            is_floating: true,
            is_drag_floating: false,
            floating_rect: Some(MinimizedRestoreRect {
                x: area.x + 420,
                y: area.y + 320,
                w: 470,
                h: 320,
            }),
            is_pip: false,
            pip_restore_sticky: false,
            old_state: true,
            fullscreen_restore_rect: None,
            minimized_order: 9_000_000_151,
        };
        *backend
            .property_ops
            .minimized_restore
            .lock()
            .expect("minimized restore lock") = Some(stale);
        let server = Rect::new(area.x + 25, area.y + 35, 640, 480);
        let window = WindowId::from_raw(0x8306);
        let geometry = Geometry {
            x: server.x,
            y: server.y,
            w: server.w as u32,
            h: server.h as u32,
            border: 0,
        };

        jwm.manage(&mut backend, window, &geometry).unwrap();

        let client_key = jwm.wintoclient(window).expect("normal client");
        let client = &jwm.state.clients[client_key];
        assert!(!client.state.is_hidden);
        assert_eq!(client.state.minimized_order, 0);
        assert_eq!(client.geometry.x, server.x);
        assert_eq!(client.geometry.y, server.y);
        assert_eq!(
            *backend
                .property_ops
                .restore_accesses
                .lock()
                .expect("restore accesses lock"),
            vec![RestoreAccess::Clear(window)]
        );
        assert!(
            backend
                .property_ops
                .minimized_restore
                .lock()
                .expect("minimized restore lock")
                .is_none()
        );
    }

    #[test]
    fn seamless_restart_refreshes_moved_hidden_state_without_clearing_it() {
        let mut backend = ClientSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let window = WindowId::from_raw(0x8303);
        let (client_key, monitor, monitor_num) = add_minimized_client(&mut jwm, window);
        let refreshed_visible = Rect::new(610, 175, 710, 515);
        {
            let client = &mut jwm.state.clients[client_key];
            client.state.tags = 0b1000;
            client.state.minimized_order = 9_000_000_200;
            client.state.is_floating = true;
            client.geometry.hidden_restore_rect = Some(refreshed_visible);
            client.geometry.floating_x = 630;
            client.geometry.floating_y = 195;
            client.geometry.floating_w = 680;
            client.geometry.floating_h = 470;
        }
        let stale = MinimizedRestoreState {
            tags: 1,
            monitor_num,
            visible_rect: MinimizedRestoreRect {
                x: 10,
                y: 20,
                w: 300,
                h: 200,
            },
            is_floating: false,
            is_drag_floating: false,
            floating_rect: None,
            is_pip: false,
            pip_restore_sticky: false,
            old_state: false,
            fullscreen_restore_rect: None,
            minimized_order: 17,
        };
        *backend
            .property_ops
            .minimized_restore
            .lock()
            .expect("minimized restore lock") = Some(stale);
        backend.property_ops.hidden.store(true, Ordering::Relaxed);
        backend.property_ops.wm_state.store(
            i64::from(crate::jwm::types::ICONIC_STATE),
            Ordering::Relaxed,
        );
        jwm.is_restarting.store(true, Ordering::SeqCst);

        jwm.cleanup_all_clients_x11_state(&mut backend).unwrap();

        let expected = MinimizedRestoreState {
            tags: 0b1000,
            monitor_num,
            visible_rect: MinimizedRestoreRect {
                x: refreshed_visible.x,
                y: refreshed_visible.y,
                w: refreshed_visible.w,
                h: refreshed_visible.h,
            },
            is_floating: true,
            is_drag_floating: false,
            floating_rect: Some(MinimizedRestoreRect {
                x: 630,
                y: 195,
                w: 680,
                h: 470,
            }),
            is_pip: false,
            pip_restore_sticky: false,
            old_state: false,
            fullscreen_restore_rect: None,
            minimized_order: 9_000_000_200,
        };
        assert_eq!(
            *backend
                .property_ops
                .minimized_restore
                .lock()
                .expect("minimized restore lock"),
            Some(expected)
        );
        assert_eq!(
            *backend
                .property_ops
                .restore_accesses
                .lock()
                .expect("restore accesses lock"),
            vec![RestoreAccess::Set(window, expected)]
        );
        assert!(jwm.state.clients[client_key].state.is_hidden);
        assert_eq!(jwm.state.clients[client_key].mon, Some(monitor));
        assert!(backend.property_ops.hidden.load(Ordering::Relaxed));
        assert_eq!(
            backend.property_ops.wm_state.load(Ordering::Relaxed),
            i64::from(crate::jwm::types::ICONIC_STATE)
        );
    }

    #[test]
    fn normal_shutdown_clears_restart_state_and_returns_hidden_window_visible() {
        let mut backend = ClientSpyBackend::new();
        backend.supports_client_list = true;
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let window = WindowId::from_raw(0x8304);
        let (client_key, _monitor, monitor_num) = add_minimized_client(&mut jwm, window);
        let snapshot = MinimizedRestoreState {
            tags: 1,
            monitor_num,
            visible_rect: MinimizedRestoreRect {
                x: 420,
                y: 80,
                w: 640,
                h: 480,
            },
            is_floating: false,
            is_drag_floating: false,
            floating_rect: None,
            is_pip: false,
            pip_restore_sticky: false,
            old_state: false,
            fullscreen_restore_rect: None,
            minimized_order: 17,
        };
        *backend
            .property_ops
            .minimized_restore
            .lock()
            .expect("minimized restore lock") = Some(snapshot);
        backend.property_ops.hidden.store(true, Ordering::Relaxed);
        backend.property_ops.wm_state.store(
            i64::from(crate::jwm::types::ICONIC_STATE),
            Ordering::Relaxed,
        );

        jwm.cleanup_all_clients_x11_state(&mut backend).unwrap();

        let client = &jwm.state.clients[client_key];
        assert!(!client.state.is_hidden);
        assert_eq!(client.state.minimized_order, 0);
        assert_eq!(client.geometry.x, 420);
        assert!(
            backend
                .property_ops
                .minimized_restore
                .lock()
                .expect("minimized restore lock")
                .is_none()
        );
        assert_eq!(
            *backend
                .property_ops
                .restore_accesses
                .lock()
                .expect("restore accesses lock"),
            vec![RestoreAccess::Clear(window)]
        );
        assert!(!backend.property_ops.hidden.load(Ordering::Relaxed));
        assert_eq!(
            backend.property_ops.wm_state.load(Ordering::Relaxed),
            i64::from(WITHDRAWN_STATE)
        );
    }

    fn prepare_initially_iconic_x11(backend: &mut ClientSpyBackend) -> (Jwm, Geometry, i32) {
        backend.supports_client_list = true;
        backend.property_ops.wm_state.store(
            i64::from(crate::jwm::types::ICONIC_STATE),
            Ordering::Relaxed,
        );
        backend
            .window_ops
            .force_unmapped
            .store(true, Ordering::Relaxed);
        let jwm = Jwm::new_with_runtime_backend(backend, "test").unwrap();
        let desktop_left = jwm.desktop_left_edge();
        let geometry = Geometry {
            x: desktop_left.saturating_add(240),
            y: 90,
            w: 640,
            h: 480,
            border: 0,
        };
        *backend
            .window_ops
            .reported_geometry
            .lock()
            .expect("reported geometry lock") = Some(geometry);
        backend
            .window_ops
            .initial_iconic_operations
            .lock()
            .expect("initial Iconic operations lock")
            .clear();
        backend
            .window_ops
            .attributes_calls
            .store(0, Ordering::Relaxed);
        backend
            .window_ops
            .geometry_calls
            .store(0, Ordering::Relaxed);

        (jwm, geometry, desktop_left)
    }

    fn manage_initially_iconic_x11(
        backend: &mut ClientSpyBackend,
        window: WindowId,
    ) -> (Jwm, ClientKey, i32) {
        let (mut jwm, geometry, desktop_left) = prepare_initially_iconic_x11(backend);

        jwm.manage(backend, window, &geometry).unwrap();
        let client_key = jwm.wintoclient(window).expect("managed Iconic client");
        (jwm, client_key, desktop_left)
    }

    #[test]
    fn initially_iconic_protocol_read_failures_stop_before_parking_or_mapping() {
        for (index, failed_read) in ["WM_STATE", "EWMH Hidden"].into_iter().enumerate() {
            let mut backend = ClientSpyBackend::new();
            let window = WindowId::from_raw(0x8410 + index as u64);
            let (mut jwm, geometry, _) = prepare_initially_iconic_x11(&mut backend);
            match failed_read {
                "WM_STATE" => backend
                    .property_ops
                    .fail_next_wm_state_read
                    .store(true, Ordering::Relaxed),
                "EWMH Hidden" => backend
                    .property_ops
                    .fail_next_hidden_read
                    .store(true, Ordering::Relaxed),
                _ => unreachable!(),
            }

            let error = jwm
                .manage(&mut backend, window, &geometry)
                .expect_err("an inconclusive minimized-state read must fail closed");

            assert!(error.to_string().contains(failed_read));
            assert!(jwm.wintoclient(window).is_none());
            assert!(
                backend
                    .window_ops
                    .maps
                    .lock()
                    .expect("window maps lock")
                    .is_empty()
            );
            assert!(backend.minimized.is_empty());
            assert!(backend.iconify_requests.is_empty());
            assert!(
                backend
                    .window_ops
                    .initial_iconic_operations
                    .lock()
                    .expect("initial Iconic operations lock")
                    .is_empty(),
                "no physical mutation may precede conclusive minimized-state reads"
            );
        }
    }

    #[test]
    fn initially_iconic_v1_read_failure_never_overwrites_or_maps_from_server_geometry() {
        let mut backend = ClientSpyBackend::new();
        let window = WindowId::from_raw(0x8412);
        let (mut jwm, geometry, _) = prepare_initially_iconic_x11(&mut backend);
        let monitor = jwm.state.monitor_order[0];
        let area = jwm.monitor_work_area(monitor).expect("monitor work area");
        let snapshot = MinimizedRestoreState {
            tags: jwm.state.monitors[monitor].get_active_tags(),
            monitor_num: jwm.state.monitors[monitor].num,
            visible_rect: MinimizedRestoreRect {
                x: area.x + 140,
                y: area.y + 100,
                w: 700,
                h: 500,
            },
            is_floating: true,
            is_drag_floating: false,
            floating_rect: None,
            is_pip: false,
            pip_restore_sticky: false,
            old_state: true,
            fullscreen_restore_rect: None,
            minimized_order: 9_000_000_076,
        };
        *backend
            .property_ops
            .minimized_restore
            .lock()
            .expect("minimized restore lock") = Some(snapshot);
        backend
            .property_ops
            .fail_restore_get
            .store(true, Ordering::Relaxed);

        let error = jwm
            .manage(&mut backend, window, &geometry)
            .expect_err("an unreadable V1 must stop adoption");

        assert!(error.to_string().contains("minimized restore read"));
        assert!(jwm.wintoclient(window).is_none());
        assert_eq!(
            *backend
                .property_ops
                .minimized_restore
                .lock()
                .expect("minimized restore lock"),
            Some(snapshot),
            "an unreadable V1 must not be replaced from server parking geometry"
        );
        assert_eq!(
            *backend
                .property_ops
                .restore_accesses
                .lock()
                .expect("restore accesses lock"),
            vec![RestoreAccess::Get(window)]
        );
        assert!(
            backend
                .window_ops
                .maps
                .lock()
                .expect("window maps lock")
                .is_empty()
        );
        assert!(backend.minimized.is_empty());
        assert!(backend.iconify_requests.is_empty());
    }

    #[test]
    fn initially_iconic_x11_client_is_mapped_captured_parked_then_iconified() {
        let mut backend = ClientSpyBackend::new();
        let window = WindowId::from_raw(0x8401);

        let (jwm, client_key, desktop_left) = manage_initially_iconic_x11(&mut backend, window);

        assert!(jwm.state.clients[client_key].state.is_hidden);
        assert_eq!(
            *backend.window_ops.maps.lock().expect("window maps lock"),
            vec![window]
        );
        assert_eq!(backend.minimized, vec![(window, true)]);
        assert_eq!(backend.iconify_requests, vec![window]);
        let operations = backend
            .window_ops
            .initial_iconic_operations
            .lock()
            .expect("initial Iconic operations lock")
            .clone();
        let map = operations
            .iter()
            .position(|operation| *operation == InitialIconicOperation::Map)
            .expect("MapWindow operation");
        let pre_map_configure = operations[..map]
            .iter()
            .rposition(|operation| matches!(operation, InitialIconicOperation::Configure { .. }))
            .expect("pre-map parking ConfigureWindow");
        let pre_map_geometry = operations[..map]
            .iter()
            .rposition(|operation| matches!(operation, InitialIconicOperation::Geometry { .. }))
            .expect("pre-map parking geometry readback");
        let post_map_attributes = operations[map + 1..]
            .iter()
            .position(|operation| {
                matches!(
                    operation,
                    InitialIconicOperation::Attributes { viewable: true }
                )
            })
            .map(|position| position + map + 1)
            .expect("post-map attributes readback");
        let post_map_geometry = operations[post_map_attributes + 1..]
            .iter()
            .position(|operation| matches!(operation, InitialIconicOperation::Geometry { .. }))
            .map(|position| position + post_map_attributes + 1)
            .expect("post-map geometry readback");
        let capture = operations
            .iter()
            .position(|operation| *operation == InitialIconicOperation::Capture)
            .expect("compositor capture request");
        let iconify = operations
            .iter()
            .position(|operation| *operation == InitialIconicOperation::Iconify)
            .expect("Iconic admission request");
        assert!(
            pre_map_configure < pre_map_geometry
                && pre_map_geometry < map
                && map < post_map_attributes
                && post_map_attributes < post_map_geometry
                && post_map_geometry < capture
                && capture < iconify,
            "unexpected initial Iconic operation order: {operations:?}"
        );
        for operation in operations.iter().filter(|operation| {
            matches!(
                operation,
                InitialIconicOperation::Configure { .. } | InitialIconicOperation::Geometry { .. }
            )
        }) {
            let (x, width) = match *operation {
                InitialIconicOperation::Configure { x, width }
                | InitialIconicOperation::Geometry { x, width } => (x, width),
                _ => unreachable!(),
            };
            assert!(
                x.saturating_add(i32::try_from(width).unwrap_or(i32::MAX)) <= desktop_left,
                "initial Iconic operation reached the desktop: {operation:?}; {operations:?}"
            );
        }
        let (observed_window, mapped, captured, geometry) = backend.iconify_observations[0];
        assert_eq!(observed_window, window);
        assert!(mapped, "the adopted client must be mapped before admission");
        assert!(captured, "the retained visual must exist before admission");
        assert!(x11_geometry_fully_left_of_desktop(geometry, desktop_left));
        assert!(
            backend
                .window_ops
                .focuses
                .lock()
                .expect("window focuses lock")
                .is_empty(),
            "adopting an Iconic client must not focus it"
        );
        assert!(
            !backend
                .window_ops
                .get_window_attributes(window)
                .unwrap()
                .map_state_viewable,
            "successful admission models true ICCCM IconicState"
        );
    }

    #[test]
    fn initially_iconic_pre_map_parking_failure_never_maps_or_captures() {
        let mut backend = ClientSpyBackend::new();
        backend
            .window_ops
            .ignore_positions
            .store(true, Ordering::Relaxed);
        let window = WindowId::from_raw(0x8402);
        let (mut jwm, geometry, desktop_left) = prepare_initially_iconic_x11(&mut backend);

        let error = jwm
            .manage(&mut backend, window, &geometry)
            .expect_err("an unverified parking ConfigureWindow must stop before MapWindow");

        let client_key = jwm.wintoclient(window).expect("staged Iconic client");
        assert!(jwm.state.clients[client_key].state.is_hidden);
        assert!(error.to_string().contains("remained inside the desktop"));
        assert!(
            backend
                .window_ops
                .maps
                .lock()
                .expect("window maps lock")
                .is_empty()
        );
        assert!(backend.window_ops.force_unmapped.load(Ordering::Relaxed));
        assert!(backend.minimized.is_empty());
        assert!(backend.iconify_requests.is_empty());
        assert!(
            !x11_geometry_fully_left_of_desktop(
                backend.window_ops.get_geometry(window).unwrap(),
                desktop_left
            ),
            "the injected server geometry should explain why mapping was refused"
        );
    }

    #[test]
    fn initially_iconic_post_map_attributes_failure_stays_mapped_and_parked_without_capture() {
        let mut backend = ClientSpyBackend::new();
        let window = WindowId::from_raw(0x8404);
        let (mut jwm, geometry, desktop_left) = prepare_initially_iconic_x11(&mut backend);
        backend
            .window_ops
            .fail_attributes_on_call
            .store(2, Ordering::Relaxed);

        let error = jwm
            .manage(&mut backend, window, &geometry)
            .expect_err("post-map attributes failure must stop before capture");

        assert!(error.to_string().contains("GetWindowAttributes"));
        assert_eq!(
            *backend.window_ops.maps.lock().expect("window maps lock"),
            vec![window]
        );
        assert!(
            backend
                .window_ops
                .get_window_attributes(window)
                .unwrap()
                .map_state_viewable,
            "a post-map verification failure must not unmap the only live fallback"
        );
        assert!(x11_geometry_fully_left_of_desktop(
            backend.window_ops.get_geometry(window).unwrap(),
            desktop_left
        ));
        assert!(backend.minimized.is_empty());
        assert!(backend.iconify_requests.is_empty());
        let operations = backend
            .window_ops
            .initial_iconic_operations
            .lock()
            .expect("initial Iconic operations lock");
        let map = operations
            .iter()
            .position(|operation| *operation == InitialIconicOperation::Map)
            .expect("MapWindow operation");
        let failure = operations
            .iter()
            .position(|operation| *operation == InitialIconicOperation::AttributesFailed)
            .expect("post-map attributes failure");
        assert!(map < failure, "operations: {operations:?}");
        assert!(!operations.contains(&InitialIconicOperation::Capture));
    }

    #[test]
    fn initially_iconic_post_map_geometry_failure_stays_mapped_and_parked_without_capture() {
        let mut backend = ClientSpyBackend::new();
        let window = WindowId::from_raw(0x8405);
        let (mut jwm, geometry, desktop_left) = prepare_initially_iconic_x11(&mut backend);
        backend
            .window_ops
            .fail_geometry_on_call
            .store(2, Ordering::Relaxed);

        let error = jwm
            .manage(&mut backend, window, &geometry)
            .expect_err("post-map geometry failure must stop before capture");

        assert!(error.to_string().contains("GetGeometry"));
        assert_eq!(
            *backend.window_ops.maps.lock().expect("window maps lock"),
            vec![window]
        );
        assert!(
            backend
                .window_ops
                .get_window_attributes(window)
                .unwrap()
                .map_state_viewable
        );
        assert!(x11_geometry_fully_left_of_desktop(
            backend.window_ops.get_geometry(window).unwrap(),
            desktop_left
        ));
        assert!(backend.minimized.is_empty());
        assert!(backend.iconify_requests.is_empty());
        let operations = backend
            .window_ops
            .initial_iconic_operations
            .lock()
            .expect("initial Iconic operations lock");
        let map = operations
            .iter()
            .position(|operation| *operation == InitialIconicOperation::Map)
            .expect("MapWindow operation");
        let attributes = operations[map + 1..]
            .iter()
            .position(|operation| {
                matches!(
                    operation,
                    InitialIconicOperation::Attributes { viewable: true }
                )
            })
            .map(|position| position + map + 1)
            .expect("post-map attributes readback");
        let failure = operations
            .iter()
            .position(|operation| *operation == InitialIconicOperation::GeometryFailed)
            .expect("post-map geometry failure");
        assert!(
            map < attributes && attributes < failure,
            "operations: {operations:?}"
        );
        assert!(!operations.contains(&InitialIconicOperation::Capture));
    }

    #[test]
    fn initially_iconic_already_mapped_verification_failure_remains_parked_without_capture() {
        let mut backend = ClientSpyBackend::new();
        let window = WindowId::from_raw(0x8406);
        let (mut jwm, geometry, desktop_left) = prepare_initially_iconic_x11(&mut backend);
        backend
            .window_ops
            .force_unmapped
            .store(false, Ordering::Relaxed);
        backend
            .window_ops
            .fail_geometry_on_call
            .store(1, Ordering::Relaxed);

        let error = jwm
            .manage(&mut backend, window, &geometry)
            .expect_err("mapped hidden clients must also pass the geometry barrier");

        let client_key = jwm.wintoclient(window).expect("staged Iconic client");
        assert!(jwm.state.clients[client_key].state.is_hidden);
        assert!(error.to_string().contains("GetGeometry"));
        assert!(
            backend
                .window_ops
                .maps
                .lock()
                .expect("window maps lock")
                .is_empty()
        );
        assert!(
            backend
                .window_ops
                .get_window_attributes(window)
                .unwrap()
                .map_state_viewable
        );
        assert!(x11_geometry_fully_left_of_desktop(
            backend.window_ops.get_geometry(window).unwrap(),
            desktop_left
        ));
        assert!(backend.minimized.is_empty());
        assert!(backend.iconify_requests.is_empty());
        let operations = backend
            .window_ops
            .initial_iconic_operations
            .lock()
            .expect("initial Iconic operations lock");
        assert!(!operations.contains(&InitialIconicOperation::Map));
        assert!(operations.contains(&InitialIconicOperation::GeometryFailed));
        assert!(!operations.contains(&InitialIconicOperation::Capture));
    }

    #[test]
    fn initially_iconic_request_failure_stays_hidden_mapped_and_retries() {
        let mut backend = ClientSpyBackend::new();
        backend.fail_next_iconify.store(true, Ordering::Relaxed);
        let window = WindowId::from_raw(0x8403);

        let (mut jwm, client_key, _) = manage_initially_iconic_x11(&mut backend, window);

        assert!(jwm.state.clients[client_key].state.is_hidden);
        assert!(
            backend
                .window_ops
                .get_window_attributes(window)
                .unwrap()
                .map_state_viewable
        );
        assert_eq!(backend.minimized, vec![(window, true)]);
        assert_eq!(backend.iconify_requests, vec![window]);

        assert!(
            !jwm.set_client_minimized(&mut backend, client_key, true)
                .unwrap(),
            "the retry must preserve the existing Dock incarnation"
        );
        assert_eq!(backend.minimized, vec![(window, true)]);
        assert_eq!(backend.iconify_requests, vec![window, window]);
        assert!(
            !backend
                .window_ops
                .get_window_attributes(window)
                .unwrap()
                .map_state_viewable
        );
    }

    fn assert_initially_minimized_ineligible_does_not_create_ghost(
        skip_taskbar: bool,
        dock_type: bool,
        swallowed: bool,
    ) {
        let mut backend = ClientSpyBackend::new();
        backend.supports_client_list = true;
        backend
            .property_ops
            .dock_type
            .store(dock_type, Ordering::Relaxed);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let window = WindowId::from_raw(
            0x8200 + u64::from(skip_taskbar) + 2 * u64::from(dock_type) + 4 * u64::from(swallowed),
        );
        let mut client = WMClient::new(window);
        client.state.is_hidden = true;
        client.state.minimized_order = 23;
        client.state.skip_taskbar = skip_taskbar;
        client.state.is_swallowed = swallowed;
        client.geometry.x = 120;
        client.geometry.old_x = 120;
        client.geometry.y = 80;
        client.geometry.w = 640;
        client.geometry.h = 480;
        let client_key = jwm.insert_client(client);

        jwm.manage_regular_client(&mut backend, client_key, None, false)
            .unwrap();

        assert!(jwm.state.clients[client_key].state.is_hidden);
        assert!(!StatusBarBuilder::is_minimized_dock_eligible(
            &jwm.state.clients[client_key]
        ));
        assert!(backend.minimized.is_empty());
        assert_eq!(backend.dock_targets, vec![(window, None)]);
        assert!(backend.iconify_requests.is_empty());
    }

    #[test]
    fn initially_minimized_ineligible_clients_never_create_compositor_ghosts() {
        assert_initially_minimized_ineligible_does_not_create_ghost(true, false, false);
        assert_initially_minimized_ineligible_does_not_create_ghost(false, true, false);
        assert_initially_minimized_ineligible_does_not_create_ghost(false, false, true);
    }
}
