pub mod types;
pub mod strut_manager;
pub mod client;
pub mod focus;
pub mod monitor;
pub mod constraints;
pub mod client_stack;
pub mod features;
pub mod geometry;
pub mod rules;
pub mod statusbar;
pub mod layout;

pub use types::{
    WMButton, WMKey, WMRule, WMWindowGeom, WMClickType, WMArgEnum, InteractionAction,
    InteractionState, SecondaryBarInstance, WMFuncType, MonitorIndex,
    WITHDRAWN_STATE, STEXT_MAX_LEN, NORMAL_STATE, ICONIC_STATE,
};

pub use features::{
    FeatureStates, MagnifierState, OverviewState, RecordingState, ScreenshotState,
};

pub use geometry::GeometryConstraints;
pub use rules::{RuleApplication, RuleMatcher};
pub use statusbar::{StatusBarBuilder, StatusBarUpdateManager};

use libc::{SIG_DFL, SIGCHLD, setsid, sigaction, sigemptyset};

use log::info;
use log::warn;
use log::{debug, error};

use nix::sys::signal::{self, Signal};
use nix::sys::wait::WaitPidFlag;
use nix::sys::wait::WaitStatus;
use nix::sys::wait::waitpid;
use nix::unistd::Pid;

use crate::backend::api::EventHandler;
use crate::backend::api::HitTarget;
use crate::backend::api::ResizeEdge;
use crate::backend::common_define::OutputId;
use crate::backend::common_define::WindowId;
use crate::backend::error::BackendError;
use crate::core::controller::WMController;
use crate::core::models::MonitorGeometry;
use crate::core::state::WMState;
use slotmap::SecondaryMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::env;
use std::process::Stdio;
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use std::usize;

use crate::backend::api::AllowMode;
use crate::backend::api::Backend;
use crate::backend::api::BackendEvent;
use crate::backend::api::Geometry;
use crate::backend::api::NetWmAction;
use crate::backend::api::NetWmState;
use crate::backend::api::PropertyKind;
use crate::backend::api::StackMode;
use crate::backend::api::StrutPartial;
use crate::backend::api::WindowChanges;
use crate::backend::api::WindowType;
use crate::backend::common_define::ArgbColor;
use crate::backend::common_define::ColorScheme;
use crate::backend::common_define::ConfigWindowBits;
use crate::backend::common_define::EventMaskBits;
use crate::backend::common_define::SchemeType;
use crate::backend::common_define::keys;
use crate::backend::common_define::{KeySym, Mods, MouseButton, StdCursorKind};
use crate::config::CONFIG;
use crate::core::layout::LayoutEnum;
use crate::core::models::{
    ClientKey, MonitorKey, Pertag, ScrollingState, SizeHints, WMClient, WMMonitor,
};
use crate::ipc::{
    self, IpcEvent, IpcResponse, MonitorInfoIpc, TreeNode, WindowInfo, WorkspaceInfo,
};
use crate::ipc_server::{IncomingIpc, IpcServer};

use crate::core::animation::{AnimationKind, AnimationManager};
use crate::core::types::Rect;
use shared_structures::CommandType;
use shared_structures::SharedCommand;
use shared_structures::{MonitorInfo, SharedMessage, SharedRingBuffer, TagStatus};

lazy_static::lazy_static! {
    pub static ref BUTTONMASK: EventMaskBits  = EventMaskBits::BUTTON_PRESS | EventMaskBits::BUTTON_RELEASE;
    pub static ref MOUSEMASK: EventMaskBits   = EventMaskBits::BUTTON_PRESS | EventMaskBits::BUTTON_RELEASE | EventMaskBits::POINTER_MOTION;
}

pub struct Jwm {
    // 纯状态数据
    pub state: WMState,

    pub s_w: i32,
    pub s_h: i32,
    pub running: AtomicBool,
    pub is_restarting: AtomicBool,
    pub last_mouse_root: (f64, f64),

    pub message: SharedMessage,

    // Per-monitor status bars
    pub secondary_bars: HashMap<i32, SecondaryBarInstance>,

    pub last_key_grab_refresh_at: Option<std::time::Instant>,

    pub pending_bar_updates: HashSet<MonitorIndex>,

    pub suppress_mouse_focus_until: Option<std::time::Instant>,
    /// When true, resizeclient() skips layout animations (used during tag
    /// switch transitions so target windows appear instantly).
    pub suppress_layout_animation: bool,

    pub last_stacking: SecondaryMap<MonitorKey, Vec<WindowId>>,

    pub scratchpads: HashMap<String, ClientKey>,
    pub scratchpad_pending_name: Option<String>,

    pub animations: AnimationManager,

    key_bindings: Vec<WMKey>,

    /// Strut reservations from external panels (polybar, trayer, etc.)
    external_struts: HashMap<WindowId, StrutPartial>,

    // IPC
    pub ipc_server: Option<IpcServer>,

    // Config hot-reload
    pub config_last_modified: Option<std::time::SystemTime>,
    pub config_reload_debounce: Option<std::time::Instant>,

    /// Override-redirect windows (menus, tooltips, dmenu, etc.) that are
    /// currently mapped.  These are not managed by the WM but must be rendered
    /// by the compositor when COMPOSITE_REDIRECT_MANUAL is active.
    pub override_redirect_windows: HashSet<WindowId>,

    /// Cached geometries for override-redirect windows.  Updated from
    /// ConfigureNotify so that `build_compositor_scene` doesn't need
    /// synchronous GetGeometry round-trips on every frame.
    pub or_window_geometries: HashMap<WindowId, (i32, i32, u32, u32)>,

    /// Per-monitor scrolling layout state
    pub scrolling_states: HashMap<MonitorKey, ScrollingState>,

    /// Night light: last time we updated color temperature
    pub last_night_light_update: Option<std::time::Instant>,

    /// 所有特殊功能的状态（截图、overview、录制、放大镜等）
    pub features: FeatureStates,
}

// =================================================================================
// 1. 实现 WMController
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
            if let Some(&old) = self.or_window_geometries.get(&win) {
                if old != (x, y, width, height) {
                    info!(
                        "[or_geom_update] win={:?} ({},{} {}x{}) -> ({},{} {}x{})",
                        win, old.0, old.1, old.2, old.3, x, y, width, height
                    );
                }
            }
            self.or_window_geometries.insert(win, (x, y, width, height));
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
// _NET_WM_MOVERESIZE & Strut helpers
// =================================================================================
impl Jwm {
    fn on_moveresize_request(&mut self, backend: &mut dyn Backend, win: WindowId, direction: u32) {
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

    fn render_compositor_immediate(&mut self, backend: &mut dyn Backend) {
        if !backend.has_compositor() {
            return;
        }
        // Skip if animations are active — tick_animations handles rendering
        // during animation frames, so we don't want to double-render.
        if self.animations.has_active() {
            return;
        }
        // When overview is active the prism rotation runs inside the render
        // pass (tick_overview_prism), but clear_needs_render() after
        // render_frame() wipes the flag it sets.  So we must keep rendering
        // every frame unconditionally while overview is up; vsync provides
        // natural ~60 fps pacing.
        if !backend.compositor_needs_render() && !self.features.overview.active {
            return;
        }
        let scene = self.build_compositor_scene(backend, &HashMap::new());
        let groups = self.build_window_groups();
        backend.compositor_set_window_groups(groups);
        let focused = self
            .get_selected_client_key()
            .and_then(|ck| self.state.clients.get(ck))
            .map(|c| c.win.raw());
        let _ = backend.compositor_render_frame(&scene, focused);
    }
}

impl Jwm {
    fn enable_floating_keep_geometry(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(sel_mon_key) = self.state.sel_mon else {
            return Ok(());
        };

        if let Some(client) = self.state.clients.get_mut(client_key) {
            if !client.state.is_floating {
                client.state.is_floating = true;
                client.geometry.floating_x = client.geometry.x;
                client.geometry.floating_y = client.geometry.y;
                client.geometry.floating_w = client.geometry.w;
                client.geometry.floating_h = client.geometry.h;
            }
        }

        self.reorder_client_in_monitor_groups(client_key);

        self.arrange(backend, Some(sel_mon_key));
        Ok(())
    }
    fn debug_drag_enabled() -> bool {
        std::env::var("JWM_DEBUG_DRAG")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(true)
    }

    fn func_name(func: WMFuncType) -> &'static str {
        macro_rules! eq {
            ($f:path) => {
                std::ptr::fn_addr_eq(func, $f as WMFuncType)
            };
        }

        if eq!(Jwm::spawn) {
            "spawn"
        } else if eq!(Jwm::focusstack) {
            "focusstack"
        } else if eq!(Jwm::focusmon) {
            "focusmon"
        } else if eq!(Jwm::take_screenshot) {
            "take_screenshot"
        } else if eq!(Jwm::quit) {
            "quit"
        } else if eq!(Jwm::restart) {
            "restart"
        } else if eq!(Jwm::killclient) {
            "killclient"
        } else if eq!(Jwm::zoom) {
            "zoom"
        } else if eq!(Jwm::setlayout) {
            "setlayout"
        } else if eq!(Jwm::togglefloating) {
            "togglefloating"
        } else if eq!(Jwm::togglebar) {
            "togglebar"
        } else if eq!(Jwm::setmfact) {
            "setmfact"
        } else if eq!(Jwm::setcfact) {
            "setcfact"
        } else if eq!(Jwm::incnmaster) {
            "incnmaster"
        } else if eq!(Jwm::movestack) {
            "movestack"
        } else if eq!(Jwm::view) {
            "view"
        } else if eq!(Jwm::tag) {
            "tag"
        } else if eq!(Jwm::toggleview) {
            "toggleview"
        } else if eq!(Jwm::toggletag) {
            "toggletag"
        } else if eq!(Jwm::tagmon) {
            "tagmon"
        } else if eq!(Jwm::loopview) {
            "loopview"
        } else if eq!(Jwm::movemouse) {
            "movemouse"
        } else if eq!(Jwm::resizemouse) {
            "resizemouse"
        } else if eq!(Jwm::togglesticky) {
            "togglesticky"
        } else if eq!(Jwm::togglescratchpad) {
            "togglescratchpad"
        } else if eq!(Jwm::togglepip) {
            "togglepip"
        } else if eq!(Jwm::toggle_overview) {
            "toggle_overview"
        } else if eq!(Jwm::cycle_overview) {
            "cycle_overview"
        } else if eq!(Jwm::toggle_magnifier) {
            "toggle_magnifier"
        } else if eq!(Jwm::toggle_peek) {
            "toggle_peek"
        } else if eq!(Jwm::toggle_expose) {
            "toggle_expose"
        } else if eq!(Jwm::toggle_recording) {
            "toggle_recording"
        } else {
            "<unknown>"
        }
    }

    fn maybe_clamp_override_redirect_notification(
        &mut self,
        backend: &mut dyn Backend,
        win: WindowId,
    ) {
        let attr = match backend.window_ops().get_window_attributes(win) {
            Ok(a) => a,
            Err(_) => return,
        };
        if !attr.override_redirect {
            return;
        }

        // Avoid meddling with regular menus/tooltips unless we're confident.
        let types = backend.property_ops().get_window_types(win);
        let (inst, cls) = backend.property_ops().get_class(win);
        let title = backend.property_ops().get_title(win);

        let is_dunst = title == "Dunst"
            || inst.eq_ignore_ascii_case("dunst")
            || cls.eq_ignore_ascii_case("dunst")
            || inst.eq_ignore_ascii_case("dunstify")
            || cls.eq_ignore_ascii_case("dunstify");
        let is_notification = types.contains(&WindowType::Notification) || is_dunst;
        if !is_notification {
            return;
        }

        let geom = match backend.window_ops().get_geometry(win) {
            Ok(g) => g,
            Err(_) => return,
        };

        // Find the monitor by window center (fallback to selected monitor).
        let cx = geom.x.saturating_add((geom.w as i32) / 2);
        let cy = geom.y.saturating_add((geom.h as i32) / 2);
        let mon_key = self.recttomon(backend, cx, cy).or(self.state.sel_mon);
        let Some(mon_key) = mon_key else {
            return;
        };

        // Skip windows that cover most of the monitor (e.g. screenshot overlays
        // like Feishu/Lark that set _NET_WM_WINDOW_TYPE_NOTIFICATION).
        // Real notifications are small; full-screen overlays must not be clamped.
        // Compare against the monitor size, not the virtual screen, so that
        // per-monitor overlays in multi-monitor setups are correctly skipped.
        let (mon_w, mon_h) = self
            .state
            .monitors
            .get(mon_key)
            .map(|m| (m.geometry.m_w as u32, m.geometry.m_h as u32))
            .unwrap_or((self.s_w as u32, self.s_h as u32));
        if geom.w >= mon_w.saturating_sub(4) && geom.h >= mon_h.saturating_sub(4) {
            return;
        }

        let work = match self.monitor_work_area(mon_key) {
            Some(r) => r,
            None => return,
        };

        let w = geom.w as i32;
        let h = geom.h as i32;
        let mut new_x = geom.x;
        let mut new_y = geom.y;

        // Clamp to workarea bounds.
        let min_x = work.x;
        let max_x = work.x + work.w - w;
        new_x = if min_x <= max_x {
            new_x.clamp(min_x, max_x)
        } else {
            min_x
        };

        let min_y = work.y;
        let max_y = work.y + work.h - h;
        new_y = if min_y <= max_y {
            new_y.clamp(min_y, max_y)
        } else {
            min_y
        };

        if new_x == geom.x && new_y == geom.y {
            return;
        }

        let changes = WindowChanges {
            x: Some(new_x),
            y: Some(new_y),
            ..Default::default()
        };
        if let Err(e) = backend.window_ops().apply_window_changes(win, changes) {
            debug!(
                "Failed to clamp override_redirect notification win={:?}: {:?}",
                win, e
            );
        }
    }

    pub fn new(backend: &mut dyn Backend) -> Result<Self, Box<dyn std::error::Error>> {
        info!("[new] Starting JWM initialization");
        Self::log_x11_environment();
        backend.cursor_provider().preload_common()?;
        let si = backend.output_ops().screen_info();
        let s_w = si.width;
        let s_h = si.height;
        info!(
            "[new] Screen info - resolution: {}x{}, root: {:?}",
            s_w,
            s_h,
            backend.root_window()
        );
        let alloc = backend.color_allocator();
        let colors = crate::config::CONFIG.load().colors().clone();
        alloc.set_scheme(
            SchemeType::Norm,
            ColorScheme::new(
                ArgbColor::from_hex(&colors.dark_sea_green1, colors.opaque)?,
                ArgbColor::from_hex(&colors.light_sky_blue1, colors.opaque)?,
                ArgbColor::from_hex(&colors.light_sky_blue1, colors.opaque)?,
            ),
        );
        alloc.set_scheme(
            SchemeType::Sel,
            ColorScheme::new(
                ArgbColor::from_hex(&colors.dark_sea_green2, colors.opaque)?,
                ArgbColor::from_hex(&colors.pale_turquoise1, colors.opaque)?,
                ArgbColor::from_hex(&colors.cyan, colors.opaque)?,
            ),
        );
        backend.color_allocator().allocate_schemes_pixels()?;
        info!("[new] JWM initialization completed successfully");
        let outputs = backend.output_ops().enumerate_outputs();
        let mut jwm = Jwm {
            state: WMState::new(),

            s_w,
            s_h,
            running: AtomicBool::new(true),
            is_restarting: AtomicBool::new(false),

            message: SharedMessage::default(),

            secondary_bars: HashMap::new(),

            last_key_grab_refresh_at: None,
            pending_bar_updates: HashSet::new(),

            suppress_mouse_focus_until: None,
            suppress_layout_animation: false,

            last_stacking: SecondaryMap::new(),
            scratchpads: HashMap::new(),
            scratchpad_pending_name: None,
            animations: AnimationManager::new(),
            key_bindings: CONFIG.load().get_keys(),
            external_struts: HashMap::new(),
            last_mouse_root: (0.0, 0.0),

            ipc_server: match IpcServer::new() {
                Ok(s) => Some(s),
                Err(e) => {
                    warn!("[ipc] failed to start IPC server: {e}");
                    None
                }
            },
            config_last_modified: crate::config::Config::get_config_modified_time().ok(),
            config_reload_debounce: None,
            override_redirect_windows: HashSet::new(),
            or_window_geometries: HashMap::new(),
            scrolling_states: HashMap::new(),
            last_night_light_update: None,
            features: FeatureStates::new(),
        };
        if let Ok((x, y)) = backend.input_ops().get_pointer_position() {
            jwm.last_mouse_root = (x, y);
        }
        for out in outputs {
            jwm.add_monitor(out);
        }
        if !jwm.state.monitor_order.is_empty() {
            jwm.state.sel_mon = Some(jwm.state.monitor_order[0]);
        }
        Ok(jwm)
    }

    // --- 热插拔处理逻辑 ---

    pub fn setup_initial_windows(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // 只有后端支持扫描才执行 (X11)
        if let Ok(windows) = backend.window_ops().scan_windows() {
            info!("[setup_initial_windows] Scanning {} windows", windows.len());
            for win in windows {
                // Windows may be destroyed between scan and query; skip on error.
                let attr = match backend.window_ops().get_window_attributes(win) {
                    Ok(a) => a,
                    Err(_) => continue,
                };
                if !attr.override_redirect && attr.map_state_viewable {
                    let geom = match backend.window_ops().get_geometry(win) {
                        Ok(g) => g,
                        Err(_) => continue,
                    };
                    self.manage(backend, win, &geom)?;
                }
            }
        }
        Ok(())
    }

    fn clean_mask(&self, backend: &mut dyn Backend, raw: u16) -> Mods {
        let mods_all = backend.key_ops().clean_mods(raw);

        mods_all
            & (Mods::SHIFT
                | Mods::CONTROL
                | Mods::ALT
                | Mods::SUPER
                | Mods::MOD2
                | Mods::MOD3
                | Mods::MOD5)
    }

    fn on_key_press_internal(
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

    fn on_button_press_internal(
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

    fn on_motion_notify_internal(
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

    fn on_configure_request_internal(
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

    fn handle_regular_configure_request_params(
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

    fn handle_unmanaged_configure_request_params(
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

    fn target_to_monitor(
        &mut self,
        backend: &mut dyn Backend,
        target: crate::backend::api::HitTarget,
        fallback_pos: (i32, i32),
    ) -> Option<MonitorKey> {
        use crate::backend::api::HitTarget;

        match target {
            HitTarget::Background { output: Some(oid) } => {
                // 直接用 output_map 找 monitor
                for (mon_key, &mapped_oid) in &self.state.output_map {
                    if mapped_oid == oid {
                        return Some(mon_key);
                    }
                }
                self.state.sel_mon
            }
            HitTarget::Background { output: None } => {
                // fallback：用坐标查
                self.recttomon(backend, fallback_pos.0, fallback_pos.1)
            }
            HitTarget::Surface(win) => {
                // 还是按原逻辑：先看 client.mon，否则用 pointer 落点
                if let Some(ck) = self.wintoclient(win) {
                    if let Some(c) = self.state.clients.get(ck) {
                        return c.mon.or(self.state.sel_mon);
                    }
                }
                self.recttomon(backend, fallback_pos.0, fallback_pos.1)
            }
        }
    }

    fn insert_client(&mut self, client: WMClient) -> ClientKey {
        let win = client.win;
        let key = self.state.clients.insert(client);
        self.state.client_order.push(key);
        self.state.win_to_client.insert(win, key);
        key
    }

    fn insert_monitor(&mut self, monitor: WMMonitor) -> MonitorKey {
        let key = self.state.monitors.insert(monitor);
        self.state.monitor_order.push(key);
        self.state.monitor_clients.insert(key, Vec::new());
        self.state.monitor_stack.insert(key, Vec::new());
        key
    }

    fn is_client_selected(&self, client_key: ClientKey) -> bool {
        self.state
            .sel_mon
            .and_then(|sel_mon_key| self.state.monitors.get(sel_mon_key))
            .and_then(|monitor| monitor.sel)
            .map(|sel_client| sel_client == client_key)
            .unwrap_or(false)
    }

    fn get_monitor_clients(&self, mon_key: MonitorKey) -> &[ClientKey] {
        self.state
            .monitor_clients
            .get(mon_key)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    fn get_selected_client_key(&self) -> Option<ClientKey> {
        self.state
            .sel_mon
            .and_then(|sel_mon_key| self.state.monitors.get(sel_mon_key))
            .and_then(|monitor| monitor.sel)
    }

    fn find_next_visible_client_by_mon(&self, mon_key: MonitorKey) -> Option<ClientKey> {
        if let Some(stack_list) = self.state.monitor_stack.get(mon_key) {
            for &client_key in stack_list {
                if let Some(_) = self.state.clients.get(client_key) {
                    if self.is_client_visible_on_monitor(client_key, mon_key) {
                        return Some(client_key);
                    }
                }
            }
        }
        None
    }

    fn is_client_visible_on_monitor(&self, client_key: ClientKey, mon_key: MonitorKey) -> bool {
        if let (Some(client), Some(monitor)) = (
            self.state.clients.get(client_key),
            self.state.monitors.get(mon_key),
        ) {
            client.state.is_sticky || (client.state.tags & monitor.get_active_tags()) > 0
        } else {
            false
        }
    }

    fn is_client_visible_by_key(&self, client_key: ClientKey) -> bool {
        if let Some(client) = self.state.clients.get(client_key) {
            if let Some(mon_key) = client.mon {
                if let Some(monitor) = self.state.monitors.get(mon_key) {
                    return client.state.is_sticky
                        || (client.state.tags & monitor.get_active_tags()) > 0;
                }
            }
        }

        false
    }

    fn should_animate_tag_switch(&self, mon_key: MonitorKey, old_mask: u32, new_mask: u32) -> bool {
        let Some(client_keys) = self.state.monitor_clients.get(mon_key) else {
            return false;
        };

        let mut has_membership_change = false;

        for client_key in client_keys.iter().copied() {
            let Some(client) = self.state.clients.get(client_key) else {
                continue;
            };

            if client.state.is_sticky {
                continue;
            }

            let old_visible = (client.state.tags & old_mask) > 0;
            let new_visible = (client.state.tags & new_mask) > 0;

            if old_visible != new_visible {
                has_membership_change = true;
                break;
            }
        }

        // Animate whenever visible window membership changes between old and
        // new tags. This includes switching to/from empty tags (wallpaper-only).
        // When both tags are empty, has_membership_change is false so we skip.
        has_membership_change
    }

    /// Return the number of pixels at the top of the monitor to exclude from
    /// the tag-switch transition. Use the monitor workarea so compositor
    /// transitions respect any top-reserved space, whether it comes from the
    /// built-in bar, secondary bars, or external panels via struts.
    fn tag_transition_exclude_top(&self, mon_key: MonitorKey) -> u32 {
        let Some(monitor) = self.state.monitors.get(mon_key) else {
            return 0;
        };

        let monitor_top = monitor.geometry.m_y;
        let workarea_top = self
            .monitor_work_area(mon_key)
            .map(|rect| rect.y)
            .unwrap_or(monitor.geometry.w_y);

        (workarea_top - monitor_top).max(0) as u32
    }

    /// Return the (x, y, w, h) rect of the given monitor for compositor transitions.
    fn monitor_rect(&self, mon_key: MonitorKey) -> (i32, i32, u32, u32) {
        if let Some(mon) = self.state.monitors.get(mon_key) {
            let g = &mon.geometry;
            (g.m_x, g.m_y, g.m_w.max(1) as u32, g.m_h.max(1) as u32)
        } else {
            (0, 0, 1, 1)
        }
    }

    fn pop(&mut self, backend: &mut dyn Backend, client_key: ClientKey) {
        let mon_key = if let Some(client) = self.state.clients.get(client_key) {
            client.mon
        } else {
            return;
        };

        self.detach(client_key);
        self.attach_front(client_key);

        let _ = self.focus(backend, Some(client_key));
        if let Some(mon_key) = mon_key {
            self.arrange(backend, Some(mon_key));
        }
    }

    fn wintoclient(&self, win: WindowId) -> Option<ClientKey> {
        self.state.win_to_client.get(&win).copied()
    }

    fn log_x11_environment() {
        info!("[X11 Environment Debug]");
        info!("DISPLAY: {:?}", env::var("DISPLAY"));
        info!("XAUTHORITY: {:?}", env::var("XAUTHORITY"));
        info!("XDG_SESSION_TYPE: {:?}", env::var("XDG_SESSION_TYPE"));
        info!("USER: {:?}", env::var("USER"));
        info!("HOME: {:?}", env::var("HOME"));

        if let Ok(display) = env::var("DISPLAY") {
            let socket_path = format!("/tmp/.X11-unix/X{}", display.trim_start_matches(":"));
            info!("X11 socket path: {}", socket_path);
            info!(
                "X11 socket exists: {}",
                std::path::Path::new(&socket_path).exists()
            );
        }

        let x_running = std::process::Command::new("pgrep")
            .arg("-f")
            .arg("X|Xorg")
            .output()
            .map(|output| !output.stdout.is_empty())
            .unwrap_or(false);
        info!("X server running: {}", x_running);
    }

    pub fn restart(
        &mut self,
        _backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[restart] Preparing seamless restart");
        self.running.store(false, Ordering::SeqCst);
        self.is_restarting.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn is_bar_visible_on_mon(&self, mon_key: MonitorKey) -> bool {
        if let Some(m) = self.state.monitors.get(mon_key) {
            if let Some(p) = m.pertag.as_ref() {
                if let Some(&show) = p.show_bars.get(p.cur_tag) {
                    return show;
                }
            }
        }
        true
    }
    fn mark_bar_update_needed_if_visible(&mut self, monitor_id: Option<i32>) {
        match monitor_id {
            Some(id) => {
                if let Some(mon_key) = self.get_monitor_by_id(id) {
                    if self.is_bar_visible_on_mon(mon_key) {
                        self.pending_bar_updates.insert(id);
                    }
                }
            }
            None => {
                for (key, m) in self.state.monitors.iter() {
                    if self.is_bar_visible_on_mon(key) {
                        self.pending_bar_updates.insert(m.num);
                    }
                }
            }
        }
    }

    fn get_wm_class(
        &self,
        backend: &mut dyn Backend,
        window: WindowId,
    ) -> Option<(String, String)> {
        let (inst, cls) = backend.property_ops().get_class(window);
        if inst.is_empty() && cls.is_empty() {
            None
        } else {
            Some((inst, cls))
        }
    }


    pub fn cleanup(&mut self, backend: &mut dyn Backend) -> Result<(), Box<dyn std::error::Error>> {
        info!("[cleanup] Starting essential cleanup (letting Rust handle memory)");
        // Shut down IPC server (also handled by Drop, but explicit is clearer)
        if let Some(ref mut ipc) = self.ipc_server {
            ipc.shutdown();
        }
        self.ipc_server = None;
        self.cleanup_x11_resources(backend)?;
        self.cleanup_system_resources()?;
        backend.color_allocator().free_all_theme_pixels()?;
        backend.window_ops().flush()?;
        info!("[cleanup] Essential cleanup completed (Rust will handle the rest)");
        Ok(())
    }

    fn cleanup_x11_resources(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[cleanup_x11_resources] Cleaning X11 resources");

        // On restart: stop recording normally, state file persists for auto-resume
        if self.is_restarting.load(Ordering::SeqCst) && self.features.recording.active {
            backend.compositor_stop_recording();
            if let Some(seg) = self.features.recording.current_segment.take() {
                self.features.recording.segments.push(seg);
            }
            Self::save_recording_state(
                self.features.recording.output_path.as_deref().unwrap_or(""),
                &self.features.recording.segments,
            );
            self.features.recording.active = false;
            info!("[cleanup_x11_resources] Recording stopped for restart, state saved");
        }

        self.cleanup_all_clients_x11_state(backend)?;

        self.cleanup_key_grabs(backend)?;

        self.reset_input_focus(backend)?;

        backend.cleanup()?;

        if let Err(e) = backend.cursor_provider().cleanup() {
            log::warn!("cursor cleanup failed: {:?}", e);
        }

        info!("[cleanup_x11_resources] X11 resources cleaned");
        Ok(())
    }

    fn cleanup_system_resources(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        info!("[cleanup_system_resources] Cleaning system resources");

        self.cleanup_statusbar_processes()?;

        self.cleanup_shared_memory_resources()?;

        info!("[cleanup_system_resources] System resources cleaned");
        Ok(())
    }

    fn cleanup_all_clients_x11_state(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[cleanup_all_clients_x11_state]");
        let restarting = self.is_restarting.load(Ordering::SeqCst);

        let mut clients_to_process = Vec::new();
        for &mon_key in &self.state.monitor_order {
            if let Some(stack) = self.state.monitor_stack.get(mon_key) {
                for &ck in stack {
                    if let Some(c) = self.state.clients.get(ck) {
                        clients_to_process.push((c.win, c.geometry.old_border_w, ck));
                    }
                }
            }
        }
        for (win, old_border_w, ck) in clients_to_process {
            if let Some(_) = self.state.clients.get(ck) {
                if restarting {
                    backend.window_ops().ungrab_all_buttons(win)?;
                } else {
                    let _ = self.restore_client_x11_state(backend, win, old_border_w);
                }
            }
        }

        Ok(())
    }

    fn restore_client_x11_state(
        &mut self,
        backend: &mut dyn Backend,
        win: WindowId,
        old_border_w: i32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Err(e) = backend
            .window_ops()
            .change_event_mask(win, EventMaskBits::NONE.bits())
        {
            log::warn!("Failed to clear events for {:?}: {:?}", win, e);
        }
        let changes = WindowChanges {
            border_width: Some(old_border_w as u32),
            ..Default::default()
        };
        if let Err(e) = backend.window_ops().apply_window_changes(win, changes) {
            log::warn!("Failed to restore border for {:?}: {:?}", win, e);
        }
        if let Err(e) = backend.window_ops().ungrab_all_buttons(win) {
            log::warn!("Failed to ungrab buttons for {:?}: {:?}", win, e);
        }
        if let Err(e) = self.setclientstate(backend, win, crate::jwm::WITHDRAWN_STATE as i64) {
            log::warn!("Failed to set withdrawn state for {:?}: {:?}", win, e);
        }
        Ok(())
    }

    fn cleanup_statusbar_processes(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // Clean up secondary bars
        self.cleanup_secondary_bars()?;
        Ok(())
    }

    fn cleanup_secondary_bars(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        for (mon_id, mut bar) in self.secondary_bars.drain() {
            let pid = bar.child.id();
            let nix_pid = Pid::from_raw(pid as i32);

            match signal::kill(nix_pid, None) {
                Err(_) => {
                    info!("Secondary bar for monitor {} already terminated", mon_id);
                    continue;
                }
                Ok(_) => {}
            }

            if let Ok(_) = signal::kill(nix_pid, Signal::SIGTERM) {
                let timeout = Duration::from_secs(3);
                let start = Instant::now();
                while start.elapsed() < timeout {
                    match bar.child.try_wait() {
                        Ok(Some(status)) => {
                            info!("Secondary bar {} exited gracefully: {:?}", mon_id, status);
                            break;
                        }
                        Ok(None) => {
                            std::thread::sleep(Duration::from_millis(100));
                        }
                        Err(_) => {
                            break;
                        }
                    }
                }
                if bar.child.try_wait().ok().flatten().is_none() {
                    warn!("Secondary bar {} timeout, forcing kill", mon_id);
                    let _ = signal::kill(nix_pid, Signal::SIGKILL);
                }
            }
        }
        Ok(())
    }

    fn cleanup_shared_memory_resources(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // Clean up all monitor bars shared memory
        for (mon_id, bar) in self.secondary_bars.drain() {
            drop(bar.shmem);
            #[cfg(unix)]
            {
                let path = format!("/dev/shm/jwm_bar_mon_{}", mon_id);
                if std::path::Path::new(&path).exists() {
                    if let Err(e) = std::fs::remove_file(&path) {
                        warn!("Failed to remove {}: {}", path, e);
                    }
                }
            }
        }

        Ok(())
    }

    fn cleanup_key_grabs(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Err(e) = backend
            .key_ops()
            .clear_key_grabs(backend.root_window().expect("no root window"))
        {
            warn!("[cleanup_key_grabs] Failed to ungrab keys: {:?}", e);
        }
        Ok(())
    }

    fn reset_input_focus(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        backend.window_ops().set_input_focus_root()?;
        Ok(())
    }

    fn configurenotify(
        &mut self,
        backend: &mut dyn Backend,
        window: WindowId,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if window == backend.root_window().expect("no root window") {
            let dirty = self.s_w != w as i32 || self.s_h != h as i32;
            self.s_w = w as i32;
            self.s_h = h as i32;
            if self.updategeom(backend) || dirty {
                self.handle_screen_geometry_change(backend)?;
            }
        }

        // For Wayland layer-shell (and other backend-driven docks), the compositor controls
        // the final geometry. Reflect it in our model and re-arrange so workareas update.
        if let Some(client_key) = self.wintoclient(window) {
            let layer_info = backend.property_ops().get_layer_surface_info(window);
            let is_likely_dock = self
                .state
                .clients
                .get(client_key)
                .map(|c| c.state.is_dock)
                .unwrap_or(false)
                || layer_info.is_some();

            if is_likely_dock {
                if let Some(c) = self.state.clients.get(client_key) {
                    info!(
                        "[dock_configure_notify] win={:?} event={}x{}+{}+{} current={}x{}+{}+{}",
                        window,
                        w,
                        h,
                        x,
                        y,
                        c.geometry.w,
                        c.geometry.h,
                        c.geometry.x,
                        c.geometry.y
                    );
                }

                let geometry_changed = self
                    .state
                    .clients
                    .get(client_key)
                    .map(|c| c.geometry.x != x || c.geometry.y != y || c.geometry.w != w as i32 || c.geometry.h != h as i32)
                    .unwrap_or(true);

                if !geometry_changed {
                    return Ok(());
                }

                // Check if this is a status bar being moved back to origin by GTK
                // If so, skip the update to prevent feedback loop with arrange
                let is_status_bar_reset = self.state.clients.get(client_key).map(|c| {
                    c.state.is_dock && x == 0 && y == 0 && c.geometry.x != 0
                }).unwrap_or(false);

                if is_status_bar_reset {
                    // Status bar trying to reset to (0,0), ignore this configure notify
                    // to prevent feedback loop with arrange repositioning it
                    return Ok(());
                }

                if let Some(c) = self.state.clients.get_mut(client_key) {
                    c.geometry.x = x;
                    c.geometry.y = y;
                    c.geometry.w = w as i32;
                    c.geometry.h = h as i32;
                }

                // Refresh type/layer metadata so exclusive_zone changes are honored.
                self.updatewindowtype(backend, client_key);

                if let Some(mon_key) = self.state.clients.get(client_key).and_then(|c| c.mon) {
                    self.arrange(backend, Some(mon_key));
                } else {
                    self.arrange(backend, None);
                }
            }
        }

        Ok(())
    }

    fn handle_screen_geometry_change(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // info!("[handle_screen_geometry_change]");
        let monitors: Vec<_> = self.state.monitor_order.to_vec();
        for mon_key in monitors {
            self.update_fullscreen_clients_on_monitor(backend, mon_key)?;
        }
        self.focus(backend, None)?;
        self.arrange(backend, None);
        Ok(())
    }

    fn update_fullscreen_clients_on_monitor(
        &mut self,
        backend: &mut dyn Backend,
        mon_key: MonitorKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let monitor_geometry = if let Some(monitor) = self.state.monitors.get(mon_key) {
            (
                monitor.geometry.m_x,
                monitor.geometry.m_y,
                monitor.geometry.m_w,
                monitor.geometry.m_h,
            )
        } else {
            warn!(
                "[update_fullscreen_clients_on_monitor] Monitor {:?} not found",
                mon_key
            );
            return Ok(());
        };

        let fullscreen_clients: Vec<ClientKey> =
            if let Some(client_keys) = self.state.monitor_clients.get(mon_key) {
                client_keys
                    .iter()
                    .filter(|&&client_key| {
                        self.state
                            .clients
                            .get(client_key)
                            .map(|client| client.state.is_fullscreen)
                            .unwrap_or(false)
                    })
                    .copied()
                    .collect()
            } else {
                Vec::new()
            };

        for client_key in fullscreen_clients {
            let _ = self.resizeclient(
                backend,
                client_key,
                monitor_geometry.0,
                monitor_geometry.1,
                monitor_geometry.2,
                monitor_geometry.3,
            );
        }
        Ok(())
    }

    fn update_client_decoration(
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

        // Compositor renders borders via GPU — tell X11 border is 0.
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

    fn grabkeys(&mut self, backend: &mut dyn Backend) -> Result<(), Box<dyn std::error::Error>> {
        let root_window = backend.root_window().expect("no root window");
        backend.key_ops().clear_key_grabs(root_window)?;
        let bindings: Vec<(Mods, KeySym)> = self
            .key_bindings
            .iter()
            .map(|k| (k.mask, k.key_sym))
            .collect();
        backend.key_ops().grab_keys(root_window, &bindings)?;
        Ok(())
    }

    fn setfullscreen(
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

    fn seturgent(
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

    fn showhide_monitor(&mut self, backend: &mut dyn Backend, mon_key: MonitorKey) {
        if let Some(stack_clients) = self.state.monitor_stack.get(mon_key).cloned() {
            for client_key in stack_clients {
                self.showhide_client(backend, client_key, mon_key);
            }
        }
    }

    fn showhide_client(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        mon_key: MonitorKey,
    ) {
        let is_visible = self.is_client_visible_on_monitor(client_key, mon_key);

        if is_visible {
            self.show_client(backend, client_key);
        } else {
            self.hide_client(backend, client_key);
        }
    }

    fn show_client(&mut self, backend: &mut dyn Backend, client_key: ClientKey) {
        // Cancel any in-flight Hide animation so it doesn't keep moving
        // the window off-screen.  Preserve Layout / Appear animations so
        // that repeated arrange() calls don't kill in-flight transitions.
        self.animations.remove_if_hide(client_key);

        // Restore on-screen x from old_x if client.geometry.x is still at
        // the hidden position (negative, off-screen).
        if let Some(client) = self.state.clients.get_mut(client_key) {
            if client.geometry.x < -(client.geometry.w) {
                client.geometry.x = client.geometry.old_x;
            }
        }

        let (win, x, y, is_floating, is_fullscreen) =
            if let Some(client) = self.state.clients.get(client_key) {
                (
                    client.win,
                    client.geometry.x,
                    client.geometry.y,
                    client.state.is_floating,
                    client.state.is_fullscreen,
                )
            } else {
                warn!("[show_client] Client {:?} not found", client_key);
                return;
            };

        if let Err(e) = self.move_window(backend, win, x, y) {
            warn!("[show_client] Failed to move window {:?}: {:?}", win, e);
        }

        if is_floating && !is_fullscreen {
            let (w, h) = if let Some(client) = self.state.clients.get(client_key) {
                (client.geometry.w, client.geometry.h)
            } else {
                return;
            };
            self.resize_client(backend, client_key, x, y, w, h, false);
        }
    }

    fn hide_client(&mut self, backend: &mut dyn Backend, client_key: ClientKey) {
        let (win, x, y, w, h, width) = if let Some(client) = self.state.clients.get(client_key) {
            (
                client.win,
                client.geometry.x,
                client.geometry.y,
                client.geometry.w,
                client.geometry.h,
                client.total_width(),
            )
        } else {
            warn!("[hide_client] Client {:?} not found", client_key);
            return;
        };

        let hidden_x = width * -2;

        // Save visible geometry so show_client can restore it, then update
        // client.geometry to the hidden position. This prevents
        // tick_animations from snapping the window back on-screen when the
        // Hide animation completes.
        //
        // Guard: only save old_x/old_y when the window is still on-screen.
        // If it is already hidden (x is far negative), a repeated hide_client
        // call must NOT overwrite old_x with the hidden position — otherwise
        // show_client will restore the window to an off-screen coordinate.
        if let Some(client) = self.state.clients.get_mut(client_key) {
            if client.geometry.x >= -(client.geometry.w) {
                client.geometry.old_x = client.geometry.x;
                client.geometry.old_y = client.geometry.y;
            }
            client.geometry.x = hidden_x;
            // y, w, h stay unchanged
        }

        let cfg = CONFIG.load();
        if cfg.animation_enabled() {
            let now = Instant::now();
            let visual = self
                .animations
                .current_visual_rect(client_key, now)
                .unwrap_or(Rect::new(x, y, w, h));
            let target = Rect::new(hidden_x, y, w, h);
            self.animations.start(
                client_key,
                visual,
                target,
                cfg.animation_duration(),
                cfg.animation_easing(),
                AnimationKind::Hide,
            );
            // When compositor is active, move the actual X11 window to the
            // hidden position immediately.  The compositor handles the visual
            // slide-out via the scene, but the X server delivers input events
            // based on the real window geometry — without this the hidden
            // window still receives hover/click events at its old position.
            if backend.has_compositor() {
                if let Err(e) = self.move_window(backend, win, hidden_x, y) {
                    warn!(
                        "[hide_client] Failed to move window off-screen {:?}: {:?}",
                        win, e
                    );
                }
            }
        } else {
            if let Err(e) = self.move_window(backend, win, hidden_x, y) {
                warn!("[hide_client] Failed to hide window {:?}: {:?}", win, e);
            }
        }
    }

    fn resize_client(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        mut x: i32,
        mut y: i32,
        mut w: i32,
        mut h: i32,
        interact: bool,
    ) {
        if self
            .applysizehints(
                backend, client_key, &mut x, &mut y, &mut w, &mut h, interact,
            )
            .is_ok()
        {
            let _ = self.resizeclient(backend, client_key, x, y, w, h);
        }
    }

    fn resizeclient(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(client) = self.state.clients.get_mut(client_key) {
            client.geometry.old_x = client.geometry.x;
            client.geometry.old_y = client.geometry.y;
            client.geometry.old_w = client.geometry.w;
            client.geometry.old_h = client.geometry.h;

            client.geometry.x = x;
            client.geometry.y = y;
            client.geometry.w = w;
            client.geometry.h = h;

            // When compositor is active, borders are rendered by the compositor
            // (Pass 3) — tell X11 the border is 0 so it doesn't draw its own.
            let x11_bw = if backend.has_compositor() {
                0
            } else {
                client.geometry.border_w as u32
            };

            let cfg = CONFIG.load();
            if cfg.animation_enabled() && !self.suppress_layout_animation {
                let old_rect = Rect::new(
                    client.geometry.old_x,
                    client.geometry.old_y,
                    client.geometry.old_w,
                    client.geometry.old_h,
                );
                let target = Rect::new(x, y, w, h);
                let duration = cfg.animation_duration();
                let easing = cfg.animation_easing();
                drop(cfg);
                let now = Instant::now();
                let visual = self
                    .animations
                    .current_visual_rect(client_key, now)
                    .unwrap_or(old_rect);
                self.animations.start(
                    client_key,
                    visual,
                    target,
                    duration,
                    easing,
                    AnimationKind::Layout,
                );
                // When compositor is active, move the actual X11 window to the
                // target position immediately.  The compositor handles visual
                // interpolation via the scene, but the X server delivers input
                // events based on the real window geometry — so the window must
                // be at the correct position for clicks to work.
                //
                // When compositor is OFF, we still need to position the X11 window
                // at the animation's starting point (visual) so that tick_animations
                // can animate from the correct position. Without this, the window
                // might be off-screen or at a stale position, causing visual glitches.
                if backend.has_compositor() {
                    backend
                        .window_ops()
                        .configure(client.win, x, y, w as u32, h as u32, x11_bw)?;
                } else {
                    backend.window_ops().configure(
                        client.win,
                        visual.x,
                        visual.y,
                        visual.w as u32,
                        visual.h as u32,
                        x11_bw,
                    )?;
                }
            } else {
                backend
                    .window_ops()
                    .configure(client.win, x, y, w as u32, h as u32, x11_bw)?;
            }
        }
        Ok(())
    }

    fn tick_animations(&mut self, backend: &mut dyn Backend) {
        // --- Night Light: update color temperature once per minute ---
        if backend.has_compositor() {
            let should_update = match self.last_night_light_update {
                Some(last) => last.elapsed() >= Duration::from_secs(60),
                None => true,
            };
            if should_update {
                self.last_night_light_update = Some(Instant::now());
                let cfg = CONFIG.load();
                let behavior = cfg.behavior();
                if behavior.night_light {
                    let temp = Self::compute_night_light_temp(
                        &behavior.night_light_start,
                        &behavior.night_light_end,
                        behavior.night_light_temp,
                        behavior.night_light_transition_mins,
                    );
                    backend.compositor_set_color_temperature(temp);
                } else {
                    backend.compositor_set_color_temperature(0.0);
                }
            }
        }

        let composited = backend.has_compositor();

        if !self.animations.has_active() {
            if composited && backend.compositor_needs_render() {
                // No animations but compositor has dirty windows (damage, add/remove, resize)
                let scene = self.build_compositor_scene(backend, &HashMap::new());
                if scene.is_empty() {
                    // Log once per second at most
                    static LAST_EMPTY: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(0);
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let prev = LAST_EMPTY.load(std::sync::atomic::Ordering::Relaxed);
                    if now > prev {
                        LAST_EMPTY.store(now, std::sync::atomic::Ordering::Relaxed);
                        log::warn!(
                            "[tick_animations] compositor scene is EMPTY (no windows to render)"
                        );
                    }
                }
                let focused = self
                    .get_selected_client_key()
                    .and_then(|ck| self.state.clients.get(ck))
                    .map(|c| c.win.raw());
                let _ = backend.compositor_render_frame(&scene, focused);
            }
            return;
        }

        let now = Instant::now();
        let mut completed = Vec::new();
        let mut visual_overrides: HashMap<ClientKey, Rect> = HashMap::new();

        let keys: Vec<ClientKey> = self.animations.active.keys().copied().collect();
        for key in keys {
            let anim = match self.animations.active.get(&key) {
                Some(a) => a,
                None => continue,
            };
            let (rect, done) = anim.sample(now);

            if self.state.clients.get(key).is_none() {
                completed.push(key);
                continue;
            }

            if composited {
                // Store visual override — compositor draws at interpolated position.
                // Real window is already at the target position (set by resizeclient).
                visual_overrides.insert(key, rect);
            } else {
                // Non-composited fallback: physically move the window each frame
                if let Some(client) = self.state.clients.get(key) {
                    let _ = backend.window_ops().configure(
                        client.win,
                        rect.x,
                        rect.y,
                        rect.w as u32,
                        rect.h as u32,
                        client.geometry.border_w as u32,
                    );
                }
            }

            if done {
                completed.push(key);
            }
        }

        if composited {
            let scene = self.build_compositor_scene(backend, &visual_overrides);
            let focused = self
                .get_selected_client_key()
                .and_then(|ck| self.state.clients.get(ck))
                .map(|c| c.win.raw());
            let _ = backend.compositor_render_frame(&scene, focused);
        }

        for key in completed {
            self.animations.active.remove(&key);
        }
    }

    /// Build window tab groups: one group per monitor, containing visible tiled windows.
    /// The focused window is marked as active tab.
    fn build_window_groups(&self) -> Vec<(u32, Vec<(u32, String, bool)>)> {
        let mut groups = Vec::new();
        let focused_ck = self.get_selected_client_key();
        for (i, &mon_key) in self.state.monitor_order.iter().enumerate() {
            let mut tabs = Vec::new();
            for &ck in self.get_monitor_clients(mon_key) {
                if !self.is_client_visible_on_monitor(ck, mon_key) {
                    continue;
                }
                let client = match self.state.clients.get(ck) {
                    Some(c) => c,
                    None => continue,
                };
                if client.state.is_floating || client.state.is_fullscreen {
                    continue;
                }
                let is_active = focused_ck == Some(ck);
                tabs.push((client.win.raw() as u32, client.name.clone(), is_active));
            }
            if tabs.len() > 1 {
                groups.push((i as u32, tabs));
            }
        }
        groups
    }

    /// Build an ordered scene for the compositor: Vec<(window_id_raw, x, y, w, h)>
    /// from bottom to top, using the last_stacking order. For windows with
    /// active animation overrides, use the interpolated rect instead of actual geometry.
    fn build_compositor_scene(
        &self,
        backend: &dyn Backend,
        visual_overrides: &HashMap<ClientKey, Rect>,
    ) -> Vec<(u64, i32, i32, u32, u32)> {
        let mut scene = Vec::new();

        let debug_compositor = std::env::var("JWM_DEBUG_COMPOSITOR")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        // Iterate all monitors, using last_stacking order (bottom to top)
        for &mon_key in &self.state.monitor_order {
            if debug_compositor {
                let has_stacking = self.last_stacking.get(mon_key).is_some();
                let stack_len = self
                    .last_stacking
                    .get(mon_key)
                    .map(|s| s.len())
                    .unwrap_or(0);
                let client_count = self
                    .state
                    .monitor_clients
                    .get(mon_key)
                    .map(|c| c.len())
                    .unwrap_or(0);
                info!(
                    "[compositor_scene] mon={:?} has_stacking={} stack_len={} clients={}",
                    mon_key, has_stacking, stack_len, client_count
                );
            }
            // Use last_stacking if available, otherwise fall back to
            // monitor_stack so the compositor still has something to render
            // when restack() hasn't run yet for this monitor.
            let stacking_source: Vec<WindowId> =
                if let Some(stacking) = self.last_stacking.get(mon_key) {
                    stacking.clone()
                } else if let Some(stack) = self.state.monitor_stack.get(mon_key) {
                    // Fallback: build bottom-to-top from monitor_stack (which is top-to-bottom)
                    stack
                        .iter()
                        .rev()
                        .filter_map(|&ck| {
                            let c = self.state.clients.get(ck)?;
                            if self.is_client_visible_on_monitor(ck, mon_key) {
                                Some(c.win)
                            } else {
                                None
                            }
                        })
                        .collect()
                } else {
                    Vec::new()
                };

            for &win_id in &stacking_source {
                // Find the client key for this window
                if let Some(&ck) = self.state.win_to_client.get(&win_id) {
                    if let Some(client) = self.state.clients.get(ck) {
                        let (x, y, w, h) = if let Some(rect) = visual_overrides.get(&ck) {
                            (rect.x, rect.y, rect.w as u32, rect.h as u32)
                        } else {
                            (
                                client.geometry.x,
                                client.geometry.y,
                                client.geometry.w as u32,
                                client.geometry.h as u32,
                            )
                        };
                        if w > 0 && h > 0 {
                            scene.push((win_id.raw(), x, y, w, h));
                        }
                    }
                }
            }
        }

        // Also include the status bar if present — but skip it when a large
        // override-redirect window (e.g. screenshot overlay) covers the bar area.
        // RGBA OR overlays don't participate in occlusion culling, so without
        // this check the real status bar would render beneath the overlay's
        // semi-transparent region, producing a "double bar" artifact.
        let overlay_win = backend.compositor_overlay_window();
        // Include per-monitor secondary status bars
        for bar_instance in self.secondary_bars.values() {
            if let Some(bar_key) = bar_instance.client_key {
                if let Some(bar) = self.state.clients.get(bar_key) {
                    let w = bar.geometry.w as u32;
                    let h = bar.geometry.h as u32;
                    if w > 0 && h > 0 {
                        scene.push((bar.win.raw(), bar.geometry.x, bar.geometry.y, w, h));
                    }
                }
            }
        }

        // Include override-redirect windows (menus, dmenu, tooltips) on top.
        // These are not managed by the WM but must be composited.
        // Filter out the compositor's overlay window to avoid feedback loops.
        // Use cached geometries to avoid synchronous GetGeometry round-trips
        // on every frame (which add per-window X11 latency).
        for &or_win in &self.override_redirect_windows {
            if Some(or_win) == overlay_win {
                continue;
            }
            if let Some(&(x, y, w, h)) = self.or_window_geometries.get(&or_win) {
                if w > 0 && h > 0 {
                    scene.push((or_win.raw(), x, y, w, h));
                }
            }
        }

        scene
    }

    fn sync_focused_floating_geometry(&mut self, backend: &mut dyn Backend) {
        let sel_key = match self.get_selected_client_key() {
            Some(k) => k,
            None => return,
        };
        let win = match self.state.clients.get(sel_key) {
            Some(c) if c.state.is_floating => c.win,
            _ => return,
        };
        let geom = match backend.window_ops().get_geometry(win) {
            Ok(g) => g,
            Err(_) => return,
        };
        if let Some(client) = self.state.clients.get_mut(sel_key) {
            client.geometry.x = geom.x as i32;
            client.geometry.y = geom.y as i32;
            client.geometry.w = geom.w as i32;
            client.geometry.h = geom.h as i32;
            client.geometry.floating_x = geom.x as i32;
            client.geometry.floating_y = geom.y as i32;
            client.geometry.floating_w = geom.w as i32;
            client.geometry.floating_h = geom.h as i32;
        }
    }

    fn configure_client(
        &self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(client) = self.state.clients.get(client_key) {
            // Compositor renders borders via GPU — tell X11 border is 0.
            let x11_bw = if backend.has_compositor() {
                0
            } else {
                client.geometry.border_w as u32
            };

            backend.window_ops().configure(
                client.win,
                client.geometry.x,
                client.geometry.y,
                client.geometry.w as u32,
                client.geometry.h as u32,
                x11_bw,
            )?;

            // 分离装饰设置
            let border_color = backend
                .color_allocator()
                .get_border_pixel_of(SchemeType::Norm)?;
            backend
                .window_ops()
                .set_decoration_style(client.win, x11_bw, border_color)?;
        }
        Ok(())
    }

    fn move_window(
        &mut self,
        backend: &mut dyn Backend,
        win: WindowId,
        x: i32,
        y: i32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        backend.window_ops().set_position(win, x, y)?;
        Ok(())
    }

    fn createmon(&mut self, show_bar: bool) -> WMMonitor {
        // info!("[createmon]");
        let cfg = CONFIG.load();
        let mut m: WMMonitor = WMMonitor::new();
        m.tag_set[0] = 1;
        m.tag_set[1] = 1;
        m.layout.m_fact = cfg.m_fact();
        m.layout.n_master = cfg.n_master();
        m.lt[0] = Rc::new(LayoutEnum::FIBONACCI);
        m.lt[1] = Rc::new(LayoutEnum::TILE);
        m.lt_symbol = m.lt[0].symbol().to_string();
        m.pertag = Some(Pertag::new(show_bar, cfg.tags_length()));
        // SAFETY: pertag was just set to Some on the line above
        let ref_pertag = m.pertag.as_mut().expect("pertag just initialized");
        ref_pertag.cur_tag = 1;
        ref_pertag.prev_tag = 1;
        let default_layout_0 = m.lt[0].clone();
        let default_layout_1 = m.lt[1].clone();
        for i in 0..=cfg.tags_length() {
            ref_pertag.n_masters[i] = m.layout.n_master;
            ref_pertag.m_facts[i] = m.layout.m_fact;

            ref_pertag.lt_idxs[i][0] = Some(default_layout_0.clone());
            ref_pertag.lt_idxs[i][1] = Some(default_layout_1.clone());
            ref_pertag.sel_lts[i] = m.sel_lt;
        }
        info!("[createmon]: {}", m);
        return m;
    }

    fn enter_notify(
        &mut self,
        backend: &mut dyn Backend,
        event_window: WindowId,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.handle_statusbar_enter_generic(backend, event_window)? {
            return Ok(());
        }
        self.handle_regular_enter_generic(backend, event_window)?;
        Ok(())
    }

    fn handle_statusbar_enter_generic(
        &mut self,
        _backend: &mut dyn Backend,
        _event_window: WindowId,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        Ok(false)
    }

    fn handle_regular_enter_generic(
        &mut self,
        backend: &mut dyn Backend,
        event_window: WindowId,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.mouse_focus_blocked() {
            return Ok(());
        }
        let client_key_opt = self.wintoclient(event_window);
        let monitor_key_opt = if let Some(client_key) = client_key_opt {
            self.state
                .clients
                .get(client_key)
                .and_then(|client| client.mon)
        } else {
            self.wintomon(backend, Some(event_window))
        };
        let current_event_monitor_key = match monitor_key_opt {
            Some(monitor_key) => monitor_key,
            None => return Ok(()),
        };
        let is_on_selected_monitor = Some(current_event_monitor_key) == self.state.sel_mon;
        if !is_on_selected_monitor {
            self.switch_to_monitor(backend, current_event_monitor_key)?;
        }
        if self.should_focus_client(client_key_opt, is_on_selected_monitor) {
            self.focus(backend, client_key_opt)?;
        }
        Ok(())
    }

    fn destroynotify(
        &mut self,
        backend: &mut dyn Backend,
        window: WindowId,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Clean up external struts for override-redirect windows (e.g. polybar)
        // that are never managed but may have set strut properties.
        if self.external_struts.remove(&window).is_some() {
            info!("[strut] Removed strut on destroy for {:?}", window);
            self.apply_strut_reservations();
            self.arrange(backend, None);
        }
        let c = self.wintoclient(window);
        if c.is_some() {
            self.unmanage(backend, c, true)?;
        }
        Ok(())
    }

    fn dirtomon(&mut self, dir: &i32) -> Option<MonitorKey> {
        let selected_monitor_key = self.state.sel_mon?;
        if self.state.monitor_order.is_empty() {
            return None;
        }
        let current_index = self
            .state
            .monitor_order
            .iter()
            .position(|&key| key == selected_monitor_key)?;
        if *dir > 0 {
            let next_index = (current_index + 1) % self.state.monitor_order.len();
            Some(self.state.monitor_order[next_index])
        } else {
            let prev_index = if current_index == 0 {
                self.state.monitor_order.len() - 1
            } else {
                current_index - 1
            };
            Some(self.state.monitor_order[prev_index])
        }
    }

    fn ensure_secondary_bars_running(&mut self, now: Instant) {
        // Get all monitor IDs sorted
        let mut all_mon_ids: Vec<i32> = self.state.monitors.values().map(|m| m.num).collect();
        all_mon_ids.sort();

        // Sequential creation: only create the next bar if all previous bars are managed
        for &mon_id in &all_mon_ids {
            // Check if this bar already exists
            if let Some(bar) = self.secondary_bars.get_mut(&mon_id) {
                // Check if process is still alive
                match bar.child.try_wait() {
                    Ok(Some(status)) => {
                        info!("Bar for monitor {} exited: {}", mon_id, status);
                        self.secondary_bars.remove(&mon_id);
                        // Don't create next bar yet, wait for next tick
                        return;
                    }
                    Ok(None) => {
                        // Process still running
                        // If not yet managed (window not created), don't create next bar
                        if bar.window.is_none() {
                            return;
                        }
                        // This bar is managed, continue to check next
                        continue;
                    }
                    Err(e) => {
                        info!("Error checking bar for monitor {}: {}", mon_id, e);
                        self.secondary_bars.remove(&mon_id);
                        return;
                    }
                }
            }

            // Bar doesn't exist, create it
            info!("Creating bar for monitor {} (sequential creation)", mon_id);
            self.spawn_secondary_bar(mon_id, now);
            // Only create one at a time, stop here
            return;
        }

        // Remove bars for monitors that no longer exist
        let existing_monitors: HashSet<i32> = self.state.monitors.values().map(|m| m.num).collect();
        self.secondary_bars
            .retain(|&mon_id, _| existing_monitors.contains(&mon_id));
    }

    fn spawn_secondary_bar(&mut self, monitor_id: i32, now: Instant) {
        // Create unique shared memory path for this monitor
        let shared_path = format!("/dev/shm/jwm_bar_mon_{}", monitor_id);

        // Create shared memory
        let ring_buffer = match SharedRingBuffer::create_aux(&shared_path, None, None) {
            Ok(rb) => rb,
            Err(e) => {
                error!(
                    "Failed to create shared memory for monitor {}: {}",
                    monitor_id, e
                );
                return;
            }
        };

        // Prepare command
        let cfg = CONFIG.load();
        let bar_name = cfg.status_bar_name();
        let mut command = if cfg!(feature = "nixgl") {
            let mut cmd = Command::new("nixGL");
            cmd.arg(bar_name).arg(&shared_path);
            cmd
        } else {
            let mut cmd = Command::new(bar_name);
            cmd.arg(&shared_path);
            cmd
        };

        // Set environment variables
        if let Ok(v) = std::env::var("WAYLAND_DISPLAY") {
            command.env("WAYLAND_DISPLAY", v);
        }
        if let Ok(v) = std::env::var("XDG_RUNTIME_DIR") {
            command.env("XDG_RUNTIME_DIR", v);
        }

        // Tell the bar which monitor it belongs to (for bar's internal use)
        command.env("JWM_MONITOR_ID", monitor_id.to_string());

        // GTK4 bars may need cairo renderer
        if bar_name == "gtk_bar" {
            if std::env::var_os("GSK_RENDERER").is_none() {
                command.env("GSK_RENDERER", "cairo");
            }
        }

        // Spawn the process
        match command
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
        {
            Ok(child) => {
                let pid = child.id();
                info!(
                    "Spawned secondary bar for monitor {} (PID: {})",
                    monitor_id, pid
                );

                let bar_instance = SecondaryBarInstance {
                    monitor_id,
                    shmem: ring_buffer,
                    pid,
                    child,
                    client_key: None,
                    window: None,
                    has_focus: false,
                    last_spawn: now,
                };

                self.secondary_bars.insert(monitor_id, bar_instance);
            }
            Err(e) => {
                error!(
                    "Failed to spawn secondary bar for monitor {}: {}",
                    monitor_id, e
                );
            }
        }
    }

    fn restack(
        &mut self,
        backend: &mut dyn Backend,
        mon_key_opt: Option<MonitorKey>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[restack]");

        let mon_key = mon_key_opt.ok_or("Monitor is required for restack operation")?;
        let monitor = self
            .state
            .monitors
            .get(mon_key)
            .ok_or("Monitor not found")?;
        let monitor_num = monitor.num;

        let stack = self.get_monitor_stack(mon_key);

        let mut tiled_bottom_to_top: Vec<WindowId> = Vec::new();
        let mut floating_bottom_to_top: Vec<WindowId> = Vec::new();
        let mut pip_bottom_to_top: Vec<WindowId> = Vec::new();

        for &ck in stack.iter().rev() {
            if let Some(c) = self.state.clients.get(ck) {
                if !self.is_client_visible_on_monitor(ck, mon_key) {
                    continue;
                }
                if c.state.is_pip {
                    pip_bottom_to_top.push(c.win);
                } else if c.state.is_floating {
                    floating_bottom_to_top.push(c.win);
                } else {
                    tiled_bottom_to_top.push(c.win);
                }
            }
        }

        // Promote selected window to top of its layer, and if it's tiled,
        // raise it above floating windows so it's not obscured.
        let sel_win = monitor
            .sel
            .and_then(|ck| self.state.clients.get(ck))
            .map(|c| (c.win, c.state.is_floating, c.state.is_pip));

        let mut final_bottom_to_top: Vec<WindowId> = Vec::with_capacity(
            tiled_bottom_to_top.len() + floating_bottom_to_top.len() + pip_bottom_to_top.len(),
        );

        if let Some((win, is_floating, is_pip)) = sel_win {
            if is_pip {
                // PiP: promote within pip layer
                if let Some(idx) = pip_bottom_to_top.iter().position(|&w| w == win) {
                    let w = pip_bottom_to_top.remove(idx);
                    pip_bottom_to_top.push(w);
                }
                final_bottom_to_top.extend(tiled_bottom_to_top);
                final_bottom_to_top.extend(floating_bottom_to_top);
                final_bottom_to_top.extend(pip_bottom_to_top);
            } else if is_floating {
                // Floating: promote to top of floating layer (above other floats, below pip)
                if let Some(idx) = floating_bottom_to_top.iter().position(|&w| w == win) {
                    let w = floating_bottom_to_top.remove(idx);
                    floating_bottom_to_top.push(w);
                }
                final_bottom_to_top.extend(tiled_bottom_to_top);
                final_bottom_to_top.extend(floating_bottom_to_top);
                final_bottom_to_top.extend(pip_bottom_to_top);
            } else {
                // Tiled: raise focused tiled window above all floats so it's not obscured
                tiled_bottom_to_top.retain(|&w| w != win);
                final_bottom_to_top.extend(tiled_bottom_to_top);
                final_bottom_to_top.extend(floating_bottom_to_top);
                final_bottom_to_top.push(win); // focused tiled above floats
                final_bottom_to_top.extend(pip_bottom_to_top);
            }
        } else {
            final_bottom_to_top.extend(tiled_bottom_to_top);
            final_bottom_to_top.extend(floating_bottom_to_top);
            final_bottom_to_top.extend(pip_bottom_to_top);
        }

        let need_restack_windows = match self.last_stacking.get(mon_key) {
            Some(prev) => prev.as_slice() != final_bottom_to_top.as_slice(),
            None => true,
        };

        if need_restack_windows {
            backend.window_ops().restack_windows(&final_bottom_to_top)?;
            self.last_stacking
                .insert(mon_key, final_bottom_to_top.clone());
        }

        self.mark_bar_update_needed_if_visible(Some(monitor_num));

        info!("[restack] finish");
        Ok(())
    }

    fn flush_pending_bar_updates(&mut self) {
        if self.pending_bar_updates.is_empty() {
            return;
        }

        // Update all monitor bars that have pending updates
        for &mon_id in self.pending_bar_updates.clone().iter() {
            if let Some(mon_key) = self.get_monitor_by_id(mon_id) {
                if !self.is_bar_visible_on_mon(mon_key) {
                    continue;
                }
                self.update_bar_message_for_monitor(Some(mon_key));

                // Send message to this monitor's bar via shared memory
                if let Some(bar) = self.secondary_bars.get_mut(&mon_id) {
                    let _ = bar.shmem.try_write_message(&self.message);
                }
            }
        }
        self.pending_bar_updates.clear();
    }

    pub fn run(&mut self, backend: &mut dyn Backend) -> Result<(), Box<dyn std::error::Error>> {
        info!("[run] Handing over control to backend");
        Ok(backend.run(self)?)
    }

    fn process_commands_from_status_bar(&mut self, backend: &mut dyn Backend) {
        let mut commands_to_process: Vec<SharedCommand> = Vec::new();

        // Read commands from all per-monitor status bars
        for bar in self.secondary_bars.values_mut() {
            while let Some(cmd) = bar.shmem.receive_command() {
                commands_to_process.push(cmd);
            }
        }

        // Process all collected commands
        for cmd in commands_to_process {
            match cmd.cmd_type.into() {
                CommandType::ViewTag => {
                    info!(
                        "[process_commands] ViewTag command received: {}",
                        cmd.parameter
                    );
                    let arg = WMArgEnum::UInt(cmd.parameter);
                    let _ = self.view(backend, &arg);
                }
                CommandType::ToggleTag => {
                    info!(
                        "[process_commands] ToggleTag command received: {}",
                        cmd.parameter
                    );
                    let arg = WMArgEnum::UInt(cmd.parameter);
                    let _ = self.toggletag(backend, &arg);
                }
                CommandType::SetLayout => {
                    info!(
                        "[process_commands] SetLayout command received: {}",
                        cmd.parameter
                    );
                    let arg = WMArgEnum::Layout(Rc::new(LayoutEnum::from(cmd.parameter)));
                    let _ = self.setlayout(backend, &arg);
                }
                CommandType::None => {}
            }
        }
    }

    fn get_transient_for(&self, backend: &mut dyn Backend, window: WindowId) -> Option<WindowId> {
        backend.property_ops().transient_for(window)
    }

    fn getrootptr(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(i32, i32), Box<dyn std::error::Error>> {
        let (x, y) = backend.input_ops().get_pointer_position()?;
        Ok((x as i32, y as i32))
    }

    fn recttomon(&mut self, backend: &mut dyn Backend, x: i32, y: i32) -> Option<MonitorKey> {
        if let Some(output_id) = backend.output_ops().output_at(x, y) {
            for (mon_key, &oid) in &self.state.output_map {
                if oid == output_id {
                    return Some(mon_key);
                }
            }
        }
        self.state.sel_mon
    }

    fn wintomon(&mut self, backend: &mut dyn Backend, w: Option<WindowId>) -> Option<MonitorKey> {
        if w.is_none() || w == backend.root_window() {
            if let Ok((x, y)) = self.getrootptr(backend) {
                return self.recttomon(backend, x, y);
            }
            return self.state.sel_mon;
        }
        let win_id = match w {
            Some(id) => id,
            None => return self.state.sel_mon,
        };
        if let Some(client_key) = self.wintoclient(win_id) {
            if let Some(client) = self.state.clients.get(client_key) {
                return client.mon.or(self.state.sel_mon);
            }
        }
        self.state.sel_mon
    }

    /// Returns `true` if `backend` is one of the Smithay-based compositors (udev, wayland-x11, wayland-winit).
    fn is_smithay_backend(backend: &dyn Backend) -> bool {
        backend
            .as_any()
            .is::<crate::backend::wayland_udev::backend::UdevBackend>()
            || backend
                .as_any()
                .is::<crate::backend::wayland_x11::backend::WaylandX11Backend>()
            || backend
                .as_any()
                .is::<crate::backend::wayland_winit::backend::WaylandWinitBackend>()
    }

    /// Returns `true` if `backend` is the udev/KMS backend (no Xwayland, no X11 DISPLAY).
    fn is_udev_backend(backend: &dyn Backend) -> bool {
        backend
            .as_any()
            .is::<crate::backend::wayland_udev::backend::UdevBackend>()
    }

    /// Set Wayland-related environment variables on a child `Command` so that
    /// toolkits can connect to this compositor.  When running the udev backend
    /// we propagate the XWayland DISPLAY so X11 apps can connect.
    fn setup_smithay_child_env(command: &mut Command, backend: &dyn Backend) {
        if Self::is_smithay_backend(backend) {
            if let Ok(v) = std::env::var("WAYLAND_DISPLAY") {
                command.env("WAYLAND_DISPLAY", &v);
            }
            if let Ok(v) = std::env::var("XDG_RUNTIME_DIR") {
                command.env("XDG_RUNTIME_DIR", &v);
            }
            if std::env::var_os("XDG_SESSION_TYPE").is_none() {
                command.env("XDG_SESSION_TYPE", "wayland");
            }
            if std::env::var_os("WINIT_UNIX_BACKEND").is_none() {
                command.env("WINIT_UNIX_BACKEND", "wayland");
            }
        }
        if Self::is_udev_backend(backend) {
            // With XWayland running, DISPLAY is set to e.g. ":0" and is valid.
            // Propagate it so X11 apps can connect via XWayland.
            if let Ok(display) = std::env::var("DISPLAY") {
                command.env("DISPLAY", &display);
            }
        }
    }

    /// Apply common child-process isolation: `setsid()` + restore `SIGCHLD` default.
    fn apply_child_pre_exec(command: &mut Command) {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(move || {
                setsid();
                let mut sa: sigaction = std::mem::zeroed();
                sigemptyset(&mut sa.sa_mask);
                sa.sa_flags = 0;
                sa.sa_sigaction = SIG_DFL;
                sigaction(SIGCHLD, &sa, std::ptr::null_mut());
                Ok(())
            });
        }
    }

    pub fn spawn(
        &mut self,
        _backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[spawn]");

        let mut mut_arg: WMArgEnum = arg.clone();
        if let WMArgEnum::StringVec(ref mut v) = mut_arg {
            info!("[spawn] spawning command: {:?}", v);

            let mut command = Command::new(&v[0]);
            command.args(&v[1..]);

            Self::setup_smithay_child_env(&mut command, _backend);

            command
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit());

            Self::apply_child_pre_exec(&mut command);

            match command.spawn() {
                Ok(child) => {
                    debug!(
                        "[spawn] successfully spawned process with PID: {}",
                        child.id()
                    );
                }
                Err(e) => {
                    error!("[spawn] failed to spawn command {:?}: {}", v, e);
                    return Err(e.into());
                }
            }
        }

        Ok(())
    }

    pub fn show_keybindings(
        &mut self,
        _backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[show_keybindings]");

        let cfg = CONFIG.load();
        let mut lines: Vec<String> = Vec::new();
        for kc in cfg.key_configs() {
            let mods = kc.modifier.join("+");
            let shortcut = if mods.is_empty() {
                kc.key.clone()
            } else {
                format!("{}+{}", mods, kc.key)
            };

            let desc = match kc.function.as_str() {
                "spawn" => match &kc.argument {
                    crate::config::ArgumentConfig::StringVec(v) => {
                        format!("spawn {}", v.first().map(|s| s.as_str()).unwrap_or(""))
                    }
                    _ => "spawn".to_string(),
                },
                "setlayout" => match &kc.argument {
                    crate::config::ArgumentConfig::String(s) => format!("layout: {}", s),
                    crate::config::ArgumentConfig::UInt(_) => "toggle layout".to_string(),
                    _ => "setlayout".to_string(),
                },
                "focusstack" => match &kc.argument {
                    crate::config::ArgumentConfig::Int(i) => {
                        if *i > 0 {
                            "focus next".to_string()
                        } else {
                            "focus prev".to_string()
                        }
                    }
                    _ => "focusstack".to_string(),
                },
                "incnmaster" => match &kc.argument {
                    crate::config::ArgumentConfig::Int(i) => {
                        if *i > 0 {
                            "master +1".to_string()
                        } else {
                            "master -1".to_string()
                        }
                    }
                    _ => "incnmaster".to_string(),
                },
                "setmfact" => match &kc.argument {
                    crate::config::ArgumentConfig::Float(f) => {
                        if *f > 0.0 {
                            "mfact +".to_string()
                        } else {
                            "mfact -".to_string()
                        }
                    }
                    _ => "setmfact".to_string(),
                },
                "view" | "tag" | "toggleview" | "toggletag" => match &kc.argument {
                    crate::config::ArgumentConfig::UInt(u) => format!("{} tag {}", kc.function, u),
                    _ => kc.function.clone(),
                },
                other => other.to_string(),
            };

            lines.push(format!("{:<28} {}", shortcut, desc));
        }

        // 添加 tag 快捷键说明
        let tags_len = cfg.tags_length();
        lines.push(format!(
            "{:<28} {}",
            "Mod1+[1-9]",
            format!("view tag 1-{}", tags_len)
        ));
        lines.push(format!(
            "{:<28} {}",
            "Mod1+Shift+[1-9]",
            format!("move to tag 1-{}", tags_len)
        ));
        lines.push(format!(
            "{:<28} {}",
            "Mod1+Ctrl+[1-9]",
            format!("toggle view tag 1-{}", tags_len)
        ));
        lines.push(format!(
            "{:<28} {}",
            "Mod1+Ctrl+Shift+[1-9]",
            format!("toggle tag 1-{}", tags_len)
        ));
        lines.push(format!("{:<28} {}", "Mod1+0", "view all tags"));

        let text = lines.join("\n");

        let dmenu_font = cfg.dmenu_font();
        let mut command = Command::new("dmenu");
        command.args([
            "-l",
            &lines.len().to_string(),
            "-fn",
            dmenu_font,
            "-p",
            "Keybindings:",
        ]);
        command.stdin(std::process::Stdio::piped());
        command.stdout(std::process::Stdio::null());
        command.stderr(std::process::Stdio::inherit());

        Self::apply_child_pre_exec(&mut command);
        Self::setup_smithay_child_env(&mut command, _backend);

        match command.spawn() {
            Ok(mut child) => {
                if let Some(stdin) = child.stdin.take() {
                    use std::io::Write;
                    let mut stdin = stdin;
                    let _ = stdin.write_all(text.as_bytes());
                }
            }
            Err(e) => {
                error!("[show_keybindings] failed to spawn dmenu: {:?}", e);
                return Err(e.into());
            }
        }

        Ok(())
    }

    pub fn reap_zombies(&mut self) {
        // 使用 WNOHANG 循环回收所有已退出的子进程
        loop {
            match waitpid(None, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(pid, status)) => {
                    info!("Child process {} exited with status {}", pid, status);
                }
                Ok(WaitStatus::Signaled(pid, sig, _)) => {
                    info!("Child process {} killed by signal {:?}", pid, sig);
                }
                // StillAlive 表示还有子进程在运行，Break 退出循环
                Ok(WaitStatus::StillAlive) => break,
                // Err 通常表示没有子进程了 (ECHILD)，也退出循环
                Err(_) => break,
                _ => break,
            }
        }
    }

    pub fn togglefloating(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // info!("[togglefloating]");
        let Some(sel_mon_key) = self.state.sel_mon else {
            return Ok(());
        };
        let Some(sel_client_key) = self.state.monitors.get(sel_mon_key).and_then(|m| m.sel) else {
            return Ok(());
        };
        let geom = if let Some(client) = self.state.clients.get_mut(sel_client_key) {
            client.state.is_floating = !client.state.is_floating;
            if client.state.is_floating {
                if client.geometry.floating_w <= 0 || client.geometry.floating_h <= 0 {
                    client.geometry.floating_x = client.geometry.x;
                    client.geometry.floating_y = client.geometry.y;
                    client.geometry.floating_w = client.geometry.w;
                    client.geometry.floating_h = client.geometry.h;
                }
                Some((
                    client.geometry.floating_x,
                    client.geometry.floating_y,
                    client.geometry.floating_w,
                    client.geometry.floating_h,
                ))
            } else {
                client.geometry.floating_x = client.geometry.x;
                client.geometry.floating_y = client.geometry.y;
                client.geometry.floating_w = client.geometry.w;
                client.geometry.floating_h = client.geometry.h;
                None
            }
        } else {
            return Ok(());
        };

        if let Some((x, y, w, h)) = geom {
            self.resize_client(backend, sel_client_key, x, y, w, h, false);
        }

        self.reorder_client_in_monitor_groups(sel_client_key);

        self.arrange(backend, Some(sel_mon_key));
        Ok(())
    }

    pub fn togglesticky(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(sel_mon_key) = self.state.sel_mon else {
            return Ok(());
        };
        let Some(sel_client_key) = self.state.monitors.get(sel_mon_key).and_then(|m| m.sel) else {
            return Ok(());
        };
        if let Some(client) = self.state.clients.get_mut(sel_client_key) {
            client.state.is_sticky = !client.state.is_sticky;
            if client.state.is_sticky {
                // Ensure sticky client has current monitor tags
                if let Some(monitor) = self.state.monitors.get(sel_mon_key) {
                    let current_tags = monitor.get_active_tags();
                    if let Some(client) = self.state.clients.get_mut(sel_client_key) {
                        client.state.tags = current_tags;
                    }
                }
            }
        }
        self.arrange(backend, Some(sel_mon_key));
        Ok(())
    }

    pub fn togglecompositor(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let enable = !backend.has_compositor();
        match backend.set_compositor_enabled(enable) {
            Ok(true) => {
                log::info!(
                    "Compositor toggled: now {}",
                    if enable { "ON" } else { "OFF" }
                );
            }
            Ok(false) => {
                log::info!("Compositor state unchanged");
            }
            Err(e) => {
                log::warn!("Failed to toggle compositor: {e}");
            }
        }
        Ok(())
    }

    /// Compute the night light color temperature factor (0.0 = neutral, up to
    /// `full_temp` when fully inside the night window).  Times are given as
    /// "HH:MM" strings.  `transition_mins` controls the linear ramp-in/out at
    /// the edges of the night window.
    fn compute_night_light_temp(
        start_str: &str,
        end_str: &str,
        full_temp: f32,
        transition_mins: u32,
    ) -> f32 {
        fn parse_hhmm(s: &str) -> Option<u32> {
            let parts: Vec<&str> = s.split(':').collect();
            if parts.len() != 2 {
                return None;
            }
            let h: u32 = parts[0].parse().ok()?;
            let m: u32 = parts[1].parse().ok()?;
            Some(h * 60 + m)
        }

        let start = match parse_hhmm(start_str) {
            Some(v) => v,
            None => return 0.0,
        };
        let end = match parse_hhmm(end_str) {
            Some(v) => v,
            None => return 0.0,
        };

        // Current time in minutes since midnight
        let now = {
            let d = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            // Local time: offset from UTC.  Use libc localtime.
            let secs = d as libc::time_t;
            let mut tm: libc::tm = unsafe { std::mem::zeroed() };
            unsafe {
                libc::localtime_r(&secs, &mut tm);
            }
            (tm.tm_hour as u32) * 60 + (tm.tm_min as u32)
        };

        let day = 24 * 60u32; // 1440
        let trans = transition_mins;

        // Normalize everything so that `start` is time-zero (modular arithmetic).
        // Night window runs from 0 to `length` in the rotated space.
        let length = if end >= start {
            end - start
        } else {
            end + day - start
        };
        let cur = if now >= start {
            now - start
        } else {
            now + day - start
        };

        if cur > length {
            // Outside the night window — check if approaching start (ramp in)
            let before_start = if now < start {
                start - now
            } else {
                start + day - now
            };
            if trans > 0 && before_start < trans {
                // Ramping in: approaching start
                let t = 1.0 - (before_start as f32 / trans as f32);
                return full_temp * t.clamp(0.0, 1.0);
            }
            return 0.0;
        }

        // Inside the night window
        if trans > 0 && cur < trans {
            // Ramp in at the start edge
            let t = cur as f32 / trans as f32;
            return full_temp * t.clamp(0.0, 1.0);
        }
        if trans > 0 && (length - cur) < trans {
            // Ramp out at the end edge
            let t = (length - cur) as f32 / trans as f32;
            return full_temp * t.clamp(0.0, 1.0);
        }
        full_temp
    }

    pub fn toggle_overview(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.features.overview.active {
            // End overview: focus selected window and promote it to master
            if let Some(&client_key) = self.features.overview.clients.get(self.features.overview.index) {
                if let Some(mon_key) = self.state.sel_mon {
                    self.detach(client_key);
                    self.attach_front(client_key);
                    self.focus(backend, Some(client_key))?;
                    self.arrange(backend, Some(mon_key));
                } else {
                    self.focus(backend, Some(client_key))?;
                }
            }
            self.features.overview.active = false;
            backend.compositor_set_overview_mode(false, &[]);
            let _ = backend.key_ops().ungrab_keyboard();
        } else {
            // Start overview: collect visible windows on current monitor
            let sel_mon_key = match self.state.sel_mon {
                Some(k) => k,
                None => return Ok(()),
            };
            let visible: Vec<ClientKey> = {
                let mon_clients = self.state.monitor_clients.get(sel_mon_key);
                match mon_clients {
                    Some(clients) => clients
                        .iter()
                        .copied()
                        .filter(|&ck| self.is_client_visible_by_key(ck))
                        .collect(),
                    None => Vec::new(),
                }
            };

            if visible.is_empty() {
                return Ok(());
            }

            // Tell compositor which monitor to render the prism on.
            if let Some(mon) = self.state.monitors.get(sel_mon_key) {
                backend.compositor_set_overview_monitor(
                    mon.geometry.w_x as i32,
                    mon.geometry.w_y as i32,
                    mon.geometry.w_w as u32,
                    mon.geometry.w_h as u32,
                );
            }

            // Build simple client list; the compositor handles all 3D positioning.
            let layout = self.build_overview_layout(&visible);

            self.features.overview.active = true;
            self.features.overview.index = 0;
            self.features.overview.slide_offset = 0;
            self.features.overview.clients = visible;
            backend.compositor_set_overview_mode(true, &layout);
            if let Some(root) = backend.root_window() {
                let _ = backend.key_ops().grab_keyboard(root);
            }
        }
        Ok(())
    }

    pub fn cycle_overview(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !self.features.overview.active || self.features.overview.clients.is_empty() {
            return Ok(());
        }

        let direction = match arg {
            WMArgEnum::Int(d) => *d,
            _ => 1,
        };

        let len = self.features.overview.clients.len();
        if direction > 0 {
            self.features.overview.index = (self.features.overview.index + 1) % len;
        } else {
            self.features.overview.index = (self.features.overview.index + len - 1) % len;
        }

        if len <= 6 {
            // All clients fit on the prism; just rotate to selection.
            if let Some(&ck) = self.features.overview.clients.get(self.features.overview.index) {
                if let Some(client) = self.state.clients.get(ck) {
                    backend.compositor_set_overview_selection(client.win);
                }
            }
        } else {
            // Sliding window: keep selected index near center of 6-window view.
            let half = 3usize;
            let new_start = if self.features.overview.index < half {
                0
            } else if self.features.overview.index + half >= len {
                len.saturating_sub(6)
            } else {
                self.features.overview.index - half
            };
            let window_end = (new_start + 6).min(len);

            if new_start != self.features.overview.slide_offset {
                // Window shifted: refresh prism with new 6-client subset.
                self.features.overview.slide_offset = new_start;
                let subset: Vec<ClientKey> = self.features.overview.clients[new_start..window_end].to_vec();
                let mut layout = self.build_overview_layout(&subset);
                // Mark the correct entry as selected.
                let sel_in_window = self.features.overview.index - new_start;
                for (i, entry) in layout.iter_mut().enumerate() {
                    entry.5 = i == sel_in_window;
                }
                backend.compositor_set_overview_mode(true, &layout);
            }
            // Set selection (rotation) to the face within the current window.
            let sel_in_window = self.features.overview.index - new_start;
            if let Some(&ck) = self.features.overview.clients.get(self.features.overview.index) {
                if let Some(client) = self.state.clients.get(ck) {
                    backend.compositor_set_overview_selection(client.win);
                }
            }
            let _ = sel_in_window; // used implicitly via set_overview_selection face_index
        }
        Ok(())
    }

    pub fn toggle_magnifier(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.features.magnifier.enabled = !self.features.magnifier.enabled;
        backend.compositor_set_magnifier(self.features.magnifier.enabled);
        Ok(())
    }

    pub fn toggle_peek(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.features.peek_active = !self.features.peek_active;
        backend.compositor_set_peek_mode(self.features.peek_active);
        Ok(())
    }

    pub fn toggle_recording(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.features.recording.active = !self.features.recording.active;
        if self.features.recording.active {
            let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
            let cfg_dir = CONFIG.load().behavior().recording_output_dir.clone();
            let videos_dir = if !cfg_dir.is_empty() {
                cfg_dir
            } else {
                std::env::var("XDG_VIDEOS_DIR")
                    .or_else(|_| std::env::var("HOME").map(|h| format!("{}/Videos", h)))
                    .unwrap_or_else(|_| "/tmp".to_string())
            };
            let mut output_dir = std::path::PathBuf::from(&videos_dir);
            if let Err(e) = std::fs::create_dir_all(&output_dir) {
                warn!(
                    "[toggle_recording] cannot create output dir '{}': {}, fallback to /tmp",
                    output_dir.display(),
                    e
                );
                output_dir = std::path::PathBuf::from("/tmp");
            }
            let output_path = output_dir
                .join(format!("recording-{}.mp4", timestamp))
                .to_string_lossy()
                .to_string();
            let seg_path = format!("/tmp/jwm-rec-{}-seg0.mp4", timestamp);

            self.features.recording.output_path = Some(output_path.clone());
            self.features.recording.segments = Vec::new();
            self.features.recording.current_segment = Some(seg_path.clone());
            Self::save_recording_state(&output_path, &[]);

            info!(
                "[toggle_recording] start → {} (segment: {})",
                output_path, seg_path
            );
            backend.compositor_start_recording(&seg_path);
        } else {
            backend.compositor_stop_recording();
            // Collect current segment
            if let Some(seg) = self.features.recording.current_segment.take() {
                self.features.recording.segments.push(seg);
            }
            let segments = std::mem::take(&mut self.features.recording.segments);
            let output_path = self.features.recording.output_path.take().unwrap_or_default();
            info!(
                "[toggle_recording] stop → {} ({} segments)",
                output_path,
                segments.len()
            );
            Self::finalize_recording(segments, output_path);
        }
        Ok(())
    }

    const RECORDING_STATE_FILE: &'static str = "/tmp/jwm-recording-state";

    fn save_recording_state(output_path: &str, segments: &[String]) {
        let mut content = output_path.to_string();
        for seg in segments {
            content.push('\n');
            content.push_str(seg);
        }
        if let Err(e) = std::fs::write(Self::RECORDING_STATE_FILE, &content) {
            warn!("[recording] failed to save state: {e}");
        }
    }

    fn load_recording_state() -> Option<(String, Vec<String>)> {
        let content = std::fs::read_to_string(Self::RECORDING_STATE_FILE).ok()?;
        let mut lines = content.lines();
        let output_path = lines.next()?.to_string();
        if output_path.is_empty() {
            return None;
        }
        let segments: Vec<String> = lines
            .map(|l| l.to_string())
            .filter(|l| !l.is_empty())
            .collect();
        Some((output_path, segments))
    }

    fn clear_recording_state() {
        let _ = std::fs::remove_file(Self::RECORDING_STATE_FILE);
    }

    /// Concatenate segments into final output, or rename if single segment.
    fn finalize_recording(segments: Vec<String>, output_path: String) {
        std::thread::spawn(move || {
            if segments.is_empty() {
                Self::clear_recording_state();
                return;
            }
            if segments.len() == 1 {
                // Single segment: just move it to the final path
                if std::fs::rename(&segments[0], &output_path).is_err() {
                    let _ = std::fs::copy(&segments[0], &output_path);
                    let _ = std::fs::remove_file(&segments[0]);
                }
            } else {
                // Multiple segments: concat with ffmpeg -c copy
                let list_path = "/tmp/jwm-recording-concat.txt";
                let list_content: String = segments
                    .iter()
                    .map(|s| format!("file '{}'", s))
                    .collect::<Vec<_>>()
                    .join("\n");
                if std::fs::write(list_path, &list_content).is_ok() {
                    let _ = std::process::Command::new("ffmpeg")
                        .args([
                            "-f",
                            "concat",
                            "-safe",
                            "0",
                            "-i",
                            list_path,
                            "-c",
                            "copy",
                            "-y",
                            &output_path,
                        ])
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status();
                    let _ = std::fs::remove_file(list_path);
                }
                for seg in &segments {
                    let _ = std::fs::remove_file(seg);
                }
            }
            Self::clear_recording_state();
            log::info!("[recording] finalized → {output_path}");
        });
    }

    /// Auto-resume recording after restart if state file exists.
    pub fn resume_recording_if_needed(&mut self, backend: &mut dyn Backend) {
        if let Some((output_path, segments)) = Self::load_recording_state() {
            let seg_index = segments.len();
            // Derive timestamp from output path for consistent naming
            let base = std::path::Path::new(&output_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .trim_start_matches("recording-");
            let seg_path = format!("/tmp/jwm-rec-{}-seg{}.mp4", base, seg_index);

            self.features.recording.output_path = Some(output_path);
            self.features.recording.segments = segments;
            self.features.recording.current_segment = Some(seg_path.clone());
            self.features.recording.active = true;

            backend.compositor_start_recording(&seg_path);
            info!("[recording] auto-resumed from restart (segment {seg_index}: {seg_path})");
        }
    }

    pub fn toggle_expose(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.features.expose_active {
            self.features.expose_active = false;
            backend.compositor_set_expose_mode(false, vec![]);
            let _ = backend.key_ops().ungrab_keyboard();
            let _ = backend.input_ops().ungrab_pointer();
        } else {
            // Collect visible windows across all monitors
            let mut windows: Vec<(WindowId, i32, i32, u32, u32)> = Vec::new();
            for &mon_key in &self.state.monitor_order.clone() {
                if let Some(clients) = self.state.monitor_clients.get(mon_key) {
                    for &ck in clients {
                        if !self.is_client_visible_on_monitor(ck, mon_key) {
                            continue;
                        }
                        if let Some(client) = self.state.clients.get(ck) {
                            let g = &client.geometry;
                            if g.w > 0 && g.h > 0 {
                                windows.push((client.win, g.x, g.y, g.w as u32, g.h as u32));
                            }
                        }
                    }
                }
            }
            if windows.is_empty() {
                return Ok(());
            }
            self.features.expose_active = true;
            backend.compositor_set_expose_mode(true, windows);
            if let Some(root) = backend.root_window() {
                let _ = backend.key_ops().grab_keyboard(root);
            }
            let pointer_mask = (EventMaskBits::BUTTON_PRESS
                | EventMaskBits::BUTTON_RELEASE
                | EventMaskBits::POINTER_MOTION)
                .bits();
            let _ = backend.input_ops().grab_pointer(pointer_mask, None);
        }
        Ok(())
    }

    fn update_sticky_tags(&mut self, mon_key: MonitorKey) {
        let new_tags = if let Some(monitor) = self.state.monitors.get(mon_key) {
            monitor.get_active_tags()
        } else {
            return;
        };
        let client_keys: Vec<ClientKey> = self
            .state
            .monitor_clients
            .get(mon_key)
            .map(|keys| keys.clone())
            .unwrap_or_default();
        for ck in client_keys {
            if let Some(client) = self.state.clients.get_mut(ck) {
                if client.state.is_sticky {
                    client.state.tags = new_tags;
                }
            }
        }
    }

    /// Toggle a named scratchpad.
    ///
    /// Argument encoding (via `StringVec`):
    ///   `["name", "cmd", "arg1", ...]`  — name + spawn command
    ///   `["name"]`                      — name only (uses default scratchpad terminal)
    ///
    /// Legacy `Int(0)` falls back to the default name `"term"`.
    pub fn togglescratchpad(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cfg = CONFIG.load();
        // Parse name and optional command from argument
        let (name, spawn_cmd) = match arg {
            WMArgEnum::StringVec(v) if !v.is_empty() => {
                let name = v[0].clone();
                let cmd = if v.len() > 1 {
                    v[1..].to_vec()
                } else {
                    crate::config::Config::get_scratchpad_termcmd()
                };
                (name, cmd)
            }
            _ => (
                "term".to_string(),
                crate::config::Config::get_scratchpad_termcmd(),
            ),
        };

        // Check if the scratchpad's client still exists
        if let Some(&sp_key) = self.scratchpads.get(&name) {
            if self.state.clients.get(sp_key).is_none() {
                self.scratchpads.remove(&name);
            }
        }

        if let Some(&sp_key) = self.scratchpads.get(&name) {
            // Scratchpad exists — toggle visibility
            let is_visible = self.is_client_visible_by_key(sp_key);
            if is_visible {
                // Hide: animate upward then hide
                if let Some(client) = self.state.clients.get(sp_key) {
                    let current_rect = Rect::new(
                        client.geometry.x,
                        client.geometry.y,
                        client.geometry.w,
                        client.geometry.h,
                    );
                    // Target: move up by window height
                    let hidden_y = current_rect.y - current_rect.h - 100;
                    let hidden_rect =
                        Rect::new(current_rect.x, hidden_y, current_rect.w, current_rect.h);

                    if cfg.animation_enabled() {
                        self.animations.start(
                            sp_key,
                            current_rect,
                            hidden_rect,
                            cfg.animation_duration(),
                            cfg.animation_easing(),
                            AnimationKind::Hide,
                        );
                    } else {
                        // If animations disabled, immediately hide
                        if let Some(c) = self.state.clients.get_mut(sp_key) {
                            c.state.tags = 0;
                        }
                    }
                }

                // Mark for deferred hiding after animation completes
                if let Some(c) = self.state.clients.get_mut(sp_key) {
                    c.state.tags = 0;
                }

                let mon_key = self.state.clients.get(sp_key).and_then(|c| c.mon);
                self.focus(backend, None)?;
                if let Some(mk) = mon_key {
                    self.arrange(backend, Some(mk));
                }
            } else {
                // Show: animate downward from top
                let sel_mon_key = self.state.sel_mon;
                if let Some(mon_key) = sel_mon_key {
                    let current_tags = self
                        .state
                        .monitors
                        .get(mon_key)
                        .map(|m| m.get_active_tags())
                        .unwrap_or(1);

                    if let Some(client) = self.state.clients.get_mut(sp_key) {
                        client.state.tags = current_tags;
                        client.mon = Some(mon_key);
                        client.state.is_floating = true;
                    }

                    self.reorder_client_in_monitor_groups(sp_key);

                    // Center at 80% of monitor work area
                    if let Some(area) = self.monitor_work_area(mon_key) {
                        let w = (area.w as f32 * 0.8) as i32;
                        let h = (area.h as f32 * 0.8) as i32;
                        let x = area.x + (area.w - w) / 2;
                        let y = area.y + (area.h - h) / 2;

                        // Suppress animation during resize to set target position
                        let suppress_flag = self.suppress_layout_animation;
                        self.suppress_layout_animation = true;
                        self.resize_client(backend, sp_key, x, y, w, h, false);
                        self.suppress_layout_animation = suppress_flag;
                    }

                    self.focus(backend, Some(sp_key))?;
                    self.arrange(backend, Some(mon_key));

                    // After arrange, get actual position and start downward animation
                    if let Some(area) = self.monitor_work_area(mon_key) {
                        let w = (area.w as f32 * 0.8) as i32;
                        let h = (area.h as f32 * 0.8) as i32;
                        let x = area.x + (area.w - w) / 2;
                        let y = area.y + (area.h - h) / 2;

                        if cfg.animation_enabled() {
                            // Animate from above screen to target position
                            // from_y: window top is at (area.y - h), so window is completely above visible area
                            let from_y = area.y - h;
                            let from_rect = Rect::new(x, from_y, w, h);
                            let to_rect = Rect::new(x, y, w, h);

                            info!(
                                "[togglescratchpad] scratchpad show animation from y={} to y={}",
                                from_y, y
                            );

                            self.animations.start(
                                sp_key,
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
        } else {
            // No scratchpad with this name — spawn command, mark pending
            if let Some(prog) = spawn_cmd.first() {
                let mut command = Command::new(prog);
                command.args(&spawn_cmd[1..]);

                Self::setup_smithay_child_env(&mut command, backend);
                command
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::inherit())
                    .stderr(std::process::Stdio::inherit());
                Self::apply_child_pre_exec(&mut command);

                match command.spawn() {
                    Ok(child) => {
                        info!("[togglescratchpad] spawned '{}' PID: {}", name, child.id());
                        self.scratchpad_pending_name = Some(name);
                    }
                    Err(e) => {
                        error!("[togglescratchpad] failed to spawn '{}': {}", name, e);
                    }
                }
            }
        }
        Ok(())
    }

    pub fn togglepip(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(sel_mon_key) = self.state.sel_mon else {
            return Ok(());
        };
        let Some(sel_client_key) = self.state.monitors.get(sel_mon_key).and_then(|m| m.sel) else {
            return Ok(());
        };

        let is_pip = self
            .state
            .clients
            .get(sel_client_key)
            .map(|c| c.state.is_pip)
            .unwrap_or(false);

        if is_pip {
            // Exit PiP: restore state
            if let Some(client) = self.state.clients.get_mut(sel_client_key) {
                client.state.is_pip = false;
                client.state.is_floating = client.state.old_state;
                client.state.is_sticky = false;
            }
            self.reorder_client_in_monitor_groups(sel_client_key);
            let (fx, fy, fw, fh) = if let Some(client) = self.state.clients.get(sel_client_key) {
                (
                    client.geometry.floating_x,
                    client.geometry.floating_y,
                    client.geometry.floating_w,
                    client.geometry.floating_h,
                )
            } else {
                return Ok(());
            };
            if fw > 0 && fh > 0 {
                self.resize_client(backend, sel_client_key, fx, fy, fw, fh, false);
            }
            self.arrange(backend, Some(sel_mon_key));
        } else {
            // Enter PiP: save state, shrink to bottom-right
            if let Some(client) = self.state.clients.get_mut(sel_client_key) {
                client.state.old_state = client.state.is_floating;
                client.geometry.floating_x = client.geometry.x;
                client.geometry.floating_y = client.geometry.y;
                client.geometry.floating_w = client.geometry.w;
                client.geometry.floating_h = client.geometry.h;
                client.state.is_pip = true;
                client.state.is_floating = true;
                client.state.is_sticky = true;
            }

            self.reorder_client_in_monitor_groups(sel_client_key);

            // Position at bottom-right, 25% of monitor, 10px padding
            if let Some(area) = self.monitor_work_area(sel_mon_key) {
                let w = (area.w as f32 * 0.25) as i32;
                let h = (area.h as f32 * 0.25) as i32;
                let x = area.x + area.w - w - 10;
                let y = area.y + area.h - h - 10;
                self.resize_client(backend, sel_client_key, x, y, w, h, false);
            }

            self.arrange(backend, Some(sel_mon_key));
            self.restack(backend, Some(sel_mon_key))?;
        }

        // Notify compositor of PiP state change
        if backend.has_compositor() {
            if let Some(client) = self.state.clients.get(sel_client_key) {
                backend.compositor_set_window_pip(client.win, client.state.is_pip);
            }
        }

        Ok(())
    }

    fn focusin(
        &mut self,
        backend: &mut dyn Backend,
        event_window: WindowId,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let sel_client_key = self.get_selected_client_key();
        if let Some(client_key) = sel_client_key {
            if let Some(client) = self.state.clients.get(client_key) {
                if event_window != client.win {
                    if self.wintoclient(event_window).is_some() {
                        self.setfocus(backend, client_key)?;
                    } else {
                        // 是未知窗口（可能是输入法、系统弹窗等），允许它持有焦点
                        // 不要调用 setfocus
                        // debug!("Focus stolen by unmanaged window, ignoring allow...");
                    }
                }
            }
        }
        Ok(())
    }

    pub fn focusmon(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.state.monitor_order.len() <= 1 {
            return Ok(());
        }

        if let WMArgEnum::Int(i) = arg {
            if let Some(target_mon_key) = self.dirtomon(i) {
                if Some(target_mon_key) == self.state.sel_mon {
                    return Ok(());
                }
                self.switch_to_monitor(backend, target_mon_key)?;
                self.focus(backend, None)?;

                let mon_num = self.state.monitors.get(target_mon_key).map(|m| m.num);
                if let Some(num) = mon_num {
                    self.broadcast_ipc_event(
                        "monitor/focus",
                        serde_json::json!({
                            "monitor": num,
                        }),
                    );
                }
            }
        }
        Ok(())
    }

    /// Prepare screenshot output path (shared by both interactive and fullscreen).
    fn prepare_screenshot_path() -> Option<String> {
        let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let pictures_dir = std::env::var("XDG_PICTURES_DIR")
            .or_else(|_| std::env::var("HOME").map(|h| format!("{}/Pictures", h)))
            .unwrap_or_else(|_| "/tmp".to_string());
        let mut output_dir = std::path::PathBuf::from(&pictures_dir);
        if let Err(e) = std::fs::create_dir_all(&output_dir) {
            warn!(
                "[take_screenshot] cannot create output dir '{}': {}, fallback to /tmp",
                output_dir.display(),
                e
            );
            output_dir = std::path::PathBuf::from("/tmp");
            if let Err(e2) = std::fs::create_dir_all(&output_dir) {
                error!(
                    "[take_screenshot] cannot create fallback dir '{}': {}",
                    output_dir.display(),
                    e2
                );
                return None;
            }
        }
        Some(
            output_dir
                .join(format!("screenshot-{}.png", timestamp))
                .to_string_lossy()
                .to_string(),
        )
    }

    /// Alt+S: enter interactive region selection mode.
    pub fn take_screenshot(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // If already in selection mode, cancel it first
        if self.features.screenshot.active {
            self.cancel_screenshot_select(backend);
            return Ok(());
        }

        let screenshot_path = match Self::prepare_screenshot_path() {
            Some(p) => p,
            None => return Ok(()),
        };

        info!(
            "[take_screenshot] entering interactive region selection mode → {}",
            screenshot_path
        );
        self.features.screenshot.active = true;
        self.features.screenshot.dragging = false;
        self.features.screenshot.committed = false;
        self.features.screenshot.start = (0.0, 0.0);
        self.features.screenshot.end = (0.0, 0.0);
        self.features.screenshot.output_path = Some(screenshot_path);

        // Grab keyboard (to intercept Escape)
        if let Some(root) = backend.root_window() {
            let _ = backend.key_ops().grab_keyboard(root);
        }
        // Grab pointer with crosshair cursor
        let crosshair_handle = backend
            .cursor_provider()
            .get(StdCursorKind::Crosshair)
            .ok()
            .map(|h| h.0);
        let pointer_mask = (EventMaskBits::BUTTON_PRESS
            | EventMaskBits::BUTTON_RELEASE
            | EventMaskBits::POINTER_MOTION)
            .bits();
        let _ = backend
            .input_ops()
            .grab_pointer(pointer_mask, crosshair_handle);

        Ok(())
    }

    /// Alt+Shift+S: take a full-screen screenshot immediately.
    pub fn take_screenshot_fullscreen(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let screenshot_path = match Self::prepare_screenshot_path() {
            Some(p) => p,
            None => return Ok(()),
        };

        let path = std::path::PathBuf::from(&screenshot_path);
        match backend.take_screenshot_to_file(&path) {
            Ok(true) => {
                info!(
                    "[take_screenshot_fullscreen] compositor screenshot → {}",
                    path.display()
                );
            }
            Ok(false) => {
                info!(
                    "[take_screenshot_fullscreen] backend doesn't support compositor screenshots"
                );
            }
            Err(e) => {
                error!("[take_screenshot_fullscreen] compositor screenshot failed: {e}");
            }
        }
        Ok(())
    }

    /// Cancel interactive screenshot selection mode.
    fn cancel_screenshot_select(&mut self, backend: &mut dyn Backend) {
        info!("[take_screenshot] cancelling region selection");
        self.features.screenshot.active = false;
        self.features.screenshot.dragging = false;
        self.features.screenshot.committed = false;
        self.features.screenshot.output_path = None;
        if backend.has_compositor() {
            backend.compositor_set_snap_preview(None);
        }
        let _ = backend.key_ops().ungrab_keyboard();
        let _ = backend.input_ops().ungrab_pointer();
        // Restore default cursor
        if let Some(root) = backend.root_window() {
            let _ = backend
                .cursor_provider()
                .apply(root, StdCursorKind::LeftPtr);
        }
    }

    /// Finish interactive screenshot selection: capture the selected region.
    /// If `to_clipboard` is true, the image is copied to the system clipboard
    /// instead of saved to a file.
    fn finish_screenshot_select(&mut self, backend: &mut dyn Backend, to_clipboard: bool) {
        let path_str = match self.features.screenshot.output_path.take() {
            Some(p) => p,
            None => {
                self.cancel_screenshot_select(backend);
                return;
            }
        };

        let (sx, sy) = self.features.screenshot.start;
        let (ex, ey) = self.features.screenshot.end;

        // Compute normalized rectangle
        let x = sx.min(ex) as i32;
        let y = sy.min(ey) as i32;
        let w = (sx - ex).abs() as u32;
        let h = (sy - ey).abs() as u32;

        // Clear state before capturing
        self.features.screenshot.active = false;
        self.features.screenshot.dragging = false;
        self.features.screenshot.committed = false;
        if backend.has_compositor() {
            backend.compositor_clear_snap_preview_immediate();
        }
        let _ = backend.key_ops().ungrab_keyboard();
        let _ = backend.input_ops().ungrab_pointer();
        if let Some(root) = backend.root_window() {
            let _ = backend
                .cursor_provider()
                .apply(root, StdCursorKind::LeftPtr);
        }

        if w < 3 || h < 3 {
            info!(
                "[take_screenshot] selection too small ({}x{}), ignoring",
                w, h
            );
            return;
        }

        // When copying to clipboard, use a temp file as an intermediate
        let save_path = if to_clipboard {
            format!("/tmp/.jwm-screenshot-clipboard-{}.png", std::process::id())
        } else {
            path_str.clone()
        };

        let path = std::path::PathBuf::from(&save_path);
        let captured = match backend.take_screenshot_region_to_file(&path, x, y, w, h) {
            Ok(true) => {
                info!(
                    "[take_screenshot] region screenshot → {} ({}x{} at {},{})",
                    path.display(),
                    w,
                    h,
                    x,
                    y
                );
                true
            }
            Ok(false) => {
                info!(
                    "[take_screenshot] backend doesn't support region screenshots, falling back to full"
                );
                backend.take_screenshot_to_file(&path).unwrap_or(false)
            }
            Err(e) => {
                error!("[take_screenshot] region screenshot failed: {e}");
                false
            }
        };

        if to_clipboard && captured {
            Self::copy_image_to_clipboard(backend, &save_path);
        }
    }

    /// Copy a PNG image file to the system clipboard using xclip or wl-copy.
    ///
    /// The screenshot is captured asynchronously by the compositor on the next
    /// render frame, so the PNG file does not exist yet when this is called.
    /// We spawn a shell wrapper that polls for the file before running the
    /// clipboard tool.
    fn copy_image_to_clipboard(backend: &dyn Backend, png_path: &str) {
        let copy_cmd = if Self::is_udev_backend(backend) {
            format!("wl-copy -t image/png < '{}'", png_path)
        } else {
            format!("xclip -selection clipboard -t image/png -i '{}'", png_path)
        };

        // Poll up to 3 s for the file to appear (compositor writes it next frame),
        // then copy to clipboard and remove the temp file.
        let script = format!(
            r#"for i in $(seq 1 60); do [ -s '{}' ] && {{ {}; rm -f '{}'; exit 0; }}; sleep 0.05; done"#,
            png_path, copy_cmd, png_path,
        );

        info!("[take_screenshot] clipboard copy scheduled: {}", copy_cmd);

        let mut command = Command::new("sh");
        command.args(["-c", &script]);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        match command.spawn() {
            Ok(_) => {
                info!("[take_screenshot] clipboard copy helper spawned");
            }
            Err(e) => {
                error!("[take_screenshot] failed to spawn clipboard helper: {e}");
            }
        }
    }

    pub fn tag(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // info!("[tag]");
        if let WMArgEnum::UInt(ui) = *arg {
            let sel_client_key = self.get_selected_client_key();
            let target_tag = ui & CONFIG.load().tagmask();

            if let Some(client_key) = sel_client_key {
                if target_tag > 0 {
                    if let Some(client) = self.state.clients.get_mut(client_key) {
                        client.state.tags = target_tag;
                    }
                    let _ = self.setclienttagprop(backend, client_key);

                    self.focus(backend, None)?;
                    self.arrange(backend, self.state.sel_mon);
                }
            }
        }
        Ok(())
    }

    pub fn tagmon(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // info!("[tagmon]");

        let sel_client_key = self.get_selected_client_key();
        if sel_client_key.is_none() {
            return Ok(());
        }
        if self.state.monitor_order.len() <= 1 {
            return Ok(());
        }
        if let WMArgEnum::Int(i) = *arg {
            let target_mon = self.dirtomon(&i);
            if let (Some(client_key), Some(target_mon_key)) = (sel_client_key, target_mon) {
                self.sendmon(backend, Some(client_key), Some(target_mon_key));
            }
        }
        Ok(())
    }

    fn sendmon(
        &mut self,
        backend: &mut dyn Backend,
        client_key_opt: Option<ClientKey>,
        target_mon_opt: Option<MonitorKey>,
    ) {
        // info!("[sendmon]");

        let client_key = match client_key_opt {
            Some(key) => key,
            None => return,
        };

        let target_mon_key = match target_mon_opt {
            Some(key) => key,
            None => return,
        };

        if let Some(client) = self.state.clients.get(client_key) {
            if client.mon == Some(target_mon_key) {
                return;
            }
        } else {
            return;
        }

        let _ = self.unfocus_client(backend, client_key, true);

        self.detach(client_key);
        self.detachstack(client_key);

        if let Some(client) = self.state.clients.get_mut(client_key) {
            client.mon = Some(target_mon_key);
        }

        if let Some(target_monitor) = self.state.monitors.get(target_mon_key) {
            let target_tags = target_monitor.get_active_tags();

            if let Some(client) = self.state.clients.get_mut(client_key) {
                client.state.tags = target_tags;
            }
        }

        self.attach_back(client_key);
        self.attachstack(client_key);

        let _ = self.setclienttagprop(backend, client_key);

        let _ = self.focus(backend, None);
        self.arrange(backend, None);
    }

    pub fn focusstack(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // In scrolling layout, Alt+j/k navigates within column
        if self.is_scrolling_layout() {
            return self.scrolling_focus_window(backend, arg);
        }

        let direction = match *arg {
            WMArgEnum::Int(i) => i,
            _ => return Ok(()),
        };

        if direction == 0 {
            return Ok(());
        }

        if !self.can_focus_switch()? {
            return Ok(());
        }

        let target_client = if direction > 0 {
            self.find_next_visible_client()?
        } else {
            self.find_previous_visible_client()?
        };

        if let Some(client_key) = target_client {
            self.focus(backend, Some(client_key))?;
            self.restack(backend, self.state.sel_mon)?;

            // V-stack: re-arrange so new focus moves to center
            if self.is_vstack_layout() {
                if let Some(mk) = self.state.sel_mon {
                    // Save each visible tiled client's current visual rect BEFORE
                    // arrangemon overwrites client.geometry.  When the compositor
                    // is active, resizeclient moves the real X11 window to the
                    // target instantly, so the old geometry values that resizeclient
                    // stores in old_x/old_y can already equal the target from a
                    // previous identical layout pass, causing the animation to be
                    // skipped (current_visual == target).  By snapshotting the
                    // visual rect here we can inject the correct "from" rect.
                    let pre_rects: HashMap<ClientKey, Rect> = {
                        let now = Instant::now();
                        self.collect_tileable_clients(mk)
                            .iter()
                            .map(|&(k, _, _)| {
                                let visual = self
                                    .animations
                                    .current_visual_rect(k, now)
                                    .or_else(|| {
                                        self.state.clients.get(k).map(|c| {
                                            Rect::new(
                                                c.geometry.x,
                                                c.geometry.y,
                                                c.geometry.w,
                                                c.geometry.h,
                                            )
                                        })
                                    })
                                    .unwrap_or_default();
                                (k, visual)
                            })
                            .collect()
                    };

                    self.arrangemon(backend, mk);

                    // Patch animations: always retarget changed clients from the
                    // pre-snapshot visual rect to the new layout target so vstack
                    // focus cycling (Alt+j/k) consistently shows move animation.
                    for (ck, pre_rect) in &pre_rects {
                        if let Some(client) = self.state.clients.get(*ck) {
                            let target = Rect::new(
                                client.geometry.x,
                                client.geometry.y,
                                client.geometry.w,
                                client.geometry.h,
                            );
                            if *pre_rect != target {
                                let cfg = CONFIG.load();
                                if cfg.animation_enabled() {
                                    let duration = cfg.animation_duration();
                                    let easing = cfg.animation_easing();
                                    self.animations.start(
                                        *ck,
                                        *pre_rect,
                                        target,
                                        duration,
                                        easing,
                                        AnimationKind::Layout,
                                    );
                                }
                            }
                        }
                    }

                    let _ = self.restack(backend, Some(mk));
                }
            }

            self.suppress_mouse_focus_until =
                Some(std::time::Instant::now() + std::time::Duration::from_millis(200));
        }
        Ok(())
    }

    /// IPC: focus_none — 取消所有窗口焦点，聚焦到 root window
    pub fn focus_none(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[focus_none]");
        self.focus(backend, None)
    }

    /// IPC: focus_window — 按窗口 ID 聚焦指定窗口
    pub fn focus_window(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let win_id = match arg {
            WMArgEnum::UInt64(id) => *id,
            _ => return Err("focus_window requires a window id".into()),
        };
        info!("[focus_window] id={}", win_id);
        let win = WindowId::from_raw(win_id);
        let client_key = self
            .wintoclient(win)
            .ok_or_else(|| format!("window {} not found", win_id))?;
        self.focus(backend, Some(client_key))?;
        if let Some(mon_key) = self.state.sel_mon {
            self.restack(backend, Some(mon_key))?;
        }
        Ok(())
    }

    /// Switch to a tab in a window group
    pub fn focus_tab(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Tab info passed as Vec of [group_id, tab_index]
        let args = match arg {
            WMArgEnum::StringVec(v) if v.len() >= 2 => v,
            _ => return Err("focus_tab requires group_id and tab_index".into()),
        };

        let group_id: u32 = args[0].parse()?;
        let tab_index: usize = args[1].parse()?;
        info!("[focus_tab] group_id={}, tab_index={}", group_id, tab_index);

        // Get the focused window in this group
        if let Some((_, tabs_info)) = self.get_tab_group(group_id) {
            if tab_index < tabs_info.len() {
                let target_win = tabs_info[tab_index].0; // x11_win from tab info
                self.focus_window(backend, &WMArgEnum::UInt64(target_win as u64))?;
                return Ok(());
            }
        }
        Err(format!("tab group {}/{} not found", group_id, tab_index).into())
    }

    /// Get tab group information by group_id
    fn get_tab_group(&self, _group_id: u32) -> Option<(u32, Vec<(u32, String)>)> {
        None
    }

    /// IPC: refocus — unfocus 当前窗口再 focus 回来
    pub fn refocus(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[refocus]");
        let sel_client_key = match self.get_selected_client_key() {
            Some(k) => k,
            None => return Ok(()),
        };
        // 1. unfocus → root
        self.unfocus_client(backend, sel_client_key, true)?;
        self.set_root_focus(backend)?;
        self.update_monitor_selection_by_key(None);
        // 2. focus 回来
        self.focus(backend, Some(sel_client_key))?;
        if let Some(mon_key) = self.state.sel_mon {
            self.restack(backend, Some(mon_key))?;
        }
        Ok(())
    }

    fn can_focus_switch(&self) -> Result<bool, Box<dyn std::error::Error>> {
        let sel_client_key = self.get_selected_client_key().ok_or("No selected client")?;

        if let Some(client) = self.state.clients.get(sel_client_key) {
            let is_locked_fullscreen =
                client.state.is_fullscreen && CONFIG.load().behavior().lock_fullscreen;
            Ok(!is_locked_fullscreen)
        } else {
            Err("Selected client not found".into())
        }
    }

    fn find_next_visible_client(&self) -> Result<Option<ClientKey>, Box<dyn std::error::Error>> {
        let sel_mon_key = self.state.sel_mon.ok_or("No selected monitor")?;
        let current_sel = self.get_selected_client_key().ok_or("No selected client")?;
        let (tile_clients, floating_clients) = self.grouped_visible_clients(sel_mon_key);
        let current_is_floating = self
            .state
            .clients
            .get(current_sel)
            .map(|client| client.state.is_floating)
            .unwrap_or(false);

        let (current_group, other_group) = if current_is_floating {
            (&floating_clients, &tile_clients)
        } else {
            (&tile_clients, &floating_clients)
        };

        if let Some(next) = Self::next_in_group(current_group, current_sel) {
            return Ok(Some(next));
        }

        if let Some(next) = other_group.first().copied() {
            return Ok(Some(next));
        }

        // Wrap around to the first of current group
        if let Some(next) = current_group.first().copied() {
            if next != current_sel {
                return Ok(Some(next));
            }
        }

        Ok(None)
    }

    fn find_previous_visible_client(
        &self,
    ) -> Result<Option<ClientKey>, Box<dyn std::error::Error>> {
        let sel_mon_key = self.state.sel_mon.ok_or("No selected monitor")?;
        let current_sel = self.get_selected_client_key().ok_or("No selected client")?;
        let (tile_clients, floating_clients) = self.grouped_visible_clients(sel_mon_key);
        let current_is_floating = self
            .state
            .clients
            .get(current_sel)
            .map(|client| client.state.is_floating)
            .unwrap_or(false);

        let (current_group, other_group) = if current_is_floating {
            (&floating_clients, &tile_clients)
        } else {
            (&tile_clients, &floating_clients)
        };

        if let Some(prev) = Self::prev_in_group(current_group, current_sel) {
            return Ok(Some(prev));
        }

        if let Some(prev) = other_group.last().copied() {
            return Ok(Some(prev));
        }

        // Wrap around to the last of current group
        if let Some(prev) = current_group.last().copied() {
            if prev != current_sel {
                return Ok(Some(prev));
            }
        }

        Ok(None)
    }

    fn grouped_visible_clients(&self, mon_key: MonitorKey) -> (Vec<ClientKey>, Vec<ClientKey>) {
        let total = self
            .state
            .monitor_clients
            .get(mon_key)
            .map(|v| v.len())
            .unwrap_or(0);
        let mut tile_clients = Vec::with_capacity(total);
        let mut floating_clients = Vec::with_capacity(total / 4 + 1);

        if let Some(client_list) = self.state.monitor_clients.get(mon_key) {
            for &client_key in client_list {
                if !self.is_client_visible_on_monitor(client_key, mon_key) {
                    continue;
                }

                if let Some(client) = self.state.clients.get(client_key) {
                    if client.state.is_floating {
                        floating_clients.push(client_key);
                    } else {
                        tile_clients.push(client_key);
                    }
                }
            }
        }

        (tile_clients, floating_clients)
    }

    fn next_in_group(group: &[ClientKey], current_sel: ClientKey) -> Option<ClientKey> {
        group
            .iter()
            .position(|&k| k == current_sel)
            .and_then(|idx| group.get(idx + 1).copied())
    }

    fn prev_in_group(group: &[ClientKey], current_sel: ClientKey) -> Option<ClientKey> {
        group
            .iter()
            .position(|&k| k == current_sel)
            .and_then(|idx| {
                idx.checked_sub(1)
                    .and_then(|prev_idx| group.get(prev_idx).copied())
            })
    }

    pub fn togglebar(
        &mut self,
        _backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[togglebar]");

        let sel_mon_key = match self.state.sel_mon {
            Some(key) => key,
            None => return Ok(()),
        };

        let mut monitor_num_opt: Option<i32> = None;
        {
            if let Some(monitor) = self.state.monitors.get_mut(sel_mon_key) {
                if let Some(ref mut pertag) = monitor.pertag {
                    let cur_tag = pertag.cur_tag;
                    if let Some(show_bar) = pertag.show_bars.get_mut(cur_tag) {
                        *show_bar = !*show_bar;
                        info!(
                            "[togglebar] show_bar[mon={}, tag={}] -> {}",
                            monitor.num, cur_tag, show_bar
                        );
                        monitor_num_opt = Some(monitor.num);
                    }
                }
            }
        }

        if let Some(mon_num) = monitor_num_opt {
            self.mark_bar_update_needed_if_visible(Some(mon_num));
        }

        Ok(())
    }


    pub fn setcfact(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[setcfact]");

        // In scrolling layout, Alt+Shift+h/l focuses columns
        if self.is_scrolling_layout() {
            if let WMArgEnum::Float(f) = arg {
                let dir = if *f > 0.0 { -1 } else { 1 };
                return self.scrolling_focus_column(backend, &WMArgEnum::Int(dir));
            }
        }

        let client_key = match self.get_selected_client_key() {
            Some(k) => k,
            None => return Ok(()),
        };

        if let WMArgEnum::Float(f0) = *arg {
            let current_fact = if let Some(client) = self.state.clients.get(client_key) {
                client.state.client_fact
            } else {
                return Ok(());
            };

            let new_fact = if f0.abs() < 0.0001 {
                1.0
            } else {
                f0 + current_fact
            };

            if new_fact < 0.25 || new_fact > 4.0 {
                return Ok(());
            }

            if let Some(client) = self.state.clients.get_mut(client_key) {
                client.state.client_fact = new_fact;
                info!(
                    "[setcfact] Updated client_fact to {} for client '{}'",
                    new_fact, client.name
                );
            }
            self.arrange(backend, self.state.sel_mon);
        }

        Ok(())
    }

    pub fn movestack(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // In scrolling layout, Alt+Shift+j/k moves within column, Alt+Shift+h/l moves columns
        if self.is_scrolling_layout() {
            return self.scrolling_move_column(backend, arg);
        }

        let direction = match arg {
            WMArgEnum::Int(i) => *i,
            _ => return Ok(()),
        };

        let selected_client_key = self.get_selected_client_key().ok_or("No client selected")?;

        let target_client_key = if direction > 0 {
            self.find_next_tiled_client(selected_client_key)?
        } else {
            self.find_previous_tiled_client(selected_client_key)?
        };

        if let Some(target_key) = target_client_key {
            if selected_client_key != target_key {
                self.swap_clients_in_monitor(selected_client_key, target_key)?;

                self.arrange(backend, self.state.sel_mon);

                self.suppress_mouse_focus_until =
                    Some(std::time::Instant::now() + std::time::Duration::from_millis(200));
            }
        }

        Ok(())
    }


    fn is_tiled_and_visible(&self, client_key: ClientKey) -> bool {
        if let Some(client) = self.state.clients.get(client_key) {
            self.is_client_visible_by_key(client_key) && !client.state.is_floating
        } else {
            false
        }
    }

    fn find_next_tiled_client(
        &self,
        current_key: ClientKey,
    ) -> Result<Option<ClientKey>, Box<dyn std::error::Error>> {
        let sel_mon_key = self.state.sel_mon.ok_or("No selected monitor")?;
        let client_list = self
            .state
            .monitor_clients
            .get(sel_mon_key)
            .ok_or("Monitor client list not found")?;

        let current_index = client_list
            .iter()
            .position(|&k| k == current_key)
            .ok_or("Current client not found in monitor list")?;

        for &client_key in &client_list[current_index + 1..] {
            if self.is_tiled_and_visible(client_key) {
                return Ok(Some(client_key));
            }
        }

        for &client_key in &client_list[..current_index] {
            if self.is_tiled_and_visible(client_key) {
                return Ok(Some(client_key));
            }
        }

        Ok(None)
    }

    fn find_previous_tiled_client(
        &self,
        current_key: ClientKey,
    ) -> Result<Option<ClientKey>, Box<dyn std::error::Error>> {
        let sel_mon_key = self.state.sel_mon.ok_or("No selected monitor")?;
        let client_list = self
            .state
            .monitor_clients
            .get(sel_mon_key)
            .ok_or("Monitor client list not found")?;

        let current_index = client_list
            .iter()
            .position(|&k| k == current_key)
            .ok_or("Current client not found in monitor list")?;

        for &client_key in client_list[..current_index].iter().rev() {
            if self.is_tiled_and_visible(client_key) {
                return Ok(Some(client_key));
            }
        }

        for &client_key in client_list[current_index + 1..].iter().rev() {
            if self.is_tiled_and_visible(client_key) {
                return Ok(Some(client_key));
            }
        }

        Ok(None)
    }

    fn swap_clients_in_monitor(
        &mut self,
        client1_key: ClientKey,
        client2_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let sel_mon_key = self.state.sel_mon.ok_or("No selected monitor")?;

        if let Some(client_list) = self.state.monitor_clients.get_mut(sel_mon_key) {
            let pos1 = client_list
                .iter()
                .position(|&k| k == client1_key)
                .ok_or("Client1 not found in monitor list")?;
            let pos2 = client_list
                .iter()
                .position(|&k| k == client2_key)
                .ok_or("Client2 not found in monitor list")?;

            client_list.swap(pos1, pos2);
        }

        if let Some(stack_list) = self.state.monitor_stack.get_mut(sel_mon_key) {
            if let (Some(pos1), Some(pos2)) = (
                stack_list.iter().position(|&k| k == client1_key),
                stack_list.iter().position(|&k| k == client2_key),
            ) {
                stack_list.swap(pos1, pos2);
            }
        }

        info!(
            "[swap_clients_in_monitor] Swapped clients {:?} and {:?}",
            client1_key, client2_key
        );
        Ok(())
    }







    pub fn zoom(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // info!("[zoom]");

        let sel_mon_key = match self.state.sel_mon {
            Some(key) => key,
            None => return Ok(()),
        };

        let selected_client_key = if let Some(monitor) = self.state.monitors.get(sel_mon_key) {
            monitor.sel
        } else {
            return Ok(());
        };

        let selected_client_key = match selected_client_key {
            Some(key) => key,
            None => return Ok(()), // 没有选中的客户端
        };

        if let Some(client) = self.state.clients.get(selected_client_key) {
            if client.state.is_floating {
                return Ok(()); // 浮动窗口不参与zoom
            }
        } else {
            return Ok(());
        }

        let first_tiled = self.nexttiled(sel_mon_key, None);

        let target_client_key = if Some(selected_client_key) == first_tiled {
            self.nexttiled(sel_mon_key, Some(selected_client_key))
        } else {
            Some(selected_client_key)
        };

        if let Some(client_key) = target_client_key {
            self.pop(backend, client_key);
        }

        Ok(())
    }

    pub fn loopview(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[loopview]");

        let direction = match arg {
            WMArgEnum::Int(val) => *val,
            _ => return Ok(()),
        };

        if direction == 0 {
            return Ok(());
        }

        let next_tag = self.calculate_next_tag(direction);

        if self.is_same_tag(next_tag) {
            return Ok(());
        }

        let (sel_mon_key, old_tag_mask) = match self.state.sel_mon {
            Some(k) => {
                let old = self
                    .state
                    .monitors
                    .get(k)
                    .map(|m| m.get_active_tags())
                    .unwrap_or(next_tag);
                (k, old)
            }
            None => return Ok(()),
        };

        // Trigger compositor transition for loopview shortcuts (Alt+Tab/PageUp/PageDown).
        let mut transitioning = false;
        if backend.has_compositor() {
            let cfg = CONFIG.load();
            if cfg.animation_enabled()
                && self.should_animate_tag_switch(sel_mon_key, old_tag_mask, next_tag)
            {
                let dir = Self::tag_switch_direction(old_tag_mask, next_tag, cfg.tags_length());
                let mon_rect = self.monitor_rect(sel_mon_key);
                backend.compositor_notify_tag_switch(
                    cfg.animation_duration(),
                    dir,
                    self.tag_transition_exclude_top(sel_mon_key),
                    mon_rect,
                );
                transitioning = true;
            }
        }

        info!(
            "[loopview] next_tag: {}, direction: {}",
            next_tag, direction
        );

        let cur_tag = self.switch_to_tag(next_tag, next_tag)?;
        if let Some(sel_mon_key) = self.state.sel_mon {
            self.update_sticky_tags(sel_mon_key);
        }

        let sel_opt = self.apply_pertag_settings(cur_tag)?;

        self.focus(backend, sel_opt)?;
        // Suppress layout animations during tag transition so target windows
        // appear instantly (the compositor overlay handles the visual effect).
        self.suppress_layout_animation = transitioning;
        self.arrange(backend, self.state.sel_mon.clone());
        self.suppress_layout_animation = false;

        Ok(())
    }

    fn calculate_next_tag(&self, direction: i32) -> u32 {
        let current_tag = if let Some(sel_mon_key) = self.state.sel_mon {
            if let Some(monitor) = self.state.monitors.get(sel_mon_key) {
                monitor.get_active_tags()
            } else {
                warn!("[calculate_next_tag] Selected monitor not found");
                return 1; // 返回默认的第一个标签
            }
        } else {
            warn!("[calculate_next_tag] No monitor selected");
            return 1; // 返回默认的第一个标签
        };

        let current_tag_index = if current_tag == 0 {
            0 // 如果当前没有选中的tag，从第一个开始
        } else {
            current_tag.trailing_zeros() as usize
        };

        const MAX_TAGS: usize = 9;
        let next_tag_index = if direction > 0 {
            (current_tag_index + 1) % MAX_TAGS
        } else {
            if current_tag_index == 0 {
                MAX_TAGS - 1
            } else {
                current_tag_index - 1
            }
        };
        let next_tag = 1 << next_tag_index;

        info!(
            "[calculate_next_tag] current_tag: {}, next_tag: {}, direction: {}",
            current_tag, next_tag, direction
        );

        next_tag
    }

    fn primary_tag_index(mask: u32) -> Option<usize> {
        if mask == 0 || mask == u32::MAX {
            return None;
        }
        Some(mask.trailing_zeros() as usize)
    }

    // Returns +1 for forward (higher tag), -1 for backward (lower tag).
    // Uses shortest circular direction to keep wrap-around natural.
    fn tag_switch_direction(old_mask: u32, new_mask: u32, tags_len: usize) -> i32 {
        let Some(old_idx) = Self::primary_tag_index(old_mask) else {
            return 1;
        };
        let Some(new_idx) = Self::primary_tag_index(new_mask) else {
            return 1;
        };
        if old_idx == new_idx || tags_len == 0 {
            return 1;
        }

        let direct = new_idx as i32 - old_idx as i32;
        let wrap_forward = direct + tags_len as i32;
        let wrap_backward = direct - tags_len as i32;

        // Pick the delta with smallest absolute distance.
        let mut best = direct;
        if wrap_forward.abs() < best.abs() {
            best = wrap_forward;
        }
        if wrap_backward.abs() < best.abs() {
            best = wrap_backward;
        }

        if best >= 0 { 1 } else { -1 }
    }

    pub fn view(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let ui = match arg {
            WMArgEnum::UInt(val) => *val,
            _ => return Ok(()),
        };
        let cfg = CONFIG.load();
        let target_mask = ui & cfg.tagmask();

        let sel_mon_key = match self.state.sel_mon {
            Some(k) => k,
            None => return Ok(()),
        };

        // 1. 检查是否无需切换
        if let Some(mon) = self.state.monitors.get(sel_mon_key) {
            if crate::core::workspace::WorkspaceManager::is_same_tag(mon, target_mask) {
                return Ok(());
            }
        }

        // 2. 状态变更 (纯逻辑)
        let mut client_to_focus = None;
        let mut old_tag_mask = 0u32;
        let mut new_tag_mask = target_mask;
        if let Some(monitor) = self.state.monitors.get_mut(sel_mon_key) {
            old_tag_mask = monitor.get_active_tags();
            monitor.view_tag(target_mask, false); // false = not toggle, direct set
            new_tag_mask = monitor.get_active_tags();
            // 获取该 Tag 上次选中的 Client
            client_to_focus = monitor.get_selected_client_for_current_tag();
        }
        self.update_sticky_tags(sel_mon_key);

        // 3. 副作用 (Backend / Arrange)
        // Notify compositor to capture old scene for slide transition
        let mut transitioning = false;
        if backend.has_compositor() {
            if cfg.animation_enabled()
                && self.should_animate_tag_switch(sel_mon_key, old_tag_mask, new_tag_mask)
            {
                let direction =
                    Self::tag_switch_direction(old_tag_mask, new_tag_mask, cfg.tags_length());
                let mon_rect = self.monitor_rect(sel_mon_key);
                let exclude_top = self.tag_transition_exclude_top(sel_mon_key);
                backend.compositor_notify_tag_switch(
                    cfg.animation_duration(),
                    direction,
                    exclude_top,
                    mon_rect,
                );
                transitioning = true;
            }
        }
        self.focus(backend, client_to_focus)?;
        self.suppress_layout_animation = transitioning;
        self.arrange(backend, Some(sel_mon_key));
        self.suppress_layout_animation = false;
        self.update_ewmh_desktop(backend)?;

        self.broadcast_ipc_event(
            "tag/view",
            serde_json::json!({
                "tag": target_mask,
            }),
        );

        Ok(())
    }

    fn is_same_tag(&self, target_tag: u32) -> bool {
        if let Some(sel_mon_key) = self.state.sel_mon {
            if let Some(monitor) = self.state.monitors.get(sel_mon_key) {
                return target_tag == monitor.get_active_tags();
            }
        }
        false
    }

    fn switch_to_tag(
        &mut self,
        target_tag: u32,
        ui: u32,
    ) -> Result<usize, Box<dyn std::error::Error>> {
        let sel_mon_key = match self.state.sel_mon {
            Some(k) => k,
            None => return Ok(0),
        };
        let sel_mon_mut = if let Some(sel_mon) = self.state.monitors.get_mut(sel_mon_key) {
            sel_mon
        } else {
            return Ok(0);
        };

        info!("[switch_to_tag] tag_set: {:?}", sel_mon_mut.tag_set);
        info!("[switch_to_tag] old sel_tags: {}", sel_mon_mut.sel_tags);

        sel_mon_mut.sel_tags ^= 1;
        let new_sel_tags = sel_mon_mut.sel_tags;
        info!("[switch_to_tag] new sel_tags: {}", new_sel_tags);

        let cur_tag = if target_tag > 0 {
            sel_mon_mut.tag_set[new_sel_tags] = target_tag;

            let new_cur_tag = if ui == !0 {
                0 // 显示所有标签
            } else {
                ui.trailing_zeros() as usize + 1
            };

            if let Some(pertag) = sel_mon_mut.pertag.as_mut() {
                pertag.prev_tag = pertag.cur_tag;
                pertag.cur_tag = new_cur_tag;
            }

            new_cur_tag
        } else {
            if let Some(pertag) = sel_mon_mut.pertag.as_mut() {
                std::mem::swap(&mut pertag.prev_tag, &mut pertag.cur_tag);
                pertag.cur_tag
            } else {
                return Err("No pertag information available".into());
            }
        };

        info!(
            "[switch_to_tag] prev_tag: {}, cur_tag: {}",
            sel_mon_mut.pertag.as_ref().map(|p| p.prev_tag).unwrap_or(0),
            cur_tag
        );

        Ok(cur_tag)
    }

    fn apply_pertag_settings(
        &mut self,
        cur_tag: usize,
    ) -> Result<Option<ClientKey>, Box<dyn std::error::Error>> {
        let sel_mon_key = self.state.sel_mon.ok_or("No monitor selected")?;

        let (n_master, m_fact, sel_lt, layout_0, layout_1, sel_client_key) = {
            let monitor = self
                .state
                .monitors
                .get(sel_mon_key)
                .ok_or("Selected monitor not found")?;

            let pertag = monitor
                .pertag
                .as_ref()
                .ok_or("No pertag information available")?;

            let sel_lt = pertag.sel_lts[cur_tag];
            (
                pertag.n_masters[cur_tag],
                pertag.m_facts[cur_tag],
                sel_lt,
                pertag.lt_idxs[cur_tag][sel_lt]
                    .clone()
                    .ok_or("Layout not found")?,
                pertag.lt_idxs[cur_tag][sel_lt ^ 1]
                    .clone()
                    .ok_or("Alternative layout not found")?,
                pertag.sel[cur_tag],
            )
        };

        if let Some(monitor) = self.state.monitors.get_mut(sel_mon_key) {
            monitor.layout.n_master = n_master;
            monitor.layout.m_fact = m_fact;
            monitor.sel_lt = sel_lt;
            monitor.lt[sel_lt] = layout_0;
            monitor.lt[sel_lt ^ 1] = layout_1;
        } else {
            return Err("Monitor disappeared during operation".into());
        }

        if let Some(client_key) = sel_client_key {
            if let Some(client) = self.state.clients.get(client_key) {
                info!(
                    "[apply_pertag_settings] selected client: {} (key: {:?})",
                    client.name, client_key
                );
            } else {
                warn!(
                    "[apply_pertag_settings] selected client key {:?} not found",
                    client_key
                );
            }
        }

        Ok(sel_client_key)
    }

    pub fn toggleview(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let ui = match arg {
            WMArgEnum::UInt(val) => *val,
            _ => return Ok(()),
        };
        let cfg = CONFIG.load();
        let mask = ui & cfg.tagmask();
        let sel_mon_key = self.state.sel_mon.ok_or("No monitor selected")?;

        // 1. 状态变更
        let mut old_tag_mask = 0u32;
        let mut new_tag_mask = mask;
        if let Some(monitor) = self.state.monitors.get_mut(sel_mon_key) {
            old_tag_mask = monitor.get_active_tags();
            monitor.view_tag(mask, true); // true = toggle
            new_tag_mask = monitor.get_active_tags();
        }
        self.update_sticky_tags(sel_mon_key);

        // 2. 副作用
        // Notify compositor to capture old scene for slide transition
        let mut transitioning = false;
        if backend.has_compositor() {
            if cfg.animation_enabled()
                && self.should_animate_tag_switch(sel_mon_key, old_tag_mask, new_tag_mask)
            {
                let direction =
                    Self::tag_switch_direction(old_tag_mask, new_tag_mask, cfg.tags_length());
                let mon_rect = self.monitor_rect(sel_mon_key);
                let exclude_top = self.tag_transition_exclude_top(sel_mon_key);
                backend.compositor_notify_tag_switch(
                    cfg.animation_duration(),
                    direction,
                    exclude_top,
                    mon_rect,
                );
                transitioning = true;
            }
        }
        self.focus(backend, None)?;
        self.suppress_layout_animation = transitioning;
        self.arrange(backend, Some(sel_mon_key));
        self.suppress_layout_animation = false;
        self.update_ewmh_desktop(backend)?;

        Ok(())
    }

    pub fn toggletag(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[toggletag]");

        let sel_client_key = if let Some(sel_mon_key) = self.state.sel_mon {
            if let Some(monitor) = self.state.monitors.get(sel_mon_key) {
                monitor.sel
            } else {
                return Ok(());
            }
        } else {
            return Ok(());
        };

        let sel_client_key = match sel_client_key {
            Some(key) => key,
            None => return Ok(()),
        };

        if let WMArgEnum::UInt(ui) = *arg {
            let current_tags = if let Some(client) = self.state.clients.get(sel_client_key) {
                client.state.tags
            } else {
                warn!("[toggletag] Selected client {:?} not found", sel_client_key);
                return Ok(());
            };

            let newtags = current_tags ^ (ui & CONFIG.load().tagmask());

            if newtags > 0 {
                if let Some(client) = self.state.clients.get_mut(sel_client_key) {
                    client.state.tags = newtags;
                } else {
                    return Ok(());
                }

                self.setclienttagprop(backend, sel_client_key)?;

                self.focus(backend, None)?;
                self.arrange(backend, self.state.sel_mon);
            }
        }

        Ok(())
    }

    pub fn quit(
        &mut self,
        _backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // info!("[quit]");
        self.running.store(false, Ordering::SeqCst);
        Ok(())
    }

    pub fn setup(&mut self, backend: &mut dyn Backend) -> Result<(), Box<dyn std::error::Error>> {
        info!("[setup]");
        let _ = self.updategeom(backend);
        backend.register_wm("jwm")?;

        let mask = (EventMaskBits::SUBSTRUCTURE_REDIRECT
            | EventMaskBits::SUBSTRUCTURE_NOTIFY
            | EventMaskBits::STRUCTURE_NOTIFY
            | EventMaskBits::BUTTON_PRESS
            | EventMaskBits::KEY_RELEASE
            | EventMaskBits::POINTER_MOTION
            | EventMaskBits::ENTER_WINDOW
            | EventMaskBits::LEAVE_WINDOW
            | EventMaskBits::PROPERTY_CHANGE)
            .bits();

        let root = backend.root_window().expect("no root window");
        backend
            .cursor_provider()
            .apply(root, StdCursorKind::LeftPtr)?;
        backend
            .window_ops()
            .change_event_mask(backend.root_window().expect("no root window"), mask)?;
        self.grabkeys(backend)?;
        self.focus(backend, None)?;

        self.setup_initial_windows(backend)?;

        self.arrange(backend, None);
        let _ = self.restack(backend, self.state.sel_mon);
        let _ = self.focus(backend, None);
        let _ = self.update_ewmh_desktop(backend);

        backend.window_ops().flush()?;
        Ok(())
    }

    pub fn killclient(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[killclient]");
        let sel_client_key = match self.get_selected_client_key() {
            Some(k) => k,
            None => return Ok(()),
        };

        let client_win = if let Some(c) = self.state.clients.get(sel_client_key) {
            c.win
        } else {
            return Ok(());
        };

        info!("[killclient] Closing window {:?}", client_win);
        let res = backend.window_ops().close_window(client_win)?;
        if res == crate::backend::api::CloseResult::Forced {
            info!("[killclient] Force killed client");
        } else {
            info!("[killclient] Sent graceful close request");
        }

        Ok(())
    }

    fn handle_transient_for_change(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // info!("[handle_transient_for_change]");
        let (is_floating, win, client_name) =
            if let Some(client) = self.state.clients.get(client_key) {
                (client.state.is_floating, client.win, client.name.clone())
            } else {
                return Ok(());
            };

        if !is_floating {
            let transient_for = self.get_transient_for(backend, win);
            if let Some(parent_window) = transient_for {
                if self.wintoclient(parent_window).is_some() {
                    if let Some(client) = self.state.clients.get_mut(client_key) {
                        client.state.is_floating = true;
                    }

                    self.reorder_client_in_monitor_groups(client_key);

                    debug!(
                        "Window '{}' became floating due to transient_for: {:?}",
                        client_name, parent_window
                    );

                    let mon_key = self.state.clients.get(client_key).and_then(|c| c.mon);
                    self.arrange(backend, mon_key);
                }
            }
        }
        Ok(())
    }

    fn handle_normal_hints_change(
        &mut self,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(client) = self.state.clients.get_mut(client_key) {
            client.size_hints.hints_valid = false;
        }
        Ok(())
    }

    fn handle_wm_hints_change(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.updatewmhints(backend, client_key);
        self.mark_bar_update_needed_if_visible(None);

        if let Some(client) = self.state.clients.get(client_key) {
            debug!("WM hints updated for window {:?}", client.win);
        }
        Ok(())
    }

    fn updatetitle_by_key(&mut self, backend: &mut dyn Backend, client_key: ClientKey) {
        let win = if let Some(client) = self.state.clients.get(client_key) {
            client.win
        } else {
            return;
        };
        let new_title = self.fetch_window_title(backend, win);
        let title_for_event;
        if let Some(client) = self.state.clients.get_mut(client_key) {
            client.name = new_title;
            title_for_event = client.name.clone();
            debug!("Updated title for window {:?}: '{}'", win, client.name);
        } else {
            return;
        }
        self.broadcast_ipc_event(
            "window/title",
            serde_json::json!({
                "id": win.raw(), "name": title_for_event,
            }),
        );
    }

    fn truncate_chars(input: String, max_chars: usize) -> String {
        if input.is_empty() {
            return input;
        }
        let mut count = 0usize;
        let mut truncate_at = input.len();
        for (idx, _) in input.char_indices() {
            if count >= max_chars {
                truncate_at = idx;
                break;
            }
            count += 1;
        }
        let mut s = input;
        s.truncate(truncate_at);
        s
    }

    fn fetch_window_title(&mut self, backend: &mut dyn Backend, window: WindowId) -> String {
        let title = backend.property_ops().get_title(window);
        Self::truncate_chars(title, STEXT_MAX_LEN)
    }

    fn handle_title_change(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.updatetitle_by_key(backend, client_key);

        let should_update_bar = self.is_client_selected(client_key);

        if should_update_bar {
            let monitor_id = self
                .state
                .clients
                .get(client_key)
                .and_then(|client| client.mon)
                .and_then(|mon_key| self.state.monitors.get(mon_key))
                .map(|monitor| monitor.num);

            if let Some(id) = monitor_id {
                self.mark_bar_update_needed_if_visible(Some(id));

                if let Some(client) = self.state.clients.get(client_key) {
                    debug!(
                        "Title updated for selected window {:?}, updating status bar",
                        client.win
                    );
                }
            }
        }
        Ok(())
    }

    fn handle_window_type_change(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.updatewindowtype(backend, client_key);

        if let Some(client) = self.state.clients.get(client_key) {
            debug!("Window type updated for window {:?}", client.win);
        }
        Ok(())
    }

    fn handle_class_change(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let win = self
            .state
            .clients
            .get(client_key)
            .map(|c| c.win)
            .ok_or("Client not found")?;

        let (inst, cls) = backend.property_ops().get_class(win);
        if let Some(client) = self.state.clients.get_mut(client_key) {
            if !inst.is_empty() {
                client.instance = inst;
            }
            if !cls.is_empty() {
                client.class = cls;
            }
        }
        Ok(())
    }

    pub fn movemouse(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let client_key = self.get_selected_client_key().ok_or("No client selected")?;

        // 获取只读引用进行检查
        let (is_fullscreen, is_floating, win_id) =
            if let Some(c) = self.state.clients.get(client_key) {
                (c.state.is_fullscreen, c.state.is_floating, c.win)
            } else {
                return Ok(());
            };

        if is_fullscreen {
            return Ok(());
        }

        // 浮动检查：如果是平铺窗口，自动切换为浮动（保持当前几何，不恢复历史 floating_*）
        if !is_floating {
            self.enable_floating_keep_geometry(backend, client_key)?;
        }
        debug!(
            "Initiating move for window {:?} (floating: {}, fullscreen: {})",
            win_id, !is_floating, is_fullscreen
        );

        // [修改] 提升窗口堆叠顺序
        self.restack(backend, self.state.sel_mon)?;

        // [修改] 将控制权移交 Backend
        backend.begin_move(win_id)?;

        // Notify compositor of window move start (for wobbly windows effect)
        if backend.has_compositor() {
            backend.compositor_notify_window_move_start(win_id);
        }

        // Jwm 不再维护 InteractionState
        Ok(())
    }

    // [重构] resizemouse
    pub fn resizemouse(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let client_key = self.get_selected_client_key().ok_or("No client selected")?;

        let (is_fullscreen, is_floating, win_id) =
            if let Some(c) = self.state.clients.get(client_key) {
                (c.state.is_fullscreen, c.state.is_floating, c.win)
            } else {
                return Ok(());
            };

        if is_fullscreen {
            return Ok(());
        }

        if !is_floating {
            self.enable_floating_keep_geometry(backend, client_key)?;
        }

        self.restack(backend, self.state.sel_mon)?;

        // [修改] 将控制权移交 Backend。
        // Wayland/udev 通常不能 warp 指针，所以根据鼠标落点选择更直观的 resize 边/角：
        // - 靠近边：Top/Bottom/Left/Right
        // - 靠近角：TopLeft/TopRight/BottomLeft/BottomRight
        // - 中间区域：退化为象限选择（避免出现“怎么拖都不动”的感觉）
        let geom = backend.window_ops().get_geometry(win_id)?;
        let (px, py) = backend.input_ops().get_pointer_position()?;

        let w = (geom.w as f64).max(1.0);
        let h = (geom.h as f64).max(1.0);

        let rel_x = px - geom.x as f64;
        let rel_y = py - geom.y as f64;

        // Dynamic grip size: small windows still get a usable edge area.
        let threshold = 24.0_f64.min(w / 3.0).min(h / 3.0).max(8.0);

        let near_left = rel_x <= threshold;
        let near_right = rel_x >= (w - threshold);
        let near_top = rel_y <= threshold;
        let near_bottom = rel_y >= (h - threshold);

        let edge = if near_top && near_left {
            crate::backend::api::ResizeEdge::TopLeft
        } else if near_top && near_right {
            crate::backend::api::ResizeEdge::TopRight
        } else if near_bottom && near_left {
            crate::backend::api::ResizeEdge::BottomLeft
        } else if near_bottom && near_right {
            crate::backend::api::ResizeEdge::BottomRight
        } else if near_top {
            crate::backend::api::ResizeEdge::Top
        } else if near_bottom {
            crate::backend::api::ResizeEdge::Bottom
        } else if near_left {
            crate::backend::api::ResizeEdge::Left
        } else if near_right {
            crate::backend::api::ResizeEdge::Right
        } else {
            // Not near any border: pick a quadrant as a reasonable default.
            let left = rel_x < (w / 2.0);
            let top = rel_y < (h / 2.0);
            match (top, left) {
                (true, true) => crate::backend::api::ResizeEdge::TopLeft,
                (true, false) => crate::backend::api::ResizeEdge::TopRight,
                (false, true) => crate::backend::api::ResizeEdge::BottomLeft,
                (false, false) => crate::backend::api::ResizeEdge::BottomRight,
            }
        };

        backend.begin_resize(win_id, edge)?;

        Ok(())
    }

    fn check_monitor_consistency(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // 类似于之前的 check_monitor_change_after_resize
        let client_key = match self.get_selected_client_key() {
            Some(k) => k,
            None => return Ok(()),
        };

        let (x, y) = match self.state.clients.get(client_key) {
            Some(client) => (client.geometry.x, client.geometry.y),
            None => return Ok(()),
        };

        let target_monitor = self.recttomon(backend, x, y);
        if let Some(target_mon_key) = target_monitor {
            if Some(target_mon_key) != self.state.sel_mon {
                self.sendmon(backend, Some(client_key), Some(target_mon_key));
                self.state.sel_mon = Some(target_mon_key);
                self.focus(backend, None)?;
            }
        }
        Ok(())
    }

    fn setclienttagprop(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(client) = self.state.clients.get(client_key) {
            let monitor_num = client
                .mon
                .and_then(|mk| self.state.monitors.get(mk))
                .map(|m| m.num as u32)
                .unwrap_or(0);

            backend.property_ops().set_client_info_props(
                client.win,
                client.state.tags,
                monitor_num,
            )?;
        }
        Ok(())
    }

    fn mouse_focus_blocked(&mut self) -> bool {
        if let Some(deadline) = self.suppress_mouse_focus_until {
            if std::time::Instant::now() < deadline {
                return true;
            }
            self.suppress_mouse_focus_until = None;
        }
        false
    }

    fn switch_to_monitor(
        &mut self,
        backend: &mut dyn Backend,
        target_monitor_key: MonitorKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.handle_monitor_switch_by_key(backend, Some(target_monitor_key))
    }

    fn should_focus_client(
        &self,
        client_key_opt: Option<ClientKey>,
        is_on_selected_monitor: bool,
    ) -> bool {
        if !is_on_selected_monitor {
            return true;
        }

        if client_key_opt.is_none() {
            return true;
        }

        let current_selected = self.get_selected_client_key();
        current_selected != client_key_opt
    }

    fn expose(
        &mut self,
        backend: &mut dyn Backend,
        window: WindowId,
        count: u16,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // info!("[expose]");
        if count != 0 {
            return Ok(());
        }

        if let Some(monitor_key) = self.wintomon(backend, Some(window)) {
            if let Some(monitor) = self.state.monitors.get(monitor_key) {
                self.mark_bar_update_needed_if_visible(Some(monitor.num));
            }
        }

        Ok(())
    }

    fn focus(
        &mut self,
        backend: &mut dyn Backend,
        mut client_key_opt: Option<ClientKey>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[focus]");

        let is_visible = match client_key_opt {
            Some(client_key) => self.is_client_visible_by_key(client_key),
            None => false,
        };

        if !is_visible {
            client_key_opt = self.find_visible_client();
        }

        self.handle_focus_change_by_key(backend, &client_key_opt)?;

        if let Some(client_key) = client_key_opt {
            self.set_client_focus_by_key(backend, client_key)?;
        } else {
            self.set_root_focus(backend)?;
        }

        self.update_monitor_selection_by_key(client_key_opt);

        self.mark_bar_update_needed_if_visible(None);

        // Broadcast focus event
        if let Some(ck) = client_key_opt {
            let event_data = self
                .state
                .clients
                .get(ck)
                .map(|c| (c.win.raw(), c.name.clone()));
            if let Some((id, name)) = event_data {
                self.broadcast_ipc_event(
                    "window/focus",
                    serde_json::json!({
                        "id": id, "name": name,
                    }),
                );
            }
        }

        Ok(())
    }


    fn update_net_client_list(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut ordered: Vec<WindowId> = Vec::with_capacity(self.state.client_order.len());
        for &key in &self.state.client_order {
            if let Some(client) = self.state.clients.get(key) {
                ordered.push(client.win);
            }
        }

        let mut stacking: Vec<WindowId> = Vec::new();
        for &mon_key in &self.state.monitor_order {
            if let Some(stack) = self.state.monitor_stack.get(mon_key) {
                for &ck in stack.iter().rev() {
                    if let Some(c) = self.state.clients.get(ck) {
                        stacking.push(c.win);
                    }
                }
            }
        }

        backend.on_client_list_changed(&ordered, &stacking)?;
        Ok(())
    }

    fn setclientstate(
        &self,
        backend: &mut dyn Backend,
        win: WindowId,
        state: i64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        Ok(backend.property_ops().set_wm_state(win, state)?)
    }



    fn updatewindowtype(&mut self, backend: &mut dyn Backend, client_key: ClientKey) {
        // 先获取必要信息，避免借用冲突
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

        // 处理全屏
        if backend.property_ops().is_fullscreen(win) {
            let _ = self.setfullscreen(backend, client_key, true);
        }

        // 获取窗口类型
        let types = backend.property_ops().get_window_types(win);
        let is_desktop = types.contains(&WindowType::Desktop);
        let is_dock = types.contains(&WindowType::Dock);
        let is_transient = backend.property_ops().transient_for(win).is_some();

        let layer_info = backend.property_ops().get_layer_surface_info(win);

        // 获取可变引用进行修改
        if let Some(c) = self.state.clients.get_mut(client_key) {
            c.state.is_dock = is_dock;
            c.state.dock_layer_info = if is_dock { layer_info } else { None };

            // 1. 如果是 Popup / Dock / Notification / Desktop
            if is_popup_like || is_desktop {
                c.state.is_floating = true;

                // 如果是 通知、Dock、桌面，则设置为所有标签可见
                // 但如果窗口是 transient（有父窗口），说明是用户交互触发的子窗口，
                // 不应设置 never_focus，否则会导致弹窗失焦后被应用自动关闭
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

    fn updatewmhints(&mut self, backend: &mut dyn Backend, client_key: ClientKey) {
        let win = match self.state.clients.get(client_key) {
            Some(c) => c.win,
            None => return,
        };
        if let Some(hints) = backend.property_ops().get_wm_hints(win) {
            if hints.urgent {
                let is_focused = self.is_client_selected(client_key);
                if is_focused {
                    let _ = backend.property_ops().set_urgent_hint(win, false);
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

    fn update_bar_message_for_monitor(&mut self, mon_key_opt: Option<MonitorKey>) {
        // info!("[update_bar_message_for_monitor]");

        let mon_key = match mon_key_opt {
            Some(key) => key,
            None => {
                error!("Monitor key is None, cannot update bar message.");
                return;
            }
        };

        let monitor = if let Some(monitor) = self.state.monitors.get(mon_key) {
            monitor
        } else {
            error!("Monitor {:?} not found", mon_key);
            return;
        };

        self.message = SharedMessage::default();
        let mut monitor_info_for_message = MonitorInfo::default();

        monitor_info_for_message.monitor_x = monitor.geometry.w_x;
        monitor_info_for_message.monitor_y = monitor.geometry.w_y;
        monitor_info_for_message.monitor_width = monitor.geometry.w_w;
        monitor_info_for_message.monitor_height = monitor.geometry.w_h;
        monitor_info_for_message.monitor_num = monitor.num;
        monitor_info_for_message.set_ltsymbol(&monitor.lt_symbol);

        let (occupied_tags_mask, urgent_tags_mask) = self.calculate_tag_masks(mon_key);

        for i in 0..CONFIG.load().tags_length() {
            let tag_bit = 1 << i;

            let is_filled_tag = self.is_filled_tag(mon_key, tag_bit);

            let monitor = match self.state.monitors.get(mon_key) {
                Some(m) => m,
                None => break,
            };
            let active_tagset = monitor.get_active_tags();
            let is_selected_tag = (active_tagset & tag_bit) != 0;
            let is_urgent_tag = (urgent_tags_mask & tag_bit) != 0;
            let is_occupied_tag = (occupied_tags_mask & tag_bit) != 0;
            let tag_status = TagStatus::new(
                is_selected_tag,
                is_urgent_tag,
                is_filled_tag,
                is_occupied_tag,
            );
            monitor_info_for_message.set_tag_status(i, tag_status);
        }
        let selected_client_name = self.get_selected_client_name(mon_key);
        monitor_info_for_message.set_client_name(&selected_client_name);
        self.message.monitor_info = monitor_info_for_message;
    }

    fn calculate_tag_masks(&self, mon_key: MonitorKey) -> (u32, u32) {
        let monitor_clients = self.state.monitor_clients.get(mon_key).map(|v| v.as_slice()).unwrap_or(&[]);
        StatusBarBuilder::calculate_tag_masks(&self.state.clients, monitor_clients)
    }

    fn is_filled_tag(&self, mon_key: MonitorKey, tag_bit: u32) -> bool {
        let is_selected = self.state.sel_mon == Some(mon_key);
        if let Some(monitor) = self.state.monitors.get(mon_key) {
            StatusBarBuilder::is_filled_tag(&self.state.clients, monitor, tag_bit, is_selected)
        } else {
            false
        }
    }

    fn get_selected_client_name(&self, mon_key: MonitorKey) -> String {
        if let Some(monitor) = self.state.monitors.get(mon_key) {
            StatusBarBuilder::get_selected_client_name(&self.state.clients, monitor)
        } else {
            String::new()
        }
    }

    // =========================================================================
    // IPC processing
    // =========================================================================

    fn process_ipc(&mut self, backend: &mut dyn Backend) {
        let ipc = match self.ipc_server.as_mut() {
            Some(s) => s,
            None => return,
        };

        ipc.accept_connections();
        let messages = ipc.poll_clients();

        for msg in messages {
            match msg {
                IncomingIpc::Command {
                    client_id,
                    name,
                    args,
                } => {
                    let resp = self.handle_ipc_command(backend, &name, &args);
                    if let Some(ipc) = self.ipc_server.as_mut() {
                        ipc.respond(client_id, &resp);
                    }
                }
                IncomingIpc::Query {
                    client_id,
                    name,
                    args,
                } => {
                    let resp = self.handle_ipc_query(&name, &args, backend);
                    if let Some(ipc) = self.ipc_server.as_mut() {
                        ipc.respond(client_id, &resp);
                    }
                }
                IncomingIpc::Subscribe { client_id, topics } => {
                    if let Some(ipc) = self.ipc_server.as_mut() {
                        ipc.subscribe(client_id, topics);
                        ipc.respond(client_id, &IpcResponse::ok(None));
                    }
                }
            }
        }
    }

    fn handle_ipc_command(
        &mut self,
        backend: &mut dyn Backend,
        name: &str,
        args: &serde_json::Value,
    ) -> IpcResponse {
        // Special command: reload_config
        if name == "reload_config" {
            return self.do_config_reload(backend);
        }

        match ipc::dispatch_command(name, args) {
            Ok((func, arg)) => match func(self, backend, &arg) {
                Ok(()) => IpcResponse::ok(None),
                Err(e) => IpcResponse::err(format!("{e}")),
            },
            Err(e) => IpcResponse::err(e),
        }
    }

    fn handle_ipc_query(
        &self,
        name: &str,
        _args: &serde_json::Value,
        backend: &dyn Backend,
    ) -> IpcResponse {
        let cfg = CONFIG.load();
        match name {
            "get_windows" => {
                let windows = self.query_windows();
                IpcResponse::ok(Some(serde_json::to_value(windows).unwrap_or_default()))
            }
            "get_workspaces" => {
                let workspaces = self.query_workspaces();
                IpcResponse::ok(Some(serde_json::to_value(workspaces).unwrap_or_default()))
            }
            "get_monitors" => {
                let monitors = self.query_monitors();
                IpcResponse::ok(Some(serde_json::to_value(monitors).unwrap_or_default()))
            }
            "get_tree" => {
                let tree = self.query_tree();
                IpcResponse::ok(Some(serde_json::to_value(tree).unwrap_or_default()))
            }
            "get_config" => {
                IpcResponse::ok(Some(serde_json::json!({
                    "border_px": cfg.border_px(),
                    "gap_px": cfg.gap_px(),
                    "snap": cfg.snap(),
                    "m_fact": cfg.m_fact(),
                    "n_master": cfg.n_master(),
                    "tags_length": cfg.tags_length(),
                    "show_bar": cfg.show_bar(),
                })))
            }
            "get_version" => IpcResponse::ok(Some(serde_json::json!({
                "version": env!("CARGO_PKG_VERSION"),
                "name": "jwm",
            }))),
            "get_metrics" => {
                if let Some(metrics) = backend.compositor_get_metrics() {
                    IpcResponse::ok(Some(serde_json::to_value(metrics).unwrap_or_default()))
                } else {
                    // Fallback if no compositor
                    IpcResponse::ok(Some(serde_json::json!({
                        "window_count": self.state.clients.len(),
                        "monitor_count": self.state.monitors.len(),
                        "tag_count": cfg.tags_length(),
                    })))
                }
            }
            _ => IpcResponse::err(format!("unknown query: {name}")),
        }
    }

    // -------------------------------------------------------------------------
    // Query helpers
    // -------------------------------------------------------------------------

    fn query_windows(&self) -> Vec<WindowInfo> {
        let sel_client = self.get_selected_client_key();
        self.state
            .client_order
            .iter()
            .filter_map(|&ck| {
                let c = self.state.clients.get(ck)?;
                Some(WindowInfo {
                    id: c.win.raw(),
                    name: c.name.clone(),
                    class: c.class.clone(),
                    instance: c.instance.clone(),
                    tags: c.state.tags,
                    monitor: c.monitor_num as i32,
                    x: c.geometry.x,
                    y: c.geometry.y,
                    w: c.geometry.w,
                    h: c.geometry.h,
                    is_floating: c.state.is_floating,
                    is_fullscreen: c.state.is_fullscreen,
                    is_urgent: c.state.is_urgent,
                    is_sticky: c.state.is_sticky,
                    is_focused: sel_client == Some(ck),
                })
            })
            .collect()
    }

    fn query_workspaces(&self) -> Vec<WorkspaceInfo> {
        let cfg = CONFIG.load();
        let mut result = Vec::new();
        for &mk in &self.state.monitor_order {
            let mon = match self.state.monitors.get(mk) {
                Some(m) => m,
                None => continue,
            };
            let active_tags = mon.get_active_tags();
            let client_count = self
                .state
                .monitor_clients
                .get(mk)
                .map(|v| v.len())
                .unwrap_or(0);
            for i in 0..cfg.tags_length() {
                let tag_bit = 1u32 << i;
                let is_active = (active_tags & tag_bit) != 0;
                result.push(WorkspaceInfo {
                    tag_mask: tag_bit,
                    tag_index: i,
                    monitor: mon.num,
                    layout: format!("{:?}", *mon.lt[mon.sel_lt]),
                    m_fact: mon.layout.m_fact,
                    n_master: mon.layout.n_master,
                    num_clients: if is_active { client_count } else { 0 },
                    focused: is_active && self.state.sel_mon == Some(mk),
                });
            }
        }
        result
    }

    fn query_monitors(&self) -> Vec<MonitorInfoIpc> {
        self.state
            .monitor_order
            .iter()
            .filter_map(|&mk| {
                let m = self.state.monitors.get(mk)?;
                Some(MonitorInfoIpc {
                    num: m.num,
                    x: m.geometry.m_x,
                    y: m.geometry.m_y,
                    w: m.geometry.m_w,
                    h: m.geometry.m_h,
                    active_tags: m.get_active_tags(),
                    layout: format!("{:?}", *m.lt[m.sel_lt]),
                    focused: self.state.sel_mon == Some(mk),
                })
            })
            .collect()
    }

    fn query_tree(&self) -> Vec<TreeNode> {
        self.state
            .monitor_order
            .iter()
            .filter_map(|&mk| {
                let m = self.state.monitors.get(mk)?;
                let sel_client = m.sel;
                let windows: Vec<WindowInfo> = self
                    .state
                    .monitor_clients
                    .get(mk)
                    .map(|clients| {
                        clients
                            .iter()
                            .filter_map(|&ck| {
                                let c = self.state.clients.get(ck)?;
                                Some(WindowInfo {
                                    id: c.win.raw(),
                                    name: c.name.clone(),
                                    class: c.class.clone(),
                                    instance: c.instance.clone(),
                                    tags: c.state.tags,
                                    monitor: c.monitor_num as i32,
                                    x: c.geometry.x,
                                    y: c.geometry.y,
                                    w: c.geometry.w,
                                    h: c.geometry.h,
                                    is_floating: c.state.is_floating,
                                    is_fullscreen: c.state.is_fullscreen,
                                    is_urgent: c.state.is_urgent,
                                    is_sticky: c.state.is_sticky,
                                    is_focused: sel_client == Some(ck),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Some(TreeNode {
                    monitor: MonitorInfoIpc {
                        num: m.num,
                        x: m.geometry.m_x,
                        y: m.geometry.m_y,
                        w: m.geometry.m_w,
                        h: m.geometry.m_h,
                        active_tags: m.get_active_tags(),
                        layout: format!("{:?}", *m.lt[m.sel_lt]),
                        focused: self.state.sel_mon == Some(mk),
                    },
                    windows,
                })
            })
            .collect()
    }

    // =========================================================================
    // IPC event broadcast helper
    // =========================================================================

    pub fn broadcast_ipc_event(&mut self, event_type: &str, payload: serde_json::Value) {
        if let Some(ipc) = self.ipc_server.as_mut() {
            ipc.broadcast(&IpcEvent {
                event: event_type.to_string(),
                payload,
            });
        }
    }

    // =========================================================================
    // Config hot-reload
    // =========================================================================

    fn do_config_reload(&mut self, backend: &mut dyn Backend) -> IpcResponse {
        match crate::config::reload_global() {
            Ok(()) => {
                self.apply_config_changes(backend);
                self.broadcast_ipc_event("config/reload", serde_json::json!({}));
                IpcResponse::ok(None)
            }
            Err(e) => IpcResponse::err(format!("config reload failed: {e}")),
        }
    }

    fn apply_config_changes(&mut self, backend: &mut dyn Backend) {
        let cfg = CONFIG.load();

        // 1. Rebind keys
        self.key_bindings = cfg.get_keys();
        if let Err(e) = self.grabkeys(backend) {
            warn!("[config] failed to re-grab keys: {e}");
        }

        // 2. Re-apply color schemes
        let colors = cfg.colors();
        let alloc = backend.color_allocator();
        let _ = alloc.free_all_theme_pixels();
        if let (Ok(norm_fg), Ok(norm_bg), Ok(norm_border)) = (
            ArgbColor::from_hex(&colors.dark_sea_green1, colors.opaque),
            ArgbColor::from_hex(&colors.light_sky_blue1, colors.opaque),
            ArgbColor::from_hex(&colors.light_sky_blue1, colors.opaque),
        ) {
            alloc.set_scheme(
                SchemeType::Norm,
                ColorScheme::new(norm_fg, norm_bg, norm_border),
            );
        }
        if let (Ok(sel_fg), Ok(sel_bg), Ok(sel_border)) = (
            ArgbColor::from_hex(&colors.dark_sea_green2, colors.opaque),
            ArgbColor::from_hex(&colors.pale_turquoise1, colors.opaque),
            ArgbColor::from_hex(&colors.cyan, colors.opaque),
        ) {
            alloc.set_scheme(
                SchemeType::Sel,
                ColorScheme::new(sel_fg, sel_bg, sel_border),
            );
        }
        let _ = alloc.allocate_schemes_pixels();

        // 3. Re-arrange all monitors (border/gap changes take effect)
        let mon_keys: Vec<MonitorKey> = self.state.monitor_order.clone();
        for mk in &mon_keys {
            self.arrange(backend, Some(*mk));
        }

        // 4. Update decoration on all visible clients
        let sel_ck = self.get_selected_client_key();

        // 5. Toggle compositor if config changed
        let compositor_wanted = cfg.compositor_enabled();
        let compositor_active = backend.has_compositor();
        if compositor_wanted != compositor_active {
            match backend.set_compositor_enabled(compositor_wanted) {
                Ok(true) => log::info!(
                    "Compositor {}",
                    if compositor_wanted {
                        "enabled"
                    } else {
                        "disabled"
                    }
                ),
                Ok(false) => {}
                Err(e) => log::warn!("Failed to set compositor: {e}"),
            }
        }

        // 6. Hot-reload all compositor settings
        backend.compositor_apply_config();

        let client_keys: Vec<ClientKey> = self.state.client_order.clone();
        for ck in client_keys {
            if let Some(_client) = self.state.clients.get(ck) {
                let is_sel = sel_ck == Some(ck);
                let _ = self.update_client_decoration(backend, ck, is_sel);
            }
        }
    }
}
