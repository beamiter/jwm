//! ext-image-copy-capture-v1 client side: per-source frame pump.
//!
//! Owns a dedicated Wayland connection on its own thread, negotiates a SHM
//! frame buffer with the compositor, then continuously memcpy's captured
//! frames into a [`SharedFrame`] that the PipeWire producer reads from its
//! on_process callback.
//!
//! MVP scope: wl_output sources, wl_shm only (no dmabuf), Xrgb8888 / Argb8888.

#![allow(dead_code)]

use std::ffi::{CString, c_void};
use std::num::NonZeroUsize;
use std::os::fd::AsFd;
use std::ptr::NonNull;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use log::{info, warn};
use nix::sys::memfd::{MFdFlags, memfd_create};
use nix::sys::mman::{MapFlags, ProtFlags, mmap, munmap};
use nix::unistd::ftruncate;
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool},
};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1, FailureReason},
    ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1},
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};

/// One captured frame, ready for the PipeWire producer to drain.
#[derive(Debug)]
pub struct FrameSlot {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    /// Last delivered frame pixels (overwritten on each capture). When `seq`
    /// is zero, contents are undefined (no frame has landed yet).
    pub data: Vec<u8>,
    /// Monotonic counter incremented per frame; PW side may use it for skip
    /// detection but the MVP just always copies.
    pub seq: u64,
}

impl Default for FrameSlot {
    fn default() -> Self {
        Self {
            width: 0,
            height: 0,
            stride: 0,
            data: Vec::new(),
            seq: 0,
        }
    }
}

pub type SharedFrame = Arc<Mutex<FrameSlot>>;

pub fn new_frame_slot() -> SharedFrame {
    Arc::new(Mutex::new(FrameSlot::default()))
}

pub struct CaptureHandle {
    pub width: u32,
    pub height: u32,
    pub framerate_num: u32,
    pub framerate_den: u32,
    shutdown: Option<mpsc::Sender<()>>,
    join: Option<JoinHandle<()>>,
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Spawn a capture thread targeting the wl_output whose `Name` matches
/// `output_name`. Returns once the session has finished its initial
/// constraint negotiation (so the caller knows the real buffer size before
/// advertising format to PipeWire).
pub fn spawn_output_capture(
    output_name: String,
    frame: SharedFrame,
) -> Result<CaptureHandle, String> {
    let (init_tx, init_rx) = mpsc::sync_channel::<Result<NegotiatedFormat, String>>(1);
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();
    let frame_for_thread = frame.clone();
    let init_tx_for_thread = init_tx.clone();
    let join = std::thread::Builder::new()
        .name("jwm-portal-cap".into())
        .spawn(move || {
            let result = run_capture(output_name, frame_for_thread, &init_tx_for_thread, shutdown_rx);
            if let Err(e) = result {
                // If we failed before sending an init result, surface it.
                let _ = init_tx_for_thread.send(Err(e.clone()));
                warn!("capture thread exited: {e}");
            }
        })
        .map_err(|e| format!("spawn capture thread: {e}"))?;

    let neg = init_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|e| format!("capture thread did not negotiate within 5s: {e}"))?
        .map_err(|e| format!("capture thread negotiation failed: {e}"))?;

    Ok(CaptureHandle {
        width: neg.width,
        height: neg.height,
        framerate_num: neg.framerate_num,
        framerate_den: neg.framerate_den,
        shutdown: Some(shutdown_tx),
        join: Some(join),
    })
}

#[derive(Debug, Clone, Copy)]
struct NegotiatedFormat {
    width: u32,
    height: u32,
    framerate_num: u32,
    framerate_den: u32,
}

#[derive(Debug, PartialEq, Eq)]
enum FrameStatus {
    Idle,
    Pending,
    Ready,
    Failed,
}

#[derive(Default, Debug)]
struct OutputProbe {
    name: String,
    refresh_mhz: i32,
}

struct State {
    outputs: Vec<(wl_output::WlOutput, OutputProbe)>,
    session_done: bool,
    session_stopped: bool,
    buffer_width: u32,
    buffer_height: u32,
    chosen_shm_format: Option<wl_shm::Format>,
    frame_status: FrameStatus,
    frame_failure: Option<FailureReason>,
}

fn run_capture(
    output_name: String,
    frame: SharedFrame,
    init_tx: &mpsc::SyncSender<Result<NegotiatedFormat, String>>,
    shutdown_rx: mpsc::Receiver<()>,
) -> Result<(), String> {
    let conn = Connection::connect_to_env().map_err(|e| format!("connect_to_env: {e}"))?;
    let (globals, mut event_queue) =
        registry_queue_init::<State>(&conn).map_err(|e| format!("registry_queue_init: {e}"))?;
    let qh = event_queue.handle();

    let shm: wl_shm::WlShm = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| format!("bind wl_shm: {e}"))?;
    let src_mgr: ExtOutputImageCaptureSourceManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| format!("bind output_image_capture_source_manager_v1: {e}"))?;
    let cap_mgr: ExtImageCopyCaptureManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| format!("bind image_copy_capture_manager_v1: {e}"))?;

    let mut state = State {
        outputs: Vec::new(),
        session_done: false,
        session_stopped: false,
        buffer_width: 0,
        buffer_height: 0,
        chosen_shm_format: None,
        frame_status: FrameStatus::Idle,
        frame_failure: None,
    };

    for global in globals.contents().clone_list() {
        if global.interface == wl_output::WlOutput::interface().name {
            let wl_out = globals.registry().bind::<wl_output::WlOutput, _, State>(
                global.name,
                global.version.min(4),
                &qh,
                (),
            );
            state.outputs.push((wl_out, OutputProbe::default()));
        }
    }
    if state.outputs.is_empty() {
        return Err("no wl_output advertised".into());
    }

    let deadline = Instant::now() + Duration::from_secs(2);
    while state.outputs.iter().any(|(_, p)| p.name.is_empty()) {
        event_queue
            .blocking_dispatch(&mut state)
            .map_err(|e| format!("dispatch (outputs): {e}"))?;
        if Instant::now() > deadline {
            warn!("capture: timed out waiting for wl_output names; using first output");
            break;
        }
    }

    let target_idx = state
        .outputs
        .iter()
        .position(|(_, p)| p.name == output_name)
        .unwrap_or(0);
    let (target_output, probe) = state.outputs[target_idx].clone_pair();
    info!(
        "capture: target output `{}` (requested `{}`, refresh {} mHz)",
        probe.name, output_name, probe.refresh_mhz
    );

    let source = src_mgr.create_source(&target_output, &qh, ());
    let session = cap_mgr.create_session(
        &source,
        ext_image_copy_capture_manager_v1::Options::PaintCursors,
        &qh,
        (),
    );

    let deadline = Instant::now() + Duration::from_secs(3);
    while !state.session_done {
        event_queue
            .blocking_dispatch(&mut state)
            .map_err(|e| format!("dispatch (session negotiation): {e}"))?;
        if state.session_stopped {
            return Err("session stopped before negotiation completed".into());
        }
        if Instant::now() > deadline {
            return Err("session negotiation timeout".into());
        }
    }

    let width = state.buffer_width;
    let height = state.buffer_height;
    let format = state
        .chosen_shm_format
        .ok_or_else(|| "session advertised no supported shm_format".to_string())?;
    if width == 0 || height == 0 {
        return Err(format!("invalid negotiated size {width}x{height}"));
    }
    let stride = width
        .checked_mul(4)
        .ok_or_else(|| "stride overflow".to_string())?;
    let buffer_bytes = (stride as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| "buffer size overflow".to_string())?;
    info!(
        "capture: negotiated {width}x{height} stride={stride} format={:?} bytes={buffer_bytes}",
        format
    );

    let cname = CString::new("jwm-portal-buffer").unwrap();
    let memfd = memfd_create(cname.as_c_str(), MFdFlags::MFD_CLOEXEC)
        .map_err(|e| format!("memfd_create: {e}"))?;
    ftruncate(&memfd, buffer_bytes as i64).map_err(|e| format!("ftruncate: {e}"))?;
    let len = NonZeroUsize::new(buffer_bytes).ok_or("zero-sized buffer")?;
    let map_ptr: NonNull<c_void> = unsafe {
        mmap(
            None,
            len,
            ProtFlags::PROT_READ,
            MapFlags::MAP_SHARED,
            &memfd,
            0,
        )
        .map_err(|e| format!("mmap: {e}"))?
    };

    let pool: wl_shm_pool::WlShmPool =
        shm.create_pool(memfd.as_fd(), buffer_bytes as i32, &qh, ());
    let wl_buf: wl_buffer::WlBuffer =
        pool.create_buffer(0, width as i32, height as i32, stride as i32, format, &qh, ());

    {
        let mut f = frame.lock().expect("frame slot mutex");
        f.width = width;
        f.height = height;
        f.stride = stride;
        f.data = vec![0u8; buffer_bytes];
        f.seq = 0;
    }

    let framerate_num = if probe.refresh_mhz > 0 {
        ((probe.refresh_mhz + 500) / 1000).max(1) as u32
    } else {
        60
    };
    init_tx
        .send(Ok(NegotiatedFormat {
            width,
            height,
            framerate_num,
            framerate_den: 1,
        }))
        .map_err(|e| format!("send init: {e}"))?;

    info!("capture: entering frame loop");
    let result = frame_loop(
        &conn,
        &mut event_queue,
        &mut state,
        &session,
        &wl_buf,
        &frame,
        width,
        height,
        buffer_bytes,
        map_ptr,
        &qh,
        &shutdown_rx,
    );

    wl_buf.destroy();
    pool.destroy();
    session.destroy();
    source.destroy();
    let _ = unsafe { munmap(map_ptr, buffer_bytes) };
    info!("capture: thread exiting");
    result
}

#[allow(clippy::too_many_arguments)]
fn frame_loop(
    conn: &Connection,
    event_queue: &mut wayland_client::EventQueue<State>,
    state: &mut State,
    session: &ExtImageCopyCaptureSessionV1,
    wl_buf: &wl_buffer::WlBuffer,
    frame: &SharedFrame,
    width: u32,
    height: u32,
    buffer_bytes: usize,
    map_ptr: NonNull<c_void>,
    qh: &QueueHandle<State>,
    shutdown_rx: &mpsc::Receiver<()>,
) -> Result<(), String> {
    loop {
        if shutdown_rx.try_recv().is_ok() {
            return Ok(());
        }
        if state.session_stopped {
            warn!("capture: session stopped by compositor");
            return Ok(());
        }

        let cap_frame = session.create_frame(qh, ());
        cap_frame.attach_buffer(wl_buf);
        cap_frame.damage_buffer(0, 0, width as i32, height as i32);
        cap_frame.capture();
        state.frame_status = FrameStatus::Pending;
        state.frame_failure = None;
        let _ = conn.flush();

        while state.frame_status == FrameStatus::Pending {
            if shutdown_rx.try_recv().is_ok() {
                cap_frame.destroy();
                return Ok(());
            }
            event_queue
                .blocking_dispatch(state)
                .map_err(|e| format!("dispatch (frame): {e}"))?;
        }

        match state.frame_status {
            FrameStatus::Ready => {
                let src = unsafe {
                    std::slice::from_raw_parts(map_ptr.as_ptr() as *const u8, buffer_bytes)
                };
                let mut f = frame.lock().expect("frame slot mutex");
                if f.data.len() == src.len() {
                    f.data.copy_from_slice(src);
                    f.seq = f.seq.wrapping_add(1);
                } else {
                    warn!(
                        "capture: frame slot size mismatch ({} vs {})",
                        f.data.len(),
                        src.len()
                    );
                }
            }
            FrameStatus::Failed => {
                warn!("capture: frame failed: {:?}", state.frame_failure);
                std::thread::sleep(Duration::from_millis(100));
            }
            _ => {}
        }

        cap_frame.destroy();
    }
}

// Helper to clone a wl_output proxy together with the latched OutputProbe.
// `(WlOutput, OutputProbe)` doesn't impl Clone via tuple-derive because
// OutputProbe is Clone but the field access pattern is ergonomic only via
// this little helper.
trait OutputEntryExt {
    fn clone_pair(&self) -> (wl_output::WlOutput, OutputProbe);
}
impl OutputEntryExt for (wl_output::WlOutput, OutputProbe) {
    fn clone_pair(&self) -> (wl_output::WlOutput, OutputProbe) {
        (
            self.0.clone(),
            OutputProbe {
                name: self.1.name.clone(),
                refresh_mhz: self.1.refresh_mhz,
            },
        )
    }
}

// --- Dispatch impls ----------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
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

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(entry) = state.outputs.iter_mut().find(|(p, _)| p == proxy) else {
            return;
        };
        match event {
            wl_output::Event::Name { name } => entry.1.name = name,
            wl_output::Event::Mode { refresh, .. } => entry.1.refresh_mhz = refresh,
            _ => {}
        }
    }
}

macro_rules! empty_dispatch {
    ($t:ty) => {
        impl Dispatch<$t, ()> for State {
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
empty_dispatch!(wl_shm::WlShm);
empty_dispatch!(wl_shm_pool::WlShmPool);
empty_dispatch!(wl_buffer::WlBuffer);
empty_dispatch!(ExtOutputImageCaptureSourceManagerV1);
empty_dispatch!(ExtImageCaptureSourceV1);
empty_dispatch!(ExtImageCopyCaptureManagerV1);

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_session_v1::Event::BufferSize { width, height } => {
                state.buffer_width = width;
                state.buffer_height = height;
            }
            ext_image_copy_capture_session_v1::Event::ShmFormat { format } => {
                if let WEnum::Value(f) = format {
                    // Prefer Xrgb8888 (no alpha channel surprises) when offered.
                    let take = match (state.chosen_shm_format, f) {
                        (None, wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888) => true,
                        (Some(wl_shm::Format::Argb8888), wl_shm::Format::Xrgb8888) => true,
                        _ => false,
                    };
                    if take {
                        state.chosen_shm_format = Some(f);
                    }
                }
            }
            ext_image_copy_capture_session_v1::Event::Done => {
                state.session_done = true;
            }
            ext_image_copy_capture_session_v1::Event::Stopped => {
                state.session_stopped = true;
                state.frame_status = FrameStatus::Failed;
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_frame_v1::Event::Ready => {
                state.frame_status = FrameStatus::Ready;
            }
            ext_image_copy_capture_frame_v1::Event::Failed { reason } => {
                state.frame_status = FrameStatus::Failed;
                if let WEnum::Value(r) = reason {
                    state.frame_failure = Some(r);
                }
            }
            _ => {}
        }
    }
}
