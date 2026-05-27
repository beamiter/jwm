//! 窗口属性处理模块
//!
//! 这个模块包含所有窗口属性变化的处理函数

use crate::backend::api::{AllowedAction, Backend};
use crate::backend::common_define::WindowId;
use crate::core::models::ClientKey;
use crate::jwm::types::STEXT_MAX_LEN;
use crate::jwm::Jwm;
use log::debug;

impl Jwm {
    /// 处理窗口 transient_for 属性变化
    ///
    /// 如果窗口设置了 transient_for 属性（对话框、弹出窗口等），
    /// 自动将其切换为浮动状态
    pub(crate) fn handle_transient_for_change(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // info!("[handle_transient_for_change]");
        let (is_floating, win, client_name) =
            if let Some(client) = self.state.clients.get(client_key) {
                (client.state.is_floating, client.win, client.name.clone())
            } else {
                return Ok(());
            };

        if !is_floating {
            let transient_for = self.get_transient_for(backend, win);
            if let Some(parent_window) = transient_for {
                if self.wintoclient(parent_window).is_some() {
                    if let Some(client) = self.state.clients.get_mut(client_key) {
                        client.state.is_floating = true;
                    }

                    self.reorder_client_in_monitor_groups(client_key);

                    debug!(
                        "Window '{}' became floating due to transient_for: {:?}",
                        client_name, parent_window
                    );

                    let mon_key = self.state.clients.get(client_key).and_then(|c| c.mon);
                    self.arrange(backend, mon_key);
                }
            }
        }
        Ok(())
    }

    /// 处理窗口 normal hints（尺寸提示）属性变化
    ///
    /// 标记尺寸提示缓存为无效，下次使用时重新获取
    pub(crate) fn handle_normal_hints_change(
        &mut self,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(client) = self.state.clients.get_mut(client_key) {
            client.size_hints.hints_valid = false;
        }
        Ok(())
    }

    /// 处理窗口 WM hints 属性变化
    ///
    /// 包括紧急状态（urgent）、输入模式等
    pub(crate) fn handle_wm_hints_change(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.updatewmhints(backend, client_key);
        self.mark_bar_update_needed_if_visible(None);

        if let Some(client) = self.state.clients.get(client_key) {
            debug!("WM hints updated for window {:?}", client.win);
        }
        Ok(())
    }

    /// 更新窗口标题并广播 IPC 事件
    pub(crate) fn updatetitle_by_key(&mut self, backend: &mut dyn Backend, client_key: ClientKey) {
        let win = if let Some(client) = self.state.clients.get(client_key) {
            client.win
        } else {
            return;
        };
        let new_title = self.fetch_window_title(backend, win);
        let title_for_event;
        if let Some(client) = self.state.clients.get_mut(client_key) {
            client.name = new_title;
            title_for_event = client.name.clone();
            debug!("Updated title for window {:?}: '{}'", win, client.name);
        } else {
            return;
        }
        self.broadcast_ipc_event(
            "window/title",
            serde_json::json!({
                "id": win.raw(), "name": title_for_event,
            }),
        );
    }

    /// 截断字符串到指定字符数
    ///
    /// 用于限制窗口标题长度，避免过长的标题占用过多资源
    fn truncate_chars(input: String, max_chars: usize) -> String {
        if input.is_empty() {
            return input;
        }
        let mut count = 0usize;
        let mut truncate_at = input.len();
        for (idx, _) in input.char_indices() {
            if count >= max_chars {
                truncate_at = idx;
                break;
            }
            count += 1;
        }
        let mut s = input;
        s.truncate(truncate_at);
        s
    }

    /// 从窗口获取标题文本
    pub(crate) fn fetch_window_title(&mut self, backend: &mut dyn Backend, window: WindowId) -> String {
        let title = backend.property_ops().get_title(window);
        Self::truncate_chars(title, STEXT_MAX_LEN)
    }

    /// 处理窗口标题属性变化
    ///
    /// 更新标题并在需要时刷新状态栏
    pub(crate) fn handle_title_change(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.updatetitle_by_key(backend, client_key);

        let should_update_bar = self.is_client_selected(client_key);

        if should_update_bar {
            let monitor_id = self
                .state
                .clients
                .get(client_key)
                .and_then(|client| client.mon)
                .and_then(|mon_key| self.state.monitors.get(mon_key))
                .map(|monitor| monitor.num);

            if let Some(id) = monitor_id {
                self.mark_bar_update_needed_if_visible(Some(id));

                if let Some(client) = self.state.clients.get(client_key) {
                    debug!(
                        "Title updated for selected window {:?}, updating status bar",
                        client.win
                    );
                }
            }
        }
        Ok(())
    }

    /// 处理窗口类型（_NET_WM_WINDOW_TYPE）属性变化
    pub(crate) fn handle_window_type_change(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.updatewindowtype(backend, client_key);

        if let Some(client) = self.state.clients.get(client_key) {
            debug!("Window type updated for window {:?}", client.win);
        }
        Ok(())
    }

    /// 处理窗口类名（WM_CLASS）属性变化
    pub(crate) fn handle_class_change(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let win = self
            .state
            .clients
            .get(client_key)
            .map(|c| c.win)
            .ok_or("Client not found")?;

        let (inst, cls) = backend.property_ops().get_class(win);
        if let Some(client) = self.state.clients.get_mut(client_key) {
            if !inst.is_empty() {
                client.instance = inst;
            }
            if !cls.is_empty() {
                client.class = cls;
            }
        }
        Ok(())
    }

    pub(crate) fn handle_motif_hints_change(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let win = self
            .state
            .clients
            .get(client_key)
            .map(|c| c.win)
            .ok_or("Client not found")?;

        if let Some(motif) = backend.property_ops().get_motif_hints(win) {
            if let Some(client) = self.state.clients.get_mut(client_key) {
                client.state.no_decorations = motif.decorations_none();
                if client.state.no_decorations {
                    client.geometry.border_w = 0;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn handle_gtk_frame_extents_change(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let win = self
            .state
            .clients
            .get(client_key)
            .map(|c| c.win)
            .ok_or("Client not found")?;

        if let Some(_extents) = backend.property_ops().get_gtk_frame_extents(win) {
            if let Some(client) = self.state.clients.get_mut(client_key) {
                client.state.no_decorations = true;
                client.geometry.border_w = 0;
            }
        }
        Ok(())
    }

    pub(crate) fn handle_bypass_compositor_change(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let win = self
            .state
            .clients
            .get(client_key)
            .map(|c| c.win)
            .ok_or("Client not found")?;

        let _bypass = backend.property_ops().get_bypass_compositor(win);
        Ok(())
    }

    pub(crate) fn apply_motif_hints(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) {
        let win = match self.state.clients.get(client_key) {
            Some(c) => c.win,
            None => return,
        };
        if let Some(motif) = backend.property_ops().get_motif_hints(win) {
            if let Some(client) = self.state.clients.get_mut(client_key) {
                if motif.decorations_none() {
                    client.state.no_decorations = true;
                    client.geometry.border_w = 0;
                }
            }
        }
    }

    pub(crate) fn apply_gtk_frame_extents(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) {
        let win = match self.state.clients.get(client_key) {
            Some(c) => c.win,
            None => return,
        };
        if backend.property_ops().get_gtk_frame_extents(win).is_some() {
            if let Some(client) = self.state.clients.get_mut(client_key) {
                client.state.no_decorations = true;
                client.geometry.border_w = 0;
            }
        }
    }

    pub(crate) fn set_initial_frame_extents(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) {
        let (win, bw) = match self.state.clients.get(client_key) {
            Some(c) => (c.win, c.geometry.border_w),
            None => return,
        };
        let _ = backend
            .property_ops()
            .set_frame_extents(win, bw as u32, bw as u32, bw as u32, bw as u32);
    }

    pub(crate) fn set_initial_allowed_actions(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) {
        let win = match self.state.clients.get(client_key) {
            Some(c) => c.win,
            None => return,
        };
        let actions = [
            AllowedAction::Move,
            AllowedAction::Resize,
            AllowedAction::Minimize,
            AllowedAction::MaximizeHorz,
            AllowedAction::MaximizeVert,
            AllowedAction::Fullscreen,
            AllowedAction::Close,
            AllowedAction::Stick,
            AllowedAction::Above,
            AllowedAction::Below,
        ];
        let _ = backend.property_ops().set_allowed_actions(win, &actions);
    }

    pub(crate) fn read_sync_counter(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) {
        let win = match self.state.clients.get(client_key) {
            Some(c) => c.win,
            None => return,
        };
        if let Some(counter) = backend.property_ops().get_sync_counter(win) {
            if let Some(client) = self.state.clients.get_mut(client_key) {
                client.state.sync_counter = Some(counter);
                client.state.sync_value = 0;
            }
        }
    }
}
