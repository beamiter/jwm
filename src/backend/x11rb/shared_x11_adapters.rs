use crate::backend::x11::compositor_common::{
    X11BootstrapOps, X11CompositeRedirectOps, X11ConnectionOps, X11PresentOps, X11RandrOps,
    X11TextureSourceOps, X11WindowResourceOps,
};
use std::collections::HashMap;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::composite::ConnectionExt as CompositeExt;
use x11rb::protocol::damage::{self, ConnectionExt as DamageExt};
use x11rb::protocol::present::{self, ConnectionExt as PresentExt};
use x11rb::protocol::randr::ConnectionExt as RandrExt;
use x11rb::protocol::xfixes::ConnectionExt as XFixesExt;
use x11rb::protocol::xproto::{self, ConnectionExt as XProtoExt};
use x11rb::wrapper::ConnectionExt as WrapperExt;

impl<T> X11BootstrapOps for T
where
    T: Connection + RequestConnection,
{
    fn query_damage_event_base(&self) -> Result<u8, String> {
        self.damage_query_version(1, 1)
            .map_err(|e| format!("damage_query_version: {e}"))?
            .reply()
            .map_err(|e| format!("damage reply: {e}"))?;

        self.extension_information(damage::X11_EXTENSION_NAME)
            .map_err(|e| format!("damage ext info: {e}"))?
            .map(|ext| ext.first_event)
            .ok_or("damage extension not available".to_string())
    }

    fn get_overlay_window(&self, root: u32) -> Result<u32, String> {
        self.composite_get_overlay_window(root)
            .map_err(|e| format!("get_overlay_window: {e}"))?
            .reply()
            .map(|reply| reply.overlay_win)
            .map_err(|e| format!("overlay reply: {e}"))
    }

    fn set_overlay_input_passthrough(&self, overlay_window: u32) -> Result<(), String> {
        let xfixes_ver = self
            .xfixes_query_version(5, 0)
            .map_err(|e| format!("xfixes_query_version: {e}"))?
            .reply()
            .map_err(|e| format!("xfixes version reply: {e}"))?;
        log::info!(
            "compositor: XFixes version {}.{}",
            xfixes_ver.major_version,
            xfixes_ver.minor_version
        );

        log::info!(
            "compositor: setting empty INPUT shape on overlay 0x{:x} to pass through input",
            overlay_window
        );
        let region = self.generate_id().map_err(|e| format!("gen id: {e}"))?;
        self.xfixes_create_region(region, &[])
            .map_err(|e| format!("create_region: {e}"))?;
        self.xfixes_set_window_shape_region(
            overlay_window,
            x11rb::protocol::shape::SK::INPUT,
            0,
            0,
            region,
        )
        .map_err(|e| format!("set_window_shape_region: {e}"))?;
        self.xfixes_destroy_region(region)
            .map_err(|e| format!("destroy_region: {e}"))?;
        self.flush()
            .map_err(|e| format!("flush after shape: {e}"))?;
        self.get_input_focus()
            .map_err(|e| format!("sync after shape: {e}"))?
            .reply()
            .map_err(|e| format!("sync reply after shape: {e}"))?;
        log::info!("compositor: overlay input shape set successfully (verified via sync)");
        Ok(())
    }

    fn set_overlay_window_type_notification(&self, overlay_window: u32) -> Result<(), String> {
        let wm_type_atom = self
            .intern_atom(false, b"_NET_WM_WINDOW_TYPE")
            .map_err(|e| format!("intern _NET_WM_WINDOW_TYPE: {e}"))?
            .reply()
            .map_err(|e| format!("intern reply: {e}"))?
            .atom;
        let notification_atom = self
            .intern_atom(false, b"_NET_WM_WINDOW_TYPE_NOTIFICATION")
            .map_err(|e| format!("intern _NET_WM_WINDOW_TYPE_NOTIFICATION: {e}"))?
            .reply()
            .map_err(|e| format!("intern reply: {e}"))?
            .atom;
        self.change_property32(
            xproto::PropMode::REPLACE,
            overlay_window,
            wm_type_atom,
            xproto::AtomEnum::ATOM,
            &[notification_atom],
        )
        .map_err(|e| format!("set overlay _NET_WM_WINDOW_TYPE: {e}"))?;
        let _ = self.flush();
        log::info!(
            "compositor: set overlay 0x{:x} _NET_WM_WINDOW_TYPE = NOTIFICATION",
            overlay_window
        );
        Ok(())
    }

    fn claim_compositor_selection_owner(&self, root: u32, screen_num: i32) -> Result<u32, String> {
        let sel_name = format!("_NET_WM_CM_S{}", screen_num);
        let cm_atom = self
            .intern_atom(false, sel_name.as_bytes())
            .map_err(|e| format!("intern {sel_name}: {e}"))?
            .reply()
            .map_err(|e| format!("intern reply {sel_name}: {e}"))?
            .atom;
        let sel_win = self
            .generate_id()
            .map_err(|e| format!("generate_id for CM selection owner: {e}"))?;
        self.create_window(
            0,
            sel_win,
            root,
            0,
            0,
            1,
            1,
            0,
            xproto::WindowClass::INPUT_ONLY,
            0,
            &xproto::CreateWindowAux::default(),
        )
        .map_err(|e| format!("create CM selection owner window: {e}"))?;
        self.set_selection_owner(sel_win, cm_atom, x11rb::CURRENT_TIME)
            .map_err(|e| format!("set_selection_owner {sel_name}: {e}"))?;
        let _ = self.flush();
        log::info!(
            "compositor: claimed {} selection (owner=0x{:x})",
            sel_name,
            sel_win
        );
        Ok(sel_win)
    }
}

impl<T> X11ConnectionOps for T
where
    T: Connection + RequestConnection,
{
    fn generate_xid(&self) -> Result<u32, String> {
        self.generate_id().map_err(|e| format!("generate_id: {e}"))
    }

    fn flush_x11(&self) -> Result<(), String> {
        self.flush().map_err(|e| format!("flush: {e}"))
    }
}

impl<T> X11CompositeRedirectOps for T
where
    T: Connection + RequestConnection,
{
    fn query_composite_version(&self) -> Result<(), String> {
        self.composite_query_version(0, 4)
            .map_err(|e| format!("composite_query_version: {e}"))?
            .reply()
            .map(|_| ())
            .map_err(|e| format!("composite reply: {e}"))
    }

    fn redirect_subwindows_manual(&self, root: u32) -> Result<(), String> {
        self.composite_redirect_subwindows(root, x11rb::protocol::composite::Redirect::MANUAL)
            .map(|_| ())
            .map_err(|e| format!("redirect_subwindows: {e}"))
    }

    fn redirect_window_manual(&self, window: u32) -> Result<(), String> {
        self.composite_redirect_window(window, x11rb::protocol::composite::Redirect::MANUAL)
            .map(|_| ())
            .map_err(|e| format!("redirect_window: {e}"))
    }

    fn unredirect_window_manual(&self, window: u32) -> Result<(), String> {
        self.composite_unredirect_window(window, x11rb::protocol::composite::Redirect::MANUAL)
            .map(|_| ())
            .map_err(|e| format!("unredirect_window: {e}"))
    }

    fn unredirect_subwindows_manual(&self, root: u32) -> Result<(), String> {
        self.composite_unredirect_subwindows(root, x11rb::protocol::composite::Redirect::MANUAL)
            .map(|_| ())
            .map_err(|e| format!("unredirect_subwindows: {e}"))
    }

    fn release_overlay_window(&self, overlay_window: u32) -> Result<(), String> {
        self.composite_release_overlay_window(overlay_window)
            .map(|_| ())
            .map_err(|e| format!("release_overlay_window: {e}"))
    }
}

impl<T> X11WindowResourceOps for T
where
    T: Connection + RequestConnection,
{
    fn destroy_window_resource(&self, window: u32) -> Result<(), String> {
        self.destroy_window(window)
            .map(|_| ())
            .map_err(|e| format!("destroy_window: {e}"))
    }
}

impl<T> X11PresentOps for T
where
    T: Connection + RequestConnection,
{
    fn query_present_version(&self) -> Result<(u32, u32), String> {
        self.present_query_version(1, 0)
            .map_err(|e| format!("present_query_version: {e}"))?
            .reply()
            .map(|reply| (reply.major_version, reply.minor_version))
            .map_err(|e| format!("present version reply: {e}"))
    }

    fn query_present_event_base(&self) -> Result<u8, String> {
        self.extension_information(present::X11_EXTENSION_NAME)
            .map_err(|e| format!("present ext info: {e}"))?
            .map(|info| info.first_event)
            .ok_or("Present extension info not available".to_string())
    }

    fn select_present_input(&self, event_id: u32, window: u32) -> Result<(), String> {
        let event_mask = present::EventMask::COMPLETE_NOTIFY | present::EventMask::IDLE_NOTIFY;
        self.present_select_input(event_id, window, event_mask)
            .map(|_| ())
            .map_err(|e| format!("select_input failed: {e}"))
    }

    fn present_pixmap_for_window(
        &self,
        window: u32,
        pixmap: u32,
        target_msc: u64,
        serial: u32,
    ) -> Result<(), String> {
        self.present_pixmap(
            window,
            pixmap,
            serial,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            target_msc,
            1,
            0,
            &[],
        )
        .map(|_| ())
        .map_err(|e| format!("present_pixmap failed: {e}"))
    }

    fn notify_present_msc(&self, window: u32, serial: u32, target_msc: u64) -> Result<(), String> {
        self.present_notify_msc(window, serial, target_msc, 1, 0)
            .map(|_| ())
            .map_err(|e| format!("notify_msc failed: {e}"))
    }
}

impl<T> X11TextureSourceOps for T
where
    T: Connection + RequestConnection,
{
    fn create_window_damage(&self, damage_id: u32, window: u32) -> Result<(), String> {
        self.damage_create(damage_id, window, damage::ReportLevel::NON_EMPTY)
            .map(|_| ())
            .map_err(|e| format!("damage_create: {e}"))
    }

    fn destroy_window_damage(&self, damage_id: u32) -> Result<(), String> {
        self.damage_destroy(damage_id)
            .map(|_| ())
            .map_err(|e| format!("damage_destroy: {e}"))
    }

    fn clear_window_damage(&self, damage_id: u32) -> Result<(), String> {
        self.damage_subtract(damage_id, 0u32, 0u32)
            .map(|_| ())
            .map_err(|e| format!("damage_subtract: {e}"))
    }

    fn name_window_pixmap(&self, window: u32, pixmap: u32) -> Result<(), String> {
        self.composite_name_window_pixmap(window, pixmap)
            .map(|_| ())
            .map_err(|e| format!("name_window_pixmap: {e}"))
    }

    fn free_window_pixmap(&self, pixmap: u32) -> Result<(), String> {
        self.free_pixmap(pixmap)
            .map(|_| ())
            .map_err(|e| format!("free_pixmap: {e}"))
    }

    fn get_window_visual(&self, window: u32) -> Result<u32, String> {
        self.get_window_attributes(window)
            .map_err(|e| format!("get_window_attributes: {e}"))?
            .reply()
            .map(|reply| reply.visual)
            .map_err(|e| format!("get_window_attributes reply: {e}"))
    }

    fn get_window_depth(&self, window: u32) -> Result<u8, String> {
        self.get_geometry(window)
            .map_err(|e| format!("get_geometry: {e}"))?
            .reply()
            .map(|reply| reply.depth)
            .map_err(|e| format!("get_geometry reply: {e}"))
    }
}

fn build_monitor_rects_from_randr<C>(conn: &C, root: u32) -> Vec<(u32, i32, i32, u32, u32)>
where
    C: Connection + RequestConnection,
{
    let mut rects = Vec::new();

    if let Ok(ver_cookie) = conn.randr_query_version(1, 5) {
        if let Ok(ver) = ver_cookie.reply() {
            if ver.major_version > 1 || (ver.major_version == 1 && ver.minor_version >= 5) {
                if let Ok(mon_cookie) = conn.randr_get_monitors(root, true) {
                    if let Ok(reply) = mon_cookie.reply() {
                        for (idx, mon) in reply.monitors.iter().enumerate() {
                            if mon.width > 0 && mon.height > 0 {
                                rects.push((
                                    idx as u32,
                                    mon.x as i32,
                                    mon.y as i32,
                                    mon.width as u32,
                                    mon.height as u32,
                                ));
                            }
                        }
                        if !rects.is_empty() {
                            return rects;
                        }
                    }
                }
            }
        }
    }

    if let Ok(res_cookie) = conn.randr_get_screen_resources(root) {
        if let Ok(resources) = res_cookie.reply() {
            for (idx, crtc_id) in resources.crtcs.iter().enumerate() {
                if let Ok(info_cookie) = conn.randr_get_crtc_info(*crtc_id, 0) {
                    if let Ok(info) = info_cookie.reply() {
                        if info.width > 0 && info.height > 0 {
                            rects.push((
                                idx as u32,
                                info.x as i32,
                                info.y as i32,
                                info.width as u32,
                                info.height as u32,
                            ));
                        }
                    }
                }
            }
            if !rects.is_empty() {
                return rects;
            }
        }
    }

    rects
}

fn build_monitor_refresh_rates_from_randr<C>(conn: &C, root: u32) -> HashMap<u32, u32>
where
    C: Connection + RequestConnection,
{
    let mut rates = HashMap::new();

    fn calc_refresh_mhz(mode: &x11rb::protocol::randr::ModeInfo) -> u32 {
        if mode.htotal == 0 || mode.vtotal == 0 {
            return 60000;
        }
        let dot_clock = mode.dot_clock as u64;
        let htotal = mode.htotal as u64;
        let vtotal = mode.vtotal as u64;
        ((dot_clock * 1000) / (htotal * vtotal)) as u32
    }

    if let Ok(ver_cookie) = conn.randr_query_version(1, 5) {
        if let Ok(ver) = ver_cookie.reply() {
            if ver.major_version > 1 || (ver.major_version == 1 && ver.minor_version >= 5) {
                if let Ok(res_cookie) = conn.randr_get_screen_resources(root) {
                    if let Ok(resources) = res_cookie.reply() {
                        let modes = resources.modes;

                        if let Ok(mon_cookie) = conn.randr_get_monitors(root, true) {
                            if let Ok(reply) = mon_cookie.reply() {
                                for (idx, mon) in reply.monitors.iter().enumerate() {
                                    if let Some(&output_id) = mon.outputs.first() {
                                        if let Ok(output_cookie) =
                                            conn.randr_get_output_info(output_id, 0)
                                        {
                                            if let Ok(output_info) = output_cookie.reply() {
                                                if output_info.crtc != 0 {
                                                    if let Ok(crtc_cookie) = conn
                                                        .randr_get_crtc_info(output_info.crtc, 0)
                                                    {
                                                        if let Ok(crtc_info) = crtc_cookie.reply() {
                                                            let refresh = modes
                                                                .iter()
                                                                .find(|m| m.id == crtc_info.mode)
                                                                .map(calc_refresh_mhz)
                                                                .unwrap_or(60000);
                                                            rates
                                                                .insert(idx as u32, refresh / 1000);
                                                        }
                                                    }
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
            }
        }
    }

    if let Ok(res_cookie) = conn.randr_get_screen_resources(root) {
        if let Ok(resources) = res_cookie.reply() {
            let modes = resources.modes;
            for (idx, crtc_id) in resources.crtcs.iter().enumerate() {
                if let Ok(info_cookie) = conn.randr_get_crtc_info(*crtc_id, 0) {
                    if let Ok(info) = info_cookie.reply() {
                        if info.width > 0 && info.height > 0 {
                            let refresh = modes
                                .iter()
                                .find(|m| m.id == info.mode)
                                .map(calc_refresh_mhz)
                                .unwrap_or(60000);
                            rates.insert(idx as u32, refresh / 1000);
                        }
                    }
                }
            }
        }
    }

    rates
}

impl<T> X11RandrOps for T
where
    T: Connection + RequestConnection,
{
    fn query_monitor_rects(&self, root: u32) -> Vec<(u32, i32, i32, u32, u32)> {
        build_monitor_rects_from_randr(self, root)
    }

    fn query_monitor_refresh_rates(&self, root: u32) -> HashMap<u32, u32> {
        build_monitor_refresh_rates_from_randr(self, root)
    }
}
