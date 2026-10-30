//! Fail-closed single-scene-node contract. Neither a heartbeat nor an IPC
//! acknowledgement is a retirement proof. This module owns no I/O or audio.
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    X11,
    Wayland,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct NativeSurface {
    pub host: String,
    pub incarnation: u64,
    pub surface: String,
    pub epoch: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub wm_instance: String,
    pub window: String,
    pub workspace: u32,
    pub revision: u64,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Visibility {
    pub locked: bool,
    pub hidden: bool,
    pub fullscreen: bool,
    pub direct_scanout: bool,
    pub output_visible: bool,
    pub workspace_visible: bool,
}
impl Visibility {
    pub fn permits_body(self) -> bool {
        !self.locked
            && !self.hidden
            && !self.fullscreen
            && !self.direct_scanout
            && self.output_visible
            && self.workspace_visible
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Blocked {
    UnverifiedBackend,
    IncompleteInventory,
    UnretiredNativeBody,
    WrongBoot,
    WrongTarget,
    StaleEpoch,
    Suppressed,
}
#[derive(Clone, Debug)]
pub struct BodySnapshot {
    pub companion_id: String,
    pub boot: String,
    pub epoch: u64,
    pub target: Target,
    pub skin: String,
    pub pose: String,
}
/// One slot, not a vector of bodies. Production backends currently create an
/// unverified gate; hardware/native present integration must explicitly supply
/// actual retirement serials before a compositor body may ever be installed.
pub struct SceneNode {
    backend: BackendKind,
    verified: bool,
    boot: String,
    inventory_complete: bool,
    native: BTreeSet<NativeSurface>,
    retired: BTreeMap<NativeSurface, u64>,
    highest_epoch: u64,
    body: Option<BodySnapshot>,
}
impl SceneNode {
    pub fn unverified(backend: BackendKind, boot: String) -> Self {
        Self {
            backend,
            verified: false,
            boot,
            inventory_complete: false,
            native: BTreeSet::new(),
            retired: BTreeMap::new(),
            highest_epoch: 0,
            body: None,
        }
    }
    pub fn backend(&self) -> BackendKind {
        self.backend
    }
    /// Reconciliation inventory must include every upgraded native body. Old
    /// unregistered clients keep `complete` false, even if no peer responds.
    pub fn inventory(&mut self, native: impl IntoIterator<Item = NativeSurface>, complete: bool) {
        self.body = None;
        self.native = native.into_iter().collect();
        self.retired.clear();
        self.inventory_complete = complete;
    }
    /// This is NOT exposed over JSON/IPC. A native backend integration must
    /// only call it after its own presented frame/surface-destruction fence.
    /// Merely calling this on an unverified backend cannot enable drawing.
    pub fn native_surface_retired(
        &mut self,
        surface: NativeSurface,
        presented_serial: u64,
    ) -> bool {
        if presented_serial == 0 || !self.native.contains(&surface) {
            return false;
        }
        if self
            .retired
            .get(&surface)
            .is_some_and(|old| *old >= presented_serial)
        {
            return false;
        }
        self.retired.insert(surface, presented_serial);
        true
    }
    pub fn install(
        &mut self,
        snapshot: BodySnapshot,
        current_target: &Target,
        visibility: Visibility,
    ) -> Result<(), Blocked> {
        self.body = None;
        if !self.verified {
            return Err(Blocked::UnverifiedBackend);
        }
        if snapshot.boot != self.boot {
            return Err(Blocked::WrongBoot);
        }
        if !self.inventory_complete {
            return Err(Blocked::IncompleteInventory);
        }
        if self.native.iter().any(|s| !self.retired.contains_key(s)) {
            return Err(Blocked::UnretiredNativeBody);
        }
        if snapshot.epoch <= self.highest_epoch {
            return Err(Blocked::StaleEpoch);
        }
        if &snapshot.target != current_target {
            return Err(Blocked::WrongTarget);
        }
        if !visibility.permits_body() {
            return Err(Blocked::Suppressed);
        }
        self.highest_epoch = snapshot.epoch;
        self.body = Some(snapshot);
        Ok(())
    }
    /// Lock/fullscreen/direct-scanout/target changes synchronously clear the
    /// same node; callers never move an old task binding to a new focus.
    pub fn visible(
        &mut self,
        current_target: &Target,
        visibility: Visibility,
    ) -> Option<&BodySnapshot> {
        if !visibility.permits_body()
            || self
                .body
                .as_ref()
                .is_some_and(|b| &b.target != current_target)
        {
            self.body = None;
        }
        self.body.as_ref()
    }
    pub fn revoke(&mut self) {
        self.body = None;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn target() -> Target {
        Target {
            wm_instance: "w".into(),
            window: "win-1".into(),
            workspace: 1,
            revision: 1,
        }
    }
    fn body() -> BodySnapshot {
        BodySnapshot {
            companion_id: "eva".into(),
            boot: "b".into(),
            epoch: 1,
            target: target(),
            skin: "cat".into(),
            pose: "idle".into(),
        }
    }
    fn visible() -> Visibility {
        Visibility {
            output_visible: true,
            workspace_visible: true,
            ..Default::default()
        }
    }
    fn surface() -> NativeSurface {
        NativeSurface {
            host: "ember".into(),
            incarnation: 1,
            surface: "window-1".into(),
            epoch: 1,
        }
    }
    #[test]
    fn x11_and_wayland_fail_closed_independently() {
        for backend in [BackendKind::X11, BackendKind::Wayland] {
            let mut node = SceneNode::unverified(backend, "b".into());
            node.inventory([], true);
            assert_eq!(
                node.install(body(), &target(), visible()),
                Err(Blocked::UnverifiedBackend)
            );
        }
    }
    #[test]
    fn heartbeat_inventory_is_not_retirement() {
        let mut node = SceneNode::unverified(BackendKind::X11, "b".into());
        node.verified = true;
        node.inventory([surface()], true);
        assert_eq!(
            node.install(body(), &target(), visible()),
            Err(Blocked::UnretiredNativeBody)
        );
        assert!(!node.native_surface_retired(surface(), 0));
        assert!(node.native_surface_retired(surface(), 9));
        assert!(node.install(body(), &target(), visible()).is_ok());
        assert_eq!(
            node.install(body(), &target(), visible()),
            Err(Blocked::StaleEpoch)
        );
    }
    #[test]
    fn all_suppression_paths_remove_node() {
        for kind in 0..6 {
            let mut node = SceneNode::unverified(BackendKind::Wayland, "b".into());
            node.verified = true;
            node.inventory([], true);
            node.install(body(), &target(), visible()).unwrap();
            let mut v = visible();
            match kind {
                0 => v.locked = true,
                1 => v.hidden = true,
                2 => v.fullscreen = true,
                3 => v.direct_scanout = true,
                4 => v.output_visible = false,
                _ => v.workspace_visible = false,
            };
            assert!(node.visible(&target(), v).is_none());
            assert!(node.visible(&target(), visible()).is_none());
        }
    }
    #[test]
    fn late_task_target_never_follows_new_focus() {
        let mut node = SceneNode::unverified(BackendKind::Wayland, "b".into());
        node.verified = true;
        node.inventory([], true);
        node.install(body(), &target(), visible()).unwrap();
        let mut t = target();
        t.revision += 1;
        assert!(node.visible(&t, visible()).is_none());
    }
}
