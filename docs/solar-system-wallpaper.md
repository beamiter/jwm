# Interactive Solar System wallpaper

The optional `behavior.wallpaper_effect = "solar-system"` effect is available in
Wallpaper → Solar System 3D on the X11 compositor (xcb and x11rb). It does not
change the default or existing Astronomy Earth/Moon settings. Selecting a static
image restores ordinary wallpaper behavior.

On uncovered desktop, hold the left mouse button and drag to orbit the camera.
Scroll to zoom. Windows and modal panels keep their normal pointer behavior.
The camera is session-local; selecting another effect does not save camera values
to configuration. Camera elevation and zoom are bounded, with no pole singularity.

This is a stylized 3D illustration: the Sun and eight planets have
exaggerated sizes, orbital distances and speeds for readability, rather than
scientific scale or real ephemerides. Perspective, sphere occlusion, sun-facing
lighting and Saturn's ring depth are computed in 3D. The Earth reuses the already bundled NASA October map with its
existing attribution; missing or failed optional Earth imagery falls back to a
procedural material. Other surfaces are original procedural materials, including
layered gas belts and clouds. No new third-party assets are introduced.

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
Set `PREVIEW_WIDTH=540 PREVIEW_HEIGHT=960` for portrait coverage, or `SOLAR_VIDEO=1`
to write ninety orbit/zoom preview frames. `SOLAR_NO_EARTH=1` verifies the procedural
fallback. `SOLAR_BENCH_ONLY=1` records six synchronized full-frame draw timings;
software Mesa results are not hardware frame-rate claims. A dedicated GitHub workflow runs the
landscape and portrait regressions.
