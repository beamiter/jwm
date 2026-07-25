//! Optional allocation counter for the Phase 5 performance contract.
//!
//! The `allocation_steady` benchmark scenario needs the number of heap
//! allocations performed while producing frames. Counting costs one relaxed
//! atomic increment per allocation, so it stays behind the off-by-default
//! `alloc-counter` feature; production builds keep the untouched system
//! allocator. `current()` reports `None` when the counter is not compiled
//! in, which the perf tooling records as a skipped scenario.

/// Total heap allocations since process start, when the `alloc-counter`
/// feature is compiled in.
#[must_use]
pub fn current() -> Option<u64> {
    #[cfg(feature = "alloc-counter")]
    {
        Some(counting::ALLOCATIONS.load(std::sync::atomic::Ordering::Relaxed))
    }
    #[cfg(not(feature = "alloc-counter"))]
    {
        None
    }
}

#[cfg(feature = "alloc-counter")]
mod counting {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicU64, Ordering};

    pub(super) static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

    struct CountingAllocator;

    // SAFETY: delegates every operation to the system allocator unchanged;
    // the counter increment has no effect on allocation behaviour.
    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            unsafe { System.alloc(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    #[global_allocator]
    static GLOBAL: CountingAllocator = CountingAllocator;
}

#[cfg(all(test, feature = "alloc-counter"))]
mod tests {
    #[test]
    fn allocations_are_counted() {
        let before = super::current().expect("counter compiled in");
        let data = vec![0_u8; 4096];
        let after = super::current().expect("counter compiled in");
        drop(data);
        assert!(after > before, "allocating must advance the counter");
    }
}
