//! PipeWire producer node for a single ScreenCast source.
//!
//! Per Start() invocation, [`spawn`] creates an OS thread that owns a
//! `pw::MainLoopRc` + a `Video/Source` stream. The thread emits one node and
//! returns the assigned `node_id` to the caller via a sync_channel; the node
//! survives until the returned `StreamHandle` is dropped.
//!
//! Two transports are supported via [`Source`]:
//!
//!   * `Shm` — SHM frame slot. on_process holds the SharedFrame lock across a
//!     memcpy into the dequeued PW buffer. Allocated by the capture thread.
//!
//!   * `Dmabuf` — three GBM-allocated LINEAR dmabuf fds shared with the
//!     capture thread. EnumFormat pins VideoModifier=0 (LINEAR), Buffers param
//!     declares `DataType::DmaBuf`. add_buffer writes the fd of slot `i%3`
//!     into each pw_buffer's data[0] via unsafe spa_data manipulation;
//!     on_process synchronously asks the capture thread to fill the relevant
//!     slot (lookup by fd), waits up to one frame, then sets chunk.size and
//!     queues. The capture thread frame rate gates everything.

use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use log::{info, warn};
use pipewire as pw;
use pw::spa;
use spa::pod::Pod;

use crate::capture::SharedFrame;
use crate::dmabuf;

/// Caller-side handle. Drop to tear the stream + worker thread down.
pub struct StreamHandle {
    pub node_id: u32,
    shutdown: Option<mpsc::Sender<()>>,
    join: Option<JoinHandle<()>>,
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct StreamSpec {
    pub width: u32,
    pub height: u32,
    pub framerate_num: u32,
    pub framerate_den: u32,
}

impl Default for StreamSpec {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            framerate_num: 60,
            framerate_den: 1,
        }
    }
}

/// Cross-thread fill protocol for the dmabuf transport. PW thread asks the
/// capture thread to fill a specific slot before queuing the dequeued buffer.
/// Wrapped in Arc so both threads can share it; mpsc receivers go behind a
/// Mutex so the PW closure can hold them in `move` capture.
pub struct DmabufBridge {
    #[allow(dead_code)]
    pub fourcc: u32,
    pub modifier: u64,
    #[allow(dead_code)]
    pub width: u32,
    #[allow(dead_code)]
    pub height: u32,
    pub stride: u32,
    /// `stride * height` — the payload size in each chunk.
    pub size: u32,
    /// PW-side copies of each slot's dmabuf fd, one per slot index. Cloned
    /// from the capture thread's originals via `OwnedFd::try_clone` so each
    /// side has independent lifetime management.
    pub fds: Vec<OwnedFd>,
    /// Monotonic request sequence allocator. Each PW on_process bumps this
    /// and sends `(seq, slot_idx)` to capture. Done signals carry the same
    /// seq back so stale dones from timed-out requests can be discarded.
    pub next_req_seq: std::sync::atomic::AtomicU64,
    /// PW → capture: `(seq, slot_idx)`.
    pub fill_req_tx: mpsc::Sender<(u64, usize)>,
    /// Capture → PW: the seq of the request just completed.
    pub fill_done_rx: Mutex<mpsc::Receiver<u64>>,
}

pub type SharedBridge = Arc<DmabufBridge>;

/// What the on_process callback should read from.
pub enum Source {
    #[allow(dead_code)]
    Empty,
    Shm(SharedFrame),
    Dmabuf(SharedBridge),
}

/// Spawn a PipeWire stream worker. Returns once the worker has reported its
/// assigned PipeWire node_id (or fails after a 5s startup deadline).
pub fn spawn(
    spec: StreamSpec,
    node_name: String,
    source: Source,
) -> Result<StreamHandle, String> {
    let (node_tx, node_rx) = mpsc::sync_channel::<Result<u32, String>>(1);
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();

    let join = std::thread::Builder::new()
        .name("jwm-portal-pw".into())
        .spawn(move || {
            if let Err(e) = run(spec, node_name, source, node_tx.clone(), shutdown_rx) {
                warn!("pw worker exited with error: {e}");
                let _ = node_tx.send(Err(e));
            }
        })
        .map_err(|e| format!("spawn pw worker: {e}"))?;

    let node_id = node_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|e| format!("pw worker did not report node_id within 5s: {e}"))?
        .map_err(|e| format!("pw worker failed: {e}"))?;

    Ok(StreamHandle {
        node_id,
        shutdown: Some(shutdown_tx),
        join: Some(join),
    })
}

fn run(
    spec: StreamSpec,
    node_name: String,
    source: Source,
    node_tx: mpsc::SyncSender<Result<u32, String>>,
    shutdown_rx: mpsc::Receiver<()>,
) -> Result<(), String> {
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| format!("MainLoop: {e}"))?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| format!("Context: {e}"))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| format!("connect core: {e}"))?;

    let props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Video",
        *pw::keys::MEDIA_CATEGORY => "Source",
        *pw::keys::MEDIA_ROLE => "Screen",
        *pw::keys::MEDIA_CLASS => "Video/Source",
        *pw::keys::NODE_NAME => node_name.as_str(),
        *pw::keys::NODE_DESCRIPTION => "jwm screen capture",
    };

    let stream = pw::stream::StreamRc::new(core, &node_name, props)
        .map_err(|e| format!("Stream: {e}"))?;

    // For dmabuf transport, the PW side needs an `fd -> slot_idx` map populated
    // in add_buffer so on_process can look up which slot to fill. Wrapping in
    // Mutex<HashMap> because the two callbacks run on the same thread but the
    // borrow checker still wants distinct mutable references.
    let fd_to_slot: Arc<Mutex<HashMap<RawFd, usize>>> = Arc::new(Mutex::new(HashMap::new()));
    let dmabuf_next_assign = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let is_dmabuf = matches!(source, Source::Dmabuf(_));
    let bridge: Option<SharedBridge> = match &source {
        Source::Dmabuf(b) => Some(b.clone()),
        _ => None,
    };
    let shm: Option<SharedFrame> = match &source {
        Source::Shm(s) => Some(s.clone()),
        _ => None,
    };

    let _listener = stream
        .add_local_listener_with_user_data::<()>(())
        .state_changed(|_, _, old, new| {
            info!("pw stream state: {old:?} -> {new:?}");
        })
        .param_changed(|_stream, _ud, id, param| {
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Some(param) = param else { return };
            let mut info = spa::param::video::VideoInfoRaw::default();
            if info.parse(param).is_ok() {
                info!(
                    "pw stream format: {:?} {}x{} @ {}/{}",
                    info.format(),
                    info.size().width,
                    info.size().height,
                    info.framerate().num,
                    info.framerate().denom,
                );
            }
        })
        .add_buffer({
            let fd_to_slot = fd_to_slot.clone();
            let next = dmabuf_next_assign.clone();
            let bridge = bridge.clone();
            move |_stream, _ud, pw_buffer| unsafe {
                // Only relevant for dmabuf transport.
                let Some(bridge) = bridge.as_ref() else { return };
                if pw_buffer.is_null() {
                    return;
                }
                let buf: *mut pw::sys::pw_buffer = pw_buffer;
                let spa_buf = (*buf).buffer;
                if spa_buf.is_null() || (*spa_buf).n_datas == 0 || (*spa_buf).datas.is_null() {
                    warn!("dmabuf add_buffer: pw_buffer with no data slots");
                    return;
                }
                let slot_idx = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    % bridge.fds.len();
                let fd = bridge.fds[slot_idx].as_raw_fd();
                let data = (*spa_buf).datas;
                (*data).type_ = libspa_sys::SPA_DATA_DmaBuf;
                (*data).flags = libspa_sys::SPA_DATA_FLAG_READABLE;
                (*data).fd = fd as i64;
                (*data).mapoffset = 0;
                (*data).maxsize = bridge.size;
                (*data).data = std::ptr::null_mut();
                let chunk = (*data).chunk;
                if !chunk.is_null() {
                    (*chunk).offset = 0;
                    (*chunk).stride = bridge.stride as i32;
                    (*chunk).size = 0;
                }
                fd_to_slot.lock().unwrap().insert(fd, slot_idx);
                info!(
                    "dmabuf add_buffer: pw_buffer={:p} -> slot {slot_idx} (fd={fd})",
                    buf
                );
            }
        })
        .remove_buffer({
            let fd_to_slot = fd_to_slot.clone();
            let bridge = bridge.clone();
            move |_stream, _ud, pw_buffer| unsafe {
                if bridge.is_none() || pw_buffer.is_null() {
                    return;
                }
                let buf: *mut pw::sys::pw_buffer = pw_buffer;
                let spa_buf = (*buf).buffer;
                if spa_buf.is_null() || (*spa_buf).n_datas == 0 || (*spa_buf).datas.is_null() {
                    return;
                }
                let fd = (*(*spa_buf).datas).fd as RawFd;
                fd_to_slot.lock().unwrap().remove(&fd);
            }
        })
        .process({
            let shm = shm.clone();
            let bridge = bridge.clone();
            let fd_to_slot = fd_to_slot.clone();
            move |stream, _ud| {
                let Some(mut buffer) = stream.dequeue_buffer() else { return };
                let datas = buffer.datas_mut();
                let Some(data) = datas.first_mut() else { return };

                if let Some(bridge) = bridge.as_ref() {
                    // dmabuf path — look up which slot this pw_buffer's fd
                    // corresponds to, request a fill keyed by a unique seq,
                    // wait up to one frame for the matching done. The fd
                    // was wired in add_buffer; we never rewrite it here.
                    let fd = data.fd();
                    let slot_idx = match fd_to_slot.lock().unwrap().get(&fd) {
                        Some(&idx) => idx,
                        None => {
                            warn!("dmabuf process: fd {fd} has no slot mapping; queueing empty");
                            let chunk = data.chunk_mut();
                            *chunk.offset_mut() = 0;
                            *chunk.stride_mut() = bridge.stride as i32;
                            *chunk.size_mut() = 0;
                            return;
                        }
                    };
                    if slot_idx >= bridge.fds.len() {
                        warn!(
                            "dmabuf process: slot_idx {slot_idx} out of range (have {} fds)",
                            bridge.fds.len()
                        );
                        let chunk = data.chunk_mut();
                        *chunk.offset_mut() = 0;
                        *chunk.stride_mut() = bridge.stride as i32;
                        *chunk.size_mut() = 0;
                        return;
                    }
                    // Seq protocol: each request gets a unique u64 id. Capture
                    // echoes the seq when it signals done. If we time out on
                    // request N, capture's eventual late done(N) gets read on
                    // the next on_process cycle and recognized as stale (seq
                    // != current) — discarded rather than misattributed to
                    // request N+1. This is the only way to make timeout safe
                    // when the worker keeps running after the deadline.
                    let req_seq = bridge.next_req_seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let _ = bridge.fill_req_tx.send((req_seq, slot_idx));
                    let deadline = std::time::Instant::now() + Duration::from_millis(100);
                    let filled = {
                        let rx = bridge.fill_done_rx.lock().unwrap();
                        loop {
                            let now = std::time::Instant::now();
                            if now >= deadline {
                                break false;
                            }
                            match rx.recv_timeout(deadline - now) {
                                Ok(seq) if seq == req_seq => break true,
                                Ok(stale) => {
                                    log::trace!("dmabuf: discarding stale done seq={stale} (want {req_seq})");
                                    continue;
                                }
                                Err(_) => break false,
                            }
                        }
                    };
                    let chunk = data.chunk_mut();
                    *chunk.offset_mut() = 0;
                    *chunk.stride_mut() = bridge.stride as i32;
                    *chunk.size_mut() = if filled { bridge.size } else { 0 };
                    return;
                }

                // SHM path — old memcpy behavior.
                let mut written_size: u32 = 0;
                let mut written_stride: i32 = 0;
                if let Some(slot) = shm.as_ref() {
                    if let (Some(dst), Ok(f)) = (data.data(), slot.lock()) {
                        if f.seq != 0 && !f.data.is_empty() {
                            let n = dst.len().min(f.data.len());
                            dst[..n].copy_from_slice(&f.data[..n]);
                            written_size = (f.stride as u64 * f.height as u64).min(n as u64) as u32;
                            written_stride = f.stride as i32;
                        }
                    }
                }
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = written_stride;
                *chunk.size_mut() = written_size;
            }
        })
        .register()
        .map_err(|e| format!("register listener: {e}"))?;

    let mut owned_pods = build_connect_params(&spec, &source)?;
    let mut params: Vec<&Pod> = owned_pods.iter_mut().map(|v| v.as_pod()).collect();

    // For dmabuf transport, disable MAP_BUFFERS (PW would try to mmap the
    // dmabuf which is wrong for hardware buffers). The DRIVER flag stays —
    // we're still the source-of-truth for frame rate.
    let stream_flags = if is_dmabuf {
        pw::stream::StreamFlags::DRIVER
    } else {
        pw::stream::StreamFlags::DRIVER | pw::stream::StreamFlags::MAP_BUFFERS
    };
    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            stream_flags,
            &mut params,
        )
        .map_err(|e| format!("stream connect: {e}"))?;

    // Poll the assigned node_id once it becomes valid, then report it back.
    let node_id_reporter = {
        let stream = stream.clone();
        let node_tx = node_tx.clone();
        let mainloop_weak = mainloop.downgrade();
        let reported = std::cell::Cell::new(false);
        mainloop.loop_().add_timer(move |_| {
            if !reported.get() {
                let id = stream.node_id();
                if id != pw::constants::ID_ANY {
                    info!("pw stream node_id assigned: {id}");
                    let _ = node_tx.send(Ok(id));
                    reported.set(true);
                }
            }
            if shutdown_rx.try_recv().is_ok() {
                if let Some(ml) = mainloop_weak.upgrade() {
                    ml.quit();
                }
            }
        })
    };
    let _ = node_id_reporter.update_timer(
        Some(Duration::from_millis(50)),
        Some(Duration::from_millis(100)),
    );

    info!("pw mainloop running for stream `{node_name}`");
    mainloop.run();

    drop(node_id_reporter);
    let _ = stream.disconnect();
    info!("pw mainloop exited for stream `{node_name}`");
    Ok(())
}

/// Build the params slice we hand to `stream.connect()`. For SHM we send just
/// an EnumFormat. For dmabuf we send EnumFormat (with VideoModifier=0 pinned)
/// + a Buffers param declaring `DataType::DmaBuf`, `buffers=3`,
/// `size=stride*height`.
fn build_connect_params(spec: &StreamSpec, source: &Source) -> Result<Vec<OwnedPod>, String> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use spa::param::video::VideoFormat;
    use spa::pod::{Property, PropertyFlags, Value, serialize::PodSerializer};
    use spa::utils::{Fraction, Rectangle, SpaTypes};

    let mut pods = Vec::new();

    // EnumFormat — same shape for both transports, but dmabuf adds the
    // VideoModifier property pinned to MOD_LINEAR (0). PW uses this to drive
    // the correct buffer-allocation path on the consumer side.
    let mut format_props = vec![
        pw::spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pw::spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pw::spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::RGBx,
            VideoFormat::RGBA,
            VideoFormat::BGRA,
        ),
        pw::spa::pod::property!(
            FormatProperties::VideoSize,
            Rectangle,
            Rectangle {
                width: spec.width,
                height: spec.height,
            }
        ),
        pw::spa::pod::property!(
            FormatProperties::VideoFramerate,
            Fraction,
            Fraction {
                num: spec.framerate_num,
                denom: spec.framerate_den,
            }
        ),
    ];
    if let Source::Dmabuf(b) = source {
        // VideoModifier as Long, MANDATORY so consumers that don't support
        // modifiers are filtered out. Pinned to whatever the bridge offers
        // (we only ever set MOD_LINEAR = 0 in this slice, but the field
        // exists for future expansion).
        format_props.push(Property {
            key: libspa_sys::SPA_FORMAT_VIDEO_modifier,
            flags: PropertyFlags::MANDATORY,
            value: Value::Long(b.modifier as i64),
        });
    }
    let fmt_obj = spa::pod::Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: format_props,
    };
    let bytes = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(fmt_obj))
        .map_err(|e| format!("serialize EnumFormat: {e}"))?
        .0
        .into_inner();
    pods.push(OwnedPod(bytes));

    // Buffers param — only emitted for dmabuf so PW pre-allocates the right
    // shape. For SHM we let PW pick defaults (matches the prior code).
    if let Source::Dmabuf(b) = source {
        let buffers_obj = spa::pod::Object {
            type_: SpaTypes::ObjectParamBuffers.as_raw(),
            id: spa::param::ParamType::Buffers.as_raw(),
            properties: vec![
                Property::new(libspa_sys::SPA_PARAM_BUFFERS_buffers, Value::Int(b.fds.len() as i32)),
                Property::new(libspa_sys::SPA_PARAM_BUFFERS_blocks, Value::Int(1)),
                Property::new(libspa_sys::SPA_PARAM_BUFFERS_size, Value::Int(b.size as i32)),
                Property::new(libspa_sys::SPA_PARAM_BUFFERS_stride, Value::Int(b.stride as i32)),
                Property::new(
                    libspa_sys::SPA_PARAM_BUFFERS_dataType,
                    Value::Int(1 << libspa_sys::SPA_DATA_DmaBuf),
                ),
            ],
        };
        let bytes = PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &Value::Object(buffers_obj),
        )
        .map_err(|e| format!("serialize Buffers: {e}"))?
        .0
        .into_inner();
        pods.push(OwnedPod(bytes));
    }

    // Suppress unused-import warning when dmabuf isn't compiled in.
    let _ = dmabuf::MOD_LINEAR;
    Ok(pods)
}

/// Self-owned byte buffer wrapping a serialized SPA pod. Survives long
/// enough for `stream.connect()` to copy it into the C side.
pub struct OwnedPod(Vec<u8>);

impl OwnedPod {
    pub fn as_pod(&mut self) -> &Pod {
        Pod::from_bytes(&self.0).expect("pod always parses (round-trip from serialize)")
    }
}
