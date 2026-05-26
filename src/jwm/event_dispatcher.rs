//! 事件分发器模块
//!
//! 这个模块包含 WMController 和 EventHandler trait 的实现，
//! 负责分发所有来自 Backend 的事件到对应的处理函数

use crate::backend::api::{
    Backend, BackendEvent, EventHandler, HitTarget, NetWmAction, NetWmState, PropertyKind,
    ResizeEdge, WindowChanges,
};
use crate::backend::common_define::{KeySym, Mods, OutputId, WindowId};
use crate::backend::error::BackendError;
use crate::config::CONFIG;
use crate::core::controller::WMController;
use crate::jwm::Jwm;
use log::{debug, error, info, warn};
use std::sync::atomic::Ordering;

// =================================================================================
// WMController trait 实现 - 事件处理器接口
// =================================================================================
impl WMController for Jwm {
    // === 硬件与输出 ===
    fn on_output_added(
        &mut self,
        backend: &mut dyn Backend,
        info: crate::backend::api::OutputInfo,
    ) {
        if let Err(e) = self.handle_output_added(backend, info) {
            error!("Error handling OutputAdded: {:?}", e);
        }
    }

    fn on_output_removed(&mut self, backend: &mut dyn Backend, id: OutputId) {
        if let Err(e) = self.handle_output_removed(backend, id) {
            error!("Error handling OutputRemoved: {:?}", e);
        }
    }

    fn on_output_changed(
        &mut self,
        backend: &mut dyn Backend,
        info: crate::backend::api::OutputInfo,
    ) {
        if let Err(e) = self.handle_output_changed(backend, info) {
            error!("Error handling OutputChanged: {:?}", e);
        }
    }

    fn on_screen_layout_changed(&mut self, backend: &mut dyn Backend) {
        info!("[WMController] Screen Layout Changed (Hotplug detected), refreshing geometry...");
        if self.updategeom(backend) {
            // Re-apply external strut reservations after geometry reset
            if !self.external_struts.is_empty() {
                self.apply_strut_reservations();
            }
            if let Err(e) = self.handle_screen_geometry_change(backend) {
                error!("Error handling ScreenLayoutChanged: {:?}", e);
            }
        }
    }

    fn on_child_process_exited(&mut self, _backend: &mut dyn Backend) {
        debug!("Received SIGCHLD, reaping zombies...");
        self.reap_zombies();
    }

    // === 窗口生命周期 ===
    fn on_map_request(&mut self, backend: &mut dyn Backend, win: WindowId) {
        if let Err(e) = self.maprequest(backend, win) {
            error!("Error handling MapRequest for {:?}: {:?}", win, e);
        }
    }

    fn on_unmap_notify(&mut self, backend: &mut dyn Backend, win: WindowId, from_configure: bool) {
        self.override_redirect_windows.remove(&win);
        self.or_window_geometries.remove(&win);
        if let Err(e) = self.unmapnotify(backend, win, from_configure) {
            error!("Error handling UnmapNotify for {:?}: {:?}", win, e);
        }
    }

    fn on_destroy_notify(&mut self, backend: &mut dyn Backend, win: WindowId) {
        self.override_redirect_windows.remove(&win);
        self.or_window_geometries.remove(&win);
        if let Err(e) = self.destroynotify(backend, win) {
            error!("Error handling DestroyNotify for {:?}: {:?}", win, e);
        }
    }

    fn on_window_configured(
        &mut self,
        backend: &mut dyn Backend,
        win: WindowId,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) {
        // Keep the OR geometry cache up to date so build_compositor_scene
        // doesn't need a synchronous GetGeometry round-trip per frame.
        if self.override_redirect_windows.contains(&win) {
            // Always update the cache with the latest geometry
            // This prevents flicker from stale geometry during coalescing window
            let new_geom = (x, y, width, height);
            if let Some(&old) = self.or_window_geometries.get(&win) {
                if old != new_geom {
                    info!(
                        "[or_geom_update] win={:?} ({},{} {}x{}) -> ({},{} {}x{})",
                        win, old.0, old.1, old.2, old.3, x, y, width, height
                    );
                }
            }
            self.or_window_geometries.insert(win, new_geom);

            // Use event coalescer to rate-limit downstream processing (configurenotify)
            // but always update cache above to keep compositor in sync
            if self.event_coalescer.coalesce_geometry(x, y, width, height).is_none() {
                // Event was coalesced (rate-limited), skip downstream processing
                return;
            }
        }
        if let Err(e) = self.configurenotify(backend, win, x, y, width, height) {
            error!("Error handling ConfigureNotify: {:?}", e);
        }
    }

    fn on_mapping_notify(&mut self, backend: &mut dyn Backend) {
        backend.key_ops_mut().clear_cache();
        if let Err(e) = self.grabkeys(backend) {
            error!("Error refreshing keys on MappingNotify: {:?}", e);
        }
    }

    // === 输入事件 ===
    fn on_key_press(&mut self, backend: &mut dyn Backend, keycode: u8, mods: u16, _time: u32) {
        let debug_keys = std::env::var("JWM_DEBUG_KEYS")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        if debug_keys {
            let keysym = backend
                .key_ops_mut()
                .keysym_from_keycode(keycode)
                .unwrap_or(0);
            let mods_clean = backend.key_ops().clean_mods(mods);
            info!(
                "[key] keycode={} keysym=0x{:x} mods_raw=0x{:x} mods_clean=0x{:x}",
                keycode,
                keysym,
                mods,
                mods_clean.bits()
            );
        }
        if let Err(e) = self.on_key_press_internal(backend, keycode, mods) {
            error!("Error handling KeyPress: {:?}", e);
        }
    }

    fn on_key_release(&mut self, _backend: &mut dyn Backend, _keycode: u8, _mods: u16, _time: u32) {
    }

    fn on_button_press(
        &mut self,
        backend: &mut dyn Backend,
        target: crate::backend::api::HitTarget,
        state: u16,
        detail: u8,
        time: u32,
    ) {
        if let Err(e) = self.on_button_press_internal(backend, target, state, detail, time) {
            error!("Error handling ButtonPress: {:?}", e);
        }
    }

    fn on_button_release(&mut self, backend: &mut dyn Backend, _target: HitTarget, _time: u32) {
        // Screenshot region selection: on mouse release, commit the selection
        // and wait for the user to choose save action (Enter=file, c=clipboard).
        if self.features.screenshot.active && self.features.screenshot.dragging {
            let (sx, sy) = self.features.screenshot.start;
            let (ex, ey) = self.last_mouse_root;
            let w = (sx - ex).abs();
            let h = (sy - ey).abs();
            if w < 3.0 || h < 3.0 {
                info!("[take_screenshot] selection too small, cancelling");
                self.cancel_screenshot_select(backend);
                return;
            }
            self.features.screenshot.dragging = false;
            self.features.screenshot.committed = true;
            self.features.screenshot.end = self.last_mouse_root;
            // Keep the snap preview visible so the user can see the selection
            return;
        }

        match backend.handle_button_release(0) {
            Ok(handled) => {
                if handled {
                    // Notify compositor of window move end (for wobbly windows effect)
                    if backend.has_compositor() {
                        if let Some(ck) = self.get_selected_client_key() {
                            if let Some(client) = self.state.clients.get(ck) {
                                backend.compositor_notify_window_move_end(client.win);
                            }
                        }
                    }

                    // Snap: if mouse is near a monitor edge, snap the window
                    let (rx, ry) = self.last_mouse_root;
                    let rx = rx as i32;
                    let ry = ry as i32;
                    let snap_dist = CONFIG.load().snap() as i32;
                    if let Some(mk) = self.recttomon(backend, rx, ry) {
                        let (mx, my, mw, mh) = self.monitor_rect(mk);
                        let mw = mw as i32;
                        let mh = mh as i32;
                        let snap_rect = if rx - mx < snap_dist {
                            Some((mx, my, mw / 2, mh))
                        } else if (mx + mw) - rx < snap_dist {
                            Some((mx + mw / 2, my, mw / 2, mh))
                        } else if ry - my < snap_dist {
                            Some((mx, my, mw, mh))
                        } else {
                            None
                        };
                        if let Some((sx, sy, sw, sh)) = snap_rect {
                            if let Some(ck) = self.get_selected_client_key() {
                                let bw = self
                                    .state
                                    .clients
                                    .get(ck)
                                    .map(|c| c.geometry.border_w)
                                    .unwrap_or(0);
                                self.resize_client(
                                    backend,
                                    ck,
                                    sx + bw,
                                    sy + bw,
                                    sw - 2 * bw,
                                    sh - 2 * bw,
                                    false,
                                );
                            }
                        }
                    }

                    // Clear snap preview
                    if backend.has_compositor() {
                        backend.compositor_set_snap_preview(None);
                    }

                    // Sync floating window geometry after drag ends
                    self.sync_focused_floating_geometry(backend);

                    if let Err(e) = self.check_monitor_consistency(backend) {
                        error!(
                            "Error checking monitor consistency after button release: {:?}",
                            e
                        );
                    }
                }
            }
            Err(e) => error!("Error in backend handle_button_release: {:?}", e),
        }
    }

    fn on_motion_notify(
        &mut self,
        backend: &mut dyn Backend,
        target: HitTarget,
        root_x: f64,
        root_y: f64,
        time: u32,
    ) {
        // Screenshot region selection: update overlay rectangle while dragging
        if self.features.screenshot.active && self.features.screenshot.dragging {
            self.last_mouse_root = (root_x, root_y);
            if backend.has_compositor() {
                backend.compositor_set_mouse_position(root_x as f32, root_y as f32);
                let (sx, sy) = self.features.screenshot.start;
                let x = sx.min(root_x) as f32;
                let y = sy.min(root_y) as f32;
                let w = (sx - root_x).abs() as f32;
                let h = (sy - root_y).abs() as f32;
                // Always update preview, even for tiny movements
                backend.compositor_set_snap_preview(Some((x, y, w.max(1.0), h.max(1.0))));
                backend.compositor_force_full_redraw();
            }
            return;
        }

        // Forward mouse position to compositor for effects (magnifier, etc.)
        if backend.has_compositor() {
            // When pointer is on the desktop (no window), clear edge-glow suppression
            // so the glow can activate at screen edges again.
            if matches!(target, HitTarget::Background { .. }) {
                backend.compositor_unsuppress_edge_glow();
            }
            backend.compositor_set_mouse_position(root_x as f32, root_y as f32);
        }

        let win_opt = match target {
            HitTarget::Surface(w) => Some(w),
            HitTarget::Background { .. } => None,
        };
        match backend.handle_motion(root_x, root_y, time) {
            Ok(true) => {
                // Backend is handling a drag — notify compositor of move delta (wobbly windows)
                if backend.has_compositor() {
                    let (prev_x, prev_y) = self.last_mouse_root;
                    let dx = (root_x - prev_x) as f32;
                    let dy = (root_y - prev_y) as f32;
                    if let Some(ck) = self.get_selected_client_key() {
                        if let Some(client) = self.state.clients.get(ck) {
                            backend.compositor_notify_window_move_delta(client.win, dx, dy);
                        }
                    }
                }
                // Sync client geometry so build_compositor_scene uses the live
                // drag position instead of the stale pre-drag geometry.
                // Also force a compositor redraw since the ConfigureNotify from
                // set_position is asynchronous and may not arrive this frame.
                if let Some((win, x, y, w, h)) = backend.interaction_geometry() {
                    if let Some(&ck) = self.state.win_to_client.get(&win) {
                        if let Some(client) = self.state.clients.get_mut(ck) {
                            client.geometry.x = x;
                            client.geometry.y = y;
                            client.geometry.w = w as i32;
                            client.geometry.h = h as i32;
                        }
                    }
                    backend.compositor_force_full_redraw();

                    // Snap preview: detect mouse near monitor edges
                    let snap_dist = CONFIG.load().snap() as i32;
                    let rx = root_x as i32;
                    let ry = root_y as i32;
                    let mon_key = self.recttomon(backend, rx, ry);
                    let preview = if let Some(mk) = mon_key {
                        let (mx, my, mw, mh) = self.monitor_rect(mk);
                        let mw = mw as i32;
                        let mh = mh as i32;
                        if rx - mx < snap_dist {
                            // Left edge → left half
                            Some((mx as f32, my as f32, (mw / 2) as f32, mh as f32))
                        } else if (mx + mw) - rx < snap_dist {
                            // Right edge → right half
                            Some(((mx + mw / 2) as f32, my as f32, (mw / 2) as f32, mh as f32))
                        } else if ry - my < snap_dist {
                            // Top edge → fullscreen
                            Some((mx as f32, my as f32, mw as f32, mh as f32))
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    backend.compositor_set_snap_preview(preview);
                }
                self.last_mouse_root = (root_x, root_y);
                return;
            }
            Ok(false) => {}
            Err(e) => {
                error!("Error in backend handle_motion: {:?}", e);
                return;
            }
        }

        self.last_mouse_root = (root_x, root_y);
        if let Err(e) =
            self.on_motion_notify_internal(backend, win_opt, root_x as i16, root_y as i16, time)
        {
            error!("Error handling MotionNotify: {:?}", e);
        }
    }

    fn on_enter_notify(
        &mut self,
        backend: &mut dyn Backend,
        win: WindowId,
        root_x: f64,
        root_y: f64,
        mode: crate::backend::api::NotifyMode,
    ) {
        if mode != crate::backend::api::NotifyMode::Normal {
            return;
        }
        self.last_mouse_root = (root_x, root_y);

        if backend.has_compositor() {
            backend.compositor_set_mouse_position(root_x as f32, root_y as f32);
            backend.compositor_deactivate_edge_glow();
        }

        if let Err(e) = self.enter_notify(backend, win) {
            error!("Error handling EnterNotify: {:?}", e);
        }
    }

    fn on_leave_notify(&mut self, _backend: &mut dyn Backend, _win: WindowId) {
        // Jwm 目前对 LeaveNotify 没做特殊处理，预留接口
    }

    fn on_focus_in(&mut self, backend: &mut dyn Backend, win: WindowId) {
        if let Err(e) = self.focusin(backend, win) {
            error!("Error handling FocusIn: {:?}", e);
        }
    }

    fn on_focus_out(&mut self, _backend: &mut dyn Backend, _win: WindowId) {
        // Jwm 目前主要处理 FocusIn
    }

    fn on_expose(&mut self, backend: &mut dyn Backend, win: WindowId) {
        if let Err(e) = self.expose(backend, win, 0) {
            error!("Error handling Expose: {:?}", e);
        }
    }

    // === 客户端请求 / 协议 ===
    fn on_configure_request(
        &mut self,
        backend: &mut dyn Backend,
        win: WindowId,
        mask_bits: u16,
        changes: WindowChanges,
    ) {
        if let Err(e) = self.on_configure_request_internal(backend, win, mask_bits, changes) {
            error!("Error handling ConfigureRequest: {:?}", e);
        }
    }

    fn on_property_changed(
        &mut self,
        backend: &mut dyn Backend,
        win: WindowId,
        kind: PropertyKind,
    ) {
        // Handle external strut changes (polybar, trayer, etc.) — works for
        // both managed and unmanaged (override-redirect) windows.
        if kind == PropertyKind::Strut {
            // Skip bar windows managed by jwm (secondary_bars)
            let is_bar_window = self
                .secondary_bars
                .values()
                .any(|bar| bar.window == Some(win));

            if is_bar_window {
                return;
            }

            if let Some(strut) = backend.property_ops().get_window_strut_partial(win) {
                if strut.left > 0 || strut.right > 0 || strut.top > 0 || strut.bottom > 0 {
                    let changed = self.external_struts.get(&win) != Some(&strut);
                    self.external_struts.insert(win, strut);
                    if changed {
                        info!(
                            "[strut] Updated external strut for {:?}: top={} bottom={} left={} right={}",
                            win, strut.top, strut.bottom, strut.left, strut.right
                        );
                        self.apply_strut_reservations();
                        self.arrange(backend, None);
                    }
                } else {
                    // All edges zero — remove
                    if self.external_struts.remove(&win).is_some() {
                        info!("[strut] Removed external strut for {:?}", win);
                        self.apply_strut_reservations();
                        self.arrange(backend, None);
                    }
                }
            } else if self.external_struts.remove(&win).is_some() {
                info!("[strut] Property deleted for {:?}", win);
                self.apply_strut_reservations();
                self.arrange(backend, None);
            }
        }

        if let Some(client_key) = self.wintoclient(win) {
            let res = match kind {
                PropertyKind::TransientFor => self.handle_transient_for_change(backend, client_key),
                PropertyKind::SizeHints => self.handle_normal_hints_change(client_key),
                PropertyKind::Urgency => self.handle_wm_hints_change(backend, client_key),
                PropertyKind::Title => self.handle_title_change(backend, client_key),
                PropertyKind::Class => self.handle_class_change(backend, client_key),
                PropertyKind::WindowType => self.handle_window_type_change(backend, client_key),
                _ => Ok(()),
            };
            if let Err(e) = res {
                error!("Error handling PropertyChanged {:?}: {:?}", kind, e);
            }
        }
    }

    fn on_client_message(&mut self, backend: &mut dyn Backend, win: WindowId) {
        // 对应 _NET_ACTIVE_WINDOW: activate (focus + raise) the requested window.
        if let Some(ck) = self.wintoclient(win) {
            if !self.is_client_selected(ck) {
                // Clear urgent flag if it was set
                if self
                    .state
                    .clients
                    .get(ck)
                    .map(|c| c.state.is_urgent)
                    .unwrap_or(false)
                {
                    let _ = self.seturgent(backend, ck, false);
                }
                if let Err(e) = self.focus(backend, Some(ck)) {
                    error!("Error focusing client on _NET_ACTIVE_WINDOW: {:?}", e);
                }
                if let Err(e) = self.restack(backend, self.state.sel_mon) {
                    error!("Error restacking on _NET_ACTIVE_WINDOW: {:?}", e);
                }
            }
        }
    }

    fn on_window_state_request(
        &mut self,
        backend: &mut dyn Backend,
        win: WindowId,
        action: NetWmAction,
        state: NetWmState,
    ) {
        if matches!(state, NetWmState::Fullscreen) {
            if let Some(ck) = self.wintoclient(win) {
                let is_fullscreen = self
                    .state
                    .clients
                    .get(ck)
                    .map(|c| c.state.is_fullscreen)
                    .unwrap_or(false);
                let fullscreen = match action {
                    NetWmAction::Add => true,
                    NetWmAction::Remove => false,
                    NetWmAction::Toggle => !is_fullscreen,
                };
                if let Err(e) = self.setfullscreen(backend, ck, fullscreen) {
                    error!("Error handling WindowStateRequest: {:?}", e);
                }
            }
        }
    }

    fn on_wm_keyboard_shortcut(&mut self, backend: &mut dyn Backend, keysym: KeySym, mods: Mods) {
        for key_config in self.key_bindings.to_vec().iter() {
            if keysym == key_config.key_sym && mods == key_config.mask {
                if let Some(func) = key_config.func_opt {
                    if let Err(e) = func(self, backend, &key_config.arg) {
                        error!("Error executing keyboard shortcut: {:?}", e);
                    }
                }
                break;
            }
        }
    }
}

// =================================================================================
// _NET_WM_MOVERESIZE 请求处理
// =================================================================================
impl Jwm {
    /// 处理 _NET_WM_MOVERESIZE 客户端消息
    ///
    /// 允许窗口通过协议请求进行移动或调整大小（例如 GTK 应用的窗口边框拖动）
    pub(crate) fn on_moveresize_request(&mut self, backend: &mut dyn Backend, win: WindowId, direction: u32) {
        const _NET_WM_MOVERESIZE_CANCEL: u32 = 11;
        const _NET_WM_MOVERESIZE_MOVE: u32 = 8;

        if direction == _NET_WM_MOVERESIZE_CANCEL {
            let _ = backend.handle_button_release(0);
            return;
        }

        let client_key = match self.wintoclient(win) {
            Some(ck) => ck,
            None => return,
        };

        if direction == _NET_WM_MOVERESIZE_MOVE {
            if let Err(e) = self.enable_floating_keep_geometry(backend, client_key) {
                error!("Error enabling floating for move-resize move: {:?}", e);
                return;
            }
            if let Err(e) = backend.begin_move(win) {
                error!("Error begin_move for _NET_WM_MOVERESIZE: {:?}", e);
            }
            // Notify compositor of window move start (for wobbly windows effect)
            if backend.has_compositor() {
                backend.compositor_notify_window_move_start(win);
            }
            return;
        }

        if direction <= 7 {
            let edge = match direction {
                0 => ResizeEdge::TopLeft,
                1 => ResizeEdge::Top,
                2 => ResizeEdge::TopRight,
                3 => ResizeEdge::Right,
                4 => ResizeEdge::BottomRight,
                5 => ResizeEdge::Bottom,
                6 => ResizeEdge::BottomLeft,
                7 => ResizeEdge::Left,
                _ => unreachable!(),
            };
            if let Err(e) = self.enable_floating_keep_geometry(backend, client_key) {
                error!("Error enabling floating for move-resize resize: {:?}", e);
                return;
            }
            if let Err(e) = backend.begin_resize(win, edge) {
                error!("Error begin_resize for _NET_WM_MOVERESIZE: {:?}", e);
            }
        }
        // direction 9 (SIZE_KEYBOARD) and 10 (MOVE_KEYBOARD) are ignored
    }
}

// =================================================================================
// EventHandler trait 实现 - 事件循环主处理器
// =================================================================================
impl EventHandler for Jwm {
    fn handle_event(
        &mut self,
        backend: &mut dyn Backend,
        event: BackendEvent,
    ) -> Result<(), BackendError> {
        match event {
            // === 硬件与输出 ===
            BackendEvent::OutputAdded(info) => self.on_output_added(backend, info),
            BackendEvent::OutputRemoved(id) => self.on_output_removed(backend, id),
            BackendEvent::OutputChanged(info) => self.on_output_changed(backend, info),
            BackendEvent::ScreenLayoutChanged => self.on_screen_layout_changed(backend),
            BackendEvent::ChildProcessExited => self.on_child_process_exited(backend),
            BackendEvent::ConfigChanged => {
                info!("[config] file change detected via inotify, reloading");
                let resp = self.do_config_reload(backend);
                if resp.success {
                    info!("[config] reload successful");
                } else {
                    warn!("[config] reload failed: {:?}", resp.error);
                }
            }

            // === 窗口生命周期 ===
            BackendEvent::WindowCreated(win) => self.on_map_request(backend, win),
            BackendEvent::WindowDestroyed(win) => self.on_destroy_notify(backend, win),
            BackendEvent::WindowMapped(win) => {
                // Track override-redirect windows so the compositor can render them.
                // BUT filter out the compositor's overlay window to avoid feedback loops.
                let is_overlay = backend.compositor_overlay_window() == Some(win);
                if !is_overlay {
                    if let Ok(attr) = backend.window_ops().get_window_attributes(win) {
                        if attr.override_redirect {
                            self.override_redirect_windows.insert(win);
                            // Cache initial geometry so build_compositor_scene doesn't
                            // need a synchronous GetGeometry round-trip every frame.
                            if let Ok(geom) = backend.window_ops().get_geometry(win) {
                                self.or_window_geometries
                                    .insert(win, (geom.x, geom.y, geom.w, geom.h));
                            }
                        }
                    }
                    // Some X11 notification daemons (e.g. dunst) use override_redirect windows.
                    // Those bypass MapRequest, so they won't be managed/clamped via normal paths.
                    // Clamp them to the monitor workarea here to avoid being covered by the status bar.
                    self.maybe_clamp_override_redirect_notification(backend, win);
                }
            }
            BackendEvent::WindowUnmapped(win) => self.on_unmap_notify(backend, win, false),
            BackendEvent::WindowConfigured {
                window,
                x,
                y,
                width,
                height,
            } => self.on_window_configured(backend, window, x, y, width, height),
            BackendEvent::MappingNotify => self.on_mapping_notify(backend),

            // === 输入事件 ===
            BackendEvent::ButtonPress {
                target,
                state,
                detail,
                time,
                ..
            } => self.on_button_press(backend, target, state, detail, time),
            BackendEvent::ButtonRelease { target, time } => {
                self.on_button_release(backend, target, time)
            }
            BackendEvent::MotionNotify {
                target,
                root_x,
                root_y,
                time,
            } => self.on_motion_notify(backend, target, root_x, root_y, time),
            BackendEvent::KeyPress {
                keycode,
                state,
                time,
            } => self.on_key_press(backend, keycode, state, time),
            BackendEvent::KeyRelease {
                keycode,
                state,
                time,
            } => self.on_key_release(backend, keycode, state, time),
            BackendEvent::EnterNotify {
                window,
                subwindow: _,
                mode,
                root_x,
                root_y,
            } => self.on_enter_notify(backend, window, root_x, root_y, mode),
            BackendEvent::LeaveNotify { window, mode: _ } => self.on_leave_notify(backend, window),
            BackendEvent::FocusIn { window } => self.on_focus_in(backend, window),
            BackendEvent::FocusOut { window } => self.on_focus_out(backend, window),
            BackendEvent::Expose { window } => self.on_expose(backend, window),

            // === 协议与属性 ===
            BackendEvent::ConfigureRequest {
                window,
                mask_bits,
                changes,
            } => self.on_configure_request(backend, window, mask_bits, changes),
            BackendEvent::PropertyChanged { window, kind } => {
                self.on_property_changed(backend, window, kind)
            }
            BackendEvent::WmKeyboardShortcut { keysym, mods } => {
                self.on_wm_keyboard_shortcut(backend, keysym, mods)
            }
            BackendEvent::WindowStateRequest {
                window,
                action,
                state,
            } => self.on_window_state_request(backend, window, action, state),
            BackendEvent::ActiveWindowMessage { window } => self.on_client_message(backend, window),

            BackendEvent::MoveResizeRequest {
                window,
                direction,
                button: _,
            } => self.on_moveresize_request(backend, window, direction),

            // Compositor: damage events are handled at the backend level
            BackendEvent::DamageNotify { .. } => {}

            // Present extension events are handled at the compositor level
            BackendEvent::PresentComplete { .. } => {}
            BackendEvent::PresentIdle { .. } => {}

            // Workspace protocol: client requests tag switch
            BackendEvent::WorkspaceActivate { monitor: _, tag_mask } => {
                use crate::jwm::types::WMArgEnum;
                let _ = self.view(backend, &WMArgEnum::UInt(tag_mask));
            }

            // Output power (DPMS) handled at backend level
            BackendEvent::OutputPowerSet { .. } => {}

            // Gamma LUT handled at backend level (DRM property)
            BackendEvent::GammaSet { .. } => {}

            // Foreign toplevel management actions (taskbar → WM)
            BackendEvent::ForeignToplevelActivate(win) => {
                let _ = self.focusin(backend, win);
            }
            BackendEvent::ForeignToplevelClose(_win) => {
                use crate::jwm::types::WMArgEnum;
                let _ = self.killclient(backend, &WMArgEnum::Int(0));
            }
            BackendEvent::ForeignToplevelSetMaximized(_win, _maximized) => {}
            BackendEvent::ForeignToplevelSetMinimized(_win, _minimized) => {}
            BackendEvent::ForeignToplevelSetFullscreen(_win, _fullscreen) => {}

            // 忽略或不需要显式处理的事件
            BackendEvent::ClientMessage { .. } => { /* ClientMessage Generic */ }
        }

        backend.request_render();
        Ok(())
    }

    fn update(&mut self, backend: &mut dyn Backend) -> Result<(), BackendError> {
        // Ensure all monitor bars are running (sequential creation)
        let now = std::time::Instant::now();
        self.ensure_secondary_bars_running(now);

        self.process_commands_from_status_bar(backend);
        self.process_ipc(backend);
        // Config reload is now handled by inotify (ConfigChanged event)
        // self.check_config_reload(backend);
        self.flush_pending_bar_updates();
        self.tick_animations(backend);

        // Poll pointer position when magnifier is active.  X11 MotionNotify
        // events are only delivered to the deepest window that selects
        // PointerMotion, so when the pointer is over a client's internal
        // subwindow the WM misses the events and the magnifier gets stuck.
        // Polling via QueryPointer on the root window always succeeds.
        if self.features.magnifier.enabled && backend.has_compositor() {
            if let Ok((x, y)) = backend.input_ops().get_pointer_position() {
                backend.compositor_set_mouse_position(x as f32, y as f32);
            }
        }

        backend.window_ops().flush()?;
        Ok(())
    }

    fn should_exit(&self) -> bool {
        // 检查原子布尔值
        !self.running.load(Ordering::SeqCst)
    }

    fn needs_tick(&self) -> bool {
        self.animations.has_active() || self.features.overview.active || self.features.expose_active
    }
}
