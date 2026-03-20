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
            Easing::EaseIn => {
                t * t * t
            }
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
                    -(2.0_f32.powf(10.0 * t1)
                        * (std::f32::consts::PI * 2.0 * (t1 - s) / p).sin())
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
