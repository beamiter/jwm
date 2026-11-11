# Native pointer-only plus static lossless repair

## Candidate scope and status

This candidate combines explicit native Wayland pointer-only control with
bounded static PNG repair. It uses application protocol v27; both peers must
match, with no silent downgrade. Transport v3 is unchanged. Both permissions
remain default off. The baseline `363a` passed all four mainline CI workflows;
that result does not validate this new combination.

The first candidate passed local strict Clippy and pure tests, but its first
real integration run had two passing and four failing tests. It is not accepted
on that evidence. Follow-up work distinguishes a legitimate late action receipt
after local revocation, uses the actual trusted-local input acquisition path,
and narrows permission-denial teardown classification. Lock now requires exactly the one time-zero focus-restore motion produced by
Smithay during locking; extra motion, buttons and axes fail. Unmap remains
strictly zero-action. These distinctions still require the real CI rerun.
The updated module has six real fixtures, including 800×600 multi-batch repair,
and one pure marker negative test. The follow-up passes strict Clippy, test
compilation, 39 pure phase/client/host/wire regressions, the marker negative test
and the arithmetic checks below. Its new real CI results remain pending. No
hardware or physical-latency acceptance is claimed for this combination.

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

The module contains six real integration tests covering:

- Repair followed by a new native proof and presentation before further input,
  while checking exact displayed pixels.
- Explicit denial when the host does not permit the requested combination.
- Lock and unmap/ABA transitions that cannot restore old authority.
- Absence of repair publication without viewer opt-in.
- Lease revocation through the owned trusted-local virtual-pointer acquisition
  path, not a claim about physical libinput hardware.
- Multiple 800×600 repair batches, each followed by new proof presentation,
  final whole-frame exactness and a subsequent actual click.

The test filter is
`host_pointer_integration::pointer_lossless_integration`, a child of the existing
pointer fixture rather than a direct host child. CI should execute it in its
own software-GL step, assert that exactly
seven tests are discovered (six real fixtures plus one pure marker negative
test), and keep the existing strict timing and native
pointer tests. Pure protocol/state tests or successful compilation are not
substitutes for these real recipient/presentation checks. Any separate ignored
phase measurement should be labeled as measurement, not as an authorization or
physical-display proof. Keep all existing deadlines and rejection behavior;
report failures rather than enlarging the acceptance window.

## Full-frame convergence budget

This combination currently promises only **local progressive repair, not
full-frame exactness at common desktop sizes within the input lease**. The
existing 200×150 fixture fits in one batch and cannot demonstrate larger-frame
convergence. An 800×600 multi-batch real fixture has been added; no passing
result is claimed for that additional test yet.

With the default 16×16 tiles, selection is capped at
`min(65536 / 256, 16384 / 16) = 256` tiles per PNG record. Partial edge tiles
still occupy a complete padded tile in the atlas. Initial JPEG makes every
sent tile owe exact repair, even at quality 100; preexisting static content is
not exempt. The following bounds assume no input, no further pixel changes,
no lost repair work and zero time spent in capture, encoding, transfer or
presentation:

| Encoded RGB size | Padded tile count / pixels | Minimum batches | Ideal time lower bound |
| --- | --- | --- | --- |
| 200×150 | 130 / 33,280 | 1 | 2 seconds |
| 800×600 | 1,900 / 486,400 | 8 | 16 seconds |
| 1280×720 | 3,600 / 921,600 | 15 | 30 seconds |
| 1920×1080 | 8,160 / 2,088,960 | 32 | 64 seconds |

The first offer requires two seconds of input idleness. After each repair, the
next two-second cooldown starts only after the new native proof is actually
acknowledged, not merely when the PNG is flushed. Every phase adds real work to
the table's ideal lower bound. At `now >= lease_deadline` the lease has ended,
so even the ideal 15th batch at exactly 30 seconds is too late. At most 14
batches can complete strictly before expiry in this optimistic model, and
actual progress can be lower. New input or damage can delay it further.

Dimensions here are the encoded canvas after scaling, not necessarily source
window dimensions. Scaling down may reduce the batch count, but the default
width ceiling does not guarantee a sufficiently small canvas. The result is
not a reason to extend the lease, shorten cooldown or treat repair feedback as
new authorization. Such changes would require a separately reviewed design.

Reproduce the arithmetic and its edge-padding/deadline negative checks with:

```sh
python3 scripts/check-pointer-lossless-budget.py --self-test
python3 scripts/check-pointer-lossless-budget.py
```

This independent pure model mirrors current constants; it is not production
code, a simulated successful protocol exchange or a latency benchmark. Keep
its constants aligned if the implementation changes. Representative real tests
should check multi-batch exactness at 800×600 and partial progress/terminal
expiry at 1080p, without demanding impossible 1080p full convergence or changing
the 30-second deadline.
