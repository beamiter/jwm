use crate::backend::error::BackendError;
use crate::backend::x11::compositor::Compositor;
use crate::backend::x11::compositor_common::{
    X11BootstrapOps, X11CompositeRedirectOps, X11ConnectionOps, X11PresentOps, X11RandrOps,
    X11TextureSourceOps, X11WindowResourceOps,
};
use std::sync::Arc;
use xcb::{Xid, XidNew, composite, shape, x, xfixes};

pub(crate) struct XcbSharedCompositorConnection {
    conn: Arc<xcb::Connection>,
}

pub(crate) type XcbSharedCompositor = Compositor<XcbSharedCompositorConnection>;

impl XcbSharedCompositorConnection {
    fn new(conn: Arc<xcb::Connection>) -> Self {
        Self { conn }
    }

    fn protocol(&self) -> XcbCompositorProtocol<'_> {
        XcbCompositorProtocol::new(self.conn.as_ref())
    }
}

pub(crate) struct XcbCompositorProtocol<'a> {
    conn: &'a xcb::Connection,
}

#[allow(dead_code)]
impl<'a> XcbCompositorProtocol<'a> {
    pub(crate) fn new(conn: &'a xcb::Connection) -> Self {
        Self { conn }
    }

    pub(crate) fn prime_extensions(&self) -> Result<(), BackendError> {
        let composite_cookie = self.conn.send_request(&xcb::composite::QueryVersion {
            client_major_version: 0,
            client_minor_version: 4,
        });
        let _ = self.conn.wait_for_reply(composite_cookie).map_err(|e| {
            BackendError::Message(format!("failed to query XComposite version: {e}"))
        })?;

        let damage_cookie = self.conn.send_request(&xcb::damage::QueryVersion {
            client_major_version: 1,
            client_minor_version: 1,
        });
        let _ = self
            .conn
            .wait_for_reply(damage_cookie)
            .map_err(|e| BackendError::Message(format!("failed to query XDamage version: {e}")))?;

        let present_cookie = self.conn.send_request(&xcb::present::QueryVersion {
            major_version: 1,
            minor_version: 0,
        });
        let _ = self
            .conn
            .wait_for_reply(present_cookie)
            .map_err(|e| BackendError::Message(format!("failed to query XPresent version: {e}")))?;

        let xfixes_cookie = self.conn.send_request(&xcb::xfixes::QueryVersion {
            client_major_version: 5,
            client_minor_version: 0,
        });
        let _ = self
            .conn
            .wait_for_reply(xfixes_cookie)
            .map_err(|e| BackendError::Message(format!("failed to query XFixes version: {e}")))?;

        Ok(())
    }

    fn intern_atom(&self, name: &[u8]) -> Result<x::Atom, BackendError> {
        let cookie = self.conn.send_request(&x::InternAtom {
            only_if_exists: false,
            name,
        });
        self.conn
            .wait_for_reply(cookie)
            .map(|reply| reply.atom())
            .map_err(|e| BackendError::Message(format!("failed to intern atom {:?}: {e}", name)))
    }

    pub(crate) fn redirect_subwindows(&self, root: x::Window) -> Result<(), BackendError> {
        self.conn
            .send_and_check_request(&composite::RedirectSubwindows {
                window: root,
                update: composite::Redirect::Manual,
            })
            .map_err(|e| BackendError::Message(format!("redirect_subwindows failed: {e}")))
    }

    pub(crate) fn unredirect_subwindows(&self, root: x::Window) -> Result<(), BackendError> {
        self.conn
            .send_and_check_request(&composite::UnredirectSubwindows {
                window: root,
                update: composite::Redirect::Manual,
            })
            .map_err(|e| BackendError::Message(format!("unredirect_subwindows failed: {e}")))
    }

    pub(crate) fn get_overlay_window(&self, root: x::Window) -> Result<x::Window, BackendError> {
        let cookie = self
            .conn
            .send_request(&composite::GetOverlayWindow { window: root });
        self.conn
            .wait_for_reply(cookie)
            .map(|reply| reply.overlay_win())
            .map_err(|e| BackendError::Message(format!("get_overlay_window failed: {e}")))
    }

    pub(crate) fn release_overlay_window(&self, root: x::Window) -> Result<(), BackendError> {
        self.conn
            .send_and_check_request(&composite::ReleaseOverlayWindow { window: root })
            .map_err(|e| BackendError::Message(format!("release_overlay_window failed: {e}")))
    }

    pub(crate) fn set_overlay_input_passthrough(
        &self,
        overlay: x::Window,
    ) -> Result<(), BackendError> {
        let region: xfixes::Region = self.conn.generate_id();
        self.conn
            .send_and_check_request(&xfixes::CreateRegion {
                region,
                rectangles: &[],
            })
            .map_err(|e| BackendError::Message(format!("xfixes create_region failed: {e}")))?;
        self.conn
            .send_and_check_request(&xfixes::SetWindowShapeRegion {
                dest: overlay,
                dest_kind: shape::Sk::Input,
                x_offset: 0,
                y_offset: 0,
                region,
            })
            .map_err(|e| {
                BackendError::Message(format!("xfixes set_window_shape_region failed: {e}"))
            })?;
        self.conn
            .send_and_check_request(&xfixes::DestroyRegion { region })
            .map_err(|e| BackendError::Message(format!("xfixes destroy_region failed: {e}")))?;
        self.conn
            .flush()
            .map_err(|e| BackendError::Message(format!("xcb flush after shape failed: {e}")))?;
        let focus_cookie = self.conn.send_request(&x::GetInputFocus {});
        let _ = self
            .conn
            .wait_for_reply(focus_cookie)
            .map_err(|e| BackendError::Message(format!("sync after shape failed: {e}")))?;
        Ok(())
    }

    pub(crate) fn set_window_type_notification(
        &self,
        window: x::Window,
    ) -> Result<(), BackendError> {
        let wm_window_type = self.intern_atom(b"_NET_WM_WINDOW_TYPE")?;
        let notification = self.intern_atom(b"_NET_WM_WINDOW_TYPE_NOTIFICATION")?;
        self.conn
            .send_and_check_request(&x::ChangeProperty {
                mode: x::PropMode::Replace,
                window,
                property: wm_window_type,
                r#type: x::ATOM_ATOM,
                data: &[notification],
            })
            .map_err(|e| BackendError::Message(format!("set _NET_WM_WINDOW_TYPE failed: {e}")))
    }

    pub(crate) fn claim_compositor_selection(
        &self,
        root: x::Window,
        screen_num: usize,
    ) -> Result<x::Window, BackendError> {
        let selection_name = format!("_NET_WM_CM_S{screen_num}");
        let selection_atom = self.intern_atom(selection_name.as_bytes())?;
        let owner_window: x::Window = self.conn.generate_id();
        self.conn
            .send_and_check_request(&x::CreateWindow {
                depth: 0,
                wid: owner_window,
                parent: root,
                x: 0,
                y: 0,
                width: 1,
                height: 1,
                border_width: 0,
                class: x::WindowClass::InputOnly,
                visual: 0,
                value_list: &[],
            })
            .map_err(|e| {
                BackendError::Message(format!("create CM selection owner window failed: {e}"))
            })?;
        self.conn
            .send_and_check_request(&x::SetSelectionOwner {
                owner: owner_window,
                selection: selection_atom,
                time: x::CURRENT_TIME,
            })
            .map_err(|e| {
                BackendError::Message(format!("set compositor selection owner failed: {e}"))
            })?;
        self.conn.flush().map_err(|e| {
            BackendError::Message(format!("xcb flush after selection owner failed: {e}"))
        })?;
        Ok(owner_window)
    }

    pub(crate) fn name_window_pixmap(
        &self,
        window: x::Window,
        pixmap: x::Pixmap,
    ) -> Result<(), BackendError> {
        self.conn
            .send_and_check_request(&composite::NameWindowPixmap { window, pixmap })
            .map_err(|e| BackendError::Message(format!("name_window_pixmap failed: {e}")))
    }
}

impl X11BootstrapOps for XcbCompositorProtocol<'_> {
    fn query_damage_event_base(&self) -> Result<u8, String> {
        let damage_cookie = self.conn.send_request(&xcb::damage::QueryVersion {
            client_major_version: 1,
            client_minor_version: 1,
        });
        self.conn
            .wait_for_reply(damage_cookie)
            .map_err(|e| format!("damage_query_version: {e}"))?;
        xcb::damage::get_extension_data(self.conn)
            .map(|ext| ext.first_event)
            .ok_or("damage extension not available".to_string())
    }

    fn get_overlay_window(&self, root: u32) -> Result<u32, String> {
        XcbCompositorProtocol::get_overlay_window(self, x::Window::new(root))
            .map(|win| win.resource_id())
            .map_err(|e| e.to_string())
    }

    fn set_overlay_input_passthrough(&self, overlay_window: u32) -> Result<(), String> {
        XcbCompositorProtocol::set_overlay_input_passthrough(self, x::Window::new(overlay_window))
            .map_err(|e| e.to_string())
    }

    fn set_overlay_window_type_notification(&self, overlay_window: u32) -> Result<(), String> {
        XcbCompositorProtocol::set_window_type_notification(self, x::Window::new(overlay_window))
            .map_err(|e| e.to_string())
    }

    fn claim_compositor_selection_owner(&self, root: u32, screen_num: i32) -> Result<u32, String> {
        XcbCompositorProtocol::claim_compositor_selection(
            self,
            x::Window::new(root),
            screen_num as usize,
        )
        .map(|win| win.resource_id())
        .map_err(|e| e.to_string())
    }
}

impl X11ConnectionOps for XcbCompositorProtocol<'_> {
    fn generate_xid(&self) -> Result<u32, String> {
        let xid: x::Window = self.conn.generate_id();
        Ok(xid.resource_id())
    }

    fn flush_x11(&self) -> Result<(), String> {
        self.conn.flush().map_err(|e| format!("flush: {e}"))
    }
}

impl X11CompositeRedirectOps for XcbCompositorProtocol<'_> {
    fn query_composite_version(&self) -> Result<(), String> {
        let cookie = self.conn.send_request(&composite::QueryVersion {
            client_major_version: 0,
            client_minor_version: 4,
        });
        self.conn
            .wait_for_reply(cookie)
            .map(|_| ())
            .map_err(|e| format!("composite_query_version: {e}"))
    }

    fn redirect_subwindows_manual(&self, root: u32) -> Result<(), String> {
        XcbCompositorProtocol::redirect_subwindows(self, x::Window::new(root))
            .map_err(|e| e.to_string())
    }

    fn redirect_window_manual(&self, window: u32) -> Result<(), String> {
        self.conn
            .send_and_check_request(&composite::RedirectWindow {
                window: x::Window::new(window),
                update: composite::Redirect::Manual,
            })
            .map_err(|e| format!("redirect_window: {e}"))
    }

    fn unredirect_window_manual(&self, window: u32) -> Result<(), String> {
        self.conn
            .send_and_check_request(&composite::UnredirectWindow {
                window: x::Window::new(window),
                update: composite::Redirect::Manual,
            })
            .map_err(|e| format!("unredirect_window: {e}"))
    }

    fn unredirect_subwindows_manual(&self, root: u32) -> Result<(), String> {
        XcbCompositorProtocol::unredirect_subwindows(self, x::Window::new(root))
            .map_err(|e| e.to_string())
    }

    fn release_overlay_window(&self, overlay_window: u32) -> Result<(), String> {
        XcbCompositorProtocol::release_overlay_window(self, x::Window::new(overlay_window))
            .map_err(|e| e.to_string())
    }
}

impl X11PresentOps for XcbCompositorProtocol<'_> {
    fn query_present_version(&self) -> Result<(u32, u32), String> {
        let cookie = self.conn.send_request(&xcb::present::QueryVersion {
            major_version: 1,
            minor_version: 0,
        });
        self.conn
            .wait_for_reply(cookie)
            .map(|reply| (reply.major_version(), reply.minor_version()))
            .map_err(|e| format!("present_query_version: {e}"))
    }

    fn query_present_event_base(&self) -> Result<u8, String> {
        xcb::present::get_extension_data(self.conn)
            .map(|info| info.first_event)
            .ok_or("Present extension info not available".to_string())
    }

    fn select_present_input(&self, event_id: u32, window: u32) -> Result<(), String> {
        self.conn.send_request(&xcb::present::SelectInput {
            eid: xcb::present::EventXid::new(event_id),
            window: x::Window::new(window),
            event_mask: xcb::present::EventMask::COMPLETE_NOTIFY
                | xcb::present::EventMask::IDLE_NOTIFY,
        });
        Ok(())
    }

    fn present_pixmap_for_window(
        &self,
        window: u32,
        pixmap: u32,
        target_msc: u64,
        serial: u32,
    ) -> Result<(), String> {
        self.conn.send_request(&xcb::present::Pixmap {
            window: x::Window::new(window),
            pixmap: x::Pixmap::new(pixmap),
            serial,
            valid: xcb::xfixes::Region::none(),
            update: xcb::xfixes::Region::none(),
            x_off: 0,
            y_off: 0,
            target_crtc: xcb::randr::Crtc::none(),
            wait_fence: xcb::sync::Fence::none(),
            idle_fence: xcb::sync::Fence::none(),
            options: 0,
            target_msc,
            divisor: 1,
            remainder: 0,
            notifies: &[],
        });
        Ok(())
    }

    fn notify_present_msc(&self, window: u32, serial: u32, target_msc: u64) -> Result<(), String> {
        self.conn.send_request(&xcb::present::NotifyMsc {
            window: x::Window::new(window),
            serial,
            target_msc,
            divisor: 1,
            remainder: 0,
        });
        Ok(())
    }
}

impl X11RandrOps for XcbCompositorProtocol<'_> {
    fn query_monitor_rects(&self, root: u32) -> Vec<(u32, i32, i32, u32, u32)> {
        let root = x::Window::new(root);
        let mut rects = Vec::new();

        let ver_cookie = self.conn.send_request(&xcb::randr::QueryVersion {
            major_version: 1,
            minor_version: 5,
        });
        if let Ok(ver) = self.conn.wait_for_reply(ver_cookie) {
            if ver.major_version() > 1 || (ver.major_version() == 1 && ver.minor_version() >= 5) {
                let mon_cookie = self.conn.send_request(&xcb::randr::GetMonitors {
                    window: root,
                    get_active: true,
                });
                if let Ok(reply) = self.conn.wait_for_reply(mon_cookie) {
                    for (idx, mon) in reply.monitors().enumerate() {
                        if mon.width() > 0 && mon.height() > 0 {
                            rects.push((
                                idx as u32,
                                mon.x() as i32,
                                mon.y() as i32,
                                mon.width() as u32,
                                mon.height() as u32,
                            ));
                        }
                    }
                    if !rects.is_empty() {
                        return rects;
                    }
                }
            }
        }

        let res_cookie = self
            .conn
            .send_request(&xcb::randr::GetScreenResources { window: root });
        if let Ok(resources) = self.conn.wait_for_reply(res_cookie) {
            for (idx, crtc_id) in resources.crtcs().iter().enumerate() {
                let info_cookie = self.conn.send_request(&xcb::randr::GetCrtcInfo {
                    crtc: *crtc_id,
                    config_timestamp: 0,
                });
                if let Ok(info) = self.conn.wait_for_reply(info_cookie) {
                    if info.width() > 0 && info.height() > 0 {
                        rects.push((
                            idx as u32,
                            info.x() as i32,
                            info.y() as i32,
                            info.width() as u32,
                            info.height() as u32,
                        ));
                    }
                }
            }
        }

        rects
    }

    fn query_monitor_refresh_rates(&self, root: u32) -> std::collections::HashMap<u32, u32> {
        let root = x::Window::new(root);
        let mut rates = std::collections::HashMap::new();

        fn calc_refresh_hz(dot_clock: u32, htotal: u16, vtotal: u16) -> u32 {
            if htotal == 0 || vtotal == 0 {
                return 60;
            }
            ((dot_clock as u64 * 1000) / (htotal as u64 * vtotal as u64) / 1000) as u32
        }

        let ver_cookie = self.conn.send_request(&xcb::randr::QueryVersion {
            major_version: 1,
            minor_version: 5,
        });
        if let Ok(ver) = self.conn.wait_for_reply(ver_cookie) {
            if ver.major_version() > 1 || (ver.major_version() == 1 && ver.minor_version() >= 5) {
                let res_cookie = self
                    .conn
                    .send_request(&xcb::randr::GetScreenResources { window: root });
                if let Ok(resources) = self.conn.wait_for_reply(res_cookie) {
                    let modes = resources.modes();
                    let mon_cookie = self.conn.send_request(&xcb::randr::GetMonitors {
                        window: root,
                        get_active: true,
                    });
                    if let Ok(reply) = self.conn.wait_for_reply(mon_cookie) {
                        for (idx, mon) in reply.monitors().enumerate() {
                            if let Some(output_id) = mon.outputs().first() {
                                let output_cookie =
                                    self.conn.send_request(&xcb::randr::GetOutputInfo {
                                        output: *output_id,
                                        config_timestamp: 0,
                                    });
                                if let Ok(output_info) = self.conn.wait_for_reply(output_cookie) {
                                    if !output_info.crtc().is_none() {
                                        let crtc_cookie =
                                            self.conn.send_request(&xcb::randr::GetCrtcInfo {
                                                crtc: output_info.crtc(),
                                                config_timestamp: 0,
                                            });
                                        if let Ok(crtc_info) = self.conn.wait_for_reply(crtc_cookie)
                                        {
                                            let refresh = modes
                                                .iter()
                                                .find(|m| m.id == crtc_info.mode().resource_id())
                                                .map(|m| {
                                                    calc_refresh_hz(m.dot_clock, m.htotal, m.vtotal)
                                                })
                                                .unwrap_or(60);
                                            rates.insert(idx as u32, refresh);
                                        }
                                    }
                                }
                            }
                        }
                        if !rates.is_empty() {
                            return rates;
                        }
                    }
                }
            }
        }

        let res_cookie = self
            .conn
            .send_request(&xcb::randr::GetScreenResources { window: root });
        if let Ok(resources) = self.conn.wait_for_reply(res_cookie) {
            let modes = resources.modes();
            for (idx, crtc_id) in resources.crtcs().iter().enumerate() {
                let info_cookie = self.conn.send_request(&xcb::randr::GetCrtcInfo {
                    crtc: *crtc_id,
                    config_timestamp: 0,
                });
                if let Ok(info) = self.conn.wait_for_reply(info_cookie) {
                    if info.width() > 0 && info.height() > 0 {
                        let refresh = modes
                            .iter()
                            .find(|m| m.id == info.mode().resource_id())
                            .map(|m| calc_refresh_hz(m.dot_clock, m.htotal, m.vtotal))
                            .unwrap_or(60);
                        rates.insert(idx as u32, refresh);
                    }
                }
            }
        }

        rates
    }
}

impl X11TextureSourceOps for XcbCompositorProtocol<'_> {
    fn create_window_damage(&self, damage_id: u32, window: u32) -> Result<(), String> {
        self.conn
            .send_and_check_request(&xcb::damage::Create {
                damage: xcb::damage::Damage::new(damage_id),
                drawable: x::Drawable::Window(x::Window::new(window)),
                level: xcb::damage::ReportLevel::NonEmpty,
            })
            .map_err(|e| format!("damage_create: {e}"))
    }

    fn destroy_window_damage(&self, damage_id: u32) -> Result<(), String> {
        self.conn
            .send_and_check_request(&xcb::damage::Destroy {
                damage: xcb::damage::Damage::new(damage_id),
            })
            .map_err(|e| format!("damage_destroy: {e}"))
    }

    fn clear_window_damage(&self, damage_id: u32) -> Result<(), String> {
        self.conn
            .send_and_check_request(&xcb::damage::Subtract {
                damage: xcb::damage::Damage::new(damage_id),
                repair: xfixes::Region::none(),
                parts: xfixes::Region::none(),
            })
            .map_err(|e| format!("damage_subtract: {e}"))
    }

    fn name_window_pixmap(&self, window: u32, pixmap: u32) -> Result<(), String> {
        XcbCompositorProtocol::name_window_pixmap(
            self,
            x::Window::new(window),
            x::Pixmap::new(pixmap),
        )
        .map_err(|e| e.to_string())
    }

    fn free_window_pixmap(&self, pixmap: u32) -> Result<(), String> {
        self.conn
            .send_and_check_request(&x::FreePixmap {
                pixmap: x::Pixmap::new(pixmap),
            })
            .map_err(|e| format!("free_pixmap: {e}"))
    }

    fn get_window_visual(&self, window: u32) -> Result<u32, String> {
        let cookie = self.conn.send_request(&x::GetWindowAttributes {
            window: x::Window::new(window),
        });
        self.conn
            .wait_for_reply(cookie)
            .map(|reply| reply.visual())
            .map_err(|e| format!("get_window_attributes reply: {e}"))
    }

    fn get_window_depth(&self, window: u32) -> Result<u8, String> {
        let cookie = self.conn.send_request(&x::GetGeometry {
            drawable: x::Drawable::Window(x::Window::new(window)),
        });
        self.conn
            .wait_for_reply(cookie)
            .map(|reply| reply.depth())
            .map_err(|e| format!("get_geometry reply: {e}"))
    }
}

impl X11WindowResourceOps for XcbCompositorProtocol<'_> {
    fn destroy_window_resource(&self, window: u32) -> Result<(), String> {
        self.conn
            .send_and_check_request(&x::DestroyWindow {
                window: x::Window::new(window),
            })
            .map_err(|e| format!("destroy_window: {e}"))
    }
}

impl X11BootstrapOps for XcbSharedCompositorConnection {
    fn query_damage_event_base(&self) -> Result<u8, String> {
        let protocol = self.protocol();
        <XcbCompositorProtocol<'_> as X11BootstrapOps>::query_damage_event_base(&protocol)
    }

    fn get_overlay_window(&self, root: u32) -> Result<u32, String> {
        let protocol = self.protocol();
        <XcbCompositorProtocol<'_> as X11BootstrapOps>::get_overlay_window(&protocol, root)
    }

    fn set_overlay_input_passthrough(&self, overlay_window: u32) -> Result<(), String> {
        let protocol = self.protocol();
        <XcbCompositorProtocol<'_> as X11BootstrapOps>::set_overlay_input_passthrough(
            &protocol,
            overlay_window,
        )
    }

    fn set_overlay_window_type_notification(&self, overlay_window: u32) -> Result<(), String> {
        let protocol = self.protocol();
        <XcbCompositorProtocol<'_> as X11BootstrapOps>::set_overlay_window_type_notification(
            &protocol,
            overlay_window,
        )
    }

    fn claim_compositor_selection_owner(&self, root: u32, screen_num: i32) -> Result<u32, String> {
        let protocol = self.protocol();
        <XcbCompositorProtocol<'_> as X11BootstrapOps>::claim_compositor_selection_owner(
            &protocol, root, screen_num,
        )
    }
}

impl X11ConnectionOps for XcbSharedCompositorConnection {
    fn generate_xid(&self) -> Result<u32, String> {
        let protocol = self.protocol();
        <XcbCompositorProtocol<'_> as X11ConnectionOps>::generate_xid(&protocol)
    }

    fn flush_x11(&self) -> Result<(), String> {
        let protocol = self.protocol();
        <XcbCompositorProtocol<'_> as X11ConnectionOps>::flush_x11(&protocol)
    }
}

impl X11CompositeRedirectOps for XcbSharedCompositorConnection {
    fn query_composite_version(&self) -> Result<(), String> {
        let protocol = self.protocol();
        <XcbCompositorProtocol<'_> as X11CompositeRedirectOps>::query_composite_version(&protocol)
    }

    fn redirect_subwindows_manual(&self, root: u32) -> Result<(), String> {
        let protocol = self.protocol();
        <XcbCompositorProtocol<'_> as X11CompositeRedirectOps>::redirect_subwindows_manual(
            &protocol, root,
        )
    }

    fn redirect_window_manual(&self, window: u32) -> Result<(), String> {
        let protocol = self.protocol();
        <XcbCompositorProtocol<'_> as X11CompositeRedirectOps>::redirect_window_manual(
            &protocol, window,
        )
    }

    fn unredirect_window_manual(&self, window: u32) -> Result<(), String> {
        let protocol = self.protocol();
        <XcbCompositorProtocol<'_> as X11CompositeRedirectOps>::unredirect_window_manual(
            &protocol, window,
        )
    }

    fn unredirect_subwindows_manual(&self, root: u32) -> Result<(), String> {
        let protocol = self.protocol();
        <XcbCompositorProtocol<'_> as X11CompositeRedirectOps>::unredirect_subwindows_manual(
            &protocol, root,
        )
    }

    fn release_overlay_window(&self, overlay_window: u32) -> Result<(), String> {
        let protocol = self.protocol();
        <XcbCompositorProtocol<'_> as X11CompositeRedirectOps>::release_overlay_window(
            &protocol,
            overlay_window,
        )
    }
}

impl X11PresentOps for XcbSharedCompositorConnection {
    fn query_present_version(&self) -> Result<(u32, u32), String> {
        let protocol = self.protocol();
        <XcbCompositorProtocol<'_> as X11PresentOps>::query_present_version(&protocol)
    }

    fn query_present_event_base(&self) -> Result<u8, String> {
        let protocol = self.protocol();
        <XcbCompositorProtocol<'_> as X11PresentOps>::query_present_event_base(&protocol)
    }

    fn select_present_input(&self, event_id: u32, window: u32) -> Result<(), String> {
        let protocol = self.protocol();
        <XcbCompositorProtocol<'_> as X11PresentOps>::select_present_input(
            &protocol, event_id, window,
        )
    }

    fn present_pixmap_for_window(
        &self,
        window: u32,
        pixmap: u32,
        target_msc: u64,
        serial: u32,
    ) -> Result<(), String> {
        let protocol = self.protocol();
        <XcbCompositorProtocol<'_> as X11PresentOps>::present_pixmap_for_window(
            &protocol, window, pixmap, target_msc, serial,
        )
    }

    fn notify_present_msc(&self, window: u32, serial: u32, target_msc: u64) -> Result<(), String> {
        let protocol = self.protocol();
        <XcbCompositorProtocol<'_> as X11PresentOps>::notify_present_msc(
            &protocol, window, serial, target_msc,
        )
    }
}

impl X11RandrOps for XcbSharedCompositorConnection {
    fn query_monitor_rects(&self, root: u32) -> std::vec::Vec<(u32, i32, i32, u32, u32)> {
        let protocol = self.protocol();
        <XcbCompositorProtocol<'_> as X11RandrOps>::query_monitor_rects(&protocol, root)
    }

    fn query_monitor_refresh_rates(&self, root: u32) -> std::collections::HashMap<u32, u32> {
        let protocol = self.protocol();
        <XcbCompositorProtocol<'_> as X11RandrOps>::query_monitor_refresh_rates(&protocol, root)
    }
}

impl X11TextureSourceOps for XcbSharedCompositorConnection {
    fn create_window_damage(&self, damage_id: u32, window: u32) -> Result<(), String> {
        let protocol = self.protocol();
        <XcbCompositorProtocol<'_> as X11TextureSourceOps>::create_window_damage(
            &protocol, damage_id, window,
        )
    }

    fn destroy_window_damage(&self, damage_id: u32) -> Result<(), String> {
        let protocol = self.protocol();
        <XcbCompositorProtocol<'_> as X11TextureSourceOps>::destroy_window_damage(
            &protocol, damage_id,
        )
    }

    fn clear_window_damage(&self, damage_id: u32) -> Result<(), String> {
        let protocol = self.protocol();
        <XcbCompositorProtocol<'_> as X11TextureSourceOps>::clear_window_damage(
            &protocol, damage_id,
        )
    }

    fn name_window_pixmap(&self, window: u32, pixmap: u32) -> Result<(), String> {
        let protocol = self.protocol();
        <XcbCompositorProtocol<'_> as X11TextureSourceOps>::name_window_pixmap(
            &protocol, window, pixmap,
        )
    }

    fn free_window_pixmap(&self, pixmap: u32) -> Result<(), String> {
        let protocol = self.protocol();
        <XcbCompositorProtocol<'_> as X11TextureSourceOps>::free_window_pixmap(&protocol, pixmap)
    }

    fn get_window_visual(&self, window: u32) -> Result<u32, String> {
        let protocol = self.protocol();
        <XcbCompositorProtocol<'_> as X11TextureSourceOps>::get_window_visual(&protocol, window)
    }

    fn get_window_depth(&self, window: u32) -> Result<u8, String> {
        let protocol = self.protocol();
        <XcbCompositorProtocol<'_> as X11TextureSourceOps>::get_window_depth(&protocol, window)
    }
}

impl X11WindowResourceOps for XcbSharedCompositorConnection {
    fn destroy_window_resource(&self, window: u32) -> Result<(), String> {
        let protocol = self.protocol();
        <XcbCompositorProtocol<'_> as X11WindowResourceOps>::destroy_window_resource(
            &protocol, window,
        )
    }
}

pub(crate) fn create_shared_compositor_connection(
    conn: Arc<xcb::Connection>,
) -> Result<Arc<XcbSharedCompositorConnection>, BackendError> {
    Ok(Arc::new(XcbSharedCompositorConnection::new(conn)))
}
