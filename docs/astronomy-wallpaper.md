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
wallpaper_earth_texture = "/absolute/path/to/earth-nasa-january.jpg"
```

Use the existing Wallpaper picker to select Astronomy Earth, or select a normal
image to return to static wallpaper. Existing `wallpaper`, per-monitor/tag
choices and `wallpaper_rotation = "daily"` remain the fallback collection.
No network fetch occurs inside JWM. Install a 2:1 equirectangular daytime Earth
map first. Changing either field uses the normal config reload path.

One globe is centered on the X framebuffer, spanning adjacent monitors rather
than repeating on each monitor. Coordinates use framebuffer pixels, not logical
monitor origins. This is deliberately an initial global-wallpaper mode.

## Time and appearance

UTC drives an approximate astronomical Sun direction, including seasonal
solar declination and equation of time. The system timezone provides only a
coarse starting viewing meridian; it is not a GPS location. The view completes
one slow orbit per UTC day. This is an original presentation choice, not a
claim to reproduce Apple's implementation. The night side is intentionally
dark; there are no live clouds, weather or city-light observations.

Animation samples UTC/timezone at at most 15 Hz, even if other clients repaint
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

The preview uses NASA's Blue Marble Next Generation January 2004 monthly
surface composite, not real-time satellite imagery. Source: NASA Earth
Observatory, [Blue Marble Next Generation base map](https://science.nasa.gov/earth/earth-observatory/blue-marble-next-generation/base-map/).
[Original 5400×2700 JPEG](https://assets.science.nasa.gov/content/dam/science/esd/eo/images/bmng/bmng-base/january/world.200401.3x5400x2700.jpg).
SHA256: `99f5faad74efe985fbf1714c8be7296ca9999759a1215b65f99b7f1df278dde5`.

NASA source material is acknowledged under its [images and media guidelines](https://www.nasa.gov/nasa-brand-center/images-and-media/).
This visualization is independently rendered; NASA has not reviewed or endorsed
JWM. No NASA logo is included. The image is a separately installed asset, not a
new binary embedded in JWM or a live download dependency.

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
