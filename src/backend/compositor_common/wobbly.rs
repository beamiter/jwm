//! Shared wobbly-window spring grid state and physics.

use crate::backend::compositor_common::effects::finite_clamp;
use std::time::Instant;

const MAX_NODE_OFFSET: f32 = 4096.0;
const MAX_NODE_VELOCITY: f32 = 20_000.0;
const MAX_PHYSICS_SUBSTEPS: usize = 32;

/// Bounds for the per-window lag budget derived from the window size.
const MIN_LAG_BUDGET: f32 = 64.0;
const MAX_LAG_BUDGET: f32 = 720.0;
const DEFAULT_LAG_BUDGET: f32 = 256.0;

/// Fraction of the longer window edge a node may lag behind the window rect.
const LAG_BUDGET_RATIO: f32 = 0.5;

/// Stiffness of the diagonal springs relative to the axis-aligned ones.
///
/// A four-neighbour lattice offers no resistance to shear at all: every axis
/// spring can stay at rest length while the mesh collapses into a
/// parallelogram, which reads as a floppy smear instead of a sheet being
/// dragged by one corner. Bracing each cell with its two diagonals restores
/// that resistance; they stay softer than the axis springs so the wobble keeps
/// its loose feel.
const SHEAR_STIFFNESS_RATIO: f32 = 0.35;

/// Offsets and velocities under these thresholds are indistinguishable from a
/// window at rest, so the effect can be dropped and the render loop can idle
/// again. Both backends share them: a per-backend threshold only changed how
/// long the compositor kept redrawing an invisible wobble.
const SETTLE_OFFSET_EPSILON: f32 = 0.35;
const SETTLE_VELOCITY_EPSILON: f32 = 1.5;

/// Axis-aligned spring neighbours.
const AXIS_NEIGHBORS: [(isize, isize); 4] = [(-1, 0), (1, 0), (0, -1), (0, 1)];
/// Diagonal (shear) spring neighbours.
const SHEAR_NEIGHBORS: [(isize, isize); 4] = [(-1, -1), (-1, 1), (1, -1), (1, 1)];

/// Per-window wobbly animation state (grid spring-mass system).
pub(crate) struct WobblyState {
    pub(crate) grid_n: usize,
    pub(crate) offsets: Vec<[f32; 2]>,
    pub(crate) velocities: Vec<[f32; 2]>,
    forces: Vec<[f32; 2]>,
    /// Share of a drag delta each node absorbs: 0.0 at the grabbed node, 1.0 at
    /// the node furthest from it.
    drag_weights: Vec<f32>,
    /// Largest lag a node may accumulate, scaled to the window it belongs to.
    lag_budget: f32,
    pub(crate) dragging: bool,
    pub(crate) anchor_row: usize,
    pub(crate) anchor_col: usize,
    pub(crate) last_tick: Instant,
}

impl WobblyState {
    pub(crate) fn new(
        grid_n: usize,
        anchor_row: usize,
        anchor_col: usize,
        width: f32,
        height: f32,
    ) -> Self {
        let grid_n = grid_n.max(2);
        let count = grid_n * grid_n;
        let anchor_row = anchor_row.min(grid_n - 1);
        let anchor_col = anchor_col.min(grid_n - 1);
        Self {
            grid_n,
            offsets: vec![[0.0; 2]; count],
            velocities: vec![[0.0; 2]; count],
            forces: vec![[0.0; 2]; count],
            drag_weights: drag_weight_table(grid_n, anchor_row, anchor_col),
            lag_budget: lag_budget_for_size(width, height),
            dragging: true,
            anchor_row,
            anchor_col,
            last_tick: Instant::now(),
        }
    }

    pub(crate) fn anchor_for_point(
        grid_n: usize,
        rel_x: f32,
        rel_y: f32,
        width: f32,
        height: f32,
    ) -> (usize, usize) {
        let grid_n = grid_n.max(2);
        let width = width.max(1.0);
        let height = height.max(1.0);
        let col = ((rel_x.clamp(0.0, width) / width) * (grid_n - 1) as f32).round() as usize;
        let row = ((rel_y.clamp(0.0, height) / height) * (grid_n - 1) as f32).round() as usize;
        (row.min(grid_n - 1), col.min(grid_n - 1))
    }

    pub(crate) fn elapsed_dt(&mut self, now: Instant) -> f32 {
        let raw_dt = now.duration_since(self.last_tick).as_secs_f32();
        self.last_tick = now;
        crate::backend::compositor_common::effects::clamp_effect_dt(raw_dt)
    }

    /// Apply a reverse impulse after the host window moved.
    ///
    /// Offsets live in window-local space, so a window that moved by `d` leaves
    /// every node where it was on screen by subtracting `d`. Subtracting the
    /// *whole* delta everywhere would drag the mesh as one rigid slab and rely
    /// on the springs to smooth it out over the following frames, which reads
    /// as the window tearing in two. Weighting the delta by grid distance from
    /// the grabbed node makes the sheet drape away from the cursor on the very
    /// first frame instead.
    pub(crate) fn apply_window_move_delta(&mut self, dx: f32, dy: f32) {
        let budget = self.lag_budget;
        let dx = finite_clamp(dx, -budget, budget, 0.0);
        let dy = finite_clamp(dy, -budget, budget, 0.0);
        for (offset, weight) in self.offsets.iter_mut().zip(&self.drag_weights) {
            offset[0] = (offset[0] - dx * weight).clamp(-budget, budget);
            offset[1] = (offset[1] - dy * weight).clamp(-budget, budget);
        }
        self.pin_anchor();
    }

    pub(crate) fn end_drag(&mut self) {
        self.dragging = false;
    }

    /// Whether the mesh is still deformed enough to be worth a redraw.
    pub(crate) fn is_active(&self) -> bool {
        !self.is_settled()
    }

    pub(crate) fn tick_physics(
        &mut self,
        dt: f32,
        neighbor_k: f32,
        restore_k: f32,
        damping: f32,
    ) -> bool {
        let n = self.grid_n;
        let dt = crate::backend::compositor_common::effects::clamp_effect_dt(dt);
        if dt <= f32::EPSILON {
            return self.is_active();
        }
        let neighbor_k = finite_clamp(neighbor_k, 0.0, 10_000.0, 600.0);
        let restore_k = finite_clamp(restore_k, 0.0, 10_000.0, 200.0);
        let damping = finite_clamp(damping, 0.0, 1_000.0, 30.0);
        let shear_k = neighbor_k * SHEAR_STIFFNESS_RATIO;
        let budget = self.lag_budget;

        // A fixed three-step Euler integration becomes unstable at the upper
        // supported stiffness. Scale the step count with the fastest spring
        // mode; damping is applied exponentially below and therefore remains
        // stable even for very large configured values.
        let angular_frequency = (restore_k + 4.0 * neighbor_k + 4.0 * shear_k).sqrt();
        let sub_steps =
            ((dt * angular_frequency / 0.5).ceil() as usize).clamp(1, MAX_PHYSICS_SUBSTEPS);
        let sub_dt = dt / sub_steps as f32;
        let velocity_decay = (-damping * sub_dt).exp();

        for _ in 0..sub_steps {
            let offsets = &self.offsets;
            let forces = &mut self.forces;
            for row in 0..n {
                for col in 0..n {
                    let idx = row * n + col;
                    if self.dragging && row == self.anchor_row && col == self.anchor_col {
                        forces[idx] = [0.0; 2];
                        continue;
                    }
                    let off = offsets[idx];
                    let mut fx = -restore_k * off[0];
                    let mut fy = -restore_k * off[1];

                    for (neighbors, k) in
                        [(&AXIS_NEIGHBORS, neighbor_k), (&SHEAR_NEIGHBORS, shear_k)]
                    {
                        for &(d_row, d_col) in neighbors {
                            let n_row = row as isize + d_row;
                            let n_col = col as isize + d_col;
                            if n_row < 0 || n_col < 0 {
                                continue;
                            }
                            let (n_row, n_col) = (n_row as usize, n_col as usize);
                            if n_row >= n || n_col >= n {
                                continue;
                            }
                            let neighbor = offsets[n_row * n + n_col];
                            fx += k * (neighbor[0] - off[0]);
                            fy += k * (neighbor[1] - off[1]);
                        }
                    }

                    forces[idx] = [fx, fy];
                }
            }

            for row in 0..n {
                for col in 0..n {
                    if self.dragging && row == self.anchor_row && col == self.anchor_col {
                        continue;
                    }
                    let idx = row * n + col;
                    self.velocities[idx][0] += self.forces[idx][0] * sub_dt;
                    self.velocities[idx][1] += self.forces[idx][1] * sub_dt;
                    self.offsets[idx][0] += self.velocities[idx][0] * sub_dt;
                    self.offsets[idx][1] += self.velocities[idx][1] * sub_dt;
                    self.velocities[idx][0] = finite_clamp(
                        self.velocities[idx][0] * velocity_decay,
                        -MAX_NODE_VELOCITY,
                        MAX_NODE_VELOCITY,
                        0.0,
                    );
                    self.velocities[idx][1] = finite_clamp(
                        self.velocities[idx][1] * velocity_decay,
                        -MAX_NODE_VELOCITY,
                        MAX_NODE_VELOCITY,
                        0.0,
                    );
                    self.offsets[idx][0] = finite_clamp(self.offsets[idx][0], -budget, budget, 0.0);
                    self.offsets[idx][1] = finite_clamp(self.offsets[idx][1], -budget, budget, 0.0);
                }
            }
        }

        let active = self.is_active();
        if !active {
            self.offsets.fill([0.0; 2]);
            self.velocities.fill([0.0; 2]);
        }
        active
    }

    fn pin_anchor(&mut self) {
        let anchor_idx = self.anchor_row * self.grid_n + self.anchor_col;
        self.offsets[anchor_idx] = [0.0, 0.0];
        self.velocities[anchor_idx] = [0.0, 0.0];
    }

    fn is_settled(&self) -> bool {
        !self.dragging
            && self
                .offsets
                .iter()
                .zip(self.velocities.iter())
                .all(|(o, v)| {
                    o[0].abs() < SETTLE_OFFSET_EPSILON
                        && o[1].abs() < SETTLE_OFFSET_EPSILON
                        && v[0].abs() < SETTLE_VELOCITY_EPSILON
                        && v[1].abs() < SETTLE_VELOCITY_EPSILON
                })
    }
}

/// How far a node may lag behind the window rect, in pixels.
///
/// A flat pixel budget either tears a small window apart or barely bends a
/// large one, and it lets a warp across monitors smear the mesh over the whole
/// screen. Scaling with the window keeps the deformation proportional.
fn lag_budget_for_size(width: f32, height: f32) -> f32 {
    let extent = width.abs().max(height.abs());
    finite_clamp(
        extent * LAG_BUDGET_RATIO,
        MIN_LAG_BUDGET,
        MAX_LAG_BUDGET.min(MAX_NODE_OFFSET),
        DEFAULT_LAG_BUDGET,
    )
}

/// Smoothstep falloff of the drag delta by grid distance from the anchor.
fn drag_weight_table(grid_n: usize, anchor_row: usize, anchor_col: usize) -> Vec<f32> {
    let distance = |row: usize, col: usize| {
        let d_row = row as f32 - anchor_row as f32;
        let d_col = col as f32 - anchor_col as f32;
        (d_row * d_row + d_col * d_col).sqrt()
    };
    let max_distance = (0..grid_n)
        .flat_map(|row| (0..grid_n).map(move |col| (row, col)))
        .map(|(row, col)| distance(row, col))
        .fold(0.0f32, f32::max)
        .max(1.0);

    let mut weights = vec![0.0f32; grid_n * grid_n];
    for row in 0..grid_n {
        for col in 0..grid_n {
            let t = (distance(row, col) / max_distance).clamp(0.0, 1.0);
            weights[row * grid_n + col] = t * t * (3.0 - 2.0 * t);
        }
    }
    weights
}

#[cfg(test)]
mod tests {
    use super::WobblyState;

    fn state(grid_n: usize) -> WobblyState {
        WobblyState::new(grid_n, 1, 1, 600.0, 400.0)
    }

    #[test]
    fn anchor_for_point_clamps_to_grid() {
        assert_eq!(
            WobblyState::anchor_for_point(5, 50.0, 50.0, 100.0, 100.0),
            (2, 2)
        );
        assert_eq!(
            WobblyState::anchor_for_point(5, -10.0, 200.0, 100.0, 100.0),
            (4, 0)
        );
    }

    #[test]
    fn reverse_move_delta_keeps_anchor_pinned() {
        let mut state = state(3);
        state.apply_window_move_delta(10.0, -5.0);
        let anchor = state.anchor_row * state.grid_n + state.anchor_col;
        assert_eq!(state.offsets[anchor], [0.0, 0.0]);
        assert_eq!(state.velocities[anchor], [0.0, 0.0]);
        // Every corner of a 3x3 grid is the furthest node from the centre, so
        // it absorbs the whole delta.
        assert_eq!(state.offsets[0], [-10.0, 5.0]);
    }

    #[test]
    fn drag_lag_falls_off_towards_the_grabbed_node() {
        // 5x5 grid grabbed at the centre: lag has to grow monotonically with
        // grid distance instead of yanking the whole sheet by the full delta.
        let mut state = WobblyState::new(5, 2, 2, 600.0, 400.0);
        state.apply_window_move_delta(100.0, 0.0);
        let at = |row: usize, col: usize| state.offsets[row * state.grid_n + col][0];

        assert_eq!(at(2, 2), 0.0);
        assert_eq!(at(0, 0), -100.0);
        assert!(at(2, 1) > at(2, 0), "{} vs {}", at(2, 1), at(2, 0));
        assert!(at(2, 0) > at(0, 0), "{} vs {}", at(2, 0), at(0, 0));
        // The ring touching the grab point absorbs well under a third of it.
        assert!(at(2, 1) > -33.0, "near node lagged {}", at(2, 1));
    }

    #[test]
    fn lag_is_bounded_by_the_window_size() {
        // A 400x300 window gets a 200px budget; a warp across the screen must
        // not stretch the mesh further than that.
        let mut state = WobblyState::new(5, 0, 0, 400.0, 300.0);
        for _ in 0..8 {
            state.apply_window_move_delta(1_500.0, 1_500.0);
        }
        assert!(
            state
                .offsets
                .iter()
                .flatten()
                .all(|value| value.abs() <= 200.0),
            "offsets exceeded the window-relative budget"
        );
    }

    #[test]
    fn shear_springs_resist_a_diagonal_collapse() {
        // Displace the (0,0) node and step once, small enough that the
        // integrator uses a single substep. (1,1) touches (0,0) only along the
        // diagonal, so with axis springs alone it could not move at all.
        let mut state = WobblyState::new(3, 2, 2, 600.0, 400.0);
        state.end_drag();
        state.offsets[0] = [40.0, 0.0];
        state.tick_physics(0.001, 600.0, 200.0, 30.0);

        let diagonal = state.offsets[state.grid_n + 1];
        assert!(
            diagonal[0] > 0.0,
            "diagonal neighbour was not braced: {diagonal:?}"
        );
    }

    #[test]
    fn physics_reports_settled_after_drag_ends_with_zero_motion() {
        let mut state = state(3);
        state.end_drag();
        assert!(!state.tick_physics(1.0 / 60.0, 600.0, 200.0, 30.0));
    }

    #[test]
    fn released_wobble_settles_promptly() {
        let mut state = state(5);
        state.apply_window_move_delta(120.0, 60.0);
        state.end_drag();

        let mut frames = 0;
        while state.tick_physics(1.0 / 60.0, 600.0, 200.0, 30.0) && frames < 90 {
            frames += 1;
        }
        assert!(
            frames < 90,
            "wobble kept the render loop awake for {frames} frames"
        );
        assert!(!state.is_active());
    }

    #[test]
    fn extreme_physics_and_invalid_drag_input_remain_finite() {
        let mut state = state(3);
        state.apply_window_move_delta(f32::NAN, f32::INFINITY);
        state.apply_window_move_delta(100_000.0, -100_000.0);
        state.end_drag();

        for _ in 0..1_000 {
            state.tick_physics(0.05, 10_000.0, 10_000.0, 1_000.0);
        }

        assert!(
            state
                .offsets
                .iter()
                .chain(&state.velocities)
                .flatten()
                .all(|value| value.is_finite())
        );
        assert!(
            state
                .offsets
                .iter()
                .flatten()
                .all(|value| value.abs() <= super::MAX_NODE_OFFSET)
        );
    }
}
