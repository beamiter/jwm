# Native pointer-only plus static lossless repair

## Candidate scope and status

This candidate combines explicit native Wayland pointer-only control with
bounded static PNG repair. It uses application protocol v27; both peers must
match, with no silent downgrade. Transport v3 is unchanged. Both permissions
remain default off. The baseline `363a` passed all four mainline CI workflows;
that result does not validate this new combination.

The candidate passes strict Clippy, compilation of the real integration tests,
37 pure phase/client/host/wire regressions, three capability/version tests, and
the pure marker negative test. The static-lossless CLI group passes three pure
tests. Independent static review found no remaining blocker after separating
advisory cancellation from actual Request/Decline completion receipts. The
integration module contains five real fixtures and one pure marker negative
test; the real fixtures have not yet run in CI. No real-pipeline, hardware or
physical-latency acceptance is claimed for the combination yet.

## Explicit selection and permissions

The candidate command shape, in addition to the normal authentication and
connection options, is:

```sh
jwm-remote host --wayland-select --once --allow-wayland-pointer --allow-static-lossless
jwm-remote connect HOST --viewer-backend wayland --wayland-pointer --static-lossless
```

The CLI now accepts the explicit host/viewer combination. Its lossless mode
group allows exactly one of view-only, keyboard or pointer, and the native-input
lossless validation requires the Wayland viewer. Scroll and the other forbidden
combinations remain explicitly rejected. The centralized static-lossless CLI
group passed all three pure tests, including
the positive pointer combination and mixed-mode/profile negative cases. Real
integration CI remains pending at this checkpoint.

The host selects one live surface locally. This combination requires a native
Wayland viewer and is not view-only: do not add `--view-only`. It does not enable
legacy input, keyboard, unified control, text submission, vertical or horizontal
scroll, clipboard, input grab, remote resize or resize-follow. Popup capture and
local source resize-follow are also excluded for this input combination. No
saved profile or automatic reconnect may recreate the authorization. Other
previously supported lossless modes do not implicitly become pointer sessions.

## Presentation is not input authorization

The existing pointer lease remains 30 seconds and nonrenewable. PNG repair,
frame acknowledgments and periods of inactivity do not extend it. The existing
exact target, focus, local takeover, lock, unmap and disconnect checks continue
to apply. Motion and atomic left click remain the only pointer actions in this
combination, with no drag or modifier-click capability.

A repair record can make the displayed encoded-resolution RGB pixels exact,
but it cannot create an input grant or a new native capture proof. Its ACK must
not be interpreted as pointer Ready. A new actionable scope must be tied to a
new genuine native capture proof and actual viewer presentation acknowledgment;
old feedback, a repair sequence or a repaired canvas cannot substitute for it.
The receiver must preserve this distinction across queued feedback and repairs,
not merely disable input while it decodes a PNG. Native authority is rechecked
when an action is submitted.

Lossless repair retains bounded batches, independent exactness debt and
successful-flush commit requirements. Later pixel changes can make repaired
tiles inexact again. Exactness describes the encoded RGB canvas, not the source
before scaling or physical monitor output. Repair consumes encoding, transport
and presentation resources; this feature is not a promise of lower click
latency or uninterrupted input readiness.

## Required real integration evidence

The module contains five real integration tests covering:

- Repair followed by a new native proof and presentation before further input,
  while checking exact displayed pixels.
- Explicit denial when the host does not permit the requested combination.
- Lock and unmap/ABA transitions that cannot restore old authority.
- Absence of repair publication without viewer opt-in.
- Lease revocation on local host takeover.

The test filter is
`host_pointer_integration::pointer_lossless_integration`, a child of the existing
pointer fixture rather than a direct host child. CI should execute it in its
own software-GL step, assert that exactly
six tests are discovered (five real fixtures plus one pure marker negative test), and keep the existing strict timing and native
pointer tests. Pure protocol/state tests or successful compilation are not
substitutes for these real recipient/presentation checks. Any separate ignored
phase measurement should be labeled as measurement, not as an authorization or
physical-display proof. Keep all existing deadlines and rejection behavior;
report failures rather than enlarging the acceptance window.
