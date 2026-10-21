# Celestial explorer: bodies, close-ups and the whole sky

JWM's X11 wallpapers provide twelve selectable scenes: the Sun, eight planets,
Moon, a 3D Solar System and a Milky Way panorama. Turn a globe with the mouse,
zoom into its surface, or drag empty sky to look around a complete celestial
sphere. In Solar System, click the Sun or a planet for an isolated close-up
that keeps following it as the scene animates.

These controls run inside the existing compositor background, below ordinary
windows. They work on both xcb and x11rb without a new desktop window or an
additional enable setting. Native Wayland keeps the existing static fallback.

## Start exploring

Open the Wallpaper picker with `Alt+Ctrl+W`, choose **Apply to: All screens**
or a connected display, then select a body, Solar System 3D or the Milky Way.
**Span** joins the desktop, **Mirror** repeats one view, and **Independent**
allows a theme per connector or a named shared group. The picker saves these
choices. Reference facts, material notes and source URLs are in
[Astronomy science](astronomy-science.md).
The common configuration uses `wallpaper_effect = "astronomy"` with
`wallpaper_astronomy_scene`, `wallpaper_astronomy_layout` and optional
`wallpaper_astronomy_outputs`. The earlier `wallpaper_effect = "solar-system"`
and `wallpaper_astronomy_layout = "earth-moon"` configurations remain supported.
The full settings and examples are in
[Astronomy wallpaper](astronomy-wallpaper.md),
[Solar System wallpaper](solar-system-wallpaper.md) and
[wallpaper.toml](examples/wallpaper.toml).

Start with a visible patch of desktop. A window, bar, modal panel or configured
mouse binding receives the input before the wallpaper. Use the plain left or
right button and the ordinary vertical wheel; no modifier is needed.

| Where the pointer starts | Action | What changes |
| --- | --- | --- |
| Body surface in a single-body Astronomy scene | Hold left and drag | The selected globe turns in all directions; edge drags also roll it |
| Body surface in a single-body Astronomy scene | Wheel up/down | That body's size increases/decreases from 0.35× to 8× |
| Empty sky in any celestial effect | Hold left and drag | The viewing direction turns through 360°, including both poles |
| Empty sky | Wheel up/down | The perspective lens narrows/widens |
| Nebula/galaxy image or its blue locator ring in Milky Way | Left-click | Open the observed telescope field |
| Focused Milky Way target | Wheel up/down | Smoothly approach/retreat; scrolling out fully returns to the overview |
| Focused Milky Way target | Hold left and drag | Pan around the target field |
| Milky Way close-up | Hold left and drag | Orbit the 3D nebula and see depth, emission and dust occlusion |
| Milky Way close-up | Right-click | Restore the original sky orientation and perspective lens |
| Sun or planet in Solar System | Left-click | A close-up opens for the selected body |
| Visible body in Solar System | Hold left and drag | The camera orbits the overview or current close-up target in all directions |
| Focused body in Solar System | Wheel up/down | The camera approaches/recedes from that body |
| Any uncovered wallpaper during a Solar close-up | Right-click | The saved overview returns, including its camera orientation and lens |
| Any uncovered Astronomy canvas | Right-click | The pointed scene or shared group's interactive view resets |

Each Independent output has its own body pose, zoom, sky orientation and lens,
even when two outputs show the same planet. Explicit shared groups and Mirror
copies share navigation state; Span uses one desktop canvas. The legacy
`per-output` and `earth-moon` layouts retain their previous sharing rules.
Solar System follows the same connector/group assignment and owns its camera
and focused-body state per scene. Orbital time and presentation settings are
global, so independent navigation does not give each screen a different date.

## A short tour

1. In Astronomy, place the pointer on Earth and scroll up a few steps. Keep the
   pointer over the body when changing its scale; scrolling over sky changes
   the sky lens instead. Earth can grow beyond the edges of its canvas.
2. Drag across the globe to inspect another hemisphere. An edge drag rolls the
   view. Release and grab again to keep rotating through either pole. Repeat
   on Moon to inspect the lunar map at a larger scale.
3. Place the pointer on empty sky and drag horizontally, vertically or
   diagonally. You are changing a perspective viewing direction, so the
   catalogue, constellation figures and nebula landmarks turn together.
   Scroll over sky to inspect a smaller field, then scroll down to widen it.
4. Switch to Solar System 3D. Click the Sun or any visible planet; Saturn's
   visible rings also select Saturn. The selection approaches the illuminated
   hemisphere and follows the body along the selected orbital model. The
   default uses date-driven JPL approximate positions. The close-up isolates
   the selected body, its rings where present, and the sky so enlarged
   neighbouring planets cannot obscure the inspection.
5. Drag a body in the close-up to orbit it, or wheel over the focused surface
   to approach. Surface patterns stay attached as the body turns. Dragging
   empty sky instead turns the camera's gaze from its current position, so the
   planet can leave the view while the rest of the sky comes into view.
6. Right-click any uncovered part of the wallpaper to return to the overview
   you left. This also works when a zoomed planet covers the entire canvas.

The initial press determines the gesture. Pointer capture keeps it attached to
that view even if you pass over a window or another output. The last release
position is included. A Solar click requires a natural left-button release and
at most four pixels of movement from the press; moving farther and then returning
does not open a planet. Wheel and secondary-button events during a held drag do
not retarget the gesture. Locking, modal capture, loss of the native pointer grab
or removal of the starting output ends it safely. There is no global Escape
shortcut for navigation.

All manual poses, zoom, lens and focus state are session-only. They do not
rewrite configuration. Changing an Astronomy body's configured orientation,
view mode or rotation period resets its manual pose and zoom; changing the
Astronomy sky-tour period resets the manual sky view. Restart restores configured
views and default navigation state.

## Zoom, maps and detail

Body magnification and sky perspective are separate controls. Astronomy's
panorama starts at a 60° vertical field of view and accepts 8°–110°. Solar uses
the same lens control range as a relative tangent scale on its existing
aspect-aware focal length; its exact vertical angle depends on the viewport.
In a Solar close-up, scrolling on the focused body moves the camera instead,
bounded to 1.12–40 body radii so it stays outside the sphere.

All animated wallpapers use three-dimensional geometry: the bodies and Solar
System retain their sphere/ring models, and Milky Way uses a spatial disc/dust
model, a distant star-catalogue skybox and twelve nebula volumes. Textures supply
surface colour and reference morphology; the camera moves through a 3D scene.

Milky Way's overview lens extends to **2°**. Locators grow with perspective
magnification, capped at 42% of canvas height; their hit areas grow with them.
Wheel zoom stays anchored to the cursor. At maximum zoom over a target, or on
clicking its locator, a **1.4-second** eased camera flight blends into that target's
volume. The flight interpolates orientation and logarithmic field of view while
preserving apparent target size. Right-click starts a **0.9-second** reverse
flight to the saved overview. A wheel-up during return reverses the flight
from the current frame, preserving the focused camera.

In the nebula view, hold left and drag to orbit freely, including the back and
poles. Wheel motion dollies the camera between **0.7 and 30 model half-heights**;
the near range allows looking through the gas. Wheel steps accumulate into a
bounded **180 ms** logarithmic dolly, without a jump on each detent. Grabbing
during a flight pauses the approach at the presented frame; release resumes it.
The finite **32-sample** volume pass integrates emission and dust absorption,
with continuous 3D turbulence, embedded light sources and cloud, dust-pillar,
shock-filament, shell or spiral-disc morphology. Each output/group retains its
own camera and transition. Covered or suspended scenes retire interaction wakeups; a still
close-up does not keep automatic sky touring active.

The renderer loads modest body images first, then requests larger maps when
they can improve a close-up. This work uses the bounded background decoder;
pointer motion does not perform file I/O or decode a new image each frame.

| Material | Initial image bound | Additional detail |
| --- | --- | --- |
| Astronomy Earth | 4096×2048 | The bundled October map's native 5400×2700, requested above 1.3× body scale |
| Astronomy Moon | 2048×1024 | Optional 8192×4096 LROC map, requested above 1.3× body scale |
| Solar Earth | 2048×1024 | Up to native 5400×2700 when an Earth close-up opens |
| Shared sky atlas | Up to 4096×2048 | Mip levels select catalogue detail appropriate to the lens and screen footprint |

The GPU's reported texture limit can reduce these sizes. A pending or failed
optional detail request leaves the current base image visible. A smaller source
is not enlarged to pretend it has more detail. Explicit custom Earth/Moon paths
remain authoritative when magnified; user data overrides are preserved too.
An optional user Moon 8K map can accompany a user base map, while a base-only
override keeps its own content. All images are installed data; JWM downloads
nothing while running. Sources, checksums and reproduction scripts are listed
in the [asset attribution](../assets/astronomy/README.md).

High-resolution images use more GPU memory. An 8192×4096 RGBA image alone is
128 MiB before its mip chain; a complete chain adds approximately one third.
Detail is loaded only when needed, retained for reuse and released with the
material. Mipmaps and analytic longitude/pole gradients reduce shimmer when
fine image features shrink on screen. Filtered artistic grain, crater-like
patterns and cloud structure add material detail where appropriate; they are
not new measured geography or a terrain mesh. Magnifying a finite image cannot
reveal unlimited real-world detail.

## Sky catalogue and nebulae

The shared `galaxy-nasa-4k.png` comes from
[NASA SVS's 2020 star maps](https://svs.gsfc.nasa.gov/4851/): Gaia's faint stars
are combined with the Hipparcos and Tycho foreground. Faint constellation figures
help connect familiar patterns across the whole sky. Those conventional figures
are distinct from official constellation boundaries. The catalogue and figures
share a galactic projection; the shader reads them with the same direction and
one RGBA lookup per visible background pixel. The fourth channel stores figure
data, not transparency. Figures fade further when the lens narrows so they do
not overwhelm close views of the star field.

The prior `milkyway-nasa-gaia.jpg` remains available for compatibility and user
overrides. The Milky Way scene adds twelve observed telescope fields in both
presentation modes. Each field uses its published J2000 right ascension,
declination, angular width/height and north angle, plus an approximate distance,
to place the centre and projected extent of a 3D volume in light-years. Physical
width is derived from distance and angular size. Images retain their observed extent in the sky;
small blue rings make even subpixel fields clickable from the overview.

| Observed field | Approximate distance (ly) | Image/coordinate source |
| --- | ---: | --- |
| Orion Nebula / M42 | 1,400 | [Source](https://esahubble.org/images/heic0601a/) |
| Lagoon Nebula / M8 | 4,000 | [Source](https://esahubble.org/images/heic1808a/) |
| Carina Nebula / NGC 3372 | 7,500 | [Source](https://esahubble.org/images/heic0707a/) |
| Eagle Nebula / M16 | 7,000 | [Source](https://esahubble.org/images/heic1501a/) |
| Andromeda Galaxy / M31 | 2,500,000 | [Source](https://esahubble.org/images/heic1502a/) |
| Triangulum Galaxy / M33 | 3,000,000 | [Source](https://esahubble.org/images/heic1901a/) |
| Rosette Nebula / NGC 2237 | 4,700 | [Source](https://www.eso.org/public/images/potw1847a/) |
| Veil Nebula / NGC 6960 | 2,100 | [Source](https://esahubble.org/images/heic1520a/) |
| Crab Nebula / M1 | 6,500 | [Source](https://esahubble.org/images/heic0515a/) |
| Helix Nebula / NGC 7293 | 650 | [Source](https://esahubble.org/images/heic0307a/) |
| Horsehead Nebula / Barnard 33 | 1,300 | [Source](https://esahubble.org/images/heic1307a/) |
| Bubble Nebula / NGC 7635 | 8,000 | [Source](https://esahubble.org/images/heic1608a/) |

The complete coordinates, image orientations, distance references and source
checksums are in [deep-sky-catalog.json](../assets/astronomy/deep-sky-catalog.json).
The atlas and its CPU/GPU geometry are generated together by
`scripts/prepare-astronomy-nebulae.py`; no network access occurs at runtime.
Image credits remain attached to each field and are collected in the
[asset attribution](../assets/astronomy/README.md#observed-deep-sky-fields).
These are historical multi-filter observations, including infrared images and
assigned-colour composites; their colours are not a naked-eye simulation.

Click a target or its locator, or zoom fully into the locator, to enter the
3D volume. Use the wheel to dolly and drag to orbit. A right-click restores the
exact prior perspective.
Dragging past the click threshold or cancelling pointer capture never activates
a target. CPU picking uses the same angular pixel footprint as the shader,
including stretched rings near the edge of a wide panorama. Each monitor or
explicitly shared monitor group retains its own focus, lens and overview. The
observed fields replace the former procedural landmark sketches; Horsehead and
Bubble now occupy the previous Pleiades and North America catalogue slots.
Distances are approximate source estimates. The source field constrains
position, orientation and projected extent; images supply broad colour references.
Cloud shape, thickness, turbulence, internal emission/absorption and
spiral/shell depth are illustrative 3D models, not measured gas tomography. The
Milky Way disc is schematic; catalogue stars still have no measured parallax.

## Time, space and rendering limits

Astronomy Earth's day/night boundary and the Moon's phase continue to follow
the actual system instant converted to UTC once. Body dragging, body zoom and
sky navigation do not advance or freeze that physical time. A timezone or DST
change does not add another light offset. The configured decorative body and
sky tours pause while their respective view is held and resume from the released
orientation. `wallpaper_sky_rotation_minutes = 0` stops the automatic Astronomy
sky tour while leaving manual panorama navigation available.

Solar System uses 3D spheres, a perspective camera, intersections, occlusion,
sun-facing lighting and tilted rings. Its default orbital model evaluates
JPL's approximate Keplerian elements at a UTC-derived dynamical date. The
native clock supports pause, 1×, one day per second and date offsets within
the implemented playback range. Distances remain compressed and bodies
enlarged for readable navigation. The separate default spin multiplier is
718.035×; choosing orbital 1× does not reset surface spin to real time.

An optional illustrative orbit tour retains decorative speeds. **Scientific**
presentation reduces invented surface detail and artificial night-side fill;
it does not turn approximate materials into measured terrain. The distant
star sphere supplies view direction rather than stellar distances or parallax.
Free translation between stars, gravitational N-body integration, a DE/SPICE
ephemeris, unrestricted interstellar flight and surface terrain traversal are
not implemented. See [scientific model notes](astronomy-science.md) for orbital
accuracy, source materials and the distinction between reference quantities
and display geometry.

Automatic animation remains capped at 15 Hz, with Astronomy adapting down to
1 Hz for slow views. Pointer-driven rendering is coalesced at up to 60 Hz.
Holding still retires the fast rendering deadline; the pointer watchdog remains
active for capture recovery. Native left-button motion samples are kept to
distinguish a real click from an out-and-back drag, and input is processed in
bounded batches. Very high mouse polling rates can therefore still increase CPU
input work. Fully covered or unpowered views pause animation under the existing
compositor gates.

The [Astronomy](astronomy-wallpaper.md#validation-and-limits) and
[Solar System](solar-system-wallpaper.md#rendering-cost-and-validation) validation
sections describe the production math, native input and GL/GLES regressions.
Headless Mesa checks support correctness and regression comparison; they do
not establish a frame rate, power saving or driver result on a particular GPU.

The deep-sky renderer can be checked independently with
`python3 scripts/test-deep-sky.py`. It compiles the actual GL330/GLES300 shader
and reads scene IDs and camera fitting from Rust. It checks all twelve volumes
from the front, edge-on and rear, a complete 360-degree return, independent
geometry without a source texture, camera dolly, an inside-volume view, growing
locators, transition frames and rejection of targets behind the observer.
Image comparisons exclude the source-credit strip. Render timings and previews
are written alongside the metrics; software timings are not desktop FPS claims.
