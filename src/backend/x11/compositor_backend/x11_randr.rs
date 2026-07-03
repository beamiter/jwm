use std::collections::HashMap;

/// Backend-neutral RandR monitor layout queries used by the shared compositor.
pub trait X11RandrOps {
    fn query_monitor_rects(&self, root: u32) -> Vec<(u32, i32, i32, u32, u32)>;
    fn query_monitor_refresh_rates(&self, root: u32) -> HashMap<u32, u32>;
}
