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
/// Loop length of the shader's open-water swell.  Prime seconds, so the
/// surface animation never beats against the 47 s / 31 s camera waves; the
/// shader receives the phase in `[0, 1)` and uses integer cycle counts, so
/// the wrap is seamless.
const WATER_WAVE_PERIOD_S: f64 = 97.0;

pub(super) struct WaterlilyIpc {
    path: PathBuf,
    socket_identity: (u64, u64),
    pending: Arc<AtomicBool>,
    new_connection: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    loop_signal: Arc<Mutex<Option<calloop::LoopSignal>>>,
    stop: Arc<AtomicBool>,
    command_stream: Arc<Mutex<Option<UnixStream>>>,
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
        let command_stream = Arc::new(Mutex::new(None::<UnixStream>));
        let thread_pending = pending.clone();
        let thread_new_connection = new_connection.clone();
        let thread_connected = connected.clone();
        let thread_loop_signal = loop_signal.clone();
        let thread_stop = stop.clone();
        let thread_command_stream = command_stream.clone();
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
                            // Hot-switch commands travel over the same stream
                            // in the other direction; publish a writable clone
                            // for the compositor thread.
                            if let Ok(mut slot) = thread_command_stream.lock() {
                                *slot = stream.try_clone().ok();
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
                            if let Ok(mut slot) = thread_command_stream.lock() {
                                *slot = None;
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
            command_stream,
            receiver: Some(receiver),
        })
    }

    /// Deliver one newline-terminated control command to the connected worker.
    /// Returns false when no worker is connected or the write fails.
    pub(super) fn send_command(&self, command: &str) -> bool {
        use std::io::Write;
        let Ok(mut slot) = self.command_stream.lock() else {
            return false;
        };
        let Some(stream) = slot.as_mut() else {
            return false;
        };
        let payload = format!("{command}\n");
        match stream.write_all(payload.as_bytes()) {
            Ok(()) => true,
            Err(error) => {
                log::warn!("compositor: WaterLily command delivery failed: {error}");
                *slot = None;
                false
            }
        }
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
    /// GL binding target: `TEXTURE_2D` for planar frames, `TEXTURE_3D` for
    /// volumetric frames that the compositor ray-marches natively.
    pub(super) target: u32,
    pub(super) width: u32,
    pub(super) height: u32,
    /// Depth slices; one for planar frames.
    pub(super) depth: u32,
    pub(super) sequence: u64,
    pub(super) timestamp_ns: u64,
}

impl WaterlilyTexture {
    pub(super) fn is_volume(&self) -> bool {
        self.target == glow::TEXTURE_3D
    }
}

/// CPU side of the volumetric camera: a near-front perspective eye whose basis
/// vectors feed the ray-marching shader. Derived deterministically from the
/// frame timestamp so re-rendering the same frame (say, for unrelated damage)
/// reproduces the exact same image, keeping damage tracking honest, while
/// every new simulation frame advances the subtle parallax motion.
struct VolumeCamera {
    position: [f32; 3],
    right: [f32; 3],
    up: [f32; 3],
    forward: [f32; 3],
    tan_half_fov: f32,
    box_half_extents: [f32; 3],
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let length = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-6);
    [v[0] / length, v[1] / length, v[2] / length]
}

/// Wrap a monotonic timestamp into a `[0, 1)` phase of `period_s` seconds.
/// The modulo runs in integer nanoseconds so the phase keeps full precision
/// no matter how large the raw timestamp grows.
fn timestamp_phase(timestamp_ns: u64, period_s: f64) -> f64 {
    let period_ns = (period_s * 1e9) as u64;
    (timestamp_ns % period_ns.max(1)) as f64 / period_ns.max(1) as f64
}

/// Place the eye just far enough from the oriented box for every projected
/// corner to remain inside `fill` of both viewport axes.  Camera-space depth
/// for a corner is `distance + dot(corner, forward)`, so each perspective-fit
/// inequality can be rearranged into a direct lower bound on `distance`.
fn fitted_camera_distance(
    half: [f64; 3],
    right: [f32; 3],
    up: [f32; 3],
    forward: [f32; 3],
    aspect: f64,
    tan_half_fov: f64,
    fill: f64,
) -> f64 {
    let mut distance = 0.0_f64;
    for sx in [-1.0_f64, 1.0] {
        for sy in [-1.0_f64, 1.0] {
            for sz in [-1.0_f64, 1.0] {
                let corner = [sx * half[0], sy * half[1], sz * half[2]];
                let project = |axis: [f32; 3]| {
                    corner[0] * f64::from(axis[0])
                        + corner[1] * f64::from(axis[1])
                        + corner[2] * f64::from(axis[2])
                };
                let corner_depth = project(forward);
                let horizontal = project(right).abs();
                let vertical = project(up).abs();
                distance = distance.max(horizontal / (tan_half_fov * aspect * fill) - corner_depth);
                distance = distance.max(vertical / (tan_half_fov * fill) - corner_depth);
            }
        }
    }
    // Leave a sub-pixel numerical guard after the f64 result is stored in the
    // f32 camera uniforms.  This is far smaller than the intended screen
    // margin and prevents a corner from landing barely outside the tank quad.
    distance + 1e-4
}

fn volume_camera(
    width: u32,
    height: u32,
    depth: u32,
    screen_w: u32,
    screen_h: u32,
    timestamp_ns: u64,
) -> VolumeCamera {
    // Keep the aquarium facing the desktop instead of rotating it through a
    // full orbit.  Small, slow yaw and elevation waves retain parallax without
    // ever showing the tank from behind or making its footprint swing wildly.
    const YAW_PERIOD_S: f64 = 47.0;
    const ELEVATION_PERIOD_S: f64 = 31.0;
    const YAW_AMPLITUDE_RAD: f64 = 0.15;
    // Keep the eye just above the open water plane at every supported aspect
    // ratio.  Besides exposing the tank's depth, this makes the water surface
    // the first interface on every wet ray, matching the shader's front-pane
    // source-over ordering.
    const BASE_ELEVATION_RAD: f64 = 0.29;
    const ELEVATION_AMPLITUDE_RAD: f64 = 0.025;
    // 38 degrees of vertical field of view.
    const TAN_HALF_FOV: f64 = 0.344_327_6;
    // NDC spans -1..1, so 0.92 leaves about four percent of the viewport on
    // each side of the limiting axis for a visible glass rim.
    const VIEWPORT_FILL: f64 = 0.92;

    // Simulation voxels are cubes, so the box proportions are the voxel
    // counts; normalize them so camera distances stay resolution-invariant.
    let longest = width.max(height).max(depth).max(1) as f64;
    let half = [
        0.5 * width as f64 / longest,
        0.5 * height as f64 / longest,
        0.5 * depth as f64 / longest,
    ];

    let yaw_phase = std::f64::consts::TAU * timestamp_phase(timestamp_ns, YAW_PERIOD_S);
    let elevation_phase = std::f64::consts::TAU * timestamp_phase(timestamp_ns, ELEVATION_PERIOD_S);
    let yaw = YAW_AMPLITUDE_RAD * yaw_phase.sin();
    let elevation = BASE_ELEVATION_RAD + ELEVATION_AMPLITUDE_RAD * elevation_phase.sin();

    let (sin_yaw, cos_yaw) = yaw.sin_cos();
    let (sin_elevation, cos_elevation) = elevation.sin_cos();
    let eye_direction = [
        (cos_elevation * sin_yaw) as f32,
        sin_elevation as f32,
        (-cos_elevation * cos_yaw) as f32,
    ];
    let forward = normalize([-eye_direction[0], -eye_direction[1], -eye_direction[2]]);
    // `up x forward` keeps world +X on screen right at the resting azimuth,
    // matching the planar projection the volumetric path replaces.
    let right = normalize(cross([0.0, 1.0, 0.0], forward));
    let up = cross(forward, right);
    let aspect = screen_w.max(1) as f64 / screen_h.max(1) as f64;
    let distance = fitted_camera_distance(
        half,
        right,
        up,
        forward,
        aspect,
        TAN_HALF_FOV,
        VIEWPORT_FILL,
    );
    let position = [
        eye_direction[0] * distance as f32,
        eye_direction[1] * distance as f32,
        eye_direction[2] * distance as f32,
    ];

    VolumeCamera {
        position,
        right,
        up,
        forward,
        tan_half_fov: TAN_HALF_FOV as f32,
        box_half_extents: [half[0] as f32, half[1] as f32, half[2] as f32],
    }
}

impl<C: CompositorConnection> Compositor<C> {
    pub(crate) fn set_waterlily_loop_signal(&mut self, signal: calloop::LoopSignal) {
        self.waterlily_loop_signal = Some(signal.clone());
        if let Some(ipc) = &self.waterlily_ipc {
            ipc.set_loop_signal(signal);
        }
    }

    /// Heal a missing wake socket. The startup bind is lost when a restart
    /// overlaps the previous compositor instance still holding the listener
    /// (its exit then removes the socket file); retrying on the next control
    /// action recovers without another restart, and the worker's per-publish
    /// reconnect picks the new socket up on its own.
    fn ensure_waterlily_ipc(&mut self) -> bool {
        if self.waterlily_ipc.is_some() {
            return true;
        }
        match WaterlilyIpc::bind_default() {
            Ok(ipc) => {
                if let Some(signal) = self.waterlily_loop_signal.clone() {
                    ipc.set_loop_signal(signal);
                }
                log::info!("compositor: WaterLily frame IPC recovered");
                self.waterlily_ipc = Some(ipc);
                true
            }
            Err(error) => {
                log::warn!("compositor: WaterLily frame IPC still unavailable: {error}");
                false
            }
        }
    }

    pub(crate) fn toggle_waterlily_effect(&mut self) -> bool {
        self.ensure_waterlily_ipc();
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

    /// Ask the connected worker to switch its simulation case. `next` cycles
    /// the worker's registry; other names must match a registered case, which
    /// the worker validates so a stale compositor list can never pin it.
    pub(crate) fn set_waterlily_case(&mut self, case: &str) -> bool {
        if !is_valid_case_request(case) {
            log::warn!("compositor: rejected malformed WaterLily case request {case:?}");
            return false;
        }
        if !self.ensure_waterlily_ipc() {
            return false;
        }
        let Some(ipc) = self.waterlily_ipc.as_ref() else {
            return false;
        };
        if !ipc.connected() {
            log::info!("compositor: no WaterLily worker connected; case request dropped");
            return false;
        }
        let delivered = ipc.send_command(&format!("case {case}"));
        if delivered {
            ipc.request_poll();
            log::info!("compositor: requested WaterLily case {case}");
        }
        delivered
    }

    /// Ask the connected worker to switch its render palette. `next` cycles
    /// the worker's registry, `auto` returns to the per-case default; the
    /// worker validates names so a stale compositor list can never pin it.
    pub(crate) fn set_waterlily_palette(&mut self, palette: &str) -> bool {
        if !is_valid_case_request(palette) {
            log::warn!("compositor: rejected malformed WaterLily palette request {palette:?}");
            return false;
        }
        if !self.ensure_waterlily_ipc() {
            return false;
        }
        let Some(ipc) = self.waterlily_ipc.as_ref() else {
            return false;
        };
        if !ipc.connected() {
            log::info!("compositor: no WaterLily worker connected; palette request dropped");
            return false;
        }
        let delivered = ipc.send_command(&format!("palette {palette}"));
        if delivered {
            ipc.request_poll();
            log::info!("compositor: requested WaterLily palette {palette}");
        }
        delivered
    }

    /// Snapshot the layer state for the `get_waterlily_status` IPC query.
    pub(crate) fn waterlily_status(&self) -> crate::backend::api::WaterlilyStatus {
        let connected = self
            .waterlily_ipc
            .as_ref()
            .is_some_and(|ipc| ipc.connected());
        let (frame_width, frame_height, frame_depth, frame_sequence) = self
            .waterlily_texture
            .as_ref()
            .map_or((0, 0, 0, 0), |texture| {
                (
                    texture.width,
                    texture.height,
                    texture.depth,
                    texture.sequence,
                )
            });
        crate::backend::api::WaterlilyStatus {
            enabled: self.waterlily_effect_enabled,
            active: self.waterlily_visible(),
            worker_connected: connected,
            frame_width,
            frame_height,
            frame_depth,
            frame_sequence,
        }
    }

    /// Stream the pointer to the worker so interactive cases (stylus) can
    /// chase it. Throttled to ~30 Hz and suppressed while the effect is not
    /// on screen; a dropped sample is harmless because the worker only ever
    /// wants the latest target.
    pub(crate) fn forward_waterlily_pointer(&mut self, x: f32, y: f32) {
        const MIN_INTERVAL: Duration = Duration::from_millis(33);
        const MIN_TRAVEL: f32 = 2.0;
        if !self.waterlily_visible() {
            return;
        }
        let Some(ipc) = self.waterlily_ipc.as_ref() else {
            return;
        };
        if !ipc.connected() {
            return;
        }
        let now = Instant::now();
        if let Some((sent_at, sent_x, sent_y)) = self.waterlily_pointer_sent {
            let travelled = (x - sent_x).hypot(y - sent_y);
            if now.duration_since(sent_at) < MIN_INTERVAL || travelled < MIN_TRAVEL {
                return;
            }
        }
        let width = self.screen_w.max(1) as f32;
        let height = self.screen_h.max(1) as f32;
        let normalized_x = (x / width).clamp(0.0, 1.0);
        let normalized_y = (y / height).clamp(0.0, 1.0);
        if ipc.send_command(&format!("pointer {normalized_x:.4} {normalized_y:.4}")) {
            self.waterlily_pointer_sent = Some((now, x, y));
        }
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

        // The WM only sees motion events during grabs or over the bare root
        // window, so motion over client windows never reaches
        // set_mouse_position. Poll the pointer on each worker wake (~worker
        // fps) instead; forward_waterlily_pointer applies its own visibility,
        // throttle, and minimum-travel gates.
        if self.waterlily_visible() {
            if let Some((x, y)) = self.conn.query_pointer_root(self.root) {
                self.forward_waterlily_pointer(f32::from(x), f32::from(y));
            }
        }
        changed
    }

    pub(super) fn waterlily_visible(&self) -> bool {
        self.waterlily_effect_enabled && self.waterlily_active && self.waterlily_texture.is_some()
    }

    /// Copy the canvas damage area into a private scene texture. The WaterLily
    /// shader samples this snapshot for its frosted backdrop, avoiding
    /// read/write feedback and leaving the regular client blur cache
    /// untouched.
    pub(super) fn prepare_waterlily_backdrop(
        &mut self,
        scissor_active: bool,
    ) -> Option<glow::Texture> {
        let damage = self.waterlily_damage_rect()?;
        if self.waterlily_scene_fbo.is_none() {
            match unsafe { Self::create_scene_fbo(&self.gl, self.screen_w, self.screen_h) } {
                Ok(snapshot) => self.waterlily_scene_fbo = Some(snapshot),
                Err(error) => {
                    log::warn!("compositor: WaterLily backdrop allocation failed: {error}");
                    return None;
                }
            }
        }
        let (framebuffer, texture) = *self.waterlily_scene_fbo.as_ref()?;
        let x0 = damage.x;
        let x1 = damage.x + damage.width as i32;
        let y0 = self.screen_h as i32 - damage.y - damage.height as i32;
        let y1 = y0 + damage.height as i32;

        unsafe {
            if scissor_active {
                self.gl.disable(glow::SCISSOR_TEST);
            }
            self.gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
            self.gl
                .bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(framebuffer));
            self.gl.blit_framebuffer(
                x0,
                y0,
                x1,
                y1,
                x0,
                y0,
                x1,
                y1,
                glow::COLOR_BUFFER_BIT,
                glow::NEAREST,
            );
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            // Prefilter the snapshot: the frosted-backdrop shaders take
            // sparse taps (about 2.75 px apart), which undersampled
            // high-frequency desktop content — photographic wallpaper
            // grain, text — into per-pixel speckle wherever water
            // transmitted the scene.  Sampling a mip level at or below the
            // tap spacing makes the broad transmission lobe alias-free.
            self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::LINEAR_MIPMAP_LINEAR as i32,
            );
            self.gl.generate_mipmap(glow::TEXTURE_2D);
            self.gl.bind_texture(glow::TEXTURE_2D, None);
            self.gl
                .viewport(0, 0, self.screen_w as i32, self.screen_h as i32);
            if scissor_active {
                self.gl.enable(glow::SCISSOR_TEST);
            }
        }
        Some(texture)
    }

    /// Draw the latest worker frame as a full-screen canvas: the simulation is
    /// stretched to cover the entire output, so its quiescent near-white fluid
    /// becomes a frosted backdrop across the whole display while the wandering
    /// body's wake ripples through it. Volumetric frames instead ray-march the
    /// 3D field through an orbiting perspective camera, showing the solve with
    /// real parallax and occlusion. The Composite Overlay Window remains
    /// input transparent; only this visual quad is added above clients.
    pub(super) fn render_waterlily_layer(
        &mut self,
        projection: &[f32; 16],
        backdrop_texture: Option<glow::Texture>,
    ) {
        if !self.waterlily_visible() {
            return;
        }
        let Some(frame) = self.waterlily_texture.as_ref() else {
            return;
        };
        if frame.is_volume() {
            self.render_waterlily_volume(projection, backdrop_texture);
            return;
        }
        let texture = frame.texture;
        let (x, y) = (0.0_f32, 0.0_f32);
        let (width, height) = (self.screen_w, self.screen_h);

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
                x,
                y,
                width as f32,
                height as f32,
            );
            self.gl
                .uniform_1_i32(self.waterlily_uniforms.texture.as_ref(), 0);
            self.gl
                .uniform_1_i32(self.waterlily_uniforms.scene_texture.as_ref(), 1);
            self.gl.uniform_1_i32(
                self.waterlily_uniforms.scene_available.as_ref(),
                i32::from(backdrop_texture.is_some()),
            );
            self.gl.uniform_2_f32(
                self.waterlily_uniforms.screen_size.as_ref(),
                self.screen_w as f32,
                self.screen_h as f32,
            );
            self.gl.uniform_1_f32(
                self.waterlily_uniforms.opacity.as_ref(),
                self.waterlily_opacity,
            );
            self.gl.active_texture(glow::TEXTURE1);
            self.gl.bind_texture(glow::TEXTURE_2D, backdrop_texture);
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            self.gl_state_tracker
                .bind_vertex_array(&self.gl, Some(self.quad_vao));
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            self.gl_state_tracker.bind_vertex_array(&self.gl, None);
            self.gl_state_tracker.use_program(&self.gl, None);
            self.gl.active_texture(glow::TEXTURE1);
            self.gl.bind_texture(glow::TEXTURE_2D, None);
            self.gl.active_texture(glow::TEXTURE0);
        }
    }

    /// Ray-march a volumetric frame: a perspective camera orbits the tank,
    /// compositing per-voxel emission and opacity front to back over the same
    /// frosted-desktop backdrop the planar path uses for quiescent water.
    fn render_waterlily_volume(
        &mut self,
        projection: &[f32; 16],
        backdrop_texture: Option<glow::Texture>,
    ) {
        let Some(frame) = self.waterlily_texture.as_ref() else {
            return;
        };
        debug_assert!(frame.is_volume());
        let texture = frame.texture;
        let camera = volume_camera(
            frame.width,
            frame.height,
            frame.depth,
            self.screen_w,
            self.screen_h,
            frame.timestamp_ns,
        );
        let uniforms = &self.waterlily_volume_uniforms;

        unsafe {
            self.gl_state_tracker
                .use_program(&self.gl, Some(self.waterlily_volume_program));
            self.gl
                .uniform_matrix_4_f32_slice(uniforms.projection.as_ref(), false, projection);
            self.gl.uniform_4_f32(
                uniforms.rect.as_ref(),
                0.0,
                0.0,
                self.screen_w as f32,
                self.screen_h as f32,
            );
            self.gl.uniform_1_i32(uniforms.volume.as_ref(), 0);
            self.gl.uniform_1_i32(uniforms.scene_texture.as_ref(), 1);
            self.gl.uniform_1_i32(
                uniforms.scene_available.as_ref(),
                i32::from(backdrop_texture.is_some()),
            );
            self.gl.uniform_2_f32(
                uniforms.screen_size.as_ref(),
                self.screen_w as f32,
                self.screen_h as f32,
            );
            self.gl
                .uniform_1_f32(uniforms.opacity.as_ref(), self.waterlily_opacity);
            self.gl.uniform_3_f32(
                uniforms.camera_position.as_ref(),
                camera.position[0],
                camera.position[1],
                camera.position[2],
            );
            self.gl.uniform_3_f32(
                uniforms.camera_right.as_ref(),
                camera.right[0],
                camera.right[1],
                camera.right[2],
            );
            self.gl.uniform_3_f32(
                uniforms.camera_up.as_ref(),
                camera.up[0],
                camera.up[1],
                camera.up[2],
            );
            self.gl.uniform_3_f32(
                uniforms.camera_forward.as_ref(),
                camera.forward[0],
                camera.forward[1],
                camera.forward[2],
            );
            self.gl
                .uniform_1_f32(uniforms.tan_half_fov.as_ref(), camera.tan_half_fov);
            self.gl.uniform_3_f32(
                uniforms.box_half_extents.as_ref(),
                camera.box_half_extents[0],
                camera.box_half_extents[1],
                camera.box_half_extents[2],
            );
            // Same clock as the camera pose: the open-water swell animates
            // with the simulation stream and re-rendering an unchanged frame
            // reproduces the identical image for damage tracking.
            self.gl.uniform_1_f32(
                uniforms.time.as_ref(),
                timestamp_phase(frame.timestamp_ns, WATER_WAVE_PERIOD_S) as f32,
            );
            self.gl.active_texture(glow::TEXTURE1);
            self.gl.bind_texture(glow::TEXTURE_2D, backdrop_texture);
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_3D, Some(texture));
            // The shader emits premultiplied RGBA.  Reassert the matching
            // blend function here because optional overlay passes can switch
            // to straight-alpha blending earlier in the frame.
            self.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
            // Hardware GL ignores dithering on 8-bit targets, but software
            // rasterizers (llvmpipe in the nested Xephyr smoke sessions)
            // stipple every smooth volume gradient with it, which polluted
            // screenshot-based debugging of real shader noise.
            self.gl.disable(glow::DITHER);
            self.gl_state_tracker
                .bind_vertex_array(&self.gl, Some(self.quad_vao));
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            self.gl.enable(glow::DITHER);
            self.gl_state_tracker.bind_vertex_array(&self.gl, None);
            self.gl_state_tracker.use_program(&self.gl, None);
            self.gl.active_texture(glow::TEXTURE1);
            self.gl.bind_texture(glow::TEXTURE_2D, None);
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_3D, None);
        }
    }

    /// The stretched canvas covers the whole output, so any frame update
    /// damages the full screen.
    fn waterlily_damage_rect(&self) -> Option<DirtyRect> {
        if !self.waterlily_visible() || self.screen_w == 0 || self.screen_h == 0 {
            return None;
        }
        Some(DirtyRect::new(0, 0, self.screen_w, self.screen_h))
    }

    fn mark_waterlily_damage(&mut self, rect: DirtyRect) {
        self.damage_tracker
            .mark_region_dirty(rect.x, rect.y, rect.width, rect.height);
        self.dirty_region_tracker.mark_dirty(rect);
    }

    fn upload_waterlily_frame(&mut self, frame: WaterlilyFrame) -> bool {
        let target = if frame.depth > 1 {
            glow::TEXTURE_3D
        } else {
            glow::TEXTURE_2D
        };
        let limit_parameter = if target == glow::TEXTURE_3D {
            glow::MAX_3D_TEXTURE_SIZE
        } else {
            glow::MAX_TEXTURE_SIZE
        };
        let max_texture_size = unsafe { self.gl.get_parameter_i32(limit_parameter) }.max(0) as u32;
        if max_texture_size == 0
            || frame.width > max_texture_size
            || frame.height > max_texture_size
            || frame.depth > max_texture_size
        {
            log::warn!(
                "compositor: rejected WaterLily texture {}x{}x{} (GPU limit {})",
                frame.width,
                frame.height,
                frame.depth,
                max_texture_size
            );
            return false;
        }

        let recreate = self.waterlily_texture.as_ref().is_none_or(|current| {
            current.width != frame.width
                || current.height != frame.height
                || current.depth != frame.depth
                || current.target != target
        });
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
                self.gl.bind_texture(target, Some(texture));
                if target == glow::TEXTURE_3D {
                    self.gl.tex_image_3d(
                        glow::TEXTURE_3D,
                        0,
                        glow::RGBA8 as i32,
                        frame.width as i32,
                        frame.height as i32,
                        frame.depth as i32,
                        0,
                        glow::RGBA,
                        glow::UNSIGNED_BYTE,
                        glow::PixelUnpackData::Slice(Some(&frame.rgba)),
                    );
                } else {
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
                }
                for filter in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
                    self.gl
                        .tex_parameter_i32(target, filter, glow::LINEAR as i32);
                }
                // The ray-marcher clamps its sample coordinates, and edge
                // clamping keeps boundary voxels from wrapping around the
                // tank on all three axes.
                let mut wraps = vec![glow::TEXTURE_WRAP_S, glow::TEXTURE_WRAP_T];
                if target == glow::TEXTURE_3D {
                    wraps.push(glow::TEXTURE_WRAP_R);
                }
                for wrap in wraps {
                    self.gl
                        .tex_parameter_i32(target, wrap, glow::CLAMP_TO_EDGE as i32);
                }
                self.gl.bind_texture(target, None);
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
                    target,
                    width: frame.width,
                    height: frame.height,
                    depth: frame.depth,
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
                if target == glow::TEXTURE_3D {
                    self.pbo_uploader.upload_texture_3d(
                        &self.gl,
                        texture,
                        frame.width,
                        frame.height,
                        frame.depth,
                        glow::RGBA,
                        &frame.rgba,
                    )
                } else {
                    self.pbo_uploader.upload_texture(
                        &self.gl,
                        texture,
                        frame.width,
                        frame.height,
                        glow::RGBA,
                        &frame.rgba,
                    )
                }
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

/// Case requests are forwarded verbatim onto the worker control stream, so
/// only short lowercase identifiers (or the `next` cycling keyword) are
/// eligible; anything else could smuggle protocol framing.
fn is_valid_case_request(case: &str) -> bool {
    !case.is_empty()
        && case.len() <= 32
        && case.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
        })
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
    fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
        a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
    }

    fn projected_box_fill(
        camera: &super::VolumeCamera,
        screen_w: u32,
        screen_h: u32,
    ) -> (f32, f32) {
        let aspect = screen_w.max(1) as f32 / screen_h.max(1) as f32;
        let mut max_x = 0.0_f32;
        let mut max_y = 0.0_f32;
        for sx in [-1.0_f32, 1.0] {
            for sy in [-1.0_f32, 1.0] {
                for sz in [-1.0_f32, 1.0] {
                    let corner = [
                        sx * camera.box_half_extents[0],
                        sy * camera.box_half_extents[1],
                        sz * camera.box_half_extents[2],
                    ];
                    let from_eye = [
                        corner[0] - camera.position[0],
                        corner[1] - camera.position[1],
                        corner[2] - camera.position[2],
                    ];
                    let depth = dot(from_eye, camera.forward);
                    assert!(
                        depth > 0.0,
                        "every aquarium corner must be in front of the eye"
                    );
                    max_x = max_x.max(
                        (dot(from_eye, camera.right) / (depth * camera.tan_half_fov * aspect))
                            .abs(),
                    );
                    max_y =
                        max_y.max((dot(from_eye, camera.up) / (depth * camera.tan_half_fov)).abs());
                }
            }
        }
        (max_x, max_y)
    }

    #[test]
    fn volume_camera_is_deterministic_and_orthonormal() {
        for timestamp in [0_u64, 1_000_000_000, 987_654_321_012, u64::MAX / 3] {
            let camera = super::volume_camera(96, 64, 32, 1920, 1080, timestamp);
            let again = super::volume_camera(96, 64, 32, 1920, 1080, timestamp);
            // Same frame timestamp, same camera: unrelated damage can force a
            // re-render of an unchanged frame and must reproduce it exactly.
            assert_eq!(camera.position, again.position);
            assert_eq!(camera.forward, again.forward);

            assert!((dot(camera.forward, camera.forward) - 1.0).abs() < 1e-4);
            assert!((dot(camera.right, camera.right) - 1.0).abs() < 1e-4);
            assert!((dot(camera.up, camera.up) - 1.0).abs() < 1e-4);
            assert!(dot(camera.forward, camera.right).abs() < 1e-4);
            assert!(dot(camera.forward, camera.up).abs() < 1e-4);
            assert!(dot(camera.right, camera.up).abs() < 1e-4);
            // The eye always looks at the box center.
            let to_center = super::normalize([
                -camera.position[0],
                -camera.position[1],
                -camera.position[2],
            ]);
            assert!(dot(to_center, camera.forward) > 0.999);
            // The aquarium remains in the front hemisphere throughout the
            // motion, with only a small side-to-side reveal of its depth.
            let horizontal_distance = camera.position[0].hypot(camera.position[2]);
            assert!(camera.position[2] < 0.0);
            assert!(-camera.position[2] / horizontal_distance > 0.98);
            assert!(camera.position[0].abs() < -0.16 * camera.position[2]);
            // Camera motion stays level: screen right never rolls off the horizon.
            assert!(camera.right[1].abs() < 1e-4);
        }

        // At the resting yaw the camera sits on the front (-Z) side with
        // world +X on screen right, matching the planar view it replaces.
        let resting = super::volume_camera(96, 64, 32, 1920, 1080, 0);
        assert!(resting.position[2] < 0.0);
        assert!(resting.position[0].abs() < 1e-4);
        assert!(resting.right[0] > 0.999);
        // The box proportions follow the voxel counts.
        assert_eq!(resting.box_half_extents[0], 0.5);
        assert!((resting.box_half_extents[1] - 64.0 / 96.0 / 2.0).abs() < 1e-6);
        assert!((resting.box_half_extents[2] - 32.0 / 96.0 / 2.0).abs() < 1e-6);
    }

    #[test]
    fn volume_camera_fits_the_oriented_tank_near_fullscreen() {
        for (screen_w, screen_h) in [(1920, 1080), (3440, 1440), (1080, 1920)] {
            for timestamp in [0_u64, 7_000_000_000, 19_000_000_000, 43_000_000_000] {
                let camera = super::volume_camera(96, 64, 32, screen_w, screen_h, timestamp);
                let (fill_x, fill_y) = projected_box_fill(&camera, screen_w, screen_h);
                let limiting_fill = fill_x.max(fill_y);
                assert!(
                    camera.position[1] > camera.box_half_extents[1] * 0.88,
                    "camera must remain above the open water surface"
                );
                assert!(
                    fill_x <= 0.921 && fill_y <= 0.921,
                    "projected tank escaped viewport fit: {fill_x} x {fill_y}"
                );
                assert!(
                    limiting_fill >= 0.915,
                    "projected tank should fill about 92% of one axis, got {fill_x} x {fill_y}"
                );
            }
        }
    }

    #[test]
    fn volume_camera_motion_is_smooth() {
        let mut previous = None;
        for second in 0_u64..=120 {
            let camera = super::volume_camera(96, 64, 32, 1920, 1080, second * 1_000_000_000);
            let direction = super::normalize(camera.position);
            if let Some(previous) = previous {
                assert!(
                    dot(previous, direction) > 0.999,
                    "one-second camera motion must not jump"
                );
            }
            previous = Some(direction);
        }
    }

    #[test]
    fn command_stream_reaches_a_connected_worker() {
        use std::io::{BufRead, BufReader};

        let path =
            std::env::temp_dir().join(format!("jwm-waterlily-cmd-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let ipc = super::WaterlilyIpc::bind(path.clone()).unwrap();

        assert!(
            !ipc.send_command("case dance"),
            "commands must not be deliverable before a worker connects"
        );

        let worker = std::os::unix::net::UnixStream::connect(&path).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !ipc.connected() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(ipc.connected());
        assert!(ipc.send_command("case dance"));

        worker
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut line = String::new();
        BufReader::new(&worker).read_line(&mut line).unwrap();
        assert_eq!(line, "case dance\n");
    }

    #[test]
    fn case_requests_are_restricted_to_safe_identifiers() {
        use super::is_valid_case_request;
        assert!(is_valid_case_request("next"));
        assert!(is_valid_case_request("hover"));
        assert!(is_valid_case_request("von-karman_2"));
        assert!(!is_valid_case_request(""));
        assert!(!is_valid_case_request("case hover"));
        assert!(!is_valid_case_request("hover\nquit"));
        assert!(!is_valid_case_request("../../etc/passwd"));
        assert!(!is_valid_case_request("Hover"));
        assert!(!is_valid_case_request(&"x".repeat(33)));
    }
}
