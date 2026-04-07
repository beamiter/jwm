/// X11 Request Batching - Reduces flush() calls for better performance
///
/// Instead of flushing after every configure/property operation,
/// batch operations and flush periodically to reduce X11 round-trips.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::sync::Mutex;
use x11rb::connection::Connection;
use crate::backend::error::BackendError;

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
            flush_op_threshold: Arc::new(AtomicU32::new(8)),     // Flush after 8 queued operations
            flush_time_threshold_ms: Arc::new(std::sync::atomic::AtomicU64::new(8)), // OR after 8ms
            system_load: Arc::new(AtomicU32::new(50)),
        }
    }

    /// Record an operation and maybe flush
    pub fn mark_op<C: Connection>(&self, conn: &C) -> Result<(), BackendError> {
        let count = self.pending_ops.fetch_add(1, Ordering::SeqCst);
        let threshold = self.flush_op_threshold.load(Ordering::Relaxed);

        // Check if we should flush
        let should_flush = if count > 0 && count % threshold == 0 {
            // Operation threshold reached
            true
        } else if let Ok(last) = self.last_flush.lock() {
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
