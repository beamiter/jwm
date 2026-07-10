use super::{Compositor, CompositorConnection};
use serde::Deserialize;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const LANDMARK_COUNT: usize = 21;
const PACKET_LIMIT: usize = 64 * 1024;
const HOLD_TIME: Duration = Duration::from_millis(120);
const FADE_TIME: Duration = Duration::from_millis(160);

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

pub(super) struct SlimeIpc {
    socket: UnixDatagram,
    path: PathBuf,
    buffer: Vec<u8>,
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
        socket.set_nonblocking(true)?;
        log::info!("compositor: slime pose IPC listening on {}", path.display());
        Ok(Self {
            socket,
            path,
            buffer: vec![0; PACKET_LIMIT],
        })
    }

    fn recv_latest(&mut self) -> Option<SlimePacket> {
        let mut latest = None;
        loop {
            match self.socket.recv(&mut self.buffer) {
                Ok(size) => match serde_json::from_slice::<SlimePacket>(&self.buffer[..size]) {
                    Ok(packet) if packet.version == 1 => latest = Some(packet),
                    Ok(packet) => log::debug!(
                        "compositor: ignoring slime packet version {}",
                        packet.version
                    ),
                    Err(err) => log::debug!("compositor: invalid slime packet: {err}"),
                },
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => {
                    log::warn!("compositor: slime IPC receive failed: {err}");
                    break;
                }
            }
        }
        latest
    }
}

impl Drop for SlimeIpc {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub(super) struct SlimeState {
    points: [f32; LANDMARK_COUNT * 2],
    bbox: [f32; 4],
    scale: f32,
    strength: f32,
    last_update: Option<Instant>,
    was_visible: bool,
}

impl Default for SlimeState {
    fn default() -> Self {
        Self {
            points: [0.0; LANDMARK_COUNT * 2],
            bbox: [0.0; 4],
            scale: 48.0,
            strength: 10.0,
            last_update: None,
            was_visible: false,
        }
    }
}

impl SlimeState {
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
        self.opacity() > 0.0
    }

    pub(super) fn points(&self) -> &[f32] {
        &self.points
    }

    pub(super) fn bbox(&self) -> [f32; 4] {
        self.bbox
    }

    pub(super) fn scale(&self) -> f32 {
        self.scale
    }

    pub(super) fn strength(&self) -> f32 {
        self.strength
    }

    fn mark_visibility(&mut self) -> bool {
        let visible = self.is_visible();
        let changed = visible != self.was_visible;
        self.was_visible = visible;
        changed
    }

    fn clear(&mut self) {
        self.last_update = None;
    }

    fn update(
        &mut self,
        packet: SlimePacket,
        window_rect: Option<(f32, f32, f32, f32)>,
        screen_size: (f32, f32),
    ) -> bool {
        if packet.active == Some(false) {
            self.clear();
            return true;
        }

        let Some(hand) = packet
            .hands
            .into_iter()
            .filter(|hand| hand.score.is_finite() && hand.score >= 0.25)
            .max_by(|a, b| a.score.total_cmp(&b.score))
        else {
            return false;
        };
        if hand.landmarks.len() != LANDMARK_COUNT {
            return false;
        }

        let (base_x, base_y, base_w, base_h) = window_rect.unwrap_or((
            0.0,
            0.0,
            screen_size.0,
            screen_size.1,
        ));
        let content = packet.content_rect.unwrap_or([0.0, 0.0, 1.0, 1.0]);
        if !content.iter().all(|v| v.is_finite()) || content[2] <= 0.0 || content[3] <= 0.0 {
            return false;
        }

        let mut min_x = f32::INFINITY;
        let mut min_y = f32::INFINITY;
        let mut max_x = f32::NEG_INFINITY;
        let mut max_y = f32::NEG_INFINITY;
        for (index, landmark) in hand.landmarks.iter().enumerate() {
            if !landmark[0].is_finite() || !landmark[1].is_finite() {
                return false;
            }
            let nx = content[0] + landmark[0].clamp(-0.25, 1.25) * content[2];
            let ny = content[1] + landmark[1].clamp(-0.25, 1.25) * content[3];
            let x = base_x + nx * base_w;
            let y = base_y + ny * base_h;
            self.points[index * 2] = x;
            self.points[index * 2 + 1] = y;
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }

        let point = |idx: usize| (self.points[idx * 2], self.points[idx * 2 + 1]);
        let distance = |a: (f32, f32), b: (f32, f32)| {
            ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
        };
        let palm_long = distance(point(0), point(9));
        let palm_wide = distance(point(5), point(17));
        let max_scale = (screen_size.0.min(screen_size.1).max(1.0) * 0.42).max(20.0);
        self.scale = palm_long.max(palm_wide * 0.9).clamp(20.0, max_scale);
        self.strength = packet
            .refract_px
            .filter(|value| value.is_finite())
            .unwrap_or(self.scale * 0.13)
            .clamp(1.0, 32.0);

        let expand = self.scale * 0.55;
        self.bbox = [
            (min_x - expand).clamp(0.0, screen_size.0),
            (min_y - expand).clamp(0.0, screen_size.1),
            (max_x + expand).clamp(0.0, screen_size.0),
            (max_y + expand).clamp(0.0, screen_size.1),
        ];
        self.last_update = Some(Instant::now());
        true
    }
}

impl<C: CompositorConnection> Compositor<C> {
    pub(super) fn poll_slime_ipc(&mut self) -> bool {
        let packet = self.slime_ipc.as_mut().and_then(SlimeIpc::recv_latest);
        let mut changed = false;
        if let Some(packet) = packet {
            let window_rect = match packet.window {
                Some(window) => self.windows.get(&window).map(|wt| {
                    (wt.x as f32, wt.y as f32, wt.w as f32, wt.h as f32)
                }),
                None => None,
            };
            // A packet naming an unknown/stale XID must not accidentally map to
            // the full screen. Screen coordinates are only selected explicitly
            // by omitting `window`.
            if packet.window.is_none() || window_rect.is_some() {
                changed |= self.slime_state.update(
                    packet,
                    window_rect,
                    (self.screen_w as f32, self.screen_h as f32),
                );
            }
        }
        changed |= self.slime_state.mark_visibility();
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

    #[test]
    fn state_fades_after_stale_pose() {
        let mut state = SlimeState::default();
        state.last_update = Some(Instant::now() - HOLD_TIME - FADE_TIME - Duration::from_millis(1));
        assert_eq!(state.opacity(), 0.0);
        assert!(!state.is_visible());
    }
}
