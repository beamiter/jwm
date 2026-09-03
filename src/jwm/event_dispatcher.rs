//! 事件分发器模块
//!
//! 这个模块包含 WMController 和 EventHandler trait 的实现，
//! 负责分发所有来自 Backend 的事件到对应的处理函数

use crate::backend::api::{
    Backend, BackendEvent, EventHandler, HitTarget, InteractionAction, NetWmAction, NetWmState,
    PropertyKind, ResizeEdge, WindowChanges,
};
use crate::backend::common_define::{KeySym, Mods, OutputId, WindowId};
use crate::backend::error::BackendError;
use crate::config::{BackendFamily, CONFIG, ClientMoveResize, get_backend_family};
use crate::core::animation::AnimationKind;
use crate::core::controller::WMController;
use crate::core::models::ClientKey;
use crate::jwm::Jwm;
use crate::jwm::features::CaptureTarget;
use crate::jwm::mouse_handler::DragMode;
use crate::jwm::statusbar::StatusBarBuilder;
use crate::jwm::types::WMArgEnum;
use log::{debug, error, info};
use std::sync::atomic::Ordering;

/// Wakeup pacing for the panels that animate on their own, roughly one frame
/// at 60 Hz.
const FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);
const PING_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
const PING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn min_optional_duration(
    left: Option<std::time::Duration>,
    right: Option<std::time::Duration>,
) -> Option<std::time::Duration> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn ping_schedule_next_wakeup(
    last_ping: Option<std::time::Instant>,
    has_target: bool,
    pending: impl IntoIterator<Item = std::time::Instant>,
    now: std::time::Instant,
) -> Option<std::time::Duration> {
    let send = has_target.then(|| {
        last_ping.map_or(std::time::Duration::ZERO, |last| {
            PING_INTERVAL.saturating_sub(now.saturating_duration_since(last))
        })
    });
    let timeout = pending
        .into_iter()
        .map(|sent| PING_TIMEOUT.saturating_sub(now.saturating_duration_since(sent)))
        .min();
    min_optional_duration(send, timeout)
}

fn requested_hidden_state(action: NetWmAction, currently_hidden: bool) -> bool {
    match action {
        NetWmAction::Add => true,
        NetWmAction::Remove => false,
        NetWmAction::Toggle => !currently_hidden,
    }
}

/// Apply a taskbar/pager request without letting a de-minimized client remain
/// parked on an inactive tag. A repeated "not minimized" request for an
/// already-visible client is deliberately only a protocol repair; it must not
/// steal focus.
fn apply_external_minimized_request(
    wm: &mut Jwm,
    backend: &mut dyn Backend,
    client_key: ClientKey,
    win: WindowId,
    minimized: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let was_hidden = wm
        .state
        .clients
        .get(client_key)
        .is_some_and(|client| client.state.is_hidden);
    if !minimized && was_hidden {
        let _revealed = wm.reveal_and_focus(backend, win)?;
    } else {
        let _changed = wm.set_client_minimized(backend, client_key, minimized)?;
    }
    Ok(())
}

fn requested_attention_state(action: NetWmAction, currently_requested: bool) -> bool {
    match action {
        NetWmAction::Add => true,
        NetWmAction::Remove => false,
        NetWmAction::Toggle => !currently_requested,
    }
}

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

    let width_i = i32::try_from(width).unwrap_or(i32::MAX);
    let height_i = i32::try_from(height).unwrap_or(i32::MAX);

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
        self.reconcile_external_struts_after_topology_change(backend);
    }

    fn on_output_removed(&mut self, backend: &mut dyn Backend, id: OutputId) {
        if let Err(e) = self.handle_output_removed(backend, id) {
            error!("Error handling OutputRemoved: {:?}", e);
        }
        self.reconcile_external_struts_after_topology_change(backend);
    }

    fn on_output_changed(
        &mut self,
        backend: &mut dyn Backend,
        info: crate::backend::api::OutputInfo,
    ) {
        if let Err(e) = self.handle_output_changed(backend, info) {
            error!("Error handling OutputChanged: {:?}", e);
        }
        self.reconcile_external_struts_after_topology_change(backend);
    }

    fn on_screen_layout_changed(&mut self, backend: &mut dyn Backend) {
        info!("[WMController] Screen Layout Changed (Hotplug detected), refreshing geometry...");
        if self.updategeom(backend) {
            self.reconcile_external_struts_after_topology_change(backend);
            if let Err(e) = self.handle_screen_geometry_change(backend) {
                error!("Error handling ScreenLayoutChanged: {:?}", e);
            }
        }
    }

    fn on_child_process_exited(&mut self, backend: &mut dyn Backend) {
        debug!("Received SIGCHLD, polling JWM-owned children...");
        // SIGCHLD is only a latency optimization on backends that expose it.
        // Bypass the one-second backend-neutral insurance poll so failed
        // scratchpad launches release their pending-name gate immediately.
        self.reap_transient_children_immediately();
        self.poll_secondary_bar_children(backend, std::time::Instant::now(), true);
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

        // A panel can cross outputs without changing its strut property. Do
        // this before OR-event coalescing so every physical host transition
        // updates legacy whole-screen strut attribution.
        if self.refresh_external_strut_host_from_geometry(backend, win, x, y, width, height) {
            info!("[strut] Rehosted external strut for {win:?} after ConfigureNotify");
            self.apply_strut_reservations();
            self.arrange(backend, None);
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
        let root_configured = backend.root_window() == Some(win);
        if let Err(e) = self.configurenotify(backend, win, x, y, width, height) {
            error!("Error handling ConfigureNotify: {:?}", e);
        }
        if root_configured {
            self.reconcile_external_struts_after_topology_change(backend);
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

    fn on_key_release(&mut self, backend: &mut dyn Backend, keycode: u8, _mods: u16, _time: u32) {
        // Only the window switcher listens to releases: letting go of the
        // modifier the gesture started with commits the highlighted row.
        if !self.features.system_ui.is_window_switcher() {
            return;
        }
        let Ok(keysym) = backend.key_ops_mut().keysym_from_keycode(keycode) else {
            return;
        };
        let commits = crate::jwm::features::switcher::modifier_of_keysym(keysym)
            .is_some_and(|modifier| self.features.window_switcher_mods.contains(modifier));
        if commits {
            if let Err(e) = self.commit_window_switcher(backend) {
                error!(
                    "Error committing window switcher on modifier release: {:?}",
                    e
                );
            }
        }
    }

    fn on_button_press(
        &mut self,
        backend: &mut dyn Backend,
        target: crate::backend::api::HitTarget,
        state: u16,
        detail: u8,
        time: u32,
    ) {
        // The Alt+Tab switcher holds no pointer grab, so only some clicks
        // ever reach here — the desktop, the bar, Alt+click on a client.
        // Whichever it is, the gesture ends: a row picks that window, any
        // other press cancels.
        if self.features.system_ui.is_window_switcher() {
            use crate::backend::api::SystemUiHitTarget;
            if detail == 1 {
                let (x, y) = backend
                    .input_ops()
                    .get_pointer_position()
                    .unwrap_or(self.last_mouse_root);
                if let SystemUiHitTarget::Item(row) = backend.compositor_system_ui_hit_test(x, y) {
                    if self.features.system_ui.select_visible_row(row).is_some() {
                        if let Err(e) = self.commit_window_switcher(backend) {
                            error!("Error committing window switcher from pointer: {:?}", e);
                        }
                        return;
                    }
                }
            }
            self.cancel_window_switcher(backend);
            return;
        }
        if self.features.system_ui.is_layout_picker() {
            let (x, y) = backend
                .input_ops()
                .get_pointer_position()
                .unwrap_or(self.last_mouse_root);
            match detail {
                // Wheel: browse the strip without committing.
                4 => {
                    let _ = self.layout_picker(backend, &WMArgEnum::Int(-1));
                }
                5 => {
                    let _ = self.layout_picker(backend, &WMArgEnum::Int(1));
                }
                _ => self.click_layout_picker(backend, x, y),
            }
            return;
        }
        if self.features.system_ui.is_active() {
            let (x, y) = backend
                .input_ops()
                .get_pointer_position()
                .unwrap_or(self.last_mouse_root);
            let hit = backend.compositor_system_ui_hit_test(x, y);
            use crate::backend::api::SystemUiHitTarget;
            match detail {
                // Wheel anywhere on the card browses the current page. The
                // scrim stays inert so an accidental scroll never changes a
                // modal selection the pointer is not near.
                4 if !matches!(
                    hit,
                    SystemUiHitTarget::Outside | SystemUiHitTarget::Unavailable
                ) =>
                {
                    self.scroll_system_ui_from_pointer(backend, -1);
                }
                5 if !matches!(
                    hit,
                    SystemUiHitTarget::Outside | SystemUiHitTarget::Unavailable
                ) =>
                {
                    self.scroll_system_ui_from_pointer(backend, 1);
                }
                1 => match hit {
                    SystemUiHitTarget::Item(row) => {
                        if let Err(error) = self.activate_system_ui_pointer_row(backend, row) {
                            error!("Error activating system UI row: {error}");
                        }
                    }
                    SystemUiHitTarget::Outside => {
                        self.dismiss_system_ui_from_pointer(backend);
                    }
                    SystemUiHitTarget::Panel | SystemUiHitTarget::Unavailable => {}
                },
                _ => {}
            }
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
        if self.features.capture.take_swallowed_button_release() {
            return;
        }
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
                // A finished mark is what makes undo available, so the strip
                // has to be rebuilt before the next click can reach it.
                self.sync_screenshot_toolbar(backend);
            }
            return;
        }

        if self.features.screenshot.active && self.features.screenshot.dragging {
            self.features
                .screenshot
                .update_drag(self.last_mouse_root.0, self.last_mouse_root.1);
            let Some(rect) = self.features.screenshot.get_selection_rect() else {
                info!("[take_screenshot] selection too small, cancelling");
                self.cancel_screenshot_select(backend);
                return;
            };
            if rect.w < 3 || rect.h < 3 {
                info!("[take_screenshot] selection too small, cancelling");
                self.cancel_screenshot_select(backend);
                return;
            }
            self.features.screenshot.commit();
            self.features
                .screenshot
                .set_tool(crate::jwm::features::screenshot::ScreenshotTool::Pencil);
            self.sync_screenshot_annotation_style(backend);
            backend.compositor_set_annotation_mode(true);
            self.sync_screenshot_annotation_overlay(backend, false);
            // The selection is now an editor, so it gets its tools.
            self.sync_screenshot_toolbar(backend);
            // Keep the snap preview visible so the user can see the selection
            return;
        }

        // Modal capture clicks must never leak through to the selected client.
        if self.features.screenshot.active {
            return;
        }

        // Window-tab reorder drag: its press armed neither a backend
        // interaction nor `drag_ctl`, so the commit paths below have nothing
        // to do for it. Commit an activated drag, discard a dormant one (a
        // plain click), and clear the state either way.
        if let Some(drag) = self.tab_drag.take() {
            if drag.activated {
                let (rx, ry) = self.last_mouse_root;
                if let Err(e) = self.commit_window_tab_reorder(backend, drag, rx, ry) {
                    error!("Error committing window tab reorder: {:?}", e);
                }
            }
            return;
        }

        // Query before handle_button_release: the backends drop their
        // interaction state inside that call.
        let interaction_action = backend.interaction_action();
        let ctl = self.drag_ctl.take();
        match backend.handle_button_release(0) {
            Ok(handled) => {
                if handled {
                    match ctl {
                        // Below the drag threshold the press was a plain
                        // click: the window was never floated, moved or
                        // resized, so there is nothing to commit or undo.
                        Some(ctl) if !ctl.activated => {
                            debug!(
                                "Pointer drag on {:?} released below threshold; treating as click",
                                ctl.win
                            );
                            if backend.has_compositor() {
                                backend.compositor_set_snap_preview(None);
                            }
                        }
                        Some(ctl) => {
                            // Notify compositor of window move end (for wobbly windows effect)
                            if matches!(ctl.mode, DragMode::MoveFloat) && backend.has_compositor() {
                                backend.compositor_notify_window_move_end(ctl.win);
                            }

                            let (rx, ry) = self.last_mouse_root;
                            let rx = rx as i32;
                            let ry = ry as i32;
                            match ctl.mode {
                                // Snap: releasing a move-drag near a monitor edge
                                // re-attaches the window into the current layout at
                                // the slot under the pointer; design floats and the
                                // float layout keep the classic floating half-screen
                                // snap.
                                DragMode::MoveFloat => {
                                    if let Some(mk) = self.recttomon(backend, rx, ry) {
                                        if let Some(plan) = self.plan_drag_snap(mk, rx, ry) {
                                            if let Some(ck) = self.get_selected_client_key() {
                                                self.apply_drag_snap(backend, ck, plan);
                                            }
                                        }
                                    }
                                }
                                // Reorder: the window stayed tiled the whole drag;
                                // the drop commits the layout slot under the pointer.
                                DragMode::Reorder => {
                                    if let Some(mk) = self.recttomon(backend, rx, ry) {
                                        if let Some(plan) =
                                            self.plan_drag_reorder(ctl.client, mk, rx, ry)
                                        {
                                            self.apply_drag_snap(backend, ctl.client, plan);
                                        }
                                    }
                                }
                                // Resize drags never snap.
                                DragMode::Resize(_) => {}
                            }

                            // Clear snap preview
                            if backend.has_compositor() {
                                backend.compositor_set_snap_preview(None);
                            }

                            if !matches!(ctl.mode, DragMode::Reorder) {
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
                        // Legacy path: a backend interaction without a drag
                        // controller (backend without track support, e.g. a
                        // fallback begin_move started elsewhere).
                        None => {
                            // Notify compositor of window move end (for wobbly windows effect)
                            if backend.has_compositor() {
                                if let Some(ck) = self.get_selected_client_key() {
                                    if let Some(client) = self.state.clients.get(ck) {
                                        backend.compositor_notify_window_move_end(client.win);
                                    }
                                }
                            }

                            // Snap: releasing a move-drag near a monitor edge re-attaches
                            // the window into the current layout at the slot under the
                            // pointer; design floats and the float layout keep the classic
                            // floating half-screen snap. Resize drags never snap.
                            let is_resize =
                                matches!(interaction_action, Some(InteractionAction::Resize(_)));
                            if !is_resize {
                                let (rx, ry) = self.last_mouse_root;
                                let rx = rx as i32;
                                let ry = ry as i32;
                                if let Some(mk) = self.recttomon(backend, rx, ry) {
                                    if let Some(plan) = self.plan_drag_snap(mk, rx, ry) {
                                        if let Some(ck) = self.get_selected_client_key() {
                                            self.apply_drag_snap(backend, ck, plan);
                                        }
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
            // The film strip follows the pointer, so the cell under it is the
            // one a click would take.
            if self.features.system_ui.is_layout_picker() {
                self.hover_layout_picker(backend, root_x, root_y);
            } else {
                let row = match backend.compositor_system_ui_hit_test(root_x, root_y) {
                    crate::backend::api::SystemUiHitTarget::Item(row) => Some(row),
                    _ => None,
                };
                self.hover_system_ui_pointer_row(backend, row);
            }
            return;
        }
        if self.features.recording.selecting_region {
            self.last_mouse_root = (root_x, root_y);
            backend.compositor_set_mouse_position(root_x as f32, root_y as f32);
            if self.features.capture.recording == CaptureTarget::Region {
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
            } else {
                self.preview_recording_capture_target(backend, target, (root_x, root_y));
            }
            return;
        }
        if self.features.screenshot.active
            && !self.features.screenshot.committed
            && !self.features.screenshot.dragging
            && self.features.capture.screenshot != CaptureTarget::Region
        {
            self.last_mouse_root = (root_x, root_y);
            backend.compositor_set_mouse_position(root_x as f32, root_y as f32);
            self.preview_screenshot_capture_target(backend, target, (root_x, root_y));
            return;
        }

        // Screenshot region selection: update overlay rectangle while dragging
        if self.features.screenshot.active && self.features.screenshot.dragging {
            self.last_mouse_root = (root_x, root_y);
            self.features.screenshot.update_drag(root_x, root_y);
            if backend.has_compositor() {
                backend.compositor_set_mouse_position(root_x as f32, root_y as f32);
                let preview = self.features.screenshot.get_selection_rect().map(|rect| {
                    (
                        rect.x as f32,
                        rect.y as f32,
                        rect.w.max(1) as f32,
                        rect.h.max(1) as f32,
                    )
                });
                backend.compositor_set_snap_preview(preview);
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
                use crate::jwm::features::screenshot::ScreenshotTool;
                match self.features.screenshot.tool {
                    // Freehand strokes grow a point at a time — rebuilding the
                    // whole overlay per motion event would re-upload every
                    // finished mark on every pixel of the drag.
                    ScreenshotTool::Pencil | ScreenshotTool::Marker => {
                        backend.compositor_annotation_add_point(root_x as f32, root_y as f32);
                    }
                    // Everything else is defined by its two endpoints, so the
                    // in-flight shape has to be redrawn from scratch.
                    ScreenshotTool::Rectangle
                    | ScreenshotTool::FilledRectangle
                    | ScreenshotTool::Ellipse
                    | ScreenshotTool::Line
                    | ScreenshotTool::Arrow
                    | ScreenshotTool::Pixelate
                    | ScreenshotTool::Invert => {
                        backend.compositor_set_snap_preview(
                            self.features
                                .screenshot
                                .get_selection_rect()
                                .map(|r| (r.x as f32, r.y as f32, r.w as f32, r.h as f32)),
                        );
                        self.sync_screenshot_annotation_overlay(backend, true);
                    }
                    // Click-placed tools and the selection mode draw nothing
                    // while the pointer moves.
                    ScreenshotTool::Select | ScreenshotTool::Text | ScreenshotTool::Counter => {}
                }
                backend.compositor_force_full_redraw();
            }
            return;
        }

        // A finished selection with nothing being drawn is the state the
        // toolbar lives in, so this is where its hover highlight is tracked.
        // Without this branch the pointer would fall through to the generic
        // motion path and the strip would never light up.
        if self.features.screenshot.active && self.features.screenshot.committed {
            self.last_mouse_root = (root_x, root_y);
            let hovered = self.screenshot_toolbar_hit(root_x, root_y);
            if self.features.screenshot.hovered_button != hovered {
                self.features.screenshot.hovered_button = hovered;
                self.sync_screenshot_toolbar(backend);
            }
            if self.screenshot_toolbar_contains(root_x, root_y) {
                backend.compositor_set_mouse_position(root_x as f32, root_y as f32);
                return;
            }
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
                // Deferred drags: below the drag threshold nothing engages —
                // a click-and-hold without movement stays a plain click and
                // the window is left untouched.
                if let Some((sx, sy, activated)) = self
                    .drag_ctl
                    .as_ref()
                    .map(|c| (c.start_root.0, c.start_root.1, c.activated))
                {
                    if !activated {
                        let thr = CONFIG.load().drag_threshold_px() as f64;
                        let (dx, dy) = (root_x - sx, root_y - sy);
                        if dx * dx + dy * dy < thr * thr {
                            self.last_mouse_root = (root_x, root_y);
                            return;
                        }
                        if let Err(e) = self.activate_pointer_drag(backend) {
                            error!("Error activating pointer drag: {:?}", e);
                        }
                    }
                }
                let reorder_drag = matches!(
                    self.drag_ctl.as_ref().map(|c| c.mode),
                    Some(DragMode::Reorder)
                );
                // Backend is handling a drag — notify compositor of move delta
                // (wobbly windows). A reorder drag never moves the window.
                if backend.has_compositor() && !reorder_drag {
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

                    // Snap preview: show where a drop would land — the layout
                    // slot the window would re-attach to (or reorder into),
                    // or the floating half.
                    let rx = root_x as i32;
                    let ry = root_y as i32;
                    let preview = match self.drag_ctl.as_ref().map(|c| (c.mode, c.client)) {
                        Some((DragMode::Resize(_), _)) => None,
                        Some((DragMode::Reorder, drag_key)) => self
                            .recttomon(backend, rx, ry)
                            .and_then(|mk| self.plan_drag_reorder(drag_key, mk, rx, ry))
                            .map(|plan| {
                                let r = plan.preview_rect();
                                (r.x as f32, r.y as f32, r.w as f32, r.h as f32)
                            }),
                        Some((DragMode::MoveFloat, _)) | None => {
                            let is_resize = matches!(
                                backend.interaction_action(),
                                Some(InteractionAction::Resize(_))
                            );
                            if is_resize {
                                None
                            } else {
                                self.recttomon(backend, rx, ry)
                                    .and_then(|mk| self.plan_drag_snap(mk, rx, ry))
                                    .map(|plan| {
                                        let r = plan.preview_rect();
                                        (r.x as f32, r.y as f32, r.w as f32, r.h as f32)
                                    })
                            }
                        }
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
                    let host = self.strut_host_monitor(backend, win);
                    let changed = self.cache_external_strut(win, strut, host);
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
                // The X11 backend consumes this hint before dispatching the
                // event to JWM, so no second synchronous property query is
                // needed here.
                PropertyKind::BypassCompositor => Ok(()),
                _ => Ok(()),
            };
            if let Err(e) = res {
                error!("Error handling PropertyChanged {:?}: {:?}", kind, e);
            }
        }
    }

    fn on_client_message(&mut self, backend: &mut dyn Backend, win: WindowId) {
        // 对应 _NET_ACTIVE_WINDOW: activate (focus + raise) the requested window.
        // `focus` deliberately falls back to another client when its target is
        // hidden, on another tag, or on another monitor.  Activation means the
        // opposite: reveal this exact window, then focus and raise it.
        if let Err(error) = self.reveal_and_focus(backend, win) {
            error!("Error activating client on _NET_ACTIVE_WINDOW: {error:?}");
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
                    let requested = if let Some(c) = self.state.clients.get_mut(ck) {
                        let on = requested_attention_state(action, c.state.demands_attention);
                        c.state.demands_attention = on;
                        c.state.is_urgent = on;
                        Some((on, c.mon))
                    } else {
                        None
                    };
                    if let Some((on, monitor)) = requested {
                        let _ = backend.property_ops().set_net_wm_state_flag(
                            win,
                            NetWmState::DemandsAttention,
                            on,
                        );
                        if backend.has_compositor() {
                            backend.compositor_set_window_urgent(win, on);
                        } else {
                            let focused = self.get_selected_client_key() == Some(ck);
                            if let Err(error) = self.update_client_decoration(backend, ck, focused)
                            {
                                log::warn!(
                                    "could not update native urgent border for {win:?}: {error}"
                                );
                            }
                        }
                        let monitor_num = monitor
                            .and_then(|key| self.state.monitors.get(key))
                            .map(|monitor| monitor.num);
                        self.mark_bar_update_needed_if_visible(monitor_num);
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
                    let mut was_dock_eligible = None;
                    let monitor = if let Some(c) = self.state.clients.get_mut(ck) {
                        was_dock_eligible = Some(StatusBarBuilder::is_minimized_dock_eligible(c));
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
                        c.mon
                    } else {
                        None
                    };
                    let monitor_num = monitor
                        .and_then(|key| self.state.monitors.get(key))
                        .map(|monitor| monitor.num);
                    if let Some(was_dock_eligible) = was_dock_eligible {
                        self.reconcile_minimized_dock_eligibility(backend, ck, was_dock_eligible);
                    }
                    self.mark_bar_update_needed_if_visible(monitor_num);
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
                    let was_hidden = self
                        .state
                        .clients
                        .get(ck)
                        .map(|client| client.state.is_hidden)
                        .unwrap_or(false);
                    let on = requested_hidden_state(action, was_hidden);
                    if let Err(error) = apply_external_minimized_request(self, backend, ck, win, on)
                    {
                        error!("Could not apply minimized state for {win:?}: {error}");
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
    /// 允许窗口通过协议请求进行移动或调整大小（例如 GTK 应用的窗口边框拖动）。
    /// `behavior.client_moveresize` 决定哪些窗口可以响应：默认只有已浮动的
    /// 窗口，平铺窗口的布局槽位不会被客户端的拖拽区域打乱；"always" 时平铺
    /// 窗口的拖动变为布局内重排（见 [`DragMode::Reorder`]）。
    pub(crate) fn on_moveresize_request(
        &mut self,
        backend: &mut dyn Backend,
        win: WindowId,
        direction: u32,
    ) {
        const _NET_WM_MOVERESIZE_CANCEL: u32 = 11;
        const _NET_WM_MOVERESIZE_MOVE: u32 = 8;

        if direction == _NET_WM_MOVERESIZE_CANCEL {
            self.cancel_pointer_drag(backend);
            return;
        }

        let client_key = match self.wintoclient(win) {
            Some(ck) => ck,
            None => return,
        };

        let policy = CONFIG.load().client_moveresize();
        if policy == ClientMoveResize::Never {
            return;
        }

        let (is_floating, is_fullscreen, mon) = match self.state.clients.get(client_key) {
            Some(c) => (c.state.is_floating, c.state.is_fullscreen, c.mon),
            None => return,
        };
        if is_fullscreen {
            return;
        }
        // Default policy: a tiled window's layout slot cannot be disturbed
        // by a client-side drag region (CSD title bar, invisible resize
        // border).
        if !is_floating && policy == ClientMoveResize::FloatingOnly {
            return;
        }

        // The drag helpers (snap planning, geometry sync) work off the
        // selected client, so make sure the dragged window is it.
        if self.get_selected_client_key() != Some(client_key) {
            let _ = self.focus(backend, Some(client_key));
        }

        if direction == _NET_WM_MOVERESIZE_MOVE {
            // Dragging a tiled window by its own surface reorders it within
            // a tiling layout instead of popping it out; under a non-tiling
            // layout it moves as a float like before.
            let tiling_layout = mon
                .and_then(|mk| self.state.monitors.get(mk))
                .map(|m| m.lt.is_tile())
                .unwrap_or(false);
            let mode = if !is_floating && tiling_layout {
                DragMode::Reorder
            } else {
                DragMode::MoveFloat
            };
            if let Err(e) = self.start_pointer_drag(backend, client_key, mode) {
                error!("Error starting drag for _NET_WM_MOVERESIZE: {:?}", e);
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
            if let Err(e) = self.start_pointer_drag(backend, client_key, DragMode::Resize(edge)) {
                error!("Error starting resize drag for _NET_WM_MOVERESIZE: {:?}", e);
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
    use crate::config::CONFIG;

    use crate::backend::api::{
        BackendDiagnostics, Capabilities, CloseResult, ColorAllocator, CompositorAnnotation,
        CompositorBenchmark, CompositorControl, CompositorMedia, CompositorRect,
        CompositorWindowEffects, CompositorWorkspaceEffects, CursorProvider, DisplayControl,
        Geometry, InputOps, KeyOps, ManagedUnmapReason, NetWmState, NormalHints, OutputInfo,
        OutputOps, PropertyOps, RenderScheduler, SystemUiHitTarget, WindowAttributes,
        WindowChanges, WindowOps, WindowType, WmHints,
    };
    use crate::backend::common_define::Pixel;
    use crate::backend::wayland_dummy_ops::{
        DummyColorAllocator, DummyCursorProvider, DummyOutputOps,
    };
    use crate::core::animation::AnimationManager;
    use crate::core::models::ClientKey;
    use crate::core::state::WMState;
    use crate::jwm::features::FeatureStates;
    use crate::jwm::types::WMArgEnum;
    use slotmap::SecondaryMap;
    use std::any::Any;
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering as AtomicOrdering};
    use xbar_core::shared_structures::SharedMessage;

    struct MapRestorePropertyOps {
        ewmh_hidden: AtomicBool,
        wm_state: AtomicI64,
    }

    impl MapRestorePropertyOps {
        fn new() -> Self {
            Self {
                ewmh_hidden: AtomicBool::new(false),
                wm_state: AtomicI64::new(i64::from(crate::jwm::types::NORMAL_STATE)),
            }
        }
    }

    impl PropertyOps for MapRestorePropertyOps {
        fn get_title(&self, _win: WindowId) -> String {
            "Test Window".into()
        }

        fn get_class(&self, _win: WindowId) -> (String, String) {
            ("test".into(), "Test".into())
        }

        fn get_window_types(&self, _win: WindowId) -> Vec<WindowType> {
            vec![WindowType::Normal]
        }

        fn is_fullscreen(&self, _win: WindowId) -> bool {
            false
        }

        fn set_fullscreen_state(&self, _win: WindowId, _on: bool) -> Result<(), BackendError> {
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
            Ok(self.wm_state.load(AtomicOrdering::Relaxed))
        }

        fn set_wm_state(&self, _win: WindowId, state: i64) -> Result<(), BackendError> {
            self.wm_state.store(state, AtomicOrdering::Relaxed);
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
                self.ewmh_hidden.store(on, AtomicOrdering::Relaxed);
            }
            Ok(())
        }

        fn has_net_wm_state_flag(
            &self,
            _win: WindowId,
            state: NetWmState,
        ) -> Result<bool, BackendError> {
            Ok(state == NetWmState::Hidden && self.ewmh_hidden.load(AtomicOrdering::Relaxed))
        }
    }

    struct MapRestoreWindowOps {
        geometry: Mutex<Geometry>,
        decoration_styles: Mutex<Vec<(WindowId, u32, Pixel)>>,
        fail_position: AtomicBool,
        fail_configure: AtomicBool,
        fail_decoration_once: AtomicBool,
        compositor_disable_trace: Mutex<Vec<&'static str>>,
    }

    impl MapRestoreWindowOps {
        fn new() -> Self {
            Self {
                geometry: Mutex::new(Geometry {
                    x: 120,
                    y: 80,
                    w: 640,
                    h: 480,
                    border: 0,
                }),
                decoration_styles: Mutex::new(Vec::new()),
                fail_position: AtomicBool::new(false),
                fail_configure: AtomicBool::new(false),
                fail_decoration_once: AtomicBool::new(false),
                compositor_disable_trace: Mutex::new(Vec::new()),
            }
        }
    }

    impl WindowOps for MapRestoreWindowOps {
        fn set_position(&self, _win: WindowId, x: i32, y: i32) -> Result<(), BackendError> {
            if self.fail_position.swap(false, AtomicOrdering::Relaxed) {
                return Err(BackendError::Message(
                    "injected set-position failure".into(),
                ));
            }
            let mut geometry = self.geometry.lock().expect("map restore geometry lock");
            geometry.x = x;
            geometry.y = y;
            Ok(())
        }

        fn configure(
            &self,
            _win: WindowId,
            x: i32,
            y: i32,
            w: u32,
            h: u32,
            border: u32,
        ) -> Result<(), BackendError> {
            self.compositor_disable_trace
                .lock()
                .expect("compositor disable trace lock")
                .push("park");
            if self.fail_configure.load(AtomicOrdering::Relaxed) {
                return Err(BackendError::Message("injected parking failure".into()));
            }
            *self.geometry.lock().expect("map restore geometry lock") =
                Geometry { x, y, w, h, border };
            Ok(())
        }

        fn set_decoration_style(
            &self,
            win: WindowId,
            border_width: u32,
            border_color: Pixel,
        ) -> Result<(), BackendError> {
            self.decoration_styles
                .lock()
                .expect("decoration styles lock")
                .push((win, border_width, border_color));
            if self
                .fail_decoration_once
                .swap(false, AtomicOrdering::Relaxed)
            {
                return Err(BackendError::Message("injected decoration failure".into()));
            }
            Ok(())
        }

        fn raise_window(&self, _win: WindowId) -> Result<(), BackendError> {
            Ok(())
        }

        fn map_window(&self, _win: WindowId) -> Result<(), BackendError> {
            Ok(())
        }

        fn unmap_window(&self, _win: WindowId) -> Result<(), BackendError> {
            Ok(())
        }

        fn close_window(&self, _win: WindowId) -> Result<CloseResult, BackendError> {
            Ok(CloseResult::Graceful)
        }

        fn set_input_focus(&self, _win: WindowId) -> Result<(), BackendError> {
            Ok(())
        }

        fn set_input_focus_root(&self) -> Result<(), BackendError> {
            Ok(())
        }

        fn get_window_attributes(&self, _win: WindowId) -> Result<WindowAttributes, BackendError> {
            Ok(WindowAttributes {
                override_redirect: false,
                map_state_viewable: true,
            })
        }

        fn get_geometry(&self, _win: WindowId) -> Result<Geometry, BackendError> {
            Ok(*self.geometry.lock().expect("map restore geometry lock"))
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
            _win: WindowId,
            changes: WindowChanges,
        ) -> Result<(), BackendError> {
            let mut geometry = self.geometry.lock().expect("map restore geometry lock");
            if let Some(x) = changes.x {
                geometry.x = x;
            }
            if let Some(y) = changes.y {
                geometry.y = y;
            }
            if let Some(width) = changes.width {
                geometry.w = width;
            }
            if let Some(height) = changes.height {
                geometry.h = height;
            }
            if let Some(border_width) = changes.border_width {
                geometry.border = border_width;
            }
            Ok(())
        }
    }

    /// `DummyInputOps` with a tally, so a test can ask whether a panel
    /// actually took the pointer rather than assuming it did.
    #[derive(Default)]
    struct GrabSpyInputOps {
        pointer_grabs: AtomicUsize,
        pointer_ungrabs: AtomicUsize,
    }

    impl InputOps for GrabSpyInputOps {
        fn set_cursor(
            &self,
            _kind: crate::backend::common_define::StdCursorKind,
        ) -> Result<(), BackendError> {
            Ok(())
        }
        fn get_pointer_position(&self) -> Result<(f64, f64), BackendError> {
            Ok((0.0, 0.0))
        }
        fn grab_pointer(&self, _mask: u32, _cursor: Option<u64>) -> Result<bool, BackendError> {
            self.pointer_grabs.fetch_add(1, AtomicOrdering::Relaxed);
            Ok(true)
        }
        fn ungrab_pointer(&self) -> Result<(), BackendError> {
            self.pointer_ungrabs.fetch_add(1, AtomicOrdering::Relaxed);
            Ok(())
        }
        fn query_pointer_root(&self) -> Result<(i32, i32, u16, u16), BackendError> {
            Ok((0, 0, 0, 0))
        }
    }

    #[derive(Default)]
    struct GrabSpyKeyOps {
        keyboard_grabs: AtomicUsize,
        keyboard_ungrabs: AtomicUsize,
    }

    impl KeyOps for GrabSpyKeyOps {
        fn grab_keys(
            &self,
            _root: WindowId,
            _bindings: &[(Mods, KeySym)],
        ) -> Result<(), BackendError> {
            Ok(())
        }

        fn clear_key_grabs(&self, _root: WindowId) -> Result<(), BackendError> {
            Ok(())
        }

        fn grab_keyboard(&self, _root: WindowId) -> Result<(), BackendError> {
            self.keyboard_grabs.fetch_add(1, AtomicOrdering::Relaxed);
            Ok(())
        }

        fn ungrab_keyboard(&self) -> Result<(), BackendError> {
            self.keyboard_ungrabs.fetch_add(1, AtomicOrdering::Relaxed);
            Ok(())
        }

        fn clean_mods(&self, _raw_state: u16) -> Mods {
            Mods::empty()
        }

        fn keysym_from_keycode(&mut self, keycode: u8) -> Result<KeySym, BackendError> {
            Ok(u32::from(keycode))
        }

        fn clear_cache(&mut self) {}
    }

    struct RenderSpyBackend {
        window_ops: MapRestoreWindowOps,
        input_ops: GrabSpyInputOps,
        property_ops: MapRestorePropertyOps,
        output_ops: DummyOutputOps,
        key_ops: GrabSpyKeyOps,
        cursor_provider: DummyCursorProvider,
        color_allocator: DummyColorAllocator,
        rendered_frames: usize,
        compositor_enabled: bool,
        compositor_supported: bool,
        compositor_transitions: Vec<bool>,
        compositor_config_applies: usize,
        compositor_monitor_updates: usize,
        compositor_brightness: Vec<f32>,
        compositor_urgency: Vec<(WindowId, bool)>,
        compositor_pip_updates: Vec<(WindowId, bool)>,
        compositor_minimized_updates: Vec<(WindowId, bool)>,
        compositor_static_ensures: Vec<WindowId>,
        compositor_forgotten_visuals: Vec<WindowId>,
        dock_geometry_updates: Vec<(WindowId, Option<CompositorRect>)>,
        dock_preview_updates: Vec<(Option<WindowId>, Option<CompositorRect>)>,
        system_ui_hit: SystemUiHitTarget,
        system_ui_hover_updates: Vec<Option<usize>>,
        x11_client_list: bool,
    }

    impl RenderSpyBackend {
        fn new() -> Self {
            Self {
                window_ops: MapRestoreWindowOps::new(),
                input_ops: GrabSpyInputOps::default(),
                property_ops: MapRestorePropertyOps::new(),
                output_ops: DummyOutputOps,
                key_ops: GrabSpyKeyOps::default(),
                cursor_provider: DummyCursorProvider,
                color_allocator: DummyColorAllocator,
                rendered_frames: 0,
                compositor_enabled: true,
                compositor_supported: true,
                compositor_transitions: Vec::new(),
                compositor_config_applies: 0,
                compositor_monitor_updates: 0,
                compositor_brightness: Vec::new(),
                compositor_urgency: Vec::new(),
                compositor_pip_updates: Vec::new(),
                compositor_minimized_updates: Vec::new(),
                compositor_static_ensures: Vec::new(),
                compositor_forgotten_visuals: Vec::new(),
                dock_geometry_updates: Vec::new(),
                dock_preview_updates: Vec::new(),
                system_ui_hit: SystemUiHitTarget::Unavailable,
                system_ui_hover_updates: Vec::new(),
                x11_client_list: false,
            }
        }
    }

    impl CompositorBenchmark for RenderSpyBackend {}
    impl BackendDiagnostics for RenderSpyBackend {}
    impl CompositorControl for RenderSpyBackend {
        fn compositor_apply_config(&mut self) {
            self.compositor_config_applies += 1;
        }

        fn compositor_set_brightness(&mut self, brightness: f32) {
            self.compositor_brightness.push(brightness);
        }
    }
    impl CompositorMedia for RenderSpyBackend {}
    impl CompositorWorkspaceEffects for RenderSpyBackend {
        fn compositor_set_monitors(&mut self, _monitors: &[(u32, i32, i32, u32, u32, u32)]) {
            self.compositor_monitor_updates += 1;
        }

        fn compositor_set_system_ui_hover(&mut self, row: Option<usize>) {
            self.system_ui_hover_updates.push(row);
        }

        fn compositor_system_ui_hit_test(&self, _x: f64, _y: f64) -> SystemUiHitTarget {
            self.system_ui_hit
        }
    }
    impl CompositorWindowEffects for RenderSpyBackend {
        fn compositor_set_window_urgent(&mut self, window: WindowId, urgent: bool) {
            self.compositor_urgency.push((window, urgent));
        }

        fn compositor_set_window_pip(&mut self, window: WindowId, pip: bool) {
            self.compositor_pip_updates.push((window, pip));
        }

        fn compositor_set_window_minimized(&mut self, window: WindowId, minimized: bool) {
            self.compositor_minimized_updates.push((window, minimized));
        }

        fn compositor_ensure_minimized_window_visual(&mut self, window: WindowId) {
            self.compositor_static_ensures.push(window);
        }

        fn compositor_forget_minimized_window_visual(&mut self, window: WindowId) {
            self.compositor_forgotten_visuals.push(window);
        }

        fn compositor_set_window_dock_geometry(
            &mut self,
            window: WindowId,
            target: Option<CompositorRect>,
        ) {
            self.dock_geometry_updates.push((window, target));
        }

        fn compositor_set_minimized_window_preview(
            &mut self,
            window: Option<WindowId>,
            anchor: Option<CompositorRect>,
        ) {
            self.dock_preview_updates.push((window, anchor));
        }
    }
    impl CompositorAnnotation for RenderSpyBackend {}
    impl DisplayControl for RenderSpyBackend {}

    impl RenderScheduler for RenderSpyBackend {
        fn has_compositor(&self) -> bool {
            self.compositor_enabled
        }

        fn compositor_needs_render(&self) -> bool {
            true
        }
    }

    impl Backend for RenderSpyBackend {
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                supports_client_list: self.x11_client_list,
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

        fn set_compositor_enabled(&mut self, enabled: bool) -> Result<bool, BackendError> {
            if !enabled {
                self.window_ops
                    .compositor_disable_trace
                    .lock()
                    .expect("compositor disable trace lock")
                    .push("disable");
            }
            self.compositor_transitions.push(enabled);
            if !self.compositor_supported || self.compositor_enabled == enabled {
                return Ok(false);
            }
            self.compositor_enabled = enabled;
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
            drag_ctl: None,
            tab_drag: None,
            message: SharedMessage::default(),
            secondary_bars: HashMap::new(),
            secondary_bar_failures: HashMap::new(),
            secondary_bar_retry_after: HashMap::new(),
            transient_children: crate::jwm::process::TransientChildSupervisor::default(),
            last_key_grab_refresh_at: None,
            pending_bar_updates: HashSet::new(),
            minimized_projection_epochs: HashMap::new(),
            reconciled_minimized_target_generations: HashMap::new(),
            minimized_dock_shelves: HashMap::new(),
            active_minimized_preview: None,
            active_minimized_preview_generation: None,
            suppress_mouse_focus_until: None,
            suppress_layout_animation: false,
            last_stacking: SecondaryMap::new(),
            scratchpads: HashMap::new(),
            scratchpad_pending: crate::jwm::scratchpad_pending::ScratchpadPendingRegistry::default(
            ),
            animations: AnimationManager::new(),
            hidden_client_park_retries: crate::jwm::monitor::HiddenClientParkRetries::default(),
            key_bindings: Vec::new(),
            chord_compiled: None,
            chord_armed_until: None,
            do_not_disturb: false,
            debug_hud_on: false,
            external_struts: HashMap::new(),
            ipc_server: None,
            update_readiness: None,
            async_update_notifier: None,
            config_reload_tracker: crate::jwm::lifecycle::ConfigReloadTracker::new(None),
            config_last_modified: None,
            config_reload_debounce: None,
            config_reload_count: 0,
            config_reload_last_unix_ms: None,
            config_reload_last_success: None,
            config_reload_last_error: None,
            layout_persist_dirty: None,
            override_redirect_windows: HashSet::new(),
            or_window_geometries: HashMap::new(),
            scrolling_states: HashMap::new(),
            last_night_light_update: None,
            night_light_override: None,
            last_battery_poll: None,
            last_idle_poll: None,
            idle: crate::jwm::features::idle::IdleTracker::default(),
            idle_inhibited: false,
            system_ui_dirty: false,
            server_saver_suppressed: false,
            features: FeatureStates::new(),
            event_coalescer:
                crate::backend::compositor_common::event_coalescer::EventCoalescer::new(),
            pending_pings: HashMap::new(),
            unresponsive_windows: HashSet::new(),
            last_ping_time: None,
            last_user_activity_time: 0,
        }
    }

    fn jwm_with_transition_client() -> (Jwm, ClientKey, WindowId) {
        use crate::core::models::WMClient;

        let mut jwm = empty_jwm();
        let window = WindowId::from_raw(0xc011_005e);
        let mut client = WMClient::new(window);
        client.geometry.x = 300;
        client.geometry.y = 200;
        client.geometry.w = 900;
        client.geometry.h = 600;
        client.geometry.border_w = 6;
        client.state.is_urgent = true;
        client.state.is_pip = true;
        let client_key = jwm.insert_client(client);
        (jwm, client_key, window)
    }

    #[test]
    fn common_update_reaps_spawned_transient_without_sigchld() {
        let mut jwm = empty_jwm();
        let mut backend = RenderSpyBackend::new();
        jwm.spawn(
            &mut backend,
            &WMArgEnum::StringVec(vec!["/bin/true".into()]),
        )
        .unwrap();
        assert!(!jwm.transient_children.is_empty());

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !jwm.transient_children.is_empty() && std::time::Instant::now() < deadline {
            EventHandler::update(&mut jwm, &mut backend).unwrap();
            if !jwm.transient_children.is_empty() {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }

        assert!(jwm.transient_children.is_empty());
    }

    #[test]
    fn headless_periodic_deadlines_are_consumed_and_stop_being_zero() {
        let mut jwm = empty_jwm();
        let mut backend = RenderSpyBackend::new();
        backend.compositor_enabled = false;
        assert_eq!(
            <Jwm as EventHandler>::next_wakeup(&jwm),
            Some(std::time::Duration::ZERO)
        );
        assert!(<Jwm as EventHandler>::needs_tick(&jwm));

        EventHandler::update(&mut jwm, &mut backend).unwrap();

        assert!(<Jwm as EventHandler>::next_wakeup(&jwm).is_some_and(|delay| !delay.is_zero()));
        assert!(!<Jwm as EventHandler>::needs_tick(&jwm));
    }

    #[test]
    fn event_handler_trait_object_delegates_immediate_render_to_jwm() {
        let mut jwm = empty_jwm();
        let mut backend = RenderSpyBackend::new();

        let handler: &mut dyn EventHandler = &mut jwm;
        handler.render_compositor_immediate(&mut backend);

        assert_eq!(backend.rendered_frames, 1);
    }

    #[test]
    fn ping_schedule_uses_exact_send_and_timeout_boundaries() {
        let now = std::time::Instant::now();
        assert_eq!(ping_schedule_next_wakeup(None, false, [], now), None);
        assert_eq!(
            ping_schedule_next_wakeup(None, true, [], now),
            Some(std::time::Duration::ZERO)
        );
        assert_eq!(
            ping_schedule_next_wakeup(
                Some(now),
                true,
                [now - PING_TIMEOUT + std::time::Duration::from_millis(1)],
                now,
            ),
            Some(std::time::Duration::from_millis(1))
        );
        assert_eq!(
            ping_schedule_next_wakeup(Some(now - PING_INTERVAL), true, [now - PING_TIMEOUT], now,),
            Some(std::time::Duration::ZERO)
        );
        assert_eq!(
            ping_schedule_next_wakeup(Some(now + std::time::Duration::from_secs(1)), true, [], now,),
            Some(PING_INTERVAL),
            "a future timestamp must not underflow"
        );
    }

    #[test]
    fn launcher_and_lock_lease_a_disabled_compositor_then_restore_black_mode() {
        let mut jwm = empty_jwm();
        let mut backend = RenderSpyBackend::new();
        backend.compositor_enabled = false;

        jwm.app_launcher(&mut backend, &WMArgEnum::Int(0)).unwrap();
        assert!(jwm.features.system_ui.is_active());
        assert!(jwm.features.system_ui_temporary_compositor);
        assert!(backend.compositor_enabled);
        jwm.close_system_ui(&mut backend);
        assert!(!backend.compositor_enabled);
        assert!(!jwm.features.system_ui_temporary_compositor);

        jwm.lock_screen(&mut backend, &WMArgEnum::Int(0)).unwrap();
        assert!(jwm.features.system_ui.is_locked());
        assert!(backend.compositor_enabled);
        jwm.close_system_ui(&mut backend);

        assert!(!backend.compositor_enabled);
        assert_eq!(backend.compositor_transitions, [true, false, true, false]);
    }

    #[test]
    fn partial_system_ui_enable_is_owned_by_the_lease_and_restored_to_native() {
        let (mut jwm, _client, _window) = jwm_with_transition_client();
        let mut backend = RenderSpyBackend::new();
        backend.compositor_enabled = false;
        backend.x11_client_list = true;
        backend
            .window_ops
            .fail_decoration_once
            .store(true, AtomicOrdering::Relaxed);

        jwm.app_launcher(&mut backend, &WMArgEnum::Int(0))
            .expect("a backend that reached ON can still host the panel");

        assert!(backend.compositor_enabled);
        assert!(jwm.features.system_ui.is_active());
        assert!(jwm.features.system_ui_temporary_compositor);
        assert_eq!(backend.compositor_transitions, [true]);
        assert_eq!(jwm.features.compositor_transition.attempts, 1);
        assert_eq!(jwm.features.compositor_transition.last_success, Some(false));

        jwm.close_system_ui(&mut backend);

        assert!(!backend.compositor_enabled);
        assert!(!jwm.features.system_ui.is_active());
        assert!(!jwm.features.system_ui_temporary_compositor);
        assert_eq!(backend.compositor_transitions, [true, false]);
        assert_eq!(jwm.features.compositor_transition.attempts, 2);
        assert_eq!(
            jwm.features.compositor_transition.last_requested_active,
            Some(false)
        );
        assert_eq!(jwm.features.compositor_transition.last_success, Some(true));
    }

    #[test]
    fn a_ui_action_closes_the_panel_it_opened_and_replaces_anyone_else_s() {
        let mut jwm = empty_jwm();
        let mut backend = RenderSpyBackend::new();

        jwm.app_launcher(&mut backend, &WMArgEnum::Int(0)).unwrap();
        assert!(jwm.features.system_ui.is_launcher());

        // The panels are mutually exclusive, so another panel's key takes the
        // screen over rather than reading as a dropped keypress. This is
        // Alt+F10 over Alt+F9's calendar.
        jwm.calendar(&mut backend, &WMArgEnum::Int(0)).unwrap();
        assert!(jwm.features.system_ui.is_calendar());

        // The action that opened it still takes it back down.
        jwm.calendar(&mut backend, &WMArgEnum::Int(0)).unwrap();
        assert!(!jwm.features.system_ui.is_active());

        jwm.app_launcher(&mut backend, &WMArgEnum::Int(0)).unwrap();
        assert!(jwm.features.system_ui.is_launcher());
        jwm.app_launcher(&mut backend, &WMArgEnum::Int(0)).unwrap();
        assert!(!jwm.features.system_ui.is_active());
    }

    #[test]
    fn system_ui_pointer_hover_click_wheel_and_scrim_follow_keyboard_semantics() {
        use crate::jwm::features::{ControlCenterInputs, ControlKind, SystemUiState};

        let mut jwm = empty_jwm();
        let mut backend = RenderSpyBackend::new();
        jwm.features.system_ui = SystemUiState::control_center(&ControlCenterInputs::default());
        assert_eq!(
            jwm.features.system_ui.selected_control(),
            Some(ControlKind::NightLight)
        );

        backend.system_ui_hit = SystemUiHitTarget::Item(1);
        <Jwm as WMController>::on_motion_notify(
            &mut jwm,
            &mut backend,
            HitTarget::Background { output: None },
            100.0,
            100.0,
            0,
        );
        assert_eq!(backend.system_ui_hover_updates.last(), Some(&Some(1)));
        assert_eq!(
            jwm.features.system_ui.selected_control(),
            Some(ControlKind::NightLight),
            "hover is a preview and must not steal keyboard focus"
        );

        <Jwm as WMController>::on_button_press(
            &mut jwm,
            &mut backend,
            HitTarget::Background { output: None },
            0,
            1,
            0,
        );
        assert_eq!(
            jwm.features.system_ui.selected_control(),
            Some(ControlKind::DoNotDisturb)
        );
        assert!(jwm.do_not_disturb, "click must run the row's Enter action");

        // The wheel uses the card, not the row under it, and clears the stale
        // hover cue before moving keyboard selection.
        backend.system_ui_hit = SystemUiHitTarget::Panel;
        <Jwm as WMController>::on_button_press(
            &mut jwm,
            &mut backend,
            HitTarget::Background { output: None },
            0,
            5,
            0,
        );
        assert_eq!(backend.system_ui_hover_updates.last(), Some(&None));
        assert_ne!(
            jwm.features.system_ui.selected_control(),
            Some(ControlKind::DoNotDisturb)
        );

        backend.system_ui_hit = SystemUiHitTarget::Outside;
        <Jwm as WMController>::on_button_press(
            &mut jwm,
            &mut backend,
            HitTarget::Background { output: None },
            0,
            1,
            0,
        );
        assert!(!jwm.features.system_ui.is_active());

        jwm.features.system_ui = SystemUiState::lock();
        <Jwm as WMController>::on_button_press(
            &mut jwm,
            &mut backend,
            HitTarget::Background { output: None },
            0,
            1,
            0,
        );
        assert!(
            jwm.features.system_ui.is_locked(),
            "the lock screen must ignore scrim clicks"
        );
    }

    #[test]
    fn swapping_panels_keeps_the_compositor_the_first_one_leased() {
        // A session running without compositing leases one for the lifetime of
        // a panel. Handing the screen from one panel to the next must not
        // release that lease and take it again: every hidden window would be
        // parked and unparked mid-swap, for a transition the user reads as one
        // motion.
        let mut jwm = empty_jwm();
        let mut backend = RenderSpyBackend::new();
        backend.compositor_enabled = false;

        jwm.app_launcher(&mut backend, &WMArgEnum::Int(0)).unwrap();
        assert!(jwm.features.system_ui_temporary_compositor);
        assert_eq!(backend.compositor_transitions, [true]);

        jwm.calendar(&mut backend, &WMArgEnum::Int(0)).unwrap();
        assert!(jwm.features.system_ui.is_calendar());
        assert!(jwm.features.system_ui_temporary_compositor);
        assert_eq!(backend.compositor_transitions, [true], "compositor flapped");

        // The lease is still honoured when the last panel finally closes.
        jwm.close_system_ui(&mut backend);
        assert!(!backend.compositor_enabled);
        assert!(!jwm.features.system_ui_temporary_compositor);
        assert_eq!(backend.compositor_transitions, [true, false]);
    }

    #[test]
    fn taking_over_from_a_keyboard_only_panel_still_takes_the_pointer() {
        // The keybinding viewer is the one panel opened without a pointer
        // grab. A panel inheriting its grabs would be modal for the keyboard
        // and transparent to the mouse, so clicks would land on the windows
        // underneath it.
        let mut jwm = empty_jwm();
        let mut backend = RenderSpyBackend::new();

        jwm.show_keybindings(&mut backend, &WMArgEnum::Int(0))
            .unwrap();
        assert_eq!(
            backend.input_ops.pointer_grabs.load(AtomicOrdering::SeqCst),
            0
        );

        jwm.calendar(&mut backend, &WMArgEnum::Int(0)).unwrap();
        assert!(jwm.features.system_ui.is_calendar());
        assert_eq!(
            backend.input_ops.pointer_grabs.load(AtomicOrdering::SeqCst),
            1,
            "the incoming panel never took the pointer"
        );
        // ... and it was taken without the screen ever being un-grabbed.
        assert_eq!(
            backend
                .input_ops
                .pointer_ungrabs
                .load(AtomicOrdering::SeqCst),
            0
        );
    }

    #[test]
    fn no_panel_takes_the_screen_from_the_lock_card() {
        // Mutual exclusion stops at the lock screen. This covers more than the
        // keyboard, which never reaches an opener while locked: `jwm_remote`
        // can call any of these by name over the IPC socket.
        let mut jwm = empty_jwm();
        let mut backend = RenderSpyBackend::new();

        jwm.lock_screen(&mut backend, &WMArgEnum::Int(0)).unwrap();
        assert!(jwm.features.system_ui.is_locked());

        let openers: [crate::jwm::types::WMFuncType; 5] = [
            Jwm::app_launcher,
            Jwm::calendar,
            Jwm::notification_center,
            Jwm::control_center,
            Jwm::session_menu,
        ];
        for open in openers {
            open(&mut jwm, &mut backend, &WMArgEnum::Int(0)).unwrap();
            assert!(jwm.features.system_ui.is_locked());
        }

        // The layout picker and the keybinding viewer never ask
        // `toggle_off_system_ui`; they go straight to `prepare_system_ui`, and
        // the first of them is reachable over IPC (`jwm-tool msg
        // layout_picker`). They have to be refused there instead — and loudly,
        // because unlike a swallowed toggle this is somebody trying to get in.
        for attempt in [
            Jwm::layout_picker as crate::jwm::types::WMFuncType,
            Jwm::show_keybindings,
        ] {
            assert!(attempt(&mut jwm, &mut backend, &WMArgEnum::Int(0)).is_err());
            assert!(jwm.features.system_ui.is_locked());
        }

        // Still a lock, not a lock-shaped panel: the password buffer survived.
        jwm.features.system_ui.push_char('x');
        assert!(jwm.features.system_ui.overlay_text().contains("JWM LOCKED"));
    }

    #[test]
    fn the_lock_action_never_unlocks() {
        let mut jwm = empty_jwm();
        let mut backend = RenderSpyBackend::new();

        jwm.lock_screen(&mut backend, &WMArgEnum::Int(0)).unwrap();
        jwm.lock_screen(&mut backend, &WMArgEnum::Int(0)).unwrap();
        assert!(jwm.features.system_ui.is_locked());
    }

    #[test]
    fn persistent_compositor_stays_enabled_after_system_ui_closes() {
        let mut jwm = empty_jwm();
        let mut backend = RenderSpyBackend::new();

        jwm.lock_screen(&mut backend, &WMArgEnum::Int(0)).unwrap();
        jwm.close_system_ui(&mut backend);

        assert!(backend.compositor_enabled);
        assert!(backend.compositor_transitions.is_empty());
    }

    #[test]
    fn unavailable_compositor_never_enters_an_invisible_lock_state() {
        let mut jwm = empty_jwm();
        let mut backend = RenderSpyBackend::new();
        backend.compositor_enabled = false;
        backend.compositor_supported = false;

        let error = jwm
            .lock_screen(&mut backend, &WMArgEnum::Int(0))
            .unwrap_err()
            .to_string();

        assert!(error.contains("could not start compositor"));
        assert!(!jwm.features.system_ui.is_active());
        assert!(!jwm.features.system_ui_temporary_compositor);
        assert!(!backend.compositor_enabled);
    }

    #[test]
    fn failed_direct_compositor_enable_is_reported_to_the_caller() {
        let mut jwm = empty_jwm();
        let mut backend = RenderSpyBackend::new();
        backend.compositor_enabled = false;
        backend.compositor_supported = false;

        let error = jwm
            .togglecompositor(&mut backend, &WMArgEnum::Int(0))
            .expect_err("the IPC action must not acknowledge a failed renderer start");

        assert!(error.to_string().contains("without reaching enabled state"));
        assert!(!backend.compositor_enabled);
        assert_eq!(backend.compositor_transitions, [true]);
        assert_eq!(jwm.features.compositor_transition.attempts, 1);
        assert_eq!(
            jwm.features.compositor_transition.last_requested_active,
            Some(true)
        );
        assert_eq!(jwm.features.compositor_transition.last_success, Some(false));
        assert!(
            jwm.features
                .compositor_transition
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("without reaching enabled state"))
        );
    }

    #[test]
    fn compositor_only_visual_modes_cannot_enter_in_native_mode() {
        let mut jwm = empty_jwm();
        let mut backend = RenderSpyBackend::new();
        backend.compositor_enabled = false;

        assert!(
            jwm.toggle_debug_hud(&mut backend, &WMArgEnum::Int(0))
                .is_err()
        );
        assert!(
            jwm.toggle_magnifier(&mut backend, &WMArgEnum::Int(0))
                .is_err()
        );
        assert!(!jwm.debug_hud_on);
        assert!(!jwm.features.magnifier.enabled);
    }

    #[test]
    fn compositor_disable_is_deferred_while_lock_screen_is_modal() {
        let mut jwm = empty_jwm();
        let mut backend = RenderSpyBackend::new();
        jwm.features.system_ui = crate::jwm::features::SystemUiState::lock();

        jwm.togglecompositor(&mut backend, &WMArgEnum::Int(0))
            .unwrap();
        assert!(backend.compositor_enabled);
        assert!(jwm.features.system_ui_temporary_compositor);

        jwm.close_system_ui(&mut backend);
        assert!(!backend.compositor_enabled);
        assert_eq!(backend.compositor_transitions, [false]);
    }

    #[test]
    fn x11_compositor_transition_swaps_native_borders_and_replays_window_state() {
        let (mut jwm, _client, window) = jwm_with_transition_client();
        let mut backend = RenderSpyBackend::new();
        backend.x11_client_list = true;

        jwm.togglecompositor(&mut backend, &WMArgEnum::Int(0))
            .unwrap();

        assert!(!backend.compositor_enabled);
        assert_eq!(backend.compositor_transitions, [false]);
        assert_eq!(jwm.features.compositor_transition.attempts, 1);
        assert_eq!(
            jwm.features.compositor_transition.last_requested_active,
            Some(false)
        );
        assert_eq!(jwm.features.compositor_transition.last_success, Some(true));
        assert_eq!(
            *backend
                .window_ops
                .decoration_styles
                .lock()
                .expect("decoration styles lock"),
            [(window, 6, Pixel(0))]
        );

        jwm.togglecompositor(&mut backend, &WMArgEnum::Int(0))
            .unwrap();

        assert!(backend.compositor_enabled);
        assert_eq!(backend.compositor_transitions, [false, true]);
        assert_eq!(
            *backend
                .window_ops
                .decoration_styles
                .lock()
                .expect("decoration styles lock"),
            [(window, 6, Pixel(0)), (window, 0, Pixel(0))]
        );
        assert_eq!(backend.compositor_urgency, [(window, true)]);
        assert_eq!(backend.compositor_pip_updates, [(window, true)]);
        assert_eq!(backend.compositor_config_applies, 1);
        assert_eq!(backend.compositor_monitor_updates, 1);
        assert_eq!(jwm.features.compositor_transition.attempts, 2);
        assert_eq!(
            jwm.features.compositor_transition.last_requested_active,
            Some(true)
        );
        assert_eq!(jwm.features.compositor_transition.last_success, Some(true));
        assert!(
            jwm.features
                .compositor_transition
                .last_attempt_unix_ms
                .is_some()
        );
        assert_eq!(jwm.features.compositor_transition.last_error, None);
    }

    #[test]
    fn x11_compositor_transition_hands_animation_geometry_to_native_and_back() {
        use crate::core::animation::{AnimationKind, ClientAnimation, Easing};
        use crate::core::types::Rect;

        let (mut jwm, client, window) = jwm_with_transition_client();
        let mut backend = RenderSpyBackend::new();
        backend.x11_client_list = true;
        let from = Rect::new(20, 40, 300, 240);
        let target = Rect::new(300, 200, 900, 600);
        jwm.animations.active.insert(
            client,
            ClientAnimation {
                from,
                to: target,
                started_at: std::time::Instant::now() - std::time::Duration::from_secs(30),
                duration: std::time::Duration::from_secs(120),
                easing: Easing::Linear,
                kind: AnimationKind::Layout,
            },
        );

        jwm.togglecompositor(&mut backend, &WMArgEnum::Int(0))
            .unwrap();

        let native = backend.window_ops.get_geometry(window).unwrap();
        assert!(native.x > from.x && native.x < target.x);
        assert!(native.y > from.y && native.y < target.y);
        assert!(native.w > from.w as u32 && native.w < target.w as u32);
        assert!(native.h > from.h as u32 && native.h < target.h as u32);
        assert_eq!(native.border, 6);

        jwm.togglecompositor(&mut backend, &WMArgEnum::Int(0))
            .unwrap();

        let composited = backend.window_ops.get_geometry(window).unwrap();
        assert_eq!(composited.x, target.x);
        assert_eq!(composited.y, target.y);
        assert_eq!(composited.w, target.w as u32);
        assert_eq!(composited.h, target.h as u32);
        assert_eq!(composited.border, 0);
    }

    #[test]
    fn failed_native_presentation_staging_attempts_every_client_and_rolls_back() {
        use crate::core::models::WMClient;

        let (mut jwm, _first, first_window) = jwm_with_transition_client();
        let second_window = WindowId::from_raw(0xc011_005f);
        let mut second = WMClient::new(second_window);
        second.geometry.border_w = 4;
        jwm.insert_client(second);

        let mut backend = RenderSpyBackend::new();
        backend.x11_client_list = true;
        backend
            .window_ops
            .fail_decoration_once
            .store(true, AtomicOrdering::Relaxed);

        let error = jwm
            .togglecompositor(&mut backend, &WMArgEnum::Int(0))
            .expect_err("partial native staging must abort and report the transition");

        assert!(error.to_string().contains("decoration failure"));
        assert!(backend.compositor_enabled);
        assert!(backend.compositor_transitions.is_empty());
        let attempts = backend
            .window_ops
            .decoration_styles
            .lock()
            .expect("decoration styles lock");
        for (window, native_border) in [(first_window, 6), (second_window, 4)] {
            assert!(attempts.contains(&(window, native_border, Pixel(0))));
            assert!(
                attempts.contains(&(window, 0, Pixel(0))),
                "rollback must restore the composited border for {window:?}"
            );
        }
    }

    #[test]
    fn compositor_enable_presentation_failure_is_reported_after_reconciling_later_clients() {
        use crate::core::models::WMClient;

        let (mut jwm, _first, first_window) = jwm_with_transition_client();
        let second_window = WindowId::from_raw(0xc011_0060);
        let mut second = WMClient::new(second_window);
        second.geometry.border_w = 4;
        jwm.insert_client(second);

        let mut backend = RenderSpyBackend::new();
        backend.compositor_enabled = false;
        backend.x11_client_list = true;
        backend
            .window_ops
            .fail_decoration_once
            .store(true, AtomicOrdering::Relaxed);

        let error = jwm
            .togglecompositor(&mut backend, &WMArgEnum::Int(0))
            .expect_err("a partially reconciled enable must not be acknowledged as successful");

        assert!(backend.compositor_enabled);
        assert_eq!(backend.compositor_transitions, [true]);
        assert!(error.to_string().contains("decoration failure"));
        assert_eq!(jwm.features.compositor_transition.attempts, 1);
        assert_eq!(
            jwm.features.compositor_transition.last_requested_active,
            Some(true)
        );
        assert_eq!(jwm.features.compositor_transition.last_success, Some(false));
        assert!(
            jwm.features
                .compositor_transition
                .last_error
                .as_deref()
                .is_some_and(|error| {
                    error.contains("compositor is enabled") && error.contains("decoration failure")
                })
        );
        // The backend already reached ON before the client presentation error.
        // Keep it usable by replaying runtime state even though the caller sees
        // the partial failure.
        assert_eq!(backend.compositor_config_applies, 1);
        assert_eq!(backend.compositor_monitor_updates, 1);
        let attempts = backend
            .window_ops
            .decoration_styles
            .lock()
            .expect("decoration styles lock");
        assert!(attempts.contains(&(first_window, 0, Pixel(0))));
        assert!(attempts.contains(&(second_window, 0, Pixel(0))));
    }

    #[test]
    fn compositor_enable_reapplies_an_existing_idle_dim() {
        let mut jwm = empty_jwm();
        let settings = crate::jwm::features::idle::IdleSettings::from_secs(1, 0.25, 0, 0, false);
        let actions = jwm.idle.poll(
            &settings,
            std::time::Duration::from_secs(2),
            false,
            false,
            std::time::Instant::now(),
        );
        assert!(
            actions
                .iter()
                .any(|action| matches!(action, crate::jwm::features::idle::IdleAction::Dim(_)))
        );

        let mut backend = RenderSpyBackend::new();
        backend.compositor_enabled = false;
        jwm.togglecompositor(&mut backend, &WMArgEnum::Int(0))
            .unwrap();

        assert!(backend.compositor_enabled);
        assert_eq!(backend.compositor_brightness.len(), 1);
        assert!(backend.compositor_brightness[0] < 1.0);
    }

    #[test]
    fn compositor_disable_closes_overview_and_expose_and_releases_their_grabs() {
        let mut jwm = empty_jwm();
        let mut backend = RenderSpyBackend::new();
        let root = backend.root_window().expect("test backend root");

        jwm.features.overview.active = true;
        backend.key_ops.grab_keyboard(root).unwrap();
        jwm.apply_expose_action(
            &mut backend,
            crate::jwm::features::ExposeAction::Enter {
                windows: vec![(WindowId::from_raw(0xe001), 10, 10, 640, 480)],
            },
        )
        .unwrap();
        assert_eq!(
            backend.key_ops.keyboard_grabs.load(AtomicOrdering::Relaxed),
            2
        );
        assert_eq!(
            backend
                .input_ops
                .pointer_grabs
                .load(AtomicOrdering::Relaxed),
            1
        );

        jwm.togglecompositor(&mut backend, &WMArgEnum::Int(0))
            .unwrap();

        assert!(!jwm.features.overview.active);
        assert!(!jwm.features.expose_active);
        assert_eq!(
            backend
                .key_ops
                .keyboard_ungrabs
                .load(AtomicOrdering::Relaxed),
            2
        );
        assert_eq!(
            backend
                .input_ops
                .pointer_ungrabs
                .load(AtomicOrdering::Relaxed),
            1
        );
        assert!(!backend.compositor_enabled);
    }

    #[test]
    fn compositor_disable_parks_hidden_clients_before_backend_teardown() {
        let (mut jwm, _target, _target_window) = jwm_with_hidden_activation_target();
        let mut backend = RenderSpyBackend::new();
        backend.x11_client_list = true;

        jwm.togglecompositor(&mut backend, &WMArgEnum::Int(0))
            .unwrap();

        assert!(!backend.compositor_enabled);
        assert_eq!(backend.compositor_transitions, [false]);
        assert_eq!(
            *backend
                .window_ops
                .compositor_disable_trace
                .lock()
                .expect("compositor disable trace lock"),
            ["park", "disable"]
        );
    }

    #[test]
    fn failed_hidden_client_parking_prevents_backend_compositor_disable() {
        let (mut jwm, _target, _target_window) = jwm_with_hidden_activation_target();
        let mut backend = RenderSpyBackend::new();
        backend.x11_client_list = true;
        backend
            .window_ops
            .fail_configure
            .store(true, AtomicOrdering::Relaxed);

        let error = jwm
            .togglecompositor(&mut backend, &WMArgEnum::Int(0))
            .expect_err("a failed parking barrier must be visible to IPC callers");

        assert!(backend.compositor_enabled);
        assert!(backend.compositor_transitions.is_empty());
        assert!(error.to_string().contains("parking failure"));
        assert_eq!(
            *backend
                .window_ops
                .compositor_disable_trace
                .lock()
                .expect("compositor disable trace lock"),
            ["park"]
        );
    }

    #[test]
    fn failed_parking_barrier_preserves_active_compositor_modes() {
        let (mut jwm, _target, _target_window) = jwm_with_hidden_activation_target();
        let mut backend = RenderSpyBackend::new();
        backend.x11_client_list = true;
        jwm.features.overview.active = true;
        jwm.apply_expose_action(
            &mut backend,
            crate::jwm::features::ExposeAction::Enter {
                windows: vec![(WindowId::from_raw(0xe002), 20, 20, 500, 400)],
            },
        )
        .unwrap();
        backend
            .window_ops
            .fail_configure
            .store(true, AtomicOrdering::Relaxed);

        jwm.togglecompositor(&mut backend, &WMArgEnum::Int(0))
            .expect_err("parking failure must keep the compositor and its modes intact");

        assert!(backend.compositor_enabled);
        assert!(jwm.features.overview.active);
        assert!(jwm.features.expose_active);
        assert_eq!(
            backend
                .key_ops
                .keyboard_ungrabs
                .load(AtomicOrdering::Relaxed),
            0
        );
        assert_eq!(
            backend
                .input_ops
                .pointer_ungrabs
                .load(AtomicOrdering::Relaxed),
            0
        );
    }

    #[test]
    fn far_left_topology_repark_retries_without_busy_loop_and_converges() {
        let (mut jwm, target, target_window) = jwm_with_hidden_activation_target();
        let original_order = jwm.state.clients[target].state.minimized_order;
        let original_restore = jwm.state.clients[target].geometry.hidden_restore_rect;
        let mut backend = RenderSpyBackend::new();
        backend.x11_client_list = true;
        // Settle the independent config-poll deadline so the assertions below
        // isolate the hidden-park scheduler's wakeup behavior.
        let settled_at = std::time::Instant::now();
        jwm.poll_config_reload(&mut backend, settled_at);
        jwm.last_battery_poll = Some(settled_at);
        jwm.last_idle_poll = Some(settled_at);
        jwm.last_ping_time = Some(settled_at);
        jwm.features.resource_sampler.defer_for_test(settled_at);
        backend
            .window_ops
            .fail_position
            .store(true, AtomicOrdering::Relaxed);

        jwm.add_monitor(output_info(99, -4000));
        for monitor_id in jwm
            .state
            .monitors
            .values()
            .map(|monitor| monitor.num)
            .collect::<Vec<_>>()
        {
            jwm.secondary_bar_retry_after
                .insert(monitor_id, settled_at + std::time::Duration::from_secs(5));
        }
        jwm.repark_all_hidden_clients(&mut backend);

        assert!(jwm.has_hidden_client_park_retry(target));
        jwm.defer_hidden_client_park_retry_for_test(target, std::time::Duration::from_secs(1));
        assert_eq!(
            backend
                .window_ops
                .geometry
                .lock()
                .expect("map restore geometry lock")
                .x,
            120,
            "the injected first failure must leave the server geometry stale"
        );
        assert!(
            !<Jwm as EventHandler>::needs_tick(&jwm),
            "a future retry deadline must not force a 1ms X11 poll loop"
        );
        assert!(
            <Jwm as EventHandler>::next_wakeup(&jwm).is_some_and(|delay| !delay.is_zero()),
            "the event loop must receive the future retry deadline"
        );

        backend
            .window_ops
            .fail_configure
            .store(true, AtomicOrdering::Relaxed);
        jwm.force_hidden_client_park_retry_due(target);
        assert!(<Jwm as EventHandler>::needs_tick(&jwm));
        assert_eq!(
            <Jwm as EventHandler>::next_wakeup(&jwm),
            Some(std::time::Duration::ZERO)
        );
        jwm.tick_hidden_client_park_retries(&mut backend, std::time::Instant::now());

        assert!(jwm.has_hidden_client_park_retry(target));
        assert!(
            !<Jwm as EventHandler>::needs_tick(&jwm),
            "a failed due attempt must back off instead of remaining due"
        );
        assert!(<Jwm as EventHandler>::next_wakeup(&jwm).is_some_and(|delay| !delay.is_zero()));

        backend
            .window_ops
            .fail_configure
            .store(false, AtomicOrdering::Relaxed);
        jwm.force_hidden_client_park_retry_due(target);
        jwm.tick_hidden_client_park_retries(&mut backend, std::time::Instant::now());

        assert!(!jwm.has_hidden_client_park_retry(target));
        let geometry = *backend
            .window_ops
            .geometry
            .lock()
            .expect("map restore geometry lock");
        assert!(
            geometry
                .x
                .saturating_add(i32::try_from(geometry.w).unwrap_or(i32::MAX))
                <= jwm.desktop_left_edge(),
            "the successful retry must read back a fully parked current-topology geometry"
        );
        let client = &jwm.state.clients[target];
        assert!(client.state.is_hidden);
        assert_eq!(client.state.minimized_order, original_order);
        assert_eq!(client.geometry.hidden_restore_rect, original_restore);
        assert!(backend.compositor_minimized_updates.is_empty());
        assert_eq!(target_window, client.win);
    }

    #[test]
    fn failed_temporary_compositor_release_keeps_the_lease_retryable() {
        let (mut jwm, _target, _target_window) = jwm_with_hidden_activation_target();
        let mut backend = RenderSpyBackend::new();
        backend.x11_client_list = true;
        backend
            .window_ops
            .fail_configure
            .store(true, AtomicOrdering::Relaxed);
        jwm.features.system_ui_temporary_compositor = true;

        jwm.close_system_ui(&mut backend);

        assert!(backend.compositor_enabled);
        assert!(jwm.features.system_ui_temporary_compositor);
        assert!(backend.compositor_transitions.is_empty());
    }

    #[test]
    fn refused_temporary_compositor_release_does_not_drop_the_lease_flag() {
        let mut jwm = empty_jwm();
        let mut backend = RenderSpyBackend::new();
        backend.compositor_supported = false;
        jwm.features.system_ui_temporary_compositor = true;

        jwm.close_system_ui(&mut backend);

        assert!(backend.compositor_enabled);
        assert!(jwm.features.system_ui_temporary_compositor);
        assert_eq!(backend.compositor_transitions, [false]);
    }

    /// A window manager with one empty monitor on a 1920x1080 screen, which is
    /// the least the layout picker needs to have something to switch.
    fn jwm_with_monitor() -> Jwm {
        use crate::core::models::{Pertag, WMMonitor};

        let mut jwm = empty_jwm();
        let mut monitor = WMMonitor::new();
        monitor.pertag = Some(Pertag::new(true, CONFIG.load().tags_length()));
        let key = jwm.state.monitors.insert(monitor);
        jwm.state.monitor_order.push(key);
        jwm.state.sel_mon = Some(key);
        jwm.s_w = 1920;
        jwm.s_h = 1080;
        jwm
    }

    fn output_info(id: u64, x: i32) -> OutputInfo {
        OutputInfo {
            id: OutputId(id),
            name: format!("test-{id}"),
            x,
            y: 0,
            width: 1920,
            height: 1080,
            scale: 1.0,
            refresh_rate: 60_000,
            hdr_capable: false,
            hdr_metadata: None,
            identity: crate::backend::api::OutputIdentity::connector_only(format!("test-{id}")),
        }
    }

    fn jwm_with_hidden_activation_target() -> (Jwm, ClientKey, WindowId) {
        use crate::core::models::WMClient;

        let mut jwm = empty_jwm();
        let mut monitor = jwm.createmon(true);
        monitor.geometry.m_w = 1920;
        monitor.geometry.m_h = 1080;
        monitor.geometry.w_w = 1920;
        monitor.geometry.w_h = 1080;
        let monitor_key = jwm.insert_monitor(monitor);
        jwm.state.sel_mon = Some(monitor_key);
        jwm.s_w = 1920;
        jwm.s_h = 1080;

        let current_window = WindowId::from_raw(0x101);
        let mut current = WMClient::new(current_window);
        current.mon = Some(monitor_key);
        current.state.tags = 0b01;
        current.geometry.x = 40;
        current.geometry.y = 80;
        current.geometry.w = 800;
        current.geometry.h = 600;
        let current_key = jwm.insert_client(current);
        jwm.attach_to_monitor(current_key, monitor_key);

        let target_window = WindowId::from_raw(0x202);
        let mut target = WMClient::new(target_window);
        target.mon = Some(monitor_key);
        target.state.tags = 0b10;
        target.state.is_hidden = true;
        target.state.minimized_order = 7;
        target.geometry.x = -1600;
        target.geometry.old_x = 120;
        target.geometry.y = 100;
        target.geometry.old_y = 100;
        target.geometry.w = 800;
        target.geometry.h = 600;
        let target_key = jwm.insert_client(target);
        jwm.attach_to_monitor(target_key, monitor_key);

        if let Some(monitor) = jwm.state.monitors.get_mut(monitor_key) {
            monitor.set_selected_client_for_current_tag(Some(current_key));
        }

        (jwm, target_key, target_window)
    }

    fn assert_activation_revealed_target(jwm: &Jwm, target: ClientKey) {
        let client = &jwm.state.clients[target];
        assert!(!client.state.is_hidden);
        assert_eq!(client.state.minimized_order, 0);
        assert_eq!(jwm.state.sel_mon, client.mon);
        let monitor = jwm.state.monitors.get(client.mon.unwrap()).unwrap();
        assert_eq!(monitor.get_active_tags(), 0b10);
        assert_eq!(monitor.sel, Some(target));
    }

    fn jwm_with_cross_monitor_hidden_map_target() -> (Jwm, ClientKey, WindowId, ClientKey) {
        use crate::core::models::WMClient;

        let mut jwm = empty_jwm();

        let mut source_monitor = jwm.createmon(true);
        source_monitor.num = 0;
        source_monitor.geometry.m_x = 0;
        source_monitor.geometry.m_w = 1280;
        source_monitor.geometry.m_h = 720;
        source_monitor.geometry.w_x = 0;
        source_monitor.geometry.w_w = 1280;
        source_monitor.geometry.w_h = 720;
        let source_monitor = jwm.insert_monitor(source_monitor);

        let mut target_monitor = jwm.createmon(true);
        target_monitor.num = 1;
        target_monitor.geometry.m_x = 1280;
        target_monitor.geometry.m_w = 1920;
        target_monitor.geometry.m_h = 1080;
        target_monitor.geometry.w_x = 1280;
        target_monitor.geometry.w_w = 1920;
        target_monitor.geometry.w_h = 1080;
        let target_monitor = jwm.insert_monitor(target_monitor);

        jwm.state.sel_mon = Some(source_monitor);
        jwm.s_w = 3200;
        jwm.s_h = 1080;

        let source_window = WindowId::from_raw(0x301);
        let mut source = WMClient::new(source_window);
        source.mon = Some(source_monitor);
        source.state.tags = 0b01;
        source.geometry.x = 80;
        source.geometry.y = 80;
        source.geometry.w = 900;
        source.geometry.h = 560;
        let source_client = jwm.insert_client(source);
        jwm.attach_to_monitor(source_client, source_monitor);
        jwm.state.monitors[source_monitor].set_selected_client_for_current_tag(Some(source_client));

        let target_window = WindowId::from_raw(0x302);
        let mut target = WMClient::new(target_window);
        target.mon = Some(target_monitor);
        target.state.tags = 0b10;
        target.state.is_hidden = true;
        target.state.minimized_order = 11;
        target.geometry.x = -1000;
        target.geometry.hidden_x = Some(-1000);
        target.geometry.old_x = 1440;
        target.geometry.y = 120;
        target.geometry.old_y = 120;
        target.geometry.w = 1000;
        target.geometry.h = 700;
        let target_client = jwm.insert_client(target);
        jwm.attach_to_monitor(target_client, target_monitor);

        (jwm, target_client, target_window, source_client)
    }

    #[test]
    fn net_active_window_reveals_a_minimized_window_on_another_tag() {
        let (mut jwm, target, target_window) = jwm_with_hidden_activation_target();
        let mut backend = RenderSpyBackend::new();

        jwm.handle_event(
            &mut backend,
            BackendEvent::ActiveWindowMessage {
                window: target_window,
            },
        )
        .unwrap();

        assert_activation_revealed_target(&jwm, target);
    }

    #[test]
    fn manager_owned_unmaps_keep_the_client_managed_for_every_reason() {
        for reason in [
            ManagedUnmapReason::SwallowDiscard,
            ManagedUnmapReason::IconifyRetain { generation: 17 },
        ] {
            let (mut jwm, target, target_window) = jwm_with_hidden_activation_target();
            let mut backend = RenderSpyBackend::new();

            jwm.handle_event(
                &mut backend,
                BackendEvent::WindowManagerUnmapped {
                    window: target_window,
                    reason,
                },
            )
            .unwrap();

            assert_eq!(jwm.wintoclient(target_window), Some(target));
        }
    }

    #[test]
    fn configure_unmap_reconverges_a_hidden_client_to_iconic_state() {
        let (mut jwm, target, target_window) = jwm_with_hidden_activation_target();
        let mut backend = RenderSpyBackend::new();
        backend.property_ops.wm_state.store(
            i64::from(crate::jwm::types::NORMAL_STATE),
            AtomicOrdering::Relaxed,
        );

        jwm.handle_event(
            &mut backend,
            BackendEvent::WindowUnmapped {
                window: target_window,
                from_configure: true,
            },
        )
        .unwrap();

        assert_eq!(jwm.wintoclient(target_window), Some(target));
        assert_eq!(
            backend.property_ops.wm_state.load(AtomicOrdering::Relaxed),
            i64::from(crate::jwm::types::ICONIC_STATE)
        );
    }

    #[test]
    fn mapped_hidden_ineligible_client_retires_the_late_live_visual() {
        let (mut jwm, target, target_window) = jwm_with_hidden_activation_target();
        let mut backend = RenderSpyBackend::new();
        jwm.state.clients[target].state.skip_taskbar = true;

        jwm.handle_event(&mut backend, BackendEvent::WindowMapped(target_window))
            .unwrap();

        assert!(jwm.state.clients[target].state.is_hidden);
        assert_eq!(backend.compositor_forgotten_visuals, vec![target_window]);
        assert!(backend.compositor_minimized_updates.is_empty());
    }

    #[test]
    fn external_unmap_still_withdraws_and_unmanages_the_client() {
        let (mut jwm, _target, target_window) = jwm_with_hidden_activation_target();
        let mut backend = RenderSpyBackend::new();

        jwm.handle_event(
            &mut backend,
            BackendEvent::WindowUnmapped {
                window: target_window,
                from_configure: false,
            },
        )
        .unwrap();

        assert_eq!(jwm.wintoclient(target_window), None);
    }

    #[test]
    fn foreign_toplevel_activate_reveals_a_minimized_window_on_another_tag() {
        let (mut jwm, target, target_window) = jwm_with_hidden_activation_target();
        let mut backend = RenderSpyBackend::new();

        jwm.handle_event(
            &mut backend,
            BackendEvent::ForeignToplevelActivate(target_window),
        )
        .unwrap();

        assert_activation_revealed_target(&jwm, target);
    }

    #[test]
    fn foreign_toplevel_unminimize_reveals_a_minimized_window_on_another_tag() {
        let (mut jwm, target, target_window) = jwm_with_hidden_activation_target();
        let mut backend = RenderSpyBackend::new();
        jwm.schedule_hidden_client_park_retry(target, std::time::Instant::now());
        assert!(jwm.has_hidden_client_park_retry(target));
        backend
            .property_ops
            .ewmh_hidden
            .store(true, AtomicOrdering::Relaxed);
        backend.property_ops.wm_state.store(
            i64::from(crate::jwm::types::ICONIC_STATE),
            AtomicOrdering::Relaxed,
        );

        jwm.handle_event(
            &mut backend,
            BackendEvent::ForeignToplevelSetMinimized(target_window, false),
        )
        .unwrap();

        assert_activation_revealed_target(&jwm, target);
        assert!(!jwm.has_hidden_client_park_retry(target));
        assert_eq!(
            backend.property_ops.wm_state.load(AtomicOrdering::Relaxed),
            i64::from(crate::jwm::types::NORMAL_STATE)
        );
        assert!(
            !backend
                .property_ops
                .ewmh_hidden
                .load(AtomicOrdering::Relaxed)
        );
    }

    #[test]
    fn ewmh_hidden_remove_reveals_a_minimized_window_on_another_tag() {
        let (mut jwm, target, target_window) = jwm_with_hidden_activation_target();
        let mut backend = RenderSpyBackend::new();
        backend
            .property_ops
            .ewmh_hidden
            .store(true, AtomicOrdering::Relaxed);
        backend.property_ops.wm_state.store(
            i64::from(crate::jwm::types::ICONIC_STATE),
            AtomicOrdering::Relaxed,
        );

        jwm.on_window_state_request(
            &mut backend,
            target_window,
            NetWmAction::Remove,
            NetWmState::Hidden,
        );

        assert_activation_revealed_target(&jwm, target);
        assert_eq!(
            backend.property_ops.wm_state.load(AtomicOrdering::Relaxed),
            i64::from(crate::jwm::types::NORMAL_STATE)
        );
        assert!(
            !backend
                .property_ops
                .ewmh_hidden
                .load(AtomicOrdering::Relaxed)
        );
    }

    #[test]
    fn repeated_external_unminimize_does_not_activate_an_off_tag_visible_client() {
        let (mut jwm, target, target_window) = jwm_with_hidden_activation_target();
        let monitor = jwm.state.clients[target].mon.unwrap();
        let current = jwm.state.monitors[monitor].sel.expect("current selection");
        jwm.state.clients[target].state.is_hidden = false;
        jwm.state.clients[target].state.minimized_order = 0;
        let mut backend = RenderSpyBackend::new();

        jwm.handle_event(
            &mut backend,
            BackendEvent::ForeignToplevelSetMinimized(target_window, false),
        )
        .unwrap();

        assert_eq!(jwm.state.monitors[monitor].get_active_tags(), 0b01);
        assert_eq!(jwm.state.monitors[monitor].sel, Some(current));
        assert_eq!(jwm.get_selected_client_key(), Some(current));
    }

    #[test]
    fn x11_map_request_deiconifies_a_managed_window_across_monitor_and_tag() {
        let (mut jwm, target, target_window, _) = jwm_with_cross_monitor_hidden_map_target();
        let target_monitor = jwm.state.clients[target].mon.unwrap();
        let mut backend = RenderSpyBackend::new();
        backend.x11_client_list = true;
        backend
            .property_ops
            .ewmh_hidden
            .store(true, AtomicOrdering::Relaxed);
        backend.property_ops.wm_state.store(
            i64::from(crate::jwm::types::ICONIC_STATE),
            AtomicOrdering::Relaxed,
        );

        // Both X11 transports bridge any MapRequest they receive to
        // WindowCreated. This is a dispatcher regression, not an end-to-end
        // XMapWindow test: an off-screen minimized client remains mapped, so
        // an ordinary XMapWindow normally emits no request. A duplicate queued
        // or synthetic request must still avoid replaying reverse Genie.
        for _ in 0..2 {
            jwm.handle_event(&mut backend, BackendEvent::WindowCreated(target_window))
                .unwrap();
        }

        assert_activation_revealed_target(&jwm, target);
        assert_eq!(jwm.state.sel_mon, Some(target_monitor));
        assert_eq!(
            backend.property_ops.wm_state.load(AtomicOrdering::Relaxed),
            i64::from(crate::jwm::types::NORMAL_STATE)
        );
        assert!(
            !backend
                .property_ops
                .ewmh_hidden
                .load(AtomicOrdering::Relaxed)
        );
        assert_eq!(
            backend.compositor_minimized_updates,
            vec![(target_window, false)]
        );
    }

    #[test]
    fn native_window_created_duplicate_does_not_deiconify_a_dock_item() {
        let (mut jwm, target, target_window, source) = jwm_with_cross_monitor_hidden_map_target();
        let source_monitor = jwm.state.clients[source].mon;
        let mut backend = RenderSpyBackend::new();
        backend
            .property_ops
            .ewmh_hidden
            .store(true, AtomicOrdering::Relaxed);
        backend.property_ops.wm_state.store(
            i64::from(crate::jwm::types::ICONIC_STATE),
            AtomicOrdering::Relaxed,
        );

        // Native Wayland uses WindowCreated for a lifecycle notification, not
        // an ICCCM map request, and does not advertise `_NET_CLIENT_LIST`.
        jwm.handle_event(&mut backend, BackendEvent::WindowCreated(target_window))
            .unwrap();

        assert!(jwm.state.clients[target].state.is_hidden);
        assert_eq!(jwm.state.sel_mon, source_monitor);
        assert_eq!(jwm.get_selected_client_key(), Some(source));
        assert_eq!(
            backend.property_ops.wm_state.load(AtomicOrdering::Relaxed),
            i64::from(crate::jwm::types::ICONIC_STATE)
        );
        assert!(
            backend
                .property_ops
                .ewmh_hidden
                .load(AtomicOrdering::Relaxed)
        );
        assert!(backend.compositor_minimized_updates.is_empty());
    }

    #[test]
    fn output_change_withdraws_physical_dock_targets_before_relayout() {
        use crate::core::models::WMClient;

        let mut jwm = empty_jwm();
        let mut monitor = jwm.createmon(true);
        monitor.num = 3;
        monitor.geometry.m_w = 1920;
        monitor.geometry.m_h = 1080;
        monitor.geometry.w_w = 1920;
        monitor.geometry.w_h = 1080;
        let monitor_key = jwm.insert_monitor(monitor);
        let output_id = OutputId(77);
        jwm.state.output_map.insert(monitor_key, output_id);
        jwm.state.sel_mon = Some(monitor_key);

        let window = WindowId::from_raw(0x303);
        let mut client = WMClient::new(window);
        client.mon = Some(monitor_key);
        client.state.tags = 1;
        client.state.is_hidden = true;
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, monitor_key);

        jwm.minimized_dock_shelves
            .insert(3, CompositorRect::new(1700.0, 4.0, 180.0, 36.0));
        jwm.active_minimized_preview = Some((3, window));

        let mut backend = RenderSpyBackend::new();
        jwm.handle_output_changed(
            &mut backend,
            OutputInfo {
                id: output_id,
                name: "Virtual-1".into(),
                x: -1280,
                y: 20,
                width: 1280,
                height: 720,
                scale: 1.5,
                refresh_rate: 60_000,
                hdr_capable: false,
                hdr_metadata: None,
                identity: crate::backend::api::OutputIdentity::connector_only("Virtual-1"),
            },
        )
        .unwrap();

        assert!(!jwm.minimized_dock_shelves.contains_key(&3));
        assert_eq!(jwm.active_minimized_preview, None);
        assert_eq!(backend.dock_preview_updates, vec![(None, None)]);
        assert_eq!(backend.dock_geometry_updates, vec![(window, None)]);
        assert!(jwm.pending_bar_updates.contains(&3));
        let geometry = &jwm.state.monitors[monitor_key].geometry;
        assert_eq!(
            (geometry.m_x, geometry.m_y, geometry.m_w, geometry.m_h),
            (-1280, 20, 1280, 720)
        );
    }

    #[test]
    fn failed_bar_cleanup_withdraws_overlays_and_arms_bounded_retry() {
        use crate::core::models::WMClient;

        let mut jwm = empty_jwm();
        let mut monitor = jwm.createmon(true);
        monitor.num = 5;
        let monitor_key = jwm.insert_monitor(monitor);

        let window = WindowId::from_raw(0x505);
        let mut client = WMClient::new(window);
        client.mon = Some(monitor_key);
        client.state.tags = 1;
        client.state.is_hidden = true;
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, monitor_key);
        jwm.minimized_dock_shelves
            .insert(5, CompositorRect::new(10.0, 20.0, 100.0, 30.0));
        jwm.active_minimized_preview = Some((5, window));

        let mut backend = RenderSpyBackend::new();
        let now = std::time::Instant::now();
        jwm.handle_secondary_bar_failure(&mut backend, 5, now, "test crash");

        assert_eq!(backend.dock_preview_updates, vec![(None, None)]);
        assert_eq!(backend.dock_geometry_updates, vec![(window, None)]);
        assert!(!jwm.minimized_dock_shelves.contains_key(&5));
        assert_eq!(jwm.secondary_bar_failures.get(&5), Some(&1));
        assert_eq!(
            jwm.secondary_bar_retry_after.get(&5).copied(),
            now.checked_add(std::time::Duration::from_secs(5))
        );
        assert_eq!(
            jwm.secondary_bar_next_wakeup(now),
            Some(std::time::Duration::from_secs(5))
        );
        assert_eq!(
            jwm.secondary_bar_next_wakeup(now + std::time::Duration::from_secs(5)),
            Some(std::time::Duration::ZERO)
        );
    }

    #[test]
    fn bar_health_deadline_and_sigchld_force_the_same_supervisor_path() {
        let mut jwm = empty_jwm();
        let mut monitor = jwm.createmon(true);
        monitor.num = 5;
        jwm.insert_monitor(monitor);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = format!("/tmp/jwm-bar-health-{}-{nonce}", std::process::id());
        let ring = std::sync::Arc::new(
            xbar_core::shared_structures::SharedRingBufferOptions::new()
                .create(&path)
                .unwrap(),
        );
        let mut child = std::process::Command::new("/bin/true").spawn().unwrap();
        child.wait().unwrap();
        let now = std::time::Instant::now();
        jwm.secondary_bars.insert(
            5,
            crate::jwm::types::SecondaryBarInstance {
                monitor_id: 5,
                shmem: ring,
                command_notifier: None,
                pid: child.id(),
                child,
                client_key: None,
                window: Some(WindowId::from_raw(0x505)),
                has_focus: false,
                last_spawn: now,
                next_health_check: now + std::time::Duration::from_secs(1),
            },
        );
        assert_eq!(
            jwm.secondary_bar_next_wakeup(now),
            Some(std::time::Duration::from_secs(1))
        );
        assert_eq!(
            jwm.secondary_bar_next_wakeup(now + std::time::Duration::from_secs(1)),
            Some(std::time::Duration::ZERO)
        );

        let mut backend = RenderSpyBackend::new();
        jwm.on_child_process_exited(&mut backend);
        assert!(!jwm.secondary_bars.contains_key(&5));
        assert_eq!(jwm.secondary_bar_failures.get(&5), Some(&1));
        assert!(jwm.secondary_bar_retry_after.contains_key(&5));
    }

    #[test]
    fn stopped_bar_notifier_revokes_async_readiness_and_is_removed() {
        let mut jwm = empty_jwm();
        jwm.async_update_notifier =
            Some(crate::backend::update_notifier::AsyncUpdateNotifier::new().unwrap());
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = format!(
            "/tmp/jwm-bar-readiness-health-{}-{nonce}",
            std::process::id()
        );
        let ring = std::sync::Arc::new(
            xbar_core::shared_structures::SharedRingBufferOptions::new()
                .command_capacity(8)
                .adaptive_poll_spins(0)
                .create(&path)
                .unwrap(),
        );
        let notifier =
            xbar_core::SharedEventNotifier::for_commands(std::sync::Arc::clone(&ring), true)
                .unwrap();
        let mut child = std::process::Command::new("/bin/true").spawn().unwrap();
        child.wait().unwrap();
        let now = std::time::Instant::now();
        jwm.secondary_bars.insert(
            5,
            crate::jwm::types::SecondaryBarInstance {
                monitor_id: 5,
                shmem: ring,
                command_notifier: Some(notifier),
                pid: child.id(),
                child,
                client_key: None,
                window: Some(WindowId::from_raw(0x505)),
                has_focus: false,
                last_spawn: now,
                next_health_check: now + std::time::Duration::from_secs(1),
            },
        );

        assert!(<Jwm as EventHandler>::async_update_readiness_healthy(&jwm));
        jwm.secondary_bars[&5]
            .command_notifier
            .as_ref()
            .unwrap()
            .request_shutdown();
        assert!(
            !<Jwm as EventHandler>::async_update_readiness_healthy(&jwm),
            "a stopped bridge must restore the X11 idle safety poll"
        );

        jwm.process_commands_from_status_bar(&mut RenderSpyBackend::new());
        assert!(jwm.secondary_bars[&5].command_notifier.is_none());
    }

    #[test]
    fn orphan_bar_is_retired_before_a_mapping_bar_blocks_creation() {
        let mut jwm = empty_jwm();
        let mut monitor = jwm.createmon(true);
        monitor.num = 0;
        jwm.insert_monitor(monitor);
        let now = std::time::Instant::now();
        for monitor_id in [0, 9] {
            let path = format!("/tmp/jwm-bar-orphan-{}-{monitor_id}", std::process::id());
            let ring = std::sync::Arc::new(
                xbar_core::shared_structures::SharedRingBufferOptions::new()
                    .reclaim_stale(true)
                    .open_or_create(&path)
                    .unwrap(),
            );
            let mut child = std::process::Command::new("/bin/true").spawn().unwrap();
            child.wait().unwrap();
            jwm.secondary_bars.insert(
                monitor_id,
                crate::jwm::types::SecondaryBarInstance {
                    monitor_id,
                    shmem: ring,
                    command_notifier: None,
                    pid: child.id(),
                    child,
                    client_key: None,
                    window: None,
                    has_focus: false,
                    last_spawn: now,
                    next_health_check: now + std::time::Duration::from_secs(1),
                },
            );
        }
        assert_eq!(
            jwm.secondary_bar_next_wakeup(now),
            Some(std::time::Duration::ZERO)
        );

        let mut backend = RenderSpyBackend::new();
        jwm.ensure_secondary_bars_running(&mut backend, now);

        assert!(jwm.secondary_bars.contains_key(&0));
        assert!(!jwm.secondary_bars.contains_key(&9));
        assert!(
            jwm.secondary_bar_next_wakeup(now)
                .is_some_and(|delay| !delay.is_zero())
        );
        jwm.retire_secondary_bar(&mut backend, 0);
    }

    #[test]
    fn non_tail_hotplug_reuses_the_free_monitor_number_without_collision() {
        let mut jwm = empty_jwm();
        let mut first = jwm.createmon(true);
        first.num = 0;
        let first_key = jwm.insert_monitor(first);
        let mut second = jwm.createmon(true);
        second.num = 1;
        let second_key = jwm.insert_monitor(second);
        let first_output = OutputId(10);
        jwm.state.output_map.insert(first_key, first_output);
        jwm.state.output_map.insert(second_key, OutputId(11));
        jwm.state.sel_mon = Some(second_key);

        let mut backend = RenderSpyBackend::new();
        jwm.handle_output_removed(&mut backend, first_output)
            .unwrap();
        jwm.add_monitor(OutputInfo {
            id: OutputId(12),
            name: "Virtual-3".into(),
            x: 1920,
            y: 0,
            width: 1920,
            height: 1080,
            scale: 1.0,
            refresh_rate: 60_000,
            hdr_capable: false,
            hdr_metadata: None,
            identity: crate::backend::api::OutputIdentity::connector_only("Virtual-3"),
        });

        let mut nums: Vec<_> = jwm
            .state
            .monitors
            .values()
            .map(|monitor| monitor.num)
            .collect();
        nums.sort_unstable();
        assert_eq!(nums, vec![0, 1]);
        assert_ne!(jwm.get_monitor_by_id(0), jwm.get_monitor_by_id(1));
        assert!(jwm.get_monitor_by_id(0).is_some());
        assert!(jwm.get_monitor_by_id(1).is_some());
    }

    fn current_layout(jwm: &Jwm) -> crate::core::layout::LayoutEnum {
        let key = jwm.state.sel_mon.unwrap();
        let monitor = jwm.state.monitors.get(key).unwrap();
        (*monitor.lt).clone()
    }

    #[test]
    fn cycling_layouts_opens_the_film_strip_and_keeps_stepping_it() {
        let mut jwm = jwm_with_monitor();
        let mut backend = RenderSpyBackend::new();
        let start = current_layout(&jwm);

        jwm.cyclelayout(&mut backend, &WMArgEnum::Int(1)).unwrap();
        assert!(jwm.features.system_ui.is_layout_picker());
        // A tap still switches the layout, exactly as the silent cycle did.
        assert_eq!(&current_layout(&jwm), start.cycle_next());

        jwm.cyclelayout(&mut backend, &WMArgEnum::Int(1)).unwrap();
        assert!(jwm.features.system_ui.is_layout_picker());
        assert_eq!(&current_layout(&jwm), start.cycle_next().cycle_next());

        // And back the way it came.
        jwm.cyclelayout(&mut backend, &WMArgEnum::Int(-1)).unwrap();
        assert_eq!(&current_layout(&jwm), start.cycle_next());
        assert_eq!(
            jwm.features
                .system_ui
                .layout_picker()
                .unwrap()
                .selected_layout(),
            start.cycle_next()
        );
    }

    #[test]
    fn confirming_keeps_the_browsed_layout_and_closes_the_strip() {
        let mut jwm = jwm_with_monitor();
        let mut backend = RenderSpyBackend::new();
        let start = current_layout(&jwm);

        jwm.cyclelayout(&mut backend, &WMArgEnum::Int(1)).unwrap();
        jwm.confirm_layout_picker(&mut backend);

        assert!(!jwm.features.system_ui.is_active());
        assert_eq!(&current_layout(&jwm), start.cycle_next());
    }

    #[test]
    fn cancelling_puts_back_the_layout_the_picker_opened_on() {
        let mut jwm = jwm_with_monitor();
        let mut backend = RenderSpyBackend::new();
        let start = current_layout(&jwm);

        jwm.cyclelayout(&mut backend, &WMArgEnum::Int(1)).unwrap();
        jwm.cyclelayout(&mut backend, &WMArgEnum::Int(1)).unwrap();
        assert_ne!(current_layout(&jwm), start);

        jwm.cancel_layout_picker(&mut backend);

        assert!(!jwm.features.system_ui.is_active());
        assert_eq!(current_layout(&jwm), start);
    }

    #[test]
    fn the_strip_commits_by_itself_once_browsing_stops() {
        use crate::jwm::features::layout_picker::AUTO_CONFIRM;

        let mut jwm = jwm_with_monitor();
        let mut backend = RenderSpyBackend::new();
        let start = current_layout(&jwm);

        jwm.cyclelayout(&mut backend, &WMArgEnum::Int(1)).unwrap();
        let now = std::time::Instant::now();

        // Still browsing: the panel stays up.
        jwm.tick_layout_picker(&mut backend, now);
        assert!(jwm.features.system_ui.is_layout_picker());

        jwm.features.system_ui.layout_picker_mut().unwrap().touched = now - AUTO_CONFIRM;
        jwm.tick_layout_picker(&mut backend, now);

        assert!(!jwm.features.system_ui.is_active());
        assert_eq!(&current_layout(&jwm), start.cycle_next());
    }

    #[test]
    fn clicking_a_cell_picks_that_layout_and_commits_it() {
        use crate::backend::compositor_common::layout_strip;

        let mut jwm = jwm_with_monitor();
        let mut backend = RenderSpyBackend::new();

        jwm.cyclelayout(&mut backend, &WMArgEnum::Int(0)).unwrap();
        let picker = jwm.features.system_ui.layout_picker().unwrap();
        let target = 4usize;
        let wanted = picker.layouts[target];
        let geometry =
            layout_strip::strip_geometry([0.0, 0.0, 1920.0, 1080.0], picker.layouts.len());
        let [x, y] = layout_strip::center(geometry.cells[target].cell);

        jwm.click_layout_picker(&mut backend, x as f64, y as f64);

        assert!(!jwm.features.system_ui.is_active());
        assert_eq!(&current_layout(&jwm), wanted);
    }

    #[test]
    fn layout_picker_hit_test_uses_the_selected_monitors_global_origin() {
        use crate::backend::compositor_common::layout_strip;

        let mut jwm = jwm_with_monitor();
        let monitor = jwm.state.sel_mon.unwrap();
        jwm.state.monitors[monitor].geometry.m_x = -1600;
        jwm.state.monitors[monitor].geometry.m_y = 120;
        jwm.state.monitors[monitor].geometry.m_w = 1600;
        jwm.state.monitors[monitor].geometry.m_h = 900;
        let mut backend = RenderSpyBackend::new();

        jwm.cyclelayout(&mut backend, &WMArgEnum::Int(0)).unwrap();
        let picker = jwm.features.system_ui.layout_picker().unwrap();
        let target = 5usize;
        let wanted = picker.layouts[target];
        let geometry =
            layout_strip::strip_geometry([-1600.0, 120.0, 1600.0, 900.0], picker.layouts.len());
        let [x, y] = layout_strip::center(geometry.cells[target].cell);

        jwm.click_layout_picker(&mut backend, x as f64, y as f64);

        assert!(!jwm.features.system_ui.is_active());
        assert_eq!(&current_layout(&jwm), wanted);
    }

    #[test]
    fn without_a_compositor_cycling_switches_layouts_silently() {
        let mut jwm = jwm_with_monitor();
        let mut backend = RenderSpyBackend::new();
        backend.compositor_enabled = false;
        backend.compositor_supported = false;
        let start = current_layout(&jwm);

        jwm.cyclelayout(&mut backend, &WMArgEnum::Int(1)).unwrap();

        assert!(!jwm.features.system_ui.is_active());
        assert_eq!(&current_layout(&jwm), start.cycle_next());
    }

    #[test]
    fn tags_overview_opens_on_the_current_tag_and_toggles_back_off() {
        let mut jwm = jwm_with_monitor();
        let mut backend = RenderSpyBackend::new();

        jwm.toggle_tags_overview(&mut backend, &WMArgEnum::Int(0))
            .unwrap();
        let overview = jwm.features.system_ui.tags_overview().unwrap();
        assert_eq!(overview.cells.len(), CONFIG.load().tags_length());
        assert_eq!(
            overview.selected, 0,
            "no active tag bit preselects the first cell"
        );

        // The key that opened it takes it back down.
        jwm.toggle_tags_overview(&mut backend, &WMArgEnum::Int(0))
            .unwrap();
        assert!(!jwm.features.system_ui.is_active());
    }

    #[test]
    fn tags_overview_arrow_walk_and_digit_jump_commit_through_view() {
        let mut jwm = jwm_with_monitor();
        let mut backend = RenderSpyBackend::new();
        let mon_key = jwm.state.sel_mon.unwrap();

        jwm.toggle_tags_overview(&mut backend, &WMArgEnum::Int(0))
            .unwrap();
        jwm.move_tags_overview_selection(
            &mut backend,
            crate::backend::api::ExposeNavDirection::Right,
        );
        jwm.move_tags_overview_selection(
            &mut backend,
            crate::backend::api::ExposeNavDirection::Right,
        );
        assert_eq!(jwm.features.system_ui.tags_overview().unwrap().selected, 2);

        jwm.confirm_tags_overview(&mut backend).unwrap();
        assert!(!jwm.features.system_ui.is_active());
        assert_eq!(jwm.state.monitors[mon_key].get_active_tags(), 1 << 2);

        // A digit jumps straight to its tag and commits in one press.
        jwm.toggle_tags_overview(&mut backend, &WMArgEnum::Int(0))
            .unwrap();
        jwm.jump_tags_overview(&mut backend, 4).unwrap();
        assert!(!jwm.features.system_ui.is_active());
        assert_eq!(jwm.state.monitors[mon_key].get_active_tags(), 1 << 4);
    }

    #[test]
    fn tags_overview_cancel_leaves_the_current_tag_untouched() {
        let mut jwm = jwm_with_monitor();
        let mut backend = RenderSpyBackend::new();
        let mon_key = jwm.state.sel_mon.unwrap();

        jwm.toggle_tags_overview(&mut backend, &WMArgEnum::Int(0))
            .unwrap();
        jwm.move_tags_overview_selection(
            &mut backend,
            crate::backend::api::ExposeNavDirection::Down,
        );
        jwm.cancel_tags_overview(&mut backend);

        assert!(!jwm.features.system_ui.is_active());
        assert_eq!(
            jwm.state.monitors[mon_key].get_active_tags(),
            0,
            "cancel must not view anything"
        );
    }

    #[test]
    fn arrange_rebuilds_the_open_overviews_cells() {
        use crate::core::models::WMClient;

        let mut jwm = empty_jwm();
        // insert_monitor (unlike a bare SlotMap insert) creates the
        // per-monitor client vectors the snapshot collects from.
        let mut monitor = jwm.createmon(true);
        monitor.geometry.m_w = 1920;
        monitor.geometry.m_h = 1080;
        monitor.geometry.w_w = 1920;
        monitor.geometry.w_h = 1080;
        let mon_key = jwm.insert_monitor(monitor);
        jwm.state.sel_mon = Some(mon_key);
        jwm.s_w = 1920;
        jwm.s_h = 1080;
        let mut backend = RenderSpyBackend::new();

        jwm.toggle_tags_overview(&mut backend, &WMArgEnum::Int(0))
            .unwrap();
        assert!(
            !jwm.features.system_ui.tags_overview().unwrap().cells[0].occupied,
            "an empty monitor opens an all-empty grid"
        );

        let mut client = WMClient::new(WindowId::from_raw(0xfeed));
        client.mon = Some(mon_key);
        client.state.tags = 1;
        client.geometry.w = 800;
        client.geometry.h = 600;
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, mon_key);

        // The arrange tail rebuilds the open panel's cells; the flush then
        // repushes the overlay. Neither touches the highlight.
        jwm.system_ui_dirty = false;
        jwm.arrange(&mut backend, Some(mon_key));

        let overview = jwm.features.system_ui.tags_overview().unwrap();
        assert!(overview.cells[0].occupied);
        assert_eq!(overview.cells[0].windows.len(), 1);
        assert!(jwm.system_ui_dirty, "the rebuild must request a repaint");

        jwm.close_system_ui(&mut backend);
        assert!(!jwm.system_ui_dirty);
    }

    #[test]
    fn native_layout_picker_leases_and_restores_the_compositor() {
        let mut jwm = jwm_with_monitor();
        let mut backend = RenderSpyBackend::new();
        backend.compositor_enabled = false;
        let start = current_layout(&jwm);

        jwm.cyclelayout(&mut backend, &WMArgEnum::Int(1)).unwrap();

        assert!(jwm.features.system_ui.is_layout_picker());
        assert!(jwm.features.system_ui_temporary_compositor);
        assert_eq!(backend.compositor_transitions, [true]);
        assert_eq!(&current_layout(&jwm), start.cycle_next());

        jwm.confirm_layout_picker(&mut backend);
        assert!(!backend.compositor_enabled);
        assert!(!jwm.features.system_ui_temporary_compositor);
        assert_eq!(backend.compositor_transitions, [true, false]);
    }

    #[test]
    fn hidden_state_requests_are_idempotent_and_toggle_current_state() {
        assert!(requested_hidden_state(NetWmAction::Add, false));
        assert!(requested_hidden_state(NetWmAction::Add, true));
        assert!(!requested_hidden_state(NetWmAction::Remove, true));
        assert!(!requested_hidden_state(NetWmAction::Remove, false));
        assert!(requested_hidden_state(NetWmAction::Toggle, false));
        assert!(!requested_hidden_state(NetWmAction::Toggle, true));
    }

    #[test]
    fn attention_state_requests_are_idempotent_and_toggle_current_state() {
        assert!(requested_attention_state(NetWmAction::Add, false));
        assert!(requested_attention_state(NetWmAction::Add, true));
        assert!(!requested_attention_state(NetWmAction::Remove, true));
        assert!(!requested_attention_state(NetWmAction::Remove, false));
        assert!(requested_attention_state(NetWmAction::Toggle, false));
        assert!(!requested_attention_state(NetWmAction::Toggle, true));
    }

    #[test]
    fn removing_skip_taskbar_statically_adopts_hidden_window_once() {
        use crate::core::models::WMClient;

        let mut jwm = jwm_with_monitor();
        let mut backend = RenderSpyBackend::new();
        let monitor = jwm.state.monitor_order[0];
        let monitor_num = jwm.state.monitors[monitor].num;
        let shelf = CompositorRect::new(30.0, 700.0, 48.0, 48.0);
        jwm.minimized_dock_shelves.insert(monitor_num, shelf);

        let window = WindowId::from_raw(0x4242);
        let mut client = WMClient::new(window);
        client.mon = Some(monitor);
        client.state.tags = 1;
        client.state.is_hidden = true;
        client.state.skip_taskbar = true;
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, monitor);

        jwm.on_window_state_request(
            &mut backend,
            window,
            NetWmAction::Remove,
            NetWmState::SkipTaskbar,
        );
        assert_eq!(backend.dock_geometry_updates, vec![(window, Some(shelf))]);
        assert_eq!(backend.compositor_static_ensures, vec![window]);

        jwm.on_window_state_request(
            &mut backend,
            window,
            NetWmAction::Remove,
            NetWmState::SkipTaskbar,
        );
        assert_eq!(backend.dock_geometry_updates.len(), 1);
        assert_eq!(backend.compositor_static_ensures.len(), 1);
    }

    #[test]
    fn adding_skip_taskbar_forgets_hidden_visual_without_restoring_the_client() {
        use crate::core::models::WMClient;

        let mut jwm = jwm_with_monitor();
        let mut backend = RenderSpyBackend::new();
        let monitor = jwm.state.monitor_order[0];
        let monitor_num = jwm.state.monitors[monitor].num;
        let shelf = CompositorRect::new(30.0, 700.0, 48.0, 48.0);
        jwm.minimized_dock_shelves.insert(monitor_num, shelf);

        let window = WindowId::from_raw(0x4343);
        let mut client = WMClient::new(window);
        client.mon = Some(monitor);
        client.state.tags = 1;
        client.state.is_hidden = true;
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, monitor);

        jwm.on_window_state_request(
            &mut backend,
            window,
            NetWmAction::Add,
            NetWmState::SkipTaskbar,
        );
        assert!(jwm.state.clients[client_key].state.is_hidden);
        assert!(jwm.state.clients[client_key].state.skip_taskbar);
        assert_eq!(backend.compositor_forgotten_visuals, vec![window]);
        assert!(backend.compositor_minimized_updates.is_empty());
        assert_eq!(backend.dock_geometry_updates, vec![(window, None)]);

        // The ineligible retirement path is intentionally repeatable: an
        // earlier checked map/attribute confirmation may have failed after
        // the property bit committed. Repeating Add safely retries target
        // withdrawal and visual retirement; removing the bit then follows the
        // static geometry-before-ensure adoption path.
        jwm.on_window_state_request(
            &mut backend,
            window,
            NetWmAction::Add,
            NetWmState::SkipTaskbar,
        );
        assert_eq!(backend.compositor_forgotten_visuals, vec![window, window]);

        jwm.on_window_state_request(
            &mut backend,
            window,
            NetWmAction::Remove,
            NetWmState::SkipTaskbar,
        );
        assert!(jwm.state.clients[client_key].state.is_hidden);
        assert!(!jwm.state.clients[client_key].state.skip_taskbar);
        assert_eq!(
            backend.dock_geometry_updates,
            vec![(window, None), (window, None), (window, Some(shelf))]
        );
        assert_eq!(backend.compositor_static_ensures, vec![window]);
        assert!(backend.compositor_minimized_updates.is_empty());
    }

    #[test]
    fn attention_state_requests_sync_the_client_and_compositor() {
        use crate::core::models::WMClient;

        let mut jwm = empty_jwm();
        let mut backend = RenderSpyBackend::new();
        let window = WindowId::from_raw(42);
        let client = jwm.insert_client(WMClient::new(window));

        for (action, expected) in [
            (NetWmAction::Add, true),
            (NetWmAction::Add, true),
            (NetWmAction::Remove, false),
            (NetWmAction::Toggle, true),
            (NetWmAction::Toggle, false),
        ] {
            jwm.on_window_state_request(&mut backend, window, action, NetWmState::DemandsAttention);

            let state = &jwm.state.clients[client].state;
            assert_eq!(state.demands_attention, expected);
            assert_eq!(state.is_urgent, expected);
            assert_eq!(backend.compositor_urgency.last(), Some(&(window, expected)));
        }
        assert_eq!(backend.compositor_urgency.len(), 5);
    }

    #[test]
    fn icccm_seturgent_is_the_client_and_compositor_sync_point() {
        use crate::core::models::WMClient;

        let mut jwm = empty_jwm();
        let mut backend = RenderSpyBackend::new();
        let window = WindowId::from_raw(43);
        let client = jwm.insert_client(WMClient::new(window));

        jwm.seturgent(&mut backend, client, true).unwrap();
        assert!(jwm.state.clients[client].state.is_urgent);
        assert_eq!(backend.compositor_urgency, vec![(window, true)]);

        jwm.seturgent(&mut backend, client, false).unwrap();
        assert!(!jwm.state.clients[client].state.is_urgent);
        assert_eq!(
            backend.compositor_urgency,
            vec![(window, true), (window, false)]
        );
    }
}

// =================================================================================
// EventHandler trait 实现 - 事件循环主处理器
// =================================================================================
impl Jwm {
    fn background_job_readiness_is_complete(&self) -> bool {
        use crate::jwm::features::connectivity::BackgroundJob;

        self.features
            .control_snapshot_job
            .as_ref()
            .is_none_or(BackgroundJob::readiness_is_covered)
            && self
                .features
                .connectivity_poll
                .as_ref()
                .is_none_or(BackgroundJob::readiness_is_covered)
            && self
                .features
                .wifi_scan
                .as_ref()
                .is_none_or(BackgroundJob::readiness_is_covered)
            && self
                .features
                .wifi_connect
                .as_ref()
                .is_none_or(BackgroundJob::readiness_is_covered)
            && self
                .features
                .bluetooth_scan
                .as_ref()
                .is_none_or(BackgroundJob::readiness_is_covered)
            && self
                .features
                .bluetooth_action
                .as_ref()
                .is_none_or(BackgroundJob::readiness_is_covered)
            && self
                .features
                .wallpaper_theme
                .as_ref()
                .is_none_or(BackgroundJob::readiness_is_covered)
            && self
                .features
                .launcher_catalog_job
                .as_ref()
                .is_none_or(BackgroundJob::readiness_is_covered)
    }

    fn maintenance_next_wakeup_at(&self, now: std::time::Instant) -> std::time::Duration {
        let mut next = Some(self.config_reload_next_wakeup(now));
        if let Some(picker) = self.layout_picker_wakeup(now) {
            next = min_optional_duration(next, Some(picker.min(FRAME_INTERVAL)));
        }
        next = min_optional_duration(next, self.hidden_client_park_retry_next_wakeup(now));
        if self.has_deferred_grab() {
            next = min_optional_duration(next, Some(crate::jwm::features::deferred_grab::RETRY));
        }
        next = min_optional_duration(next, self.transient_child_next_wakeup(now));
        next = min_optional_duration(next, self.scratchpad_pending.next_wakeup(now));
        next = min_optional_duration(next, self.layout_persist_next_wakeup(now));
        next = min_optional_duration(next, self.secondary_bar_next_wakeup(now));
        next = min_optional_duration(next, self.ping_next_wakeup(now));
        next = min_optional_duration(next, self.idle_next_wakeup(now));
        next = min_optional_duration(next, self.resources_next_wakeup(now));
        next = min_optional_duration(next, Some(self.battery_next_wakeup(now)));
        next.expect("config reload always supplies a maintenance deadline")
    }
}

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

                // A hidden client that is deliberately absent from the Dock
                // can be remapped by recovery from X11 UnmapGravity.  The
                // compositor observes MapNotify before this JWM callback and
                // may therefore have imported a new live pixmap even though
                // no shelf/preview can address it. Retire that late import
                // without changing the client's hidden state or playing a
                // reverse Genie transition.
                if self.wintoclient(win).is_some_and(|client_key| {
                    self.state.clients.get(client_key).is_some_and(|client| {
                        client.state.is_hidden
                            && !StatusBarBuilder::is_minimized_dock_eligible(client)
                    })
                }) {
                    backend.compositor_forget_minimized_window_visual(win);
                }
            }
            BackendEvent::WindowUnmapped {
                window,
                from_configure,
            } => self.on_unmap_notify(backend, window, from_configure),
            BackendEvent::WindowManagerUnmapped { .. } => {
                // The X11 transport has already correlated this with a checked
                // JWM request. The client remains managed; compositor resource
                // policy is applied before dispatch by the shared event bridge.
            }
            BackendEvent::WindowConfigured {
                window,
                x,
                y,
                width,
                height,
                ..
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
                if let Err(error) = self.reveal_and_focus(backend, win) {
                    error!("Error activating foreign toplevel {win:?}: {error:?}");
                }
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
            BackendEvent::ForeignToplevelSetMinimized(win, minimized) => {
                if let Some(ck) = self.wintoclient(win) {
                    if let Err(error) =
                        apply_external_minimized_request(self, backend, ck, win, minimized)
                    {
                        error!(
                            "Could not apply foreign-toplevel minimized state for {win:?}: {error}"
                        );
                    }
                }
            }
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
        let now = std::time::Instant::now();
        // Clear the counted completion level before inspecting any worker
        // queues. A worker that publishes after this point leaves the eventfd
        // readable for a follow-up update, so no completion can be swallowed
        // by a drain that ran after its queue was already checked.
        if let Some(notifier) = self.async_update_notifier.as_ref()
            && let Err(error) = notifier.drain()
        {
            log::warn!("could not drain async update notifier; restoring timer fallback: {error}");
        }
        // Backends without SIGCHLD still reap exact child handles, but the
        // supervisor rate-limits this insurance path instead of issuing one
        // wait syscall per live application on every frame/update tick.
        self.poll_transient_children(now);

        // Ensure all monitor bars are running (sequential creation)
        self.expire_pending_scratchpads(now);
        self.tick_hidden_client_park_retries(backend, now);
        self.ensure_secondary_bars_running(backend, now);

        self.process_commands_from_status_bar(backend);
        self.process_ipc(backend);
        self.poll_config_reload(backend, now);
        // After the reload poll: a pending edit of the user's gets to land
        // before JWM writes its own per-tag layouts over the same file.
        self.flush_layout_persistence(now);
        self.flush_pending_bar_updates();
        // The layout picker commits on its own once the user stops browsing.
        // Ahead of the animation tick, whose panel flush then carries the
        // countdown's new position out in the same iteration.
        self.tick_layout_picker(backend, now);

        self.tick_animations(backend);

        // A request the pointer was busy for retries here rather than from a
        // sleep, so waiting for a status bar's click to end costs no frames.
        self.tick_deferred_grab(backend, now);

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

        // Individual level sources above have now been consumed. Drain the
        // stable aggregate last, otherwise epoll can immediately requeue a
        // still-readable child and cause one redundant handler update.
        if let Some(readiness) = self.update_readiness.as_mut()
            && let Err(error) = readiness.drain()
        {
            if let Some(notifier) = self.async_update_notifier.as_ref() {
                notifier.mark_unhealthy();
            }
            log::warn!("could not drain update readiness hub; restoring timer fallback: {error}");
        }

        backend.window_ops().flush()?;
        Ok(())
    }

    fn should_exit(&self) -> bool {
        // 检查原子布尔值
        !self.running.load(Ordering::SeqCst)
    }

    fn needs_tick(&self) -> bool {
        let now = std::time::Instant::now();
        self.animations.has_active()
            || self.features.overview.active
            || self.features.expose_active
            || self.features.system_ui.is_layout_picker()
            || self.has_deferred_grab()
            || self.maintenance_next_wakeup_at(now).is_zero()
    }

    fn next_wakeup(&self) -> Option<std::time::Duration> {
        Some(self.maintenance_next_wakeup_at(std::time::Instant::now()))
    }

    fn duplicate_update_readiness_fd(&self) -> Option<std::os::fd::OwnedFd> {
        if let Some(readiness) = self.update_readiness.as_ref() {
            match readiness.duplicate_fd() {
                Ok(fd) => return Some(fd),
                Err(error) => {
                    log::warn!("could not duplicate update readiness hub fd: {error}");
                }
            }
        }
        let ipc = self.ipc_server.as_ref()?;
        match ipc.duplicate_readiness_fd() {
            Ok(fd) => fd,
            Err(error) => {
                log::warn!("[ipc] could not duplicate update readiness fd: {error}");
                None
            }
        }
    }

    fn async_update_notifier(
        &self,
    ) -> Option<crate::backend::update_notifier::AsyncUpdateNotifier> {
        self.async_update_notifier.clone()
    }

    fn async_update_readiness_healthy(&self) -> bool {
        self.async_update_notifier
            .as_ref()
            .is_some_and(crate::backend::update_notifier::AsyncUpdateNotifier::is_healthy)
            && self
                .ipc_server
                .as_ref()
                .is_none_or(crate::ipc_server::IpcServer::readiness_is_healthy)
            && self
                .secondary_bars
                .values()
                .all(crate::jwm::types::SecondaryBarInstance::command_readiness_is_healthy)
            && self.background_job_readiness_is_complete()
    }

    fn render_compositor_immediate(&mut self, backend: &mut dyn Backend) {
        self.render_pending_frame(backend);
    }
}

impl Jwm {
    fn ping_next_wakeup(&self, now: std::time::Instant) -> Option<std::time::Duration> {
        ping_schedule_next_wakeup(
            self.last_ping_time,
            self.get_selected_client_key().is_some(),
            self.pending_pings.values().copied(),
            now,
        )
    }

    fn tick_ping_check(&mut self, backend: &mut dyn Backend, now: std::time::Instant) {
        let timed_out: Vec<_> = self
            .pending_pings
            .iter()
            .filter(|(_, sent_at)| now.saturating_duration_since(**sent_at) >= PING_TIMEOUT)
            .map(|(win, _)| *win)
            .collect();
        for win in timed_out {
            self.pending_pings.remove(&win);
            self.unresponsive_windows.insert(win);
        }

        let Some(sel) = self.get_selected_client_key() else {
            return;
        };
        let should_ping = self
            .last_ping_time
            .map(|t| now.saturating_duration_since(t) >= PING_INTERVAL)
            .unwrap_or(true);
        if !should_ping {
            return;
        }
        self.last_ping_time = Some(now);

        let win = match self.state.clients.get(sel) {
            Some(c) => c.win,
            None => return,
        };
        if let std::collections::hash_map::Entry::Vacant(entry) = self.pending_pings.entry(win) {
            let ts = now.elapsed().subsec_millis();
            if let Ok(true) = backend.property_ops().send_ping(win, ts) {
                entry.insert(now);
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
