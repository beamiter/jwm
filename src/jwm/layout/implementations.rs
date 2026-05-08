// Concrete layout algorithm implementations

use crate::backend::api::Backend;
use crate::config::CONFIG;
use crate::core::animation::AnimationKind;
use crate::core::layout::{self as core_layout, LayoutClient, LayoutParams, LayoutResult, ScrollingParams};
use crate::core::models::{ClientKey, MonitorKey, ScrollingState};
use crate::core::types::Rect;
use crate::jwm::Jwm;
use log::info;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

impl Jwm {
    pub(crate) fn fibonacci(&mut self, backend: &mut dyn Backend, mon_key: MonitorKey) {
        info!("[fibonacci] via pure layout engine");

        // 1. 获取显示器信息和配置
        let (wx, wy, ww, wh, mfact, nmaster, _monitor_num, _client_y_offset) =
            self.get_monitor_info(mon_key);

        // 计算可用区域 (优先使用 statusbar 的真实几何)
        let screen_area = self
            .monitor_work_area(mon_key)
            .unwrap_or(Rect::new(wx, wy, ww, wh));

        // 2. 收集需要参与布局的客户端 (使用现有的辅助函数)
        let raw_clients = self.collect_tileable_clients(mon_key);
        if raw_clients.is_empty() {
            return;
        }

        let (_effective_border, effective_gap) = self.apply_smart_borders(&raw_clients);
        let default_border = CONFIG.load().border_px() as i32;

        // 转换为 LayoutClient 结构
        let layout_clients: Vec<LayoutClient<ClientKey>> = raw_clients
            .iter()
            .map(|&(key, factor, _)| LayoutClient {
                key,
                factor,
                border_w: self
                    .state
                    .clients
                    .get(key)
                    .map(|c| c.geometry.border_w)
                    .unwrap_or(default_border),
            })
            .collect();

        // 3. 构造参数
        let params = LayoutParams {
            screen_area,
            n_master: nmaster,
            m_fact: mfact,
            gap: effective_gap,
        };

        // 4. 计算布局
        let results = core_layout::calculate_fibonacci(&params, &layout_clients);

        // 5. 应用结果 (调整窗口大小和位置)
        for res in results {
            self.resize_client(
                backend, res.key, res.rect.x, res.rect.y, res.rect.w, res.rect.h, false,
            );
        }
    }

    pub(crate) fn tiling_layout_wrapper(
        &mut self,
        backend: &mut dyn Backend,
        mon_key: MonitorKey,
        name: &str,
        calc_fn: fn(&LayoutParams, &[LayoutClient<ClientKey>]) -> Vec<LayoutResult<ClientKey>>,
    ) {
        info!("[{}] via pure layout engine", name);
        let (wx, wy, ww, wh, mfact, nmaster, _monitor_num, _client_y_offset) =
            self.get_monitor_info(mon_key);

        let screen_area = self
            .monitor_work_area(mon_key)
            .unwrap_or(Rect::new(wx, wy, ww, wh));

        let raw_clients = self.collect_tileable_clients(mon_key);
        if raw_clients.is_empty() {
            return;
        }

        let (_effective_border, effective_gap) = self.apply_smart_borders(&raw_clients);
        let default_border = CONFIG.load().border_px() as i32;

        let layout_clients: Vec<LayoutClient<ClientKey>> = raw_clients
            .iter()
            .map(|&(key, factor, _)| LayoutClient {
                key,
                factor,
                border_w: self
                    .state
                    .clients
                    .get(key)
                    .map(|c| c.geometry.border_w)
                    .unwrap_or(default_border),
            })
            .collect();

        let params = LayoutParams {
            screen_area,
            n_master: nmaster,
            m_fact: mfact,
            gap: effective_gap,
        };

        let results = calc_fn(&params, &layout_clients);

        for res in results {
            self.resize_client(
                backend, res.key, res.rect.x, res.rect.y, res.rect.w, res.rect.h, false,
            );
        }
    }

    pub(crate) fn centered_master(&mut self, backend: &mut dyn Backend, mon_key: MonitorKey) {
        self.tiling_layout_wrapper(
            backend,
            mon_key,
            "centered_master",
            core_layout::calculate_centered_master,
        );
    }

    pub(crate) fn bstack(&mut self, backend: &mut dyn Backend, mon_key: MonitorKey) {
        self.tiling_layout_wrapper(backend, mon_key, "bstack", core_layout::calculate_bstack);
    }

    pub(crate) fn grid(&mut self, backend: &mut dyn Backend, mon_key: MonitorKey) {
        self.tiling_layout_wrapper(backend, mon_key, "grid", core_layout::calculate_grid);
    }

    pub(crate) fn deck(&mut self, backend: &mut dyn Backend, mon_key: MonitorKey) {
        self.tiling_layout_wrapper(backend, mon_key, "deck", core_layout::calculate_deck);
    }

    pub(crate) fn three_col(&mut self, backend: &mut dyn Backend, mon_key: MonitorKey) {
        self.tiling_layout_wrapper(backend, mon_key, "three_col", core_layout::calculate_three_col);
    }

    pub(crate) fn tatami(&mut self, backend: &mut dyn Backend, mon_key: MonitorKey) {
        self.tiling_layout_wrapper(backend, mon_key, "tatami", core_layout::calculate_tatami);
    }

    pub(crate) fn vstack(&mut self, backend: &mut dyn Backend, mon_key: MonitorKey) {
        info!("[vstack] via pure layout engine");

        let cfg = CONFIG.load();
        let (wx, wy, ww, wh, mfact, nmaster, _monitor_num, _client_y_offset) =
            self.get_monitor_info(mon_key);

        let screen_area = self
            .monitor_work_area(mon_key)
            .unwrap_or(Rect::new(wx, wy, ww, wh));

        let raw_clients = self.collect_tileable_clients(mon_key);
        if raw_clients.is_empty() {
            return;
        }

        let (_effective_border, effective_gap) = self.apply_smart_borders(&raw_clients);
        let default_border = cfg.border_px() as i32;

        // Reorder: move the focused client (monitor.sel) to clients[0]
        let sel_key = self.state.monitors.get(mon_key).and_then(|m| m.sel);
        let mut ordered = raw_clients.clone();
        if let Some(sk) = sel_key {
            if let Some(pos) = ordered.iter().position(|&(k, _, _)| k == sk) {
                let item = ordered.remove(pos);
                ordered.insert(0, item);
            }
        }

        let layout_clients: Vec<LayoutClient<ClientKey>> = ordered
            .iter()
            .map(|&(key, factor, _)| LayoutClient {
                key,
                factor,
                border_w: self
                    .state
                    .clients
                    .get(key)
                    .map(|c| c.geometry.border_w)
                    .unwrap_or(default_border),
            })
            .collect();

        let pre_rects: HashMap<ClientKey, Rect> = {
            let now = Instant::now();
            ordered
                .iter()
                .map(|&(key, _, _)| {
                    let visual = self
                        .animations
                        .current_visual_rect(key, now)
                        .or_else(|| {
                            self.state.clients.get(key).map(|c| {
                                Rect::new(c.geometry.x, c.geometry.y, c.geometry.w, c.geometry.h)
                            })
                        })
                        .unwrap_or_default();
                    (key, visual)
                })
                .collect()
        };

        let params = LayoutParams {
            screen_area,
            n_master: nmaster,
            m_fact: mfact,
            gap: effective_gap,
        };

        let results = core_layout::calculate_vstack(&params, &layout_clients);

        let target_rects: Vec<(ClientKey, Rect)> =
            results.iter().map(|res| (res.key, res.rect)).collect();

        for res in results {
            self.resize_client(
                backend, res.key, res.rect.x, res.rect.y, res.rect.w, res.rect.h, false,
            );
        }

        if cfg.animation_enabled() {
            let duration = cfg.animation_duration();
            let easing = cfg.animation_easing();
            for (client_key, target) in target_rects {
                if let Some(pre_rect) = pre_rects.get(&client_key) {
                    if *pre_rect != target {
                        self.animations.start(
                            client_key,
                            *pre_rect,
                            target,
                            duration,
                            easing,
                            AnimationKind::Layout,
                        );
                    }
                }
            }
        }
    }

    pub(crate) fn scrolling(&mut self, backend: &mut dyn Backend, mon_key: MonitorKey) {
        info!("[scrolling] via pure layout engine");

        let (wx, wy, ww, wh, mfact, _nmaster, _monitor_num, _client_y_offset) =
            self.get_monitor_info(mon_key);

        let screen_area = self
            .monitor_work_area(mon_key)
            .unwrap_or(Rect::new(wx, wy, ww, wh));

        let raw_clients = self.collect_tileable_clients(mon_key);
        if raw_clients.is_empty() {
            return;
        }

        let (_effective_border, effective_gap) = self.apply_smart_borders(&raw_clients);
        let default_border = CONFIG.load().border_px() as i32;

        let visible_keys: Vec<ClientKey> = raw_clients.iter().map(|&(k, _, _)| k).collect();

        // Get or create scrolling state
        let state = self
            .scrolling_states
            .entry(mon_key)
            .or_insert_with(ScrollingState::new);

        // Sync columns with currently visible clients
        Self::sync_scrolling_columns(state, &visible_keys);

        // Determine focus column
        let sel = self.state.monitors.get(mon_key).and_then(|m| m.sel);
        let focus_col = sel
            .and_then(|sel_key| state.columns.iter().position(|col| col.contains(&sel_key)))
            .unwrap_or(0);

        // Build layout clients grouped by column
        let columns: Vec<Vec<LayoutClient<ClientKey>>> = state
            .columns
            .iter()
            .map(|col| {
                col.iter()
                    .map(|&key| LayoutClient {
                        key,
                        factor: self
                            .state
                            .clients
                            .get(key)
                            .map(|c| c.state.client_fact)
                            .unwrap_or(1.0),
                        border_w: self
                            .state
                            .clients
                            .get(key)
                            .map(|c| c.geometry.border_w)
                            .unwrap_or(default_border),
                    })
                    .collect()
            })
            .collect();

        let params = ScrollingParams {
            screen_area,
            column_width_ratio: mfact,
            gap: effective_gap,
            viewport_x: state.viewport_x,
        };

        let (results, new_vp_x) = core_layout::calculate_scrolling(&params, &columns, focus_col);

        // Update viewport
        self.scrolling_states.get_mut(&mon_key).unwrap().viewport_x = new_vp_x;

        // Apply results
        for res in results {
            self.resize_client(
                backend, res.key, res.rect.x, res.rect.y, res.rect.w, res.rect.h, false,
            );
        }
    }

    pub(crate) fn sync_scrolling_columns(state: &mut ScrollingState, visible_clients: &[ClientKey]) {
        // 1. Remove clients that are no longer visible
        for col in &mut state.columns {
            col.retain(|k| visible_clients.contains(k));
        }
        // 2. Remove empty columns
        state.columns.retain(|col| !col.is_empty());

        // 3. Find new clients not in any column
        let existing: HashSet<ClientKey> = state.columns.iter().flatten().copied().collect();
        let new_clients: Vec<ClientKey> = visible_clients
            .iter()
            .filter(|k| !existing.contains(k))
            .copied()
            .collect();

        // 4. Insert new clients as individual columns (at the end)
        for key in new_clients {
            state.columns.push(vec![key]);
        }
    }

    pub(crate) fn fullscreen_layout(&mut self, backend: &mut dyn Backend, mon_key: MonitorKey) {
        info!("[fullscreen_layout] via pure layout engine");

        // 使用完整显示器区域 (m_x, m_y, m_w, m_h)，不是 work area
        let (mx, my, mw, mh) = if let Some(monitor) = self.state.monitors.get(mon_key) {
            (
                monitor.geometry.m_x,
                monitor.geometry.m_y,
                monitor.geometry.m_w,
                monitor.geometry.m_h,
            )
        } else {
            return;
        };

        let raw_clients = self.collect_tileable_clients(mon_key);
        if raw_clients.is_empty() {
            return;
        }

        // 全屏模式下 border_w = 0
        let layout_clients: Vec<LayoutClient<ClientKey>> = raw_clients
            .iter()
            .map(|&(key, factor, _border_w)| LayoutClient {
                key,
                factor,
                border_w: 0,
            })
            .collect();

        let params = LayoutParams {
            screen_area: Rect::new(mx, my, mw, mh),
            n_master: 0,
            m_fact: 0.0,
            gap: 0,
        };

        let results = core_layout::calculate_fullscreen(&params, &layout_clients);

        // 临时将 border_w 设为 0，应用布局后恢复
        for &(key, _, _original_border_w) in &raw_clients {
            if let Some(client) = self.state.clients.get_mut(key) {
                client.geometry.border_w = 0;
            }
        }

        for res in results {
            self.resize_client(
                backend, res.key, res.rect.x, res.rect.y, res.rect.w, res.rect.h, false,
            );
        }
    }

    pub(crate) fn tile(&mut self, backend: &mut dyn Backend, mon_key: MonitorKey) {
        info!("[tile] via pure layout engine");

        // 1. 准备数据
        let (wx, wy, ww, wh, mfact, nmaster, _monitor_num, _client_y_offset) =
            self.get_monitor_info(mon_key);

        // 计算可用区域 (优先使用 statusbar 的真实几何)
        let screen_area = self
            .monitor_work_area(mon_key)
            .unwrap_or(Rect::new(wx, wy, ww, wh));

        // 获取需要布局的客户端
        let raw_clients = self.collect_tileable_clients(mon_key);
        if raw_clients.is_empty() {
            return;
        }

        let (_effective_border, effective_gap) = self.apply_smart_borders(&raw_clients);
        let default_border = CONFIG.load().border_px() as i32;

        // 转换为纯数据结构 LayoutClient
        let layout_clients: Vec<LayoutClient<ClientKey>> = raw_clients
            .iter()
            .map(|&(key, factor, _)| LayoutClient {
                key,
                factor,
                border_w: self
                    .state
                    .clients
                    .get(key)
                    .map(|c| c.geometry.border_w)
                    .unwrap_or(default_border),
            })
            .collect();

        // 2. 调用纯计算逻辑 (无副作用)
        let params = LayoutParams {
            screen_area,
            n_master: nmaster,
            m_fact: mfact,
            gap: effective_gap,
        };
        let results = core_layout::calculate_tile(&params, &layout_clients);

        // 3. 应用结果 (执行副作用：移动窗口)
        for res in results {
            self.resize_client(
                backend, res.key, res.rect.x, res.rect.y, res.rect.w, res.rect.h, false,
            );
        }
    }

    pub(crate) fn monocle(&mut self, backend: &mut dyn Backend, mon_key: MonitorKey) {
        info!("[monocle] via pure layout engine");
        let (wx, wy, ww, wh, _, _, monitor_num, _client_y_offset) = self.get_monitor_info(mon_key);
        let mut visible_count = 0u32;
        let mut tiled_keys = Vec::new();
        if let Some(client_keys) = self.state.monitor_clients.get(mon_key) {
            for &client_key in client_keys {
                if let Some(client) = self.state.clients.get(client_key) {
                    let is_visible = self.is_client_visible_on_monitor(client_key, mon_key);

                    if is_visible {
                        visible_count += 1;
                        if !client.state.is_floating {
                            tiled_keys.push(client_key);
                        }
                    }
                }
            }
        }

        let default_border = CONFIG.load().border_px() as i32;
        let effective_border = if tiled_keys.len() == 1 {
            0
        } else {
            default_border
        };
        for &ck in &tiled_keys {
            if let Some(client) = self.state.clients.get_mut(ck) {
                client.geometry.border_w = effective_border;
            }
        }

        let layout_clients: Vec<LayoutClient<ClientKey>> = tiled_keys
            .iter()
            .map(|&key| LayoutClient {
                key,
                factor: 1.0,
                border_w: effective_border,
            })
            .collect();
        if visible_count > 0 {
            let formatted_string = format!("[{}]", visible_count);
            if let Some(monitor) = self.state.monitors.get_mut(mon_key) {
                monitor.lt_symbol = formatted_string.clone();
            }
            info!(
                "[monocle] formatted_string: {}, monitor_num: {}",
                formatted_string, monitor_num
            );
        }
        if layout_clients.is_empty() {
            return;
        }
        let screen_area = self
            .monitor_work_area(mon_key)
            .unwrap_or(Rect::new(wx, wy, ww, wh));
        // 纯计算
        let params = LayoutParams {
            screen_area,
            n_master: 0, // 不相关
            m_fact: 0.0, // 不相关
            gap: 0,      // monocle 不使用 gap
        };
        let results = core_layout::calculate_monocle(&params, &layout_clients);
        // 应用
        for res in results {
            self.resize_client(
                backend, res.key, res.rect.x, res.rect.y, res.rect.w, res.rect.h, false,
            );
        }
    }
}
