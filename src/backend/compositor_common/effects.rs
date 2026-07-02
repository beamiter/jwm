/// Single particle for close animation.
pub struct Particle {
    pub x: f32,
    pub y: f32,
    pub vx: f32,
    pub vy: f32,
    pub color: [f32; 4],
    pub lifetime: f32,
    pub max_lifetime: f32,
}

/// Active particle system (one per closing window).
pub struct ParticleSystem {
    pub particles: Vec<Particle>,
}

/// Active window-open ripple state for one window.
pub struct RippleState {
    pub x11_win: u32,
    pub start: std::time::Instant,
}
