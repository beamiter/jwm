// Concrete layout algorithm implementations
//
// Every layout follows the same three-step pipeline:
//   1. collect  — gather tileable clients and monitor parameters
//   2. compute  — hand them to the pure engine in core::layout
//   3. apply    — write the computed rects back verbatim
//
// The engine's output is authoritative: the shell never clamps or
// second-guesses the rects a layout produced.

use crate::backend::api::Backend;
use crate::config::CONFIG;
use crate::core::layout::{
    self as core_layout, LayoutClient, LayoutParams, LayoutResult, ScrollingParams,
};
use crate::core::models::{ClientKey, MonitorKey, ScrollingState};
use crate::core::types::Rect;
use crate::jwm::Jwm;
use log::info;
use std::collections::HashSet;

impl Jwm {
    /// Step 1 of the layout pipeline: monitor parameters plus the tileable
    /// clients, with smart borders/gaps already applied. `None` means there
    /// is nothing to lay out.
    fn layout_inputs(
        &mut self,
        mon_key: MonitorKey,
    ) -> Option<(LayoutParams, Vec<(ClientKey, f32, i32)>)> {
        let (wx, wy, ww, wh, mfact, nmaster, _monitor_num, _client_y_offset) =
            self.get_monitor_info(mon_key);

        // 优先使用 statusbar 的真实几何
        let screen_area = self
            .monitor_work_area(mon_key)
            .unwrap_or(Rect::new(wx, wy, ww, wh));

        let raw_clients = self.collect_tileable_clients(mon_key);
        if raw_clients.is_empty() {
            return None;
        }

        let (_effective_border, effective_gap) = self.apply_smart_borders(mon_key, &raw_clients);

        Some((
            LayoutParams {
                screen_area,
                n_master: nmaster,
                m_fact: mfact,
                gap: effective_gap,
            },
            raw_clients,
        ))
    }

    /// Convert collected clients into the pure engine's input structure.
    fn to_layout_clients(&self, raw: &[(ClientKey, f32, i32)]) -> Vec<LayoutClient<ClientKey>> {
        let default_border = CONFIG.load().border_px() as i32;
        raw.iter()
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
            .collect()
    }

    /// Step 3: apply the engine's rects as computed. The only adjustment is
    /// the user-facing `resize_hints` option (honoured inside
    /// `apply_size_hints_constraints`); positions are never clamped — some
    /// layouts (e.g. scrolling) place windows off-screen by design.
    fn apply_layout_results(
        &mut self,
        backend: &mut dyn Backend,
        results: Vec<LayoutResult<ClientKey>>,
    ) {
        for res in results {
            let (mut w, mut h) = (res.rect.w.max(1), res.rect.h.max(1));
            let _ = self.apply_size_hints_constraints(backend, res.key, &mut w, &mut h);
            let _ = self.resizeclient(backend, res.key, res.rect.x, res.rect.y, w, h);
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
        let Some((params, raw_clients)) = self.layout_inputs(mon_key) else {
            return;
        };
        let layout_clients = self.to_layout_clients(&raw_clients);
        let results = calc_fn(&params, &layout_clients);
        self.apply_layout_results(backend, results);
    }

    pub(crate) fn tile(&mut self, backend: &mut dyn Backend, mon_key: MonitorKey) {
        self.tiling_layout_wrapper(backend, mon_key, "tile", core_layout::calculate_tile);
    }

    pub(crate) fn fibonacci(&mut self, backend: &mut dyn Backend, mon_key: MonitorKey) {
        self.tiling_layout_wrapper(backend, mon_key, "fibonacci", core_layout::calculate_fibonacci);
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
        self.tiling_layout_wrapper(
            backend,
            mon_key,
            "three_col",
            core_layout::calculate_three_col,
        );
    }

    pub(crate) fn tatami(&mut self, backend: &mut dyn Backend, mon_key: MonitorKey) {
        self.tiling_layout_wrapper(backend, mon_key, "tatami", core_layout::calculate_tatami);
    }

    pub(crate) fn vstack(&mut self, backend: &mut dyn Backend, mon_key: MonitorKey) {
        info!("[vstack] via pure layout engine");
        let Some((params, raw_clients)) = self.layout_inputs(mon_key) else {
            return;
        };

        // vstack's design puts the focused client at clients[0] (the main
        // card at the bottom of the V); reorder before handing off.
        let sel_key = self.state.monitors.get(mon_key).and_then(|m| m.sel);
        let mut ordered = raw_clients;
        if let Some(sk) = sel_key {
            if let Some(pos) = ordered.iter().position(|&(k, _, _)| k == sk) {
                let item = ordered.remove(pos);
                ordered.insert(0, item);
            }
        }

        let layout_clients = self.to_layout_clients(&ordered);
        let results = core_layout::calculate_vstack(&params, &layout_clients);
        self.apply_layout_results(backend, results);
    }

    pub(crate) fn scrolling(&mut self, backend: &mut dyn Backend, mon_key: MonitorKey) {
        info!("[scrolling] via pure layout engine");
        let Some((params, raw_clients)) = self.layout_inputs(mon_key) else {
            return;
        };

        let default_border = CONFIG.load().border_px() as i32;

        let visible_keys: Vec<ClientKey> = raw_clients.iter().map(|&(k, _, _)| k).collect();
        let default_column_widths = visible_keys
            .iter()
            .filter_map(|&key| {
                self.scrolling_default_column_width_for_client(key)
                    .map(|width| (key, width))
            })
            .collect::<std::collections::HashMap<_, _>>();

        let sel = self.state.monitors.get(mon_key).and_then(|m| m.sel);
        let (columns_keys, column_width_factors, focus_col, viewport_x) = {
            let state = match self.scrolling_state_for_monitor_mut_or_default(mon_key) {
                Some(state) => state,
                None => return,
            };

            // Sync columns with currently visible clients
            let newly_inserted = Self::sync_scrolling_columns(state, &visible_keys);
            for key in newly_inserted {
                if let Some(width_factor) = default_column_widths.get(&key).copied() {
                    if let Some(col_idx) = state.columns.iter().position(|col| col.contains(&key)) {
                        state.column_width_factors[col_idx] = width_factor;
                    }
                }
            }

            // Determine focus column and keep the per-column focus memory aligned
            // with normal focus changes outside explicit scrolling commands.
            let focus_col = if let Some(sel_key) = sel {
                state.remember_focus(sel_key);
                state.focused_column_index().unwrap_or(0)
            } else {
                state.focused_column_index().unwrap_or(0)
            };

            (
                state.columns.clone(),
                state.column_width_factors.clone(),
                focus_col,
                state.viewport_x,
            )
        };

        // Build layout clients grouped by column
        let columns: Vec<Vec<LayoutClient<ClientKey>>> = columns_keys
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

        let scroll_params = ScrollingParams {
            screen_area: params.screen_area,
            column_width_ratio: params.m_fact,
            column_width_factors,
            gap: params.gap,
            viewport_x,
        };

        let (results, new_vp_x) =
            core_layout::calculate_scrolling(&scroll_params, &columns, focus_col);

        // Update viewport
        if let Some(state) = self.scrolling_state_for_monitor_mut(mon_key) {
            state.viewport_x = new_vp_x;
        }

        self.apply_layout_results(backend, results);
    }

    pub(crate) fn sync_scrolling_columns(
        state: &mut ScrollingState,
        visible_clients: &[ClientKey],
    ) -> Vec<ClientKey> {
        state.ensure_column_metadata();

        // 1. Remove clients that are no longer visible
        for col in &mut state.columns {
            col.retain(|k| visible_clients.contains(k));
        }

        // 2. Remove empty columns while preserving width factors for retained columns
        state.retain_non_empty_columns();

        // 3. Find new clients not in any column
        let existing: HashSet<ClientKey> = state.columns.iter().flatten().copied().collect();
        let new_clients: Vec<ClientKey> = visible_clients
            .iter()
            .filter(|k| !existing.contains(k))
            .copied()
            .collect();

        // 4. Insert new clients as individual columns (at the end)
        for key in new_clients.iter().copied() {
            state.insert_new_client(key);
        }

        new_clients
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
        for &(key, _, _) in &raw_clients {
            if let Some(client) = self.state.clients.get_mut(key) {
                client.geometry.border_w = 0;
            }
        }
        let layout_clients: Vec<LayoutClient<ClientKey>> = raw_clients
            .iter()
            .map(|&(key, factor, _)| LayoutClient {
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
        self.apply_layout_results(backend, results);
    }

    pub(crate) fn monocle(&mut self, backend: &mut dyn Backend, mon_key: MonitorKey) {
        info!("[monocle] via pure layout engine");
        let (wx, wy, ww, wh, _, _, _monitor_num, _client_y_offset) =
            self.get_monitor_info(mon_key);

        // The layout symbol counts every visible client, floating included.
        let visible_count = self
            .state
            .monitor_clients
            .get(mon_key)
            .map(|keys| {
                keys.iter()
                    .filter(|&&ck| self.is_client_visible_on_monitor(ck, mon_key))
                    .count()
            })
            .unwrap_or(0);
        if visible_count > 0 {
            if let Some(monitor) = self.state.monitors.get_mut(mon_key) {
                monitor.lt_symbol = format!("[{}]", visible_count);
            }
        }

        let raw_clients = self.collect_tileable_clients(mon_key);
        if raw_clients.is_empty() {
            return;
        }
        // Smart borders: a single window gets none. Monocle uses no gap by design.
        let (_effective_border, _gap) = self.apply_smart_borders(mon_key, &raw_clients);

        let layout_clients = self.to_layout_clients(&raw_clients);
        let screen_area = self
            .monitor_work_area(mon_key)
            .unwrap_or(Rect::new(wx, wy, ww, wh));
        let params = LayoutParams {
            screen_area,
            n_master: 0, // 不相关
            m_fact: 0.0, // 不相关
            gap: 0,      // monocle 不使用 gap
        };
        let results = core_layout::calculate_monocle(&params, &layout_clients);
        self.apply_layout_results(backend, results);
    }
}
