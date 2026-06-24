//! Wayland client. Binds the globals advertised by jwm and tracks live
//! outputs + toplevels so the picker can resolve env-var matches.
//!
//! The capture-session machinery (image-copy-capture frame lifecycle, dmabuf
//! import, PipeWire bridge) is intentionally not in this module yet — this is
//! the enumeration half. [`crate::capture`] will reuse the resolved
//! `WlOutput` / `ExtForeignToplevelHandleV1` handles stored here.

use std::sync::{Arc, Mutex};

use log::{debug, info, warn};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_output, wl_registry},
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1,
    ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1;

#[derive(Debug, Default, Clone)]
pub struct OutputInfo {
    pub name: String,
    pub description: String,
    pub width: i32,
    pub height: i32,
    pub refresh_mhz: i32,
}

#[derive(Debug, Default, Clone)]
pub struct ToplevelInfo {
    pub identifier: String,
    pub app_id: String,
    pub title: String,
}

/// Live snapshot the D-Bus thread reads when answering SelectSources. The
/// per-proxy handles are kept on the Wayland thread (see [`Client`]).
#[derive(Default)]
pub struct WaylandSnapshot {
    pub outputs: Vec<OutputInfo>,
    pub toplevels: Vec<ToplevelInfo>,
}

pub type SharedSnapshot = Arc<Mutex<WaylandSnapshot>>;

/// Owned by the Wayland thread. Stores live proxy handles + the manager
/// globals needed to start a capture session for either source kind.
struct Client {
    snapshot: SharedSnapshot,
    /// `(registry_name, proxy, info)` per advertised wl_output. The
    /// registry_name lets us drop entries on GlobalRemove without scanning.
    outputs: Vec<(u32, wl_output::WlOutput, OutputInfo)>,
    toplevels: Vec<(ExtForeignToplevelHandleV1, ToplevelInfo)>,
    // Manager globals — kept alive for the process lifetime and referenced
    // by the (not-yet-wired) capture session paths.
    _toplevel_list: Option<ExtForeignToplevelListV1>,
    _output_source_mgr: Option<ExtOutputImageCaptureSourceManagerV1>,
    _toplevel_source_mgr: Option<ExtForeignToplevelImageCaptureSourceManagerV1>,
    _capture_mgr: Option<ExtImageCopyCaptureManagerV1>,
}

impl Client {
    fn publish(&self) {
        let outputs = self.outputs.iter().map(|(_, _, i)| i.clone()).collect();
        let toplevels = self.toplevels.iter().map(|(_, i)| i.clone()).collect();
        let mut guard = self.snapshot.lock().expect("snapshot mutex poisoned");
        guard.outputs = outputs;
        guard.toplevels = toplevels;
    }
}

/// Spawn the Wayland client thread. Returns the shared snapshot handle and a
/// shutdown signal (drop to stop the thread).
pub fn spawn() -> std::io::Result<(SharedSnapshot, std::sync::mpsc::Sender<()>)> {
    let snapshot: SharedSnapshot = Arc::new(Mutex::new(WaylandSnapshot::default()));
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let snap_for_thread = snapshot.clone();
    std::thread::Builder::new()
        .name("jwm-portal-wayland".into())
        .spawn(move || {
            if let Err(e) = run_event_loop(snap_for_thread, rx) {
                warn!("jwm-portal wayland thread exited: {e}");
            }
        })?;
    Ok((snapshot, tx))
}

fn run_event_loop(
    snapshot: SharedSnapshot,
    shutdown: std::sync::mpsc::Receiver<()>,
) -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut event_queue): (_, EventQueue<Client>) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    let toplevel_list = globals
        .bind::<ExtForeignToplevelListV1, _, _>(&qh, 1..=1, ())
        .map_err(|e| {
            warn!("jwm-portal: ext_foreign_toplevel_list_v1 not advertised: {e}");
            e
        })
        .ok();
    let output_source_mgr = globals
        .bind::<ExtOutputImageCaptureSourceManagerV1, _, _>(&qh, 1..=1, ())
        .map_err(|e| {
            warn!("jwm-portal: ext_output_image_capture_source_manager_v1 not advertised: {e}");
            e
        })
        .ok();
    let toplevel_source_mgr = globals
        .bind::<ExtForeignToplevelImageCaptureSourceManagerV1, _, _>(&qh, 1..=1, ())
        .ok();
    let capture_mgr = globals
        .bind::<ExtImageCopyCaptureManagerV1, _, _>(&qh, 1..=1, ())
        .map_err(|e| {
            warn!("jwm-portal: ext_image_copy_capture_manager_v1 not advertised: {e}");
            e
        })
        .ok();

    let mut outputs = Vec::new();
    for global in globals.contents().clone_list() {
        if global.interface == wl_output::WlOutput::interface().name {
            let wl_out = globals
                .registry()
                .bind::<wl_output::WlOutput, _, Client>(global.name, global.version.min(4), &qh, ());
            outputs.push((global.name, wl_out, OutputInfo::default()));
        }
    }

    let mut client = Client {
        snapshot: snapshot.clone(),
        outputs,
        toplevels: Vec::new(),
        _toplevel_list: toplevel_list,
        _output_source_mgr: output_source_mgr,
        _toplevel_source_mgr: toplevel_source_mgr,
        _capture_mgr: capture_mgr,
    };

    info!(
        "jwm-portal: bound globals — capture_mgr={} output_src={} toplevel_src={} toplevel_list={}",
        client._capture_mgr.is_some(),
        client._output_source_mgr.is_some(),
        client._toplevel_source_mgr.is_some(),
        client._toplevel_list.is_some(),
    );

    loop {
        event_queue.blocking_dispatch(&mut client)?;
        client.publish();
        if shutdown.try_recv().is_ok() {
            break;
        }
    }
    Ok(())
}

// --- Dispatch impls -----------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Client {
    fn event(
        state: &mut Self,
        proxy: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        // Track wl_output add/remove so the picker sees fresh monitors after
        // hot-plug. Other globals (manager bindings, toplevel-list) are
        // bound once at startup; if the compositor restarts them mid-life
        // we'd need a fuller redo — the portal is cheap to restart, so for
        // now we only track the dynamic output set.
        match event {
            wl_registry::Event::Global { name, interface, version } => {
                if interface == wl_output::WlOutput::interface().name {
                    let wl_out = proxy.bind::<wl_output::WlOutput, _, Client>(
                        name,
                        version.min(4),
                        qh,
                        (),
                    );
                    state.outputs.push((name, wl_out, OutputInfo::default()));
                    info!("jwm-portal: wl_output hot-plug add registry_name={name}");
                }
            }
            wl_registry::Event::GlobalRemove { name } => {
                let before = state.outputs.len();
                state.outputs.retain(|(rn, out, _)| {
                    if *rn == name {
                        // Release v3+ wl_output (no-op on older versions).
                        if out.version() >= 3 {
                            out.release();
                        }
                        false
                    } else {
                        true
                    }
                });
                if state.outputs.len() != before {
                    info!("jwm-portal: wl_output hot-plug remove registry_name={name}");
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for Client {
    fn event(
        state: &mut Self,
        proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let Some(entry) = state.outputs.iter_mut().find(|(_, p, _)| p == proxy) else {
            return;
        };
        match event {
            wl_output::Event::Name { name } => entry.2.name = name,
            wl_output::Event::Description { description } => entry.2.description = description,
            wl_output::Event::Mode { width, height, refresh, .. } => {
                entry.2.width = width;
                entry.2.height = height;
                entry.2.refresh_mhz = refresh;
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for Client {
    fn event(
        state: &mut Self,
        _proxy: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            debug!("jwm-portal: new toplevel handle id={}", toplevel.id());
            state.toplevels.push((toplevel, ToplevelInfo::default()));
        }
    }

    fn event_created_child(
        opcode: u16,
        qh: &QueueHandle<Self>,
    ) -> Arc<dyn wayland_client::backend::ObjectData> {
        match opcode {
            ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => {
                qh.make_data::<ExtForeignToplevelHandleV1, _>(())
            }
            _ => panic!("unexpected child opcode for ExtForeignToplevelListV1: {opcode}"),
        }
    }
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for Client {
    fn event(
        state: &mut Self,
        proxy: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let Some(entry) = state.toplevels.iter_mut().find(|(p, _)| p == proxy) else {
            return;
        };
        match event {
            ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => {
                entry.1.identifier = identifier;
            }
            ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                entry.1.app_id = app_id;
            }
            ext_foreign_toplevel_handle_v1::Event::Title { title } => {
                entry.1.title = title;
            }
            ext_foreign_toplevel_handle_v1::Event::Closed => {
                state.toplevels.retain(|(p, _)| p != proxy);
            }
            _ => {}
        }
    }
}

// Manager globals: bound for their side-effect of being available; no events
// flow on these managers themselves (children carry the work).
macro_rules! manager_dispatch {
    ($t:ty) => {
        impl Dispatch<$t, ()> for Client {
            fn event(
                _: &mut Self,
                _: &$t,
                _: <$t as Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    };
}
manager_dispatch!(ExtOutputImageCaptureSourceManagerV1);
manager_dispatch!(ExtForeignToplevelImageCaptureSourceManagerV1);
manager_dispatch!(ExtImageCopyCaptureManagerV1);
