// src/core/workspace.rs

use crate::core::models::WMMonitor;

pub struct WorkspaceManager;

impl WorkspaceManager {
    /// 计算 client.state.tags 的变更
    pub fn calculate_new_tags(current_tags: u32, mask: u32, toggle: bool) -> u32 {
        if toggle { current_tags ^ mask } else { mask }
    }

    /// 检查 target_tag 是否已经是当前 Monitor 的激活 Tag
    pub fn is_same_tag(monitor: &WMMonitor, target_tag_mask: u32) -> bool {
        monitor.tag_set[monitor.sel_tags] == target_tag_mask
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::models::WMMonitor;

    // -----------------------------------------------------------------------
    // calculate_new_tags
    // -----------------------------------------------------------------------

    #[test]
    fn test_calculate_new_tags_set_no_toggle() {
        // toggle=false → just return mask
        assert_eq!(WorkspaceManager::calculate_new_tags(0b0101, 0b0010, false), 0b0010);
    }

    #[test]
    fn test_calculate_new_tags_toggle_on() {
        // current has bit 0; toggle mask bit 1 → XOR sets bit 1
        assert_eq!(WorkspaceManager::calculate_new_tags(0b01, 0b10, true), 0b11);
    }

    #[test]
    fn test_calculate_new_tags_toggle_off() {
        // current has bit 1; toggling bit 1 → XOR clears it
        assert_eq!(WorkspaceManager::calculate_new_tags(0b11, 0b10, true), 0b01);
    }

    #[test]
    fn test_calculate_new_tags_toggle_no_op_on_zero() {
        // XOR with 0 leaves current unchanged
        assert_eq!(WorkspaceManager::calculate_new_tags(0b1010, 0, true), 0b1010);
    }

    #[test]
    fn test_calculate_new_tags_set_replaces_all() {
        // no toggle: mask fully replaces current
        assert_eq!(WorkspaceManager::calculate_new_tags(0xFF, 0x01, false), 0x01);
    }

    // -----------------------------------------------------------------------
    // is_same_tag
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_same_tag_matching() {
        let mut mon = WMMonitor::new();
        mon.tag_set[mon.sel_tags] = 0b0001;
        assert!(WorkspaceManager::is_same_tag(&mon, 0b0001));
    }

    #[test]
    fn test_is_same_tag_not_matching() {
        let mut mon = WMMonitor::new();
        mon.tag_set[mon.sel_tags] = 0b0001;
        assert!(!WorkspaceManager::is_same_tag(&mon, 0b0010));
    }

    #[test]
    fn test_is_same_tag_uses_sel_tags_index() {
        let mut mon = WMMonitor::new();
        mon.tag_set[0] = 0b0001;
        mon.tag_set[1] = 0b1000;
        mon.sel_tags = 1;
        assert!(WorkspaceManager::is_same_tag(&mon, 0b1000));
        assert!(!WorkspaceManager::is_same_tag(&mon, 0b0001));
    }

    #[test]
    fn test_is_same_tag_zero_mask() {
        let mut mon = WMMonitor::new();
        mon.tag_set[mon.sel_tags] = 0;
        assert!(WorkspaceManager::is_same_tag(&mon, 0));
    }
}
