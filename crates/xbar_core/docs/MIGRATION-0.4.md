# Adopting the 0.4 frontend core

Version 0.4 keeps the 0.3 model, runtime, provider, transport, scene, and Cairo
surfaces. It adds the frontend-facing layer found duplicated across all 24 JWM
bar repositories. The one intentional behavior correction is the default JWM
layout catalog: protocol ID `1` is now `><>` and ID `2` is `[M]`.

## Coherent runtime frames

Toolkit and web frontends can stop cloning a snapshot separately from the
operation that produced it:

```rust
use xbar_core::{BarRuntime, RuntimeSchedule};

let mut runtime = BarRuntime::default();
let mut schedule = RuntimeSchedule::default();
let frame = schedule.service_frame(&mut runtime);

if frame.needs_redraw() {
    // `frame.snapshot` and `frame.update.changes` describe one revision.
}
for issue in &frame.update.issues {
    eprintln!("{issue}");
}
```

`BarRuntime::current_frame` supplies initial state or an explicit replay;
`dispatch_frame` does the same for one `UserAction`. Every captured frame gets
a monotonic revision, and accumulated model dirtiness is consumed exactly
once. Platform effects and issues are not silently replayed.

## One frontend wire contract

The six Tauri backends can replace their four DTO mappers and `EmittedState`
cache with a complete envelope and cursor:

```rust
use xbar_core::{FrontendEnvelope, SnapshotCursor};

let mut cursor = SnapshotCursor::new();
if let Some(envelope) = cursor.update_frame(&frame) {
    // Emit `envelope` as one event, or use
    // `envelope.effective_partition_changes()` for independent stores.
    let _: FrontendEnvelope = envelope;
}
```

The payload retains `geometry: None`, `BatteryState::absent()`, and the exact
`layout_symbol`. It never substitutes zero geometry, a full battery, or a
string containing host scale/debug data. `SnapshotCursor::replay` returns a
complete initial state for a newly ready webview and rejects stale revisions.

Replace multiple Tauri command functions with one internally tagged
`ActionRequest`. `ActionRequest::dispatch_frame` checks wire tag indices,
converts to `UserAction`, dispatches, and captures the result. The model still
performs the authoritative configured-tag check.

Hosts that always need the runtime, schedule, and delivery cursor together can
use `FrontendSession`. Both `service` and `dispatch` return `SessionOutput`:
the frame retains issues and platform effects, while its optional envelope is
already ordered and deduplicated. `next_service_deadline` lets an event loop
sleep until the earlier of its provider tick and managed transport retry.

The workspace `xbar_tauri` companion applies this composition to all six Tauri
consumers. A backend keeps its generated context and plugins but replaces its
local DTOs, worker, retry cache, commands, and window-effect switch with:

```rust,ignore
let shared_path = std::env::args().nth(1).unwrap_or_default();
let builder = tauri::Builder::default().plugin(tauri_plugin_opener::init());
let builder = xbar_tauri::configure(
    builder,
    xbar_tauri::BridgeConfig::new(shared_path),
)?;
builder.run(tauri::generate_context!())?;
```

After registering its one `xbar-state` listener, the web frontend invokes
`frontend_ready`. All user intent goes through `dispatch_action` with a
`request` containing the internally tagged `ActionRequest`; for example
`{"action":"adjust_volume","delta":5}`. Revision ordering lets the frontend
discard a delayed older event without maintaining four backend DTO caches.

## Shared display and control semantics

Use the `display` module instead of local threshold/icon/format helpers:

- `usage_tone`, `battery_tone`, and configurable validated thresholds;
- `volume_level` / `volume_level_for_device` with distinct unavailable and
  muted states;
- `format_bytes` using base-1024 IEC units;
- `IconSet::nerd_font` with safe dynamic tag/monitor fallback;
- `CANONICAL_LAYOUTS` and its lookup/catalog helpers.

Unknown CPU, memory, battery, brightness, or audio values stay unavailable;
they are never displayed as healthy zero usage or 100% battery.

`PresentationProjector` turns `BarView + PresentationConfig` into a
geometry-free `BarPresentation`. Toolkit widgets consume `ControlSpec`
values (separate icon/value, raw tag state, metric tone/progress, availability,
and all four input bindings). `LayoutEngine` consumes the same projection, so
tag precedence, layout commands, labels, and actions cannot drift between
scene and toolkit bars.

## Native reconnect, pointer, and placement adapters

Epoll-style loops synchronize a `TransportNotifierSlot` after servicing the
runtime. `NotifierChange::Replaced` safely borrows the descriptor to register;
disconnect removes the owned notifier, and a generation change constructs the
replacement atomically.

Winit/tao loops can replace their tick/notifier forwarding copies with
`AlignedWakeThread`, `CoalescedNotifierForwarder`, and `WakeAck`. The forwarder
handles EINTR/terminal poll states, eventfd drain, coalescing, bounded shutdown,
and worker join without depending on either framework. `TransportWakeSlot`
also owns the generation check and reconnect replacement.

Native pointer handling can use `CairoBar::handle_pointer(PointerInput)`.
Activation occurs only when press/release use the same semantic button and
stable node; wheel direction, hover, hit testing, and action dispatch are
core-owned. Existing immediate `pointer_action` calls remain available.

Use `BarPlacement::top` for logical-height/scale conversion and
`BarPlacement::ewmh_strut` for `_NET_WM_STRUT` and
`_NET_WM_STRUT_PARTIAL` values. Window calls remain frontend-owned.

## Platform effects

`RuntimeUpdate::handle_platform_effects` drains work through one
`PlatformEffectHandler` (a closure implements it automatically) and returns a
`PlatformEffectReport`. Each failure retains its original `BarEffect`, letting
the host retry or log without duplicating the traversal policy in every bar.

Screenshots, process launch policy, native window calls, Tauri event names,
widget construction, colors, and GPU/surface lifetime remain outside the
portable core.
