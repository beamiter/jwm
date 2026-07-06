// src/core/models.rs

use crate::backend::api::LayerSurfaceInfo;
use crate::backend::common_define::WindowId;
use crate::core::layout::LayoutEnum;
use slotmap::DefaultKey;
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

        let old_columns = std::mem::take(&mut self.columns);
        let old_widths = std::mem::take(&mut self.column_width_factors);
        let old_focuses = std::mem::take(&mut self.focused_clients);
        let mut retained_old_indices = Vec::new();

        for (idx, col) in old_columns.into_iter().enumerate() {
            if col.is_empty() {
                continue;
            }

            let focus = old_focuses
                .get(idx)
                .copied()
                .flatten()
                .filter(|key| col.contains(key))
                .or_else(|| col.first().copied());
            retained_old_indices.push(idx);
            self.columns.push(col);
            self.column_width_factors
                .push(old_widths.get(idx).copied().unwrap_or(1.0));
            self.focused_clients.push(focus);
        }

        self.focused_column = self
            .focused_column
            .and_then(|old_idx| old_column_index_after_retain(old_idx, &retained_old_indices))
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
}

fn old_column_index_after_retain(old_idx: usize, retained_old_indices: &[usize]) -> Option<usize> {
    retained_old_indices.iter().position(|idx| *idx == old_idx)
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
    pub is_urgent: bool,
    pub never_focus: bool,
    pub old_state: bool,
    pub is_fullscreen: bool,
    pub is_sticky: bool,
    pub is_pip: bool,
    pub is_dock: bool,
    pub is_maximized_vert: bool,
    pub is_maximized_horz: bool,
    pub is_hidden: bool,
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

    pub dock_layer_info: Option<LayerSurfaceInfo>,
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
        self.geometry.w + 2 * self.geometry.border_w
    }

    pub fn total_height(&self) -> i32 {
        self.geometry.h + 2 * self.geometry.border_w
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
    pub sel_lt: usize,
    pub tag_set: [u32; 2],
    pub sel: Option<ClientKey>,
    pub lt: [Rc<LayoutEnum>; 2],
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
    pub sel_lts: Vec<usize>,
    pub lt_idxs: Vec<Vec<Option<Rc<LayoutEnum>>>>,
    pub show_bars: Vec<bool>,
    pub sel: Vec<Option<ClientKey>>,
}

impl Pertag {
    pub fn new(show_bar: bool, tags_length: usize) -> Self {
        let len = tags_length + 1;
        Self {
            cur_tag: 0,
            prev_tag: 0,
            n_masters: vec![0; len],
            m_facts: vec![0.; len],
            gaps: vec![0; len],
            sel_lts: vec![0; len],
            lt_idxs: vec![vec![None; 2]; len],
            show_bars: vec![show_bar; len],
            sel: vec![None; len],
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
            sel_lt: 0,
            tag_set: [0; 2],
            sel: None,
            lt: [Rc::new(LayoutEnum::FIBONACCI), Rc::new(LayoutEnum::TILE)],
            pertag: None,
        }
    }

    pub fn intersect_area(&self, x: i32, y: i32, w: i32, h: i32) -> i32 {
        let geom = &self.geometry;
        std::cmp::max(
            0,
            std::cmp::min(x + w, geom.w_x + geom.w_w) - std::cmp::max(x, geom.w_x),
        ) * std::cmp::max(
            0,
            std::cmp::min(y + h, geom.w_y + geom.w_h) - std::cmp::max(y, geom.w_y),
        )
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
        let new_cur_tag = if tag_mask == !0 {
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

    /// 应用 Pertag 上下文 (logic from: apply_pertag_settings/apply_pertag_settings_for_monitor)
    fn apply_pertag_context(&mut self, new_tag_idx: usize) {
        if let Some(ref mut pertag) = self.pertag {
            pertag.prev_tag = pertag.cur_tag;
            pertag.cur_tag = new_tag_idx;

            // 从 Pertag 恢复布局状态到 Monitor
            self.layout.n_master = pertag.n_masters[new_tag_idx];
            self.layout.m_fact = pertag.m_facts[new_tag_idx];
            self.layout.gap = pertag.gaps[new_tag_idx];
            self.sel_lt = pertag.sel_lts[new_tag_idx];

            if let Some(l0) = &pertag.lt_idxs[new_tag_idx][0] {
                self.lt[0] = l0.clone();
            }
            if let Some(l1) = &pertag.lt_idxs[new_tag_idx][1] {
                self.lt[1] = l1.clone();
            }
        }
        // 更新符号
        self.lt_symbol = self.lt[self.sel_lt].symbol().to_string();
    }

    /// 更新当前 Tag 的布局参数 (当 incnmaster 或 setmfact 时调用)
    pub fn update_current_tag_layout_params(&mut self) {
        if let Some(ref mut pertag) = self.pertag {
            let cur = pertag.cur_tag;
            pertag.n_masters[cur] = self.layout.n_master;
            pertag.m_facts[cur] = self.layout.m_fact;
            pertag.gaps[cur] = self.layout.gap;
        }
    }

    /// 更新当前 Tag 的 Layout 选择
    pub fn update_current_tag_layout_selection(&mut self) {
        if let Some(ref mut pertag) = self.pertag {
            let cur = pertag.cur_tag;
            let sel = self.sel_lt;
            pertag.sel_lts[cur] = sel;
            // 更新 layout 引用
            pertag.lt_idxs[cur][sel] = Some(self.lt[sel].clone());
        }
    }

    /// 获取当前 Tag 记录的选中客户端
    pub fn get_selected_client_for_current_tag(&self) -> Option<ClientKey> {
        self.pertag.as_ref().and_then(|p| p.sel[p.cur_tag])
    }

    /// 设置当前 Tag 的选中客户端
    pub fn set_selected_client_for_current_tag(&mut self, client: Option<ClientKey>) {
        if let Some(ref mut pertag) = self.pertag {
            pertag.sel[pertag.cur_tag] = client;
        }
        self.sel = client;
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
        assert!(p.sel_lts.iter().all(|&s| s == 0));
        assert!(p.sel.iter().all(Option::is_none));
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

        s.retain_non_empty_columns();

        assert_eq!(s.columns, vec![vec![a], vec![b]]);
        assert_eq!(s.column_width_factors, vec![1.25, 1.5]);
        assert_eq!(s.focused_clients, vec![Some(a), Some(b)]);
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
}
