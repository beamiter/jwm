//! Same-device DMA-BUF capture for an explicitly selected Wayland window.
use super::*;
use crate::remote::{
    gpu_host_video::{Encoder, Frame, Pixels},
    gpu_import::XRGB8888,
    gpu_present::same_drm_device,
    h264_wire::{h264_padded_dimensions, hardware_has_idr},
    messages::FrameCodec,
};
use gbm::{BufferObjectFlags, Device, Format, Modifier};
use std::{
    fs::OpenOptions,
    os::unix::fs::{FileTypeExt, MetadataExt},
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self as params, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};

#[derive(Default)]
pub(super) struct Gpu {
    pub(super) manager: Option<(u32, ZwpLinuxDmabufV1)>,
    pub(super) compositor_device: Option<u64>,
    pub(super) xrgb_linear: bool,
    pub(super) capturing: bool,
    device: Option<Device<File>>,
    request: Option<ZwpLinuxBufferParamsV1>,
    imported: Option<Result<wl_buffer::WlBuffer, ()>>,
}

impl Gpu {
    pub(super) fn clear_constraints(&mut self) {
        self.compositor_device = None;
        self.xrgb_linear = false;
    }

    pub(super) fn remove_global(&mut self, name: u32) {
        if self.manager.as_ref().is_some_and(|(id, _)| *id == name) {
            if let Some((_, manager)) = self.manager.take() {
                manager.destroy();
            }
        }
    }

    pub(super) fn read_device(&mut self, bytes: &[u8]) {
        self.compositor_device = bytes
            .try_into()
            .ok()
            .map(u64::from_ne_bytes)
            .filter(|dev| *dev != 0);
    }

    pub(super) fn read_format(&mut self, format: u32, modifiers: &[u8]) {
        if format == XRGB8888 && modifiers.len() % 8 == 0 {
            self.xrgb_linear |= modifiers.chunks_exact(8).any(|modifier| modifier == [0; 8]);
        }
    }

    fn matches_device(&self, device: &File) -> RemoteResult<()> {
        let id = device.metadata()?.rdev();
        if self.manager.is_none()
            || !self.xrgb_linear
            || !self
                .compositor_device
                .is_some_and(|expected| same_drm_device(expected, id))
        {
            return Err(
                "Wayland capture requires same-device linear XRGB8888 DMA-BUF constraints".into(),
            );
        }
        Ok(())
    }
}

struct CaptureBuffer(wl_buffer::WlBuffer);

impl Drop for CaptureBuffer {
    fn drop(&mut self) {
        self.0.destroy();
    }
}

impl WaylandCapture {
    pub(in crate::remote) fn probe_gpu_capture(
        &mut self,
        path: &Path,
        fps: u16,
    ) -> RemoteResult<PathBuf> {
        if self.state.resize_follow
            || self.state.popups
            || self.state.interactive_popup.enabled
            || self.state.text.enabled
            || self.state.control.enabled
            || self.state.keyboard.enabled
            || self.state.pointer.enabled
        {
            return Err("Wayland DMA-BUF capture requires a fixed read-only window source".into());
        }
        let path = path.canonicalize()?;
        let file = OpenOptions::new().read(true).write(true).open(&path)?;
        if !file.metadata()?.file_type().is_char_device()
            || !path
                .file_name()
                .is_some_and(|name| name.as_encoded_bytes().starts_with(b"renderD"))
        {
            return Err("Wayland GPU capture requires a DRM render node".into());
        }
        self.state.gpu.matches_device(&file)?;
        self.state.gpu.device = Some(Device::new(file)?);
        let result = (|| {
            // Probe the real authorized source and its negotiated dimensions.
            // This frame is discarded before transport starts.
            let frame = self
                .frame_gpu()?
                .ok_or("Wayland source ended during GPU preflight")?;
            let mut encoder = Encoder::new(&path, frame.encoded, fps)?;
            let mut packet = Vec::new();
            encoder.encode(&frame.pixels, 0, true, &mut packet)?;
            if !hardware_has_idr(FrameCodec::H264, &packet) {
                return Err("Wayland VA-API capture preflight did not produce an IDR".into());
            }
            Ok(path)
        })();
        if result.is_err() {
            self.prepare_gpu_fallback();
        }
        result
    }

    pub(in crate::remote) fn prepare_gpu_fallback(&mut self) {
        self.state.gpu.device = None;
    }

    pub(in crate::remote) fn frame_gpu(&mut self) -> RemoteResult<Option<Frame>> {
        self.state.check()?;
        if self.state.session_stopped {
            return Ok(None);
        }
        let source = self
            .state
            .size
            .filter(|_| self.state.constraints_done)
            .ok_or("Wayland GPU capture constraints are incomplete")?;
        let device = self
            .state
            .gpu
            .device
            .as_ref()
            .ok_or("Wayland GPU capture is inactive")?;
        self.state.gpu.matches_device(device)?;
        let encoded = frame_dimensions(
            source.0,
            source.1,
            self.max_width,
            self.max_height,
            self.limits,
        );
        let encoded = (encoded.0 & !1, encoded.1 & !1);
        h264_padded_dimensions(encoded.0, encoded.1)?;

        // The encoder and capture thread may overlap. A new allocation for
        // each captured frame prevents writes into an outstanding import.
        // Align the linear VA-API import storage so the driver processes the
        // final partial block. Wayland and VPP keep the authorized extent.
        let storage = (source.0.next_multiple_of(16), source.1.next_multiple_of(16));
        let output = device.create_buffer_object::<()>(
            u32::from(storage.0),
            u32::from(storage.1),
            Format::Xrgb8888,
            BufferObjectFlags::LINEAR | BufferObjectFlags::RENDERING,
        )?;
        if output.modifier() != Modifier::Linear || output.plane_count() != 1 {
            return Err("GBM returned an incompatible Wayland capture layout".into());
        }
        let pixels = Pixels::from_xrgb(
            File::from(output.fd()?),
            source,
            storage,
            output.stride(),
            output.offset(0),
            0,
        )?;
        let deadline = Instant::now() + PROTOCOL_TIMEOUT;
        let buffer = self.import_gpu_buffer(&pixels, source, deadline)?;
        let geometry_epoch = self.epoch;
        self.capture_gpu_buffer(&buffer, source, deadline)?;
        self.sync_until(deadline)?;
        if self.state.session_stopped {
            return Ok(None);
        }
        if self.state.size != Some(source)
            || !self.state.constraints_done
            || geometry_epoch != self.epoch
        {
            return Err("Wayland DMA-BUF frame lost its capture geometry".into());
        }
        Ok(Some(Frame {
            pixels,
            geometry_epoch,
            source,
            encoded,
        }))
    }

    fn import_gpu_buffer(
        &mut self,
        pixels: &Pixels,
        size: (u16, u16),
        deadline: Instant,
    ) -> RemoteResult<CaptureBuffer> {
        let request = self
            .state
            .gpu
            .manager
            .as_ref()
            .ok_or("Wayland DMA-BUF global unavailable")?
            .1
            .create_params(&self.queue.handle(), ());
        let (fd, offset, pitch) = pixels.wayland_plane();
        request.add(fd, 0, offset, pitch, 0, 0);
        request.create(
            i32::from(size.0),
            i32::from(size.1),
            XRGB8888,
            params::Flags::empty(),
        );
        self.state.gpu.request = Some(request.clone());
        let wait = self.drive_until(deadline, |state| {
            state.gpu.imported.is_some() || state.session_stopped
        });
        let imported = self
            .state
            .gpu
            .imported
            .take()
            .and_then(Result::ok)
            .map(CaptureBuffer);
        self.state.gpu.request = None;
        request.destroy();
        wait?;
        imported.ok_or_else(|| "Wayland compositor rejected the linear capture buffer".into())
    }

    fn capture_gpu_buffer(
        &mut self,
        buffer: &CaptureBuffer,
        size: (u16, u16),
        deadline: Instant,
    ) -> RemoteResult<()> {
        if self.inflight.is_some() || self.retained_frame.is_some() {
            return Err("Wayland capture already owns a frame".into());
        }
        self.state.frame_serial = self
            .state
            .frame_serial
            .checked_add(1)
            .ok_or_else(|| invalid("Wayland frame sequence exhausted"))?;
        self.state.frame_failed = false;
        self.state.frame_ready = false;
        self.state.normal_transform = false;
        self.state.gpu.capturing = true;
        let frame = self
            .session
            .as_ref()
            .unwrap()
            .create_frame(&self.queue.handle(), self.state.frame_serial);
        frame.attach_buffer(&buffer.0);
        frame.damage_buffer(0, 0, i32::from(size.0), i32::from(size.1));
        frame.capture();
        self.inflight = Some(frame);
        let wait = self.drive_until(deadline, |state| {
            state.frame_ready || state.frame_failed || state.session_stopped
        });
        self.state.gpu.capturing = false;
        self.inflight.take().unwrap().destroy();
        wait?;
        if !self.state.frame_ready && !self.state.session_stopped {
            return Err("Wayland compositor failed the DMA-BUF capture".into());
        }
        Ok(())
    }
}

wayland_client::delegate_noop!(State: ignore ZwpLinuxDmabufV1);

impl Dispatch<ZwpLinuxBufferParamsV1, ()> for State {
    wayland_client::event_created_child!(State, ZwpLinuxBufferParamsV1, [0 => (wl_buffer::WlBuffer, ())]);

    fn event(
        state: &mut Self,
        request: &ZwpLinuxBufferParamsV1,
        event: params::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            params::Event::Created { buffer } => {
                if state.gpu.request.as_ref() == Some(request) && state.gpu.imported.is_none() {
                    state.gpu.imported = Some(Ok(buffer));
                } else {
                    buffer.destroy();
                }
            }
            params::Event::Failed if state.gpu.request.as_ref() == Some(request) => {
                state.gpu.imported = Some(Err(()));
            }
            _ => {}
        }
    }
}
