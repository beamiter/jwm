# Wallpaper pipeline and validation

This foundation update improves the existing static and astronomy/solar-system
wallpapers. It does not add video, parallax, a shader library, or interactive
wallpaper features.

## Loading and display

X11 and Wayland use the same lazy decode slots and process-wide gate (at most
four concurrent decodes, or fewer on a small CPU). Queued requests do not create
waiting threads. Dropping a replaced/closed request cancels queued work; running
codecs finish their current call, then skip obsolete resize/upload work. Separate
outputs keep independent slots. Preview closure uses the same cancellation path.

Regular files and symlinks to regular files are accepted. FIFO and directory
inputs are rejected without waiting for a FIFO writer. Extension-based format
selection is retained. Ordinary source files are capped at 64 MiB, each dimension
at 16,384 pixels, and decoded/RGBA output at 256 MiB. Embedded orientation is
applied before layout; copying quarter-turns reserve additional allocation.
Sky textures retain their stricter caller-provided source/dimension limits and
64 MiB allocation budget. Reads use a bounded snapshot even if the file grows.
The codec allocation budget is best-effort: codec-internal overhead, and some
allocations before decoder limits take effect, are not a strict process RSS cap.

Lanczos3 downsampling is mode-aware: Fill retains enough pixels to cover both
axes; Fit fits inside the target; Stretch reduces axes independently; Center
preserves native pixels and is clipped by the output. Upscaling is left to GPU
sampling. Texture upload validates dimensions/byte count and restores texture,
unpack-buffer, alignment, row-length and skip state on success and failure.

Global image changes use smoothstep crossfades. The old texture and its layout
remain visible until a replacement decode and upload succeed. Monitor refreshes
match generation-bearing monitor identities separately from configuration slots,
then compare content, requested mode, and sampling dimensions, so moved/reordered/unchanged outputs keep textures and pending decodes.
Unmatched jobs are cancelled and removed textures are retired once. Global
fallback outputs share the global image/crossfade rather than duplicating it.

## Procedural cache

Astronomy and solar-system rendering cache a full-resolution GPU frame, choosing
RGBA8 or RGB10_A2 to follow rendering-destination precision, including SDR
postprocessing targets. Unrelated window damage copies the
cached image instead of rerunning the procedural shader. Astronomy copies only
its canvases, preserving static wallpaper gaps. Material, layout/camera, sampled
time, size, and precision changes invalidate the cache. Allocation failure falls
back to direct rendering; disable/cleanup releases resources.

Astronomy updates adapt between 1 and 15 Hz using visible decorative rotation
speed and canvas radius. Physical sunlight refreshes at least once per second;
wall-clock sampling and pause/resume behavior are preserved. Solar-system timing
keeps its existing cadence. No real-display FPS, power, or HDR improvement is
claimed from software-renderer tests.

## Picker

Directory scanning retains at most 200 sorted image rows and visits at most
100,000 raw directory entries, including sidecars and errors. Beyond that bound,
sorting applies to the visited prefix. Close/reopen cancels obsolete scans
cooperatively; a blocked filesystem syscall cannot be interrupted. Worker-side
canonical matching recognizes tilde/symlink aliases without changing row paths,
retains the current image in the row cap, and ignores stale selection metadata
if configuration changed during scanning.

The already-merged 4096-bucket palette extractor is unchanged by this update.

## Reproducible validation

Run with the repository toolchain and normal native development dependencies:

```sh
cargo fmt --all -- --check
cargo check --locked --all-targets --no-default-features
cargo check --locked --all-targets --no-default-features --features backend-x11rb
cargo check --locked --all-targets --no-default-features --features backend-wayland-udev
cargo clippy --locked --lib --bins --tests --no-deps -- -D warnings
scripts/test.sh --locked --lib --bins --tests
JWM_REQUIRE_HEADLESS_GL=1 bash scripts/test-procedural-wallpaper-cache.sh
```

The existing CI feature matrix also covers XCB, nested Wayland and combined
profiles. The new cache test runs in CI and includes the exact production cache
module. It exercises surfaceless desktop GL and GLES, RGBA8/RGB10_A2, exact 10-bit
channel retention, damage scissoring, state restoration, repeated cache repairs,
resize invalidation, rejected-size fallback, and idempotent cleanup. It needs local
`glow` sources (fetched by Cargo), Rust, and an EGL runtime; unavailable support is
an explicit skip unless `JWM_REQUIRE_HEADLESS_GL=1`, which makes it fail.

Real GPU occupancy/power, physical display HDR, multi-output hotplug, and desktop
visual inspection remain hardware acceptance work. Passing the cache harness is
not evidence that those hardware scenarios have been tested.
