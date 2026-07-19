use smithay::backend::renderer::gles::ffi;
use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub(crate) struct RecordingState {
    active: bool,
    child: Option<Child>,
    pbo: [u32; 2],
    current_pbo: usize,
    width: u32,
    height: u32,
    source_width: u32,
    source_height: u32,
    region: (i32, i32, u32, u32),
    capture_fbo: u32,
    capture_texture: u32,
    frame_regions: [(i32, i32, u32, u32); 2],
    pointer_positions: [(f32, f32); 2],
    frame_count: u64,
    start_time: Option<Instant>,
    fps: u32,
    last_capture: Instant,
}

impl RecordingState {
    pub(crate) fn new() -> Self {
        Self {
            active: false,
            child: None,
            pbo: [0; 2],
            current_pbo: 0,
            width: 0,
            height: 0,
            source_width: 0,
            source_height: 0,
            region: (0, 0, 0, 0),
            capture_fbo: 0,
            capture_texture: 0,
            frame_regions: [(0, 0, 0, 0); 2],
            pointer_positions: [(0.0, 0.0); 2],
            frame_count: 0,
            start_time: None,
            fps: 30,
            last_capture: Instant::now(),
        }
    }

    pub(crate) unsafe fn start(
        &mut self,
        gl: &ffi::Gles2,
        width: u32,
        height: u32,
        output_path: &str,
        fps: u32,
        bitrate: &str,
        quality: u32,
        configured_encoder: &str,
        region: (i32, i32, u32, u32),
    ) -> Result<(), String> {
        if self.active {
            return Err("Recording already active".to_string());
        }

        self.source_width = width;
        self.source_height = height;
        self.region = region;
        self.width = region.2;
        self.height = region.3;
        self.fps = fps;
        self.frame_regions = [region; 2];

        unsafe {
            const GL_RGBA8: u32 = 0x8058;
            let (capture_fbo, capture_texture) =
                super::create_fbo_texture_fmt(gl, self.width, self.height, GL_RGBA8).map_err(
                    |status| {
                        format!(
                            "failed to create recording framebuffer ({}x{}, status=0x{status:x})",
                            self.width, self.height
                        )
                    },
                )?;
            self.capture_fbo = capture_fbo;
            self.capture_texture = capture_texture;
            gl.GenBuffers(2, self.pbo.as_mut_ptr());

            let buffer_size = (self.width * self.height * 4) as isize;
            for i in 0..2 {
                gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, self.pbo[i]);
                gl.BufferData(
                    ffi::PIXEL_PACK_BUFFER,
                    buffer_size,
                    std::ptr::null(),
                    ffi::STREAM_READ,
                );
            }
            gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, 0);
            gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
        }

        // Keep ffmpeg diagnostics available: discarding stderr makes a failed
        // encoder look like a successfully-created but unplayable MP4.
        let stderr = std::fs::File::create("/tmp/jwm-wayland-recording-ffmpeg.log")
            .map_err(|e| format!("create ffmpeg log: {e}"))?;
        let size = format!("{}x{}", self.width, self.height);
        let fps = fps.to_string();
        use crate::backend::compositor_common::media::{
            RecordingEncoder, append_recording_audio_input, append_recording_audio_output,
            recording_audio_available, select_recording_encoder,
        };
        let encoder = select_recording_encoder(configured_encoder);
        let (audio_enabled, audio_device, audio_bitrate) = {
            let cfg = crate::config::CONFIG.load();
            let behavior = cfg.behavior();
            (
                behavior.recording_audio_enabled,
                behavior.recording_audio_device.clone(),
                behavior.recording_audio_bitrate.clone(),
            )
        };
        let with_audio = audio_enabled && recording_audio_available(&audio_device);
        if audio_enabled && !with_audio {
            log::warn!(
                "[recording] microphone '{}' unavailable; continuing video-only",
                audio_device
            );
        }

        let quality = quality.to_string();
        let mut args: Vec<String> = Vec::new();
        if matches!(encoder, RecordingEncoder::Vaapi) {
            args.extend(["-vaapi_device", "/dev/dri/renderD128"].map(str::to_string));
        }
        args.extend(
            [
                "-y",
                "-use_wallclock_as_timestamps",
                "1",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgba",
                "-s",
                size.as_str(),
                "-i",
                "pipe:0",
            ]
            .map(str::to_string),
        );
        if with_audio {
            append_recording_audio_input(&mut args, &audio_device);
        }
        // OpenGL's origin is bottom-left, unlike normal video.
        args.push("-vf".into());
        match encoder {
            RecordingEncoder::Nvenc => {
                args.extend(["vflip", "-c:v", "h264_nvenc", "-b:v", bitrate].map(str::to_string))
            }
            RecordingEncoder::Vaapi => args.extend(
                [
                    "vflip,format=nv12,hwupload",
                    "-c:v",
                    "h264_vaapi",
                    "-rc_mode",
                    "CQP",
                    "-qp",
                    quality.as_str(),
                ]
                .map(str::to_string),
            ),
            RecordingEncoder::Software => args.extend(
                [
                    "vflip", "-c:v", "libx264", "-preset", "fast", "-crf", "23", "-b:v", bitrate,
                ]
                .map(str::to_string),
            ),
        }
        if with_audio {
            append_recording_audio_output(&mut args, &audio_bitrate);
        }
        args.extend(
            [
                "-r",
                fps.as_str(),
                "-pix_fmt",
                "yuv420p",
                "-movflags",
                "+faststart",
                output_path,
            ]
            .map(str::to_string),
        );
        let codec_name = encoder.codec_name("libx264");
        log::info!(
            "[recording] Wayland encoder={codec_name} size={size} fps={fps} microphone={}",
            if with_audio {
                audio_device.as_str()
            } else {
                "off"
            }
        );
        let child = match Command::new("ffmpeg")
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                unsafe {
                    gl.DeleteBuffers(2, self.pbo.as_ptr());
                    gl.DeleteFramebuffers(1, &self.capture_fbo);
                    gl.DeleteTextures(1, &self.capture_texture);
                }
                self.pbo = [0; 2];
                self.capture_fbo = 0;
                self.capture_texture = 0;
                return Err(format!("Failed to spawn ffmpeg: {error}"));
            }
        };

        self.child = Some(child);
        self.active = true;
        self.frame_count = 0;
        self.current_pbo = 0;
        self.start_time = Some(Instant::now());
        self.last_capture = Instant::now();

        Ok(())
    }

    pub(crate) unsafe fn capture_frame(
        &mut self,
        gl: &ffi::Gles2,
        source_fbo: u32,
        pointer_position: (f32, f32),
    ) {
        if !self.active {
            return;
        }

        let frame_duration = Duration::from_secs_f64(1.0 / self.fps as f64);
        if self.last_capture.elapsed() < frame_duration {
            return;
        }
        self.last_capture = Instant::now();

        unsafe {
            gl.BindFramebuffer(ffi::READ_FRAMEBUFFER, source_fbo);
            gl.BindFramebuffer(ffi::DRAW_FRAMEBUFFER, self.capture_fbo);
            let (x, y, region_width, region_height) = self.region;
            let source_bottom = self.source_height as i32 - (y + region_height as i32);
            gl.BlitFramebuffer(
                x,
                source_bottom,
                x + region_width as i32,
                source_bottom + region_height as i32,
                0,
                0,
                self.width as i32,
                self.height as i32,
                ffi::COLOR_BUFFER_BIT,
                ffi::LINEAR,
            );
            gl.BindFramebuffer(ffi::FRAMEBUFFER, self.capture_fbo);

            let written_pbo = self.current_pbo;
            gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, self.pbo[written_pbo]);
            gl.ReadPixels(
                0,
                0,
                self.width as i32,
                self.height as i32,
                ffi::RGBA,
                ffi::UNSIGNED_BYTE,
                std::ptr::null_mut(),
            );
            self.frame_regions[written_pbo] = self.region;
            self.pointer_positions[written_pbo] = pointer_position;

            self.current_pbo ^= 1;

            if self.frame_count > 0 {
                // `written_pbo` is being filled by this ReadPixels; map the
                // other PBO, which was filled by the preceding capture.
                let other_pbo = written_pbo ^ 1;
                gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, self.pbo[other_pbo]);

                let buffer_size = (self.width * self.height * 4) as isize;
                let ptr =
                    gl.MapBufferRange(ffi::PIXEL_PACK_BUFFER, 0, buffer_size, ffi::MAP_READ_BIT);

                if !ptr.is_null() {
                    let data = std::slice::from_raw_parts_mut(ptr as *mut u8, buffer_size as usize);
                    composite_software_cursor(
                        data,
                        self.width,
                        self.height,
                        self.frame_regions[other_pbo],
                        self.pointer_positions[other_pbo],
                    );

                    if let Some(ref mut child) = self.child {
                        if let Some(ref mut stdin) = child.stdin {
                            if let Err(e) = stdin.write_all(data) {
                                log::warn!("[recording] ffmpeg input write failed: {e}");
                            }
                        }
                    }

                    gl.UnmapBuffer(ffi::PIXEL_PACK_BUFFER);
                }
            }

            gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, 0);
            gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
        }
        self.frame_count += 1;
    }

    pub(crate) unsafe fn stop(&mut self, gl: &ffi::Gles2) {
        if !self.active {
            return;
        }

        // The most recent ReadPixels has no subsequent capture to trigger its
        // readback. Drain it before closing stdin so the file is complete.
        if self.frame_count > 0 {
            let last_pbo = self.current_pbo ^ 1;
            unsafe {
                gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, self.pbo[last_pbo]);
                let buffer_size = (self.width * self.height * 4) as isize;
                let ptr =
                    gl.MapBufferRange(ffi::PIXEL_PACK_BUFFER, 0, buffer_size, ffi::MAP_READ_BIT);
                if !ptr.is_null() {
                    let data = std::slice::from_raw_parts_mut(ptr as *mut u8, buffer_size as usize);
                    composite_software_cursor(
                        data,
                        self.width,
                        self.height,
                        self.frame_regions[last_pbo],
                        self.pointer_positions[last_pbo],
                    );
                    if let Some(child) = self.child.as_mut() {
                        if let Some(stdin) = child.stdin.as_mut() {
                            if let Err(e) = stdin.write_all(data) {
                                log::warn!("[recording] final ffmpeg input write failed: {e}");
                            }
                        }
                    }
                    gl.UnmapBuffer(ffi::PIXEL_PACK_BUFFER);
                }
                gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, 0);
            }
        }

        if let Some(mut child) = self.child.take() {
            drop(child.stdin.take());
            match child.wait() {
                Ok(status) if !status.success() => log::warn!(
                    "[recording] ffmpeg exited with {status}; see /tmp/jwm-wayland-recording-ffmpeg.log"
                ),
                Err(e) => log::warn!("[recording] failed waiting for ffmpeg: {e}"),
                Ok(_) => {}
            }
        }

        unsafe {
            gl.DeleteBuffers(2, self.pbo.as_ptr());
            if self.capture_fbo != 0 {
                gl.DeleteFramebuffers(1, &self.capture_fbo);
            }
            if self.capture_texture != 0 {
                gl.DeleteTextures(1, &self.capture_texture);
            }
        }
        self.pbo = [0; 2];
        self.capture_fbo = 0;
        self.capture_texture = 0;

        self.active = false;
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active
    }

    pub(crate) fn set_region(&mut self, region: (i32, i32, u32, u32)) {
        self.region = region;
    }

    pub(crate) fn frame_count(&self) -> u64 {
        self.frame_count
    }

    pub(crate) fn elapsed(&self) -> Option<Duration> {
        self.start_time.map(|t| t.elapsed())
    }
}

// Coordinates are relative to the pointer hotspot at (0, 0).
const SOFTWARE_CURSOR_RECTS: &[(i32, i32, i32, i32)] = &[
    (0, 0, 1, 1),
    (0, 1, 2, 1),
    (0, 2, 3, 1),
    (0, 3, 4, 1),
    (0, 4, 5, 1),
    (0, 5, 6, 1),
    (0, 6, 7, 1),
    (0, 7, 8, 1),
    (0, 8, 9, 1),
    (0, 9, 10, 1),
    (0, 10, 11, 1),
    (3, 11, 3, 7),
    (2, 18, 5, 2),
];

fn composite_software_cursor(
    frame: &mut [u8],
    frame_width: u32,
    frame_height: u32,
    source_region: (i32, i32, u32, u32),
    pointer_position: (f32, f32),
) {
    let (_, _, region_width, region_height) = source_region;
    if region_width == 0 || region_height == 0 {
        return;
    }
    let scale_x = frame_width as f64 / region_width as f64;
    let scale_y = frame_height as f64 / region_height as f64;
    let pointer_x = pointer_position.0.round() as i32;
    let pointer_y = pointer_position.1.round() as i32;

    for &(offset_x, offset_y, width, height) in SOFTWARE_CURSOR_RECTS {
        draw_scaled_cursor_rect(
            frame,
            frame_width,
            frame_height,
            source_region,
            (
                pointer_x + offset_x + 1,
                pointer_y + offset_y + 1,
                width,
                height,
            ),
            scale_x,
            scale_y,
            [0, 0, 0, 140],
        );
    }
    for &(offset_x, offset_y, width, height) in SOFTWARE_CURSOR_RECTS {
        draw_scaled_cursor_rect(
            frame,
            frame_width,
            frame_height,
            source_region,
            (pointer_x + offset_x, pointer_y + offset_y, width, height),
            scale_x,
            scale_y,
            [250, 250, 250, 255],
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_scaled_cursor_rect(
    frame: &mut [u8],
    frame_width: u32,
    frame_height: u32,
    source_region: (i32, i32, u32, u32),
    rect: (i32, i32, i32, i32),
    scale_x: f64,
    scale_y: f64,
    color: [u8; 4],
) {
    let (region_x, region_y, _, _) = source_region;
    let (x, y, width, height) = rect;
    let left = ((x - region_x) as f64 * scale_x).floor() as i32;
    let right = ((x + width - region_x) as f64 * scale_x).ceil() as i32;
    let top = ((y - region_y) as f64 * scale_y).floor() as i32;
    let bottom = ((y + height - region_y) as f64 * scale_y).ceil() as i32;
    let alpha = u32::from(color[3]);
    let inverse_alpha = 255 - alpha;

    for output_y in top.max(0)..bottom.min(frame_height as i32) {
        let frame_y = frame_height - 1 - output_y as u32;
        for output_x in left.max(0)..right.min(frame_width as i32) {
            let index = (frame_y as usize * frame_width as usize + output_x as usize) * 4;
            if index + 3 >= frame.len() {
                continue;
            }
            for channel in 0..3 {
                frame[index + channel] = ((u32::from(color[channel]) * alpha
                    + u32::from(frame[index + channel]) * inverse_alpha
                    + 127)
                    / 255) as u8;
            }
            frame[index + 3] = 255;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::composite_software_cursor;

    #[test]
    fn software_cursor_is_offset_and_clipped_to_recording_region() {
        let mut frame = vec![0_u8; 100 * 50 * 4];
        composite_software_cursor(&mut frame, 100, 50, (200, 100, 400, 200), (400.0, 200.0));
        assert!(frame.iter().any(|&channel| channel == 250));

        let mut outside = vec![0_u8; 100 * 50 * 4];
        composite_software_cursor(&mut outside, 100, 50, (200, 100, 400, 200), (50.0, 50.0));
        assert!(outside.iter().all(|&channel| channel == 0));
    }
}
