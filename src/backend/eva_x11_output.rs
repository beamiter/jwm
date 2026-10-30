//! Read-only X11 presentation evidence for Eva. A live GL context alone does
//! not prove that a host window is mapped or that DPMS permits scanout.
use x11rb::{
    connection::RequestConnection,
    protocol::{
        dpms, randr,
        xproto::{self, ConnectionExt as _},
    },
    rust_connection::RustConnection,
};

pub(crate) struct EvaX11OutputProbe {
    connection: Option<RustConnection>,
    dpms_extension: Option<bool>,
}
impl EvaX11OutputProbe {
    pub(crate) fn new() -> Self {
        Self::for_display(None)
    }
    fn for_display(display: Option<&str>) -> Self {
        let connection = xbar_core::x11_connection::connect(display)
            .ok()
            .map(|(connection, _)| connection);
        let dpms_extension = connection.as_ref().and_then(|connection| {
            connection
                .extension_information(dpms::X11_EXTENSION_NAME)
                .ok()
                .map(|extension| extension.is_some())
        });
        Self {
            connection,
            dpms_extension,
        }
    }
    pub(crate) fn visible(&self, window: Option<u32>) -> bool {
        let Some(connection) = self.connection.as_ref() else {
            return false;
        };
        let capable = match self.dpms_extension {
            Some(false) => Some(false),
            Some(true) => dpms::capable(connection)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .map(|reply| reply.capable),
            None => None,
        };
        let power = if capable == Some(true) {
            dpms::info(connection)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .map(|reply| (reply.state, reply.power_level))
        } else {
            None
        };
        if !power_visible(capable, power) {
            return false;
        }
        window.is_none_or(|window| self.host_window_visible(connection, window))
    }
    fn host_window_visible(&self, connection: &RustConnection, window: u32) -> bool {
        let Some(attributes) = connection
            .get_window_attributes(window)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
        else {
            return false;
        };
        if attributes.map_state != xproto::MapState::VIEWABLE {
            return false;
        }
        let Some(geometry) = connection
            .get_geometry(window)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
        else {
            return false;
        };
        let Some(position) = connection
            .translate_coordinates(window, geometry.root, 0, 0)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
        else {
            return false;
        };
        let rect = [
            position.dst_x as f32,
            position.dst_y as f32,
            geometry.width as f32,
            geometry.height as f32,
        ];
        let outputs = match connection.extension_information(randr::X11_EXTENSION_NAME) {
            Ok(None) => connection
                .get_geometry(geometry.root)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .map(|root| vec![[0.0, 0.0, root.width as f32, root.height as f32]]),
            Ok(Some(_)) => {
                if let Some(monitors) = randr::get_monitors(connection, geometry.root, true)
                    .ok()
                    .and_then(|cookie| cookie.reply().ok())
                {
                    Some(
                        monitors
                            .monitors
                            .iter()
                            .map(|o| [o.x as f32, o.y as f32, o.width as f32, o.height as f32])
                            .collect(),
                    )
                } else {
                    let resources = randr::get_screen_resources_current(connection, geometry.root)
                        .ok()
                        .and_then(|cookie| cookie.reply().ok());
                    resources.map(|resources| {
                        resources
                            .crtcs
                            .iter()
                            .filter_map(|crtc| {
                                randr::get_crtc_info(connection, *crtc, resources.config_timestamp)
                                    .ok()
                                    .and_then(|cookie| cookie.reply().ok())
                            })
                            .filter(|crtc| crtc.mode != 0)
                            .map(|crtc| {
                                [
                                    crtc.x as f32,
                                    crtc.y as f32,
                                    crtc.width as f32,
                                    crtc.height as f32,
                                ]
                            })
                            .collect()
                    })
                }
            }
            Err(_) => None,
        };
        outputs.is_some_and(|outputs: Vec<[f32; 4]>| {
            outputs.into_iter().any(|output| {
                crate::backend::compositor_common::eva_overlay::eva_viewport_on_output(rect, output)
            })
        })
    }
}

fn power_visible(capable: Option<bool>, reply: Option<(bool, dpms::DPMSMode)>) -> bool {
    match capable {
        Some(false) => true,
        Some(true) => reply.is_some_and(|(enabled, level)| !enabled || level == dpms::DPMSMode::ON),
        None => false,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(any(
        feature = "backend-x11rb",
        feature = "backend-xcb",
        feature = "wayland-backends"
    ))]
    use x11rb::connection::Connection;
    #[test]
    fn eva_dpms_off_and_unknown_are_not_presentation_evidence() {
        assert!(power_visible(Some(false), None));
        assert!(!power_visible(None, None));
        assert!(!power_visible(Some(true), None));
        assert!(power_visible(Some(true), Some((true, dpms::DPMSMode::ON))));
        for level in [
            dpms::DPMSMode::STANDBY,
            dpms::DPMSMode::SUSPEND,
            dpms::DPMSMode::OFF,
        ] {
            assert!(!power_visible(Some(true), Some((true, level))));
            assert!(power_visible(Some(true), Some((false, level))));
        }
    }
    #[cfg(any(
        feature = "backend-x11rb",
        feature = "backend-xcb",
        feature = "wayland-backends"
    ))]
    #[test]
    fn eva_native_x11_hidden_and_destroyed_hosts_never_authorize_capture() {
        let x11 = crate::backend::clipboard_offer::IsolatedXvfb::acquire();
        let (connection, screen) = x11rb::connect(Some(x11.name())).unwrap();
        let window = connection.generate_id().unwrap();
        connection
            .create_window(
                0,
                window,
                connection.setup().roots[screen].root,
                0,
                0,
                64,
                64,
                0,
                xproto::WindowClass::INPUT_OUTPUT,
                0,
                &xproto::CreateWindowAux::new(),
            )
            .unwrap()
            .check()
            .unwrap();
        let probe = EvaX11OutputProbe::for_display(Some(x11.name()));
        assert!(!probe.visible(Some(window)));
        connection.map_window(window).unwrap().check().unwrap();
        connection.get_input_focus().unwrap().reply().unwrap();
        assert!(probe.visible(Some(window)));
        connection
            .configure_window(window, &xproto::ConfigureWindowAux::new().x(30000).y(30000))
            .unwrap()
            .check()
            .unwrap();
        connection.get_input_focus().unwrap().reply().unwrap();
        assert!(
            !probe.visible(Some(window)),
            "mapped but offscreen host cannot display the microphone indicator"
        );
        connection
            .configure_window(window, &xproto::ConfigureWindowAux::new().x(0).y(0))
            .unwrap()
            .check()
            .unwrap();
        connection.get_input_focus().unwrap().reply().unwrap();
        assert!(probe.visible(Some(window)));
        connection.unmap_window(window).unwrap().check().unwrap();
        connection.get_input_focus().unwrap().reply().unwrap();
        assert!(!probe.visible(Some(window)));
        connection.destroy_window(window).unwrap().check().unwrap();
        connection.get_input_focus().unwrap().reply().unwrap();
        assert!(!probe.visible(Some(window)));
    }
}
