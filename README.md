# xbar_core

`xbar_core` is the shared status-bar core used by the XCB, x11rb, winit, tao,
wgpu, pixels, softbuffer, and toolkit-based bars in JWM.

The crate now has two API layers:

- `model` is backend-independent. It reduces `BarEvent` into a read-only
  `BarView`, visual `DirtyBits`, and typed `BarEffect` values. It never opens a
  window, invokes a process, accesses ALSA/sysfs, or writes shared memory.
- `legacy-full` is the default compatibility facade. It preserves the existing
  `AppState`, Cairo/Pango renderer, Linux providers, shared-memory notifier,
  timer, and logging APIs while frontends migrate incrementally.

## Pure model usage

Use only the backend-independent layer:

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
    let view = model.view();
    // Render `view` with Cairo, wgpu, egui, GTK, HTML, or another backend.
}

for effect in update.effects {
    // Execute the intent in the frontend/provider adapter.
    println!("{effect:?}");
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

For the current JWM transport, enable `transport-shared`. The compatibility
conversion is explicit: `SharedMessage -> WmSnapshot` and
`WmCommand::into_shared_command()`.

## Existing frontend migration

Existing manifests do not need to change: default features preserve the old
surface. A low-level frontend can migrate in small steps:

1. Replace its copied visual defaults with `BarConfig::desktop_emoji()` and
   `tuned_colors_for_theme()` or `draw_bar_for_theme()`.
2. Replace its hand-written provider timer with `AppState::tick()`.
3. Replace raw notifier ownership with `SharedEventNotifier`.
4. Translate native input into `UserAction`, execute returned `BarEffect`
   values, and render `BarModel::view()`.
5. Once migrated, disable default features and opt into only the adapter crates
   or features it actually uses.

The legacy dirty-render entry points currently do a correct full redraw for any
non-empty change set. The previous partial implementation erased unchanged
widgets after repainting the whole background. True partial redraw requires a
retained layout/scene diff with old and new bounds.

## Feature overview

| Feature | Purpose |
|---|---|
| `legacy-full` | Existing all-in-one API; enabled by default |
| `transport-shared` | JWM `shared_structures` compatibility bridge |
| `provider-alsa` | ALSA audio provider |
| `provider-system` | sysinfo system provider |
| `provider-brightnessctl` | brightnessctl provider |
| `provider-battery-sysfs` | Linux sysfs battery provider |
| `render-cairo` | Cairo/Pango exports and legacy renderer dependency |
| `runtime-linux` | Linux fd/event runtime support |
| `logging-flexi` | Legacy flexi_logger integration |

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the target layering and
frontend migration rules.

## Validation

```bash
cargo fmt --all -- --check
cargo test --all-targets
cargo test --no-default-features
cargo check --all-features
cargo clippy --all-targets --all-features -- -D warnings
```

The repository intentionally does not declare a license until the project
owner selects and adds one; downstream redistribution should not infer a
license from dependencies.
