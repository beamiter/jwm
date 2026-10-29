//! XWayland for isolated windowed compositor sessions. The backend's child
//! environment owns DISPLAY; the process/host session environment is untouched.
use crate::{
    backend::{error::BackendError, wayland::state::JwmWaylandState},
    child_environment::ChildEnvironment,
};
use smithay::{
    reexports::{calloop::LoopHandle, wayland_server::DisplayHandle},
    xwayland::{X11Wm, XWayland, XWaylandEvent},
};

pub(crate) fn start_nested(
    display: &DisplayHandle,
    loop_handle: LoopHandle<'static, JwmWaylandState>,
    child_environment: ChildEnvironment,
) -> Result<(), BackendError> {
    // An outer DISPLAY is a rendering host, never the nested clients' server.
    child_environment.remove("DISPLAY");
    let (xwayland, client) = XWayland::spawn(
        display,
        None,
        std::iter::empty::<(String, String)>(),
        std::iter::empty::<String>(),
        true,
        std::process::Stdio::null(),
        std::process::Stdio::null(),
        |_| {},
    )
    .map_err(|error| BackendError::Message(format!("nested XWayland spawn failed: {error}")))?;
    let wm_loop = loop_handle.clone();
    loop_handle
        .insert_source(xwayland, move |event, _, state| match event {
            XWaylandEvent::Ready {
                x11_socket,
                display_number,
            } => {
                log::info!("[nested-xwayland] ready on DISPLAY=:{display_number}");
                let display = state.display_handle.clone();
                match X11Wm::start_wm(wm_loop.clone(), &display, x11_socket, client.clone()) {
                    Ok(wm) => {
                        state.x11_wm = Some(wm);
                        child_environment.set("DISPLAY", format!(":{display_number}"));
                    }
                    Err(error) => {
                        log::error!("nested XWayland WM initialization failed: {error:?}")
                    }
                }
            }
            XWaylandEvent::Error => {
                child_environment.remove("DISPLAY");
                log::error!("nested XWayland exited");
            }
        })
        .map_err(|error| {
            BackendError::Message(format!("nested XWayland event source failed: {error}"))
        })?;
    Ok(())
}
