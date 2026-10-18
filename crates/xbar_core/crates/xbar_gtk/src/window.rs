//! Native geometry and X11 dock hints shared by the GTK4 and Relm frontends.
//! GTK4's public window API only requests a size. On X11 a separate connection
//! sets the root-relative origin; on Wayland JWM/compositor owns positioning.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::prelude::*;
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{AtomEnum, ConfigureWindowAux, ConnectionExt as _, PropMode};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use xbar_core::presentation::PresentationConfig;
use xbar_core::{BarPlacement, DockPropertyValue, DockWindowSpec, MonitorGeometry};

type NativeError = Box<dyn std::error::Error>;

struct Inner {
    window: gtk4::Window,
    config: PresentationConfig,
    title: String,
    managed: bool,
    geometry: Cell<Option<MonitorGeometry>>,
    connection: RefCell<Option<(RustConnection, usize)>>,
    announced: Cell<bool>,
    applying: Cell<bool>,
}

/// Retained geometry policy; a late realize or DPI change reapplies the same
/// edge without deriving a second offset from JWM's already placed snapshot.
#[derive(Clone)]
pub struct BarWindowPlacement(Rc<Inner>);

impl BarWindowPlacement {
    pub fn new(
        window: &impl IsA<gtk4::Window>,
        config: &PresentationConfig,
        title: &str,
        managed: bool,
    ) -> Self {
        let inner = Rc::new(Inner {
            window: window.as_ref().clone(),
            config: config.clone(),
            title: title.to_owned(),
            managed,
            geometry: Cell::new(None),
            connection: RefCell::new(None),
            announced: Cell::new(false),
            applying: Cell::new(false),
        });
        inner.window.connect_realize({
            let weak = Rc::downgrade(&inner);
            move |_| {
                if let Some(inner) = weak.upgrade() {
                    inner.apply();
                }
            }
        });
        inner.window.connect_map({
            let weak = Rc::downgrade(&inner);
            move |_| {
                // GDK publishes its initial toplevel state while mapping and
                // may replace the desktop/state properties set on realize.
                // Reassert our dock contract once that map cycle completes.
                let weak = weak.clone();
                gtk4::glib::idle_add_local_once(move || {
                    if let Some(inner) = weak.upgrade() {
                        inner.announced.set(false);
                        inner.apply();
                    }
                });
            }
        });
        inner.window.connect_scale_factor_notify({
            let weak = Rc::downgrade(&inner);
            move |_| {
                if let Some(inner) = weak.upgrade() {
                    inner.apply();
                }
            }
        });
        let placement = Self(inner);
        placement.refresh();
        placement
    }

    pub fn apply_geometry(&self, geometry: Option<MonitorGeometry>) {
        self.0.geometry.set(geometry);
        self.refresh();
    }

    pub fn refresh(&self) {
        self.0.apply();
    }
}

impl Inner {
    fn placement(&self) -> Result<BarPlacement, xbar_core::placement::PlacementError> {
        let scale = f64::from(self.window.scale_factor().max(1));
        let thickness = self.config.logical_thickness(scale);
        if let Some(geometry) = self.geometry.get() {
            return BarPlacement::from_bar_geometry(geometry, thickness, scale, self.config.edge);
        }
        let monitor = self
            .window
            .surface()
            .and_then(|surface| {
                gtk4::prelude::WidgetExt::display(&self.window).monitor_at_surface(&surface)
            })
            .or_else(super::primary_monitor);
        let geometry = monitor
            .map(|monitor| {
                let rect = monitor.geometry();
                let scale = monitor.scale_factor().max(1);
                MonitorGeometry {
                    x: rect.x().saturating_mul(scale),
                    y: rect.y().saturating_mul(scale),
                    width: rect.width().saturating_mul(scale).max(1) as u32,
                    height: rect.height().saturating_mul(scale).max(1) as u32,
                }
            })
            .unwrap_or(MonitorGeometry {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            });
        BarPlacement::at_edge(geometry, thickness, scale, self.config.edge)
    }

    fn apply(&self) {
        if self.applying.replace(true) {
            return;
        }
        let result = self.placement().map(|placement| {
            let scale = f64::from(self.window.scale_factor().max(1));
            let (width, height) = super::logical_placement_size(placement, scale);
            self.window.set_default_size(width, height);
            if let Err(error) = self.apply_x11(placement) {
                log::warn!("could not apply GTK dock placement: {error}");
            }
        });
        if let Err(error) = result {
            log::warn!("ignoring invalid GTK bar geometry: {error}");
        }
        self.applying.set(false);
    }

    fn apply_x11(&self, placement: BarPlacement) -> Result<(), NativeError> {
        let Some(surface) = self.window.surface() else {
            return Ok(());
        };
        let Some(surface) = surface.downcast_ref::<gdk4_x11::X11Surface>() else {
            return Ok(());
        };
        // The hints use a second X connection. Finish GDK's pending requests
        // first so initial toplevel properties cannot race this update.
        gtk4::prelude::WidgetExt::display(&self.window).sync();
        let xid = u32::try_from(surface.xid())?;
        let mut connection = self.connection.borrow_mut();
        if connection.is_none() {
            *connection = Some(x11rb::connect(Some(
                gtk4::prelude::WidgetExt::display(&self.window)
                    .name()
                    .as_str(),
            ))?);
        }
        let (connection, screen_number) = connection.as_ref().expect("connected X11 display");
        let root = connection.setup().roots[*screen_number].root;
        let geometry = connection.get_geometry(root)?.reply()?;
        let root = MonitorGeometry {
            x: 0,
            y: 0,
            width: u32::from(geometry.width),
            height: u32::from(geometry.height),
        };
        connection
            .configure_window(
                xid,
                &ConfigureWindowAux::new()
                    .x(placement.x)
                    .y(placement.y)
                    .width(placement.width)
                    .height(placement.height),
            )?
            .check()?;
        let spec = DockWindowSpec::for_edge(&self.title, placement, root, self.config.edge);
        let properties = if self.announced.get() {
            spec.strut_properties().to_vec()
        } else {
            spec.properties()
        };
        for property in properties {
            // JWM alone knows the reserved inner gap and which monitor edges
            // are interior. Never replace that policy with root-wide struts.
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
                        .collect::<Result<Vec<_>, NativeError>>()?;
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
                DockPropertyValue::Utf8Text(text) => {
                    let utf8 = connection.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
                    connection
                        .change_property8(PropMode::REPLACE, xid, name, utf8, text.as_bytes())?
                        .check()?;
                }
            }
        }
        connection.flush()?;
        self.announced.set(true);
        Ok(())
    }
}
