//! A separate, non-upgradable interactive root-family capture capability.
use super::{
    capture_popup_snapshot::{ImmutableLayout, interactive_snapshot},
    image_copy_capture::{CaptureAuthority, authority_is_current},
};
use crate::backend::wayland::state::JwmWaylandState;
use smithay::reexports::wayland_server::Resource;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
#[derive(Debug)]
pub(crate) struct FamilyAuthority {
    pub(crate) root: Arc<CaptureAuthority>,
    pub(crate) created: Instant,
    generation: AtomicU64,
    revoked: AtomicBool,
}
#[derive(Clone, Debug)]
pub(crate) struct FamilyProof {
    pub(crate) authority: Arc<FamilyAuthority>,
    pub(crate) generation: u64,
    pub(crate) layout: ImmutableLayout,
}
impl FamilyAuthority {
    pub(crate) fn new(state: &JwmWaylandState, root: &Arc<CaptureAuthority>) -> Option<Arc<Self>> {
        if !root.is_original_root()
            || !state
                .toplevels
                .get(&root.window)
                .is_some_and(|t| t.wl_surface() == &root.surface)
            || !authority_is_current(state, root)
        {
            return None;
        }
        let layout = interactive_snapshot(state, &root.surface, root.width, root.height)?;
        if layout.popup.is_some()
            || state
                .popups
                .values()
                .any(|p| p.get_parent_surface().as_ref() == Some(&root.surface))
        {
            return None;
        }
        Some(Arc::new(Self {
            root: root.clone(),
            created: Instant::now(),
            generation: AtomicU64::new(1),
            revoked: AtomicBool::new(false),
        }))
    }
    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
    pub(crate) fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
    }
    pub(crate) fn invalidate(&self) {
        let mut previous = self.generation.load(Ordering::Acquire);
        loop {
            let Some(next) = previous.checked_add(1) else {
                self.revoke();
                return;
            };
            match self.generation.compare_exchange(
                previous,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(actual) => previous = actual,
            }
        }
    }

    pub(crate) fn current(&self, state: &JwmWaylandState) -> bool {
        if self.revoked.load(Ordering::Acquire)
            || self.created.elapsed() >= Duration::from_secs(30)
            || !self.root.surface.is_alive()
            || !state
                .toplevels
                .get(&self.root.window)
                .is_some_and(|t| t.wl_surface() == &self.root.surface)
            || !authority_is_current(state, &self.root)
        {
            self.revoke();
            return false;
        }
        true
    }
    #[cfg(feature = "backend-wayland-udev")]
    pub(crate) fn snapshot(self: &Arc<Self>, state: &JwmWaylandState) -> Option<Arc<FamilyProof>> {
        if !self.current(state) {
            return None;
        }
        let generation = self.generation();
        let layout =
            interactive_snapshot(state, &self.root.surface, self.root.width, self.root.height)?;
        (self.current(state) && generation == self.generation()).then(|| {
            Arc::new(FamilyProof {
                authority: self.clone(),
                generation,
                layout,
            })
        })
    }
}
impl FamilyProof {
    pub(crate) fn current(&self, state: &JwmWaylandState) -> bool {
        self.authority.current(state)
            && self.generation == self.authority.generation()
            && interactive_snapshot(
                state,
                &self.authority.root.surface,
                self.authority.root.width,
                self.authority.root.height,
            )
            .as_ref()
                == Some(&self.layout)
    }
}
