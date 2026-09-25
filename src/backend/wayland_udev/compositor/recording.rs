use crate::backend::compositor_common::recording_nv12::{
    NV12_PACK_FRAGMENT_BODY, nv12_frame_bytes, nv12_packed_target_size, nv12_target_fits,
    recording_canvas_rect, recording_output_size,
};
use crate::backend::compositor_common::recording_sink::RecordingSink;
use smithay::backend::renderer::gles::ffi;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Fullscreen quad shared by both recording passes.
///
/// Attribute-free so it needs no vertex buffer of its own; the recorder draws
/// it as a four-vertex triangle strip.
pub(super) const RECORDING_QUAD_VERTEX: &str = r#"#version 300 es
precision highp float;
void main() {
    vec2 corner = vec2(float(gl_VertexID & 1), float(gl_VertexID >> 1));
    gl_Position = vec4(corner * 2.0 - 1.0, 0.0, 1.0);
}
"#;

/// Draws the pointer into the capture target.
///
/// When KMS assembles the real cursor outside the compositor texture (its own
/// plane, or an exact-sRGB fallback frame), the capture source has no cursor
/// image to sample, so the recorder synthesises the same arrow it always has —
/// the shape below is `SOFTWARE_CURSOR_RECTS` expressed for the GPU, drawn as
/// a black shadow offset by one cursor unit and then an opaque white fill,
/// exactly as the CPU compositing pass did it. Frames where the cursor class
/// was internalized into the common-linear target already carry the themed
/// cursor in the capture view and skip this draw (`cursor_already_present`).
///
/// `u_origin` is where the pointer sits in the recorded frame and `u_scale` how
/// many output pixels one cursor unit spans, so the arrow follows a scaled
/// region the same way the scene does.
pub(super) const RECORDING_CURSOR_FRAGMENT: &str = r#"#version 300 es
precision highp float;

uniform vec2 u_origin;
uniform vec2 u_scale;
uniform vec2 u_target_size;
out vec4 frag_color;

const int RECT_COUNT = 13;
const vec4 RECTS[13] = vec4[13](
    vec4(0.0,  0.0,  1.0, 1.0),
    vec4(0.0,  1.0,  2.0, 1.0),
    vec4(0.0,  2.0,  3.0, 1.0),
    vec4(0.0,  3.0,  4.0, 1.0),
    vec4(0.0,  4.0,  5.0, 1.0),
    vec4(0.0,  5.0,  6.0, 1.0),
    vec4(0.0,  6.0,  7.0, 1.0),
    vec4(0.0,  7.0,  8.0, 1.0),
    vec4(0.0,  8.0,  9.0, 1.0),
    vec4(0.0,  9.0, 10.0, 1.0),
    vec4(0.0, 10.0, 11.0, 1.0),
    vec4(3.0, 11.0,  3.0, 7.0),
    vec4(2.0, 18.0,  5.0, 2.0)
);

bool covered(vec2 local) {
    for (int i = 0; i < RECT_COUNT; ++i) {
        vec4 r = RECTS[i];
        if (local.x >= r.x && local.x < r.x + r.z
         && local.y >= r.y && local.y < r.y + r.w) {
            return true;
        }
    }
    return false;
}

void main() {
    // The capture target is bottom-up; the pointer is reported top-down.
    vec2 pixel = vec2(gl_FragCoord.x, u_target_size.y - gl_FragCoord.y);
    vec2 local = (pixel - u_origin) / u_scale;
    if (covered(local)) {
        frag_color = vec4(250.0 / 255.0, 250.0 / 255.0, 250.0 / 255.0, 1.0);
    } else if (covered(local - vec2(1.0))) {
        frag_color = vec4(0.0, 0.0, 0.0, 140.0 / 255.0);
    } else {
        discard;
    }
}
"#;

/// Set a uniform by name. `name` must be NUL-terminated.
///
/// # Safety
/// Requires `program` in use and a current GL context.
unsafe fn uniform_1i(gl: &ffi::Gles2, program: u32, name: &[u8], value: i32) {
    unsafe {
        let location = gl.GetUniformLocation(program, name.as_ptr() as *const _);
        if location >= 0 {
            gl.Uniform1i(location, value);
        }
    }
}

/// # Safety
/// Requires `program` in use and a current GL context.
unsafe fn uniform_1f(gl: &ffi::Gles2, program: u32, name: &[u8], value: f32) {
    unsafe {
        let location = gl.GetUniformLocation(program, name.as_ptr() as *const _);
        if location >= 0 {
            gl.Uniform1f(location, value);
        }
    }
}

/// # Safety
/// Requires `program` in use and a current GL context.
unsafe fn uniform_2f(gl: &ffi::Gles2, program: u32, name: &[u8], x: f32, y: f32) {
    unsafe {
        let location = gl.GetUniformLocation(program, name.as_ptr() as *const _);
        if location >= 0 {
            gl.Uniform2f(location, x, y);
        }
    }
}

/// Name of the recorder's ffmpeg diagnostics log inside the per-user runtime
/// directory.
const RECORDING_FFMPEG_LOG_NAME: &str = "jwm-wayland-recording-ffmpeg.log";

/// Where the recorder keeps ffmpeg's diagnostics.
///
/// ffmpeg runs at `-loglevel warning`, so this log is the only place a missing
/// encoder or a VAAPI failure is explained. `$XDG_RUNTIME_DIR` is private to
/// the user, so the stable name there cannot collide with anyone else's.
/// Without one (or with a relative value, which would follow the compositor's
/// working directory) the log falls back to the shared temporary directory
/// under a per-user name: the single fixed name it used to have there belonged
/// to whichever user recorded first, and every other user's recording then
/// failed to start.
fn recording_ffmpeg_log_path(
    runtime_dir: Option<&std::ffi::OsStr>,
    temp_dir: &std::path::Path,
    euid: u32,
) -> std::path::PathBuf {
    match runtime_dir.map(std::path::Path::new) {
        Some(dir) if dir.is_absolute() => dir.join(RECORDING_FFMPEG_LOG_NAME),
        _ => temp_dir.join(format!("jwm-wayland-recording-ffmpeg-{euid}.log")),
    }
}

/// Open the recorder's ffmpeg log, emptied for this recording.
///
/// The fallback directory is world-writable, so the open never follows a
/// symlink and never blocks on a planted FIFO, and the file is accepted only as
/// a regular file owned by this user. It is truncated and made private only
/// after that check, so a file belonging to someone else is never clobbered.
fn open_recording_ffmpeg_log(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "not a regular file owned by this user",
        ));
    }
    file.set_len(0)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

pub(crate) struct RecordingState {
    active: bool,
    sink: Option<RecordingSink>,
    pbo: [u32; 2],
    current_pbo: usize,
    width: u32,
    height: u32,
    source_width: u32,
    source_height: u32,
    region: (i32, i32, u32, u32),
    /// Size of the region the recording started with, which the encode size
    /// was derived from. Later regions are fitted to the canvas relative to
    /// it; see [`recording_canvas_rect`].
    start_region_size: (u32, u32),
    capture_fbo: u32,
    capture_texture: u32,
    frame_count: u64,
    start_time: Option<Instant>,
    fps: u32,
    last_capture: Instant,
    /// Captures retired without reading pixels since the last real one. Only
    /// the first of a run is reported, so a capture source that stays
    /// unavailable does not write one log line per capture interval.
    skipped_since_capture: u64,
    /// Packed NV12 target and the passes that fill it. Zero when this recording
    /// fell back to a plain RGBA readback.
    packed_fbo: u32,
    packed_texture: u32,
    pack_program: u32,
    cursor_program: u32,
    nv12: bool,
}

impl RecordingState {
    pub(crate) fn new() -> Self {
        Self {
            active: false,
            sink: None,
            pbo: [0; 2],
            current_pbo: 0,
            width: 0,
            height: 0,
            source_width: 0,
            source_height: 0,
            region: (0, 0, 0, 0),
            start_region_size: (0, 0),
            capture_fbo: 0,
            capture_texture: 0,
            frame_count: 0,
            start_time: None,
            fps: 30,
            last_capture: Instant::now(),
            skipped_since_capture: 0,
            packed_fbo: 0,
            packed_texture: 0,
            pack_program: 0,
            cursor_program: 0,
            nv12: false,
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

        // Open the diagnostic sink before allocating any GL objects, so no
        // filesystem trouble can interleave with them. A log that cannot be
        // opened safely costs the encoder's diagnostics, never the recording:
        // the log's directory may be shared, and another user's file at its
        // path must not be able to block recording.
        let stderr_log_path = recording_ffmpeg_log_path(
            std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
            &std::env::temp_dir(),
            unsafe { libc::geteuid() },
        );
        let stderr = match open_recording_ffmpeg_log(&stderr_log_path) {
            Ok(file) => {
                log::info!(
                    "wayland recording: encoder diagnostics go to {}",
                    stderr_log_path.display()
                );
                Stdio::from(file)
            }
            Err(error) => {
                log::warn!(
                    "wayland recording: cannot open ffmpeg log {}: {error}; encoder diagnostics will be discarded",
                    stderr_log_path.display()
                );
                Stdio::null()
            }
        };

        self.source_width = width;
        self.source_height = height;
        self.region = region;
        self.start_region_size = (region.2, region.3);
        let max_height = crate::config::CONFIG.load().behavior().recording_max_height;
        let (encoded_w, encoded_h) = recording_output_size(region.2, region.3, max_height);
        if encoded_w == 0 || encoded_h == 0 {
            return Err(format!(
                "recording region {}x{} is too small to encode",
                region.2, region.3
            ));
        }
        self.width = encoded_w;
        self.height = encoded_h;
        self.fps = fps;

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

            // Convert on the GPU when the driver can hold the packed target,
            // which every real desktop driver can. The plain RGBA readback
            // stays as the fallback for one at the ES 3.0 minimum size.
            let mut max_texture_size: ffi::types::GLint = 0;
            gl.GetIntegerv(ffi::MAX_TEXTURE_SIZE, &mut max_texture_size);
            let (packed_w, packed_h) = nv12_packed_target_size(self.width, self.height);
            self.nv12 = nv12_target_fits(self.width, self.height, max_texture_size.max(0) as u32);
            if self.nv12 {
                match super::create_fbo_texture_fmt(gl, packed_w, packed_h, GL_RGBA8) {
                    Ok((fbo, texture)) => {
                        self.packed_fbo = fbo;
                        self.packed_texture = texture;
                    }
                    Err(status) => {
                        log::warn!(
                            "[recording] NV12 packing target {packed_w}x{packed_h} unavailable \
                             (status=0x{status:x}); falling back to RGBA capture"
                        );
                        self.nv12 = false;
                    }
                }
            } else {
                log::warn!(
                    "[recording] {}x{} needs a {packed_w}x{packed_h} packing target but the \
                     driver caps textures at {max_texture_size}; falling back to RGBA capture",
                    self.width,
                    self.height
                );
            }

            // Compile before the buffers are sized: a shader that fails to
            // build falls back to the RGBA readback, which needs a larger
            // buffer than NV12, and a PBO sized for the wrong one would be
            // overrun by the very next glReadPixels.
            if self.nv12 {
                let pack_source =
                    format!("#version 300 es\nprecision highp float;\n{NV12_PACK_FRAGMENT_BODY}");
                let programs = {
                    super::shader_cache::ShaderCache::compile_program(
                        gl,
                        RECORDING_QUAD_VERTEX,
                        &pack_source,
                    )
                    .and_then(|pack| {
                        super::shader_cache::ShaderCache::compile_program(
                            gl,
                            RECORDING_QUAD_VERTEX,
                            RECORDING_CURSOR_FRAGMENT,
                        )
                        .map(|cursor| (pack, cursor))
                    })
                };
                match programs {
                    Ok((pack, cursor)) => {
                        self.pack_program = pack;
                        self.cursor_program = cursor;
                    }
                    Err(error) => {
                        log::warn!(
                            "[recording] recording shaders failed to compile ({error}); \
                             falling back to RGBA capture"
                        );
                        self.nv12 = false;
                        self.release_packed_target(gl);
                    }
                }
            }

            let buffer_size = self.frame_bytes() as isize;
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
        let size = format!("{}x{}", self.width, self.height);
        let fps = fps.to_string();
        use crate::backend::compositor_common::media::VAAPI_DEVICE;
        use crate::backend::compositor_common::media::{
            RecordingEncoder, append_recording_audio_input, append_recording_audio_output,
            append_recording_log_args, append_software_encoder_pacing, deprioritize_encoder,
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
        append_recording_log_args(&mut args);
        if matches!(encoder, RecordingEncoder::Vaapi) {
            args.extend(["-vaapi_device", VAAPI_DEVICE].map(str::to_string));
        }
        args.extend(
            [
                "-y",
                "-use_wallclock_as_timestamps",
                "1",
                "-f",
                "rawvideo",
                "-pix_fmt",
                if self.nv12 { "nv12" } else { "rgba" },
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
        // The packing shader samples the capture target upside down, so on that
        // path the bytes already arrive the right way up. The RGBA fallback
        // reads a bottom-up framebuffer and still needs the flip. VAAPI uploads
        // either way, and nv12 needs no conversion with it.
        let mut filters: Vec<&str> = Vec::new();
        if !self.nv12 {
            filters.push("vflip");
        }
        if matches!(encoder, RecordingEncoder::Vaapi) {
            if !self.nv12 {
                filters.push("format=nv12");
            }
            filters.push("hwupload");
        }
        if !filters.is_empty() {
            args.extend(["-vf".to_string(), filters.join(",")]);
        }
        match encoder {
            RecordingEncoder::Nvenc => {
                args.extend(["-c:v", "h264_nvenc", "-b:v", bitrate].map(str::to_string))
            }
            RecordingEncoder::Vaapi => args.extend(
                [
                    "-c:v",
                    "h264_vaapi",
                    "-rc_mode",
                    "CQP",
                    "-qp",
                    quality.as_str(),
                ]
                .map(str::to_string),
            ),
            RecordingEncoder::Software => {
                args.extend(["-c:v", "libx264", "-crf", "23", "-b:v", bitrate].map(str::to_string));
                append_software_encoder_pacing(&mut args);
            }
        }
        if self.nv12 {
            // Must travel with the shader's BT.709 matrix. Tagging without
            // converting, or converting without tagging, both shift colour.
            args.extend(
                [
                    "-colorspace",
                    "bt709",
                    "-color_primaries",
                    "bt709",
                    "-color_trc",
                    "bt709",
                    "-color_range",
                    "tv",
                ]
                .map(str::to_string),
            );
        }
        if with_audio {
            append_recording_audio_output(&mut args, &audio_bitrate);
        }
        // Software only: libx264 would otherwise negotiate yuv444p from RGBA
        // input, at roughly twice the encoding cost. The hardware encoders
        // convert from RGB on the GPU, so naming a format here just forces a
        // CPU swscale pass in front of them.
        if matches!(encoder, RecordingEncoder::Software) {
            args.extend(["-pix_fmt", "yuv420p"].map(str::to_string));
        }
        args.extend(
            ["-r", fps.as_str(), "-movflags", "+faststart", output_path].map(str::to_string),
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
        let mut command = Command::new("ffmpeg");
        command
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(stderr);
        deprioritize_encoder(&mut command);
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                unsafe { self.release_gpu_resources(gl) };
                return Err(format!("Failed to spawn ffmpeg: {error}"));
            }
        };

        // Frames reach ffmpeg through a writer thread that drops them when the
        // encoder is behind. Writing an 8 MB frame into a 64 KiB pipe from the
        // render loop stalled the whole session whenever the encoder lagged.
        self.sink = Some(RecordingSink::spawn(
            child,
            self.frame_bytes(),
            "[recording]",
        ));
        self.active = true;
        self.frame_count = 0;
        self.current_pbo = 0;
        self.start_time = Some(Instant::now());
        self.last_capture = Instant::now();
        self.skipped_since_capture = 0;

        Ok(())
    }

    pub(crate) unsafe fn capture_frame(
        &mut self,
        gl: &ffi::Gles2,
        source_fbo: u32,
        pointer_position: (f32, f32),
        cursor_already_present: bool,
    ) {
        if !self.active {
            return;
        }
        if !self.frame_due() {
            return;
        }
        self.last_capture = self.next_capture_anchor();
        self.skipped_since_capture = 0;

        unsafe {
            gl.BindFramebuffer(ffi::READ_FRAMEBUFFER, source_fbo);
            gl.BindFramebuffer(ffi::DRAW_FRAMEBUFFER, self.capture_fbo);
            let (x, y, region_width, region_height) = self.region;
            let source_bottom = self.source_height as i32 - (y + region_height as i32);
            let (dest_x, dest_y, dest_width, dest_height) = self.canvas_rect();
            if (dest_width, dest_height) != (self.width, self.height) {
                // The blit leaves the bars untouched, and they would otherwise
                // keep whatever an earlier region of another shape put there.
                // The clear honours the scissor box, so that goes first.
                gl.Disable(ffi::SCISSOR_TEST);
                gl.ClearColor(0.0, 0.0, 0.0, 1.0);
                gl.Clear(ffi::COLOR_BUFFER_BIT);
            }
            // The canvas rect is top-down; the framebuffer counts rows from the
            // bottom.
            let dest_bottom = self.height.saturating_sub(dest_y + dest_height) as i32;
            gl.BlitFramebuffer(
                x,
                source_bottom,
                x + region_width as i32,
                source_bottom + region_height as i32,
                dest_x as i32,
                dest_bottom,
                (dest_x + dest_width) as i32,
                dest_bottom + dest_height as i32,
                ffi::COLOR_BUFFER_BIT,
                ffi::LINEAR,
            );
            gl.BindFramebuffer(ffi::FRAMEBUFFER, self.capture_fbo);
            gl.Viewport(0, 0, self.width as i32, self.height as i32);

            // The pointer is drawn into the capture target before packing. The
            // CPU pass this replaces wrote it into the readback buffer
            // afterwards, which a subsampled pixel format cannot accommodate.
            // Skipped when the source already carries the real cursor: on
            // frames where the cursor class was internalized into the
            // compositor's common-linear target, the capture view contains the
            // themed cursor and the synthesised arrow would double it.
            if !cursor_already_present {
                self.draw_cursor(gl, pointer_position);
            }

            // Convert to NV12 on the GPU so the readback, the copy out of
            // mapped memory, the pipe and ffmpeg's read all carry 1.5 bytes per
            // pixel instead of 4, and the encoder needs no conversion pass.
            if self.nv12 {
                self.pack_nv12(gl);
            }

            let (read_w, read_h) = self.readback_extent();
            let written_pbo = self.current_pbo;
            gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, self.pbo[written_pbo]);
            gl.ReadPixels(
                0,
                0,
                read_w as i32,
                read_h as i32,
                ffi::RGBA,
                ffi::UNSIGNED_BYTE,
                std::ptr::null_mut(),
            );

            self.current_pbo ^= 1;

            if self.frame_count > 0 {
                // `written_pbo` is being filled by this ReadPixels; map the
                // other PBO, which was filled by the preceding capture.
                let other_pbo = written_pbo ^ 1;
                gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, self.pbo[other_pbo]);
                self.drain_pbo(gl);
            }

            gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, 0);
            gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
            // Restore what the rest of the frame is entitled to assume. The
            // capture borrows the viewport and the blend enable for its own
            // passes, and whatever draws next would otherwise inherit them.
            gl.Viewport(0, 0, self.source_width as i32, self.source_height as i32);
            gl.Disable(ffi::BLEND);
            gl.UseProgram(0);
        }
        self.frame_count += 1;
    }

    /// Copy the already-bound pixel buffer into a pooled heap buffer and hand
    /// it to the writer thread.
    ///
    /// The copy is deliberately one bulk `copy_nonoverlapping`: mapped pixel
    /// buffers commonly live in uncached or write-combined memory, where reads
    /// cost an order of magnitude more than against the heap. Unmapping
    /// immediately also releases the buffer back to the driver a frame earlier.
    ///
    /// # Safety
    /// The caller must have the recording PBO bound to `PIXEL_PACK_BUFFER` and
    /// a current GL context.
    unsafe fn drain_pbo(&mut self, gl: &ffi::Gles2) {
        let Some(mut sink) = self.sink.take() else {
            return;
        };
        let buffer_size = self.frame_bytes();
        let mut frame = sink.take_buffer();
        let mut filled = false;
        unsafe {
            let ptr = gl.MapBufferRange(
                ffi::PIXEL_PACK_BUFFER,
                0,
                buffer_size as isize,
                ffi::MAP_READ_BIT,
            );
            if ptr.is_null() {
                log::warn!("[recording] pixel buffer map returned null");
            } else {
                // The sink was sized from the same width/height that sized the
                // PBO, so these always agree; clamp anyway rather than let a
                // future divergence turn into an out-of-bounds copy.
                debug_assert_eq!(frame.len(), buffer_size);
                let copied = buffer_size.min(frame.len());
                std::ptr::copy_nonoverlapping(ptr as *const u8, frame.as_mut_ptr(), copied);
                gl.UnmapBuffer(ffi::PIXEL_PACK_BUFFER);
                filled = copied == buffer_size;
            }
        }

        if filled {
            // Already a finished frame: the cursor was drawn on the GPU before
            // packing.
            sink.submit(frame);
        } else {
            sink.return_buffer(frame);
        }
        if sink.is_broken() {
            log::warn!("[recording] ffmpeg input closed; stopping capture");
            self.active = false;
        }
        self.sink = Some(sink);
    }

    pub(crate) unsafe fn stop(&mut self, gl: &ffi::Gles2) {
        // The most recent ReadPixels has no subsequent capture to trigger its
        // readback. Drain it before closing stdin so the file is complete.
        // Skipped once the encoder pipe has already failed — there is nothing
        // left to write to.
        if self.active && self.frame_count > 0 {
            let last_pbo = self.current_pbo ^ 1;
            unsafe {
                gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, self.pbo[last_pbo]);
                self.drain_pbo(gl);
                gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, 0);
            }
        }

        // Unconditional, not gated on `active`: a broken encoder pipe clears
        // that flag on its own, and skipping teardown there would strand the
        // writer thread holding ffmpeg's stdin open — the encoder would wait
        // forever for input and never finalize its file.
        //
        // Do not wait for ffmpeg either. Its exit path flushes the encoder and,
        // with `+faststart`, rewrites the whole MP4 to move the moov atom to the
        // front; on a long recording that is seconds during which this thread
        // would render nothing and answer no input. The writer thread reaps it.
        if let Some(sink) = self.sink.take() {
            log::info!("[recording] stopped ({})", sink.finish());
        }

        // Also heals a partially-initialized state from a start that failed
        // after raw allocation.
        unsafe { self.release_gpu_resources(gl) };

        self.active = false;
    }

    /// Release raw recording objects regardless of the business-level
    /// `active` flag. Every handle is cleared after deletion, making this safe
    /// to call from both error rollback and compositor teardown.
    /// Bytes in one captured frame, which the PBOs and the encoder sink size to.
    fn frame_bytes(&self) -> usize {
        if self.nv12 {
            nv12_frame_bytes(self.width, self.height)
        } else {
            (self.width as usize) * (self.height as usize) * 4
        }
    }

    /// Extent `glReadPixels` covers: the packed target, or the frame itself.
    fn readback_extent(&self) -> (u32, u32) {
        if self.nv12 {
            nv12_packed_target_size(self.width, self.height)
        } else {
            (self.width, self.height)
        }
    }

    unsafe fn release_packed_target(&mut self, gl: &ffi::Gles2) {
        unsafe {
            if self.packed_fbo != 0 {
                gl.DeleteFramebuffers(1, &self.packed_fbo);
            }
            if self.packed_texture != 0 {
                gl.DeleteTextures(1, &self.packed_texture);
            }
        }
        self.packed_fbo = 0;
        self.packed_texture = 0;
    }

    pub(crate) unsafe fn release_gpu_resources(&mut self, gl: &ffi::Gles2) {
        unsafe {
            if self.pbo.iter().any(|&buffer| buffer != 0) {
                gl.DeleteBuffers(2, self.pbo.as_ptr());
            }
            if self.capture_fbo != 0 {
                gl.DeleteFramebuffers(1, &self.capture_fbo);
            }
            if self.capture_texture != 0 {
                gl.DeleteTextures(1, &self.capture_texture);
            }
        }
        unsafe {
            self.release_packed_target(gl);
            for program in [self.pack_program, self.cursor_program] {
                if program != 0 {
                    gl.DeleteProgram(program);
                }
            }
        }
        self.pack_program = 0;
        self.cursor_program = 0;
        self.nv12 = false;
        self.pbo = [0; 2];
        self.capture_fbo = 0;
        self.capture_texture = 0;
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active
    }

    /// Where the current region lands on the encode canvas this frame.
    fn canvas_rect(&self) -> (u32, u32, u32, u32) {
        recording_canvas_rect(
            self.start_region_size,
            (self.region.2, self.region.3),
            (self.width, self.height),
        )
    }

    /// Top-down canvas position of the synthesised pointer and how many canvas
    /// pixels one cursor unit spans on each axis, or `None` without a region.
    ///
    /// Follows the scene's own mapping onto the canvas, bars included, so the
    /// arrow keeps pointing at the pixel under the real pointer and keeps the
    /// scene's proportions when the region changes shape mid-recording.
    fn cursor_placement(&self, pointer: (f32, f32)) -> Option<([f32; 2], [f32; 2])> {
        let (region_x, region_y, region_w, region_h) = self.region;
        if region_w == 0 || region_h == 0 {
            return None;
        }
        let (dest_x, dest_y, dest_w, dest_h) = self.canvas_rect();
        let scale = [
            dest_w as f32 / region_w as f32,
            dest_h as f32 / region_h as f32,
        ];
        let origin = [
            dest_x as f32 + (pointer.0.round() - region_x as f32) * scale[0],
            dest_y as f32 + (pointer.1.round() - region_y as f32) * scale[1],
        ];
        Some((origin, scale))
    }

    /// The canvas rect in framebuffer coordinates (rows counted from the
    /// bottom), which the synthesised pointer is clipped to.
    ///
    /// The cursor pass covers the whole capture target, so without the clip a
    /// pointer outside a letterboxed region was painted into the bars: the
    /// video showed an arrow tracking pointer activity outside the recorded
    /// area. Clipping at the canvas edge, rather than skipping the arrow once
    /// the pointer leaves the region, keeps the part of an arrow just outside
    /// that reaches into the region, as the target edge clips it without bars
    /// and as the region blit crops an internalized cursor.
    fn cursor_clip_rect(&self) -> (i32, i32, i32, i32) {
        let (dest_x, dest_y, dest_width, dest_height) = self.canvas_rect();
        let bottom = self.height.saturating_sub(dest_y + dest_height);
        (
            dest_x as i32,
            bottom as i32,
            dest_width as i32,
            dest_height as i32,
        )
    }

    /// Draw the synthesised pointer into the capture target.
    ///
    /// # Safety
    /// Requires the capture framebuffer bound and a current GL context.
    unsafe fn draw_cursor(&self, gl: &ffi::Gles2, pointer: (f32, f32)) {
        if self.cursor_program == 0 {
            return;
        }
        let Some((origin, scale)) = self.cursor_placement(pointer) else {
            return;
        };
        let (clip_x, clip_y, clip_width, clip_height) = self.cursor_clip_rect();
        unsafe {
            // The scene's scissor state is put back afterwards, so the rest
            // of the frame keeps the state it had.
            let scissor_was_enabled = gl.IsEnabled(ffi::SCISSOR_TEST) != ffi::FALSE;
            let mut scissor_box = [0; 4];
            gl.GetIntegerv(ffi::SCISSOR_BOX, scissor_box.as_mut_ptr());
            gl.Enable(ffi::SCISSOR_TEST);
            gl.Scissor(clip_x, clip_y, clip_width, clip_height);

            gl.UseProgram(self.cursor_program);
            uniform_2f(gl, self.cursor_program, b"u_origin\0", origin[0], origin[1]);
            uniform_2f(gl, self.cursor_program, b"u_scale\0", scale[0], scale[1]);
            uniform_2f(
                gl,
                self.cursor_program,
                b"u_target_size\0",
                self.width as f32,
                self.height as f32,
            );
            gl.Enable(ffi::BLEND);
            // The synthesised arrow carries straight alpha, unlike the
            // premultiplied cursor image the X11 backend samples.
            gl.BlendFunc(ffi::SRC_ALPHA, ffi::ONE_MINUS_SRC_ALPHA);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
            gl.Disable(ffi::BLEND);

            gl.Scissor(
                scissor_box[0],
                scissor_box[1],
                scissor_box[2],
                scissor_box[3],
            );
            if !scissor_was_enabled {
                gl.Disable(ffi::SCISSOR_TEST);
            }
        }
    }

    /// Render the capture target into its packed NV12 form and leave the packed
    /// framebuffer bound for the readback.
    ///
    /// # Safety
    /// Requires a current GL context.
    unsafe fn pack_nv12(&self, gl: &ffi::Gles2) {
        let (packed_w, packed_h) = nv12_packed_target_size(self.width, self.height);
        unsafe {
            gl.BindFramebuffer(ffi::FRAMEBUFFER, self.packed_fbo);
            gl.Viewport(0, 0, packed_w as i32, packed_h as i32);
            gl.Disable(ffi::BLEND);
            gl.UseProgram(self.pack_program);
            uniform_1i(gl, self.pack_program, b"u_source\0", 0);
            uniform_2f(
                gl,
                self.pack_program,
                b"u_video_size\0",
                self.width as f32,
                self.height as f32,
            );
            uniform_1f(gl, self.pack_program, b"u_luma_rows\0", self.height as f32);
            gl.ActiveTexture(ffi::TEXTURE0);
            gl.BindTexture(ffi::TEXTURE_2D, self.capture_texture);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
            gl.BindTexture(ffi::TEXTURE_2D, 0);
        }
    }

    /// Retire the capture this frame owed without reading any pixels, and say
    /// whether the caller should report it.
    ///
    /// The capture clock has to advance even when the source was unusable.
    /// `frame_due` is what keeps the compositor rendering for the encoder, so
    /// a due frame that neither captured nor advanced leaves `frame_due` true
    /// for good: a full-screen recomposite every loop iteration, on an
    /// otherwise static desktop, until the recording stops.
    ///
    /// Only the first skip of a run answers true. The one failure that reaches
    /// here — the encoded capture view could not be allocated — repeats every
    /// frame, and at `fps` reports a second the log is the noisier half of the
    /// bug.
    pub(crate) fn skip_frame(&mut self) -> bool {
        if !self.active || !self.frame_due() {
            return false;
        }
        self.last_capture = self.next_capture_anchor();
        self.skipped_since_capture = self.skipped_since_capture.saturating_add(1);
        self.skipped_since_capture == 1
    }

    /// Whether the next capture is due. Recording used to force a full-screen
    /// recomposite on every loop iteration to feed an encoder that only takes
    /// `fps` frames a second; the composite is only worth doing when a frame
    /// will actually be read back from it.
    pub(crate) fn frame_due(&self) -> bool {
        self.frame_deadline()
            .is_some_and(|remaining| remaining.is_zero())
    }

    /// Time until the next capture, or `None` when not recording. The event
    /// loop folds this into its dispatch timeout so a static desktop is still
    /// captured at the configured rate.
    pub(crate) fn frame_deadline(&self) -> Option<Duration> {
        if !self.active {
            return None;
        }
        Some(
            self.frame_interval()
                .saturating_sub(self.last_capture.elapsed()),
        )
    }

    fn frame_interval(&self) -> Duration {
        Duration::from_secs_f64(1.0 / self.fps.clamp(1, 240) as f64)
    }

    /// Advance the capture clock by one interval rather than restarting it from
    /// the present, so the time a frame itself takes does not push every later
    /// capture progressively later. Falling more than one interval behind
    /// resynchronizes to now instead of bursting to catch up.
    fn next_capture_anchor(&self) -> Instant {
        let now = Instant::now();
        let next = self.last_capture + self.frame_interval();
        if next + self.frame_interval() > now {
            next
        } else {
            now
        }
    }

    pub(crate) fn set_region(&mut self, region: (i32, i32, u32, u32)) {
        self.region = region;
    }

    pub(crate) fn frame_count(&self) -> u64 {
        self.frame_count
    }

    /// What the recording in progress is actually achieving, as X11 reports
    /// it: encoded size, frames read back, frames the writer thread dropped
    /// because the encoder was behind, and wall time since start.
    pub(crate) fn stats(&self) -> Option<crate::backend::api::RecordingStats> {
        if !self.active {
            return None;
        }
        let started = self.start_time?;
        Some(crate::backend::api::RecordingStats {
            output_size: (self.width, self.height),
            captured: self.frame_count,
            dropped: self.sink.as_ref().map_or(0, |sink| sink.dropped_frames()),
            elapsed_secs: started.elapsed().as_secs_f64(),
        })
    }

    pub(crate) fn elapsed(&self) -> Option<Duration> {
        self.start_time.map(|t| t.elapsed())
    }

    #[cfg(test)]
    pub(crate) unsafe fn seed_inactive_gpu_resources_for_tests(&mut self, gl: &ffi::Gles2) {
        debug_assert!(!self.active);
        debug_assert_eq!(self.pbo, [0; 2]);
        unsafe {
            let (framebuffer, texture) = super::create_fbo_texture(gl, 2, 2);
            self.capture_fbo = framebuffer;
            self.capture_texture = texture;
            gl.GenBuffers(2, self.pbo.as_mut_ptr());
        }
    }

    #[cfg(test)]
    pub(crate) const fn gpu_resources_for_tests(&self) -> ([u32; 2], u32, u32) {
        (self.pbo, self.capture_fbo, self.capture_texture)
    }

    /// An inactive recorder whose `canvas` was sized for a region of
    /// `start_region_size` and which now shows `region`, drawing its pointer
    /// with `cursor_program`, for the headless GL tests. It owns no GL
    /// resources; the caller keeps the program.
    #[cfg(test)]
    pub(crate) fn letterboxed_for_tests(
        canvas: (u32, u32),
        start_region_size: (u32, u32),
        region: (i32, i32, u32, u32),
        cursor_program: u32,
    ) -> Self {
        let mut state = Self::new();
        (state.width, state.height) = canvas;
        state.start_region_size = start_region_size;
        state.region = region;
        state.cursor_program = cursor_program;
        state
    }

    /// [`Self::draw_cursor`] for the headless GL tests.
    ///
    /// # Safety
    /// Requires the capture framebuffer bound and a current GL context.
    #[cfg(test)]
    pub(crate) unsafe fn draw_cursor_for_tests(&self, gl: &ffi::Gles2, pointer: (f32, f32)) {
        unsafe { self.draw_cursor(gl, pointer) }
    }
}

#[cfg(test)]
mod ffmpeg_log_tests {
    use super::{open_recording_ffmpeg_log, recording_ffmpeg_log_path};
    use std::ffi::OsStr;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    /// A directory only this test uses, removed again on drop.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "jwm-wayland-recording-log-{tag}-{}-{:016x}",
                std::process::id(),
                rand::random::<u64>()
            ));
            std::fs::create_dir(&path).expect("create a private scratch directory");
            Self(path)
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_log_lives_in_the_private_runtime_dir_or_under_a_per_user_temp_name() {
        let temp = Path::new("/tmp");
        assert_eq!(
            recording_ffmpeg_log_path(Some(OsStr::new("/run/user/1000")), temp, 1000),
            Path::new("/run/user/1000/jwm-wayland-recording-ffmpeg.log")
        );
        // No runtime directory: a shared directory needs a per-user name, or
        // the first user to record owns the path for everybody.
        assert_eq!(
            recording_ffmpeg_log_path(None, temp, 1000),
            Path::new("/tmp/jwm-wayland-recording-ffmpeg-1000.log")
        );
        assert_ne!(
            recording_ffmpeg_log_path(None, temp, 1000),
            recording_ffmpeg_log_path(None, temp, 1001)
        );
        // A relative runtime directory would follow the working directory.
        assert_eq!(
            recording_ffmpeg_log_path(Some(OsStr::new("run/user/1000")), temp, 1000),
            Path::new("/tmp/jwm-wayland-recording-ffmpeg-1000.log")
        );
    }

    #[test]
    fn opening_the_log_empties_this_users_file_and_keeps_it_private() {
        let dir = ScratchDir::new("reuse");
        let path = dir.0.join("jwm-wayland-recording-ffmpeg.log");
        std::fs::write(&path, b"errors from the previous recording").expect("seed old log");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("loosen seeded log");

        let mut file = open_recording_ffmpeg_log(&path).expect("reopen own log");
        file.write_all(b"fresh")
            .expect("write through the handed-out file");
        drop(file);

        assert_eq!(std::fs::read(&path).expect("read log back"), b"fresh");
        let mode = std::fs::metadata(&path)
            .expect("stat log")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn a_planted_symlink_or_non_regular_file_is_refused() {
        let dir = ScratchDir::new("planted");
        let target = dir.0.join("victim");
        std::fs::write(&target, b"keep me").expect("seed symlink target");
        let link = dir.0.join("jwm-wayland-recording-ffmpeg.log");
        std::os::unix::fs::symlink(&target, &link).expect("plant symlink");
        // The recorder logs this error and records without diagnostics,
        // instead of writing through the link.
        assert!(open_recording_ffmpeg_log(&link).is_err());
        assert_eq!(std::fs::read(&target).expect("read target"), b"keep me");

        let planted_dir = dir.0.join("planted-dir.log");
        std::fs::create_dir(&planted_dir).expect("plant a directory at the log path");
        assert!(open_recording_ffmpeg_log(&planted_dir).is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::{Duration, Instant, RecordingState, recording_output_size};

    /// A recorder mid-recording, without GL: encode size from the start region
    /// the way `start` derives it, then the region moved to `region`.
    fn recording_from(
        start: (i32, i32, u32, u32),
        max_height: u32,
        region: (i32, i32, u32, u32),
    ) -> RecordingState {
        let mut state = RecordingState::new();
        let (width, height) = recording_output_size(start.2, start.3, max_height);
        state.active = true;
        state.width = width;
        state.height = height;
        state.start_region_size = (start.2, start.3);
        state.set_region(region);
        state
    }

    #[test]
    fn a_region_reshaped_mid_recording_is_letterboxed_rather_than_stretched() {
        // Record 1600x900, then drag the right edge in to 800x900. The encoder
        // keeps its 1600x900 canvas; the new region must keep its shape inside
        // it instead of being stretched twice as wide.
        let state = recording_from((0, 0, 1600, 900), 0, (0, 0, 800, 900));
        assert_eq!(state.canvas_rect(), (400, 0, 800, 900));

        // The synthesised arrow follows the scene: one-to-one, offset by the bar.
        let (origin, scale) = state
            .cursor_placement((100.0, 50.0))
            .expect("a region places the cursor");
        assert_eq!(scale, [1.0, 1.0], "the cursor must not stretch either");
        assert_eq!(origin, [500.0, 50.0]);
        // The arrow is clipped to the canvas, so a pointer left of the region
        // (operating another app) stays out of the bar it maps into.
        let clip = state.cursor_clip_rect();
        assert_eq!(clip, (400, 0, 800, 900));
        let (origin, _) = state
            .cursor_placement((-100.0, 50.0))
            .expect("a region places the cursor");
        assert_eq!(origin, [300.0, 50.0]);
        assert!(origin[0] < clip.0 as f32, "the arrow's tip lies in the bar");

        // A capped 4K recording resized to a wide strip gets bars above and
        // below, at the canvas's own half scale, and the cursor with it.
        let state = recording_from((0, 0, 3840, 2160), 1080, (0, 540, 3840, 1080));
        assert_eq!((state.width, state.height), (1920, 1080));
        assert_eq!(state.canvas_rect(), (0, 270, 1920, 540));
        let (origin, scale) = state
            .cursor_placement((100.0, 640.0))
            .expect("a region places the cursor");
        assert_eq!(scale, [0.5, 0.5]);
        assert_eq!(origin, [50.0, 320.0]);
        assert_eq!(state.cursor_clip_rect(), (0, 270, 1920, 540));
        // The clip counts rows from the bottom, as the framebuffer does: an
        // odd leftover row puts the one-row bar at the bottom.
        let state = recording_from((0, 0, 1600, 900), 0, (0, 0, 1600, 899));
        assert_eq!(state.canvas_rect(), (0, 0, 1600, 899));
        assert_eq!(state.cursor_clip_rect(), (0, 1, 1600, 899));
    }

    #[test]
    fn the_start_region_and_its_moves_fill_the_whole_canvas() {
        // The NV12 snap makes 1603x901 encode at 1600x900, a canvas slightly
        // off the region's aspect ratio. That must still be a full-canvas blit
        // with no sliver of bar, for the start region and for a pure move.
        for (start, max_height) in [
            ((0, 0, 1603, 901), 0),
            ((0, 0, 1600, 901), 0),
            ((0, 0, 3846, 2160), 1080),
            ((0, 0, 7, 1000), 0),
        ] {
            let state = recording_from(start, max_height, start);
            let canvas = (0, 0, state.width, state.height);
            assert_eq!(state.canvas_rect(), canvas, "start region {start:?}");
            // Without bars the cursor clip is the capture target itself.
            assert_eq!(
                state.cursor_clip_rect(),
                (0, 0, state.width as i32, state.height as i32),
                "start region {start:?}"
            );
            let moved = (start.0 + 17, start.1 + 9, start.2, start.3);
            let state = recording_from(start, max_height, moved);
            assert_eq!(state.canvas_rect(), canvas, "moved region {moved:?}");
        }
    }

    #[test]
    fn stats_describe_only_a_recording_in_progress() {
        let mut state = RecordingState::new();
        assert!(state.stats().is_none(), "no recording, no stats");

        state.active = true;
        state.width = 1280;
        state.height = 720;
        state.frame_count = 48;
        state.start_time = Instant::now().checked_sub(Duration::from_secs(2));
        let stats = state.stats().expect("an active recording reports stats");
        assert_eq!(stats.output_size, (1280, 720));
        assert_eq!(stats.captured, 48);
        assert_eq!(stats.dropped, 0, "no sink, nothing dropped");
        assert!(stats.elapsed_secs >= 2.0);

        // A broken encoder pipe clears `active`; the numbers go with it.
        state.active = false;
        assert!(state.stats().is_none());
    }

    #[test]
    fn an_idle_recorder_asks_the_event_loop_for_nothing() {
        let state = RecordingState::new();
        assert_eq!(state.frame_deadline(), None);
        assert!(!state.frame_due());
    }

    #[test]
    fn a_capture_is_due_only_once_its_interval_has_passed() {
        let mut state = RecordingState::new();
        state.active = true;
        state.fps = 30;

        state.last_capture = Instant::now();
        assert!(
            !state.frame_due(),
            "a frame captured just now must not force another composite"
        );
        let remaining = state
            .frame_deadline()
            .expect("an active recording paces itself");
        assert!(
            remaining > Duration::ZERO && remaining <= Duration::from_millis(34),
            "deadline should be the remainder of one 30 fps frame, got {remaining:?}"
        );

        state.last_capture = Instant::now() - Duration::from_millis(100);
        assert!(state.frame_due());
        assert_eq!(state.frame_deadline(), Some(Duration::ZERO));
    }

    #[test]
    fn a_frame_whose_source_was_unusable_still_advances_the_capture_clock() {
        let mut state = RecordingState::new();
        state.active = true;
        state.fps = 30;

        // An unusable capture source must retire the frame it owed: leaving
        // `frame_due` true holds the compositor at a full-screen recomposite
        // per loop iteration for the rest of the recording.
        state.last_capture = Instant::now() - Duration::from_millis(100);
        assert!(state.frame_due());
        assert!(state.skip_frame(), "the first skip of a run is reported");
        assert!(!state.frame_due());

        // The run is reported once, not once per interval.
        state.last_capture = Instant::now() - Duration::from_millis(100);
        assert!(state.frame_due());
        assert!(!state.skip_frame());
        assert!(!state.frame_due());

        // A frame that is not due skips nothing and reports nothing, and an
        // idle recorder has no clock to advance at all.
        assert!(!state.skip_frame());
        let mut idle = RecordingState::new();
        idle.last_capture = Instant::now() - Duration::from_millis(100);
        assert!(!idle.skip_frame());
        assert!(!idle.frame_due());

        // A capture that succeeds re-arms the report for the next run.
        state.skipped_since_capture = 0;
        state.last_capture = Instant::now() - Duration::from_millis(100);
        assert!(state.skip_frame());
    }

    #[test]
    fn the_capture_clock_advances_by_an_interval_and_resyncs_when_far_behind() {
        let mut state = RecordingState::new();
        state.active = true;
        state.fps = 30;
        let interval = state.frame_interval();

        // A capture that ran 40 ms after the last one on a 33.3 ms budget still
        // advances by exactly one interval; restarting at `now` would push every
        // later capture further out and degrade the real sampling rate.
        state.last_capture = Instant::now() - Duration::from_millis(40);
        assert_eq!(state.next_capture_anchor(), state.last_capture + interval);

        // Half a second behind is 15 missed frames: resynchronize rather than
        // submit them back to back into the encoder we are protecting.
        state.last_capture = Instant::now() - Duration::from_millis(500);
        assert!(state.next_capture_anchor() > state.last_capture + interval);
    }
}
