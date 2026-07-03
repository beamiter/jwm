/// Backend-neutral Present extension operations used by the shared compositor.
pub trait X11PresentOps {
    fn query_present_version(&self) -> Result<(u32, u32), String>;
    fn query_present_event_base(&self) -> Result<u8, String>;
    fn select_present_input(&self, event_id: u32, window: u32) -> Result<(), String>;
    fn present_pixmap_for_window(
        &self,
        window: u32,
        pixmap: u32,
        target_msc: u64,
        serial: u32,
    ) -> Result<(), String>;
    fn notify_present_msc(&self, window: u32, serial: u32, target_msc: u64) -> Result<(), String>;
}
