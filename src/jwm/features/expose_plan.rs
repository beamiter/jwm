//! Expose / Mission Control 决策（Phase 2 服务抽取）。
//!
//! 进入/退出 expose 的决策此前散落在 `toggles.rs` 与 `input_handler.rs`：
//! 同一段"清状态、关合成器模式、解除键盘和指针抓取"的退出序列重复了
//! 四次（切换关闭、Escape、点中缩略图、点击空白），进入时的窗口资格
//! 规则（可见且尺寸为正、无候选则不进入）内联在切换函数里。这里把
//! 决策收敛为纯函数返回的 [`ExposeAction`]，编排层只负责执行动作。

use crate::backend::common_define::WindowId;

/// 一个待进入 expose 的候选窗口：`(窗口, x, y, 宽, 高, 标题)`，几何与
/// 标题均为客户端原始记录（宽高可能为非正值，由计划过滤；标题由计划
/// 清洗，见 [`sanitize_title`]）。
pub type ExposeCandidate = (WindowId, i32, i32, i32, i32, String);

/// 编排层要执行的动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExposeAction {
    /// 进入 expose：把窗口列表交给合成器排布，并抓取键盘与指针。
    /// 列表保证非空、尺寸均为正且标题已清洗。
    Enter {
        windows: Vec<(WindowId, i32, i32, u32, u32, String)>,
    },
    /// 退出 expose：关闭合成器模式、解除抓取；`focus` 存在时聚焦该窗口
    /// 并重排其显示器的堆叠顺序。
    Exit { focus: Option<WindowId> },
    /// 状态不变（例如没有可进入的窗口）。
    Keep,
}

/// 把窗口标题清洗成可以安全交给合成器绘制的单行文本。
///
/// 标题直接来自 `_NET_WM_NAME`（或 Wayland 的等价属性），内容可以是任何
/// 字符；启动器与通知中心跨越 WM→后端边界时同样把控制字符折叠为空格
/// （标题里的换行会让按行度量的 UI 错位）。清洗只折叠控制字符并修剪
/// 两端空白，内部的空白折叠与省略号截断由渲染侧的
/// `compositor_font::fit_ui_text` 统一完成。清洗后为空的标题表示
/// "不绘制任何标签"。
#[must_use]
pub fn sanitize_title(title: &str) -> String {
    title
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>()
        .trim()
        .to_string()
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
        .filter(|&(_, _, _, w, h, _)| w > 0 && h > 0)
        .map(|(win, x, y, w, h, title)| (win, x, y, w as u32, h as u32, sanitize_title(&title)))
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

/// 编排层在 expose 中按下 Delete/BackSpace 后要执行的动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExposeCloseAction {
    /// 关闭 `window`，用 `survivors`（原网格移除该项、顺序不变）就地重建
    /// 网格，并把高亮落到 `select`（`survivors` 的索引）：被关项之后的那
    /// 个滑到高亮下；若关的是尾项则钳到新尾。与切换器「行移除保持索引、
    /// 尾行钳位」同式。
    Close {
        window: WindowId,
        survivors: Vec<(WindowId, i32, i32, u32, u32, String)>,
        select: usize,
    },
    /// 高亮项是网格最后一项：关闭 `window` 并结束手势。进入路径本就拒绝
    /// 空网格（[`plan_toggle`] 的 `Keep`），覆盖层同样不悬停在空网格上；
    /// 网格已不存在，之后也不可能提交一个死窗。
    CloseLast { window: WindowId },
    /// 没有高亮，或高亮命名的不是网格中的活窗：状态不变。合成器只回报
    /// 高亮的窗口 id，WM 对网格的认知就是活窗候选列表——关不了也重钳不
    /// 了的 Delete 必须什么也不做。（expose 的网格归合成器持有，这是与
    /// 切换器的差别：切换器快照在 WM 手里，死行照样能摘除。）
    Keep,
}

/// 决定 expose 模式下按下 Delete/BackSpace 要做什么：关闭**高亮**的缩略
/// 图（不是焦点窗——手势中两者通常不同），手势不退出。
///
/// 候选的过滤与标题清洗与 [`plan_toggle`] 完全一致，所以重建出的网格与
/// 重新进入排出的那个逐字节相同。
#[must_use]
pub fn plan_close(
    candidates: impl IntoIterator<Item = ExposeCandidate>,
    highlighted: Option<WindowId>,
) -> ExposeCloseAction {
    let Some(highlighted) = highlighted else {
        return ExposeCloseAction::Keep;
    };
    let mut windows: Vec<_> = candidates
        .into_iter()
        .filter(|&(_, _, _, w, h, _)| w > 0 && h > 0)
        .map(|(win, x, y, w, h, title)| (win, x, y, w as u32, h as u32, sanitize_title(&title)))
        .collect();
    let Some(index) = windows.iter().position(|&(win, ..)| win == highlighted) else {
        return ExposeCloseAction::Keep;
    };
    windows.remove(index);
    if windows.is_empty() {
        return ExposeCloseAction::CloseLast {
            window: highlighted,
        };
    }
    // 高亮保持索引：次旧的滑到光标下；关的是尾项时钳到新尾。
    let select = index.min(windows.len() - 1);
    ExposeCloseAction::Close {
        window: highlighted,
        survivors: windows,
        select,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(id: u64) -> WindowId {
        WindowId::from_raw(id)
    }

    #[test]
    fn toggling_while_active_exits_without_focusing() {
        let action = plan_toggle(true, vec![(win(1), 0, 0, 100, 100, "a".to_string())]);
        assert_eq!(action, ExposeAction::Exit { focus: None });
    }

    #[test]
    fn entering_filters_non_positive_geometry_and_keeps_order() {
        let action = plan_toggle(
            false,
            vec![
                (win(1), 0, 0, 100, 200, "one".to_string()),
                (win(2), 5, 5, 0, 50, "two".to_string()),
                (win(3), 5, 5, 50, -1, "three".to_string()),
                (win(4), -10, 20, 300, 400, "four".to_string()),
            ],
        );
        assert_eq!(
            action,
            ExposeAction::Enter {
                windows: vec![
                    (win(1), 0, 0, 100, 200, "one".to_string()),
                    (win(4), -10, 20, 300, 400, "four".to_string())
                ],
            }
        );
    }

    #[test]
    fn entering_with_no_eligible_windows_changes_nothing() {
        assert_eq!(plan_toggle(false, vec![]), ExposeAction::Keep);
        assert_eq!(
            plan_toggle(false, vec![(win(1), 0, 0, 0, 0, String::new())]),
            ExposeAction::Keep
        );
    }

    #[test]
    fn entering_carries_sanitized_titles() {
        let action = plan_toggle(
            false,
            vec![
                (win(1), 0, 0, 100, 100, "  spaced  ".to_string()),
                (win(2), 0, 0, 100, 100, "line\nbreak\ttab".to_string()),
                (win(3), 0, 0, 100, 100, " \n\t ".to_string()),
            ],
        );
        assert_eq!(
            action,
            ExposeAction::Enter {
                windows: vec![
                    (win(1), 0, 0, 100, 100, "spaced".to_string()),
                    (win(2), 0, 0, 100, 100, "line break tab".to_string()),
                    (win(3), 0, 0, 100, 100, String::new()),
                ],
            }
        );
    }

    #[test]
    fn sanitize_title_collapses_controls_and_trims() {
        assert_eq!(sanitize_title("plain"), "plain");
        assert_eq!(sanitize_title("a\nb\x07c\0d"), "a b c d");
        assert_eq!(sanitize_title("\t padded \r\n"), "padded");
        // Whitespace-only and empty titles sanitize to empty: the renderer
        // draws no label for them.
        assert_eq!(sanitize_title("   "), "");
        assert_eq!(sanitize_title(""), "");
        // Non-ASCII text passes through untouched.
        assert_eq!(sanitize_title("终端 — 编辑器"), "终端 — 编辑器");
        // Interior whitespace runs are the rasterizer's business
        // (`fit_ui_text` collapses them); sanitizing does not.
        assert_eq!(sanitize_title("a  b"), "a  b");
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

    fn candidates(ids: &[u64]) -> Vec<ExposeCandidate> {
        ids.iter()
            .map(|&id| (win(id), 0, 0, 100, 100, format!("w{id}")))
            .collect()
    }

    #[test]
    fn closing_the_highlight_keeps_survivor_order_and_slides_the_next_under() {
        // Highlight in the middle: the entry after it takes its index.
        let action = plan_close(candidates(&[1, 2, 3, 4]), Some(win(2)));
        assert_eq!(
            action,
            ExposeCloseAction::Close {
                window: win(2),
                survivors: vec![
                    (win(1), 0, 0, 100, 100, "w1".to_string()),
                    (win(3), 0, 0, 100, 100, "w3".to_string()),
                    (win(4), 0, 0, 100, 100, "w4".to_string()),
                ],
                select: 1,
            }
        );
        // Highlight first: the same rule lands on the new head.
        let action = plan_close(candidates(&[1, 2, 3]), Some(win(1)));
        assert_eq!(
            action,
            ExposeCloseAction::Close {
                window: win(1),
                survivors: vec![
                    (win(2), 0, 0, 100, 100, "w2".to_string()),
                    (win(3), 0, 0, 100, 100, "w3".to_string()),
                ],
                select: 0,
            }
        );
    }

    #[test]
    fn closing_the_tail_clamps_the_highlight_to_the_new_tail() {
        let action = plan_close(candidates(&[1, 2, 3]), Some(win(3)));
        assert_eq!(
            action,
            ExposeCloseAction::Close {
                window: win(3),
                survivors: vec![
                    (win(1), 0, 0, 100, 100, "w1".to_string()),
                    (win(2), 0, 0, 100, 100, "w2".to_string()),
                ],
                select: 1,
            }
        );
    }

    #[test]
    fn closing_the_last_entry_signals_the_gesture_ends() {
        assert_eq!(
            plan_close(candidates(&[7]), Some(win(7))),
            ExposeCloseAction::CloseLast { window: win(7) }
        );
    }

    #[test]
    fn close_refuses_an_empty_grid_and_an_unknown_highlight() {
        // No windows at all, nothing highlighted, or a highlight that names
        // no live candidate: nothing to close, nothing to rebuild.
        assert_eq!(
            plan_close(candidates(&[]), Some(win(1))),
            ExposeCloseAction::Keep
        );
        assert_eq!(
            plan_close(candidates(&[1, 2]), None),
            ExposeCloseAction::Keep
        );
        assert_eq!(
            plan_close(candidates(&[1, 2]), Some(win(9))),
            ExposeCloseAction::Keep
        );
    }

    #[test]
    fn close_filters_geometry_and_sanitizes_titles_like_entering() {
        // A candidate the enter plan would have dropped cannot be closed,
        // and the survivors carry the same cleaned titles a re-enter would.
        let action = plan_close(
            vec![
                (win(1), 0, 0, 100, 100, "one\ntwo".to_string()),
                (win(2), 0, 0, 0, 50, "dead".to_string()),
                (win(3), 0, 0, 100, 100, "  three  ".to_string()),
            ],
            Some(win(1)),
        );
        assert_eq!(
            action,
            ExposeCloseAction::Close {
                window: win(1),
                survivors: vec![(win(3), 0, 0, 100, 100, "three".to_string())],
                select: 0,
            }
        );
        // A candidate with non-positive geometry never made the grid, so a
        // highlight naming it refuses to close.
        assert_eq!(
            plan_close(
                vec![(win(2), 0, 0, 0, 50, "dead".to_string())],
                Some(win(2))
            ),
            ExposeCloseAction::Keep
        );
    }
}
