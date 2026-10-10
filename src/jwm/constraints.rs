// Window constraints: size hints, boundary constraints, and geometry validation

use crate::Jwm;
use crate::backend::api::{Backend, NormalHints};
use crate::config::CONFIG;
use crate::core::models::{ClientKey, MonitorGeometry, SizeHints, WMClient};
use crate::jwm::geometry::GeometryConstraints;

fn refresh_client_size_hints<E>(
    client: &mut WMClient,
    fetch: impl FnOnce(crate::backend::common_define::WindowId) -> Result<Option<NormalHints>, E>,
) -> Result<(), E> {
    if client.size_hints.hints_valid {
        return Ok(());
    }

    let win = client.win;
    match fetch(win)? {
        Some(hints) => {
            client.size_hints.base_w = hints.base_w;
            client.size_hints.base_h = hints.base_h;
            client.size_hints.inc_w = hints.inc_w;
            client.size_hints.inc_h = hints.inc_h;
            client.size_hints.max_w = hints.max_w;
            client.size_hints.max_h = hints.max_h;
            client.size_hints.min_w = hints.min_w;
            client.size_hints.min_h = hints.min_h;
            client.size_hints.min_aspect = hints.min_aspect;
            client.size_hints.max_aspect = hints.max_aspect;
            client.state.is_fixed = (hints.max_w > 0)
                && (hints.max_h > 0)
                && (hints.max_w == hints.min_w)
                && (hints.max_h == hints.min_h);
            client.size_hints.hints_valid = true;
            if hints.max_w > 0 || hints.max_h > 0 {
                // A capped client cannot fill a tile, so record the caps:
                // this is what tells us afterwards whether such a window
                // was floated (min==max) or merely clamped inside a slot.
                log::info!(
                    "[updatesizehints] {win:?} min={}x{} max={}x{} is_fixed={}",
                    hints.min_w,
                    hints.min_h,
                    hints.max_w,
                    hints.max_h,
                    client.state.is_fixed,
                );
            }
        }
        None => {
            // Absence is a valid cached answer. Keeping this invalid would
            // issue one synchronous X11 GetProperty round-trip per client on
            // every arrange; keeping the old values would also continue to
            // constrain a client after it deleted WM_NORMAL_HINTS.
            client.size_hints = SizeHints {
                hints_valid: true,
                ..SizeHints::default()
            };
            client.state.is_fixed = false;
        }
    }
    Ok(())
}

impl Jwm {
    pub(crate) fn applysizehints(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        x: &mut i32,
        y: &mut i32,
        w: &mut i32,
        h: &mut i32,
        interact: bool,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        *w = (*w).max(1);
        *h = (*h).max(1);
        let original_geometry = if let Some(client) = self.state.clients.get(client_key) {
            (
                client.geometry.x,
                client.geometry.y,
                client.geometry.w,
                client.geometry.h,
            )
        } else {
            return Err("Client not found".into());
        };
        self.apply_boundary_constraints(client_key, x, y, w, h, interact)?;
        let geometry_changed = self.apply_size_hints_constraints(backend, client_key, w, h)?;
        if geometry_changed {
            // The first clamp kept a pixel of the requested size visible.
            // Hints can shrink that size, moving its far edge wholly outside
            // the output. Recheck with the dimensions we will configure.
            self.apply_boundary_constraints(client_key, x, y, w, h, interact)?;
        }
        Ok(geometry_changed
            || *x != original_geometry.0
            || *y != original_geometry.1
            || *w != original_geometry.2
            || *h != original_geometry.3)
    }

    pub(crate) fn apply_boundary_constraints(
        &self,
        client_key: ClientKey,
        x: &mut i32,
        y: &mut i32,
        w: &i32,
        h: &i32,
        interact: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (client_total_width, client_total_height, mon_key) =
            if let Some(client) = self.state.clients.get(client_key) {
                (
                    i64::from(*w) + 2 * i64::from(client.geometry.border_w),
                    i64::from(*h) + 2 * i64::from(client.geometry.border_w),
                    client.mon,
                )
            } else {
                return Err("Client not found".into());
            };

        if interact {
            self.constrain_to_screen(x, y, client_total_width, client_total_height);
        } else {
            if let Some(mon_key) = mon_key {
                if let Some(monitor) = self.state.monitors.get(mon_key) {
                    self.constrain_to_monitor(
                        x,
                        y,
                        client_total_width,
                        client_total_height,
                        &monitor.geometry,
                    );
                }
            }
        }

        Ok(())
    }

    pub(crate) fn constrain_to_screen(
        &self,
        x: &mut i32,
        y: &mut i32,
        total_width: i64,
        total_height: i64,
    ) {
        GeometryConstraints::constrain_to_screen_wide(
            x,
            y,
            total_width,
            total_height,
            self.s_w,
            self.s_h,
        );
    }

    pub(crate) fn constrain_to_monitor(
        &self,
        x: &mut i32,
        y: &mut i32,
        total_width: i64,
        total_height: i64,
        monitor_geometry: &MonitorGeometry,
    ) {
        GeometryConstraints::constrain_to_monitor_wide(
            x,
            y,
            total_width,
            total_height,
            monitor_geometry,
        );
    }

    pub(crate) fn apply_size_hints_constraints(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        w: &mut i32,
        h: &mut i32,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let is_floating = self
            .state
            .clients
            .get(client_key)
            .map(|client| client.state.is_floating)
            .unwrap_or(false);

        if !CONFIG.load().behavior().resize_hints && !is_floating {
            return Ok(false);
        }

        self.ensure_size_hints_valid(backend, client_key)?;

        let hints = if let Some(client) = self.state.clients.get(client_key) {
            client.size_hints.clone()
        } else {
            return Err("Client not found".into());
        };

        let (new_w, new_h) = self.calculate_constrained_size(*w, *h, &hints);
        let changed = *w != new_w || *h != new_h;
        *w = new_w;
        *h = new_h;

        Ok(changed)
    }

    pub(crate) fn ensure_size_hints_valid(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let hints_valid = self
            .state
            .clients
            .get(client_key)
            .map(|client| client.size_hints.hints_valid)
            .unwrap_or(false);
        if !hints_valid {
            self.updatesizehints(backend, client_key)?;
        }

        Ok(())
    }

    pub(crate) fn calculate_constrained_size(
        &self,
        w: i32,
        h: i32,
        hints: &SizeHints,
    ) -> (i32, i32) {
        GeometryConstraints::calculate_constrained_size(w, h, hints)
    }

    pub(crate) fn updatesizehints(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (previous_fixed, monitor) = self
            .state
            .clients
            .get(client_key)
            .map(|client| (client.state.is_fixed, client.mon))
            .ok_or("Client not found")?;
        let client = self
            .state
            .clients
            .get_mut(client_key)
            .ok_or("Client not found")?;
        refresh_client_size_hints(client, |win| backend.property_ops().fetch_normal_hints(win))?;
        let fixed = self
            .state
            .clients
            .get(client_key)
            .map(|client| client.state.is_fixed)
            .unwrap_or(previous_fixed);
        if previous_fixed != fixed {
            if let Some(mk) = monitor {
                self.broadcast_monitor_bar_ipc(backend, mk);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::common_define::WindowId;
    use std::cell::Cell;

    #[test]
    fn missing_hints_are_cached_until_a_property_event_invalidates_them() {
        let mut client = WMClient::new(WindowId::from_raw(42));
        // Model a property that used to describe a fixed-size client and was
        // then deleted. The first None must clear every stale constraint.
        client.size_hints.min_w = 640;
        client.size_hints.min_h = 480;
        client.size_hints.max_w = 640;
        client.size_hints.max_h = 480;
        client.state.is_fixed = true;

        let fetches = Cell::new(0_u32);
        for _ in 0..100 {
            refresh_client_size_hints(&mut client, |_| {
                fetches.set(fetches.get() + 1);
                Ok::<_, ()>(None)
            })
            .unwrap();
        }
        assert_eq!(fetches.get(), 1, "repeated arrange validation re-fetched");
        assert_eq!(
            client.size_hints,
            SizeHints {
                hints_valid: true,
                ..SizeHints::default()
            }
        );
        assert!(!client.state.is_fixed);

        // `handle_normal_hints_change` performs this invalidation for both a
        // replacement and a deletion. Exactly one subsequent validation must
        // query again, then cache the second absent answer too.
        client.size_hints.hints_valid = false;
        for _ in 0..100 {
            refresh_client_size_hints(&mut client, |_| {
                fetches.set(fetches.get() + 1);
                Ok::<_, ()>(None)
            })
            .unwrap();
        }
        assert_eq!(
            fetches.get(),
            2,
            "property invalidation did not re-fetch once"
        );
        assert!(client.size_hints.hints_valid);
    }

    fn apply_cached_hints(
        hints: SizeHints,
        origin: (i32, i32),
        border: i32,
        input: (i32, i32, i32, i32),
        interact: bool,
    ) -> ((i32, i32, i32, i32), bool) {
        use crate::jwm::features::monitor_lock::test_support::LockSpyBackend;

        // Existing CPU-only backend; the test build's constructor explicitly
        // uses ControlSocketSource::Detached and never binds a session socket.
        let mut backend = LockSpyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        jwm.s_w = 200;
        jwm.s_h = 100;
        let monitor = jwm.state.sel_mon.unwrap();
        let geometry = &mut jwm.state.monitors[monitor].geometry;
        geometry.w_x = origin.0;
        geometry.w_y = origin.1;
        geometry.w_w = 200;
        geometry.w_h = 100;
        let mut client = WMClient::new(WindowId::from_raw(42));
        client.mon = Some(monitor);
        client.state.is_floating = true;
        client.size_hints = SizeHints {
            hints_valid: true,
            ..hints
        };
        client.geometry.x = input.0;
        client.geometry.y = input.1;
        client.geometry.w = input.2;
        client.geometry.h = input.3;
        client.geometry.border_w = border;
        let key = jwm.insert_client(client);
        let (mut x, mut y, mut w, mut h) = input;
        let changed = jwm
            .applysizehints(&mut backend, key, &mut x, &mut y, &mut w, &mut h, interact)
            .unwrap();
        ((x, y, w, h), changed)
    }

    #[test]
    fn shrinking_size_hints_keep_a_pixel_on_screen() {
        let hints = SizeHints {
            max_w: 10,
            max_h: 10,
            ..SizeHints::default()
        };
        let input = (-90, -90, 100, 100);
        let (rect, changed) = apply_cached_hints(hints, (0, 0), 0, input, true);
        assert_eq!(rect, (-9, -9, 10, 10));
        assert!(changed);
        assert!(rect.0 + rect.2 > 0 && rect.1 + rect.3 > 0);
    }

    #[test]
    fn final_hint_boundary_includes_borders_and_negative_monitor_origins() {
        let hints = SizeHints {
            max_w: 10,
            max_h: 10,
            ..SizeHints::default()
        };
        let origin = (-1920, -1080);
        let input = (-2010, -1170, 100, 100);
        let (rect, _) = apply_cached_hints(hints, origin, 2, input, false);
        assert_eq!(rect, (-1933, -1093, 10, 10));
        assert_eq!(rect.0 + rect.2 + 4, -1919);
        assert_eq!(rect.1 + rect.3 + 4, -1079);
    }

    #[test]
    fn aspect_and_increment_shrink_recheck_the_visible_edge() {
        for (hints, expected) in [
            (
                SizeHints {
                    inc_w: 16,
                    inc_h: 16,
                    ..SizeHints::default()
                },
                (-95, -95, 96, 96),
            ),
            (
                SizeHints {
                    min_aspect: 0.5,
                    max_aspect: 0.5,
                    ..SizeHints::default()
                },
                (-49, -99, 50, 100),
            ),
        ] {
            let input = (-99, -99, 100, 100);
            let (rect, _) = apply_cached_hints(hints, (0, 0), 0, input, true);
            assert_eq!(rect, expected);
        }
    }

    #[test]
    fn unchanged_and_growing_hints_preserve_existing_positioning() {
        let input = (20, 30, 80, 40);
        assert_eq!(
            apply_cached_hints(SizeHints::default(), (0, 0), 3, input, true),
            (input, false)
        );
        let hints = SizeHints {
            min_w: 100,
            min_h: 100,
            ..SizeHints::default()
        };
        let input = (-9, -9, 10, 10);
        let (rect, changed) = apply_cached_hints(hints.clone(), (0, 0), 0, input, true);
        assert_eq!(rect, (-9, -9, 100, 100));
        assert!(changed);
        assert_eq!(
            apply_cached_hints(hints, (0, 0), 0, rect, true),
            (rect, false)
        );
    }

    #[test]
    fn shrinking_hints_do_not_displace_right_or_bottom_edge_windows() {
        let hints = SizeHints {
            max_w: 10,
            max_h: 10,
            ..SizeHints::default()
        };
        let input = (199, 99, 100, 100);
        let (rect, _) = apply_cached_hints(hints, (0, 0), 0, input, true);
        assert_eq!(rect, (199, 99, 10, 10));
    }
}
