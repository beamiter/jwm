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
        aligned_offset(i64::from(size), i64::from(increment)) as i32
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
    pub fn calculate_constrained_size(w: i32, h: i32, hints: &SizeHints) -> (i32, i32) {
        // 应用增量约束（中间量走 i64，见 `increment_aligned_dimension`）
        let mut w = increment_aligned_dimension(w, hints.base_w, hints.inc_w);
        let mut h = increment_aligned_dimension(h, hints.base_h, hints.inc_h);

        // 应用宽高比约束。入参已经在区间内且非零，所以 `w/h` 不会除零，
        // `h * aspect` 也只会在比例本身荒谬时饱和——随后立刻被收回来。
        (w, h) = Self::apply_aspect_ratio_constraints(w, h, hints);
        w = clamp_dimension(i64::from(w));
        h = clamp_dimension(i64::from(h));

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
        assert_eq!(square(4.0 / 3.0, 16.0 / 9.0), (533, 400));
    }
}
