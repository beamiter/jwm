# Native Wayland single-surface source (experimental)

This document describes the native Wayland **source** and explicitly selected
native Wayland viewer. The X11 viewer remains available. Building the helper with
`remote-wayland` also enables shared `remote-x11` code, but selecting native
Wayland capture or presentation does not authorize an X11 fallback. These are
experimental scoped features, not a production-complete remote desktop.

## Local, single-use selection

```sh
jwm-remote host --wayland-select --once --key-file /path/to/session.key
# Optionally select a specific native socket with --wayland-display wayland-0.
```

The trusted local host enumerates current foreign-toplevel handles on one
Wayland connection and asks for a one-based index on its own stdin. Titles and
application IDs are bounded, escaped labels only, not lookup keys. The selected
live protocol resource is retained while creating its capture source and session
on that same connection. There is no identifier re-resolution, title matching,
automatic replacement, reconnect or source switch. An invalid choice, closed
handle or timeout ends the attempt. The host creates this session before it
announces a network listener.

`--key-stdin` cannot be combined with local selection: they would compete for
stdin. Use a key file without putting key contents in logs or command history.
The default network listener remains loopback; the normal transport key and
trusted-LAN restrictions continue to apply. The network viewer cannot choose
which native window to share.

This authorization boundary is the user explicitly launching a trusted local
host and selecting its current resource. Existing compositor capture globals
are not a new per-client capability system or isolation from other unsandboxed
local capture applications. This path performs no IPC/native resize action and
therefore does not use an X11 nonce or cross-connection native window lookup.

## Narrow capture contract

Only the selected native surface tree at logical-resolution scale 1 may be rendered. There is no desktop,
output, root-rectangle or overlay fallback. Global cursors, keyboard, pointer,
clipboard and remote resize remain disabled. Extra source grants, X11 selection
options and input permissions are rejected rather than ignored.

The client uses one in-flight SHM capture at a time and accepts only bounded
XRGB8888 buffers with a normal transform. Size, stride and allocation arithmetic
must be checked before allocation/readback. The existing frame limits also bound
the encoded canvas. Changing a viewer viewport changes only encoding, never the
application's native geometry.

A closed/unmapped source, lock transition, native source size change, or committed
buffer scale/transform change ends this
default grant permanently, even after remap/unlock/resize-back. The separately
opted-in read-only resize-follow mode below permits size changes only. The server
must bind the captured surface and its mapping/lock generation, and revalidate
queued frames before rendering; checking only when a session was created is
insufficient. The native server bounds live sources to 256 and sessions to 512;
overflow produces inert/stopped objects, not a fallback capture. The renderer
requires exact buffer geometry and must not send ready after a failed copy.
Output capture is not covered by this single-surface hardening. The client must treat stopped/failed/disconnected sessions as
terminal, not rebuild a source. Already delivered pixels cannot be recalled;
the recipient may retain what they have seen.

## Validation boundaries

Run fresh client/state tests and real protocol requests against the compositor
for lifecycle ordering, malformed constraints, selection cancellation and lock
revocation. Wire-state tests do not exercise GL pixels. A nested/offscreen
renderer test must be labeled as such, not as real DRM/KMS verification. Actual
DRM validation still requires a native session and capture of a known surface,
including occlusion, lock and unmap races. Neither compilation nor a synthetic
nested display is evidence that physical-display capture has passed.

## Optional keyboard-only lease (experimental)

This separate capability requires both local `host --wayland-select --once
--allow-wayland-keyboard` and explicit viewer `connect ... --wayland-keyboard`.
It does not enable the old global input flag, pointer, clipboard, resize, XTEST,
source switching or automatic input grab. A saved profile cannot grant it.
The default remains view-only. The selected target must already have exact
native keyboard focus: the remote path never changes focus to obtain access.

Current protocol v23 retains the independent keyboard capability introduced
in v16 and the pointer capability introduced in v17; scrolling has its own v18
opt-in. After the first frame is actually reported presented by the viewer, it
requests one keyboard lease for
that source epoch, encoded geometry epoch and frame sequence. The host checks
its acknowledged-frame metadata and arms the native lease with the retained,
ready image-copy frame resource on the same Wayland connection. A native
`armed` event is required before telling the viewer it may send keys. This
trusts the authenticated viewer's presentation report, not proof of physical
display, and does not promise the latest visual content before every keystroke.

The viewer and native host must agree on the **entire** canonical XKB keymap,
using SHA-256 of the bounded canonical text, not the older truncated per-key
core-X11 comparison. Release all viewer keys before connecting; initial
modifier/group state must be clean. Wait for **Wayland keyboard ready; you may type**
before typing. The earlier **host approved** message is not readiness: local focus
validation is still pending, and a key edge during that interval terminates the session.
The ready message follows successful local validation; later focus loss still revokes access. Keymap/layout
changes, including a change later restored, revoke the permission. Unsupported
or different maps are rejected without guessing a keycode translation.
The native lease uses independent keyboard state and accepts only its bounded
shared key allowlist, not arbitrary keycodes or compositor shortcut commands.

The hard lifetime is **30 seconds**, non-renewable. Each original capture
authority permits only one keyboard lease attempt, including failed attempts;
reusing its source or session cannot renew permission. Expiry is enforced by the
native compositor timer, not merely on the next received key. It cannot be
extended by heartbeats, frames or another request. Focus change, local takeover,
source or geometry change, lock, disconnect, malformed sequence or a budget
violation permanently revoke it. Cleanup belongs to the original lease's own
held keys and must never inject releases into a new focus target. No keys are
replayed and no grant is automatically recreated after revocation.

There is one bounded edge per record, strictly increasing sequence numbers
starting at one (maximum 60,000), at most 32 held keys and bounded traffic.
The network request queue holds at most 32 entries; overflow fails closed.
Viewer focus loss or cancellation clears its queue and ends permission.
Normal source viewing may continue after keyboard revocation; starting a new
keyboard grant requires a fresh explicit local authorization.

New fixed-size records are request (56 bytes: source/geometry/frame plus full
keymap digest), status (33 bytes), key edge (43 bytes) and revoke (8 bytes).
All are independently validated; legacy InputBatch cannot bypass the lease.
Real recipient and revocation-negative tests must pass against the native
protocol before this capability is described as verified. Compilation, state
unit tests or the earlier read-only CI do not establish keyboard safety.

## Optional single-surface pointer lease (experimental)

The separately negotiated pointer mode requires explicit local
`host --wayland-select --once --allow-wayland-pointer` and viewer
`connect ... --wayland-pointer`. It is off by default and cannot be combined
with the keyboard lease, legacy input, clipboard, resize or input grabs. It
never enables XTEST. Saved profiles and automatic reconnect cannot grant it.

The native viewer may explicitly select this mode with
`connect ... --viewer-backend wayland --wayland-pointer`. Choose exactly one
of native read-only, keyboard, pointer or unified control mode. Native scrolling additionally
requires `--wayland-scroll` and the host's separate `--allow-wayland-scroll`
permission. Native pointer input accepts
only observed LeftDown→LeftUp pairs for atomic clicks. An unmatched release
cancels, never creates a click. Since `wl_pointer.enter` provides no held-button
snapshot, this does not establish that all buttons were initially released.
Non-left button input, unsupported axis input, or leaving the viewer surface permanently revokes
the pointer lease. Video may continue read-only; returning to the surface does
not automatically recreate permission. This differs from native keyboard focus
loss, which terminates the session.
No drag, chords, implicit input grab or automatic renewal is enabled. The same
fresh actual-presentation/Ready gate and 30-second lease apply. Default read-only
native viewing does not bind input devices.

The native scroll adapter binds pointer protocol versions 5–7, not version 8.
It accepts only explicit `axis_source=wheel`, separately authorized single-axis
`axis_discrete` of exactly
+1 or -1 and matching finite, nonzero continuous-axis direction in one complete
`wl_pointer.frame`. Source/discrete ordering is not assumed. Missing or unknown
source, unauthorized axes, smooth/finger/continuous scrolling, multiple notches,
and mixed button/axis input are not supported. It does not accumulate fractional
steps, batch notches or replay scrolling received while Waiting. Each accepted
notch still consumes a fresh actual-presented frame/Ready ticket. This reuses the
existing scroll protocol and host permission without extending the input lease.

The base pointer grant supports surface-local absolute motion and an atomic
left click. Optional discrete vertical scrolling requires the separate explicit
grant below; a pointer grant alone does not permit scrolling. Dragging, separate
button down/up, relative motion, remote focus changes and remote pointer enter
remain unsupported. Both native pointer and keyboard focus must already be the selected
surface. The compositor sends only to the captured receiver set and does not
change the physical seat's coordinates, held buttons, grab or enter serial.
Remote clicks are not authority to move/resize windows, grab popups or start
window-manager drag-and-drop operations.

Coordinates are checked against the captured source and the viewer's actual
presented-image rectangle. Black bars and out-of-bounds coordinates are not
clamped onto the application. The initial native implementation intentionally
rejects unsupported mapping arrangements rather than guessing: transformed or
scaled buffers, viewport/cropping offsets and subsurface layouts require a
separate supported mapping implementation. Input-region exclusion also denies
an action. Changes to the admitted mapping revoke the lease.

Each action consumes a fresh, genuinely ready capture frame that the
authenticated viewer has reported presenting. A frame captured before the
previous action cannot authorize the next one. A matching source, geometry,
frame and lease scope is required throughout; rejected or stale actions are
never silently retargeted to a newer frame. This trusts the authenticated
viewer's presentation report, not proof of physical display, and cannot prove
that an application has not changed its content since capture.

The viewer uses a short click-ready interval rather than continuously changing
the picture underneath a pending click. After presentation and host
confirmation, the frame can remain held only while its native capture age is
at most one second, not for an additional second after the reply arrives. A
delayed action can therefore be denied even if the viewer just displayed Ready.
A motion or
click consumes that opportunity and requires a new capture/presentation cycle.
Consequently this conservative mode may briefly pause video and cannot click
immediately while moving. A new frame, focus loss or viewport change cancels a
pending local button candidate. No remote button is left pressed while waiting
for a frame or network reply.

The lease is limited to 30 seconds, is not renewable, and each selected capture
authority allows only one attempt. Local input, lock, focus/receiver changes,
unmap, source or geometry invalidation, malformed sequences and disconnect
permanently revoke it. Starting again requires fresh explicit local selection;
it never reconnects or replays actions automatically.

Acceptance requires actual `wl_pointer` receiver and serial-authority negative
tests, native production-renderer frame tests, adapter tests and the
authenticated transport path. Pure coordinate tests or successful compilation
alone are not evidence of safe pointer delivery. These cloud/offscreen checks
are not physical DRM, dual-machine or real desktop-viewer acceptance.

### Optional discrete vertical scrolling

Scrolling is off by default and requires both local opt-ins, in addition to the
existing pointer flags:

```sh
jwm-remote host --wayland-select --once --allow-wayland-pointer --allow-wayland-scroll ...
jwm-remote connect HOST:48221 --wayland-pointer --wayland-scroll ...
```

The independent scroll capability does not extend an old pointer grant. Both
sides must explicitly enable it; permission cannot be inferred from incoming
actions. The current stream protocol version is 19, and older versions are rejected
rather than silently downgrading. No profile, reconnect, keyboard, clipboard or
legacy input permission is enabled by these flags.

Only a single vertical wheel notch up or down is supported per action. There
are no arbitrary axis magnitudes, implicit horizontal permission, smooth or
continuous scrolling, or inertia. The native compositor delivers the fixed wheel sequence
to the same authorized surface/receiver snapshot at the validated surface-local
coordinates; it does not change physical seat position or global focus. The
application decides what one notch means, so equal line or pixel movement
across applications is not promised.

Each notch consumes its own fresh capture/presentation ticket under the existing
source, geometry, receiver, focus and one-second capture-age checks. Waiting
notches are dropped, never accumulated, batched or replayed when Ready returns.
A new physical notch after a new Ready frame is required for another action.
The existing 30-second nonrenewable lease and every local-takeover/lifecycle
revocation still apply. This conservative interaction is not touchpad-like
continuous scrolling and does not increase the input authority of a read-only
session.

The actual viewer regression includes an owned application that changes its
buffer only after receiving a real native axis event. Its changed pixels must
then traverse production capture, authenticated TCP, viewer drawing and a new
presented-frame acknowledgement. It also covers an unrequested scroll grant,
Waiting bursts and a non-target receiver. These are test requirements, not
physical-display, dual-machine or general application compatibility claims.

### Actual viewer interaction regression

`python3 scripts/test-remote-viewer-e2e.py` is the CI-only actual-viewer runner.
It creates and later reaps its own isolated Xvfb, removes ambient display
selection, and requires the exact ignored integration test to execute once.
It does not operate on a user desktop. The test runs the production viewer,
authenticated TCP host, native capture renderer and original `wl_pointer`
recipients. XTEST is used only by this test driver on that owned Xvfb; it does
not expand host input permission.

The matrix covers actual presented red target pixels and Ready-gated clicks,
black-bar input, focus loss, viewport change, resize-restore history and a
view-only session. The viewer must revoke on each real size-change event,
even if the window returns to its original dimensions in the same event batch.
Final dimensions alone are not enough to establish the presented coordinate
mapping. Any revocation in a batch takes precedence over pointer actions.

The pending-click scenario also crosses an actual new presented frame during
a short local button hold. A per-session test-only trigger may shorten the old
frame's lifetime and omit that old frame's advisory Waiting notification to
isolate the new-frame cancellation path. It cannot extend a native permission
or inject a frame ACK.
The next frame still uses real capture, encoding, transport and presentation.
This controlled case uses a real 200×150 SHM source and a 400×300 viewer
viewport to bound debug software-rendering cost. Other pointer cases retain
800×600 sources. This is a cancellation regression under controlled load,
not a guarantee that arbitrary sources or networks present within 250 ms.
The test logs candidate, refresh-consumption, presentation and release timing,
and requires the release to complete within the existing 250 ms click limit, no action to be sent for that cancelled candidate, and a subsequent
legitimate click on the same lease to succeed.

A separate actual keyboard viewer matrix explicitly installs the owned Xvfb's
complete canonical XKB map on the native test seat before admission. It tests
key edges only reaching the original target, held-key cleanup after viewer
focus loss, and rejection of a deliberately different map. This establishes
the explicitly matched-map path, not default keymap compatibility on arbitrary
desktops. The product's full-map comparison remains unchanged.

These are controlled software-display regressions, not physical-display/DRM
tests, two-computer network tests or proof of usability under WAN latency.

### Scroll-copy is not scroll permission

The separate experimental host `--scroll-copy` flag enables a default-off
software codec optimization (introduced in v19, extended in v22), not a native input capability. It can optimize a
read-only source without allowing any keyboard, pointer or scroll events.
The original explicit `--allow-wayland-scroll` / viewer opt-in, fresh-frame
checks, exact receiver binding and 30-second nonrenewable lease remain unchanged.

Only exact full-width RGB shifts of ±1..64 pixels with at least half-frame
overlap qualify. In v22 the shift can be restricted to one bounded vertical band,
so fixed top and bottom toolbars need not move. Copies require the matching
source, geometry and base sequence; pixels outside the band still use ordinary
dirty-tile detection. Unsupported or non-beneficial shifts and resizes fall back
to normal tiles.
Older peers are rejected, and profiles/SSH do not implicitly persist this flag.
This CPU experiment does not establish GPU, physical-display or WAN latency
improvements; copying existing decoded pixels also does not guarantee better
quality than re-encoding them.

### Native Wayland viewer: explicit keyboard-only mode

The native viewer still requires an explicit mode. Read-only remains:

```sh
jwm-remote connect HOST --viewer-backend wayland --view-only
```

For keyboard-only control, the trusted host must separately allow its existing
single-surface keyboard lease, and the viewer must explicitly request it:

```sh
jwm-remote host --wayland-select --once --allow-wayland-keyboard --key-file /path/to/session.key
jwm-remote connect HOST --viewer-backend wayland --wayland-keyboard --key-file /path/to/session.key
```

Use `--viewer-wayland-display SOCKET` to select the local native display, not
`--display`. Exactly one of `--view-only`, `--wayland-keyboard`,
`--wayland-pointer` and `--wayland-control` is required with `--viewer-backend wayland`; these modes are
mutually exclusive and omitting all four is rejected rather than enabling legacy input. No host authorization is inferred from choosing a viewer backend.

This mode reuses the existing exact-surface keyboard authority, full keymap
compatibility check, and nonrenewable 30-second lease. A mismatched keymap or
missing capability fails closed, without an X11 fallback. Focus/lifecycle loss,
local host takeover, lock or disconnect revoke the lease and apply the existing
held-key cleanup; the old lease is never resumed automatically.

Native presentation does not grant pointer or wheel input, clipboard access,
input grabs, source resizing/following, automatic reconnect, or a saved profile.
Those combinations remain rejected. This addition does not change the network
protocol or relax host input permission. It is distinct from the X11 viewer's
separately authorized pointer and discrete-scroll modes, also available through
the native viewer with the narrower wheel-event limits described above.

Tests must verify actual native keyboard recipients and presentation feedback;
compilation or protocol fixtures alone do not demonstrate physical DRM display,
real desktop keymap compatibility, two-machine networking or interactive latency.

While scoped keyboard authorization is waiting for the host grant, do not type
into the viewer. Any actual key edge during that interval, even a balanced
press/release pair, cancels the attempt and terminates the session rather than
being silently dropped and later granting control. This safety rule applies to
both the X11 and native Wayland scoped-keyboard viewers.

In the native Wayland keyboard viewer, local keyboard focus loss is fail-closed:
the session terminates rather than reusing the lease after focus returns. Start
a new explicitly authorized session to regain control.


## Optional unified single-surface control bundle (experimental)

This is a separate, default-off authorization, not a combination of the old
keyboard and pointer flags. The host explicitly selects one surface with
`host --wayland-select --once --allow-wayland-control`. The viewer explicitly
uses `connect HOST --viewer-backend wayland --wayland-control`. Both peers must
use application protocol **v23** (31-byte capabilities). Older peers are rejected;
there is no protocol downgrade. Transport encryption independently requires v3.

The bundle has one native resource, source owner/instance, epoch, cross-type
sequence and creation-time **30-second nonrenewable deadline**. It atomically
consumes both input attempts on that capture authority. It never partially
falls back to a separate lease or legacy XTEST. Existing keyboard-only and
pointer-only modes remain separate and mutually exclusive with this mode.

Keyboard and pointer must both already focus the selected source. Granting
control never changes focus. The viewer binds both devices on one actual seat,
requires a clean keyboard enter and matching complete canonical keymap, and
requests permission only after a real frame presentation. Each pointer action
needs a matching, recent host Ready ticket backed by capture and presentation.
Every key invalidates the pointer ticket; subsequent pointer input needs a newer
captured and presented frame. Keyboard chords such as Shift+A can span video
updates, but any held key blocks a real pointer action. Pointer gestures also
block keyboard input. Overlap ends the whole session rather than guessing order.

Supported actions are keyboard edges, absolute motion and an atomic left click
from the viewer's own bounded down/up gesture. There is **no implicit wheel permission, drag, modifier-click, clipboard,
resize/follow-size, reconnect or stored profile permission** in this mode. The independent scroll-copy video codec option grants
no input permission. The global cursor and unrelated surfaces are not recipients.

Focus loss on either viewer device, seat/keymap changes, source remapping or
loss, lock, local takeover, expired authority, invalid sequence or frame, and
transport loss revoke the entire bundle. Cleanup affects only that lease's held
keys on its original recipient. No return of focus or content revives permission.
The viewer preserves actual event dispatch order in a bounded queue; it does not
concatenate separately polled keyboard and pointer batches. A new video mapping
can cancel an old pending click without treating that cancellation as a real
pointer action while a key is held.

Validation for this slice includes pure wire/state/CLI checks and dedicated CI
fixtures for actual wl_keyboard/wl_pointer recipients, shared lease revocation,
owned native viewer presentation, and authenticated TCP from production capture
to production viewer GL. The combined fixture checks click→application repaint→
actual newly presented pixels→typing, and Shift across a new frame before more
key edges. Negative cases cover either-side leave, held-enter, stale frames and
held-key/pointer conflicts with cleanup only on the original target. Until these
checks run on the final commit, compilation alone is not a runtime pass. These
owned offscreen/software-renderer tests do not establish physical DRM/GPU,
two-machine or input-to-visible latency acceptance.


### Separately authorized discrete wheel in a unified session

Add `--allow-wayland-scroll` to the host's `--allow-wayland-control` and
`--wayland-scroll` to the native viewer's `--wayland-control` to opt in on both
ends. Omitting either permission must not silently enable wheel input. The
same flags still work with the separate pointer-only mode; a keyboard-only or
read-only mode cannot request them. Protocol v23 is required on both network
peers. The native control manager must support version 2 for its separate
`create_lease_with_scroll` request; an older manager is rejected, not treated as
permission. Ordinary `create_lease` remains wheel-disabled even on version 2.

Only a real Wheel-source, single vertical discrete notch of +1 or -1, with a
finite nonzero matching axis value in the same completed pointer frame, is
accepted. Both source orderings within that frame are supported. No smooth,
multi-notch, continuous, missing-source or malformed axis event is
converted into a guessed notch. The target receives one native vertical wheel
action, scoped to the same surface and sequence as keyboard/click input.

Each wheel action consumes its fresh frame ticket. A new actual capture and
viewer presentation are needed before another pointer action. Events observed
while Waiting are dropped rather than accumulated or replayed later. Opening
Ready includes a real same-connection fence with the input gate closed, so old
queued axis groups cannot inherit a newly available ticket. Any held remote key
or observed viewer left button forbids scrolling and revokes the bundle; scrolling
never grants modifier-wheel or drag behavior. The original lease expiry, source,
focus and local-takeover revocations are unchanged and never renew the deadline.

Dedicated owned-server regression fixtures exercise actual wheel recipients,
both directions and new-pixel feedback, alongside old-permission, stale-frame,
Waiting and held-input negative cases. As elsewhere, this is software-rendered
protocol/transport verification, not physical input or two-machine acceptance.


### Bounded-band scroll-copy codec (v22)

The video-only `--scroll-copy` option remains default-off. Its wire extension is
14 bytes: prior frame sequence (u64), signed vertical shift (i16), band top (u16)
and exclusive bottom (u16). A band always spans the encoded width. Only the
intersection where both old source row and new destination row are inside the
band is copied. This does not authorize capturing a new region or any input.
Both network peers must upgrade to v22; the capability block remains 31 bytes.

Candidate search has a fixed sample budget and chooses at most one shift for a
full-width row comparison. A contiguous candidate overlap must be pixel-exact
against the committed raw reference and cover at least half the image. A copy
that cannot reduce dirty-tile work is rejected. All exposed and mixed boundary
tiles are transmitted completely; other pixels outside the copy band are left
in place and still checked for ordinary updates. Quality debt follows copied
pixels rather than being silently reset to the current JPEG quality.

The receiver validates the whole extension, base sequence, source/geometry,
bitmap coverage, atlas and budgets before committing a new image. Malformed
bands, missing boundary tiles, invalid JPEG or stale bases cannot partially
change its canvas. Existing immutable snapshots remain unchanged. The encoder
commits only after the authenticated frame send and flush succeed; discard never
advances its reference or sequence.

Offline A/B measures identical source hashes, encoded bytes, distortion and CPU
work for whole-frame scrolling, fixed toolbars/footers, threshold-sized bands,
static/random content and unsupported regions. These measurements do not imply
GPU or physical presentation speed, and copied JPEG pixels are not claimed to
have the same quality as re-encoding. Exact results belong with the benchmark
report for the tested revision.

See [the bounded-band offline A/B report](remote-scroll-band-benchmark.md) for
identical-input byte, quality and CPU measurements, including fallback cost.

## Explicit read-only native resize following

`host --wayland-select --once --wayland-resize-follow` opts into following
size changes made by the selected application or its local user. It does not
request a native resize, move a window, or reuse the X11 `--follow-source-size`
control. Without this option, the original resize-ends-capture contract remains
unchanged. The mode is strictly read-only and cannot be combined with native
keyboard, pointer, unified control, clipboard, reconnect or remote resize grants.

The native compositor must explicitly support the read-only resize-follow
`jwm_readonly_resize_capture_manager_v1` extension. This is a local protocol
capability; network protocol v23 and the existing geometry-epoch frame format
are unchanged. An older compositor is rejected, never treated as permission
to recreate an ordinary source. The original surface resource and connection
remain fixed. A new layout replaces the capture session and starts a new video
geometry epoch and keyframe; old-layout frames cannot be acknowledged as the
new layout. Queued host frames from the old epoch are discarded before the
next send; bytes already written to the socket cannot be recalled. Following
is not reselecting a similarly named window.

The derived capture authority and its original authorization family cannot
subsequently grant input. Resize generations and capture allocations are bounded;
exhaustion ends the source instead of wrapping or resetting the budget. Source
unmap/remap, destruction, lock, disconnect and committed buffer scale/transform
changes remain terminal, even if the original geometry or mapping later returns.
Only size changes within the original live authorization may be followed.
This first follow mode supports a root surface without subsurfaces. A subsurface
tree is rejected rather than silently clipped or traversed without a layout
budget. Ordinary non-follow capture retains its existing surface-tree contract.
When no explicit xdg window geometry is supplied, the layout comes from the
actual committed root buffer view, not the window manager’s requested size.
If another resize interrupts session reconstruction, the bounded attempt may
terminate rather than retry indefinitely.

Verification must cover old pending frames and resize-back (ABA), new-size pixel
isolation, and terminal lifecycle changes using actual native protocol dispatch.
Authenticated TCP and owned software-rendered viewer tests must confirm the new
geometry and actual presented pixels. These tests are not physical DRM/GPU or
two-machine latency acceptance.

### Independently authorized horizontal wheel (v23)

Wide tables and canvases may opt into one discrete horizontal wheel notch:

```sh
jwm-remote host --wayland-select --once --allow-wayland-control --allow-wayland-horizontal-scroll ...
jwm-remote connect HOST:48221 --wayland-control --wayland-horizontal-scroll ...
```

The same horizontal flags are available with pointer-only mode. They do not
imply vertical permission: add the existing `--allow-wayland-scroll` and
`--wayland-scroll` independently if both axes are desired. Both ends must
explicitly authorize the chosen axis. Native manager/lease version 3 adds an
explicit axes request; old create and version-2 vertical requests remain
horizontal-disabled even when bound at version 3. A missing new native
capability is an error, never a downgrade to a different input mechanism.

Each completed Wheel-source frame must describe exactly one axis, one matching
finite axis value and one discrete notch of -1 or +1. Mixed axes, mismatched
axis/discrete fields, repeated fields, smooth input and multi-notch input are
rejected. Shift plus vertical input is not translated to horizontal. Native
Wayland viewers retain the version-5–7 discrete-event contract; X11 viewers use
physical wheel buttons 6/7. The native target receives a horizontal axis event,
not a modifier or synthetic key. No focus, recipient, lifetime or coordinate
scope is enlarged.

Horizontal actions consume the same fresh-frame ticket as clicks and vertical
wheel. Waiting input is not replayed, held keys/buttons remain prohibited, and
local takeover, lock, unmap and deadline revocations remain terminal. This adds
neither drag nor continuous scrolling.

Compatibility: the authenticated application protocol is now v23. Byte 28 of
the existing capability record carries the vertical/horizontal permission bits;
unknown bits are rejected. v22 peers must be upgraded together, rather than
silently interpreting horizontal permission as an old boolean. The scroll-copy
video format is unchanged and still grants no input authority.


## Explicit read-only popup capture

`host --wayland-select --once --wayland-popups` explicitly includes live
xdg-popup descendants of the selected application in a read-only stream.
The default still captures only the selected root's surface tree. The popup
mode requires its separate native compositor capability and is incompatible
with input, clipboard and remote resizing. Combining it with resize following
requires the additional explicit capability described below.

Every included popup must belong to the root application's client and have an
exact live parent chain ending at that selected surface. Titles and application
IDs are not authority. Popup depth, count, total surface count and checked
relative coordinates are bounded; malformed or excessive trees fail capture
rather than falling back to the desktop or silently selecting another window.
IME surfaces, global cursors and unrelated windows are not included.

The canvas stays the root window's original size. Menus outside that canvas
are clipped; this is not complete capture of an expanded application footprint.
Popup movement, creation and destruction are visual updates, not input grants.
Both the original authorization and its derived popup source remain unable to
grant keyboard, pointer or unified control. Root size/layout changes, unmap,
lock, destruction or disconnect remain terminal; no title-based reselection,
reconnect or permission renewal occurs.

Verification separates native protocol/production offscreen pixel tests from
an authenticated TCP and owned native viewer presentation path. Neither is
physical DRM/GPU, two-machine or input-to-visible latency acceptance.


### Read-only popups with size following

Use both `--wayland-popups --wayland-resize-follow` with
`host --wayland-select --once` to capture the same application's popup family
while following local root-window size changes. This combination requires
version 2 of `jwm_readonly_resize_capture_manager_v1`; its single atomic
`create_source_with_popups` request consumes the original input authorization
attempts and creates one read-only family. A version-1 compositor is rejected.
The version-1 request remains root-only even when bound at version 2, and
separate popup-only capture still uses a fixed original-size canvas.

Each new layout has an immutable capture-session size, crop origin and epoch.
A suspended layout invalidates pending old sessions, including a resize back
to the same dimensions. Initial xdg and effective crop origins may differ,
but each is frozen: changing either origin permanently ends the combined
source, even if it later returns. Popups are positioned relative to the committed root
window geometry and clipped to the current root canvas. This does not expand
the canvas beyond the application window, enable popup interaction, or permit
root subsurface trees in resize-follow mode. Existing tree/count/coordinate
budgets remain in force. Popup movement never grants input or renews authority.

The adapter rebuilds sessions only on the original source and connection.
All original and derived authorities remain unable to grant keyboard, pointer
or unified control. Lock, unmap/remap, destruction, disconnect and buffer
scale/transform changes remain terminal. Existing old-epoch queue barriers
apply; already transmitted socket bytes cannot be recalled. No desktop capture,
title-based reselection, reconnect or capability downgrade is used.

Verification covers actual native dispatch and production offscreen composition,
old pending-buffer non-writes, resize-back epochs and an authenticated TCP viewer
with exact presented-frame pixel checks. These remain owned software-rendered
tests, not physical DRM/GPU or two-machine latency acceptance.
