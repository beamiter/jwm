# Migrating from 0.1 to 0.2

Version 0.2 is intentionally source-breaking. `legacy-full` and the monolithic
root facade are removed rather than deprecated because all JWM bar consumers
are migrated in the same release.

## Manifest

Always disable defaults and list exact capabilities:

```toml
xbar_core = { git = "https://github.com/beamiter/xbar_core.git", default-features = false, features = [
  "logging-flexi", "provider-alsa", "provider-system"
] }
```

Add `render-cairo`, `runtime-linux`, `transport-shared`, and the remaining
provider features only for native bars that use them. Depend on `cairo-rs` and
`pango` directly when native types occur in frontend code. In particular,
`provider-system` now covers only CPU/memory; battery UI must opt into
`provider-battery-sysfs` and consume `BarSnapshot::battery`. Toolkit and Tauri
frontends that display provider data select the provider features on
`BarRuntime`; they do not construct provider managers separately. An XCB Cairo
surface enables `cairo-rs`'s `xcb` feature in that frontend rather than forcing
XCB into every renderer.

## API mapping

```rust
// 0.1
// let mut state = AppState::new(shared_buffer);
// draw_bar(cr, width, height, colors, &mut state, font, config)?;

// 0.2
use xbar_core::render::cairo::CairoBar;
use xbar_core::{BarRuntime, ModelConfig};
use xbar_core::presentation::{PresentationConfig, Size};

let runtime = BarRuntime::new(ModelConfig::default())?;
let mut bar = CairoBar::new(runtime, PresentationConfig::default(), font);
bar.render(cr, Size::new(width as f32, height as f32))?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

- Pointer motion/leave goes through `CairoBar::pointer_motion` and
  `pointer_leave`.
- Native buttons/wheels map to `PointerAction` and
  `CairoBar::pointer_action`.
- Timer events call `CairoBar::tick`; shared notifications call
  `CairoBar::poll_transport`.
- `SharedTransport::open(path)` returns `io::Result` and only opens an existing
  WM-owned ring. A bar no longer creates/destroys protocol storage implicitly.
- Protocol conversion is private to `SharedTransport`; the model no longer
  accepts `SharedMessage` or exposes `SharedCommand` compatibility methods.
- Transport errors make the runtime drop the broken adapter and reduce
  `WindowManagerUnavailable`, clearing stale WM fields and active monitor
  geometry before reporting the issue. Retry loops only reopen and install a
  fresh `SharedTransport`; commands remain rejected until that transport
  supplies a new authoritative WM snapshot.
- Keep a periodic nonblocking `poll_transport` fallback even when an fd/proxy
  notifier is installed. A frontend can then reopen with bounded backoff on
  its normal tick without rebuilding native event-loop registration from a
  worker thread.
- Frontends must process `RuntimeUpdate::issues` and `platform_effects`.
- Toolkit/Tauri state is projected from `BarSnapshot`; rich fields live in
  `system_details` and `audio_device`. Remove direct `shared_structures` and
  provider-manager dependencies from frontend code.
- Use `ViewTagOn`, `ToggleTagOn`, and `SetLayoutOn` when a remote/web command
  carries an explicit monitor; native current-monitor actions can keep the
  shorter variants.
- Override `ModelConfig::clock_minute_format` and `clock_second_format` when a
  frontend has a different existing clock presentation.
- Register `linux::AlignedTimer` and `SharedEventNotifier` by borrowed raw fd;
  keep the owning Rust values alive instead of manually closing descriptors.
- Use `logging::init` instead of the removed root logging function.

Provider and WM values are authoritative in 0.2. User actions emit effects;
the model changes when the corresponding provider or WM snapshot arrives.

## Coordinated publication

Publish `xbar_core` before regenerating application lockfiles. A Git
dependency lock records the exact core commit, so a consumer lock generated
before that commit exists cannot represent the migration. After publication,
run the normal locked-dependency update in every bar repository and commit the
resulting `Cargo.lock` together with that consumer's migration. This also
aligns the transitive `shared_structures` revision used by `transport-shared`.
