# Interactive Solar System wallpaper

The optional `behavior.wallpaper_effect = "solar-system"` effect is available in
Wallpaper → Solar System 3D on the X11 compositor (xcb and x11rb). It does not
change the default or existing Astronomy Earth/Moon settings. Selecting a static
image restores ordinary wallpaper behavior.

On uncovered desktop, hold the left mouse button and drag to orbit the camera.
Scroll to zoom. Drag sensitivity uses the starting output’s shorter edge; crossing
onto another output ends the drag and requires a fresh press. Windows and modal panels keep their normal pointer behavior.
The camera is session-local; selecting another effect does not save camera values
to configuration. Camera elevation and zoom are bounded, with no pole singularity.

This is a stylized 3D illustration: the Sun and eight planets have
exaggerated sizes, orbital distances and speeds for readability, rather than
scientific scale or real ephemerides. Perspective, sphere occlusion, sun-facing
lighting and Saturn's ring depth are computed in 3D. Earth reuses the bundled NASA
October surface and historical MODIS cloud composite. The background reuses the
NASA SVS / ESA Gaia stellar-density panorama, including its dark dust lanes. It is
a fixed decorative celestial orientation, not a nearby nebula simulation, live
sky, current weather, or date-specific astronomical view. Existing attribution
is retained in `assets/astronomy/README.md`; no new third-party assets are added.

Each of these optional textures loads off-thread at no more than 2048 × 1024.
Successful loads are adopted only on a new full material frame, preserving
scissored repairs; missing/invalid imagery falls back independently to procedural
materials. The sky uses one texture lookup only for visible background pixels,
with sRGB decoding before the Solar tone mapper. The Earth cloud luminance is a
visual opacity mask in the same body-fixed coordinates as its surface. Venus has
an opaque pale cloud deck; Jupiter has unequal, sheared cloud zones and a curled
storm; Saturn has softer, low-contrast bands. These other clouds are original,
fixed-cost procedural illustrations, not measured atmospheric data.

Each planet now rotates independently as a rigid body around its own tilted axis.
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
controls never alter the existing Astronomy Earth/Moon rotation settings.

The wallpaper uses a fixed number of analytic intersections, never unbounded ray
marching. Planet and ring material pixels and silhouette edges receive four
subpixel rays, while smooth sky and corona use one ray; soft orbit guides have
analytic coverage. A tone-mapped warm corona and translucent, shadowed Saturn
rings keep the planets visually distinct. Idle animation is capped at 15 frames per second and pauses when fully
covered or displays are asleep. Pointer changes request immediate repaint and
backdrop invalidation. Power state is checked at the existing two-second interval.
Failure to compile the material retains the static fallback and stops animation
wakes until the effect is changed.

Run `python3 scripts/test-solar-system-shader.py` to compile and render the actual
shader on surfaceless Mesa GL330 and GLES300. It uses production Rust camera and
phase helpers, verifies camera extremes, orbital/surface wrap continuity, ring/Sun
alignment and
scissored repair, and saves PNGs plus metrics under `/tmp/jwm-solar-preview`.
Set `SOLAR_SPIN_VIDEO=1` for a fixed-camera, fixed-orbit self-rotation preview
(three displayed seconds per saved frame), or `PREVIEW_WIDTH=540 PREVIEW_HEIGHT=960` for portrait coverage, or `SOLAR_VIDEO=1`
to write ninety orbit/zoom preview frames. `SOLAR_NO_EARTH=1`, `SOLAR_NO_CLOUD=1`, and `SOLAR_NO_SKY=1` independently verify
optional-image fallbacks. The saved `material-closeups` images show the actual
production material functions on diagnostic spheres, not alternate camera views. `SOLAR_BENCH_ONLY=1` records six synchronized full-frame draw timings;
`SOLAR_BENCH_NEAR=1` selects the close camera benchmark. Software Mesa results
are not hardware frame-rate claims. A dedicated GitHub workflow runs the
landscape and portrait regressions.
