use crate::backend::api::OutputInfo;
use crate::backend::api::{
    AllowedAction, BackendEvent, EwmhFeature, IconData, MotifWmHints, NetWmAction, NetWmState,
    NormalHints, PropertyKind, StackMode, StrutPartial, WindowChanges, WindowType, WmHints,
};
use crate::backend::common_define::{OutputId, WindowId};
use std::ops::BitOr;

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
}

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
    Other,
}

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
    EwmhFeature::RestackWindow,
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
    ClientMessageKind::Other
}

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
    let mut events = Vec::new();
    for atom in [first, second] {
        if atom == 0 {
            continue;
        }
        if let Some(state) = decode_state(atom) {
            events.push(BackendEvent::WindowStateRequest {
                window,
                action,
                state,
            });
        }
    }
    events
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
        inc_w = values[9] as i32;
        inc_h = values[10] as i32;
    }
    if flags & P_MAX_SIZE != 0 {
        max_w = values[7] as i32;
        max_h = values[8] as i32;
    }
    match (flags & P_BASE_SIZE != 0, flags & P_MIN_SIZE != 0) {
        (true, true) => {
            base_w = values[15] as i32;
            base_h = values[16] as i32;
            min_w = values[5] as i32;
            min_h = values[6] as i32;
        }
        (true, false) => {
            base_w = values[15] as i32;
            base_h = values[16] as i32;
            min_w = base_w;
            min_h = base_h;
        }
        (false, true) => {
            min_w = values[5] as i32;
            min_h = values[6] as i32;
            base_w = min_w;
            base_h = min_h;
        }
        (false, false) => {}
    }
    if flags & P_ASPECT != 0 && values[12] != 0 && values[14] != 0 {
        min_aspect = values[11] as f32 / values[12] as f32;
        max_aspect = values[13] as f32 / values[14] as f32;
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
        decode_text_property, parse_icon_data, parse_normal_hints, parse_strut, parse_wm_class,
    };

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
