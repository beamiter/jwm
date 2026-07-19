//! Damage history used to repair recycled GLX/EGL back buffers.

use super::dirty_region::DirtyRect;
use std::collections::VecDeque;

/// Keep enough history for normal double/triple buffering while bounding both
/// memory use and the amount of work spent merging unusually old buffers.
const MAX_DAMAGE_HISTORY: usize = 8;

pub(crate) struct BufferAgeDamageHistory {
    /// Most recently presented frame first. These are scene changes, not the
    /// larger repair regions used to bring recycled buffers up to date.
    recent_damage: VecDeque<DirtyRect>,
}

impl BufferAgeDamageHistory {
    pub(crate) fn new() -> Self {
        Self {
            recent_damage: VecDeque::with_capacity(MAX_DAMAGE_HISTORY),
        }
    }

    /// Expand the current frame damage with the changes missing from a recycled
    /// back buffer. `None` means the buffer is undefined or older than the
    /// bounded history and must be redrawn in full.
    pub(crate) fn repair_region(
        &self,
        current_damage: DirtyRect,
        buffer_age: u32,
    ) -> Option<DirtyRect> {
        if buffer_age == 0 {
            return None;
        }

        let missing_frames = usize::try_from(buffer_age - 1).ok()?;
        if missing_frames > self.recent_damage.len() {
            return None;
        }

        let mut repair = current_damage;
        for previous_damage in self.recent_damage.iter().take(missing_frames) {
            repair = repair.union(previous_damage);
        }
        Some(repair)
    }

    pub(crate) fn record(&mut self, frame_damage: DirtyRect) {
        if self.recent_damage.len() == MAX_DAMAGE_HISTORY {
            self.recent_damage.pop_back();
        }
        self.recent_damage.push_front(frame_damage);
    }

    pub(crate) fn clear(&mut self) {
        self.recent_damage.clear();
    }
}

impl Default for BufferAgeDamageHistory {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32) -> DirtyRect {
        DirtyRect::new(x, 0, 10, 10)
    }

    #[test]
    fn age_one_reuses_the_previous_frame_directly() {
        let history = BufferAgeDamageHistory::new();

        assert_eq!(history.repair_region(rect(20), 1), Some(rect(20)));
    }

    #[test]
    fn older_buffers_include_each_missing_frames_damage() {
        let mut history = BufferAgeDamageHistory::new();
        history.record(rect(20));
        history.record(rect(40));

        assert_eq!(
            history.repair_region(rect(60), 3),
            Some(DirtyRect::new(20, 0, 50, 10))
        );
        assert_eq!(
            history.repair_region(rect(60), 2),
            Some(DirtyRect::new(40, 0, 30, 10))
        );
    }

    #[test]
    fn undefined_or_untracked_buffers_require_a_full_redraw() {
        let mut history = BufferAgeDamageHistory::new();
        history.record(rect(20));

        assert_eq!(history.repair_region(rect(40), 0), None);
        assert_eq!(history.repair_region(rect(40), 3), None);
    }

    #[test]
    fn history_is_bounded_and_can_be_reset() {
        let mut history = BufferAgeDamageHistory::new();
        for x in 0..=MAX_DAMAGE_HISTORY {
            history.record(rect((x * 20) as i32));
        }

        assert!(
            history
                .repair_region(rect(200), MAX_DAMAGE_HISTORY as u32 + 1)
                .is_some()
        );
        assert_eq!(
            history.repair_region(rect(200), MAX_DAMAGE_HISTORY as u32 + 2),
            None
        );

        history.clear();
        assert_eq!(history.repair_region(rect(200), 2), None);
    }
}
