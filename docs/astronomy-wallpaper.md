# Astronomy Earth wallpaper (X11)

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

One globe is centered on the X framebuffer, spanning adjacent monitors rather
than repeating on each monitor. Coordinates use framebuffer pixels, not logical
monitor origins. This is deliberately an initial global-wallpaper mode.

## Time and appearance

UTC drives an approximate astronomical Sun direction, including seasonal
solar declination and equation of time. The true current system instant is sampled
as UTC, without adding the presentation offset to solar calculations. The display
timezone defaults to UTC+8 and is configurable with `wallpaper_timezone_offset_hours`;
it neither reads nor changes the operating-system timezone. This offset provides
only a coarse starting viewing meridian, not a GPS location. The view completes
one slow orbit per UTC day. This is an original presentation choice, not a
claim to reproduce Apple's implementation. The night side is intentionally
dark; there are no live clouds, weather or city-light observations. The stationary
galaxy background is an artistic star field, not an astronomical catalogue or
live sky observation.

Animation samples current UTC and the configured display offset at at most 15 Hz, even if other clients repaint
faster. DPMS is sampled only while enabled, at two-second intervals. Standby,
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
