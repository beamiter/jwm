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

#[cfg(test)]
mod tests {
    use super::*;
    use slotmap::SlotMap;
    use std::time::{Duration, Instant};

    fn make_key() -> ClientKey {
        let mut sm: SlotMap<ClientKey, ()> = SlotMap::new();
        sm.insert(())
    }

    fn make_two_keys() -> (ClientKey, ClientKey) {
        let mut sm: SlotMap<ClientKey, ()> = SlotMap::new();
        let k1 = sm.insert(());
        let k2 = sm.insert(());
        (k1, k2)
    }

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect::new(x, y, w, h)
    }

    // -----------------------------------------------------------------------
    // Easing::from_str
    // -----------------------------------------------------------------------

    #[test]
    fn test_easing_from_str_variants() {
        assert_eq!(Easing::from_str("linear"), Easing::Linear);
        assert_eq!(Easing::from_str("ease-in-out"), Easing::EaseInOut);
        assert_eq!(Easing::from_str("ease-in"), Easing::EaseIn);
        assert_eq!(Easing::from_str("bounce"), Easing::Bounce);
        assert_eq!(Easing::from_str("elastic"), Easing::Elastic);
    }

    #[test]
    fn test_easing_from_str_default_is_ease_out() {
        assert_eq!(Easing::from_str("unknown"), Easing::EaseOut);
        assert_eq!(Easing::from_str(""), Easing::EaseOut);
    }

    // -----------------------------------------------------------------------
    // Easing::apply — boundary invariants
    // -----------------------------------------------------------------------

    fn easing_boundaries(e: Easing) {
        let at0 = e.apply(0.0);
        let at1 = e.apply(1.0);
        assert!(
            at0.abs() < 1e-4,
            "{e:?}.apply(0.0) = {at0}, expected ~0"
        );
        assert!(
            (at1 - 1.0).abs() < 1e-4,
            "{e:?}.apply(1.0) = {at1}, expected ~1"
        );
    }

    #[test]
    fn test_easing_boundaries_all_variants() {
        for e in [
            Easing::Linear,
            Easing::EaseOut,
            Easing::EaseInOut,
            Easing::EaseIn,
            Easing::Bounce,
            Easing::Elastic,
        ] {
            easing_boundaries(e);
        }
    }

    #[test]
    fn test_easing_clamps_out_of_range() {
        // Values outside [0,1] should be clamped
        assert!((Easing::Linear.apply(-1.0) - 0.0).abs() < 1e-4);
        assert!((Easing::Linear.apply(2.0) - 1.0).abs() < 1e-4);
    }

    #[test]
    fn test_easing_linear_midpoint() {
        assert!((Easing::Linear.apply(0.5) - 0.5).abs() < 1e-4);
    }

    #[test]
    fn test_easing_ease_in_accelerates() {
        // EaseIn: slow start → t³, so apply(0.25) < 0.25
        let val = Easing::EaseIn.apply(0.25);
        assert!(val < 0.25, "EaseIn at 0.25 should be < 0.25, got {val}");
    }

    #[test]
    fn test_easing_ease_out_decelerates() {
        // EaseOut: fast start, so apply(0.25) > 0.25
        let val = Easing::EaseOut.apply(0.25);
        assert!(val > 0.25, "EaseOut at 0.25 should be > 0.25, got {val}");
    }

    #[test]
    fn test_easing_output_always_in_0_1_range() {
        // For Bounce/Elastic, output may dip slightly but overall stays near [0,1]
        for t in [0.0f32, 0.1, 0.2, 0.5, 0.8, 0.9, 1.0] {
            for e in [Easing::Linear, Easing::EaseOut, Easing::EaseInOut, Easing::EaseIn] {
                let v = e.apply(t);
                assert!(v >= -0.01 && v <= 1.01, "{e:?}.apply({t}) = {v} out of range");
            }
        }
    }

    // -----------------------------------------------------------------------
    // AnimationSpeed
    // -----------------------------------------------------------------------

    #[test]
    fn test_animation_speed_from_str() {
        assert_eq!(AnimationSpeed::from_str("slow"), AnimationSpeed::Slow);
        assert_eq!(AnimationSpeed::from_str("fast"), AnimationSpeed::Fast);
        assert_eq!(AnimationSpeed::from_str("instant"), AnimationSpeed::Instant);
        assert_eq!(AnimationSpeed::from_str("normal"), AnimationSpeed::Normal);
        assert_eq!(AnimationSpeed::from_str("unknown"), AnimationSpeed::Normal);
    }

    #[test]
    fn test_animation_speed_duration_multiplier() {
        assert!((AnimationSpeed::Slow.duration_multiplier() - 2.0).abs() < 1e-4);
        assert!((AnimationSpeed::Normal.duration_multiplier() - 1.0).abs() < 1e-4);
        assert!((AnimationSpeed::Fast.duration_multiplier() - 0.5).abs() < 1e-4);
        assert!((AnimationSpeed::Instant.duration_multiplier() - 0.0).abs() < 1e-4);
    }

    #[test]
    fn test_animation_speed_apply_duration_instant_is_zero() {
        assert_eq!(AnimationSpeed::Instant.apply_duration(300), 0);
    }

    #[test]
    fn test_animation_speed_apply_duration_slow() {
        assert_eq!(AnimationSpeed::Slow.apply_duration(150), 300);
    }

    #[test]
    fn test_animation_speed_apply_duration_fast() {
        assert_eq!(AnimationSpeed::Fast.apply_duration(200), 100);
    }

    #[test]
    fn test_animation_speed_apply_fade_step_clamps_at_1() {
        // Instant multiplier = 100.0, so 0.05 * 100.0 = 5.0 → clamped to 1.0
        let step = AnimationSpeed::Instant.apply_fade_step(0.05);
        assert!((step - 1.0).abs() < 1e-4);
    }

    #[test]
    fn test_animation_speed_apply_fade_step_normal() {
        let step = AnimationSpeed::Normal.apply_fade_step(0.1);
        assert!((step - 0.1).abs() < 1e-4);
    }

    // -----------------------------------------------------------------------
    // AnimationManager
    // -----------------------------------------------------------------------

    #[test]
    fn test_animation_manager_starts_empty() {
        let mgr = AnimationManager::new();
        assert!(!mgr.has_active());
    }

    #[test]
    fn test_animation_manager_start_adds_animation() {
        let mut mgr = AnimationManager::new();
        let key = make_key();
        mgr.start(
            key,
            rect(0, 0, 100, 100),
            rect(200, 200, 100, 100),
            Duration::from_millis(300),
            Easing::EaseOut,
            AnimationKind::Layout,
        );
        assert!(mgr.has_active());
    }

    #[test]
    fn test_animation_manager_start_same_from_to_removes() {
        let mut mgr = AnimationManager::new();
        let key = make_key();
        // Insert first
        mgr.start(key, rect(0,0,100,100), rect(200,200,100,100),
            Duration::from_millis(300), Easing::Linear, AnimationKind::Layout);
        assert!(mgr.has_active());
        // Same from == to → should remove
        mgr.start(key, rect(200,200,100,100), rect(200,200,100,100),
            Duration::from_millis(300), Easing::Linear, AnimationKind::Layout);
        assert!(!mgr.has_active());
    }

    #[test]
    fn test_animation_manager_remove() {
        let mut mgr = AnimationManager::new();
        let key = make_key();
        mgr.start(key, rect(0,0,100,100), rect(200,0,100,100),
            Duration::from_millis(200), Easing::Linear, AnimationKind::Layout);
        mgr.remove(key);
        assert!(!mgr.has_active());
    }

    #[test]
    fn test_animation_manager_remove_if_hide_only_removes_hide() {
        let mut mgr = AnimationManager::new();
        let (k1, k2) = make_two_keys();
        mgr.start(k1, rect(0,0,100,100), rect(200,0,100,100),
            Duration::from_millis(200), Easing::Linear, AnimationKind::Layout);
        mgr.start(k2, rect(0,0,100,100), rect(200,0,100,100),
            Duration::from_millis(200), Easing::Linear, AnimationKind::Hide);
        mgr.remove_if_hide(k1); // should NOT remove Layout
        mgr.remove_if_hide(k2); // should remove Hide
        assert!(mgr.active.contains_key(&k1), "Layout animation should remain");
        assert!(!mgr.active.contains_key(&k2), "Hide animation should be removed");
    }

    #[test]
    fn test_animation_manager_current_visual_rect_returns_none_without_anim() {
        let mgr = AnimationManager::new();
        let key = make_key();
        assert!(mgr.current_visual_rect(key, Instant::now()).is_none());
    }

    #[test]
    fn test_animation_manager_current_visual_rect_at_start() {
        let mut mgr = AnimationManager::new();
        let key = make_key();
        let from = rect(0, 0, 100, 100);
        let to = rect(200, 0, 100, 100);
        mgr.start(key, from, to, Duration::from_millis(500), Easing::Linear, AnimationKind::Layout);
        // Immediately after start, visual should be near `from`
        let visual = mgr.current_visual_rect(key, Instant::now()).unwrap();
        assert!((visual.x - from.x).abs() <= 5);
    }

    #[test]
    fn test_client_animation_sample_at_end() {
        let anim = ClientAnimation {
            from: rect(0, 0, 100, 100),
            to: rect(200, 0, 100, 100),
            started_at: Instant::now() - Duration::from_secs(10),
            duration: Duration::from_millis(300),
            easing: Easing::Linear,
            kind: AnimationKind::Layout,
        };
        let (r, done) = anim.sample(Instant::now());
        assert!(done);
        assert_eq!(r, rect(200, 0, 100, 100));
    }

    // -----------------------------------------------------------------------
    // MagneticSnap
    // -----------------------------------------------------------------------

    #[test]
    fn test_magnetic_snap_detect_snap_within_range() {
        let snap = MagneticSnap::new(20.0);
        let result = snap.detect_snap(5.0, 100.0, &[0.0, 200.0]);
        assert!(result.is_some(), "Window edge at 5 should snap to 0");
        let (edge, target) = result.unwrap();
        assert_eq!(edge, SnapEdge::Left);
        assert!((target - 0.0).abs() < 1e-4);
    }

    #[test]
    fn test_magnetic_snap_detect_snap_outside_range() {
        let snap = MagneticSnap::new(10.0);
        let result = snap.detect_snap(50.0, 100.0, &[0.0]);
        assert!(result.is_none(), "Window edge at 50 is outside snap range 10");
    }

    #[test]
    fn test_magnetic_snap_detect_right_edge() {
        let snap = MagneticSnap::new(20.0);
        // window at x=181, w=100 → right edge at 281, target=300, dist=19 < 20
        let result = snap.detect_snap(181.0, 100.0, &[300.0]);
        assert!(result.is_some(), "Right edge at 281 should snap to 300 (dist=19 < 20)");
        let (edge, _) = result.unwrap();
        assert_eq!(edge, SnapEdge::Right);
    }

    #[test]
    fn test_magnetic_snap_detect_no_targets() {
        let snap = MagneticSnap::new(20.0);
        let result = snap.detect_snap(0.0, 100.0, &[]);
        assert!(result.is_none());
    }

    #[test]
    fn test_magnetic_snap_cancel() {
        let mut snap = MagneticSnap::new(20.0);
        let key = make_key();
        snap.start_snap(key, SnapEdge::Left, 0.0, 5.0);
        assert!(snap.is_active());
        snap.cancel(key);
        assert!(!snap.is_active());
    }

    #[test]
    fn test_magnetic_snap_tick_converges() {
        let mut snap = MagneticSnap::new(20.0);
        let key = make_key();
        snap.start_snap(key, SnapEdge::Left, 0.0, 10.0);
        // Tick many times until settled
        for _ in 0..1000 {
            let still_active = snap.tick(0.016);
            if !still_active {
                break;
            }
        }
        assert!(!snap.is_active(), "Snap should have settled");
    }

    #[test]
    fn test_magnetic_snap_snapped_position_before_tick() {
        let mut snap = MagneticSnap::new(20.0);
        let key = make_key();
        snap.start_snap(key, SnapEdge::Left, 100.0, 110.0);
        let pos = snap.snapped_position(key);
        assert!(pos.is_some());
        // Initial: target (100) + offset (10) = 110
        assert!((pos.unwrap() - 110.0).abs() < 1e-3);
    }

    // -----------------------------------------------------------------------
    // ElasticScroll
    // -----------------------------------------------------------------------

    #[test]
    fn test_elastic_scroll_initial_state() {
        let s = ElasticScroll::new();
        assert!((s.current_offset() - 0.0).abs() < 1e-4);
        assert!(!s.active);
    }

    #[test]
    fn test_elastic_scroll_apply_scroll() {
        let mut s = ElasticScroll::new();
        let offset = s.apply_scroll(30.0);
        assert!(offset > 0.0);
        assert!(s.active);
    }

    #[test]
    fn test_elastic_scroll_clamped_at_max() {
        let mut s = ElasticScroll::new();
        // max_overscroll = 100
        s.apply_scroll(200.0);
        assert!((s.current_offset() - 100.0).abs() < 1e-4);
    }

    #[test]
    fn test_elastic_scroll_springs_back_to_zero() {
        let mut s = ElasticScroll::new();
        s.apply_scroll(50.0);
        for _ in 0..2000 {
            if !s.tick(0.016) {
                break;
            }
        }
        assert!(!s.active, "Elastic scroll should have settled");
        assert!(s.current_offset().abs() < 1e-4);
    }

    #[test]
    fn test_elastic_scroll_tick_inactive_returns_false() {
        let mut s = ElasticScroll::new();
        assert!(!s.tick(0.016));
    }

    #[test]
    fn test_elastic_scroll_default_equals_new() {
        let s1 = ElasticScroll::new();
        let s2 = ElasticScroll::default();
        assert!((s1.offset - s2.offset).abs() < 1e-6);
        assert_eq!(s1.active, s2.active);
    }
}
