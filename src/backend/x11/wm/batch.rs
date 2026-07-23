//! Protocol-free flush-batching coordinator shared by the X11RB and XCB
//! transports.
//!
//! Both transports reduce X11 round-trips by batching configure/property
//! operations and flushing periodically instead of after every request. The
//! *decision* — how many queued operations or how much elapsed time triggers
//! a flush, and how those thresholds adapt to system load — is identical on
//! both and touches no protocol types. It lives here so the two backends
//! cannot drift; each transport keeps only its own `conn.flush()` call.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Default flush thresholds: flush after 8 queued operations OR 8 ms.
const DEFAULT_OP_THRESHOLD: u32 = 8;
const DEFAULT_TIME_THRESHOLD_MS: u64 = 8;
/// Neutral starting load estimate (0-100).
const DEFAULT_SYSTEM_LOAD: u32 = 50;

/// Shared, cheaply-clonable batching state. A clone shares the same atomic
/// counters (the transports hand clones to worker threads), matching the
/// `Arc`-wrapped fields the two backends previously duplicated.
pub struct BatchCounters {
    /// Count of operations queued since the last flush.
    pending_ops: Arc<AtomicU32>,
    /// When the last flush happened, for the time-based threshold.
    last_flush: Arc<Mutex<Instant>>,
    /// Flush once this many operations are queued (0 disables the count rule).
    flush_op_threshold: Arc<AtomicU32>,
    /// Flush once this many milliseconds have elapsed since the last flush.
    flush_time_threshold_ms: Arc<AtomicU64>,
    /// Most recent system-load estimate (0-100), higher means busier.
    system_load: Arc<AtomicU32>,
}

impl BatchCounters {
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending_ops: Arc::new(AtomicU32::new(0)),
            last_flush: Arc::new(Mutex::new(Instant::now())),
            flush_op_threshold: Arc::new(AtomicU32::new(DEFAULT_OP_THRESHOLD)),
            flush_time_threshold_ms: Arc::new(AtomicU64::new(DEFAULT_TIME_THRESHOLD_MS)),
            system_load: Arc::new(AtomicU32::new(DEFAULT_SYSTEM_LOAD)),
        }
    }

    /// Record one queued operation and report whether the caller should flush
    /// now. The transport performs the actual `conn.flush()` and then calls
    /// [`Self::on_flushed`]; this keeps the protocol call in the backend while
    /// the batching policy stays here.
    ///
    /// A flush is due when the queued-operation threshold is reached, or —
    /// checked only on the first operation and every fourth after that, to
    /// avoid locking the timestamp on every call — when the time threshold has
    /// elapsed. `threshold = 8` flushes the eighth queued operation, not the
    /// ninth. `AcqRel` on the counter is sufficient: it drives batching only
    /// and never publishes request payloads.
    #[must_use]
    pub fn note_op(&self) -> bool {
        let count = self.pending_ops.fetch_add(1, Ordering::AcqRel) + 1;
        let threshold = self.flush_op_threshold.load(Ordering::Relaxed);
        if threshold > 0 && count >= threshold {
            true
        } else if count == 1 || count % 4 == 0 {
            let timeout_ms = self.flush_time_threshold_ms.load(Ordering::Relaxed);
            self.last_flush
                .lock()
                .map(|last| last.elapsed() > Duration::from_millis(timeout_ms))
                .unwrap_or(false)
        } else {
            false
        }
    }

    /// Reset the batch after the transport has flushed the connection.
    pub fn on_flushed(&self) {
        self.pending_ops.store(0, Ordering::Release);
        if let Ok(mut last) = self.last_flush.lock() {
            *last = Instant::now();
        }
    }

    /// Operations queued since the last flush (diagnostics).
    #[must_use]
    pub fn pending_count(&self) -> u32 {
        self.pending_ops.load(Ordering::Acquire)
    }

    /// Adapt the thresholds to the current system load (0-100). A busier
    /// system batches more aggressively to cut overhead; an idle system
    /// flushes sooner for lower latency.
    pub fn adjust_thresholds(&self, load: u32) {
        self.system_load.store(load.min(100), Ordering::Relaxed);
        let (ops, time_ms) = if load > 80 {
            (16, 16)
        } else if load > 60 {
            (12, 12)
        } else if load < 30 {
            (4, 4)
        } else {
            (DEFAULT_OP_THRESHOLD, DEFAULT_TIME_THRESHOLD_MS)
        };
        self.flush_op_threshold.store(ops, Ordering::Release);
        self.flush_time_threshold_ms
            .store(time_ms, Ordering::Release);
    }

    /// Most recent system-load estimate.
    #[must_use]
    pub fn system_load(&self) -> u32 {
        self.system_load.load(Ordering::Relaxed)
    }
}

impl Clone for BatchCounters {
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

impl Default for BatchCounters {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flushes_on_the_nth_queued_operation() {
        let counters = BatchCounters::new();
        // Isolate the count rule: push the time threshold out of reach so
        // timing on the machine cannot make an earlier op flush.
        counters
            .flush_time_threshold_ms
            .store(u64::MAX, Ordering::Release);
        // Counts 1..=7 stay below the default threshold of 8.
        for _ in 0..7 {
            assert!(!counters.note_op());
        }
        // The eighth queued operation reaches threshold 8 and flushes.
        assert!(counters.note_op());
        counters.on_flushed();
        assert_eq!(counters.pending_count(), 0);
    }

    #[test]
    fn on_flushed_clears_the_pending_count() {
        let counters = BatchCounters::new();
        let _ = counters.note_op();
        let _ = counters.note_op();
        assert!(counters.pending_count() >= 1);
        counters.on_flushed();
        assert_eq!(counters.pending_count(), 0);
    }

    #[test]
    fn thresholds_track_load_bands() {
        let counters = BatchCounters::new();

        counters.adjust_thresholds(90);
        assert_eq!(counters.system_load(), 90);
        assert_eq!(counters.flush_op_threshold.load(Ordering::Acquire), 16);

        counters.adjust_thresholds(70);
        assert_eq!(counters.flush_op_threshold.load(Ordering::Acquire), 12);

        counters.adjust_thresholds(10);
        assert_eq!(counters.flush_op_threshold.load(Ordering::Acquire), 4);

        counters.adjust_thresholds(50);
        assert_eq!(
            counters.flush_op_threshold.load(Ordering::Acquire),
            DEFAULT_OP_THRESHOLD
        );

        // Out-of-range loads are clamped for reporting.
        counters.adjust_thresholds(250);
        assert_eq!(counters.system_load(), 100);
    }

    #[test]
    fn a_disabled_count_threshold_falls_back_to_the_time_rule() {
        let counters = BatchCounters::new();
        counters.flush_op_threshold.store(0, Ordering::Release);
        counters.flush_time_threshold_ms.store(0, Ordering::Release);
        // With a zero time threshold, any elapsed time counts, so the first
        // operation (always time-checked) flushes even with the count rule off.
        std::thread::sleep(Duration::from_millis(1));
        assert!(counters.note_op());
    }

    #[test]
    fn clones_share_one_counter() {
        let counters = BatchCounters::new();
        let clone = counters.clone();
        let _ = counters.note_op();
        let _ = counters.note_op();
        // The clone observes the same atomic state.
        assert_eq!(clone.pending_count(), counters.pending_count());
        clone.on_flushed();
        assert_eq!(counters.pending_count(), 0);
    }
}
