use crate::backend::common_define::WindowId;
use crate::config::CONFIG;
use crate::core::layout::LayoutEnum;
use crate::core::models::{ClientKey, MonitorKey, WMMonitor};
use crate::core::types::Rect;
use crate::jwm::Jwm;
use log::{info, warn};
use std::collections::HashMap;
use std::sync::OnceLock;

/// `JWM_DEBUG_WORKAREA=1` traces every work-area decision. Read once: the
/// work area is computed per monitor on every frame and per client on every
/// arrange, and an environment lookup takes the process-wide environment
/// lock and allocates each time.
fn workarea_debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();

    *ENABLED.get_or_init(|| {
        std::env::var("JWM_DEBUG_WORKAREA")
            .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

/// Pixels one dock takes off its edge of the work area. The depth is a
/// client-owned number — a layer-shell exclusive zone, or a geometry the
/// client chose — so, like an X11 strut, it is bounded by the output before
/// it is trusted, and the padding is added without overflowing.
///
/// The padded depth, not the depth, is what is floored at zero: a dock whose
/// geometry places it entirely outside the work area reserves nothing, which
/// is what leaves `top`/`bottom`/`left`/`right` all zero and lets the
/// historical status-bar offset below stand in. Flooring the depth first
/// would turn such a dock into a `pad`-pixel reservation and suppress that
/// fallback, hiding the bar's own offset behind a five-pixel gap.
fn bounded_dock_reservation(depth: i64, pad: i32, extent: i32) -> i32 {
    let limit = i64::from(extent.max(1));
    depth.saturating_add(i64::from(pad.max(0))).clamp(0, limit) as i32
}

/// Widen before both coordinate addition and subtraction: negative outputs
/// and parked client coordinates must not overflow while classifying an edge.
fn dock_edge_distances(dock: Rect, work: Rect) -> (i64, i64, i64, i64) {
    let (dx, dy, dw, dh) = (
        i64::from(dock.x),
        i64::from(dock.y),
        i64::from(dock.w),
        i64::from(dock.h),
    );
    let (wx, wy, ww, wh) = (
        i64::from(work.x),
        i64::from(work.y),
        i64::from(work.w),
        i64::from(work.h),
    );
    (
        (dy - wy).abs(),
        ((wy + wh) - (dy + dh)).abs(),
        (dx - wx).abs(),
        ((wx + ww) - (dx + dw)).abs(),
    )
}

/// A dock must overlap this physical output before its edge reservation counts.
/// Padding extends only the edge-normal axis, never the panel's long axis.
fn dock_reaches_output_edge(dock: Rect, physical: Rect, edge: &str, pad: i32) -> bool {
    if dock.w <= 0 || dock.h <= 0 || physical.w <= 0 || physical.h <= 0 {
        return false;
    }
    let (dx, dy, dw, dh) = (
        i64::from(dock.x),
        i64::from(dock.y),
        i64::from(dock.w),
        i64::from(dock.h),
    );
    let (mx, my, mw, mh) = (
        i64::from(physical.x),
        i64::from(physical.y),
        i64::from(physical.w),
        i64::from(physical.h),
    );
    let pad = i64::from(pad.max(0));
    match edge {
        "top" | "bottom" => {
            dx < mx + mw && dx + dw > mx && dy - pad < my + mh && dy + dh + pad > my
        }
        "left" | "right" => {
            dy < my + mh && dy + dh > my && dx - pad < mx + mw && dx + dw + pad > mx
        }
        _ => false,
    }
}

/// The configured bar starts at the physical top plus padding. Work-area
/// struts may already reserve that strip; only its uncovered remainder counts.
fn unreserved_top_fallback(offset: i32, m_y: i32, w_y: i32) -> i32 {
    let reserved = (i64::from(w_y) - i64::from(m_y)).max(0);
    (i64::from(offset.max(0)) - reserved).max(0) as i32
}

/// Share the decoration policy between live layout and drag simulation.
pub(super) fn tiled_client_border_width(configured: i32, no_decorations: bool) -> i32 {
    if no_decorations { 0 } else { configured }
}

impl Jwm {
    pub(crate) fn nexttiled(
        &self,
        mon_key: MonitorKey,
        start_from: Option<ClientKey>,
    ) -> Option<ClientKey> {
        let client_list = self.get_monitor_clients(mon_key);
        let start_index = if let Some(start_key) = start_from {
            client_list
                .iter()
                .position(|&k| k == start_key)
                .map(|i| i + 1)
                .unwrap_or(0)
        } else {
            0
        };

        for &client_key in &client_list[start_index..] {
            if let Some(client) = self.state.clients.get(client_key) {
                if !client.state.is_floating
                    && self.is_client_visible_on_monitor(client_key, mon_key)
                {
                    return Some(client_key);
                }
            }
        }
        None
    }

    pub(crate) fn get_monitor_info(
        &self,
        mon_key: MonitorKey,
    ) -> (i32, i32, i32, i32, f32, u32, i32, i32) {
        if let Some(monitor) = self.state.monitors.get(mon_key) {
            let client_y_offset = self.get_client_y_offset(monitor);
            (
                monitor.geometry.w_x,
                monitor.geometry.w_y,
                monitor.geometry.w_w,
                monitor.geometry.w_h,
                monitor.layout.m_fact,
                monitor.layout.n_master,
                monitor.num,
                client_y_offset,
            )
        } else {
            warn!("[get_monitor_info] Monitor {:?} not found", mon_key);
            (0, 0, 0, 0, 0.55, 1, 0, 0)
        }
    }

    /// Keep the configured border and gap on every tiled client, including a
    /// lone window, so the compositor ring, glow and shadow have room on all
    /// four sides. Client-side decorations stay borderless; they still sit
    /// inside the gap.
    pub(crate) fn apply_smart_borders(
        &mut self,
        mon_key: MonitorKey,
        clients: &[(ClientKey, f32, i32)],
    ) -> (i32, i32) {
        let cfg = CONFIG.load();
        let default_border = cfg.border_px() as i32;
        let monitor_gap = self
            .state
            .monitors
            .get(mon_key)
            .map(|m| m.layout.gap)
            .unwrap_or_else(|| cfg.gap_px() as i32);
        let effective_border = default_border;
        let effective_gap = monitor_gap;
        for &(key, _, _) in clients {
            if let Some(client) = self.state.clients.get_mut(key) {
                // A client-side frame owns the decoration permanently. The
                // layout ring must never be added back onto a CSD client.
                client.geometry.border_w =
                    tiled_client_border_width(effective_border, client.state.no_decorations);
            }
        }
        (effective_border, effective_gap)
    }

    pub(crate) fn collect_tileable_clients(
        &self,
        mon_key: MonitorKey,
    ) -> Vec<(ClientKey, f32, i32)> {
        let client_list = self.get_monitor_clients(mon_key);
        let mut clients = Vec::new();
        for &client_key in client_list {
            if let Some(client) = self.state.clients.get(client_key) {
                if !client.state.is_floating
                    && self.is_client_visible_on_monitor(client_key, mon_key)
                {
                    clients.push((
                        client_key,
                        client.state.client_fact,
                        client.geometry.border_w,
                    ));
                }
            }
        }
        clients
    }

    /// Pull windows that only float because the user dragged/resized them back
    /// into the tiling grid.
    ///
    /// Applying a layout is an explicit "tile what is on screen" request, so
    /// drag-induced floats are reclaimed. Floats that come from a rule, a window
    /// type (dialog/dock/desktop), an explicit toggle, PiP, fullscreen or a fixed
    /// size window are left alone — those float by design, not by accident.
    /// Maximized windows (either axis) are deliberate floats too: maximize owns
    /// their geometry until they are unmaximized, even when a drag or a
    /// promotion from the tiling is what made them float.
    ///
    /// Returns the number of clients reclaimed.
    pub(crate) fn reclaim_drag_floating(&mut self, mon_key: MonitorKey) -> usize {
        let candidates: Vec<ClientKey> = self
            .state
            .monitor_clients
            .get(mon_key)
            .map(|keys| {
                keys.iter()
                    .copied()
                    .filter(|&key| {
                        self.state
                            .clients
                            .get(key)
                            .map(|c| {
                                c.state.is_floating
                                    && c.state.is_drag_floating
                                    && !c.state.is_fixed
                                    && !c.state.is_fullscreen
                                    && !c.state.is_pip
                                    && !c.state.is_dock
                                    && !c.state.is_sticky
                                    && !c.state.is_swallowed
                                    && !c.state.maximized_axes().any()
                            })
                            .unwrap_or(false)
                            && self.is_client_visible_on_monitor(key, mon_key)
                    })
                    .collect()
            })
            .unwrap_or_default();

        for key in &candidates {
            if let Some(client) = self.state.clients.get_mut(*key) {
                // Remember where the user left it, so toggling back to floating
                // (or switching to the float layout) restores that geometry.
                client.geometry.floating_x = client.geometry.x;
                client.geometry.floating_y = client.geometry.y;
                client.geometry.floating_w = client.geometry.w;
                client.geometry.floating_h = client.geometry.h;
                client.state.is_floating = false;
                client.state.is_drag_floating = false;
            }
            // Floating clients live at the tail of the monitor list; move the
            // client back among the tiled ones so it gets a layout slot.
            self.reorder_client_in_monitor_groups(*key);
        }

        if !candidates.is_empty() {
            info!(
                "[reclaim_drag_floating] retiled {} dragged client(s) on monitor {:?}",
                candidates.len(),
                mon_key
            );
        }
        candidates.len()
    }

    pub(crate) fn get_client_y_offset(&self, monitor: &WMMonitor) -> i32 {
        let show_bar = monitor
            .pertag
            .as_ref()
            .and_then(|p| p.show_bars.get(p.cur_tag))
            .copied()
            .unwrap_or(true);

        if show_bar {
            // Config-only fallback; observed dock geometry is handled by the
            // work-area calculation before this offset is used.
            let cfg = CONFIG.load();
            let fallback = cfg.status_bar_height() + cfg.status_bar_padding() * 2;

            fallback
        } else {
            0
        }
    }

    /// The area windows may occupy: everything left after the status bar,
    /// docks and the window tab bar have taken their share.
    pub(crate) fn monitor_work_area(&self, mon_key: MonitorKey) -> Option<Rect> {
        let area = self.monitor_work_area_untabbed(mon_key)?;
        Some(crate::jwm::window_tabs::without_tab_bar(
            area,
            self.tab_bar_reserved(mon_key),
        ))
    }

    /// The work area before the window tab bar is subtracted — which is also
    /// where that bar goes. Only [`Jwm::monitor_tab_bar`] and
    /// [`Jwm::monitor_work_area`] should need it; everything laying out or
    /// placing a window wants the tabbed area, or it will put the window
    /// underneath the strip.
    pub(crate) fn monitor_work_area_untabbed(&self, mon_key: MonitorKey) -> Option<Rect> {
        let monitor = self.state.monitors.get(mon_key)?;

        let debug_workarea = workarea_debug_enabled();

        let wx = monitor.geometry.w_x;
        let wy = monitor.geometry.w_y;
        let ww = monitor.geometry.w_w;
        let wh = monitor.geometry.w_h;

        let show_bar = monitor
            .pertag
            .as_ref()
            .and_then(|p| p.show_bars.get(p.cur_tag))
            .copied()
            .unwrap_or(true);
        if !show_bar {
            return Some(Rect::new(wx, wy, ww, wh));
        }

        // Subtract all visible dock-like clients (includes Wayland layer-shell panels).
        let mut top = 0i32;
        let mut bottom = 0i32;
        let mut left = 0i32;
        let mut right = 0i32;

        let pad = CONFIG.load().status_bar_padding().max(0);
        let threshold = i64::from(pad.max(8));

        if let Some(client_keys) = self.state.monitor_clients.get(mon_key) {
            for &client_key in client_keys {
                let client = match self.state.clients.get(client_key) {
                    Some(c) => c,
                    None => continue,
                };

                if !client.state.is_dock {
                    continue;
                }
                if !self.is_client_visible_on_monitor(client_key, mon_key) {
                    continue;
                }

                // Compute dock rect in monitor coordinates.
                let dx = client.geometry.x;
                let dy = client.geometry.y;
                let dw = client.geometry.w.max(0);
                let dh = client.geometry.h.max(0);

                // Skip degenerate geometry.
                if dw == 0 || dh == 0 {
                    continue;
                }

                // Ignore wallpaper / background-like surfaces that cover (almost) the entire
                // monitor. Some layer-shell backgrounds may appear as "dock" due to
                // exclusive_zone semantics, but they must not shrink the tiling area.
                if dw >= (ww * 9 / 10) && dh >= (wh * 9 / 10) {
                    if debug_workarea {
                        info!(
                            "[workarea] skip fullscreen dock win={:?} geom=({},{} {}x{}) ww={} wh={}",
                            client.win, dx, dy, dw, dh, ww, wh
                        );
                    }
                    continue;
                }

                let (dist_top, dist_bottom, dist_left, dist_right) =
                    dock_edge_distances(Rect::new(dx, dy, dw, dh), Rect::new(wx, wy, ww, wh));

                // Heuristic classification: prefer horizontal vs vertical panels.
                let is_horizontal = dw >= (ww * 2 / 3) && dh <= (wh / 2).max(1);
                let is_vertical = dh >= (wh * 2 / 3) && dw <= (ww / 2).max(1);

                let edge = if is_horizontal {
                    if dist_top <= dist_bottom {
                        "top"
                    } else {
                        "bottom"
                    }
                } else if is_vertical {
                    if dist_left <= dist_right {
                        "left"
                    } else {
                        "right"
                    }
                } else {
                    // Pick the closest edge.
                    let min = dist_top.min(dist_bottom).min(dist_left).min(dist_right);
                    if min == dist_top {
                        "top"
                    } else if min == dist_bottom {
                        "bottom"
                    } else if min == dist_left {
                        "left"
                    } else {
                        "right"
                    }
                };

                let physical = Rect::new(
                    monitor.geometry.m_x,
                    monitor.geometry.m_y,
                    monitor.geometry.m_w,
                    monitor.geometry.m_h,
                );
                if !dock_reaches_output_edge(Rect::new(dx, dy, dw, dh), physical, edge, pad) {
                    continue;
                }

                let exclusive_zone = client
                    .state
                    .dock_layer_info
                    .as_ref()
                    .map(|i| i.exclusive_zone)
                    .unwrap_or(0);

                let anchor_ok = client
                    .state
                    .dock_layer_info
                    .as_ref()
                    .map(|i| {
                        let any =
                            i.anchor_top || i.anchor_bottom || i.anchor_left || i.anchor_right;
                        if !any {
                            return true;
                        }
                        match edge {
                            "top" => i.anchor_top,
                            "bottom" => i.anchor_bottom,
                            "left" => i.anchor_left,
                            "right" => i.anchor_right,
                            _ => true,
                        }
                    })
                    .unwrap_or(true);

                let zone_px: i64 = if exclusive_zone == -1 {
                    match edge {
                        "top" | "bottom" => i64::from(dh),
                        "left" | "right" => i64::from(dw),
                        _ => 0,
                    }
                } else if exclusive_zone > 0 {
                    i64::from(exclusive_zone)
                } else {
                    0
                };

                if debug_workarea {
                    info!(
                        "[workarea] dock win={:?} edge={} geom=({},{} {}x{}) exclusive_zone={} zone_px={} dist(top/bot/left/right)=({}/{}/{}/{})",
                        client.win,
                        edge,
                        dx,
                        dy,
                        dw,
                        dh,
                        exclusive_zone,
                        zone_px,
                        dist_top,
                        dist_bottom,
                        dist_left,
                        dist_right
                    );
                }

                // The zone wins only where the client anchored the surface;
                // otherwise the visible geometry says how deep the dock is.
                let (dx, dy, dw, dh) = (i64::from(dx), i64::from(dy), i64::from(dw), i64::from(dh));
                match edge {
                    "top" => {
                        if dist_top <= threshold {
                            let depth = if zone_px > 0 && anchor_ok {
                                zone_px
                            } else {
                                dy + dh - i64::from(wy)
                            };
                            top = top.max(bounded_dock_reservation(depth, pad, wh));
                        }
                    }
                    "bottom" => {
                        if dist_bottom <= threshold {
                            let depth = if zone_px > 0 && anchor_ok {
                                zone_px
                            } else {
                                i64::from(wy) + i64::from(wh) - dy
                            };
                            bottom = bottom.max(bounded_dock_reservation(depth, pad, wh));
                        }
                    }
                    "left" => {
                        if dist_left <= threshold {
                            let depth = if zone_px > 0 && anchor_ok {
                                zone_px
                            } else {
                                dx + dw - i64::from(wx)
                            };
                            left = left.max(bounded_dock_reservation(depth, pad, ww));
                        }
                    }
                    "right" => {
                        if dist_right <= threshold {
                            let depth = if zone_px > 0 && anchor_ok {
                                zone_px
                            } else {
                                i64::from(wx) + i64::from(ww) - dx
                            };
                            right = right.max(bounded_dock_reservation(depth, pad, ww));
                        }
                    }
                    _ => {}
                }
            }
        }

        // Only the config fallback discounts a strip already covered by struts.
        // Observed docks can be stacked and keep their full reservation above.
        if top == 0 && bottom == 0 && left == 0 && right == 0 {
            top = unreserved_top_fallback(
                self.get_client_y_offset(monitor),
                monitor.geometry.m_y,
                wy,
            );
        }

        // Every reservation above came from a client. As with X11 struts,
        // two of them must never meet in the middle and leave a zero or
        // negative work area for the layouts to hand out.
        let (top, bottom) = crate::jwm::strut_manager::clamp_opposing_edges(top, bottom, wh);
        let (left, right) = crate::jwm::strut_manager::clamp_opposing_edges(left, right, ww);

        let x = wx.saturating_add(left);
        let y = wy.saturating_add(top);
        let w = ww.saturating_sub(left).saturating_sub(right).max(0);
        let h = wh.saturating_sub(top).saturating_sub(bottom).max(0);

        if debug_workarea {
            info!(
                "[workarea] result mon={} wx/wy/ww/wh=({},{},{},{}) offsets(top/bot/left/right)=({},{},{},{}) -> ({},{},{},{})",
                monitor.num, wx, wy, ww, wh, top, bottom, left, right, x, y, w, h
            );
        }
        Some(Rect::new(x, y, w, h))
    }

    /// Build the compositor layout for overview mode: a list of
    /// (win, x, y, w, h, is_selected, title) tuples.
    pub(crate) fn build_overview_layout(
        &self,
        clients: &[ClientKey],
    ) -> Vec<(WindowId, f32, f32, f32, f32, bool, String)> {
        let mut layout = Vec::new();
        let selected_client = self
            .state
            .sel_mon
            .and_then(|mon_key| self.state.monitors.get(mon_key))
            .and_then(|monitor| monitor.sel);
        let strip_geometry = self
            .state
            .sel_mon
            .and_then(|mon_key| {
                let monitor = self.state.monitors.get(mon_key)?;
                if *monitor.lt != LayoutEnum::SCROLLING {
                    return None;
                }
                let visible = self
                    .state
                    .monitor_clients
                    .get(mon_key)
                    .map(|monitor_clients| {
                        monitor_clients
                            .iter()
                            .copied()
                            .filter(|&ck| self.is_client_visible_by_key(ck))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_else(|| clients.to_vec());
                self.scrolling_state_for_monitor(mon_key)
                    .map(|state| state.overview_strip_geometry(&visible))
            })
            .unwrap_or_default()
            .into_iter()
            .map(|geometry| (geometry.client, geometry))
            .collect::<HashMap<_, _>>();

        for (i, &ck) in clients.iter().enumerate() {
            if let Some(client) = self.state.clients.get(ck) {
                let is_selected = selected_client.map_or(i == 0, |selected| selected == ck);
                let title = if client.name.is_empty() {
                    client.class.clone()
                } else if !client.class.is_empty()
                    && !client.name.eq_ignore_ascii_case(&client.class)
                {
                    format!("{} [{}]", client.name, client.class)
                } else {
                    client.name.clone()
                };
                let geometry = strip_geometry.get(&ck).copied();
                layout.push((
                    client.win,
                    geometry.map(|g| g.x_ratio).unwrap_or(0.0),
                    geometry.map(|g| g.y_ratio).unwrap_or(0.0),
                    geometry.map(|g| g.width_ratio).unwrap_or(0.0),
                    geometry.map(|g| g.height_ratio).unwrap_or(0.0),
                    is_selected,
                    title,
                ));
            }
        }
        layout
    }
}

#[cfg(test)]
mod tests {
    use super::{
        bounded_dock_reservation, dock_edge_distances, dock_reaches_output_edge,
        tiled_client_border_width, unreserved_top_fallback,
    };
    use crate::core::types::Rect;
    use crate::jwm::strut_manager::clamp_opposing_edges;

    #[test]
    fn dock_distances_widen_before_add_subtract_and_abs() {
        let work = Rect::new(0, 0, 1920, 1080);
        assert_eq!(
            dock_edge_distances(Rect::new(10, 20, 100, 30), work),
            (20, 1030, 10, 1810),
        );
        assert_eq!(
            dock_edge_distances(Rect::new(i32::MIN, 10, 30, 20), work),
            (10, 1050, 2_147_483_648, 2_147_485_538),
        );
        assert_eq!(
            dock_edge_distances(
                Rect::new(i32::MIN, i32::MAX, i32::MAX, i32::MAX),
                Rect::new(i32::MAX, i32::MIN, i32::MAX, i32::MAX),
            ),
            (4_294_967_295, 4_294_967_295, 4_294_967_295, 4_294_967_295),
        );
        assert_eq!(
            dock_edge_distances(
                Rect::new(-1920, -1080, 1920, 30),
                Rect::new(-1920, -1080, 1920, 1080),
            ),
            (0, 1050, 0, 0),
        );
    }

    #[test]
    fn tiled_borders_preserve_configured_width_unless_client_decorates() {
        for configured in [i32::MIN, -1, 0, 1, 5, i32::MAX] {
            assert_eq!(tiled_client_border_width(configured, false), configured);
            assert_eq!(tiled_client_border_width(configured, true), 0);
        }
    }

    #[test]
    fn dock_overlap_uses_physical_output_and_edge_normal_padding() {
        let output = Rect::new(-1920, -1080, 1920, 1080);
        for (dock, edge, pad, expected) in [
            (Rect::new(-1920, -1080, 1920, 30), "top", 0, true),
            (Rect::new(-3840, -1080, 1920, 30), "top", 8, false),
            (Rect::new(-1920, -1110, 1920, 30), "top", 0, false),
            (Rect::new(-1920, -1110, 1920, 30), "top", 1, true),
            (Rect::new(-1920, 0, 1920, 30), "bottom", 1, true),
            (Rect::new(-1950, -1080, 30, 1080), "left", 0, false),
            (Rect::new(-1950, -1080, 30, 1080), "left", 1, true),
            (Rect::new(0, -1080, 30, 1080), "right", 1, true),
            (Rect::new(-1920, -2160, 30, 1080), "left", 8, false),
            (Rect::new(-1920, -1080, 0, 30), "top", 8, false),
        ] {
            assert_eq!(dock_reaches_output_edge(dock, output, edge, pad), expected);
        }
        assert!(!dock_reaches_output_edge(
            output,
            Rect::new(0, 0, 0, 100),
            "top",
            8
        ));
        let extreme = Rect::new(i32::MAX - 4, i32::MIN, 100, 100);
        assert!(dock_reaches_output_edge(extreme, extreme, "top", i32::MAX));
    }

    #[test]
    fn config_fallback_only_reserves_the_uncovered_physical_top_strip() {
        for (offset, physical_y, work_y, expected) in [
            (38, 0, 0, 38),
            (38, 0, 30, 8),
            (38, 0, 38, 0),
            (38, 0, 50, 0),
            (38, -1080, -1050, 8),
            (38, 30, 0, 38),
            (-1, 0, 0, 0),
            (i32::MAX, i32::MIN, i32::MAX, 0),
            (i32::MAX, i32::MAX, i32::MIN, i32::MAX),
        ] {
            assert_eq!(
                unreserved_top_fallback(offset, physical_y, work_y),
                expected
            );
        }
    }

    #[test]
    fn a_dock_reservation_is_bounded_by_the_output_and_never_overflows() {
        // An ordinary 30 px bar with 4 px of padding.
        assert_eq!(bounded_dock_reservation(30, 4, 1080), 34);
        // `set_exclusive_zone(i32::MAX)` plus padding: no wrap, no more
        // than the output itself.
        assert_eq!(bounded_dock_reservation(i64::from(i32::MAX), 8, 1080), 1080);
        // A geometry-derived depth that puts the dock outside the work area
        // reserves nothing at all — not the padding — so the all-zero
        // fallback to the historical bar offset still fires.
        assert_eq!(bounded_dock_reservation(-20, 8, 1080), 0);
        // Padding still counts once the dock is actually inside.
        assert_eq!(bounded_dock_reservation(-2, 8, 1080), 6);
        assert_eq!(bounded_dock_reservation(0, -3, 1080), 0);
        assert_eq!(bounded_dock_reservation(50, 0, 0), 1);
    }

    #[test]
    fn opposing_dock_zones_leave_the_layouts_at_least_one_pixel() {
        // Two absurd zones, one per edge, on a second output at y = 1080:
        // the work area stays representable and non-empty.
        let wy = 1080i32;
        let wh = 1080i32;
        let top = bounded_dock_reservation(i64::from(i32::MAX), 0, wh);
        let bottom = bounded_dock_reservation(2_000_000_000, 0, wh);
        let (top, bottom) = clamp_opposing_edges(top, bottom, wh);
        assert_eq!((top, bottom), (1079, 0));
        let y = wy.saturating_add(top);
        let h = wh.saturating_sub(top).saturating_sub(bottom).max(0);
        assert_eq!((y, h), (2159, 1));
    }

    #[test]
    fn the_work_area_never_reads_the_environment_per_call() {
        // The work area is computed on every frame and every arrange; the
        // debug switch is read once at first use, as the compositor's is.
        const SOURCE: &str = include_str!("helpers.rs");
        let body = SOURCE
            .split_once("fn monitor_work_area_untabbed")
            .expect("monitor_work_area_untabbed")
            .1
            .split_once("fn build_overview_layout")
            .expect("the function after monitor_work_area_untabbed")
            .0;
        let needle = format!("{}::var(", "env");
        assert!(
            !body.contains(&needle),
            "monitor_work_area_untabbed reads the environment on every call"
        );
        assert!(body.contains("workarea_debug_enabled()"));
    }
}
