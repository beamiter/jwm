/// Event coalescing for better performance
use std::time::Instant;
use std::collections::VecDeque;

/// Motion event for coalescing
#[derive(Clone, Copy, Debug)]
pub struct MotionEvent {
    pub x: i32,
    pub y: i32,
    pub time: Instant,
}

/// Coalesces similar events to reduce event processing overhead
pub struct EventCoalescer {
    last_motion: Option<MotionEvent>,
    motion_queue: VecDeque<MotionEvent>,
    max_queue_size: usize,
}

impl EventCoalescer {
    pub fn new() -> Self {
        Self {
            last_motion: None,
            motion_queue: VecDeque::with_capacity(32),
            max_queue_size: 32,
        }
    }

    /// Record a motion event, returns Some if should be processed immediately
    /// Returns None if the event should be coalesced with the next one
    pub fn coalesce_motion(&mut self, x: i32, y: i32) -> Option<MotionEvent> {
        let event = MotionEvent {
            x,
            y,
            time: Instant::now(),
        };

        self.motion_queue.push_back(event);
        if self.motion_queue.len() > self.max_queue_size {
            // Keep queue bounded
            self.motion_queue.pop_front();
        }

        // Return the last queued event (coalesced from any duplicates)
        // This reduces the number of events we actually process
        if self.motion_queue.len() % 3 == 0 {
            // Process every 3rd event, or let frame boundary decide
            self.motion_queue.pop_back()
        } else {
            None
        }
    }

    /// Get the most recent motion event, clearing the queue
    /// Call this at frame boundaries to get the latest coalesced motion
    pub fn flush_motion(&mut self) -> Option<MotionEvent> {
        if let Some(event) = self.motion_queue.pop_back() {
            self.motion_queue.clear();
            self.last_motion = Some(event);
            Some(event)
        } else {
            None
        }
    }

    /// Get the last recorded motion without flushing
    pub fn last_motion(&self) -> Option<MotionEvent> {
        self.last_motion
    }

    /// Clear all coalesced events
    pub fn clear(&mut self) {
        self.motion_queue.clear();
        self.last_motion = None;
    }

    /// Get queue size (for debugging)
    pub fn queue_size(&self) -> usize {
        self.motion_queue.len()
    }
}

impl Default for EventCoalescer {
    fn default() -> Self {
        Self::new()
    }
}

/// Checks if two events should be considered identical for coalescing
pub fn events_similar(e1: &MotionEvent, e2: &MotionEvent, threshold: i32) -> bool {
    let dx = (e1.x - e2.x).abs();
    let dy = (e1.y - e2.y).abs();
    dx <= threshold && dy <= threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_motion_coalescing() {
        let mut coalescer = EventCoalescer::new();

        // Add several motion events
        let _ = coalescer.coalesce_motion(100, 100);
        let _ = coalescer.coalesce_motion(101, 101);
        let _ = coalescer.coalesce_motion(102, 102);

        // Flush and get the latest
        let latest = coalescer.flush_motion();
        assert!(latest.is_some());
        let evt = latest.unwrap();
        assert_eq!(evt.x, 102);
        assert_eq!(evt.y, 102);

        // Queue should be cleared
        assert_eq!(coalescer.queue_size(), 0);
    }
}
