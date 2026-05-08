// Window constraints: size hints, boundary constraints, and geometry validation

use crate::backend::api::Backend;
use crate::config::CONFIG;
use crate::core::models::{ClientKey, MonitorGeometry, SizeHints};
use crate::jwm::geometry::GeometryConstraints;
use crate::Jwm;

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

    pub(crate) fn constrain_to_screen(&self, x: &mut i32, y: &mut i32, total_width: i32, total_height: i32) {
        GeometryConstraints::constrain_to_screen(x, y, total_width, total_height, self.s_w, self.s_h);
    }

    pub(crate) fn constrain_to_monitor(
        &self,
        x: &mut i32,
        y: &mut i32,
        total_width: i32,
        total_height: i32,
        monitor_geometry: &MonitorGeometry,
    ) {
        GeometryConstraints::constrain_to_monitor(x, y, total_width, total_height, monitor_geometry);
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

    pub(crate) fn calculate_constrained_size(&self, w: i32, h: i32, hints: &SizeHints) -> (i32, i32) {
        GeometryConstraints::calculate_constrained_size(w, h, hints)
    }

    pub(crate) fn updatesizehints(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let win = self
            .state
            .clients
            .get(client_key)
            .map(|c| c.win)
            .ok_or("Client not found")?;

        match backend.property_ops().fetch_normal_hints(win)? {
            Some(h) => {
                let c = self
                    .state
                    .clients
                    .get_mut(client_key)
                    .ok_or("Client not found")?;
                c.size_hints.base_w = h.base_w;
                c.size_hints.base_h = h.base_h;
                c.size_hints.inc_w = h.inc_w;
                c.size_hints.inc_h = h.inc_h;
                c.size_hints.max_w = h.max_w;
                c.size_hints.max_h = h.max_h;
                c.size_hints.min_w = h.min_w;
                c.size_hints.min_h = h.min_h;
                c.size_hints.min_aspect = h.min_aspect;
                c.size_hints.max_aspect = h.max_aspect;
                c.state.is_fixed =
                    (h.max_w > 0) && (h.max_h > 0) && (h.max_w == h.min_w) && (h.max_h == h.min_h);
                c.size_hints.hints_valid = true;
            }
            None => {
                if let Some(c) = self.state.clients.get_mut(client_key) {
                    c.size_hints.hints_valid = false;
                }
            }
        }
        Ok(())
    }
}
