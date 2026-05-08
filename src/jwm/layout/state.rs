// Layout state management functions

use crate::backend::api::Backend;
use crate::config::CONFIG;
use crate::core::layout::LayoutEnum;
use crate::core::models::{ClientKey, MonitorKey};
use crate::jwm::types::WMArgEnum;
use crate::jwm::Jwm;
use log::info;
use std::rc::Rc;

impl Jwm {
    pub(crate) fn incnmaster(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let WMArgEnum::Int(i) = *arg {
            let sel_mon_key = self.state.sel_mon.ok_or("No monitor selected")?;

            if let Some(monitor) = self.state.monitors.get_mut(sel_mon_key) {
                let new_n = (monitor.layout.n_master as i32 + i).max(0) as u32;
                monitor.layout.n_master = new_n;
                // 关键：调用新方法同步状态
                monitor.update_current_tag_layout_params();
                info!("[incnmaster] Updated n_master to {}", new_n);
            }
            self.arrange(backend, Some(sel_mon_key));
        }
        Ok(())
    }

    /// Check if the current monitor is in scrolling layout
    pub(crate) fn is_scrolling_layout(&self) -> bool {
        self.state
            .sel_mon
            .and_then(|mk| {
                self.state
                    .monitors
                    .get(mk)
                    .map(|m| *m.lt[m.sel_lt] == LayoutEnum::SCROLLING)
            })
            .unwrap_or(false)
    }

    /// Check if the current monitor is in vstack layout
    pub(crate) fn is_vstack_layout(&self) -> bool {
        self.state
            .sel_mon
            .and_then(|mk| {
                self.state
                    .monitors
                    .get(mk)
                    .map(|m| *m.lt[m.sel_lt] == LayoutEnum::VSTACK)
            })
            .unwrap_or(false)
    }

    /// Move the currently focused client to the front of the monitor's client
    /// list so it becomes master in tiling layouts.
    fn promote_focused_to_master(&mut self, mon_key: MonitorKey) {
        let sel = match self.state.monitors.get(mon_key).and_then(|m| m.sel) {
            Some(k) => k,
            None => return,
        };
        // Already master?
        let first_tiled = self.nexttiled(mon_key, None);
        if first_tiled == Some(sel) {
            return;
        }
        self.detach(sel);
        self.attach_front(sel);
    }

    pub(crate) fn setmfact(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let WMArgEnum::Float(f) = arg {
            let sel_mon_key = self.state.sel_mon.ok_or("No monitor selected")?;
            if let Some(monitor) = self.state.monitors.get_mut(sel_mon_key) {
                let new_mfact = if f < &1.0 {
                    f + monitor.layout.m_fact
                } else {
                    f - 1.0
                };
                if new_mfact >= 0.05 && new_mfact <= 0.95 {
                    monitor.layout.m_fact = new_mfact;
                    // 关键：调用新方法同步状态
                    monitor.update_current_tag_layout_params();
                }
            }
            self.arrange(backend, Some(sel_mon_key));
        }
        Ok(())
    }

    /// 退出当前 monitor 上所有全屏窗口的全屏状态
    fn exit_fullscreen_on_monitor(&mut self, backend: &mut dyn Backend, mon_key: MonitorKey) {
        let fs_clients: Vec<ClientKey> = self
            .state
            .monitor_clients
            .get(mon_key)
            .map(|keys| {
                keys.iter()
                    .copied()
                    .filter(|&ck| {
                        self.state
                            .clients
                            .get(ck)
                            .map(|c| c.state.is_fullscreen)
                            .unwrap_or(false)
                    })
                    .collect()
            })
            .unwrap_or_default();

        for ck in fs_clients {
            let _ = self.setfullscreen(backend, ck, false);
        }
    }

    pub(crate) fn setlayout(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[setlayout]");
        let sel_mon_key = self.state.sel_mon.ok_or("No selected monitor")?;

        let old_layout = self
            .state
            .monitors
            .get(sel_mon_key)
            .map(|m| m.lt[m.sel_lt].clone())
            .ok_or("No monitor")?;

        // Leaving vstack: promote the focused client to master so it stays
        // master in the new layout.
        if *old_layout == LayoutEnum::VSTACK {
            self.promote_focused_to_master(sel_mon_key);
        }

        self.exit_fullscreen_on_monitor(backend, sel_mon_key);
        self.update_layout_selection(sel_mon_key, arg)?;

        let new_layout = self
            .state
            .monitors
            .get(sel_mon_key)
            .map(|m| m.lt[m.sel_lt].clone())
            .ok_or("No monitor")?;

        self.handle_fullscreen_layout_transition(backend, sel_mon_key, &old_layout, &new_layout)?;

        let (should_arrange, mon_num) = self.finalize_layout_update(sel_mon_key);

        if should_arrange {
            self.arrange(backend, Some(sel_mon_key));
        } else {
            self.mark_bar_update_needed_if_visible(mon_num);
        }

        self.broadcast_ipc_event(
            "layout/set",
            serde_json::json!({
                "layout": format!("{:?}", *new_layout),
            }),
        );

        Ok(())
    }

    pub(crate) fn cyclelayout(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[cyclelayout]");
        let sel_mon_key = self.state.sel_mon.ok_or("No selected monitor")?;

        let old_layout = self
            .state
            .monitors
            .get(sel_mon_key)
            .map(|m| m.lt[m.sel_lt].clone())
            .ok_or("No monitor")?;

        if *old_layout == LayoutEnum::VSTACK {
            self.promote_focused_to_master(sel_mon_key);
        }

        self.exit_fullscreen_on_monitor(backend, sel_mon_key);

        let dir = match arg {
            WMArgEnum::Int(i) => *i,
            _ => 1,
        };

        let cur_tag = self
            .state
            .monitors
            .get(sel_mon_key)
            .and_then(|m| m.pertag.as_ref())
            .map(|p| p.cur_tag)
            .ok_or("No pertag")?;

        let current = self
            .state
            .monitors
            .get(sel_mon_key)
            .map(|m| m.lt[m.sel_lt].clone())
            .ok_or("No monitor")?;

        let next = if dir >= 0 {
            current.cycle_next()
        } else {
            current.cycle_prev()
        };

        let next_rc = Rc::new(next.clone());
        self.set_new_layout(sel_mon_key, &next_rc, cur_tag);

        self.handle_fullscreen_layout_transition(backend, sel_mon_key, &old_layout, &next_rc)?;

        let (should_arrange, mon_num) = self.finalize_layout_update(sel_mon_key);
        if should_arrange {
            self.arrange(backend, Some(sel_mon_key));
        } else {
            self.mark_bar_update_needed_if_visible(mon_num);
        }

        self.broadcast_ipc_event(
            "layout/set",
            serde_json::json!({
                "layout": format!("{:?}", next),
            }),
        );

        Ok(())
    }

    /// Handle bar visibility and border_w changes when transitioning to/from fullscreen layout
    fn handle_fullscreen_layout_transition(
        &mut self,
        _backend: &mut dyn Backend,
        mon_key: MonitorKey,
        old_layout: &LayoutEnum,
        new_layout: &LayoutEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let was_fullscreen = old_layout.is_fullscreen_layout();
        let is_fullscreen = new_layout.is_fullscreen_layout();

        if was_fullscreen == is_fullscreen {
            return Ok(());
        }

        if is_fullscreen {
            // Entering fullscreen layout: hide bar
            if let Some(monitor) = self.state.monitors.get_mut(mon_key) {
                if let Some(ref mut pertag) = monitor.pertag {
                    let cur_tag = pertag.cur_tag;
                    if let Some(show_bar) = pertag.show_bars.get_mut(cur_tag) {
                        *show_bar = false;
                    }
                }
            }
        } else {
            // Leaving fullscreen layout: show bar, restore border_w
            if let Some(monitor) = self.state.monitors.get_mut(mon_key) {
                if let Some(ref mut pertag) = monitor.pertag {
                    let cur_tag = pertag.cur_tag;
                    if let Some(show_bar) = pertag.show_bars.get_mut(cur_tag) {
                        *show_bar = true;
                    }
                }
            }

            // Restore border_w for all clients on this monitor
            let border_w = CONFIG.load().border_px() as i32;
            let client_keys: Vec<ClientKey> = self
                .state
                .monitor_clients
                .get(mon_key)
                .map(|keys| keys.iter().copied().collect())
                .unwrap_or_default();

            for ck in client_keys {
                if let Some(client) = self.state.clients.get_mut(ck) {
                    if !client.state.is_floating {
                        client.geometry.border_w = border_w;
                    }
                }
            }
        }

        Ok(())
    }

    fn update_layout_selection(
        &mut self,
        sel_mon_key: MonitorKey,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        match *arg {
            WMArgEnum::Layout(ref lt) => self.handle_specific_layout(sel_mon_key, lt),
            _ => self.toggle_layout_selection(sel_mon_key),
        }
    }

    fn handle_specific_layout(
        &mut self,
        sel_mon_key: MonitorKey,
        layout: &Rc<LayoutEnum>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let monitor = self
            .state
            .monitors
            .get(sel_mon_key)
            .ok_or("Monitor not found")?;

        let current_layout = monitor.lt[monitor.sel_lt].clone();
        let cur_tag = monitor
            .pertag
            .as_ref()
            .ok_or("No pertag information")?
            .cur_tag;

        if **layout == *current_layout {
            self.toggle_layout_selection_impl(sel_mon_key, cur_tag);
        } else {
            self.set_new_layout(sel_mon_key, layout, cur_tag);
        }

        Ok(())
    }

    fn toggle_layout_selection(
        &mut self,
        sel_mon_key: MonitorKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cur_tag = self
            .state
            .monitors
            .get(sel_mon_key)
            .and_then(|m| m.pertag.as_ref())
            .map(|p| p.cur_tag)
            .ok_or("No pertag information available")?;

        self.toggle_layout_selection_impl(sel_mon_key, cur_tag);
        Ok(())
    }

    fn toggle_layout_selection_impl(&mut self, sel_mon_key: MonitorKey, cur_tag: usize) {
        if let Some(monitor) = self.state.monitors.get_mut(sel_mon_key) {
            if let Some(ref mut pertag) = monitor.pertag {
                pertag.sel_lts[cur_tag] ^= 1;
                monitor.sel_lt = pertag.sel_lts[cur_tag];
            }
        }
    }

    fn set_new_layout(&mut self, sel_mon_key: MonitorKey, layout: &Rc<LayoutEnum>, cur_tag: usize) {
        if let Some(monitor) = self.state.monitors.get_mut(sel_mon_key) {
            let sel_lt = monitor.sel_lt;
            if let Some(ref mut pertag) = monitor.pertag {
                pertag.lt_idxs[cur_tag][sel_lt] = Some(layout.clone());
                monitor.lt[sel_lt] = layout.clone();
            }
        }
    }

    fn finalize_layout_update(&mut self, sel_mon_key: MonitorKey) -> (bool, Option<i32>) {
        if let Some(monitor) = self.state.monitors.get_mut(sel_mon_key) {
            monitor.lt_symbol = monitor.lt[monitor.sel_lt].symbol().to_string();

            let has_selection = monitor.sel.is_some();
            let mon_num = monitor.num;

            (has_selection, Some(mon_num))
        } else {
            (false, None)
        }
    }
}
