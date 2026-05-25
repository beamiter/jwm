use std::collections::HashMap;

/// Statistics for pixel buffer pool usage.
#[derive(Debug, Clone, Default)]
pub struct PixelBufferStats {
    pub allocations: u64,
    pub reuses: u64,
    pub total_bytes: usize,
}

/// CPU-side `Vec<u8>` buffer pool for reuse (screenshot readback, overview snapshots).
///
/// Buffers are keyed by their capacity so that a released buffer can be reused
/// by a subsequent acquire of the same (or smaller) size without reallocating.
pub struct PixelBufferPool {
    pool: HashMap<usize, Vec<Vec<u8>>>,
    stats: PixelBufferStats,
    max_pool_size: usize,
}

impl PixelBufferPool {
    /// Create a new pool with the default maximum of 16 buffers per size.
    pub fn new() -> Self {
        Self {
            pool: HashMap::new(),
            stats: PixelBufferStats::default(),
            max_pool_size: 16,
        }
    }

    /// Create a new pool with a custom maximum number of buffers per size.
    pub fn with_max_pool_size(max: usize) -> Self {
        Self {
            pool: HashMap::new(),
            stats: PixelBufferStats::default(),
            max_pool_size: max,
        }
    }

    /// Acquire a buffer of at least `size` bytes.
    ///
    /// If a pooled buffer with matching capacity is available it is reused;
    /// otherwise a new zeroed buffer is allocated.
    pub fn acquire(&mut self, size: usize) -> Vec<u8> {
        if let Some(buffers) = self.pool.get_mut(&size) {
            if let Some(mut buf) = buffers.pop() {
                self.stats.reuses += 1;
                // Clear contents but keep allocation.
                buf.clear();
                buf.resize(size, 0);
                return buf;
            }
        }

        self.stats.allocations += 1;
        self.stats.total_bytes += size;
        vec![0u8; size]
    }

    /// Return a buffer to the pool for later reuse.
    ///
    /// The buffer is keyed by its current capacity. If the pool for that
    /// capacity is already at maximum size the buffer is simply dropped.
    pub fn release(&mut self, buffer: Vec<u8>) {
        let capacity = buffer.capacity();
        let buffers = self.pool.entry(capacity).or_insert_with(Vec::new);
        if buffers.len() < self.max_pool_size {
            buffers.push(buffer);
        }
        // else: drop the buffer
    }

    /// Remove all pooled buffers, freeing memory.
    pub fn clear(&mut self) {
        self.pool.clear();
    }

    /// Return a reference to the current pool statistics.
    pub fn stats(&self) -> &PixelBufferStats {
        &self.stats
    }

    /// Return the number of pooled buffers available for the given size.
    pub fn available_for_size(&self, size: usize) -> usize {
        self.pool.get(&size).map_or(0, |v| v.len())
    }

    /// Return the total number of bytes held across all pooled buffers.
    pub fn total_buffered(&self) -> usize {
        self.pool
            .iter()
            .map(|(cap, bufs)| cap * bufs.len())
            .sum()
    }
}

impl Default for PixelBufferPool {
    fn default() -> Self {
        Self::new()
    }
}
