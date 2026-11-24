//! Inject one fixed two-axis gesture into a private compositor fixture.
use std::{env, os::unix::net::UnixStream, path::PathBuf, thread, time::Duration};
use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_pointer::{Axis, AxisSource},
        wl_registry,
    },
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

struct Fixture;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Fixture {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(Fixture: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(Fixture: ignore ZwlrVirtualPointerV1);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let socket = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("missing socket")?;
    if !socket.is_absolute() {
        return Err("expected the absolute private fixture socket".into());
    }
    let connection = Connection::from_socket(UnixStream::connect(socket)?)?;
    let (globals, mut queue) = registry_queue_init::<Fixture>(&connection)?;
    let handle = queue.handle();
    let manager: ZwlrVirtualPointerManagerV1 = globals.bind(&handle, 1..=1, ())?;
    let pointer = manager.create_virtual_pointer(None, &handle, ());
    let mut state = Fixture;
    queue.roundtrip(&mut state)?;
    // The application first needs to bind the seat's new pointer capability.
    thread::sleep(Duration::from_millis(250));
    pointer.motion_absolute(1, 640, 485, 1280, 900);
    pointer.frame();
    queue.roundtrip(&mut state)?;
    pointer.axis_source(AxisSource::Finger);
    pointer.axis(2, Axis::HorizontalScroll, 48.);
    pointer.axis(2, Axis::VerticalScroll, -30.);
    pointer.frame();
    queue.roundtrip(&mut state)?;
    pointer.axis_source(AxisSource::Finger);
    pointer.axis_stop(3, Axis::HorizontalScroll);
    pointer.axis_stop(3, Axis::VerticalScroll);
    pointer.frame();
    queue.roundtrip(&mut state)?;
    Ok(())
}
