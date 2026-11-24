//! Inject a bounded finger-scroll gesture into a private compositor fixture.
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
    let mut args = env::args_os().skip(1);
    let socket = args.next().map(PathBuf::from).ok_or("missing socket")?;
    let zoom = match args.next().as_deref().and_then(std::ffi::OsStr::to_str) {
        Some("pan") => false,
        Some("zoom") => true,
        _ => return Err("expected pan or zoom mode".into()),
    };
    let (x, y) = if zoom {
        let mut coordinate = || -> Result<u32, Box<dyn std::error::Error>> {
            Ok(args
                .next()
                .and_then(|arg| arg.into_string().ok())
                .ok_or("missing anchor coordinate")?
                .parse()?)
        };
        (coordinate()?, coordinate()?)
    } else {
        (640, 485)
    };
    if args.next().is_some() || x >= 1280 || y >= 900 {
        return Err("invalid fixture anchor".into());
    }
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
    pointer.motion_absolute(1, x, y, 1280, 900);
    pointer.frame();
    queue.roundtrip(&mut state)?;
    // Older wlroots applies source to the most recently queued axis.
    if !zoom {
        pointer.axis(2, Axis::HorizontalScroll, 48.);
        pointer.axis_source(AxisSource::Finger);
    }
    pointer.axis(2, Axis::VerticalScroll, -30.);
    pointer.axis_source(AxisSource::Finger);
    pointer.frame();
    queue.roundtrip(&mut state)?;
    if !zoom {
        pointer.axis_stop(3, Axis::HorizontalScroll);
        pointer.axis_source(AxisSource::Finger);
    }
    pointer.axis_stop(3, Axis::VerticalScroll);
    pointer.axis_source(AxisSource::Finger);
    pointer.frame();
    queue.roundtrip(&mut state)?;
    Ok(())
}
