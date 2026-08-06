//! The X11 facts eframe does not surface.
//!
//! Two of them decide how this bar looks: whether a compositing manager is
//! running — which is what lets the bar hand its per-pixel alpha over and be
//! blurred behind, instead of frosting a wallpaper strip itself — and the
//! EWMH properties that make a window manager treat the window as a dock
//! rather than as one more client to tile.

use anyhow::{Result, anyhow};
use log::debug;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt as _, PropMode, Window};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use xbar_core::{DockProperty, DockPropertyValue, DockWindowSpec};

/// A second connection to the display eframe is already using.
///
/// Opening our own costs one socket and keeps every property write off
/// winit's connection, whose event loop owns it.
pub struct X11Session {
    conn: RustConnection,
    screen: usize,
}

impl X11Session {
    pub fn open() -> Result<Self> {
        let (conn, screen) = x11rb::connect(None)?;
        Ok(Self { conn, screen })
    }

    /// Physical size of the X screen, used to size the bar before the window
    /// manager has said where it goes.
    #[must_use]
    pub fn screen_size(&self) -> (u32, u32) {
        let screen = &self.conn.setup().roots[self.screen];
        (
            u32::from(screen.width_in_pixels),
            u32::from(screen.height_in_pixels),
        )
    }

    /// True when a compositing manager owns the conventional `_NET_WM_CM_Sn`
    /// selection.
    ///
    /// With one, the bar asks for a translucent (depth-32) window and paints
    /// real alpha, and the compositor blurs the desktop behind it — the same
    /// bargain `xcb_bar` makes. Without one nothing would blur behind a
    /// translucent window, so the bar bakes its own frosted strip instead.
    #[must_use]
    pub fn compositor_active(&self) -> bool {
        let selection = format!("_NET_WM_CM_S{}", self.screen);
        match self
            .intern(&selection)
            .and_then(|atom| Ok(self.conn.get_selection_owner(atom)?.reply()?.owner))
        {
            Ok(owner) => owner != x11rb::NONE,
            Err(error) => {
                debug!("could not read {selection}: {error}");
                false
            }
        }
    }

    fn intern(&self, name: &str) -> Result<Atom> {
        Ok(self.conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
    }

    /// Write the EWMH dock property set the core describes.
    ///
    /// Only interning and the property writes live here; which properties a
    /// dock needs is `xbar_core::placement`'s decision, shared with every
    /// other X11 bar.
    pub fn write_dock_properties(
        &self,
        window: Window,
        properties: &[DockProperty],
    ) -> Result<()> {
        for property in properties {
            let name = self.intern(property.name)?;
            match &property.value {
                DockPropertyValue::Atoms(values) => {
                    let atoms = values
                        .iter()
                        .map(|value| self.intern(value))
                        .collect::<Result<Vec<_>>>()?;
                    self.conn
                        .change_property32(PropMode::REPLACE, window, name, AtomEnum::ATOM, &atoms)?
                        .check()?;
                }
                DockPropertyValue::Cardinals(values) => {
                    self.conn
                        .change_property32(
                            PropMode::REPLACE,
                            window,
                            name,
                            AtomEnum::CARDINAL,
                            values,
                        )?
                        .check()?;
                }
                DockPropertyValue::Utf8Text(text) => {
                    let utf8 = self.intern("UTF8_STRING")?;
                    self.conn
                        .change_property8(
                            PropMode::REPLACE,
                            window,
                            name,
                            utf8,
                            text.as_bytes(),
                        )?
                        .check()?;
                }
            }
        }
        self.conn.flush()?;
        Ok(())
    }

    /// Announce the window as a top dock covering `placement`.
    pub fn apply_dock_spec(&self, window: Window, spec: &DockWindowSpec) -> Result<()> {
        self.write_dock_properties(window, &spec.properties())
    }

    /// Update only the struts, for when the bar has been moved or resized.
    pub fn apply_struts(&self, window: Window, spec: &DockWindowSpec) -> Result<()> {
        self.write_dock_properties(window, &spec.strut_properties())
    }
}

/// The X11 window id behind an eframe window, if this is an X11 session.
pub fn window_id(handle: &impl raw_window_handle::HasWindowHandle) -> Result<Window> {
    use raw_window_handle::RawWindowHandle;

    let handle = handle
        .window_handle()
        .map_err(|error| anyhow!("no window handle: {error}"))?;
    match handle.as_raw() {
        RawWindowHandle::Xlib(handle) => Ok(Window::try_from(handle.window)?),
        RawWindowHandle::Xcb(handle) => Ok(handle.window.get()),
        other => Err(anyhow!("not an X11 window: {other:?}")),
    }
}
