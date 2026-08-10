# Cube effects

JWM's X11 compositor draws two Compiz-style 3D effects from one shared object:
the Alt+Ctrl+Tab window switcher and the `cube` tag-switch transition. Both are
a lit prism standing on a mirrored floor inside a procedural skydome, so they
read as the same cube seen twice rather than two separate effects. The Wayland
overview now uses the same protocol-free prism camera and regular-polygon
geometry, plus its own GLES implementations of the lit face and cap materials.

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
  floor plane at its own bottom edge, fading with distance and rippling gently.
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

An open switcher animates continuously (twinkling sky, sheening caps), so it
asks for frames until it closes.

Wayland currently shares the solid's geometry, pitched framing and
world-space face lighting. Selection is a bevel and inner accent rendered in
the face plane, so it stays attached while that face turns; the title is
anchored below the projected selected face and remains inside its owning
monitor. Every geometric side is drawn: unused slots in a one- or two-window
triangular prism, and live entries whose surface or texture is temporarily
unavailable, become dark tinted filler faces rather than holes. A separate lit
triangle-fan material closes the visible polygon cap. Faces and caps retain the
shared back-to-front `PrismPiece` order because the overlay has no depth buffer.

The GLES materials preserve premultiplied alpha, honor opaque-region semantics
for live textures, and emit either encoded or scene-linear legacy-sRGB pixels
to match the output pipeline. Per-surface `wp_color_management` transforms
inside the overview remain future work. The mirrored floor and procedural
skydome are still X11-only scene layers.

Headless GLES pixel regressions cover selection lighting, fade premultiplication,
opaque-region alpha, texture-independent filler faces, linked polygon caps,
scene-linear materials and the scene-linear backdrop. CI forces Mesa's
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

The three programs — `overview_bg` (skydome), `overview_face`, `overview_cap` —
participate in the compositor's shader hot-reload. Drop an `overview_face.frag`
into the configured shader directory to iterate on the lighting without
restarting the compositor.
