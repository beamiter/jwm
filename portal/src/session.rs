#![allow(dead_code)]
//! Per-portal runtime state. Holds the shared handle to Wayland state, the
//! PipeWire connection, and the live map of ScreenCast sessions.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::picker::SourceSelection;
use crate::pipewire_stream::StreamHandle;
use crate::wayland::SharedSnapshot;

#[derive(Clone)]
pub struct Runtime {
    inner: Arc<Inner>,
}

struct Inner {
    pub sessions: Mutex<HashMap<String, Session>>,
    pub wayland: SharedSnapshot,
}

#[derive(Default)]
pub struct Session {
    /// Whatever the caller asked for in SelectSources, resolved against current
    /// outputs/toplevels at the time the call was made.
    pub selection: Option<SourceSelection>,
    /// Live PipeWire stream handles; dropping them tears the streams down.
    /// Order matches the `streams` array returned by Start.
    pub streams: Vec<StreamHandle>,
}

impl Runtime {
    pub async fn start(wayland: SharedSnapshot) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Self {
            inner: Arc::new(Inner {
                sessions: Mutex::new(HashMap::new()),
                wayland,
            }),
        })
    }

    pub fn wayland(&self) -> &SharedSnapshot {
        &self.inner.wayland
    }

    pub async fn with_session<F, R>(&self, handle: &str, f: F) -> Option<R>
    where
        F: FnOnce(&mut Session) -> R,
    {
        let mut guard = self.inner.sessions.lock().await;
        guard.get_mut(handle).map(f)
    }

    pub async fn insert_session(&self, handle: String) {
        self.inner
            .sessions
            .lock()
            .await
            .insert(handle, Session::default());
    }

    pub async fn remove_session(&self, handle: &str) -> Option<Session> {
        self.inner.sessions.lock().await.remove(handle)
    }
}
