# Isolated popup interaction foundation

The current candidate connects the native isolated-seat foundation to an
explicit, experimental `jwm-remote` popup-pointer mode. It is not a portal
permission prompt or a claim of general GTK/Qt menu compatibility. Existing
read-only `--wayland-popups` capture remains read-only. The existing keyboard,
pointer and unified-control modes retain their single-surface restrictions.

## Explicit network use (application protocol v31)

Upgrade both endpoints together: application protocol v31 requires an exact
match and rejects older peers rather than falling back to ordinary input.
Transport v3 is unchanged; the host compositor must provide the native popup
protocol v2 capability. Both helpers need native Wayland support.

Using an existing private pre-shared key securely installed on both endpoints,
run on the host's JWM Wayland session:

```sh
jwm-remote host --wayland-select --once --max-width 0 \
  --allow-wayland-popup-pointer --key-file "$HOME/.config/jwm/remote.key"
```

Select the intended live root locally. On the viewer, connect through the
loopback listener (or an explicitly arranged tunnel):

```sh
jwm-remote connect 127.0.0.1:48221 --viewer-backend wayland \
  --wayland-popup-pointer --key-file "$HOME/.config/jwm/remote.key"
```

The key-file paths are examples, not key-generation instructions. For an
explicit trusted-LAN connection, the host also needs `--allow-lan --listen
HOST_IP:48221`, and the viewer must use that address. This does not replace
protecting the key or obtaining both endpoint permissions.

Both popup-pointer flags default off and are required. Do not add `--view-only`
or the older single-surface pointer flag. This mode cannot be combined with
keyboard, unified control, text commit, either scroll axis, clipboard, PNG
repair, scroll-copy, source resize/follow, read-only popup derivation, input
grab or automatic reconnect. It is not saved as profile authorization or
implicitly enabled by the SSH helper. The root canvas stays unscaled: nonzero
host `--max-width` is rejected, and negotiated frame limits must admit the
actual canvas rather than silently resizing it.

## Network proof and lifetime boundaries

One native lease is created after explicit negotiation and retains its original
30-second deadline. Successful captures, ACKs, arm completion, clicks and idle
refreshes do not renew it. Each proof has a separate one-second freshness bound
measured from the actual capture request. Missing presentation ACKs expire the
proof and end the session; a successful socket write alone cannot arm input.

The host sends a bounded family-metadata frame, commits it only after the full
record flush, and requires the exact sequence ACK produced after actual viewer
presentation. Arming consumes that first proof. A new capture and actual
presentation are required before a user click, and every accepted click consumes
its proof again. Old, future, duplicate or mismatched ACKs cannot grant input.

While idle with an acknowledged click opportunity, the host retires the current
proof at approximately 500 ms of capture age: it flushes Waiting before
releasing that proof and capturing its replacement. This is not a new lease or
a guaranteed refresh latency. A click already in transit when its proof is
retired may conservatively terminate the session; it is not replayed against a
new frame. Revocation, local takeover, lock, source loss and original expiry
terminate authority without reconnect or automatic regrant.

## Current network verification status

The product path and CLI are implemented, but the first network CI run exposed
a production Hello capability conflict before most interaction cases could
execute. The follow-up candidate corrects that construction and adds continued
real presentation through the original lease deadline. It also corrects the
popup reader's authenticated-record budget to include padding for a real Click;
four in-memory record-budget/keepalive regressions are being verified alongside the
network cases. Keepalive is one-way and does not reset proof freshness or the
original lease deadline. Its seven real network cases and these new regressions
are **not yet claimed to have passed** here. Pure codec or CLI checks are
not substitutes for those receiver, capture and presentation checks.

All presentation evidence is from owned software-GL test environments, not
physical display hardware, two-machine latency or arbitrary toolkit acceptance.
GTK3 dynamic isolated-seat operation is not established as compatible; an app
that does not use the authorized seat/serial must fail closed without a local
seat fallback. The native foundation and its earlier validation history are
outlined below.

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
viewer presentation remains an obligation of the trusted host. The v31 network
path adds the exact viewer-presentation ACK gate described above; native capture
`ready` by itself must still not be described as actual viewer presentation.

## Original native verification scope (historical)

At the first native candidate, implementation and independent static review
were complete while real CI validation was pending. Fifteen native protocol
tests and two pure lease tests were compiled as part of that candidate. Native tests include a
real root click, direct popup creation and pre-map grab using the emitted
serial, captured popup pixels, exact popup-local click delivery, unchanged local
seat state, and zero input to other clients. Negative coverage must reject old
frames, unsupported actions, wrong parents/owners, nested families, serial
replay, generic move/resize/drag use, lifecycle ABA and late input after revoke. Additional tests cover both directions of exclusion
with older input leases, stale or foreign export requests, and an owner
requesting a second capture source. A real timer test waits for the original
30-second expiry and checks private-seat retirement without submitting input.

All real Wayland and software-GL tests run in CI, not in the local restricted
execution environment. Those tests do not establish physical DRM,
two-machine performance or arbitrary toolkit compatibility. A subsequent
GTK/Qt integration must actually bind the isolated seat and open/select a real
menu, with no synthetic action callback or local-seat fallback.

## Dynamic global filtering prerequisite

The first native CI candidate passed the three cross-lease/export cases but all
12 main fixtures stopped at the private-seat visibility precondition. The
system backend called `wl_global_create` before registering the resulting global,
so synchronous registry advertisements treated it as an unknown foreign global
and bypassed its visibility callback. The separate bind check was still present;
this result does not establish unauthorized input delivery.

That follow-up candidate included a documented local patch to the same official
`wayland-backend` 0.3.17 release. It identifies only the exact in-progress Rust
global by display/state, type and userdata identity, preserves genuinely foreign
global handling, and guards lifecycle operations during creation/destruction.
Callback-owned data is cloned before reentrant code runs. An isolated harness
executes three production FFI-free guard tests; a separate real owned-display
regression checks callback reentrancy and registered handler retrieval in CI.
The registry visibility assertions remain strict. At that stage, these fixes required successful real CI before validation; this
historical failure is not evidence that the new v31 network candidate has passed.
