use crate::config::{CONFIG, NewClientPosition};
use crate::core::models::{ClientKey, MonitorKey};
use crate::jwm::Jwm;

/// Index at which a new client joins a monitor client list of `len` entries
/// whose floating group starts at `first_floating`.
///
/// `sel_index` is the position of the focused client, and is only passed when
/// that client shares the newcomer's group; without it `AfterFocused` degrades
/// to `Master`. `Tail` never reaches here — it goes through `attach_back`.
fn new_client_insert_index(
    position: NewClientPosition,
    len: usize,
    first_floating: usize,
    is_floating: bool,
    sel_index: Option<usize>,
) -> usize {
    let first_floating = first_floating.min(len);
    let (group_start, group_end) = if is_floating {
        (first_floating, len)
    } else {
        (0, first_floating)
    };

    match position {
        NewClientPosition::AfterFocused => sel_index
            .filter(|&idx| (group_start..group_end).contains(&idx))
            .map_or(group_start, |idx| idx + 1),
        _ => group_start,
    }
}

impl Jwm {
    pub fn get_monitor_stack(&self, mon_key: MonitorKey) -> &[ClientKey] {
        const EMPTY_STACK: &[ClientKey] = &[];
        self.state
            .monitor_stack
            .get(mon_key)
            .map_or(EMPTY_STACK, Vec::as_slice)
    }

    pub fn attach_front(&mut self, client_key: ClientKey) {
        let Some(mon_key) = self
            .state
            .clients
            .get(client_key)
            .and_then(|client| client.mon)
        else {
            return;
        };
        let Some(client_list) = self.state.monitor_clients.get_mut(mon_key) else {
            return;
        };
        client_list.insert(0, client_key);
    }

    pub fn attach_back(&mut self, client_key: ClientKey) {
        if let Some(mon_key) = self
            .state
            .clients
            .get(client_key)
            .and_then(|client| client.mon)
            && let Some(client_list) = self.state.monitor_clients.get_mut(mon_key)
        {
            client_list.push(client_key);
        }
        self.reorder_client_in_monitor_groups(client_key);
    }

    /// Insert a freshly managed client into its monitor's client list according
    /// to `behavior.new_client_position` (default: master, i.e. the head).
    ///
    /// The list keeps tiled windows ahead of floating ones (see
    /// [`Self::reorder_client_in_monitor_groups`]), so every policy inserts
    /// inside the client's own group rather than blindly at index 0 / the end.
    pub fn attach_new_client(&mut self, client_key: ClientKey) {
        let position = CONFIG.load().new_client_position();
        if position == NewClientPosition::Tail {
            self.attach_back(client_key);
            return;
        }

        let Some(mon_key) = self
            .state
            .clients
            .get(client_key)
            .and_then(|client| client.mon)
        else {
            return;
        };
        let is_floating = self.is_client_floating(client_key);

        let Some(client_list) = self.state.monitor_clients.get(mon_key) else {
            return;
        };

        // Boundary between the tiled group and the trailing floating group.
        let first_floating = client_list
            .iter()
            .position(|&key| self.is_client_floating(key))
            .unwrap_or(client_list.len());

        let sel_index = self
            .state
            .monitors
            .get(mon_key)
            .and_then(|monitor| monitor.sel)
            .filter(|&sel| sel != client_key && self.is_client_floating(sel) == is_floating)
            .and_then(|sel| client_list.iter().position(|&key| key == sel));

        let insert_pos = new_client_insert_index(
            position,
            client_list.len(),
            first_floating,
            is_floating,
            sel_index,
        );

        if let Some(client_list) = self.state.monitor_clients.get_mut(mon_key) {
            client_list.insert(insert_pos, client_key);
        }
    }

    fn is_client_floating(&self, client_key: ClientKey) -> bool {
        self.state
            .clients
            .get(client_key)
            .is_some_and(|client| client.state.is_floating)
    }

    pub fn detach(&mut self, client_key: ClientKey) {
        if let Some(mon_key) = self
            .state
            .clients
            .get(client_key)
            .and_then(|client| client.mon)
            && let Some(client_list) = self.state.monitor_clients.get_mut(mon_key)
            && let Some(pos) = client_list.iter().position(|&key| key == client_key)
        {
            client_list.remove(pos);
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
        if let Some(mon_key) = self
            .state
            .clients
            .get(client_key)
            .and_then(|client| client.mon)
            && let Some(stack_list) = self.state.monitor_stack.get_mut(mon_key)
        {
            stack_list.insert(0, client_key);
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
        let Some(mon_key) = self
            .state
            .clients
            .get(client_key)
            .and_then(|client| client.mon)
        else {
            return;
        };

        if let Some(stack_list) = self.state.monitor_stack.get_mut(mon_key)
            && let Some(pos) = stack_list.iter().position(|&key| key == client_key)
        {
            stack_list.remove(pos);
        }

        let next_visible_client = self.find_next_visible_client_by_mon(mon_key);
        if let Some(monitor) = self.state.monitors.get_mut(mon_key)
            && monitor.sel == Some(client_key)
        {
            monitor.sel = next_visible_client;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{NewClientPosition, new_client_insert_index};

    // List of 5: three tiled, then two floating.
    const LEN: usize = 5;
    const FIRST_FLOATING: usize = 3;

    #[test]
    fn master_puts_new_clients_at_the_head_of_their_group() {
        // Tiled newcomers become master…
        assert_eq!(
            new_client_insert_index(
                NewClientPosition::Master,
                LEN,
                FIRST_FLOATING,
                false,
                Some(1)
            ),
            0
        );
        // …floating ones lead the floating group instead of the whole list.
        assert_eq!(
            new_client_insert_index(
                NewClientPosition::Master,
                LEN,
                FIRST_FLOATING,
                true,
                Some(4)
            ),
            FIRST_FLOATING
        );
    }

    #[test]
    fn after_focused_lands_behind_the_focused_client() {
        assert_eq!(
            new_client_insert_index(
                NewClientPosition::AfterFocused,
                LEN,
                FIRST_FLOATING,
                false,
                Some(1)
            ),
            2
        );
        assert_eq!(
            new_client_insert_index(
                NewClientPosition::AfterFocused,
                LEN,
                FIRST_FLOATING,
                true,
                Some(3)
            ),
            4
        );
    }

    #[test]
    fn after_focused_falls_back_to_master_without_a_usable_focus() {
        // Nothing focused.
        assert_eq!(
            new_client_insert_index(
                NewClientPosition::AfterFocused,
                LEN,
                FIRST_FLOATING,
                false,
                None
            ),
            0
        );
        // Focus sits in the other group: never cross the tiled/floating border.
        assert_eq!(
            new_client_insert_index(
                NewClientPosition::AfterFocused,
                LEN,
                FIRST_FLOATING,
                false,
                Some(4)
            ),
            0
        );
    }

    #[test]
    fn empty_and_all_floating_lists_stay_in_range() {
        assert_eq!(
            new_client_insert_index(NewClientPosition::Master, 0, 0, false, None),
            0
        );
        assert_eq!(
            new_client_insert_index(NewClientPosition::Master, 0, 0, true, None),
            0
        );
        // Every existing client floats: a tiled newcomer still leads the list.
        assert_eq!(
            new_client_insert_index(NewClientPosition::Master, 3, 0, false, None),
            0
        );
        assert_eq!(
            new_client_insert_index(NewClientPosition::Master, 3, 0, true, None),
            0
        );
    }
}
