// src/core/layout.rs
use super::types::Rect;
use std::sync::LazyLock;
use xbar_core::LayoutId;
use xbar_core::display::{
    CANONICAL_LAYOUTS, CanonicalLayout, canonical_layout, canonical_layout_by_name,
};

// 用于布局计算的客户端信息
#[derive(Clone, Copy)]
pub struct LayoutClient<K> {
    pub key: K,        // ClientKey, 用于标识
    pub factor: f32,   // client_fact
    pub border_w: i32, // border width
}

pub struct LayoutParams {
    pub screen_area: Rect,
    pub n_master: u32,
    pub m_fact: f32,
    pub gap: i32,
}

pub struct ScrollingParams {
    pub screen_area: Rect,
    pub column_width_ratio: f32, // 列宽占屏幕比例 (来自 m_fact)
    pub column_width_factors: Vec<f32>,
    pub gap: i32,
    pub viewport_x: f32, // 当前视口偏移
}

// 通用布局结果
pub struct LayoutResult<K> {
    pub key: K,
    pub rect: Rect,
}

fn bounded_gap(screen_area: Rect, gap: i32) -> i32 {
    let shortest_side = screen_area.w.max(0).min(screen_area.h.max(0));
    gap.clamp(0, shortest_side / 2)
}

fn usable_area(screen_area: Rect, gap: i32) -> Rect {
    let gap = bounded_gap(screen_area, gap);
    let inset = gap.saturating_mul(2);
    Rect::new(
        screen_area.x.saturating_add(gap),
        screen_area.y.saturating_add(gap),
        screen_area.w.saturating_sub(inset).max(1),
        screen_area.h.saturating_sub(inset).max(1),
    )
}

fn client_rect(x: i32, y: i32, w: i32, h: i32, border_w: i32) -> Rect {
    let border2 = border_w.max(0).saturating_mul(2);
    Rect::new(
        x,
        y,
        w.saturating_sub(border2).max(1),
        h.saturating_sub(border2).max(1),
    )
}

fn choose_grid_dimensions(n: usize, area: Rect) -> (i32, i32) {
    if n <= 1 {
        return (1, 1);
    }

    let target_aspect = if area.h > 0 {
        ((area.w as f32 / area.h as f32).sqrt()).clamp(1.0, 1.8)
    } else {
        1.0
    };

    let mut best = (n as i32, 1);
    let mut best_score = f32::MAX;
    for cols in 1..=n as i32 {
        let rows = (n as i32 + cols - 1) / cols;
        let cell_aspect = (area.w as f32 / cols as f32) / (area.h as f32 / rows as f32);
        let empty_cells = cols * rows - n as i32;
        let score = (cell_aspect - target_aspect).abs() + empty_cells as f32 * 0.15;
        if score < best_score {
            best = (cols, rows);
            best_score = score;
        }
    }
    best
}

fn distribute_length(
    total: i32,
    gap: i32,
    used: i32,
    index: i32,
    count: i32,
    factor: f32,
    remaining_factor: f32,
) -> i32 {
    let available = (total - (count - 1).max(0) * gap).max(1);
    let remaining = (available - used).max(1);
    if remaining_factor > 0.001 {
        (remaining as f32 * (factor.max(0.0) / remaining_factor)) as i32
    } else {
        remaining / (count - index).max(1)
    }
    .max(1)
}

fn push_factor_row<K: Copy>(
    results: &mut Vec<LayoutResult<K>>,
    clients: &[LayoutClient<K>],
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    gap: i32,
) {
    let count = clients.len() as i32;
    if count == 0 {
        return;
    }

    let mut used_w = 0;
    let mut remaining_factor: f32 = clients.iter().map(|c| c.factor.max(0.0)).sum();

    for (i, c) in clients.iter().enumerate() {
        let cw = distribute_length(w, gap, used_w, i as i32, count, c.factor, remaining_factor);
        results.push(LayoutResult {
            key: c.key,
            rect: client_rect(x + used_w + i as i32 * gap, y, cw, h, c.border_w),
        });
        used_w += cw;
        remaining_factor -= c.factor.max(0.0);
    }
}

fn push_factor_column<K: Copy>(
    results: &mut Vec<LayoutResult<K>>,
    clients: &[LayoutClient<K>],
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    gap: i32,
) {
    let count = clients.len() as i32;
    if count == 0 {
        return;
    }

    let mut used_h = 0;
    let mut remaining_factor: f32 = clients.iter().map(|c| c.factor.max(0.0)).sum();

    for (i, c) in clients.iter().enumerate() {
        let ch = distribute_length(h, gap, used_h, i as i32, count, c.factor, remaining_factor);
        results.push(LayoutResult {
            key: c.key,
            rect: client_rect(x, y + used_h + i as i32 * gap, w, ch, c.border_w),
        });
        used_h += ch;
        remaining_factor -= c.factor.max(0.0);
    }
}

fn push_deck_previews<K: Copy>(
    results: &mut Vec<LayoutResult<K>>,
    clients: &[LayoutClient<K>],
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    gap: i32,
) {
    let preview_step = gap.clamp(6, 16);
    for (i, c) in clients.iter().enumerate() {
        let preview_offset = (i as i32).min(5) * preview_step;
        results.push(LayoutResult {
            key: c.key,
            rect: client_rect(
                x + preview_offset,
                y + preview_offset,
                w - preview_offset,
                h - preview_offset,
                c.border_w,
            ),
        });
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutEnum(pub &'static str);

impl LayoutEnum {
    pub const TILE: Self = Self("tile");
    pub const FLOAT: Self = Self("float");
    pub const MONOCLE: Self = Self("monocle");
    pub const FIBONACCI: Self = Self("fibonacci");
    pub const CENTERED_MASTER: Self = Self("centeredmaster");
    pub const BSTACK: Self = Self("bstack");
    pub const GRID: Self = Self("grid");
    pub const DECK: Self = Self("deck");
    pub const THREE_COL: Self = Self("threecol");
    pub const TATAMI: Self = Self("tatami");
    pub const FULLSCREEN: Self = Self("fullscreen");
    pub const SCROLLING: Self = Self("scrolling");
    pub const VSTACK: Self = Self("vstack");
    pub const ANY: Self = Self("");

    /// The row this layout occupies in the shared protocol catalog, if any.
    /// `LayoutEnum::ANY` and any name outside the catalog have none.
    fn canonical(&self) -> Option<&'static CanonicalLayout> {
        canonical_layout_by_name(self.0)
    }

    pub fn symbol(&self) -> &str {
        self.canonical().map_or("", |layout| layout.symbol)
    }

    /// Identifier this layout travels under on the status-bar protocol. A
    /// layout outside the catalog has no wire representation and reports
    /// `None` rather than a plausible-looking zero.
    pub fn protocol_id(&self) -> Option<u32> {
        self.canonical().map(|layout| layout.id.0)
    }

    pub fn is_tile(&self) -> bool {
        matches!(
            self.0,
            "tile"
                | "fibonacci"
                | "centeredmaster"
                | "bstack"
                | "grid"
                | "deck"
                | "threecol"
                | "tatami"
                | "fullscreen"
                | "scrolling"
                | "vstack"
        )
    }
    pub fn is_float(&self) -> bool {
        self.0 == "float"
    }
    pub fn is_monocle(&self) -> bool {
        self.0 == "monocle" || self.0 == "fullscreen"
    }

    pub fn is_fullscreen_layout(&self) -> bool {
        self.0 == "fullscreen"
    }

    /// 所有布局的循环顺序 — the shared catalog's own order, so the cycle, the
    /// picker's film strip and every status bar's layout menu offer exactly
    /// the same layouts without three lists to keep in step.
    fn cycle() -> &'static [LayoutEnum] {
        static CYCLE: LazyLock<Vec<LayoutEnum>> = LazyLock::new(|| {
            CANONICAL_LAYOUTS
                .iter()
                .map(|layout| LayoutEnum(layout.name))
                .collect()
        });
        &CYCLE
    }

    pub fn cycle_next(&self) -> &'static LayoutEnum {
        let cycle = Self::cycle();
        let idx = cycle.iter().position(|l| l == self).unwrap_or(0);
        &cycle[(idx + 1) % cycle.len()]
    }

    pub fn cycle_prev(&self) -> &'static LayoutEnum {
        let cycle = Self::cycle();
        let idx = cycle.iter().position(|l| l == self).unwrap_or(0);
        &cycle[(idx + cycle.len() - 1) % cycle.len()]
    }

    /// Every layout, in the order the cycle visits them. This is what the
    /// layout picker lays out on its film strip.
    pub fn all() -> &'static [LayoutEnum] {
        Self::cycle()
    }

    /// Position in [`LayoutEnum::all`], or 0 for a layout that is not in it.
    pub fn cycle_index(&self) -> usize {
        Self::cycle().iter().position(|l| l == self).unwrap_or(0)
    }

    /// The layout `name` identifies — `tile`, `fibonacci`, `centeredmaster`, …
    /// — or `None` when nothing goes by that name.
    ///
    /// Resolving against [`LayoutEnum::all`] rather than a second list of
    /// names is what keeps a layout added to the cycle immediately settable
    /// from a keybinding and restorable from a saved per-tag entry, instead of
    /// silently missing from one of them.
    pub fn from_name(name: &str) -> Option<&'static LayoutEnum> {
        let name = name.trim().to_ascii_lowercase();
        Self::cycle().iter().find(|layout| layout.0 == name)
    }

    /// Human-facing name, for UI that has room for more than the symbol.
    pub fn label(&self) -> &'static str {
        self.canonical().map_or("Layout", |layout| layout.label)
    }
}

/// Windows a layout thumbnail is drawn with.
///
/// Four is enough for a master column plus a stack and for fibonacci's second
/// turn; the layouts whose signature only appears once the grid has to fold
/// ask for more.
pub fn preview_window_count(layout: &LayoutEnum) -> usize {
    match layout.0 {
        // Tatami weaves rows of different heights together, which a 2x2 grid
        // cannot show — at four windows it is indistinguishable from Grid.
        "tatami" => 6,
        _ => 4,
    }
}

/// The frames of one layout thumbnail, in `0.0..=1.0` of the thumbnail box.
///
/// Produced by running the layout's own geometry function over a virtual
/// monitor, so a thumbnail cannot describe a layout the window manager does
/// not actually produce. Rects are returned front-to-back in stacking order,
/// which is what makes the deck and float cascades read correctly.
pub fn preview_frames(layout: &LayoutEnum, count: usize) -> Vec<[f32; 4]> {
    /// Virtual monitor the preview is computed on: 16:10, large enough that
    /// integer rounding inside the layouts stays under a thumbnail pixel.
    const W: i32 = 1600;
    const H: i32 = 1000;
    const GAP: i32 = 28;

    let count = count.max(1);
    let clients: Vec<LayoutClient<usize>> = (0..count)
        .map(|key| LayoutClient {
            key,
            factor: 1.0,
            border_w: 0,
        })
        .collect();
    let params = LayoutParams {
        screen_area: Rect::new(0, 0, W, H),
        n_master: 1,
        m_fact: 0.55,
        gap: GAP,
    };

    let mut results = match layout.0 {
        "tile" => calculate_tile(&params, &clients),
        "fibonacci" => calculate_fibonacci(&params, &clients),
        "centeredmaster" => calculate_centered_master(&params, &clients),
        "bstack" => calculate_bstack(&params, &clients),
        "grid" => calculate_grid(&params, &clients),
        "deck" => calculate_deck(&params, &clients),
        "threecol" => calculate_three_col(&params, &clients),
        "tatami" => calculate_tatami(&params, &clients),
        "monocle" => calculate_monocle(&params, &clients),
        "fullscreen" => calculate_fullscreen(&params, &clients),
        "vstack" => calculate_vstack(&params, &clients),
        "scrolling" => {
            // One window per column is what the strip is about; the focused
            // column sits centred and its neighbours are cut off by the
            // monitor edge, which is exactly what the thumbnail should show.
            let columns: Vec<Vec<LayoutClient<usize>>> = (0..count)
                .map(|key| {
                    vec![LayoutClient {
                        key,
                        factor: 1.0,
                        border_w: 0,
                    }]
                })
                .collect();
            let scrolling = ScrollingParams {
                screen_area: params.screen_area,
                column_width_ratio: 0.45,
                column_width_factors: Vec::new(),
                gap: GAP,
                viewport_x: 0.0,
            };
            calculate_scrolling(&scrolling, &columns, count / 2).0
        }
        // Float has no tiling function: nothing places these windows, so the
        // thumbnail shows the cascade a user ends up with by hand.
        _ => {
            let step = (W / 12).min(H / 8);
            (0..count)
                .map(|i| {
                    let i = i as i32;
                    LayoutResult {
                        key: i as usize,
                        rect: Rect::new(GAP + i * step, GAP + i * step, W / 2, (H * 5) / 9),
                    }
                })
                .collect()
        }
    };

    // Monocle stacks every window in the same place; drawing them all would
    // just thicken one outline.
    results.dedup_by(|a, b| a.rect == b.rect);

    results
        .into_iter()
        .filter_map(|result| {
            let r = result.rect;
            if r.w <= 0 || r.h <= 0 {
                return None;
            }
            // Clip to the monitor: the scrolling strip runs off both edges.
            let x0 = r.x.max(0);
            let y0 = r.y.max(0);
            let x1 = (r.x + r.w).min(W);
            let y1 = (r.y + r.h).min(H);
            if x1 - x0 < W / 40 || y1 - y0 < H / 40 {
                return None;
            }
            Some([
                x0 as f32 / W as f32,
                y0 as f32 / H as f32,
                (x1 - x0) as f32 / W as f32,
                (y1 - y0) as f32 / H as f32,
            ])
        })
        .collect()
}

/// Protocol identifier → layout, as a status bar's `SetLayout` sends it.
///
/// An identifier outside the shared catalog resolves to [`LayoutEnum::ANY`],
/// which `setlayout` treats as "leave the layout alone" — a bar built against a
/// newer catalog cannot push this compositor into a layout it does not have.
impl From<u32> for LayoutEnum {
    fn from(value: u32) -> Self {
        canonical_layout(LayoutId(value)).map_or(LayoutEnum::ANY, |layout| LayoutEnum(layout.name))
    }
}

pub fn calculate_tile<K: Copy>(
    params: &LayoutParams,
    clients: &[LayoutClient<K>],
) -> Vec<LayoutResult<K>> {
    let n = clients.len() as u32;
    if n == 0 {
        return Vec::new();
    }

    let mut results = Vec::with_capacity(clients.len());
    let LayoutParams {
        screen_area,
        n_master,
        m_fact,
        gap,
    } = params;
    let gap = bounded_gap(*screen_area, *gap);

    // 外边距：缩小可用区域。gap 已夹到短边的一半，极端配置会退化成
    // 中心的一像素可用区，而不会让后续分割算术溢出。
    let area = usable_area(*screen_area, gap);
    let (wx, wy, ww, wh) = (area.x, area.y, area.w, area.h);

    let has_stack = n > *n_master && *n_master > 0;

    if *n_master == 0 {
        push_factor_column(&mut results, clients, wx, wy, ww, wh, gap);
        return results;
    }

    if has_stack && wh > ww {
        let mh = ((wh - gap) as f32 * m_fact.clamp(0.05, 0.95)) as i32;
        let sh = (wh - mh - gap).max(1);
        let master_end = (*n_master as usize).min(clients.len());
        push_factor_row(&mut results, &clients[..master_end], wx, wy, ww, mh, gap);
        push_factor_row(
            &mut results,
            &clients[master_end..],
            wx,
            wy + mh + gap,
            ww,
            sh,
            gap,
        );
        return results;
    }

    // Master 和 Stack 列之间留 gap
    let mw = if has_stack {
        ((ww - gap) as f32 * m_fact.clamp(0.05, 0.95)) as i32
    } else {
        ww
    };
    let stack_w = (ww - mw - gap).max(1);

    let master_end = (*n_master as usize).min(clients.len());
    push_factor_column(&mut results, &clients[..master_end], wx, wy, mw, wh, gap);
    push_factor_column(
        &mut results,
        &clients[master_end..],
        wx + mw + gap,
        wy,
        stack_w,
        wh,
        gap,
    );

    results
}

pub fn calculate_monocle<K: Copy>(
    params: &LayoutParams,
    clients: &[LayoutClient<K>],
) -> Vec<LayoutResult<K>> {
    let LayoutParams { screen_area, .. } = params;
    // monocle 模式不使用 gap，窗口占满整个工作区
    let (wx, wy, ww, wh) = (screen_area.x, screen_area.y, screen_area.w, screen_area.h);

    clients
        .iter()
        .map(|c| LayoutResult {
            key: c.key,
            rect: client_rect(wx, wy, ww, wh, c.border_w),
        })
        .collect()
}

pub fn calculate_fibonacci<K: Copy>(
    params: &LayoutParams,
    clients: &[LayoutClient<K>],
) -> Vec<LayoutResult<K>> {
    let n = clients.len() as u32;
    if n == 0 {
        return Vec::new();
    }

    let mut results = Vec::with_capacity(clients.len());
    let LayoutParams {
        screen_area,
        n_master,
        m_fact,
        gap,
    } = params;
    let gap = bounded_gap(*screen_area, *gap);

    // 外边距
    let area = usable_area(*screen_area, gap);
    let (wx, wy, ww, wh) = (area.x, area.y, area.w, area.h);

    let has_stack = n > *n_master;
    let mw = if has_stack {
        ((ww - gap) as f32 * m_fact.clamp(0.05, 0.95)) as i32
    } else {
        ww
    };

    let n_master_count = n.min(*n_master) as i32;
    let total_m_fact: f32 = clients
        .iter()
        .take(*n_master as usize)
        .map(|c| c.factor.max(0.0))
        .sum();
    let mut mi = 0i32;
    let mut my = 0;
    let mut remaining_m_fact = total_m_fact;

    // Stack 区域的初始状态
    let mut sx = if *n_master > 0 { wx + mw + gap } else { wx };
    let mut sy = wy;
    let mut sw = if *n_master > 0 { ww - mw - gap } else { ww };
    let mut sh = wh;

    for (i, c) in clients.iter().enumerate() {
        let is_master = (i as u32) < *n_master;
        if is_master {
            let h = distribute_length(wh, gap, my, mi, n_master_count, c.factor, remaining_m_fact);

            let res_y = wy + my + mi * gap;
            my += h;
            mi += 1;
            remaining_m_fact -= c.factor.max(0.0);

            results.push(LayoutResult {
                key: c.key,
                rect: client_rect(wx, res_y, mw, h, c.border_w),
            });
        } else {
            let stack_idx = (i as u32) - *n_master;
            let stack_count = n - *n_master;

            if stack_idx == stack_count - 1 {
                results.push(LayoutResult {
                    key: c.key,
                    rect: client_rect(sx, sy, sw, sh, c.border_w),
                });
            } else {
                if stack_idx % 2 == 0 {
                    // 水平分割
                    let h = (sh - gap) / 2;
                    results.push(LayoutResult {
                        key: c.key,
                        rect: client_rect(sx, sy, sw, h, c.border_w),
                    });
                    sy += h + gap;
                    sh -= h + gap;
                } else {
                    // 垂直分割
                    let w = (sw - gap) / 2;
                    results.push(LayoutResult {
                        key: c.key,
                        rect: client_rect(sx, sy, w, sh, c.border_w),
                    });
                    sx += w + gap;
                    sw -= w + gap;
                }
            }
        }
    }

    results
}

/// 三列骨架：左 Stack | 中 Master | 右 Stack。
/// centered_master 与 three_col 共用；`shrink_master` 控制 stack 窗口增多时
/// 是否收窄 master（centered_master 的设计），three_col 保持固定 m_fact。
fn centered_columns<K: Copy>(
    params: &LayoutParams,
    clients: &[LayoutClient<K>],
    shrink_master: bool,
) -> Vec<LayoutResult<K>> {
    let n = clients.len() as u32;
    if n == 0 {
        return Vec::new();
    }

    let n_master = params.n_master;
    if n_master == 0 {
        return calculate_grid(params, clients);
    }

    let mut results = Vec::with_capacity(clients.len());
    let gap = bounded_gap(params.screen_area, params.gap);

    let area = usable_area(params.screen_area, gap);
    let (wx, wy, ww, wh) = (area.x, area.y, area.w, area.h);

    let n_stack = (n as i32 - n_master as i32).max(0);

    if n_stack == 0 {
        // 全部是 master：纵向排满，与其他布局一样尊重 client_fact
        push_factor_column(&mut results, clients, wx, wy, ww, wh, gap);
        return results;
    }

    if wh > ww {
        return calculate_bstack(params, clients);
    }

    let mfact = params.m_fact.clamp(0.25, 0.75);
    let master_bias = if !shrink_master || n_stack <= 2 {
        1.0
    } else {
        (2.0 / n_stack as f32).max(0.7)
    };
    let mw = ((ww - 2 * gap) as f32 * mfact * master_bias).max(1.0) as i32;
    let side_w_total = (ww - mw - 2 * gap).max(1);
    let left_w = (side_w_total / 2).max(1);
    let right_w = (side_w_total - left_w).max(1);

    let master_x = wx + left_w + gap;
    let right_x = master_x + mw + gap;

    // Stack 交替分到左右两列
    let master_end = (n_master as usize).min(clients.len());
    let stack = &clients[master_end..];
    let left_clients: Vec<_> = stack.iter().step_by(2).copied().collect();
    let right_clients: Vec<_> = stack.iter().skip(1).step_by(2).copied().collect();

    push_factor_column(
        &mut results,
        &clients[..master_end],
        master_x,
        wy,
        mw,
        wh,
        gap,
    );
    push_factor_column(&mut results, &left_clients, wx, wy, left_w, wh, gap);
    push_factor_column(&mut results, &right_clients, right_x, wy, right_w, wh, gap);

    results
}

/// Centered Master: Master 居中，Stack 分列两侧
pub fn calculate_centered_master<K: Copy>(
    params: &LayoutParams,
    clients: &[LayoutClient<K>],
) -> Vec<LayoutResult<K>> {
    centered_columns(params, clients, true)
}

/// Bottom Stack: Master 在上，Stack 横排在下
pub fn calculate_bstack<K: Copy>(
    params: &LayoutParams,
    clients: &[LayoutClient<K>],
) -> Vec<LayoutResult<K>> {
    let n = clients.len() as u32;
    if n == 0 {
        return Vec::new();
    }

    let mut results = Vec::with_capacity(clients.len());
    let gap = bounded_gap(params.screen_area, params.gap);

    let area = usable_area(params.screen_area, gap);
    let (wx, wy, ww, wh) = (area.x, area.y, area.w, area.h);

    let n_master = params.n_master;
    let n_master_count = n.min(n_master) as i32;
    let n_stack = (n as i32 - n_master as i32).max(0);

    if n_master == 0 {
        return calculate_grid(params, clients);
    }

    let has_stack = n_stack > 0 && n_master_count > 0;
    let mh = if has_stack {
        ((wh - gap) as f32 * params.m_fact) as i32
    } else {
        wh
    };

    let stack_rows = if n_stack > 4 { 2 } else { 1 };
    let stack_cols = if n_stack > 0 {
        (n_stack + stack_rows - 1) / stack_rows
    } else {
        0
    };
    let stack_total_h = (wh - mh - gap).max(1);
    let stack_cell_h = ((stack_total_h - (stack_rows - 1).max(0) * gap) / stack_rows.max(1)).max(1);

    let master_end = (n_master as usize).min(clients.len());
    push_factor_row(&mut results, &clients[..master_end], wx, wy, ww, mh, gap);

    let stack_clients = &clients[master_end..];
    for row in 0..stack_rows {
        let row_start = (row * stack_cols) as usize;
        if row_start >= stack_clients.len() {
            break;
        }
        let row_len = if row == stack_rows - 1 {
            (n_stack - row * stack_cols).max(0) as usize
        } else {
            stack_cols as usize
        };
        let row_end = (row_start + row_len).min(stack_clients.len());
        // 末行吸收整除余数，与工作区底边齐平
        let row_h = if row == stack_rows - 1 {
            (stack_total_h - row * (stack_cell_h + gap)).max(1)
        } else {
            stack_cell_h
        };
        push_factor_row(
            &mut results,
            &stack_clients[row_start..row_end],
            wx,
            wy + mh + gap + row * (stack_cell_h + gap),
            ww,
            row_h,
            gap,
        );
    }

    results
}

/// Grid: 等大小网格排列
pub fn calculate_grid<K: Copy>(
    params: &LayoutParams,
    clients: &[LayoutClient<K>],
) -> Vec<LayoutResult<K>> {
    let n = clients.len();
    if n == 0 {
        return Vec::new();
    }

    let mut results = Vec::with_capacity(n);
    let gap = bounded_gap(params.screen_area, params.gap);

    let area = usable_area(params.screen_area, gap);
    let (wx, wy, ww, wh) = (area.x, area.y, area.w, area.h);

    let (cols, rows) = choose_grid_dimensions(n, area);

    let cell_h = (wh - (rows - 1) * gap) / rows;

    for (i, c) in clients.iter().enumerate() {
        let row = i as i32 / cols;
        let col = i as i32 - row * cols;
        // 最后一行可能不满，拉宽填满
        let row_cols = if row == rows - 1 {
            n as i32 - row * cols
        } else {
            cols
        };
        let cell_w = (ww - (row_cols - 1) * gap) / row_cols;

        // 行尾 / 底行吸收整除余数，让网格与工作区边缘齐平
        let x_off = col * (cell_w + gap);
        let y_off = row * (cell_h + gap);
        let w = if col == row_cols - 1 {
            ww - x_off
        } else {
            cell_w
        };
        let h = if row == rows - 1 { wh - y_off } else { cell_h };

        results.push(LayoutResult {
            key: c.key,
            rect: client_rect(wx + x_off, wy + y_off, w, h, c.border_w),
        });
    }

    results
}

/// Deck: Master 在左，Stack 区所有窗口重叠
pub fn calculate_deck<K: Copy>(
    params: &LayoutParams,
    clients: &[LayoutClient<K>],
) -> Vec<LayoutResult<K>> {
    let n = clients.len() as u32;
    if n == 0 {
        return Vec::new();
    }

    let mut results = Vec::with_capacity(clients.len());
    let gap = bounded_gap(params.screen_area, params.gap);

    let area = usable_area(params.screen_area, gap);
    let (wx, wy, ww, wh) = (area.x, area.y, area.w, area.h);

    let n_master = params.n_master;

    let has_stack = n > n_master && n_master > 0;

    if n_master == 0 {
        push_deck_previews(&mut results, clients, wx, wy, ww, wh, gap);
        return results;
    }

    if has_stack && wh > ww {
        let mfact = params.m_fact.clamp(0.25, 0.75);
        let mh = ((wh - gap) as f32 * mfact).max(1.0) as i32;
        let sh = (wh - mh - gap).max(1);
        let master_end = (n_master as usize).min(clients.len());
        push_factor_row(&mut results, &clients[..master_end], wx, wy, ww, mh, gap);
        push_deck_previews(
            &mut results,
            &clients[master_end..],
            wx,
            wy + mh + gap,
            ww,
            sh,
            gap,
        );
        return results;
    }

    let mw = if has_stack {
        ((ww - gap) as f32 * params.m_fact) as i32
    } else {
        ww
    };

    let master_end = (n_master as usize).min(clients.len());
    push_factor_column(&mut results, &clients[..master_end], wx, wy, mw, wh, gap);
    push_deck_previews(
        &mut results,
        &clients[master_end..],
        wx + mw + gap,
        wy,
        ww - mw - gap,
        wh,
        gap,
    );

    results
}

/// Three Column: 左Stack | 中Master | 右Stack
pub fn calculate_three_col<K: Copy>(
    params: &LayoutParams,
    clients: &[LayoutClient<K>],
) -> Vec<LayoutResult<K>> {
    centered_columns(params, clients, false)
}

/// Tatami: 日式榻榻米布局，根据窗口数量选择不同的排列图案
pub fn calculate_tatami<K: Copy>(
    params: &LayoutParams,
    clients: &[LayoutClient<K>],
) -> Vec<LayoutResult<K>> {
    let n = clients.len();
    if n == 0 {
        return Vec::new();
    }

    let mut results = Vec::with_capacity(n);
    let gap = bounded_gap(params.screen_area, params.gap);

    if n > 10 {
        return calculate_grid(params, clients);
    }

    let area = usable_area(params.screen_area, gap);
    let (wx, wy, ww, wh) = (area.x, area.y, area.w, area.h);

    if n <= 4 {
        // 少量窗口直接铺
        match n {
            1 => {
                results.push(LayoutResult {
                    key: clients[0].key,
                    rect: client_rect(wx, wy, ww, wh, clients[0].border_w),
                });
            }
            2 => {
                let w = (ww - gap) / 2;
                for (i, c) in clients.iter().enumerate() {
                    results.push(LayoutResult {
                        key: c.key,
                        rect: client_rect(wx + i as i32 * (w + gap), wy, w, wh, c.border_w),
                    });
                }
            }
            3 => {
                let lw = (ww - gap) / 2;
                let rw = ww - lw - gap;
                let rh = (wh - gap) / 2;
                results.push(LayoutResult {
                    key: clients[0].key,
                    rect: client_rect(wx, wy, lw, wh, clients[0].border_w),
                });
                results.push(LayoutResult {
                    key: clients[1].key,
                    rect: client_rect(wx + lw + gap, wy, rw, rh, clients[1].border_w),
                });
                results.push(LayoutResult {
                    key: clients[2].key,
                    rect: client_rect(
                        wx + lw + gap,
                        wy + rh + gap,
                        rw,
                        wh - rh - gap,
                        clients[2].border_w,
                    ),
                });
            }
            4 => {
                let cw = (ww - gap) / 2;
                let ch = (wh - gap) / 2;
                for (i, c) in clients.iter().enumerate() {
                    let col = i as i32 % 2;
                    let row = i as i32 / 2;
                    results.push(LayoutResult {
                        key: c.key,
                        rect: client_rect(
                            wx + col * (cw + gap),
                            wy + row * (ch + gap),
                            cw,
                            ch,
                            c.border_w,
                        ),
                    });
                }
            }
            _ => {}
        }
    } else {
        // 5+ 窗口：分组，每组 5 个，交替使用两种榻榻米图案
        let mut idx = 0;
        let groups = (n + 4) / 5;
        let row_h = (wh - (groups as i32 - 1) * gap) / groups as i32;

        for g in 0..groups {
            let remaining = n - idx;
            let count = remaining.min(5);
            let gy = wy + g as i32 * (row_h + gap);
            // 末组吸收整除余数，与工作区底边齐平
            let row_h = if g == groups - 1 {
                (wh - g as i32 * (row_h + gap)).max(1)
            } else {
                row_h
            };

            if count < 5 {
                // 不足 5 个的尾部组用 grid 方式铺
                let cols = count as i32;
                let cw = (ww - (cols - 1) * gap) / cols;
                for j in 0..count {
                    let actual_w = if j as i32 == cols - 1 {
                        ww - (cols - 1) * (cw + gap)
                    } else {
                        cw
                    };
                    results.push(LayoutResult {
                        key: clients[idx + j].key,
                        rect: client_rect(
                            wx + j as i32 * (cw + gap),
                            gy,
                            actual_w,
                            row_h,
                            clients[idx + j].border_w,
                        ),
                    });
                }
            } else {
                // 5 个窗口：经典榻榻米
                let top_h = (row_h - gap) / 2;
                let bot_h = row_h - top_h - gap;

                if g % 2 == 0 {
                    // 图案 A: 上 3 下 2
                    let tw = (ww - 2 * gap) / 3;
                    let tw_last = ww - 2 * (tw + gap);
                    let bw = (ww - gap) / 2;
                    let bw_last = ww - bw - gap;
                    results.push(LayoutResult {
                        key: clients[idx].key,
                        rect: client_rect(wx, gy, tw, top_h, clients[idx].border_w),
                    });
                    results.push(LayoutResult {
                        key: clients[idx + 1].key,
                        rect: client_rect(wx + tw + gap, gy, tw, top_h, clients[idx + 1].border_w),
                    });
                    results.push(LayoutResult {
                        key: clients[idx + 2].key,
                        rect: client_rect(
                            wx + 2 * (tw + gap),
                            gy,
                            tw_last,
                            top_h,
                            clients[idx + 2].border_w,
                        ),
                    });
                    results.push(LayoutResult {
                        key: clients[idx + 3].key,
                        rect: client_rect(
                            wx,
                            gy + top_h + gap,
                            bw,
                            bot_h,
                            clients[idx + 3].border_w,
                        ),
                    });
                    results.push(LayoutResult {
                        key: clients[idx + 4].key,
                        rect: client_rect(
                            wx + bw + gap,
                            gy + top_h + gap,
                            bw_last,
                            bot_h,
                            clients[idx + 4].border_w,
                        ),
                    });
                } else {
                    // 图案 B: 上 2 下 3
                    let tw = (ww - gap) / 2;
                    let tw_last = ww - tw - gap;
                    let bw = (ww - 2 * gap) / 3;
                    let bw_last = ww - 2 * (bw + gap);
                    results.push(LayoutResult {
                        key: clients[idx].key,
                        rect: client_rect(wx, gy, tw, top_h, clients[idx].border_w),
                    });
                    results.push(LayoutResult {
                        key: clients[idx + 1].key,
                        rect: client_rect(
                            wx + tw + gap,
                            gy,
                            tw_last,
                            top_h,
                            clients[idx + 1].border_w,
                        ),
                    });
                    results.push(LayoutResult {
                        key: clients[idx + 2].key,
                        rect: client_rect(
                            wx,
                            gy + top_h + gap,
                            bw,
                            bot_h,
                            clients[idx + 2].border_w,
                        ),
                    });
                    results.push(LayoutResult {
                        key: clients[idx + 3].key,
                        rect: client_rect(
                            wx + bw + gap,
                            gy + top_h + gap,
                            bw,
                            bot_h,
                            clients[idx + 3].border_w,
                        ),
                    });
                    results.push(LayoutResult {
                        key: clients[idx + 4].key,
                        rect: client_rect(
                            wx + 2 * (bw + gap),
                            gy + top_h + gap,
                            bw_last,
                            bot_h,
                            clients[idx + 4].border_w,
                        ),
                    });
                }
            }
            idx += count;
        }
    }

    results
}

/// Fullscreen: 真全屏，占满整个显示器，无边框无 gap
pub fn calculate_fullscreen<K: Copy>(
    params: &LayoutParams,
    clients: &[LayoutClient<K>],
) -> Vec<LayoutResult<K>> {
    let LayoutParams { screen_area, .. } = params;
    // screen_area 由调用方传入完整显示器区域 (m_x, m_y, m_w, m_h)
    clients
        .iter()
        .map(|c| LayoutResult {
            key: c.key,
            rect: Rect::new(screen_area.x, screen_area.y, screen_area.w, screen_area.h),
        })
        .collect()
}

/// Scrolling tiling layout (Niri-style):
/// Columns arranged horizontally in a strip, focused column centered.
/// Returns (layout results, new viewport_x).
pub fn calculate_scrolling<K: Copy>(
    params: &ScrollingParams,
    columns: &[Vec<LayoutClient<K>>],
    focus_col: usize,
) -> (Vec<LayoutResult<K>>, f32) {
    let mut results = Vec::new();
    if columns.is_empty() {
        return (results, 0.0);
    }

    let gap = bounded_gap(params.screen_area, params.gap);
    let screen = &params.screen_area;
    let base_col_w = (screen.w as f32 * params.column_width_ratio) as i32;
    let base_col_w = base_col_w.max(1);

    // Outer margin
    let outer_gap = gap;
    let avail_h = screen.h.saturating_sub(outer_gap.saturating_mul(2)).max(0);

    // Calculate total strip width and per-column x positions (in strip space, starting at 0)
    let mut col_positions: Vec<i32> = Vec::with_capacity(columns.len());
    let mut col_widths: Vec<i32> = Vec::with_capacity(columns.len());
    let mut x_cursor = 0i32;
    for (i, _col) in columns.iter().enumerate() {
        let width_factor = params
            .column_width_factors
            .get(i)
            .copied()
            .unwrap_or(1.0)
            .clamp(0.25, 2.5);
        let col_w = ((base_col_w as f32) * width_factor) as i32;
        let col_w = col_w.max(1);
        col_positions.push(x_cursor);
        col_widths.push(col_w);
        x_cursor = x_cursor.saturating_add(col_w);
        if i + 1 < columns.len() {
            x_cursor = x_cursor.saturating_add(gap);
        }
    }

    // Center focused column in viewport
    let focus_col = focus_col.min(columns.len() - 1);
    let focus_col_center = col_positions[focus_col] as f32 + col_widths[focus_col] as f32 / 2.0;
    let new_viewport_x = focus_col_center - screen.w as f32 / 2.0;

    // Layout each column
    for (col_idx, col) in columns.iter().enumerate() {
        if col.is_empty() {
            continue;
        }
        let strip_x = col_positions[col_idx];
        let col_w = col_widths[col_idx];
        // Screen x = strip_x - viewport_x + screen.x
        // round 而不是向零截断：向零截断会让视口左侧的列（负坐标）相对
        // 右侧的列偏移 1px。
        let screen_x = (strip_x as f32 - new_viewport_x + screen.x as f32).round();

        let inner_gap_count = i32::try_from(col.len().saturating_sub(1)).unwrap_or(i32::MAX);
        let inner_gaps = inner_gap_count.saturating_mul(gap);
        let avail_col_h = avail_h.saturating_sub(inner_gaps).max(0);
        let mut remaining_fact: f32 = col.iter().map(|client| client.factor.max(0.0)).sum();

        let mut y_cursor = 0;
        for (win_idx, client) in col.iter().enumerate() {
            let remaining = i32::try_from(col.len() - win_idx).unwrap_or(i32::MAX);
            let remaining_h = avail_col_h.saturating_sub(y_cursor).max(0);
            let client_fact = client.factor.max(0.0);
            let h = if remaining_fact > 0.001 {
                (remaining_h as f32 * (client_fact / remaining_fact)) as i32
            } else {
                remaining_h / remaining.max(1)
            };

            let window_offset = i32::try_from(win_idx)
                .unwrap_or(i32::MAX)
                .saturating_mul(gap);
            let win_y = screen
                .y
                .saturating_add(outer_gap)
                .saturating_add(y_cursor)
                .saturating_add(window_offset);

            results.push(LayoutResult {
                key: client.key,
                rect: client_rect(screen_x as i32, win_y, col_w, h, client.border_w),
            });

            y_cursor = y_cursor.saturating_add(h);
            remaining_fact -= client_fact;
        }
    }

    (results, new_viewport_x)
}

/// V-Stack: all windows are half-monitor size.  The focused window
/// (clients[0]) is centred at the bottom edge.  The remaining windows fan
/// out in a V-shape – odd indices go right-up, even indices go left-up –
/// each step offset at 30° from horizontal (tan 30° ≈ 0.577).
pub fn calculate_vstack<K: Copy>(
    params: &LayoutParams,
    clients: &[LayoutClient<K>],
) -> Vec<LayoutResult<K>> {
    let n = clients.len();
    if n == 0 {
        return Vec::new();
    }

    let mut results = Vec::with_capacity(n);
    let gap = bounded_gap(params.screen_area, params.gap);

    let area = usable_area(params.screen_area, gap);
    let (wx, wy, ww, wh) = (area.x, area.y, area.w, area.h);

    // Keep the roomy half-monitor feel for a few windows, then shrink the
    // cards gradually so dense V-stacks remain readable.
    let scale = if n <= 5 {
        1.0
    } else {
        (5.0 / n as f32).sqrt().clamp(0.35, 1.0)
    };
    let half_w = ((ww as f32 / 2.0) * scale) as i32;
    let half_h = ((wh as f32 / 2.0) * scale) as i32;

    // Focused (main) client: centred horizontally, flush with the bottom
    let center_x = wx + (ww - half_w) / 2;
    let bottom_y = wy + wh - half_h;

    // Dynamic step: spread the V arms as wide as possible while keeping
    // every window inside the monitor.  max_depth is the largest depth
    // value among all non-focused windows (depth = 1,1,2,2,3,3,...).
    const TAN30: f32 = 0.57735; // tan(30°)
    let max_depth = if n <= 1 { 1 } else { n as i32 / 2 };

    // Horizontal limit: the outermost window edge must stay inside.
    //   center_x ± max_depth*step_x + half_w  <=  wx + ww
    //   ⇒ step_x <= (ww - half_w) / (2 * max_depth)       [= ww/4 / max_depth]
    let max_step_x = (ww - half_w) / (2 * max_depth);

    // Vertical limit: the topmost window must not go above the monitor.
    //   bottom_y - max_depth*step_y >= wy  ⇒  step_y <= (wh-half_h) / max_depth
    //   step_y = step_x * tan30  ⇒  step_x <= (wh-half_h) / (max_depth * tan30)
    let max_step_y = ((wh - half_h) as f32 / (max_depth as f32 * TAN30)) as i32;

    let step_x = max_step_x.min(max_step_y).max(gap);
    let step_y = (step_x as f32 * TAN30) as i32;

    for (i, c) in clients.iter().enumerate() {
        let border2 = 2 * c.border_w;
        let (x, y) = if i == 0 {
            (center_x, bottom_y)
        } else {
            let depth = ((i as i32) + 1) / 2; // 1,1,2,2,3,3,...
            let is_right = (i % 2) == 1;
            let dx = depth * step_x;
            let dy = depth * step_y;
            let x = if is_right {
                center_x + dx
            } else {
                center_x - dx
            };
            let y = bottom_y - dy;
            (x, y)
        };

        results.push(LayoutResult {
            key: c.key,
            rect: Rect::new(x, y, (half_w - border2).max(1), (half_h - border2).max(1)),
        });
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(key: u32, factor: f32) -> LayoutClient<u32> {
        LayoutClient {
            key,
            factor,
            border_w: 1,
        }
    }

    fn params(w: i32, h: i32) -> LayoutParams {
        LayoutParams {
            screen_area: Rect::new(0, 0, w, h),
            n_master: 1,
            m_fact: 0.55,
            gap: 0,
        }
    }

    // -----------------------------------------------------------------------
    // LayoutEnum
    // -----------------------------------------------------------------------

    #[test]
    fn test_layout_enum_symbol_tile() {
        assert_eq!(LayoutEnum::TILE.symbol(), "[]=");
    }

    #[test]
    fn test_layout_enum_symbol_float() {
        assert_eq!(LayoutEnum::FLOAT.symbol(), "><>");
    }

    #[test]
    fn test_layout_enum_symbol_monocle() {
        assert_eq!(LayoutEnum::MONOCLE.symbol(), "[M]");
    }

    #[test]
    fn test_layout_enum_symbol_unknown() {
        assert_eq!(LayoutEnum::ANY.symbol(), "");
    }

    /// The cycle, the picker and every status bar's layout menu are the same
    /// list seen from three places. If this drifts, a bar offers a layout this
    /// compositor cannot enter, or hides one it can.
    #[test]
    fn the_cycle_is_the_shared_protocol_catalog() {
        let names: Vec<&str> = LayoutEnum::all().iter().map(|layout| layout.0).collect();
        let catalog: Vec<&str> = CANONICAL_LAYOUTS.iter().map(|row| row.name).collect();
        assert_eq!(names, catalog);
    }

    /// Round-tripping every wire identifier is what makes a bar's layout pill
    /// land on the layout it drew.
    #[test]
    fn every_protocol_id_round_trips_through_the_layout_it_names() {
        for row in CANONICAL_LAYOUTS {
            let layout = LayoutEnum::from(row.id.0);
            assert_eq!(layout.0, row.name);
            assert_eq!(layout.protocol_id(), Some(row.id.0));
            assert_eq!(layout.symbol(), row.symbol);
            assert_eq!(layout.label(), row.label);
            assert_eq!(LayoutEnum::from_name(row.name), Some(&layout));
        }
    }

    /// A bar built against a newer catalog must not be able to push this
    /// compositor into a layout it has no arrangement for.
    #[test]
    fn an_unknown_protocol_id_resolves_to_no_layout_at_all() {
        assert_eq!(LayoutEnum::from(u32::MAX), LayoutEnum::ANY);
        assert_eq!(
            LayoutEnum::from(CANONICAL_LAYOUTS.len() as u32 + 1),
            LayoutEnum::ANY
        );
        assert_eq!(LayoutEnum::ANY.protocol_id(), None);
    }

    #[test]
    fn test_layout_enum_is_tile() {
        assert!(LayoutEnum::TILE.is_tile());
        assert!(LayoutEnum::FIBONACCI.is_tile());
        assert!(LayoutEnum::GRID.is_tile());
        assert!(!LayoutEnum::FLOAT.is_tile());
        assert!(!LayoutEnum::MONOCLE.is_tile());
    }

    #[test]
    fn test_layout_enum_is_float() {
        assert!(LayoutEnum::FLOAT.is_float());
        assert!(!LayoutEnum::TILE.is_float());
    }

    #[test]
    fn test_layout_enum_is_monocle() {
        assert!(LayoutEnum::MONOCLE.is_monocle());
        assert!(LayoutEnum::FULLSCREEN.is_monocle());
        assert!(!LayoutEnum::TILE.is_monocle());
    }

    #[test]
    fn test_layout_enum_is_fullscreen_layout() {
        assert!(LayoutEnum::FULLSCREEN.is_fullscreen_layout());
        assert!(!LayoutEnum::MONOCLE.is_fullscreen_layout());
    }

    #[test]
    fn test_layout_enum_cycle_next_wraps() {
        // Float is the last in CYCLE; next should wrap to TILE
        let next = LayoutEnum::FLOAT.cycle_next();
        assert_eq!(next, &LayoutEnum::TILE);
    }

    #[test]
    fn test_layout_enum_cycle_prev_wraps() {
        // TILE is the first in CYCLE; prev should wrap to FLOAT
        let prev = LayoutEnum::TILE.cycle_prev();
        assert_eq!(prev, &LayoutEnum::FLOAT);
    }

    #[test]
    fn test_layout_enum_cycle_next_from_tile() {
        let next = LayoutEnum::TILE.cycle_next();
        assert_eq!(next, &LayoutEnum::FIBONACCI);
    }

    #[test]
    fn test_layout_enum_from_u32_known() {
        assert_eq!(LayoutEnum::from(0), LayoutEnum::TILE);
        assert_eq!(LayoutEnum::from(1), LayoutEnum::FLOAT);
        assert_eq!(LayoutEnum::from(2), LayoutEnum::MONOCLE);
        assert_eq!(LayoutEnum::from(6), LayoutEnum::GRID);
    }

    #[test]
    fn test_layout_enum_from_u32_unknown() {
        assert_eq!(LayoutEnum::from(99), LayoutEnum::ANY);
    }

    // -----------------------------------------------------------------------
    // calculate_tile
    // -----------------------------------------------------------------------

    #[test]
    fn test_tile_empty_clients() {
        let p = params(1920, 1080);
        let result = calculate_tile::<u32>(&p, &[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_tile_single_client_fills_screen() {
        let p = params(1920, 1080);
        let clients = [client(1, 1.0)];
        let result = calculate_tile(&p, &clients);
        assert_eq!(result.len(), 1);
        let r = result[0].rect;
        assert_eq!(r.x, 0);
        assert_eq!(r.y, 0);
        // width = screen_w - border2 (2*1=2)
        assert_eq!(r.w, 1920 - 2);
        assert_eq!(r.h, 1080 - 2);
    }

    #[test]
    fn test_tile_master_and_stack() {
        let p = LayoutParams {
            screen_area: Rect::new(0, 0, 1920, 1080),
            n_master: 1,
            m_fact: 0.5,
            gap: 0,
        };
        let clients = [client(1, 1.0), client(2, 1.0)];
        let result = calculate_tile(&p, &clients);
        assert_eq!(result.len(), 2);
        let master = result[0].rect;
        let stack = result[1].rect;
        // master on the left, stack on the right
        assert!(master.x < stack.x, "master should be left of stack");
        // Both should have the same height (within rounding)
        assert!((master.h - stack.h).abs() <= 2);
    }

    #[test]
    fn test_tile_keys_preserved() {
        let p = params(1920, 1080);
        let clients = [client(42, 1.0), client(99, 1.0)];
        let result = calculate_tile(&p, &clients);
        assert_eq!(result[0].key, 42);
        assert_eq!(result[1].key, 99);
    }

    #[test]
    fn test_tile_no_overlap_between_master_and_stack() {
        let p = LayoutParams {
            screen_area: Rect::new(0, 0, 1920, 1080),
            n_master: 1,
            m_fact: 0.55,
            gap: 4,
        };
        let clients = [client(1, 1.0), client(2, 1.0)];
        let result = calculate_tile(&p, &clients);
        let master = result[0].rect;
        let stack = result[1].rect;
        // Right edge of master must not exceed left edge of stack
        let master_right = master.x + master.w + 2; // +border
        assert!(
            master_right <= stack.x,
            "master and stack should not overlap"
        );
    }

    #[test]
    fn test_tile_portrait_uses_top_bottom_split() {
        let p = LayoutParams {
            screen_area: Rect::new(0, 0, 900, 1600),
            n_master: 1,
            m_fact: 0.5,
            gap: 10,
        };
        let clients = [client(1, 1.0), client(2, 1.0)];
        let result = calculate_tile(&p, &clients);

        assert_eq!(result.len(), 2);
        assert!(result[0].rect.y < result[1].rect.y);
        assert_eq!(result[0].rect.x, result[1].rect.x);
    }

    #[test]
    fn test_tile_zero_master_stays_on_screen() {
        let p = LayoutParams {
            screen_area: Rect::new(0, 0, 1000, 700),
            n_master: 0,
            m_fact: 0.55,
            gap: 8,
        };
        let clients = [client(1, 1.0), client(2, 2.0)];
        let result = calculate_tile(&p, &clients);

        assert_eq!(result.len(), 2);
        for res in &result {
            assert!(res.rect.x >= 0);
            assert!(res.rect.x + res.rect.w <= 1000);
            assert!(res.rect.w > 0 && res.rect.h > 0);
        }
        assert!(result[1].rect.h > result[0].rect.h);
    }

    // -----------------------------------------------------------------------
    // calculate_monocle
    // -----------------------------------------------------------------------

    #[test]
    fn test_monocle_empty() {
        let p = params(1920, 1080);
        assert!(calculate_monocle::<u32>(&p, &[]).is_empty());
    }

    #[test]
    fn test_monocle_all_clients_same_rect() {
        let p = params(1920, 1080);
        let clients = [client(1, 1.0), client(2, 1.0), client(3, 1.0)];
        let result = calculate_monocle(&p, &clients);
        assert_eq!(result.len(), 3);
        // All windows get the same rect in monocle
        let r0 = result[0].rect;
        for r in &result {
            assert_eq!(r.rect, r0);
        }
    }

    #[test]
    fn test_monocle_fills_screen() {
        let p = params(1920, 1080);
        let clients = [client(7, 1.0)];
        let result = calculate_monocle(&p, &clients);
        let r = result[0].rect;
        assert_eq!(r.x, 0);
        assert_eq!(r.y, 0);
        assert_eq!(r.w, 1920 - 2); // border2
        assert_eq!(r.h, 1080 - 2);
    }

    // -----------------------------------------------------------------------
    // calculate_fibonacci
    // -----------------------------------------------------------------------

    #[test]
    fn test_fibonacci_empty() {
        let p = params(1920, 1080);
        assert!(calculate_fibonacci::<u32>(&p, &[]).is_empty());
    }

    #[test]
    fn test_fibonacci_single_fills_screen() {
        let p = params(1920, 1080);
        let clients = [client(1, 1.0)];
        let result = calculate_fibonacci(&p, &clients);
        assert_eq!(result.len(), 1);
        let r = result[0].rect;
        assert_eq!(r.w, 1920 - 2);
        assert_eq!(r.h, 1080 - 2);
    }

    #[test]
    fn test_fibonacci_multiple_produces_correct_count() {
        let p = params(1920, 1080);
        let clients: Vec<_> = (0..4).map(|i| client(i, 1.0)).collect();
        let result = calculate_fibonacci(&p, &clients);
        assert_eq!(result.len(), 4);
    }

    #[test]
    fn test_fibonacci_keys_preserved() {
        let p = params(1920, 1080);
        let clients = [client(10, 1.0), client(20, 1.0)];
        let result = calculate_fibonacci(&p, &clients);
        assert_eq!(result[0].key, 10);
        assert_eq!(result[1].key, 20);
    }

    #[test]
    fn test_fibonacci_master_uses_client_factors() {
        let p = LayoutParams {
            screen_area: Rect::new(0, 0, 1200, 900),
            n_master: 2,
            m_fact: 0.5,
            gap: 0,
        };
        let clients = [client(1, 2.0), client(2, 1.0), client(3, 1.0)];
        let result = calculate_fibonacci(&p, &clients);

        assert_eq!(result.len(), 3);
        assert!(result[0].rect.h > result[1].rect.h);
    }

    // -----------------------------------------------------------------------
    // calculate_grid
    // -----------------------------------------------------------------------

    #[test]
    fn test_grid_single_fills_screen() {
        let p = params(1920, 1080);
        let clients = [client(1, 1.0)];
        let result = calculate_grid(&p, &clients);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_grid_four_clients_two_by_two() {
        let p = params(1920, 1080);
        let clients: Vec<_> = (0..4).map(|i| client(i, 1.0)).collect();
        let result = calculate_grid(&p, &clients);
        assert_eq!(result.len(), 4);
        // All rects should be non-zero
        for r in &result {
            assert!(r.rect.w > 0 && r.rect.h > 0);
        }
    }

    #[test]
    fn test_grid_fills_work_area_flush() {
        // 整除余数由行尾/底行吸收：网格必须与可用区边缘齐平
        let p = LayoutParams {
            screen_area: Rect::new(0, 0, 1601, 999),
            n_master: 1,
            m_fact: 0.55,
            gap: 28,
        };
        for n in 1..=7 {
            let clients: Vec<_> = (0..n)
                .map(|key| LayoutClient {
                    key,
                    factor: 1.0,
                    border_w: 0,
                })
                .collect();
            let result = calculate_grid(&p, &clients);
            let right = result.iter().map(|r| r.rect.x + r.rect.w).max().unwrap();
            let bottom = result.iter().map(|r| r.rect.y + r.rect.h).max().unwrap();
            assert_eq!(right, 1601 - 28, "n={} right edge should be flush", n);
            assert_eq!(bottom, 999 - 28, "n={} bottom edge should be flush", n);
        }
    }

    #[test]
    fn test_tatami_groups_fill_work_area_flush() {
        let p = LayoutParams {
            screen_area: Rect::new(0, 0, 1601, 999),
            n_master: 1,
            m_fact: 0.55,
            gap: 28,
        };
        let clients: Vec<_> = (0..10)
            .map(|key| LayoutClient {
                key,
                factor: 1.0,
                border_w: 0,
            })
            .collect();
        let result = calculate_tatami(&p, &clients);
        let bottom = result.iter().map(|r| r.rect.y + r.rect.h).max().unwrap();
        assert_eq!(bottom, 999 - 28);
    }

    #[test]
    fn test_bstack_dense_stack_wraps_to_second_row() {
        let p = LayoutParams {
            screen_area: Rect::new(0, 0, 1600, 1000),
            n_master: 1,
            m_fact: 0.55,
            gap: 10,
        };
        let clients: Vec<_> = (0..7).map(|i| client(i, 1.0)).collect();
        let result = calculate_bstack(&p, &clients);

        assert_eq!(result.len(), 7);
        let first_stack_y = result[1].rect.y;
        assert!(
            result.iter().skip(2).any(|res| res.rect.y > first_stack_y),
            "dense bottom stack should wrap instead of staying in one row"
        );
    }

    #[test]
    fn test_bstack_master_uses_client_factors() {
        let p = LayoutParams {
            screen_area: Rect::new(0, 0, 1600, 900),
            n_master: 2,
            m_fact: 0.55,
            gap: 0,
        };
        let clients = [client(1, 2.0), client(2, 1.0), client(3, 1.0)];
        let result = calculate_bstack(&p, &clients);

        assert_eq!(result.len(), 3);
        assert!(result[0].rect.w > result[1].rect.w);
    }

    #[test]
    fn test_bstack_stack_uses_client_factors() {
        let p = LayoutParams {
            screen_area: Rect::new(0, 0, 1600, 900),
            n_master: 1,
            m_fact: 0.55,
            gap: 0,
        };
        let clients = [client(1, 1.0), client(2, 2.0), client(3, 1.0)];
        let result = calculate_bstack(&p, &clients);

        assert_eq!(result.len(), 3);
        assert!(result[1].rect.w > result[2].rect.w);
    }

    #[test]
    fn test_bstack_zero_master_falls_back_to_grid() {
        let p = LayoutParams {
            screen_area: Rect::new(0, 0, 900, 700),
            n_master: 0,
            m_fact: 0.55,
            gap: 8,
        };
        let clients: Vec<_> = (0..3).map(|i| client(i, 1.0)).collect();
        let bstack = calculate_bstack(&p, &clients);
        let grid = calculate_grid(&p, &clients);

        assert_eq!(bstack.len(), grid.len());
        assert_eq!(bstack[0].rect, grid[0].rect);
    }

    #[test]
    fn test_centered_master_portrait_uses_top_bottom_split() {
        let p = LayoutParams {
            screen_area: Rect::new(0, 0, 900, 1600),
            n_master: 1,
            m_fact: 0.55,
            gap: 10,
        };
        let clients: Vec<_> = (0..4).map(|i| client(i, 1.0)).collect();
        let result = calculate_centered_master(&p, &clients);

        assert_eq!(result.len(), 4);
        assert!(result[0].rect.y < result[1].rect.y);
        assert_eq!(result[0].rect.x, result[1].rect.x);
    }

    #[test]
    fn test_centered_master_uses_column_factors() {
        let p = LayoutParams {
            screen_area: Rect::new(0, 0, 1600, 900),
            n_master: 2,
            m_fact: 0.5,
            gap: 0,
        };
        let clients = [
            client(1, 2.0),
            client(2, 1.0),
            client(3, 3.0),
            client(4, 1.0),
            client(5, 1.0),
        ];
        let result = calculate_centered_master(&p, &clients);

        assert_eq!(result.len(), 5);
        let by_key = |key| result.iter().find(|res| res.key == key).unwrap().rect;
        assert!(by_key(1).h > by_key(2).h);
        assert!(by_key(3).h > by_key(5).h);
    }

    #[test]
    fn test_centered_master_zero_master_falls_back_to_grid() {
        let p = LayoutParams {
            screen_area: Rect::new(0, 0, 1200, 800),
            n_master: 0,
            m_fact: 0.55,
            gap: 8,
        };
        let clients: Vec<_> = (0..4).map(|i| client(i, 1.0)).collect();
        let centered = calculate_centered_master(&p, &clients);
        let grid = calculate_grid(&p, &clients);

        assert_eq!(centered.len(), grid.len());
        assert_eq!(centered[0].rect, grid[0].rect);
    }

    #[test]
    fn test_deck_offsets_stack_previews() {
        let p = params(1200, 800);
        let clients: Vec<_> = (0..4).map(|i| client(i, 1.0)).collect();
        let result = calculate_deck(&p, &clients);

        assert_eq!(result.len(), 4);
        assert!(result[2].rect.x > result[1].rect.x);
        assert!(result[2].rect.y > result[1].rect.y);
    }

    #[test]
    fn test_deck_master_uses_client_factors() {
        let p = LayoutParams {
            screen_area: Rect::new(0, 0, 1200, 800),
            n_master: 2,
            m_fact: 0.55,
            gap: 0,
        };
        let clients = [client(1, 2.0), client(2, 1.0), client(3, 1.0)];
        let result = calculate_deck(&p, &clients);

        assert_eq!(result.len(), 3);
        assert!(result[0].rect.h > result[1].rect.h);
    }

    #[test]
    fn test_deck_portrait_uses_top_master_bottom_deck() {
        let p = LayoutParams {
            screen_area: Rect::new(0, 0, 900, 1600),
            n_master: 1,
            m_fact: 0.5,
            gap: 10,
        };
        let clients: Vec<_> = (0..3).map(|i| client(i, 1.0)).collect();
        let result = calculate_deck(&p, &clients);

        assert_eq!(result.len(), 3);
        assert!(result[0].rect.y < result[1].rect.y);
        assert!(result[2].rect.x > result[1].rect.x);
    }

    #[test]
    fn test_deck_zero_master_stays_on_screen() {
        let p = LayoutParams {
            screen_area: Rect::new(0, 0, 900, 700),
            n_master: 0,
            m_fact: 0.55,
            gap: 8,
        };
        let clients: Vec<_> = (0..3).map(|i| client(i, 1.0)).collect();
        let result = calculate_deck(&p, &clients);

        assert_eq!(result.len(), 3);
        for res in result {
            assert!(res.rect.x >= 0);
            assert!(res.rect.y >= 0);
            assert!(res.rect.x + res.rect.w <= 900);
            assert!(res.rect.y + res.rect.h <= 700);
        }
    }

    #[test]
    fn test_three_col_portrait_uses_top_bottom_split() {
        let p = LayoutParams {
            screen_area: Rect::new(0, 0, 900, 1600),
            n_master: 1,
            m_fact: 0.55,
            gap: 10,
        };
        let clients: Vec<_> = (0..4).map(|i| client(i, 1.0)).collect();
        let result = calculate_three_col(&p, &clients);

        assert_eq!(result.len(), 4);
        assert!(result[0].rect.y < result[1].rect.y);
        assert_eq!(result[0].rect.x, result[1].rect.x);
    }

    #[test]
    fn test_three_col_uses_column_factors() {
        let p = LayoutParams {
            screen_area: Rect::new(0, 0, 1600, 900),
            n_master: 2,
            m_fact: 0.5,
            gap: 0,
        };
        let clients = [
            client(1, 2.0),
            client(2, 1.0),
            client(3, 3.0),
            client(4, 1.0),
            client(5, 1.0),
        ];
        let result = calculate_three_col(&p, &clients);

        assert_eq!(result.len(), 5);
        let by_key = |key| result.iter().find(|res| res.key == key).unwrap().rect;
        assert!(by_key(1).h > by_key(2).h);
        assert!(by_key(3).h > by_key(5).h);
    }

    #[test]
    fn test_three_col_zero_master_falls_back_to_grid() {
        let p = LayoutParams {
            screen_area: Rect::new(0, 0, 1200, 800),
            n_master: 0,
            m_fact: 0.55,
            gap: 8,
        };
        let clients: Vec<_> = (0..4).map(|i| client(i, 1.0)).collect();
        let three_col = calculate_three_col(&p, &clients);
        let grid = calculate_grid(&p, &clients);

        assert_eq!(three_col.len(), grid.len());
        assert_eq!(three_col[0].rect, grid[0].rect);
    }

    #[test]
    fn test_tatami_dense_falls_back_to_adaptive_grid() {
        let p = params(1600, 900);
        let clients: Vec<_> = (0..12).map(|i| client(i, 1.0)).collect();
        let tatami = calculate_tatami(&p, &clients);
        let grid = calculate_grid(&p, &clients);

        assert_eq!(tatami.len(), grid.len());
        assert_eq!(tatami[0].rect, grid[0].rect);
    }

    #[test]
    fn test_layouts_keep_positive_rects_with_huge_gap() {
        let p = LayoutParams {
            screen_area: Rect::new(0, 0, 120, 90),
            n_master: 1,
            m_fact: 0.55,
            gap: 80,
        };
        let clients: Vec<_> = (0..4).map(|i| client(i, 1.0)).collect();
        let layouts = [
            calculate_tile(&p, &clients),
            calculate_monocle(&p, &clients),
            calculate_fibonacci(&p, &clients),
            calculate_centered_master(&p, &clients),
            calculate_bstack(&p, &clients),
            calculate_grid(&p, &clients),
            calculate_deck(&p, &clients),
            calculate_three_col(&p, &clients),
            calculate_tatami(&p, &clients),
            calculate_vstack(&p, &clients),
        ];

        for layout in layouts {
            assert_eq!(layout.len(), clients.len());
            for res in layout {
                assert!(res.rect.w > 0 && res.rect.h > 0);
            }
        }
    }

    #[test]
    fn extreme_client_borders_collapse_safely_to_one_pixel() {
        let p = params(1920, 1080);
        let clients = [LayoutClient {
            key: 1,
            factor: 1.0,
            border_w: i32::MAX,
        }];

        let result = calculate_monocle(&p, &clients);

        assert_eq!(result[0].rect, Rect::new(0, 0, 1, 1));
    }

    // -----------------------------------------------------------------------
    // calculate_vstack
    // -----------------------------------------------------------------------

    #[test]
    fn test_vstack_single_fills_screen() {
        let p = params(1920, 1080);
        let clients = [client(1, 1.0)];
        let result = calculate_vstack(&p, &clients);
        assert_eq!(result.len(), 1);
        let r = result[0].rect;
        assert!(r.w > 0 && r.h > 0);
    }

    #[test]
    fn test_vstack_two_stacks_vertically() {
        let p = LayoutParams {
            screen_area: Rect::new(0, 0, 1920, 1080),
            n_master: 1,
            m_fact: 0.5,
            gap: 0,
        };
        let clients = [client(1, 1.0), client(2, 1.0)];
        let result = calculate_vstack(&p, &clients);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_vstack_many_windows_shrink() {
        let p = params(1920, 1080);
        let few_clients: Vec<_> = (0..5).map(|i| client(i, 1.0)).collect();
        let many_clients: Vec<_> = (0..10).map(|i| client(i, 1.0)).collect();

        let few = calculate_vstack(&p, &few_clients);
        let many = calculate_vstack(&p, &many_clients);

        assert!(many[0].rect.w < few[0].rect.w);
        assert!(many[0].rect.h < few[0].rect.h);
    }

    // -----------------------------------------------------------------------
    // calculate_scrolling
    // -----------------------------------------------------------------------

    #[test]
    fn test_scrolling_column_uses_client_factors() {
        let p = ScrollingParams {
            screen_area: Rect::new(0, 0, 1000, 600),
            column_width_ratio: 0.5,
            column_width_factors: Vec::new(),
            gap: 0,
            viewport_x: 0.0,
        };
        let columns = vec![vec![client(1, 2.0), client(2, 1.0)]];

        let (result, _) = calculate_scrolling(&p, &columns, 0);

        assert_eq!(result.len(), 2);
        assert!(
            result[0].rect.h > result[1].rect.h,
            "larger factor should receive more column height"
        );
        assert!((result[0].rect.h - 398).abs() <= 2);
        assert!((result[1].rect.h - 198).abs() <= 2);
    }

    #[test]
    fn test_scrolling_centers_focused_column() {
        let p = ScrollingParams {
            screen_area: Rect::new(0, 0, 1000, 600),
            column_width_ratio: 0.5,
            column_width_factors: Vec::new(),
            gap: 10,
            viewport_x: 0.0,
        };
        let columns = vec![vec![client(1, 1.0)], vec![client(2, 1.0)]];

        let (result, viewport_x) = calculate_scrolling(&p, &columns, 1);

        assert!((viewport_x - 260.0).abs() < 1e-6);
        let focused = result.iter().find(|res| res.key == 2).unwrap().rect;
        assert_eq!(focused.x, 250);
    }

    #[test]
    fn test_scrolling_supports_per_column_widths() {
        let p = ScrollingParams {
            screen_area: Rect::new(0, 0, 1000, 600),
            column_width_ratio: 0.4,
            column_width_factors: vec![1.0, 1.5, 0.5],
            gap: 10,
            viewport_x: 0.0,
        };
        let columns = vec![
            vec![client(1, 1.0)],
            vec![client(2, 1.0)],
            vec![client(3, 1.0)],
        ];

        let (result, _) = calculate_scrolling(&p, &columns, 0);

        let first = result.iter().find(|res| res.key == 1).unwrap().rect;
        let second = result.iter().find(|res| res.key == 2).unwrap().rect;
        let third = result.iter().find(|res| res.key == 3).unwrap().rect;
        assert_eq!(first.w, 398);
        assert_eq!(second.w, 598);
        assert_eq!(third.w, 198);
        assert_eq!(second.x - first.x, 410);
        assert_eq!(third.x - second.x, 610);
    }

    #[test]
    fn test_scrolling_centers_variable_width_focused_column() {
        let p = ScrollingParams {
            screen_area: Rect::new(0, 0, 1000, 600),
            column_width_ratio: 0.4,
            column_width_factors: vec![1.0, 1.5],
            gap: 10,
            viewport_x: 0.0,
        };
        let columns = vec![vec![client(1, 1.0)], vec![client(2, 1.0)]];

        let (result, viewport_x) = calculate_scrolling(&p, &columns, 1);

        assert!((viewport_x - 210.0).abs() < 1e-6);
        let focused = result.iter().find(|res| res.key == 2).unwrap().rect;
        assert_eq!(focused.x, 200);
    }

    #[test]
    fn canonical_layouts_tolerate_extreme_gaps() {
        let clients: Vec<_> = (0..10).map(|key| client(key, 1.0)).collect();

        for gap in [i32::MAX, i32::MIN] {
            let params = LayoutParams {
                screen_area: Rect::new(100, 200, 320, 240),
                n_master: 1,
                m_fact: 0.55,
                gap,
            };

            for layout in LayoutEnum::all() {
                let results = match layout.0 {
                    "tile" => calculate_tile(&params, &clients),
                    "float" => continue,
                    "monocle" => calculate_monocle(&params, &clients),
                    "fibonacci" => calculate_fibonacci(&params, &clients),
                    "centeredmaster" => calculate_centered_master(&params, &clients),
                    "bstack" => calculate_bstack(&params, &clients),
                    "grid" => calculate_grid(&params, &clients),
                    "deck" => calculate_deck(&params, &clients),
                    "threecol" => calculate_three_col(&params, &clients),
                    "tatami" => calculate_tatami(&params, &clients),
                    "fullscreen" => calculate_fullscreen(&params, &clients),
                    "scrolling" => continue,
                    "vstack" => calculate_vstack(&params, &clients),
                    unknown => panic!("missing extreme-gap coverage for {unknown}"),
                };

                assert_eq!(results.len(), clients.len(), "{} with gap {gap}", layout.0);
                assert!(
                    results
                        .iter()
                        .all(|result| result.rect.w > 0 && result.rect.h > 0),
                    "{} produced a non-positive rectangle with gap {gap}",
                    layout.0
                );
            }
        }
    }

    #[test]
    fn scrolling_tolerates_extreme_gaps() {
        let columns = vec![
            vec![client(1, 1.0), client(2, 1.0)],
            vec![client(3, 1.0), client(4, 1.0)],
            vec![client(5, 1.0), client(6, 1.0)],
        ];

        for gap in [i32::MAX, i32::MIN] {
            let params = ScrollingParams {
                screen_area: Rect::new(100, 200, 320, 240),
                column_width_ratio: 0.5,
                column_width_factors: vec![1.0, 1.5, 0.5],
                gap,
                viewport_x: 0.0,
            };

            let (results, viewport_x) = calculate_scrolling(&params, &columns, 1);
            assert_eq!(results.len(), 6, "gap {gap}");
            assert!(viewport_x.is_finite(), "gap {gap}");
            assert!(
                results
                    .iter()
                    .all(|result| result.rect.w > 0 && result.rect.h > 0),
                "gap {gap} produced a non-positive rectangle"
            );
        }
    }

    #[test]
    fn scrolling_saturates_extreme_origins_offsets_and_borders() {
        let extreme_client = LayoutClient {
            key: 1,
            factor: 1.0,
            border_w: i32::MAX,
        };
        let columns = vec![vec![extreme_client; 4], vec![extreme_client; 4]];
        let params = ScrollingParams {
            screen_area: Rect::new(i32::MAX - 4, i32::MAX - 4, i32::MAX, i32::MAX),
            column_width_ratio: 1.0,
            column_width_factors: Vec::new(),
            gap: i32::MAX,
            viewport_x: 0.0,
        };

        let (results, viewport_x) = calculate_scrolling(&params, &columns, 1);

        assert_eq!(results.len(), 8);
        assert!(viewport_x.is_finite());
        assert!(results.iter().all(|result| {
            result.rect.y == i32::MAX && result.rect.w == 1 && result.rect.h == 1
        }));
    }
}
