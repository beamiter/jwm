/// Shared XComposite redirect control operations used by the compositor's
/// lifecycle and fullscreen direct-scanout paths.
pub trait X11CompositeRedirectOps {
    fn query_composite_version(&self) -> Result<(), String>;
    fn redirect_subwindows_manual(&self, root: u32) -> Result<(), String>;
    fn redirect_window_manual(&self, window: u32) -> Result<(), String>;
    fn unredirect_window_manual(&self, window: u32) -> Result<(), String>;
    fn unredirect_subwindows_manual(&self, root: u32) -> Result<(), String>;
    /// Release this client's reference to the Composite overlay for `root`.
    ///
    /// The XComposite request takes the root drawable used by
    /// `GetOverlayWindow`, not the overlay window returned by that request.
    fn release_overlay_window(&self, root: u32) -> Result<(), String>;
}
