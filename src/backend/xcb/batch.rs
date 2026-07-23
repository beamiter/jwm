use crate::backend::error::BackendError;
use crate::backend::x11::wm::batch::BatchCounters;
use std::collections::HashMap;
use xcb::{Xid, XidNew, x};

/// X11 request batching tuned for an `xcb::Connection`.
///
/// The batching policy (thresholds, timing, load adaptation) lives in the
/// shared, transport-neutral [`BatchCounters`]; this wrapper only performs
/// the xcb-specific `conn.flush()`.
#[derive(Clone, Default)]
pub struct XcbRequestBatcher {
    counters: BatchCounters,
}

impl XcbRequestBatcher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mark_op(&self, conn: &xcb::Connection) -> Result<(), BackendError> {
        if self.counters.note_op() {
            self.do_flush(conn)?;
        }
        Ok(())
    }

    pub fn flush(&self, conn: &xcb::Connection) -> Result<(), BackendError> {
        self.do_flush(conn)
    }

    fn do_flush(&self, conn: &xcb::Connection) -> Result<(), BackendError> {
        conn.flush()
            .map_err(|e| BackendError::Message(format!("xcb flush failed: {e}")))?;
        self.counters.on_flushed();
        Ok(())
    }

    pub fn pending_count(&self) -> u32 {
        self.counters.pending_count()
    }

    pub fn adjust_thresholds(&self, load: u32) {
        self.counters.adjust_thresholds(load);
    }

    pub fn system_load(&self) -> u32 {
        self.counters.system_load()
    }
}

pub struct BatchedGeometryRequest<'a> {
    conn: &'a xcb::Connection,
    windows: Vec<x::Window>,
}

impl<'a> BatchedGeometryRequest<'a> {
    pub fn new(conn: &'a xcb::Connection) -> Self {
        Self {
            conn,
            windows: Vec::new(),
        }
    }

    pub fn queue_geometry(&mut self, window: u32) {
        self.windows.push(x::Window::new(window));
    }

    pub fn flush_and_collect(self) -> Result<HashMap<u32, (i16, i16, u16, u16)>, BackendError> {
        let mut cookies = Vec::with_capacity(self.windows.len());

        for window in &self.windows {
            let cookie = self.conn.send_request(&x::GetGeometry {
                drawable: x::Drawable::Window(*window),
            });
            cookies.push((*window, cookie));
        }

        self.conn
            .flush()
            .map_err(|e| BackendError::Message(format!("xcb flush failed: {e}")))?;

        let mut results = HashMap::new();
        for (window, cookie) in cookies {
            match self.conn.wait_for_reply(cookie) {
                Ok(reply) => {
                    results.insert(
                        window.resource_id(),
                        (reply.x(), reply.y(), reply.width(), reply.height()),
                    );
                }
                Err(e) => {
                    log::debug!(
                        "Failed to get geometry for window 0x{:x}: {}",
                        window.resource_id(),
                        e
                    );
                }
            }
        }

        Ok(results)
    }

    pub fn pending_count(&self) -> usize {
        self.windows.len()
    }
}

type PropertyKey = (u32, u32);

#[derive(Clone, Copy)]
pub struct PropertyQuery {
    pub window: u32,
    pub atom: u32,
    pub prop_type: u32,
    pub max_len: u32,
}

pub struct BatchedPropertyRequest<'a> {
    conn: &'a xcb::Connection,
    queries: Vec<PropertyQuery>,
}

impl<'a> BatchedPropertyRequest<'a> {
    pub fn new(conn: &'a xcb::Connection) -> Self {
        Self {
            conn,
            queries: Vec::new(),
        }
    }

    pub fn queue_property(&mut self, window: u32, atom: u32, prop_type: u32, max_len: u32) {
        self.queries.push(PropertyQuery {
            window,
            atom,
            prop_type,
            max_len,
        });
    }

    pub fn flush_and_collect(self) -> Result<HashMap<PropertyKey, Vec<u8>>, BackendError> {
        let mut cookies = Vec::with_capacity(self.queries.len());

        for query in &self.queries {
            let window = x::Window::new(query.window);
            let atom = x::Atom::new(query.atom);
            let prop_type = x::Atom::new(query.prop_type);
            let cookie = self.conn.send_request(&x::GetProperty {
                delete: false,
                window,
                property: atom,
                r#type: prop_type,
                long_offset: 0,
                long_length: query.max_len,
            });
            cookies.push(((query.window, query.atom), cookie));
        }

        self.conn
            .flush()
            .map_err(|e| BackendError::Message(format!("xcb flush failed: {e}")))?;

        let mut results = HashMap::new();
        for (key, cookie) in cookies {
            match self.conn.wait_for_reply(cookie) {
                Ok(reply) => {
                    results.insert(key, reply.value::<u8>().to_vec());
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

    pub fn pending_count(&self) -> usize {
        self.queries.len()
    }
}

pub struct BatchedAttributesRequest<'a> {
    conn: &'a xcb::Connection,
    windows: Vec<x::Window>,
}

impl<'a> BatchedAttributesRequest<'a> {
    pub fn new(conn: &'a xcb::Connection) -> Self {
        Self {
            conn,
            windows: Vec::new(),
        }
    }

    pub fn queue_attributes(&mut self, window: u32) {
        self.windows.push(x::Window::new(window));
    }

    pub fn flush_and_collect(
        self,
    ) -> Result<HashMap<u32, x::GetWindowAttributesReply>, BackendError> {
        let mut cookies = Vec::with_capacity(self.windows.len());

        for window in &self.windows {
            let cookie = self
                .conn
                .send_request(&x::GetWindowAttributes { window: *window });
            cookies.push((*window, cookie));
        }

        self.conn
            .flush()
            .map_err(|e| BackendError::Message(format!("xcb flush failed: {e}")))?;

        let mut results = HashMap::new();
        for (window, cookie) in cookies {
            match self.conn.wait_for_reply(cookie) {
                Ok(reply) => {
                    results.insert(window.resource_id(), reply);
                }
                Err(e) => {
                    log::debug!(
                        "Failed to get attributes for window 0x{:x}: {}",
                        window.resource_id(),
                        e
                    );
                }
            }
        }

        Ok(results)
    }

    pub fn pending_count(&self) -> usize {
        self.windows.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_batcher_creation() {
        let batcher = XcbRequestBatcher::new();
        assert_eq!(batcher.pending_count(), 0);
        assert_eq!(batcher.system_load(), 50);
    }

    #[test]
    fn test_adjust_thresholds_records_load() {
        let batcher = XcbRequestBatcher::new();
        batcher.adjust_thresholds(85);
        assert_eq!(batcher.system_load(), 85);
    }

    #[test]
    fn test_geometry_batch_pending_count() {
        // This test only covers local queue bookkeeping. Keep headless CI
        // usable when no X server is available.
        let Ok((conn, _)) = xcb::Connection::connect(None) else {
            return;
        };
        let mut batch = BatchedGeometryRequest::new(&conn);
        batch.queue_geometry(1);
        batch.queue_geometry(2);
        assert_eq!(batch.pending_count(), 2);
    }
}
