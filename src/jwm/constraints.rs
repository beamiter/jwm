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
                    *w + 2 * client.geometry.border_w,
                    *h + 2 * client.geometry.border_w,
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
        total_width: i32,
        total_height: i32,
    ) {
        GeometryConstraints::constrain_to_screen(
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
        total_width: i32,
        total_height: i32,
        monitor_geometry: &MonitorGeometry,
    ) {
        GeometryConstraints::constrain_to_monitor(
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
        let client = self
            .state
            .clients
            .get_mut(client_key)
            .ok_or("Client not found")?;
        refresh_client_size_hints(client, |win| backend.property_ops().fetch_normal_hints(win))?;
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
}
