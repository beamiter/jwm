# Interactive Solar System wallpaper

The optional `behavior.wallpaper_effect = "solar-system"` effect is available in
Wallpaper → Solar System 3D on the X11 compositor (xcb and x11rb). It does not
change the default or existing Astronomy Earth/Moon settings. Selecting a static
image restores ordinary wallpaper behavior.

The [celestial explorer guide](celestial-explorer.md) covers the shared Earth,
Moon and Solar System controls. All gestures start on uncovered wallpaper with
no modifier key. Windows, bars, tabs, modal panels, the lock screen and configured
mouse bindings keep their normal priority.

## Explore the system and individual bodies

| Gesture | Result |
| --- | --- |
| Left-click the Sun or any of the eight planets | Open a close-up that follows that body along its orbit |
| Left-drag a visible body in the overview | Orbit the whole system in any direction, including roll and passage through either pole |
| Left-drag a visible body in a close-up | Orbit the focused body in any direction |
| Left-drag empty sky | Turn the camera's viewing direction through the full celestial sphere |
| Wheel in the overview, or over empty sky in a close-up | Narrow or widen the perspective lens |
| Wheel over the focused body's visible surface | Move the camera closer to or farther from that body |
| Right-click any uncovered wallpaper in a close-up | Restore the saved overview camera, orientation and lens |

Saturn's visible rings select Saturn. Picking uses the nearest displayed sphere
or visible ring, including occlusion by foreground objects. A click opens the
body only when the left button is naturally released and the pointer never
travels more than four pixels from its initial position. A drag that moves out
and back remains a drag. Clicking empty sky does not select a body. There is no
global Escape binding; application key handling stays intact.

Both body and sky gestures capture the pointer and retain the projection visible
at the initial press. They can continue across windows and onto another output
without retargeting or changing sensitivity. Release includes the final event
position. Pointer loss, modal capture, locking, removed/resized outputs and an
unavailable renderer cancel the gesture without opening a body. Wheel events
during a held drag do not change its reference geometry.

Body dragging uses the same rigid quaternion trackball as Earth and Moon, so
the new interaction has no elevation clamp. Empty-sky dragging changes the view
from the current camera position: planets and distant stars move coherently
through the same perspective. It can point away from the focused planet; drag
back, or right-click the wallpaper to return to the saved overview. Right-click
also works when a close-up fills the canvas with a planet.

The shared panorama lens control defaults to 60° and is bounded to 8°–110°.
Solar applies its tangent ratio to the existing aspect-aware focal length, so
these control values are not the exact vertical field of view for every screen
shape. Close-up surface scrolling instead changes distance, bounded between
1.12 and 40 times the focused body's radius, keeping the camera outside its
sphere. A fresh selection approaches the illuminated hemisphere and gives
Saturn extra room for its rings. Close-up navigation tracks orbital position;
it does not freeze the body's rotation or move the Sun.

Camera poses, lens, current selection and the saved overview last only in the
running session. They are not written to configuration. No new setting is
needed to enable these controls.

## Scene and surface detail

This is a stylized 3D illustration: the Sun and eight planets have
exaggerated sizes, orbital distances and speeds for readability, rather than
scientific scale or real ephemerides. Perspective, sphere occlusion, sun-facing
lighting and Saturn's ring depth are computed in 3D. The close-up uses those same
world-space spheres and material functions, with a new camera position. Earth
uses the bundled NASA October surface and historical MODIS cloud composite.
The sky uses the shared 4K NASA full-star atlas, with faint constellation figures
and four procedural nebula landmarks. Its directions remain attached to a distant
celestial sphere as the camera turns. This provides a foundation for spatial
exploration; free interstellar translation, volumetric nebula flight and
gravitational N-body dynamics are not implemented. Source data and derivations
are recorded in the [asset attribution](../assets/astronomy/README.md), and the
[explorer guide](celestial-explorer.md#sky-catalogue-and-nebulae) separates sourced
directions from artistic nebula shapes.

Earth and its cloud image initially load off-thread at no more than 2048×1024.
Entering an Earth close-up requests the surface at up to its native 5400×2700,
bounded by the GPU texture limit. The base map remains visible until additional
detail is ready; a failed detail load keeps that base. The sky atlas retains up
to 4096×2048 where supported. Successful loads are adopted only on a new full
material frame, preserving scissored repairs. Missing/invalid optional imagery
falls back independently to procedural materials. The old sky JPEG remains a
compatibility fallback.

The sky's RGB catalogue and constellation data share one RGBA lookup per visible
background pixel, with sRGB decoding before the Solar tone mapper. Figures are
subtle in wide views and fade further at close magnification. Nebula colours,
shapes and apparent sizes are original artwork around sourced directions; they
are not telescope images or measured volumes of gas. The Earth cloud luminance is a
visual opacity mask in the same body-fixed coordinates as its surface. Venus has
an opaque pale cloud deck; Jupiter has unequal, sheared cloud zones and a curled
storm; Saturn has softer, low-contrast bands. These other clouds are original,
fixed-cost procedural illustrations, not measured atmospheric data. Approaching
a surface reveals filtered, body-attached grain, rock/crater patterns and finer
cloud structure where appropriate. These spatial patterns stay attached while
the body spins. The additional layers fade when their features become smaller
than a pixel, reducing shimmer while zooming or orbiting. They do not turn a
colour map into a measured height field or a terrain mesh.

Image materials use mipmaps and trilinear minification, with analytic texture
gradients that remain stable at longitude seams and near the poles. This reduces
moiré and flickering when detailed maps shrink onto a distant planet. Mipmaps
are generated once when the material is adopted and add approximately one-third
to its texture storage. A failed mipmap generation keeps the base image and
falls back to linear sampling. The full-resolution scene cache is unchanged.

## Rotation and time

Each planet rotates independently as a rigid body around its own tilted axis.
Sidereal spin periods and obliquities follow the
[NASA GSFC planetary fact sheets](https://nssdc.gsfc.nasa.gov/planetary/factsheet/).
Venus and Uranus rotate retrograde; their full obliquities encode that direction
once. Saturn’s rings share its equatorial plane. Surface patterns, Earth’s map,
clouds and storms stay attached to each body, while sunlight and shadows remain
in the world frame. Axis azimuths, starting longitudes and positions are
illustrative, not a date-specific ephemeris or IAU pole-orientation solution.

`behavior.wallpaper_solar_spin_speed` controls only these eight self-rotations:

- `718.035` (default): Earth completes a sidereal turn in 120 displayed seconds.
- `1.0`: real-time sidereal rotation; Earth takes 23.9345 hours, not the 24-hour solar day.
- `0.0`: pause planetary self-rotation, while the illustrative orbits continue.

The finite range is 0 through 10000. Every planet uses the same time multiplier,
so Mercury and Venus visibly turn much more slowly than the gas giants. Changing
speed preserves the current surface longitude. Spin pauses while the effect is
inactive, fully covered or unpowered and resumes from its held phase. Restarting
the application resets illustrative phases. Orbital speeds remain separately
compressed for visibility, so orbit-to-spin ratios are not astronomical. Orbits run prograde around the
scene’s north axis, with the original starting positions preserved. These
controls never alter the existing Astronomy Earth/Moon rotation settings. Solar
orbits and phases are illustrative session animation. The separate Astronomy
effect still derives Earth's daylight and the Moon's phase from the real UTC
instant; zooming or turning either camera does not add a timezone offset.

## Rendering cost and validation

The wallpaper uses a fixed number of analytic intersections, never unbounded ray
marching. Planet and ring material pixels and silhouette edges receive four
subpixel rays, while smooth sky and corona use one ray; soft orbit guides have
analytic coverage. A tone-mapped warm corona and translucent, shadowed Saturn
rings keep the planets visually distinct. Idle animation is capped at 15 frames
per second and pauses when fully covered or displays are asleep. Pointer-driven
camera and lens updates have a separate coalesced 60 Hz rendering cap and
invalidate the matching backdrop. Holding still does not keep the fast rendering
deadline active; the pointer watchdog continues to protect the grab. Native
left-button motion samples are retained to classify clicks correctly and are
processed in bounded batches, so high polling rates still affect CPU input work.
Power state is checked at the existing two-second interval.
Failure to compile the material retains the static fallback and stops animation
wakes until the effect is changed.

Run `python3 scripts/test-solar-system-shader.py` to compile and render the actual
shader on surfaceless Mesa GL330 and GLES300. It uses production Rust camera and
phase helpers, verifies actual close-up cameras for the Sun and every planet,
CPU/GPU picking agreement, full-axis views, panorama lens limits, filtered
surface detail, orbital/surface wrap continuity, ring/Sun alignment and
scissored repair. PNGs and metrics are saved under `/tmp/jwm-solar-preview`.
Set `SOLAR_SPIN_VIDEO=1` for a fixed-camera, fixed-orbit self-rotation preview
(three displayed seconds per saved frame), or `PREVIEW_WIDTH=540 PREVIEW_HEIGHT=960` for portrait coverage, or `SOLAR_VIDEO=1`
to write ninety orbit/zoom preview frames. `SOLAR_NO_EARTH=1`, `SOLAR_NO_CLOUD=1`, and `SOLAR_NO_SKY=1` independently verify
optional-image fallbacks. The saved `focus-*` and `near-*` images use the real
close-up cameras. The additional `material-closeups` sheet places the production
material functions on diagnostic spheres for comparison. `SOLAR_BENCH_ONLY=1`
records six synchronized full-frame draw timings;
`SOLAR_BENCH_NEAR=1` selects the close camera benchmark. Software Mesa results
are not hardware frame-rate claims. A dedicated GitHub workflow runs the
landscape and portrait regressions.
