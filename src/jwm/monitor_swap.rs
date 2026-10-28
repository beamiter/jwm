//! Exchange workspaces without switching the physical output being viewed.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use crate::backend::api::Backend;
use crate::config::CONFIG;
use crate::core::models::MonitorKey;
use crate::core::workspace::WorkspaceManager;
use crate::jwm::statusbar::StatusBarBuilder;
use crate::jwm::{Jwm, WMArgEnum};

impl Jwm {
    /// Int(-1/+1): previous/next unlocked monitor. UInt: explicit monitor id.
    /// The selected monitor and pointer do not follow the outgoing windows.
    pub fn swapmon(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.features.system_ui.is_session_lock() {
            return Err("cannot exchange workspaces while the session is locked".into());
        }
        let Some(source) = self.state.sel_mon else {
            return Ok(());
        };
        let target = match *arg {
            WMArgEnum::Int(0) => return Ok(()),
            WMArgEnum::Int(direction @ (-1 | 1)) => self.dirtomon(&direction),
            WMArgEnum::UInt(id) => Some(
                self.get_monitor_by_id(i32::try_from(id)?)
                    .ok_or("target monitor does not exist")?,
            ),
            _ => return Err("swapmon expects direction -1/+1 or an unsigned monitor id".into()),
        };
        let Some(target) = target else {
            return Ok(());
        };
        if source == target {
            return Ok(());
        }
        self.swap_monitor_contents(backend, source, target)
    }

    fn swap_monitor_contents(
        &mut self,
        backend: &mut dyn Backend,
        source: MonitorKey,
        target: MonitorKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.monitor_key_is_locked(source) || self.monitor_key_is_locked(target) {
            return Err("cannot exchange workspaces of a locked monitor".into());
        }
        if self.drag_ctl.is_some()
            || self.tab_drag.is_some()
            || self.wallpaper_drag.is_some()
            || self.features.screenshot.active
            || self.features.overview.active
            || self.features.peek_active
            || self.features.expose_active
        {
            return Err(
                "finish the active desktop interaction before exchanging workspaces".into(),
            );
        }
        // A preview belongs to its original tag/monitor; restore it before
        // capturing the workspace, rather than letting a later cancel undo it.
        if self.features.system_ui.is_layout_picker() {
            self.cancel_layout_picker(backend);
        } else if self.features.system_ui.is_active() {
            self.close_system_ui(backend);
        }
        let source_work = self
            .monitor_migration_areas(source)
            .ok_or("source monitor disappeared")?
            .1;
        let target_work = self
            .monitor_migration_areas(target)
            .ok_or("target monitor disappeared")?
            .1;
        let nums = [
            self.state.monitors[source].num,
            self.state.monitors[target].num,
        ];
        let status_bar = CONFIG.load().status_bar_name().to_owned();
        let physical: HashSet<_> = self
            .state
            .clients
            .iter()
            .filter_map(|(key, client)| {
                (client.state.is_dock
                    || client.state.is_desktop
                    || client.is_status_bar(&status_bar)
                    || self
                        .secondary_bars
                        .values()
                        .any(|bar| bar.client_key == Some(key)))
                .then_some(key)
            })
            .collect();
        if let Some(selected) = self.get_selected_client_key() {
            let _ = self.unfocus_client(backend, selected, false);
        }
        // Withdraw old Dock targets while their old ownership is still known.
        for &num in &nums {
            self.clear_minimized_preview_for(backend, num, None);
        }
        for (key, client) in &self.state.clients {
            if !physical.contains(&key)
                && (client.mon == Some(source) || client.mon == Some(target))
                && client.state.is_hidden
            {
                backend.compositor_set_window_dock_geometry(client.win, None);
            }
        }
        let moved = WorkspaceManager::swap_monitor_contents(
            &mut self.state,
            &mut self.scrolling_states,
            source,
            target,
            &physical,
        )
        .ok_or("monitor exchange could not be planned")?;
        self.last_stacking.remove(source);
        self.last_stacking.remove(target);
        let suppress = self.suppress_layout_animation;
        self.suppress_layout_animation = true;
        for (index, destination, old_work) in [(0, target, source_work), (1, source, target_work)] {
            let (output, work) = self
                .monitor_migration_areas(destination)
                .expect("validated monitor");
            for &key in &moved[index] {
                self.animations.remove(key);
                if !self.migrate_hidden_client_restore(backend, key, Some(old_work), output, work) {
                    self.migrate_swapped_client(backend, key, Some(old_work), output, work);
                }
                if let Some(client) = self.state.clients.get(key)
                    && client.state.is_hidden
                    && StatusBarBuilder::is_minimized_dock_eligible(client)
                    && let Some(shelf) = self
                        .minimized_dock_shelves
                        .get(&self.state.monitors[destination].num)
                {
                    backend.compositor_set_window_dock_geometry(client.win, Some(*shelf));
                }
            }
        }
        self.arrange(backend, Some(source));
        self.arrange(backend, Some(target));
        self.suppress_layout_animation = suppress;
        self.suppress_mouse_focus_until = Some(Instant::now() + Duration::from_millis(300));
        let incoming = self.state.monitors[source].sel.filter(|key| {
            self.state
                .clients
                .get(*key)
                .is_some_and(|client| client.mon == Some(source))
        });
        if let Err(error) = self.focus(backend, incoming) {
            log::warn!("monitor exchange focus: {error}");
        }
        let _ = self.restack(backend, Some(source));
        let _ = self.update_ewmh_desktop(backend);
        self.refresh_compositor_monitors(backend);
        self.ensure_native_root_wallpaper(backend);
        self.refresh_wallpaper_theme();
        self.mark_layout_dirty();
        for &key in moved.iter().flatten() {
            if let Err(error) = self.setclienttagprop(backend, key) {
                log::warn!("monitor exchange client metadata: {error}");
            }
            if let Err(error) = self.persist_minimized_restore_state(backend, key) {
                log::warn!("monitor exchange restore metadata: {error}");
            }
            self.broadcast_window_state_ipc(backend, key);
        }
        for mon in [source, target] {
            self.broadcast_monitor_bar_ipc(backend, mon);
            self.pending_bar_updates
                .insert(self.state.monitors[mon].num);
        }
        self.flush_pending_bar_updates();
        self.broadcast_ipc_event(
            "tag/view",
            serde_json::json!({
                "tag": self.state.monitors[source].get_active_tags(), "monitor": nums[0],
            }),
        );
        self.broadcast_ipc_event(
            "monitor/swap",
            serde_json::json!({"source": nums[0], "target": nums[1]}),
        );
        backend.compositor_force_full_redraw();
        log::info!(
            "exchanged all workspaces of monitors {} and {}",
            nums[0],
            nums[1]
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::api::MaximizeAxes;
    use crate::backend::common_define::WindowId;
    use crate::core::layout::LayoutEnum;
    use crate::core::master_resize::{ResizeContext, ResizeIntent};
    use crate::core::models::{ClientKey, ScrollingState, WMClient};
    use crate::core::types::Rect;
    use crate::jwm::monitor::test_support::{DisplaySpyBackend, output};
    use std::rc::Rc;

    fn desktop() -> (Jwm, DisplaySpyBackend, [MonitorKey; 3]) {
        let mut backend = DisplaySpyBackend::new(vec![
            output(1, 0, 0, 1920, 1080),
            output(2, -1600, 100, 1600, 900),
            output(3, 1920, 0, 1280, 1024),
        ]);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitors = [
            jwm.state.monitor_order[0],
            jwm.state.monitor_order[1],
            jwm.state.monitor_order[2],
        ];
        jwm.state.sel_mon = Some(monitors[0]);
        (jwm, backend, monitors)
    }

    fn client(jwm: &mut Jwm, mon: MonitorKey, id: u64, tags: u32, rect: Rect) -> ClientKey {
        let mut c = WMClient::new(WindowId::from_raw(id));
        c.mon = Some(mon);
        c.state.tags = tags;
        c.geometry.x = rect.x;
        c.geometry.y = rect.y;
        c.geometry.w = rect.w;
        c.geometry.h = rect.h;
        c.geometry.floating_x = rect.x;
        c.geometry.floating_y = rect.y;
        c.geometry.floating_w = rect.w;
        c.geometry.floating_h = rect.h;
        let key = jwm.insert_client(c);
        jwm.attach_to_monitor(key, mon);
        key
    }

    fn rect(jwm: &Jwm, key: ClientKey) -> Rect {
        let g = &jwm.state.clients[key].geometry;
        Rect::new(g.x, g.y, g.w, g.h)
    }

    fn set_float_layout(jwm: &mut Jwm, mon: MonitorKey) {
        let m = &mut jwm.state.monitors[mon];
        m.lt = Rc::new(LayoutEnum::FLOAT);
        if let Some(p) = &mut m.pertag {
            p.lts[p.cur_tag] = m.lt.clone();
        }
    }

    #[test]
    fn monitor_swap_moves_complete_profiles_orders_and_all_tag_scrolling_states() {
        let (mut jwm, _, [left, right, third]) = desktop();
        let a = client(&mut jwm, left, 101, 1, Rect::new(100, 100, 400, 300));
        let a_hidden_tag = client(&mut jwm, left, 102, 16, Rect::new(200, 200, 400, 300));
        let b = client(&mut jwm, right, 201, 6, Rect::new(-1400, 200, 400, 300));
        let dock = client(&mut jwm, left, 301, !0, Rect::new(0, 0, 1920, 30));
        jwm.state.clients[dock].state.is_dock = true;
        let physical = HashSet::from([dock]);
        jwm.state.monitors[left].sel = Some(a);
        jwm.state.monitors[left].pertag.as_mut().unwrap().sel[5] = Some(a_hidden_tag);
        jwm.state.monitors[right].tag_set = [4, 2];
        jwm.state.monitors[right].sel_tags = 1;
        jwm.state.monitors[right].sel = Some(b);
        jwm.state.monitors[right].layout.m_fact = 0.7;
        jwm.state.monitors[right].layout.n_master = 3;
        jwm.state.monitors[right].layout.gap = 21;
        let p = jwm.state.monitors[right].pertag.as_mut().unwrap();
        p.cur_tag = 2;
        p.prev_tag = 3;
        p.sel[2] = Some(b);
        p.lts[7] = Rc::new(LayoutEnum::GRID);
        p.prev_lts[7] = Rc::new(LayoutEnum::MONOCLE);
        p.show_bars[7] = false;
        p.gaps[7] = 17;
        p.m_facts[7] = 0.66;
        p.n_masters[7] = 4;
        jwm.state.monitors[left].layout_resizes.insert(
            (1, LayoutEnum::TILE.0),
            ResizeContext {
                topology: vec![a],
                intent: Some(ResizeIntent {
                    client: a,
                    width: 0.7,
                    height: 0.4,
                }),
                ..Default::default()
            },
        );
        for (mon, tags, key, viewport) in [
            (left, 1, a, 175.0),
            (left, 16, a_hidden_tag, 280.0),
            (right, 2, b, 95.0),
        ] {
            let mut s = ScrollingState::new();
            s.columns = vec![vec![key]];
            s.column_width_factors = vec![0.65];
            s.focused_clients = vec![Some(key)];
            s.focused_column = Some(0);
            s.attach_new_windows_to_focused_column = true;
            s.viewport_x = viewport;
            jwm.scrolling_states.insert((mon, tags), s);
        }
        let before = [
            jwm.state.monitors[left].clone(),
            jwm.state.monitors[right].clone(),
            jwm.state.monitors[third].clone(),
        ];
        let lists = [
            jwm.state.monitor_clients[left].clone(),
            jwm.state.monitor_clients[right].clone(),
        ];
        let stacks = [
            jwm.state.monitor_stack[left].clone(),
            jwm.state.monitor_stack[right].clone(),
        ];
        let outputs = jwm.state.output_map.clone();
        let global = jwm.state.client_order.clone();
        let order = jwm.state.monitor_order.clone();
        let tag_values: Vec<_> = jwm
            .state
            .clients
            .iter()
            .map(|(k, c)| (k, c.state.tags))
            .collect();
        let moved = WorkspaceManager::swap_monitor_contents(
            &mut jwm.state,
            &mut jwm.scrolling_states,
            left,
            right,
            &physical,
        )
        .unwrap();
        assert_eq!(moved, [vec![a, a_hidden_tag], vec![b]]);
        for (dest, old, fixed) in [
            (left, &before[1], &before[0]),
            (right, &before[0], &before[1]),
        ] {
            let mut expected = old.clone();
            expected.num = fixed.num;
            expected.geometry = fixed.geometry.clone();
            assert_eq!(jwm.state.monitors[dest], expected);
        }
        assert_eq!(jwm.state.monitors[third], before[2]);
        assert_eq!(jwm.state.sel_mon, Some(left));
        assert_eq!(jwm.state.monitor_order, order);
        for (mon, id) in &outputs {
            assert_eq!(jwm.state.output_map.get(mon), Some(id));
        }
        assert_eq!(jwm.state.client_order, global);
        assert_eq!(jwm.state.clients[dock].mon, Some(left));
        for (key, tags) in tag_values {
            assert_eq!(jwm.state.clients[key].state.tags, tags);
        }
        assert_eq!(jwm.scrolling_states[&(right, 16)].viewport_x, 280.0);
        assert_eq!(jwm.scrolling_states[&(left, 2)].columns, vec![vec![b]]);
        assert_eq!(
            jwm.scrolling_states[&(right, 1)].focused_clients,
            vec![Some(a)]
        );
        WorkspaceManager::swap_monitor_contents(
            &mut jwm.state,
            &mut jwm.scrolling_states,
            left,
            right,
            &physical,
        )
        .unwrap();
        assert_eq!(jwm.state.monitors[left], before[0]);
        assert_eq!(jwm.state.monitors[right], before[1]);
        assert_eq!(jwm.state.monitor_clients[left], lists[0]);
        assert_eq!(jwm.state.monitor_clients[right], lists[1]);
        // Shell surfaces retain physical ownership; only application MRU
        // order travels with the workspace (the dock's slot is not a tag).
        for (mon, original) in [(left, &stacks[0]), (right, &stacks[1])] {
            let applications = |keys: &[ClientKey]| {
                keys.iter()
                    .copied()
                    .filter(|key| !physical.contains(key))
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                applications(&jwm.state.monitor_stack[mon]),
                applications(original)
            );
        }
        assert!(jwm.state.monitor_stack[left].contains(&dock));
        assert_eq!(jwm.scrolling_states[&(left, 16)].viewport_x, 280.0);
        assert!(jwm.scrolling_states[&(left, 1)].attach_new_windows_to_focused_column);
    }

    #[test]
    fn monitor_swap_includes_unlisted_owners_and_keeps_unassigned_scratchpads() {
        let (mut jwm, _, [left, right, _]) = desktop();
        let mut c = WMClient::new(WindowId::from_raw(401));
        c.mon = Some(left);
        c.state.tags = 0;
        c.state.is_swallowed = true;
        let key = jwm.state.clients.insert(c);
        let parked = jwm
            .state
            .clients
            .insert(WMClient::new(WindowId::from_raw(402)));
        let moved = WorkspaceManager::swap_monitor_contents(
            &mut jwm.state,
            &mut jwm.scrolling_states,
            left,
            right,
            &HashSet::new(),
        )
        .unwrap();
        assert!(moved[0].contains(&key));
        assert_eq!(jwm.state.clients[key].mon, Some(right));
        assert_eq!(jwm.state.clients[key].state.tags, 0);
        assert!(jwm.state.clients[key].state.is_swallowed);
        assert!(jwm.state.monitor_clients[right].contains(&key));
        assert_eq!(jwm.state.clients[parked].mon, None);
    }

    #[test]
    fn monitor_swap_runtime_keeps_focus_and_pointer_on_the_original_output() {
        let (mut jwm, mut backend, [left, right, third]) = desktop();
        let a = client(&mut jwm, left, 501, 1, Rect::new(100, 100, 400, 300));
        let hidden_tag = client(&mut jwm, left, 502, 8, Rect::new(200, 200, 400, 300));
        let b = client(&mut jwm, right, 503, 2, Rect::new(-1400, 200, 400, 300));
        let third_client = client(&mut jwm, third, 504, 1, Rect::new(2000, 100, 400, 300));
        jwm.state.monitors[left].set_selected_client_for_current_tag(Some(a));
        jwm.state.monitors[right].view_tag(2, false);
        jwm.state.monitors[right].set_selected_client_for_current_tag(Some(b));
        jwm.last_mouse_root = (250.0, 300.0);
        jwm.swapmon(&mut backend, &WMArgEnum::Int(1)).unwrap();
        assert_eq!(jwm.state.sel_mon, Some(left));
        assert_eq!(jwm.get_selected_client_key(), Some(b));
        assert_eq!(jwm.last_mouse_root, (250.0, 300.0));
        assert_eq!(jwm.state.clients[a].mon, Some(right));
        assert_eq!(jwm.state.clients[hidden_tag].mon, Some(right));
        assert_eq!(jwm.state.clients[hidden_tag].state.tags, 8);
        assert_eq!(jwm.state.clients[third_client].mon, Some(third));
        assert_eq!(jwm.state.monitors[left].get_active_tags(), 2);
        assert_eq!(jwm.state.monitors[right].get_active_tags(), 1);
        jwm.swapmon(&mut backend, &WMArgEnum::Int(1)).unwrap();
        assert_eq!(jwm.get_selected_client_key(), Some(a));
        assert_eq!(jwm.state.clients[hidden_tag].mon, Some(left));
        assert_eq!(jwm.state.monitors[left].get_active_tags(), 1);
    }

    #[test]
    fn monitor_swap_translates_unflagged_float_layout_clients_and_saved_positions() {
        let (mut jwm, mut backend, [left, right, _]) = desktop();
        set_float_layout(&mut jwm, left);
        set_float_layout(&mut jwm, right);
        let a = client(&mut jwm, left, 601, 1, Rect::new(100, 160, 500, 300));
        let old = jwm.monitor_migration_areas(left).unwrap().1;
        let target = jwm.monitor_migration_areas(right).unwrap().1;
        let expected = crate::jwm::monitor::translate_and_clamp_restore_rect(
            rect(&jwm, a),
            Some(old),
            target,
            0,
        );
        jwm.swapmon(
            &mut backend,
            &WMArgEnum::UInt(jwm.state.monitors[right].num as u32),
        )
        .unwrap();
        assert_eq!(rect(&jwm, a), expected);
        assert!(
            !jwm.state.clients[a].state.is_floating,
            "the exchange does not change tiling policy"
        );
        assert_eq!(jwm.state.clients[a].geometry.floating_x, expected.x);
    }

    #[test]
    fn monitor_swap_migrates_minimized_fullscreen_maximize_and_pip_restore_slots() {
        let (mut jwm, mut backend, [left, right, _]) = desktop();
        let work = jwm.monitor_migration_areas(left).unwrap().1;
        let target_work = jwm.monitor_migration_areas(right).unwrap().1;
        let target_output = jwm.monitor_migration_areas(right).unwrap().0;
        let restore = Rect::new(100, 160, 600, 350);
        let expected = crate::jwm::monitor::translate_and_clamp_restore_rect(
            restore,
            Some(work),
            target_work,
            0,
        );
        let full = client(&mut jwm, left, 701, 1, Rect::new(0, 0, 1920, 1080));
        let minimized = client(&mut jwm, left, 702, 4, Rect::new(-10000, 160, 600, 350));
        let pip = client(&mut jwm, left, 703, 1, restore);
        let max = client(&mut jwm, left, 704, 1, restore);
        {
            let c = &mut jwm.state.clients[full];
            c.state.is_fullscreen = true;
            c.state.is_floating = true;
            c.geometry.old_x = restore.x;
            c.geometry.old_y = restore.y;
            c.geometry.old_w = restore.w;
            c.geometry.old_h = restore.h;
        }
        {
            let c = &mut jwm.state.clients[minimized];
            c.state.is_hidden = true;
            c.state.is_floating = true;
            c.state.minimized_order = 77;
            c.geometry.hidden_x = Some(-10000);
            c.geometry.hidden_restore_rect = Some(restore);
        }
        {
            let c = &mut jwm.state.clients[pip];
            c.state.is_pip = true;
            c.state.is_floating = true;
            c.state.is_sticky = true;
        }
        {
            let c = &mut jwm.state.clients[max];
            c.state.is_floating = true;
            c.state.set_maximized_axes(MaximizeAxes::BOTH);
            c.geometry.maximize_restore_rect = Some(restore);
        }
        jwm.swapmon(&mut backend, &WMArgEnum::Int(1)).unwrap();
        assert_eq!(rect(&jwm, full), target_output);
        let c = &jwm.state.clients[full];
        assert_eq!(
            Rect::new(
                c.geometry.old_x,
                c.geometry.old_y,
                c.geometry.old_w,
                c.geometry.old_h
            ),
            expected
        );
        let c = &jwm.state.clients[minimized];
        assert_eq!(c.geometry.hidden_restore_rect, Some(expected));
        assert_eq!(c.state.minimized_order, 77);
        assert!(c.state.is_hidden);
        assert_eq!(c.state.tags, 4);
        assert!(c.geometry.x + c.geometry.w < -1600);
        let c = &jwm.state.clients[pip];
        assert!(c.state.is_pip && c.state.is_sticky);
        assert_eq!(
            Rect::new(
                c.geometry.floating_x,
                c.geometry.floating_y,
                c.geometry.floating_w,
                c.geometry.floating_h
            ),
            expected
        );
        assert_eq!(
            jwm.state.clients[max].geometry.maximize_restore_rect,
            Some(expected)
        );
        assert_eq!(
            rect(&jwm, max),
            crate::core::maximize::maximize_target(expected, target_work, MaximizeAxes::BOTH, 0)
        );
    }

    #[test]
    fn monitor_swap_refuses_locked_outputs_and_active_interactions_without_changes() {
        let (mut jwm, mut backend, [left, right, _]) = desktop();
        let before = [
            jwm.state.monitors[left].clone(),
            jwm.state.monitors[right].clone(),
        ];
        let num = jwm.state.monitors[right].num;
        jwm.features.monitor_lock.lock(num, (-1600, 100, 1600, 900));
        assert!(
            jwm.swapmon(&mut backend, &WMArgEnum::UInt(num as u32))
                .is_err()
        );
        assert_eq!(jwm.state.monitors[left], before[0]);
        assert_eq!(jwm.state.monitors[right], before[1]);
        jwm.features.monitor_lock.unlock(num);
        jwm.features.screenshot.active = true;
        assert!(jwm.swapmon(&mut backend, &WMArgEnum::Int(1)).is_err());
        assert_eq!(jwm.state.monitors[left], before[0]);
        jwm.features.screenshot.active = false;
        jwm.swapmon(
            &mut backend,
            &WMArgEnum::UInt(jwm.state.monitors[left].num as u32),
        )
        .unwrap();
        jwm.swapmon(&mut backend, &WMArgEnum::Int(0)).unwrap();
        assert!(jwm.swapmon(&mut backend, &WMArgEnum::UInt(999)).is_err());
        assert_eq!(jwm.state.monitors[left], before[0]);
    }

    #[test]
    fn monitor_swap_single_output_and_direction_skip_locked_monitors() {
        let (mut jwm, mut backend, [left, right, third]) = desktop();
        let right_id = jwm.state.monitors[right].num;
        jwm.features
            .monitor_lock
            .lock(right_id, (-1600, 100, 1600, 900));
        let c = client(&mut jwm, left, 801, 1, Rect::new(100, 100, 300, 200));
        jwm.swapmon(&mut backend, &WMArgEnum::Int(1)).unwrap();
        assert_eq!(jwm.state.clients[c].mon, Some(third));
        assert_eq!(jwm.state.sel_mon, Some(left));
        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let mut single = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let mon = single.state.monitor_order[0];
        let before = single.state.monitors[mon].clone();
        single.swapmon(&mut backend, &WMArgEnum::Int(1)).unwrap();
        single.swapmon(&mut backend, &WMArgEnum::Int(-1)).unwrap();
        assert_eq!(single.state.monitors[mon], before);
    }

    #[test]
    fn monitor_swap_stale_selection_cannot_take_focus_to_a_third_monitor() {
        let (mut jwm, mut backend, [left, right, third]) = desktop();
        let incoming = client(&mut jwm, right, 901, 1, Rect::new(-1400, 200, 400, 300));
        let foreign = client(&mut jwm, third, 902, 1, Rect::new(2100, 200, 400, 300));
        jwm.state.monitors[right].sel = Some(foreign);
        jwm.swapmon(&mut backend, &WMArgEnum::Int(1)).unwrap();
        assert_eq!(jwm.state.sel_mon, Some(left));
        assert_eq!(jwm.get_selected_client_key(), Some(incoming));
        assert_eq!(jwm.state.clients[foreign].mon, Some(third));
    }

    #[test]
    fn monitor_swap_keeps_dock_and_desktop_clients_on_their_physical_outputs() {
        let (mut jwm, mut backend, [left, right, _]) = desktop();
        let app = client(&mut jwm, left, 911, 1, Rect::new(100, 200, 400, 300));
        let dock = client(&mut jwm, left, 912, !0, Rect::new(0, 0, 1920, 30));
        let desktop = client(&mut jwm, right, 913, !0, Rect::new(-1600, 100, 1600, 900));
        jwm.state.clients[dock].state.is_dock = true;
        jwm.state.clients[desktop].state.is_desktop = true;
        // Managed shell surfaces float by type; arrange must leave their
        // physical rectangles alone when the workspace layout changes.
        jwm.state.clients[dock].state.is_floating = true;
        jwm.state.clients[desktop].state.is_floating = true;
        let before = [rect(&jwm, dock), rect(&jwm, desktop)];
        jwm.swapmon(&mut backend, &WMArgEnum::Int(1)).unwrap();
        assert_eq!(jwm.state.clients[app].mon, Some(right));
        assert_eq!(jwm.state.clients[dock].mon, Some(left));
        assert_eq!(jwm.state.clients[desktop].mon, Some(right));
        assert_eq!(rect(&jwm, dock), before[0]);
        assert_eq!(rect(&jwm, desktop), before[1]);
    }

    #[test]
    fn monitor_swap_previous_direction_and_explicit_ids_are_not_monitor_indices() {
        let (mut jwm, mut backend, [left, right, third]) = desktop();
        jwm.state.monitors[right].num = 7;
        let a = client(&mut jwm, left, 921, 1, Rect::new(100, 200, 400, 300));
        let b = client(&mut jwm, right, 922, 1, Rect::new(-1400, 200, 400, 300));
        jwm.swapmon(&mut backend, &WMArgEnum::Int(-1)).unwrap();
        assert_eq!(jwm.state.clients[a].mon, Some(third));
        assert_eq!(jwm.state.sel_mon, Some(left));
        jwm.swapmon(&mut backend, &WMArgEnum::UInt(7)).unwrap();
        assert_eq!(jwm.state.clients[b].mon, Some(left));
        assert_eq!(jwm.state.clients[a].mon, Some(third));
        assert_eq!(jwm.state.monitors[right].num, 7);
    }

    #[test]
    fn monitor_swap_rebases_floats_already_overlapping_the_destination() {
        let (mut jwm, mut backend, [left, right, _]) = desktop();
        set_float_layout(&mut jwm, left);
        set_float_layout(&mut jwm, right);
        let c = client(&mut jwm, left, 931, 1, Rect::new(-1400, 200, 300, 200));
        jwm.state.clients[c].state.is_floating = true;
        let source = jwm.monitor_migration_areas(left).unwrap().1;
        let target = jwm.monitor_migration_areas(right).unwrap().1;
        let expected = crate::jwm::monitor::translate_and_clamp_restore_rect(
            rect(&jwm, c),
            Some(source),
            target,
            0,
        );
        jwm.swapmon(&mut backend, &WMArgEnum::Int(1)).unwrap();
        assert_eq!(rect(&jwm, c), expected);
    }
}
