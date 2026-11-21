//! Configure and retire opt-in native H.264 presentation.

use super::{ClientOptions, RECEIVER_JOIN_TIMEOUT, ReceiverExit, ReceiverThread};
use crate::remote::{
    RemoteResult,
    frame::{FrameInfo, RecyclableDecodedFrame, SharedDecodeBufferPool, prepare_decode_buffer},
    gpu_present::{self, RgbAllocator, RgbBuffer},
    h264,
    messages::FrameCodec,
    telemetry::{NativeStage, NativeStageTimes},
    viewer_backend::{Viewer, ViewerBackend},
    x11_viewer,
};
use ffmpeg_next::frame::Video;
#[cfg(feature = "remote-wayland")]
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::{
    ffi::CString,
    io,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

#[derive(Clone)]
pub(super) struct Decode {
    node: CString,
    backend: ViewerBackend,
    enabled: Arc<AtomicBool>,
}

impl Decode {
    pub(super) fn open(&self, size: (u32, u32)) -> Option<h264::Decoder> {
        if !self.active() {
            return None;
        }
        match h264::Decoder::native_vaapi(size.0, size.1, &self.node) {
            Ok(decoder) => Some(decoder),
            Err(error) => {
                self.disable(
                    "Trying RGB fallback",
                    &format!("opening the selected VA-API decoder: {error}"),
                );
                None
            }
        }
    }

    pub(super) fn active(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    fn disable(&self, action: &str, reason: &str) {
        if self.enabled.swap(false, Ordering::AcqRel) {
            eprintln!(
                "jwm-remote: native {} video disabled. {action} ({reason})",
                self.backend
            );
        }
    }
}

pub(super) struct Presenter {
    decode: Decode,
    renderer: gpu_present::Renderer,
    transfer: h264::RgbTransfer,
    reported_active: bool,
    x11_target_prefetch: Option<X11TargetPrefetch>,
    #[cfg(feature = "remote-wayland")]
    device: u64,
    #[cfg(feature = "remote-wayland")]
    measure_wayland_stages: bool,
}

struct X11TargetPrefetch {
    jobs: Option<mpsc::SyncSender<(u16, u16)>>,
    ready: Option<mpsc::Receiver<RemoteResult<(RgbBuffer, Duration)>>>,
    thread: Option<ReceiverThread>,
    pending: bool,
}

impl X11TargetPrefetch {
    fn start(node: PathBuf) -> io::Result<Self> {
        let (jobs, requests) = mpsc::sync_channel(1);
        let (replies, ready) = mpsc::sync_channel(1);
        let thread = ReceiverThread::spawn_named("jwm-remote-x11-target", move || {
            let Ok(first) = requests.recv() else {
                return;
            };
            let allocator = match RgbAllocator::open(&node) {
                Ok(allocator) => allocator,
                Err(error) => {
                    let _ = replies.send(Err(error));
                    return;
                }
            };
            for size in std::iter::once(first).chain(&requests) {
                let started = Instant::now();
                // The FD owns fresh storage after dropping its GBM wrapper.
                // The worker has never drawn into or exported it to X11.
                let result = allocator
                    .allocate(size)
                    .map(|(_storage, target)| (target, started.elapsed()));
                let failed = result.is_err();
                if replies.send(result).is_err() || failed {
                    break;
                }
            }
        })?;
        Ok(Self {
            jobs: Some(jobs),
            ready: Some(ready),
            thread: Some(thread),
            pending: false,
        })
    }

    fn next(&mut self, size: (u16, u16)) -> Option<(RgbBuffer, Duration)> {
        let available = match self.ready.as_ref()?.try_recv() {
            Ok(Ok((target, elapsed))) => {
                self.pending = false;
                (target.dimensions() == size).then_some((target, elapsed))
            }
            Err(mpsc::TryRecvError::Empty) => None,
            Ok(Err(error)) => {
                eprintln!("jwm-remote: X11 RGB preallocation stopped: {error}");
                self.cancel();
                return None;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.cancel();
                return None;
            }
        };
        // There is at most one allocation running or awaiting checkout. A
        // miss leaves the presenter's existing synchronous allocation in use.
        if !self.pending && can_prefetch_target(size) {
            self.pending = self.jobs.as_ref()?.try_send(size).is_ok();
        }
        available
    }

    fn cancel(&mut self) {
        self.jobs = None;
        self.ready = None;
    }

    fn close(&mut self) -> RemoteResult<()> {
        self.cancel();
        if let Some(thread) = self.thread.take() {
            match thread.join_timeout(RECEIVER_JOIN_TIMEOUT) {
                ReceiverExit::Completed => (),
                exit => {
                    return Err(io::Error::other(format!(
                        "X11 RGB preallocation worker did not close cleanly: {exit:?}"
                    ))
                    .into());
                }
            }
        }
        Ok(())
    }
}

impl Drop for X11TargetPrefetch {
    fn drop(&mut self) {
        if let Err(error) = self.close() {
            eprintln!("jwm-remote: {error}");
        }
    }
}

fn can_prefetch_target((width, height): (u16, u16)) -> bool {
    (1..=4096).contains(&width)
        && (1..=4096).contains(&height)
        && u32::from(width) * u32::from(height) <= 4096 * 2160
}

#[cfg(test)]
#[test]
fn x11_target_prefetch_bounds_work_and_drops_a_different_size() {
    use std::fs::File;
    let (jobs, requests) = mpsc::sync_channel(1);
    let (replies, ready) = mpsc::sync_channel(1);
    let mut worker = X11TargetPrefetch {
        jobs: Some(jobs),
        ready: Some(ready),
        thread: None,
        pending: false,
    };
    assert!(worker.next((4096, 4096)).is_none());
    assert!(matches!(
        requests.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));

    let initial = (321, 241);
    let resized = (320, 240);
    assert!(worker.next(initial).is_none());
    assert_eq!(requests.try_recv().unwrap(), initial);
    assert!(worker.next(resized).is_none());
    assert!(matches!(
        requests.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));

    let buffer = |size| RgbBuffer::test_backing(File::open("/dev/null").unwrap().into(), size);
    replies
        .send(Ok((buffer(initial), Duration::from_millis(2))))
        .unwrap();
    assert!(worker.next(resized).is_none());
    assert_eq!(requests.try_recv().unwrap(), resized);

    replies
        .send(Ok((buffer(resized), Duration::from_millis(4))))
        .unwrap();
    let (target, elapsed) = worker.next(resized).unwrap();
    assert_eq!(target.dimensions(), resized);
    assert_eq!(elapsed, Duration::from_millis(4));
    assert_eq!(requests.try_recv().unwrap(), resized);
    worker.close().unwrap();
    assert!(replies.send(Ok((buffer(resized), Duration::ZERO))).is_err());
}

pub(super) struct Submission {
    pub(super) drawn: bool,
    pub(super) native: Option<(&'static str, Duration)>,
    pub(super) fallback: Option<h264::DecodeTimings>,
    pub(super) native_stages: Option<NativeStageTimes>,
}

impl Presenter {
    pub(super) fn configure(options: &ClientOptions, codec: FrameCodec) -> Option<Self> {
        let node = std::env::var_os("JWM_REMOTE_GPU_PRESENT")?;
        let backend = options.viewer_backend;
        let result = if codec == FrameCodec::H264
            && options.scaling != crate::remote::ScalingPolicy::Cpu
        {
            match backend {
                ViewerBackend::X11 => Self::open_x11(Path::new(&node), options.display.as_deref()),
                #[cfg(feature = "remote-wayland")]
                ViewerBackend::Wayland if options.view_only => {
                    Self::open_wayland(Path::new(&node), options.stats_json)
                }
                _ => Err("native Wayland video requires view-only mode".into()),
            }
        } else {
            Err("native video requires negotiated H.264 and GPU-compatible scaling".into())
        };
        match result {
            Ok(presenter) => Some(presenter),
            Err(error) => {
                eprintln!("jwm-remote: native {backend} video unavailable; using RGB ({error})");
                None
            }
        }
    }

    fn open_x11(path: &Path, display: Option<&str>) -> RemoteResult<Self> {
        let (conn, root) = x11_viewer::open_root(display)?;
        let path = gpu_present::select_device(&conn, root, path)?;
        Self::open(&path, ViewerBackend::X11)
    }

    #[cfg(feature = "remote-wayland")]
    fn open_wayland(path: &Path, stats_json: bool) -> RemoteResult<Self> {
        let path = path.canonicalize()?;
        if !path.metadata()?.file_type().is_char_device()
            || !path
                .file_name()
                .is_some_and(|name| name.as_encoded_bytes().starts_with(b"renderD"))
        {
            return Err("Wayland GPU presentation requires a DRM render node".into());
        }
        let mut presenter = Self::open(&path, ViewerBackend::Wayland)?;
        presenter.measure_wayland_stages = stats_json;
        Ok(presenter)
    }

    fn open(path: &Path, backend: ViewerBackend) -> RemoteResult<Self> {
        let renderer = gpu_present::Renderer::new(path)?;
        let x11_target_prefetch = (backend == ViewerBackend::X11
            && std::env::var_os("JWM_REMOTE_X11_PREFETCH_TARGET").as_deref() == Some("1".as_ref()))
        .then(|| match X11TargetPrefetch::start(path.to_owned()) {
            Ok(worker) => Some(worker),
            Err(error) => {
                eprintln!("jwm-remote: X11 RGB preallocation unavailable: {error}");
                None
            }
        })
        .flatten();
        let decode = Decode {
            node: CString::new(path.as_os_str().as_bytes())?,
            backend,
            enabled: Arc::new(AtomicBool::new(true)),
        };
        Ok(Self {
            decode,
            renderer,
            transfer: h264::RgbTransfer::default(),
            reported_active: false,
            x11_target_prefetch,
            #[cfg(feature = "remote-wayland")]
            device: path.metadata()?.rdev(),
            #[cfg(feature = "remote-wayland")]
            measure_wayland_stages: false,
        })
    }

    pub(super) fn decoder_config(&self) -> Decode {
        self.decode.clone()
    }

    pub(super) fn close_prefetch(&mut self) -> RemoteResult<()> {
        if let Some(prefetch) = self.x11_target_prefetch.as_mut() {
            #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
            let pending_request = prefetch.thread.as_ref().map(|_| prefetch.pending);
            prefetch.close()?;
            #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
            if let Some(pending) = pending_request {
                gpu_present::fence_test::x11_prefetch_joined(pending)?;
            }
        }
        Ok(())
    }

    pub(super) fn draw(
        &mut self,
        viewer: &mut Viewer,
        pixels: Video,
        info: FrameInfo,
        pool: &SharedDecodeBufferPool,
        measure: bool,
    ) -> RemoteResult<Submission> {
        self.draw_frame(viewer, pixels, info, pool, measure, None)
    }

    #[cfg(feature = "remote-wayland")]
    pub(super) fn refresh_wayland(
        &mut self,
        viewer: &mut Viewer,
        pool: &SharedDecodeBufferPool,
        measure: bool,
    ) -> RemoteResult<Option<(u64, Submission)>> {
        let Viewer::Wayland(wayland) = viewer else {
            return Ok(None);
        };
        let Some((pixels, info, acknowledge)) = wayland.take_native_redraw()? else {
            return Ok(None);
        };
        self.draw_frame(viewer, pixels, info, pool, measure, Some(acknowledge))
            .map(|submitted| Some((info.sequence, submitted)))
    }

    fn draw_frame(
        &mut self,
        viewer: &mut Viewer,
        pixels: Video,
        info: FrameInfo,
        pool: &SharedDecodeBufferPool,
        measure: bool,
        redraw: Option<bool>,
    ) -> RemoteResult<Submission> {
        if self.decode.active() {
            let started = Instant::now();
            let (path, result, native_stages) = match viewer {
                Viewer::X11(x11) => {
                    let mut stages = measure.then(NativeStageTimes::default);
                    let visible = (
                        u16::try_from(info.encoded_size.0)?,
                        u16::try_from(info.encoded_size.1)?,
                    );
                    let target = self
                        .x11_target_prefetch
                        .as_mut()
                        .and_then(|prefetch| prefetch.next(visible))
                        .map(|(target, allocation)| {
                            if let Some(stages) = stages.as_mut() {
                                stages.record(NativeStage::X11PrefetchRgbAllocation, allocation);
                            }
                            target
                        });
                    let result = match self.renderer.render_to(
                        &pixels,
                        visible,
                        visible,
                        (0, 0, visible.0, visible.1),
                        target,
                        stages.as_mut(),
                    ) {
                        Ok(buffer) => x11
                            .draw_native(
                                buffer,
                                info.sequence,
                                info.geometry_epoch,
                                info.source_size,
                                stages.as_mut(),
                            )?
                            .ok_or_else(|| "X11 rejected the native pixmap".into()),
                        Err(error) => Err(error),
                    };
                    ("x11-vaapi-dmabuf-dri3", result, stages)
                }
                #[cfg(feature = "remote-wayland")]
                Viewer::Wayland(wayland) => {
                    // Match the aggregate submission series, which excludes
                    // standalone viewport redraws and the RGB fallback path.
                    let mut stages = (self.measure_wayland_stages && redraw.is_none())
                        .then(NativeStageTimes::default);
                    (
                        "wayland-vaapi-dmabuf",
                        wayland.draw_native(
                            &mut self.renderer,
                            self.device,
                            &pixels,
                            info,
                            redraw.unwrap_or(true),
                            stages.as_mut(),
                        ),
                        stages,
                    )
                }
            };
            match result {
                Ok(drawn) => {
                    #[cfg(feature = "remote-wayland")]
                    if let Viewer::Wayland(wayland) = viewer
                        && (drawn || redraw.is_some())
                    {
                        wayland.remember_native(pixels, info);
                    }
                    if drawn && !self.reported_active {
                        let renderer = match self.decode.backend {
                            ViewerBackend::X11 => "VA-API/EGL/DRI3",
                            ViewerBackend::Wayland => "VA-API/EGL/Wayland DMA-BUF",
                        };
                        eprintln!("jwm-remote: native H.264 {renderer} video rendering active");
                        self.reported_active = true;
                    }
                    return Ok(Submission {
                        drawn,
                        native: Some((path, started.elapsed())),
                        fallback: None,
                        native_stages,
                    });
                }
                Err(error) => {
                    return self.render_failure(viewer, pixels, info, pool, measure, redraw, error);
                }
            }
        }
        self.draw_fallback(viewer, pixels, info, pool, measure, redraw)
    }

    fn render_failure(
        &mut self,
        viewer: &mut Viewer,
        pixels: Video,
        info: FrameInfo,
        pool: &SharedDecodeBufferPool,
        measure: bool,
        redraw: Option<bool>,
        error: Box<dyn std::error::Error + Send + Sync>,
    ) -> RemoteResult<Submission> {
        if error.is::<gpu_present::DecodeFenceFailure>() || self.renderer.has_unresolved_draw() {
            self.decode
                .disable("Closing the session", &error.to_string());
            self.renderer.retire_if_pending(pixels);
            return Err(super::hardware_decoder_error(std::io::Error::other(error)));
        }
        self.decode
            .disable("Trying RGB fallback", &error.to_string());
        self.draw_fallback(viewer, pixels, info, pool, measure, redraw)
    }

    fn draw_fallback(
        &mut self,
        viewer: &mut Viewer,
        pixels: Video,
        info: FrameInfo,
        pool: &SharedDecodeBufferPool,
        measure: bool,
        _redraw: Option<bool>,
    ) -> RemoteResult<Submission> {
        if let Some(prefetch) = self.x11_target_prefetch.as_mut() {
            prefetch.cancel();
        }
        // An import or format rejection leaves the decoded picture available
        // for the existing CPU presentation path, without replaying packets.
        let (frame, timings) = self
            .resolve_rgb(&pixels, info, pool, measure)
            .map_err(|error| super::hardware_decoder_error(std::io::Error::other(error)))?;
        #[cfg(feature = "remote-wayland")]
        if let Viewer::Wayland(wayland) = viewer
            && let Some(acknowledge) = _redraw
        {
            let drawn = wayland.draw_rgb(frame, acknowledge)?;
            if !drawn {
                wayland.remember_native(pixels, info);
            }
            return Ok(Submission {
                drawn,
                native: None,
                fallback: Some(timings),
                native_stages: None,
            });
        }
        Ok(Submission {
            drawn: viewer.draw_recyclable(frame)?,
            native: None,
            fallback: Some(timings),
            native_stages: None,
        })
    }

    fn resolve_rgb(
        &mut self,
        pixels: &Video,
        info: FrameInfo,
        pool: &SharedDecodeBufferPool,
        measure: bool,
    ) -> RemoteResult<(RecyclableDecodedFrame, h264::DecodeTimings)> {
        let size = info.encoded_size;
        let (mut bytes, checkout) =
            prepare_decode_buffer(pool, size.0 as usize * size.1 as usize * 3, measure)?;
        let timings = self
            .transfer
            .vaapi_fallback(pixels, size, &mut bytes, measure)?
            .with_checkout(checkout);
        let rgb = image::RgbImage::from_raw(size.0, size.1, bytes)
            .ok_or("invalid decoded RGB fallback dimensions")?;
        let frame = RecyclableDecodedFrame::new(
            info.sequence,
            info.source_size.0,
            info.source_size.1,
            rgb,
            Arc::clone(pool),
        )
        .with_session_epoch(info.session_epoch)
        .with_source_epoch(info.source_epoch)
        .with_geometry_epoch(info.geometry_epoch);
        Ok((frame, timings))
    }
}
