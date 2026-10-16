//! Physical-axis resizing within a layout. Old mfact/cfact bindings retain
//! their layout-specific meanings; this path never turns a tile into a float.
use std::collections::HashMap;

use crate::backend::api::{Backend, ResizeEdge};
use crate::config::CONFIG;
use crate::core::layout::{LayoutEnum, LayoutResult};
use crate::core::master_resize::{self, ResizeFrame, ResizeIntent};
use crate::core::models::{ClientKey, MonitorKey, SizeHints};
use crate::core::types::Rect;
use crate::jwm::Jwm;
use crate::jwm::geometry::GeometryConstraints;
use crate::jwm::mouse_handler::DragMode;
use crate::jwm::types::WMArgEnum;

pub(crate) type ResizeScope = (u32, &'static str);

impl Jwm {
    pub(crate) fn layout_resize_scope(&self, mon: MonitorKey) -> Option<ResizeScope> {
        let m = self.state.monitors.get(mon)?;
        Some((m.tag_set[m.sel_tags], m.lt.0))
    }

    pub(crate) fn is_layout_resize_target(&self, key: ClientKey) -> bool {
        let Some(c) = self.state.clients.get(key) else {
            return false;
        };
        if c.state.is_floating
            || c.state.is_fullscreen
            || c.state.is_fixed
            || c.state.is_maximize_realized()
            || c.state.is_pip
        {
            return false;
        }
        let Some(mon) = c.mon else {
            return false;
        };
        let Some(m) = self.state.monitors.get(mon) else {
            return false;
        };
        if *m.lt == LayoutEnum::FLOAT {
            return false;
        }
        let clients = self.collect_tileable_clients(mon);
        let Some(index) = clients.iter().position(|&(k, _, _)| k == key) else {
            return false;
        };
        if clients.len() == 1 || m.layout.n_master == 0 {
            return true;
        }
        match *m.lt {
            LayoutEnum::TILE
            | LayoutEnum::FIBONACCI
            | LayoutEnum::CENTERED_MASTER
            | LayoutEnum::BSTACK
            | LayoutEnum::DECK
            | LayoutEnum::THREE_COL => index < m.layout.n_master as usize,
            _ => true, // Equal-rank layouts and vstack's selected main card.
        }
    }

    pub(crate) fn resize_drag_mode(&self, key: ClientKey, edge: ResizeEdge) -> DragMode {
        if self.is_layout_resize_target(key)
            && let Some(scope) = self
                .state
                .clients
                .get(key)
                .and_then(|c| c.mon)
                .and_then(|m| self.layout_resize_scope(m))
        {
            DragMode::ResizeTiled(edge, scope)
        } else {
            DragMode::Resize(edge)
        }
    }

    fn layout_resize_hints(
        &mut self,
        backend: &mut dyn Backend,
        frames: &[ResizeFrame<ClientKey>],
    ) -> HashMap<ClientKey, SizeHints> {
        if !CONFIG.load().behavior().resize_hints {
            return HashMap::new();
        }
        frames
            .iter()
            .filter_map(|f| {
                let _ = self.ensure_size_hints_valid(backend, f.client);
                self.state
                    .clients
                    .get(f.client)
                    .map(|c| (f.client, c.size_hints.clone()))
            })
            .collect()
    }

    pub(crate) fn prepare_layout_resize(
        &mut self,
        backend: &mut dyn Backend,
        mon: MonitorKey,
        area: Rect,
        gap: i32,
        results: Vec<LayoutResult<ClientKey>>,
    ) -> (Vec<LayoutResult<ClientKey>>, bool) {
        let Some(scope) = self.layout_resize_scope(mon) else {
            return (results, false);
        };
        // Ordinary arrange must not create observable empty monitor state.
        if !self
            .state
            .monitors
            .get(mon)
            .is_some_and(|m| m.layout_resizes.contains_key(&scope))
        {
            return (results, false);
        }
        // Vstack temporarily reorders compute input on focus. The logical tiled
        // order is the topology: focus alone must not reset a resize intention.
        let topology = self
            .collect_tileable_clients(mon)
            .into_iter()
            .map(|(k, _, _)| k)
            .collect();
        let frames: Vec<_> = results
            .iter()
            .map(|r| ResizeFrame {
                client: r.key,
                rect: r.rect,
                border: self
                    .state
                    .clients
                    .get(r.key)
                    .map_or(0, |c| c.geometry.border_w),
            })
            .collect();
        let groups = if scope.1 == LayoutEnum::SCROLLING.0 {
            self.scrolling_state_for_monitor(mon)
                .map(|s| s.columns.clone())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let intent = if let Some(m) = self.state.monitors.get_mut(mon) {
            let context = m.layout_resizes.get_mut(&scope).unwrap();
            context.sync(topology, area, gap, frames.clone());
            context.sync_groups(groups);
            context.intent
        } else {
            None
        };
        let Some(intent) = intent else {
            self.prune_layout_resize(mon, scope);
            return (results, false);
        };
        let Some(anchor) = frames.iter().find(|f| f.client == intent.client) else {
            return (results, false);
        };
        let start =
            master_resize::intent_from_rect(anchor.client, anchor.rect, area, gap, anchor.border);
        let hints = self.layout_resize_hints(backend, &frames);
        let fit = |key: ClientKey, w: i32, h: i32| {
            let border = frames
                .iter()
                .find(|f| f.client == key)
                .map_or(0, |f| f.border);
            let w = if scope.1 == LayoutEnum::SCROLLING.0 {
                w.min(master_resize::usable_content(area, gap, border).0 as i32)
            } else {
                w
            };
            hints.get(&key).map_or((w, h), |hint| {
                GeometryConstraints::calculate_constrained_size(w, h, hint)
            })
        };
        if let Some((_, mut plan)) =
            master_resize::bounded_plan(area, gap, &frames, start, intent, &fit)
        {
            if scope.1 == LayoutEnum::SCROLLING.0 {
                let group = self
                    .scrolling_state_for_monitor(mon)
                    .and_then(|s| s.focused_column_index().and_then(|i| s.columns.get(i)))
                    .cloned()
                    .unwrap_or_default();
                if !master_resize::center_group(area, gap, &mut plan, &group) {
                    return (results, false);
                }
            }
            // Already fitted atomically. Applying size hints again is not
            // idempotent for every aspect/increment combination.
            (
                plan.into_iter()
                    .map(|f| LayoutResult {
                        key: f.client,
                        rect: f.rect,
                    })
                    .collect(),
                true,
            )
        } else {
            // A new output/hint set can make even the native layout infeasible.
            // Drop the override and leave that pre-existing layout policy alone.
            if let Some(m) = self.state.monitors.get_mut(mon) {
                m.layout_resizes.remove(&scope);
            }
            (results, false)
        }
    }

    fn set_layout_resize_intent(
        &mut self,
        backend: &mut dyn Backend,
        mon: MonitorKey,
        scope: ResizeScope,
        wanted: ResizeIntent<ClientKey>,
    ) -> bool {
        let Some(context) = self
            .state
            .monitors
            .get(mon)
            .and_then(|m| m.layout_resizes.get(&scope))
            .cloned()
        else {
            return false;
        };
        let Some(c) = self.state.clients.get(wanted.client) else {
            return false;
        };
        if c.mon != Some(mon) || c.state.is_fullscreen || c.state.is_floating {
            return false;
        }
        let current = Rect::new(c.geometry.x, c.geometry.y, c.geometry.w, c.geometry.h);
        let start = if context.intent.is_some_and(|i| i.client == wanted.client) {
            context.intent.unwrap()
        } else {
            master_resize::intent_from_rect(
                wanted.client,
                current,
                context.area,
                context.gap,
                c.geometry.border_w,
            )
        };
        let hints = self.layout_resize_hints(backend, &context.frames);
        let fit = |key: ClientKey, w: i32, h: i32| {
            let border = context
                .frames
                .iter()
                .find(|f| f.client == key)
                .map_or(0, |f| f.border);
            let w = if scope.1 == LayoutEnum::SCROLLING.0 {
                w.min(master_resize::usable_content(context.area, context.gap, border).0 as i32)
            } else {
                w
            };
            hints.get(&key).map_or((w, h), |hint| {
                GeometryConstraints::calculate_constrained_size(w, h, hint)
            })
        };
        let Some((effective, _)) = master_resize::bounded_plan(
            context.area,
            context.gap,
            &context.frames,
            start,
            wanted,
            &fit,
        ) else {
            return false;
        };
        if let Some(c) = self
            .state
            .monitors
            .get_mut(mon)
            .and_then(|m| m.layout_resizes.get_mut(&scope))
        {
            c.intent = Some(effective);
        }
        let animations = self.suppress_layout_animation;
        self.suppress_layout_animation = true;
        self.arrange(backend, Some(mon));
        self.suppress_layout_animation = animations;
        self.broadcast_visible_window_states_on_monitor(backend, mon);
        true
    }

    fn prime_layout_resize(
        &mut self,
        backend: &mut dyn Backend,
        mon: MonitorKey,
        scope: ResizeScope,
    ) {
        if let Some(m) = self.state.monitors.get_mut(mon) {
            m.layout_resizes.entry(scope).or_default().preparing = true;
        }
        self.arrange(backend, Some(mon));
    }

    fn prune_layout_resize(&mut self, mon: MonitorKey, scope: ResizeScope) {
        if let Some(m) = self.state.monitors.get_mut(mon)
            && m.layout_resizes
                .get(&scope)
                .is_some_and(|c| !c.preparing && c.intent.is_none() && c.drag_snapshot.is_none())
        {
            m.layout_resizes.remove(&scope);
        }
    }

    fn resize_physical_axis(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
        horizontal: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let WMArgEnum::Float(delta) = *arg else {
            return Ok(());
        };
        if !delta.is_finite() || delta == 0.0 {
            return Ok(());
        }
        let Some(key) = self.get_selected_client_key() else {
            return Ok(());
        };
        let Some(c) = self.state.clients.get(key) else {
            return Ok(());
        };
        if c.state.is_fullscreen
            || c.state.is_fixed
            || c.state.is_maximize_realized()
            || c.state.is_pip
        {
            return Ok(());
        }
        let Some(mon) = c.mon else {
            return Ok(());
        };
        let Some(scope) = self.layout_resize_scope(mon) else {
            return Ok(());
        };
        if self.state.sel_mon != Some(mon) || !self.is_client_visible_on_monitor(key, mon) {
            return Ok(());
        }
        if c.state.is_floating || scope.1 == LayoutEnum::FLOAT.0 {
            let rect = Rect::new(c.geometry.x, c.geometry.y, c.geometry.w, c.geometry.h);
            let area = self.monitor_work_area(mon).unwrap_or(rect);
            let (mut w, mut h) = (rect.w, rect.h);
            if horizontal {
                w = (f64::from(w) + f64::from(area.w) * f64::from(delta))
                    .round()
                    .clamp(1.0, f64::from(area.w.max(1))) as i32;
            } else {
                h = (f64::from(h) + f64::from(area.h) * f64::from(delta))
                    .round()
                    .clamp(1.0, f64::from(area.h.max(1))) as i32;
            }
            self.resize_client(
                backend,
                key,
                rect.x.saturating_add((rect.w - w) / 2),
                rect.y.saturating_add((rect.h - h) / 2),
                w,
                h,
                false,
            );
            if let Some(c) = self.state.clients.get_mut(key) {
                c.geometry.floating_x = c.geometry.x;
                c.geometry.floating_y = c.geometry.y;
                c.geometry.floating_w = c.geometry.w;
                c.geometry.floating_h = c.geometry.h;
            }
            self.broadcast_visible_window_states_on_monitor(backend, mon);
            return Ok(());
        }
        // Keyboard/IPC axes resize the selected tile, including stack clients.
        // Pointer admission keeps its separate master-only compatibility rules.
        if !self
            .collect_tileable_clients(mon)
            .iter()
            .any(|&(candidate, _, _)| candidate == key)
        {
            return Ok(());
        }
        self.prime_layout_resize(backend, mon, scope);
        if let Some(context) = self
            .state
            .monitors
            .get_mut(mon)
            .and_then(|m| m.layout_resizes.get_mut(&scope))
        {
            context.preparing = false;
        }
        let Some(c) = self.state.clients.get(key) else {
            return Ok(());
        };
        let Some(context) = self
            .state
            .monitors
            .get(mon)
            .and_then(|m| m.layout_resizes.get(&scope))
        else {
            return Ok(());
        };
        let start = master_resize::intent_from_rect(
            key,
            Rect::new(c.geometry.x, c.geometry.y, c.geometry.w, c.geometry.h),
            context.area,
            context.gap,
            c.geometry.border_w,
        );
        if let Some(wanted) = master_resize::adjust_intent(start, horizontal, f64::from(delta)) {
            self.set_layout_resize_intent(backend, mon, scope, wanted);
        }
        self.prune_layout_resize(mon, scope);
        Ok(())
    }

    pub(crate) fn resize_horizontal(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.resize_physical_axis(backend, arg, true)
    }
    pub(crate) fn resize_vertical(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.resize_physical_axis(backend, arg, false)
    }
    pub(crate) fn clear_current_layout_resize(&mut self) {
        if let Some(mon) = self.state.sel_mon
            && let Some(scope) = self.layout_resize_scope(mon)
            && let Some(m) = self.state.monitors.get_mut(mon)
        {
            m.layout_resizes.remove(&scope);
        }
    }

    pub(crate) fn reset_layout_size(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(mon) = self.state.sel_mon
            && let Some(scope) = self.layout_resize_scope(mon)
        {
            if let Some(m) = self.state.monitors.get_mut(mon) {
                m.layout_resizes.remove(&scope);
            }
            self.arrange(backend, Some(mon));
            self.broadcast_visible_window_states_on_monitor(backend, mon);
        }
        Ok(())
    }

    pub(crate) fn arm_tiled_resize(
        &mut self,
        backend: &mut dyn Backend,
        key: ClientKey,
        scope: ResizeScope,
    ) {
        let Some(mon) = self.state.clients.get(key).and_then(|c| c.mon) else {
            return;
        };
        self.prime_layout_resize(backend, mon, scope);
        if let Some(c) = self
            .state
            .monitors
            .get_mut(mon)
            .and_then(|m| m.layout_resizes.get_mut(&scope))
        {
            c.drag_snapshot = Some(c.intent);
            c.preparing = false;
        }
    }

    pub(crate) fn finish_tiled_resize(
        &mut self,
        backend: &mut dyn Backend,
        mon: Option<MonitorKey>,
        scope: ResizeScope,
        cancel: bool,
    ) {
        if let Some(mon) = mon {
            let cancel = cancel || self.layout_resize_scope(mon) != Some(scope);
            if let Some(c) = self
                .state
                .monitors
                .get_mut(mon)
                .and_then(|m| m.layout_resizes.get_mut(&scope))
                && let Some(original) = c.drag_snapshot.take()
                && cancel
            {
                c.intent = original;
            }
            self.prune_layout_resize(mon, scope);
            self.arrange(backend, Some(mon));
            self.broadcast_visible_window_states_on_monitor(backend, mon);
        }
    }

    pub(crate) fn tiled_resize_valid(
        &self,
        client: ClientKey,
        mon: Option<MonitorKey>,
        scope: ResizeScope,
    ) -> bool {
        mon.is_some_and(|mon| {
            self.layout_resize_scope(mon) == Some(scope)
                && self.is_layout_resize_target(client)
                && self
                    .state
                    .clients
                    .get(client)
                    .is_some_and(|c| c.mon == Some(mon))
                && self
                    .state
                    .monitors
                    .get(mon)
                    .and_then(|m| m.layout_resizes.get(&scope))
                    .is_some_and(|c| c.drag_snapshot.is_some())
        })
    }

    /// Return true for every tiled resize, including invalidated grabs, so the
    /// caller never falls through to backend floating geometry synchronization.
    pub(crate) fn motion_tiled_resize(
        &mut self,
        backend: &mut dyn Backend,
        x: f64,
        y: f64,
    ) -> bool {
        let Some(ctl) = self.drag_ctl else {
            return false;
        };
        let DragMode::ResizeTiled(edge, scope) = ctl.mode else {
            return false;
        };
        let valid = self.tiled_resize_valid(ctl.client, ctl.mon, scope);
        if !valid {
            self.cancel_pointer_drag(backend);
            return true;
        }
        let mon = ctl.mon.unwrap();
        let Some(context) = self
            .state
            .monitors
            .get(mon)
            .and_then(|m| m.layout_resizes.get(&scope))
        else {
            return true;
        };
        let border = self
            .state
            .clients
            .get(ctl.client)
            .map_or(0, |c| c.geometry.border_w);
        let (w, h) = master_resize::usable_content(context.area, context.gap, border);
        let (dx, dy) = master_resize::pointer_resize_delta(
            matches!(
                edge,
                ResizeEdge::Left | ResizeEdge::TopLeft | ResizeEdge::BottomLeft
            ),
            matches!(
                edge,
                ResizeEdge::Right | ResizeEdge::TopRight | ResizeEdge::BottomRight
            ),
            matches!(
                edge,
                ResizeEdge::Top | ResizeEdge::TopLeft | ResizeEdge::TopRight
            ),
            matches!(
                edge,
                ResizeEdge::Bottom | ResizeEdge::BottomLeft | ResizeEdge::BottomRight
            ),
            x - ctl.start_root.0,
            y - ctl.start_root.1,
        );
        let (gain_x, gain_y) = master_resize::pointer_resize_gains(
            context.area,
            context.gap,
            &context.frames,
            ctl.client,
            matches!(
                edge,
                ResizeEdge::Left | ResizeEdge::TopLeft | ResizeEdge::BottomLeft
            ),
            matches!(
                edge,
                ResizeEdge::Top | ResizeEdge::TopLeft | ResizeEdge::TopRight
            ),
        );
        let (dx, dy) = (dx * gain_x, dy * gain_y);
        let wanted = ResizeIntent {
            client: ctl.client,
            width: ((f64::from(ctl.orig_geom.2) + dx) / w).clamp(0.0, 1.0),
            height: ((f64::from(ctl.orig_geom.3) + dy) / h).clamp(0.0, 1.0),
        };
        self.set_layout_resize_intent(backend, mon, scope, wanted);
        self.last_mouse_root = (x, y);
        true
    }
}
