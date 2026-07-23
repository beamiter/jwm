/// Minimal X11 connection capabilities required by the shared compositor core.
pub trait X11ConnectionOps {
    /// Stable transport name (`x11rb` / `xcb`) used to tag `BackendError`
    /// contexts produced inside the shared compositor.
    fn backend_name(&self) -> &'static str;
    fn generate_xid(&self) -> Result<u32, String>;
    fn flush_x11(&self) -> Result<(), String>;
}
