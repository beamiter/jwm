// Layout state management functions

use crate::backend::api::Backend;
use crate::config::CONFIG;
use crate::core::layout::LayoutEnum;
use crate::core::models::{ClientKey, MonitorKey};
use crate::jwm::Jwm;
use crate::jwm::types::WMArgEnum;
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
            self.mark_layout_dirty();
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
                    .map(|m| *m.lt == LayoutEnum::SCROLLING)
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
                    .map(|m| *m.lt == LayoutEnum::VSTACK)
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
        if self.is_scrolling_layout() {
            return self.scrolling_set_column_width(backend, arg);
        }

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
            self.mark_layout_dirty();
            self.arrange(backend, Some(sel_mon_key));
        }
        Ok(())
    }

    /// 调整平铺窗口之间的间距 (gap)，per-monitor + per-tag 保存
    pub(crate) fn setgaps(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let WMArgEnum::Int(delta) = *arg {
            let sel_mon_key = self.state.sel_mon.ok_or("No monitor selected")?;
            if let Some(monitor) = self.state.monitors.get_mut(sel_mon_key) {
                let new_gap = (monitor.layout.gap + delta).clamp(0, 100);
                if new_gap != monitor.layout.gap {
                    monitor.layout.gap = new_gap;
                    monitor.update_current_tag_layout_params();
                    info!("[setgaps] Updated gap to {}", new_gap);
                }
            }
            self.mark_layout_dirty();
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

    pub(crate) fn current_selected_layout(
        &self,
        mon_key: MonitorKey,
    ) -> Result<Rc<LayoutEnum>, Box<dyn std::error::Error>> {
        self.state
            .monitors
            .get(mon_key)
            .map(|m| m.lt.clone())
            .ok_or_else(|| "No monitor".into())
    }

    pub(crate) fn apply_layout_change<F>(
        &mut self,
        backend: &mut dyn Backend,
        sel_mon_key: MonitorKey,
        change_layout: F,
    ) -> Result<Rc<LayoutEnum>, Box<dyn std::error::Error>>
    where
        F: FnOnce(&mut Self, MonitorKey) -> Result<Rc<LayoutEnum>, Box<dyn std::error::Error>>,
    {
        let old_layout = self.current_selected_layout(sel_mon_key)?;

        // Leaving vstack: promote the focused client to master so it stays
        // master in the new layout.
        if *old_layout == LayoutEnum::VSTACK {
            self.promote_focused_to_master(sel_mon_key);
        }

        self.exit_fullscreen_on_monitor(backend, sel_mon_key);

        let new_layout = change_layout(self, sel_mon_key)?;
        self.handle_fullscreen_layout_transition(backend, sel_mon_key, &old_layout, &new_layout)?;

        // Applying a tiling layout re-manages windows that only float because
        // they were dragged around; the float layout keeps them as they are.
        let reclaimed = if *new_layout != LayoutEnum::FLOAT {
            self.reclaim_drag_floating(sel_mon_key)
        } else {
            0
        };

        let (should_arrange, mon_num) = self.finalize_layout_update(sel_mon_key);
        if should_arrange || reclaimed > 0 {
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

        // Every path that changes which layout a tag is on — `setlayout`, the
        // cycle, the film-strip picker — comes through here, so this is the
        // one place the save has to be armed from.
        self.mark_layout_dirty();

        Ok(new_layout)
    }

    pub(crate) fn setlayout(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[setlayout]");
        let sel_mon_key = self.state.sel_mon.ok_or("No selected monitor")?;

        let layout = match arg {
            WMArgEnum::Layout(lt) => lt.clone(),
            // The argument-less form was the toggle before `lastlayout`
            // existed; old bindings keep their meaning.
            _ => return self.lastlayout(backend, arg),
        };

        // A layout key is idempotent: re-pressing it is a no-op, not a flip
        // to whatever invisible state the other slot happened to hold.
        if *self.current_selected_layout(sel_mon_key)? == *layout {
            return Ok(());
        }

        self.apply_layout_change(backend, sel_mon_key, |this, mon_key| {
            let cur_tag = this.pertag_current_tag(mon_key)?;
            this.set_new_layout(mon_key, &layout, cur_tag);
            Ok(layout.clone())
        })?;

        Ok(())
    }

    /// Go back to the layout the current tag was on before the last change —
    /// the explicit form of what re-pressing a layout key used to mean.
    pub(crate) fn lastlayout(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[lastlayout]");
        let sel_mon_key = self.state.sel_mon.ok_or("No selected monitor")?;

        let previous = self
            .state
            .monitors
            .get(sel_mon_key)
            .ok_or("No monitor")?
            .prev_lt
            .clone();
        if *self.current_selected_layout(sel_mon_key)? == *previous {
            return Ok(());
        }

        self.apply_layout_change(backend, sel_mon_key, |this, mon_key| {
            let cur_tag = this.pertag_current_tag(mon_key)?;
            this.set_new_layout(mon_key, &previous, cur_tag);
            Ok(previous.clone())
        })?;

        Ok(())
    }

    fn pertag_current_tag(&self, mon_key: MonitorKey) -> Result<usize, Box<dyn std::error::Error>> {
        self.state
            .monitors
            .get(mon_key)
            .and_then(|m| m.pertag.as_ref())
            .map(|p| p.cur_tag)
            .ok_or_else(|| "No pertag".into())
    }

    pub(crate) fn cyclelayout(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("[cyclelayout]");
        let sel_mon_key = self.state.sel_mon.ok_or("No selected monitor")?;

        let dir = match arg {
            WMArgEnum::Int(i) => *i,
            _ => 1,
        };

        // The film strip takes over the cycle when it can: pressing the key
        // still steps to the next layout, it just shows what the neighbours
        // look like while it does.
        if self.cycle_through_layout_picker(backend, dir)? {
            return Ok(());
        }

        self.apply_layout_change(backend, sel_mon_key, |this, mon_key| {
            let cur_tag = this.pertag_current_tag(mon_key)?;

            let current = this.current_selected_layout(mon_key)?;
            let next = if dir >= 0 {
                current.cycle_next()
            } else {
                current.cycle_prev()
            };

            let next_rc = Rc::new(next.clone());
            this.set_new_layout(mon_key, &next_rc, cur_tag);
            Ok(next_rc)
        })?;

        Ok(())
    }

    /// Handle bar visibility and border_w changes when transitioning to/from fullscreen layout
    fn handle_fullscreen_layout_transition(
        &mut self,
        backend: &mut dyn Backend,
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
                .map(|keys| keys.to_vec())
                .unwrap_or_default();

            for ck in client_keys {
                if let Some(client) = self.state.clients.get_mut(ck) {
                    if !client.state.is_floating {
                        client.geometry.border_w = border_w;
                    }
                }
            }
        }

        // The flag alone only stops the work area from reserving the bar's
        // pixels; the bar window itself has to move. The caller's arrange()
        // would also do this, but it is skipped when the tag has no selection.
        self.sync_secondary_bar_position(backend, mon_key);

        Ok(())
    }

    pub(crate) fn set_new_layout(
        &mut self,
        sel_mon_key: MonitorKey,
        layout: &Rc<LayoutEnum>,
        cur_tag: usize,
    ) {
        if let Some(monitor) = self.state.monitors.get_mut(sel_mon_key) {
            if *monitor.lt == **layout {
                return;
            }
            monitor.prev_lt = monitor.lt.clone();
            monitor.lt = layout.clone();
            if let Some(ref mut pertag) = monitor.pertag {
                pertag.prev_lts[cur_tag] = pertag.lts[cur_tag].clone();
                pertag.lts[cur_tag] = layout.clone();
            }
        }
    }

    fn finalize_layout_update(&mut self, sel_mon_key: MonitorKey) -> (bool, Option<i32>) {
        if let Some(monitor) = self.state.monitors.get_mut(sel_mon_key) {
            monitor.lt_symbol = monitor.lt.symbol().to_string();

            let has_selection = monitor.sel.is_some();
            let mon_num = monitor.num;

            (has_selection, Some(mon_num))
        } else {
            (false, None)
        }
    }
}
