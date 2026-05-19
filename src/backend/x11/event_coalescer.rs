use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// Motion event for coalescing
#[derive(Clone, Copy, Debug)]
pub struct MotionEvent {
    pub x: i32,
    pub y: i32,
    pub time: Instant,
}

/// Geometry update event for coalescing
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GeometryEvent {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Property change event for coalescing (per window + atom)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PropertyKey {
    pub window: u64,
    pub atom: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct PropertyEvent {
    pub window: u64,
    pub atom: u32,
    pub time: Instant,
}

/// Expose/damage region for coalescing
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExposeRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl ExposeRect {
    pub fn merge(&self, other: &ExposeRect) -> ExposeRect {
        let x1 = self.x.min(other.x);
        let y1 = self.y.min(other.y);
        let x2 = (self.x + self.width as i32).max(other.x + other.width as i32);
        let y2 = (self.y + self.height as i32).max(other.y + other.height as i32);
        ExposeRect {
            x: x1,
            y: y1,
            width: (x2 - x1) as u32,
            height: (y2 - y1) as u32,
        }
    }
}

/// Coalesces similar events to reduce event processing overhead
pub struct EventCoalescer {
    last_motion: Option<MotionEvent>,
    last_emitted: Option<MotionEvent>,
    pending: Option<MotionEvent>,
    motion_queue: VecDeque<MotionEvent>,
    max_queue_size: usize,
    time_threshold: Duration,
    distance_threshold_sq: i32,
    // Geometry event coalescing
    pending_geometry: Option<GeometryEvent>,
    last_geometry_emitted: Option<Instant>,
    geometry_time_threshold: Duration,
    // Property change coalescing (per window+atom)
    pending_properties: HashMap<PropertyKey, PropertyEvent>,
    property_time_threshold: Duration,
    // Expose event coalescing (per window, merged regions)
    pending_exposes: HashMap<u64, ExposeRect>,
    expose_time_threshold: Duration,
}

impl EventCoalescer {
    pub fn new() -> Self {
        Self {
            last_motion: None,
            last_emitted: None,
            pending: None,
            motion_queue: VecDeque::with_capacity(32),
            max_queue_size: 32,
            time_threshold: Duration::from_millis(16), // 60Hz frame time
            distance_threshold_sq: 4,                  // 2px squared
            pending_geometry: None,
            last_geometry_emitted: None,
            geometry_time_threshold: Duration::from_millis(32), // 30Hz for geometry updates
            pending_properties: HashMap::new(),
            property_time_threshold: Duration::from_millis(50), // 20Hz for property changes
            pending_exposes: HashMap::new(),
            expose_time_threshold: Duration::from_millis(16), // 60Hz for expose events
        }
    }

    /// Create a new coalescer with custom thresholds
    pub fn with_thresholds(time_ms: u64, distance_px: i32) -> Self {
        Self {
            last_motion: None,
            last_emitted: None,
            pending: None,
            motion_queue: VecDeque::with_capacity(32),
            max_queue_size: 32,
            time_threshold: Duration::from_millis(time_ms),
            distance_threshold_sq: distance_px * distance_px,
            pending_geometry: None,
            last_geometry_emitted: None,
            geometry_time_threshold: Duration::from_millis(32),
            pending_properties: HashMap::new(),
            property_time_threshold: Duration::from_millis(50),
            pending_exposes: HashMap::new(),
            expose_time_threshold: Duration::from_millis(16),
        }
    }

    /// Record a motion event, returns Some if should be processed immediately
    /// Returns None if the event should be coalesced with the next one
    pub fn coalesce_motion(&mut self, x: i32, y: i32) -> Option<MotionEvent> {
        let now = Instant::now();
        let event = MotionEvent { x, y, time: now };

        // Time + distance based coalescing:
        // Always pass through if queue empty or time window exceeded or distance threshold exceeded
        if let Some(last) = self.last_emitted {
            let dt = now.duration_since(last.time);
            let dist_sq = (x - last.x).pow(2) + (y - last.y).pow(2);

            if dt < self.time_threshold && dist_sq < self.distance_threshold_sq {
                // Within coalescing window: store but don't emit
                self.pending = Some(event);
                self.motion_queue.push_back(event);
                if self.motion_queue.len() > self.max_queue_size {
                    self.motion_queue.pop_front();
                }
                return None;
            }
        }

        // Emit this event
        self.last_emitted = Some(event);
        self.last_motion = Some(event);
        self.pending = None;
        self.motion_queue.push_back(event);
        if self.motion_queue.len() > self.max_queue_size {
            self.motion_queue.pop_front();
        }
        Some(event)
    }

    /// Get the most recent motion event, clearing the queue
    /// Call this at frame boundaries to get the latest coalesced motion
    pub fn flush_motion(&mut self) -> Option<MotionEvent> {
        // If there's a pending event, emit it
        if let Some(event) = self.pending.take() {
            self.motion_queue.clear();
            self.last_motion = Some(event);
            self.last_emitted = Some(event);
            Some(event)
        } else if let Some(event) = self.motion_queue.pop_back() {
            self.motion_queue.clear();
            self.last_motion = Some(event);
            self.last_emitted = Some(event);
            Some(event)
        } else {
            None
        }
    }

    /// Get the last recorded motion without flushing
    pub fn last_motion(&self) -> Option<MotionEvent> {
        self.last_motion
    }

    /// Record a geometry update event, returns Some if should be processed immediately
    /// Returns None if the event should be coalesced with the next one
    pub fn coalesce_geometry(&mut self, x: i32, y: i32, width: u32, height: u32) -> Option<GeometryEvent> {
        let now = Instant::now();
        let event = GeometryEvent { x, y, width, height };

        // Check if we should emit this geometry event
        if let Some(last_time) = self.last_geometry_emitted {
            let dt = now.duration_since(last_time);

            // If within time window, store as pending but don't emit
            if dt < self.geometry_time_threshold {
                self.pending_geometry = Some(event);
                return None;
            }
        }

        // Emit this event
        self.last_geometry_emitted = Some(now);
        self.pending_geometry = None;
        Some(event)
    }

    /// Get the most recent geometry event, clearing pending
    /// Call this at frame boundaries to get the latest coalesced geometry
    pub fn flush_geometry(&mut self) -> Option<GeometryEvent> {
        if let Some(event) = self.pending_geometry.take() {
            self.last_geometry_emitted = Some(Instant::now());
            Some(event)
        } else {
            None
        }
    }

    /// Clear all coalesced geometry events
    pub fn clear_geometry(&mut self) {
        self.pending_geometry = None;
        self.last_geometry_emitted = None;
    }

    /// Clear all coalesced events
    pub fn clear(&mut self) {
        self.motion_queue.clear();
        self.last_motion = None;
        self.last_emitted = None;
        self.pending = None;
        self.clear_geometry();
        self.clear_properties();
        self.clear_exposes();
    }

    /// Get queue size (for debugging)
    pub fn queue_size(&self) -> usize {
        self.motion_queue.len()
    }

    // ========== Property Event Coalescing ==========

    /// Record a property change event
    /// Returns Some if should be processed immediately, None if coalesced
    pub fn coalesce_property(&mut self, window: u64, atom: u32) -> Option<PropertyEvent> {
        let now = Instant::now();
        let key = PropertyKey { window, atom };
        let event = PropertyEvent { window, atom, time: now };

        // Check if we have a recent pending event for this window+atom
        if let Some(pending) = self.pending_properties.get(&key) {
            let dt = now.duration_since(pending.time);
            if dt < self.property_time_threshold {
                // Update the pending event timestamp
                self.pending_properties.insert(key, event);
                return None;
            }
        }

        // Emit this event and clear any pending
        self.pending_properties.remove(&key);
        Some(event)
    }

    /// Flush all pending property events
    /// Returns iterator over (window, atom) pairs that need processing
    pub fn flush_properties(&mut self) -> Vec<(u64, u32)> {
        let result: Vec<(u64, u32)> = self
            .pending_properties
            .iter()
            .map(|(k, _)| (k.window, k.atom))
            .collect();
        self.pending_properties.clear();
        result
    }

    /// Clear all pending property events
    pub fn clear_properties(&mut self) {
        self.pending_properties.clear();
    }

    // ========== Expose Event Coalescing ==========

    /// Record an expose/damage event
    /// Returns Some(merged_rect) if should be processed immediately, None if coalesced
    pub fn coalesce_expose(&mut self, window: u64, x: i32, y: i32, width: u32, height: u32) -> Option<ExposeRect> {
        let rect = ExposeRect { x, y, width, height };

        // Merge with pending expose for this window
        let merged = if let Some(pending) = self.pending_exposes.get(&window) {
            pending.merge(&rect)
        } else {
            rect
        };

        self.pending_exposes.insert(window, merged);

        // Return None to indicate coalesced - caller should flush_exposes() later
        None
    }

    /// Flush all pending expose events
    /// Returns map of window -> merged expose rect
    pub fn flush_exposes(&mut self) -> HashMap<u64, ExposeRect> {
        std::mem::take(&mut self.pending_exposes)
    }

    /// Clear all pending expose events
    pub fn clear_exposes(&mut self) {
        self.pending_exposes.clear();
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
    use std::thread::sleep;

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

    #[test]
    fn test_time_based_coalescing() {
        let mut coalescer = EventCoalescer::with_thresholds(50, 10);

        // First event passes through
        let e1 = coalescer.coalesce_motion(100, 100);
        assert!(e1.is_some());

        // Second event within time window - coalesced
        let e2 = coalescer.coalesce_motion(101, 101);
        assert!(e2.is_none());

        // Wait for time window to expire
        sleep(Duration::from_millis(60));

        // Third event after time window - passes through
        let e3 = coalescer.coalesce_motion(102, 102);
        assert!(e3.is_some());
    }

    #[test]
    fn test_distance_based_coalescing() {
        let mut coalescer = EventCoalescer::with_thresholds(1000, 2);

        // First event passes through
        let e1 = coalescer.coalesce_motion(100, 100);
        assert!(e1.is_some());

        // Second event within distance threshold (1px) - coalesced
        let e2 = coalescer.coalesce_motion(101, 100);
        assert!(e2.is_none());

        // Third event beyond distance threshold (50px) - passes through
        let e3 = coalescer.coalesce_motion(150, 100);
        assert!(e3.is_some());
    }

    #[test]
    fn test_pending_flush() {
        let mut coalescer = EventCoalescer::with_thresholds(50, 10);

        // First event passes through
        coalescer.coalesce_motion(100, 100);

        // Second event coalesced (pending)
        let e2 = coalescer.coalesce_motion(101, 101);
        assert!(e2.is_none());

        // Flush should return the pending event
        let flushed = coalescer.flush_motion();
        assert!(flushed.is_some());
        assert_eq!(flushed.unwrap().x, 101);
    }

    #[test]
    fn test_geometry_coalescing() {
        let mut coalescer = EventCoalescer::new();

        // First geometry event passes through
        let g1 = coalescer.coalesce_geometry(100, 100, 300, 50);
        assert!(g1.is_some());

        // Second event within time window - coalesced
        let g2 = coalescer.coalesce_geometry(100, 100, 310, 50);
        assert!(g2.is_none());

        // Flush should return the pending event
        let flushed = coalescer.flush_geometry();
        assert!(flushed.is_some());
        let evt = flushed.unwrap();
        assert_eq!(evt.width, 310);
    }

    #[test]
    fn test_geometry_time_based_coalescing() {
        let mut coalescer = EventCoalescer::with_thresholds(50, 10);

        // First event passes through
        let g1 = coalescer.coalesce_geometry(100, 100, 300, 50);
        assert!(g1.is_some());

        // Second event within time window - coalesced
        let g2 = coalescer.coalesce_geometry(100, 100, 320, 50);
        assert!(g2.is_none());

        // Wait for time window to expire
        sleep(Duration::from_millis(60));

        // Third event after time window - passes through
        let g3 = coalescer.coalesce_geometry(100, 100, 340, 50);
        assert!(g3.is_some());
    }

    #[test]
    fn test_geometry_clear() {
        let mut coalescer = EventCoalescer::new();

        coalescer.coalesce_geometry(100, 100, 300, 50);
        let g2 = coalescer.coalesce_geometry(100, 100, 310, 50);
        assert!(g2.is_none());

        coalescer.clear_geometry();
        assert!(coalescer.flush_geometry().is_none());
    }
}
