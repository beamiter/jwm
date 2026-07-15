# xbar_core

`xbar_core` 0.3 is the backend-neutral status-bar kernel shared by the XCB,
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
  platform/window responsibilities. It can own transport opening, bounded
  reconnects, lifecycle status, and notifier generations.
- `RuntimeSchedule` gives high-frequency event loops one portable service
  call: transport is polled every turn while providers tick at a bounded
  monotonic cadence.
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

For core-managed recovery, construct `TransportRecoveryConfig` and use
`BarRuntime::with_managed_transport`; its first poll opens the existing
WM-owned ring and later polls retry boundedly after failures. If low-latency
native notification is useful, create a `SharedEventNotifier` from the current
transport and rebuild it whenever `transport_generation()` changes. Then wrap
the runtime with `render::cairo::CairoBar`. Native pointer events map to
`presentation::PointerAction`; unhandled `RuntimeUpdate::platform_effects`
remain the frontend's responsibility (window geometry, screenshots, and
process launching).

Toolkit and Tauri frontends use the same `BarRuntime` directly. A
`RuntimeSchedule` replaces their local `last_tick`, reconnect deadline, and
`tick + poll` merge logic; they project `BarSnapshot` into widgets or JSON and
dispatch typed `UserAction` values. They do not depend on `shared_structures`
or instantiate provider managers themselves. `SystemDetails` and
`AudioDeviceInfo` preserve rich provider data without leaking adapter types.
If the shared transport breaks, the runtime drops it, marks the WM projection
unavailable, returns geometry-clear work, and schedules its own reopen. A
reopened transport remains command-gated until its first authoritative WM
snapshot arrives; stale availability and monitor selection are never owned by
widget state.

```rust
use std::time::Duration;
use xbar_core::{
    BarRuntime, ModelConfig, RuntimeSchedule, TransportRecoveryConfig,
};

let recovery = TransportRecoveryConfig::new("/run/user/1000/jwm", Duration::from_secs(2))?;
let mut runtime = BarRuntime::with_managed_transport(ModelConfig::default(), recovery)?;
let mut schedule = RuntimeSchedule::default();

// Call from a framework timer, idle callback, or native event-loop turn.
let update = schedule.service(&mut runtime);
if update.needs_redraw() {
    let snapshot = runtime.snapshot();
    # let _ = snapshot;
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Features

| Feature | Capability |
|---|---|
| `clock-chrono` | Chrono clock adapter used by `BarRuntime::tick` |
| `logging-flexi` | `logging::init` with rotation |
| `provider-alsa` | ALSA audio manager/runtime adapter |
| `provider-system` | sysinfo CPU/memory provider (no battery dependency) |
| `provider-brightnessctl` | brightnessctl provider |
| `provider-battery-sysfs` | independent deterministic multi-battery sysfs provider |
| `transport-shared` | typed JWM transport plus core-managed bounded recovery |
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

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for module ownership,
[docs/CONSUMER-MATRIX.md](docs/CONSUMER-MATRIX.md) for every JWM bar family,
and [docs/MIGRATION-0.3.md](docs/MIGRATION-0.3.md) for lifecycle adoption.

The repository intentionally does not declare a license until the project
owner selects and adds one.
