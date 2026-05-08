//! 状态栏消息生成和标签状态计算
//!
//! 此模块负责生成发送给状态栏的消息，包括标签状态、
//! 窗口信息、监视器几何等。

use crate::config::CONFIG;
use crate::core::models::{ClientKey, MonitorKey, WMClient, WMMonitor};
use shared_structures::{MonitorInfo, SharedMessage, TagStatus};

/// 状态栏消息构建器
pub struct StatusBarBuilder;

impl StatusBarBuilder {
    /// 计算标签掩码（已占用和紧急）
    ///
    /// # 参数
    /// - `clients`: 客户端映射
    /// - `monitor_clients`: 监视器的客户端列表
    ///
    /// # 返回
    /// (已占用标签掩码, 紧急标签掩码)
    pub fn calculate_tag_masks(
        clients: &slotmap::SlotMap<ClientKey, WMClient>,
        monitor_clients: &[ClientKey],
    ) -> (u32, u32) {
        let mut occupied_tags_mask = 0u32;
        let mut urgent_tags_mask = 0u32;

        let config_mask = CONFIG.load().tagmask();

        for &client_key in monitor_clients {
            if let Some(client) = clients.get(client_key) {
                let effective_tags = client.state.tags & config_mask;

                // 跳过 sticky 窗口（在所有标签上显示）
                if effective_tags == config_mask {
                    continue;
                }

                occupied_tags_mask |= effective_tags;

                if client.state.is_urgent {
                    urgent_tags_mask |= effective_tags;
                }
            }
        }

        let final_occupied = occupied_tags_mask & config_mask;
        let final_urgent = urgent_tags_mask & config_mask;

        log::info!(
            "[StatusBar] Tag masks - Occupied: {:b}, Urgent: {:b}",
            final_occupied,
            final_urgent
        );

        (final_occupied, final_urgent)
    }

    /// 检查标签是否被填充（当前选中客户端在此标签上）
    ///
    /// # 参数
    /// - `clients`: 客户端映射
    /// - `monitor`: 监视器
    /// - `tag_bit`: 标签位掩码
    /// - `is_selected_monitor`: 是否是当前选中的监视器
    ///
    /// # 返回
    /// 如果标签被填充则返回 true
    pub fn is_filled_tag(
        clients: &slotmap::SlotMap<ClientKey, WMClient>,
        monitor: &WMMonitor,
        tag_bit: u32,
        is_selected_monitor: bool,
    ) -> bool {
        // 如果不是当前选中的显示器，不用高亮 Focus 状态
        if !is_selected_monitor {
            return false;
        }

        if let Some(sel_client_key) = monitor.sel {
            if let Some(client) = clients.get(sel_client_key) {
                let mask = CONFIG.load().tagmask();

                // Sticky 窗口（在所有标签上）
                if (client.state.tags & mask) == mask {
                    // 策略 A: 直接返回 false
                    // 视觉效果: 状态栏显示当前 Tag 为 "Selected" (通常是亮色)，
                    // 其他 Tag 恢复为 "Occupied" 或 "Empty"。
                    // 这是最符合直觉的，因为 Sticky 窗口是浮在所有 Tag 之上的。
                    return false;
                }

                return (client.state.tags & tag_bit) != 0;
            }
        }

        false
    }

    /// 获取选中客户端的名称
    ///
    /// # 参数
    /// - `clients`: 客户端映射
    /// - `monitor`: 监视器
    ///
    /// # 返回
    /// 客户端名称，如果没有选中则返回空字符串
    pub fn get_selected_client_name(
        clients: &slotmap::SlotMap<ClientKey, WMClient>,
        monitor: &WMMonitor,
    ) -> String {
        if let Some(sel_client_key) = monitor.sel {
            if let Some(client) = clients.get(sel_client_key) {
                return client.name.clone();
            }
        }
        String::new()
    }

    /// 构建监视器的状态栏消息
    ///
    /// # 参数
    /// - `clients`: 客户端映射
    /// - `monitor`: 监视器
    /// - `monitor_clients`: 监视器的客户端列表
    /// - `is_selected_monitor`: 是否是当前选中的监视器
    ///
    /// # 返回
    /// 完整的状态栏消息
    pub fn build_message(
        clients: &slotmap::SlotMap<ClientKey, WMClient>,
        monitor: &WMMonitor,
        monitor_clients: &[ClientKey],
        is_selected_monitor: bool,
    ) -> SharedMessage {
        let mut message = SharedMessage::default();
        let mut monitor_info = MonitorInfo::default();

        // 设置监视器几何信息
        monitor_info.monitor_x = monitor.geometry.w_x;
        monitor_info.monitor_y = monitor.geometry.w_y;
        monitor_info.monitor_width = monitor.geometry.w_w;
        monitor_info.monitor_height = monitor.geometry.w_h;
        monitor_info.monitor_num = monitor.num;
        monitor_info.set_ltsymbol(&monitor.lt_symbol);

        // 计算标签掩码
        let (occupied_tags_mask, urgent_tags_mask) =
            Self::calculate_tag_masks(clients, monitor_clients);

        // 设置每个标签的状态
        for i in 0..CONFIG.load().tags_length() {
            let tag_bit = 1 << i;

            let is_filled = Self::is_filled_tag(clients, monitor, tag_bit, is_selected_monitor);

            let active_tagset = monitor.get_active_tags();
            let is_selected_tag = (active_tagset & tag_bit) != 0;
            let is_urgent_tag = (urgent_tags_mask & tag_bit) != 0;
            let is_occupied_tag = (occupied_tags_mask & tag_bit) != 0;

            let tag_status = TagStatus::new(
                is_selected_tag,
                is_urgent_tag,
                is_filled,
                is_occupied_tag,
            );
            monitor_info.set_tag_status(i, tag_status);
        }

        // 设置选中客户端名称
        let selected_client_name = Self::get_selected_client_name(clients, monitor);
        monitor_info.set_client_name(&selected_client_name);

        message.monitor_info = monitor_info;
        message
    }
}

/// 状态栏更新管理器
pub struct StatusBarUpdateManager {
    /// 待更新的监视器 ID 集合
    pending_updates: std::collections::HashSet<i32>,
}

impl StatusBarUpdateManager {
    pub fn new() -> Self {
        Self {
            pending_updates: std::collections::HashSet::new(),
        }
    }

    /// 标记需要更新状态栏
    ///
    /// # 参数
    /// - `monitor_id`: 监视器 ID，None 表示更新所有监视器
    /// - `monitors`: 监视器映射
    /// - `show_bar`: 是否显示状态栏的函数
    pub fn mark_update_needed<F>(
        &mut self,
        monitor_id: Option<i32>,
        monitors: &slotmap::SlotMap<MonitorKey, WMMonitor>,
        monitor_order: &[MonitorKey],
        show_bar: F,
    ) where
        F: Fn(&WMMonitor) -> bool,
    {
        if let Some(id) = monitor_id {
            // 检查指定监视器是否显示状态栏
            let visible = monitor_order
                .iter()
                .filter_map(|&k| monitors.get(k))
                .any(|m| m.num == id && show_bar(m));

            if visible {
                self.pending_updates.insert(id);
                log::debug!("[StatusBar] Marked monitor {} for update", id);
            }
        } else {
            // 更新所有可见的状态栏
            for &mon_key in monitor_order {
                if let Some(monitor) = monitors.get(mon_key) {
                    if show_bar(monitor) {
                        self.pending_updates.insert(monitor.num);
                        log::debug!("[StatusBar] Marked monitor {} for update", monitor.num);
                    }
                }
            }
        }
    }

    /// 检查是否有待更新的状态栏
    pub fn has_pending_updates(&self) -> bool {
        !self.pending_updates.is_empty()
    }

    /// 获取所有待更新的监视器 ID
    pub fn take_pending_updates(&mut self) -> Vec<i32> {
        self.pending_updates.drain().collect()
    }

    /// 清除所有待更新标记
    pub fn clear(&mut self) {
        self.pending_updates.clear();
    }
}

impl Default for StatusBarUpdateManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::common_define::WindowId;

    fn create_test_client(tags: u32, is_urgent: bool, name: &str) -> WMClient {
        // Use a default WindowId - the actual value doesn't matter for these tests
        let win = unsafe { std::mem::zeroed::<WindowId>() };
        let mut client = WMClient::new(win);
        client.state.tags = tags;
        client.state.is_urgent = is_urgent;
        client.name = name.to_string();
        client
    }

    #[test]
    fn test_calculate_tag_masks_empty() {
        let clients = slotmap::SlotMap::new();
        let monitor_clients = vec![];

        let (occupied, urgent) = StatusBarBuilder::calculate_tag_masks(&clients, &monitor_clients);

        assert_eq!(occupied, 0);
        assert_eq!(urgent, 0);
    }

    #[test]
    fn test_calculate_tag_masks_single_client() {
        let mut clients = slotmap::SlotMap::new();
        let key = clients.insert(create_test_client(0b0001, false, "test"));
        let monitor_clients = vec![key];

        let (occupied, urgent) = StatusBarBuilder::calculate_tag_masks(&clients, &monitor_clients);

        assert_eq!(occupied, 0b0001);
        assert_eq!(urgent, 0);
    }

    #[test]
    fn test_calculate_tag_masks_urgent_client() {
        let mut clients = slotmap::SlotMap::new();
        let key = clients.insert(create_test_client(0b0010, true, "urgent"));
        let monitor_clients = vec![key];

        let (occupied, urgent) = StatusBarBuilder::calculate_tag_masks(&clients, &monitor_clients);

        assert_eq!(occupied, 0b0010);
        assert_eq!(urgent, 0b0010);
    }

    #[test]
    fn test_calculate_tag_masks_multiple_clients() {
        let mut clients = slotmap::SlotMap::new();
        let key1 = clients.insert(create_test_client(0b0001, false, "client1"));
        let key2 = clients.insert(create_test_client(0b0010, true, "client2"));
        let key3 = clients.insert(create_test_client(0b0100, false, "client3"));
        let monitor_clients = vec![key1, key2, key3];

        let (occupied, urgent) = StatusBarBuilder::calculate_tag_masks(&clients, &monitor_clients);

        assert_eq!(occupied, 0b0111);
        assert_eq!(urgent, 0b0010);
    }

    #[test]
    fn test_get_selected_client_name() {
        let mut clients = slotmap::SlotMap::new();
        let key = clients.insert(create_test_client(0b0001, false, "TestWindow"));

        let mut monitor = WMMonitor::new();
        monitor.sel = Some(key);

        let name = StatusBarBuilder::get_selected_client_name(&clients, &monitor);
        assert_eq!(name, "TestWindow");
    }

    #[test]
    fn test_get_selected_client_name_none() {
        let clients = slotmap::SlotMap::new();
        let monitor = WMMonitor::new();

        let name = StatusBarBuilder::get_selected_client_name(&clients, &monitor);
        assert_eq!(name, "");
    }

    #[test]
    fn test_update_manager() {
        let mut manager = StatusBarUpdateManager::new();

        assert!(!manager.has_pending_updates());

        manager.pending_updates.insert(1);
        assert!(manager.has_pending_updates());

        let updates = manager.take_pending_updates();
        assert_eq!(updates.len(), 1);
        assert!(updates.contains(&1));
        assert!(!manager.has_pending_updates());
    }
}
