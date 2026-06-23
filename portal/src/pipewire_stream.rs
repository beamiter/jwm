//! PipeWire producer node for a single ScreenCast source.
//!
//! Per Start() invocation, [`spawn`] creates an OS thread that owns a
//! `pw::MainLoopRc` + a `Video/Source` stream. The thread emits one node and
//! returns the assigned `node_id` to the caller via a sync_channel; the node
//! survives until the returned `StreamHandle` is dropped.
//!
//! **MVP scope.** The stream is wired with format negotiation (BGRx /
//! RGBx / RGBA / BGRA) and a process callback that dequeues + zero-fills
//! buffers, so the node is visible and pollable by OBS / Chrome but does
//! not yet copy real Wayland frames into the buffers — that bridge lands
//! in [`crate::capture`] in the next slice.

use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use log::{info, warn};
use pipewire as pw;
use pw::spa;
use spa::pod::Pod;

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

/// Spawn a PipeWire stream worker. Returns once the worker has reported its
/// assigned PipeWire node_id (or fails after a 5s startup deadline).
pub fn spawn(spec: StreamSpec, node_name: String) -> Result<StreamHandle, String> {
    let (node_tx, node_rx) = mpsc::sync_channel::<Result<u32, String>>(1);
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();

    let join = std::thread::Builder::new()
        .name("jwm-portal-pw".into())
        .spawn(move || {
            if let Err(e) = run(spec, node_name, node_tx.clone(), shutdown_rx) {
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
        .process(|stream, _| {
            // MVP: drain + re-queue empty buffers. crate::capture will fill
            // `buffer.datas_mut()[0]` with real Wayland pixels before queue.
            if let Some(mut buffer) = stream.dequeue_buffer() {
                let datas = buffer.datas_mut();
                if let Some(data) = datas.first_mut() {
                    let chunk = data.chunk_mut();
                    *chunk.offset_mut() = 0;
                    *chunk.stride_mut() = 0;
                    *chunk.size_mut() = 0;
                }
            }
        })
        .register()
        .map_err(|e| format!("register listener: {e}"))?;

    let mut owned_pods = build_format_params(&spec)?;
    let mut params: Vec<&Pod> = owned_pods.iter_mut().map(|v| v.as_pod()).collect();

    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::DRIVER | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| format!("stream connect: {e}"))?;

    // Poll the assigned node_id once it becomes valid, then report it back.
    // ID_ANY (0xffffffff) means "not yet assigned"; the loop iterates until
    // the registry returns the real id.
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

/// Build the `EnumFormat` param the stream advertises in `connect()`. We
/// declare a single concrete size + framerate so PW negotiation completes
/// without back-and-forth — the Wayland session has already pinned both.
fn build_format_params(spec: &StreamSpec) -> Result<Vec<OwnedPod>, String> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use spa::param::video::VideoFormat;
    use spa::pod::{Value, serialize::PodSerializer};
    use spa::utils::{Fraction, Rectangle, SpaTypes};

    let obj = pw::spa::pod::object!(
        SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
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
    );

    let bytes = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
        .map_err(|e| format!("serialize EnumFormat: {e}"))?
        .0
        .into_inner();
    Ok(vec![OwnedPod(bytes)])
}

/// Self-owned byte buffer wrapping a serialized SPA pod. Survives long
/// enough for `stream.connect()` to copy it into the C side.
pub struct OwnedPod(Vec<u8>);

impl OwnedPod {
    pub fn as_pod(&mut self) -> &Pod {
        Pod::from_bytes(&self.0).expect("pod always parses (round-trip from serialize)")
    }
}
