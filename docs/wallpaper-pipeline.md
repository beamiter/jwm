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
Configured `~` and `~/...` paths resolve against the session home directory
inside the worker. Decoding and cache identity use that same resolved path;
the compositor retains the original configuration spelling. Relative paths,
`~user` and environment-variable syntax retain their literal meaning.
Jobs, including cache identity checks, reuse a lazily started pool of at most
four threads. Its four-slot nonblocking queue is covered by the same gate:
queued plus running decode jobs cannot exceed the permit count. Initialization
contention leaves the request queued, failed initialization can be retried, and
a codec job panic releases its permit without killing the pool worker.

Regular files and symlinks to regular files are accepted. FIFO and directory
inputs are rejected without waiting for a FIFO writer. Extension-based format
selection is retained. Ordinary source files are capped at 64 MiB, each dimension
at 16,384 pixels, and decoded/RGBA output at 256 MiB. Embedded orientation is
applied before layout; copying quarter-turns reserve additional allocation.
Sky textures retain their stricter caller-provided source/dimension limits and
64 MiB allocation budget. Reads use a bounded snapshot even if the file grows.
The codec allocation budget is best-effort: codec-internal overhead, and some
allocations before decoder limits take effect, are not a strict process RSS cap.

Compositor-off X11 uses the same bounded regular-file decoder and orientation
handling, while retaining its existing content-based format detection. Its
native-root compositor validates the canvas before loading sources, decodes
each source on first output use, and releases it after its last output. This
preserves monitor draw order and shared-source reuse while shortening source
buffer lifetimes. It does not impose an aggregate process memory cap.
The event thread compares borrowed output/configuration values without file
access or per-frame request strings. Existing maintenance ticks admit at most
one background file probe per second while images are configured, without
arming a separate polling timer. Each unique path is checked once, and an unchanged source
signature skips both decoding and canvas allocation. Solid-colour canvas fills
also run off-thread. Output/tag/F5 changes coalesce into the newest generation;
obsolete work skips conversion/resizing after its current codec call, and a
compositor handoff retains that one receiver until it completes.
Native-root source signatures are committed only after an atomic installation
acknowledgement. A failed source, placement or pixmap upload keeps the previous
canvas and retries the same version with a 1/2/4/8/16/30-second capped delay.
Failed codecs begin their delay after they finish; failed thread starts are
also backed off. Metadata probes still detect a changed file version promptly,
and a new input generation or F5 resets backoff. One failing output does not
replace the working root with a partially composed image.
Codec/resize panics use the same failure path. An unexpected native worker
panic also publishes completion, so its retained receiver cannot permanently
block later refreshes.
Ordinary configuration applications preserve native decode ownership; the
fingerprint detects relevant image/layout/base changes. Explicit file reload,
F5 and reapplying the current image still invalidate it. Applying extracted
border colours therefore does not decode the same native wallpaper again.

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
and revisiting picker previews. Simultaneous first loads with the same cache
identity share one decode and resize, including cold loads requested by several
monitors. The output slots remain independent; only their source work and
resulting pixel buffer are shared.

Workers that find a busy matching in-flight request return its handle and
release their decode permit immediately. Waiting slots poll only an in-memory
lock, creating no waiting thread and doing no filesystem work until the
producer releases it. Their next bounded worker rechecks the source identity
before reusing the result, so unrelated images can continue decoding even when
several outputs request the same source. A cancelled waiter does not cancel the
other outputs. If the producer fails or is cancelled, a live waiter can retry.
Weak references track active shared work without retaining a second pixel cache
after its users leave, including for results too large for the LRU. Waiting and
requeued slots retain their shared result until the retry worker has checked it.

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
premultiplies alpha before thumbnailing, then restores source RGB and weights
the retained pixels by alpha. This prevents invisible RGB from creating a
false hue while preserving source colours independently of the desktop base.
Compositor-off X11 retains its existing native-root compositing policy.
Opaque RGBA pixels bypass the RGB premultiplication arithmetic, while fully
transparent pixels are cleared directly; partially transparent pixels retain
the same rounded integer formula. Palette selection uses two linear histogram
scans instead of allocating and sorting occupied buckets. RGBA8 and LumaA8
palette thumbnails premultiply as pixels are sampled, preserving the former
pixel rounding without a separate full-image write pass; LumaA8 also avoids a
full-size RGBA conversion. Unchanged daily collections retain their path
buffers across unrelated config applies.

Palette extraction keeps the last applied palette when decoding fails or no
usable colour remains. It releases the failed request so a later config apply
can retry the same path, without scheduling retries from the failure itself.
Clearing the configured wallpaper drops pending extraction ownership, preventing
late results from changing the theme.
Palette extraction resolves the first valid display's actual monitor/tag image
or daily/manual selection. Tag switches and output changes refresh that source;
changing the focused monitor does not. Before adoption it rechecks the source
and extraction switch, preventing a late palette from retinting a new selection.
Its decoder shares the regular-file and resource limits of wallpaper loading.

## Procedural cache

Astronomy and solar-system rendering cache a full-resolution GPU frame, choosing
RGBA8 or RGB10_A2 to follow rendering-destination precision, including SDR
postprocessing targets. Unrelated window damage copies the
cached image instead of rerunning the procedural shader. Astronomy copies only
its canvases, preserving static wallpaper gaps. Material, layout/camera, sampled
time, size, and precision changes invalidate the cache. Allocation failure falls
back to direct rendering; disable/cleanup releases resources.

Each renderer checks the current resolved output clips against conservative
opaque client coverage. Hidden clips do not draw or contribute animation work:
a covered Solar scene or fast Moon cannot keep another exposed scene ticking.
Rounded, translucent, shaped, transformed or transitioning clients remain
conservative, and even a one-pixel gap keeps its scene visible. A shared scene
group stays interactive while any of its clips is visible; fully hidden groups
release their pending input. A visibility or topology change invalidates the
cache, so revealing a clip immediately paints it using a fresh time sample.
If overlapping outputs fragment one renderer into more than 128 clips, that
renderer conservatively keeps all its clips visible instead of running an
excessive repetition of coverage checks. This trades rare-layout power
savings for predictable CPU work and never suppresses visible pixels.
Wallpaper-only bar luminance readback is deferred while any celestial clip is
omitted, rather than sampling its static fallback or waking hidden animation.

Astronomy updates adapt between 1 and 15 Hz from each visible body's admitted
radius and rotation speed, plus the sky's admitted perspective FOV and full
canvas geometry. The sky budget accounts for both center and corner motion,
aiming at roughly half a pixel per update; the 15 Hz ceiling takes priority for
fast or strongly zoomed scenes. Earth and Moon retain a 1 Hz physical-light
refresh. A stationary Galaxy has no ordinary animation deadline once its maps
are settled. Input, configuration changes and newly visible clips still repaint;
pending visible maps poll at a bounded 1 Hz until adoption, and separate 2-second
power checks do not themselves invalidate unchanged pixels. Solar keeps its
existing visible cadence and pauses when all of its own clips are hidden.
No real-display FPS, power, or HDR improvement is claimed from software-renderer
or CPU scheduling tests.

### Solar wheel targets

Solar wheel picking and camera/sky routing use the same actually displayed
view. A wheel arriving before an entered or exited close-up has been presented
takes over that visible view, even if the other transition was already admitted
for rendering. Same-focus wheel bursts continue accumulating pending zoom.
Retaining a displayed close-up also retains its overview return destination;
zero, invalid, or saturated wheel requests do not cancel a queued transition.

Solar drags instead keep the projection captured at press time, including when
an already-admitted frame appears during capture. Valid motion that resumes a
close-up after a queued exit preserves its original overview return view.
Refused grabs, stationary empty-sky releases, and invalid motion leave the
queued exit intact. Canceling after valid motion retains the moved pose and
its return view, without activating the pressed planet.

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
100,000 raw entries per directory, including sidecars and errors. Beyond that bound,
sorting applies to the visited prefix. Close/reopen cancels obsolete scans
cooperatively; a blocked filesystem syscall cannot be interrupted. Worker-side
canonical matching recognizes tilde/symlink aliases without changing row paths,
retains the current image in the row cap, and ignores stale selection metadata
if configuration changed during scanning.

Retained aliases are deduplicated within and between the primary and bundled
collections. Bundled identities reserve their slots before the primary list is
truncated, preserving a primary alias even near its cutoff. Scan-limit status
is recorded before directory summaries are merged, so two complete scans do
not incorrectly report a partial scan from their combined entry count.
Rows with duplicate filenames include their full parent paths; unique names
keep their compact labels. Path keys, search and current-image markers are
unchanged.

Typing filters the retained catalog by case-insensitive filename, path, effect
or control words, including Unicode text and spaces. Enter or a click applies the selected
row; Space is search text. Arrow/Tab, Home/End, PageUp/PageDown, mouse selection,
scrolling and preview all address the same filtered rows. The title shows the
selected position and match count. A scan completing while the user types keeps
the query and a still-matching highlight. `F5` and **Refresh image library**
reuse the same asynchronous path while preserving the query, retained selection
and display target. An active scan absorbs repeated refresh requests.
An explicit refresh is consumed once when its scan completes: both compositors
requeue global images, output overrides and the current side preview, while
native-root composition and accent extraction also refresh. The image cache
still checks file identity on its workers. Current textures and their layouts
survive failed reloads. Closing or replacing the picker discards its deferred
refresh; ordinary opening does not force reloads. Reapplying the current image
or successfully reloading the configuration also refreshes same-path content.

Sizing, crossfade, accent colours and daily-library rotation controls apply
through validated live-config transactions and leave the panel open. Their
changes are session-only. Rotation takes a snapshot of every image row in the
retained source catalog, including bundled and current images within the
200-image cap; filtering never changes that input. Stopping rotation restores
the static rules. Celestial view/spin controls retain the existing saved
configuration path, as documented in [Astronomy](astronomy-wallpaper.md).

Directory errors, empty folders, unreadable entries, unrepresentable names and
scan/catalog caps have distinct status text. Searching a capped catalog covers
only its retained images, as stated in the status; it is not a recursive or
unbounded filesystem search.

## Reproducible validation

Run with the repository toolchain and normal native development dependencies:

```sh
cargo fmt --all -- --check
cargo check --locked --all-targets --no-default-features
cargo check --locked --all-targets --no-default-features --features backend-x11rb
cargo check --locked --all-targets --no-default-features --features backend-wayland-udev
cargo clippy --locked --lib --bins --tests --no-deps -- -D warnings
scripts/test.sh --lib --bins --tests
EGL_PLATFORM=surfaceless LIBGL_ALWAYS_SOFTWARE=1 JWM_REQUIRE_HEADLESS_GL=1 \
  scripts/test.sh --lib --no-default-features --features backend-wayland-udev \
  -- wayland_wallpaper_explicit_refresh
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

The Rust suite also checks LRU budgets and shared pixel ownership, overlapping
cold decodes and cancellation, source replacement and symlink identity,
sampling-mode isolation, transparent palette sampling, search/input/scan
lifecycle, and actual GLES crossfade pixels, output isolation,
damage scissors, overlap, floating-point target alpha and texture retirement. The source-material harness
checks actual mipmap pixels and fallback. The shader suites render GL/GLES
scene comparisons plus seam/pole probes; Astronomy adds high-frequency
checkerboard controls against the expected area average. Those synthetic
controls measure alias suppression, not visual sharpness of every photograph.

Deferred-load tests additionally hold a producer busy while several same-source
waiters and an unrelated request compete for a small gate, verify non-LRU pixel
sharing across requeueing, and cover cancellation, failed starts and source
replacement during the wait. Refresh tests cover scan coalescing/cancellation,
same-path palette replacement, and real Wayland GL texture/layout preservation
after failed loading followed by successful same-path image and preview retry.

Real GPU occupancy/power, physical display HDR, multi-output hotplug, and desktop
visual inspection remain hardware acceptance work. Passing the cache harness is
not evidence that those hardware scenarios have been tested.

## CPU microbenchmarks

```sh
python3 scripts/bench-wallpaper-rotation.py
python3 scripts/bench-wallpaper-decode.py
scripts/test.sh --release --lib --no-default-features \
  wallpaper_colors_performance_comparison -- --ignored --nocapture
```

The scripts compile with `rustc -O` and use current production code for the
optimized path. The ignored colour test uses the same Cargo release profile
for both paths, checks equal pixels/palettes and excludes source decoding.
The rotation comparison measures 10,000 unchanged applications of 200 paths:
the former clone made 2,010,000 allocations totalling 104 MB, while retaining
the collection made zero. A local optimized run measured about 4.6–4.7 µs
versus 0.32–0.33 µs per application. Timing varies with scheduling and hardware.

For 160×160 palette samples, the local sorted histogram took about 463 µs
versus 292 µs with linear selection. At 3840×2160, RGBA8 alpha thumbnailing
took about 15.3–15.4 ms versus 14.9 ms; LumaA8 took about 66.5–67.2 ms versus
11.0 ms. These are CPU-stage comparisons, not end-to-end wallpaper latency,
GPU, desktop FPS or power measurements.
The reproducible dispatch/pixel script measured a local median of about
41.4 ms versus 9.0 ms for 2,000 dispatch round trips. For twelve 4K RGBA buffers,
opaque processing took about 114.8 ms versus 41.6 ms and transparent processing
119.0 ms versus 40.1 ms; partially transparent and mixed-alpha inputs were
approximately unchanged. The dispatch comparison excludes pool startup.
