// Monitor management operations: output handling, geometry, and client distribution

use crate::backend::api::Backend;
use crate::backend::common_define::OutputId;
use crate::config::CONFIG;
use crate::core::models::{ClientKey, MonitorKey};
use crate::Jwm;
use log::{info, warn};

impl Jwm {
    pub(crate) fn add_monitor(&mut self, info: crate::backend::api::OutputInfo) {
        info!("[add_monitor] Adding output: {:?}", info);
        let mut m = self.createmon(CONFIG.load().show_bar());

        // 设置 Monitor 几何属性
        m.geometry.m_x = info.x;
        m.geometry.m_y = info.y;
        m.geometry.m_w = info.width;
        m.geometry.m_h = info.height;
        // 工作区通常等于屏幕区，减去 Bar 的计算在 layout 中动态进行
        m.geometry.w_x = info.x;
        m.geometry.w_y = info.y;
        m.geometry.w_w = info.width;
        m.geometry.w_h = info.height;
        m.num = self.state.monitors.len() as i32;

        let key = self.state.monitors.insert(m);
        self.state.monitor_order.push(key);
        self.state.output_map.insert(key, info.id);
        self.state.monitor_clients.insert(key, Vec::new());
        self.state.monitor_stack.insert(key, Vec::new());

        if self.state.sel_mon.is_none() {
            self.state.sel_mon = Some(key);
        }
    }

    pub(crate) fn handle_output_added(
        &mut self,
        backend: &mut dyn Backend,
        info: crate::backend::api::OutputInfo,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Jwm::new() already calls add_monitor for every output returned by
        // enumerate_outputs().  The udev backend then fires OutputAdded for
        // the same outputs when the event loop starts.  Skip the duplicate.
        if self.state.output_map.values().any(|&id| id == info.id) {
            return Ok(());
        }
        self.add_monitor(info);

        // Wayland clients can appear before outputs are fully initialized (early autostart).
        // Those clients end up with `mon=None`, meaning JWM will treat them as invisible:
        // - click-to-focus won't stick (focus() falls back to visible clients)
        // - arrange() won't resize them
        // The udev backend still renders them, so they look "stuck" at their initial size.
        self.attach_unassigned_clients_to_selected_monitor();

        self.arrange(backend, None);
        Ok(())
    }

    pub(crate) fn attach_unassigned_clients_to_selected_monitor(&mut self) {
        let target_mon_key = self
            .state
            .sel_mon
            .or_else(|| self.state.monitor_order.first().copied());

        let Some(mon_key) = target_mon_key else {
            return;
        };

        let target_tags = self
            .state
            .monitors
            .get(mon_key)
            .map(|m| m.get_active_tags())
            .unwrap_or(1);

        let unassigned: Vec<ClientKey> = self
            .state
            .clients
            .iter()
            .filter_map(|(k, c)| if c.mon.is_none() { Some(k) } else { None })
            .collect();

        for client_key in unassigned {
            if let Some(client) = self.state.clients.get_mut(client_key) {
                client.mon = Some(mon_key);
                if client.state.tags == 0 {
                    client.state.tags = target_tags;
                }
            }

            // Ensure this client participates in layout/focus stacks.
            self.attach_to_monitor(client_key, mon_key);
        }
    }

    pub(crate) fn handle_output_removed(
        &mut self,
        backend: &mut dyn Backend,
        id: OutputId,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[handle_output_removed] Removing output {:?}", id);

        // 查找对应的 MonitorKey
        let mon_key_opt = self
            .state
            .output_map
            .iter()
            .find(|&(_, &oid)| oid == id)
            .map(|(k, _)| k);

        if let Some(mon_key) = mon_key_opt {
            self.move_clients_to_first_monitor(mon_key);

            // 移除数据
            self.state.monitors.remove(mon_key);
            self.state.output_map.remove(mon_key);
            self.state.monitor_clients.remove(mon_key);
            self.state.monitor_stack.remove(mon_key);
            self.state.monitor_order.retain(|&k| k != mon_key);

            // 如果删除了当前选中的 Monitor，重置选中
            if self.state.sel_mon == Some(mon_key) {
                self.state.sel_mon = self.state.monitor_order.first().copied();
                self.focus(backend, None)?;
            }

            self.arrange(backend, None);
        }
        Ok(())
    }

    pub(crate) fn handle_output_changed(
        &mut self,
        backend: &mut dyn Backend,
        info: crate::backend::api::OutputInfo,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mon_key_opt = self
            .state
            .output_map
            .iter()
            .find(|&(_, &oid)| oid == info.id)
            .map(|(k, _)| k);
        if let Some(mon_key) = mon_key_opt {
            if let Some(m) = self.state.monitors.get_mut(mon_key) {
                m.geometry.m_x = info.x;
                m.geometry.m_y = info.y;
                m.geometry.m_w = info.width;
                m.geometry.m_h = info.height;
                m.geometry.w_x = info.x;
                m.geometry.w_y = info.y;
                m.geometry.w_w = info.width;
                m.geometry.w_h = info.height;
            }
            self.arrange(backend, Some(mon_key));
        }
        Ok(())
    }
    pub(crate) fn updategeom(&mut self, backend: &mut dyn Backend) -> bool {
        info!("[updategeom]");
        let outputs = backend.output_ops().enumerate_outputs();

        let dirty = if outputs.len() <= 1 {
            self.setup_single_monitor()
        } else {
            let mons: Vec<(i32, i32, i32, i32)> = outputs
                .iter()
                .map(|o| (o.x, o.y, o.width, o.height))
                .collect();
            self.setup_multiple_monitors(mons)
        };

        if dirty {
            let root_window = backend.root_window();
            self.state.sel_mon = self.wintomon(backend, root_window);
            if self.state.sel_mon.is_none() && !self.state.monitor_order.is_empty() {
                self.state.sel_mon = self.state.monitor_order.first().copied();
            }
        }

        // Update compositor with current monitor geometries (for per-monitor wallpaper)
        {
            let mon_list: Vec<(u32, i32, i32, u32, u32)> = self
                .state
                .monitor_order
                .iter()
                .enumerate()
                .filter_map(|(idx, &mk)| {
                    self.state.monitors.get(mk).map(|m| {
                        (
                            idx as u32,
                            m.geometry.m_x,
                            m.geometry.m_y,
                            m.geometry.m_w.max(1) as u32,
                            m.geometry.m_h.max(1) as u32,
                        )
                    })
                })
                .collect();
            backend.compositor_set_monitors(&mon_list);
        }

        dirty
    }

    pub(crate) fn setup_single_monitor(&mut self) -> bool {
        let mut dirty = false;

        if self.state.monitor_order.is_empty() {
            let new_monitor = self.createmon(CONFIG.load().show_bar());
            let mon_key = self.insert_monitor(new_monitor);
            self.state.sel_mon = Some(mon_key);
            dirty = true;
        }

        if let Some(&mon_key) = self.state.monitor_order.first() {
            if let Some(monitor) = self.state.monitors.get_mut(mon_key) {
                if monitor.geometry.m_w != self.s_w || monitor.geometry.m_h != self.s_h {
                    dirty = true;
                    monitor.num = 0;
                    monitor.geometry.m_x = 0;
                    monitor.geometry.w_x = 0;
                    monitor.geometry.m_y = 0;
                    monitor.geometry.w_y = 0;
                    monitor.geometry.m_w = self.s_w;
                    monitor.geometry.w_w = self.s_w;
                    monitor.geometry.m_h = self.s_h;
                    monitor.geometry.w_h = self.s_h;
                }
            }
        }

        dirty
    }

    pub(crate) fn setup_multiple_monitors(&mut self, monitors: Vec<(i32, i32, i32, i32)>) -> bool {
        let mut dirty = false;
        let num_detected_monitors = monitors.len();
        let current_num_monitors = self.state.monitor_order.len();

        if num_detected_monitors > current_num_monitors {
            dirty = true;
            for _ in current_num_monitors..num_detected_monitors {
                let new_monitor = self.createmon(CONFIG.load().show_bar());
                let mon_key = self.insert_monitor(new_monitor);
                info!(
                    "[setup_multiple_monitors] Created new monitor {:?}",
                    mon_key
                );
            }
        }

        for (i, &(x, y, w, h)) in monitors.iter().enumerate() {
            if let Some(&mon_key) = self.state.monitor_order.get(i) {
                if let Some(monitor) = self.state.monitors.get_mut(mon_key) {
                    if monitor.geometry.m_x != x
                        || monitor.geometry.m_y != y
                        || monitor.geometry.m_w != w
                        || monitor.geometry.m_h != h
                    {
                        dirty = true;
                        monitor.num = i as i32;
                        monitor.geometry.m_x = x;
                        monitor.geometry.w_x = x;
                        monitor.geometry.m_y = y;
                        monitor.geometry.w_y = y;
                        monitor.geometry.m_w = w;
                        monitor.geometry.w_w = w;
                        monitor.geometry.m_h = h;
                        monitor.geometry.w_h = h;
                    }
                }
            }
        }

        if num_detected_monitors < current_num_monitors {
            dirty = true;
            self.remove_excess_monitors(num_detected_monitors);
        }

        dirty
    }

    pub(crate) fn remove_excess_monitors(&mut self, target_count: usize) {
        while self.state.monitor_order.len() > target_count {
            if let Some(mon_key_to_remove) = self.state.monitor_order.pop() {
                self.move_clients_to_first_monitor(mon_key_to_remove);

                if self.state.sel_mon == Some(mon_key_to_remove) {
                    self.state.sel_mon = self.state.monitor_order.first().copied();
                }

                self.state.monitors.remove(mon_key_to_remove);
                self.state.monitor_clients.remove(mon_key_to_remove);
                self.state.monitor_stack.remove(mon_key_to_remove);

                info!(
                    "[remove_excess_monitors] Removed monitor {:?}",
                    mon_key_to_remove
                );
            }
        }
    }

    pub(crate) fn move_clients_to_first_monitor(&mut self, from_monitor_key: MonitorKey) {
        let target_monitor_key = if let Some(&first_mon_key) = self.state.monitor_order.first() {
            first_mon_key
        } else {
            warn!("[move_clients_to_first_monitor] No target monitor available");
            return;
        };

        let clients_to_move: Vec<ClientKey> = self
            .state
            .monitor_clients
            .get(from_monitor_key)
            .cloned()
            .unwrap_or_default();

        let target_tags = if let Some(target_monitor) = self.state.monitors.get(target_monitor_key)
        {
            target_monitor.get_active_tags()
        } else {
            1
        };

        for client_key in clients_to_move {
            if let Some(client) = self.state.clients.get_mut(client_key) {
                client.mon = Some(target_monitor_key);
                client.state.tags = target_tags;
            }

            self.detach_from_monitor(client_key, from_monitor_key);

            self.attach_to_monitor(client_key, target_monitor_key);

            info!(
                "[move_clients_to_first_monitor] Moved client {:?} from monitor {:?} to {:?}",
                client_key, from_monitor_key, target_monitor_key
            );
        }
    }
}
