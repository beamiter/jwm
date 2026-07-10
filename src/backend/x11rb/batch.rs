use crate::backend::error::BackendError;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
/// X11 Request Batching - Reduces flush() calls for better performance
///
/// Instead of flushing after every configure/property operation,
/// batch operations and flush periodically to reduce X11 round-trips.
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;

/// Batches X11 requests and flushes intelligently
pub struct X11RequestBatcher {
    /// Count of pending operations
    pending_ops: Arc<AtomicU32>,
    /// Last flush time
    last_flush: Arc<Mutex<Instant>>,
    /// Threshold: flush after N operations OR M milliseconds
    flush_op_threshold: Arc<AtomicU32>,
    flush_time_threshold_ms: Arc<std::sync::atomic::AtomicU64>,
    /// CPU/GPU load estimation (0-100)
    system_load: Arc<AtomicU32>,
}

impl X11RequestBatcher {
    pub fn new() -> Self {
        Self {
            pending_ops: Arc::new(AtomicU32::new(0)),
            last_flush: Arc::new(Mutex::new(Instant::now())),
            flush_op_threshold: Arc::new(AtomicU32::new(8)), // Flush after 8 queued operations
            flush_time_threshold_ms: Arc::new(std::sync::atomic::AtomicU64::new(8)), // OR after 8ms
            system_load: Arc::new(AtomicU32::new(50)),
        }
    }

    /// Record an operation and maybe flush
    pub fn mark_op<C: Connection>(&self, conn: &C) -> Result<(), BackendError> {
        // Keep the batching semantics identical to the native XCB backend:
        // `threshold = 8` flushes the eighth queued operation, not the
        // ninth. AcqRel is sufficient here; the counter is only used for
        // batching decisions and does not publish request payloads.
        let count = self.pending_ops.fetch_add(1, Ordering::AcqRel) + 1;
        let threshold = self.flush_op_threshold.load(Ordering::Relaxed);

        // Check if we should flush
        let should_flush = if threshold > 0 && count >= threshold {
            // Operation threshold reached
            true
        } else if (count == 1 || count % 4 == 0)
            && let Ok(last) = self.last_flush.lock()
        {
            // Time threshold reached?
            let timeout_ms = self.flush_time_threshold_ms.load(Ordering::Relaxed);
            last.elapsed() > Duration::from_millis(timeout_ms)
        } else {
            false
        };

        if should_flush {
            self.do_flush(conn)?;
        }

        Ok(())
    }

    /// Force a flush
    pub fn flush<C: Connection>(&self, conn: &C) -> Result<(), BackendError> {
        self.do_flush(conn)?;
        Ok(())
    }

    fn do_flush<C: Connection>(&self, conn: &C) -> Result<(), BackendError> {
        conn.flush()?;
        self.pending_ops.store(0, Ordering::SeqCst);
        if let Ok(mut last) = self.last_flush.lock() {
            *last = Instant::now();
        }
        Ok(())
    }

    /// Get pending operations count (for debugging)
    #[allow(dead_code)]
    pub fn pending_count(&self) -> u32 {
        self.pending_ops.load(Ordering::Acquire)
    }

    /// Adjust batch thresholds based on system load
    /// load: 0-100, higher means busier system
    pub fn adjust_thresholds(&self, load: u32) {
        self.system_load.store(load.min(100), Ordering::Relaxed);

        if load > 80 {
            // High load: batch more to reduce overhead
            self.flush_op_threshold.store(16, Ordering::Release);
            self.flush_time_threshold_ms.store(16, Ordering::Release);
        } else if load > 60 {
            self.flush_op_threshold.store(12, Ordering::Release);
            self.flush_time_threshold_ms.store(12, Ordering::Release);
        } else if load < 30 {
            // Low load: respond more quickly
            self.flush_op_threshold.store(4, Ordering::Release);
            self.flush_time_threshold_ms.store(4, Ordering::Release);
        } else {
            // Normal load: default thresholds
            self.flush_op_threshold.store(8, Ordering::Release);
            self.flush_time_threshold_ms.store(8, Ordering::Release);
        }
    }

    /// Get current system load estimate
    pub fn system_load(&self) -> u32 {
        self.system_load.load(Ordering::Relaxed)
    }
}

impl Clone for X11RequestBatcher {
    fn clone(&self) -> Self {
        Self {
            pending_ops: self.pending_ops.clone(),
            last_flush: self.last_flush.clone(),
            flush_op_threshold: self.flush_op_threshold.clone(),
            flush_time_threshold_ms: self.flush_time_threshold_ms.clone(),
            system_load: self.system_load.clone(),
        }
    }
}

impl Default for X11RequestBatcher {
    fn default() -> Self {
        Self::new()
    }
}

// ===============================================================
// Batch Geometry Requests - Query multiple window geometries in one round-trip
// ===============================================================

/// Batched geometry request handler
/// This uses a different approach: queue windows, then send all requests and collect replies
pub struct BatchedGeometryRequest<'a, C: Connection> {
    conn: &'a C,
    windows: Vec<u32>,
}

impl<'a, C: Connection> BatchedGeometryRequest<'a, C> {
    pub fn new(conn: &'a C) -> Self {
        Self {
            conn,
            windows: Vec::new(),
        }
    }

    /// Queue a geometry request for a window
    pub fn queue_geometry(&mut self, window: u32) {
        self.windows.push(window);
    }

    /// Send all requests and collect results
    /// Returns map of window -> (x, y, width, height)
    pub fn flush_and_collect(self) -> Result<HashMap<u32, (i16, i16, u16, u16)>, BackendError> {
        let mut cookies = Vec::with_capacity(self.windows.len());

        // Send all requests
        for &window in &self.windows {
            cookies.push((window, self.conn.get_geometry(window)?));
        }

        // Flush to send all requests at once
        self.conn.flush()?;

        // Collect all replies
        let mut results = HashMap::new();
        for (window, cookie) in cookies {
            match cookie.reply() {
                Ok(reply) => {
                    results.insert(window, (reply.x, reply.y, reply.width, reply.height));
                }
                Err(e) => {
                    log::debug!("Failed to get geometry for window 0x{:x}: {}", window, e);
                }
            }
        }
        Ok(results)
    }

    /// Get number of pending requests
    pub fn pending_count(&self) -> usize {
        self.windows.len()
    }
}

// ===============================================================
// Batch Property Requests - Query multiple window properties in one round-trip
// ===============================================================

type PropertyKey = (u32, Atom); // (window, atom)

#[derive(Clone, Copy)]
pub struct PropertyQuery {
    pub window: u32,
    pub atom: Atom,
    pub prop_type: Atom,
    pub max_len: u32,
}

/// Batched property request handler
pub struct BatchedPropertyRequest<'a, C: Connection> {
    conn: &'a C,
    queries: Vec<PropertyQuery>,
}

impl<'a, C: Connection> BatchedPropertyRequest<'a, C> {
    pub fn new(conn: &'a C) -> Self {
        Self {
            conn,
            queries: Vec::new(),
        }
    }

    /// Queue a property request for a window
    pub fn queue_property(&mut self, window: u32, atom: Atom, prop_type: Atom, max_len: u32) {
        self.queries.push(PropertyQuery {
            window,
            atom,
            prop_type,
            max_len,
        });
    }

    /// Send all requests and collect results
    /// Returns map of (window, atom) -> property value bytes
    pub fn flush_and_collect(self) -> Result<HashMap<PropertyKey, Vec<u8>>, BackendError> {
        let mut cookies = Vec::with_capacity(self.queries.len());

        // Send all requests
        for query in &self.queries {
            let cookie = self.conn.get_property(
                false,
                query.window,
                query.atom,
                query.prop_type,
                0,
                query.max_len,
            )?;
            cookies.push(((query.window, query.atom), cookie));
        }

        // Flush to send all at once
        self.conn.flush()?;

        // Collect all replies
        let mut results = HashMap::new();
        for (key, cookie) in cookies {
            match cookie.reply() {
                Ok(reply) => {
                    results.insert(key, reply.value);
                }
                Err(e) => {
                    log::debug!(
                        "Failed to get property for window 0x{:x} atom {}: {}",
                        key.0,
                        key.1,
                        e
                    );
                }
            }
        }
        Ok(results)
    }

    /// Get number of pending requests
    pub fn pending_count(&self) -> usize {
        self.queries.len()
    }
}

// ===============================================================
// Batch Window Attributes - Query multiple window attributes in one round-trip
// ===============================================================

pub struct BatchedAttributesRequest<'a, C: Connection> {
    conn: &'a C,
    windows: Vec<u32>,
}

impl<'a, C: Connection> BatchedAttributesRequest<'a, C> {
    pub fn new(conn: &'a C) -> Self {
        Self {
            conn,
            windows: Vec::new(),
        }
    }

    /// Queue a window attributes request
    pub fn queue_attributes(&mut self, window: u32) {
        self.windows.push(window);
    }

    /// Send all requests and collect results
    pub fn flush_and_collect(self) -> Result<HashMap<u32, GetWindowAttributesReply>, BackendError> {
        let mut cookies = Vec::with_capacity(self.windows.len());

        // Send all requests
        for &window in &self.windows {
            cookies.push((window, self.conn.get_window_attributes(window)?));
        }

        // Flush to send all at once
        self.conn.flush()?;

        // Collect all replies
        let mut results = HashMap::new();
        for (window, cookie) in cookies {
            match cookie.reply() {
                Ok(reply) => {
                    results.insert(window, reply);
                }
                Err(e) => {
                    log::debug!("Failed to get attributes for window 0x{:x}: {}", window, e);
                }
            }
        }
        Ok(results)
    }

    /// Get number of pending requests
    pub fn pending_count(&self) -> usize {
        self.windows.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_batcher_creation() {
        let batcher = X11RequestBatcher::new();
        assert_eq!(batcher.pending_count(), 0);
        assert_eq!(batcher.system_load(), 50);
    }

    #[test]
    fn test_pending_ops_count() {
        let batcher = X11RequestBatcher::new();
        assert_eq!(batcher.pending_count(), 0);

        batcher.pending_ops.fetch_add(5, Ordering::SeqCst);
        assert_eq!(batcher.pending_count(), 5);
    }

    #[test]
    fn test_load_adjustment_high_load() {
        let batcher = X11RequestBatcher::new();

        batcher.adjust_thresholds(85);
        assert_eq!(batcher.system_load(), 85);
        assert_eq!(
            batcher.flush_op_threshold.load(Ordering::Acquire),
            16,
            "High load should increase operation threshold"
        );
        assert_eq!(
            batcher.flush_time_threshold_ms.load(Ordering::Acquire),
            16,
            "High load should increase time threshold"
        );
    }

    #[test]
    fn test_load_adjustment_medium_load() {
        let batcher = X11RequestBatcher::new();

        batcher.adjust_thresholds(70);
        assert_eq!(batcher.system_load(), 70);
        assert_eq!(
            batcher.flush_op_threshold.load(Ordering::Acquire),
            12,
            "Medium load should adjust operation threshold"
        );
        assert_eq!(
            batcher.flush_time_threshold_ms.load(Ordering::Acquire),
            12,
            "Medium load should adjust time threshold"
        );
    }

    #[test]
    fn test_load_adjustment_low_load() {
        let batcher = X11RequestBatcher::new();

        batcher.adjust_thresholds(20);
        assert_eq!(batcher.system_load(), 20);
        assert_eq!(
            batcher.flush_op_threshold.load(Ordering::Acquire),
            4,
            "Low load should decrease operation threshold"
        );
        assert_eq!(
            batcher.flush_time_threshold_ms.load(Ordering::Acquire),
            4,
            "Low load should decrease time threshold"
        );
    }

    #[test]
    fn test_load_adjustment_clamping() {
        let batcher = X11RequestBatcher::new();

        batcher.adjust_thresholds(150);
        assert_eq!(batcher.system_load(), 100, "Load should be clamped to 100");
    }

    #[test]
    fn test_load_adjustment_normal_load() {
        let batcher = X11RequestBatcher::new();

        batcher.adjust_thresholds(50);
        assert_eq!(batcher.system_load(), 50);
        assert_eq!(
            batcher.flush_op_threshold.load(Ordering::Acquire),
            8,
            "Normal load should use default thresholds"
        );
    }

    #[test]
    fn test_batcher_clone() {
        let batcher = X11RequestBatcher::new();
        batcher.pending_ops.fetch_add(3, Ordering::SeqCst);

        let cloned = batcher.clone();
        assert_eq!(cloned.pending_count(), 3, "Clone should share state");

        cloned.pending_ops.fetch_add(2, Ordering::SeqCst);
        assert_eq!(
            batcher.pending_count(),
            5,
            "Both instances should see the same pending count"
        );
    }

    #[test]
    fn test_batcher_default() {
        let batcher = X11RequestBatcher::default();
        assert_eq!(batcher.pending_count(), 0);
        assert_eq!(batcher.system_load(), 50);
    }
}
