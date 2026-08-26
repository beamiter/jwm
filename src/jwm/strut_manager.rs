use crate::backend::api::{Backend, StrutPartial};
use crate::backend::common_define::WindowId;
use crate::core::models::MonitorKey;
use crate::core::types::Rect;
use crate::jwm::Jwm;
use log::info;
use std::collections::HashMap;

type ExternalStrut = (StrutPartial, Option<MonitorKey>);

fn cache_external_strut(
    entries: &mut HashMap<WindowId, ExternalStrut>,
    win: WindowId,
    next: ExternalStrut,
) -> bool {
    if entries.get(&win) == Some(&next) {
        return false;
    }
    entries.insert(win, next);
    true
}

fn window_center(origin: i32, extent: u32) -> i32 {
    i64::from(origin)
        .saturating_add(i64::from(extent) / 2)
        .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DesktopBounds {
    left: i64,
    top: i64,
    right: i64,
    bottom: i64,
}

fn span_intersects_monitor(
    start: u32,
    end: u32,
    monitor_start: i64,
    monitor_end: i64,
    hosts_here: bool,
) -> bool {
    // The four-value legacy _NET_WM_STRUT property is expanded with zero
    // partial extents by the X11 adapters. It carries no span information, so
    // keep attributing it to the monitor that owns the panel window.
    if start == 0 && end == 0 {
        return hosts_here;
    }

    i64::from(start) < monitor_end && i64::from(end) >= monitor_start
}

fn edge_depth(advertised: u32, distance_to_desktop_edge: i64, extent: i32) -> i32 {
    let limit = i64::from(extent.max(1));
    (i64::from(advertised) - distance_to_desktop_edge.max(0)).clamp(0, limit) as i32
}

fn clamp_opposing_edges(first: i32, second: i32, extent: i32) -> (i32, i32) {
    // A client-owned property is untrusted. Preserve the near edge first, but
    // never allow two malformed reservations to create a zero/negative work
    // area that later turns into enormous u32 ConfigureWindow dimensions.
    let capacity = extent.max(1).saturating_sub(1);
    let first = first.clamp(0, capacity);
    let second = second.clamp(0, capacity.saturating_sub(first));
    (first, second)
}

fn strut_reservation_for_monitor(
    strut: StrutPartial,
    host_matches: bool,
    monitor: Rect,
    desktop: DesktopBounds,
) -> (i32, i32, i32, i32) {
    let monitor_left = i64::from(monitor.x);
    let monitor_top = i64::from(monitor.y);
    let monitor_right = monitor_left.saturating_add(i64::from(monitor.w.max(1)));
    let monitor_bottom = monitor_top.saturating_add(i64::from(monitor.h.max(1)));

    let horizontal_span =
        |start, end| span_intersects_monitor(start, end, monitor_left, monitor_right, host_matches);
    let vertical_span =
        |start, end| span_intersects_monitor(start, end, monitor_top, monitor_bottom, host_matches);

    let top = horizontal_span(strut.top_start_x, strut.top_end_x).then(|| {
        edge_depth(
            strut.top,
            monitor_top.saturating_sub(desktop.top),
            monitor.h,
        )
    });
    let bottom = horizontal_span(strut.bottom_start_x, strut.bottom_end_x).then(|| {
        edge_depth(
            strut.bottom,
            desktop.bottom.saturating_sub(monitor_bottom),
            monitor.h,
        )
    });
    let left = vertical_span(strut.left_start_y, strut.left_end_y).then(|| {
        edge_depth(
            strut.left,
            monitor_left.saturating_sub(desktop.left),
            monitor.w,
        )
    });
    let right = vertical_span(strut.right_start_y, strut.right_end_y).then(|| {
        edge_depth(
            strut.right,
            desktop.right.saturating_sub(monitor_right),
            monitor.w,
        )
    });

    (
        top.unwrap_or(0),
        bottom.unwrap_or(0),
        left.unwrap_or(0),
        right.unwrap_or(0),
    )
}

impl Jwm {
    pub(crate) fn cache_external_strut(
        &mut self,
        win: WindowId,
        strut: StrutPartial,
        host: Option<MonitorKey>,
    ) -> bool {
        cache_external_strut(&mut self.external_struts, win, (strut, host))
    }

    pub fn get_strut_reserved(&self, mon_key: MonitorKey) -> (i32, i32, i32, i32) {
        let monitor = match self.state.monitors.get(mon_key) {
            Some(m) => m,
            None => return (0, 0, 0, 0),
        };
        let monitor_rect = Rect::new(
            monitor.geometry.m_x,
            monitor.geometry.m_y,
            monitor.geometry.m_w.max(1),
            monitor.geometry.m_h.max(1),
        );
        let desktop = self.state.monitors.values().fold(
            DesktopBounds {
                left: i64::MAX,
                top: i64::MAX,
                right: i64::MIN,
                bottom: i64::MIN,
            },
            |bounds, monitor| {
                let left = i64::from(monitor.geometry.m_x);
                let top = i64::from(monitor.geometry.m_y);
                let right = left.saturating_add(i64::from(monitor.geometry.m_w.max(1)));
                let bottom = top.saturating_add(i64::from(monitor.geometry.m_h.max(1)));
                DesktopBounds {
                    left: bounds.left.min(left),
                    top: bounds.top.min(top),
                    right: bounds.right.max(right),
                    bottom: bounds.bottom.max(bottom),
                }
            },
        );

        let mut top = 0i32;
        let mut bottom = 0i32;
        let mut left = 0i32;
        let mut right = 0i32;

        for (strut, host_mon) in self.external_struts.values() {
            let host_matches = host_mon.is_none_or(|host| host == mon_key);
            let reservation =
                strut_reservation_for_monitor(*strut, host_matches, monitor_rect, desktop);
            top = top.max(reservation.0);
            bottom = bottom.max(reservation.1);
            left = left.max(reservation.2);
            right = right.max(reservation.3);
        }

        let (top, bottom) = clamp_opposing_edges(top, bottom, monitor_rect.h);
        let (left, right) = clamp_opposing_edges(left, right, monitor_rect.w);
        (top, bottom, left, right)
    }

    pub fn apply_strut_reservations(&mut self) {
        let mon_keys: Vec<MonitorKey> = self.state.monitor_order.clone();
        for mon_key in mon_keys {
            let (strut_top, strut_bottom, strut_left, strut_right) =
                self.get_strut_reserved(mon_key);
            if let Some(monitor) = self.state.monitors.get_mut(mon_key) {
                monitor.geometry.w_x = monitor.geometry.m_x.saturating_add(strut_left);
                monitor.geometry.w_y = monitor.geometry.m_y.saturating_add(strut_top);
                monitor.geometry.w_w = monitor
                    .geometry
                    .m_w
                    .saturating_sub(strut_left)
                    .saturating_sub(strut_right)
                    .max(1);
                monitor.geometry.w_h = monitor
                    .geometry
                    .m_h
                    .saturating_sub(strut_top)
                    .saturating_sub(strut_bottom)
                    .max(1);
            }
        }
    }

    /// Resolve the monitor that physically hosts a panel window, so legacy
    /// whole-screen struts can be attributed to a single output.
    pub(crate) fn strut_host_monitor(
        &mut self,
        backend: &mut dyn Backend,
        win: WindowId,
    ) -> Option<MonitorKey> {
        let g = backend.window_ops().get_geometry(win).ok()?;
        self.strut_host_monitor_for_geometry(backend, g.x, g.y, g.w, g.h)
    }

    fn strut_host_monitor_for_geometry(
        &mut self,
        backend: &mut dyn Backend,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) -> Option<MonitorKey> {
        let cx = window_center(x, width);
        let cy = window_center(y, height);
        self.recttomon(backend, cx, cy)
    }

    /// Rehost one cached strut from ConfigureNotify geometry. The property can
    /// remain byte-for-byte identical while a panel crosses an output edge;
    /// comparing the complete `(strut, host)` entry makes that move visible to
    /// workarea policy.
    pub(crate) fn refresh_external_strut_host_from_geometry(
        &mut self,
        backend: &mut dyn Backend,
        win: WindowId,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) -> bool {
        let Some((strut, _)) = self.external_struts.get(&win).copied() else {
            return false;
        };
        let host = self.strut_host_monitor_for_geometry(backend, x, y, width, height);
        self.cache_external_strut(win, strut, host)
    }

    /// Re-resolve every host after monitor keys or output rectangles change.
    /// A failed geometry query intentionally stores `None` instead of keeping
    /// a slotmap key that may now refer to a retired monitor generation.
    pub(crate) fn refresh_external_strut_hosts(&mut self, backend: &mut dyn Backend) -> bool {
        let windows: Vec<WindowId> = self.external_struts.keys().copied().collect();
        let resolved: Vec<(WindowId, Option<MonitorKey>)> = windows
            .into_iter()
            .map(|win| (win, self.strut_host_monitor(backend, win)))
            .collect();
        let mut changed = false;
        for (win, host) in resolved {
            let Some((strut, _)) = self.external_struts.get(&win).copied() else {
                continue;
            };
            changed |= self.cache_external_strut(win, strut, host);
        }
        changed
    }

    /// Restore strut-adjusted workareas after an output mutation reset monitor
    /// geometry, then immediately feed the new workareas back through layout.
    pub(crate) fn reconcile_external_struts_after_topology_change(
        &mut self,
        backend: &mut dyn Backend,
    ) {
        if self.external_struts.is_empty() {
            return;
        }
        let hosts_changed = self.refresh_external_strut_hosts(backend);
        if hosts_changed {
            info!("[strut] Rehosted external struts after monitor topology change");
        }
        self.apply_strut_reservations();
        self.arrange(backend, None);
    }

    pub fn check_strut_on_manage(&mut self, backend: &mut dyn Backend, win: WindowId) {
        if let Some(strut) = backend.property_ops().get_window_strut_partial(win) {
            if strut.left > 0 || strut.right > 0 || strut.top > 0 || strut.bottom > 0 {
                info!(
                    "[strut] New window {:?} has strut: top={} bottom={} left={} right={}",
                    win, strut.top, strut.bottom, strut.left, strut.right
                );
                let host = self.strut_host_monitor(backend, win);
                self.cache_external_strut(win, strut, host);
                self.apply_strut_reservations();
                self.arrange(backend, None);
            }
        }
    }

    pub fn remove_strut_on_unmanage(&mut self, backend: &mut dyn Backend, win: WindowId) {
        if self.external_struts.remove(&win).is_some() {
            info!("[strut] Removed strut on unmanage for {:?}", win);
            self.apply_strut_reservations();
            self.arrange(backend, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_compares_the_complete_strut_and_host_entry() {
        let mut monitor_generations = slotmap::SlotMap::<MonitorKey, ()>::with_key();
        let retired = monitor_generations.insert(());
        monitor_generations.remove(retired);
        let current = monitor_generations.insert(());
        assert_ne!(retired, current);

        let win = WindowId::from_raw(77);
        let strut = StrutPartial {
            top: 24,
            ..Default::default()
        };
        let mut entries = HashMap::new();
        assert!(cache_external_strut(
            &mut entries,
            win,
            (strut, Some(retired))
        ));
        assert!(!cache_external_strut(
            &mut entries,
            win,
            (strut, Some(retired))
        ));
        assert!(
            cache_external_strut(&mut entries, win, (strut, Some(current))),
            "a host-only change must invalidate the workarea"
        );
        assert_eq!(entries[&win], (strut, Some(current)));
    }

    #[test]
    fn configure_and_topology_paths_are_both_wired_to_rehosting() {
        let dispatcher = include_str!("event_dispatcher.rs");
        assert!(dispatcher.contains("refresh_external_strut_host_from_geometry("));
        assert!(
            dispatcher
                .matches("reconcile_external_struts_after_topology_change(backend)")
                .count()
                >= 5,
            "OutputAdded/Removed/Changed, ScreenLayoutChanged and root ConfigureNotify must rehost"
        );
    }

    #[test]
    fn huge_configure_extents_do_not_wrap_the_host_probe() {
        assert_eq!(window_center(i32::MAX, u32::MAX), i32::MAX);
        assert_eq!(window_center(i32::MIN, u32::MAX), -1);
    }

    #[test]
    fn right_strut_is_relative_to_the_desktop_right_edge() {
        let desktop = DesktopBounds {
            left: 0,
            top: 0,
            right: 3840,
            bottom: 1080,
        };
        let strut = StrutPartial {
            right: 32,
            right_start_y: 0,
            right_end_y: 1079,
            ..Default::default()
        };

        assert_eq!(
            strut_reservation_for_monitor(strut, true, Rect::new(0, 0, 1920, 1080), desktop,),
            (0, 0, 0, 0)
        );
        assert_eq!(
            strut_reservation_for_monitor(strut, true, Rect::new(1920, 0, 1920, 1080), desktop,),
            (0, 0, 0, 32)
        );
    }

    #[test]
    fn bottom_strut_is_relative_to_the_desktop_bottom_edge() {
        let desktop = DesktopBounds {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 2160,
        };
        let strut = StrutPartial {
            bottom: 40,
            bottom_start_x: 0,
            bottom_end_x: 1919,
            ..Default::default()
        };

        assert_eq!(
            strut_reservation_for_monitor(strut, true, Rect::new(0, 0, 1920, 1080), desktop,),
            (0, 0, 0, 0)
        );
        assert_eq!(
            strut_reservation_for_monitor(strut, true, Rect::new(0, 1080, 1920, 1080), desktop,),
            (0, 40, 0, 0)
        );
    }

    #[test]
    fn negative_origin_edges_use_the_complete_desktop_bounds() {
        let desktop = DesktopBounds {
            left: -1920,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let strut = StrutPartial {
            left: 24,
            ..Default::default()
        };

        assert_eq!(
            strut_reservation_for_monitor(strut, true, Rect::new(-1920, 0, 1920, 1080), desktop,),
            (0, 0, 24, 0)
        );
        assert_eq!(
            strut_reservation_for_monitor(strut, false, Rect::new(0, 0, 1920, 1080), desktop,),
            (0, 0, 0, 0)
        );
    }

    #[test]
    fn untrusted_opposing_struts_leave_a_valid_workarea() {
        assert_eq!(clamp_opposing_edges(i32::MAX, i32::MAX, 1920), (1919, 0));
        assert_eq!(clamp_opposing_edges(20, 30, 1), (0, 0));
        assert_eq!(clamp_opposing_edges(20, 30, -10), (0, 0));
    }
}
