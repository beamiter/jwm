# Native Wayland seamless input: integration contract

Status: experimental, explicit CLI opt-in for a **JWM udev viewer compositor
and JWM native host**, not general Wayland seamless input. `--seamless-input`
remains X11-only. Native use is `connect --viewer-backend wayland
--wayland-control --wayland-seamless-input`; the dedicated flag requests the
resumable capability, without a separate client resumable flag. The host must
independently enable `--wayland-select --once --allow-wayland-control
--allow-resumable-control`. Defaults remain off; an unavailable capability or
local compositor protocol fails closed, without downgrade or XWayland fallback.
Nested JWM backends and other compositors do not advertise this contract.
The default 30-second path requires source control v4 and local handoff v2
(the interface names retain `_v1`). Local v2 guarantees scene hit-testing and
position-aware geometry invalidation; v1 compositors are refused. An explicitly
longer budget requires source control v5 and local handoff v3, with no fallback.
All network sessions require application hello v35 on both helpers. Update and
restart the relevant JWM compositors as well as updating both helper binaries.

See [the user-facing example and limitations](remote-seamless-input.md#experimental-native-wayland-jwm-udev).
The native option is not supported by `jwm-remote ssh` or saved connection
profiles. A manually configured SSH port forward is a separate transport option.
Entering the remote image can redirect typing away from the previous local
window; use this mode only when that per-session behavior is intended.

## Local compositor handoff contract

`jwm_remote_viewer_handoff_v1` creates one explicitly requested session for the
viewer's own normal toplevel and seat. Sandboxed/foreign surfaces are denied.
It grants no remote authority. Its fixed lifetime uses only the remaining
initial client budget: default 30 seconds, or an explicitly negotiated duration
of at most 300 seconds. It can only end earlier, never renew the remote lease.
The legacy `create` request remains limited to 30000 ms at every binding version;
only v3 `create_timed` accepts up to 300000 ms of remaining budget.

Entry consumes a fresh real `wl_pointer.enter` serial, requires clean physical
keys/buttons, and brackets the focus change with an ordered transition and
active acknowledgement. Only then may the client request/restore remote input,
and remote resume still needs its separate release ACK and fresh presented proof.
A leave, black-bar crossing or pending-exit latch stops local forwarding at once.
The local exit transaction removes shortcut inhibition and restores the previous
focus only when physical keys are clean. Held-key exit quarantines key edges until
release, rather than replaying the held chord into a local application.

The v1 entry token is surface-based. An exit inside the surface (including black
bars) releases authority, but a subsequent resume needs a complete surface exit
and new entry. It does not support black-bar-only back-and-forth re-entry. The
pointer is never confined. Configured lock-screen/lock-monitor shortcuts and the
fixed Ctrl+Super+F12 emergency chord remain local; emergency release is terminal,
not a grant that can be restored by hovering. The native keyboard allowlist
rejects Super and terminates control on its press; the emergency chord stays
local, but termination may already have occurred at the Super edge. This is
not general desktop/system-shortcut forwarding. Unrelated focus loss, lock,
surface or geometry changes (including position), deadline expiry, malformed transitions and disconnection
are terminal. Existing input restrictions, including no drag/modifier-click,
remain unchanged. The local gate covers keyboard shortcuts, not a global mouse
grab: compositor modifier+mouse bindings are not suppressed by this slice and
may safely terminate the session through the resulting focus/geometry change.
Actual pointer focus must also be refreshed when window movement or occlusion
changes its recipient without physical mouse movement. Crossing monitors can
cause a non-transactional WM focus change and is currently terminal; resumable
cross-monitor handoff is not guaranteed. Already-inside startup relies on
Smithay's normal pointer-enter delivery, but that ordering has not received a
dedicated startup acceptance test.

This is a software-rendered, owned-protocol test target. It is not evidence of
physical mouse/keyboard/GPU acceptance on a live desktop. CLI availability does
not remove that separate acceptance requirement. Parser-to-options tests and
production-viewer TCP integration are layered evidence, not a single CLI launch
acceptance test. The TCP fixtures construct options directly. A separate
independent-process CI gate, `scripts/test-remote-wayland-cli.sh`, launches the
actual host and viewer binaries with explicit flags and requires three executed
cases with strict GL: same-lease input/pause/resume and clean successful closure,
missing host permission denial, and old local protocol denial. The gate fails
if private Unix sockets or rendering are unavailable; it never counts a skipped
process test as acceptance. It remains owned-software, not physical hardware,
validation.

## Exact duration negotiation and absolute budgets

`connect --wayland-control-duration SECONDS` is valid only with native seamless
input; `host --max-wayland-control-duration SECONDS` only with explicit resumable
control. Each typed configuration value is 30..=300 seconds and defaults to 30.
The host ceiling is not a grant: the client requests its exact duration, and a
request above the ceiling is refused rather than clamped. The authenticated
server must echo the exact request. These flags never become saved authority.

Application v35 extends stream capabilities from 34 to 36 bytes. Bytes 34..36
contain the big-endian u16 duration in seconds. The default value 30 alone grants
no input; a value above 30 requires the combined/resumable bitfield and independent
local permission. Old application versions and old record lengths are rejected.
The lifecycle record version and generation/sequence rules remain unchanged.

`Negotiated::new` still authorizes at most 30 seconds. The duration-aware token
validates both opt-ins and the exact request against the local ceiling. One
process-local `LeaseBudget` retains its original start and deadline; native
confirmation can only tighten it. Remaining milliseconds are rounded down,
including values below 30000 after setup. Clock reversal, exhaustion and expired
budgets are terminal. No cross-machine `Instant` is serialized. Hello, native
creation and local handoff each consume the existing budget; receiving Grant,
Arm, Pause or Resume ACKs cannot restart it. Paused time counts toward expiry.

Native control v5 adds `create_timed_resumable_lease`; local handoff v3 adds
`create_timed`. These explicitly accept only bounded remaining lifetime, not
new input permissions. Old creation opcodes retain their original 30-second
limits even on newer bindings. Default 30-second requests can still use source
v4/local v2; a request above 30 requires v5/v3 even if transit has already reduced
its remainder below 30 seconds. Existing sequence/rate, proof freshness and
transition-timeout limits remain unchanged and may end a longer session early.

## Implemented protocol slice

- `ClientOptions.resumable_control` and `HostOptions.allow_resumable_control`
  must both be true, alongside existing combined-control permission. The
  dedicated client seamless flag and host resumable flag set those permissions;
  defaults remain false. Static-lossless repair is excluded
  until its phase cancellation has a separate proved acknowledgement.
- Stream byte 30 is a bitfield: ordinary combined control is `1`, version-one
  resumable combined control is `3`; `2` is invalid. Application v35 additionally
  carries the exact duration. A peer refuses an unavailable requested mode
  instead of downgrading.
- Authenticated message kind 53 carries an exact version-one record: 56-byte
  lifecycle or 104-byte action envelope. Every action binds the native session,
  source/geometry epochs, generation and global sequence; the inner legacy
  action fields must agree. Untagged actions are rejected in this mode.
- Native manager/lease v4 adds explicit `create_resumable_lease`, `pause` and
  `resume`. Using an old creation request on a v4 object still creates a one-shot
  lease. Pause applies native keyboard cleanup and balances held XKB keys before
  emitting `paused`; the adapter waits for that event before the TCP ACK.
- Pause/resume consume the same monotonically increasing sequence as input;
  resume increments generation. No new lease is created, no absolute deadline
  is moved, and failure/revocation cannot return to a live state.
- While paused, captured/presented proof announcements may continue. A
  `PointerReady` record is only proof availability, **not input authority**:
  both client and host session gates reject forwarding until matching resume
  confirmation. Resume requires the exact current proof and a native frame
  completed after pause; stale input/proofs cannot cross the transition.

The original resumable integration fixture drives lifecycle transitions through
a test-only queue but uses production authenticated TCP and native release ACKs.
The seamless fixture instead drives actual compositor seat pointer/key events
through the production local handoff route. It does not fabricate grants or use
the lifecycle command queue. The ordinary one-shot path still revokes on leave.

Run pure wire/state tests with `remote-x11`, and native/TCP integration tests
with `remote-wayland,backend-wayland-udev` and strict headless GL. Keep the native
key-routing assertions separate from TCP tests: observing an inhibitor boolean
alone is not proof that a compositor shortcut was withheld or delivered.

## Why this is a distinct protocol feature

The X11 local grab can end without destroying the host's ordinary input
permission. Without the dedicated native seamless opt-in, Wayland's ordinary
one-shot combined-control path retains a different contract:

- `wayland_viewer_pointer.rs`: surface leave permanently revokes the pointer.
- `wayland_viewer_keyboard.rs`: keyboard leave revokes; a second enter is not a
  fresh authorization. The compositor, not the viewer, owns keyboard focus.
- `wayland_viewer.rs`: combined collection propagates either device's revocation
  to both devices. Local revocation clears queued actions.
- `client_control.rs`: one request, one epoch, an absolute lease deadline;
  terminal status cannot revive the lease.
- `host_control.rs`: stop releases native authority and invalidates presentation
  proofs. It is not a pause operation.

Removing those checks or reconnecting on hover would turn a terminal revocation
into a new grant. Neither is an implementation of seamless input.

## First supported target

Target an explicitly opted-in JWM viewer compositor and JWM host first. Do not
promise hover-to-keyboard-focus on arbitrary Wayland compositors: a normal client
cannot force keyboard focus merely because its pointer entered a surface.
Keyboard-shortcuts inhibition does not itself grant focus or remote permission.
The experimental JWM-local transaction supplies a dedicated inhibition gate;
it does not reinterpret generic shortcut-inhibit permission as a focus grant.

The native opt-in negotiates resumable-control capability with both endpoints.
Old peers deny that capability; there is no downgrade to an old one-shot grant
or XWayland fallback. Existing one-shot behavior is preserved when absent.

## State transitions and authority

Use a separate, bounded resumable session, not two independent keyboard/pointer
leases. Bind its ID to the authenticated connection, source and geometry epochs,
viewer surface, seat and keymap. Keep the original absolute expiry through every
pause. Pointer movement cannot create, extend or renew permission.

| State | Event | Required result |
| --- | --- | --- |
| Unauthorised | pointer enter | No request or input; explicit session opt-in and host approval are required first |
| Armed outside | fresh image enter | Start local focus/clean-input and presentation checks, not forwarding |
| Enter pending | clean own-surface focus and current presented proof, acknowledged host resume | Active with a new input generation under the same session |
| Active | leave image, including letterbox | Stop forwarding immediately; release local inhibition and queue host pause/release in order |
| Pause pending | pointer enters again | Remember no input; wait for pause acknowledgement before any resume |
| Pause pending | matching release acknowledgement | Armed outside; require a fresh checked entry, never replay held input |
| Active or pending | Ctrl+Super+F12 emergency chord | Terminal release; hovering cannot reauthorize input |
| Any live state | expiry, revoke, disconnect, topology/keymap/source/geometry change, focus loss not attributable to normal exit, malformed event or failed fence | Terminal; release locally and remotely; never resume from hover |
| Terminal | any enter, late ACK or presentation | Remain terminal; a new explicit authorization is required |

Normal pointer exit can lead to a compositor keyboard-focus change. Define an
ordered, bounded exit/focus transaction in the JWM compositor integration; do not
classify arbitrary focus loss as a resumable exit. If attribution is ambiguous,
terminate. The host must release held keys/buttons as part of pause before it
acknowledges. If that acknowledgement fails or times out, terminate locally and
close the session so existing host teardown releases authority.

## Implementation stages and remaining acceptance gates

1. Negotiate a separate versioned capability and explicit permission. Define
   pause/resume messages with session ID, monotonically increasing input
   generation and action sequence. Reject stale/replayed/out-of-order records.
   No generation change may reset the session deadline or reuse a presented gate.
2. Extend host/native authority with a real paused state that holds no keys or
   buttons and accepts no input. A pause acknowledgement proves releases were
   applied, not just queued. Keep terminal revoke separate and irreversible.
3. Add the client state machine next to `ClientControl`, only after the host
   contract exists. Cancel queued candidates, scroll frames and repair gates at
   pause; require new same-connection presentation evidence on resume. Existing
   one-shot `ReleaseAll` must not be reinterpreted as pause.
4. Add JWM-local, explicit focus-follow integration bound to the viewer surface
   and session. Acquire shortcut inhibition only after real own-surface focus;
   wait for compositor acknowledgement before declaring keyboard control. Never
   confine the pointer. Destroy inhibition on exit, escape, failure and teardown.
   Preserve compositor emergency/lock shortcuts and sandbox restrictions.
5. Expose the dedicated CLI opt-in only through the complete negotiated path.
   Keep defaults off and reject profiles and unsupported `ssh` combinations.
   No automatic reconnect/regrant. Report active, paused and terminal states
   truthfully; explicit reauthorization is a separate session, not hover.

## Merge gates

Tests must exercise the real native viewer, client wire path, host and compositor,
not only a detached state-machine model:

- clean entry forwards both devices only after focus, presentation and resume;
- exit with held key/button releases once before returning local shortcuts;
- fast exit/enter waits for release ACK and cannot replay queued input;
- letterbox crossing, drag, scroll and modifier-held entry fail safely;
- emergency escape is terminal even after exit/entry; compositor escape and
  lock remain usable while inhibition is active;
- pointer exit plus focus event reorder cannot make unrelated focus loss resume;
- missing/denied inhibition or unsupported compositor never reports full control;
- stale ACKs, duplicate resumes, transport loss and queue bounds cannot revive;
- expiry while paused or active is terminal, with the original deadline unchanged;
- source, size, keymap, seat, surface or permission changes revoke permanently;
- old peers and all existing one-shot tests retain their behavior.

Run headless protocol integration first, then real compositor and physical-input
checks for focus, shortcut routing and cross-window movement. A synthetic protocol
pass alone is not proof of natural desktop handoff.
