use crate::backend::api::OutputInfo;
use crate::backend::api::{
    AllowedAction, BackendEvent, EwmhFeature, HitTarget, IconData, MaximizeAxes, MotifWmHints,
    NetWmAction, NetWmState, NormalHints, PropertyKind, StackMode, StrutPartial, WindowChanges,
    WindowType, WmHints,
};
use crate::backend::common_define::{OutputId, WindowId};
use std::ops::BitOr;

/// Protocol-free flush-batching coordinator shared by both X11 transports.
pub mod batch;
/// Shared generation of the compositor capability trait impls.
pub mod compositor_delegation;
/// Transport-free planning of compositor effects for backend events.
pub mod event_bridge;
/// Generation-fenced state machine for true ICCCM Iconic transitions.
pub(crate) mod iconify;
/// Overflow-safe geometry calculation for interactive X11 moves and resizes.
pub(crate) mod interactive_resize;
/// Sequence-aware classification of JWM-owned X11 unmap requests.
pub(crate) mod managed_unmap;
/// Strict codec for JWM's private minimized-client exec-restart snapshot.
pub(crate) mod minimized_restore;

#[derive(Clone, Copy)]
pub struct WindowTypeAtoms<A> {
    pub desktop: A,
    pub dock: A,
    pub toolbar: A,
    pub menu: A,
    pub utility: A,
    pub splash: A,
    pub dialog: A,
    pub dropdown_menu: A,
    pub popup_menu: A,
    pub tooltip: A,
    pub notification: A,
    pub combo: A,
}

#[derive(Clone, Copy)]
pub struct NetWmStateAtoms<A> {
    pub fullscreen: A,
    pub maximized_vert: A,
    pub maximized_horz: A,
    pub hidden: A,
    pub above: A,
    pub below: A,
    pub demands_attention: A,
    pub sticky: A,
    pub skip_taskbar: A,
    pub skip_pager: A,
}

#[derive(Clone, Copy)]
pub struct AllowedActionAtoms<A> {
    pub move_: A,
    pub resize: A,
    pub minimize: A,
    pub maximize_horz: A,
    pub maximize_vert: A,
    pub fullscreen: A,
    pub close: A,
    pub stick: A,
    pub above: A,
    pub below: A,
}

#[derive(Clone, Copy)]
pub struct EwmhFeatureAtoms<A> {
    pub active_window: A,
    pub supported: A,
    pub wm_name: A,
    pub wm_state: A,
    pub supporting_wm_check: A,
    pub wm_state_fullscreen: A,
    pub wm_state_maximized_vert: A,
    pub wm_state_maximized_horz: A,
    pub wm_state_hidden: A,
    pub wm_state_above: A,
    pub wm_state_below: A,
    pub wm_state_demands_attention: A,
    pub wm_state_sticky: A,
    pub wm_state_skip_taskbar: A,
    pub wm_state_skip_pager: A,
    pub client_list: A,
    pub client_info: A,
    pub wm_window_type: A,
    pub wm_window_type_dialog: A,
    pub current_desktop: A,
    pub number_of_desktops: A,
    pub desktop_names: A,
    pub desktop_viewport: A,
    pub wm_moveresize: A,
    pub frame_extents: A,
    pub wm_allowed_actions: A,
    pub workarea: A,
    pub close_window: A,
    pub restack_window: A,
    pub wm_ping: A,
    pub wm_user_time: A,
    pub wm_icon: A,
    pub wm_bypass_compositor: A,
    pub wm_opaque_region: A,
}

#[derive(Clone, Copy)]
pub struct PropertyKindAtoms<A> {
    pub wm_transient_for: A,
    pub wm_normal_hints: A,
    pub wm_hints: A,
    pub wm_name: A,
    pub net_wm_name: A,
    pub wm_class: A,
    pub net_wm_window_type: A,
    pub wm_protocols: A,
    pub net_wm_strut: A,
    pub net_wm_strut_partial: A,
    pub motif_wm_hints: A,
    pub gtk_frame_extents: A,
    pub net_wm_bypass_compositor: A,
    pub jwm_remote_capture_owner: A,
    pub net_wm_opaque_region: A,
    pub net_wm_icon: A,
    pub net_wm_user_time: A,
}

#[derive(Clone, Copy)]
pub struct ClientMessageAtoms<A> {
    pub net_wm_state: A,
    pub net_active_window: A,
    pub net_close_window: A,
    pub net_wm_moveresize: A,
    pub wm_protocols: A,
    pub net_wm_ping: A,
    pub wm_change_state: A,
}

/// ICCCM `WM_STATE` value asking for a window to be iconified. A client that
/// minimises itself — which is what `XIconifyWindow`, and therefore most
/// toolkits' minimise buttons and most taskbars, ends up doing — sends
/// `WM_CHANGE_STATE` carrying this, not an `_NET_WM_STATE_HIDDEN` request.
pub const ICCCM_ICONIC_STATE: u32 = crate::backend::api::ICCCM_ICONIC_STATE as u32;

pub enum ClientMessageKind {
    WindowState {
        action: NetWmAction,
        first: u32,
        second: u32,
    },
    ActiveWindow,
    CloseWindow,
    MoveResize {
        direction: u32,
        button: u32,
    },
    PingResponse {
        window: u32,
    },
    /// An ICCCM `WM_CHANGE_STATE` asking for the window to be minimised.
    Iconify,
    Other,
}

/// The EWMH hints JWM advertises in `_NET_SUPPORTED`.
///
/// Pagers read this list to decide which requests to send, so a request
/// listed here must actually be acted on. `_NET_RESTACK_WINDOW` is left out on
/// purpose: stacking is owned by policy (above/below state, floating order,
/// per-monitor restack), which ignores a managed client's sibling/stack-mode
/// changes, so a pager's restack request would be dropped or overwritten by
/// the next restack. Advertising it would promise an effect JWM never has.
pub const SUPPORTED_EWMH_FEATURES: &[EwmhFeature] = &[
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
    EwmhFeature::WmMoveResize,
    EwmhFeature::FrameExtents,
    EwmhFeature::WmAllowedActions,
    EwmhFeature::Workarea,
    EwmhFeature::CloseWindow,
    EwmhFeature::WmPing,
    EwmhFeature::WmUserTime,
    EwmhFeature::WmIcon,
    EwmhFeature::WmBypassCompositor,
    EwmhFeature::WmOpaqueRegion,
];

pub fn window_type_from_atom<A: Copy + Eq>(atom: A, atoms: WindowTypeAtoms<A>) -> WindowType {
    if atom == atoms.desktop {
        WindowType::Desktop
    } else if atom == atoms.dock {
        WindowType::Dock
    } else if atom == atoms.toolbar {
        WindowType::Toolbar
    } else if atom == atoms.menu {
        WindowType::Menu
    } else if atom == atoms.utility {
        WindowType::Utility
    } else if atom == atoms.splash {
        WindowType::Splash
    } else if atom == atoms.dialog {
        WindowType::Dialog
    } else if atom == atoms.dropdown_menu {
        WindowType::DropdownMenu
    } else if atom == atoms.popup_menu {
        WindowType::PopupMenu
    } else if atom == atoms.tooltip {
        WindowType::Tooltip
    } else if atom == atoms.notification {
        WindowType::Notification
    } else if atom == atoms.combo {
        WindowType::Combo
    } else {
        WindowType::Unknown
    }
}

pub fn atom_for_net_wm_state<A: Copy>(state: NetWmState, atoms: NetWmStateAtoms<A>) -> A {
    match state {
        NetWmState::Fullscreen => atoms.fullscreen,
        NetWmState::MaximizedVert => atoms.maximized_vert,
        NetWmState::MaximizedHorz => atoms.maximized_horz,
        NetWmState::Hidden => atoms.hidden,
        NetWmState::Above => atoms.above,
        NetWmState::Below => atoms.below,
        NetWmState::DemandsAttention => atoms.demands_attention,
        NetWmState::Sticky => atoms.sticky,
        NetWmState::SkipTaskbar => atoms.skip_taskbar,
        NetWmState::SkipPager => atoms.skip_pager,
    }
}

pub fn net_wm_state_from_atom<A: Copy + Eq>(
    atom: A,
    atoms: NetWmStateAtoms<A>,
) -> Option<NetWmState> {
    Some(if atom == atoms.fullscreen {
        NetWmState::Fullscreen
    } else if atom == atoms.maximized_vert {
        NetWmState::MaximizedVert
    } else if atom == atoms.maximized_horz {
        NetWmState::MaximizedHorz
    } else if atom == atoms.hidden {
        NetWmState::Hidden
    } else if atom == atoms.above {
        NetWmState::Above
    } else if atom == atoms.below {
        NetWmState::Below
    } else if atom == atoms.demands_attention {
        NetWmState::DemandsAttention
    } else if atom == atoms.sticky {
        NetWmState::Sticky
    } else if atom == atoms.skip_taskbar {
        NetWmState::SkipTaskbar
    } else if atom == atoms.skip_pager {
        NetWmState::SkipPager
    } else {
        return None;
    })
}

pub fn atom_for_allowed_action<A: Copy>(action: AllowedAction, atoms: AllowedActionAtoms<A>) -> A {
    match action {
        AllowedAction::Move => atoms.move_,
        AllowedAction::Resize => atoms.resize,
        AllowedAction::Minimize => atoms.minimize,
        AllowedAction::MaximizeHorz => atoms.maximize_horz,
        AllowedAction::MaximizeVert => atoms.maximize_vert,
        AllowedAction::Fullscreen => atoms.fullscreen,
        AllowedAction::Close => atoms.close,
        AllowedAction::Stick => atoms.stick,
        AllowedAction::Above => atoms.above,
        AllowedAction::Below => atoms.below,
    }
}

pub fn atom_for_ewmh_feature<A: Copy>(feature: EwmhFeature, atoms: EwmhFeatureAtoms<A>) -> A {
    match feature {
        EwmhFeature::ActiveWindow => atoms.active_window,
        EwmhFeature::Supported => atoms.supported,
        EwmhFeature::WmName => atoms.wm_name,
        EwmhFeature::WmState => atoms.wm_state,
        EwmhFeature::SupportingWmCheck => atoms.supporting_wm_check,
        EwmhFeature::WmStateFullscreen => atoms.wm_state_fullscreen,
        EwmhFeature::WmStateMaximizedVert => atoms.wm_state_maximized_vert,
        EwmhFeature::WmStateMaximizedHorz => atoms.wm_state_maximized_horz,
        EwmhFeature::WmStateHidden => atoms.wm_state_hidden,
        EwmhFeature::WmStateAbove => atoms.wm_state_above,
        EwmhFeature::WmStateBelow => atoms.wm_state_below,
        EwmhFeature::WmStateDemandsAttention => atoms.wm_state_demands_attention,
        EwmhFeature::WmStateSticky => atoms.wm_state_sticky,
        EwmhFeature::WmStateSkipTaskbar => atoms.wm_state_skip_taskbar,
        EwmhFeature::WmStateSkipPager => atoms.wm_state_skip_pager,
        EwmhFeature::ClientList => atoms.client_list,
        EwmhFeature::ClientInfo => atoms.client_info,
        EwmhFeature::WmWindowType => atoms.wm_window_type,
        EwmhFeature::WmWindowTypeDialog => atoms.wm_window_type_dialog,
        EwmhFeature::CurrentDesktop => atoms.current_desktop,
        EwmhFeature::NumberOfDesktops => atoms.number_of_desktops,
        EwmhFeature::DesktopNames => atoms.desktop_names,
        EwmhFeature::DesktopViewport => atoms.desktop_viewport,
        EwmhFeature::WmMoveResize => atoms.wm_moveresize,
        EwmhFeature::FrameExtents => atoms.frame_extents,
        EwmhFeature::WmAllowedActions => atoms.wm_allowed_actions,
        EwmhFeature::Workarea => atoms.workarea,
        EwmhFeature::CloseWindow => atoms.close_window,
        EwmhFeature::RestackWindow => atoms.restack_window,
        EwmhFeature::WmPing => atoms.wm_ping,
        EwmhFeature::WmUserTime => atoms.wm_user_time,
        EwmhFeature::WmIcon => atoms.wm_icon,
        EwmhFeature::WmBypassCompositor => atoms.wm_bypass_compositor,
        EwmhFeature::WmOpaqueRegion => atoms.wm_opaque_region,
    }
}

pub fn property_kind_from_atom<A: Copy + Eq>(atom: A, atoms: PropertyKindAtoms<A>) -> PropertyKind {
    if atom == atoms.wm_transient_for {
        PropertyKind::TransientFor
    } else if atom == atoms.wm_normal_hints {
        PropertyKind::SizeHints
    } else if atom == atoms.wm_hints {
        PropertyKind::Urgency
    } else if atom == atoms.wm_name || atom == atoms.net_wm_name {
        PropertyKind::Title
    } else if atom == atoms.wm_class {
        PropertyKind::Class
    } else if atom == atoms.net_wm_window_type {
        PropertyKind::WindowType
    } else if atom == atoms.wm_protocols {
        PropertyKind::Protocols
    } else if atom == atoms.net_wm_strut || atom == atoms.net_wm_strut_partial {
        PropertyKind::Strut
    } else if atom == atoms.motif_wm_hints {
        PropertyKind::MotifHints
    } else if atom == atoms.gtk_frame_extents {
        PropertyKind::GtkFrameExtents
    } else if atom == atoms.net_wm_bypass_compositor {
        PropertyKind::BypassCompositor
    } else if atom == atoms.jwm_remote_capture_owner {
        PropertyKind::RemoteCapture
    } else if atom == atoms.net_wm_opaque_region {
        PropertyKind::OpaqueRegion
    } else if atom == atoms.net_wm_icon {
        PropertyKind::NetWmIcon
    } else if atom == atoms.net_wm_user_time {
        PropertyKind::UserTime
    } else {
        PropertyKind::Other
    }
}

pub fn net_wm_action_from_raw(action: u32) -> Option<NetWmAction> {
    match action {
        0 => Some(NetWmAction::Remove),
        1 => Some(NetWmAction::Add),
        2 => Some(NetWmAction::Toggle),
        _ => None,
    }
}

pub fn classify_client_message(
    type_: u32,
    format: u8,
    data: [u32; 5],
    atoms: ClientMessageAtoms<u32>,
) -> ClientMessageKind {
    if type_ == atoms.net_wm_state && format == 32 {
        if let Some(action) = net_wm_action_from_raw(data[0]) {
            return ClientMessageKind::WindowState {
                action,
                first: data[1],
                second: data[2],
            };
        }
    }
    if type_ == atoms.net_active_window {
        return ClientMessageKind::ActiveWindow;
    }
    if type_ == atoms.net_close_window {
        return ClientMessageKind::CloseWindow;
    }
    if type_ == atoms.net_wm_moveresize && format == 32 {
        return ClientMessageKind::MoveResize {
            direction: data[2],
            button: data[3],
        };
    }
    if type_ == atoms.wm_protocols && format == 32 && data[0] == atoms.net_wm_ping {
        return ClientMessageKind::PingResponse { window: data[2] };
    }
    if type_ == atoms.wm_change_state && format == 32 && data[0] == ICCCM_ICONIC_STATE {
        return ClientMessageKind::Iconify;
    }
    ClientMessageKind::Other
}

/// Decode an EWMH `_NET_CURRENT_DESKTOP` ClientMessage into the tag switch it
/// asks for.
///
/// This is the request a pager click, `wmctrl -s N` or `xdotool set_desktop N`
/// sends to the root window, and EWMH requires pagers to switch desktops with
/// it. [`classify_client_message`] reports it as [`ClientMessageKind::Other`],
/// so an X11 transport must run this on that fallback, before forwarding the
/// generic `ClientMessage`, for the request to reach policy.
///
/// JWM publishes `_NET_CURRENT_DESKTOP` as the index of the lowest active tag
/// of the selected monitor and `_NET_NUMBER_OF_DESKTOPS` as the tag count, so
/// the inverse of a request for desktop `data[0]` is the single-tag mask
/// `1 << data[0]`. The result is the same [`BackendEvent::WorkspaceActivate`]
/// the Wayland workspace protocol produces, and policy applies it to the
/// selected monitor, the one `_NET_CURRENT_DESKTOP` describes. The request
/// names no monitor, so `monitor` is `None`.
///
/// The index is written by the client. An index at or past
/// `number_of_desktops`, or one that does not fit in a `u32` mask, returns
/// `None`: turned into a mask anyway, it would either wrap onto a real tag or
/// reach policy as an empty view that refocuses and re-arranges for nothing.
pub fn current_desktop_request(
    type_: u32,
    format: u8,
    data: [u32; 5],
    net_current_desktop: u32,
    number_of_desktops: u32,
) -> Option<BackendEvent> {
    if type_ != net_current_desktop || format != 32 {
        return None;
    }
    let index = data[0];
    if index >= number_of_desktops {
        return None;
    }
    let tag_mask = 1u32.checked_shl(index)?;
    Some(BackendEvent::WorkspaceActivate {
        monitor: None,
        tag_mask,
    })
}

/// The event for a ClientMessage that [`classify_client_message`] reports as
/// [`ClientMessageKind::Other`].
///
/// A pager's `_NET_CURRENT_DESKTOP` request, which EWMH addresses to the root
/// window, becomes the tag switch it asks for (see
/// [`current_desktop_request`]). Anything else, a desktop request that is
/// malformed, out of range or addressed elsewhere included, reaches policy
/// unchanged as the generic [`BackendEvent::ClientMessage`] it always was.
/// Both X11 transports decode through this one function so they cannot
/// disagree about which messages switch tags.
pub fn unclassified_client_message_event(
    window: WindowId,
    to_root: bool,
    type_: u32,
    format: u8,
    data: [u32; 5],
    net_current_desktop: u32,
    number_of_desktops: u32,
) -> BackendEvent {
    to_root
        .then(|| {
            current_desktop_request(type_, format, data, net_current_desktop, number_of_desktops)
        })
        .flatten()
        .unwrap_or(BackendEvent::ClientMessage {
            window,
            type_,
            data,
            format,
        })
}

/// Whether a `PropertyNotify` reaches the window manager. A new value
/// always does. A deletion does for the kinds whose consumers read the
/// property again and act on its absence: a deleted strut must release
/// its reservation, deleted size hints must drop the cached constraints,
/// transient-for is re-read by its handler, a withdrawn bypass request
/// must redirect the window again, the remote-capture marker going away
/// ends the capture, and a withdrawn Motif or GTK frame hint must restore
/// the JWM border (the decoration reconcile re-reads both hints). Deletions
/// of other kinds stay ignored, as they always have been.
///
/// Both X11 transports filter through this one list: a private copy per
/// transport once let a dock's deleted strut release its reservation on one
/// and keep it on the other.
pub fn forwards_property_notify(deleted: bool, kind: PropertyKind) -> bool {
    !deleted
        || matches!(
            kind,
            PropertyKind::Strut
                | PropertyKind::SizeHints
                | PropertyKind::TransientFor
                | PropertyKind::BypassCompositor
                | PropertyKind::RemoteCapture
                | PropertyKind::MotifHints
                | PropertyKind::GtkFrameExtents
        )
}

/// The kinds of asynchronous X protocol error [`protocol_error_log_level`]
/// tells apart, whichever transport decoded the error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtocolErrorClass {
    /// Core `BadWindow`.
    Window,
    /// Core `BadDrawable`.
    Drawable,
    /// Core `BadPixmap`.
    Pixmap,
    /// DAMAGE `BadDamage`.
    Damage,
    /// Core `BadMatch`.
    Match,
    /// Any other error.
    Other,
}

/// Core-protocol major opcode of `ConfigureWindow`.
pub const CONFIGURE_WINDOW_OPCODE: u8 = 12;
/// Core-protocol major opcode of `SetInputFocus`.
pub const SET_INPUT_FOCUS_OPCODE: u8 = 42;

/// The log level for an asynchronous X protocol error, the only report an
/// unchecked request ever gets. A stale window, drawable, pixmap or damage
/// id, or `BadMatch` from focusing or configuring a window, is the routine
/// race with a client that has just unmapped or destroyed it, so it stays at
/// debug; logging it as an error made closing a window during a layout pass
/// look like a failure. Anything else means JWM sent a bad request, which
/// deserves a warning rather than silence. `major_opcode` is the failed
/// request's; it only matters for `BadMatch`.
pub fn protocol_error_log_level(class: ProtocolErrorClass, major_opcode: u8) -> log::Level {
    let window_race = match class {
        ProtocolErrorClass::Window
        | ProtocolErrorClass::Drawable
        | ProtocolErrorClass::Pixmap
        | ProtocolErrorClass::Damage => true,
        ProtocolErrorClass::Match => {
            matches!(
                major_opcode,
                SET_INPUT_FOCUS_OPCODE | CONFIGURE_WINDOW_OPCODE
            )
        }
        ProtocolErrorClass::Other => false,
    };
    if window_race {
        log::Level::Debug
    } else {
        log::Level::Warn
    }
}

/// Keeps `SIGCHLD` blocked in the calling thread while an X11 backend spawns
/// its worker threads (compositor, clipboard, tray), then restores the
/// caller's previous mask.
///
/// calloop's `Signals` source, created later in `run`, blocks `SIGCHLD`
/// only in the thread that creates it and reads it through a signalfd. Any
/// other thread that leaves it unblocked is an eligible target for the
/// process-directed signal; with the default disposition that thread
/// discards it, the signalfd never becomes readable, and child reaping
/// falls back to the one-second insurance poll. Threads inherit the
/// spawner's mask, so blocking around the spawns is enough. The calling
/// thread's own mask is restored so processes it launches before `run`
/// keep the mask they had before.
pub(crate) struct SigchldBlockedForSpawns {
    previous: Option<nix::sys::signal::SigSet>,
}

impl SigchldBlockedForSpawns {
    pub(crate) fn new() -> Self {
        let mut sigchld = nix::sys::signal::SigSet::empty();
        sigchld.add(nix::sys::signal::Signal::SIGCHLD);
        match sigchld.thread_swap_mask(nix::sys::signal::SigmaskHow::SIG_BLOCK) {
            Ok(previous) => Self {
                previous: Some(previous),
            },
            Err(error) => {
                log::warn!("could not block SIGCHLD for backend worker threads: {error}");
                Self { previous: None }
            }
        }
    }
}

impl Drop for SigchldBlockedForSpawns {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take()
            && let Err(error) = previous.thread_set_mask()
        {
            log::warn!("could not restore the signal mask after spawning backend threads: {error}");
        }
    }
}

/// Resolve which output a pointer event over the background landed on, and
/// invalidate the output cache when the screen layout changes.
///
/// Both X11 transports run this identical, protocol-neutral step on the
/// events they decode: a `ButtonPress`/`MotionNotify` whose hit target is the
/// background gets its `output` filled from `output_at(root_x, root_y)`, and
/// `ScreenLayoutChanged` triggers `invalidate_cache`. Other events, and
/// pointer events already resolved to a surface, are left untouched.
pub fn enrich_background_event<Lookup, Invalidate>(
    event: &mut BackendEvent,
    output_at: Lookup,
    invalidate_cache: Invalidate,
) where
    Lookup: FnOnce(i32, i32) -> Option<OutputId>,
    Invalidate: FnOnce(),
{
    match event {
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
                    output: output_at(*root_x as i32, *root_y as i32),
                };
            }
        }
        BackendEvent::ScreenLayoutChanged => invalidate_cache(),
        _ => {}
    }
}

/// One `_NET_WM_STATE` ClientMessage -> policy events in message order. Decoded
/// MaximizedVert/MaximizedHorz atoms merge into ONE
/// `BackendEvent::WindowMaximizeRequest { window, action, axes }` placed where the
/// first maximize atom appeared; every other decoded atom becomes a
/// `WindowStateRequest`. Zero, unknown and repeated atoms are skipped. An empty
/// result keeps the transports' generic ClientMessage fallback.
pub fn expand_net_wm_state_requests<F>(
    window: WindowId,
    action: NetWmAction,
    first: u32,
    second: u32,
    mut decode_state: F,
) -> Vec<BackendEvent>
where
    F: FnMut(u32) -> Option<NetWmState>,
{
    // Pagers and toolkits maximize by naming both axes in one message. Two
    // per-axis events would run two policy transactions, and a Toggle would
    // flip each axis on its own (vert on, horz off when only one was set),
    // so the axes of one message travel together.
    let mut events = Vec::with_capacity(2);
    let mut axes = MaximizeAxes::NONE;
    let mut maximize_index = None;
    for (index, atom) in [first, second].into_iter().enumerate() {
        // A repeated atom names the same state twice; acting on it twice
        // would make a Toggle cancel itself.
        if atom == 0 || (index == 1 && atom == first) {
            continue;
        }
        let Some(state) = decode_state(atom) else {
            continue;
        };
        if let Some(axis) = MaximizeAxes::from_net_wm_state(state) {
            axes = axes.union(axis);
            maximize_index.get_or_insert(events.len());
        } else {
            events.push(BackendEvent::WindowStateRequest {
                window,
                action,
                state,
            });
        }
    }
    if let Some(index) = maximize_index {
        events.insert(
            index,
            BackendEvent::WindowMaximizeRequest {
                window,
                action,
                axes,
            },
        );
    }
    events
}

/// `current` with only the two maximize atoms rewritten to `axes`. If the list
/// already has exactly the requested membership and no duplicate maximize atom,
/// it is returned unchanged (order preserved); otherwise every other atom keeps
/// its order, all maximize atoms are removed, and `vert` then `horz` are appended
/// as requested.
pub fn with_maximize_atoms<A: Copy + PartialEq>(
    current: &[A],
    vert: A,
    horz: A,
    axes: MaximizeAxes,
) -> Vec<A> {
    let count = |wanted: A| current.iter().filter(|&&atom| atom == wanted).count();
    // Returning the list untouched lets both transports skip the property
    // write entirely, so a republish of unchanged state sends no
    // PropertyNotify to the client or to pagers.
    if count(vert) == usize::from(axes.vert) && count(horz) == usize::from(axes.horz) {
        return current.to_vec();
    }
    let mut next: Vec<A> = current
        .iter()
        .copied()
        .filter(|&atom| atom != vert && atom != horz)
        .collect();
    if axes.vert {
        next.push(vert);
    }
    if axes.horz {
        next.push(horz);
    }
    next
}

pub fn stack_mode_from_index(index: u8) -> Option<StackMode> {
    match index {
        0 => Some(StackMode::Above),
        1 => Some(StackMode::Below),
        2 => Some(StackMode::TopIf),
        3 => Some(StackMode::BottomIf),
        4 => Some(StackMode::Opposite),
        _ => None,
    }
}

pub fn stack_mode_to_index(mode: StackMode) -> u8 {
    match mode {
        StackMode::Above => 0,
        StackMode::Below => 1,
        StackMode::TopIf => 2,
        StackMode::BottomIf => 3,
        StackMode::Opposite => 4,
    }
}

pub fn window_changes_from_configure_request_parts(
    x: Option<i32>,
    y: Option<i32>,
    width: Option<u32>,
    height: Option<u32>,
    border_width: Option<u32>,
    sibling: Option<WindowId>,
    stack_mode: Option<StackMode>,
) -> WindowChanges {
    WindowChanges {
        x,
        y,
        width,
        height,
        border_width,
        sibling,
        stack_mode,
    }
}

pub fn restack_window_changes(windows: &[WindowId]) -> Vec<(WindowId, WindowChanges)> {
    let mut changes = Vec::new();
    let Some((&first, rest)) = windows.split_first() else {
        return changes;
    };

    changes.push((
        first,
        WindowChanges {
            stack_mode: Some(StackMode::Above),
            ..Default::default()
        },
    ));

    let mut prev = first;
    for &window in rest {
        changes.push((
            window,
            WindowChanges {
                sibling: Some(prev),
                stack_mode: Some(StackMode::Above),
                ..Default::default()
            },
        ));
        prev = window;
    }

    changes
}

pub fn lock_modifier_combinations<M>(base: M, caps_lock: M, numlock: M) -> [M; 4]
where
    M: Copy + BitOr<Output = M>,
{
    [
        base,
        base | caps_lock,
        base | numlock,
        base | caps_lock | numlock,
    ]
}

pub fn protocol_supported<A: Copy + Eq>(protocols: &[A], protocol: A) -> bool {
    protocols.contains(&protocol)
}

pub const DEFAULT_OUTPUT_REFRESH_MHZ: u32 = 60_000;

/// Convert the backend-facing millihertz representation to the nearest whole Hz.
///
/// X11 output enumeration preserves fractional rates such as 120.081 Hz as
/// 120_081 mHz. Compositor policy (blur tiers, frame pacing) intentionally uses
/// whole Hz and must never consume the raw millihertz value directly.
pub fn refresh_millihz_to_hz(refresh_millihz: u32) -> u32 {
    if refresh_millihz == 0 {
        return 0;
    }
    ((refresh_millihz as u64 + 500) / 1000).clamp(1, u32::MAX as u64) as u32
}

/// Calculate a rounded whole-Hz refresh rate from a RandR mode.
pub fn mode_refresh_hz(dot_clock: u32, htotal: u16, vtotal: u16) -> u32 {
    if dot_clock == 0 || htotal == 0 || vtotal == 0 {
        return 60;
    }
    let denominator = htotal as u64 * vtotal as u64;
    let refresh_millihz =
        ((dot_clock as u64 * 1000 + denominator / 2) / denominator).min(u32::MAX as u64) as u32;
    refresh_millihz_to_hz(refresh_millihz).max(1)
}

/// Primary-monitor refresh rate in both the backend-facing millihertz
/// representation and the whole-Hz form compositor policy consumes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrimaryRefresh {
    pub millihz: u32,
    pub hz: u32,
}

/// Derive the primary-monitor refresh rate for compositor construction: the
/// first output reporting a positive rate wins, otherwise the 60 Hz default
/// applies. The whole-Hz form is never zero.
pub fn primary_refresh(outputs: &[OutputInfo]) -> PrimaryRefresh {
    let millihz = outputs
        .iter()
        .find_map(|output| (output.refresh_rate > 0).then_some(output.refresh_rate))
        .unwrap_or(DEFAULT_OUTPUT_REFRESH_MHZ);
    PrimaryRefresh {
        millihz,
        hz: refresh_millihz_to_hz(millihz).max(1),
    }
}

pub fn output_at(outputs: &[OutputInfo], x: i32, y: i32) -> Option<OutputId> {
    outputs.iter().find_map(|output| {
        if x >= output.x
            && x < output.x + output.width
            && y >= output.y
            && y < output.y + output.height
        {
            Some(output.id)
        } else {
            None
        }
    })
}

pub fn fallback_output(name: &str, width: i32, height: i32) -> OutputInfo {
    build_output_info(
        OutputId(0),
        name.to_string(),
        0,
        0,
        width,
        height,
        DEFAULT_OUTPUT_REFRESH_MHZ,
        false,
        None,
    )
}

pub fn build_output_info(
    id: OutputId,
    name: String,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    refresh_rate: u32,
    hdr_capable: bool,
    hdr_metadata: Option<crate::backend::edid::EdidHdrCapabilities>,
) -> OutputInfo {
    let identity = crate::backend::api::OutputIdentity::connector_only(name.clone());
    OutputInfo {
        id,
        name,
        x,
        y,
        width,
        height,
        scale: 1.0,
        refresh_rate,
        hdr_capable,
        hdr_metadata,
        identity,
    }
}

pub fn wm_delete_window_message(protocol: u32) -> [u32; 5] {
    [protocol, 0, 0, 0, 0]
}

pub fn wm_take_focus_message(protocol: u32, timestamp: u32) -> [u32; 5] {
    [protocol, timestamp, 0, 0, 0]
}

pub fn net_wm_ping_message(protocol: u32, timestamp: u32, window: u32) -> [u32; 5] {
    [protocol, timestamp, window, 0, 0]
}

pub fn net_wm_sync_request_message(protocol: u32, timestamp: u32, value: u64) -> [u32; 5] {
    let lo = (value & 0xFFFF_FFFF) as u32;
    let hi = (value >> 32) as u32;
    [protocol, timestamp, lo, hi, 0]
}

pub fn parse_wm_class(raw: &[u8]) -> (String, String) {
    let mut parts = raw.split(|&b| b == 0).filter(|part| !part.is_empty());
    (
        decode_x11_string(parts.next().unwrap_or_default()).to_lowercase(),
        decode_x11_string(parts.next().unwrap_or_default()).to_lowercase(),
    )
}

pub fn decode_text_property<A: Copy + Eq>(
    bytes: &[u8],
    property_type: A,
    utf8_string: A,
    string: A,
) -> Option<String> {
    let value = bytes.strip_suffix(&[0]).unwrap_or(bytes);
    if property_type == utf8_string {
        decode_utf8(value)
    } else if property_type == string {
        Some(decode_latin1(value))
    } else {
        decode_utf8(value).or_else(|| Some(decode_latin1(value)))
    }
}

pub fn parse_wm_hints(values: &[u32]) -> Option<WmHints> {
    let flags = *values.first()?;
    Some(WmHints {
        urgent: flags & (1 << 8) != 0,
        input: if flags & 1 != 0 {
            Some(values.get(1).copied().unwrap_or(0) != 0)
        } else {
            None
        },
    })
}

/// Largest width or height an X11 `ConfigureWindow` accepts (a CARD16).
///
/// `WM_NORMAL_HINTS` words are client-written INT32s with no server-side
/// validation, and the size-hint consumers (`calculate_constrained_size`,
/// `total_width()`) do plain `i32` arithmetic on them. A hint the server could
/// never satisfy is therefore recorded as absent (`0`, the convention every
/// consumer already uses) instead of being carried as `i32::MAX` into a
/// subtraction that overflows or a `ConfigureWindow` the server rejects.
const MAX_NORMAL_HINT_DIMENSION: u32 = u16::MAX as u32;

/// Widest aspect band a hint may ask for, as `w/h` and `h/w`. Real clients
/// stay within a few to one; the bound exists so `h * aspect` for any
/// displayable `h` stays far inside `i32` instead of saturating to `i32::MAX`
/// when a client writes a ratio like `0xFFFF_FFFF : 1`.
const MAX_NORMAL_HINT_ASPECT: f32 = 256.0;

/// One `WM_NORMAL_HINTS` dimension word, or `0` (absent) when it exceeds what
/// the X server could ever configure. Negative INT32s arrive as words above
/// `i32::MAX` and land in the same bucket.
fn bounded_normal_hint_dimension(word: u32) -> i32 {
    if word > MAX_NORMAL_HINT_DIMENSION {
        0
    } else {
        word as i32
    }
}

/// The `PAspect` pair, or `(0.0, 0.0)` (absent) unless both ratios are
/// well-formed and inside the sane band. The two are only ever applied
/// together, so a bad half disables the pair the same way a zero denominator
/// always has.
///
/// Dropping both when only one is out of band looks lossy — a client asking
/// "never narrower than 4:3, no practical maximum" appears to lose its
/// minimum too — but the only consumer is the WM's aspect-ratio constraint,
/// whose gate is `min_aspect > 0.0 && max_aspect > 0.0`. A surviving half
/// would be read by nobody, while making `0.0` mean "absent" in one field and
/// "the pair is half-present" in the other. Keeping the halves independent is
/// therefore a change to that gate, not to this parser: this side is pinned
/// by `a_half_aspect_pair_is_recorded_absent`, and the gate that makes it
/// harmless by `a_half_aspect_pair_constrains_nothing` beside the consumer.
fn bounded_normal_hint_aspects(
    min_num: u32,
    min_den: u32,
    max_num: u32,
    max_den: u32,
) -> (f32, f32) {
    fn ratio(num: u32, den: u32) -> Option<f32> {
        if num == 0 || den == 0 {
            return None;
        }
        let value = num as f32 / den as f32;
        (1.0 / MAX_NORMAL_HINT_ASPECT..=MAX_NORMAL_HINT_ASPECT)
            .contains(&value)
            .then_some(value)
    }
    match (ratio(min_num, min_den), ratio(max_num, max_den)) {
        (Some(min_aspect), Some(max_aspect)) => (min_aspect, max_aspect),
        _ => (0.0, 0.0),
    }
}

pub fn parse_normal_hints(values: &[u32]) -> Option<NormalHints> {
    if values.len() < 18 {
        return None;
    }

    const P_MIN_SIZE: u32 = 1 << 4;
    const P_MAX_SIZE: u32 = 1 << 5;
    const P_RESIZE_INC: u32 = 1 << 6;
    const P_ASPECT: u32 = 1 << 7;
    const P_BASE_SIZE: u32 = 1 << 8;

    let flags = values[0];
    let mut base_w = 0;
    let mut base_h = 0;
    let mut inc_w = 0;
    let mut inc_h = 0;
    let mut max_w = 0;
    let mut max_h = 0;
    let mut min_w = 0;
    let mut min_h = 0;
    let mut min_aspect = 0.0;
    let mut max_aspect = 0.0;

    if flags & P_RESIZE_INC != 0 {
        inc_w = bounded_normal_hint_dimension(values[9]);
        inc_h = bounded_normal_hint_dimension(values[10]);
    }
    if flags & P_MAX_SIZE != 0 {
        max_w = bounded_normal_hint_dimension(values[7]);
        max_h = bounded_normal_hint_dimension(values[8]);
    }
    match (flags & P_BASE_SIZE != 0, flags & P_MIN_SIZE != 0) {
        (true, true) => {
            base_w = bounded_normal_hint_dimension(values[15]);
            base_h = bounded_normal_hint_dimension(values[16]);
            min_w = bounded_normal_hint_dimension(values[5]);
            min_h = bounded_normal_hint_dimension(values[6]);
        }
        (true, false) => {
            base_w = bounded_normal_hint_dimension(values[15]);
            base_h = bounded_normal_hint_dimension(values[16]);
            min_w = base_w;
            min_h = base_h;
        }
        (false, true) => {
            min_w = bounded_normal_hint_dimension(values[5]);
            min_h = bounded_normal_hint_dimension(values[6]);
            base_w = min_w;
            base_h = min_h;
        }
        (false, false) => {}
    }
    if flags & P_ASPECT != 0 {
        (min_aspect, max_aspect) =
            bounded_normal_hint_aspects(values[11], values[12], values[13], values[14]);
    }

    Some(NormalHints {
        base_w,
        base_h,
        inc_w,
        inc_h,
        max_w,
        max_h,
        min_w,
        min_h,
        min_aspect,
        max_aspect,
    })
}

pub fn parse_strut_partial(values: &[u32]) -> Option<StrutPartial> {
    if values.len() < 12 {
        return None;
    }
    Some(StrutPartial {
        left: values[0],
        right: values[1],
        top: values[2],
        bottom: values[3],
        left_start_y: values[4],
        left_end_y: values[5],
        right_start_y: values[6],
        right_end_y: values[7],
        top_start_x: values[8],
        top_end_x: values[9],
        bottom_start_x: values[10],
        bottom_end_x: values[11],
    })
}

pub fn parse_strut(values: &[u32]) -> Option<StrutPartial> {
    if values.len() < 4 {
        return None;
    }
    Some(StrutPartial {
        left: values[0],
        right: values[1],
        top: values[2],
        bottom: values[3],
        ..Default::default()
    })
}

pub fn parse_icon_data(values: &[u32]) -> Option<Vec<IconData>> {
    let mut icons = Vec::new();
    let mut i = 0usize;
    while i + 2 <= values.len() {
        let width = values[i];
        let height = values[i + 1];
        i += 2;

        if width == 0 || height == 0 {
            break;
        }

        let pixel_count = (width as usize).checked_mul(height as usize)?;
        let rgba_bytes = pixel_count.checked_mul(4)?;
        if i + pixel_count > values.len() {
            break;
        }

        let mut data = Vec::with_capacity(rgba_bytes);
        for argb in &values[i..i + pixel_count] {
            let [b, g, r, a] = argb.to_le_bytes();
            data.extend_from_slice(&[r, g, b, a]);
        }
        icons.push(IconData {
            width,
            height,
            data,
        });
        i += pixel_count;
    }

    if icons.is_empty() { None } else { Some(icons) }
}

pub fn parse_opaque_region(values: &[u32]) -> Option<Vec<(i32, i32, u32, u32)>> {
    if values.len() < 4 || values.len() % 4 != 0 {
        return None;
    }
    let regions = values
        .chunks_exact(4)
        .map(|c| (c[0] as i32, c[1] as i32, c[2], c[3]))
        .collect::<Vec<_>>();
    if regions.is_empty() {
        None
    } else {
        Some(regions)
    }
}

pub fn parse_motif_hints(values: &[u32]) -> Option<MotifWmHints> {
    if values.len() < 5 {
        return None;
    }
    Some(MotifWmHints {
        flags: values[0],
        functions: values[1],
        decorations: values[2],
        input_mode: values[3] as i32,
        status: values[4],
    })
}

pub fn parse_gtk_frame_extents(values: &[u32]) -> Option<[u32; 4]> {
    Some([
        *values.first()?,
        *values.get(1)?,
        *values.get(2)?,
        *values.get(3)?,
    ])
}

fn decode_x11_string(bytes: &[u8]) -> String {
    decode_utf8(bytes).unwrap_or_else(|| decode_latin1(bytes))
}

fn decode_utf8(bytes: &[u8]) -> Option<String> {
    String::from_utf8(bytes.to_vec()).ok()
}

fn decode_latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

#[cfg(test)]
mod tests {
    use super::{
        CONFIGURE_WINDOW_OPCODE, ClientMessageAtoms, ClientMessageKind, DEFAULT_OUTPUT_REFRESH_MHZ,
        ICCCM_ICONIC_STATE, NetWmStateAtoms, ProtocolErrorClass, SET_INPUT_FOCUS_OPCODE,
        SUPPORTED_EWMH_FEATURES, classify_client_message, current_desktop_request,
        decode_text_property, enrich_background_event, expand_net_wm_state_requests,
        forwards_property_notify, mode_refresh_hz, net_wm_state_from_atom, parse_icon_data,
        parse_normal_hints, parse_strut, parse_wm_class, primary_refresh, protocol_error_log_level,
        refresh_millihz_to_hz, unclassified_client_message_event, with_maximize_atoms,
    };
    use crate::backend::api::{
        BackendEvent, EwmhFeature, HitTarget, MaximizeAxes, NetWmAction, NetWmState, PropertyKind,
    };
    use crate::backend::common_define::{OutputId, WindowId};
    use std::cell::Cell;

    const MESSAGE_ATOMS: ClientMessageAtoms<u32> = ClientMessageAtoms {
        net_wm_state: 10,
        net_active_window: 11,
        net_close_window: 12,
        net_wm_moveresize: 13,
        wm_protocols: 14,
        net_wm_ping: 15,
        wm_change_state: 16,
    };

    #[test]
    fn wm_change_state_asking_for_iconic_is_a_minimise_request() {
        let kind = classify_client_message(
            MESSAGE_ATOMS.wm_change_state,
            32,
            [ICCCM_ICONIC_STATE, 0, 0, 0, 0],
            MESSAGE_ATOMS,
        );
        assert!(matches!(kind, ClientMessageKind::Iconify));
    }

    #[test]
    fn wm_change_state_asking_for_anything_else_is_not() {
        // NormalState is a restore request, which arrives as a map instead;
        // acting on it here would minimise a window that asked to come back.
        for data0 in [0u32, 1, 2, 4] {
            let kind = classify_client_message(
                MESSAGE_ATOMS.wm_change_state,
                32,
                [data0, 0, 0, 0, 0],
                MESSAGE_ATOMS,
            );
            assert!(matches!(kind, ClientMessageKind::Other), "data0={data0}");
        }
        // A byte-format message carrying the same number is not a 32-bit
        // WM_CHANGE_STATE and must not be read as one.
        let kind = classify_client_message(
            MESSAGE_ATOMS.wm_change_state,
            8,
            [ICCCM_ICONIC_STATE, 0, 0, 0, 0],
            MESSAGE_ATOMS,
        );
        assert!(matches!(kind, ClientMessageKind::Other));
    }

    /// `_NET_CURRENT_DESKTOP`, distinct from every atom in `MESSAGE_ATOMS`.
    const NET_CURRENT_DESKTOP: u32 = 40;

    /// The tag mask a `_NET_CURRENT_DESKTOP` request switches to, or `None`
    /// when it is not turned into a tag switch.
    fn desktop_switch(format: u8, index: u32, number_of_desktops: u32) -> Option<u32> {
        let event = current_desktop_request(
            NET_CURRENT_DESKTOP,
            format,
            [index, 0, 0, 0, 0],
            NET_CURRENT_DESKTOP,
            number_of_desktops,
        )?;
        match event {
            BackendEvent::WorkspaceActivate {
                monitor: None,
                tag_mask,
            } => Some(tag_mask),
            other => panic!("unexpected event {other:?}"),
        }
    }

    #[test]
    fn net_current_desktop_request_switches_to_that_single_tag() {
        // `wmctrl -s 2` / a pager click on the third desktop. JWM publishes
        // `_NET_CURRENT_DESKTOP` as `trailing_zeros(tagset)`, so the request
        // for desktop N is the single-tag mask `1 << N`.
        assert_eq!(desktop_switch(32, 0, 9), Some(0b1));
        assert_eq!(desktop_switch(32, 2, 9), Some(0b100));
        assert_eq!(desktop_switch(32, 8, 9), Some(1 << 8));
        assert_eq!(desktop_switch(32, 31, 32), Some(1 << 31));
    }

    #[test]
    fn net_current_desktop_request_past_the_last_desktop_is_dropped() {
        // The index is client-written: one past `_NET_NUMBER_OF_DESKTOPS`
        // must not become a mask policy reduces to an empty view.
        assert_eq!(desktop_switch(32, 9, 9), None);
        assert_eq!(desktop_switch(32, u32::MAX, 9), None);
        // Even with a count that would allow it, an index past the mask
        // width must not shift-overflow (panic in debug, wrap in release).
        assert_eq!(desktop_switch(32, 32, u32::MAX), None);
        assert_eq!(desktop_switch(32, 0, 0), None);
    }

    #[test]
    fn a_root_addressed_net_current_desktop_message_switches_tags_on_either_transport() {
        let root = WindowId::from_raw(1);
        let event = unclassified_client_message_event(
            root,
            true,
            NET_CURRENT_DESKTOP,
            32,
            [2, 0, 0, 0, 0],
            NET_CURRENT_DESKTOP,
            9,
        );
        assert!(
            matches!(
                event,
                BackendEvent::WorkspaceActivate {
                    monitor: None,
                    tag_mask: 0b100,
                }
            ),
            "{event:?}"
        );
        // Anything that is not a valid root-addressed desktop request stays
        // the generic message it always was: another type (a tray opcode),
        // an index past the last desktop, or a message not sent to the root.
        for (to_root, sent_type, index) in [
            (true, 41, 2),
            (true, NET_CURRENT_DESKTOP, 9),
            (false, NET_CURRENT_DESKTOP, 2),
        ] {
            let sent_data = [index, 0, 0, 0, 0];
            let event = unclassified_client_message_event(
                root,
                to_root,
                sent_type,
                32,
                sent_data,
                NET_CURRENT_DESKTOP,
                9,
            );
            assert!(
                matches!(
                    event,
                    BackendEvent::ClientMessage {
                        window,
                        type_,
                        data,
                        format: 32,
                    } if window == root && type_ == sent_type && data == sent_data
                ),
                "to_root={to_root} type_={sent_type} index={index}: {event:?}"
            );
        }
    }

    #[test]
    fn only_a_32_bit_net_current_desktop_message_is_a_desktop_switch() {
        assert_eq!(desktop_switch(8, 2, 9), None);
        assert_eq!(desktop_switch(16, 2, 9), None);
        // Any other message type (a tray opcode, a state change, ...) is left
        // to the generic `ClientMessage` fallback.
        for type_ in [MESSAGE_ATOMS.net_wm_state, MESSAGE_ATOMS.wm_protocols, 41] {
            assert!(
                current_desktop_request(type_, 32, [2, 0, 0, 0, 0], NET_CURRENT_DESKTOP, 9)
                    .is_none(),
                "type_={type_}"
            );
        }
    }

    #[test]
    fn net_current_desktop_reaches_the_client_message_fallback() {
        // The transports decode `_NET_CURRENT_DESKTOP` on the `Other` arm;
        // a classifier change that claimed it would bypass that decoder.
        let kind = classify_client_message(NET_CURRENT_DESKTOP, 32, [2, 0, 0, 0, 0], MESSAGE_ATOMS);
        assert!(matches!(kind, ClientMessageKind::Other));
    }

    #[test]
    fn supported_list_advertises_only_requests_jwm_acts_on() {
        // Pagers read `_NET_SUPPORTED` to decide which requests to send.
        // `_NET_CURRENT_DESKTOP` requests are decoded into a tag switch, so it
        // stays advertised; `_NET_RESTACK_WINDOW` has no policy path (stacking
        // is policy-owned), so advertising it would promise a no-op.
        assert!(SUPPORTED_EWMH_FEATURES.contains(&EwmhFeature::CurrentDesktop));
        assert!(!SUPPORTED_EWMH_FEATURES.contains(&EwmhFeature::RestackWindow));
    }

    const STATE_ATOMS: NetWmStateAtoms<u32> = NetWmStateAtoms {
        fullscreen: 1,
        maximized_vert: 2,
        maximized_horz: 3,
        hidden: 4,
        above: 5,
        below: 6,
        demands_attention: 7,
        sticky: 8,
        skip_taskbar: 9,
        skip_pager: 10,
    };
    const FULLSCREEN: u32 = STATE_ATOMS.fullscreen;
    const VERT: u32 = STATE_ATOMS.maximized_vert;
    const HORZ: u32 = STATE_ATOMS.maximized_horz;
    const ABOVE: u32 = STATE_ATOMS.above;

    /// `BackendEvent` has no `PartialEq`; the state-request events are
    /// projected onto this comparable shape so whole vectors can be pinned.
    #[derive(Debug, PartialEq)]
    enum Expanded {
        State(WindowId, NetWmAction, NetWmState),
        Maximize(WindowId, NetWmAction, MaximizeAxes),
    }

    fn expand(action: NetWmAction, first: u32, second: u32) -> Vec<Expanded> {
        expand_net_wm_state_requests(WindowId::from_raw(7), action, first, second, |atom| {
            net_wm_state_from_atom(atom, STATE_ATOMS)
        })
        .into_iter()
        .map(|event| match event {
            BackendEvent::WindowStateRequest {
                window,
                action,
                state,
            } => Expanded::State(window, action, state),
            BackendEvent::WindowMaximizeRequest {
                window,
                action,
                axes,
            } => Expanded::Maximize(window, action, axes),
            other => panic!("unexpected event: {other:?}"),
        })
        .collect()
    }

    #[test]
    fn property_deletions_reach_policy_only_for_kinds_that_act_on_absence() {
        for kind in [
            PropertyKind::Strut,
            PropertyKind::SizeHints,
            PropertyKind::TransientFor,
            PropertyKind::BypassCompositor,
            PropertyKind::RemoteCapture,
            // Regression: a client that deleted `_GTK_FRAME_EXTENTS` (CSD
            // turned off) or its decorations=0 `_MOTIF_WM_HINTS` kept
            // `no_decorations` and a zero border until it was remanaged.
            PropertyKind::MotifHints,
            PropertyKind::GtkFrameExtents,
        ] {
            assert!(forwards_property_notify(true, kind), "{kind:?}");
        }
        for kind in [
            PropertyKind::Title,
            PropertyKind::Class,
            PropertyKind::Other,
        ] {
            assert!(!forwards_property_notify(true, kind), "{kind:?}");
            assert!(forwards_property_notify(false, kind), "{kind:?}");
        }
    }

    #[test]
    fn window_races_log_at_debug_and_bad_requests_at_warn() {
        for (class, opcode) in [
            (ProtocolErrorClass::Window, 18),
            (ProtocolErrorClass::Drawable, CONFIGURE_WINDOW_OPCODE),
            (ProtocolErrorClass::Pixmap, 0),
            (ProtocolErrorClass::Damage, 0),
            (ProtocolErrorClass::Match, SET_INPUT_FOCUS_OPCODE),
            (ProtocolErrorClass::Match, CONFIGURE_WINDOW_OPCODE),
        ] {
            assert_eq!(
                protocol_error_log_level(class, opcode),
                log::Level::Debug,
                "{class:?} from request {opcode}"
            );
        }
        // BadMatch from ChangeProperty (18), or any other error, is JWM's bug.
        for (class, opcode) in [
            (ProtocolErrorClass::Match, 18),
            (ProtocolErrorClass::Other, SET_INPUT_FOCUS_OPCODE),
        ] {
            assert_eq!(
                protocol_error_log_level(class, opcode),
                log::Level::Warn,
                "{class:?} from request {opcode}"
            );
        }
    }

    #[test]
    fn classify_client_message_decodes_a_maximize_pair() {
        let kind = classify_client_message(
            MESSAGE_ATOMS.net_wm_state,
            32,
            [2, VERT, HORZ, 1, 0],
            MESSAGE_ATOMS,
        );
        assert!(matches!(
            kind,
            ClientMessageKind::WindowState {
                action: NetWmAction::Toggle,
                first: VERT,
                second: HORZ,
            }
        ));
    }

    #[test]
    fn paired_maximize_atoms_coalesce_into_one_request() {
        // Either atom order is one request for both axes: a Toggle must not
        // flip each axis in its own transaction.
        let win = WindowId::from_raw(7);
        for (first, second) in [(VERT, HORZ), (HORZ, VERT)] {
            assert_eq!(
                expand(NetWmAction::Toggle, first, second),
                vec![Expanded::Maximize(
                    win,
                    NetWmAction::Toggle,
                    MaximizeAxes::BOTH
                )],
                "first={first} second={second}"
            );
        }
    }

    #[test]
    fn single_maximize_atom_and_other_states_keep_message_order() {
        let win = WindowId::from_raw(7);
        for action in [NetWmAction::Add, NetWmAction::Remove, NetWmAction::Toggle] {
            assert_eq!(
                expand(action, FULLSCREEN, HORZ),
                vec![
                    Expanded::State(win, action, NetWmState::Fullscreen),
                    Expanded::Maximize(win, action, MaximizeAxes::HORZ),
                ]
            );
            assert_eq!(
                expand(action, VERT, ABOVE),
                vec![
                    Expanded::Maximize(win, action, MaximizeAxes::VERT),
                    Expanded::State(win, action, NetWmState::Above),
                ]
            );
        }
    }

    #[test]
    fn zero_unknown_and_repeated_maximize_atoms_are_ignored() {
        let win = WindowId::from_raw(7);
        let vert_only = vec![Expanded::Maximize(
            win,
            NetWmAction::Add,
            MaximizeAxes::VERT,
        )];
        assert_eq!(expand(NetWmAction::Add, VERT, 0), vert_only);
        assert_eq!(expand(NetWmAction::Add, VERT, VERT), vert_only);
        // Nothing decodable leaves the transports' ClientMessage fallback.
        assert_eq!(expand(NetWmAction::Add, 0, 999), vec![]);
    }

    #[test]
    fn with_maximize_atoms_rewrites_only_the_two_axes() {
        const A: u32 = 100;
        const B: u32 = 101;
        assert_eq!(
            with_maximize_atoms(&[A, VERT, B], VERT, HORZ, MaximizeAxes::HORZ),
            vec![A, B, HORZ]
        );
        assert_eq!(
            with_maximize_atoms(&[A], VERT, HORZ, MaximizeAxes::BOTH),
            vec![A, VERT, HORZ]
        );
        // Already right: the order is left alone, so callers can skip the write.
        assert_eq!(
            with_maximize_atoms(&[A, HORZ, VERT], VERT, HORZ, MaximizeAxes::BOTH),
            vec![A, HORZ, VERT]
        );
        // Duplicate maximize atoms collapse even when membership is right.
        assert_eq!(
            with_maximize_atoms(&[VERT, A, VERT], VERT, HORZ, MaximizeAxes::VERT),
            vec![A, VERT]
        );
        assert_eq!(
            with_maximize_atoms(&[A], VERT, HORZ, MaximizeAxes::NONE),
            vec![A]
        );
    }

    #[test]
    fn background_pointer_events_get_their_output_filled() {
        let mut event = BackendEvent::ButtonPress {
            target: HitTarget::Background { output: None },
            state: 0,
            detail: 1,
            time: 0,
            root_x: 640.0,
            root_y: 400.0,
        };
        let looked_up = Cell::new(None);
        enrich_background_event(
            &mut event,
            |x, y| {
                looked_up.set(Some((x, y)));
                Some(OutputId(2))
            },
            || panic!("a pointer event must not invalidate the output cache"),
        );
        // The lookup receives the truncated integer root coordinates...
        assert_eq!(looked_up.get(), Some((640, 400)));
        // ...and its result is written back into the hit target.
        match event {
            BackendEvent::ButtonPress {
                target: HitTarget::Background { output },
                ..
            } => assert_eq!(output, Some(OutputId(2))),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn a_pointer_event_already_on_a_surface_is_left_untouched() {
        let mut event = BackendEvent::MotionNotify {
            target: HitTarget::Surface(crate::backend::common_define::WindowId::from_raw(7)),
            root_x: 1.0,
            root_y: 2.0,
            time: 0,
        };
        enrich_background_event(
            &mut event,
            |_, _| panic!("a resolved surface must not be looked up"),
            || panic!("a pointer event must not invalidate the output cache"),
        );
        assert!(matches!(
            event,
            BackendEvent::MotionNotify {
                target: HitTarget::Surface(_),
                ..
            }
        ));
    }

    #[test]
    fn screen_layout_change_invalidates_the_output_cache() {
        let mut event = BackendEvent::ScreenLayoutChanged;
        let invalidated = Cell::new(false);
        enrich_background_event(
            &mut event,
            |_, _| panic!("a layout change must not look up an output"),
            || invalidated.set(true),
        );
        assert!(invalidated.get());
    }

    #[test]
    fn refresh_units_are_rounded_for_compositor_policy() {
        assert_eq!(refresh_millihz_to_hz(0), 0);
        assert_eq!(refresh_millihz_to_hz(59_940), 60);
        assert_eq!(refresh_millihz_to_hz(120_081), 120);
        assert_eq!(mode_refresh_hz(497_500_000, 2720, 1525), 120);
        assert_eq!(mode_refresh_hz(0, 0, 0), 60);
    }

    #[test]
    fn primary_refresh_picks_the_first_output_with_a_positive_rate() {
        let outputs = [
            output_with_rate(0),
            output_with_rate(120_081),
            output_with_rate(60_000),
        ];
        let refresh = primary_refresh(&outputs);
        assert_eq!(refresh.millihz, 120_081);
        assert_eq!(refresh.hz, 120);
    }

    #[test]
    fn primary_refresh_defaults_to_60hz_without_a_reporting_output() {
        for outputs in [&[][..], &[output_with_rate(0)][..]] {
            let refresh = primary_refresh(outputs);
            assert_eq!(refresh.millihz, DEFAULT_OUTPUT_REFRESH_MHZ);
            assert_eq!(refresh.hz, 60);
        }
    }

    fn output_with_rate(refresh_millihz: u32) -> crate::backend::api::OutputInfo {
        super::build_output_info(
            OutputId(1),
            "test".to_string(),
            0,
            0,
            1920,
            1080,
            refresh_millihz,
            false,
            None,
        )
    }

    #[test]
    fn parses_wm_class_to_lowercase_parts() {
        assert_eq!(
            parse_wm_class(b"XTerm\0UXTerm\0"),
            ("xterm".to_string(), "uxterm".to_string())
        );
    }

    #[test]
    fn normal_hints_fill_base_and_min_defaults() {
        let mut values = vec![0; 18];
        values[0] = 1 << 8;
        values[15] = 640;
        values[16] = 480;
        let hints = parse_normal_hints(&values).expect("normal hints");
        assert_eq!(hints.base_w, 640);
        assert_eq!(hints.base_h, 480);
        assert_eq!(hints.min_w, 640);
        assert_eq!(hints.min_h, 480);
    }

    #[test]
    fn normal_hints_treat_sizes_the_server_cannot_configure_as_absent() {
        // PMinSize | PMaxSize | PResizeInc | PBaseSize with every word past
        // what a ConfigureWindow can carry: `min == max > 0` would otherwise
        // float the client as fixed-size at i32::MAX and every later
        // `w + 2 * border_w` would overflow.
        let mut values = vec![0u32; 18];
        values[0] = (1 << 4) | (1 << 5) | (1 << 6) | (1 << 8);
        values[5] = 0x7fff_ffff;
        values[6] = 0x7fff_ffff;
        values[7] = 0x7fff_ffff;
        values[8] = 0x7fff_ffff;
        values[9] = 0xffff_ffff;
        values[10] = 0xffff_ffff;
        values[15] = 0x8000_0000;
        values[16] = 0x8000_0000;
        let hints = parse_normal_hints(&values).expect("normal hints");
        assert_eq!((hints.min_w, hints.min_h), (0, 0));
        assert_eq!((hints.max_w, hints.max_h), (0, 0));
        assert_eq!((hints.inc_w, hints.inc_h), (0, 0));
        assert_eq!((hints.base_w, hints.base_h), (0, 0));

        // The CARD16 ceiling itself is still a hint.
        let mut values = vec![0u32; 18];
        values[0] = (1 << 4) | (1 << 5);
        values[5] = 65535;
        values[6] = 1;
        values[7] = 65535;
        values[8] = 1;
        let hints = parse_normal_hints(&values).expect("normal hints");
        assert_eq!((hints.min_w, hints.max_w), (65535, 65535));

        // A rejected base word does not leak into the min fallback either.
        let mut values = vec![0u32; 18];
        values[0] = 1 << 8;
        values[15] = 70_000;
        values[16] = 480;
        let hints = parse_normal_hints(&values).expect("normal hints");
        assert_eq!((hints.min_w, hints.min_h), (0, 480));
    }

    #[test]
    fn normal_hints_drop_aspect_pairs_outside_the_sane_band() {
        let with_aspect = |min: (u32, u32), max: (u32, u32)| {
            let mut values = vec![0u32; 18];
            values[0] = 1 << 7;
            values[11] = min.0;
            values[12] = min.1;
            values[13] = max.0;
            values[14] = max.1;
            let hints = parse_normal_hints(&values).expect("normal hints");
            (hints.min_aspect, hints.max_aspect)
        };
        // `0xFFFF_FFFF : 1` saturates `h * aspect` to i32::MAX downstream.
        assert_eq!(with_aspect((0xffff_ffff, 1), (16, 9)), (0.0, 0.0));
        assert_eq!(with_aspect((4, 3), (1, 0xffff_ffff)), (0.0, 0.0));
        // A zero anywhere disables the pair exactly as it always did.
        assert_eq!(with_aspect((0, 3), (16, 9)), (0.0, 0.0));
        assert_eq!(with_aspect((4, 3), (16, 0)), (0.0, 0.0));
        // Ordinary ratios survive untouched.
        let (min_aspect, max_aspect) = with_aspect((4, 3), (16, 9));
        assert!((min_aspect - 4.0 / 3.0).abs() < 1e-6);
        assert!((max_aspect - 16.0 / 9.0).abs() < 1e-6);
    }

    #[test]
    fn a_half_aspect_pair_is_recorded_absent() {
        // "Never narrower than 4:3, no practical maximum" is the case that
        // looks like it loses something: the maximum is far outside the band
        // the parser will carry, so the pair is recorded absent. That this
        // costs a window nothing is the consumer's half of the contract,
        // pinned by `a_half_aspect_pair_constrains_nothing` beside the gate —
        // the transport may not reach into policy to assert it here.
        let mut values = vec![0u32; 18];
        values[0] = 1 << 7;
        values[11] = 4;
        values[12] = 3;
        values[13] = 1_000_000;
        values[14] = 1;
        let hints = parse_normal_hints(&values).expect("normal hints");
        assert_eq!((hints.min_aspect, hints.max_aspect), (0.0, 0.0));
    }

    #[test]
    fn parses_strut_fallback() {
        let strut = parse_strut(&[1, 2, 3, 4]).expect("strut");
        assert_eq!(strut.left, 1);
        assert_eq!(strut.top, 3);
        assert_eq!(strut.bottom_end_x, 0);
    }

    #[test]
    fn parses_argb_icon_into_rgba() {
        let icons = parse_icon_data(&[1, 1, 0x11223344]).expect("icon");
        assert_eq!(icons.len(), 1);
        assert_eq!(icons[0].data, vec![0x22, 0x33, 0x44, 0x11]);
    }

    #[test]
    fn text_property_trims_trailing_nul_and_falls_back_to_latin1() {
        assert_eq!(
            decode_text_property(b"title\0", 1, 1, 2).as_deref(),
            Some("title")
        );
        assert_eq!(decode_text_property(&[0xff], 3, 1, 2).as_deref(), Some("ÿ"));
    }
}
