/// wlr-virtual-pointer-unstable-v1 protocol implementation.
///
/// Allows wayvnc and other remote desktop tools to inject pointer events.

use log::info;

use smithay::reexports::wayland_protocols_wlr::virtual_pointer::v1::server::{
    zwlr_virtual_pointer_manager_v1::{self, ZwlrVirtualPointerManagerV1},
    zwlr_virtual_pointer_v1::{self, ZwlrVirtualPointerV1},
};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New,
};

use crate::backend::wayland::state::JwmWaylandState;

pub struct VirtualPointerManagerData;
unsafe impl Send for VirtualPointerManagerData {}

pub struct VirtualPointerData;
unsafe impl Send for VirtualPointerData {}

pub fn init_virtual_pointer_manager(dh: &DisplayHandle) {
    dh.create_global::<JwmWaylandState, ZwlrVirtualPointerManagerV1, _>(2, VirtualPointerManagerData);
    info!("[udev/wayland] zwlr-virtual-pointer-unstable-v1 global registered");
}

impl GlobalDispatch<ZwlrVirtualPointerManagerV1, VirtualPointerManagerData> for JwmWaylandState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrVirtualPointerManagerV1>,
        _global_data: &VirtualPointerManagerData,
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, VirtualPointerManagerData);
    }
}

impl Dispatch<ZwlrVirtualPointerManagerV1, VirtualPointerManagerData> for JwmWaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZwlrVirtualPointerManagerV1,
        request: zwlr_virtual_pointer_manager_v1::Request,
        _data: &VirtualPointerManagerData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_virtual_pointer_manager_v1::Request::CreateVirtualPointer { seat: _, id } => {
                data_init.init(id, VirtualPointerData);
            }
            zwlr_virtual_pointer_manager_v1::Request::CreateVirtualPointerWithOutput { seat: _, output: _, id } => {
                data_init.init(id, VirtualPointerData);
            }
            zwlr_virtual_pointer_manager_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ZwlrVirtualPointerV1, VirtualPointerData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwlrVirtualPointerV1,
        request: zwlr_virtual_pointer_v1::Request,
        _data: &VirtualPointerData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_virtual_pointer_v1::Request::Motion { time: _, dx, dy } => {
                state.pointer_location.x += dx;
                state.pointer_location.y += dy;
                state.needs_redraw = true;
            }
            zwlr_virtual_pointer_v1::Request::MotionAbsolute { time: _, x, y, x_extent, y_extent } => {
                if x_extent > 0 && y_extent > 0 {
                    let nx = (x as f64) / (x_extent as f64);
                    let ny = (y as f64) / (y_extent as f64);
                    if let Some(rect) = state.output_rects.first() {
                        state.pointer_location.x = rect.loc.x as f64 + nx * rect.size.w as f64;
                        state.pointer_location.y = rect.loc.y as f64 + ny * rect.size.h as f64;
                    }
                }
                state.needs_redraw = true;
            }
            zwlr_virtual_pointer_v1::Request::Button { time: _, button: _, state: _ } => {
                // Input events should be routed through the seat's pointer.
                // For now, we accept the global but don't inject events into the input pipeline.
            }
            zwlr_virtual_pointer_v1::Request::Axis { .. } => {}
            zwlr_virtual_pointer_v1::Request::Frame => {}
            zwlr_virtual_pointer_v1::Request::AxisSource { .. } => {}
            zwlr_virtual_pointer_v1::Request::AxisStop { .. } => {}
            zwlr_virtual_pointer_v1::Request::AxisDiscrete { .. } => {}
            zwlr_virtual_pointer_v1::Request::Destroy => {}
            _ => {}
        }
    }
}
