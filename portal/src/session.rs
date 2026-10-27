#![allow(dead_code)]
//! Per-portal runtime state. Holds the shared handle to Wayland state, the
//! PipeWire connection, and the live map of ScreenCast sessions.

use std::collections::{HashMap, hash_map::Entry};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Mutex;

use crate::capture::CaptureHandle;
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

/// Identity survives blocking work without letting an old operation mutate a
/// session that was closed and recreated at the same object path.
#[derive(Clone, Debug, Default)]
pub struct SessionToken(Arc<AtomicBool>);

impl SessionToken {
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    fn matches(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Created,
    Selecting,
    Selected,
    Starting,
    Started,
    Failed,
}

pub struct StartRequest {
    pub token: SessionToken,
    pub selection: SourceSelection,
    pub persist_mode: u32,
    pub prior_token: Option<String>,
    pub paint_cursors: bool,
}

#[derive(Default)]
pub struct Session {
    token: SessionToken,
    pub phase: Phase,
    /// Whatever the caller asked for in SelectSources, resolved against current
    /// outputs/toplevels at the time the call was made.
    pub selection: Option<SourceSelection>,
    /// Live PipeWire stream handles; dropping them tears the streams down.
    /// Order matches the `streams` array returned by Start.
    pub streams: Vec<StreamHandle>,
    /// Live Wayland capture threads, one per output source. Dropped together
    /// with the PW streams when the session ends (or the dbus Session map
    /// entry is replaced).
    pub captures: Vec<CaptureHandle>,
    /// Caller's persist_mode from SelectSources (0=none, 1=transient,
    /// 2=permanent). When >0, Start returns backend restore_data.
    pub persist_mode: u32,
    /// Private key decoded from validated backend restore_data (wrapped in
    /// Start if the stored selection still resolves).
    pub restore_token: Option<String>,
    /// Whether the compositor should composite the cursor into captured
    /// frames. Mirrors the cursor_mode bit set in SelectSources (Embedded=true,
    /// Hidden=false). Defaults to false as required by the backend interface.
    pub paint_cursors: bool,
}

impl Session {
    #[cfg(test)]
    pub fn token_for_test(&self) -> SessionToken {
        self.token.clone()
    }

    pub fn begin_selection(&mut self) -> Option<SessionToken> {
        if self.phase != Phase::Created {
            return None;
        }
        self.phase = Phase::Selecting;
        Some(self.token.clone())
    }

    pub fn begin_start(&mut self) -> Option<StartRequest> {
        if self.phase != Phase::Selected {
            return None;
        }
        let selection = self.selection.clone()?;
        if selection.outputs.is_empty() && selection.toplevels.is_empty() {
            self.phase = Phase::Failed;
            return None;
        }
        self.phase = Phase::Starting;
        Some(StartRequest {
            token: self.token.clone(),
            selection,
            persist_mode: self.persist_mode,
            prior_token: self.restore_token.clone(),
            paint_cursors: self.paint_cursors,
        })
    }

    pub fn started_token(&self) -> Option<SessionToken> {
        (self.phase == Phase::Started).then(|| self.token.clone())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.token.0.store(true, Ordering::Release);
    }
}

impl Runtime {
    pub async fn start(
        wayland: SharedSnapshot,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
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

    pub async fn with_current_session<F, R>(
        &self,
        handle: &str,
        token: &SessionToken,
        f: F,
    ) -> Option<R>
    where
        F: FnOnce(&mut Session) -> R,
    {
        self.with_session(handle, |session| {
            if session.token.matches(token) && !token.is_cancelled() {
                Some(f(session))
            } else {
                None
            }
        })
        .await
        .flatten()
    }

    pub async fn insert_session(&self, handle: String) -> Option<SessionToken> {
        match self.inner.sessions.lock().await.entry(handle) {
            Entry::Vacant(entry) => {
                let session = Session::default();
                let token = session.token.clone();
                entry.insert(session);
                Some(token)
            }
            Entry::Occupied(_) => None,
        }
    }

    pub async fn remove_session(&self, handle: &str, token: &SessionToken) -> Option<Session> {
        let mut sessions = self.inner.sessions.lock().await;
        if !sessions.get(handle)?.token.matches(token) {
            return None;
        }
        let session = sessions.remove(handle)?;
        // Mark cancellation before releasing the map lock or joining workers.
        session.token.0.store(true, Ordering::Release);
        Some(session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn runtime() -> Runtime {
        Runtime::start(Arc::new(std::sync::Mutex::new(Default::default())))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn duplicate_create_cannot_reset_an_existing_session() {
        let rt = runtime().await;
        let handle = "/session/duplicate";
        assert!(rt.insert_session(handle.into()).await.is_some());
        rt.with_session(handle, |session| session.persist_mode = 2)
            .await;
        assert!(rt.insert_session(handle.into()).await.is_none());
        assert_eq!(
            rt.with_session(handle, |session| session.persist_mode)
                .await,
            Some(2)
        );
    }

    #[tokio::test]
    async fn close_invalidates_pending_work_before_workers_are_joined() {
        let rt = runtime().await;
        let handle = "/session/reused";
        let old = rt.insert_session(handle.into()).await.unwrap();
        let removed = rt.remove_session(handle, &old).await.unwrap();
        assert!(old.is_cancelled());
        let new = rt.insert_session(handle.into()).await.unwrap();
        assert!(!new.is_cancelled());
        assert!(
            rt.with_current_session(handle, &old, |s| s.persist_mode = 2)
                .await
                .is_none()
        );
        assert!(rt.remove_session(handle, &old).await.is_none());
        assert_eq!(
            rt.with_current_session(handle, &new, |s| s.persist_mode)
                .await,
            Some(0)
        );
        drop(removed);
    }

    #[tokio::test]
    async fn simultaneous_start_requests_cannot_both_launch_workers() {
        let rt = runtime().await;
        let handle = "/session/start_once";
        let token = rt.insert_session(handle.into()).await.unwrap();
        rt.with_current_session(handle, &token, |s| {
            s.phase = Phase::Selected;
            s.selection = Some(SourceSelection {
                outputs: vec![crate::wayland::OutputInfo::default()],
                toplevels: Vec::new(),
            });
        })
        .await
        .unwrap();
        let (a, b) = tokio::join!(
            rt.with_session(handle, Session::begin_start),
            rt.with_session(handle, Session::begin_start),
        );
        assert_eq!(
            usize::from(a.flatten().is_some()) + usize::from(b.flatten().is_some()),
            1
        );
        assert!(
            rt.with_session(handle, |s| s.started_token())
                .await
                .flatten()
                .is_none()
        );
    }

    #[test]
    fn selection_and_start_follow_the_session_lifecycle() {
        let mut session = Session::default();
        assert!(session.begin_start().is_none());
        assert!(session.begin_selection().is_some());
        assert!(session.begin_selection().is_none());
        assert!(session.begin_start().is_none());
        session.phase = Phase::Selected;
        session.selection = Some(SourceSelection::default());
        assert!(session.begin_start().is_none());
        assert_eq!(session.phase, Phase::Failed);
    }
}
