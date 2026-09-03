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
   texture before layout parks the real window off-screen. On X11, an eligible
   client is then truly changed to ICCCM Iconic only after a bounded CPU
   snapshot has been admitted; a capacity or capture failure deliberately
   leaves the mapped, off-screen fallback intact and retryable.
2. The bar lays out a translucent shelf and reports both its fallback region
   and every realized item rectangle. A new item may start at the shelf centre;
   the in-flight animation adopts the exact rectangle when the next bar frame arrives.
3. Hover sends one enter transition rather than one command per motion event.
   While magnification moves the same card, `xbar_core` refreshes its anchor at
   a bounded 50 ms cadence; the 2 s lease remains the idle heartbeat. The
   compositor animates an aspect-preserving preview below the item (or above it
   at the screen edge). Leave fades it out; stale previews also expire inside
   the compositor. Preview ownership includes the projection generation, so
   the first command from a rebuilt scene retires an older overlay immediately.
4. A click sends the session-scoped window token, the minimized projection
   generation that produced the item, and its current anchor.
   JWM uses `reveal_and_focus`, which selects the correct monitor and tag before
   starting the reverse Genie. It never restores by title matching.

All rectangle commands use global physical pixels. The bar derives them from
the monitor origin in the JWM snapshot plus its local logical layout and output
scale. Negative monitor origins and fractional scale factors are valid.

## Protocol and restart behavior

The Dock metadata is part of shared-memory protocol v14. Each message includes
a window-manager session id, minimized-list generation, explicit overflow bit,
and at most 16 fixed-size window records. Commands carry a 64-bit window id,
the same session id, source monitor, exact generation, and an anchor rectangle.

The generation is a per-monitor minimized-projection epoch. Membership, Dock
slot order, and restore-then-re-minimize allocate a new epoch (even when the
backend reuses the same window id); title, focus, tag, and other non-Dock bar
updates retain it. `xbar_core` exposes this epoch as `wm_sequence`, captures it
with the rendered scene, and echoes it in geometry, preview, and restore
commands. JWM accepts a Dock command only when session, source monitor, and
generation all exactly match the monitor's current projection. Removing an
output invalidates its epoch before that monitor number can be reused.

When more than 16 windows are minimized, the newest 16 remain addressable in
their original insertion order and the overflow marker is shown. Targets for
older entries are withdrawn immediately, so a clipped thumbnail cannot remain
painted after its slot leaves the bounded snapshot. Native reporters retain an
unacknowledged withdrawal across a same-session transport replacement; JWM
also clears the non-addressable prefix once on the first valid command of each
new projection epoch, covering stateless Web render trees without turning
preview renewals into repeated compositor teardown.

An old mapping is rejected by layout/version validation; JWM and a bar must be
restarted together after upgrading. Delayed commands from an earlier JWM
session, another monitor queue, or an old projection generation are ignored,
so a recycled backend id cannot restore the wrong window or overwrite a new
Dock item's geometry/preview state.

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
  Re-enabling or recreating a compositor replays Dock targets before statically
  adopting minimized windows, so an off-screen parking coordinate never becomes
  a new Genie origin.
- Full-resolution retained visuals are bounded by both 32 entries and an
  estimated 128 MiB RGBA budget. Geometry, preview and restore operations
  refresh true LRU recency; rendering alone does not. An evicted addressable
  item is recaptured statically when its Dock geometry returns or it is
  hovered, while the newest item is always retained. A hover whose full-size
  pixels are still being imported keeps its intent but pauses both the show
  animation and lease, so it fades in normally instead of popping in or
  expiring before the texture arrives.
- The X11 and Wayland udev compositors also capture a separate low-resolution
  snapshot at minimize time. Each snapshot is at most 256 by 192 RGBA pixels;
  the top-left CPU cache is bounded to 128 entries/24 MiB and the independently
  owned GPU residency cache to 64 entries/12 MiB. Static Dock cards prefer the
  GPU snapshot, hover prefers a retained or live full-size source but can
  display the low tier immediately, and reverse Genie restore categorically
  rejects both low-resolution tiers. Full-resolution LRU eviction therefore
  cannot blank an addressable card, and a lost GPU residency can be rebuilt
  from the CPU copy without remapping the client. A failed lazy upload consumes
  one explicit demand and is only rearmed by new geometry, hover, or capture;
  CPU-only data is not treated as drawable and cannot indefinitely block direct
  presentation. On X11, retained-texture readback is additionally deferred
  until `render_frame` has successfully made the GL context current. One
  explicit demand/capacity epoch permits one attempt; releasing a pinned cache
  entry advances the capacity epoch and wakes exactly one retry, while ordinary
  frames cannot spin on an exhausted cache. A normal snapshot remains a
  recapturable cache entry. X11 may
  atomically promote the current generation to `IconicPinned`; only that exact
  bounded CPU owner authorizes a checked ICCCM unmap, and a pinned generation
  cannot be replaced or evicted until the physical Iconic lifecycle releases it.
- A Wayland hidden-surface import is keyed to commits from the root surface or
  any subsurface. A known commit with no usable buffer is attempted once; a
  new commit retries immediately, while transient renderer/fence failures use
  a bounded 2--64 frame backoff. Restore, withdrawal, destruction, and
  compositor recreation clear the gate, so unrelated animations cannot turn a
  missing hidden buffer into per-frame GPU work.
- A minimized client that becomes ineligible for the Dock (for example after
  gaining `SKIP_TASKBAR`) remains hidden in JWM but explicitly releases every
  compositor texture, animation, preview, target, and replay request. Small
  resource-free class/PiP/style metadata survives so eligibility can return
  without changing the window's presentation rules.
- Wayland retains the surface colour transform with the texture, so Genie,
  shelf thumbnails and hover previews keep the same P3/PQ/HLG rendering as the
  live window.
- Direct scanout is suspended while a visible shelf thumbnail, Genie, or hover
  preview needs composition. Hiding/losing the bar withdraws its targets and
  makes direct scanout eligible again.
- On X11, minimizing a fullscreen client that is being presented through
  manual Composite unredirect never detaches its stale pre-unredirect texture.
  JWM first restores Composite ownership and imports the replacement named
  pixmap, then converges to a static retained visual. A restore that arrives
  before that capture simply cancels the unseen minimize transition, keeping
  the direct-presentation owner valid instead of creating a reverse Genie from
  a freed GL resource.
- X11 true Iconic transitions are generation-fenced and transport-checked.
  JWM writes EWMH Hidden and ICCCM `WM_STATE=IconicState`, parks the input
  window, reserves the exact CPU snapshot generation, and only then issues
  `UnmapWindow`. The raw X11 event source correlates the request sequence and
  collapses the root/client `UnmapNotify` copies, so the manager-owned unmap is
  never mistaken for client withdrawal. If admission is unavailable, the
  standards-visible state remains Iconic while the real window stays safely
  mapped off-screen as the degraded path. The bounded managed-unmap tracker
  admits capacity before sending an X request and never evicts an unacknowledged
  sequence. A checked Map followed by a confirmed `IsViewable=false` also skips
  a redundant Unmap request, because that server no-op would have no event with
  which to retire its tracker entry.
- Client withdrawal cannot race an older manager-owned unmap into a newly
  managed incarnation of the same XID. Synthetic `UnmapNotify` converts the
  outstanding request into a bounded suppression tombstone until both
  root/client copies have been consumed; a later live generation wins even
  when the 16-bit wire sequence wraps. An `UnmapGravity` notification does
  not retire the pinned snapshot: JWM checked-maps the still-managed client,
  republishes Normal state when visible or re-parks/re-iconifies it when
  hidden, with failure compensation for geometry and both public properties.
- Restore performs the inverse physical transaction: it checks `MapWindow` and
  `IsViewable` before committing JWM's visible state, then verifies that the
  real geometry left the parking region. The pinned snapshot survives mapping
  and any import failure; it is released only after a live named-pixmap texture
  has been imported, so reverse Genie never consumes the sole durable source.
  Disabling the compositor first remaps every physical Iconic client
  transactionally and rolls the entire batch back on failure.
- On X11, the standard ICCCM/EWMH properties preserve the public minimized bit,
  while JWM's private, versioned `_JWM_MINIMIZED_RESTORE_V1` property preserves
  the semantic restore state that those standards do not carry: monitor/tag,
  visible and floating rectangles, fullscreen return rectangle, PiP/floating
  flags, and Dock insertion order. JWM writes it before committing a minimize,
  refreshes it immediately before a seamless exec, and removes it after a
  restore, live unmanage, or normal shutdown. Missing or malformed snapshots
  fall back safely and are normalized after adoption. Because the property is
  stored on a client-owned X11 window, an implausibly large recovered order is
  rebased locally instead of being allowed to exhaust the session allocator;
  the remaining monitor/tag/mode/geometry state is still recovered.
- A seamless exec intentionally leaves true Iconic clients unmapped. X11
  `QueryTree` still returns those root children, and the replacement JWM adopts
  either a viewable client or one whose `WM_STATE` is Iconic. For an unmapped
  client it first configures and reads back the current off-screen parking
  geometry, then maps, confirms `IsViewable` and reads back the geometry again
  before any compositor capture. It can therefore never expose an on-screen
  frame while rebuilding a fresh process-local generation and iconifying it
  again. Snapshot pixels and coordinator tokens never cross
  the process boundary; the client-owned V1 property carries only semantic
  restore state.
- Before that exec becomes irreversible, JWM performs a fail-closed restart
  preflight. It flushes pending layout state, validates the next config,
  captures stable scratchpad identities, verifies every persistent root child,
  parks hidden clients, and requires an exact V1 write/readback. ICCCM and EWMH
  minimized proofs are checked independently: an unmapped client with exact
  Iconic state remains unmapped, while one whose only reliable proof is EWMH
  Hidden is selectively mapped at its verified off-screen coordinate so the
  replacement scan can still find it. Any failure reverses those selective
  maps and resumes the existing event loop without entering cleanup.
- Normal shutdown uses a separate global handoff transaction. All true-Iconic
  and parked clients are mapped, restored to their saved visible geometry and
  synchronously verified before JWM releases any event mask, button grab,
  border, Hidden/WM_STATE property, V1 snapshot, compositor pin, or backend
  resource. If client N fails, clients N through 1 are compensated in reverse
  order and the same WM keeps running; only a completed one-shot handoff proof
  can enter destructive cleanup.
- PiP and fullscreen are mutually exclusive owners of their return geometry.
  Switching between them completes the old mode first; hidden clients remain
  parked throughout, and PiP restores both the pre-PiP floating/tiled choice
  and sticky state. Fullscreen clients reject ConfigureRequest geometry so a
  client cannot overwrite either a visible or minimized return slot.
- Output moves and hotplug migrate the semantic restore rectangle and the real
  parking coordinate together. A transient Configure/SetPosition failure is
  retained per `ClientKey` and retried from the main loop with a 50 ms--2 s
  capped exponential backoff, recomputing the current desktop-left edge and
  verifying server geometry each time. Restore or client removal cancels the
  pending retry, so topology churn cannot expose a mapped hidden window or
  leave a stale retry aimed at a reused slot.
- If a process dies after publishing `NormalState` but before moving a parked
  window back on screen, the next JWM recognizes the off-screen geometry plus
  valid snapshot as an interrupted restore and finishes it at the semantic
  target. Floating placement therefore survives seamless restarts exactly when
  the output still exists, and is clamped when topology changed. Exact tiled
  client order is preserved through the session snapshot: a v3 snapshot records
  each monitor's logical client sequence as an ordered list of class/instance
  identities, and `restore_session` replays it onto the monitor's client list
  before arranging — saved windows keep their saved relative order, while
  windows unknown to the snapshot append under the usual insertion rules.
- Named scratchpads also carry a bounded, versioned identity handoff across a
  seamless exec. Established X11 entries use the server-owned XID rather than
  the fresh backend's discovery-order `WindowId`, are resolved only against an
  XID the replacement backend already scanned, and are then checked against
  the managed-client table and, when available, PID. Still-launching entries
  are bound to the exact spawned PID,
  Linux process start time, and an expiring deadline. Unrelated MapRequests
  cannot consume a pending name, duplicate toggles cannot spawn a second copy,
  and malformed, stale, reused-PID, or ordinary-startup payloads are rejected.
  A launcher that forks into a different daemon PID is intentionally not
  guessed by title or class: its pending entry expires and can be retried.
