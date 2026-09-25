use crate::backend::api::{
    BackendEvent, Geometry, LayerSurfaceInfo, MaximizeAxes, NetWmAction, NetWmState, PropertyKind,
    WindowType,
};
use crate::backend::common_define::WindowId;
use crate::backend::error::BackendError;
use crate::sync_ext::MutexExt;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::{debug, info, warn};

fn env_flag(name: &str) -> bool {
    std::env::var_os(name).as_deref() == Some(std::ffi::OsStr::new("1"))
}

fn optional_global_enabled(config_enabled: bool, flag_name: &str) -> bool {
    config_enabled || env_flag("JWM_OPTIONAL_GLOBALS") || env_flag(flag_name)
}

use smithay::delegate_dispatch2;
use smithay::xwayland::{X11Wm, X11Surface, XwmHandler, XWaylandClientData, xwm::{Reorder, ResizeEdge as XwmResizeEdge, XwmId, WmWindowProperty}};
use smithay::wayland::xwayland_shell::{XWaylandShellHandler, XWaylandShellState};
use smithay::input::keyboard::XkbConfig;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::calloop::channel::Sender;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason, ObjectId};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::{Client, DisplayHandle, Resource};
use smithay::utils::{
    Logical, Point, Rectangle, Serial, SERIAL_COUNTER as SCOUNTER,
};
use smithay::desktop::{
    find_popup_root_surface, get_popup_toplevel_coords, layer_map_for_output,
    utils::under_from_surface_tree, LayerSurface as DesktopLayerSurface, PopupKind,
    WindowSurfaceType,
};
use smithay::output::Output;
use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::Format as DmabufFormat;
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    get_parent, with_states, with_surface_tree_downward, BufferAssignment, CompositorClientState,
    CompositorHandler, CompositorState, SurfaceAttributes, TraversalAction,
};
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::shell::wlr_layer::{Anchor, Layer, LayerSurface as WlrLayerSurface, LayerSurfaceData, WlrLayerShellHandler, WlrLayerShellState};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, SurfaceCachedState, ToplevelState, ToplevelSurface,
    XdgShellHandler, XdgShellState,
};
use smithay::wayland::shell::xdg::decoration::{XdgDecorationHandler, XdgDecorationState};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::wayland::socket::ListeningSocketSource;
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::{SelectionHandler, SelectionTarget, SelectionSource};
use smithay::wayland::selection::data_device::{
    DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler,
    clear_data_device_selection, current_data_device_selection_userdata,
    request_data_device_client_selection, set_data_device_focus, set_data_device_selection,
};
use smithay::input::dnd::{DnDGrab, DndGrabHandler, GrabType, Source, DndTarget};
use smithay::input::pointer::Focus;
use smithay::wayland::selection::primary_selection::{
    PrimarySelectionHandler, PrimarySelectionState,
    clear_primary_selection, current_primary_selection_userdata,
    request_primary_client_selection, set_primary_focus, set_primary_selection,
};
use smithay::wayland::viewporter::ViewporterState;
use smithay::wayland::text_input::TextInputManagerState;
use smithay::wayland::input_method::{InputMethodHandler, InputMethodManagerState, PopupSurface as ImPopupSurface};
use smithay::wayland::virtual_keyboard::VirtualKeyboardManagerState;
use smithay::wayland::xdg_activation::{XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData};
use smithay::wayland::pointer_constraints::{
    PointerConstraint, PointerConstraintsHandler, PointerConstraintsState, with_pointer_constraint,
};
use smithay::wayland::relative_pointer::RelativePointerManagerState;
use smithay::wayland::session_lock::{SessionLockHandler, SessionLockManagerState, SessionLocker, LockSurface};
use smithay::reexports::wayland_protocols::ext::session_lock::v1::server::ext_session_lock_v1::ExtSessionLockV1;
use smithay::wayland::idle_inhibit::{IdleInhibitHandler, IdleInhibitManagerState};
use smithay::wayland::idle_notify::IdleNotifierState;
use smithay::wayland::fractional_scale::{with_fractional_scale, FractionalScaleHandler, FractionalScaleManagerState};
use smithay::wayland::cursor_shape::CursorShapeManagerState;
use smithay::wayland::presentation::PresentationState;
use smithay::wayland::pointer_gestures::PointerGesturesState;
use smithay::wayland::single_pixel_buffer::SinglePixelBufferState;
use smithay::wayland::content_type::ContentTypeState;
use smithay::wayland::alpha_modifier::AlphaModifierState;
use smithay::wayland::background_effect::{BackgroundEffectState, ExtBackgroundEffectHandler};
use smithay::wayland::foreign_toplevel_list::{ForeignToplevelListState, ForeignToplevelListHandler, ForeignToplevelHandle};
use smithay::wayland::tablet_manager::TabletManagerState;
use smithay::wayland::fifo::FifoManagerState;
use smithay::wayland::keyboard_shortcuts_inhibit::{
    KeyboardShortcutsInhibitState, KeyboardShortcutsInhibitor, KeyboardShortcutsInhibitorSeat,
};
use smithay::wayland::security_context::SecurityContextState;
use smithay::wayland::commit_timing::{CommitTimerStateUserData, CommitTimingManagerState};
use smithay::wayland::shell::xdg::dialog::{XdgDialogState, XdgDialogHandler, ToplevelDialogHint};
use smithay::wayland::xdg_foreign::{XdgForeignState, XdgForeignHandler};
use smithay::wayland::xdg_system_bell::{XdgSystemBellState, XdgSystemBellHandler};
use smithay::wayland::pointer_warp::{PointerWarpManager, PointerWarpHandler};
use smithay::wayland::xwayland_keyboard_grab::{XWaylandKeyboardGrabState, XWaylandKeyboardGrabHandler};
use smithay::wayland::drm_syncobj::{DrmSyncobjState, DrmSyncobjHandler};
use smithay::wayland::xdg_toplevel_icon::{XdgToplevelIconManager, XdgToplevelIconHandler};
use smithay::wayland::xdg_toplevel_tag::{XdgToplevelTagManager, XdgToplevelTagHandler};
use smithay::wayland::selection::wlr_data_control::{DataControlState, DataControlHandler};
use smithay::wayland::selection::ext_data_control::{
    DataControlState as ExtDataControlState,
    DataControlHandler as ExtDataControlHandler,
};
use smithay::wayland::shell::kde::decoration::{KdeDecorationHandler, KdeDecorationState};
use smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration_manager::Mode as KdeMode;
use smithay::input::pointer::PointerHandle;

const INITIAL_CONFIGURE_TIMEOUT: Duration = Duration::from_millis(250);

/// How long an xdg-activation token may still activate a surface.
const XDG_ACTIVATION_TOKEN_LIFETIME: Duration = Duration::from_secs(10);

#[derive(Debug, Default)]
pub struct JwmClientState {
    pub compositor_state: CompositorClientState,
    /// Set for clients that connected through a wp_security_context listener
    /// (Flatpak/sandbox). `None` for normal clients on the main socket.
    pub security_context: Option<smithay::wayland::security_context::SecurityContext>,
}

/// Deferred ack for wlr-output-management Apply requests. The udev backend
/// invokes the callback with `true` after a successful modeset and `false`
/// otherwise, so the wlr-output-configuration resource is acked only after
/// the actual outcome is known. FIFO with respect to the matching
/// `BackendEvent::OutputConfigure` entries in `pending_events`.
pub struct PendingOutputAck {
    pub on_complete: Box<dyn FnOnce(bool) + Send>,
}

impl std::fmt::Debug for PendingOutputAck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PendingOutputAck")
    }
}

/// Whether `client` connected through a wp_security_context listener
/// (Flatpak and other sandboxes). Clients with other client data, such as
/// Xwayland, are never sandboxed.
pub(crate) fn client_is_sandboxed(client: &Client) -> bool {
    client
        .get_data::<JwmClientState>()
        .is_some_and(|data| data.security_context.is_some())
}

/// Global filter for privileged protocols: visible to every client except a
/// sandboxed one.
fn client_is_unsandboxed(client: &Client) -> bool {
    !client_is_sandboxed(client)
}

impl ClientData for JwmClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, client_id: ClientId, reason: DisconnectReason) {
        log::info!("[udev/wayland] client disconnected: id={client_id:?} reason={reason:?}");
    }
}

#[derive(Debug, Clone)]
pub struct DndIcon {
    pub surface: WlSurface,
    pub offset: Point<i32, Logical>,
}

/// In-progress touchpad swipe gesture, accumulated between Begin and End.
/// When `intercept` is true, neither the corresponding Begin/Update/End nor
/// any in-flight events should be forwarded to client surfaces; the WM has
/// claimed the configured gesture.
#[derive(Debug, Default, Clone)]
pub struct GestureSwipeTracker {
    pub fingers: u32,
    pub intercept: bool,
    pub dx: f64,
    pub dy: f64,
}

#[derive(Debug, Default, Clone)]
pub struct CaptureCounters {
    pub screencopy_queued_total: u64,
    pub screencopy_failed_total: u64,
    pub screencopy_fulfilled_total: u64,
    pub screencopy_render_failed_total: u64,
    pub image_copy_sessions_total: u64,
    pub image_copy_queued_total: u64,
    pub image_copy_failed_total: u64,
    pub image_copy_fulfilled_total: u64,
    pub image_copy_render_failed_total: u64,
    pub image_copy_output_queued_total: u64,
    pub image_copy_toplevel_queued_total: u64,
    pub last_queued_unix_ms: Option<u64>,
    pub last_fulfilled_unix_ms: Option<u64>,
    pub last_failed_unix_ms: Option<u64>,
    pub last_failure_reason: Option<String>,
}

impl CaptureCounters {
    fn now_unix_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64
    }

    pub fn note_screencopy_queued(&mut self) {
        self.screencopy_queued_total = self.screencopy_queued_total.saturating_add(1);
        self.last_queued_unix_ms = Some(Self::now_unix_ms());
    }

    pub fn note_screencopy_failed(&mut self, reason: impl Into<String>) {
        self.screencopy_failed_total = self.screencopy_failed_total.saturating_add(1);
        self.note_failed(reason);
    }

    pub fn note_screencopy_fulfilled(&mut self) {
        self.screencopy_fulfilled_total = self.screencopy_fulfilled_total.saturating_add(1);
        self.last_fulfilled_unix_ms = Some(Self::now_unix_ms());
    }

    pub fn note_screencopy_render_failed(&mut self, reason: impl Into<String>) {
        self.screencopy_render_failed_total = self.screencopy_render_failed_total.saturating_add(1);
        self.note_failed(reason);
    }

    pub fn note_image_copy_session(&mut self) {
        self.image_copy_sessions_total = self.image_copy_sessions_total.saturating_add(1);
    }

    pub fn note_image_copy_queued(&mut self, source: &str) {
        self.image_copy_queued_total = self.image_copy_queued_total.saturating_add(1);
        match source {
            "output" => {
                self.image_copy_output_queued_total =
                    self.image_copy_output_queued_total.saturating_add(1);
            }
            "toplevel" => {
                self.image_copy_toplevel_queued_total =
                    self.image_copy_toplevel_queued_total.saturating_add(1);
            }
            _ => {}
        }
        self.last_queued_unix_ms = Some(Self::now_unix_ms());
    }

    pub fn note_image_copy_failed(&mut self, reason: impl Into<String>) {
        self.image_copy_failed_total = self.image_copy_failed_total.saturating_add(1);
        self.note_failed(reason);
    }

    pub fn note_image_copy_fulfilled(&mut self) {
        self.image_copy_fulfilled_total = self.image_copy_fulfilled_total.saturating_add(1);
        self.last_fulfilled_unix_ms = Some(Self::now_unix_ms());
    }

    pub fn note_image_copy_render_failed(&mut self, reason: impl Into<String>) {
        self.image_copy_render_failed_total = self.image_copy_render_failed_total.saturating_add(1);
        self.note_failed(reason);
    }

    fn note_failed(&mut self, reason: impl Into<String>) {
        self.last_failed_unix_ms = Some(Self::now_unix_ms());
        self.last_failure_reason = Some(reason.into());
    }
}

pub struct JwmWaylandState {
    pub display_handle: DisplayHandle,
    /// Text copied by clients, waiting to be drained into the history. Filled
    /// by the reader threads started in `SelectionHandler::new_selection`, in
    /// the order the reads finish, each tagged with the capture generation
    /// its read started under.
    pub clipboard_captured: std::sync::Arc<
        std::sync::Mutex<Vec<(u64, crate::backend::clipboard_offer::CapturedClipboard)>>,
    >,
    /// Generation of the newest selection read started by `capture_clipboard`.
    clipboard_capture_generation: u64,
    /// Newest generation already handed out by `drain_clipboard_captured`.
    clipboard_delivered_generation: u64,
    /// Entry JWM is currently offering as the selection source, if any.
    /// `send_selection` writes this; a client taking the selection clears it.
    pub(crate) clipboard_offered: Option<crate::backend::clipboard_offer::ClipboardOffer>,
    /// MIME types of a selection to read on the next tick.
    ///
    /// `new_selection` fires *before* smithay stores the new selection on the
    /// seat, so asking for it from inside the handler always answers
    /// `NoSelection`. The request is deferred by one turn of the event loop.
    pub clipboard_pending: Option<Vec<String>>,
    pub loop_handle: smithay::reexports::calloop::LoopHandle<'static, JwmWaylandState>,
    pub pending_events: Arc<Mutex<std::collections::VecDeque<BackendEvent>>>,

    /// Window ids that left the live compositor scene through unmap or surface
    /// destruction. Drained each frame by the udev backend to start a safe
    /// close/genie retirement and evict side-map state.
    pub compositor_dead_windows: Vec<u64>,

    pub pointer_location: Point<f64, Logical>,
    /// Pointer location requested by pointer-warp/cursor-position-hint.
    /// The udev backend drains this after calloop dispatch and mirrors it into
    /// its shared input state, which drives WM hit testing and cursor rendering.
    pub pending_pointer_warp: Option<Point<f64, Logical>>,
    pub needs_redraw: bool,

    pub dnd_icon: Option<DndIcon>,

    pub output_manager_state: OutputManagerState,

    pub compositor_state: CompositorState,
    pub shm_state: ShmState,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
    pub seat_state: SeatState<JwmWaylandState>,
    pub seat: Seat<JwmWaylandState>,
    pub xdg_shell_state: XdgShellState,
    pub xdg_decoration_state: XdgDecorationState,
    pub viewporter_state: ViewporterState,

    pub dmabuf_state: DmabufState,
    pub dmabuf_global: Option<DmabufGlobal>,
    /// DRM device node (dev_t) backing the renderer, and the renderable dmabuf
    /// formats. Captured when the dmabuf global is created so the
    /// ext-image-copy-capture session can advertise dmabuf buffers to clients.
    pub dmabuf_main_device: Option<libc::dev_t>,
    pub dmabuf_render_formats: Vec<DmabufFormat>,

    pub layer_shell_state: WlrLayerShellState,

    pub xdg_activation_state: XdgActivationState,

    // --- SOTA protocol state ---
    pub pointer_constraints_state: PointerConstraintsState,
    pub relative_pointer_state: RelativePointerManagerState,
    pub session_lock_state: SessionLockManagerState,
    pub idle_inhibit_state: IdleInhibitManagerState,
    pub idle_notifier_state: IdleNotifierState<JwmWaylandState>,
    pub fractional_scale_state: FractionalScaleManagerState,
    pub cursor_shape_state: CursorShapeManagerState,
    pub presentation_state: PresentationState,
    pub pointer_gestures_state: PointerGesturesState,
    pub single_pixel_buffer_state: SinglePixelBufferState,
    pub content_type_state: ContentTypeState,
    pub alpha_modifier_state: AlphaModifierState,
    pub foreign_toplevel_list_state: ForeignToplevelListState,
    pub tablet_manager_state: TabletManagerState,
    pub fifo_state: FifoManagerState,
    pub keyboard_shortcuts_inhibit_state: KeyboardShortcutsInhibitState,
    pub security_context_state: SecurityContextState,
    pub commit_timing_state: CommitTimingManagerState,
    pub xdg_dialog_state: XdgDialogState,
    pub xdg_foreign_state: XdgForeignState,
    pub xdg_system_bell_state: XdgSystemBellState,
    pub pointer_warp_state: PointerWarpManager,
    pub xwayland_keyboard_grab_state: XWaylandKeyboardGrabState,
    pub drm_syncobj_state: Option<DrmSyncobjState>,
    pub data_control_state: DataControlState,
    pub ext_data_control_state: ExtDataControlState,
    pub kde_decoration_state: KdeDecorationState,

    /// Live idle inhibitors per surface. A count, not a set: two inhibitors
    /// on one surface must not clear each other. Smithay reports only an
    /// explicit inhibitor destroy, so `CompositorHandler::destroyed` drops a
    /// dead surface's entry (client crash or surface destroyed first).
    pub idle_inhibiting_surfaces: HashMap<ObjectId, usize>,
    /// When input last arrived, for the session idle policy. Kept beside the
    /// idle notifier because both are fed from the same libinput callback.
    pub last_input: std::time::Instant,
    pub session_locked: bool,
    /// Monotonic lock generation. A repeat armed before a fast lock/unlock
    /// cycle must not resume after the lock disappears.
    pub session_lock_epoch: u64,
    /// Per-output session lock surfaces. Key: Smithay output name.
    /// Populated on `SessionLockHandler::new_surface`, drained on unlock or
    /// destruction. Used to know whether the lock client has a presence on a
    /// given output and (later) to render only those surfaces while locked.
    pub lock_surfaces: HashMap<String, LockSurface>,
    /// A lock request whose `locked` event is owed until every output shows
    /// locked content. See [`JwmWaylandState::note_locked_frame_presented`].
    pending_session_lock: Option<PendingSessionLock>,
    /// The lock object that was sent `locked` and may unlock the session.
    /// Kept after its client dies (Smithay's `LockStatus::Defunct`), which is
    /// the only state in which another client may take the lock over.
    active_session_lock: Option<ExtSessionLockV1>,
    pub foreign_toplevel_handles: HashMap<WindowId, ForeignToplevelHandle>,

    /// Touchpad swipe-gesture tracker. When `intercept` is true, the WM is
    /// "consuming" the in-progress swipe and forwarding nothing to clients.
    pub gesture_swipe: GestureSwipeTracker,

    /// XWayland shell state (for associating X11 windows with wl_surfaces).
    pub xwayland_shell_state: XWaylandShellState,

    /// The X11 WM instance (set after XWayland becomes ready).
    pub x11_wm: Option<X11Wm>,

    /// Map from X11Surface window_id -> our WindowId.
    pub x11_surface_to_window: HashMap<u32, WindowId>,

    /// Map from our WindowId -> X11Surface (for property queries etc.).
    pub x11_surfaces: HashMap<WindowId, X11Surface>,

    /// XWayland may associate a `wl_surface` with an X11 window before we allocate a `WindowId`.
    /// Stash the association so we can wire it up once `map_window_request`/`mapped_override_redirect_window`
    /// allocates the window.
    pub pending_x11_wl_surfaces: HashMap<u32, WlSurface>,

    /// Map from our WindowId -> the X11 window's content `wl_surface`, resolved manually for the
    /// legacy `WL_SURFACE_ID` association path (XWayland < 23.1). smithay only auto-associates via
    /// the modern xwayland_shell protocol, so for old XWayland `X11Surface::wl_surface()` stays
    /// `None` and we track the surface here instead.
    pub x11_wl_surfaces: HashMap<WindowId, WlSurface>,

    /// KMS-backed outputs currently available for mapping layer surfaces.
    pub outputs: Vec<Output>,

    /// Runtime bind counters for JWM-owned Wayland globals. Smithay-managed
    /// core globals are not counted here; this tracks the custom desktop,
    /// capture, color, power, and control protocols we implement directly.
    pub protocol_bind_counts: HashMap<&'static str, crate::backend::api::ProtocolBindStatus>,

    /// FIFO of pending `wlr-output-configuration::Apply` acks waiting for the
    /// udev backend to finish their modeset. Drained in order matching
    /// `BackendEvent::OutputConfigure` entries in `pending_events`.
    pub pending_output_acks: std::collections::VecDeque<PendingOutputAck>,

    /// Output names a client has soft-disabled via wlr-output-management
    /// `disable_head`. We do not currently tear the DrmOutput down; instead the
    /// output is advertised as disabled to clients and the compositor skips
    /// frame submission for it. Re-enabled by an Apply that enables the head.
    pub soft_disabled_outputs: HashSet<String>,

    /// Most recent wlr-output-management Apply/Test request rejected during
    /// protocol validation, kept for `jwm-tool wayland-status` diagnostics.
    pub last_output_management_rejection:
        Option<crate::backend::api::OutputManagementRejectedConfig>,

    /// Hardware gamma LUT size per output name, queried from the CRTC.
    /// Used by wlr-gamma-control to advertise the correct ramp size.
    pub gamma_sizes: HashMap<String, u32>,

    pub next_window_raw: u64,
    pub toplevels: HashMap<WindowId, ToplevelSurface>,
    pub layer_surfaces: HashMap<WindowId, WlSurface>,
    pub surface_to_window: HashMap<ObjectId, WindowId>,
    /// Monotonic per-window generation advanced by commits on the root
    /// surface or any of its subsurfaces. The udev renderer uses this to avoid
    /// retrying a hidden minimized-surface import on every unrelated frame.
    surface_commit_epochs: HashMap<WindowId, u64>,

    pub pending_initial_configure: HashSet<WindowId>,
    pending_size_reconfigure: HashMap<WindowId, ((u32, u32), Instant)>,

    pub popups: HashMap<ObjectId, PopupSurface>,
    pub popup_order: Vec<ObjectId>,

    pub im_popups: Vec<ImPopupSurface>,
    pub im_client_id: Option<ObjectId>,
    /// (popup surface, failure kind) pairs `im_popup_positions` has already
    /// warned about; the warn-once contract lives with that function. `Mutex`
    /// because the position query — and therefore the gate — only has `&self`.
    im_popup_warned: Mutex<HashSet<(ObjectId, ImPopupWarn)>>,

    pub active_toplevel: Option<WindowId>,
    pub popup_grab_toplevel: Option<WindowId>,
    pub popup_grab_prev_kbd_focus: Option<WlSurface>,
    pub output_rects: Vec<Rectangle<i32, Logical>>,

    pub window_geometry: HashMap<WindowId, Geometry>,
    pub window_stack: Vec<WindowId>,

    pub mapped_windows: HashSet<WindowId>,
    /// Windows deliberately hidden by the window manager while their client
    /// surface remains alive. A later buffer commit must not map one again
    /// behind JWM's back.
    manager_unmapped_windows: HashSet<WindowId>,
    pub window_title: HashMap<WindowId, String>,
    pub window_app_id: HashMap<WindowId, String>,
    pub window_activation_app_id: HashMap<WindowId, String>,
    pub window_is_fullscreen: HashMap<WindowId, bool>,
    /// Maximize state accepted by shared policy; only non-NONE entries.
    pub(crate) window_maximized: HashMap<WindowId, MaximizeAxes>,
    /// xdg toplevels whose set_maximized/unset_maximized still owes the configure
    /// that xdg-shell requires.
    pub(crate) xdg_state_reply_owed: HashSet<WindowId>,
    pub window_type_overrides: HashMap<WindowId, Vec<WindowType>>,

    pub window_layer_info: HashMap<WindowId, LayerSurfaceInfo>,

    /// Per-window border color (ARGB, used for server-side decoration in tiling WM).
    pub window_border_color: HashMap<WindowId, [f32; 4]>,

    /// Shared queue for pending wlr-screencopy copy requests (filled by screencopy Dispatch,
    /// drained during KMS render).
    pub screencopy_pending:
        Option<crate::backend::wayland_udev::screencopy::PendingScreencopyQueue>,

    /// Per-surface tearing hint map (from wp-tearing-control-v1).
    pub tearing_hints: Option<crate::backend::wayland_udev::tearing_control::TearingHintMap>,

    /// ext-workspace-v1 state for taskbar integration.
    pub workspace_state: Option<crate::backend::wayland_udev::workspace_protocol::WorkspaceState>,

    /// Pending ext-image-copy-capture frames (drained during render, like screencopy).
    pub image_capture_pending:
        Option<crate::backend::wayland_udev::image_copy_capture::PendingImageCaptureQueue>,
    /// Live ext-image-copy-capture sessions of each captured window, stopped
    /// when the window is retired (see `retire_window_geometry`).
    pub(crate) toplevel_capture_sessions:
        crate::backend::wayland_udev::image_copy_capture::ToplevelCaptureSessions,

    /// Runtime counters for capture protocols. These are protocol-dispatch
    /// counters (queued/rejected), separate from the render-drain queue depth.
    pub capture_counters: Arc<Mutex<CaptureCounters>>,

    /// wlr-foreign-toplevel-management state (taskbar window list + control).
    pub foreign_toplevel_mgmt:
        Option<crate::backend::wayland_udev::foreign_toplevel_management::ForeignToplevelMgmtState>,

    /// wp-color-management-v1 state (per-surface image description registry).
    pub color_manager: Option<crate::backend::wayland_udev::color_management::ColorManagerState>,

    /// Bound wlr-output-management managers and the head state they were
    /// last sent. `None` where the global is not advertised.
    pub output_management:
        Option<crate::backend::wayland_udev::output_management::OutputManagementState>,

    /// Which wlr-gamma-control control holds each output. The backend fails
    /// the controls a layout change made stale through it.
    pub(crate) gamma_owners: Option<crate::backend::wayland_udev::gamma_control::GammaOwners>,

    /// Wakes the backend's client flush for events queued outside a request
    /// dispatch or a render (a timer callback, for instance).
    client_flush_tx: Sender<()>,
    client_flush_pending: Arc<AtomicBool>,
}

/// How long a session lock may wait for every output to present a locked
/// frame before it is confirmed anyway. A presenting output needs a frame or
/// two (tens of milliseconds, a modeset included); the bound only matters
/// when an output cannot present at all (a stuck page flip, a device whose
/// delivery is blocked) and keeps `swaylock -f && systemctl suspend` from
/// waiting forever.
pub(crate) const SESSION_LOCK_CONFIRM_DEADLINE: Duration = Duration::from_secs(1);

/// An ext-session-lock request whose `locked` event is still owed.
///
/// ext-session-lock-v1 allows `locked` only once no unlocked content is
/// visible any more, so the confirmation waits for a locked frame (black
/// shield, lock surface on top) to reach every output that was showing
/// content when the lock was requested.
struct PendingSessionLock {
    locker: SessionLocker,
    /// `session_lock_epoch` of this request. A frame rendered for an older
    /// lock does not pay this one.
    epoch: u64,
    /// Outputs that still owe a presented locked frame.
    owed_outputs: HashSet<String>,
}

/// Placement anchor for an IME candidate popup. Carries the cursor line in
/// absolute coords plus the parent window's rect so the renderer can decide
/// whether to draw the popup below the cursor or flip it above when it would
/// overflow the bottom edge — the popup height isn't known until its texture
/// has been imported, so the final clamp happens at render time.
pub struct ImPopupAnchor {
    pub surface: WlSurface,
    pub x: i32,
    /// Top of the text-cursor line (popup top when flipped above).
    pub cursor_top: i32,
    /// Bottom of the text-cursor line (popup top when placed below).
    pub cursor_bottom: i32,
    pub area_left: i32,
    pub area_top: i32,
    pub area_right: i32,
    pub area_bottom: i32,
}

// This file is also compiled through a compatibility module; only the shared
// udev state instance receives WindowOps callbacks in that build.
#[allow(dead_code)]
fn apply_manager_mapping_state(
    mapped_windows: &mut HashSet<WindowId>,
    manager_unmapped_windows: &mut HashSet<WindowId>,
    win: WindowId,
    mapped: bool,
) {
    if mapped {
        manager_unmapped_windows.remove(&win);
        mapped_windows.insert(win);
    } else {
        manager_unmapped_windows.insert(win);
        mapped_windows.remove(&win);
    }
}

fn manager_allows_surface_map(manager_unmapped_windows: &HashSet<WindowId>, win: WindowId) -> bool {
    !manager_unmapped_windows.contains(&win)
}

fn claim_initial_configure_fallback(pending: &mut HashSet<WindowId>, win: WindowId) -> bool {
    pending.remove(&win)
}

fn take_window_mapping_state(
    mapped_windows: &mut HashSet<WindowId>,
    manager_unmapped_windows: &mut HashSet<WindowId>,
    win: WindowId,
) -> bool {
    let was_manager_unmapped = manager_unmapped_windows.remove(&win);
    let was_mapped = mapped_windows.remove(&win);
    was_manager_unmapped || was_mapped
}

/// The distinct ways `im_popup_positions` can fail to place a popup. Part of
/// the warn-once key, so a popup that degrades from one failure into the next
/// warns once for each.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum ImPopupWarn {
    NoParent,
    ParentUnmapped,
    NoGeometry,
}

/// Warn-once gate for `im_popup_positions`. Records `key` among this call's
/// failures and reports whether a warn is due — only when the previous call
/// had not already failed the same way. The caller swaps its warned set for
/// the recorded failures after the pass, so a failure that clears, or a popup
/// that goes away, warns again on its next occurrence, while one that
/// persists warns once instead of once per frame.
fn ime_popup_warn_due<K>(warned: &HashSet<K>, failing: &mut HashSet<K>, key: K) -> bool
where
    K: Eq + std::hash::Hash,
{
    let due = !warned.contains(&key);
    failing.insert(key);
    due
}

impl JwmWaylandState {
    #[allow(dead_code)]
    pub(crate) fn set_manager_window_mapped(&mut self, win: WindowId, mapped: bool) {
        apply_manager_mapping_state(
            &mut self.mapped_windows,
            &mut self.manager_unmapped_windows,
            win,
            mapped,
        );
        self.needs_redraw = true;
    }

    fn manager_allows_surface_map(&self, win: WindowId) -> bool {
        manager_allows_surface_map(&self.manager_unmapped_windows, win)
    }

    fn take_window_mapping(&mut self, win: WindowId) -> bool {
        take_window_mapping_state(
            &mut self.mapped_windows,
            &mut self.manager_unmapped_windows,
            win,
        )
    }

    /// Retire a Wayland window after the caller removes its surface mapping.
    /// Both role destruction and abrupt wl_surface destruction must close the
    /// same published handles, even when the wl_surface outlives its role.
    fn remove_wayland_window(&mut self, win: WindowId) {
        self.forget_surface_commit_epoch(win);
        self.take_window_mapping(win);
        self.toplevels.remove(&win);
        self.layer_surfaces.remove(&win);
        self.pending_initial_configure.remove(&win);
        self.pending_size_reconfigure.remove(&win);
        self.retire_window_geometry(win);
        self.window_stack.retain(|w| *w != win);
        self.window_title.remove(&win);
        self.window_app_id.remove(&win);
        self.window_activation_app_id.remove(&win);
        self.window_is_fullscreen.remove(&win);
        self.window_maximized.remove(&win);
        self.xdg_state_reply_owed.remove(&win);
        self.window_type_overrides.remove(&win);
        self.window_layer_info.remove(&win);
        self.window_border_color.remove(&win);

        if let Some(handle) = self.foreign_toplevel_handles.remove(&win) {
            self.foreign_toplevel_list_state.remove_toplevel(&handle);
        }
        if let Some(ref ftm) = self.foreign_toplevel_mgmt {
            ftm.remove_window(win);
        }

        self.compositor_dead_windows.push(win.raw());
        self.push_event(BackendEvent::WindowDestroyed(win));
        self.needs_redraw = true;
    }

    /// Drop `win`'s geometry, the liveness key of its capture sessions, and
    /// stop those sessions with it, so a paused portal or OBS stream hears
    /// `stopped` now rather than on its next `create_frame`. Every path that
    /// retires a window (a closed Wayland window, an unmapped or destroyed
    /// X11 one) goes through here, so a new one cannot forget the sessions.
    fn retire_window_geometry(&mut self, win: WindowId) {
        self.window_geometry.remove(&win);
        if crate::backend::wayland_udev::image_copy_capture::stop_toplevel_capture_sessions(
            self, win,
        ) {
            self.request_client_flush();
        }
    }

    fn request_window_state(&mut self, window: WindowId, state: NetWmState, on: bool) {
        self.push_event(BackendEvent::WindowStateRequest {
            window,
            action: if on {
                NetWmAction::Add
            } else {
                NetWmAction::Remove
            },
            state,
        });
    }

    fn request_x11_state(&mut self, x11_id: u32, state: NetWmState, on: bool) {
        if let Some(window) = self.x11_surface_to_window.get(&x11_id).copied() {
            self.request_window_state(window, state, on);
        }
    }

    /// Queue a two-axis maximize request for shared policy. xdg-shell,
    /// XWayland (Smithay only decodes the MAXIMIZED_HORZ+VERT pair) and
    /// wlr-foreign-toplevel cannot name a single axis, so every Wayland-side
    /// entry names `BOTH`. Nothing is confirmed here: policy publishes the
    /// accepted state through `set_window_maximized`.
    fn request_window_maximize(&mut self, window: WindowId, on: bool) {
        self.push_event(BackendEvent::WindowMaximizeRequest {
            window,
            action: if on {
                NetWmAction::Add
            } else {
                NetWmAction::Remove
            },
            axes: MaximizeAxes::BOTH,
        });
    }

    fn request_x11_maximize(&mut self, x11_id: u32, on: bool) {
        if let Some(window) = self.x11_surface_to_window.get(&x11_id).copied() {
            self.request_window_maximize(window, on);
        }
    }

    /// xdg-shell requires a configure in reply to set_maximized and
    /// unset_maximized, even when the compositor refuses. The reply is owed
    /// instead of sent: policy's own configure (carrying the accepted state
    /// and size together) pays it, and `flush_owed_xdg_state_replies` pays
    /// whatever policy dropped. Sending here would announce a size the
    /// client must not adopt before policy has decided.
    fn xdg_maximize_request(&mut self, surface: &ToplevelSurface, on: bool) {
        match self
            .surface_to_window
            .get(&surface.wl_surface().id())
            .copied()
        {
            Some(window) => {
                self.xdg_state_reply_owed.insert(window);
                self.request_window_maximize(window, on);
            }
            None => {
                surface.send_configure();
            }
        }
    }

    fn request_x11_minimized(&mut self, x11_id: u32, minimized: bool) {
        // Shared policy owns the Dock entry, animation and restore placement.
        self.request_x11_state(x11_id, NetWmState::Hidden, minimized);
    }

    fn request_window_activation(&mut self, window: WindowId) {
        self.push_event(BackendEvent::ActiveWindowMessage { window });
    }

    fn request_x11_activation(&mut self, x11_id: u32) {
        if let Some(window) = self.x11_surface_to_window.get(&x11_id).copied() {
            self.request_window_activation(window);
        }
    }

    pub(crate) fn set_x11_net_state(
        &mut self,
        win: WindowId,
        flag: NetWmState,
        on: bool,
    ) -> Result<(), BackendError> {
        let Some(surface) = self.x11_surfaces.get(&win) else {
            return Ok(());
        };
        let result = match flag {
            NetWmState::Hidden => surface.set_hidden(on),
            NetWmState::Above => surface.set_above(on),
            NetWmState::Below => surface.set_below(on),
            _ => return Ok(()),
        };
        result.map_err(|error| BackendError::Other(Box::new(error)))
    }

    pub(crate) fn has_x11_net_state(&self, win: WindowId, flag: NetWmState) -> bool {
        self.x11_surfaces
            .get(&win)
            .is_some_and(|surface| match flag {
                NetWmState::Hidden => surface.is_hidden(),
                NetWmState::Fullscreen => surface.is_fullscreen(),
                NetWmState::Above => surface.is_above(),
                NetWmState::Below => surface.is_below(),
                // Smithay tracks only the atom pair, so each axis reads as
                // the pair; single-axis XWayland state is not expressible.
                NetWmState::MaximizedVert | NetWmState::MaximizedHorz => surface.is_maximized(),
                _ => false,
            })
    }

    /// Publish the maximize state shared policy accepted for `win`.
    ///
    /// XWayland gets `_NET_WM_STATE` through `X11Surface::set_maximized`,
    /// which can only express the two-axis pair, so a single axis is published
    /// as not maximized. An xdg toplevel only has `State::Maximized` staged:
    /// nothing is sent here, because the caller's following
    /// `WindowOps::configure` must deliver the state and the new size in ONE
    /// configure. `Tiled*` is left alone; xdg-shell allows it to coexist with
    /// `Maximized`. wlr-foreign-toplevel keeps both axes and reports
    /// `Maximized` only for the pair; axes turn off before others turn on so
    /// a taskbar never sees a transient pair while one axis is swapped.
    pub(crate) fn set_window_maximized(
        &mut self,
        win: WindowId,
        axes: MaximizeAxes,
    ) -> Result<(), BackendError> {
        let maximized = axes.both();
        if let Some(surface) = self.x11_surfaces.get(&win)
            && surface.is_maximized() != maximized
        {
            // Fail before any other write so policy's rollback starts from a
            // backend that still matches the previous published state.
            surface
                .set_maximized(maximized)
                .map_err(|error| BackendError::Other(Box::new(error)))?;
        }
        if let Some(toplevel) = self.toplevels.get(&win) {
            toplevel.with_pending_state(|s| {
                if maximized {
                    s.states.set(xdg_toplevel::State::Maximized);
                } else {
                    s.states.unset(xdg_toplevel::State::Maximized);
                }
            });
        }
        let writes = [
            (NetWmState::MaximizedVert, axes.vert),
            (NetWmState::MaximizedHorz, axes.horz),
        ];
        for turning_on in [false, true] {
            for (flag, on) in writes {
                if on == turning_on {
                    self.update_foreign_toplevel_net_state(win, flag, on);
                }
            }
        }
        if axes.any() {
            self.window_maximized.insert(win, axes);
        } else {
            self.window_maximized.remove(&win);
        }
        Ok(())
    }

    /// Per-atom `_NET_WM_STATE` write shared by every Wayland backend's
    /// `PropertyOps::set_net_wm_state_flag`. A maximize axis is merged with
    /// the other published axis and goes through `set_window_maximized`, so a
    /// legacy per-axis caller cannot desynchronise the xdg, XWayland and wlr
    /// views of one window.
    pub(crate) fn set_window_net_state(
        &mut self,
        win: WindowId,
        flag: NetWmState,
        on: bool,
    ) -> Result<(), BackendError> {
        if MaximizeAxes::from_net_wm_state(flag).is_some() {
            let current = self.published_maximize_axes(win);
            return self.set_window_maximized(win, current.with_net_wm_state(flag, on));
        }
        self.set_x11_net_state(win, flag, on)?;
        self.update_foreign_toplevel_net_state(win, flag, on);
        Ok(())
    }

    /// Per-atom `_NET_WM_STATE` read shared by every Wayland backend's
    /// `PropertyOps::has_net_wm_state_flag`. Maximize axes come from the
    /// policy cache (xdg has no per-axis state to read back); without an
    /// entry, XWayland's own atoms answer, so adoption still sees a state the
    /// client set before it was managed.
    pub(crate) fn has_window_net_state(&self, win: WindowId, flag: NetWmState) -> bool {
        let cached = match flag {
            NetWmState::MaximizedVert => self.window_maximized.get(&win).map(|axes| axes.vert),
            NetWmState::MaximizedHorz => self.window_maximized.get(&win).map(|axes| axes.horz),
            _ => None,
        };
        cached.unwrap_or_else(|| self.has_x11_net_state(win, flag))
    }

    /// Axes currently published for `win`: the policy cache, or XWayland's
    /// own pair for a window policy has not maximized yet.
    fn published_maximize_axes(&self, win: WindowId) -> MaximizeAxes {
        MaximizeAxes::new(
            self.has_window_net_state(win, NetWmState::MaximizedVert),
            self.has_window_net_state(win, NetWmState::MaximizedHorz),
        )
    }

    /// Send the configure for an xdg toplevel whose pending state policy just
    /// staged. This is the only send path of `WindowOps::configure`, so it is
    /// also where a set_maximized/unset_maximized reply owed to the client is
    /// paid: an owed window always gets a full configure, even when the
    /// pending state equals what was last sent (a refusal or a no-op).
    /// Otherwise `always` (nested backends) or a missing initial configure
    /// forces a send, and udev sends only real changes. Returns whether a
    /// configure went out; `false` for windows that are not xdg toplevels.
    pub(crate) fn send_toplevel_configure(&mut self, win: WindowId, always: bool) -> bool {
        // Take the mark before borrowing the toplevel: the reply is paid by
        // this configure whatever it carries.
        let owed = self.xdg_state_reply_owed.remove(&win);
        let Some(toplevel) = self.try_lookup_toplevel(win) else {
            return false;
        };
        if always || owed || !toplevel.is_initial_configure_sent() {
            toplevel.send_configure();
            true
        } else {
            toplevel.send_pending_configure().is_some()
        }
    }

    /// Answer every set_maximized/unset_maximized that shared policy dropped
    /// without configuring the window (unknown to policy, or a request that
    /// raced its destruction). Run loops call this right after draining the
    /// pending events, so policy has already had its chance to reply through
    /// `send_toplevel_configure`. Returns whether any configure was sent.
    pub(crate) fn flush_owed_xdg_state_replies(&mut self) -> bool {
        if self.xdg_state_reply_owed.is_empty() {
            return false;
        }
        let mut sent = false;
        for win in std::mem::take(&mut self.xdg_state_reply_owed) {
            if let Some(toplevel) = self.toplevels.get(&win) {
                toplevel.send_configure();
                sent = true;
            }
        }
        sent
    }

    pub(crate) fn raise_window(&mut self, win: WindowId) -> Result<(), BackendError> {
        if let Some(surface) = self.x11_surfaces.get(&win).cloned() {
            let xwm = self.x11_wm.as_mut().ok_or_else(|| {
                BackendError::Message("XWayland surface has no active X11 window manager".into())
            })?;
            xwm.raise_window(&surface)
                .map_err(|error| BackendError::Other(Box::new(error)))?;
        }
        if let Some(pos) = self.window_stack.iter().position(|window| *window == win) {
            self.window_stack.remove(pos);
            self.window_stack.push(win);
        }
        self.needs_redraw = true;
        Ok(())
    }

    /// Return the latest commit generation observed for a window's surface
    /// tree. `None` is possible for legacy XWayland association paths whose
    /// first commit predated the WindowId mapping; callers must retain a
    /// bounded fallback retry for that case.
    // The compatibility path `backend::wayland_udev::state` re-exports this
    // module, so both public paths name the same state type.
    #[allow(dead_code)]
    pub(crate) fn surface_commit_epoch(&self, win: WindowId) -> Option<u64> {
        self.surface_commit_epochs.get(&win).copied()
    }

    fn committed_surface_window(&self, surface: &WlSurface) -> Option<WindowId> {
        let mut candidate = Some(surface.clone());
        // Wayland surface trees cannot contain cycles. Keep a defensive bound
        // so malformed internal state can never turn a client commit into an
        // unbounded walk.
        for _ in 0..64 {
            let current = candidate?;
            if let Some(win) = self.surface_to_window.get(&current.id()).copied() {
                return Some(win);
            }
            candidate = get_parent(&current);
        }
        None
    }

    fn note_surface_tree_commit(&mut self, surface: &WlSurface) {
        let Some(win) = self.committed_surface_window(surface) else {
            return;
        };
        let epoch = self.surface_commit_epochs.entry(win).or_default();
        *epoch = epoch.saturating_add(1);
    }

    fn forget_surface_commit_epoch(&mut self, win: WindowId) {
        self.surface_commit_epochs.remove(&win);
    }

    pub(crate) fn set_toplevel_tiled_state(state: &mut ToplevelState, tiled: bool) {
        let edges = [
            xdg_toplevel::State::TiledLeft,
            xdg_toplevel::State::TiledRight,
            xdg_toplevel::State::TiledTop,
            xdg_toplevel::State::TiledBottom,
        ];
        for edge in edges {
            if tiled {
                state.states.set(edge);
            } else {
                state.states.unset(edge);
            }
        }
    }

    fn surface_window_geometry_loc(&self, surface: &WlSurface) -> Point<i32, Logical> {
        // xdg_surface.set_window_geometry sets this. When non-zero, the compositor must shift the
        // wl_surface buffer origin by -loc so the window-geometry aligns with the WM's x/y.
        with_states(surface, |states| {
            let mut cached = states.cached_state.get::<SurfaceCachedState>();
            cached
                .current()
                .geometry
                .map(|r| r.loc)
                .unwrap_or_else(|| (0, 0).into())
        })
    }

    pub(crate) fn surface_window_geometry_rect(
        &self,
        surface: &WlSurface,
    ) -> Option<Rectangle<i32, Logical>> {
        with_states(surface, |states| {
            let mut cached = states.cached_state.get::<SurfaceCachedState>();
            cached.current().geometry
        })
    }

    pub(crate) fn is_dialog_like_toplevel(&self, win: WindowId) -> bool {
        let has_parent = self
            .toplevels
            .get(&win)
            .and_then(|toplevel| toplevel.parent())
            .is_some();
        if has_parent {
            return true;
        }

        let has_dialog_hint = self
            .window_type_overrides
            .get(&win)
            .is_some_and(|types| types.contains(&WindowType::Dialog));

        let Some(app_id) = self.window_app_id.get(&win) else {
            return false;
        };
        if app_id.is_empty() {
            return false;
        }

        let has_same_app_peer = self.mapped_windows.iter().any(|peer| {
            *peer != win
                && self
                    .window_app_id
                    .get(peer)
                    .is_some_and(|peer_app_id| peer_app_id.eq_ignore_ascii_case(app_id))
        });
        if has_dialog_hint && Self::should_honor_dialog_hint(has_parent) {
            return true;
        }

        let Some(activation_app_id) = self.window_activation_app_id.get(&win) else {
            return false;
        };
        if !activation_app_id.eq_ignore_ascii_case(app_id) {
            return false;
        }

        has_same_app_peer
    }

    fn should_honor_dialog_hint(has_parent: bool) -> bool {
        // Some toolkits mark independent first windows as xdg-dialog-v1 dialogs.
        // Without an explicit parent, the hint is too weak: forge/frost expose
        // it on normal independent terminals, so repeated launches must tile.
        has_parent
    }

    fn toplevel_buffer_origin(&self, win: WindowId) -> Option<Point<i32, Logical>> {
        let geo = self.window_geometry.get(&win).copied()?;
        let surface = self.surface_for_window(win)?;
        let offset = self.surface_window_geometry_loc(&surface);
        Some((geo.x - offset.x, geo.y - offset.y).into())
    }

    fn popup_buffer_origin(
        &self,
        win: WindowId,
        popup_surface: &WlSurface,
        popup_rect: Rectangle<i32, Logical>,
    ) -> Option<Point<i32, Logical>> {
        // `popup_rect.loc` is the window-geometry origin of the popup in global coords.
        // Convert it to the actual buffer origin by subtracting the committed geometry loc.
        let _ = win;
        let offset = self.surface_window_geometry_loc(popup_surface);
        Some((popup_rect.loc.x - offset.x, popup_rect.loc.y - offset.y).into())
    }

    pub fn ensure_dmabuf_global(
        &mut self,
        display_handle: &DisplayHandle,
        formats: impl IntoIterator<Item = DmabufFormat>,
    ) {
        if self.dmabuf_global.is_some() {
            return;
        }

        let global = self
            .dmabuf_state
            .create_global::<JwmWaylandState>(display_handle, formats);
        self.dmabuf_global = Some(global);
        info!("[udev/wayland] linux-dmabuf global created");
    }

    pub fn ensure_dmabuf_global_with_feedback(
        &mut self,
        display_handle: &DisplayHandle,
        render_formats: impl IntoIterator<Item = DmabufFormat>,
        scanout_formats: impl IntoIterator<Item = DmabufFormat>,
        main_device: libc::dev_t,
    ) {
        use smithay::wayland::dmabuf::DmabufFeedbackBuilder;
        use smithay::reexports::wayland_protocols::wp::linux_dmabuf::zv1::server::zwp_linux_dmabuf_feedback_v1::TrancheFlags;

        let render_fmts: Vec<DmabufFormat> = render_formats.into_iter().collect();
        let scanout_fmts: Vec<DmabufFormat> = scanout_formats.into_iter().collect();

        // Stash for ext-image-copy-capture dmabuf advertising. Keep this fresh
        // across KMS rebuilds even when the Wayland global already exists.
        self.dmabuf_main_device = Some(main_device);
        self.dmabuf_render_formats = render_fmts.clone();

        if self.dmabuf_global.is_some() {
            return;
        }

        match DmabufFeedbackBuilder::new(main_device, render_fmts.iter().copied())
            .add_preference_tranche(
                main_device,
                TrancheFlags::Scanout,
                scanout_fmts.iter().copied(),
                4u32..=6,
            )
            .build()
        {
            Ok(default_feedback) => {
                let global = self
                    .dmabuf_state
                    .create_global_with_default_feedback::<JwmWaylandState>(
                        display_handle,
                        &default_feedback,
                    );
                self.dmabuf_global = Some(global);
                info!(
                    "[udev/wayland] linux-dmabuf global created with feedback (render={}, scanout={})",
                    render_fmts.len(),
                    scanout_fmts.len()
                );
            }
            Err(e) => {
                warn!(
                    "[udev/wayland] dmabuf feedback build failed: {e:?}, falling back to basic global"
                );
                let global = self
                    .dmabuf_state
                    .create_global::<JwmWaylandState>(display_handle, render_fmts);
                self.dmabuf_global = Some(global);
            }
        }
    }
}

delegate_dispatch2!(JwmWaylandState);

// ---------------------------------------------------------------------------
// Pointer Constraints Handler – pointer lock/confine for games
// ---------------------------------------------------------------------------
impl PointerConstraintsHandler for JwmWaylandState {
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        if let Some(win) = self.surface_to_window.get(&surface.id()).copied() {
            if self.active_toplevel == Some(win) {
                with_pointer_constraint(surface, pointer, |constraint| {
                    if let Some(constraint) = constraint {
                        if !constraint.is_active() {
                            constraint.activate();
                        }
                    }
                });
            }
        }
    }

    fn remove_constraint(
        &mut self,
        _surface: &WlSurface,
        _pointer: &PointerHandle<Self>,
        _constraint_remove: smithay::wayland::pointer_constraints::ConstraintRemove,
    ) {
    }

    fn cursor_position_hint(
        &mut self,
        surface: &WlSurface,
        _pointer: &PointerHandle<Self>,
        location: Point<f64, Logical>,
    ) {
        if let Some(win) = self.surface_to_window.get(&surface.id()).copied() {
            if let Some(geo) = self.window_geometry.get(&win) {
                self.pointer_location =
                    (geo.x as f64 + location.x, geo.y as f64 + location.y).into();
                self.pending_pointer_warp = Some(self.pointer_location);
                self.needs_redraw = true;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Session Lock Handler – screen locker support
// ---------------------------------------------------------------------------
impl SessionLockHandler for JwmWaylandState {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.session_lock_state
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        info!("[udev/wayland] session lock requested");
        // Smithay hands over every request, even while another client holds
        // the lock. Confirming it would make this client the owner, whose
        // `unlock_and_destroy` Smithay then accepts: any client could unlock
        // a live swaylock without its password. Only one live locker exists;
        // dropping `confirmation` refuses the newcomer with `finished` and
        // stops Smithay from routing its lock surfaces to `new_surface`.
        if self.session_lock_owner_alive() {
            warn!("[udev/wayland] refused a session lock: another locker is alive");
            return;
        }
        // The previous locker died without unlocking (`LockStatus::Defunct`)
        // or gave its request up: the spec lets a new client take over
        // without an `unlock`. Drop stale surfaces so the new locker owns
        // every output cleanly.
        self.active_session_lock = None;
        self.lock_surfaces.clear();
        self.session_locked = true;
        self.session_lock_epoch = self.session_lock_epoch.wrapping_add(1);
        self.pending_events
            .lock_safe()
            .retain(|event| !matches!(event, BackendEvent::KeyPress { .. }));
        self.needs_redraw = true;

        // `locked` is owed only once no unlocked content is visible: the
        // render loops report each output's first presented locked frame.
        // A pending request replaced here was abandoned, so its `finished`
        // reaches no one.
        let epoch = self.session_lock_epoch;
        let owed_outputs = self
            .outputs
            .iter()
            .map(Output::name)
            .filter(|name| !self.soft_disabled_outputs.contains(name))
            .collect();
        self.pending_session_lock = Some(PendingSessionLock {
            locker: confirmation,
            epoch,
            owed_outputs,
        });
        // An output that never presents must not leave the lock unconfirmed.
        let deadline = Timer::from_duration(SESSION_LOCK_CONFIRM_DEADLINE);
        if let Err(error) = self
            .loop_handle
            .insert_source(deadline, move |_, _, state| {
                state.confirm_session_lock_after_deadline(epoch);
                TimeoutAction::Drop
            })
        {
            warn!("[udev/wayland] could not arm the session lock deadline: {error}");
            self.confirm_session_lock_after_deadline(epoch);
            return;
        }
        // No output to wait for: nothing unlocked can be on screen.
        self.settle_pending_session_lock();
    }

    fn unlock(&mut self) {
        info!("[udev/wayland] session unlocked");
        self.session_locked = false;
        self.lock_surfaces.clear();
        self.active_session_lock = None;
        // A request still waiting for its first locked frame is refused with
        // `finished`: the session it wanted to lock is gone.
        self.pending_session_lock = None;
        self.needs_redraw = true;
    }

    fn new_surface(&mut self, surface: LockSurface, output: WlOutput) {
        // Find the matching Output to learn its size; default to (0,0) which
        // tells the client to pick its own size. ext-session-lock configures
        // in surface-local (logical) units, the same rectangle `surface_under`
        // hit-tests and the renderer scales, not the physical mode size.
        let output = Output::from_resource(&output);
        let output_name = output
            .as_ref()
            .map(|o| o.name())
            .unwrap_or_else(|| "unknown".to_string());
        let (w, h) = output
            .and_then(|o| {
                let mode = o.current_mode()?;
                Some(output_logical_size(
                    mode.size,
                    o.current_scale().fractional_scale(),
                    o.current_transform(),
                ))
            })
            .map(|size| (size.w.max(0) as u32, size.h.max(0) as u32))
            .unwrap_or((0, 0));

        // Configure the surface to the output size.
        surface.with_pending_state(|state| {
            state.size = Some((w, h).into());
        });
        surface.send_configure();

        info!(
            "[udev/wayland] session lock surface registered ({}x{})",
            w, h
        );
        self.lock_surfaces.insert(output_name, surface);
        self.needs_redraw = true;
    }
}

impl JwmWaylandState {
    /// Bring the protocol state that follows the output layout up to date.
    /// Backends call it after they republished `outputs`, `gamma_sizes` and
    /// `soft_disabled_outputs` or changed an output's mode, scale or
    /// transform: output-management heads are re-sent, stale gamma controls
    /// are failed, lock surfaces take the new output size, and a pending
    /// session lock stops waiting on outputs that are gone.
    pub(crate) fn refresh_output_dependent_state(&mut self) {
        // kanshi and wlr-randr learn about hotplugged heads and about the
        // outcome of their own Apply (the handler refreshes before the ack).
        let heads_changed = self.output_management.as_ref().is_some_and(|management| {
            management.refresh(
                &self.display_handle,
                &self.outputs,
                &self.soft_disabled_outputs,
            )
        });
        if heads_changed {
            self.request_client_flush();
        }
        if crate::backend::wayland_udev::gamma_control::fail_stale_controls(self) > 0 {
            self.request_client_flush();
        }
        self.reconfigure_lock_surfaces();
        self.settle_pending_session_lock();
        // A rebuilt output (VT switch back, re-plugged monitor) is a new
        // `Output` of the same connector. Workspace groups must follow it, or
        // a taskbar's newly bound wl_output never gets `output_enter`.
        self.publish_workspace_monitors();
    }

    /// Publish JWM's monitors and their active tags to ext-workspace
    /// managers (waybar and other taskbars). `monitors` is the list
    /// `CompositorWorkspaceEffects::compositor_set_monitors` receives after
    /// every monitor or tag change.
    ///
    /// The workspace count is re-read from the configuration each time: a
    /// config reload can change `tags_length`, and the groups taskbars hold
    /// must follow it (the protocol re-sends a group whose count differs)
    /// instead of keeping the count the global was created with.
    pub(crate) fn sync_workspace_monitors(&mut self, monitors: &[(u32, i32, i32, u32, u32, u32)]) {
        let tags_length = crate::config::CONFIG.load().tags_length();
        let sent = self.workspace_state.as_ref().is_some_and(|workspaces| {
            workspaces.set_tags_length(tags_length);
            workspaces.sync_monitors(&self.display_handle, &self.outputs, monitors)
        });
        if sent {
            self.request_client_flush();
        }
    }

    /// Re-send the monitors policy last published against the current
    /// outputs, following each to the output of the same connector name.
    /// Unchanged groups send nothing, so repeating it is cheap.
    ///
    /// Not by origin: the backend moves an output (a wlr-randr or kanshi
    /// Apply, a rebuild replaying their positions) before policy hears of
    /// the new layout, so the origins policy last published are stale here.
    /// Matching them would swap two swapped outputs' groups, or retire a
    /// moved output's group, until policy publishes again.
    ///
    /// The one exception is a policy monitor left without a group: it is
    /// matched by origin to an output no group follows, so a hotplugged or
    /// re-plugged connector gets its group on the refresh after the KMS
    /// rebuild (see `WorkspaceState::rebind_outputs`).
    fn publish_workspace_monitors(&self) {
        let tags_length = crate::config::CONFIG.load().tags_length();
        let sent = self.workspace_state.as_ref().is_some_and(|workspaces| {
            workspaces.set_tags_length(tags_length);
            workspaces.rebind_outputs(&self.display_handle, &self.outputs)
        });
        if sent {
            self.request_client_flush();
        }
    }

    /// Whether a live client holds the session lock or waits for its
    /// `locked` event. A lock object is dead once its client disconnected or
    /// destroyed an unconfirmed request.
    fn session_lock_owner_alive(&self) -> bool {
        self.active_session_lock
            .as_ref()
            .is_some_and(Resource::is_alive)
            || self
                .pending_session_lock
                .as_ref()
                .is_some_and(|pending| pending.locker.ext_session_lock().is_alive())
    }

    /// Whether a session lock request is still waiting for its `locked`
    /// event.
    // Only the DRM/KMS loop polls it; the nested hosts show every output.
    #[cfg(any(test, feature = "backend-wayland-udev"))]
    pub(crate) fn session_lock_confirmation_pending(&self) -> bool {
        self.pending_session_lock.is_some()
    }

    /// A frame rendered while the session was locked (lock generation
    /// `epoch`) reached the screen of `output_name`. Once every output owed
    /// one, the pending lock request is confirmed.
    pub(crate) fn note_locked_frame_presented(&mut self, output_name: &str, epoch: u64) {
        let Some(pending) = self.pending_session_lock.as_mut() else {
            return;
        };
        if pending.epoch != epoch {
            return;
        }
        pending.owed_outputs.remove(output_name);
        self.settle_pending_session_lock();
    }

    /// Stop waiting on owed outputs for which `lit` is false: a backend that
    /// knows an output is powered off, or that none of its content is on
    /// screen, reports it here, since such an output shows nothing unlocked
    /// and will not present a frame to confirm with.
    #[cfg(any(test, feature = "backend-wayland-udev"))]
    pub(crate) fn release_session_lock_outputs(&mut self, mut lit: impl FnMut(&str) -> bool) {
        let Some(pending) = self.pending_session_lock.as_mut() else {
            return;
        };
        pending.owed_outputs.retain(|name| lit(name));
        self.settle_pending_session_lock();
    }

    /// Confirm the pending lock once no output owes a locked frame. Owed
    /// outputs that were unplugged or soft-disabled since are forgiven. A
    /// request whose client already destroyed it is dropped without a
    /// confirmation; the session stays locked, as for a locker that dies
    /// after `locked`, until a new locker takes over.
    fn settle_pending_session_lock(&mut self) {
        let Some(pending) = self.pending_session_lock.as_mut() else {
            return;
        };
        if !pending.locker.ext_session_lock().is_alive() {
            warn!("[udev/wayland] session lock abandoned before it was confirmed");
            self.pending_session_lock = None;
            return;
        }
        let outputs = &self.outputs;
        let soft_disabled = &self.soft_disabled_outputs;
        pending.owed_outputs.retain(|name| {
            !soft_disabled.contains(name) && outputs.iter().any(|output| output.name() == *name)
        });
        if pending.owed_outputs.is_empty() {
            self.confirm_pending_session_lock();
        }
    }

    /// Timer callback for [`SESSION_LOCK_CONFIRM_DEADLINE`]: confirm lock
    /// generation `epoch` even though some output never presented a locked
    /// frame. A no-op once that request was confirmed, replaced or unlocked.
    fn confirm_session_lock_after_deadline(&mut self, epoch: u64) {
        let Some(pending) = self.pending_session_lock.as_ref() else {
            return;
        };
        if pending.epoch != epoch {
            return;
        }
        if !pending.locker.ext_session_lock().is_alive() {
            self.settle_pending_session_lock();
            return;
        }
        warn!(
            "[udev/wayland] confirming the session lock without a locked frame on {:?}",
            pending.owed_outputs
        );
        self.confirm_pending_session_lock();
    }

    fn confirm_pending_session_lock(&mut self) {
        let Some(pending) = self.pending_session_lock.take() else {
            return;
        };
        info!("[udev/wayland] session lock confirmed");
        self.active_session_lock = Some(pending.locker.ext_session_lock().clone());
        pending.locker.lock();
        self.request_client_flush();
    }

    /// Re-send every lock surface the size of its output. Lock surfaces are
    /// configured once at creation, so a mode, scale or transform change (or
    /// a rebuilt output) while locked left them sized for the old output.
    /// Smithay only sends a configure whose size actually changed.
    pub(crate) fn reconfigure_lock_surfaces(&mut self) {
        if self.lock_surfaces.is_empty() {
            return;
        }
        let mut sent = false;
        for output in &self.outputs {
            let Some(lock_surface) = self.lock_surfaces.get(&output.name()) else {
                continue;
            };
            if !lock_surface.alive() {
                continue;
            }
            let Some(mode) = output.current_mode() else {
                continue;
            };
            let size = output_logical_size(
                mode.size,
                output.current_scale().fractional_scale(),
                output.current_transform(),
            );
            let size = (size.w.max(0) as u32, size.h.max(0) as u32);
            lock_surface.with_pending_state(|state| {
                state.size = Some(size.into());
            });
            lock_surface.send_configure();
            sent = true;
        }
        if sent {
            self.needs_redraw = true;
            self.request_client_flush();
        }
    }

    /// Ask the backend to flush client connections.
    fn request_client_flush(&self) {
        if !self.client_flush_pending.swap(true, Ordering::SeqCst) {
            let _ = self.client_flush_tx.send(());
        }
    }
}

// ---------------------------------------------------------------------------
// Idle Inhibit Handler – video players prevent idle/screensaver
// ---------------------------------------------------------------------------
impl IdleInhibitHandler for JwmWaylandState {
    fn inhibit(&mut self, surface: WlSurface) {
        debug!("[udev/wayland] idle inhibit activated");
        *self
            .idle_inhibiting_surfaces
            .entry(surface.id())
            .or_default() += 1;
        self.idle_notifier_state.set_is_inhibited(true);
    }

    fn uninhibit(&mut self, surface: WlSurface) {
        debug!("[udev/wayland] idle inhibit released");
        let id = surface.id();
        if let Some(count) = self.idle_inhibiting_surfaces.get_mut(&id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.idle_inhibiting_surfaces.remove(&id);
            }
        }
        self.sync_idle_inhibited();
    }
}

impl JwmWaylandState {
    /// Drop every inhibitor of a surface that is gone. Smithay never calls
    /// `uninhibit` for an inhibitor that dies with its client or outlives its
    /// surface, so without this idle (auto-lock, DPMS) stays off for good.
    fn forget_idle_inhibiting_surface(&mut self, surface: &ObjectId) {
        if self.idle_inhibiting_surfaces.remove(surface).is_some() {
            self.sync_idle_inhibited();
        }
    }

    fn sync_idle_inhibited(&mut self) {
        self.idle_notifier_state
            .set_is_inhibited(!self.idle_inhibiting_surfaces.is_empty());
    }
}

// ---------------------------------------------------------------------------
// Fractional Scale Handler
// ---------------------------------------------------------------------------
impl FractionalScaleHandler for JwmWaylandState {
    fn new_fractional_scale(
        &mut self,
        surface: smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) {
        // Deliver an initial preferred scale so HiDPI clients render at the
        // right resolution instead of being upscaled (blurry). We default to the
        // primary output's scale here; the per-window map path refines it for the
        // output the window actually lands on.
        let scale = self
            .outputs
            .first()
            .map(|o| o.current_scale().fractional_scale())
            .unwrap_or(1.0);
        with_states(&surface, |states| {
            with_fractional_scale(states, |fs| {
                fs.set_preferred_scale(scale);
            });
        });
    }
}

// ---------------------------------------------------------------------------
// Foreign Toplevel List Handler – taskbar/dock integration
// ---------------------------------------------------------------------------
impl ForeignToplevelListHandler for JwmWaylandState {
    fn foreign_toplevel_list_state(&mut self) -> &mut ForeignToplevelListState {
        &mut self.foreign_toplevel_list_state
    }
}

// ---------------------------------------------------------------------------
// Idle Notifier Handler
// ---------------------------------------------------------------------------
impl smithay::wayland::idle_notify::IdleNotifierHandler for JwmWaylandState {
    fn idle_notifier_state(&mut self) -> &mut IdleNotifierState<JwmWaylandState> {
        &mut self.idle_notifier_state
    }
}

// ---------------------------------------------------------------------------
// Keyboard Shortcuts Inhibit Handler
// ---------------------------------------------------------------------------

/// Whether `inhibitor` may ever become active. Smithay has no filter for this
/// global, so a sandboxed client may create inhibitors, but they never take
/// effect: an active one would swallow JWM's own bindings, including the
/// lock and session keys, while the sandboxed window has focus.
fn may_inhibit_shortcuts(inhibitor: &KeyboardShortcutsInhibitor) -> bool {
    !inhibitor
        .wl_surface()
        .client()
        .is_some_and(|client| client_is_sandboxed(&client))
}

impl smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitHandler
    for JwmWaylandState
{
    fn keyboard_shortcuts_inhibit_state(&mut self) -> &mut KeyboardShortcutsInhibitState {
        &mut self.keyboard_shortcuts_inhibit_state
    }

    fn new_inhibitor(&mut self, inhibitor: KeyboardShortcutsInhibitor) {
        if may_inhibit_shortcuts(&inhibitor)
            && self
                .active_toplevel
                .and_then(|win| self.surface_for_window(win))
                .as_ref()
                .is_some_and(|surface| surface.id() == inhibitor.wl_surface().id())
        {
            inhibitor.activate();
        }
    }

    fn inhibitor_destroyed(&mut self, _inhibitor: KeyboardShortcutsInhibitor) {
        self.needs_redraw = true;
    }
}

// ---------------------------------------------------------------------------
// Tablet Seat Handler – drawing tablet support
// ---------------------------------------------------------------------------
impl smithay::input::tablet::TabletSeatHandler for JwmWaylandState {
    type ToolFocus = WlSurface;

    fn tablet_tool_image(
        &mut self,
        _tool: &smithay::backend::input::TabletToolDescriptor,
        _image: smithay::input::pointer::CursorImageStatus,
    ) {
    }
}

// ---------------------------------------------------------------------------
// Security Context Handler – sandboxed app isolation (Flatpak)
// ---------------------------------------------------------------------------
impl smithay::wayland::security_context::SecurityContextHandler for JwmWaylandState {
    fn context_created(
        &mut self,
        source: smithay::wayland::security_context::SecurityContextListenerSource,
        security_context: smithay::wayland::security_context::SecurityContext,
    ) {
        let res = self
            .loop_handle
            .insert_source(source, move |client_stream, _, data| {
                let client_state = Arc::new(JwmClientState {
                    security_context: Some(security_context.clone()),
                    ..JwmClientState::default()
                });
                if let Err(e) = data
                    .display_handle
                    .insert_client(client_stream, client_state)
                {
                    warn!("[udev/wayland] sandboxed insert_client failed: {e:?}");
                }
            });
        if let Err(e) = res {
            warn!("[udev/wayland] failed to listen on security_context socket: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// wp-commit-timing-v1 – frame-perfect scheduling
// ---------------------------------------------------------------------------
// No handler trait needed – CommitTimingManagerState is purely passive.

// ---------------------------------------------------------------------------
// xdg-dialog-v1 – modal dialog hints
// ---------------------------------------------------------------------------
impl XdgDialogHandler for JwmWaylandState {
    fn dialog_hint_changed(&mut self, toplevel: ToplevelSurface, hint: ToplevelDialogHint) {
        if let Some(win) = self
            .surface_to_window
            .get(&toplevel.wl_surface().id())
            .copied()
        {
            match hint {
                ToplevelDialogHint::Dialog | ToplevelDialogHint::Modal => {
                    self.window_type_overrides
                        .insert(win, vec![WindowType::Dialog]);
                }
                ToplevelDialogHint::Unknown => {
                    self.window_type_overrides.remove(&win);
                }
            }
            info!("[udev/wayland] dialog_hint_changed win={win:?} hint={hint:?}");
            self.push_event(BackendEvent::PropertyChanged {
                window: win,
                kind: PropertyKind::WindowType,
            });
        }
        self.needs_redraw = true;
    }
}

// ---------------------------------------------------------------------------
// xdg-foreign-v2 – cross-client parent/child relationships
// ---------------------------------------------------------------------------
impl XdgForeignHandler for JwmWaylandState {
    fn xdg_foreign_state(&mut self) -> &mut XdgForeignState {
        &mut self.xdg_foreign_state
    }
}

// ---------------------------------------------------------------------------
// xdg-system-bell – audible bell notification
// ---------------------------------------------------------------------------
impl XdgSystemBellHandler for JwmWaylandState {
    fn ring(&mut self, _surface: Option<WlSurface>) {
        // Could trigger a visual bell or system beep
    }
}

// ---------------------------------------------------------------------------
// pointer-warp – programmatic pointer movement
// ---------------------------------------------------------------------------
impl PointerWarpHandler for JwmWaylandState {
    fn warp_pointer(
        &mut self,
        surface: WlSurface,
        _pointer: smithay::reexports::wayland_server::protocol::wl_pointer::WlPointer,
        pos: Point<f64, Logical>,
        _serial: Serial,
    ) {
        if let Some(win) = self.surface_to_window.get(&surface.id()).copied() {
            let origin = self
                .toplevel_buffer_origin(win)
                .or_else(|| self.window_geometry.get(&win).map(|g| (g.x, g.y).into()));
            if let Some(origin) = origin {
                self.pointer_location = (origin.x as f64 + pos.x, origin.y as f64 + pos.y).into();
                self.pending_pointer_warp = Some(self.pointer_location);
                self.needs_redraw = true;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// xwayland-keyboard-grab – better XWayland keyboard handling
// ---------------------------------------------------------------------------
impl XWaylandKeyboardGrabHandler for JwmWaylandState {
    fn keyboard_focus_for_xsurface(
        &self,
        surface: &WlSurface,
    ) -> Option<<Self as SeatHandler>::KeyboardFocus> {
        // Xwayland asks for its own surfaces, which never enter `toplevels`:
        // an associated X11 window's surface is itself the focus to grab.
        let win = self.surface_to_window.get(&surface.id())?;
        if self.x11_surfaces.contains_key(win) || self.x11_wl_surfaces.contains_key(win) {
            return Some(surface.clone());
        }
        self.toplevels.get(win).map(|t| t.wl_surface().clone())
    }
}

// ---------------------------------------------------------------------------
// wp-linux-drm-syncobj-v1 – explicit sync for NVIDIA
// ---------------------------------------------------------------------------
impl DrmSyncobjHandler for JwmWaylandState {
    fn drm_syncobj_state(&mut self) -> Option<&mut DrmSyncobjState> {
        self.drm_syncobj_state.as_mut()
    }
}

impl XdgToplevelIconHandler for JwmWaylandState {}

impl XdgToplevelTagHandler for JwmWaylandState {}

impl DataControlHandler for JwmWaylandState {
    fn data_control_state(&mut self) -> &mut DataControlState {
        &mut self.data_control_state
    }
}

impl ExtDataControlHandler for JwmWaylandState {
    fn data_control_state(&mut self) -> &mut ExtDataControlState {
        &mut self.ext_data_control_state
    }
}

impl KdeDecorationHandler for JwmWaylandState {
    fn kde_decoration_state(&self) -> &KdeDecorationState {
        &self.kde_decoration_state
    }
}

// ---------------------------------------------------------------------------
// XDG Activation Handler – allows clients to request surface activation
// ---------------------------------------------------------------------------
impl XdgActivationHandler for JwmWaylandState {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation_state
    }

    fn token_created(&mut self, _token: XdgActivationToken, _data: XdgActivationTokenData) -> bool {
        // Smithay keeps every token until the compositor removes it. Tokens
        // that were never used expire here, bounding the pool without a timer.
        self.xdg_activation_state
            .retain_tokens(|_, data| data.timestamp.elapsed() < XDG_ACTIVATION_TOKEN_LIFETIME);
        true
    }

    fn request_activation(
        &mut self,
        token: XdgActivationToken,
        token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        // A token activates once: consume it on every path so it can neither
        // be replayed nor pin its app_id, seat and surface handles.
        self.xdg_activation_state.remove_token(&token);
        if token_data.timestamp.elapsed() < XDG_ACTIVATION_TOKEN_LIFETIME {
            // Find the window that corresponds to this surface and activate it.
            if let Some(&win_id) = self.surface_to_window.get(&surface.id()) {
                debug!(
                    "[xdg_activation] activating window {:?} (app_id={:?})",
                    win_id, token_data.app_id
                );
                if let Some(app_id) = token_data.app_id.as_deref() {
                    self.window_activation_app_id
                        .insert(win_id, app_id.to_string());
                }
                // Activation is a policy request: revealing another tag,
                // restoring a minimized window, focus and stacking must take
                // the same path as `_NET_ACTIVE_WINDOW` and foreign-toplevel.
                self.request_window_activation(win_id);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// XWayland Shell Handler – associates X11 windows with Wayland surfaces
// ---------------------------------------------------------------------------
impl XWaylandShellHandler for JwmWaylandState {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }

    fn surface_associated(&mut self, _xwm: XwmId, wl_surface: WlSurface, window: X11Surface) {
        let x11_id = window.window_id();
        debug!(
            "[xwayland] surface_associated: x11={} wl={:?} title={:?}",
            x11_id,
            wl_surface.id(),
            window.title(),
        );

        if let Some(win_id) = self.x11_surface_to_window.get(&x11_id).copied() {
            self.surface_to_window.insert(wl_surface.id(), win_id);
            self.needs_redraw = true;
        } else {
            self.pending_x11_wl_surfaces.insert(x11_id, wl_surface);
        }
    }
}

/// Map Smithay's `XwmResizeEdge` onto the `_NET_WM_MOVERESIZE` direction
/// codes used by the shared window-manager policy (0..=7 resize, 8 move).
fn xwm_resize_edge_direction(edge: XwmResizeEdge) -> u32 {
    match edge {
        XwmResizeEdge::TopLeft => 0,
        XwmResizeEdge::Top => 1,
        XwmResizeEdge::TopRight => 2,
        XwmResizeEdge::Right => 3,
        XwmResizeEdge::BottomRight => 4,
        XwmResizeEdge::Bottom => 5,
        XwmResizeEdge::BottomLeft => 6,
        XwmResizeEdge::Left => 7,
    }
}

// ---------------------------------------------------------------------------
// XWM Handler – manages X11 windows running under XWayland
// ---------------------------------------------------------------------------
impl XwmHandler for JwmWaylandState {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.x11_wm.as_mut().expect("X11Wm not yet started")
    }

    fn new_window(&mut self, _xwm: XwmId, window: X11Surface) {
        debug!(
            "[xwayland] new_window: id={} title={:?} class={:?} override_redirect={}",
            window.window_id(),
            window.title(),
            window.class(),
            window.is_override_redirect(),
        );
    }

    fn new_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        debug!(
            "[xwayland] new_override_redirect_window: id={} class={:?}",
            window.window_id(),
            window.class(),
        );
    }

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let iconic = window.is_hidden();
        info!(
            "[xwayland] map_window_request: id={} title={:?} class={:?} iconic={}",
            window.window_id(),
            window.title(),
            window.class(),
            iconic,
        );

        // Grant the map request. Smithay seeds `_NET_WM_STATE_HIDDEN` before
        // this callback when WmHints initial state is Iconic; `set_mapped`
        // then writes ICCCM IconicState when that atom is present.
        if let Err(e) = window.set_mapped(true) {
            warn!("[xwayland] set_mapped(true) failed: {e:?}");
            return;
        }
        // Re-assert Hidden after map so a client that raced Normal hints
        // still lands Iconic; harmless when already Hidden.
        if iconic && let Err(e) = window.set_hidden(true) {
            warn!("[xwayland] set_hidden(true) for Iconic map failed: {e:?}");
        }

        // Send a configure with the requested geometry (or a reasonable default).
        let geo = window.geometry();
        let w = if geo.size.w > 0 {
            geo.size.w as u32
        } else {
            800
        };
        let h = if geo.size.h > 0 {
            geo.size.h as u32
        } else {
            600
        };
        let _ = window.configure(Some(smithay::utils::Rectangle::new(
            (geo.loc.x, geo.loc.y).into(),
            (w as i32, h as i32).into(),
        )));

        // Allocate a WindowId and track the surface.
        let win_id = self.alloc_window_id();
        let x11_id = window.window_id();
        self.x11_surface_to_window.insert(x11_id, win_id);
        self.x11_surfaces.insert(win_id, window.clone());

        if let Some(wl_surface) = self.pending_x11_wl_surfaces.remove(&x11_id) {
            self.surface_to_window.insert(wl_surface.id(), win_id);
        }
        self.window_geometry.insert(
            win_id,
            Geometry {
                x: geo.loc.x,
                y: geo.loc.y,
                w,
                h,
                border: 0,
            },
        );
        self.window_title.insert(win_id, window.title());
        self.window_app_id.insert(win_id, window.class());
        self.window_is_fullscreen
            .insert(win_id, window.is_fullscreen());
        self.window_stack.push(win_id);

        // Managed X11 windows (Steam, Wine) belong in taskbars exactly like
        // xdg toplevels; override-redirect menus and tooltips never do.
        // Unmap/destroy send `closed` through `remove_window`.
        if let Some(ref ftm) = self.foreign_toplevel_mgmt {
            crate::backend::wayland_udev::foreign_toplevel_management::announce_new_toplevel(
                &self.display_handle,
                ftm,
                win_id,
                &window.title(),
                &window.class(),
            );
        }

        // Iconic starts must still be managed (WindowCreated → on_map_request)
        // so JWM can adopt them as minimized; keep them out of the compositor
        // draw set until the manager explicitly maps/restores them.
        // Record the manager's hidden state too: an early buffer commit must
        // not map an Iconic window before JWM processes the creation event.
        self.set_manager_window_mapped(win_id, !iconic);

        self.push_event(BackendEvent::WindowCreated(win_id));
        self.push_event(BackendEvent::WindowMapped(win_id));
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        info!(
            "[xwayland] mapped_override_redirect: id={} class={:?}",
            window.window_id(),
            window.class(),
        );

        // Override-redirect windows (menus, tooltips, etc.) are managed separately.
        let win_id = self.alloc_window_id();
        let x11_id = window.window_id();
        self.x11_surface_to_window.insert(x11_id, win_id);
        self.x11_surfaces.insert(win_id, window.clone());

        if let Some(wl_surface) = self.pending_x11_wl_surfaces.remove(&x11_id) {
            self.surface_to_window.insert(wl_surface.id(), win_id);
        }

        let geo = window.geometry();
        self.window_geometry.insert(
            win_id,
            Geometry {
                x: geo.loc.x,
                y: geo.loc.y,
                w: geo.size.w.max(1) as u32,
                h: geo.size.h.max(1) as u32,
                border: 0,
            },
        );
        self.window_title.insert(win_id, window.title());
        self.window_app_id.insert(win_id, window.class());
        self.window_is_fullscreen.insert(win_id, false);
        self.window_stack.push(win_id);

        self.mapped_windows.insert(win_id);
        self.needs_redraw = true;

        self.push_event(BackendEvent::WindowCreated(win_id));
        self.push_event(BackendEvent::WindowMapped(win_id));
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        let x11_id = window.window_id();
        info!("[xwayland] unmapped_window: id={}", x11_id);

        if let Some(win_id) = self.x11_surface_to_window.remove(&x11_id) {
            self.forget_surface_commit_epoch(win_id);
            self.x11_surfaces.remove(&win_id);
            self.x11_wl_surfaces.remove(&win_id);
            let was_managed = self.take_window_mapping(win_id);
            self.surface_to_window.retain(|_, w| *w != win_id);
            self.retire_window_geometry(win_id);
            self.window_stack.retain(|w| *w != win_id);
            self.window_title.remove(&win_id);
            self.window_app_id.remove(&win_id);
            self.window_is_fullscreen.remove(&win_id);
            self.window_maximized.remove(&win_id);
            self.xdg_state_reply_owed.remove(&win_id);
            self.window_border_color.remove(&win_id);
            // A remap allocates a new WindowId and announces it afresh.
            if let Some(ref ftm) = self.foreign_toplevel_mgmt {
                ftm.remove_window(win_id);
            }

            self.needs_redraw = true;

            if was_managed {
                self.compositor_dead_windows.push(win_id.raw());
                self.push_event(BackendEvent::WindowUnmapped {
                    window: win_id,
                    from_configure: false,
                });
            }
        }
    }

    fn destroyed_window(&mut self, _xwm: XwmId, window: X11Surface) {
        let x11_id = window.window_id();
        info!("[xwayland] destroyed_window: id={}", x11_id);

        if let Some(win_id) = self.x11_surface_to_window.remove(&x11_id) {
            self.forget_surface_commit_epoch(win_id);
            self.x11_surfaces.remove(&win_id);
            self.x11_wl_surfaces.remove(&win_id);
            self.take_window_mapping(win_id);
            self.surface_to_window.retain(|_, w| *w != win_id);
            self.retire_window_geometry(win_id);
            self.window_stack.retain(|w| *w != win_id);
            self.window_title.remove(&win_id);
            self.window_app_id.remove(&win_id);
            self.window_is_fullscreen.remove(&win_id);
            self.window_maximized.remove(&win_id);
            self.xdg_state_reply_owed.remove(&win_id);
            self.window_border_color.remove(&win_id);
            if let Some(ref ftm) = self.foreign_toplevel_mgmt {
                ftm.remove_window(win_id);
            }

            self.needs_redraw = true;

            self.compositor_dead_windows.push(win_id.raw());
            self.push_event(BackendEvent::WindowDestroyed(win_id));
        }
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        let x11_id = window.window_id();
        debug!(
            "[xwayland] configure_request: id={} x={:?} y={:?} w={:?} h={:?}",
            x11_id, x, y, w, h
        );

        let managed =
            self.x11_surface_to_window
                .get(&x11_id)
                .map(|&win_id| ManagedXwaylandWindow {
                    window: win_id,
                    maximized: window.is_maximized(),
                    configured: self.window_geometry.get(&win_id).copied(),
                });
        match route_xwayland_configure_request(managed, window.geometry(), x, y, w, h) {
            XwaylandConfigureRoute::Policy {
                window: win_id,
                mask_bits,
                changes,
            } => {
                // Same contract as the X11 backends: policy keeps a tiled or
                // fullscreen client in its slot and replies through
                // WindowOps::configure, which reaches `X11Surface::configure`.
                self.push_event(BackendEvent::ConfigureRequest {
                    window: win_id,
                    changes,
                    mask_bits,
                });
            }
            XwaylandConfigureRoute::Grant(rect) | XwaylandConfigureRoute::Reply(rect) => {
                let _ = window.configure(Some(rect));
            }
        }

        self.needs_redraw = true;
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
        let x11_id = window.window_id();
        if let Some(win_id) = self.x11_surface_to_window.get(&x11_id).copied() {
            self.window_geometry.insert(
                win_id,
                Geometry {
                    x: geometry.loc.x,
                    y: geometry.loc.y,
                    w: geometry.size.w.max(1) as u32,
                    h: geometry.size.h.max(1) as u32,
                    border: 0,
                },
            );
        }
        self.needs_redraw = true;
    }

    fn resize_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        button: u32,
        resize_edge: XwmResizeEdge,
    ) {
        // Feed the existing Jwm `_NET_WM_MOVERESIZE` drag pipeline (same
        // direction encoding Smithay already decoded from the client message).
        let x11_id = window.window_id();
        let Some(win_id) = self.x11_surface_to_window.get(&x11_id).copied() else {
            return;
        };
        self.push_event(BackendEvent::MoveResizeRequest {
            window: win_id,
            direction: xwm_resize_edge_direction(resize_edge),
            button,
        });
    }

    fn move_request(&mut self, _xwm: XwmId, window: X11Surface, button: u32) {
        let x11_id = window.window_id();
        let Some(win_id) = self.x11_surface_to_window.get(&x11_id).copied() else {
            return;
        };
        self.push_event(BackendEvent::MoveResizeRequest {
            window: win_id,
            direction: 8, // _NET_WM_MOVERESIZE_MOVE
            button,
        });
    }

    fn property_notify(&mut self, _xwm: XwmId, window: X11Surface, property: WmWindowProperty) {
        let x11_id = window.window_id();
        if let Some(win_id) = self.x11_surface_to_window.get(&x11_id).copied() {
            match property {
                WmWindowProperty::Title => {
                    let title = window.title();
                    if let Some(ref ftm) = self.foreign_toplevel_mgmt {
                        ftm.update_title(win_id, &title);
                    }
                    self.window_title.insert(win_id, title);
                    self.push_event(BackendEvent::PropertyChanged {
                        window: win_id,
                        kind: PropertyKind::Title,
                    });
                }
                WmWindowProperty::Class => {
                    let class = window.class();
                    if let Some(ref ftm) = self.foreign_toplevel_mgmt {
                        ftm.update_app_id(win_id, &class);
                    }
                    self.window_app_id.insert(win_id, class);
                    self.push_event(BackendEvent::PropertyChanged {
                        window: win_id,
                        kind: PropertyKind::Class,
                    });
                }
                // Smithay now parses `_MOTIF_WM_HINTS`; forward so JWM can drop
                // SSD borders for Steam/Electron/etc. XWayland clients the same
                // way the native X11 backends already do.
                WmWindowProperty::MotifHints => {
                    self.push_event(BackendEvent::PropertyChanged {
                        window: win_id,
                        kind: PropertyKind::MotifHints,
                    });
                }
                _ => {}
            }
        }
    }

    fn fullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.request_x11_state(window.window_id(), NetWmState::Fullscreen, true);
    }

    // Smithay raises these only for a `_NET_WM_STATE` message naming the
    // MAXIMIZED_HORZ+VERT pair, already gated on `is_maximized()`.
    fn maximize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.request_x11_maximize(window.window_id(), true);
    }

    fn unmaximize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.request_x11_maximize(window.window_id(), false);
    }

    fn minimize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.request_x11_minimized(window.window_id(), true);
    }

    fn unminimize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.request_x11_minimized(window.window_id(), false);
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.request_x11_state(window.window_id(), NetWmState::Fullscreen, false);
    }

    fn active_window_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        _timestamp: u32,
        _currently_active_window: Option<X11Surface>,
    ) {
        self.request_x11_activation(window.window_id());
    }

    fn above_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.request_x11_state(window.window_id(), NetWmState::Above, true);
    }

    fn unabove_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.request_x11_state(window.window_id(), NetWmState::Above, false);
    }

    fn below_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.request_x11_state(window.window_id(), NetWmState::Below, true);
    }

    fn unbelow_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.request_x11_state(window.window_id(), NetWmState::Below, false);
    }

    fn allow_selection_access(&mut self, _xwm: XwmId, _selection: SelectionTarget) -> bool {
        // Permit X11 clients to read the Wayland selection. jwm tracks keyboard
        // focus as a bare WlSurface, so we cannot cheaply assert the focused
        // window is X11; allow access so the clipboard bridge works in practice.
        true
    }

    fn send_selection(
        &mut self,
        _xwm: XwmId,
        selection: SelectionTarget,
        mime_type: String,
        fd: std::os::fd::OwnedFd,
    ) {
        self.send_selection_to_xwayland(selection, mime_type, fd);
    }

    fn new_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        self.adopt_xwayland_selection(selection, mime_types);
    }

    fn cleared_selection(&mut self, _xwm: XwmId, selection: SelectionTarget) {
        self.xwayland_selection_cleared(selection);
    }
}

// The XwmHandler selection callbacks, kept outside the trait impl because a
// test cannot construct the `XwmId` the trait methods take.
impl JwmWaylandState {
    /// An X11 client asked for the Wayland-side selection.
    fn send_selection_to_xwayland(
        &mut self,
        selection: SelectionTarget,
        mime_type: String,
        fd: std::os::fd::OwnedFd,
    ) {
        match selection {
            SelectionTarget::Clipboard => {
                // JWM's own offer is a compositor-owned selection, which
                // `request_data_device_client_selection` refuses; serve it
                // here the way `SelectionHandler::send_selection` does.
                if let Some(offer) = self.clipboard_offered.as_ref() {
                    if let Some(payload) = selection_payload_for_mime(offer, &mime_type) {
                        write_selection_async(payload.to_vec(), fd);
                    }
                    return;
                }
                if let Err(err) = request_data_device_client_selection(&self.seat, mime_type, fd) {
                    warn!("Failed to request Wayland clipboard for Xwayland: {err:?}");
                }
            }
            SelectionTarget::Primary => {
                if let Err(err) = request_primary_client_selection(&self.seat, mime_type, fd) {
                    warn!("Failed to request Wayland primary selection for Xwayland: {err:?}");
                }
            }
        }
    }

    /// An X11 client took the selection.
    fn adopt_xwayland_selection(&mut self, selection: SelectionTarget, mime_types: Vec<String>) {
        match selection {
            SelectionTarget::Clipboard => {
                // Smithay's `set_data_device_selection` does not run
                // `SelectionHandler::new_selection`, so the history entry JWM
                // was offering must be retired here. Otherwise
                // `send_selection` keeps serving it to Wayland pastes.
                self.clipboard_offered = None;
                set_data_device_selection(&self.display_handle, &self.seat, mime_types, ());
            }
            SelectionTarget::Primary => {
                set_primary_selection(&self.display_handle, &self.seat, mime_types, ());
            }
        }
    }

    /// The X11 client owning the selection went away.
    fn xwayland_selection_cleared(&mut self, selection: SelectionTarget) {
        match selection {
            SelectionTarget::Clipboard => {
                // JWM's offer replaced that X11 owner on the seat; clearing
                // now would drop JWM's live offer, not the X11 selection.
                if self.clipboard_offered.is_some() {
                    return;
                }
                if current_data_device_selection_userdata(&self.seat).is_some() {
                    clear_data_device_selection(&self.display_handle, &self.seat);
                }
            }
            SelectionTarget::Primary => {
                if current_primary_selection_userdata(&self.seat).is_some() {
                    clear_primary_selection(&self.display_handle, &self.seat);
                }
            }
        }
    }
}

impl JwmWaylandState {
    pub fn record_protocol_bind(&mut self, protocol: &'static str) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        let entry = self
            .protocol_bind_counts
            .entry(protocol)
            .or_insert_with(|| crate::backend::api::ProtocolBindStatus {
                protocol: protocol.to_string(),
                bind_count: 0,
                last_bound_unix_ms: None,
            });
        entry.bind_count = entry.bind_count.saturating_add(1);
        entry.last_bound_unix_ms = Some(now);
    }

    pub fn protocol_bind_counts_snapshot(&self) -> Vec<crate::backend::api::ProtocolBindStatus> {
        let mut counts: Vec<_> = self.protocol_bind_counts.values().cloned().collect();
        counts.sort_by(|a, b| a.protocol.cmp(&b.protocol));
        counts
    }

    fn sync_keyboard_shortcuts_inhibitors(
        &mut self,
        previous: Option<WindowId>,
        active: Option<WindowId>,
    ) {
        let active_surface = active.and_then(|win| self.surface_for_window(win));
        if let Some(prev_win) = previous {
            if Some(prev_win) != active {
                if let Some(surface) = self.surface_for_window(prev_win) {
                    if let Some(inhibitor) =
                        self.seat.keyboard_shortcuts_inhibitor_for_surface(&surface)
                    {
                        if inhibitor.is_active() {
                            inhibitor.inactivate();
                        }
                    }
                }
            }
        }

        if let Some(surface) = active_surface {
            if let Some(inhibitor) = self.seat.keyboard_shortcuts_inhibitor_for_surface(&surface) {
                if !inhibitor.is_active() && may_inhibit_shortcuts(&inhibitor) {
                    inhibitor.activate();
                }
            }
        }
    }

    pub fn set_active_toplevel(&mut self, win: Option<WindowId>) {
        if self.active_toplevel == win {
            return;
        }

        let debug_focus = std::env::var("JWM_DEBUG_FOCUS")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        let prev = self.active_toplevel.take();
        self.sync_keyboard_shortcuts_inhibitors(prev, win);
        if let Some(ref foreign_toplevel_mgmt) = self.foreign_toplevel_mgmt {
            use crate::backend::wayland_udev::foreign_toplevel_management::StateFlag;
            if let Some(prev_win) = prev {
                foreign_toplevel_mgmt.update_state(prev_win, StateFlag::Activated, false);
            }
            if let Some(new_win) = win {
                foreign_toplevel_mgmt.update_state(new_win, StateFlag::Activated, true);
            }
        }
        if debug_focus {
            info!("[udev/focus] active_toplevel {:?} -> {:?}", prev, win);
        }

        if let Some(prev_win) = prev {
            if let Some(toplevel) = self.toplevels.get(&prev_win).cloned() {
                let size = self
                    .window_geometry
                    .get(&prev_win)
                    .map(|g| (g.w as i32, g.h as i32).into());
                let tiled = !self.is_dialog_like_toplevel(prev_win);
                toplevel.with_pending_state(|s| {
                    s.states.unset(xdg_toplevel::State::Activated);
                    // Preserve the configured size. smithay clears s.size after each
                    // send_configure, so omitting this sends configure(0,0) which tells
                    // GTK4 to choose its own natural size — shrinking the status bar.
                    if let Some(sz) = size {
                        s.size = Some(sz);
                    }
                    Self::set_toplevel_tiled_state(s, tiled);
                });
                toplevel.send_pending_configure();
            }
        }

        self.active_toplevel = win;
        if let Some(new_win) = win {
            let size = self
                .window_geometry
                .get(&new_win)
                .map(|g| (g.w as i32, g.h as i32).into());
            let tiled = !self.is_dialog_like_toplevel(new_win);
            if let Some(toplevel) = self.toplevels.get(&new_win).cloned() {
                toplevel.with_pending_state(|s| {
                    s.states.set(xdg_toplevel::State::Activated);
                    if let Some(sz) = size {
                        s.size = Some(sz);
                    }
                    Self::set_toplevel_tiled_state(s, tiled);
                });
                toplevel.send_pending_configure();
            }
        }
    }

    pub fn update_foreign_toplevel_net_state(
        &mut self,
        win: WindowId,
        state: NetWmState,
        on: bool,
    ) {
        use crate::backend::wayland_udev::foreign_toplevel_management::StateFlag;

        let flag = match state {
            NetWmState::Hidden => StateFlag::Minimized,
            NetWmState::MaximizedHorz => StateFlag::MaximizedHorz,
            NetWmState::MaximizedVert => StateFlag::MaximizedVert,
            _ => return,
        };
        if let Some(ref foreign_toplevel_mgmt) = self.foreign_toplevel_mgmt {
            foreign_toplevel_mgmt.update_state(win, flag, on);
        }
    }

    pub fn update_foreign_toplevel_fullscreen(&mut self, win: WindowId, on: bool) {
        use crate::backend::wayland_udev::foreign_toplevel_management::StateFlag;

        if let Some(ref foreign_toplevel_mgmt) = self.foreign_toplevel_mgmt {
            foreign_toplevel_mgmt.update_state(win, StateFlag::Fullscreen, on);
        }
    }

    pub fn init(
        dh: &DisplayHandle,
        handle: smithay::reexports::calloop::LoopHandle<'static, JwmWaylandState>,
        pending_events: Arc<Mutex<std::collections::VecDeque<BackendEvent>>>,
        flush_tx: Sender<()>,
        flush_pending: Arc<AtomicBool>,
        seat_name: String,
        listen_on_socket: bool,
        frame_capture_supported: bool,
    ) -> Result<(Self, Option<String>), Box<dyn std::error::Error + Send + Sync>> {
        let socket_name = if listen_on_socket {
            let source = ListeningSocketSource::new_auto()?;
            let socket_name = source.socket_name().to_string_lossy().into_owned();
            let accept_flush_tx = flush_tx.clone();
            let accept_flush_pending = flush_pending.clone();
            handle.insert_source(source, move |client_stream, _, data| {
                match data
                    .display_handle
                    .insert_client(client_stream, Arc::new(JwmClientState::default()))
                {
                    Ok(client) => {
                        info!("[udev/wayland] client connected: {client:?}");
                        if !accept_flush_pending.swap(true, Ordering::SeqCst) {
                            let _ = accept_flush_tx.send(());
                        }
                    }
                    Err(e) => {
                        warn!("[udev/wayland] insert_client failed: {e:?}");
                    }
                }
            })?;
            Some(socket_name)
        } else {
            None
        };

        let compositor_state = CompositorState::new::<JwmWaylandState>(dh);
        let shm_state = ShmState::new::<JwmWaylandState>(
            dh,
            vec![wl_shm::Format::Argb8888, wl_shm::Format::Xrgb8888],
        );

        // Toolkits like GTK expect wl_data_device_manager (clipboard/DnD) and often primary
        // selection to be available.
        let data_device_state = DataDeviceState::new::<JwmWaylandState>(dh);
        let primary_selection_state = PrimarySelectionState::new::<JwmWaylandState>(dh);
        let xdg_shell_state = XdgShellState::new::<JwmWaylandState>(dh);
        let xdg_decoration_state = XdgDecorationState::new::<JwmWaylandState>(dh);
        let viewporter_state = ViewporterState::new::<JwmWaylandState>(dh);

        let dmabuf_state = DmabufState::new();

        // Layer surfaces stack above every window and may take exclusive
        // keyboard focus, which lets a sandbox fake a password prompt.
        let layer_shell_state =
            WlrLayerShellState::new_with_filter::<JwmWaylandState, _>(dh, client_is_unsandboxed);
        let xdg_activation_state = XdgActivationState::new::<JwmWaylandState>(dh);

        let xwayland_shell_state = XWaylandShellState::new::<JwmWaylandState>(dh);

        let cfg = crate::config::CONFIG.load();
        let behavior = cfg.behavior();

        // Extra desktop/tooling protocols are useful, but clients enumerate and
        // bind globals before creating toplevels. Keep them individually
        // toggleable while locating early-client native faults.
        // Frame-capture globals are only advertised when the backend can also
        // service them: today only the DRM/KMS frame pipeline drains the
        // pending capture queues. Advertising them on the nested development
        // backends made capture clients (grim etc.) hang forever instead of
        // failing with a clear "protocol not supported" error.
        let screencopy_pending = if frame_capture_supported
            && optional_global_enabled(behavior.wayland_enable_screencopy, "JWM_ENABLE_SCREENCOPY")
        {
            // wlr-screencopy-unstable-v1 – allows grim and similar tools to capture screen content.
            Some(crate::backend::wayland_udev::screencopy::init_screencopy_manager(dh))
        } else {
            None
        };

        let tearing_hints = if optional_global_enabled(
            behavior.wayland_enable_tearing_control,
            "JWM_ENABLE_TEARING_CONTROL",
        ) {
            // wp-tearing-control-v1 – allows games to opt into async page flips.
            Some(crate::backend::wayland_udev::tearing_control::init_tearing_control_manager(dh))
        } else {
            None
        };

        let color_manager = if optional_global_enabled(
            behavior.wayland_enable_color_management,
            "JWM_ENABLE_COLOR_MANAGEMENT",
        ) {
            // wp-color-management-v1 – HDR / color-space surface metadata.
            Some(crate::backend::wayland_udev::color_management::init_color_management(dh))
        } else {
            None
        };

        // Same rule as the capture globals: only the DRM/KMS run loop services
        // `BackendEvent::OutputConfigure` and pops the ack an Apply queues.
        // On the nested backends nothing ever answered an Apply, so
        // wlr-randr/kanshi blocked forever and the ack queue grew per Apply.
        let output_management = if frame_capture_supported
            && optional_global_enabled(
                behavior.wayland_enable_output_management,
                "JWM_ENABLE_OUTPUT_MANAGEMENT",
            ) {
            // wlr-output-management-unstable-v1 – output config for kanshi/wlr-randr.
            Some(crate::backend::wayland_udev::output_management::init_output_management(dh))
        } else {
            None
        };

        if optional_global_enabled(
            behavior.wayland_enable_output_power,
            "JWM_ENABLE_OUTPUT_POWER",
        ) {
            // wlr-output-power-management-unstable-v1 – DPMS for swayidle.
            crate::backend::wayland_udev::output_power::init_output_power_management(dh);
        }

        let workspace_state =
            if optional_global_enabled(behavior.wayland_enable_workspace, "JWM_ENABLE_WORKSPACE") {
                // ext-workspace-v1 – workspace/tag state for taskbars (Waybar etc.).
                Some(
                    crate::backend::wayland_udev::workspace_protocol::init_workspace_protocol(
                        dh,
                        cfg.tags_length(),
                    ),
                )
            } else {
                None
            };

        let image_capture_pending = if frame_capture_supported
            && optional_global_enabled(
                behavior.wayland_enable_image_copy_capture,
                "JWM_ENABLE_IMAGE_COPY_CAPTURE",
            ) {
            // ext-image-copy-capture-v1 – modern screen capture (replaces wlr-screencopy).
            Some(crate::backend::wayland_udev::image_copy_capture::init_image_copy_capture(dh))
        } else {
            None
        };

        let gamma_owners = if optional_global_enabled(
            behavior.wayland_enable_gamma_control,
            "JWM_ENABLE_GAMMA_CONTROL",
        ) {
            // wlr-gamma-control-unstable-v1 – night light (gammastep/wlsunset).
            Some(crate::backend::wayland_udev::gamma_control::init_gamma_control(dh))
        } else {
            None
        };

        let foreign_toplevel_mgmt = if optional_global_enabled(
            behavior.wayland_enable_foreign_toplevel_management,
            "JWM_ENABLE_FOREIGN_TOPLEVEL_MANAGEMENT",
        ) {
            // wlr-foreign-toplevel-management-unstable-v1 – taskbar window control.
            Some(crate::backend::wayland_udev::foreign_toplevel_management::init_foreign_toplevel_management(dh))
        } else {
            None
        };

        if optional_global_enabled(
            behavior.wayland_enable_virtual_pointer,
            "JWM_ENABLE_VIRTUAL_POINTER",
        ) {
            // wlr-virtual-pointer-unstable-v1 – remote desktop pointer injection.
            crate::backend::wayland_udev::virtual_pointer::init_virtual_pointer_manager(dh);
        }

        if !env_flag("JWM_OPTIONAL_GLOBALS") {
            info!(
                "[udev/wayland] optional globals are config-gated; set behavior.wayland_enable_* or JWM_OPTIONAL_GLOBALS=1 to enable all"
            );
        }

        // Optional but very useful for toolkit compatibility.
        let output_manager_state = OutputManagerState::new_with_xdg_output::<JwmWaylandState>(dh);

        // IME / text input support – required for Chinese / Japanese / Korean input.
        TextInputManagerState::new::<JwmWaylandState>(dh);
        // Privileged globals (input injection, clipboard monitoring, session
        // lock, minting security contexts) are hidden from sandboxed clients:
        // advertising wp_security_context promises that isolation.
        InputMethodManagerState::new::<JwmWaylandState, _>(dh, client_is_unsandboxed);
        VirtualKeyboardManagerState::new::<JwmWaylandState, _>(dh, client_is_unsandboxed);

        // --- SOTA protocols ---
        let pointer_constraints_state = PointerConstraintsState::new::<JwmWaylandState>(dh);
        let relative_pointer_state = RelativePointerManagerState::new::<JwmWaylandState>(dh);
        let session_lock_state =
            SessionLockManagerState::new::<JwmWaylandState, _>(dh, client_is_unsandboxed);
        let idle_inhibit_state = IdleInhibitManagerState::new::<JwmWaylandState>(dh);
        let idle_notifier_state = IdleNotifierState::<JwmWaylandState>::new(&dh, handle.clone());
        let fractional_scale_state = FractionalScaleManagerState::new::<JwmWaylandState>(dh);
        let cursor_shape_state = CursorShapeManagerState::new::<JwmWaylandState>(dh);
        let presentation_state =
            PresentationState::new::<JwmWaylandState>(dh, libc::CLOCK_MONOTONIC as u32);
        let pointer_gestures_state = PointerGesturesState::new::<JwmWaylandState>(dh);
        let single_pixel_buffer_state = SinglePixelBufferState::new::<JwmWaylandState>(dh);
        let content_type_state = ContentTypeState::new::<JwmWaylandState>(dh);
        let alpha_modifier_state = AlphaModifierState::new::<JwmWaylandState>(dh);
        // Every window's title and app_id, the same stream the
        // wlr-foreign-toplevel filter keeps from sandboxes.
        let foreign_toplevel_list_state =
            ForeignToplevelListState::new_with_filter::<JwmWaylandState>(dh, client_is_unsandboxed);
        let tablet_manager_state = TabletManagerState::new::<JwmWaylandState>(dh);
        // Use unmanaged mode for commit-pacing protocols. Smithay's managed
        // mode installs pre-commit blockers; without deeper transaction
        // scheduling integration those blockers can deadlock Vulkan/wgpu
        // clients that chain wp-fifo and wp-commit-timing commits.
        let fifo_state = FifoManagerState::unmanaged::<JwmWaylandState>(dh);
        let keyboard_shortcuts_inhibit_state =
            KeyboardShortcutsInhibitState::new::<JwmWaylandState>(dh);
        // A sandboxed client must not mint a nested context of its own.
        let security_context_state =
            SecurityContextState::new::<JwmWaylandState, _>(dh, client_is_unsandboxed);
        let commit_timing_state = CommitTimingManagerState::unmanaged::<JwmWaylandState>(dh);
        let xdg_dialog_state = XdgDialogState::new::<JwmWaylandState>(dh);
        let xdg_foreign_state = XdgForeignState::new::<JwmWaylandState>(dh);
        let xdg_system_bell_state = XdgSystemBellState::new::<JwmWaylandState>(dh);
        let pointer_warp_state = PointerWarpManager::new::<JwmWaylandState>(dh);
        let xwayland_keyboard_grab_state = XWaylandKeyboardGrabState::new::<JwmWaylandState>(dh);
        XdgToplevelIconManager::new::<JwmWaylandState>(dh);
        XdgToplevelTagManager::new::<JwmWaylandState>(dh);
        let data_control_state = DataControlState::new::<JwmWaylandState, _>(
            dh,
            Some(&primary_selection_state),
            client_is_unsandboxed,
        );
        let ext_data_control_state = ExtDataControlState::new::<JwmWaylandState, _>(
            dh,
            Some(&primary_selection_state),
            client_is_unsandboxed,
        );
        let kde_decoration_state = KdeDecorationState::new::<JwmWaylandState>(dh, KdeMode::Server);
        // ext-background-effect-v1: advertise the global so clients can request
        // background blur regions. The region is stored per-surface in
        // BackgroundEffectSurfaceCachedState; the GL compositor's frosted-glass
        // system can read it during rendering. GlobalId may be dropped — the
        // global persists in the Display.
        BackgroundEffectState::new::<JwmWaylandState>(dh);

        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(dh, seat_name);
        seat.add_pointer();
        seat.add_keyboard(XkbConfig::default(), 200, 25)?;
        seat.add_touch();

        Ok((
            Self {
                display_handle: dh.clone(),
                clipboard_captured: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                clipboard_capture_generation: 0,
                clipboard_delivered_generation: 0,
                clipboard_offered: None,
                clipboard_pending: None,
                loop_handle: handle.clone(),
                pending_events,
                compositor_dead_windows: Vec::new(),

                pointer_location: (0.0, 0.0).into(),
                pending_pointer_warp: None,
                needs_redraw: true,
                dnd_icon: None,

                output_manager_state,
                compositor_state,
                shm_state,
                data_device_state,
                primary_selection_state,
                seat_state,
                seat,
                xdg_shell_state,
                xdg_decoration_state,
                viewporter_state,

                dmabuf_state,
                dmabuf_global: None,
                dmabuf_main_device: None,
                dmabuf_render_formats: Vec::new(),

                layer_shell_state,
                xdg_activation_state,

                pointer_constraints_state,
                relative_pointer_state,
                session_lock_state,
                idle_inhibit_state,
                idle_notifier_state,
                fractional_scale_state,
                cursor_shape_state,
                presentation_state,
                pointer_gestures_state,
                single_pixel_buffer_state,
                content_type_state,
                alpha_modifier_state,
                foreign_toplevel_list_state,
                tablet_manager_state,
                fifo_state,
                keyboard_shortcuts_inhibit_state,
                security_context_state,
                commit_timing_state,
                xdg_dialog_state,
                xdg_foreign_state,
                xdg_system_bell_state,
                pointer_warp_state,
                xwayland_keyboard_grab_state,
                drm_syncobj_state: None,
                data_control_state,
                ext_data_control_state,
                kde_decoration_state,

                idle_inhibiting_surfaces: HashMap::new(),
                last_input: std::time::Instant::now(),
                session_locked: false,
                session_lock_epoch: 0,
                lock_surfaces: HashMap::new(),
                pending_session_lock: None,
                active_session_lock: None,
                foreign_toplevel_handles: HashMap::new(),
                gesture_swipe: GestureSwipeTracker::default(),

                xwayland_shell_state,
                x11_wm: None,
                x11_surface_to_window: HashMap::new(),
                x11_surfaces: HashMap::new(),
                pending_x11_wl_surfaces: HashMap::new(),
                x11_wl_surfaces: HashMap::new(),
                active_toplevel: None,

                outputs: Vec::new(),
                protocol_bind_counts: HashMap::new(),
                pending_output_acks: std::collections::VecDeque::new(),
                soft_disabled_outputs: HashSet::new(),
                last_output_management_rejection: None,
                gamma_sizes: HashMap::new(),
                next_window_raw: 1,
                toplevels: HashMap::new(),
                layer_surfaces: HashMap::new(),
                surface_to_window: HashMap::new(),
                surface_commit_epochs: HashMap::new(),

                pending_initial_configure: HashSet::new(),
                pending_size_reconfigure: HashMap::new(),

                popups: HashMap::new(),
                popup_order: Vec::new(),

                im_popups: Vec::new(),
                im_client_id: None,
                im_popup_warned: Mutex::new(HashSet::new()),

                popup_grab_toplevel: None,
                popup_grab_prev_kbd_focus: None,

                output_rects: Vec::new(),

                window_geometry: HashMap::new(),
                window_stack: Vec::new(),

                mapped_windows: HashSet::new(),
                manager_unmapped_windows: HashSet::new(),
                window_title: HashMap::new(),
                window_app_id: HashMap::new(),
                window_activation_app_id: HashMap::new(),
                window_is_fullscreen: HashMap::new(),
                window_maximized: HashMap::new(),
                xdg_state_reply_owed: HashSet::new(),
                window_type_overrides: HashMap::new(),

                window_layer_info: HashMap::new(),

                window_border_color: HashMap::new(),

                screencopy_pending,
                tearing_hints,

                workspace_state,

                image_capture_pending,
                toplevel_capture_sessions: HashMap::new(),

                capture_counters: Arc::new(Mutex::new(CaptureCounters::default())),

                foreign_toplevel_mgmt,

                color_manager,

                output_management,

                gamma_owners,

                client_flush_tx: flush_tx,
                client_flush_pending: flush_pending,
            },
            socket_name,
        ))
    }
    fn ensure_initial_configure_fallback(&mut self, win: WindowId) {
        // The ordinary WindowOps::configure path removes this token. A
        // one-shot fallback that fires afterwards is therefore a constant-time
        // no-op instead of a perpetual scan over every surface.
        if !claim_initial_configure_fallback(&mut self.pending_initial_configure, win) {
            return;
        }

        let Some(toplevel) = self.toplevels.get(&win).cloned() else {
            return;
        };
        if toplevel.is_initial_configure_sent() {
            return;
        }

        let (w, h) = self
            .window_geometry
            .get(&win)
            .map(|g| (g.w, g.h))
            .unwrap_or((800, 600));
        // Decide before `with_pending_state`: it holds this surface's state
        // mutex for the closure, and `is_dialog_like_toplevel` reads the
        // parent through the same non-reentrant mutex. WindowOps::configure
        // hoists the same check for the same reason.
        let choose_natural_size = self.is_dialog_like_toplevel(win) && w == 800 && h == 600;
        toplevel.with_pending_state(|s| {
            if choose_natural_size {
                s.size = None;
                Self::set_toplevel_tiled_state(s, false);
            } else {
                s.size = Some((w as i32, h as i32).into());
                Self::set_toplevel_tiled_state(s, true);
            }
        });
        let _ = toplevel.send_configure();
        self.needs_redraw = true;
    }

    pub(crate) fn enforce_toplevel_configure_size(&mut self, win: WindowId, surface: &WlSurface) {
        if self.is_dialog_like_toplevel(win) {
            self.pending_size_reconfigure.remove(&win);
            return;
        }

        let Some(expected) = self.window_geometry.get(&win).copied() else {
            self.pending_size_reconfigure.remove(&win);
            return;
        };
        let Some(committed) = self.surface_window_geometry_rect(surface) else {
            return;
        };

        let expected_size = (expected.w.max(1), expected.h.max(1));
        let committed_size = (
            committed.size.w.max(1) as u32,
            committed.size.h.max(1) as u32,
        );

        let close_enough = committed_size.0.abs_diff(expected_size.0) <= 4
            && committed_size.1.abs_diff(expected_size.1) <= 4;
        if committed_size == expected_size || close_enough {
            self.pending_size_reconfigure.remove(&win);
            return;
        }

        if let Some((pending_size, last_sent)) = self.pending_size_reconfigure.get(&win).copied() {
            if pending_size == expected_size && last_sent.elapsed() < Duration::from_millis(100) {
                return;
            }
        }

        let Some(toplevel) = self.toplevels.get(&win).cloned() else {
            return;
        };

        toplevel.with_pending_state(|state| {
            state.size = Some((expected_size.0 as i32, expected_size.1 as i32).into());
            Self::set_toplevel_tiled_state(state, true);
        });
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        } else {
            toplevel.send_configure();
        }
        self.pending_size_reconfigure
            .insert(win, (expected_size, Instant::now()));
        self.send_surface_frame_callbacks_now(surface);
        self.needs_redraw = true;
        debug!(
            "[udev/wayland] reconfigure size mismatch win={win:?} committed={}x{} expected={}x{}",
            committed_size.0, committed_size.1, expected_size.0, expected_size.1
        );
    }

    fn send_surface_frame_callbacks_now(&mut self, surface: &WlSurface) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or(std::time::Duration::ZERO)
            .as_millis()
            .min(u128::from(u32::MAX)) as u32;

        with_surface_tree_downward(
            surface,
            (),
            |_, _, _| TraversalAction::DoChildren(()),
            |_surface, states, _| {
                let mut cached = states.cached_state.get::<SurfaceAttributes>();
                for callback in cached.current().frame_callbacks.drain(..) {
                    callback.done(now);
                }
            },
            |_, _, _| true,
        );
    }

    /// Preferred fractional scale for the output a window currently occupies,
    /// falling back to the primary output (then 1.0).
    fn preferred_scale_for_window(&self, win: WindowId) -> f64 {
        if let Some(g) = self.window_geometry.get(&win) {
            let center: Point<i32, Logical> = (g.x + g.w as i32 / 2, g.y + g.h as i32 / 2).into();
            for (idx, rect) in self.output_rects.iter().enumerate() {
                if center.x >= rect.loc.x
                    && center.y >= rect.loc.y
                    && center.x < rect.loc.x + rect.size.w
                    && center.y < rect.loc.y + rect.size.h
                {
                    if let Some(o) = self.outputs.get(idx) {
                        return o.current_scale().fractional_scale();
                    }
                }
            }
        }
        self.outputs
            .first()
            .map(|o| o.current_scale().fractional_scale())
            .unwrap_or(1.0)
    }

    pub fn surface_under(
        &self,
        location: Point<f64, Logical>,
    ) -> Option<(Option<WindowId>, WlSurface, Point<f64, Logical>)> {
        if self.session_locked {
            for output in &self.outputs {
                let Some(mode) = output.current_mode() else {
                    continue;
                };
                let logical_size = output_logical_size(
                    mode.size,
                    output.current_scale().fractional_scale(),
                    output.current_transform(),
                );
                let rect = Rectangle::<i32, Logical>::new(output.current_location(), logical_size);
                if rect.to_f64().contains(location) {
                    if let Some(lock_surface) = self.lock_surfaces.get(&output.name()) {
                        // Crashed locker: surface may still be keyed but dead.
                        // Keep the session locked (no passthrough to apps) and
                        // wait for a replacement locker / PAM UI.
                        if !lock_surface.alive() {
                            return None;
                        }
                        let origin: Point<f64, Logical> =
                            (rect.loc.x as f64, rect.loc.y as f64).into();
                        if let Some((surface, surf_loc)) = under_from_surface_tree(
                            lock_surface.wl_surface(),
                            location,
                            rect.loc,
                            WindowSurfaceType::ALL,
                        ) {
                            return Some((
                                None,
                                surface,
                                (surf_loc.x as f64, surf_loc.y as f64).into(),
                            ));
                        }
                        return Some((None, lock_surface.wl_surface().clone(), origin));
                    }
                    return None;
                }
            }

            return None;
        }

        // Layer surfaces should receive input before normal windows.
        for output in &self.outputs {
            let Some(mode) = output.current_mode() else {
                continue;
            };
            let scale = output.current_scale().fractional_scale();
            let logical_size = mode.size.to_f64().to_logical(scale).to_i32_round();
            let logical_size = output.current_transform().transform_size(logical_size);
            let rect = Rectangle::<i32, Logical>::new(output.current_location(), logical_size);
            if !rect.to_f64().contains(location) {
                continue;
            }

            let map = layer_map_for_output(output);

            // Prefer overlay then top layer for hit-testing.
            for layer in [Layer::Overlay, Layer::Top] {
                if let Some(ls) = map.layer_under(layer, location) {
                    if let Some(geo) = map.layer_geometry(ls) {
                        let origin: Point<f64, Logical> =
                            (geo.loc.x as f64, geo.loc.y as f64).into();
                        return Some((None, ls.wl_surface().clone(), origin));
                    }
                }
            }
        }

        if let Some((win, surface, origin)) = self.popup_surface_under(location) {
            return Some((Some(win), surface, origin));
        }

        for win in self.window_stack.iter().rev() {
            if !self.mapped_windows.contains(win) {
                continue;
            }
            let geo = self.window_geometry.get(win)?;
            // Hit test includes border area so clicks on the border count as
            // clicking the window. `geo` stores the content-area origin
            // (x = original_x + bw), so expand outward by `border`.
            let bw = geo.border as f64;
            let x0 = geo.x as f64 - bw;
            let y0 = geo.y as f64 - bw;
            let x1 = geo.x as f64 + geo.w as f64 + bw;
            let y1 = geo.y as f64 + geo.h as f64 + bw;
            if location.x >= x0 && location.y >= y0 && location.x < x1 && location.y < y1 {
                let origin = self
                    .toplevel_buffer_origin(*win)
                    .unwrap_or((geo.x, geo.y).into());
                // For X11 windows, descend into subsurfaces so DnD enter/motion/drop
                // target the correct child surface (xdnd drop targeting).
                if let Some(x11) = self.x11_surfaces.get(win) {
                    if let Some((surface, surf_loc)) =
                        x11.surface_under(location, origin, WindowSurfaceType::ALL)
                    {
                        return Some((
                            Some(*win),
                            surface,
                            (surf_loc.x as f64, surf_loc.y as f64).into(),
                        ));
                    }
                }
                if let Some(surface) = self.surface_for_window(*win) {
                    if let Some((under_surface, surf_loc)) =
                        under_from_surface_tree(&surface, location, origin, WindowSurfaceType::ALL)
                    {
                        return Some((
                            Some(*win),
                            under_surface,
                            (surf_loc.x as f64, surf_loc.y as f64).into(),
                        ));
                    }
                    return Some((
                        Some(*win),
                        surface,
                        (origin.x as f64, origin.y as f64).into(),
                    ));
                }
            }
        }

        None
    }

    pub fn popup_surface_under(
        &self,
        location: Point<f64, Logical>,
    ) -> Option<(WindowId, WlSurface, Point<f64, Logical>)> {
        // Popups are always above their parent toplevel. Prefer them for hit-testing.
        for win in self.window_stack.iter().rev() {
            if !self.mapped_windows.contains(win) {
                continue;
            }

            for (popup_surface, popup_rect) in self.popup_rects_for_toplevel(*win) {
                let x0 = popup_rect.loc.x as f64;
                let y0 = popup_rect.loc.y as f64;
                let x1 = x0 + popup_rect.size.w as f64;
                let y1 = y0 + popup_rect.size.h as f64;
                if location.x >= x0 && location.y >= y0 && location.x < x1 && location.y < y1 {
                    let origin = self
                        .popup_buffer_origin(*win, &popup_surface, popup_rect)
                        .unwrap_or(popup_rect.loc);
                    if let Some((surface, surf_loc)) = under_from_surface_tree(
                        &popup_surface,
                        location,
                        origin,
                        WindowSurfaceType::ALL,
                    ) {
                        return Some((
                            *win,
                            surface,
                            (surf_loc.x as f64, surf_loc.y as f64).into(),
                        ));
                    }
                    return Some((
                        *win,
                        popup_surface,
                        (origin.x as f64, origin.y as f64).into(),
                    ));
                }
            }
        }

        None
    }

    pub fn constrain_pointer_location(
        &self,
        current: Point<f64, Logical>,
        proposed: Point<f64, Logical>,
        pointer: &PointerHandle<Self>,
    ) -> Point<f64, Logical> {
        let Some((_win, surface, origin)) = self.surface_under(current) else {
            return proposed;
        };

        with_pointer_constraint(&surface, pointer, |constraint| {
            let Some(constraint) = constraint else {
                return proposed;
            };
            if !constraint.is_active() {
                return proposed;
            }

            match &*constraint {
                PointerConstraint::Locked(_) => current,
                PointerConstraint::Confined(_) => {
                    if let Some(region) = constraint.region() {
                        let local: Point<i32, Logical> = (
                            (proposed.x - origin.x).round() as i32,
                            (proposed.y - origin.y).round() as i32,
                        )
                            .into();
                        if region.contains(local) {
                            proposed
                        } else {
                            current
                        }
                    } else if self.surface_under(proposed).is_some_and(
                        |(_, proposed_surface, _)| proposed_surface.id() == surface.id(),
                    ) {
                        proposed
                    } else {
                        current
                    }
                }
            }
        })
    }

    fn popup_committed_geometry(popup: &PopupSurface) -> Option<Rectangle<i32, Logical>> {
        popup.with_committed_state(|s| s.map(|st| st.geometry))
    }

    fn popup_root_toplevel(&self, popup: &PopupSurface, depth: u8) -> Option<WindowId> {
        if depth > 16 {
            return None;
        }
        let parent = popup.get_parent_surface()?;
        let parent_id = parent.id();

        if let Some(win) = self.surface_to_window.get(&parent_id).copied() {
            return Some(win);
        }

        let parent_popup = self.popups.get(&parent_id)?;
        self.popup_root_toplevel(parent_popup, depth.saturating_add(1))
    }

    fn popup_global_origin(&self, popup: &PopupSurface, depth: u8) -> Option<Point<i32, Logical>> {
        if depth > 16 {
            return None;
        }
        let geo = Self::popup_committed_geometry(popup)?;

        let parent = popup.get_parent_surface()?;
        let parent_id = parent.id();

        if let Some(win) = self.surface_to_window.get(&parent_id).copied() {
            let parent_geo = self.window_geometry.get(&win)?;
            return Some((parent_geo.x + geo.loc.x, parent_geo.y + geo.loc.y).into());
        }

        let parent_popup = self.popups.get(&parent_id)?;
        let parent_origin = self.popup_global_origin(parent_popup, depth.saturating_add(1))?;
        Some((parent_origin.x + geo.loc.x, parent_origin.y + geo.loc.y).into())
    }

    pub fn dismiss_popups_for_toplevel(&mut self, win: WindowId) {
        // Send popup_done for all popups that belong to this toplevel grab.
        // Clients will unmap/destroy them asynchronously.
        let ids: Vec<ObjectId> = self
            .popup_order
            .iter()
            .filter(|id| {
                self.popups
                    .get(*id)
                    .is_some_and(|p| self.popup_root_toplevel(p, 0) == Some(win))
            })
            .cloned()
            .collect();

        for id in ids {
            if let Some(popup) = self.popups.get(&id) {
                popup.send_popup_done();
            }
        }
    }

    fn unconstrain_popup(&mut self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };
        let Some(win) = self.surface_to_window.get(&root.id()).copied() else {
            return;
        };

        let Some(window_geo) = self.window_geometry.get(&win).copied() else {
            return;
        };
        let window_rect: Rectangle<i32, Logical> = Rectangle::new(
            (window_geo.x, window_geo.y).into(),
            (window_geo.w as i32, window_geo.h as i32).into(),
        );

        let Some(mut outputs_geo) = self.output_rects.first().copied() else {
            return;
        };

        // Prefer constraining to the output that contains the parent toplevel (or pointer).
        // Falling back to the union of all outputs keeps behavior reasonable even if we can't
        // determine a best output.
        let best_output = {
            let window_center: Point<i32, Logical> = (
                window_geo.x + (window_geo.w as i32 / 2),
                window_geo.y + (window_geo.h as i32 / 2),
            )
                .into();

            let pointer: Point<i32, Logical> = (
                self.pointer_location.x.round() as i32,
                self.pointer_location.y.round() as i32,
            )
                .into();

            fn contains(rect: &Rectangle<i32, Logical>, p: Point<i32, Logical>) -> bool {
                p.x >= rect.loc.x
                    && p.y >= rect.loc.y
                    && p.x < rect.loc.x + rect.size.w
                    && p.y < rect.loc.y + rect.size.h
            }

            fn overlap_area(a: Rectangle<i32, Logical>, b: Rectangle<i32, Logical>) -> i64 {
                let x0 = a.loc.x.max(b.loc.x);
                let y0 = a.loc.y.max(b.loc.y);
                let x1 = (a.loc.x + a.size.w).min(b.loc.x + b.size.w);
                let y1 = (a.loc.y + a.size.h).min(b.loc.y + b.size.h);
                let w = (x1 - x0).max(0) as i64;
                let h = (y1 - y0).max(0) as i64;
                w * h
            }

            // 1) contains window center
            self.output_rects
                .iter()
                .find(|r| contains(r, window_center))
                .copied()
                // 2) contains pointer
                .or_else(|| {
                    self.output_rects
                        .iter()
                        .find(|r| contains(r, pointer))
                        .copied()
                })
                // 3) max overlap with parent window rect
                .or_else(|| {
                    self.output_rects
                        .iter()
                        .copied()
                        .max_by_key(|r| overlap_area(*r, window_rect))
                })
        };

        if let Some(rect) = best_output {
            outputs_geo = rect;
        } else {
            for rect in self.output_rects.iter().skip(1) {
                outputs_geo = outputs_geo.merge(*rect);
            }
        }

        // Target geometry for positioner is relative to the parent's window geometry.
        let mut target = outputs_geo;
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        target.loc -= window_rect.loc;

        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }

    pub fn reconstrain_popups_for_toplevel(&mut self, win: WindowId) {
        if self.popups.is_empty() {
            return;
        }

        let popups: Vec<PopupSurface> = self
            .popup_order
            .iter()
            .filter_map(|id| {
                let popup = self.popups.get(id)?.clone();
                (self.popup_root_toplevel(&popup, 0) == Some(win)).then_some(popup)
            })
            .collect();

        for popup in popups {
            self.unconstrain_popup(&popup);
            let _ = popup.send_pending_configure();
        }

        self.needs_redraw = true;
    }

    pub fn popup_rects_for_toplevel(
        &self,
        win: WindowId,
    ) -> Vec<(WlSurface, Rectangle<i32, Logical>)> {
        // Front-to-back order: newest popups first.
        let mut out = Vec::new();

        for id in self.popup_order.iter().rev() {
            let Some(popup) = self.popups.get(id) else {
                continue;
            };
            if self.popup_root_toplevel(popup, 0) != Some(win) {
                continue;
            }

            let Some(geo) = Self::popup_committed_geometry(popup) else {
                continue;
            };
            let Some(origin) = self.popup_global_origin(popup, 0) else {
                continue;
            };

            let rect = Rectangle::<i32, Logical>::new(origin, geo.size);
            out.push((popup.wl_surface().clone(), rect));
        }

        out
    }

    pub fn popup_grab_area(&self, win: WindowId) -> Option<Rectangle<i32, Logical>> {
        // Define a conservative grab area: union of parent toplevel and all its popups.
        // This approximates the "popup grab" region well enough for toolkits.
        let parent_geo = self.window_geometry.get(&win).copied()?;
        let mut area: Rectangle<i32, Logical> = Rectangle::new(
            (parent_geo.x, parent_geo.y).into(),
            (parent_geo.w as i32, parent_geo.h as i32).into(),
        );

        for (_surf, rect) in self.popup_rects_for_toplevel(win) {
            area = area.merge(rect);
        }

        Some(area)
    }

    fn alloc_window_id(&mut self) -> WindowId {
        let id = WindowId::from_raw(self.next_window_raw);
        self.next_window_raw = self.next_window_raw.wrapping_add(1);
        id
    }

    /// Stable `ext-foreign-toplevel-list` identifier for `win`.
    ///
    /// Protocol limit: non-empty, ≤32 printable ASCII. `jwm-` + 16 hex digits
    /// of the `WindowId` raw value fits (20 chars) and stays unique per window.
    pub(crate) fn foreign_toplevel_identifier(win: WindowId) -> String {
        format!("jwm-{:016x}", win.raw())
    }

    /// Convert smithay's parsed Motif hints into JWM's wire-compatible struct
    /// so `MotifWmHints::decorations_none` keeps matching the X11 backends.
    // Only the DRM/KMS backend runs XWayland; the nested ones have no X11
    // surfaces to read Motif hints from.
    #[cfg(any(test, feature = "backend-wayland-udev"))]
    pub(crate) fn motif_wm_hints_from_smithay(
        hints: &smithay::xwayland::xwm::MwmHints,
    ) -> crate::backend::api::MotifWmHints {
        let mut flags = 0u32;
        let mut functions = 0u32;
        let mut decorations = 0u32;
        let mut input_mode = 0i32;
        let mut status = 0u32;
        if let Some(f) = hints.functions {
            flags |= 1 << 0;
            functions = f.bits();
        }
        if let Some(d) = hints.decorations {
            flags |= 1 << 1;
            decorations = d.bits();
        }
        if let Some(mode) = hints.input_mode {
            flags |= 1 << 2;
            input_mode = mode as u32 as i32;
        }
        if let Some(s) = hints.status {
            flags |= 1 << 3;
            status = s.bits();
        }
        crate::backend::api::MotifWmHints {
            flags,
            functions,
            decorations,
            input_mode,
            status,
        }
    }

    pub(crate) fn push_event(&mut self, ev: BackendEvent) {
        self.pending_events.lock_safe().push_back(ev);
    }

    pub fn try_lookup_toplevel(&mut self, win: WindowId) -> Option<&mut ToplevelSurface> {
        self.toplevels.get_mut(&win)
    }

    pub fn surface_for_window(&self, win: WindowId) -> Option<WlSurface> {
        // Try Wayland toplevel first.
        if let Some(t) = self.toplevels.get(&win) {
            return Some(t.wl_surface().clone());
        }
        if let Some(surface) = self.layer_surfaces.get(&win) {
            return Some(surface.clone());
        }
        // Fall back to X11 surface. Prefer the manually-resolved legacy association
        // (XWayland < 23.1, WL_SURFACE_ID path) before smithay's own accessor, which is
        // only populated for the modern xwayland_shell protocol.
        if let Some(s) = self.x11_wl_surfaces.get(&win) {
            return Some(s.clone());
        }
        if let Some(x11) = self.x11_surfaces.get(&win) {
            return x11.wl_surface();
        }
        None
    }

    pub fn hit_test(
        &self,
        location: Point<f64, Logical>,
    ) -> Option<(WindowId, WlSurface, Point<f64, Logical>)> {
        self.surface_under(location)
            .and_then(|(win, surface, origin)| win.map(|w| (w, surface, origin)))
    }

    /// Returns active IME popup surfaces with their absolute (global) position.
    /// Each entry is (wl_surface, x, y).
    pub fn xdg_popup_positions(&self) -> Vec<(WlSurface, i32, i32, u32, u32)> {
        let mut result = Vec::new();
        for id in &self.popup_order {
            let Some(popup) = self.popups.get(id) else {
                continue;
            };
            let Some(geo) = Self::popup_committed_geometry(popup) else {
                continue;
            };
            let Some(origin) = self.popup_global_origin(popup, 0) else {
                continue;
            };
            let w = geo.size.w as u32;
            let h = geo.size.h as u32;
            if w > 0 && h > 0 {
                result.push((popup.wl_surface().clone(), origin.x, origin.y, w, h));
            }
        }
        result
    }

    pub fn im_popup_positions(&self) -> Vec<ImPopupAnchor> {
        let mut result = Vec::new();
        // The render paths call this once per frame, so a persistently broken
        // popup must not warn per frame: each (popup surface, failure kind)
        // pair warns the first time it fails and is then held in
        // `im_popup_warned`, which this call's failures replace at the end —
        // a popup that goes away or whose condition clears may warn again on
        // its next failure.
        let mut warned = self.im_popup_warned.lock_safe();
        #[allow(
            clippy::mutable_key_type,
            reason = "ObjectId hashes by stable protocol-object identity; its internal liveness flag is not part of Hash or Eq"
        )]
        let mut failing = HashSet::new();
        for popup in &self.im_popups {
            // Dead popups are pruned in `new_popup`/`dismiss_popup`; skip silently here.
            if !popup.alive() {
                continue;
            }
            let loc = popup.location();
            let cursor_rect = popup.text_input_rectangle();
            let parent = match popup.get_parent() {
                Some(p) => p,
                None => {
                    let key = (popup.wl_surface().id(), ImPopupWarn::NoParent);
                    if ime_popup_warn_due(&warned, &mut failing, key) {
                        log::warn!(
                            "[ime-pos] popup {:?} has no parent",
                            popup.wl_surface().id()
                        );
                    }
                    continue;
                }
            };
            let parent_win = match self.surface_to_window.get(&parent.surface.id()) {
                Some(&w) => w,
                None => {
                    let key = (popup.wl_surface().id(), ImPopupWarn::ParentUnmapped);
                    if ime_popup_warn_due(&warned, &mut failing, key) {
                        log::warn!(
                            "[ime-pos] parent surface {:?} not mapped to a window",
                            parent.surface.id()
                        );
                    }
                    continue;
                }
            };
            let geo = match self.window_geometry.get(&parent_win) {
                Some(g) => g,
                None => {
                    let key = (popup.wl_surface().id(), ImPopupWarn::NoGeometry);
                    if ime_popup_warn_due(&warned, &mut failing, key) {
                        log::warn!("[ime-pos] window {parent_win:?} has no geometry");
                    }
                    continue;
                }
            };
            let abs_x = geo.x + loc.x;
            let cursor_top = geo.y + loc.y;
            let cursor_bottom = cursor_top + cursor_rect.size.h;
            // Debug, not info: the manual KMS path calls this once per output
            // per frame and the compositor path once per frame, so at info
            // (the default release filter) an open candidate window formatted
            // and wrote a line per popup per frame for as long as it was up.
            log::debug!(
                "[ime-pos] popup {:?} parent={parent_win:?} loc=({},{}) cursor_h={} -> x={abs_x} cursor_top={cursor_top} cursor_bottom={cursor_bottom}",
                popup.wl_surface().id(),
                loc.x,
                loc.y,
                cursor_rect.size.h,
            );
            result.push(ImPopupAnchor {
                surface: popup.wl_surface().clone(),
                x: abs_x,
                cursor_top,
                cursor_bottom,
                area_left: geo.x,
                area_top: geo.y,
                area_right: geo.x + geo.w as i32,
                area_bottom: geo.y + geo.h as i32,
            });
        }
        // Retire every warned pair this call did not fail again: gone popups
        // and cleared conditions leave the set, so their next failure warns.
        *warned = failing;
        result
    }
}

impl OutputHandler for JwmWaylandState {
    /// A workspace group can only enter the wl_outputs its client had bound
    /// when it was sent. A taskbar that binds one later (the new global of a
    /// rebuilt output, or its outputs after its workspace manager) is sent
    /// the missing `output_enter` now, once policy has published monitors.
    fn output_bound(&mut self, _output: Output, _wl_output: WlOutput) {
        self.publish_workspace_monitors();
    }
}

impl CompositorHandler for JwmWaylandState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        // Regular Wayland clients are inserted with `JwmClientState`.
        if let Some(data) = client.get_data::<JwmClientState>() {
            return &data.compositor_state;
        }

        // XWayland is itself a Wayland client with `XWaylandClientData`.
        // Without this branch we would panic as soon as XWayland commits a surface.
        if let Some(data) = client.get_data::<XWaylandClientData>() {
            return &data.compositor_state;
        }

        panic!("Missing compositor client state (neither JwmClientState nor XWaylandClientData)")
    }

    fn new_surface(&mut self, surface: &WlSurface) {
        // Per-surface pre-commit hook for wp-linux-drm-syncobj-v1 explicit
        // sync. Without this the smithay protocol module accepts acquire/
        // release points but never actually waits on the acquire fence, so a
        // commit could be applied before the client's GPU writes are visible.
        // The hook reads DrmSyncobjCachedState.pending().acquire_point on
        // every commit, builds a (Blocker, EventSource) pair, registers the
        // source with calloop, and parks the commit on the blocker — calloop
        // releases it the instant the kernel signals the syncobj eventfd.
        smithay::wayland::compositor::add_pre_commit_hook::<JwmWaylandState, _>(
            surface,
            |state, _dh, surface| {
                if state.drm_syncobj_state.is_none() {
                    return;
                }
                let acquire_point = with_states(surface, |states| {
                    let mut cached = states
                        .cached_state
                        .get::<smithay::wayland::drm_syncobj::DrmSyncobjCachedState>();
                    cached.pending().acquire_point.clone()
                });
                let Some(acquire) = acquire_point else { return };
                let (blocker, source) = match acquire.generate_blocker() {
                    Ok(pair) => pair,
                    Err(err) => {
                        log::warn!(
                            "[drm_syncobj] generate_blocker failed, falling back to implicit sync: {err}"
                        );
                        return;
                    }
                };
                let Some(client) = surface.client() else {
                    return;
                };
                let registered = state.loop_handle.insert_source(source, move |_, _, data| {
                    let dh = data.display_handle.clone();
                    data.client_compositor_state(&client)
                        .blocker_cleared(data, &dh);
                    Ok(())
                });
                if registered.is_err() {
                    log::warn!("[drm_syncobj] failed to register sync-point source with calloop");
                    return;
                }
                smithay::wayland::compositor::add_blocker(surface, blocker);
            },
        );

        // wp-commit-timing is advertised in unmanaged mode so clients can use
        // the protocol without Smithay installing commit blockers. We still
        // must consume one pending timestamp per commit; otherwise a client
        // that updates the target timestamp again trips TimestampExists.
        // Unmanaged mode creates no CommitTimerBarrierState, so there is also
        // deliberately no periodic surface-tree barrier scan to perform.
        smithay::wayland::compositor::add_pre_commit_hook::<JwmWaylandState, _>(
            surface,
            |_state, _dh, surface| {
                with_states(surface, |states| {
                    if let Some(timer_state) = states.data_map.get::<CommitTimerStateUserData>() {
                        timer_state.borrow_mut().timestamp.take();
                    }
                });
            },
        );
    }

    fn commit(&mut self, surface: &WlSurface) {
        // wp-color-management double-buffer latch: move the image description
        // staged by set/unset_image_description into the committed snapshot the
        // render path reads. Smithay invokes this hook when the surface's
        // transaction applies — immediately for plain surfaces and
        // desynchronized subsurfaces, and at the parent commit for
        // synchronized subsurfaces — which is exactly the protocol's commit
        // point for the whole surface tree.
        if let Some(cm) = self.color_manager.as_ref() {
            if cm.commit_surface_description(&surface.id()) {
                self.needs_redraw = true;
            }
        }

        // wp-tearing-control double-buffer latch, at the same commit point
        // and for the same reason: a presentation hint describes the buffer
        // committed with it, not the one already on screen.
        if let Some(hints) = self.tearing_hints.as_ref()
            && crate::backend::wayland_udev::tearing_control::commit_surface_hint(
                hints,
                &surface.id(),
            )
        {
            self.needs_redraw = true;
        }

        // Snapshot the buffer assignment kind BEFORE on_commit_buffer_handler consumes it.
        // on_commit_buffer_handler calls RendererSurfaceState::update_buffer which takes
        // the buffer out of SurfaceAttributes::current().buffer via .take(). If we read
        // the buffer afterwards it will always be None and windows will never be mapped.
        #[derive(Debug, Clone, Copy)]
        enum BufferState {
            NewBuffer,
            Removed,
            None,
        }

        let (buf_state, has_damage, has_buffer_delta) = with_states(surface, |states| {
            let mut cached = states.cached_state.get::<SurfaceAttributes>();
            let current = cached.current();
            let has_damage = !current.damage.is_empty();
            let has_buffer_delta = current.buffer_delta.is_some();
            let buf_state = match &current.buffer {
                Some(BufferAssignment::NewBuffer(_)) => BufferState::NewBuffer,
                Some(BufferAssignment::Removed) => BufferState::Removed,
                None => BufferState::None,
            };
            (buf_state, has_damage, has_buffer_delta)
        });

        // Keep renderer surface state in sync with wl_surface buffer commits.
        // Without this, WaylandSurfaceRenderElement will often have no view/texture and nothing
        // will be drawn even though windows are managed and receive input.
        on_commit_buffer_handler::<JwmWaylandState>(surface);

        // Legacy XWayland (< 23.1) associates content via the WL_SURFACE_ID atom. smithay records
        // it as `wl_surface_id` but, unlike the modern xwayland_shell path, never calls
        // `set_wl_surface`/`surface_associated`, so `X11Surface::wl_surface()` stays `None` and the
        // window renders fully transparent. Do the matching ourselves: when an unassociated
        // XWayland-client surface commits, link it to the X11 window whose recorded id matches.
        if !self.surface_to_window.contains_key(&surface.id())
            && surface
                .client()
                .map(|c| c.get_data::<XWaylandClientData>().is_some())
                .unwrap_or(false)
        {
            let pid = surface.id().protocol_id();
            #[allow(deprecated)] // wl_surface_id is the only association path for XWayland < 23.1
            let matched = match_x11_window_by_surface_id(
                self.x11_surfaces
                    .iter()
                    .map(|(win_id, x11)| (*win_id, x11.wl_surface_id())),
                pid,
            );
            if let Some(win_id) = matched {
                info!(
                    "[xwayland] legacy WL_SURFACE_ID association: win={win_id:?} wl_surface={pid}"
                );
                self.surface_to_window.insert(surface.id(), win_id);
                self.x11_wl_surfaces.insert(win_id, surface.clone());
                self.needs_redraw = true;
            }
        }

        // Count commits after legacy XWayland association has had a chance to
        // establish the root mapping. Descendant commits are attributed to
        // the same WindowId by walking the subsurface parent chain.
        self.note_surface_tree_commit(surface);

        let win = self.surface_to_window.get(&surface.id()).copied();

        // Root-surface mapping/unmapping -> translate into JWM window events.
        if let Some(win) = win {
            match buf_state {
                BufferState::NewBuffer => {
                    if self.manager_allows_surface_map(win) && self.mapped_windows.insert(win) {
                        info!("[udev/wayland] window mapped win={win:?}");

                        let offset = self.surface_window_geometry_loc(surface);
                        if offset.x != 0 || offset.y != 0 {
                            let geo = self.window_geometry.get(&win).copied();
                            debug!(
                                "[udev/wayland] mapped window-geometry offset win={win:?} surface_id={:?} window_geo={geo:?} xdg_loc=({}, {})",
                                surface.id(),
                                offset.x,
                                offset.y
                            );
                        }

                        self.push_event(BackendEvent::WindowMapped(win));

                        // Refine the fractional scale now that the window has a
                        // geometry and we know which output it lands on.
                        let scale = self.preferred_scale_for_window(win);
                        with_states(surface, |states| {
                            with_fractional_scale(states, |fs| {
                                fs.set_preferred_scale(scale);
                            });
                        });
                    }

                    if self.is_dialog_like_toplevel(win) {
                        if let Some(rect) = self.surface_window_geometry_rect(surface) {
                            let new_w = rect.size.w.max(1) as u32;
                            let new_h = rect.size.h.max(1) as u32;
                            let should_update = self
                                .window_geometry
                                .get(&win)
                                .map(|geo| geo.w != new_w || geo.h != new_h)
                                .unwrap_or(false);
                            if should_update {
                                if let Some(geo) = self.window_geometry.get_mut(&win) {
                                    geo.w = new_w;
                                    geo.h = new_h;
                                    self.pending_events.lock().unwrap().push_back(
                                        BackendEvent::WindowConfigured {
                                            window: win,
                                            x: geo.x,
                                            y: geo.y,
                                            width: new_w,
                                            height: new_h,
                                            border_width: 0,
                                        },
                                    );
                                }
                            }
                        }
                    } else {
                        self.enforce_toplevel_configure_size(win, surface);
                    }
                    self.needs_redraw = true;
                }
                BufferState::Removed => {
                    let was_managed = self.take_window_mapping(win);
                    self.forget_surface_commit_epoch(win);
                    if was_managed {
                        info!("[udev/wayland] window unmapped win={win:?}");
                        self.compositor_dead_windows.push(win.raw());
                        self.push_event(BackendEvent::WindowUnmapped {
                            window: win,
                            from_configure: false,
                        });
                    }
                    self.needs_redraw = true;
                }
                BufferState::None => {}
            }
        }

        // Rendering changes without a buffer attach (damage, buffer offset, etc).
        if !matches!(buf_state, BufferState::None) || has_damage || has_buffer_delta {
            self.needs_redraw = true;
        }

        // Ensure initial configure for layer surfaces, similar to Anvil.
        // Layer surfaces cannot attach a buffer before the initial configure is acked.
        for output in &self.outputs {
            let mut map = layer_map_for_output(output);
            if map
                .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                .is_none()
            {
                continue;
            }

            let initial_configure_sent = with_states(surface, |states| {
                states
                    .data_map
                    .get::<LayerSurfaceData>()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .initial_configure_sent
            });

            map.arrange();

            if !initial_configure_sent {
                if let Some(layer) = map.layer_for_surface(surface, WindowSurfaceType::TOPLEVEL) {
                    layer.layer_surface().send_configure();
                    self.needs_redraw = true;
                }
            }

            // Update tracked geometry for JWM and emit a configure notify when it changes.
            if let (Some(win), Some(layer)) = (
                win,
                map.layer_for_surface(surface, WindowSurfaceType::TOPLEVEL),
            ) {
                let layer_info = layer
                    .layer_surface()
                    .with_cached_state(|data| LayerSurfaceInfo {
                        exclusive_zone: data.exclusive_zone.into(),
                        anchor_top: data.anchor.contains(Anchor::TOP),
                        anchor_bottom: data.anchor.contains(Anchor::BOTTOM),
                        anchor_left: data.anchor.contains(Anchor::LEFT),
                        anchor_right: data.anchor.contains(Anchor::RIGHT),
                    });
                self.window_layer_info.insert(win, layer_info);

                if let Some(geo) = map.layer_geometry(layer) {
                    let new_geo = Geometry {
                        x: geo.loc.x,
                        y: geo.loc.y,
                        w: geo.size.w.max(0) as u32,
                        h: geo.size.h.max(0) as u32,
                        border: 0,
                    };

                    let changed = self
                        .window_geometry
                        .get(&win)
                        .map(|old| {
                            old.x != new_geo.x
                                || old.y != new_geo.y
                                || old.w != new_geo.w
                                || old.h != new_geo.h
                        })
                        .unwrap_or(true);

                    if changed {
                        self.window_geometry.insert(win, new_geo);
                        self.pending_events.lock().unwrap().push_back(
                            BackendEvent::WindowConfigured {
                                window: win,
                                x: new_geo.x,
                                y: new_geo.y,
                                width: new_geo.w,
                                height: new_geo.h,
                                border_width: 0,
                            },
                        );
                    }
                }
            }

            break;
        }
    }

    fn destroyed(&mut self, surface: &WlSurface) {
        self.forget_idle_inhibiting_surface(&surface.id());

        // The wl_surface is gone: no future commit can latch a staged image
        // description. Drop both latch halves and the feedback bookkeeping so
        // the ObjectId cannot linger past the surface's lifetime.
        if let Some(cm) = self.color_manager.as_ref() {
            if cm.destroy_surface_description(&surface.id()) {
                self.needs_redraw = true;
            }
            cm.forget_surface(&surface.id());
        }

        // Same reasoning for the tearing hint: the protocol only *asks* a
        // client to destroy its wp_tearing_control_v1 when the surface goes,
        // so without this the entry outlives the surface under an ObjectId
        // the server may later hand to something unrelated.
        if let Some(hints) = self.tearing_hints.as_ref()
            && crate::backend::wayland_udev::tearing_control::forget_surface(hints, &surface.id())
        {
            self.needs_redraw = true;
        }

        // Cleanup any tracked popups as well.
        if self.popups.remove(&surface.id()).is_some() {
            self.popup_order.retain(|id| *id != surface.id());
            self.needs_redraw = true;
        }

        if let Some(win) = self.surface_to_window.remove(&surface.id()) {
            log::info!(
                "[udev/wayland] surface_destroyed win={win:?} (client disconnected abruptly)"
            );
            // If this surface is a layer-shell surface, ensure it is also removed from the layer map.
            for output in &self.outputs {
                let map = layer_map_for_output(output);
                let layer = map
                    .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                    .cloned();
                drop(map);

                if let Some(layer) = layer {
                    let mut map = layer_map_for_output(output);
                    map.unmap_layer(&layer);
                    break;
                }
            }

            self.remove_wayland_window(win);
        }
    }
}

impl ShmHandler for JwmWaylandState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl BufferHandler for JwmWaylandState {
    fn buffer_destroyed(
        &mut self,
        _buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
    ) {
    }
}

impl DmabufHandler for JwmWaylandState {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        _dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        // Create the wl_buffer resource for the client. The actual renderer import happens
        // later when rendering the surface (via RendererSurfaceState).
        let _ = notifier.successful::<JwmWaylandState>();
        self.needs_redraw = true;
    }
}

impl SeatHandler for JwmWaylandState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&Self::KeyboardFocus>) {
        let client = focused.and_then(|surface| surface.client());
        set_data_device_focus(&self.display_handle, seat, client.clone());
        set_primary_focus(&self.display_handle, seat, client);
    }
}

impl InputMethodHandler for JwmWaylandState {
    fn new_popup(&mut self, surface: ImPopupSurface) {
        // Drop any popups whose role/surface fcitx5 already destroyed: smithay's
        // ZwpInputPopupSurfaceV2 destructor only flips the alive tracker, it never
        // calls `dismiss_popup`, so stale dead entries would otherwise pile up here.
        self.im_popups.retain(|p| p.alive());
        self.im_client_id = Some(surface.wl_surface().id());
        log::info!(
            "[ime] new_popup surface={:?} has_parent={} alive={} surface_alive={} total={}",
            surface.wl_surface().id(),
            surface.get_parent().is_some(),
            surface.alive(),
            surface.wl_surface().is_alive(),
            self.im_popups.len() + 1,
        );
        self.im_popups.push(surface);
        self.needs_redraw = true;
    }

    fn dismiss_popup(&mut self, surface: ImPopupSurface) {
        log::info!(
            "[ime] dismiss_popup surface={:?}",
            surface.wl_surface().id()
        );
        self.im_popups.retain(|p| p != &surface && p.alive());
        self.needs_redraw = true;
    }

    fn popup_repositioned(&mut self, _surface: ImPopupSurface) {
        self.needs_redraw = true;
    }

    fn parent_geometry(&self, parent: &WlSurface) -> Rectangle<i32, Logical> {
        // Return the geometry of the toplevel that owns this surface so the IME
        // popup can position itself correctly.
        if let Some(win) = self.surface_to_window.get(&parent.id()).copied() {
            if let Some(geo) = self.window_geometry.get(&win) {
                return Rectangle::new((geo.x, geo.y).into(), (geo.w as i32, geo.h as i32).into());
            }
        }
        Rectangle::default()
    }
}

impl SelectionHandler for JwmWaylandState {
    type SelectionUserData = ();

    fn new_selection(
        &mut self,
        ty: SelectionTarget,
        source: Option<SelectionSource>,
        _seat: Seat<Self>,
    ) {
        if ty == SelectionTarget::Clipboard {
            // A client owns the clipboard now, so whatever JWM was offering
            // is no longer current.
            self.clipboard_offered = None;
            if let Some(source) = source.as_ref() {
                self.clipboard_pending = Some(source.mime_types());
            }
        }
        if let Some(xwm) = self.x11_wm.as_mut() {
            if let Err(err) = xwm.new_selection(ty, source.map(|s| s.mime_types())) {
                warn!("Failed to set Xwayland selection {ty:?}: {err:?}");
            }
        }
    }

    fn send_selection(
        &mut self,
        ty: SelectionTarget,
        mime_type: String,
        fd: std::os::fd::OwnedFd,
        _seat: Seat<Self>,
        _user_data: &(),
    ) {
        // A history entry JWM is offering is served here; anything else is
        // XWayland's selection and stays its business.
        if ty == SelectionTarget::Clipboard
            && let Some(offer) = self.clipboard_offered.as_ref()
        {
            if let Some(payload) = selection_payload_for_mime(offer, &mime_type) {
                write_selection_async(payload.to_vec(), fd);
            }
            // Mismatched MIME: close without data. Do not hand JWM's offer to
            // Xwayland — we own the clipboard selection.
            return;
        }
        if let Some(xwm) = self.x11_wm.as_mut() {
            if let Err(err) = xwm.send_selection(ty, mime_type, fd) {
                warn!("Failed to send selection (X11 -> Wayland): {err:?}");
            }
        }
    }
}

/// MIME types JWM advertises when it offers a text history entry.
pub(crate) const CLIPBOARD_OFFER_MIMES: [&str; 4] = [
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
];

/// MIME type JWM advertises when it offers a PNG history entry.
pub(crate) const CLIPBOARD_PNG_OFFER_MIMES: [&str; 1] = ["image/png"];

/// Whether `mime` is a text type we are willing to serve for a text offer.
fn is_text_clipboard_mime(mime: &str) -> bool {
    CLIPBOARD_OFFER_MIMES
        .iter()
        .any(|offered| mime.eq_ignore_ascii_case(offered))
        || mime.to_ascii_lowercase().starts_with("text/")
}

/// Whether `mime` is an `image/png` request (including case variants).
fn is_png_clipboard_mime(mime: &str) -> bool {
    mime.eq_ignore_ascii_case("image/png")
}

/// Bytes to write for a client MIME request against the current offer, if any.
fn selection_payload_for_mime<'a>(
    offer: &'a crate::backend::clipboard_offer::ClipboardOffer,
    mime_type: &str,
) -> Option<&'a [u8]> {
    use crate::backend::clipboard_offer::ClipboardOffer;
    match offer {
        ClipboardOffer::Text(text) if is_text_clipboard_mime(mime_type) => Some(text.as_bytes()),
        ClipboardOffer::Png(png) if is_png_clipboard_mime(mime_type) => Some(png.as_slice()),
        ClipboardOffer::Text(_) | ClipboardOffer::Png(_) => None,
    }
}

/// A clipboard peer is another Wayland client and is not trusted to consume
/// or produce its pipe promptly. Keep both the lifetime and the number of the
/// detached I/O workers bounded so stalled peers cannot exhaust compositor
/// threads.
const CLIPBOARD_IO_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CLIPBOARD_IO_WORKERS: usize = 8;
static ACTIVE_CLIPBOARD_IO_WORKERS: AtomicUsize = AtomicUsize::new(0);

struct ClipboardIoPermit<'a>(&'a AtomicUsize);

impl Drop for ClipboardIoPermit<'_> {
    fn drop(&mut self) {
        let previous = self.0.fetch_sub(1, Ordering::Relaxed);
        debug_assert!(previous > 0, "clipboard I/O permit count underflowed");
    }
}

fn try_acquire_clipboard_io_permit(
    active: &AtomicUsize,
    limit: usize,
) -> Option<ClipboardIoPermit<'_>> {
    active
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
            (count < limit).then(|| count + 1)
        })
        .ok()?;
    Some(ClipboardIoPermit(active))
}

fn acquire_clipboard_io_permit() -> Option<ClipboardIoPermit<'static>> {
    try_acquire_clipboard_io_permit(&ACTIVE_CLIPBOARD_IO_WORKERS, MAX_CLIPBOARD_IO_WORKERS)
}

fn set_clipboard_fd_nonblocking(fd: std::os::fd::RawFd, nonblocking: bool) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let flags = if nonblocking {
        flags | libc::O_NONBLOCK
    } else {
        flags & !libc::O_NONBLOCK
    };
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn clipboard_io_timeout() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "clipboard transfer deadline expired",
    )
}

fn wait_for_clipboard_fd(
    fd: std::os::fd::RawFd,
    events: libc::c_short,
    deadline: Instant,
) -> std::io::Result<()> {
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(clipboard_io_timeout());
        };
        let timeout_ms = i32::try_from(remaining.as_millis().max(1)).unwrap_or(i32::MAX);
        let mut descriptor = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if ready > 0 {
            if descriptor.revents & libc::POLLNVAL != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "clipboard transfer fd became invalid",
                ));
            }
            // POLLERR/POLLHUP are deliberately handed back to read/write so
            // the ordinary EOF/BrokenPipe result remains authoritative.
            return Ok(());
        }
        if ready == 0 {
            return Err(clipboard_io_timeout());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn write_clipboard_payload(
    mut file: std::fs::File,
    mut payload: &[u8],
    timeout: Duration,
) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::fd::AsRawFd as _;

    set_clipboard_fd_nonblocking(file.as_raw_fd(), true)?;
    let deadline = Instant::now() + timeout;
    while !payload.is_empty() {
        if Instant::now() >= deadline {
            return Err(clipboard_io_timeout());
        }
        match file.write(payload) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "clipboard client accepted zero bytes",
                ));
            }
            Ok(written) => payload = &payload[written..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                wait_for_clipboard_fd(file.as_raw_fd(), libc::POLLOUT, deadline)?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn read_clipboard_payload(
    mut file: std::fs::File,
    limit: usize,
    timeout: Duration,
) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;
    use std::os::fd::AsRawFd as _;

    set_clipboard_fd_nonblocking(file.as_raw_fd(), true)?;
    let deadline = Instant::now() + timeout;
    let mut buffer = Vec::with_capacity(limit.min(8 * 1024));
    let mut chunk = [0u8; 8 * 1024];
    while buffer.len() < limit {
        if Instant::now() >= deadline {
            return Err(clipboard_io_timeout());
        }
        let remaining = limit - buffer.len();
        let request = remaining.min(chunk.len());
        match file.read(&mut chunk[..request]) {
            Ok(0) => return Ok(buffer),
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                wait_for_clipboard_fd(file.as_raw_fd(), libc::POLLIN, deadline)?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(buffer)
}

/// Put finished clipboard reads back into selection order.
///
/// Reads run concurrently, so a slow read of an older selection can finish
/// after a newer one. Sorting by generation keeps a batch in copy order, and
/// a read older than one already delivered is dropped: recording it now would
/// put a superseded copy on top of the history.
fn order_clipboard_captures<T>(mut finished: Vec<(u64, T)>, delivered: &mut u64) -> Vec<T> {
    finished.sort_by_key(|(generation, _)| *generation);
    finished
        .into_iter()
        .filter_map(|(generation, payload)| {
            (generation > *delivered).then(|| {
                *delivered = generation;
                payload
            })
        })
        .collect()
}

/// Hand `payload` to a client on `fd` without blocking the compositor.
///
/// The reader is another process and may be slow or may never read at all. A
/// bounded worker keeps that peer off the compositor thread without letting it
/// pin an unbounded detached thread.
fn write_selection_async(payload: Vec<u8>, fd: std::os::fd::OwnedFd) {
    let Some(permit) = acquire_clipboard_io_permit() else {
        debug!("clipboard: rejecting transfer because all I/O workers are occupied");
        return;
    };
    let worker = std::thread::Builder::new()
        .name("jwm-clipboard-write".to_string())
        .spawn(move || {
            let _permit = permit;
            if let Err(error) =
                write_clipboard_payload(std::fs::File::from(fd), &payload, CLIPBOARD_IO_TIMEOUT)
            {
                debug!("clipboard: client did not consume offered selection: {error}");
            }
        });
    if let Err(error) = worker {
        warn!("clipboard: failed to start selection writer: {error}");
    }
}

impl JwmWaylandState {
    /// Ask the selection owner for its payload and record it in the history.
    ///
    /// Offers marked as secrets never get this far. Text wins when present;
    /// otherwise `image/png` is read under the image history cap. The payload
    /// is read on a thread: the owning client writes at its own pace, and a
    /// compositor that waited would stall every other client with it.
    fn capture_clipboard(&mut self, mime_types: &[String]) {
        if !crate::config::CONFIG.load().behavior().clipboard_history {
            return;
        }
        if crate::backend::clipboard_offer::is_secret(mime_types) {
            debug!("clipboard: offer marked secret, not reading it");
            return;
        }
        let (mime, as_png) = if let Some(mime) =
            crate::backend::clipboard_offer::preferred_text_mime(mime_types)
        {
            (mime, false)
        } else if let Some(mime) = crate::backend::clipboard_offer::preferred_image_mime(mime_types)
        {
            (mime, true)
        } else {
            return;
        };
        let Some(permit) = acquire_clipboard_io_permit() else {
            debug!("clipboard: skipping capture because all I/O workers are occupied");
            return;
        };
        let (read, write) = match std::io::pipe() {
            Ok(pair) => pair,
            Err(error) => {
                warn!("clipboard: pipe failed: {error}");
                return;
            }
        };
        if let Err(error) = request_data_device_client_selection(&self.seat, mime, write.into()) {
            warn!("clipboard: requesting the selection failed: {error:?}");
            return;
        }

        self.clipboard_capture_generation += 1;
        let generation = self.clipboard_capture_generation;
        let captured = std::sync::Arc::clone(&self.clipboard_captured);
        let worker = std::thread::Builder::new()
            .name("jwm-clipboard-read".to_string())
            .spawn(move || {
                let _permit = permit;
                let limit = if as_png {
                    crate::backend::clipboard_offer::MAX_IMAGE_HISTORY_BYTES + 1
                } else {
                    crate::backend::clipboard_offer::MAX_TEXT_BYTES + 1
                };
                let Ok(buffer) = read_clipboard_payload(
                    std::fs::File::from(std::os::fd::OwnedFd::from(read)),
                    limit,
                    CLIPBOARD_IO_TIMEOUT,
                ) else {
                    return;
                };
                let payload = if as_png {
                    if buffer.is_empty()
                        || buffer.len() > crate::backend::clipboard_offer::MAX_IMAGE_HISTORY_BYTES
                    {
                        return;
                    }
                    crate::backend::clipboard_offer::CapturedClipboard::Png(buffer)
                } else {
                    if buffer.len() > crate::backend::clipboard_offer::MAX_TEXT_BYTES {
                        return;
                    }
                    let Ok(text) = String::from_utf8(buffer) else {
                        return;
                    };
                    crate::backend::clipboard_offer::CapturedClipboard::Text(text)
                };
                if let Ok(mut captured) = captured.lock() {
                    captured.push((generation, payload));
                }
            });
        if let Err(error) = worker {
            warn!("clipboard: failed to start selection reader: {error}");
        }
    }

    /// Payloads copied since the last call, oldest first.
    ///
    /// Also starts the read for a selection announced since the last call:
    /// by now smithay has stored it on the seat and it can be asked for.
    pub fn drain_clipboard_captured(
        &mut self,
    ) -> Vec<crate::backend::clipboard_offer::CapturedClipboard> {
        if let Some(mime_types) = self.clipboard_pending.take() {
            self.capture_clipboard(&mime_types);
        }
        let finished = self
            .clipboard_captured
            .lock()
            .map(|mut captured| std::mem::take(&mut *captured))
            .unwrap_or_default();
        order_clipboard_captures(finished, &mut self.clipboard_delivered_generation)
    }

    /// Offer `text` to clients as the clipboard selection.
    pub fn offer_clipboard_text(&mut self, text: &str) -> bool {
        self.clipboard_offered = Some(crate::backend::clipboard_offer::ClipboardOffer::Text(
            text.to_string(),
        ));
        self.publish_clipboard_offer(&CLIPBOARD_OFFER_MIMES);
        true
    }

    /// Offer PNG bytes to clients as the clipboard selection.
    pub fn offer_clipboard_png(&mut self, png: Vec<u8>) -> bool {
        self.clipboard_offered = Some(crate::backend::clipboard_offer::ClipboardOffer::Png(png));
        self.publish_clipboard_offer(&CLIPBOARD_PNG_OFFER_MIMES);
        true
    }

    /// Make JWM's offer the clipboard for Wayland and X11 clients alike.
    /// Neither Smithay call below runs `SelectionHandler::new_selection`,
    /// so Xwayland is told directly; its requests then reach
    /// `send_selection_to_xwayland`, which serves the offer.
    fn publish_clipboard_offer(&mut self, mime_types: &[&str]) {
        let mime_types: Vec<String> = mime_types.iter().map(|m| (*m).to_string()).collect();
        set_data_device_selection(&self.display_handle, &self.seat, mime_types.clone(), ());
        if let Some(xwm) = self.x11_wm.as_mut()
            && let Err(err) = xwm.new_selection(SelectionTarget::Clipboard, Some(mime_types))
        {
            warn!("Failed to offer JWM clipboard to Xwayland: {err:?}");
        }
    }
}

impl DataDeviceHandler for JwmWaylandState {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

impl WaylandDndGrabHandler for JwmWaylandState {
    fn dnd_requested<S: Source>(
        &mut self,
        source: S,
        icon: Option<WlSurface>,
        seat: Seat<Self>,
        serial: Serial,
        type_: GrabType,
    ) {
        self.dnd_icon = icon.map(|surface| DndIcon {
            surface,
            offset: (0, 0).into(),
        });
        self.needs_redraw = true;
        match type_ {
            GrabType::Pointer => {
                let Some(pointer) = seat.get_pointer() else {
                    source.cancel();
                    return;
                };
                let Some(start_data) = pointer.grab_start_data() else {
                    source.cancel();
                    return;
                };
                pointer.set_grab(
                    self,
                    DnDGrab::new_pointer(&self.display_handle, start_data, source, seat),
                    serial,
                    Focus::Keep,
                );
            }
            GrabType::Touch => {
                let Some(touch) = seat.get_touch() else {
                    source.cancel();
                    return;
                };
                let Some(start_data) = touch.grab_start_data() else {
                    source.cancel();
                    return;
                };
                touch.set_grab(
                    self,
                    DnDGrab::new_touch(&self.display_handle, start_data, source, seat),
                    serial,
                );
            }
        }
    }
}

impl DndGrabHandler for JwmWaylandState {
    fn dropped(
        &mut self,
        _target: Option<DndTarget<'_, Self>>,
        _validated: bool,
        _seat: Seat<Self>,
        _location: Point<f64, Logical>,
    ) {
        self.dnd_icon = None;
        self.needs_redraw = true;
    }
}

impl PrimarySelectionHandler for JwmWaylandState {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.primary_selection_state
    }
}

impl ExtBackgroundEffectHandler for JwmWaylandState {
    fn set_blur_region(
        &mut self,
        _wl_surface: WlSurface,
        _region: smithay::wayland::compositor::RegionAttributes,
    ) {
        // Region is stored in the surface's BackgroundEffectSurfaceCachedState by
        // the protocol; just request a redraw so the new effect is picked up.
        self.needs_redraw = true;
    }

    fn unset_blur_region(&mut self, _wl_surface: WlSurface) {
        self.needs_redraw = true;
    }
}

// ---------------------------------------------------------------------------
// XDG Decoration Handler – always prefer server-side decorations so GTK apps
// (terminator, gnome-terminal, …) don't draw a CSD titlebar inside the window.
// ---------------------------------------------------------------------------
impl XdgDecorationHandler for JwmWaylandState {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        // Set ServerSide decoration mode in pending state.  If the WM hasn't
        // sent its initial configure yet, the mode will be included when the
        // WM calls WindowOps::configure (which calls send_pending_configure).
        //
        // If the initial configure was already sent before new_decoration fired
        // (e.g. the client creates the decoration object in a separate commit
        // after the WM already processed WindowCreated), smithay's server_pending
        // is re-initialised from current_server_state() — which carries the last
        // configured size — so send_pending_configure() delivers the correct size
        // together with the ServerSide mode without any size=None problem.
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(Mode::ServerSide);
        });
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }
    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: Mode) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(Mode::ServerSide);
        });
        toplevel.send_pending_configure();
    }
    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(Mode::ServerSide);
        });
        toplevel.send_pending_configure();
    }
}

impl XdgShellHandler for JwmWaylandState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let win = self.alloc_window_id();
        let obj_id = surface.wl_surface().id();

        // Don't let IME client toplevels be managed by the WM — managing them
        // triggers focus changes that kill the input method popup.
        let is_ime_client = self
            .im_client_id
            .as_ref()
            .map_or(false, |im_id| obj_id.same_client_as(im_id));

        info!("[udev/wayland] new_toplevel win={win:?} surface_id={obj_id:?} ime={is_ime_client}");

        self.surface_to_window.insert(obj_id, win);
        self.toplevels.insert(win, surface);

        self.window_geometry.insert(
            win,
            Geometry {
                x: 0,
                y: 0,
                w: 800,
                h: 600,
                border: 0,
            },
        );
        self.window_stack.push(win);

        self.window_title.insert(win, String::new());
        self.window_app_id.insert(win, String::new());
        self.window_is_fullscreen.insert(win, false);

        if is_ime_client {
            self.needs_redraw = true;
            return;
        }

        // Keep the identifier stable for this toplevel's lifetime. Destroying
        // and recreating the role allocates a new WindowId and identifier.
        let handle = self
            .foreign_toplevel_list_state
            .new_toplevel_with_identifier::<JwmWaylandState>(
                "",
                "",
                Self::foreign_toplevel_identifier(win),
            );
        self.foreign_toplevel_handles.insert(win, handle);

        // Announce to wlr-foreign-toplevel-management clients.
        if let Some(ref ftm) = self.foreign_toplevel_mgmt {
            crate::backend::wayland_udev::foreign_toplevel_management::announce_new_toplevel(
                &self.display_handle,
                ftm,
                win,
                "",
                "",
            );
        }

        // Track windows that still need their initial configure. Normally the WM triggers this via
        // `WindowOps::configure`, but we keep a timeout-based fallback to avoid clients stalling
        // indefinitely if the WM doesn't configure quickly enough.
        self.pending_initial_configure.insert(win);

        // One timer per new toplevel preserves the 250 ms safety bound without
        // waking the compositor 20 times a second for the rest of the session.
        let timer = Timer::from_duration(INITIAL_CONFIGURE_TIMEOUT);
        if let Err(error) = self.loop_handle.insert_source(timer, move |_, _, state| {
            state.ensure_initial_configure_fallback(win);
            TimeoutAction::Drop
        }) {
            warn!("[udev/wayland] could not arm initial configure fallback for {win:?}: {error}");
            // Losing the timer must not leave the client stalled forever.
            self.ensure_initial_configure_fallback(win);
        }

        self.push_event(BackendEvent::WindowCreated(win));
        self.needs_redraw = true;
    }

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        // Store the initial positioner state and compute a constrained geometry.
        surface.with_pending_state(|state| {
            state.positioner = positioner;
            state.geometry = state.positioner.get_geometry();
        });
        self.unconstrain_popup(&surface);
        let _ = surface.send_configure();

        let id = surface.wl_surface().id();
        self.popup_order.push(id.clone());
        self.popups.insert(id, surface);
        self.needs_redraw = true;
    }

    fn grab(
        &mut self,
        _surface: PopupSurface,
        _seat: smithay::reexports::wayland_server::protocol::wl_seat::WlSeat,
        _serial: Serial,
    ) {
        // Record the toplevel this grab belongs to, and remember current keyboard focus.
        if self.popup_grab_prev_kbd_focus.is_none() {
            self.popup_grab_prev_kbd_focus =
                self.seat.get_keyboard().and_then(|k| k.current_focus());
        }

        let toplevel = if let Some(existing) = self.popups.get(&_surface.wl_surface().id()) {
            self.popup_root_toplevel(existing, 0)
        } else {
            self.popup_root_toplevel(&_surface, 0)
        };
        self.popup_grab_toplevel = toplevel;

        // Give the popup keyboard focus (menus often need this), while we remember the previous focus
        // for restoration when the grab ends.
        if let Some(kbd) = self.seat.get_keyboard() {
            let serial = SCOUNTER.next_serial();
            kbd.set_focus(self, Some(_surface.wl_surface().clone()), serial);
        }
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.positioner = positioner;
            state.geometry = state.positioner.get_geometry();
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn fullscreen_request(&mut self, surface: ToplevelSurface, _output: Option<WlOutput>) {
        if let Some(window) = self
            .surface_to_window
            .get(&surface.wl_surface().id())
            .copied()
        {
            // JWM's fullscreen policy owns placement and therefore uses the
            // window's current monitor. The protocol output is only a hint;
            // honoring it would bypass monitor/tag ownership in shared policy.
            self.request_window_state(window, NetWmState::Fullscreen, true);
        }
        // Preserve Smithay's default protocol progress. In particular, a
        // request made before JWM handles WindowCreated must still receive an
        // initial configure rather than waiting solely on the policy queue.
        surface.send_configure();
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        if let Some(window) = self
            .surface_to_window
            .get(&surface.wl_surface().id())
            .copied()
        {
            self.request_window_state(window, NetWmState::Fullscreen, false);
        }
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        self.xdg_maximize_request(&surface, true);
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        self.xdg_maximize_request(&surface, false);
    }

    fn minimize_request(&mut self, surface: ToplevelSurface) {
        if let Some(window) = self
            .surface_to_window
            .get(&surface.wl_surface().id())
            .copied()
        {
            self.request_window_state(window, NetWmState::Hidden, true);
        }
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        if let Some(win) = self.surface_to_window.remove(&surface.wl_surface().id()) {
            info!("[udev/wayland] toplevel_destroyed win={win:?}");
            self.remove_wayland_window(win);
        }
    }

    fn popup_destroyed(&mut self, surface: PopupSurface) {
        let id = surface.wl_surface().id();
        self.popups.remove(&id);
        self.popup_order.retain(|x| *x != id);
        self.needs_redraw = true;

        if let Some(grab_win) = self.popup_grab_toplevel {
            let any_left = self
                .popups
                .values()
                .any(|p| self.popup_root_toplevel(p, 0) == Some(grab_win));
            if !any_left {
                self.popup_grab_toplevel = None;

                // Restore keyboard focus to what it was before the popup grab.
                if let Some(kbd) = self.seat.get_keyboard() {
                    let serial = SCOUNTER.next_serial();
                    if let Some(prev) = self.popup_grab_prev_kbd_focus.take() {
                        kbd.set_focus(self, Some(prev), serial);
                    } else if let Some(surface) = self.surface_for_window(grab_win) {
                        kbd.set_focus(self, Some(surface), serial);
                    }
                }
            }
        }
    }

    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        let Some(win) = self
            .surface_to_window
            .get(&surface.wl_surface().id())
            .copied()
        else {
            return;
        };

        let app_id = with_states(surface.wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .unwrap()
                .lock()
                .unwrap()
                .app_id
                .clone()
                .unwrap_or_default()
        });

        info!(
            "[udev/wayland] app_id_changed win={win:?} app_id={}",
            app_id
        );

        self.window_app_id.insert(win, app_id.clone());

        if let Some(handle) = self.foreign_toplevel_handles.get(&win) {
            handle.send_app_id(&app_id);
            handle.send_done();
        }
        if let Some(ref ftm) = self.foreign_toplevel_mgmt {
            ftm.update_app_id(win, &app_id);
        }

        self.push_event(BackendEvent::PropertyChanged {
            window: win,
            kind: PropertyKind::Class,
        });
        if self.is_dialog_like_toplevel(win) {
            self.push_event(BackendEvent::PropertyChanged {
                window: win,
                kind: PropertyKind::WindowType,
            });
        }
    }

    fn title_changed(&mut self, surface: ToplevelSurface) {
        let Some(win) = self
            .surface_to_window
            .get(&surface.wl_surface().id())
            .copied()
        else {
            return;
        };

        let title = with_states(surface.wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .unwrap()
                .lock()
                .unwrap()
                .title
                .clone()
                .unwrap_or_default()
        });

        info!("[udev/wayland] title_changed win={win:?} title={}", title);

        self.window_title.insert(win, title.clone());

        if let Some(handle) = self.foreign_toplevel_handles.get(&win) {
            handle.send_title(&title);
            handle.send_done();
        }
        if let Some(ref ftm) = self.foreign_toplevel_mgmt {
            ftm.update_title(win, &title);
        }

        self.push_event(BackendEvent::PropertyChanged {
            window: win,
            kind: PropertyKind::Title,
        });
        if self.is_dialog_like_toplevel(win) {
            self.push_event(BackendEvent::PropertyChanged {
                window: win,
                kind: PropertyKind::WindowType,
            });
        }
    }

    fn parent_changed(&mut self, surface: ToplevelSurface) {
        let Some(win) = self
            .surface_to_window
            .get(&surface.wl_surface().id())
            .copied()
        else {
            return;
        };

        self.push_event(BackendEvent::PropertyChanged {
            window: win,
            kind: PropertyKind::TransientFor,
        });
        self.push_event(BackendEvent::PropertyChanged {
            window: win,
            kind: PropertyKind::WindowType,
        });
    }
}

impl WlrLayerShellHandler for JwmWaylandState {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        output: Option<WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        let output = output
            .as_ref()
            .and_then(Output::from_resource)
            .or_else(|| {
                // If the client didn't pick an output, prefer the one under the pointer.
                let location = self.pointer_location;
                self.outputs.iter().find_map(|o| {
                    let Some(mode) = o.current_mode() else {
                        return None;
                    };
                    let scale = o.current_scale().fractional_scale();
                    let logical_size = mode.size.to_f64().to_logical(scale).to_i32_round();
                    let logical_size = o.current_transform().transform_size(logical_size);
                    let rect = Rectangle::<i32, Logical>::new(o.current_location(), logical_size);
                    if rect.to_f64().contains(location) {
                        Some(o.clone())
                    } else {
                        None
                    }
                })
            })
            .or_else(|| self.outputs.first().cloned());
        let Some(output) = output else {
            return;
        };

        // Log the client-provided intent; very useful to confirm whether bars are using layer-shell
        // and which anchors/exclusive zone they request.
        surface.with_cached_state(|data| {
            log::info!(
                "[layer-shell] new_surface ns='{}' layer={:?} anchor={:?} excl_zone={:?} size={:?} margin={:?} kbd={:?}",
                namespace,
                data.layer,
                data.anchor,
                data.exclusive_zone,
                data.size,
                data.margin,
                data.keyboard_interactivity
            );
        });

        let win = self.alloc_window_id();
        let obj_id = surface.wl_surface().id();

        let layer_info = surface.with_cached_state(|data| LayerSurfaceInfo {
            exclusive_zone: data.exclusive_zone.into(),
            anchor_top: data.anchor.contains(Anchor::TOP),
            anchor_bottom: data.anchor.contains(Anchor::BOTTOM),
            anchor_left: data.anchor.contains(Anchor::LEFT),
            anchor_right: data.anchor.contains(Anchor::RIGHT),
        });

        // Track as a JWM window so status bars (and other docks) can be detected via title/app_id.
        self.surface_to_window.insert(obj_id, win);
        self.layer_surfaces
            .insert(win, surface.wl_surface().clone());
        self.window_layer_info.insert(win, layer_info);

        // Placeholder geometry until the layer map arranges and we observe it in `commit()`.
        self.window_geometry.insert(
            win,
            Geometry {
                x: 0,
                y: 0,
                w: 1,
                h: 1,
                border: 0,
            },
        );
        self.window_title.insert(win, namespace.clone());
        self.window_app_id.insert(win, namespace.clone());
        self.window_is_fullscreen.insert(win, false);

        self.push_event(BackendEvent::WindowCreated(win));

        let mut map = layer_map_for_output(&output);
        let _ = map.map_layer(&DesktopLayerSurface::new(surface, namespace));
        self.needs_redraw = true;
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        for output in &self.outputs {
            let map = layer_map_for_output(output);
            let layer = map
                .layers()
                .find(|&layer| layer.layer_surface() == &surface)
                .cloned();
            drop(map);

            if let Some(layer) = layer {
                let mut map = layer_map_for_output(output);
                map.unmap_layer(&layer);
                self.needs_redraw = true;
                break;
            }
        }

        // The wl_surface may outlive its layer role and even take a new one,
        // which allocates a fresh WindowId. Retire this role's window now, as
        // `toplevel_destroyed` does, so policy drops its strut and entry.
        if let Some(win) = self.surface_to_window.remove(&surface.wl_surface().id()) {
            info!("[udev/wayland] layer_destroyed win={win:?}");
            self.remove_wayland_window(win);
        }
    }
}

/// Find the X11 window whose recorded `WL_SURFACE_ID` matches a committed surface's protocol id.
///
/// XWayland < 23.1 associates content via the legacy `WL_SURFACE_ID` atom, which smithay records
/// as `X11Surface::wl_surface_id()` but never auto-associates (only the modern xwayland_shell path
/// does). We resolve the link ourselves in the commit handler; this is the pure matching core,
/// kept separate so it can be unit-tested without a live Wayland server.
fn match_x11_window_by_surface_id(
    candidates: impl IntoIterator<Item = (WindowId, Option<u32>)>,
    protocol_id: u32,
) -> Option<WindowId> {
    candidates
        .into_iter()
        .find(|(_, id)| *id == Some(protocol_id))
        .map(|(win, _)| win)
}

/// The logical size an output covers: its mode scaled down by the output
/// scale, then rotated by its transform. Session-lock surfaces are
/// configured with it and hit-tested against it.
fn output_logical_size(
    mode_size: smithay::utils::Size<i32, smithay::utils::Physical>,
    scale: f64,
    transform: smithay::utils::Transform,
) -> smithay::utils::Size<i32, Logical> {
    transform.transform_size(mode_size.to_f64().to_logical(scale).to_i32_round())
}

/// What JWM knows about the managed window an XWayland ConfigureRequest
/// names.
#[derive(Debug, Clone, Copy)]
struct ManagedXwaylandWindow {
    window: WindowId,
    /// The window carries the `_NET_WM_STATE` maximized pair.
    maximized: bool,
    /// The geometry JWM last configured the window with, if any.
    configured: Option<Geometry>,
}

/// What an XWayland ConfigureRequest turns into.
#[derive(Debug)]
enum XwaylandConfigureRoute {
    /// A mapped window with a WindowId: shared policy decides, exactly as for
    /// an X11 ConfigureRequest. The mask names only the fields the client
    /// asked for.
    Policy {
        window: WindowId,
        mask_bits: u16,
        changes: crate::backend::api::WindowChanges,
    },
    /// A window policy has not seen yet (before its map request): nothing
    /// owns its geometry, so the request is granted over the current one.
    Grant(Rectangle<i32, Logical>),
    /// A maximized window asked to move or resize: the geometry JWM last
    /// configured is repeated without reaching policy.
    Reply(Rectangle<i32, Logical>),
}

fn route_xwayland_configure_request(
    managed: Option<ManagedXwaylandWindow>,
    current: Rectangle<i32, Logical>,
    x: Option<i32>,
    y: Option<i32>,
    w: Option<u32>,
    h: Option<u32>,
) -> XwaylandConfigureRoute {
    use crate::backend::common_define::ConfigWindowBits;

    let Some(managed) = managed else {
        return XwaylandConfigureRoute::Grant(Rectangle::new(
            (x.unwrap_or(current.loc.x), y.unwrap_or(current.loc.y)).into(),
            (
                w.map_or(current.size.w.max(1), |w| w as i32),
                h.map_or(current.size.h.max(1), |h| h as i32),
            )
                .into(),
        ));
    };
    // A maximized window's geometry belongs to shared policy (the work
    // area); a client self-resize would silently un-maximize it behind JWM's
    // back. A restack-only request, or a window JWM never configured, still
    // goes to policy like any other.
    if managed.maximized
        && (x.is_some() || y.is_some() || w.is_some() || h.is_some())
        && let Some(g) = managed.configured
    {
        return XwaylandConfigureRoute::Reply(Rectangle::new(
            (g.x, g.y).into(),
            (g.w as i32, g.h as i32).into(),
        ));
    }
    let window = managed.window;
    let mut mask = ConfigWindowBits::empty();
    for (named, bit) in [
        (x.is_some(), ConfigWindowBits::X),
        (y.is_some(), ConfigWindowBits::Y),
        (w.is_some(), ConfigWindowBits::WIDTH),
        (h.is_some(), ConfigWindowBits::HEIGHT),
    ] {
        mask.set(bit, named);
    }
    XwaylandConfigureRoute::Policy {
        window,
        mask_bits: mask.bits(),
        changes: crate::backend::api::WindowChanges {
            x,
            y,
            width: w,
            height: h,
            ..Default::default()
        },
    }
}

#[cfg(test)]
mod xwayland_moveresize_tests {
    use super::xwm_resize_edge_direction;
    use smithay::xwayland::xwm::ResizeEdge as XwmResizeEdge;

    #[test]
    fn xwm_resize_edges_match_net_wm_moveresize_codes() {
        assert_eq!(xwm_resize_edge_direction(XwmResizeEdge::TopLeft), 0);
        assert_eq!(xwm_resize_edge_direction(XwmResizeEdge::Top), 1);
        assert_eq!(xwm_resize_edge_direction(XwmResizeEdge::TopRight), 2);
        assert_eq!(xwm_resize_edge_direction(XwmResizeEdge::Right), 3);
        assert_eq!(xwm_resize_edge_direction(XwmResizeEdge::BottomRight), 4);
        assert_eq!(xwm_resize_edge_direction(XwmResizeEdge::Bottom), 5);
        assert_eq!(xwm_resize_edge_direction(XwmResizeEdge::BottomLeft), 6);
        assert_eq!(xwm_resize_edge_direction(XwmResizeEdge::Left), 7);
    }

    #[test]
    fn xwayland_moveresize_stubs_feed_the_shared_drag_pipeline() {
        const SOURCE: &str = include_str!("state.rs");
        let production = SOURCE.split_once("#[cfg(test)]").unwrap().0;
        assert!(
            production.contains("fn xwm_resize_edge_direction"),
            "XWayland resize must map edges onto _NET_WM_MOVERESIZE codes"
        );
        assert!(
            production.contains("BackendEvent::MoveResizeRequest")
                && production.contains("direction: 8"),
            "XWayland move/resize stubs must emit MoveResizeRequest into Jwm drag policy"
        );
        assert!(
            !production.contains("Interactive resize not yet supported for X11 windows.")
                && !production.contains("Interactive move not yet supported for X11 windows."),
            "XWayland interactive move/resize stubs must not remain empty"
        );
    }
}

#[cfg(test)]
mod smithay_feature_follow_tests {
    use super::JwmWaylandState;
    use crate::backend::common_define::WindowId;
    use smithay::xwayland::xwm::{MwmDecorationsHint, MwmHints};

    #[test]
    fn foreign_toplevel_identifier_is_stable_ascii_and_protocol_sized() {
        let win = WindowId::from_raw(0xabcdu64);
        let id = JwmWaylandState::foreign_toplevel_identifier(win);
        assert_eq!(id, "jwm-000000000000abcd");
        assert!(!id.is_empty() && id.len() <= 32 && id.is_ascii());
        assert_eq!(
            JwmWaylandState::foreign_toplevel_identifier(win),
            id,
            "identifier must be stable for the same WindowId"
        );
    }

    #[test]
    fn motif_conversion_marks_empty_decorations_as_borderless() {
        let hints = MwmHints {
            decorations: Some(MwmDecorationsHint::empty()),
            ..MwmHints::default()
        };
        let motif = JwmWaylandState::motif_wm_hints_from_smithay(&hints);
        assert!(motif.decorations_none());
        assert_eq!(motif.flags, 1 << 1);
        assert_eq!(motif.decorations, 0);
    }

    #[test]
    fn motif_conversion_without_decorations_flag_is_not_borderless() {
        let motif = JwmWaylandState::motif_wm_hints_from_smithay(&MwmHints::default());
        assert!(!motif.decorations_none());
        assert_eq!(motif.flags, 0);
    }

    #[test]
    fn property_notify_forwards_motif_hints() {
        const SOURCE: &str = include_str!("state.rs");
        let production = SOURCE.split_once("#[cfg(test)]").unwrap().0;
        assert!(
            production.contains("WmWindowProperty::MotifHints")
                && production.contains("PropertyKind::MotifHints"),
            "XWayland Motif property changes must reach JWM decoration reconcile"
        );
        assert!(
            production.contains("new_toplevel_with_identifier")
                && production.contains("foreign_toplevel_identifier"),
            "ext-foreign-toplevel-list must use a stable WindowId-backed identifier"
        );
        assert!(
            production.contains("let iconic = window.is_hidden()")
                && production.contains("set_hidden(true)"),
            "Iconic MapRequest must re-assert Hidden and skip compositor draw set"
        );
        assert!(
            production.contains("lock_surfaces.clear()")
                && production.contains("session lock requested"),
            "session lock must clear stale surfaces so a Defunct locker can be replaced"
        );
    }
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod lifecycle_tests;

#[cfg(test)]
mod xwayland_legacy_assoc_tests {
    use super::{
        JwmWaylandState, apply_manager_mapping_state, claim_initial_configure_fallback,
        manager_allows_surface_map, match_x11_window_by_surface_id, take_window_mapping_state,
    };
    use crate::backend::common_define::WindowId;
    use std::collections::HashSet;

    #[test]
    fn matches_window_with_equal_surface_id() {
        let a = WindowId::from_raw(1);
        let b = WindowId::from_raw(2);
        let candidates = vec![(a, Some(10u32)), (b, Some(20u32))];
        assert_eq!(match_x11_window_by_surface_id(candidates, 20), Some(b));
    }

    #[test]
    fn no_match_returns_none() {
        let candidates = vec![(WindowId::from_raw(1), Some(10u32))];
        assert_eq!(match_x11_window_by_surface_id(candidates, 99), None);
    }

    #[test]
    fn windows_without_recorded_id_are_ignored() {
        // A protocol id of 0 must not match a window that has no recorded WL_SURFACE_ID (None).
        let a = WindowId::from_raw(1);
        let b = WindowId::from_raw(2);
        let candidates = vec![(a, None), (b, Some(0u32))];
        assert_eq!(
            match_x11_window_by_surface_id(candidates.clone(), 0),
            Some(b)
        );
        // And a None candidate alone never matches.
        assert_eq!(match_x11_window_by_surface_id(vec![(a, None)], 0), None);
    }

    #[test]
    fn empty_candidates_returns_none() {
        let empty: Vec<(WindowId, Option<u32>)> = Vec::new();
        assert_eq!(match_x11_window_by_surface_id(empty, 5), None);
    }

    #[test]
    fn manager_unmap_survives_client_commits_until_explicit_remap() {
        let win = WindowId::from_raw(7);
        let mut mapped = HashSet::from([win]);
        let mut manager_unmapped = HashSet::new();

        apply_manager_mapping_state(&mut mapped, &mut manager_unmapped, win, false);
        assert!(!mapped.contains(&win));
        assert!(!manager_allows_surface_map(&manager_unmapped, win));

        // A later buffer commit must not resurrect a swallowed parent.
        if manager_allows_surface_map(&manager_unmapped, win) {
            mapped.insert(win);
        }
        assert!(!mapped.contains(&win));

        apply_manager_mapping_state(&mut mapped, &mut manager_unmapped, win, true);
        assert!(mapped.contains(&win));
        assert!(manager_allows_surface_map(&manager_unmapped, win));
    }

    #[test]
    fn real_client_unmap_after_manager_hide_is_observable_once() {
        let hidden = WindowId::from_raw(7);
        let visible = WindowId::from_raw(8);
        let mut mapped = HashSet::from([hidden, visible]);
        let mut manager_unmapped = HashSet::new();

        apply_manager_mapping_state(&mut mapped, &mut manager_unmapped, hidden, false);
        assert!(take_window_mapping_state(
            &mut mapped,
            &mut manager_unmapped,
            hidden
        ));
        assert!(manager_unmapped.is_empty());
        assert!(
            !take_window_mapping_state(&mut mapped, &mut manager_unmapped, hidden),
            "duplicate surface-unmap delivery must be suppressed"
        );

        assert!(take_window_mapping_state(
            &mut mapped,
            &mut manager_unmapped,
            visible
        ));
        assert!(!mapped.contains(&visible));
    }

    /// The source text of one item: from `needle` through the closing brace
    /// that matches its first opening brace.
    fn braced_item_after<'a>(source: &'a str, needle: &str) -> &'a str {
        let start = source.find(needle).expect("source item missing");
        let open = start
            + source[start..]
                .find('{')
                .expect("source item has no opening brace");
        let mut depth = 0usize;
        for (offset, byte) in source[open..].bytes().enumerate() {
            match byte {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return &source[start..open + offset + 1];
                    }
                }
                _ => {}
            }
        }
        panic!("source item has no closing brace");
    }

    #[test]
    fn udev_window_ops_publish_manager_mapping_to_shared_state() {
        let backend = include_str!("backend.rs");
        // Each verdict is pinned inside its own method body: the flush the
        // message names is the one that follows that method's mapping write,
        // not any of the two dozen other `request_flush` calls in the file.
        for (method, mapped) in [("map_window", "true"), ("unmap_window", "false")] {
            let body = braced_item_after(backend, &format!("fn {method}(&self, win: WindowId)"));
            let publish = format!("state.set_manager_window_mapped(win, {mapped})");
            let publish_at = body
                .find(&publish)
                .unwrap_or_else(|| panic!("{method} must publish the manager mapping"));
            let flush = format!("self.{}();", "request_flush");
            let flush_at = body
                .find(&flush)
                .unwrap_or_else(|| panic!("{method} must wake the native render loop"));
            assert!(
                publish_at < flush_at,
                "{method}: the wake must follow the mapping write so the loop sees it"
            );
        }
    }

    #[test]
    fn unmanaged_pacing_has_no_periodic_barrier_tree_scan() {
        let state = include_str!("state.rs");
        let production = state.split_once("#[cfg(test)]").unwrap().0;
        assert!(production.contains("CommitTimingManagerState::unmanaged"));
        assert!(production.contains("FifoManagerState::unmanaged"));
        assert!(!production.contains("pub fn signal_due_commit_timing_barriers"));
        assert!(!production.contains("pub fn signal_surface_pacing_barriers"));
        assert!(
            !production.contains("get::<CommitTimerBarrierStateUserData>()"),
            "unmanaged mode must not pretend Smithay created managed barriers"
        );
    }

    #[test]
    fn initial_configure_fallback_is_per_toplevel_and_one_shot() {
        let state = include_str!("state.rs");
        let production = state.split_once("#[cfg(test)]").unwrap().0;
        let arm = format!("Timer::from_duration({})", "INITIAL_CONFIGURE_TIMEOUT");
        assert_eq!(
            production.matches(&arm).count(),
            1,
            "exactly one arming site: the toplevel constructor"
        );
        assert!(production.contains("state.ensure_initial_configure_fallback(win)"));
        assert!(production.contains("TimeoutAction::Drop"));

        // The backends neither arm the timer nor claim the fallback
        // themselves; a second, per-backend timer firing for every toplevel
        // is the duplicate this test exists to keep out.
        for backend in [
            include_str!("backend.rs"),
            include_str!("../wayland_x11/backend.rs"),
            include_str!("../wayland_winit/backend.rs"),
        ] {
            for needle in [
                "ensure_initial_configure_timeout".to_string(),
                "INITIAL_CONFIGURE_TIMEOUT".to_string(),
                format!("ensure_initial_configure_{}(", "fallback"),
            ] {
                assert!(
                    !backend.contains(&needle),
                    "backends must not arm or claim the initial configure fallback: `{needle}`"
                );
            }
        }
    }

    #[test]
    fn initial_configure_claims_are_independent_and_consumed_once() {
        let a = WindowId::from_raw(41);
        let b = WindowId::from_raw(42);
        let mut pending = HashSet::from([a, b]);

        assert!(claim_initial_configure_fallback(&mut pending, a));
        assert!(!claim_initial_configure_fallback(&mut pending, a));
        assert!(pending.contains(&b), "one timer consumed another window");

        // Normal configure/destroy paths use the same removal. Their later
        // timer callback must therefore be a no-op.
        pending.remove(&b);
        assert!(!claim_initial_configure_fallback(&mut pending, b));
    }

    #[test]
    fn independent_first_window_dialog_hint_is_not_popup_like() {
        assert!(!JwmWaylandState::should_honor_dialog_hint(false));
    }

    #[test]
    fn dialog_hint_is_honored_for_parent() {
        assert!(JwmWaylandState::should_honor_dialog_hint(true));
    }

    #[test]
    fn unparented_dialog_hint_is_not_popup_like() {
        assert!(!JwmWaylandState::should_honor_dialog_hint(false));
    }
}

#[cfg(test)]
mod ime_popup_warn_tests {
    use super::ime_popup_warn_due;
    use std::collections::HashSet;

    // `im_popup_positions` swaps its warned set for the failures of the
    // current call at the end of the pass; these tests model that swap
    // directly with plain integer keys (a real key is a popup's `ObjectId`
    // plus the failure kind, and `ObjectId` has no test constructor).

    #[test]
    fn a_persistent_failure_warns_once_not_once_per_frame() {
        let mut warned = HashSet::new();
        // First frame the failure occurs: the warn is due.
        let mut failing = HashSet::new();
        assert!(ime_popup_warn_due(&warned, &mut failing, 7u32));
        warned = failing;
        // Every later frame while it persists: silent, and the key stays.
        for _ in 0..3 {
            let mut failing = HashSet::new();
            assert!(!ime_popup_warn_due(&warned, &mut failing, 7u32));
            warned = failing;
        }
        assert!(warned.contains(&7u32));
    }

    #[test]
    fn a_cleared_failure_may_warn_again() {
        let mut warned = HashSet::new();
        let mut failing = HashSet::new();
        assert!(ime_popup_warn_due(&warned, &mut failing, 7u32));
        warned = failing;
        assert!(warned.contains(&7u32));
        // A call on which the popup no longer fails — or is gone — records
        // nothing, so the swap drops the key...
        warned = HashSet::new();
        assert!(warned.is_empty());
        // ...and the failure's next occurrence warns again.
        let mut failing = HashSet::new();
        assert!(ime_popup_warn_due(&warned, &mut failing, 7u32));
        warned = failing;
        assert!(warned.contains(&7u32));
    }

    #[test]
    fn distinct_failures_of_one_popup_warn_independently() {
        let mut warned = HashSet::new();
        let mut failing = HashSet::new();
        assert!(ime_popup_warn_due(&warned, &mut failing, (7u32, 0u8)));
        assert!(ime_popup_warn_due(&warned, &mut failing, (7u32, 1u8)));
        warned = failing;
        let mut failing = HashSet::new();
        assert!(!ime_popup_warn_due(&warned, &mut failing, (7u32, 0u8)));
        assert!(!ime_popup_warn_due(&warned, &mut failing, (7u32, 1u8)));
        warned = failing;
        assert_eq!(warned.len(), 2);
    }
}

#[cfg(test)]
mod clipboard_io_tests {
    use super::{
        is_png_clipboard_mime, is_text_clipboard_mime, read_clipboard_payload,
        selection_payload_for_mime, set_clipboard_fd_nonblocking, try_acquire_clipboard_io_permit,
        write_clipboard_payload,
    };
    use crate::backend::clipboard_offer::ClipboardOffer;
    use nix::unistd::pipe;
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    fn fill_pipe(writer: &mut File) {
        set_clipboard_fd_nonblocking(writer.as_raw_fd(), true).unwrap();
        let chunk = [0u8; 4096];
        loop {
            match writer.write(&chunk) {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("could not fill test pipe: {error}"),
            }
        }
        // Model the old implementation precisely: its write_all operated on a
        // blocking client fd and would park here forever.
        set_clipboard_fd_nonblocking(writer.as_raw_fd(), false).unwrap();
    }

    #[test]
    fn clipboard_write_deadline_retires_a_client_that_never_reads() {
        let (read_end, write_end) = pipe().unwrap();
        let mut reader = File::from(read_end);
        let mut writer = File::from(write_end);
        fill_pipe(&mut writer);
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = write_clipboard_payload(writer, b"x", Duration::from_millis(40));
            let _ = sender.send(result);
        });

        let result = match receiver.recv_timeout(Duration::from_secs(2)) {
            Ok(result) => result,
            Err(error) => {
                // Free one pipe slot before failing so a regressed blocking
                // write cannot leave the test worker behind.
                let mut slot = [0u8; 4096];
                reader.read_exact(&mut slot).unwrap();
                worker.join().unwrap();
                panic!("clipboard writer did not honor its deadline: {error}");
            }
        };
        worker.join().unwrap();
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn clipboard_read_deadline_retires_an_owner_that_never_writes() {
        let (read_end, write_end) = pipe().unwrap();
        let reader = File::from(read_end);
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = read_clipboard_payload(reader, 16, Duration::from_millis(40));
            let _ = sender.send(result);
        });

        let result = match receiver.recv_timeout(Duration::from_secs(2)) {
            Ok(result) => result,
            Err(error) => {
                // Closing the held-open write end releases a regressed
                // read_to_end before the test reports the failure.
                drop(write_end);
                worker.join().unwrap();
                panic!("clipboard reader did not honor its deadline: {error}");
            }
        };
        drop(write_end);
        worker.join().unwrap();
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn clipboard_worker_permits_bound_concurrency_and_are_reusable() {
        let active = AtomicUsize::new(0);
        let first = try_acquire_clipboard_io_permit(&active, 2).unwrap();
        let second = try_acquire_clipboard_io_permit(&active, 2).unwrap();
        assert!(try_acquire_clipboard_io_permit(&active, 2).is_none());

        drop(first);
        let replacement = try_acquire_clipboard_io_permit(&active, 2).unwrap();
        drop((second, replacement));
        assert_eq!(active.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[test]
    fn text_offers_only_serve_text_mimes() {
        let offer = ClipboardOffer::Text("hello".into());
        assert_eq!(
            selection_payload_for_mime(&offer, "text/plain;charset=utf-8"),
            Some(b"hello".as_slice())
        );
        assert_eq!(
            selection_payload_for_mime(&offer, "UTF8_STRING"),
            Some(b"hello".as_slice())
        );
        assert_eq!(
            selection_payload_for_mime(&offer, "text/html"),
            Some(b"hello".as_slice())
        );
        assert_eq!(selection_payload_for_mime(&offer, "image/png"), None);
        assert!(is_text_clipboard_mime("STRING"));
        assert!(!is_text_clipboard_mime("image/png"));
    }

    #[test]
    fn png_offers_only_serve_image_png() {
        let offer = ClipboardOffer::Png(vec![0x89, 0x50, 0x4e, 0x47]);
        assert_eq!(
            selection_payload_for_mime(&offer, "image/png"),
            Some(&[0x89, 0x50, 0x4e, 0x47][..])
        );
        assert_eq!(
            selection_payload_for_mime(&offer, "IMAGE/PNG"),
            Some(&[0x89, 0x50, 0x4e, 0x47][..])
        );
        assert_eq!(
            selection_payload_for_mime(&offer, "text/plain;charset=utf-8"),
            None
        );
        assert_eq!(selection_payload_for_mime(&offer, "image/jpeg"), None);
        assert!(is_png_clipboard_mime("image/png"));
        assert!(!is_png_clipboard_mime("text/plain"));
    }

    #[test]
    fn send_selection_mime_branching_is_pinned_in_source() {
        const SOURCE: &str = include_str!("state.rs");
        // Exclude this test module so pin strings cannot false-positive.
        let production = SOURCE
            .split_once("mod clipboard_io_tests")
            .map(|(code, _)| code)
            .unwrap_or(SOURCE);
        let handler = production
            .split("impl SelectionHandler for JwmWaylandState")
            .nth(1)
            .expect("SelectionHandler impl");
        assert!(
            handler.contains("selection_payload_for_mime(offer, &mime_type)"),
            "SelectionHandler::send_selection must mime-match before writing"
        );
        assert!(
            production.contains("fn is_text_clipboard_mime")
                && production.contains("fn is_png_clipboard_mime"),
            "text and PNG MIME matchers must exist"
        );
        assert!(
            !handler.contains("clipboard_offered.as_deref()"),
            "send_selection must not treat every offer as bare text"
        );
    }
}

#[cfg(test)]
mod protocol_hardening_tests {
    use super::{
        JwmClientState, JwmWaylandState, ManagedXwaylandWindow, XDG_ACTIVATION_TOKEN_LIFETIME,
        XwaylandConfigureRoute, order_clipboard_captures, output_logical_size,
        route_xwayland_configure_request,
    };
    use crate::backend::api::{BackendEvent, Geometry};
    use crate::backend::clipboard_offer::CapturedClipboard;
    use crate::backend::common_define::{ConfigWindowBits, WindowId};
    use crate::backend::wayland_udev::image_copy_capture::wire_test_client::{Server, test_output};
    use smithay::reexports::wayland_server::Resource;
    use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
    use smithay::utils::{Rectangle, Transform};
    use smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitorSeat;
    use smithay::wayland::security_context::SecurityContext;
    use smithay::wayland::selection::SelectionTarget;
    use smithay::wayland::selection::data_device::current_data_device_selection_userdata;
    use smithay::wayland::session_lock::SessionLockHandler;
    use smithay::wayland::xdg_activation::{
        XdgActivationHandler, XdgActivationToken, XdgActivationTokenData,
    };
    use smithay::wayland::xwayland_keyboard_grab::XWaylandKeyboardGrabHandler;
    use std::io::{ErrorKind, Read, Write};
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// A raw client whose client data the test chooses (the shared wire
    /// client always connects as an ordinary client) and whose server-side
    /// handle can look up the objects it created.
    struct RawClient {
        peer: UnixStream,
        handle: smithay::reexports::wayland_server::Client,
        globals: Vec<(u32, String, u32)>,
        next_id: u32,
    }

    impl RawClient {
        fn connect(server: &mut Server, data: JwmClientState) -> Self {
            let (server_end, peer) = UnixStream::pair().expect("create Wayland socket pair");
            let handle = server
                .display
                .handle()
                .insert_client(server_end, Arc::new(data))
                .expect("insert raw Wayland client");
            peer.set_nonblocking(true)
                .expect("make the test peer non-blocking");
            let mut client = Self {
                peer,
                handle,
                globals: Vec::new(),
                next_id: 2,
            };
            // wl_display.get_registry(new_id=2).
            client.request(1, 1, &[2]);
            server.roundtrip();
            client.globals = client
                .events()
                .into_iter()
                .filter(|(sender, opcode, _)| *sender == 2 && *opcode == 0)
                .map(|(_, _, args)| {
                    let len = args[1] as usize;
                    let bytes: Vec<u8> = args[2..].iter().flat_map(|w| w.to_ne_bytes()).collect();
                    let interface = std::str::from_utf8(&bytes[..len - 1])
                        .expect("registry interface is UTF-8")
                        .to_owned();
                    (args[0], interface, args[2 + len.div_ceil(4)])
                })
                .collect();
            client
        }

        fn advertises(&self, interface: &str) -> bool {
            self.globals.iter().any(|(_, name, _)| name == interface)
        }

        fn new_id(&mut self) -> u32 {
            self.next_id += 1;
            self.next_id
        }

        fn bind(&mut self, interface: &str, version: u32) -> u32 {
            let name = self
                .globals
                .iter()
                .find(|(_, advertised, _)| advertised == interface)
                .map(|(name, _, _)| *name)
                .unwrap_or_else(|| panic!("{interface} is not advertised"));
            self.bind_global(name, version)
        }

        /// Bind the global registered as `name`, for interfaces advertised
        /// more than once (one wl_output per output).
        fn bind_global(&mut self, name: u32, version: u32) -> u32 {
            let (interface, advertised) = self
                .globals
                .iter()
                .find(|(advertised, _, _)| *advertised == name)
                .map(|(_, interface, version)| (interface.clone(), *version))
                .unwrap_or_else(|| panic!("global {name} is not advertised"));
            let id = self.new_id();
            let mut args = vec![name];
            args.extend(string_words(&interface));
            args.extend([version.min(advertised), id]);
            self.request(2, 0, &args);
            id
        }

        /// Registry names of every advertised `interface` global, in
        /// creation order.
        fn global_names(&self, interface: &str) -> Vec<u32> {
            self.globals
                .iter()
                .filter(|(_, advertised, _)| advertised == interface)
                .map(|(name, _, _)| *name)
                .collect()
        }

        fn request(&mut self, object: u32, opcode: u16, args: &[u32]) {
            let size = 8 + 4 * args.len() as u32;
            let mut message = Vec::with_capacity(size as usize);
            message.extend_from_slice(&object.to_ne_bytes());
            message.extend_from_slice(&((size << 16) | u32::from(opcode)).to_ne_bytes());
            for arg in args {
                message.extend_from_slice(&arg.to_ne_bytes());
            }
            self.peer.write_all(&message).expect("send Wayland request");
        }

        /// Every flushed event as (sender, opcode, 32-bit words).
        fn events(&mut self) -> Vec<(u32, u16, Vec<u32>)> {
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                match self.peer.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => bytes.extend_from_slice(&chunk[..read]),
                    Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                    Err(error) => panic!("read Wayland events: {error}"),
                }
            }
            let word = |at: usize| {
                u32::from_ne_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
            };
            let mut events = Vec::new();
            let mut offset = 0;
            while offset + 8 <= bytes.len() {
                let header = word(offset + 4);
                let size = (header >> 16) as usize;
                assert!(size >= 8 && offset + size <= bytes.len(), "truncated event");
                let args = (offset + 8..offset + size).step_by(4).map(word).collect();
                events.push((word(offset), header as u16, args));
                offset += size;
            }
            events
        }

        fn surface(&self, server: &Server, id: u32) -> WlSurface {
            self.handle
                .object_from_protocol_id(&server.display.handle(), id)
                .expect("the client created this wl_surface")
        }
    }

    /// A Wayland string argument as 32-bit words: length with the NUL, then
    /// the padded bytes.
    fn string_words(value: &str) -> Vec<u32> {
        let mut bytes = value.as_bytes().to_vec();
        bytes.push(0);
        let len = bytes.len() as u32;
        bytes.resize(bytes.len().next_multiple_of(4), 0);
        std::iter::once(len)
            .chain(
                bytes
                    .chunks_exact(4)
                    .map(|w| u32::from_ne_bytes([w[0], w[1], w[2], w[3]])),
            )
            .collect()
    }

    /// Client data of a Flatpak app that `creator` connected through a
    /// security-context listener.
    fn sandboxed_client_data(creator: &RawClient) -> JwmClientState {
        JwmClientState {
            security_context: Some(SecurityContext {
                sandbox_engine: Some("org.flatpak".to_owned()),
                app_id: Some("org.example.App".to_owned()),
                instance_id: None,
                creator_client_id: creator.handle.id(),
            }),
            ..JwmClientState::default()
        }
    }

    fn drain_events(server: &Server) -> Vec<BackendEvent> {
        server
            .backend_events
            .lock()
            .expect("backend event lock")
            .drain(..)
            .collect()
    }

    fn created_window(server: &Server) -> WindowId {
        drain_events(server)
            .into_iter()
            .find_map(|event| match event {
                BackendEvent::WindowCreated(window) => Some(window),
                _ => None,
            })
            .expect("the role reaches the backend as a window")
    }

    /// Returns the wl_surface id, the xdg_toplevel id and its window.
    fn create_toplevel(server: &mut Server, client: &mut RawClient) -> (u32, u32, WindowId) {
        let compositor = client.bind("wl_compositor", 6);
        let wm_base = client.bind("xdg_wm_base", 6);
        let surface = client.new_id();
        client.request(compositor, 0, &[surface]);
        let xdg_surface = client.new_id();
        client.request(wm_base, 2, &[xdg_surface, surface]);
        let toplevel = client.new_id();
        client.request(xdg_surface, 1, &[toplevel]);
        server.roundtrip();
        (surface, toplevel, created_window(server))
    }

    #[test]
    fn initial_configure_fallback_configures_without_relocking_the_surface() {
        // The old fallback re-entered the surface state mutex and hung the
        // compositor thread; the channel turns that hang into a failure.
        let (done, outcome) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut server = Server::new();
            let mut client = RawClient::connect(&mut server, JwmClientState::default());
            let (_, toplevel, window) = create_toplevel(&mut server, &mut client);
            server.state.ensure_initial_configure_fallback(window);
            server.roundtrip();
            let configures: Vec<(u32, u32)> = client
                .events()
                .into_iter()
                .filter(|(sender, opcode, _)| *sender == toplevel && *opcode == 0)
                .map(|(_, _, args)| (args[0], args[1]))
                .collect();
            let _ = done.send(configures);
        });
        let configures = outcome
            .recv_timeout(Duration::from_secs(30))
            .expect("the initial configure fallback deadlocked on the surface state mutex");
        assert_eq!(configures, vec![(800, 600)]);
    }

    #[test]
    fn idle_inhibitors_are_counted_and_die_with_their_surface() {
        let mut server = Server::new();
        let mut client = RawClient::connect(&mut server, JwmClientState::default());
        let compositor = client.bind("wl_compositor", 6);
        let manager = client.bind("zwp_idle_inhibit_manager_v1", 1);
        let surface = client.new_id();
        client.request(compositor, 0, &[surface]);
        let first = client.new_id();
        client.request(manager, 1, &[first, surface]);
        let second = client.new_id();
        client.request(manager, 1, &[second, surface]);
        server.roundtrip();
        assert!(server.state.idle_notifier_state.is_inhibited());

        client.request(first, 0, &[]);
        server.roundtrip();
        assert!(
            server.state.idle_notifier_state.is_inhibited(),
            "one inhibitor's destroy must not clear another on the same surface"
        );

        // A player closing its window without destroying its inhibitor:
        // Smithay never calls `uninhibit` for `second`.
        client.request(surface, 0, &[]);
        server.roundtrip();
        assert!(server.state.idle_inhibiting_surfaces.is_empty());
        assert!(!server.state.idle_notifier_state.is_inhibited());

        client.request(second, 0, &[]);
        server.roundtrip();
        assert!(!server.state.idle_notifier_state.is_inhibited());
    }

    #[test]
    fn a_crashed_client_stops_inhibiting_idle() {
        let mut server = Server::new();
        let mut client = RawClient::connect(&mut server, JwmClientState::default());
        let compositor = client.bind("wl_compositor", 6);
        let manager = client.bind("zwp_idle_inhibit_manager_v1", 1);
        let surface = client.new_id();
        client.request(compositor, 0, &[surface]);
        let inhibitor = client.new_id();
        client.request(manager, 1, &[inhibitor, surface]);
        server.roundtrip();
        assert!(server.state.idle_notifier_state.is_inhibited());

        drop(client);
        server.roundtrip();
        assert!(server.state.idle_inhibiting_surfaces.is_empty());
        assert!(!server.state.idle_notifier_state.is_inhibited());
    }

    #[test]
    fn an_x11_copy_retires_the_offered_history_entry() {
        let mut server = Server::new();
        let state = &mut server.state;
        assert!(state.offer_clipboard_text("from history"));
        state.adopt_xwayland_selection(
            SelectionTarget::Clipboard,
            vec!["text/plain;charset=utf-8".to_owned()],
        );
        assert!(
            state.clipboard_offered.is_none(),
            "Wayland pastes would still be served the stale history entry"
        );
        assert!(current_data_device_selection_userdata(&state.seat).is_some());
    }

    #[test]
    fn a_departed_x11_owner_only_clears_its_own_selection() {
        let mut server = Server::new();
        let state = &mut server.state;
        let x11_mimes = || vec!["UTF8_STRING".to_owned()];

        // X11 copy, then a JWM history offer, then the X11 owner exits: the
        // offer is the live selection and must survive.
        state.adopt_xwayland_selection(SelectionTarget::Clipboard, x11_mimes());
        assert!(state.offer_clipboard_text("from history"));
        state.xwayland_selection_cleared(SelectionTarget::Clipboard);
        assert!(state.clipboard_offered.is_some());
        assert!(current_data_device_selection_userdata(&state.seat).is_some());

        // A later X11 copy whose owner exits leaves nothing behind.
        state.adopt_xwayland_selection(SelectionTarget::Clipboard, x11_mimes());
        state.xwayland_selection_cleared(SelectionTarget::Clipboard);
        assert!(current_data_device_selection_userdata(&state.seat).is_none());
    }

    #[test]
    fn x11_pastes_are_served_the_jwm_offer() {
        let mut server = Server::new();
        assert!(server.state.offer_clipboard_text("from history"));
        let (mut read, write) = std::io::pipe().expect("create selection pipe");
        server.state.send_selection_to_xwayland(
            SelectionTarget::Clipboard,
            "text/plain;charset=utf-8".to_owned(),
            write.into(),
        );
        let mut pasted = String::new();
        read.read_to_string(&mut pasted)
            .expect("read the served selection");
        assert_eq!(pasted, "from history");
    }

    /// A managed XWayland window that is not maximized and that JWM never
    /// configured.
    fn ordinary(window: WindowId) -> Option<ManagedXwaylandWindow> {
        Some(ManagedXwaylandWindow {
            window,
            maximized: false,
            configured: None,
        })
    }

    #[test]
    fn managed_xwayland_configure_requests_go_to_policy() {
        let window = WindowId::from_raw(0x71);
        let current = Rectangle::new((10, 20).into(), (300, 200).into());
        match route_xwayland_configure_request(
            ordinary(window),
            current,
            None,
            None,
            Some(400),
            None,
        ) {
            XwaylandConfigureRoute::Policy {
                window: routed,
                mask_bits,
                changes,
            } => {
                assert_eq!(routed, window);
                assert_eq!(mask_bits, ConfigWindowBits::WIDTH.bits());
                assert_eq!(
                    (changes.x, changes.y, changes.width, changes.height),
                    (None, None, Some(400), None)
                );
            }
            other => panic!("a managed window granted its own geometry: {other:?}"),
        }
        match route_xwayland_configure_request(
            ordinary(window),
            current,
            Some(-5),
            Some(7),
            Some(640),
            Some(480),
        ) {
            XwaylandConfigureRoute::Policy { mask_bits, .. } => assert_eq!(
                mask_bits,
                (ConfigWindowBits::X
                    | ConfigWindowBits::Y
                    | ConfigWindowBits::WIDTH
                    | ConfigWindowBits::HEIGHT)
                    .bits()
            ),
            other => panic!("a managed window granted its own geometry: {other:?}"),
        }
    }

    #[test]
    fn unmanaged_xwayland_configure_requests_are_granted_over_the_current_geometry() {
        let current = Rectangle::new((10, 20).into(), (300, 200).into());
        match route_xwayland_configure_request(None, current, Some(50), None, None, Some(90)) {
            XwaylandConfigureRoute::Grant(rect) => {
                assert_eq!(rect, Rectangle::new((50, 20).into(), (300, 90).into()));
            }
            other => panic!("an unmapped window has no policy owner: {other:?}"),
        }
    }

    #[test]
    fn maximized_xwayland_windows_are_answered_with_jwm_geometry() {
        let window = WindowId::from_raw(0x72);
        let current = Rectangle::new((10, 20).into(), (300, 200).into());
        let work_area = Geometry {
            x: 0,
            y: 30,
            w: 1920,
            h: 1050,
            border: 0,
        };
        let maximized = |configured| {
            Some(ManagedXwaylandWindow {
                window,
                maximized: true,
                configured,
            })
        };
        let jwm_rect = Rectangle::new((0, 30).into(), (1920, 1050).into());

        // A self-resize or self-move gets JWM's rectangle back: neither the
        // client's size nor its stale current geometry.
        for (x, y, w, h) in [
            (None, None, Some(400), None),
            (Some(5), Some(5), None, None),
        ] {
            match route_xwayland_configure_request(maximized(Some(work_area)), current, x, y, w, h)
            {
                XwaylandConfigureRoute::Reply(rect) => assert_eq!(rect, jwm_rect),
                other => panic!("a maximized window left the work area: {other:?}"),
            }
        }

        // A restack-only request names no geometry: policy handles it.
        match route_xwayland_configure_request(
            maximized(Some(work_area)),
            current,
            None,
            None,
            None,
            None,
        ) {
            XwaylandConfigureRoute::Policy { mask_bits, .. } => assert_eq!(mask_bits, 0),
            other => panic!("a restack must reach policy: {other:?}"),
        }

        // Without a geometry of JWM's to repeat, policy decides.
        match route_xwayland_configure_request(
            maximized(None),
            current,
            None,
            None,
            Some(400),
            None,
        ) {
            XwaylandConfigureRoute::Policy { mask_bits, .. } => {
                assert_eq!(mask_bits, ConfigWindowBits::WIDTH.bits());
            }
            other => panic!("nothing to answer with: {other:?}"),
        }

        // The gate never catches an ordinary window JWM configured.
        let configured = Some(ManagedXwaylandWindow {
            window,
            maximized: false,
            configured: Some(work_area),
        });
        match route_xwayland_configure_request(configured, current, None, None, Some(400), None) {
            XwaylandConfigureRoute::Policy {
                window: routed,
                mask_bits,
                ..
            } => {
                assert_eq!(routed, window);
                assert_eq!(mask_bits, ConfigWindowBits::WIDTH.bits());
            }
            other => panic!("an ordinary window must reach policy: {other:?}"),
        }
    }

    #[test]
    fn xwayland_configure_request_no_longer_self_publishes_geometry() {
        const SOURCE: &str = include_str!("state.rs");
        let production = SOURCE.split_once("#[cfg(test)]").expect("test split").0;
        let body = production
            .split_once("    fn configure_request(\n")
            .expect("XwmHandler::configure_request")
            .1
            .split_once("\n    fn configure_notify(")
            .expect("configure_notify follows configure_request")
            .0;
        let (before_route, routed) = body
            .split_once("route_xwayland_configure_request(")
            .expect("configure_request routes through the pure router");
        assert!(
            !before_route.contains("window.configure(") && routed.contains("window.configure("),
            "every reply, the maximized one included, must come from the route"
        );
        assert_eq!(body.matches("window.configure(").count(), 1);
        assert!(
            !body.contains("BackendEvent::WindowConfigured"),
            "a client-chosen geometry must not be adopted into the client model"
        );
    }

    #[test]
    fn destroying_a_layer_role_retires_its_window() {
        let mut server = Server::new();
        server.state.outputs.push(test_output("LAYER-1"));
        let mut client = RawClient::connect(&mut server, JwmClientState::default());
        let compositor = client.bind("wl_compositor", 6);
        let layer_shell = client.bind("zwlr_layer_shell_v1", 4);
        let surface = client.new_id();
        client.request(compositor, 0, &[surface]);
        let get_layer_surface = |client: &mut RawClient| {
            let layer = client.new_id();
            // get_layer_surface(id, surface, output = null, layer = top, namespace)
            let mut args = vec![layer, surface, 0, 2];
            args.extend(string_words("bar"));
            client.request(layer_shell, 0, &args);
            layer
        };

        let first_layer = get_layer_surface(&mut client);
        server.roundtrip();
        let first = created_window(&server);
        assert!(server.state.window_layer_info.contains_key(&first));

        // zwlr_layer_surface_v1.destroy; the wl_surface lives on.
        client.request(first_layer, 7, &[]);
        server.roundtrip();
        assert!(
            drain_events(&server)
                .iter()
                .any(|event| matches!(event, BackendEvent::WindowDestroyed(w) if *w == first)),
            "policy must drop the bar whose role is gone"
        );
        assert!(!server.state.window_layer_info.contains_key(&first));
        assert!(!server.state.layer_surfaces.contains_key(&first));

        // The same wl_surface takes a new layer role: only the new window
        // remains, and it is retired the same way.
        let second_layer = get_layer_surface(&mut client);
        server.roundtrip();
        let second = created_window(&server);
        assert_ne!(first, second);
        client.request(second_layer, 7, &[]);
        server.roundtrip();
        assert!(
            drain_events(&server)
                .iter()
                .any(|event| matches!(event, BackendEvent::WindowDestroyed(w) if *w == second))
        );
        assert!(server.state.window_layer_info.is_empty());
    }

    #[test]
    fn lock_surfaces_are_sized_in_logical_output_units() {
        assert_eq!(
            output_logical_size((3840, 2160).into(), 2.0, Transform::Normal),
            (1920, 1080).into()
        );
        assert_eq!(
            output_logical_size((1920, 1080).into(), 1.0, Transform::_90),
            (1080, 1920).into()
        );
        assert_eq!(
            output_logical_size((2560, 1440).into(), 1.5, Transform::Normal),
            (1707, 960).into()
        );
    }

    #[test]
    fn sandboxed_clients_are_not_offered_privileged_globals() {
        const PRIVILEGED: [&str; 10] = [
            "wp_security_context_manager_v1",
            "zwlr_data_control_manager_v1",
            "ext_data_control_manager_v1",
            "zwp_input_method_manager_v2",
            "zwp_virtual_keyboard_manager_v1",
            "ext_session_lock_manager_v1",
            "zwlr_gamma_control_manager_v1",
            // Every window's title and app_id.
            "ext_foreign_toplevel_list_v1",
            // Overlay surfaces with exclusive keyboard focus.
            "zwlr_layer_shell_v1",
            // Tag activity per named output, and switching any monitor's tag.
            "ext_workspace_manager_v1",
        ];
        let mut server = Server::new();
        let trusted = RawClient::connect(&mut server, JwmClientState::default());
        let sandboxed = RawClient::connect(&mut server, sandboxed_client_data(&trusted));
        for interface in PRIVILEGED {
            assert!(
                trusted.advertises(interface),
                "{interface} must stay available to ordinary clients"
            );
            assert!(
                !sandboxed.advertises(interface),
                "{interface} was offered to a sandboxed client"
            );
        }
        assert!(sandboxed.advertises("wl_compositor"));
        assert!(sandboxed.advertises("xdg_wm_base"));
    }

    #[test]
    fn sandboxed_surfaces_never_inhibit_compositor_shortcuts() {
        /// Create a focused toplevel with a shortcuts inhibitor; returns its
        /// surface and window.
        fn focused_inhibitor(server: &mut Server, client: &mut RawClient) -> (WlSurface, WindowId) {
            // zwp_keyboard_shortcuts_inhibit_manager_v1.inhibit_shortcuts
            const INHIBIT_SHORTCUTS: u16 = 1;
            let (surface, _, window) = create_toplevel(server, client);
            let seat = client.bind("wl_seat", 5);
            let manager = client.bind("zwp_keyboard_shortcuts_inhibit_manager_v1", 1);
            server.state.active_toplevel = Some(window);
            let inhibitor = client.new_id();
            client.request(manager, INHIBIT_SHORTCUTS, &[inhibitor, surface, seat]);
            server.roundtrip();
            (client.surface(server, surface), window)
        }
        let inhibiting = |server: &Server, surface: &WlSurface| {
            let inhibitor = server
                .state
                .seat
                .keyboard_shortcuts_inhibitor_for_surface(surface)
                .expect("the inhibitor request is accepted");
            inhibitor.is_active()
        };

        let mut server = Server::new();
        let mut trusted = RawClient::connect(&mut server, JwmClientState::default());
        let data = sandboxed_client_data(&trusted);
        let mut sandboxed = RawClient::connect(&mut server, data);

        let (trusted_surface, trusted_window) = focused_inhibitor(&mut server, &mut trusted);
        assert!(
            inhibiting(&server, &trusted_surface),
            "an ordinary client's focused inhibitor takes effect"
        );
        let (sandboxed_surface, sandboxed_window) = focused_inhibitor(&mut server, &mut sandboxed);
        assert!(
            !inhibiting(&server, &sandboxed_surface),
            "a sandbox must not swallow JWM's bindings"
        );

        // Focus returning to the sandboxed window does not activate it
        // either, while the ordinary inhibitor still follows focus.
        server
            .state
            .sync_keyboard_shortcuts_inhibitors(Some(trusted_window), Some(sandboxed_window));
        assert!(!inhibiting(&server, &sandboxed_surface));
        assert!(!inhibiting(&server, &trusted_surface));
        server
            .state
            .sync_keyboard_shortcuts_inhibitors(Some(sandboxed_window), Some(trusted_window));
        assert!(inhibiting(&server, &trusted_surface));
    }

    #[test]
    fn activation_tokens_are_single_use_and_unused_ones_expire() {
        let mut server = Server::new();
        let mut client = RawClient::connect(&mut server, JwmClientState::default());
        let (surface, _, window) = create_toplevel(&mut server, &mut client);
        let surface = client.surface(&server, surface);

        let (token, data) = {
            let (token, data) = server
                .state
                .xdg_activation_state
                .create_external_token(None);
            (token.clone(), data.clone())
        };
        server
            .state
            .request_activation(token.clone(), data, surface);
        assert!(
            drain_events(&server).iter().any(
                |event| matches!(event, BackendEvent::ActiveWindowMessage { window: w } if *w == window)
            ),
            "a fresh token still activates"
        );
        assert!(
            server
                .state
                .xdg_activation_state
                .data_for_token(&token)
                .is_none(),
            "a used token must not be replayable"
        );

        let stale = XdgActivationTokenData {
            timestamp: Instant::now()
                .checked_sub(XDG_ACTIVATION_TOKEN_LIFETIME + Duration::from_secs(1))
                .expect("the monotonic clock is older than one token lifetime"),
            ..XdgActivationTokenData::default()
        };
        let stale = server
            .state
            .xdg_activation_state
            .create_external_token(stale)
            .0
            .clone();
        let fresh = server
            .state
            .xdg_activation_state
            .create_external_token(None)
            .0
            .clone();
        assert!(server.state.token_created(
            XdgActivationToken::from("client-token".to_owned()),
            XdgActivationTokenData::default(),
        ));
        let state = &server.state.xdg_activation_state;
        assert!(
            state.data_for_token(&stale).is_none(),
            "unused tokens expire"
        );
        assert!(state.data_for_token(&fresh).is_some());
    }

    #[test]
    fn xwayland_keyboard_grabs_resolve_x11_surfaces() {
        let mut server = Server::new();
        let mut client = RawClient::connect(&mut server, JwmClientState::default());
        let compositor = client.bind("wl_compositor", 6);
        let x11_surface = client.new_id();
        client.request(compositor, 0, &[x11_surface]);
        let stray_surface = client.new_id();
        client.request(compositor, 0, &[stray_surface]);
        server.roundtrip();
        let x11_surface = client.surface(&server, x11_surface);
        let stray_surface = client.surface(&server, stray_surface);

        // What `surface_associated` records for a mapped X11 window.
        let window = WindowId::from_raw(0x5151);
        server
            .state
            .surface_to_window
            .insert(x11_surface.id(), window);
        server
            .state
            .x11_wl_surfaces
            .insert(window, x11_surface.clone());

        assert_eq!(
            server.state.keyboard_focus_for_xsurface(&x11_surface),
            Some(x11_surface.clone()),
            "Xwayland's grab must be granted for its own window"
        );
        assert_eq!(
            server.state.keyboard_focus_for_xsurface(&stray_surface),
            None
        );
    }

    #[test]
    fn clipboard_reads_are_delivered_in_selection_order() {
        let mut delivered = 0;
        // Copy 1's read is slow and finishes after copy 2's.
        assert_eq!(
            order_clipboard_captures(vec![(2, "second"), (1, "first")], &mut delivered),
            vec!["first", "second"]
        );
        assert_eq!(delivered, 2);
        assert_eq!(
            order_clipboard_captures(vec![(4, "fourth")], &mut delivered),
            vec!["fourth"]
        );
        // Copy 3 finishes after copy 4 was already recorded as the newest.
        assert!(order_clipboard_captures(vec![(3, "third")], &mut delivered).is_empty());
        assert_eq!(delivered, 4);
    }

    #[test]
    fn drained_clipboard_captures_follow_selection_order() {
        let mut server = Server::new();
        server
            .state
            .clipboard_captured
            .lock()
            .expect("capture lock")
            .extend([
                (2, CapturedClipboard::Text("newer".to_owned())),
                (1, CapturedClipboard::Text("older".to_owned())),
            ]);
        assert_eq!(
            server.state.drain_clipboard_captured(),
            vec![
                CapturedClipboard::Text("older".to_owned()),
                CapturedClipboard::Text("newer".to_owned()),
            ]
        );
    }

    // ext_session_lock_manager_v1.lock / ext_session_lock_v1 wire opcodes.
    const LOCK_REQUEST: u16 = 1;
    const LOCK_DESTROY: u16 = 0;
    const GET_LOCK_SURFACE: u16 = 1;
    const UNLOCK_AND_DESTROY: u16 = 2;
    const LOCKED_EVENT: u16 = 0;
    const FINISHED_EVENT: u16 = 1;
    const LOCK_SURFACE_CONFIGURE_EVENT: u16 = 0;

    /// Send ext_session_lock_manager_v1.lock and dispatch it.
    fn request_lock(server: &mut Server, client: &mut RawClient, manager: u32) -> u32 {
        let lock = client.new_id();
        client.request(manager, LOCK_REQUEST, &[lock]);
        server.roundtrip();
        lock
    }

    /// Flush the server and return the opcodes `object` was sent since the
    /// last read.
    fn opcodes_for(server: &mut Server, client: &mut RawClient, object: u32) -> Vec<u16> {
        server.roundtrip();
        client
            .events()
            .into_iter()
            .filter(|(sender, _, _)| *sender == object)
            .map(|(_, opcode, _)| opcode)
            .collect()
    }

    fn locked_server(outputs: &[&str]) -> (Server, RawClient, u32) {
        let mut server = Server::new();
        server.state.outputs = outputs.iter().map(|name| test_output(name)).collect();
        let mut client = RawClient::connect(&mut server, JwmClientState::default());
        let manager = client.bind("ext_session_lock_manager_v1", 1);
        (server, client, manager)
    }

    #[test]
    fn session_lock_is_confirmed_once_every_output_presented_a_locked_frame() {
        let (mut server, mut client, manager) = locked_server(&["LOCK-1", "LOCK-2"]);
        let lock = request_lock(&mut server, &mut client, manager);
        assert!(
            server.state.session_locked,
            "locked content is drawn from the request on"
        );
        assert!(
            opcodes_for(&mut server, &mut client, lock).is_empty(),
            "`locked` is owed until no output shows unlocked content"
        );

        let epoch = server.state.session_lock_epoch;
        server.state.note_locked_frame_presented("LOCK-1", epoch);
        // A frame rendered for an earlier lock does not pay this one.
        server
            .state
            .note_locked_frame_presented("LOCK-2", epoch.wrapping_sub(1));
        assert!(opcodes_for(&mut server, &mut client, lock).is_empty());
        assert!(server.state.session_lock_confirmation_pending());

        server.state.note_locked_frame_presented("LOCK-2", epoch);
        assert_eq!(opcodes_for(&mut server, &mut client, lock), [LOCKED_EVENT]);
        assert!(!server.state.session_lock_confirmation_pending());
        server.state.note_locked_frame_presented("LOCK-2", epoch);
        assert!(
            opcodes_for(&mut server, &mut client, lock).is_empty(),
            "a lock is confirmed exactly once"
        );
    }

    #[test]
    fn outputs_that_cannot_present_do_not_hold_a_session_lock_back() {
        // No output: nothing unlocked can be on screen.
        let (mut server, mut client, manager) = locked_server(&[]);
        let lock = request_lock(&mut server, &mut client, manager);
        assert_eq!(opcodes_for(&mut server, &mut client, lock), [LOCKED_EVENT]);

        let (mut server, mut client, manager) = locked_server(&["LIT-1", "GONE-1", "OFF-1"]);
        // Soft-disabled at lock time: it owes nothing.
        server
            .state
            .soft_disabled_outputs
            .insert("OFF-1".to_owned());
        let lock = request_lock(&mut server, &mut client, manager);
        server
            .state
            .outputs
            .retain(|output| output.name() != "GONE-1");
        server.state.refresh_output_dependent_state();
        assert!(
            opcodes_for(&mut server, &mut client, lock).is_empty(),
            "LIT-1 still shows unlocked content"
        );
        // The backend reports LIT-1 powered off (DPMS): it shows nothing.
        server
            .state
            .release_session_lock_outputs(|output_name| output_name != "LIT-1");
        assert_eq!(opcodes_for(&mut server, &mut client, lock), [LOCKED_EVENT]);
    }

    #[test]
    fn the_deadline_confirms_a_lock_no_output_presented() {
        let (mut server, mut client, manager) = locked_server(&["STUCK-1"]);
        let lock = request_lock(&mut server, &mut client, manager);
        let epoch = server.state.session_lock_epoch;

        // A deadline armed for an earlier request is a no-op.
        server
            .state
            .confirm_session_lock_after_deadline(epoch.wrapping_sub(1));
        assert!(opcodes_for(&mut server, &mut client, lock).is_empty());

        server.state.confirm_session_lock_after_deadline(epoch);
        assert_eq!(opcodes_for(&mut server, &mut client, lock), [LOCKED_EVENT]);
        // Firing again after the confirmation changes nothing.
        server.state.confirm_session_lock_after_deadline(epoch);
        assert!(opcodes_for(&mut server, &mut client, lock).is_empty());
    }

    #[test]
    fn a_second_lock_request_is_refused_while_the_first_is_pending() {
        let (mut server, mut client, manager) = locked_server(&["LOCK-1"]);
        let first = request_lock(&mut server, &mut client, manager);
        let epoch = server.state.session_lock_epoch;
        let second = request_lock(&mut server, &mut client, manager);
        assert_eq!(
            opcodes_for(&mut server, &mut client, second),
            [FINISHED_EVENT],
            "only one live client locks the session"
        );
        assert!(opcodes_for(&mut server, &mut client, first).is_empty());
        assert_eq!(
            server.state.session_lock_epoch, epoch,
            "a refused request changes nothing"
        );

        // The first request is still the one its locked frame confirms.
        server.state.note_locked_frame_presented("LOCK-1", epoch);
        assert_eq!(opcodes_for(&mut server, &mut client, first), [LOCKED_EVENT]);
    }

    #[test]
    fn unlocking_finishes_a_pending_lock() {
        let (mut server, mut client, manager) = locked_server(&["LOCK-1"]);
        let lock = request_lock(&mut server, &mut client, manager);
        SessionLockHandler::unlock(&mut server.state);
        assert!(!server.state.session_locked);
        assert!(!server.state.session_lock_confirmation_pending());
        assert_eq!(
            opcodes_for(&mut server, &mut client, lock),
            [FINISHED_EVENT],
            "the session it wanted to lock is gone"
        );
    }

    #[test]
    fn a_live_locker_cannot_be_taken_over() {
        let mut server = Server::new();
        let output = test_output("LOCK-1");
        output.create_global::<JwmWaylandState>(&server.display.handle());
        server.state.outputs = vec![output];
        let mut locker = RawClient::connect(&mut server, JwmClientState::default());
        let compositor = locker.bind("wl_compositor", 6);
        let wl_output = locker.bind("wl_output", 4);
        let manager = locker.bind("ext_session_lock_manager_v1", 1);
        let surface = locker.new_id();
        locker.request(compositor, 0, &[surface]);
        let lock = request_lock(&mut server, &mut locker, manager);
        let lock_surface = locker.new_id();
        locker.request(lock, GET_LOCK_SURFACE, &[lock_surface, surface, wl_output]);
        server.roundtrip();
        let epoch = server.state.session_lock_epoch;
        server.state.note_locked_frame_presented("LOCK-1", epoch);
        assert_eq!(opcodes_for(&mut server, &mut locker, lock), [LOCKED_EVENT]);

        // Any other unsandboxed client can bind the manager.
        let mut intruder = RawClient::connect(&mut server, JwmClientState::default());
        let intruder_manager = intruder.bind("ext_session_lock_manager_v1", 1);
        let takeover = request_lock(&mut server, &mut intruder, intruder_manager);
        assert_eq!(
            opcodes_for(&mut server, &mut intruder, takeover),
            [FINISHED_EVENT],
            "a live locker keeps the session"
        );
        assert!(server.state.session_locked);
        assert_eq!(server.state.session_lock_epoch, epoch);
        assert!(!server.state.session_lock_confirmation_pending());
        assert!(
            server.state.lock_surfaces.contains_key("LOCK-1"),
            "the password prompt stays on screen"
        );

        // The refused lock never owned the session: its unlock is a protocol
        // error, not an unlock.
        intruder.request(takeover, UNLOCK_AND_DESTROY, &[]);
        server.roundtrip();
        assert!(server.state.session_locked);

        locker.request(lock, UNLOCK_AND_DESTROY, &[]);
        server.roundtrip();
        assert!(!server.state.session_locked, "the owner still unlocks");
    }

    #[test]
    fn an_abandoned_or_dead_locker_can_be_taken_over() {
        let (mut server, mut first, manager) = locked_server(&["LOCK-1"]);
        // Legal before `locked`: the request is given up, the session stays
        // locked with no live locker.
        let abandoned = request_lock(&mut server, &mut first, manager);
        let abandoned_epoch = server.state.session_lock_epoch;
        first.request(abandoned, LOCK_DESTROY, &[]);
        server.roundtrip();

        let takeover = request_lock(&mut server, &mut first, manager);
        // The abandoned request's frames and deadline pay nothing.
        server
            .state
            .note_locked_frame_presented("LOCK-1", abandoned_epoch);
        server
            .state
            .confirm_session_lock_after_deadline(abandoned_epoch);
        assert!(opcodes_for(&mut server, &mut first, takeover).is_empty());
        let epoch = server.state.session_lock_epoch;
        server.state.note_locked_frame_presented("LOCK-1", epoch);
        assert_eq!(
            opcodes_for(&mut server, &mut first, takeover),
            [LOCKED_EVENT]
        );

        // The owner dies without unlocking (Smithay's `Defunct`): the next
        // locker takes over and can unlock.
        drop(first);
        server.roundtrip();
        assert!(server.state.session_locked);
        let mut next = RawClient::connect(&mut server, JwmClientState::default());
        let next_manager = next.bind("ext_session_lock_manager_v1", 1);
        let next_lock = request_lock(&mut server, &mut next, next_manager);
        let epoch = server.state.session_lock_epoch;
        server.state.note_locked_frame_presented("LOCK-1", epoch);
        assert_eq!(
            opcodes_for(&mut server, &mut next, next_lock),
            [LOCKED_EVENT]
        );
        next.request(next_lock, UNLOCK_AND_DESTROY, &[]);
        server.roundtrip();
        assert!(!server.state.session_locked);
    }

    #[test]
    fn a_lock_abandoned_before_confirmation_keeps_the_session_locked() {
        let (mut server, mut client, manager) = locked_server(&["LOCK-1"]);
        let lock = request_lock(&mut server, &mut client, manager);
        let epoch = server.state.session_lock_epoch;
        // Legal before `locked`: the client gives the request up.
        client.request(lock, LOCK_DESTROY, &[]);
        server.roundtrip();

        server.state.note_locked_frame_presented("LOCK-1", epoch);
        assert!(!server.state.session_lock_confirmation_pending());
        assert!(
            server.state.session_locked,
            "like a locker that dies after `locked`, only a new locker ends it"
        );
        server.state.confirm_session_lock_after_deadline(epoch);
        assert!(opcodes_for(&mut server, &mut client, lock).is_empty());
    }

    #[test]
    fn output_and_tag_changes_reach_output_and_workspace_managers() {
        use crate::backend::wayland_udev::output_management::init_output_management;
        use crate::backend::wayland_udev::workspace_protocol::init_workspace_protocol;
        // zwlr_output_manager_v1.head/done, ext_workspace_manager_v1.done.
        const HEAD_EVENT: u16 = 0;
        const HEADS_DONE_EVENT: u16 = 1;
        const WORKSPACES_DONE_EVENT: u16 = 2;

        let mut server = Server::new();
        let display = server.display.handle();
        server.state.output_management = Some(init_output_management(&display));
        server.state.workspace_state = Some(init_workspace_protocol(&display, 9));
        server.state.outputs = vec![test_output("HEAD-1")];
        let mut client = RawClient::connect(&mut server, JwmClientState::default());
        let heads = client.bind("zwlr_output_manager_v1", 4);
        let workspaces = client.bind("ext_workspace_manager_v1", 1);
        server.roundtrip();
        client.events();

        // A hotplugged monitor: kanshi must hear about the new head.
        server.state.outputs.push(test_output("HEAD-2"));
        server.state.refresh_output_dependent_state();
        let opcodes = opcodes_for(&mut server, &mut client, heads);
        assert!(opcodes.contains(&HEAD_EVENT), "{opcodes:?}");
        assert_eq!(opcodes.last(), Some(&HEADS_DONE_EVENT));

        // A tag switch: waybar must see the new active workspace.
        server
            .state
            .sync_workspace_monitors(&[(0, 0, 0, 64, 48, 0b10)]);
        assert_eq!(
            opcodes_for(&mut server, &mut client, workspaces).last(),
            Some(&WORKSPACES_DONE_EVENT)
        );
    }

    #[test]
    fn workspace_groups_follow_a_rebuilt_output_and_its_late_wl_output() {
        use crate::backend::wayland_udev::workspace_protocol::init_workspace_protocol;
        // ext_workspace_manager_v1.workspace_group/done and
        // ext_workspace_group_handle_v1.output_enter/removed.
        const WORKSPACE_GROUP_EVENT: u16 = 0;
        const WORKSPACES_DONE_EVENT: u16 = 2;
        const OUTPUT_ENTER_EVENT: u16 = 1;
        const GROUP_REMOVED_EVENT: u16 = 5;

        let mut server = Server::new();
        let display = server.display.handle();
        // The configured count: a publish re-sends groups of any other count.
        let tags_length = crate::config::CONFIG.load().tags_length();
        server.state.workspace_state = Some(init_workspace_protocol(&display, tags_length));
        // A KMS rebuild creates a fresh `Output` for the same connector.
        let old_output = test_output("WS-1");
        old_output.create_global::<JwmWaylandState>(&display);
        let new_output = test_output("WS-1");
        new_output.create_global::<JwmWaylandState>(&display);
        server.state.outputs = vec![old_output];
        let mut client = RawClient::connect(&mut server, JwmClientState::default());
        let [old_global, new_global] = client.global_names("wl_output")[..] else {
            panic!("one wl_output global per Output");
        };
        let old_wl_output = client.bind_global(old_global, 4);
        let manager = client.bind("ext_workspace_manager_v1", 1);
        server.roundtrip();
        server
            .state
            .sync_workspace_monitors(&[(0, 0, 0, 64, 48, 0b1)]);
        server.roundtrip();
        let events = client.events();
        let groups = |events: &[(u32, u16, Vec<u32>)]| -> Vec<u32> {
            events
                .iter()
                .filter(|(sender, opcode, _)| {
                    *sender == manager && *opcode == WORKSPACE_GROUP_EVENT
                })
                .map(|(_, _, args)| args[0])
                .collect()
        };
        let entered = |events: &[(u32, u16, Vec<u32>)], group: u32| -> Vec<u32> {
            events
                .iter()
                .filter(|(sender, opcode, _)| *sender == group && *opcode == OUTPUT_ENTER_EVENT)
                .map(|(_, _, args)| args[0])
                .collect()
        };
        let [old_group] = groups(&events)[..] else {
            panic!("one group for the one output: {events:?}");
        };
        assert_eq!(entered(&events, old_group), [old_wl_output]);

        // The rebuild keeps the layout, so policy publishes nothing new; the
        // group must still move to the new output.
        server.state.outputs = vec![new_output];
        server.state.refresh_output_dependent_state();
        server.roundtrip();
        let events = client.events();
        assert!(
            events.contains(&(old_group, GROUP_REMOVED_EVENT, Vec::new())),
            "the replaced output's group is retired: {events:?}"
        );
        let [new_group] = groups(&events)[..] else {
            panic!("the new output gets a group: {events:?}");
        };
        assert!(entered(&events, new_group).is_empty());

        // waybar binds the new wl_output only after the rebuild.
        let new_wl_output = client.bind_global(new_global, 4);
        server.roundtrip();
        let events = client.events();
        assert_eq!(entered(&events, new_group), [new_wl_output]);
        assert_eq!(
            events
                .iter()
                .rfind(|(sender, _, _)| *sender == manager)
                .map(|(_, opcode, _)| *opcode),
            Some(WORKSPACES_DONE_EVENT)
        );
    }

    /// Regression: the refresh after a wlr-randr or kanshi Apply matched
    /// policy's last published monitors to the outputs by origin, but KMS
    /// has moved the outputs by then while policy has not published the new
    /// layout. Swapped outputs were sent each other's active tags.
    #[test]
    fn an_output_refresh_keeps_workspace_groups_of_moved_outputs() {
        use crate::backend::wayland_udev::workspace_protocol::init_workspace_protocol;

        let mut server = Server::new();
        let display = server.display.handle();
        let tags_length = crate::config::CONFIG.load().tags_length();
        server.state.workspace_state = Some(init_workspace_protocol(&display, tags_length));
        let left = test_output("WS-1");
        let right = test_output("WS-2");
        right.change_current_state(None, None, None, Some((64, 0).into()));
        server.state.outputs = vec![left.clone(), right.clone()];
        let mut client = RawClient::connect(&mut server, JwmClientState::default());
        client.bind("ext_workspace_manager_v1", 1);
        server.roundtrip();
        server
            .state
            .sync_workspace_monitors(&[(0, 0, 0, 64, 48, 0b1), (1, 64, 0, 64, 48, 0b10)]);
        server.roundtrip();
        client.events();

        // The Apply swapped the outputs; the KMS sync refreshes before policy
        // hears of the new layout.
        left.change_current_state(None, None, None, Some((64, 0).into()));
        right.change_current_state(None, None, None, Some((0, 0).into()));
        server.state.refresh_output_dependent_state();
        server.roundtrip();
        let events = client.events();
        assert!(
            events.is_empty(),
            "no group may take another output's tags: {events:?}"
        );
    }

    /// Regression: a toplevel's capture sessions learned of the window's
    /// close only on their next `create_frame`, so a paused portal or OBS
    /// stream kept showing a frozen window as live.
    #[test]
    fn closing_a_captured_window_stops_its_idle_capture_session() {
        use crate::backend::wayland_udev::image_copy_capture::init_image_copy_capture;
        // ext_image_copy_capture_session_v1.done/stopped.
        const SESSION_DONE: u16 = 4;
        const SESSION_STOPPED: u16 = 5;
        // xdg_toplevel.destroy.
        const TOPLEVEL_DESTROY: u16 = 0;

        let mut server = Server::new();
        let display = server.display.handle();
        server.state.image_capture_pending = Some(init_image_copy_capture(&display));
        let mut client = RawClient::connect(&mut server, JwmClientState::default());
        let (_, toplevel, window) = create_toplevel(&mut server, &mut client);
        let list = client.bind("ext_foreign_toplevel_list_v1", 1);
        let sources = client.bind("ext_foreign_toplevel_image_capture_source_manager_v1", 1);
        let capture = client.bind("ext_image_copy_capture_manager_v1", 1);
        server.roundtrip();
        let handle = client
            .events()
            .into_iter()
            .find(|(sender, opcode, _)| *sender == list && *opcode == 0)
            .map(|(_, _, args)| args[0])
            .expect("the list announces the window");
        let source = client.new_id();
        client.request(sources, 0, &[source, handle]);
        let session = client.new_id();
        client.request(capture, 0, &[session, source, 0]);
        assert_eq!(
            opcodes_for(&mut server, &mut client, session).last(),
            Some(&SESSION_DONE)
        );
        assert!(server.state.toplevel_capture_sessions.contains_key(&window));

        // No frame is in flight when the window closes.
        client.request(toplevel, TOPLEVEL_DESTROY, &[]);
        assert_eq!(
            opcodes_for(&mut server, &mut client, session),
            [SESSION_STOPPED]
        );
        assert!(server.state.toplevel_capture_sessions.is_empty());
    }

    #[test]
    fn every_window_retirement_stops_its_capture_sessions() {
        const SOURCE: &str = include_str!("state.rs");
        let production = SOURCE.split_once("#[cfg(test)]").expect("test split").0;
        // The helper is the only place a window's geometry, the liveness key
        // of its capture sessions, is dropped.
        assert_eq!(
            production.matches("window_geometry.remove(").count(),
            1,
            "retire a window's geometry through `retire_window_geometry`"
        );
        for (name, call) in [
            ("remove_wayland_window", "self.retire_window_geometry(win)"),
            ("unmapped_window", "self.retire_window_geometry(win_id)"),
            ("destroyed_window", "self.retire_window_geometry(win_id)"),
        ] {
            let body = production
                .split_once(&format!("    fn {name}("))
                .unwrap_or_else(|| panic!("{name} exists"))
                .1
                .split_once("\n    fn ")
                .map_or("", |(body, _)| body);
            assert!(
                body.contains(call),
                "{name} must stop the window's captures"
            );
        }
    }

    #[test]
    fn workspaces_follow_the_configured_tag_count() {
        const SOURCE: &str = include_str!("state.rs");
        let production = SOURCE.split_once("#[cfg(test)]").expect("test split").0;
        let call = production
            .split_once("init_workspace_protocol(")
            .expect("ext-workspace is initialised")
            .1
            .split_once(';')
            .expect("the call ends")
            .0;
        assert!(
            call.contains("cfg.tags_length()"),
            "taskbars must see the configured tags, not a fixed nine: {call}"
        );
    }

    /// Regression: the workspace count was fixed when the global was
    /// created, so after a config reload that changed `tags_length`
    /// taskbars kept offering the old number of workspaces (a click past
    /// the new count activated a tag policy no longer has). The next
    /// publish now re-sends each group with the configured count.
    #[test]
    fn workspace_publish_follows_a_reloaded_tag_count() {
        use crate::backend::wayland_udev::workspace_protocol::init_workspace_protocol;
        // ext_workspace_manager_v1.workspace_group/workspace/done and
        // ext_workspace_group_handle_v1.removed.
        const WORKSPACE_GROUP_EVENT: u16 = 0;
        const WORKSPACE_EVENT: u16 = 1;
        const WORKSPACES_DONE_EVENT: u16 = 2;
        const GROUP_REMOVED_EVENT: u16 = 5;

        let configured = crate::config::CONFIG.load().tags_length();
        // The count the global was created with, before the reload.
        let stale = if configured > 1 { configured - 1 } else { 2 };
        let mut server = Server::new();
        let display = server.display.handle();
        server.state.workspace_state = Some(init_workspace_protocol(&display, stale));
        server.state.outputs = vec![test_output("WS-1")];
        let mut client = RawClient::connect(&mut server, JwmClientState::default());
        let manager = client.bind("ext_workspace_manager_v1", 1);
        server.roundtrip();
        let announced = |events: &[(u32, u16, Vec<u32>)], opcode: u16| -> Vec<u32> {
            events
                .iter()
                .filter(|(sender, event, _)| *sender == manager && *event == opcode)
                .map(|(_, _, args)| args[0])
                .collect()
        };
        let events = client.events();
        let [old_group] = announced(&events, WORKSPACE_GROUP_EVENT)[..] else {
            panic!("one group for the one output: {events:?}");
        };
        assert_eq!(announced(&events, WORKSPACE_EVENT).len(), stale);

        server
            .state
            .sync_workspace_monitors(&[(0, 0, 0, 64, 48, 0b1)]);
        server.roundtrip();
        let events = client.events();
        assert!(
            events.contains(&(old_group, GROUP_REMOVED_EVENT, Vec::new())),
            "the group with the old count is retired: {events:?}"
        );
        let [new_group] = announced(&events, WORKSPACE_GROUP_EVENT)[..] else {
            panic!("the output gets one group with the new count: {events:?}");
        };
        assert_ne!(new_group, old_group);
        assert_eq!(announced(&events, WORKSPACE_EVENT).len(), configured);
        assert_eq!(
            events
                .iter()
                .rfind(|(sender, _, _)| *sender == manager)
                .map(|(_, opcode, _)| *opcode),
            Some(WORKSPACES_DONE_EVENT)
        );
        assert_eq!(
            server
                .state
                .workspace_state
                .as_ref()
                .map(|workspaces| workspaces.tags_length()),
            Some(configured)
        );

        // The count now matches: republishing sends nothing.
        server
            .state
            .sync_workspace_monitors(&[(0, 0, 0, 64, 48, 0b1)]);
        server.roundtrip();
        let events = client.events();
        assert!(
            events.iter().all(|(sender, _, _)| *sender != manager),
            "{events:?}"
        );
    }

    #[test]
    fn lock_surfaces_follow_an_output_resize_while_locked() {
        let mut server = Server::new();
        let output = test_output("LOCK-1");
        output.create_global::<JwmWaylandState>(&server.display.handle());
        server.state.outputs = vec![output.clone()];
        let mut client = RawClient::connect(&mut server, JwmClientState::default());
        let compositor = client.bind("wl_compositor", 6);
        let wl_output = client.bind("wl_output", 4);
        let manager = client.bind("ext_session_lock_manager_v1", 1);
        let surface = client.new_id();
        client.request(compositor, 0, &[surface]);
        let lock = request_lock(&mut server, &mut client, manager);
        let lock_surface = client.new_id();
        client.request(lock, GET_LOCK_SURFACE, &[lock_surface, surface, wl_output]);
        server.roundtrip();

        let configures = |server: &mut Server, client: &mut RawClient| -> Vec<(u32, u32)> {
            server.roundtrip();
            client
                .events()
                .into_iter()
                .filter(|(sender, opcode, _)| {
                    *sender == lock_surface && *opcode == LOCK_SURFACE_CONFIGURE_EVENT
                })
                .map(|(_, _, args)| (args[1], args[2]))
                .collect()
        };
        assert_eq!(configures(&mut server, &mut client), [(64, 48)]);

        // Unchanged layout: no redundant configure.
        server.state.refresh_output_dependent_state();
        assert!(configures(&mut server, &mut client).is_empty());

        // A modeset and a scale change while locked.
        output.change_current_state(
            Some(smithay::output::Mode {
                size: (256, 192).into(),
                refresh: 60_000,
            }),
            None,
            Some(smithay::output::Scale::Integer(2)),
            None,
        );
        server.state.refresh_output_dependent_state();
        assert_eq!(configures(&mut server, &mut client), [(128, 96)]);
    }
}
