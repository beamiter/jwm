# Architecture

## Boundary

`xbar_core` serves many frontends with very different window/event/rendering
systems. The portable boundary is semantic state and intent, not an X11 event,
Cairo context, winit callback, or wgpu surface.

```text
native input ──> frontend adapter ──> BarModel::update ──> ModelUpdate
provider data ───────────────────────>       │             ├── DirtyBits
WM snapshot ─────────────────────────>       │             └── BarEffect[]
                                             ▼
                                           BarView
                                             │
                         Cairo / wgpu / egui / GTK / HTML renderer
```

The core owns:

- checked identifiers and canonical state invariants;
- event reduction and optimistic UI state;
- provider/WM snapshot normalization;
- semantic effects;
- a read-only renderer projection;
- change classification.

A frontend or adapter owns:

- XCB/x11rb/winit/tao/toolkit event translation;
- window creation, dock/strut properties, monitor placement, scale factor;
- event-loop wakeups and frame scheduling;
- execution of IPC, audio, brightness, screenshot, and other effects;
- the concrete renderer and GPU/surface lifetime.

## Current modules

- `model`: portable reducer, views, effects, monitor geometry, and the optional
  current-JWM transport bridge.
- `notifier`: owned eventfd bridge with cancellation and thread joining.
- `audio_manager`, `system_monitor`, `brightness`, `battery`: compatibility
  Linux providers, individually feature-gated.
- `legacy` (inside `lib.rs`): current AppState/Cairo/runtime/logging facade.

The default feature keeps all existing consumers building. A pure frontend can
already compile with `--no-default-features`.

## Compatibility rules

Until every frontend migrates:

- preserve root and module-path manager imports;
- preserve `AppState`, `BarConfig`, Cairo/Pango re-exports, `draw_bar*`,
  `arm_second_timer`, `spawn_shared_eventfd_notifier`, and `SHARED_TOKEN`;
- do not add required fields to `BarConfig`, because existing consumers use
  complete struct literals;
- add new behavior through constructors, presets, and methods;
- keep `legacy-full` enabled by default.

## Next structural milestones

1. Move the compatibility facade from the large `lib.rs` module into explicit
   adapter crates without changing re-exported paths.
2. Introduce provider traits and fake implementations; make controller tick
   scheduling return the next provider deadline.
3. Build a renderer-neutral layout tree and scene. Layout must produce both
   paint bounds and hit actions, rather than mutating hit rectangles while
   drawing.
4. Diff old/new scenes to compute damage as the union of old and new bounds,
   including shadows. Redraw every node intersecting each damage clip.
5. Move the JWM `shared_structures` conversion to a dedicated transport crate.
6. Migrate XCB and x11rb first, then wgpu/pixels/softbuffer, then winit/tao,
   and finally toolkit/HTML frontends.

XCB-specific Cairo visuals, pixmap back buffers, EWMH atoms, and X connection
error handling belong in an `xbar-xcb` adapter. They are valuable platform
infrastructure but are not portable core state.
