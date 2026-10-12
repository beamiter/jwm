# X11 restart state

`Alt+Shift+R` (`restart`) on both `xcb` and `x11rb` replaces JWM while the X
server keeps application windows alive. JWM hands the replacement process:

- Each ordinary live window's complete tag bitmask and output identity.
- Each output's current and previous tag views.
- Every tag's layout and previous layout, master count and fraction, gap, and
  bar preference, including the all-tags layout slot.
- The managed client order (tiled group before floating group) and current
  selected window where it remains eligible for focus.

The one-restart snapshot takes precedence over new-window placement rules and
closed-window memory. Ordinary minimized, sticky, and swallowed windows are
included; their existing specialized state/admission handling still applies.
Named scratchpads and shell bars/docks keep their own ownership paths.

Outputs are matched by unambiguous stable identity and connector, never by a
reused monitor index. If an output is missing, its clients move to the nearest
remaining output, retaining their valid tags. Its saved layout does not
replace that other output's layout. Ambiguous duplicate identities do not
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
An incomplete or oversized snapshot cancels restart and leaves the current
JWM running, with an error toast and log. The snapshot limit is 64 KiB;
partial state is never silently truncated. Startup consumes the snapshot only
after setup succeeds; bounded startup retries and the existing exec-failure
fallback can reuse it. Cleanup errors are logged, and leftover properties
cannot enable restoration without the matching one-shot intent.

Existing `[[layout.tags]]` persistence remains the durable default for future
sessions. A restart snapshot also preserves current layout values when the
configuration file is read-only. No configuration migration is required.

## Scope

This change does not alter Wayland restart behavior or promise to preserve
Wayland/XWayland client connections across compositor replacement. It does not
persist layout-local master resize intentions, scrolling column structure,
full per-tag focus history, or a complete MRU stack. The manual
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
