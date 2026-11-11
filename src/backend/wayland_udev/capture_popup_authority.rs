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

/// Scalar metadata from the immutable layout actually used to render a frame.
/// Target 0 denotes the root and target 1 its sole direct popup, scoped to this
/// generation. These are not Wayland object IDs or independent input authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExportedFamilyLayout {
    pub(crate) generation: u64,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) popup: Option<(u32, u32, u32, u32)>,
}

fn export_layout(
    generation: u64,
    width: u32,
    height: u32,
    popup: Option<(i32, i32, i32, i32)>,
) -> Option<ExportedFamilyLayout> {
    if generation == 0 || width == 0 || height == 0 || width > 16384 || height > 16384 {
        return None;
    }
    let popup = match popup {
        None => None,
        Some((x, y, w, h)) => {
            let (x, y, w, h) = (
                u32::try_from(x).ok()?,
                u32::try_from(y).ok()?,
                u32::try_from(w).ok()?,
                u32::try_from(h).ok()?,
            );
            if w == 0 || h == 0 || x.checked_add(w)? > width || y.checked_add(h)? > height {
                return None;
            }
            Some((x, y, w, h))
        }
    };
    Some(ExportedFamilyLayout {
        generation,
        width,
        height,
        popup,
    })
}

impl FamilyProof {
    pub(crate) fn export_layout(&self) -> Option<ExportedFamilyLayout> {
        if self.layout.root != self.authority.root.surface
            || self.layout.width != self.authority.root.width
            || self.layout.height != self.authority.root.height
        {
            return None;
        }
        export_layout(
            self.generation,
            self.layout.width,
            self.layout.height,
            self.layout.popup.as_ref().map(|p| p.rect),
        )
    }
}

#[cfg(test)]
mod export_tests {
    use super::export_layout;
    #[test]
    fn family_export_layout_preserves_exact_frozen_region() {
        let root = export_layout(7, 320, 240, None).unwrap();
        assert_eq!(
            (root.generation, root.width, root.height, root.popup),
            (7, 320, 240, None)
        );
        let popup = export_layout(8, 320, 240, Some((200, 140, 120, 100))).unwrap();
        assert_eq!(popup.popup, Some((200, 140, 120, 100)));
        assert_ne!(root.generation, popup.generation);
    }
    #[test]
    fn family_export_layout_rejects_unbounded_or_invalid_regions() {
        for rect in [
            (-1, 0, 1, 1),
            (0, -1, 1, 1),
            (0, 0, 0, 1),
            (0, 0, 1, -1),
            (319, 0, 2, 1),
            (0, 239, 1, 2),
            (i32::MAX, 0, i32::MAX, 1),
        ] {
            assert!(export_layout(1, 320, 240, Some(rect)).is_none());
        }
        for (g, w, h) in [(0, 320, 240), (1, 0, 240), (1, 320, 0), (1, 16385, 240)] {
            assert!(export_layout(g, w, h, None).is_none());
        }
    }
}
