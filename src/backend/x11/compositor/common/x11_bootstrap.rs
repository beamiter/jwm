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

    /// Replace the overlay window's INPUT shape with `rects`, given as
    /// root-relative `(x, y, width, height)`. An empty slice restores the
    /// fully click-through overlay [`Self::set_overlay_input_passthrough`]
    /// bootstraps.
    ///
    /// The empty INPUT region makes every press fall through to whatever
    /// client is underneath, which is right for the compositing surface and
    /// wrong for the few regions the compositor draws something clickable
    /// in: JWM selects no button events on client windows and grabs only the
    /// configured modifier combinations on the focused one, so a plain press
    /// on a toast card over the focused client reaches the client and the
    /// window manager's toast intercept never runs. Shaping the overlay to
    /// those regions puts the press on the overlay, which selects nothing
    /// and therefore propagates it to the root window the WM *does* select
    /// `ButtonPress` on.
    ///
    /// The default is a no-op returning `Ok(())`: a transport that has not
    /// implemented it keeps the click-through overlay every transport had
    /// before, rather than failing the frame that asked.
    fn set_overlay_input_shape(
        &self,
        _overlay_window: u32,
        _rects: &[(i16, i16, u16, u16)],
    ) -> Result<(), String> {
        Ok(())
    }

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
