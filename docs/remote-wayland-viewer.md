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

SHM storage has at most two retained slots and a 128 MiB aggregate ceiling,
including buffers still held from an earlier size. Each buffer also has checked
width, height, stride and pixel limits. A compositor may retain the current
buffer until a replacement arrives: a presented-but-unreleased slot is kept
intact while the second slot carries the replacement. Busy storage is never
rewritten. Missing presentation feedback or two-slot submission blockage has a
five-second deadline. A single presented current buffer may legitimately remain
held indefinitely during a static image; it consumes only its bounded slot.
These are native-upload bounds, not a claim about total process memory including
the decoder and transport.

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
until they can forward real host presentation evidence. In the initial KMS
batching implementation, only exactly one configured output object (disabled
outputs also count), with individually proven surface render elements, can
acknowledge a frame. FBO-composited content and
multi-output frames are conservatively discarded instead of being reported as
presented. This includes common/default composited configurations: the native
viewer will stop rather than manufacture a display ACK. The existing X11 viewer
remains the default usable path while these production rendering paths are
integrated and validated.

KMS feedback belongs to the exact queued submission and is discarded on an
invalid output/VT generation, uncertain completion time, queue failure, or
unsubmitted batch drop. Pure batch tests and controlled protocol tests do not
constitute a real DRM pageflip acceptance run.
