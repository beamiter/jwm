# JWM architecture

JWM is split into a process shell, an application composition root, window
management policy, platform backends, and reusable state/layout code.

```text
src/main.rs                 process setup (logging, locale, D-Bus)
    |
    v
src/application.rs          backend selection and application lifecycle
    |
    +-------> src/jwm/      window-management policy and use cases
    |              |
    |              v
    +-------> src/core/     state, models, layout and animation
    |
    v
src/backend/api.rs          platform boundary
    |
    +-- x11rb / xcb / X11 compositor
    +-- Wayland udev / X11 / winit
```

## Dependency rules

1. `main` may depend on `application` and OS process services only.
2. `application` is the composition root. Concrete backend constructors belong
   here; policy code must not select a concrete backend.
3. `jwm` implements window-management behavior against `backend::api::Backend`.
4. `core` contains state and deterministic policy. New core code should avoid
   concrete backend modules; platform-neutral IDs and events should gradually
   move from `backend` into `core`.
5. Backend implementations may depend on `core` and compositor-common code,
   but must not call JWM feature modules directly. Events cross the boundary
   through the backend event-handler interfaces.

## Current hotspots

- `backend/api.rs` is a broad interface. Split it by capability (windowing,
  input, outputs, rendering, capture) as implementations are migrated.
- `jwm::Jwm` owns both durable WM state and infrastructure caches. Move feature
  state behind focused services before adding more fields.
- X11RB and XCB backends duplicate substantial behavior. Extract protocol-free
  operations into `backend/x11` and retain only transport adapters in each.
- X11 and Wayland compositor trees contain parallel render/effect modules.
  Prefer `backend/compositor_common` for platform-neutral algorithms.
- `config.rs` is large and process-global. Separate schema, loading, validation,
  defaults and live-reload; pass an immutable configuration snapshot inward.

## Incremental migration order

1. Keep `cargo check --all-targets` and unit tests green as the safety baseline.
2. Introduce typed application events and split the backend API by capability.
3. Move JWM feature state into screenshot, recording, overview and magnifier
   services with explicit inputs/outputs.
4. Consolidate X11 transports and compositor-common algorithms.
5. Gate concrete backends with Cargo features so a production build compiles
   only the selected platform stack.

## Migration status

- Application composition root extracted from the process bootstrap.
- Interactive move/resize transport state moved into the backend contract.
- Key-repeat policy is now binding metadata produced by configuration; the
  Wayland input backend no longer inspects or imports concrete `Jwm` methods.
- Compositor benchmarking is the first capability extracted from the monolithic
  `Backend` trait. Application startup now depends on `CompositorBenchmark`
  rather than the complete platform interface for benchmark configuration.
- Read-only compositor and protocol telemetry now lives in `BackendDiagnostics`:
  performance, direct scanout, presentation, capture, XWayland, session lock,
  tearing hints, color management and protocol-bind snapshots are separated
  from commands that mutate backend state.
- Compositor-wide visual mutations now live in `CompositorControl`: color
  temperature, saturation, brightness, contrast, inversion, grayscale, debug
  HUD, transition mode, slime toggle and live config application no longer
  expand the core `Backend` method set.
- Capture and media workflows now live in `CompositorMedia`: full/region
  screenshots, static/live thumbnails, recording lifecycle and audio timing
  are isolated from window-management and general compositor controls.
- Workspace transition effects are moving into `CompositorWorkspaceEffects`.
  Tag transitions, magnifier state, snap-preview lifecycle, overview/expose and
  monitor-layout synchronization are isolated behind backend-specific ID and
  refresh-rate adapters.
- Per-window visual state is moving into `CompositorWindowEffects`. Frame
  extents, shaped-window flags, urgency, picture-in-picture, wobbly movement,
  pointer/edge-glow effects, dock targets, peek, tab groups and zoom-to-fit are
  isolated behind native window-ID adapters.
- Accessibility color correction and interactive screen drawing now live in
  `CompositorAnnotation`, separating annotation stroke state from the general
  backend lifecycle.
- Output hardware queries and mutations now live in `DisplayControl`, covering
  VRR capabilities/toggles, KMS color-pipeline capabilities and HDR metadata.
- Lightweight render scheduling now lives in `RenderScheduler`: render requests,
  compositor presence, pending-render state and overlay identity are separated
  from frame production and compositor resource initialization.

Each step should be behavior-preserving and land independently. Avoid moving a
module and changing its behavior in the same change unless tests cover it.
