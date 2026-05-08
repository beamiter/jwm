//! 交互式截图功能 - 区域选择和保存

use crate::core::types::Rect;

/// 截图选择状态
#[derive(Debug, Default, Clone)]
pub struct ScreenshotState {
    /// 截图选择模式是否激活
    pub active: bool,
    /// 是否正在拖动选择区域
    pub dragging: bool,
    /// 选择已完成，等待保存操作
    pub committed: bool,
    /// 选择起始点 (x, y)
    pub start: (f64, f64),
    /// 选择结束点 (x, y)
    pub end: (f64, f64),
    /// 保存路径
    pub output_path: Option<String>,
}

impl ScreenshotState {
    pub fn new() -> Self {
        Self::default()
    }

    /// 开始截图选择模式
    pub fn start(&mut self) {
        self.active = true;
        self.dragging = false;
        self.committed = false;
        self.start = (0.0, 0.0);
        self.end = (0.0, 0.0);
    }

    /// 开始拖动选择
    pub fn begin_drag(&mut self, x: f64, y: f64) {
        self.dragging = true;
        self.start = (x, y);
        self.end = (x, y);
    }

    /// 更新拖动位置
    pub fn update_drag(&mut self, x: f64, y: f64) {
        if self.dragging {
            self.end = (x, y);
        }
    }

    /// 完成选择
    pub fn commit(&mut self) {
        if self.dragging {
            self.dragging = false;
            self.committed = true;
        }
    }

    /// 取消截图
    pub fn cancel(&mut self) {
        *self = Self::default();
    }

    /// 获取选择区域矩形
    pub fn get_selection_rect(&self) -> Option<Rect> {
        if !self.committed && !self.dragging {
            return None;
        }

        let (x1, y1) = self.start;
        let (x2, y2) = self.end;

        let x = x1.min(x2) as i32;
        let y = y1.min(y2) as i32;
        let w = (x1 - x2).abs() as i32;
        let h = (y1 - y2).abs() as i32;

        if w > 0 && h > 0 {
            Some(Rect { x, y, w, h })
        } else {
            None
        }
    }

    /// 设置输出路径
    pub fn set_output_path(&mut self, path: String) {
        self.output_path = Some(path);
    }

    /// 获取输出路径
    pub fn take_output_path(&mut self) -> Option<String> {
        self.output_path.take()
    }

    /// 是否需要渲染选择框
    pub fn should_render_selection(&self) -> bool {
        self.active && (self.dragging || self.committed)
    }

    /// 是否正在选择中
    pub fn is_selecting(&self) -> bool {
        self.active && !self.committed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_screenshot_workflow() {
        let mut state = ScreenshotState::new();

        // 开始截图
        state.start();
        assert!(state.active);
        assert!(!state.committed);

        // 开始拖动
        state.begin_drag(100.0, 100.0);
        assert!(state.dragging);

        // 更新位置
        state.update_drag(200.0, 200.0);
        assert_eq!(state.end, (200.0, 200.0));

        // 完成选择
        state.commit();
        assert!(!state.dragging);
        assert!(state.committed);

        // 获取选择区域
        let rect = state.get_selection_rect().unwrap();
        assert_eq!(rect.x, 100);
        assert_eq!(rect.y, 100);
        assert_eq!(rect.w, 100);
        assert_eq!(rect.h, 100);
    }

    #[test]
    fn test_cancel() {
        let mut state = ScreenshotState::new();
        state.start();
        state.begin_drag(10.0, 10.0);

        state.cancel();
        assert!(!state.active);
        assert!(!state.dragging);
    }

    #[test]
    fn test_empty_selection() {
        let mut state = ScreenshotState::new();
        state.start();
        state.begin_drag(100.0, 100.0);
        state.update_drag(100.0, 100.0); // 同一点
        state.commit();

        // 零尺寸选择应该返回 None
        assert!(state.get_selection_rect().is_none());
    }
}
