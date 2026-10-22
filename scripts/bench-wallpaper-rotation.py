#!/usr/bin/env python3
"""Compare retained rotation paths with the former per-apply clone, using rustc -O.

Only the production state and its pure impl are extracted; no desktop or Cargo
build is needed. Allocation counters include only repeated configure calls.
"""

from pathlib import Path
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parents[1]
source = (ROOT / "src/jwm/features/wallpaper_rotation.rs").read_text()
start = source.index("const CHECK_INTERVAL:")
end = source.index("pub(crate) fn local_day()")
production = source[start:end]

HARNESS = r"""
use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
static ALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
struct Counting;
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(size as u64, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, size) }
    }
}
#[global_allocator]
static ALLOCATOR: Counting = Counting;
"""

REFERENCE = r"""
// Exact pre-optimization configure behavior: clone every enabled collection.
fn baseline_configure(state: &mut WallpaperRotationState, mode: &str,
    paths: &[String], day: i32, now: Instant) -> bool {
    let enabled = mode == "daily" && !paths.is_empty();
    let changed = state.day.is_some() != enabled
        || (enabled && (state.paths != paths || state.day != Some(day)));
    state.initialized = true;
    state.paths = if enabled { paths.to_vec() } else { Vec::new() };
    state.day = enabled.then_some(day);
    state.next_check = enabled.then_some(now + CHECK_INTERVAL);
    changed
}
fn main() {
    let paths: Vec<_> = (0..200)
        .map(|i| format!("/pictures/wallpapers/{i:03}.png")).collect();
    let now = Instant::now();
    let calls = 10_000;
    println!("variant,calls,ns_per_call,allocations,allocated_bytes");
    for optimized in [false, true, true, false] {
        let mut state = WallpaperRotationState::default();
        state.configure("daily", &paths, 100, now);
        let allocations = ALLOCS.load(Ordering::Relaxed);
        let bytes = BYTES.load(Ordering::Relaxed);
        let start = Instant::now();
        for _ in 0..calls {
            black_box(if optimized {
                state.configure(black_box("daily"), black_box(&paths), 100, now)
            } else {
                baseline_configure(&mut state, black_box("daily"), black_box(&paths), 100, now)
            });
        }
        let elapsed = start.elapsed();
        let allocations = ALLOCS.load(Ordering::Relaxed) - allocations;
        let bytes = BYTES.load(Ordering::Relaxed) - bytes;
        println!("{},{calls},{:.1},{allocations},{bytes}",
            if optimized { "retained" } else { "clone" },
            elapsed.as_secs_f64() * 1e9 / calls as f64);
    }
}
"""

with tempfile.TemporaryDirectory(prefix="jwm-rotation-bench-") as directory:
    work = Path(directory)
    rust = work / "bench.rs"
    binary = work / "bench"
    rust.write_text(HARNESS + production + REFERENCE)
    subprocess.run(
        ["rustc", "--edition=2024", "-O", "-A", "dead_code", str(rust), "-o", str(binary)],
        check=True,
    )
    subprocess.run([str(binary)], check=True)
