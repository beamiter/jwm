//! Overview 导航决策（Phase 2 服务抽取）。
//!
//! 3D 棱镜轮播的滑动窗口算法此前在三处重复：`OverviewState` 的
//! 偏移调整、可见范围计算，以及 `cycle_overview` 的内联窗口推进。
//! 这里给出唯一的规范实现；导航后"棱镜要不要用新子集刷新、还是只
//! 旋转到新选择"由纯函数 `plan_cycle` 决定，编排层只负责把计划执行
//! 到合成器上。

/// 棱镜上同时可见的最大窗口数。
pub const MAX_VISIBLE: usize = 6;

/// 滑动窗口的起始索引：让选中项尽量居中，并夹取到列表边界。
///
/// 全部客户端都放得下时恒为 0。
#[must_use]
pub fn window_start(index: usize, len: usize) -> usize {
    if len <= MAX_VISIBLE {
        0
    } else {
        index.saturating_sub(MAX_VISIBLE / 2).min(len - MAX_VISIBLE)
    }
}

/// 环形导航后的新索引；空列表返回 `None`。
#[must_use]
pub fn cycle_index(current: usize, len: usize, forward: bool) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(if forward {
        (current + 1) % len
    } else {
        (current + len - 1) % len
    })
}

/// 一次导航的完整计划。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CyclePlan {
    /// 导航后的选中索引。
    pub index: usize,
    /// 导航后的滑动窗口偏移。
    pub slide_offset: usize,
    /// 滑动窗口移动时需要刷新棱镜：`(子集起点, 子集终点, 子集内选中位置)`。
    /// `None` 表示窗口未动，只需旋转到新选择。
    pub refresh_window: Option<(usize, usize, usize)>,
}

/// 决定一次环形导航要做什么。纯函数：列表长度与当前状态显式传入。
#[must_use]
pub fn plan_cycle(
    current_index: usize,
    current_offset: usize,
    len: usize,
    forward: bool,
) -> Option<CyclePlan> {
    let index = cycle_index(current_index, len, forward)?;
    if len <= MAX_VISIBLE {
        return Some(CyclePlan {
            index,
            slide_offset: current_offset,
            refresh_window: None,
        });
    }

    let start = window_start(index, len);
    let refresh_window = (start != current_offset).then(|| {
        let end = (start + MAX_VISIBLE).min(len);
        (start, end, index - start)
    });
    Some(CyclePlan {
        index,
        slide_offset: start,
        refresh_window,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_lists_never_slide() {
        assert_eq!(window_start(3, 4), 0);
        let plan = plan_cycle(3, 0, 4, true).unwrap();
        assert_eq!(plan.index, 0, "forward from the end wraps to the front");
        assert_eq!(plan.refresh_window, None);

        let plan = plan_cycle(0, 0, 4, false).unwrap();
        assert_eq!(plan.index, 3, "backward from the front wraps to the end");
    }

    #[test]
    fn empty_lists_produce_no_plan() {
        assert_eq!(cycle_index(0, 0, true), None);
        assert_eq!(plan_cycle(0, 0, 0, true), None);
    }

    #[test]
    fn window_start_centers_the_selection_and_clamps_at_both_ends() {
        // 前边界：选中项靠前时窗口贴住 0。
        assert_eq!(window_start(0, 10), 0);
        assert_eq!(window_start(2, 10), 0);
        // 居中：窗口跟随选中项。
        assert_eq!(window_start(5, 10), 2);
        // 后边界：窗口贴住列表末尾。
        assert_eq!(window_start(8, 10), 4);
        assert_eq!(window_start(9, 10), 4);
    }

    #[test]
    fn cycling_refreshes_the_prism_only_when_the_window_moves() {
        // 10 个客户端，窗口在 0：索引 3 内前进到 4，窗口移到 1。
        let plan = plan_cycle(3, 0, 10, true).unwrap();
        assert_eq!(plan.index, 4);
        assert_eq!(plan.slide_offset, 1);
        assert_eq!(plan.refresh_window, Some((1, 7, 3)));

        // 索引 1 前进到 2：窗口保持 0，只旋转。
        let plan = plan_cycle(1, 0, 10, true).unwrap();
        assert_eq!(plan.index, 2);
        assert_eq!(plan.slide_offset, 0);
        assert_eq!(plan.refresh_window, None);

        // 从 0 后退环绕到 9：窗口跳到末尾。
        let plan = plan_cycle(0, 0, 10, false).unwrap();
        assert_eq!(plan.index, 9);
        assert_eq!(plan.refresh_window, Some((4, 10, 5)));
    }

    #[test]
    fn selected_position_always_falls_inside_the_visible_window() {
        for len in [7usize, 10, 13] {
            let mut index = 0usize;
            let mut offset = 0usize;
            for _ in 0..(len * 2) {
                let plan = plan_cycle(index, offset, len, true).unwrap();
                index = plan.index;
                offset = plan.slide_offset;
                let start = offset;
                let end = (start + MAX_VISIBLE).min(len);
                assert!(
                    (start..end).contains(&index),
                    "index {index} escaped window {start}..{end} (len {len})"
                );
                if let Some((s, e, selected)) = plan.refresh_window {
                    assert_eq!(s, plan.slide_offset);
                    assert!(selected < e - s);
                }
            }
        }
    }
}
