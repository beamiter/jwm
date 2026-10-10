//! 几何计算和窗口尺寸约束
//!
//! 此模块处理窗口的几何约束、尺寸提示和位置调整。

use crate::core::models::{MonitorGeometry, SizeHints};
use crate::core::types::Rect;

/// 约束后的边长下限：一个窗口至少要有一个像素。
pub const MIN_CONSTRAINED_DIMENSION: i32 = 1;

/// 约束后的边长上限。
///
/// X11 `ConfigureWindow` 的 width/height 是 CARD16，比这更宽的窗口服务器
/// 根本配置不了；Wayland 侧的缓冲区只会更小。上限放在这里而不是只放在
/// hint 解析器里，是因为 [`SizeHints`] 是普通数据——任何 backend、任何
/// 测试都能直接把 `i32::MAX` 填进去，而结果的每一个下游消费者
/// （`total_width()`、configure 编码）做的都是朴素的 `i32` 运算。
pub const MAX_CONSTRAINED_DIMENSION: i32 = u16::MAX as i32;

/// 把任意中间量收进可配置的边长区间。
fn clamp_dimension(value: i64) -> i32 {
    value.clamp(
        i64::from(MIN_CONSTRAINED_DIMENSION),
        i64::from(MAX_CONSTRAINED_DIMENSION),
    ) as i32
}

/// 增量对齐的唯一实现：把 `offset` 落到 `increment` 的整数倍（向零取整）。
/// 分母为正，所以结果的绝对值不会超过 `offset`。
fn aligned_offset(offset: i64, increment: i64) -> i64 {
    if increment > 0 {
        offset / increment * increment
    } else {
        offset
    }
}

/// 以 `base` 为原点做增量对齐，并把结果收进可配置区间。
///
/// `base`/`increment` 都是客户端写的 hint，`size - base` 与回加 `base`
/// 在 `i32` 上都会溢出（debug 下 panic，release 下回绕成负数几何），
/// 所以中间量一律走 `i64`。
fn increment_aligned_dimension(size: i32, base: i32, increment: i32) -> i32 {
    let base = i64::from(base);
    let aligned = aligned_offset(i64::from(size) - base, i64::from(increment));
    clamp_dimension(aligned + base)
}

// Keep boundary arithmetic wide until the final representable coordinate.
// Both incoming configure sizes and global output origins are signed i32.
fn constrain_axis(position: &mut i32, origin: i32, span: i32, total: i64) {
    let lower = i64::from(origin) - total.max(1) + 1;
    let upper = i64::from(origin) + i64::from(span) - 1;
    let constrained = if lower <= upper {
        i64::from(*position).clamp(lower, upper)
    } else {
        lower
    };
    *position = constrained.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
}

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
        Self::constrain_to_screen_wide(
            x,
            y,
            i64::from(total_width),
            i64::from(total_height),
            screen_w,
            screen_h,
        );
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
        Self::constrain_to_monitor_wide(
            x,
            y,
            i64::from(total_width),
            i64::from(total_height),
            monitor_geometry,
        );
    }

    pub(crate) fn constrain_to_screen_wide(
        x: &mut i32,
        y: &mut i32,
        total_width: i64,
        total_height: i64,
        screen_w: i32,
        screen_h: i32,
    ) {
        constrain_axis(x, 0, screen_w, total_width);
        constrain_axis(y, 0, screen_h, total_height);
    }

    pub(crate) fn constrain_to_monitor_wide(
        x: &mut i32,
        y: &mut i32,
        total_width: i64,
        total_height: i64,
        monitor_geometry: &MonitorGeometry,
    ) {
        constrain_axis(x, monitor_geometry.w_x, monitor_geometry.w_w, total_width);
        constrain_axis(y, monitor_geometry.w_y, monitor_geometry.w_h, total_height);
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
        aligned_offset(i64::from(size), i64::from(increment)) as i32
    }

    /// 应用宽高比约束
    ///
    /// The offered `w`×`h` box is a ceiling, not a suggestion: a tiled cell
    /// or an interactive resize hands over the space the window may use.
    /// So an out-of-range ratio is corrected by *shrinking* the dimension
    /// that is too long (as dwm's `applysizehints` and ICCCM 4.1.2.3 do),
    /// never by growing the other one — growing configured a 16:9 client
    /// in a 900×1000 stack cell 1778 px wide, over its neighbour and off
    /// the monitor.
    ///
    /// # 参数
    /// - `w`, `h`: 原始宽高
    /// - `hints`: 尺寸提示（包含最小/最大宽高比）; both aspects are width/height
    ///
    /// # 返回
    /// 调整后的 (宽度, 高度); neither side exceeds its input
    pub fn apply_aspect_ratio_constraints(mut w: i32, mut h: i32, hints: &SizeHints) -> (i32, i32) {
        // A non-positive side has no ratio to correct (and `w / h` would
        // divide by zero); the dimension clamp downstream owns that case.
        if hints.min_aspect > 0.0 && hints.max_aspect > 0.0 && w > 0 && h > 0 {
            let ratio = w as f32 / h as f32;
            if ratio < hints.min_aspect {
                // Too tall for the narrowest allowed ratio: keep the width,
                // cut the height. `w / min_aspect < h` here, so rounding
                // cannot push the result past the offered height.
                h = (w as f32 / hints.min_aspect + 0.5) as i32;
            } else if ratio > hints.max_aspect {
                // Too wide for the widest allowed ratio: keep the height,
                // cut the width.
                w = (h as f32 * hints.max_aspect + 0.5) as i32;
            }
        }
        (w, h)
    }

    /// 计算完全约束后的尺寸
    ///
    /// 按顺序应用 (the same order as dwm's `applysizehints`)：
    /// 1. 宽高比约束（min/max aspect）, which only shrinks
    /// 2. 增量约束（inc_w, inc_h）, rounded toward the base, also only shrinking
    /// 3. 最小/最大尺寸约束
    ///
    /// Aspect runs before the increments so the side it shortens still
    /// lands on an increment step (a terminal keeps whole rows); both steps
    /// only shrink, so the result stays inside the offered box unless the
    /// client's own minimum asks for more.
    ///
    /// # 参数
    /// - `w`, `h`: 原始宽高
    /// - `hints`: 尺寸提示
    ///
    /// # 返回
    /// 完全约束后的 (宽度, 高度)
    pub fn calculate_constrained_size(w: i32, h: i32, hints: &SizeHints) -> (i32, i32) {
        // Aspect first. It only shortens the side that is too long, and
        // `as i32` saturates instead of wrapping, so even an absurd ratio
        // cannot make the result larger than the input.
        let (w, h) = Self::apply_aspect_ratio_constraints(w, h, hints);

        // Increments, clamped into the configurable band (intermediates are
        // i64, see `increment_aligned_dimension`).
        let mut w = increment_aligned_dimension(w, hints.base_w, hints.inc_w);
        let mut h = increment_aligned_dimension(h, hints.base_h, hints.inc_h);

        // 应用最小尺寸约束。min 也先收进区间：`min_w = i32::MAX` 表达的是
        // 「越大越好」，不是要一个服务器配置不了的窗口。
        w = w.max(clamp_dimension(i64::from(hints.min_w)));
        h = h.max(clamp_dimension(i64::from(hints.min_h)));

        // 应用最大尺寸约束（<= 0 仍然表示「没有上限」）
        if hints.max_w > 0 {
            w = w.min(clamp_dimension(i64::from(hints.max_w)));
        }
        if hints.max_h > 0 {
            h = h.min(clamp_dimension(i64::from(hints.max_h)));
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
        let min_x = i64::from(boundary.x);
        let max_x = min_x + i64::from(boundary.w) - i64::from(width);
        if min_x <= max_x {
            *x = i64::from(*x).clamp(min_x, max_x) as i32;
        } else {
            *x = boundary.x;
            log::warn!(
                "Skip X clamp because max_x({}) < min_x({}); width={}, boundary.w={}",
                max_x,
                min_x,
                width,
                boundary.w
            );
        }

        let min_y = i64::from(boundary.y);
        let max_y = min_y + i64::from(boundary.h) - i64::from(height);
        if min_y <= max_y {
            *y = i64::from(*y).clamp(min_y, max_y) as i32;
        } else {
            *y = boundary.y;
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
            && i64::from(window_rect.x) + i64::from(window_rect.w)
                >= i64::from(monitor_rect.x) + i64::from(monitor_rect.w)
            && i64::from(window_rect.y) + i64::from(window_rect.h)
                >= i64::from(monitor_rect.y) + i64::from(monitor_rect.h)
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
        let right =
            (i64::from(rect1.x) + i64::from(rect1.w)).min(i64::from(rect2.x) + i64::from(rect2.w));
        let bottom =
            (i64::from(rect1.y) + i64::from(rect1.h)).min(i64::from(rect2.y) + i64::from(rect2.h));

        let w = right - i64::from(left);
        let h = bottom - i64::from(top);

        if w > 0 && h > 0 {
            // A positive intersection is no larger than either input extent.
            Some(Rect::new(left, top, w as i32, h as i32))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundary_constraints_handle_large_borders_and_output_origins() {
        let total = i64::from(i32::MAX) + 2 * i64::from(i32::MAX);
        let mut coordinate = i32::MIN;
        constrain_axis(&mut coordinate, 0, 1920, total);
        assert_eq!(coordinate, i32::MIN);
        coordinate = i32::MAX;
        constrain_axis(&mut coordinate, 0, 1920, total);
        assert_eq!(coordinate, 1919);

        coordinate = i32::MAX;
        constrain_axis(&mut coordinate, i32::MAX - 100, 1920, 200);
        assert_eq!(coordinate, i32::MAX);
        coordinate = i32::MIN;
        constrain_axis(&mut coordinate, i32::MIN + 100, 1920, total);
        assert_eq!(coordinate, i32::MIN);
    }

    #[test]
    fn boundary_constraints_keep_one_pixel_visible_at_each_edge() {
        let mut coordinate = -200;
        constrain_axis(&mut coordinate, 100, 1920, 200);
        assert_eq!(coordinate, -99);
        coordinate = 2500;
        constrain_axis(&mut coordinate, 100, 1920, 200);
        assert_eq!(coordinate, 2019);
        coordinate = 500;
        constrain_axis(&mut coordinate, 100, 1920, 200);
        assert_eq!(coordinate, 500);
    }

    #[test]
    fn monitor_boundary_entry_points_share_normal_and_extreme_behavior() {
        for (origin, span, total, input, expected) in [
            (100, 1920, 200, -200, -99),
            (100, 1920, 200, 2500, 2019),
            (i32::MAX - 100, 1920, 200, i32::MAX, i32::MAX),
            (i32::MIN + 100, 1920, i32::MAX, i32::MIN, i32::MIN),
        ] {
            let geometry = MonitorGeometry {
                w_x: origin,
                w_y: origin,
                w_w: span,
                w_h: span,
                ..MonitorGeometry::default()
            };
            let (mut x, mut y) = (input, input);
            GeometryConstraints::constrain_to_monitor(&mut x, &mut y, total, total, &geometry);
            assert_eq!((x, y), (expected, expected));
            let (mut wide_x, mut wide_y) = (input, input);
            GeometryConstraints::constrain_to_monitor_wide(
                &mut wide_x,
                &mut wide_y,
                i64::from(total),
                i64::from(total),
                &geometry,
            );
            assert_eq!((wide_x, wide_y), (x, y));
        }
    }

    #[test]
    fn screen_boundary_wide_entry_accepts_full_border_extent() {
        let total = 3 * i64::from(i32::MAX);
        let (mut x, mut y) = (i32::MIN, i32::MAX);
        GeometryConstraints::constrain_to_screen_wide(&mut x, &mut y, total, total, 1920, 1080);
        assert_eq!((x, y), (i32::MIN, 1079));
    }

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

        // Ratio too small (100/100 = 1.0 < 1.5): keep the width, cut the height.
        let (w, h) = GeometryConstraints::apply_aspect_ratio_constraints(100, 100, &hints);
        assert_eq!(w, 100);
        assert_eq!(h, 67); // 100 / 1.5, rounded

        // Ratio too large (200/50 = 4.0 > 2.0): keep the height, cut the width.
        let (w, h) = GeometryConstraints::apply_aspect_ratio_constraints(200, 50, &hints);
        assert_eq!(w, 100); // 50 * 2.0
        assert_eq!(h, 50);

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

    /// `SizeHints` 是普通数据：X11 的 hint 解析器现在把每个词收进了
    /// CARD16，但 `calculate_constrained_size` 不能靠「唯一的调用者很小心」
    /// 活着。这里直接注入敌意 hint——`base_w = i32::MIN` 让 `w - base_w`
    /// 在 i32 上溢出（debug 下 panic），`min_w = i32::MAX` 和 40 亿的宽高比
    /// 则会把结果顶到 `i32::MAX`——断言结果始终落在服务器配置得了的区间。
    #[test]
    fn hostile_size_hints_stay_inside_the_configurable_band() {
        let hostile = [
            SizeHints {
                min_w: i32::MAX,
                base_w: i32::MIN,
                min_aspect: 4e9,
                max_aspect: 4e9,
                ..Default::default()
            },
            SizeHints {
                base_w: i32::MIN,
                base_h: i32::MIN,
                inc_w: i32::MAX,
                inc_h: i32::MAX,
                min_w: i32::MAX,
                min_h: i32::MAX,
                max_w: i32::MIN,
                max_h: i32::MIN,
                min_aspect: f32::MAX,
                max_aspect: f32::MIN_POSITIVE,
                hints_valid: true,
            },
            SizeHints {
                base_w: i32::MAX,
                base_h: i32::MAX,
                inc_w: 1,
                inc_h: 1,
                max_w: i32::MAX,
                max_h: i32::MAX,
                min_aspect: f32::MIN_POSITIVE,
                max_aspect: f32::MAX,
                ..Default::default()
            },
            SizeHints {
                base_w: i32::MIN,
                base_h: i32::MAX,
                inc_w: -1,
                inc_h: i32::MIN,
                min_w: i32::MIN,
                min_h: i32::MIN,
                min_aspect: f32::NAN,
                max_aspect: f32::INFINITY,
                ..Default::default()
            },
        ];

        let band = MIN_CONSTRAINED_DIMENSION..=MAX_CONSTRAINED_DIMENSION;
        for hints in &hostile {
            for (w, h) in [(800, 600), (1, 1), (i32::MIN, i32::MAX), (0, 0)] {
                let (cw, ch) = GeometryConstraints::calculate_constrained_size(w, h, hints);
                assert!(band.contains(&cw), "width {cw} escaped for {hints:?}");
                assert!(band.contains(&ch), "height {ch} escaped for {hints:?}");
            }
        }
    }

    /// 增量对齐本身也要扛住 `i32::MIN`：`(size / inc) * inc` 只在分母为正
    /// 时安全，而 `size` 是可以到达边界的。
    #[test]
    fn increment_alignment_survives_the_i32_extremes() {
        for size in [i32::MIN, -1, 0, 1, i32::MAX] {
            for increment in [i32::MIN, -1, 0, 1, 7, i32::MAX] {
                let aligned = GeometryConstraints::apply_increments(size, increment);
                if increment > 0 {
                    assert_eq!(aligned % increment, 0, "size={size} inc={increment}");
                    assert!(aligned.unsigned_abs() <= size.unsigned_abs());
                } else {
                    assert_eq!(aligned, size, "size={size} inc={increment}");
                }
            }
        }
    }

    #[test]
    fn full_monitor_coverage_requires_both_far_edges() {
        for monitor in [
            Rect::new(0, 0, 1920, 1080),
            Rect::new(1920, 200, 1920, 1080),
        ] {
            let shifted_left = Rect::new(monitor.x - 100, monitor.y, monitor.w, monitor.h);
            let shifted_up = Rect::new(monitor.x, monitor.y - 100, monitor.w, monitor.h);
            assert!(!GeometryConstraints::covers_full_monitor(
                &shifted_left,
                &monitor
            ));
            assert!(!GeometryConstraints::covers_full_monitor(
                &shifted_up,
                &monitor
            ));
            let covering = Rect::new(
                monitor.x - 100,
                monitor.y - 100,
                monitor.w + 100,
                monitor.h + 100,
            );
            assert!(GeometryConstraints::covers_full_monitor(
                &covering, &monitor
            ));
        }
        let edge = Rect::new(i32::MAX - 100, i32::MAX - 100, 200, 200);
        assert!(GeometryConstraints::covers_full_monitor(&edge, &edge));
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

    #[test]
    fn boundary_clamping_widens_far_edges_before_subtracting_window_size() {
        for (origin, extent, size, coordinate, expected) in [
            (i32::MAX - 10, 100, 100, i32::MAX, i32::MAX - 10),
            (i32::MAX - 10, 100, 1, i32::MAX, i32::MAX),
            (i32::MIN, 10, 100, i32::MAX, i32::MIN),
            (i32::MIN, i32::MAX, 1, i32::MAX, -2),
            (-1920, 1920, 800, 100, -800),
            (-1920, 1920, 800, -3000, -1920),
            (100, 0, 1, i32::MAX, 100),
            (100, -10, 1, i32::MIN, 100),
            (i32::MIN, i32::MIN, i32::MAX, i32::MAX, i32::MIN),
        ] {
            let boundary = Rect::new(origin, origin, extent, extent);
            let (mut x, mut y) = (coordinate, coordinate);
            GeometryConstraints::clamp_rect_to_boundary(&mut x, &mut y, size, size, &boundary);
            assert_eq!((x, y), (expected, expected));
        }
    }

    #[test]
    fn intersection_widens_edges_and_rejects_extremely_separated_rectangles() {
        let low = Rect::new(i32::MIN, i32::MIN, 100, 100);
        let high = Rect::new(i32::MAX - 50, i32::MAX - 50, 100, 100);
        assert_eq!(GeometryConstraints::rect_intersection(&low, &high), None);
        assert_eq!(GeometryConstraints::rect_intersection(&high, &low), None);
        assert_eq!(
            GeometryConstraints::rect_intersection(&high, &high),
            Some(high)
        );
        let inner = Rect::new(i32::MAX - 25, i32::MAX - 25, 25, 25);
        assert_eq!(
            GeometryConstraints::rect_intersection(&high, &inner),
            Some(inner)
        );
        assert_eq!(
            GeometryConstraints::rect_intersection(&inner, &high),
            Some(inner)
        );
        for size in [0, -1, i32::MIN] {
            let empty = Rect::new(i32::MAX, i32::MAX, size, size);
            assert_eq!(GeometryConstraints::rect_intersection(&high, &empty), None);
        }
    }

    #[test]
    fn a_half_aspect_pair_constrains_nothing() {
        // The X11 hint parser records an aspect pair only when both halves
        // are well-formed and inside its sane band, so a client asking
        // "never narrower than 4:3, no practical maximum" arrives here as
        // `(0.0, 0.0)`. That is lossless precisely because this gate needs
        // both halves: a surviving 4:3 minimum would constrain exactly as
        // much as the absent pair does, which is nothing. If this gate is
        // ever split so one half can act alone, the parser has to stop
        // dropping the pair — that is the other side of the contract, pinned
        // by `a_half_aspect_pair_is_recorded_absent` beside the parser.
        let square = |min_aspect: f32, max_aspect: f32| {
            GeometryConstraints::apply_aspect_ratio_constraints(
                400,
                400,
                &SizeHints {
                    min_aspect,
                    max_aspect,
                    ..Default::default()
                },
            )
        };
        assert_eq!(square(0.0, 0.0), (400, 400));
        assert_eq!(square(4.0 / 3.0, 0.0), (400, 400));
        assert_eq!(square(0.0, 16.0 / 9.0), (400, 400));
        // With both halves present the same minimum does constrain, so this
        // would notice if the gate itself changed.
        assert_eq!(square(4.0 / 3.0, 16.0 / 9.0), (400, 300));
    }

    /// Regression: the aspect correction used to *grow* the other side, so
    /// a 16:9 client (mpv with keepaspect-window) tiled into a 900×1000
    /// stack cell was configured 1778 px wide — over the neighbouring tile
    /// and off the monitor. The offered box is a ceiling in both
    /// directions, whichever side is out of range.
    #[test]
    fn aspect_hints_fit_the_window_inside_the_offered_box() {
        let sixteen_nine = SizeHints {
            min_aspect: 16.0 / 9.0,
            max_aspect: 16.0 / 9.0,
            ..Default::default()
        };
        // Too narrow: the width stays, the height is cut to 900 / (16/9).
        assert_eq!(
            GeometryConstraints::calculate_constrained_size(900, 1000, &sixteen_nine),
            (900, 506)
        );
        // Too wide: the height stays, the width is cut to 500 * (16/9).
        assert_eq!(
            GeometryConstraints::calculate_constrained_size(1920, 500, &sixteen_nine),
            (889, 500)
        );

        for (w, h) in [(900, 1000), (1920, 500), (1, 1000), (1000, 1), (640, 480)] {
            let (cw, ch) = GeometryConstraints::calculate_constrained_size(w, h, &sixteen_nine);
            assert!(cw <= w && ch <= h, "({w}, {h}) grew to ({cw}, {ch})");
        }
    }

    /// Aspect runs before the increments, so the side it shortens is still
    /// aligned to the client's step (a terminal keeps whole rows), and the
    /// alignment rounds toward the base, keeping the box a ceiling.
    #[test]
    fn aspect_correction_keeps_increment_alignment_inside_the_box() {
        let hints = SizeHints {
            base_w: 4,
            base_h: 6,
            inc_w: 10,
            inc_h: 16,
            min_aspect: 16.0 / 9.0,
            max_aspect: 16.0 / 9.0,
            ..Default::default()
        };
        let (w, h) = GeometryConstraints::calculate_constrained_size(900, 1000, &hints);
        assert!(w <= 900 && h <= 1000, "({w}, {h}) left the 900x1000 box");
        assert_eq!((w - hints.base_w) % hints.inc_w, 0, "width {w} off-step");
        assert_eq!((h - hints.base_h) % hints.inc_h, 0, "height {h} off-step");
        // Aspect cuts the height to 506; the steps then take 900 -> 894 and
        // 506 -> 502.
        assert_eq!((w, h), (894, 502));
    }
}
