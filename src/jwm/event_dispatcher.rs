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
use crate::config::{BackendFamily, CONFIG, get_backend_family};
use crate::core::animation::AnimationKind;
use crate::core::controller::WMController;
use crate::jwm::Jwm;
use log::{debug, error, info};
use std::sync::atomic::Ordering;

fn sync_configured_client_geometry(
    wm: &mut Jwm,
    win: WindowId,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) {
    let Some(client_key) = wm.wintoclient(win) else {
        return;
    };

    let width_i = width as i32;
    let height_i = height as i32;

    {
        let Some(client) = wm.state.clients.get_mut(client_key) else {
            return;
        };

        if client.geometry.x == x
            && client.geometry.y == y
            && client.geometry.w == width_i
            && client.geometry.h == height_i
        {
            return;
        }

        info!(
            "[wayland_configure_sync] win={:?} {}x{}+{}+{} -> {}x{}+{}+{}",
            win,
            client.geometry.w,
            client.geometry.h,
            client.geometry.x,
            client.geometry.y,
            width,
            height,
            x,
            y
        );

        client.geometry.x = x;
        client.geometry.y = y;
        client.geometry.w = width_i;
        client.geometry.h = height_i;

        if client.state.is_floating {
            client.geometry.floating_x = x;
            client.geometry.floating_y = y;
            client.geometry.floating_w = width_i;
            client.geometry.floating_h = height_i;
        }
    }

    if wm
        .animations
        .active
        .get(&client_key)
        .is_some_and(|anim| anim.kind == AnimationKind::Appear)
    {
        info!("[wayland_configure_sync] cancel stale appear animation win={win:?}");
        wm.animations.remove(client_key);
    }
}

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
        if get_backend_family() == BackendFamily::Wayland {
            sync_configured_client_geometry(self, win, x, y, width, height);
        }

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
            if self
                .event_coalescer
                .coalesce_geometry(x, y, width, height)
                .is_none()
            {
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
        if self.features.system_ui.is_active() {
            return;
        }
        // Annotation mode: a button press starts a new stroke at the cursor.
        if self.features.annotation_active {
            self.features.annotation_drawing = true;
            if backend.has_compositor() {
                let (rx, ry) = self.last_mouse_root;
                backend.compositor_annotation_begin_stroke();
                backend.compositor_annotation_add_point(rx as f32, ry as f32);
                backend.compositor_force_full_redraw();
            }
            return;
        }

        if let Err(e) = self.on_button_press_internal(backend, target, state, detail, time) {
            error!("Error handling ButtonPress: {:?}", e);
        }
    }

    fn on_button_release(&mut self, backend: &mut dyn Backend, _target: HitTarget, _time: u32) {
        if self.features.system_ui.is_active() {
            return;
        }
        // Annotation mode: a button release lifts the pen (ends the current stroke).
        if self.features.annotation_active && self.features.annotation_drawing {
            self.features.annotation_drawing = false;
            return;
        }

        if self.features.recording.selecting_region {
            self.features.recording.end_region_drag();
            if self.features.recording.adjusting_region {
                if let Some(region) = self
                    .features
                    .recording
                    .region
                    .and_then(Self::recording_region_tuple)
                {
                    backend.compositor_set_recording_region(region);
                }
            }
            self.sync_recording_region_overlay(backend);
            return;
        }

        // Screenshot region selection: on mouse release, commit the selection
        // and wait for the user to choose save action (Enter=file, c=clipboard).
        if self.features.screenshot.active && self.features.screenshot.drawing_annotation {
            self.features.screenshot.commit_annotation();
            if backend.has_compositor() {
                backend.compositor_set_snap_preview(
                    self.features
                        .screenshot
                        .get_selection_rect()
                        .map(|r| (r.x as f32, r.y as f32, r.w as f32, r.h as f32)),
                );
                self.sync_screenshot_annotation_overlay(backend, false);
            }
            return;
        }

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
            self.features
                .screenshot
                .set_tool(crate::jwm::features::screenshot::ScreenshotTool::Pencil);
            self.sync_screenshot_annotation_style(backend);
            backend.compositor_set_annotation_mode(true);
            self.sync_screenshot_annotation_overlay(backend, false);
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
        if self.features.system_ui.is_active() {
            self.last_mouse_root = (root_x, root_y);
            return;
        }
        if self.features.recording.selecting_region {
            self.last_mouse_root = (root_x, root_y);
            backend.compositor_set_mouse_position(root_x as f32, root_y as f32);
            let region = self.features.recording.update_region_drag(
                root_x.round() as i32,
                root_y.round() as i32,
                self.s_w,
                self.s_h,
            );
            if self.features.recording.adjusting_region {
                if let Some(region) = region.and_then(Self::recording_region_tuple) {
                    backend.compositor_set_recording_region(region);
                }
            }
            self.sync_recording_region_overlay(backend);
            return;
        }
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

        if self.features.screenshot.active && self.features.screenshot.drawing_annotation {
            self.last_mouse_root = (root_x, root_y);
            self.features
                .screenshot
                .update_annotation(root_x as f32, root_y as f32);
            if backend.has_compositor() {
                match self.features.screenshot.tool {
                    crate::jwm::features::screenshot::ScreenshotTool::Pencil => {
                        backend.compositor_annotation_add_point(root_x as f32, root_y as f32);
                    }
                    crate::jwm::features::screenshot::ScreenshotTool::Rectangle
                    | crate::jwm::features::screenshot::ScreenshotTool::Ellipse
                    | crate::jwm::features::screenshot::ScreenshotTool::Line
                    | crate::jwm::features::screenshot::ScreenshotTool::Arrow => {
                        backend.compositor_set_snap_preview(
                            self.features
                                .screenshot
                                .get_selection_rect()
                                .map(|r| (r.x as f32, r.y as f32, r.w as f32, r.h as f32)),
                        );
                        self.sync_screenshot_annotation_overlay(backend, true);
                    }
                    crate::jwm::features::screenshot::ScreenshotTool::Select => {}
                }
                backend.compositor_force_full_redraw();
            }
            return;
        }

        // Annotation drawing: while the pen is down, feed points into the current stroke.
        if self.features.annotation_active && self.features.annotation_drawing {
            self.last_mouse_root = (root_x, root_y);
            if backend.has_compositor() {
                backend.compositor_annotation_add_point(root_x as f32, root_y as f32);
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
                    let changed = self.external_struts.get(&win).map(|(s, _)| s) != Some(&strut);
                    let host = self.strut_host_monitor(backend, win);
                    self.external_struts.insert(win, (strut, host));
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
                PropertyKind::MotifHints => self.handle_motif_hints_change(backend, client_key),
                PropertyKind::GtkFrameExtents => {
                    self.handle_gtk_frame_extents_change(backend, client_key)
                }
                PropertyKind::BypassCompositor => {
                    self.handle_bypass_compositor_change(backend, client_key)
                }
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
        if let Some(ck) = self.wintoclient(win) {
            match state {
                NetWmState::Fullscreen => {
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
                NetWmState::DemandsAttention => {
                    if let Some(c) = self.state.clients.get_mut(ck) {
                        let on = match action {
                            NetWmAction::Add => true,
                            NetWmAction::Remove => false,
                            NetWmAction::Toggle => !c.state.demands_attention,
                        };
                        c.state.demands_attention = on;
                        c.state.is_urgent = on;
                        let _ = backend.property_ops().set_net_wm_state_flag(
                            win,
                            NetWmState::DemandsAttention,
                            on,
                        );
                    }
                }
                NetWmState::Above => {
                    if let Some(c) = self.state.clients.get_mut(ck) {
                        let on = match action {
                            NetWmAction::Add => true,
                            NetWmAction::Remove => false,
                            NetWmAction::Toggle => !c.state.is_above,
                        };
                        c.state.is_above = on;
                        if on {
                            c.state.is_below = false;
                            let _ = backend.property_ops().set_net_wm_state_flag(
                                win,
                                NetWmState::Below,
                                false,
                            );
                        }
                        let _ = backend.property_ops().set_net_wm_state_flag(
                            win,
                            NetWmState::Above,
                            on,
                        );
                    }
                }
                NetWmState::Below => {
                    if let Some(c) = self.state.clients.get_mut(ck) {
                        let on = match action {
                            NetWmAction::Add => true,
                            NetWmAction::Remove => false,
                            NetWmAction::Toggle => !c.state.is_below,
                        };
                        c.state.is_below = on;
                        if on {
                            c.state.is_above = false;
                            let _ = backend.property_ops().set_net_wm_state_flag(
                                win,
                                NetWmState::Above,
                                false,
                            );
                        }
                        let _ = backend.property_ops().set_net_wm_state_flag(
                            win,
                            NetWmState::Below,
                            on,
                        );
                    }
                }
                NetWmState::Sticky => {
                    if let Some(c) = self.state.clients.get_mut(ck) {
                        let on = match action {
                            NetWmAction::Add => true,
                            NetWmAction::Remove => false,
                            NetWmAction::Toggle => !c.state.is_sticky,
                        };
                        c.state.is_sticky = on;
                        let _ = backend.property_ops().set_net_wm_state_flag(
                            win,
                            NetWmState::Sticky,
                            on,
                        );
                    }
                }
                NetWmState::SkipTaskbar => {
                    if let Some(c) = self.state.clients.get_mut(ck) {
                        let on = match action {
                            NetWmAction::Add => true,
                            NetWmAction::Remove => false,
                            NetWmAction::Toggle => !c.state.skip_taskbar,
                        };
                        c.state.skip_taskbar = on;
                        let _ = backend.property_ops().set_net_wm_state_flag(
                            win,
                            NetWmState::SkipTaskbar,
                            on,
                        );
                    }
                }
                NetWmState::SkipPager => {
                    if let Some(c) = self.state.clients.get_mut(ck) {
                        let on = match action {
                            NetWmAction::Add => true,
                            NetWmAction::Remove => false,
                            NetWmAction::Toggle => !c.state.skip_pager,
                        };
                        c.state.skip_pager = on;
                        let _ = backend.property_ops().set_net_wm_state_flag(
                            win,
                            NetWmState::SkipPager,
                            on,
                        );
                    }
                }
                NetWmState::Hidden => {
                    if let Some(c) = self.state.clients.get_mut(ck) {
                        let on = match action {
                            NetWmAction::Add => true,
                            NetWmAction::Remove => false,
                            NetWmAction::Toggle => !c.state.is_hidden,
                        };
                        c.state.is_hidden = on;
                        let _ = backend.property_ops().set_net_wm_state_flag(
                            win,
                            NetWmState::Hidden,
                            on,
                        );
                    }
                }
                NetWmState::MaximizedVert | NetWmState::MaximizedHorz => {
                    if let Some(c) = self.state.clients.get_mut(ck) {
                        let is_max = match state {
                            NetWmState::MaximizedVert => c.state.is_maximized_vert,
                            NetWmState::MaximizedHorz => c.state.is_maximized_horz,
                            _ => false,
                        };
                        let on = match action {
                            NetWmAction::Add => true,
                            NetWmAction::Remove => false,
                            NetWmAction::Toggle => !is_max,
                        };
                        match state {
                            NetWmState::MaximizedVert => c.state.is_maximized_vert = on,
                            NetWmState::MaximizedHorz => c.state.is_maximized_horz = on,
                            _ => {}
                        }
                        let _ = backend.property_ops().set_net_wm_state_flag(win, state, on);
                    }
                }
            }
        }
    }

    fn on_wm_keyboard_shortcut(&mut self, backend: &mut dyn Backend, keysym: KeySym, mods: Mods) {
        // Find the first matching binding by immutable borrow, then extract the
        // (Copy) fn pointer and clone only the matched arg. Avoids cloning the
        // whole key_bindings Vec on every keystroke.
        let matched = self
            .key_bindings
            .iter()
            .find(|kc| keysym == kc.key_sym && mods == kc.mask)
            .and_then(|kc| kc.func_opt.map(|func| (func, kc.arg.clone())));
        if let Some((func, arg)) = matched {
            if let Err(e) = func(self, backend, &arg) {
                error!("Error executing keyboard shortcut: {:?}", e);
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
    pub(crate) fn on_moveresize_request(
        &mut self,
        backend: &mut dyn Backend,
        win: WindowId,
        direction: u32,
    ) {
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

#[cfg(test)]
// Kept next to the event-handler implementation it protects; this file also
// contains later inherent helpers used by unrelated event families.
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use crate::backend::api::{
        BackendDiagnostics, Capabilities, ColorAllocator, CompositorAnnotation,
        CompositorBenchmark, CompositorControl, CompositorMedia, CompositorWindowEffects,
        CompositorWorkspaceEffects, CursorProvider, DisplayControl, InputOps, KeyOps, OutputOps,
        PropertyOps, RenderScheduler, WindowOps,
    };
    use crate::backend::wayland_dummy_ops::{
        DummyColorAllocator, DummyCursorProvider, DummyInputOps, DummyKeyOps, DummyOutputOps,
        DummyPropertyOps, DummyWindowOps,
    };
    use crate::core::animation::AnimationManager;
    use crate::core::state::WMState;
    use crate::jwm::features::FeatureStates;
    use shared_structures::SharedMessage;
    use slotmap::SecondaryMap;
    use std::any::Any;
    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::AtomicBool;

    struct RenderSpyBackend {
        window_ops: DummyWindowOps,
        input_ops: DummyInputOps,
        property_ops: DummyPropertyOps,
        output_ops: DummyOutputOps,
        key_ops: DummyKeyOps,
        cursor_provider: DummyCursorProvider,
        color_allocator: DummyColorAllocator,
        rendered_frames: usize,
    }

    impl RenderSpyBackend {
        fn new() -> Self {
            Self {
                window_ops: DummyWindowOps,
                input_ops: DummyInputOps,
                property_ops: DummyPropertyOps,
                output_ops: DummyOutputOps,
                key_ops: DummyKeyOps,
                cursor_provider: DummyCursorProvider,
                color_allocator: DummyColorAllocator,
                rendered_frames: 0,
            }
        }
    }

    impl CompositorBenchmark for RenderSpyBackend {}
    impl BackendDiagnostics for RenderSpyBackend {}
    impl CompositorControl for RenderSpyBackend {}
    impl CompositorMedia for RenderSpyBackend {}
    impl CompositorWorkspaceEffects for RenderSpyBackend {}
    impl CompositorWindowEffects for RenderSpyBackend {}
    impl CompositorAnnotation for RenderSpyBackend {}
    impl DisplayControl for RenderSpyBackend {}

    impl RenderScheduler for RenderSpyBackend {
        fn has_compositor(&self) -> bool {
            true
        }

        fn compositor_needs_render(&self) -> bool {
            true
        }
    }

    impl Backend for RenderSpyBackend {
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
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

        fn run(&mut self, _handler: &mut dyn EventHandler) -> Result<(), BackendError> {
            Ok(())
        }

        fn compositor_render_frame(
            &mut self,
            _scene: &[(u64, i32, i32, u32, u32)],
            _focused_window: Option<u64>,
        ) -> Result<bool, BackendError> {
            self.rendered_frames += 1;
            Ok(true)
        }
    }

    fn empty_jwm() -> Jwm {
        Jwm {
            state: WMState::new(),
            runtime_backend: "test".into(),
            started_at: std::time::Instant::now(),
            s_w: 0,
            s_h: 0,
            running: AtomicBool::new(true),
            is_restarting: AtomicBool::new(false),
            last_mouse_root: (0.0, 0.0),
            message: SharedMessage::default(),
            secondary_bars: HashMap::new(),
            secondary_bar_failures: HashMap::new(),
            secondary_bar_retry_after: HashMap::new(),
            last_key_grab_refresh_at: None,
            pending_bar_updates: HashSet::new(),
            suppress_mouse_focus_until: None,
            suppress_layout_animation: false,
            last_stacking: SecondaryMap::new(),
            scratchpads: HashMap::new(),
            scratchpad_pending_name: None,
            animations: AnimationManager::new(),
            key_bindings: Vec::new(),
            chord_compiled: None,
            chord_armed_until: None,
            do_not_disturb: false,
            debug_hud_on: false,
            external_struts: HashMap::new(),
            ipc_server: None,
            config_reload_tracker: crate::jwm::lifecycle::ConfigReloadTracker::new(None),
            config_last_modified: None,
            config_reload_debounce: None,
            config_reload_count: 0,
            config_reload_last_unix_ms: None,
            config_reload_last_success: None,
            config_reload_last_error: None,
            override_redirect_windows: HashSet::new(),
            or_window_geometries: HashMap::new(),
            scrolling_states: HashMap::new(),
            last_night_light_update: None,
            features: FeatureStates::new(),
            event_coalescer:
                crate::backend::x11::compositor_common::event_coalescer::EventCoalescer::new(),
            pending_pings: HashMap::new(),
            unresponsive_windows: HashSet::new(),
            last_ping_time: None,
            last_user_activity_time: 0,
        }
    }

    #[test]
    fn event_handler_trait_object_delegates_immediate_render_to_jwm() {
        let mut jwm = empty_jwm();
        let mut backend = RenderSpyBackend::new();

        let handler: &mut dyn EventHandler = &mut jwm;
        handler.render_compositor_immediate(&mut backend);

        assert_eq!(backend.rendered_frames, 1);
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
            // Consumed directly by the udev backend loop; a no-op if it reaches here.
            BackendEvent::OutputConfigure { .. } => {}
            BackendEvent::ScreenLayoutChanged => self.on_screen_layout_changed(backend),
            BackendEvent::ChildProcessExited => self.on_child_process_exited(backend),
            BackendEvent::ConfigChanged => {
                self.observe_config_reload(std::time::Instant::now(), "inotify");
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
            } => {
                self.last_user_activity_time = time;
                self.on_button_press(backend, target, state, detail, time);
            }
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
            } => {
                self.last_user_activity_time = time;
                self.on_key_press(backend, keycode, state, time);
            }
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
            BackendEvent::CloseWindowRequest { window } => {
                if let Err(e) = backend.window_ops().close_window(window) {
                    log::warn!("[_NET_CLOSE_WINDOW] close_window failed: {e:?}");
                }
            }

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
            BackendEvent::WorkspaceActivate {
                monitor: _,
                tag_mask,
            } => {
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
            BackendEvent::ForeignToplevelClose(win) => {
                let _ = backend.window_ops().close_window(win);
            }
            BackendEvent::ForeignToplevelSetMaximized(win, maximized) => {
                if let Some(ck) = self.wintoclient(win) {
                    if let Some(c) = self.state.clients.get_mut(ck) {
                        c.state.is_maximized_vert = maximized;
                        c.state.is_maximized_horz = maximized;
                    }
                    let _ = backend.property_ops().set_net_wm_state_flag(
                        win,
                        NetWmState::MaximizedVert,
                        maximized,
                    );
                    let _ = backend.property_ops().set_net_wm_state_flag(
                        win,
                        NetWmState::MaximizedHorz,
                        maximized,
                    );
                }
            }
            BackendEvent::ForeignToplevelSetMinimized(_win, _minimized) => {}
            BackendEvent::ForeignToplevelSetFullscreen(win, fullscreen) => {
                if let Some(ck) = self.wintoclient(win) {
                    let _ = self.setfullscreen(backend, ck, fullscreen);
                }
            }

            BackendEvent::PingResponse { window } => {
                self.handle_ping_response(window);
            }
            BackendEvent::ShapeChanged { window, shaped } => {
                backend.compositor_set_window_shaped(window, shaped);
            }
            BackendEvent::ClientMessage { .. } => {}

            BackendEvent::GestureSwipeAction { fingers, direction } => {
                self.handle_gesture_swipe(backend, fingers, direction);
            }
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
        self.poll_config_reload(backend, now);
        self.flush_pending_bar_updates();
        self.tick_animations(backend);

        // _NET_WM_PING: send pings every 2 seconds, check for timeouts
        self.tick_ping_check(backend, now);

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
        self.animations.has_active()
            || self.features.overview.active
            || self.features.expose_active
            || self.config_reload_deadline_is_due(std::time::Instant::now())
    }

    fn next_wakeup(&self) -> Option<std::time::Duration> {
        Some(self.config_reload_next_wakeup(std::time::Instant::now()))
    }

    fn render_compositor_immediate(&mut self, backend: &mut dyn Backend) {
        self.render_pending_frame(backend);
    }
}

impl Jwm {
    fn tick_ping_check(&mut self, backend: &mut dyn Backend, now: std::time::Instant) {
        const PING_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
        const PING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

        let timed_out: Vec<_> = self
            .pending_pings
            .iter()
            .filter(|(_, sent_at)| now.duration_since(**sent_at) > PING_TIMEOUT)
            .map(|(win, _)| *win)
            .collect();
        for win in timed_out {
            self.pending_pings.remove(&win);
            self.unresponsive_windows.insert(win);
        }

        let should_ping = self
            .last_ping_time
            .map(|t| now.duration_since(t) > PING_INTERVAL)
            .unwrap_or(true);
        if !should_ping {
            return;
        }
        self.last_ping_time = Some(now);

        if let Some(sel) = self.get_selected_client_key() {
            let win = match self.state.clients.get(sel) {
                Some(c) => c.win,
                None => return,
            };
            if !self.pending_pings.contains_key(&win) {
                let ts = now.elapsed().subsec_millis();
                if let Ok(true) = backend.property_ops().send_ping(win, ts) {
                    self.pending_pings.insert(win, now);
                }
            }
        }
    }

    pub(crate) fn handle_ping_response(&mut self, window: WindowId) {
        self.pending_pings.remove(&window);
        self.unresponsive_windows.remove(&window);
    }

    /// Dispatch a touchpad swipe gesture to its configured WM action.
    /// Looks up the (fingers, direction) pair in `behavior.gesture_swipe`
    /// and invokes the matching command via `ipc::dispatch_command`.
    pub(crate) fn handle_gesture_swipe(
        &mut self,
        backend: &mut dyn Backend,
        fingers: u32,
        direction: &str,
    ) {
        let cfg = crate::config::CONFIG.load();
        let bindings = &cfg.behavior().gesture_swipe;
        let entry = match bindings
            .iter()
            .find(|g| g.fingers == fingers && g.direction.eq_ignore_ascii_case(direction))
        {
            Some(e) => e.clone(),
            None => return,
        };
        let arg_value = match &entry.argument {
            crate::config::ArgumentConfig::Int(i) => serde_json::json!(i),
            crate::config::ArgumentConfig::UInt(u) => serde_json::json!(u),
            crate::config::ArgumentConfig::Float(f) => serde_json::json!(f),
            crate::config::ArgumentConfig::String(s) => serde_json::json!(s),
            crate::config::ArgumentConfig::StringVec(v) => serde_json::json!(v),
        };
        match crate::ipc::dispatch_command(&entry.function, &arg_value) {
            Ok((func, arg)) => {
                if let Err(e) = func(self, backend, &arg) {
                    log::warn!(
                        "[gesture] {}-finger {} → {}: {e}",
                        fingers,
                        direction,
                        entry.function
                    );
                }
            }
            Err(e) => log::warn!(
                "[gesture] {}-finger {} → unknown command {}: {e}",
                fingers,
                direction,
                entry.function
            ),
        }
    }
}
