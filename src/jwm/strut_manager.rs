use log::info;
use crate::backend::api::Backend;
use crate::backend::common_define::WindowId;
use crate::core::models::MonitorKey;
use crate::jwm::Jwm;

impl Jwm {
    pub fn get_strut_reserved(&self, mon_key: MonitorKey) -> (i32, i32, i32, i32) {
        let monitor = match self.state.monitors.get(mon_key) {
            Some(m) => m,
            None => return (0, 0, 0, 0),
        };
        let mx = monitor.geometry.m_x;
        let my = monitor.geometry.m_y;
        let mw = monitor.geometry.m_w;
        let mh = monitor.geometry.m_h;
        let mx_end = mx + mw;
        let my_end = my + mh;

        let mut top = 0i32;
        let mut bottom = 0i32;
        let mut left = 0i32;
        let mut right = 0i32;

        for strut in self.external_struts.values() {
            if strut.top > 0 {
                let sx = strut.top_start_x as i32;
                let ex = strut.top_end_x as i32;
                if (sx == 0 && ex == 0) || (sx < mx_end && ex >= mx) {
                    top = top.max(strut.top as i32 - my);
                }
            }
            if strut.bottom > 0 {
                let sx = strut.bottom_start_x as i32;
                let ex = strut.bottom_end_x as i32;
                if (sx == 0 && ex == 0) || (sx < mx_end && ex >= mx) {
                    bottom = bottom.max(strut.bottom as i32 - (my_end - mh).max(0));
                }
            }
            if strut.left > 0 {
                let sy = strut.left_start_y as i32;
                let ey = strut.left_end_y as i32;
                if (sy == 0 && ey == 0) || (sy < my_end && ey >= my) {
                    left = left.max(strut.left as i32 - mx);
                }
            }
            if strut.right > 0 {
                let sy = strut.right_start_y as i32;
                let ey = strut.right_end_y as i32;
                if (sy == 0 && ey == 0) || (sy < my_end && ey >= my) {
                    right = right.max(strut.right as i32 - (mx_end - mw).max(0));
                }
            }
        }

        (top.max(0), bottom.max(0), left.max(0), right.max(0))
    }

    pub fn apply_strut_reservations(&mut self) {
        let mon_keys: Vec<MonitorKey> = self.state.monitor_order.clone();
        for mon_key in mon_keys {
            let (strut_top, strut_bottom, strut_left, strut_right) =
                self.get_strut_reserved(mon_key);
            if let Some(monitor) = self.state.monitors.get_mut(mon_key) {
                monitor.geometry.w_x = monitor.geometry.m_x + strut_left;
                monitor.geometry.w_y = monitor.geometry.m_y + strut_top;
                monitor.geometry.w_w = monitor.geometry.m_w - strut_left - strut_right;
                monitor.geometry.w_h = monitor.geometry.m_h - strut_top - strut_bottom;
            }
        }
    }

    pub fn check_strut_on_manage(&mut self, backend: &mut dyn Backend, win: WindowId) {
        if let Some(strut) = backend.property_ops().get_window_strut_partial(win) {
            if strut.left > 0 || strut.right > 0 || strut.top > 0 || strut.bottom > 0 {
                info!(
                    "[strut] New window {:?} has strut: top={} bottom={} left={} right={}",
                    win, strut.top, strut.bottom, strut.left, strut.right
                );
                self.external_struts.insert(win, strut);
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

