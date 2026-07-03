/// Shared X11 window resource operations used by compositor teardown and
/// ownership cleanup paths.
pub trait X11WindowResourceOps {
    fn destroy_window_resource(&self, window: u32) -> Result<(), String>;
}
