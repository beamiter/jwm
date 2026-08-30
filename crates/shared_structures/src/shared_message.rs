use std::borrow::Cow;
use std::time::{SystemTime, UNIX_EPOCH};

// 常量定义
pub const MAX_CLIENT_NAME_LEN: usize = 128;
pub const MAX_LT_SYMBOL_LEN: usize = 32;
pub const MAX_TAGS: usize = 9;
/// Maximum number of minimized windows carried by one monitor snapshot.
pub const MAX_MINIMIZED_WINDOWS: usize = 16;
/// Fixed wire storage for a minimized window title, including its NUL terminator.
pub const MAX_WINDOW_TITLE_LEN: usize = 96;
/// Fixed wire storage for a Wayland app-id or X11 class, including its NUL terminator.
pub const MAX_APP_ID_LEN: usize = 64;

/// `MinimizedWindowInfo::flags`: the compositor can present a cached preview.
pub const MINIMIZED_WINDOW_FLAG_PREVIEW_AVAILABLE: u32 = 1 << 0;
/// `MinimizedWindowInfo::flags`: the minimized window currently demands attention.
pub const MINIMIZED_WINDOW_FLAG_URGENT: u32 = 1 << 1;
/// `SharedMessage::minimized_flags`: more entries exist than fit in the snapshot.
pub const MINIMIZED_LIST_FLAG_OVERFLOW: u32 = 1 << 0;
/// `SharedCommand::flags`: show (rather than dismiss) a minimized preview.
pub const PREVIEW_MINIMIZED_FLAG_VISIBLE: u32 = 1 << 0;
/// `SharedCommand::flags`: refresh an existing preview lease without taking ownership.
pub const PREVIEW_MINIMIZED_FLAG_RENEWAL: u32 = 1 << 1;

#[inline]
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn copy_utf8_truncated(destination: &mut [u8], value: &str) {
    // These arrays are NUL-terminated wire strings. Do not retain an
    // invisible suffix after an embedded terminator: it would make equal
    // displayed values have different wire bytes and checksums.
    let content_len = value
        .as_bytes()
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(value.len());
    let mut len = content_len.min(destination.len().saturating_sub(1));
    while !value.is_char_boundary(len) {
        len -= 1;
    }
    destination.fill(0);
    destination[..len].copy_from_slice(&value.as_bytes()[..len]);
}

fn bytes_lossy(bytes: &[u8]) -> Cow<'_, str> {
    let null_pos = bytes
        .iter()
        .position(|&value| value == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..null_pos])
}

// 使用合理对齐
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "rkyv",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct TagStatus {
    pub is_selected: bool,
    pub is_urg: bool,
    pub is_filled: bool,
    pub is_occ: bool,
}

impl TagStatus {
    #[must_use]
    pub fn new(is_selected: bool, is_urg: bool, is_filled: bool, is_occ: bool) -> Self {
        Self {
            is_selected,
            is_urg,
            is_filled,
            is_occ,
        }
    }
}

// 使用更合理的对齐策略
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "rkyv",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct MonitorInfo {
    pub monitor_num: i32,
    pub monitor_width: i32,
    pub monitor_height: i32,
    pub monitor_x: i32,
    pub monitor_y: i32,
    pub tag_status_vec: [TagStatus; MAX_TAGS],
    #[cfg_attr(feature = "serde", serde(with = "serde_big_array::BigArray"))]
    pub client_name: [u8; MAX_CLIENT_NAME_LEN],
    /// Wayland app-id or X11 class of the focused client, for a bar that wants
    /// to show its desktop icon beside the title. Empty when nothing is
    /// focused, or when the window carries no identity of its own.
    #[cfg_attr(feature = "serde", serde(with = "serde_big_array::BigArray"))]
    pub client_app_id: [u8; MAX_APP_ID_LEN],
    pub ltsymbol: [u8; MAX_LT_SYMBOL_LEN],
    /// How many layouts this window manager can be put into.
    ///
    /// A bar sizes its layout menu from this rather than from its own compiled
    /// catalog, so a compositor that gained a layout is offered in full and one
    /// that dropped one is not offered a dead entry. Zero means the window
    /// manager did not say, and the bar falls back to its own catalog.
    pub layout_count: u32,
    /// Protocol identifier of the layout currently in use, so a bar can mark
    /// it in that menu without matching symbols back to identifiers.
    pub layout_id: u32,
}

impl Default for MonitorInfo {
    fn default() -> Self {
        Self {
            client_name: [0; MAX_CLIENT_NAME_LEN],
            client_app_id: [0; MAX_APP_ID_LEN],
            tag_status_vec: [TagStatus::default(); MAX_TAGS],
            monitor_num: 0,
            monitor_width: 0,
            monitor_height: 0,
            monitor_x: 0,
            monitor_y: 0,
            ltsymbol: [0; MAX_LT_SYMBOL_LEN],
            layout_count: 0,
            layout_id: 0,
        }
    }
}

impl MonitorInfo {
    pub fn set_client_name(&mut self, name: &str) {
        copy_utf8_truncated(&mut self.client_name, name);
    }

    #[must_use]
    pub fn get_client_name(&self) -> String {
        self.client_name_lossy().into_owned()
    }

    /// 以零拷贝形式返回 client name；外部直接写入了非 UTF-8 字节时
    /// 才会分配并做损坏替换。
    #[must_use]
    pub fn client_name_lossy(&self) -> Cow<'_, str> {
        bytes_lossy(&self.client_name)
    }

    pub fn set_client_app_id(&mut self, app_id: &str) {
        copy_utf8_truncated(&mut self.client_app_id, app_id);
    }

    #[must_use]
    pub fn get_client_app_id(&self) -> String {
        self.client_app_id_lossy().into_owned()
    }

    /// 以零拷贝形式返回焦点窗口的 app-id，非 UTF-8 时做损坏替换。
    #[must_use]
    pub fn client_app_id_lossy(&self) -> Cow<'_, str> {
        bytes_lossy(&self.client_app_id)
    }

    pub fn set_ltsymbol(&mut self, symbol: &str) {
        copy_utf8_truncated(&mut self.ltsymbol, symbol);
    }

    #[must_use]
    pub fn get_ltsymbol(&self) -> String {
        self.ltsymbol_lossy().into_owned()
    }

    /// 以零拷贝形式返回 layout symbol，非 UTF-8 时做损坏替换。
    #[must_use]
    pub fn ltsymbol_lossy(&self) -> Cow<'_, str> {
        bytes_lossy(&self.ltsymbol)
    }

    pub fn set_tag_status(&mut self, index: usize, status: TagStatus) {
        let _ = self.try_set_tag_status(index, status);
    }

    /// 设置 tag 状态，越界时返回 `false`。
    pub fn try_set_tag_status(&mut self, index: usize, status: TagStatus) -> bool {
        let Some(slot) = self.tag_status_vec.get_mut(index) else {
            return false;
        };
        *slot = status;
        true
    }

    #[must_use]
    pub fn get_tag_status(&self, index: usize) -> Option<TagStatus> {
        self.tag_status_vec.get(index).copied()
    }
}

/// One minimized window exposed to a status bar.
///
/// `window_id` is stable only within the `wm_session_id` carried by the
/// surrounding [`SharedMessage`]. `app_id` contains the Wayland app-id or the
/// X11 class, depending on the backend.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "rkyv",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct MinimizedWindowInfo {
    pub window_id: u64,
    pub monitor_id: i32,
    pub flags: u32,
    #[cfg_attr(feature = "serde", serde(with = "serde_big_array::BigArray"))]
    pub title: [u8; MAX_WINDOW_TITLE_LEN],
    #[cfg_attr(feature = "serde", serde(with = "serde_big_array::BigArray"))]
    pub app_id: [u8; MAX_APP_ID_LEN],
}

impl Default for MinimizedWindowInfo {
    fn default() -> Self {
        Self {
            window_id: 0,
            monitor_id: 0,
            flags: 0,
            title: [0; MAX_WINDOW_TITLE_LEN],
            app_id: [0; MAX_APP_ID_LEN],
        }
    }
}

impl MinimizedWindowInfo {
    #[must_use]
    pub fn new(window_id: u64, monitor_id: i32, flags: u32) -> Self {
        Self {
            window_id,
            monitor_id,
            flags,
            ..Self::default()
        }
    }

    pub fn set_title(&mut self, title: &str) {
        copy_utf8_truncated(&mut self.title, title);
    }

    #[must_use]
    pub fn get_title(&self) -> String {
        self.title_lossy().into_owned()
    }

    #[must_use]
    pub fn title_lossy(&self) -> Cow<'_, str> {
        bytes_lossy(&self.title)
    }

    pub fn set_app_id(&mut self, app_id: &str) {
        copy_utf8_truncated(&mut self.app_id, app_id);
    }

    #[must_use]
    pub fn get_app_id(&self) -> String {
        self.app_id_lossy().into_owned()
    }

    #[must_use]
    pub fn app_id_lossy(&self) -> Cow<'_, str> {
        bytes_lossy(&self.app_id)
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "rkyv",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct SharedMessage {
    pub timestamp: u64,
    /// Identifies one window-manager lifetime; window ids are scoped to it.
    pub wm_session_id: u64,
    /// Monotonic revision of the minimized-window projection.
    pub minimized_generation: u64,
    pub monitor_info: MonitorInfo,
    pub minimized_count: u32,
    pub minimized_flags: u32,
    pub minimized_windows: [MinimizedWindowInfo; MAX_MINIMIZED_WINDOWS],
}

/// `Default` 是廉价、可复现的零值（`timestamp == 0`），不含时钟副作用；
/// 需要打时间戳时使用 [`SharedMessage::new`] 或
/// [`SharedMessage::with_monitor_info`]。
impl Default for SharedMessage {
    fn default() -> Self {
        Self {
            timestamp: 0,
            wm_session_id: 0,
            minimized_generation: 0,
            monitor_info: MonitorInfo::default(),
            minimized_count: 0,
            minimized_flags: 0,
            minimized_windows: [MinimizedWindowInfo::default(); MAX_MINIMIZED_WINDOWS],
        }
    }
}

impl SharedMessage {
    /// 创建一条带当前时间戳的空消息。
    #[must_use]
    pub fn new() -> Self {
        Self {
            timestamp: now_millis(),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with_monitor_info(monitor_info: MonitorInfo) -> Self {
        Self {
            timestamp: now_millis(),
            monitor_info,
            ..Self::default()
        }
    }

    pub fn update_timestamp(&mut self) {
        self.timestamp = now_millis();
    }

    #[must_use]
    pub fn get_timestamp(&self) -> u64 {
        self.timestamp
    }

    #[must_use]
    pub fn get_monitor_info(&self) -> &MonitorInfo {
        &self.monitor_info
    }

    pub fn get_monitor_info_mut(&mut self) -> &mut MonitorInfo {
        &mut self.monitor_info
    }

    /// Replace the minimized-window projection and return the number stored.
    /// Extra entries are truncated and reflected in `MINIMIZED_LIST_FLAG_OVERFLOW`.
    pub fn set_minimized_windows(&mut self, windows: &[MinimizedWindowInfo]) -> usize {
        let count = windows.len().min(MAX_MINIMIZED_WINDOWS);
        self.minimized_windows.fill(MinimizedWindowInfo::default());
        self.minimized_windows[..count].copy_from_slice(&windows[..count]);
        self.minimized_count = count as u32;
        if windows.len() > MAX_MINIMIZED_WINDOWS {
            self.minimized_flags |= MINIMIZED_LIST_FLAG_OVERFLOW;
        } else {
            self.minimized_flags &= !MINIMIZED_LIST_FLAG_OVERFLOW;
        }
        count
    }

    /// Return only validated initialized entries; corrupt oversized counts are clamped.
    #[must_use]
    pub fn minimized_windows(&self) -> &[MinimizedWindowInfo] {
        let count = usize::try_from(self.minimized_count)
            .unwrap_or(usize::MAX)
            .min(MAX_MINIMIZED_WINDOWS);
        &self.minimized_windows[..count]
    }
}

// 命令相关定义
#[repr(u32)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "rkyv",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum CommandType {
    #[default]
    None = 0,
    ViewTag = 1,
    ToggleTag = 2,
    SetLayout = 3,
    /// 状态栏请求窗口管理器打开自带的 Shell Hub。`parameter` 携带路由编号，
    /// 由 `ShellHubRoute` 定义；未知路由退化为 Hub 首页而不是被丢弃。
    ShellHub = 4,
    /// Restore one minimized window identified by `SharedCommand::window_id`.
    RestoreMinimized = 5,
    /// Show or dismiss a compositor-owned preview for a minimized window.
    PreviewMinimized = 6,
    /// Publish a minimized-window shelf or item geometry.
    SetMinimizedGeometry = 7,
}

impl From<u32> for CommandType {
    fn from(value: u32) -> Self {
        match value {
            1 => CommandType::ViewTag,
            2 => CommandType::ToggleTag,
            3 => CommandType::SetLayout,
            4 => CommandType::ShellHub,
            5 => CommandType::RestoreMinimized,
            6 => CommandType::PreviewMinimized,
            7 => CommandType::SetMinimizedGeometry,
            _ => CommandType::None,
        }
    }
}

impl CommandType {
    /// 严格解析原始命令值，未知值不会与 `None` 混淆。
    #[must_use]
    pub const fn from_raw(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::None),
            1 => Some(Self::ViewTag),
            2 => Some(Self::ToggleTag),
            3 => Some(Self::SetLayout),
            4 => Some(Self::ShellHub),
            5 => Some(Self::RestoreMinimized),
            6 => Some(Self::PreviewMinimized),
            7 => Some(Self::SetMinimizedGeometry),
            _ => None,
        }
    }
}

/// Shell Hub 的路由编号，与 `CommandType::ShellHub` 的 `parameter` 一一对应。
///
/// 这是一个纯协议枚举：窗口管理器把它映射到自己的界面状态，状态栏用它标注
/// 按钮，两端都不需要知道对方的实现。
#[repr(u32)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "rkyv",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub enum ShellHubRoute {
    /// Hub 首页：路由列表加快捷开关。
    #[default]
    Hub = 0,
    Applications = 1,
    Notifications = 2,
    Clipboard = 3,
    Calendar = 4,
    Wallpaper = 5,
}

impl ShellHubRoute {
    pub const ALL: [Self; 6] = [
        Self::Hub,
        Self::Applications,
        Self::Notifications,
        Self::Clipboard,
        Self::Calendar,
        Self::Wallpaper,
    ];

    /// 严格解析路由编号。未知编号返回 `None`，调用方决定是丢弃还是退化。
    #[must_use]
    pub const fn from_raw(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Hub),
            1 => Some(Self::Applications),
            2 => Some(Self::Notifications),
            3 => Some(Self::Clipboard),
            4 => Some(Self::Calendar),
            5 => Some(Self::Wallpaper),
            _ => None,
        }
    }

    /// 宽松解析：未知编号退化为 Hub 首页，这样新版状态栏配上旧版窗口管理器
    /// 时仍然能打开一个有用的界面。
    #[must_use]
    pub const fn from_raw_or_hub(value: u32) -> Self {
        match Self::from_raw(value) {
            Some(route) => route,
            None => Self::Hub,
        }
    }

    #[must_use]
    pub const fn as_raw(self) -> u32 {
        self as u32
    }
}

impl From<u32> for ShellHubRoute {
    fn from(value: u32) -> Self {
        Self::from_raw_or_hub(value)
    }
}

impl From<ShellHubRoute> for u32 {
    fn from(route: ShellHubRoute) -> Self {
        route.as_raw()
    }
}

impl From<CommandType> for u32 {
    fn from(cmd_type: CommandType) -> Self {
        cmd_type as u32
    }
}

/// A rectangle in global physical pixels used to anchor minimize and preview effects.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "rkyv",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct MinimizedWindowAnchor {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl MinimizedWindowAnchor {
    #[must_use]
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "rkyv",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
pub struct SharedCommand {
    pub cmd_type: u32,
    pub parameter: u32,
    pub monitor_id: i32,
    pub flags: u32,
    pub window_id: u64,
    pub wm_session_id: u64,
    /// Minimized projection generation observed by the command producer.
    pub minimized_generation: u64,
    pub anchor_x: i32,
    pub anchor_y: i32,
    pub anchor_w: u32,
    pub anchor_h: u32,
    pub timestamp: u64,
}

impl SharedCommand {
    #[must_use]
    pub fn new(cmd_type: CommandType, parameter: u32, monitor_id: i32) -> Self {
        Self {
            cmd_type: cmd_type.into(),
            parameter,
            monitor_id,
            flags: 0,
            window_id: 0,
            wm_session_id: 0,
            minimized_generation: 0,
            anchor_x: 0,
            anchor_y: 0,
            anchor_w: 0,
            anchor_h: 0,
            timestamp: now_millis(),
        }
    }

    #[must_use]
    pub fn get_command_type(&self) -> CommandType {
        self.cmd_type.into()
    }

    /// 严格返回命令类型；原始值未知时返回 `None`。
    #[must_use]
    pub const fn command_type_checked(&self) -> Option<CommandType> {
        CommandType::from_raw(self.cmd_type)
    }

    #[must_use]
    pub fn view_tag(tag_bit: u32, monitor_id: i32) -> Self {
        Self::new(CommandType::ViewTag, tag_bit, monitor_id)
    }

    #[must_use]
    pub fn toggle_tag(tag_bit: u32, monitor_id: i32) -> Self {
        Self::new(CommandType::ToggleTag, tag_bit, monitor_id)
    }

    #[must_use]
    pub fn set_layout(layout_idx: u32, monitor_id: i32) -> Self {
        Self::new(CommandType::SetLayout, layout_idx, monitor_id)
    }

    #[must_use]
    pub fn shell_hub(route: ShellHubRoute, monitor_id: i32) -> Self {
        Self::new(CommandType::ShellHub, route.as_raw(), monitor_id)
    }

    #[must_use]
    pub fn restore_minimized(
        window_id: u64,
        wm_session_id: u64,
        minimized_generation: u64,
        monitor_id: i32,
        anchor: MinimizedWindowAnchor,
    ) -> Self {
        Self {
            window_id,
            wm_session_id,
            minimized_generation,
            anchor_x: anchor.x,
            anchor_y: anchor.y,
            anchor_w: anchor.width,
            anchor_h: anchor.height,
            ..Self::new(CommandType::RestoreMinimized, 0, monitor_id)
        }
    }

    #[must_use]
    pub fn preview_minimized(
        window_id: u64,
        wm_session_id: u64,
        minimized_generation: u64,
        monitor_id: i32,
        flags: u32,
        anchor: MinimizedWindowAnchor,
    ) -> Self {
        Self {
            flags,
            window_id,
            wm_session_id,
            minimized_generation,
            anchor_x: anchor.x,
            anchor_y: anchor.y,
            anchor_w: anchor.width,
            anchor_h: anchor.height,
            ..Self::new(CommandType::PreviewMinimized, 0, monitor_id)
        }
    }

    #[must_use]
    pub fn set_minimized_geometry(
        window_id: u64,
        wm_session_id: u64,
        minimized_generation: u64,
        monitor_id: i32,
        anchor: MinimizedWindowAnchor,
    ) -> Self {
        Self {
            window_id,
            wm_session_id,
            minimized_generation,
            anchor_x: anchor.x,
            anchor_y: anchor.y,
            anchor_w: anchor.width,
            anchor_h: anchor.height,
            ..Self::new(CommandType::SetMinimizedGeometry, 0, monitor_id)
        }
    }

    /// Return a minimized-window target only for commands that name one.
    #[must_use]
    pub const fn minimized_window_id(&self) -> Option<u64> {
        match CommandType::from_raw(self.cmd_type) {
            Some(CommandType::RestoreMinimized | CommandType::PreviewMinimized) => {
                Some(self.window_id)
            }
            Some(CommandType::SetMinimizedGeometry) if self.window_id != 0 => Some(self.window_id),
            _ => None,
        }
    }

    #[must_use]
    pub const fn anchor(&self) -> MinimizedWindowAnchor {
        MinimizedWindowAnchor::new(self.anchor_x, self.anchor_y, self.anchor_w, self.anchor_h)
    }

    #[must_use]
    pub const fn get_flags(&self) -> u32 {
        self.flags
    }

    #[must_use]
    pub const fn get_window_id(&self) -> u64 {
        self.window_id
    }

    #[must_use]
    pub const fn get_wm_session_id(&self) -> u64 {
        self.wm_session_id
    }

    #[must_use]
    pub const fn get_minimized_generation(&self) -> u64 {
        self.minimized_generation
    }

    /// 命令是 `ShellHub` 时返回它的路由，其它命令返回 `None`。
    #[must_use]
    pub const fn shell_hub_route(&self) -> Option<ShellHubRoute> {
        match CommandType::from_raw(self.cmd_type) {
            Some(CommandType::ShellHub) => Some(ShellHubRoute::from_raw_or_hub(self.parameter)),
            _ => None,
        }
    }

    #[must_use]
    pub fn get_parameter(&self) -> u32 {
        self.parameter
    }

    #[must_use]
    pub fn get_monitor_id(&self) -> i32 {
        self.monitor_id
    }

    #[must_use]
    pub fn get_timestamp(&self) -> u64 {
        self.timestamp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── TagStatus ────────────────────────────────────────────────────────────

    #[test]
    fn test_tag_status_default_all_false() {
        let s = TagStatus::default();
        assert!(!s.is_selected);
        assert!(!s.is_urg);
        assert!(!s.is_filled);
        assert!(!s.is_occ);
    }

    #[test]
    fn test_tag_status_all_bool_combinations() {
        for bits in 0u8..16 {
            let sel = bits & 1 != 0;
            let urg = bits & 2 != 0;
            let fil = bits & 4 != 0;
            let occ = bits & 8 != 0;
            let s = TagStatus::new(sel, urg, fil, occ);
            assert_eq!(s.is_selected, sel);
            assert_eq!(s.is_urg, urg);
            assert_eq!(s.is_filled, fil);
            assert_eq!(s.is_occ, occ);
        }
    }

    // ── MonitorInfo ──────────────────────────────────────────────────────────

    #[test]
    fn test_monitor_info_default_values() {
        let mi = MonitorInfo::default();
        assert_eq!(mi.monitor_num, 0);
        assert_eq!(mi.monitor_width, 0);
        assert_eq!(mi.monitor_height, 0);
        assert_eq!(mi.monitor_x, 0);
        assert_eq!(mi.monitor_y, 0);
        assert_eq!(mi.client_name, [0u8; MAX_CLIENT_NAME_LEN]);
        assert_eq!(mi.ltsymbol, [0u8; MAX_LT_SYMBOL_LEN]);
        for i in 0..MAX_TAGS {
            assert_eq!(mi.tag_status_vec[i], TagStatus::default());
        }
    }

    #[test]
    fn test_monitor_info_equality() {
        let mut a = MonitorInfo::default();
        let mut b = MonitorInfo::default();
        assert_eq!(a, b);
        a.monitor_num = 5;
        assert_ne!(a, b);
        b.monitor_num = 5;
        assert_eq!(a, b);
    }

    // ── MonitorInfo: client_name / ltsymbol ──────────────────────────────────

    #[test]
    fn test_client_name_empty_string() {
        let mut mi = MonitorInfo::default();
        mi.set_client_name("nonempty");
        mi.set_client_name("");
        assert_eq!(mi.get_client_name(), "");
        assert_eq!(mi.client_name, [0u8; MAX_CLIENT_NAME_LEN]);
    }

    #[test]
    fn test_client_name_exact_max_minus_one() {
        // MAX_CLIENT_NAME_LEN-1 个字节应能完整存储
        let name = "x".repeat(MAX_CLIENT_NAME_LEN - 1);
        let mut mi = MonitorInfo::default();
        mi.set_client_name(&name);
        assert_eq!(mi.get_client_name(), name);
    }

    #[test]
    fn test_client_name_truncated_at_max() {
        // 超长名称必须被截断，且不能读出超过 MAX_CLIENT_NAME_LEN-1 个字符
        let long = "y".repeat(MAX_CLIENT_NAME_LEN + 50);
        let mut mi = MonitorInfo::default();
        mi.set_client_name(&long);
        let got = mi.get_client_name();
        assert!(got.len() < MAX_CLIENT_NAME_LEN);
        assert!(got.chars().all(|c| c == 'y'));
    }

    #[test]
    fn test_client_name_truncation_preserves_utf8_boundary() {
        let mut mi = MonitorInfo::default();
        let name = "界".repeat(MAX_CLIENT_NAME_LEN);
        mi.set_client_name(&name);
        let got = mi.get_client_name();
        assert!(got.len() < MAX_CLIENT_NAME_LEN);
        assert!(!got.contains('\u{fffd}'));
        assert!(got.chars().all(|character| character == '界'));
        assert!(matches!(mi.client_name_lossy(), Cow::Borrowed(_)));
    }

    #[test]
    fn test_client_name_overwrite_clears_previous() {
        let mut mi = MonitorInfo::default();
        mi.set_client_name("long_name_here");
        mi.set_client_name("hi");
        // 之前的字符不能泄漏到新名称后面
        assert_eq!(mi.get_client_name(), "hi");
        let null_pos = mi.client_name.iter().position(|&b| b == 0).unwrap();
        assert_eq!(null_pos, 2);
    }

    #[test]
    fn test_client_name_single_char() {
        let mut mi = MonitorInfo::default();
        mi.set_client_name("X");
        assert_eq!(mi.get_client_name(), "X");
        // 只有第 0 字节被写入，第 1 字节应为 0（终止符）
        assert_eq!(mi.client_name[0], b'X');
        assert_eq!(mi.client_name[1], 0);
    }

    #[test]
    fn fixed_string_setters_discard_suffix_after_embedded_nul() {
        let mut monitor = MonitorInfo::default();
        monitor.set_client_name("界\0hidden");
        monitor.set_client_app_id("org.example.App\0hidden");
        monitor.set_ltsymbol("[T]\0hidden");
        assert_eq!(monitor.client_name_lossy(), "界");
        assert!(monitor.client_name["界".len()..]
            .iter()
            .all(|byte| *byte == 0));
        assert!(monitor.client_app_id[b"org.example.App".len()..]
            .iter()
            .all(|byte| *byte == 0));
        assert!(monitor.ltsymbol[b"[T]".len()..]
            .iter()
            .all(|byte| *byte == 0));

        let mut window = MinimizedWindowInfo::default();
        window.set_title("终端\0hidden");
        window.set_app_id("org.example.Terminal\0hidden");
        assert_eq!(window.title_lossy(), "终端");
        assert!(window.title["终端".len()..].iter().all(|byte| *byte == 0));
        assert!(window.app_id[b"org.example.Terminal".len()..]
            .iter()
            .all(|byte| *byte == 0));
    }

    #[test]
    fn test_ltsymbol_empty_string() {
        let mut mi = MonitorInfo::default();
        mi.set_ltsymbol("[M]");
        mi.set_ltsymbol("");
        assert_eq!(mi.get_ltsymbol(), "");
    }

    #[test]
    fn test_ltsymbol_exact_max_minus_one() {
        let sym = "s".repeat(MAX_LT_SYMBOL_LEN - 1);
        let mut mi = MonitorInfo::default();
        mi.set_ltsymbol(&sym);
        assert_eq!(mi.get_ltsymbol(), sym);
    }

    #[test]
    fn test_ltsymbol_truncated_at_max() {
        let long = "t".repeat(MAX_LT_SYMBOL_LEN + 10);
        let mut mi = MonitorInfo::default();
        mi.set_ltsymbol(&long);
        assert!(mi.get_ltsymbol().len() < MAX_LT_SYMBOL_LEN);
    }

    #[test]
    fn test_ltsymbol_truncation_preserves_utf8_boundary() {
        let mut mi = MonitorInfo::default();
        mi.set_ltsymbol(&"🖥".repeat(MAX_LT_SYMBOL_LEN));
        let got = mi.get_ltsymbol();
        assert!(got.len() < MAX_LT_SYMBOL_LEN);
        assert!(!got.contains('\u{fffd}'));
        assert!(got.chars().all(|character| character == '🖥'));
    }

    // ── MonitorInfo: tag 状态 ────────────────────────────────────────────────

    #[test]
    fn test_all_tag_indices_valid() {
        let mut mi = MonitorInfo::default();
        for i in 0..MAX_TAGS {
            let s = TagStatus::new(i % 2 == 0, i % 3 == 0, i % 4 == 0, i % 5 == 0);
            mi.set_tag_status(i, s);
        }
        for i in 0..MAX_TAGS {
            let expected = TagStatus::new(i % 2 == 0, i % 3 == 0, i % 4 == 0, i % 5 == 0);
            assert_eq!(mi.get_tag_status(i), Some(expected));
        }
    }

    #[test]
    fn test_tag_index_out_of_bounds() {
        let mi = MonitorInfo::default();
        assert_eq!(mi.get_tag_status(MAX_TAGS), None);
        assert_eq!(mi.get_tag_status(MAX_TAGS + 100), None);
    }

    #[test]
    fn test_set_tag_status_at_max_is_noop() {
        let mut mi = MonitorInfo::default();
        let before: Vec<_> = (0..MAX_TAGS).map(|i| mi.get_tag_status(i)).collect();
        // MAX_TAGS 及以上应为 no-op，不 panic
        mi.set_tag_status(MAX_TAGS, TagStatus::new(true, true, true, true));
        mi.set_tag_status(MAX_TAGS + 1, TagStatus::new(true, true, true, true));
        let after: Vec<_> = (0..MAX_TAGS).map(|i| mi.get_tag_status(i)).collect();
        assert_eq!(before, after);
    }

    // ── MinimizedWindowInfo ─────────────────────────────────────────────────

    #[test]
    fn minimized_window_strings_truncate_on_utf8_boundaries_and_clear_old_bytes() {
        let mut window = MinimizedWindowInfo::new(
            0xfeed_beef,
            -2,
            MINIMIZED_WINDOW_FLAG_PREVIEW_AVAILABLE | MINIMIZED_WINDOW_FLAG_URGENT,
        );
        window.set_title(&"界".repeat(MAX_WINDOW_TITLE_LEN));
        window.set_app_id(&"应用".repeat(MAX_APP_ID_LEN));

        assert!(window.get_title().len() < MAX_WINDOW_TITLE_LEN);
        assert!(window.get_app_id().len() < MAX_APP_ID_LEN);
        assert!(!window.get_title().contains('\u{fffd}'));
        assert!(!window.get_app_id().contains('\u{fffd}'));

        window.set_title("term");
        window.set_app_id("org.example.Terminal");
        assert_eq!(window.title_lossy(), "term");
        assert_eq!(window.app_id_lossy(), "org.example.Terminal");
        assert!(window.title[5..].iter().all(|byte| *byte == 0));
    }

    // ── SharedMessage ────────────────────────────────────────────────────────

    #[test]
    fn test_shared_message_default_is_zero_valued_and_reproducible() {
        // Default 不含时钟副作用：可复现的零值，default() == default()。
        let msg = SharedMessage::default();
        assert_eq!(msg.get_timestamp(), 0);
        assert_eq!(msg, SharedMessage::default());
    }

    #[test]
    fn test_shared_message_new_and_default_equivalent_structure() {
        let a = SharedMessage::new();
        let b = SharedMessage::default();
        // 两者的 monitor_info 字段应相同（都是 MonitorInfo::default()）
        assert_eq!(a.get_monitor_info(), b.get_monitor_info());
        // new() 打时间戳，default() 是零值
        assert!(a.get_timestamp() > 0);
        assert_eq!(b.get_timestamp(), 0);
    }

    #[test]
    fn test_update_timestamp_strictly_increases() {
        let mut msg = SharedMessage::new();
        let t1 = msg.get_timestamp();
        std::thread::sleep(std::time::Duration::from_millis(5));
        msg.update_timestamp();
        let t2 = msg.get_timestamp();
        assert!(t2 > t1, "t2={t2} should be strictly greater than t1={t1}");
    }

    #[test]
    fn test_get_monitor_info_ref_reflects_mutation() {
        let mut msg = SharedMessage::new();
        msg.get_monitor_info_mut().monitor_num = 42;
        // 通过不可变引用读取刚才的修改
        assert_eq!(msg.get_monitor_info().monitor_num, 42);
        msg.get_monitor_info_mut().set_client_name("hello");
        assert_eq!(msg.get_monitor_info().get_client_name(), "hello");
    }

    #[test]
    fn test_with_monitor_info_preserves_all_fields() {
        let mut mi = MonitorInfo {
            monitor_num: 3,
            monitor_width: 2560,
            monitor_height: 1440,
            monitor_x: -100,
            monitor_y: 50,
            ..MonitorInfo::default()
        };
        mi.set_client_name("wm_client");
        mi.set_ltsymbol("[M]");
        for i in 0..MAX_TAGS {
            mi.set_tag_status(i, TagStatus::new(i % 2 == 0, true, false, i % 3 == 0));
        }

        let msg = SharedMessage::with_monitor_info(mi);
        assert!(msg.get_timestamp() > 0);
        let got = msg.get_monitor_info();

        assert_eq!(got.monitor_num, 3);
        assert_eq!(got.monitor_width, 2560);
        assert_eq!(got.monitor_height, 1440);
        assert_eq!(got.monitor_x, -100);
        assert_eq!(got.monitor_y, 50);
        assert_eq!(got.get_client_name(), "wm_client");
        assert_eq!(got.get_ltsymbol(), "[M]");
        for i in 0..MAX_TAGS {
            assert_eq!(
                got.get_tag_status(i),
                Some(TagStatus::new(i % 2 == 0, true, false, i % 3 == 0))
            );
        }
    }

    #[test]
    fn minimized_window_projection_is_bounded_and_reports_overflow() {
        let windows: Vec<_> = (0..MAX_MINIMIZED_WINDOWS + 3)
            .map(|index| MinimizedWindowInfo::new(index as u64 + 1, index as i32, 0))
            .collect();
        let mut message = SharedMessage::default();
        assert_eq!(
            message.set_minimized_windows(&windows),
            MAX_MINIMIZED_WINDOWS
        );
        assert_eq!(
            message.minimized_windows(),
            &windows[..MAX_MINIMIZED_WINDOWS]
        );
        assert_ne!(message.minimized_flags & MINIMIZED_LIST_FLAG_OVERFLOW, 0);

        assert_eq!(message.set_minimized_windows(&windows[..2]), 2);
        assert_eq!(message.minimized_windows(), &windows[..2]);
        assert_eq!(message.minimized_flags & MINIMIZED_LIST_FLAG_OVERFLOW, 0);
        assert_eq!(message.minimized_windows[2], MinimizedWindowInfo::default());

        message.minimized_count = u32::MAX;
        assert_eq!(message.minimized_windows().len(), MAX_MINIMIZED_WINDOWS);
    }

    // ── CommandType ──────────────────────────────────────────────────────────

    #[test]
    fn test_command_type_default() {
        assert_eq!(CommandType::default(), CommandType::None);
    }

    #[test]
    fn test_command_type_known_u32_values() {
        assert_eq!(u32::from(CommandType::None), 0);
        assert_eq!(u32::from(CommandType::ViewTag), 1);
        assert_eq!(u32::from(CommandType::ToggleTag), 2);
        assert_eq!(u32::from(CommandType::SetLayout), 3);
        assert_eq!(u32::from(CommandType::ShellHub), 4);
        assert_eq!(u32::from(CommandType::RestoreMinimized), 5);
        assert_eq!(u32::from(CommandType::PreviewMinimized), 6);
        assert_eq!(u32::from(CommandType::SetMinimizedGeometry), 7);
    }

    #[test]
    fn test_command_type_from_u32() {
        assert_eq!(CommandType::from(0u32), CommandType::None);
        assert_eq!(CommandType::from(1u32), CommandType::ViewTag);
        assert_eq!(CommandType::from(2u32), CommandType::ToggleTag);
        assert_eq!(CommandType::from(3u32), CommandType::SetLayout);
        assert_eq!(CommandType::from(4u32), CommandType::ShellHub);
        assert_eq!(CommandType::from(5u32), CommandType::RestoreMinimized);
        assert_eq!(CommandType::from(6u32), CommandType::PreviewMinimized);
        assert_eq!(CommandType::from(7u32), CommandType::SetMinimizedGeometry);
    }

    #[test]
    fn test_command_type_unknown_values_map_to_none() {
        assert_eq!(CommandType::from(8u32), CommandType::None);
        assert_eq!(CommandType::from(u32::MAX), CommandType::None);
        assert_eq!(CommandType::from(100u32), CommandType::None);
        assert_eq!(CommandType::from_raw(8), None);
        assert_eq!(CommandType::from_raw(u32::MAX), None);
    }

    #[test]
    fn test_command_type_roundtrip() {
        let types = [
            CommandType::None,
            CommandType::ViewTag,
            CommandType::ToggleTag,
            CommandType::SetLayout,
            CommandType::ShellHub,
            CommandType::RestoreMinimized,
            CommandType::PreviewMinimized,
            CommandType::SetMinimizedGeometry,
        ];
        for &ct in &types {
            let v: u32 = ct.into();
            assert_eq!(CommandType::from(v), ct);
        }
    }

    // ── ShellHubRoute ────────────────────────────────────────────────────────

    #[test]
    fn test_shell_hub_route_roundtrip_is_total_over_all_variants() {
        for &route in &ShellHubRoute::ALL {
            let raw = route.as_raw();
            assert_eq!(ShellHubRoute::from_raw(raw), Some(route));
            assert_eq!(ShellHubRoute::from(raw), route);
        }
        assert_eq!(ShellHubRoute::default(), ShellHubRoute::Hub);
    }

    #[test]
    fn test_shell_hub_route_unknown_is_strict_but_degrades_to_hub() {
        assert_eq!(ShellHubRoute::from_raw(6), None);
        assert_eq!(ShellHubRoute::from_raw(u32::MAX), None);
        // A newer bar talking to an older window manager still lands somewhere
        // useful instead of having its command silently dropped.
        assert_eq!(ShellHubRoute::from_raw_or_hub(6), ShellHubRoute::Hub);
        assert_eq!(ShellHubRoute::from(u32::MAX), ShellHubRoute::Hub);
    }

    // ── SharedCommand ────────────────────────────────────────────────────────

    #[test]
    fn test_shared_command_default() {
        let cmd = SharedCommand::default();
        assert_eq!(cmd.get_command_type(), CommandType::None);
        assert_eq!(cmd.get_parameter(), 0);
        assert_eq!(cmd.get_monitor_id(), 0);
    }

    #[test]
    fn test_shared_command_new_direct() {
        let cmd = SharedCommand::new(CommandType::ViewTag, 7, 3);
        assert_eq!(cmd.get_command_type(), CommandType::ViewTag);
        assert_eq!(cmd.get_parameter(), 7);
        assert_eq!(cmd.get_monitor_id(), 3);
        assert!(cmd.get_timestamp() > 0);
    }

    #[test]
    fn test_shared_command_view_tag() {
        let cmd = SharedCommand::view_tag(1 << 2, 0);
        assert_eq!(cmd.get_command_type(), CommandType::ViewTag);
        assert_eq!(cmd.get_parameter(), 1 << 2);
        assert_eq!(cmd.get_monitor_id(), 0);
    }

    #[test]
    fn test_shared_command_toggle_tag() {
        let cmd = SharedCommand::toggle_tag(0b101, 2);
        assert_eq!(cmd.get_command_type(), CommandType::ToggleTag);
        assert_eq!(cmd.get_parameter(), 0b101);
        assert_eq!(cmd.get_monitor_id(), 2);
    }

    #[test]
    fn test_shared_command_set_layout() {
        let cmd = SharedCommand::set_layout(2, -1);
        assert_eq!(cmd.get_command_type(), CommandType::SetLayout);
        assert_eq!(cmd.get_parameter(), 2);
        assert_eq!(cmd.get_monitor_id(), -1);
    }

    #[test]
    fn test_shared_command_shell_hub_carries_the_route() {
        for &route in &ShellHubRoute::ALL {
            let cmd = SharedCommand::shell_hub(route, 1);
            assert_eq!(cmd.get_command_type(), CommandType::ShellHub);
            assert_eq!(cmd.get_parameter(), route.as_raw());
            assert_eq!(cmd.get_monitor_id(), 1);
            assert_eq!(cmd.shell_hub_route(), Some(route));
        }
    }

    #[test]
    fn test_shell_hub_route_accessor_ignores_other_commands() {
        assert_eq!(SharedCommand::view_tag(1, 0).shell_hub_route(), None);
        assert_eq!(SharedCommand::set_layout(2, 0).shell_hub_route(), None);
        assert_eq!(SharedCommand::default().shell_hub_route(), None);
    }

    #[test]
    fn minimized_command_constructors_preserve_target_session_flags_and_anchor() {
        let anchor = MinimizedWindowAnchor::new(-12, 34, 56, 78);
        let restore = SharedCommand::restore_minimized(0x1234_5678_9abc_def0, 91, 73, -3, anchor);
        assert_eq!(
            restore.command_type_checked(),
            Some(CommandType::RestoreMinimized)
        );
        assert_eq!(restore.minimized_window_id(), Some(0x1234_5678_9abc_def0));
        assert_eq!(restore.get_window_id(), 0x1234_5678_9abc_def0);
        assert_eq!(restore.get_wm_session_id(), 91);
        assert_eq!(restore.get_minimized_generation(), 73);
        assert_eq!(restore.get_monitor_id(), -3);
        assert_eq!(restore.get_flags(), 0);
        assert_eq!(restore.anchor(), anchor);

        let preview = SharedCommand::preview_minimized(
            44,
            91,
            73,
            2,
            PREVIEW_MINIMIZED_FLAG_VISIBLE | PREVIEW_MINIMIZED_FLAG_RENEWAL,
            anchor,
        );
        assert_eq!(
            preview.command_type_checked(),
            Some(CommandType::PreviewMinimized)
        );
        assert_eq!(preview.minimized_window_id(), Some(44));
        assert_eq!(
            preview.get_flags(),
            PREVIEW_MINIMIZED_FLAG_VISIBLE | PREVIEW_MINIMIZED_FLAG_RENEWAL
        );
        assert_eq!(preview.anchor(), anchor);

        let geometry = SharedCommand::set_minimized_geometry(44, 91, 73, 2, anchor);
        assert_eq!(
            geometry.command_type_checked(),
            Some(CommandType::SetMinimizedGeometry)
        );
        assert_eq!(geometry.minimized_window_id(), Some(44));
        assert_eq!(geometry.get_window_id(), 44);
        assert_eq!(geometry.get_wm_session_id(), 91);
        assert_eq!(geometry.anchor(), anchor);

        let shelf = SharedCommand::set_minimized_geometry(0, 91, 73, 2, anchor);
        assert_eq!(shelf.minimized_window_id(), None);
    }

    #[test]
    fn test_all_shared_command_constructors_have_nonzero_timestamp() {
        assert!(SharedCommand::view_tag(0, 0).get_timestamp() > 0);
        assert!(SharedCommand::toggle_tag(0, 0).get_timestamp() > 0);
        assert!(SharedCommand::set_layout(0, 0).get_timestamp() > 0);
        assert!(SharedCommand::shell_hub(ShellHubRoute::Hub, 0).get_timestamp() > 0);
        let anchor = MinimizedWindowAnchor::default();
        assert!(SharedCommand::restore_minimized(1, 2, 3, 0, anchor).get_timestamp() > 0);
        assert!(SharedCommand::preview_minimized(1, 2, 3, 0, 0, anchor).get_timestamp() > 0);
        assert!(SharedCommand::set_minimized_geometry(0, 2, 3, 0, anchor).get_timestamp() > 0);
        assert!(SharedCommand::new(CommandType::None, 0, 0).get_timestamp() > 0);
    }

    // ── 内存布局 ──────────────────────────────────────────────────────────────

    #[test]
    fn test_repr_c_sizes_are_deterministic() {
        // repr(C) 的大小在同一平台应当固定
        let sz_tag = std::mem::size_of::<TagStatus>();
        let sz_mi = std::mem::size_of::<MonitorInfo>();
        let sz_msg = std::mem::size_of::<SharedMessage>();
        let sz_cmd = std::mem::size_of::<SharedCommand>();
        assert!(sz_tag > 0);
        assert!(
            sz_mi >= sz_tag * MAX_TAGS + MAX_CLIENT_NAME_LEN + MAX_APP_ID_LEN + MAX_LT_SYMBOL_LEN
        );
        assert_eq!(std::mem::size_of::<MinimizedWindowInfo>(), 176);
        assert_eq!(std::mem::size_of::<MinimizedWindowAnchor>(), 16);
        assert_eq!(sz_msg, 3136);
        assert_eq!(sz_cmd, 64);
    }

    #[test]
    fn test_tag_status_size_is_four_bytes() {
        // 4 个 bool，repr(C) 下每个 bool 占 1 字节
        assert_eq!(std::mem::size_of::<TagStatus>(), 4);
    }

    #[test]
    fn test_shared_command_field_offsets_stable() {
        assert_eq!(std::mem::offset_of!(SharedCommand, cmd_type), 0);
        assert_eq!(std::mem::offset_of!(SharedCommand, parameter), 4);
        assert_eq!(std::mem::offset_of!(SharedCommand, monitor_id), 8);
        assert_eq!(std::mem::offset_of!(SharedCommand, flags), 12);
        assert_eq!(std::mem::offset_of!(SharedCommand, window_id), 16);
        assert_eq!(std::mem::offset_of!(SharedCommand, wm_session_id), 24);
        assert_eq!(
            std::mem::offset_of!(SharedCommand, minimized_generation),
            32
        );
        assert_eq!(std::mem::offset_of!(SharedCommand, anchor_x), 40);
        assert_eq!(std::mem::offset_of!(SharedCommand, anchor_y), 44);
        assert_eq!(std::mem::offset_of!(SharedCommand, anchor_w), 48);
        assert_eq!(std::mem::offset_of!(SharedCommand, anchor_h), 52);
        assert_eq!(std::mem::offset_of!(SharedCommand, timestamp), 56);
    }
}
