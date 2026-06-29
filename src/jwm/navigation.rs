// Navigation and workspace management: client finding, tag switching, and navigation

use std::sync::atomic::Ordering;

use crate::Jwm;
use crate::backend::api::Backend;
use crate::backend::common_define::{EventMaskBits, StdCursorKind};
use crate::config::CONFIG;
use crate::core::models::{ClientKey, MonitorKey};
use crate::jwm::types::WMArgEnum;
use log::{info, warn};

impl Jwm {
    pub(crate) fn can_focus_switch(&self) -> Result<bool, Box<dyn std::error::Error>> {
        let sel_client_key = self.get_selected_client_key().ok_or("No selected client")?;

        if let Some(client) = self.state.clients.get(sel_client_key) {
            let is_locked_fullscreen =
                client.state.is_fullscreen && CONFIG.load().behavior().lock_fullscreen;
            Ok(!is_locked_fullscreen)
        } else {
            Err("Selected client not found".into())
        }
    }

    pub(crate) fn find_next_visible_client(
        &self,
    ) -> Result<Option<ClientKey>, Box<dyn std::error::Error>> {
        let sel_mon_key = self.state.sel_mon.ok_or("No selected monitor")?;
        let current_sel = self.get_selected_client_key().ok_or("No selected client")?;
        let (tile_clients, floating_clients) = self.grouped_visible_clients(sel_mon_key);
        let current_is_floating = self
            .state
            .clients
            .get(current_sel)
            .map(|client| client.state.is_floating)
            .unwrap_or(false);

        let (current_group, other_group) = if current_is_floating {
            (&floating_clients, &tile_clients)
        } else {
            (&tile_clients, &floating_clients)
        };

        if let Some(next) = Self::next_in_group(current_group, current_sel) {
            return Ok(Some(next));
        }

        if let Some(next) = other_group.first().copied() {
            return Ok(Some(next));
        }

        // Wrap around to the first of current group
        if let Some(next) = current_group.first().copied() {
            if next != current_sel {
                return Ok(Some(next));
            }
        }

        Ok(None)
    }

    pub(crate) fn find_previous_visible_client(
        &self,
    ) -> Result<Option<ClientKey>, Box<dyn std::error::Error>> {
        let sel_mon_key = self.state.sel_mon.ok_or("No selected monitor")?;
        let current_sel = self.get_selected_client_key().ok_or("No selected client")?;
        let (tile_clients, floating_clients) = self.grouped_visible_clients(sel_mon_key);
        let current_is_floating = self
            .state
            .clients
            .get(current_sel)
            .map(|client| client.state.is_floating)
            .unwrap_or(false);

        let (current_group, other_group) = if current_is_floating {
            (&floating_clients, &tile_clients)
        } else {
            (&tile_clients, &floating_clients)
        };

        if let Some(prev) = Self::prev_in_group(current_group, current_sel) {
            return Ok(Some(prev));
        }

        if let Some(prev) = other_group.last().copied() {
            return Ok(Some(prev));
        }

        // Wrap around to the last of current group
        if let Some(prev) = current_group.last().copied() {
            if prev != current_sel {
                return Ok(Some(prev));
            }
        }

        Ok(None)
    }

    pub(crate) fn grouped_visible_clients(
        &self,
        mon_key: MonitorKey,
    ) -> (Vec<ClientKey>, Vec<ClientKey>) {
        let total = self
            .state
            .monitor_clients
            .get(mon_key)
            .map(|v| v.len())
            .unwrap_or(0);
        let mut tile_clients = Vec::with_capacity(total);
        let mut floating_clients = Vec::with_capacity(total / 4 + 1);

        if let Some(client_list) = self.state.monitor_clients.get(mon_key) {
            for &client_key in client_list {
                if !self.is_client_visible_on_monitor(client_key, mon_key) {
                    continue;
                }

                if let Some(client) = self.state.clients.get(client_key) {
                    if client.state.is_floating {
                        floating_clients.push(client_key);
                    } else {
                        tile_clients.push(client_key);
                    }
                }
            }
        }

        (tile_clients, floating_clients)
    }

    pub(crate) fn next_in_group(group: &[ClientKey], current_sel: ClientKey) -> Option<ClientKey> {
        group
            .iter()
            .position(|&k| k == current_sel)
            .and_then(|idx| group.get(idx + 1).copied())
    }

    pub(crate) fn prev_in_group(group: &[ClientKey], current_sel: ClientKey) -> Option<ClientKey> {
        group
            .iter()
            .position(|&k| k == current_sel)
            .and_then(|idx| {
                idx.checked_sub(1)
                    .and_then(|prev_idx| group.get(prev_idx).copied())
            })
    }

    pub fn togglebar(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[togglebar]");

        let sel_mon_key = match self.state.sel_mon {
            Some(key) => key,
            None => return Ok(()),
        };

        let mut monitor_num_opt: Option<i32> = None;
        {
            if let Some(monitor) = self.state.monitors.get_mut(sel_mon_key) {
                if let Some(ref mut pertag) = monitor.pertag {
                    let cur_tag = pertag.cur_tag;
                    if let Some(show_bar) = pertag.show_bars.get_mut(cur_tag) {
                        *show_bar = !*show_bar;
                        info!(
                            "[togglebar] show_bar[mon={}, tag={}] -> {}",
                            monitor.num, cur_tag, show_bar
                        );
                        monitor_num_opt = Some(monitor.num);
                    }
                }
            }
        }

        if let Some(mon_num) = monitor_num_opt {
            self.mark_bar_update_needed_if_visible(Some(mon_num));

            // Reposition the status bar window (hide or show) and update its strut.
            let bar_info = self
                .secondary_bars
                .get(&mon_num)
                .and_then(|bar| bar.client_key.zip(bar.window));
            if let Some((client_key, win)) = bar_info {
                let _ = self.position_secondary_bar_on_monitor(backend, client_key, win, mon_num);
            }

            // Re-arrange all windows on this monitor to fill or vacate the bar space.
            self.arrange(backend, Some(sel_mon_key));
        }

        Ok(())
    }

    pub fn setcfact(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[setcfact]");

        // In scrolling layout, Alt+Shift+h/l focuses columns
        if self.is_scrolling_layout() {
            if let WMArgEnum::Float(f) = arg {
                let dir = if *f > 0.0 { -1 } else { 1 };
                return self.scrolling_focus_column(backend, &WMArgEnum::Int(dir));
            }
        }

        let client_key = match self.get_selected_client_key() {
            Some(k) => k,
            None => return Ok(()),
        };

        if let WMArgEnum::Float(f0) = *arg {
            let current_fact = if let Some(client) = self.state.clients.get(client_key) {
                client.state.client_fact
            } else {
                return Ok(());
            };

            let new_fact = if f0.abs() < 0.0001 {
                1.0
            } else {
                f0 + current_fact
            };

            if new_fact < 0.25 || new_fact > 4.0 {
                return Ok(());
            }

            if let Some(client) = self.state.clients.get_mut(client_key) {
                client.state.client_fact = new_fact;
                info!(
                    "[setcfact] Updated client_fact to {} for client '{}'",
                    new_fact, client.name
                );
            }
            self.arrange(backend, self.state.sel_mon);
        }

        Ok(())
    }

    pub fn movestack(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // In scrolling layout, Alt+Shift+j/k moves within column, Alt+Shift+h/l moves columns
        if self.is_scrolling_layout() {
            return self.scrolling_move_column(backend, arg);
        }

        let direction = match arg {
            WMArgEnum::Int(i) => *i,
            _ => return Ok(()),
        };

        let selected_client_key = self.get_selected_client_key().ok_or("No client selected")?;

        let target_client_key = if direction > 0 {
            self.find_next_tiled_client(selected_client_key)?
        } else {
            self.find_previous_tiled_client(selected_client_key)?
        };

        if let Some(target_key) = target_client_key {
            if selected_client_key != target_key {
                self.swap_clients_in_monitor(selected_client_key, target_key)?;

                self.arrange(backend, self.state.sel_mon);

                self.suppress_mouse_focus_until =
                    Some(std::time::Instant::now() + std::time::Duration::from_millis(200));
            }
        }

        Ok(())
    }

    pub(crate) fn is_tiled_and_visible(&self, client_key: ClientKey) -> bool {
        if let Some(client) = self.state.clients.get(client_key) {
            self.is_client_visible_by_key(client_key) && !client.state.is_floating
        } else {
            false
        }
    }

    pub(crate) fn find_next_tiled_client(
        &self,
        current_key: ClientKey,
    ) -> Result<Option<ClientKey>, Box<dyn std::error::Error>> {
        let sel_mon_key = self.state.sel_mon.ok_or("No selected monitor")?;
        let client_list = self
            .state
            .monitor_clients
            .get(sel_mon_key)
            .ok_or("Monitor client list not found")?;

        let current_index = client_list
            .iter()
            .position(|&k| k == current_key)
            .ok_or("Current client not found in monitor list")?;

        for &client_key in &client_list[current_index + 1..] {
            if self.is_tiled_and_visible(client_key) {
                return Ok(Some(client_key));
            }
        }

        for &client_key in &client_list[..current_index] {
            if self.is_tiled_and_visible(client_key) {
                return Ok(Some(client_key));
            }
        }

        Ok(None)
    }

    pub(crate) fn find_previous_tiled_client(
        &self,
        current_key: ClientKey,
    ) -> Result<Option<ClientKey>, Box<dyn std::error::Error>> {
        let sel_mon_key = self.state.sel_mon.ok_or("No selected monitor")?;
        let client_list = self
            .state
            .monitor_clients
            .get(sel_mon_key)
            .ok_or("Monitor client list not found")?;

        let current_index = client_list
            .iter()
            .position(|&k| k == current_key)
            .ok_or("Current client not found in monitor list")?;

        for &client_key in client_list[..current_index].iter().rev() {
            if self.is_tiled_and_visible(client_key) {
                return Ok(Some(client_key));
            }
        }

        for &client_key in client_list[current_index + 1..].iter().rev() {
            if self.is_tiled_and_visible(client_key) {
                return Ok(Some(client_key));
            }
        }

        Ok(None)
    }

    pub(crate) fn swap_clients_in_monitor(
        &mut self,
        client1_key: ClientKey,
        client2_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let sel_mon_key = self.state.sel_mon.ok_or("No selected monitor")?;

        if let Some(client_list) = self.state.monitor_clients.get_mut(sel_mon_key) {
            let pos1 = client_list
                .iter()
                .position(|&k| k == client1_key)
                .ok_or("Client1 not found in monitor list")?;
            let pos2 = client_list
                .iter()
                .position(|&k| k == client2_key)
                .ok_or("Client2 not found in monitor list")?;

            client_list.swap(pos1, pos2);
        }

        if let Some(stack_list) = self.state.monitor_stack.get_mut(sel_mon_key) {
            if let (Some(pos1), Some(pos2)) = (
                stack_list.iter().position(|&k| k == client1_key),
                stack_list.iter().position(|&k| k == client2_key),
            ) {
                stack_list.swap(pos1, pos2);
            }
        }

        info!(
            "[swap_clients_in_monitor] Swapped clients {:?} and {:?}",
            client1_key, client2_key
        );
        Ok(())
    }

    pub fn zoom(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // info!("[zoom]");

        let sel_mon_key = match self.state.sel_mon {
            Some(key) => key,
            None => return Ok(()),
        };

        let selected_client_key = if let Some(monitor) = self.state.monitors.get(sel_mon_key) {
            monitor.sel
        } else {
            return Ok(());
        };

        let selected_client_key = match selected_client_key {
            Some(key) => key,
            None => return Ok(()), // 没有选中的客户端
        };

        if let Some(client) = self.state.clients.get(selected_client_key) {
            if client.state.is_floating {
                return Ok(()); // 浮动窗口不参与zoom
            }
        } else {
            return Ok(());
        }

        let first_tiled = self.nexttiled(sel_mon_key, None);

        let target_client_key = if Some(selected_client_key) == first_tiled {
            self.nexttiled(sel_mon_key, Some(selected_client_key))
        } else {
            Some(selected_client_key)
        };

        if let Some(client_key) = target_client_key {
            self.pop(backend, client_key);
        }

        Ok(())
    }

    pub fn loopview(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[loopview]");

        let direction = match arg {
            WMArgEnum::Int(val) => *val,
            _ => return Ok(()),
        };

        if direction == 0 {
            return Ok(());
        }

        let next_tag = self.calculate_next_tag(direction);

        if self.is_same_tag(next_tag) {
            return Ok(());
        }

        let (sel_mon_key, old_tag_mask) = match self.state.sel_mon {
            Some(k) => {
                let old = self
                    .state
                    .monitors
                    .get(k)
                    .map(|m| m.get_active_tags())
                    .unwrap_or(next_tag);
                (k, old)
            }
            None => return Ok(()),
        };

        // Trigger compositor transition for loopview shortcuts (Alt+Tab/PageUp/PageDown).
        let mut transitioning = false;
        if backend.has_compositor() {
            let cfg = CONFIG.load();
            if cfg.animation_enabled()
                && self.should_animate_tag_switch(sel_mon_key, old_tag_mask, next_tag)
            {
                let dir = Self::tag_switch_direction(old_tag_mask, next_tag, cfg.tags_length());
                let mon_rect = self.monitor_rect(sel_mon_key);
                backend.compositor_notify_tag_switch(
                    cfg.animation_duration(),
                    dir,
                    self.tag_transition_exclude_top(sel_mon_key),
                    mon_rect,
                );
                transitioning = true;
            }
        }

        info!(
            "[loopview] next_tag: {}, direction: {}",
            next_tag, direction
        );

        let cur_tag = self.switch_to_tag(next_tag, next_tag)?;
        if let Some(sel_mon_key) = self.state.sel_mon {
            self.update_sticky_tags(sel_mon_key);
        }

        let sel_opt = self.apply_pertag_settings(cur_tag)?;

        self.focus(backend, sel_opt)?;
        // Suppress layout animations during tag transition so target windows
        // appear instantly (the compositor overlay handles the visual effect).
        self.suppress_layout_animation = transitioning;
        self.arrange(backend, self.state.sel_mon.clone());
        self.suppress_layout_animation = false;
        self.refresh_compositor_monitors(backend);

        Ok(())
    }

    pub(crate) fn calculate_next_tag(&self, direction: i32) -> u32 {
        let current_tag = if let Some(sel_mon_key) = self.state.sel_mon {
            if let Some(monitor) = self.state.monitors.get(sel_mon_key) {
                monitor.get_active_tags()
            } else {
                warn!("[calculate_next_tag] Selected monitor not found");
                return 1; // 返回默认的第一个标签
            }
        } else {
            warn!("[calculate_next_tag] No monitor selected");
            return 1; // 返回默认的第一个标签
        };

        let current_tag_index = if current_tag == 0 {
            0 // 如果当前没有选中的tag，从第一个开始
        } else {
            current_tag.trailing_zeros() as usize
        };

        let max_tags = CONFIG.load().tags_length().max(1);
        let current_tag_index = current_tag_index % max_tags;
        let next_tag_index = if direction > 0 {
            (current_tag_index + 1) % max_tags
        } else {
            if current_tag_index == 0 {
                max_tags - 1
            } else {
                current_tag_index - 1
            }
        };
        let next_tag = 1 << next_tag_index;

        info!(
            "[calculate_next_tag] current_tag: {}, next_tag: {}, direction: {}",
            current_tag, next_tag, direction
        );

        next_tag
    }

    pub(crate) fn primary_tag_index(mask: u32) -> Option<usize> {
        if mask == 0 || mask == u32::MAX {
            return None;
        }
        Some(mask.trailing_zeros() as usize)
    }

    // Returns +1 for forward (higher tag), -1 for backward (lower tag).
    // Uses shortest circular direction to keep wrap-around natural.
    pub(crate) fn tag_switch_direction(old_mask: u32, new_mask: u32, tags_len: usize) -> i32 {
        let Some(old_idx) = Self::primary_tag_index(old_mask) else {
            return 1;
        };
        let Some(new_idx) = Self::primary_tag_index(new_mask) else {
            return 1;
        };
        if old_idx == new_idx || tags_len == 0 {
            return 1;
        }

        let direct = new_idx as i32 - old_idx as i32;
        let wrap_forward = direct + tags_len as i32;
        let wrap_backward = direct - tags_len as i32;

        // Pick the delta with smallest absolute distance.
        let mut best = direct;
        if wrap_forward.abs() < best.abs() {
            best = wrap_forward;
        }
        if wrap_backward.abs() < best.abs() {
            best = wrap_backward;
        }

        if best >= 0 { 1 } else { -1 }
    }

    pub fn view(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let ui = match arg {
            WMArgEnum::UInt(val) => *val,
            _ => return Ok(()),
        };
        let cfg = CONFIG.load();
        let target_mask = ui & cfg.tagmask();

        let sel_mon_key = match self.state.sel_mon {
            Some(k) => k,
            None => return Ok(()),
        };

        // 1. 检查是否无需切换
        if let Some(mon) = self.state.monitors.get(sel_mon_key) {
            if crate::core::workspace::WorkspaceManager::is_same_tag(mon, target_mask) {
                return Ok(());
            }
        }

        // 2. 状态变更 (纯逻辑)
        let mut client_to_focus = None;
        let mut old_tag_mask = 0u32;
        let mut new_tag_mask = target_mask;
        if let Some(monitor) = self.state.monitors.get_mut(sel_mon_key) {
            old_tag_mask = monitor.get_active_tags();
            monitor.view_tag(target_mask, false); // false = not toggle, direct set
            new_tag_mask = monitor.get_active_tags();
            // 获取该 Tag 上次选中的 Client
            client_to_focus = monitor.get_selected_client_for_current_tag();
        }
        // pertag.sel 可能仍记录着已被移动到其它显示器或已销毁的窗口。若它已不属于
        // 当前显示器,丢弃它,让 focus() 在本显示器内回退选择,避免焦点跳到别的屏。
        if let Some(ck) = client_to_focus {
            let still_here = self.state.clients.get(ck).and_then(|c| c.mon) == Some(sel_mon_key);
            if !still_here {
                client_to_focus = None;
            }
        }
        self.update_sticky_tags(sel_mon_key);

        // 3. 副作用 (Backend / Arrange)
        // Notify compositor to capture old scene for slide transition
        let mut transitioning = false;
        if backend.has_compositor() {
            if cfg.animation_enabled()
                && self.should_animate_tag_switch(sel_mon_key, old_tag_mask, new_tag_mask)
            {
                let direction =
                    Self::tag_switch_direction(old_tag_mask, new_tag_mask, cfg.tags_length());
                let mon_rect = self.monitor_rect(sel_mon_key);
                let exclude_top = self.tag_transition_exclude_top(sel_mon_key);
                backend.compositor_notify_tag_switch(
                    cfg.animation_duration(),
                    direction,
                    exclude_top,
                    mon_rect,
                );
                transitioning = true;
            }
        }
        self.focus(backend, client_to_focus)?;
        self.suppress_layout_animation = transitioning;
        self.arrange(backend, Some(sel_mon_key));
        self.suppress_layout_animation = false;
        self.update_ewmh_desktop(backend)?;
        // Tag changed: re-resolve per-tag wallpapers in the compositor.
        if old_tag_mask != new_tag_mask {
            self.refresh_compositor_monitors(backend);
        }

        self.broadcast_ipc_event(
            "tag/view",
            serde_json::json!({
                "tag": target_mask,
            }),
        );

        Ok(())
    }

    pub(crate) fn is_same_tag(&self, target_tag: u32) -> bool {
        if let Some(sel_mon_key) = self.state.sel_mon {
            if let Some(monitor) = self.state.monitors.get(sel_mon_key) {
                return target_tag == monitor.get_active_tags();
            }
        }
        false
    }

    pub(crate) fn switch_to_tag(
        &mut self,
        target_tag: u32,
        ui: u32,
    ) -> Result<usize, Box<dyn std::error::Error>> {
        let sel_mon_key = match self.state.sel_mon {
            Some(k) => k,
            None => return Ok(0),
        };
        let sel_mon_mut = if let Some(sel_mon) = self.state.monitors.get_mut(sel_mon_key) {
            sel_mon
        } else {
            return Ok(0);
        };

        info!("[switch_to_tag] tag_set: {:?}", sel_mon_mut.tag_set);
        info!("[switch_to_tag] old sel_tags: {}", sel_mon_mut.sel_tags);

        sel_mon_mut.sel_tags ^= 1;
        let new_sel_tags = sel_mon_mut.sel_tags;
        info!("[switch_to_tag] new sel_tags: {}", new_sel_tags);

        let cur_tag = if target_tag > 0 {
            sel_mon_mut.tag_set[new_sel_tags] = target_tag;

            let new_cur_tag = if ui == !0 {
                0 // 显示所有标签
            } else {
                ui.trailing_zeros() as usize + 1
            };

            if let Some(pertag) = sel_mon_mut.pertag.as_mut() {
                pertag.prev_tag = pertag.cur_tag;
                pertag.cur_tag = new_cur_tag;
            }

            new_cur_tag
        } else {
            if let Some(pertag) = sel_mon_mut.pertag.as_mut() {
                std::mem::swap(&mut pertag.prev_tag, &mut pertag.cur_tag);
                pertag.cur_tag
            } else {
                return Err("No pertag information available".into());
            }
        };

        info!(
            "[switch_to_tag] prev_tag: {}, cur_tag: {}",
            sel_mon_mut.pertag.as_ref().map(|p| p.prev_tag).unwrap_or(0),
            cur_tag
        );

        Ok(cur_tag)
    }

    pub(crate) fn apply_pertag_settings(
        &mut self,
        cur_tag: usize,
    ) -> Result<Option<ClientKey>, Box<dyn std::error::Error>> {
        let sel_mon_key = self.state.sel_mon.ok_or("No monitor selected")?;

        let (n_master, m_fact, sel_lt, layout_0, layout_1, sel_client_key) = {
            let monitor = self
                .state
                .monitors
                .get(sel_mon_key)
                .ok_or("Selected monitor not found")?;

            let pertag = monitor
                .pertag
                .as_ref()
                .ok_or("No pertag information available")?;

            let sel_lt = pertag.sel_lts[cur_tag];
            (
                pertag.n_masters[cur_tag],
                pertag.m_facts[cur_tag],
                sel_lt,
                pertag.lt_idxs[cur_tag][sel_lt]
                    .clone()
                    .ok_or("Layout not found")?,
                pertag.lt_idxs[cur_tag][sel_lt ^ 1]
                    .clone()
                    .ok_or("Alternative layout not found")?,
                pertag.sel[cur_tag],
            )
        };

        if let Some(monitor) = self.state.monitors.get_mut(sel_mon_key) {
            monitor.layout.n_master = n_master;
            monitor.layout.m_fact = m_fact;
            monitor.sel_lt = sel_lt;
            monitor.lt[sel_lt] = layout_0;
            monitor.lt[sel_lt ^ 1] = layout_1;
        } else {
            return Err("Monitor disappeared during operation".into());
        }

        if let Some(client_key) = sel_client_key {
            if let Some(client) = self.state.clients.get(client_key) {
                info!(
                    "[apply_pertag_settings] selected client: {} (key: {:?})",
                    client.name, client_key
                );
            } else {
                warn!(
                    "[apply_pertag_settings] selected client key {:?} not found",
                    client_key
                );
            }
        }

        Ok(sel_client_key)
    }

    pub fn toggleview(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let ui = match arg {
            WMArgEnum::UInt(val) => *val,
            _ => return Ok(()),
        };
        let cfg = CONFIG.load();
        let mask = ui & cfg.tagmask();
        let sel_mon_key = self.state.sel_mon.ok_or("No monitor selected")?;

        // 1. 状态变更
        let mut old_tag_mask = 0u32;
        let mut new_tag_mask = mask;
        if let Some(monitor) = self.state.monitors.get_mut(sel_mon_key) {
            old_tag_mask = monitor.get_active_tags();
            monitor.view_tag(mask, true); // true = toggle
            new_tag_mask = monitor.get_active_tags();
        }
        self.update_sticky_tags(sel_mon_key);

        // 2. 副作用
        // Notify compositor to capture old scene for slide transition
        let mut transitioning = false;
        if backend.has_compositor() {
            if cfg.animation_enabled()
                && self.should_animate_tag_switch(sel_mon_key, old_tag_mask, new_tag_mask)
            {
                let direction =
                    Self::tag_switch_direction(old_tag_mask, new_tag_mask, cfg.tags_length());
                let mon_rect = self.monitor_rect(sel_mon_key);
                let exclude_top = self.tag_transition_exclude_top(sel_mon_key);
                backend.compositor_notify_tag_switch(
                    cfg.animation_duration(),
                    direction,
                    exclude_top,
                    mon_rect,
                );
                transitioning = true;
            }
        }
        self.focus(backend, None)?;
        self.suppress_layout_animation = transitioning;
        self.arrange(backend, Some(sel_mon_key));
        self.suppress_layout_animation = false;
        self.update_ewmh_desktop(backend)?;
        if old_tag_mask != new_tag_mask {
            self.refresh_compositor_monitors(backend);
        }

        Ok(())
    }

    pub fn toggletag(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[toggletag]");

        let sel_client_key = if let Some(sel_mon_key) = self.state.sel_mon {
            if let Some(monitor) = self.state.monitors.get(sel_mon_key) {
                monitor.sel
            } else {
                return Ok(());
            }
        } else {
            return Ok(());
        };

        let sel_client_key = match sel_client_key {
            Some(key) => key,
            None => return Ok(()),
        };

        if let WMArgEnum::UInt(ui) = *arg {
            let current_tags = if let Some(client) = self.state.clients.get(sel_client_key) {
                client.state.tags
            } else {
                warn!("[toggletag] Selected client {:?} not found", sel_client_key);
                return Ok(());
            };

            let newtags = current_tags ^ (ui & CONFIG.load().tagmask());

            if newtags > 0 {
                if let Some(client) = self.state.clients.get_mut(sel_client_key) {
                    client.state.tags = newtags;
                } else {
                    return Ok(());
                }

                self.setclienttagprop(backend, sel_client_key)?;

                self.focus(backend, None)?;
                self.arrange(backend, self.state.sel_mon);
            }
        }

        Ok(())
    }

    pub fn quit(
        &mut self,
        _backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // info!("[quit]");
        self.running.store(false, Ordering::SeqCst);
        Ok(())
    }

    pub fn setup(&mut self, backend: &mut dyn Backend) -> Result<(), Box<dyn std::error::Error>> {
        info!("[setup]");
        let _ = self.updategeom(backend);
        backend.register_wm("jwm")?;

        let mask = (EventMaskBits::SUBSTRUCTURE_REDIRECT
            | EventMaskBits::SUBSTRUCTURE_NOTIFY
            | EventMaskBits::STRUCTURE_NOTIFY
            | EventMaskBits::BUTTON_PRESS
            | EventMaskBits::KEY_RELEASE
            | EventMaskBits::POINTER_MOTION
            | EventMaskBits::ENTER_WINDOW
            | EventMaskBits::LEAVE_WINDOW
            | EventMaskBits::PROPERTY_CHANGE)
            .bits();

        let root = backend.root_window().expect("no root window");
        backend
            .cursor_provider()
            .apply(root, StdCursorKind::LeftPtr)?;
        backend
            .window_ops()
            .change_event_mask(backend.root_window().expect("no root window"), mask)?;
        self.grabkeys(backend)?;
        self.focus(backend, None)?;

        self.setup_initial_windows(backend)?;

        self.arrange(backend, None);
        let _ = self.restack(backend, self.state.sel_mon);
        let _ = self.focus(backend, None);
        let _ = self.update_ewmh_desktop(backend);

        backend.window_ops().flush()?;
        Ok(())
    }

    pub fn killclient(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[killclient]");
        let sel_client_key = match self.get_selected_client_key() {
            Some(k) => k,
            None => return Ok(()),
        };

        let client_win = if let Some(c) = self.state.clients.get(sel_client_key) {
            c.win
        } else {
            return Ok(());
        };

        info!("[killclient] Closing window {:?}", client_win);
        let res = backend.window_ops().close_window(client_win)?;
        if res == crate::backend::api::CloseResult::Forced {
            info!("[killclient] Force killed client");
        } else {
            info!("[killclient] Sent graceful close request");
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::ClientKey;
    use crate::Jwm;
    use slotmap::SlotMap;

    fn keys(n: usize) -> (SlotMap<ClientKey, ()>, Vec<ClientKey>) {
        let mut sm: SlotMap<ClientKey, ()> = SlotMap::new();
        let ks = (0..n).map(|_| sm.insert(())).collect();
        (sm, ks)
    }

    #[test]
    fn primary_tag_index_basics() {
        // Empty and "all tags" masks are ambiguous → None.
        assert_eq!(Jwm::primary_tag_index(0), None);
        assert_eq!(Jwm::primary_tag_index(u32::MAX), None);
        // Single-bit masks map to their bit position.
        assert_eq!(Jwm::primary_tag_index(0b0001), Some(0));
        assert_eq!(Jwm::primary_tag_index(0b0100), Some(2));
        // Multi-bit mask resolves to the lowest set bit (trailing zeros).
        assert_eq!(Jwm::primary_tag_index(0b0110), Some(1));
    }

    #[test]
    fn tag_switch_direction_forward_and_backward() {
        let len = 9;
        // tag 1 (idx 0) → tag 2 (idx 1): forward.
        assert_eq!(Jwm::tag_switch_direction(0b001, 0b010, len), 1);
        // tag 4 (idx 2) → tag 2 (idx 1): backward.
        assert_eq!(Jwm::tag_switch_direction(0b100, 0b010, len), -1);
    }

    #[test]
    fn tag_switch_direction_wraps_shortest_path() {
        let len = 9;
        // idx 8 → idx 0: direct delta -8, wrap-forward +1 is shorter → forward.
        let last = 1u32 << 8;
        assert_eq!(Jwm::tag_switch_direction(last, 0b1, len), 1);
        // idx 0 → idx 8: direct +8, wrap-backward -1 is shorter → backward.
        assert_eq!(Jwm::tag_switch_direction(0b1, last, len), -1);
    }

    #[test]
    fn tag_switch_direction_edge_cases() {
        // Same tag → defaults to forward (1).
        assert_eq!(Jwm::tag_switch_direction(0b010, 0b010, 9), 1);
        // Invalid masks → defaults to forward (1).
        assert_eq!(Jwm::tag_switch_direction(0, 0b010, 9), 1);
        assert_eq!(Jwm::tag_switch_direction(0b010, u32::MAX, 9), 1);
        // tags_len 0 → forward.
        assert_eq!(Jwm::tag_switch_direction(0b001, 0b010, 0), 1);
    }

    #[test]
    fn next_in_group_walks_forward_then_stops() {
        let (_sm, k) = keys(3);
        assert_eq!(Jwm::next_in_group(&k, k[0]), Some(k[1]));
        assert_eq!(Jwm::next_in_group(&k, k[1]), Some(k[2]));
        // Last element has no successor.
        assert_eq!(Jwm::next_in_group(&k, k[2]), None);
    }

    #[test]
    fn prev_in_group_walks_backward_then_stops() {
        let (_sm, k) = keys(3);
        assert_eq!(Jwm::prev_in_group(&k, k[2]), Some(k[1]));
        assert_eq!(Jwm::prev_in_group(&k, k[1]), Some(k[0]));
        // First element has no predecessor.
        assert_eq!(Jwm::prev_in_group(&k, k[0]), None);
    }

    #[test]
    fn in_group_missing_key_returns_none() {
        // Mint 3 keys from one map; the group is only the first 2, so k[2] is
        // a valid-but-absent key. (Keys from two separate SlotMaps would alias.)
        let (_sm, k) = keys(3);
        let group = &k[..2];
        assert_eq!(Jwm::next_in_group(group, k[2]), None);
        assert_eq!(Jwm::prev_in_group(group, k[2]), None);
        // Empty group.
        assert_eq!(Jwm::next_in_group(&[], k[0]), None);
        assert_eq!(Jwm::prev_in_group(&[], k[0]), None);
    }
}
