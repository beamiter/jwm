# Isolated popup interaction foundation

This work is a native, trusted-local capability foundation. It is not yet a
`jwm-remote` command-line mode, a portal permission prompt, or a claim that GTK/Qt
menus work over the network. Existing read-only popup capture remains read-only.
The existing keyboard, pointer and unified-control modes retain their single
surface restrictions.

## Intended authority

A separate explicit lease binds one selected root instance and its application
client. Its pointer-only seat is visible only to that application. It must not
change the local seat's focus, pointer position, buttons, grabs, keyboard,
selection or cursor. Failure to bind the separate seat has no main-seat fallback.

The initial scope is the fixed root canvas and at most one direct child popup.
Once the child exists, root-area actions and outside-click dismissal are not
supported; the child must have completed its dedicated grab and initial mapping.
Nested or sibling popups, another root or client, unsupported transforms,
ambiguous hit regions and out-of-canvas targets must fail closed. No keyboard,
scroll, drag, independent button edges, resize, PNG repair or automatic renewal
is included. The original 30-second lease budget starts at creation.

Root clicks may create only a dedicated, one-use popup-grab capability. Such a
serial must never be an ordinary implicit-grab serial authorizing window move,
resize or drag-and-drop. A popup must request its dedicated grab before first
mapping. A captured frame does not grant permission to add a different popup.

## Capture and input proof

An interactive family source is separate from existing read-only derived
sources. Every usable frame must freeze the root, optional direct child,
geometry, mapping, input region, stacking and family generation that produced
its pixels. Renderer completion and input consumption revalidate that evidence.
An old layout or a destroy/recreate cycle cannot revive an earlier proof.

Input is checked in root-canvas coordinates and delivered only to the exact
surface and receiver set authorized by that proof. Changing or newly registering
receivers invalidates the old authority. Each action consumes a fresh frame;
arming or acting never extends the lease.

The native capture protocol proves captured pixels and native dispatch. Actual
viewer presentation remains an obligation of the trusted host. Until the
network/viewer integration is implemented and tested, native capture `ready`
must not be described as actual viewer presentation.

## Verification status

The first native implementation and independent static review are complete;
real CI validation is pending. Fifteen native protocol tests and two pure lease
tests are compiled as part of the candidate. Native tests include a
real root click, direct popup creation and pre-map grab using the emitted
serial, captured popup pixels, exact popup-local click delivery, unchanged local
seat state, and zero input to other clients. Negative coverage must reject old
frames, unsupported actions, wrong parents/owners, nested families, serial
replay, generic move/resize/drag use, lifecycle ABA and late input after revoke. Additional tests cover both directions of exclusion
with older input leases, stale or foreign export requests, and an owner
requesting a second capture source. A real timer test waits for the original
30-second expiry and checks private-seat retirement without submitting input.

All real Wayland and software-GL tests run in CI, not in the local restricted
execution environment. Passing those tests will not establish physical DRM,
two-machine performance or arbitrary toolkit compatibility. A subsequent
GTK/Qt integration must actually bind the isolated seat and open/select a real
menu, with no synthetic action callback or local-seat fallback.
