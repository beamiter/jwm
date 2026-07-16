# Companion crate boundaries

`xbar_core` owns portable state, intent, cadence, projections, and renderer-
neutral scene data. Companion crates reuse integration code whose concrete
dependencies or lifecycle belong to a host platform.

## Why separate packages

- Dependency isolation: a Tauri upgrade cannot change the build graph of an
  XCB or GTK consumer.
- Independent compatibility: adapters can follow different backend major
  versions while the semantic protocol remains stable.
- Smaller feature matrices: core feature combinations remain meaningful
  instead of multiplying every GUI and renderer backend together.
- Focused tests: process ownership, X11 properties, GPU surface recovery, and
  webview emission can each be exercised at their actual boundary.
- Incremental adoption: one consumer family can migrate without forcing all
  24 repositories to select the same framework dependencies.

The dependency direction is always:

```text
consumer -> companion adapter -> xbar_core
consumer -----------------------> xbar_core
```

No edge may point from `xbar_core` to a companion.

## Current adapters

`xbar_linux_actions` handles only `Screenshot` and `OpenAudioControl`. It owns
configurable command specs, synchronous launch-error reporting, non-blocking
child waiting, and zombie prevention. Other effects are returned as explicit
errors rather than being silently ignored.

### `xbar_tauri`

Owns Tauri state registration, one `xbar-state` envelope event, one checked
action command, frontend replay, scale-factor window placement, and a bounded
runtime worker. `configure` accepts a caller-created builder so the generated
Tauri context, application plugins, program name, and logging remain in a tiny
consumer `main.rs`.

It must not own web-framework components, CSS, event-loop-global configuration,
or model DTO copies. React, Vue, Svelte, Solid, Leptos, and Yew consume
the same `FrontendEnvelope` schema directly.

## Proposed adapters

### `xbar_xcb` and `xbar_x11rb`

Own atom interning and typed writes for dock type, above state, desktop,
window name, and EWMH struts. They consume `BarPlacement`/`EwmhStrut` from core
but retain concrete connection and window types.

### Presentation adapters

`xbar_present_pixels`, `xbar_present_softbuffer`, and `xbar_present_wgpu` own
surface lifetime, resize/recovery, pixel format conversion, and damaged-region
upload. They consume `Scene` or a Cairo-produced buffer without moving GPU
types into core.

## Extraction threshold

Add a companion only after the same lifecycle appears in at least three
consumers or two independent backend families. Keep application branding,
framework widget trees, CSS, and one-off policy local.
