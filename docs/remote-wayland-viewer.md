# Native Wayland read-only viewer (experimental)

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
It does not probe `DISPLAY`, bind `wl_seat`, inject input, or acquire clipboard
ownership. The initial local size defaults to 800x600, subject to the compositor's
configure events. Authentication, decoding, frame limits and transport shutdown
remain in the existing client loop.

## Deliberately narrow permissions

This first backend requires `--view-only` and rejects keyboard/pointer leases,
legacy input, clipboard, source resize, input grabs, X11 display selection,
window positioning, automatic reconnect and saving this selection in profiles.
The SSH wrapper and saved-profile path do not implicitly enable it. Source
switching is unsupported and fails closed rather than remapping an old surface
without a new configure handshake.

## Presentation is not buffer release

The compositor must advertise `wp_presentation`; absence is an error. Only a
matching `presented` feedback for the submitted surface commit can produce a
display acknowledgement. `wl_buffer.release` only permits storage reclamation;
ordinary frame callbacks are not evidence that pixels were displayed.

There is at most one unresolved presentation feedback. A discarded, obsolete,
invalid or timed-out submission ends the session without a display ACK. This
avoids reusing X11's superseded-frame recovery semantics for unpresented native
commits. Configure/source changes that invalidate an outstanding presentation
also fail closed; uninterrupted resize is not promised in this first slice.

SHM storage uses a bounded pool of at most two slots. Once the compositor
releases a buffer, its storage can be reused for a later frame of the same size
instead of creating a new memfd and protocol buffer for every frame. Each reuse
still rewrites the entire image, including letterboxing: this is allocation
reuse, not incremental upload or zero-copy presentation.

Idle and busy slots both count toward the 128 MiB aggregate ceiling, including
buffers still held from an earlier size. Each buffer also has checked width,
height, stride and pixel limits. A size change may replace only idle storage.
Replacement first destroys the released idle slot but keeps its old bytes
charged until a real `wl_display.sync` callback confirms the server processed
the preceding destroy request; only then may replacement storage be allocated.
A five-second fence timeout or configure change terminates the session. This is
protocol-resource accounting and destroy acknowledgement, not a guarantee that
an arbitrary compositor immediately frees private caches or physical pages.
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
client path with X11 display environment variables removed.

Those tests do **not** establish physical display timing, production KMS/DRM
pageflip correctness, a real two-machine session, hardware encoding or NVIDIA
performance. The local compositor is trusted to tell the truth about
presentation. JWM's existing production presentation batching is a separate
integration concern: a vblank must not acknowledge a newer surface commit that
was not part of the completed output submission. Protocol-level viewer tests do
not replace that audit or a real hardware acceptance run.

## Current JWM compositor compatibility

The viewer requires truthful presentation feedback, not just a compositor that
advertises the protocol. JWM's nested backends do not advertise `wp_presentation`
until they can forward real host presentation evidence. KMS presentation
requires exactly one configured output object (disabled outputs also count)
and proof that the acknowledged content belongs to the completed submission.
Individually proven surface render elements remain a separate path.

The candidate FBO integration admits only a deliberately narrow case:

- The single output has origin `(0, 0)`, a positive mode size exactly equal to
  the entire FBO, scale `1`, and `Normal` output transform.
- The scene contains one opaque native root surface, fully inside the FBO,
  without subsurfaces, viewport cropping/scaling, effects, overlays inside the
  FBO or occlusion. Surface buffer scale/transform and window geometry must also
  satisfy the implementation's exact, untransformed layout checks.
- Outside the FBO, only the actual external-cursor assembly prefix may precede
  the one FBO element. Every cursor element's final bounding box must be
  disjoint from the source rectangle, including bitmap hotspot offsets and
  every fallback body/shadow rectangle. Pointer coordinates or a `Cursor`
  label alone are not proof. Any overlap, unknown/extra element or invalid
  geometry fails closed. These candidate proof frames require software cursor
  composition into the primary output, with cursor-plane and overlay-plane
  scanout disabled: element bounds do not establish that unused hardware
  cursor-plane padding is clear. This trades away hardware-plane offloading
  for the narrow proof; it is not hardware cursor/plane validation. Ordinary
  frames retain their existing plane flags. A cursor already drawn into the
  FBO remains outside this supported case.
- Successful import is followed by an actual full redraw of that exact input
  texture and rectangle. The proof owns the candidate commit's callbacks;
  cached FBO content or an allocation identity alone cannot qualify.
- An independent FBO content serial, allocation identity and live surface
  generation must still match. Final output render-element states must confirm
  the FBO element (after any proven disjoint cursor prefix) was not skipped
  and is visible before the proof is
  consumed for one output submission.

Anything outside the provable paths is discarded, including multi-output
frames. These are candidate capability conditions, not a claim that the new
FBO path has passed validation. Complex and common/default composited desktops
may still be unsupported: the native viewer stops rather than manufacturing a
display ACK. This is not general FBO presentation support or hardware DRM
acceptance. The existing X11 viewer remains the default path while production
rendering integrations are validated. The native viewer remains read-only;
this presentation work adds no input or clipboard permissions.

KMS feedback belongs to the exact queued submission and is discarded on an
invalid output/VT generation, uncertain completion time, queue failure, or
unsubmitted batch drop. Pure batch tests and controlled protocol tests do not
constitute a real DRM pageflip acceptance run.
