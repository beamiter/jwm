# Adopting 0.9 compositor-coupled translucency

Version 0.9 changes no `xbar_core` API. What changes is policy: every
non-Tauri bar stops baking its own frosted wallpaper strip and couples its
translucency to the compositor instead. The `glass` module keeps compiling
unchanged — the baked path simply loses all consumers except the Tauri
webview bridge.

## The uniform mode decision

Each bar decides once, at startup, before creating its window:

```text
translucent = compositor_active && alpha_capable
```

- `compositor_active`: a compositing manager owns the `_NET_WM_CM_S<screen>`
  selection (GTK bars ask `gdk4::Display::is_composited()`, which watches the
  same selection on X11). Any error at any step reads as *no compositor*.
- `alpha_capable`: the bar's pipeline can actually deliver per-pixel alpha to
  the server — a depth-32 TrueColor visual for native X bars, a
  `PreMultiplied`/`Inherit` composite alpha mode for wgpu presenters, a
  depth-32 window for softbuffer, `is_rgba()` for GTK, and so on. A toolkit
  that cannot deliver alpha forces solid mode; it must never bake.

The check is deliberately startup-only: the visual (or the toolkit's
transparency flag) is a creation-time decision, so a compositor started or
stopped after bar launch takes effect on the next bar start. Note also that
owning the CM selection proves compositing is on, not that any blur is
configured behind the bar.

## Translucent mode

A 32-bit ARGB window (or the toolkit's transparent-window equivalent). The
background layer is painted in `glass::fallback_rgb(theme)` at the top-level
`background_opacity` config key, default
`glass::DEFAULT_BACKGROUND_OPACITY` (0.55). Foreground elements keep their
own alpha, unmultiplied. Scene bars get the color for free — the palette
background is byte-identical to `fallback_rgb` — and only set
`set_background_opacity(Some(opacity))`.

## Solid mode

A plain opaque window painted in `fallback_rgb(theme)` at exactly 1.0.
`background_opacity` is ignored — a configured 0.55 must never leak into an
opaque window as a wash over an undefined clear. Scene bars set
`set_background_opacity(None)`. Where a bar toggles its theme live, the solid
color follows the runtime theme, not a startup snapshot.

## What remains, and for whom

The `[glass]` TOML table still parses and `glass.wallpaper` still points at
the compositor's wallpaper file, but only the Tauri webview bridge reads
them: `backdrop-filter` cannot blur the desktop behind a webview, so its
pages are still handed a pre-frosted PNG strip. `GlassStrip`,
`WallpaperFile`, `GlassCache`, `frost`, `GlassBackdrop`, and
`CairoRenderer::render_over` all stay — `render_over(..., None)` is exactly
`render` — but nothing outside `xbar_tauri` should gain a new dependency on
them.

## Migrating a bar

- Downgrade the `xbar_core` feature `glass-wallpaper` → `glass`; the bar
  drops the image codec and keeps `fallback_rgb` plus
  `DEFAULT_BACKGROUND_OPACITY`.
- Delete the wallpaper plumbing: root-pixmap capture, `WallpaperSource`
  implementations, `GlassBackdrop` state, and the per-frame backdrop branch.
  The redraw path collapses to a plain `render`.
- Make the background opacity conditional on the startup mode decision:
  `Some(config.background_opacity.unwrap_or(DEFAULT_BACKGROUND_OPACITY))`
  when translucent, `None` when solid.
