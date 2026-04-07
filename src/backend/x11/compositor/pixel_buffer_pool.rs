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
