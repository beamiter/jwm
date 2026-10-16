# Status bars

JWM can spawn a status bar named in `config_*.toml` (`status_bar.name`). All bar
crates live under this directory and share protocol helpers from `xbar_core`.

## Official bar

**`tao_glow_bar`** is the supported daily-drive bar. `scripts/install_jwm_scripts.sh`
installs it by default. Release packaging and the maintainer checklist exercise
this crate.

```bash
scripts/install_jwm_scripts.sh                  # installs tao_glow_bar
scripts/install_jwm_scripts.sh -b tao_glow_bar  # explicit
```

## Example bars

Every other crate in this directory is an **example** or experiment (toolkit
coverage, web frontends, alternate presenters). They remain in-tree so
contributors can compare implementations, but they are not the product default
and are not required for a SOTA daily-drive install.

Use `-b <name>` only when you intentionally want an example bar.

## Shell Hub

Whatever bar you run, the Shell Hub entry should open the same built-in control
center documented in [docs/control-center.md](../docs/control-center.md).

`tao_glow_bar` defaults to 42 logical pixels of height so it matches
`appearance.status_bar_height`. Hide or restore pills from
`~/.config/xbar/config.toml` (`[presentation.visibility]`,
`presentation.shell_routes`). The monitor index pill is off unless you turn
it on.

## Wallpaper-aware workspace tags

`[presentation] tag_contrast = "auto"` (the default) selects a tag-only light or
dark material from the WM's monitor wallpaper luminance. Missing samples fall
back to the theme. `"theme"` ignores the wallpaper; `"dark"` and `"light"` pin the
tag material. These options are read at startup, like the other bar config.
Live WM wallpaper samples and runtime theme changes update the palette.

The policy lives in `xbar_core::tag_contrast::TagPalette`. Empty and occupied tags
use strongly separated near-opaque neutral surfaces. Occupied tags also carry a
small, high-contrast bottom dash; empty tags have no dash. This cue stays visible
when selected, urgent or hovered, so color is never the only occupancy signal.
The cue follows actual occupancy, not the selected/filter `filled` flag.
Scene renderers preserve the original icon box and use actual glyph metrics to
show the dash only when the pill has 12px of vertical clearance beyond the
measured glyph height. Oversized fonts or very short pills keep their original
text placement and rely on the stronger occupied surface instead.
Selected/filled tags retain the
shared selected accent; urgent tags take precedence and retain the urgent
accent. Text and borders come from that same policy. The bar's background,
compositor mode, non-tag pills, hit targets and actions are unchanged.

JWM measures the wallpaper layer after wallpaper/astronomy drawing and before
any client or overlay. It reads one center scanline per monitor (at most 16,384
RGBA pixels, 65,536 bytes) and averages 12 evenly spaced CPU samples in linear
luminance. Static results are cached until source/geometry changes; animated
wallpaper is sampled at most once per second on existing frames. The read is
synchronous, so GPU timing still needs live validation. Only the small versioned
luminance statistic crosses shared memory; no pixel row leaves the compositor.
A partial repair never reads retained pixels: an initial/invalidated probe may
request one full scene repair. EMA smoothing and a 38/54 hysteresis band around
the 46/255 policy threshold prevent rapid light/dark flips.

Neutral pills use 94% alpha to bound arbitrary local wallpaper variation. Shared
numerical tests establish at least 4.5:1 foreground contrast across RGB extremes
under both encoded-sRGB and linear-light alpha blending. This covers color
contrast, not font rasterization quality or physical-display calibration.

### Frontend paths

- Scene consumers (egui; tao/winit/x11rb/xcb pixel, softbuffer, Glow and WGPU
  variants) use the shared Scene palette through their existing renderers.
- GTK and Relm use the shared `xbar_gtk` widget styling.
- GPUI, Iced and Xilem convert shared palette colors into their
  toolkit colors. Their old independent rainbow tag fills are no longer used.
  GPUI and Iced apply complete hovered colors, including their matching borders.
  Xilem makes only the inner workspace Button background/border transparent via
  local properties (which outrank Masonry's hover/active theme defaults), leaving
  the shared material on its fixed-size outer box visible in every state.
- GPUI-component remains on its original, independent legacy palette and is
  **not wallpaper-adaptive** in this change. Its pinned 0.6.6 Button transforms
  custom fills and overrides interaction borders; adoption is deferred until
  its native styling adapter is implemented and validated. No contrast-policy
  guarantee is made for this example frontend.
- Dioxus emits tag-scoped CSS from the shared Rust palette on each snapshot.
- Tauri React, Solid, Svelte, Vue, Leptos and Yew receive tag-scoped CSS from
  `xbar_tauri`. The bridge refreshes it when theme or monitor wallpaper palette
  changes and reinstalls it on frontend-ready replay. No JavaScript luminance
  or contrast formula is maintained. Legacy CSS remains as a startup/error
  fallback until the bridge stylesheet is installed; it is not wallpaper-aware.

CSS overrides are restricted to workspace buttons inside `.buttons-container`.
They suppress old tag opacity/press overlays that would defeat the shared
contrast colors, leaving the rest of the page and backdrop alone. A missing WM
sample uses theme fallback, rather than guessing from the local display. Negative
or off-canvas monitor coordinates and wallpaper sample rows above 16,384 pixels
also use this fallback; their producer sampling path is not adaptive.

### Validation limits

Shared palette tests cover the numerical contrast policy independently of any
GUI. Frontend adoption is not a claim that every experimental toolkit has been
built or visually exercised: offline checks can be blocked by uncached Git
checkouts/packages (GPUI, Iced, Xilem, Dioxus), and the standalone Tauri bridge
currently has a workspace-membership build blocker. Runtime screenshots on the
supported compositor and toolkit versions are still needed, especially for
native widget hover/press styling and WebKit stylesheet installation. The
extracted frontend-policy harness passes five Rust tests (including urgent plus
selected precedence and CSS palette serialization); extracted class selectors
pass twenty Node assertions across the four JavaScript frontends. These are
source-level tests, not full toolkit or browser integration tests.
