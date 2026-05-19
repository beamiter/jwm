/// Pixel buffer pool for reusing memory allocations
use std::collections::HashMap;
use std::sync::{Mutex, Arc};

/// Manages reusable pixel buffers to reduce allocation overhead
pub struct PixelBufferPool {
    buffers: Arc<Mutex<HashMap<usize, Vec<Vec<u8>>>>>,
    stats: Arc<Mutex<PixelBufferStats>>,
}

#[derive(Clone, Default, Debug)]
pub struct PixelBufferStats {
    pub allocations: usize,
    pub reuses: usize,
    pub total_bytes: usize,
}

impl PixelBufferPool {
    pub fn new() -> Self {
        Self {
            buffers: Arc::new(Mutex::new(HashMap::new())),
            stats: Arc::new(Mutex::new(PixelBufferStats::default())),
        }
    }

    /// Acquire or create a buffer of the given size
    pub fn acquire(&self, size: usize) -> Vec<u8> {
        if let Ok(mut buffers) = self.buffers.lock() {
            if let Some(pool) = buffers.get_mut(&size) {
                if let Some(buffer) = pool.pop() {
                    if let Ok(mut stats) = self.stats.lock() {
                        stats.reuses += 1;
                    }
                    return buffer;
                }
            }
        }

        // Allocate new buffer
        if let Ok(mut stats) = self.stats.lock() {
            stats.allocations += 1;
            stats.total_bytes += size;
        }

        vec![0u8; size]
    }

    /// Release a buffer back to the pool for reuse
    pub fn release(&self, buffer: Vec<u8>) {
        let size = buffer.capacity();
        if let Ok(mut buffers) = self.buffers.lock() {
            buffers.entry(size)
                .or_insert_with(Vec::new)
                .push(buffer);
        }
    }

    /// Clear all pooled buffers
    pub fn clear(&self) {
        if let Ok(mut buffers) = self.buffers.lock() {
            buffers.clear();
        }
        if let Ok(mut stats) = self.stats.lock() {
            stats.total_bytes = 0;
        }
    }

    /// Get pool statistics
    pub fn stats(&self) -> PixelBufferStats {
        self.stats.lock()
            .ok()
            .map(|s| s.clone())
            .unwrap_or_default()
    }

    /// Get number of available buffers for a given size
    pub fn available_for_size(&self, size: usize) -> usize {
        self.buffers.lock()
            .ok()
            .and_then(|b| Some(b.get(&size)?.len()))
            .unwrap_or(0)
    }

    /// Get total number of buffered items
    pub fn total_buffered(&self) -> usize {
        self.buffers.lock()
            .ok()
            .map(|b| b.values().map(|v| v.len()).sum())
            .unwrap_or(0)
    }
}

impl Clone for PixelBufferPool {
    fn clone(&self) -> Self {
        Self {
            buffers: self.buffers.clone(),
            stats: self.stats.clone(),
        }
    }
}

impl Default for PixelBufferPool {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_pool_is_empty() {
        let pool = PixelBufferPool::new();
        assert_eq!(pool.total_buffered(), 0);
        let stats = pool.stats();
        assert_eq!(stats.allocations, 0);
        assert_eq!(stats.reuses, 0);
        assert_eq!(stats.total_bytes, 0);
    }

    #[test]
    fn test_acquire_allocates_new_buffer() {
        let pool = PixelBufferPool::new();
        let buf = pool.acquire(64);
        assert_eq!(buf.len(), 64);
        assert!(buf.iter().all(|&b| b == 0), "new buffer should be zeroed");
        let stats = pool.stats();
        assert_eq!(stats.allocations, 1);
        assert_eq!(stats.total_bytes, 64);
        assert_eq!(stats.reuses, 0);
    }

    #[test]
    fn test_release_and_reuse() {
        let pool = PixelBufferPool::new();
        let buf = pool.acquire(128);
        pool.release(buf);
        assert_eq!(pool.available_for_size(128), 1);

        let _buf2 = pool.acquire(128);
        let stats = pool.stats();
        assert_eq!(stats.reuses, 1);
        assert_eq!(stats.allocations, 1);
    }

    #[test]
    fn test_multiple_sizes_independent() {
        let pool = PixelBufferPool::new();
        let b1 = pool.acquire(64);
        let b2 = pool.acquire(128);
        pool.release(b1);
        pool.release(b2);

        assert_eq!(pool.available_for_size(64), 1);
        assert_eq!(pool.available_for_size(128), 1);
        assert_eq!(pool.total_buffered(), 2);
    }

    #[test]
    fn test_available_for_size_empty() {
        let pool = PixelBufferPool::new();
        assert_eq!(pool.available_for_size(256), 0);
    }

    #[test]
    fn test_total_buffered_accumulates() {
        let pool = PixelBufferPool::new();
        // Acquire 5 buffers, hold them all, then release all 5
        let bufs: Vec<_> = (0..5).map(|_| pool.acquire(32)).collect();
        for b in bufs {
            pool.release(b);
        }
        assert_eq!(pool.total_buffered(), 5);
    }

    #[test]
    fn test_reuse_drains_pool() {
        let pool = PixelBufferPool::new();
        // Put 3 distinct buffers in the pool by holding them simultaneously
        let b1 = pool.acquire(16);
        let b2 = pool.acquire(16);
        let b3 = pool.acquire(16);
        pool.release(b1);
        pool.release(b2);
        pool.release(b3);
        assert_eq!(pool.available_for_size(16), 3);

        let _b1 = pool.acquire(16);
        assert_eq!(pool.available_for_size(16), 2);
        let _b2 = pool.acquire(16);
        assert_eq!(pool.available_for_size(16), 1);
        let _b3 = pool.acquire(16);
        assert_eq!(pool.available_for_size(16), 0);

        // Next acquire should allocate fresh
        let _b4 = pool.acquire(16);
        let stats = pool.stats();
        assert_eq!(stats.allocations, 4); // 3 original + 1 fresh
        assert_eq!(stats.reuses, 3);
    }

    #[test]
    fn test_clear_removes_all_buffers() {
        let pool = PixelBufferPool::new();
        pool.release(pool.acquire(64));
        pool.release(pool.acquire(128));
        assert_eq!(pool.total_buffered(), 2);

        pool.clear();
        assert_eq!(pool.total_buffered(), 0);
        assert_eq!(pool.available_for_size(64), 0);
        assert_eq!(pool.available_for_size(128), 0);
    }

    #[test]
    fn test_clear_resets_total_bytes_in_stats() {
        let pool = PixelBufferPool::new();
        pool.acquire(100); // counts towards total_bytes
        pool.clear();
        let stats = pool.stats();
        assert_eq!(stats.total_bytes, 0);
    }

    #[test]
    fn test_clone_shares_underlying_state() {
        let pool = PixelBufferPool::new();
        let pool2 = pool.clone();

        pool.release(pool.acquire(32));
        // Both clones see the same pool
        assert_eq!(pool2.available_for_size(32), 1);
    }

    #[test]
    fn test_stats_allocation_count_matches_acquires() {
        let pool = PixelBufferPool::new();
        for _ in 0..10 {
            pool.acquire(8);
        }
        assert_eq!(pool.stats().allocations, 10);
    }

    #[test]
    fn test_zero_size_buffer() {
        let pool = PixelBufferPool::new();
        let buf = pool.acquire(0);
        assert_eq!(buf.len(), 0);
        pool.release(buf);
        // Zero-size buffer can be pooled without panic
        assert_eq!(pool.available_for_size(0), 1);
    }

    #[test]
    fn test_default_equals_new() {
        let p1 = PixelBufferPool::new();
        let p2 = PixelBufferPool::default();
        assert_eq!(p1.total_buffered(), p2.total_buffered());
        assert_eq!(p1.stats().allocations, p2.stats().allocations);
    }
}
