# Everyday UI ergonomics

The October 2026 pass concentrates on three everyday interactions: entering
search text without losing clipboard history, recognizing a button press
without chasing a moving target, and reading small text over different
wallpapers. These changes use the existing shared shell, palette and bar scene.

## Clipboard search and deliberate deletion

Open history with `Alt+Ctrl+V`, or choose Clipboard from the Shell Hub. Every
printable character, including `c`, `d` and spaces, belongs to the search query.
The earlier bare-letter shortcuts could delete history while a user typed an
ordinary word; they have been replaced with explicit management keys.

| Intent | Interaction |
| --- | --- |
| Search | Type a word or phrase; Backspace edits it |
| Browse | Arrow keys, Tab / Shift+Tab, Home / End, or the pointer wheel |
| Copy a result | Enter, keypad Enter, or left click |
| Forget one result | Delete, or middle click on its row |
| Clear history | Ctrl+Delete, then a fresh unmodified Enter to confirm |
| Cancel the question | Escape or click outside; the query and selection remain |
| Leave the picker | Escape follows the existing picker → Hub → close hierarchy |

Delete auto-repeat cannot erase several entries with one physical press. The
clear question displays the whole history count, including entries outside the
current filter. While it is open, row activation, wheel browsing and scrollbar
seeking cannot change its subject. A clipboard update invalidates the question
before newly copied entries could be cleared using an older confirmation.

See [Clipboard history](clipboard.md) for capture and storage behavior.

## Press feedback with stable targets

The shared bar scene now distinguishes hover from a held pointer press for
workspace pills, status controls, minimized-window cards and the `+N` restore
entry. A contrasting inset outline supplies feedback while the existing text,
fill, selected/urgent indication and hit rectangle remain in place.

The feedback disappears when the press is cancelled, the pointer leaves, or
the captured action is replaced. Matching release still owns activation. An
unavailable button action does not arm a press. Cairo and Egui both invalidate
the scene when only the pressed state changes, including a stationary pointer.

The same policy applies to top, bottom, left and right bars. The existing
transparent/frosted background setting remains independent of control feedback.

The actual shared Cairo/Pango renderer can produce repeatable fixtures:

```bash
cargo run --locked -p xbar_core --features render-cairo \
  --example render_four_edges -- /tmp/jwm-bar-idle standard
cargo run --locked -p xbar_core --features render-cairo \
  --example render_four_edges -- /tmp/jwm-bar-pressed pressed
cargo run --locked -p xbar_core --features render-cairo \
  --example render_four_edges -- /tmp/jwm-bar-dock pressed-dock
```

Each directory contains the four edges at 1× and 2× in both light and dark
themes, together with the corresponding scene and hit-test JSON. The pure
geometry regressions additionally cover 1.25× and 1.5× scale.

## Readable secondary text

The palette treats footer shortcuts, HUD labels, notification attribution and
other normal-size text as information people need to read. Their quieter
hierarchy comes from position and tone, without an extra opacity reduction
that makes them disappear into a glass surface.

The palette tests use a 4.5:1 minimum for the covered normal-text pairs. Light
glass is checked over dark backdrops and dark glass over bright backdrops;
query fields and chips include their additional fill composition. In linear
color rendering, dark surfaces need extra tint coverage to produce a readable
background. The shared resolver derives that coverage from the text luminance
and the glass shader's interior sheen and grain, with rounding headroom.

The real X11 OpenGL and Wayland OpenGL ES glass shaders also run under the
headless graphics tests. Readback checks cover bright/dark backdrops, three
glass themes, four kinds of text-bearing surface and multiple interior points;
Wayland is checked in both encoded and linear color modes.

These are bounded checks of settled text-bearing interiors. They are not a
claim of complete WCAG conformance, a physical-display calibration, or coverage
of HDR tone mapping, animated fades, decorative bevel highlights, antialiased
glyph edges or arbitrary user accent colors. See [UI theme](ui-theme.md).

## Validation and rollout

Run the root package and shared bar gates:

```bash
cargo fmt --all -- --check
EGL_PLATFORM=surfaceless JWM_REQUIRE_HEADLESS_GL=1 scripts/test.sh
scripts/test.sh -p xbar_core --all-features --all-targets
cargo check --locked --all-targets
```

The standalone Egui bar has its own manifest and lockfile; its pointer-state
regression is run with that manifest. Desktop acceptance should then exercise
clipboard filtering/confirmation and held/released/cancelled bar controls on
the installed backend, including a mixed-DPI multi-monitor setup.

Rebuild JWM and the selected bar, then restart their processes to use the new
rendering and input code. A configuration reload alone does not replace a
running binary. The defaults and existing configuration files need no new key.

## Design references

- [GNOME HIG: Keyboard](https://developer.gnome.org/hig/guidelines/keyboard.html)
  — ordinary navigation, predictable text entry and discoverable actions.
- [W3C: Understanding Contrast (Minimum)](https://www.w3.org/WAI/WCAG22/Understanding/contrast-minimum.html)
  — the 4.5:1 reference for normal-size text. JWM uses it as a design and
  regression target for the explicit surface pairs above.
