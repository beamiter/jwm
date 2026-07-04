// src/core/layout.rs
use super::types::Rect;

// 用于布局计算的客户端信息
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

fn usable_area(screen_area: Rect, gap: i32) -> Rect {
    let gap = gap.max(0);
    Rect::new(
        screen_area.x + gap,
        screen_area.y + gap,
        (screen_area.w - 2 * gap).max(1),
        (screen_area.h - 2 * gap).max(1),
    )
}

fn client_rect(x: i32, y: i32, w: i32, h: i32, border_w: i32) -> Rect {
    let border2 = 2 * border_w.max(0);
    Rect::new(x, y, (w - border2).max(1), (h - border2).max(1))
}

fn split_evenly(total: i32, count: i32, gap: i32, used: i32, index: i32) -> i32 {
    let remaining = (total - (count - 1).max(0) * gap - used).max(0);
    let remaining_count = (count - index).max(1);
    (remaining / remaining_count).max(1)
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
    for (i, c) in clients.iter().enumerate() {
        let preview_step = gap.max(6).min(16);
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

    pub fn symbol(&self) -> &str {
        match self.0 {
            "tile" => "[]=",
            "float" => "><>",
            "monocle" => "[M]",
            "fibonacci" => "[@]",
            "centeredmaster" => "|M|",
            "bstack" => "TTT",
            "grid" => "HHH",
            "deck" => "[D]",
            "threecol" => "|||",
            "tatami" => "[+]",
            "fullscreen" => "[ ]",
            "scrolling" => "[S]",
            "vstack" => "V[]",
            _ => "",
        }
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

    /// 所有布局的循环顺序
    const CYCLE: &'static [LayoutEnum] = &[
        Self::TILE,
        Self::FIBONACCI,
        Self::CENTERED_MASTER,
        Self::BSTACK,
        Self::GRID,
        Self::DECK,
        Self::THREE_COL,
        Self::TATAMI,
        Self::MONOCLE,
        Self::FULLSCREEN,
        Self::SCROLLING,
        Self::VSTACK,
        Self::FLOAT,
    ];

    pub fn cycle_next(&self) -> &'static LayoutEnum {
        let idx = Self::CYCLE.iter().position(|l| l == self).unwrap_or(0);
        &Self::CYCLE[(idx + 1) % Self::CYCLE.len()]
    }

    pub fn cycle_prev(&self) -> &'static LayoutEnum {
        let idx = Self::CYCLE.iter().position(|l| l == self).unwrap_or(0);
        &Self::CYCLE[(idx + Self::CYCLE.len() - 1) % Self::CYCLE.len()]
    }
}

impl From<u32> for LayoutEnum {
    fn from(value: u32) -> Self {
        match value {
            0 => LayoutEnum::TILE,
            1 => LayoutEnum::FLOAT,
            2 => LayoutEnum::MONOCLE,
            3 => LayoutEnum::FIBONACCI,
            4 => LayoutEnum::CENTERED_MASTER,
            5 => LayoutEnum::BSTACK,
            6 => LayoutEnum::GRID,
            7 => LayoutEnum::DECK,
            8 => LayoutEnum::THREE_COL,
            9 => LayoutEnum::TATAMI,
            10 => LayoutEnum::FULLSCREEN,
            11 => LayoutEnum::SCROLLING,
            12 => LayoutEnum::VSTACK,
            _ => LayoutEnum::ANY,
        }
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
    let gap = *gap;

    // 外边距：缩小可用区域。gap 过大（> 屏幕一半）时 w/h 会变负，进而让 mw/列宽
    // 变成负数并产生非法窗口尺寸，这里夹到 >= 0。
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
    let gap = *gap;

    // 外边距
    let area = usable_area(*screen_area, gap);
    let (wx, wy, ww, wh) = (area.x, area.y, area.w, area.h);

    let has_stack = n > *n_master;
    let mw = if has_stack {
        ((ww - gap) as f32 * m_fact) as i32
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

/// Centered Master: Master 居中，Stack 分列两侧
pub fn calculate_centered_master<K: Copy>(
    params: &LayoutParams,
    clients: &[LayoutClient<K>],
) -> Vec<LayoutResult<K>> {
    let n = clients.len() as u32;
    if n == 0 {
        return Vec::new();
    }

    let mut results = Vec::with_capacity(clients.len());
    let gap = params.gap;

    let area = usable_area(params.screen_area, gap);
    let (wx, wy, ww, wh) = (area.x, area.y, area.w, area.h);

    let n_master = params.n_master;
    let n_master_count = n.min(n_master) as i32;
    let n_stack = (n as i32 - n_master as i32).max(0);

    if n_master == 0 {
        return calculate_grid(params, clients);
    }

    if n_stack == 0 {
        let mut my = 0;
        for (i, c) in clients.iter().enumerate() {
            let h = split_evenly(wh, n_master_count, gap, my, i as i32);
            results.push(LayoutResult {
                key: c.key,
                rect: client_rect(wx, wy + my + i as i32 * gap, ww, h, c.border_w),
            });
            my += h;
        }
        return results;
    }

    if wh > ww {
        return calculate_bstack(params, clients);
    }

    // 左 stack | 中 master | 右 stack
    let mfact = params.m_fact.clamp(0.25, 0.75);
    let side_total = n_stack.max(1) as f32;
    let master_bias = if n_stack <= 2 {
        1.0
    } else {
        (2.0 / side_total).max(0.7)
    };
    let mw = ((ww - 2 * gap) as f32 * mfact * master_bias).max(1.0) as i32;
    let n_left = (n_stack + 1) / 2;
    let n_right = n_stack - n_left;
    let side_w_total = (ww - mw - 2 * gap).max(1);
    let left_w = (side_w_total / 2).max(1);
    let right_w = (side_w_total - left_w).max(1);

    let master_x = wx + left_w + gap;
    let right_x = master_x + mw + gap;

    let master_end = (n_master as usize).min(clients.len());
    let mut left_clients = Vec::with_capacity(n_left as usize);
    let mut right_clients = Vec::with_capacity(n_right as usize);
    for (idx, c) in clients[master_end..].iter().enumerate() {
        if idx % 2 == 0 {
            left_clients.push(LayoutClient {
                key: c.key,
                factor: c.factor,
                border_w: c.border_w,
            });
        } else {
            right_clients.push(LayoutClient {
                key: c.key,
                factor: c.factor,
                border_w: c.border_w,
            });
        }
    }
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
    let gap = params.gap;

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
        push_factor_row(
            &mut results,
            &stack_clients[row_start..row_end],
            wx,
            wy + mh + gap + row * (stack_cell_h + gap),
            ww,
            stack_cell_h,
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
    let gap = params.gap;

    let area = usable_area(params.screen_area, gap);
    let (wx, wy, ww, wh) = (area.x, area.y, area.w, area.h);

    let (cols, rows) = choose_grid_dimensions(n, area);

    let cell_w = (ww - (cols - 1) * gap) / cols;
    let cell_h = (wh - (rows - 1) * gap) / rows;

    for (i, c) in clients.iter().enumerate() {
        let row = i as i32 / cols;
        let col = i as i32 % cols;
        // 最后一行可能不满，拉宽填满
        let (cx, cw) = if row == rows - 1 {
            let last_row_count = n as i32 - row * cols;
            let last_cell_w = (ww - (last_row_count - 1) * gap) / last_row_count;
            let last_col = i as i32 - row * cols;
            (wx + last_col * (last_cell_w + gap), last_cell_w)
        } else {
            (wx + col * (cell_w + gap), cell_w)
        };

        results.push(LayoutResult {
            key: c.key,
            rect: client_rect(cx, wy + row * (cell_h + gap), cw, cell_h, c.border_w),
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
    let gap = params.gap;

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
    let n = clients.len() as u32;
    if n == 0 {
        return Vec::new();
    }

    let mut results = Vec::with_capacity(clients.len());
    let gap = params.gap;

    let area = usable_area(params.screen_area, gap);
    let (wx, wy, ww, wh) = (area.x, area.y, area.w, area.h);

    let n_master = params.n_master;
    let n_master_count = n.min(n_master) as i32;
    let n_stack = (n as i32 - n_master as i32).max(0);

    if n_master == 0 {
        return calculate_grid(params, clients);
    }

    if n_stack == 0 {
        let mut my = 0;
        for (i, c) in clients.iter().enumerate() {
            let h = split_evenly(wh, n_master_count, gap, my, i as i32);
            results.push(LayoutResult {
                key: c.key,
                rect: client_rect(wx, wy + my + i as i32 * gap, ww, h, c.border_w),
            });
            my += h;
        }
        return results;
    }

    if wh > ww {
        return calculate_bstack(params, clients);
    }

    let mfact = params.m_fact.clamp(0.25, 0.75);
    let mw = ((ww - 2 * gap) as f32 * mfact).max(1.0) as i32;
    let side_w_total = (ww - mw - 2 * gap).max(1);
    let side_w = (side_w_total / 2).max(1);
    let right_side_w = (side_w_total - side_w).max(1);

    let n_left = (n_stack + 1) / 2;
    let n_right = n_stack - n_left;

    let master_x = wx + side_w + gap;
    let right_x = master_x + mw + gap;

    let master_end = (n_master as usize).min(clients.len());
    let mut left_clients = Vec::with_capacity(n_left as usize);
    let mut right_clients = Vec::with_capacity(n_right as usize);
    for (idx, c) in clients[master_end..].iter().enumerate() {
        if idx % 2 == 0 {
            left_clients.push(LayoutClient {
                key: c.key,
                factor: c.factor,
                border_w: c.border_w,
            });
        } else {
            right_clients.push(LayoutClient {
                key: c.key,
                factor: c.factor,
                border_w: c.border_w,
            });
        }
    }
    push_factor_column(
        &mut results,
        &clients[..master_end],
        master_x,
        wy,
        mw,
        wh,
        gap,
    );
    push_factor_column(&mut results, &left_clients, wx, wy, side_w, wh, gap);
    push_factor_column(
        &mut results,
        &right_clients,
        right_x,
        wy,
        right_side_w,
        wh,
        gap,
    );

    results
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
    let gap = params.gap;

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

    let gap = params.gap;
    let screen = &params.screen_area;
    let base_col_w = (screen.w as f32 * params.column_width_ratio) as i32;
    let base_col_w = base_col_w.max(1);

    // Outer margin
    let outer_gap = gap;
    let avail_h = (screen.h - 2 * outer_gap).max(0);

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
        x_cursor += col_w;
        if i + 1 < columns.len() {
            x_cursor += gap;
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
        let screen_x = strip_x as f32 - new_viewport_x + screen.x as f32;

        let inner_gaps = (col.len() as i32 - 1).max(0) * gap;
        let avail_col_h = (avail_h - inner_gaps).max(0);
        let mut remaining_fact: f32 = col.iter().map(|client| client.factor.max(0.0)).sum();

        let mut y_cursor = 0;
        for (win_idx, client) in col.iter().enumerate() {
            let remaining = col.len() as i32 - win_idx as i32;
            let remaining_h = (avail_col_h - y_cursor).max(0);
            let client_fact = client.factor.max(0.0);
            let h = if remaining_fact > 0.001 {
                (remaining_h as f32 * (client_fact / remaining_fact)) as i32
            } else {
                remaining_h / remaining.max(1)
            };
            let border2 = 2 * client.border_w;

            let win_y = screen.y + outer_gap + y_cursor + win_idx as i32 * gap;

            results.push(LayoutResult {
                key: client.key,
                rect: Rect::new(
                    screen_x as i32,
                    win_y,
                    (col_w - border2).max(1),
                    (h - border2).max(1),
                ),
            });

            y_cursor += h;
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
    let gap = params.gap;

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
}
