/// Minimal X11 connection capabilities required by the shared compositor core.
pub trait X11ConnectionOps {
    /// Stable transport name (`x11rb` / `xcb`) used to tag `BackendError`
    /// contexts produced inside the shared compositor.
    fn backend_name(&self) -> &'static str;
    fn generate_xid(&self) -> Result<u32, String>;
    fn flush_x11(&self) -> Result<(), String>;
    /// Current `(width, height)` of a window, or `None` when the query fails
    /// (e.g. the window is already gone). Used to resize the compositor
    /// viewport to the root window after screen-layout changes.
    fn query_window_size(&self, window: u32) -> Option<(u32, u32)>;
    /// Root-relative pointer position, or `None` when the query fails. The
    /// WM only receives motion events during grabs or over the root window,
    /// so interactive layers (WaterLily stylus) poll this per frame instead.
    fn query_pointer_root(&self, root: u32) -> Option<(i16, i16)>;
    /// Replace any root-window wallpaper pixmap with the screen's solid black
    /// pixel and repaint it. This is used before handing drawing back to the X
    /// server when the compositor is disabled.
    fn paint_root_solid_black(&self, root: u32) -> Result<(), String>;
}
