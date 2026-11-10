//! Private trusted-local control capability protocol shared by the compositor
//! and its explicit opt-in remote host. Never an unrestricted virtual control.
#[cfg(feature = "remote-wayland")]
#[allow(
    dead_code,
    non_camel_case_types,
    unused_unsafe,
    unused_variables,
    non_upper_case_globals,
    non_snake_case,
    unused_imports,
    missing_docs,
    clippy::all
)]
pub mod client {
    use wayland_client;
    use wayland_protocols::ext::image_capture_source::v1::client::*;
    use wayland_protocols::ext::image_copy_capture::v1::client::*;
    pub mod __interfaces {
        use wayland_client::backend as wayland_backend;
        use wayland_protocols::ext::image_capture_source::v1::client::__interfaces::*;
        use wayland_protocols::ext::image_copy_capture::v1::client::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/jwm-readonly-resize-capture-v1.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("protocols/jwm-readonly-resize-capture-v1.xml");
}
#[cfg(feature = "wayland-backends")]
#[allow(
    dead_code,
    non_camel_case_types,
    unused_unsafe,
    unused_variables,
    non_upper_case_globals,
    non_snake_case,
    unused_imports,
    missing_docs,
    clippy::all
)]
pub mod server {
    use smithay::reexports::wayland_protocols::ext::image_capture_source::v1::server::*;
    use smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::*;
    use smithay::reexports::wayland_server;
    pub mod __interfaces {
        use smithay::reexports::wayland_server::backend as wayland_backend;
        use smithay::reexports::wayland_protocols::ext::image_capture_source::v1::server::__interfaces::*;
        use smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/jwm-readonly-resize-capture-v1.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_server_code!("protocols/jwm-readonly-resize-capture-v1.xml");
}
