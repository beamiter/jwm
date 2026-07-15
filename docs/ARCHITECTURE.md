# Architecture

## Stable boundary

The portable boundary is semantic state, intent, and a renderer-neutral scene;
it is not an X11 event, winit callback, Cairo context, GPU surface, process, or
shared-memory layout.

```text
native input ──> UserAction ──> BarRuntime ──> BarModel
provider data ──> BarEvent ───────┘              │
WM snapshot ─────────────────────────────────────┤
                                                 ├── RuntimeUpdate
                                                 ├── BarView
                                                 └── BarSnapshot
                                                       │
                    PresentationConfig + TextMeasurer ─┤
                                                       ▼
                                               retained Scene
                                                │          │
                                           HitRegion   damage_from
                                                │          │
                                       semantic action   repaint
```

## Module ownership

- `model`: checked values, state invariants, reducer, typed events/effects,
  borrowed view, and owned serializable snapshot.
- `runtime`: optional provider/transport orchestration, managed reconnect
  policy, transport lifecycle status/generation, and portable service cadence.
  The embedded `BarModel` remains the only semantic state owner.
- `presentation`: owned dynamic configuration, logical-coordinate layout,
  stable scene nodes, interaction state, semantic hit regions, and old/new
  scene damage.
- `render::cairo`: Cairo/Pango scene renderer and the high-level `CairoBar`
  facade. It does not own window or transport resources.
- provider modules: independently selected ALSA, sysinfo CPU/memory,
  brightnessctl, and battery-sysfs adapters.
- `transport`: current JWM shared-memory adapter and queue outcome mapping.
- `notifier` and `linux`: owned eventfd/timerfd primitives. No public API asks
  callers to close a raw descriptor.
- `logging`: optional process-global logger setup.

## Frontend responsibilities

Every frontend owns:

- window creation, dock/strut properties, monitor placement, scale factor;
- translation of native pointer/key events into semantic actions;
- frame scheduling and execution of returned platform effects;
- event-loop registration of owned notifier/timer descriptors;
- renderer/GPU/surface lifetime and device-pixel transforms.

Core owns:

- state validation and normalization (`Percent`, checked tag IDs);
- provider/WM snapshot reduction and suppression of semantically unchanged frames;
- compact validated status plus rich provider projections (`SystemDetails`,
  `AudioDeviceInfo`) for toolkit and web frontends;
- semantic effects and explicit runtime failures/backpressure;
- a single layout/hit-test result shared by every renderer;
- correct damage as the union of previous and current component bounds.

## Feature rules

- `default = []`; a default build stays platform-neutral.
- Every optional feature must compile independently.
- No umbrella feature may silently enable unrelated platform capabilities.
- Frontend manifests use `default-features = false` and list exact adapters.
- Cairo/Pango and shared protocol concrete types are not re-exported from the
  crate root; adapters depend on their native libraries directly.
- `provider-system` never probes batteries; consumers that display battery
  state explicitly select `provider-battery-sysfs`.
- Bars open an existing WM-owned shared ring. They never create the protocol
  object implicitly, so dropping a consumer cannot destroy global transport.
- Only the core transport adapter depends on `shared_structures`; frontend
  repositories consume `SharedTransport`, `BarSnapshot`, and typed actions.
- A broken transport is reduced as `WindowManagerUnavailable`: the runtime
  drops the adapter, clears every WM-owned projection, returns
  `ClearMonitorGeometry` when a constraint had been active, and—when a
  `TransportRecoveryConfig` is installed—schedules a bounded reopen itself.
  Frontends do not maintain a second availability or retry cache.
- Installing or reopening a transport does not make WM state authoritative.
  `BarRuntime` rejects WM commands with an explicit issue until a fresh WM
  snapshot has been reduced, so a reconnect gap cannot target fallback monitor
  zero.
- Event-loop proxies coalesce shared notifications until the main loop has
  drained the transport, preventing an unbounded queue during UI stalls.
- Native notifier registration is an optimization, not the only progress
  path. Native timer turns also poll the transport, and frontends can observe
  `transport_generation()` to replace a notifier after reconnect.
  `RuntimeSchedule` always polls on service turns while coalescing missed
  provider ticks. This lets an old notifier generation go quiet without
  touching a UI loop from a worker thread.

## Removed 0.1 surface

Version 0.2 deliberately removes `legacy-full`, `AppState`, `BarConfig`,
`Colors`, `draw_bar*`, root Cairo/Pango re-exports, raw
`spawn_shared_eventfd_notifier`, `arm_second_timer`, `SHARED_TOKEN`, and root
`initialize_logging`.

Replacement mapping:

| Removed | Replacement |
|---|---|
| `AppState` | `BarRuntime` or `render::cairo::CairoBar` |
| fixed `BarConfig` | owned dynamic `presentation::PresentationConfig` |
| draw-time rectangle mutation | `LayoutEngine -> Scene + HitRegion` |
| `draw_bar*` | `CairoRenderer::render` / `CairoBar::render` |
| raw timerfd helper | `linux::AlignedTimer` |
| raw eventfd worker | `SharedEventNotifier` |
| direct shared messages/commands | private conversion inside `SharedTransport` |
| root logger function | `logging::init` |

Because consumers are separate Git repositories, this breaking release must
be published atomically: migrate and validate every consumer first, then tag or
pin the core revision before updating the remote dependency graph.
