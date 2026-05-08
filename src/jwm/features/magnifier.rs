//! 放大镜功能 - 跟随鼠标的屏幕放大

/// 放大镜状态
#[derive(Debug, Default, Clone, Copy)]
pub struct MagnifierState {
    /// 放大镜是否启用
    pub enabled: bool,
    /// 放大倍率（默认2倍）
    pub zoom_level: f32,
    /// 放大镜窗口半径（像素）
    pub radius: u32,
}

impl MagnifierState {
    pub fn new() -> Self {
        Self {
            enabled: false,
            zoom_level: 2.0,
            radius: 150,
        }
    }

    /// 切换放大镜开关
    pub fn toggle(&mut self) {
        self.enabled = !self.enabled;
    }

    /// 启用放大镜
    pub fn enable(&mut self) {
        self.enabled = true;
    }

    /// 禁用放大镜
    pub fn disable(&mut self) {
        self.enabled = false;
    }

    /// 设置放大倍率
    pub fn set_zoom_level(&mut self, level: f32) {
        self.zoom_level = level.clamp(1.0, 10.0);
    }

    /// 增加放大倍率
    pub fn zoom_in(&mut self) {
        self.set_zoom_level(self.zoom_level + 0.5);
    }

    /// 减少放大倍率
    pub fn zoom_out(&mut self) {
        self.set_zoom_level(self.zoom_level - 0.5);
    }

    /// 设置放大镜半径
    pub fn set_radius(&mut self, radius: u32) {
        self.radius = radius.clamp(50, 500);
    }

    /// 获取放大镜窗口尺寸（直径）
    pub fn get_window_size(&self) -> u32 {
        self.radius * 2
    }

    /// 计算放大区域的源矩形
    ///
    /// # 参数
    /// - `cursor_x`, `cursor_y`: 鼠标位置
    ///
    /// # 返回
    /// (源区域 x, 源区域 y, 源区域宽度, 源区域高度)
    pub fn get_source_rect(&self, cursor_x: i32, cursor_y: i32) -> (i32, i32, u32, u32) {
        let source_size = (self.radius as f32 / self.zoom_level) as u32;
        let half_size = source_size as i32 / 2;

        let src_x = cursor_x - half_size;
        let src_y = cursor_y - half_size;

        (src_x, src_y, source_size, source_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_magnifier_toggle() {
        let mut mag = MagnifierState::new();
        assert!(!mag.enabled);

        mag.toggle();
        assert!(mag.enabled);

        mag.toggle();
        assert!(!mag.enabled);
    }

    #[test]
    fn test_zoom_levels() {
        let mut mag = MagnifierState::new();
        assert_eq!(mag.zoom_level, 2.0);

        mag.zoom_in();
        assert_eq!(mag.zoom_level, 2.5);

        mag.zoom_in();
        assert_eq!(mag.zoom_level, 3.0);

        mag.zoom_out();
        assert_eq!(mag.zoom_level, 2.5);

        // 测试上限
        mag.set_zoom_level(15.0);
        assert_eq!(mag.zoom_level, 10.0);

        // 测试下限
        mag.set_zoom_level(0.5);
        assert_eq!(mag.zoom_level, 1.0);
    }

    #[test]
    fn test_radius() {
        let mut mag = MagnifierState::new();
        assert_eq!(mag.radius, 150);

        mag.set_radius(200);
        assert_eq!(mag.radius, 200);
        assert_eq!(mag.get_window_size(), 400);

        // 测试上限
        mag.set_radius(1000);
        assert_eq!(mag.radius, 500);

        // 测试下限
        mag.set_radius(10);
        assert_eq!(mag.radius, 50);
    }

    #[test]
    fn test_source_rect() {
        let mut mag = MagnifierState::new();
        mag.set_zoom_level(2.0);
        mag.set_radius(100);

        let (x, y, w, h) = mag.get_source_rect(500, 500);

        // 半径 100, 缩放 2x, 源区域大小应该是 50x50
        assert_eq!(w, 50);
        assert_eq!(h, 50);
        // 中心在 (500, 500), 所以左上角在 (475, 475)
        assert_eq!(x, 475);
        assert_eq!(y, 475);
    }
}
