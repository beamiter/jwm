/// Backend-neutral X11 window texture source operations used by the shared
/// compositor logic when creating and refreshing TFP-backed window textures.
pub trait X11TextureSourceOps {
    fn create_window_damage(&self, damage_id: u32, window: u32) -> Result<(), String>;
    fn destroy_window_damage(&self, damage_id: u32) -> Result<(), String>;
    fn clear_window_damage(&self, damage_id: u32) -> Result<(), String>;
    fn name_window_pixmap(&self, window: u32, pixmap: u32) -> Result<(), String>;
    fn free_window_pixmap(&self, pixmap: u32) -> Result<(), String>;
    fn get_window_visual(&self, window: u32) -> Result<u32, String>;
    fn get_window_depth(&self, window: u32) -> Result<u8, String>;
}
