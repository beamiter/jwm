# Native Wayland seamless input: integration contract

Status: experimental JWM-udev-only programmatic vertical slice, not general
Wayland seamless input. `--seamless-input` remains X11-only. CLI constructors
keep native seamless/resumable options off; no new user-facing flag is enabled.
The native path requires `ClientOptions.wayland_seamless_input`, explicit
resumable combined control on both peers, and the JWM local handoff protocol.
Nested JWM backends and other compositors do not advertise this contract.

## Local compositor handoff contract

`jwm_remote_viewer_handoff_v1` creates one explicitly requested session for the
viewer's own normal toplevel and seat. Sandboxed/foreign surfaces are denied.
It grants no remote authority. Its at-most-30-second fixed lifetime starts
before the first remote control request and can only end earlier, never extend the remote lease.

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
not a grant that can be restored by hovering. Unrelated focus loss, lock, surface
or geometry changes, deadline expiry, malformed transitions and disconnection
are terminal. Existing input restrictions, including no drag/modifier-click,
remain unchanged. The local gate covers keyboard shortcuts, not a global mouse
grab: compositor modifier+mouse bindings are not suppressed by this slice and
may safely terminate the session through the resulting focus/geometry change.

This is a software-rendered, owned-protocol test target. It is not evidence of
physical mouse/keyboard/GPU acceptance on a live desktop. Such acceptance and a
reviewed CLI opt-in remain separate release gates.

## Implemented protocol slice

- `ClientOptions.resumable_control` and `HostOptions.allow_resumable_control`
  must both be true, alongside existing combined-control permission. Defaults
  and every CLI constructor remain false. Static-lossless repair is excluded
  until its phase cancellation has a separate proved acknowledgement.
- Stream byte 30 is a bitfield: ordinary combined control is `1`, version-one
  resumable combined control is `3`; `2` is invalid. Old peers reject `3`, and a
  new peer refuses an unavailable requested mode instead of downgrading.
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
permission. The native Wayland combined-control path has a different contract:

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

A future opt-in must explain that entering the remote image can redirect typing.
It must negotiate a new resumable-control capability with both endpoints. Old
peers deny that capability; do not downgrade to an old one-shot grant or pretend
XWayland is native support. Preserve existing one-shot behavior when absent.

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
| Active or pending | escape chord | Pause/release; latch re-entry off until an observed exit and subsequent fresh entry |
| Any live state | expiry, revoke, disconnect, topology/keymap/source/geometry change, focus loss not attributable to normal exit, malformed event or failed fence | Terminal; release locally and remotely; never resume from hover |
| Terminal | any enter, late ACK or presentation | Remain terminal; a new explicit authorization is required |

Normal pointer exit can lead to a compositor keyboard-focus change. Define an
ordered, bounded exit/focus transaction in the JWM compositor integration; do not
classify arbitrary focus loss as a resumable exit. If attribution is ambiguous,
terminate. The host must release held keys/buttons as part of pause before it
acknowledges. If that acknowledgement fails or times out, terminate locally and
close the session so existing host teardown releases authority.

## Original implementation order and remaining release gates

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
5. Wire the CLI option only when the complete negotiated path is available.
   Report active, paused and terminal states truthfully. No profile persistence
   or automatic reconnect/regrant. Implement a clear explicit reauthorization
   action separately if desired.

## Merge gates

Tests must exercise the real native viewer, client wire path, host and compositor,
not only a detached state-machine model:

- clean entry forwards both devices only after focus, presentation and resume;
- exit with held key/button releases once before returning local shortcuts;
- fast exit/enter waits for release ACK and cannot replay queued input;
- letterbox crossing, drag, scroll and modifier-held entry fail safely;
- escape while inside remains suspended until exit/entry; compositor escape and
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
