pub trait PresentController: Send {
    fn is_available(&self) -> bool;
    fn get_event_base(&self) -> u8;
    fn register_window(&mut self, x11_win: u32) -> Result<(), String>;
    fn unregister_window(&mut self, x11_win: u32);
    fn present_pixmap(
        &self,
        x11_win: u32,
        pixmap: u32,
        target_msc: u64,
        serial: u32,
    ) -> Result<(), String>;
    fn notify_msc(&self, x11_win: u32, serial: u32, target_msc: u64) -> Result<(), String>;
    fn window_count(&self) -> usize;
    fn is_window_registered(&self, x11_win: u32) -> bool;
}
