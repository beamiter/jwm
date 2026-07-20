//! Expose / Mission Control 决策（Phase 2 服务抽取）。
//!
//! 进入/退出 expose 的决策此前散落在 `toggles.rs` 与 `input_handler.rs`：
//! 同一段"清状态、关合成器模式、解除键盘和指针抓取"的退出序列重复了
//! 四次（切换关闭、Escape、点中缩略图、点击空白），进入时的窗口资格
//! 规则（可见且尺寸为正、无候选则不进入）内联在切换函数里。这里把
//! 决策收敛为纯函数返回的 [`ExposeAction`]，编排层只负责执行动作。

use crate::backend::common_define::WindowId;

/// 一个待进入 expose 的候选窗口：`(窗口, x, y, 宽, 高)`，几何为客户端
/// 原始记录（宽高可能为非正值，由计划过滤）。
pub type ExposeCandidate = (WindowId, i32, i32, i32, i32);

/// 编排层要执行的动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExposeAction {
    /// 进入 expose：把窗口列表交给合成器排布，并抓取键盘与指针。
    /// 列表保证非空且尺寸均为正。
    Enter {
        windows: Vec<(WindowId, i32, i32, u32, u32)>,
    },
    /// 退出 expose：关闭合成器模式、解除抓取；`focus` 存在时聚焦该窗口
    /// 并重排其显示器的堆叠顺序。
    Exit { focus: Option<WindowId> },
    /// 状态不变（例如没有可进入的窗口）。
    Keep,
}

/// 决定一次 expose 切换要做什么。
///
/// 已激活时总是退出且不聚焦任何窗口；未激活时过滤掉尺寸非正的候选，
/// 没有剩余候选则保持现状。
#[must_use]
pub fn plan_toggle(
    active: bool,
    candidates: impl IntoIterator<Item = ExposeCandidate>,
) -> ExposeAction {
    if active {
        return ExposeAction::Exit { focus: None };
    }
    let windows: Vec<_> = candidates
        .into_iter()
        .filter(|&(_, _, _, w, h)| w > 0 && h > 0)
        .map(|(win, x, y, w, h)| (win, x, y, w as u32, h as u32))
        .collect();
    if windows.is_empty() {
        ExposeAction::Keep
    } else {
        ExposeAction::Enter { windows }
    }
}

/// 决定 expose 模式下一次点击要做什么：无论是否命中缩略图都退出，
/// 命中时退出后聚焦命中的窗口。
#[must_use]
pub fn plan_click(hit: Option<WindowId>) -> ExposeAction {
    ExposeAction::Exit { focus: hit }
}

/// 决定 expose 模式下按下 Escape 要做什么：直接退出，不聚焦。
#[must_use]
pub fn plan_escape() -> ExposeAction {
    ExposeAction::Exit { focus: None }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(id: u64) -> WindowId {
        WindowId::from_raw(id)
    }

    #[test]
    fn toggling_while_active_exits_without_focusing() {
        let action = plan_toggle(true, vec![(win(1), 0, 0, 100, 100)]);
        assert_eq!(action, ExposeAction::Exit { focus: None });
    }

    #[test]
    fn entering_filters_non_positive_geometry_and_keeps_order() {
        let action = plan_toggle(
            false,
            vec![
                (win(1), 0, 0, 100, 200),
                (win(2), 5, 5, 0, 50),
                (win(3), 5, 5, 50, -1),
                (win(4), -10, 20, 300, 400),
            ],
        );
        assert_eq!(
            action,
            ExposeAction::Enter {
                windows: vec![(win(1), 0, 0, 100, 200), (win(4), -10, 20, 300, 400)],
            }
        );
    }

    #[test]
    fn entering_with_no_eligible_windows_changes_nothing() {
        assert_eq!(plan_toggle(false, vec![]), ExposeAction::Keep);
        assert_eq!(
            plan_toggle(false, vec![(win(1), 0, 0, 0, 0)]),
            ExposeAction::Keep
        );
    }

    #[test]
    fn clicks_always_exit_and_focus_only_on_a_hit() {
        assert_eq!(
            plan_click(Some(win(9))),
            ExposeAction::Exit {
                focus: Some(win(9))
            }
        );
        assert_eq!(plan_click(None), ExposeAction::Exit { focus: None });
    }

    #[test]
    fn escape_exits_without_focusing() {
        assert_eq!(plan_escape(), ExposeAction::Exit { focus: None });
    }
}
