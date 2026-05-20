use serde::Serialize;
use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};
use serde_big_array::BigArray;
use std::time::{SystemTime, UNIX_EPOCH};

// 常量定义
pub const MAX_CLIENT_NAME_LEN: usize = 128;
pub const MAX_LT_SYMBOL_LEN: usize = 32;
pub const MAX_TAGS: usize = 9;

#[inline]
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// 使用合理对齐
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Archive, RkyvSerialize, RkyvDeserialize)]
pub struct TagStatus {
    pub is_selected: bool,
    pub is_urg: bool,
    pub is_filled: bool,
    pub is_occ: bool,
}

impl Default for TagStatus {
    fn default() -> Self {
        Self {
            is_selected: false,
            is_urg: false,
            is_filled: false,
            is_occ: false,
        }
    }
}

impl TagStatus {
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Archive, RkyvSerialize, RkyvDeserialize)]
pub struct MonitorInfo {
    pub monitor_num: i32,
    pub monitor_width: i32,
    pub monitor_height: i32,
    pub monitor_x: i32,
    pub monitor_y: i32,
    pub tag_status_vec: [TagStatus; MAX_TAGS],
    #[serde(with = "BigArray")]
    pub client_name: [u8; MAX_CLIENT_NAME_LEN],
    pub ltsymbol: [u8; MAX_LT_SYMBOL_LEN],
}

impl Default for MonitorInfo {
    fn default() -> Self {
        Self {
            client_name: [0; MAX_CLIENT_NAME_LEN],
            tag_status_vec: [TagStatus::default(); MAX_TAGS],
            monitor_num: 0,
            monitor_width: 0,
            monitor_height: 0,
            monitor_x: 0,
            monitor_y: 0,
            ltsymbol: [0; MAX_LT_SYMBOL_LEN],
        }
    }
}

impl MonitorInfo {
    pub fn set_client_name(&mut self, name: &str) {
        let bytes = name.as_bytes();
        let len = bytes.len().min(MAX_CLIENT_NAME_LEN - 1);
        self.client_name.fill(0);
        self.client_name[..len].copy_from_slice(&bytes[..len]);
    }

    pub fn get_client_name(&self) -> String {
        let null_pos = self
            .client_name
            .iter()
            .position(|&x| x == 0)
            .unwrap_or(MAX_CLIENT_NAME_LEN);
        String::from_utf8_lossy(&self.client_name[..null_pos]).to_string()
    }

    pub fn set_ltsymbol(&mut self, symbol: &str) {
        let bytes = symbol.as_bytes();
        let len = bytes.len().min(MAX_LT_SYMBOL_LEN - 1);
        self.ltsymbol.fill(0);
        self.ltsymbol[..len].copy_from_slice(&bytes[..len]);
    }

    pub fn get_ltsymbol(&self) -> String {
        let null_pos = self
            .ltsymbol
            .iter()
            .position(|&x| x == 0)
            .unwrap_or(MAX_LT_SYMBOL_LEN);
        String::from_utf8_lossy(&self.ltsymbol[..null_pos]).to_string()
    }

    pub fn set_tag_status(&mut self, index: usize, status: TagStatus) {
        if index < MAX_TAGS {
            self.tag_status_vec[index] = status;
        }
    }

    pub fn get_tag_status(&self, index: usize) -> Option<TagStatus> {
        if index < MAX_TAGS {
            Some(self.tag_status_vec[index])
        } else {
            None
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Archive, RkyvSerialize, RkyvDeserialize)]
pub struct SharedMessage {
    pub timestamp: u64,
    pub monitor_info: MonitorInfo,
}

impl Default for SharedMessage {
    fn default() -> Self {
        Self {
            timestamp: now_millis(),
            monitor_info: MonitorInfo::default(),
        }
    }
}

impl SharedMessage {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_monitor_info(monitor_info: MonitorInfo) -> Self {
        Self {
            timestamp: now_millis(),
            monitor_info,
        }
    }

    pub fn update_timestamp(&mut self) {
        self.timestamp = now_millis();
    }

    pub fn get_timestamp(&self) -> u64 {
        self.timestamp
    }

    pub fn get_monitor_info(&self) -> &MonitorInfo {
        &self.monitor_info
    }

    pub fn get_monitor_info_mut(&mut self) -> &mut MonitorInfo {
        &mut self.monitor_info
    }
}

// 命令相关定义
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandType {
    None = 0,
    ViewTag = 1,
    ToggleTag = 2,
    SetLayout = 3,
}

impl Default for CommandType {
    fn default() -> Self {
        CommandType::None
    }
}

impl From<u32> for CommandType {
    fn from(value: u32) -> Self {
        match value {
            1 => CommandType::ViewTag,
            2 => CommandType::ToggleTag,
            3 => CommandType::SetLayout,
            _ => CommandType::None,
        }
    }
}

impl From<CommandType> for u32 {
    fn from(cmd_type: CommandType) -> Self {
        cmd_type as u32
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SharedCommand {
    pub cmd_type: u32,
    pub parameter: u32,
    pub monitor_id: i32,
    pub timestamp: u64,
}

impl SharedCommand {
    pub fn new(cmd_type: CommandType, parameter: u32, monitor_id: i32) -> Self {
        Self {
            cmd_type: cmd_type.into(),
            parameter,
            monitor_id,
            timestamp: now_millis(),
        }
    }

    pub fn get_command_type(&self) -> CommandType {
        self.cmd_type.into()
    }

    pub fn view_tag(tag_bit: u32, monitor_id: i32) -> Self {
        Self::new(CommandType::ViewTag, tag_bit, monitor_id)
    }

    pub fn toggle_tag(tag_bit: u32, monitor_id: i32) -> Self {
        Self::new(CommandType::ToggleTag, tag_bit, monitor_id)
    }

    pub fn set_layout(layout_idx: u32, monitor_id: i32) -> Self {
        Self::new(CommandType::SetLayout, layout_idx, monitor_id)
    }

    pub fn get_parameter(&self) -> u32 {
        self.parameter
    }

    pub fn get_monitor_id(&self) -> i32 {
        self.monitor_id
    }

    pub fn get_timestamp(&self) -> u64 {
        self.timestamp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── TagStatus ────────────────────────────────────────────────────────────

    #[test]
    fn test_struct_alignment() {
        assert!(std::mem::size_of::<SharedMessage>() > 0);
        assert!(std::mem::size_of::<SharedCommand>() > 0);
        let mut mi = MonitorInfo::default();
        mi.set_client_name("abc");
        mi.set_ltsymbol("[]=");
        assert_eq!(&mi.get_client_name(), "abc");
        assert_eq!(&mi.get_ltsymbol(), "[]=");
    }

    #[test]
    fn test_tag_status_default_all_false() {
        let s = TagStatus::default();
        assert!(!s.is_selected);
        assert!(!s.is_urg);
        assert!(!s.is_filled);
        assert!(!s.is_occ);
    }

    #[test]
    fn test_tag_status_new() {
        let s = TagStatus::new(true, false, true, false);
        assert!(s.is_selected);
        assert!(!s.is_urg);
        assert!(s.is_filled);
        assert!(!s.is_occ);
    }

    #[test]
    fn test_tag_status_equality() {
        let a = TagStatus::new(true, true, false, false);
        let b = TagStatus::new(true, true, false, false);
        let c = TagStatus::new(false, true, false, false);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn test_tag_status_copy() {
        let original = TagStatus::new(true, false, true, true);
        let copied = original;
        assert_eq!(original, copied);
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
    fn test_monitor_info_methods() {
        let mut info = MonitorInfo::default();
        info.set_client_name("test_client");
        assert_eq!(info.get_client_name(), "test_client");

        let long_name = "a".repeat(200);
        info.set_client_name(&long_name);
        let result = info.get_client_name();
        assert!(result.len() < MAX_CLIENT_NAME_LEN);

        info.set_ltsymbol("[]=");
        assert_eq!(info.get_ltsymbol(), "[]=");

        let status = TagStatus::new(true, false, true, false);
        info.set_tag_status(0, status);
        assert_eq!(info.get_tag_status(0), Some(status));
        assert_eq!(info.get_tag_status(MAX_TAGS), None);
    }

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
    fn test_monitor_info_numeric_fields() {
        let mut mi = MonitorInfo::default();
        mi.monitor_num = -1;
        mi.monitor_width = 1920;
        mi.monitor_height = 1080;
        mi.monitor_x = -320;
        mi.monitor_y = 0;
        assert_eq!(mi.monitor_num, -1);
        assert_eq!(mi.monitor_width, 1920);
        assert_eq!(mi.monitor_height, 1080);
        assert_eq!(mi.monitor_x, -320);
        assert_eq!(mi.monitor_y, 0);
    }

    #[test]
    fn test_monitor_info_copy_semantics() {
        let mut a = MonitorInfo::default();
        a.set_client_name("original");
        a.monitor_num = 7;
        let mut b = a;
        b.set_client_name("copy");
        b.monitor_num = 99;
        // 修改 b 不影响 a
        assert_eq!(a.get_client_name(), "original");
        assert_eq!(a.monitor_num, 7);
    }

    // ── SharedMessage ────────────────────────────────────────────────────────

    #[test]
    fn test_shared_message() {
        let mut message = SharedMessage::new();
        assert!(message.get_timestamp() > 0);
        let monitor_info = message.get_monitor_info_mut();
        monitor_info.set_client_name("test");
        monitor_info.monitor_num = 42;
        assert_eq!(message.get_monitor_info().get_client_name(), "test");
        assert_eq!(message.get_monitor_info().monitor_num, 42);
    }

    #[test]
    fn test_shared_message_with_monitor_info() {
        let mut mi = MonitorInfo::default();
        mi.monitor_num = 3;
        mi.set_client_name("wm");
        let msg = SharedMessage::with_monitor_info(mi);
        assert!(msg.get_timestamp() > 0);
        assert_eq!(msg.get_monitor_info().monitor_num, 3);
        assert_eq!(msg.get_monitor_info().get_client_name(), "wm");
    }

    #[test]
    fn test_shared_message_update_timestamp() {
        let mut msg = SharedMessage::new();
        let t1 = msg.get_timestamp();
        // 等待 1ms 确保时钟前进
        std::thread::sleep(std::time::Duration::from_millis(2));
        msg.update_timestamp();
        let t2 = msg.get_timestamp();
        assert!(t2 >= t1);
    }

    #[test]
    fn test_shared_message_copy_semantics() {
        let mut a = SharedMessage::new();
        a.get_monitor_info_mut().monitor_num = 5;
        let mut b = a;
        b.get_monitor_info_mut().monitor_num = 99;
        assert_eq!(a.get_monitor_info().monitor_num, 5);
    }

    #[test]
    fn test_shared_message_default_timestamp_nonzero() {
        let msg = SharedMessage::default();
        assert!(msg.get_timestamp() > 0);
    }

    // ── CommandType ──────────────────────────────────────────────────────────

    #[test]
    fn test_command_type_from_u32() {
        assert_eq!(CommandType::from(0u32), CommandType::None);
        assert_eq!(CommandType::from(1u32), CommandType::ViewTag);
        assert_eq!(CommandType::from(2u32), CommandType::ToggleTag);
        assert_eq!(CommandType::from(3u32), CommandType::SetLayout);
    }

    #[test]
    fn test_command_type_unknown_values_map_to_none() {
        assert_eq!(CommandType::from(4u32), CommandType::None);
        assert_eq!(CommandType::from(u32::MAX), CommandType::None);
        assert_eq!(CommandType::from(100u32), CommandType::None);
    }

    #[test]
    fn test_command_type_roundtrip() {
        let types = [
            CommandType::None,
            CommandType::ViewTag,
            CommandType::ToggleTag,
            CommandType::SetLayout,
        ];
        for &ct in &types {
            let v: u32 = ct.into();
            assert_eq!(CommandType::from(v), ct);
        }
    }

    #[test]
    fn test_command_type_default() {
        assert_eq!(CommandType::default(), CommandType::None);
    }

    // ── SharedCommand ────────────────────────────────────────────────────────

    #[test]
    fn test_shared_command() {
        let cmd = SharedCommand::view_tag(1 << 2, 0);
        assert_eq!(cmd.get_command_type(), CommandType::ViewTag);
        assert_eq!(cmd.get_parameter(), 1 << 2);
        assert_eq!(cmd.get_monitor_id(), 0);
        assert!(cmd.get_timestamp() > 0);
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
    fn test_shared_command_new_direct() {
        let cmd = SharedCommand::new(CommandType::ViewTag, 7, 3);
        assert_eq!(cmd.get_command_type(), CommandType::ViewTag);
        assert_eq!(cmd.get_parameter(), 7);
        assert_eq!(cmd.get_monitor_id(), 3);
        assert!(cmd.get_timestamp() > 0);
    }

    #[test]
    fn test_shared_command_copy_semantics() {
        let a = SharedCommand::view_tag(1, 0);
        let b = a;
        assert_eq!(a.get_parameter(), b.get_parameter());
        assert_eq!(a.get_monitor_id(), b.get_monitor_id());
    }

    #[test]
    fn test_shared_command_default() {
        let cmd = SharedCommand::default();
        assert_eq!(cmd.get_command_type(), CommandType::None);
        assert_eq!(cmd.get_parameter(), 0);
        assert_eq!(cmd.get_monitor_id(), 0);
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
        assert!(sz_mi >= sz_tag * MAX_TAGS + MAX_CLIENT_NAME_LEN + MAX_LT_SYMBOL_LEN);
        assert!(sz_msg >= sz_mi + 8); // timestamp (u64) + MonitorInfo
        assert!(sz_cmd >= 16); // cmd_type + parameter + monitor_id + timestamp
    }

    #[test]
    fn test_shared_message_size_matches_components() {
        // SharedMessage 至少能容纳 timestamp(u64) 和 MonitorInfo
        assert!(
            std::mem::size_of::<SharedMessage>()
                >= std::mem::size_of::<u64>() + std::mem::size_of::<MonitorInfo>()
        );
    }

    // ── TagStatus 全部 16 种布尔组合 ─────────────────────────────────────────

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
            // 相同参数构造两次，结果应相等
            assert_eq!(s, TagStatus::new(sel, urg, fil, occ));
        }
    }

    // ── set_tag_status 越界写入应为无副作用 ──────────────────────────────────

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

    // ── client_name 含特殊字符 ────────────────────────────────────────────────

    #[test]
    fn test_client_name_special_ascii_chars() {
        let mut mi = MonitorInfo::default();
        let special = "foo_bar-baz.123 !@#";
        mi.set_client_name(special);
        assert_eq!(mi.get_client_name(), special);
    }

    #[test]
    fn test_ltsymbol_common_patterns() {
        let mut mi = MonitorInfo::default();
        for sym in &["[]=", "[M]", "TTT", ">", "###", "floating"] {
            mi.set_ltsymbol(sym);
            assert_eq!(mi.get_ltsymbol(), *sym);
        }
    }

    // ── MonitorInfo PartialEq ─────────────────────────────────────────────────

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

    #[test]
    fn test_monitor_info_equality_client_name() {
        let mut a = MonitorInfo::default();
        let mut b = MonitorInfo::default();
        a.set_client_name("foo");
        assert_ne!(a, b);
        b.set_client_name("foo");
        assert_eq!(a, b);
    }

    // ── SharedMessage::new() 与 default() 结构等价 ────────────────────────────

    #[test]
    fn test_shared_message_new_and_default_equivalent_structure() {
        let a = SharedMessage::new();
        let b = SharedMessage::default();
        // 两者的 monitor_info 字段应相同（都是 MonitorInfo::default()）
        assert_eq!(a.get_monitor_info(), b.get_monitor_info());
        // timestamp 均非零
        assert!(a.get_timestamp() > 0);
        assert!(b.get_timestamp() > 0);
    }

    // ── 所有 SharedCommand 构造器时间戳均非零 ─────────────────────────────────

    #[test]
    fn test_all_shared_command_constructors_have_nonzero_timestamp() {
        assert!(SharedCommand::view_tag(0, 0).get_timestamp() > 0);
        assert!(SharedCommand::toggle_tag(0, 0).get_timestamp() > 0);
        assert!(SharedCommand::set_layout(0, 0).get_timestamp() > 0);
        assert!(SharedCommand::new(CommandType::None, 0, 0).get_timestamp() > 0);
    }

    // ── CommandType::None 可作为 SharedCommand 参数 ───────────────────────────

    #[test]
    fn test_shared_command_with_command_type_none() {
        let cmd = SharedCommand::new(CommandType::None, 0, 0);
        assert_eq!(cmd.get_command_type(), CommandType::None);
        assert_eq!(cmd.cmd_type, 0u32);
    }

    // ── client_name 全零时 get_client_name 返回空串 ───────────────────────────

    #[test]
    fn test_get_client_name_on_zero_array_returns_empty() {
        let mi = MonitorInfo::default(); // client_name 全为 0
        assert_eq!(mi.get_client_name(), "");
        assert_eq!(mi.get_ltsymbol(), "");
    }

    // ── SharedMessage PartialEq ───────────────────────────────────────────────

    #[test]
    fn test_shared_message_partial_eq_same_content() {
        let mi = MonitorInfo::default();
        let x = SharedMessage::with_monitor_info(mi);
        let y = SharedMessage::with_monitor_info(mi);
        // monitor_info 部分应相等
        assert_eq!(x.get_monitor_info(), y.get_monitor_info());
    }

    #[test]
    fn test_shared_message_partial_eq_differs_on_monitor_num() {
        let mut a = SharedMessage::default();
        let mut b = SharedMessage::default();
        a.get_monitor_info_mut().monitor_num = 1;
        b.get_monitor_info_mut().monitor_num = 2;
        assert_ne!(a.get_monitor_info(), b.get_monitor_info());
    }

    // ── SharedCommand PartialEq ───────────────────────────────────────────────

    #[test]
    fn test_shared_command_partial_eq() {
        let a = SharedCommand::new(CommandType::ViewTag, 42, 1);
        let b = SharedCommand::new(CommandType::ViewTag, 42, 1);
        // 字段值相同（但 timestamp 可能不同）
        assert_eq!(a.get_command_type(), b.get_command_type());
        assert_eq!(a.get_parameter(), b.get_parameter());
        assert_eq!(a.get_monitor_id(), b.get_monitor_id());
    }

    #[test]
    fn test_shared_command_ne_on_different_type() {
        let a = SharedCommand::view_tag(1, 0);
        let b = SharedCommand::toggle_tag(1, 0);
        assert_ne!(a.get_command_type(), b.get_command_type());
    }

    // ── MonitorInfo: 设置某个 tag 不影响其他 tag ──────────────────────────────

    #[test]
    fn test_tag_status_set_one_does_not_affect_others() {
        let mut mi = MonitorInfo::default();
        // 先把所有 tag 设为全 true
        for i in 0..MAX_TAGS {
            mi.set_tag_status(i, TagStatus::new(true, true, true, true));
        }
        // 修改 tag[3]
        mi.set_tag_status(3, TagStatus::new(false, false, false, false));
        // 其他 tag 应不受影响
        for i in 0..MAX_TAGS {
            if i == 3 {
                assert_eq!(
                    mi.get_tag_status(i),
                    Some(TagStatus::new(false, false, false, false))
                );
            } else {
                assert_eq!(
                    mi.get_tag_status(i),
                    Some(TagStatus::new(true, true, true, true))
                );
            }
        }
    }

    // ── MonitorInfo: 所有 tag 全真 ────────────────────────────────────────────

    #[test]
    fn test_monitor_info_all_tags_truthy() {
        let mut mi = MonitorInfo::default();
        let full = TagStatus::new(true, true, true, true);
        for i in 0..MAX_TAGS {
            mi.set_tag_status(i, full);
        }
        for i in 0..MAX_TAGS {
            assert_eq!(mi.get_tag_status(i), Some(full));
        }
    }

    // ── SharedCommand: 极端参数值 ─────────────────────────────────────────────

    #[test]
    fn test_shared_command_max_parameter() {
        let cmd = SharedCommand::view_tag(u32::MAX, i32::MIN);
        assert_eq!(cmd.get_parameter(), u32::MAX);
        assert_eq!(cmd.get_monitor_id(), i32::MIN);
    }

    #[test]
    fn test_shared_command_max_monitor_id() {
        let cmd = SharedCommand::toggle_tag(0, i32::MAX);
        assert_eq!(cmd.get_monitor_id(), i32::MAX);
    }

    // ── client_name / ltsymbol: 单字符 ───────────────────────────────────────

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
    fn test_ltsymbol_single_char() {
        let mut mi = MonitorInfo::default();
        mi.set_ltsymbol("F");
        assert_eq!(mi.get_ltsymbol(), "F");
        assert_eq!(mi.ltsymbol[0], b'F');
        assert_eq!(mi.ltsymbol[1], 0);
    }

    // ── update_timestamp 时间戳实际递增 ──────────────────────────────────────

    #[test]
    fn test_update_timestamp_strictly_increases() {
        let mut msg = SharedMessage::new();
        let t1 = msg.get_timestamp();
        std::thread::sleep(std::time::Duration::from_millis(5));
        msg.update_timestamp();
        let t2 = msg.get_timestamp();
        assert!(t2 > t1, "t2={t2} should be strictly greater than t1={t1}");
    }

    // ── CommandType 完整性：所有枚举值 u32 为已知数 ───────────────────────────

    #[test]
    fn test_command_type_known_u32_values() {
        assert_eq!(u32::from(CommandType::None), 0);
        assert_eq!(u32::from(CommandType::ViewTag), 1);
        assert_eq!(u32::from(CommandType::ToggleTag), 2);
        assert_eq!(u32::from(CommandType::SetLayout), 3);
    }

    // ── SharedMessage 完整相等性（同 timestamp + 同内容）─────────────────────

    #[test]
    fn test_shared_message_full_equality() {
        // 直接构造相同内容的两条消息，timestamp 手动对齐
        let mi = MonitorInfo::default();
        let mut a = SharedMessage::with_monitor_info(mi);
        let mut b = SharedMessage::with_monitor_info(mi);
        // 强制 timestamp 一致后应完全相等
        let ts = a.get_timestamp();
        // 通过 unsafe 写入相同 timestamp（测试 PartialEq 覆盖 timestamp 字段）
        a.timestamp = ts;
        b.timestamp = ts;
        assert_eq!(a, b);
    }

    #[test]
    fn test_shared_message_ne_on_timestamp() {
        let mi = MonitorInfo::default();
        let a = SharedMessage::with_monitor_info(mi);
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = SharedMessage::with_monitor_info(mi);
        // 时间戳不同则消息不等
        if a.get_timestamp() != b.get_timestamp() {
            assert_ne!(a, b);
        }
    }

    // ── MonitorInfo: 负坐标值 ─────────────────────────────────────────────────

    #[test]
    fn test_monitor_info_negative_coordinates() {
        let mut mi = MonitorInfo::default();
        mi.monitor_x = i32::MIN;
        mi.monitor_y = i32::MIN;
        mi.monitor_width = -1920;
        mi.monitor_height = -1080;
        assert_eq!(mi.monitor_x, i32::MIN);
        assert_eq!(mi.monitor_y, i32::MIN);
        assert_eq!(mi.monitor_width, -1920);
        assert_eq!(mi.monitor_height, -1080);
    }

    #[test]
    fn test_monitor_info_ne_on_each_field() {
        let base = MonitorInfo::default();
        let mut mi = base;
        mi.monitor_num = 1;
        assert_ne!(mi, base);

        let mut mi = base;
        mi.monitor_width = 1920;
        assert_ne!(mi, base);

        let mut mi = base;
        mi.monitor_height = 1080;
        assert_ne!(mi, base);

        let mut mi = base;
        mi.monitor_x = 10;
        assert_ne!(mi, base);

        let mut mi = base;
        mi.monitor_y = 20;
        assert_ne!(mi, base);

        let mut mi = base;
        mi.set_ltsymbol("[M]");
        assert_ne!(mi, base);

        let mut mi = base;
        mi.set_tag_status(0, TagStatus::new(true, false, false, false));
        assert_ne!(mi, base);
    }

    // ── get_monitor_info 返回的引用反映可变引用的修改 ─────────────────────────

    #[test]
    fn test_get_monitor_info_ref_reflects_mutation() {
        let mut msg = SharedMessage::new();
        msg.get_monitor_info_mut().monitor_num = 42;
        // 通过不可变引用读取刚才的修改
        assert_eq!(msg.get_monitor_info().monitor_num, 42);
        msg.get_monitor_info_mut().set_client_name("hello");
        assert_eq!(msg.get_monitor_info().get_client_name(), "hello");
    }

    // ── with_monitor_info 精确保留所有字段 ───────────────────────────────────

    #[test]
    fn test_with_monitor_info_preserves_all_fields() {
        let mut mi = MonitorInfo::default();
        mi.monitor_num = 3;
        mi.monitor_width = 2560;
        mi.monitor_height = 1440;
        mi.monitor_x = -100;
        mi.monitor_y = 50;
        mi.set_client_name("wm_client");
        mi.set_ltsymbol("[M]");
        for i in 0..MAX_TAGS {
            mi.set_tag_status(i, TagStatus::new(i % 2 == 0, true, false, i % 3 == 0));
        }

        let msg = SharedMessage::with_monitor_info(mi);
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

    // ── CommandType PartialEq ─────────────────────────────────────────────────

    #[test]
    fn test_command_type_partial_eq() {
        assert_eq!(CommandType::None, CommandType::None);
        assert_eq!(CommandType::ViewTag, CommandType::ViewTag);
        assert_ne!(CommandType::ViewTag, CommandType::ToggleTag);
        assert_ne!(CommandType::ToggleTag, CommandType::SetLayout);
        assert_ne!(CommandType::None, CommandType::SetLayout);
    }

    // ── SharedCommand: 不同监视器 ID 的不等性 ────────────────────────────────

    #[test]
    fn test_shared_command_ne_on_monitor_id() {
        let a = SharedCommand::new(CommandType::ViewTag, 1, 0);
        let b = SharedCommand::new(CommandType::ViewTag, 1, 1);
        assert_ne!(a.get_monitor_id(), b.get_monitor_id());
    }

    #[test]
    fn test_shared_command_ne_on_parameter() {
        let a = SharedCommand::view_tag(1, 0);
        let b = SharedCommand::view_tag(2, 0);
        assert_ne!(a.get_parameter(), b.get_parameter());
    }

    // ── 结构体大小对齐在编译期固定（确保 repr(C) 稳定）────────────────────────

    #[test]
    fn test_tag_status_size_is_four_bytes() {
        // 4 个 bool，repr(C) 下每个 bool 占 1 字节
        assert_eq!(std::mem::size_of::<TagStatus>(), 4);
    }

    #[test]
    fn test_shared_command_field_offsets_stable() {
        // SharedCommand 是 repr(C)，字段顺序固定：cmd_type(4) + parameter(4) + monitor_id(4) + _pad(4) + timestamp(8)
        assert!(std::mem::size_of::<SharedCommand>() >= 20);
    }
}
