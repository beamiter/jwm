use super::{Compositor, CompositorConnection, DirtyRect};
use crate::backend::compositor_common::waterlily::WaterlilyFrame;
use glow::HasContext;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const RECEIVE_TIMEOUT: Duration = Duration::from_millis(100);
const ACCEPT_RETRY: Duration = Duration::from_millis(20);
const PRODUCER_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_WATERLILY_PBO_BYTES: usize = 64 * 1024 * 1024;

pub(super) struct WaterlilyIpc {
    path: PathBuf,
    socket_identity: (u64, u64),
    pending: Arc<AtomicBool>,
    new_connection: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    loop_signal: Arc<Mutex<Option<calloop::LoopSignal>>>,
    stop: Arc<AtomicBool>,
    receiver: Option<JoinHandle<()>>,
}

impl WaterlilyIpc {
    pub(super) fn bind_default() -> io::Result<Self> {
        Self::bind(default_socket_path())
    }

    fn bind(path: PathBuf) -> io::Result<Self> {
        prepare_runtime_parent(&path)?;
        remove_stale_socket(&path)?;

        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let metadata = fs::symlink_metadata(&path)?;
        let socket_identity = (metadata.dev(), metadata.ino());

        let pending = Arc::new(AtomicBool::new(false));
        let new_connection = Arc::new(AtomicBool::new(false));
        let connected = Arc::new(AtomicBool::new(false));
        let loop_signal = Arc::new(Mutex::new(None::<calloop::LoopSignal>));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_pending = pending.clone();
        let thread_new_connection = new_connection.clone();
        let thread_connected = connected.clone();
        let thread_loop_signal = loop_signal.clone();
        let thread_stop = stop.clone();
        let log_path = path.clone();

        let receiver = std::thread::Builder::new()
            .name("jwm-waterlily-ipc".to_string())
            .spawn(move || {
                while !thread_stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            if !peer_is_current_user(&stream) {
                                log::warn!(
                                    "compositor: rejected WaterLily worker owned by another user"
                                );
                                continue;
                            }
                            if let Err(error) = stream.set_read_timeout(Some(RECEIVE_TIMEOUT)) {
                                log::warn!(
                                    "compositor: could not bound WaterLily stream reads: {error}"
                                );
                                continue;
                            }
                            thread_connected.store(true, Ordering::Release);
                            thread_new_connection.store(true, Ordering::Release);
                            mark_pending(&thread_pending, &thread_loop_signal);
                            let mut wakeups = [0u8; 64];
                            let mut last_activity = Instant::now();
                            loop {
                                if thread_stop.load(Ordering::Acquire) {
                                    break;
                                }
                                match stream.read(&mut wakeups) {
                                    Ok(0) => break,
                                    Ok(_) => {
                                        last_activity = Instant::now();
                                        mark_pending(&thread_pending, &thread_loop_signal);
                                    }
                                    Err(error)
                                        if matches!(
                                            error.kind(),
                                            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                                        ) =>
                                    {
                                        if last_activity.elapsed() >= PRODUCER_IDLE_TIMEOUT {
                                            log::warn!(
                                                "compositor: disconnected idle WaterLily producer"
                                            );
                                            break;
                                        }
                                    }
                                    Err(error) => {
                                        if !thread_stop.load(Ordering::Acquire) {
                                            log::warn!(
                                                "compositor: WaterLily wake stream failed on {}: {error}",
                                                log_path.display()
                                            );
                                        }
                                        break;
                                    }
                                }
                            }
                            thread_connected.store(false, Ordering::Release);
                            mark_pending(&thread_pending, &thread_loop_signal);
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(ACCEPT_RETRY);
                        }
                        Err(error) => {
                            if !thread_stop.load(Ordering::Acquire) {
                                log::warn!(
                                    "compositor: WaterLily IPC accept failed on {}: {error}",
                                    log_path.display()
                                );
                            }
                            break;
                        }
                    }
                }
                thread_connected.store(false, Ordering::Release);
            })?;

        log::info!(
            "compositor: WaterLily frame wake socket listening on {}",
            path.display()
        );
        Ok(Self {
            path,
            socket_identity,
            pending,
            new_connection,
            connected,
            loop_signal,
            stop,
            receiver: Some(receiver),
        })
    }

    pub(super) fn has_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }

    pub(super) fn take_pending(&self) -> bool {
        self.pending.swap(false, Ordering::AcqRel)
    }

    fn take_new_connection(&self) -> bool {
        self.new_connection.swap(false, Ordering::AcqRel)
    }

    pub(super) fn request_poll(&self) {
        mark_pending(&self.pending, &self.loop_signal);
    }

    pub(super) fn connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    pub(super) fn set_loop_signal(&self, signal: calloop::LoopSignal) {
        if let Ok(mut slot) = self.loop_signal.lock() {
            *slot = Some(signal.clone());
        }
        if self.has_pending() {
            signal.wakeup();
        }
    }
}

impl Drop for WaterlilyIpc {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = UnixStream::connect(&self.path).and_then(|mut stream| {
            use std::io::Write;
            stream.write_all(&[0])
        });
        if let Some(receiver) = self.receiver.take() {
            let _ = receiver.join();
        }
        if fs::symlink_metadata(&self.path)
            .ok()
            .is_some_and(|metadata| (metadata.dev(), metadata.ino()) == self.socket_identity)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub(super) struct WaterlilyTexture {
    pub(super) texture: glow::Texture,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) sequence: u64,
    pub(super) timestamp_ns: u64,
}

impl<C: CompositorConnection> Compositor<C> {
    pub(crate) fn set_waterlily_loop_signal(&self, signal: calloop::LoopSignal) {
        if let Some(ipc) = &self.waterlily_ipc {
            ipc.set_loop_signal(signal);
        }
    }

    pub(crate) fn toggle_waterlily_effect(&mut self) -> bool {
        let previous_damage = self.waterlily_damage_rect();
        self.waterlily_effect_enabled = !self.waterlily_effect_enabled;
        self.waterlily_active = false;
        self.waterlily_frame_reader.reset();
        if let Some(ipc) = &self.waterlily_ipc {
            ipc.request_poll();
        }
        self.needs_render = true;
        if let Some(rect) = previous_damage {
            self.mark_waterlily_damage(rect);
            self.waterlily_layer_dirty = true;
        }
        log::info!(
            "compositor: WaterLily effect {}",
            if self.waterlily_effect_enabled {
                "enabled"
            } else {
                "disabled"
            }
        );
        self.waterlily_effect_enabled
    }

    pub(super) fn poll_waterlily_frame(&mut self) -> bool {
        let Some(ipc) = self.waterlily_ipc.as_ref() else {
            return false;
        };
        if !ipc.take_pending() {
            return false;
        }

        let connected = ipc.connected();
        let new_connection = ipc.take_new_connection();
        if !self.waterlily_effect_enabled {
            self.waterlily_active = false;
            return false;
        }

        // A worker restart begins its sequence at one again. Treat every
        // accepted producer connection as a fresh publication epoch so a
        // crashed/restarted worker cannot be wedged behind the old sequence.
        if new_connection {
            self.waterlily_frame_reader.reset();
        }

        let previous_damage = self.waterlily_damage_rect();
        let previously_active = self.waterlily_active;
        let mut changed = false;
        match self.waterlily_frame_reader.read_latest() {
            Ok(Some(frame)) => {
                changed = self.upload_waterlily_frame(frame);
                self.waterlily_active = connected && self.waterlily_texture.is_some();
            }
            Ok(None) => {
                if !connected && self.waterlily_active {
                    self.waterlily_active = false;
                    changed = true;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if !connected && self.waterlily_active {
                    self.waterlily_active = false;
                    changed = true;
                }
            }
            Err(error) => {
                log::warn!(
                    "compositor: rejected WaterLily frame {}: {error}",
                    self.waterlily_frame_reader.path().display()
                );
                if !connected && self.waterlily_active {
                    self.waterlily_active = false;
                    changed = true;
                }
            }
        }
        changed |= self.waterlily_active != previously_active;

        if changed {
            self.waterlily_layer_dirty = true;
            if let Some(rect) = previous_damage {
                self.mark_waterlily_damage(rect);
            }
            if let Some(rect) = self.waterlily_damage_rect() {
                self.mark_waterlily_damage(rect);
            }
        }
        changed
    }

    pub(super) fn waterlily_visible(&self) -> bool {
        self.waterlily_effect_enabled && self.waterlily_active && self.waterlily_texture.is_some()
    }

    /// Draw the latest worker frame as a one-device-pixel-per-frame-pixel
    /// compositor layer. The full-screen Composite Overlay Window remains input
    /// transparent; only this visual quad is added above clients.
    pub(super) fn render_waterlily_layer(&mut self, projection: &[f32; 16]) {
        if !self.waterlily_visible() {
            return;
        }
        let Some(frame) = self.waterlily_texture.as_ref() else {
            return;
        };
        let (texture, width, height) = (frame.texture, frame.width, frame.height);

        unsafe {
            self.gl_state_tracker
                .use_program(&self.gl, Some(self.waterlily_program));
            self.gl.uniform_matrix_4_f32_slice(
                self.waterlily_uniforms.projection.as_ref(),
                false,
                projection,
            );
            self.gl.uniform_4_f32(
                self.waterlily_uniforms.rect.as_ref(),
                0.0,
                0.0,
                width as f32,
                height as f32,
            );
            self.gl
                .uniform_1_i32(self.waterlily_uniforms.texture.as_ref(), 0);
            self.gl.uniform_1_f32(
                self.waterlily_uniforms.opacity.as_ref(),
                self.waterlily_opacity,
            );
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            self.gl_state_tracker
                .bind_vertex_array(&self.gl, Some(self.quad_vao));
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            self.gl_state_tracker.bind_vertex_array(&self.gl, None);
            self.gl_state_tracker.use_program(&self.gl, None);
        }
    }

    fn waterlily_damage_rect(&self) -> Option<DirtyRect> {
        if !self.waterlily_visible() {
            return None;
        }
        let frame = self.waterlily_texture.as_ref()?;
        visible_waterlily_size(self.screen_w, self.screen_h, frame.width, frame.height)
            .map(|(width, height)| DirtyRect::new(0, 0, width, height))
    }

    fn mark_waterlily_damage(&mut self, rect: DirtyRect) {
        self.damage_tracker
            .mark_region_dirty(rect.x, rect.y, rect.width, rect.height);
        self.dirty_region_tracker.mark_dirty(rect);
    }

    fn upload_waterlily_frame(&mut self, frame: WaterlilyFrame) -> bool {
        let max_texture_size =
            unsafe { self.gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE) }.max(0) as u32;
        if max_texture_size == 0
            || frame.width > max_texture_size
            || frame.height > max_texture_size
        {
            log::warn!(
                "compositor: rejected WaterLily texture {}x{} (GPU limit {})",
                frame.width,
                frame.height,
                max_texture_size
            );
            return false;
        }

        let recreate = self
            .waterlily_texture
            .as_ref()
            .is_none_or(|current| current.width != frame.width || current.height != frame.height);
        if recreate {
            unsafe {
                let texture = match self.gl.create_texture() {
                    Ok(texture) => texture,
                    Err(error) => {
                        log::warn!("compositor: WaterLily texture creation failed: {error}");
                        return false;
                    }
                };

                // Discard stale errors so allocation validation below pertains
                // to this replacement texture.
                for _ in 0..8 {
                    if self.gl.get_error() == glow::NO_ERROR {
                        break;
                    }
                }
                self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                self.gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA8 as i32,
                    frame.width as i32,
                    frame.height as i32,
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(Some(&frame.rgba)),
                );
                for filter in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
                    self.gl
                        .tex_parameter_i32(glow::TEXTURE_2D, filter, glow::LINEAR as i32);
                }
                for wrap in [glow::TEXTURE_WRAP_S, glow::TEXTURE_WRAP_T] {
                    self.gl
                        .tex_parameter_i32(glow::TEXTURE_2D, wrap, glow::CLAMP_TO_EDGE as i32);
                }
                self.gl.bind_texture(glow::TEXTURE_2D, None);
                let allocation_error = self.gl.get_error();
                if allocation_error != glow::NO_ERROR {
                    self.gl.delete_texture(texture);
                    log::warn!(
                        "compositor: WaterLily texture allocation failed with GL error 0x{allocation_error:x}"
                    );
                    return false;
                }

                let replacement = WaterlilyTexture {
                    texture,
                    width: frame.width,
                    height: frame.height,
                    sequence: frame.sequence,
                    timestamp_ns: frame.timestamp_ns,
                };
                if let Some(previous) = self.waterlily_texture.replace(replacement) {
                    self.gl.delete_texture(previous.texture);
                }
            }
        } else {
            let texture = self.waterlily_texture.as_ref().unwrap().texture;
            let _ = self.pbo_uploader.ensure_capacity(
                &self.gl,
                frame.rgba.len(),
                MAX_WATERLILY_PBO_BYTES,
            );
            let uploaded = unsafe {
                self.pbo_uploader.upload_texture(
                    &self.gl,
                    texture,
                    frame.width,
                    frame.height,
                    glow::RGBA,
                    &frame.rgba,
                )
            };
            if !uploaded {
                return false;
            }
            let current = self.waterlily_texture.as_mut().unwrap();
            current.sequence = frame.sequence;
            current.timestamp_ns = frame.timestamp_ns;
        }
        true
    }
}

fn visible_waterlily_size(
    screen_width: u32,
    screen_height: u32,
    frame_width: u32,
    frame_height: u32,
) -> Option<(u32, u32)> {
    let width = screen_width.min(frame_width);
    let height = screen_height.min(frame_height);
    (width > 0 && height > 0).then_some((width, height))
}

fn mark_pending(pending: &AtomicBool, loop_signal: &Mutex<Option<calloop::LoopSignal>>) {
    pending.store(true, Ordering::Release);
    if let Ok(signal) = loop_signal.lock()
        && let Some(signal) = signal.as_ref()
    {
        signal.wakeup();
    }
}

pub(super) fn default_frame_path() -> PathBuf {
    std::env::var_os("JWM_WATERLILY_FRAME_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| runtime_dir().join("jwm-waterlily.frame"))
}

fn default_socket_path() -> PathBuf {
    std::env::var_os("JWM_WATERLILY_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| runtime_dir().join("jwm-waterlily.sock"))
}

fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/jwm-{}", unsafe { libc::getuid() })))
}

fn prepare_runtime_parent(path: &Path) -> io::Result<()> {
    let Some(parent) = path.parent() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "WaterLily socket has no parent directory",
        ));
    };
    if parent.exists() {
        let metadata = fs::metadata(parent)?;
        let private_owner =
            metadata.uid() == unsafe { libc::getuid() } && metadata.mode() & 0o022 == 0;
        let sticky_shared_directory = metadata.mode() & 0o1000 != 0;
        if !metadata.is_dir() || (!private_owner && !sticky_shared_directory) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "WaterLily runtime directory is neither private nor sticky",
            ));
        }
    } else {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn remove_stale_socket(path: &Path) -> io::Result<()> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::getuid() } {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "WaterLily socket path is not a stale socket owned by this user",
        ));
    }
    if UnixStream::connect(path).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "another WaterLily consumer is already listening",
        ));
    }
    fs::remove_file(path)
}

fn peer_is_current_user(stream: &UnixStream) -> bool {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            std::os::unix::io::AsRawFd::as_raw_fd(stream),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    result == 0 && credentials.uid == unsafe { libc::getuid() }
}

#[cfg(test)]
mod tests {
    use super::visible_waterlily_size;

    #[test]
    fn worker_frame_keeps_its_native_display_size() {
        assert_eq!(
            visible_waterlily_size(1920, 1080, 320, 200),
            Some((320, 200))
        );
    }

    #[test]
    fn oversized_worker_frame_is_clipped_not_scaled() {
        assert_eq!(
            visible_waterlily_size(1920, 1080, 2560, 1440),
            Some((1920, 1080))
        );
    }
}
