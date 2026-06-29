//! 焦点管理模块
//!
//! 这个模块包含所有窗口焦点管理相关的功能

use crate::backend::api::Backend;
use crate::backend::common_define::WindowId;
use crate::config::CONFIG;
use crate::core::animation::AnimationKind;
use crate::core::models::ClientKey;
use crate::core::types::Rect;
use crate::jwm::Jwm;
use crate::jwm::types::WMArgEnum;
use log::info;
use std::collections::HashMap;
use std::time::Instant;

impl Jwm {
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
        let win = WindowId::from_raw(win_id);
        let client_key = self
            .wintoclient(win)
            .ok_or_else(|| format!("window {} not found", win_id))?;
        self.focus(backend, Some(client_key))?;
        if let Some(mon_key) = self.state.sel_mon {
            self.restack(backend, Some(mon_key))?;
        }
        Ok(())
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
