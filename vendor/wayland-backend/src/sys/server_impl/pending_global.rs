//! Shared, FFI-free identity checks for the system backend's global lifecycle.
use scoped_tls::scoped_thread_local;
use std::{
    any::TypeId,
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

pub(super) fn registered_identity<'a>(
    known: impl Iterator<Item = (*mut c_void, &'a Arc<AtomicBool>)>,
    pointer: *mut c_void,
    alive: &Arc<AtomicBool>,
) -> bool {
    alive.load(Ordering::Acquire)
        && known
            .into_iter()
            .any(|(p, a)| p == pointer && Arc::ptr_eq(a, alive))
}

// wl_global_create broadcasts before returning the pointer that can be added
// to known_globals. Keep a typed, display-bound identity only for that FFI call.
// The same window exists while destroying a removed known global.
pub(super) struct PendingGlobal {
    pub(super) state: *const (),
    pub(super) state_type: TypeId,
    pub(super) userdata: *mut c_void,
}
scoped_thread_local! {
    #[allow(unsafe_op_in_unsafe_fn)]
    pub(super) static PENDING_GLOBAL: PendingGlobal
}
impl PendingGlobal {
    pub(super) fn matches(
        &self,
        state: *const (),
        state_type: TypeId,
        userdata: *mut c_void,
    ) -> bool {
        !userdata.is_null()
            && self.state == state
            && self.state_type == state_type
            && self.userdata == userdata
    }
}

#[cfg(test)]
mod jwm_pending_global_tests {
    use super::*;
    #[derive(Clone)]
    struct TestId {
        ptr: *mut c_void,
        alive: Arc<AtomicBool>,
    }
    fn registered(known: &[TestId], id: &TestId) -> bool {
        registered_identity(known.iter().map(|g| (g.ptr, &g.alive)), id.ptr, &id.alive)
    }

    #[test]
    fn lifecycle_access_requires_registered_live_exact_identity() {
        // These pointers are identities only; the pure guard never dereferences them.
        let mut token = 0u8;
        let pending = TestId {
            ptr: (&mut token as *mut u8).cast(),
            alive: Arc::new(AtomicBool::new(true)),
        };
        let mut known = Vec::new();
        assert!(!registered(&known, &pending));
        known.push(pending.clone());
        assert!(registered(&known, &pending));
        let other_lifetime = TestId {
            ptr: pending.ptr,
            alive: Arc::new(AtomicBool::new(true)),
        };
        assert!(!registered(&known, &other_lifetime));
        let wrong_pointer = TestId {
            ptr: std::ptr::null_mut(),
            alive: pending.alive.clone(),
        };
        assert!(!registered(&known, &wrong_pointer));
        pending.alive.store(false, Ordering::Release);
        assert!(!registered(&known, &pending));
        known.clear();
        assert!(!registered(&known, &pending));
    }

    #[test]
    fn pending_global_identity_requires_exact_display_type_and_userdata() {
        let state_a = 1u8;
        let state_b = 2u8;
        let mut data_a = 3u8;
        let mut data_b = 4u8;
        let pending = PendingGlobal {
            state: (&state_a as *const u8).cast(),
            state_type: TypeId::of::<u8>(),
            userdata: (&mut data_a as *mut u8).cast(),
        };
        assert!(pending.matches(pending.state, pending.state_type, pending.userdata));
        assert!(!pending.matches(
            (&state_b as *const u8).cast(),
            pending.state_type,
            pending.userdata
        ));
        assert!(!pending.matches(pending.state, TypeId::of::<u16>(), pending.userdata));
        assert!(!pending.matches(
            pending.state,
            pending.state_type,
            (&mut data_b as *mut u8).cast()
        ));
        assert!(!pending.matches(pending.state, pending.state_type, std::ptr::null_mut()));
    }

    #[test]
    fn nested_pending_scope_restores_outer_and_clears_after_return() {
        let state = 1u8;
        let mut outer_data = 2u8;
        let mut inner_data = 3u8;
        let outer = PendingGlobal {
            state: (&state as *const u8).cast(),
            state_type: TypeId::of::<u8>(),
            userdata: (&mut outer_data as *mut u8).cast(),
        };
        let inner = PendingGlobal {
            state: outer.state,
            state_type: TypeId::of::<u16>(),
            userdata: (&mut inner_data as *mut u8).cast(),
        };
        assert!(!PENDING_GLOBAL.is_set());
        PENDING_GLOBAL.set(&outer, || {
            PENDING_GLOBAL
                .with(|p| assert!(p.matches(outer.state, outer.state_type, outer.userdata)));
            PENDING_GLOBAL.set(&inner, || {
                PENDING_GLOBAL.with(|p| {
                    assert!(p.matches(inner.state, inner.state_type, inner.userdata));
                    assert!(!p.matches(outer.state, outer.state_type, outer.userdata));
                });
            });
            PENDING_GLOBAL
                .with(|p| assert!(p.matches(outer.state, outer.state_type, outer.userdata)));
        });
        assert!(!PENDING_GLOBAL.is_set());
    }
}
