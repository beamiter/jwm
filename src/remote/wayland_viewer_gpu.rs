//! Immutable DMA-BUF presentation on the viewer's existing Wayland role.

use super::{State as ViewerState, WaylandViewer, damage::Layout, dimensions, wl_buffer};
use crate::remote::{
    RemoteResult,
    frame::{DamageRegions, FrameInfo},
    gpu_import::XRGB8888,
    gpu_present::{Renderer, RgbBuffer, same_drm_device},
    telemetry::{NativeStage, NativeStageTimes},
};
use ffmpeg_next::frame::Video;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use std::{
    fs::File,
    os::fd::{AsFd, OwnedFd},
    os::unix::fs::FileExt,
    time::{Duration, Instant},
};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self as params, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_feedback_v1::{self as feedback, ZwpLinuxDmabufFeedbackV1},
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};

const IPC_LIMIT: Duration = Duration::from_millis(250);
const BUFFER_LIMIT: usize = 2;

#[derive(Clone, Copy, PartialEq, Eq)]
struct BackingScope {
    role: u64,
    generation: u64,
    source: (u64, u64),
    device: u64,
    size: (u16, u16),
}

#[derive(Default)]
pub(super) struct State {
    pub(super) global: Option<u32>,
    manager: Option<ZwpLinuxDmabufV1>,
    feedback: Option<ZwpLinuxDmabufFeedbackV1>,
    formats: Vec<bool>,
    main: Option<u64>,
    target: Option<u64>,
    tranche_linear: bool,
    main_linear: bool,
    capability: Option<Option<u64>>,
    request: Option<ZwpLinuxBufferParamsV1>,
    imported: Option<Result<Buffer, ()>>,
    buffers: Vec<Buffer>,
    reuse_rgb: bool,
    reuse_reported: bool,
    backing_scope: Option<BackingScope>,
    idle: Vec<RgbBuffer>,
}

struct Buffer {
    object: wl_buffer::WlBuffer,
    backing: Option<RgbBuffer>,
    retired: bool,
}

impl Drop for Buffer {
    fn drop(&mut self) {
        self.object.destroy();
    }
}

impl State {
    pub(super) fn new() -> Self {
        Self {
            // Experimental for the native view-only Wayland path.
            reuse_rgb: matches!(
                std::env::var("JWM_REMOTE_WAYLAND_REUSE_RGB").as_deref(),
                Ok("1")
            ),
            ..Self::default()
        }
    }

    pub(super) fn storage_blocked(&self) -> bool {
        self.buffers.len() >= BUFFER_LIMIT
    }

    pub(super) fn invalidate_backings(&mut self) {
        if self.backing_scope.take().is_some() {
            self.idle.clear();
            for buffer in &mut self.buffers {
                buffer.retired = true;
            }
        }
    }

    pub(super) fn reset_role(&mut self) {
        self.invalidate_backings();
        self.buffers.clear();
    }

    pub(super) fn leave_native(&mut self) {
        if self.reuse_rgb {
            // The role is still alive. Keep submitted, retired backing until
            // its exact release while the viewer switches to SHM.
            self.invalidate_backings();
        } else {
            self.buffers.clear();
        }
    }

    fn checkout(&mut self, scope: BackingScope) -> Option<RgbBuffer> {
        if !self.reuse_rgb {
            return None;
        }
        if self.backing_scope != Some(scope) {
            self.invalidate_backings();
            self.backing_scope = Some(scope);
        }
        self.idle.pop()
    }

    fn release(&mut self, object: &wl_buffer::WlBuffer) {
        if let Some(index) = self
            .buffers
            .iter()
            .position(|entry| entry.object == *object)
        {
            let mut buffer = self.buffers.swap_remove(index);
            if !buffer.retired
                && let Some(backing) = buffer.backing.take()
            {
                self.idle.push(backing);
            }
        }
    }

    pub(super) fn remove_global(&mut self, name: u32) {
        if self.global != Some(name) {
            return;
        }
        self.global = None;
        if let Some(feedback) = self.feedback.take() {
            feedback.destroy();
        }
        if let Some(manager) = self.manager.take() {
            manager.destroy();
        }
        self.capability = Some(None);
        self.main = None;
        self.target = None;
        self.tranche_linear = false;
        self.main_linear = false;
        self.formats.clear();
        self.invalidate_backings();
    }

    fn supports(&self, device: u64) -> bool {
        self.capability
            .flatten()
            .is_some_and(|main| same_drm_device(main, device))
    }
}

impl WaylandViewer {
    fn wait_gpu(&mut self, ready: impl Fn(&State) -> bool) -> RemoteResult<()> {
        let deadline = Instant::now() + IPC_LIMIT;
        loop {
            self.pump()?;
            if self.state.closed {
                return Err("Wayland viewer closed during DMA-BUF setup".into());
            }
            if ready(&self.state.gpu) {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("Wayland DMA-BUF request timed out".into());
            }
            self.prepare_wait()?;
            let mut flags = PollFlags::POLLIN;
            if self.write_pending.get() {
                flags |= PollFlags::POLLOUT;
            }
            let mut fds = [PollFd::new(self.conn.as_fd(), flags)];
            match poll(&mut fds, PollTimeout::try_from(remaining)?) {
                Ok(_) | Err(nix::errno::Errno::EINTR) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn gpu_capability(&mut self, device: u64) -> RemoteResult<()> {
        if self.state.gpu.manager.is_none() {
            let name = self
                .state
                .gpu
                .global
                .ok_or("no Wayland DMA-BUF v4 global")?;
            let qh = self.queue.handle();
            let manager: ZwpLinuxDmabufV1 = self.registry.bind(name, 4, &qh, ());
            self.state.gpu.capability = None;
            self.state.gpu.feedback = Some(manager.get_default_feedback(&qh, ()));
            self.state.gpu.manager = Some(manager);
        }
        if self.state.gpu.capability.is_none() {
            self.wait_gpu(|state| state.capability.is_some())?;
        }
        if !self.state.gpu.supports(device) {
            return Err("Wayland compositor lacks same-device linear XRGB8888 DMA-BUF".into());
        }
        Ok(())
    }

    fn import_gpu_buffer(&mut self, rgb: &RgbBuffer, device: u64) -> RemoteResult<Buffer> {
        let qh = self.queue.handle();
        let request = self
            .state
            .gpu
            .manager
            .as_ref()
            .ok_or("Wayland DMA-BUF global removed")?
            .create_params(&qh, ());
        let (fd, stride) = rgb.wayland_plane();
        let (width, height) = rgb.dimensions();
        request.add(fd, 0, 0, stride, 0, 0);
        request.create(
            i32::from(width),
            i32::from(height),
            XRGB8888,
            params::Flags::empty(),
        );
        self.state.gpu.request = Some(request.clone());
        let wait = self.wait_gpu(|state| !state.supports(device) || state.imported.is_some());
        self.state.gpu.request = None;
        request.destroy();
        let imported = self.state.gpu.imported.take();
        wait?;
        if !self.state.gpu.supports(device) {
            return Err("Wayland DMA-BUF capability withdrawn".into());
        }
        imported
            .and_then(Result::ok)
            .ok_or_else(|| "Wayland compositor rejected the native video buffer".into())
    }

    pub(in crate::remote) fn draw_native(
        &mut self,
        renderer: &mut Renderer,
        device: u64,
        pixels: &Video,
        info: FrameInfo,
        acknowledge: bool,
        mut timings: Option<&mut NativeStageTimes>,
    ) -> RemoteResult<bool> {
        self.pump()?;
        if !self.can_submit_frame() {
            return Ok(false);
        }
        if !self.state.is_view_only_session() {
            return Err("Wayland DMA-BUF video requires a view-only session".into());
        }
        self.gpu_capability(device)?;
        let scope = (info.source_epoch, info.geometry_epoch);
        self.prepare_draw_scope(scope)?;
        let encoded = (
            u16::try_from(info.encoded_size.0)?,
            u16::try_from(info.encoded_size.1)?,
        );

        // The compositor can resize while resolving an import. Retry using
        // the current layout before attaching any pixels to the surface.
        for _ in 0..2 {
            if !self.can_submit_frame() {
                return Ok(false);
            }
            let stamp = (self.state.role_id, self.state.generation);
            let size = self.state.buffer_size();
            dimensions(size.0, size.1)?;
            let layout = Layout::new(encoded, size).ok_or("invalid Wayland GPU video layout")?;
            let content = (
                layout.offset.0,
                layout.offset.1,
                layout.content.0,
                layout.content.1,
            );
            let backing_scope = BackingScope {
                role: stamp.0,
                generation: stamp.1,
                source: scope,
                device,
                size,
            };
            let reusable = self.state.gpu.checkout(backing_scope);
            let reused = reusable.is_some();
            let rgb = renderer.render_to(
                pixels,
                encoded,
                size,
                content,
                reusable,
                timings.as_deref_mut(),
            );
            #[cfg(all(test, feature = "backend-wayland-udev"))]
            if rgb.is_err() {
                crate::remote::gpu_present::fence_test::observe_wayland_pool_after_render_error(
                    self.state.gpu.idle.iter().chain(
                        self.state
                            .gpu
                            .buffers
                            .iter()
                            .filter_map(|buffer| buffer.backing.as_ref()),
                    ),
                );
            }
            let rgb = rgb?;
            // Give each attachment its own proxy, even on a reused allocation.
            // An old release can never make a new attachment writable.
            let mut buffer = {
                let _timer = timings
                    .as_deref_mut()
                    .map(|timings| timings.start(NativeStage::WaylandDmabufImport));
                self.import_gpu_buffer(&rgb, device)?
            };
            if stamp != (self.state.role_id, self.state.generation)
                || (self.state.gpu.reuse_rgb && self.state.gpu.backing_scope != Some(backing_scope))
            {
                continue;
            }
            if !self.can_submit_frame() {
                return Ok(false);
            }
            self.last_frame = None;
            self.commit_buffer(
                &buffer.object,
                info.sequence,
                scope,
                encoded,
                info.source_size,
                DamageRegions::default(),
                acknowledge,
            )?;
            if self.state.gpu.reuse_rgb {
                buffer.backing = Some(rgb);
            }
            self.state.gpu.buffers.push(buffer);
            debug_assert!(self.state.gpu.buffers.len() + self.state.gpu.idle.len() <= BUFFER_LIMIT);
            if reused && !self.state.gpu.reuse_reported {
                eprintln!("jwm-remote: native Wayland XRGB DMA-BUF backing reuse active");
                self.state.gpu.reuse_reported = true;
            }
            return Ok(true);
        }
        Err("Wayland viewport changed repeatedly during DMA-BUF import".into())
    }

    pub(in crate::remote) fn remember_native(&mut self, pixels: Video, info: FrameInfo) {
        self.last_native = Some((pixels, info));
    }

    pub(in crate::remote) fn take_native_redraw(
        &mut self,
    ) -> RemoteResult<Option<(Video, FrameInfo, bool)>> {
        self.pump()?;
        if !self.can_submit_frame() || self.state.viewport_redraw.is_none() {
            return Ok(None);
        }
        let acknowledge = self.state.viewport_redraw == Some(super::ViewportRedraw::AwaitingAck);
        Ok(self
            .last_native
            .take()
            .map(|(pixels, info)| (pixels, info, acknowledge)))
    }
}

fn read_formats(fd: OwnedFd, size: u32) -> Option<Vec<bool>> {
    let entries = 16;
    if size == 0 || size % entries != 0 || size > (u32::from(u16::MAX) + 1) * entries {
        return None;
    }
    let file = File::from(fd);
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() < u64::from(size) {
        return None;
    }
    let mut table = vec![0; size as usize];
    file.read_exact_at(&mut table, 0).ok()?;
    Some(
        table
            .chunks_exact(entries as usize)
            .map(|entry| entry[..4] == XRGB8888.to_ne_bytes() && entry[8..] == [0; 8])
            .collect(),
    )
}

impl Dispatch<ZwpLinuxDmabufFeedbackV1, ()> for ViewerState {
    fn event(
        state: &mut Self,
        object: &ZwpLinuxDmabufFeedbackV1,
        event: feedback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let gpu = &mut state.gpu;
        if gpu.feedback.as_ref() != Some(object) {
            return;
        }
        match event {
            feedback::Event::FormatTable { fd, size } => {
                gpu.formats = read_formats(fd, size).unwrap_or_default();
            }
            feedback::Event::MainDevice { device } => {
                gpu.main = device.as_slice().try_into().ok().map(u64::from_ne_bytes);
            }
            feedback::Event::TrancheTargetDevice { device } => {
                gpu.target = device.as_slice().try_into().ok().map(u64::from_ne_bytes);
            }
            feedback::Event::TrancheFormats { indices } if indices.len() % 2 == 0 => {
                gpu.tranche_linear |= indices.chunks_exact(2).any(|index| {
                    let index = usize::from(u16::from_ne_bytes([index[0], index[1]]));
                    gpu.formats.get(index) == Some(&true)
                });
            }
            feedback::Event::TrancheDone => {
                gpu.main_linear |= gpu.tranche_linear
                    && gpu
                        .main
                        .zip(gpu.target.take())
                        .is_some_and(|(main, target)| same_drm_device(main, target));
                gpu.target = None;
                gpu.tranche_linear = false;
            }
            feedback::Event::Done => {
                let main_linear = std::mem::take(&mut gpu.main_linear);
                let capability = Some(gpu.main.take().filter(|_| main_linear));
                if gpu.capability != capability {
                    gpu.invalidate_backings();
                }
                gpu.capability = capability;
            }
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(ViewerState: ignore ZwpLinuxDmabufV1);

impl Dispatch<ZwpLinuxBufferParamsV1, ()> for ViewerState {
    wayland_client::event_created_child!(ViewerState, ZwpLinuxBufferParamsV1, [0 => (wl_buffer::WlBuffer, GpuBuffer)]);

    fn event(
        state: &mut Self,
        request: &ZwpLinuxBufferParamsV1,
        event: params::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let matches = state.gpu.request.as_ref() == Some(request) && state.gpu.imported.is_none();
        match event {
            params::Event::Created { buffer } => {
                let buffer = Buffer {
                    object: buffer,
                    backing: None,
                    retired: false,
                };
                if matches {
                    state.gpu.imported = Some(Ok(buffer));
                }
            }
            params::Event::Failed if matches => state.gpu.imported = Some(Err(())),
            _ => {}
        }
    }
}

struct GpuBuffer;

impl Dispatch<wl_buffer::WlBuffer, GpuBuffer> for ViewerState {
    fn event(
        state: &mut Self,
        buffer: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        _: &GpuBuffer,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            state.gpu.release(buffer);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::fd::AsRawFd, os::unix::net::UnixStream};
    use wayland_client::protocol::wl_shm;

    fn test_buffers() -> (Connection, UnixStream, [wl_buffer::WlBuffer; 3]) {
        let (client, peer) = UnixStream::pair().unwrap();
        let conn = Connection::from_socket(client).unwrap();
        let queue = conn.new_event_queue::<ViewerState>();
        let qh = queue.handle();
        let registry = conn.display().get_registry(&qh, ());
        let shm = registry.bind::<wl_shm::WlShm, _, _>(1, 1, &qh, ());
        let placeholder = File::open("/dev/null").unwrap();
        let pool = shm.create_pool(placeholder.as_fd(), 4, &qh, ());
        // Only proxy identity is exercised. No requests go to a server.
        let buffers = std::array::from_fn(|_| {
            pool.create_buffer(0, 1, 1, 4, wl_shm::Format::Xrgb8888, &qh, GpuBuffer)
        });
        (conn, peer, buffers)
    }

    const SCOPE: BackingScope = BackingScope {
        role: 1,
        generation: 1,
        source: (1, 1),
        device: 1,
        size: (1, 1),
    };

    fn backing() -> RgbBuffer {
        RgbBuffer::test_backing(File::open("/dev/null").unwrap().into(), SCOPE.size)
    }

    fn submit(state: &mut State, object: &wl_buffer::WlBuffer, backing: RgbBuffer) {
        state.buffers.push(Buffer {
            object: object.clone(),
            backing: Some(backing),
            retired: false,
        });
    }

    #[test]
    fn native_rgb_reuse_requires_release_of_the_exact_attachment() {
        let (_conn, _peer, [first, second, other]) = test_buffers();
        let mut state = State {
            reuse_rgb: true,
            ..State::default()
        };
        assert!(state.checkout(SCOPE).is_none());
        let allocated = backing();
        let original_fd = allocated.wayland_plane().0.as_raw_fd();
        submit(&mut state, &first, allocated);
        state.release(&other);
        assert!(state.checkout(SCOPE).is_none());
        state.release(&first);
        let released = state.checkout(SCOPE).unwrap();
        assert_eq!(released.wayland_plane().0.as_raw_fd(), original_fd);
        submit(&mut state, &second, released);
        state.release(&first);
        assert!(state.checkout(SCOPE).is_none());
        state.release(&second);
        let released = state.checkout(SCOPE).unwrap();
        assert_eq!(released.wayland_plane().0.as_raw_fd(), original_fd);
    }

    #[test]
    fn native_rgb_reuse_retires_busy_storage_across_scope_and_fallback() {
        let (_conn, _peer, [first, second, next]) = test_buffers();
        let mut state = State {
            reuse_rgb: true,
            ..State::default()
        };
        assert!(state.checkout(SCOPE).is_none());
        submit(&mut state, &first, backing());
        submit(&mut state, &second, backing());
        assert!(state.storage_blocked());
        state.release(&first);
        assert!(!state.storage_blocked());
        assert_eq!(state.idle.len(), 1);
        let resized = BackingScope {
            generation: 2,
            ..SCOPE
        };
        assert!(state.checkout(resized).is_none());
        assert_eq!(state.buffers.len(), 1);
        assert!(state.buffers[0].backing.is_some());
        state.release(&second);
        assert!(state.checkout(resized).is_none());
        submit(&mut state, &next, backing());
        state.leave_native();
        assert_eq!(state.buffers.len(), 1);
        assert!(state.buffers[0].retired);
        state.reset_role();
        assert!(state.buffers.is_empty());
        assert!(state.idle.is_empty());
    }
}
