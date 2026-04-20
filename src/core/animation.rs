// src/core/animation.rs

use crate::core::models::ClientKey;
use crate::core::types::Rect;
use std::collections::HashMap;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Easing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Easing {
    Linear,
    EaseOut,
    EaseInOut,
    EaseIn,
    Bounce,
    Elastic,
}

impl Easing {
    pub fn from_str(s: &str) -> Self {
        match s {
            "linear" => Easing::Linear,
            "ease-in-out" => Easing::EaseInOut,
            "ease-in" => Easing::EaseIn,
            "bounce" => Easing::Bounce,
            "elastic" => Easing::Elastic,
            // default
            _ => Easing::EaseOut,
        }
    }

    pub fn apply(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Easing::Linear => t,
            Easing::EaseOut => {
                let inv = 1.0 - t;
                1.0 - inv * inv * inv
            }
            Easing::EaseInOut => {
                if t < 0.5 {
                    4.0 * t * t * t
                } else {
                    let p = -2.0 * t + 2.0;
                    1.0 - p * p * p / 2.0
                }
            }
            Easing::EaseIn => t * t * t,
            Easing::Bounce => {
                let t = 1.0 - t;
                let v = if t < 1.0 / 2.75 {
                    7.5625 * t * t
                } else if t < 2.0 / 2.75 {
                    let t = t - 1.5 / 2.75;
                    7.5625 * t * t + 0.75
                } else if t < 2.5 / 2.75 {
                    let t = t - 2.25 / 2.75;
                    7.5625 * t * t + 0.9375
                } else {
                    let t = t - 2.625 / 2.75;
                    7.5625 * t * t + 0.984375
                };
                1.0 - v
            }
            Easing::Elastic => {
                if t == 0.0 || t == 1.0 {
                    t
                } else {
                    let p = 0.3;
                    let s = p / 4.0;
                    let t1 = t - 1.0;
                    -(2.0_f32.powf(10.0 * t1) * (std::f32::consts::PI * 2.0 * (t1 - s) / p).sin())
                        + 1.0
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// AnimationSpeed – preset speed modes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnimationSpeed {
    /// Very slow animations for accessibility / dramatic effect
    Slow,
    /// Default speed
    Normal,
    /// Snappy, responsive animations
    Fast,
    /// No animation delay, instant transitions
    Instant,
}

impl AnimationSpeed {
    pub fn from_str(s: &str) -> Self {
        match s {
            "slow" => AnimationSpeed::Slow,
            "fast" => AnimationSpeed::Fast,
            "instant" => AnimationSpeed::Instant,
            _ => AnimationSpeed::Normal,
        }
    }

    /// Duration multiplier applied to base duration_ms.
    pub fn duration_multiplier(self) -> f32 {
        match self {
            AnimationSpeed::Slow => 2.0,
            AnimationSpeed::Normal => 1.0,
            AnimationSpeed::Fast => 0.5,
            AnimationSpeed::Instant => 0.0,
        }
    }

    /// Fade step multiplier (higher = faster fade).
    pub fn fade_step_multiplier(self) -> f32 {
        match self {
            AnimationSpeed::Slow => 0.5,
            AnimationSpeed::Normal => 1.0,
            AnimationSpeed::Fast => 2.0,
            AnimationSpeed::Instant => 100.0,
        }
    }

    /// Effective duration in ms given a base duration.
    pub fn apply_duration(self, base_ms: u64) -> u64 {
        match self {
            AnimationSpeed::Instant => 0,
            _ => (base_ms as f32 * self.duration_multiplier()).round() as u64,
        }
    }

    /// Effective fade step given a base step.
    pub fn apply_fade_step(self, base_step: f32) -> f32 {
        (base_step * self.fade_step_multiplier()).min(1.0)
    }
}

// ---------------------------------------------------------------------------
// AnimationKind
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnimationKind {
    Layout,
    Float,
    Appear,
    Hide,
}

// ---------------------------------------------------------------------------
// ClientAnimation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ClientAnimation {
    pub from: Rect,
    pub to: Rect,
    pub started_at: Instant,
    pub duration: Duration,
    pub easing: Easing,
    pub kind: AnimationKind,
}

impl ClientAnimation {
    /// Returns (interpolated rect, is_done).
    pub fn sample(&self, now: Instant) -> (Rect, bool) {
        let elapsed = now.duration_since(self.started_at);
        if elapsed >= self.duration {
            return (self.to, true);
        }
        let t = elapsed.as_secs_f32() / self.duration.as_secs_f32();
        let e = self.easing.apply(t);
        let rect = Rect::new(
            lerp(self.from.x, self.to.x, e),
            lerp(self.from.y, self.to.y, e),
            lerp(self.from.w, self.to.w, e),
            lerp(self.from.h, self.to.h, e),
        );
        (rect, false)
    }
}

fn lerp(a: i32, b: i32, t: f32) -> i32 {
    (a as f32 + (b as f32 - a as f32) * t).round() as i32
}

// ---------------------------------------------------------------------------
// AnimationManager
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct AnimationManager {
    pub active: HashMap<ClientKey, ClientAnimation>,
}

impl AnimationManager {
    pub fn new() -> Self {
        Self {
            active: HashMap::new(),
        }
    }

    /// Start or retarget an animation for `key`.
    ///
    /// If `current_visual == target`, the animation is removed (already at goal).
    /// If an animation is already running, the current interpolated position is
    /// used as the new start point (smooth retarget).
    pub fn start(
        &mut self,
        key: ClientKey,
        current_visual: Rect,
        target: Rect,
        duration: Duration,
        easing: Easing,
        kind: AnimationKind,
    ) {
        if current_visual == target {
            self.active.remove(&key);
            return;
        }
        let anim = ClientAnimation {
            from: current_visual,
            to: target,
            started_at: Instant::now(),
            duration,
            easing,
            kind,
        };
        self.active.insert(key, anim);
    }

    pub fn remove(&mut self, key: ClientKey) {
        self.active.remove(&key);
    }

    /// Remove the animation for `key` only if it is a `Hide` animation.
    /// Layout / Appear / Float animations are preserved so that repeated
    /// `arrange()` calls don't kill in-flight layout transitions.
    pub fn remove_if_hide(&mut self, key: ClientKey) {
        if let Some(anim) = self.active.get(&key) {
            if anim.kind == AnimationKind::Hide {
                self.active.remove(&key);
            }
        }
    }

    pub fn has_active(&self) -> bool {
        !self.active.is_empty()
    }

    /// Returns the current visual (interpolated) rect for `key`, or `None` if
    /// no animation is running for that client.
    pub fn current_visual_rect(&self, key: ClientKey, now: Instant) -> Option<Rect> {
        self.active.get(&key).map(|anim| anim.sample(now).0)
    }
}

// ---------------------------------------------------------------------------
// MagneticSnap — spring-physics edge snapping
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapEdge {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Debug, Clone)]
pub struct SnapAttraction {
    pub edge: SnapEdge,
    pub target_pos: f32,
    pub current_offset: f32,
    pub velocity: f32,
}

#[derive(Debug)]
pub struct MagneticSnap {
    pub snap_distance: f32,
    pub spring_stiffness: f32,
    pub damping: f32,
    pub active_snaps: HashMap<ClientKey, SnapAttraction>,
}

impl MagneticSnap {
    pub fn new(snap_distance: f32) -> Self {
        Self {
            snap_distance,
            spring_stiffness: 300.0,
            damping: 20.0,
            active_snaps: HashMap::new(),
        }
    }

    /// Check if a window edge is close enough to a snap target.
    /// Returns the snap target position if within range.
    pub fn detect_snap(
        &self,
        window_pos: f32,
        window_size: f32,
        targets: &[f32],
    ) -> Option<(SnapEdge, f32)> {
        for &target in targets {
            let left_dist = (window_pos - target).abs();
            let right_dist = (window_pos + window_size - target).abs();

            if left_dist < self.snap_distance {
                return Some((SnapEdge::Left, target));
            }
            if right_dist < self.snap_distance {
                return Some((SnapEdge::Right, target - window_size));
            }
        }
        None
    }

    /// Start a snap attraction for a client
    pub fn start_snap(&mut self, key: ClientKey, edge: SnapEdge, target_pos: f32, current_pos: f32) {
        self.active_snaps.insert(key, SnapAttraction {
            edge,
            target_pos,
            current_offset: current_pos - target_pos,
            velocity: 0.0,
        });
    }

    /// Tick all active snap animations. Returns true if any are still active.
    /// Uses critically damped spring: F = -k*x - c*v
    pub fn tick(&mut self, dt: f32) -> bool {
        let mut done_keys = Vec::new();

        for (key, snap) in self.active_snaps.iter_mut() {
            let force = -self.spring_stiffness * snap.current_offset
                - self.damping * snap.velocity;
            snap.velocity += force * dt;
            snap.current_offset += snap.velocity * dt;

            // Settled: offset and velocity near zero
            if snap.current_offset.abs() < 0.5 && snap.velocity.abs() < 0.5 {
                snap.current_offset = 0.0;
                snap.velocity = 0.0;
                done_keys.push(*key);
            }
        }

        for key in done_keys {
            self.active_snaps.remove(&key);
        }

        !self.active_snaps.is_empty()
    }

    /// Get the snapped position for a client (target + current offset)
    pub fn snapped_position(&self, key: ClientKey) -> Option<f32> {
        self.active_snaps.get(&key).map(|s| s.target_pos + s.current_offset)
    }

    /// Remove snap for a client (e.g., when drag starts)
    pub fn cancel(&mut self, key: ClientKey) {
        self.active_snaps.remove(&key);
    }

    /// Check if any snaps are active
    pub fn is_active(&self) -> bool {
        !self.active_snaps.is_empty()
    }
}

// ---------------------------------------------------------------------------
// ElasticScroll — iOS-style overscroll bounce
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct ElasticScroll {
    pub offset: f32,
    pub velocity: f32,
    pub spring_k: f32,
    pub damping: f32,
    pub max_overscroll: f32,
    pub active: bool,
}

impl ElasticScroll {
    pub fn new() -> Self {
        Self {
            offset: 0.0,
            velocity: 0.0,
            spring_k: 400.0,
            damping: 25.0,
            max_overscroll: 100.0,
            active: false,
        }
    }

    /// Apply scroll delta. Returns the clamped overscroll offset.
    pub fn apply_scroll(&mut self, delta: f32) -> f32 {
        self.offset = (self.offset + delta).clamp(-self.max_overscroll, self.max_overscroll);
        self.velocity = delta * 10.0;
        self.active = true;
        self.offset
    }

    /// Tick the spring-back animation. Returns true if still active.
    pub fn tick(&mut self, dt: f32) -> bool {
        if !self.active {
            return false;
        }

        // Spring force toward zero
        let force = -self.spring_k * self.offset - self.damping * self.velocity;
        self.velocity += force * dt;
        self.offset += self.velocity * dt;

        if self.offset.abs() < 0.5 && self.velocity.abs() < 0.5 {
            self.offset = 0.0;
            self.velocity = 0.0;
            self.active = false;
        }

        self.active
    }

    pub fn current_offset(&self) -> f32 {
        self.offset
    }
}

impl Default for ElasticScroll {
    fn default() -> Self {
        Self::new()
    }
}
