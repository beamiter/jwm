use super::types::WindowTexture;
use super::{Compositor, Particle};
use crate::backend::compositor_common::effects::{
    MAX_PARTICLE_SYSTEMS, MotionTrailParams, clamp_effect_dt, effect_noise, motion_trail_lifetime,
    particle_burst_count, sanitize_animation_dt, smoothing_alpha,
};
use crate::backend::compositor_common::genie::{
    GenieDirection, PreviewDirection, genie_progress, preview_motion, retarget_genie_timeline,
};
use crate::backend::compositor_common::minimized_thumbnail::{
    AdmissionOutcome, IconicSnapshotReservationError, MinimizedSnapshotCache, SnapshotGeneration,
    SnapshotRecaptureGate, SnapshotRetention,
};
use glow::HasContext;

use super::CompositorConnection;

/// Clock for effects whose state is advanced by a frame delta.
///
/// `FrameStats::last_frame_time` measures time since the previous completed
/// compositor frame.  That interval can include an arbitrarily long idle
/// period before a new window starts fading or a close burst is spawned.  A
/// newly-active effect must start with a zero delta and only accumulate time
/// while incremental effects remain active.
#[derive(Default)]
pub(super) struct EffectTickClock {
    last_tick: Option<std::time::Instant>,
}

impl EffectTickClock {
    pub(super) fn reset(&mut self) {
        self.last_tick = None;
    }

    pub(super) fn delta(&mut self, now: std::time::Instant, active: bool) -> f32 {
        if !active {
            self.reset();
            return 0.0;
        }

        self.last_tick
            .replace(now)
            .map(|last| now.saturating_duration_since(last).as_secs_f32())
            .unwrap_or(0.0)
    }

    pub(super) fn finish_frame(&mut self, now: std::time::Instant, still_active: bool) {
        if still_active {
            // Some effects (notably pointer-driven tilt) discover their target
            // during drawing, after `delta` sampled an inactive clock. Prime
            // the clock here so their next frame receives a non-zero delta.
            self.last_tick.get_or_insert(now);
        } else {
            self.reset();
        }
    }
}

/// Per-frame result of `tick_fades`.
///
/// `any` keeps the render loop pacing frames while anything is still fading —
/// every animated pixel needs a redraw, whoever owns it. `on_clients` only
/// reports animation on windows the WM manages: override-redirect overlays
/// (fcitx5's candidate list and input-method switcher, menus, tooltips, drag
/// icons) fade in and out on every keystroke, and letting those fades feed
/// the adaptive blur-quality downgrade made every frosted client visibly pump
/// between Full and Reduced blur for as long as a user typed Chinese.
#[derive(Clone, Copy, Default)]
pub(super) struct FadeTick {
    pub(super) any: bool,
    pub(super) on_clients: bool,
}

impl FadeTick {
    /// Fold one animating window into the frame-wide flags.
    pub(super) fn record(&mut self, is_override_redirect: bool) {
        self.any = true;
        if !is_override_redirect {
            self.on_clients = true;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LateMinimizedWindowDisposition {
    AwaitLiveTexture,
    CacheLiveTexture,
    ReleaseDuplicateLiveTexture,
}

const fn late_minimized_window_disposition(
    has_live_texture: bool,
    has_retained_pixels: bool,
) -> LateMinimizedWindowDisposition {
    if !has_live_texture {
        LateMinimizedWindowDisposition::AwaitLiveTexture
    } else if has_retained_pixels {
        LateMinimizedWindowDisposition::ReleaseDuplicateLiveTexture
    } else {
        LateMinimizedWindowDisposition::CacheLiveTexture
    }
}

fn late_minimized_visual_dimensions(width: u32, height: u32) -> Option<(f32, f32)> {
    (width > 0 && height > 0).then_some((width as f32, height as f32))
}

fn touch_retained_visual(
    cached_at: Option<&mut std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    let Some(cached_at) = cached_at else {
        return false;
    };
    *cached_at = now;
    true
}

fn retained_lru_candidate<K: Copy + Eq>(
    entries: impl IntoIterator<Item = (K, std::time::Instant)>,
    protected: K,
) -> Option<K> {
    entries
        .into_iter()
        .filter(|(window, _)| *window != protected)
        .min_by_key(|(_, cached_at)| *cached_at)
        .map(|(window, _)| window)
}

fn gpu_snapshot_lru_candidate<K: Copy + Eq>(
    entries: impl IntoIterator<Item = (K, u64)>,
    protected: K,
) -> Option<K> {
    entries
        .into_iter()
        .filter(|(window, _)| *window != protected)
        .min_by_key(|(_, last_use)| *last_use)
        .map(|(window, _)| window)
}

fn minimized_capture_source_dimensions(width: f32, height: f32) -> Option<(u32, u32)> {
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return None;
    }
    Some((
        width.round().clamp(1.0, u32::MAX as f32) as u32,
        height.round().clamp(1.0, u32::MAX as f32) as u32,
    ))
}

const fn preview_loses_source_after_full_eviction(has_cpu: bool, has_gpu: bool) -> bool {
    !has_cpu && !has_gpu
}

/// Consume one explicitly armed CPU-to-GPU promotion attempt.  CPU pixels by
/// themselves are not drawable by the GL renderer; importantly, a failed
/// attempt is still consumed and must be rearmed by a later Dock request.
fn consume_minimized_gpu_upload_request(
    pending: &mut std::collections::HashSet<u32>,
    x11_win: u32,
    has_cpu: bool,
    has_gpu: bool,
) -> bool {
    if has_gpu {
        pending.remove(&x11_win);
        return false;
    }
    has_cpu && pending.remove(&x11_win)
}

/// Consume a retained-source readback epoch only while the compositor has a
/// successfully-current graphics context. A failed/deferred `make_current`
/// therefore leaves the demand intact for the next render attempt.
fn begin_retained_recapture_attempt(
    gate: &mut SnapshotRecaptureGate,
    capacity_epoch: u64,
    context_current: bool,
    has_retained_source: bool,
) -> bool {
    context_current && has_retained_source && gate.begin_attempt(capacity_epoch)
}

/// Keep the authoritative generation map and the cache mutation in one small,
/// resource-free transaction. A retained full-size visual is deliberately not
/// an input: only bounded CPU pixels can authorize the later X unmap.
fn reserve_current_iconic_snapshot(
    generations: &std::collections::HashMap<u32, SnapshotGeneration>,
    snapshots: &mut MinimizedSnapshotCache<u32>,
    x11_win: u32,
) -> Result<SnapshotGeneration, IconicSnapshotReservationError> {
    let Some(generation) = generations.get(&x11_win).copied() else {
        return Err(IconicSnapshotReservationError::NoSnapshot);
    };
    snapshots.reserve_iconic_snapshot(&x11_win, generation)
}

fn release_iconic_snapshot_token(
    snapshots: &mut MinimizedSnapshotCache<u32>,
    x11_win: u32,
    generation: u64,
) -> bool {
    let Some(generation) = SnapshotGeneration::new(generation) else {
        return false;
    };
    snapshots.release_iconic_snapshot_reservation(&x11_win, generation)
}

fn has_iconic_snapshot_token(
    snapshots: &MinimizedSnapshotCache<u32>,
    x11_win: u32,
    generation: u64,
) -> bool {
    let Some(generation) = SnapshotGeneration::new(generation) else {
        return false;
    };
    snapshots.has_iconic_snapshot_reservation(&x11_win, generation)
}

pub(super) fn discard_minimized_cpu_snapshot_state(
    pending_uploads: &mut std::collections::HashSet<u32>,
    snapshots: &mut MinimizedSnapshotCache<u32>,
    generations: &mut std::collections::HashMap<u32, SnapshotGeneration>,
    x11_win: u32,
) -> bool {
    pending_uploads.remove(&x11_win);
    let removed_cpu = snapshots.remove(&x11_win).is_some();
    let removed_generation = generations.remove(&x11_win).is_some();
    removed_cpu || removed_generation
}

pub(super) fn tilt_animation_pending(
    enabled: bool,
    current_x: f32,
    current_y: f32,
    target_x: f32,
    target_y: f32,
) -> bool {
    enabled && ((current_x - target_x).abs() > 0.0001 || (current_y - target_y).abs() > 0.0001)
}

const fn completed_genie_may_settle(done_at_frame_sample: bool, frame_presented: bool) -> bool {
    done_at_frame_sample && frame_presented
}

impl<C: CompositorConnection> Compositor<C> {
    pub(super) fn ensure_minimized_snapshot_generation(
        &mut self,
        x11_win: u32,
    ) -> crate::backend::compositor_common::minimized_thumbnail::SnapshotGeneration {
        if let Some(generation) = self.minimized_snapshot_generations.get(&x11_win) {
            return *generation;
        }
        let generation =
            super::next_snapshot_generation(&mut self.minimized_snapshot_generation_clock);
        self.minimized_snapshot_generations
            .insert(x11_win, generation);
        generation
    }

    /// Arm one explicit true-Iconic CPU recapture demand. The demand survives
    /// a capacity rejection so a later pin release can unlock one retry, while
    /// its epoch prevents ordinary admission-service frames from reading back
    /// the retained texture repeatedly.
    pub(crate) fn request_iconic_snapshot_recapture(&mut self, x11_win: u32) {
        self.iconic_snapshot_recapture_gates
            .entry(x11_win)
            .or_default()
            .request();
        // Wake one render attempt. The epoch gate, not this boolean, decides
        // whether a current-context readback is still due.
        self.needs_render = true;
    }

    pub(super) fn iconic_snapshot_recapture_due(&self, x11_win: u32) -> bool {
        let capacity_epoch = self.minimized_snapshots.capacity_epoch();
        self.iconic_snapshot_recapture_gates
            .get(&x11_win)
            .is_some_and(|gate| gate.is_due(capacity_epoch))
    }

    fn begin_iconic_snapshot_recapture(&mut self, x11_win: u32) -> bool {
        let capacity_epoch = self.minimized_snapshots.capacity_epoch();
        let context_current = self.context_current;
        self.iconic_snapshot_recapture_gates
            .get_mut(&x11_win)
            .is_some_and(|gate| {
                begin_retained_recapture_attempt(gate, capacity_epoch, context_current, true)
            })
    }

    /// Account an ordinary minimize/static-import capture against a currently
    /// armed demand too. Otherwise a capacity rejection in that first capture
    /// would be followed immediately by a duplicate retained-source readback.
    fn note_iconic_snapshot_capture_attempt(&mut self, x11_win: u32) {
        let capacity_epoch = self.minimized_snapshots.capacity_epoch();
        if let Some(gate) = self.iconic_snapshot_recapture_gates.get_mut(&x11_win) {
            let _ = gate.begin_attempt(capacity_epoch);
        }
    }

    pub(super) fn clear_iconic_snapshot_recapture(&mut self, x11_win: u32) {
        self.iconic_snapshot_recapture_gates.remove(&x11_win);
    }

    fn iconic_snapshot_retained_source(
        &self,
        x11_win: u32,
    ) -> Option<(glow::Texture, u32, u32, bool)> {
        self.genie_active
            .iter()
            .find(|animation| animation.x11_win == x11_win)
            .and_then(|animation| {
                minimized_capture_source_dimensions(animation.w, animation.h).map(
                    |(width, height)| (animation.gl_texture, width, height, animation.has_rgba),
                )
            })
            .or_else(|| {
                self.minimized_visuals.get(&x11_win).and_then(|visual| {
                    minimized_capture_source_dimensions(visual.w, visual.h)
                        .map(|(width, height)| (visual.gl_texture, width, height, visual.has_rgba))
                })
            })
    }

    /// A render wakeup/fullscreen-unredirect blocker exists only while an
    /// actual retained source can satisfy a due demand. Missing sources do not
    /// create a polling loop; their eventual insertion already requests render.
    pub(super) fn iconic_snapshot_recapture_pending(&self) -> bool {
        let capacity_epoch = self.minimized_snapshots.capacity_epoch();
        self.iconic_snapshot_recapture_gates
            .iter()
            .any(|(&x11_win, gate)| {
                gate.is_due(capacity_epoch)
                    && !self.current_minimized_cpu_snapshot_available(x11_win)
                    && self.iconic_snapshot_retained_source(x11_win).is_some()
            })
    }

    /// Service every retained-source recapture only after `render_frame` has
    /// successfully made the compositor context current. A failed readback
    /// consumes its exact demand/capacity tuple; no ordinary frame retries it.
    pub(super) fn service_iconic_snapshot_recaptures_current_context(&mut self) -> usize {
        if !self.context_current {
            self.needs_render |= self.iconic_snapshot_recapture_pending();
            return 0;
        }

        let mut windows = self
            .iconic_snapshot_recapture_gates
            .keys()
            .copied()
            .collect::<Vec<_>>();
        windows.sort_unstable();
        let mut attempts = 0;
        for x11_win in windows {
            if self.current_minimized_cpu_snapshot_available(x11_win) {
                self.clear_iconic_snapshot_recapture(x11_win);
                continue;
            }
            let Some((texture, width, height, has_alpha)) =
                self.iconic_snapshot_retained_source(x11_win)
            else {
                continue;
            };
            if !self.begin_iconic_snapshot_recapture(x11_win) {
                continue;
            }
            attempts += 1;
            self.cache_minimized_snapshot_from_texture(x11_win, texture, width, height, has_alpha);
        }
        attempts
    }

    /// Reserve the current CPU snapshot before a managed X11 client is
    /// actually unmapped. The returned epoch is the token carried through the
    /// true-Iconic transaction and its eventual acknowledgement/cancellation.
    pub(crate) fn reserve_iconic_snapshot(
        &mut self,
        x11_win: u32,
    ) -> Result<u64, IconicSnapshotReservationError> {
        let reservation = reserve_current_iconic_snapshot(
            &self.minimized_snapshot_generations,
            &mut self.minimized_snapshots,
            x11_win,
        );
        if reservation.is_ok() {
            self.clear_iconic_snapshot_recapture(x11_win);
        }
        reservation.map(SnapshotGeneration::get)
    }

    /// Cancel/unpin exactly one reservation without discarding either cache
    /// tier. Zero and stale generation tokens are harmless no-ops.
    pub(crate) fn release_iconic_snapshot_reservation(
        &mut self,
        x11_win: u32,
        generation: u64,
    ) -> bool {
        release_iconic_snapshot_token(&mut self.minimized_snapshots, x11_win, generation)
    }

    /// Query the exact CPU reservation; a full-resolution retained visual or
    /// a GPU-only mirror can never satisfy this durability check.
    pub(crate) fn has_iconic_snapshot_reservation(&self, x11_win: u32, generation: u64) -> bool {
        has_iconic_snapshot_token(&self.minimized_snapshots, x11_win, generation)
    }

    fn next_minimized_gpu_use(&mut self) -> u64 {
        self.minimized_gpu_use_clock = self.minimized_gpu_use_clock.saturating_add(1);
        self.minimized_gpu_use_clock
    }

    /// Arm at most one lazy upload for an explicit Dock consumer.  Merely
    /// retaining a CPU snapshot never arms work, otherwise an idle fullscreen
    /// scene would retry a failed allocation on every compositor frame.
    pub(super) fn arm_minimized_gpu_upload(&mut self, x11_win: u32) -> bool {
        if self.current_minimized_cpu_snapshot_available(x11_win)
            && !self.current_minimized_gpu_snapshot_available(x11_win)
        {
            let armed = self.pending_minimized_gpu_uploads.insert(x11_win);
            self.needs_render |= armed;
            armed
        } else {
            self.pending_minimized_gpu_uploads.remove(&x11_win);
            false
        }
    }

    pub(super) fn minimized_gpu_upload_pending(&self, x11_win: u32) -> bool {
        self.pending_minimized_gpu_uploads.contains(&x11_win)
            && self.current_minimized_cpu_snapshot_available(x11_win)
            && !self.current_minimized_gpu_snapshot_available(x11_win)
    }

    pub(super) fn consume_minimized_gpu_upload(&mut self, x11_win: u32) -> bool {
        let has_cpu = self.current_minimized_cpu_snapshot_available(x11_win);
        let has_gpu = self.current_minimized_gpu_snapshot_available(x11_win);
        consume_minimized_gpu_upload_request(
            &mut self.pending_minimized_gpu_uploads,
            x11_win,
            has_cpu,
            has_gpu,
        )
    }

    /// Service every currently armed promotion after the GL context becomes
    /// current.  Doing this before overlay opacity/geometry early returns
    /// guarantees that even an awaiting hover with no realized static card
    /// consumes its single attempt instead of pinning composition forever.
    pub(super) fn service_minimized_gpu_uploads(&mut self) {
        let pending: Vec<_> = self.pending_minimized_gpu_uploads.iter().copied().collect();
        for x11_win in pending {
            if self.consume_minimized_gpu_upload(x11_win)
                && self.ensure_minimized_gpu_snapshot(x11_win)
            {
                self.resume_minimized_preview_after_capture(x11_win);
            }
        }
    }

    pub(super) fn remove_minimized_gpu_snapshot(&mut self, x11_win: u32) -> bool {
        let Some(snapshot) = self.minimized_gpu_snapshots.remove(&x11_win) else {
            return false;
        };
        unsafe {
            self.gl.delete_texture(snapshot.texture);
        }
        true
    }

    pub(super) fn discard_minimized_snapshot_resources(&mut self, x11_win: u32) -> bool {
        self.clear_iconic_snapshot_recapture(x11_win);
        let removed_cpu_state = discard_minimized_cpu_snapshot_state(
            &mut self.pending_minimized_gpu_uploads,
            &mut self.minimized_snapshots,
            &mut self.minimized_snapshot_generations,
            x11_win,
        );
        let removed_gpu = self.remove_minimized_gpu_snapshot(x11_win);
        removed_cpu_state || removed_gpu
    }

    fn enforce_minimized_gpu_budget(&mut self, protected: u32) {
        while self.minimized_gpu_snapshots.len() > super::MINIMIZED_GPU_CACHE_MAX_ENTRIES
            || self
                .minimized_gpu_snapshots
                .values()
                .map(super::MinimizedGpuSnapshot::estimated_bytes)
                .fold(0usize, usize::saturating_add)
                > super::MINIMIZED_GPU_CACHE_MAX_BYTES
        {
            let Some(victim) = gpu_snapshot_lru_candidate(
                self.minimized_gpu_snapshots
                    .iter()
                    .map(|(&window, snapshot)| (window, snapshot.last_use)),
                protected,
            ) else {
                break;
            };
            self.remove_minimized_gpu_snapshot(victim);
        }
    }

    fn insert_minimized_gpu_snapshot(
        &mut self,
        x11_win: u32,
        mut snapshot: super::MinimizedGpuSnapshot,
    ) {
        snapshot.last_use = self.next_minimized_gpu_use();
        if let Some(old) = self.minimized_gpu_snapshots.insert(x11_win, snapshot) {
            unsafe {
                self.gl.delete_texture(old.texture);
            }
        }
        self.enforce_minimized_gpu_budget(x11_win);
    }

    fn admit_captured_minimized_snapshot(
        &mut self,
        x11_win: u32,
        captured: super::CapturedMinimizedSnapshot,
    ) -> bool {
        if self.minimized_snapshot_generations.get(&x11_win) != Some(&captured.cpu.generation())
            || captured.gpu.generation != captured.cpu.generation()
        {
            unsafe {
                self.gl.delete_texture(captured.gpu.texture);
            }
            return false;
        }

        match self.minimized_snapshots.admit(
            x11_win,
            captured.cpu,
            SnapshotRetention::RecapturableMapped,
        ) {
            AdmissionOutcome::Admitted { evicted } => {
                for victim in evicted {
                    self.pending_minimized_gpu_uploads.remove(&victim);
                    self.remove_minimized_gpu_snapshot(victim);
                }
                self.insert_minimized_gpu_snapshot(x11_win, captured.gpu);
                self.pending_minimized_gpu_uploads.remove(&x11_win);
                self.clear_iconic_snapshot_recapture(x11_win);
                self.resume_minimized_preview_after_capture(x11_win);
                self.needs_render = true;
                true
            }
            AdmissionOutcome::AlreadyCurrent
            | AdmissionOutcome::RejectedStale
            | AdmissionOutcome::RejectedCapacity => {
                unsafe {
                    self.gl.delete_texture(captured.gpu.texture);
                }
                false
            }
        }
    }

    pub(super) fn cache_minimized_snapshot_from_texture(
        &mut self,
        x11_win: u32,
        texture: glow::Texture,
        width: u32,
        height: u32,
        has_alpha: bool,
    ) -> bool {
        // This helper performs framebuffer work and readback. WM-facing
        // minimize/adoption paths may run after a failed swap, when the
        // graphics context is explicitly known not-current; defer those
        // callers instead of issuing undefined GL commands.
        if !self.context_current {
            self.request_iconic_snapshot_recapture(x11_win);
            return false;
        }
        let generation = self.ensure_minimized_snapshot_generation(x11_win);
        if self
            .minimized_snapshots
            .peek(&x11_win)
            .is_some_and(|snapshot| snapshot.generation() == generation)
        {
            self.clear_iconic_snapshot_recapture(x11_win);
            self.arm_minimized_gpu_upload(x11_win);
            return true;
        }
        self.note_iconic_snapshot_capture_attempt(x11_win);
        let Some(captured) = self
            .capture_minimized_snapshot_from_texture(texture, width, height, has_alpha, generation)
        else {
            log::warn!(
                "{}: window 0x{x11_win:x}",
                self.renderer_ctx("minimized thumbnail: capture")
            );
            return false;
        };
        let admitted = self.admit_captured_minimized_snapshot(x11_win, captured);
        if admitted {
            // Today a successful capture supplies its own independent GPU
            // texture.  Keep the explicit arm here so a future CPU-only
            // capture path still receives exactly one promotion attempt.
            self.arm_minimized_gpu_upload(x11_win);
        }
        admitted
    }

    pub(super) fn ensure_minimized_gpu_snapshot(&mut self, x11_win: u32) -> bool {
        let Some(generation) = self.minimized_snapshot_generations.get(&x11_win).copied() else {
            return false;
        };
        if self
            .minimized_gpu_snapshots
            .get(&x11_win)
            .is_some_and(|snapshot| snapshot.generation == generation)
        {
            let last_use = self.next_minimized_gpu_use();
            self.minimized_gpu_snapshots
                .get_mut(&x11_win)
                .expect("checked minimized GPU snapshot disappeared")
                .last_use = last_use;
            let _ = self.minimized_snapshots.get(&x11_win);
            return true;
        }
        self.remove_minimized_gpu_snapshot(x11_win);

        let Some(snapshot) = self.minimized_snapshots.get(&x11_win).cloned() else {
            return false;
        };
        if snapshot.generation() != generation {
            return false;
        }
        let Some(gpu) = self.upload_minimized_snapshot_texture(&snapshot) else {
            return false;
        };
        self.insert_minimized_gpu_snapshot(x11_win, gpu);
        true
    }

    pub(super) fn touch_minimized_snapshot(&mut self, x11_win: u32) -> bool {
        let mut touched = self.minimized_snapshots.get(&x11_win).is_some();
        if self.minimized_gpu_snapshots.contains_key(&x11_win) {
            let last_use = self.next_minimized_gpu_use();
            self.minimized_gpu_snapshots
                .get_mut(&x11_win)
                .expect("checked minimized GPU snapshot disappeared")
                .last_use = last_use;
            touched = true;
        }
        touched
    }

    /// Record an externally-observable use of a retained Dock texture. Keep
    /// this out of render/tick paths: one touch per geometry, preview, or
    /// restore request is enough to protect an actively used item from LRU
    /// eviction without turning every frame into a cache write.
    pub(super) fn touch_minimized_visual(&mut self, x11_win: u32, now: std::time::Instant) -> bool {
        touch_retained_visual(
            self.minimized_visuals
                .get_mut(&x11_win)
                .map(|visual| &mut visual.cached_at),
            now,
        )
    }

    pub(super) fn incremental_effects_active(&self) -> bool {
        (!self.particle_systems.is_empty() && self.particle_effects)
            || tilt_animation_pending(
                self.window_tilt,
                self.tilt_current_x,
                self.tilt_current_y,
                self.tilt_target_x,
                self.tilt_target_y,
            )
            || self.windows.values().any(|wt| {
                ((self.fading || self.window_animation_uses_fade())
                    && (wt.fading_out || wt.fade_opacity < 1.0))
                    || (self.window_animation
                        && (wt.anim_scale - wt.anim_scale_target).abs() > 0.001)
            })
    }

    /// Tick wobbly grid spring-mass physics. Returns true if any wobbly is active.
    pub(super) fn tick_wobbly(&mut self) -> bool {
        if !self.wobbly_windows {
            return false;
        }
        let neighbor_k = self.wobbly_stiffness;
        let restore_k = self.wobbly_restore_stiffness;
        let damping = self.wobbly_damping;
        let mut any_active = false;
        let mut to_clear = Vec::new();

        let now = std::time::Instant::now();

        for (&win, wt) in self.windows.iter_mut() {
            if let Some(ref mut w) = wt.wobbly {
                let dt = w.elapsed_dt(now);
                if w.tick_physics(dt, neighbor_k, restore_k, damping) {
                    any_active = true;
                } else {
                    to_clear.push(win);
                }
            }
        }

        for win in to_clear {
            if let Some(wt) = self.windows.get_mut(&win) {
                wt.wobbly = None;
            }
        }

        any_active
    }

    /// Tick particle systems. Removes dead particles and empty systems.
    pub(super) fn tick_particles(&mut self, dt: f32) -> bool {
        if !self.particle_effects {
            self.particle_systems.clear();
            return false;
        }
        let simulation_dt = clamp_effect_dt(dt);
        let lifetime_dt = sanitize_animation_dt(dt);
        let gravity = self.particle_gravity;

        self.particle_systems.retain_mut(|sys| {
            sys.particles.retain_mut(|p| {
                p.vy += gravity * simulation_dt;
                p.x += p.vx * simulation_dt;
                p.y += p.vy * simulation_dt;
                // Lifetime is a visual timeline, not a numerical integration:
                // catch it up after stalls while keeping motion bounded.
                p.lifetime -= lifetime_dt;
                p.lifetime > 0.0
            });
            !sys.particles.is_empty()
        });
        !self.particle_systems.is_empty()
    }

    /// Render active particle systems.
    pub(super) fn render_particles(&mut self, proj: &[f32; 16]) {
        if self.particle_systems.is_empty() {
            return;
        }

        // Collect all particles into a persistent flat buffer. Close bursts can
        // overlap, so allocating this vector on every animation frame showed up
        // prominently in allocator profiles.
        self.scratch_particle_data.clear();
        let expected_floats = self
            .particle_systems
            .iter()
            .map(|system| system.particles.len() * 7)
            .sum();
        self.scratch_particle_data.reserve(expected_floats);
        let mut count = 0u32;
        for sys in &self.particle_systems {
            for p in &sys.particles {
                let life_frac = (p.lifetime / p.max_lifetime).clamp(0.0, 1.0);
                self.scratch_particle_data.extend_from_slice(&[
                    p.x, p.y, p.color[0], p.color[1], p.color[2], p.color[3], life_frac,
                ]);
                count += 1;
            }
        }

        if count == 0 {
            return;
        }

        unsafe {
            self.gl.use_program(Some(self.particle_program));
            self.gl.uniform_matrix_4_f32_slice(
                self.particle_uniforms.projection.as_ref(),
                false,
                proj,
            );
            self.gl
                .uniform_1_f32(self.particle_uniforms.point_size.as_ref(), 4.0);

            self.gl.enable(glow::PROGRAM_POINT_SIZE);
            self.gl.bind_vertex_array(Some(self.particle_vao));
            self.gl
                .bind_buffer(glow::ARRAY_BUFFER, Some(self.particle_vbo));

            let byte_data: &[u8] = std::slice::from_raw_parts(
                self.scratch_particle_data.as_ptr() as *const u8,
                self.scratch_particle_data.len() * std::mem::size_of::<f32>(),
            );
            self.gl
                .buffer_data_u8_slice(glow::ARRAY_BUFFER, byte_data, glow::STREAM_DRAW);
            self.gl.draw_arrays(glow::POINTS, 0, count as i32);

            self.gl.disable(glow::PROGRAM_POINT_SIZE);
            self.gl.bind_buffer(glow::ARRAY_BUFFER, None);
            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);
        }
    }

    /// Spawn particles when a window is removed (particle effect).
    pub(super) fn spawn_particles_for_window(&mut self, x: i32, y: i32, w: u32, h: u32) {
        if !self.particle_effects {
            return;
        }

        let count = particle_burst_count(self.particle_count);
        if count == 0 || w == 0 || h == 0 {
            return;
        }
        let lifetime = self.particle_lifetime.max(0.001);
        let mut particles = Vec::with_capacity(count);

        let cols = (count as f32).sqrt().ceil() as u32;
        let rows = (count as u32 + cols - 1) / cols;

        for i in 0..count {
            let col = i as u32 % cols;
            let row = i as u32 / cols;

            let px = x as f32 + (col as f32 + 0.5) / cols as f32 * w as f32;
            let py = y as f32 + (row as f32 + 0.5) / rows as f32 * h as f32;

            // Repeatable variation avoids pulling an RNG into the render thread
            // while still preventing visible rows of identical particles.
            let seed = (i as u32)
                .wrapping_mul(0x9e37_79b9)
                .wrapping_add((x as u32).rotate_left(11))
                .wrapping_add((y as u32).rotate_left(21));
            let vx = (effect_noise(seed) * 2.0 - 1.0) * 180.0;
            let vy = -(80.0 + effect_noise(seed ^ 0xa5a5_5a5a) * 300.0);

            // Color from window position (gradient)
            let r = (col as f32 / cols as f32 * 0.5 + 0.5).clamp(0.3, 1.0);
            let g = (row as f32 / rows as f32 * 0.5 + 0.5).clamp(0.3, 1.0);
            let b = 0.8;

            particles.push(Particle {
                x: px,
                y: py,
                vx,
                vy,
                color: [r, g, b, 1.0],
                lifetime,
                max_lifetime: lifetime,
            });
        }

        if self.particle_systems.len() >= MAX_PARTICLE_SYSTEMS {
            self.particle_systems.remove(0);
        }
        // A burst created between frames must not inherit time accumulated by
        // an older fade/particle system (for example across suspend/resume).
        self.effect_tick_clock.reset();
        self.particle_systems
            .push(super::ParticleSystem { particles });
        self.needs_render = true;
    }

    /// Advance fade animations.
    pub(super) fn tick_fades(&mut self, dt: f32) -> FadeTick {
        let frame_scale = sanitize_animation_dt(dt) * 60.0;
        let animation_fades = self.window_animation_uses_fade();
        let mut tick = FadeTick::default();
        let mut to_remove = Vec::new();

        for (&win, wt) in self.windows.iter_mut() {
            let mut window_active = false;

            // Fade animation
            if self.fading || animation_fades {
                if wt.fading_out {
                    wt.fade_opacity -= self.fade_out_step * frame_scale;
                    if wt.fade_opacity <= 0.0 {
                        wt.fade_opacity = 0.0;
                        to_remove.push(win);
                    } else {
                        window_active = true;
                    }
                } else if wt.fade_opacity < 1.0 {
                    wt.fade_opacity += self.fade_in_step * frame_scale;
                    if wt.fade_opacity >= 1.0 {
                        wt.fade_opacity = 1.0;
                    } else {
                        window_active = true;
                    }
                }
            }

            // Scale animation (window open/close zoom)
            if self.window_animation {
                let diff = wt.anim_scale_target - wt.anim_scale;
                if diff.abs() > 0.001 {
                    let step = if diff > 0.0 {
                        self.fade_in_step * frame_scale
                    } else {
                        -self.fade_out_step * frame_scale
                    };
                    wt.anim_scale += step;
                    if (wt.anim_scale_target - wt.anim_scale).abs() < 0.001
                        || (step > 0.0 && wt.anim_scale >= wt.anim_scale_target)
                        || (step < 0.0 && wt.anim_scale <= wt.anim_scale_target)
                    {
                        wt.anim_scale = wt.anim_scale_target;
                    } else {
                        window_active = true;
                    }
                }
            }

            if window_active {
                tick.record(wt.is_override_redirect);
            }
        }

        for win in to_remove {
            self.remove_window_immediate(win);
        }

        tick
    }

    // =================================================================
    // Phase 3.1: Motion trail
    // =================================================================

    /// Trail tuning for one window, combining config with the window's size.
    pub(super) fn motion_trail_params(&self, wt: &WindowTexture) -> MotionTrailParams {
        MotionTrailParams::new(
            if self.motion_trail_enabled {
                self.motion_trail_frames
            } else {
                0
            },
            self.motion_trail_opacity,
            wt.w as f32,
            wt.h as f32,
        )
    }

    /// Advance a window's motion trail by an interactive-move delta.
    pub(super) fn update_motion_trail(&mut self, x11_win: u32, dx: f32, dy: f32) {
        if !self.motion_trail_enabled {
            return;
        }
        let Some(params) = self
            .windows
            .get(&x11_win)
            .map(|wt| self.motion_trail_params(wt))
        else {
            return;
        };
        if let Some(wt) = self.windows.get_mut(&x11_win) {
            // The window rect has already moved, so its pre-move origin seeds
            // the logical cursor when the drag was not announced through
            // move_start.
            let fallback = (wt.x as f32 - dx, wt.y as f32 - dy);
            wt.motion_trail.record_delta(dx, dy, fallback, &params);
        }
    }

    /// Expire motion-trail samples using wall-clock time.
    pub(super) fn tick_motion_trails(&mut self) -> bool {
        if !self.motion_trail_enabled || self.motion_trail_frames == 0 {
            for wt in self.windows.values_mut() {
                wt.motion_trail.clear();
            }
            return false;
        }
        let now = std::time::Instant::now();
        let lifetime = motion_trail_lifetime(self.motion_trail_frames);
        let mut active = false;
        for wt in self.windows.values_mut() {
            active |= wt.motion_trail.retain_live(now, lifetime);
        }
        active
    }

    // =================================================================
    // Phase 3.2: Genie minimize tick
    // =================================================================

    /// Tick Genie-adjacent state. Returns true if a Genie or Dock preview is active.
    ///
    /// Completed Genie owners are deliberately retained here. This method runs
    /// before drawing, so settling them now can delete the texture before its
    /// terminal mesh is ever presented. `finish_genie_frame` performs that
    /// ownership transfer only after a successful swap.
    pub(super) fn tick_genie(&mut self) -> bool {
        let preview_active = self.tick_dock_preview();
        !self.genie_active.is_empty() || preview_active
    }

    /// Settle animations whose terminal state was sampled by the frame that
    /// has just been presented.
    pub(super) fn finish_genie_frame(&mut self, frame_sample_time: std::time::Instant) {
        let duration_secs = self.genie_duration_ms.max(1) as f32 / 1000.0;
        let mut i = 0;
        while i < self.genie_active.len() {
            let (_, done) =
                genie_animation_progress(&self.genie_active[i], frame_sample_time, duration_secs);
            if completed_genie_may_settle(done, true) {
                let ga = self.genie_active.remove(i);
                match ga.direction {
                    GenieDirection::Minimize if ga.owns_resources => {
                        self.cache_minimized_visual(ga);
                    }
                    GenieDirection::Restore => {
                        if ga.owns_resources {
                            self.free_texture_resources(
                                ga.gl_texture,
                                ga.binding,
                                ga.pixmap,
                                ga.damage,
                            );
                        }
                        self.minimized_windows.remove(&ga.x11_win);
                        self.minimized_window_intents.remove(&ga.x11_win);
                        self.pending_static_minimized_captures.remove(&ga.x11_win);
                        self.genie_targets.remove(&ga.x11_win);
                        self.minimized_window_metadata.remove(&ga.x11_win);
                        self.discard_minimized_snapshot_resources(ga.x11_win);
                        if self
                            .dock_preview
                            .is_some_and(|preview| preview.x11_win == ga.x11_win)
                        {
                            self.set_minimized_window_preview(None);
                        }
                    }
                    GenieDirection::Minimize => {}
                }
                self.needs_render = true;
            } else {
                i += 1;
            }
        }
    }

    /// Start a genie animation for a window explicitly being minimized.
    ///
    /// Takes ownership of the window's GL texture + imported/X pixmap + damage by
    /// removing the WindowTexture from the live set and moving its resources
    /// into the animation. `tick_genie` frees them when the animation ends.
    /// This avoids both double-drawing the window and sampling a freed texture.
    pub(super) fn start_genie_animation(
        &mut self,
        x11_win: u32,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        animate: bool,
    ) {
        // A manually-unredirected fullscreen client is still being presented
        // by the X server.  Its old named pixmap/binding is no longer an
        // authoritative capture source, and merely clearing the marker here
        // leaves the window unredirected forever.  Keep the live owner and the
        // PendingMinimize intent until the render loop has re-redirected the
        // client and imported the replacement backing pixmap.  That successful
        // refresh settles the window into the retained cache without sampling
        // stale direct-presentation pixels.
        if minimized_capture_waits_for_redirect(self.unredirected_window == Some(x11_win)) {
            self.needs_render = true;
            return;
        }
        if let Some(wt) =
            crate::backend::compositor_common::genie::take_live_window_preserving_metadata(
                x11_win,
                &mut self.windows,
                &mut self.minimized_window_metadata,
                |window| super::WindowVisualMetadata::from(window),
            )
        {
            self.needs_render = true;
            // Keep the authoritative texture alive first. CPU readback is
            // serviced only after render_frame successfully makes the
            // graphics context current.
            if !self.current_minimized_cpu_snapshot_available(x11_win) {
                self.request_iconic_snapshot_recapture(x11_win);
            }

            // Restore -> Minimize is a reversal of the one existing mesh, not
            // a second animation.  In the cached-visual branch the restore
            // already owns its old detached texture, so this newly imported
            // live entry must be released.  In the cache-eviction branch the
            // restore merely borrowed the live texture; removing WindowTexture
            // here transfers every native resource into that same animation.
            if let Some(index) = self
                .genie_active
                .iter()
                .position(|animation| animation.x11_win == x11_win)
            {
                let duration_secs = self.genie_duration_ms.max(1) as f32 / 1000.0;
                let now = std::time::Instant::now();
                let action =
                    reverse_restore_resource_action(self.genie_active[index].owns_resources);
                if self.genie_active[index].direction == GenieDirection::Restore {
                    let animation = &mut self.genie_active[index];
                    retarget_genie_timeline(
                        &mut animation.start,
                        &mut animation.start_progress,
                        &mut animation.direction,
                        GenieDirection::Minimize,
                        now,
                        duration_secs,
                    );
                }

                match action {
                    ReverseRestoreResourceAction::ReleaseLive => {
                        self.free_texture_resources(
                            wt.gl_texture,
                            wt.binding,
                            wt.pixmap,
                            wt.damage,
                        );
                    }
                    ReverseRestoreResourceAction::AdoptLive => {
                        let animation = &mut self.genie_active[index];
                        animation.gl_texture = wt.gl_texture;
                        animation.has_rgba = wt.has_rgba;
                        animation.binding = wt.binding;
                        animation.pixmap = wt.pixmap;
                        animation.damage = wt.damage;
                        animation.owns_resources = true;
                    }
                }
                return;
            }

            let animation = super::GenieAnimation {
                x11_win,
                start: std::time::Instant::now(),
                start_progress: 0.0,
                direction: GenieDirection::Minimize,
                x,
                y,
                w,
                h,
                gl_texture: wt.gl_texture,
                has_rgba: wt.has_rgba,
                target: self.genie_target_for(x11_win),
                owns_resources: true,
                binding: wt.binding,
                pixmap: wt.pixmap,
                damage: wt.damage,
            };
            if animate {
                self.genie_active.push(animation);
            } else {
                self.cache_minimized_visual(animation);
            }
        }
    }

    /// Settle a texture that arrived after the WM had already minimized the
    /// client. The X geometry is deliberately ignored: startup adoption parks
    /// hidden windows outside the desktop, so playing a Genie from that source
    /// would produce a flash across a negative-origin output. The imported
    /// pixels go directly into the same bounded LRU used by completed
    /// animations.
    pub(super) fn settle_late_minimized_window(&mut self, x11_win: u32) {
        // See start_genie_animation: a live texture that predates manual
        // unredirect is not a safe retained source.  The refresh path calls us
        // again only after redirect + native pixmap import succeeds.
        if minimized_capture_waits_for_redirect(self.unredirected_window == Some(x11_win)) {
            self.needs_render = true;
            return;
        }
        let has_retained_pixels = self.minimized_visuals.contains_key(&x11_win)
            || self
                .genie_active
                .iter()
                .any(|animation| animation.x11_win == x11_win);
        let disposition = late_minimized_window_disposition(
            self.windows.contains_key(&x11_win),
            has_retained_pixels,
        );
        let Some(wt) =
            crate::backend::compositor_common::genie::take_live_window_preserving_metadata(
                x11_win,
                &mut self.windows,
                &mut self.minimized_window_metadata,
                |window| super::WindowVisualMetadata::from(window),
            )
        else {
            debug_assert_eq!(
                disposition,
                LateMinimizedWindowDisposition::AwaitLiveTexture
            );
            return;
        };

        self.ripple_active
            .retain(|ripple| ripple.x11_win != x11_win);
        self.needs_render = true;

        // Settlement is reachable from synchronous WM/event paths. It may
        // publish or reuse a retained source, but the current-context render
        // service alone is allowed to read it back.
        if !self.current_minimized_cpu_snapshot_available(x11_win) {
            self.request_iconic_snapshot_recapture(x11_win);
        }

        if disposition == LateMinimizedWindowDisposition::ReleaseDuplicateLiveTexture {
            // A delayed duplicate AddWindow must not replace or reverse the
            // animation/cache that already owns this minimize request.
            self.free_texture_resources(wt.gl_texture, wt.binding, wt.pixmap, wt.damage);
            return;
        }

        let Some((w, h)) = late_minimized_visual_dimensions(wt.w, wt.h) else {
            self.free_texture_resources(wt.gl_texture, wt.binding, wt.pixmap, wt.damage);
            return;
        };
        self.cache_minimized_visual(super::GenieAnimation {
            x11_win,
            start: std::time::Instant::now(),
            start_progress: 1.0,
            direction: GenieDirection::Minimize,
            // Static convergence never samples client placement. In
            // particular, do not retain deliberately off-screen hidden x/y.
            x: 0.0,
            y: 0.0,
            w,
            h,
            gl_texture: wt.gl_texture,
            has_rgba: wt.has_rgba,
            target: self.genie_target_for(x11_win),
            owns_resources: true,
            binding: wt.binding,
            pixmap: wt.pixmap,
            damage: wt.damage,
        });
    }

    pub(super) fn genie_target_for(&self, x11_win: u32) -> crate::backend::api::CompositorRect {
        self.genie_targets
            .get(&x11_win)
            .copied()
            .unwrap_or_else(|| {
                crate::backend::api::CompositorRect::new(
                    (self.screen_w / 2) as f32,
                    self.screen_h.saturating_sub(1) as f32,
                    1.0,
                    1.0,
                )
                .normalized()
                .expect("one-pixel fallback Dock target is valid")
            })
    }

    /// Start or reverse the visual after the X window has been restored to
    /// its final live geometry by `add_window`.
    pub(super) fn start_genie_restore(&mut self, x11_win: u32, x: f32, y: f32, w: f32, h: f32) {
        if !self.minimized_windows.contains(&x11_win) || !self.windows.contains_key(&x11_win) {
            return;
        }
        // `add_window` calls us only after its native pixmap import has
        // produced this live WindowTexture. A low-resolution Dock card is
        // never a restore source, so only now retire both cache tiers and this
        // XID's epoch before starting/borrowing the exact full-resolution
        // source.
        self.discard_minimized_snapshot_resources(x11_win);
        if !self.genie_minimize {
            if let Some(visual) = self.minimized_visuals.remove(&x11_win) {
                self.free_texture_resources(
                    visual.gl_texture,
                    visual.binding,
                    visual.pixmap,
                    visual.damage,
                );
            }
            self.minimized_windows.remove(&x11_win);
            self.minimized_window_intents.remove(&x11_win);
            self.pending_static_minimized_captures.remove(&x11_win);
            self.genie_targets.remove(&x11_win);
            self.minimized_window_metadata.remove(&x11_win);
            if self
                .dock_preview
                .is_some_and(|preview| preview.x11_win == x11_win)
            {
                self.set_minimized_window_preview(None);
            }
            self.needs_render = true;
            return;
        }
        let now = std::time::Instant::now();
        let duration_secs = self.genie_duration_ms.max(1) as f32 / 1000.0;
        let target = self.genie_target_for(x11_win);
        if let Some(animation) = self
            .genie_active
            .iter_mut()
            .find(|animation| animation.x11_win == x11_win)
        {
            let (progress, _) = genie_animation_progress(animation, now, duration_secs);
            animation.start = now;
            animation.start_progress = progress;
            animation.direction = GenieDirection::Restore;
            animation.x = x;
            animation.y = y;
            animation.w = w;
            animation.h = h;
            animation.target = target;
            self.needs_render = true;
            return;
        }

        if let Some(visual) = self.minimized_visuals.remove(&x11_win) {
            self.genie_active.push(super::GenieAnimation {
                x11_win,
                start: now,
                start_progress: 1.0,
                direction: GenieDirection::Restore,
                x,
                y,
                w,
                h,
                gl_texture: visual.gl_texture,
                has_rgba: visual.has_rgba,
                target,
                owns_resources: true,
                binding: visual.binding,
                pixmap: visual.pixmap,
                damage: visual.damage,
            });
            self.needs_render = true;
            return;
        }

        // Cache eviction must not turn restore into a hard pop. Borrow the
        // freshly imported live texture and suppress its ordinary scene draw
        // until this animation reaches progress zero.
        if let Some(wt) = self.windows.get(&x11_win) {
            self.genie_active.push(super::GenieAnimation {
                x11_win,
                start: now,
                start_progress: 1.0,
                direction: GenieDirection::Restore,
                x,
                y,
                w,
                h,
                gl_texture: wt.gl_texture,
                has_rgba: wt.has_rgba,
                target,
                owns_resources: false,
                binding: None,
                pixmap: 0,
                damage: 0,
            });
            self.needs_render = true;
        }
    }

    pub(super) fn cache_minimized_visual(&mut self, ga: super::GenieAnimation) {
        let newest_window = ga.x11_win;
        self.pending_static_minimized_captures
            .remove(&newest_window);
        let mut evicted_visual = false;
        let estimated_bytes =
            crate::backend::compositor_common::genie::estimated_visual_bytes(ga.w, ga.h);
        let visual = super::MinimizedVisual {
            w: ga.w,
            h: ga.h,
            gl_texture: ga.gl_texture,
            has_rgba: ga.has_rgba,
            target: self.genie_targets.get(&ga.x11_win).copied(),
            binding: ga.binding,
            pixmap: ga.pixmap,
            damage: ga.damage,
            cached_at: std::time::Instant::now(),
            estimated_bytes,
        };
        if let Some(old) = self.minimized_visuals.insert(newest_window, visual) {
            self.free_texture_resources(old.gl_texture, old.binding, old.pixmap, old.damage);
        }
        if !self.current_minimized_cpu_snapshot_available(newest_window) {
            // Settlement publishes a stable retained source and arms one
            // fallback demand. Genie settlement now happens after presenting
            // its terminal frame, so the render-only service performs the
            // readback on the follow-up frame already armed by this method.
            self.request_iconic_snapshot_recapture(newest_window);
        }
        self.resume_minimized_preview_after_capture(newest_window);
        while crate::backend::compositor_common::genie::minimized_cache_over_budget(
            self.minimized_visuals.len(),
            self.minimized_visuals
                .values()
                .map(|visual| visual.estimated_bytes)
                .fold(0u64, u64::saturating_add),
        ) {
            let Some(oldest) = retained_lru_candidate(
                self.minimized_visuals
                    .iter()
                    .map(|(&window, visual)| (window, visual.cached_at)),
                newest_window,
            ) else {
                break;
            };
            if let Some(old) = self.minimized_visuals.remove(&oldest) {
                evicted_visual = true;
                self.pending_static_minimized_captures.remove(&oldest);
                if preview_loses_source_after_full_eviction(
                    self.current_minimized_cpu_snapshot_available(oldest),
                    self.current_minimized_gpu_snapshot_available(oldest),
                ) {
                    suspend_preview_for_eviction(
                        self.dock_preview.as_mut(),
                        |preview| preview.x11_win == oldest,
                        |preview| {
                            let now = std::time::Instant::now();
                            preview.started = now;
                            preview.start_opacity = 0.0;
                            preview.start_scale = 0.86;
                            preview.opacity = 0.0;
                            preview.scale = 0.86;
                            preview.awaiting_source = true;
                        },
                    );
                }
                self.free_texture_resources(old.gl_texture, old.binding, old.pixmap, old.damage);
            }
        }
        if evicted_visual {
            // The retained thumbnail (and, when selected, its preview) can be
            // the only compositor-owned pixels left for an unmapped window.
            // Clear its last framebuffer image when the LRU releases it.
            self.force_full_redraw();
        }
    }

    pub(super) fn discard_minimized_visual(&mut self, x11_win: u32) {
        let removed_snapshot = self.discard_minimized_snapshot_resources(x11_win);
        self.minimized_windows.remove(&x11_win);
        self.minimized_window_intents.remove(&x11_win);
        self.pending_static_minimized_captures.remove(&x11_win);
        self.genie_targets.remove(&x11_win);
        self.minimized_window_metadata.remove(&x11_win);
        let mut removed_animation = false;
        while let Some(index) = self
            .genie_active
            .iter()
            .position(|animation| animation.x11_win == x11_win)
        {
            let animation = self.genie_active.remove(index);
            if animation.owns_resources {
                self.free_texture_resources(
                    animation.gl_texture,
                    animation.binding,
                    animation.pixmap,
                    animation.damage,
                );
            }
            removed_animation = true;
        }
        let removed_visual = if let Some(visual) = self.minimized_visuals.remove(&x11_win) {
            self.free_texture_resources(
                visual.gl_texture,
                visual.binding,
                visual.pixmap,
                visual.damage,
            );
            true
        } else {
            false
        };
        let removed_preview = self
            .dock_preview
            .is_some_and(|preview| preview.x11_win == x11_win);
        if removed_preview {
            self.dock_preview = None;
        }
        arm_full_redraw_after_retained_discard(
            removed_animation,
            removed_visual || removed_snapshot,
            removed_preview,
            || self.force_full_redraw(),
        );
    }

    /// Retire compositor ownership for a client that remains hidden in JWM
    /// but is no longer a Dock item. Unlike `prepare_window_restore`, this
    /// cannot create a reverse animation or make the live X pixmap drawable.
    ///
    /// The live-resource branch covers the target-less texture that can
    /// survive an older desired-state replay. Calling this method repeatedly
    /// is safe: every collection removal and native-resource release is
    /// conditional on ownership still being present.
    pub(crate) fn forget_minimized_window_visual(&mut self, x11_win: u32) {
        // Eligibility withdrawal is not a close.  A fullscreen client may
        // have entered minimize while manually unredirected, so restore
        // Composite ownership before releasing its live texture.  On a
        // transient protocol failure preserve the marker after release; the
        // render loop can retry the redirect without reviving Dock resources.
        let retry_redirect = if self.unredirected_window.take() == Some(x11_win) {
            !self.restore_unredirected_window(
                x11_win,
                "hidden window left minimized Dock eligibility",
            )
        } else {
            false
        };
        let preview_window = self.dock_preview.as_ref().map(|preview| preview.x11_win);
        let removed_snapshot = self.discard_minimized_snapshot_resources(x11_win);
        let removed_intent = self.minimized_window_intents.remove(&x11_win).is_some();
        let mut resources =
            crate::backend::compositor_common::genie::take_forgotten_minimized_resources(
                x11_win,
                &mut self.minimized_windows,
                &mut self.pending_static_minimized_captures,
                &mut self.genie_targets,
                &mut self.genie_active,
                |animation| animation.x11_win,
                &mut self.minimized_visuals,
                preview_window,
            );
        resources.state_changed |= removed_intent;
        resources.state_changed |= removed_snapshot;
        if resources.preview_removed {
            self.dock_preview = None;
        }
        for animation in resources.animations {
            if animation.owns_resources {
                self.free_texture_resources(
                    animation.gl_texture,
                    animation.binding,
                    animation.pixmap,
                    animation.damage,
                );
            }
        }
        if let Some(visual) = resources.visual {
            self.free_texture_resources(
                visual.gl_texture,
                visual.binding,
                visual.pixmap,
                visual.damage,
            );
        }
        let live = crate::backend::compositor_common::genie::take_live_window_preserving_metadata(
            x11_win,
            &mut self.windows,
            &mut self.minimized_window_metadata,
            |live| super::WindowVisualMetadata::from(live),
        );
        resources.state_changed |= live.is_some();
        if let Some(live) = live {
            self.free_texture_resources(live.gl_texture, live.binding, live.pixmap, live.damage);
            log::debug!("compositor: forgot hidden Dock window 0x{x11_win:x}");
        }
        if retry_redirect {
            self.unredirected_window = Some(x11_win);
        }
        let ripple_count = self.ripple_active.len();
        self.ripple_active
            .retain(|ripple| ripple.x11_win != x11_win);
        if resources.state_changed || self.ripple_active.len() != ripple_count {
            // A replay-imported hidden window or detached animation may have
            // been the only owner of pixels in the previous framebuffer.
            self.force_full_redraw();
        }
    }

    fn tick_dock_preview(&mut self) -> bool {
        let now = std::time::Instant::now();
        if let Some(x11_win) = self
            .dock_preview
            .filter(|preview| preview.awaiting_source)
            .map(|preview| preview.x11_win)
        {
            if !self.minimized_preview_source_available(x11_win) {
                // A pending static import is event driven. Do not spin frames,
                // consume the show transition, or expire the lease while the
                // hidden pixmap has no drawable pixels yet.
                return false;
            }
            self.resume_minimized_preview_after_capture(x11_win);
        }
        if self.dock_preview.is_some_and(|preview| {
            crate::backend::compositor_common::genie::preview_lease_timeout(
                preview.direction,
                now,
                preview.lease_deadline,
            ) == Some(std::time::Duration::ZERO)
        }) {
            self.set_minimized_window_preview(None);
        }
        let Some(preview) = self.dock_preview.as_mut() else {
            return false;
        };
        let (opacity, scale, done) = preview_motion(
            preview.start_opacity,
            preview.start_scale,
            preview.direction,
            now.saturating_duration_since(preview.started).as_secs_f32(),
        );
        preview.opacity = opacity;
        preview.scale = scale;
        if !done {
            return true;
        }
        if preview.direction == PreviewDirection::Hide {
            self.dock_preview = None;
        } else {
            preview.start_opacity = 1.0;
            preview.start_scale = 1.0;
            preview.opacity = 1.0;
            preview.scale = 1.0;
        }
        false
    }

    // =================================================================
    // Phase 3.3: Ripple tick
    // =================================================================

    /// Tick ripple effects. Returns true if any are active.
    pub(super) fn tick_ripples(&mut self) -> bool {
        if self.ripple_active.is_empty() {
            return false;
        }
        let duration = std::time::Duration::from_secs_f32(self.ripple_duration.max(f32::EPSILON));
        let now = std::time::Instant::now();
        self.ripple_active
            .retain(|r| now.duration_since(r.start) < duration);
        !self.ripple_active.is_empty()
    }

    // =================================================================
    // Phase 3.4: Focus highlight tick
    // =================================================================

    /// Returns true if focus highlight is currently animating.
    pub(super) fn tick_focus_highlight(&self) -> bool {
        if !self.focus_highlight {
            return false;
        }
        if let Some((_, start)) = self.focus_highlight_start {
            let elapsed = start.elapsed().as_millis() as u64;
            elapsed < self.focus_highlight_duration_ms
        } else {
            false
        }
    }

    // =================================================================
    // Phase 3.5: Wallpaper crossfade tick
    // =================================================================

    /// Tick tilt smooth interpolation. Returns true if tilt is visually active.
    pub(super) fn tick_tilt(&mut self, dt: f32) -> bool {
        if !self.window_tilt {
            return false;
        }
        let alpha = smoothing_alpha(self.tilt_speed, dt);
        self.tilt_current_x += (self.tilt_target_x - self.tilt_current_x) * alpha;
        self.tilt_current_y += (self.tilt_target_y - self.tilt_current_y) * alpha;
        let epsilon = 0.0001;
        let dx = (self.tilt_current_x - self.tilt_target_x).abs();
        let dy = (self.tilt_current_y - self.tilt_target_y).abs();
        if dx < epsilon && dy < epsilon {
            self.tilt_current_x = self.tilt_target_x;
            self.tilt_current_y = self.tilt_target_y;
        }
        dx > epsilon || dy > epsilon
    }

    /// Returns true if wallpaper crossfade is currently animating.
    pub(super) fn tick_wallpaper_crossfade(&mut self) -> bool {
        if !self.wallpaper_crossfade {
            return false;
        }
        if let Some(start) = self.wallpaper_transition_start {
            let elapsed = start.elapsed().as_millis() as u64;
            if elapsed >= self.wallpaper_crossfade_duration_ms {
                // Transition finished — clean up old texture
                if let Some(tex) = self.old_wallpaper_texture.take() {
                    unsafe {
                        self.gl.delete_texture(tex);
                    }
                }
                self.old_wallpaper_img_w = 0;
                self.old_wallpaper_img_h = 0;
                self.wallpaper_transition_start = None;
                false
            } else {
                true
            }
        } else {
            false
        }
    }
}

/// A texture imported before XComposite manual re-redirection cannot be
/// detached as a minimized visual.  Keep this policy transport/GL agnostic so
/// its safety boundary remains covered without requiring a live X server.
const fn minimized_capture_waits_for_redirect(is_manually_unredirected: bool) -> bool {
    is_manually_unredirected
}

/// Preserve a preview whose retained source was evicted, while allowing its
/// renderer-specific animation state to be suspended. The next preview/ensure
/// request can then arm exactly one static recapture instead of requiring a
/// synthetic leave/enter cycle from the Dock.
fn suspend_preview_for_eviction<T>(
    preview: Option<&mut T>,
    matches_evicted_source: impl FnOnce(&T) -> bool,
    suspend: impl FnOnce(&mut T),
) -> bool {
    if let Some(preview) = preview
        && matches_evicted_source(preview)
    {
        suspend(preview);
        true
    } else {
        false
    }
}

/// A minimized window has normally left `Compositor::windows` already, so its
/// retained animation/cache/preview can be the only thing changing the final
/// framebuffer.  Arm a full redraw whenever one of those rendered owners is
/// discarded; relying on live-window removal or the ordinary scene hash would
/// leave the previous Dock pixels in a recycled back buffer.
fn arm_full_redraw_after_retained_discard(
    removed_animation: bool,
    removed_visual: bool,
    removed_preview: bool,
    arm: impl FnOnce(),
) {
    if removed_animation || removed_visual || removed_preview {
        arm();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReverseRestoreResourceAction {
    /// The reverse animation owns a detached minimized visual; the separate
    /// live entry removed for this minimize request must be released.
    ReleaseLive,
    /// The reverse animation borrowed this live entry; adopt all of it.
    AdoptLive,
}

const fn reverse_restore_resource_action(
    animation_owns_resources: bool,
) -> ReverseRestoreResourceAction {
    if animation_owns_resources {
        ReverseRestoreResourceAction::ReleaseLive
    } else {
        ReverseRestoreResourceAction::AdoptLive
    }
}

pub(super) fn genie_animation_progress(
    animation: &super::GenieAnimation,
    now: std::time::Instant,
    duration_secs: f32,
) -> (f32, bool) {
    genie_progress(
        animation.start_progress,
        animation.direction,
        now.saturating_duration_since(animation.start).as_secs_f32(),
        duration_secs,
    )
}

#[cfg(test)]
mod tests {
    use super::{
        EffectTickClock, FadeTick, LateMinimizedWindowDisposition, ReverseRestoreResourceAction,
        arm_full_redraw_after_retained_discard, begin_retained_recapture_attempt,
        completed_genie_may_settle, consume_minimized_gpu_upload_request,
        discard_minimized_cpu_snapshot_state, gpu_snapshot_lru_candidate,
        has_iconic_snapshot_token, late_minimized_visual_dimensions,
        late_minimized_window_disposition, minimized_capture_source_dimensions,
        minimized_capture_waits_for_redirect, preview_loses_source_after_full_eviction,
        release_iconic_snapshot_token, reserve_current_iconic_snapshot, retained_lru_candidate,
        reverse_restore_resource_action, suspend_preview_for_eviction, tilt_animation_pending,
        touch_retained_visual,
    };
    use crate::backend::common_define::WindowId;
    use crate::backend::compositor_common::minimized_thumbnail::{
        AdmissionOutcome, IconicSnapshotReservationError, MinimizedSnapshot,
        MinimizedSnapshotCache, SnapshotGeneration, SnapshotRecaptureGate, SnapshotRetention,
    };
    use crate::backend::x11::wm::iconify::{IconifyCoordinator, finish_checked_unmap};
    use std::collections::{HashMap, HashSet};
    use std::time::{Duration, Instant};

    fn cpu_snapshot(generation: u64, fill: u8) -> MinimizedSnapshot {
        MinimizedSnapshot::try_new(1, 1, generation, true, vec![fill; 4]).unwrap()
    }

    #[test]
    fn compositor_state_reserves_and_releases_only_the_current_cpu_generation() {
        let window = 42_u32;
        let current = SnapshotGeneration::new(7).unwrap();
        let stale = SnapshotGeneration::new(6).unwrap();
        let mut generations = HashMap::new();
        let mut snapshots = MinimizedSnapshotCache::new();

        // A full-size visual could exist elsewhere in Compositor, but it is
        // intentionally absent from this admission state and cannot make the
        // transaction ready without bounded CPU pixels.
        generations.insert(window, current);
        assert_eq!(
            reserve_current_iconic_snapshot(&generations, &mut snapshots, window),
            Err(IconicSnapshotReservationError::NoSnapshot)
        );

        assert!(matches!(
            snapshots.admit(
                window,
                cpu_snapshot(current.get(), 7),
                SnapshotRetention::RecapturableMapped,
            ),
            AdmissionOutcome::Admitted { evicted } if evicted.is_empty()
        ));
        generations.insert(window, stale);
        assert_eq!(
            reserve_current_iconic_snapshot(&generations, &mut snapshots, window),
            Err(IconicSnapshotReservationError::GenerationMismatch {
                expected: stale,
                actual: current,
            })
        );
        assert!(!snapshots.has_iconic_snapshot_reservation(&window, current));

        generations.insert(window, current);
        assert_eq!(
            reserve_current_iconic_snapshot(&generations, &mut snapshots, window),
            Ok(current)
        );
        assert!(has_iconic_snapshot_token(&snapshots, window, current.get()));
        assert!(!has_iconic_snapshot_token(&snapshots, window, 0));
        assert!(!release_iconic_snapshot_token(
            &mut snapshots,
            window,
            stale.get()
        ));
        assert!(!release_iconic_snapshot_token(&mut snapshots, window, 0));
        assert!(has_iconic_snapshot_token(&snapshots, window, current.get()));
        assert!(release_iconic_snapshot_token(
            &mut snapshots,
            window,
            current.get()
        ));
        assert!(!has_iconic_snapshot_token(
            &snapshots,
            window,
            current.get()
        ));
        assert_eq!(snapshots.peek(&window).unwrap().rgba()[0], 7);
    }

    #[test]
    fn failed_make_current_never_consumes_readback_gate_then_success_consumes_once() {
        let mut gate = SnapshotRecaptureGate::default();
        gate.request();
        let capacity_epoch = 11;
        let mut readbacks = 0;

        // Model both a context already known false and a render whose
        // make_current attempt failed. Neither is allowed to consume demand.
        for _ in 0..2 {
            if begin_retained_recapture_attempt(&mut gate, capacity_epoch, false, true) {
                readbacks += 1;
            }
        }
        assert_eq!(readbacks, 0);
        assert!(gate.is_due(capacity_epoch));

        if begin_retained_recapture_attempt(&mut gate, capacity_epoch, true, true) {
            readbacks += 1;
        }
        assert_eq!(readbacks, 1);
        for _ in 0..32 {
            assert!(!begin_retained_recapture_attempt(
                &mut gate,
                capacity_epoch,
                true,
                true,
            ));
        }
        assert_eq!(readbacks, 1);
    }

    #[test]
    fn current_context_recapture_unstarves_pinned_capacity_and_unmaps_once() {
        const A: u32 = 1;
        const B: u32 = 2;
        let generation = SnapshotGeneration::new(1).unwrap();
        let generations = HashMap::from([(A, generation), (B, generation)]);
        let mut snapshots = MinimizedSnapshotCache::with_limits(4, 1);
        assert!(matches!(
            snapshots.admit(
                A,
                cpu_snapshot(generation.get(), 1),
                SnapshotRetention::RecapturableMapped,
            ),
            AdmissionOutcome::Admitted { evicted } if evicted.is_empty()
        ));
        assert_eq!(
            reserve_current_iconic_snapshot(&generations, &mut snapshots, A),
            Ok(generation)
        );

        let b_window = WindowId::from_raw(B as u64);
        let mut coordinator = IconifyCoordinator::default();
        assert!(coordinator.request(b_window));
        let retained_full_visual = true;
        let mut gate = SnapshotRecaptureGate::default();
        gate.request();
        let mut readbacks = 0;
        assert!(begin_retained_recapture_attempt(
            &mut gate,
            snapshots.capacity_epoch(),
            true,
            retained_full_visual,
        ));
        readbacks += 1;
        assert_eq!(
            snapshots.admit(
                B,
                cpu_snapshot(generation.get(), 2),
                SnapshotRetention::RecapturableMapped,
            ),
            AdmissionOutcome::RejectedCapacity
        );
        assert_eq!(
            reserve_current_iconic_snapshot(&generations, &mut snapshots, B),
            Err(IconicSnapshotReservationError::NoSnapshot)
        );
        for _ in 0..32 {
            assert!(!begin_retained_recapture_attempt(
                &mut gate,
                snapshots.capacity_epoch(),
                true,
                retained_full_visual,
            ));
        }

        assert!(release_iconic_snapshot_token(
            &mut snapshots,
            A,
            generation.get(),
        ));
        gate.request();
        // Explicit retry while make_current is unavailable remains armed.
        assert!(!begin_retained_recapture_attempt(
            &mut gate,
            snapshots.capacity_epoch(),
            false,
            retained_full_visual,
        ));
        assert!(begin_retained_recapture_attempt(
            &mut gate,
            snapshots.capacity_epoch(),
            true,
            retained_full_visual,
        ));
        readbacks += 1;
        assert_eq!(
            snapshots.admit(
                B,
                cpu_snapshot(generation.get(), 2),
                SnapshotRetention::RecapturableMapped,
            ),
            AdmissionOutcome::Admitted { evicted: vec![A] }
        );
        assert_eq!(
            reserve_current_iconic_snapshot(&generations, &mut snapshots, B),
            Ok(generation)
        );

        let mut checked_unmaps = 0;
        if coordinator.phase(b_window)
            == Some(crate::backend::x11::wm::iconify::IconifyPhase::AwaitingAdmission)
        {
            checked_unmaps += 1;
            finish_checked_unmap(
                &mut coordinator,
                b_window,
                generation.get(),
                Ok::<(), &str>(()),
                || unreachable!("successful checked unmap keeps the pin"),
            )
            .unwrap();
        }
        for _ in 0..32 {
            assert!(coordinator.awaiting_windows().is_empty());
        }
        assert_eq!(readbacks, 2);
        assert_eq!(checked_unmaps, 1);
        assert!(snapshots.has_iconic_snapshot_reservation(&B, generation));
    }

    #[test]
    fn ordinary_unpinned_snapshot_discard_semantics_are_unchanged() {
        let window = 42_u32;
        let generation = SnapshotGeneration::new(7).unwrap();
        let mut generations = HashMap::from([(window, generation)]);
        let mut snapshots = MinimizedSnapshotCache::new();
        let mut pending_uploads = HashSet::from([window]);
        assert!(matches!(
            snapshots.admit(
                window,
                cpu_snapshot(generation.get(), 7),
                SnapshotRetention::RecapturableMapped,
            ),
            AdmissionOutcome::Admitted { evicted } if evicted.is_empty()
        ));

        assert!(discard_minimized_cpu_snapshot_state(
            &mut pending_uploads,
            &mut snapshots,
            &mut generations,
            window,
        ));
        assert!(!pending_uploads.contains(&window));
        assert!(snapshots.peek(&window).is_none());
        assert!(!generations.contains_key(&window));
    }

    #[test]
    fn failed_explicit_gpu_upload_is_not_retried_by_unrelated_frames() {
        let window = 42_u32;
        let mut pending = HashSet::from([window]);
        assert!(consume_minimized_gpu_upload_request(
            &mut pending,
            window,
            true,
            false,
        ));

        // Model one hundred later render passes after that attempt failed.
        // Without a new geometry/hover/capture arm, none may upload again.
        for _ in 0..100 {
            assert!(!consume_minimized_gpu_upload_request(
                &mut pending,
                window,
                true,
                false,
            ));
        }

        // A later explicit geometry/hover/capture demand permits one new
        // attempt, then closes the gate again.
        pending.insert(window);
        assert!(consume_minimized_gpu_upload_request(
            &mut pending,
            window,
            true,
            false,
        ));
        assert!(!consume_minimized_gpu_upload_request(
            &mut pending,
            window,
            true,
            false,
        ));
    }

    #[test]
    fn visual_forget_takes_every_x11_owner_once_and_preserves_other_windows() {
        let window = 42_u32;
        let other = 7_u32;
        let mut minimized = HashSet::from([window, other]);
        let mut pending = HashSet::from([window, other]);
        let mut targets = HashMap::from([(window, "target"), (other, "other target")]);
        let mut animations = vec![(window, "first"), (other, "other"), (window, "second")];
        let mut visuals = HashMap::from([(window, "visual"), (other, "other visual")]);
        let mut live = HashMap::from([(window, "live"), (other, "other live")]);
        let mut metadata = HashMap::new();

        let first = crate::backend::compositor_common::genie::take_forgotten_minimized_resources(
            window,
            &mut minimized,
            &mut pending,
            &mut targets,
            &mut animations,
            |animation| animation.0,
            &mut visuals,
            Some(window),
        );
        let first_live =
            crate::backend::compositor_common::genie::take_live_window_preserving_metadata(
                window,
                &mut live,
                &mut metadata,
                |_| ("org.example.Player", true),
            );
        assert_eq!(
            first.animations,
            vec![(window, "first"), (window, "second")]
        );
        assert_eq!(first.visual, Some("visual"));
        assert_eq!(first_live, Some("live"));
        let &(class_name, is_pip) = metadata.get(&window).unwrap();
        assert_eq!(class_name, "org.example.Player");
        assert!(is_pip);
        assert!(first.preview_removed);
        assert!(first.state_changed);
        assert_eq!(minimized, HashSet::from([other]));
        assert_eq!(pending, HashSet::from([other]));
        assert_eq!(targets, HashMap::from([(other, "other target")]));
        assert_eq!(animations, vec![(other, "other")]);
        assert_eq!(visuals, HashMap::from([(other, "other visual")]));
        assert_eq!(live, HashMap::from([(other, "other live")]));

        let second = crate::backend::compositor_common::genie::take_forgotten_minimized_resources(
            window,
            &mut minimized,
            &mut pending,
            &mut targets,
            &mut animations,
            |animation| animation.0,
            &mut visuals,
            None,
        );
        let second_live =
            crate::backend::compositor_common::genie::take_live_window_preserving_metadata(
                window,
                &mut live,
                &mut metadata,
                |_| unreachable!("an idempotent forget has no second live owner"),
            );
        assert!(second.animations.is_empty());
        assert!(second.visual.is_none());
        assert!(second_live.is_none());
        let &(class_name, is_pip) = metadata.get(&window).unwrap();
        assert_eq!(class_name, "org.example.Player");
        assert!(is_pip);
        assert!(!second.preview_removed);
        assert!(!second.state_changed);
    }

    #[test]
    fn ime_popup_fades_do_not_count_as_client_fades() {
        // fcitx5's candidate list fading on every keystroke keeps the render
        // loop pacing frames (`any`) but must not feed the adaptive blur
        // downgrade (`on_clients`), or a lone frosted client pumps between
        // Full and Reduced blur the whole time a user types Chinese.
        let mut tick = FadeTick::default();
        assert!(!tick.any && !tick.on_clients);

        tick.record(true); // override-redirect IME popup
        assert!(tick.any);
        assert!(!tick.on_clients);

        tick.record(false); // managed client
        assert!(tick.any);
        assert!(tick.on_clients);
    }

    #[test]
    fn idle_time_is_not_applied_to_a_new_effect() {
        let mut clock = EffectTickClock::default();
        let start = Instant::now();

        assert_eq!(clock.delta(start, false), 0.0);
        assert_eq!(clock.delta(start + Duration::from_secs(30), true), 0.0);

        let frame_dt = clock.delta(
            start + Duration::from_secs(30) + Duration::from_millis(16),
            true,
        );
        assert!((frame_dt - 0.016).abs() < 0.000_001);
    }

    #[test]
    fn independent_gpu_lru_protects_the_newest_insert() {
        assert_eq!(
            gpu_snapshot_lru_candidate([(1_u32, 9_u64), (2, 2), (3, 7)], 2),
            Some(3)
        );
        assert_eq!(gpu_snapshot_lru_candidate([(2_u32, 1_u64)], 2), None);
    }

    #[test]
    fn retained_eviction_keeps_preview_live_when_either_low_tier_exists() {
        assert!(!preview_loses_source_after_full_eviction(true, false));
        assert!(!preview_loses_source_after_full_eviction(false, true));
        assert!(preview_loses_source_after_full_eviction(false, false));
    }

    #[test]
    fn animation_dimensions_are_sanitized_before_fallback_capture() {
        assert_eq!(
            minimized_capture_source_dimensions(1279.6, 719.5),
            Some((1280, 720))
        );
        assert_eq!(minimized_capture_source_dimensions(f32::NAN, 720.0), None);
        assert_eq!(minimized_capture_source_dimensions(1280.0, 0.0), None);
    }

    #[test]
    fn resetting_an_active_clock_protects_a_newly_spawned_effect() {
        let mut clock = EffectTickClock::default();
        let start = Instant::now();

        assert_eq!(clock.delta(start, true), 0.0);
        assert!(clock.delta(start + Duration::from_millis(16), true) > 0.0);

        clock.reset();
        assert_eq!(clock.delta(start + Duration::from_secs(10), true), 0.0);
    }

    #[test]
    fn finishing_the_last_effect_rearms_the_clock_for_the_next_one() {
        let mut clock = EffectTickClock::default();
        let start = Instant::now();

        assert_eq!(clock.delta(start, true), 0.0);
        clock.finish_frame(start, false);
        assert_eq!(clock.delta(start + Duration::from_secs(10), true), 0.0);
    }

    #[test]
    fn tilt_pending_keeps_the_incremental_clock_running_after_its_zero_delta_frame() {
        let mut clock = EffectTickClock::default();
        let start = Instant::now();

        assert!(!tilt_animation_pending(true, 0.0, 0.0, 0.0, 0.0));
        assert_eq!(clock.delta(start, false), 0.0);

        // The render pass discovers the pointer-derived target after the
        // frame delta has already been sampled. Preserve that first frame as
        // active so the following frame receives a real delta.
        assert!(tilt_animation_pending(true, 0.0, 0.0, 0.08, -0.04));
        clock.finish_frame(start, true);
        let next_delta = clock.delta(start + Duration::from_millis(16), true);
        assert!((next_delta - 0.016).abs() < 0.000_001);

        assert!(!tilt_animation_pending(false, 0.0, 0.0, 0.08, -0.04));
    }

    #[test]
    fn completed_genie_retains_its_texture_until_terminal_frame_is_presented() {
        assert!(!completed_genie_may_settle(true, false));
        assert!(!completed_genie_may_settle(false, true));
        assert!(completed_genie_may_settle(true, true));
    }

    #[test]
    fn restore_reversal_releases_an_unrelated_live_texture_when_animation_owns_pixels() {
        assert_eq!(
            reverse_restore_resource_action(true),
            ReverseRestoreResourceAction::ReleaseLive
        );
    }

    #[test]
    fn restore_reversal_adopts_the_live_texture_when_animation_only_borrowed_pixels() {
        assert_eq!(
            reverse_restore_resource_action(false),
            ReverseRestoreResourceAction::AdoptLive
        );
    }

    #[test]
    fn retained_only_discard_arms_full_redraw_without_a_live_window() {
        let mut redraw_armed = false;

        // The live WindowTexture has already been detached by minimization;
        // removing the cached Dock visual must still invalidate the framebuffer.
        arm_full_redraw_after_retained_discard(false, true, false, || {
            redraw_armed = true;
        });

        assert!(redraw_armed);
    }

    #[test]
    fn eviction_suspends_but_preserves_preview_intent() {
        let mut preview = Some((42_u32, false));

        assert!(suspend_preview_for_eviction(
            preview.as_mut(),
            |(window, _)| *window == 42,
            |(_, awaiting_source)| *awaiting_source = true,
        ));
        assert_eq!(preview, Some((42, true)));

        let mut other_preview = Some((7_u32, false));
        assert!(!suspend_preview_for_eviction(
            other_preview.as_mut(),
            |(window, _)| *window == 42,
            |(_, awaiting_source)| *awaiting_source = true,
        ));
        assert_eq!(other_preview, Some((7, false)));
    }

    #[test]
    fn preview_or_restore_touch_protects_an_old_retained_visual_from_lru_eviction() {
        let captured = Instant::now();
        let mut active_cached_at = captured;
        let idle_cached_at = captured + Duration::from_millis(1);
        let newest_cached_at = captured + Duration::from_millis(2);

        // The same helper is wired to preview/geometry events and to the
        // pre-import restore intent. An old-but-active entry becomes newer
        // than the idle one before inserting the next retained visual.
        assert!(touch_retained_visual(
            Some(&mut active_cached_at),
            captured + Duration::from_millis(3),
        ));
        assert_eq!(
            retained_lru_candidate(
                [
                    (42_u32, active_cached_at),
                    (7_u32, idle_cached_at),
                    (99_u32, newest_cached_at),
                ],
                99,
            ),
            Some(7)
        );
    }

    #[test]
    fn late_minimize_waits_then_caches_exactly_one_live_texture() {
        assert_eq!(
            late_minimized_window_disposition(false, false),
            LateMinimizedWindowDisposition::AwaitLiveTexture
        );
        assert_eq!(
            late_minimized_window_disposition(true, false),
            LateMinimizedWindowDisposition::CacheLiveTexture
        );
        assert_eq!(
            late_minimized_window_disposition(true, true),
            LateMinimizedWindowDisposition::ReleaseDuplicateLiveTexture
        );
    }

    #[test]
    fn late_minimize_static_cache_uses_size_not_offscreen_placement() {
        // The settlement helper accepts only dimensions. Hidden x/y therefore
        // cannot leak into the retained visual or manufacture a Genie path
        // across a negative-origin monitor.
        assert_eq!(
            late_minimized_visual_dimensions(1280, 720),
            Some((1280.0, 720.0))
        );
        assert_eq!(late_minimized_visual_dimensions(0, 720), None);
        assert_eq!(late_minimized_visual_dimensions(1280, 0), None);
    }

    #[test]
    fn manually_unredirected_window_is_never_detached_as_a_minimize_capture() {
        assert!(minimized_capture_waits_for_redirect(true));
        assert!(!minimized_capture_waits_for_redirect(false));
    }
}
