#[derive(Debug, Clone, Copy)]
pub struct BootstrapState {
    pub damage_event_base: u8,
    pub overlay_window: u32,
}

pub trait X11BootstrapOps {
    fn query_damage_event_base(&self) -> Result<u8, String>;
    fn get_overlay_window(&self, root: u32) -> Result<u32, String>;
    fn set_overlay_input_passthrough(&self, overlay_window: u32) -> Result<(), String>;
    fn set_overlay_window_type_notification(&self, overlay_window: u32) -> Result<(), String>;
    fn claim_compositor_selection_owner(&self, root: u32, screen_num: i32) -> Result<u32, String>;

    fn bootstrap_state(&self, root: u32) -> Result<BootstrapState, String> {
        let damage_event_base = self.query_damage_event_base()?;
        let overlay_window = self.get_overlay_window(root)?;
        self.set_overlay_input_passthrough(overlay_window)?;
        self.set_overlay_window_type_notification(overlay_window)?;
        Ok(BootstrapState {
            damage_event_base,
            overlay_window,
        })
    }
}
