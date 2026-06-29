//! 几何计算和窗口尺寸约束
//!
//! 此模块处理窗口的几何约束、尺寸提示和位置调整。

use crate::core::models::{MonitorGeometry, SizeHints};
use crate::core::types::Rect;

/// 几何约束工具 - 纯函数集合
pub struct GeometryConstraints;

impl GeometryConstraints {
    /// 约束坐标到屏幕范围内
    ///
    /// # 参数
    /// - `x`, `y`: 要约束的坐标（会被修改）
    /// - `total_width`, `total_height`: 窗口总尺寸（包括边框）
    /// - `screen_w`, `screen_h`: 屏幕尺寸
    pub fn constrain_to_screen(
        x: &mut i32,
        y: &mut i32,
        total_width: i32,
        total_height: i32,
        screen_w: i32,
        screen_h: i32,
    ) {
        let min_x = -(total_width - 1);
        let max_x = screen_w - 1;
        if min_x <= max_x {
            *x = (*x).clamp(min_x, max_x);
        } else {
            log::warn!(
                "Skip screen X clamp because max_x({}) < min_x({}); total_width={}, screen_w={}",
                max_x,
                min_x,
                total_width,
                screen_w
            );
            *x = min_x;
        }

        let min_y = -(total_height - 1);
        let max_y = screen_h - 1;
        if min_y <= max_y {
            *y = (*y).clamp(min_y, max_y);
        } else {
            log::warn!(
                "Skip screen Y clamp because max_y({}) < min_y({}); total_height={}, screen_h={}",
                max_y,
                min_y,
                total_height,
                screen_h
            );
            *y = min_y;
        }
    }

    /// 约束坐标到监视器工作区范围内
    ///
    /// # 参数
    /// - `x`, `y`: 要约束的坐标（会被修改）
    /// - `total_width`, `total_height`: 窗口总尺寸（包括边框）
    /// - `monitor_geometry`: 监视器几何信息
    pub fn constrain_to_monitor(
        x: &mut i32,
        y: &mut i32,
        total_width: i32,
        total_height: i32,
        monitor_geometry: &MonitorGeometry,
    ) {
        let MonitorGeometry {
            w_x: wx,
            w_y: wy,
            w_w: ww,
            w_h: wh,
            ..
        } = *monitor_geometry;

        let min_x = wx - total_width + 1;
        let max_x = wx + ww - 1;
        if min_x <= max_x {
            *x = (*x).clamp(min_x, max_x);
        } else {
            log::warn!(
                "Skip monitor X clamp because max_x({}) < min_x({}); total_width={}, monitor_ww={}",
                max_x,
                min_x,
                total_width,
                ww
            );
            *x = min_x;
        }

        let min_y = wy - total_height + 1;
        let max_y = wy + wh - 1;
        if min_y <= max_y {
            *y = (*y).clamp(min_y, max_y);
        } else {
            log::warn!(
                "Skip monitor Y clamp because max_y({}) < min_y({}); total_height={}, monitor_wh={}",
                max_y,
                min_y,
                total_height,
                wh
            );
            *y = min_y;
        }
    }

    /// 应用增量约束（用于终端等按字符调整大小的窗口）
    ///
    /// # 参数
    /// - `size`: 原始尺寸
    /// - `increment`: 增量步长
    ///
    /// # 返回
    /// 调整后的尺寸（增量的整数倍）
    pub fn apply_increments(size: i32, increment: i32) -> i32 {
        if increment > 0 {
            (size / increment) * increment
        } else {
            size
        }
    }

    /// 应用宽高比约束
    ///
    /// # 参数
    /// - `w`, `h`: 原始宽高
    /// - `hints`: 尺寸提示（包含最小/最大宽高比）
    ///
    /// # 返回
    /// 调整后的 (宽度, 高度)
    pub fn apply_aspect_ratio_constraints(mut w: i32, mut h: i32, hints: &SizeHints) -> (i32, i32) {
        if hints.min_aspect > 0.0 && hints.max_aspect > 0.0 {
            let ratio = w as f32 / h as f32;
            if ratio < hints.min_aspect {
                w = (h as f32 * hints.min_aspect + 0.5) as i32;
            } else if ratio > hints.max_aspect {
                h = (w as f32 / hints.max_aspect + 0.5) as i32;
            }
        }
        (w, h)
    }

    /// 计算完全约束后的尺寸
    ///
    /// 按顺序应用：
    /// 1. 增量约束（inc_w, inc_h）
    /// 2. 宽高比约束（min/max aspect）
    /// 3. 最小/最大尺寸约束
    ///
    /// # 参数
    /// - `w`, `h`: 原始宽高
    /// - `hints`: 尺寸提示
    ///
    /// # 返回
    /// 完全约束后的 (宽度, 高度)
    pub fn calculate_constrained_size(mut w: i32, mut h: i32, hints: &SizeHints) -> (i32, i32) {
        // 应用增量约束
        w = Self::apply_increments(w - hints.base_w, hints.inc_w) + hints.base_w;
        h = Self::apply_increments(h - hints.base_h, hints.inc_h) + hints.base_h;

        // 应用宽高比约束
        (w, h) = Self::apply_aspect_ratio_constraints(w, h, hints);

        // 应用最小尺寸约束
        w = w.max(hints.min_w);
        h = h.max(hints.min_h);

        // 应用最大尺寸约束
        if hints.max_w > 0 {
            w = w.min(hints.max_w);
        }
        if hints.max_h > 0 {
            h = h.min(hints.max_h);
        }

        (w, h)
    }

    /// 约束矩形到边界内
    ///
    /// # 参数
    /// - `x`, `y`: 矩形左上角坐标（会被修改）
    /// - `width`, `height`: 矩形尺寸
    /// - `boundary`: 边界矩形
    pub fn clamp_rect_to_boundary(
        x: &mut i32,
        y: &mut i32,
        width: i32,
        height: i32,
        boundary: &Rect,
    ) {
        let min_x = boundary.x;
        let max_x = boundary.x + boundary.w - width;
        if min_x <= max_x {
            *x = (*x).clamp(min_x, max_x);
        } else {
            *x = min_x;
            log::warn!(
                "Skip X clamp because max_x({}) < min_x({}); width={}, boundary.w={}",
                max_x,
                min_x,
                width,
                boundary.w
            );
        }

        let min_y = boundary.y;
        let max_y = boundary.y + boundary.h - height;
        if min_y <= max_y {
            *y = (*y).clamp(min_y, max_y);
        } else {
            *y = min_y;
            log::warn!(
                "Skip Y clamp because max_y({}) < min_y({}); height={}, boundary.h={}",
                max_y,
                min_y,
                height,
                boundary.h
            );
        }
    }

    /// 检查窗口是否覆盖整个监视器
    ///
    /// # 参数
    /// - `window_rect`: 窗口矩形（包括边框）
    /// - `monitor_rect`: 监视器矩形
    ///
    /// # 返回
    /// 如果窗口完全覆盖监视器则返回 true
    pub fn covers_full_monitor(window_rect: &Rect, monitor_rect: &Rect) -> bool {
        window_rect.x <= monitor_rect.x
            && window_rect.y <= monitor_rect.y
            && window_rect.w >= monitor_rect.w
            && window_rect.h >= monitor_rect.h
    }

    /// 计算两个矩形的交集
    ///
    /// # 参数
    /// - `rect1`, `rect2`: 两个矩形
    ///
    /// # 返回
    /// 交集矩形，如果没有交集则返回 None
    pub fn rect_intersection(rect1: &Rect, rect2: &Rect) -> Option<Rect> {
        let left = rect1.x.max(rect2.x);
        let top = rect1.y.max(rect2.y);
        let right = (rect1.x + rect1.w).min(rect2.x + rect2.w);
        let bottom = (rect1.y + rect1.h).min(rect2.y + rect2.h);

        let w = (right - left).max(0);
        let h = (bottom - top).max(0);

        if w > 0 && h > 0 {
            Some(Rect::new(left, top, w, h))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constrain_to_screen() {
        let mut x = -100;
        let mut y = -50;
        GeometryConstraints::constrain_to_screen(&mut x, &mut y, 200, 100, 1920, 1080);
        // min_x = -(200-1) = -199, max_x = 1920-1 = 1919
        // x=-100 在范围内，保持不变
        assert_eq!(x, -100);
        assert_eq!(y, -50);

        let mut x = -300;
        let mut y = 2000;
        GeometryConstraints::constrain_to_screen(&mut x, &mut y, 200, 100, 1920, 1080);
        // x=-300 < -199, 应该被约束到 -199
        // y=2000 > 1079, 应该被约束到 1079
        assert_eq!(x, -199);
        assert_eq!(y, 1079);
    }

    #[test]
    fn test_apply_increments() {
        assert_eq!(GeometryConstraints::apply_increments(100, 10), 100);
        assert_eq!(GeometryConstraints::apply_increments(105, 10), 100);
        assert_eq!(GeometryConstraints::apply_increments(99, 10), 90);
        assert_eq!(GeometryConstraints::apply_increments(100, 0), 100);
        assert_eq!(GeometryConstraints::apply_increments(100, -5), 100);
    }

    #[test]
    fn test_apply_aspect_ratio() {
        let hints = SizeHints {
            min_aspect: 1.5,
            max_aspect: 2.0,
            ..Default::default()
        };

        // 比例太小 (100/100 = 1.0 < 1.5)，应该增加宽度
        let (w, h) = GeometryConstraints::apply_aspect_ratio_constraints(100, 100, &hints);
        assert_eq!(w, 150); // 100 * 1.5
        assert_eq!(h, 100);

        // 比例太大 (200/50 = 4.0 > 2.0)，应该增加高度
        let (w, h) = GeometryConstraints::apply_aspect_ratio_constraints(200, 50, &hints);
        assert_eq!(w, 200);
        assert_eq!(h, 100); // 200 / 2.0

        // 比例在范围内，保持不变
        let (w, h) = GeometryConstraints::apply_aspect_ratio_constraints(180, 100, &hints);
        assert_eq!(w, 180);
        assert_eq!(h, 100);
    }

    #[test]
    fn test_calculate_constrained_size() {
        let hints = SizeHints {
            base_w: 10,
            base_h: 10,
            inc_w: 8,
            inc_h: 16,
            min_w: 100,
            min_h: 100,
            max_w: 800,
            max_h: 600,
            ..Default::default()
        };

        // 测试增量约束：宽度应该是 base_w + n*inc_w
        let (w, h) = GeometryConstraints::calculate_constrained_size(200, 200, &hints);
        // w: (200-10)/8*8 + 10 = 190/8*8 + 10 = 23*8 + 10 = 184 + 10 = 194
        // h: (200-10)/16*16 + 10 = 190/16*16 + 10 = 11*16 + 10 = 176 + 10 = 186
        assert_eq!(w, 194);
        assert_eq!(h, 186);

        // 测试最小尺寸约束
        let (w, h) = GeometryConstraints::calculate_constrained_size(50, 50, &hints);
        assert_eq!(w, 100); // 被最小值约束
        assert_eq!(h, 100);

        // 测试最大尺寸约束
        let (w, h) = GeometryConstraints::calculate_constrained_size(1000, 1000, &hints);
        assert_eq!(w, 800); // 被最大值约束
        assert_eq!(h, 600);
    }

    #[test]
    fn test_covers_full_monitor() {
        let monitor = Rect::new(0, 0, 1920, 1080);

        // 完全覆盖
        let window = Rect::new(0, 0, 1920, 1080);
        assert!(GeometryConstraints::covers_full_monitor(&window, &monitor));

        // 更大的窗口也算覆盖
        let window = Rect::new(-10, -10, 2000, 1200);
        assert!(GeometryConstraints::covers_full_monitor(&window, &monitor));

        // 稍小一点就不算
        let window = Rect::new(0, 0, 1900, 1080);
        assert!(!GeometryConstraints::covers_full_monitor(&window, &monitor));
    }

    #[test]
    fn test_rect_intersection() {
        let rect1 = Rect::new(0, 0, 100, 100);
        let rect2 = Rect::new(50, 50, 100, 100);

        // 有交集
        let intersection = GeometryConstraints::rect_intersection(&rect1, &rect2).unwrap();
        assert_eq!(intersection.x, 50);
        assert_eq!(intersection.y, 50);
        assert_eq!(intersection.w, 50);
        assert_eq!(intersection.h, 50);

        // 无交集
        let rect3 = Rect::new(200, 200, 100, 100);
        assert!(GeometryConstraints::rect_intersection(&rect1, &rect3).is_none());

        // 边缘相接（无交集）
        let rect4 = Rect::new(100, 0, 100, 100);
        assert!(GeometryConstraints::rect_intersection(&rect1, &rect4).is_none());
    }

    #[test]
    fn test_clamp_rect_to_boundary() {
        let boundary = Rect::new(100, 100, 800, 600);
        let mut x = 50;
        let mut y = 50;

        GeometryConstraints::clamp_rect_to_boundary(&mut x, &mut y, 200, 150, &boundary);

        // x 应该被约束到 boundary.x (100)
        // y 应该被约束到 boundary.y (100)
        assert_eq!(x, 100);
        assert_eq!(y, 100);

        // 测试右下角溢出
        let mut x = 1000;
        let mut y = 800;
        GeometryConstraints::clamp_rect_to_boundary(&mut x, &mut y, 200, 150, &boundary);

        // x 应该被约束到 boundary.x + boundary.w - width = 100 + 800 - 200 = 700
        // y 应该被约束到 boundary.y + boundary.h - height = 100 + 600 - 150 = 550
        assert_eq!(x, 700);
        assert_eq!(y, 550);
    }
}
