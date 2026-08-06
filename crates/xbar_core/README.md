# xbar_core

`xbar_core` 0.8 is the backend-neutral status-bar kernel shared by the XCB,
x11rb, winit, tao, wgpu, pixels, softbuffer, toolkit, and web bars in JWM.

The default build has no window-system, Cairo, ALSA, sysfs, logging, or shared
memory dependency. There is no compatibility umbrella and no `legacy-full`:
every frontend selects only the adapters it actually uses.

This repository is also a workspace for narrow companion adapters. Companion
crates depend on `xbar_core`, never the reverse, so framework and platform
dependencies cannot leak into the portable kernel. Current adapters:
`xbar_linux_actions` (process effects, checked command output, and the
standard `EffectRouter` host route), `xbar_present_wgpu` (wgpu surface
lifecycle and damage-aware CPU-frame presentation), `xbar_dbus_providers`
(UPower battery and MPRIS now-playing over D-Bus), and `xbar_tauri` (the
complete webview bridge). Releases are tagged; consumers pin `tag = "vX.Y.Z"`.

## Architecture

```text
native/provider input -> BarRuntime -> RuntimeFrame -> FrontendEnvelope
                              |              |               |
                              v              v               v
                           BarModel ----> BarView       toolkit / web
                                             |
                              PresentationProjector
                                    |             |
                              LayoutEngine    native widgets
                                    |
                              Scene + HitRegion -> Cairo / wgpu
```

- `BarModel` is the only semantic state owner. It reduces typed `BarEvent`
  values and emits change hints plus typed `BarEffect` values.
- `BarSnapshot` is an owned, serializable projection for toolkit, Tauri, and
  asynchronous consumers; it includes rich provider detail such as per-core
  CPU/memory counters and audio-device capabilities. `BarView` is the borrowed
  render fast path.
- `RuntimeFrame` captures a revision, accumulated changes, snapshot, issues,
  and platform work coherently. `FrontendEnvelope`, `SnapshotCursor`, and
  `ActionRequest` provide one host-neutral wire contract without a Tauri
  dependency or framework-specific DTO copies.
- `FrontendSession` combines a runtime, portable cadence, and delivery cursor
  for hosts that want one `service`/`dispatch` API. Its `SessionOutput` always
  retains the coherent frame for platform work while emitting an envelope only
  when frontend-observable state changed.
- `display` centralizes availability-aware metric tones, volume bands, byte
  formatting, Nerd Font symbols, monitor labels, and explicit JWM layout IDs.
  The geometry-free presentation projection gives widget toolkits the same
  control state and input bindings used by the scene layout.
- `BarRuntime` coordinates optional providers and transport without absorbing
  platform/window responsibilities. It can own transport opening, bounded
  reconnects, lifecycle status, and notifier generations.
- `RuntimeSchedule` gives high-frequency event loops one portable service
  call: transport is polled every turn while providers tick at a bounded
  monotonic cadence.
- `LayoutEngine` produces a renderer-neutral retained `Scene` and semantic hit
  map. `Scene::damage_from` invalidates both old and new component bounds.
- `CairoBar` is the high-level native facade combining runtime, layout,
  interaction, and Cairo rendering. Its `handle_pointer` API owns hover and
  matching press/release semantics.

## Pure model

```toml
[dependencies]
xbar_core = { git = "https://github.com/beamiter/xbar_core.git", default-features = false }
```

```rust
use xbar_core::{BarEvent, BarModel, TagId, UserAction};

let mut model = BarModel::default();
let update = model.update(BarEvent::User(UserAction::ViewTag(
    TagId::new(2).unwrap(),
)))?;

if update.needs_redraw() {
    let snapshot = model.snapshot();
    println!("active tag: {:?}", snapshot.active_tag);
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Native Cairo frontend

```toml
xbar_core = { git = "https://github.com/beamiter/xbar_core.git", default-features = false, features = [
  "clock-chrono", "logging-flexi", "provider-alsa", "provider-system",
  "provider-brightnessctl", "provider-battery-sysfs", "transport-shared",
  "runtime-linux", "render-cairo",
] }
```

For core-managed recovery, construct `TransportRecoveryConfig` and use
`BarRuntime::with_managed_transport`; its first poll opens the existing
WM-owned ring and later polls retry boundedly after failures. If low-latency
native notification is useful, create a `SharedEventNotifier` from the current
transport and let `TransportNotifierSlot` rebuild it whenever
`transport_generation()` changes. Then wrap the runtime with
`render::cairo::CairoBar`. Native pointer events map to `PointerInput` (or
through `PointerAction::from_x11_button`/`from_vertical_delta`);
`BarPlacement`, `EwmhStrut`, and the `DockWindowSpec` property protocol
centralize pure window geometry, while `linux::Epoll` owns the native wait
loop. Unhandled `RuntimeUpdate::platform_effects` remain the frontend's
responsibility and can be drained through one `PlatformEffectHandler` policy —
or through `xbar_linux_actions::EffectRouter` for the standard Linux route.

Toolkit and Tauri frontends use the same `BarRuntime` directly. A
`RuntimeSchedule` replaces their local `last_tick`, reconnect deadline, and
`tick + poll` merge logic; `service_frame` returns one coherent state handoff.
Event loops can use `next_service_deadline` to sleep until either the next
provider tick or an earlier managed-transport retry. Web bridges may instead
own a `FrontendSession`, which applies that schedule and snapshot
deduplication together without owning a thread or framework handle.
Toolkits consume the geometry-free control projection, while web bridges send
a complete `FrontendEnvelope` and dispatch a single `ActionRequest`. They do
not depend on `shared_structures` or instantiate provider managers themselves.
`SystemDetails` and `AudioDeviceInfo` preserve rich provider data without
leaking adapter types.
If the shared transport breaks, the runtime drops it, marks the WM projection
unavailable, returns geometry-clear work, and schedules its own reopen. A
reopened transport remains command-gated until its first authoritative WM
snapshot arrives; stale availability and monitor selection are never owned by
widget state.

```rust
use std::time::Duration;
use xbar_core::{BarRuntime, ModelConfig, RuntimeSchedule, SnapshotCursor,
    TransportRecoveryConfig};

let recovery = TransportRecoveryConfig::new("/run/user/1000/jwm", Duration::from_secs(2))?;
let mut runtime = BarRuntime::with_managed_transport(ModelConfig::default(), recovery)?;
let mut schedule = RuntimeSchedule::default();

// Call from a framework timer, idle callback, or native event-loop turn.
let frame = schedule.service_frame(&mut runtime);
let mut cursor = SnapshotCursor::new();
if let Some(envelope) = cursor.update_frame(&frame) {
    # let _ = envelope; // emit/store one complete, revisioned snapshot
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Features

| Feature | Capability |
|---|---|
| `clock-chrono` | Chrono clock adapter used by `BarRuntime::tick` |
| `config-toml` | validated shared TOML `config::BarConfig` with XDG lookup |
| `logging-flexi` | `logging::init` with rotation |
| `provider-alsa` | ALSA audio manager/runtime adapter |
| `provider-system` | sysinfo CPU/memory provider (no battery dependency) |
| `provider-brightnessctl` | brightnessctl provider |
| `provider-network-sysfs` | dependency-free primary-interface throughput provider |
| `provider-battery-sysfs` | independent deterministic multi-battery sysfs provider |
| `transport-shared` | typed JWM transport plus core-managed bounded recovery |
| `runtime-linux` | `AlignedTimer`, owned token-based `Epoll`, reconnect-aware notifier ownership, and owned wake forwarding |
| `render-cairo` | Scene-based `CairoRenderer`, text measurer, `CairoBar`, and validated CPU-frame rendering (`render_into_bgra`, `CpuCanvas`) |
| `glass` | Compositor-coupling constants (`fallback_rgb`, `DEFAULT_BACKGROUND_OPACITY`) for every native and toolkit bar, plus the baked-backdrop machinery (`frost`, `GlassCache`, the `WallpaperSource` trait, Cairo surface conversions when `render-cairo` is on) that only the Tauri bridge still consumes |
| `glass-wallpaper` | `glass` plus the file-backed `WallpaperFile` source and PNG/data-URL export (adds an image codec); consumed only by the Tauri webview frontends |

### Frosted glass

A bar cannot see through itself, so real glass is the compositor's to provide.
Every non-Tauri bar decides its mode once at startup, detection first: is a
compositing manager active (`_NET_WM_CM_S<screen>` has an owner, or the
toolkit's equivalent signal), and can this pipeline actually deliver per-pixel
alpha to the server? When both hold, the bar creates a translucent window and
paints its background at `background_opacity` (default
`glass::DEFAULT_BACKGROUND_OPACITY`, 0.55) so the compositor blends — and,
under JWM, blurs — the desktop behind it. When either fails, the bar paints
`glass::fallback_rgb(theme)` fully opaque; the opacity key is ignored rather
than washed over an undefined clear. The check is startup-only (the visual is
a creation-time decision), and owning the CM selection only proves compositing
is on, not that any blur is configured behind the bar.

The baked path — sample the wallpaper where the bar sits, blur and saturate
that strip, draw the scene over it — still lives in this module, but only the
Tauri webview bridge takes it: `backdrop-filter` blurs what is inside the
page, never the desktop behind the window, so the page is handed a pre-frosted
strip as a PNG (`to_png`, `to_png_data_uri`) and composites its own tint over
it in CSS. Point `glass.wallpaper` at the file the compositor draws;
`GlassStrip`, `WallpaperFile`, `GlassCache`, `frost`, and the Cairo
conversions remain for that consumer and should not gain new ones.
`render_over(None)` is still exactly `render`, so retained Cairo call sites
keep compiling either way.

## Shell surface

Bars can reach the window manager's own shell — launcher, notification center,
clipboard history, calendar, wallpaper picker — the way DMS, Noctalia,
Caelestia, and end-4 all do it: one bar entry that routes to every page.

`xbar_core` implements none of those pages. `ShellRoute` names one, and
`UserAction::OpenShellHub` asks the window manager to open it; everything the
page then does belongs to the window manager. A bar therefore gains the whole
feature without gaining any shell state to keep in sync.

```rust
use xbar_core::{ShellRoute, UserAction};

// One cell reaches every page: primary opens it, scroll walks the cycle.
let open_notifications = UserAction::OpenShellHub(ShellRoute::Notifications);
assert_eq!(ShellRoute::Hub.next(), ShellRoute::Applications);
assert_eq!(ShellRoute::from_key("control-center"), Some(ShellRoute::Hub));
```

`CairoBar` / `LayoutEngine` frontends project the entry automatically; toolkit
and webview frontends dispatch the action from their own widget. The entry is
tied to `wm_available`, so an unreachable window manager grays it out instead
of swallowing the click. See [docs/MIGRATION-0.8.md](docs/MIGRATION-0.8.md) for
the wire encoding, configuration keys, and default bindings.

## Companion crates

```toml
[dependencies]
xbar_core = { git = "https://github.com/beamiter/xbar_core.git", default-features = false }
xbar_linux_actions = { git = "https://github.com/beamiter/xbar_core.git" }
xbar_tauri = { git = "https://github.com/beamiter/xbar_core.git", features = [
  "clock-chrono", "provider-alsa", "provider-battery-sysfs",
  "provider-brightnessctl", "provider-system",
] }
```

```rust
use xbar_core::{BarEffect, PlatformEffectHandler};
use xbar_linux_actions::{CommandRunner, CommandSpec, ProcessActionHandler};

let mut actions = ProcessActionHandler::default();
actions.handle(BarEffect::Screenshot)?;

let output = CommandRunner::output(
    &CommandSpec::new("ip").with_args(["-4", "-o", "addr"]),
)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

Executable policy is configurable through `ProcessActionConfig`; the defaults
are `flameshot gui` and `pavucontrol`. Window placement, provider effects, and
WM commands are rejected as unsupported so another host adapter can handle
them explicitly. `CommandRunner` reuses `CommandSpec` for output-producing
host probes, rejects non-zero exits, retains stderr in its error, and never
invokes a shell implicitly.

`xbar_tauri::configure` installs the shared runtime worker, managed transport,
one `xbar-state` event, `dispatch_action`, `frontend_ready`, scale-aware window
placement, and the process-action adapter onto a caller-supplied Tauri builder.
Each application keeps its own generated context and optional plugins.

## Validation

```bash
cargo fmt --all -- --check
cargo test --no-default-features
cargo test --no-default-features --features render-cairo
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo doc --no-default-features --no-deps
```

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for module ownership,
[docs/CONSUMER-MATRIX.md](docs/CONSUMER-MATRIX.md) for every JWM bar family,
[docs/COMPANION-CRATES.md](docs/COMPANION-CRATES.md) for adapter boundaries,
[docs/MIGRATION-0.9.md](docs/MIGRATION-0.9.md) for compositor-coupled
translucency, and [docs/MIGRATION-0.7.md](docs/MIGRATION-0.7.md) for the
network/media states
([0.6 presentation/config](docs/MIGRATION-0.6.md),
[0.5 host-integration](docs/MIGRATION-0.5.md),
[0.4 projection/bridge](docs/MIGRATION-0.4.md), and
[0.3 lifecycle](docs/MIGRATION-0.3.md) notes remain relevant).

The repository intentionally does not declare a license until the project
owner selects and adds one.
