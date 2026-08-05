# Cube effects

JWM's X11 compositor draws two Compiz-style 3D effects from one shared object:
the Alt+Ctrl+Tab window switcher and the `cube` tag-switch transition. Both are
a lit prism standing on a mirrored floor inside a procedural skydome, so they
read as the same cube seen twice rather than two separate effects.

```toml
[behavior]
transition_mode = "cube"   # cube | flip | coverflow | helix | slide | fade | …
overview_enabled = true    # Alt+Ctrl+Tab switcher

[animation]
duration_ms = 250          # cube modes stretch this; see "Timing" below
speed = "normal"           # slow | normal | fast | instant
```

## The shared prism

`backend::x11::compositor::prism` owns the geometry, the framing and the
drawing:

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
