//! ext-image-copy-capture-v1 client side: per-source frame pump.
//!
//! Owns a dedicated Wayland connection on its own thread. Tries dmabuf
//! transport first (GBM-allocated LINEAR Argb/Xrgb buffers shared with the
//! PipeWire producer thread via fd), falls back to SHM (memcpy a captured
//! frame into a [`SharedFrame`] that the PW on_process reads) if any part of
//! the dmabuf init fails or the session didn't offer compatible dmabuf.
//!
//! Frame-rate / synchronization model:
//!   * SHM: capture loop runs free, overwrites the SharedFrame on every
//!     captured frame; PW reads whenever on_process fires.
//!   * Dmabuf: capture loop is request-driven — blocks on
//!     `bridge.fill_req_rx.recv()`, attaches the requested slot's wl_buffer,
//!     captures, signals done via `fill_done_tx`. PW on_process drives the
//!     cadence by requesting a fill before queuing each pw_buffer.

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
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1,
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1, FailureReason},
    ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1},
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};

use crate::dmabuf::{self, DmabufBuffer, GbmContext};
use crate::pipewire_stream::{DmabufBridge, SharedBridge};

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

/// Transport the capture thread ended up using. The PW side reads this to
/// know whether to declare DataType::DmaBuf or fall through to the legacy
/// SHM memcpy path.
pub enum CaptureTransport {
    Shm(SharedFrame),
    Dmabuf(SharedBridge),
}

pub struct CaptureHandle {
    pub width: u32,
    pub height: u32,
    pub framerate_num: u32,
    pub framerate_den: u32,
    pub transport: CaptureTransport,
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

/// Which source the capture thread should track. The string identifies it on
/// the compositor side: a `wl_output::Name` value for Output, or an
/// `ext_foreign_toplevel_handle_v1::Identifier` value for Toplevel.
#[derive(Debug, Clone)]
pub enum CaptureTarget {
    Output { name: String },
    Toplevel { identifier: String },
}

impl CaptureTarget {
    fn debug_label(&self) -> &str {
        match self {
            CaptureTarget::Output { name } => name,
            CaptureTarget::Toplevel { identifier } => identifier,
        }
    }
}

/// Spawn a capture thread targeting the wl_output whose `Name` matches
/// `output_name`. Returns once the session has finished its initial
/// constraint negotiation (so the caller knows the real buffer size before
/// advertising format to PipeWire).
pub fn spawn_output_capture(
    output_name: String,
    paint_cursors: bool,
) -> Result<CaptureHandle, String> {
    spawn_capture(CaptureTarget::Output { name: output_name }, paint_cursors)
}

/// Spawn a capture thread targeting the ext-foreign-toplevel-list-v1 handle
/// whose `Identifier` matches `identifier`. Same negotiation contract as
/// [`spawn_output_capture`].
pub fn spawn_toplevel_capture(
    identifier: String,
    paint_cursors: bool,
) -> Result<CaptureHandle, String> {
    spawn_capture(CaptureTarget::Toplevel { identifier }, paint_cursors)
}

fn spawn_capture(
    target: CaptureTarget,
    paint_cursors: bool,
) -> Result<CaptureHandle, String> {
    let (init_tx, init_rx) = mpsc::sync_channel::<Result<InitResult, String>>(1);
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();
    let init_tx_for_thread = init_tx.clone();
    let join = std::thread::Builder::new()
        .name("jwm-portal-cap".into())
        .spawn(move || {
            let result = run_capture(target, paint_cursors, &init_tx_for_thread, shutdown_rx);
            if let Err(e) = result {
                // If we failed before sending an init result, surface it.
                let _ = init_tx_for_thread.send(Err(e.clone()));
                warn!("capture thread exited: {e}");
            }
        })
        .map_err(|e| format!("spawn capture thread: {e}"))?;

    let init = init_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|e| format!("capture thread did not negotiate within 5s: {e}"))?
        .map_err(|e| format!("capture thread negotiation failed: {e}"))?;

    Ok(CaptureHandle {
        width: init.width,
        height: init.height,
        framerate_num: init.framerate_num,
        framerate_den: init.framerate_den,
        transport: init.transport,
        shutdown: Some(shutdown_tx),
        join: Some(join),
    })
}

struct InitResult {
    width: u32,
    height: u32,
    framerate_num: u32,
    framerate_den: u32,
    transport: CaptureTransport,
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

#[derive(Default, Debug)]
struct ToplevelProbe {
    identifier: String,
}

struct State {
    outputs: Vec<(wl_output::WlOutput, OutputProbe)>,
    toplevels: Vec<(ExtForeignToplevelHandleV1, ToplevelProbe)>,
    /// Framerate-mhz hint from the resolved source (output's Mode event). 0
    /// means "no hint" — caller will fall back to 60.
    target_refresh_mhz: i32,
    session_done: bool,
    session_stopped: bool,
    buffer_width: u32,
    buffer_height: u32,
    chosen_shm_format: Option<wl_shm::Format>,
    /// dev_t of the DRM device the compositor renders into, if it advertised
    /// one. Used to open GBM on the matching render node.
    dmabuf_device: Option<u64>,
    /// (fourcc, [modifier]) pairs collected from `dmabuf_format` events.
    /// Empty when the session offered no dmabuf at all.
    dmabuf_formats: Vec<(u32, Vec<u64>)>,
    frame_status: FrameStatus,
    frame_failure: Option<FailureReason>,
}

fn run_capture(
    target: CaptureTarget,
    paint_cursors: bool,
    init_tx: &mpsc::SyncSender<Result<InitResult, String>>,
    shutdown_rx: mpsc::Receiver<()>,
) -> Result<(), String> {
    let conn = Connection::connect_to_env().map_err(|e| format!("connect_to_env: {e}"))?;
    let (globals, mut event_queue) =
        registry_queue_init::<State>(&conn).map_err(|e| format!("registry_queue_init: {e}"))?;
    let qh = event_queue.handle();

    let shm: wl_shm::WlShm = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| format!("bind wl_shm: {e}"))?;
    let cap_mgr: ExtImageCopyCaptureManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| format!("bind image_copy_capture_manager_v1: {e}"))?;
    // linux-dmabuf-v1 binding is optional — capture still works over SHM if
    // the compositor doesn't expose it. We need v3+ for the modifier-aware
    // create_immed path, but accept v3 or v4 (no functional diff for our use).
    let dmabuf_mgr: Option<ZwpLinuxDmabufV1> = globals.bind(&qh, 3..=4, ()).ok();
    if dmabuf_mgr.is_none() {
        info!("capture: zwp_linux_dmabuf_v1 v3+ not available, will use SHM only");
    }

    let mut state = State {
        outputs: Vec::new(),
        toplevels: Vec::new(),
        target_refresh_mhz: 0,
        session_done: false,
        session_stopped: false,
        buffer_width: 0,
        buffer_height: 0,
        chosen_shm_format: None,
        dmabuf_device: None,
        dmabuf_formats: Vec::new(),
        frame_status: FrameStatus::Idle,
        frame_failure: None,
    };

    let mut output_src_mgr: Option<ExtOutputImageCaptureSourceManagerV1> = None;
    let mut toplevel_src_mgr: Option<ExtForeignToplevelImageCaptureSourceManagerV1> = None;
    let mut _toplevel_list: Option<ExtForeignToplevelListV1> = None;

    match &target {
        CaptureTarget::Output { .. } => {
            output_src_mgr = Some(
                globals
                    .bind(&qh, 1..=1, ())
                    .map_err(|e| format!("bind output_image_capture_source_manager_v1: {e}"))?,
            );
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
        }
        CaptureTarget::Toplevel { .. } => {
            toplevel_src_mgr = Some(globals.bind(&qh, 1..=1, ()).map_err(|e| {
                format!("bind foreign_toplevel_image_capture_source_manager_v1: {e}")
            })?);
            _toplevel_list = Some(
                globals
                    .bind(&qh, 1..=1, ())
                    .map_err(|e| format!("bind foreign_toplevel_list_v1: {e}"))?,
            );
        }
    }

    let source = match &target {
        CaptureTarget::Output { name } => {
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
                .position(|(_, p)| &p.name == name)
                .unwrap_or(0);
            let target_output = state.outputs[target_idx].0.clone();
            let refresh_mhz = state.outputs[target_idx].1.refresh_mhz;
            let resolved_name = state.outputs[target_idx].1.name.clone();
            info!(
                "capture: target output `{resolved_name}` (requested `{name}`, refresh {refresh_mhz} mHz)"
            );
            state.target_refresh_mhz = refresh_mhz;
            output_src_mgr
                .as_ref()
                .expect("output source manager bound above")
                .create_source(&target_output, &qh, ())
        }
        CaptureTarget::Toplevel { identifier } => {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let have_match = state
                    .toplevels
                    .iter()
                    .any(|(_, p)| &p.identifier == identifier);
                if have_match {
                    break;
                }
                if Instant::now() > deadline {
                    break;
                }
                event_queue
                    .blocking_dispatch(&mut state)
                    .map_err(|e| format!("dispatch (toplevels): {e}"))?;
            }
            let target_idx = state
                .toplevels
                .iter()
                .position(|(_, p)| &p.identifier == identifier)
                .ok_or_else(|| format!("no toplevel matching identifier `{identifier}`"))?;
            let target_handle = state.toplevels[target_idx].0.clone();
            info!("capture: target toplevel `{identifier}`");
            toplevel_src_mgr
                .as_ref()
                .expect("toplevel source manager bound above")
                .create_source(&target_handle, &qh, ())
        }
    };

    state.session_done = false;
    state.session_stopped = false;
    let session_options = if paint_cursors {
        ext_image_copy_capture_manager_v1::Options::PaintCursors
    } else {
        ext_image_copy_capture_manager_v1::Options::empty()
    };
    let session = cap_mgr.create_session(&source, session_options, &qh, ());

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
    if width == 0 || height == 0 {
        return Err(format!("invalid negotiated size {width}x{height}"));
    }
    let stride = width
        .checked_mul(4)
        .ok_or_else(|| "stride overflow".to_string())?;
    let buffer_bytes = (stride as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| "buffer size overflow".to_string())?;

    let framerate_num = if state.target_refresh_mhz > 0 {
        ((state.target_refresh_mhz + 500) / 1000).max(1) as u32
    } else {
        60
    };

    // ---- Try dmabuf first --------------------------------------------------
    // Conditions:
    //   * dmabuf_mgr (zwp_linux_dmabuf_v1 v3+) was bindable
    //   * session offered at least one of ARGB8888 / XRGB8888 with LINEAR (0)
    //     modifier listed
    //   * GBM allocator opens successfully on the matching render node
    //   * 3 buffers + 3 wl_buffers + fd dup all succeed
    let dmabuf_attempt = (|| -> Option<DmabufSetup> {
        let dmabuf_mgr = dmabuf_mgr.as_ref()?;
        // Pick a fourcc that's in the offered set AND has LINEAR.
        let pick = state.dmabuf_formats.iter().find_map(|(fc, mods)| {
            let supported = matches!(*fc, dmabuf::fourcc::ARGB8888 | dmabuf::fourcc::XRGB8888);
            let has_linear = mods.contains(&dmabuf::MOD_LINEAR);
            (supported && has_linear).then_some(*fc)
        })?;
        let gbm = match GbmContext::open(state.dmabuf_device) {
            Ok(g) => g,
            Err(e) => {
                warn!("dmabuf: GBM open failed ({e}); falling back to SHM");
                return None;
            }
        };
        let mut buffers: Vec<DmabufBuffer> = Vec::new();
        for i in 0..3 {
            match gbm.allocate_linear(width, height, pick) {
                Ok(b) => buffers.push(b),
                Err(e) => {
                    warn!("dmabuf: slot {i} alloc failed ({e}); falling back to SHM");
                    return None;
                }
            }
        }
        // Dup the fds for the PW side. Each side gets its own OwnedFd.
        let mut pw_fds: Vec<std::os::fd::OwnedFd> = Vec::with_capacity(3);
        for (i, b) in buffers.iter().enumerate() {
            match b.fd().try_clone_to_owned() {
                Ok(fd) => pw_fds.push(fd),
                Err(e) => {
                    warn!("dmabuf: fd dup slot {i} failed ({e}); falling back to SHM");
                    return None;
                }
            }
        }
        let mut wl_buffers: Vec<wl_buffer::WlBuffer> = Vec::with_capacity(3);
        for b in buffers.iter() {
            wl_buffers.push(dmabuf::create_wl_buffer::<State>(dmabuf_mgr, b, &qh));
        }
        let stride0 = buffers[0].stride;
        let size = stride0 as u32 * height;
        let modifier = buffers[0].modifier;
        let (fill_req_tx, fill_req_rx) = mpsc::channel::<usize>();
        let (fill_done_tx, fill_done_rx) = mpsc::channel::<()>();
        let bridge = Arc::new(DmabufBridge {
            fourcc: pick,
            modifier,
            width,
            height,
            stride: stride0 as u32,
            size,
            fds: pw_fds,
            fill_req_tx,
            fill_done_rx: Mutex::new(fill_done_rx),
        });
        info!(
            "dmabuf: ready — {width}x{height} fourcc=0x{pick:x} stride={stride0} modifier=0x{modifier:x} 3 slots"
        );
        Some(DmabufSetup {
            _gbm: gbm,
            _bufs: buffers,
            wl_buffers,
            bridge,
            fill_req_rx,
            fill_done_tx,
        })
    })();

    if let Some(setup) = dmabuf_attempt {
        let bridge_for_caller = setup.bridge.clone();
        init_tx
            .send(Ok(InitResult {
                width,
                height,
                framerate_num,
                framerate_den: 1,
                transport: CaptureTransport::Dmabuf(bridge_for_caller),
            }))
            .map_err(|e| format!("send init: {e}"))?;
        info!("capture: entering dmabuf frame loop");
        let result = frame_loop_dmabuf(
            &conn,
            &mut event_queue,
            &mut state,
            &session,
            &setup.wl_buffers,
            width,
            height,
            &setup.fill_req_rx,
            &setup.fill_done_tx,
            &qh,
            &shutdown_rx,
        );
        for wb in &setup.wl_buffers {
            wb.destroy();
        }
        session.destroy();
        source.destroy();
        info!("capture: dmabuf thread exiting");
        return result;
    }

    // ---- SHM fallback ------------------------------------------------------
    let format = state
        .chosen_shm_format
        .ok_or_else(|| "session advertised no supported shm_format".to_string())?;
    info!(
        "capture: SHM transport — {width}x{height} stride={stride} format={:?} bytes={buffer_bytes}",
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

    let frame = new_frame_slot();
    {
        let mut f = frame.lock().expect("frame slot mutex");
        f.width = width;
        f.height = height;
        f.stride = stride;
        f.data = vec![0u8; buffer_bytes];
        f.seq = 0;
    }

    init_tx
        .send(Ok(InitResult {
            width,
            height,
            framerate_num,
            framerate_den: 1,
            transport: CaptureTransport::Shm(frame.clone()),
        }))
        .map_err(|e| format!("send init: {e}"))?;

    info!("capture: entering SHM frame loop");
    let result = frame_loop_shm(
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
    info!("capture: SHM thread exiting");
    result
}

/// Owns the dmabuf-side resources for the lifetime of the capture thread.
/// All fields drop in declaration order: wl_buffers first (already destroyed
/// explicitly), then DmabufBuffers (close fds + free BO), then GbmContext.
struct DmabufSetup {
    _gbm: GbmContext,
    _bufs: Vec<DmabufBuffer>,
    wl_buffers: Vec<wl_buffer::WlBuffer>,
    bridge: SharedBridge,
    fill_req_rx: mpsc::Receiver<usize>,
    fill_done_tx: mpsc::Sender<()>,
}

#[allow(clippy::too_many_arguments)]
fn frame_loop_shm(
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

/// Dmabuf-mode frame loop. Blocks on `fill_req_rx` for slot requests from
/// the PW thread; for each request, attaches the slot's wl_buffer, runs one
/// capture, signals done. Shutdown is checked on each recv timeout tick.
#[allow(clippy::too_many_arguments)]
fn frame_loop_dmabuf(
    conn: &Connection,
    event_queue: &mut wayland_client::EventQueue<State>,
    state: &mut State,
    session: &ExtImageCopyCaptureSessionV1,
    wl_buffers: &[wl_buffer::WlBuffer],
    width: u32,
    height: u32,
    fill_req_rx: &mpsc::Receiver<usize>,
    fill_done_tx: &mpsc::Sender<()>,
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
        let slot_idx = match fill_req_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(idx) => idx,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        };
        let Some(wl_buf) = wl_buffers.get(slot_idx) else {
            warn!("capture: bad slot index {slot_idx} from PW");
            continue;
        };

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
                let _ = fill_done_tx.send(());
            }
            FrameStatus::Failed => {
                warn!("capture: dmabuf frame failed: {:?}", state.frame_failure);
                // Still signal done so PW doesn't hang on its wait — it will
                // queue an empty chunk (size=0) which downstream treats as
                // a dropped frame. Brief backoff so we don't tight-loop on a
                // persistently broken source.
                let _ = fill_done_tx.send(());
                std::thread::sleep(Duration::from_millis(100));
            }
            _ => {}
        }

        cap_frame.destroy();
        let _ = height; // silence unused-warning in case the protocol stops using it
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
empty_dispatch!(ExtForeignToplevelImageCaptureSourceManagerV1);
empty_dispatch!(ExtImageCaptureSourceV1);
empty_dispatch!(ExtImageCopyCaptureManagerV1);
empty_dispatch!(ZwpLinuxDmabufV1);
empty_dispatch!(ZwpLinuxBufferParamsV1);

impl Dispatch<ExtForeignToplevelListV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            state
                .toplevels
                .push((toplevel, ToplevelProbe::default()));
        }
    }

    fn event_created_child(
        opcode: u16,
        qh: &QueueHandle<Self>,
    ) -> std::sync::Arc<dyn wayland_client::backend::ObjectData> {
        match opcode {
            ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => {
                qh.make_data::<ExtForeignToplevelHandleV1, _>(())
            }
            _ => panic!("unexpected child opcode for ExtForeignToplevelListV1: {opcode}"),
        }
    }
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(entry) = state.toplevels.iter_mut().find(|(p, _)| p == proxy) else {
            return;
        };
        match event {
            ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => {
                entry.1.identifier = identifier;
            }
            ext_foreign_toplevel_handle_v1::Event::Closed => {
                state.toplevels.retain(|(p, _)| p != proxy);
            }
            _ => {}
        }
    }
}

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
            ext_image_copy_capture_session_v1::Event::DmabufDevice { device } => {
                let mut buf = [0u8; 8];
                let n = device.len().min(8);
                buf[..n].copy_from_slice(&device[..n]);
                state.dmabuf_device = Some(u64::from_le_bytes(buf));
            }
            ext_image_copy_capture_session_v1::Event::DmabufFormat { format, modifiers } => {
                let mods: Vec<u64> = modifiers
                    .chunks_exact(8)
                    .map(|c| {
                        let mut b = [0u8; 8];
                        b.copy_from_slice(c);
                        u64::from_le_bytes(b)
                    })
                    .collect();
                state.dmabuf_formats.push((format, mods));
            }
            ext_image_copy_capture_session_v1::Event::Done => {
                if state.dmabuf_device.is_some() || !state.dmabuf_formats.is_empty() {
                    log::info!(
                        "capture: session offered dmabuf (device=0x{:x}, {} format(s))",
                        state.dmabuf_device.unwrap_or(0),
                        state.dmabuf_formats.len(),
                    );
                }
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
