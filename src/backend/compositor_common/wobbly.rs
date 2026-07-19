//! Shared wobbly-window spring grid state and physics.

use crate::backend::compositor_common::effects::finite_clamp;
use std::time::Instant;

const MAX_NODE_OFFSET: f32 = 4096.0;
const MAX_NODE_VELOCITY: f32 = 20_000.0;
const MAX_PHYSICS_SUBSTEPS: usize = 32;

/// Per-window wobbly animation state (grid spring-mass system).
pub(crate) struct WobblyState {
    pub(crate) grid_n: usize,
    pub(crate) offsets: Vec<[f32; 2]>,
    pub(crate) velocities: Vec<[f32; 2]>,
    forces: Vec<[f32; 2]>,
    pub(crate) dragging: bool,
    pub(crate) anchor_row: usize,
    pub(crate) anchor_col: usize,
    pub(crate) last_tick: Instant,
}

impl WobblyState {
    pub(crate) fn new(grid_n: usize, anchor_row: usize, anchor_col: usize) -> Self {
        let grid_n = grid_n.max(2);
        let count = grid_n * grid_n;
        Self {
            grid_n,
            offsets: vec![[0.0; 2]; count],
            velocities: vec![[0.0; 2]; count],
            forces: vec![[0.0; 2]; count],
            dragging: true,
            anchor_row: anchor_row.min(grid_n - 1),
            anchor_col: anchor_col.min(grid_n - 1),
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

    /// Apply a reverse impulse to all non-anchor nodes after the host window moved.
    pub(crate) fn apply_window_move_delta(&mut self, dx: f32, dy: f32) {
        let dx = finite_clamp(dx, -MAX_NODE_OFFSET, MAX_NODE_OFFSET, 0.0);
        let dy = finite_clamp(dy, -MAX_NODE_OFFSET, MAX_NODE_OFFSET, 0.0);
        let n = self.grid_n;
        for row in 0..n {
            for col in 0..n {
                if row == self.anchor_row && col == self.anchor_col {
                    continue;
                }
                let idx = row * n + col;
                self.offsets[idx][0] =
                    (self.offsets[idx][0] - dx).clamp(-MAX_NODE_OFFSET, MAX_NODE_OFFSET);
                self.offsets[idx][1] =
                    (self.offsets[idx][1] - dy).clamp(-MAX_NODE_OFFSET, MAX_NODE_OFFSET);
            }
        }
        self.pin_anchor();
    }

    pub(crate) fn end_drag(&mut self) {
        self.dragging = false;
    }

    pub(crate) fn tick_physics(
        &mut self,
        dt: f32,
        neighbor_k: f32,
        restore_k: f32,
        damping: f32,
        velocity_epsilon: f32,
    ) -> bool {
        let n = self.grid_n;
        let dt = crate::backend::compositor_common::effects::clamp_effect_dt(dt);
        if dt <= f32::EPSILON {
            return !self.is_settled(0.1, velocity_epsilon);
        }
        let neighbor_k = finite_clamp(neighbor_k, 0.0, 10_000.0, 600.0);
        let restore_k = finite_clamp(restore_k, 0.0, 10_000.0, 200.0);
        let damping = finite_clamp(damping, 0.0, 1_000.0, 30.0);
        let velocity_epsilon = finite_clamp(velocity_epsilon, 0.001, 100.0, 0.1);

        // A fixed three-step Euler integration becomes unstable at the upper
        // supported stiffness. Scale the step count with the fastest spring
        // mode; damping is applied exponentially below and therefore remains
        // stable even for very large configured values.
        let angular_frequency = (restore_k + 4.0 * neighbor_k).sqrt();
        let sub_steps =
            ((dt * angular_frequency / 0.5).ceil() as usize).clamp(1, MAX_PHYSICS_SUBSTEPS);
        let sub_dt = dt / sub_steps as f32;
        let velocity_decay = (-damping * sub_dt).exp();

        for _ in 0..sub_steps {
            self.forces.fill([0.0; 2]);

            for row in 0..n {
                for col in 0..n {
                    if self.dragging && row == self.anchor_row && col == self.anchor_col {
                        continue;
                    }
                    let idx = row * n + col;
                    let off = self.offsets[idx];
                    let mut fx = 0.0f32;
                    let mut fy = 0.0f32;

                    if row > 0 {
                        let ni = (row - 1) * n + col;
                        fx += neighbor_k * (self.offsets[ni][0] - off[0]);
                        fy += neighbor_k * (self.offsets[ni][1] - off[1]);
                    }
                    if row + 1 < n {
                        let ni = (row + 1) * n + col;
                        fx += neighbor_k * (self.offsets[ni][0] - off[0]);
                        fy += neighbor_k * (self.offsets[ni][1] - off[1]);
                    }
                    if col > 0 {
                        let ni = row * n + (col - 1);
                        fx += neighbor_k * (self.offsets[ni][0] - off[0]);
                        fy += neighbor_k * (self.offsets[ni][1] - off[1]);
                    }
                    if col + 1 < n {
                        let ni = row * n + (col + 1);
                        fx += neighbor_k * (self.offsets[ni][0] - off[0]);
                        fy += neighbor_k * (self.offsets[ni][1] - off[1]);
                    }

                    fx += -restore_k * off[0];
                    fy += -restore_k * off[1];
                    self.forces[idx] = [fx, fy];
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
                    self.offsets[idx][0] =
                        finite_clamp(self.offsets[idx][0], -MAX_NODE_OFFSET, MAX_NODE_OFFSET, 0.0);
                    self.offsets[idx][1] =
                        finite_clamp(self.offsets[idx][1], -MAX_NODE_OFFSET, MAX_NODE_OFFSET, 0.0);
                }
            }
        }

        let active = !self.is_settled(0.1, velocity_epsilon);
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

    fn is_settled(&self, offset_epsilon: f32, velocity_epsilon: f32) -> bool {
        !self.dragging
            && self
                .offsets
                .iter()
                .zip(self.velocities.iter())
                .all(|(o, v)| {
                    o[0].abs() < offset_epsilon
                        && o[1].abs() < offset_epsilon
                        && v[0].abs() < velocity_epsilon
                        && v[1].abs() < velocity_epsilon
                })
    }
}

#[cfg(test)]
mod tests {
    use super::WobblyState;

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
        let mut state = WobblyState::new(3, 1, 1);
        state.apply_window_move_delta(10.0, -5.0);
        let anchor = state.anchor_row * state.grid_n + state.anchor_col;
        assert_eq!(state.offsets[anchor], [0.0, 0.0]);
        assert_eq!(state.velocities[anchor], [0.0, 0.0]);
        assert_eq!(state.offsets[0], [-10.0, 5.0]);
    }

    #[test]
    fn physics_reports_settled_after_drag_ends_with_zero_motion() {
        let mut state = WobblyState::new(3, 1, 1);
        state.end_drag();
        assert!(!state.tick_physics(1.0 / 60.0, 600.0, 200.0, 30.0, 0.1));
    }

    #[test]
    fn extreme_physics_and_invalid_drag_input_remain_finite() {
        let mut state = WobblyState::new(3, 1, 1);
        state.apply_window_move_delta(f32::NAN, f32::INFINITY);
        state.apply_window_move_delta(100_000.0, -100_000.0);
        state.end_drag();

        for _ in 0..1_000 {
            state.tick_physics(0.05, 10_000.0, 10_000.0, 1_000.0, 0.1);
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
