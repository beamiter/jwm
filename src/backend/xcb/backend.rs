//! Native X11 backend implemented with the `xcb` crate.
//!
//! This backend owns a real `xcb::Connection` and implements the same JWM
//! backend traits as the x11rb backend.  It intentionally starts without the
//! GLX texture-from-pixmap compositor path; window management, EWMH, input and
//! event dispatch are handled directly through XCB.

use crate::backend::api::{
    AllowMode, Backend, BackendEvent, Capabilities, CloseResult, ColorAllocator, CursorProvider,
    EventHandler, EwmhFacade, EwmhFeature, Geometry, HitTarget, InputOps, KeyOps, LayerSurfaceInfo,
    NetWmAction, NetWmState, NormalHints, NotifyMode, OutputInfo, OutputOps, PropertyKind,
    PropertyOps, ResizeEdge, ScreenInfo, StackMode, StrutPartial, WindowAttributes, WindowChanges,
    WindowOps, WindowType, WmHints,
};
use crate::backend::common_define::{
    ArgbColor, ColorScheme, CursorHandle, EventMaskBits, KeySym, Mods, OutputId, Pixel, SchemeType,
    StdCursorKind, WindowId,
};
use crate::backend::error::BackendError;
use crate::jwm::InteractionAction;
use calloop::signals::{Signal, Signals};
use calloop::{
    EventLoop,
    timer::{TimeoutAction, Timer},
};
use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;
use xcb::{Xid, XidNew, x};

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
    net_wm_user_time: x::Atom,
    net_wm_icon: x::Atom,
    net_wm_bypass_compositor: x::Atom,
    net_wm_opaque_region: x::Atom,
    net_wm_strut: x::Atom,
    net_wm_strut_partial: x::Atom,
    net_wm_moveresize: x::Atom,
    net_frame_extents: x::Atom,
    net_wm_allowed_actions: x::Atom,
    net_workarea: x::Atom,
    motif_wm_hints: x::Atom,
    gtk_frame_extents: x::Atom,
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
            net_wm_user_time: Self::intern(conn, b"_NET_WM_USER_TIME")?,
            net_wm_icon: Self::intern(conn, b"_NET_WM_ICON")?,
            net_wm_bypass_compositor: Self::intern(conn, b"_NET_WM_BYPASS_COMPOSITOR")?,
            net_wm_opaque_region: Self::intern(conn, b"_NET_WM_OPAQUE_REGION")?,
            net_wm_strut: Self::intern(conn, b"_NET_WM_STRUT")?,
            net_wm_strut_partial: Self::intern(conn, b"_NET_WM_STRUT_PARTIAL")?,
            net_wm_moveresize: Self::intern(conn, b"_NET_WM_MOVERESIZE")?,
            net_frame_extents: Self::intern(conn, b"_NET_FRAME_EXTENTS")?,
            net_wm_allowed_actions: Self::intern(conn, b"_NET_WM_ALLOWED_ACTIONS")?,
            net_workarea: Self::intern(conn, b"_NET_WORKAREA")?,
            motif_wm_hints: Self::intern(conn, b"_MOTIF_WM_HINTS")?,
            gtk_frame_extents: Self::intern(conn, b"_GTK_FRAME_EXTENTS")?,
        })
    }

    fn atom_for_state(&self, state: NetWmState) -> x::Atom {
        match state {
            NetWmState::Fullscreen => self.net_wm_state_fullscreen,
            NetWmState::MaximizedVert => self.net_wm_state_maximized_vert,
            NetWmState::MaximizedHorz => self.net_wm_state_maximized_horz,
            NetWmState::Hidden => self.net_wm_state_hidden,
            NetWmState::Above => self.net_wm_state_above,
            NetWmState::Below => self.net_wm_state_below,
            NetWmState::DemandsAttention => self.net_wm_state_demands_attention,
            NetWmState::Sticky => self.net_wm_state_sticky,
            NetWmState::SkipTaskbar => self.net_wm_state_skip_taskbar,
            NetWmState::SkipPager => self.net_wm_state_skip_pager,
        }
    }

    fn state_from_atom(&self, atom: x::Atom) -> Option<NetWmState> {
        Some(match atom {
            a if a == self.net_wm_state_fullscreen => NetWmState::Fullscreen,
            a if a == self.net_wm_state_maximized_vert => NetWmState::MaximizedVert,
            a if a == self.net_wm_state_maximized_horz => NetWmState::MaximizedHorz,
            a if a == self.net_wm_state_hidden => NetWmState::Hidden,
            a if a == self.net_wm_state_above => NetWmState::Above,
            a if a == self.net_wm_state_below => NetWmState::Below,
            a if a == self.net_wm_state_demands_attention => NetWmState::DemandsAttention,
            a if a == self.net_wm_state_sticky => NetWmState::Sticky,
            a if a == self.net_wm_state_skip_taskbar => NetWmState::SkipTaskbar,
            a if a == self.net_wm_state_skip_pager => NetWmState::SkipPager,
            _ => return None,
        })
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

struct XcbLoopData<'a> {
    backend: &'a mut XcbBackend,
    handler: &'a mut dyn EventHandler,
    should_exit: bool,
}

impl XcbBackend {
    pub fn new() -> XcbResult<Self> {
        let (conn, screen_num) = xcb::Connection::connect(None).map_err(xcb_err)?;
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

        let window_ops = Box::new(XcbWindowOps::new(conn.clone(), ids.clone(), atoms));
        let input_ops = Box::new(XcbInputOps::new(conn.clone(), ids.clone(), root));
        let property_ops = Box::new(XcbPropertyOps::new(conn.clone(), ids.clone(), atoms));
        let output_ops = Box::new(XcbOutputOps::new(screen_width, screen_height));
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

        Ok(Self {
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
        })
    }

    fn map_event(&mut self, event: xcb::Event) -> Option<BackendEvent> {
        match event {
            xcb::Event::X(x::Event::MapRequest(ev)) => {
                Some(BackendEvent::WindowCreated(self.ids.intern(ev.window())))
            }
            xcb::Event::X(x::Event::MapNotify(ev)) => {
                Some(BackendEvent::WindowMapped(self.ids.intern(ev.window())))
            }
            xcb::Event::X(x::Event::UnmapNotify(ev)) => {
                Some(BackendEvent::WindowUnmapped(self.ids.intern(ev.window())))
            }
            xcb::Event::X(x::Event::DestroyNotify(ev)) => {
                let id = self.ids.intern(ev.window());
                self.ids.remove(ev.window());
                Some(BackendEvent::WindowDestroyed(id))
            }
            xcb::Event::X(x::Event::ConfigureRequest(ev)) => {
                let mut changes = WindowChanges::default();
                let mask = ev.value_mask();
                if mask.contains(x::ConfigWindowMask::X) {
                    changes.x = Some(ev.x() as i32);
                }
                if mask.contains(x::ConfigWindowMask::Y) {
                    changes.y = Some(ev.y() as i32);
                }
                if mask.contains(x::ConfigWindowMask::WIDTH) {
                    changes.width = Some(ev.width() as u32);
                }
                if mask.contains(x::ConfigWindowMask::HEIGHT) {
                    changes.height = Some(ev.height() as u32);
                }
                if mask.contains(x::ConfigWindowMask::BORDER_WIDTH) {
                    changes.border_width = Some(ev.border_width() as u32);
                }
                if mask.contains(x::ConfigWindowMask::SIBLING) {
                    changes.sibling = Some(self.ids.intern(ev.sibling()));
                }
                if mask.contains(x::ConfigWindowMask::STACK_MODE) {
                    changes.stack_mode = Some(stack_mode_from_xcb(ev.stack_mode()));
                }
                Some(BackendEvent::ConfigureRequest {
                    window: self.ids.intern(ev.window()),
                    mask_bits: ev.value_mask().bits() as u16,
                    changes,
                })
            }
            xcb::Event::X(x::Event::ConfigureNotify(ev)) => Some(BackendEvent::WindowConfigured {
                window: self.ids.intern(ev.window()),
                x: ev.x() as i32,
                y: ev.y() as i32,
                width: ev.width() as u32,
                height: ev.height() as u32,
            }),
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
            xcb::Event::X(x::Event::PropertyNotify(ev)) => Some(BackendEvent::PropertyChanged {
                window: self.ids.intern(ev.window()),
                kind: self.property_kind(ev.atom()),
            }),
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
            xcb::Event::X(x::Event::MappingNotify(_)) => Some(BackendEvent::MappingNotify),
            xcb::Event::X(x::Event::ClientMessage(ev)) => self.map_client_message(ev),
            _ => None,
        }
    }

    fn hit_target(&self, window: x::Window) -> HitTarget {
        if window == self.root {
            HitTarget::Background {
                output: Some(OutputId(1)),
            }
        } else {
            HitTarget::Surface(self.ids.intern(window))
        }
    }

    fn property_kind(&self, atom: x::Atom) -> PropertyKind {
        if atom == self.atoms.net_wm_name || atom == x::ATOM_WM_NAME {
            PropertyKind::Title
        } else if atom == self.atoms.wm_class {
            PropertyKind::Class
        } else if atom == self.atoms.wm_transient_for {
            PropertyKind::TransientFor
        } else if atom == self.atoms.net_wm_window_type {
            PropertyKind::WindowType
        } else if atom == self.atoms.wm_protocols {
            PropertyKind::Protocols
        } else if atom == self.atoms.net_wm_strut || atom == self.atoms.net_wm_strut_partial {
            PropertyKind::Strut
        } else if atom == self.atoms.motif_wm_hints {
            PropertyKind::MotifHints
        } else if atom == self.atoms.gtk_frame_extents {
            PropertyKind::GtkFrameExtents
        } else if atom == self.atoms.net_wm_bypass_compositor {
            PropertyKind::BypassCompositor
        } else if atom == self.atoms.net_wm_opaque_region {
            PropertyKind::OpaqueRegion
        } else if atom == self.atoms.net_wm_icon {
            PropertyKind::NetWmIcon
        } else if atom == self.atoms.net_wm_user_time {
            PropertyKind::UserTime
        } else {
            PropertyKind::Other
        }
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

        if ev.r#type() == self.atoms.net_wm_state {
            let action = match data[0] {
                0 => NetWmAction::Remove,
                1 => NetWmAction::Add,
                2 => NetWmAction::Toggle,
                _ => NetWmAction::Toggle,
            };
            if let Some(state) = self.atoms.state_from_atom(x::Atom::new(data[1])) {
                if let Some(second) = self.atoms.state_from_atom(x::Atom::new(data[2])) {
                    self.pending.push_back(BackendEvent::WindowStateRequest {
                        window,
                        state: second,
                        action,
                    });
                }
                return Some(BackendEvent::WindowStateRequest {
                    window,
                    state,
                    action,
                });
            }
        }
        if ev.r#type() == self.atoms.net_active_window {
            return Some(BackendEvent::ActiveWindowMessage { window });
        }
        if ev.r#type() == self.atoms.net_close_window {
            return Some(BackendEvent::CloseWindowRequest { window });
        }
        if ev.r#type() == self.atoms.net_wm_moveresize {
            return Some(BackendEvent::MoveResizeRequest {
                window,
                direction: data[2],
                button: data[3],
            });
        }
        if ev.r#type() == self.atoms.wm_protocols && data[0] == self.atoms.net_wm_ping.resource_id()
        {
            return Some(BackendEvent::PingResponse {
                window: self.ids.intern(x::Window::new(data[2])),
            });
        }
        Some(BackendEvent::ClientMessage {
            window,
            type_: ev.r#type().resource_id(),
            data,
            format: ev.format(),
        })
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
            .change_event_mask(self.root_id, root_event_mask().bits())
            .map_err(|e| {
                BackendError::Message(format!("Another window manager is already running: {e}"))
            })
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
        self.cursor_provider.cleanup()?;
        self.color_allocator.free_all_theme_pixels()?;
        self.conn.flush().map_err(xcb_err)
    }

    fn on_focused_client_changed(&mut self, win: Option<WindowId>) -> XcbResult<()> {
        if let Some(ewmh) = self.ewmh.as_ref() {
            match win {
                Some(w) => ewmh.set_active_window(w)?,
                None => ewmh.clear_active_window()?,
            }
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

    fn begin_move(&mut self, win: WindowId) -> XcbResult<()> {
        let geom = self.window_ops.get_geometry(win)?;
        let (rx, ry, _, _) = self.input_ops.query_pointer_root()?;
        let mask = (EventMaskBits::BUTTON_RELEASE | EventMaskBits::POINTER_MOTION).bits();
        if self.input_ops.grab_pointer(mask, None)? {
            self.interaction = Some(XcbInteraction {
                win,
                start_geom: geom,
                start_root_x: rx as f64,
                start_root_y: ry as f64,
                action: InteractionAction::Move,
                current_x: geom.x,
                current_y: geom.y,
                current_w: geom.w,
                current_h: geom.h,
            });
        }
        Ok(())
    }

    fn begin_resize(&mut self, win: WindowId, edge: ResizeEdge) -> XcbResult<()> {
        let geom = self.window_ops.get_geometry(win)?;
        let (rx, ry, _, _) = self.input_ops.query_pointer_root()?;
        let mask = (EventMaskBits::BUTTON_RELEASE | EventMaskBits::POINTER_MOTION).bits();
        if self.input_ops.grab_pointer(mask, None)? {
            self.interaction = Some(XcbInteraction {
                win,
                start_geom: geom,
                start_root_x: rx as f64,
                start_root_y: ry as f64,
                action: InteractionAction::Resize(edge),
                current_x: geom.x,
                current_y: geom.y,
                current_w: geom.w,
                current_h: geom.h,
            });
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
                    self.window_ops
                        .set_position(state.win, state.current_x, state.current_y)?;
                }
                InteractionAction::Resize(_) => {
                    state.current_w = (state.start_geom.w as i32 + dx).max(1) as u32;
                    state.current_h = (state.start_geom.h as i32 + dy).max(1) as u32;
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
        if self.interaction.take().is_some() {
            self.input_ops.ungrab_pointer()?;
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
                    loop {
                        let event = match data.backend.conn.poll_for_event() {
                            Ok(Some(event)) => event,
                            Ok(None) => break,
                            Err(err) => {
                                log::error!("XCB event error: {err}");
                                break;
                            }
                        };
                        if let Some(mapped) = data.backend.map_event(event) {
                            if let Err(err) = data.handler.handle_event(data.backend, mapped) {
                                log::error!("Error handling XCB event: {err:?}");
                            }
                        }
                        while let Some(mapped) = data.backend.pending.pop_front() {
                            if let Err(err) = data.handler.handle_event(data.backend, mapped) {
                                log::error!("Error handling queued XCB event: {err:?}");
                            }
                        }
                    }
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

        let mut data = XcbLoopData {
            backend: self,
            handler,
            should_exit: false,
        };
        while !data.should_exit {
            let timeout = if data.handler.needs_tick() {
                Some(Duration::from_millis(1))
            } else {
                None
            };
            event_loop.dispatch(timeout, &mut data)?;
        }
        Ok(())
    }
}

struct XcbWindowOps {
    conn: Arc<xcb::Connection>,
    ids: XcbIdRegistry,
    atoms: XcbAtoms,
}

impl XcbWindowOps {
    fn new(conn: Arc<xcb::Connection>, ids: XcbIdRegistry, atoms: XcbAtoms) -> Self {
        Self { conn, ids, atoms }
    }

    fn win(&self, win: WindowId) -> XcbResult<x::Window> {
        self.ids.window(win)
    }
}

impl WindowOps for XcbWindowOps {
    fn set_position(&self, win: WindowId, x: i32, y: i32) -> XcbResult<()> {
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
        )
    }

    fn set_decoration_style(
        &self,
        win: WindowId,
        border_width: u32,
        border_color: Pixel,
    ) -> XcbResult<()> {
        let w = self.win(win)?;
        self.conn
            .send_and_check_request(&x::ChangeWindowAttributes {
                window: w,
                value_list: &[x::Cw::BorderPixel(border_color.0)],
            })
            .map_err(xcb_err)?;
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
        let protocols = get_atoms(&self.conn, w, self.atoms.wm_protocols, self.atoms.atom);
        if protocols.contains(&self.atoms.wm_delete_window) {
            let event = x::ClientMessageEvent::new(
                w,
                self.atoms.wm_protocols,
                x::ClientMessageData::Data32([
                    self.atoms.wm_delete_window.resource_id(),
                    0,
                    0,
                    0,
                    0,
                ]),
            );
            self.conn
                .send_and_check_request(&x::SendEvent {
                    propagate: false,
                    destination: x::SendEventDest::Window(w),
                    event_mask: x::EventMask::NO_EVENT,
                    event: &event,
                })
                .map_err(xcb_err)?;
            Ok(CloseResult::Graceful)
        } else {
            self.kill_client(win)?;
            Ok(CloseResult::Forced)
        }
    }

    fn set_input_focus(&self, win: WindowId) -> XcbResult<()> {
        self.conn
            .send_and_check_request(&x::SetInputFocus {
                revert_to: x::InputFocus::PointerRoot,
                focus: self.win(win)?,
                time: x::CURRENT_TIME,
            })
            .map_err(xcb_err)
    }

    fn set_input_focus_root(&self) -> XcbResult<()> {
        Ok(())
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
        let root = self
            .ids
            .x_to_wid
            .read()
            .unwrap()
            .keys()
            .next()
            .copied()
            .map(x::Window::new)
            .ok_or(BackendError::NotFound("root window"))?;
        let cookie = self.conn.send_request(&x::QueryTree { window: root });
        let reply = self.conn.wait_for_reply(cookie).map_err(xcb_err)?;
        Ok(reply
            .children()
            .iter()
            .map(|&w| self.ids.intern(w))
            .collect())
    }

    fn flush(&self) -> XcbResult<()> {
        self.conn.flush().map_err(xcb_err)
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
        self.conn
            .send_and_check_request(&x::ConfigureWindow {
                window: self.win(win)?,
                value_list: &values,
            })
            .map_err(xcb_err)
    }

    fn ungrab_all_buttons(&self, win: WindowId) -> XcbResult<()> {
        self.conn
            .send_and_check_request(&x::UngrabButton {
                button: x::ButtonIndex::Any,
                grab_window: self.win(win)?,
                modifiers: x::ModMask::ANY,
            })
            .map_err(xcb_err)
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
            .map_err(xcb_err)
    }

    fn grab_button(&self, win: WindowId, btn: u8, mask: u32, mods: Mods) -> XcbResult<()> {
        self.conn
            .send_and_check_request(&x::GrabButton {
                owner_events: false,
                grab_window: self.win(win)?,
                event_mask: event_mask_from_bits(mask),
                pointer_mode: x::GrabMode::Async,
                keyboard_mode: x::GrabMode::Async,
                confine_to: x::WINDOW_NONE,
                cursor: x::CURSOR_NONE,
                button: button_index(btn),
                modifiers: mods_to_xcb(mods),
            })
            .map_err(xcb_err)
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
        let protocols = get_atoms(&self.conn, w, self.atoms.wm_protocols, self.atoms.atom);
        if !protocols.contains(&self.atoms.wm_take_focus) {
            return Ok(false);
        }
        let event = x::ClientMessageEvent::new(
            w,
            self.atoms.wm_protocols,
            x::ClientMessageData::Data32([
                self.atoms.wm_take_focus.resource_id(),
                x::CURRENT_TIME,
                0,
                0,
                0,
            ]),
        );
        self.conn
            .send_and_check_request(&x::SendEvent {
                propagate: false,
                destination: x::SendEventDest::Window(w),
                event_mask: x::EventMask::NO_EVENT,
                event: &event,
            })
            .map_err(xcb_err)?;
        Ok(true)
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
        let cookie = self.conn.send_request(&x::GrabPointer {
            owner_events: false,
            grab_window: self.root,
            event_mask: event_mask_from_bits(mask),
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

    fn get_string(&self, win: WindowId, property: x::Atom, ty: x::Atom) -> String {
        let Ok(w) = self.win(win) else {
            return String::new();
        };
        let cookie = self.conn.send_request(&x::GetProperty {
            delete: false,
            window: w,
            property,
            r#type: ty,
            long_offset: 0,
            long_length: 4096,
        });
        self.conn
            .wait_for_reply(cookie)
            .ok()
            .and_then(|r| String::from_utf8(r.value::<u8>().to_vec()).ok())
            .unwrap_or_default()
            .trim_end_matches('\0')
            .to_string()
    }

    fn states(&self, win: WindowId) -> Vec<x::Atom> {
        self.win(win)
            .ok()
            .map(|w| get_atoms(&self.conn, w, self.atoms.net_wm_state, self.atoms.atom))
            .unwrap_or_default()
    }
}

impl PropertyOps for XcbPropertyOps {
    fn get_title(&self, win: WindowId) -> String {
        let title = self.get_string(win, self.atoms.net_wm_name, self.atoms.utf8_string);
        if title.is_empty() {
            self.get_string(win, x::ATOM_WM_NAME, self.atoms.string)
        } else {
            title
        }
    }

    fn get_class(&self, win: WindowId) -> (String, String) {
        let raw = self.get_string(win, self.atoms.wm_class, self.atoms.string);
        let mut parts = raw.split('\0');
        (
            parts.next().unwrap_or_default().to_string(),
            parts.next().unwrap_or_default().to_string(),
        )
    }

    fn get_window_types(&self, win: WindowId) -> Vec<WindowType> {
        let Ok(w) = self.win(win) else {
            return vec![WindowType::Normal];
        };
        let atoms = get_atoms(
            &self.conn,
            w,
            self.atoms.net_wm_window_type,
            self.atoms.atom,
        );
        if atoms.is_empty() {
            return vec![WindowType::Normal];
        }
        atoms
            .into_iter()
            .map(|a| match a {
                a if a == self.atoms.net_wm_window_type_desktop => WindowType::Desktop,
                a if a == self.atoms.net_wm_window_type_dock => WindowType::Dock,
                a if a == self.atoms.net_wm_window_type_toolbar => WindowType::Toolbar,
                a if a == self.atoms.net_wm_window_type_menu => WindowType::Menu,
                a if a == self.atoms.net_wm_window_type_utility => WindowType::Utility,
                a if a == self.atoms.net_wm_window_type_splash => WindowType::Splash,
                a if a == self.atoms.net_wm_window_type_dialog => WindowType::Dialog,
                a if a == self.atoms.net_wm_window_type_dropdown_menu => WindowType::DropdownMenu,
                a if a == self.atoms.net_wm_window_type_popup_menu => WindowType::PopupMenu,
                a if a == self.atoms.net_wm_window_type_tooltip => WindowType::Tooltip,
                a if a == self.atoms.net_wm_window_type_notification => WindowType::Notification,
                a if a == self.atoms.net_wm_window_type_combo => WindowType::Combo,
                _ => WindowType::Unknown,
            })
            .collect()
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
        .map(|w| self.ids.intern(w))
    }

    fn get_wm_hints(&self, win: WindowId) -> Option<WmHints> {
        let w = self.win(win).ok()?;
        let values = get_u32s(&self.conn, w, self.atoms.wm_hints, self.atoms.wm_hints);
        if values.is_empty() {
            return None;
        }
        let flags = values[0];
        Some(WmHints {
            urgent: flags & (1 << 8) != 0,
            input: if flags & 1 != 0 {
                Some(values.get(1).copied().unwrap_or(0) != 0)
            } else {
                None
            },
        })
    }

    fn set_urgent_hint(&self, _win: WindowId, _urgent: bool) -> XcbResult<()> {
        Ok(())
    }

    fn fetch_normal_hints(&self, win: WindowId) -> XcbResult<Option<NormalHints>> {
        let w = self.win(win)?;
        let v = get_u32s(
            &self.conn,
            w,
            self.atoms.wm_normal_hints,
            self.atoms.wm_normal_hints,
        );
        if v.len() < 18 {
            return Ok(None);
        }
        Ok(Some(NormalHints {
            min_w: v[5] as i32,
            min_h: v[6] as i32,
            max_w: v[7] as i32,
            max_h: v[8] as i32,
            inc_w: v[9] as i32,
            inc_h: v[10] as i32,
            base_w: v.get(15).copied().unwrap_or(0) as i32,
            base_h: v.get(16).copied().unwrap_or(0) as i32,
            ..Default::default()
        }))
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
        Ok(
            get_u32s(&self.conn, w, self.atoms.wm_state, self.atoms.wm_state)
                .first()
                .copied()
                .unwrap_or(0) as i64,
        )
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
        let v = get_u32s(
            &self.conn,
            w,
            self.atoms.net_wm_strut_partial,
            self.atoms.cardinal,
        );
        if v.len() < 12 {
            return None;
        }
        Some(StrutPartial {
            left: v[0],
            right: v[1],
            top: v[2],
            bottom: v[3],
            left_start_y: v[4],
            left_end_y: v[5],
            right_start_y: v[6],
            right_end_y: v[7],
            top_start_x: v[8],
            top_end_x: v[9],
            bottom_start_x: v[10],
            bottom_end_x: v[11],
        })
    }

    fn get_layer_surface_info(&self, _win: WindowId) -> Option<LayerSurfaceInfo> {
        None
    }

    fn get_window_pid(&self, win: WindowId) -> Option<u32> {
        let w = self.win(win).ok()?;
        get_u32s(&self.conn, w, self.atoms.net_wm_pid, self.atoms.cardinal)
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

    fn set_allowed_actions(
        &self,
        win: WindowId,
        _actions: &[crate::backend::api::AllowedAction],
    ) -> XcbResult<()> {
        change_u32s(
            &self.conn,
            self.win(win)?,
            self.atoms.net_wm_allowed_actions,
            self.atoms.atom,
            &[],
        )
    }

    fn send_ping(&self, win: WindowId, timestamp: u32) -> XcbResult<bool> {
        let w = self.win(win)?;
        let event = x::ClientMessageEvent::new(
            w,
            self.atoms.wm_protocols,
            x::ClientMessageData::Data32([
                self.atoms.net_wm_ping.resource_id(),
                timestamp,
                w.resource_id(),
                0,
                0,
            ]),
        );
        self.conn
            .send_and_check_request(&x::SendEvent {
                propagate: false,
                destination: x::SendEventDest::Window(w),
                event_mask: x::EventMask::NO_EVENT,
                event: &event,
            })
            .map_err(xcb_err)?;
        Ok(true)
    }

    fn get_user_time(&self, win: WindowId) -> Option<u32> {
        let w = self.win(win).ok()?;
        get_u32s(
            &self.conn,
            w,
            self.atoms.net_wm_user_time,
            self.atoms.cardinal,
        )
        .first()
        .copied()
    }
}

struct XcbOutputOps {
    width: i32,
    height: i32,
}

impl XcbOutputOps {
    fn new(width: i32, height: i32) -> Self {
        Self { width, height }
    }
}

impl OutputOps for XcbOutputOps {
    fn enumerate_outputs(&self) -> Vec<OutputInfo> {
        vec![OutputInfo {
            id: OutputId(1),
            name: "XCB Screen".into(),
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
            scale: 1.0,
            refresh_rate: 60,
            hdr_capable: false,
            hdr_metadata: None,
        }]
    }

    fn screen_info(&self) -> ScreenInfo {
        ScreenInfo {
            width: self.width,
            height: self.height,
        }
    }

    fn output_at(&self, x: i32, y: i32) -> Option<OutputId> {
        if x >= 0 && y >= 0 && x < self.width && y < self.height {
            Some(OutputId(1))
        } else {
            None
        }
    }
}

struct XcbKeyOps {
    conn: Arc<xcb::Connection>,
    ids: XcbIdRegistry,
    min_keycode: u8,
    max_keycode: u8,
    keymap: RwLock<Option<XcbKeymap>>,
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
            if let Some(&primary) = symbols.iter().find(|&&sym| sym != 0) {
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
        for &(mods, keysym) in bindings {
            let Some(keycodes) = keymap.keysym_to_keycodes.get(&keysym) else {
                log::warn!("XCB: no keycode found for keysym 0x{keysym:x}");
                continue;
            };
            for &key in keycodes {
                let _ = self.conn.send_and_check_request(&x::GrabKey {
                    owner_events: false,
                    grab_window: root,
                    modifiers: mods_to_xcb(mods),
                    key,
                    pointer_mode: x::GrabMode::Async,
                    keyboard_mode: x::GrabMode::Async,
                });
            }
        }
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
        Ok(())
    }

    fn ungrab_keyboard(&self) -> XcbResult<()> {
        self.conn
            .send_and_check_request(&x::UngrabKeyboard {
                time: x::CURRENT_TIME,
            })
            .map_err(xcb_err)
    }

    fn clean_mods(&self, raw_state: u16) -> Mods {
        mods_from_bits(raw_state)
    }

    fn keysym_from_keycode(&mut self, keycode: u8) -> XcbResult<KeySym> {
        let keymap = self.load_keymap()?;
        keymap
            .keycode_to_keysym
            .get(&keycode)
            .copied()
            .ok_or(BackendError::NotFound("keycode not found in XCB keymap"))
    }

    fn clear_cache(&mut self) {
        *self.keymap.write().unwrap() = None;
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
        change_u32s(
            &self.conn,
            self.root,
            self.atoms.net_active_window,
            self.atoms.window,
            &[0],
        )
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
                value_list: &[],
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
        let joined = names.join("\0");
        change_bytes(
            &self.conn,
            self.root,
            self.atoms.net_desktop_names,
            self.atoms.utf8_string,
            joined.as_bytes(),
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

fn root_event_mask() -> x::EventMask {
    x::EventMask::SUBSTRUCTURE_REDIRECT
        | x::EventMask::SUBSTRUCTURE_NOTIFY
        | x::EventMask::PROPERTY_CHANGE
        | x::EventMask::BUTTON_PRESS
        | x::EventMask::BUTTON_RELEASE
        | x::EventMask::POINTER_MOTION
        | x::EventMask::ENTER_WINDOW
        | x::EventMask::LEAVE_WINDOW
        | x::EventMask::FOCUS_CHANGE
        | x::EventMask::KEY_PRESS
        | x::EventMask::KEY_RELEASE
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

fn mods_from_bits(bits: u16) -> Mods {
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
    if bits & x::ModMask::N2.bits() != 0 {
        mods |= Mods::MOD2;
    }
    if bits & x::ModMask::N3.bits() != 0 {
        mods |= Mods::MOD3;
    }
    if bits & x::ModMask::N4.bits() != 0 {
        mods |= Mods::SUPER;
    }
    if bits & x::ModMask::N5.bits() != 0 {
        mods |= Mods::MOD5;
    }
    if bits & x::ModMask::LOCK.bits() != 0 {
        mods |= Mods::CAPS;
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
    match mode {
        StackMode::Above => x::StackMode::Above,
        StackMode::Below => x::StackMode::Below,
        StackMode::TopIf => x::StackMode::TopIf,
        StackMode::BottomIf => x::StackMode::BottomIf,
        StackMode::Opposite => x::StackMode::Opposite,
    }
}

fn stack_mode_from_xcb(mode: x::StackMode) -> StackMode {
    match mode {
        x::StackMode::Above => StackMode::Above,
        x::StackMode::Below => StackMode::Below,
        x::StackMode::TopIf => StackMode::TopIf,
        x::StackMode::BottomIf => StackMode::BottomIf,
        x::StackMode::Opposite => StackMode::Opposite,
    }
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

fn get_u32s(conn: &xcb::Connection, window: x::Window, property: x::Atom, ty: x::Atom) -> Vec<u32> {
    let cookie = conn.send_request(&x::GetProperty {
        delete: false,
        window,
        property,
        r#type: ty,
        long_offset: 0,
        long_length: 4096,
    });
    conn.wait_for_reply(cookie)
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
    vec![
        EwmhFeature::ActiveWindow,
        EwmhFeature::Supported,
        EwmhFeature::WmName,
        EwmhFeature::WmState,
        EwmhFeature::SupportingWmCheck,
        EwmhFeature::WmStateFullscreen,
        EwmhFeature::WmStateMaximizedVert,
        EwmhFeature::WmStateMaximizedHorz,
        EwmhFeature::WmStateHidden,
        EwmhFeature::WmStateAbove,
        EwmhFeature::WmStateBelow,
        EwmhFeature::WmStateDemandsAttention,
        EwmhFeature::WmStateSticky,
        EwmhFeature::WmStateSkipTaskbar,
        EwmhFeature::WmStateSkipPager,
        EwmhFeature::ClientList,
        EwmhFeature::ClientInfo,
        EwmhFeature::WmWindowType,
        EwmhFeature::WmWindowTypeDialog,
        EwmhFeature::CurrentDesktop,
        EwmhFeature::NumberOfDesktops,
        EwmhFeature::DesktopNames,
        EwmhFeature::DesktopViewport,
        EwmhFeature::FrameExtents,
        EwmhFeature::WmAllowedActions,
        EwmhFeature::Workarea,
        EwmhFeature::CloseWindow,
        EwmhFeature::RestackWindow,
        EwmhFeature::WmPing,
        EwmhFeature::WmUserTime,
        EwmhFeature::WmIcon,
        EwmhFeature::WmBypassCompositor,
        EwmhFeature::WmOpaqueRegion,
    ]
}

fn feature_atom(atoms: XcbAtoms, feature: EwmhFeature) -> Option<x::Atom> {
    Some(match feature {
        EwmhFeature::ActiveWindow => atoms.net_active_window,
        EwmhFeature::Supported => atoms.net_supported,
        EwmhFeature::WmName => atoms.net_wm_name,
        EwmhFeature::WmState => atoms.net_wm_state,
        EwmhFeature::SupportingWmCheck => atoms.net_supporting_wm_check,
        EwmhFeature::WmStateFullscreen => atoms.net_wm_state_fullscreen,
        EwmhFeature::WmStateMaximizedVert => atoms.net_wm_state_maximized_vert,
        EwmhFeature::WmStateMaximizedHorz => atoms.net_wm_state_maximized_horz,
        EwmhFeature::WmStateHidden => atoms.net_wm_state_hidden,
        EwmhFeature::WmStateAbove => atoms.net_wm_state_above,
        EwmhFeature::WmStateBelow => atoms.net_wm_state_below,
        EwmhFeature::WmStateDemandsAttention => atoms.net_wm_state_demands_attention,
        EwmhFeature::WmStateSticky => atoms.net_wm_state_sticky,
        EwmhFeature::WmStateSkipTaskbar => atoms.net_wm_state_skip_taskbar,
        EwmhFeature::WmStateSkipPager => atoms.net_wm_state_skip_pager,
        EwmhFeature::ClientList => atoms.net_client_list,
        EwmhFeature::ClientInfo => atoms.net_client_info,
        EwmhFeature::WmWindowType => atoms.net_wm_window_type,
        EwmhFeature::WmWindowTypeDialog => atoms.net_wm_window_type_dialog,
        EwmhFeature::CurrentDesktop => atoms.net_current_desktop,
        EwmhFeature::NumberOfDesktops => atoms.net_number_of_desktops,
        EwmhFeature::DesktopNames => atoms.net_desktop_names,
        EwmhFeature::DesktopViewport => atoms.net_desktop_viewport,
        EwmhFeature::FrameExtents => atoms.net_frame_extents,
        EwmhFeature::WmAllowedActions => atoms.net_wm_allowed_actions,
        EwmhFeature::Workarea => atoms.net_workarea,
        EwmhFeature::CloseWindow => atoms.net_close_window,
        EwmhFeature::RestackWindow => atoms.net_restack_window,
        EwmhFeature::WmPing => atoms.net_wm_ping,
        EwmhFeature::WmUserTime => atoms.net_wm_user_time,
        EwmhFeature::WmIcon => atoms.net_wm_icon,
        EwmhFeature::WmBypassCompositor => atoms.net_wm_bypass_compositor,
        EwmhFeature::WmOpaqueRegion => atoms.net_wm_opaque_region,
        EwmhFeature::WmMoveResize => return None,
    })
}

fn xcb_err<E>(err: E) -> BackendError
where
    E: std::error::Error + Send + Sync + 'static,
{
    BackendError::Other(Box::new(err))
}
