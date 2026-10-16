# Astronomy Earth and Moon wallpaper (X11)

Astronomy is an optional material in JWM's existing wallpaper feature. It draws
inside the compositor background pass, below all windows, with no new window,
focus target or input region. Both xcb and x11rb use the same implementation.
Native Wayland does not implement this material yet; the picker explains that
limitation and leaves the existing static wallpaper selected.

## Configuration

```toml
[behavior]
wallpaper_effect = "astronomy" # "off" is the backwards-compatible default
wallpaper_earth_texture = "" # bundled NASA Earth texture
wallpaper_timezone_offset_hours = 8 # presentation timezone, integer -12..14
wallpaper_astronomy_layout = "span" # "per-output" shows one complete globe per screen
```

Use the existing Wallpaper picker to select Astronomy Earth, or select a normal
image to return to static wallpaper. Existing `wallpaper`, per-monitor/tag
choices and `wallpaper_rotation = "daily"` remain the fallback collection.
No network fetch occurs inside JWM. The repository, source installer and release
archives include the NASA Earth image and attribution. An explicit texture path
must name a regular, decodable 2:1 equirectangular daytime map; a bad explicit
path never silently falls back to the bundle. Changing these fields uses normal
config reload. Display-timezone changes repaint without reloading the texture.

With an empty texture setting, both picker and renderer use this order:
1. `$XDG_DATA_HOME/jwm/astronomy/earth-nasa-january.jpg` (default `~/.local/share`).
2. `../share/jwm/astronomy/earth-nasa-january.jpg` relative to the real executable's
   directory. Managed releases therefore use their own immutable version assets.
3. Each absolute `$XDG_DATA_DIRS` entry (default `/usr/local/share:/usr/share`).
4. Debug builds only: the compile-time source checkout's `assets/astronomy`.

There is no current-working-directory lookup. Source installs place the image
and credit in `/usr/local/share/jwm/astronomy`. Release upgrades preserve the
stable-link map; rollback switches executable and version-local assets together,
and uninstall removes those version assets through the existing managed flow.
User data overrides remain untouched. A running process keeps its loaded texture;
restart JWM after a managed release rollback to use that release's executable and
asset. Missing assets leave static wallpaper intact and the picker explains the
missing prerequisite.

`wallpaper_astronomy_layout = "span"` is the default: one globe is centered on
the X framebuffer and spans adjacent monitors. Choose `"per-output"` to show a
complete globe on each output, using that output's size and position. All globes
share the same current UTC instant, display timezone, and texture. Coordinates
use framebuffer pixels.

This setting controls Astronomy placement only. `wallpaper_mode` still controls
static-image scaling (`fill`, `fit`, `stretch`, or `center`). Changing layout uses
the existing config reload or hot-config path; selecting a static image in the
Wallpaper picker preserves the Astronomy layout for the next time it is enabled.
For a persistent per-screen choice, set `wallpaper_astronomy_layout = "per-output"`
in `[behavior]` and reload the configuration. Hot changes are session-only unless
also written to the configuration file.

## Earth on one output, Moon on another

With `wallpaper_effect = "astronomy"`, the single enabling setting is
`wallpaper_astronomy_layout = "earth-moon"`. Each output gets an independent
canvas containing a complete spinning body; it does not crop a shared globe.
For example, replace the connector names below with your actual outputs:

```toml
[behavior]
wallpaper_effect = "astronomy"
wallpaper_astronomy_layout = "earth-moon"
wallpaper_earth_texture = "" # bundled Earth texture
wallpaper_moon_texture = "" # bundled Moon texture
wallpaper_earth_rotation_minutes = 1440 # 24 hours
wallpaper_moon_rotation_minutes = 39343 # about 27.3 days
wallpaper_astronomy_timezone = "system" # or "configured"
wallpaper_astronomy_bodies = { "DP-1" = "earth", "HDMI-1" = "moon" }
```

The explicit connector-name map guarantees stable body assignments across
hotplug and restart. Without an explicit assignment, outputs sorted by connector
name alternate Earth, Moon, Earth, Moon. A single output defaults to Earth unless
explicitly assigned Moon. If you want a lone remaining Moon output to stay Moon
after unplugging the Earth output, use the explicit map. Unknown/disconnected
connector entries remain available for when that output returns.

The map defaults to empty and accepts at most 128 entries. Connector names must
be nonempty, at most 256 bytes, and contain no NUL; body names are exactly
`earth` or `moon`. Each rotation period is an integer from 1 through 525600 minutes.
The Moon texture setting defaults to empty for the bundled asset and follows the
same bounded-path validation and lookup order as the Earth texture, using
`moon-nasa-lroc.jpg` in place of `earth-nasa-january.jpg`. Mirrored outputs share
one canvas; the alphabetically first connector supplies its body assignment. Layout, body map, textures,
and rotation periods support normal reload and hot-config updates; hot changes
are session-only unless saved to the configuration file. Static wallpapers,
daily rotation, and the default `span` layout remain unchanged.

Earth defaults to a 24-hour presentation turn. Moon defaults to 39343 minutes
(about 27.3 days), rounded from NASA's [655.720-hour sidereal rotation period](https://nssdc.gsfc.nasa.gov/planetary/factsheet/moonfact.html).
These are independent, configurable periods; set e.g. 20 and 30 minutes for an
accelerated decorative view. Existing explicit period settings are preserved;
only missing settings receive the new defaults.

Both phases are anchored to the current UTC system instant rather than process
uptime, so restarting or resuming at the same instant reproduces the same angle.
Earth adds a timezone-derived observer meridian exactly once; it does not shift
UTC before advancing the clock. The default `wallpaper_astronomy_timezone = "system"`
reads the system's current offset, including fractional hours and daylight saving.
An offset change moves Earth's observer meridian but does not change either
rotation period or Moon's phase. If a system offset cannot be resolved, the
configured offset is used (default UTC+8). No OS clock or timezone is changed.

For a fixed UTC+8 observer, including users who previously set an explicit offset,
use both settings in `[behavior]`:

```toml
wallpaper_astronomy_timezone = "configured"
wallpaper_timezone_offset_hours = 8
```

The offset is a coarse camera meridian, not a geographic location. The 24-hour
Earth view is the requested civil-day presentation, not a claim to reproduce
Earth's shorter sidereal rotation precisely. Moon's phase origin is a stable
UTC epoch, not a calibrated lunar orientation or current lunar phase; tidal
locking/orbits/libration are not simulated. This mode retains fixed decorative
view-space lighting. Legacy `span` and `per-output` keep their existing UTC
solar illumination and configured timezone behavior below, unaffected by the
new timezone-source and rotation-period controls.

## Time and appearance

In `span` and `per-output` modes, UTC drives an approximate astronomical Sun direction, including seasonal
solar declination and equation of time. The true current system instant is sampled
as UTC, without adding the presentation offset to solar calculations. The display
timezone defaults to UTC+8 and is configurable with `wallpaper_timezone_offset_hours`;
it neither reads nor changes the operating-system timezone. This offset provides
only a coarse starting viewing meridian, not a GPS location. The view completes
one slow orbit per UTC day. This is an original presentation choice, not a
claim to reproduce Apple's implementation. The night side is intentionally
dark. A static NASA Blue Marble cloud composite adds cloud structure to the
bundled Earth map; it is historical imagery, not current weather. Explicit custom
Earth textures remain unmodified by this extra layer. There are no live clouds,
weather or city-light observations. The stationary
galaxy background is an artistic star field, not an astronomical catalogue or
live sky observation.

Regular animation ticks are capped at 15 Hz. Enabled prepare checks the current
system instant and applicable timezone offset; a timezone change requests an
immediate repaint. DPMS is sampled only while enabled, at two-second intervals. Standby,
suspend, off, an unavailable extension or failed query pauses animation; retry
continues every two seconds so wake/error recovery does not depend on input.
The normal static wallpaper is retained when the animation cannot render.
A fully covered framebuffer pauses animation using a conservative union of
opaque, untransformed, square client rectangles (bounded to 128 rectangles).
Overview and transitions do not use that occlusion shortcut. Shader/image
failures stop animation and preserve static wallpaper until config changes.
Daily static selection continues in the background and is restored on disable.

Transparent bars see the actual globe. Bars using sampled-wallpaper fallback
continue to show their static sample. Frosted bars invalidate their blur cache
on each animation update, which costs more than clear bars. Fifteen Hz bounds
animation updates, not repaint work demanded by other windows. No GPU power
claim has been established for the user's hardware.

## Source image and usage

The bundled image is NASA's Blue Marble Next Generation January 2004 monthly
surface composite, not real-time satellite imagery. Source: NASA Earth
Observatory, [Blue Marble Next Generation base map](https://science.nasa.gov/earth/earth-observatory/blue-marble-next-generation/base-map/).
[Original 5400×2700 JPEG](https://assets.science.nasa.gov/content/dam/science/esd/eo/images/bmng/bmng-base/january/world.200401.3x5400x2700.jpg).
SHA256: `99f5faad74efe985fbf1714c8be7296ca9999759a1215b65f99b7f1df278dde5`.

NASA source material is acknowledged under its [images and media guidelines](https://www.nasa.gov/nasa-brand-center/images-and-media/).
This visualization is independently rendered; NASA has not reviewed or endorsed
JWM. No NASA logo is included. The unchanged image and its attribution are bundled
under `assets/astronomy`, installed as data rather than embedded in the executable,
and do not create a runtime download dependency.

The Moon map is the unchanged NASA SVS [CGI Moon Kit](https://svs.gsfc.nasa.gov/4720/)
2048×1024 LROC color map (`moon-nasa-lroc.jpg`, 457,942 bytes).
Credit: NASA’s Scientific Visualization Studio; LROC/LOLA.
SHA256: `f7130a1822681fa7512d7dcfd40db8c10b9ba4f06777910348698260ed7a2170`.
The optional cloud mask is NASA Goddard / Reto Stöckli’s 2002 Blue Marble
composite, bundled unchanged at 2048×1024 as `earth-nasa-clouds.jpg`. It uses
one extra texture sample on Earth (none on Moon), with a maximum 8 MiB RGBA
upload. If it is missing or fails to load, both surface materials still work.

The renderer uses linear-light surface shading, a restrained ocean glint and
thin Earth limb, an airless lunar regolith lighting approximation, and an
irregular decorative Milky Way band with dust lanes. These are visual models,
not a physically calibrated atmosphere or terrain-displacement simulation.

Full source and media-use attribution accompany all three images in
`assets/astronomy/README.md`. Moon loading is also offline; if either required
texture fails in Earth/Moon mode, the static wallpaper remains visible.

## Validation and limits

The actual fragment shader compiles, links and renders in Mesa surfaceless EGL
on desktop GLSL 330 and GLES 300. Twelve corresponding RGBA frame pairs match
exactly and remain opaque; UTC 00/06/12/18, UTC+0/+8, frame-to-frame motion and
5120×1440 single-globe previews were checked. Landmarks/hemisphere orientation
were visually inspected. These are real shader tests, not compositor runtime
or NVIDIA driver verification. CPU tests cover clock cap/pause/resume, clipping,
union coverage, equinox/solstice lighting and timezone view separation.

Config/picker regressions and X11 integration must pass repository CI before
release. Local full Cargo tests are blocked by the uncached workspace `drm`
dependency; no dependency changes were made. DPMS hardware transitions,
compositor framebuffer state, live blur/damage and multi-output deployment need
runtime verification with rollback. Keep the previous executable and static
wallpaper configuration before any authorized local deployment.

### Reproduce validation

With the existing Rust toolchain, Python 3, Pillow, and Mesa EGL/OpenGL/GLES
libraries installed:

```sh
python3 scripts/test-astronomy-assets.py
python3 scripts/test-release-install-failures.py
rustc --edition 2024 --test src/backend/compositor_common/animated_wallpaper.rs -o /tmp/jwm-astronomy-tests
/tmp/jwm-astronomy-tests
ASTRONOMY_TEST_OUTPUT=/tmp/jwm-astronomy-preview python3 scripts/test-astronomy-shader.py
```

The shader test defaults to the repository's bundled texture and writes frames
and metrics to its output directory. Fixed UTC/offset cases are regressions;
the presentation preview at 2026-10-15T14:01:14Z uses display offset UTC+8.
Neither test modifies the OS clock or timezone. Asset tests use placeholder
executables and private temporary install roots; they do not validate a live
compositor binary. Full config/picker tests still require the Cargo workspace
and its existing dependencies.
