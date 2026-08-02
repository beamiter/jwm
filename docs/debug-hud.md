# Debug HUD

`Alt+Shift+F12` (`toggle_debug_hud`) draws the compositor's live counters as a
material card in the top-left corner, on both the X11 and the Wayland backend.
The same key turns the extended sections on, so one press gets everything.

## Reading the card

| Part | Shows |
| --- | --- |
| Title row | `JWM Compositor`, with a chip naming the backend and whether the extended sections are on |
| Meter | Frame rate against the fastest connected monitor's refresh rate |
| Label column | The counter names, in the dim body tone |
| Value column | The readings, in the bright primary tone |

The meter is the part to watch out of the corner of an eye: **green** at 90% of
the refresh target or better, **amber** down to 60%, **red** below that. The
numeric FPS is the first row of the stat list, so the meter never has to be
read precisely.

Sections group the counters — `FRAME`, `SCENE`, `SYSTEM`, and, once the
extended sections are on, `RENDER`, `INPUT LATENCY` and `PROFILER`. The
profiler rows are `avg / min / max` milliseconds per zone over the last 120
frames, and they appear only while the frame profiler has samples.

`MEMORY` and `CPU` are JWM's own process, read from `/proc/self/*` — not the
machine's, which is what the status bar's resource rows show. See
[resources.md](resources.md).

## Styling

The card uses the same tones, radii and elevation as the system-UI launcher,
the notification toasts and the OSD — all four follow `appearance.ui_theme`,
see [ui-theme.md](ui-theme.md) — and its text is rasterized with the configured
`appearance.system_ui_font` rather than the built-in bitmap face. Everything
that is not GL — the row model and the layout arithmetic — lives in
`src/backend/compositor_common/debug_hud.rs`, shared by both compositors; each
backend only rasterizes the four text sections and issues the draws.

A proportional UI font makes the profiler's `avg / min / max` triples ragged
because nothing pads them into columns. The default
(`SauceCodePro Nerd Font Regular 11`) is monospaced and lines up.

## Turning it on at startup

```toml
[behavior]
debug_hud = true
debug_hud_extended = true
```

`debug_hud_extended` also switches the frame profiler on, which costs a
per-zone timer on every frame; leave it off for a long-running session.
