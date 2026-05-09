pub mod client;
pub mod client_stack;
pub mod constraints;
pub mod features;
pub mod focus;
pub mod focus_manager;
pub mod geometry;
pub mod input_handler;
pub mod ipc_handler;
pub mod layout;
pub mod lifecycle;
pub mod monitor;
pub mod mouse_handler;
pub mod navigation;
pub mod property_handler;
pub mod rules;
pub mod statusbar;
pub mod strut_manager;
pub mod tag_manager;
pub mod types;

pub mod process;
pub mod rendering;
pub mod window_state;
pub mod monitor_management;
pub mod positioning;
pub use types::{
    ICONIC_STATE, InteractionAction, InteractionState, MonitorIndex, NORMAL_STATE, STEXT_MAX_LEN,
    SecondaryBarInstance, WITHDRAWN_STATE, WMArgEnum, WMButton, WMClickType, WMFuncType, WMKey,
    WMRule, WMWindowGeom,
};

pub use features::{FeatureStates, MagnifierState, OverviewState, RecordingState, ScreenshotState};

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
use std::process::Command;
use std::process::Stdio;
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
                        window, w, h, x, y, c.geometry.w, c.geometry.h, c.geometry.x, c.geometry.y
                    );
                }

                let geometry_changed = self
                    .state
                    .clients
                    .get(client_key)
                    .map(|c| {
                        c.geometry.x != x
                            || c.geometry.y != y
                            || c.geometry.w != w as i32
                            || c.geometry.h != h as i32
                    })
                    .unwrap_or(true);

                if !geometry_changed {
                    return Ok(());
                }

                // Check if this is a status bar being moved back to origin by GTK
                // If so, skip the update to prevent feedback loop with arrange
                let is_status_bar_reset = self
                    .state
                    .clients
                    .get(client_key)
                    .map(|c| c.state.is_dock && x == 0 && y == 0 && c.geometry.x != 0)
                    .unwrap_or(false);

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



    /// 处理 Expose 事件（窗口需要重绘）
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
        let monitor_clients = self
            .state
            .monitor_clients
            .get(mon_key)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
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
}
