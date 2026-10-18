//! X11 dock hints shared by the tao and winit presentation adapters.
//!
//! Included as a module so each binary uses its existing x11rb dependency.
//! Wayland handles are left to their compositor. JWM owns reservations for
//! managed bars: only it knows output padding and whether an edge is interior.

use anyhow::Result;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, PropMode};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use xbar_core::{BarEdge, BarPlacement, DockPropertyValue, DockWindowSpec, MonitorGeometry};

pub struct DockHints {
    connection: Option<(RustConnection, usize)>,
    managed: bool,
    last: Option<(u32, BarPlacement, MonitorGeometry, BarEdge)>,
}

impl Default for DockHints {
    fn default() -> Self {
        Self {
            connection: None,
            // All native bar binaries accept the shared transport path as
            // their final argument. Decide ownership before the first IPC
            // snapshot, so late transport attachment cannot clobber struts.
            managed: std::env::args()
                .skip(1)
                .last()
                .is_some_and(|path| !path.is_empty()),
            last: None,
        }
    }
}

impl DockHints {
    pub fn sync(
        &mut self,
        window: &impl HasWindowHandle,
        placement: BarPlacement,
        edge: BarEdge,
        title: &str,
    ) -> Result<()> {
        let xid = match window.window_handle()?.as_raw() {
            RawWindowHandle::Xlib(handle) => u32::try_from(handle.window)?,
            RawWindowHandle::Xcb(handle) => handle.window.get(),
            _ => return Ok(()),
        };
        if self.connection.is_none() {
            self.connection = Some(x11rb::connect(None)?);
        }
        let (connection, screen_number) = self.connection.as_ref().expect("X11 connection");
        let screen = &connection.setup().roots[*screen_number];
        // setup() dimensions are a connection-time snapshot. Query the root
        // again when geometry changes so hotplug also updates bottom/right.
        let geometry = connection.get_geometry(screen.root)?.reply()?;
        let root = MonitorGeometry {
            x: 0,
            y: 0,
            width: u32::from(geometry.width),
            height: u32::from(geometry.height),
        };
        let next = (xid, placement, root, edge);
        if self.last == Some(next) {
            return Ok(());
        }
        let initial = self.last.as_ref().is_none_or(|last| last.0 != xid);
        let spec = DockWindowSpec::for_edge(title, placement, root, edge);
        let properties = if initial {
            spec.properties()
        } else {
            spec.strut_properties().to_vec()
        };
        for property in properties {
            if self.managed && property.name.starts_with("_NET_WM_STRUT") {
                continue;
            }
            let name = connection
                .intern_atom(false, property.name.as_bytes())?
                .reply()?
                .atom;
            match property.value {
                DockPropertyValue::Atoms(names) => {
                    let atoms = names
                        .into_iter()
                        .map(|name| {
                            Ok(connection
                                .intern_atom(false, name.as_bytes())?
                                .reply()?
                                .atom)
                        })
                        .collect::<Result<Vec<_>>>()?;
                    connection
                        .change_property32(PropMode::REPLACE, xid, name, AtomEnum::ATOM, &atoms)?
                        .check()?;
                }
                DockPropertyValue::Cardinals(values) => {
                    connection
                        .change_property32(
                            PropMode::REPLACE,
                            xid,
                            name,
                            AtomEnum::CARDINAL,
                            &values,
                        )?
                        .check()?;
                }
                DockPropertyValue::Utf8Text(value) => {
                    let utf8 = connection.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
                    connection
                        .change_property8(PropMode::REPLACE, xid, name, utf8, value.as_bytes())?
                        .check()?;
                }
            }
        }
        connection.flush()?;
        self.last = Some(next);
        Ok(())
    }
}
