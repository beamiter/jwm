# Cube effects

JWM's X11 compositor draws two Compiz-style 3D effects from one shared object:
the Alt+Ctrl+Tab window switcher and the `cube` tag-switch transition. Both are
a lit prism standing on a mirrored floor inside a procedural skydome, so they
read as the same cube seen twice rather than two separate effects. The Wayland
overview now uses the same protocol-free prism camera and regular-polygon
geometry, plus its own GLES implementations of the lit face, cap, mirrored-floor
and skydome materials.

```toml
[behavior]
transition_mode = "cube"   # cube | flip | coverflow | helix | slide | fade | …
overview_enabled = true    # Alt+Ctrl+Tab switcher

[animation]
duration_ms = 250          # cube modes stretch this; see "Timing" below
speed = "normal"           # slow | normal | fast | instant
```

## The shared prism

`backend::compositor_common::prism` owns the renderer-independent camera,
regular-polygon metrics, transforms, face/cap ordering and projection. The X11
adapter in `backend::x11::compositor::prism` owns the full scene drawing:

- **Faces.** One quad per side, textured with a window snapshot (switcher) or a
  workspace snapshot (transition). Empty slots become tinted glass panels so the
  solid never has a hole in it. Each face is lit per fragment: a diffuse term, a
  specular highlight that sweeps across the surface as the prism turns, a
  Fresnel rim, rounded corners and a narrow accent bevel along the seams.
- **Caps.** Attributeless triangle fans closing the top and bottom, with a
  radial gradient, a slow angular sheen and a lit rim. Back-facing caps are
  culled on the CPU.
- **Reflection.** The whole object is drawn a second time mirrored through the
  floor plane at its own bottom edge and fading with distance. X11 adds a gentle
  animated ripple; Wayland keeps the reflection static once geometry settles so
  it does not turn an otherwise damage-driven overlay into a continuous redraw.
- **Skydome.** A procedural backdrop: star layers that pan with the rotation,
  a glow band on the horizon, and a light pool on the floor centered where the
  prism stands. The horizon is derived from the camera pitch, so it always lines
  up with the reflection.

There is no depth buffer, so faces and caps are depth-sorted per frame and drawn
back to front. Rotating the prism turns the faces translucent, which is what
lets the back of the cube show through — Compiz's transparent cube.

The shared geometry clamps the solid to three through six sides and derives its
apothem as `face_half_width / tan(PI / sides)`. This matters visibly: the former
Wayland implementation always used the hexagon constant `sqrt(3)`, leaving gaps
between four nominal "cube" faces. Both backends now consume one canonical side
count, camera fit, baseline solve and painter order. Regression tests close
every adjacent seam for 3–6 sides and cover portrait, standard and 32:9 output
aspects. On ultrawide outputs the baseline solve is no longer truncated, so the
prism leaves the intended room for its title and reflection.

The camera frames the front face to a fixed share of the monitor height whatever
the side count is, and solves for the lift that lands the prism's bottom edge on
a fixed line. Without that, a six-sided prism — which sits further from the
camera than a cube — would push its own reflection off the bottom of the screen.

## Alt+Ctrl+Tab switcher

The polygon takes the shape of the window count: four windows really do give a
cube, three a triangular prism, up to six sides. The selected window's face
carries a full-strength accent rim; the others are dimmed and desaturated.
Rotation is an exponential ease with a spin-energy term that drives the
see-through body, an extra camera pull-back and a deeper tilt, then decays so
the cube settles instead of snapping back to opaque.

The X11 switcher animates continuously (twinkling sky, sheening caps), so it
asks for frames until it closes. Wayland's stars, light pool, caps and reflection
are deterministic for the current angle; after opacity and rotation converge,
only content damage requests another frame.

Wayland currently shares the solid's geometry, pitched framing and
world-space face lighting. Selection is a bevel and inner accent rendered in
the face plane, so it stays attached while that face turns; the title is
anchored below the projected selected face and remains inside its owning
monitor. Every geometric side is drawn: unused slots in a one- or two-window
triangular prism, and live entries whose surface or texture is temporarily
unavailable, become dark tinted filler faces rather than holes. A separate lit
triangle-fan material closes the visible polygon cap. The whole solid is also
drawn through its animated bottom plane as a restrained reflection: it keeps
only camera-facing pieces and fades from the floor contact line toward the far
edge. Faces and caps retain the shared back-to-front `PrismPiece` order inside
each reflected and solid pass because the overlay has no depth buffer.

The dedicated GLES skydome is clipped to the owning monitor while using the
full-output viewport required by its pixel-space projection. It provides a
static parallax star field, camera-derived horizon and floor light pool without
reusing `overview_bg_program`; Expose and Peek therefore retain their simpler
vignette material and cannot inherit overview-only uniform state.

All GLES scene materials preserve premultiplied alpha and honor opaque-region
semantics for live textures. Live faces consume the same per-window
`wp_color_management` plan as the ordinary window pass, in both the solid and
mirrored draw. Encoded premultiplied RGB is unpremultiplied, decoded, gamut
mapped, optionally re-encoded, then premultiplied again; alpha-zero samples are
forced to zero and filler faces never inherit a live window's transform.
Explicit PQ and HLG source descriptions retain their decode plan in scene-linear
mode so they cannot fall through to the undescribed-sRGB fallback, while the
encoded path still elides identity transforms and preserves direct-scanout and
effect fast paths. Runtime matrices use one column-major/`GL_FALSE` upload
contract shared with the ordinary window renderer.

The skydome, caps, title and scroll strip bind their color domain explicitly.
With a live FP16 target, each described live face is converted from its source
description into the compositor's normalized linear-sRGB workspace. Its plan no
longer depends on pre-overview output overlap, so rotating or retargeting the
prism across outputs cannot leave a face encoded for its old monitor. The whole
overview remains in common linear light at its existing painter layer until the
frame's output-delivery stage.

When every later pass is scene-linear-aware, JWM finalizes each supported
physical output region with that output's linear-sRGB-to-native matrix and
transfer function. The current software partition accepts nonnegative physical
origins, unit scale, normal transform and nonconflicting overlaps. Alternatively,
hardware delivery is armed only when every participating CRTC coherently accepts
both its CTM and GAMMA_LUT; a LUT-only result is rolled back. Both routes keep
mixed-transfer regions independent in the planner and shader infrastructure
rather than choosing one global encode.

Delivery deliberately falls back to one global sRGB encode whenever an
encoded-only late overlay is visible, capture needs an encoded framebuffer, the
FP16 target or a safe region plan is unavailable, or Smithay will append an
external KMS cursor, drag icon, lock surface, or top/overlay layer. The ordinary
desktop cursor is external and normally lies on an active output, so live
per-output delivery is currently exercised mainly as infrastructure while most
interactive frames use the fallback. Encoded-only generated UI and external
elements do not yet participate in the common workspace. Until they do, JWM
rejects enabling DRM `HDR_OUTPUT_METADATA`, clears inherited metadata during KMS
ownership transitions, and keeps the actual output signal on exact sRGB; EDID
PQ/HLG profiles exposed by diagnostics describe capability, not an active route.

The working values are relative: JWM does not yet normalize SDR, PQ and HLG to
one absolute luminance scale. Explicit non-D65 white points also lack a
chromatic-adaptation transform; dynamic surface-description changes are not yet
latched to their matching `wl_surface.commit`; and KMS color-property updates
are not committed atomically with their matching framebuffer. Those are
output-pipeline limits, not overview-specific exceptions.

Headless GLES pixel regressions cover selection lighting, fade premultiplication,
opaque-region alpha, texture-independent filler faces, linked polygon caps,
reflection contact falloff, deterministic skydome pixels, scene-linear
materials, non-symmetric color matrices, explicit PQ/HLG decode plans and transparent
color-managed texels. The fullscreen transfer pair is also checked against the
CPU oracle for Linear, Power, BT.1886, Gamma 2.2, PQ, HLG and sRGB. Asymmetric
matrix and disjoint-region pixel tests cover independent per-output delivery,
including untouched gaps, while route/scissor tests lock conservative fallback
selection. CI forces Mesa's
surfaceless EGL platform and treats a missing GL context as a failure, so these
checks cannot silently downgrade to skipped tests. The legacy workspace-cube
branch is also rendered with hostile lit/filler uniforms to lock its original
brightness-only pixels.

## Tag-switch transition

`transition_mode = "cube"` maps the outgoing tag onto one face and the incoming
tag onto its neighbour, then turns the cube a quarter turn. The camera starts
square to the front face, which makes the first and last frame identical to the
flat workspace, and pulls back and tips down in between. Faces stay opaque: the
two workspaces are the whole story.

This is the one transition that needs the destination as a texture rather than
as the layer underneath, so it allocates a second monitor-sized target
(`needs_new_scene_fbo`) and captures the composited destination once per
transition, not once per frame.

`flip`, `coverflow` and `helix` fly a single workspace card instead of turning a
solid, but they share the same lit-face shader, and CoverFlow gets the floor
reflection its name implies.

`book` turns the old workspace like a book page: hinged on the spine-side
monitor edge (left going forward, right going back), bending along a circular
arc away from the viewer. The page is tessellated into strips whose chords ride
the arc (`backend::compositor_common::page_curl`, shared with the Wayland
compositor), parametrized by arc length so the paper never stretches. The curl
is zero at both ends — the first and last frame are the flat workspace — and
the free edge leads but is clamped at the landed position, so the page settles
tip-first the way paper does.

## Timing

Flat wipes read best when they are quick; a rotating solid needs long enough for
the eye to follow it around. The cube family therefore stretches the configured
animation duration (cube and helix ×1.8, book ×1.6, flip and coverflow ×1.4)
rather than asking for a second duration setting.

## Shader hot-reload

On X11 the three programs — `overview_bg` (skydome), `overview_face`,
`overview_cap` — participate in the compositor's shader hot-reload. Drop an
`overview_face.frag` into the configured shader directory to iterate on the
lighting without restarting the compositor. Wayland owns a distinct
`overview_skydome_program` in addition to the vignette program shared by Expose
and Peek; both are constructed and released with the compositor's other raw
GLES resources.
