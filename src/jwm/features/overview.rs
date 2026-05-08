//! Overview 模式 - Alt+Ctrl+Tab 3D 棱镜轮播视图

use crate::core::models::ClientKey;

/// Overview 模式状态（3D 窗口切换器）
#[derive(Debug, Default, Clone)]
pub struct OverviewState {
    /// Overview 模式是否激活
    pub active: bool,
    /// 当前选中的窗口索引
    pub index: usize,
    /// 参与 overview 的客户端列表
    pub clients: Vec<ClientKey>,
    /// 滑动窗口偏移（用于超过6个客户端时）
    pub slide_offset: usize,
}

impl OverviewState {
    pub fn new() -> Self {
        Self::default()
    }

    /// 激活 overview 模式
    pub fn activate(&mut self, clients: Vec<ClientKey>) {
        self.active = true;
        self.clients = clients;
        self.index = 0;
        self.slide_offset = 0;
    }

    /// 退出 overview 模式
    pub fn deactivate(&mut self) {
        self.active = false;
        self.clients.clear();
        self.index = 0;
        self.slide_offset = 0;
    }

    /// 切换到下一个窗口
    pub fn next(&mut self) {
        if self.clients.is_empty() {
            return;
        }

        self.index = (self.index + 1) % self.clients.len();
        self.adjust_slide_offset();
    }

    /// 切换到上一个窗口
    pub fn previous(&mut self) {
        if self.clients.is_empty() {
            return;
        }

        if self.index == 0 {
            self.index = self.clients.len() - 1;
        } else {
            self.index -= 1;
        }
        self.adjust_slide_offset();
    }

    /// 获取当前选中的客户端
    pub fn get_selected_client(&self) -> Option<ClientKey> {
        self.clients.get(self.index).copied()
    }

    /// 获取当前窗口数量
    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    /// 获取可见窗口范围（用于棱镜渲染）
    /// 最多显示 6 个窗口，当前选中的在中间
    pub fn get_visible_range(&self) -> (usize, usize) {
        const MAX_VISIBLE: usize = 6;
        let count = self.clients.len();

        if count <= MAX_VISIBLE {
            (0, count)
        } else {
            let half = MAX_VISIBLE / 2;
            let mut start = self.index.saturating_sub(half);
            let end = (start + MAX_VISIBLE).min(count);

            // 调整边界
            if end == count {
                start = count.saturating_sub(MAX_VISIBLE);
            }

            (start, end)
        }
    }

    /// 调整滑动偏移以保持当前选中窗口可见
    fn adjust_slide_offset(&mut self) {
        const MAX_VISIBLE: usize = 6;
        let count = self.clients.len();

        if count <= MAX_VISIBLE {
            self.slide_offset = 0;
            return;
        }

        let half = MAX_VISIBLE / 2;

        // 确保当前索引在可见范围内
        if self.index < self.slide_offset + half {
            self.slide_offset = self.index.saturating_sub(half);
        } else if self.index >= self.slide_offset + half {
            self.slide_offset = (self.index - half).min(count - MAX_VISIBLE);
        }
    }

    /// 跳转到指定索引
    pub fn jump_to(&mut self, index: usize) {
        if index < self.clients.len() {
            self.index = index;
            self.adjust_slide_offset();
        }
    }

    /// 获取客户端在可见范围内的相对位置
    pub fn get_relative_position(&self, client_key: ClientKey) -> Option<usize> {
        self.clients
            .iter()
            .position(|&c| c == client_key)
            .and_then(|pos| {
                let (start, end) = self.get_visible_range();
                if pos >= start && pos < end {
                    Some(pos - start)
                } else {
                    None
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_keys(count: usize) -> Vec<ClientKey> {
        use slotmap::SlotMap;
        let mut map = SlotMap::new();
        (0..count).map(|_| map.insert(())).collect()
    }

    #[test]
    fn test_overview_navigation() {
        let mut state = OverviewState::new();
        let clients = create_test_keys(5);

        state.activate(clients.clone());
        assert!(state.active);
        assert_eq!(state.client_count(), 5);
        assert_eq!(state.index, 0);

        // 向前导航
        state.next();
        assert_eq!(state.index, 1);

        state.next();
        assert_eq!(state.index, 2);

        // 向后导航
        state.previous();
        assert_eq!(state.index, 1);

        // 循环
        state.jump_to(4);
        state.next();
        assert_eq!(state.index, 0);
    }

    #[test]
    fn test_visible_range_small() {
        let mut state = OverviewState::new();
        let clients = create_test_keys(4);

        state.activate(clients);
        let (start, end) = state.get_visible_range();
        assert_eq!(start, 0);
        assert_eq!(end, 4);
    }

    #[test]
    fn test_visible_range_large() {
        let mut state = OverviewState::new();
        let clients = create_test_keys(10);

        state.activate(clients);

        // 初始位置
        let (start, end) = state.get_visible_range();
        assert_eq!(end - start, 6);

        // 跳到中间
        state.jump_to(5);
        let (start, end) = state.get_visible_range();
        assert_eq!(end - start, 6);
        assert!(start <= 5 && 5 < end);
    }

    #[test]
    fn test_deactivate() {
        let mut state = OverviewState::new();
        let clients = create_test_keys(3);

        state.activate(clients);
        state.next();

        state.deactivate();
        assert!(!state.active);
        assert_eq!(state.client_count(), 0);
        assert_eq!(state.index, 0);
    }
}
