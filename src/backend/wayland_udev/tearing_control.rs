//! wp-tearing-control-v1 protocol implementation for JWM.
//!
//! Game clients use this to say they would rather see a torn frame than wait
//! for the next vblank. The compositor stores the per-surface hint and
//! reports it through telemetry, IPC (`get_tearing_hints`), and the
//! per-output presentation policy in `udev_kms`.
//!
//! **The hint is double-buffered state**, exactly like the image description
//! wp-color-management carries: the protocol says `set_presentation_hint`
//! "will be applied on the next wl_surface.commit", and destroying the
//! control object reverts to vsync on the next commit as well. Applying
//! either immediately would let a hint change describe a buffer the client
//! submitted under the other rule — so requests *stage*, and
//! `CompositorHandler::commit` latches, the same shape as
//! `SurfaceDescriptionLatch`. Everything downstream reads only the committed
//! half.
//!
//! The map operations are generic over the key so the whole state machine is
//! unit tested without a display to mint `ObjectId`s from; the public
//! wrappers are the locking layer and nothing else.
//!
//! **What is not here.** JWM does not issue asynchronous page flips: the
//! pinned Smithay revision routes every frame through
//! `DrmCompositor::queue_frame`, whose submission path hardcodes its atomic
//! commit flags and exposes no way to request `PAGE_FLIP_ASYNC`. `udev_kms`
//! computes the per-output decision anyway and reports why it did or did not
//! fire, so the gap is visible in diagnostics rather than silent; what jwm
//! acts on today is VRR, which the same policy drives.
use crate::sync_ext::MutexExt;
use smithay::reexports::wayland_protocols::wp::tearing_control::v1::server::{
    wp_tearing_control_manager_v1::{self, WpTearingControlManagerV1},
    wp_tearing_control_v1::{self, WpTearingControlV1},
};
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};

use crate::backend::wayland::state::JwmWaylandState;

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex};

/// Per-surface tearing preference, stored in JwmWaylandState.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TearingHint {
    #[default]
    Vsync,
    Async,
}

/// One surface's pending/current hint pair.
///
/// `pending` is `Some(hint)` when a request has staged a change that no
/// commit has applied yet; `None` means the committed half is current.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TearingHintLatch {
    pending: Option<TearingHint>,
    current: TearingHint,
    /// Whether a live `wp_tearing_control_v1` exists for this surface. The
    /// protocol requires `tearing_control_exists` on a second one, and a
    /// destroyed object must not take the first one's hint with it.
    control_alive: bool,
}

impl TearingHintLatch {
    /// Committed vsync, nothing staged, and no object to protect: the entry
    /// carries no information and can be recycled.
    fn is_idle(&self) -> bool {
        self.pending.is_none() && self.current == TearingHint::Vsync && !self.control_alive
    }

    /// Apply a staged change. Returns whether the committed hint moved.
    fn commit(&mut self) -> bool {
        match self.pending.take() {
            Some(hint) if hint != self.current => {
                self.current = hint;
                true
            }
            _ => false,
        }
    }
}

/// Shared map of surface ObjectId -> latched hint, read once per frame by the
/// KMS presentation policy.
pub type TearingHintMap = Arc<Mutex<HashMap<ObjectId, TearingHintLatch>>>;

pub fn new_tearing_hint_map() -> TearingHintMap {
    Arc::new(Mutex::new(HashMap::new()))
}

// ---------------------------------------------------------------------------
// The state machine, over any key (pure)
// ---------------------------------------------------------------------------

fn stage_in<K: Eq + Hash + Clone>(
    map: &mut HashMap<K, TearingHintLatch>,
    key: &K,
    hint: TearingHint,
) {
    map.entry(key.clone()).or_default().pending = Some(hint);
}

fn commit_in<K: Eq + Hash>(map: &mut HashMap<K, TearingHintLatch>, key: &K) -> bool {
    let Some(latch) = map.get_mut(key) else {
        return false;
    };
    let changed = latch.commit();
    if latch.is_idle() {
        map.remove(key);
    }
    changed
}

/// Claim the surface's one control object. `false` when it already has one,
/// which is the protocol's `tearing_control_exists` error.
fn claim_control_in<K: Eq + Hash + Clone>(map: &mut HashMap<K, TearingHintLatch>, key: &K) -> bool {
    let latch = map.entry(key.clone()).or_default();
    if latch.control_alive {
        return false;
    }
    latch.control_alive = true;
    true
}

/// The object died without a Destroy request. Release the claim and stage the
/// revert to vsync the protocol specifies, to be applied at the next commit.
fn release_control_in<K: Eq + Hash>(map: &mut HashMap<K, TearingHintLatch>, key: &K) {
    if let Some(latch) = map.get_mut(key) {
        latch.control_alive = false;
        latch.pending = Some(TearingHint::Vsync);
    }
}

fn committed_in<K: Eq + Hash>(map: &HashMap<K, TearingHintLatch>, key: &K) -> TearingHint {
    map.get(key)
        .map_or(TearingHint::Vsync, |latch| latch.current)
}

fn async_count_in<K>(map: &HashMap<K, TearingHintLatch>) -> usize {
    map.values()
        .filter(|latch| latch.current == TearingHint::Async)
        .count()
}

// ---------------------------------------------------------------------------
// Locking wrappers
// ---------------------------------------------------------------------------

/// Latch whatever the surface staged. Returns whether the committed hint
/// changed, so the caller can arm a redraw: the presentation route for that
/// output may now differ.
pub fn commit_surface_hint(hints: &TearingHintMap, surface: &ObjectId) -> bool {
    commit_in(&mut hints.lock_safe(), surface)
}

/// Forget a surface entirely.
///
/// The protocol lets a client destroy the `wl_surface` and keep the control
/// object ("should be destroyed", not "must"), so the object-death hook is
/// not enough on its own: without this the entry outlives the surface, keyed
/// by an ObjectId the server may later hand to something else. Called from
/// `CompositorHandler::destroyed`, beside the equivalent color-management
/// purge.
pub fn forget_surface(hints: &TearingHintMap, surface: &ObjectId) -> bool {
    hints.lock_safe().remove(surface).is_some()
}

/// The committed hint for one surface. Absent entries are vsync — the
/// protocol's own default, and the safe one.
#[must_use]
pub fn committed_hint(hints: &TearingHintMap, surface: &ObjectId) -> TearingHint {
    committed_in(&hints.lock_safe(), surface)
}

/// How many surfaces are currently asking to tear. This is client *demand*,
/// not what the compositor did with it.
#[must_use]
pub fn async_hint_count(hints: &TearingHintMap) -> usize {
    async_count_in(&hints.lock_safe())
}

/// User data stored per wp_tearing_control_v1 object.
pub struct TearingControlData {
    surface: WlSurface,
}

// TearingControlData contains Wayland protocol objects which are !Send.
// JWM runs everything on the main thread so this is fine.
unsafe impl Send for TearingControlData {}

/// Initialize the wp_tearing_control_manager_v1 global.
pub fn init_tearing_control_manager(dh: &DisplayHandle) -> TearingHintMap {
    dh.create_global::<JwmWaylandState, WpTearingControlManagerV1, _>(1, ());
    log::info!("[udev/wayland] wp-tearing-control-v1 global registered");
    new_tearing_hint_map()
}

// --- GlobalDispatch for the manager ---

impl GlobalDispatch<WpTearingControlManagerV1, ()> for JwmWaylandState {
    fn bind(
        state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<WpTearingControlManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        state.record_protocol_bind("wp_tearing_control_manager_v1");
        data_init.init(resource, ());
    }
}

// --- Dispatch for the manager ---

impl Dispatch<WpTearingControlManagerV1, ()> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &WpTearingControlManagerV1,
        request: wp_tearing_control_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_tearing_control_manager_v1::Request::GetTearingControl { id, surface } => {
                // Two control objects on one surface would share a single
                // hint, so destroying the second would silently clear the
                // first's Async request. The protocol makes this an error;
                // hiding it would leave the tearing decision dependent on
                // which object a buggy client happened to destroy last.
                if let Some(hints) = state.tearing_hints.as_ref()
                    && !claim_control_in(&mut hints.lock_safe(), &surface.id())
                {
                    resource.post_error(
                        wp_tearing_control_manager_v1::Error::TearingControlExists,
                        "the surface already has a wp_tearing_control_v1",
                    );
                    return;
                }
                data_init.init(
                    id,
                    TearingControlData {
                        surface: surface.clone(),
                    },
                );
            }
            wp_tearing_control_manager_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

// --- Dispatch for per-surface tearing control ---

impl Dispatch<WpTearingControlV1, TearingControlData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &WpTearingControlV1,
        request: wp_tearing_control_v1::Request,
        data: &TearingControlData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let Some(hints) = state.tearing_hints.as_ref() else {
            return;
        };
        match request {
            wp_tearing_control_v1::Request::SetPresentationHint { hint } => {
                let hint = match hint.into_result() {
                    Ok(wp_tearing_control_v1::PresentationHint::Async) => TearingHint::Async,
                    _ => TearingHint::Vsync,
                };
                stage_in(&mut hints.lock_safe(), &data.surface.id(), hint);
            }
            // "Destroying this object ... reverts the presentation hint to
            // vsync. The change will be applied on the next
            // wl_surface.commit" — so this stages like any other request.
            wp_tearing_control_v1::Request::Destroy => {
                stage_in(
                    &mut hints.lock_safe(),
                    &data.surface.id(),
                    TearingHint::Vsync,
                );
            }
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        _resource: &WpTearingControlV1,
        data: &TearingControlData,
    ) {
        // A client that exits without an explicit Destroy request would
        // otherwise leave the surface marked as holding a control object,
        // and a later get_tearing_control on a recycled surface would be
        // refused on behalf of a peer that no longer exists.
        if let Some(hints) = state.tearing_hints.as_ref() {
            release_control_in(&mut hints.lock_safe(), &data.surface.id());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The map keyed by a stand-in for `ObjectId`, which cannot be minted
    /// without a live display. Every rule under test is about the pair of
    /// halves and the entry's lifetime, neither of which the key affects.
    type Hints = HashMap<&'static str, TearingHintLatch>;

    const A: &str = "surface-a";
    const B: &str = "surface-b";

    fn armed(map: &mut Hints, key: &'static str) {
        assert!(claim_control_in(map, &key));
    }

    #[test]
    fn a_hint_takes_effect_at_the_commit_that_carries_its_buffer() {
        let mut map = Hints::new();
        armed(&mut map, A);
        stage_in(&mut map, &A, TearingHint::Async);
        // Staged but not committed: the render path still sees vsync, so a
        // frame is never presented under a rule its buffer was not drawn
        // for.
        assert_eq!(committed_in(&map, &A), TearingHint::Vsync);
        assert_eq!(async_count_in(&map), 0);

        assert!(commit_in(&mut map, &A));
        assert_eq!(committed_in(&map, &A), TearingHint::Async);
        assert_eq!(async_count_in(&map), 1);

        // A commit with nothing staged changes nothing and says so, so the
        // caller does not arm a redraw on every frame of every client.
        assert!(!commit_in(&mut map, &A));
        // Re-staging the value already committed is likewise not a change.
        stage_in(&mut map, &A, TearingHint::Async);
        assert!(!commit_in(&mut map, &A));
        assert_eq!(committed_in(&map, &A), TearingHint::Async);
    }

    #[test]
    fn a_surface_nobody_asked_about_is_vsync() {
        let map = Hints::new();
        // The protocol's default, and the safe answer: an absent entry needs
        // no special case at the call site.
        assert_eq!(committed_in(&map, &A), TearingHint::Vsync);
        assert_eq!(async_count_in(&map), 0);
    }

    #[test]
    fn a_surface_may_hold_only_one_control_object() {
        let mut map = Hints::new();
        assert!(claim_control_in(&mut map, &A));
        // The second get_tearing_control is the protocol error; without it
        // the two objects would share one hint and the surviving one's
        // request would depend on destruction order.
        assert!(!claim_control_in(&mut map, &A));
        // A different surface is unaffected.
        assert!(claim_control_in(&mut map, &B));

        // Once the object dies the surface may be claimed again — and the
        // revert to vsync is staged, not applied, exactly as the protocol
        // specifies for an explicit destroy.
        stage_in(&mut map, &A, TearingHint::Async);
        assert!(commit_in(&mut map, &A));
        release_control_in(&mut map, &A);
        assert_eq!(
            committed_in(&map, &A),
            TearingHint::Async,
            "the revert waits for the next commit"
        );
        assert!(commit_in(&mut map, &A));
        assert_eq!(committed_in(&map, &A), TearingHint::Vsync);
        assert!(claim_control_in(&mut map, &A));
    }

    #[test]
    fn an_idle_entry_is_recycled_and_a_live_one_is_not() {
        let mut map = Hints::new();
        armed(&mut map, A);
        stage_in(&mut map, &A, TearingHint::Vsync);
        // A live control object keeps its entry, or a second
        // get_tearing_control would be allowed on a surface that has one.
        assert!(!commit_in(&mut map, &A));
        assert!(map.contains_key(A));

        // Object gone, hint back to the default: nothing left to remember.
        release_control_in(&mut map, &A);
        assert!(!commit_in(&mut map, &A));
        assert!(!map.contains_key(A), "an idle entry is not kept forever");

        // An Async hint is state worth keeping even with no object left.
        armed(&mut map, B);
        stage_in(&mut map, &B, TearingHint::Async);
        assert!(commit_in(&mut map, &B));
        map.get_mut(B).expect("entry").control_alive = false;
        assert!(!commit_in(&mut map, &B));
        assert!(map.contains_key(B));
    }

    #[test]
    fn a_surface_that_dies_first_does_not_leave_its_hint_behind() {
        // The protocol permits destroying the wl_surface while keeping the
        // control object, so the object-death hook cannot be the only purge:
        // the entry would outlive the surface, keyed by an id the server may
        // later reuse for something entirely unrelated.
        let mut map = Hints::new();
        armed(&mut map, A);
        stage_in(&mut map, &A, TearingHint::Async);
        assert!(commit_in(&mut map, &A));
        assert_eq!(async_count_in(&map), 1);

        assert!(map.remove(A).is_some());
        assert_eq!(async_count_in(&map), 0);
        assert_eq!(committed_in(&map, &A), TearingHint::Vsync);
        assert!(map.remove(A).is_none(), "purging twice is not an error");
    }
}
