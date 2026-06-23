//! zbus implementation of `org.freedesktop.impl.portal.ScreenCast`.
//!
//! This interface is the *backend* one (`impl.portal.ScreenCast`), called by
//! `xdg-desktop-portal` after it has done its own UI/permissions dance.
//! Methods get a request `handle`, options dict, and must return a tuple
//! `(response, results)` where `response = 0` means success.

use std::collections::HashMap;

use enumflags2::{BitFlags, bitflags};
use log::{info, warn};
use zbus::{Connection, interface, zvariant::OwnedObjectPath, zvariant::OwnedValue, zvariant::Value};

use crate::picker::{SourceSelection, pick_outputs, pick_windows};
use crate::pipewire_stream::{self, StreamSpec};
use crate::session::Runtime;

/// Source type bits per the portal spec.
#[bitflags]
#[repr(u32)]
#[derive(Copy, Clone, Debug)]
pub enum SourceType {
    Monitor = 1,
    Window = 2,
    Virtual = 4,
}

/// Cursor mode bits per the portal spec.
#[bitflags]
#[repr(u32)]
#[derive(Copy, Clone, Debug)]
pub enum CursorMode {
    Hidden = 1,
    Embedded = 2,
    Metadata = 4,
}

struct ScreenCast {
    rt: Runtime,
}

#[interface(name = "org.freedesktop.impl.portal.ScreenCast")]
impl ScreenCast {
    #[zbus(property)]
    fn version(&self) -> u32 {
        4
    }

    #[zbus(property, name = "AvailableSourceTypes")]
    fn available_source_types(&self) -> u32 {
        BitFlags::from(SourceType::Monitor | SourceType::Window).bits()
    }

    #[zbus(property, name = "AvailableCursorModes")]
    fn available_cursor_modes(&self) -> u32 {
        BitFlags::from(CursorMode::Embedded).bits()
    }

    async fn create_session(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        _app_id: String,
        _options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        info!("CreateSession {session_handle}");
        self.rt.insert_session(session_handle.to_string()).await;
        (0, HashMap::new())
    }

    async fn select_sources(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        _app_id: String,
        options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        info!("SelectSources {session_handle} options={:?}", options.keys().collect::<Vec<_>>());

        let types_mask = options
            .get("types")
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(SourceType::Monitor as u32);
        let multiple = options
            .get("multiple")
            .and_then(|v| bool::try_from(v).ok())
            .unwrap_or(false);
        let want_monitor = types_mask & (SourceType::Monitor as u32) != 0;
        let want_window = types_mask & (SourceType::Window as u32) != 0;

        let (outputs, toplevels) = {
            let snap = self.rt.wayland().lock().expect("wayland snapshot mutex");
            (snap.outputs.clone(), snap.toplevels.clone())
        };
        let selection = SourceSelection {
            outputs: if want_monitor {
                pick_outputs(&outputs, multiple)
            } else {
                Vec::new()
            },
            toplevels: if want_window {
                pick_windows(&toplevels, multiple)
            } else {
                Vec::new()
            },
        };
        info!(
            "SelectSources picked {} output(s), {} toplevel(s)",
            selection.outputs.len(),
            selection.toplevels.len()
        );

        let _ = self
            .rt
            .with_session(&session_handle.to_string(), |s| {
                s.selection = Some(selection);
            })
            .await;
        (0, HashMap::new())
    }

    async fn start(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        _app_id: String,
        _parent_window: String,
        _options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        info!("Start {session_handle}");

        // Pull the resolved selection out of the session — SelectSources put it
        // there. Without it, this is a no-op-but-success Start.
        let selection = self
            .rt
            .with_session(&session_handle.to_string(), |s| s.selection.clone())
            .await
            .flatten()
            .unwrap_or_default();

        // Spin up one PipeWire producer per picked source. Each handle is
        // held on the Session so its worker thread + PW node stay alive.
        let mut streams_meta: Vec<(u32, HashMap<String, OwnedValue>)> = Vec::new();
        let mut handles = Vec::new();
        for (idx, o) in selection.outputs.iter().enumerate() {
            let spec = StreamSpec {
                width: o.width.max(1) as u32,
                height: o.height.max(1) as u32,
                framerate_num: if o.refresh_mhz > 0 { (o.refresh_mhz / 1000) as u32 } else { 60 },
                framerate_den: 1,
            };
            let name = if o.name.is_empty() {
                format!("jwm-output-{idx}")
            } else {
                format!("jwm-output-{}", o.name)
            };
            match pipewire_stream::spawn(spec, name) {
                Ok(h) => {
                    let mut props: HashMap<String, OwnedValue> = HashMap::new();
                    if let Ok(v) = Value::from((spec.width as i32, spec.height as i32)).try_into() {
                        props.insert("size".to_string(), v);
                    }
                    if let Ok(v) = Value::from(SourceType::Monitor as u32).try_into() {
                        props.insert("source_type".to_string(), v);
                    }
                    streams_meta.push((h.node_id, props));
                    handles.push(h);
                }
                Err(e) => warn!("Start: failed to spawn output stream: {e}"),
            }
        }
        for (idx, t) in selection.toplevels.iter().enumerate() {
            let spec = StreamSpec::default();
            let name = if t.app_id.is_empty() {
                format!("jwm-window-{idx}")
            } else {
                format!("jwm-window-{}", t.app_id)
            };
            match pipewire_stream::spawn(spec, name) {
                Ok(h) => {
                    let mut props: HashMap<String, OwnedValue> = HashMap::new();
                    if let Ok(v) = Value::from((spec.width as i32, spec.height as i32)).try_into() {
                        props.insert("size".to_string(), v);
                    }
                    if let Ok(v) = Value::from(SourceType::Window as u32).try_into() {
                        props.insert("source_type".to_string(), v);
                    }
                    streams_meta.push((h.node_id, props));
                    handles.push(h);
                }
                Err(e) => warn!("Start: failed to spawn window stream: {e}"),
            }
        }

        let _ = self
            .rt
            .with_session(&session_handle.to_string(), |s| {
                s.streams = handles;
            })
            .await;

        let mut results = HashMap::new();
        if let Ok(v) = Value::from(streams_meta).try_into() {
            results.insert("streams".to_string(), v);
        }
        info!("Start {session_handle} returning streams");
        (0, results)
    }

    async fn open_pipe_wire_remote(
        &self,
        session_handle: OwnedObjectPath,
        _options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<zbus::zvariant::OwnedFd> {
        info!("OpenPipeWireRemote {session_handle}");
        // MVP placeholder: connect to the user PipeWire daemon and hand a
        // socket fd back. Until pipewire_stream.rs is wired, refuse.
        Err(zbus::fdo::Error::NotSupported(
            "OpenPipeWireRemote not yet implemented".into(),
        ))
    }
}

pub async fn serve(rt: Runtime) -> Result<Connection, Box<dyn std::error::Error + Send + Sync>> {
    let conn = zbus::connection::Builder::session()?
        .name("org.freedesktop.impl.portal.desktop.jwm")?
        .serve_at("/org/freedesktop/portal/desktop", ScreenCast { rt })?
        .build()
        .await?;
    info!("D-Bus service registered as org.freedesktop.impl.portal.desktop.jwm");
    Ok(conn)
}
