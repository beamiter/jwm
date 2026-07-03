/// Shared XComposite redirect control operations used by the compositor's
/// lifecycle and fullscreen direct-scanout paths.
pub trait X11CompositeRedirectOps {
    fn query_composite_version(&self) -> Result<(), String>;
    fn redirect_subwindows_manual(&self, root: u32) -> Result<(), String>;
    fn redirect_window_manual(&self, window: u32) -> Result<(), String>;
    fn unredirect_window_manual(&self, window: u32) -> Result<(), String>;
    fn unredirect_subwindows_manual(&self, root: u32) -> Result<(), String>;
    fn release_overlay_window(&self, overlay_window: u32) -> Result<(), String>;
}
