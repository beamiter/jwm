use super::{Compositor, CompositorConnection};
use serde::Deserialize;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const LANDMARK_COUNT: usize = 21;
const PACKET_LIMIT: usize = 64 * 1024;
const HOLD_TIME: Duration = Duration::from_millis(120);
const FADE_TIME: Duration = Duration::from_millis(160);
const RECEIVE_TIMEOUT: Duration = Duration::from_millis(100);
const RIPPLE_LIFETIME: Duration = Duration::from_millis(1550);
const MAX_RIPPLES: usize = 48;
const FINGERTIP_INDICES: [usize; 5] = [4, 8, 12, 16, 20];

#[derive(Debug, Deserialize)]
struct SlimePacket {
    #[serde(default = "default_version")]
    version: u32,
    #[serde(default)]
    active: Option<bool>,
    #[serde(default)]
    window: Option<u32>,
    #[serde(default)]
    content_rect: Option<[f32; 4]>,
    #[serde(default)]
    refract_px: Option<f32>,
    #[serde(default)]
    hands: Vec<SlimeHand>,
}

#[derive(Debug, Deserialize)]
struct SlimeHand {
    #[serde(default = "default_score")]
    score: f32,
    landmarks: Vec<[f32; 2]>,
}

const fn default_version() -> u32 {
    1
}

const fn default_score() -> f32 {
    1.0
}

/// Lossy pose data plane for the slime effect.
///
/// A small receiver thread owns the datagram socket and continuously replaces a
/// single pending packet. This gives us two useful properties:
///
/// * old inference results can never queue up and add visual latency;
/// * `Compositor::needs_render(&self)` can cheaply observe `has_pending()` and
///   wake the compositor even while a fullscreen window was unredirected.
pub(super) struct SlimeIpc {
    path: PathBuf,
    latest: Arc<Mutex<Option<SlimePacket>>>,
    pending: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    receiver: Option<JoinHandle<()>>,
}

impl SlimeIpc {
    pub(super) fn bind_default() -> io::Result<Self> {
        let path = std::env::var_os("JWM_SLIME_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let runtime = std::env::var_os("XDG_RUNTIME_DIR")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| {
                        PathBuf::from(format!("/tmp/jwm-{}", unsafe { libc::getuid() }))
                    });
                runtime.join("jwm-slime.sock")
            });
        Self::bind(path)
    }

    fn bind(path: PathBuf) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            let created = !parent.exists();
            fs::create_dir_all(parent)?;
            if created {
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
            }
        }
        if path.exists() {
            fs::remove_file(&path)?;
        }

        let socket = UnixDatagram::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        socket.set_read_timeout(Some(RECEIVE_TIMEOUT))?;

        let latest = Arc::new(Mutex::new(None));
        let pending = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));

        let thread_latest = latest.clone();
        let thread_pending = pending.clone();
        let thread_stop = stop.clone();
        let log_path = path.clone();
        let receiver = std::thread::Builder::new()
            .name("jwm-slime-ipc".to_string())
            .spawn(move || {
                let mut buffer = vec![0u8; PACKET_LIMIT];
                while !thread_stop.load(Ordering::Acquire) {
                    match socket.recv(&mut buffer) {
                        Ok(size) => {
                            if thread_stop.load(Ordering::Acquire) {
                                break;
                            }
                            match serde_json::from_slice::<SlimePacket>(&buffer[..size]) {
                                Ok(packet) if packet.version == 1 => {
                                    if let Ok(mut slot) = thread_latest.lock() {
                                        *slot = Some(packet);
                                        // Set while holding the same mutex used by
                                        // `take_latest`, avoiding a lost wakeup.
                                        thread_pending.store(true, Ordering::Release);
                                    }
                                }
                                Ok(packet) => log::debug!(
                                    "compositor: ignoring slime packet version {}",
                                    packet.version
                                ),
                                Err(err) => {
                                    log::debug!("compositor: invalid slime packet: {err}")
                                }
                            }
                        }
                        Err(err)
                            if matches!(
                                err.kind(),
                                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                            ) => {}
                        Err(err) => {
                            if !thread_stop.load(Ordering::Acquire) {
                                log::warn!(
                                    "compositor: slime IPC receive failed on {}: {err}",
                                    log_path.display()
                                );
                            }
                            break;
                        }
                    }
                }
            })?;

        log::info!("compositor: slime pose IPC listening on {}", path.display());
        Ok(Self {
            path,
            latest,
            pending,
            stop,
            receiver: Some(receiver),
        })
    }

    pub(super) fn has_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }

    fn take_latest(&self) -> Option<SlimePacket> {
        let mut slot = self.latest.lock().ok()?;
        let packet = slot.take();
        self.pending.store(false, Ordering::Release);
        packet
    }
}

impl Drop for SlimeIpc {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Wake a receiver currently sleeping in recv(). The payload is ignored
        // because the stop flag is checked immediately after wakeup.
        if let Ok(waker) = UnixDatagram::unbound() {
            let _ = waker.send_to(&[0], &self.path);
        }
        if let Some(receiver) = self.receiver.take() {
            let _ = receiver.join();
        }
        let _ = fs::remove_file(&self.path);
    }
}

struct SlimeRipple {
    center: [f32; 2],
    direction: [f32; 2],
    born: Instant,
    amplitude: f32,
}

pub(super) struct SlimeState {
    points: [f32; LANDMARK_COUNT * 2],
    bbox: [f32; 4],
    surface_rect: [f32; 4],
    scale: f32,
    strength: f32,
    last_update: Option<Instant>,
    was_visible: bool,
    initialized: bool,
    ripple_tips: [[f32; 2]; 5],
    ripple_tips_initialized: bool,
    wave_tips: [[f32; 2]; 5],
    wave_tips_initialized: bool,
    ripples: Vec<SlimeRipple>,
    pending_wave_injections: Vec<([f32; 2], [f32; 2], f32)>,
    screen_size: (f32, f32),
}

impl Default for SlimeState {
    fn default() -> Self {
        Self {
            points: [0.0; LANDMARK_COUNT * 2],
            bbox: [0.0; 4],
            surface_rect: [0.0, 0.0, 1.0, 1.0],
            scale: 48.0,
            strength: 10.0,
            last_update: None,
            was_visible: false,
            initialized: false,
            ripple_tips: [[0.0; 2]; 5],
            ripple_tips_initialized: false,
            wave_tips: [[0.0; 2]; 5],
            wave_tips_initialized: false,
            ripples: Vec::with_capacity(MAX_RIPPLES),
            pending_wave_injections: Vec::with_capacity(10),
            screen_size: (1.0, 1.0),
        }
    }
}

impl SlimeState {
    fn clear(&mut self) {
        self.last_update = None;
        self.was_visible = false;
        self.initialized = false;
        self.ripple_tips_initialized = false;
        self.wave_tips_initialized = false;
        self.ripples.clear();
        self.pending_wave_injections.clear();
    }

    pub(super) fn opacity(&self) -> f32 {
        let Some(last) = self.last_update else {
            return 0.0;
        };
        let age = last.elapsed();
        if age <= HOLD_TIME {
            1.0
        } else if age < HOLD_TIME + FADE_TIME {
            1.0 - (age - HOLD_TIME).as_secs_f32() / FADE_TIME.as_secs_f32()
        } else {
            0.0
        }
    }

    pub(super) fn is_visible(&self) -> bool {
        self.opacity() > 0.0 || self.has_live_ripples()
    }

    /// Includes the final cleanup frame after opacity reaches zero.
    pub(super) fn render_active(&self) -> bool {
        self.is_visible() || self.was_visible
    }

    pub(super) fn points(&self) -> &[f32] {
        &self.points
    }

    pub(super) fn bbox(&self) -> [f32; 4] {
        let mut bbox = if self.opacity() > 0.0 {
            self.bbox
        } else {
            [
                f32::INFINITY,
                f32::INFINITY,
                f32::NEG_INFINITY,
                f32::NEG_INFINITY,
            ]
        };
        // The persistent field keeps propagating after the hand fades, so its
        // wake can travel well beyond the original fingertip capsule.
        let expand = self.scale * 1.1 + self.strength * 2.0 + 320.0;
        for ripple in self.live_ripples() {
            bbox[0] = bbox[0].min(ripple.center[0] - expand);
            bbox[1] = bbox[1].min(ripple.center[1] - expand);
            bbox[2] = bbox[2].max(ripple.center[0] + expand);
            bbox[3] = bbox[3].max(ripple.center[1] + expand);
        }
        if !bbox[0].is_finite() {
            return [0.0; 4];
        }
        [
            bbox[0].clamp(0.0, self.screen_size.0),
            bbox[1].clamp(0.0, self.screen_size.1),
            bbox[2].clamp(0.0, self.screen_size.0),
            bbox[3].clamp(0.0, self.screen_size.1),
        ]
    }

    pub(super) fn scale(&self) -> f32 {
        self.scale
    }

    pub(super) fn surface_rect(&self) -> [f32; 4] {
        self.surface_rect
    }

    pub(super) fn strength(&self) -> f32 {
        self.strength
    }

    pub(super) fn ripple_uniforms(&self) -> ([f32; MAX_RIPPLES * 4], [f32; MAX_RIPPLES * 2], i32) {
        let mut data = [0.0; MAX_RIPPLES * 4];
        let mut directions = [0.0; MAX_RIPPLES * 2];
        let mut count = 0usize;
        for ripple in self.live_ripples().take(MAX_RIPPLES) {
            let base = count * 4;
            data[base] = ripple.center[0];
            data[base + 1] = ripple.center[1];
            data[base + 2] = (ripple.born.elapsed().as_secs_f32() / RIPPLE_LIFETIME.as_secs_f32())
                .clamp(0.0, 1.0);
            data[base + 3] = ripple.amplitude;
            directions[count * 2] = ripple.direction[0];
            directions[count * 2 + 1] = ripple.direction[1];
            count += 1;
        }
        (data, directions, count as i32)
    }

    pub(super) fn take_wave_injections(&mut self) -> ([f32; 40], [f32; 20], i32) {
        let mut segments = [0.0; 40];
        let mut params = [0.0; 20];
        let count = self.pending_wave_injections.len().min(10);
        let screen_w = self.screen_size.0.max(1.0);
        let screen_h = self.screen_size.1.max(1.0);
        for (index, (start, end, amplitude)) in
            self.pending_wave_injections.drain(..count).enumerate()
        {
            let base = index * 4;
            segments[base] = start[0] / screen_w;
            segments[base + 1] = start[1] / screen_h;
            segments[base + 2] = end[0] / screen_w;
            segments[base + 3] = end[1] / screen_h;
            params[index * 2] = self.scale * 0.055 / screen_h;
            params[index * 2 + 1] = 0.42 * (0.55 + amplitude * 1.45);
        }
        self.pending_wave_injections.clear();
        (segments, params, count as i32)
    }

    fn live_ripples(&self) -> impl Iterator<Item = &SlimeRipple> {
        self.ripples
            .iter()
            .filter(|ripple| ripple.born.elapsed() < RIPPLE_LIFETIME)
    }

    fn has_live_ripples(&self) -> bool {
        self.live_ripples().next().is_some()
    }

    fn sample_fingertips(&mut self, pose_is_continuous: bool) {
        let tips =
            FINGERTIP_INDICES.map(|index| [self.points[index * 2], self.points[index * 2 + 1]]);
        if !pose_is_continuous || !self.ripple_tips_initialized {
            self.ripple_tips = tips;
            self.ripple_tips_initialized = true;
            self.wave_tips = tips;
            self.wave_tips_initialized = true;
            return;
        }

        self.ripples
            .retain(|ripple| ripple.born.elapsed() < RIPPLE_LIFETIME);
        let sample_dt = self.last_update.map_or(1.0 / 30.0, |last| {
            last.elapsed().as_secs_f32().clamp(1.0 / 120.0, 0.10)
        });
        let wave_spacing = (self.scale * 0.008).clamp(1.2, 3.5);
        if self.wave_tips_initialized {
            for (index, current) in tips.iter().copied().enumerate() {
                let previous = self.wave_tips[index];
                let dx = current[0] - previous[0];
                let dy = current[1] - previous[1];
                let distance = (dx * dx + dy * dy).sqrt();
                if distance >= wave_spacing && self.pending_wave_injections.len() < 10 {
                    let speed = distance / sample_dt;
                    let amplitude = (speed / (self.scale * 5.0).max(1.0)).clamp(0.16, 1.0);
                    self.pending_wave_injections
                        .push((previous, current, amplitude));
                    self.wave_tips[index] = current;
                }
            }
        }
        let spacing = (self.scale * 0.13).clamp(7.0, 22.0);
        let mut emitted = Vec::with_capacity(15);
        for (index, current) in tips.into_iter().enumerate() {
            let previous = self.ripple_tips[index];
            let dx = current[0] - previous[0];
            let dy = current[1] - previous[1];
            let distance = (dx * dx + dy * dy).sqrt();
            if distance < spacing {
                continue;
            }
            let samples = ((distance / spacing).ceil() as usize).clamp(1, 3);
            let amplitude = (distance / (self.scale * 0.50).max(1.0)).clamp(0.44, 1.0);
            let direction = [dx / distance, dy / distance];
            for sample in 1..=samples {
                let t = sample as f32 / samples as f32;
                emitted.push(SlimeRipple {
                    center: [previous[0] + dx * t, previous[1] + dy * t],
                    direction,
                    born: Instant::now(),
                    amplitude,
                });
            }
            self.ripple_tips[index] = current;
        }
        for ripple in emitted {
            if self.ripples.len() == MAX_RIPPLES {
                self.ripples.remove(0);
            }
            self.ripples.push(ripple);
        }
    }

    fn mark_visibility(&mut self) -> bool {
        let visible = self.is_visible();
        let changed = visible != self.was_visible;
        self.was_visible = visible;
        changed
    }

    fn begin_fade(&mut self) {
        self.ripple_tips_initialized = false;
        self.wave_tips_initialized = false;
        let Some(last) = self.last_update else {
            return;
        };
        // Trackers commonly report an inactive pose every frame while no hand is
        // visible. Start the fade once, but do not restart it at full opacity on
        // every repeated inactive packet.
        if last.elapsed() < HOLD_TIME {
            let now = Instant::now();
            self.last_update = Some(now.checked_sub(HOLD_TIME).unwrap_or(now));
        }
    }

    fn update(
        &mut self,
        packet: SlimePacket,
        window_rect: Option<(f32, f32, f32, f32)>,
        screen_size: (f32, f32),
    ) -> bool {
        self.screen_size = screen_size;
        if packet.active == Some(false) {
            self.begin_fade();
            return true;
        }

        let Some(hand) = packet
            .hands
            .into_iter()
            .filter(|hand| hand.score.is_finite() && hand.score >= 0.25)
            .max_by(|a, b| a.score.total_cmp(&b.score))
        else {
            self.begin_fade();
            return true;
        };
        if hand.landmarks.len() != LANDMARK_COUNT {
            return false;
        }

        let (base_x, base_y, base_w, base_h) =
            window_rect.unwrap_or((0.0, 0.0, screen_size.0, screen_size.1));
        self.surface_rect = [base_x, base_y, base_x + base_w, base_y + base_h];
        let content = packet.content_rect.unwrap_or([0.0, 0.0, 1.0, 1.0]);
        if !content.iter().all(|v| v.is_finite()) || content[2] <= 0.0 || content[3] <= 0.0 {
            return false;
        }

        let mut target = [0.0f32; LANDMARK_COUNT * 2];
        for (index, landmark) in hand.landmarks.iter().enumerate() {
            if !landmark[0].is_finite() || !landmark[1].is_finite() {
                return false;
            }
            let nx = content[0] + landmark[0].clamp(-0.25, 1.25) * content[2];
            let ny = content[1] + landmark[1].clamp(-0.25, 1.25) * content[3];
            target[index * 2] = base_x + nx * base_w;
            target[index * 2 + 1] = base_y + ny * base_h;
        }

        let raw_point = |idx: usize| (target[idx * 2], target[idx * 2 + 1]);
        let distance =
            |a: (f32, f32), b: (f32, f32)| ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt();
        let palm_long = distance(raw_point(0), raw_point(9));
        let palm_wide = distance(raw_point(5), raw_point(17));
        let max_scale = (screen_size.0.min(screen_size.1).max(1.0) * 0.42).max(20.0);
        let raw_scale = palm_long.max(palm_wide * 0.9).clamp(20.0, max_scale);

        let pose_is_continuous = self.initialized
            && self
                .last_update
                .is_some_and(|last| last.elapsed() < HOLD_TIME + FADE_TIME);
        let mut max_motion = 0.0f32;
        if pose_is_continuous {
            for index in 0..LANDMARK_COUNT {
                max_motion = max_motion.max(distance(
                    (self.points[index * 2], self.points[index * 2 + 1]),
                    (target[index * 2], target[index * 2 + 1]),
                ));
            }
        }
        // Slow motion is heavily smoothed; fast gestures catch up quickly.
        let alpha = if pose_is_continuous {
            (0.34 + 0.48 * (max_motion / raw_scale.max(1.0)).clamp(0.0, 1.0)).clamp(0.34, 0.82)
        } else {
            1.0
        };
        for (current, next) in self.points.iter_mut().zip(target) {
            *current += (next - *current) * alpha;
        }
        self.scale += (raw_scale - self.scale) * alpha;

        let raw_strength = packet
            .refract_px
            .filter(|value| value.is_finite())
            .unwrap_or(raw_scale * 0.13)
            .clamp(1.0, 32.0);
        self.strength += (raw_strength - self.strength) * alpha;
        self.sample_fingertips(pose_is_continuous);

        let mut min_x = f32::INFINITY;
        let mut min_y = f32::INFINITY;
        let mut max_x = f32::NEG_INFINITY;
        let mut max_y = f32::NEG_INFINITY;
        for point in self.points.chunks_exact(2) {
            min_x = min_x.min(point[0]);
            min_y = min_y.min(point[1]);
            max_x = max_x.max(point[0]);
            max_y = max_y.max(point[1]);
        }
        let expand = self.scale * 0.62 + self.strength * 2.0;
        self.bbox = [
            (min_x - expand).clamp(0.0, screen_size.0),
            (min_y - expand).clamp(0.0, screen_size.1),
            (max_x + expand).clamp(0.0, screen_size.0),
            (max_y + expand).clamp(0.0, screen_size.1),
        ];
        self.last_update = Some(Instant::now());
        self.initialized = true;
        true
    }
}

impl<C: CompositorConnection> Compositor<C> {
    pub(crate) fn toggle_slime_effect(&mut self) -> bool {
        self.slime_effect_enabled = !self.slime_effect_enabled;
        if !self.slime_effect_enabled {
            self.slime_state.clear();
        }
        self.needs_render = true;
        self.damage_tracker.mark_all_dirty();
        self.dirty_region_tracker.mark_all_dirty();
        log::info!(
            "compositor: slime effect {}",
            if self.slime_effect_enabled {
                "enabled"
            } else {
                "disabled"
            }
        );
        self.slime_effect_enabled
    }

    pub(super) fn poll_slime_ipc(&mut self) -> bool {
        let packet = self.slime_ipc.as_ref().and_then(SlimeIpc::take_latest);
        let mut changed = false;
        if self.slime_effect_enabled {
            if let Some(packet) = packet {
                let window_rect = match packet.window {
                    Some(window) => self
                        .windows
                        .get(&window)
                        .map(|wt| (wt.x as f32, wt.y as f32, wt.w as f32, wt.h as f32)),
                    None => None,
                };
                // A packet naming an unknown/stale XID must not accidentally map to
                // the full screen. Screen coordinates are selected only by omitting
                // `window` explicitly.
                if packet.window.is_none() || window_rect.is_some() {
                    changed |= self.slime_state.update(
                        packet,
                        window_rect,
                        (self.screen_w as f32, self.screen_h as f32),
                    );
                }
            }
            changed |= self.slime_state.mark_visibility();
            if self.slime_state.is_visible() {
                self.ensure_postprocess_fbo();
            }
        }
        if changed {
            self.needs_render = true;
            self.damage_tracker.mark_all_dirty();
            self.dirty_region_tracker.mark_all_dirty();
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet_with_tip_offset(offset: f32) -> SlimePacket {
        let mut landmarks = vec![[0.5, 0.5]; LANDMARK_COUNT];
        landmarks[0] = [0.5, 0.72];
        landmarks[5] = [0.38, 0.50];
        landmarks[9] = [0.50, 0.42];
        landmarks[13] = [0.60, 0.48];
        landmarks[17] = [0.68, 0.54];
        for (tip, x) in FINGERTIP_INDICES
            .into_iter()
            .zip([0.30, 0.42, 0.52, 0.62, 0.72])
        {
            landmarks[tip] = [x + offset, 0.24];
        }
        SlimePacket {
            version: 1,
            active: Some(true),
            window: None,
            content_rect: None,
            refract_px: Some(14.0),
            hands: vec![SlimeHand {
                score: 1.0,
                landmarks,
            }],
        }
    }

    #[test]
    fn state_fades_after_stale_pose() {
        let mut state = SlimeState::default();
        state.last_update = Some(Instant::now() - HOLD_TIME - FADE_TIME - Duration::from_millis(1));
        state.was_visible = true;
        assert_eq!(state.opacity(), 0.0);
        assert!(!state.is_visible());
        assert!(state.render_active());
        assert!(state.mark_visibility());
        assert!(!state.render_active());
    }

    #[test]
    fn begin_fade_keeps_a_short_transition() {
        let mut state = SlimeState::default();
        state.last_update = Some(Instant::now());
        state.begin_fade();
        assert!(state.opacity() > 0.9);
        assert!(state.is_visible());
    }

    #[test]
    fn repeated_inactive_packets_do_not_restart_fade() {
        let mut state = SlimeState::default();
        state.last_update = Some(Instant::now());
        state.begin_fade();
        state.last_update = state
            .last_update
            .map(|last| last - Duration::from_millis(80));
        let opacity_before = state.opacity();
        state.begin_fade();
        assert!(state.opacity() <= opacity_before + 0.02);
    }

    #[test]
    fn moving_fingertips_emit_ripples_but_initial_pose_does_not() {
        let mut state = SlimeState::default();
        assert!(state.update(packet_with_tip_offset(0.0), None, (1000.0, 1000.0)));
        assert_eq!(state.ripple_uniforms().2, 0);

        assert!(state.update(packet_with_tip_offset(0.08), None, (1000.0, 1000.0)));
        let (ripples, directions, count) = state.ripple_uniforms();
        assert!(count >= 5);
        assert!(ripples[0].is_finite());
        assert!(ripples[1].is_finite());
        assert!((0.0..=1.0).contains(&ripples[2]));
        assert!((0.44..=1.0).contains(&ripples[3]));
        assert!(directions[0].abs() > 0.9);
        let (segments, params, injection_count) = state.take_wave_injections();
        assert_eq!(injection_count, 5);
        assert!(segments[..20].iter().all(|value| value.is_finite()));
        assert!(params[..10].iter().all(|value| *value > 0.0));
    }

    #[test]
    fn ripple_outlives_hand_then_expires() {
        let mut state = SlimeState::default();
        state.ripples.push(SlimeRipple {
            center: [120.0, 80.0],
            direction: [1.0, 0.0],
            born: Instant::now(),
            amplitude: 1.0,
        });
        assert!(state.is_visible());
        state.ripples[0].born = Instant::now() - RIPPLE_LIFETIME - Duration::from_millis(1);
        assert!(!state.is_visible());
    }
}
