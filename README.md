# xbar_core

`xbar_core` 0.2 is the backend-neutral status-bar kernel shared by the XCB,
x11rb, winit, tao, wgpu, pixels, softbuffer, toolkit, and web bars in JWM.

The default build has no window-system, Cairo, ALSA, sysfs, logging, or shared
memory dependency. There is no compatibility umbrella and no `legacy-full`:
every frontend selects only the adapters it actually uses.

## Architecture

```text
native/provider input -> BarRuntime -> BarModel -> BarSnapshot / BarView
                                             |
                                             v
                               LayoutEngine -> Scene + HitRegion
                                             |
                              Cairo / wgpu / toolkit / web renderer
```

- `BarModel` is the only semantic state owner. It reduces typed `BarEvent`
  values and emits change hints plus typed `BarEffect` values.
- `BarSnapshot` is an owned, serializable projection for toolkit, Tauri, and
  asynchronous consumers; it includes rich provider detail such as per-core
  CPU/memory counters and audio-device capabilities. `BarView` is the borrowed
  render fast path.
- `BarRuntime` coordinates optional providers and transport without absorbing
  platform/window responsibilities.
- `LayoutEngine` produces a renderer-neutral retained `Scene` and semantic hit
  map. `Scene::damage_from` invalidates both old and new component bounds.
- `CairoBar` is the high-level native facade combining runtime, layout,
  interaction, and Cairo rendering.

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

Open the existing WM-owned ring with `SharedTransport::open`, attach its owned
`SharedEventNotifier` to the native event loop, construct `BarRuntime`, then wrap it with
`render::cairo::CairoBar`. Native pointer events map to
`presentation::PointerAction`; unhandled `RuntimeUpdate::platform_effects`
remain the frontend's responsibility (window geometry, screenshots, and
process launching).

Toolkit and Tauri frontends use the same `BarRuntime` directly: poll/tick it,
project `BarSnapshot` into widgets or JSON, and dispatch typed `UserAction`
values. They do not depend on `shared_structures` or instantiate provider
managers themselves. `SystemDetails` and `AudioDeviceInfo` preserve the rich
provider data needed by those frontends without leaking adapter types.
If the shared transport breaks, the runtime drops it, marks the WM projection
unavailable, and returns any geometry-clear work before a frontend retries the
open. A reopened transport remains command-gated until its first authoritative
WM snapshot arrives; stale availability and monitor selection are never owned
by widget state.

## Features

| Feature | Capability |
|---|---|
| `clock-chrono` | Chrono clock adapter used by `BarRuntime::tick` |
| `logging-flexi` | `logging::init` with rotation |
| `provider-alsa` | ALSA audio manager/runtime adapter |
| `provider-system` | sysinfo CPU/memory provider (no battery dependency) |
| `provider-brightnessctl` | brightnessctl provider |
| `provider-battery-sysfs` | independent deterministic multi-battery sysfs provider |
| `transport-shared` | typed, consumer-owned JWM `SharedTransport` |
| `runtime-linux` | `AlignedTimer` and owned shared event notifier |
| `render-cairo` | Scene-based `CairoRenderer`, text measurer, and `CairoBar` |

## Validation

```bash
cargo fmt --all -- --check
cargo test --no-default-features
cargo test --no-default-features --features render-cairo
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo doc --no-default-features --no-deps
```

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for module ownership and
[docs/MIGRATION-0.2.md](docs/MIGRATION-0.2.md) for the breaking API mapping.

The repository intentionally does not declare a license until the project
owner selects and adds one.
