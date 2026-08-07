# Minimized-window Dock

JWM and xbar share one minimized-window model instead of treating minimization
as an off-screen layout trick. The result follows the macOS interaction model:
the window folds into a bar shelf, remains represented by a compact thumbnail,
expands around the pointer, shows a compositor-retained preview on hover, and folds
back out when restored.

## Ownership

- JWM owns window identity, minimized state, focus/tag restoration, and the
  authoritative per-monitor list.
- `shared_structures` carries bounded metadata and typed restore/preview/geometry
  commands. It never carries raw pixels, GL objects, or dma-buf handles.
- `xbar_core` owns the shelf layout, hit regions, pointer magnification, and the
  conversion from a bar-local logical rectangle to global physical pixels.
- The X11 and Wayland compositors own window textures, forward/reverse Genie
  meshes, and the floating hover preview. This lets the preview extend beyond
  the bar's 38-pixel input/strut window without making the bar cover clients.

No Apple artwork or private API is used. The behavior is implemented with
JWM's own window textures and renderer-neutral bar primitives.

## State and command flow

1. A minimize request marks the client hidden and publishes it in that
   monitor's next bar snapshot. The compositor detaches the still-visible
   texture before layout moves the real window off-screen.
2. The bar lays out a translucent shelf and reports both its fallback region
   and every realized item rectangle. A new item may start at the shelf centre;
   the in-flight animation adopts the exact rectangle when the next bar frame arrives.
3. Hover sends one enter transition, not one command per motion event. The
   compositor animates an aspect-preserving preview below the item (or above it
   at the screen edge). Leave fades it out; stale previews also expire inside
   the compositor.
4. A click sends the session-scoped window token and its current item anchor.
   JWM uses `reveal_and_focus`, which selects the correct monitor and tag before
   starting the reverse Genie. It never restores by title matching.

All rectangle commands use global physical pixels. The bar derives them from
the monitor origin in the JWM snapshot plus its local logical layout and output
scale. Negative monitor origins and fractional scale factors are valid.

## Protocol and restart behavior

The Dock metadata is part of shared-memory protocol v12. Each message includes
a window-manager session id, minimized-list generation, explicit overflow bit,
and at most 16 fixed-size window records. Commands carry a 64-bit window id,
the same session id, source monitor, and an anchor rectangle.

When more than 16 windows are minimized, the newest 16 remain addressable in
their original insertion order and the overflow marker is shown. Targets for
older entries are withdrawn immediately, so a clipped thumbnail cannot remain
painted after its slot leaves the bounded snapshot.

An old mapping is rejected by layout/version validation; JWM and a bar must be
restarted together after upgrading. Delayed commands from an earlier JWM
session or a different monitor queue are ignored, so a recycled backend id
cannot restore the wrong window.

Snapshots use overwrite-oldest delivery because they are complete state, while
commands remain bounded and non-overwriting. A slow bar therefore converges to
the newest minimized list without turning hover traffic into an unbounded
queue.

## Degraded paths

- With no current item geometry, Genie targets the reported shelf centre.
- With no compositor texture, the bar item still restores the window and the
  preview is simply omitted.
- Closing a minimized window removes its cached visual; disabling the effect or
  tearing down the compositor releases every retained texture exactly once.
- Cached visuals are bounded by both 32 entries and an estimated 128 MiB RGBA
  budget, with least-recently-cached eviction while retaining the newest item.
- Wayland retains the surface colour transform with the texture, so Genie,
  shelf thumbnails and hover previews keep the same P3/PQ/HLG rendering as the
  live window.
- Direct scanout is suspended while a visible shelf thumbnail, Genie, or hover
  preview needs composition. Hiding/losing the bar withdraws its targets and
  makes direct scanout eligible again.
