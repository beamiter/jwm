pub mod client;
pub mod client_stack;
pub mod constraints;
pub mod event_dispatcher;
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
pub mod session;
pub mod stacking;
pub mod statusbar;
pub mod strut_manager;
pub mod swallowing;
pub mod tag_manager;
pub mod types;
pub mod visibility;

pub mod monitor_management;
pub mod positioning;
pub mod process;
pub mod rendering;
pub mod window_state;
pub use types::{
    ICONIC_STATE, InteractionAction, InteractionState, MonitorIndex, NORMAL_STATE, STEXT_MAX_LEN,
    SecondaryBarInstance, WITHDRAWN_STATE, WMArgEnum, WMButton, WMClickType, WMFuncType, WMKey,
    WMRule, WMWindowGeom,
};

pub use features::{FeatureStates, MagnifierState, OverviewState, RecordingState, ScreenshotState};

pub use geometry::GeometryConstraints;
pub use rules::{RuleApplication, RuleMatcher};
pub use statusbar::{StatusBarBuilder, StatusBarUpdateManager};

use log::info;
use log::warn;
use log::{debug, error};

use crate::backend::common_define::WindowId;
use crate::core::state::WMState;
use slotmap::SecondaryMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::env;
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::backend::api::Backend;
use crate::backend::api::StrutPartial;
use crate::backend::api::WindowChanges;
use crate::backend::api::WindowType;
use crate::backend::common_define::ArgbColor;
use crate::backend::common_define::ColorScheme;
use crate::backend::common_define::EventMaskBits;
use crate::backend::common_define::SchemeType;
use crate::backend::common_define::{KeySym, Mods};
use crate::config::CONFIG;
use crate::core::layout::LayoutEnum;
use crate::core::models::{ClientKey, MonitorKey, ScrollingState, WMClient, WMMonitor};
use crate::ipc_server::IpcServer;

use crate::core::animation::AnimationManager;
use shared_structures::CommandType;
use shared_structures::SharedCommand;
use shared_structures::{MonitorInfo, SharedMessage, TagStatus};

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

    /// Compiled chord (leader + second-key bindings). `None` when disabled.
    pub(crate) chord_compiled: Option<crate::config::CompiledChord>,
    /// Set when the leader fired and we're waiting for the second key.
    pub(crate) chord_armed_until: Option<std::time::Instant>,

    /// Do-not-disturb: when true, suppress urgent-window propagation and
    /// hide override-redirect notification surfaces. Initialized from
    /// `behavior.do_not_disturb` and toggled live via the `toggle_dnd` IPC.
    pub(crate) do_not_disturb: bool,

    /// Debug HUD on/off, toggled by `toggle_debug_hud` (default keybinding
    /// Alt+Shift+F12). Initialized from `behavior.debug_hud`.
    pub(crate) debug_hud_on: bool,

    /// Strut reservations from external panels (polybar, trayer, etc.).
    /// The second tuple element is the monitor that physically hosts the
    /// panel window, used to attribute legacy whole-screen (`_NET_WM_STRUT`)
    /// reservations to a single output instead of every monitor.
    external_struts: HashMap<WindowId, (StrutPartial, Option<MonitorKey>)>,

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

    /// Event coalescer for reducing high-frequency updates
    pub event_coalescer: crate::backend::x11::compositor_common::event_coalescer::EventCoalescer,

    /// _NET_WM_PING: pending pings awaiting pong response
    pub pending_pings: HashMap<WindowId, std::time::Instant>,
    /// Windows that failed to respond to ping within timeout
    pub unresponsive_windows: HashSet<WindowId>,
    /// Last time we sent pings to visible windows
    pub last_ping_time: Option<std::time::Instant>,
    /// Last user interaction timestamp (for _NET_WM_USER_TIME focus-steal prevention)
    pub last_user_activity_time: u32,
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
        } else if eq!(Jwm::scrolling_toggle_attach_mode) {
            "scrolling_toggle_attach_mode"
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
        } else if eq!(Jwm::toggle_annotation) {
            "toggle_annotation"
        } else if eq!(Jwm::save_session) {
            "save_session"
        } else if eq!(Jwm::restore_session) {
            "restore_session"
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

        // Do-not-disturb: suppress notification windows entirely.
        if self.do_not_disturb {
            if let Err(e) = backend.window_ops().unmap_window(win) {
                debug!("DND: unmap notification {:?} failed: {:?}", win, e);
            }
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
            chord_compiled: CONFIG.load().compile_chord(),
            chord_armed_until: None,
            do_not_disturb: CONFIG.load().behavior().do_not_disturb,
            debug_hud_on: CONFIG.load().behavior().debug_hud,
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
            event_coalescer:
                crate::backend::x11::compositor_common::event_coalescer::EventCoalescer::new(),
            pending_pings: HashMap::new(),
            unresponsive_windows: HashSet::new(),
            last_ping_time: None,
            last_user_activity_time: 0,
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
                // Adopt viewable windows and also windows left in IconicState by a
                // previous WM (ICCCM WM_STATE == 3). Without the iconic check we
                // silently drop minimized windows across a WM restart.
                const ICONIC_STATE: i64 = 3;
                let iconic = backend
                    .property_ops()
                    .get_wm_state(win)
                    .map(|s| s == ICONIC_STATE)
                    .unwrap_or(false);
                if !attr.override_redirect && (attr.map_state_viewable || iconic) {
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
            if client.state.is_swallowed {
                return false;
            }
            client.state.is_sticky || (client.state.tags & monitor.get_active_tags()) > 0
        } else {
            false
        }
    }

    fn is_client_visible_by_key(&self, client_key: ClientKey) -> bool {
        if let Some(client) = self.state.clients.get(client_key) {
            if client.state.is_swallowed {
                return false;
            }
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
        let mut bindings: Vec<(Mods, KeySym)> = self
            .key_bindings
            .iter()
            .map(|k| (k.mask, k.key_sym))
            .collect();
        // Also grab the chord leader so the WM (not the focused client) sees it.
        // Second-key bindings inside the chord are handled via grab_keyboard
        // after the leader fires, so they don't need to be globally grabbed.
        if let Some(chord) = &self.chord_compiled {
            if !bindings.iter().any(|b| *b == chord.leader) {
                bindings.push(chord.leader);
            }
        }
        backend.key_ops().grab_keys(root_window, &bindings)?;
        Ok(())
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
        lines.push(format!("{:<28} view tag 1-{}", "Mod1+[1-9]", tags_len));
        lines.push(format!(
            "{:<28} move to tag 1-{}",
            "Mod1+Shift+[1-9]", tags_len
        ));
        lines.push(format!(
            "{:<28} toggle view tag 1-{}",
            "Mod1+Ctrl+[1-9]", tags_len
        ));
        lines.push(format!(
            "{:<28} toggle tag 1-{}",
            "Mod1+Ctrl+Shift+[1-9]", tags_len
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

        let Some(mon_key) = mon_key_opt else {
            error!("Monitor key is None, cannot update bar message.");
            return;
        };

        let Some(monitor) = self.state.monitors.get(mon_key) else {
            error!("Monitor {mon_key:?} not found");
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
        let active_tagset = monitor.get_active_tags();

        for i in 0..CONFIG.load().tags_length() {
            let tag_bit = 1 << i;

            let is_filled_tag = self.is_filled_tag(mon_key, tag_bit);

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
        const EMPTY_CLIENTS: &[ClientKey] = &[];
        let monitor_clients = self
            .state
            .monitor_clients
            .get(mon_key)
            .map_or(EMPTY_CLIENTS, Vec::as_slice);
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
