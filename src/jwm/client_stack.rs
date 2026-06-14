use crate::core::models::{ClientKey, MonitorKey};
use crate::jwm::Jwm;

impl Jwm {
    pub fn get_monitor_stack(&self, mon_key: MonitorKey) -> &[ClientKey] {
        self.state
            .monitor_stack
            .get(mon_key)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn attach_front(&mut self, client_key: ClientKey) {
        if let Some(client) = self.state.clients.get(client_key) {
            if let Some(mon_key) = client.mon {
                if let Some(client_list) = self.state.monitor_clients.get_mut(mon_key) {
                    client_list.insert(0, client_key);
                }
            }
        }
    }

    pub fn attach_back(&mut self, client_key: ClientKey) {
        if let Some(client) = self.state.clients.get(client_key) {
            if let Some(mon_key) = client.mon {
                if let Some(client_list) = self.state.monitor_clients.get_mut(mon_key) {
                    client_list.push(client_key);
                }
            }
        }
        self.reorder_client_in_monitor_groups(client_key);
    }

    pub fn detach(&mut self, client_key: ClientKey) {
        if let Some(client) = self.state.clients.get(client_key) {
            if let Some(mon_key) = client.mon {
                if let Some(client_list) = self.state.monitor_clients.get_mut(mon_key) {
                    if let Some(pos) = client_list.iter().position(|&k| k == client_key) {
                        client_list.remove(pos);
                    }
                }
            }
        }
    }

    pub fn reorder_client_in_monitor_groups(&mut self, client_key: ClientKey) {
        let (Some(mon_key), Some(is_floating)) = (
            self.state.clients.get(client_key).and_then(|c| c.mon),
            self.state
                .clients
                .get(client_key)
                .map(|c| c.state.is_floating),
        ) else {
            return;
        };

        let Some(client_list) = self.state.monitor_clients.get_mut(mon_key) else {
            return;
        };

        if let Some(pos) = client_list.iter().position(|&k| k == client_key) {
            client_list.remove(pos);
        }

        if is_floating {
            client_list.push(client_key);
            return;
        }

        let mut insert_pos = client_list.len();
        for (idx, &key) in client_list.iter().enumerate() {
            let other_is_floating = self
                .state
                .clients
                .get(key)
                .map(|c| c.state.is_floating)
                .unwrap_or(false);
            if other_is_floating {
                insert_pos = idx;
                break;
            }
        }

        client_list.insert(insert_pos, client_key);
    }

    pub fn attachstack(&mut self, client_key: ClientKey) {
        if let Some(client) = self.state.clients.get(client_key) {
            if let Some(mon_key) = client.mon {
                if let Some(stack_list) = self.state.monitor_stack.get_mut(mon_key) {
                    stack_list.insert(0, client_key);
                }
            }
        }
    }

    pub fn detach_from_monitor(&mut self, client_key: ClientKey, mon_key: MonitorKey) {
        if let Some(client_list) = self.state.monitor_clients.get_mut(mon_key) {
            client_list.retain(|&k| k != client_key);
        }
        if let Some(stack_list) = self.state.monitor_stack.get_mut(mon_key) {
            stack_list.retain(|&k| k != client_key);
        }
    }

    pub fn attach_to_monitor(&mut self, client_key: ClientKey, mon_key: MonitorKey) {
        if let Some(client_list) = self.state.monitor_clients.get_mut(mon_key) {
            client_list.push(client_key);
        }
        // 与新建窗口的 attachstack 保持一致:插入到聚焦栈"首部"(= 最近使用)。
        // 此前迁移窗口被 push 到栈尾,使其被当作最久未用,导致 find_visible_client
        // 的焦点回退顺序与 restack 的 Z 序和新建窗口表现不一致。
        if let Some(stack_list) = self.state.monitor_stack.get_mut(mon_key) {
            stack_list.insert(0, client_key);
        }
        self.reorder_client_in_monitor_groups(client_key);
    }

    pub fn detachstack(&mut self, client_key: ClientKey) {
        if let Some(client) = self.state.clients.get(client_key) {
            if let Some(mon_key) = client.mon {
                if let Some(stack_list) = self.state.monitor_stack.get_mut(mon_key) {
                    if let Some(pos) = stack_list.iter().position(|&k| k == client_key) {
                        stack_list.remove(pos);
                    }
                }
                let next_visible_client = self.find_next_visible_client_by_mon(mon_key);
                if let Some(monitor) = self.state.monitors.get_mut(mon_key) {
                    if monitor.sel == Some(client_key) {
                        monitor.sel = next_visible_client;
                    }
                }
            }
        }
    }
}
