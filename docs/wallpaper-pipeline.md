# Wallpaper pipeline and validation

The shared image pipeline serves static wallpaper and picker previews on X11
and native Wayland. The X11 compositor additionally renders Astronomy and the
interactive Solar System. The following describes the current pipeline,
including decoded-image reuse, independent output transitions and material
minification filtering.

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
then compare content, requested mode, and required sampling dimensions, so
moved/reordered outputs and compatible resizes keep textures and pending decodes.
Center keeps native pixels across every resize; other modes reuse a request when
neither output dimension grows. Failed requests still retry on refresh.
Unmatched jobs are cancelled and removed textures are retired once. Global
fallback outputs share the global image/crossfade rather than duplicating it.
Explicit monitor/tag image or mode rules own independent textures even when
those values currently equal the global setting. Changing the global wallpaper
therefore keeps a previously loaded, unchanged override visible, without an
unnecessary reload that could show the wrong fallback while pending or after
failure. Until an output's first successful decode, the existing global fallback
still applies.

Successful replacements of an already loaded monitor/tag image now use the same
configurable crossfade timing as the global image. Each output owns its previous
texture, layout and transition clock. Reordering preserves that owner; unplug,
effect cleanup, replacement and disabling crossfade retire it. Finishing a fade
requests the final repair before the tickless renderer returns to idle. Removing
an override fades to the global image, or to that backend's plain desktop when
there is no global image.

The draw pass adds complementary old/new RGB weights over an opaque backdrop,
preserving its alpha even on floating-point render targets.
This includes Fill/Fit/Center margins: an old image no longer remains in a new
letterbox until the last frame. Every output clears only its damage-intersected
scissor, preserving both partial repairs and last-writer behavior for overlapping
or mirrored outputs. This is interpolation in the wallpaper pass's encoded
color domain, not a claim of scene-linear or HDR crossfade processing.

### Decoded-image cache

A process-wide LRU retains up to **64 MiB of resized RGBA pixels and 24 entries**.
Completed repeat requests with the same source, layout and sampling size reuse
the pixel buffer through `Arc`; Center requests can reuse it across output
sizes because Center keeps native pixels. This benefits repeated tag switching
and revisiting picker previews. Simultaneous first loads still have independent
decode slots; this is completed-result reuse, not single-flight decoding.

Canonical path, decoder format, device/inode, file length, modification/change
timestamps, layout and target dimensions identify each entry. All filesystem
checks run on a worker. Cache publication also checks the actual decoded file
descriptor and rechecks the pathname, so replaced files and symlink retargeting
cannot publish another file's pixels under an old identity. Missing files do not
use stale entries; failed and cancelled work is not cached. An oversized result
can still be displayed but is not retained by the LRU.

The 64 MiB limit covers the cache's own retained pixels. Codecs, resampling,
initial conversion to `Arc`, active uploads and evicted buffers still held by
callers can allocate additional memory. It is not a process RSS limit. The
existing process-wide decode gate remains in force. Cancellation is checked
between decode, transparency conversion, resampling and publication.

Composited wallpapers and previews flatten transparent source pixels over black
before resampling. Hidden RGB can therefore neither appear as an opaque image
nor create bright fringes through filtering. The independent palette extractor
continues to ignore transparent color; compositor-off X11 retains its existing
native-root compositing policy.

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

### Source texture quality

Earth, Moon, cloud and sky maps receive mipmaps once when their successful
upload is adopted. Trilinear minification reduces distant texture aliasing and
moiré. Explicit analytic sphere/ray gradients keep the sample footprint stable
at the longitude seam, poles and branch boundaries. Moon pixels fully covered
by the opaque body skip the hidden background computation; the antialiased
silhouette still samples the background.

Mipmaps add approximately one-third to these source textures' GPU storage and
one generation step at adoption. They are not regenerated by ordinary redraws,
and do not reduce the full-resolution procedural frame cache or its precision.
If mip generation reports a GL error, the original base image stays usable with
linear filtering and an explicitly limited base level.

## Picker

Directory scanning retains at most 200 sorted image rows and visits at most
100,000 raw directory entries, including sidecars and errors. Beyond that bound,
sorting applies to the visited prefix. Close/reopen cancels obsolete scans
cooperatively; a blocked filesystem syscall cannot be interrupted. Worker-side
canonical matching recognizes tilde/symlink aliases without changing row paths,
retains the current image in the row cap, and ignores stale selection metadata
if configuration changed during scanning.

Typing filters the retained catalog by case-insensitive filename/path/effect
words, including Unicode text and spaces. Enter or a click applies the selected
row; Space is search text. Arrow/Tab, Home/End, PageUp/PageDown, mouse selection,
scrolling and preview all address the same filtered rows. The title shows the
selected position and match count. A scan completing while the user types keeps
the query and a still-matching highlight.

Directory errors, empty folders, unreadable entries, unrepresentable names and
scan/catalog caps have distinct status text. Searching a capped catalog covers
only its retained images, as stated in the status; it is not a recursive or
unbounded filesystem search.

The already-merged 4096-bucket palette extractor is unchanged by this update.

## Reproducible validation

Run with the repository toolchain and normal native development dependencies:

```sh
cargo fmt --all -- --check
cargo check --locked --all-targets --no-default-features
cargo check --locked --all-targets --no-default-features --features backend-x11rb
cargo check --locked --all-targets --no-default-features --features backend-wayland-udev
cargo clippy --locked --lib --bins --tests --no-deps -- -D warnings
scripts/test.sh --lib --bins --tests
JWM_REQUIRE_HEADLESS_GL=1 bash scripts/test-procedural-wallpaper-cache.sh
python3 scripts/test-astronomy-shader.py
python3 scripts/test-solar-system-shader.py
```

The existing CI feature matrix also covers XCB, nested Wayland and combined
profiles. The new cache test runs in CI and includes the exact production cache
module. It exercises surfaceless desktop GL and GLES, RGBA8/RGB10_A2, exact 10-bit
channel retention, damage scissoring, state restoration, repeated cache repairs,
resize invalidation, rejected-size fallback, and idempotent cleanup. It needs local
`glow` sources (fetched by Cargo), Rust, and an EGL runtime; unavailable support is
an explicit skip unless `JWM_REQUIRE_HEADLESS_GL=1`, which makes it fail.

The Rust suite also checks LRU budgets and shared pixel ownership, source
replacement and symlink identity, sampling-mode isolation, transparency,
search/input/scan lifecycle, and actual GLES crossfade pixels, output isolation,
damage scissors, overlap, floating-point target alpha and texture retirement. The source-material harness
checks actual mipmap pixels and fallback. The shader suites render GL/GLES
scene comparisons plus seam/pole probes; Astronomy adds high-frequency
checkerboard controls against the expected area average. Those synthetic
controls measure alias suppression, not visual sharpness of every photograph.

Real GPU occupancy/power, physical display HDR, multi-output hotplug, and desktop
visual inspection remain hardware acceptance work. Passing the cache harness is
not evidence that those hardware scenarios have been tested.
