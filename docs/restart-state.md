# X11 restart state

`Alt+Shift+R` (`restart`) on both `xcb` and `x11rb` replaces JWM while the X
server keeps application windows alive. JWM hands the replacement process:

- Each ordinary live window's complete tag bitmask, output identity, and
  per-window tiling weight (`client_fact`).
- Each output's current and previous tag views.
- Every tag's layout and previous layout, master count and fraction, gap, and
  bar preference, including the all-tags layout slot.
- The managed client order (tiled group before floating group) and current
  selected window where it remains eligible for focus.
- Optional committed physical resize intentions and scrolling column groups,
  width factors, per-column focus, and new-window attachment mode.

The one-restart snapshot takes precedence over new-window placement rules and
closed-window memory. Ordinary minimized, sticky, and swallowed windows are
included; their existing specialized state/admission handling still applies.
Named scratchpads and shell bars/docks keep their own ownership paths.

Window weights are restored only after the same XID and generation checks as
tags and output placement, before arranging the adopted windows. Older snapshots
without weights use `1.0`. Non-finite weights or values outside `0.25..=4.0`
reject the entire snapshot; invalid live weights cancel capture before any
property writes.

Outputs are matched by unambiguous stable identity and connector, never by a
reused monitor index. If an output is missing, its clients move to the nearest
remaining output, retaining their valid tags. Its saved layout does not
replace that other output's layout. Hand-float restore rectangles follow a moved
or resized output, or the replacement for a missing output, and fit its current
work area including borders. Unchanged output geometry preserves the exact
saved rectangle, including deliberate partial off-screen placement.
Ambiguous duplicate identities do not
silently select the first panel. If the configured tag count shrinks, masks
are clipped; windows left with no valid tag use their destination's active
view. A view that becomes all tags uses the all-tags layout slot.

The snapshot is temporary X-server state, not a saved login session. The
replacement must have the explicit same-exec intent, the matching root
snapshot, and the same generation marker on each still-live window. This
prevents accidental reuse of stale window IDs; it is not authentication or an
isolation boundary between clients on the same X server. Normal starts do not
adopt abandoned restart snapshots, and applications are never relaunched.

All property writes are checked and read back before destructive cleanup.
An incomplete or oversized base snapshot cancels restart and leaves the current
JWM running, with an error toast and log. The snapshot limit remains 64 KiB.
Optional shape contexts are lower priority: if they do not fit, complete contexts
are omitted deterministically with a warning, preserving the base client, tag,
layout, and order handoff. A scrolling grouping and its resize anchor are one
atomic context; neither is partially truncated. Startup consumes the snapshot only
after setup succeeds; bounded startup retries and the existing exec-failure
fallback can reuse it. Cleanup errors are logged, and leftover properties
cannot enable restoration without the matching one-shot intent.

Existing `[[layout.tags]]` persistence remains the durable default for future
sessions. A restart snapshot also preserves current layout values when the
configuration file is read-only. No configuration migration is required.

## Committed layout shapes

A shape belongs to an exact output identity, tag mask, and canonical layout.
Resize targets are normalized fractions of today's usable output, with one
verified live anchor; fresh native layout frames and size hints constrain them.
Pixel rectangles and scrolling viewport offsets are not saved. Only the
pre-grab intention is captured during an unfinished pointer resize.

Scrolling preserves the order of surviving groups and members, each group's
width and remembered focus, and the existing new-window attachment policy.
Hidden, minimized, swallowed, floating, removed, or otherwise ineligible members
are pruned under the existing visible-membership contract. There is no dormant
hidden-column or hidden-anchor promise. New windows use ordinary insertion
policy. The saved monitor selection retains normal precedence over column focus.

Shape references must pass the existing generation check and retain the exact
adopted runtime client identity. Missing or ambiguous outputs never transfer a
shape to a fallback output. If configured tags change, an optional context whose
exact mask is no longer legal is dropped rather than clipped into another scope.
The base window placement fallback is unchanged.

For a verified shape handoff, initial adoption defers layout arrangement until
the root-child scan completes and order, groups, and intentions are restored.
Both ordinary arrange and focus-triggered direct layout arrangement honor the
barrier. A scan/manage error releases it and retains the handoff for the existing
retry path; successful adoption then arranges from the complete restored state.

Old snapshots without optional shapes still load. Older strict binaries reject
new snapshots containing added fields and use ordinary window adoption on a
downgrade; this transient handoff does not promise reverse-version compatibility.

## Scope

This change does not alter Wayland restart behavior or promise to preserve
Wayland/XWayland client connections across compositor replacement. It does not
persist full per-tag focus history or a complete MRU stack. The manual
`save_session`/`restore_session` feature remains separate.

Pure state/codec and mock-backend tests cover identity, hotplug matching,
write/readback failure ordering, and off-view adoption without creating a
native connection. Actual Xorg multi-monitor restarts, compositor-on/off
behavior, and live hotplug still require hardware/session validation.

## External X11 panels and work areas

During startup and restart, mapped override-redirect panels keep their existing
unmanaged status. A nonzero strut is rediscovered without waiting for the panel
to rewrite its property. JWM observes later property changes without replacing
its other event subscriptions. Unmapping or destroying the panel removes its
reservation; remapping rereads it. Hidden panels cannot reclaim space through
a property update. Failed attribute, observation or host-geometry queries do
not create a reservation. Inverted spans on nonzero edges are rejected for
these unmanaged observations; legacy zero/zero spans retain host attribution.

A mapped override-redirect window explicitly typed as `DOCK` is also observed
before its first nonzero strut, allowing a panel to publish the property after
mapping. Untyped windows with no strut at scan/map time are not subscribed: if
such a window declares both its dock type and first strut only later, a remap is
needed for this discovery path. Ordinary menus and compositor overlays do not
become managed clients or receive focus through this handling. Wayland's
layer-shell exclusive zones retain their existing behavior.
