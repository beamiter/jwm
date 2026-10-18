# Status bars

JWM can spawn a status bar named in `config_*.toml` (`status_bar.name`). All bar
crates live under this directory and share protocol helpers from `xbar_core`.
Bars support the top, bottom, left and right output edges. The complete
configuration, interaction and geometry contract is in
[Four-edge bars](../docs/bars-four-edges.md).

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

The Hub's **Sound & Display** section includes saved controls for the current
screen's bar edge, all-screen thickness and margins. Edge and thickness changes
restart the affected bar processes; margin changes reposition them. These
controls preserve unrelated configuration settings and comments.

## Placement and thickness

Edit the active JWM configuration, normally
`~/.config/jwm/config_x11.toml` or `~/.config/jwm/config_wayland.toml`:

```toml
[appearance]
status_bar_height = 48 # Physical thickness: height at top/bottom, width at left/right
status_bar_padding = 4

[status_bar]
name = "tao_glow_bar"
show_bar = true
position = "top" # top, bottom, left, right

[[status_bar.monitors]]
monitor = 0
position = "left"

[[status_bar.monitors]]
monitor = 1
position = "bottom"
```

Monitor IDs come from `jwm-tool msg get_monitors`. A monitor without an override
uses `status_bar.position`; old configurations with no position keep the top
edge. Thickness defaults to **42 physical pixels** for JWM-managed bars. The
historical `status_bar_height` key also controls sidebar width. The WM reserves
the matching strip on each output and supplies the effective edge/thickness to
its bar process, including on scaled or very small outputs.

For a bar launched independently, `~/.config/xbar/config.toml` uses logical
units instead:

```toml
[presentation]
position = "right"
thickness = 42.0 # Alias for the existing bar_height key; logical pixels
```

Use either `thickness` or `bar_height`, not both. Managed launch values
`JWM_BAR_POSITION` and `JWM_BAR_THICKNESS` override these standalone settings.
`XBAR_POSITION=left` is a one-process standalone edge override.
See the [unit and precedence table](../docs/bars-four-edges.md#units-and-precedence)
before changing size on a scaled output.

Hide or restore pills from
`~/.config/xbar/config.toml` (`[presentation.visibility]`,
`presentation.shell_routes`). The monitor index pill is off unless you turn
it on.

## Sidebar interaction

Sidebars keep glyphs and window thumbnails upright. The shared Scene stacks
clock components and short readings, protects the active workspace and key
status controls, and condenses long content. Workspace `+N` controls and mouse
wheel navigation keep omitted workspaces reachable. Dock magnification follows
the bar's main axis while click targets remain fixed; actionable Dock `+N`
restores the most recent locally omitted window.

GTK/Relm and HTML bars retain overflowing control groups in native or DOM
scrollers. GTK controls support native focus and tooltips. The common HTML
adapter adds visible keyboard focus, Enter/Space activation, arrow navigation
along the displayed axis, and Home/End. Focused controls scroll into view.
The [interaction guide](../docs/bars-four-edges.md#interaction-and-overflow)
distinguishes these adapters from the shared Scene's pointer controls.

## Package boundaries and checks

The 22 non-Tauri bar packages are independent of the root Cargo workspace so
their GUI dependency graphs stay separate. Fifteen consume the shared Scene;
the other seven use GTK/Relm, GPUI/GPUI-component, Iced, Xilem or Dioxus. The six
Tauri frontend applications share `xbar_tauri`, whose explicit `[workspace]`
boundary allows it to be checked independently too.

Run these commands from the repository root:

```bash
cargo check --manifest-path bars/tao_glow_bar/Cargo.toml
cargo check --manifest-path crates/xbar_core/crates/xbar_tauri/Cargo.toml
cargo test -p xbar_core --all-features
cargo run -p xbar_core --features render-cairo --example render_four_edges -- /tmp/jwm-bar-previews
```

The final command renders the actual Cairo/Pango scene at all four edges in
dark/light themes and at 1×/2×, with a PNG and matching scene JSON for each case.
It needs no live WM or display server. The full
[validation matrix](../docs/bars-four-edges.md#validation) separates package
checks, automated geometry/interaction tests and native visual smoke tests.

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
accent. Text and borders come from that same policy. Wallpaper adaptation
controls tag paint; normal shared layout and input policy still determine
geometry, hit targets and actions.

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
GUI. All 22 non-Tauri bar packages and the standalone Tauri bridge have passed
package checks during the four-edge refactor. The bridge also passes 13 library
tests; Leptos and Yew pass Rust checks for `wasm32-unknown-unknown`. React, Solid,
Svelte and Vue production builds pass. The layout browser regression uses each
of the seven HTML frontends' real stylesheet and exercises four edges, three
viewport sizes, keyboard interaction, transformed/scrolling Dock geometry and
fixed Dioxus allocations. A separate 16-case suite loads the actual built
React/Solid/Svelte/Vue applications on all four edges in Chromium, with a mock
Tauri host, and verifies readiness, keyboard actions and scrolling Dock
geometry. Native X11 smoke tests cover `tao_glow_bar` on all four edges and GTK
on all four edges at 1×/2×.

This coverage does not establish visual parity for every experimental toolkit
or behavior on physical Wayland outputs. Actual font availability, compositor
materials, native hover/press styling and WebKit rendering still depend on the
desktop environment. The experimental Iced adapter has no explicit bar keyboard
traversal, and Xilem's in-window tooltips can clip within a narrow sidebar.
JWM's native minimized-window previews follow the inward-facing edge placement.
The supported daily-drive target remains `tao_glow_bar`.
