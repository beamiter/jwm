//! 标签（Tag）管理模块
//!
//! 这个模块包含所有与窗口标签和工作区管理相关的功能

use crate::backend::api::Backend;
use crate::config::CONFIG;
use crate::core::models::{ClientKey, MonitorKey};
use crate::jwm::Jwm;
use crate::jwm::types::WMArgEnum;

impl Jwm {
    /// 将当前选中的窗口移动到指定标签
    ///
    /// 参数 arg 应为 `UInt(tag_mask)`，表示目标标签掩码
    pub fn tag(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // info!("[tag]");
        if let WMArgEnum::UInt(ui) = *arg {
            let sel_client_key = self.get_selected_client_key();
            let target_tag = ui & CONFIG.load().tagmask();

            if let Some(client_key) = sel_client_key {
                if target_tag > 0 {
                    if let Some(client) = self.state.clients.get_mut(client_key) {
                        client.state.tags = target_tag;
                    }
                    let _ = self.setclienttagprop(backend, client_key);

                    self.focus(backend, None)?;
                    self.arrange(backend, self.state.sel_mon);
                }
            }
        }
        Ok(())
    }

    /// 将当前选中的窗口移动到指定显示器
    ///
    /// 参数 arg 应为 `Int(i)`，表示方向：+1 下一个，-1 上一个
    pub fn tagmon(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // info!("[tagmon]");

        let sel_client_key = self.get_selected_client_key();
        if sel_client_key.is_none() {
            return Ok(());
        }
        if self.state.monitor_order.len() <= 1 {
            return Ok(());
        }
        if let WMArgEnum::Int(i) = *arg {
            let target_mon = self.dirtomon(&i);
            if let (Some(client_key), Some(target_mon_key)) = (sel_client_key, target_mon) {
                self.sendmon(backend, Some(client_key), Some(target_mon_key));
            }
        }
        Ok(())
    }

    /// 将指定窗口发送到目标显示器
    ///
    /// 内部函数，由 tagmon 调用
    pub(crate) fn sendmon(
        &mut self,
        backend: &mut dyn Backend,
        client_key_opt: Option<ClientKey>,
        target_mon_opt: Option<MonitorKey>,
    ) {
        // info!("[sendmon]");

        let client_key = match client_key_opt {
            Some(key) => key,
            None => return,
        };

        let target_mon_key = match target_mon_opt {
            Some(key) => key,
            None => return,
        };

        if let Some(client) = self.state.clients.get(client_key) {
            if client.mon == Some(target_mon_key) {
                return;
            }
        } else {
            return;
        }

        let _ = self.unfocus_client(backend, client_key, true);

        let source_mon = self.state.clients.get(client_key).and_then(|c| c.mon);

        self.detach(client_key);
        self.detachstack(client_key);

        // 把该 client 从源显示器的选中记录(monitor.sel + 全部 pertag.sel)中清除,
        // 否则切回源显示器的旧 tag 时会读到一个已迁走的 key。
        if let Some(src) = source_mon {
            if let Some(m) = self.state.monitors.get_mut(src) {
                m.clear_selection_of(client_key);
            }
        }

        if let Some(client) = self.state.clients.get_mut(client_key) {
            client.mon = Some(target_mon_key);
        }

        if let Some(target_monitor) = self.state.monitors.get(target_mon_key) {
            let target_tags = target_monitor.get_active_tags();

            if let Some(client) = self.state.clients.get_mut(client_key) {
                client.state.tags = target_tags;
            }
        }

        self.attach_back(client_key);
        self.attachstack(client_key);

        let _ = self.setclienttagprop(backend, client_key);

        let _ = self.focus(backend, None);
        self.arrange(backend, None);
    }

    /// 设置窗口的标签属性（EWMH）
    ///
    /// 更新 _NET_WM_DESKTOP 等 X11 属性
    pub(crate) fn setclienttagprop(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(client) = self.state.clients.get(client_key) {
            let monitor_num = client
                .mon
                .and_then(|mk| self.state.monitors.get(mk))
                .map(|m| m.num as u32)
                .unwrap_or(0);

            backend.property_ops().set_client_info_props(
                client.win,
                client.state.tags,
                monitor_num,
            )?;
        }
        Ok(())
    }
}
