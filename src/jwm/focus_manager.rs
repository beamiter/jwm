//! 焦点管理模块
//!
//! 这个模块包含所有窗口焦点管理相关的功能

use crate::backend::api::Backend;
use crate::backend::common_define::WindowId;
use crate::config::CONFIG;
use crate::core::animation::AnimationKind;
use crate::core::layout::LayoutEnum;
use crate::core::models::{ClientKey, MonitorKey, ScrollingState, WMMonitor};
use crate::core::types::Rect;
use crate::jwm::Jwm;
use crate::jwm::statusbar::StatusBarBuilder;
use crate::jwm::types::WMArgEnum;
use crate::jwm::visibility::hidden_x_left_of_desktop;
use log::info;
use std::collections::HashMap;
use std::time::Instant;

#[derive(Clone)]
struct RevealNavigationSnapshot {
    selected_monitor: Option<MonitorKey>,
    selected_client: Option<ClientKey>,
    target_monitor: MonitorKey,
    target_state: WMMonitor,
    target_stack: Option<Vec<ClientKey>>,
    sticky_tags: Vec<(ClientKey, u32)>,
    scrolling_states: Vec<((MonitorKey, u32), ScrollingState)>,
}

impl Jwm {
    fn remember_scrolling_focus_for_client(&mut self, client_key: ClientKey) {
        let mon_key = match self.state.clients.get(client_key).and_then(|c| c.mon) {
            Some(mon_key) => mon_key,
            None => return,
        };
        let is_scrolling = self
            .state
            .monitors
            .get(mon_key)
            .map(|monitor| *monitor.lt == LayoutEnum::SCROLLING)
            .unwrap_or(false);
        if !is_scrolling || !self.is_client_visible_on_monitor(client_key, mon_key) {
            return;
        }
        if let Some(state) = self.scrolling_state_for_monitor_mut(mon_key) {
            state.remember_focus(client_key);
        }
    }

    /// 处理 FocusIn 事件：当焦点被其他窗口抢占时，重新设置焦点
    pub(crate) fn focusin(
        &mut self,
        backend: &mut dyn Backend,
        event_window: WindowId,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let sel_client_key = self.get_selected_client_key();
        if let Some(client_key) = sel_client_key {
            if let Some(client) = self.state.clients.get(client_key) {
                if event_window != client.win {
                    if self.wintoclient(event_window).is_some() {
                        self.setfocus(backend, client_key)?;
                    } else {
                        // 是未知窗口（可能是输入法、系统弹窗等），允许它持有焦点
                        // 不要调用 setfocus
                        // debug!("Focus stolen by unmanaged window, ignoring allow...");
                    }
                }
            }
        }
        Ok(())
    }

    /// 切换焦点到不同的显示器
    ///
    /// 参数 arg 应为 `Int(i)`，表示方向：+1 下一个，-1 上一个
    pub fn focusmon(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.state.monitor_order.len() <= 1 {
            return Ok(());
        }

        if let WMArgEnum::Int(i) = arg {
            if let Some(target_mon_key) = self.dirtomon(i) {
                if Some(target_mon_key) == self.state.sel_mon {
                    return Ok(());
                }
                self.switch_to_monitor(backend, target_mon_key)?;
                self.focus(backend, None)?;

                let mon_num = self.state.monitors.get(target_mon_key).map(|m| m.num);
                if let Some(num) = mon_num {
                    self.broadcast_ipc_event(
                        "monitor/focus",
                        serde_json::json!({
                            "monitor": num,
                        }),
                    );
                }
            }
        }
        Ok(())
    }

    /// 在窗口栈中切换焦点（Alt+j/k）
    ///
    /// 参数 arg 应为 `Int(i)`：正数向下，负数向上
    pub fn focusstack(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // In scrolling layout, Alt+j/k navigates within column
        if self.is_scrolling_layout() {
            return self.scrolling_focus_window(backend, arg);
        }

        let direction = match *arg {
            WMArgEnum::Int(i) => i,
            _ => return Ok(()),
        };

        if direction == 0 {
            return Ok(());
        }

        if !self.can_focus_switch()? {
            return Ok(());
        }

        let target_client = if direction > 0 {
            self.find_next_visible_client()?
        } else {
            self.find_previous_visible_client()?
        };

        if let Some(client_key) = target_client {
            self.focus(backend, Some(client_key))?;
            self.restack(backend, self.state.sel_mon)?;

            // V-stack: re-arrange so new focus moves to center
            if self.is_vstack_layout() {
                if let Some(mk) = self.state.sel_mon {
                    // Save each visible tiled client's current visual rect BEFORE
                    // arrangemon overwrites client.geometry.  When the compositor
                    // is active, resizeclient moves the real X11 window to the
                    // target instantly, so the old geometry values that resizeclient
                    // stores in old_x/old_y can already equal the target from a
                    // previous identical layout pass, causing the animation to be
                    // skipped (current_visual == target).  By snapshotting the
                    // visual rect here we can inject the correct "from" rect.
                    let pre_rects: HashMap<ClientKey, Rect> = {
                        let now = Instant::now();
                        self.collect_tileable_clients(mk)
                            .iter()
                            .map(|&(k, _, _)| {
                                let visual = self
                                    .animations
                                    .current_visual_rect(k, now)
                                    .or_else(|| {
                                        self.state.clients.get(k).map(|c| {
                                            Rect::new(
                                                c.geometry.x,
                                                c.geometry.y,
                                                c.geometry.w,
                                                c.geometry.h,
                                            )
                                        })
                                    })
                                    .unwrap_or_default();
                                (k, visual)
                            })
                            .collect()
                    };

                    self.arrangemon(backend, mk);

                    // Patch animations: always retarget changed clients from the
                    // pre-snapshot visual rect to the new layout target so vstack
                    // focus cycling (Alt+j/k) consistently shows move animation.
                    for (ck, pre_rect) in &pre_rects {
                        if let Some(client) = self.state.clients.get(*ck) {
                            let target = Rect::new(
                                client.geometry.x,
                                client.geometry.y,
                                client.geometry.w,
                                client.geometry.h,
                            );
                            if *pre_rect != target {
                                let cfg = CONFIG.load();
                                if cfg.animation_enabled() {
                                    let duration = cfg.animation_duration();
                                    let easing = cfg.animation_easing();
                                    self.animations.start(
                                        *ck,
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

                    let _ = self.restack(backend, Some(mk));
                }
            }

            self.suppress_mouse_focus_until =
                Some(std::time::Instant::now() + std::time::Duration::from_millis(200));
        }
        Ok(())
    }

    /// IPC: focus_none — 取消所有窗口焦点，聚焦到 root window
    pub fn focus_none(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[focus_none]");
        self.focus(backend, None)
    }

    /// IPC: focus_window — 按窗口 ID 聚焦指定窗口
    pub fn focus_window(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let win_id = match arg {
            WMArgEnum::UInt64(id) => *id,
            _ => return Err("focus_window requires a window id".into()),
        };
        info!("[focus_window] id={}", win_id);
        if !self.reveal_and_focus(backend, WindowId::from_raw(win_id))? {
            return Err(format!("window {win_id} not found").into());
        }
        Ok(())
    }

    fn capture_reveal_navigation(
        &self,
        target_monitor: MonitorKey,
    ) -> Option<RevealNavigationSnapshot> {
        let target_state = self.state.monitors.get(target_monitor)?.clone();
        let sticky_tags = self
            .state
            .monitor_clients
            .get(target_monitor)
            .into_iter()
            .flatten()
            .filter_map(|client_key| {
                self.state
                    .clients
                    .get(*client_key)
                    .filter(|client| client.state.is_sticky)
                    .map(|client| (*client_key, client.state.tags))
            })
            .collect();
        let scrolling_states = self
            .scrolling_states
            .iter()
            .filter(|((monitor, _), _)| *monitor == target_monitor)
            .map(|(key, state)| (*key, state.clone()))
            .collect();
        Some(RevealNavigationSnapshot {
            selected_monitor: self.state.sel_mon,
            selected_client: self.get_selected_client_key(),
            target_monitor,
            target_state,
            target_stack: self.state.monitor_stack.get(target_monitor).cloned(),
            sticky_tags,
            scrolling_states,
        })
    }

    /// Restore only the navigation context around a failed hidden-window
    /// reveal. Scratchpad placement is intentionally captured after its
    /// ownership migration, so this never sends a retryable Dock item back to
    /// the source monitor; it only undoes monitor/tag/focus changes made while
    /// attempting the reveal.
    fn rollback_failed_reveal_navigation(
        &mut self,
        backend: &mut dyn Backend,
        snapshot: &RevealNavigationSnapshot,
        win: WindowId,
    ) {
        let current_target_selection =
            self.state
                .monitors
                .get(snapshot.target_monitor)
                .and_then(|monitor| {
                    monitor
                        .get_selected_client_for_current_tag()
                        .or(monitor.sel)
                });
        if let Some(current) = current_target_selection
            && Some(current) != snapshot.selected_client
            && let Err(error) = self.unfocus_client(backend, current, false)
        {
            log::warn!(
                "could not clear temporary target focus while rolling back reveal for {win:?}: {error}"
            );
        }

        if let Some(target) = self.state.monitors.get_mut(snapshot.target_monitor) {
            *target = snapshot.target_state.clone();
        }
        match &snapshot.target_stack {
            Some(stack) => {
                self.state
                    .monitor_stack
                    .insert(snapshot.target_monitor, stack.clone());
            }
            None => {
                self.state.monitor_stack.remove(snapshot.target_monitor);
            }
        }
        for &(client_key, tags) in &snapshot.sticky_tags {
            if let Some(client) = self.state.clients.get_mut(client_key) {
                client.state.tags = tags;
            }
        }
        self.scrolling_states
            .retain(|(monitor, _), _| *monitor != snapshot.target_monitor);
        self.scrolling_states
            .extend(snapshot.scrolling_states.iter().cloned());
        self.state.sel_mon = snapshot.selected_monitor;

        // Never trust the pre-failure stacking cache: both `view` and focus
        // may already have changed the real server stack before reporting an
        // error. Force an authoritative arrange/restack of the restored view.
        self.last_stacking.remove(snapshot.target_monitor);
        self.arrange(backend, Some(snapshot.target_monitor));

        let selected_client = snapshot
            .selected_client
            .filter(|client_key| self.is_client_visible_by_key(*client_key));
        let focus_result = if selected_client.is_some() {
            self.focus(backend, selected_client)
        } else {
            let result = self.set_root_focus(backend);
            self.update_monitor_selection_by_key(None);
            result
        };
        if let Err(error) = focus_result {
            log::warn!("could not restore focus after failed reveal for {win:?}: {error}");
        }

        // `focus` promotes its client in the logical stack. Put the exact
        // pre-navigation order back, then force the physical stack to match;
        // selected-window promotion remains an explicit restack policy.
        if let Some(stack) = &snapshot.target_stack {
            self.state
                .monitor_stack
                .insert(snapshot.target_monitor, stack.clone());
        }
        self.last_stacking.remove(snapshot.target_monitor);
        if let Err(error) = self.restack(backend, Some(snapshot.target_monitor)) {
            log::warn!(
                "could not restore target stacking after failed reveal for {win:?}: {error}"
            );
        }
        if let Some(selected_monitor) = snapshot.selected_monitor
            && selected_monitor != snapshot.target_monitor
        {
            self.last_stacking.remove(selected_monitor);
            if let Err(error) = self.restack(backend, Some(selected_monitor)) {
                log::warn!(
                    "could not restore source stacking after failed reveal for {win:?}: {error}"
                );
            }
        }
        if let Err(error) = self.update_ewmh_desktop(backend) {
            log::warn!("could not restore EWMH desktop after failed reveal for {win:?}: {error}");
        }
        self.refresh_compositor_monitors(backend);
        let target_num = self
            .state
            .monitors
            .get(snapshot.target_monitor)
            .map(|monitor| monitor.num);
        let source_num = snapshot
            .selected_monitor
            .and_then(|monitor| self.state.monitors.get(monitor))
            .map(|monitor| monitor.num);
        self.mark_bar_update_needed_if_visible(target_num);
        self.mark_bar_update_needed_if_visible(source_num);
    }

    /// Bring a window to the front of the session and focus it: its monitor,
    /// then its tag, then un-minimised, then focused and restacked.
    ///
    /// `focus()` on its own cannot do this. When the requested client is not
    /// visible it silently redirects to whichever one is, so "focus that
    /// window" quietly focused a *different* window whenever the target sat
    /// on a hidden tag or another monitor. Every caller that means "show me
    /// that window" goes through here, so the keybinding, the launcher and
    /// the IPC command cannot drift apart.
    ///
    /// Returns false when the window is gone — a launcher row can outlive the
    /// window it names, and that is a no-op rather than an error.
    pub(crate) fn reveal_and_focus(
        &mut self,
        backend: &mut dyn Backend,
        win: WindowId,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(client_key) = self.wintoclient(win) else {
            log::debug!("[reveal_and_focus] window {win:?} is gone");
            return Ok(false);
        };

        // A parked scratchpad deliberately has no tag, so the generic
        // visibility path cannot infer where to reveal it and `focus()` would
        // silently fall back to another client. A minimized scratchpad has the
        // same problem when the selected monitor changed while it was in the
        // Dock. Give both a concrete placement first, while a minimized client
        // is still hidden, so the shared restore transition remains the only
        // path that exposes it.
        self.prepare_scratchpad_reveal_placement(backend, client_key)?;

        // Refuse an invalid placement before calling `focus()`, whose normal
        // contract is to substitute another visible client. Scratchpads above
        // have just been assigned a concrete monitor/tag; any remaining
        // monitor-less or tag-less client cannot be revealed by this path.
        let has_revealable_placement = self
            .state
            .clients
            .get(client_key)
            .and_then(|client| {
                self.state.monitors.get(client.mon?).map(|_| {
                    client.state.is_sticky || client.state.tags & CONFIG.load().tagmask() != 0
                })
            })
            .unwrap_or(false);
        if !has_revealable_placement {
            return Err(format!("window {win:?} has no visible monitor/tag placement").into());
        }

        // Keep the client hidden while selecting its monitor and tag. This
        // prevents an intermediate arrange from exposing it before the
        // reverse Genie owns the restore transition.
        let was_hidden = self
            .state
            .clients
            .get(client_key)
            .is_some_and(|client| client.state.is_hidden);

        // The monitor has to move before the tag does: `view` acts on the
        // selected monitor.
        let client_monitor = self
            .state
            .clients
            .get(client_key)
            .and_then(|client| client.mon);
        let reveal_navigation = if was_hidden {
            client_monitor.and_then(|monitor| self.capture_reveal_navigation(monitor))
        } else {
            None
        };
        if let Some(monitor_key) = client_monitor
            && Some(monitor_key) != self.state.sel_mon
        {
            self.switch_to_monitor(backend, monitor_key)?;
        }

        if let Some(target) = self.tags_to_reveal(client_key) {
            self.view(backend, &WMArgEnum::UInt(target))?;
        }

        if was_hidden {
            // The shared transition writes ICCCM/EWMH state, arranges the now
            // visible client, starts the reverse Genie, then focuses it.
            if let Err(error) = self.set_client_minimized(backend, client_key, false) {
                if let Some(snapshot) = &reveal_navigation {
                    self.rollback_failed_reveal_navigation(backend, snapshot, win);
                }
                return Err(error);
            }
        } else {
            self.focus(backend, Some(client_key))?;
            if let Some(mon_key) = self.state.sel_mon {
                self.restack(backend, Some(mon_key))?;
            }
        }

        // `focus()` intentionally substitutes a visible fallback for an
        // invalid target. Activation/reveal must never report success after
        // doing that, otherwise callers close their launcher or acknowledge a
        // taskbar action even though a different window received focus.
        if !self.is_client_visible_by_key(client_key)
            || self.get_selected_client_key() != Some(client_key)
        {
            return Err(format!("window {win:?} could not be made visible and focused").into());
        }
        Ok(true)
    }

    /// Put a parked or minimized scratchpad on the selected monitor/tag before
    /// it is revealed. Returns whether a scratchpad placement was applied.
    ///
    /// Hidden geometry is staged in the dedicated restore slot plus the
    /// floating rectangle while the real surface remains off-screen.
    /// `set_client_minimized(false)` will then arrange it at that target before
    /// starting the compositor restore; configuring it here would briefly
    /// expose an input-active window before the reverse Genie owns the
    /// transition.
    fn prepare_scratchpad_reveal_placement(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        if !self.scratchpads.values().any(|&key| key == client_key) {
            return Ok(false);
        }

        let Some((source_monitor, is_hidden, parked, dock_eligible)) =
            self.state.clients.get(client_key).map(|client| {
                (
                    client.mon,
                    client.state.is_hidden,
                    client.state.tags & CONFIG.load().tagmask() == 0,
                    StatusBarBuilder::is_minimized_dock_eligible(client),
                )
            })
        else {
            return Ok(false);
        };
        if !is_hidden && !parked {
            return Ok(false);
        }

        let target_monitor = self
            .state
            .sel_mon
            .filter(|&monitor| self.state.monitors.get(monitor).is_some())
            .or_else(|| {
                source_monitor.filter(|&monitor| self.state.monitors.get(monitor).is_some())
            })
            .ok_or("scratchpad has no monitor available for reveal")?;
        if source_monitor.is_some() && source_monitor != Some(target_monitor) {
            // This is only the placement phase of a reveal transaction. Do
            // the ownership transfer without `sendmon`, because `sendmon`
            // focuses a fallback and arranges immediately; both would be
            // externally visible before the shared restore focuses the
            // scratchpad. Hidden Dock state still has to migrate before the
            // client monitor changes so stale source-bar messages cannot own
            // its preview or Genie target.
            let source_monitor = source_monitor.expect("checked above");
            let win = self.state.clients[client_key].win;
            let source_monitor_num = self
                .state
                .monitors
                .get(source_monitor)
                .map(|monitor| monitor.num);
            let target_monitor_num = self
                .state
                .monitors
                .get(target_monitor)
                .map(|monitor| monitor.num);
            let target_dock_shelf = target_monitor_num
                .and_then(|monitor_num| self.minimized_dock_shelves.get(&monitor_num))
                .copied();

            if is_hidden {
                if let Some(source_monitor_num) = source_monitor_num {
                    self.clear_minimized_preview_for(backend, source_monitor_num, Some(win));
                }
                backend.compositor_set_window_dock_geometry(win, None);
            }

            self.detach(client_key);
            self.detachstack(client_key);
            if let Some(monitor) = self.state.monitors.get_mut(source_monitor) {
                monitor.clear_selection_of(client_key);
            }
            if let Some(client) = self.state.clients.get_mut(client_key) {
                client.mon = Some(target_monitor);
            }
            self.attach_back(client_key);
            self.attachstack(client_key);

            if is_hidden {
                if dock_eligible && let Some(target) = target_dock_shelf {
                    backend.compositor_set_window_dock_geometry(win, Some(target));
                }
                self.mark_bar_update_needed_if_visible(source_monitor_num);
                self.mark_bar_update_needed_if_visible(target_monitor_num);
            }
        } else if source_monitor.is_none() {
            if let Some(client) = self.state.clients.get_mut(client_key) {
                client.mon = Some(target_monitor);
            }
            self.attach_to_monitor(client_key, target_monitor);
        }

        let target_tags = self
            .state
            .monitors
            .get(target_monitor)
            .map(|monitor| monitor.get_active_tags())
            .unwrap_or(1);
        if let Some(client) = self.state.clients.get_mut(client_key) {
            client.mon = Some(target_monitor);
            client.state.tags = target_tags;
            client.state.is_floating = true;
        }
        self.reorder_client_in_monitor_groups(client_key);
        self.setclienttagprop(backend, client_key)?;

        if let Some(area) = self.monitor_work_area(target_monitor) {
            let width = area.w.saturating_mul(4) / 5;
            let height = area.h.saturating_mul(4) / 5;
            let x = area.x + (area.w - width) / 2;
            let y = area.y + (area.h - height) / 2;

            if is_hidden {
                let desktop_left = self.desktop_left_edge();
                if let Some(client) = self.state.clients.get_mut(client_key) {
                    // Keep x at a hidden coordinate sized for the new target.
                    // show_client() atomically restores the complete staged
                    // rectangle without touching layout/fullscreen `old_*`.
                    let hidden_width =
                        width.saturating_add(client.geometry.border_w.saturating_mul(2));
                    let hidden_x = hidden_x_left_of_desktop(desktop_left, hidden_width);
                    client.geometry.x = hidden_x;
                    client.geometry.hidden_x = Some(hidden_x);
                    client.geometry.hidden_restore_rect = Some(Rect::new(x, y, width, height));
                    client.geometry.y = y;
                    client.geometry.w = width;
                    client.geometry.h = height;
                    client.geometry.floating_x = x;
                    client.geometry.floating_y = y;
                    client.geometry.floating_w = width;
                    client.geometry.floating_h = height;
                }
            } else {
                let suppress = self.suppress_layout_animation;
                self.suppress_layout_animation = true;
                self.resize_client(backend, client_key, x, y, width, height, false);
                self.suppress_layout_animation = suppress;
                if let Some(client) = self.state.clients.get_mut(client_key) {
                    client.geometry.floating_x = client.geometry.x;
                    client.geometry.floating_y = client.geometry.y;
                    client.geometry.floating_w = client.geometry.w;
                    client.geometry.floating_h = client.geometry.h;
                }
                self.arrange(backend, Some(target_monitor));
            }
        } else if !is_hidden {
            self.arrange(backend, Some(target_monitor));
        }

        Ok(true)
    }

    /// The tag mask to switch to so `client_key` becomes visible, or `None`
    /// when it already is. A sticky window is on every tag and never needs
    /// one.
    fn tags_to_reveal(&self, client_key: ClientKey) -> Option<u32> {
        let client = self.state.clients.get(client_key)?;
        if client.state.is_sticky {
            return None;
        }
        let monitor = self.state.monitors.get(client.mon?)?;
        if client.state.tags & monitor.get_active_tags() != 0 {
            return None;
        }
        let wanted = client.state.tags & CONFIG.load().tagmask();
        (wanted != 0).then_some(wanted)
    }

    /// 获取标签组信息（当前未实现）
    fn get_tab_group(&self, _group_id: u32) -> Option<(u32, Vec<(u32, String)>)> {
        None
    }

    /// 切换到窗口组中的某个标签页
    pub fn focus_tab(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Tab info passed as Vec of [group_id, tab_index]
        let args = match arg {
            WMArgEnum::StringVec(v) if v.len() >= 2 => v,
            _ => return Err("focus_tab requires group_id and tab_index".into()),
        };

        let group_id: u32 = args[0].parse()?;
        let tab_index: usize = args[1].parse()?;
        info!("[focus_tab] group_id={}, tab_index={}", group_id, tab_index);

        // Get the focused window in this group
        if let Some((_, tabs_info)) = self.get_tab_group(group_id) {
            if tab_index < tabs_info.len() {
                let target_win = tabs_info[tab_index].0; // x11_win from tab info
                self.focus_window(backend, &WMArgEnum::UInt64(target_win as u64))?;
                return Ok(());
            }
        }
        Err(format!("tab group {}/{} not found", group_id, tab_index).into())
    }

    /// IPC: refocus — unfocus 当前窗口再 focus 回来（用于刷新焦点状态）
    pub fn refocus(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[refocus]");
        let sel_client_key = match self.get_selected_client_key() {
            Some(k) => k,
            None => return Ok(()),
        };
        // 1. unfocus → root
        self.unfocus_client(backend, sel_client_key, true)?;
        self.set_root_focus(backend)?;
        self.update_monitor_selection_by_key(None);
        // 2. focus 回来
        self.focus(backend, Some(sel_client_key))?;
        if let Some(mon_key) = self.state.sel_mon {
            self.restack(backend, Some(mon_key))?;
        }
        Ok(())
    }

    /// 检查鼠标焦点是否被临时阻止
    ///
    /// 在键盘操作后的短时间内阻止鼠标焦点切换，避免意外跳焦点
    pub(crate) fn mouse_focus_blocked(&mut self) -> bool {
        if let Some(deadline) = self.suppress_mouse_focus_until {
            if std::time::Instant::now() < deadline {
                return true;
            }
            self.suppress_mouse_focus_until = None;
        }
        false
    }

    /// 判断是否应该切换焦点到指定窗口
    ///
    /// 返回 true 表示需要切换焦点
    pub(crate) fn should_focus_client(
        &self,
        client_key_opt: Option<ClientKey>,
        is_on_selected_monitor: bool,
    ) -> bool {
        if !is_on_selected_monitor {
            return true;
        }

        if client_key_opt.is_none() {
            return true;
        }

        let current_selected = self.get_selected_client_key();
        current_selected != client_key_opt
    }

    /// 核心焦点管理函数：设置焦点到指定窗口
    ///
    /// - 如果 client_key_opt 为 None，焦点设置到 root window
    /// - 如果指定窗口不可见，自动查找可见窗口
    /// - 广播焦点变化的 IPC 事件
    pub(crate) fn focus(
        &mut self,
        backend: &mut dyn Backend,
        mut client_key_opt: Option<ClientKey>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[focus]");

        let is_visible = match client_key_opt {
            Some(client_key) => self.is_client_visible_by_key(client_key),
            None => false,
        };

        if !is_visible {
            client_key_opt = self.find_visible_client();
        }

        self.handle_focus_change_by_key(backend, &client_key_opt)?;

        if let Some(client_key) = client_key_opt {
            self.set_client_focus_by_key(backend, client_key)?;
        } else {
            self.set_root_focus(backend)?;
        }

        self.update_monitor_selection_by_key(client_key_opt);
        if let Some(client_key) = client_key_opt {
            self.remember_scrolling_focus_for_client(client_key);
        }

        self.mark_bar_update_needed_if_visible(None);

        // Broadcast focus event
        if let Some(ck) = client_key_opt {
            let event_data = self
                .state
                .clients
                .get(ck)
                .map(|c| (c.win.raw(), c.name.clone()));
            if let Some((id, name)) = event_data {
                self.broadcast_ipc_event(
                    "window/focus",
                    serde_json::json!({
                        "id": id, "name": name,
                    }),
                );
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod scratchpad_reveal_tests {
    use super::*;
    use crate::backend::api::{
        BackendDiagnostics, Capabilities, ColorAllocator, CompositorAnnotation,
        CompositorBenchmark, CompositorControl, CompositorMedia, CompositorWindowEffects,
        CompositorWorkspaceEffects, CursorProvider, DisplayControl, InputOps, KeyOps, OutputInfo,
        OutputOps, PropertyOps, RenderScheduler, WindowOps,
    };
    use crate::backend::common_define::OutputId;
    use crate::backend::error::BackendError;
    use crate::backend::wayland_dummy_ops::{
        DummyColorAllocator, DummyCursorProvider, DummyInputOps, DummyKeyOps, DummyOutputOps,
        DummyPropertyOps, DummyWindowOps,
    };
    use crate::core::animation::{AnimationKind, AnimationManager};
    use crate::core::models::WMClient;
    use crate::core::state::WMState;
    use crate::jwm::features::FeatureStates;
    use slotmap::SecondaryMap;
    use std::any::Any;
    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::AtomicBool;
    use xbar_core::shared_structures::SharedMessage;

    struct ScratchpadBackend {
        window_ops: DummyWindowOps,
        input_ops: DummyInputOps,
        property_ops: DummyPropertyOps,
        output_ops: DummyOutputOps,
        key_ops: DummyKeyOps,
        cursor_provider: DummyCursorProvider,
        color_allocator: DummyColorAllocator,
        focused: Vec<Option<WindowId>>,
        minimized: Vec<(WindowId, bool)>,
        dock_targets: Vec<(WindowId, Option<crate::backend::api::CompositorRect>)>,
        fail_focus_for: Option<WindowId>,
    }

    impl ScratchpadBackend {
        fn new() -> Self {
            Self {
                window_ops: DummyWindowOps,
                input_ops: DummyInputOps,
                property_ops: DummyPropertyOps,
                output_ops: DummyOutputOps,
                key_ops: DummyKeyOps,
                cursor_provider: DummyCursorProvider,
                color_allocator: DummyColorAllocator,
                focused: Vec::new(),
                minimized: Vec::new(),
                dock_targets: Vec::new(),
                fail_focus_for: None,
            }
        }
    }

    impl CompositorBenchmark for ScratchpadBackend {}
    impl BackendDiagnostics for ScratchpadBackend {}
    impl CompositorControl for ScratchpadBackend {}
    impl CompositorMedia for ScratchpadBackend {}
    impl CompositorWorkspaceEffects for ScratchpadBackend {}
    impl CompositorWindowEffects for ScratchpadBackend {
        fn compositor_set_window_minimized(&mut self, window: WindowId, minimized: bool) {
            self.minimized.push((window, minimized));
        }

        fn compositor_set_window_dock_geometry(
            &mut self,
            window: WindowId,
            target: Option<crate::backend::api::CompositorRect>,
        ) {
            self.dock_targets.push((window, target));
        }
    }
    impl CompositorAnnotation for ScratchpadBackend {}
    impl DisplayControl for ScratchpadBackend {}
    impl RenderScheduler for ScratchpadBackend {
        fn has_compositor(&self) -> bool {
            true
        }
    }

    impl Backend for ScratchpadBackend {
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }

        fn root_window(&self) -> Option<WindowId> {
            Some(WindowId::from_raw(0))
        }

        fn as_any(&self) -> &dyn Any {
            self
        }

        fn check_existing_wm(&self) -> Result<(), BackendError> {
            Ok(())
        }

        fn window_ops(&self) -> &dyn WindowOps {
            &self.window_ops
        }

        fn input_ops(&self) -> &dyn InputOps {
            &self.input_ops
        }

        fn property_ops(&self) -> &dyn PropertyOps {
            &self.property_ops
        }

        fn output_ops(&self) -> &dyn OutputOps {
            &self.output_ops
        }

        fn key_ops(&self) -> &dyn KeyOps {
            &self.key_ops
        }

        fn key_ops_mut(&mut self) -> &mut dyn KeyOps {
            &mut self.key_ops
        }

        fn cursor_provider(&mut self) -> &mut dyn CursorProvider {
            &mut self.cursor_provider
        }

        fn color_allocator(&mut self) -> &mut dyn ColorAllocator {
            &mut self.color_allocator
        }

        fn on_focused_client_changed(
            &mut self,
            window: Option<WindowId>,
        ) -> Result<(), BackendError> {
            if window.is_some() && window == self.fail_focus_for {
                self.fail_focus_for = None;
                return Err(BackendError::Message("injected focus failure".into()));
            }
            self.focused.push(window);
            Ok(())
        }

        fn run(
            &mut self,
            _handler: &mut dyn crate::backend::api::EventHandler,
        ) -> Result<(), BackendError> {
            Ok(())
        }
    }

    fn empty_jwm() -> Jwm {
        Jwm {
            state: WMState::new(),
            runtime_backend: "test".into(),
            started_at: std::time::Instant::now(),
            s_w: 2400,
            s_h: 900,
            running: AtomicBool::new(true),
            is_restarting: AtomicBool::new(false),
            last_mouse_root: (0.0, 0.0),
            drag_ctl: None,
            message: SharedMessage::default(),
            secondary_bars: HashMap::new(),
            secondary_bar_failures: HashMap::new(),
            secondary_bar_retry_after: HashMap::new(),
            last_key_grab_refresh_at: None,
            pending_bar_updates: HashSet::new(),
            minimized_projection_epochs: HashMap::new(),
            reconciled_minimized_target_generations: HashMap::new(),
            minimized_dock_shelves: HashMap::new(),
            active_minimized_preview: None,
            active_minimized_preview_generation: None,
            suppress_mouse_focus_until: None,
            suppress_layout_animation: false,
            last_stacking: SecondaryMap::new(),
            scratchpads: HashMap::new(),
            scratchpad_pending: crate::jwm::scratchpad_pending::ScratchpadPendingRegistry::default(
            ),
            animations: AnimationManager::new(),
            hidden_client_park_retries: crate::jwm::monitor::HiddenClientParkRetries::default(),
            key_bindings: Vec::new(),
            chord_compiled: None,
            chord_armed_until: None,
            do_not_disturb: false,
            debug_hud_on: false,
            external_struts: HashMap::new(),
            ipc_server: None,
            config_reload_tracker: crate::jwm::lifecycle::ConfigReloadTracker::new(None),
            config_last_modified: None,
            config_reload_debounce: None,
            config_reload_count: 0,
            config_reload_last_unix_ms: None,
            config_reload_last_success: None,
            config_reload_last_error: None,
            layout_persist_dirty: None,
            override_redirect_windows: HashSet::new(),
            or_window_geometries: HashMap::new(),
            scrolling_states: HashMap::new(),
            last_night_light_update: None,
            night_light_override: None,
            last_battery_poll: None,
            last_idle_poll: None,
            idle: crate::jwm::features::idle::IdleTracker::default(),
            idle_inhibited: false,
            system_ui_dirty: false,
            server_saver_suppressed: false,
            features: FeatureStates::new(),
            event_coalescer:
                crate::backend::compositor_common::event_coalescer::EventCoalescer::new(),
            pending_pings: HashMap::new(),
            unresponsive_windows: HashSet::new(),
            last_ping_time: None,
            last_user_activity_time: 0,
        }
    }

    fn output(id: u64, x: i32) -> OutputInfo {
        OutputInfo {
            id: OutputId(id),
            name: format!("test-{id}"),
            x,
            y: 0,
            width: 1200,
            height: 900,
            scale: 1.0,
            refresh_rate: 60_000,
            hdr_capable: false,
            hdr_metadata: None,
            identity: crate::backend::api::OutputIdentity::connector_only(format!("test-{id}")),
        }
    }

    fn jwm_with_cross_monitor_scratchpad(
        minimized: bool,
    ) -> (Jwm, ClientKey, WindowId, crate::core::models::MonitorKey) {
        let mut jwm = empty_jwm();
        jwm.add_monitor(output(1, 0));
        jwm.add_monitor(output(2, 1200));
        let source = jwm.state.monitor_order[0];
        let target = jwm.state.monitor_order[1];
        jwm.state.sel_mon = Some(target);
        if let Some(monitor) = jwm.state.monitors.get_mut(target) {
            monitor.view_tag(0b10, false);
        }

        let current_window = WindowId::from_raw(0x701);
        let mut current = WMClient::new(current_window);
        current.mon = Some(target);
        current.state.tags = 0b10;
        current.geometry.x = 1250;
        current.geometry.y = 80;
        current.geometry.w = 700;
        current.geometry.h = 500;
        let current_key = jwm.insert_client(current);
        jwm.attach_to_monitor(current_key, target);
        if let Some(monitor) = jwm.state.monitors.get_mut(target) {
            monitor.set_selected_client_for_current_tag(Some(current_key));
        }

        let scratchpad_window = WindowId::from_raw(0x702);
        let mut scratchpad = WMClient::new(scratchpad_window);
        scratchpad.mon = Some(source);
        scratchpad.state.tags = 0;
        scratchpad.state.is_floating = true;
        scratchpad.state.is_hidden = minimized;
        scratchpad.state.minimized_order = u64::from(minimized) * 9;
        scratchpad.geometry.x = if minimized { -600 } else { 100 };
        scratchpad.geometry.old_x = 100;
        scratchpad.geometry.y = 100;
        scratchpad.geometry.old_y = 100;
        scratchpad.geometry.w = 300;
        scratchpad.geometry.h = 400;
        let scratchpad_key = jwm.insert_client(scratchpad);
        jwm.attach_to_monitor(scratchpad_key, source);
        jwm.scratchpads.insert("term".into(), scratchpad_key);

        (jwm, scratchpad_key, scratchpad_window, target)
    }

    #[test]
    fn reveal_places_a_parked_scratchpad_on_the_current_monitor_and_tag() {
        let (mut jwm, scratchpad, window, target) = jwm_with_cross_monitor_scratchpad(false);
        let mut backend = ScratchpadBackend::new();

        assert!(jwm.reveal_and_focus(&mut backend, window).unwrap());

        let client = &jwm.state.clients[scratchpad];
        assert_eq!(client.mon, Some(target));
        assert_eq!(client.state.tags, 0b10);
        assert!(!client.state.is_hidden);
        assert_eq!(jwm.state.monitors[target].sel, Some(scratchpad));
        assert_eq!(backend.focused, vec![Some(window)]);
    }

    #[test]
    fn toggle_restores_a_parked_minimized_scratchpad_once_without_appear_animation() {
        let (mut jwm, scratchpad, window, target) = jwm_with_cross_monitor_scratchpad(true);
        let mut backend = ScratchpadBackend::new();
        let source = jwm.state.monitor_order[0];
        let area = jwm.monitor_work_area(target).unwrap();
        let expected_width = area.w.saturating_mul(4) / 5;
        let expected_height = area.h.saturating_mul(4) / 5;

        jwm.togglescratchpad(&mut backend, &WMArgEnum::StringVec(vec!["term".to_owned()]))
            .unwrap();

        let client = &jwm.state.clients[scratchpad];
        assert_eq!(client.mon, Some(target));
        assert_eq!(client.state.tags, 0b10);
        assert!(!client.state.is_hidden);
        assert_eq!(client.state.minimized_order, 0);
        assert_eq!(jwm.state.monitors[target].sel, Some(scratchpad));
        assert!(!jwm.state.monitor_clients[source].contains(&scratchpad));
        assert!(jwm.state.monitor_clients[target].contains(&scratchpad));
        assert_eq!(
            (
                client.geometry.x,
                client.geometry.y,
                client.geometry.w,
                client.geometry.h
            ),
            (
                area.x + (area.w - expected_width) / 2,
                area.y + (area.h - expected_height) / 2,
                expected_width,
                expected_height,
            )
        );
        assert_eq!(
            backend
                .minimized
                .iter()
                .filter(|&&(candidate, minimized)| candidate == window && !minimized)
                .count(),
            1,
            "the shared minimized transition must own the restore"
        );
        assert_eq!(backend.focused, vec![Some(window)]);
        assert!(
            jwm.animations
                .active
                .get(&scratchpad)
                .is_none_or(|animation| animation.kind != AnimationKind::Appear),
            "reverse Genie must not be combined with scratchpad Appear"
        );
    }

    #[test]
    fn failed_cross_monitor_restore_returns_to_the_original_monitor_and_tag() {
        let mut jwm = empty_jwm();
        jwm.add_monitor(output(1, 0));
        jwm.add_monitor(output(2, 1200));
        let source = jwm.state.monitor_order[0];
        let target = jwm.state.monitor_order[1];
        jwm.state.sel_mon = Some(source);

        let source_window = WindowId::from_raw(0x710);
        let mut source_client = WMClient::new(source_window);
        source_client.mon = Some(source);
        source_client.state.tags = jwm.state.monitors[source].get_active_tags();
        source_client.geometry.x = 100;
        source_client.geometry.y = 100;
        source_client.geometry.w = 600;
        source_client.geometry.h = 420;
        let source_client = jwm.insert_client(source_client);
        jwm.attach_to_monitor(source_client, source);
        jwm.state.monitors[source].set_selected_client_for_current_tag(Some(source_client));

        let sticky_window = WindowId::from_raw(0x711);
        let mut sticky = WMClient::new(sticky_window);
        sticky.mon = Some(target);
        sticky.state.tags = jwm.state.monitors[target].get_active_tags();
        sticky.state.is_sticky = true;
        sticky.geometry.x = 1300;
        sticky.geometry.y = 100;
        sticky.geometry.w = 500;
        sticky.geometry.h = 360;
        let sticky = jwm.insert_client(sticky);
        jwm.attach_to_monitor(sticky, target);
        jwm.state.monitors[target].set_selected_client_for_current_tag(Some(sticky));

        let target_window = WindowId::from_raw(0x712);
        let restore_rect = Rect::new(1420, 160, 680, 500);
        let mut target_client = WMClient::new(target_window);
        target_client.mon = Some(target);
        target_client.state.tags = 0b10;
        target_client.state.is_floating = true;
        target_client.state.is_hidden = true;
        target_client.state.minimized_order = 41;
        target_client.geometry.x = -900;
        target_client.geometry.y = restore_rect.y;
        target_client.geometry.w = restore_rect.w;
        target_client.geometry.h = restore_rect.h;
        target_client.geometry.hidden_x = Some(-900);
        target_client.geometry.hidden_restore_rect = Some(restore_rect);
        let target_client = jwm.insert_client(target_client);
        jwm.attach_to_monitor(target_client, target);

        let target_state = jwm.state.monitors[target].clone();
        let target_stack = jwm.state.monitor_stack[target].clone();
        let sticky_tags = jwm.state.clients[sticky].state.tags;
        let mut backend = ScratchpadBackend::new();
        backend.fail_focus_for = Some(target_window);

        assert!(jwm.reveal_and_focus(&mut backend, target_window).is_err());
        assert_eq!(jwm.state.sel_mon, Some(source));
        assert_eq!(jwm.get_selected_client_key(), Some(source_client));
        assert_eq!(jwm.state.monitors[target], target_state);
        assert_eq!(jwm.state.monitor_stack[target], target_stack);
        assert_eq!(jwm.state.clients[sticky].state.tags, sticky_tags);
        let client = &jwm.state.clients[target_client];
        assert!(client.state.is_hidden);
        assert_eq!(client.state.minimized_order, 41);
        assert_eq!(client.geometry.hidden_restore_rect, Some(restore_rect));
        assert_eq!(backend.focused.last(), Some(&Some(source_window)));
        assert!(
            backend
                .minimized
                .iter()
                .all(|&(window, minimized)| window != target_window || minimized),
            "failed navigation must not release the retained Dock visual"
        );
    }

    #[test]
    fn failed_minimized_scratchpad_restore_keeps_its_committed_target_ownership() {
        let (mut jwm, scratchpad, window, target) = jwm_with_cross_monitor_scratchpad(true);
        let source = jwm.state.monitor_order[0];
        let current = jwm
            .get_selected_client_key()
            .expect("current target client");
        let target_num = jwm.state.monitors[target].num;
        let target_shelf = crate::backend::api::CompositorRect::new(1500.0, 32.0, 320.0, 80.0);
        jwm.minimized_dock_shelves.insert(target_num, target_shelf);
        let mut backend = ScratchpadBackend::new();
        backend.fail_focus_for = Some(window);

        assert!(jwm.reveal_and_focus(&mut backend, window).is_err());

        let client = &jwm.state.clients[scratchpad];
        assert!(client.state.is_hidden);
        assert_eq!(client.state.minimized_order, 9);
        assert_eq!(client.mon, Some(target));
        assert_eq!(client.state.tags, 0b10);
        assert!(client.geometry.hidden_restore_rect.is_some());
        assert!(!jwm.state.monitor_clients[source].contains(&scratchpad));
        assert!(jwm.state.monitor_clients[target].contains(&scratchpad));
        assert_eq!(jwm.state.sel_mon, Some(target));
        assert_eq!(jwm.get_selected_client_key(), Some(current));
        assert_eq!(
            backend.dock_targets.last(),
            Some(&(window, Some(target_shelf))),
            "failed restore must keep the migrated Dock target owned by the target bar"
        );
        assert!(
            backend
                .minimized
                .iter()
                .all(|&(candidate, minimized)| candidate != window || minimized)
        );
    }

    #[test]
    fn failed_ineligible_scratchpad_restore_never_creates_a_target_bar_ghost() {
        let (mut jwm, scratchpad, window, target) = jwm_with_cross_monitor_scratchpad(true);
        jwm.state.clients[scratchpad].state.skip_taskbar = true;
        let target_num = jwm.state.monitors[target].num;
        jwm.minimized_dock_shelves.insert(
            target_num,
            crate::backend::api::CompositorRect::new(1500.0, 32.0, 320.0, 80.0),
        );
        let mut backend = ScratchpadBackend::new();
        backend.fail_focus_for = Some(window);

        assert!(jwm.reveal_and_focus(&mut backend, window).is_err());

        let client = &jwm.state.clients[scratchpad];
        assert!(client.state.is_hidden);
        assert!(client.state.skip_taskbar);
        assert_eq!(client.mon, Some(target));
        assert!(
            backend
                .dock_targets
                .iter()
                .filter(|(candidate, _)| *candidate == window)
                .all(|(_, target)| target.is_none()),
            "a Dock-ineligible scratchpad must remain targetless when its cross-monitor restore rolls back: {:?}",
            backend.dock_targets
        );
    }
}
