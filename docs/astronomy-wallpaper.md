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
wallpaper_sky_rotation_minutes = 1440 # slow 24-hour galaxy travel; 0 = static
wallpaper_earth_view = "fixed" # default; "rotating" enables the period below
wallpaper_earth_latitude = 23.13 # Guangzhou city-level framing, not device location
wallpaper_earth_longitude = 113.28 # degrees east; stays fixed as time passes
wallpaper_timezone_offset_hours = 8 # retained compatibility setting, integer -12..14
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
1. `$XDG_DATA_HOME/jwm/astronomy/earth-nasa-october.jpg` (default `~/.local/share`).
2. A legacy `$XDG_DATA_HOME/jwm/astronomy/earth-nasa-january.jpg` override, if present.
3. `../share/jwm/astronomy/earth-nasa-october.jpg` relative to the real executable's
   directory. Managed releases therefore use their own immutable version assets.
4. Each absolute `$XDG_DATA_DIRS` entry (default `/usr/local/share:/usr/share`).
5. Debug builds only: the compile-time source checkout's `assets/astronomy`.

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
share the same current UTC instant, geographic camera, and texture. Coordinates
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
canvas containing a complete body; it does not crop a shared globe. Earth stays
fixed by default, while Moon retains its independently rotating presentation.
For example, replace the connector names below with your actual outputs:

```toml
[behavior]
wallpaper_effect = "astronomy"
wallpaper_astronomy_layout = "earth-moon"
wallpaper_earth_texture = "" # bundled Earth texture
wallpaper_moon_texture = "" # bundled Moon texture
wallpaper_earth_view = "fixed" # "rotating" opts into Earth camera rotation
wallpaper_earth_latitude = 23.13
wallpaper_earth_longitude = 113.28
wallpaper_earth_rotation_minutes = 1440 # used only by rotating Earth view
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
`moon-nasa-lroc.jpg` in place of `earth-nasa-october.jpg`. Mirrored outputs share
one canvas; the alphabetically first connector supplies its body assignment. Layout, body map, textures,
and rotation periods support normal reload and hot-config updates; hot changes
are session-only unless saved to the configuration file. Static wallpapers,
daily rotation, and the default `span` layout remain unchanged.

## Fixed geographic view and real daylight

Every Earth canvas now defaults to `wallpaper_earth_view = "fixed"`. Latitude
23.13°N and longitude 113.28°E frame Guangzhou, using the city-level point from
[UNGEGN / China Institute of Toponymy](https://ungegn.un.org/dashboard/cities/details?id=2049).
This is not a precise home/device location. Configure latitude within
−90..90 and longitude within −180..180 (finite degrees, east positive). The
camera and land stay fixed throughout the day; UTC, date and Earth's seasonal
tilt move the sunlight and day/night boundary independently. All three layouts
use the same policy. Reload/hot updates change framing without decoding textures.

The explicit `"rotating"` view retains accelerated camera tours:
`wallpaper_earth_rotation_minutes = 1440` makes one turn per day, or use 20 for
a twenty-minute tour. Existing period values remain stored, but do not move a
fixed camera. The configured longitude is the tour's epoch origin; UTC drives
phase, independently of timezone. This is camera motion, not an accelerated
physical Sun. Earth always uses the actual current solar illumination.

Moon defaults to 39343 minutes (about 27.3 days), rounded from NASA's
[655.720-hour sidereal rotation period](https://nssdc.gsfc.nasa.gov/planetary/factsheet/moonfact.html).
Current lunar illumination is always enabled and follows the actual UTC instant,
independently of this decorative texture spin. The terminator progresses smoothly
through new, waxing, full and waning phases, with waxing lit on the right and
waning on the left. Timezone changes and local midnight do not reset the phase.
The faint night-side fill is artistic, not a prediction of earthshine.

The calculation uses [Schlyter’s short Sun/Moon orbital model](https://stjarnhimlen.se/comp/ppcomp.html)
with all listed major lunar longitude/latitude/distance perturbations and finite
Sun distance, rather than an average calendar-month cycle. This is a low-precision
contemporary wallpaper model, not a navigation or eclipse ephemeris. Tests check
2026 [USNO primary phases](https://aa.usno.navy.mil/api/moon/phases/year?year=2026)
and NASA illumination within 0.5 percentage point; that fixture tolerance is not
a guaranteed all-date error bound. TT–UTC, aberration, observer parallax, tidal
locking and libration are omitted. Invalid clocks or dates outside 1900–2100
use a deterministic 2000-01-01 UTC phase fallback to keep GPU uniforms finite.
The stable horizontal presentation does not
model Guangzhou’s changing local-horizon tilt or the celestial bright-limb angle;
the configured spin deliberately continues to reveal the whole lunar texture.
Restart/resume at the same instant restores the same view without accumulated
animation drift. Timezone/DST changes never rotate Earth or move the Sun.
`wallpaper_astronomy_timezone` and `wallpaper_timezone_offset_hours` remain
compatibility settings; there is no user-facing sunrise/sunset clock. Neither
changes the OS timezone or clock.

The Sun is calculated locally from UTC and the date, using solar declination
and the equation of time, following [NOAA calculation details](https://gml.noaa.gov/grad/solcalc/calcdetails.html)
and its [solar equations](https://gml.noaa.gov/grad/solcalc/solareqns.PDF).
The internal sunrise/sunset test/diagnostic helper uses that same ephemeris
and the conventional −0.833° geometric solar-center horizon (approximate
refraction and solar radius). Its caller supplies a numeric offset to select the
local civil day (the Guangzhou fixtures use UTC+8). Results distinguish polar
day/night and transition days.
No location API, online weather service or calendar of guessed clock times is
used. The Guangzhou fixtures use the above city point; UTC+8 alone would be insufficient.
Terrain, altitude and actual atmospheric refraction are not modeled, so these
are approximate events, not guaranteed observed sunrise/sunset times. The globe's
terminator comes directly from its surface-normal/Sun dot product; it is never
warped to force a timezone-wide sunrise time. The shader adds a narrow artistic
twilight and a sunlight-dependent atmosphere rather than a screen-space mask.

This is an original renderer inspired by the fixed-location, time-dependent
presentation in the supplied iPhone reference. Apple describes its astronomy
wallpapers as updating with live conditions in its
[iOS 17 feature overview](https://www.apple.com/mideast/ios/ios-17/a/pdf/iOS_All_New_Features_en-JO.pdf).
No Apple UI, assets, watermark or proprietary implementation is copied.
The existing NASA surface and static Blue Marble cloud composite remain offline,
historical imagery, not current weather. Custom Earth maps do not receive the
extra cloud layer. There are no live cloud or city-light observations. The
NASA/Gaia-derived galaxy backdrop retains decorative framing rather
than representing the observer's live sky.

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

The default image is NASA's Blue Marble Next Generation October 2004 monthly
surface composite, not real-time satellite imagery. The surface does not switch
automatically with the month. The prior January image remains bundled/installed
under its original filename for explicit choices, and user-owned override files
are never changed or removed. Source: NASA Earth
Observatory, [Blue Marble Next Generation base map](https://science.nasa.gov/earth/earth-observatory/blue-marble-next-generation/base-map/).
[Original 5400×2700 JPEG](https://assets.science.nasa.gov/content/dam/science/esd/eo/images/bmng/bmng-base/october/world.200410.3x5400x2700.jpg).
SHA256: `ccfd9b7a5e5ee601e1466556716bba50ea418f3a8e068ab0194ae0425815b1b0`.

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

Full source and media-use attribution accompany all bundled images in
`assets/astronomy/README.md`. Moon loading is also offline; if either required
texture fails in Earth/Moon mode, the static wallpaper remains visible.

## Validation and limits

The actual fragment shader compiles, links and renders in Mesa surfaceless EGL
on desktop GLSL 330 and GLES 300. Corresponding RGBA frames match
exactly and remain opaque. Coverage includes dated Guangzhou UTC+8
00/06/12/18 views, fixed-camera controls with a frozen Sun, seasonal northern/
southern/polar views, explicit rotating Earth, legacy controls, and
5120×1440 single-globe previews. Landmarks/hemisphere orientation
were visually inspected. These are real shader tests, not compositor runtime
or NVIDIA driver verification. CPU tests cover clock cap/pause/resume, clipping,
union coverage, fixed/rotating view separation, NOAA-reference solar positions,
approximate sunrise/sunset, polar transitions, civil-date boundaries and invalid
clocks. The sunrise/sunset helper is test/diagnostic-only, not extra work in each
render frame.

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
the Guangzhou contact sheet shows 2026-10-16 at 06:00, 12:00, 18:00 and
00:00 UTC+8 using the documented city coordinates.
Neither test modifies the OS clock or timezone. Asset tests use placeholder
executables and private temporary install roots; they do not validate a live
compositor binary. Full config/picker tests still require the Cargo workspace
and its existing dependencies.

## Slow galaxy motion

`wallpaper_sky_rotation_minutes = 1440` is the default: one calm panoramic circuit
per 24 hours. Set it to `0` to restore the exact original stationary framing,
`720` for a 12-hour circuit, or `60` for a visibly faster one-hour preview. The range
is integer `0..=525600`; normal config reload applies it without texture reload.
This controls only the background, not Guangzhou's fixed Earth camera, solar
lighting, lunar phase or either body's rotation period.

The angle uses absolute Unix UTC time, with a full turn reduced in double
precision before computing the two rotation components. Restarting at the same
time reproduces the same sky; display timezone and DST have no effect. Sleep
and occlusion pause rendering, then resume at the current UTC orientation.
Changing the system clock or period can reposition the sky intentionally.

Motion pans through the full panorama: old sky leaves the edge as new sectors
enter. The aspect-correct rectilinear ray yaws through the equirectangular
catalogue, keeping latitude inside the poles even on ultrawide screens. This
slightly changes edge framing relative to the old static crop. The procedural
fallback uses a smoothly blended periodic spatial field so its stars and dust
travel together without a hard reset. Neither is
an observational planetarium or a simulation of the sky above Guangzhou.
There is no twinkle, new texture, extra texture fetch, file I/O or new timer:
the procedural fallback evaluates its field twice to blend its wrap seam;
frames reuse the existing capped 15 FPS wallpaper schedule and power/occlusion
gates. The phase wrap is continuous. Static mode retains the existing schedule
because UTC sunlight and lunar lighting still update.
