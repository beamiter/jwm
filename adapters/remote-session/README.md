# JWM terminal adapters

`client.rs` contains the shared local controller for `jwm-remote session`.
It owns the helper process, bounds its JSON Lines IO, validates that source
selections are current, and presents the same connection state and metrics to
each toolkit. GTK terminals also share `gtk.rs`.

The adapters offer view-only connections and explicit input requests. Each
authorized Wayland window has a one-shot lease. After release, the viewer can
continue showing that window in read-only mode. A helper that advertises
`wayland_source_control` permits switching between locally authorized windows.
A switch releases the current lease first. Once the new window has presented,
**Request Input** sends the current session, source epoch and window ID if
`wayland_input` is `available`. Both fields are optional for older helpers.

The **Modifier clicks and dragging** option is available when the native
Wayland viewer and control are selected. It passes `--wayland-gestures` to the
helper and requires the host's independent opt-in. The optional
`wayland_gestures.ready_frame` snapshot field drives the ready indicator.

X11 clipboard starts off. The connection settings let the viewer opt in
and select content and direction. The host must authorize sharing separately.
The helper reports the negotiated policy and progress for active image or file
transfers. The byte counts include flow-control processing for an upload.
The optional clipboard field also lets current adapters keep viewing with
older helpers when clipboard is off.

For X11 view-only sessions with clipboard off, **Reconnect automatically**
passes `--reconnect-attempts 3` to the helper. The panel shows the waiting
state and clears the source choices until the reauthenticated session presents
a frame. Aether uses the same recovery option for its XEmbed viewer. Upgrade
the installed `jwm-remote` before enabling this option.

Forge, Anvil, Ember and Frost are separate repositories. Each builds with a
vendored copy under `vendor/jwm-remote/` and uses its existing `serde` and
`serde_json` dependencies. Aether vendors the controller under
`src-tauri/vendor/jwm-remote/`. Update the canonical files here, then copy
`client.rs` to the five repositories and `gtk.rs` to Forge and Anvil.
The installed `jwm-remote` binary runs the engine and presenter.
