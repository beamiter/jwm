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

`xbar_linux_actions::ProcessActionHandler` handles only `Screenshot` and
`OpenAudioControl`. It owns configurable command specs, synchronous
launch-error reporting, non-blocking child waiting, and zombie prevention.
Other effects are returned as explicit errors rather than being silently
ignored. The same crate's separate `CommandRunner` executes output-producing
host probes directly (without a shell), accepts only successful exit status,
and preserves stderr in structured failures.

`xbar_linux_actions::EffectRouter` (0.2) composes that handler with the
standard Linux host policy for one `RuntimeUpdate`: issues and unhandled
effects are logged, geometry effects go to the caller's window closure, and
only window-system errors can fail the route. Bars with non-standard effect
policy keep using `RuntimeUpdate::handle_platform_effects` directly.

### `xbar_present_wgpu`

Owns the wgpu 30 surface lifecycle for CPU-rendered bars: sRGB format choice,
BGRA/RGBA upload conversion with 256-byte row alignment, damage-aware
sub-rectangle upload, outdated/lost surface recovery, and the fullscreen blit.
It accepts any `Into<wgpu::SurfaceTarget>` so XCB, x11rb, winit, and tao
consumers share one presenter. It never renders scenes or owns windows.

### `xbar_dbus_providers`

Owns D-Bus service policy (bus names, object paths, service quirks) and
translates desktop services into model values. `UPowerBatteryProvider`
reduces UPower's display device to `BatteryState`; `MprisMediaProvider` (0.2)
reduces the first MPRIS player on the session bus to `MediaState`. Hosts feed
polled media state through `BarEvent::Media`.

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

Own atom interning and typed property writes with concrete connection and
window types. Since 0.5 the protocol itself (`DockWindowSpec` atom names and
typed values) lives in core placement, so these adapters would only remove the
small generic intern/write loop; they stay below the extraction threshold
until a third X11 lifecycle appears.

### Remaining presentation adapters

`xbar_present_wgpu` shipped in 0.6 (below). `xbar_present_pixels` and
`xbar_present_softbuffer` stay unextracted: after `CpuCanvas` and damage
metadata, each backend keeps only a few lines of library-specific present
calls, which is below the extraction threshold.

## Extraction threshold

Add a companion only after the same lifecycle appears in at least three
consumers or two independent backend families. Keep application branding,
framework widget trees, CSS, and one-off policy local.
