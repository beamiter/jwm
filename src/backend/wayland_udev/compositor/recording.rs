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
    ) -> Result<(), String> {
        if self.active {
            return Err("Recording already active".to_string());
        }

        self.width = width;
        self.height = height;
        self.fps = fps;

        unsafe {
            gl.GenBuffers(2, self.pbo.as_mut_ptr());

            let buffer_size = (width * height * 4) as isize;
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
        }

        let child = Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgba",
                "-s",
                &format!("{}x{}", width, height),
                "-r",
                &fps.to_string(),
                "-i",
                "pipe:0",
                "-c:v",
                "libx264",
                "-preset",
                "fast",
                "-crf",
                "23",
                "-pix_fmt",
                "yuv420p",
                output_path,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("Failed to spawn ffmpeg: {}", e))?;

        self.child = Some(child);
        self.active = true;
        self.frame_count = 0;
        self.current_pbo = 0;
        self.start_time = Some(Instant::now());
        self.last_capture = Instant::now();

        Ok(())
    }

    pub(crate) unsafe fn capture_frame(&mut self, gl: &ffi::Gles2, source_fbo: u32) {
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

            gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, self.pbo[self.current_pbo]);
            gl.ReadPixels(
                0,
                0,
                self.width as i32,
                self.height as i32,
                ffi::RGBA,
                ffi::UNSIGNED_BYTE,
                std::ptr::null_mut(),
            );

            self.current_pbo ^= 1;

            if self.frame_count > 0 {
                let other_pbo = self.current_pbo;
                gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, self.pbo[other_pbo]);

                let buffer_size = (self.width * self.height * 4) as isize;
                let ptr = gl.MapBufferRange(
                    ffi::PIXEL_PACK_BUFFER,
                    0,
                    buffer_size,
                    ffi::MAP_READ_BIT,
                );

                if !ptr.is_null() {
                    let data =
                        std::slice::from_raw_parts(ptr as *const u8, buffer_size as usize);

                    if let Some(ref mut child) = self.child {
                        if let Some(ref mut stdin) = child.stdin {
                            let _ = stdin.write_all(data);
                        }
                    }

                    gl.UnmapBuffer(ffi::PIXEL_PACK_BUFFER);
                }
            }

            gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, 0);
        }
        self.frame_count += 1;
    }

    pub(crate) unsafe fn stop(&mut self, gl: &ffi::Gles2) {
        if !self.active {
            return;
        }

        if let Some(mut child) = self.child.take() {
            drop(child.stdin.take());
            let _ = child.wait();
        }

        unsafe {
            gl.DeleteBuffers(2, self.pbo.as_ptr());
        }
        self.pbo = [0; 2];

        self.active = false;
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active
    }

    pub(crate) fn frame_count(&self) -> u64 {
        self.frame_count
    }

    pub(crate) fn elapsed(&self) -> Option<Duration> {
        self.start_time.map(|t| t.elapsed())
    }
}
