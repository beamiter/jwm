# Native Wayland viewer

This backend displays the existing authenticated remote video stream in a
native `xdg_toplevel`. It is separate from the native Wayland **source** described
in [remote-wayland-source.md](remote-wayland-source.md). The default viewer remains
X11; selection is explicit and never falls back to X11.

```sh
# Build with the remote-wayland feature, then connect explicitly read-only.
jwm-remote connect HOST:48221 --viewer-backend wayland --view-only \
  --key-file /path/to/session.key
# Optional: --viewer-wayland-display wayland-0 --window-size 1280x720
```

The backend uses the selected native socket, or `WAYLAND_DISPLAY` below the
absolute `XDG_RUNTIME_DIR`. An explicit absolute socket path is also accepted.
In view-only mode the viewer opens the native socket and presents frames.
The initial local size defaults to 800x600, subject to the compositor's
configure events. Authentication, decoding, frame limits and transport shutdown
remain in the shared client. See the [capture and viewer capability matrix](remote-control.md#capture-and-viewer-capabilities).

Set `JWM_REMOTE_READ_AHEAD=1` for a native view-only connection to enable
the bounded authenticated reader and ordered decoder. A switched source
still requires its own presentation feedback before the viewer confirms
the new selection. [Scheduling controls](remote-control.md#experimental-scheduling-controls)

With `remote-h264`, a read-only viewer can negotiate H.264 when the hardware
decoder probe passes. Set `JWM_REMOTE_GPU_PRESENT` to the compositor's DRM
render node to keep those decoded pixels on the GPU through the Wayland
buffer submission. Eligible compositors advertise linear XRGB8888 on that
device. The default presentation path downloads decoded pixels to XRGB SHM,
which also handles a rejected DMA-BUF import. Scoped input and proof modes use
JPEG. [GPU configuration](remote-control.md#experimental-scheduling-controls)

For native view-only measurement, `JWM_REMOTE_WAYLAND_REUSE_RGB=1` enables
experimental reuse of the XRGB DMA-BUF backing. The viewer keeps at most two
backings. Each attachment has a separate `wl_buffer`; the backing becomes
reusable only after that object's release. Layout changes retire outstanding
backings and discard idle storage from the old layout. Actual presentation
feedback still determines display acknowledgements.

The viewport and input coordinates are in logical pixels. The SHM buffer
follows the compositor's preferred integer scale, or the highest scale of
the outputs the surface has entered. The viewer reduces the buffer scale
if necessary to stay within its pixel and storage limits. Scaling also
changes the presentation generation. Managed input waits for a frame
presented at the new scale before it accepts new pointer actions.

## Deliberately narrow permissions

Choose `--view-only` or one explicitly scoped input mode. The available modes
are keyboard, pointer, unified control, explicit text and popup pointer.
Scoped input needs the corresponding host authorization and presentation proof.
Each scoped lease has a fixed source. Applications using the Rust
`jwm::remote::engine::RemoteSession` API can select another source from the
authenticated host catalog in view-only mode, including native Wayland
windows explicitly selected on the host. Automatic reconnect, clipboard
and legacy XTEST grabs are unavailable in the native viewer.

The managed session command accepts one-shot combined control with
`jwm-remote session HOST --viewer-backend wayland --control`. Both helpers
must support confirmed native release for this mode. The controller
can release an active lease and keep viewing the same source. Losing keyboard
or pointer focus, or deactivating the viewer window, also releases the lease.
After the host compositor processes the revoke, the viewer continues with
read-only resize handling. A managed session with several locally authorized
windows can switch sources, including while control is active. The viewer
waits for the host compositor to confirm old input cleanup before replacing
the old surface. After the new window actually presents, **Request Input**
can activate its unused one-shot lease. Using that action requires a focused
viewer and a fresh native proof. Start a new session and obtain fresh host
authorization to control the same window again.

During managed control, resizing the local viewer with keyboard and pointer
focus keeps the same surface and lease. The viewer fits the existing encoded
stream to the new viewport. It retires superseded commits and clears the old
pointer mapping and partial click.
The new pointer mapping requires actual presentation and a matching host Ready
record. Keyboard edges still target the same authorized surface, including a
release for a key pressed before resizing. Once the lease ends, the viewer can
report its current size for host-side encoding again.

## Presentation is not buffer release

The compositor must advertise `wp_presentation`; absence is an error. Only a
matching `presented` feedback for the submitted surface commit can produce a
display acknowledgement. `wl_buffer.release` only permits storage reclamation;
ordinary frame callbacks are not evidence that pixels were displayed.

The viewer tracks at most two unresolved presentation feedback objects. In
view-only mode a size change retires an older commit. A late presentation or
discard for that commit drains its feedback without granting display credit.
The viewer resubmits the retained frame at the latest configured size when
the host still awaits its ACK. A frame already acknowledged is redrawn locally
without a second ACK. An invalid, timed-out or discarded current submission
ends the session. In scoped modes an unexpected configure or source change
that invalidates outstanding presentation still fails closed and revokes
input authority.

An explicit view-only source selection destroys the old xdg role and surface,
clears the retained image, and creates a fresh toplevel. The host must confirm
the selected source ID at the next epoch. The new surface waits for its own
configure before attaching pixels, and the controller publishes the new
catalog only after that source's first matching presentation. Feedback from
the previous role cannot generate an ACK. A source change outside the matching
selection ends the native viewer session.

SHM storage uses a bounded pool of at most two slots. Once the compositor
releases a buffer, its storage can be reused for a later frame of the same size
instead of creating a new memfd and protocol buffer for every frame. The viewer
retains its last submitted RGB canvas and tile snapshot. New tile snapshots
patch that canvas against the actual submitted predecessor, so frames skipped
by the latest-only mailbox cannot lose changes. JPEG frames or damage with an
unknown predecessor use a complete upload.

Each slot independently accumulates destination damage until its next upload,
including while busy. A released slot is patched with every change since its
last upload, not just the latest frame's rectangle. At most eight rectangles
are retained per slot; fragmented or large updates conservatively merge or
become a full-frame update. Nearest-neighbor scaling maps both rectangle edges
with ceiling division, including odd-size scaling and source pixels that are
not sampled at all. Only changed destination spans are converted and written.
New storage, changed source geometry or local configure generations rebuild
the complete image and black letterboxing. Resize-and-restore also invalidates
the baseline. The compositor retains the surface content between updates.

Conversion reuses one 256 KiB scratch buffer. Full-width updates batch complete
rows; partial-width spans seek and write only their own bytes. The scratch and
retained decoded canvas are additional CPU memory, outside the 128 MiB SHM-slot
budget. This is incremental CPU upload, not zero-copy presentation. A validated
unchanged frame needs no SHM writes, but still attaches its buffer, damages the
surface and requests fresh presentation feedback. This preserves keepalive and
native input-proof rendering; no ACK is inferred from unchanged pixels. Every
required upload must succeed before the buffer is attached and committed.

Idle and busy slots both count toward the 128 MiB aggregate ceiling, including
buffers still held from an earlier size. Each buffer also has checked width,
height, stride and pixel limits. A size change may replace only idle storage.
Replacement first destroys the released idle slot but keeps its old bytes
charged until a real `wl_display.sync` callback confirms the server processed
the preceding destroy request; only then may replacement storage be allocated.
A five-second fence timeout terminates the session. In view-only mode a
configure during the fence updates the dimensions used for replacement.
The fence accounts for protocol resources and confirms the destroy request.
Compositor caches and physical memory follow the compositor's lifecycle.
Old-size busy storage remains charged and cannot be overwritten. A compositor
may retain the current buffer until a replacement arrives: a
presented-but-unreleased slot stays intact while the second slot carries the
replacement. Busy storage is never rewritten.

Storage identity is separate from submission identity. Every commit requests
its own presentation feedback, and only that commit's matching `presented`
event can produce its display ACK; neither a release nor reuse of a previously
presented slot is sufficient. Missing presentation feedback or two-slot
submission blockage has a five-second deadline. A single presented current
buffer may legitimately remain held indefinitely during a static image; it
consumes only its bounded slot. These are native-upload bounds, not a claim
about total process memory including the decoder and transport.

## Validation boundary

The native viewer tests use an owned Wayland server, real protocol dispatch,
SHM buffers and production software rendering to inspect pixels. Their
compositor deliberately controls presentation, release and discard events to
verify the viewer's contract. The authenticated TCP test exercises the normal
client and in-process controller with a private Wayland socket. It checks
viewport changes, old callback retirement, a fresh configured source surface,
rendered pixels and the source-scoped ACK.

Those tests do **not** establish physical display timing, production KMS/DRM
pageflip correctness, a real two-machine session, hardware encoding or NVIDIA
performance. The local compositor is trusted to tell the truth about
presentation. JWM's existing production presentation batching is a separate
integration concern: a vblank must not acknowledge a newer surface commit that
was not part of the completed output submission. Protocol-level viewer tests do
not replace that audit or a real hardware acceptance run.

## Offline upload measurement

The deterministic incremental-upload tests compare patched storage against an
independent complete-image reference across odd upscales, downscales and black
bars. They also check alternating/held-slot damage accumulation, bounded region
counts, untouched neighboring bytes, zero-write keepalives and I/O failures:

```sh
cargo test --locked --no-default-features --features remote-wayland \
  --lib remote::wayland_viewer::damage::tests::
```

A counted 3x2 patch in a 100x80 destination writes 24 bytes in two spans, versus
32,000 bytes for a complete upload. This is a deterministic byte/write-count
check, not a runtime speedup claim. Partial-width patches use one seek/write per
affected row; broad damage can have different syscall costs from a full upload.

An opt-in test calls the production chunked upload helper and the historical
row-at-a-time reference against separate owned memfds. It opens no display or
socket and adds no runtime viewer option. This measures the full-upload fallback,
not incremental patch performance:

```sh
cargo test --locked --no-default-features --features remote-wayland \
  --lib remote::wayland_viewer::upload_benchmark::offline_memfd_upload_samples \
  -- --ignored --exact --nocapture
```

The `JWM_UPLOAD_BENCHMARK` JSON line records three bounded input/output cases,
byte-for-byte output comparison, counted `Write` calls and bytes, scratch size
and initialization time, first-use samples, and five subsequent raw nanosecond
samples per implementation with alternating execution order. First-use samples
are separate from subsequent samples; they are not claimed to be cold-cache
measurements. Input generation, memfd creation/sizing, seeks and pixel checks
are outside upload timings. The reference's per-frame row allocation is inside
its upload timing; production scratch initialization is reported separately.

These samples include CPU conversion and writes to memfd, not Wayland protocol
submission, compositor rendering, GPU work, network or physical display latency.
`Write` counts describe actual wrapper calls, including any short-write retries,
not independently traced syscalls. Preserve the JSON with the source revision,
dirty-tree status and build profile; `debug_assertions` is included in the
report. Default debug runs establish correctness and call counts, not a CPU or
frame-rate improvement. No statistical significance or hardware speedup follows
from this small measurement alone. Memfds close automatically after each case;
there are no persistent benchmark artifacts unless the caller saves stdout.

## Current JWM compositor compatibility

The viewer requires truthful presentation feedback, not just a compositor that
advertises the protocol. JWM's nested backends do not advertise `wp_presentation`
until they can forward real host presentation evidence. KMS binds each
acknowledged commit to one completed submission on the output that presented
it. On a multi-output desktop, the native window must fit fully within one
presenting output and must not overlap another. This path requires all
presenting outputs to use nonnegative origins and the normal transform.
Outputs may use different scales. JWM keeps the window, output crop and
KMS plane geometry in physical pixels. The viewer's committed integer
buffer scale determines the source texture dimensions.

The effects compositor verifies an interior of the actual window draw:

- The presenting output has a positive mode size and its entire crop fits
  within the desktop FBO. The verified interior is translated into output-local
  pixels for the cursor and visibility checks.
- The scene contains one opaque native root surface, fully inside the FBO.
  The sampled quad must match the committed logical extent, and the full
  texture must match that extent times the committed integer buffer scale.
  The buffer must use the normal transform. Rounded corners and the border
  trim the verified interior.
  Background shadows can remain below the opaque window. Subsurfaces, viewport
  cropping, transformed draws and unproven overlays prevent the receipt.
- Outside the FBO, only the actual cursor assembly prefix may precede the
  FBO element. The final cursor bounds may cover part of the verified interior.
  A receipt can reach KMS only while some of that interior remains visible.
  Unknown elements and invalid cursor geometry invalidate the proof. These
  proof frames composite the cursor into the primary output without hardware
  overlay or cursor planes.
- When an opening or monitor move animation prevents an exact draw, the
  compositor can retain the callback for up to two seconds. The original
  surface, buffer identity, commit and presentation generation must still
  match on retry. A newer commit retires unsubmitted feedback for its
  predecessor even when it requests no feedback of its own.
- Successful import is followed by an actual full redraw of that exact input
  texture and rectangle. The proof owns the candidate commit's callbacks;
  cached FBO content or an allocation identity alone cannot qualify.
- An independent FBO content serial, allocation identity, live surface
  generation and output crop must still match. The final render-element
  states must confirm that the FBO element contributed visible pixels to
  that output before the proof is consumed.

With effects off, the multi-output path checks the visible area of the actual
native surface render element at its physical destination size. In the effects
path, surface output membership follows the intersection of the actual draw
with each output crop. Both paths can defer feedback during a move between
adjacent outputs for up to two seconds while the same commit is live. A window
that stays across a seam, a fully occluded render, or unsupported output geometry
can lead to discarded feedback. The native viewer ends the session when the
current frame is discarded. These presentation rules apply independently of
remote input and clipboard permissions.

KMS feedback belongs to the exact queued submission and is discarded on an
invalid output/VT generation, uncertain completion time, queue failure, or
unsubmitted batch drop. A monotonic DRM event timestamp is used for feedback
without advertising `hw_clock`. The timestamp can come from software in the
display driver. The managed viewer reports `wayland_presentation_discarded`
when the compositor discards its current submission.
It reports `wayland_presentation_timeout` when a current or retired
submission misses the viewer's completion deadline.

DPMS power replies follow the DRM write and refer to the original output
object. A fresh control reads the confirmed mode. Powering down retires any
queued submission that can no longer complete. A new eligible commit can wait
while the output is dark and receive feedback after a verified flip on wake.
When a canceled flip could race recovery, KMS completes a synchronous clear
before using new feedback. Connector changes are reprobed on the active DRM
device and retire the old presentation generation and its renderer receipts.

The KMS acceptance script matches viewer feedback to a logged compositor
submission and checks two colors using the selected output's screencopy.
It launches a private Xvfb source and connects to a disposable JWM output.
Start the test compositor with `--backend wayland-udev` and
`--log 'info,jwm::kms::presentation=debug'`, then pass its debug log:

```sh
python3 scripts/test-remote-wayland-kms.py ./target/debug/jwm-remote \
  --wayland-display /path/to/test/wayland-1 --output Virtual-1 \
  --kms-log /path/to/test/jwm.log
```

Use `--require-path composited` to check that an effects-composited frame
owned the matching DRM submission. Use `--require-path cursor-overlap` when
the cursor is positioned inside the verified interior on the test output.
Use `--require-path direct` with effects disabled. With two active outputs,
add `--move-to-output Virtual-2 --ipc-socket /path/to/test/jwm-ipc.sock` to
move and refocus the same viewer between the red and blue frames. The test
checks the matching completion and pixel samples on both outputs. For an owned
desktop already configured at 1× and 2×, `--check-viewport --expect-output-scales 1 2`
checks four source quadrants, both configured output scales and the integer
buffer scale of each sampled presentation commit.

An owned VirGL guest can also read back the current DRM primary framebuffer.
Build the sampler against the guest's libdrm headers and library.

```sh
cc -O2 scripts/test-virtio-drm-primary-rgb.c $(pkg-config --cflags --libs libdrm) \
  -o /path/to/test/virtio-primary-rgb
```

Run the harness with `--check-viewport --virtio-primary-rgb /path/to/test/virtio-primary-rgb`
and `--virtio-drm-card /dev/dri/card0` inside that guest. The sampler needs
access to the active framebuffer's GEM handle. It verifies a full-size linear
RGB primary plane and synchronizes its VirGL readback before sampling. The
harness checks both the active output pixels and the peer output's samples.
Physical scanout remains a separate acceptance test.
[Guest KMS evidence](benchmarks/remote-upgrade-2026-11-20/README.md#wayland-viewer-kms-completion-in-a-guest)

For a DPMS cycle on an owned output, add `--wlopm /path/to/wlopm` and
`--drm-dpms-status /sys/class/drm/card0-Virtual-1/dpms`. The harness confirms
the kernel and new Wayland controls agree, submits the blue pattern while
the output is off, and checks that the same commit completes after wake.

For connector hotplug on an owned two-output guest, start the viewer on the
connector being tested and add `--move-to-output Virtual-1` with its IPC
socket and `--hotplug-status /sys/class/drm/card0-Virtual-2/status`. The
host fixture responds to `JWM_HOTPLUG_PHASE=disconnect` and
`JWM_HOTPLUG_PHASE=reconnect` on the guest's output. The guest checks kernel
and compositor topology, whether the same viewer moved, and the new DRM
completion and pixels after each transition. On QEMU's second virtio console,
the host can set the requested dimensions to 0×0 to disconnect and restore
the original dimensions to reconnect. [QEMU virtio display state](https://github.com/qemu/qemu/blob/v8.2.2/hw/display/virtio-gpu-base.c#L83-L106)

For full device loss, run the harness against a dedicated compositor with
`--device-loss-card /sys/class/drm/card0` and the selected connector as
`--output`. The external fixture should remove the owned device after it
sees `JWM_KMS_DEVICE_LOSS_READY card0`. The harness observes sysfs, waits up
to 15 seconds for the card and connector to disappear, and then submits a
second color. It checks for a terminal presentation timeout or discard
without a reconnect certificate. The second color must request
presentation while the original presentation and drawn-ACK counts stay
fixed.

### Horizontal wheel opt-in

Use `--wayland-horizontal-scroll` together with `--wayland-pointer` or
`--wayland-control`; the source must separately enable
`--allow-wayland-horizontal-scroll`. Existing `--wayland-scroll` remains
vertical-only. Both network peers require v23, and the source compositor needs
native pointer/control protocol version 3. See the [source-side permission and
single-axis contract](remote-wayland-source.md#independently-authorized-horizontal-wheel-v23).
This does not enable smooth scrolling, mixed-axis gestures, modifier-wheel,
dragging or input replay while waiting for fresh pixels.


## Incremental upload evidence

The Wayland safety job explicitly runs all seven bounded damage-helper tests,
all seven full-upload/baseline tests, and the owned compositor regression
`actual_wayland_viewer_accumulates_skipped_tile_damage_across_held_slots`.
The latter skips a decoded tile snapshot, alternates two held SHM slots,
checks both complete SHM canvases and actual software-GL pixels, and verifies
that release/frame callbacks still cannot acknowledge a new submission.

A separate ignored test is explicitly enabled by that job:

```sh
cargo test --locked --no-default-features \
  --features remote-wayland,backend-wayland-udev --lib \
  remote::wayland_viewer::damage_benchmark::native_memfd_sparse_damage_matches_full_canvas_and_counts_writes \
  -- --ignored --exact --test-threads=1 --nocapture
```

It compares full conversion with 64×64-source-region patches using real owned
memfds at native 2560×1440, scaled 1280×720→1920×1080, and letterboxed
800×600→1920×1080. Six samples per case alternate measurement order. Every
patch must match the complete full-upload bytes, write exactly the mapped
region's bytes, and write fewer than one sixteenth of the full canvas. The
unchanged-image case must perform zero seeks and writes. The JSON line records
conversion/write time and actual file-I/O counts without imposing a noisy
wall-clock performance threshold.

These are reproducible CPU/SHM checks, not RTX 5080, zero-copy, physical-display,
or two-machine latency acceptance. Test definitions alone are not a successful
run; use the CI result for the exact candidate/master commit.
