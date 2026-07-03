/// Minimal X11 connection capabilities required by the shared compositor core.
pub trait X11ConnectionOps {
    fn generate_xid(&self) -> Result<u32, String>;
    fn flush_x11(&self) -> Result<(), String>;
}
