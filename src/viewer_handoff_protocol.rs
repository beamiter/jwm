//! Private JWM-only bounded viewer focus and shortcut handoff.
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
    use wayland_client::protocol::*;
    pub mod __interfaces {
        use wayland_client::backend as wayland_backend;
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/jwm-remote-viewer-handoff-v1.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("protocols/jwm-remote-viewer-handoff-v1.xml");
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
    use smithay::reexports::wayland_server;
    use smithay::reexports::wayland_server::protocol::*;
    pub mod __interfaces {
        use smithay::reexports::wayland_server::backend as wayland_backend;
        use smithay::reexports::wayland_server::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/jwm-remote-viewer-handoff-v1.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_server_code!("protocols/jwm-remote-viewer-handoff-v1.xml");
}
