//! Native X11 backend implemented with the `xcb` crate.
//!
//! This backend owns a real `xcb::Connection` and implements the full JWM
//! backend trait surface directly on top of XCB. Window management, EWMH,
//! input, systray, and primary event dispatch are handled natively. The
//! compositor path reuses the shared X11 compositor layer so both X11
//! backends expose the same higher-level feature surface.

use crate::backend::api::{
    AllowMode, AllowedAction, Backend, BackendEvent, Capabilities, CloseResult, ColorAllocator,
    CursorProvider, EventHandler, EwmhFacade, EwmhFeature, Geometry, HitTarget, IconData, InputOps,
    KeyOps, LayerSurfaceInfo, MotifWmHints, NetWmState, NormalHints, NotifyMode, OutputInfo,
    OutputOps, PropertyKind, PropertyOps, ResizeEdge, ScreenInfo, StackMode, StrutPartial,
    VrrCapabilities, WindowAttributes, WindowChanges, WindowOps, WindowType, WmHints,
};
use crate::backend::common_define::{
    ArgbColor, ColorScheme, CursorHandle, EventMaskBits, KeySym, Mods, OutputId, Pixel, SchemeType,
    StdCursorKind, WindowId,
};
use crate::backend::error::BackendError;
use crate::backend::x11::wm::{
    AllowedActionAtoms, ClientMessageAtoms, ClientMessageKind, DEFAULT_OUTPUT_REFRESH_MHZ,
    EwmhFeatureAtoms, NetWmStateAtoms, PropertyKindAtoms, SUPPORTED_EWMH_FEATURES, WindowTypeAtoms,
    atom_for_allowed_action, atom_for_ewmh_feature, atom_for_net_wm_state, build_output_info,
    classify_client_message, decode_text_property, expand_net_wm_state_requests, fallback_output,
    lock_modifier_combinations, net_wm_ping_message, net_wm_state_from_atom,
    net_wm_sync_request_message, output_at, parse_gtk_frame_extents, parse_icon_data,
    parse_motif_hints, parse_normal_hints, parse_opaque_region, parse_strut, parse_strut_partial,
    parse_wm_class, parse_wm_hints, property_kind_from_atom, protocol_supported,
    restack_window_changes, stack_mode_from_index, stack_mode_to_index,
    window_changes_from_configure_request_parts, window_type_from_atom, wm_delete_window_message,
    wm_take_focus_message,
};
use crate::backend::xcb::batch::{BatchedGeometryRequest, XcbRequestBatcher};
use crate::backend::xcb::compositor_protocol::{
    XcbCompositorProtocol, XcbSharedCompositor, XcbSharedCompositorConnection,
    create_shared_compositor_connection,
};
use crate::backend::xcb::present::load_present_manager as load_xcb_present_manager;
use crate::jwm::InteractionAction;
use calloop::signals::{Signal, Signals};
use calloop::{
    EventLoop,
    timer::{TimeoutAction, Timer},
};
use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::env;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::sync::{
    Arc, Mutex, RwLock,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;
use xcb::{Raw, Xid, XidNew, x};

type XcbResult<T> = Result<T, BackendError>;

#[derive(Clone, Copy)]
struct XcbAtoms {
    wm_protocols: x::Atom,
    wm_delete_window: x::Atom,
    wm_take_focus: x::Atom,
    wm_state: x::Atom,
    wm_transient_for: x::Atom,
    wm_class: x::Atom,
    wm_hints: x::Atom,
    wm_normal_hints: x::Atom,
    wm_size_hints: x::Atom,
    utf8_string: x::Atom,
    string: x::Atom,
    cardinal: x::Atom,
    atom: x::Atom,
    window: x::Atom,
    net_active_window: x::Atom,
    net_supported: x::Atom,
    net_wm_name: x::Atom,
    net_wm_pid: x::Atom,
    net_wm_state: x::Atom,
    net_supporting_wm_check: x::Atom,
    net_wm_state_fullscreen: x::Atom,
    net_wm_state_maximized_vert: x::Atom,
    net_wm_state_maximized_horz: x::Atom,
    net_wm_state_hidden: x::Atom,
    net_wm_state_above: x::Atom,
    net_wm_state_below: x::Atom,
    net_wm_state_demands_attention: x::Atom,
    net_wm_state_sticky: x::Atom,
    net_wm_state_skip_taskbar: x::Atom,
    net_wm_state_skip_pager: x::Atom,
    net_wm_window_type: x::Atom,
    net_wm_window_type_desktop: x::Atom,
    net_wm_window_type_splash: x::Atom,
    net_wm_window_type_utility: x::Atom,
    net_wm_window_type_menu: x::Atom,
    net_wm_window_type_dialog: x::Atom,
    net_wm_window_type_toolbar: x::Atom,
    net_wm_window_type_dock: x::Atom,
    net_wm_window_type_popup_menu: x::Atom,
    net_wm_window_type_dropdown_menu: x::Atom,
    net_wm_window_type_tooltip: x::Atom,
    net_wm_window_type_combo: x::Atom,
    net_wm_window_type_notification: x::Atom,
    net_client_list: x::Atom,
    net_client_list_stacking: x::Atom,
    net_client_info: x::Atom,
    net_current_desktop: x::Atom,
    net_number_of_desktops: x::Atom,
    net_desktop_names: x::Atom,
    net_desktop_viewport: x::Atom,
    net_close_window: x::Atom,
    net_restack_window: x::Atom,
    net_wm_ping: x::Atom,
    net_wm_sync_request: x::Atom,
    net_wm_sync_request_counter: x::Atom,
    net_wm_user_time: x::Atom,
    #[allow(dead_code)]
    net_wm_user_time_window: x::Atom,
    net_wm_icon: x::Atom,
    net_wm_bypass_compositor: x::Atom,
    net_wm_opaque_region: x::Atom,
    net_wm_strut: x::Atom,
    net_wm_strut_partial: x::Atom,
    net_wm_moveresize: x::Atom,
    net_frame_extents: x::Atom,
    net_wm_allowed_actions: x::Atom,
    net_wm_action_move: x::Atom,
    net_wm_action_resize: x::Atom,
    net_wm_action_minimize: x::Atom,
    net_wm_action_maximize_horz: x::Atom,
    net_wm_action_maximize_vert: x::Atom,
    net_wm_action_fullscreen: x::Atom,
    net_wm_action_close: x::Atom,
    net_wm_action_stick: x::Atom,
    net_wm_action_above: x::Atom,
    net_wm_action_below: x::Atom,
    net_workarea: x::Atom,
    net_system_tray_opcode: x::Atom,
    net_system_tray_orientation: x::Atom,
    #[allow(dead_code)]
    net_system_tray_visual: x::Atom,
    manager: x::Atom,
    xembed: x::Atom,
    xembed_info: x::Atom,
    motif_wm_hints: x::Atom,
    gtk_frame_extents: x::Atom,
    #[allow(dead_code)]
    compound_text: x::Atom,
}

impl XcbAtoms {
    fn intern(conn: &xcb::Connection, name: &[u8]) -> XcbResult<x::Atom> {
        let cookie = conn.send_request(&x::InternAtom {
            only_if_exists: false,
            name,
        });
        conn.wait_for_reply(cookie)
            .map(|r| r.atom())
            .map_err(xcb_err)
    }

    fn new(conn: &xcb::Connection) -> XcbResult<Self> {
        Ok(Self {
            wm_protocols: Self::intern(conn, b"WM_PROTOCOLS")?,
            wm_delete_window: Self::intern(conn, b"WM_DELETE_WINDOW")?,
            wm_take_focus: Self::intern(conn, b"WM_TAKE_FOCUS")?,
            wm_state: Self::intern(conn, b"WM_STATE")?,
            wm_transient_for: x::ATOM_WM_TRANSIENT_FOR,
            wm_class: x::ATOM_WM_CLASS,
            wm_hints: x::ATOM_WM_HINTS,
            wm_normal_hints: x::ATOM_WM_NORMAL_HINTS,
            wm_size_hints: Self::intern(conn, b"WM_SIZE_HINTS")?,
            utf8_string: Self::intern(conn, b"UTF8_STRING")?,
            string: x::ATOM_STRING,
            cardinal: x::ATOM_CARDINAL,
            atom: x::ATOM_ATOM,
            window: x::ATOM_WINDOW,
            net_active_window: Self::intern(conn, b"_NET_ACTIVE_WINDOW")?,
            net_supported: Self::intern(conn, b"_NET_SUPPORTED")?,
            net_wm_name: Self::intern(conn, b"_NET_WM_NAME")?,
            net_wm_pid: Self::intern(conn, b"_NET_WM_PID")?,
            net_wm_state: Self::intern(conn, b"_NET_WM_STATE")?,
            net_supporting_wm_check: Self::intern(conn, b"_NET_SUPPORTING_WM_CHECK")?,
            net_wm_state_fullscreen: Self::intern(conn, b"_NET_WM_STATE_FULLSCREEN")?,
            net_wm_state_maximized_vert: Self::intern(conn, b"_NET_WM_STATE_MAXIMIZED_VERT")?,
            net_wm_state_maximized_horz: Self::intern(conn, b"_NET_WM_STATE_MAXIMIZED_HORZ")?,
            net_wm_state_hidden: Self::intern(conn, b"_NET_WM_STATE_HIDDEN")?,
            net_wm_state_above: Self::intern(conn, b"_NET_WM_STATE_ABOVE")?,
            net_wm_state_below: Self::intern(conn, b"_NET_WM_STATE_BELOW")?,
            net_wm_state_demands_attention: Self::intern(conn, b"_NET_WM_STATE_DEMANDS_ATTENTION")?,
            net_wm_state_sticky: Self::intern(conn, b"_NET_WM_STATE_STICKY")?,
            net_wm_state_skip_taskbar: Self::intern(conn, b"_NET_WM_STATE_SKIP_TASKBAR")?,
            net_wm_state_skip_pager: Self::intern(conn, b"_NET_WM_STATE_SKIP_PAGER")?,
            net_wm_window_type: Self::intern(conn, b"_NET_WM_WINDOW_TYPE")?,
            net_wm_window_type_desktop: Self::intern(conn, b"_NET_WM_WINDOW_TYPE_DESKTOP")?,
            net_wm_window_type_splash: Self::intern(conn, b"_NET_WM_WINDOW_TYPE_SPLASH")?,
            net_wm_window_type_utility: Self::intern(conn, b"_NET_WM_WINDOW_TYPE_UTILITY")?,
            net_wm_window_type_menu: Self::intern(conn, b"_NET_WM_WINDOW_TYPE_MENU")?,
            net_wm_window_type_dialog: Self::intern(conn, b"_NET_WM_WINDOW_TYPE_DIALOG")?,
            net_wm_window_type_toolbar: Self::intern(conn, b"_NET_WM_WINDOW_TYPE_TOOLBAR")?,
            net_wm_window_type_dock: Self::intern(conn, b"_NET_WM_WINDOW_TYPE_DOCK")?,
            net_wm_window_type_popup_menu: Self::intern(conn, b"_NET_WM_WINDOW_TYPE_POPUP_MENU")?,
            net_wm_window_type_dropdown_menu: Self::intern(
                conn,
                b"_NET_WM_WINDOW_TYPE_DROPDOWN_MENU",
            )?,
            net_wm_window_type_tooltip: Self::intern(conn, b"_NET_WM_WINDOW_TYPE_TOOLTIP")?,
            net_wm_window_type_combo: Self::intern(conn, b"_NET_WM_WINDOW_TYPE_COMBO")?,
            net_wm_window_type_notification: Self::intern(
                conn,
                b"_NET_WM_WINDOW_TYPE_NOTIFICATION",
            )?,
            net_client_list: Self::intern(conn, b"_NET_CLIENT_LIST")?,
            net_client_list_stacking: Self::intern(conn, b"_NET_CLIENT_LIST_STACKING")?,
            net_client_info: Self::intern(conn, b"_NET_CLIENT_INFO")?,
            net_current_desktop: Self::intern(conn, b"_NET_CURRENT_DESKTOP")?,
            net_number_of_desktops: Self::intern(conn, b"_NET_NUMBER_OF_DESKTOPS")?,
            net_desktop_names: Self::intern(conn, b"_NET_DESKTOP_NAMES")?,
            net_desktop_viewport: Self::intern(conn, b"_NET_DESKTOP_VIEWPORT")?,
            net_close_window: Self::intern(conn, b"_NET_CLOSE_WINDOW")?,
            net_restack_window: Self::intern(conn, b"_NET_RESTACK_WINDOW")?,
            net_wm_ping: Self::intern(conn, b"_NET_WM_PING")?,
            net_wm_sync_request: Self::intern(conn, b"_NET_WM_SYNC_REQUEST")?,
            net_wm_sync_request_counter: Self::intern(conn, b"_NET_WM_SYNC_REQUEST_COUNTER")?,
            net_wm_user_time: Self::intern(conn, b"_NET_WM_USER_TIME")?,
            net_wm_user_time_window: Self::intern(conn, b"_NET_WM_USER_TIME_WINDOW")?,
            net_wm_icon: Self::intern(conn, b"_NET_WM_ICON")?,
            net_wm_bypass_compositor: Self::intern(conn, b"_NET_WM_BYPASS_COMPOSITOR")?,
            net_wm_opaque_region: Self::intern(conn, b"_NET_WM_OPAQUE_REGION")?,
            net_wm_strut: Self::intern(conn, b"_NET_WM_STRUT")?,
            net_wm_strut_partial: Self::intern(conn, b"_NET_WM_STRUT_PARTIAL")?,
            net_wm_moveresize: Self::intern(conn, b"_NET_WM_MOVERESIZE")?,
            net_frame_extents: Self::intern(conn, b"_NET_FRAME_EXTENTS")?,
            net_wm_allowed_actions: Self::intern(conn, b"_NET_WM_ALLOWED_ACTIONS")?,
            net_wm_action_move: Self::intern(conn, b"_NET_WM_ACTION_MOVE")?,
            net_wm_action_resize: Self::intern(conn, b"_NET_WM_ACTION_RESIZE")?,
            net_wm_action_minimize: Self::intern(conn, b"_NET_WM_ACTION_MINIMIZE")?,
            net_wm_action_maximize_horz: Self::intern(conn, b"_NET_WM_ACTION_MAXIMIZE_HORZ")?,
            net_wm_action_maximize_vert: Self::intern(conn, b"_NET_WM_ACTION_MAXIMIZE_VERT")?,
            net_wm_action_fullscreen: Self::intern(conn, b"_NET_WM_ACTION_FULLSCREEN")?,
            net_wm_action_close: Self::intern(conn, b"_NET_WM_ACTION_CLOSE")?,
            net_wm_action_stick: Self::intern(conn, b"_NET_WM_ACTION_STICK")?,
            net_wm_action_above: Self::intern(conn, b"_NET_WM_ACTION_ABOVE")?,
            net_wm_action_below: Self::intern(conn, b"_NET_WM_ACTION_BELOW")?,
            net_workarea: Self::intern(conn, b"_NET_WORKAREA")?,
            net_system_tray_opcode: Self::intern(conn, b"_NET_SYSTEM_TRAY_OPCODE")?,
            net_system_tray_orientation: Self::intern(conn, b"_NET_SYSTEM_TRAY_ORIENTATION")?,
            net_system_tray_visual: Self::intern(conn, b"_NET_SYSTEM_TRAY_VISUAL")?,
            manager: Self::intern(conn, b"MANAGER")?,
            xembed: Self::intern(conn, b"_XEMBED")?,
            xembed_info: Self::intern(conn, b"_XEMBED_INFO")?,
            motif_wm_hints: Self::intern(conn, b"_MOTIF_WM_HINTS")?,
            gtk_frame_extents: Self::intern(conn, b"_GTK_FRAME_EXTENTS")?,
            compound_text: Self::intern(conn, b"COMPOUND_TEXT")?,
        })
    }

    fn atom_for_state(&self, state: NetWmState) -> x::Atom {
        atom_for_net_wm_state(state, self.state_atoms())
    }

    fn state_from_atom(&self, atom: x::Atom) -> Option<NetWmState> {
        net_wm_state_from_atom(atom, self.state_atoms())
    }

    fn state_atoms(&self) -> NetWmStateAtoms<x::Atom> {
        NetWmStateAtoms {
            fullscreen: self.net_wm_state_fullscreen,
            maximized_vert: self.net_wm_state_maximized_vert,
            maximized_horz: self.net_wm_state_maximized_horz,
            hidden: self.net_wm_state_hidden,
            above: self.net_wm_state_above,
            below: self.net_wm_state_below,
            demands_attention: self.net_wm_state_demands_attention,
            sticky: self.net_wm_state_sticky,
            skip_taskbar: self.net_wm_state_skip_taskbar,
            skip_pager: self.net_wm_state_skip_pager,
        }
    }

    fn window_type_atoms(&self) -> WindowTypeAtoms<x::Atom> {
        WindowTypeAtoms {
            desktop: self.net_wm_window_type_desktop,
            dock: self.net_wm_window_type_dock,
            toolbar: self.net_wm_window_type_toolbar,
            menu: self.net_wm_window_type_menu,
            utility: self.net_wm_window_type_utility,
            splash: self.net_wm_window_type_splash,
            dialog: self.net_wm_window_type_dialog,
            dropdown_menu: self.net_wm_window_type_dropdown_menu,
            popup_menu: self.net_wm_window_type_popup_menu,
            tooltip: self.net_wm_window_type_tooltip,
            notification: self.net_wm_window_type_notification,
            combo: self.net_wm_window_type_combo,
        }
    }

    fn allowed_action_atoms(&self) -> AllowedActionAtoms<x::Atom> {
        AllowedActionAtoms {
            move_: self.net_wm_action_move,
            resize: self.net_wm_action_resize,
            minimize: self.net_wm_action_minimize,
            maximize_horz: self.net_wm_action_maximize_horz,
            maximize_vert: self.net_wm_action_maximize_vert,
            fullscreen: self.net_wm_action_fullscreen,
            close: self.net_wm_action_close,
            stick: self.net_wm_action_stick,
            above: self.net_wm_action_above,
            below: self.net_wm_action_below,
        }
    }

    fn feature_atoms(&self) -> EwmhFeatureAtoms<x::Atom> {
        EwmhFeatureAtoms {
            active_window: self.net_active_window,
            supported: self.net_supported,
            wm_name: self.net_wm_name,
            wm_state: self.net_wm_state,
            supporting_wm_check: self.net_supporting_wm_check,
            wm_state_fullscreen: self.net_wm_state_fullscreen,
            wm_state_maximized_vert: self.net_wm_state_maximized_vert,
            wm_state_maximized_horz: self.net_wm_state_maximized_horz,
            wm_state_hidden: self.net_wm_state_hidden,
            wm_state_above: self.net_wm_state_above,
            wm_state_below: self.net_wm_state_below,
            wm_state_demands_attention: self.net_wm_state_demands_attention,
            wm_state_sticky: self.net_wm_state_sticky,
            wm_state_skip_taskbar: self.net_wm_state_skip_taskbar,
            wm_state_skip_pager: self.net_wm_state_skip_pager,
            client_list: self.net_client_list,
            client_info: self.net_client_info,
            wm_window_type: self.net_wm_window_type,
            wm_window_type_dialog: self.net_wm_window_type_dialog,
            current_desktop: self.net_current_desktop,
            number_of_desktops: self.net_number_of_desktops,
            desktop_names: self.net_desktop_names,
            desktop_viewport: self.net_desktop_viewport,
            wm_moveresize: self.net_wm_moveresize,
            frame_extents: self.net_frame_extents,
            wm_allowed_actions: self.net_wm_allowed_actions,
            workarea: self.net_workarea,
            close_window: self.net_close_window,
            restack_window: self.net_restack_window,
            wm_ping: self.net_wm_ping,
            wm_user_time: self.net_wm_user_time,
            wm_icon: self.net_wm_icon,
            wm_bypass_compositor: self.net_wm_bypass_compositor,
            wm_opaque_region: self.net_wm_opaque_region,
        }
    }

    fn property_kind_atoms(&self) -> PropertyKindAtoms<x::Atom> {
        PropertyKindAtoms {
            wm_transient_for: self.wm_transient_for,
            wm_normal_hints: self.wm_normal_hints,
            wm_hints: self.wm_hints,
            wm_name: x::ATOM_WM_NAME,
            net_wm_name: self.net_wm_name,
            wm_class: self.wm_class,
            net_wm_window_type: self.net_wm_window_type,
            wm_protocols: self.wm_protocols,
            net_wm_strut: self.net_wm_strut,
            net_wm_strut_partial: self.net_wm_strut_partial,
            motif_wm_hints: self.motif_wm_hints,
            gtk_frame_extents: self.gtk_frame_extents,
            net_wm_bypass_compositor: self.net_wm_bypass_compositor,
            net_wm_opaque_region: self.net_wm_opaque_region,
            net_wm_icon: self.net_wm_icon,
            net_wm_user_time: self.net_wm_user_time,
        }
    }

    fn client_message_atoms(&self) -> ClientMessageAtoms<u32> {
        ClientMessageAtoms {
            net_wm_state: self.net_wm_state.resource_id(),
            net_active_window: self.net_active_window.resource_id(),
            net_close_window: self.net_close_window.resource_id(),
            net_wm_moveresize: self.net_wm_moveresize.resource_id(),
            wm_protocols: self.wm_protocols.resource_id(),
            net_wm_ping: self.net_wm_ping.resource_id(),
        }
    }
}

#[derive(Clone, Default)]
struct XcbIdRegistry {
    next: Arc<AtomicU64>,
    x_to_wid: Arc<RwLock<HashMap<u32, WindowId>>>,
    wid_to_x: Arc<RwLock<HashMap<WindowId, u32>>>,
}

impl XcbIdRegistry {
    fn new(start: u64) -> Self {
        Self {
            next: Arc::new(AtomicU64::new(start)),
            x_to_wid: Arc::new(RwLock::new(HashMap::new())),
            wid_to_x: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn intern(&self, win: x::Window) -> WindowId {
        let raw = win.resource_id();
        if let Some(id) = self.x_to_wid.read().unwrap().get(&raw).copied() {
            return id;
        }
        let mut x_to_wid = self.x_to_wid.write().unwrap();
        if let Some(id) = x_to_wid.get(&raw).copied() {
            return id;
        }
        let id = WindowId::from_raw(self.next.fetch_add(1, Ordering::Relaxed));
        x_to_wid.insert(raw, id);
        self.wid_to_x.write().unwrap().insert(id, raw);
        id
    }

    fn window(&self, id: WindowId) -> XcbResult<x::Window> {
        self.wid_to_x
            .read()
            .unwrap()
            .get(&id)
            .copied()
            .map(x::Window::new)
            .ok_or(BackendError::NotFound("WindowId not mapped to XCB window"))
    }

    fn x11(&self, id: WindowId) -> XcbResult<u32> {
        self.wid_to_x
            .read()
            .unwrap()
            .get(&id)
            .copied()
            .ok_or(BackendError::NotFound("WindowId not mapped to X11 window"))
    }

    fn intern_raw(&self, raw: u32) -> WindowId {
        self.intern(x::Window::new(raw))
    }

    fn all_x11_windows(&self) -> Vec<(u32, WindowId)> {
        self.x_to_wid
            .read()
            .unwrap()
            .iter()
            .map(|(&raw, &id)| (raw, id))
            .collect()
    }

    fn remove(&self, win: x::Window) {
        let raw = win.resource_id();
        if let Some(id) = self.x_to_wid.write().unwrap().remove(&raw) {
            self.wid_to_x.write().unwrap().remove(&id);
        }
    }
}

pub struct XcbBackend {
    conn: Arc<xcb::Connection>,
    root: x::Window,
    root_id: WindowId,
    ids: XcbIdRegistry,
    atoms: XcbAtoms,
    caps: Capabilities,
    window_ops: Box<dyn WindowOps>,
    input_ops: Box<dyn InputOps>,
    property_ops: Box<dyn PropertyOps>,
    output_ops: Box<dyn OutputOps>,
    key_ops: Box<dyn KeyOps>,
    ewmh: Option<Box<dyn EwmhFacade>>,
    cursor_provider: Box<dyn CursorProvider>,
    color_allocator: Box<dyn ColorAllocator>,
    pending: VecDeque<BackendEvent>,
    interaction: Option<XcbInteraction>,
    shared_compositor_conn: Option<Arc<XcbSharedCompositorConnection>>,
    compositor: Option<XcbSharedCompositor>,
    systray: Option<XcbSystemTray>,
    benchmark_auto_exit: bool,
    scratch_x11_scene: Vec<(u32, i32, i32, u32, u32)>,
}

struct XcbInteraction {
    win: WindowId,
    start_geom: Geometry,
    start_root_x: f64,
    start_root_y: f64,
    action: InteractionAction,
    current_x: i32,
    current_y: i32,
    current_w: u32,
    current_h: u32,
}

#[derive(Debug, Clone)]
struct XcbTrayIcon {
    window: x::Window,
    mapped: bool,
}

struct XcbSystemTray {
    conn: Arc<xcb::Connection>,
    atoms: XcbAtoms,
    tray_window: x::Window,
    selection_atom: x::Atom,
    root: x::Window,
    icon_size: u32,
    icons: Vec<XcbTrayIcon>,
    active: bool,
}

impl XcbSystemTray {
    fn new(
        conn: Arc<xcb::Connection>,
        atoms: XcbAtoms,
        root: x::Window,
        screen_num: usize,
    ) -> XcbResult<Self> {
        let selection_name = format!("_NET_SYSTEM_TRAY_S{screen_num}");
        let selection_atom = XcbAtoms::intern(&conn, selection_name.as_bytes())?;
        let tray_window: x::Window = conn.generate_id();
        conn.send_and_check_request(&x::CreateWindow {
            depth: x::COPY_FROM_PARENT as u8,
            wid: tray_window,
            parent: root,
            x: -1,
            y: -1,
            width: 1,
            height: 1,
            border_width: 0,
            class: x::WindowClass::InputOutput,
            visual: x::COPY_FROM_PARENT,
            value_list: &[x::Cw::EventMask(x::EventMask::PROPERTY_CHANGE)],
        })
        .map_err(xcb_err)?;

        Ok(Self {
            conn,
            atoms,
            tray_window,
            selection_atom,
            root,
            icon_size: 24,
            icons: Vec::new(),
            active: false,
        })
    }

    fn acquire_selection(&mut self) -> XcbResult<bool> {
        let owner = self
            .conn
            .wait_for_reply(self.conn.send_request(&x::GetSelectionOwner {
                selection: self.selection_atom,
            }))
            .map_err(xcb_err)?
            .owner();
        if owner != x::WINDOW_NONE {
            return Ok(false);
        }

        self.conn
            .send_and_check_request(&x::SetSelectionOwner {
                owner: self.tray_window,
                selection: self.selection_atom,
                time: x::CURRENT_TIME,
            })
            .map_err(xcb_err)?;

        let owner = self
            .conn
            .wait_for_reply(self.conn.send_request(&x::GetSelectionOwner {
                selection: self.selection_atom,
            }))
            .map_err(xcb_err)?
            .owner();
        if owner != self.tray_window {
            return Ok(false);
        }

        let manager_event = x::ClientMessageEvent::new(
            self.root,
            self.atoms.manager,
            x::ClientMessageData::Data32([
                x::CURRENT_TIME,
                self.selection_atom.resource_id(),
                self.tray_window.resource_id(),
                0,
                0,
            ]),
        );
        self.conn
            .send_and_check_request(&x::SendEvent {
                propagate: false,
                destination: x::SendEventDest::Window(self.root),
                event_mask: x::EventMask::STRUCTURE_NOTIFY,
                event: &manager_event,
            })
            .map_err(xcb_err)?;

        change_u32s(
            &self.conn,
            self.tray_window,
            self.atoms.net_system_tray_orientation,
            self.atoms.cardinal,
            &[0],
        )?;
        self.conn.flush().map_err(xcb_err)?;
        self.active = true;
        Ok(true)
    }

    fn handle_client_message(&mut self, data: &[u32; 5]) -> bool {
        if !self.active {
            return false;
        }
        if data[1] == 0 && data[2] != 0 {
            let _ = self.dock_icon(x::Window::new(data[2]));
            return true;
        }
        false
    }

    fn dock_icon(&mut self, icon_window: x::Window) -> XcbResult<()> {
        if self.is_tray_icon(icon_window) {
            return Ok(());
        }

        self.conn
            .send_and_check_request(&x::ChangeWindowAttributes {
                window: icon_window,
                value_list: &[x::Cw::EventMask(
                    x::EventMask::STRUCTURE_NOTIFY | x::EventMask::PROPERTY_CHANGE,
                )],
            })
            .map_err(xcb_err)?;
        self.conn
            .send_and_check_request(&x::ReparentWindow {
                window: icon_window,
                parent: self.tray_window,
                x: 0,
                y: 0,
            })
            .map_err(xcb_err)?;
        self.configure_icon(icon_window, 0)?;
        self.conn
            .send_and_check_request(&x::MapWindow {
                window: icon_window,
            })
            .map_err(xcb_err)?;

        let xembed_event = x::ClientMessageEvent::new(
            icon_window,
            self.atoms.xembed,
            x::ClientMessageData::Data32([
                x::CURRENT_TIME,
                0,
                0,
                self.tray_window.resource_id(),
                0,
            ]),
        );
        self.conn
            .send_and_check_request(&x::SendEvent {
                propagate: false,
                destination: x::SendEventDest::Window(icon_window),
                event_mask: x::EventMask::NO_EVENT,
                event: &xembed_event,
            })
            .map_err(xcb_err)?;

        self.icons.push(XcbTrayIcon {
            window: icon_window,
            mapped: true,
        });
        self.layout_icons()?;
        self.conn.flush().map_err(xcb_err)
    }

    fn handle_destroy(&mut self, window: x::Window) {
        if let Some(pos) = self.icons.iter().position(|i| i.window == window) {
            self.icons.remove(pos);
            let _ = self.layout_icons();
            let _ = self.conn.flush();
        }
    }

    fn handle_unmap(&mut self, window: x::Window) {
        if let Some(icon) = self.icons.iter_mut().find(|i| i.window == window) {
            icon.mapped = false;
            let _ = self.layout_icons();
            let _ = self.conn.flush();
        }
    }

    fn handle_map(&mut self, window: x::Window) {
        if let Some(icon) = self.icons.iter_mut().find(|i| i.window == window) {
            icon.mapped = true;
            let _ = self.layout_icons();
            let _ = self.conn.flush();
        }
    }

    fn handle_xembed_info_change(&mut self, window: x::Window) {
        let mapped = self.read_xembed_mapped(window);
        if let Some(icon) = self.icons.iter_mut().find(|i| i.window == window) {
            if mapped && !icon.mapped {
                icon.mapped = true;
                let _ = self.conn.send_and_check_request(&x::MapWindow { window });
            } else if !mapped && icon.mapped {
                icon.mapped = false;
                let _ = self.conn.send_and_check_request(&x::UnmapWindow { window });
            }
            let _ = self.layout_icons();
            let _ = self.conn.flush();
        }
    }

    fn is_tray_icon(&self, window: x::Window) -> bool {
        self.icons.iter().any(|i| i.window == window)
    }

    fn cleanup(&self) {
        if !self.active {
            return;
        }
        for icon in &self.icons {
            let _ = self.conn.send_and_check_request(&x::ReparentWindow {
                window: icon.window,
                parent: self.root,
                x: 0,
                y: 0,
            });
            let _ = self.conn.send_and_check_request(&x::UnmapWindow {
                window: icon.window,
            });
        }
        let _ = self.conn.send_and_check_request(&x::DestroyWindow {
            window: self.tray_window,
        });
        let _ = self.conn.flush();
    }

    fn layout_icons(&self) -> XcbResult<()> {
        let mut x_offset: u32 = 0;
        for icon in &self.icons {
            if !icon.mapped {
                continue;
            }
            self.configure_icon(icon.window, x_offset)?;
            x_offset += self.icon_size;
        }
        self.conn
            .send_and_check_request(&x::ConfigureWindow {
                window: self.tray_window,
                value_list: &[
                    x::ConfigWindow::Width(x_offset.max(1)),
                    x::ConfigWindow::Height(self.icon_size),
                ],
            })
            .map_err(xcb_err)
    }

    fn configure_icon(&self, window: x::Window, x_offset: u32) -> XcbResult<()> {
        self.conn
            .send_and_check_request(&x::ConfigureWindow {
                window,
                value_list: &[
                    x::ConfigWindow::X(x_offset as i32),
                    x::ConfigWindow::Y(0),
                    x::ConfigWindow::Width(self.icon_size),
                    x::ConfigWindow::Height(self.icon_size),
                ],
            })
            .map_err(xcb_err)
    }

    fn read_xembed_mapped(&self, window: x::Window) -> bool {
        let cookie = self.conn.send_request(&x::GetProperty {
            delete: false,
            window,
            property: self.atoms.xembed_info,
            r#type: x::ATOM_ANY,
            long_offset: 0,
            long_length: 2,
        });
        let Ok(reply) = self.conn.wait_for_reply(cookie) else {
            return true;
        };
        if reply.format() != 32 {
            return true;
        }
        let data = reply.value::<u32>();
        data.len() < 2 || data[1] & 1 != 0
    }
}

struct XcbLoopData<'a> {
    backend: &'a mut XcbBackend,
    handler: &'a mut dyn EventHandler,
    should_exit: bool,
}

impl XcbLoopData<'_> {
    fn dispatch_backend_event(
        &mut self,
        event: BackendEvent,
        pending_motion: &mut Option<BackendEvent>,
        context: &str,
    ) {
        match &event {
            BackendEvent::MotionNotify { target, .. } => {
                if let Some(BackendEvent::MotionNotify {
                    target: prev_target,
                    ..
                }) = pending_motion
                {
                    if prev_target != target {
                        self.flush_pending_motion(pending_motion, context);
                    }
                }
                *pending_motion = Some(event);
            }
            _ => {
                self.flush_pending_motion(pending_motion, context);
                self.deliver_backend_event(event, context);
            }
        }
    }

    fn flush_pending_motion(&mut self, pending_motion: &mut Option<BackendEvent>, context: &str) {
        if let Some(event) = pending_motion.take() {
            self.deliver_backend_event(event, context);
        }
    }

    fn deliver_backend_event(&mut self, event: BackendEvent, context: &str) {
        self.backend.compositor_handle_event(&event);
        if self.backend.systray_handle_event(&event) {
            return;
        }
        let event = self.backend.enrich_event_with_output(event);
        if let Err(err) = self.handler.handle_event(self.backend, event) {
            log::error!("Error handling {context}: {err:?}");
        }
    }
}

impl XcbBackend {
    fn debug_drag_enabled() -> bool {
        static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *CACHE.get_or_init(|| {
            env::var("JWM_DEBUG_DRAG")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(true)
        })
    }

    pub fn new() -> XcbResult<Self> {
        let (conn, screen_num) = xcb::Connection::connect_with_extensions(
            None,
            &[],
            &[
                xcb::Extension::Composite,
                xcb::Extension::RandR,
                xcb::Extension::Shape,
                xcb::Extension::Damage,
                xcb::Extension::Present,
                xcb::Extension::XFixes,
            ],
        )
        .map_err(xcb_err)?;
        let conn = Arc::new(conn);
        let (root, screen_width, screen_height, default_colormap, min_keycode, max_keycode) = {
            let setup = conn.get_setup();
            let screen = setup
                .roots()
                .nth(screen_num as usize)
                .ok_or(BackendError::NotFound("XCB screen"))?;
            (
                screen.root(),
                screen.width_in_pixels() as i32,
                screen.height_in_pixels() as i32,
                screen.default_colormap(),
                setup.min_keycode(),
                setup.max_keycode(),
            )
        };

        let ids = XcbIdRegistry::new(1);
        let root_id = ids.intern(root);
        let atoms = XcbAtoms::new(&conn)?;
        let caps = Capabilities {
            can_warp_pointer: true,
            supports_client_list: true,
        };

        let window_ops = Box::new(XcbWindowOps::new(conn.clone(), ids.clone(), atoms, root));
        let input_ops = Box::new(XcbInputOps::new(conn.clone(), ids.clone(), root));
        let property_ops = Box::new(XcbPropertyOps::new(conn.clone(), ids.clone(), atoms));
        let output_ops = Box::new(XcbOutputOps::new(
            conn.clone(),
            root,
            screen_width,
            screen_height,
        ));
        let key_ops = Box::new(XcbKeyOps::new(
            conn.clone(),
            ids.clone(),
            min_keycode,
            max_keycode,
        ));
        let ewmh = Some(
            Box::new(XcbEwmh::new(conn.clone(), ids.clone(), atoms, root)) as Box<dyn EwmhFacade>,
        );
        let cursor_provider = Box::new(XcbCursorProvider::new(conn.clone(), ids.clone())?);
        let color_allocator = Box::new(XcbColorAllocator::new(conn.clone(), default_colormap));

        let _ = conn.check_request(conn.send_request_checked(&xcb::randr::SelectInput {
            window: root,
            enable: xcb::randr::NotifyMask::SCREEN_CHANGE
                | xcb::randr::NotifyMask::OUTPUT_CHANGE
                | xcb::randr::NotifyMask::CRTC_CHANGE,
        }));

        let compositor_enabled = env::var("JWM_COMPOSITOR")
            .map(|v| v == "1")
            .unwrap_or_else(|_| crate::config::CONFIG.load().compositor_enabled());

        let primary_refresh_hz = output_ops
            .enumerate_outputs()
            .into_iter()
            .find_map(|o| (o.refresh_rate > 0).then_some(o.refresh_rate))
            .unwrap_or(60);
        log::info!(
            "xcb backend: primary monitor refresh rate: {}Hz",
            primary_refresh_hz
        );

        let shared_compositor_conn = if compositor_enabled {
            match create_shared_compositor_connection(conn.clone()) {
                Ok(comp_conn) => Some(comp_conn),
                Err(e) => {
                    log::warn!("XCB backend: failed to create shared compositor wrapper: {e}");
                    None
                }
            }
        } else {
            None
        };

        let compositor = if compositor_enabled {
            let _ = Self::prime_compositor_event_extensions(&conn);
            match shared_compositor_conn.clone() {
                Some(comp_conn) => match XcbSharedCompositor::new(
                    comp_conn,
                    root.resource_id(),
                    screen_width as u32,
                    screen_height as u32,
                    primary_refresh_hz,
                ) {
                    Ok(c) => {
                        log::info!("XCB backend: GPU compositor initialized successfully");
                        let mut c = c;
                        c.set_present_manager(load_xcb_present_manager(conn.clone()));
                        Some(c)
                    }
                    Err(e) => {
                        log::warn!(
                            "XCB backend: compositor init failed, falling back to non-composited mode: {e}"
                        );
                        None
                    }
                },
                None => None,
            }
        } else {
            log::info!("XCB backend: compositor disabled (set JWM_COMPOSITOR=1 to enable)");
            None
        };

        let mut backend = Self {
            conn,
            root,
            root_id,
            ids,
            atoms,
            caps,
            window_ops,
            input_ops,
            property_ops,
            output_ops,
            key_ops,
            ewmh,
            cursor_provider,
            color_allocator,
            pending: VecDeque::new(),
            interaction: None,
            shared_compositor_conn,
            compositor,
            systray: None,
            benchmark_auto_exit: false,
            scratch_x11_scene: Vec::new(),
        };
        backend.init_systray(screen_num as usize);
        backend.compositor_auto_configure_hdr();
        Ok(backend)
    }

    fn compositor_overlay_raw(&self) -> Option<u32> {
        self.compositor.as_ref().map(|c| c.overlay_window())
    }

    fn is_compositor_overlay(&self, window: x::Window) -> bool {
        self.compositor_overlay_raw()
            .is_some_and(|overlay| overlay == window.resource_id())
    }

    fn get_primary_monitor_refresh_rate(&self) -> u32 {
        self.output_ops
            .enumerate_outputs()
            .into_iter()
            .find_map(|o| (o.refresh_rate > 0).then_some(o.refresh_rate))
            .unwrap_or(60)
    }

    fn prime_compositor_event_extensions(conn: &xcb::Connection) -> XcbResult<()> {
        XcbCompositorProtocol::new(conn).prime_extensions()
    }

    fn shared_compositor_connection(&mut self) -> XcbResult<Arc<XcbSharedCompositorConnection>> {
        if let Some(conn) = &self.shared_compositor_conn {
            return Ok(conn.clone());
        }

        let conn = create_shared_compositor_connection(self.conn.clone())?;
        self.shared_compositor_conn = Some(conn.clone());
        Ok(conn)
    }

    fn init_systray(&mut self, screen_num: usize) {
        match XcbSystemTray::new(self.conn.clone(), self.atoms, self.root, screen_num) {
            Ok(mut tray) => match tray.acquire_selection() {
                Ok(true) => {
                    log::info!("[systray] Acquired system tray selection");
                    self.systray = Some(tray);
                }
                Ok(false) => {
                    log::info!("[systray] Another tray owner exists, skipping");
                }
                Err(e) => {
                    log::warn!("[systray] Failed to acquire selection: {e}");
                }
            },
            Err(e) => {
                log::warn!("[systray] Failed to create system tray: {e}");
            }
        }
    }

    fn systray_handle_event(&mut self, ev: &BackendEvent) -> bool {
        let systray = match self.systray.as_mut() {
            Some(s) => s,
            None => return false,
        };
        match ev {
            BackendEvent::ClientMessage { type_, data, .. } => {
                if *type_ == self.atoms.net_system_tray_opcode.resource_id() {
                    return systray.handle_client_message(data);
                }
                false
            }
            BackendEvent::WindowDestroyed(win) => {
                let x11w = x::Window::new(self.ids.x11(*win).unwrap_or(0));
                if systray.is_tray_icon(x11w) {
                    systray.handle_destroy(x11w);
                    return true;
                }
                false
            }
            BackendEvent::WindowUnmapped(win) => {
                let x11w = x::Window::new(self.ids.x11(*win).unwrap_or(0));
                if systray.is_tray_icon(x11w) {
                    systray.handle_unmap(x11w);
                    return true;
                }
                false
            }
            BackendEvent::WindowMapped(win) => {
                let x11w = x::Window::new(self.ids.x11(*win).unwrap_or(0));
                if systray.is_tray_icon(x11w) {
                    systray.handle_map(x11w);
                    return true;
                }
                false
            }
            BackendEvent::PropertyChanged { window, kind } => {
                if matches!(kind, PropertyKind::Other) {
                    let x11w = x::Window::new(self.ids.x11(*window).unwrap_or(0));
                    if systray.is_tray_icon(x11w) {
                        systray.handle_xembed_info_change(x11w);
                        return true;
                    }
                }
                false
            }
            _ => false,
        }
    }

    fn compositor_auto_configure_hdr(&mut self) {
        let cfg = crate::config::CONFIG.load();
        let behavior = cfg.behavior();
        if !behavior.hdr_enabled {
            return;
        }

        if let Some(output_id) = self.query_primary_randr_output() {
            if let Some(caps) = self.query_output_edid_hdr(xcb::randr::Output::new(output_id)) {
                log::info!(
                    "HDR EDID: max={:.0} nits, min={:.2} nits, PQ={}, HLG={}, BT.2020={}",
                    caps.max_luminance_nits,
                    caps.min_luminance_nits,
                    caps.supports_pq,
                    caps.supports_hlg,
                    caps.supports_bt2020
                );

                if let Some(c) = self.compositor.as_mut() {
                    if caps.max_luminance_nits > 0.0 {
                        c.set_hdr_peak_nits(caps.max_luminance_nits);
                    }
                    if caps.supports_pq {
                        c.set_eotf_mode(1);
                    } else if caps.supports_hlg {
                        c.set_eotf_mode(2);
                    }
                    if caps.supports_bt2020 {
                        c.set_output_colorspace(1);
                    }
                    c.set_hdr_output_10bit(true);
                }

                let _ = self.set_output_hdr_properties(output_id, true);
            } else {
                log::info!("HDR enabled but display EDID has no HDR metadata; using SDR EOTF");
            }
        }
    }

    fn query_primary_randr_output(&self) -> Option<u32> {
        let cookie = self
            .conn
            .send_request(&xcb::randr::GetScreenResources { window: self.root });
        let resources = self.conn.wait_for_reply(cookie).ok()?;

        for output in resources.outputs() {
            let cookie = self.conn.send_request(&xcb::randr::GetOutputInfo {
                output: *output,
                config_timestamp: 0,
            });
            let info = self.conn.wait_for_reply(cookie).ok()?;
            if info.crtc() != xcb::randr::Crtc::none()
                && info.connection() == xcb::randr::Connection::Connected
            {
                return Some(output.resource_id());
            }
        }

        None
    }

    fn query_output_edid_hdr(
        &self,
        output: xcb::randr::Output,
    ) -> Option<crate::backend::edid::EdidHdrCapabilities> {
        let atom = XcbAtoms::intern(&self.conn, b"EDID").ok()?;
        let cookie = self.conn.send_request(&xcb::randr::GetOutputProperty {
            output,
            property: atom,
            r#type: x::ATOM_ANY,
            long_offset: 0,
            long_length: 256,
            delete: false,
            pending: false,
        });
        let reply = self.conn.wait_for_reply(cookie).ok()?;
        let data = reply.data::<u8>();
        if data.len() < 128 {
            return None;
        }
        crate::backend::edid::parse_edid_hdr_from_bytes(data)
    }

    fn register_existing_windows_with_compositor(&mut self, compositor: &mut XcbSharedCompositor) {
        let overlay = compositor.overlay_window();
        let windows: Vec<_> = self
            .ids
            .all_x11_windows()
            .into_iter()
            .filter(|(x11w, _)| *x11w != self.root.resource_id() && *x11w != overlay)
            .collect();

        if windows.is_empty() {
            return;
        }

        let mut batch = BatchedGeometryRequest::new(&self.conn);
        for &(x11w, _) in &windows {
            batch.queue_geometry(x11w);
        }

        match batch.flush_and_collect() {
            Ok(geometries) => {
                let mut registered = 0usize;
                for (x11w, wid) in &windows {
                    if let Some((x, y, w, h)) = geometries.get(x11w) {
                        compositor.add_window(*x11w, *x as i32, *y as i32, *w as u32, *h as u32);
                        self.apply_compositor_window_metadata(compositor, *x11w, *wid);
                        registered += 1;
                    }
                }
                log::info!(
                    "XCB backend: compositor registered {registered} existing windows via batched geometry"
                );
                return;
            }
            Err(e) => {
                log::warn!(
                    "XCB backend: batched geometry request failed: {e:?}; falling back to individual queries"
                );
            }
        }

        let mut registered = 0usize;
        for (x11w, wid) in windows {
            if let Ok(geom) = self.window_ops.get_geometry(wid) {
                compositor.add_window(x11w, geom.x, geom.y, geom.w, geom.h);
                self.apply_compositor_window_metadata(compositor, x11w, wid);
                registered += 1;
            }
        }

        log::info!("XCB backend: compositor registered {registered} existing windows");
    }

    fn apply_compositor_window_metadata(
        &self,
        compositor: &mut XcbSharedCompositor,
        x11w: u32,
        wid: WindowId,
    ) {
        let (_, cls) = self.property_ops.get_class(wid);
        if !cls.is_empty() {
            compositor.set_window_class(x11w, &cls);
        }
        if let Ok(attr) = self.window_ops.get_window_attributes(wid) {
            if attr.override_redirect {
                compositor.set_window_override_redirect(x11w, true);
            }
        }
    }

    fn compositor_handle_event(&mut self, event: &BackendEvent) {
        let compositor = match self.compositor.as_mut() {
            Some(c) => c,
            None => return,
        };
        let overlay = compositor.overlay_window();
        match event {
            BackendEvent::WindowMapped(win) => {
                if let Ok(x11w) = self.ids.x11(*win) {
                    if x11w != self.root.resource_id() && x11w != overlay {
                        if let Ok(geom) = self.window_ops.get_geometry(*win) {
                            compositor.add_window(x11w, geom.x, geom.y, geom.w, geom.h);
                        }
                        let (_, cls) = self.property_ops.get_class(*win);
                        if !cls.is_empty() {
                            compositor.set_window_class(x11w, &cls);
                        }
                        if let Ok(attr) = self.window_ops.get_window_attributes(*win) {
                            if attr.override_redirect {
                                compositor.set_window_override_redirect(x11w, true);
                            }
                        }
                    }
                }
            }
            BackendEvent::WindowUnmapped(win) | BackendEvent::WindowDestroyed(win) => {
                if let Ok(x11w) = self.ids.x11(*win) {
                    compositor.remove_window(x11w);
                }
            }
            BackendEvent::WindowConfigured {
                window,
                x,
                y,
                width,
                height,
            } => {
                if let Ok(x11w) = self.ids.x11(*window) {
                    if x11w != overlay {
                        compositor.update_geometry(x11w, *x, *y, *width, *height);
                    }
                }
            }
            BackendEvent::WindowStateRequest {
                window,
                state,
                action,
            } => {
                if *state == NetWmState::Fullscreen {
                    if let Ok(x11w) = self.ids.x11(*window) {
                        let is_fs = matches!(
                            action,
                            crate::backend::api::NetWmAction::Add
                                | crate::backend::api::NetWmAction::Toggle
                        );
                        compositor.set_window_fullscreen(x11w, is_fs);
                    }
                }
            }
            BackendEvent::PropertyChanged { window, kind } => {
                if matches!(kind, PropertyKind::Class) {
                    if let Ok(x11w) = self.ids.x11(*window) {
                        let (_, cls) = self.property_ops.get_class(*window);
                        if !cls.is_empty() {
                            compositor.set_window_class(x11w, &cls);
                        }
                    }
                }
            }
            BackendEvent::DamageNotify { drawable } => {
                if let Ok(x11w) = self.ids.x11(*drawable) {
                    if x11w != overlay {
                        compositor.mark_damaged(x11w);
                    }
                }
            }
            BackendEvent::PresentComplete {
                window,
                serial,
                msc,
                ust,
            } => {
                if let Ok(x11w) = self.ids.x11(*window) {
                    if let Some(oml) = compositor.oml_mut() {
                        oml.on_window_presented(x11w, *msc, *ust);
                    }
                    compositor.on_present_complete(x11w, *serial, *msc, *ust);
                }
            }
            BackendEvent::PresentIdle {
                window,
                serial,
                pixmap,
            } => {
                if let Ok(x11w) = self.ids.x11(*window) {
                    compositor.on_present_idle(x11w, *serial, *pixmap);
                }
            }
            BackendEvent::MotionNotify { root_x, root_y, .. } => {
                compositor.set_mouse_position(*root_x as f32, *root_y as f32);
                compositor.record_input_event();
            }
            BackendEvent::ButtonPress { .. } | BackendEvent::ButtonRelease { .. } => {
                compositor.record_input_event();
            }
            BackendEvent::ScreenLayoutChanged => {
                let cookie = self.conn.send_request(&x::GetGeometry {
                    drawable: x::Drawable::Window(self.root),
                });
                if let Ok(geo) = self.conn.wait_for_reply(cookie) {
                    compositor.resize(geo.width() as u32, geo.height() as u32);
                }
                compositor.refresh_monitor_layout(self.root.resource_id());
            }
            _ => {}
        }
    }

    fn map_event(&mut self, event: xcb::Event) -> Option<BackendEvent> {
        match event {
            xcb::Event::X(x::Event::MapRequest(ev)) => {
                if self.is_compositor_overlay(ev.window()) {
                    return None;
                }
                Some(BackendEvent::WindowCreated(self.ids.intern(ev.window())))
            }
            xcb::Event::X(x::Event::MapNotify(ev)) => {
                if self.is_compositor_overlay(ev.window()) {
                    return None;
                }
                Some(BackendEvent::WindowMapped(self.ids.intern(ev.window())))
            }
            xcb::Event::X(x::Event::UnmapNotify(ev)) => {
                if self.is_compositor_overlay(ev.window()) {
                    return None;
                }
                Some(BackendEvent::WindowUnmapped(self.ids.intern(ev.window())))
            }
            xcb::Event::X(x::Event::DestroyNotify(ev)) => {
                if self.is_compositor_overlay(ev.window()) {
                    return None;
                }
                let id = self.ids.intern(ev.window());
                self.ids.remove(ev.window());
                Some(BackendEvent::WindowDestroyed(id))
            }
            xcb::Event::X(x::Event::ConfigureRequest(ev)) => {
                let mask = ev.value_mask();
                let changes = window_changes_from_configure_request_parts(
                    mask.contains(x::ConfigWindowMask::X)
                        .then_some(ev.x() as i32),
                    mask.contains(x::ConfigWindowMask::Y)
                        .then_some(ev.y() as i32),
                    mask.contains(x::ConfigWindowMask::WIDTH)
                        .then_some(ev.width() as u32),
                    mask.contains(x::ConfigWindowMask::HEIGHT)
                        .then_some(ev.height() as u32),
                    mask.contains(x::ConfigWindowMask::BORDER_WIDTH)
                        .then_some(ev.border_width() as u32),
                    mask.contains(x::ConfigWindowMask::SIBLING)
                        .then_some(self.ids.intern(ev.sibling())),
                    mask.contains(x::ConfigWindowMask::STACK_MODE)
                        .then_some(stack_mode_from_xcb(ev.stack_mode())),
                );
                Some(BackendEvent::ConfigureRequest {
                    window: self.ids.intern(ev.window()),
                    mask_bits: ev.value_mask().bits() as u16,
                    changes,
                })
            }
            xcb::Event::X(x::Event::ConfigureNotify(ev)) => {
                if self.is_compositor_overlay(ev.window()) {
                    return None;
                }
                Some(BackendEvent::WindowConfigured {
                    window: self.ids.intern(ev.window()),
                    x: ev.x() as i32,
                    y: ev.y() as i32,
                    width: ev.width() as u32,
                    height: ev.height() as u32,
                })
            }
            xcb::Event::X(x::Event::ButtonPress(ev)) => Some(BackendEvent::ButtonPress {
                target: self.hit_target(ev.event()),
                state: ev.state().bits() as u16,
                detail: ev.detail(),
                root_x: ev.root_x() as f64,
                root_y: ev.root_y() as f64,
                time: ev.time(),
            }),
            xcb::Event::X(x::Event::ButtonRelease(ev)) => Some(BackendEvent::ButtonRelease {
                target: self.hit_target(ev.event()),
                time: ev.time(),
            }),
            xcb::Event::X(x::Event::MotionNotify(ev)) => Some(BackendEvent::MotionNotify {
                target: self.hit_target(ev.event()),
                root_x: ev.root_x() as f64,
                root_y: ev.root_y() as f64,
                time: ev.time(),
            }),
            xcb::Event::X(x::Event::EnterNotify(ev)) => Some(BackendEvent::EnterNotify {
                window: self.ids.intern(ev.event()),
                subwindow: if ev.child().is_none() {
                    None
                } else {
                    Some(self.ids.intern(ev.child()))
                },
                mode: notify_mode_from_xcb(ev.mode()),
                root_x: ev.root_x() as f64,
                root_y: ev.root_y() as f64,
            }),
            xcb::Event::X(x::Event::LeaveNotify(ev)) => Some(BackendEvent::LeaveNotify {
                window: self.ids.intern(ev.event()),
                mode: notify_mode_from_xcb(ev.mode()),
            }),
            xcb::Event::X(x::Event::FocusIn(ev)) => Some(BackendEvent::FocusIn {
                window: self.ids.intern(ev.event()),
            }),
            xcb::Event::X(x::Event::FocusOut(ev)) => Some(BackendEvent::FocusOut {
                window: self.ids.intern(ev.event()),
            }),
            xcb::Event::X(x::Event::Expose(ev)) => Some(BackendEvent::Expose {
                window: self.ids.intern(ev.window()),
            }),
            xcb::Event::X(x::Event::PropertyNotify(ev)) => {
                if ev.state() == x::Property::Delete {
                    return None;
                }
                Some(BackendEvent::PropertyChanged {
                    window: self.ids.intern(ev.window()),
                    kind: self.property_kind(ev.atom()),
                })
            }
            xcb::Event::X(x::Event::KeyPress(ev)) => Some(BackendEvent::KeyPress {
                keycode: ev.detail(),
                state: ev.state().bits() as u16,
                time: ev.time(),
            }),
            xcb::Event::X(x::Event::KeyRelease(ev)) => Some(BackendEvent::KeyRelease {
                keycode: ev.detail(),
                state: ev.state().bits() as u16,
                time: ev.time(),
            }),
            xcb::Event::RandR(xcb::randr::Event::ScreenChangeNotify(_))
            | xcb::Event::RandR(xcb::randr::Event::Notify(_)) => {
                Some(BackendEvent::ScreenLayoutChanged)
            }
            xcb::Event::Damage(xcb::damage::Event::Notify(ev)) => {
                Some(BackendEvent::DamageNotify {
                    // xcb's generated DAMAGE bindings keep `drawable` private because
                    // it is encoded as a Drawable union on the wire; read the raw
                    // 32-bit XID directly from the fixed event layout.
                    drawable: self
                        .ids
                        .intern_raw(unsafe { *(ev.as_raw() as *const u8).add(4).cast::<u32>() }),
                })
            }
            xcb::Event::Present(xcb::present::Event::CompleteNotify(ev)) => {
                Some(BackendEvent::PresentComplete {
                    window: self.ids.intern_raw(ev.window().resource_id()),
                    serial: ev.serial(),
                    msc: ev.msc(),
                    ust: ev.ust(),
                })
            }
            xcb::Event::Present(xcb::present::Event::IdleNotify(ev)) => {
                Some(BackendEvent::PresentIdle {
                    window: self.ids.intern_raw(ev.window().resource_id()),
                    serial: ev.serial(),
                    pixmap: ev.pixmap().resource_id(),
                })
            }
            xcb::Event::Shape(xcb::shape::Event::Notify(ev)) => Some(BackendEvent::ShapeChanged {
                window: self.ids.intern(ev.affected_window()),
                shaped: ev.shaped(),
            }),
            xcb::Event::X(x::Event::MappingNotify(_)) => Some(BackendEvent::MappingNotify),
            xcb::Event::X(x::Event::ClientMessage(ev)) => self.map_client_message(ev),
            _ => None,
        }
    }

    fn hit_target(&self, window: x::Window) -> HitTarget {
        if window == self.root || self.is_compositor_overlay(window) {
            HitTarget::Background { output: None }
        } else {
            HitTarget::Surface(self.ids.intern(window))
        }
    }

    fn property_kind(&self, atom: x::Atom) -> PropertyKind {
        property_kind_from_atom(atom, self.atoms.property_kind_atoms())
    }

    fn map_client_message(&mut self, ev: x::ClientMessageEvent) -> Option<BackendEvent> {
        let window = self.ids.intern(ev.window());
        let data = match ev.data() {
            x::ClientMessageData::Data32(d) => d,
            _ => {
                return Some(BackendEvent::ClientMessage {
                    window,
                    type_: ev.r#type().resource_id(),
                    data: [0; 5],
                    format: ev.format(),
                });
            }
        };
        match classify_client_message(
            ev.r#type().resource_id(),
            ev.format(),
            data,
            self.atoms.client_message_atoms(),
        ) {
            ClientMessageKind::WindowState {
                action,
                first,
                second,
            } => {
                let mut events =
                    expand_net_wm_state_requests(window, action, first, second, |atom| {
                        self.atoms.state_from_atom(x::Atom::new(atom))
                    });
                if events.is_empty() {
                    Some(BackendEvent::ClientMessage {
                        window,
                        type_: ev.r#type().resource_id(),
                        data,
                        format: ev.format(),
                    })
                } else {
                    for event in events.drain(1..) {
                        self.pending.push_back(event);
                    }
                    events.into_iter().next()
                }
            }
            ClientMessageKind::ActiveWindow => Some(BackendEvent::ActiveWindowMessage { window }),
            ClientMessageKind::CloseWindow => Some(BackendEvent::CloseWindowRequest { window }),
            ClientMessageKind::MoveResize { direction, button } => {
                Some(BackendEvent::MoveResizeRequest {
                    window,
                    direction,
                    button,
                })
            }
            ClientMessageKind::PingResponse { window } => Some(BackendEvent::PingResponse {
                window: self.ids.intern(x::Window::new(window)),
            }),
            ClientMessageKind::Other => Some(BackendEvent::ClientMessage {
                window,
                type_: ev.r#type().resource_id(),
                data,
                format: ev.format(),
            }),
        }
    }

    fn enrich_event_with_output(&self, mut event: BackendEvent) -> BackendEvent {
        let fill_output = |x: f64, y: f64| self.output_ops.output_at(x as i32, y as i32);

        match &mut event {
            BackendEvent::ButtonPress {
                target,
                root_x,
                root_y,
                ..
            }
            | BackendEvent::MotionNotify {
                target,
                root_x,
                root_y,
                ..
            } => {
                if matches!(target, HitTarget::Background { .. }) {
                    *target = HitTarget::Background {
                        output: fill_output(*root_x, *root_y),
                    };
                }
            }
            BackendEvent::ScreenLayoutChanged => {
                self.output_ops.invalidate_output_cache();
            }
            _ => {}
        }

        event
    }

    fn query_output_vrr_capable(&self, output: u32) -> bool {
        let atom = match XcbAtoms::intern(&self.conn, b"vrr_capable") {
            Ok(atom) => atom,
            Err(_) => return false,
        };
        let cookie = self.conn.send_request(&xcb::randr::GetOutputProperty {
            output: xcb::randr::Output::new(output),
            property: atom,
            r#type: x::ATOM_ANY,
            long_offset: 0,
            long_length: 1,
            delete: false,
            pending: false,
        });
        let Ok(reply) = self.conn.wait_for_reply(cookie) else {
            return false;
        };
        match reply.format() {
            8 => reply.data::<u8>().first().copied().unwrap_or(0) != 0,
            32 => reply.data::<u32>().first().copied().unwrap_or(0) != 0,
            _ => false,
        }
    }

    fn set_output_hdr_properties(&self, output: u32, enable: bool) {
        let output = xcb::randr::Output::new(output);
        let Ok(max_bpc_atom) = XcbAtoms::intern(&self.conn, b"max_bpc") else {
            log::warn!("HDR: failed to intern max_bpc atom");
            let _ = self.conn.flush();
            return;
        };
        let max_bpc_value = if enable { 10u32 } else { 8u32 };
        if let Err(err) = self
            .conn
            .send_and_check_request(&xcb::randr::ChangeOutputProperty {
                output,
                property: max_bpc_atom,
                r#type: x::ATOM_INTEGER,
                mode: x::PropMode::Replace,
                data: &[max_bpc_value],
            })
        {
            log::warn!(
                "HDR: failed to set max_bpc={} on output 0x{:x}: {:?}",
                max_bpc_value,
                output.resource_id(),
                err
            );
        } else if enable {
            log::info!("HDR: set max_bpc=10 on output 0x{:x}", output.resource_id());
        } else {
            log::info!(
                "HDR: restored max_bpc=8 on output 0x{:x}",
                output.resource_id()
            );
        }

        if enable {
            match (
                XcbAtoms::intern(&self.conn, b"Colorspace"),
                XcbAtoms::intern(&self.conn, b"BT2020_RGB"),
            ) {
                (Ok(colorspace_atom), Ok(bt2020_atom)) => {
                    if let Err(err) =
                        self.conn
                            .send_and_check_request(&xcb::randr::ChangeOutputProperty {
                                output,
                                property: colorspace_atom,
                                r#type: x::ATOM_ATOM,
                                mode: x::PropMode::Replace,
                                data: &[bt2020_atom.resource_id()],
                            })
                    {
                        log::warn!(
                            "HDR: failed to set Colorspace=BT2020_RGB on output 0x{:x}: {:?}",
                            output.resource_id(),
                            err
                        );
                    } else {
                        log::info!(
                            "HDR: set Colorspace=BT2020_RGB on output 0x{:x}",
                            output.resource_id()
                        );
                    }
                }
                _ => {
                    log::warn!("HDR: failed to intern Colorspace/BT2020_RGB atoms");
                }
            }
        }

        let _ = self.conn.flush();
    }
}

impl Backend for XcbBackend {
    fn capabilities(&self) -> Capabilities {
        self.caps
    }

    fn root_window(&self) -> Option<WindowId> {
        Some(self.root_id)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn check_existing_wm(&self) -> XcbResult<()> {
        self.window_ops
            .change_event_mask(self.root_id, EventMaskBits::SUBSTRUCTURE_REDIRECT.bits())
            .map_err(|e| {
                BackendError::Message(format!("Another window manager is already running: {e}"))
            })
    }

    fn request_render(&mut self) {
        let _ = self.conn.flush();
    }

    fn has_compositor(&self) -> bool {
        self.compositor.is_some()
    }

    fn set_compositor_enabled(&mut self, enabled: bool) -> XcbResult<bool> {
        let currently_enabled = self.compositor.is_some();
        if enabled == currently_enabled {
            return Ok(false);
        }

        if !enabled {
            log::info!("XCB backend: compositor disabled at runtime");
            self.compositor.take();
            return Ok(true);
        }

        let screen = self.output_ops.screen_info();
        Self::prime_compositor_event_extensions(&self.conn)?;
        let comp_conn = self.shared_compositor_connection()?;
        let mut compositor = XcbSharedCompositor::new(
            comp_conn,
            self.root.resource_id(),
            screen.width as u32,
            screen.height as u32,
            self.get_primary_monitor_refresh_rate(),
        )
        .map_err(|e| BackendError::Message(format!("compositor init failed: {e}")))?;
        compositor.set_present_manager(load_xcb_present_manager(self.conn.clone()));
        self.register_existing_windows_with_compositor(&mut compositor);
        self.compositor = Some(compositor);
        self.compositor_auto_configure_hdr();
        Ok(true)
    }

    fn has_partial_damage(&self) -> bool {
        self.compositor
            .as_ref()
            .is_some_and(|c| c.has_partial_damage())
    }

    fn set_partial_damage(&mut self, enabled: bool) -> XcbResult<bool> {
        Ok(self
            .compositor
            .as_mut()
            .is_some_and(|c| c.set_partial_damage(enabled)))
    }

    fn compositor_needs_render(&self) -> bool {
        self.compositor.as_ref().is_some_and(|c| c.needs_render())
    }

    fn compositor_overlay_window(&self) -> Option<WindowId> {
        self.compositor
            .as_ref()
            .map(|c| self.ids.intern_raw(c.overlay_window()))
    }

    fn compositor_render_frame(
        &mut self,
        scene: &[(u64, i32, i32, u32, u32)],
        focused_window: Option<u64>,
    ) -> XcbResult<bool> {
        if self.compositor.is_none() {
            return Ok(false);
        }

        let mut x11_scene = std::mem::take(&mut self.scratch_x11_scene);
        x11_scene.clear();
        x11_scene.extend(scene.iter().filter_map(|&(wid_raw, x, y, w, h)| {
            let wid = WindowId::from_raw(wid_raw);
            self.ids.x11(wid).ok().map(|x11w| (x11w, x, y, w, h))
        }));
        let focused_x11 = focused_window.and_then(|raw| self.ids.x11(WindowId::from_raw(raw)).ok());
        let root = self.root.resource_id();
        let compositor = self.compositor.as_mut().unwrap();

        if !scene.is_empty() && x11_scene.is_empty() {
            log::warn!(
                "[xcb compositor] scene has {} entries but x11_scene is empty (ID lookup failed)",
                scene.len()
            );
        }

        for &(x11w, x, y, w, h) in &x11_scene {
            if !compositor.has_window(x11w) && x11w != root {
                log::info!(
                    "[xcb compositor] lazily adding untracked window 0x{:x} {}x{} at ({},{})",
                    x11w,
                    w,
                    h,
                    x,
                    y
                );
                compositor.add_window(x11w, x, y, w, h);
            }
        }

        let _ = self.conn.flush();
        let rendered = compositor.render_frame(&x11_scene, focused_x11);
        compositor.clear_needs_render();
        self.scratch_x11_scene = x11_scene;
        Ok(rendered)
    }

    fn take_screenshot_to_file(&mut self, path: &std::path::Path) -> XcbResult<bool> {
        if let Some(compositor) = self.compositor.as_mut() {
            compositor.request_screenshot(path.to_path_buf());
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn take_screenshot_region_to_file(
        &mut self,
        path: &std::path::Path,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    ) -> XcbResult<bool> {
        if let Some(compositor) = self.compositor.as_mut() {
            compositor.request_screenshot_region(path.to_path_buf(), x, y, w, h);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn compositor_set_color_temperature(&mut self, temp: f32) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_color_temperature(temp);
        }
    }

    fn compositor_set_saturation(&mut self, sat: f32) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_saturation(sat);
        }
    }

    fn compositor_set_brightness(&mut self, val: f32) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_brightness(val);
        }
    }

    fn compositor_set_contrast(&mut self, val: f32) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_contrast(val);
        }
    }

    fn compositor_set_invert_colors(&mut self, invert: bool) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_invert_colors(invert);
        }
    }

    fn compositor_set_grayscale(&mut self, gs: bool) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_grayscale(gs);
        }
    }

    fn compositor_set_debug_hud(&mut self, enabled: bool) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_debug_hud(enabled);
        }
    }

    fn compositor_set_debug_hud_extended(&mut self, enabled: bool) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_debug_hud_extended(enabled);
        }
    }

    fn compositor_set_transition_mode(&mut self, mode: &str) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_transition_mode(mode);
        }
    }

    fn compositor_apply_config(&mut self) {
        if let Some(c) = self.compositor.as_mut() {
            c.apply_config();
        }
    }

    fn compositor_fps(&self) -> f32 {
        self.compositor
            .as_ref()
            .map_or(0.0, |c| c.frame_stats_fps())
    }

    fn compositor_get_metrics(&self) -> Option<crate::backend::api::CompositorMetrics> {
        self.compositor.as_ref().map(|c| c.get_metrics())
    }

    fn compositor_benchmark_start(&mut self, frames: u32, warmup: u32) -> bool {
        if let Some(c) = self.compositor.as_mut() {
            c.benchmark_start(frames, warmup);
            true
        } else {
            false
        }
    }

    fn compositor_benchmark_stop(&mut self) -> Option<String> {
        self.compositor.as_mut().and_then(|c| c.benchmark_stop())
    }

    fn compositor_benchmark_report(&self) -> Option<String> {
        self.compositor.as_ref().and_then(|c| c.benchmark_report())
    }

    fn compositor_benchmark_is_complete(&self) -> bool {
        self.compositor
            .as_ref()
            .is_some_and(|c| c.benchmark_is_complete())
    }

    fn compositor_benchmark_set_auto_exit(&mut self, enabled: bool) {
        self.benchmark_auto_exit = enabled;
    }

    fn compositor_capture_thumbnail(
        &self,
        window: WindowId,
        max_size: u32,
    ) -> Option<(Vec<u8>, u32, u32)> {
        let x11w = self.ids.x11(window).ok()?;
        self.compositor
            .as_ref()?
            .capture_window_thumbnail(x11w, max_size)
    }

    fn compositor_set_frame_extents(
        &mut self,
        window: WindowId,
        left: u32,
        right: u32,
        top: u32,
        bottom: u32,
    ) {
        if let Some(c) = self.compositor.as_mut() {
            if let Ok(x11w) = self.ids.x11(window) {
                c.set_frame_extents(x11w, left, right, top, bottom);
            }
        }
    }

    fn compositor_set_window_shaped(&mut self, window: WindowId, shaped: bool) {
        if let Some(c) = self.compositor.as_mut() {
            if let Ok(x11w) = self.ids.x11(window) {
                c.set_window_shaped(x11w, shaped);
            }
        }
    }

    fn compositor_notify_tag_switch(
        &mut self,
        duration: Duration,
        direction: i32,
        exclude_top: u32,
        mon_rect: (i32, i32, u32, u32),
    ) {
        if let Some(c) = self.compositor.as_mut() {
            c.notify_tag_switch(duration, direction, exclude_top, mon_rect);
        }
    }

    fn compositor_force_full_redraw(&mut self) {
        if let Some(c) = self.compositor.as_mut() {
            c.force_full_redraw();
        }
    }

    fn compositor_set_mouse_position(&mut self, x: f32, y: f32) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_mouse_position(x, y);
        }
    }

    fn compositor_deactivate_edge_glow(&mut self) {
        if let Some(c) = self.compositor.as_mut() {
            c.deactivate_edge_glow();
        }
    }

    fn compositor_unsuppress_edge_glow(&mut self) {
        if let Some(c) = self.compositor.as_mut() {
            c.unsuppress_edge_glow();
        }
    }

    fn compositor_set_window_urgent(&mut self, window: WindowId, urgent: bool) {
        if let Some(c) = self.compositor.as_mut() {
            if let Ok(x11w) = self.ids.x11(window) {
                c.set_window_urgent(x11w, urgent);
            }
        }
    }

    fn compositor_set_window_pip(&mut self, window: WindowId, pip: bool) {
        if let Some(c) = self.compositor.as_mut() {
            if let Ok(x11w) = self.ids.x11(window) {
                c.set_window_pip(x11w, pip);
            }
        }
    }

    fn compositor_set_magnifier(&mut self, enabled: bool) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_magnifier(enabled);
        }
    }

    fn compositor_notify_audio_timing(&mut self, window: WindowId, fps: f32, latency_ms: u32) {
        if let Some(c) = self.compositor.as_mut() {
            if let Ok(x11w) = self.ids.x11(window) {
                c.notify_audio_timing(x11w, fps, latency_ms);
            }
        }
    }

    fn compositor_set_overview_mode(
        &mut self,
        active: bool,
        windows: &[(WindowId, f32, f32, f32, f32, bool, String)],
    ) {
        if let Some(c) = self.compositor.as_mut() {
            let x11_windows = windows
                .iter()
                .filter_map(|(wid, x, y, w, h, sel, title)| {
                    self.ids
                        .x11(*wid)
                        .ok()
                        .map(|x11w| (x11w, *x, *y, *w, *h, *sel, title.clone()))
                })
                .collect();
            c.set_overview_mode(active, x11_windows);
        }
    }

    fn compositor_set_overview_monitor(&mut self, x: i32, y: i32, w: u32, h: u32) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_overview_monitor(x, y, w, h);
        }
    }

    fn compositor_set_monitors(&mut self, monitors: &[(u32, i32, i32, u32, u32, u32)]) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_monitors(monitors);
        }
    }

    fn compositor_set_overview_selection(&mut self, window: WindowId) {
        if let Some(c) = self.compositor.as_mut() {
            if let Ok(x11w) = self.ids.x11(window) {
                c.set_overview_selection(x11w);
            }
        }
    }

    fn compositor_notify_window_move_start(&mut self, window: WindowId) {
        if let Some(c) = self.compositor.as_mut() {
            if let Ok(x11w) = self.ids.x11(window) {
                c.notify_window_move_start(x11w);
            }
        }
    }

    fn compositor_notify_window_move_delta(&mut self, window: WindowId, dx: f32, dy: f32) {
        if let Some(c) = self.compositor.as_mut() {
            if let Ok(x11w) = self.ids.x11(window) {
                c.notify_window_move_delta(x11w, dx, dy);
            }
        }
    }

    fn compositor_notify_window_move_end(&mut self, window: WindowId) {
        if let Some(c) = self.compositor.as_mut() {
            if let Ok(x11w) = self.ids.x11(window) {
                c.notify_window_move_end(x11w);
            }
        }
    }

    fn compositor_set_dock_position(&mut self, x: f32, y: f32) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_dock_position(x, y);
        }
    }

    fn compositor_set_expose_mode(
        &mut self,
        active: bool,
        windows: Vec<(WindowId, i32, i32, u32, u32)>,
    ) {
        if let Some(c) = self.compositor.as_mut() {
            let x11_windows = windows
                .iter()
                .filter_map(|(wid, x, y, w, h)| {
                    self.ids.x11(*wid).ok().map(|x11w| (x11w, *x, *y, *w, *h))
                })
                .collect();
            c.set_expose_mode(active, x11_windows);
        }
    }

    fn compositor_set_snap_preview(&mut self, preview: Option<(f32, f32, f32, f32)>) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_snap_preview(preview);
        }
    }

    fn compositor_clear_snap_preview_immediate(&mut self) {
        if let Some(c) = self.compositor.as_mut() {
            c.clear_snap_preview_immediate();
        }
    }

    fn compositor_set_peek_mode(&mut self, active: bool) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_peek_mode(active);
        }
    }

    fn compositor_set_window_groups(&mut self, groups: Vec<(u32, Vec<(u32, String, bool)>)>) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_window_groups(groups);
        }
    }

    fn compositor_request_live_thumbnail(
        &mut self,
        window: u32,
        max_size: u32,
    ) -> Option<(Vec<u8>, u32, u32)> {
        self.compositor
            .as_ref()?
            .request_live_thumbnail(window, max_size)
    }

    fn compositor_zoom_to_fit(&mut self, window: Option<u32>) {
        if let Some(c) = self.compositor.as_mut() {
            c.zoom_to_fit(window);
        }
    }

    fn compositor_expose_click(&mut self, x: f32, y: f32) -> Option<WindowId> {
        let x11_win = self.compositor.as_mut()?.expose_click(x, y)?;
        Some(self.ids.intern_raw(x11_win))
    }

    fn compositor_set_colorblind_mode(&mut self, mode: &str) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_colorblind_mode(mode);
        }
    }

    fn compositor_set_annotation_mode(&mut self, active: bool) {
        if let Some(c) = self.compositor.as_mut() {
            c.set_annotation_mode(active);
        }
    }

    fn compositor_annotation_add_point(&mut self, x: f32, y: f32) {
        if let Some(c) = self.compositor.as_mut() {
            c.annotation_add_point(x, y);
        }
    }

    fn compositor_annotation_begin_stroke(&mut self) {
        if let Some(c) = self.compositor.as_mut() {
            c.annotation_new_stroke();
        }
    }

    fn compositor_start_recording(&mut self, path: &str) {
        if let Some(c) = self.compositor.as_mut() {
            c.start_recording(path);
        }
    }

    fn compositor_stop_recording(&mut self) {
        if let Some(c) = self.compositor.as_mut() {
            c.stop_recording();
        }
    }

    fn window_ops(&self) -> &dyn WindowOps {
        &*self.window_ops
    }
    fn input_ops(&self) -> &dyn InputOps {
        &*self.input_ops
    }
    fn property_ops(&self) -> &dyn PropertyOps {
        &*self.property_ops
    }
    fn output_ops(&self) -> &dyn OutputOps {
        &*self.output_ops
    }
    fn key_ops(&self) -> &dyn KeyOps {
        &*self.key_ops
    }
    fn key_ops_mut(&mut self) -> &mut dyn KeyOps {
        &mut *self.key_ops
    }
    fn cursor_provider(&mut self) -> &mut dyn CursorProvider {
        &mut *self.cursor_provider
    }
    fn color_allocator(&mut self) -> &mut dyn ColorAllocator {
        &mut *self.color_allocator
    }

    fn register_wm(&self, name: &str) -> XcbResult<()> {
        if let Some(ewmh) = self.ewmh.as_ref() {
            let _ = ewmh.setup_supporting_wm_check(name)?;
            ewmh.declare_supported(&supported_features())?;
        }
        Ok(())
    }

    fn cleanup(&mut self) -> XcbResult<()> {
        self.compositor.take();
        if let Some(ref tray) = self.systray {
            tray.cleanup();
        }
        self.systray.take();
        self.cursor_provider.cleanup()?;
        self.color_allocator.free_all_theme_pixels()?;
        if let Some(ewmh) = self.ewmh.as_ref() {
            let _ = ewmh.reset_root_properties();
        }
        self.conn.flush().map_err(xcb_err)
    }

    fn on_focused_client_changed(&mut self, win: Option<WindowId>) -> XcbResult<()> {
        if let Some(w) = win {
            let wants_input = self
                .property_ops
                .get_wm_hints(w)
                .and_then(|h| h.input)
                .unwrap_or(true);
            if wants_input {
                self.window_ops.set_input_focus(w)?;
            }
            let _ = self.window_ops.send_take_focus(w);
        } else {
            self.window_ops.set_input_focus_root()?;
        }

        if let Some(ewmh) = self.ewmh.as_ref() {
            match win {
                Some(w) => ewmh.set_active_window(w)?,
                None => ewmh.clear_active_window()?,
            }
        }
        if let Some(compositor) = self.compositor.as_mut() {
            compositor.force_full_redraw();
        }
        Ok(())
    }

    fn on_client_list_changed(
        &mut self,
        clients: &[WindowId],
        stack: &[WindowId],
    ) -> XcbResult<()> {
        if let Some(ewmh) = self.ewmh.as_ref() {
            ewmh.set_client_list(clients)?;
            ewmh.set_client_list_stacking(stack)?;
        }
        Ok(())
    }

    fn on_desktop_changed(&mut self, current: u32, total: u32, names: &[&str]) -> XcbResult<()> {
        if let Some(ewmh) = self.ewmh.as_ref() {
            ewmh.set_desktop_info(current, total, names)?;
        }
        Ok(())
    }

    fn set_workarea(&mut self, areas: &[(i32, i32, u32, u32)]) -> XcbResult<()> {
        if let Some(ewmh) = self.ewmh.as_ref() {
            ewmh.set_workarea(areas)?;
        }
        Ok(())
    }

    fn query_vrr_capabilities(&self, output: OutputId) -> Option<VrrCapabilities> {
        let cfg = crate::config::CONFIG.load();
        let behavior = cfg.behavior();
        if !behavior.vrr_enabled {
            return None;
        }

        let output_exists = self
            .output_ops
            .enumerate_outputs()
            .into_iter()
            .any(|o| o.id == output);
        if !output_exists {
            return None;
        }

        Some(VrrCapabilities {
            supported: self.query_output_vrr_capable(output.0 as u32),
            current_enabled: false,
            min_refresh_hz: behavior.vrr_min_fps,
            max_refresh_hz: behavior.vrr_max_fps,
        })
    }

    fn set_vrr_enabled(&mut self, _output: OutputId, _enabled: bool) -> XcbResult<()> {
        Err(BackendError::Unsupported(
            "X11 set_vrr_enabled not implemented",
        ))
    }

    fn set_hdr_metadata(&mut self, output: OutputId, enabled: bool) -> XcbResult<()> {
        self.set_output_hdr_properties(output.0 as u32, enabled);
        Ok(())
    }

    fn begin_move(&mut self, win: WindowId) -> XcbResult<()> {
        let geom = self.window_ops.get_geometry(win)?;
        let (rx, ry) = self.input_ops.get_pointer_position()?;
        if Self::debug_drag_enabled() {
            log::info!(
                "[drag] begin_move win={:?} geom={:?} pointer=({:.1},{:.1})",
                win,
                geom,
                rx,
                ry
            );
        }
        let cursor = self.cursor_provider.get(StdCursorKind::Hand)?.0;
        self.input_ops.set_cursor(StdCursorKind::Hand)?;
        let mask = (EventMaskBits::BUTTON_RELEASE | EventMaskBits::POINTER_MOTION).bits();
        if self.input_ops.grab_pointer(mask, Some(cursor))? {
            self.interaction = Some(XcbInteraction {
                win,
                start_geom: geom,
                start_root_x: rx,
                start_root_y: ry,
                action: InteractionAction::Move,
                current_x: geom.x,
                current_y: geom.y,
                current_w: geom.w,
                current_h: geom.h,
            });
        } else if Self::debug_drag_enabled() {
            log::info!("[drag] begin_move grab_pointer failed win={:?}", win);
        }
        Ok(())
    }

    fn begin_resize(&mut self, win: WindowId, edge: ResizeEdge) -> XcbResult<()> {
        let geom = self.window_ops.get_geometry(win)?;
        let (rx, ry) = self.input_ops.get_pointer_position()?;
        if Self::debug_drag_enabled() {
            log::info!(
                "[drag] begin_resize win={:?} edge={:?} geom={:?}",
                win,
                edge,
                geom
            );
        }
        let cursor_kind = match edge {
            ResizeEdge::Top | ResizeEdge::Bottom => StdCursorKind::VDoubleArrow,
            ResizeEdge::Left | ResizeEdge::Right => StdCursorKind::HDoubleArrow,
            ResizeEdge::TopLeft => StdCursorKind::TopLeftCorner,
            ResizeEdge::TopRight => StdCursorKind::TopRightCorner,
            ResizeEdge::BottomLeft => StdCursorKind::BottomLeftCorner,
            ResizeEdge::BottomRight => StdCursorKind::BottomRightCorner,
        };
        let cursor = self.cursor_provider.get(cursor_kind)?.0;
        self.input_ops.set_cursor(cursor_kind)?;
        let mask = (EventMaskBits::BUTTON_RELEASE | EventMaskBits::POINTER_MOTION).bits();
        if self.input_ops.grab_pointer(mask, Some(cursor))? {
            self.interaction = Some(XcbInteraction {
                win,
                start_geom: geom,
                start_root_x: rx,
                start_root_y: ry,
                action: InteractionAction::Resize(edge),
                current_x: geom.x,
                current_y: geom.y,
                current_w: geom.w,
                current_h: geom.h,
            });
        } else if Self::debug_drag_enabled() {
            log::info!("[drag] begin_resize grab_pointer failed win={:?}", win);
        }
        Ok(())
    }

    fn handle_motion(&mut self, x: f64, y: f64, _time: u32) -> XcbResult<bool> {
        if let Some(state) = self.interaction.as_mut() {
            let dx = (x - state.start_root_x) as i32;
            let dy = (y - state.start_root_y) as i32;
            match state.action {
                InteractionAction::Move => {
                    state.current_x = state.start_geom.x + dx;
                    state.current_y = state.start_geom.y + dy;
                    if Self::debug_drag_enabled() {
                        log::debug!(
                            "[drag] motion(move) win={:?} start=({},{}) dxdy=({},{}) -> pos=({},{}) keep_size=({}x{})",
                            state.win,
                            state.start_geom.x,
                            state.start_geom.y,
                            dx,
                            dy,
                            state.current_x,
                            state.current_y,
                            state.start_geom.w,
                            state.start_geom.h
                        );
                    }
                    self.window_ops
                        .set_position(state.win, state.current_x, state.current_y)?;
                }
                InteractionAction::Resize(_) => {
                    state.current_w = (state.start_geom.w as i32 + dx).max(1) as u32;
                    state.current_h = (state.start_geom.h as i32 + dy).max(1) as u32;
                    if Self::debug_drag_enabled() {
                        log::debug!(
                            "[drag] motion(resize) win={:?} start_size=({}x{}) dxdy=({},{}) -> size=({}x{}) pos=({},{}) border={}",
                            state.win,
                            state.start_geom.w,
                            state.start_geom.h,
                            dx,
                            dy,
                            state.current_w,
                            state.current_h,
                            state.start_geom.x,
                            state.start_geom.y,
                            state.start_geom.border
                        );
                    }
                    self.window_ops.configure(
                        state.win,
                        state.start_geom.x,
                        state.start_geom.y,
                        state.current_w,
                        state.current_h,
                        state.start_geom.border,
                    )?;
                }
            }
            return Ok(true);
        }
        Ok(false)
    }

    fn handle_button_release(&mut self, _time: u32) -> XcbResult<bool> {
        if self.interaction.is_some() {
            if Self::debug_drag_enabled() {
                if let Some(state) = self.interaction.as_ref() {
                    log::info!(
                        "[drag] end_interaction win={:?} action={:?}",
                        state.win,
                        state.action
                    );
                } else {
                    log::info!("[drag] end_interaction");
                }
            }
            self.interaction.take();
            self.input_ops.ungrab_pointer()?;
            self.input_ops.set_cursor(StdCursorKind::LeftPtr)?;
            return Ok(true);
        }
        Ok(false)
    }

    fn interaction_geometry(&self) -> Option<(WindowId, i32, i32, u32, u32)> {
        let s = self.interaction.as_ref()?;
        Some((s.win, s.current_x, s.current_y, s.current_w, s.current_h))
    }

    fn run(&mut self, handler: &mut dyn EventHandler) -> XcbResult<()> {
        let mut event_loop: EventLoop<XcbLoopData> = EventLoop::try_new()?;
        let handle = event_loop.handle();

        let fd = self.conn.as_raw_fd();
        handle
            .insert_source(
                calloop::generic::Generic::new(
                    unsafe { BorrowedFd::borrow_raw(fd) },
                    calloop::Interest::READ,
                    calloop::Mode::Level,
                ),
                |_, _, data| {
                    let mut pending_motion = None;
                    loop {
                        let event = match data.backend.conn.poll_for_event() {
                            Ok(Some(event)) => event,
                            Ok(None) => break,
                            Err(err) => {
                                log::error!("XCB event error (continuing): {err}");
                                continue;
                            }
                        };
                        if let Some(mapped) = data.backend.map_event(event) {
                            data.dispatch_backend_event(mapped, &mut pending_motion, "XCB event");
                        }
                        while let Some(mapped) = data.backend.pending.pop_front() {
                            data.dispatch_backend_event(
                                mapped,
                                &mut pending_motion,
                                "queued XCB event",
                            );
                        }
                    }
                    data.flush_pending_motion(&mut pending_motion, "XCB event");
                    Ok(calloop::PostAction::Continue)
                },
            )
            .map_err(|e| BackendError::Message(format!("Failed to insert XCB source: {e}")))?;

        let signals = Signals::new(&[Signal::SIGCHLD])?;
        handle
            .insert_source(signals, |event, _, data| {
                if event.signal() == Signal::SIGCHLD {
                    if let Err(e) = data
                        .handler
                        .handle_event(data.backend, BackendEvent::ChildProcessExited)
                    {
                        log::error!("Error handling SIGCHLD: {e:?}");
                    }
                }
            })
            .map_err(|e| BackendError::Message(format!("Failed to insert Signal source: {e}")))?;

        let update_interval = Duration::from_millis(20);
        let timer = Timer::from_duration(update_interval);
        handle
            .insert_source(timer, move |_, _, data| {
                if let Err(e) = data.handler.update(data.backend) {
                    log::error!("Error in update loop: {e:?}");
                }
                if data.handler.should_exit() {
                    data.should_exit = true;
                }
                TimeoutAction::ToDuration(update_interval)
            })
            .map_err(|e| BackendError::Message(format!("Failed to insert Timer source: {e}")))?;

        let setup_inotify = || -> Result<(), BackendError> {
            use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};

            let config_path = crate::config::Config::get_default_config_path();
            let watch_dir = config_path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| config_path.clone());
            let config_file_name = config_path.file_name().map(|n| n.to_os_string());

            let inotify = Inotify::init(InitFlags::IN_NONBLOCK)
                .map_err(|e| BackendError::Message(format!("Failed to init inotify: {e}")))?;
            inotify
                .add_watch(
                    &watch_dir,
                    AddWatchFlags::IN_CLOSE_WRITE
                        | AddWatchFlags::IN_MOVED_TO
                        | AddWatchFlags::IN_CREATE,
                )
                .map_err(|e| {
                    BackendError::Message(format!("Failed to watch config dir {watch_dir:?}: {e}"))
                })?;

            handle
                .insert_source(
                    calloop::generic::Generic::new(
                        inotify,
                        calloop::Interest::READ,
                        calloop::Mode::Level,
                    ),
                    move |_, inotify, data| {
                        let events = inotify.read_events().unwrap_or_default();
                        let relevant =
                            events.iter().any(|ev| match (&config_file_name, &ev.name) {
                                (Some(want), Some(got)) => got == want,
                                _ => true,
                            });
                        if relevant {
                            if let Err(e) = data
                                .handler
                                .handle_event(data.backend, BackendEvent::ConfigChanged)
                            {
                                log::error!("Error handling ConfigChanged: {e:?}");
                            }
                        }
                        Ok(calloop::PostAction::Continue)
                    },
                )
                .map_err(|e| {
                    BackendError::Message(format!("Failed to insert inotify source: {e}"))
                })?;
            Ok(())
        };

        if let Err(e) = setup_inotify() {
            log::warn!("Failed to set up config file watching: {e}. Falling back to polling.");
        } else {
            log::info!("Config file hot-reload enabled via inotify");
        }

        let mut data = XcbLoopData {
            backend: self,
            handler,
            should_exit: false,
        };
        while !data.should_exit {
            let timeout = if data.handler.needs_tick() || data.backend.compositor_needs_render() {
                Some(Duration::from_millis(1))
            } else {
                None
            };
            event_loop.dispatch(timeout, &mut data)?;
            if !data.should_exit {
                data.handler.render_compositor_immediate(data.backend);
            }
            if data.backend.benchmark_auto_exit && data.backend.compositor_benchmark_is_complete() {
                if let Some(report) = data.backend.compositor_benchmark_report() {
                    println!("{report}");
                }
                data.should_exit = true;
            }
        }
        Ok(())
    }
}

struct XcbWindowOps {
    conn: Arc<xcb::Connection>,
    ids: XcbIdRegistry,
    atoms: XcbAtoms,
    root: x::Window,
    batcher: XcbRequestBatcher,
}

impl XcbWindowOps {
    fn new(
        conn: Arc<xcb::Connection>,
        ids: XcbIdRegistry,
        atoms: XcbAtoms,
        root: x::Window,
    ) -> Self {
        Self {
            conn,
            ids,
            atoms,
            root,
            batcher: XcbRequestBatcher::new(),
        }
    }

    fn win(&self, win: WindowId) -> XcbResult<x::Window> {
        self.ids.window(win)
    }

    fn send_configure_notify(
        &self,
        win: WindowId,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        border: u32,
    ) -> XcbResult<()> {
        let window = self.win(win)?;
        let event = x::ConfigureNotifyEvent::new(
            window,
            window,
            x::WINDOW_NONE,
            x as i16,
            y as i16,
            w as u16,
            h as u16,
            border as u16,
            false,
        );
        self.conn
            .send_and_check_request(&x::SendEvent {
                propagate: false,
                destination: x::SendEventDest::Window(window),
                event_mask: x::EventMask::STRUCTURE_NOTIFY,
                event: &event,
            })
            .map_err(xcb_err)
    }

    fn detect_numlock_mask(&self) -> x::ModMask {
        let cookie = self.conn.send_request(&x::GetKeyboardMapping {
            first_keycode: self.conn.get_setup().min_keycode(),
            count: self
                .conn
                .get_setup()
                .max_keycode()
                .saturating_sub(self.conn.get_setup().min_keycode())
                .saturating_add(1),
        });
        let Ok(mapping) = self.conn.wait_for_reply(cookie) else {
            return x::ModMask::empty();
        };
        let per = mapping.keysyms_per_keycode() as usize;
        let mut numlock_keys = Vec::new();
        for (idx, symbols) in mapping.keysyms().chunks(per).enumerate() {
            if symbols.contains(&0xff7f) {
                numlock_keys.push(
                    self.conn
                        .get_setup()
                        .min_keycode()
                        .saturating_add(idx as u8),
                );
            }
        }
        if numlock_keys.is_empty() {
            return x::ModMask::empty();
        }

        let cookie = self.conn.send_request(&x::GetModifierMapping {});
        let Ok(reply) = self.conn.wait_for_reply(cookie) else {
            return x::ModMask::empty();
        };
        let per = reply.keycodes_per_modifier() as usize;
        for (idx, chunk) in reply.keycodes().chunks(per).enumerate() {
            if chunk
                .iter()
                .filter(|&&code| code != 0)
                .any(|code| numlock_keys.contains(code))
            {
                return match idx {
                    3 => x::ModMask::N1,
                    4 => x::ModMask::N2,
                    5 => x::ModMask::N3,
                    6 => x::ModMask::N4,
                    7 => x::ModMask::N5,
                    _ => x::ModMask::N2,
                };
            }
        }
        x::ModMask::empty()
    }
}

impl WindowOps for XcbWindowOps {
    fn set_position(&self, win: WindowId, x: i32, y: i32) -> XcbResult<()> {
        if XcbBackend::debug_drag_enabled() {
            log::debug!("[drag] x11 set_position win={:?} x={} y={}", win, x, y);
        }
        self.apply_window_changes(
            win,
            WindowChanges {
                x: Some(x),
                y: Some(y),
                ..Default::default()
            },
        )
    }

    fn configure(
        &self,
        win: WindowId,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        border: u32,
    ) -> XcbResult<()> {
        if XcbBackend::debug_drag_enabled() {
            log::debug!(
                "[drag] x11 configure win={:?} x={} y={} w={} h={} border={}",
                win,
                x,
                y,
                w,
                h,
                border
            );
        }
        self.apply_window_changes(
            win,
            WindowChanges {
                x: Some(x),
                y: Some(y),
                width: Some(w),
                height: Some(h),
                border_width: Some(border),
                ..Default::default()
            },
        )?;
        self.send_configure_notify(win, x, y, w, h, border)
    }

    fn set_decoration_style(
        &self,
        win: WindowId,
        border_width: u32,
        border_color: Pixel,
    ) -> XcbResult<()> {
        let w = self.win(win)?;
        self.conn.send_request(&x::ChangeWindowAttributes {
            window: w,
            value_list: &[x::Cw::BorderPixel(border_color.0)],
        });
        self.batcher.mark_op(&self.conn)?;
        self.apply_window_changes(
            win,
            WindowChanges {
                border_width: Some(border_width),
                ..Default::default()
            },
        )
    }

    fn raise_window(&self, win: WindowId) -> XcbResult<()> {
        self.apply_window_changes(
            win,
            WindowChanges {
                stack_mode: Some(StackMode::Above),
                ..Default::default()
            },
        )
    }

    fn map_window(&self, win: WindowId) -> XcbResult<()> {
        self.conn
            .send_and_check_request(&x::MapWindow {
                window: self.win(win)?,
            })
            .map_err(xcb_err)
    }

    fn unmap_window(&self, win: WindowId) -> XcbResult<()> {
        self.conn
            .send_and_check_request(&x::UnmapWindow {
                window: self.win(win)?,
            })
            .map_err(xcb_err)
    }

    fn close_window(&self, win: WindowId) -> XcbResult<CloseResult> {
        let w = self.win(win)?;
        let protocols = get_atoms_with_length(
            &self.conn,
            w,
            self.atoms.wm_protocols,
            self.atoms.atom,
            MAX_PROTOCOL_ATOMS,
        );
        if protocol_supported(&protocols, self.atoms.wm_delete_window) {
            let event = x::ClientMessageEvent::new(
                w,
                self.atoms.wm_protocols,
                x::ClientMessageData::Data32(wm_delete_window_message(
                    self.atoms.wm_delete_window.resource_id(),
                )),
            );
            self.conn
                .send_and_check_request(&x::SendEvent {
                    propagate: false,
                    destination: x::SendEventDest::Window(w),
                    event_mask: x::EventMask::NO_EVENT,
                    event: &event,
                })
                .map_err(xcb_err)?;
            self.conn.flush().map_err(xcb_err)?;
            Ok(CloseResult::Graceful)
        } else {
            self.kill_client(win)?;
            Ok(CloseResult::Forced)
        }
    }

    fn set_input_focus(&self, win: WindowId) -> XcbResult<()> {
        self.conn
            .send_and_check_request(&x::SetInputFocus {
                revert_to: x::InputFocus::Parent,
                focus: self.win(win)?,
                time: x::CURRENT_TIME,
            })
            .map_err(xcb_err)
    }

    fn set_input_focus_root(&self) -> XcbResult<()> {
        self.conn
            .send_and_check_request(&x::SetInputFocus {
                revert_to: x::InputFocus::PointerRoot,
                focus: self.root,
                time: x::CURRENT_TIME,
            })
            .map_err(xcb_err)
    }

    fn get_window_attributes(&self, win: WindowId) -> XcbResult<WindowAttributes> {
        let cookie = self.conn.send_request(&x::GetWindowAttributes {
            window: self.win(win)?,
        });
        let reply = self.conn.wait_for_reply(cookie).map_err(xcb_err)?;
        Ok(WindowAttributes {
            override_redirect: reply.override_redirect(),
            map_state_viewable: reply.map_state() == x::MapState::Viewable,
        })
    }

    fn get_geometry(&self, win: WindowId) -> XcbResult<Geometry> {
        let cookie = self.conn.send_request(&x::GetGeometry {
            drawable: x::Drawable::Window(self.win(win)?),
        });
        let r = self.conn.wait_for_reply(cookie).map_err(xcb_err)?;
        Ok(Geometry {
            x: r.x() as i32,
            y: r.y() as i32,
            w: r.width() as u32,
            h: r.height() as u32,
            border: r.border_width() as u32,
        })
    }

    fn scan_windows(&self) -> XcbResult<Vec<WindowId>> {
        let cookie = self.conn.send_request(&x::QueryTree { window: self.root });
        let reply = self.conn.wait_for_reply(cookie).map_err(xcb_err)?;
        Ok(reply
            .children()
            .iter()
            .map(|&w| self.ids.intern(w))
            .collect())
    }

    fn flush(&self) -> XcbResult<()> {
        self.batcher.flush(&self.conn)
    }

    fn kill_client(&self, win: WindowId) -> XcbResult<()> {
        self.conn
            .send_and_check_request(&x::KillClient {
                resource: self.win(win)?.resource_id(),
            })
            .map_err(xcb_err)
    }

    fn apply_window_changes(&self, win: WindowId, changes: WindowChanges) -> XcbResult<()> {
        let mut values = Vec::new();
        if let Some(x) = changes.x {
            values.push(x::ConfigWindow::X(x));
        }
        if let Some(y) = changes.y {
            values.push(x::ConfigWindow::Y(y));
        }
        if let Some(w) = changes.width {
            values.push(x::ConfigWindow::Width(w));
        }
        if let Some(h) = changes.height {
            values.push(x::ConfigWindow::Height(h));
        }
        if let Some(b) = changes.border_width {
            values.push(x::ConfigWindow::BorderWidth(b));
        }
        if let Some(sibling) = changes.sibling {
            values.push(x::ConfigWindow::Sibling(self.win(sibling)?));
        }
        if let Some(mode) = changes.stack_mode {
            values.push(x::ConfigWindow::StackMode(stack_mode_to_xcb(mode)));
        }
        if values.is_empty() {
            return Ok(());
        }
        self.conn.send_request(&x::ConfigureWindow {
            window: self.win(win)?,
            value_list: &values,
        });
        self.batcher.mark_op(&self.conn)
    }

    fn ungrab_all_buttons(&self, win: WindowId) -> XcbResult<()> {
        self.conn
            .send_and_check_request(&x::UngrabButton {
                button: x::ButtonIndex::Any,
                grab_window: self.win(win)?,
                modifiers: x::ModMask::ANY,
            })
            .map_err(xcb_err)?;
        self.conn.flush().map_err(xcb_err)
    }

    fn grab_button_any_anymod(&self, win: WindowId, mask: u32) -> XcbResult<()> {
        self.conn
            .send_and_check_request(&x::GrabButton {
                owner_events: false,
                grab_window: self.win(win)?,
                event_mask: event_mask_from_bits(mask),
                pointer_mode: x::GrabMode::Async,
                keyboard_mode: x::GrabMode::Async,
                confine_to: x::WINDOW_NONE,
                cursor: x::CURSOR_NONE,
                button: x::ButtonIndex::Any,
                modifiers: x::ModMask::ANY,
            })
            .map_err(xcb_err)?;
        self.conn.flush().map_err(xcb_err)
    }

    fn grab_button(&self, win: WindowId, btn: u8, mask: u32, mods: Mods) -> XcbResult<()> {
        let window = self.win(win)?;
        let base = mods_to_xcb(mods);
        let numlock = self.detect_numlock_mask();
        let combos = lock_modifier_combinations(base, x::ModMask::LOCK, numlock);
        for modifiers in combos {
            self.conn
                .send_and_check_request(&x::GrabButton {
                    owner_events: false,
                    grab_window: window,
                    event_mask: event_mask_from_bits(mask),
                    pointer_mode: x::GrabMode::Async,
                    keyboard_mode: x::GrabMode::Async,
                    confine_to: x::WINDOW_NONE,
                    cursor: x::CURSOR_NONE,
                    button: button_index(btn),
                    modifiers,
                })
                .map_err(xcb_err)?;
        }
        self.conn.flush().map_err(xcb_err)
    }

    fn change_event_mask(&self, win: WindowId, mask: u32) -> XcbResult<()> {
        self.conn
            .send_and_check_request(&x::ChangeWindowAttributes {
                window: self.win(win)?,
                value_list: &[x::Cw::EventMask(event_mask_from_bits(mask))],
            })
            .map_err(xcb_err)
    }

    fn get_tree_child(&self, win: WindowId) -> XcbResult<Vec<WindowId>> {
        let cookie = self.conn.send_request(&x::QueryTree {
            window: self.win(win)?,
        });
        let reply = self.conn.wait_for_reply(cookie).map_err(xcb_err)?;
        Ok(reply
            .children()
            .iter()
            .map(|&w| self.ids.intern(w))
            .collect())
    }

    fn send_take_focus(&self, win: WindowId) -> XcbResult<bool> {
        let w = self.win(win)?;
        let protocols = get_atoms_with_length(
            &self.conn,
            w,
            self.atoms.wm_protocols,
            self.atoms.atom,
            MAX_PROTOCOL_ATOMS,
        );
        if !protocol_supported(&protocols, self.atoms.wm_take_focus) {
            return Ok(false);
        }
        let event = x::ClientMessageEvent::new(
            w,
            self.atoms.wm_protocols,
            x::ClientMessageData::Data32(wm_take_focus_message(
                self.atoms.wm_take_focus.resource_id(),
                x::CURRENT_TIME,
            )),
        );
        self.conn
            .send_and_check_request(&x::SendEvent {
                propagate: false,
                destination: x::SendEventDest::Window(w),
                event_mask: x::EventMask::NO_EVENT,
                event: &event,
            })
            .map_err(xcb_err)?;
        self.conn.flush().map_err(xcb_err)?;
        Ok(true)
    }

    fn restack_windows(&self, windows: &[WindowId]) -> XcbResult<()> {
        for (window, changes) in restack_window_changes(windows) {
            if self.ids.window(window).is_ok() {
                self.apply_window_changes(window, changes)?;
            }
        }
        Ok(())
    }

    fn shape_select_input(&self, win: WindowId) -> XcbResult<()> {
        let w = self.win(win)?;
        self.conn
            .send_and_check_request(&xcb::shape::SelectInput {
                destination_window: w,
                enable: true,
            })
            .map_err(xcb_err)
    }

    fn get_window_shaped(&self, win: WindowId) -> bool {
        let w = match self.win(win) {
            Ok(w) => w,
            Err(_) => return false,
        };
        let cookie = self.conn.send_request(&xcb::shape::QueryExtents {
            destination_window: w,
        });
        match self.conn.wait_for_reply(cookie) {
            Ok(reply) => reply.bounding_shaped(),
            Err(_) => false,
        }
    }
}

struct XcbInputOps {
    conn: Arc<xcb::Connection>,
    ids: XcbIdRegistry,
    root: x::Window,
}

impl XcbInputOps {
    fn new(conn: Arc<xcb::Connection>, ids: XcbIdRegistry, root: x::Window) -> Self {
        Self { conn, ids, root }
    }
}

impl InputOps for XcbInputOps {
    fn set_cursor(&self, _kind: StdCursorKind) -> XcbResult<()> {
        Ok(())
    }

    fn get_pointer_position(&self) -> XcbResult<(f64, f64)> {
        let (x, y, _, _) = self.query_pointer_root()?;
        Ok((x as f64, y as f64))
    }

    fn grab_pointer(&self, mask: u32, cursor: Option<u64>) -> XcbResult<bool> {
        let event_mask = if mask != 0 {
            event_mask_from_bits(mask)
        } else {
            x::EventMask::BUTTON_RELEASE | x::EventMask::POINTER_MOTION
        };
        let cookie = self.conn.send_request(&x::GrabPointer {
            owner_events: false,
            grab_window: self.root,
            event_mask,
            pointer_mode: x::GrabMode::Async,
            keyboard_mode: x::GrabMode::Async,
            confine_to: x::WINDOW_NONE,
            cursor: cursor.map_or(x::CURSOR_NONE, |c| x::Cursor::new(c as u32)),
            time: x::CURRENT_TIME,
        });
        let reply = self.conn.wait_for_reply(cookie).map_err(xcb_err)?;
        Ok(reply.status() == x::GrabStatus::Success)
    }

    fn ungrab_pointer(&self) -> XcbResult<()> {
        self.conn
            .send_and_check_request(&x::UngrabPointer {
                time: x::CURRENT_TIME,
            })
            .map_err(xcb_err)
    }

    fn warp_pointer(&self, x: f64, y: f64) -> XcbResult<()> {
        self.conn
            .send_and_check_request(&x::WarpPointer {
                src_window: x::WINDOW_NONE,
                dst_window: self.root,
                src_x: 0,
                src_y: 0,
                src_width: 0,
                src_height: 0,
                dst_x: x as i16,
                dst_y: y as i16,
            })
            .map_err(xcb_err)
    }

    fn query_pointer_root(&self) -> XcbResult<(i32, i32, u16, u16)> {
        let cookie = self
            .conn
            .send_request(&x::QueryPointer { window: self.root });
        let r = self.conn.wait_for_reply(cookie).map_err(xcb_err)?;
        Ok((
            r.root_x() as i32,
            r.root_y() as i32,
            r.mask().bits() as u16,
            0,
        ))
    }

    fn warp_pointer_to_window(&self, win: WindowId, x: i16, y: i16) -> XcbResult<()> {
        self.conn
            .send_and_check_request(&x::WarpPointer {
                src_window: x::WINDOW_NONE,
                dst_window: self.ids.window(win)?,
                src_x: 0,
                src_y: 0,
                src_width: 0,
                src_height: 0,
                dst_x: x,
                dst_y: y,
            })
            .map_err(xcb_err)
    }

    fn allow_events(&self, mode: AllowMode, time: u32) -> XcbResult<()> {
        self.conn
            .send_and_check_request(&x::AllowEvents {
                mode: match mode {
                    AllowMode::AsyncPointer => x::Allow::AsyncPointer,
                    AllowMode::ReplayPointer => x::Allow::ReplayPointer,
                    AllowMode::SyncPointer => x::Allow::SyncPointer,
                    AllowMode::AsyncKeyboard => x::Allow::AsyncKeyboard,
                    AllowMode::ReplayKeyboard => x::Allow::ReplayKeyboard,
                    AllowMode::SyncKeyboard => x::Allow::SyncKeyboard,
                    AllowMode::AsyncBoth => x::Allow::AsyncBoth,
                    AllowMode::SyncBoth => x::Allow::SyncBoth,
                },
                time,
            })
            .map_err(xcb_err)
    }
}

struct XcbPropertyOps {
    conn: Arc<xcb::Connection>,
    ids: XcbIdRegistry,
    atoms: XcbAtoms,
}

impl XcbPropertyOps {
    fn new(conn: Arc<xcb::Connection>, ids: XcbIdRegistry, atoms: XcbAtoms) -> Self {
        Self { conn, ids, atoms }
    }

    fn win(&self, win: WindowId) -> XcbResult<x::Window> {
        self.ids.window(win)
    }

    fn get_text_property(&self, win: WindowId, property: x::Atom) -> Option<String> {
        let Ok(w) = self.win(win) else {
            return None;
        };
        let cookie = self.conn.send_request(&x::GetProperty {
            delete: false,
            window: w,
            property,
            r#type: x::ATOM_ANY,
            long_offset: 0,
            long_length: MAX_TEXT_PROPERTY_BYTES,
        });
        self.conn.wait_for_reply(cookie).ok().and_then(|r| {
            if r.format() != 8 {
                return None;
            }
            decode_text_property(
                r.value::<u8>(),
                r.r#type(),
                self.atoms.utf8_string,
                self.atoms.string,
            )
        })
    }

    fn get_string(&self, win: WindowId, property: x::Atom) -> String {
        self.get_text_property(win, property).unwrap_or_default()
    }

    fn states(&self, win: WindowId) -> Vec<x::Atom> {
        self.win(win)
            .ok()
            .map(|w| {
                get_u32s_with_length(
                    &self.conn,
                    w,
                    self.atoms.net_wm_state,
                    self.atoms.atom,
                    MAX_ATOM_LIST_ITEMS,
                )
                .into_iter()
                .map(x::Atom::new)
                .collect()
            })
            .unwrap_or_default()
    }
}

impl PropertyOps for XcbPropertyOps {
    fn get_title(&self, win: WindowId) -> String {
        let title = self.get_string(win, self.atoms.net_wm_name);
        if title.is_empty() {
            self.get_string(win, x::ATOM_WM_NAME)
        } else {
            title
        }
    }

    fn get_class(&self, win: WindowId) -> (String, String) {
        let Ok(w) = self.win(win) else {
            return (String::new(), String::new());
        };
        let cookie = self.conn.send_request(&x::GetProperty {
            delete: false,
            window: w,
            property: self.atoms.wm_class,
            r#type: self.atoms.string,
            long_offset: 0,
            long_length: 256,
        });
        let Ok(reply) = self.conn.wait_for_reply(cookie) else {
            return (String::new(), String::new());
        };
        if reply.r#type() == self.atoms.string
            && reply.format() == 8
            && !reply.value::<u8>().is_empty()
        {
            parse_wm_class(reply.value::<u8>())
        } else {
            (String::new(), String::new())
        }
    }

    fn get_window_types(&self, win: WindowId) -> Vec<WindowType> {
        let Ok(w) = self.win(win) else {
            return Vec::new();
        };
        let atoms: Vec<x::Atom> = get_u32s_with_length(
            &self.conn,
            w,
            self.atoms.net_wm_window_type,
            self.atoms.atom,
            MAX_ATOM_LIST_ITEMS,
        )
        .into_iter()
        .map(x::Atom::new)
        .collect();
        let mut result = atoms
            .into_iter()
            .map(|a| window_type_from_atom(a, self.atoms.window_type_atoms()))
            .filter(|wt| *wt != WindowType::Unknown)
            .collect::<Vec<_>>();
        if result.is_empty() {
            if self.transient_for(win).is_some() {
                result.push(WindowType::Dialog);
            } else {
                result.push(WindowType::Normal);
            }
        }
        result
    }

    fn is_fullscreen(&self, win: WindowId) -> bool {
        self.states(win)
            .contains(&self.atoms.net_wm_state_fullscreen)
    }

    fn set_fullscreen_state(&self, win: WindowId, on: bool) -> XcbResult<()> {
        self.set_net_wm_state_flag(win, NetWmState::Fullscreen, on)
    }

    fn transient_for(&self, win: WindowId) -> Option<WindowId> {
        let w = self.win(win).ok()?;
        get_windows(
            &self.conn,
            w,
            self.atoms.wm_transient_for,
            self.atoms.window,
        )
        .first()
        .copied()
        .and_then(|transient| {
            if transient.resource_id() == 0 || transient == w {
                None
            } else {
                Some(self.ids.intern(transient))
            }
        })
    }

    fn get_wm_hints(&self, win: WindowId) -> Option<WmHints> {
        let w = self.win(win).ok()?;
        let values = get_u32s_with_length(
            &self.conn,
            w,
            self.atoms.wm_hints,
            self.atoms.wm_hints,
            MAX_WM_HINTS_ITEMS,
        );
        parse_wm_hints(&values)
    }

    fn set_urgent_hint(&self, win: WindowId, urgent: bool) -> XcbResult<()> {
        const X_URGENCY_HINT: u32 = 1 << 8;
        let w = self.win(win)?;
        let mut data = get_u32s_with_length(
            &self.conn,
            w,
            self.atoms.wm_hints,
            self.atoms.wm_hints,
            MAX_WM_HINTS_ITEMS,
        );
        if data.is_empty() {
            data.push(0);
        }
        if urgent {
            data[0] |= X_URGENCY_HINT;
        } else {
            data[0] &= !X_URGENCY_HINT;
        }
        change_u32s(
            &self.conn,
            w,
            self.atoms.wm_hints,
            self.atoms.wm_hints,
            &data,
        )
    }

    fn fetch_normal_hints(&self, win: WindowId) -> XcbResult<Option<NormalHints>> {
        let w = self.win(win)?;
        let v = get_u32s_with_length(
            &self.conn,
            w,
            self.atoms.wm_normal_hints,
            self.atoms.wm_size_hints,
            MAX_WM_NORMAL_HINTS_ITEMS,
        );
        Ok(parse_normal_hints(&v))
    }

    fn set_window_strut_top(
        &self,
        win: WindowId,
        top: u32,
        start_x: u32,
        end_x: u32,
    ) -> XcbResult<()> {
        let w = self.win(win)?;
        change_u32s(
            &self.conn,
            w,
            self.atoms.net_wm_strut,
            self.atoms.cardinal,
            &[0, 0, top, 0],
        )?;
        change_u32s(
            &self.conn,
            w,
            self.atoms.net_wm_strut_partial,
            self.atoms.cardinal,
            &[0, 0, top, 0, 0, 0, 0, 0, start_x, end_x, 0, 0],
        )
    }

    fn set_window_type_dock(&self, win: WindowId) -> XcbResult<()> {
        change_u32s(
            &self.conn,
            self.win(win)?,
            self.atoms.net_wm_window_type,
            self.atoms.atom,
            &[self.atoms.net_wm_window_type_dock.resource_id()],
        )
    }

    fn clear_window_strut(&self, win: WindowId) -> XcbResult<()> {
        let w = self.win(win)?;
        self.conn
            .send_and_check_request(&x::DeleteProperty {
                window: w,
                property: self.atoms.net_wm_strut,
            })
            .map_err(xcb_err)?;
        self.conn
            .send_and_check_request(&x::DeleteProperty {
                window: w,
                property: self.atoms.net_wm_strut_partial,
            })
            .map_err(xcb_err)
    }

    fn get_wm_state(&self, win: WindowId) -> XcbResult<i64> {
        let w = self.win(win)?;
        Ok(get_u32s_with_length(
            &self.conn,
            w,
            self.atoms.wm_state,
            self.atoms.wm_state,
            MAX_WM_STATE_ITEMS,
        )
        .first()
        .copied()
        .map(|state| state as i64)
        .unwrap_or(-1))
    }

    fn set_wm_state(&self, win: WindowId, state: i64) -> XcbResult<()> {
        change_u32s(
            &self.conn,
            self.win(win)?,
            self.atoms.wm_state,
            self.atoms.wm_state,
            &[state as u32, 0],
        )
    }

    fn set_client_info_props(&self, win: WindowId, tags: u32, monitor_num: u32) -> XcbResult<()> {
        change_u32s(
            &self.conn,
            self.win(win)?,
            self.atoms.net_client_info,
            self.atoms.cardinal,
            &[tags, monitor_num],
        )
    }

    fn get_window_strut_partial(&self, win: WindowId) -> Option<StrutPartial> {
        let w = self.win(win).ok()?;
        let v = get_u32s_with_length(
            &self.conn,
            w,
            self.atoms.net_wm_strut_partial,
            self.atoms.cardinal,
            MAX_STRUT_PARTIAL_ITEMS,
        );
        if let Some(strut) = parse_strut_partial(&v) {
            return Some(strut);
        }
        let fallback = get_u32s_with_length(
            &self.conn,
            w,
            self.atoms.net_wm_strut,
            self.atoms.cardinal,
            MAX_STRUT_ITEMS,
        );
        parse_strut(&fallback)
    }

    fn get_layer_surface_info(&self, _win: WindowId) -> Option<LayerSurfaceInfo> {
        None
    }

    fn get_window_pid(&self, win: WindowId) -> Option<u32> {
        let w = self.win(win).ok()?;
        get_u32s_with_length(
            &self.conn,
            w,
            self.atoms.net_wm_pid,
            self.atoms.cardinal,
            MAX_SINGLE_U32_ITEMS,
        )
        .first()
        .copied()
    }

    fn set_net_wm_state_flag(&self, win: WindowId, state: NetWmState, on: bool) -> XcbResult<()> {
        let w = self.win(win)?;
        let atom = self.atoms.atom_for_state(state);
        let mut states = get_atoms(&self.conn, w, self.atoms.net_wm_state, self.atoms.atom);
        let has = states.contains(&atom);
        if on && !has {
            states.push(atom);
        } else if !on && has {
            states.retain(|a| *a != atom);
        }
        let raw: Vec<u32> = states.iter().map(Xid::resource_id).collect();
        change_u32s(
            &self.conn,
            w,
            self.atoms.net_wm_state,
            self.atoms.atom,
            &raw,
        )
    }

    fn set_frame_extents(
        &self,
        win: WindowId,
        left: u32,
        right: u32,
        top: u32,
        bottom: u32,
    ) -> XcbResult<()> {
        change_u32s(
            &self.conn,
            self.win(win)?,
            self.atoms.net_frame_extents,
            self.atoms.cardinal,
            &[left, right, top, bottom],
        )
    }

    fn set_allowed_actions(&self, win: WindowId, actions: &[AllowedAction]) -> XcbResult<()> {
        let raw = actions
            .iter()
            .map(|action| {
                atom_for_allowed_action(*action, self.atoms.allowed_action_atoms()).resource_id()
            })
            .collect::<Vec<_>>();
        change_u32s(
            &self.conn,
            self.win(win)?,
            self.atoms.net_wm_allowed_actions,
            self.atoms.atom,
            &raw,
        )
    }

    fn send_ping(&self, win: WindowId, timestamp: u32) -> XcbResult<bool> {
        let w = self.win(win)?;
        let protocols = get_atoms_with_length(
            &self.conn,
            w,
            self.atoms.wm_protocols,
            self.atoms.atom,
            MAX_PROTOCOL_ATOMS,
        );
        if !protocol_supported(&protocols, self.atoms.net_wm_ping) {
            return Ok(false);
        }
        let event = x::ClientMessageEvent::new(
            w,
            self.atoms.wm_protocols,
            x::ClientMessageData::Data32(net_wm_ping_message(
                self.atoms.net_wm_ping.resource_id(),
                timestamp,
                w.resource_id(),
            )),
        );
        self.conn
            .send_and_check_request(&x::SendEvent {
                propagate: false,
                destination: x::SendEventDest::Window(w),
                event_mask: x::EventMask::NO_EVENT,
                event: &event,
            })
            .map_err(xcb_err)?;
        self.conn.flush().map_err(xcb_err)?;
        Ok(true)
    }

    fn get_user_time(&self, win: WindowId) -> Option<u32> {
        let w = self.win(win).ok()?;
        get_u32s_with_length(
            &self.conn,
            w,
            self.atoms.net_wm_user_time,
            self.atoms.cardinal,
            MAX_SINGLE_U32_ITEMS,
        )
        .first()
        .copied()
    }

    fn get_net_wm_icon(&self, win: WindowId) -> Option<Vec<IconData>> {
        let w = self.win(win).ok()?;
        let values = get_u32s_with_length(
            &self.conn,
            w,
            self.atoms.net_wm_icon,
            self.atoms.cardinal,
            MAX_ICON_ITEMS_U32,
        );
        parse_icon_data(&values)
    }

    fn get_bypass_compositor(&self, win: WindowId) -> Option<u32> {
        let w = self.win(win).ok()?;
        get_u32s_with_length(
            &self.conn,
            w,
            self.atoms.net_wm_bypass_compositor,
            self.atoms.cardinal,
            MAX_SINGLE_U32_ITEMS,
        )
        .first()
        .copied()
    }

    fn get_opaque_region(&self, win: WindowId) -> Option<Vec<(i32, i32, u32, u32)>> {
        let w = self.win(win).ok()?;
        let values = get_u32s_with_length(
            &self.conn,
            w,
            self.atoms.net_wm_opaque_region,
            self.atoms.cardinal,
            MAX_OPAQUE_REGION_ITEMS_U32,
        );
        parse_opaque_region(&values)
    }

    fn get_motif_hints(&self, win: WindowId) -> Option<MotifWmHints> {
        let w = self.win(win).ok()?;
        let values = get_u32s_with_length(
            &self.conn,
            w,
            self.atoms.motif_wm_hints,
            x::ATOM_ANY,
            MAX_MOTIF_HINTS_ITEMS,
        );
        parse_motif_hints(&values)
    }

    fn get_gtk_frame_extents(&self, win: WindowId) -> Option<[u32; 4]> {
        let w = self.win(win).ok()?;
        let values = get_u32s_with_length(
            &self.conn,
            w,
            self.atoms.gtk_frame_extents,
            self.atoms.cardinal,
            MAX_GTK_FRAME_EXTENTS_ITEMS,
        );
        parse_gtk_frame_extents(&values)
    }

    fn get_sync_counter(&self, win: WindowId) -> Option<u32> {
        let w = self.win(win).ok()?;
        get_u32s_with_length(
            &self.conn,
            w,
            self.atoms.net_wm_sync_request_counter,
            self.atoms.cardinal,
            MAX_SINGLE_U32_ITEMS,
        )
        .first()
        .copied()
    }

    fn send_sync_request(&self, win: WindowId, _counter: u32, value: u64) -> XcbResult<()> {
        let w = self.win(win)?;
        let event = x::ClientMessageEvent::new(
            w,
            self.atoms.wm_protocols,
            x::ClientMessageData::Data32(net_wm_sync_request_message(
                self.atoms.net_wm_sync_request.resource_id(),
                x::CURRENT_TIME,
                value,
            )),
        );
        self.conn
            .send_and_check_request(&x::SendEvent {
                propagate: false,
                destination: x::SendEventDest::Window(w),
                event_mask: x::EventMask::NO_EVENT,
                event: &event,
            })
            .map_err(xcb_err)?;
        self.conn.flush().map_err(xcb_err)
    }
}

struct XcbOutputOps {
    conn: Arc<xcb::Connection>,
    root: x::Window,
    width: i32,
    height: i32,
    cached_outputs: Mutex<Option<Vec<OutputInfo>>>,
}

impl XcbOutputOps {
    fn new(conn: Arc<xcb::Connection>, root: x::Window, width: i32, height: i32) -> Self {
        Self {
            conn,
            root,
            width,
            height,
            cached_outputs: Mutex::new(None),
        }
    }

    fn calc_refresh_mhz(mode: &xcb::randr::ModeInfo) -> u32 {
        if mode.htotal == 0 || mode.vtotal == 0 {
            return 60000;
        }
        let mut vtotal = mode.vtotal as u64;
        let flags = mode.mode_flags.bits();
        if flags & (1 << 4) != 0 {
            vtotal *= 2;
        }
        if flags & (1 << 0) != 0 {
            vtotal /= 2;
        }
        let denom = mode.htotal as u64 * vtotal;
        if denom == 0 {
            return 60000;
        }
        ((mode.dot_clock as u64 * 1000) / denom) as u32
    }

    fn query_output_hdr_capable(&self, output: xcb::randr::Output) -> bool {
        let atom = match XcbAtoms::intern(&self.conn, b"max_bpc") {
            Ok(atom) => atom,
            Err(_) => return true,
        };
        let cookie = self.conn.send_request(&xcb::randr::GetOutputProperty {
            output,
            property: atom,
            r#type: x::ATOM_INTEGER,
            long_offset: 0,
            long_length: 1,
            delete: false,
            pending: false,
        });
        let Ok(reply) = self.conn.wait_for_reply(cookie) else {
            return true;
        };
        if reply.format() != 32 || reply.data::<u32>().is_empty() {
            return true;
        }
        reply.data::<u32>()[0] >= 10
    }

    fn query_output_edid_hdr(
        &self,
        output: xcb::randr::Output,
    ) -> Option<crate::backend::edid::EdidHdrCapabilities> {
        let atom = XcbAtoms::intern(&self.conn, b"EDID").ok()?;
        let cookie = self.conn.send_request(&xcb::randr::GetOutputProperty {
            output,
            property: atom,
            r#type: x::ATOM_ANY,
            long_offset: 0,
            long_length: 256,
            delete: false,
            pending: false,
        });
        let reply = self.conn.wait_for_reply(cookie).ok()?;
        let data = reply.data::<u8>();
        if data.len() < 128 {
            return None;
        }
        crate::backend::edid::parse_edid_hdr_from_bytes(data)
    }

    fn get_cached_or_query(&self) -> Vec<OutputInfo> {
        if let Ok(cache) = self.cached_outputs.lock() {
            if let Some(outputs) = cache.as_ref() {
                return outputs.clone();
            }
        }

        let outputs = self.query_outputs_internal();
        if let Ok(mut cache) = self.cached_outputs.lock() {
            *cache = Some(outputs.clone());
        }
        outputs
    }

    fn query_outputs_internal(&self) -> Vec<OutputInfo> {
        let version_cookie = self.conn.send_request(&xcb::randr::QueryVersion {
            major_version: 1,
            minor_version: 5,
        });
        if let Ok(version) = self.conn.wait_for_reply(version_cookie) {
            if version.major_version() > 1
                || (version.major_version() == 1 && version.minor_version() >= 5)
            {
                let cookie = self.conn.send_request(&xcb::randr::GetMonitors {
                    window: self.root,
                    get_active: true,
                });
                if let Ok(reply) = self.conn.wait_for_reply(cookie) {
                    let resources_cookie = self
                        .conn
                        .send_request(&xcb::randr::GetScreenResources { window: self.root });
                    let modes = self
                        .conn
                        .wait_for_reply(resources_cookie)
                        .ok()
                        .map(|r| r.modes().to_vec())
                        .unwrap_or_default();

                    let mut outputs = Vec::new();
                    for (idx, monitor) in reply.monitors().enumerate() {
                        if monitor.width() == 0 || monitor.height() == 0 {
                            continue;
                        }
                        let first_output = monitor.outputs().first().copied();
                        let refresh = first_output
                            .and_then(|output| {
                                let output_info = self
                                    .conn
                                    .wait_for_reply(self.conn.send_request(
                                        &xcb::randr::GetOutputInfo {
                                            output,
                                            config_timestamp: 0,
                                        },
                                    ))
                                    .ok()?;
                                if output_info.crtc() == xcb::randr::Crtc::none() {
                                    return None;
                                }
                                self.conn
                                    .wait_for_reply(self.conn.send_request(
                                        &xcb::randr::GetCrtcInfo {
                                            crtc: output_info.crtc(),
                                            config_timestamp: 0,
                                        },
                                    ))
                                    .ok()
                            })
                            .and_then(|crtc_info| {
                                modes
                                    .iter()
                                    .find(|mode| mode.id == crtc_info.mode().resource_id())
                                    .map(Self::calc_refresh_mhz)
                            })
                            .unwrap_or(DEFAULT_OUTPUT_REFRESH_MHZ);

                        let (hdr_capable, hdr_metadata) = if let Some(output) = first_output {
                            let caps = self.query_output_edid_hdr(output);
                            (
                                self.query_output_hdr_capable(output) || caps.is_some(),
                                caps,
                            )
                        } else {
                            (false, None)
                        };

                        let id = first_output
                            .map(|output| OutputId(output.resource_id() as u64))
                            .unwrap_or(OutputId(idx as u64));
                        let name = first_output
                            .and_then(|output| {
                                self.conn
                                    .wait_for_reply(self.conn.send_request(
                                        &xcb::randr::GetOutputInfo {
                                            output,
                                            config_timestamp: 0,
                                        },
                                    ))
                                    .ok()
                                    .and_then(|info| String::from_utf8(info.name().to_vec()).ok())
                            })
                            .filter(|name| !name.is_empty())
                            .unwrap_or_else(|| format!("Monitor-{}", idx));

                        outputs.push(build_output_info(
                            id,
                            name,
                            monitor.x() as i32,
                            monitor.y() as i32,
                            monitor.width() as i32,
                            monitor.height() as i32,
                            refresh,
                            hdr_capable,
                            hdr_metadata,
                        ));
                    }
                    if !outputs.is_empty() {
                        return outputs;
                    }
                }
            }
        }

        let cookie = self
            .conn
            .send_request(&xcb::randr::GetScreenResources { window: self.root });
        if let Ok(resources) = self.conn.wait_for_reply(cookie) {
            let modes = resources.modes().to_vec();
            let mut outputs = Vec::new();
            for (idx, crtc) in resources.crtcs().iter().enumerate() {
                let info_cookie = self.conn.send_request(&xcb::randr::GetCrtcInfo {
                    crtc: *crtc,
                    config_timestamp: 0,
                });
                if let Ok(info) = self.conn.wait_for_reply(info_cookie) {
                    if info.width() == 0 || info.height() == 0 {
                        continue;
                    }
                    let refresh = modes
                        .iter()
                        .find(|mode| mode.id == info.mode().resource_id())
                        .map(Self::calc_refresh_mhz)
                        .unwrap_or(DEFAULT_OUTPUT_REFRESH_MHZ);
                    let first_output = info.outputs().first().copied();
                    let id = first_output
                        .map(|output| OutputId(output.resource_id() as u64))
                        .unwrap_or(OutputId(crtc.resource_id() as u64));
                    let name = first_output
                        .and_then(|output| {
                            self.conn
                                .wait_for_reply(self.conn.send_request(
                                    &xcb::randr::GetOutputInfo {
                                        output,
                                        config_timestamp: 0,
                                    },
                                ))
                                .ok()
                                .and_then(|info| String::from_utf8(info.name().to_vec()).ok())
                        })
                        .filter(|name| !name.is_empty())
                        .unwrap_or_else(|| format!("CRTC-{}", idx));
                    let (hdr_capable, hdr_metadata) = if let Some(output) = first_output {
                        let caps = self.query_output_edid_hdr(output);
                        (
                            self.query_output_hdr_capable(output) || caps.is_some(),
                            caps,
                        )
                    } else {
                        (false, None)
                    };
                    outputs.push(build_output_info(
                        id,
                        name,
                        info.x() as i32,
                        info.y() as i32,
                        info.width() as i32,
                        info.height() as i32,
                        refresh,
                        hdr_capable,
                        hdr_metadata,
                    ));
                }
            }
            if !outputs.is_empty() {
                return outputs;
            }
        }

        vec![fallback_output("Default", self.width, self.height)]
    }

    fn output_to_crtc(&self, output_id: u32) -> Option<xcb::randr::Crtc> {
        let cookie = self
            .conn
            .send_request(&xcb::randr::GetScreenResources { window: self.root });
        let resources = self.conn.wait_for_reply(cookie).ok()?;
        if resources
            .crtcs()
            .iter()
            .any(|crtc| crtc.resource_id() == output_id)
        {
            return Some(xcb::randr::Crtc::new(output_id));
        }
        for output in resources.outputs() {
            if output.resource_id() != output_id {
                continue;
            }
            let info = self
                .conn
                .wait_for_reply(self.conn.send_request(&xcb::randr::GetOutputInfo {
                    output: *output,
                    config_timestamp: 0,
                }))
                .ok()?;
            if info.crtc() != xcb::randr::Crtc::none() {
                return Some(info.crtc());
            }
        }
        None
    }
}

impl OutputOps for XcbOutputOps {
    fn enumerate_outputs(&self) -> Vec<OutputInfo> {
        self.get_cached_or_query()
    }

    fn screen_info(&self) -> ScreenInfo {
        ScreenInfo {
            width: self.width,
            height: self.height,
        }
    }

    fn output_at(&self, x: i32, y: i32) -> Option<OutputId> {
        let outputs = self.get_cached_or_query();
        output_at(&outputs, x, y)
    }

    fn invalidate_output_cache(&self) {
        if let Ok(mut cache) = self.cached_outputs.lock() {
            *cache = None;
        }
    }

    fn set_gamma_ramp(
        &self,
        output: OutputId,
        red: &[u16],
        green: &[u16],
        blue: &[u16],
    ) -> Result<(), BackendError> {
        if let Some(crtc) = self.output_to_crtc(output.0 as u32) {
            self.conn
                .send_and_check_request(&xcb::randr::SetCrtcGamma {
                    crtc,
                    red,
                    green,
                    blue,
                })
                .map_err(xcb_err)?;
            self.conn.flush().map_err(xcb_err)?;
        }
        Ok(())
    }

    fn get_gamma_ramp(&self, output: OutputId) -> Option<(Vec<u16>, Vec<u16>, Vec<u16>)> {
        let crtc = self.output_to_crtc(output.0 as u32)?;
        let reply = self
            .conn
            .wait_for_reply(self.conn.send_request(&xcb::randr::GetCrtcGamma { crtc }))
            .ok()?;
        Some((
            reply.red().to_vec(),
            reply.green().to_vec(),
            reply.blue().to_vec(),
        ))
    }
}

struct XcbKeyOps {
    conn: Arc<xcb::Connection>,
    ids: XcbIdRegistry,
    min_keycode: u8,
    max_keycode: u8,
    keymap: RwLock<Option<XcbKeymap>>,
    numlock_mask: RwLock<Option<x::ModMask>>,
}

impl XcbKeyOps {
    fn new(
        conn: Arc<xcb::Connection>,
        ids: XcbIdRegistry,
        min_keycode: u8,
        max_keycode: u8,
    ) -> Self {
        Self {
            conn,
            ids,
            min_keycode,
            max_keycode,
            keymap: RwLock::new(None),
            numlock_mask: RwLock::new(None),
        }
    }

    fn load_keymap(&self) -> XcbResult<XcbKeymap> {
        if let Some(map) = self.keymap.read().unwrap().clone() {
            return Ok(map);
        }

        let count = self
            .max_keycode
            .saturating_sub(self.min_keycode)
            .saturating_add(1);
        let cookie = self.conn.send_request(&x::GetKeyboardMapping {
            first_keycode: self.min_keycode,
            count,
        });
        let reply = self.conn.wait_for_reply(cookie).map_err(xcb_err)?;
        let per = reply.keysyms_per_keycode() as usize;
        let mut keysym_to_keycodes: HashMap<KeySym, Vec<u8>> = HashMap::new();
        let mut keycode_to_keysym: HashMap<u8, KeySym> = HashMap::new();

        for (idx, symbols) in reply.keysyms().chunks(per).enumerate() {
            let keycode = self.min_keycode.saturating_add(idx as u8);
            if let Some(&primary) = symbols.first().filter(|&&sym| sym != 0) {
                keycode_to_keysym.insert(keycode, primary);
            }
            for &sym in symbols.iter().filter(|&&sym| sym != 0) {
                keysym_to_keycodes.entry(sym).or_default().push(keycode);
            }
        }

        let map = XcbKeymap {
            keysym_to_keycodes,
            keycode_to_keysym,
        };
        *self.keymap.write().unwrap() = Some(map.clone());
        Ok(map)
    }

    fn numlock_mask(&self) -> x::ModMask {
        if let Some(mask) = *self.numlock_mask.read().unwrap() {
            return mask;
        }

        let mask = self.detect_numlock_mask().unwrap_or(x::ModMask::empty());
        *self.numlock_mask.write().unwrap() = Some(mask);
        mask
    }

    fn detect_numlock_mask(&self) -> XcbResult<x::ModMask> {
        let keymap = self.load_keymap()?;
        let numlock_keys = keymap
            .keysym_to_keycodes
            .get(&0xff7f)
            .cloned()
            .unwrap_or_default();
        if numlock_keys.is_empty() {
            return Ok(x::ModMask::empty());
        }

        let cookie = self.conn.send_request(&x::GetModifierMapping {});
        let reply = self.conn.wait_for_reply(cookie).map_err(xcb_err)?;
        let per = reply.keycodes_per_modifier() as usize;
        for (idx, chunk) in reply.keycodes().chunks(per).enumerate() {
            if chunk
                .iter()
                .filter(|&&code| code != 0)
                .any(|code| numlock_keys.contains(code))
            {
                return Ok(match idx {
                    3 => x::ModMask::N1,
                    4 => x::ModMask::N2,
                    5 => x::ModMask::N3,
                    6 => x::ModMask::N4,
                    7 => x::ModMask::N5,
                    _ => x::ModMask::N2,
                });
            }
        }

        Ok(x::ModMask::empty())
    }
}

#[derive(Clone)]
struct XcbKeymap {
    keysym_to_keycodes: HashMap<KeySym, Vec<u8>>,
    keycode_to_keysym: HashMap<u8, KeySym>,
}

impl KeyOps for XcbKeyOps {
    fn grab_keys(&self, root: WindowId, bindings: &[(Mods, KeySym)]) -> XcbResult<()> {
        let root = self.ids.window(root)?;
        let keymap = self.load_keymap()?;
        let numlock = self.numlock_mask();
        for &(mods, keysym) in bindings {
            let keycodes = keymap
                .keycode_to_keysym
                .iter()
                .filter_map(|(&keycode, &primary)| (primary == keysym).then_some(keycode))
                .collect::<Vec<_>>();
            if keycodes.is_empty() {
                log::warn!("XCB: no keycode found for keysym 0x{keysym:x}");
                continue;
            }
            for key in keycodes {
                let base = mods_to_xcb(mods);
                let combos = lock_modifier_combinations(base, x::ModMask::LOCK, numlock);
                for modifiers in combos {
                    if let Err(err) = self.conn.send_and_check_request(&x::GrabKey {
                        owner_events: false,
                        grab_window: root,
                        modifiers,
                        key,
                        pointer_mode: x::GrabMode::Async,
                        keyboard_mode: x::GrabMode::Async,
                    }) {
                        log::warn!(
                            "XCB grab_key failed (keysym=0x{:x}, keycode={}, mods=0x{:x}): {:?}",
                            keysym,
                            key,
                            modifiers.bits(),
                            err
                        );
                    }
                }
            }
        }
        self.conn.flush().map_err(xcb_err)?;
        Ok(())
    }

    fn clear_key_grabs(&self, root: WindowId) -> XcbResult<()> {
        self.conn
            .send_and_check_request(&x::UngrabKey {
                key: 0,
                grab_window: self.ids.window(root)?,
                modifiers: x::ModMask::ANY,
            })
            .map_err(xcb_err)
    }

    fn grab_keyboard(&self, root: WindowId) -> XcbResult<()> {
        let cookie = self.conn.send_request(&x::GrabKeyboard {
            owner_events: false,
            grab_window: self.ids.window(root)?,
            time: x::CURRENT_TIME,
            pointer_mode: x::GrabMode::Async,
            keyboard_mode: x::GrabMode::Async,
        });
        let _ = self.conn.wait_for_reply(cookie).map_err(xcb_err)?;
        self.conn.flush().map_err(xcb_err)?;
        Ok(())
    }

    fn ungrab_keyboard(&self) -> XcbResult<()> {
        self.conn
            .send_and_check_request(&x::UngrabKeyboard {
                time: x::CURRENT_TIME,
            })
            .map_err(xcb_err)?;
        self.conn.flush().map_err(xcb_err)
    }

    fn clean_mods(&self, raw_state: u16) -> Mods {
        mods_from_bits_with_numlock(raw_state, self.numlock_mask())
    }

    fn keysym_from_keycode(&mut self, keycode: u8) -> XcbResult<KeySym> {
        let keymap = self.load_keymap()?;
        Ok(keymap.keycode_to_keysym.get(&keycode).copied().unwrap_or(0))
    }

    fn clear_cache(&mut self) {
        *self.keymap.write().unwrap() = None;
        *self.numlock_mask.write().unwrap() = None;
    }
}

struct XcbEwmh {
    conn: Arc<xcb::Connection>,
    ids: XcbIdRegistry,
    atoms: XcbAtoms,
    root: x::Window,
}

impl XcbEwmh {
    fn new(
        conn: Arc<xcb::Connection>,
        ids: XcbIdRegistry,
        atoms: XcbAtoms,
        root: x::Window,
    ) -> Self {
        Self {
            conn,
            ids,
            atoms,
            root,
        }
    }
}

impl EwmhFacade for XcbEwmh {
    fn set_active_window(&self, win: WindowId) -> XcbResult<()> {
        change_u32s(
            &self.conn,
            self.root,
            self.atoms.net_active_window,
            self.atoms.window,
            &[self.ids.window(win)?.resource_id()],
        )
    }

    fn clear_active_window(&self) -> XcbResult<()> {
        self.conn
            .send_and_check_request(&x::DeleteProperty {
                window: self.root,
                property: self.atoms.net_active_window,
            })
            .map_err(xcb_err)
    }

    fn set_client_list(&self, list: &[WindowId]) -> XcbResult<()> {
        let raw = list
            .iter()
            .filter_map(|w| self.ids.window(*w).ok())
            .map(|w| w.resource_id())
            .collect::<Vec<_>>();
        change_u32s(
            &self.conn,
            self.root,
            self.atoms.net_client_list,
            self.atoms.window,
            &raw,
        )
    }

    fn set_client_list_stacking(&self, list: &[WindowId]) -> XcbResult<()> {
        let raw = list
            .iter()
            .filter_map(|w| self.ids.window(*w).ok())
            .map(|w| w.resource_id())
            .collect::<Vec<_>>();
        change_u32s(
            &self.conn,
            self.root,
            self.atoms.net_client_list_stacking,
            self.atoms.window,
            &raw,
        )
    }

    fn setup_supporting_wm_check(&self, wm_name: &str) -> XcbResult<WindowId> {
        let win: x::Window = self.conn.generate_id();
        self.conn
            .send_and_check_request(&x::CreateWindow {
                depth: x::COPY_FROM_PARENT as u8,
                wid: win,
                parent: self.root,
                x: 0,
                y: 0,
                width: 1,
                height: 1,
                border_width: 0,
                class: x::WindowClass::InputOutput,
                visual: x::COPY_FROM_PARENT,
                value_list: &[
                    x::Cw::OverrideRedirect(true),
                    x::Cw::EventMask(x::EventMask::EXPOSURE | x::EventMask::KEY_PRESS),
                ],
            })
            .map_err(xcb_err)?;
        change_u32s(
            &self.conn,
            self.root,
            self.atoms.net_supporting_wm_check,
            self.atoms.window,
            &[win.resource_id()],
        )?;
        change_u32s(
            &self.conn,
            win,
            self.atoms.net_supporting_wm_check,
            self.atoms.window,
            &[win.resource_id()],
        )?;
        change_bytes(
            &self.conn,
            win,
            self.atoms.net_wm_name,
            self.atoms.utf8_string,
            wm_name.as_bytes(),
        )?;
        change_bytes(
            &self.conn,
            win,
            x::ATOM_WM_NAME,
            self.atoms.string,
            wm_name.as_bytes(),
        )?;
        Ok(self.ids.intern(win))
    }

    fn declare_supported(&self, features: &[EwmhFeature]) -> XcbResult<()> {
        let raw = features
            .iter()
            .filter_map(|f| feature_atom(self.atoms, *f))
            .map(|a| a.resource_id())
            .collect::<Vec<_>>();
        change_u32s(
            &self.conn,
            self.root,
            self.atoms.net_supported,
            self.atoms.atom,
            &raw,
        )
    }

    fn reset_root_properties(&self) -> XcbResult<()> {
        for property in [
            self.atoms.net_active_window,
            self.atoms.net_client_list,
            self.atoms.net_supported,
            self.atoms.net_client_list_stacking,
            self.atoms.net_supporting_wm_check,
            self.atoms.net_current_desktop,
            self.atoms.net_number_of_desktops,
            self.atoms.net_desktop_names,
            self.atoms.net_desktop_viewport,
        ] {
            let _ = self.conn.send_and_check_request(&x::DeleteProperty {
                window: self.root,
                property,
            });
        }
        Ok(())
    }

    fn set_desktop_info(&self, current: u32, total: u32, names: &[&str]) -> XcbResult<()> {
        change_u32s(
            &self.conn,
            self.root,
            self.atoms.net_current_desktop,
            self.atoms.cardinal,
            &[current],
        )?;
        change_u32s(
            &self.conn,
            self.root,
            self.atoms.net_number_of_desktops,
            self.atoms.cardinal,
            &[total],
        )?;
        let mut name_bytes = Vec::new();
        for name in names {
            name_bytes.extend_from_slice(name.as_bytes());
            name_bytes.push(0);
        }
        change_bytes(
            &self.conn,
            self.root,
            self.atoms.net_desktop_names,
            self.atoms.utf8_string,
            &name_bytes,
        )?;
        let viewports = vec![0; total as usize * 2];
        change_u32s(
            &self.conn,
            self.root,
            self.atoms.net_desktop_viewport,
            self.atoms.cardinal,
            &viewports,
        )
    }

    fn set_workarea(&self, areas: &[(i32, i32, u32, u32)]) -> XcbResult<()> {
        let raw = areas
            .iter()
            .flat_map(|(x, y, w, h)| [*x as u32, *y as u32, *w, *h])
            .collect::<Vec<_>>();
        change_u32s(
            &self.conn,
            self.root,
            self.atoms.net_workarea,
            self.atoms.cardinal,
            &raw,
        )
    }
}

struct XcbColorAllocator {
    conn: Arc<xcb::Connection>,
    colormap: x::Colormap,
    cache: HashMap<u32, Pixel>,
    schemes: HashMap<SchemeType, ColorScheme>,
}

impl XcbColorAllocator {
    fn new(conn: Arc<xcb::Connection>, colormap: x::Colormap) -> Self {
        Self {
            conn,
            colormap,
            cache: HashMap::new(),
            schemes: HashMap::new(),
        }
    }

    fn ensure_pixel(&mut self, color: ArgbColor) -> XcbResult<Pixel> {
        if let Some(p) = self.cache.get(&color.value).copied() {
            return Ok(p);
        }
        let (_, r, g, b) = color.components();
        let cookie = self.conn.send_request(&x::AllocColor {
            cmap: self.colormap,
            red: (r as u16) << 8,
            green: (g as u16) << 8,
            blue: (b as u16) << 8,
        });
        let pixel = Pixel(self.conn.wait_for_reply(cookie).map_err(xcb_err)?.pixel());
        self.cache.insert(color.value, pixel);
        Ok(pixel)
    }
}

impl ColorAllocator for XcbColorAllocator {
    fn set_scheme(&mut self, t: SchemeType, s: ColorScheme) {
        self.schemes.insert(t, s);
    }

    fn allocate_schemes_pixels(&mut self) -> XcbResult<()> {
        let colors: Vec<_> = self
            .schemes
            .values()
            .flat_map(|s| [s.fg, s.bg, s.border])
            .collect();
        for color in colors {
            let _ = self.ensure_pixel(color)?;
        }
        Ok(())
    }

    fn get_border_pixel_of(&mut self, t: SchemeType) -> XcbResult<Pixel> {
        let scheme = self
            .schemes
            .get(&t)
            .ok_or(BackendError::NotFound("scheme not found"))?
            .clone();
        self.ensure_pixel(scheme.border)
    }

    fn free_all_theme_pixels(&mut self) -> XcbResult<()> {
        if self.cache.is_empty() {
            return Ok(());
        }
        let pixels: Vec<u32> = self.cache.values().map(|p| p.0).collect();
        self.conn
            .send_and_check_request(&x::FreeColors {
                cmap: self.colormap,
                plane_mask: 0,
                pixels: &pixels,
            })
            .map_err(xcb_err)?;
        self.cache.clear();
        Ok(())
    }
}

struct XcbCursorProvider {
    conn: Arc<xcb::Connection>,
    ids: XcbIdRegistry,
    font: x::Font,
    cache: HashMap<StdCursorKind, x::Cursor>,
}

impl XcbCursorProvider {
    fn new(conn: Arc<xcb::Connection>, ids: XcbIdRegistry) -> XcbResult<Self> {
        let font: x::Font = conn.generate_id();
        conn.send_and_check_request(&x::OpenFont {
            fid: font,
            name: b"cursor",
        })
        .map_err(xcb_err)?;
        Ok(Self {
            conn,
            ids,
            font,
            cache: HashMap::new(),
        })
    }

    fn glyph(kind: StdCursorKind) -> u16 {
        match kind {
            StdCursorKind::LeftPtr => 68,
            StdCursorKind::Hand => 58,
            StdCursorKind::XTerm => 152,
            StdCursorKind::Watch => 150,
            StdCursorKind::Crosshair => 34,
            StdCursorKind::Fleur => 52,
            StdCursorKind::HDoubleArrow => 108,
            StdCursorKind::VDoubleArrow => 116,
            StdCursorKind::TopLeftCorner => 134,
            StdCursorKind::TopRightCorner => 136,
            StdCursorKind::BottomLeftCorner => 12,
            StdCursorKind::BottomRightCorner => 14,
            StdCursorKind::Sizing => 120,
        }
    }
}

impl CursorProvider for XcbCursorProvider {
    fn preload_common(&mut self) -> XcbResult<()> {
        for kind in [
            StdCursorKind::LeftPtr,
            StdCursorKind::Hand,
            StdCursorKind::XTerm,
            StdCursorKind::Watch,
            StdCursorKind::Crosshair,
            StdCursorKind::Fleur,
            StdCursorKind::HDoubleArrow,
            StdCursorKind::VDoubleArrow,
            StdCursorKind::TopLeftCorner,
            StdCursorKind::TopRightCorner,
            StdCursorKind::BottomLeftCorner,
            StdCursorKind::BottomRightCorner,
            StdCursorKind::Sizing,
        ] {
            let _ = self.get(kind)?;
        }
        Ok(())
    }

    fn get(&mut self, kind: StdCursorKind) -> XcbResult<CursorHandle> {
        if let Some(c) = self.cache.get(&kind).copied() {
            return Ok(CursorHandle(c.resource_id() as u64));
        }
        let cursor: x::Cursor = self.conn.generate_id();
        let glyph = Self::glyph(kind);
        self.conn
            .send_and_check_request(&x::CreateGlyphCursor {
                cid: cursor,
                source_font: self.font,
                mask_font: self.font,
                source_char: glyph,
                mask_char: glyph + 1,
                fore_red: 0,
                fore_green: 0,
                fore_blue: 0,
                back_red: 65535,
                back_green: 65535,
                back_blue: 65535,
            })
            .map_err(xcb_err)?;
        self.cache.insert(kind, cursor);
        Ok(CursorHandle(cursor.resource_id() as u64))
    }

    fn apply(&mut self, window_id: WindowId, kind: StdCursorKind) -> XcbResult<()> {
        let cursor = x::Cursor::new(self.get(kind)?.0 as u32);
        self.conn
            .send_and_check_request(&x::ChangeWindowAttributes {
                window: self.ids.window(window_id)?,
                value_list: &[x::Cw::Cursor(cursor)],
            })
            .map_err(xcb_err)
    }

    fn cleanup(&mut self) -> XcbResult<()> {
        for cursor in self.cache.values().copied() {
            let _ = self.conn.send_and_check_request(&x::FreeCursor { cursor });
        }
        let _ = self
            .conn
            .send_and_check_request(&x::CloseFont { font: self.font });
        Ok(())
    }
}

fn event_mask_from_bits(bits: u32) -> x::EventMask {
    let mut mask = x::EventMask::empty();
    if bits & EventMaskBits::BUTTON_PRESS.bits() != 0 {
        mask |= x::EventMask::BUTTON_PRESS;
    }
    if bits & EventMaskBits::BUTTON_RELEASE.bits() != 0 {
        mask |= x::EventMask::BUTTON_RELEASE;
    }
    if bits & EventMaskBits::POINTER_MOTION.bits() != 0 {
        mask |= x::EventMask::POINTER_MOTION;
    }
    if bits & EventMaskBits::ENTER_WINDOW.bits() != 0 {
        mask |= x::EventMask::ENTER_WINDOW;
    }
    if bits & EventMaskBits::LEAVE_WINDOW.bits() != 0 {
        mask |= x::EventMask::LEAVE_WINDOW;
    }
    if bits & EventMaskBits::PROPERTY_CHANGE.bits() != 0 {
        mask |= x::EventMask::PROPERTY_CHANGE;
    }
    if bits & EventMaskBits::STRUCTURE_NOTIFY.bits() != 0 {
        mask |= x::EventMask::STRUCTURE_NOTIFY;
    }
    if bits & EventMaskBits::SUBSTRUCTURE_REDIRECT.bits() != 0 {
        mask |= x::EventMask::SUBSTRUCTURE_REDIRECT;
    }
    if bits & EventMaskBits::SUBSTRUCTURE_NOTIFY.bits() != 0 {
        mask |= x::EventMask::SUBSTRUCTURE_NOTIFY;
    }
    if bits & EventMaskBits::FOCUS_CHANGE.bits() != 0 {
        mask |= x::EventMask::FOCUS_CHANGE;
    }
    if bits & EventMaskBits::KEY_RELEASE.bits() != 0 {
        mask |= x::EventMask::KEY_RELEASE;
    }
    mask
}

fn mods_to_xcb(mods: Mods) -> x::ModMask {
    let mut mask = x::ModMask::empty();
    if mods.contains(Mods::SHIFT) {
        mask |= x::ModMask::SHIFT;
    }
    if mods.contains(Mods::CONTROL) {
        mask |= x::ModMask::CONTROL;
    }
    if mods.contains(Mods::ALT) {
        mask |= x::ModMask::N1;
    }
    if mods.contains(Mods::MOD2) || mods.contains(Mods::NUMLOCK) {
        mask |= x::ModMask::N2;
    }
    if mods.contains(Mods::MOD3) {
        mask |= x::ModMask::N3;
    }
    if mods.contains(Mods::SUPER) {
        mask |= x::ModMask::N4;
    }
    if mods.contains(Mods::MOD5) {
        mask |= x::ModMask::N5;
    }
    if mods.contains(Mods::CAPS) {
        mask |= x::ModMask::LOCK;
    }
    mask
}

fn mods_from_bits_with_numlock(bits: u16, numlock_mask: x::ModMask) -> Mods {
    let bits = bits as u32;
    let mut mods = Mods::empty();
    if bits & x::ModMask::SHIFT.bits() != 0 {
        mods |= Mods::SHIFT;
    }
    if bits & x::ModMask::CONTROL.bits() != 0 {
        mods |= Mods::CONTROL;
    }
    if bits & x::ModMask::N1.bits() != 0 {
        mods |= Mods::ALT;
    }
    if bits & x::ModMask::N2.bits() != 0 && !numlock_mask.contains(x::ModMask::N2) {
        mods |= Mods::MOD2;
    }
    if bits & x::ModMask::N3.bits() != 0 && !numlock_mask.contains(x::ModMask::N3) {
        mods |= Mods::MOD3;
    }
    if bits & x::ModMask::N4.bits() != 0 {
        mods |= Mods::SUPER;
    }
    if bits & x::ModMask::N5.bits() != 0 && !numlock_mask.contains(x::ModMask::N5) {
        mods |= Mods::MOD5;
    }
    if bits & x::ModMask::LOCK.bits() != 0 {
        mods |= Mods::CAPS;
    }
    if bits & numlock_mask.bits() != 0 {
        mods |= Mods::NUMLOCK;
    }
    mods
}

fn button_index(btn: u8) -> x::ButtonIndex {
    match btn {
        1 => x::ButtonIndex::N1,
        2 => x::ButtonIndex::N2,
        3 => x::ButtonIndex::N3,
        4 => x::ButtonIndex::N4,
        5 => x::ButtonIndex::N5,
        _ => x::ButtonIndex::Any,
    }
}

fn stack_mode_to_xcb(mode: StackMode) -> x::StackMode {
    match stack_mode_to_index(mode) {
        0 => x::StackMode::Above,
        1 => x::StackMode::Below,
        2 => x::StackMode::TopIf,
        3 => x::StackMode::BottomIf,
        4 => x::StackMode::Opposite,
        _ => x::StackMode::Above,
    }
}

fn stack_mode_from_xcb(mode: x::StackMode) -> StackMode {
    stack_mode_from_index(match mode {
        x::StackMode::Above => 0,
        x::StackMode::Below => 1,
        x::StackMode::TopIf => 2,
        x::StackMode::BottomIf => 3,
        x::StackMode::Opposite => 4,
    })
    .unwrap_or(StackMode::Above)
}

fn notify_mode_from_xcb(mode: x::NotifyMode) -> NotifyMode {
    match mode {
        x::NotifyMode::Grab => NotifyMode::Grab,
        x::NotifyMode::Ungrab => NotifyMode::Ungrab,
        _ => NotifyMode::Normal,
    }
}

fn get_atoms(
    conn: &xcb::Connection,
    window: x::Window,
    property: x::Atom,
    ty: x::Atom,
) -> Vec<x::Atom> {
    get_u32s(conn, window, property, ty)
        .into_iter()
        .map(x::Atom::new)
        .collect()
}

fn get_windows(
    conn: &xcb::Connection,
    window: x::Window,
    property: x::Atom,
    ty: x::Atom,
) -> Vec<x::Window> {
    get_u32s(conn, window, property, ty)
        .into_iter()
        .map(x::Window::new)
        .collect()
}

const MAX_U32_PROPERTY_ITEMS: u32 = 4096;
const MAX_ICON_ITEMS_U32: u32 = 4 * 1024 * 1024;
const MAX_OPAQUE_REGION_ITEMS_U32: u32 = 1024 * 1024;
const MAX_TEXT_PROPERTY_BYTES: u32 = 256 * 1024;
const MAX_ATOM_LIST_ITEMS: u32 = 4096;
const MAX_PROTOCOL_ATOMS: u32 = 1024;
const MAX_WM_HINTS_ITEMS: u32 = 20;
const MAX_WM_NORMAL_HINTS_ITEMS: u32 = 18;
const MAX_WM_STATE_ITEMS: u32 = 2;
const MAX_STRUT_ITEMS: u32 = 4;
const MAX_STRUT_PARTIAL_ITEMS: u32 = 12;
const MAX_MOTIF_HINTS_ITEMS: u32 = 5;
const MAX_GTK_FRAME_EXTENTS_ITEMS: u32 = 4;
const MAX_SINGLE_U32_ITEMS: u32 = 1;

fn get_u32s(conn: &xcb::Connection, window: x::Window, property: x::Atom, ty: x::Atom) -> Vec<u32> {
    get_u32s_with_length(conn, window, property, ty, MAX_U32_PROPERTY_ITEMS)
}

fn get_atoms_with_length(
    conn: &xcb::Connection,
    window: x::Window,
    property: x::Atom,
    ty: x::Atom,
    long_length: u32,
) -> Vec<x::Atom> {
    get_u32s_with_length(conn, window, property, ty, long_length)
        .into_iter()
        .map(x::Atom::new)
        .collect()
}

fn get_u32s_with_length(
    conn: &xcb::Connection,
    window: x::Window,
    property: x::Atom,
    ty: x::Atom,
    long_length: u32,
) -> Vec<u32> {
    let cookie = conn.send_request(&x::GetProperty {
        delete: false,
        window,
        property,
        r#type: ty,
        long_offset: 0,
        long_length,
    });
    conn.wait_for_reply(cookie)
        .ok()
        .filter(|r| r.format() == 32)
        .map(|r| r.value::<u32>().to_vec())
        .unwrap_or_default()
}

fn change_u32s(
    conn: &xcb::Connection,
    window: x::Window,
    property: x::Atom,
    ty: x::Atom,
    data: &[u32],
) -> XcbResult<()> {
    conn.send_and_check_request(&x::ChangeProperty {
        mode: x::PropMode::Replace,
        window,
        property,
        r#type: ty,
        data,
    })
    .map_err(xcb_err)
}

fn change_bytes(
    conn: &xcb::Connection,
    window: x::Window,
    property: x::Atom,
    ty: x::Atom,
    data: &[u8],
) -> XcbResult<()> {
    conn.send_and_check_request(&x::ChangeProperty {
        mode: x::PropMode::Replace,
        window,
        property,
        r#type: ty,
        data,
    })
    .map_err(xcb_err)
}

fn supported_features() -> Vec<EwmhFeature> {
    SUPPORTED_EWMH_FEATURES.to_vec()
}

fn feature_atom(atoms: XcbAtoms, feature: EwmhFeature) -> Option<x::Atom> {
    Some(atom_for_ewmh_feature(feature, atoms.feature_atoms()))
}

fn xcb_err<E>(err: E) -> BackendError
where
    E: std::error::Error + Send + Sync + 'static,
{
    BackendError::Other(Box::new(err))
}

#[cfg(test)]
mod parity_tests {
    use std::collections::BTreeSet;

    const X11RB_BACKEND_SRC: &str = include_str!("../x11rb/backend.rs");
    const X11RB_MOD_SRC: &str = include_str!("../x11rb/mod.rs");
    const XCB_BACKEND_SRC: &str = include_str!("backend.rs");

    fn impl_body_after<'a>(src: &'a str, needle: &str) -> &'a str {
        let start = src
            .find(&needle)
            .unwrap_or_else(|| panic!("missing `{needle}`"));
        let body_start = src[start..]
            .find('{')
            .map(|idx| start + idx + 1)
            .unwrap_or_else(|| panic!("missing body for `{needle}`"));
        let mut depth = 1usize;
        let mut end = body_start;
        for (offset, ch) in src[body_start..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = body_start + offset;
                        break;
                    }
                }
                _ => {}
            }
        }
        &src[body_start..end]
    }

    fn method_names(body: &str) -> BTreeSet<String> {
        body.lines()
            .filter_map(|line| {
                let trimmed = line.trim_start();
                let rest = trimmed.strip_prefix("fn ")?;
                let name = rest
                    .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
                    .next()?;
                Some(name.to_string())
            })
            .collect()
    }

    fn x11rb_atom_names() -> BTreeSet<String> {
        let start = X11RB_MOD_SRC
            .find("pub Atoms: AtomsCookie")
            .expect("missing x11rb Atoms declaration");
        let body_start = X11RB_MOD_SRC[start..]
            .find('{')
            .map(|idx| start + idx + 1)
            .expect("missing x11rb Atoms body");
        let body_end = X11RB_MOD_SRC[body_start..]
            .find('}')
            .map(|idx| body_start + idx)
            .expect("missing x11rb Atoms body end");

        X11RB_MOD_SRC[body_start..body_end]
            .lines()
            .filter_map(|line| {
                let name = line.split("//").next()?.trim().trim_end_matches(',');
                (!name.is_empty()).then(|| name.to_string())
            })
            .collect()
    }

    fn xcb_atom_fields() -> BTreeSet<String> {
        let start = XCB_BACKEND_SRC
            .find("struct XcbAtoms")
            .expect("missing XcbAtoms");
        let body_start = XCB_BACKEND_SRC[start..]
            .find('{')
            .map(|idx| start + idx + 1)
            .expect("missing XcbAtoms body");
        let body_end = XCB_BACKEND_SRC[body_start..]
            .find('}')
            .map(|idx| body_start + idx)
            .expect("missing XcbAtoms body end");

        XCB_BACKEND_SRC[body_start..body_end]
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                let (name, ty) = line.split_once(':')?;
                ty.trim_start()
                    .starts_with("x::Atom")
                    .then(|| name.trim().to_string())
            })
            .collect()
    }

    fn x11rb_atom_to_xcb_field(atom: &str) -> String {
        atom.trim_start_matches('_').to_ascii_lowercase()
    }

    fn assert_same_methods(
        x11rb_needle: &str,
        xcb_needle: &str,
        label: &str,
        allow_xcb_extra: bool,
    ) {
        let x11rb = method_names(impl_body_after(X11RB_BACKEND_SRC, x11rb_needle));
        let xcb = method_names(impl_body_after(XCB_BACKEND_SRC, xcb_needle));

        let missing: Vec<_> = x11rb.difference(&xcb).cloned().collect();
        let extra: Vec<_> = xcb.difference(&x11rb).cloned().collect();

        assert!(
            missing.is_empty() && (allow_xcb_extra || extra.is_empty()),
            "{label} impl drift between x11rb and xcb; missing={missing:?}, extra={extra:?}"
        );
    }

    #[test]
    fn xcb_backend_overrides_match_x11rb_backend() {
        assert_same_methods(
            "impl Backend for X11rbBackend",
            "impl Backend for XcbBackend",
            "Backend",
            false,
        );
    }

    #[test]
    fn xcb_ops_trait_methods_match_x11rb() {
        for (label, x11rb_needle, xcb_needle) in [
            (
                "WindowOps",
                "WindowOps for X11WindowOps",
                "WindowOps for XcbWindowOps",
            ),
            (
                "InputOps",
                "InputOpsTrait for X11InputOps",
                "InputOps for XcbInputOps",
            ),
            (
                "PropertyOps",
                "PropertyOpsTrait for X11PropertyOps",
                "PropertyOps for XcbPropertyOps",
            ),
            (
                "OutputOps",
                "OutputOps for X11OutputOps",
                "OutputOps for XcbOutputOps",
            ),
            ("KeyOps", "KeyOps for X11KeyOps", "KeyOps for XcbKeyOps"),
            (
                "EwmhFacade",
                "EwmhFacade for X11EwmhFacade",
                "EwmhFacade for XcbEwmh",
            ),
            (
                "ColorAllocator",
                "ColorAllocator for X11ColorAllocator",
                "ColorAllocator for XcbColorAllocator",
            ),
            (
                "CursorProvider",
                "CursorProvider for X11CursorProvider",
                "CursorProvider for XcbCursorProvider",
            ),
        ] {
            assert_same_methods(x11rb_needle, xcb_needle, label, true);
        }
    }

    #[test]
    fn xcb_atoms_cover_x11rb_atoms() {
        let xcb = xcb_atom_fields();
        let missing: Vec<_> = x11rb_atom_names()
            .into_iter()
            .filter(|atom| !xcb.contains(&x11rb_atom_to_xcb_field(atom)))
            .collect();

        assert!(
            missing.is_empty(),
            "XcbAtoms does not cover x11rb Atoms: {missing:?}"
        );
    }

    #[test]
    fn xcb_normal_hints_use_wm_size_hints_type() {
        let start = XCB_BACKEND_SRC
            .find("fn fetch_normal_hints")
            .expect("missing fetch_normal_hints");
        let end = XCB_BACKEND_SRC[start..]
            .find("fn set_window_strut_top")
            .map(|idx| start + idx)
            .expect("missing function after fetch_normal_hints");
        let body = &XCB_BACKEND_SRC[start..end];

        assert!(
            body.contains("self.atoms.wm_normal_hints")
                && body.contains("self.atoms.wm_size_hints"),
            "WM_NORMAL_HINTS must be fetched with property=WM_NORMAL_HINTS and type=WM_SIZE_HINTS"
        );
    }
}
