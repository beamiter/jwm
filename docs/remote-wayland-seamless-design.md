# Native Wayland seamless input: integration contract

Status: design, not an enabled feature. `--seamless-input` still requires an
X11 viewer. Native Wayland requests fail before display or network I/O. The
concrete viewer factory also rejects them rather than silently dropping the
option. Existing one-shot native permissions are unchanged.

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
The viewer currently has no shortcuts-inhibit client integration.

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

## Required wiring, in order

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
