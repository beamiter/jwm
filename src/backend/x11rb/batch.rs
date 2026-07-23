use crate::backend::error::BackendError;
use crate::backend::x11::wm::batch::BatchCounters;
use std::collections::HashMap;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;

/// Batches X11 requests and flushes intelligently.
///
/// The batching policy (thresholds, timing, load adaptation) lives in the
/// shared, transport-neutral [`BatchCounters`]; this wrapper only performs
/// the x11rb-specific `conn.flush()`.
#[derive(Clone, Default)]
pub struct X11RequestBatcher {
    counters: BatchCounters,
}

impl X11RequestBatcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an operation and maybe flush.
    pub fn mark_op<C: Connection>(&self, conn: &C) -> Result<(), BackendError> {
        if self.counters.note_op() {
            self.do_flush(conn)?;
        }
        Ok(())
    }

    /// Force a flush.
    pub fn flush<C: Connection>(&self, conn: &C) -> Result<(), BackendError> {
        self.do_flush(conn)
    }

    fn do_flush<C: Connection>(&self, conn: &C) -> Result<(), BackendError> {
        conn.flush()?;
        self.counters.on_flushed();
        Ok(())
    }

    /// Get pending operations count (for debugging).
    #[allow(dead_code)]
    pub fn pending_count(&self) -> u32 {
        self.counters.pending_count()
    }

    /// Adjust batch thresholds based on system load (0-100).
    pub fn adjust_thresholds(&self, load: u32) {
        self.counters.adjust_thresholds(load);
    }

    /// Get current system load estimate.
    pub fn system_load(&self) -> u32 {
        self.counters.system_load()
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

    // The batching *policy* (thresholds, timing, load bands, shared clone
    // state) is tested once in `backend::x11::wm::batch`. These cover the
    // x11rb wrapper delegating through the shared counters.

    #[test]
    fn wrapper_starts_empty_with_a_neutral_load() {
        let batcher = X11RequestBatcher::new();
        assert_eq!(batcher.pending_count(), 0);
        assert_eq!(batcher.system_load(), 50);
    }

    #[test]
    fn load_adjustment_is_reflected_through_the_wrapper() {
        let batcher = X11RequestBatcher::new();
        batcher.adjust_thresholds(85);
        assert_eq!(batcher.system_load(), 85);
        batcher.adjust_thresholds(150);
        assert_eq!(batcher.system_load(), 100, "load is clamped to 100");
    }

    #[test]
    fn default_matches_new() {
        let batcher = X11RequestBatcher::default();
        assert_eq!(batcher.pending_count(), 0);
        assert_eq!(batcher.system_load(), 50);
    }
}
