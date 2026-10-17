# Astronomy Earth and Moon wallpaper (X11)

Astronomy is an optional material in JWM's existing wallpaper feature. It draws
inside the compositor background pass, below all windows, with no new window,
focus target or input region. Both xcb and x11rb use the same implementation.
Native Wayland does not implement this material yet; the picker explains that
limitation and leaves the existing static wallpaper selected.

Earth and Moon support full-axis dragging and independent wheel zoom. Dragging
the empty background turns a perspective camera through the entire celestial
sphere, including both poles. See the [celestial explorer guide](celestial-explorer.md)
for the shared controls, sky catalogue and Solar System close-ups.

## Configuration

```toml
[behavior]
wallpaper_effect = "astronomy" # "off" is the backwards-compatible default
wallpaper_earth_texture = "" # bundled NASA Earth texture
wallpaper_sky_rotation_minutes = 1440 # 24-hour sky tour; 0 stops the automatic tour
wallpaper_earth_view = "fixed" # default; "rotating" enables the period below
wallpaper_earth_latitude = 23.13 # Guangzhou city-level framing, not device location
wallpaper_earth_longitude = 113.28 # degrees east; stays fixed as time passes
wallpaper_earth_roll = 0.0 # initial orientation around the viewing direction
wallpaper_earth_rotation_axis = [0.0, 1.0, 0.0] # used by "rotating" only
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
wallpaper_earth_roll = 0.0
wallpaper_earth_rotation_axis = [0.0, 1.0, 0.0]
wallpaper_earth_rotation_minutes = 1440 # used only by rotating Earth view
wallpaper_moon_view = "rotating" # "fixed" stops texture motion, not the lunar phase
wallpaper_moon_latitude = 10.313240312354818 # original 0.18-radian tilt
wallpaper_moon_longitude = 0.0
wallpaper_moon_roll = 0.0
wallpaper_moon_rotation_axis = [0.0, 1.0, 0.0]
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
one canvas; the alphabetically first connector supplies its body assignment.
Layout, body map, textures, orientation, axes and rotation periods support normal
reload and hot-config updates; hot changes
are session-only unless saved to the configuration file. Static wallpapers,
daily rotation, and the default `span` layout remain unchanged.

## Rotation in every direction

Earth and Moon each have independent latitude, longitude, roll, rotation axis,
period and fixed/rotating controls. Latitude and longitude select the initial
center; roll turns the initial image around that viewing direction, clockwise
for positive values and counterclockwise for negative values. These finite
degree values accept latitude `-90..=90` and longitude/roll `-180..=180`, including
the poles and a fully upside-down view.

`wallpaper_earth_rotation_axis` and `wallpaper_moon_rotation_axis` select an
arbitrary axis through the body's center. Each is a three-number TOML/JSON array
`[x, y, z]`, with every component finite and in `-1..=1`, and at least one nonzero.
JWM normalizes the vector, so `[0.5, 0.5, 0]` and `[1, 1, 0]` describe the same
direction. Negating every component reverses the tour; changing vector length
does not change its speed. Rotation uses an axis-angle transform and supports a
complete turn through the poles without clamping the animated latitude.

The axes are fixed in **body coordinates**: `+X` points toward the equator at
90° east, `+Y` toward the north pole, and `+Z` toward the equator at zero longitude.
They are not always the screen's horizontal, vertical and depth axes. The examples
below use initial latitude, longitude and roll all set to zero so the axes align
with the reference view:

| Axis value | Tour from the reference view |
| --- | --- |
| `[0.0, 1.0, 0.0]` | Horizontal sweep around the north/south polar axis; legacy default |
| `[1.0, 0.0, 0.0]` | Vertical sweep through the poles |
| `[1.0, 1.0, 0.0]` | Diagonal tour |
| `[-1.0, -1.0, 0.0]` | The same diagonal tour in reverse |
| `[0.0, 0.0, 1.0]` | Rotation around the reference view's line of sight |

For other initial centers, `[0, 0, 1]` still means the body's zero-longitude axis.
Use `wallpaper_earth_roll` or `wallpaper_moon_roll` to set the initial screen
orientation for any center. A tour rotates that entire initial camera basis
around the chosen body axis, preserving all three directions as a rigid frame.

For example, merge these keys into the existing `[behavior]` section to give
Earth a vertical tour and Moon a reversed diagonal tour:

```toml
wallpaper_earth_view = "rotating"
wallpaper_earth_latitude = 0.0
wallpaper_earth_longitude = 0.0
wallpaper_earth_roll = 0.0
wallpaper_earth_rotation_axis = [1.0, 0.0, 0.0]
wallpaper_earth_rotation_minutes = 20

wallpaper_moon_view = "rotating"
wallpaper_moon_latitude = 0.0
wallpaper_moon_longitude = 0.0
wallpaper_moon_roll = 0.0
wallpaper_moon_rotation_axis = [-1.0, -1.0, 0.0]
wallpaper_moon_rotation_minutes = 30
```

Set either axis to `[0.0, 1.0, 0.0]` for the horizontal example, or
`[1.0, 1.0, 0.0]` for the forward diagonal example. Set
`wallpaper_moon_view = "fixed"` to stop Moon texture motion while the current
lunar phase continues updating. Fixed mode uses the configured latitude,
longitude and roll directly; it does not hold the last frame of a moving tour.
The axis and period remain stored for a later return to rotating mode.

To apply a vertical Earth tour immediately in the running session, the repository's
existing CLI accepts a batch through `jwm-tool`:

```sh
jwm-tool msg set_config_batch --args '{"values":{
  "behavior.wallpaper_earth_view":"rotating",
  "behavior.wallpaper_earth_latitude":0,
  "behavior.wallpaper_earth_longitude":0,
  "behavior.wallpaper_earth_roll":0,
  "behavior.wallpaper_earth_rotation_axis":[1,0,0],
  "behavior.wallpaper_earth_rotation_minutes":20
}}'
jwm-tool msg set_config --args '{"key":"behavior.wallpaper_moon_view","value":"fixed"}'
```

The wallpaper effect must already be enabled, and Moon appears in the
`earth-moon` layout. These IPC updates are session-only; save the same keys in
the config file for restart persistence. Invalid axes, angles or modes reject
the whole batch without partially changing the scene. Orientation changes
reuse loaded textures and request a frame using the current system instant.

## Drag Earth, Moon or the sky with the mouse

Direct manipulation is enabled automatically for a rendered Astronomy globe
on either X11 transport; no extra configuration or modifier key is needed.

1. Place the pointer on the visible surface of Earth or Moon, on uncovered desktop.
2. Hold the left mouse button and drag horizontally, vertically or diagonally.
   Dragging around the edge also rolls the globe. Release and grab again to
   continue a turn through either pole or inspect the opposite hemisphere.
3. Release the left button to keep the resulting pose. A fixed view stays there;
   a rotating view resumes its configured tour smoothly from that pose.

Only the body under the initial press is selected. Earth and Moon keep independent
manual orientations and zoom; if the same body appears on several outputs,
those copies share that body's state. A press on empty sky starts a panorama
gesture: drag horizontally, vertically or diagonally to look in any direction,
passing through either pole without a latitude clamp. Release and grab again to
continue looking around. The background keeps its own orientation and lens,
separate from the body's pose. Existing window, bar, tab, modal, lock-screen
and configured mouse-binding handling takes
priority, so dragging a window or selecting text does not rotate the wallpaper.
The feature does not create a focusable window. The initial desktop click keeps
normal monitor-selection behavior; subsequent drag motion does not change focus
or select another body.

An active gesture captures the pointer. It may continue outside the globe,
across an existing window or onto another output, using the original globe's
pixel radius throughout. Sky gestures likewise keep the canvas and perspective
that were visible at the initial press. A virtual sphere joins a smooth outer
trackball surface near the limb, avoiding a sudden sensitivity clamp. Equivalent motion as a
fraction of the globe radius gives the same rotation on 1080p, 4K and portrait
outputs. Motion is anchored to the initial press rather than accumulated from
every input event, so coalescing a burst of mouse events does not change its
final orientation. The release position is included even if the last motion
was not displayed yet.

While held, the selected body's decorative tour pauses at the displayed pose,
but the actual Sun direction and lunar phase continue to use the current system
instant. The other body and sky retain their own animation. Releasing rebases
the selected tour at the current instant instead of snapping back to its old
epoch orientation. Timezone changes do not add another offset to the light.

The manual orientation lasts for this running session and does not rewrite
latitude, longitude, roll or rotation-axis settings. Changing a body's configured
orientation, fixed/rotating mode or period resets that body's manual offset and
zoom; reloading identical values preserves them. Restart restores the configured pose
and its current-time tour. Layout changes, output removal, unavailable rendering,
screen locking and modal capture retire an active drag safely. Layout/material
changes retain the current pose where possible rather than leaving a pointer
grab attached to a disappearing canvas.

Pointer-driven rendering has a separate, coalesced cadence capped at 60 Hz.
Holding the pointer still does not keep that fast cadence running: real-light updates use
the normal low-power schedule, and release retires the interaction wake after
its final frame. Texture, cloud and shader resources are reused throughout a
gesture; the background cache and sampled-bar identity follow accepted poses.
The native input queue retains left-button motion samples so an out-and-back
drag cannot become a Solar System click. Event processing has bounded batches;
a high-polling-rate mouse can still produce more CPU input work than the rendering
cadence. The pointer-grab watchdog remains active while a gesture is held.

## Zoom and surface detail

Scroll up over a visible globe to enlarge it; scroll down to pull back. Earth
and Moon have independent scales from **0.35× to 8×** their normal canvas radius.
Zoomed globes may extend beyond their canvas, so keep the pointer on the visible
surface when scrolling out again. A grab uses the displayed radius throughout;
wheel events during a held drag do not change its sensitivity. Zoom is session
state and does not change `wallpaper_mode` or write new configuration values.

Earth starts with a surface map bounded to 4096×2048, and Moon to 2048×1024.
Once the body scale
exceeds **1.3×**, a bounded worker can load additional
detail: the bundled Earth map has **5400×2700** native pixels and the optional
Moon detail map has **8192×4096**. The existing map remains visible while that
one-time request completes. Decoding and upload obey the GPU's reported texture
limit, never enlarge a smaller source image, and retain the base map if the
detail image is missing, invalid or cannot be uploaded. This is progressive
image resolution, not an unlimited streaming terrain system.

An explicit Earth or Moon texture path remains authoritative at every zoom
level. JWM reads additional detail from that same source when available; it does
not replace a custom map with a bundled NASA close-up. Bundled detail is likewise
not substituted for a user data override. High-resolution images use mipmaps
and stable longitude/pole gradients. The surface shader reveals restrained
artistic material grain as the body grows; it does not infer measured elevation
or create a displaced terrain mesh.

Scroll over empty sky to narrow or widen the panorama's perspective lens,
independently of globe scale. Astronomy starts at a **60° vertical field of
view**, bounded to **8°–110°**. A narrow lens reveals catalogue stars and finer
nebula artwork; scrolling back out restores a wider view. Sky orientation and
lens are shared across the Astronomy canvases for this session. Neither sky
zoom nor body zoom changes the current UTC Sun direction or lunar phase.

## Fixed geographic view and real daylight

Every Earth canvas now defaults to `wallpaper_earth_view = "fixed"`. Latitude
23.13°N and longitude 113.28°E frame Guangzhou, using the city-level point from
[UNGEGN / China Institute of Toponymy](https://ungegn.un.org/dashboard/cities/details?id=2049).
This is not a precise home/device location. Configure latitude within
−90..90 and longitude within −180..180 (finite degrees, east positive). The
camera and land stay fixed throughout the day unless manually dragged; UTC,
date and Earth's seasonal tilt move the sunlight and day/night boundary
independently. All three layouts use the same policy. Reload/hot updates change
framing without decoding textures.

The explicit `"rotating"` view retains accelerated camera tours:
`wallpaper_earth_rotation_minutes = 1440` makes one turn per day, or use 20 for
a twenty-minute tour. Existing period values remain stored, but do not move a
fixed camera. Latitude, longitude and roll define the initial camera; the axis
defines its full circular tour. Absolute Unix UTC time drives the phase,
independently of timezone. Changing the period or axis may reposition the tour
immediately and clears that body's manual offset. With no manual interaction,
a restart at the same instant restores the same orientation.
Earth's Sun direction stays in geographic coordinates while the camera moves,
so the same geographic location keeps its real day/night state in every view.
An accelerated tour never accelerates the physical Sun.

Moon defaults to 39343 minutes (about 27.3 days), rounded from NASA's
[655.720-hour sidereal rotation period](https://nssdc.gsfc.nasa.gov/planetary/factsheet/moonfact.html).
Moon's default view remains `"rotating"`; choosing `"fixed"` preserves a selected
texture view while keeping the live phase. Current lunar illumination always
follows the actual UTC instant, independently of decorative texture rotation,
axis, latitude, longitude or roll. The terminator progresses smoothly
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
The stable horizontal phase presentation does not model Guangzhou's changing
local-horizon tilt or the celestial bright-limb angle. JWM transforms the lunar
light and surface normal through the same orientation, so the current illuminated
fraction and waxing/waning side remain stable while the texture turns in any
direction. This is an artistic whole-texture presentation, not the phase an
observer would see while physically flying around the Moon.

### Current local time, timezone changes and resume

The system clock supplies one current instant shared by both bodies and every
output. Local time is another representation of that instant: for example,
12:00 in UTC+8 is evaluated as 04:00 UTC on the same date. JWM does not add the
timezone offset again to the Sun or Moon calculation. Changing the displayed
timezone or crossing a DST boundary therefore keeps the same physical
illumination for the same instant. Choosing a different geographic Earth center
changes which part of that correctly lit globe is visible.

Resume restores the current real phase and tour with any retained session
orientation; restart restores the configured orientation. Neither accumulates
animation drift or replays the time spent asleep. Timezone/DST
changes request a repaint but never rotate Earth or move the Sun.
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
catalogue-based galaxy backdrop uses decorative camera framing. It is a distant
celestial sphere, independent of the observer's geographic location.

Regular animation ticks adapt between 1 and 15 Hz. Only visible bodies whose
views are rotating and are not currently held contribute their decorative speed
to the cadence; fixed Earth and Moon still update real illumination at least once per second while
active. Enabled prepare checks the current
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
automatic animation updates; pointer-driven poses use the separate 60 Hz cap.
Other windows may still require additional repaints. No GPU power
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
The optional `moon-nasa-lroc-8k.jpg` supplies 8192×4096 close-up detail from
the same NASA SVS 2025 LROC/LOLA map family. Its derivation, checksum and source
are recorded in the [asset attribution](../assets/astronomy/README.md#moon-close-up-detail).
The optional cloud mask is NASA Goddard / Reto Stöckli’s 2002 Blue Marble
composite, bundled unchanged at 2048×1024 as `earth-nasa-clouds.jpg`. It uses
one extra texture sample on Earth (none on Moon), with a maximum 8 MiB RGBA
upload. If it is missing or fails to load, both surface materials still work.

The shared sky now prefers `galaxy-nasa-4k.png`, a 4096×2048 atlas derived from
[NASA SVS's full star maps](https://svs.gsfc.nasa.gov/4851/), including
Gaia, Hipparcos and Tycho stars. The fourth channel carries faint constellation
figures as data, so catalogue and figure overlay use one RGBA texture lookup.
These figures are conventional patterns, not official constellation boundaries.
The prior `milkyway-nasa-gaia.jpg` remains installed as a compatibility fallback.
The new atlas is GPU-size bounded and optional; a procedural sky keeps navigation
available if no sky map can be loaded. Four fixed-direction procedural nebula
landmarks add artistic structure around sourced celestial directions; see the
[coordinate and rendering notes](celestial-explorer.md#sky-catalogue-and-nebulae).

The renderer uses linear-light surface shading, a restrained ocean glint and
thin Earth limb, and an airless lunar regolith lighting approximation. Surface
grain, atmosphere and nebula appearance are visual models. They do not supply
measured terrain displacement, a calibrated atmosphere or a volume of gas that
the camera can fly through.

Full source and media-use attribution accompany all bundled images in
`assets/astronomy/README.md`. Moon loading is also offline; if either required
texture fails in Earth/Moon mode, the static wallpaper remains visible.

## Validation and limits

The shader regression script compiles, links and renders the actual fragment
shader in Mesa surfaceless EGL on desktop GLSL 330 and GLES 300. It compares
the corresponding RGBA frames and checks their opacity. Coverage includes
dated Guangzhou UTC+8 00/06/12/18 views, fixed-camera controls with a frozen Sun, seasonal northern/
southern/polar views, explicit rotating Earth, legacy controls, and
5120×1440 single-globe previews. Landmarks/hemisphere orientation
were visually inspected. These are real shader tests, not compositor runtime
or NVIDIA driver verification. CPU tests cover clock cap/pause/resume, clipping,
union coverage, fixed/rotating view separation, NOAA-reference solar positions,
approximate sunrise/sunset, polar transitions, civil-date boundaries and invalid
clocks. The sunrise/sunset helper is test/diagnostic-only, not extra work in each
render frame.

Pointer regressions additionally cover surface tracking in all drag directions,
rolled and polar starting poses, off-globe motion, same-endpoint event coalescing,
long holds, release rebasing after clock corrections, and 20,000 successive
gestures without scale/shear drift. Renderer tests cover visible-circle hit
testing, clipped/overlapping outputs, radius-normalized coordinates, independent
body offsets, wheel limits and displayed-radius picking, lazy detail loading,
panorama capture through both poles, final-motion consumption and retirement
of interaction wakeups.
The GL/GLES script feeds the production trackball into the actual shader and
checks geographic landmarks, current sunlight, lunar phase contours and texture
gradients. WM tests cover desktop admission, body versus sky routing, wheel
ownership, cross-output capture and pointer-grab ownership separately. Native
XCB/x11rb tests retain out-and-back motion through bounded event batches so a
Solar System drag cannot be mistaken for a planet click.

Config/picker regressions and X11 integration run through the Cargo workspace
with its locked dependencies. Software rendering exercises shader and framebuffer
behavior; physical DPMS transitions, live blur/damage, different GPU drivers and
multi-output deployment also need verification in a running desktop session.

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
compositor binary. With the repository's native build dependencies and Mesa EGL
installed, run the full config/picker and renderer regressions with:

```sh
EGL_PLATFORM=surfaceless LIBGL_ALWAYS_SOFTWARE=1 JWM_REQUIRE_HEADLESS_GL=1 \
  scripts/test.sh --lib --bins --tests
```

The wrapper already supplies `--locked`; append `--offline` only if all locked
dependencies are cached. The suite includes real shell and IPC tests, so it
requires a test environment that allows their local sockets and child processes.

## Automatic sky tour and 360° navigation

`wallpaper_sky_rotation_minutes = 1440` is the default: one calm panoramic circuit
per 24 hours. Set it to `0` to stop the automatic tour, `720` for a 12-hour circuit,
or `60` for a faster one-hour preview. Manual sky dragging and wheel zoom work
with every value, including zero. The range is integer `0..=525600`; normal config
reload applies it without texture reload. Changing the period resets the manual
sky orientation and lens. This setting controls the Astronomy background only;
it does not change Earth framing, solar lighting, lunar phase, either body's
rotation period or the Solar System camera.

The automatic angle uses absolute Unix UTC time, reduced to one turn in double
precision. With no manual offset, restarting at the same instant reproduces the
same orientation; display timezone and DST have no effect. Sleep and occlusion
pause rendering, then resume at the current UTC orientation with the retained
session offset. Holding the sky pauses its decorative tour at the displayed
orientation; release resumes smoothly from the chosen direction. Earth and Moon
continue their separate illumination updates throughout.

A perspective ray samples the distant celestial sphere. Full-axis camera
orientation can pass through both poles and wrap around longitude without
clamping latitude, stretching a screen-sized image or leaving a flat backdrop
behind the globe. Catalogue stars, constellation figures and nebula landmarks
use that same direction. If the catalogue is unavailable, fallback stars and dust
come from a seamless spatial field; the nebula artwork is fixed in space rather
than animated noise. The catalogue and its constellation channel share one
lookup per visible background pixel. Nebula detail uses a bounded number of
noise layers and fades unresolved frequencies at wide views.

Automatic frames retain the existing maximum 15 Hz schedule and power/occlusion
gates; interactive sky changes use the separate 60 Hz cap. There is no runtime
asset download or decoding on each pointer event. Setting the tour to zero keeps
real-time Earth sunlight and lunar phase updates active.
