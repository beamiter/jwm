//! The X11 facts eframe does not surface.
//!
//! Three of them decide how this bar looks: whether a compositing manager is
//! running, whether a wgpu surface here can actually composite alpha — the
//! two together are what let the bar hand its per-pixel alpha over and be
//! blurred behind, instead of painting an opaque background — and the EWMH
//! properties that make a window manager treat the window as a dock rather
//! than as one more client to tile.

use anyhow::{Result, anyhow};
use log::debug;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt as _, PropMode, Window};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use xbar_core::{DockProperty, DockPropertyValue, DockWindowSpec, MonitorGeometry};

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

    /// The actual eframe window must have an alpha-carrying native visual,
    /// even after the startup probe found a compatible scratch surface.
    pub fn window_is_argb(&self, window: Window) -> bool {
        self.conn
            .get_geometry(window)
            .ok()
            .and_then(|reply| reply.reply().ok())
            .is_some_and(|geometry| geometry.depth == 32)
    }

    /// Root dimensions may change after hotplug; setup() is only a startup
    /// snapshot and would make bottom/right reservations stale.
    pub fn screen_geometry(&self) -> Result<MonitorGeometry> {
        let root = self.conn.setup().roots[self.screen].root;
        let geometry = self.conn.get_geometry(root)?.reply()?;
        Ok(MonitorGeometry {
            x: 0,
            y: 0,
            width: u32::from(geometry.width),
            height: u32::from(geometry.height),
        })
    }

    /// True when a compositing manager owns the conventional `_NET_WM_CM_Sn`
    /// selection.
    ///
    /// With one, the bar asks for a translucent (depth-32) window and paints
    /// real alpha, and the compositor blends the desktop behind it — the same
    /// bargain `xcb_bar` makes. Without one nothing would show through a
    /// translucent window, so the bar goes opaque instead. Sampled once at
    /// startup, because transparency is a window-creation decision: a
    /// compositor toggled later is not noticed until the bar restarts. And
    /// the selection only promises compositing — whether anything is blurred
    /// behind the bar remains the compositor's choice.
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
    pub fn write_dock_properties(&self, window: Window, properties: &[DockProperty]) -> Result<()> {
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
                        .change_property8(PropMode::REPLACE, window, name, utf8, text.as_bytes())?
                        .check()?;
                }
            }
        }
        self.conn.flush()?;
        Ok(())
    }

    /// JWM owns reservations for managed bars, including padding and the
    /// policy for interior output edges; frontends must not overwrite them.
    pub fn apply_dock_spec(
        &self,
        window: Window,
        spec: &DockWindowSpec,
        managed: bool,
    ) -> Result<()> {
        let properties: Vec<_> = spec
            .properties()
            .into_iter()
            .filter(|property| !managed || !property.name.starts_with("_NET_WM_STRUT"))
            .collect();
        self.write_dock_properties(window, &properties)
    }

    /// Update only the struts, for when the bar has been moved or resized.
    pub fn apply_struts(&self, window: Window, spec: &DockWindowSpec) -> Result<()> {
        self.write_dock_properties(window, &spec.strut_properties())
    }
}

/// Whether a verified X11 ARGB surface can retain alpha.
///
/// GLES advertises `Opaque` even for an EGL window on an ARGB visual. This
/// does not erase the native alpha channel. Keep that exception specific to
/// GLES and a verified native visual; other backends follow their WSI modes.
fn x11_alpha_supported(
    native_argb: bool,
    backend: wgpu::Backend,
    modes: &[wgpu::CompositeAlphaMode],
) -> bool {
    native_argb
        && modes.iter().any(|mode| {
            matches!(
                mode,
                wgpu::CompositeAlphaMode::PreMultiplied
                    | wgpu::CompositeAlphaMode::PostMultiplied
                    | wgpu::CompositeAlphaMode::Inherit
            ) || (backend == wgpu::Backend::Gl && *mode == wgpu::CompositeAlphaMode::Opaque)
        })
}

/// Whether a wgpu surface on this display can composite alpha.
///
/// egui-wgpu negotiates the surface's `CompositeAlphaMode` deep inside eframe
/// and never reports the outcome, so the bar asks the same question ahead of
/// time: a scratch depth-32 window, a surface on it, and the adapter's
/// advertised alpha modes. The native visual remains authoritative for GLES,
/// whose `Opaque` advertisement does not describe its framebuffer alpha.
/// Any failure along the way reads as "no alpha" and keeps the solid fallback.
pub fn surface_alpha_capable() -> bool {
    use std::num::NonZeroU32;
    use std::ptr::NonNull;
    use std::sync::Arc;

    use raw_window_handle::{
        DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
        RawWindowHandle, WindowHandle, XcbDisplayHandle, XcbWindowHandle,
    };
    use x11rb::protocol::xproto::{ColormapAlloc, CreateWindowAux, VisualClass, WindowClass};
    use x11rb::xcb_ffi::XCBConnection;

    /// The scratch window, shaped the way wgpu's safe surface API wants it.
    #[derive(Debug, Clone)]
    struct ProbeTarget {
        conn: Arc<XCBConnection>,
        screen: i32,
        window: u32,
        visual: u32,
    }
    impl HasDisplayHandle for ProbeTarget {
        fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
            let connection =
                NonNull::new(self.conn.get_raw_xcb_connection()).ok_or(HandleError::Unavailable)?;
            let handle = XcbDisplayHandle::new(Some(connection), self.screen);
            Ok(unsafe { DisplayHandle::borrow_raw(RawDisplayHandle::Xcb(handle)) })
        }
    }

    impl HasWindowHandle for ProbeTarget {
        fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
            let Some(window) = NonZeroU32::new(self.window) else {
                return Err(HandleError::Unavailable);
            };
            let mut handle = XcbWindowHandle::new(window);
            handle.visual_id = NonZeroU32::new(self.visual);
            Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::Xcb(handle)) })
        }
    }

    // libxcb is loaded lazily and a missing library would panic mid-connect;
    // forcing the load first turns that into a plain "not capable".
    if x11rb::xcb_ffi::load_libxcb().is_err() {
        return false;
    }
    let Ok((conn, screen_num)) = XCBConnection::connect(None) else {
        return false;
    };
    let conn = Arc::new(conn);
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;
    // The same visual winit will pick for the real window once transparency
    // is requested: depth-32 TrueColor.
    let Some(visual) = screen
        .allowed_depths
        .iter()
        .find(|depth| depth.depth == 32)
        .and_then(|depth| {
            depth
                .visuals
                .iter()
                .find(|visual| visual.class == VisualClass::TRUE_COLOR)
        })
        .map(|visual| visual.visual_id)
    else {
        return false;
    };

    let (Ok(colormap), Ok(window)) = (conn.generate_id(), conn.generate_id()) else {
        return false;
    };
    // A 1x1 never-mapped stand-in for the bar window; wgpu only needs
    // something a surface can attach to.
    let created = conn
        .create_colormap(ColormapAlloc::NONE, colormap, root, visual)
        .is_ok()
        && conn
            .create_window(
                32,
                window,
                root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_OUTPUT,
                visual,
                &CreateWindowAux::new()
                    .border_pixel(0)
                    .background_pixel(0)
                    .override_redirect(1)
                    .colormap(colormap),
            )
            .is_ok()
        && conn.flush().is_ok();

    // Confirm the server accepted the ARGB window before trusting its alpha.
    let native_argb = created
        && conn
            .get_geometry(window)
            .ok()
            .and_then(|reply| reply.reply().ok())
            .is_some_and(|geometry| geometry.depth == 32);
    // The descriptor and surface each own a connection reference, and both
    // die before the scratch native window is destroyed below.
    let capable = created
        && native_argb
        && (|| {
            let target = ProbeTarget {
                conn: Arc::clone(&conn),
                screen: screen_num as i32,
                window,
                visual,
            };
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle(
                Box::new(target.clone()),
            ));
            let Ok(surface) = instance.create_surface(target) else {
                return false;
            };
            let adapter =
                pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::LowPower,
                    compatible_surface: Some(&surface),
                    ..Default::default()
                }));
            let Ok(adapter) = adapter else {
                return false;
            };
            x11_alpha_supported(
                native_argb,
                adapter.get_info().backend,
                &surface.get_capabilities(&adapter).alpha_modes,
            )
        })();

    let _ = conn.destroy_window(window);
    let _ = conn.free_colormap(colormap);
    let _ = conn.flush();
    capable
}

#[cfg(test)]
mod alpha_tests {
    use super::x11_alpha_supported;
    use wgpu::{Backend, CompositeAlphaMode as Alpha};

    #[test]
    fn gles_opaque_advertisement_does_not_remove_verified_native_argb() {
        let modes = [Alpha::Opaque];
        assert!(x11_alpha_supported(true, Backend::Gl, &modes));
        assert_eq!(modes, [Alpha::Opaque]);
        assert!(!x11_alpha_supported(false, Backend::Gl, &modes));
        assert!(!x11_alpha_supported(true, Backend::Vulkan, &modes));
    }

    #[test]
    fn advertised_alpha_still_requires_the_native_argb_visual() {
        for mode in [Alpha::PreMultiplied, Alpha::PostMultiplied, Alpha::Inherit] {
            assert!(x11_alpha_supported(true, Backend::Gl, &[mode]));
            assert!(x11_alpha_supported(true, Backend::Vulkan, &[mode]));
            assert!(!x11_alpha_supported(false, Backend::Gl, &[mode]));
        }
        assert!(!x11_alpha_supported(true, Backend::Gl, &[]));
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
