// src/core/workspace.rs

use crate::core::models::{ClientKey, MonitorKey, ScrollingState, WMMonitor};
use crate::core::state::WMState;
use std::collections::{HashMap, HashSet};

pub struct WorkspaceManager;

impl WorkspaceManager {
    /// Exchange complete workspace state while output identities, geometry,
    /// monitor order and the selected physical monitor stay fixed. Physical
    /// shell clients (bars/docks/desktops) are supplied by the caller.
    /// Returns each side's outgoing clients for backend geometry migration.
    pub(crate) fn swap_monitor_contents(
        state: &mut WMState,
        scrolling: &mut HashMap<(MonitorKey, u32), ScrollingState>,
        left: MonitorKey,
        right: MonitorKey,
        physical_clients: &HashSet<ClientKey>,
    ) -> Option<[Vec<ClientKey>; 2]> {
        if left == right
            || !state.monitors.contains_key(left)
            || !state.monitors.contains_key(right)
        {
            return None;
        }
        let owned = |monitor| {
            let mut seen = HashSet::new();
            state
                .monitor_clients
                .get(monitor)
                .into_iter()
                .flatten()
                .copied()
                .chain(
                    state
                        .clients
                        .iter()
                        .filter_map(|(key, c)| (c.mon == Some(monitor)).then_some(key)),
                )
                .filter(|key| {
                    state
                        .clients
                        .get(*key)
                        .is_some_and(|c| c.mon == Some(monitor))
                        && seen.insert(*key)
                })
                .collect::<Vec<_>>()
        };
        let owners = [owned(left), owned(right)];
        let moved = owners.each_ref().map(|clients| {
            clients
                .iter()
                .copied()
                .filter(|key| !physical_clients.contains(key))
                .collect::<Vec<_>>()
        });
        let stacks = [left, right].map(|mon| {
            let mut seen = HashSet::new();
            state
                .monitor_stack
                .get(mon)
                .into_iter()
                .flatten()
                .copied()
                .chain(owners[usize::from(mon == right)].iter().copied())
                .filter(|key| {
                    state.clients.get(*key).is_some_and(|c| c.mon == Some(mon)) && seen.insert(*key)
                })
                .collect::<Vec<_>>()
        });
        let [a, b] = state.monitors.get_disjoint_mut([left, right])?;
        std::mem::swap(a, b);
        std::mem::swap(&mut a.num, &mut b.num);
        std::mem::swap(&mut a.geometry, &mut b.geometry);
        for (index, target) in [(0, right), (1, left)] {
            for &key in &moved[index] {
                if let Some(client) = state.clients.get_mut(key) {
                    client.mon = Some(target);
                }
            }
        }
        for (index, mon) in [(0, left), (1, right)] {
            let other = 1 - index;
            let list = moved[other]
                .iter()
                .copied()
                .chain(
                    owners[index]
                        .iter()
                        .copied()
                        .filter(|key| physical_clients.contains(key)),
                )
                .collect();
            state.monitor_clients.insert(mon, list);
            let stack = stacks[other]
                .iter()
                .copied()
                .filter(|key| !physical_clients.contains(key))
                .chain(
                    stacks[index]
                        .iter()
                        .copied()
                        .filter(|key| physical_clients.contains(key)),
                )
                .collect();
            state.monitor_stack.insert(mon, stack);
        }
        let keys: Vec<_> = scrolling
            .keys()
            .copied()
            .filter(|(mon, _)| *mon == left || *mon == right)
            .collect();
        let entries: Vec<_> = keys
            .into_iter()
            .filter_map(|key @ (mon, tags)| {
                scrolling
                    .remove(&key)
                    .map(|value| ((if mon == left { right } else { left }, tags), value))
            })
            .collect();
        scrolling.extend(entries);
        Some(moved)
    }

    /// 计算 client.state.tags 的变更
    pub fn calculate_new_tags(current_tags: u32, mask: u32, toggle: bool) -> u32 {
        if toggle { current_tags ^ mask } else { mask }
    }

    /// 检查 target_tag 是否已经是当前 Monitor 的激活 Tag
    pub fn is_same_tag(monitor: &WMMonitor, target_tag_mask: u32) -> bool {
        monitor.tag_set[monitor.sel_tags] == target_tag_mask
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::models::WMMonitor;

    // -----------------------------------------------------------------------
    // calculate_new_tags
    // -----------------------------------------------------------------------

    #[test]
    fn test_calculate_new_tags_set_no_toggle() {
        // toggle=false → just return mask
        assert_eq!(
            WorkspaceManager::calculate_new_tags(0b0101, 0b0010, false),
            0b0010
        );
    }

    #[test]
    fn test_calculate_new_tags_toggle_on() {
        // current has bit 0; toggle mask bit 1 → XOR sets bit 1
        assert_eq!(WorkspaceManager::calculate_new_tags(0b01, 0b10, true), 0b11);
    }

    #[test]
    fn test_calculate_new_tags_toggle_off() {
        // current has bit 1; toggling bit 1 → XOR clears it
        assert_eq!(WorkspaceManager::calculate_new_tags(0b11, 0b10, true), 0b01);
    }

    #[test]
    fn test_calculate_new_tags_toggle_no_op_on_zero() {
        // XOR with 0 leaves current unchanged
        assert_eq!(
            WorkspaceManager::calculate_new_tags(0b1010, 0, true),
            0b1010
        );
    }

    #[test]
    fn test_calculate_new_tags_set_replaces_all() {
        // no toggle: mask fully replaces current
        assert_eq!(
            WorkspaceManager::calculate_new_tags(0xFF, 0x01, false),
            0x01
        );
    }

    // -----------------------------------------------------------------------
    // is_same_tag
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_same_tag_matching() {
        let mut mon = WMMonitor::new();
        mon.tag_set[mon.sel_tags] = 0b0001;
        assert!(WorkspaceManager::is_same_tag(&mon, 0b0001));
    }

    #[test]
    fn test_is_same_tag_not_matching() {
        let mut mon = WMMonitor::new();
        mon.tag_set[mon.sel_tags] = 0b0001;
        assert!(!WorkspaceManager::is_same_tag(&mon, 0b0010));
    }

    #[test]
    fn test_is_same_tag_uses_sel_tags_index() {
        let mut mon = WMMonitor::new();
        mon.tag_set[0] = 0b0001;
        mon.tag_set[1] = 0b1000;
        mon.sel_tags = 1;
        assert!(WorkspaceManager::is_same_tag(&mon, 0b1000));
        assert!(!WorkspaceManager::is_same_tag(&mon, 0b0001));
    }

    #[test]
    fn test_is_same_tag_zero_mask() {
        let mut mon = WMMonitor::new();
        mon.tag_set[mon.sel_tags] = 0;
        assert!(WorkspaceManager::is_same_tag(&mon, 0));
    }
}
