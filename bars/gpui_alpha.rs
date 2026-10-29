//! Shared startup capability check for the two GPUI frontends.
//! The owned side connection outlives the transient wgpu instance and surface.
use std::sync::Arc;
use x11rb::connection::Connection as _;
use x11rb::xcb_ffi::XCBConnection;

// -------- Compositor coupling ------------------------------------------------

/// The startup mode decision: translucent only when a compositing manager is
/// running AND gpui's renderer can actually deliver per-pixel alpha; anything
/// less paints the solid fallback. Both checks ride one side connection that
/// is gone before gpui opens its own.
pub fn detect_translucency() -> bool {
    if x11rb::xcb_ffi::load_libxcb().is_err() {
        return false;
    }
    // No X server to ask (a native-Wayland session, say) means nobody honors
    // the CM-selection contract either way: solid.
    let Ok((conn, screen_num)) = XCBConnection::connect(None) else {
        return false;
    };
    let conn = Arc::new(conn);
    let translucent =
        compositor_active(&conn, screen_num) && renderer_alpha_capable(&conn, screen_num);
    log::info!("GPUI startup mode: translucent={translucent}");
    translucent
}

/// True when a compositing manager owns the conventional `_NET_WM_CM_Sn`
/// selection.
///
/// Sampled once, before gpui runs: per-pixel alpha is a window-creation
/// decision, so a compositor started or stopped later goes unnoticed until
/// the bar restarts. Ownership also only promises compositing — whether the
/// compositor blurs behind the bar is its own affair.
fn compositor_active(conn: &XCBConnection, screen_num: usize) -> bool {
    use x11rb::protocol::xproto::ConnectionExt as _;

    let name = format!("_NET_WM_CM_S{screen_num}");
    let Ok(cookie) = conn.intern_atom(false, name.as_bytes()) else {
        return false;
    };
    let Ok(atom) = cookie.reply() else {
        return false;
    };
    conn.get_selection_owner(atom.atom)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|reply| reply.owner != x11rb::NONE)
        .unwrap_or(false)
}

/// Match GPUI's 8-bit RGB masks with a remaining alpha byte.
fn find_argb_visual(screen: &x11rb::protocol::xproto::Screen) -> Option<u32> {
    use x11rb::protocol::xproto::VisualClass;

    screen
        .allowed_depths
        .iter()
        .find(|depth| depth.depth == 32)
        .and_then(|depth| {
            depth.visuals.iter().find(|visual| {
                visual.class == VisualClass::TRUE_COLOR
                    && (visual.red_mask, visual.green_mask, visual.blue_mask)
                        == (0xFF0000, 0xFF00, 0xFF)
            })
        })
        .map(|visual| visual.visual_id)
}

/// Whether gpui's wgpu renderer could actually hand per-pixel alpha to the
/// server.
///
/// gpui negotiates its surface alpha mode internally and never reports the
/// outcome, so this asks wgpu the same question ahead of time: an unmapped
/// 1x1 ARGB window, a surface over it, and the adapter's capabilities, on the
/// same backends gpui itself enables (`gpui_wgpu::WgpuContext::instance`).
/// GLES reports `Opaque` for an EGL window even when its verified native
/// visual retains alpha. GPUI still clears transparent and writes alpha in
/// that path, so this exception is specific to GLES plus a real ARGB window.
/// Other backends must advertise GPUI's supported transparent modes.
fn x11_alpha_supported(
    native_argb: bool,
    backend: wgpu::Backend,
    modes: &[wgpu::CompositeAlphaMode],
) -> bool {
    native_argb
        && modes.iter().any(|mode| {
            matches!(
                mode,
                wgpu::CompositeAlphaMode::PreMultiplied | wgpu::CompositeAlphaMode::Inherit
            ) || (backend == wgpu::Backend::Gl && *mode == wgpu::CompositeAlphaMode::Opaque)
        })
}

fn renderer_alpha_capable(conn: &Arc<XCBConnection>, screen_num: usize) -> bool {
    use x11rb::protocol::xproto::{
        ColormapAlloc, ConnectionExt as _, CreateWindowAux, WindowClass,
    };

    let screen = &conn.setup().roots[screen_num];
    let Some(visual_id) = find_argb_visual(screen) else {
        return false;
    };
    let (Ok(colormap), Ok(window)) = (conn.generate_id(), conn.generate_id()) else {
        return false;
    };
    if !conn
        .create_colormap(ColormapAlloc::NONE, colormap, screen.root, visual_id)
        .map(|cookie| cookie.check().is_ok())
        .unwrap_or(false)
    {
        return false;
    }
    let created = conn
        .create_window(
            32,
            window,
            screen.root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            visual_id,
            &CreateWindowAux::new()
                .background_pixel(0)
                .border_pixel(0)
                .colormap(colormap)
                .override_redirect(1),
        )
        .map(|cookie| cookie.check().is_ok())
        .unwrap_or(false);

    // Check the server's accepted depth, not just our requested visual.
    let native_argb = created
        && conn
            .get_geometry(window)
            .ok()
            .and_then(|reply| reply.reply().ok())
            .is_some_and(|geometry| geometry.depth == 32);
    let capable = native_argb && {
        let target = Arc::new(ProbeTarget {
            conn: Arc::clone(conn),
            screen: screen_num as i32,
            window,
            visual_id,
        });
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
            ..wgpu::InstanceDescriptor::new_with_display_handle(Box::new(target.clone()))
        });
        instance
            .create_surface(target)
            .ok()
            .and_then(|surface| {
                let adapter =
                    pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                        power_preference: wgpu::PowerPreference::LowPower,
                        force_fallback_adapter: false,
                        compatible_surface: Some(&surface),
                        ..Default::default()
                    }))
                    .ok()?;
                let modes = surface.get_capabilities(&adapter).alpha_modes;
                Some(x11_alpha_supported(
                    native_argb,
                    adapter.get_info().backend,
                    &modes,
                ))
            })
            .unwrap_or(false)
    };

    if created {
        let _ = conn.destroy_window(window);
    }
    let _ = conn.free_colormap(colormap);
    let _ = conn.flush();
    capable
}

/// Raw-handle wrapper the capability probe hands to wgpu: the side XCB
/// connection plus the throwaway ARGB window created on it.
#[derive(Debug)]
struct ProbeTarget {
    conn: Arc<XCBConnection>,
    screen: i32,
    window: u32,
    visual_id: u32,
}

impl raw_window_handle::HasDisplayHandle for ProbeTarget {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        let handle = raw_window_handle::XcbDisplayHandle::new(
            std::ptr::NonNull::new(self.conn.get_raw_xcb_connection()),
            self.screen,
        );
        Ok(unsafe {
            raw_window_handle::DisplayHandle::borrow_raw(raw_window_handle::RawDisplayHandle::Xcb(
                handle,
            ))
        })
    }
}

impl raw_window_handle::HasWindowHandle for ProbeTarget {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        let mut handle = raw_window_handle::XcbWindowHandle::new(
            std::num::NonZeroU32::new(self.window).expect("X11 resource ids are never zero"),
        );
        handle.visual_id = std::num::NonZeroU32::new(self.visual_id);
        Ok(unsafe {
            raw_window_handle::WindowHandle::borrow_raw(raw_window_handle::RawWindowHandle::Xcb(
                handle,
            ))
        })
    }
}

#[cfg(test)]
mod alpha_tests {
    use super::x11_alpha_supported;
    use wgpu::{Backend, CompositeAlphaMode as Alpha};

    #[test]
    fn gles_opaque_requires_verified_argb() {
        assert!(x11_alpha_supported(true, Backend::Gl, &[Alpha::Opaque]));
        assert!(!x11_alpha_supported(false, Backend::Gl, &[Alpha::Opaque]));
        assert!(!x11_alpha_supported(
            true,
            Backend::Vulkan,
            &[Alpha::Opaque]
        ));
    }

    #[test]
    fn native_modes_and_failure_fallback_remain_conservative() {
        assert!(x11_alpha_supported(
            true,
            Backend::Vulkan,
            &[Alpha::PreMultiplied]
        ));
        assert!(x11_alpha_supported(
            true,
            Backend::Vulkan,
            &[Alpha::Inherit]
        ));
        assert!(!x11_alpha_supported(
            false,
            Backend::Gl,
            &[Alpha::PreMultiplied]
        ));
        assert!(!x11_alpha_supported(true, Backend::Gl, &[]));
    }
}
