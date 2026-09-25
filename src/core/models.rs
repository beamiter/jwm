// src/core/models.rs

use crate::backend::api::{LayerSurfaceInfo, MaximizeAxes};
use crate::backend::common_define::WindowId;
use crate::core::layout::LayoutEnum;
use crate::core::types::Rect;
use slotmap::DefaultKey;
use std::collections::HashSet;
use std::fmt;
use std::rc::Rc;

pub type ClientKey = DefaultKey;
pub type MonitorKey = DefaultKey;

#[derive(Debug, Clone)]
pub struct ScrollingState {
    pub columns: Vec<Vec<ClientKey>>,
    pub column_width_factors: Vec<f32>,
    pub focused_clients: Vec<Option<ClientKey>>,
    pub focused_column: Option<usize>,
    pub attach_new_windows_to_focused_column: bool,
    pub viewport_x: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollingOverviewGeometry {
    pub client: ClientKey,
    pub column_index: usize,
    pub x_ratio: f32,
    pub width_ratio: f32,
    pub y_ratio: f32,
    pub height_ratio: f32,
    pub focused_column: bool,
}

impl ScrollingState {
    pub fn new() -> Self {
        Self {
            columns: Vec::new(),
            column_width_factors: Vec::new(),
            focused_clients: Vec::new(),
            focused_column: None,
            attach_new_windows_to_focused_column: false,
            viewport_x: 0.0,
        }
    }

    pub fn ensure_column_metadata(&mut self) {
        self.column_width_factors.resize(self.columns.len(), 1.0);
        self.focused_clients.resize(self.columns.len(), None);
        self.column_width_factors.truncate(self.columns.len());
        self.focused_clients.truncate(self.columns.len());
    }

    pub fn remember_focus(&mut self, client_key: ClientKey) {
        self.ensure_column_metadata();
        if let Some(col_idx) = self
            .columns
            .iter()
            .position(|col| col.contains(&client_key))
        {
            self.focused_clients[col_idx] = Some(client_key);
            self.focused_column = Some(col_idx);
        }
    }

    pub fn focused_column_index(&self) -> Option<usize> {
        self.focused_column.filter(|&idx| idx < self.columns.len())
    }

    pub fn set_focused_column(&mut self, col_idx: usize) {
        if col_idx < self.columns.len() {
            self.focused_column = Some(col_idx);
        }
    }

    pub fn target_for_column(&self, col_idx: usize) -> Option<ClientKey> {
        let col = self.columns.get(col_idx)?;
        self.focused_clients
            .get(col_idx)
            .and_then(|focus| focus.filter(|key| col.contains(key)))
            .or_else(|| col.first().copied())
    }

    pub fn retain_non_empty_columns(&mut self) {
        self.ensure_column_metadata();

        let old_focused_column = self.focused_column;
        let mut old_idx = 0;
        let mut retained_len = 0;
        let mut retained_focused_column = None;
        let columns = &mut self.columns;
        let widths = &mut self.column_width_factors;
        let focuses = &mut self.focused_clients;

        // Compact all three parallel vectors in place. This method runs on
        // every scrolling-layout sync; rebuilding the vectors made even the
        // no-change steady state allocate fresh backing storage each time.
        columns.retain(|col| {
            let idx = old_idx;
            old_idx += 1;
            if col.is_empty() {
                return false;
            }

            let width = widths[idx];
            let focus = focuses[idx]
                .filter(|key| col.contains(key))
                .or_else(|| col.first().copied());
            widths[retained_len] = width;
            focuses[retained_len] = focus;
            if old_focused_column == Some(idx) {
                retained_focused_column = Some(retained_len);
            }
            retained_len += 1;
            true
        });
        widths.truncate(retained_len);
        focuses.truncate(retained_len);

        self.focused_column = retained_focused_column
            .or_else(|| {
                self.focused_clients
                    .iter()
                    .position(|focus| focus.is_some())
            })
            .filter(|&idx| idx < self.columns.len());
    }

    pub fn insert_new_client(&mut self, client_key: ClientKey) {
        self.ensure_column_metadata();

        if self.attach_new_windows_to_focused_column {
            if let Some(col_idx) = self.focused_column_index() {
                self.columns[col_idx].push(client_key);
                self.focused_clients[col_idx] = Some(client_key);
                self.focused_column = Some(col_idx);
                return;
            }
        }

        self.columns.push(vec![client_key]);
        self.column_width_factors.push(1.0);
        self.focused_clients.push(Some(client_key));
        self.focused_column = Some(self.columns.len() - 1);
    }

    /// Return visible clients in stable scrolling-strip order: left-to-right
    /// columns first, then any visible clients not yet synced into the strip.
    pub fn ordered_visible_clients(&self, visible_clients: &[ClientKey]) -> Vec<ClientKey> {
        let visible: HashSet<ClientKey> = visible_clients.iter().copied().collect();
        let mut seen = HashSet::new();
        let mut ordered = Vec::with_capacity(visible_clients.len());

        for key in self.columns.iter().flatten().copied() {
            if visible.contains(&key) && seen.insert(key) {
                ordered.push(key);
            }
        }

        for key in visible_clients.iter().copied() {
            if seen.insert(key) {
                ordered.push(key);
            }
        }

        ordered
    }

    /// Return normalized strip geometry for overview rendering. Columns keep
    /// their configured width factors; visible clients not yet synced into a
    /// column are appended as one-window synthetic columns.
    pub fn overview_strip_geometry(
        &self,
        visible_clients: &[ClientKey],
    ) -> Vec<ScrollingOverviewGeometry> {
        let visible: HashSet<ClientKey> = visible_clients.iter().copied().collect();
        let mut seen = HashSet::new();
        let orphan_count = visible_clients
            .iter()
            .copied()
            .filter(|key| !self.columns.iter().any(|column| column.contains(key)))
            .count();
        let column_weights = self
            .columns
            .iter()
            .enumerate()
            .map(|(idx, _)| {
                self.column_width_factors
                    .get(idx)
                    .copied()
                    .unwrap_or(1.0)
                    .max(0.1)
            })
            .collect::<Vec<_>>();
        let total_weight = column_weights.iter().sum::<f32>() + orphan_count as f32;
        if total_weight <= 0.0 {
            return Vec::new();
        }

        let focused_column = self.focused_column_index();
        let mut cursor = 0.0f32;
        let mut geometry = Vec::with_capacity(visible_clients.len());

        for (column_index, column) in self.columns.iter().enumerate() {
            let weight = column_weights.get(column_index).copied().unwrap_or(1.0);
            let column_x = cursor / total_weight;
            let column_width = weight / total_weight;
            cursor += weight;

            let column_visible = column
                .iter()
                .copied()
                .filter(|key| visible.contains(key))
                .collect::<Vec<_>>();
            let row_height = if column_visible.is_empty() {
                1.0
            } else {
                1.0 / column_visible.len() as f32
            };

            for (row_idx, client) in column_visible.into_iter().enumerate() {
                seen.insert(client);
                geometry.push(ScrollingOverviewGeometry {
                    client,
                    column_index,
                    x_ratio: column_x,
                    width_ratio: column_width,
                    y_ratio: row_idx as f32 * row_height,
                    height_ratio: row_height,
                    focused_column: focused_column == Some(column_index),
                });
            }
        }

        let mut synthetic_column_index = self.columns.len();
        for client in visible_clients.iter().copied() {
            if !seen.insert(client) {
                continue;
            }

            let column_x = cursor / total_weight;
            let column_width = 1.0 / total_weight;
            cursor += 1.0;
            geometry.push(ScrollingOverviewGeometry {
                client,
                column_index: synthetic_column_index,
                x_ratio: column_x,
                width_ratio: column_width,
                y_ratio: 0.0,
                height_ratio: 1.0,
                focused_column: false,
            });
            synthetic_column_index += 1;
        }

        geometry
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WMClient {
    pub name: String,
    pub class: String,
    pub instance: String,
    pub win: WindowId,

    pub geometry: ClientGeometry,
    pub size_hints: SizeHints,

    pub state: ClientState,

    pub mon: Option<MonitorKey>,

    /// Legacy compatibility field. The authoritative monitor association is
    /// [`Self::mon`]; code that needs a monitor number must resolve that key
    /// through `WMState::monitors` because monitor numbers can change.
    pub monitor_num: u32,

    /// PID of the process that owns this window (None when unknown / Wayland).
    pub pid: Option<u32>,

    /// If this window is currently swallowing a parent terminal, the swallowed
    /// parent's ClientKey. When this client unmaps, the parent is restored.
    pub swallowing: Option<ClientKey>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ClientGeometry {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub old_x: i32,
    pub old_y: i32,
    pub old_w: i32,
    pub old_h: i32,
    pub border_w: i32,
    pub old_border_w: i32,

    /// Exact off-screen x chosen by JWM while this client is hidden. Keeping
    /// an explicit marker avoids mistaking legitimate windows on a monitor
    /// with a negative global origin and lets topology changes preserve the
    /// first visible restore slot while the client is parked.
    pub hidden_x: Option<i32>,

    /// Visible geometry to restore after JWM parks the real window off-screen.
    ///
    /// This is deliberately separate from `old_*`: `resizeclient` uses those
    /// fields as the previous layout/fullscreen rectangle. Reusing `old_x` for
    /// hiding corrupts a fullscreen client's return position when it is
    /// minimized before leaving fullscreen.
    pub hidden_restore_rect: Option<Rect>,

    /// Pre-maximize content rectangle (same convention as x/y/w/h). `Some`
    /// exactly while a maximize axis is set. Owned by `Jwm::set_client_maximized`
    /// and its helpers; output migration translates it and the arrange refit
    /// re-syncs its free axes. Never shares storage with `old_*`
    /// (resizeclient/fullscreen), `floating_*` (floating/PiP) or
    /// `hidden_restore_rect` (parking).
    pub maximize_restore_rect: Option<Rect>,

    pub floating_x: i32,
    pub floating_y: i32,
    pub floating_w: i32,
    pub floating_h: i32,
}

impl fmt::Display for ClientGeometry {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}x{}+{}+{}", self.w, self.h, self.x, self.y)
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SizeHints {
    pub base_w: i32,
    pub base_h: i32,
    pub inc_w: i32,
    pub inc_h: i32,
    pub max_w: i32,
    pub max_h: i32,
    pub min_w: i32,
    pub min_h: i32,
    pub min_aspect: f32,
    pub max_aspect: f32,
    pub hints_valid: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ClientState {
    pub tags: u32,
    pub client_fact: f32,
    pub is_fixed: bool,
    pub is_floating: bool,
    /// True when this client floats only because the user dragged/resized it out
    /// of the tiling grid. Re-applying a layout pulls those back under
    /// management, while rule- or type-driven floats keep floating.
    pub is_drag_floating: bool,
    pub is_urgent: bool,
    pub never_focus: bool,
    pub old_state: bool,
    pub is_fullscreen: bool,
    pub is_sticky: bool,
    pub is_pip: bool,
    /// Sticky state to reinstate when PiP exits. PiP temporarily forces a
    /// client to be sticky, but a client that was already sticky must remain
    /// so afterwards.
    pub pip_restore_sticky: bool,
    pub is_dock: bool,
    /// True for a managed `_NET_WM_WINDOW_TYPE_DESKTOP` window (a desktop
    /// icon layer or wallpaper client). Like a dock it carries every tag bit
    /// and never takes focus, so window lists treat it as shell chrome rather
    /// than workspace content.
    pub is_desktop: bool,
    pub is_maximized_vert: bool,
    pub is_maximized_horz: bool,
    /// Maximize pulled this client out of the layout (togglemaximize on a tiled
    /// window, or an admitted maximize under the FLOAT layout). Clearing the
    /// last axis re-tiles it. Implies at least one maximize axis.
    pub maximize_restore_tiled: bool,
    /// The client this one preceded in its monitor's tiled order when
    /// maximize promoted it: the next tile, or a window promoted from between
    /// the two that is still out of the layout (None: nothing followed it).
    /// Re-tiling puts it back in front of that client's slot while that
    /// client is still in the same list. Meaningful only while
    /// `maximize_restore_tiled` is set; an in-place unmaximize keeps it so a
    /// cancelled drag, which reinstates the promotion, re-tiles into the
    /// same slot.
    pub maximize_restore_anchor: Option<ClientKey>,
    pub is_hidden: bool,
    /// Session-local insertion order in the minimized-window Dock. Zero means
    /// the client is not currently represented there.
    pub minimized_order: u64,
    /// True when this client is a terminal that has been "swallowed" by a child
    /// process (e.g. mpv launched from a shell). Excluded from arrange and from
    /// visibility queries until the swallowing child unmaps.
    pub is_swallowed: bool,
    pub is_above: bool,
    pub is_below: bool,
    pub demands_attention: bool,
    pub skip_taskbar: bool,
    pub skip_pager: bool,
    pub no_decorations: bool,
    pub sync_counter: Option<u32>,
    pub sync_value: u64,
    /// True for a regular, non-transient, non-popup top-level: the kind of
    /// window whose monitor and tags are worth remembering when it closes,
    /// so the next window of the same application can return there. See
    /// `jwm::closed_placement`.
    pub remembers_closed_placement: bool,

    pub dock_layer_info: Option<LayerSurfaceInfo>,
}

impl ClientState {
    /// `(is_maximized_vert, is_maximized_horz)`.
    pub fn maximized_axes(&self) -> MaximizeAxes {
        MaximizeAxes::new(self.is_maximized_vert, self.is_maximized_horz)
    }

    /// Write both maximize axes at once.
    pub fn set_maximized_axes(&mut self, axes: MaximizeAxes) {
        self.is_maximized_vert = axes.vert;
        self.is_maximized_horz = axes.horz;
    }

    /// A maximize axis is set and maximize currently owns the live geometry.
    pub fn is_maximize_realized(&self) -> bool {
        self.maximized_axes().any() && self.is_floating && !self.is_fullscreen && !self.is_pip
    }
}

impl WMClient {
    pub fn new(win: WindowId) -> Self {
        Self {
            name: String::new(),
            class: String::new(),
            instance: String::new(),
            win,
            geometry: ClientGeometry::default(),
            size_hints: SizeHints::default(),
            state: ClientState::default(),
            mon: None,
            monitor_num: 1000,
            pid: None,
            swallowing: None,
        }
    }

    pub fn total_width(&self) -> i32 {
        self.geometry
            .w
            .saturating_add(self.geometry.border_w.saturating_mul(2))
    }

    pub fn total_height(&self) -> i32 {
        self.geometry
            .h
            .saturating_add(self.geometry.border_w.saturating_mul(2))
    }

    pub fn is_status_bar(&self, status_bar_name: &str) -> bool {
        self.name == status_bar_name
            || self.class == status_bar_name
            || self.instance == status_bar_name
    }

    pub fn rect(&self) -> (i32, i32, i32, i32) {
        (
            self.geometry.x,
            self.geometry.y,
            self.geometry.w,
            self.geometry.h,
        )
    }
}

impl fmt::Display for WMClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "WMClient {{ name: \"{}\", class: \"{}\", instance: \"{}\", win: {:?}, geometry: {}, monitor: {} }}",
            self.name,
            self.class,
            self.instance,
            self.win,
            self.geometry,
            if self.mon.is_some() { "Some" } else { "None" }
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WMMonitor {
    pub num: i32,
    pub lt_symbol: String,
    pub layout: MonitorLayout,
    pub geometry: MonitorGeometry,
    pub sel_tags: usize,
    pub tag_set: [u32; 2],
    pub sel: Option<ClientKey>,
    pub lt: Rc<LayoutEnum>,
    pub prev_lt: Rc<LayoutEnum>,
    pub pertag: Option<Pertag>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MonitorLayout {
    pub m_fact: f32,
    pub n_master: u32,
    pub gap: i32,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct MonitorGeometry {
    pub m_x: i32,
    pub m_y: i32,
    pub m_w: i32,
    pub m_h: i32,
    pub w_x: i32,
    pub w_y: i32,
    pub w_w: i32,
    pub w_h: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pertag {
    pub cur_tag: usize,
    pub prev_tag: usize,
    pub n_masters: Vec<u32>,
    pub m_facts: Vec<f32>,
    pub gaps: Vec<i32>,
    pub lts: Vec<Rc<LayoutEnum>>,
    pub prev_lts: Vec<Rc<LayoutEnum>>,
    pub show_bars: Vec<bool>,
    pub sel: Vec<Option<ClientKey>>,
}

impl Pertag {
    pub fn new(show_bar: bool, tags_length: usize) -> Self {
        let len = tags_length + 1;
        // Layout descriptors are immutable, so all tags can intentionally
        // share the default instances until a tag selects another layout.
        let default_layout = Rc::new(LayoutEnum::FIBONACCI);
        let default_prev_layout = Rc::new(LayoutEnum::TILE);
        Self {
            cur_tag: 0,
            prev_tag: 0,
            n_masters: vec![0; len],
            m_facts: vec![0.; len],
            gaps: vec![0; len],
            lts: vec![default_layout; len],
            prev_lts: vec![default_prev_layout; len],
            show_bars: vec![show_bar; len],
            sel: vec![None; len],
        }
    }

    /// Whether `mask` selects every tag this monitor has — "view all". The
    /// `view` command masks its `!0` argument down to the configured tags
    /// first, so testing for `!0` alone never matched and "view all" kept
    /// (and overwrote) the previous tag's layout slot. A single-tag setup
    /// has no separate "all" view.
    pub fn is_all_tags(&self, mask: u32) -> bool {
        let tags = self.sel.len().saturating_sub(1);
        if mask == !0 {
            return true;
        }
        if tags < 2 {
            return false;
        }
        let all = if tags >= 32 { u32::MAX } else { (1u32 << tags) - 1 };
        mask & all == all
    }

    /// The slot a tag mask addresses: 0 for "view all", else its lowest tag.
    pub fn slot_for_mask(&self, mask: u32) -> usize {
        if self.is_all_tags(mask) {
            0
        } else {
            self.clamp_tag(mask.trailing_zeros() as usize + 1)
        }
    }

    /// Slot 0 is "view all"; 1..=tags_length are the numbered tags.
    pub fn clamp_tag(&self, idx: usize) -> usize {
        match self.sel.len() {
            0 => 0,
            n => idx.min(n - 1),
        }
    }

    /// Grow or shrink every per-tag vec to `tags_length + 1`. New slots copy
    /// the current layout so a live `tags_length` reload can view the new
    /// tags without indexing off the end of a createmon-sized block.
    pub fn resize_for_tags(
        &mut self,
        tags_length: usize,
        n_master: u32,
        m_fact: f32,
        gap: i32,
        lt: Rc<LayoutEnum>,
        prev_lt: Rc<LayoutEnum>,
        show_bar: bool,
    ) {
        let tags_length = tags_length.clamp(1, 31);
        let len = tags_length + 1;
        if self.sel.len() == len {
            return;
        }
        self.n_masters.resize(len, n_master);
        self.m_facts.resize(len, m_fact);
        self.gaps.resize(len, gap);
        self.lts.resize(len, lt);
        self.prev_lts.resize(len, prev_lt);
        self.show_bars.resize(len, show_bar);
        self.sel.resize(len, None);
        if self.cur_tag >= len {
            self.cur_tag = tags_length;
        }
        if self.prev_tag >= len {
            self.prev_tag = self.cur_tag;
        }
    }
}

impl Default for MonitorLayout {
    fn default() -> Self {
        Self {
            m_fact: 0.55, // 默认主区域比例
            n_master: 1,  // 默认主窗口数量
            gap: 0,       // 默认间距，createmon 时从配置覆盖
        }
    }
}

impl WMMonitor {
    pub fn new() -> Self {
        Self {
            num: 0,
            lt_symbol: String::new(),
            layout: MonitorLayout {
                m_fact: 0.55,
                n_master: 1,
                gap: 0,
            },
            geometry: MonitorGeometry::default(),
            sel_tags: 0,
            tag_set: [0; 2],
            sel: None,
            lt: Rc::new(LayoutEnum::FIBONACCI),
            prev_lt: Rc::new(LayoutEnum::TILE),
            pertag: None,
        }
    }

    pub fn intersect_area(&self, x: i32, y: i32, w: i32, h: i32) -> i32 {
        let geom = &self.geometry;
        // Geometry can legitimately sit near either end of the signed
        // coordinate space. Compute both far edges and the area in i64 so an
        // extreme output/window cannot overflow before the overlap is clamped.
        let (x, y, w, h) = (i64::from(x), i64::from(y), i64::from(w), i64::from(h));
        let (wx, wy, ww, wh) = (
            i64::from(geom.w_x),
            i64::from(geom.w_y),
            i64::from(geom.w_w),
            i64::from(geom.w_h),
        );
        let overlap_w = std::cmp::min(x + w, wx + ww) - std::cmp::max(x, wx);
        let overlap_h = std::cmp::min(y + h, wy + wh) - std::cmp::max(y, wy);
        let area = overlap_w.max(0).saturating_mul(overlap_h.max(0));

        area.min(i64::from(i32::MAX)) as i32
    }

    /// 切换到指定标签，返回新的 cur_tag 索引
    /// logic from: switch_to_tag & update_tagset_and_pertag
    pub fn view_tag(&mut self, target_tag_mask: u32, toggle: bool) -> usize {
        let tag_mask = if toggle {
            self.tag_set[self.sel_tags] ^ target_tag_mask
        } else {
            target_tag_mask
        };

        // 避免切换到空标签
        if tag_mask == 0 {
            return self.pertag.as_ref().map(|p| p.cur_tag).unwrap_or(1);
        }

        self.sel_tags ^= 1; // 切换当前激活的 tagset 索引 (0 或 1)
        self.tag_set[self.sel_tags] = tag_mask;

        // 计算新的 cur_tag 索引 (用于 Pertag)
        let new_cur_tag = if self.pertag.as_ref().is_some_and(|p| p.is_all_tags(tag_mask)) {
            // 查看所有标签
            0
        } else {
            // 如果是单个标签，直接取索引 + 1
            // 如果是多个标签，且包含当前 Pertag 的标签，保持不变
            // 否则取第一个激活的标签
            let current_cur_tag = self.pertag.as_ref().map(|p| p.cur_tag).unwrap_or(1);

            if current_cur_tag > 0 && (tag_mask & (1 << (current_cur_tag - 1))) > 0 {
                current_cur_tag
            } else {
                // trailing_zeros 得到 0..31，加1对应 1..32
                (tag_mask.trailing_zeros() as usize) + 1
            }
        };

        self.apply_pertag_context(new_cur_tag);
        new_cur_tag
    }

    /// 把当前 tag 的 Pertag 状态重新加载到 monitor 上。
    ///
    /// 用于在不切换 tag 的情况下改写了 Pertag（例如启动时从配置恢复每个 tag
    /// 的布局）之后，让 monitor 的 `layout`/`lt`/`prev_lt`/`lt_symbol` 与之
    /// 一致；否则显示的仍是初始化时的默认布局。
    pub fn reload_current_tag_context(&mut self) {
        let cur_tag = self.pertag.as_ref().map(|p| p.cur_tag).unwrap_or(0);
        self.apply_pertag_context(cur_tag);
    }

    /// 应用 Pertag 上下文 (logic from: apply_pertag_settings/apply_pertag_settings_for_monitor)
    fn apply_pertag_context(&mut self, new_tag_idx: usize) {
        if let Some(ref mut pertag) = self.pertag {
            let idx = pertag.clamp_tag(new_tag_idx);
            pertag.prev_tag = pertag.cur_tag;
            pertag.cur_tag = idx;

            // 从 Pertag 恢复布局状态到 Monitor
            if let Some(&n_master) = pertag.n_masters.get(idx) {
                self.layout.n_master = n_master;
            }
            if let Some(&m_fact) = pertag.m_facts.get(idx) {
                self.layout.m_fact = m_fact;
            }
            if let Some(&gap) = pertag.gaps.get(idx) {
                self.layout.gap = gap;
            }
            if let Some(lt) = pertag.lts.get(idx) {
                self.lt = lt.clone();
            }
            if let Some(prev) = pertag.prev_lts.get(idx) {
                self.prev_lt = prev.clone();
            }
        }
        // 更新符号
        self.lt_symbol = self.lt.symbol().to_string();
    }

    /// 更新当前 Tag 的布局参数 (当 incnmaster 或 setmfact 时调用)
    pub fn update_current_tag_layout_params(&mut self) {
        if let Some(ref mut pertag) = self.pertag {
            let cur = pertag.clamp_tag(pertag.cur_tag);
            if let Some(slot) = pertag.n_masters.get_mut(cur) {
                *slot = self.layout.n_master;
            }
            if let Some(slot) = pertag.m_facts.get_mut(cur) {
                *slot = self.layout.m_fact;
            }
            if let Some(slot) = pertag.gaps.get_mut(cur) {
                *slot = self.layout.gap;
            }
        }
    }

    /// 获取当前 Tag 记录的选中客户端
    pub fn get_selected_client_for_current_tag(&self) -> Option<ClientKey> {
        self.pertag
            .as_ref()
            .and_then(|p| p.sel.get(p.clamp_tag(p.cur_tag)).copied().flatten())
    }

    /// Pre-select `client` on the lowest tag of `tag_mask` without touching
    /// the current view's selection, so that viewing the tag opens on it.
    /// The all-tags mask addresses the "view everything" slot.
    pub fn set_selected_client_for_tag_mask(&mut self, tag_mask: u32, client: Option<ClientKey>) {
        if tag_mask == 0 {
            return;
        }
        let Some(pertag) = self.pertag.as_mut() else {
            return;
        };
        let index = pertag.slot_for_mask(tag_mask);
        if let Some(slot) = pertag.sel.get_mut(index) {
            *slot = client;
        }
    }

    /// 设置当前 Tag 的选中客户端
    pub fn set_selected_client_for_current_tag(&mut self, client: Option<ClientKey>) {
        if let Some(ref mut pertag) = self.pertag {
            let cur = pertag.clamp_tag(pertag.cur_tag);
            if let Some(slot) = pertag.sel.get_mut(cur) {
                *slot = client;
            }
        }
        self.sel = client;
    }

    /// Keep this monitor's Pertag and tag_set aligned with a live
    /// `layout.tags_length` change. New slots copy the current layout;
    /// retired high tags are masked out so the selection cannot point at a
    /// tag the config no longer has.
    pub fn sync_tag_slots(&mut self, tags_length: usize, tagmask: u32) {
        if let Some(ref mut pertag) = self.pertag {
            let show_bar = pertag.show_bars.first().copied().unwrap_or(true);
            pertag.resize_for_tags(
                tags_length,
                self.layout.n_master,
                self.layout.m_fact,
                self.layout.gap,
                self.lt.clone(),
                self.prev_lt.clone(),
                show_bar,
            );
        }
        self.tag_set[0] &= tagmask;
        self.tag_set[1] &= tagmask;
        let idx = self.sel_tags & 1;
        if self.tag_set[idx] == 0 {
            if let Some(ref mut pertag) = self.pertag {
                if pertag.cur_tag == 0 || pertag.cur_tag > tags_length {
                    pertag.cur_tag = 1.min(tags_length);
                }
                let bit = 1u32.checked_shl((pertag.cur_tag - 1) as u32).unwrap_or(0);
                self.tag_set[idx] = bit & tagmask;
                if self.tag_set[idx] == 0 {
                    self.tag_set[idx] = 1;
                    pertag.cur_tag = 1;
                }
            } else {
                self.tag_set[idx] = 1;
            }
        }
        self.reload_current_tag_context();
    }

    /// 清除该显示器对某 client 的所有"上次选中"记录(monitor.sel 及全部 per-tag
    /// pertag.sel)。当 client 被移动到别的显示器或销毁时调用,否则切回某个 tag 时
    /// 会读到一个已不属于本显示器的陈旧 key,导致焦点错误地跳到其它屏。
    pub fn clear_selection_of(&mut self, client: ClientKey) {
        if self.sel == Some(client) {
            self.sel = None;
        }
        if let Some(ref mut pertag) = self.pertag {
            for slot in pertag.sel.iter_mut() {
                if *slot == Some(client) {
                    *slot = None;
                }
            }
        }
    }

    /// 安全地获取当前活跃的 tag_set 值
    /// 确保 sel_tags 在有效范围内 [0, 1]，防止数组越界
    pub fn get_active_tags(&self) -> u32 {
        let safe_idx = self.sel_tags & 1; // 位与 1 确保索引只能是 0 或 1
        self.tag_set[safe_idx]
    }
}

impl fmt::Display for WMMonitor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "WMMonitor {{ num: {}, geometry: {:?}, sel: {} }}",
            self.num,
            self.geometry,
            self.sel.is_some()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::common_define::WindowId;

    fn win(id: u64) -> WindowId {
        WindowId::from_raw(id)
    }

    // -----------------------------------------------------------------------
    // WMClient
    // -----------------------------------------------------------------------

    #[test]
    fn test_wm_client_new_defaults() {
        let c = WMClient::new(win(1));
        assert_eq!(c.name, "");
        assert_eq!(c.class, "");
        assert_eq!(c.win, win(1));
        assert!(c.mon.is_none());
        assert_eq!(c.monitor_num, 1000);
    }

    #[test]
    fn test_wm_client_total_width() {
        let mut c = WMClient::new(win(1));
        c.geometry.w = 800;
        c.geometry.border_w = 2;
        assert_eq!(c.total_width(), 804); // 800 + 2*2
    }

    #[test]
    fn total_dimensions_saturate_instead_of_overflowing_a_huge_border() {
        let mut c = WMClient::new(win(1));
        c.geometry.w = 800;
        c.geometry.h = 600;
        c.geometry.border_w = i32::MAX;
        assert_eq!(c.total_width(), i32::MAX);
        assert_eq!(c.total_height(), i32::MAX);
    }

    #[test]
    fn test_wm_client_total_height() {
        let mut c = WMClient::new(win(1));
        c.geometry.h = 600;
        c.geometry.border_w = 3;
        assert_eq!(c.total_height(), 606); // 600 + 2*3
    }

    #[test]
    fn test_wm_client_total_dimensions_zero_border() {
        let mut c = WMClient::new(win(1));
        c.geometry.w = 1920;
        c.geometry.h = 1080;
        c.geometry.border_w = 0;
        assert_eq!(c.total_width(), 1920);
        assert_eq!(c.total_height(), 1080);
    }

    #[test]
    fn test_wm_client_is_status_bar_by_name() {
        let mut c = WMClient::new(win(1));
        c.name = "mybar".to_string();
        assert!(c.is_status_bar("mybar"));
        assert!(!c.is_status_bar("other"));
    }

    #[test]
    fn test_wm_client_is_status_bar_by_class() {
        let mut c = WMClient::new(win(1));
        c.class = "xbar".to_string();
        assert!(c.is_status_bar("xbar"));
    }

    #[test]
    fn test_wm_client_is_status_bar_by_instance() {
        let mut c = WMClient::new(win(1));
        c.instance = "polybar".to_string();
        assert!(c.is_status_bar("polybar"));
    }

    #[test]
    fn test_wm_client_is_not_status_bar() {
        let c = WMClient::new(win(1));
        assert!(!c.is_status_bar("xbar"));
    }

    #[test]
    fn test_wm_client_rect() {
        let mut c = WMClient::new(win(1));
        c.geometry.x = 10;
        c.geometry.y = 20;
        c.geometry.w = 800;
        c.geometry.h = 600;
        assert_eq!(c.rect(), (10, 20, 800, 600));
    }

    #[test]
    fn test_client_geometry_display() {
        let g = ClientGeometry {
            w: 1280,
            h: 720,
            x: 100,
            y: 50,
            ..Default::default()
        };
        let s = format!("{g}");
        assert!(s.contains("1280"), "expected width in display: {s}");
        assert!(s.contains("720"), "expected height in display: {s}");
        assert!(s.contains("100"), "expected x in display: {s}");
        assert!(s.contains("50"), "expected y in display: {s}");
    }

    // -----------------------------------------------------------------------
    // WMMonitor
    // -----------------------------------------------------------------------

    #[test]
    fn test_wm_monitor_new_defaults() {
        let m = WMMonitor::new();
        assert_eq!(m.num, 0);
        assert!(m.sel.is_none());
        assert_eq!(m.sel_tags, 0);
        assert_eq!(m.tag_set, [0u32; 2]);
    }

    #[test]
    fn test_wm_monitor_get_active_tags_default() {
        let m = WMMonitor::new();
        // tag_set starts at [0, 0], sel_tags=0 → active tags = 0
        assert_eq!(m.get_active_tags(), 0);
    }

    #[test]
    fn test_wm_monitor_get_active_tags_safe_index() {
        let mut m = WMMonitor::new();
        m.tag_set[0] = 0b0001;
        m.tag_set[1] = 0b0010;
        m.sel_tags = 0;
        assert_eq!(m.get_active_tags(), 0b0001);
        m.sel_tags = 1;
        assert_eq!(m.get_active_tags(), 0b0010);
    }

    #[test]
    fn test_wm_monitor_get_active_tags_bad_sel_tags_clamped() {
        let mut m = WMMonitor::new();
        m.tag_set[0] = 0xFF;
        m.tag_set[1] = 0x0F;
        m.sel_tags = 100; // out of range — masked to 0 via & 1
        // 100 & 1 = 0 → tag_set[0]
        assert_eq!(m.get_active_tags(), 0xFF);
    }

    #[test]
    fn test_wm_monitor_intersect_area_fully_inside() {
        let mut m = WMMonitor::new();
        m.geometry.w_x = 0;
        m.geometry.w_y = 0;
        m.geometry.w_w = 1920;
        m.geometry.w_h = 1080;
        // Window fully inside
        let area = m.intersect_area(100, 100, 200, 200);
        assert_eq!(area, 200 * 200);
    }

    #[test]
    fn test_wm_monitor_intersect_area_no_overlap() {
        let mut m = WMMonitor::new();
        m.geometry.w_x = 0;
        m.geometry.w_y = 0;
        m.geometry.w_w = 1920;
        m.geometry.w_h = 1080;
        // Window completely to the right of monitor
        let area = m.intersect_area(2000, 0, 100, 100);
        assert_eq!(area, 0);
    }

    #[test]
    fn test_wm_monitor_intersect_area_partial_overlap() {
        let mut m = WMMonitor::new();
        m.geometry.w_x = 0;
        m.geometry.w_y = 0;
        m.geometry.w_w = 100;
        m.geometry.w_h = 100;
        // Window overlaps the right half of the monitor
        let area = m.intersect_area(50, 0, 100, 100);
        assert_eq!(area, 50 * 100);
    }

    #[test]
    fn test_wm_monitor_intersect_area_edge_touch() {
        let mut m = WMMonitor::new();
        m.geometry.w_x = 0;
        m.geometry.w_y = 0;
        m.geometry.w_w = 100;
        m.geometry.w_h = 100;
        // Window starts at the right edge → no overlap
        let area = m.intersect_area(100, 0, 100, 100);
        assert_eq!(area, 0);
    }

    #[test]
    fn test_wm_monitor_intersect_area_handles_extreme_coordinates() {
        let mut m = WMMonitor::new();
        m.geometry.w_x = i32::MAX - 10;
        m.geometry.w_y = i32::MIN;
        m.geometry.w_w = 20;
        m.geometry.w_h = 20;

        let area = m.intersect_area(i32::MAX - 5, i32::MIN + 5, 20, 20);
        assert_eq!(area, 15 * 15);
    }

    #[test]
    fn test_wm_monitor_intersect_area_saturates_large_area() {
        let mut m = WMMonitor::new();
        m.geometry.w_x = i32::MIN;
        m.geometry.w_y = i32::MIN;
        m.geometry.w_w = i32::MAX;
        m.geometry.w_h = i32::MAX;

        let area = m.intersect_area(i32::MIN, i32::MIN, i32::MAX, i32::MAX);
        assert_eq!(area, i32::MAX);
    }

    #[test]
    fn test_wm_monitor_view_tag_no_toggle() {
        let mut m = WMMonitor::new();
        m.tag_set[0] = 0b0001;
        let new_tag = m.view_tag(0b0010, false);
        // After view_tag, sel_tags flips (0→1)
        let active = m.tag_set[m.sel_tags];
        assert_eq!(active, 0b0010);
        // Returns cur_tag (trailing zeros of 0b0010 = 1, +1 = 2)
        assert_eq!(new_tag, 2);
    }

    #[test]
    fn test_wm_monitor_view_tag_empty_mask_is_noop() {
        let mut m = WMMonitor::new();
        m.tag_set[0] = 0b0101;
        let initial_sel = m.sel_tags;
        m.view_tag(0, false);
        // Empty mask (0) after no-toggle → same tag_set
        assert_eq!(m.sel_tags, initial_sel);
    }

    #[test]
    fn test_wm_monitor_update_current_tag_layout_params_no_pertag() {
        let mut m = WMMonitor::new();
        m.layout.m_fact = 0.7;
        m.layout.n_master = 2;
        // Without pertag, this should be a no-op (no panic)
        m.update_current_tag_layout_params();
    }

    #[test]
    fn test_wm_monitor_set_get_selected_client_no_pertag() {
        let mut m = WMMonitor::new();
        // Without pertag, set_selected_client_for_current_tag updates sel only
        m.set_selected_client_for_current_tag(None);
        assert!(m.get_selected_client_for_current_tag().is_none());
    }

    // -----------------------------------------------------------------------
    // Pertag
    // -----------------------------------------------------------------------

    #[test]
    fn test_pertag_new_correct_length() {
        let p = Pertag::new(true, 9); // 9 tags → len=10
        assert_eq!(p.n_masters.len(), 10);
        assert_eq!(p.m_facts.len(), 10);
        assert_eq!(p.show_bars.len(), 10);
        assert_eq!(p.sel.len(), 10);
    }

    #[test]
    fn test_pertag_new_show_bar_propagated() {
        let p = Pertag::new(true, 4);
        assert!(p.show_bars.iter().all(|&b| b));
        let p2 = Pertag::new(false, 4);
        assert!(p2.show_bars.iter().all(|&b| !b));
    }

    #[test]
    fn test_pertag_new_initial_values_zero() {
        let p = Pertag::new(false, 4);
        assert!(p.n_masters.iter().all(|&n| n == 0));
        assert!(p.m_facts.iter().all(|&f| f == 0.0));
        assert!(p.sel.iter().all(Option::is_none));
    }

    #[test]
    fn pertag_grows_and_preserves_existing_slots() {
        let mut sm: slotmap::SlotMap<ClientKey, ()> = slotmap::SlotMap::new();
        let key = sm.insert(());
        let mut p = Pertag::new(true, 9);
        p.cur_tag = 3;
        p.n_masters[3] = 4;
        p.sel[3] = Some(key);
        p.resize_for_tags(
            12,
            1,
            0.55,
            8,
            p.lts[0].clone(),
            p.prev_lts[0].clone(),
            true,
        );
        assert_eq!(p.sel.len(), 13);
        assert_eq!(p.n_masters[3], 4);
        assert_eq!(p.sel[3], Some(key));
        assert_eq!(p.n_masters[12], 1);
        assert_eq!(p.gaps[12], 8);
        assert_eq!(p.cur_tag, 3);
    }

    #[test]
    fn pertag_shrink_clamps_cur_tag_to_the_new_last_slot() {
        let mut p = Pertag::new(true, 12);
        p.cur_tag = 12;
        p.prev_tag = 11;
        p.resize_for_tags(9, 1, 0.55, 0, p.lts[0].clone(), p.prev_lts[0].clone(), true);
        assert_eq!(p.sel.len(), 10);
        assert_eq!(p.cur_tag, 9);
        assert_eq!(p.prev_tag, 9);
    }

    #[test]
    fn viewing_every_configured_tag_uses_the_view_all_slot() {
        let mut m = WMMonitor::new();
        m.pertag = Some(Pertag::new(true, 9));
        if let Some(pertag) = m.pertag.as_mut() {
            pertag.cur_tag = 1;
            pertag.n_masters[0] = 3;
            pertag.n_masters[1] = 1;
        }
        m.tag_set[0] = 1;
        // `view` hands over `!0 & tagmask`, never `!0` itself.
        assert_eq!(m.view_tag(0x1ff, false), 0, "view all takes slot 0");
        assert_eq!(m.layout.n_master, 3, "and slot 0's settings");
        assert_eq!(m.view_tag(1, false), 1);
        assert_eq!(m.layout.n_master, 1, "tag 1 kept its own settings");
        // A partial multi-tag view keeps a numbered slot.
        assert_eq!(m.view_tag(0b11, false), 1);

        let pertag = m.pertag.as_ref().unwrap();
        assert_eq!(pertag.slot_for_mask(0x1ff), 0);
        assert_eq!(pertag.slot_for_mask(!0), 0);
        assert_eq!(pertag.slot_for_mask(0b100), 3);

        // With a single tag there is no separate "all" view.
        let single = Pertag::new(true, 1);
        assert!(!single.is_all_tags(1));
        assert_eq!(single.slot_for_mask(1), 1);
    }

    #[test]
    fn sync_tag_slots_lets_view_reach_a_newly_added_tag() {
        let mut m = WMMonitor::new();
        m.pertag = Some(Pertag::new(true, 9));
        if let Some(pertag) = m.pertag.as_mut() {
            pertag.cur_tag = 1;
            for i in 0..=9 {
                pertag.n_masters[i] = 1;
            }
        }
        m.tag_set[0] = 1;
        m.sync_tag_slots(12, (1 << 12) - 1);
        let new_tag = m.view_tag(1 << 11, false);
        assert_eq!(new_tag, 12);
        assert_eq!(m.pertag.as_ref().unwrap().n_masters.len(), 13);
        assert_eq!(m.layout.n_master, 1);
    }

    #[test]
    fn sync_tag_slots_masks_a_retired_high_tag_onto_a_live_one() {
        let mut m = WMMonitor::new();
        m.pertag = Some(Pertag::new(true, 12));
        if let Some(pertag) = m.pertag.as_mut() {
            pertag.cur_tag = 12;
        }
        m.tag_set[0] = 1 << 11;
        m.sync_tag_slots(9, (1 << 9) - 1);
        assert_eq!(m.pertag.as_ref().unwrap().sel.len(), 10);
        assert_eq!(m.pertag.as_ref().unwrap().cur_tag, 9);
        assert_eq!(m.get_active_tags(), 1 << 8);
    }

    // -----------------------------------------------------------------------
    // ScrollingState
    // -----------------------------------------------------------------------

    #[test]
    fn test_scrolling_state_new() {
        let s = ScrollingState::new();
        assert!(s.columns.is_empty());
        assert!(s.column_width_factors.is_empty());
        assert!(s.focused_clients.is_empty());
        assert!(s.focused_column.is_none());
        assert!(!s.attach_new_windows_to_focused_column);
        assert!((s.viewport_x - 0.0).abs() < 1e-6);
    }

    #[test]
    fn test_scrolling_state_remembers_focus_per_column() {
        let mut keys = slotmap::SlotMap::<ClientKey, ()>::with_key();
        let a = keys.insert(());
        let b = keys.insert(());
        let c = keys.insert(());
        let mut s = ScrollingState::new();
        s.columns = vec![vec![a, b], vec![c]];

        s.remember_focus(b);

        assert_eq!(s.target_for_column(0), Some(b));
        assert_eq!(s.target_for_column(1), Some(c));
        assert_eq!(s.focused_column_index(), Some(0));
    }

    #[test]
    fn test_scrolling_state_retains_column_metadata() {
        let mut keys = slotmap::SlotMap::<ClientKey, ()>::with_key();
        let a = keys.insert(());
        let b = keys.insert(());
        let mut s = ScrollingState::new();
        s.columns = vec![vec![a], Vec::new(), vec![b]];
        s.column_width_factors = vec![1.25, 0.75, 1.5];
        s.focused_clients = vec![Some(a), None, Some(b)];
        s.focused_column = Some(2);

        s.retain_non_empty_columns();

        assert_eq!(s.columns, vec![vec![a], vec![b]]);
        assert_eq!(s.column_width_factors, vec![1.25, 1.5]);
        assert_eq!(s.focused_clients, vec![Some(a), Some(b)]);
        assert_eq!(s.focused_column_index(), Some(1));
    }

    #[test]
    fn test_scrolling_state_steady_retain_reuses_storage() {
        let mut keys = slotmap::SlotMap::<ClientKey, ()>::with_key();
        let mut s = ScrollingState::new();
        for _ in 0..16 {
            let key = keys.insert(());
            s.columns.push(vec![key]);
            s.column_width_factors.push(1.0);
            s.focused_clients.push(Some(key));
        }

        let columns_storage = s.columns.as_ptr();
        let widths_storage = s.column_width_factors.as_ptr();
        let focuses_storage = s.focused_clients.as_ptr();
        let capacities = (
            s.columns.capacity(),
            s.column_width_factors.capacity(),
            s.focused_clients.capacity(),
        );

        s.retain_non_empty_columns();

        assert_eq!(s.columns.as_ptr(), columns_storage);
        assert_eq!(s.column_width_factors.as_ptr(), widths_storage);
        assert_eq!(s.focused_clients.as_ptr(), focuses_storage);
        assert_eq!(
            (
                s.columns.capacity(),
                s.column_width_factors.capacity(),
                s.focused_clients.capacity(),
            ),
            capacities
        );
    }

    #[test]
    fn test_scrolling_state_preserves_focused_column_when_focus_client_disappears() {
        let mut keys = slotmap::SlotMap::<ClientKey, ()>::with_key();
        let a = keys.insert(());
        let b = keys.insert(());
        let c = keys.insert(());
        let mut s = ScrollingState::new();
        s.columns = vec![vec![a], vec![b, c]];
        s.column_width_factors = vec![1.0, 1.5];
        s.focused_clients = vec![Some(a), Some(b)];
        s.focused_column = Some(1);

        s.columns[1].remove(0);
        s.retain_non_empty_columns();

        assert_eq!(s.columns, vec![vec![a], vec![c]]);
        assert_eq!(s.column_width_factors, vec![1.0, 1.5]);
        assert_eq!(s.focused_clients, vec![Some(a), Some(c)]);
        assert_eq!(s.focused_column_index(), Some(1));
        assert_eq!(s.target_for_column(1), Some(c));
    }

    #[test]
    fn test_scrolling_state_attaches_new_client_to_focused_column() {
        let mut keys = slotmap::SlotMap::<ClientKey, ()>::with_key();
        let a = keys.insert(());
        let b = keys.insert(());
        let c = keys.insert(());
        let mut s = ScrollingState::new();
        s.columns = vec![vec![a], vec![b]];
        s.remember_focus(a);
        s.attach_new_windows_to_focused_column = true;

        s.insert_new_client(c);

        assert_eq!(s.columns, vec![vec![a, c], vec![b]]);
        assert_eq!(s.focused_clients, vec![Some(c), None]);
        assert_eq!(s.focused_column_index(), Some(0));
    }

    #[test]
    fn test_scrolling_state_new_client_defaults_to_new_column() {
        let mut keys = slotmap::SlotMap::<ClientKey, ()>::with_key();
        let a = keys.insert(());
        let b = keys.insert(());
        let mut s = ScrollingState::new();
        s.columns = vec![vec![a]];
        s.remember_focus(a);

        s.insert_new_client(b);

        assert_eq!(s.columns, vec![vec![a], vec![b]]);
        assert_eq!(s.focused_column_index(), Some(1));
    }

    #[test]
    fn test_scrolling_state_orders_visible_clients_for_overview() {
        let mut keys = slotmap::SlotMap::<ClientKey, ()>::with_key();
        let a = keys.insert(());
        let b = keys.insert(());
        let c = keys.insert(());
        let d = keys.insert(());
        let e = keys.insert(());
        let mut s = ScrollingState::new();
        s.columns = vec![vec![c, a], vec![b], vec![d]];

        let ordered = s.ordered_visible_clients(&[a, b, e, c]);

        assert_eq!(ordered, vec![c, a, b, e]);
    }

    #[test]
    fn test_scrolling_state_builds_overview_strip_geometry() {
        let mut keys = slotmap::SlotMap::<ClientKey, ()>::with_key();
        let a = keys.insert(());
        let b = keys.insert(());
        let c = keys.insert(());
        let e = keys.insert(());
        let mut s = ScrollingState::new();
        s.columns = vec![vec![c, a], vec![b]];
        s.column_width_factors = vec![2.0, 1.0];
        s.set_focused_column(1);

        let geometry = s.overview_strip_geometry(&[a, b, c, e]);

        assert_eq!(
            geometry.iter().map(|g| g.client).collect::<Vec<_>>(),
            vec![c, a, b, e]
        );
        assert!((geometry[0].x_ratio - 0.0).abs() < 0.0001);
        assert!((geometry[0].width_ratio - 0.5).abs() < 0.0001);
        assert!((geometry[0].y_ratio - 0.0).abs() < 0.0001);
        assert!((geometry[0].height_ratio - 0.5).abs() < 0.0001);
        assert!((geometry[1].y_ratio - 0.5).abs() < 0.0001);
        assert!(!geometry[0].focused_column);
        assert!(geometry[2].focused_column);
        assert!((geometry[3].x_ratio - 0.75).abs() < 0.0001);
        assert_eq!(geometry[3].column_index, 2);
    }

    #[test]
    fn maximized_axes_round_trip_and_realization_requires_a_plain_floating_client() {
        let mut state = ClientState::default();
        assert_eq!(state.maximized_axes(), MaximizeAxes::NONE);
        for axes in [
            MaximizeAxes::VERT,
            MaximizeAxes::HORZ,
            MaximizeAxes::BOTH,
            MaximizeAxes::NONE,
        ] {
            state.set_maximized_axes(axes);
            assert_eq!(state.maximized_axes(), axes);
            assert_eq!(state.is_maximized_vert, axes.vert);
            assert_eq!(state.is_maximized_horz, axes.horz);
        }

        state.set_maximized_axes(MaximizeAxes::VERT);
        assert!(
            !state.is_maximize_realized(),
            "a tiled client is never realized"
        );
        state.is_floating = true;
        assert!(state.is_maximize_realized());
        state.is_fullscreen = true;
        assert!(
            !state.is_maximize_realized(),
            "fullscreen owns the geometry"
        );
        state.is_fullscreen = false;
        state.is_pip = true;
        assert!(!state.is_maximize_realized(), "PiP owns the geometry");
        state.is_pip = false;
        state.set_maximized_axes(MaximizeAxes::NONE);
        assert!(!state.is_maximize_realized());
    }
}
